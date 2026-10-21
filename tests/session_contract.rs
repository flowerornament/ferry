//! A run on its place (SPEC §5, §7, §11): the detached supervisor and the
//! client, the session stop and its survivors, locks and the exclusive host,
//! the deadline, output, collection and artifacts, and the record cycle.
#![allow(clippy::wildcard_imports)]
mod common;
use common::*;

#[test]
fn collection_failure_records_its_error_without_changing_recipe_verdict() {
    let dir = fixture("true", "");
    fs::write(dir.path().join("ferry.toml"), "version=1\n[hooks]\ncollect='missing-collect'\n[recipe.pass]\ncommand=['sh','-c','echo attempted']\n").unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "broken collection hook"]);
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(receipt(&out)["outcome"], "ran-pass");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(receipt(&out)["collection"]["exit_code"], 127);
}

#[test]
fn collection_runs_after_recipe_failure_timeout_and_prepare_refusal() {
    // Only the timeout case gets a tight budget: it also covers seat
    // preparation, which a loaded build sandbox can spend a second on.
    for (command, prepare, expected, timeout) in [
        ("exit 7", false, "ran-fail", "60"),
        ("sleep 10", false, "timeout", "1"),
        ("exit 0", true, "refused", "60"),
    ] {
        let dir = fixture("true", "");
        fs::write(
            dir.path().join("collect"),
            "#!/bin/sh\necho collected > collected\n",
        )
        .unwrap();
        fs::set_permissions(
            dir.path().join("collect"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::write(dir.path().join("ferry.toml"), format!("version=1\n[hooks]\ncollect='collect'\n{}[recipe.pass]\ncommand=['sh','-c','{command}']\n", if prepare { "prepare='missing-prepare'\n" } else { "" })).unwrap();
        git(dir.path(), &["add", "ferry.toml", "collect"]);
        git(dir.path(), &["commit", "-m", "collection contract"]);
        let out = run(
            dir.path(),
            &[
                "run",
                "pass",
                "--local",
                "--timeout-seconds",
                timeout,
                "--json",
            ],
        );
        let record = receipt(&out);
        assert_eq!(
            record["outcome"],
            expected,
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert_eq!(record["collection"]["exit_code"], 0);
        let slot = only_checkout(dir.path());
        assert_eq!(
            fs::read_to_string(slot.join("collected")).unwrap(),
            "collected\n"
        );
    }
}

/// A session that ends with no result keeps its own error as the reason
/// (SPEC §5): refused before the recipe was attempted, lost after it.
/// Without the error file (`owner-error` injected), it says only that the
/// owner exited.
#[test]
fn a_session_owners_error_is_the_records_reason() {
    for (prepare, recipe, fail_at, outcome, code, reason) in [
        (
            "exit 1",
            "true",
            "",
            "refused",
            125,
            "prepare hook failed: exit status: 1",
        ),
        (
            "exit 0",
            "/nonexistent/recipe",
            "",
            "lost",
            255,
            "No such file",
        ),
        (
            "exit 1",
            "true",
            "owner-error",
            "refused",
            125,
            "session owner exited without result",
        ),
    ] {
        // Injection exists only in debug builds.
        if !fail_at.is_empty() && !cfg!(debug_assertions) {
            continue;
        }
        let dir = fixture("true", "");
        let hook = dir.path().join("prepare");
        fs::write(&hook, format!("#!/bin/sh\n{prepare}\n")).unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let toml =
            format!("version=1\n[hooks]\nprepare='prepare'\n[recipe.pass]\ncommand=['{recipe}']\n");
        fs::write(dir.path().join("ferry.toml"), toml).unwrap();
        git(dir.path(), &["add", "ferry.toml", "prepare"]);
        git(dir.path(), &["commit", "-m", "owner error"]);
        let out = command(dir.path())
            .env("FERRY_FAIL_AT", fail_at)
            .args(["run", "pass", "--local", "--json"])
            .output()
            .unwrap();
        let record = receipt(&out);
        assert_eq!(record["outcome"], outcome, "{record}");
        assert_eq!(record["exit_code"], code, "{record}");
        let text = record["reason"].as_str().unwrap_or_default();
        assert!(text.contains(reason), "{record}");
    }
}

/// An owner error carrying a hook's long stderr is capped in the record.
#[test]
fn a_long_owner_error_is_capped() {
    let dir = fixture("true", "");
    let hook = dir.path().join("environment");
    let noisy = "#!/bin/sh\nhead -c 100000 /dev/zero | tr '\\0' x >&2\nexit 1\n";
    fs::write(&hook, noisy).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let toml = "version=1\n[hooks]\nenv='environment'\n[recipe.pass]\ncommand=['true']\n";
    fs::write(dir.path().join("ferry.toml"), toml).unwrap();
    git(dir.path(), &["add", "ferry.toml", "environment"]);
    git(dir.path(), &["commit", "-m", "noisy env hook"]);
    let record = receipt(&run(dir.path(), &["run", "pass", "--local", "--json"]));
    assert_eq!(record["outcome"], "refused", "{record}");
    let reason = record["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("xxxx") && reason.ends_with("(truncated)"),
        "{reason}"
    );
    assert!(reason.len() < 5000, "{} bytes", reason.len());
}

/// ferry-8d6 (PRINCIPLES 2): with its supervisor killed (`SIGKILL`), the
/// session owner itself stops the session at the run's end: the recipe, the
/// process it started and the owner are gone soon after the timeout, not
/// when they exit, and the record reconciles to lost.
#[test]
fn a_killed_supervisor_s_session_is_stopped_at_its_timeout() {
    let dir = fixture("true", "");
    let (id, owner, recipe, guard, _caller) = unsupervised(
        dir.path(),
        "sleep 600 & echo $! > $DIR/member; echo $$ > $DIR/recipe.tmp; mv $DIR/recipe.tmp $DIR/recipe; wait",
        "",
        "8",
    );
    let started = std::time::Instant::now();
    let sleeper: i32 = fs::read_to_string(dir.path().join("member"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _sleeper = OwnedMember {
        pid: sleeper,
        session: owner,
    };
    assert!(
        alive(recipe) && alive(sleeper),
        "the session ended before its timeout"
    );
    // The timeout and the owner's lag, then TERM, which both obey.
    gone_within(
        &[recipe, sleeper, owner],
        started,
        std::time::Duration::from_secs(8) + limits::OWNER_LAG + common::slack(),
        "the session outlived its timeout without its supervisor",
    );
    let out = run(dir.path(), &["record", &id]);
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["outcome"], "lost", "{record}");
    drop(guard);
}

/// ferry-8d6 (murail-4a R1): a recipe that ends with its supervisor already
/// killed leaves a process it started; the owner stops its session then,
/// as the supervisor would have, not at the timeout and not never.
#[test]
fn an_unsupervised_recipes_leftover_is_stopped_at_its_end() {
    let dir = fixture("true", "");
    let release = dir.path().join("release");
    let (id, owner, recipe, guard, _caller) = unsupervised(
        dir.path(),
        "sleep 600 & echo $! > $DIR/member; echo $$ > $DIR/recipe.tmp; mv $DIR/recipe.tmp $DIR/recipe; while test ! -e $DIR/release; do sleep 0.1; done",
        "",
        "600",
    );
    let sleeper: i32 = fs::read_to_string(dir.path().join("member"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _sleeper = OwnedMember {
        pid: sleeper,
        session: owner,
    };
    fs::write(&release, "end").unwrap();
    // The owner polls its parent every 100 ms, then TERM, which both obey.
    gone_within(
        &[recipe, sleeper, owner],
        std::time::Instant::now(),
        common::slack(),
        "the recipe's leftover outlived its end without a supervisor",
    );
    let out = run(dir.path(), &["record", &id]);
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["outcome"], "lost", "{record}");
    drop(guard);
}

/// ferry-8d6 (murail-4a R3): with every session listing failing
/// (`session-list`), the owner still ends its session: it signals the
/// running stage itself, and once the stages have returned kills its own
/// process group, so the recipe and what it started in the background are
/// gone after the timeout, and the owner with them.
#[test]
fn an_unlistable_session_still_ends_at_its_timeout() {
    // Injection exists only in debug builds.
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let (_id, owner, recipe, guard, _caller) = unsupervised(
        dir.path(),
        "trap \"\" TERM; sleep 600 & echo $! > $DIR/member; echo $$ > $DIR/recipe.tmp; mv $DIR/recipe.tmp $DIR/recipe; while :; do sleep 0.1; done",
        "session-list",
        "6",
    );
    let started = std::time::Instant::now();
    let sleeper: i32 = fs::read_to_string(dir.path().join("member"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _sleeper = OwnedMember {
        pid: sleeper,
        session: owner,
    };
    // TERM is ignored: only the KILL after the grace ends the recipe, and
    // the owner's last act ends what it left in the background.
    gone_within(
        &[recipe, sleeper, owner],
        started,
        std::time::Duration::from_secs(6)
            + limits::OWNER_LAG
            + limits::STOP_GRACE
            + limits::KILL_GRACE
            + common::slack(),
        "an unlistable session outlived its bound",
    );
    drop(guard);
}

/// ferry-8d6 (murail-4a): the residue of an unlistable session, reported. A
/// member that moved to a process group of its own is out of the owner's
/// reach when nothing can list the session (as for the supervisor's
/// fallback): the recipe and the owner still end, and `ferry status` lists
/// the member as a leftover of its run.
#[test]
fn an_unlistable_sessions_split_group_member_is_a_reported_leftover() {
    // Injection exists only in debug builds.
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let (id, owner, recipe, guard, _caller) = unsupervised(
        dir.path(),
        "trap \"\" TERM; python3 -c \"import os,time; os.setpgid(0,0); open(\\\"$DIR/split.tmp\\\",\\\"w\\\").write(str(os.getpid())); os.rename(\\\"$DIR/split.tmp\\\",\\\"$DIR/split\\\"); time.sleep(600)\" & echo $$ > $DIR/recipe.tmp; mv $DIR/recipe.tmp $DIR/recipe; while :; do sleep 0.1; done",
        "session-list",
        "6",
    );
    let started = std::time::Instant::now();
    let split_path = dir.path().join("split");
    while !split_path.exists() {
        assert!(
            started.elapsed() < common::slack(),
            "the split member did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let split: i32 = fs::read_to_string(&split_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _split = OwnedMember {
        pid: split,
        session: owner,
    };
    gone_within(
        &[recipe, owner],
        started,
        std::time::Duration::from_secs(6)
            + limits::OWNER_LAG
            + limits::STOP_GRACE
            + limits::KILL_GRACE
            + common::slack(),
        "an unlistable session outlived its bound",
    );
    assert!(
        alive(split),
        "the residue changed: the split member was reached"
    );
    let out = run(dir.path(), &["status", "--json"]);
    let seen: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let left = &seen["leftovers"];
    let listed = left["processes"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|p| p["pid"] == split && p["run"] == id.as_str());
    // SPEC §11: listed when its marker can be read; otherwise reported as
    // unreadable or an error, never silently absent. On Linux this
    // fixture's marker can be read (an ordinary same-user process; a
    // non-dumpable one would not be), so only this pid under this run's id
    // will do: an unrelated unreadable process must not pass for it.
    if cfg!(target_os = "linux") {
        assert!(listed, "status does not list the split member: {left}");
    } else {
        assert!(
            listed || left["unreadable"].as_u64() > Some(0) || left["error"].is_string(),
            "status does not report the split member: {left}"
        );
    }
    drop(guard);
}

/// ferry-8d6 (herald-1b): the owner's stop keeps its TERM grace through a
/// listing that fails (`owner-list`, once): only a session seen empty ends
/// it early, so a member that ignores TERM is killed only after
/// `STOP_GRACE`.
#[test]
fn a_failed_listing_does_not_cut_the_owners_term_grace() {
    // Injection exists only in debug builds.
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let termed = dir.path().join("termed");
    let (_id, owner, recipe, guard, _caller) = unsupervised(
        dir.path(),
        "trap \"touch $DIR/termed\" TERM; echo $$ > $DIR/recipe.tmp; mv $DIR/recipe.tmp $DIR/recipe; while :; do sleep 0.1; done",
        "owner-list",
        "5",
    );
    let started = std::time::Instant::now();
    while !termed.exists() {
        assert!(
            started.elapsed()
                < std::time::Duration::from_secs(5) + limits::OWNER_LAG + common::slack(),
            "the owner never sent TERM"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let term = std::time::Instant::now();
    gone_within(
        &[recipe],
        term,
        limits::STOP_GRACE + limits::KILL_GRACE + common::slack(),
        "the recipe outlived the owner's KILL",
    );
    // Polling only shortens the grace that is seen, by at most a poll.
    assert!(
        term.elapsed() >= limits::STOP_GRACE.saturating_sub(std::time::Duration::from_secs(1)),
        "KILL came {:?} after TERM, before the grace",
        term.elapsed()
    );
    gone_within(
        &[owner],
        term,
        limits::STOP_GRACE + limits::KILL_GRACE + common::slack(),
        "the owner stayed",
    );
    drop(guard);
}

#[test]
#[allow(clippy::too_many_lines)]
fn dead_supervisor_refuses_seat_reuse_until_the_recorded_session_is_empty() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let dir = fixture("true", "");
    let marker = dir.path().join("recipe-started");
    let release = dir.path().join("recipe-release");
    fs::write(
        dir.path().join("ferry.toml"),
        format!(
            "version=1\n[recipe.pass]\ncommand=['sh','-c','echo $$ > {}.tmp; mv {}.tmp {}; while test ! -e {}; do sleep 0.1; done']\n",
            marker.display(), marker.display(), marker.display(), release.display()
        ),
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "hard-death fixture"]);
    let diagnostic = dir.path().join("caller-stderr");
    let caller_output = dir.path().join("caller-stdout");
    let child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .args(["run", "pass", "--local", "--json"])
        .stdout(fs::File::create(&caller_output).unwrap())
        .stderr(fs::File::create(&diagnostic).unwrap())
        .spawn()
        .unwrap();
    let mut caller = OwnedCaller {
        child: Some(child),
        member: marker.clone(),
    };
    let deadline = common::startup_deadline();
    while !marker.exists() {
        assert!(
            Instant::now() < deadline
                && caller.child.as_mut().unwrap().try_wait().unwrap().is_none(),
            "recipe did not start: {} {}",
            fs::read_to_string(&diagnostic).unwrap_or_default(),
            fs::read_to_string(&caller_output).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(10));
    }
    let recipe: i32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    let group = OwnedGroup(getsid(Some(Pid::from_raw(recipe))).unwrap().as_raw());
    let seat_lock = fs::read_dir(dir.path().join(".ferry/local/v1/locks"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|name| name.ends_with("-default"))
        .unwrap();
    let seat = seat_lock.as_str();
    // Kill the supervisor itself: the run's, named by
    // its `submitted` event. Its run reconciles to lost.
    let supervisor = fs::read_to_string(&caller_output)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["event"] == "submitted")
        .and_then(|event| event["pid"].as_i64())
        .expect("the submitted event names the supervisor");
    kill(
        Pid::from_raw(i32::try_from(supervisor).unwrap()),
        Signal::SIGKILL,
    )
    .unwrap();
    caller.child.as_mut().unwrap().wait().unwrap();
    let locks = dir.path().join(".ferry/local/v1/locks");
    let held = |name: &str| {
        let probe = fs::File::open(locks.join(name)).unwrap();
        match fs2::FileExt::try_lock_exclusive(&probe) {
            Ok(()) => false,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(e) => panic!("probing {name}: {e}"),
        }
    };
    // The session inherited the seat lock, on every platform: it stays
    // held while the recipe lives, though its supervisor is gone. The host
    // lease was the supervisor's alone and closed with it (SPEC §11).
    assert!(held(seat), "the seat lock freed while the recipe lives");
    assert!(!held("host"), "the host lease outlived its supervisor");
    // A new run on the seat refuses at once, never starting beside it: the
    // lock names the killed run, reconciled to lost, whose work holds it.
    let killed = fs::read_to_string(&caller_output)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["event"] == "submitted")
        .and_then(|event| event["id"].as_str().map(str::to_owned))
        .unwrap();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(125));
    let reason = receipt(&out)["reason"].as_str().unwrap().to_owned();
    assert!(
        reason.contains(&format!("seat busy: held after run {killed} ended (lost)")),
        "{reason}"
    );
    fs::write(&release, "finish").unwrap();
    let deadline = common::startup_deadline();
    while held(seat) {
        assert!(Instant::now() < deadline, "the recipe kept the seat");
        thread::sleep(Duration::from_millis(10));
    }
    // Once the session is gone, the seat takes the next run: no marker of
    // the dead supervisor is left to clear (slice 4b).
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    drop(group);
}

#[test]
fn an_exclusive_run_waits_for_in_flight_runs_and_keeps_new_ones_out() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let dir = fixture("true", "");
    let log = dir.path().join("lease.log");
    // Each run logs its start, waits for its own release file, logs its end.
    fs::write(
        dir.path().join("ferry.toml"),
        format!(
            "version=1\n[recipe.hold]\ncommand=['sh','-c','echo \"$1-start\" >> {log}; while [ ! -e \"{log}.$1\" ] && [ -e {log} ]; do sleep 0.05; done; echo \"$1-end\" >> {log}','hold']\n",
            log = log.display()
        ),
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "lease"]);
    let mut runs = HoldRuns { log, pids: vec![] };
    let wait_for = |runs: &HoldRuns, line: &str| {
        let deadline = common::startup_deadline();
        while !runs.logged().lines().any(|l| l == line) {
            assert!(Instant::now() < deadline, "never logged {line}");
            thread::sleep(Duration::from_millis(20));
        }
    };
    let first = runs.spawn(dir.path(), "first", false);
    wait_for(&runs, "first-start");
    let alone = runs.spawn(dir.path(), "alone", true);
    // Observe, rather than assume, that the exclusive run now holds the gate.
    let gate = dir.path().join(".ferry/local/v1/locks/host.gate");
    let deadline = common::startup_deadline();
    loop {
        // The first run already passed the gate, so it must exist. Only a
        // contended lock means the exclusive run holds it; any other error is
        // a failure, never readiness.
        let probe = fs::File::open(&gate)
            .unwrap_or_else(|e| panic!("the first run created the host gate: {e}"));
        match fs2::FileExt::try_lock_shared(&probe) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("probing the host gate: {e}"),
            Ok(()) => drop(probe),
        }
        assert!(
            Instant::now() < deadline,
            "the exclusive run never took the gate"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let gated = Instant::now();
    assert!(
        !runs.logged().contains("alone-start"),
        "exclusive run started beside an in-flight run"
    );
    let later = runs.spawn(dir.path(), "later", false);
    thread::sleep(Duration::from_millis(1500));
    assert!(
        !runs.logged().contains("later-start"),
        "a new run started while an exclusive run waited"
    );
    let held_for = gated.elapsed();
    runs.release("first");
    wait_for(&runs, "alone-start");
    thread::sleep(Duration::from_millis(500));
    assert!(
        !runs.logged().contains("later-start"),
        "a new run started beside an exclusive run"
    );
    runs.release("alone");
    wait_for(&runs, "later-start");
    runs.release("later");
    let receipts: Vec<serde_json::Value> = [first, alone, later]
        .into_iter()
        .map(|child| receipt(&child.wait_with_output().unwrap()))
        .collect();
    assert_eq!(
        runs.logged().lines().collect::<Vec<_>>(),
        [
            "first-start",
            "first-end",
            "alone-start",
            "alone-end",
            "later-start",
            "later-end"
        ]
    );
    assert_lease_windows(&receipts, held_for, dir.path());
}

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
        only_record(&dir.path().join(".ferry/local/v1/records"))["outcome"],
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
    let host = only_record(&dir.path().join(".ferry/local/v1/records"));
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
    let record = only_record(&dir.path().join(".ferry/local/v1/records"));
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
    let records = dir.path().join(".ferry/local/v1/records");
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
        .env("XDG_STATE_HOME", dir.path().join("state"))
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
    let runs = dir.path().join(".ferry/local/v1/runs");
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
    let bound = limits::STOP_GRACE + limits::DRAIN + Duration::from_secs(2) + common::slack();
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
                .join(".ferry/local/v1/runs")
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
            .join(".ferry/local/v1/records")
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
    let records = dir.path().join(".ferry/local/v1/records");
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
        .join(format!(".ferry/local/v1/runs/{id}/output.log"));
    wait_until("the output was written", || {
        fs::metadata(&log).is_ok_and(|m| m.len() >= 4_000_000)
    });
    // The supervisor this control's caller started, by its exact pid.
    kill(
        Pid::from_raw(submitted_pid(&events).unwrap()),
        Signal::SIGKILL,
    )
    .unwrap();
    let state = dir.path().join(".ferry/local/v1");
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
    let runs = dir.path().join(".ferry/local/v1/runs");
    let mut id = String::new();
    wait_until("the run paused before its spawn", || {
        fs::read_dir(&runs)
            .ok()
            .and_then(|entries| {
                entries
                    .flatten()
                    .find(|e| e.path().join("spawn.paused").exists())
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
    // The stop's signal is delivered; let the run go on to its spawn.
    thread::sleep(Duration::from_millis(200));
    fs::write(runs.join(&id).join("spawn.go"), "").unwrap();
    let mut code = None;
    wait_until("the caller ended", || {
        code = caller.0.try_wait().unwrap().map(|status| status.code());
        code.is_some()
    });
    let host = only_record(&dir.path().join(".ferry/local/v1/records"));
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
    let state = dir.path().join(".ferry/local/v1");
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
    assert_eq!(
        record["reason"], "stopped: w (first, another caller's run) with SIGINT",
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
    let state = dir.path().join(".ferry/local/v1");
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
    let records = dir.path().join(".ferry/local/v1/records");
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
    let owners = dir.path().join(".ferry/local/v1/owners");
    assert!(!owners.exists() || fs::read_dir(&owners).unwrap().count() == 0);
}

/// A busy seat refuses at once (SPEC §12.7): 125, naming the run that
/// holds it, its age and the next step. It never waits behind it.
#[test]
fn a_busy_seat_refuses_at_once_naming_the_run() {
    let scratch = tempfile::tempdir().unwrap();
    let (started, release) = (
        scratch.path().join("started"),
        scratch.path().join("release"),
    );
    let dir = fixture(
        &format!(
            "touch {}; while [ ! -e {} ]; do sleep 0.1; done",
            started.display(),
            release.display()
        ),
        "",
    );
    let holder = Caller(
        ferry(dir.path())
            .args(["run", "probe", "--local", "--timeout-seconds", "120"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("the holder's recipe started", || started.exists());
    let holder_id = only_receipt_file(dir.path())["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // A long wait would show: the refusal must come long before it.
    let asked = Instant::now();
    let out = bounded_output(ferry(dir.path()).args([
        "run",
        "probe",
        "--local",
        "--json",
        "--wait-seconds",
        "600",
    ]));
    assert!(asked.elapsed() < Duration::from_secs(10) + common::slack());
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("seat busy: run {holder_id}, submitted"))
            && reason.contains(&format!("ferry follow {holder_id}")),
        "{record}"
    );
    // A holder younger than two minutes is aged in seconds, never "0 min".
    let age = reason
        .split(", submitted ")
        .nth(1)
        .and_then(|rest| rest.split(" ago").next())
        .unwrap_or_default();
    assert!(
        age.strip_suffix(" s")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        "{reason}"
    );
    fs::write(&release, "").unwrap();
    drop(holder);
}

/// Between taking the seat lock and writing its id, the lock file still
/// names the previous, finished run (amendment A1). State cannot tell that
/// from a leftover of the finished run, so a run refused in that window
/// names both: the finished run, and the one starting (murail-4a, herald-1b).
#[test]
fn a_run_refused_while_the_holder_starts_names_both_possible_holders() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let out = ferry(dir.path())
        .args(["run", "probe", "--local"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let lock = seat_lock(dir.path());
    let previous = fs::read_to_string(&lock).unwrap();
    let holder = Caller(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "seat-lock-write")
            .args(["run", "probe", "--local", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("the holder took the lock", || {
        lock.with_extension("paused").exists()
    });
    let out = bounded_output(ferry(dir.path()).args(["run", "probe", "--local", "--json"]));
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    let reason = record["reason"].as_str().unwrap();
    let starting = fs::read_dir(dir.path().join(".ferry/local/v1/records"))
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".reserved.json").map(str::to_owned)
        })
        .find(|id| *id != record["id"])
        .unwrap();
    assert!(
        reason.contains(&format!(
            "held after run {previous} ended (ran-pass) by something it started, or by a run that is starting ({starting})"
        )),
        "{reason}"
    );
    fs::write(lock.with_extension("go"), "").unwrap();
    let mut holder = holder;
    let mut code = None;
    wait_until("the holder ended", || {
        code = holder.0.try_wait().unwrap().map(|status| status.code());
        code.is_some()
    });
    assert_eq!(code.flatten(), Some(0));
}

/// The work inherits its seat lock on every platform (SPEC §3): what a run
/// started and left running, still holding it, keeps the seat busy, and
/// the next run's refusal names the run it outlived. Both kernels: with the
/// census gone (slice 4b), Linux leaves such a process alone too.
#[test]
fn a_lock_held_after_its_run_keeps_the_seat_busy() {
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("leaver.pid");
    let dir = fixture(&leaving(&pidfile), "");
    let _leaver = Owned(pidfile.clone());
    let out = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let first = receipt(&out);
    assert_eq!(first["outcome"], "ran-pass", "{first}");
    let out = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!(
            "seat busy: held after run {} ended (ran-pass)",
            first["id"].as_str().unwrap()
        )),
        "{reason}"
    );
}

/// A reservation proves a run was submitted, not that it holds the lock
/// (murail-4a's reproduction, 5c27e351): a finished run's leftover holds
/// it while a contender has reserved but not tried it. A third run's
/// refusal must name the finished run; the contender only as a possibility.
#[test]
fn a_starting_contender_is_not_named_as_a_finished_runs_lock_holder() {
    // The pause before the seat is a debug-build point.
    if !cfg!(debug_assertions) {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("leaver.pid");
    let dir = fixture(&leaving(&pidfile), "");
    let _leaver = Owned(pidfile);
    let out = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let first = receipt(&out);
    assert_eq!(first["outcome"], "ran-pass", "{first}");
    let mut contender = Caller(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "seat-try")
            .args(["run", "probe", "--local", "--json"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let runs = dir.path().join(".ferry/local/v1/runs");
    let mut paused = PathBuf::new();
    wait_until("the contender reserved, before trying the seat", || {
        fs::read_dir(&runs)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                paused = e.path().join("seat.paused");
                paused.exists()
            })
    });
    let contender_id = paused
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let out = bounded_output(ferry(dir.path()).args(["run", "probe", "--local", "--json"]));
    fs::write(paused.with_extension("go"), "").unwrap();
    wait_until("the contender ended", || {
        contender.0.try_wait().unwrap().is_some()
    });
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!(
            "held after run {} ended (ran-pass) by something it started, or by a run that is starting ({contender_id})",
            first["id"].as_str().unwrap()
        )),
        "{reason}"
    );
}

/// Runs refused together behind a leaver each see the others' reservations
/// (critic L1): every refusal must still name the ended run whose leftover
/// holds the lock, with the others only as "or" possibilities.
#[test]
fn runs_refused_together_behind_a_leaver_each_name_the_ended_run() {
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("leaver.pid");
    let dir = fixture(&leaving(&pidfile), "");
    let _leaver = Owned(pidfile);
    let out = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let first = receipt(&out);
    assert_eq!(first["outcome"], "ran-pass", "{first}");
    let ended = format!(
        "held after run {} ended (ran-pass)",
        first["id"].as_str().unwrap()
    );
    for _ in 0..3 {
        let barrier = std::sync::Barrier::new(3);
        thread::scope(|scope| {
            let refused: Vec<_> = (0..3)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        bounded_output(
                            ferry(dir.path()).args(["run", "probe", "--local", "--json"]),
                        )
                    })
                })
                .collect();
            for out in refused {
                let record = receipt(&out.join().unwrap());
                let reason = record["reason"].as_str().unwrap();
                assert!(reason.contains(&ended), "{reason}");
            }
        });
    }
}

/// Only the seat's lock is inherited (SPEC §3, §12.9): an exclusive run's
/// leaver keeps its own seat busy, naming the run it outlived, and cannot
/// hold the host: an exclusive run on another seat is admitted while it
/// lives.
#[test]
fn an_exclusive_runs_leaver_holds_its_seat_but_not_the_host() {
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("leaver.pid");
    let dir = fixture(&leaving(&pidfile), "");
    let _leaver = Owned(pidfile);
    let out = ferry(dir.path())
        .args(["run", "probe", "--local", "--json", "--exclusive"])
        .output()
        .unwrap();
    let first = receipt(&out);
    assert_eq!(first["outcome"], "ran-pass", "{first}");
    let out = bounded_output(ferry(dir.path()).args([
        "run",
        "pass",
        "--local",
        "--json",
        "--exclusive",
        "--seat",
        "other",
        "--wait-seconds",
        "3",
    ]));
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let out = bounded_output(ferry(dir.path()).args(["run", "pass", "--local", "--json"]));
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains(&format!(
            "held after run {} ended (ran-pass)",
            first["id"].as_str().unwrap()
        )),
        "{record}"
    );
}

/// A run's record is final before its locks close (SPEC §5), so a named
/// run that is still live holds its seat (murail-4a's reproduction,
/// 2324dd77): paused just before finalizing, it still holds it.
#[test]
fn a_run_holds_its_seat_until_its_record_is_final() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let mut first = Caller(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "pre-finalize")
            .args(["run", "probe", "--local", "--json"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let (id, paused) = paused_at(&dir.path().join(".ferry/local/v1/runs"), "finalize");
    let out = bounded_output(ferry(dir.path()).args(["run", "probe", "--local", "--json"]));
    fs::write(paused.with_extension("go"), "").unwrap();
    wait_until("the first run ended", || {
        first.0.try_wait().unwrap().is_some()
    });
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("seat busy: run {id}, submitted")),
        "{reason}"
    );
}

/// The same for the host (murail-4a's 75fafcb6): an exclusive run paused
/// before finalizing still holds the host, and a shared waiter names it.
#[test]
fn an_exclusive_run_holds_the_host_until_its_record_is_final() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let mut first = Caller(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "pre-finalize")
            .args(["run", "probe", "--local", "--json", "--exclusive"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let (id, paused) = paused_at(&dir.path().join(".ferry/local/v1/runs"), "finalize");
    let out = bounded_output(ferry(dir.path()).args([
        "run",
        "probe",
        "--local",
        "--json",
        "--seat",
        "other",
        "--wait-seconds",
        "2",
    ]));
    fs::write(paused.with_extension("go"), "").unwrap();
    wait_until("the first run ended", || {
        first.0.try_wait().unwrap().is_some()
    });
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("held by exclusive run {id};")),
        "{reason}"
    );
}

/// A supervisor gone before its record is final (here a failed finalize; a
/// killed one alike, SPEC §11) closes its host locks with it: a live leaver
/// of its exclusive run holds no host, and an exclusive run on another seat
/// is admitted.
#[test]
fn a_dead_exclusive_runs_leaver_does_not_hold_the_host() {
    if !cfg!(debug_assertions) {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("leaver.pid");
    let dir = fixture(&leaving(&pidfile), "");
    let _leaver = Owned(pidfile.clone());
    let out = ferry(dir.path())
        .env("FERRY_FAIL_AT", "finalize-link")
        .env("FERRY_UNREACHABLE_MS", "2000")
        .args([
            "run",
            "probe",
            "--local",
            "--json",
            "--exclusive",
            "--seat",
            "x",
        ])
        .output()
        .unwrap();
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let reserved = dir
        .path()
        .join(format!(".ferry/local/v1/records/{id}.reserved.json"));
    let reservation: serde_json::Value =
        serde_json::from_slice(&fs::read(&reserved).unwrap()).unwrap();
    let supervisor =
        Pid::from_raw(i32::try_from(reservation["supervisor"]["pid"].as_i64().unwrap()).unwrap());
    wait_until("its supervisor exited", || kill(supervisor, None).is_err());
    let out = bounded_output(ferry(dir.path()).args([
        "run",
        "pass",
        "--local",
        "--json",
        "--exclusive",
        "--seat",
        "other",
        "--wait-seconds",
        "3",
    ]));
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let leaver: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!gone(leaver), "the leaver ended before the admitted run");
}

/// After a normal exclusive run, shared runs holding the host are proven
/// by a shared probe and named alone (critic L4), not as "after exclusive
/// run X ended".
#[test]
fn a_host_held_by_shared_runs_says_so_alone() {
    let scratch = tempfile::tempdir().unwrap();
    let (started, release) = (
        scratch.path().join("started"),
        scratch.path().join("release"),
    );
    let dir = fixture(
        &format!(
            "touch {}; while [ ! -e {} ]; do sleep 0.1; done",
            started.display(),
            release.display()
        ),
        "",
    );
    // A normal exclusive run first; it ends, leaving its name in the files.
    let out = ferry(dir.path())
        .args([
            "run",
            "pass",
            "--local",
            "--json",
            "--exclusive",
            "--seat",
            "x",
        ])
        .output()
        .unwrap();
    assert_eq!(receipt(&out)["outcome"], "ran-pass");
    let shared = Caller(
        ferry(dir.path())
            .args([
                "run",
                "probe",
                "--local",
                "--seat",
                "s",
                "--timeout-seconds",
                "60",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("the shared run started", || started.exists());
    let out = bounded_output(ferry(dir.path()).args([
        "run",
        "pass",
        "--local",
        "--json",
        "--exclusive",
        "--seat",
        "e",
        "--wait-seconds",
        "3",
    ]));
    fs::write(&release, "").unwrap();
    drop(shared);
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.ends_with(
            ": no exclusive holder at the probe: held by shared runs, or just released; retry"
        ) && !reason.contains("exclusive run"),
        "{reason}"
    );
}

/// A lock passes to the supervisor as it is taken, so a refusal after the
/// seat is held keeps it until the refusal is final (herald-1b's
/// reproduction, 08211dcd): here, a cleanup-pending marker refuses.
#[test]
fn a_prepare_refusal_keeps_its_acquired_seat_until_finalization() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let locks = dir.path().join(".ferry/local/v1/locks");
    fs::create_dir_all(&locks).unwrap();
    fs::write(locks.join("cleanup-pending-review"), "unconfirmed").unwrap();
    let mut first = Caller(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "pre-finalize")
            .args(["run", "probe", "--local", "--json"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let (_, paused) = paused_at(&dir.path().join(".ferry/local/v1/runs"), "finalize");
    let held = seat_held(dir.path());
    // Release the rendezvous before asserting, so a failure cleans up.
    fs::write(paused.with_extension("go"), "").unwrap();
    wait_until("the refused run ended", || {
        first.0.try_wait().unwrap().is_some()
    });
    assert!(
        held,
        "prepare dropped its seat lock before its refusal was final"
    );
}

/// The same when the host wait expires after the seat is held: the seat
/// stays held until the refused record is final.
#[test]
fn a_host_wait_refusal_keeps_its_seat_until_finalization() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let locks = dir.path().join(".ferry/local/v1/locks");
    fs::create_dir_all(&locks).unwrap();
    let host = Holder::exclusive(&locks.join("host"));
    let mut first = Caller(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "pre-finalize")
            .args(["run", "probe", "--local", "--json", "--wait-seconds", "1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let (_, paused) = paused_at(&dir.path().join(".ferry/local/v1/runs"), "finalize");
    let held = seat_held(dir.path());
    fs::write(paused.with_extension("go"), "").unwrap();
    wait_until("the refused run ended", || {
        first.0.try_wait().unwrap().is_some()
    });
    drop(host);
    assert!(
        held,
        "the host refusal dropped the seat before its record was final"
    );
}

/// An exclusive run that takes the gate, then gives up waiting for the
/// host lock, keeps the gate until its refusal is final (critic on
/// 785949a0): the gate is the supervisor's from the moment it is taken.
#[test]
fn a_partly_taken_host_lease_keeps_its_gate_until_finalization() {
    use fs2::FileExt;
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let locks = dir.path().join(".ferry/local/v1/locks");
    fs::create_dir_all(&locks).unwrap();
    let host = Holder::exclusive(&locks.join("host"));
    let mut first = Caller(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "pre-finalize")
            .args([
                "run",
                "probe",
                "--local",
                "--json",
                "--exclusive",
                "--wait-seconds",
                "1",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let (_, paused) = paused_at(&dir.path().join(".ferry/local/v1/runs"), "finalize");
    let gate = fs::File::open(locks.join("host.gate")).unwrap();
    let held = gate
        .try_lock_exclusive()
        .is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock);
    drop(gate);
    fs::write(paused.with_extension("go"), "").unwrap();
    wait_until("the refused run ended", || {
        first.0.try_wait().unwrap().is_some()
    });
    drop(host);
    assert!(held, "the gate closed before the refusal was final");
}

/// A finalize that fails leaves the locks to the supervisor's exit, never
/// closed while it lives with its record unwritten (murail-4a on 4e88f5a1):
/// paused after the failed finalize, it still holds its seat.
#[test]
fn a_failed_finalize_keeps_the_locks_until_the_supervisor_exits() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let mut first = Caller(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "finalize-link,finalize-failed")
            .env("FERRY_UNREACHABLE_MS", "2000")
            .args(["run", "probe", "--local", "--json"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let (_, paused) = paused_at(&dir.path().join(".ferry/local/v1/runs"), "finalize-failed");
    let held = seat_held(dir.path());
    fs::write(paused.with_extension("go"), "").unwrap();
    wait_until("the caller ended", || first.0.try_wait().unwrap().is_some());
    assert!(
        held,
        "the locks closed while the supervisor lived, its record unwritten"
    );
}

/// murail-4a's 27ca72a5, case 1: the exclusive holder releases between the
/// two probes, so the shared probe succeeds on a free lock. The reason says
/// only what that proves, never "held by shared runs" alone.
#[test]
fn an_exclusive_holder_released_between_probes_is_not_claimed_as_shared() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let root = dir.path().join(".ferry/local/v1");
    let locks = root.join("locks");
    fs::create_dir_all(&locks).unwrap();
    let host = Holder::exclusive(&locks.join("host"));
    let caller = Caller(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "between-probes")
            .args(["run", "pass", "--local", "--json", "--wait-seconds", "1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let paused = root.join("probe-gap.paused");
    wait_until("the exclusive probe failed", || paused.exists());
    drop(host);
    let (code, record) = released(caller, &paused);
    assert_eq!(code, Some(125), "{record}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        !reason.ends_with(": held by shared runs") && reason.contains("or just released"),
        "{reason}"
    );
}

/// murail-4a's 27ca72a5, case 2, under the fix: a run whose finalize
/// failed still holds its seat until it exits, so a successor cannot take
/// the seat in that window, and its refusal names the old run truthfully.
#[test]
fn a_successor_cannot_take_a_seat_whose_finalize_failed() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let mut old = Caller(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "finalize-link,finalize-failed")
            .env("FERRY_UNREACHABLE_MS", "2000")
            .args(["run", "pass", "--local", "--json"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let (old_id, paused) = paused_at(&dir.path().join(".ferry/local/v1/runs"), "finalize-failed");
    let lock = seat_lock(dir.path());
    let out = bounded_output(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "seat-lock-write")
            .args(["run", "pass", "--local", "--json"]),
    );
    let took_the_seat = lock.with_extension("paused").exists();
    if took_the_seat {
        fs::write(lock.with_extension("go"), "").unwrap();
    }
    fs::write(paused.with_extension("go"), "").unwrap();
    wait_until("the old caller ended", || {
        old.0.try_wait().unwrap().is_some()
    });
    assert!(
        !took_the_seat,
        "a successor took the seat while the old supervisor lived"
    );
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains(&format!("seat busy: run {old_id}, submitted")),
        "{record}"
    );
}

/// A panic in the supervisor after its locks are taken is caught: the
/// record is final first, then the locks close.
#[test]
fn a_supervisor_panic_keeps_its_locks_until_finalization() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let mut first = Caller(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "supervise-panic,pre-finalize")
            .args(["run", "probe", "--local", "--json"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let (id, paused) = paused_at(&dir.path().join(".ferry/local/v1/runs"), "finalize");
    let held = seat_held(dir.path());
    fs::write(paused.with_extension("go"), "").unwrap();
    wait_until("the run ended", || first.0.try_wait().unwrap().is_some());
    assert!(
        held,
        "a panic dropped the locks before the record was final"
    );
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(
            dir.path()
                .join(format!(".ferry/local/v1/records/{id}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("supervisor panicked"),
        "{record}"
    );
}

/// A host lock freed just as the wait ended is not "held by shared runs":
/// a shared probe succeeds on a free lock too (murail-4a's P1). The
/// exclusive probe proves it free, and the refusal says retry.
#[test]
fn a_host_lock_freed_as_the_wait_ends_says_so() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let (caller, host, paused) = paused_at_the_host_probe(dir.path());
    drop(host);
    let (code, record) = released(caller, &paused);
    assert_eq!(code, Some(125), "{record}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.ends_with(": the lock was released as the wait ended; retry"),
        "{reason}"
    );
}

/// A lock file the probe cannot open names no holder (murail-4a's P2): an
/// error is not evidence of one.
#[test]
fn a_host_lock_that_cannot_be_probed_names_no_holder() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let (caller, host, paused) = paused_at_the_host_probe(dir.path());
    let file = dir.path().join(".ferry/local/v1/locks/host");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    let (code, record) = released(caller, &paused);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    drop(host);
    assert_eq!(code, Some(125), "{record}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains(": holder unknown (") && !reason.contains("held"),
        "{reason}"
    );
}

/// One wait budget for all of a run's locks (critic C3): gate and host,
/// each held in turn for less than the wait, must not add up past it.
/// Every wait ends by submitted + wait, so the deadline holds by
/// construction; the run is refused naming a holder.
#[test]
fn the_lock_waits_together_end_by_one_bound() {
    use fs2::FileExt;
    let dir = fixture("echo ran", "");
    let locks = dir.path().join(".ferry/local/v1/locks");
    fs::create_dir_all(&locks).unwrap();
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
    let (gate, host) = (hold(&locks.join("host.gate")), hold(&locks.join("host")));
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
    // Each is released once the run has waited 3 s on it (its supervisor
    // says so in its log), under the 4 s wait; together over 6 s. Following
    // the run, not the clock, keeps this true on a loaded machine. (The seat
    // is never waited for: a held seat refuses at once.)
    let runs = dir.path().join(".ferry/local/v1/runs");
    let said = |name: &str| {
        fs::read_dir(&runs)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                fs::read_to_string(e.path().join("supervisor.log"))
                    .is_ok_and(|log| log.contains(&format!("waiting for lock {name}:")))
            })
    };
    let mut locks = vec![("host.gate", Some(gate)), ("host", Some(host))];
    let mut code = None;
    wait_until("the caller ended", || {
        for (name, lock) in &mut locks {
            if lock.is_some() && said(name) {
                drop(lock.take());
            }
        }
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
    // The test holds the locks itself, writing no run id. On a slow machine
    // the wait's end can meet the test's release of the gate: the place
    // then says so, and the run is refused at the same bound.
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains("held exclusively, by a run that is starting or a process outside Ferry")
            || reason.contains("the lock was released as the wait ended; retry"),
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
        reason.contains(&format!("held by exclusive run {holder_id}")),
        "{record}"
    );
    // Seconds a reader takes in, and a stop that works on another
    // caller's run as printed.
    let waited = reason.split("waited ").nth(1).unwrap_or_default();
    let seconds = waited.split(" s for lock ").next().unwrap_or_default();
    assert!(
        seconds.split_once('.').is_some_and(|(whole, tenth)| {
            !whole.is_empty()
                && tenth.len() == 1
                && format!("{whole}{tenth}")
                    .bytes()
                    .all(|b| b.is_ascii_digit())
        }),
        "{record}"
    );
    assert!(
        reason.contains(&format!(
            "ferry stop {holder_id} (another caller's run: --other --reason '<why>')"
        )),
        "{record}"
    );
    // The wait is bounded apart from the run's own timeout: the deadline is
    // submitted + wait + timeout (the place's 6 h maximum) + teardown.
    let id = record["id"].as_str().unwrap();
    let ran: serde_json::Value = serde_json::from_slice(
        &fs::read(
            dir.path()
                .join(format!(".ferry/local/v1/records/{id}.json")),
        )
        .unwrap(),
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
    // The setsid writer leaves the run's session, so the stop does not reach
    // it: it outlives the run, holding the output pipe.
    let out = bounded_output(ferry(dir.path()).args(["run", "probe", "--local", "--json"]));
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
    let host = only_record(&dir.path().join(".ferry/local/v1/records"));
    let took = since_acquired(&host);
    let bound = limits::STOP_GRACE * 2 + limits::DRAIN * 2 + common::slack();
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

/// Collect is part of the run (SPEC §10, §12.17): it sees the run's own
/// `FERRY_ID` and request file, and the run leaves exactly one run directory.
#[test]
fn the_collect_hook_sees_the_runs_own_id_and_request() {
    let dir = with_collect(
        "echo \"id=$FERRY_ID file=$FERRY_REQUEST_FILE\"; grep -q FERRY_EXIT_CODE \"$FERRY_REQUEST_FILE\" && echo collects-request || echo runs-request",
        "echo ran",
    );
    let out = bounded_output(ferry(dir.path()).args(["run", "probe", "--local", "--json"]));
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(record["collection"]["exit_code"], 0, "{record}");
    let id = record["id"].as_str().unwrap();
    let log = String::from_utf8(output_log(dir.path(), id)).unwrap();
    assert!(
        log.contains(&format!("id={id} file="))
            && log.contains(&format!("/runs/{id}/request.json\n")),
        "{log}"
    );
    assert!(log.contains("runs-request"), "{log}");
    let runs: Vec<_> = fs::read_dir(dir.path().join(".ferry/local/v1/runs"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(runs, [std::ffi::OsString::from(id)]);
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
    // Altered bytes are never fetched, and none stay on disk (SPEC §8).
    let failed = &record["artifacts_status"]["failed"];
    assert_eq!(failed["kind"], "digest", "{record}");
    assert_eq!(failed["path"], "out.txt", "{record}");
    assert!(record["artifacts"].is_null(), "{record}");
    let id = record["id"].as_str().unwrap();
    assert!(!d.join(".ferry/runs").join(id).exists(), "{record}");
}

/// A run that ends before its recipe collects nothing, and says which:
/// `none` when none are declared, else failed as not collected. A place
/// from before per-file artifacts (no list in its record) is never
/// `fetched`, even when its archive arrives.
#[test]
fn uncollected_artifacts_and_an_older_place_are_never_fetched() {
    let refused = "[hooks]\nprepare='missing-prepare'\n";
    for (artifacts, expected) in [
        ("[]", serde_json::json!("none")),
        ("['out.txt']", serde_json::json!("collection")),
    ] {
        let dir = artifact_fixture("echo built > out.txt", artifacts, refused);
        let out = ferry(dir.path())
            .args(["run", "probe", "--local", "--json"])
            .output()
            .unwrap();
        let record = receipt(&out);
        assert_eq!(record["outcome"], "refused", "{record}");
        let status = &record["artifacts_status"];
        let got = if status.is_string() {
            status.clone()
        } else {
            status["failed"]["kind"].clone()
        };
        assert_eq!(got, expected, "{record}");
    }
    if !cfg!(debug_assertions) {
        return;
    }
    // Completed again from a submitted receipt, its own records edited to
    // reach each branch: an earlier directory and list never outlive it,
    // whatever the status; a place without a list is never `fetched`.
    let dir = artifact_fixture("echo built > out.txt", "['out.txt']", "");
    let out = ferry(dir.path())
        .env("FERRY_FAIL_AT", "record-read")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let read = |path: &std::path::Path| -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    };
    let receipt_path = dir.path().join(format!(".ferry/receipts/{id}.json"));
    let place_path = dir
        .path()
        .join(format!(".ferry/local/v1/records/{id}.json"));
    let (submitted, place) = (read(&receipt_path), read(&place_path));
    let runs = dir.path().join(".ferry/runs").join(&id);
    let again = |status: Option<serde_json::Value>, list: bool| {
        let mut edited = place.clone();
        let fields = edited.as_object_mut().unwrap();
        match status {
            Some(status) => fields.insert("artifacts_status".into(), status),
            None => fields.remove("artifacts_status"),
        };
        if !list {
            fields.remove("artifacts");
        }
        fs::write(&place_path, edited.to_string()).unwrap();
        let mut earlier = submitted.clone();
        earlier["artifacts"] = serde_json::json!([{"path": "stale", "sha256": "0"}]);
        fs::write(&receipt_path, earlier.to_string()).unwrap();
        fs::create_dir_all(&runs).unwrap();
        fs::write(runs.join("stale"), "earlier").unwrap();
        let out = ferry(dir.path()).args(["record", &id]).output().unwrap();
        let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(record["outcome"], "ran-pass", "{record}");
        assert!(record["artifacts"].is_null(), "{record}");
        record["artifacts_status"]["failed"].clone()
    };
    let empty =
        serde_json::json!({"failed": {"kind": "empty-pattern", "path": "out*", "error": "e"}});
    assert_eq!(again(Some(empty), true)["kind"], "empty-pattern");
    assert!(!runs.exists());
    assert_eq!(again(Some(serde_json::json!(5)), true)["kind"], "archive");
    assert!(!runs.exists());
    let older = again(None, false);
    assert!(
        older["error"].as_str().unwrap().contains("predates"),
        "{older}"
    );
    assert!(!runs.exists());
    // A directory that cannot be emptied vouches for nothing: nothing fetched.
    let locked = runs.join("locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("stale"), "earlier").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
    fs::write(&place_path, place.to_string()).unwrap();
    fs::write(&receipt_path, submitted.to_string()).unwrap();
    let out = ferry(dir.path()).args(["record", &id]).output().unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let failed = &record["artifacts_status"]["failed"];
    assert_eq!(failed["kind"], "archive", "{record}");
    assert!(
        failed["error"].as_str().unwrap().contains("remains"),
        "{record}"
    );
    assert!(record["artifacts"].is_null(), "{record}");
    // A file where the directory goes (herald-1b's case) is no exception.
    fs::remove_dir_all(&runs).unwrap();
    fs::write(&runs, "not a directory").unwrap();
    fs::write(&receipt_path, submitted.to_string()).unwrap();
    let out = ferry(dir.path()).args(["record", &id]).output().unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        record["artifacts_status"]["failed"]["kind"], "archive",
        "{record}"
    );
}

/// Artifacts are bound file by file with a status of their own (SPEC §8):
/// fetched with each file's digest; `none` when none are declared; a pattern
/// that matches nothing, or a file in a preserved path (which may be an
/// earlier run's), is failed. The verdict stays the recipe's.
#[test]
fn artifacts_are_bound_per_file_with_a_status_apart_from_the_verdict() {
    use sha2::{Digest, Sha256};
    let run = |dir: &TempDir| {
        let out = ferry(dir.path())
            .args(["run", "probe", "--local", "--json"])
            .output()
            .unwrap();
        let record = receipt(&out);
        assert_eq!(record["outcome"], "ran-pass", "{record}");
        record
    };
    let dir = artifact_fixture("echo built > out.txt", "['out.txt']", "");
    let record = run(&dir);
    assert_eq!(record["artifacts_status"], "fetched", "{record}");
    let sha256 = Sha256::digest(b"built\n")
        .iter()
        .fold(String::new(), |hex, b| hex + &format!("{b:02x}"));
    let listed = serde_json::json!([{"path": "out.txt", "sha256": sha256}]);
    assert_eq!(record["artifacts"], listed, "{record}");
    let fetched = dir
        .path()
        .join(".ferry/runs")
        .join(record["id"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(fetched.join("out.txt")).unwrap(),
        "built\n"
    );
    let dir = artifact_fixture("true", "[]", "");
    assert_eq!(run(&dir)["artifacts_status"], "none");
    let dir = artifact_fixture("true", "['out*']", "");
    let record = run(&dir);
    let failed = &record["artifacts_status"]["failed"];
    assert_eq!(failed["kind"], "empty-pattern", "{record}");
    assert_eq!(failed["path"], "out*", "{record}");
    let dir = artifact_fixture(
        "mkdir -p keep; echo x > keep/out.txt",
        "['keep/out.txt']",
        "preserve=['keep']\n",
    );
    let record = run(&dir);
    let failed = &record["artifacts_status"]["failed"];
    assert_eq!(failed["kind"], "collection", "{record}");
    assert!(
        failed["error"].as_str().unwrap().contains("preserved"),
        "{record}"
    );
    if cfg!(debug_assertions) {
        let dir = artifact_fixture("echo built > out.txt", "['out.txt']", "");
        let out = ferry(dir.path())
            .env("FERRY_FAIL_AT", "artifact-transfer")
            .args(["run", "probe", "--local", "--json"])
            .output()
            .unwrap();
        let record = receipt(&out);
        assert_eq!(record["outcome"], "ran-pass", "{record}");
        assert_eq!(
            record["artifacts_status"]["failed"]["kind"], "transfer",
            "{record}"
        );
        let id = record["id"].as_str().unwrap();
        assert!(
            !dir.path().join(".ferry/runs").join(id).exists(),
            "{record}"
        );
    }
}

#[test]
fn a_failed_teardown_after_the_verdict_is_a_note() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo done", "");
    let out = ferry(dir.path())
        .env("FERRY_FAIL_AT", "session-term")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains("session TERM"),
        "{record}"
    );
    // KILL still followed, and nothing quarantines the host.
    assert_no_survivors(&record);
    let next = ferry(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    assert_eq!(receipt(&next)["outcome"], "ran-pass");
}

/// Locks reach the recipe, never the supervisor's other children: here
/// git, which the supervisor runs under the held locks to reset the seat
/// (moved from `custody_contract`, whose census and `ps` witness are gone).
#[cfg(target_os = "linux")]
#[test]
fn locks_reach_the_recipe_but_not_the_supervisors_other_children() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = tmp.path().join("git-fds");
    let real_git = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    fs::write(
        bin.join("git"),
        format!(
            "#!/bin/sh\nls -l /proc/$$/fd >> {} 2>&1\nexec {} \"$@\"\n",
            log.display(),
            real_git.trim()
        ),
    )
    .unwrap();
    fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    let recipe_fds = tmp.path().join("recipe-fds");
    let dir = fixture(&format!("ls -l /proc/$$/fd > {}", recipe_fds.display()), "");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let out = ferry(dir.path())
        .env("PATH", path)
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", receipt(&out));
    let recipe = fs::read_to_string(&recipe_fds).unwrap();
    let git = fs::read_to_string(&log).unwrap_or_default();
    // This run's own locks only: inside another Ferry run (a bench cell),
    // every process here also inherits that run's locks, as it should.
    let locks = dir
        .path()
        .canonicalize()
        .unwrap()
        .join(".ferry/local/v1/locks/");
    let locks = locks.to_str().unwrap();
    assert!(
        recipe.contains(locks),
        "the recipe did not inherit its seat lock"
    );
    assert!(
        !recipe.contains(&format!("{locks}host")),
        "the recipe inherited a host lock:\n{recipe}"
    );
    assert!(!git.is_empty(), "the git wrapper never ran");
    assert!(
        !git.contains(locks),
        "a supervisor child inherited a lock:\n{git}"
    );
}

/// The marker pass lists what left the session (SPEC §7, §12.9): a
/// recipe's escapee is listed by pid and command, never `[]`, and holding
/// the seat lock it keeps the seat busy, naming the run it outlived.
#[test]
fn a_recipes_escapee_is_listed_by_its_marker() {
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("escapee.pid");
    let dir = fixture(&escapee(&pidfile, false, false), "");
    let escaped = Owned(pidfile);
    let out = bounded_output(ferry(dir.path()).args(["run", "probe", "--local", "--json"]));
    let first = receipt(&out);
    assert_eq!(first["outcome"], "ran-pass", "{first}");
    let pid = escaped.pid().unwrap();
    if let Some(list) = survivors(&first) {
        assert!(
            list.iter().any(|s| s["pid"] == pid
                && s["start"].is_number()
                && s["command"].as_str().is_some_and(|c| c.contains("python"))),
            "{first}"
        );
    }
    let out = bounded_output(ferry(dir.path()).args(["run", "pass", "--local", "--json"]));
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains(&format!(
            "held after run {} ended",
            first["id"].as_str().unwrap()
        )),
        "{record}"
    );
}

/// The collect hook's escapee carries the run's own id (SPEC §12.9).
#[test]
fn a_collect_hooks_escapee_is_listed_by_its_marker() {
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("escapee.pid");
    let dir = with_collect(&escapee(&pidfile, true, false), "echo ran");
    let escaped = Owned(pidfile);
    let out = bounded_output(ferry(dir.path()).args(["run", "probe", "--local", "--json"]));
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    let pid = escaped.pid().unwrap();
    if let Some(list) = survivors(&record) {
        assert!(list.iter().any(|s| s["pid"] == pid), "{record}");
    }
}

/// An escapee that closed its fds (SPEC §12.16) is listed, and holds
/// nothing: the next run on the seat is admitted. One under `env -i` is not
/// listed: the residue, asserted so a later change cannot claim it.
#[test]
fn an_escapee_without_fds_is_listed_and_one_without_its_marker_is_not() {
    let scratch = tempfile::tempdir().unwrap();
    for cleared in [false, true] {
        let pidfile = scratch.path().join(format!("escapee-{cleared}.pid"));
        let dir = fixture(&escapee(&pidfile, true, cleared), "");
        let escaped = Owned(pidfile);
        let out = bounded_output(ferry(dir.path()).args(["run", "probe", "--local", "--json"]));
        let record = receipt(&out);
        assert_eq!(record["outcome"], "ran-pass", "{record}");
        let pid = escaped.pid().unwrap();
        // `kill(pid, 0)`, not `gone`: `gone` spawns `ps`, which the Darwin
        // Nix build cannot, and would then call a live process gone.
        assert!(
            kill(Pid::from_raw(pid), None).is_ok(),
            "the escapee (cleared {cleared}) ended early: {record}"
        );
        if let Some(list) = survivors(&record) {
            assert_eq!(list.iter().any(|s| s["pid"] == pid), !cleared, "{record}");
        }
        let out = bounded_output(ferry(dir.path()).args(["run", "pass", "--local", "--json"]));
        assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    }
}

/// Every wait in the stop path is bounded (PRINCIPLES 5): a marker pass
/// that hangs (a read blocked on a process stuck in the kernel) is abandoned
/// at `MARKER_BOUND`, and the record is final with the survivors `unknown`.
#[test]
fn a_hanging_marker_pass_is_abandoned_at_its_bound() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("echo ran", "");
    let started = Instant::now();
    let out = bounded_output(
        ferry(dir.path())
            .env("FERRY_FAIL_AT", "marker-hang")
            .args(["run", "probe", "--local", "--json"]),
    );
    let took = started.elapsed();
    let record = receipt(&out);
    assert_eq!(record["state"], "final", "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(record["custody"]["survivors"], "unknown", "{record}");
    assert!(
        record["custody"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("marker pass timed out")),
        "{record}"
    );
    assert!(took >= limits::MARKER_BOUND, "{took:?}");
    assert!(took < limits::MARKER_BOUND * 3, "{took:?}");
}

/// The pass's fallible points (SPEC §7, §12.16a): a listing that fails, an
/// eligible process that cannot be read, and one whose start changes during
/// the read each make the survivors `unknown`, never a list; the error keeps
/// what was seen, the session's survivors included.
#[test]
fn a_marker_pass_that_cannot_see_everything_is_unknown() {
    if !cfg!(debug_assertions) {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    for (points, said) in [
        ("marker-list", "list processes"),
        ("marker-read", "unreadable"),
        ("marker-restart", "start changed"),
        ("marker-owner", "owner: injected"),
        ("session-kill,marker-read", "session survivors ["),
    ] {
        let pidfile = scratch.path().join(format!("{points}.pid"));
        // Only the merge case needs a session survivor (one that ignores
        // TERM, under `session-kill`); the others run a plain recipe.
        let script = if points.starts_with("session-kill") {
            format!(
                "sh -c 'trap \"\" TERM; echo $$ > {p}.tmp; mv {p}.tmp {p}; while :; do sleep 0.2; done' </dev/null >/dev/null 2>&1 & while [ ! -e {p} ]; do sleep 0.05; done",
                p = pidfile.display()
            )
        } else {
            "echo ran".to_owned()
        };
        let dir = fixture(&script, "");
        let _member = Owned(pidfile);
        let out = bounded_output(
            ferry(dir.path())
                .env("FERRY_FAIL_AT", points)
                .args(["run", "probe", "--local", "--json"]),
        );
        let record = receipt(&out);
        assert_eq!(record["outcome"], "ran-pass", "{points}: {record}");
        assert_eq!(
            record["custody"]["survivors"], "unknown",
            "{points}: {record}"
        );
        let error = record["custody"]["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("marker pass") && error.contains(said),
            "{points}: {record}"
        );
    }
}

/// A stop ends the recipe, and collect still runs after it under the same
/// run id: nothing the stop leaves under `runs/<id>/` reaches collect.
#[test]
fn collect_runs_after_a_stop() {
    let scratch = tempfile::tempdir().unwrap();
    let (started, collected) = (
        scratch.path().join("started"),
        scratch.path().join("collected"),
    );
    let dir = with_collect(
        &format!("touch {}", collected.display()),
        &format!("touch {}; sleep 60", started.display()),
    );
    let caller = Caller(
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
    let out = bounded_output(ferry(dir.path()).args(["stop", &id]));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    drop(caller);
    let mut record = serde_json::Value::Null;
    wait_until("the record is final", || {
        let out = bounded_output(ferry(dir.path()).args(["record", &id]));
        record = serde_json::from_slice(&out.stdout).unwrap_or_default();
        record["state"] == "final"
    });
    assert!(record["stopped_by"].is_object(), "{record}");
    assert_eq!(record["collection"]["exit_code"], 0, "{record}");
    assert!(collected.exists(), "collect did not run after the stop");
}

/// A session member that ignores TERM is killed after the grace (SPEC §7,
/// `STOP_GRACE`), and is not left as a survivor.
#[test]
fn a_session_member_that_ignores_term_is_killed_after_the_grace() {
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("member.pid");
    let dir = fixture(
        &format!(
            "sh -c 'trap \"\" TERM; echo $$ > {p}.tmp; mv {p}.tmp {p}; while :; do sleep 0.2; done' </dev/null >/dev/null 2>&1 & while [ ! -e {p} ]; do sleep 0.05; done",
            p = pidfile.display()
        ),
        "",
    );
    let _member = Owned(pidfile.clone());
    let started = Instant::now();
    let out = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(record["custody"]["tier"], "session", "{record}");
    assert_no_survivors(&record);
    assert!(
        started.elapsed() >= limits::STOP_GRACE,
        "{:?}",
        started.elapsed()
    );
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(gone(pid), "the member survived");
}

/// The stop never gives up early (4b review R4): when the session cannot
/// be listed, the leader's group is signalled, the KILL still follows, the
/// member is gone, and the survivors are unknown because the final listing
/// failed.
#[test]
fn a_stop_that_cannot_list_its_session_still_kills_it() {
    if !cfg!(debug_assertions) {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("member.pid");
    let dir = fixture(
        &format!(
            "sh -c 'trap \"\" TERM; echo $$ > {p}.tmp; mv {p}.tmp {p}; while :; do sleep 0.2; done' </dev/null >/dev/null 2>&1 & while [ ! -e {p} ]; do sleep 0.05; done",
            p = pidfile.display()
        ),
        "",
    );
    let _member = Owned(pidfile.clone());
    let started = Instant::now();
    let out = ferry(dir.path())
        .env("FERRY_FAIL_AT", "session-list")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["state"], "final", "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(
        record["custody"]["survivors"],
        serde_json::json!("unknown"),
        "{record}"
    );
    assert!(
        record["custody"]["error"].as_str().is_some_and(
            |error| error.contains("session-list") && !error.contains("already reaped")
        ),
        "{record}"
    );
    assert!(
        started.elapsed() < limits::TEARDOWN,
        "{:?}",
        started.elapsed()
    );
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(gone(pid), "the member survived");
}

/// The stop never gives up early (4b review R4): one failed KILL is kept
/// as the custody error, every other member is still signalled, the leader
/// is reaped, and the member is killed again within the grace.
#[test]
fn a_failed_kill_to_one_member_still_empties_the_session() {
    if !cfg!(debug_assertions) {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("member.pid");
    let dir = fixture(
        &format!(
            "sh -c 'trap \"\" TERM; echo $$ > {p}.tmp; mv {p}.tmp {p}; while :; do sleep 0.2; done' </dev/null >/dev/null 2>&1 & while [ ! -e {p} ]; do sleep 0.05; done",
            p = pidfile.display()
        ),
        "",
    );
    let _member = Owned(pidfile.clone());
    let started = Instant::now();
    let out = ferry(dir.path())
        .env("FERRY_FAIL_AT", "session-signal-one")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["state"], "final", "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_no_survivors(&record);
    assert!(
        record["custody"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("SIGKILL") && !error.contains("already reaped")),
        "{record}"
    );
    assert!(
        started.elapsed() < limits::TEARDOWN,
        "{:?}",
        started.elapsed()
    );
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(gone(pid), "the member survived");
}

/// A survivor whose start cannot be read, though it is alive, is reported
/// with `start: null` and the error noted, never hidden (4b review R6).
#[test]
fn a_live_survivor_whose_start_cannot_be_read_is_still_reported() {
    if !cfg!(debug_assertions) {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("survivor.pid");
    let dir = fixture(
        &format!(
            "sh -c 'trap \"\" TERM; echo $$ > {p}.tmp; mv {p}.tmp {p}; while :; do sleep 0.2; done' </dev/null >/dev/null 2>&1 & while [ ! -e {p} ]; do sleep 0.05; done",
            p = pidfile.display()
        ),
        "",
    );
    let _survivor = Owned(pidfile.clone());
    let out = ferry(dir.path())
        .env("FERRY_FAIL_AT", "session-kill,survivor-start")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    let pid: i64 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The loop's own `sleep` may be listed too; every entry is kept, with
    // the start the session's listing could not read left empty.
    if let Some(list) = survivors(&record) {
        assert!(
            list.iter().any(|s| s["pid"] == pid && s["start"].is_null()),
            "{record}"
        );
    }
    assert!(
        record["custody"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("survivor-start")),
        "{record}"
    );
}

/// The stop's listing runs no subprocess: a `ps` that hangs cannot stall
/// the stop past its bounds (the critic's P2 on d502ae21). Linux; macOS reads
/// the process table through libproc.
#[cfg(target_os = "linux")]
#[test]
fn a_hanging_ps_cannot_stall_the_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let ran = tmp.path().join("ps-ran");
    fs::write(
        bin.join("ps"),
        format!("#!/bin/sh\ntouch {}\nexec sleep 120\n", ran.display()),
    )
    .unwrap();
    fs::set_permissions(bin.join("ps"), fs::Permissions::from_mode(0o755)).unwrap();
    let dir = fixture("echo ran", "");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let started = Instant::now();
    let out = ferry(dir.path())
        .env("PATH", path)
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_no_survivors(&record);
    assert!(
        started.elapsed() < limits::TEARDOWN,
        "{:?}",
        started.elapsed()
    );
    assert!(!ran.exists(), "the stop ran ps");
}

/// A survivor holding a lock (SPEC §12.9): a session member that ignores
/// TERM, with the KILL skipped, outlives the stop. The record names it, by
/// pid and start; it keeps the seat busy, and the next run's refusal names
/// the run it outlived. Once it is stopped, the seat takes runs again.
#[test]
fn a_survivor_holding_its_lock_is_named_and_keeps_the_seat_busy() {
    if !cfg!(debug_assertions) {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("survivor.pid");
    let dir = fixture(
        &format!(
            "sh -c 'trap \"\" TERM; echo $$ > {p}.tmp; mv {p}.tmp {p}; while :; do sleep 0.2; done' </dev/null >/dev/null 2>&1 & while [ ! -e {p} ]; do sleep 0.05; done",
            p = pidfile.display()
        ),
        "",
    );
    let _survivor = Owned(pidfile.clone());
    let out = ferry(dir.path())
        .env("FERRY_FAIL_AT", "session-kill")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let first = receipt(&out);
    assert_eq!(first["outcome"], "ran-pass", "{first}");
    let pid: i64 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    if let Some(list) = survivors(&first) {
        assert!(
            list.iter()
                .any(|s| s["pid"] == pid && s["start"].is_number()),
            "{first}"
        );
    }
    let out = ferry(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains(&format!(
            "held after run {} ended (ran-pass)",
            first["id"].as_str().unwrap()
        )),
        "{record}"
    );
    kill(Pid::from_raw(i32::try_from(pid).unwrap()), Signal::SIGKILL).unwrap();
    wait_until("the seat is free", || !seat_held(dir.path()));
    let out = ferry(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
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
            // Never stopped by the terminal: it ends, however loaded the
            // machine (a stopped caller would sit until the harness's bound).
            assert_eq!(code, 0, "took {took}");
        }
    }
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
    let started = || {
        let id = only_receipt_file(dir.path())["id"].as_str()?.to_owned();
        let log = dir
            .path()
            .join(format!(".ferry/local/v1/runs/{id}/output.log"));
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
    let path = dir
        .path()
        .join(format!(".ferry/local/v1/records/{id}.json"));
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
                .join(format!(".ferry/local/v1/records/{id}.reserved.json")),
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
    // The locks held through the failed finalize closed when its supervisor
    // exited: the seat takes the next run.
    let out = ferry(dir.path())
        .args(["run", "probe", "--local"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
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
            .join(format!(".ferry/local/v1/runs/{id}/output.log"))
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
            !dir.path().join(".ferry/local/v1/records").exists()
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
