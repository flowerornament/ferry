//! The caller and the place (SPEC §6): describe, capabilities and the pinned
//! binary, shipping objects to the store, the push and its watchdog, the
//! short-reply bound, submit and its refusals, and a caller that dies before
//! the place decides.
#![allow(clippy::wildcard_imports)]
mod common;
use common::*;

#[test]
fn internal_push_skips_hooks_and_remote_execution_receives_declared_environment() {
    let dir = fixture("true", "");
    let host = dir.path().join("host");
    let tools = dir.path().join("transport-tools");
    fs::create_dir_all(host.join(".local/bin")).unwrap();
    fs::create_dir_all(&tools).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_ferry"), host.join(".local/bin/ferry")).unwrap();
    fs::write(tools.join("ssh"), "#!/bin/sh\ncase \"$*\" in *BatchMode=yes*ControlMaster=auto*ControlPersist=10*ControlPath=*/.cache/ferry/ssh/*ConnectTimeout=10*ServerAliveInterval=5*ServerAliveCountMax=3*|*BatchMode=yes*ControlMaster=no*ControlPath=none*ConnectTimeout=10*ServerAliveInterval=5*ServerAliveCountMax=3*) ;; *) exit 125;; esac\nfor last do :; done\ncd \"$FERRY_TEST_HOST\" || exit 125\nunset APP_ALLOWED\nexport HOME=\"$FERRY_TEST_HOST\"\nexec sh -c \"$last\"\n").unwrap();
    fs::set_permissions(tools.join("ssh"), fs::Permissions::from_mode(0o755)).unwrap();
    let hook_marker = dir.path().join("pre-push-ran");
    let hook = dir.path().join(".git/hooks/pre-push");
    fs::write(
        &hook,
        format!("#!/bin/sh\ntouch '{}'\nexit 77\n", hook_marker.display()),
    )
    .unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.path().join("config/ferry");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("machines.toml"),
        format!(
            "[host.test-host]\nssh='test-host'\ntriple='{}-{}'\ncores=1\nfeatures=[]\n",
            std::env::consts::ARCH,
            if cfg!(target_os = "macos") {
                "apple-darwin"
            } else {
                "unknown-linux-gnu"
            }
        ),
    )
    .unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\n[environment]\nforwards=['APP_ALLOWED']\n[recipe.pass]\ncommand=['sh','-c','echo remote-$APP_ALLOWED \"[$*]\"','recipe']\nroutable=true\n").unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "remote contract fixture"]);
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("HOME", dir.path())
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("GIT_SSH", tools.join("ssh"))
        .env("FERRY_TEST_HOST", &host)
        .env("APP_ALLOWED", "selected")
        .args([
            "run",
            "pass",
            "--on",
            "test-host",
            "--json",
            "--timeout-seconds",
            "5",
            "--",
            "a b",
            "--seat",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!hook_marker.exists());
    assert_eq!(
        receipt(&out)["forwarded_env"],
        serde_json::json!({"APP_ALLOWED":"selected"})
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("remote-selected [a b --seat]"));
    assert_eq!(receipt(&out)["args"], serde_json::json!(["a b", "--seat"]));
    assert_eq!(receipt(&out)["host_source_rev"], SOURCE_REV);
    assert_eq!(
        receipt(&out)["host_exe"],
        host.join(".local/bin/ferry")
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    );
}

/// A `[[ship]]` url entry is served from the place's one store at its exact
/// commit: the recipe reaches it at its URL, and an ordinary clone of the
/// store fetches it, since it is kept as a head (SPEC §5 objects, §10).
#[test]
fn a_shipped_url_is_served_from_the_store_at_its_exact_commit() {
    let dir = fixture("true", "");
    let dependency = dir.path().join("dependency");
    fs::create_dir(&dependency).unwrap();
    git(&dependency, &["init"]);
    git(&dependency, &["config", "user.name", "Ferry Test"]);
    git(
        &dependency,
        &["config", "user.email", "ferry@example.invalid"],
    );
    fs::write(dependency.join("value"), "mirrored").unwrap();
    git(&dependency, &["add", "value"]);
    git(&dependency, &["commit", "-m", "dep"]);
    let out = Command::new("git")
        .current_dir(&dependency)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let sha = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    fs::write(dir.path().join("ferry.toml"), format!("version=1\n[[ship]]\nurl='https://ferry.invalid/private.git'\npath='dependency'\ncommit='{sha}'\n[recipe.mirror]\ncommand=['git','ls-remote','https://ferry.invalid/private.git']\n")).unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "ship a dependency"]);
    let out = run(dir.path(), &["run", "mirror", "--local", "--json"]);
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let shipped = serde_json::json!([{"url": "https://ferry.invalid/private.git",
        "commit": sha, "from": "dependency"}]);
    assert_eq!(receipt(&out)["ship"], shipped);
    // The recipe reached the URL (its output is on stderr with --json).
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains(&format!("refs/heads/ferry/ship/{sha}")),
        "{said}"
    );
    // The run's own served repository: an ordinary clone of it fetches the
    // commit, borrowing the store's objects.
    let slot = only_checkout(dir.path());
    let store = slot.join(".ferry/ship.git");
    let clone = dir.path().join("ordinary-clone");
    git(
        dir.path(),
        &[
            "clone",
            "--no-local",
            store.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    git(&clone, &["cat-file", "-e", &sha]);
}

/// A ref a run shipped is gone from the seat when a later run there does
/// not ship it; the seat clone's own config lists what Ferry made, and a
/// list that cannot be read refuses.
#[test]
fn a_ref_no_longer_shipped_is_removed_from_the_seat() {
    let dir = fixture("true", "");
    git(dir.path(), &["branch", "companion"]);
    fs::write(dir.path().join("ferry.toml"), "version=1\n[[ship]]\nref='refs/remotes/origin/master'\nfrom='companion'\n[recipe.pass]\ncommand=['true']\n").unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "ship a ref"]);
    assert!(
        run(dir.path(), &["run", "pass", "--local", "--json"])
            .status
            .success()
    );
    fs::write(
        dir.path().join("ferry.toml"),
        "version=1\n[recipe.pass]\ncommand=['true']\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-am", "ship nothing"]);
    assert!(
        run(dir.path(), &["run", "pass", "--local", "--json"])
            .status
            .success()
    );
    let slot = only_checkout(dir.path());
    let left = Command::new("git")
        .current_dir(&slot)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/remotes/origin/master",
        ])
        .output()
        .unwrap();
    assert!(!left.status.success(), "the old ref is still in the seat");
    // A list that cannot be read refuses: going on could keep a stale ref.
    // (Injection exists only in debug builds.)
    if !cfg!(debug_assertions) {
        return;
    }
    let out = command(dir.path())
        .env("FERRY_FAIL_AT", "ship-refs")
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["outcome"], "refused", "{record}");
    let reason = record["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("listing last run's shipped refs"),
        "{record}"
    );
}

#[test]
fn client_term_kills_children_in_another_process_group() {
    interrupted_caller_kills_children_and_exits_128_plus_signal(
        nix::sys::signal::Signal::SIGTERM,
        143,
    );
}

#[test]
fn client_interrupt_exits_130() {
    interrupted_caller_kills_children_and_exits_128_plus_signal(
        nix::sys::signal::Signal::SIGINT,
        130,
    );
}

#[test]
fn a_signal_before_dispatch_still_writes_a_receipt() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let dir = fixture("true", "");
    let marker = dir.path().join("adapter-started");
    with_adapter(
        dir.path(),
        &format!(
            "#!/bin/sh\ntouch '{}'\nsleep 2\nprintf '%s\\n' '{{\"version\":1,\"recipe\":{{\"pass\":{{\"command\":[\"sh\",\"-c\",\"echo recipe-ran\"],\"requires\":[],\"routable\":false}}}}}}'\n",
            marker.display()
        ),
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .args(["run", "pass", "--local", "--json"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = common::startup_deadline();
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(marker.exists(), "adapter never started");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let deadline = common::startup_deadline();
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(143));
    let record = receipt(&output);
    assert_eq!(record["outcome"], "lost");
    assert_eq!(record["exit_code"], 143);
    assert!(record["reason"].as_str().unwrap().contains("SIGTERM"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("recipe-ran"));
}

/// Every wait is bounded (PRINCIPLES 5, ferry-c72): a place whose SSH
/// stalls before submit (here host-info never answers) is refused within
/// the reply bound, final 125 with nothing run, and no receipt claims a run.
#[test]
fn a_stalled_place_before_submit_is_refused_within_its_bound() {
    use std::time::{Duration, Instant};
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let pidfile = dir.path().join("hung-ssh.pid");
    let started = Instant::now();
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_HANG_INFO", &pidfile)
    .env("FERRY_SSH_REPLY_MS", "2000")
    .output()
    .unwrap();
    let took = started.elapsed();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert_eq!(record["outcome"], "refused", "{record}");
    assert_eq!(record["state"], "final", "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("did not reply within"),
        "{record}"
    );
    assert!(took < Duration::from_secs(30), "{took:?}");
    // The caller stopped its own ssh at the bound.
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        kill(Pid::from_raw(pid), None).is_err(),
        "the stalled ssh outlived the caller"
    );
}

/// SPEC §12 item 25: a push that stops making progress on a live link (the
/// place's git never reads) is refused within the progress bound plus
/// slack, with no submit and no host record, and its ssh is gone.
#[test]
fn a_push_that_stalls_is_refused_within_its_progress_bound() {
    use std::time::{Duration, Instant};
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let pidfile = dir.path().join("hung-push.pid");
    let started = Instant::now();
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_HANG_PUSH", &pidfile)
    .env("FERRY_PUSH_STALL_MS", "2000")
    .output()
    .unwrap();
    let took = started.elapsed();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert_eq!(record["outcome"], "refused", "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("the push printed no progress for 2 s"),
        "{record}"
    );
    assert!(
        took < Duration::from_secs(2) + common::slack() + Duration::from_secs(10),
        "{took:?}"
    );
    let records = dir.path().join("host/.local/state/ferry/v1/records");
    assert!(
        fs::read_dir(&records).map_or(true, |mut entries| entries.next().is_none()),
        "a run was submitted"
    );
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Its group is killed with git, at once: not when the hang would end.
    let until = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(Instant::now() < until, "the stalled push's ssh outlived it");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// SPEC §12 item 25: a push that keeps printing progress for longer than the
/// bound is not cut, and its run passes.
#[test]
fn a_push_that_keeps_progressing_is_never_cut() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_TRICKLE_PUSH", "4")
    .env("FERRY_PUSH_STALL_MS", "1000")
    .output()
    .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
}

/// SPEC §6: in a process group of its own, the push no longer gets the
/// terminal's signal, so the caller that catches one stops it: a signal to
/// the caller's group (as a terminal's Ctrl-C) ends a push that would hang,
/// at once, as before dispatch, its ssh gone.
#[test]
fn a_terminal_signal_stops_a_push_in_its_own_group() {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let pidfile = dir.path().join("hung-push.pid");
    let mut child = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_HANG_PUSH", &pidfile)
    .env("FERRY_PUSH_STALL_MS", "100000")
    .process_group(0)
    .stdout(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    let caller = Pid::from_raw(i32::try_from(child.id()).unwrap());
    let deadline = common::startup_deadline();
    while !pidfile.exists() {
        assert!(Instant::now() < deadline, "the push never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let signalled = Instant::now();
    killpg(caller, Signal::SIGINT).unwrap();
    while child.try_wait().unwrap().is_none() {
        if signalled.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            panic!("the caller waited on its push after a signal");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    let record = receipt(&output);
    assert_eq!(output.status.code(), Some(130), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("before dispatch"),
        "{record}"
    );
    let until = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(Instant::now() < until, "the push's ssh outlived the signal");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// SPEC §6: git's own progress keeps a slow push alive. A real 5 MB commit
/// pushed through an ssh that passes about 320 KB/s takes several times the
/// bound, and only `git push --progress` prints meanwhile: it is not cut.
/// The bound is 4 s, room for a loaded CI runner between git's lines.
#[test]
fn a_slow_push_is_kept_alive_by_gits_own_progress() {
    use std::io::Read as _;
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let mut random = vec![0u8; 5 << 20];
    fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut random)
        .unwrap();
    fs::write(dir.path().join("large.bin"), &random).unwrap();
    git(dir.path(), &["add", "large.bin"]);
    git(dir.path(), &["commit", "-m", "a large commit"]);
    let started = std::time::Instant::now();
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_THROTTLE_PUSH", "1")
    .env("FERRY_PUSH_STALL_MS", "4000")
    .output()
    .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    // The push itself outlasted the bound several times over.
    assert!(
        started.elapsed() > std::time::Duration::from_secs(12),
        "{:?}",
        started.elapsed()
    );
}

/// A push that fails at once, then an ls-remote check that hangs: a signal
/// to the caller's group (a terminal's Ctrl-C) ends the caller at once,
/// lost with the signal and the push's cause, its ls-remote's ssh gone
/// (murail-4a's return).
#[test]
fn a_terminal_signal_stops_the_ls_remote_after_a_failed_push() {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};
    let dir = fixture("true", "");
    let pidfile = dir.path().join("hung-ls-remote.pid");
    let mut child = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_FAIL_PUSH", "1")
    .env("FERRY_TEST_HANG_LSREMOTE", &pidfile)
    .process_group(0)
    .stdout(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    let caller = Pid::from_raw(i32::try_from(child.id()).unwrap());
    let deadline = common::startup_deadline();
    while !pidfile.exists() {
        assert!(Instant::now() < deadline, "the ls-remote never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let signalled = Instant::now();
    killpg(caller, Signal::SIGINT).unwrap();
    while child.try_wait().unwrap().is_none() {
        if signalled.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            panic!("the caller waited on its ls-remote after a signal");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    let record = receipt(&output);
    assert_eq!(output.status.code(), Some(130), "{record}");
    // The signal, and the push's own cause.
    let reason = record["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("caller received SIGINT")
            && reason.contains("fatal: the place refused the push"),
        "{record}"
    );
    let until = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(
            Instant::now() < until,
            "the ls-remote's ssh outlived the signal"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A push whose ssh leaves a process holding its stderr, in the push's
/// group: the run is not delayed by it, and the leftover is gone once git
/// has exited (herald-1b's return).
#[test]
fn a_push_leaves_nothing_of_its_group_behind() {
    use std::time::{Duration, Instant};
    let dir = fixture("true", "");
    let pidfile = dir.path().join("push-leaver.pid");
    let started = Instant::now();
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_PUSH_LEAVER", &pidfile)
    .output()
    .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "{:?}",
        started.elapsed()
    );
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let guard = OwnedPid(pid);
    let until = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(Instant::now() < until, "the push's leftover outlived it");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(guard);
}

/// A push whose git's state cannot be read is never taken for exited: the
/// watchdog only reaps it, as before, and a healthy push passes (ferry-1a's
/// hold on 738fd0a4).
#[test]
fn a_push_whose_state_cannot_be_read_is_not_killed() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_FAIL_AT", "push-state")
    .env("FERRY_TEST_TRICKLE_PUSH", "1")
    .output()
    .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
}

/// An unreadable state as git exits (murail-4a's and herald-1b's combined
/// case): git is reaped without a kill while it might be alive, and its
/// group is killed after the reap all the same, so the run is not delayed
/// and nothing it left holding its stderr stays.
#[test]
fn an_unreadable_state_still_leaves_nothing_of_the_group() {
    use std::time::{Duration, Instant};
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let pidfile = dir.path().join("push-leaver.pid");
    let started = Instant::now();
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_FAIL_AT", "push-state")
    .env("FERRY_TEST_PUSH_LEAVER", &pidfile)
    .output()
    .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "{:?}",
        started.elapsed()
    );
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let guard = OwnedPid(pid);
    let until = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(Instant::now() < until, "the push's leftover outlived it");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(guard);
}

/// A push that fails names why: git's fatal line reaches the receipt.
#[test]
fn a_failed_push_names_its_fatal_line() {
    let dir = fixture("true", "");
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_FAIL_PUSH", "1")
    .output()
    .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("fatal: the place refused the push"),
        "{record}"
    );
}

/// SPEC §12 item 25: a store setup that never answers is refused within the
/// reply bound, and its ssh is gone.
#[test]
fn a_store_setup_that_never_answers_is_refused_within_the_reply_bound() {
    use std::time::{Duration, Instant};
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let pidfile = dir.path().join("hung-init.pid");
    let started = Instant::now();
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_HANG_INIT", &pidfile)
    // Room for the short replies before it (host-info) on a loaded machine:
    // only the store setup is meant to reach the bound.
    .env("FERRY_SSH_REPLY_MS", "5000")
    .output()
    .unwrap();
    let took = started.elapsed();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("did not reply within"),
        "{record}"
    );
    assert!(
        took < Duration::from_secs(5) + common::slack() + Duration::from_secs(10),
        "{took:?}"
    );
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        !alive(pid),
        "the hung store setup's ssh outlived the caller"
    );
}

/// The reply bound covers the pipes too: an ssh that exits while a
/// descendant still holds its stdout and stderr (a `ProxyCommand`, a helper)
/// is refused within the bound, never waited on to the descendant's end.
#[test]
fn an_ssh_whose_pipes_outlive_it_is_refused_within_its_bound() {
    use std::time::{Duration, Instant};
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let pidfile = dir.path().join("pipe-holder.pid");
    let started = Instant::now();
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_HOLD_PIPE", &pidfile)
    .env("FERRY_SSH_REPLY_MS", "2000")
    .output();
    let took = started.elapsed();
    // The descendant is this test's own: stop it by its exact pid.
    if let Some(pid) = fs::read_to_string(&pidfile)
        .ok()
        .and_then(|pid| pid.trim().parse::<i32>().ok())
    {
        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
    let out = out.unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert_eq!(record["state"], "final", "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("did not reply within"),
        "{record}"
    );
    assert!(took < Duration::from_secs(30), "{took:?}");
}

/// A caller killed while the place has not been asked to run anything
/// leaves no receipt saying `submitted`: nothing ran, so `ferry record`
/// has nothing to wait for.
#[test]
fn a_caller_killed_before_submit_leaves_no_submitted_receipt() {
    use std::{thread, time::Duration};
    let dir = fixture("true", "");
    let pidfile = dir.path().join("hung-ssh.pid");
    let mut caller = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_HANG_INFO", &pidfile)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()
    .unwrap();
    let deadline = common::startup_deadline();
    while !pidfile.exists() {
        assert!(std::time::Instant::now() < deadline, "host-info never ran");
        thread::sleep(Duration::from_millis(20));
    }
    caller.kill().unwrap();
    caller.wait().unwrap();
    // The hung ssh is ours: stop it by its exact pid.
    if let Ok(pid) = fs::read_to_string(&pidfile).unwrap().trim().parse::<i32>() {
        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
    let receipts = dir.path().join(".ferry/receipts");
    for entry in fs::read_dir(&receipts).into_iter().flatten().flatten() {
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
        assert_ne!(record["state"], "submitted", "{record}");
    }
}

#[test]
fn a_signal_during_the_push_stops_the_run_before_dispatch() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let dir = fixture("true", "");
    let marker = dir.path().join("pushing");
    let mut child = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_SLOW_PUSH", &marker)
    .stdout(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    let deadline = common::startup_deadline();
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(marker.exists(), "the push never started");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let deadline = common::startup_deadline();
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(143), "{stdout}");
    let record = receipt(&output);
    assert_eq!(record["outcome"], "lost");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("before dispatch")
    );
    assert!(
        !stdout.contains("\"submitted\""),
        "the host was asked to run: {stdout}"
    );
    assert!(!stdout.contains("committed"));
    assert!(
        !dir.path()
            .join("host/.local/state/ferry/v1/records")
            .exists()
    );
}

#[test]
fn a_host_refusal_before_acceptance_is_refused_not_lost() {
    let dir = fixture("true", "");
    // A file where the host keeps its run records: it cannot reserve the id.
    fs::create_dir_all(dir.path().join("host/.local/state/ferry/v1")).unwrap();
    fs::write(dir.path().join("host/.local/state/ferry/v1/records"), "").unwrap();
    let out = remote_run(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    );
    assert_eq!(
        out.status.code(),
        Some(125),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record = receipt(&out);
    assert_eq!(record["outcome"], "refused");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .starts_with("place refused before acceptance: reserving "),
        "{}",
        record["reason"]
    );
    // The place's why and next (SPEC §6), joined.
    assert!(
        record["reason"].as_str().unwrap().ends_with(&format!(
            "; ferry record {}",
            record["id"].as_str().unwrap()
        )),
        "{}",
        record["reason"]
    );
    // The receipt names the host binary that refused.
    assert_eq!(record["host_binary_sha256"].as_str().unwrap().len(), 64);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("committed"));
}

#[test]
fn a_place_exiting_125_before_replying_is_refused_not_lost() {
    let dir = fixture("true", "");
    let bin = dir.path().join("host/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    // A place whose submit fails before replying, exiting 125.
    fs::write(
        bin.join("old-ferry"),
        "#!/bin/sh\ncase \"$1\" in host-info) exec \"$HOME/.local/bin/ferry\" host-info;; *) echo 'ferry: File exists (os error 17)' >&2; exit 125;; esac\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("old-ferry"), fs::Permissions::from_mode(0o755)).unwrap();
    let out = remote_run(
        dir.path(),
        ".local/bin/old-ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    );
    assert_eq!(
        out.status.code(),
        Some(125),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let record = receipt(&out);
    assert_eq!(record["outcome"], "refused");
    assert_eq!(record["exit_code"], 125);
}

/// A place record written by the 4a binary, with the old custody and
/// admission shapes, still completes its run: `ferry record` keeps both as
/// written (4b review R3, the critic's x1).
#[test]
fn ferry_record_completes_a_run_whose_record_has_the_old_shapes() {
    let dir = fixture("true", "");
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let old_custody = serde_json::json!({"census": "unsupported", "record": "none"});
    let old_admission = serde_json::json!({
        "policy": "not-configured", "floor_bytes": null, "filesystem": null,
        "available_before": 7, "evicted": [], "skipped": {"busy": 0},
        "available_after": 7, "result": "admitted",
    });
    let place = dir
        .path()
        .join(format!(".ferry/local/v1/records/{id}.json"));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&place).unwrap()).unwrap();
    record["custody"] = old_custody.clone();
    record["admission"] = old_admission.clone();
    fs::write(&place, record.to_string()).unwrap();
    // The caller had left it submitted, as a killed caller does.
    let path = dir.path().join(format!(".ferry/receipts/{id}.json"));
    let mut stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    stored["state"] = "submitted".into();
    stored["outcome"] = "lost".into();
    stored["exit_code"] = 255.into();
    fs::write(&path, stored.to_string()).unwrap();
    let out = run(dir.path(), &["record", &id]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(after["state"], "final", "{after}");
    assert_eq!(after["outcome"], "ran-pass", "{after}");
    assert_eq!(after["custody"], old_custody, "{after}");
    assert_eq!(after["admission"], old_admission, "{after}");
}

#[test]
fn a_host_without_a_required_capability_is_refused_before_acceptance() {
    let dir = fixture("true", "");
    fs::write(
        dir.path().join("ferry.toml"),
        "version=1\n[provider]\nrequires=['admission']\n[recipe.pass]\ncommand=['sh','-c','echo committed']\nroutable=true\n",
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "requires admission"]);
    let bin = dir.path().join("host/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    // A host that can detach and ship, without the required capability.
    fs::write(
        bin.join("old-ferry"),
        "#!/bin/sh\ncase \"$1\" in host-info) \"$HOME/.local/bin/ferry\" host-info | sed 's/\"capabilities\":\\[[^]]*\\]/\"capabilities\":[\"detach\",\"ship\"]/';; *) exec \"$HOME/.local/bin/ferry\" \"$@\";; esac\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("old-ferry"), fs::Permissions::from_mode(0o755)).unwrap();
    let out = remote_run(
        dir.path(),
        ".local/bin/old-ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    );
    assert_eq!(out.status.code(), Some(125));
    let record = receipt(&out);
    assert_eq!(record["outcome"], "refused");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("lacks required capabilities"),
        "{}",
        record["reason"]
    );
    assert_eq!(record["host_binary_sha256"].as_str().unwrap().len(), 64);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("\"submitted\""));
}

#[test]
fn a_new_caller_refuses_a_place_without_detach_naming_its_revision() {
    // SPEC §12.11: an old place is refused before anything is shipped or
    // submitted, so no state root is ever shared by old and new binaries.
    let dir = fixture("true", "");
    let bin = dir.path().join("host/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        bin.join("old-ferry"),
        "#!/bin/sh\ncase \"$1\" in host-info) \"$HOME/.local/bin/ferry\" host-info | sed -e 's/\"capabilities\":\\[[^]]*\\]/\"capabilities\":[]/' -e 's/\"source_rev\":\"[^\"]*\"/\"source_rev\":\"0ld-place-rev\"/';; *) exec \"$HOME/.local/bin/ferry\" \"$@\";; esac\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("old-ferry"), fs::Permissions::from_mode(0o755)).unwrap();
    let out = remote_run(
        dir.path(),
        ".local/bin/old-ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    );
    assert_eq!(out.status.code(), Some(125));
    let record = receipt(&out);
    assert_eq!(record["outcome"], "refused");
    let reason = record["reason"].as_str().unwrap();
    assert!(reason.contains("lacks detach"), "{reason}");
    assert!(reason.contains("0ld-place-rev"), "{reason}");
    assert!(reason.contains("install this caller's ferry"), "{reason}");
    assert!(
        !dir.path()
            .join("host/.local/state/ferry/v1/records")
            .exists()
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("\"submitted\""));
}

#[test]
fn a_place_refuses_a_request_requiring_a_capability_it_lacks() {
    let dir = tempfile::tempdir().unwrap();
    let request = serde_json::json!({
        "protocol": 1, "id": "1-aa", "token": "tok", "commit": "0".repeat(40), "seat": "s",
        "recipe": {"command": ["true"]}, "hooks": {},
        "timeout_seconds": null, "forwarded_env": {},
        "provider_requires": ["teleport"]
    });
    let (code, reply) = submit(dir.path(), &request);
    assert_eq!(code, Some(125), "{reply}");
    assert!(reply.contains("teleport"), "{reply}");
    assert!(!dir.path().join("records").exists());
}

#[test]
fn a_second_submit_follows_its_own_run_and_refuses_another() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let records = state.join("records");
    fs::create_dir_all(&records).unwrap();
    let request = |id: &str, token: &str| {
        serde_json::json!({
            "protocol": 1, "id": id, "token": token, "commit": "0".repeat(40), "seat": "s",
            "recipe": {"command": ["true"]}, "hooks": {},
            "timeout_seconds": null, "forwarded_env": {}
        })
    };
    // A run still under way: its supervisor is a live process.
    let mut live = Command::new("sleep").arg("60").spawn().unwrap();
    let reservation = serde_json::json!({
        "state": "reserved", "id": "1-live", "token": "tok", "commit": "0".repeat(40),
        "seat": "s", "binary_sha256": "x", "supervisor": {"pid": live.id(), "start": null},
        "boot_id": null, "submitted_ms": 1
    });
    let reserved = records.join("1-live.reserved.json");
    fs::write(&reserved, reservation.to_string()).unwrap();
    // Another caller's request with the same id: refused.
    let (code, reply) = submit(&state, &request("1-live", "other"));
    assert_eq!(code, Some(125), "{reply}");
    assert!(reply.contains("already submitted"), "{reply}");
    // Its own caller retrying: the run it started, not a second one.
    let (code, reply) = submit(&state, &request("1-live", "tok"));
    assert_eq!(code, Some(0), "{reply}");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["pid"], live.id());
    // Another run on the seat reconciles gone supervisors, never a live one.
    let (code, reply) = submit(&state, &request("2-next", "tok"));
    assert_eq!(code, Some(0), "{reply}");
    let deadline = common::startup_deadline();
    while !records.join("2-next.json").exists() {
        assert!(std::time::Instant::now() < deadline, "2-next never final");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        fs::read_to_string(&reserved).unwrap(),
        reservation.to_string()
    );
    assert!(!records.join("1-live.json").exists());
    // A finished run: its caller gets it back, another is refused.
    let (code, reply) = submit(&state, &request("2-next", "tok"));
    assert_eq!(code, Some(0), "{reply}");
    let (code, reply) = submit(&state, &request("2-next", "other"));
    assert_eq!(code, Some(125), "{reply}");
    assert!(reply.contains("already finished"), "{reply}");
    // The test's own child, by its handle.
    live.kill().unwrap();
    live.wait().unwrap();
}

/// After the place accepted a run, only its final record gives a verdict: a
/// failure reading it (one dropped link, critic C1) leaves the receipt
/// submitted, and `ferry record` later completes it to the place's verdict.
#[test]
fn a_failure_reading_the_final_record_leaves_the_receipt_submitted() {
    let dir = fixture("true", "");
    fs::create_dir_all(dir.path().join("host")).unwrap();
    fs::write(dir.path().join("host/fail-record-once"), "").unwrap();
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .output()
    .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(255), "{record}");
    assert_eq!(record["state"], "submitted", "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains("ferry record"),
        "{record}"
    );
    let id = record["id"].as_str().unwrap();
    let out = remote_command(dir.path(), ".local/bin/ferry", &["record", id])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["state"], "final", "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
}

/// The same on the local route, which must not call a run that ran
/// "refused" (C1).
#[test]
fn a_local_failure_reading_the_final_record_leaves_the_receipt_submitted() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("FERRY_FAIL_AT", "record-read")
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(255), "{record}");
    assert_eq!(record["state"], "submitted", "{record}");
    // The place's own error, which it wrote only to stderr, is the cause.
    let reason = record["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("record-read: injected"), "{record}");
    let id = record["id"].as_str().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .args(["record", id])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["state"], "final", "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
}

/// A stop the place fails with nothing on stdout names the place's error,
/// never an empty one: here its reservation cannot be read.
#[test]
fn a_failed_stop_names_the_places_error() {
    let dir = fixture("true", "");
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let records = dir.path().join(".ferry/local/v1/records");
    fs::write(records.join(format!("{id}.reserved.json")), "not json").unwrap();
    let out = run(dir.path(), &["stop", &id]);
    assert_eq!(out.status.code(), Some(125));
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("at line 1 column"), "{said}");
}

#[test]
fn an_unreachable_place_leaves_the_receipt_submitted_until_ferry_record() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    // While this file exists the place cannot be followed (ssh fails, 255).
    let unreachable = dir.path().join("host/unreachable");
    fs::create_dir_all(dir.path().join("host")).unwrap();
    fs::write(&unreachable, "").unwrap();
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_UNREACHABLE_MS", "2000")
    .output()
    .unwrap();
    let record = receipt(&out);
    // The run may go on: no verdict is claimed.
    assert_eq!(out.status.code(), Some(255), "{record}");
    assert_eq!(record["state"], "submitted", "{record}");
    assert_eq!(record["outcome"], "lost", "{record}");
    assert!(
        record["reason"].as_str().unwrap().contains("ferry record"),
        "{record}"
    );
    // Once the place answers, the receipt follows its final record, which
    // names the binary that ran even after an install replaced it.
    fs::remove_file(&unreachable).unwrap();
    let id = record["id"].as_str().unwrap();
    let host_record = dir
        .path()
        .join(format!("host/.local/state/ferry/v1/records/{id}.json"));
    let deadline = common::startup_deadline();
    while !host_record.exists() {
        assert!(std::time::Instant::now() < deadline, "{id} never final");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let mut ran: serde_json::Value =
        serde_json::from_slice(&fs::read(&host_record).unwrap()).unwrap();
    ran["binary_sha256"] = "e".repeat(64).into();
    fs::write(&host_record, ran.to_string()).unwrap();
    let out = remote_command(dir.path(), ".local/bin/ferry", &["record", id])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["state"], "final", "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(record["host_binary_sha256"], "e".repeat(64), "{record}");
}

/// A place refused for its capabilities receives nothing first: no store and
/// no shipped objects (SPEC §12.11; mirrors were pushed before the check).
#[test]
fn an_old_place_receives_no_objects_before_its_refusal() {
    let dir = fixture("true", "");
    let dependency = dir.path().join("dependency");
    fs::create_dir(&dependency).unwrap();
    git(&dependency, &["init"]);
    git(&dependency, &["config", "user.name", "Ferry Test"]);
    git(
        &dependency,
        &["config", "user.email", "ferry@example.invalid"],
    );
    fs::write(dependency.join("value"), "dep").unwrap();
    git(&dependency, &["add", "value"]);
    git(&dependency, &["commit", "-m", "dep"]);
    fs::write(
        dir.path().join(".gitignore"),
        "dependency/\nhost/\nconfig/\nstate/\ntransport-tools/\n",
    )
    .unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\n[[ship]]\nurl='https://ferry.invalid/dep.git'\npath='dependency'\ncommit='HEAD'\n[recipe.pass]\ncommand=['true']\nroutable=true\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "ship a dependency"]);
    let bin = dir.path().join("host/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        bin.join("old-ferry"),
        "#!/bin/sh\ncase \"$1\" in host-info) \"$HOME/.local/bin/ferry\" host-info | sed -e 's/\"capabilities\":\\[[^]]*\\]/\"capabilities\":[]/';; *) exec \"$HOME/.local/bin/ferry\" \"$@\";; esac\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("old-ferry"), fs::Permissions::from_mode(0o755)).unwrap();
    let out = remote_run(
        dir.path(),
        ".local/bin/old-ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    );
    assert_eq!(out.status.code(), Some(125), "{}", receipt(&out));
    assert!(
        !dir.path().join("host/.local/state").exists(),
        "the refused place received state"
    );
}

/// The immediate predecessor (b2756526) advertises detach and retire but
/// cannot read `ship`: it is refused before anything is pushed, naming its
/// revision (SPEC §12.11; herald-1b's amendment to slice 6).
#[test]
fn a_place_without_ship_is_refused_before_any_push() {
    let dir = fixture("true", "");
    let bin = dir.path().join("host/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        bin.join("old-ferry"),
        "#!/bin/sh\ncase \"$1\" in host-info) \"$HOME/.local/bin/ferry\" host-info | sed -e 's/\"capabilities\":\\[[^]]*\\]/\"capabilities\":[\"detach\",\"retire\"]/' -e 's/\"source_rev\":\"[^\"]*\"/\"source_rev\":\"0ld-place-rev\"/';; *) exec \"$HOME/.local/bin/ferry\" \"$@\";; esac\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("old-ferry"), fs::Permissions::from_mode(0o755)).unwrap();
    let out = remote_run(
        dir.path(),
        ".local/bin/old-ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    );
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains("lacks ship") && reason.contains("0ld-place-rev"),
        "{reason}"
    );
    assert!(
        !dir.path().join("host/.local/state").exists(),
        "the refused place received state"
    );
}

/// A recipe sees its commit and its own declared ship entries, nothing else:
/// another seat's shipped dependency is in none of its branches, refs or
/// served URLs (the critic's l1 on slice 6).
#[test]
fn a_recipe_sees_only_its_own_ship_entries() {
    let dir = fixture("true", "");
    fs::write(dir.path().join(".gitignore"), "deps/\nconfig/\nstate/\n").unwrap();
    let a = dependency(dir.path(), "deps/a");
    let b = dependency(dir.path(), "deps/b");
    let probe = "git branch -r; git for-each-ref --format=\"%(refname)\"; git ls-remote https://example.invalid/org/b";
    for (seat, name, sha) in [("one", "a", &a), ("two", "b", &b)] {
        fs::write(dir.path().join("ferry.toml"), format!("version=1\n[[ship]]\nurl='https://example.invalid/org/{name}'\npath='deps/{name}'\ncommit='{sha}'\n[recipe.probe]\ncommand=['sh','-c','{probe}']\n")).unwrap();
        git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
        git(dir.path(), &["commit", "-qm", &format!("ship {name}")]);
        let out = run(
            dir.path(),
            &["run", "probe", "--local", "--seat", seat, "--json"],
        );
        let seen = String::from_utf8_lossy(&out.stderr).into_owned();
        if seat == "two" {
            assert!(out.status.success(), "{seen}");
            assert!(
                seen.contains(&format!("refs/heads/ferry/ship/{b}")),
                "{seen}"
            );
            assert!(
                !seen.contains(&a),
                "seat two sees seat one's dependency: {seen}"
            );
        }
        assert!(
            !seen.contains("origin/ferry/ship"),
            "a ship entry was cloned: {seen}"
        );
    }
}

/// Two first runs on a fresh root, shipping the same object, never refuse:
/// the store's init and the object's ref may race, and both are benign
/// (the critic's l2 on slice 6).
#[test]
fn concurrent_first_runs_shipping_one_object_never_refuse() {
    for round in 0..4 {
        let dir = fixture("true", "");
        fs::write(dir.path().join("ferry.toml"), format!("version=1\n[[ship]]\nref='refs/heads/dep'\nfrom='HEAD'\n[recipe.probe]\ncommand=['git','rev-parse','refs/heads/dep']\n# {round}\n")).unwrap();
        git(dir.path(), &["add", "ferry.toml"]);
        git(dir.path(), &["commit", "-qm", "ship a ref"]);
        let spawn = |seat: &str| {
            Command::new(env!("CARGO_BIN_EXE_ferry"))
                .current_dir(dir.path())
                .env("XDG_CONFIG_HOME", dir.path().join("config"))
                .env("XDG_STATE_HOME", dir.path().join("state"))
                .args(["run", "probe", "--local", "--seat", seat, "--json"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        let (one, two) = (spawn("one"), spawn("two"));
        for child in [one, two] {
            let out = child.wait_with_output().unwrap();
            let record = receipt(&out);
            assert_eq!(record["outcome"], "ran-pass", "round {round}: {record}");
        }
    }
}

/// M1 keeps its state in `v1` inside the old root, which pre-M1 binaries
/// never look into, and touches nothing of an old root it finds: old and
/// new never share a state root (SPEC §6, §9, §12.11).
#[test]
fn an_old_root_is_never_shared() {
    let dir = fixture("true", "");
    let old = dir.path().join(".ferry/local");
    fs::create_dir_all(old.join("locks")).unwrap();
    fs::create_dir_all(old.join("slots/old-seat")).unwrap();
    fs::write(old.join("slots/old-seat/kept"), "an old binary's").unwrap();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(out.status.success(), "{}", receipt(&out));
    assert_eq!(fs::read_dir(old.join("locks")).unwrap().count(), 0);
    assert!(old.join("slots/old-seat/kept").exists());
    assert!(only_checkout(dir.path()).join("ferry.toml").exists());
}

/// Every caller before slice 7 requires `cores` in host-info: an upgraded
/// place still emits it, so such a caller parses the reply and refuses
/// naming the revision, never lost (SPEC §12.11; the critic's m2).
#[test]
fn host_info_still_carries_what_older_callers_require() {
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("host-info")
        .output()
        .unwrap();
    let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        info["cores"].as_u64().is_some_and(|cores| cores > 0),
        "{info}"
    );
}

/// A request this place cannot parse (an older caller's shape) is refused
/// naming this place's revision, so the caller knows what to install
/// (SPEC §12.11; the critic's m2, run by hand with the real d1b8984f caller).
#[test]
fn an_unparsable_request_is_refused_naming_the_places_revision() {
    let dir = tempfile::tempdir().unwrap();
    let request = serde_json::json!({
        "protocol": 1, "id": "1-aa", "token": "tok", "commit": "0".repeat(40), "seat": "s",
        "recipe": {"command": ["true"]}, "hooks": {}, "caller_binary_sha256": "x",
        "timeout_seconds": null, "forwarded_env": {}
    });
    let (code, reply) = submit(dir.path(), &request);
    assert_eq!(code, Some(125), "{reply}");
    assert!(reply.contains("this place runs "), "{reply}");
    assert!(!dir.path().join("records").exists());
}

/// SPEC §3 (ferry-ryl): on a remote route, `needs ⊆ has` where `has` is
/// the declared `has`, what the place reports it detects, and its Ferry's
/// capabilities. A capability needs no declaration; a platform the
/// caller's `machines.toml` claims but the place does not report is
/// refused, naming what is missing; a need nothing could supply is refused
/// before the place is asked.
#[test]
fn needs_are_checked_against_what_the_place_reports() {
    let dir = fixture("true", "");
    let (here, other) = if cfg!(target_os = "macos") {
        ("macos", "linux")
    } else {
        ("linux", "macos")
    };
    with_project(
        dir.path(),
        &format!(
            "version = 2\n[recipe.cap]\ncommand = ['true']\nneeds = ['status']\nmay_run_elsewhere = true\n[recipe.other]\ncommand = ['true']\nneeds = ['{other}']\nmay_run_elsewhere = true\n[recipe.gpu]\ncommand = ['true']\nneeds = ['gpu']\nmay_run_elsewhere = true\n"
        ),
    );
    let run_on = |recipe: &str, platform: Option<&str>| {
        let mut command = remote_command(
            dir.path(),
            ".local/bin/ferry",
            &["run", recipe, "--on", "test-host", "--json"],
        );
        if let Some(platform) = platform {
            with_machines(
                dir.path(),
                &format!(
                    "version = 2\n[machine.test-host]\nssh = 'test-host'\nbinary = '.local/bin/ferry'\nplatform = '{platform}'\nhas = []\n"
                ),
            );
        }
        let out = command.output().unwrap();
        (out.status.code(), receipt(&out))
    };
    // A Ferry capability: no declaration needed; the place confirms it.
    let (code, record) = run_on("cap", None);
    assert_eq!(code, Some(0), "{record}");
    // The caller's file says the other OS; the place reports its own.
    let claimed = if other == "linux" {
        "x86_64-unknown-linux-gnu"
    } else {
        "aarch64-apple-darwin"
    };
    let (code, record) = run_on("other", Some(claimed));
    assert_eq!(code, Some(125), "{record}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("machine test-host does not have {other} (it has "))
            && reason.contains(here),
        "{record}"
    );
    // Nothing could supply it: refused before the place is asked.
    let (code, record) = run_on("gpu", Some(claimed));
    assert_eq!(code, Some(125), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .starts_with("machine test-host does not have gpu (it has "),
        "{record}"
    );
    assert!(record["host_source_rev"].is_null(), "{record}");
}

/// SPEC §12 item 28 (ferry-ljc.2): the place's binary is replaced after
/// `describe` answered and before `submit` (during the push). The run is
/// refused, naming the change, with no reservation and no record on the
/// place; a rerun passes on the new binary, and its receipt names it.
#[test]
fn a_binary_flipped_between_describe_and_submit_is_refused_then_runs() {
    let dir = fixture("true", "");
    let marker = dir.path().join("pushing");
    let command = |slow: bool| {
        let mut command = remote_command(
            dir.path(),
            ".local/bin/ferry",
            &["run", "pass", "--on", "test-host", "--json"],
        );
        if slow {
            command.env("FERRY_TEST_SLOW_PUSH", &marker);
        }
        command
    };
    // Built before the run: the flip itself is one rename, between the
    // push's start and its end, never during submit.
    let bin = dir.path().join("host/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    if !another_build(&dir.path().join("flipped")) {
        eprintln!("skipped: no codesign here");
        return;
    }
    let flipped = sha256(&fs::read(dir.path().join("flipped")).unwrap());
    let caller = command(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = common::startup_deadline();
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline, "no push");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // describe has answered (the push follows it): flip, by rename.
    fs::copy(dir.path().join("flipped"), bin.join(".ferry.flip")).unwrap();
    fs::rename(bin.join(".ferry.flip"), bin.join("ferry")).unwrap();
    let out = caller.wait_with_output().unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("the place's ferry changed since it was asked; run again"),
        "{record}"
    );
    let id = record["id"].as_str().unwrap();
    let records = dir.path().join("host/.local/state/ferry/v1/records");
    for name in [format!("{id}.json"), format!("{id}.reserved.json")] {
        assert!(!records.join(&name).exists(), "{name}: {record}");
    }
    // A rerun asks again, meets one binary and names it. (remote_command
    // stages this build again: put the flipped one back after it.)
    let mut rerun = command(false);
    fs::copy(dir.path().join("flipped"), bin.join(".ferry.flip")).unwrap();
    fs::rename(bin.join(".ferry.flip"), bin.join("ferry")).unwrap();
    let out = rerun.output().unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert_eq!(record["host_binary_sha256"], flipped.as_str(), "{record}");
}

/// SPEC §12 item 28: a request with `binary_sha256` goes only to a place
/// advertising `pinned`; one from before it would refuse the field.
#[test]
fn a_place_without_pinned_is_never_sent_the_digest() {
    let dir = fixture("true", "");
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_TEST_UNPIN", "1")
    .output()
    .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    let id = record["id"].as_str().unwrap();
    let request: serde_json::Value = serde_json::from_slice(
        &fs::read(
            dir.path()
                .join(format!("host/.local/state/ferry/v1/runs/{id}/request.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(request.get("binary_sha256").is_none(), "{request}");
    // With `pinned`, it is sent: describe's digest.
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .output()
    .unwrap();
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let request: serde_json::Value = serde_json::from_slice(
        &fs::read(
            dir.path()
                .join(format!("host/.local/state/ferry/v1/runs/{id}/request.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        request["binary_sha256"],
        sha256(&fs::read(env!("CARGO_BIN_EXE_ferry")).unwrap()).as_str(),
        "{request}"
    );
}

/// The place's own check (SPEC §6): a request naming another digest is
/// refused before anything is reserved; a local run carries the caller's
/// own, which its place (the same file) always matches.
#[test]
fn a_request_for_another_binary_is_refused_before_reserving() {
    let dir = fixture("true", "");
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    let state = dir.path().join(".ferry/local/v1");
    let id = record["id"].as_str().unwrap();
    let mut request: serde_json::Value =
        serde_json::from_slice(&fs::read(state.join(format!("runs/{id}/request.json"))).unwrap())
            .unwrap();
    assert_eq!(
        request["binary_sha256"], record["caller_binary_sha256"],
        "{request}"
    );
    request["id"] = "1-another".into();
    request["binary_sha256"] = "0".repeat(64).into();
    let mut submit = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["place", "--state"])
        .arg(&state)
        .arg("submit")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        submit.stdin.as_mut().unwrap(),
        format!("{request}\n").as_bytes(),
    )
    .unwrap();
    let out = submit.wait_with_output().unwrap();
    let reply = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(125), "{reply}");
    assert!(reply.contains("changed since it was asked"), "{reply}");
    assert!(!state.join("records/1-another.reserved.json").exists());
    assert!(!state.join("runs/1-another").exists());
}
