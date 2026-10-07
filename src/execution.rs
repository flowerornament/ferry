//! Locked real checkouts and recipe sessions shared by caller and host.
use crate::{
    config::{Hooks, Recipe, Refusal, identifier, preserve_path},
    receipt, repository,
};
use fs2::FileExt;
use nix::{
    sys::signal::{Signal, kill},
    unistd::{Pid, getsid, setsid},
};
use serde::{Deserialize, Serialize};
use std::os::unix::process::ExitStatusExt;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
        let sid = self.child.id();
        signal_session(sid, Signal::SIGTERM)?;
        let deadline = Instant::now() + Duration::from_secs(5);
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
            eprintln!("ferry: cleanup failed: {e}");
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
pub fn stopped() -> Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, flag.clone())?;
    }
    Ok(flag)
}
fn acquire(path: &Path, stop: &AtomicBool) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err("client lost while waiting for lock".into());
        }
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    }
}
pub fn prepare(
    root: &Path,
    request: &Request,
    stop: &AtomicBool,
) -> Result<(PathBuf, fs::File, Option<fs::File>)> {
    validate(request)?;
    fs::create_dir_all(root.join("locks"))?;
    let lease = acquire(&root.join("locks").join(&request.seat), stop)?;
    crate::seat::check(root, &request.seat, root.ends_with("local"))?;
    for entry in fs::read_dir(root.join("locks"))? {
        if entry?
            .file_name()
            .to_string_lossy()
            .starts_with("cleanup-pending-")
        {
            return Err("cleanup_pending: an earlier session has unconfirmed teardown".into());
        }
    }
    let exclusive = if request.recipe.requires.contains("exclusive") {
        Some(acquire(&root.join("locks/host-exclusive"), stop)?)
    } else {
        None
    };
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
        })
        .collect())
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

pub fn run(
    root: &Path,
    workdir: &Path,
    request: &Request,
    stop: &AtomicBool,
    json: bool,
) -> Result<(i32, bool)> {
    let outcome = run_session(root, workdir, request, stop, json);
    if let Some(hook) = &request.hooks.collect {
        let mut collect = request.clone();
        collect.id = receipt::run_id()?;
        collect.hooks.prepare = None;
        collect.hooks.collect = None;
        collect.args.clear();
        collect.recipe.command = vec![
            "env".into(),
            format!(
                "FERRY_EXIT_CODE={}",
                outcome.as_ref().map_or(125, |(code, _)| *code)
            ),
            workdir
                .join(hook)
                .to_str()
                .ok_or("non-UTF-8 collect path")?
                .to_owned(),
        ];
        collect.timeout_seconds = Some(60);
        // Collection has its own bounded session after recipe teardown, including
        // timeout or preparation failure. Never reinterpret the recipe verdict.
        let result = if crate::seat::path(root, &request.seat).exists() {
            Err("recipe teardown unconfirmed; collection not started".into())
        } else {
            run_session(root, workdir, &collect, &AtomicBool::new(false), json)
        };
        let collection = match result {
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
        fs::write(
            root.join("runs").join(&request.id).join("collection.json"),
            serde_json::to_vec(&collection)?,
        )?;
    }
    outcome
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

pub fn collection(root: &Path, request: &Request) -> Result<Option<Collection>> {
    match fs::read(root.join("runs").join(&request.id).join("collection.json")) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn run_session(
    root: &Path,
    workdir: &Path,
    request: &Request,
    stop: &AtomicBool,
    json: bool,
) -> Result<(i32, bool)> {
    let run_dir = root.join("runs").join(&request.id);
    fs::create_dir_all(&run_dir)?;
    let spec = run_dir.join("request.json");
    fs::write(&spec, serde_json::to_vec(request)?)?;
    let result_path = run_dir.join("result.json");
    let log = fs::File::create(run_dir.join("output.log"))?;
    crate::seat::reserve(root, request)?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("execute-session")
        .arg(&spec)
        .arg(&result_path)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1024);
    let stdout = child.stdout.take().ok_or("missing stdout")?;
    let stderr = child.stderr.take().ok_or("missing stderr")?;
    let mut owner = SessionOwner {
        child,
        cleaned: false,
        quarantine: root
            .join("locks")
            .join(format!("cleanup-pending-{}", request.id)),
        ownership: crate::seat::path(root, &request.seat),
    };
    for (stream, reader) in [
        ("stdout", Box::new(stdout) as Box<dyn std::io::Read + Send>),
        ("stderr", Box::new(stderr) as Box<dyn std::io::Read + Send>),
    ] {
        let sender = sender.clone();
        thread::spawn(move || {
            for line in std::io::BufReader::new(reader).split(b'\n') {
                match line {
                    Ok(line) => {
                        if sender.send((stream, line)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    drop(sender);
    let start = Instant::now();
    let mut log = log;
    let mut timeout = false;
    let code = loop {
        while let Ok((stream, line)) = receiver.try_recv() {
            log.write_all(&line)?;
            log.write_all(b"\n")?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"protocol": receipt::PROTOCOL,"event":"output","stream":stream,"text":String::from_utf8_lossy(&line)})
                );
            } else {
                let mut target: Box<dyn Write> = if stream == "stdout" {
                    Box::new(std::io::stdout())
                } else {
                    Box::new(std::io::stderr())
                };
                target.write_all(&line)?;
                target.write_all(b"\n")?;
            }
        }
        if stop.load(Ordering::SeqCst) {
            break 130;
        }
        if request
            .timeout_seconds
            .is_some_and(|t| start.elapsed() >= Duration::from_secs(t))
        {
            timeout = true;
            break 124;
        }
        if let Ok(bytes) = fs::read(&result_path) {
            break serde_json::from_slice::<i32>(&bytes)?;
        }
        if let Some(status) = owner.child.try_wait()? {
            return Err(format!("session owner exited without result: {status}").into());
        }
        thread::sleep(Duration::from_millis(20));
    };
    owner.cleanup()?;
    for (stream, line) in receiver.try_iter() {
        log.write_all(&line)?;
        log.write_all(b"\n")?;
        if json {
            println!(
                "{}",
                serde_json::json!({"protocol":1,"event":"output","stream":stream,"text":String::from_utf8_lossy(&line)})
            );
        }
    }
    Ok((code, timeout))
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
    let mut env = BTreeMap::from([
        ("FERRY_SEAT".to_owned(), request.seat.clone()),
        ("FERRY_COMMIT".to_owned(), request.commit.clone()),
        ("FERRY_WORKDIR".to_owned(), workdir.display().to_string()),
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
