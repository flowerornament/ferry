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
    time::Duration,
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
        return Ok(vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ControlMaster=no".into(),
            "-o".into(),
            "ControlPath=none".into(),
        ]);
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
    Ok(vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        // Bounds how long a master outlives a caller that was killed.
        "ControlPersist=10".into(),
        "-o".into(),
        format!("ControlPath={socket}"),
    ])
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
    let ssh_command = std::iter::once("ssh".to_owned())
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
    let pushed = repository::output(git(&["push", "--no-verify"])?.arg(refspec));
    let Err(error) = pushed else {
        return Ok(());
    };
    let (commit, reference) = refspec.split_once(':').ok_or("invalid refspec")?;
    // Any failure to check keeps the push's own error. ls-remote matches
    // pattern tails, so only the line naming exactly this ref counts.
    let listed =
        git(&["ls-remote"]).and_then(|mut command| repository::output(command.arg(reference)));
    let Ok(listed) = listed else {
        return Err(error);
    };
    let published = String::from_utf8_lossy(&listed).lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some(commit) && fields.next() == Some(reference)
    });
    if published { Ok(()) } else { Err(error) }
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
                    .args(["-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3"])
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
            Place::Remote(machine) => Ok(serde_json::from_slice(&repository::output(
                ssh(&machine.ssh)?.arg(format!("{} host-info", machine.binary)),
            )?)?),
        }
    }

    /// A run's file under the place's state root (`runs/<id>/<name>`).
    fn read(&self, id: &str, name: &str) -> Result<Vec<u8>> {
        config::identifier(id)?;
        match self {
            Place::Local(state) => Ok(fs::read(crate::record::run_dir(state, id).join(name))?),
            Place::Remote(machine) => repository::output(ssh(&machine.ssh)?.arg(format!(
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
    init_store(
        Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&bare),
    )?;
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

/// Initializes the place's store, again if it exists (harmless). Two first
/// runs may race on its config lock, so a failure is retried a few times.
fn init_store(command: &mut Command) -> Result<()> {
    let mut tries = 0;
    loop {
        match repository::output(command) {
            Ok(_) => return Ok(()),
            Err(_) if tries < 10 => tries += 1,
            Err(error) => return Err(error),
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
    if json {
        execution::emit(
            serde_json::json!({"protocol": receipt::PROTOCOL, "event": "submitted", "id": request.id, "pid": reply["pid"]})
                .to_string()
                .as_bytes(),
        );
    }
    finish(root, place, record, json);
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
pub fn finish(root: &Path, place: &Place, record: &mut Receipt, json: bool) {
    let id = record.id.clone();
    // After the place accepted the run, only its final record decides: any
    // other failure leaves the receipt submitted (SPEC §4, §8).
    let followed = follow(root, place, record, json);
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
fn follow(root: &Path, place: &Place, record: &Receipt, json: bool) -> Result<Followed> {
    let offset = std::sync::Arc::new(AtomicU64::new(0));
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
            Some(4) => return Err(format!("the place has no run {}", record.id).into()),
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
pub fn retire(place: &Place, key: &str) -> Result<serde_json::Value> {
    let info = place.info()?;
    if !info.capabilities.iter().any(|c| c == "retire") {
        return Ok(serde_json::json!({
            "refused": "retire",
            "why": format!(
                "the place runs ferry {}, which lacks retire",
                info.source_rev.as_deref().unwrap_or("of unknown revision")
            ),
            "next": format!("install this caller's ferry ({}) there", receipt::source_rev()),
        }));
    }
    let out = place.verb(&["retire", key])?.output()?;
    serde_json::from_slice(&out.stdout)
        .map_err(|_| format!("retire gave no reply ({})", out.status).into())
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
    } else {
        Err(reply.into())
    }
}

/// Completes the receipt from the place's final record, if it is final:
/// bound to the request's token and commit. The record names the binary
/// that ran, which a later install on the place does not change. Artifacts
/// are fetched and checked.
pub fn complete(root: &Path, place: &Place, record: &mut Receipt) -> Result<()> {
    let out = place.verb(&["record", &record.id])?.output()?;
    if out.status.code() == Some(3) {
        return Err(format!("run {} is not final yet", record.id).into());
    }
    if !out.status.success() {
        return Err(format!("reading the record of run {}: {}", record.id, out.status).into());
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
    if let Some(error) = result.artifacts_error {
        note(
            &mut record.reason,
            format!("artifact collection failed: {error}"),
        );
    }
    // The verdict is the place's; a failed transfer of its artifacts is a
    // note on it, never a reason to withhold it.
    if let Some(expected) = result.artifacts_sha256 {
        let fetched = (|| -> Result<()> {
            let destination = root.join(".ferry/runs").join(&record.id);
            fs::create_dir_all(&destination)?;
            let archive = destination.join("artifacts.tar");
            fs::write(&archive, place.read(&record.id, "artifacts.tar")?)?;
            if receipt::file_digest(&archive)? != expected {
                return Err("artifact transfer integrity failed".into());
            }
            crate::artifacts::unpack(&archive, &destination)
        })();
        if let Err(error) = fetched {
            note(
                &mut record.reason,
                format!("artifacts not fetched: {error}"),
            );
        }
    }
    Ok(())
}
