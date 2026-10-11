//! Custody controls adopted from the coordinator's critic probes on 8fcfc1aa
//! (ferry-ljc.16/.17/.22): survivors after any failure, pre-existing
//! children, unstoppable survivors, inheritance boundaries, malformed taint.
//! Linux only: the census they rely on exists only there.
#![cfg(target_os = "linux")]
mod common;
use common::{ferry, git, receipt};
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

/// Kills, by exact pid, a process this test started, only if its cmdline
/// still carries the unique marker it was started with.
struct Owned {
    pidfile: PathBuf,
    marker: &'static str,
}
impl Owned {
    fn pid(&self) -> Option<i32> {
        fs::read_to_string(&self.pidfile).ok()?.trim().parse().ok()
    }
    fn alive(&self) -> bool {
        self.pid().is_some_and(|pid| {
            fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| String::from_utf8_lossy(&c).contains(self.marker))
                && !fs::read_to_string(format!("/proc/{pid}/stat"))
                    .unwrap_or_default()
                    .rsplit_once(')')
                    .is_some_and(|(_, f)| f.trim_start().starts_with('Z'))
        })
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if self.alive() {
            let pid = self.pid().unwrap();
            eprintln!("critic cleanup: SIGKILL own pid {pid} ({})", self.marker);
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A repo whose ferry.toml has recipe `pass`, recipe `probe` running `probe`,
/// and the given hooks (name -> script body, committed).
fn fixture(probe: &str, hooks: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init"]);
    git(d, &["config", "user.name", "Ferry Critic"]);
    git(d, &["config", "user.email", "critic@example.invalid"]);
    let mut config = String::from("version=1\n");
    if !hooks.is_empty() {
        config.push_str("[hooks]\n");
        for (kind, body) in hooks {
            script(d, kind, body);
            git(d, &["add", kind]);
            writeln!(config, "{kind}='{kind}'").unwrap();
        }
    }
    let recipes = toml::to_string(&serde_json::json!({"recipe": {
        "pass": {"command": ["sh", "-c", "echo committed"]},
        "probe": {"command": ["sh", "-c", probe]},
    }}))
    .unwrap();
    config.push_str(&recipes);
    fs::write(d.join("ferry.toml"), config).unwrap();
    git(d, &["add", "ferry.toml"]);
    git(d, &["commit", "-m", "fixture"]);
    dir
}

/// Starts `ferry run pass --local` and reports whether it is still running
/// after `secs` seconds (blocked); stops it by its exact pid either way.
fn next_run_blocks(dir: &Path, secs: u64) -> (bool, Option<serde_json::Value>) {
    run_blocks(dir, secs, &[])
}
fn run_blocks(dir: &Path, secs: u64, extra: &[&str]) -> (bool, Option<serde_json::Value>) {
    let mut child = ferry(dir)
        .args(["run", "pass", "--local", "--json"])
        .args(extra)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            let out = child.wait_with_output().unwrap();
            return (false, Some(receipt(&out)));
        }
        thread::sleep(Duration::from_millis(100));
    }
    // Interrupt our own blocked caller (it waits on the lock, nothing spawned).
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    );
    let _ = child.wait();
    (true, None)
}

/// An escapee daemon a hook or recipe starts: setsid, closes stdio but keeps
/// other descriptors (as `nohup`/`setsid` daemons do), writes its pid.
fn daemon_line(pidfile: &Path, marker: &str) -> String {
    format!(
        "setsid sh -c 'echo $$ > {p}; exec </dev/null >/dev/null 2>&1; exec sleep {marker}' &\nwhile [ ! -s {p} ]; do sleep 0.05; done\n",
        p = pidfile.display()
    )
}

// F1: a failure after spawn (here: the prepare hook) skips custody release:
// no census, no stop, no taint, and the escapee keeps the inherited seat lock.
#[test]
fn a_failed_prepare_hook_still_releases_custody_and_stops_its_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("daemon.pid");
    let owned = Owned {
        pidfile: pidfile.clone(),
        marker: "61.7",
    };
    let hook = format!("#!/bin/sh\n{}exit 1\n", daemon_line(&pidfile, "61.7"));
    let dir = fixture("echo recipe-ran", &[("prepare", &hook)]);
    let out = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    let alive = owned.alive();
    let taint = fs::read_dir(dir.path().join(".ferry/local/taint")).map_or(0, Iterator::count);
    let (blocked, next) = next_run_blocks(dir.path(), 5);
    let (excl_blocked, excl) = run_blocks(dir.path(), 5, &["--exclusive", "--seat", "other"]);
    let report = format!(
        "exit={:?} outcome={} reason={} custody={} daemon_alive_after_release={alive} taint_files={taint} next_run_blocked_5s={blocked} next={next:?} exclusive_other_seat_blocked_5s={excl_blocked} excl={excl:?}",
        out.status.code(),
        record["outcome"],
        record["reason"],
        record["custody"],
    );
    eprintln!("F1 {report}");
    assert!(
        !alive || taint > 0,
        "survivor of a failed prepare hook left running, untainted: {report}"
    );
    assert!(!record["custody"].is_null(), "custody skipped: {report}");
    assert!(!alive, "the daemon was not stopped: {report}");
    assert!(!blocked, "next run hangs on the seat lock: {report}");
    assert!(
        !excl_blocked,
        "an exclusive run on another seat hangs: {report}"
    );
}

// F2: a pre-existing child of the process that execs `ferry run --local`
// (a wrapper's background job) is not this run's, but the census owns it.
#[test]
fn a_preexisting_child_of_the_caller_is_not_the_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("bg.pid");
    let owned = Owned {
        pidfile: pidfile.clone(),
        marker: "62.3",
    };
    let dir = fixture("echo recipe-ran", &[]);
    let out = Command::new("sh")
        .current_dir(dir.path())
        .env_remove("GIT_DIR")
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .arg("-c")
        .arg(format!(
            "sleep 62.3 </dev/null >/dev/null 2>&1 & echo $! > {}; exec {} run pass --local --json",
            pidfile.display(),
            env!("CARGO_BIN_EXE_ferry")
        ))
        .output()
        .unwrap();
    let record = receipt(&out);
    let pid = owned.pid().unwrap();
    let alive = owned.alive();
    let report = format!(
        "bg_pid={pid} alive={alive} custody={} exit={:?}",
        record["custody"],
        out.status.code()
    );
    eprintln!("F2 {report}");
    assert!(
        alive,
        "a process the run never started was killed: {report}"
    );
    assert_eq!(record["custody"]["record"], "idle", "{report}");
}

// F3: when the census attributes no pids (blind diagnostic; on macOS the
// census is unsupported), nothing is stopped, the survivor keeps the
// inherited seat lock, and the next run hangs instead of refusing 125.
#[test]
fn an_unstoppable_survivor_taints_and_the_next_runs_refuse_instead_of_waiting() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("blind.pid");
    let owned = Owned {
        pidfile: pidfile.clone(),
        marker: "63.1",
    };
    let dir = fixture(&format!("{}exit 0\n", daemon_line(&pidfile, "63.1")), &[]);
    let out = ferry(dir.path())
        .env("FERRY_DIAGNOSTIC_PROC", "/nonexistent")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    let (blocked, next) = next_run_blocks(dir.path(), 5);
    let seat = fs::read_dir(dir.path().join(".ferry/local/locks"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| !n.starts_with("host") && !n.starts_with("cleanup"))
        .unwrap();
    let (excl_blocked, excl) = run_blocks(dir.path(), 5, &["--exclusive", "--seat", "other"]);
    let recover_live = ferry(dir.path())
        .args(["seat", "recover", &seat, "--local"])
        .output()
        .unwrap();
    drop(owned); // kill the daemon (ours, by pid)
    thread::sleep(Duration::from_millis(300));
    let recover_dead = ferry(dir.path())
        .args(["seat", "recover", &seat, "--local"])
        .output()
        .unwrap();
    let accepted = ferry(dir.path())
        .args([
            "seat",
            "recover",
            &seat,
            "--local",
            "--accept-unknown",
            "--reason",
            "daemon killed by the control",
        ])
        .output()
        .unwrap();
    let report = format!(
        "custody={} next_blocked_5s={blocked} next={next:?} recover_while_alive=({:?}, {}) recover_after_dead=({:?}, {})",
        record["custody"],
        recover_live.status.code(),
        receipt(&recover_live)["reason"],
        recover_dead.status.code(),
        receipt(&recover_dead)["reason"],
    );
    eprintln!("F3 {report}");
    assert_eq!(record["custody"]["record"], "taint", "{report}");
    assert!(
        !blocked,
        "a tainted seat hangs its next run instead of refusing 125: {report}"
    );
    let next = next.unwrap();
    assert_eq!(next["outcome"], "refused", "{report}");
    assert!(
        next["reason"].as_str().unwrap().contains("tainted"),
        "{report}"
    );
    // M1: no exclusive claim while a tainted seat's survivors may hold the lease.
    assert!(!excl_blocked, "{report}");
    assert!(
        excl.unwrap()["reason"]
            .as_str()
            .unwrap()
            .contains("cannot claim the host"),
        "{report}"
    );
    let live = receipt(&recover_live)["reason"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        live.contains("in use") && !live.contains("os error"),
        "{report}"
    );
    assert_eq!(recover_dead.status.code(), Some(125), "{report}");
    assert!(
        receipt(&recover_dead)["reason"]
            .as_str()
            .unwrap()
            .contains("--accept-unknown"),
        "{report}"
    );
    assert_eq!(accepted.status.code(), Some(0), "{}", receipt(&accepted));
    assert!(
        receipt(&accepted)["reason"]
            .as_str()
            .unwrap()
            .contains("daemon killed by the control")
    );
    let after = ferry(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    assert!(after.status.success(), "{}", receipt(&after));
}

// P1: CLOEXEC is restored after the session spawn: the supervisor's own later
// children (ps, during session teardown) do not hold the lock descriptors,
// while the recipe does.
#[test]
fn locks_reach_the_recipe_but_not_the_supervisors_other_children() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = tmp.path().join("ps-fds");
    let real_ps = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v ps"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    script(
        &bin,
        "ps",
        &format!(
            "#!/bin/sh\nls -l /proc/$$/fd >> {} 2>&1\nexec {} \"$@\"\n",
            log.display(),
            real_ps.trim()
        ),
    );
    let recipe_fds = tmp.path().join("recipe-fds");
    let dir = fixture(
        &format!("ls -l /proc/$$/fd > {}", recipe_fds.display()),
        &[],
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let out = ferry(dir.path())
        .env("PATH", path)
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", receipt(&out));
    let recipe = fs::read_to_string(&recipe_fds).unwrap();
    let ps = fs::read_to_string(&log).unwrap_or_default();
    eprintln!("P1 recipe fds:\n{recipe}\nps fds:\n{ps}");
    assert!(recipe.contains("/locks/"), "recipe did not inherit locks");
    assert!(!ps.is_empty(), "ps wrapper never ran");
    assert!(
        !ps.contains("/locks/"),
        "a supervisor child inherited a lock"
    );
}

// P2: a survivor whose TERM handler spawns a fresh setsid escapee and exits:
// the adopted grandchild must be stopped too.
#[test]
fn a_term_handler_that_spawns_an_escapee_is_still_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let p1 = tmp.path().join("p1.pid");
    let p2 = tmp.path().join("p2.pid");
    let o1 = Owned {
        pidfile: p1.clone(),
        marker: "s1.sh",
    };
    let o2 = Owned {
        pidfile: p2.clone(),
        marker: "64.9",
    };
    script(
        tmp.path(),
        "s1.sh",
        &format!(
            "#!/bin/sh\necho $$ > {p1}\nexec </dev/null >/dev/null 2>&1\ntrap 'setsid sh -c \"echo \\$\\$ > {p2}; exec sleep 64.9\" & sleep 0.2; exit 0' TERM\nwhile :; do sleep 0.1; done\n",
            p1 = p1.display(),
            p2 = p2.display()
        ),
    );
    let s1 = tmp.path().join("s1.sh");
    let dir = fixture(
        &format!(
            "setsid {} & while [ ! -s {} ]; do sleep 0.05; done; exit 0",
            s1.display(),
            p1.display()
        ),
        &[],
    );
    let out = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    let report = format!(
        "custody={} p1_alive={} p2_written={} p2_alive={}",
        record["custody"],
        o1.alive(),
        p2.exists(),
        o2.alive()
    );
    eprintln!("P2 {report}");
    assert!(p2.exists(), "TERM handler never ran: {report}");
    assert!(!o1.alive() && !o2.alive(), "{report}");
    assert_eq!(record["custody"]["record"], "idle", "{report}");
}

// P3: a malformed taint record: the run must refuse 125 (not run, not lost),
// and the refusal should say what to do.
#[test]
fn a_malformed_taint_refuses_with_its_path_and_a_next_step() {
    let dir = fixture("echo recipe-ran", &[]);
    let first = ferry(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    assert!(first.status.success());
    let seat = fs::read_dir(dir.path().join(".ferry/local/locks"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| !n.starts_with("host") && !n.starts_with("cleanup"))
        .unwrap();
    let taint = dir.path().join(format!(".ferry/local/taint/{seat}.json"));
    fs::create_dir_all(taint.parent().unwrap()).unwrap();
    fs::write(&taint, "{not json").unwrap();
    let out = ferry(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    let rec = ferry(dir.path())
        .args(["seat", "recover", &seat, "--local"])
        .output()
        .unwrap();
    let report = format!(
        "run=({:?}, {}, {}) recover=({:?}, {})",
        out.status.code(),
        record["outcome"],
        record["reason"],
        rec.status.code(),
        receipt(&rec)["reason"]
    );
    eprintln!("P3 {report}");
    assert_eq!(out.status.code(), Some(125), "{report}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("committed"),
        "{report}"
    );
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains(&taint.display().to_string()) && reason.contains("--accept-unknown"),
        "{report}"
    );
    assert_eq!(rec.status.code(), Some(125), "{report}");
    assert!(
        receipt(&rec)["reason"]
            .as_str()
            .unwrap()
            .contains("--accept-unknown"),
        "{report}"
    );
    let accepted = ferry(dir.path())
        .args([
            "seat",
            "recover",
            &seat,
            "--local",
            "--accept-unknown",
            "--reason",
            "corrupt record",
        ])
        .output()
        .unwrap();
    assert_eq!(accepted.status.code(), Some(0), "{}", receipt(&accepted));
    assert!(!taint.exists());
}

/// The internal key of the probe's seat (not the `other` seat).
fn seat_key(dir: &Path) -> String {
    fs::read_dir(dir.join(".ferry/local/locks"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| !n.starts_with("host") && !n.starts_with("cleanup") && !n.contains("other"))
        .unwrap()
}

// Round 2, C1: a grandchild of the caller's background job, orphaned while
// the run is in flight, is not the run's: the supervisor is a fresh
// host-run, so nothing outside the run can be adopted.
#[test]
fn an_orphan_of_the_callers_own_job_is_not_the_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("gc.pid");
    let owned = Owned {
        pidfile: pidfile.clone(),
        marker: "66.1",
    };
    let dir = fixture("sleep 4", &[]);
    let out = Command::new("sh")
        .current_dir(dir.path())
        .env_remove("GIT_DIR")
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .arg("-c")
        .arg(format!(
            "sh -c 'sleep 66.1 </dev/null >/dev/null 2>&1 & echo $! > {}; sleep 2; true' & exec {} run probe --local --json",
            pidfile.display(),
            env!("CARGO_BIN_EXE_ferry")
        ))
        .output()
        .unwrap();
    let record = receipt(&out);
    let alive = owned.alive();
    let report = format!(
        "alive={alive} custody={} outcome={}",
        record["custody"], record["outcome"]
    );
    eprintln!("C1 {report}");
    assert!(
        alive,
        "a grandchild of the caller's job was killed: {report}"
    );
    assert_eq!(record["outcome"], "ran-pass", "{report}");
    assert_eq!(record["custody"]["record"], "idle", "{report}");
}

// Round 2, C2: an exclusive run's unstoppable survivor may hold the gate and
// the exclusive lease; a run on another seat refuses instead of waiting.
#[test]
fn a_tainted_exclusive_survivor_makes_other_seats_refuse_not_wait() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("ex.pid");
    let owned = Owned {
        pidfile: pidfile.clone(),
        marker: "66.2",
    };
    let dir = fixture(&format!("{}exit 0\n", daemon_line(&pidfile, "66.2")), &[]);
    let out = ferry(dir.path())
        .env("FERRY_DIAGNOSTIC_PROC", "/nonexistent")
        .args(["run", "probe", "--local", "--json", "--exclusive"])
        .output()
        .unwrap();
    let record = receipt(&out);
    let taint: serde_json::Value = serde_json::from_slice(
        &fs::read(
            dir.path()
                .join(format!(".ferry/local/taint/{}.json", seat_key(dir.path()))),
        )
        .unwrap(),
    )
    .unwrap();
    let (blocked, next) = run_blocks(dir.path(), 5, &["--seat", "other"]);
    let report = format!(
        "custody={} taint={taint} alive={} other_seat_blocked_5s={blocked} next={next:?}",
        record["custody"],
        owned.alive()
    );
    eprintln!("C2 {report}");
    drop(owned);
    assert_eq!(taint["locks"]["host"], "exclusive", "{report}");
    assert_eq!(taint["locks"]["gate"], true, "{report}");
    assert!(!blocked, "another seat's run waits silently: {report}");
    let next = next.unwrap();
    assert_eq!(next["outcome"], "refused", "{report}");
    assert!(
        next["reason"]
            .as_str()
            .unwrap()
            .contains("host lease is held"),
        "{report}"
    );
}

// A shared run's unstoppable survivor holds the lease only shared: runs on
// other seats proceed.
#[test]
fn a_tainted_shared_survivor_does_not_hold_up_other_seats() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("sh.pid");
    let owned = Owned {
        pidfile: pidfile.clone(),
        marker: "67.2",
    };
    let dir = fixture(&format!("{}exit 0\n", daemon_line(&pidfile, "67.2")), &[]);
    let out = ferry(dir.path())
        .env("FERRY_DIAGNOSTIC_PROC", "/nonexistent")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    assert_eq!(receipt(&out)["custody"]["record"], "taint");
    let (blocked, next) = run_blocks(dir.path(), 5, &["--seat", "other"]);
    drop(owned);
    assert!(!blocked);
    assert_eq!(next.unwrap()["outcome"], "ran-pass");
}

// Round 2, C4: the recipe ran and passed, but custody release failed (its
// idle record cannot be written): the verdict stands, custody records the
// error, and the seat is tainted.
#[test]
fn a_failed_release_never_changes_the_verdict_of_work_that_ran() {
    let dir = fixture(
        "d=$PWD; while [ ! -d \"$d/.ferry/local\" ]; do d=$(dirname \"$d\"); done; rm -rf \"$d/.ferry/local/idle\"; : > \"$d/.ferry/local/idle\"; echo ran",
        &[],
    );
    let out = ferry(dir.path())
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    let report = format!(
        "exit={:?} outcome={} reason={} custody={}",
        out.status.code(),
        record["outcome"],
        record["reason"],
        record["custody"]
    );
    eprintln!("C4 {report}");
    assert_eq!(out.status.code(), Some(0), "{report}");
    assert_eq!(record["outcome"], "ran-pass", "{report}");
    assert!(record["custody"]["error"].is_string(), "{report}");
    assert_eq!(record["custody"]["record"], "taint", "{report}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("custody release failed"),
        "{report}"
    );
}

// R2b: a taint names the whole signalled subtree, pid and start time, and
// recovery checks every member.
#[test]
fn a_taint_names_the_whole_subtree_and_recovery_checks_every_member() {
    if !cfg!(debug_assertions) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let leader = tmp.path().join("leader.pid");
    let child = tmp.path().join("child.pid");
    let o1 = Owned {
        pidfile: leader.clone(),
        marker: "68.1",
    };
    let o2 = Owned {
        pidfile: child.clone(),
        marker: "68.2",
    };
    let dir = fixture(
        &format!(
            "setsid sh -c 'echo $$ > {l}; exec </dev/null >/dev/null 2>&1; sleep 68.2 & echo $! > {c}; exec sleep 68.1' & while [ ! -s {c} ]; do sleep 0.05; done; exit 0",
            l = leader.display(),
            c = child.display()
        ),
        &[],
    );
    let out = ferry(dir.path())
        .env("FERRY_FAIL_AT", "stop-survivors")
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["custody"]["record"], "taint", "{record}");
    let seat = seat_key(dir.path());
    let taint: serde_json::Value = serde_json::from_slice(
        &fs::read(dir.path().join(format!(".ferry/local/taint/{seat}.json"))).unwrap(),
    )
    .unwrap();
    let named: Vec<i64> = taint["survivors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["pid"].as_i64().unwrap())
        .collect();
    for owned in [&o1, &o2] {
        assert!(named.contains(&i64::from(owned.pid().unwrap())), "{taint}");
    }
    assert!(
        taint["survivors"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["start"].is_number()),
        "{taint}"
    );
    // The grandchild alone keeps recovery refused.
    drop(o1);
    thread::sleep(Duration::from_millis(300));
    let refused = ferry(dir.path())
        .args(["seat", "recover", &seat, "--local"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(125), "{}", receipt(&refused));
    drop(o2);
    thread::sleep(Duration::from_millis(300));
    let recovered = ferry(dir.path())
        .args(["seat", "recover", &seat, "--local"])
        .output()
        .unwrap();
    assert_eq!(recovered.status.code(), Some(0), "{}", receipt(&recovered));
}
