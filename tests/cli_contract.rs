//! What the caller says (SPEC §4): its lines and events, help, the commands
//! that read records, and its refusals in words.
#![allow(clippy::wildcard_imports)]
mod common;
use common::*;

#[test]
fn the_caller_says_where_the_run_went_and_ends_with_its_verdict() {
    let dir = fixture("echo out; exit 17", "");
    let out = bounded_output(ferry(dir.path()).args(["run", "probe", "--local"]));
    assert_eq!(out.status.code(), Some(17));
    let lines = said(&out);
    let record = only_receipt_file(dir.path());
    let (id, commit) = (
        record["id"].as_str().unwrap(),
        record["commit"].as_str().unwrap(),
    );
    assert_eq!(
        lines.first().map(String::as_str),
        Some(
            format!(
                "ferry: {id} → local, seat default, commit {}",
                short(commit)
            )
            .as_str()
        ),
        "{lines:?}"
    );
    assert_eq!(
        lines.last().map(String::as_str),
        Some(format!("ferry: ran-fail, exit 17, run {id}").as_str()),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("uncommitted")),
        "{lines:?}"
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");

    // An uncommitted edit is named, and the commit it is not in still runs.
    fs::write(dir.path().join("edit"), "not committed").unwrap();
    let out = bounded_output(ferry(dir.path()).args(["run", "pass", "--local"]));
    assert_eq!(out.status.code(), Some(0));
    assert!(
        said(&out).contains(&format!(
            "ferry: uncommitted changes are not in {}",
            short(commit)
        )),
        "{:?}",
        said(&out)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "committed\n");

    // With --json, stdout is events only: submitted (where), then receipt.
    let out = bounded_output(ferry(dir.path()).args(["run", "pass", "--local", "--json"]));
    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0]["event"], "submitted", "{events:?}");
    assert_eq!(events[0]["machine"], "local", "{events:?}");
    assert_eq!(events[0]["seat"], "default", "{events:?}");
    assert_eq!(events[0]["commit"], commit, "{events:?}");
    assert_eq!(events[1]["event"], "receipt", "{events:?}");
}

#[test]
fn a_run_waiting_on_a_host_lock_says_what_holds_it_once() {
    let scratch = tempfile::tempdir().unwrap();
    let (holding, hold) = (scratch.path().join("holding"), scratch.path().join("hold"));
    fs::write(&hold, "").unwrap();
    let dir = fixture(
        &format!(
            "touch {}; while [ -e {} ]; do sleep 0.1; done",
            holding.display(),
            hold.display()
        ),
        "",
    );
    let holder = Caller(
        ferry(dir.path())
            .args(["run", "probe", "--local", "--exclusive", "--seat", "first"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("the first run holds the host alone", || holding.exists());
    let holder_id = only_receipt_file(dir.path())["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let note = format!("waiting for lock host.gate: held by run {holder_id}");
    let (text_err, json_out) = (scratch.path().join("text"), scratch.path().join("json"));
    let waiter = |seat: &str, json: bool, to: &Path| {
        let mut command = ferry(dir.path());
        command.args(["run", "pass", "--local", "--seat", seat]);
        if json {
            command
                .arg("--json")
                .stdout(fs::File::create(to).unwrap())
                .stderr(Stdio::null());
        } else {
            command
                .stdout(Stdio::null())
                .stderr(fs::File::create(to).unwrap());
        }
        Caller(command.spawn().unwrap())
    };
    let mut text = waiter("second", false, &text_err);
    let mut json = waiter("third", true, &json_out);
    // From the place writing the note (its supervisor's log) to the caller
    // saying it: within 5 s, however long the run took to start waiting.
    let runs = dir.path().join(".ferry/local/v1/runs");
    let written = || {
        fs::read_dir(&runs)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                fs::read_to_string(e.path().join("supervisor.log"))
                    .is_ok_and(|log| log.contains(&note))
            })
            .count()
    };
    // The clock starts once both waiters' places have written it.
    wait_until("both waiters' places wrote their note", || written() == 2);
    let noted = Instant::now();
    let text_said = || fs::read_to_string(&text_err).unwrap_or_default();
    let json_said = || fs::read_to_string(&json_out).unwrap_or_default();
    while !(text_said().contains(&format!("ferry: {note}")) && json_said().contains("\"waiting\""))
    {
        assert!(
            noted.elapsed() < Duration::from_secs(5) + common::slack() + Duration::from_secs(3),
            "{} | {}",
            text_said(),
            json_said()
        );
        thread::sleep(Duration::from_millis(50));
    }
    // Several reads later, each caller has said it once.
    thread::sleep(Duration::from_secs(5));
    fs::remove_file(&hold).unwrap();
    for caller in [&mut text, &mut json] {
        wait_until("a waiter ended", || caller.0.try_wait().unwrap().is_some());
    }
    drop(holder);
    assert_eq!(text_said().matches(&note).count(), 1, "{}", text_said());
    let events: Vec<serde_json::Value> = json_said()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let kinds: Vec<_> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["submitted", "waiting", "receipt"], "{events:?}");
    assert_eq!(events[1]["note"], note.as_str(), "{events:?}");
    assert_eq!(events[2]["receipt"]["outcome"], "ran-pass", "{events:?}");
}

#[test]
fn a_record_read_as_the_run_ends_is_never_not_found() {
    if !cfg!(debug_assertions) {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let (started, hold) = (scratch.path().join("started"), scratch.path().join("hold"));
    fs::write(&hold, "").unwrap();
    let dir = fixture(
        &format!(
            "touch {}; while [ -e {} ]; do sleep 0.1; done",
            started.display(),
            hold.display()
        ),
        "",
    );
    let caller = Caller(
        ferry(dir.path())
            .args(["run", "probe", "--local", "--json"])
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
    // The place's record verb pauses between its two reads; the run ends
    // (record final, reservation removed) in that gap.
    let reader = ferry(dir.path())
        .args(["record", &id])
        .env("FERRY_FAIL_AT", "record-gap")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let state = dir.path().join(".ferry/local/v1");
    let gap = state.join(format!("runs/{id}/record-gap"));
    wait_until("the record read paused", || {
        gap.with_extension("paused").exists()
    });
    fs::remove_file(&hold).unwrap();
    wait_until("the run is final and its reservation gone", || {
        state.join(format!("records/{id}.json")).exists()
            && !state.join(format!("records/{id}.reserved.json")).exists()
    });
    fs::write(gap.with_extension("go"), "").unwrap();
    let out = reader.wait_with_output().unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(record["state"], "final", "{record}");
    drop(caller);
}

#[test]
fn an_unknown_run_id_is_named_not_an_os_error() {
    let dir = fixture("true", "");
    for verb in ["record", "follow", "stop"] {
        let out = bounded_output(ferry(dir.path()).args([verb, "nonsense-id"]));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(125), "{verb}: {stderr}");
        assert!(
            stderr.contains("no run nonsense-id in this repository's receipts"),
            "{verb}: {stderr}"
        );
        assert!(!stderr.contains("os error"), "{verb}: {stderr}");
    }
}

#[test]
fn deleted_paths_are_usage_errors_and_leave_no_receipt() {
    let dir = fixture("true", "");
    // `receipt` duplicated `record`: gone.
    let out = bounded_output(ferry(dir.path()).args(["receipt", "x"]));
    assert_eq!(out.status.code(), Some(2));
    // --on with --local is a usage error, not a refused run.
    let out = bounded_output(ferry(dir.path()).args(["run", "pass", "--on", "m", "--local"]));
    assert_eq!(out.status.code(), Some(2));
    assert!(!dir.path().join(".ferry/receipts").exists());
}

#[test]
fn git_and_toml_refusals_say_what_they_mean() {
    let dir = fixture("true", "");
    let out = bounded_output(
        ferry(dir.path()).args(["run", "pass", "--local", "--json", "--commit", "deadbeef"]),
    );
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert_eq!(record["reason"], "unknown commit deadbeef", "{record}");
    // Outside a repository.
    let outside = tempfile::tempdir().unwrap();
    let out = bounded_output(
        ferry(outside.path())
            .env("GIT_CEILING_DIRECTORIES", outside.path().parent().unwrap())
            .args(["run", "pass", "--local"]),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(125), "{stderr}");
    assert!(stderr.contains("not in a git or jj repository"), "{stderr}");
    assert!(!stderr.contains("fatal"), "{stderr}");
    // A ferry.toml that does not parse names the file and the commit.
    fs::write(dir.path().join("ferry.toml"), "version = 1\n[recipe.pass\n").unwrap();
    git(dir.path(), &["commit", "-qam", "broken"]);
    let out = bounded_output(ferry(dir.path()).args(["run", "pass", "--local", "--json"]));
    let record = receipt(&out);
    let at = short(record["commit"].as_str().unwrap()).to_owned();
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .starts_with(&format!("ferry.toml at {at}: TOML parse error")),
        "{record}"
    );
    // One line, as every `ferry: ` line is: TOML's own line breaks joined.
    assert!(
        !record["reason"].as_str().unwrap().contains('\n'),
        "{record}"
    );
}

#[test]
fn recipes_and_machines_are_one_line_each_in_version_two_names() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init"]);
    git(d, &["config", "user.name", "Ferry Test"]);
    git(d, &["config", "user.email", "ferry@example.invalid"]);
    fs::write(
        d.join("ferry.toml"),
        "version = 2\n[recipe.check]\ncommand = ['just', 'check']\nneeds = ['linux', 'big']\nmay_run_elsewhere = true\n[recipe.fmt]\ncommand = ['cargo', 'fmt']\n[recipe.say]\ncommand = ['sh', '-c', \"echo 'hi there'; exit 17\", '']\n",
    )
    .unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "fixture"]);
    fs::create_dir_all(d.join("config/ferry")).unwrap();
    fs::write(
        d.join("config/ferry/machines.toml"),
        "version = 2\n[machine.bench]\nssh = 'bench'\nplatform = 'x86_64-unknown-linux-gnu'\nhas = ['big', 'gpu']\nreserved_for = ['gpu']\n",
    )
    .unwrap();
    let out = bounded_output(ferry(d).arg("recipes"));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "check: just check; needs big, linux; may_run_elsewhere\nfmt: cargo fmt\nsay: sh -c 'echo '\\''hi there'\\''; exit 17' ''\n"
    );
    let out = bounded_output(ferry(d).arg("machines"));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "bench: ssh bench, platform x86_64-unknown-linux-gnu, has big, gpu, reserved_for gpu\n"
    );
}

#[test]
fn a_stop_is_said_and_names_who_stopped_the_run() {
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let dir = fixture(&format!("touch {}; sleep 60", started.display()), "");
    let err = scratch.path().join("err");
    let mut caller = Caller(
        ferry(dir.path())
            .args(["run", "probe", "--local", "--timeout-seconds", "120"])
            .stdout(Stdio::null())
            .stderr(fs::File::create(&err).unwrap())
            .spawn()
            .unwrap(),
    );
    wait_until("the recipe started", || started.exists());
    let id = only_receipt_file(dir.path())["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = bounded_output(ferry(dir.path()).args(["stop", &id]));
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        said(&out),
        [format!(
            "ferry: stop sent to run {id}; ferry follow {id} shows its end"
        )]
    );
    wait_until("the caller ended", || {
        caller.0.try_wait().unwrap().is_some()
    });
    let lines: Vec<String> = fs::read_to_string(&err)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let n = lines.len();
    assert!(n >= 2, "{lines:?}");
    assert!(
        lines[n - 2].starts_with("ferry: stopped: ferry stop (")
            && lines[n - 2].ends_with(") with SIGTERM"),
        "{lines:?}"
    );
    assert_eq!(lines[n - 1], format!("ferry: stopped, exit 143, run {id}"));
    // A second stop is refused as a sentence with its next step.
    let out = bounded_output(ferry(dir.path()).args(["stop", &id]));
    assert_eq!(out.status.code(), Some(125));
    assert_eq!(
        said(&out),
        [format!(
            "ferry: run {id} is not running here; next: ferry record {id}"
        )]
    );
}

#[test]
fn every_command_and_option_says_what_it_does() {
    let dir = fixture("true", "");
    let help = |args: &[&str]| {
        let out = bounded_output(ferry(dir.path()).args(args).arg("-h"));
        assert_eq!(out.status.code(), Some(0));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let top = help(&[]);
    let mut pages = vec![(String::from("ferry"), top.clone())];
    let commands: Vec<String> = top
        .split("Commands:")
        .nth(1)
        .unwrap()
        .split("\n\n")
        .next()
        .unwrap()
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .filter(|name| name != "help")
        .collect();
    assert!(commands.contains(&"run".to_owned()), "{top}");
    for command in &commands {
        pages.push((command.clone(), help(&[command])));
    }
    pages.push(("seats prune".into(), help(&["seats", "prune"])));
    for (page, text) in &pages {
        // Each entry: its name, then what it does, after two spaces or on
        // the next, deeper line.
        // Entries start 2 to 6 columns in (6: a long option alone); what
        // they say starts further in.
        let indent = |line: &str| line.len() - line.trim_start().len();
        let lines: Vec<&str> = text.lines().collect();
        for (at, line) in lines.iter().enumerate() {
            let entry = Some(line.trim_start()).filter(|rest| {
                (2..=6).contains(&indent(line))
                    && (rest.starts_with(['-', '<', '[']) || rest.starts_with(char::is_alphabetic))
            });
            if let Some(entry) = entry {
                // A description below is indented past every entry name,
                // and is not itself an entry.
                let below = lines.get(at + 1).copied().unwrap_or_default();
                let described_below = indent(below) > 6 && !below.trim().is_empty();
                assert!(
                    entry.trim_end().contains("  ") || described_below,
                    "{page}: {line:?} says nothing\n{text}"
                );
            }
        }
    }
    let run = help(&["run"]);
    for needed in ["default: HEAD", "124", "125", "Exit codes"] {
        assert!(run.contains(needed), "{needed}: {run}");
    }
}

#[test]
fn follow_replays_a_finished_runs_output() {
    let dir = fixture("true", "");
    let out = bounded_output(ferry(dir.path()).args(["run", "pass", "--local", "--json"]));
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let out = bounded_output(ferry(dir.path()).args(["follow", &id]));
    assert_eq!(out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("committed\n"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}
