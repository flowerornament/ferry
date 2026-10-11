//! The place side of a run: short verbs, run over ssh or as a local exec.
//!
//! - `submit` reads a request, starts its supervisor paused, reserves the id
//!   with the supervisor's own pid, then releases it and replies at once.
//! - `supervise` runs the request to its record, detached: no client, no
//!   terminal, one absolute deadline.
//! - `follow` writes `runs/<id>/output.log` from an offset until the record
//!   is final and every byte before it is sent, or its reader is gone.
//! - `stop` stops a live run's supervisor; `record` prints a run's record.
use crate::{
    execution::{self, Request},
    receipt::{self, Outcome, note},
    record,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufRead, Read, Seek, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// What the place reports at the end of a run, and keeps as its record.
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
    /// What reached `output.log`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<execution::Output>,
    /// When the run had to end by, whatever happened (ms since the epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u128>,
    /// Who stopped the run and why, as `stop` recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<serde_json::Value>,
    /// `final` in a host record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
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
            output: None,
            deadline_ms: None,
            stopped_by: None,
            state: None,
        })
    }
}

/// The state root: `~/.local/state/ferry` unless named.
pub fn root(state: Option<PathBuf>) -> Result<PathBuf> {
    match state {
        Some(state) => Ok(state),
        None => Ok(
            PathBuf::from(std::env::var_os("HOME").ok_or("HOME missing")?)
                .join(".local/state/ferry"),
        ),
    }
}

fn run_dir(root: &Path, id: &str) -> PathBuf {
    root.join("runs").join(id)
}

/// Prints one JSON reply line.
fn reply(value: &serde_json::Value) {
    execution::emit(value.to_string().as_bytes());
}

fn refuse(why: &str) -> i32 {
    reply(&serde_json::json!({"refused": why}));
    125
}

/// Reads, validates and starts a request. Refusals exit 125 and nothing ran.
pub fn submit(root: &Path) -> Result<i32> {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let mut request: Request = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(e) => return Ok(refuse(&format!("parsing the request: {e}"))),
    };
    let limit = crate::limits::MAX_TIMEOUT.as_secs();
    if request.timeout_seconds.is_some_and(|t| t > limit) {
        return Ok(refuse(&format!(
            "timeout above this place's maximum ({limit} s)"
        )));
    }
    request.timeout_seconds.get_or_insert(limit);
    if let Err(e) = execution::validate(&request) {
        return Ok(refuse(&format!("validating the request: {e}")));
    }
    // A retried submit (same id and token) follows the run it started.
    if let Some(pid) = existing(root, &request) {
        reply(&serde_json::json!({"id": request.id, "pid": pid}));
        return Ok(0);
    }
    if execution::fail_at("submit-spawn") {
        return Ok(refuse("submit-spawn: injected by FERRY_FAIL_AT"));
    }
    // Paused on its stdin: nothing runs until the reservation names it.
    let mut supervisor = Command::new(std::env::current_exe()?)
        .arg("place")
        .arg("--state")
        .arg(root)
        .arg("supervise")
        .arg(&request.id)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let pid = supervisor.id();
    let submitted_ms = receipt::now_ms();
    let reserved = HostResult::refused(&request).and_then(|result| {
        record::reserve(
            root,
            &record::Reservation {
                state: "reserved".into(),
                id: request.id.clone(),
                token: request.token.clone(),
                commit: request.commit.clone(),
                seat: request.seat.clone(),
                binary_sha256: result.binary_sha256,
                supervisor: record::Supervisor {
                    pid,
                    start: i32::try_from(pid)
                        .ok()
                        .and_then(|pid| crate::seat::start_time(pid).ok()),
                },
                boot_id: record::boot_id(),
                submitted_ms,
            },
        )
    });
    if let Err(error) = reserved {
        // Its own child, by its handle: it never ran anything.
        let _ = supervisor.kill();
        let _ = supervisor.wait();
        return Ok(refuse(&error.to_string()));
    }
    if execution::fail_at("submit-release") {
        // As if submit died here: no reply, and its supervisor reads EOF
        // and leaves without effect.
        std::process::exit(70);
    }
    let released = (|| -> Result<()> {
        let dir = run_dir(root, &request.id);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("request.json"), serde_json::to_vec(&request)?)?;
        let mut go = supervisor.stdin.take().ok_or("no supervisor stdin")?;
        go.write_all(b"go")?;
        Ok(())
    })();
    if let Err(error) = released {
        // Never released, so nothing ran: the record says so at once.
        let _ = supervisor.kill();
        let _ = supervisor.wait();
        let mut result = HostResult::refused(&request)?;
        result.reason = Some(format!("starting the supervisor: {error}"));
        result.state = Some("final".into());
        let _ = record::finalize(root, &request.id, &serde_json::to_value(&result)?);
        return Ok(refuse(&format!("starting the supervisor: {error}")));
    }
    reply(
        &serde_json::json!({"id": request.id, "pid": pid, "deadline_ms": deadline_ms(submitted_ms, &request)}),
    );
    Ok(0)
}

/// One absolute deadline (SPEC §5): the lock waits, the run, and its
/// teardown, from the moment the run was submitted.
fn deadline_ms(submitted_ms: u128, request: &Request) -> u128 {
    submitted_ms
        + request.wait().as_millis()
        + u128::from(request.timeout_seconds.unwrap_or_default()) * 1000
        + crate::limits::TEARDOWN.as_millis()
}

/// The supervisor's pid of a run this request already started (0 once it
/// is final): its own caller retrying gets that run, never a second one.
fn existing(root: &Path, request: &Request) -> Option<u32> {
    let records = root.join("records");
    if let Ok(bytes) = fs::read(records.join(format!("{}.reserved.json", request.id))) {
        let reservation: record::Reservation = serde_json::from_slice(&bytes).ok()?;
        return (reservation.token == request.token).then_some(reservation.supervisor.pid);
    }
    let bytes = fs::read(records.join(format!("{}.json", request.id))).ok()?;
    let done: HostResult = serde_json::from_slice(&bytes).ok()?;
    (done.token == request.token).then_some(0)
}

/// Runs a submitted request to its final record, detached from any client.
pub fn supervise(root: &Path, id: &str) -> Result<i32> {
    // Its own session, no terminal: nothing the client's session does
    // reaches it.
    nix::unistd::setsid()?;
    let mut go = [0u8; 2];
    if std::io::stdin().read_exact(&mut go).is_err() {
        // Submit never released it: leave without effect.
        return Ok(0);
    }
    let dir = run_dir(root, id);
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("supervisor.log"))?;
    nix::unistd::dup2_stderr(&log)?;
    let request: Request = serde_json::from_slice(&fs::read(dir.join("request.json"))?)?;
    let stop = execution::stopped()?;
    let mut result = HostResult::refused(&request)?;
    let reservation: record::Reservation = serde_json::from_slice(&fs::read(
        root.join("records").join(format!("{id}.reserved.json")),
    )?)?;
    let deadline = deadline_ms(reservation.submitted_ms, &request);
    result.deadline_ms = Some(deadline);
    record::reconcile_seat(root, &request.seat);
    let executed: Result<execution::Ended> = (|| {
        crate::custody::supervise()?;
        let (seat, lease, host_lease) =
            execution::prepare(root, &request, &stop, &mut result.admission)?;
        let locks = execution::inherited(&lease, &host_lease);
        let finished = execution::run_released(root, &seat, &request, &stop, &locks, deadline);
        if let Some(error) = &finished.custody.error {
            note(
                &mut result.reason,
                format!("custody release failed: {error}"),
            );
        }
        result.custody = Some(finished.custody);
        for after in finished.notes {
            note(&mut result.reason, after);
        }
        result.output = Some(finished.output);
        result.collection = finished.collection;
        if let Some(error) = result.collection.as_ref().and_then(|c| c.error.as_ref()) {
            note(&mut result.reason, format!("collection: {error}"));
        }
        let ended = finished.executed?;
        if matches!(
            ended,
            execution::Ended::Result(_) | execution::Ended::Timeout
        ) {
            collect_artifacts(root, &seat, &request, &mut result);
        }
        // Stamped while held; the locks drop as this scope ends.
        result.lease = Some(host_lease.stamp());
        Ok(ended)
    })();
    // One verdict, from the first terminal fact (Rule B).
    let (outcome, exit_code, reason) = execution::verdict(
        &executed.map_err(|error| error.to_string()),
        execution::caught_signal(),
        execution::attempted(root, &request),
    );
    result.outcome = outcome;
    result.exit_code = exit_code;
    if outcome == Outcome::Stopped {
        result.stopped_by = fs::read(dir.join("stop.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    }
    if let Some(reason) = reason {
        result.reason = Some(match result.reason.take() {
            Some(notes) => format!("{reason}; {notes}"),
            None => reason,
        });
    }
    // The record is the truth; a failure to write it leaves the reservation
    // for a reconcile once this supervisor is gone.
    result.state = Some("final".into());
    let stored = serde_json::to_value(&result)
        .map_err(Into::into)
        .and_then(|value| record::finalize(root, id, &value));
    if let Err(error) = stored {
        execution::warn(&format!("ferry: host record: {error}"));
    }
    Ok(result.exit_code)
}

/// Archives the recipe's declared artifacts. A failure, the digest's
/// included, is recorded in the result and never changes its verdict.
fn collect_artifacts(root: &Path, seat: &Path, request: &Request, result: &mut HostResult) {
    if request.recipe.artifacts.is_empty() {
        return;
    }
    let output = run_dir(root, &request.id).join("artifacts.tar");
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

/// Writes the run's output from `offset` until its record is final and
/// every byte written before that is sent, or until its reader is gone.
/// Exit 0 at the end, 4 for a run this place never had.
pub fn follow(root: &Path, id: &str, offset: u64) -> Result<i32> {
    let reserved = root.join("records").join(format!("{id}.reserved.json"));
    if !reserved.exists() && record::reconcile(root, id)?.is_none() {
        return Ok(4);
    }
    let path = run_dir(root, id).join("output.log");
    let mut sent = offset;
    let mut out = std::io::stdout().lock();
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        // Final first, then everything up to the end: the log is complete
        // before the record is final.
        let done = record::reconcile(root, id)?.is_some();
        if let Ok(mut log) = fs::File::open(&path) {
            log.seek(std::io::SeekFrom::Start(sent))?;
            loop {
                let read = log.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                out.write_all(&buffer[..read])?;
                sent += read as u64;
                if execution::fail_at("follow-drop") {
                    out.flush()?;
                    return Err("follow-drop: injected by FERRY_FAIL_AT".into());
                }
            }
            out.flush()?;
        }
        if done {
            return Ok(0);
        }
        // A reader that is gone (a killed caller, a dropped ssh) ends this
        // follow now, not at the run's deadline; the run goes on.
        if reader_gone(&out) {
            return Err("follow: its reader is gone".into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Whether the other end of `out` has closed: a pipe or socket whose
/// reader is gone reports POLLHUP or POLLERR. A file never does. macOS
/// reports it only when POLLOUT is asked for.
fn reader_gone(out: &impl std::os::fd::AsFd) -> bool {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let mut fds = [PollFd::new(out.as_fd(), PollFlags::POLLOUT)];
    poll(&mut fds, PollTimeout::ZERO).is_ok()
        && fds[0]
            .revents()
            .is_some_and(|r| r.intersects(PollFlags::POLLHUP | PollFlags::POLLERR))
}

/// Stops a live run with `signal` (its verdict is then 128 plus it). Only
/// its own caller (the same seat key) may, unless it is `other` with a
/// reason. The first stop is recorded; any later one is accepted and
/// changes nothing.
pub fn stop(
    root: &Path,
    id: &str,
    seat: &str,
    who: &str,
    why: &str,
    other: bool,
    signal: i32,
) -> Result<i32> {
    let signal = nix::sys::signal::Signal::try_from(signal)?;
    if ![
        nix::sys::signal::Signal::SIGINT,
        nix::sys::signal::Signal::SIGTERM,
        nix::sys::signal::Signal::SIGHUP,
    ]
    .contains(&signal)
    {
        return Ok(refuse("a stop sends INT, TERM or HUP"));
    }
    let path = root.join("records").join(format!("{id}.reserved.json"));
    let Ok(bytes) = fs::read(&path) else {
        return Ok(refuse(&format!("run {id} is not running here")));
    };
    let reservation: record::Reservation = serde_json::from_slice(&bytes)?;
    if reservation.seat != seat && !other {
        return Ok(refuse(&format!(
            "run {id} belongs to another caller; stop it with --other --reason"
        )));
    }
    let pid = nix::unistd::Pid::from_raw(i32::try_from(reservation.supervisor.pid)?);
    let live = match crate::seat::start_time(pid.as_raw()) {
        Ok(start) => reservation
            .supervisor
            .start
            .is_none_or(|then| then == start),
        Err(_) => nix::sys::signal::kill(pid, None).is_ok(),
    };
    if !live {
        return Ok(refuse(&format!("run {id}'s supervisor is gone")));
    }
    let stopped = serde_json::json!({
        "who": who, "why": why, "other": other, "at_ms": receipt::now_ms(),
    });
    let written = if execution::fail_at("stop-write") {
        Err("stop-write: injected by FERRY_FAIL_AT".into())
    } else {
        record::write_new(
            &run_dir(root, id).join("stop.json"),
            stopped.to_string().as_bytes(),
            None,
        )
    };
    if let Err(error) = written
        && !run_dir(root, id).join("stop.json").exists()
    {
        return Ok(refuse(&format!("recording the stop: {error}")));
    }
    nix::sys::signal::kill(pid, signal)?;
    reply(&serde_json::json!({"accepted": id}));
    Ok(0)
}

/// Prints the run's final record (exit 0), or its reservation while it is
/// not final (exit 3), or nothing for a run this place never had (exit 4).
pub fn print_record(root: &Path, id: &str) -> Result<i32> {
    if let Some(bytes) = record::reconcile(root, id)? {
        execution::emit(&bytes);
        return Ok(0);
    }
    match fs::read(root.join("records").join(format!("{id}.reserved.json"))) {
        Ok(bytes) => {
            execution::emit(&bytes);
            Ok(3)
        }
        Err(_) => Ok(4),
    }
}

/// An old caller speaks the host-run stream protocol, which this place no
/// longer serves: it refuses, so the caller records a refusal.
pub fn host_run() -> i32 {
    reply(&serde_json::json!({
        "protocol": 1,
        "event": "refused",
        "reason": "this place runs detached ferry: install the same ferry on the caller",
    }));
    125
}
