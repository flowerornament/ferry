//! SSH carries a typed request and heartbeat stream, never interpolated recipe code.
use crate::{
    config,
    execution::{self, Request},
    receipt::{self, Outcome, Receipt},
    repository,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, Write},
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

/// Closing a failed conversation ends heartbeats and triggers host cleanup.
/// An SSH client is killed at once: the host sees the connection close. A
/// local host-run is the supervisor itself, so it is given `grace` to stop
/// the work and release, and killed only after that, with a note.
struct Connection {
    child: std::process::Child,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    grace: Duration,
}
impl Drop for Connection {
    fn drop(&mut self) {
        // The heartbeat thread owns the host's stdin: once stop is set it
        // exits within one beat and drops it, and the host reads EOF.
        self.stop.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + self.grace;
        while std::time::Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if !self.grace.is_zero() {
            execution::warn(&format!(
                "ferry: host-run {} did not stop within {:?}; killing it",
                self.child.id(),
                self.grace
            ));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HostInfo {
    pub protocol: u32,
    pub binary_sha256: String,
    pub os_arch: String,
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

pub fn mirror(
    root: &Path,
    declarations: &[config::Mirror],
    host: Option<&config::Machine>,
) -> Result<Vec<execution::Mirrored>> {
    let mut mirrors = vec![];
    for declaration in declarations {
        let source = root.join(&declaration.path);
        let commit = repository::commit(&source, Some(&declaration.commit))?;
        let key = receipt::digest(declaration.url.as_bytes());
        let store = format!(".local/state/ferry/mirrors/{key}.git");
        if let Some(host) = host {
            config::identifier(&host.ssh)?;
            repository::output(ssh(&host.ssh)?.arg(format!(
                "mkdir -p .local/state/ferry/mirrors; test -d {store} || git init --bare {store}"
            )))?;
            push(
                &source,
                &host.ssh,
                &store,
                &format!("{commit}:refs/ferry/{commit}"),
            )?;
            pin_mirror(&store, &commit, Some(host))?;
        } else {
            let store = root.join(".ferry/local/mirrors").join(format!("{key}.git"));
            fs::create_dir_all(store.parent().ok_or("invalid mirror directory")?)?;
            if !store.exists() {
                repository::output(Command::new("git").args(["init", "--bare"]).arg(&store))?;
            }
            repository::output(
                Command::new("git")
                    .arg("--git-dir")
                    .arg(&store)
                    .arg("fetch")
                    .arg(repository::git_dir(&source)?)
                    .arg(&commit),
            )?;
            pin_mirror(
                store.to_str().ok_or("non-UTF-8 mirror path")?,
                &commit,
                None,
            )?;
        }
        mirrors.push(execution::Mirrored {
            url: declaration.url.clone(),
            commit,
            key,
        });
    }
    Ok(mirrors)
}

fn pin_mirror(store: &str, commit: &str, host: Option<&config::Machine>) -> Result<()> {
    // Ordinary dependency fetchers request heads, not Ferry's custody refs.
    // A compare-and-set makes each content-addressed head an immutable GC pin.
    let script = "set -eu; head=$(git --git-dir=\"$1\" symbolic-ref HEAD); case $head in refs/heads/ferry/*) exit 125;; esac; ref=refs/heads/ferry/$2; git --git-dir=\"$1\" update-ref \"$ref\" \"$2\" 0000000000000000000000000000000000000000 2>/dev/null || test \"$(git --git-dir=\"$1\" rev-parse --verify \"$ref\")\" = \"$2\"";
    if let Some(host) = host {
        let command = ["sh", "-c", script, "ferry-mirror", store, commit]
            .into_iter()
            .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");
        repository::output(ssh(&host.ssh)?.arg(command))?;
    } else {
        repository::output(Command::new("sh").args(["-c", script, "ferry-mirror", store, commit]))?;
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HostResult {
    pub protocol: u32,
    pub token: String,
    pub commit: String,
    pub outcome: Outcome,
    pub exit_code: i32,
    pub reason: Option<String>,
    pub binary_sha256: String,
    pub artifacts_sha256: Option<String>,
    pub artifacts_error: Option<String>,
    #[serde(default)]
    pub collection: Option<execution::Collection>,
    #[serde(default)]
    pub source_rev: Option<String>,
    #[serde(default)]
    pub exe: Option<String>,
    #[serde(default)]
    pub custody: Option<crate::custody::Release>,
    #[serde(default)]
    pub lease: Option<execution::Lease>,
    #[serde(default)]
    pub admission: Option<crate::admission::Admission>,
    /// Output lines and events the host could not deliver to its client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dropped: Option<u64>,
}
impl HostResult {
    /// A result that says nothing ran yet: refused 125 until the run says more.
    fn refused(request: &Request) -> Result<Self> {
        Ok(Self {
            protocol: receipt::PROTOCOL,
            token: request.token.clone(),
            commit: request.commit.clone(),
            outcome: Outcome::Refused,
            exit_code: 125,
            reason: None,
            binary_sha256: receipt::digest(&fs::read(std::env::current_exe()?)?),
            artifacts_sha256: None,
            artifacts_error: None,
            collection: None,
            source_rev: Some(receipt::source_rev()),
            exe: receipt::exe_path(),
            custody: None,
            lease: None,
            admission: None,
            output_dropped: None,
        })
    }
}

/// How long one of the host's own events may wait for room in the output
/// channel before it is dropped and counted.
const EVENT_BOUND: Duration = Duration::from_secs(5);
/// How long host-run waits for its final events to reach the client.
pub const FINAL_EMIT: Duration = Duration::from_secs(5);

/// Queues one protocol event for the output writer. Output is data: it never
/// blocks the supervisor for long and never stops the run.
fn event(value: &serde_json::Value) {
    crate::output::event(value, EVENT_BOUND);
}

/// Refuses before acceptance: the event, then a bounded wait for it to
/// reach the client before host-run exits.
fn refuse(reason: &str) -> i32 {
    event(&serde_json::json!({"protocol":1,"event":"refused","reason":reason}));
    crate::output::flush(FINAL_EMIT);
    125
}

/// Archives the recipe's declared artifacts. A failure, the digest's
/// included, is recorded in the result and never changes its verdict.
fn collect_artifacts(root: &Path, seat: &Path, request: &Request, result: &mut HostResult) {
    if request.recipe.artifacts.is_empty() {
        return;
    }
    let output = root.join("runs").join(&request.id).join("artifacts.tar");
    match crate::artifacts::collect(seat, &request.recipe.artifacts, &output) {
        Ok(()) => {
            let digest = if execution::fail_at("artifact-digest") {
                Err(std::io::Error::other("injected by FERRY_FAIL_AT"))
            } else {
                receipt::file_digest(&output)
            };
            match digest {
                Ok(digest) => result.artifacts_sha256 = Some(digest),
                Err(e) => result.artifacts_error = Some(format!("digest: {e}")),
            }
        }
        Err(e) => result.artifacts_error = Some(e.to_string()),
    }
}

/// Records what did not travel over the stream: lines the host could not
/// deliver, and a verdict read from the host record instead of the stream.
fn note_delivery(record: &mut Receipt, host_dropped: Option<u64>, from_record: bool) {
    if let Some(dropped) = host_dropped {
        record.output_dropped.insert("host".into(), dropped);
    }
    if from_record {
        note(
            &mut record.reason,
            "the host-result did not arrive; the verdict is from the host record".into(),
        );
    }
}

/// Adds a note to a reason, keeping what was already there.
fn note(reason: &mut Option<String>, text: String) {
    *reason = Some(match reason.take() {
        Some(previous) => format!("{previous}; {text}"),
        None => text,
    });
}
/// Reads, validates and reserves a request. Any error here is a refusal.
fn admit(input: &mut impl BufRead, state: Option<PathBuf>) -> Result<(Request, PathBuf, fs::File)> {
    let mut line = String::new();
    input
        .read_line(&mut line)
        .map_err(|e| format!("reading the request: {e}"))?;
    let request: Request =
        serde_json::from_str(&line).map_err(|e| format!("parsing the request: {e}"))?;
    execution::validate(&request).map_err(|e| format!("validating the request: {e}"))?;
    let root = match state {
        Some(state) => state,
        None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME missing")?)
            .join(".local/state/ferry"),
    };
    let records = root.join("records");
    fs::create_dir_all(&records).map_err(|e| format!("creating {}: {e}", records.display()))?;
    let record_path = records.join(format!("{}.json", request.id));
    // A reservation refuses reuse even after a crash, before acquiring any seat.
    let record = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&record_path)
        .map_err(|e| format!("reserving {}: {e}", record_path.display()))?;
    Ok((request, root, record))
}
/// Runs one request read from stdin, as the run's supervisor. `state` is the
/// state root: `~/.local/state/ferry` by default, or a repository's
/// `.ferry/local` for the local route.
pub fn host_run(state: Option<PathBuf>) -> Result<i32> {
    let mut input = std::io::BufReader::new(std::io::stdin());
    // Everything before `accepted` is a refusal: nothing ran. Say so in the
    // protocol, so the caller records refused 125 rather than a lost run.
    let admitted = admit(&mut input, state);
    let (request, root, mut record) = match admitted {
        Ok(admitted) => admitted,
        Err(error) => return Ok(refuse(&error.to_string())),
    };
    let stop = execution::stopped()?;
    let reader_stop = stop.clone();
    thread::spawn(move || {
        loop {
            let mut heartbeat = String::new();
            match input.read_line(&mut heartbeat) {
                Ok(0) | Err(_) => break,
                Ok(_) if heartbeat.trim() == "heartbeat" => {}
                Ok(_) => break,
            }
        }
        reader_stop.store(true, Ordering::SeqCst);
    });
    let mut result = HostResult::refused(&request)?;
    // Before acceptance, so a failure here is a refusal: nothing ran.
    if let Err(error) = crate::custody::supervise() {
        return Ok(refuse(&format!("supervising: {error}")));
    }
    // The pid names this supervisor, so an operator can stop exactly it.
    event(
        &serde_json::json!({"protocol":1,"event":"accepted","id":request.id,"pid":std::process::id()}),
    );
    let executed: Result<execution::Ended> = (|| {
        let (seat, lease, host_lease) =
            execution::prepare(&root, &request, &stop, &mut result.admission)?;
        event(
            &serde_json::json!({"protocol":1,"event":"prepared","commit":request.commit,"seat":request.seat}),
        );
        let locks = execution::inherited(&lease, &host_lease);
        let finished = execution::run_released(&root, &seat, &request, &stop, &locks);
        // Custody first: it is recorded whatever else failed (S8). A failed
        // release, collection or artifact digest is recorded, and never
        // changes the verdict of work that ran.
        if let Some(error) = &finished.custody.error {
            note(
                &mut result.reason,
                format!("custody release failed: {error}"),
            );
        }
        result.custody = Some(finished.custody);
        // Stamped while held; the locks drop as this scope ends.
        result.lease = Some(host_lease.stamp());
        result.collection = finished.collection;
        if let Some(error) = result.collection.as_ref().and_then(|c| c.error.as_ref()) {
            note(&mut result.reason, format!("collection: {error}"));
        }
        let ended = finished.executed?;
        if matches!(
            ended,
            execution::Ended::Result(_) | execution::Ended::Timeout
        ) {
            collect_artifacts(&root, &seat, &request, &mut result);
        }
        result.lease = Some(host_lease.stamp());
        Ok(ended)
    })();
    // One verdict, from the first terminal fact (Rule B).
    let (outcome, exit_code, reason) = execution::verdict(
        &executed.map_err(|error| error.to_string()),
        execution::stop_cause(&stop),
        execution::attempted(&root, &request),
    );
    result.outcome = outcome;
    result.exit_code = exit_code;
    if let Some(reason) = reason {
        result.reason = Some(match result.reason.take() {
            Some(notes) => format!("{reason}; {notes}"),
            None => reason,
        });
    }
    let dropped = crate::output::dropped();
    if dropped > 0 {
        result.output_dropped = Some(dropped);
    }
    // The host record is the truth; a failure to write it is noted in the
    // result the caller still receives.
    let stored = serde_json::to_vec(&result)
        .map_err(std::io::Error::from)
        .and_then(|bytes| record.write_all(&bytes))
        .and_then(|()| record.sync_all());
    if let Err(error) = stored {
        note(
            &mut result.reason,
            format!("host record not written: {error}"),
        );
    }
    event(&serde_json::json!({"protocol":1,"event":"host-result","result":result}));
    // A stalled client never holds the supervisor: the caller can read the
    // record instead.
    if !crate::output::flush(FINAL_EMIT) {
        execution::warn(
            "ferry: host-run: the client did not take the host-result in time; the host record has it",
        );
    }
    Ok(result.exit_code)
}

/// Runs a request on this machine through a freshly spawned `ferry host-run`
/// on the repository's `.ferry/local` state, over the same protocol as the
/// remote route. That child is the run's supervisor: just spawned, it has no
/// children of its own, so custody can attribute nothing outside the run.
/// Longer than the host's worst-case stop after its input closes: session
/// teardown, the collect hook and its teardown, stopping survivors, and the
/// final emit, plus a margin.
fn supervisor_grace() -> Duration {
    execution::SESSION_CLEANUP
        + execution::COLLECT_TIMEOUT
        + execution::SESSION_CLEANUP
        + crate::custody::STOP_GRACE
        + crate::custody::KILL_GRACE
        + FINAL_EMIT
        + Duration::from_secs(10)
}

pub fn local(root: &Path, request: &Request, record: &mut Receipt, json: bool) -> Result<()> {
    let state = root.join(".ferry/local");
    fs::create_dir_all(&state)?;
    let bare = state.join("repo.git");
    if !bare.exists() {
        repository::output(Command::new("git").args(["init", "--bare"]).arg(&bare))?;
    }
    repository::output(
        Command::new("git")
            .arg("--git-dir")
            .arg(&bare)
            .arg("fetch")
            .arg(repository::git_dir(root)?)
            .arg(&request.commit),
    )?;
    for commit in request.companion_refs.values() {
        repository::output(
            Command::new("git")
                .arg("--git-dir")
                .arg(&bare)
                .arg("fetch")
                .arg(repository::git_dir(root)?)
                .arg(commit),
        )?;
    }
    record.outcome = Outcome::Lost;
    record.exit_code = 255;
    let stop = execution::stopped()?;
    // The last moment before the host is asked to run anything.
    execution::interrupted()?;
    let child = Command::new(std::env::current_exe()?)
        .arg("host-run")
        .arg("--state")
        .arg(&state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let (mut stream, status) = converse(child, request, &stop, json, supervisor_grace())?;
    record.output_dropped.clone_from(&stream.dropped);
    // The record on disk is the truth when the host-result did not arrive.
    // The record is reserved only after admission and bound by the token
    // checks below, so it can stand in even when no event arrived at all.
    let from_record = stream.result.is_none() && stream.refusal.is_none();
    if from_record {
        stream.result = fs::read(state.join("records").join(format!("{}.json", request.id)))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<HostResult>(&bytes).ok());
    }
    if let Some(result) = stream.result {
        if result.token != request.token
            || result.commit != request.commit
            || result.protocol != receipt::PROTOCOL
        {
            return Err("host result binding mismatch".into());
        }
        record.collection.clone_from(&result.collection);
        record.outcome = result.outcome;
        record.exit_code = result.exit_code;
        record.reason = result.reason;
        record.host_binary_sha256 = Some(result.binary_sha256);
        record.host_source_rev = result.source_rev;
        record.host_exe = result.exe;
        record.custody = result.custody;
        record.lease = result.lease;
        record.admission = result.admission;
        note_delivery(record, result.output_dropped, from_record);
        if record.host_binary_sha256.as_deref() != Some(&record.caller_binary_sha256) {
            record.outcome = Outcome::Lost;
            record.exit_code = 255;
            return Err("ferry binary changed during run".into());
        }
        if let Some(error) = result.artifacts_error {
            note(
                &mut record.reason,
                format!("artifact collection failed: {error}"),
            );
        }
        if let Some(expected) = result.artifacts_sha256 {
            let archive = state.join("runs").join(&request.id).join("artifacts.tar");
            if receipt::file_digest(&archive)? != expected {
                return Err("artifact integrity failed".into());
            }
            crate::artifacts::unpack(&archive, &root.join(".ferry/runs").join(&request.id))?;
        }
    } else if let Some(reason) = stream.refusal {
        record.outcome = Outcome::Refused;
        record.exit_code = 125;
        record.reason = Some(reason);
    } else {
        record.reason = Some(format!("no result from host-run: {status}"));
    }
    Ok(())
}

/// Sends the request to a host-run child, keeps it alive with heartbeats,
/// and relays its events until it ends. Dropping the conversation early
/// ends the heartbeats, so the host stops the work and releases.
fn converse(
    child: std::process::Child,
    request: &Request,
    stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    json: bool,
    grace: Duration,
) -> Result<(HostStream, std::process::ExitStatus)> {
    let mut connection = Connection {
        child,
        stop: stop.clone(),
        grace,
    };
    let child = &mut connection.child;
    let mut stdin = child.stdin.take().ok_or("missing host stdin")?;
    serde_json::to_writer(&mut stdin, request)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    let heartbeat_stop = stop.clone();
    let heartbeat = thread::spawn(move || {
        while !heartbeat_stop.load(Ordering::SeqCst) {
            if stdin
                .write_all(b"heartbeat\n")
                .and_then(|()| stdin.flush())
                .is_err()
            {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }
    });
    let stdout = child.stdout.take().ok_or("missing host stdout")?;
    let stream = read_stream(stdout, json, stop)?;
    stop.store(true, Ordering::SeqCst);
    heartbeat.join().map_err(|_| "heartbeat thread panicked")?;
    let status = child.wait()?;
    Ok((stream, status))
}

pub fn remote(
    root: &Path,
    machine: &config::Machine,
    request: &Request,
    record: &mut Receipt,
    json: bool,
) -> Result<()> {
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
    // Only validated SSH alias and relative binary path reach a shell. All repo
    // data travels through git or the typed stdin protocol.
    record.outcome = Outcome::Lost;
    record.exit_code = 255;
    let info: HostInfo = serde_json::from_slice(&repository::output(
        ssh(&machine.ssh)?.arg(format!("{} host-info", machine.binary)),
    )?)?;
    if let Err(missing) = receipt::provides(&request.provider_requires, &info.capabilities) {
        refused_by(record, &info);
        record.outcome = Outcome::Refused;
        record.exit_code = 125;
        return Err(format!("host {missing}").into());
    }
    if info.protocol != receipt::PROTOCOL {
        record.outcome = Outcome::Refused;
        record.exit_code = 125;
        return Err("host protocol mismatch".into());
    }
    for requirement in &request.recipe.requires {
        if (requirement.starts_with("linux-") || requirement.starts_with("macos-"))
            && *requirement != info.os_arch
        {
            record.outcome = Outcome::Refused;
            record.exit_code = 125;
            return Err("reported host architecture does not meet requirements".into());
        }
    }
    repository::output(ssh(&machine.ssh)?.arg("mkdir -p .local/state/ferry; test -d .local/state/ferry/repo.git || git init --bare .local/state/ferry/repo.git"))?;
    push(
        root,
        &machine.ssh,
        ".local/state/ferry/repo.git",
        &format!(
            "{}:refs/ferry/{}/{}",
            request.commit, request.seat, request.commit
        ),
    )?;
    push_companions(root, machine, request)?;
    let stop = execution::stopped()?;
    // The last moment before the host is asked to run anything.
    execution::interrupted()?;
    let child = ssh(&machine.ssh)?
        .arg(format!("{} host-run", machine.binary))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let (mut stream, status) = converse(child, request, &stop, json, Duration::ZERO)?;
    record.output_dropped.clone_from(&stream.dropped);
    // The record on disk is the truth when the host-result did not arrive.
    // The record is reserved only after admission and bound by the token
    // checks below, so it can stand in even when no event arrived at all.
    let from_record = stream.result.is_none() && stream.refusal.is_none();
    if from_record {
        stream.result = repository::output(ssh()?.arg(&machine.ssh).arg(format!(
            "cat .local/state/ferry/records/{}.json",
            request.id
        )))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<HostResult>(&bytes).ok());
    }
    if let Some(result) = stream.result {
        let dropped = result.output_dropped;
        accept_result(root, machine, request, record, &info, result)?;
        note_delivery(record, dropped, from_record);
    } else if let Some(reason) = stream.refusal {
        refused_by(record, &info);
        record.outcome = Outcome::Refused;
        record.exit_code = 125;
        record.reason = Some(format!("host refused before acceptance: {reason}"));
    } else if !stream.accepted && status.code() == Some(125) {
        // A host from before the refusal event exits 125 for every error it
        // meets before acceptance, and the recipe cannot have started.
        refused_by(record, &info);
        record.outcome = Outcome::Refused;
        record.exit_code = 125;
        record.reason = Some("host refused before acceptance (exit 125; see its stderr)".into());
    } else {
        record.reason = Some(format!("no host result: {status}"));
    }
    Ok(())
}

/// A refusal names the host binary that refused, as host-info reported it.
fn refused_by(record: &mut Receipt, info: &HostInfo) {
    record.host_binary_sha256 = Some(info.binary_sha256.clone());
    record.host_source_rev.clone_from(&info.source_rev);
    record.host_exe.clone_from(&info.exe);
}

/// What the host said on the protocol stream.
struct HostStream {
    result: Option<HostResult>,
    accepted: bool,
    refusal: Option<String>,
    /// Lines this caller could not write, per stream.
    dropped: BTreeMap<String, u64>,
}

/// Relays recipe output and collects the host's protocol verdicts.
///
/// Output is data (Rule A): it goes to a writer thread over a bounded
/// channel. A slow reader of this caller's output slows the run through
/// backpressure. A stream that cannot be written (`| head -1`, a closed
/// stderr) is dropped and its lost lines counted; it never stops the run.
/// Only a signal (`stop`) makes this reader give up waiting for room, so a
/// stalled consumer cannot keep a signalled caller from its receipt.
fn read_stream(
    stdout: impl std::io::Read,
    json: bool,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<HostStream> {
    let (sender, receiver) = std::sync::mpsc::sync_channel::<(bool, Vec<u8>)>(1024);
    let dropped = std::sync::Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let writer = {
        let dropped = dropped.clone();
        let done = done.clone();
        thread::spawn(move || {
            use std::io::Write as _;
            let mut dead = [false, false];
            for (to_stderr, bytes) in receiver {
                let index = usize::from(to_stderr);
                if !dead[index] {
                    let written = if to_stderr {
                        let mut err = std::io::stderr().lock();
                        err.write_all(&bytes).and_then(|()| err.write_all(b"\n"))
                    } else {
                        let mut out = std::io::stdout().lock();
                        out.write_all(&bytes)
                            .and_then(|()| out.write_all(b"\n"))
                            .and_then(|()| out.flush())
                    };
                    dead[index] = written.is_err();
                }
                if dead[index] {
                    dropped[index].fetch_add(1, Ordering::SeqCst);
                }
            }
            done.store(true, Ordering::SeqCst);
        })
    };
    let mut stream = HostStream {
        result: None,
        accepted: false,
        refusal: None,
        dropped: BTreeMap::new(),
    };
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line?;
        let value: serde_json::Value = serde_json::from_str(&line)?;
        if value.get("protocol").and_then(serde_json::Value::as_u64) != Some(1) {
            return Err("host protocol mismatch".into());
        }
        match value.get("event").and_then(|v| v.as_str()) {
            Some("accepted") => stream.accepted = true,
            Some("refused") if !stream.accepted => {
                stream.refusal = Some(value["reason"].as_str().unwrap_or("").to_owned());
            }
            Some("host-result") => {
                stream.result = Some(serde_json::from_value::<HostResult>(
                    value["result"].clone(),
                )?);
                continue;
            }
            _ => {}
        }
        let message = if json {
            Some((false, line.into_bytes()))
        } else {
            value.get("text").and_then(|v| v.as_str()).map(|text| {
                let bytes = value
                    .get("base64")
                    .and_then(|v| v.as_str())
                    .and_then(receipt::unbase64)
                    .unwrap_or_else(|| text.as_bytes().to_vec());
                (
                    value.get("stream").and_then(|v| v.as_str()) == Some("stderr"),
                    bytes,
                )
            })
        };
        if let Some(message) = message {
            offer(&sender, message, stop, &dropped);
        }
    }
    drop(sender);
    // Everything relayed is written before the receipt, unless signalled.
    while !done.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(10));
    }
    if done.load(Ordering::SeqCst) {
        let _ = writer.join();
    }
    for (index, name) in ["stdout", "stderr"].into_iter().enumerate() {
        let count = dropped[index].load(Ordering::SeqCst);
        if count > 0 {
            stream.dropped.insert(name.to_owned(), count);
        }
    }
    Ok(stream)
}

/// Hands one line to the caller's writer, waiting for room unless the
/// caller has been signalled; then the line is dropped and counted.
fn offer(
    sender: &std::sync::mpsc::SyncSender<(bool, Vec<u8>)>,
    message: (bool, Vec<u8>),
    stop: &std::sync::atomic::AtomicBool,
    dropped: &[AtomicU64; 2],
) {
    let index = usize::from(message.0);
    let mut message = message;
    loop {
        match sender.try_send(message) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return,
            Err(std::sync::mpsc::TrySendError::Full(back)) => {
                if stop.load(Ordering::SeqCst) {
                    dropped[index].fetch_add(1, Ordering::SeqCst);
                    return;
                }
                message = back;
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn push_companions(root: &Path, machine: &config::Machine, request: &Request) -> Result<()> {
    for commit in request.companion_refs.values() {
        push(
            root,
            &machine.ssh,
            ".local/state/ferry/repo.git",
            &format!(
                "{commit}:refs/ferry/{}/{}/companions/{commit}",
                request.seat, request.id
            ),
        )?;
    }
    Ok(())
}

fn accept_result(
    root: &Path,
    machine: &config::Machine,
    request: &Request,
    record: &mut Receipt,
    info: &HostInfo,
    result: HostResult,
) -> Result<()> {
    record.collection.clone_from(&result.collection);
    if result.token != request.token
        || result.commit != request.commit
        || result.protocol != receipt::PROTOCOL
    {
        return Err("host result binding mismatch".into());
    }
    record.outcome = result.outcome;
    record.exit_code = result.exit_code;
    record.reason = result.reason;
    record.host_binary_sha256 = Some(result.binary_sha256);
    record.host_source_rev = result.source_rev;
    record.custody = result.custody;
    record.lease = result.lease;
    record.admission = result.admission;
    record.host_exe = result.exe;
    if record.host_binary_sha256.as_deref() != Some(&info.binary_sha256) {
        record.outcome = Outcome::Lost;
        record.exit_code = 255;
        return Err("host binary changed during run".into());
    }
    if let Some(error) = result.artifacts_error {
        note(
            &mut record.reason,
            format!("artifact collection failed: {error}"),
        );
    }
    if let Some(expected) = result.artifacts_sha256 {
        let destination = root.join(".ferry/runs").join(&request.id);
        fs::create_dir_all(&destination)?;
        let archive = destination.join("artifacts.tar");
        let file = fs::File::create(&archive)?;
        let status = ssh(&machine.ssh)?
            .arg(format!(
                "cat .local/state/ferry/runs/{}/artifacts.tar",
                request.id
            ))
            .stdout(file)
            .status()?;
        if !status.success() || receipt::file_digest(&archive)? != expected {
            return Err("artifact transfer integrity failed".into());
        }
        crate::artifacts::unpack(&archive, &destination)?;
    }
    Ok(())
}
