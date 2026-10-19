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
/// Like a receipt, every construction names `outcome`, `exit_code`, `state`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HostResult {
    pub protocol: u32,
    pub token: String,
    pub commit: String,
    pub outcome: Outcome,
    pub exit_code: i32,
    pub reason: Option<String>,
    pub binary_sha256: String,
    /// The whole archive's digest, and a failure as text: what callers
    /// before per-file artifacts read, so they still fetch (SPEC §6).
    pub artifacts_sha256: Option<String>,
    pub artifacts_error: Option<String>,
    /// Each collected file and its digest (SPEC §8), and `none` or why
    /// collection failed (absent once collected): kept as the place wrote
    /// them, like `custody`, and read where they are used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts_status: Option<serde_json::Value>,
    #[serde(default)]
    pub collection: Option<crate::run::Collection>,
    #[serde(default)]
    pub source_rev: Option<String>,
    #[serde(default)]
    pub exe: Option<String>,
    /// Kept as the place wrote it: a record of an older shape must always
    /// parse, or `ferry record` could never complete its run.
    #[serde(default)]
    pub custody: Option<serde_json::Value>,
    #[serde(default)]
    pub lease: Option<crate::lock::Lease>,
    /// As the place wrote it, for the same reason as `custody`.
    #[serde(default)]
    pub admission: Option<serde_json::Value>,
    /// What the per-seat cap wiped at this run's start (SPEC §9), as the
    /// place wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pruned: Option<serde_json::Value>,
    /// What reached `output.log`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<crate::run::Output>,
    /// When the run had to end by, whatever happened (ms since the epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u128>,
    /// Who stopped the run and why, as `stop` recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<serde_json::Value>,
    /// `final` in a host record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The seat was declared for this run only; the record never says
    /// whether its retire happened (SPEC §9).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub once: bool,
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
            state: None,
            binary_sha256: receipt::digest(&fs::read(std::env::current_exe()?)?),
            source_rev: Some(receipt::source_rev()),
            exe: receipt::exe_path(),
            once: request.once,
            ..Default::default()
        })
    }
}

/// The place's state root under the home directory (SPEC §9). It is `v1`
/// inside the root binaries before M1 used, which never look inside it, so
/// old and new never share a root (SPEC §6); the local route's is
/// `<repo>/.ferry/local/v1`, the same rule.
pub const REMOTE_ROOT: &str = ".local/state/ferry/v1";

/// The state root: `~/` + `REMOTE_ROOT` unless named.
pub fn root(state: Option<PathBuf>) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    state
        .or_else(|| home.map(|home| home.join(REMOTE_ROOT)))
        .ok_or_else(|| "HOME missing".into())
}

/// Prints one JSON reply line.
fn reply(value: &serde_json::Value) {
    execution::emit(value.to_string().as_bytes());
}

/// Names this place's revision, so an older caller knows what to install.
fn same_ferry() -> String {
    let rev = receipt::source_rev();
    format!("install the same ferry on both ends (this place runs {rev})")
}
const FIX: &str = "fix what it names, then run again";

/// A refusal (SPEC §6): `{refused, why, next}`.
fn refuse(why: &str, next: &str) -> i32 {
    reply(&serde_json::json!({"refused": why, "why": why, "next": next}));
    125
}

/// Reads, validates and starts a request. Refusals exit 125 and nothing ran.
pub fn submit(root: &Path) -> Result<i32> {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let mut request: Request = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(e) => {
            return Ok(refuse(&format!("parsing the request: {e}"), &same_ferry()));
        }
    };
    let limit = crate::limits::MAX_TIMEOUT.as_secs();
    if request.timeout_seconds.is_some_and(|t| t > limit) {
        return Ok(refuse(
            &format!("timeout above this place's maximum ({limit} s)"),
            &format!("ask for at most {limit} s with --timeout-seconds"),
        ));
    }
    request.timeout_seconds.get_or_insert(limit);
    if let Err(e) = execution::validate(&request) {
        return Ok(refuse(&format!("validating the request: {e}"), FIX));
    }
    // A retried submit (same id and token) follows the run it started.
    if let Some(pid) = existing(root, &request) {
        reply(&serde_json::json!({"id": request.id, "pid": pid}));
        return Ok(0);
    }
    if execution::fail_at("submit-spawn") {
        return Ok(refuse(
            "submit-spawn: injected by FERRY_FAIL_AT",
            "run again",
        ));
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
                        .and_then(|pid| crate::record::start_time(pid).ok()),
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
        return Ok(refuse(
            &error.to_string(),
            &format!("ferry record {}", request.id),
        ));
    }
    if execution::fail_at("submit-release") {
        // As if submit died here: no reply, and its supervisor reads EOF
        // and leaves without effect.
        std::process::exit(70);
    }
    let released = (|| -> Result<()> {
        let dir = record::run_dir(root, &request.id);
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
        return Ok(refuse(
            &format!("starting the supervisor: {error}"),
            "run again",
        ));
    }
    reply(
        &serde_json::json!({"id": request.id, "pid": pid, "deadline_ms": deadline_ms(submitted_ms, &request)}),
    );
    Ok(0)
}

/// One absolute deadline (SPEC §5): the lock waits, the run, and its
/// teardown, from the moment the run was submitted. Debug builds read
/// `FERRY_DEADLINE_MS` (from submit), so a control can make the deadline,
/// not the timeout, end a run.
pub(crate) fn deadline_ms(submitted_ms: u128, request: &Request) -> u128 {
    if let Some(ms) = execution::debug_bound::<u128>("FERRY_DEADLINE_MS") {
        return submitted_ms + ms;
    }
    submitted_ms
        + request.wait().as_millis()
        + u128::from(request.timeout_seconds.unwrap_or_default()) * 1000
        + crate::limits::TEARDOWN.as_millis()
}

/// The supervisor's pid of a run this request already started (0 once it
/// is final): its own caller retrying gets that run, never a second one.
fn existing(root: &Path, request: &Request) -> Option<u32> {
    if let Ok(bytes) = fs::read(record::reserved_path(root, &request.id)) {
        let reservation: record::Reservation = serde_json::from_slice(&bytes).ok()?;
        return (reservation.token == request.token).then_some(reservation.supervisor.pid);
    }
    let bytes = fs::read(record::final_path(root, &request.id)).ok()?;
    let done: HostResult = serde_json::from_slice(&bytes).ok()?;
    (done.token == request.token).then_some(0)
}

/// Records admission and what the cap pruned as JSON, as the place writes
/// them (an older shape must always parse), and the notes prepare left when
/// no run took them (a refusal after the cap).
fn record_admission(result: &mut HostResult, admission: Option<crate::admission::Admission>) {
    for after in crate::run::take_notes() {
        note(&mut result.reason, after);
    }
    let mut admission = admission;
    result.pruned = admission
        .as_mut()
        .and_then(|a| serde_json::to_value(a.pruned.take()?).ok());
    result.admission = admission.and_then(|admission| serde_json::to_value(admission).ok());
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
    let dir = record::run_dir(root, id);
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("supervisor.log"))?;
    nix::unistd::dup2_stderr(&log)?;
    let mut request: Request = serde_json::from_slice(&fs::read(dir.join("request.json"))?)?;
    let stop = execution::stopped()?;
    let mut result = HostResult::refused(&request)?;
    let reservation: record::Reservation =
        serde_json::from_slice(&fs::read(record::reserved_path(root, id))?)?;
    // One wait budget for all the run's locks, from its submit (SPEC §5).
    request.waits_end_ms = Some(reservation.submitted_ms + request.wait().as_millis());
    let deadline = deadline_ms(reservation.submitted_ms, &request);
    result.deadline_ms = Some(deadline);
    record::reconcile_seat(root, &request.seat);
    // The locks stay held until the record is final (SPEC §5: final record,
    // then close locks): each passes to `held` as it is taken, and `held`
    // drops only after finalize, on every path.
    let mut held = crate::lock::Held::default();
    let mut admission = None;
    // A panic anywhere in here is caught, so the verdict is still recorded
    // with `held` intact, and the locks still close only after finalize.
    let executed: Result<crate::run::Ended> = std::panic::catch_unwind(
        std::panic::AssertUnwindSafe(|| -> Result<crate::run::Ended> {
            let seat = execution::prepare(root, &request, &stop, &mut admission, &mut held)?;
            assert!(
                !execution::fail_at("supervise-panic"),
                "injected by FERRY_FAIL_AT"
            );
            let (Some(lock), Some(host_lease)) = (&held.seat, &held.host) else {
                return Err("prepare returned without its locks".into());
            };
            // The work inherits the seat's lock only; the host's stay here,
            // so nothing a run leaves can close the place (SPEC §3).
            let finished = crate::run::run_released(root, &seat, &request, &stop, lock, deadline);
            if let Some(error) = &finished.custody.stop_error {
                note(
                    &mut result.reason,
                    format!("custody release failed: {error}"),
                );
            }
            result.custody = serde_json::to_value(finished.custody).ok();
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
                crate::run::Ended::Result(_) | crate::run::Ended::Timeout
            ) {
                collect_artifacts(root, &seat, &request, &mut result);
            }
            result.lease = Some(host_lease.stamp());
            Ok(ended)
        }),
    )
    .unwrap_or_else(|_| Err("supervisor panicked".into()));
    let history_days = admission.as_ref().and_then(|a| a.history_days);
    record_admission(&mut result, admission);
    uncollected(&mut result, &request);
    // One verdict, from the first terminal fact (SPEC §5). A stop's signal is
    // the first stop's, whatever arrived after it.
    let signal = execution::caught_signal().map(|caught| first_stop_signal(&dir).unwrap_or(caught));
    let (outcome, exit_code, reason) = crate::run::verdict(
        &executed.map_err(|error| error.to_string()),
        signal,
        crate::run::attempted(root, &request),
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
    // for a reconcile once this supervisor is gone (and its locks with it).
    execution::pause_at("pre-finalize", &dir.join("finalize"));
    result.state = Some("final".into());
    let stored = serde_json::to_value(&result)
        .map_err(Into::into)
        .and_then(|value| record::finalize(root, id, &value));
    // The locks close only after the record is final, or at process death:
    // never while this supervisor lives with its record unwritten, since
    // its live reservation would then name a lock it no longer holds.
    match stored {
        Ok(()) => {
            // Closed, never unlocked: a survivor that inherited the seat's
            // descriptor keeps it held, so the retire below finds it busy.
            drop(held);
            after_final(root, &request, history_days);
            Ok(result.exit_code)
        }
        Err(error) => {
            execution::warn(&format!("ferry: host record: {error}"));
            execution::pause_at("finalize-failed", &dir.join("finalize-failed"));
            // `held` closes with the process; reconcile then proves it lost.
            std::process::exit(255)
        }
    }
}

/// What a run does once its record is final and its locks are closed, so
/// it never delays the recipe or the caller, who has the final record
/// already (SPEC §9): a `--once` run retires its seat, then a host that
/// bounds its run history deletes what is older.
fn after_final(root: &Path, request: &Request, history_days: Option<u64>) {
    if request.once {
        retire_once(root, request);
    }
    if let Some(days) = history_days {
        execution::warn(&crate::history::delete_old(root, &request.id, days));
    }
}

/// Retires a `--once` run's seat after its record is final and its locks
/// are closed, through the ordinary retire with `once_id` (SPEC §9): busy
/// while a survivor holds the lock, `used` if a later run took the seat,
/// `exclusive` while an exclusive run holds or waits for the host. What it
/// did goes to the supervisor's log only; a seat it leaves stays for
/// `status` to show and a later prune to retire.
fn retire_once(root: &Path, request: &Request) {
    let meant = Meant {
        idle_ms: None,
        once_id: Some(request.id.clone()),
    };
    execution::pause_at(
        "once-retire-pause",
        &record::run_dir(root, &request.id).join("once"),
    );
    let said = if execution::fail_at("once-retire") {
        Err("once-retire: injected by FERRY_FAIL_AT".into())
    } else {
        retire_seat(root, &request.seat, &meant)
    };
    let line = match said {
        Ok(said) => format!("ferry: --once retire: {said}"),
        Err(error) => format!("ferry: --once retire failed: {error}"),
    };
    execution::warn(&line);
}

/// The artifact status of a run that collected none: none declared, or the
/// run ended before its recipe did (stopped, refused, lost).
fn uncollected(result: &mut HostResult, request: &Request) {
    use crate::artifacts::{Kind, Status, failed};
    if result.artifacts_sha256.is_some() || result.artifacts_status.is_some() {
        return;
    }
    let status = if request.recipe.artifacts.is_empty() {
        Status::None
    } else {
        failed(
            Kind::Collection,
            None,
            "not collected: the run ended before its recipe",
        )
    };
    result.artifacts_status = serde_json::to_value(status).ok();
}

/// Archives the recipe's declared artifacts, each file hashed. A failure is
/// recorded in the result and never changes its verdict.
fn collect_artifacts(root: &Path, seat: &Path, request: &Request, result: &mut HostResult) {
    use crate::artifacts::{Kind, failed};
    if request.recipe.artifacts.is_empty() {
        return;
    }
    let output = record::run_dir(root, &request.id).join("artifacts.tar");
    let patterns = &request.recipe.artifacts;
    let collected =
        crate::artifacts::collect(seat, patterns, &request.preserve, &output).and_then(|listed| {
            let sha256 = receipt::file_digest(&output);
            Ok((
                listed,
                sha256.map_err(|e| failed(Kind::Collection, None, e))?,
            ))
        });
    match collected {
        Ok((mut listed, sha256)) => {
            // Debug builds: each file listed with a digest its bytes lack.
            if execution::fail_at("artifact-digest") {
                for artifact in &mut listed {
                    artifact.sha256 = receipt::digest(b"injected");
                }
            }
            result.artifacts = serde_json::to_value(listed).ok();
            result.artifacts_sha256 = Some(sha256);
        }
        Err(status) => {
            if let crate::artifacts::Status::Failed { error, .. } = &status {
                result.artifacts_error = Some(error.clone());
            }
            result.artifacts_status = serde_json::to_value(status).ok();
        }
    }
}

/// Writes the run's output from `offset` until its record is final and
/// every byte written before that is sent, or until its reader is gone.
/// Exit 0 at the end, 4 for a run this place never had.
pub fn follow(root: &Path, id: &str, offset: u64) -> Result<i32> {
    let reserved = record::reserved_path(root, id);
    if !reserved.exists() && record::reconcile(root, id)?.is_none() {
        return Ok(not_found());
    }
    let path = record::run_dir(root, id).join("output.log");
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
    use nix::sys::signal::Signal::{self, SIGHUP, SIGINT, SIGTERM};
    let signal = Signal::try_from(signal)?;
    if !matches!(signal, SIGINT | SIGTERM | SIGHUP) {
        return Ok(refuse(
            "a stop sends INT, TERM or HUP",
            "stop with one of them",
        ));
    }
    let path = record::reserved_path(root, id);
    let Ok(bytes) = fs::read(&path) else {
        return Ok(refuse(
            &format!("run {id} is not running here"),
            &format!("ferry record {id}"),
        ));
    };
    let reservation: record::Reservation = serde_json::from_slice(&bytes)?;
    if reservation.seat != seat && !other {
        return Ok(refuse(
            &format!("run {id} belongs to another caller"),
            &format!("ferry stop {id} --other --reason '<why>'"),
        ));
    }
    let pid = nix::unistd::Pid::from_raw(i32::try_from(reservation.supervisor.pid)?);
    let live = match crate::record::start_time(pid.as_raw()) {
        Ok(start) => reservation
            .supervisor
            .start
            .is_none_or(|then| then == start),
        Err(_) => nix::sys::signal::kill(pid, None).is_ok(),
    };
    if !live {
        return Ok(refuse(
            &format!("run {id}'s supervisor is gone"),
            &format!("ferry record {id}"),
        ));
    }
    let stopped = serde_json::json!({
        "who": who, "why": why, "other": other, "at_ms": receipt::now_ms(),
        "signal": signal as i32,
    });
    let written = if execution::fail_at("stop-write") {
        Err("stop-write: injected by FERRY_FAIL_AT".into())
    } else {
        record::write_new(
            &record::run_dir(root, id).join("stop.json"),
            stopped.to_string().as_bytes(),
            None,
        )
    };
    // The first stop decides: a later one is accepted and changes nothing.
    // It sends the first stop's signal again, in case that stop's sender
    // died between recording and signalling.
    let signal = match written {
        Ok(()) => signal,
        Err(error) => match first_stop_signal(&record::run_dir(root, id)) {
            Some(first) => nix::sys::signal::Signal::try_from(first)?,
            None => {
                return Ok(refuse(
                    &format!("recording the stop: {error}"),
                    "stop it again; the run goes on",
                ));
            }
        },
    };
    nix::sys::signal::kill(pid, signal)?;
    reply(&serde_json::json!({"accepted": id}));
    Ok(0)
}

/// The signal the first recorded stop of a run asked for.
fn first_stop_signal(dir: &Path) -> Option<i32> {
    let stop: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("stop.json")).ok()?).ok()?;
    stop["signal"]
        .as_i64()
        .and_then(|signal| i32::try_from(signal).ok())
}

/// The name a retire writes into a seat's lock file while it holds it, so a
/// run refused then is told the seat is being retired.
pub const RETIRING: &str = "retiring";

/// What a retire checks under the seat lock that the seat is still the one
/// its caller meant (SPEC §9), from `seats/<key>/used`.
#[derive(Debug, Clone, Default)]
pub struct Meant {
    /// Refused `used` when the seat ran within this many ms, on this clock.
    pub idle_ms: Option<u128>,
    /// Refused `used` when the seat's latest run is not this one.
    pub once_id: Option<String>,
}

/// The seat's latest run, as written when it took the seat lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Used {
    pub ms: u128,
    pub id: String,
}

/// Writes `seats/<key>/used` for the run that has just taken the seat lock,
/// renamed into place whole.
pub fn mark_used(root: &Path, key: &str, id: &str) -> Result<()> {
    if execution::fail_at("used-write") {
        return Err("used-write: injected by FERRY_FAIL_AT".into());
    }
    let rel = Path::new("seats").join(key);
    let seat = root.join(&rel);
    // Proven inside the root, no symbolic link on the way, before creating
    // and again before writing: a link there would carry either out of the
    // state root.
    let inside = || {
        crate::admission::outside(root, &rel)
            .map_or(Ok(()), |why| Err(format!("{why}: not written")))
    };
    inside()?;
    fs::create_dir_all(&seat)?;
    inside()?;
    // A temporary left by a write that died is unlinked (never followed),
    // and the new one created exclusively, so it is never a link.
    let tmp = seat.join("used.tmp");
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec(&Used {
        ms: receipt::now_ms(),
        id: id.to_owned(),
    })?)?;
    // A rename replaces `used` itself, even a link, never what it names.
    fs::rename(&tmp, seat.join("used"))?;
    Ok(())
}

/// The prefix of a retire's tombstone, `seats/.retired-<key>-<ms>`: never a
/// seat, so never counted, measured or capped as one.
pub const TOMBSTONE: &str = ".retired-";

/// Retires a seat (SPEC §6, §9), printing the reply: 0 retired, 125 not.
pub fn retire(root: &Path, key: &str, meant: &Meant) -> Result<i32> {
    let said = retire_seat(root, key, meant)?;
    reply(&said);
    Ok(if said["retired"].is_string() { 0 } else { 125 })
}

/// Retires a seat, trying every lock and waiting on none: a shared host
/// lease by try (an exclusive run holding the gate or `host` refuses it
/// `exclusive`), then the seat lock (held: `busy`, naming the holder), then
/// `used` under it (`used` when the seat ran since its caller meant it,
/// `unknown` when `used` cannot be read). Then, under both, it renames
/// `seats/<key>` to a tombstone, records `records/retired-<key>.json` and
/// deletes the tombstone; a delete that fails leaves only the tombstone and
/// is reported failed, so the caller comes back. A retire that retired also
/// deletes tombstones earlier retires left, under the same lease. The lock file is never removed: unlinking a
/// held flock file would let a new run lock a fresh file of the same name.
///
/// Lock order: host, then seat, the reverse of a run's; safe only because
/// both are tries, and nothing here waits on a lock while holding one.
pub fn retire_seat(root: &Path, key: &str, meant: &Meant) -> Result<serde_json::Value> {
    use serde_json::json;
    crate::config::identifier(key)?;
    let refused =
        |kind: &str, why: String, next: &str| json!({"refused": kind, "why": why, "next": next});
    let lock = root.join("locks").join(format!("seat.{key}"));
    if !lock.exists() {
        return Ok(refused(
            "absent",
            format!("no seat {key} on this place"),
            "nothing to retire",
        ));
    }
    let Some(_host) = crate::lock::try_shared_host(root)? else {
        return Ok(refused(
            "exclusive",
            "an exclusive run holds or waits for this place".into(),
            "prune again once it has ended",
        ));
    };
    let Some(_held) = crate::lock::try_seat(&lock, RETIRING)? else {
        let named = fs::read_to_string(&lock).unwrap_or_default();
        let why = record::busy(root, key, RETIRING, named.trim());
        return Ok(refused("busy", why, "prune again later"));
    };
    execution::pause_at("retire-pause", &lock);
    let seat = root.join("seats").join(key);
    if let Some(kept) = not_meant(&seat, key, meant) {
        return Ok(kept);
    }
    let rel = Path::new("seats").join(key);
    // Proven inside the root, no symbolic link on the way, before the
    // rename moves anything.
    if let Some(why) = crate::admission::outside(root, &rel) {
        return Ok(json!({"failed": key, "why": format!("{why}: not retired")}));
    }
    let bytes = crate::admission::size_in(root, &rel);
    // Renamed whole first, so a failed delete never leaves half a seat
    // under the key.
    let tombstone = match to_tombstone(root, key) {
        Ok(tombstone) => tombstone,
        Err(why) => return Ok(json!({"failed": key, "why": why})),
    };
    let retired = json!({"key": key, "bytes": bytes, "retired_ms": receipt::now_ms()});
    let path = root.join("records").join(format!("retired-{key}.json"));
    // The key's latest retire, replacing an earlier one whole: a key reused
    // and retired again ages from this retire. A retire that found nothing
    // to rename (a retry, an earlier retire's key) keeps the record it finds.
    let written = if tombstone.is_none() && path.exists() {
        Ok(())
    } else if execution::fail_at("retire-record") {
        Err("retire-record: injected by FERRY_FAIL_AT".into())
    } else {
        record::write_replacing(&path, retired.to_string().as_bytes()).map_err(|e| e.to_string())
    };
    // Not retired until recorded and deleted: the caller's book keeps the
    // seat, so its next prune comes back, finds the key gone and finishes.
    if let Err(error) = written {
        return Ok(json!({"failed": key, "why": format!("recording the retire: {error}")}));
    }
    let gone = tombstone.is_none();
    if let Some(name) = tombstone {
        let deleted = if execution::fail_at("retire-delete") {
            Err("retire-delete: injected by FERRY_FAIL_AT".to_owned())
        } else {
            crate::admission::remove(root, &Path::new("seats").join(&name))
                .map_err(|e| e.to_string())
        };
        if let Err(why) = deleted {
            return Ok(json!({
                "failed": key,
                "why": format!("deleting the tombstone seats/{name}: {why}; the next prune deletes it"),
            }));
        }
    }
    let mut said = json!({"retired": key, "bytes": bytes});
    // Only this key's tombstones, left by its own earlier retires: an
    // exclusive run arriving meanwhile waits for this seat's deletes only.
    // An incomplete sweep is a failure: the caller's book keeps the key, so
    // its next prune comes back for what is left.
    match delete_tombstones(root, key) {
        // Nothing was under the key, nor left of it: an earlier retire (a
        // --once run's, one by hand) already took it.
        Ok(0) if gone => said["gone"] = true.into(),
        Ok(_) => {}
        Err(why) => {
            return Ok(json!({
                "failed": key,
                "why": format!("{why}; the next prune deletes it"),
            }));
        }
    }
    Ok(said)
}

/// The refusal when the seat is no longer the one its caller meant (SPEC
/// §9), read from `seats/<key>/used` under the seat lock: `used` when it ran
/// within `idle_ms` on this clock or its latest run is not `once_id`,
/// `unknown` when `used` cannot be read. No `used` means no run since this
/// rule shipped: the caller's age stands.
fn not_meant(seat: &Path, key: &str, meant: &Meant) -> Option<serde_json::Value> {
    use serde_json::json;
    let used = match fs::read(seat.join("used")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        read => read
            .map_err(|e| e.to_string())
            .and_then(|bytes| serde_json::from_slice::<Used>(&bytes).map_err(|e| e.to_string())),
    };
    let used = match used {
        Ok(used) => used,
        Err(why) => return Some(json!({"unknown": key, "why": format!("its used record: {why}")})),
    };
    let now = receipt::now_ms();
    let why = if meant
        .idle_ms
        .is_some_and(|idle| used.ms.saturating_add(idle) > now)
    {
        format!(
            "used {} s ago by run {}",
            now.saturating_sub(used.ms) / 1000,
            used.id
        )
    } else if meant.once_id.as_ref().is_some_and(|once| *once != used.id) {
        format!("its latest run is {}, not this one", used.id)
    } else {
        return None;
    };
    Some(json!({"refused": "used", "why": why, "next": "it is not idle"}))
}

/// Renames `seats/<key>` to a fresh tombstone `seats/.retired-<key>-<ms>`,
/// never over an existing one; `None` when there was no seat directory.
fn to_tombstone(root: &Path, key: &str) -> std::result::Result<Option<String>, String> {
    let seat = root.join("seats").join(key);
    if fs::symlink_metadata(&seat).is_err() {
        return Ok(None);
    }
    if execution::fail_at("retire-rename") {
        return Err("retire-rename: injected by FERRY_FAIL_AT".into());
    }
    let mut ms = receipt::now_ms();
    let named = loop {
        let name = format!("{TOMBSTONE}{key}-{ms}");
        if fs::symlink_metadata(root.join("seats").join(&name)).is_err() {
            break name;
        }
        ms += 1;
    };
    fs::rename(&seat, root.join("seats").join(&named))
        .map_err(|e| format!("renaming seats/{key}: {e}"))?;
    Ok(Some(named))
}

/// Deletes the tombstones earlier retires of `key` left under `seats/`
/// (`.retired-<key>-<ms>`, exactly: never another key's), under the
/// caller's shared host lease: how many there were. The first failure is
/// returned; the rest are still tried.
fn delete_tombstones(root: &Path, key: &str) -> std::result::Result<usize, String> {
    let mut first = None;
    let prefix = format!("{TOMBSTONE}{key}-");
    if execution::fail_at("tombstone-sweep") {
        return Err("tombstone-sweep: injected by FERRY_FAIL_AT".into());
    }
    // A listing that fails cannot show the key's tombstones gone.
    let listed = match fs::read_dir(root.join("seats")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        listed => listed.map_err(|e| format!("listing seats: {e}"))?,
    };
    let names: Vec<String> = listed
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(|e| format!("listing seats: {e}"))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|name| {
            name.strip_prefix(&prefix)
                .is_some_and(|ms| !ms.is_empty() && ms.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    let count = names.len();
    for name in names {
        let rel = Path::new("seats").join(&name);
        let deleted = crate::admission::remove(root, &rel).map_err(|e| e.to_string());
        if let Err(why) = deleted {
            first.get_or_insert(format!("deleting {}: {why}", rel.display()));
        }
    }
    first.map_or(Ok(count), Err)
}

/// Prints the run's final record (exit 0), or its reservation while it is
/// not final (exit 3), or nothing for a run this place never had (exit 4).
pub fn print_record(root: &Path, id: &str) -> Result<i32> {
    if execution::fail_at("record-read") {
        return Err("record-read: injected by FERRY_FAIL_AT".into());
    }
    if let Some(bytes) = record::reconcile(root, id)? {
        execution::emit(&bytes);
        return Ok(0);
    }
    match fs::read(record::reserved_path(root, id)) {
        Ok(bytes) => {
            execution::emit(&bytes);
            Ok(3)
        }
        Err(_) => Ok(not_found()),
    }
}

/// Says a run is not found here (exit 4), naming how long this host keeps
/// run history: context, not proof, since the id may never have existed.
fn not_found() -> i32 {
    match crate::admission::history_kept_days() {
        Some(days) => execution::warn(&format!(
            "not found (this host keeps run history for {days} days)"
        )),
        None => execution::warn("not found"),
    }
    4
}
