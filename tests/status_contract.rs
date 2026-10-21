//! `ferry status` (SPEC §4): one read-only view of a place.
#![allow(clippy::wildcard_imports)]
mod common;
use common::*;

/// `ferry status` (SPEC §4, §12.18) on one place with a shared run, two
/// waiting exclusive runs and a leftover: it names each, the gate's holder
/// apart from the requester, and once they end it never names a stale
/// holder. It takes no lock, so a run submitted meanwhile is not refused.
#[test]
#[allow(clippy::too_many_lines)]
fn status_names_runs_the_gate_and_leftovers() {
    let scratch = tempfile::tempdir().unwrap();
    let (release, pidfile) = (
        scratch.path().join("release"),
        scratch.path().join("left.pid"),
    );
    // `$0` is the run's first argument: `leave` starts an escapee and ends;
    // anything else holds until released.
    let dir = fixture(
        &format!(
            "if [ \"$0\" = leave ]; then {}; else while [ ! -e {} ]; do sleep 0.1; done; fi",
            escapee(&pidfile, true, false),
            release.display()
        ),
        "",
    );
    let left = Owned(pidfile);
    let run = |seat: &str, more: &[&str]| {
        let mut args = vec!["run", "probe", "--local", "--json", "--seat", seat];
        args.extend_from_slice(more);
        args.extend_from_slice(&["--wait-seconds", "120"]);
        Caller(
            ferry(dir.path())
                .args(&args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    let status = || {
        let out = bounded_output(ferry(dir.path()).args(["status", "--json"]));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let gone_run = bounded_output(ferry(dir.path()).args([
        "run", "probe", "--local", "--json", "--seat", "d", "--", "leave",
    ]));
    let leaver = receipt(&gone_run)["id"].as_str().unwrap().to_owned();
    let phase = |seen: &serde_json::Value, seat: &str| {
        seen["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| {
                r["seat"]
                    .as_str()
                    .is_some_and(|s| s.ends_with(&format!("-{seat}")))
            })
            .map(|r| {
                (
                    r["id"].as_str().unwrap().to_owned(),
                    r["phase"].as_str().unwrap().to_owned(),
                )
            })
    };
    let _shared = run("a", &[]);
    wait_until("the shared run is running", || {
        phase(&status(), "a").is_some_and(|(_, p)| p == "running")
    });
    // Status takes no lock: a run on an existing idle seat, submitted
    // between its reads, is admitted.
    let out =
        bounded_output(ferry(dir.path()).args(["run", "pass", "--local", "--json", "--seat", "e"]));
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let reading = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let reads = std::thread::spawn({
        let (dir, reading) = (dir.path().to_owned(), reading.clone());
        move || {
            while reading.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = ferry(&dir).args(["status", "--json"]).output();
            }
        }
    });
    for _ in 0..5 {
        let out = bounded_output(
            ferry(dir.path()).args(["run", "pass", "--local", "--json", "--seat", "e"]),
        );
        assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    }
    reading.store(false, std::sync::atomic::Ordering::SeqCst);
    reads.join().unwrap();
    let _holder = run("b", &["--exclusive"]);
    wait_until("the exclusive run took the gate", || {
        status()["gate"]["holder"]
            .as_str()
            .is_some_and(|h| h != "none")
    });
    let _requester = run("c", &["--exclusive"]);
    let pid = left.pid().unwrap();
    // Every fact at once, in one read: the runs change phase and the gate
    // its requesters as they start, so the read is retried until all hold
    // (or the bound ends, showing the last one).
    let holds = |seen: &serde_json::Value| {
        let (Some((b, b_phase)), Some((c, c_phase))) = (phase(seen, "b"), phase(seen, "c")) else {
            return false;
        };
        let listed = seen["leftovers"]["processes"]
            .as_array()
            .is_some_and(|p| p.iter().any(|p| p["pid"] == pid && p["run"] == leaver));
        // macOS withholds some environments: then it is counted, not listed.
        let counted =
            cfg!(target_os = "macos") && seen["leftovers"]["unreadable"].as_u64() > Some(0);
        (b_phase.as_str(), c_phase.as_str()) == ("waiting", "waiting")
            && seen["gate"]["holder"] == b
            && seen["gate"]["requesters"] == serde_json::json!([c])
            && (listed || counted)
            && seen["seats"]
                .as_array()
                .is_some_and(|s| s.iter().any(|s| s["last_run"] == leaver))
    };
    let deadline = common::startup_deadline();
    loop {
        let seen = status();
        if holds(&seen) {
            break;
        }
        assert!(Instant::now() < deadline, "never held together: {seen}");
        thread::sleep(Duration::from_millis(100));
    }
    fs::write(&release, "").unwrap();
    wait_until("every run ended", || {
        status()["runs"].as_array().unwrap().is_empty()
    });
    // `host.gate` still names the last exclusive run: never a stale holder.
    let after = status();
    assert_eq!(after["gate"]["holder"], "unknown", "{after}");
}

#[test]
fn status_lines_carry_units_and_leave_nulls_out() {
    let dir = fixture("true", "");
    // Exclusive: the gate keeps naming this run after it ends.
    let out = bounded_output(ferry(dir.path()).args(["run", "pass", "--local", "--exclusive"]));
    assert_eq!(out.status.code(), Some(0));
    let out = bounded_output(ferry(dir.path()).arg("status"));
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{text}");
    let disk = text.lines().next().unwrap();
    assert!(
        disk.starts_with("disk: ") && disk.contains("B free"),
        "{text}"
    );
    for noise in ["null", "unreadable", "\"", "_ms", "bytes", "unknown"] {
        assert!(!text.contains(noise), "{noise}: {text}");
    }
    // This caller labels its own seat with its workspace.
    let repo = fs::canonicalize(dir.path()).unwrap();
    assert!(
        text.lines()
            .any(|line| line.starts_with(&format!("seat default of {}: ", repo.display()))),
        "{text}"
    );
}
