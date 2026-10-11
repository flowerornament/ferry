//! Locked real checkouts and recipe sessions shared by caller and host.
use crate::{
    config::{Hooks, Recipe, Refusal, identifier, preserve_path},
    limits::{COLLECT_TIMEOUT, SESSION_CLEANUP},
    receipt, repository,
};
use fs2::FileExt;
use nix::{
    sys::signal::{Signal, kill},
    unistd::{Pid, getsid, setsid},
};
use serde::{Deserialize, Serialize};
use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct SessionOwner {
    child: std::process::Child,
    cleaned: bool,
    quarantine: PathBuf,
    ownership: PathBuf,
}
impl SessionOwner {
    fn cleanup(&mut self) -> Result<()> {
        let result = self.cleanup_session();
        if let Err(error) = &result {
            // A released OS lock must not make an unconfirmed session reusable.
            fs::write(&self.quarantine, error.to_string())?;
        } else if self.quarantine.exists() {
            fs::remove_file(&self.quarantine)?;
        }
        result
    }

    fn cleanup_session(&mut self) -> Result<()> {
        if fail_at("session-cleanup") {
            return Err("session cleanup failed: injected by FERRY_FAIL_AT".into());
        }
        let sid = self.child.id();
        signal_session(sid, Signal::SIGTERM)?;
        let deadline = Instant::now() + SESSION_CLEANUP;
        while Instant::now() < deadline {
            // Do not reap the owner until all other session members are gone.
            let members = session_members(sid)?;
            if members
                .iter()
                .all(|pid| u32::try_from(*pid).ok() == Some(sid))
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        signal_session(sid, Signal::SIGKILL)?;
        // Also handles failure before the child called setsid.
        match self.child.kill() {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {}
            Err(e) => return Err(format!("kill retained session leader {sid}: {e}").into()),
        }
        self.child
            .wait()
            .map_err(|e| format!("wait for retained session leader {sid}: {e}"))?;
        if !session_members(sid)?.is_empty() {
            return Err("session cleanup unconfirmed".into());
        }
        self.cleaned = true;
        fs::remove_file(&self.ownership)?;
        Ok(())
    }
}
impl Drop for SessionOwner {
    fn drop(&mut self) {
        if !self.cleaned
            && let Err(e) = self.cleanup()
        {
            warn(&format!("ferry: cleanup failed: {e}"));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub protocol: u32,
    pub id: String,
    pub token: String,
    pub commit: String,
    pub seat: String,
    pub recipe: Recipe,
    pub hooks: Hooks,
    pub caller_binary_sha256: String,
    pub timeout_seconds: Option<u64>,
    pub mirrors: Vec<Mirrored>,
    pub forwarded_env: BTreeMap<String, String>,
    #[serde(default)]
    pub companion_refs: BTreeMap<String, String>,
    #[serde(default)]
    pub preserve: Vec<String>,
    // Omitted when empty so argument-free runs still reach hosts that predate
    // arguments; such hosts refuse a request that carries them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    // Omitted when empty, like args; a host that predates it refuses it.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub provider_requires: BTreeSet<String>,
    /// How long the run may wait for its locks, all together; the place's
    /// default if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_seconds: Option<u64>,
    /// When every lock wait ends (unix ms): submitted + wait, set by the
    /// supervisor, so the waits together never pass the deadline.
    #[serde(skip)]
    pub waits_end_ms: Option<u128>,
}
impl Request {
    /// The bound on all lock waits together.
    pub fn wait(&self) -> Duration {
        self.wait_seconds
            .map_or(crate::limits::WAIT, Duration::from_secs)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mirrored {
    pub url: String,
    pub commit: String,
    pub key: String,
}

pub fn validate(request: &Request) -> Result<()> {
    if request.protocol != receipt::PROTOCOL {
        return Err(Refusal::Declaration("protocol mismatch".into()).into());
    }
    for value in [&request.id, &request.token, &request.seat] {
        identifier(value)?;
    }
    if request.commit.len() != 40 || !request.commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid commit".into());
    }
    for (name, commit) in &request.companion_refs {
        repository::ref_name(name)?;
        if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("invalid companion commit".into());
        }
    }
    if request.recipe.command.is_empty() {
        return Err("empty command".into());
    }
    crate::receipt::provides(&request.provider_requires, &crate::receipt::capabilities())?;
    if request.args.iter().any(|arg| arg.contains('\0')) {
        return Err("invalid recipe argument".into());
    }
    if request.forwarded_env.iter().any(|(name, value)| {
        name.is_empty()
            || name.contains(['=', '\0'])
            || value.contains('\0')
            || name.starts_with("FERRY_")
            || name == "GIT_CONFIG_GLOBAL"
    }) {
        return Err("invalid forwarded environment".into());
    }
    for hook in [
        &request.hooks.prepare,
        &request.hooks.env,
        &request.hooks.collect,
    ]
    .into_iter()
    .flatten()
    {
        crate::config::relative_path(hook)?;
    }
    for mirror in &request.mirrors {
        identifier(&mirror.key)?;
        if mirror.commit.len() != 40 || !mirror.commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid mirror commit".into());
        }
    }
    Ok(())
}
/// Writes one line to stdout, never panicking. False if it could not.
pub fn emit(line: &[u8]) -> bool {
    let mut out = std::io::stdout().lock();
    out.write_all(line)
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
        .is_ok()
}

/// Writes a diagnostic line to stderr, ignoring a closed stderr.
pub fn warn(message: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{message}");
}

/// Debug builds only: whether a control asked this fallible point to fail
/// (`FERRY_FAIL_AT=<point>[,<point>…]`). Release builds never read it.
pub fn fail_at(point: &str) -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var("FERRY_FAIL_AT").is_ok_and(|points| points.split(',').any(|p| p == point))
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = point;
        false
    }
}

/// The last of SIGINT, SIGTERM or SIGHUP this process caught; zero for none.
static CAUGHT: LazyLock<Arc<AtomicUsize>> = LazyLock::new(|| Arc::new(AtomicUsize::new(0)));

pub fn stopped() -> Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, flag.clone())?;
        signal_hook::flag::register_usize(signal, CAUGHT.clone(), usize::try_from(signal)?)?;
    }
    Ok(flag)
}

/// The signal this process caught, if any.
pub fn caught_signal() -> Option<i32> {
    i32::try_from(CAUGHT.load(Ordering::SeqCst))
        .ok()
        .filter(|signal| *signal != 0)
}

pub fn signal_name(signal: i32) -> String {
    nix::sys::signal::Signal::try_from(signal)
        .map_or_else(|_| format!("signal {signal}"), |s| s.as_str().to_owned())
}

/// Refuses to start another phase once this process has been signalled.
/// The caller checks at every phase boundary and immediately before dispatch.
pub fn interrupted() -> Result<()> {
    match caught_signal() {
        Some(signal) => {
            Err(format!("caller received {} before dispatch", signal_name(signal)).into())
        }
        None => Ok(()),
    }
}

/// Polls for a lock until it is granted, `stop` is set, `waiting` (run on
/// every contended attempt) refuses, or the run's one wait budget ends
/// (`waits_end_ms`, else `wait` from now); then it refuses naming the holder. An exclusive holder writes its run id into
/// the lock file for that.
fn acquire_as(
    path: &Path,
    stop: &AtomicBool,
    shared: bool,
    waiting: &dyn Fn() -> Result<()>,
    request: &Request,
) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let started = Instant::now();
    let ends_ms = request
        .waits_end_ms
        .unwrap_or_else(|| crate::receipt::now_ms() + request.wait().as_millis());
    let mut said = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err("stopped while waiting for a lock".into());
        }
        let locked = if shared {
            FileExt::try_lock_shared(&file)
        } else {
            file.try_lock_exclusive()
        };
        match locked {
            Ok(()) => {
                if !shared {
                    file.set_len(0)?;
                    (&file).write_all(request.id.as_bytes())?;
                }
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                waiting()?;
                if crate::receipt::now_ms() >= ends_ms {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let holder = fs::read_to_string(path).unwrap_or_default();
                    let holder = match holder.trim() {
                        "" => "shared runs".to_owned(),
                        id => format!("run {id}"),
                    };
                    return Err(format!(
                        "waited {:?} for lock {name}, the end of the run's wait: held by {holder}",
                        started.elapsed()
                    )
                    .into());
                }
                if !said && started.elapsed() >= Duration::from_secs(3) {
                    said = true;
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    warn(&format!(
                        "ferry: waiting for lock {name}: another run holds it"
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    }
}
impl Drop for HostLease {
    /// An exclusive holder clears its name before the lock drops, so a
    /// later wait never names a run that is gone.
    fn drop(&mut self) {
        if self.record.mode == "exclusive" {
            for file in &self.held {
                let _ = file.set_len(0);
            }
        }
    }
}

/// The host-wide lease every session holds while it runs. Ordinary runs
/// share it. A recipe that requires `exclusive` holds it alone: it first
/// closes the gate, so no new run starts, then waits for in-flight runs to
/// release their shared side. New runs pass the gate briefly before taking
/// their shared side, so once the exclusive run holds the gate, later runs
/// cannot extend its wait; before that it competes for the gate, unqueued. The lease
/// covers sessions under this state root only: not other users, processes
/// outside Ferry, or other machines sharing the hardware.
///
/// Locks are released only by closing their descriptors (dropping the
/// files), never by `LOCK_UN`: a session may share the open file
/// description through inherited descriptors, and an explicit unlock would
/// free it under that session.
pub struct HostLease {
    held: Vec<fs::File>,
    pub record: Lease,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lease {
    /// `shared` or `exclusive`.
    pub mode: String,
    /// How long the session waited for the lease.
    pub wait_ms: u128,
    /// When the lease was granted and released (ms since the epoch).
    #[serde(default)]
    pub acquired_ms: Option<u128>,
    #[serde(default)]
    pub released_ms: Option<u128>,
}

impl HostLease {
    /// The lock descriptors this lease holds, for a session to inherit.
    pub fn files(&self) -> impl Iterator<Item = &fs::File> {
        self.held.iter()
    }
    /// The lease record, stamped with the release time while the lease is
    /// still held; the lease itself is released when this value is dropped.
    pub fn stamp(&self) -> Lease {
        Lease {
            released_ms: Some(crate::receipt::now_ms()),
            ..self.record.clone()
        }
    }
    fn acquire(root: &Path, request: &Request, stop: &AtomicBool) -> Result<Self> {
        let started = std::time::Instant::now();
        let gate = root.join("locks/host-gate");
        // The name earlier binaries used for their exclusive lock, so an old and
        // a new exclusive run on one state root still exclude each other.
        let host = root.join("locks/host-exclusive");
        let exclusive = request.recipe.requires.contains("exclusive");
        let local = root.ends_with("local");
        let held = if exclusive {
            // A tainted seat's survivors may hold the lease: no exclusive claim.
            let tainted = || crate::custody::check_exclusive(root, local);
            tainted()?;
            let gate = acquire_as(&gate, stop, false, &tainted, request)?;
            let host = acquire_as(&host, stop, false, &tainted, request)?;
            tainted()?;
            vec![gate, host]
        } else {
            // A tainted exclusive run's survivors may hold the gate or lease.
            let tainted = || crate::custody::check_host(root, local);
            // Before the first try too, not only once contended.
            tainted()?;
            let gate = acquire_as(&gate, stop, true, &tainted, request)?;
            let host = acquire_as(&host, stop, true, &tainted, request)?;
            drop(gate);
            vec![host]
        };
        Ok(Self {
            held,
            record: Lease {
                mode: if exclusive { "exclusive" } else { "shared" }.into(),
                wait_ms: started.elapsed().as_millis(),
                acquired_ms: Some(crate::receipt::now_ms()),
                released_ms: None,
            },
        })
    }
}

pub fn prepare(
    root: &Path,
    request: &Request,
    stop: &AtomicBool,
    admission: &mut Option<crate::admission::Admission>,
) -> Result<(PathBuf, fs::File, HostLease)> {
    validate(request)?;
    fs::create_dir_all(root.join("locks"))?;
    let local = root.ends_with("local");
    // A tainted seat refuses before, while and after waiting for its lock: a
    // survivor that kept the lock would otherwise keep this run waiting.
    let tainted = || crate::custody::check(root, &request.seat, local);
    tainted()?;
    let lease = acquire_as(
        &root.join("locks").join(&request.seat),
        stop,
        false,
        &tainted,
        request,
    )?;
    crate::seat::check(root, &request.seat, local)?;
    tainted()?;
    crate::custody::begin(root, &request.seat)?;
    for entry in fs::read_dir(root.join("locks"))? {
        if entry?
            .file_name()
            .to_string_lossy()
            .starts_with("cleanup-pending-")
        {
            return Err("cleanup_pending: an earlier session has unconfirmed teardown".into());
        }
    }
    let exclusive = HostLease::acquire(root, request, stop)?;
    // Measured after any wait for the lease, under this seat's lock.
    crate::admission::admit(
        root,
        &request.seat,
        admission.get_or_insert_with(Default::default),
    )?;
    let bare = root.join("repo.git");
    repository::output(Command::new("git").arg("--git-dir").arg(&bare).args([
        "cat-file",
        "-e",
        &format!("{}^{{commit}}", request.commit),
    ]))?;
    let seat = root.join("slots").join(&request.seat);
    fs::create_dir_all(root.join("slots"))?;
    for path in &request.preserve {
        preserve_path(path)?;
    }
    migrate_legacy(&bare, &seat)?;
    if !seat.exists() {
        repository::output(
            Command::new("git")
                .args(["clone", "--shared", "--no-checkout"])
                .arg(&bare)
                .arg(&seat),
        )?;
    }
    if !seat.join(".git").is_dir() {
        return Err("seat has a legacy shared-ref checkout; select a new --seat".into());
    }
    reset_checkout(&seat, request)?;
    materialize_refs(root, &seat, request)?;
    if !request.mirrors.is_empty() {
        fs::create_dir_all(seat.join(".ferry"))?;
        let config = seat.join(".ferry/gitconfig");
        for mirror in &request.mirrors {
            let store = root.join("mirrors").join(format!("{}.git", mirror.key));
            repository::output(Command::new("git").arg("--git-dir").arg(&store).args([
                "cat-file",
                "-e",
                &format!("{}^{{commit}}", mirror.commit),
            ]))?;
            repository::output(
                Command::new("git")
                    .args(["config", "--file"])
                    .arg(&config)
                    .arg(format!("url.file://{}/.insteadOf", store.display()))
                    .arg(&mirror.url),
            )?;
        }
    }
    Ok((seat, lease, exclusive))
}

fn reset_checkout(seat: &Path, request: &Request) -> Result<()> {
    // Check index flags BEFORE resetting: reset would conceal tampering.
    let index = repository::output(
        Command::new("git")
            .current_dir(seat)
            .args(["ls-files", "-v"]),
    )?;
    if index.split(|b| *b == b'\n').any(|line| {
        line.first()
            .is_some_and(|b| b.is_ascii_lowercase() || *b == b'S')
    }) {
        return Err("hidden index entries".into());
    }
    repository::output(Command::new("git").current_dir(seat).args([
        "checkout",
        "--force",
        "--detach",
        &request.commit,
    ]))?;
    let mut clean = Command::new("git");
    clean.current_dir(seat).args(["clean", "-ffdx"]);
    for path in &request.preserve {
        repository::output(Command::new("git").current_dir(seat).args([
            "check-ignore",
            "--no-index",
            "--",
            &format!("{path}/"),
        ]))?;
        if !repository::output(
            Command::new("git")
                .current_dir(seat)
                .args(["ls-files", "--", path]),
        )?
        .is_empty()
        {
            return Err("preserve path contains tracked files".into());
        }
        clean.args(["-e", &format!("/{path}/")]);
    }
    repository::output(&mut clean)?;
    let head = repository::output(
        Command::new("git")
            .current_dir(seat)
            .args(["rev-parse", "HEAD"]),
    )?;
    let status = repository::output(Command::new("git").current_dir(seat).args([
        "status",
        "--porcelain",
        "--untracked-files=all",
    ]))?;
    if String::from_utf8(head)?.trim() != request.commit || !status.is_empty() {
        return Err("checkout integrity failed".into());
    }
    Ok(())
}

fn migrate_legacy(bare: &Path, seat: &Path) -> Result<()> {
    if !seat.join(".git").is_file() {
        return Ok(());
    }
    let common = repository::output(Command::new("git").current_dir(seat).args([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ]))?;
    if fs::canonicalize(String::from_utf8(common)?.trim())? != fs::canonicalize(bare)? {
        return Err("legacy seat belongs to another repository".into());
    }
    let status = repository::output(Command::new("git").current_dir(seat).args([
        "status",
        "--porcelain",
        "--untracked-files=all",
    ]))?;
    let index = repository::output(
        Command::new("git")
            .current_dir(seat)
            .args(["ls-files", "-v"]),
    )?;
    if !status.is_empty()
        || index.split(|b| *b == b'\n').any(|line| {
            line.first()
                .is_some_and(|b| b.is_ascii_lowercase() || *b == b'S')
        })
    {
        return Err("legacy seat has changed or hidden entries".into());
    }
    // This is Ferry-owned state, checked under its lease and durable owner gate.
    // Ignored build outputs have the same disposable contract as clean -ffdx.
    repository::output(
        Command::new("git")
            .arg("--git-dir")
            .arg(bare)
            .args(["worktree", "remove", "--force"])
            .arg(seat),
    )?;
    repository::output(
        Command::new("git")
            .arg("--git-dir")
            .arg(bare)
            .args(["worktree", "prune"]),
    )?;
    Ok(())
}

fn materialize_refs(root: &Path, seat: &Path, request: &Request) -> Result<()> {
    let directory = root.join("companion-refs");
    fs::create_dir_all(&directory)?;
    let manifest = directory.join(format!("{}.json", request.seat));
    let previous: BTreeMap<String, String> = match fs::read(&manifest) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => return Err(error.into()),
    };
    for name in previous
        .keys()
        .filter(|name| !request.companion_refs.contains_key(*name))
    {
        repository::ref_name(name)?;
        repository::output(
            Command::new("git")
                .current_dir(seat)
                .args(["update-ref", "-d", name]),
        )?;
    }
    for (name, commit) in &request.companion_refs {
        repository::output(Command::new("git").current_dir(seat).args([
            "cat-file",
            "-e",
            &format!("{commit}^{{commit}}"),
        ]))?;
        repository::output(Command::new("git").current_dir(seat).args([
            "update-ref",
            name,
            commit,
        ]))?;
    }
    fs::write(manifest, serde_json::to_vec(&request.companion_refs)?)?;
    Ok(())
}

pub fn session_members(sid: u32) -> Result<Vec<i32>> {
    Ok(process_ids()?
        .into_iter()
        .filter(|pid| {
            getsid(Some(Pid::from_raw(*pid)))
                .is_ok_and(|session| u32::try_from(session.as_raw()).ok() == Some(sid))
                && !zombie(*pid)
        })
        .collect())
}

/// A zombie has exited and holds nothing. Under the custody subreaper,
/// killed session members wait as zombies until the release census reaps them.
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
    // Read the same OS census directly, without weakening the sandbox.
    Ok(
        libproc::processes::pids_by_type(libproc::processes::ProcFilter::All)?
            .into_iter()
            .filter_map(|pid| i32::try_from(pid).ok())
            .collect(),
    )
}

#[cfg(not(target_os = "macos"))]
fn process_ids() -> Result<Vec<i32>> {
    let bytes = repository::output(Command::new("ps").args(["-axo", "pid="]))?;
    Ok(String::from_utf8(bytes)?
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect())
}
fn signal_session(sid: u32, signal: Signal) -> Result<()> {
    // The retained session leader keeps the SID from being reused until reaped.
    for pid in session_members(sid)? {
        match kill(Pid::from_raw(pid), signal) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(e) => {
                return Err(format!("signal {signal:?} to session {sid} member {pid}: {e}").into());
            }
        }
    }
    Ok(())
}
pub fn attempted(root: &Path, request: &Request) -> bool {
    root.join("runs")
        .join(&request.id)
        .join("result.attempted")
        .exists()
}

/// Sets or clears close-on-exec on a lock descriptor.
fn close_on_exec(file: &fs::File, close: bool) -> Result<()> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    let flags = if close {
        FdFlag::FD_CLOEXEC
    } else {
        FdFlag::empty()
    };
    fcntl(file, FcntlArg::F_SETFD(flags))?;
    Ok(())
}

/// The verdict, decided once by the first terminal fact (Rule B):
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

fn note_after(note: String) {
    if let Ok(mut notes) = NOTES.lock() {
        notes.push(note);
    }
}

/// What a supervised run came to. Custody is always present once a session
/// may have been spawned (S8).
pub struct Finished {
    pub executed: Result<Ended>,
    pub collection: Option<Collection>,
    pub custody: crate::custody::Release,
    /// Failures after the verdict: recorded, never changing it.
    pub notes: Vec<String>,
    pub output: Output,
}

/// The lock descriptors a session inherits: the seat lock and the host
/// lease, only where survivors can be stopped (the Linux census). Elsewhere
/// a survivor that kept them would hold them indefinitely, so they stay
/// close-on-exec and a killed supervisor frees them while the recipe lives.
pub fn inherited<'a>(seat: &'a fs::File, lease: &'a HostLease) -> Vec<&'a fs::File> {
    if cfg!(target_os = "linux") {
        std::iter::once(seat).chain(lease.files()).collect()
    } else {
        vec![]
    }
}

/// Runs the recipe and collect sessions, then custody release, while the
/// locks are still held. No failure in between skips the release: every
/// path collects, censuses and stops survivors, then reports.
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
    // Only a recipe that reached its own result (or timed out) may leave the
    // seat idle.
    let terminal = matches!(executed, Ok(Ended::Result(_) | Ended::Timeout));
    let custody = crate::custody::release(root, request, terminal);
    Finished {
        executed,
        collection,
        custody,
        output,
        notes: NOTES
            .lock()
            .map(|mut n| std::mem::take(&mut *n))
            .unwrap_or_default(),
    }
}

/// Runs the recipe session, then the collect hook's. `locks` are the seat
/// lock and host lease descriptors: each session inherits them, so they stay
/// held while the session lives even if this supervisor is killed.
pub fn run(
    root: &Path,
    workdir: &Path,
    request: &Request,
    stop: &AtomicBool,
    locks: &[&fs::File],
    deadline_ms: u128,
) -> (Result<Ended>, Option<Collection>, Output) {
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
        Ok((code, false)) => Collection {
            exit_code: Some(code),
            error: None,
        },
        Ok((code, true)) => Collection {
            exit_code: Some(code),
            error: Some("collection timed out".into()),
        },
        Err(error) => Collection {
            exit_code: None,
            error: Some(error.to_string()),
        },
    };
    let path = root.join("runs").join(&request.id).join("collection.json");
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
    {
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
        if crate::seat::path(root, &request.seat).exists() {
            return Err("recipe teardown unconfirmed; collection not started".into());
        }
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
        close_on_exec(lock, false)?;
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
    let restored = locks.iter().try_for_each(|lock| close_on_exec(lock, true));
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
    let run_dir = root.join("runs").join(&request.id);
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
    let run_dir = root.join("runs").join(&request.id);
    fs::create_dir_all(&run_dir)?;
    let spec = run_dir.join("request.json");
    fs::write(&spec, serde_json::to_vec(request)?)?;
    let result_path = run_dir.join("result.json");
    // A stop that came while the run prepared: nothing is spawned.
    if stop.load(Ordering::SeqCst) {
        return Ok(Ended::Stopped);
    }
    crate::seat::reserve(root, request)?;
    let (reader, writer) = std::io::pipe()?;
    let (spawned, restored) = spawn_session(workdir, &spec, &result_path, locks, writer)?;
    let child = spawned?;
    let mut owner = SessionOwner {
        child,
        cleaned: false,
        quarantine: root
            .join("locks")
            .join(format!("cleanup-pending-{}", request.id)),
        ownership: crate::seat::path(root, &request.seat),
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
        crate::custody::reap_adopted(owner.child.id());
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
        if let Some(status) = owner.child.try_wait()? {
            return Err(format!("session owner exited without result: {status}").into());
        }
        thread::sleep(Duration::from_millis(20));
    };
    // A teardown that cannot be confirmed quarantines the host (cleanup
    // pending) and taints nothing silently; after the verdict it is a note.
    if let Err(error) = owner.cleanup() {
        note_after(format!("session teardown: {error}"));
    }
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
    #[cfg(debug_assertions)]
    if let Some(cap) = std::env::var("FERRY_OUTPUT_CAP")
        .ok()
        .and_then(|cap| cap.parse().ok())
    {
        return cap;
    }
    crate::limits::OUTPUT_CAP
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
    crate::seat::bind(root, &request)?;
    let workdir = std::env::current_dir()?;
    // A Ferry-owned, per-seat cache directory for regenerable build output.
    let cache = root.join("caches").join(&request.seat);
    fs::create_dir_all(&cache)?;
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700))?;
    let mut env = BTreeMap::from([
        ("FERRY_SEAT".to_owned(), request.seat.clone()),
        ("FERRY_COMMIT".to_owned(), request.commit.clone()),
        ("FERRY_WORKDIR".to_owned(), workdir.display().to_string()),
        ("FERRY_CACHE_DIR".to_owned(), cache.display().to_string()),
        ("FERRY_ID".to_owned(), request.id.clone()),
        ("FERRY_REQUEST_FILE".to_owned(), spec.display().to_string()),
        (
            "FERRY_MIRRORS_DIR".to_owned(),
            root.join("mirrors").display().to_string(),
        ),
    ]);
    if request
        .forwarded_env
        .keys()
        .any(|name| name.starts_with("FERRY_") || name == "GIT_CONFIG_GLOBAL")
    {
        return Err("reserved forwarded environment".into());
    }
    env.extend(request.forwarded_env.clone());
    if !request.mirrors.is_empty() {
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
        if additions.iter().any(|(k, v)| {
            k.is_empty()
                || k.contains(['=', '\0'])
                || v.contains('\0')
                || k.starts_with("FERRY_")
                || k == "GIT_CONFIG_GLOBAL"
                || request.forwarded_env.contains_key(k)
        }) {
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
    use super::{Ended, verdict};
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

#[cfg(test)]
mod lock_tests {
    /// Release is by close: while any descriptor of the open file
    /// description lives (an inherited copy, here a clone), the lock is held,
    /// which is why Ferry never unlocks explicitly.
    #[test]
    fn a_lock_is_released_only_when_every_descriptor_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let held = std::fs::File::create(&path).unwrap();
        fs2::FileExt::try_lock_exclusive(&held).unwrap();
        let inherited = held.try_clone().unwrap();
        drop(held);
        let probe = std::fs::File::open(&path).unwrap();
        assert!(
            fs2::FileExt::try_lock_exclusive(&probe).is_err(),
            "freed while a copy lived"
        );
        drop(inherited);
        assert!(fs2::FileExt::try_lock_exclusive(&probe).is_ok());
    }
}
