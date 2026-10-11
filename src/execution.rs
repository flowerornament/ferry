//! Locked real checkouts and recipe sessions shared by caller and host.
use crate::{
    config::{Hooks, Recipe, Refusal, identifier, preserve_path},
    receipt, repository,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

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
    pub forwarded_env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ship: Vec<Shipped>,
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

/// A shipped object as the place sees it: a commit in the store, made a ref
/// in the checkout or served at a URL (SPEC §5 objects).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shipped {
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub commit: String,
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
    for shipped in &request.ship {
        match (&shipped.name, &shipped.url) {
            (Some(name), None) => repository::ref_name(name)?,
            (None, Some(url)) if !url.is_empty() && !url.contains(['\n', '\0']) => {}
            _ => return Err("invalid ship entry".into()),
        }
        if shipped.commit.len() != 40 || !shipped.commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid ship commit".into());
        }
    }
    if request.recipe.command.is_empty() {
        return Err("empty command".into());
    }
    // Before anything touches the disk: the cap deletes under these paths
    // (slice 5 review S2; checked again where it deletes).
    for path in &request.preserve {
        crate::config::preserve_path(path)?;
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
/// Debug builds, `FERRY_FAIL_AT=<point>`: mark `<at>.paused` and wait (at
/// most 60 s) for `<at>.go`, so a control can act inside a window.
pub fn pause_at(point: &str, at: &Path) {
    if !fail_at(point) {
        return;
    }
    let (paused, go) = (at.with_extension("paused"), at.with_extension("go"));
    let _ = fs::write(&paused, "");
    let until = Instant::now() + Duration::from_secs(60);
    while !go.exists() && Instant::now() < until {
        thread::sleep(Duration::from_millis(20));
    }
}

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

pub fn prepare(
    root: &Path,
    request: &Request,
    stop: &AtomicBool,
    admission: &mut Option<crate::admission::Admission>,
    held: &mut crate::lock::Held,
) -> Result<PathBuf> {
    validate(request)?;
    fs::create_dir_all(root.join("locks"))?;
    // A busy seat refuses at once, naming the run that holds it (SPEC §5).
    pause_at(
        "seat-try",
        &root.join("runs").join(&request.id).join("seat"),
    );
    let path = root.join("locks").join(&request.seat);
    let Some(lease) = crate::lock::try_seat(&path, &request.id)? else {
        let named = fs::read_to_string(&path).unwrap_or_default();
        return Err(crate::record::busy(root, &request.seat, &request.id, named.trim()).into());
    };
    // Each lock goes to the supervisor as soon as it is taken, so no later
    // refusal here closes it before the record is final.
    held.seat = Some(lease);
    held.host = Some(crate::lock::HostLease::acquire(
        root,
        request,
        stop,
        &mut held.partial,
    )?);
    // Measured after any wait for the lease, under this seat's lock.
    crate::admission::admit(root, admission.get_or_insert_with(Default::default))?;
    // The per-seat cap, under this run's seat lock, before the checkout: a
    // failure to wipe is a note, and the run goes on with what is there.
    let admitted = admission.get_or_insert_with(Default::default);
    if let Some(cap) = admitted.seat_cap_bytes {
        // Not when the run has spent its wait budget: measuring would eat
        // into its recipe's time (review P2).
        let spent = request
            .waits_end_ms
            .is_some_and(|end| receipt::now_ms() >= end);
        match crate::admission::cap(root, &request.seat, &request.preserve, cap, spent) {
            Ok(wiped) => {
                if let Some(wiped) = &wiped {
                    crate::run::note_after(format!(
                        "seat over its cap ({} bytes > {}): wiped, the run started cold",
                        wiped.bytes, wiped.cap
                    ));
                }
                admitted.pruned = wiped;
            }
            Err(error) => crate::run::note_after(format!("seat cap: {error}")),
        }
    }
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
    present_ship(&bare, &seat, request)?;
    Ok(seat)
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

/// The shipped objects in the checkout: each `ref` entry made in the seat
/// clone, and last run's that this run did not ask for removed (the clone's
/// own config lists them, so the list lives and dies with the seat); each
/// `url` entry served from the store through the checkout's git config.
fn present_ship(bare: &Path, seat: &Path, request: &Request) -> Result<()> {
    let git = |args: &[&str]| repository::output(Command::new("git").current_dir(seat).args(args));
    let made = git(&["config", "--local", "--get-all", "ferry.ref"]).unwrap_or_default();
    for name in String::from_utf8(made)?.lines() {
        if !request.ship.iter().any(|s| s.name.as_deref() == Some(name)) {
            repository::ref_name(name)?;
            git(&["update-ref", "-d", name])?;
        }
    }
    let _ = git(&["config", "--local", "--unset-all", "ferry.ref"]);
    for shipped in &request.ship {
        git(&["cat-file", "-e", &format!("{}^{{commit}}", shipped.commit)])?;
        if let Some(name) = &shipped.name {
            git(&["update-ref", name, &shipped.commit])?;
            git(&["config", "--local", "--add", "ferry.ref", name])?;
        }
        if let Some(url) = &shipped.url {
            fs::create_dir_all(seat.join(".ferry"))?;
            repository::output(
                Command::new("git")
                    .args(["config", "--add", "--file"])
                    .arg(seat.join(".ferry/gitconfig"))
                    .arg(format!("url.file://{}/.insteadOf", bare.display()))
                    .arg(url),
            )?;
        }
    }
    Ok(())
}
