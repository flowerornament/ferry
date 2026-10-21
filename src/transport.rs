//! The caller's side of a run: ship the objects, then submit, follow, stop
//! and read the record through short place verbs. SSH carries typed
//! requests and quoted arguments, never interpolated recipe code.
use crate::{
    config,
    execution::{self, Request},
    receipt::{self, Outcome, Receipt, note},
    repository,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// This process's SSH control directory. Each caller process multiplexes its
/// own SSH commands over one connection per destination that it alone owns: a
/// run never rides on a connection another run opened, whose transport (a
/// `ProxyJump` helper, say) lives in that other caller's process group and
/// dies with it.
fn control_directory() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").ok_or("HOME missing")?)
            .join(".cache/ferry/ssh")
            .join(std::process::id().to_string()),
    )
}

/// The longest Unix socket path every supported platform accepts (macOS:
/// 104 bytes with the terminator), less the 17 bytes of the temporary name
/// ssh binds before renaming it into place.
const SOCKET_PATH_BUDGET: usize = 103 - 17;

/// This caller's control socket for `destination`, or `None` when its path
/// would be too long for a Unix socket.
fn control_socket(destination: &str) -> Result<Option<String>> {
    // A short name of our own instead of ssh's 40-hex %C keeps the path in
    // budget; one caller talks to a handful of destinations at most.
    let socket = control_directory()?.join(&receipt::digest(destination.as_bytes())[..16]);
    let socket = socket.to_str().ok_or("non-UTF-8 SSH control path")?;
    Ok((socket.len() <= SOCKET_PATH_BUDGET).then(|| socket.to_owned()))
}

fn ssh_options(destination: &str) -> Result<Vec<String>> {
    use std::os::unix::fs::PermissionsExt;
    static PREPARED: std::sync::Once = std::sync::Once::new();
    let Some(socket) = control_socket(destination)? else {
        // Multiplexing only saves handshakes: without a usable socket path
        // (a long HOME), every command opens its own connection.
        let mut options: Vec<String> = ["BatchMode=yes", "ControlMaster=no", "ControlPath=none"]
            .iter()
            .flat_map(|o| ["-o".into(), (*o).to_owned()])
            .collect();
        options.extend(bounded());
        return Ok(options);
    };
    let directory = control_directory()?;
    let parent = directory.parent().ok_or("invalid SSH control directory")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    PREPARED.call_once(|| {
        // A directory with this pid can only be left by a dead process.
        let _ = fs::remove_dir_all(&directory);
        prune_dead(parent);
    });
    fs::create_dir_all(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let mut options = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        // Bounds how long a master outlives a caller that was killed.
        "ControlPersist=10".into(),
        "-o".into(),
        format!("ControlPath={socket}"),
    ];
    options.extend(bounded());
    Ok(options)
}

/// Every SSH connection the caller makes is bounded: its connect and banner
/// wait, and a dead link once it is up (three missed 5 s keepalives).
fn bounded() -> Vec<String> {
    [
        "ConnectTimeout=10",
        "ServerAliveInterval=5",
        "ServerAliveCountMax=3",
    ]
    .iter()
    .flat_map(|o| ["-o".into(), (*o).to_owned()])
    .collect()
}

/// A short reply from a place before submit, within `SSH_REPLY` wall-clock
/// time (debug builds read `FERRY_SSH_REPLY_MS`): `ConnectTimeout` and
/// keepalives do not reach a jump host's own connection or a stalled key
/// exchange, so the caller stops its own ssh at the bound and refuses.
fn replied(command: &mut Command) -> Result<Vec<u8>> {
    let (status, out, err) = answered(command)?;
    if !status.success() {
        return Err(format!("command failed: {}", String::from_utf8_lossy(&err).trim()).into());
    }
    Ok(out)
}

/// A place's reply within `SSH_REPLY`, whatever its exit status (a verb
/// that refuses exits 125 with its reply on stdout): the status, stdout and
/// stderr. Past the bound our own child is killed and the wait is an error.
fn answered(command: &mut Command) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    let bound = execution::debug_bound("FERRY_SSH_REPLY_MS")
        .map_or(crate::limits::SSH_REPLY, Duration::from_millis);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // The bound covers the child's exit and both pipes' end of file: a
    // descendant of ssh (a ProxyCommand, a helper it spawned) can hold a pipe
    // open after ssh itself has exited.
    let drain = |mut pipe: Box<dyn Read + Send>| {
        let (sent, received) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = sent.send(bytes);
        });
        received
    };
    let out = drain(Box::new(child.stdout.take().ok_or("no stdout")?));
    let err = drain(Box::new(child.stderr.take().ok_or("no stderr")?));
    let until = Instant::now() + bound;
    let late = || format!("the place did not reply within {bound:?}");
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= until {
            // Our own child, by its handle: never a pattern.
            let _ = child.kill();
            let _ = child.wait();
            return Err(late().into());
        }
        thread::sleep(Duration::from_millis(50));
    };
    let collect = |pipe: &std::sync::mpsc::Receiver<Vec<u8>>| {
        pipe.recv_timeout(until.saturating_duration_since(Instant::now()))
    };
    let (Ok(out), Ok(err)) = (collect(&out), collect(&err)) else {
        return Err(late().into());
    };
    Ok((status, out, err))
}

/// Removes the empty control directories of callers that are gone (a killed
/// caller leaves one). A non-empty directory, or one whose pid is alive, is
/// left alone.
fn prune_dead(parent: &Path) {
    for entry in fs::read_dir(parent).into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
            == Err(nix::errno::Errno::ESRCH)
        {
            let _ = fs::remove_dir(entry.path());
        }
    }
}

/// Closes this process's SSH connections and removes its control directory.
/// Called as the caller exits; each close waits at most 2 s. A killed
/// caller's master exits by itself after its `ControlPersist` idle time.
pub fn close_connections() {
    let Ok(directory) = control_directory() else {
        return;
    };
    for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
        let Ok(mut close) = Command::new("ssh")
            .arg("-S")
            .arg(entry.path())
            .args(["-O", "exit", "ferry-control"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while matches!(close.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if matches!(close.try_wait(), Ok(None)) {
            let _ = close.kill();
            let _ = close.wait();
        }
    }
    let _ = fs::remove_dir_all(&directory);
}

/// An SSH command to `destination`, over this caller's own connection.
fn ssh(destination: &str) -> Result<Command> {
    let mut command = Command::new("ssh");
    command.args(ssh_options(destination)?).arg(destination);
    Ok(command)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HostInfo {
    pub protocol: u32,
    pub binary_sha256: String,
    pub os_arch: String,
    /// Not read here; emitted because callers older than the v1 root require
    /// it, so they parse this reply and refuse naming the revision (§12.11).
    #[serde(default)]
    pub cores: usize,
    #[serde(default)]
    pub source_rev: Option<String>,
    #[serde(default)]
    pub exe: Option<String>,
    /// Absent on hosts that predate capabilities: none.
    #[serde(default)]
    pub capabilities: Vec<String>,
}
pub fn host_info() -> Result<HostInfo> {
    Ok(HostInfo {
        protocol: receipt::PROTOCOL,
        binary_sha256: receipt::digest(&fs::read(std::env::current_exe()?)?),
        os_arch: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        cores: std::thread::available_parallelism()?.get(),
        source_rev: Some(receipt::source_rev()),
        exe: receipt::exe_path(),
        capabilities: receipt::capabilities(),
    })
}

/// Publishes `refspec` (`<commit>:<ref>`) to the host. Ferry's refs name
/// their commit, so a concurrent run of the same commit may create the same
/// ref first and win git's ref lock: a failed push whose ref now holds that
/// commit is published, not an error.
fn push(root: &Path, ssh: &str, store: &str, refspec: &str) -> Result<()> {
    // Quiet: ssh shares git's stderr, so its own diagnostics would read as
    // the push's progress (SPEC §6).
    let ssh_command = ["ssh", "-o", "LogLevel=ERROR"]
        .into_iter()
        .map(str::to_owned)
        .chain(ssh_options(ssh)?)
        .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    let git = |args: &[&str]| {
        let mut command = Command::new("git");
        command
            .arg("--git-dir")
            .arg(repository::git_dir(root)?)
            .env("GIT_SSH_COMMAND", &ssh_command)
            .args(args)
            .arg(format!("{ssh}:{store}"));
        Ok::<_, Box<dyn std::error::Error>>(command)
    };
    let error = match progressing(git(&["push", "--no-verify", "--progress"])?.arg(refspec))? {
        Pushed::Done => return Ok(()),
        // A stalled place is not asked again: its answer would wait too.
        Pushed::Stalled(why) => return Err(why.into()),
        Pushed::Failed(why) => why,
    };
    let (commit, reference) = refspec.split_once(':').ok_or("invalid refspec")?;
    // Any failure to check keeps the push's own error. ls-remote matches
    // pattern tails, so only the line naming exactly this ref counts.
    // In the caller's own process group, so a terminal's signal reaches it
    // directly; on a timeout its git is killed, and its ssh ends on its pipe
    // or by keepalive.
    let listed = git(&["ls-remote"]).and_then(|mut command| replied(command.arg(reference)));
    let Ok(listed) = listed else {
        return Err(error.into());
    };
    let published = String::from_utf8_lossy(&listed).lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some(commit) && fields.next() == Some(reference)
    });
    if published { Ok(()) } else { Err(error.into()) }
}

/// How a push ended.
enum Pushed {
    Done,
    Failed(String),
    Stalled(String),
}

/// Runs a `git push --progress` in a process group of its own, bounded by
/// progress, not wall-clock time (SPEC §6): git refreshes its progress while
/// it counts, compresses and writes, and the place's `remote:` lines arrive
/// on the same stderr, so a place that stops reading leaves it silent. No
/// new byte for `PUSH_STALL` (debug builds read `FERRY_PUSH_STALL_MS`) is a
/// stall: the group, ours and recorded by its pid, is killed.
fn progressing(command: &mut Command) -> Result<Pushed> {
    use std::os::unix::process::CommandExt;
    let stall = execution::debug_bound("FERRY_PUSH_STALL_MS")
        .map_or(crate::limits::PUSH_STALL, Duration::from_millis);
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let group = nix::unistd::Pid::from_raw(i32::try_from(child.id())?);
    let mut pipe = child.stderr.take().ok_or("no push stderr")?;
    let started = Instant::now();
    // Milliseconds from `started` to the last progress byte, and the end of
    // what was said (16 KiB), readable at once when git exits, so a
    // descendant still holding the pipe neither delays nor blanks it.
    let heard = std::sync::Arc::new(AtomicU64::new(0));
    let said = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (last, kept) = (heard.clone(), said.clone());
    let (ended, drained) = std::sync::mpsc::channel::<()>();
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        while let Ok(read @ 1..) = pipe.read(&mut buffer) {
            last.store(
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                Ordering::SeqCst,
            );
            if let Ok(mut all) = kept.lock() {
                all.extend_from_slice(&buffer[..read]);
                let excess = all.len().saturating_sub(16 << 10);
                all.drain(..excess);
            }
        }
        let _ = ended.send(());
    });
    let stop = || {
        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
    };
    // Whether git has exited, seen without reaping it: until it is reaped
    // its pid, and so the group's id, cannot be reused.
    let exited = || crate::record::exited(group.as_raw());
    let status = loop {
        // Whatever git left in its group (a pipe-holder, a ProxyCommand) is
        // ours: when git is seen to have exited, the group goes before git
        // is reaped. A state that cannot be read proves nothing, so
        // unreadability alone never kills a live push (see below).
        match exited() {
            Some(true) => {
                stop();
                break child.wait()?;
            }
            Some(false) => {}
            // Unknown: nothing is signalled while git may be alive. Once a
            // reap shows it exited, its recorded group is signalled after the
            // reap: while a leftover lives, a group's id is never reused, so
            // it is ours. Residue: if the group has just emptied, its id may
            // be reused by a new group in the instant before the signal (a
            // pid wraparound in that instant).
            None => {
                if let Some(status) = child.try_wait()? {
                    stop();
                    break status;
                }
            }
        }
        // In a group of its own, the push never sees the terminal's
        // signal: the caller that caught one stops it, as before dispatch.
        if execution::caught_signal().is_some() {
            stop();
            let _ = child.wait();
            execution::interrupted()?;
        }
        let quiet = started
            .elapsed()
            .saturating_sub(Duration::from_millis(heard.load(Ordering::SeqCst)));
        if quiet >= stall {
            stop();
            let _ = child.wait();
            return Ok(Pushed::Stalled(format!(
                "the push printed no progress for {} s",
                stall.as_secs_f64()
            )));
        }
        thread::sleep(Duration::from_millis(50));
    };
    if status.success() {
        return Ok(Pushed::Done);
    }
    // Its stderr ends with its group; what was still in the pipe is read,
    // within a bound, so the last lines are not lost.
    let _ = drained.recv_timeout(crate::limits::DRAIN);
    // The last lines git said: its `fatal:`, `error:` or `!` lines and what
    // explains them, never only a trailing hint.
    let all = said.lock().map(|all| all.clone()).unwrap_or_default();
    let text = String::from_utf8_lossy(&all);
    let lines: Vec<&str> = text
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(4)..].join("; ");
    Ok(Pushed::Failed(format!("command failed: {status}: {tail}")))
}

/// Where a run's place verbs run: a local exec on the repository's state
/// root, or ssh to a machine.
pub enum Place<'a> {
    Local(PathBuf),
    Remote(&'a config::Machine),
}

/// Quotes one argument for the remote shell.
fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

impl Place<'_> {
    /// The command for one place verb. Only the validated binary path and
    /// quoted arguments reach a remote shell; requests travel on stdin.
    fn verb(&self, args: &[&str]) -> Result<Command> {
        match self {
            Place::Local(state) => {
                let mut command = Command::new(std::env::current_exe()?);
                command.arg("place").arg("--state").arg(state).args(args);
                Ok(command)
            }
            Place::Remote(machine) => {
                // Options precede the destination, so this is built in full.
                let mut command = Command::new("ssh");
                command
                    .args(ssh_options(&machine.ssh)?)
                    .arg(&machine.ssh)
                    .arg(format!(
                        "{} place {}",
                        machine.binary,
                        args.iter()
                            .map(|arg| quote(arg))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ));
                Ok(command)
            }
        }
    }

    /// What the place's binary says of itself.
    fn info(&self) -> Result<HostInfo> {
        match self {
            Place::Local(_) => host_info(),
            Place::Remote(machine) => Ok(serde_json::from_slice(&replied(
                ssh(&machine.ssh)?.arg(format!("{} host-info", machine.binary)),
            )?)?),
        }
    }

    /// A run's file under the place's state root (`runs/<id>/<name>`).
    fn read(&self, id: &str, name: &str) -> Result<Vec<u8>> {
        config::identifier(id)?;
        match self {
            Place::Local(state) => Ok(fs::read(crate::record::run_dir(state, id).join(name))?),
            Place::Remote(machine) => replied(ssh(&machine.ssh)?.arg(format!(
                "cat {}/runs/{id}/{name}",
                crate::place::REMOTE_ROOT
            ))),
        }
    }
}

/// Ships the commit and its `[[ship]]` objects (`(repository, commit)`) to
/// the place's one store, only after checking that the place can run the
/// request: a refused place receives nothing. Each object is kept as
/// `refs/ferry/ship/<commit>`, outside `refs/heads`, so a seat cloned from
/// the store copies no project's ship entries (the place serves each run
/// its own, `execution::present_ship`). A place without `detach` and `ship`
/// is refused: callers are never installed before their places.
pub fn ship(
    root: &Path,
    place: &Place,
    request: &Request,
    objects: &[(PathBuf, String)],
    record: &mut Receipt,
) -> Result<()> {
    match place {
        Place::Local(state) => ship_local(root, state, request, objects),
        Place::Remote(machine) => {
            config::identifier(&machine.ssh)?;
            config::relative_path(&machine.binary)?;
            for component in Path::new(&machine.binary).components() {
                config::identifier(
                    component
                        .as_os_str()
                        .to_str()
                        .ok_or("binary path must be UTF-8")?,
                )?;
            }
            let info = place.info()?;
            let refused = |record: &mut Receipt, why: String| {
                refused_by(record, &info);
                record.outcome = Outcome::Refused;
                record.exit_code = 125;
                Err(why.into())
            };
            // Every M1 request ships (at least its commit), so `ship` is a
            // protocol gate, checked with `detach` before any push: the
            // predecessor that has detach but not ship gets nothing.
            let lacks = ["detach", "ship"]
                .into_iter()
                .find(|needed| !info.capabilities.iter().any(|c| c == needed));
            if let Some(lacks) = lacks {
                return refused(
                    record,
                    format!(
                        "place {} runs ferry {}, which lacks {lacks}: install this caller's ferry ({}) there",
                        machine.ssh,
                        info.source_rev.as_deref().unwrap_or("of unknown revision"),
                        receipt::source_rev()
                    ),
                );
            }
            if let Err(missing) = receipt::provides(&request.provider_requires, &info.capabilities)
            {
                return refused(record, format!("host {missing}"));
            }
            if info.protocol != receipt::PROTOCOL {
                return refused(record, "host protocol mismatch".into());
            }
            if request.recipe.requires.iter().any(|requirement| {
                (requirement.starts_with("linux-") || requirement.starts_with("macos-"))
                    && *requirement != info.os_arch
            }) {
                return refused(
                    record,
                    "reported host architecture does not meet requirements".into(),
                );
            }
            init_store(ssh(&machine.ssh)?.arg(format!(
                "mkdir -p {store} && git init -q --bare {store}",
                store = *STORE
            )))?;
            push(
                root,
                &machine.ssh,
                &STORE,
                &format!(
                    "{}:refs/ferry/{}/{}",
                    request.commit, request.seat, request.commit
                ),
            )?;
            for (repo, commit) in objects {
                push(repo, &machine.ssh, &STORE, &kept(commit))?;
            }
            Ok(())
        }
    }
}

/// The local route of `ship`: fetches into the repository's own store.
fn ship_local(
    root: &Path,
    state: &Path,
    request: &Request,
    objects: &[(PathBuf, String)],
) -> Result<()> {
    fs::create_dir_all(state)?;
    let bare = state.join("store/repo.git");
    // A local command: not under the place's reply bound (SPEC §6). Two
    // runs may race to create it, so a failure is retried.
    let mut tries = 0;
    while let Err(error) = repository::output(
        Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&bare),
    ) {
        tries += 1;
        if tries > 10 {
            return Err(error);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Two runs may fetch the same object at once: a fetch that loses
    // the ref's lock to the same commit has shipped it.
    let fetch = |repo: &Path, refspec: &str| {
        let fetched = repository::output(
            Command::new("git")
                .arg("--git-dir")
                .arg(&bare)
                .arg("fetch")
                .arg(repository::git_dir(repo)?)
                .arg(refspec),
        );
        let Some((commit, reference)) = refspec.split_once(':') else {
            return fetched;
        };
        let mut check = Command::new("git");
        check
            .arg("--git-dir")
            .arg(&bare)
            .args(["rev-parse", "--verify", reference]);
        match fetched {
            Err(_)
                if repository::output(&mut check)
                    .is_ok_and(|held| held.trim_ascii() == commit.as_bytes()) =>
            {
                Ok(vec![])
            }
            fetched => fetched,
        }
    };
    fetch(root, &request.commit)?;
    for (repo, commit) in objects {
        fetch(repo, &kept(commit))?;
    }
    Ok(())
}

/// The place's store, relative to its home directory (SPEC §9).
static STORE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| format!("{}/store/repo.git", crate::place::REMOTE_ROOT));

/// The refspec keeping a shipped object in the store, outside `refs/heads`.
fn kept(commit: &str) -> String {
    format!("{commit}:refs/ferry/ship/{commit}")
}

/// Sets up the place's store, again if it exists (harmless), as a short
/// reply under the reply bound (SPEC §6). Two first runs may race on its
/// config lock, so git's own failure is retried a few times; ssh's (255,
/// the link) and a place that does not answer are not.
fn init_store(command: &mut Command) -> Result<()> {
    let mut tries = 0;
    loop {
        let (status, _, err) = answered(command)?;
        // 255 is ssh's own failure (the link), not the race retried here.
        match (status.success(), status.code()) {
            (true, _) => return Ok(()),
            (false, Some(code)) if code != 255 && tries < 10 => tries += 1,
            (false, _) => {
                return Err(
                    format!("command failed: {}", String::from_utf8_lossy(&err).trim()).into(),
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// A refusal names the host binary that refused, as host-info reported it.
fn refused_by(record: &mut Receipt, info: &HostInfo) {
    record.host_binary_sha256 = Some(info.binary_sha256.clone());
    record.host_source_rev.clone_from(&info.source_rev);
    record.host_exe.clone_from(&info.exe);
}

/// Submits the request, then follows the run to its record. The receipt
/// becomes final only from the place's final record (or a refusal).
pub fn run_on(
    root: &Path,
    place: &Place,
    request: &Request,
    record: &mut Receipt,
    json: bool,
) -> Result<()> {
    // The last moment before the place is asked to run anything.
    execution::interrupted()?;
    let mut submit = place
        .verb(&["submit"])?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    // From here the place may start the run: a failure is lost, not refused.
    record.outcome = Outcome::Lost;
    record.exit_code = 255;
    submit
        .stdin
        .take()
        .ok_or("no submit stdin")?
        .write_all(format!("{}\n", serde_json::to_string(request)?).as_bytes())?;
    let out = submit.wait_with_output()?;
    let reply = serde_json::from_slice::<serde_json::Value>(&out.stdout).ok();
    let refused = match &reply {
        // `why; next` (SPEC §6), as every place past the `ship` gate sends.
        Some(reply) => reply["refused"].as_str().map(|refused| {
            let next = reply["next"].as_str().unwrap_or_default();
            format!("{}; {next}", reply["why"].as_str().unwrap_or(refused))
        }),
        // No reply: exit 125 refused before any effect; anything else may
        // have started the run, so the receipt stays submitted.
        None if out.status.code() == Some(125) => {
            Some("exit 125 without a reply; see its stderr".to_owned())
        }
        None => {
            record.state = Some("submitted".into());
            record.outcome = Outcome::Lost;
            record.exit_code = 255;
            note(
                &mut record.reason,
                format!(
                    "submit gave no reply ({}); the run may have started: ferry record {}",
                    out.status, request.id
                ),
            );
            return Ok(());
        }
    };
    let reply = reply.unwrap_or_default();
    if let Some(why) = refused {
        if let Ok(info) = place.info() {
            refused_by(record, &info);
        }
        // The place's own answer: final, like its record.
        record.state = Some("final".into());
        record.outcome = Outcome::Refused;
        record.exit_code = 125;
        record.reason = Some(format!("place refused before acceptance: {why}"));
        return Ok(());
    }
    record.deadline_ms = reply["deadline_ms"].as_u64().map(u128::from);
    // The seat book (SPEC §9): a failure is a note, never the run's.
    if let Err(error) = crate::seats::record(&request.seat, &record.machine, root, &record.seat) {
        note(&mut record.reason, format!("seat book: {error}"));
    }
    // Where the run went, before any of its output (SPEC §4).
    if json {
        execution::emit(
            serde_json::json!({
                "protocol": receipt::PROTOCOL, "event": "submitted", "id": request.id,
                "pid": reply["pid"], "machine": record.machine, "seat": record.seat,
                "commit": request.commit,
            })
            .to_string()
            .as_bytes(),
        );
    } else {
        execution::warn(&format!(
            "ferry: {} → {}, seat {}, commit {}",
            request.id,
            record.machine,
            record.seat,
            &request.commit[..12.min(request.commit.len())]
        ));
    }
    finish(
        root,
        place,
        record,
        json,
        Some(Instant::now() + request.wait()),
    );
    Ok(())
}

/// How following a run ended.
enum Followed {
    Final,
    Unreachable,
    /// The stop a signal asked for could not be delivered.
    NotStopped,
    /// A second signal while the run was stopping.
    Left(i32),
}

/// Follows a run to its end, then completes the receipt from the place's
/// final record. If the run's end cannot be seen, the receipt stays
/// submitted: the run may go on, and `ferry record <id>` finishes it later.
/// `waits_end`, when the run's locks may still be waited for: until then,
/// a quiet run's wait notes are said (SPEC §4).
pub fn finish(
    root: &Path,
    place: &Place,
    record: &mut Receipt,
    json: bool,
    waits_end: Option<Instant>,
) {
    let id = record.id.clone();
    // After the place accepted the run, only its final record decides: any
    // other failure leaves the receipt submitted (SPEC §4, §8).
    let offset = std::sync::Arc::new(AtomicU64::new(0));
    let done = std::sync::Arc::new(std::sync::Mutex::new(false));
    // Never joined: a stalled read must not hold the caller past the run.
    if let Some(until) = waits_end {
        let (place, id, offset, done) = (owned(place), id.clone(), offset.clone(), done.clone());
        thread::spawn(move || narrate(&place, &id, &offset, &done, until, json));
    }
    let followed = follow(root, place, record, json, &offset);
    // Under the lock a note is said, so none comes after this.
    *done
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    let left = match followed {
        Ok(Followed::Final) => match complete(root, place, record) {
            Ok(()) => return,
            Err(error) => format!("{error}; the run's verdict is on the place: ferry record {id}"),
        },
        Err(error) => format!("{error}; the run may go on: ferry record {id}"),
        Ok(Followed::Unreachable) => format!(
            "the place did not answer for {:?}; the run may go on: ferry record {id}",
            unreachable()
        ),
        Ok(Followed::NotStopped) => {
            let until = record.deadline_ms.map_or_else(
                || "its deadline".to_owned(),
                |ms| {
                    format!(
                        "its deadline in {} min (unix ms {ms})",
                        ms.saturating_sub(receipt::now_ms()).div_ceil(60_000)
                    )
                },
            );
            let left = format!("not stopped; continues until {until}; ferry stop {id}");
            execution::warn(&format!("ferry: {left}"));
            left
        }
        Ok(Followed::Left(signal)) => format!(
            "left at a second {} while the run was stopping: ferry record {id}",
            execution::signal_name(signal)
        ),
    };
    // No verdict is claimed while the run may go on (SPEC §4).
    record.state = Some("submitted".into());
    record.outcome = Outcome::Lost;
    record.exit_code = 255;
    note(&mut record.reason, left);
}

/// Ends this follow's own child, by its handle, and leaves `how`.
fn leave(child: &mut std::process::Child, how: Followed) -> Followed {
    let _ = child.kill();
    let _ = child.wait();
    how
}

/// Writes the run's output as the place logs it, from offset 0, reconnecting
/// from the bytes received after any drop. Output is written by its own
/// thread, so a stalled stdout never keeps this loop from a signal. The
/// first signal asks the place to stop the run; from then on the run's
/// record, not its output, decides when this ends. A second signal leaves.
/// Says why a run waits while it has no output yet (SPEC §4): every 2 s it
/// reads the place's record for its latest wait note, and says each new
/// one once. It is narration only: an answer it cannot read is skipped.
fn narrate(
    place: &Place,
    id: &str,
    output: &AtomicU64,
    done: &std::sync::Mutex<bool>,
    until: Instant,
    json: bool,
) {
    let ended = || {
        *done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    let quiet = || output.load(Ordering::SeqCst) == 0 && !ended() && Instant::now() < until;
    let mut said = String::new();
    while quiet() {
        let next = Instant::now() + narration();
        while quiet() && Instant::now() < next {
            thread::sleep(Duration::from_millis(50));
        }
        if !quiet() {
            return;
        }
        let Ok((status, out, _)) = place
            .verb(&["record", id])
            .and_then(|mut verb| answered(&mut verb))
        else {
            continue;
        };
        // Final, or gone: nothing more to say. Any other answer is skipped.
        if matches!(status.code(), Some(0 | 4)) {
            return;
        }
        let note = serde_json::from_slice::<serde_json::Value>(&out)
            .ok()
            .and_then(|record| record["waiting"].as_str().map(str::to_owned));
        let Some(note) = note.filter(|note| *note != said) else {
            continue;
        };
        // Checked and said under the lock `finish` sets when it is done.
        let done = done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*done && output.load(Ordering::SeqCst) == 0 {
            if json {
                execution::emit(
                    serde_json::json!({"protocol": receipt::PROTOCOL, "event": "waiting", "note": note})
                        .to_string()
                        .as_bytes(),
                );
            } else {
                execution::warn(&format!("ferry: {note}"));
            }
            said = note;
        }
    }
}

/// A place the narrating thread owns, outliving `finish`: a run's machine
/// is copied once and kept until the caller exits.
fn owned(place: &Place) -> Place<'static> {
    match place {
        Place::Local(state) => Place::Local(state.clone()),
        Place::Remote(machine) => Place::Remote(Box::leak(Box::new((*machine).clone()))),
    }
}

/// How often a quiet run's wait note is read; debug builds read
/// `FERRY_NARRATION_MS`.
fn narration() -> Duration {
    execution::debug_bound("FERRY_NARRATION_MS")
        .map_or(crate::limits::NARRATION, Duration::from_millis)
}

fn follow(
    root: &Path,
    place: &Place,
    record: &Receipt,
    json: bool,
    offset: &std::sync::Arc<AtomicU64>,
) -> Result<Followed> {
    let mut contact = std::time::Instant::now();
    let mut backoff = Duration::from_secs(1);
    let mut stopping: Option<(i32, std::sync::Arc<AtomicBool>)> = None;
    // A signal that came after the run was final: the verdict stands, and
    // the follow delivers every byte; a second signal leaves with it.
    let mut ended: Option<std::sync::Arc<AtomicBool>> = None;
    loop {
        let mut child = place
            .verb(&[
                "follow",
                &record.id,
                &offset.load(Ordering::SeqCst).to_string(),
            ])?
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Its failures are retried, and reported as an unreachable
            // place; they never mix into the output.
            .stderr(Stdio::null())
            .spawn()?;
        let mut pipe = child.stdout.take().ok_or("no follow stdout")?;
        let copied = offset.clone();
        let reader = thread::spawn(move || {
            let mut out: Box<dyn Write> = if json {
                Box::new(std::io::stderr())
            } else {
                Box::new(std::io::stdout())
            };
            let mut buffer = vec![0u8; 64 << 10];
            while let Ok(read @ 1..) = pipe.read(&mut buffer) {
                // A closed stdout is data: the run goes on regardless.
                let _ = out.write_all(&buffer[..read]).and_then(|()| out.flush());
                copied.fetch_add(read as u64, Ordering::SeqCst);
            }
        });
        let attempt = std::time::Instant::now();
        let before = offset.load(Ordering::SeqCst);
        let mut checked = attempt;
        // Both the follow and its reader: the next offset counts every byte
        // this one delivered.
        let status = loop {
            if let Some(status) = child.try_wait()?
                && reader.is_finished()
            {
                break status;
            }
            match &stopping {
                Some((signal, again)) if again.load(Ordering::SeqCst) => {
                    return Ok(leave(&mut child, Followed::Left(*signal)));
                }
                Some(_) if checked.elapsed() >= Duration::from_secs(1) => {
                    checked = std::time::Instant::now();
                    if is_final(place, record)? {
                        return Ok(leave(&mut child, Followed::Final));
                    }
                }
                Some(_) => {}
                None => match &ended {
                    Some(again) if again.load(Ordering::SeqCst) => {
                        return Ok(leave(&mut child, Followed::Final));
                    }
                    Some(_) => {}
                    None => {
                        if let Some(signal) = execution::caught_signal() {
                            // Watch for a second signal before asking, so one
                            // that arrives while the stop is delivered counts.
                            let again = execution::stopped()?;
                            match stop(root, place, record, &stop_reason(signal), false, signal) {
                                Ok(_) => stopping = Some((signal, again)),
                                // Refused because the run already ended.
                                Err(_) if is_final(place, record)? => ended = Some(again),
                                Err(error) => {
                                    execution::warn(&format!(
                                        "ferry: stopping run {}: {error}",
                                        record.id
                                    ));
                                    return Ok(leave(&mut child, Followed::NotStopped));
                                }
                            }
                        }
                    }
                },
            }
            thread::sleep(Duration::from_millis(50));
        };
        match status.code() {
            Some(0) => return Ok(Followed::Final),
            Some(4) => return Err(no_run(place, &record.id).into()),
            _ => {}
        }
        // Contact is output received, or a follow that stayed connected; a
        // place failing at once (ssh or follow alike) is not.
        if offset.load(Ordering::SeqCst) > before || attempt.elapsed() >= Duration::from_secs(10) {
            contact = std::time::Instant::now();
            backoff = Duration::from_secs(1);
        }
        if contact.elapsed() >= unreachable() {
            return Ok(Followed::Unreachable);
        }
        thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// The place has no run `id`, with the context its record verb gives (how
/// long it keeps run history): a follow's own stderr is not read.
fn no_run(place: &Place, id: &str) -> String {
    let said = place
        .verb(&["record", id])
        .and_then(|mut verb| answered(&mut verb))
        .ok()
        .filter(|(status, _, _)| status.code() == Some(4))
        .map(|(_, _, err)| String::from_utf8_lossy(&err).trim().to_owned())
        .filter(|said| !said.is_empty());
    match said {
        Some(said) => format!("the place has no run {id}: {said}"),
        None => format!("the place has no run {id}"),
    }
}

/// How long a caller keeps trying a place that does not answer; debug
/// builds read `FERRY_UNREACHABLE_MS` so a control can reach it quickly.
fn unreachable() -> Duration {
    execution::debug_bound("FERRY_UNREACHABLE_MS")
        .map_or(crate::limits::UNREACHABLE, Duration::from_millis)
}

fn stop_reason(signal: i32) -> String {
    format!("caller received {}", execution::signal_name(signal))
}

/// Whether the place holds the run's final record.
fn is_final(place: &Place, record: &Receipt) -> Result<bool> {
    Ok(place
        .verb(&["record", &record.id])?
        .output()?
        .status
        .success())
}

/// Asks a place to retire one of this caller's seats (SPEC §6). A place
/// without `retire` is refused, naming its revision, as for `detach`.
pub fn retire(
    place: &Place,
    key: &str,
    meant: &crate::place::Meant,
    until: Option<std::time::Instant>,
) -> Result<serde_json::Value> {
    let info = place.info()?;
    // A slow `host-info` may use up a prune's budget: then no retire starts.
    if until.is_some_and(|until| std::time::Instant::now() >= until) {
        return Ok(serde_json::json!({"skipped": "budget"}));
    }
    let lacks = |capability: &str| !info.capabilities.iter().any(|c| c == capability);
    // An idle or one-run retire needs a place that checks `used` under the
    // seat lock and never retires during an exclusive run.
    let needs = if meant.idle_ms.is_some() || meant.once_id.is_some() {
        "seat-lifetime"
    } else {
        "retire"
    };
    if lacks(needs) {
        return Ok(serde_json::json!({
            "refused": "old",
            "why": format!(
                "the place runs ferry {}, which lacks {needs}",
                info.source_rev.as_deref().unwrap_or("of unknown revision")
            ),
            "next": format!("install this caller's ferry ({}) there", receipt::source_rev()),
        }));
    }
    let mut args = vec!["retire".to_owned(), key.to_owned()];
    if let Some(idle) = meant.idle_ms {
        args.extend(["--idle-ms".to_owned(), idle.to_string()]);
    }
    if let Some(once) = &meant.once_id {
        args.extend(["--once-id".to_owned(), once.clone()]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // Bounded like every short reply (SPEC §9: a retire in flight is waited
    // for up to the ssh reply bound); the place goes on with it regardless.
    let (status, out, err) = answered(&mut place.verb(&args)?)?;
    serde_json::from_slice(&out).map_err(|_| {
        format!(
            "retire gave no reply ({status}: {})",
            String::from_utf8_lossy(&err).trim()
        )
        .into()
    })
}

/// Replays a final run's output from the start, as `follow` does for a
/// live one, while the place still keeps it.
pub fn replay(place: &Place, record: &Receipt) -> Result<()> {
    let out = place
        .verb(&["follow", &record.id, "0"])?
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "run {}'s output is no longer on {} ({})",
        record.id,
        record.machine,
        failed(&out)
    )
    .into())
}

/// Asks a place what it sees now (SPEC §4). A place without `status` is
/// refused, naming its revision.
pub fn status(place: &Place) -> Result<serde_json::Value> {
    let info = place.info()?;
    if !info.capabilities.iter().any(|c| c == "status") {
        return Err(format!(
            "the place runs ferry {}, which lacks status; install this caller's ferry ({}) there",
            info.source_rev.as_deref().unwrap_or("of unknown revision"),
            receipt::source_rev()
        )
        .into());
    }
    let out = place.verb(&["status"])?.output()?;
    serde_json::from_slice(&out.stdout)
        .map_err(|_| format!("status gave no reply ({})", failed(&out)).into())
}

/// How a place command failed: its exit status, and its stderr when it
/// said something (an ssh failure or a place error is only there).
fn failed(out: &std::process::Output) -> String {
    match String::from_utf8_lossy(&out.stderr).trim() {
        "" => out.status.to_string(),
        said => format!("{}: {said}", out.status),
    }
}

/// The key that identifies this caller's seat on a place: the caller's host,
/// the repository and the seat name.
pub fn seat_key(root: &Path, seat: &str) -> Result<String> {
    let caller = String::from_utf8(repository::output(&mut Command::new("hostname"))?)?;
    let identity = receipt::digest(
        format!(
            "{}\n{}\n{seat}",
            caller.trim(),
            root.canonicalize()?.display()
        )
        .as_bytes(),
    );
    Ok(format!("{}-{seat}", &identity[..24]))
}

/// Asks the place to stop a run: as its own caller, or as another with a
/// reason. Returns the place's reply.
pub fn stop(
    root: &Path,
    place: &Place,
    record: &Receipt,
    why: &str,
    other: bool,
    signal: i32,
) -> Result<String> {
    let seat = seat_key(root, &record.seat)?;
    let who = format!(
        "{}:{}",
        String::from_utf8_lossy(&repository::output(&mut Command::new("hostname"))?).trim(),
        std::process::id()
    );
    let signal = signal.to_string();
    let mut args = vec![
        "stop", &record.id, "--seat", &seat, "--who", &who, "--why", why, "--signal", &signal,
    ];
    if other {
        args.push("--other");
    }
    let out = place.verb(&args)?.output()?;
    let reply = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() {
        Ok(reply)
    } else if reply.is_empty() {
        Err(failed(&out).into())
    } else {
        Err(reply.into())
    }
}

/// Completes the receipt from the place's final record, if it is final:
/// bound to the request's token and commit. The record names the binary
/// that ran, which a later install on the place does not change. Artifacts
/// are fetched and checked.
pub fn complete(root: &Path, place: &Place, record: &mut Receipt) -> Result<()> {
    use crate::artifacts::{self, Artifact, Kind, Status};
    let out = place.verb(&["record", &record.id])?.output()?;
    if out.status.code() == Some(3) {
        return Err(format!("run {} is not final yet", record.id).into());
    }
    if !out.status.success() {
        return Err(format!("reading the record of run {}: {}", record.id, failed(&out)).into());
    }
    let result: crate::place::HostResult = serde_json::from_slice(&out.stdout)?;
    if Some(&result.token) != record.token.as_ref()
        || Some(&result.commit) != record.commit.as_ref()
        || result.protocol != receipt::PROTOCOL
    {
        return Err("host result binding mismatch".into());
    }
    record.state = Some("final".into());
    record.collection = result.collection;
    record.outcome = result.outcome;
    record.exit_code = result.exit_code;
    record.reason = result.reason;
    record.host_binary_sha256 = Some(result.binary_sha256);
    record.host_source_rev = result.source_rev;
    record.host_exe = result.exe;
    record.custody = result.custody;
    record.lease = result.lease;
    record.admission = result.admission;
    record.pruned = result.pruned;
    record.output = result.output;
    record.deadline_ms = result.deadline_ms;
    record.stopped_by = result.stopped_by;
    // The seat book (SPEC §9): recorded at acceptance; a failure then is
    // retried here, and a failure now is a note on the final receipt.
    if let Err(error) = seat_key(root, &record.seat)
        .and_then(|key| crate::seats::ensure(&key, &record.machine, root, &record.seat))
    {
        note(&mut record.reason, format!("seat book: {error}"));
    }
    // The verdict is the place's; its artifacts have their own status
    // (SPEC §8), and only files that status vouches for stay on disk.
    let status = result
        .artifacts_status
        .map(serde_json::from_value::<Status>)
        .transpose();
    let listed = result
        .artifacts
        .map(serde_json::from_value::<Vec<Artifact>>)
        .transpose();
    // Emptied on every path, files and list, so nothing earlier outlives a
    // status; one that cannot be emptied vouches for nothing.
    let destination = root.join(".ferry/runs").join(&record.id);
    let cleared = match fs::remove_dir_all(&destination) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    };
    record.artifacts.clear();
    record.artifacts_status = Some(if let Err(e) = cleared {
        artifacts::failed(
            Kind::Archive,
            None,
            format!("an earlier run directory remains: {e}"),
        )
    } else {
        match (status, listed, result.artifacts_sha256) {
            (Err(e), ..) | (_, Err(e), _) => {
                artifacts::failed(Kind::Archive, None, format!("unreadable: {e}"))
            }
            (Ok(Some(status)), ..) => status,
            (Ok(None), Ok(Some(listed)), Some(_)) => {
                let archive = if execution::fail_at("artifact-transfer") {
                    Err("artifact-transfer: injected by FERRY_FAIL_AT".into())
                } else {
                    place.read(&record.id, "artifacts.tar")
                };
                let status = artifacts::fetch(archive, &listed, &destination);
                if status == Status::Fetched {
                    record.artifacts = listed;
                }
                status
            }
            // A place before per-file artifacts sends no list or status.
            (Ok(None), Ok(_), _) => {
                let why =
                    "the place predates per-file artifacts: install the same ferry on both ends";
                artifacts::failed(
                    Kind::Collection,
                    None,
                    result.artifacts_error.as_deref().unwrap_or(why),
                )
            }
        }
    });
    Ok(())
}
