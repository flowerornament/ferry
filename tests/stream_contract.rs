//! Controls for a detached run (SPEC §5, §6, §12): the caller submits,
//! follows `output.log` by offset and stops by asking; nothing else the
//! client does changes how a run ends. Output is the bytes the recipe
//! wrote, through one capped writer.
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

/// A repository with a collect hook (`collect`, a shell script) and a
/// `probe` recipe running `command`.
fn with_collect(collect: &str, command: &str) -> TempDir {
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

/// Runs a caller to its end like `output()`, but bounded: a caller that
/// never ends fails the test instead of hanging it.
fn bounded_output(command: &mut Command) -> Output {
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

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = common::startup_deadline();
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// From the moment the run took its host lease (in the host record) to now,
/// when the test sees the complete record.
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

/// The one receipt in the repository, whatever its state.
fn only_receipt_file(dir: &Path) -> serde_json::Value {
    let entry = fs::read_dir(dir.join(".ferry/receipts"))
        .unwrap()
        .flatten()
        .find(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .unwrap();
    serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap()
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

/// A run's output log.
fn output_log(dir: &Path, id: &str) -> Vec<u8> {
    fs::read(dir.join(format!(".ferry/local/runs/{id}/output.log"))).unwrap()
}

/// The supervisor's pid, from the caller's `submitted` event in `events`.
fn submitted_pid(events: &Path) -> Option<i32> {
    fs::read_to_string(events)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["event"] == "submitted")
        .and_then(|event| event["pid"].as_i64())
        .and_then(|pid| i32::try_from(pid).ok())
}

/// A process that holds a pipe and never reads it, for a stalled consumer.
struct Stalled(std::process::Child);
impl Stalled {
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
impl Drop for Stalled {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A caller a control spawned: stopped (SIGINT, so it asks the place to
/// stop its run, then KILL) if the control ends early.
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

// --- Client independence (SPEC §12.4) ---

#[test]
fn concurrent_readers_complete_the_same_submitted_receipt() {
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let dir = fixture(
        &format!("touch {}; sleep 2; echo finished", started.display()),
        "",
    );
    let mut caller = Caller(
        ferry(dir.path())
            .args(["run", "probe", "--local", "--timeout-seconds", "60"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("the recipe started", || started.exists());
    caller.0.kill().unwrap();
    caller.0.wait().unwrap();
    assert_eq!(
        only_record(&dir.path().join(".ferry/local/records"))["outcome"],
        "ran-pass"
    );
    let submitted = only_receipt_file(dir.path());
    let id = submitted["id"].as_str().unwrap();
    let barrier = std::sync::Barrier::new(32);
    thread::scope(|scope| {
        let readers: Vec<_> = (0..32)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    bounded_output(ferry(dir.path()).args(["record", id]))
                })
            })
            .collect();
        for reader in readers {
            let out = reader.join().unwrap();
            assert_eq!(
                out.status.code(),
                Some(0),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(record["state"], "final");
            assert_eq!(record["outcome"], "ran-pass");
        }
    });
    assert_eq!(only_receipt_file(dir.path())["state"], "final");
}

#[test]
fn a_killed_caller_leaves_the_run_to_finish_and_ferry_record_completes_it() {
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let dir = fixture(
        &format!("touch {}; sleep 2; echo finished", started.display()),
        "",
    );
    let mut caller = ferry(dir.path())
        .args(["run", "probe", "--local", "--timeout-seconds", "60"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the recipe started", || started.exists());
    caller.kill().unwrap();
    caller.wait().unwrap();
    // The run is not the connection's: it reaches its own end.
    let host = only_record(&dir.path().join(".ferry/local/records"));
    assert_eq!(host["outcome"], "ran-pass", "{host}");
    let submitted = only_receipt_file(dir.path());
    assert_eq!(submitted["state"], "submitted", "{submitted}");
    let id = submitted["id"].as_str().unwrap();
    let out = ferry(dir.path()).args(["record", id]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["state"], "final", "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(only_receipt_file(dir.path())["state"], "final");
    assert_eq!(output_log(dir.path(), id), b"finished\n");
}

#[test]
fn a_kill_of_the_callers_group_spares_the_supervisor() {
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let dir = fixture(&format!("touch {}; sleep 2", started.display()), "");
    let mut caller = ferry(dir.path())
        .args([
            "run",
            "probe",
            "--local",
            "--json",
            "--timeout-seconds",
            "60",
        ])
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the recipe started", || started.exists());
    let group = Pid::from_raw(i32::try_from(caller.id()).unwrap());
    nix::sys::signal::killpg(group, Signal::SIGKILL).unwrap();
    caller.wait().unwrap();
    // The supervisor lives in its own session: the run completes.
    let record = only_record(&dir.path().join(".ferry/local/records"));
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert!(!record["custody"].is_null(), "{record}");
}

#[test]
fn a_killed_callers_place_follow_ends_and_the_run_goes_on() {
    let scratch = tempfile::tempdir().unwrap();
    let (started, release) = (
        scratch.path().join("started"),
        scratch.path().join("release"),
    );
    let dir = fixture(
        &format!(
            "touch {}; while [ ! -e {} ]; do sleep 0.1; done; echo finished",
            started.display(),
            release.display()
        ),
        "",
    );
    // Its own group holds the caller and the `place follow` it starts; the
    // supervisor has its own session.
    let mut caller = ferry(dir.path())
        .args(["run", "probe", "--local", "--timeout-seconds", "120"])
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let group = Pid::from_raw(-i32::try_from(caller.id()).unwrap());
    wait_until("the recipe started", || started.exists());
    caller.kill().unwrap();
    caller.wait().unwrap();
    // The run is quiet and still going: only the closed pipe can end its
    // follow, and it must, long before the run's own end.
    let bound = Instant::now() + Duration::from_secs(5) + common::slack();
    while kill(group, None).is_ok() {
        assert!(
            Instant::now() < bound,
            "the place follow outlived its caller"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let records = dir.path().join(".ferry/local/records");
    assert!(
        record_names(dir.path())
            .iter()
            .all(|n| n.starts_with('.') || n.ends_with(".reserved.json")),
        "{:?}",
        record_names(dir.path())
    );
    fs::write(&release, "").unwrap();
    let host = only_record(&records);
    assert_eq!(host["outcome"], "ran-pass", "{host}");
    let id = host["id"].as_str().unwrap();
    assert_eq!(output_log(dir.path(), id), b"finished\n");
}

#[test]
fn a_dropped_follow_reconnects_and_receives_every_byte() {
    if !cfg!(debug_assertions) {
        return;
    }
    // More than one 64 KiB chunk, written in two bursts.
    let dir = fixture("seq 1 30000; sleep 0.5; seq 1 30000", "");
    let out = bounded_output(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "follow-drop")
            .args(["run", "probe", "--local", "--json"]),
    );
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    let log = output_log(dir.path(), record["id"].as_str().unwrap());
    assert!(log.len() > 128 << 10);
    // Every follow broke after its first chunk; the caller resumed from
    // the offset it had, and its byte count equals the file's.
    assert_eq!(out.stderr.len(), log.len());
    assert_eq!(out.stderr, log);
    assert_eq!(record["output"]["bytes"], log.len());
}

#[test]
fn ferry_follow_reattaches_from_any_terminal() {
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let dir = fixture(
        &format!("echo one; touch {}; sleep 2; echo two", started.display()),
        "",
    );
    let mut caller = ferry(dir.path())
        .args(["run", "probe", "--local", "--timeout-seconds", "60"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the recipe started", || started.exists());
    caller.kill().unwrap();
    caller.wait().unwrap();
    let id = only_receipt_file(dir.path())["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = ferry(dir.path()).args(["follow", &id]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    // The whole output from its start, then the completed receipt.
    assert!(out.stdout.starts_with(b"one\ntwo\n"), "{:?}", out.stdout);
    let record: serde_json::Value = serde_json::from_slice(&out.stdout[8..]).unwrap();
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(record["state"], "final", "{record}");
}

#[test]
fn a_closed_caller_pipe_is_data_and_the_run_reaches_its_own_end() {
    // Far more than a pipe's worth, so the caller writes on after `head`
    // has gone; the recipe ends by itself, so no timer races the machine.
    let dir = fixture("seq 1 100000", "");
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
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1\n");
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(record["exit_code"], 0, "{record}");
    // Every byte the recipe wrote is in its log.
    assert_eq!(record["output"]["bytes"], 588_895, "{record}");
}

#[test]
fn a_closed_caller_stderr_is_not_a_stop() {
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
    // One pipe for both streams: the order the recipe wrote, on stdout.
    assert_eq!(out.stdout, [&b"err\n".repeat(20)[..], b"done\n"].concat());
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "ran-pass", "{record}");
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
    assert_eq!(out.stdout, b"a\xffb\n\xfe\n");
}

// --- Stop (SPEC §12.5) ---

#[test]
fn a_signal_stops_a_run_whose_consumer_stalls() {
    let dir = fixture("exec yes", "");
    let (mut stalled, pipe) = Stalled::lasting(600);
    let mut caller = Caller(
        ferry(dir.path())
            .args(["run", "probe", "--local", "--timeout-seconds", "600"])
            .stdout(pipe)
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    // Signal once the stall is real: the log has passed what a pipe holds.
    let runs = dir.path().join(".ferry/local/runs");
    wait_until("output backed up behind the stalled consumer", || {
        fs::read_dir(&runs)
            .into_iter()
            .flatten()
            .flatten()
            .any(|run| fs::metadata(run.path().join("output.log")).is_ok_and(|m| m.len() > 4 << 20))
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
    // The consumer never read: the record, not the output, ended the wait.
    assert!(stalled.alive(), "the consumer stopped stalling");
    let bound = limits::SESSION_CLEANUP + limits::DRAIN + Duration::from_secs(2) + common::slack();
    assert!(
        signalled.elapsed() < bound,
        "{:?} >= {bound:?}",
        signalled.elapsed()
    );
    assert_eq!(code.flatten(), Some(130));
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "stopped", "{record}");
    assert!(
        record["stopped_by"]["why"]
            .as_str()
            .unwrap()
            .contains("SIGINT"),
        "{record}"
    );
}

#[test]
fn a_caller_whose_stop_fails_or_that_is_signalled_twice_leaves_the_receipt_submitted() {
    // SPEC §4 and A3: while the run may go on, no verdict is claimed; the
    // caller exits 255 naming the next step, and `ferry record` later
    // finalizes the receipt from the place's record.
    if !cfg!(debug_assertions) {
        return;
    }
    for (fail_at, script, signals, next, outcome, exit) in [
        (
            "stop-write",
            "sleep 3",
            1,
            "not stopped; continues until its deadline in",
            "ran-pass",
            0,
        ),
        (
            "",
            "trap '' TERM; sleep 3",
            2,
            "left at a second SIGINT",
            "stopped",
            130,
        ),
    ] {
        let scratch = tempfile::tempdir().unwrap();
        let started = scratch.path().join("started");
        let dir = fixture(&format!("touch {}; {script}", started.display()), "");
        let stderr = scratch.path().join("stderr");
        let mut caller = Caller(
            ferry(dir.path())
                .env("FERRY_FAIL_AT", fail_at)
                .args(["run", "probe", "--local", "--timeout-seconds", "60"])
                .stdout(Stdio::null())
                .stderr(fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        wait_until("the recipe started", || started.exists());
        let id = only_receipt_file(dir.path())["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let pid = Pid::from_raw(i32::try_from(caller.0.id()).unwrap());
        kill(pid, Signal::SIGINT).unwrap();
        if signals == 2 {
            let stop = dir
                .path()
                .join(".ferry/local/runs")
                .join(&id)
                .join("stop.json");
            wait_until("the stop was delivered", || stop.exists());
            kill(pid, Signal::SIGINT).unwrap();
        }
        let mut code = None;
        wait_until("the signalled caller ended", || {
            code = caller.0.try_wait().unwrap().map(|status| status.code());
            code.is_some()
        });
        assert_eq!(code.flatten(), Some(255), "{fail_at}");
        let receipt = only_receipt_file(dir.path());
        assert_eq!(receipt["state"], "submitted", "{receipt}");
        // The caller names the next step (SPEC §4).
        let said = fs::read_to_string(&stderr).unwrap();
        assert!(said.contains(next) && said.contains(&id), "{said}");
        let final_record = dir
            .path()
            .join(".ferry/local/records")
            .join(format!("{id}.json"));
        wait_until("the run finished", || final_record.exists());
        let out = ferry(dir.path()).args(["record", &id]).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(exit),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let receipt = only_receipt_file(dir.path());
        assert_eq!(receipt["state"], "final", "{receipt}");
        assert_eq!(receipt["outcome"], outcome, "{receipt}");
    }
}

/// A signal after the run is final (critic C6), while its output is still
/// on the way to a slow reader: the verdict stands, every byte arrives, and
/// the caller exits with the run's code, not "not stopped" and 255.
#[test]
fn a_signal_after_the_run_is_final_keeps_its_verdict_and_every_byte() {
    use std::io::Read;
    let dir = fixture("head -c 4000000 /dev/zero; echo done", "");
    let mut caller = Caller(
        ferry(dir.path())
            .args(["run", "probe", "--local", "--timeout-seconds", "60"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    // Nobody reads its stdout yet: a slow consumer.
    let records = dir.path().join(".ferry/local/records");
    only_record(&records);
    thread::sleep(Duration::from_millis(500));
    kill(
        Pid::from_raw(i32::try_from(caller.0.id()).unwrap()),
        Signal::SIGINT,
    )
    .unwrap();
    let mut stdout = caller.0.stdout.take().unwrap();
    let reader = thread::spawn(move || {
        let mut all = Vec::new();
        let _ = stdout.read_to_end(&mut all);
        all.len()
    });
    let mut stderr = String::new();
    caller
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let mut code = None;
    wait_until("the caller ended", || {
        code = caller.0.try_wait().unwrap().map(|status| status.code());
        code.is_some()
    });
    assert_eq!(code.flatten(), Some(0), "{stderr}");
    assert_eq!(reader.join().unwrap(), 4_000_005, "{stderr}");
    let receipt = only_receipt_file(dir.path());
    assert_eq!(receipt["state"], "final", "{receipt}");
    assert_eq!(receipt["outcome"], "ran-pass", "{receipt}");
}

/// A signal after the place's verdict keeps it, whatever its outcome
/// (critic C7): here the supervisor is killed and reconciled to lost/255,
/// and a later SIGINT must not turn the final receipt into lost/130.
#[test]
fn a_signal_after_a_lost_verdict_keeps_the_places_code() {
    let scratch = tempfile::tempdir().unwrap();
    let events = scratch.path().join("events");
    // Far more than a pipe's worth, then a short wait: the recipe outlives
    // its killed supervisor by at most 4 s and ends on its own.
    let dir = fixture("head -c 4000000 /dev/zero; sleep 4", "");
    let mut caller = Caller(
        ferry(dir.path())
            .args([
                "run",
                "probe",
                "--local",
                "--json",
                "--timeout-seconds",
                "60",
            ])
            .stdout(fs::File::create(&events).unwrap())
            // The output, unread: a slow consumer.
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    wait_until("submitted", || submitted_pid(&events).is_some());
    let id = only_receipt_file(dir.path())["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let log = dir
        .path()
        .join(format!(".ferry/local/runs/{id}/output.log"));
    wait_until("the output was written", || {
        fs::metadata(&log).is_ok_and(|m| m.len() >= 4_000_000)
    });
    // The supervisor this control's caller started, by its exact pid.
    kill(
        Pid::from_raw(submitted_pid(&events).unwrap()),
        Signal::SIGKILL,
    )
    .unwrap();
    let state = dir.path().join(".ferry/local");
    let record = state.join(format!("records/{id}.json"));
    wait_until("the run was reconciled to lost", || {
        let _ = Command::new(env!("CARGO_BIN_EXE_ferry"))
            .args(["place", "--state"])
            .arg(&state)
            .args(["record", &id])
            .output();
        record.exists()
    });
    let place: serde_json::Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    assert_eq!(place["outcome"], "lost", "{place}");
    // Its caller has read the verdict by now, or reads it soon.
    thread::sleep(Duration::from_millis(500));
    kill(
        Pid::from_raw(i32::try_from(caller.0.id()).unwrap()),
        Signal::SIGINT,
    )
    .unwrap();
    let mut stderr = caller.0.stderr.take().unwrap();
    let drain = thread::spawn(move || {
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
    });
    let mut code = None;
    wait_until("the caller ended", || {
        code = caller.0.try_wait().unwrap().map(|status| status.code());
        code.is_some()
    });
    let _ = drain.join();
    let receipt = only_receipt_file(dir.path());
    assert_eq!(receipt["state"], "final", "{receipt}");
    assert_eq!(receipt["outcome"], "lost", "{receipt}");
    assert_eq!(receipt["exit_code"], place["exit_code"], "{receipt}");
    assert_eq!(code.flatten(), Some(255), "{receipt}");
}

/// A stop while the run prepares spawns nothing: neither the recipe nor
/// the collect hook runs.
#[test]
fn a_stop_during_prepare_spawns_nothing() {
    if !cfg!(debug_assertions) {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let (ran, collected) = (scratch.path().join("ran"), scratch.path().join("collected"));
    let dir = with_collect(
        &format!("touch {}", collected.display()),
        &format!("touch {}", ran.display()),
    );
    let mut caller = Caller(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "prepare-pause")
            .args(["run", "probe", "--local", "--timeout-seconds", "60"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let runs = dir.path().join(".ferry/local/runs");
    let mut id = String::new();
    wait_until("the run paused before its spawn", || {
        fs::read_dir(&runs)
            .ok()
            .and_then(|entries| {
                entries
                    .flatten()
                    .find(|e| e.path().join("paused").exists())
                    .map(|e| id = e.file_name().to_string_lossy().into_owned())
            })
            .is_some()
    });
    let out = ferry(dir.path()).args(["stop", &id]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut code = None;
    wait_until("the caller ended", || {
        code = caller.0.try_wait().unwrap().map(|status| status.code());
        code.is_some()
    });
    let host = only_record(&dir.path().join(".ferry/local/records"));
    assert_eq!(host["outcome"], "stopped", "{host}");
    assert!(!ran.exists(), "the recipe was spawned");
    assert!(!collected.exists(), "the collect hook ran");
}

/// The first stop decides (critic C2): a later stop with another signal is
/// accepted and changes nothing, neither the signal nor `stopped_by`.
#[test]
fn a_second_stop_changes_nothing() {
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    // It takes 3 s to leave once stopped, so the second stop always comes
    // before the verdict.
    let dir = fixture(
        &format!(
            "trap 'sleep 3; exit 1' INT TERM; touch {}; while :; do sleep 0.1; done",
            started.display()
        ),
        "",
    );
    let _caller = Caller(
        ferry(dir.path())
            .args(["run", "probe", "--local", "--timeout-seconds", "60"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("the recipe started", || started.exists());
    let id = only_receipt_file(dir.path())["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let state = dir.path().join(".ferry/local");
    let stop = |who: &str, signal: &str| {
        Command::new(env!("CARGO_BIN_EXE_ferry"))
            .args(["place", "--state"])
            .arg(&state)
            .args([
                "stop", &id, "--seat", "x", "--who", who, "--why", "w", "--other", "--signal",
                signal,
            ])
            .output()
            .unwrap()
    };
    assert!(stop("first", "2").status.success());
    let second = stop("second", "15");
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stdout)
    );
    let record = only_record(&state.join("records"));
    assert_eq!(record["outcome"], "stopped", "{record}");
    assert_eq!(record["exit_code"], 130, "{record}");
    assert_eq!(record["stopped_by"]["who"], "first", "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains("SIGINT"),
        "{record}"
    );
}

#[test]
fn ferry_stop_stops_its_own_run_and_refuses_anothers_without_a_reason() {
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let dir = fixture(&format!("touch {}; sleep 60", started.display()), "");
    let _caller = Caller(
        ferry(dir.path())
            .args([
                "run",
                "probe",
                "--local",
                "--json",
                "--timeout-seconds",
                "120",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("the recipe started", || started.exists());
    let id = only_receipt_file(dir.path())["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let state = dir.path().join(".ferry/local");
    let place_stop = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ferry"))
            .arg("place")
            .arg("--state")
            .arg(&state)
            .args([
                "stop",
                &id,
                "--seat",
                "someone-else",
                "--who",
                "w",
                "--why",
                "y",
            ])
            .args(extra)
            .output()
            .unwrap()
    };
    // Another caller's seat, without --other: refused, nothing stopped.
    let refused = place_stop(&[]);
    assert_eq!(refused.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&refused.stdout).contains("another caller"));
    assert!(!state.join(format!("runs/{id}/stop.json")).exists());
    // Its own caller, by `ferry stop`.
    let stopped = ferry(dir.path()).args(["stop", &id]).output().unwrap();
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "stopped", "{record}");
    assert_eq!(record["exit_code"], 143, "{record}");
    assert_eq!(record["stopped_by"]["why"], "ferry stop", "{record}");
    assert_eq!(record["stopped_by"]["other"], false, "{record}");
}

#[test]
fn a_callers_signal_after_the_verdict_is_named_and_keeps_it() {
    let scratch = tempfile::tempdir().unwrap();
    let collecting = scratch.path().join("collecting");
    let dir = with_collect(
        &format!("touch {}\nsleep 3", collecting.display()),
        "echo done",
    );
    let caller = ferry(dir.path())
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

// --- Deadline and waits (SPEC §12.6, A1) ---

#[test]
fn the_deadline_ends_a_run_with_no_client() {
    if !cfg!(debug_assertions) {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let dir = fixture(&format!("touch {}; sleep 600", started.display()), "");
    // The deadline, 30 s after submit (long enough to start under load),
    // ends it; the timeout, 600 s, never could within this test.
    let mut caller = ferry(dir.path())
        .env("FERRY_DEADLINE_MS", "30000")
        .args(["run", "probe", "--local", "--timeout-seconds", "600"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the recipe started", || started.exists());
    caller.kill().unwrap();
    caller.wait().unwrap();
    let records = dir.path().join(".ferry/local/records");
    let bound = Instant::now() + Duration::from_secs(30) + common::slack();
    let host = loop {
        let host = fs::read_dir(&records).ok().and_then(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .find(|p| !p.to_string_lossy().ends_with(".reserved.json"))
                .and_then(|p| serde_json::from_slice::<serde_json::Value>(&fs::read(p).ok()?).ok())
        });
        if let Some(host) = host {
            break host;
        }
        assert!(Instant::now() < bound, "the deadline did not end the run");
        thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(host["outcome"], "timeout", "{host}");
    assert_eq!(host["exit_code"], 124, "{host}");
    assert_eq!(
        host["deadline_ms"].as_u64().unwrap() - host["submitted_ms"].as_u64().unwrap(),
        30_000,
        "{host}"
    );
    // The session was stopped with it.
    let owners = dir.path().join(".ferry/local/owners");
    assert!(!owners.exists() || fs::read_dir(&owners).unwrap().count() == 0);
}

/// One wait budget for all of a run's locks (critic C3): seat, gate and
/// host each held in turn for less than the wait must not add up past it.
/// Every wait ends by submitted + wait, so the deadline holds by
/// construction; the run is refused naming a holder.
#[test]
fn the_lock_waits_together_end_by_one_bound() {
    use fs2::FileExt;
    let dir = fixture("echo ran", "");
    // One run to create the seat's lock and learn its name.
    let out = ferry(dir.path())
        .args(["run", "probe", "--local"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let locks = dir.path().join(".ferry/local/locks");
    let seat = fs::read_dir(&locks)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-default")
        })
        .unwrap();
    let hold = |p: &Path| {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(p)
            .unwrap();
        file.lock_exclusive().unwrap();
        file
    };
    let (seat_lock, gate, host) = (
        hold(&seat),
        hold(&locks.join("host-gate")),
        hold(&locks.join("host-exclusive")),
    );
    let scratch = tempfile::tempdir().unwrap();
    let stdout = scratch.path().join("out");
    let mut caller = Caller(
        ferry(dir.path())
            .args(["run", "probe", "--local", "--json", "--wait-seconds", "4"])
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    // Each held 3 s, under the 4 s wait; together 9 s.
    for lock in [seat_lock, gate, host] {
        thread::sleep(Duration::from_secs(3));
        drop(lock);
    }
    let mut code = None;
    wait_until("the caller ended", || {
        code = caller.0.try_wait().unwrap().map(|status| status.code());
        code.is_some()
    });
    let record = receipt(&Output {
        status: caller.0.wait().unwrap(),
        stdout: fs::read(&stdout).unwrap(),
        stderr: vec![],
    });
    assert_eq!(code.flatten(), Some(125), "{record}");
    assert_eq!(record["outcome"], "refused", "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains("held by"),
        "{record}"
    );
}

#[test]
fn a_host_lease_wait_past_its_bound_refuses_naming_the_holder() {
    let scratch = tempfile::tempdir().unwrap();
    let (holding, hold) = (scratch.path().join("holding"), scratch.path().join("hold"));
    // The holder holds until the test lets go, however slow the second
    // run is to reach its wait.
    fs::write(&hold, "").unwrap();
    let dir = fixture(
        &format!(
            "touch {}; while [ -e {} ]; do sleep 0.1; done",
            holding.display(),
            hold.display()
        ),
        "",
    );
    let mut holder = Caller(
        ferry(dir.path())
            .args([
                "run",
                "probe",
                "--local",
                "--exclusive",
                "--seat",
                "first",
                "--json",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("the first run holds the host alone", || holding.exists());
    // Bounded: a wait with no bound fails here instead of hanging.
    let out = bounded_output(ferry(dir.path()).args([
        "run",
        "pass",
        "--local",
        "--seat",
        "second",
        "--wait-seconds",
        "2",
        "--json",
    ]));
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert_eq!(record["outcome"], "refused", "{record}");
    let holder_id = fs::read_dir(dir.path().join(".ferry/receipts"))
        .unwrap()
        .flatten()
        .filter_map(|e| serde_json::from_slice::<serde_json::Value>(&fs::read(e.path()).ok()?).ok())
        .find(|r| r["seat"] == "first")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("held by run {holder_id}")),
        "{record}"
    );
    // The wait is bounded apart from the run's own timeout: the deadline is
    // submitted + wait + timeout (the place's 6 h maximum) + teardown.
    let id = record["id"].as_str().unwrap();
    let ran: serde_json::Value = serde_json::from_slice(
        &fs::read(dir.path().join(format!(".ferry/local/records/{id}.json"))).unwrap(),
    )
    .unwrap();
    let wait_timeout_ms = (2 + limits::MAX_TIMEOUT.as_secs()) * 1000;
    assert_eq!(
        ran["deadline_ms"].as_u64().unwrap() - ran["submitted_ms"].as_u64().unwrap(),
        wait_timeout_ms + u64::try_from(limits::TEARDOWN.as_millis()).unwrap(),
        "{ran}"
    );
    fs::remove_file(&hold).unwrap();
    wait_until("the holder ended", || {
        holder.0.try_wait().unwrap().is_some()
    });
}

// --- Output: one capped writer (SPEC §11) ---

#[test]
fn output_past_the_cap_is_dropped_and_the_run_goes_on() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("seq 1 100000; echo end", "");
    let out = ferry(dir.path())
        .env("FERRY_OUTPUT_CAP", "1000")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(record["output"]["capped"], true, "{record}");
    assert_eq!(record["output"]["bytes"], 1000, "{record}");
    let log = output_log(dir.path(), record["id"].as_str().unwrap());
    assert_eq!(log.len(), 1000);
}

#[test]
fn a_failed_output_write_is_counted_and_the_run_goes_on() {
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
    assert_eq!(record["output"]["write_errors"], 1, "{record}");
    assert_eq!(record["output"]["bytes"], 0, "{record}");
}

#[test]
fn a_survivor_holding_the_output_pipe_cannot_keep_the_run() {
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("chatty.pid");
    let _owned = Owned(pidfile.clone());
    let dir = fixture(
        &format!(
            "python3 -c 'import os,time\nos.setsid()\nopen(\"{p}.tmp\",\"w\").write(str(os.getpid()))\nos.rename(\"{p}.tmp\",\"{p}\")\nwhile True:\n    print(\"chat\", flush=True)\n    time.sleep(0.001)' & while [ ! -e {p} ]; do sleep 0.05; done; echo done",
            p = pidfile.display()
        ),
        "",
    );
    // Debug builds keep the survivor alive on Linux too.
    let out = bounded_output(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "stop-survivors")
            .args(["run", "probe", "--local", "--json"]),
    );
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("output still open"),
        "{record}"
    );
    // Teardown and the drain, from the lease to the record.
    let host = only_record(&dir.path().join(".ferry/local/records"));
    let took = since_acquired(&host);
    let bound = limits::SESSION_CLEANUP * 2 + limits::DRAIN * 2 + common::slack();
    assert!(took < bound, "{took:?} >= {bound:?}");
    // The log was closed before the record: its size is the count.
    let log = output_log(dir.path(), record["id"].as_str().unwrap());
    assert_eq!(record["output"]["bytes"], log.len(), "{record}");
}

#[test]
fn the_collect_hooks_output_is_in_the_runs_log() {
    let dir = with_collect("seq 1 500", "seq 1 2");
    let out = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    let log = output_log(dir.path(), record["id"].as_str().unwrap());
    let expected: Vec<u8> = (1..=2)
        .chain(1..=500)
        .flat_map(|i| format!("{i}\n").into_bytes())
        .collect();
    assert_eq!(log, expected);
    assert_eq!(record["output"]["bytes"], log.len(), "{record}");
}

// --- Failures inside the run keep the verdict ---

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
fn a_terminal_with_tostop_never_stops_the_run() {
    for interrupt in [false, true] {
        let scratch = tempfile::tempdir().unwrap();
        let holding = scratch.path().join("holding");
        let dir = fixture(&format!("touch {}; sleep 8", holding.display()), "");
        // The first run holds the host lease alone for 8 s, so the second
        // waits while its caller sits on the terminal.
        let mut holder = ferry(dir.path())
            .args(["run", "probe", "--local", "--exclusive", "--seat", "first"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_until("the first run holds the lease", || holding.exists());
        let (code, took) = under_tostop(dir.path(), interrupt);
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

// --- The record cycle (SPEC §12.1-3, slice 1) ---

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
    let started = || {
        let id = only_receipt_file(dir.path())["id"].as_str()?.to_owned();
        let log = dir
            .path()
            .join(format!(".ferry/local/runs/{id}/output.log"));
        fs::read(log).ok().filter(|log| log.starts_with(b"started"))
    };
    wait_until("the recipe started", || {
        submitted_pid(&events).is_some() && started().is_some()
    });
    // Under way, the receipt holds the caller's facts and no verdict.
    let submitted = only_receipt_file(dir.path());
    assert_eq!(submitted["state"], "submitted", "{submitted}");
    assert_eq!(submitted["outcome"], "lost", "{submitted}");
    assert_eq!(submitted["exit_code"], 255, "{submitted}");
    assert!(submitted["caller_exe"].is_string(), "{submitted}");
    // The supervisor of this caller's run, by the pid it announced.
    kill(
        Pid::from_raw(submitted_pid(&events).unwrap()),
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
fn a_record_left_unfinalized_is_reconciled_exactly_once() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    // Every finalize fails, the caller's reconcile included: the run ended,
    // but nothing can say so yet, and the receipt claims no verdict.
    let first = ferry(dir.path())
        .env("FERRY_FAIL_AT", "finalize-link")
        .env("FERRY_UNREACHABLE_MS", "3000")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let first = receipt(&first);
    assert_eq!(first["state"], "submitted", "{first}");
    assert_eq!(first["outcome"], "lost", "{first}");
    let id = first["id"].as_str().unwrap().to_owned();
    // The next run on the seat reconciles it, once.
    let path = dir.path().join(format!(".ferry/local/records/{id}.json"));
    let mut bytes = None;
    for _ in 0..2 {
        let next = ferry(dir.path())
            .args(["run", "pass", "--local", "--json"])
            .output()
            .unwrap();
        assert_eq!(receipt(&next)["outcome"], "ran-pass");
        let now = fs::read(&path).unwrap();
        assert_eq!(*bytes.get_or_insert_with(|| now.clone()), now);
    }
    let lost: serde_json::Value = serde_json::from_slice(&bytes.unwrap()).unwrap();
    assert_eq!(lost["outcome"], "lost", "{lost}");
    assert_eq!(lost["reason"], "supervisor gone (reconciled)", "{lost}");
    // The reservation's fields, then the verdict.
    assert_eq!(lost["id"], id, "{lost}");
    assert!(lost["supervisor"]["pid"].is_u64(), "{lost}");
    // And the receipt follows the place's record.
    let out = ferry(dir.path()).args(["record", &id]).output().unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["state"], "final", "{record}");
    assert_eq!(record["outcome"], "lost", "{record}");
}

#[test]
fn a_failed_reconcile_leaves_the_receipt_submitted_until_the_next_reader() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    // Its finalize and every reconcile fail: the receipt cannot be final.
    let first = ferry(dir.path())
        .env("FERRY_FAIL_AT", "finalize-link,reconcile")
        .env("FERRY_UNREACHABLE_MS", "3000")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let first = receipt(&first);
    assert_eq!(first["state"], "submitted", "{first}");
    let id = first["id"].as_str().unwrap().to_owned();
    assert_eq!(record_names(dir.path()), [format!("{id}.reserved.json")]);
    // Once its supervisor is gone (the caller gave up first, however slow
    // the machine), the next reader reconciles it and the receipt is final.
    let reservation: serde_json::Value = serde_json::from_slice(
        &fs::read(
            dir.path()
                .join(format!(".ferry/local/records/{id}.reserved.json")),
        )
        .unwrap(),
    )
    .unwrap();
    let supervisor =
        Pid::from_raw(i32::try_from(reservation["supervisor"]["pid"].as_i64().unwrap()).unwrap());
    wait_until("the supervisor exited", || kill(supervisor, None).is_err());
    let out = ferry(dir.path()).args(["record", &id]).output().unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["state"], "final", "{record}");
    assert_eq!(record["outcome"], "lost", "{record}");
}

#[test]
fn a_submit_that_dies_after_reserving_leaves_a_lost_run_and_a_submitted_receipt() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo never", "");
    let out = ferry(dir.path())
        .env("FERRY_FAIL_AT", "submit-release")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let first = receipt(&out);
    // No reply: the run may have started, so no verdict is claimed.
    assert_eq!(first["state"], "submitted", "{first}");
    let id = first["id"].as_str().unwrap().to_owned();
    // Its supervisor was never released and left: reconciled as lost.
    let out = ferry(dir.path()).args(["record", &id]).output().unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["outcome"], "lost", "{record}");
    assert!(
        !dir.path()
            .join(format!(".ferry/local/runs/{id}/output.log"))
            .exists(),
        "the recipe never ran"
    );
}

#[test]
fn a_failed_reservation_or_spawn_refuses_before_anything_runs() {
    if !cfg!(debug_assertions) {
        return;
    }
    // A failed sync is met first by the reservation.
    for point in ["reserve", "fsync", "submit-spawn"] {
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
        assert!(!stdout.contains("\"submitted\""), "{stdout}");
        assert!(
            !dir.path().join(".ferry/local/records").exists()
                || record_names(dir.path()).is_empty()
        );
    }
}

#[test]
fn a_stop_that_cannot_be_recorded_is_refused_and_the_run_goes_on() {
    if !cfg!(debug_assertions) {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let dir = fixture(&format!("touch {}; sleep 2", started.display()), "");
    let caller = Caller(
        ferry(dir.path())
            .args([
                "run",
                "probe",
                "--local",
                "--json",
                "--timeout-seconds",
                "60",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("the recipe started", || started.exists());
    let id = only_receipt_file(dir.path())["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let stopped = ferry(dir.path())
        .env("FERRY_FAIL_AT", "stop-write")
        .args(["stop", &id])
        .output()
        .unwrap();
    assert_eq!(stopped.status.code(), Some(125));
    let record = only_record(&dir.path().join(".ferry/receipts"));
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    drop(caller);
}
