//! Controls for the run's stream between caller and supervisor (S8, P1 C4):
//! after spawn, a lost caller, a closed pipe or a failed diagnostic never
//! skips custody release and never changes the verdict of work that ran.
mod common;
#[path = "../src/limits.rs"]
#[allow(dead_code)]
mod limits;
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt},
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
    let deadline = common::startup_deadline();
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// From the moment the run took its host lease (in the host record) to now,
/// when the test sees the complete record: the run, its delivery and the
/// record write, without the caller's startup (which a loaded machine
/// stretches, and which the design does not bound).
fn since_acquired(host: &serde_json::Value) -> Duration {
    let acquired = u128::from(host["lease"]["acquired_ms"].as_u64().unwrap());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    Duration::from_millis(u64::try_from(now.saturating_sub(acquired)).unwrap())
}

/// The only stored receipt or host record in `dir`, once it is complete.
fn only_record(dir: &Path) -> serde_json::Value {
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
        Self::lasting(60)
    }
    /// A consumer that holds the pipe unread for `secs`.
    fn lasting(secs: u32) -> (Self, Stdio) {
        let mut child = Command::new("sleep")
            .arg(secs.to_string())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let pipe = Stdio::from(child.stdin.take().unwrap());
        (Self(child), pipe)
    }
    fn alive(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }
}
/// A caller a control spawned: stopped (SIGINT, then KILL) if the control
/// ends early, so a failed assertion never leaves a run behind.
struct Caller(std::process::Child);
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

/// A recipe writing 16 KiB lines forever: the queue's byte bound, not its
/// line bound, is what fills first.
const WIDE_LINES: &str = "exec python3 -c 'import sys\nline = \"x\" * 16383 + \"\\n\"\nwhile True:\n    sys.stdout.write(line)'";

impl Drop for Stalled {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_closed_caller_pipe_is_data_and_the_run_reaches_its_own_end() {
    // Far more than a pipe's worth up front, so drops are certain.
    let dir = fixture("seq 1 100000; while :; do echo tick; sleep 0.01; done", "");
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
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1\n");
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "timeout", "{record}");
    assert_eq!(record["exit_code"], 124, "{record}");
    let host = only_record(&dir.path().join(".ferry/local/records"));
    let count = |value: &serde_json::Value| value.as_u64().unwrap_or(0);
    assert!(count(&record["output_dropped"]["stdout"]) > 0, "{record}");
    // Every line the host read was delivered or dropped, once.
    assert_eq!(
        count(&record["output_delivered"]["stdout"])
            + count(&record["output_dropped"]["stdout"])
            + count(&record["output_dropped"]["host"]),
        count(&host["output_lines"]),
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
    let (mut stalled, pipe) = Stalled::lasting(600);
    let mut caller = Caller(
        ferry(dir.path())
            .args(["run", "probe", "--local", "--timeout-seconds", "3"])
            .stdout(pipe)
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    // The host record is the truth, written while the consumer still stalls.
    let host = only_record(&dir.path().join(".ferry/local/records"));
    let took = since_acquired(&host);
    assert!(
        stalled.alive(),
        "the consumer stopped stalling before the record"
    );
    assert_eq!(host["outcome"], "timeout", "{host}");
    assert_eq!(host["exit_code"], 124, "{host}");
    // The design's bound from taking the lease to a complete record: the
    // 3 s timeout, session teardown, the delivery stall bound and the
    // drain; plus scheduling slack.
    let bound = Duration::from_secs(3)
        + limits::SESSION_CLEANUP
        + limits::STALL
        + limits::DRAIN
        + common::slack();
    assert!(took < bound, "{took:?} >= {bound:?}");
    assert!(
        caller.0.try_wait().unwrap().is_none(),
        "the caller waits for its consumer"
    );
    drop(stalled);
    assert_eq!(caller.0.wait().unwrap().code(), Some(124));
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "timeout", "{record}");
}

#[test]
fn a_signal_stops_a_run_whose_consumer_stalls() {
    let dir = fixture(WIDE_LINES, "");
    let (mut stalled, pipe) = Stalled::lasting(600);
    let mut caller = Caller(
        ferry(dir.path())
            .args(["run", "probe", "--local", "--timeout-seconds", "600"])
            .stdout(pipe)
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    // Signal only once the stall is real: the run's log has passed what the
    // queue and a pipe can hold, so output is backed up behind the consumer.
    let runs = dir.path().join(".ferry/local/runs");
    let past = u64::try_from(limits::MAX_QUEUE).unwrap() + (64 << 10);
    wait_until("output backed up behind the stalled consumer", || {
        fs::read_dir(&runs)
            .into_iter()
            .flatten()
            .flatten()
            .any(|run| fs::metadata(run.path().join("output.log")).is_ok_and(|m| m.len() > past))
    });
    let signalled = Instant::now();
    kill(
        Pid::from_raw(i32::try_from(caller.0.id()).unwrap()),
        Signal::SIGINT,
    )
    .unwrap();
    let mut code = None;
    wait_until("the signalled caller ended", || {
        code = caller.0.try_wait().unwrap().map(|status| status.code());
        code.is_some()
    });
    // The consumer never read: the caller ended on its own signal.
    assert!(stalled.alive(), "the consumer stopped stalling");
    let bound = limits::SESSION_CLEANUP + limits::DRAIN + limits::FINAL_EMIT + common::slack();
    assert!(
        signalled.elapsed() < bound,
        "{:?} >= {bound:?}",
        signalled.elapsed()
    );
    assert_eq!(code.flatten(), Some(130));
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "lost", "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains("SIGINT"),
        "{record}"
    );
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
fn a_host_whose_output_is_dead_still_sends_its_events() {
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
    // The output line was not delivered, and is counted once; the
    // host-result still arrived over the stream, not from the record.
    assert!(!String::from_utf8_lossy(&out.stdout).contains("hidden"));
    assert_eq!(record["output_dropped"]["host"], 1, "{record}");
    assert!(record["reason"].is_null(), "{record}");
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

#[test]
fn a_slow_consumer_gets_every_line_and_nothing_is_counted_dropped() {
    let dir = fixture("seq 1 40000", "");
    let consumer = dir.path().join("consumer.py");
    fs::write(
        &consumer,
        "import sys,time\nn=0\nok=True\nfor i,l in enumerate(sys.stdin):\n    n+=1\n    ok = ok and l == '%d\\n' % (i+1)\n    if i % 100 == 0: time.sleep(0.02)\nprint(n, ok)\n",
    )
    .unwrap();
    let out = Command::new("sh")
        .current_dir(dir.path())
        .env_remove("GIT_DIR")
        .env_remove("FERRY_ID")
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .arg("-c")
        .arg(format!(
            "{} run probe --local | python3 {}",
            env!("CARGO_BIN_EXE_ferry"),
            consumer.display()
        ))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "40000 True");
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert!(record["output_dropped"].is_null(), "{record}");
    assert_eq!(record["output_delivered"]["stdout"], 40000, "{record}");
}

#[test]
fn a_stalled_consumer_accounts_for_every_line() {
    let dir = fixture("seq 1 40000", "");
    let (stalled, pipe) = Stalled::new();
    let mut caller = ferry(dir.path())
        .args(["run", "probe", "--local"])
        .stdout(pipe)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Backpressure holds the recipe while the consumer stalls; then the
    // consumer goes, and every line must be delivered or counted.
    thread::sleep(Duration::from_secs(2));
    assert!(
        caller.try_wait().unwrap().is_none(),
        "the consumer was stalled"
    );
    drop(stalled);
    caller.wait().unwrap();
    let host = only_record(&dir.path().join(".ferry/local/records"));
    assert_eq!(host["outcome"], "ran-pass", "{host}");
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    let count = |value: &serde_json::Value| value.as_u64().unwrap_or(0);
    let host_dropped = count(&host["output_dropped"]);
    assert_eq!(
        count(&record["output_dropped"]["host"]),
        host_dropped,
        "{record}"
    );
    assert_eq!(
        host_dropped
            + count(&record["output_delivered"]["stdout"])
            + count(&record["output_dropped"]["stdout"]),
        40000,
        "{record}"
    );
}

#[test]
fn a_failed_teardown_after_the_verdict_is_a_note() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo done", "");
    let out = ferry(dir.path())
        .env("FERRY_FAIL_AT", "session-cleanup")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("session teardown"),
        "{record}"
    );
    // The unconfirmed teardown quarantines the host until an operator acts.
    let next = ferry(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    assert_eq!(receipt(&next)["outcome"], "refused");
    assert!(
        receipt(&next)["reason"]
            .as_str()
            .unwrap()
            .contains("cleanup_pending")
    );
}

#[test]
fn a_callers_signal_after_the_verdict_is_named_and_keeps_it() {
    let scratch = tempfile::tempdir().unwrap();
    let collecting = scratch.path().join("collecting");
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init"]);
    git(d, &["config", "user.name", "Ferry Test"]);
    git(d, &["config", "user.email", "ferry@example.invalid"]);
    fs::write(
        d.join("collect"),
        format!("#!/bin/sh\ntouch {}\nsleep 3\n", collecting.display()),
    )
    .unwrap();
    fs::set_permissions(d.join("collect"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        d.join("ferry.toml"),
        "version=1\n[hooks]\ncollect='collect'\n[recipe.probe]\ncommand=['sh','-c','echo done']\n",
    )
    .unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-m", "fixture"]);
    let caller = ferry(d)
        .args(["run", "probe", "--local", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the collect hook started", || collecting.exists());
    kill(
        Pid::from_raw(i32::try_from(caller.id()).unwrap()),
        Signal::SIGINT,
    )
    .unwrap();
    let out = caller.wait_with_output().unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("after the verdict: caller received SIGINT"),
        "{record}"
    );
}

#[test]
fn a_line_longer_than_the_cap_is_split_without_losing_bytes() {
    let dir = fixture(
        "head -c 3145728 /dev/zero | tr '\\0' a; echo; echo tail",
        "",
    );
    let out = ferry(dir.path())
        .args(["run", "probe", "--local"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let chunk = vec![b'a'; 1 << 20];
    let mut expected = Vec::new();
    for _ in 0..3 {
        expected.extend_from_slice(&chunk);
        expected.push(b'\n');
    }
    expected.extend_from_slice(b"tail\n");
    assert!(out.stdout == expected, "{} bytes", out.stdout.len());
}

/// A survivor the recipe leaves holding its stdout: `chatty` writes a line
/// a second; otherwise it is quiet.
fn pipe_holder(pidfile: &Path, chatty: bool) -> String {
    let body = if chatty {
        "while True:\n    print(\"chat\", flush=True)\n    time.sleep(0.0001)"
    } else {
        "time.sleep(120)"
    };
    format!(
        "python3 -c 'import os,time\nos.setsid()\nopen(\"{p}.tmp\",\"w\").write(str(os.getpid()))\nos.rename(\"{p}.tmp\",\"{p}\")\n{body}' & while [ ! -e {p} ]; do sleep 0.05; done; echo done",
        p = pidfile.display()
    )
}

#[test]
fn a_chatty_survivor_cannot_keep_the_run_past_the_cap() {
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("chatty.pid");
    let _owned = Owned(pidfile.clone());
    let dir = fixture(&pipe_holder(&pidfile, true), "");
    let started = Instant::now();
    // Debug builds keep the survivor alive on Linux too, so readers are
    // still reading when delivery ends at the cap.
    let stdout = scratch.path().join("caller.out");
    let mut caller = ferry(dir.path())
        .env("FERRY_FAIL_AT", "stop-survivors")
        .args(["run", "probe", "--local", "--json"])
        .stdout(fs::File::create(&stdout).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // A hang fails here instead of hanging the suite.
    let deadline = common::startup_deadline();
    while caller.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = caller.kill();
            let _ = caller.wait();
            panic!(
                "the run outlived the startup deadline: {:?}",
                started.elapsed()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
    let out = Output {
        status: caller.wait().unwrap(),
        stdout: fs::read(&stdout).unwrap(),
        stderr: vec![],
    };
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    // The count is final when taken: the log holds exactly those lines,
    // and each was delivered or dropped, once.
    let host = only_record(&dir.path().join(".ferry/local/records"));
    // From the lease to the complete record: teardown, delivery's cap and
    // the drain, plus scheduling slack.
    let took = since_acquired(&host);
    let bound = limits::SESSION_CLEANUP + limits::CAP + limits::DRAIN + common::slack();
    assert!(took < bound, "{took:?} >= {bound:?}");
    let id = record["id"].as_str().unwrap();
    let log = fs::read(
        dir.path()
            .join(format!(".ferry/local/runs/{id}/output.log")),
    )
    .unwrap();
    let logged = u64::try_from(log.split(|b| *b == b'\n').count() - 1).unwrap();
    let count = |value: &serde_json::Value| value.as_u64().unwrap_or(0);
    assert_eq!(logged, count(&host["output_lines"]), "{host}");
    assert_eq!(
        count(&record["output_delivered"]["stdout"])
            + count(&record["output_dropped"]["stdout"])
            + count(&record["output_dropped"]["host"]),
        logged,
        "{record}"
    );
}

#[test]
fn a_stop_during_delivery_ends_it_promptly() {
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("chatty.pid");
    let _owned = Owned(pidfile.clone());
    let dir = fixture(&pipe_holder(&pidfile, true), "");
    let caller = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the recipe finished", || pidfile.exists());
    thread::sleep(Duration::from_secs(2));
    let signalled = Instant::now();
    kill(
        Pid::from_raw(i32::try_from(caller.id()).unwrap()),
        Signal::SIGINT,
    )
    .unwrap();
    let out = caller.wait_with_output().unwrap();
    assert!(
        signalled.elapsed() < Duration::from_secs(20) + common::slack(),
        "{:?}",
        signalled.elapsed()
    );
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
}

#[test]
fn a_quiet_pipe_holder_does_not_suppress_the_host_result() {
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("quiet.pid");
    let _owned = Owned(pidfile.clone());
    let dir = fixture(&pipe_holder(&pidfile, false), "");
    let out = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    let reason = record["reason"].as_str().unwrap_or("");
    assert!(!reason.contains("did not arrive"), "{record}");
}

/// Resident memory of a pid in KiB, from ps.
fn rss_kib(pid: u32) -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// The pid of the caller's host-run child, from ps.
fn child_of(parent: u32) -> Option<u32> {
    let out = Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let pid: u32 = fields.next()?.parse().ok()?;
            (fields.next()?.parse::<u32>().ok()? == parent).then_some(pid)
        })
}

#[test]
fn queues_stay_within_their_byte_bound() {
    // Memory is read with ps, which the Darwin Nix sandbox cannot spawn;
    // this control runs on a Mac host and on Linux.
    if Command::new("ps").arg("-p").arg("1").output().is_err() {
        return;
    }
    let dir = fixture("head -c 52428800 /dev/zero", "");
    let (stalled, pipe) = Stalled::new();
    let mut caller = ferry(dir.path())
        .args(["run", "probe", "--local"])
        .stdout(pipe)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut host = None;
    wait_until("the host-run started", || {
        host = child_of(caller.id());
        host.is_some()
    });
    thread::sleep(Duration::from_secs(4));
    let (caller_kib, host_kib) = (rss_kib(caller.id()), rss_kib(host.unwrap()));
    drop(stalled);
    caller.wait().unwrap();
    // 8 MiB queue + a 1 MiB line in hand + the process itself.
    assert!(caller_kib < 64 * 1024, "caller {caller_kib} KiB");
    assert!(host_kib < 64 * 1024, "host-run {host_kib} KiB");
}

#[test]
fn a_taint_that_may_hold_the_host_refuses_before_any_wait() {
    let dir = fixture("echo ran", "");
    let taint = dir.path().join(".ferry/local/taint");
    fs::create_dir_all(&taint).unwrap();
    fs::write(
        taint.join("abc-held.json"),
        serde_json::json!({"id": "earlier", "reason": "descendants survived the session",
            "pids": [], "survivors": [],
            "locks": {"seat": "abc-held", "host": "exclusive", "gate": true}})
        .to_string(),
    )
    .unwrap();
    let started = Instant::now();
    let out = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["outcome"], "refused", "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("host lease is held"),
        "{record}"
    );
    // Nobody holds the lock here, so without the pre-check the run would
    // take the lease and pass: refused, with the taint's reason, is the
    // pre-check's evidence. (A refusal never records a lease.)
    assert!(
        Instant::now() < started + Duration::from_secs(300),
        "a hang"
    );
}

#[test]
fn a_kill_of_the_callers_group_spares_the_supervisor() {
    let scratch = tempfile::tempdir().unwrap();
    let started_file = scratch.path().join("started");
    let dir = fixture(
        &format!(
            "touch {}; while :; do sleep 0.1; done",
            started_file.display()
        ),
        "",
    );
    let caller = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the recipe started", || started_file.exists());
    let group = Pid::from_raw(i32::try_from(caller.id()).unwrap());
    nix::sys::signal::killpg(group, Signal::SIGKILL).unwrap();
    let mut caller = caller;
    caller.wait().unwrap();
    // The supervisor survived in its own group: it saw its client go, stopped
    // the work, accounted for custody, and wrote a final record.
    let record = only_record(&dir.path().join(".ferry/local/records"));
    assert_eq!(record["outcome"], "lost", "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains("client lost"),
        "{record}"
    );
    assert!(!record["custody"].is_null(), "{record}");
    let next = ferry(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    assert_eq!(receipt(&next)["outcome"], "ran-pass");
}

#[test]
fn the_count_is_taken_after_the_readers_stop_and_matches_the_log() {
    let dir = fixture("exec yes", "");
    let consumer = dir.path().join("sleepy.py");
    // Reads a little, stalls past the stall bound, then drains.
    fs::write(
        &consumer,
        "import sys,time\nsys.stdin.buffer.read(1 << 20)\ntime.sleep(20)\nwhile sys.stdin.buffer.read(1 << 16):\n    pass\n",
    )
    .unwrap();
    let out = Command::new("sh")
        .current_dir(dir.path())
        .env_remove("GIT_DIR")
        .env_remove("FERRY_ID")
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .arg("-c")
        .arg(format!(
            "{} run probe --local --timeout-seconds 3 | python3 {}",
            env!("CARGO_BIN_EXE_ferry"),
            consumer.display()
        ))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record = only_record(&dir.path().join(".ferry/receipts"));
    let host = only_record(&dir.path().join(".ferry/local/records"));
    let id = record["id"].as_str().unwrap();
    let log = fs::read(
        dir.path()
            .join(format!(".ferry/local/runs/{id}/output.log")),
    )
    .unwrap();
    let logged = u64::try_from(log.split(|b| *b == b'\n').count() - 1).unwrap();
    let count = |value: &serde_json::Value| value.as_u64().unwrap_or(0);
    let lines = count(&host["output_lines"]);
    assert_eq!(logged, lines, "{host}");
    assert_eq!(
        count(&record["output_delivered"]["stdout"])
            + count(&record["output_dropped"]["stdout"])
            + count(&record["output_dropped"]["host"]),
        lines,
        "{record}"
    );
}

/// Runs `ferry run pass --local` in a pty with `tostop`, its foreground,
/// while another run holds the host lease exclusively; `interrupt` sends
/// SIGINT to the caller after 5 s. Returns (exit code, seconds taken).
fn under_tostop(dir: &Path, interrupt: bool) -> (i32, f64) {
    let harness = r#"
import os, pty, select, signal, sys, termios, time
pid, fd = pty.fork()
if pid == 0:
    attrs = termios.tcgetattr(0)
    attrs[3] |= termios.TOSTOP
    termios.tcsetattr(0, termios.TCSANOW, attrs)
    os.execv(sys.argv[2], sys.argv[2:])
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
    if time.time() - start > 150:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        print(-1, 150)
        sys.exit(0)
    time.sleep(0.05)
"#;
    let out = Command::new("python3")
        .current_dir(dir)
        .env_remove("FERRY_ID")
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .arg("-c")
        .arg(harness)
        .arg(if interrupt { "interrupt" } else { "wait" })
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

#[test]
fn a_terminal_with_tostop_never_stops_the_supervisor() {
    for interrupt in [false, true] {
        let scratch = tempfile::tempdir().unwrap();
        let holding = scratch.path().join("holding");
        let dir = fixture(&format!("touch {}; sleep 8", holding.display()), "");
        // The first run holds the host lease alone for 8 s, so the second
        // waits more than 3 s and prints its waiting line to the terminal.
        let holder = ferry(dir.path())
            .args(["run", "probe", "--local", "--exclusive", "--seat", "first"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_until("the first run holds the lease", || holding.exists());
        let (code, took) = under_tostop(dir.path(), interrupt);
        let mut holder = holder;
        holder.wait().unwrap();
        if interrupt {
            assert_eq!(code, 130, "took {took}");
            assert!(took < 30.0 + common::slack().as_secs_f64(), "took {took}");
        } else {
            assert_eq!(code, 0, "took {took}");
            assert!(took < 60.0 + common::slack().as_secs_f64(), "took {took}");
        }
    }
}

/// Lines in a file.
fn lines_in(path: &Path) -> u64 {
    let bytes = fs::read(path).unwrap_or_default();
    u64::try_from(bytes.split(|b| *b == b'\n').count() - 1).unwrap()
}

#[test]
fn everything_written_before_delivery_ended_is_in_the_log() {
    let scratch = tempfile::tempdir().unwrap();
    let side = scratch.path().join("side");
    let writer = scratch.path().join("writer.py");
    // Each line goes to the side file once its write to stdout succeeded.
    fs::write(
        &writer,
        format!(
            "import sys\nside = open('{}', 'a')\nfor i in range(16107):\n    sys.stdout.write('line %d\\n' % i)\n    sys.stdout.flush()\n    side.write('%d\\n' % i)\n    side.flush()\n",
            side.display()
        ),
    )
    .unwrap();
    let dir = fixture(&format!("exec python3 {}", writer.display()), "");
    let (stalled, pipe) = Stalled::new();
    let mut caller = ferry(dir.path())
        .args(["run", "probe", "--local", "--timeout-seconds", "3"])
        .stdout(pipe)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let host = only_record(&dir.path().join(".ferry/local/records"));
    drop(stalled);
    caller.wait().unwrap();
    let record = only_record(&dir.path().join(".ferry/receipts"));
    let id = record["id"].as_str().unwrap();
    let logged = lines_in(
        &dir.path()
            .join(format!(".ferry/local/runs/{id}/output.log")),
    );
    let count = |value: &serde_json::Value| value.as_u64().unwrap_or(0);
    assert_eq!(logged, lines_in(&side), "{host}");
    assert_eq!(logged, count(&host["output_lines"]), "{host}");
    assert_eq!(
        count(&record["output_delivered"]["stdout"])
            + count(&record["output_dropped"]["stdout"])
            + count(&record["output_dropped"]["host"]),
        logged,
        "{record}"
    );
}

#[test]
fn the_collect_hooks_output_is_in_the_runs_log_and_count() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init"]);
    git(d, &["config", "user.name", "Ferry Test"]);
    git(d, &["config", "user.email", "ferry@example.invalid"]);
    fs::write(d.join("collect"), "#!/bin/sh\nseq 1 500\n").unwrap();
    fs::set_permissions(d.join("collect"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        d.join("ferry.toml"),
        "version=1\n[hooks]\ncollect='collect'\n[recipe.probe]\ncommand=['sh','-c','seq 1 2']\n",
    )
    .unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-m", "fixture"]);
    let out = ferry(d)
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    let host = only_record(&d.join(".ferry/local/records"));
    let id = record["id"].as_str().unwrap();
    let logged = lines_in(&d.join(format!(".ferry/local/runs/{id}/output.log")));
    assert_eq!(logged, 502, "{host}");
    assert_eq!(host["output_lines"], 502, "{host}");
}

#[test]
fn recipe_and_collect_lines_never_interleave_in_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init"]);
    git(d, &["config", "user.name", "Ferry Test"]);
    git(d, &["config", "user.email", "ferry@example.invalid"]);
    fs::write(
        d.join("emit.py"),
        "import sys\nprefix, count = sys.argv[1], int(sys.argv[2])\nfor i in range(count):\n    sys.stdout.write('%s%06d\\n' % (prefix, i))\n",
    )
    .unwrap();
    fs::write(
        d.join("collect"),
        "#!/bin/sh\nexec python3 emit.py C 100000\n",
    )
    .unwrap();
    fs::set_permissions(d.join("collect"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        d.join("ferry.toml"),
        "version=1\n[hooks]\ncollect='collect'\n[recipe.probe]\ncommand=['python3','emit.py','R','20000']\n",
    )
    .unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-m", "fixture"]);
    let consumer = d.join("slow.py");
    fs::write(
        &consumer,
        "import sys,time\nfor i,l in enumerate(sys.stdin):\n    if i % 100 == 0: time.sleep(0.01)\n",
    )
    .unwrap();
    let out = Command::new("sh")
        .current_dir(d)
        .env_remove("GIT_DIR")
        .env_remove("FERRY_ID")
        .env("XDG_CONFIG_HOME", d.join("config"))
        .arg("-c")
        .arg(format!(
            "{} run probe --local | python3 {}",
            env!("CARGO_BIN_EXE_ferry"),
            consumer.display()
        ))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record = only_record(&d.join(".ferry/receipts"));
    let host = only_record(&d.join(".ferry/local/records"));
    let id = record["id"].as_str().unwrap();
    let log = fs::read_to_string(d.join(format!(".ferry/local/runs/{id}/output.log"))).unwrap();
    let (mut recipe, mut collect) = (0, 0);
    for line in log.lines() {
        let whole = line.len() == 7 && line[1..].bytes().all(|b| b.is_ascii_digit());
        assert!(whole, "a line from more than one writer: {line:?}");
        match &line[..1] {
            "R" => recipe += 1,
            "C" => collect += 1,
            _ => panic!("{line:?}"),
        }
    }
    // The collect hook is bounded (60 s): in a slow environment it may be
    // cut short, but every line it wrote is whole and counted.
    assert_eq!(recipe, 20000);
    assert!(collect > 0);
    assert_eq!(host["output_lines"], recipe + collect, "{host}");
}

/// The names in a run-records directory, sorted.
fn record_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir.join(".ferry/local/records"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The one receipt in the repository, whatever its state.
fn only_receipt_file(dir: &Path) -> serde_json::Value {
    let entry = fs::read_dir(dir.join(".ferry/receipts"))
        .unwrap()
        .flatten()
        .find(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .unwrap();
    serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap()
}

#[test]
fn a_crashed_supervisor_is_reconciled_to_lost_by_its_caller() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = fixture("echo started; sleep 2", "");
    let events = scratch.path().join("caller.out");
    let caller = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .stdout(fs::File::create(&events).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut caller = Caller(caller);
    let mut supervisor = None;
    wait_until("the recipe started", || {
        let text = fs::read_to_string(&events).unwrap_or_default();
        supervisor = text
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|event| event["event"] == "accepted")
            .and_then(|event| event["pid"].as_i64());
        supervisor.is_some() && text.contains("started")
    });
    // Under way, the receipt holds the caller's facts and no verdict.
    let submitted = only_receipt_file(dir.path());
    assert_eq!(submitted["state"], "submitted", "{submitted}");
    assert_eq!(submitted["outcome"], "lost", "{submitted}");
    assert_eq!(submitted["exit_code"], 255, "{submitted}");
    assert!(submitted["caller_exe"].is_string(), "{submitted}");
    // The supervisor this caller spawned, by the pid it announced.
    kill(
        Pid::from_raw(i32::try_from(supervisor.unwrap()).unwrap()),
        Signal::SIGKILL,
    )
    .unwrap();
    assert_eq!(caller.0.wait().unwrap().code(), Some(255));
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "lost", "{record}");
    assert_eq!(record["state"], "final", "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("supervisor gone (reconciled)"),
        "{record}"
    );
    let id = record["id"].as_str().unwrap();
    assert_eq!(record_names(dir.path()), [format!("{id}.json")]);
}

#[test]
fn a_record_left_unfinalized_is_reconciled_exactly_once_by_the_next_run() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let first = ferry(dir.path())
        .env("FERRY_FAIL_AT", "finalize-link")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let first = receipt(&first);
    // The verdict still reached the caller; the host could not record it.
    assert_eq!(first["outcome"], "ran-pass", "{first}");
    assert!(
        first["reason"]
            .as_str()
            .unwrap()
            .contains("host record: finalizing run"),
        "{first}"
    );
    let id = first["id"].as_str().unwrap();
    let reserved = format!("{id}.reserved.json");
    assert_eq!(record_names(dir.path()), [reserved.as_str()]);
    // A reconcile that fails leaves the reservation for the next reader.
    let held = ferry(dir.path())
        .env("FERRY_FAIL_AT", "reconcile")
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    assert_eq!(receipt(&held)["outcome"], "ran-pass");
    assert!(record_names(dir.path()).contains(&reserved));
    let mut lost = None;
    for _ in 0..2 {
        let next = ferry(dir.path())
            .args(["run", "pass", "--local", "--json"])
            .output()
            .unwrap();
        assert_eq!(receipt(&next)["outcome"], "ran-pass");
        let path = dir.path().join(format!(".ferry/local/records/{id}.json"));
        let bytes = fs::read(path).unwrap();
        // Reconciled once; the next run leaves the record as it was.
        assert_eq!(*lost.get_or_insert_with(|| bytes.clone()), bytes);
        assert!(!record_names(dir.path()).contains(&reserved));
    }
    let lost: serde_json::Value = serde_json::from_slice(&lost.unwrap()).unwrap();
    assert_eq!(lost["outcome"], "lost", "{lost}");
    assert_eq!(lost["exit_code"], 255, "{lost}");
    assert_eq!(lost["reason"], "supervisor gone (reconciled)", "{lost}");
    // The reservation's fields, then the verdict.
    assert_eq!(lost["id"], id, "{lost}");
    assert!(lost["supervisor"]["pid"].is_u64(), "{lost}");
}

#[test]
fn a_failed_reservation_refuses_before_anything_runs() {
    if !cfg!(debug_assertions) {
        return;
    }
    // A failed sync is met first by the reservation.
    for point in ["reserve", "fsync"] {
        let dir = fixture("echo ran", "");
        let out = ferry(dir.path())
            .env("FERRY_FAIL_AT", point)
            .args(["run", "probe", "--local", "--json"])
            .output()
            .unwrap();
        let record = receipt(&out);
        assert_eq!(out.status.code(), Some(125), "{record}");
        assert_eq!(record["outcome"], "refused", "{record}");
        assert!(
            record["reason"].as_str().unwrap().contains(point),
            "{record}"
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(!stdout.contains("\"accepted\""), "{stdout}");
        assert!(
            !dir.path().join(".ferry/local/records").exists()
                || record_names(dir.path()).is_empty()
        );
    }
}
