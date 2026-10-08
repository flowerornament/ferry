//! Controls for the run's stream between caller and supervisor (S8, P1 C4):
//! after spawn, a lost caller, a closed pipe or a failed diagnostic never
//! skips custody release and never changes the verdict of work that ran.
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

/// A process a control started, recorded by pid; stopped by that exact pid.
struct Owned(PathBuf);
impl Owned {
    fn pid(&self) -> Option<i32> {
        fs::read_to_string(&self.0).ok()?.trim().parse().ok()
    }
    fn alive(&self) -> bool {
        self.pid()
            .is_some_and(|pid| kill(Pid::from_raw(pid), None).is_ok())
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(pid) = self.pid() {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

fn git(root: &Path, args: &[&str]) {
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

/// A repository whose `probe` recipe runs `script`, plus `pass`.
fn fixture(script: &str, extra: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init"]);
    git(d, &["config", "user.name", "Ferry Test"]);
    git(d, &["config", "user.email", "ferry@example.invalid"]);
    let recipes = toml::to_string(&serde_json::json!({"recipe": {
        "pass": {"command": ["sh", "-c", "echo committed"]},
        "probe": {"command": ["sh", "-c", script]},
    }}))
    .unwrap();
    fs::write(d.join("ferry.toml"), format!("version=1\n{extra}{recipes}")).unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-m", "fixture"]);
    dir
}

fn ferry(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("FERRY_ID")
        .env("XDG_CONFIG_HOME", dir.join("config"));
    command
}

fn receipt(out: &Output) -> serde_json::Value {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["event"] == "receipt")
        .map_or_else(
            || panic!("no receipt: {}", String::from_utf8_lossy(&out.stderr)),
            |value| value["receipt"].clone(),
        )
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// The only stored receipt or host record in `dir`, once it is complete.
fn only_record(dir: &Path) -> serde_json::Value {
    let mut found = None;
    wait_until(&format!("a complete record in {}", dir.display()), || {
        found = fs::read_dir(dir).ok().and_then(|entries| {
            entries
                .flatten()
                .filter_map(|e| fs::read(e.path()).ok())
                .find_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        });
        found.is_some()
    });
    found.unwrap()
}

/// A daemon the recipe starts: its own session, stdio closed, other
/// descriptors kept (so on Linux it holds the inherited locks).
fn daemon(pidfile: &Path) -> String {
    format!(
        "python3 -c 'import os,time; os.setsid(); open(\"{p}.tmp\",\"w\").write(str(os.getpid())); os.rename(\"{p}.tmp\",\"{p}\"); time.sleep(60)' </dev/null >/dev/null 2>&1 &",
        p = pidfile.display()
    )
}

#[test]
fn a_killed_caller_still_gets_a_complete_host_record_and_a_released_seat() {
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("daemon.pid");
    let owned = Owned(pidfile.clone());
    let dir = fixture(
        &format!(
            "{} while :; do echo tick; sleep 0.01; done",
            daemon(&pidfile)
        ),
        "",
    );
    let mut caller = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .stdout(fs::File::create(scratch.path().join("caller.out")).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the daemon started", || pidfile.exists());
    thread::sleep(Duration::from_millis(500));
    kill(
        Pid::from_raw(i32::try_from(caller.id()).unwrap()),
        Signal::SIGKILL,
    )
    .unwrap();
    caller.wait().unwrap();
    let record = only_record(&dir.path().join(".ferry/local/records"));
    assert_eq!(record["outcome"], "lost", "{record}");
    assert!(!record["custody"].is_null(), "{record}");
    if cfg!(target_os = "linux") {
        assert!(!owned.alive(), "the daemon was not stopped: {record}");
        assert!(
            !dir.path().join(".ferry/local/taint").exists()
                || fs::read_dir(dir.path().join(".ferry/local/taint"))
                    .unwrap()
                    .count()
                    == 0,
            "{record}"
        );
    }
    let next = ferry(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    assert_eq!(receipt(&next)["outcome"], "ran-pass", "{record}");
}

/// A process that holds a pipe and never reads it, for a stalled consumer.
struct Stalled(std::process::Child);
impl Stalled {
    fn new() -> (Self, Stdio) {
        let mut child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let pipe = Stdio::from(child.stdin.take().unwrap());
        (Self(child), pipe)
    }
}
impl Drop for Stalled {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_closed_caller_pipe_is_data_and_the_run_reaches_its_own_end() {
    let dir = fixture("while :; do echo tick; sleep 0.01; done", "");
    let out = Command::new("sh")
        .current_dir(dir.path())
        .env_remove("GIT_DIR")
        .env_remove("FERRY_ID")
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .arg("-c")
        .arg(format!(
            "{} run probe --local --timeout-seconds 3 | head -1",
            env!("CARGO_BIN_EXE_ferry")
        ))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "tick\n");
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "timeout", "{record}");
    assert_eq!(record["exit_code"], 124, "{record}");
    assert!(
        record["output_dropped"]["stdout"].as_u64().unwrap() > 0,
        "{record}"
    );
    let next = ferry(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    assert_eq!(receipt(&next)["outcome"], "ran-pass");
}

#[test]
fn a_stalled_consumer_never_holds_the_supervisor_past_the_timeout() {
    let dir = fixture("exec yes", "");
    let (stalled, pipe) = Stalled::new();
    let started = Instant::now();
    let mut caller = ferry(dir.path())
        .args(["run", "probe", "--local", "--timeout-seconds", "3"])
        .stdout(pipe)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The host record is the truth, written while the consumer still stalls.
    let host = only_record(&dir.path().join(".ferry/local/records"));
    let took = started.elapsed();
    assert_eq!(host["outcome"], "timeout", "{host}");
    assert_eq!(host["exit_code"], 124, "{host}");
    assert!(took < Duration::from_secs(30), "{took:?}");
    assert!(
        caller.try_wait().unwrap().is_none(),
        "the consumer was stalled"
    );
    drop(stalled);
    assert_eq!(caller.wait().unwrap().code(), Some(124));
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "timeout", "{record}");
}

#[test]
fn a_signal_stops_a_run_whose_consumer_stalls() {
    let dir = fixture("exec yes", "");
    let (stalled, pipe) = Stalled::new();
    let mut caller = ferry(dir.path())
        .args(["run", "probe", "--local", "--timeout-seconds", "60"])
        .stdout(pipe)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(1500));
    let signalled = Instant::now();
    kill(
        Pid::from_raw(i32::try_from(caller.id()).unwrap()),
        Signal::SIGINT,
    )
    .unwrap();
    let mut code = None;
    wait_until("the signalled caller ended", || {
        code = caller.try_wait().unwrap().map(|status| status.code());
        code.is_some()
    });
    assert!(signalled.elapsed() < Duration::from_secs(30));
    assert_eq!(code.flatten(), Some(130));
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "lost", "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains("SIGINT"),
        "{record}"
    );
    drop(stalled);
}

#[test]
fn a_closed_caller_stderr_is_dropped_and_counted_not_a_stop() {
    let dir = fixture(
        "i=0; while [ $i -lt 20 ]; do echo err >&2; i=$((i+1)); done; echo done",
        "",
    );
    let mut caller = ferry(dir.path())
        .args(["run", "probe", "--local"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(caller.stderr.take());
    let out = caller.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"done\n");
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(record["output_dropped"]["stderr"], 20, "{record}");
}

#[test]
fn a_host_that_cannot_write_still_delivers_its_verdict_through_the_record() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo hidden", "");
    let out = ferry(dir.path())
        .env("FERRY_FAIL_AT", "output-write")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains("host record"),
        "{record}"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("hidden"));
}

#[test]
fn a_panic_inside_the_run_still_reaches_custody_release() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo never", "");
    let out = ferry(dir.path())
        .env("FERRY_FAIL_AT", "run-panic")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert!(!record["custody"].is_null(), "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains("panicked"),
        "{record}"
    );
    assert_ne!(record["outcome"], "ran-pass", "{record}");
    let next = ferry(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    assert_eq!(receipt(&next)["outcome"], "ran-pass");
}

#[test]
fn injected_collection_and_artifact_failures_keep_the_verdict() {
    // The injection exists only in debug builds of ferry.
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init"]);
    git(d, &["config", "user.name", "Ferry Test"]);
    git(d, &["config", "user.email", "ferry@example.invalid"]);
    fs::write(d.join("collect"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(d.join("collect"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        d.join("ferry.toml"),
        "version=1\n[hooks]\ncollect='collect'\n[recipe.probe]\ncommand=['sh','-c','echo built > out.txt']\nartifacts=['out.txt']\n",
    )
    .unwrap();
    fs::write(d.join(".gitignore"), "out.txt\n").unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-m", "fixture"]);
    let out = ferry(d)
        .env("FERRY_FAIL_AT", "collection-write,artifact-digest")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    let collection_error = record["collection"]["error"].as_str().unwrap();
    assert!(collection_error.contains("injected"), "{record}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains("collection") && reason.contains("artifact collection failed"),
        "{record}"
    );
}

#[test]
fn output_bytes_that_are_not_utf8_arrive_exactly() {
    let dir = fixture("printf 'a\\377b\\n'; printf '\\376\\n' >&2", "");
    let out = ferry(dir.path())
        .args(["run", "probe", "--local"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"a\xffb\n");
    assert!(out.stderr.starts_with(b"\xfe\n"), "{:?}", out.stderr);
}
