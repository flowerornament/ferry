//! A run (SPEC §3 Run): its session spawned with the locks inherited, the
//! deadline loop, the output copier, the stop that ends the session, and the
//! verdict, decided once.
use crate::{
    execution::{Request, fail_at, pause_at, signal_name, validate},
    limits::{COLLECT_TIMEOUT, KILL_GRACE, STOP_GRACE},
    receipt, repository,
};
use nix::{
    sys::signal::{Signal, kill},
    unistd::{Pid, getsid, setsid},
};
use serde::{Deserialize, Serialize};
use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// A run's session, by its leader: stopped once, on every path.
///
/// Invariant: no signal by pid or group is sent unless the leader is
/// unreaped. Its zombie pins its pid, which is the session and group id, so
/// nothing that is signalled can belong to another session. The leader is
/// reaped exactly once, after the last signal.
struct SessionOwner {
    child: std::process::Child,
    stopped: bool,
    reaped: bool,
}
impl SessionOwner {
    /// Signals the session; refused, as an error, once the leader is reaped.
    fn signal(&self, signal: Signal, errors: &mut Vec<String>) {
        let sid = self.child.id();
        if self.reaped {
            errors.push(format!(
                "refused {signal:?} to session {sid}: its leader is already reaped"
            ));
            return;
        }
        signal_session(sid, signal, errors);
    }

    /// Whether the leader has exited, without reaping it: an unreaped
    /// child's pid cannot be reused, and an exited one is a zombie, whose
    /// process details cannot be read.
    fn leader_exited(&self) -> bool {
        let pid = self.child.id();
        #[cfg(target_os = "macos")]
        {
            i32::try_from(pid).is_ok_and(|pid| {
                libproc::proc_pid::pidinfo::<libproc::bsd_info::BSDInfo>(pid, 0).is_err()
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            i32::try_from(pid).is_ok_and(zombie)
        }
    }

    /// Reaps the leader, once, after the last signal.
    fn reap(&mut self, errors: &mut Vec<String>) {
        if let Err(e) = self.child.wait() {
            errors.push(format!("wait for session leader {}: {e}", self.child.id()));
        }
        self.reaped = true;
    }

    /// Stops the session (SPEC §7): TERM to every member, up to
    /// `STOP_GRACE` for them to leave, KILL, then up to `KILL_GRACE` for them
    /// to be gone, killing again whatever is left. It never gives up early:
    /// an error on the way is kept, and the stop goes on to the KILL and the
    /// leader's reaping: giving up would leave members running unrecorded.
    fn stop(&mut self) -> crate::custody::Stopped {
        self.stopped = true;
        let sid = self.child.id();
        let mut errors = Vec::new();
        if fail_at("session-term") {
            errors.push("session TERM: injected by FERRY_FAIL_AT".to_owned());
        } else {
            self.signal(Signal::SIGTERM, &mut errors);
            // A listing that fails cannot confirm the others left, so the
            // grace runs out.
            let until = Instant::now() + STOP_GRACE;
            while Instant::now() < until
                && session_list(sid).ok().is_none_or(|members| {
                    members
                        .iter()
                        .any(|pid| u32::try_from(*pid).ok() != Some(sid))
                })
            {
                thread::sleep(Duration::from_millis(20));
            }
        }
        let killing = !fail_at("session-kill");
        if killing {
            self.signal(Signal::SIGKILL, &mut errors);
        }
        // Also handles failure before the child called setsid.
        match self.child.kill() {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {}
            Err(e) => errors.push(format!("kill session leader {sid}: {e}")),
        }
        let until = Instant::now() + KILL_GRACE;
        let mut left = session_list(sid);
        while Instant::now() < until && !left.as_ref().is_ok_and(Vec::is_empty) {
            thread::sleep(Duration::from_millis(20));
            // Again, for whatever a failed signal or listing missed; the
            // first pass's errors are already kept, a refusal is not.
            if killing {
                let mut again = Vec::new();
                self.signal(Signal::SIGKILL, &mut again);
                errors.extend(again.into_iter().filter(|e| e.starts_with("refused")));
            }
            left = session_list(sid);
        }
        self.reap(&mut errors);
        let survivors = match left {
            // A member whose start cannot be read is dropped only on proven
            // absence; otherwise it is kept without a start, and the error
            // noted.
            Ok(pids) => Some(
                pids.into_iter()
                    .filter_map(|pid| {
                        match if fail_at("survivor-start") {
                            Err("survivor-start: injected by FERRY_FAIL_AT".into())
                        } else {
                            crate::record::start_time(pid)
                        } {
                            Ok(start) => Some(crate::custody::Survivor {
                                pid,
                                start: u64::try_from(start).ok(),
                            }),
                            Err(_)
                                if kill(Pid::from_raw(pid), None)
                                    == Err(nix::errno::Errno::ESRCH) =>
                            {
                                None
                            }
                            Err(error) => {
                                errors.push(format!("start of survivor {pid}: {error}"));
                                Some(crate::custody::Survivor { pid, start: None })
                            }
                        }
                    })
                    .collect(),
            ),
            Err(error) => {
                errors.push(format!("list session {sid}: {error}"));
                None
            }
        };
        crate::custody::Stopped { survivors, errors }
    }
}
impl Drop for SessionOwner {
    fn drop(&mut self) {
        if !self.stopped {
            crate::custody::note(self.stop());
        }
    }
}

/// The session's members, as the stop lists them (`session-list` injects a
/// failure).
fn session_list(sid: u32) -> Result<Vec<i32>> {
    if fail_at("session-list") {
        return Err("session-list: injected by FERRY_FAIL_AT".into());
    }
    Ok(process_ids()?
        .into_iter()
        .filter(|pid| {
            getsid(Some(Pid::from_raw(*pid)))
                .is_ok_and(|session| u32::try_from(session.as_raw()).ok() == Some(sid))
                && !zombie(*pid)
        })
        .collect())
}

/// A zombie has exited and holds nothing, though it is listed until its
/// parent reaps it: a session member that is a zombie is not a survivor.
#[cfg(target_os = "linux")]
fn zombie(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().next())
            == Some("Z")
    })
}

#[cfg(not(target_os = "linux"))]
fn zombie(_pid: i32) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn process_ids() -> Result<Vec<i32>> {
    // Apple's ps executable cannot be spawned in the Darwin Nix sandbox.
    // Read the process table directly, without weakening the sandbox.
    Ok(
        libproc::processes::pids_by_type(libproc::processes::ProcFilter::All)?
            .into_iter()
            .filter_map(|pid| i32::try_from(pid).ok())
            .collect(),
    )
}

// Read from /proc, not by running `ps`: a subprocess could hang and stall
// the stop past every bound. A /proc that
// cannot be read is a listing failure, which takes the group fallback.
#[cfg(not(target_os = "macos"))]
fn process_ids() -> Result<Vec<i32>> {
    Ok(fs::read_dir("/proc")?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect())
}
/// Signals every member of the session, keeping each error. When the
/// session cannot be listed, the leader's process group (the session, unless
/// a member moved to its own group) is signalled instead.
fn signal_session(sid: u32, signal: Signal, errors: &mut Vec<String>) {
    // Only through `SessionOwner::signal`, while the leader is unreaped.
    let members = match session_list(sid) {
        Ok(members) => members,
        Err(error) => {
            errors.push(format!("list session {sid} for {signal:?}: {error}"));
            let group = i32::try_from(sid).map(Pid::from_raw);
            match group.map(|group| nix::sys::signal::killpg(group, signal)) {
                Ok(Ok(()) | Err(nix::errno::Errno::ESRCH)) => {}
                Ok(Err(e)) => errors.push(format!("signal {signal:?} to group {sid}: {e}")),
                Err(e) => errors.push(format!("session {sid}: {e}")),
            }
            return;
        }
    };
    for pid in members {
        let signalled = if signal == Signal::SIGKILL && signal_one_fails() {
            Err(nix::errno::Errno::EPERM)
        } else {
            kill(Pid::from_raw(pid), signal)
        };
        match signalled {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(e) => errors.push(format!(
                "signal {signal:?} to session {sid} member {pid}: {e}"
            )),
        }
    }
}

/// `session-signal-one`: the stop's first KILL to a member fails, once.
fn signal_one_fails() -> bool {
    static FIRED: AtomicBool = AtomicBool::new(false);
    fail_at("session-signal-one") && !FIRED.swap(true, Ordering::SeqCst)
}
pub fn attempted(root: &Path, request: &Request) -> bool {
    root.join("runs")
        .join(&request.id)
        .join("result.attempted")
        .exists()
}

/// The verdict, decided once by the first terminal fact (SPEC §5):
/// - a recipe that reached its result, or timed out, keeps that verdict
///   whatever happened later; a later signal is only noted;
/// - a signal (a `stop`, or one sent to the supervisor) before the result
///   is `stopped`, 128 plus the signal;
/// - any other failure after spawn (`attempted`) is lost 255; before it,
///   refused 125.
pub fn verdict(
    ended: &std::result::Result<Ended, String>,
    signal: Option<i32>,
    attempted: bool,
) -> (receipt::Outcome, i32, Option<String>) {
    use receipt::Outcome;
    let named = |signal: i32| format!("supervisor received {}", signal_name(signal));
    let after = || signal.map(|signal| format!("after the verdict: {}", named(signal)));
    match (ended, signal) {
        (Ok(Ended::Result(0)), _) => (Outcome::RanPass, 0, after()),
        (Ok(Ended::Result(exit)), _) => (Outcome::RanFail, *exit, after()),
        (Ok(Ended::Timeout), _) => (Outcome::Timeout, 124, after()),
        (Ok(Ended::Stopped), Some(signal)) => (Outcome::Stopped, 128 + signal, Some(named(signal))),
        (Ok(Ended::Stopped), None) => (Outcome::Lost, 255, Some("stopped".into())),
        (Err(error), Some(signal)) => (
            Outcome::Stopped,
            128 + signal,
            Some(format!("{}; {error}", named(signal))),
        ),
        (Err(error), None) if attempted => (Outcome::Lost, 255, Some(error.clone())),
        (Err(error), None) => (Outcome::Refused, 125, Some(error.clone())),
    }
}

/// Notes on what failed after a run reached its verdict (a session
/// teardown that could not be confirmed, output still open). One supervisor
/// runs one request, so they are kept here and taken once.
static NOTES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn note_after(note: String) {
    if let Ok(mut notes) = NOTES.lock() {
        notes.push(note);
    }
}

/// The notes so far, taken once.
pub(crate) fn take_notes() -> Vec<String> {
    NOTES
        .lock()
        .map(|mut notes| std::mem::take(&mut *notes))
        .unwrap_or_default()
}

/// What a supervised run came to. Custody is always present once a session
/// may have been spawned: an unrecorded stop could hide survivors.
pub struct Finished {
    pub executed: Result<Ended>,
    pub collection: Option<Collection>,
    pub custody: crate::custody::Release,
    /// Failures after the verdict: recorded, never changing it.
    pub notes: Vec<String>,
    pub output: Output,
}

/// Runs the recipe and collect sessions, then custody release, while the
/// locks are still held. No failure in between skips the release: every
/// path collects, stops the session, then reports.
pub fn run_released(
    root: &Path,
    workdir: &Path,
    request: &Request,
    stop: &AtomicBool,
    locks: &[&fs::File],
    deadline_ms: u128,
) -> Finished {
    // A backstop: nothing in here should panic, but an unwind must still
    // reach the release below.
    let (executed, collection, output) =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(!fail_at("run-panic"), "injected by FERRY_FAIL_AT");
            run(root, workdir, request, stop, locks, deadline_ms)
        }))
        .unwrap_or_else(|panic| {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            (
                Err(format!("supervisor panicked: {what}").into()),
                None,
                Output::default(),
            )
        });
    let custody = crate::custody::take();
    Finished {
        executed,
        collection,
        custody,
        output,
        notes: take_notes(),
    }
}

/// Runs the recipe session, then the collect hook's. `locks` are the seat
/// lock and host lease descriptors: each session inherits them, so they stay
/// held while the session lives even if this supervisor is killed.
fn run(
    root: &Path,
    workdir: &Path,
    request: &Request,
    stop: &AtomicBool,
    locks: &[&fs::File],
    deadline_ms: u128,
) -> (Result<Ended>, Option<Collection>, Output) {
    // A stop that came while the run prepared: nothing is spawned, neither
    // the recipe nor the collect hook.
    pause_at(
        "prepare-pause",
        &crate::record::run_dir(root, &request.id).join("spawn"),
    );
    if stop.load(Ordering::SeqCst) {
        return (Ok(Ended::Stopped), None, Output::default());
    }
    // One log for the run, shared by every session's writer.
    let log = match open_log(root, request) {
        Ok(log) => log,
        Err(error) => return (Err(error), None, Output::default()),
    };
    let outcome = run_session(root, workdir, request, stop, locks, &log, deadline_ms);
    let collection = request
        .hooks
        .collect
        .as_ref()
        .map(|hook| collect(root, workdir, request, hook, &outcome, locks, &log));
    // Closed here, before the record: nothing writes the log after this.
    let output = log
        .lock()
        .ok()
        .and_then(|mut log| log.take())
        .map(|(_, output)| output)
        .unwrap_or_default();
    (outcome, collection, output)
}

/// Runs the collect hook in its own bounded session after recipe teardown,
/// and records it. Its failures are its own: they never touch the verdict.
fn collect(
    root: &Path,
    workdir: &Path,
    request: &Request,
    hook: &str,
    outcome: &Result<Ended>,
    locks: &[&fs::File],
    log: &RunLog,
) -> Collection {
    let mut collection = match run_collect(root, workdir, request, hook, outcome, locks, log) {
        Ok((code, timed_out)) => Collection {
            exit_code: Some(code),
            error: timed_out.then(|| "collection timed out".into()),
        },
        Err(error) => Collection {
            exit_code: None,
            error: Some(error.to_string()),
        },
    };
    let path = crate::record::run_dir(root, &request.id).join("collection.json");
    let written = if fail_at("collection-write") {
        Err(std::io::Error::other("injected by FERRY_FAIL_AT"))
    } else {
        serde_json::to_vec(&collection)
            .map_err(std::io::Error::from)
            .and_then(|bytes| fs::write(&path, bytes))
    };
    if let Err(error) = written {
        crate::receipt::note(
            &mut collection.error,
            format!("writing {}: {error}", path.display()),
        );
    }
    collection
}

fn run_collect(
    root: &Path,
    workdir: &Path,
    request: &Request,
    hook: &str,
    outcome: &Result<Ended>,
    locks: &[&fs::File],
    log: &RunLog,
) -> Result<(i32, bool)> {
    let mut collect = request.clone();
    collect.id = receipt::run_id()?;
    collect.hooks.prepare = None;
    collect.hooks.collect = None;
    collect.args.clear();
    collect.recipe.command = vec![
        "env".into(),
        format!(
            "FERRY_EXIT_CODE={}",
            match outcome {
                Ok(Ended::Result(code)) => *code,
                Ok(Ended::Timeout) => 124,
                _ => 125,
            }
        ),
        workdir
            .join(hook)
            .to_str()
            .ok_or("non-UTF-8 collect path")?
            .to_owned(),
    ];
    collect.timeout_seconds = Some(COLLECT_TIMEOUT.as_secs());
    // Collection has its own bounded session after recipe teardown, including
    // timeout or preparation failure. Never reinterpret the recipe verdict.
    match run_session(
        root,
        workdir,
        &collect,
        &AtomicBool::new(false),
        locks,
        log,
        u128::MAX,
    )? {
        Ended::Result(code) => Ok((code, false)),
        Ended::Timeout => Ok((124, true)),
        Ended::Stopped => Err("collection stopped".into()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

/// Spawns the session owner with the locks inherited. Only the session
/// inherits them: they close on exec again at once, so no other child of
/// this supervisor (git, for one) can keep them. Returns the spawn, and
/// whether close-on-exec was restored.
fn spawn_session(
    workdir: &Path,
    spec: &Path,
    result_path: &Path,
    locks: &[&fs::File],
    output: std::io::PipeWriter,
) -> Result<(std::io::Result<std::process::Child>, Result<()>)> {
    for lock in locks {
        crate::lock::close_on_exec(lock, false)?;
    }
    // One pipe for both streams keeps the order they were written in.
    let spawned = Command::new(std::env::current_exe()?)
        .arg("execute-session")
        .arg(spec)
        .arg(result_path)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output)
        .spawn();
    let restored = locks
        .iter()
        .try_for_each(|lock| crate::lock::close_on_exec(lock, true));
    Ok((spawned, restored))
}

/// How a session ended: the recipe's own result, the timeout, or a stop
/// (a signal or `ferry stop`) before either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Result(i32),
    Timeout,
    Stopped,
}

/// A run's output log and what reached it: one for every session, taken
/// (closed) once the run's output ends.
type RunLog = Arc<std::sync::Mutex<Option<(fs::File, Output)>>>;

fn open_log(root: &Path, request: &Request) -> Result<RunLog> {
    let run_dir = crate::record::run_dir(root, &request.id);
    fs::create_dir_all(&run_dir)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("output.log"))?;
    Ok(Arc::new(std::sync::Mutex::new(Some((
        file,
        Output::default(),
    )))))
}

fn run_session(
    root: &Path,
    workdir: &Path,
    request: &Request,
    stop: &AtomicBool,
    locks: &[&fs::File],
    log: &RunLog,
    deadline_ms: u128,
) -> Result<Ended> {
    let run_dir = crate::record::run_dir(root, &request.id);
    fs::create_dir_all(&run_dir)?;
    let spec = run_dir.join("request.json");
    fs::write(&spec, serde_json::to_vec(request)?)?;
    let result_path = run_dir.join("result.json");
    let (reader, writer) = std::io::pipe()?;
    let (spawned, restored) = spawn_session(workdir, &spec, &result_path, locks, writer)?;
    let child = spawned?;
    let mut owner = SessionOwner {
        child,
        stopped: false,
        reaped: false,
    };
    // After the owner exists, so a failure here still tears the session down.
    restored?;
    let (copied, done) = std::sync::mpsc::channel();
    let log = log.clone();
    thread::spawn(move || {
        copy_output(reader, &log);
        let _ = copied.send(());
    });
    let start = Instant::now();
    let ended = loop {
        // The first terminal fact wins: a result before a stop or timeout.
        if let Ok(bytes) = fs::read(&result_path) {
            break Ended::Result(serde_json::from_slice::<i32>(&bytes)?);
        }
        if stop.load(Ordering::SeqCst) {
            break Ended::Stopped;
        }
        if request
            .timeout_seconds
            .is_some_and(|t| start.elapsed() >= Duration::from_secs(t))
            || receipt::now_ms() >= deadline_ms
        {
            break Ended::Timeout;
        }
        // Not reaped here: the stop still signals the session by its id.
        if owner.leader_exited() {
            // Its own error when it left one (SPEC §5), else only the fact.
            return Err(owner_error(&result_path)
                .unwrap_or_else(|| "session owner exited without result".into())
                .into());
        }
        thread::sleep(Duration::from_millis(20));
    };
    // What the stop leaves is recorded, never chased (SPEC §7).
    crate::custody::note(owner.stop());
    // A process outside the session may still hold the pipe: its output is
    // not waited for past the drain, and that is reported.
    if done.recv_timeout(crate::limits::DRAIN).is_err() {
        note_after(format!(
            "output still open {:?} after the session stop; later output is not logged",
            crate::limits::DRAIN
        ));
    }
    Ok(ended)
}

/// What reached a run's output log.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Output {
    /// Bytes written to `output.log`.
    pub bytes: u64,
    /// Output past the cap was read and discarded.
    pub capped: bool,
    /// Writes that failed; their bytes are lost, the run went on.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub write_errors: u64,
}
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// The output cap; debug builds read `FERRY_OUTPUT_CAP` so a control can
/// reach it quickly.
fn output_cap() -> u64 {
    crate::execution::debug_bound("FERRY_OUTPUT_CAP").unwrap_or(crate::limits::OUTPUT_CAP)
}

/// The one writer of a run's output: copies a session's pipe into the log
/// until EOF, never blocking the recipe (past the cap it reads and drops).
/// Once `log` is taken (`None`), it stops writing at once.
fn copy_output(mut pipe: std::io::PipeReader, log: &RunLog) {
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        let read = match pipe.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let Ok(mut guard) = log.lock() else {
            return;
        };
        let Some((file, output)) = guard.as_mut() else {
            return;
        };
        let room = output_cap().saturating_sub(output.bytes);
        let take = read.min(usize::try_from(room).unwrap_or(usize::MAX));
        output.capped |= take < read;
        if take > 0 {
            if fail_at("output-write") || file.write_all(&buffer[..take]).is_err() {
                output.write_errors += 1;
            } else {
                output.bytes += take as u64;
            }
        }
    }
}

/// The most of an owner's error a record carries: a failed hook's error
/// includes its stderr, which can be long or hold what it was given.
const OWNER_ERROR_CAP: usize = 4096;

/// The session owner's error, written beside its result (`result.error`,
/// SPEC §9) before it exits, so the record gives the cause, not only that
/// it exited: capped, and renamed into place whole. Best effort: without it
/// the record says the owner exited.
pub fn left_error(result: &Path, error: &dyn std::error::Error) {
    if fail_at("owner-error") {
        return;
    }
    let mut text = error.to_string();
    if text.len() > OWNER_ERROR_CAP {
        text.truncate(text.floor_char_boundary(OWNER_ERROR_CAP));
        text.push_str(" … (truncated)");
    }
    let tmp = result.with_extension("error.tmp");
    let _ = fs::write(&tmp, text).and_then(|()| fs::rename(&tmp, result.with_extension("error")));
}

/// The error the owner left, if it left any text.
fn owner_error(result: &Path) -> Option<String> {
    let text = fs::read_to_string(result.with_extension("error")).ok()?;
    (!text.trim().is_empty()).then_some(text)
}

pub fn execute_session(spec: &Path, result: &Path) -> Result<()> {
    let supervisor = nix::unistd::getppid();
    setsid()?;
    let request: Request = serde_json::from_slice(&fs::read(spec)?)?;
    validate(&request)?;
    let root = spec
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("invalid request path")?;
    let workdir = std::env::current_dir()?;
    // A Ferry-owned, per-seat cache directory for regenerable build output.
    let cache = root.join("seats").join(&request.seat).join("cache");
    fs::create_dir_all(&cache)?;
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700))?;
    let mut env = BTreeMap::from([
        ("FERRY_SEAT".to_owned(), request.seat.clone()),
        ("FERRY_COMMIT".to_owned(), request.commit.clone()),
        ("FERRY_WORKDIR".to_owned(), workdir.display().to_string()),
        ("FERRY_CACHE_DIR".to_owned(), cache.display().to_string()),
        ("FERRY_ID".to_owned(), request.id.clone()),
        ("FERRY_REQUEST_FILE".to_owned(), spec.display().to_string()),
    ]);
    env.extend(request.forwarded_env.clone());
    if request.ship.iter().any(|shipped| shipped.url.is_some()) {
        env.insert(
            "GIT_CONFIG_GLOBAL".into(),
            workdir.join(".ferry/gitconfig").display().to_string(),
        );
    }
    if let Some(prepare) = &request.hooks.prepare {
        let status = Command::new(workdir.join(prepare)).envs(&env).status()?;
        if !status.success() {
            return Err(format!("prepare hook failed: {status}").into());
        }
    }
    if let Some(hook) = &request.hooks.env {
        let bytes = repository::output(Command::new(workdir.join(hook)).envs(&env))?;
        let additions: BTreeMap<String, String> = serde_json::from_slice(&bytes)?;
        if additions
            .iter()
            .any(|(k, v)| crate::config::invalid_env(k, v) || request.forwarded_env.contains_key(k))
        {
            return Err("invalid hook environment".into());
        }
        env.extend(additions);
    }
    fs::write(
        result.with_extension("attempted"),
        b"recipe dispatch begins",
    )?;
    let status = Command::new(&request.recipe.command[0])
        .args(&request.recipe.command[1..])
        .args(&request.args)
        .envs(&env)
        .status()?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
    let tmp = result.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(&code)?)?;
    fs::rename(tmp, result)?;
    // Retain the leader for supervised cleanup, but not forever after supervisor death.
    // Direct parent identity cannot recycle without the kernel reparenting this process.
    while nix::unistd::getppid() == supervisor {
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Ended, owner_error, verdict};

    /// An empty `result.error` (a full disk, a kill mid-write) is no error:
    /// the record then says the owner exited, never an empty reason.
    #[test]
    fn an_empty_owner_error_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let result = dir.path().join("result.json");
        assert_eq!(owner_error(&result), None);
        std::fs::write(result.with_extension("error"), " \n").unwrap();
        assert_eq!(owner_error(&result), None);
        std::fs::write(result.with_extension("error"), "boom").unwrap();
        assert_eq!(owner_error(&result).as_deref(), Some("boom"));
    }
    use crate::receipt::Outcome;

    #[test]
    fn the_verdict_is_decided_once_by_the_first_terminal_fact() {
        type Row = (
            Result<Ended, String>,
            Option<i32>,
            bool,
            Outcome,
            i32,
            Option<&'static str>,
        );
        let failed = || Err::<Ended, String>("boom".into());
        let table: [Row; 8] = [
            (Ok(Ended::Result(0)), None, true, Outcome::RanPass, 0, None),
            (Ok(Ended::Result(3)), None, true, Outcome::RanFail, 3, None),
            (Ok(Ended::Timeout), None, true, Outcome::Timeout, 124, None),
            // A stop after the result is only noted.
            (
                Ok(Ended::Timeout),
                Some(15),
                true,
                Outcome::Timeout,
                124,
                Some("after the verdict: supervisor received SIGTERM"),
            ),
            // A stop before it is stopped, with its signal's code.
            (
                Ok(Ended::Stopped),
                Some(15),
                true,
                Outcome::Stopped,
                143,
                Some("supervisor received SIGTERM"),
            ),
            (
                failed(),
                Some(2),
                false,
                Outcome::Stopped,
                130,
                Some("supervisor received SIGINT; boom"),
            ),
            // Any other failure: lost after spawn, refused before.
            (failed(), None, true, Outcome::Lost, 255, Some("boom")),
            (failed(), None, false, Outcome::Refused, 125, Some("boom")),
        ];
        for (ended, signal, attempted, outcome, code, reason) in table {
            let got = verdict(&ended, signal, attempted);
            assert_eq!(
                got,
                (outcome, code, reason.map(str::to_owned)),
                "{ended:?} {signal:?} {attempted}"
            );
        }
    }
}
