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

#[test]
fn a_closed_caller_pipe_stops_the_run_and_still_writes_a_receipt() {
    let dir = fixture("while :; do echo tick; sleep 0.01; done", "");
    let started = Instant::now();
    let out = Command::new("sh")
        .current_dir(dir.path())
        .env_remove("GIT_DIR")
        .env_remove("FERRY_ID")
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .arg("-c")
        .arg(format!(
            "{} run probe --local | head -1",
            env!("CARGO_BIN_EXE_ferry")
        ))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "tick\n");
    assert!(started.elapsed() < Duration::from_secs(60));
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "lost", "{record}");
    assert!(!record["custody"].is_null(), "{record}");
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
