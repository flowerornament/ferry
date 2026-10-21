//! Shared by the contract tests: one fixture, the fake transport, the
//! guards that stop what a control started, and the waits.
#![allow(dead_code, unused_imports)]
#[path = "../../src/limits.rs"]
pub mod limits;
pub use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::{Pid, getsid},
};
pub use sha2::{Digest, Sha256};
pub use std::fmt::Write as _;
pub use std::os::unix::{
    fs::PermissionsExt,
    process::{CommandExt, ExitStatusExt},
};
pub use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
pub use tempfile::TempDir;

/// Runs git in a test's own repository; never inherits another one's.
#[allow(dead_code)]
pub fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The receipt in a `--json` run's events.
#[allow(dead_code)]
pub fn receipt(out: &Output) -> serde_json::Value {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["event"] == "receipt")
        .map_or_else(
            || {
                panic!(
                    "no receipt: {} {}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                )
            },
            |value| value["receipt"].clone(),
        )
}

/// The ferry binary under test, in `dir`, with its own configuration.
#[allow(dead_code)]
pub fn ferry(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("FERRY_ID")
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"));
    command
}

/// When a control stops waiting for something to start or finish and calls
/// it a hang. The wait ends as soon as the thing happens, so a generous
/// budget costs nothing on a fast machine; it only bounds a hang, and keeps
/// a loaded builder (the Darwin Nix check) from failing a correct run.
/// 300 s, scaled by `FERRY_TEST_DEADLINE_SCALE` (for example 0.1 locally).
#[allow(dead_code)]
pub fn startup_deadline() -> Instant {
    Instant::now() + Duration::from_secs_f64(300.0 * scale())
}

/// Scheduling slack added to a promptness bound that the design derives
/// from its constants: what a loaded machine adds on top. 30 s, scaled by
/// `FERRY_TEST_DEADLINE_SCALE`. A hang still exceeds it.
#[allow(dead_code)]
pub fn slack() -> Duration {
    Duration::from_secs_f64(30.0 * scale())
}

#[allow(dead_code)]
fn scale() -> f64 {
    std::env::var("FERRY_TEST_DEADLINE_SCALE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|scale| *scale > 0.0)
        .unwrap_or(1.0)
}

/// A repository with `pass` and `fail` (both may run elsewhere) and a
/// `probe` recipe running `script`; `extra` comes before the recipes.
pub fn fixture(script: &str, extra: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init"]);
    git(d, &["config", "user.name", "Ferry Test"]);
    git(d, &["config", "user.email", "ferry@example.invalid"]);
    let recipes = toml::to_string(&serde_json::json!({"recipe": {
        "pass": {"command": ["sh", "-c", "echo committed"], "routable": true},
        "fail": {"command": ["sh", "-c", "echo refused; exit 125"], "routable": true},
        "probe": {"command": ["sh", "-c", script]},
    }}))
    .unwrap();
    fs::write(d.join("ferry.toml"), format!("version=1\n{extra}{recipes}")).unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-m", "fixture"]);
    dir
}

pub struct OwnedGroup(pub i32);

impl Drop for OwnedGroup {
    fn drop(&mut self) {
        if getsid(Some(Pid::from_raw(self.0))).is_ok_and(|sid| sid.as_raw() == self.0) {
            let _ = killpg(Pid::from_raw(self.0), Signal::SIGKILL);
        }
    }
}

/// A caller, and the session of the recipe whose pid it wrote to `member`:
/// both stopped if the control ends early.
pub struct OwnedCaller {
    pub child: Option<std::process::Child>,
    pub member: std::path::PathBuf,
}

impl Drop for OwnedCaller {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Startup assertions can fail before the session guard is constructed.
        let session = fs::read_to_string(&self.member)
            .ok()
            .and_then(|pid| pid.trim().parse().ok())
            .and_then(|pid| getsid(Some(Pid::from_raw(pid))).ok());
        if let Some(session) = session {
            drop(OwnedGroup(session.as_raw()));
        }
    }
}

/// The local route's one seat checkout (SPEC §9: `seats/<key>/checkout`).
pub fn only_checkout(repo: &Path) -> std::path::PathBuf {
    fs::read_dir(repo.join(".ferry/local/v1/seats"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("checkout")
}

pub fn run(root: &Path, args: &[&str]) -> Output {
    command(root).args(args).output().unwrap()
}

pub fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"));
    command
}

pub const SOURCE_REV: &str = match option_env!("FERRY_SOURCE_REV") {
    Some(rev) => rev,
    None => "unpinned",
};

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            write!(hex, "{byte:02x}").unwrap();
            hex
        })
}

/// A process a control started (through a recipe), killed by its exact pid
/// when the control ends; its parent is gone, so nothing reaps it here.
pub struct OwnedPid(pub i32);

impl Drop for OwnedPid {
    fn drop(&mut self) {
        let _ = kill(Pid::from_raw(self.0), Signal::SIGKILL);
    }
}

/// A member of a session whose owner may be gone, killed when the control
/// ends while it is still in that session.
pub struct OwnedMember {
    pub pid: i32,
    pub session: i32,
}

impl Drop for OwnedMember {
    fn drop(&mut self) {
        if getsid(Some(Pid::from_raw(self.pid))).is_ok_and(|sid| sid.as_raw() == self.session) {
            let _ = kill(Pid::from_raw(self.pid), Signal::SIGKILL);
        }
    }
}

/// An unsupervised session (ferry-8d6): runs `pass` locally with `script`
/// as its `sh -c` recipe, which must write its pid to `$DIR/recipe`, waits
/// for that, then SIGKILLs the run's supervisor. Returns the run id, the
/// owner's pid (the session's id) and the guards on the recipe and caller.
pub fn unsupervised(
    dir: &Path,
    script: &str,
    fail_at: &str,
    timeout: &str,
) -> (String, i32, i32, OwnedMember, OwnedCaller) {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let marker = dir.join("recipe");
    fs::write(
        dir.join("ferry.toml"),
        format!(
            "version=1\n[recipe.pass]\ncommand=['sh','-c','{}']\n",
            script.replace("$DIR", &dir.display().to_string())
        ),
    )
    .unwrap();
    git(dir, &["add", "ferry.toml"]);
    git(dir, &["commit", "-m", "unsupervised recipe"]);
    let caller_output = dir.join("caller-stdout");
    let child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("FERRY_FAIL_AT", fail_at)
        .args([
            "run",
            "pass",
            "--local",
            "--json",
            "--timeout-seconds",
            timeout,
        ])
        .stdout(fs::File::create(&caller_output).unwrap())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut caller = OwnedCaller {
        child: Some(child),
        member: marker.clone(),
    };
    let deadline = startup_deadline();
    while !marker.exists() {
        // A caller that ended (a refusal) or a startup past the bound: say
        // what the caller printed.
        let ended = caller.child.as_mut().unwrap().try_wait().unwrap().is_some();
        assert!(
            Instant::now() < deadline && !ended,
            "recipe did not start (caller ended: {ended}): {}",
            fs::read_to_string(&caller_output).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(10));
    }
    let recipe: i32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    let owner = getsid(Some(Pid::from_raw(recipe))).unwrap().as_raw();
    let guard = OwnedMember {
        pid: recipe,
        session: owner,
    };
    let submitted = fs::read_to_string(&caller_output)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["event"] == "submitted")
        .expect("the caller prints its submitted event");
    let supervisor = i32::try_from(submitted["pid"].as_i64().unwrap()).unwrap();
    kill(Pid::from_raw(supervisor), Signal::SIGKILL).unwrap();
    caller.child.as_mut().unwrap().wait().unwrap();
    let id = submitted["id"].as_str().unwrap().to_owned();
    (id, owner, recipe, guard, caller)
}

pub fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// Waits for every pid to be gone, within `bound` from `since`.
pub fn gone_within(
    pids: &[i32],
    since: std::time::Instant,
    bound: std::time::Duration,
    what: &str,
) {
    while pids.iter().any(|pid| alive(*pid)) {
        assert!(since.elapsed() < bound, "{what}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

pub fn interrupted_caller_kills_children_and_exits_128_plus_signal(
    signal: nix::sys::signal::Signal,
    code: i32,
) {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let dir = fixture("true", "");
    let marker = dir.path().join("child-pid");
    let program = format!(
        "import os,time\nos.setpgrp()\np={:?}\nopen(p+'.tmp','w').write(str(os.getpid()))\nos.rename(p+'.tmp',p)\ntime.sleep(60)",
        marker.to_str().unwrap()
    );
    let config =
        serde_json::json!({"version":1,"recipe":{"hold":{"command":["python3","-c",program]}}});
    fs::write(
        dir.path().join("ferry.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "hold"]);
    let child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .args(["run", "hold", "--local", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut caller = OwnedCaller {
        child: Some(child),
        member: marker.clone(),
    };
    let child = caller.child.as_mut().unwrap();
    let deadline = startup_deadline();
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(marker.exists(), "recipe child never reached rendezvous");
    let pid: i32 = fs::read_to_string(&marker).unwrap().parse().unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        signal,
    )
    .unwrap();
    let deadline = startup_deadline();
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        panic!("caller did not finish bounded cancellation");
    }
    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
        "sibling process-group child survived"
    );
    let output = caller.child.take().unwrap().wait_with_output().unwrap();
    // The caller asked the place to stop the run with its own signal.
    assert_eq!(output.status.code(), Some(code));
    assert_eq!(receipt(&output)["outcome"], "stopped");
    assert_eq!(receipt(&output)["exit_code"], code);
    assert!(receipt(&output)["stopped_by"]["why"].is_string());
}

pub fn with_adapter(root: &Path, script: &str) {
    fs::write(
        root.join("ferry.toml"),
        "version=1\n[catalog]\nadapter='catalog'\n",
    )
    .unwrap();
    fs::write(root.join("catalog"), script).unwrap();
    fs::set_permissions(root.join("catalog"), fs::Permissions::from_mode(0o755)).unwrap();
    git(root, &["add", "ferry.toml", "catalog"]);
    git(root, &["commit", "-m", "adapter"]);
}

/// A host reached through an SSH shim: `host` is its HOME, `binary` its
/// ferry path relative to that HOME.
pub fn remote_run(dir: &Path, binary: &str, args: &[&str]) -> Output {
    remote_command(dir, binary, args).output().unwrap()
}

pub fn remote_command(dir: &Path, binary: &str, args: &[&str]) -> Command {
    let host = dir.join("host");
    let tools = dir.join("transport-tools");
    fs::create_dir_all(host.join(".local/bin")).unwrap();
    fs::create_dir_all(&tools).unwrap();
    // By rename, never in place: macOS kills a process whose signed binary is
    // rewritten under it (seen as `signal: 9` on a later verb; ferry-xtx.15).
    let staged = host.join(".local/bin/.ferry.new");
    fs::copy(env!("CARGO_BIN_EXE_ferry"), &staged).unwrap();
    fs::rename(&staged, host.join(".local/bin/ferry")).unwrap();
    // FERRY_TEST_SLOW_PUSH names a marker: a git push touches it, then stalls.
    // A file `unreachable` on the host fails every follow, as a dead link;
    // a file `fail-record-once` fails the next `place record` of a final
    // record, once (a read while the run waits is narration, not the verdict);
    // FERRY_TEST_UNPIN: host-info answers without `pinned`, as a place
    // from before it;
    // FERRY_TEST_SLOW_INFO names a file: while it exists, host-info first
    // sleeps the seconds it holds, then answers;
    // FERRY_TEST_HANG_INFO names a file: host-info writes its pid there and
    // hangs, as a stalled SSH connect or jump host; FERRY_TEST_HOLD_PIPE
    // names a file: host-info leaves a descendant holding its pipes (its
    // pid written there) and exits at once. FERRY_TEST_HANG_PUSH names a
    // file: a push's ssh writes its pid there and hangs, a live link whose
    // git never reads (printing ssh diagnostics meanwhile, unless asked for
    // LogLevel=ERROR); FERRY_TEST_THROTTLE_PUSH: a push's ssh passes git's
    // data on at about 320 KB/s, so a large push takes a while;
    // FERRY_TEST_FAIL_PUSH: a push fails at once, saying a fatal line;
    // FERRY_TEST_PUSH_LEAVER names a file: a push's ssh leaves a process
    // holding its stderr (pid there); FERRY_TEST_HANG_LSREMOTE names a file:
    // an ls-remote's ssh writes its pid there and hangs; FERRY_TEST_TRICKLE_PUSH holds seconds: a push's ssh
    // first prints a progress byte every 0.2 s for that long, then pushes;
    // FERRY_TEST_HANG_INIT names a file: the store setup writes its pid
    // there and hangs.
    fs::write(tools.join("ssh"), "#!/bin/sh\nfor last do :; done\ncase \"$last\" in *git-receive-pack*) if [ -n \"$FERRY_TEST_FAIL_PUSH\" ]; then echo \"fatal: the place refused the push\" >&2; exit 1; fi; if [ -n \"$FERRY_TEST_PUSH_LEAVER\" ]; then sleep 120 & echo $! > \"$FERRY_TEST_PUSH_LEAVER\"; fi; if [ -n \"$FERRY_TEST_SLOW_PUSH\" ]; then touch \"$FERRY_TEST_SLOW_PUSH\"; sleep 3; fi; if [ -n \"$FERRY_TEST_HANG_PUSH\" ]; then echo $$ > \"$FERRY_TEST_HANG_PUSH\"; case \" $* \" in *LogLevel=ERROR*) ;; *) (n=0; while [ $n -lt 240 ]; do echo \"debug1: an ssh diagnostic\" >&2; sleep 0.5; n=$((n + 1)); done) & ;; esac; exec sleep 120; fi; if [ -n \"$FERRY_TEST_THROTTLE_PUSH\" ]; then cd \"$FERRY_TEST_HOST\" || exit 125; export HOME=\"$FERRY_TEST_HOST\"; t=$(mktemp); { while :; do dd bs=32768 count=1 of=\"$t\" 2>/dev/null; [ -s \"$t\" ] || break; cat \"$t\"; sleep 0.1; done; rm -f \"$t\"; } | sh -c \"$last\"; exit $?; fi; if [ -n \"$FERRY_TEST_TRICKLE_PUSH\" ]; then n=0; while [ $n -lt $((FERRY_TEST_TRICKLE_PUSH * 5)) ]; do printf . >&2; sleep 0.2; n=$((n + 1)); done; fi;; *git-upload-pack*) if [ -n \"$FERRY_TEST_HANG_LSREMOTE\" ]; then echo $$ > \"$FERRY_TEST_HANG_LSREMOTE\"; exec sleep 120; fi;; *\"git init\"*) if [ -n \"$FERRY_TEST_HANG_INIT\" ]; then echo $$ > \"$FERRY_TEST_HANG_INIT\"; exec sleep 30; fi;; *\" place \"*\"follow\"*) if [ -e \"$FERRY_TEST_HOST/unreachable\" ]; then exit 255; fi;; *\" place \"*\"record\"*) if [ -e \"$FERRY_TEST_HOST/fail-record-once\" ] && [ -e \"$FERRY_TEST_HOST/.local/state/ferry/v1/records/$(printf %s \"${last##* }\" | tr -d \"'\").json\" ]; then rm -f \"$FERRY_TEST_HOST/fail-record-once\"; exit 255; fi;; *host-info*) if [ -n \"$FERRY_TEST_UNPIN\" ]; then cd \"$FERRY_TEST_HOST\" || exit 125; export HOME=\"$FERRY_TEST_HOST\"; sh -c \"$last\" | sed 's/\"pinned\",//; s/,\"pinned\"//'; exit 0; fi; if [ -n \"$FERRY_TEST_SLOW_INFO\" ] && [ -e \"$FERRY_TEST_SLOW_INFO\" ]; then sleep \"$(cat \"$FERRY_TEST_SLOW_INFO\")\"; fi; if [ -n \"$FERRY_TEST_HANG_INFO\" ]; then echo $$ > \"$FERRY_TEST_HANG_INFO\"; exec sleep 600; fi; if [ -n \"$FERRY_TEST_HOLD_PIPE\" ]; then sleep 600 & echo $! > \"$FERRY_TEST_HOLD_PIPE\"; exit 255; fi;; esac\ncd \"$FERRY_TEST_HOST\" || exit 125\nexport HOME=\"$FERRY_TEST_HOST\"\nexec sh -c \"$last\"\n").unwrap();
    fs::set_permissions(tools.join("ssh"), fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.join("config/ferry");
    fs::create_dir_all(&config).unwrap();
    // Top-level keys a control sets (`machines-top.toml`) come first.
    let top = fs::read_to_string(dir.join("machines-top.toml")).unwrap_or_default();
    fs::write(
        config.join("machines.toml"),
        format!(
            "{top}[host.test-host]\nssh='test-host'\nbinary='{binary}'\ntriple='{}-{}'\ncores=1\nfeatures=[]\n",
            std::env::consts::ARCH,
            if cfg!(target_os = "macos") {
                "apple-darwin"
            } else {
                "unknown-linux-gnu"
            }
        ),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("HOME", dir)
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("GIT_SSH", tools.join("ssh"))
        .env("FERRY_TEST_HOST", &host)
        .args(args);
    command
}

/// Runs of the `hold` recipe that a lease test spawned. Dropping it releases
/// every run and then stops exactly the callers it spawned, so a failed
/// assertion never leaves a run waiting for a lock or a release file.
pub struct HoldRuns {
    pub log: std::path::PathBuf,
    pub pids: Vec<i32>,
}

impl HoldRuns {
    pub fn spawn(&mut self, dir: &Path, name: &str, exclusive: bool) -> std::process::Child {
        let mut args = vec!["run", "hold", "--local", "--json", "--seat", name];
        if exclusive {
            args.push("--exclusive");
        }
        args.extend(["--", name]);
        let child = Command::new(env!("CARGO_BIN_EXE_ferry"))
            .current_dir(dir)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .args(args)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        self.pids.push(i32::try_from(child.id()).unwrap());
        child
    }
    pub fn release(&self, name: &str) {
        fs::write(format!("{}.{name}", self.log.display()), "").unwrap();
    }
    pub fn logged(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for HoldRuns {
    fn drop(&mut self) {
        for name in ["first", "alone", "later"] {
            let _ = fs::write(format!("{}.{name}", self.log.display()), "");
        }
        for pid in &self.pids {
            let _ = kill(Pid::from_raw(*pid), Signal::SIGTERM);
        }
    }
}

/// The three lease receipts, in the order first, alone, later.
pub fn assert_lease_windows(
    receipts: &[serde_json::Value],
    held_for: std::time::Duration,
    dir: &Path,
) {
    assert_eq!(receipts[0]["lease"]["mode"], "shared");
    assert_eq!(receipts[1]["lease"]["mode"], "exclusive");
    // It held the gate and waited at least from then until A was released.
    assert!(
        u128::from(receipts[1]["lease"]["wait_ms"].as_u64().unwrap()) >= held_for.as_millis(),
        "{} < {held_for:?}",
        receipts[1]["lease"]["wait_ms"]
    );
    assert_eq!(receipts[2]["lease"]["mode"], "shared");
    // The release and acquire times order the windows the same way.
    let at = |i: usize, field: &str| receipts[i]["lease"][field].as_u64().unwrap();
    assert!(at(0, "released_ms") <= at(1, "acquired_ms"));
    assert!(at(1, "released_ms") <= at(2, "acquired_ms"));
    // Older binaries used this name for their exclusive lock.
    assert!(dir.join(".ferry/local/v1/locks/host").exists());
    assert!(
        receipts[1]["requirements"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("exclusive"))
    );
}

pub fn with_host_policy(dir: &Path, policy: &str) {
    let config = dir.join("config/ferry");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("host.toml"), policy).unwrap();
}

/// Submits `request` to `ferry place --state <state> submit`; its exit code
/// and reply.
pub fn submit(state: &Path, request: &serde_json::Value) -> (Option<i32>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("place")
        .arg("--state")
        .arg(state)
        .arg("submit")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        child.stdin.as_mut().unwrap(),
        format!("{request}\n").as_bytes(),
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// A second workspace of the fixture's repository, at `dir/<name>`.
pub fn workspace(dir: &Path, name: &str) -> std::path::PathBuf {
    git(dir, &["clone", "--quiet", ".", name]);
    dir.join(name)
}

/// The caller's seat book, one JSON value per line.
pub fn seats_book(dir: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(dir.join("state/ferry/seats.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The key this caller recorded for its seat in `repo`.
pub fn key_of(dir: &Path, repo: &Path) -> String {
    let repo = repo.canonicalize().unwrap().display().to_string();
    seats_book(dir)
        .iter()
        .find(|line| line["repo_path"] == repo.as_str())
        .and_then(|line| line["key"].as_str())
        .expect("the seat was recorded")
        .to_owned()
}

pub fn remote_pass(dir: &Path, repo: &Path) -> Output {
    remote_command(
        dir,
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .current_dir(repo)
    .output()
    .unwrap()
}

pub fn prune(dir: &Path, binary: &str) -> (Option<i32>, String) {
    let out = remote_command(dir, binary, &["seats", "prune"])
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Makes `key` look last used `ago` ms back: on the caller's book (every
/// line of that key) and, unless `book_only`, on the place's `used` record.
pub fn age(dir: &Path, key: &str, ago: u128, book_only: bool) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let book = dir.join("state/ferry/seats.jsonl");
    let lines: Vec<String> = fs::read_to_string(&book)
        .unwrap()
        .lines()
        .map(|line| {
            let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
            if value["key"] == key {
                value["last_used_ms"] = serde_json::json!(now - ago);
            }
            value.to_string()
        })
        .collect();
    fs::write(&book, lines.join("\n") + "\n").unwrap();
    if !book_only {
        let used = dir
            .join("host/.local/state/ferry/v1/seats")
            .join(key)
            .join("used");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&used).unwrap()).unwrap();
        value["ms"] = serde_json::json!(now - ago);
        fs::write(&used, value.to_string()).unwrap();
    }
}

pub fn prune_with(dir: &Path, args: &[&str]) -> (Option<i32>, String) {
    let mut all = vec!["seats", "prune"];
    all.extend_from_slice(args);
    let out = remote_command(dir, ".local/bin/ferry", &all)
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

pub const DAY_MS: u128 = 86_400_000;

/// Waits for run `id`'s `--once` retire to be logged by its supervisor,
/// and returns that line.
pub fn once_logged(dir: &Path, id: &str) -> String {
    let log = dir
        .join("host/.local/state/ferry/v1/runs")
        .join(id)
        .join("supervisor.log");
    let until = startup_deadline();
    loop {
        if let Some(line) = fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .find(|line| line.starts_with("ferry: --once retire"))
        {
            return line.to_owned();
        }
        assert!(
            std::time::Instant::now() < until,
            "the --once retire never ran"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The host record of run `id` on the test place.
pub fn host_record(dir: &Path, id: &str) -> serde_json::Value {
    let path = dir
        .join("host/.local/state/ferry/v1/records")
        .join(format!("{id}.json"));
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// A dependency repository at `dir/<name>` with one commit; its hash.
pub fn dependency(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    fs::create_dir_all(&path).unwrap();
    git(&path, &["init", "-q"]);
    git(&path, &["config", "user.name", "Ferry Test"]);
    git(&path, &["config", "user.email", "ferry@example.invalid"]);
    fs::write(path.join("value"), name).unwrap();
    git(&path, &["add", "value"]);
    git(&path, &["commit", "-qm", name]);
    let out = Command::new("git")
        .current_dir(&path)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// ferry-8wq: the local place's state root.
pub fn local_root(dir: &Path) -> std::path::PathBuf {
    dir.join(".ferry/local/v1")
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

pub const HISTORY_DAY: &str =
    "version = 1\nmin_free_bytes = 1\ndelete_run_history_after_days = 1\n";

/// Plants run `id`'s history on the place at `root`, submitted `ago` ms
/// before now: its folder (a log and a nested file) and its record, final
/// or (`reserved`) a reservation, with `extra` fields over the defaults.
/// Returns the folder.
pub fn plant(
    root: &Path,
    id: &str,
    ago: u128,
    reserved: bool,
    extra: &serde_json::Value,
) -> std::path::PathBuf {
    let folder = root.join("runs").join(id);
    fs::create_dir_all(folder.join("artifacts/deep")).unwrap();
    fs::write(folder.join("output.log"), "old output\n").unwrap();
    fs::write(folder.join("artifacts/deep/file"), "old artifact\n").unwrap();
    let mut record = serde_json::json!({
        "state": if reserved { "reserved" } else { "final" },
        "id": id,
        "token": "t",
        "commit": "0000000000000000000000000000000000000000",
        "seat": "planted",
        "binary_sha256": "b",
        "supervisor": {"pid": 1, "start": null},
        "boot_id": null,
        "submitted_ms": now_ms() - ago,
    });
    if !reserved {
        record["outcome"] = "ran-pass".into();
        record["exit_code"] = 0.into();
    }
    for (key, value) in extra.as_object().unwrap() {
        record[key] = value.clone();
    }
    let name = if reserved {
        format!("{id}.reserved.json")
    } else {
        format!("{id}.json")
    };
    fs::create_dir_all(root.join("records")).unwrap();
    fs::write(root.join("records").join(name), record.to_string()).unwrap();
    folder
}

/// Sets `path`'s modification time to `ago` ms before now.
pub fn age_file(path: &Path, ago: u128) {
    let seconds = (now_ms() - ago) / 1000;
    let set = Command::new("python3")
        .args([
            "-c",
            "import os,sys; t=int(sys.argv[2]); os.utime(sys.argv[1], (t, t))",
            path.to_str().unwrap(),
            &seconds.to_string(),
        ])
        .status()
        .unwrap();
    assert!(set.success());
}

/// Waits for run `id`'s history pass to be logged by its supervisor, and
/// returns that line.
pub fn history_logged(root: &Path, id: &str) -> String {
    let log = root.join("runs").join(id).join("supervisor.log");
    let until = startup_deadline();
    loop {
        if let Some(line) = fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .find(|line| line.starts_with("ferry: run history:"))
        {
            return line.to_owned();
        }
        assert!(
            std::time::Instant::now() < until,
            "the history pass never ran"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Waits until run `id`'s supervisor has exited, so whatever it would do
/// after its record is done.
pub fn supervisor_exited(root: &Path, id: &str) {
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(format!("records/{id}.json"))).unwrap())
            .unwrap();
    let pid = i32::try_from(record["supervisor"]["pid"].as_u64().unwrap()).unwrap();
    let until = startup_deadline();
    while alive(pid) {
        assert!(std::time::Instant::now() < until, "the supervisor stayed");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The account run `id`'s history pass wrote, if it wrote one.
pub fn history_account(root: &Path, id: &str) -> Option<serde_json::Value> {
    let suffix = format!("-{id}.json");
    fs::read_dir(root.join("records"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("history-") && name.ends_with(&suffix))
        .map(|name| {
            serde_json::from_slice(&fs::read(root.join("records").join(name)).unwrap()).unwrap()
        })
}

/// The history passes from run `id`'s on: while the last one stopped at
/// its budget (a loaded host), the next run's pass goes on, as SPEC §9
/// promises. Each pass's run and line, and the runs their accounts deleted
/// in all.
pub fn settled(dir: &Path, root: &Path, id: &str) -> (Vec<(String, String)>, u64) {
    let mut passes = vec![(id.to_owned(), history_logged(root, id))];
    while passes.last().unwrap().1.contains("stopped at its") {
        assert!(passes.len() < 20, "{passes:?}");
        let out = run(dir, &["run", "pass", "--local", "--json"]);
        let next = receipt(&out)["id"].as_str().unwrap().to_owned();
        let line = history_logged(root, &next);
        passes.push((next, line));
    }
    let deleted = passes
        .iter()
        .filter_map(|(id, _)| history_account(root, id))
        .map(|account| account["deleted"].as_u64().unwrap())
        .sum();
    (passes, deleted)
}

/// How many entries a pass's line says it read.
pub fn read_of(line: &str) -> u64 {
    let (_, after) = line.split_once("read ").unwrap();
    after.split(',').next().unwrap().parse().unwrap()
}

/// A pid that was a process of this test and is gone.
pub fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Plants the records of `records/` that are not a run's, for item 22: a
/// two-day-old and a fresh retirement record, one without `retired_ms`, a
/// two-day-old pass's account, and a two-day-old and a fresh temporary file
/// of a crashed write. Returns the old ones' time.
pub fn plant_other_records(records: &Path) -> u128 {
    let then = now_ms() - 2 * DAY_MS;
    fs::write(
        records.join("retired-oldkey.json"),
        serde_json::json!({"key": "oldkey", "retired_ms": then}).to_string(),
    )
    .unwrap();
    fs::write(
        records.join("retired-newkey.json"),
        serde_json::json!({"key": "newkey", "retired_ms": now_ms()}).to_string(),
    )
    .unwrap();
    fs::write(
        records.join("retired-ageless.json"),
        serde_json::json!({"key": "ageless"}).to_string(),
    )
    .unwrap();
    fs::write(
        records.join(format!("history-{then}-earlier.json")),
        serde_json::json!({"ms": then, "run": "earlier", "deleted": 1, "older_than_days": 1})
            .to_string(),
    )
    .unwrap();
    let crashed = records.join(".old-final.json.1.0.tmp");
    fs::write(&crashed, "half a record").unwrap();
    age_file(&crashed, 2 * DAY_MS);
    let fresh_tmp = records.join(".recent-final.json.1.0.tmp");
    fs::write(&fresh_tmp, "half a record").unwrap();
    then
}

pub fn host_record_at(root: &Path, id: &str) -> serde_json::Value {
    serde_json::from_slice(&fs::read(root.join(format!("records/{id}.json"))).unwrap()).unwrap()
}

/// This process's start time as the place records it on Linux (clock ticks
/// since boot); `None` elsewhere.
pub fn own_start() -> Option<u64> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// The platform `remote_command`'s `machines.toml` declares for the test host.
pub fn test_platform() -> String {
    format!(
        "{}-{}",
        std::env::consts::ARCH,
        if cfg!(target_os = "macos") {
            "apple-darwin"
        } else {
            "unknown-linux-gnu"
        }
    )
}

/// Replaces the caller's `machines.toml` (after `remote_command` wrote its own).
pub fn with_machines(dir: &Path, text: &str) {
    fs::create_dir_all(dir.join("config/ferry")).unwrap();
    fs::write(dir.join("config/ferry/machines.toml"), text).unwrap();
}

/// Commits `ferry.toml` with `text`.
pub fn with_project(dir: &Path, text: &str) {
    fs::write(dir.join("ferry.toml"), text).unwrap();
    // Kept build directories are ignored ones.
    fs::write(dir.join(".gitignore"), "_build/\n").unwrap();
    git(dir, &["add", "ferry.toml", ".gitignore"]);
    git(dir, &["commit", "-m", "ferry.toml"]);
}

/// A copy of this build that is another binary (another digest) and runs
/// the same: on macOS re-signed under another identifier (an appended byte
/// would break its signature), elsewhere one byte appended. False where
/// macOS's `codesign` cannot run (the Darwin Nix sandbox).
pub fn another_build(to: &Path) -> bool {
    fs::copy(env!("CARGO_BIN_EXE_ferry"), to).unwrap();
    if cfg!(target_os = "macos") {
        let Ok(signed) = Command::new("/usr/bin/codesign")
            .args(["-f", "-s", "-", "-i", "dev.ferry.flipped"])
            .arg(to)
            .output()
        else {
            return false;
        };
        assert!(signed.status.success(), "{signed:?}");
    } else {
        std::io::Write::write_all(
            &mut fs::OpenOptions::new().append(true).open(to).unwrap(),
            b"\0",
        )
        .unwrap();
    }
    assert_ne!(
        sha256(&fs::read(to).unwrap()),
        sha256(&fs::read(env!("CARGO_BIN_EXE_ferry")).unwrap())
    );
    true
}

/// A process a control started, recorded by pid; stopped by that exact pid.
pub struct Owned(pub PathBuf);

impl Owned {
    pub fn pid(&self) -> Option<i32> {
        fs::read_to_string(&self.0).ok()?.trim().parse().ok()
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(pid) = self.pid() {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

/// A repository with a collect hook (`collect`, a shell script) and a
/// `probe` recipe running `command`.
pub fn with_collect(collect: &str, command: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init"]);
    git(d, &["config", "user.name", "Ferry Test"]);
    git(d, &["config", "user.email", "ferry@example.invalid"]);
    fs::write(d.join("collect"), format!("#!/bin/sh\n{collect}\n")).unwrap();
    fs::set_permissions(d.join("collect"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        d.join("ferry.toml"),
        format!("version=1\n[hooks]\ncollect='collect'\n[recipe.probe]\ncommand=['sh','-c','{command}']\n"),
    )
    .unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-m", "fixture"]);
    dir
}

/// Runs a caller to its end like `output()`, but bounded: a caller that
/// never ends fails the test instead of hanging it.
pub fn bounded_output(command: &mut Command) -> Output {
    let scratch = tempfile::tempdir().unwrap();
    let (stdout, stderr) = (scratch.path().join("out"), scratch.path().join("err"));
    let mut caller = Caller(
        command
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    wait_until("the caller ended", || {
        caller.0.try_wait().unwrap().is_some()
    });
    Output {
        status: caller.0.wait().unwrap(),
        stdout: fs::read(&stdout).unwrap(),
        stderr: fs::read(&stderr).unwrap(),
    }
}

pub fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = startup_deadline();
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// From the moment the run took its host lease (in the host record) to now,
/// when the test sees the complete record.
pub fn since_acquired(host: &serde_json::Value) -> Duration {
    let acquired = u128::from(host["lease"]["acquired_ms"].as_u64().unwrap());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    Duration::from_millis(u64::try_from(now.saturating_sub(acquired)).unwrap())
}

/// The only stored receipt or host record in `dir`, once it is complete.
pub fn only_record(dir: &Path) -> serde_json::Value {
    let mut found = None;
    wait_until(&format!("a complete record in {}", dir.display()), || {
        found = fs::read_dir(dir).ok().and_then(|entries| {
            entries
                .flatten()
                // A final record: not a reservation, not a temporary file.
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    !name.starts_with('.') && !name.ends_with(".reserved.json")
                })
                .filter_map(|e| fs::read(e.path()).ok())
                .find_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                // A receipt is complete once its run is no longer under way.
                .filter(|record| record["state"] != "submitted")
        });
        found.is_some()
    });
    found.unwrap()
}

/// The one receipt in the repository, whatever its state.
pub fn only_receipt_file(dir: &Path) -> serde_json::Value {
    let entry = fs::read_dir(dir.join(".ferry/receipts"))
        .unwrap()
        .flatten()
        .find(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .unwrap();
    serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap()
}

/// The names in a run-records directory, sorted.
pub fn record_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir.join(".ferry/local/v1/records"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A run's output log.
pub fn output_log(dir: &Path, id: &str) -> Vec<u8> {
    fs::read(dir.join(format!(".ferry/local/v1/runs/{id}/output.log"))).unwrap()
}

/// The supervisor's pid, from the caller's `submitted` event in `events`.
pub fn submitted_pid(events: &Path) -> Option<i32> {
    fs::read_to_string(events)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["event"] == "submitted")
        .and_then(|event| event["pid"].as_i64())
        .and_then(|pid| i32::try_from(pid).ok())
}

/// A process that holds a pipe and never reads it, for a stalled consumer.
pub struct Stalled(pub std::process::Child);

impl Stalled {
    /// A consumer that holds the pipe unread for `secs`.
    pub fn lasting(secs: u32) -> (Self, Stdio) {
        let mut child = Command::new("sleep")
            .arg(secs.to_string())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let pipe = Stdio::from(child.stdin.take().unwrap());
        (Self(child), pipe)
    }
    pub fn alive(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }
}

impl Drop for Stalled {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A caller a control spawned: stopped (SIGINT, so it asks the place to
/// stop its run, then KILL) if the control ends early.
pub struct Caller(pub std::process::Child);

impl Drop for Caller {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = kill(
                Pid::from_raw(i32::try_from(self.0.id()).unwrap()),
                Signal::SIGINT,
            );
            let deadline = Instant::now() + Duration::from_secs(30);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(100));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// The local seat lock of the default seat, once a run has created it.
pub fn seat_lock(dir: &Path) -> PathBuf {
    fs::read_dir(dir.join(".ferry/local/v1/locks"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with("-default"))
        .unwrap()
}

/// A recipe line that leaves a setsid python running, holding the run's
/// inherited seat lock, and records its pid. It outlives any loaded test
/// (600 s); the test's `Owned` guard stops it by that pid.
pub fn leaving(pidfile: &Path) -> String {
    format!(
        "python3 -c 'import os,time\nos.setsid()\nopen(\"{p}.tmp\",\"w\").write(str(os.getpid()))\nos.rename(\"{p}.tmp\",\"{p}\")\ntime.sleep(600)' </dev/null >/dev/null 2>&1 & while [ ! -e {p} ]; do sleep 0.05; done",
        p = pidfile.display()
    )
}

/// An exclusive lock held by a helper process, released when dropped. One
/// held by the test process itself is shared, until exec, with every child
/// a parallel test spawns (Linux), so its release cannot be timed: such a
/// child can still hold it a moment after `drop`.
pub struct Holder(pub std::process::Child);

impl Holder {
    pub fn exclusive(path: &Path) -> Self {
        let mut child = Command::new("python3")
            .args([
                "-c",
                "import fcntl,sys,time\nf=open(sys.argv[1],'a+')\nfcntl.flock(f,fcntl.LOCK_EX)\nprint('held',flush=True)\ntime.sleep(600)",
            ])
            .arg(path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.as_mut().unwrap()),
            &mut line,
        )
        .unwrap();
        assert_eq!(line.trim(), "held");
        Self(child)
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The run in `runs` paused at `point` (its `<at>.paused` marker), as its
/// id and the marker's path.
pub fn paused_at(runs: &Path, at: &str) -> (String, PathBuf) {
    let mut found = None;
    wait_until(&format!("a run paused at {at}"), || {
        found = fs::read_dir(runs)
            .into_iter()
            .flatten()
            .flatten()
            .find_map(|e| {
                let marker = e.path().join(format!("{at}.paused"));
                marker
                    .exists()
                    .then(|| (e.file_name().to_string_lossy().into_owned(), marker))
            });
        found.is_some()
    });
    found.unwrap()
}

/// Whether the default seat's lock is held now, by an exclusive probe.
pub fn seat_held(dir: &Path) -> bool {
    let probe = fs::File::open(seat_lock(dir)).unwrap();
    fs2::FileExt::try_lock_exclusive(&probe)
        .is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)
}

/// A run waiting for a host lock this test holds, paused at the host
/// probe after its wait expired; returns the caller, the held lock file
/// and the pause marker.
pub fn paused_at_the_host_probe(dir: &Path) -> (Caller, Holder, PathBuf) {
    let locks = dir.join(".ferry/local/v1/locks");
    fs::create_dir_all(&locks).unwrap();
    let host = Holder::exclusive(&locks.join("host"));
    let caller = Caller(
        ferry(dir)
            .env("FERRY_FAIL_AT", "host-probe")
            .args(["run", "probe", "--local", "--json", "--wait-seconds", "1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let (_, paused) = paused_at(&dir.join(".ferry/local/v1/runs"), "probe");
    (caller, host, paused)
}

/// The receipt a paused caller ends with, once released.
pub fn released(mut caller: Caller, paused: &Path) -> (Option<i32>, serde_json::Value) {
    fs::write(paused.with_extension("go"), "").unwrap();
    let mut stdout = caller.0.stdout.take().unwrap();
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut stdout, &mut out).unwrap();
    let code = caller.0.wait().unwrap().code();
    let record = receipt(&Output {
        status: ExitStatus::from_raw(0),
        stdout: out,
        stderr: vec![],
    });
    (code, record)
}

/// A recipe with artifacts in a fresh repository: `extra` adds top-level
/// ferry.toml lines; the recipe `probe` runs `script` and declares `artifacts`.
pub fn artifact_fixture(script: &str, artifacts: &str, extra: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init"]);
    git(d, &["config", "user.name", "Ferry Test"]);
    git(d, &["config", "user.email", "ferry@example.invalid"]);
    let toml = format!(
        "version=1\n{extra}[recipe.probe]\ncommand=['sh','-c','{script}']\nartifacts={artifacts}\n"
    );
    fs::write(d.join("ferry.toml"), toml).unwrap();
    fs::write(d.join(".gitignore"), "out.txt\nkeep/\n").unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-m", "fixture"]);
    dir
}

/// A record's survivors as SPEC §7 allows them here: exact on Linux; on
/// macOS, which withholds its platform binaries' environment from the marker
/// pass, `unknown` (None) is always allowed, never a wrong list.
pub fn survivors(record: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    match &record["custody"]["survivors"] {
        serde_json::Value::Array(list) => Some(list.clone()),
        unknown if cfg!(target_os = "macos") && unknown == "unknown" => None,
        other => panic!("survivors {other} in {record}"),
    }
}

/// No survivor: `[]`, or on macOS `unknown` with no session member among
/// what the pass saw, so the session stop is still checked there.
pub fn assert_no_survivors(record: &serde_json::Value) {
    assert!(
        survivors(record).is_none_or(|list| list.is_empty()),
        "{record}"
    );
    let error = record["custody"]["error"].as_str().unwrap_or_default();
    assert!(!error.contains("session survivors"), "{record}");
}

/// A recipe or hook line that leaves a python in its own session (`setsid`,
/// re-parented once the line's shell exits), recording its pid. `closed`:
/// it closes every inherited fd first, so it holds no lock; `cleared`: it
/// starts under `env -i`, so it carries no marker.
pub fn escapee(pidfile: &Path, closed: bool, cleared: bool) -> String {
    let python = if cleared {
        "env -i \"$(command -v python3)\""
    } else {
        "python3"
    };
    let close = if closed {
        "os.closerange(3, 4096)\n"
    } else {
        ""
    };
    format!(
        "{python} -c 'import os,time\nos.setsid()\n{close}open(\"{p}.tmp\",\"w\").write(str(os.getpid()))\nos.rename(\"{p}.tmp\",\"{p}\")\ntime.sleep(600)' </dev/null >/dev/null 2>&1 & while [ ! -e {p} ]; do sleep 0.05; done",
        p = pidfile.display()
    )
}

/// Whether `pid` is gone: no such process, or a zombie (dead, not yet
/// reaped by its new parent; `kill(pid, 0)` still succeeds on one). Bench
/// once caught a zombie here (ferry-xtx.25: a test artifact).
pub fn gone(pid: i32) -> bool {
    if kill(Pid::from_raw(pid), None).is_err() {
        return true;
    }
    let state = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default();
    state.is_empty() || state.starts_with('Z')
}

/// Runs `ferry run pass --local` in a pty with `tostop`, its foreground,
/// while another run holds the host lease exclusively; `interrupt` sends
/// SIGINT to the caller after 5 s. Returns (exit code, seconds taken).
pub fn under_tostop(dir: &Path, interrupt: bool) -> (i32, f64) {
    let harness = r#"
import os, pty, select, signal, sys, termios, time
pid, fd = pty.fork()
if pid == 0:
    attrs = termios.tcgetattr(0)
    attrs[3] |= termios.TOSTOP
    termios.tcsetattr(0, termios.TCSANOW, attrs)
    os.execv(sys.argv[3], sys.argv[3:])
start = time.time()
sent = False
while True:
    if sys.argv[1] == "interrupt" and not sent and time.time() - start > 5:
        os.kill(pid, signal.SIGINT)
        sent = True
    # Read only what is ready, so the signal goes out on time.
    if select.select([fd], [], [], 0.05)[0]:
        try:
            os.read(fd, 65536)
        except OSError:
            pass
    done, status = os.waitpid(pid, os.WNOHANG)
    if done:
        print(os.waitstatus_to_exitcode(status), round(time.time() - start, 1))
        sys.exit(0)
    if time.time() - start > float(sys.argv[2]):
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        print(-1, sys.argv[2])
        sys.exit(0)
    time.sleep(0.05)
"#;
    let out = Command::new("python3")
        .current_dir(dir)
        .env_remove("FERRY_ID")
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .arg("-c")
        .arg(harness)
        .arg(if interrupt { "interrupt" } else { "wait" })
        // A hang, not a loaded machine, reaches this bound.
        .arg((startup_deadline() - Instant::now()).as_secs().to_string())
        .arg(env!("CARGO_BIN_EXE_ferry"))
        .args(["run", "pass", "--local", "--seat", "second"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut fields = text.split_whitespace();
    let code = fields.next().and_then(|c| c.parse().ok()).unwrap_or(-2);
    let took = fields.next().and_then(|t| t.parse().ok()).unwrap_or(0.0);
    (code, took)
}

/// The `ferry: ` lines a caller wrote on stderr.
pub fn said(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|line| line.starts_with("ferry: "))
        .map(str::to_owned)
        .collect()
}

/// A commit id cut to the 12 digits the caller prints.
pub fn short(commit: &str) -> &str {
    &commit[..12]
}

/// A repository whose `nest` recipe starts `ferry run pass --local --seat
/// inner` in `target` (default: this repository itself, whose place runs
/// it), and prints that run's code.
pub fn nesting(target: Option<&Path>) -> TempDir {
    let dir = fixture("true", "");
    let target = target.unwrap_or(dir.path());
    let recipes = toml::to_string(&serde_json::json!({"recipe": {
        "pass": {"command": ["sh", "-c", "echo committed"]},
        "nest": {"command": ["sh", "-c", format!(
            "cd '{}' && '{}' run pass --local --seat inner --wait-seconds 900; echo inner=$?",
            target.display(),
            env!("CARGO_BIN_EXE_ferry")
        )]},
    }}))
    .unwrap();
    fs::write(
        dir.path().join("ferry.toml"),
        format!("version=1\n{recipes}"),
    )
    .unwrap();
    git(dir.path(), &["commit", "-qam", "nest"]);
    dir
}
