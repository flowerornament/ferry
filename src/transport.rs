//! SSH carries a typed request and heartbeat stream, never interpolated recipe code.
use crate::{
    config,
    execution::{self, Request},
    receipt::{self, Outcome, Receipt},
    repository,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::Ordering,
    thread,
    time::Duration,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn ssh_options() -> Result<Vec<String>> {
    use std::os::unix::fs::PermissionsExt;
    let directory =
        PathBuf::from(std::env::var_os("HOME").ok_or("HOME missing")?).join(".cache/ferry/ssh");
    fs::create_dir_all(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let socket = directory.join("%C");
    Ok(vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        "ControlPersist=60".into(),
        "-o".into(),
        format!(
            "ControlPath={}",
            socket.to_str().ok_or("non-UTF-8 SSH control path")?
        ),
    ])
}

fn ssh() -> Result<Command> {
    let mut command = Command::new("ssh");
    command.args(ssh_options()?);
    Ok(command)
}

/// Closing a failed SSH conversation ends heartbeats and triggers host cleanup.
struct Connection {
    child: std::process::Child,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
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

fn push(root: &Path, ssh: &str, store: &str, reference: &str) -> Result<()> {
    let ssh_command = std::iter::once("ssh".to_owned())
        .chain(ssh_options()?)
        .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    repository::output(
        Command::new("git")
            .arg("--git-dir")
            .arg(repository::git_dir(root)?)
            .env("GIT_SSH_COMMAND", ssh_command)
            .args(["push", "--no-verify"])
            .arg(format!("{ssh}:{store}"))
            .arg(reference),
    )?;
    Ok(())
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
            repository::output(ssh()?.arg(&host.ssh).arg(format!(
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
        repository::output(ssh()?.arg(&host.ssh).arg(command))?;
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
}
fn event(value: &serde_json::Value) {
    serde_json::to_writer(std::io::stdout(), value)
        .unwrap_or_else(|e| eprintln!("ferry: event write failed: {e}"));
    println!();
}
/// Reads, validates and reserves a request. Any error here is a refusal.
fn admit(input: &mut impl BufRead) -> Result<(Request, PathBuf, fs::File)> {
    let mut line = String::new();
    input
        .read_line(&mut line)
        .map_err(|e| format!("reading the request: {e}"))?;
    let request: Request =
        serde_json::from_str(&line).map_err(|e| format!("parsing the request: {e}"))?;
    execution::validate(&request).map_err(|e| format!("validating the request: {e}"))?;
    let root =
        PathBuf::from(std::env::var_os("HOME").ok_or("HOME missing")?).join(".local/state/ferry");
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
pub fn host_run() -> Result<i32> {
    let mut input = std::io::BufReader::new(std::io::stdin());
    // Everything before `accepted` is a refusal: nothing ran. Say so in the
    // protocol, so the caller records refused 125 rather than a lost run.
    let admitted = admit(&mut input);
    let (request, root, mut record) = match admitted {
        Ok(admitted) => admitted,
        Err(error) => {
            event(&serde_json::json!({"protocol":1,"event":"refused","reason":error.to_string()}));
            return Ok(125);
        }
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
    let mut result = HostResult {
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
    };
    event(&serde_json::json!({"protocol":1,"event":"accepted","id":request.id}));
    crate::custody::supervise()?;
    let executed: Result<()> = (|| {
        let (seat, lease, host_lease) =
            execution::prepare(&root, &request, &stop, &mut result.admission)?;
        event(
            &serde_json::json!({"protocol":1,"event":"prepared","commit":request.commit,"seat":request.seat}),
        );
        let executed = execution::run(&root, &seat, &request, &stop, true);
        result.collection = execution::collection(&root, &request)?;
        let (code, timeout) = executed?;
        result.exit_code = code;
        result.outcome = if stop.load(Ordering::SeqCst) {
            Outcome::Lost
        } else if timeout {
            Outcome::Timeout
        } else if code == 0 {
            Outcome::RanPass
        } else {
            Outcome::RanFail
        };
        if !request.recipe.artifacts.is_empty() {
            let output = root.join("runs").join(&request.id).join("artifacts.tar");
            match crate::artifacts::collect(&seat, &request.recipe.artifacts, &output) {
                Ok(()) => result.artifacts_sha256 = Some(receipt::file_digest(&output)?),
                Err(e) => result.artifacts_error = Some(e.to_string()),
            }
        }
        result.custody = Some(crate::custody::release(
            &root,
            &request,
            terminal(result.outcome),
        )?);
        result.lease = Some(host_lease.release());
        drop(lease);
        Ok(())
    })();
    if let Err(error) = executed {
        result.reason = Some(error.to_string());
        if stop.load(Ordering::SeqCst) {
            result.outcome = Outcome::Lost;
            result.exit_code = execution::interrupted_code();
        } else if execution::attempted(&root, &request) {
            result.outcome = Outcome::Lost;
            result.exit_code = 255;
        }
    }
    serde_json::to_writer(&mut record, &result)?;
    record.sync_all()?;
    event(&serde_json::json!({"protocol":1,"event":"host-result","result":result}));
    Ok(result.exit_code)
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
    let stop = execution::stopped()?;
    execution::interrupted()?;
    crate::custody::supervise()?;
    let (seat, lease, host_lease) =
        execution::prepare(&state, request, &stop, &mut record.admission)?;
    if json {
        event(&serde_json::json!({"protocol":1,"event":"prepared","commit":request.commit}));
    }
    let executed = execution::run(&state, &seat, request, &stop, json);
    record.collection = execution::collection(&state, request)?;
    let (code, timeout) = executed.inspect_err(|_| {
        if execution::attempted(&state, request) {
            record.outcome = Outcome::Lost;
            record.exit_code = 255;
        }
    })?;
    record.exit_code = code;
    record.outcome = if stop.load(Ordering::SeqCst) {
        Outcome::Lost
    } else if timeout {
        Outcome::Timeout
    } else if code == 0 {
        Outcome::RanPass
    } else {
        Outcome::RanFail
    };
    record.host_binary_sha256 = Some(record.caller_binary_sha256.clone());
    record.host_source_rev.clone_from(&record.caller_source_rev);
    record.host_exe.clone_from(&record.caller_exe);
    if !request.recipe.artifacts.is_empty() {
        let output = state.join("runs").join(&request.id).join("artifacts.tar");
        match crate::artifacts::collect(&seat, &request.recipe.artifacts, &output) {
            Ok(()) => {
                crate::artifacts::unpack(&output, &root.join(".ferry/runs").join(&request.id))?;
            }
            Err(e) => record.reason = Some(format!("artifact collection failed: {e}")),
        }
    }
    record.custody = Some(crate::custody::release(
        &state,
        request,
        terminal(record.outcome),
    )?);
    record.lease = Some(host_lease.release());
    drop(lease);
    Ok(())
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
        ssh()?
            .arg(&machine.ssh)
            .arg(format!("{} host-info", machine.binary)),
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
    repository::output(ssh()?.arg(&machine.ssh).arg("mkdir -p .local/state/ferry; test -d .local/state/ferry/repo.git || git init --bare .local/state/ferry/repo.git"))?;
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
    let child = ssh()?
        .arg(&machine.ssh)
        .arg(format!("{} host-run", machine.binary))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut connection = Connection {
        child,
        stop: stop.clone(),
    };
    let child = &mut connection.child;
    let mut stdin = child.stdin.take().ok_or("missing SSH stdin")?;
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
    let stdout = child.stdout.take().ok_or("missing SSH stdout")?;
    let stream = read_stream(stdout, json)?;
    stop.store(true, Ordering::SeqCst);
    heartbeat.join().map_err(|_| "heartbeat thread panicked")?;
    let status = child.wait()?;
    if let Some(result) = stream.result {
        accept_result(root, machine, request, record, &info, result)?;
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

/// Only these outcomes may leave a seat idle.
fn terminal(outcome: Outcome) -> bool {
    matches!(
        outcome,
        Outcome::RanPass | Outcome::RanFail | Outcome::Timeout
    )
}

/// What the host said on the protocol stream.
struct HostStream {
    result: Option<HostResult>,
    accepted: bool,
    refusal: Option<String>,
}

/// Relays recipe output and collects the host's protocol verdicts.
fn read_stream(stdout: impl std::io::Read, json: bool) -> Result<HostStream> {
    let mut stream = HostStream {
        result: None,
        accepted: false,
        refusal: None,
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
        if json {
            println!("{line}");
        } else if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
            println!("{text}");
        }
    }
    Ok(stream)
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
        record.reason = Some(format!("artifact collection failed: {error}"));
    }
    if let Some(expected) = result.artifacts_sha256 {
        let destination = root.join(".ferry/runs").join(&request.id);
        fs::create_dir_all(&destination)?;
        let archive = destination.join("artifacts.tar");
        let file = fs::File::create(&archive)?;
        let status = ssh()?
            .arg(&machine.ssh)
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
