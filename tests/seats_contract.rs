//! A place's seats and what it keeps (SPEC §9): prune and retire, one-run
//! seats, the seat cap, the seat book, run history, and host admission.
#![allow(clippy::wildcard_imports)]
mod common;
use common::*;

#[test]
fn admission_without_a_policy_is_not_configured_and_claims_no_floor() {
    let dir = fixture("true", "");
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(out.status.success());
    let admission = &receipt(&out)["admission"];
    assert_eq!(admission["policy"], "not-configured");
    assert!(admission["floor_bytes"].is_null());
    assert!(admission["available_before"].is_null());
    assert_eq!(admission["result"], "admitted");
}

#[test]
fn a_malformed_or_unsupported_policy_refuses_without_measuring() {
    for policy in [
        "version = 1\nmin_free_bytes = 'lots'\n",
        "version = 3\nmin_free_bytes = 1\n",
        "version = 1\nmin_free_bytes = 1\nsurprise = true\n",
        "version = 1\nmin_free_bytes = 1\nevict = { mode = 'idle-seat-targets', targets = ['cargo-target'] }\n",
        "version = 2\nmin_free_bytes = 1\nevict = { mode = 'idle-seat-targets', targets = ['../escape'] }\n",
    ] {
        let dir = fixture("true", "");
        with_host_policy(dir.path(), policy);
        let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
        assert_eq!(out.status.code(), Some(125), "{policy}");
        let admission = &receipt(&out)["admission"];
        assert_eq!(admission["result"], "refused", "{policy}");
        assert_eq!(admission["policy"]["sha256"].as_str().unwrap().len(), 64);
        assert!(
            admission["floor_bytes"].is_null() || policy.contains("evict"),
            "{policy}"
        );
        assert!(admission["available_before"].is_null(), "{policy}");
        assert!(!String::from_utf8_lossy(&out.stdout).contains("committed"));
    }
}

#[test]
fn version_one_guards_the_floor() {
    let dir = fixture("true", "");
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1000000000000000000\n",
    );
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(125));
    let record = receipt(&out);
    let admission = &record["admission"];
    assert_eq!(admission["result"], "refused");
    assert_eq!(admission["policy"]["version"], 1);
    assert_eq!(admission["floor_bytes"], 1_000_000_000_000_000_000u64);
    assert!(admission["available"].as_u64().is_some());
    let reason = record["reason"].as_str().unwrap();
    assert!(reason.contains("below host floor"), "{reason}");
    // SPEC §9: the refusal names where to look and what frees room.
    assert!(
        reason.contains("ferry status --on")
            && reason.contains("ferry seats prune --on")
            && reason.contains("--idle"),
        "{reason}"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("committed"));
}

/// The retired version 2 eviction policy (its `evict` block) is refused
/// naming what bounds seats now, not as a schema error (murail-4a's
/// db0c1892, carried into version 2's plain names).
#[test]
fn a_retired_eviction_policy_names_what_bounds_seats_now() {
    let dir = fixture("true", "");
    with_host_policy(
        dir.path(),
        "version = 2\nrefuse_runs_below_free_disk = '1 B'\nevict = { mode = 'idle-seat-targets', targets = ['cargo-target'] }\n",
    );
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(125));
    let record = receipt(&out);
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains("`evict` is retired") && reason.contains("max_cache_per_checkout"),
        "{record}"
    );
}

/// `ferry seats prune` retires a seat whose workspace is gone, through the
/// place's `retire` verb, and spares one whose workspace exists. The lock
/// file survives; the place records the retire and the book a tombstone
/// (SPEC §9, slice 5).
#[test]
fn seats_prune_retires_a_gone_workspaces_seat_and_spares_a_live_one() {
    let dir = fixture("true", "");
    let gone = workspace(dir.path(), "gone");
    assert_eq!(remote_pass(dir.path(), dir.path()).status.code(), Some(0));
    assert_eq!(remote_pass(dir.path(), &gone).status.code(), Some(0));
    let live = key_of(dir.path(), dir.path());
    let retired = key_of(dir.path(), &gone);
    assert_ne!(live, retired);
    let place = dir.path().join("host/.local/state/ferry/v1");
    assert!(place.join("seats").join(&retired).join("checkout").exists());
    fs::remove_dir_all(&gone).unwrap();
    let (code, said) = prune(dir.path(), ".local/bin/ferry");
    assert_eq!(code, Some(0), "{said}");
    assert!(said.contains("retired"), "{said}");
    assert!(
        !place.join("seats").join(&retired).join("checkout").exists(),
        "{said}"
    );
    // Nothing of the seat is left: checkout, cache and measurement stamp.
    assert!(!place.join("seats").join(&retired).exists(), "{said}");
    assert!(place.join("locks").join(format!("seat.{retired}")).exists());
    assert!(
        place
            .join(format!("records/retired-{retired}.json"))
            .exists()
    );
    assert!(place.join("seats").join(&live).join("checkout").exists());
    assert!(
        seats_book(dir.path())
            .iter()
            .any(|line| line["key"] == retired.as_str() && line["retired_ms"].is_u64())
    );
    // Nothing is left to do.
    let (code, said) = prune(dir.path(), ".local/bin/ferry");
    assert_eq!(code, Some(0), "{said}");
    assert!(said.trim().is_empty(), "{said}");
}

/// Prune touches only the seats this caller recorded: another caller's book
/// does not name them.
#[test]
fn seats_prune_never_names_another_callers_seat() {
    let dir = fixture("true", "");
    let gone = workspace(dir.path(), "gone");
    assert_eq!(remote_pass(dir.path(), &gone).status.code(), Some(0));
    let key = key_of(dir.path(), &gone);
    fs::remove_dir_all(&gone).unwrap();
    let out = remote_command(dir.path(), ".local/bin/ferry", &["seats", "prune"])
        .env("XDG_STATE_HOME", dir.path().join("another-caller"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "");
    let place = dir.path().join("host/.local/state/ferry/v1");
    assert!(place.join("seats").join(&key).join("checkout").exists());
}

/// While a retire holds a seat: a second prune skips it as busy, a run on
/// that seat is refused naming the retire, and a run's line appended to the
/// book meanwhile survives. After it, a run on the same workspace path
/// revives the seat, and prune spares it (SPEC §9, slice 5).
#[test]
fn a_seat_being_retired_is_busy_and_the_book_keeps_concurrent_appends() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let gone = workspace(dir.path(), "gone");
    assert_eq!(remote_pass(dir.path(), &gone).status.code(), Some(0));
    let key = key_of(dir.path(), &gone);
    fs::remove_dir_all(&gone).unwrap();
    let lock = dir
        .path()
        .join("host/.local/state/ferry/v1/locks")
        .join(format!("seat.{key}"));
    let first = remote_command(dir.path(), ".local/bin/ferry", &["seats", "prune"])
        .env("FERRY_FAIL_AT", "retire-pause")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = lock.with_extension("paused");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !paused.exists() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(paused.exists(), "the retire never paused");
    let (code, said) = prune(dir.path(), ".local/bin/ferry");
    assert_eq!(code, Some(0), "{said}");
    assert!(
        said.contains("skipped") && said.contains("being retired"),
        "{said}"
    );
    // The same workspace path again: the same seat.
    let back = workspace(dir.path(), "gone");
    let out = remote_pass(dir.path(), &back);
    let refused = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{refused}");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap()
            .contains("being retired"),
        "{refused}"
    );
    // Appended while the prune runs.
    assert_eq!(remote_pass(dir.path(), dir.path()).status.code(), Some(0));
    let live = key_of(dir.path(), dir.path());
    fs::write(lock.with_extension("go"), "").unwrap();
    let out = first.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{said}");
    assert!(said.contains("retired"), "{said}");
    let book = seats_book(dir.path());
    assert!(book.iter().any(|line| line["key"] == live.as_str()));
    assert!(
        book.iter()
            .any(|line| line["key"] == key.as_str() && line["retired_ms"].is_u64())
    );
    // A run on the workspace revives the seat; prune then spares it.
    fs::remove_file(lock.with_extension("go")).unwrap();
    assert_eq!(remote_pass(dir.path(), &back).status.code(), Some(0));
    let (code, said) = prune(dir.path(), ".local/bin/ferry");
    assert_eq!(code, Some(0), "{said}");
    assert!(!said.contains("retired"), "{said}");
}

/// A place without `retire` is refused, naming its revision (125), as for
/// `detach`; nothing is deleted.
#[test]
fn seats_prune_refuses_a_place_without_retire_naming_its_revision() {
    let dir = fixture("true", "");
    let gone = workspace(dir.path(), "gone");
    assert_eq!(remote_pass(dir.path(), &gone).status.code(), Some(0));
    let key = key_of(dir.path(), &gone);
    fs::remove_dir_all(&gone).unwrap();
    let bin = dir.path().join("host/.local/bin");
    fs::write(
        bin.join("old-ferry"),
        "#!/bin/sh\ncase \"$1\" in host-info) \"$HOME/.local/bin/ferry\" host-info | sed -e 's/\"retire\"//' -e 's/,,/,/' -e 's/,\\]/]/' -e 's/\"source_rev\":\"[^\"]*\"/\"source_rev\":\"0ld-place-rev\"/';; *) exec \"$HOME/.local/bin/ferry\" \"$@\";; esac\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("old-ferry"), fs::Permissions::from_mode(0o755)).unwrap();
    let (code, said) = prune(dir.path(), ".local/bin/old-ferry");
    assert_eq!(code, Some(125), "{said}");
    assert!(
        said.contains("lacks retire") && said.contains("0ld-place-rev"),
        "{said}"
    );
    let place = dir.path().join("host/.local/state/ferry/v1");
    assert!(place.join("seats").join(&key).join("checkout").exists());
}

/// The release points of retire: a failed record or delete is reported and
/// refused (125), and a later prune finishes the retire. A delete that fails
/// after the rename leaves only the tombstone (SPEC §9, §12 item 21): the
/// key is gone, `status` lists the tombstone, and the next prune deletes it.
#[test]
fn a_failed_retire_is_reported_and_a_later_prune_finishes_it() {
    if !cfg!(debug_assertions) {
        return;
    }
    for point in ["retire-delete", "retire-record"] {
        let dir = fixture("true", "");
        let gone = workspace(dir.path(), "gone");
        assert_eq!(remote_pass(dir.path(), &gone).status.code(), Some(0));
        let key = key_of(dir.path(), &gone);
        fs::remove_dir_all(&gone).unwrap();
        let out = remote_command(dir.path(), ".local/bin/ferry", &["seats", "prune"])
            .env("FERRY_FAIL_AT", point)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(125), "{point}: {said}");
        assert!(said.contains(point), "{said}");
        assert!(
            !seats_book(dir.path())
                .iter()
                .any(|line| line["retired_ms"].is_u64()),
            "{point}: a failed retire left a tombstone"
        );
        let place = dir.path().join("host/.local/state/ferry/v1");
        let tombstones = || {
            fs::read_dir(place.join("seats"))
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(&format!(".retired-{key}-")))
                .collect::<Vec<_>>()
        };
        // Never half a seat under the key: it was renamed whole.
        assert!(!place.join("seats").join(&key).exists(), "{point}: {said}");
        assert_eq!(tombstones().len(), 1, "{point}: {said}");
        let status = remote_command(
            dir.path(),
            ".local/bin/ferry",
            &["status", "--on", "test-host", "--json"],
        )
        .output()
        .unwrap();
        let seen: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(seen["tombstones"].as_array().unwrap().len(), 1, "{seen}");
        assert!(
            !seen["seats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|seat| seat["key"].as_str().unwrap().starts_with('.')),
            "a tombstone counted as a seat: {seen}"
        );
        let (code, said) = prune(dir.path(), ".local/bin/ferry");
        assert_eq!(code, Some(0), "{point}: {said}");
        assert!(said.contains("retired"), "{said}");
        assert!(place.join(format!("records/retired-{key}.json")).exists());
        assert!(tombstones().is_empty(), "{point}: the tombstone stayed");
    }
}

/// SPEC §12 item 20: `prune --idle` retires a seat idle on both clocks
/// (this caller's book and the place's `used`), keeps one used since by
/// either, keeps one whose age is unknown, skips a busy one, and reports
/// each; records and run folders stay. Item 21: a book that predates a run
/// on the seat (the place's `used` is newer) is refused `used`.
#[test]
fn seats_prune_idle_retires_only_what_is_idle_on_both_clocks() {
    let dir = fixture("true", "");
    let names = ["idle", "recent", "reused", "ageless", "busy"];
    let mut keys = BTreeMap::new();
    for name in names {
        let repo = workspace(dir.path(), name);
        assert_eq!(remote_pass(dir.path(), &repo).status.code(), Some(0));
        keys.insert(name, key_of(dir.path(), &repo));
    }
    age(dir.path(), &keys["idle"], 2 * DAY_MS, false);
    age(dir.path(), &keys["reused"], 2 * DAY_MS, true);
    age(dir.path(), &keys["busy"], 2 * DAY_MS, false);
    // A line written before last_used_ms existed has an unknown age.
    let book = dir.path().join("state/ferry/seats.jsonl");
    let lines: Vec<String> = fs::read_to_string(&book)
        .unwrap()
        .lines()
        .map(|line| {
            let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
            if value["key"] == keys["ageless"].as_str() {
                value.as_object_mut().unwrap().remove("last_used_ms");
            }
            value.to_string()
        })
        .collect();
    fs::write(&book, lines.join("\n") + "\n").unwrap();
    let place = dir.path().join("host/.local/state/ferry/v1");
    let held = fs::File::open(place.join("locks").join(format!("seat.{}", keys["busy"]))).unwrap();
    fs2::FileExt::try_lock_exclusive(&held).unwrap();
    let runs = fs::read_dir(place.join("runs")).unwrap().count();
    let (code, said) = prune_with(dir.path(), &["--idle", "1d"]);
    assert_eq!(code, Some(0), "{said}");
    let seat = |name: &str| place.join("seats").join(&keys[name]);
    let line = |name: &str| {
        said.lines()
            .find(|line| line.contains(&format!("/{name} on test-host")))
            .unwrap_or_default()
            .to_owned()
    };
    assert!(!seat("idle").exists(), "{said}");
    assert!(
        line("idle").starts_with("retired") && line("idle").contains("unused for 2d"),
        "{said}"
    );
    for kept in ["recent", "reused", "ageless", "busy"] {
        assert!(seat(kept).join("checkout").exists(), "{kept}: {said}");
    }
    assert!(
        line("recent").is_empty(),
        "a recent seat is not considered: {said}"
    );
    assert!(line("reused").starts_with("skipped (used since)"), "{said}");
    assert!(
        line("ageless").starts_with("kept") && line("ageless").contains("age is unknown"),
        "{said}"
    );
    assert!(line("busy").starts_with("skipped default"), "{said}");
    assert_eq!(fs::read_dir(place.join("runs")).unwrap().count(), runs);
    assert!(
        seats_book(dir.path())
            .iter()
            .any(|line| line["key"] == keys["idle"].as_str() && line["retired_ms"].is_u64())
    );
    drop(held);
}

/// SPEC §9: no retire, by hand or by itself, deletes while an exclusive run
/// holds the host or waits at its gate: it is refused `exclusive`.
#[test]
fn retire_is_refused_while_an_exclusive_run_holds_the_host() {
    for lock in ["host", "host.gate"] {
        let dir = fixture("true", "");
        let repo = workspace(dir.path(), "idle");
        assert_eq!(remote_pass(dir.path(), &repo).status.code(), Some(0));
        let key = key_of(dir.path(), &repo);
        age(dir.path(), &key, 2 * DAY_MS, false);
        let place = dir.path().join("host/.local/state/ferry/v1");
        let held = fs::File::open(place.join("locks").join(lock)).unwrap();
        fs2::FileExt::try_lock_exclusive(&held).unwrap();
        let (code, said) = prune_with(dir.path(), &["--idle", "1d"]);
        assert_eq!(code, Some(0), "{lock}: {said}");
        assert!(
            said.starts_with("skipped (exclusive run)") && said.contains("/idle on"),
            "{lock}: {said}"
        );
        assert!(
            place.join("seats").join(&key).join("checkout").exists(),
            "{lock}"
        );
        drop(held);
        let (code, said) = prune_with(dir.path(), &["--idle", "1d"]);
        assert_eq!(code, Some(0), "{said}");
        assert!(
            said.starts_with("retired") && said.contains("/idle on"),
            "{lock}: {said}"
        );
    }
}

/// SPEC §12 item 19: a `--once` run ends with one final record saying
/// `once`, the seat retired and recorded, and no `seats/<key>`. With the
/// retire failing before the rename, the record is the same and final, the
/// seat stays, `status` lists it, and a later `prune --idle 0s` retires it.
#[test]
fn a_once_run_retires_its_seat_after_its_final_record() {
    let dir = fixture("true", "");
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--once", "--json"],
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let key = key_of(dir.path(), dir.path());
    let place = dir.path().join("host/.local/state/ferry/v1");
    let record = host_record(dir.path(), &id);
    assert_eq!(record["once"], true, "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    // The retire comes after the record is final: wait for its effect.
    let until = common::startup_deadline();
    while place.join("seats").join(&key).exists() {
        assert!(std::time::Instant::now() < until, "the --once seat stayed");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(place.join(format!("records/retired-{key}.json")).exists());
    assert_eq!(
        host_record(dir.path(), &id),
        record,
        "the record changed after final"
    );
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--once", "--json"],
    )
    .env("FERRY_FAIL_AT", "retire-rename")
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let key = key_of(dir.path(), dir.path());
    let place = dir.path().join("host/.local/state/ferry/v1");
    let record = host_record(dir.path(), &id);
    assert_eq!(record["once"], true, "{record}");
    let line = once_logged(dir.path(), &id);
    assert!(line.contains("retire-rename"), "{line}");
    assert!(place.join("seats").join(&key).join("checkout").exists());
    let status = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["status", "--on", "test-host", "--json"],
    )
    .output()
    .unwrap();
    let seen: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(
        seen["seats"]
            .as_array()
            .unwrap()
            .iter()
            .any(|seat| seat["key"] == key.as_str()),
        "{seen}"
    );
    let (code, said) = prune_with(dir.path(), &["--idle", "0s"]);
    assert_eq!(code, Some(0), "{said}");
    assert!(said.contains("retired"), "{said}");
    assert!(!place.join("seats").join(&key).exists());
    assert_eq!(host_record(dir.path(), &id), record);
}

/// SPEC §12 items 19 and 21: a `--once` run that leaves a child holding the
/// seat's inherited descriptor (item 9's escaped child; also one that
/// cleared its marker) keeps its seat: the record is final and unchanged,
/// the seat stays busy, prune skips it while the child lives, and retires it
/// once the child has exited. Closing, never `LOCK_UN`, is what keeps it.
#[test]
fn a_once_seat_held_by_a_survivor_stays_until_it_exits() {
    for launch in ["", "env -i PATH=\"$PATH\" "] {
        let dir = fixture("true", "");
        let child = dir.path().join("child");
        let p = child.display();
        // Item 9's leaver: its own session, so the run's stop never reaches
        // it, holding the seat's inherited descriptor.
        fs::write(
            dir.path().join("ferry.toml"),
            format!(
                "version=1\n[recipe.pass]\ncommand=['sh','-c','{launch}python3 -c \"import os,time; os.setsid(); open(\\\"{p}.tmp\\\",\\\"w\\\").write(str(os.getpid())); os.rename(\\\"{p}.tmp\\\",\\\"{p}\\\"); time.sleep(600)\" </dev/null >/dev/null 2>&1 & while [ ! -e {p} ]; do sleep 0.05; done']\nroutable=true\n"
            ),
        )
        .unwrap();
        git(dir.path(), &["add", "ferry.toml"]);
        git(dir.path(), &["commit", "-m", "an escaped child"]);
        let out = remote_command(
            dir.path(),
            ".local/bin/ferry",
            &["run", "pass", "--on", "test-host", "--once", "--json"],
        )
        .output()
        .unwrap();
        let pid: i32 = fs::read_to_string(&child).unwrap().trim().parse().unwrap();
        let guard = OwnedPid(pid);
        assert_eq!(out.status.code(), Some(0), "{launch}: {}", receipt(&out));
        let id = receipt(&out)["id"].as_str().unwrap().to_owned();
        let key = key_of(dir.path(), dir.path());
        let place = dir.path().join("host/.local/state/ferry/v1");
        let record = host_record(dir.path(), &id);
        let line = once_logged(dir.path(), &id);
        assert!(line.contains("\"refused\":\"busy\""), "{launch}: {line}");
        assert!(
            place.join("seats").join(&key).join("checkout").exists(),
            "{launch}: the seat was retired under a live child"
        );
        let (code, said) = prune_with(dir.path(), &["--idle", "0s"]);
        assert_eq!(code, Some(0), "{said}");
        assert!(said.contains("skipped"), "{launch}: {said}");
        assert!(place.join("seats").join(&key).join("checkout").exists());
        drop(guard);
        let until = common::startup_deadline();
        while alive(pid) {
            assert!(std::time::Instant::now() < until, "the child stayed");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let (code, said) = prune_with(dir.path(), &["--idle", "0s"]);
        assert_eq!(code, Some(0), "{said}");
        assert!(said.contains("retired"), "{launch}: {said}");
        assert_eq!(host_record(dir.path(), &id), record, "{launch}");
    }
}

/// SPEC §12 item 21: a `--once` retire delayed past another run on the same
/// seat finds that run's `used` and is refused: the later run keeps it.
#[test]
fn a_delayed_once_retire_never_deletes_a_reused_seat() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let once = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--once", "--json"],
    )
    .env("FERRY_FAIL_AT", "once-retire-pause")
    .stdout(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    let place = dir.path().join("host/.local/state/ferry/v1");
    let until = common::startup_deadline();
    let paused = loop {
        let found = fs::read_dir(place.join("runs"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|run| run.path().join("once.paused"))
            .find(|paused| paused.exists());
        if let Some(paused) = found {
            break paused;
        }
        assert!(
            std::time::Instant::now() < until,
            "the --once retire never paused"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let once = once.wait_with_output().unwrap();
    assert_eq!(once.status.code(), Some(0), "{}", receipt(&once));
    // The seat is free (its record is final, its lock closed): a later run
    // takes it and writes its `used`.
    assert_eq!(remote_pass(dir.path(), dir.path()).status.code(), Some(0));
    fs::write(paused.with_extension("go"), "").unwrap();
    let key = key_of(dir.path(), dir.path());
    let id = paused
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let line = once_logged(dir.path(), &id);
    assert!(line.contains("\"refused\":\"used\""), "{line}");
    assert!(
        place.join("seats").join(&key).join("checkout").exists(),
        "a delayed --once retire deleted the seat a later run used"
    );
}

/// SPEC §12 item 23: with `delete_unused_checkouts_after_days`, a run prunes
/// its place's idle seats by itself, after its receipt: it retires a seat
/// unused for two days, keeps one used an hour ago, prints one line on
/// stderr, and keeps its own code and `--json`. A second run within the
/// hour does not prune, nor one that finds the claim's lock held.
#[test]
fn a_run_prunes_its_places_idle_seats_by_itself() {
    let dir = fixture("true", "");
    fs::write(
        dir.path().join("machines-top.toml"),
        "delete_unused_checkouts_after_days = 1\n",
    )
    .unwrap();
    let old = workspace(dir.path(), "old");
    let hour = workspace(dir.path(), "hour");
    for repo in [&old, &hour] {
        assert_eq!(remote_pass(dir.path(), repo).status.code(), Some(0));
    }
    let (old_key, hour_key) = (key_of(dir.path(), &old), key_of(dir.path(), &hour));
    // The runs above already claimed this hour: start from none.
    fs::remove_file(dir.path().join("state/ferry/pruned.json")).unwrap();
    age(dir.path(), &old_key, 2 * DAY_MS, false);
    age(dir.path(), &hour_key, 3_600_000, false);
    let place = dir.path().join("host/.local/state/ferry/v1");
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "fail", "--on", "test-host", "--json"],
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(125), "the run's own code");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let pruned: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("ferry: pruned test-host"))
        .collect();
    assert_eq!(pruned.len(), 1, "{stderr}");
    assert!(pruned[0].contains("retired 1 unused for 1d"), "{stderr}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("pruned"));
    assert!(!place.join("seats").join(&old_key).exists());
    assert!(
        place
            .join("seats")
            .join(&hour_key)
            .join("checkout")
            .exists()
    );
    // Within the hour: no prune, even of a seat now idle.
    age(dir.path(), &hour_key, 2 * DAY_MS, false);
    let out = remote_pass(dir.path(), dir.path());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("pruned"));
    assert!(
        place
            .join("seats")
            .join(&hour_key)
            .join("checkout")
            .exists()
    );
    // The claim's lock held by another run: skipped silently.
    fs::remove_file(dir.path().join("state/ferry/pruned.json")).unwrap();
    let lock = fs::File::create(dir.path().join("state/ferry/pruned.lock")).unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let out = remote_pass(dir.path(), dir.path());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("pruned"));
    assert!(
        place
            .join("seats")
            .join(&hour_key)
            .join("checkout")
            .exists()
    );
    drop(lock);
    let out = remote_pass(dir.path(), dir.path());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ferry: pruned test-host: retired 1"));
}

/// SPEC §12 item 23: two runs ending together prune their place once: the
/// hour is claimed under the lock before pruning.
#[test]
fn two_runs_ending_together_prune_once() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    fs::write(
        dir.path().join("machines-top.toml"),
        "delete_unused_checkouts_after_days = 1\n",
    )
    .unwrap();
    let repos: Vec<_> = ["a", "b", "old"]
        .iter()
        .map(|name| workspace(dir.path(), name))
        .collect();
    for repo in &repos {
        assert_eq!(remote_pass(dir.path(), repo).status.code(), Some(0));
    }
    fs::remove_file(dir.path().join("state/ferry/pruned.json")).unwrap();
    let old = key_of(dir.path(), &repos[2]);
    age(dir.path(), &old, 2 * DAY_MS, false);
    // Both runs stop inside the claim (`prune-claim`, after its lock): the
    // first holds it there; the second must find it held and skip, never
    // wait to read the same old time.
    let spawn = |repo: &Path| {
        remote_command(
            dir.path(),
            ".local/bin/ferry",
            &["run", "pass", "--on", "test-host", "--json"],
        )
        .current_dir(repo)
        .env("FERRY_FAIL_AT", "prune-claim")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
    };
    let claim = dir.path().join("state/ferry/prune-claim");
    let mut first = spawn(&repos[0]);
    let until = common::startup_deadline();
    while !claim.with_extension("paused").exists() {
        assert!(
            std::time::Instant::now() < until,
            "the first claim never paused"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut second = spawn(&repos[1]);
    let until = std::time::Instant::now() + common::slack();
    while second.try_wait().unwrap().is_none() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    fs::write(claim.with_extension("go"), "").unwrap();
    let mut pruned = 0;
    for run in [&mut first, &mut second] {
        let mut stderr = String::new();
        std::io::Read::read_to_string(run.stderr.as_mut().unwrap(), &mut stderr).unwrap();
        assert_eq!(run.wait().unwrap().code(), Some(0), "{stderr}");
        pruned += stderr
            .lines()
            .filter(|line| line.starts_with("ferry: pruned test-host"))
            .count();
    }
    assert_eq!(pruned, 1, "two runs pruned the same place in one hour");
    let place = dir.path().join("host/.local/state/ferry/v1");
    assert!(!place.join("seats").join(&old).exists());
}

/// A place without `seat-lifetime` (an older Ferry) is refused an idle
/// retire, naming its revision, and a `--once` run before acceptance: it
/// would neither check the seat's latest use nor keep out of an exclusive
/// run. Its workspace-gone retire still works as before.
#[test]
fn a_place_without_seat_lifetime_is_refused_idle_and_once() {
    let dir = fixture("true", "");
    let bin = dir.path().join("host/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        bin.join("old-ferry"),
        "#!/bin/sh\ncase \"$1\" in host-info) \"$HOME/.local/bin/ferry\" host-info | sed -e 's/\"seat-lifetime\",//' -e 's/\"source_rev\":\"[^\"]*\"/\"source_rev\":\"0ld-place-rev\"/';; *) exec \"$HOME/.local/bin/ferry\" \"$@\";; esac\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("old-ferry"), fs::Permissions::from_mode(0o755)).unwrap();
    let repo = workspace(dir.path(), "idle");
    let out = remote_command(
        dir.path(),
        ".local/bin/old-ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .current_dir(&repo)
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let key = key_of(dir.path(), &repo);
    age(dir.path(), &key, 2 * DAY_MS, false);
    let out = remote_command(
        dir.path(),
        ".local/bin/old-ferry",
        &["seats", "prune", "--idle", "1d"],
    )
    .output()
    .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(125), "{said}");
    assert!(
        said.contains("lacks seat-lifetime") && said.contains("0ld-place-rev"),
        "{said}"
    );
    let place = dir.path().join("host/.local/state/ferry/v1");
    assert!(place.join("seats").join(&key).join("checkout").exists());
    let out = remote_command(
        dir.path(),
        ".local/bin/old-ferry",
        &["run", "pass", "--on", "test-host", "--once", "--json"],
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(125), "{}", receipt(&out));
    assert!(
        receipt(&out)["reason"]
            .as_str()
            .unwrap()
            .contains("seat-lifetime"),
        "{}",
        receipt(&out)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("\"submitted\""));
    fs::remove_dir_all(&repo).unwrap();
    let out = remote_command(dir.path(), ".local/bin/old-ferry", &["seats", "prune"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!place.join("seats").join(&key).exists());
}

/// Every wait is bounded (SPEC §9, PRINCIPLES 5): a caller waits for a
/// retire in flight only up to the ssh reply bound, then reports the place
/// unreachable (255); the place goes on with the retire.
#[test]
fn a_slow_retire_is_waited_for_only_up_to_the_reply_bound() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let repo = workspace(dir.path(), "idle");
    assert_eq!(remote_pass(dir.path(), &repo).status.code(), Some(0));
    let key = key_of(dir.path(), &repo);
    age(dir.path(), &key, 2 * DAY_MS, false);
    let lock = dir
        .path()
        .join("host/.local/state/ferry/v1/locks")
        .join(format!("seat.{key}"));
    let started = std::time::Instant::now();
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["seats", "prune", "--idle", "1d"],
    )
    .env("FERRY_FAIL_AT", "retire-pause")
    .env("FERRY_SSH_REPLY_MS", "2000")
    .output()
    .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(255), "{said}");
    assert!(said.contains("did not reply within"), "{said}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2) + common::slack());
    // Let the paused retire go on, as the place would.
    fs::write(lock.with_extension("go"), "").unwrap();
}

/// ferry-1a F1: a seat an earlier retire already took (a `--once` run's, or
/// one retired by hand, its lock file gone too) is "already gone": the book
/// marks it retired and prune exits 0, and the next prune does not consider
/// it again, never a failure that stays forever.
#[test]
fn an_already_retired_seat_is_already_gone_not_failed() {
    let dir = fixture("true", "");
    let once = workspace(dir.path(), "once");
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--once", "--json"],
    )
    .current_dir(&once)
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    assert!(once_logged(dir.path(), &id).contains("\"retired\""));
    let by_hand = workspace(dir.path(), "by-hand");
    assert_eq!(remote_pass(dir.path(), &by_hand).status.code(), Some(0));
    let hand = key_of(dir.path(), &by_hand);
    let place = dir.path().join("host/.local/state/ferry/v1");
    fs::remove_dir_all(place.join("seats").join(&hand)).unwrap();
    fs::remove_file(place.join("locks").join(format!("seat.{hand}"))).unwrap();
    let once_key = key_of(dir.path(), &once);
    let record_path = place.join(format!("records/retired-{once_key}.json"));
    let record = fs::read(&record_path).unwrap();
    let (code, said) = prune_with(dir.path(), &["--idle", "0s"]);
    assert_eq!(code, Some(0), "{said}");
    // ferry-hm5: the --once retire's record keeps its bytes and time.
    assert_eq!(
        fs::read(&record_path).unwrap(),
        record,
        "already gone rewrote the record"
    );
    for name in ["once", "by-hand"] {
        assert!(
            said.lines()
                .any(|line| line.starts_with("already gone")
                    && line.contains(&format!("/{name} on"))),
            "{name}: {said}"
        );
    }
    assert!(!said.contains("failed"), "{said}");
    let (code, said) = prune_with(dir.path(), &["--idle", "0s"]);
    assert_eq!(code, Some(0), "{said}");
    assert!(
        !said.contains("/once on") && !said.contains("/by-hand on"),
        "considered again: {said}"
    );
}

/// murail-4a ferry-6p0: the retirement record is written through a
/// temporary file of its own, never one a link already holds: a link left
/// at the old fixed temporary name (or any other) is not followed, and its
/// target outside the state root stays untouched.
#[test]
fn the_retirement_record_is_never_written_through_a_link() {
    let dir = fixture("true", "");
    let repo = workspace(dir.path(), "idle");
    assert_eq!(remote_pass(dir.path(), &repo).status.code(), Some(0));
    let key = key_of(dir.path(), &repo);
    age(dir.path(), &key, 2 * DAY_MS, false);
    let records = dir.path().join("host/.local/state/ferry/v1/records");
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    std::os::unix::fs::symlink(&sentinel, records.join(format!("retired-{key}.json.tmp"))).unwrap();
    let (code, said) = prune_with(dir.path(), &["--idle", "1d"]);
    assert_eq!(code, Some(0), "{said}");
    assert!(said.starts_with("retired"), "{said}");
    assert_eq!(
        fs::read_to_string(&sentinel).unwrap(),
        "keep",
        "written through the link"
    );
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(records.join(format!("retired-{key}.json"))).unwrap())
            .unwrap();
    assert_eq!(record["key"], key.as_str());
}

/// herald-1b: a sweep of the key's earlier tombstones that does not finish
/// is a failure, so the book keeps the key and the next prune deletes what
/// is left; the retire's record keeps its bytes.
#[test]
fn a_failed_sweep_keeps_the_key_for_the_next_prune() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let repo = workspace(dir.path(), "idle");
    assert_eq!(remote_pass(dir.path(), &repo).status.code(), Some(0));
    let key = key_of(dir.path(), &repo);
    age(dir.path(), &key, 2 * DAY_MS, false);
    let place = dir.path().join("host/.local/state/ferry/v1");
    let older = place.join("seats").join(format!(".retired-{key}-5"));
    fs::create_dir_all(older.join("checkout")).unwrap();
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["seats", "prune", "--idle", "1d"],
    )
    .env("FERRY_FAIL_AT", "tombstone-sweep")
    .output()
    .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(125), "{said}");
    assert!(
        said.starts_with("failed") && said.contains("tombstone-sweep"),
        "{said}"
    );
    assert!(older.exists());
    assert!(
        !seats_book(dir.path())
            .iter()
            .any(|line| line["key"] == key.as_str() && line["retired_ms"].is_u64()),
        "an incomplete sweep was booked retired"
    );
    let record_path = place.join(format!("records/retired-{key}.json"));
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert!(record["bytes"].as_u64() > Some(0), "{record}");
    let (code, said) = prune_with(dir.path(), &["--idle", "1d"]);
    assert_eq!(code, Some(0), "{said}");
    assert!(said.starts_with("retired"), "{said}");
    assert!(!older.exists(), "the next prune left the tombstone");
    let again: serde_json::Value =
        serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(again, record, "the retry rewrote the record");
}

/// ferry-1a F2: a key reused after a retire and retired again replaces its
/// retirement record whole, so the record ages from the latest retire.
#[test]
fn a_key_retired_again_replaces_its_retirement_record() {
    let dir = fixture("true", "");
    let place = dir.path().join("host/.local/state/ferry/v1");
    let mut retired = vec![];
    for _ in 0..2 {
        let out = remote_command(
            dir.path(),
            ".local/bin/ferry",
            &["run", "pass", "--on", "test-host", "--once", "--json"],
        )
        .output()
        .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
        let id = receipt(&out)["id"].as_str().unwrap().to_owned();
        assert!(once_logged(dir.path(), &id).contains("\"retired\""));
        let key = key_of(dir.path(), dir.path());
        let record: serde_json::Value = serde_json::from_slice(
            &fs::read(place.join(format!("records/retired-{key}.json"))).unwrap(),
        )
        .unwrap();
        retired.push(record["retired_ms"].as_u64().unwrap());
    }
    assert!(
        retired[1] > retired[0],
        "the second retire kept the first record: {retired:?}"
    );
}

/// herald-1b R1: a run that cannot write its seat's `used` record is
/// refused before anything runs: a stale one would let a delayed retire
/// take this run's seat.
#[test]
fn a_run_that_cannot_write_its_used_record_is_refused() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_FAIL_AT", "used-write")
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(125), "{}", receipt(&out));
    assert!(
        receipt(&out)["reason"]
            .as_str()
            .unwrap()
            .contains("seat used record"),
        "{}",
        receipt(&out)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("committed"));
}

/// herald-1b R2: a retire deletes only its own key's tombstones, never
/// another key's (another caller's), so an exclusive run waits one seat's
/// deletes at most.
#[test]
fn a_retire_deletes_only_its_own_keys_tombstones() {
    let dir = fixture("true", "");
    let repo = workspace(dir.path(), "idle");
    assert_eq!(remote_pass(dir.path(), &repo).status.code(), Some(0));
    let key = key_of(dir.path(), &repo);
    age(dir.path(), &key, 2 * DAY_MS, false);
    let seats = dir.path().join("host/.local/state/ferry/v1/seats");
    let own = seats.join(format!(".retired-{key}-5"));
    let others = [
        seats.join(".retired-another-key-5"),
        seats.join(format!(".retired-{key}-longer-5")),
    ];
    for tomb in others.iter().chain([&own]) {
        fs::create_dir_all(tomb.join("checkout")).unwrap();
    }
    let (code, said) = prune_with(dir.path(), &["--idle", "1d"]);
    assert_eq!(code, Some(0), "{said}");
    assert!(said.starts_with("retired"), "{said}");
    assert!(!own.exists(), "its own tombstone stayed");
    for other in &others {
        assert!(
            other.exists(),
            "another key's tombstone was swept: {}",
            other.display()
        );
    }
}

/// murail-4a ferry-m4u: a prune a run makes by itself starts no retire past
/// its budget, even one whose `host-info` answered only after it: the seat
/// stays, and the summary says nothing was retired.
#[test]
fn a_prune_past_its_budget_starts_no_retire_after_a_slow_host_info() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    fs::write(
        dir.path().join("machines-top.toml"),
        "delete_unused_checkouts_after_days = 1\n",
    )
    .unwrap();
    let old = workspace(dir.path(), "old");
    assert_eq!(remote_pass(dir.path(), &old).status.code(), Some(0));
    let key = key_of(dir.path(), &old);
    fs::remove_file(dir.path().join("state/ferry/pruned.json")).unwrap();
    age(dir.path(), &key, 2 * DAY_MS, false);
    // Slow only once the run's own host-info has answered: the prune is
    // what waits, paused at its claim until the slowness is set.
    let slow = dir.path().join("slow-info");
    let run = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_AUTO_PRUNE_BUDGET_MS", "1000")
    .env("FERRY_TEST_SLOW_INFO", &slow)
    .env("FERRY_FAIL_AT", "prune-claim")
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    let claim = dir.path().join("state/ferry/prune-claim");
    let until = common::startup_deadline();
    while !claim.with_extension("paused").exists() {
        assert!(std::time::Instant::now() < until, "the prune never started");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    fs::write(&slow, "2").unwrap();
    fs::write(claim.with_extension("go"), "").unwrap();
    let out = run.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ferry: pruned test-host: retired 0"),
        "{stderr}"
    );
    let place = dir.path().join("host/.local/state/ferry/v1");
    assert!(
        place.join("seats").join(&key).join("checkout").exists(),
        "{stderr}"
    );
}

/// murail-4a ferry-6p0: the `used` record is written only inside the state
/// root: a seat directory that is a symbolic link out of it refuses the run,
/// and nothing is written where the link points.
#[test]
fn the_used_record_is_never_written_through_a_link() {
    let dir = fixture("true", "");
    assert_eq!(remote_pass(dir.path(), dir.path()).status.code(), Some(0));
    let key = key_of(dir.path(), dir.path());
    let seat = dir
        .path()
        .join("host/.local/state/ferry/v1/seats")
        .join(&key);
    fs::remove_dir_all(&seat).unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), &seat).unwrap();
    let out = remote_pass(dir.path(), dir.path());
    assert_eq!(out.status.code(), Some(125), "{}", receipt(&out));
    assert!(
        receipt(&out)["reason"]
            .as_str()
            .unwrap()
            .contains("seat used record"),
        "{}",
        receipt(&out)
    );
    assert_eq!(
        fs::read_dir(outside.path()).unwrap().count(),
        0,
        "written through the link"
    );
}

/// The book's `delete_unused_checkouts_after_days` is a whole number of
/// days, at least 1: anything else is a bad `machines.toml`.
#[test]
fn delete_unused_checkouts_after_days_is_whole_days_from_one() {
    for bad in ["0", "1.5", "-1", "'1d'", "36501", "18446744073709551615"] {
        let dir = fixture("true", "");
        fs::write(
            dir.path().join("machines-top.toml"),
            format!("delete_unused_checkouts_after_days = {bad}\n"),
        )
        .unwrap();
        let out = remote_pass(dir.path(), dir.path());
        assert_eq!(out.status.code(), Some(125), "{bad}");
    }
}

/// A failed append to the seat book is a note; the run is not the book's.
#[test]
fn a_failed_seat_book_append_is_a_note() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let out = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .env("FERRY_FAIL_AT", "seats-append")
    .output()
    .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or("")
            .contains("seat book"),
        "{record}"
    );
    assert_eq!(seats_book(dir.path()), Vec::<serde_json::Value>::new());
}

/// A seat over `seat_cap_bytes` starts cold: its preserve paths and cache
/// are wiped before the checkout, the record says `pruned`, and tracked
/// source is reset as always. It is measured at most once an hour, so the
/// stamp is removed to force the measurement (SPEC §9, slice 5).
#[test]
fn a_seat_over_its_cap_starts_cold_with_pruned() {
    let dir = fixture("true", "");
    fs::write(dir.path().join(".gitignore"), "big/\nconfig/\nstate/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['big']\n[recipe.pass]\ncommand=['sh','-c','if test -f big/blob; then echo warm; else echo cold; mkdir -p big; head -c 4096 /dev/zero > big/blob; fi']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "a preserved path that grows"]);
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1\nseat_cap_bytes = 1000\n",
    );
    let first = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(String::from_utf8_lossy(&first.stderr).contains("cold"));
    // Measured within the hour: not again, so the seat runs warm.
    let second = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(String::from_utf8_lossy(&second.stderr).contains("warm"));
    assert!(receipt(&second)["pruned"].is_null());
    let slot = only_checkout(dir.path());
    fs::remove_file(slot.parent().unwrap().join("measured")).unwrap();
    let third = run(dir.path(), &["run", "pass", "--local", "--json"]);
    let record = receipt(&third);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert!(String::from_utf8_lossy(&third.stderr).contains("cold"));
    assert_eq!(record["pruned"]["cap"], 1000, "{record}");
    assert!(
        record["pruned"]["bytes"].as_u64().unwrap() >= 4096,
        "{record}"
    );
    assert!(
        record["reason"].as_str().unwrap().contains("started cold"),
        "{record}"
    );
    let slot = only_checkout(dir.path());
    assert!(
        slot.join("ferry.toml").exists(),
        "tracked source was touched"
    );
    // A local seat lives inside its workspace: the book does not record it.
    assert_eq!(seats_book(dir.path()), Vec::<serde_json::Value>::new());
}

/// A failed wipe is a note, and the run goes on with what is there.
#[test]
fn a_failed_cap_wipe_is_a_note_and_the_run_goes_on() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    fs::write(dir.path().join(".gitignore"), "big/\nconfig/\nstate/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['big']\n[recipe.pass]\ncommand=['sh','-c','if test -f big/blob; then echo warm; else echo cold; mkdir -p big; head -c 4096 /dev/zero > big/blob; fi']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "a preserved path that grows"]);
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1\nseat_cap_bytes = 1000\n",
    );
    run(dir.path(), &["run", "pass", "--local", "--json"]);
    let measured = only_checkout(dir.path()).parent().unwrap().join("measured");
    fs::remove_file(measured).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("FERRY_FAIL_AT", "cap-wipe")
        .args(["run", "pass", "--local", "--json"])
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("warm"));
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("seat cap: cap-wipe"),
        "{record}"
    );
}

/// A live workspace prune cannot inspect is kept and reported, never
/// retired: unknown is not gone (slice 5 review P1-b; murail-4a's repro).
#[test]
fn seats_prune_keeps_a_workspace_it_cannot_inspect() {
    let dir = fixture("true", "");
    let hidden = dir.path().join("hidden");
    fs::create_dir(&hidden).unwrap();
    let live = workspace(dir.path(), "hidden/live");
    assert_eq!(remote_pass(dir.path(), &live).status.code(), Some(0));
    let key = key_of(dir.path(), &live);
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    let (code, said) = prune(dir.path(), ".local/bin/ferry");
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(code, Some(0), "{said}");
    assert!(said.contains("kept") && said.contains("unknown"), "{said}");
    let slot = dir
        .path()
        .join("host/.local/state/ferry/v1/seats")
        .join(&key)
        .join("checkout");
    assert!(slot.exists(), "{said}");
}

/// A workspace whose parent directory is missing, as under an unmounted
/// volume, is unknown and kept.
#[test]
fn seats_prune_keeps_a_workspace_whose_parent_is_missing() {
    let dir = fixture("true", "");
    fs::create_dir(dir.path().join("volume")).unwrap();
    let live = workspace(dir.path(), "volume/live");
    assert_eq!(remote_pass(dir.path(), &live).status.code(), Some(0));
    let key = key_of(dir.path(), &live);
    fs::remove_dir_all(dir.path().join("volume")).unwrap();
    let (code, said) = prune(dir.path(), ".local/bin/ferry");
    assert_eq!(code, Some(0), "{said}");
    assert!(said.contains("kept") && said.contains("unknown"), "{said}");
    let slot = dir
        .path()
        .join("host/.local/state/ferry/v1/seats")
        .join(&key)
        .join("checkout");
    assert!(slot.exists(), "{said}");
}

/// The cap measures and wipes only what is proven inside the seat: a
/// preserve path behind a symbolic link is neither, and its target survives
/// (slice 5 review P1-a; murail-4a's repro).
#[test]
fn the_cap_never_follows_a_symbolic_link_out_of_the_seat() {
    let dir = fixture("true", "");
    fs::write(dir.path().join(".gitignore"), "build/\nconfig/\nstate/\n").unwrap();
    fs::write(
        dir.path().join("ferry.toml"),
        "version=1\npreserve=['build/cache']\n[recipe.pass]\ncommand=['sh','-c','echo committed']\n",
    )
    .unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "nested preserve"]);
    let first = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(first.status.code(), Some(0), "{}", receipt(&first));
    let slot = only_checkout(dir.path());
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir(outside.path().join("cache")).unwrap();
    let sentinel = outside.path().join("cache/sentinel");
    fs::write(&sentinel, vec![1u8; 4096]).unwrap();
    std::os::unix::fs::symlink(outside.path(), slot.join("build")).unwrap();
    with_host_policy(
        dir.path(),
        "version=1\nmin_free_bytes=1\nseat_cap_bytes=1000\n",
    );
    let second = run(dir.path(), &["run", "pass", "--local", "--json"]);
    let record = receipt(&second);
    assert!(sentinel.exists(), "{record}");
    assert!(record["pruned"].is_null(), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("is behind a symbolic link: not measured or wiped"),
        "{record}"
    );
}

/// A place validates `preserve` before anything touches the disk: an
/// absolute path is refused at submit, and its target survives a cap of 0
/// (slice 5 review S2; the critic's s2).
#[test]
fn a_place_refuses_an_unvalidated_preserve_path_before_any_delete() {
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("precious"), "do not delete").unwrap();
    with_host_policy(
        dir.path(),
        "version=1\nmin_free_bytes=1\nseat_cap_bytes=0\n",
    );
    let state = dir.path().join("state");
    let request = serde_json::json!({
        "protocol": 1, "id": "1-aa", "token": "tok", "commit": "0".repeat(40), "seat": "s",
        "recipe": {"command": ["true"]}, "hooks": {},
        "timeout_seconds": null, "forwarded_env": {},
        "preserve": [victim.display().to_string()]
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .args(["place", "--state"])
        .arg(&state)
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
    let reply = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(125), "{reply}");
    assert!(victim.join("precious").exists(), "{reply}");
    assert!(!state.join("records/1-aa.json").exists());
}

/// Measuring runs before the recipe, inside its deadline: a run that has
/// spent its wait budget is not measured, and says so (slice 5 review P2).
#[test]
fn a_run_with_its_wait_budget_spent_is_not_measured() {
    let dir = fixture("true", "");
    fs::write(dir.path().join(".gitignore"), "big/\nconfig/\nstate/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['big']\n[recipe.pass]\ncommand=['sh','-c','mkdir -p big; head -c 4096 /dev/zero > big/blob']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "a preserved path that grows"]);
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1\nseat_cap_bytes = 1000\n",
    );
    let out = run(
        dir.path(),
        &["run", "pass", "--local", "--json", "--wait-seconds", "0"],
    );
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or("")
            .contains("not measured: the run's wait budget is spent"),
        "{record}"
    );
}

/// A workspace whose parent is now on another device than the one recorded
/// (an unmounted volume's empty mount point) is unknown and kept.
#[test]
fn seats_prune_keeps_a_workspace_whose_parent_changed_device() {
    let dir = fixture("true", "");
    let gone = workspace(dir.path(), "gone");
    assert_eq!(remote_pass(dir.path(), &gone).status.code(), Some(0));
    let key = key_of(dir.path(), &gone);
    fs::remove_dir_all(&gone).unwrap();
    // The same seat as recorded from another device.
    let mut line = seats_book(dir.path()).pop().unwrap();
    line["parent_device"] = (line["parent_device"].as_u64().unwrap() + 1).into();
    let book = dir.path().join("state/ferry/seats.jsonl");
    let mut text = fs::read_to_string(&book).unwrap();
    writeln!(text, "{line}").unwrap();
    fs::write(&book, text).unwrap();
    let (code, said) = prune(dir.path(), ".local/bin/ferry");
    assert_eq!(code, Some(0), "{said}");
    assert!(said.contains("kept") && said.contains("device"), "{said}");
    let slot = dir
        .path()
        .join("host/.local/state/ferry/v1/seats")
        .join(&key)
        .join("checkout");
    assert!(slot.exists(), "{said}");
}

/// Retire deletes only what is proven inside the place's root: through a
/// symbolic link it refuses, and the link's target survives (review P1-a).
#[test]
fn retire_never_follows_a_symbolic_link_out_of_the_place() {
    let dir = fixture("true", "");
    let gone = workspace(dir.path(), "gone");
    assert_eq!(remote_pass(dir.path(), &gone).status.code(), Some(0));
    let key = key_of(dir.path(), &gone);
    fs::remove_dir_all(&gone).unwrap();
    let place = dir.path().join("host/.local/state/ferry/v1");
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(outside.path().join(&key)).unwrap();
    let sentinel = outside.path().join(&key).join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let _ = fs::remove_dir_all(place.join("seats"));
    std::os::unix::fs::symlink(outside.path(), place.join("seats")).unwrap();
    let (code, said) = prune(dir.path(), ".local/bin/ferry");
    assert_eq!(code, Some(125), "{said}");
    assert!(said.contains("symbolic link"), "{said}");
    assert!(sentinel.exists(), "{said}");
}

/// herald-1b's diagnostic (eedc813a): a workspace whose lookup is denied is
/// unknown, not gone, and its seat survives.
#[test]
fn consumer_prune_must_not_treat_permission_denied_as_workspace_gone() {
    let dir = fixture("true", "");
    fs::create_dir(dir.path().join("private")).unwrap();
    let live = workspace(dir.path(), "private/live");
    assert_eq!(remote_pass(dir.path(), &live).status.code(), Some(0));
    let key = key_of(dir.path(), &live);
    let parent = dir.path().join("private");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();
    let observation = live.try_exists();
    let (code, said) = prune(dir.path(), ".local/bin/ferry");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        observation.is_err(),
        "fixture must establish unknown, not absence"
    );
    assert!(live.exists(), "the caller workspace still exists");
    assert!(
        dir.path()
            .join("host/.local/state/ferry/v1/seats")
            .join(key)
            .join("checkout")
            .exists(),
        "prune deleted a live workspace's seat: code={code:?}, {said}"
    );
}

/// A directory swapped for a symbolic link while a wipe runs is unlinked,
/// never followed: the delete walks directory handles (std's
/// `remove_dir_all`), not paths. `remove-walk` holds the delete after its
/// checks, so the swap is deterministic (ferry-xtx.23; the critic's race).
#[test]
fn a_link_swapped_in_during_a_wipe_is_never_followed() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    fs::write(dir.path().join(".gitignore"), "big/\nconfig/\nstate/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['big']\n[recipe.pass]\ncommand=['sh','-c','mkdir -p big/sub; head -c 4096 /dev/zero > big/sub/blob']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "a preserved tree that grows"]);
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1\nseat_cap_bytes = 1000\n",
    );
    run(dir.path(), &["run", "pass", "--local", "--json"]);
    let state = dir.path().join(".ferry/local/v1");
    let slot = only_checkout(dir.path());
    let key = slot
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    fs::remove_file(state.join(format!("seats/{key}/measured"))).unwrap();
    // The cache's own delete is not held.
    fs::write(state.join(format!("seats/{key}/cache.go")), "").unwrap();
    let victim = tempfile::tempdir().unwrap();
    fs::write(victim.path().join("precious"), "keep").unwrap();
    let wipe = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("FERRY_FAIL_AT", "remove-walk")
        .args(["run", "pass", "--local", "--json"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = slot.join("big.paused");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !paused.exists() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(paused.exists(), "the wipe never paused");
    fs::remove_dir_all(slot.join("big/sub")).unwrap();
    std::os::unix::fs::symlink(victim.path(), slot.join("big/sub")).unwrap();
    fs::write(slot.join("big.go"), "").unwrap();
    let out = wipe.wait_with_output().unwrap();
    let record = receipt(&out);
    assert!(victim.path().join("precious").exists(), "{record}");
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(record["pruned"]["cap"], 1000, "{record}");
}

/// A wipe fails closed: a subdirectory it cannot inspect stops the delete
/// with a note, and nothing in the preserve tree is wiped (ferry-xtx.23).
#[test]
fn a_wipe_that_cannot_inspect_its_tree_deletes_nothing() {
    let dir = fixture("true", "");
    fs::write(dir.path().join(".gitignore"), "big/\nconfig/\nstate/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['big']\n[recipe.pass]\ncommand=['sh','-c','mkdir -p big/locked big/other; head -c 4096 /dev/zero > big/other/blob']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "a preserved tree"]);
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1\nseat_cap_bytes = 1000\n",
    );
    run(dir.path(), &["run", "pass", "--local", "--json"]);
    let state = dir.path().join(".ferry/local/v1");
    let slot = only_checkout(dir.path());
    let key = slot
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    fs::remove_file(state.join(format!("seats/{key}/measured"))).unwrap();
    fs::write(slot.join("big/other/keep"), "keep").unwrap();
    // After the preserve path fails, the wipe still reaches the cache.
    let cache = state.join(format!("seats/{key}/cache"));
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("blob"), "x").unwrap();
    let locked = slot.join("big/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert!(slot.join("big/other/keep").exists(), "{record}");
    // The run makes its cache again; what the wipe reached is gone.
    assert!(
        !cache.join("blob").exists(),
        "the wipe stopped at the first failure: {record}"
    );
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or("")
            .contains("not deleted"),
        "{record}"
    );
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or("")
            .contains("could not be inspected"),
        "{record}"
    );
}

/// A wipe path that is a regular file is deleted as one, not left as an
/// error (ferry-xtx.24 (a)).
#[test]
fn a_file_at_a_wipe_path_is_deleted() {
    let dir = fixture("true", "");
    fs::write(dir.path().join(".gitignore"), "big/\nconfig/\nstate/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['big']\n[recipe.pass]\ncommand=['sh','-c','mkdir -p big; head -c 4096 /dev/zero > big/blob']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "a preserved tree"]);
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1\nseat_cap_bytes = 1000\n",
    );
    run(dir.path(), &["run", "pass", "--local", "--json"]);
    let state = dir.path().join(".ferry/local/v1");
    let slot = only_checkout(dir.path());
    let key = slot
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    fs::remove_file(state.join(format!("seats/{key}/measured"))).unwrap();
    let cache = state.join(format!("seats/{key}/cache"));
    let _ = fs::remove_dir_all(&cache);
    fs::write(&cache, "a file where a directory belongs").unwrap();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(record["pruned"]["cap"], 1000, "{record}");
    assert!(!cache.is_file(), "{record}");
    assert!(
        !record["reason"]
            .as_str()
            .unwrap_or("")
            .contains("seat cap:"),
        "{record}"
    );
}

/// Containment is proven again after the device walk, right before the
/// delete: a seat directory swapped for a link while the walk ran stops the
/// delete, and the link's target survives (ferry-xtx.24 (b)).
#[test]
fn a_seat_swapped_for_a_link_after_the_walk_is_not_deleted_through() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    fs::write(dir.path().join(".gitignore"), "big/\nconfig/\nstate/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['big']\n[recipe.pass]\ncommand=['sh','-c','mkdir -p big; head -c 4096 /dev/zero > big/blob']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "a preserved tree"]);
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1\nseat_cap_bytes = 1000\n",
    );
    run(dir.path(), &["run", "pass", "--local", "--json"]);
    let state = dir.path().join(".ferry/local/v1");
    let slot = only_checkout(dir.path());
    let key = slot
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    fs::remove_file(state.join(format!("seats/{key}/measured"))).unwrap();
    fs::write(state.join(format!("seats/{key}/cache.go")), "").unwrap();
    let victim = tempfile::tempdir().unwrap();
    fs::create_dir_all(victim.path().join("big")).unwrap();
    fs::write(victim.path().join("big/precious"), "keep").unwrap();
    let wipe = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("FERRY_FAIL_AT", "remove-walk")
        .args(["run", "pass", "--local", "--json"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = slot.join("big.paused");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !paused.exists() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(paused.exists(), "the wipe never paused");
    fs::rename(&slot, state.join("moved-away")).unwrap();
    std::os::unix::fs::symlink(victim.path(), &slot).unwrap();
    fs::write(victim.path().join("big.go"), "").unwrap();
    let out = wipe.wait_with_output().unwrap();
    let record = receipt(&out);
    assert!(victim.path().join("big/precious").exists(), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or("")
            .contains("is behind a symbolic link: not deleted"),
        "{record}"
    );
}

/// SPEC §12 item 22: with `delete_run_history_after_days = 1`, a run deletes,
/// after its record is final, the history of a final run from two days ago
/// and of a two-day-old reservation whose supervisor is gone (reconciled
/// first), and keeps an hour-old run and a live supervisor's reservation.
/// Its recipe starts with nothing deleted. Retirement records, an earlier
/// pass's account and a crashed write's temporary file age on the same
/// rule; seats, `store/` and the caller's receipts stay.
#[test]
fn run_history_older_than_its_days_is_deleted_after_the_record_is_final() {
    let dir = fixture("true", "");
    let root = local_root(dir.path());
    let first = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(first.status.code(), Some(0), "{}", receipt(&first));
    let first_id = receipt(&first)["id"].as_str().unwrap().to_owned();
    let none = serde_json::json!({});
    let old = plant(&root, "old-final", 2 * DAY_MS, false, &none);
    let recent = plant(&root, "recent-final", 3_600_000, false, &none);
    let live = plant(
        &root,
        "old-live",
        2 * DAY_MS,
        true,
        &serde_json::json!({"supervisor": {"pid": std::process::id(), "start": null}}),
    );
    let lost = plant(
        &root,
        "old-lost",
        2 * DAY_MS,
        true,
        &serde_json::json!({
            "supervisor": {"pid": dead_pid(), "start": 1},
            "boot_id": "another-boot",
        }),
    );
    // A crashed finalize: final record and reservation both; deleting the
    // run deletes both, so no reconcile brings it back as lost.
    let both = plant(&root, "old-both", 2 * DAY_MS, false, &none);
    let final_record = root.join("records/old-both.json");
    let reserved_copy = root.join("records/old-both.reserved.json");
    fs::copy(&final_record, &reserved_copy).unwrap();
    let records = root.join("records");
    let then = plant_other_records(&records);
    // The recipe proves nothing was deleted before it started.
    fs::write(
        dir.path().join("ferry.toml"),
        format!(
            "version=1\n[recipe.check]\ncommand=['sh','-c','test -e {}']\nroutable=true\n",
            old.join("output.log").display()
        ),
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(
        dir.path(),
        &["commit", "-m", "a recipe that sees the old run"],
    );
    with_host_policy(dir.path(), HISTORY_DAY);
    let out = run(dir.path(), &["run", "check", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let (passes, deleted) = settled(dir.path(), &root, &id);
    let logged = format!("{passes:?}");
    assert_eq!(deleted, 3, "{logged}");
    for gone in [&old, &lost, &both] {
        assert!(!gone.exists(), "{logged}: {}", gone.display());
    }
    for name in [
        "old-final.json",
        "old-lost.json",
        "old-lost.reserved.json",
        "old-both.json",
        "old-both.reserved.json",
        "retired-oldkey.json",
        ".old-final.json.1.0.tmp",
        &format!("history-{then}-earlier.json"),
    ] {
        assert!(!records.join(name).exists(), "{logged}: {name}");
    }
    for kept in [&recent, &live] {
        assert!(
            kept.join("output.log").exists(),
            "{logged}: {}",
            kept.display()
        );
    }
    for name in [
        "recent-final.json",
        "old-live.reserved.json",
        "retired-newkey.json",
        "retired-ageless.json",
        ".recent-final.json.1.0.tmp",
    ] {
        assert!(records.join(name).exists(), "{logged}: {name}");
    }
    let (by, account) = passes
        .iter()
        .find_map(|(id, _)| Some((id, history_account(&root, id)?)))
        .expect("no account");
    assert_eq!(account["older_than_days"], 1, "{account}");
    assert_eq!(account["run"], by.as_str(), "{account}");
    assert_eq!(host_record_at(&root, &id)["state"], "final");
    assert!(fs::read_dir(root.join("seats")).unwrap().next().is_some());
    assert!(root.join("store").exists());
    assert!(
        dir.path()
            .join(format!(".ferry/receipts/{first_id}.json"))
            .exists()
    );
    assert_eq!(host_record_at(&root, &first_id)["state"], "final");
}

/// SPEC §12 item 22: without the key nothing is deleted; `0` or `1.5`
/// refuses the run as a bad `host.toml`, naming the key.
#[test]
fn delete_run_history_after_days_is_whole_days_from_one_and_absent_keeps_all() {
    for (days, refused) in [(None, false), (Some("0"), true), (Some("1.5"), true)] {
        let dir = fixture("true", "");
        let root = local_root(dir.path());
        assert_eq!(
            run(dir.path(), &["run", "pass", "--local", "--json"])
                .status
                .code(),
            Some(0)
        );
        let old = plant(
            &root,
            "old-final",
            2 * DAY_MS,
            false,
            &serde_json::json!({}),
        );
        let mut policy = "version = 1\nmin_free_bytes = 1\n".to_owned();
        if let Some(days) = days {
            writeln!(policy, "delete_run_history_after_days = {days}").unwrap();
        }
        with_host_policy(dir.path(), &policy);
        let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
        let record = receipt(&out);
        if refused {
            assert_eq!(out.status.code(), Some(125), "{days:?}: {record}");
            assert!(
                record["reason"]
                    .as_str()
                    .unwrap()
                    .contains("delete_run_history_after_days"),
                "{days:?}: {record}"
            );
        } else {
            assert_eq!(out.status.code(), Some(0), "{record}");
        }
        supervisor_exited(&root, record["id"].as_str().unwrap());
        assert!(old.join("output.log").exists(), "{days:?}");
        assert!(root.join("records/old-final.json").exists(), "{days:?}");
    }
}

/// SPEC §12 item 22: an old final run whose recorded survivor is still
/// alive is kept, with or without a recorded start (the start only where
/// the test can read it: Linux); one whose pid lives with another start,
/// whose survivor is gone, or whose survivors are `unknown` is deleted.
#[test]
fn an_old_run_with_a_live_survivor_is_kept() {
    let dir = fixture("true", "");
    let root = local_root(dir.path());
    assert_eq!(
        run(dir.path(), &["run", "pass", "--local", "--json"])
            .status
            .code(),
        Some(0)
    );
    let me = std::process::id();
    let survivors = |list: serde_json::Value| serde_json::json!({"custody": {"survivors": list}});
    let mut kept = vec![plant(
        &root,
        "live-no-start",
        2 * DAY_MS,
        false,
        &survivors(serde_json::json!([{"pid": me}])),
    )];
    if let Some(start) = own_start() {
        kept.push(plant(
            &root,
            "live-start",
            2 * DAY_MS,
            false,
            &survivors(serde_json::json!([{"pid": me, "start": start}])),
        ));
    }
    let deleted = [
        plant(
            &root,
            "pid-reused",
            2 * DAY_MS,
            false,
            &survivors(serde_json::json!([{"pid": me, "start": 1}])),
        ),
        plant(
            &root,
            "survivor-gone",
            2 * DAY_MS,
            false,
            &survivors(serde_json::json!([{"pid": dead_pid(), "start": null}])),
        ),
        plant(
            &root,
            "survivors-unknown",
            2 * DAY_MS,
            false,
            &survivors(serde_json::json!("unknown")),
        ),
    ];
    with_host_policy(dir.path(), HISTORY_DAY);
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let (passes, count) = settled(dir.path(), &root, &id);
    let line = &passes.last().unwrap().1;
    assert!(
        line.contains(&format!("kept {} with a live survivor", kept.len())),
        "{passes:?}"
    );
    for folder in &kept {
        assert!(folder.exists(), "{line}: {}", folder.display());
    }
    for folder in &deleted {
        assert!(!folder.exists(), "{line}: {}", folder.display());
    }
    assert_eq!(count, 3, "{passes:?}");
}

/// SPEC §12 item 22: the pass holds a shared host lease: held mid-delete, an
/// exclusive run submitted then does not start its recipe until the pass
/// has released it; with an exclusive run holding `host`, the pass is
/// skipped and deletes nothing.
#[test]
fn the_history_pass_never_runs_beside_an_exclusive_run() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let root = local_root(dir.path());
    let marked = dir.path().join("marked");
    fs::write(
        dir.path().join("ferry.toml"),
        format!(
            "version=1\n[recipe.pass]\ncommand=['sh','-c','echo committed']\nroutable=true\n[recipe.mark]\ncommand=['sh','-c','touch {}']\nroutable=true\n",
            marked.display()
        ),
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(
        dir.path(),
        &["commit", "-m", "a recipe that marks its start"],
    );
    assert_eq!(
        run(dir.path(), &["run", "pass", "--local", "--json"])
            .status
            .code(),
        Some(0)
    );
    // Name order is the reverse of age order (as listed on APFS), so only
    // the pass's own order deletes the older first.
    let first = plant(&root, "900-old", 3 * DAY_MS, false, &serde_json::json!({}));
    let second = plant(&root, "1000-old", 2 * DAY_MS, false, &serde_json::json!({}));
    with_host_policy(dir.path(), HISTORY_DAY);
    let out = command(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .env("FERRY_FAIL_AT", "history-pause")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let pause = root.join("runs").join(&id).join("history");
    let until = common::startup_deadline();
    while !pause.with_extension("paused").exists() {
        assert!(std::time::Instant::now() < until, "the pass never paused");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!first.exists() && second.exists(), "oldest first");
    let exclusive = command(dir.path())
        .args([
            "run",
            "mark",
            "--local",
            "--json",
            "--exclusive",
            "--seat",
            "other",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let guard = OwnedPid(i32::try_from(exclusive.id()).unwrap());
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(
        !marked.exists(),
        "an exclusive recipe started beside the pass"
    );
    fs::write(pause.with_extension("go"), "").unwrap();
    let ended = exclusive.wait_with_output().unwrap();
    drop(guard);
    assert_eq!(ended.status.code(), Some(0), "{}", receipt(&ended));
    assert!(marked.exists());
    let line = history_logged(&root, &id);
    assert!(line.contains("deleted 1 runs"), "{line}");

    // An exclusive run holding `host`, or waiting at the gate: skipped.
    // (The exclusive run's own pass deleted the second old run.)
    for lock in ["host", "host.gate"] {
        let third = plant(
            &root,
            "old-three",
            2 * DAY_MS,
            false,
            &serde_json::json!({}),
        );
        let out = command(dir.path())
            .args(["run", "pass", "--local", "--json"])
            .env("FERRY_FAIL_AT", "history-start")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
        let id = receipt(&out)["id"].as_str().unwrap().to_owned();
        let start = root.join("runs").join(&id).join("history-start");
        while !start.with_extension("paused").exists() {
            assert!(std::time::Instant::now() < until, "the pass never paused");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let held = fs::File::open(root.join("locks").join(lock)).unwrap();
        fs2::FileExt::try_lock_exclusive(&held).unwrap();
        fs::write(start.with_extension("go"), "").unwrap();
        let line = history_logged(&root, &id);
        assert!(line.contains("skipped: an exclusive run"), "{lock}: {line}");
        assert!(third.join("output.log").exists(), "{lock}: {line}");
        drop(held);
    }
}

/// SPEC §9: the pass stops starting work at its budget, and the rest waits
/// for the next run: with no budget a run deletes nothing and says so; the
/// next, with its own, deletes every old run.
#[test]
fn a_history_pass_past_its_budget_leaves_the_rest_for_the_next_run() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let root = local_root(dir.path());
    assert_eq!(
        run(dir.path(), &["run", "pass", "--local", "--json"])
            .status
            .code(),
        Some(0)
    );
    let old: Vec<_> = (0..20)
        .map(|n| {
            plant(
                &root,
                &format!("{n}-old"),
                2 * DAY_MS,
                false,
                &serde_json::json!({}),
            )
        })
        .collect();
    with_host_policy(dir.path(), HISTORY_DAY);
    let out = command(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .env("FERRY_HISTORY_BUDGET_MS", "0")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let line = history_logged(&root, &id);
    assert!(
        line.contains("deleted 0 runs") && line.contains("budget"),
        "{line}"
    );
    // Nothing started past the budget: not even a read.
    assert!(line.contains("read 0,"), "{line}");
    assert!(old.iter().all(|folder| folder.exists()), "{line}");
    assert!(history_account(&root, &id).is_none(), "{line}");
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let (passes, deleted) = settled(dir.path(), &root, &id);
    assert_eq!(deleted, 20, "{passes:?}");
    assert!(old.iter().all(|folder| !folder.exists()), "{passes:?}");
}

/// SPEC §12 item 22: with the deletion injected to fail after a run's
/// folder, that run's record stays and the count leaves it out (other old
/// records still go); the run's own record is unchanged, and the next pass
/// deletes the record and counts the run.
#[test]
fn a_run_whose_record_failed_to_delete_is_deleted_by_the_next_pass() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let root = local_root(dir.path());
    assert_eq!(
        run(dir.path(), &["run", "pass", "--local", "--json"])
            .status
            .code(),
        Some(0)
    );
    let old = plant(
        &root,
        "old-final",
        2 * DAY_MS,
        false,
        &serde_json::json!({}),
    );
    fs::write(
        root.join("records/retired-oldkey.json"),
        serde_json::json!({"key": "oldkey", "retired_ms": now_ms() - 2 * DAY_MS}).to_string(),
    )
    .unwrap();
    with_host_policy(dir.path(), HISTORY_DAY);
    let out = command(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .env("FERRY_FAIL_AT", "history-record")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let own = host_record_at(&root, &id);
    let line = history_logged(&root, &id);
    assert!(line.contains("history-record"), "{line}");
    assert!(!old.exists(), "{line}");
    assert!(root.join("records/old-final.json").exists(), "{line}");
    assert!(!root.join("records/retired-oldkey.json").exists(), "{line}");
    assert_eq!(history_account(&root, &id).unwrap()["deleted"], 0);
    assert_eq!(host_record_at(&root, &id), own);
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let (passes, deleted) = settled(dir.path(), &root, &id);
    assert!(!root.join("records/old-final.json").exists(), "{passes:?}");
    assert_eq!(deleted, 1, "{passes:?}");

    // A stale reservation beside the final record, with the injection
    // between the reservation and the record: the final record stays, as
    // it was, and nothing reconciles it as lost.
    plant(&root, "old-both", 2 * DAY_MS, false, &serde_json::json!({}));
    let final_record = root.join("records/old-both.json");
    let planted = fs::read(&final_record).unwrap();
    fs::copy(&final_record, root.join("records/old-both.reserved.json")).unwrap();
    let out = command(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .env("FERRY_FAIL_AT", "history-final")
        .output()
        .unwrap();
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let line = history_logged(&root, &id);
    assert!(line.contains("history-final"), "{line}");
    assert!(
        !root.join("records/old-both.reserved.json").exists(),
        "{line}"
    );
    assert_eq!(fs::read(&final_record).unwrap(), planted, "{line}");
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let (passes, deleted) = settled(dir.path(), &root, &id);
    assert!(!final_record.exists(), "{passes:?}");
    assert_eq!(deleted, 1, "{passes:?}");
}

/// SPEC §12 item 22, progress: a pass never reads a run named under 23
/// hours old (at one day), and names without a time come last, temporary
/// files first. So with more young runs and young or timeless retirement
/// records than one pass can read, the FIRST pass still deletes the
/// two-day-old run and, by its age and unread, a crashed write's temporary
/// file. Debug builds make the pass's work two entries (exactly those two);
/// a release build checks that no pass reads a young run.
#[test]
fn a_history_pass_reads_no_young_run_and_reaches_the_old_ones() {
    let dir = fixture("true", "");
    let root = local_root(dir.path());
    assert_eq!(
        run(dir.path(), &["run", "pass", "--local", "--json"])
            .status
            .code(),
        Some(0)
    );
    let none = serde_json::json!({});
    let old = plant(
        &root,
        &format!("{}-old", now_ms() - 2 * DAY_MS),
        2 * DAY_MS,
        false,
        &none,
    );
    let young = 22 * 3_600_000;
    let young_runs: Vec<_> = (0..30)
        .map(|n| {
            plant(
                &root,
                &format!("{}-young{n}", now_ms() - young),
                young,
                false,
                &none,
            )
        })
        .collect();
    let records = root.join("records");
    for n in 0..30 {
        fs::write(
            records.join(format!("retired-young{n}.json")),
            serde_json::json!({"retired_ms": now_ms()}).to_string(),
        )
        .unwrap();
    }
    for n in 0..10 {
        fs::write(records.join(format!("retired-ageless{n}.json")), "{}").unwrap();
    }
    let crashed = records.join(".x.json.1.0.tmp");
    fs::write(&crashed, "not json").unwrap();
    age_file(&crashed, 2 * DAY_MS);
    with_host_policy(dir.path(), HISTORY_DAY);
    let mut first = command(dir.path());
    first.args(["run", "pass", "--local", "--json"]);
    if cfg!(debug_assertions) {
        first.env("FERRY_HISTORY_MAX_ENTRIES", "2");
    }
    let out = first.output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let line = history_logged(&root, &id);
    if cfg!(debug_assertions) {
        // Two entries: the old run (read) and the temporary file (a stat);
        // then the pass stops, every retirement record unread.
        assert!(line.contains("deleted 1 runs and 1 other"), "{line}");
        assert!(
            line.contains("read 1,") && line.contains("stopped at its"),
            "{line}"
        );
        assert!(!old.exists() && !crashed.exists(), "{line}");
        assert!(records.join("retired-ageless0.json").exists(), "{line}");
    } else {
        // At most the old run and the forty retirement records.
        let (passes, deleted) = settled(dir.path(), &root, &id);
        assert!(
            passes.iter().all(|(_, line)| read_of(line) <= 41),
            "{passes:?}"
        );
        assert_eq!(deleted, 1, "{passes:?}");
        assert!(!old.exists() && !crashed.exists(), "{passes:?}");
    }
    assert!(young_runs.iter().all(|folder| folder.exists()), "{line}");
}

/// SPEC §12 item 22: `ferry record` and `ferry follow` on a run whose
/// history was deleted name the setting, as context.
#[test]
fn a_deleted_runs_record_names_how_long_the_host_keeps_history() {
    let dir = fixture("true", "");
    let root = local_root(dir.path());
    with_host_policy(dir.path(), HISTORY_DAY);
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
    let real = receipt(&out)["id"].as_str().unwrap().to_owned();
    supervisor_exited(&root, &real);
    // The same run as if submitted two days ago (its id carries its submit
    // time), by a caller that lost its end: its receipt stays submitted.
    let then = now_ms() - 2 * DAY_MS;
    let id = format!("{then}-{}", real.split_once('-').unwrap().1);
    let mut mine: serde_json::Value = serde_json::from_slice(
        &fs::read(dir.path().join(format!(".ferry/receipts/{real}.json"))).unwrap(),
    )
    .unwrap();
    mine["id"] = id.as_str().into();
    mine["state"] = "submitted".into();
    fs::write(
        dir.path().join(format!(".ferry/receipts/{id}.json")),
        mine.to_string(),
    )
    .unwrap();
    let path = root.join(format!("records/{id}.json"));
    let mut record = host_record_at(&root, &real);
    record["id"] = id.as_str().into();
    record["submitted_ms"] = serde_json::json!(then);
    fs::write(&path, record.to_string()).unwrap();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    let later = receipt(&out)["id"].as_str().unwrap().to_owned();
    let (passes, _) = settled(dir.path(), &root, &later);
    assert!(!path.exists(), "{passes:?}");
    let said = "not found (this host keeps run history for 1 days)";
    let out = run(dir.path(), &["record", &id]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(said), "{stderr}");
    let out = run(dir.path(), &["follow", &id]);
    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(both.contains(said), "{both}");
}

/// ferry-83k (SPEC §9: a run that recreates a seat makes it live again): a
/// prune's acknowledgement held after the place retired the seat, while a
/// fresh run recreates it, never hides the recreated seat. Its late
/// retirement meant an older use than the book now holds, so the next
/// prune still considers the seat and retires it.
#[test]
fn a_late_retirement_never_hides_a_recreated_seat() {
    use std::time::{Duration, Instant};
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture("true", "");
    let repo = workspace(dir.path(), "reused");
    assert_eq!(remote_pass(dir.path(), &repo).status.code(), Some(0));
    let key = key_of(dir.path(), &repo);
    age(dir.path(), &key, 2 * DAY_MS, false);
    let place = dir.path().join("host/.local/state/ferry/v1");
    let paused = dir.path().join("state/ferry/prune-ack.paused");
    let held = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["seats", "prune", "--idle", "1d"],
    )
    .env("FERRY_FAIL_AT", "prune-ack")
    .stdout(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    let guard = OwnedPid(i32::try_from(held.id()).unwrap());
    let until = common::startup_deadline();
    while !paused.exists() {
        assert!(Instant::now() < until, "the prune never reached its ack");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The place retired the seat; before the prune books it, a fresh run
    // recreates it.
    assert!(!place.join("seats").join(&key).exists());
    assert_eq!(remote_pass(dir.path(), &repo).status.code(), Some(0));
    assert!(place.join("seats").join(&key).join("checkout").exists());
    fs::write(dir.path().join("state/ferry/prune-ack.go"), "").unwrap();
    let out = held.wait_with_output().unwrap();
    drop(guard);
    let said = String::from_utf8_lossy(&out.stdout);
    let workspace = repo.canonicalize().unwrap().display().to_string();
    assert!(
        said.starts_with("retired") && said.contains(&workspace),
        "{said}"
    );
    // The recreated seat is still in the book: the next prune retires it.
    let (code, said) = prune_with(dir.path(), &["--idle", "0s"]);
    assert_eq!(code, Some(0), "{said}");
    assert!(
        said.starts_with("retired") && said.contains(&workspace),
        "{said}"
    );
    assert!(!place.join("seats").join(&key).exists(), "{said}");
}

/// SPEC §12 item 24, `host.toml`: a version 1 key in a version 2 policy is
/// refused naming the new key; so is a size without a unit, `max_timeout`
/// (named version 1 in SPEC §10), a `max_run_time` outside 1s to 7d, and
/// `max_run_time` in a version 1 policy.
#[test]
fn a_version_two_host_policy_names_its_rules() {
    let dir = fixture("true", "");
    for (policy, said) in [
        (
            "version = 2\nmin_free_bytes = 1\n",
            "`min_free_bytes` is version 1: use `refuse_runs_below_free_disk`",
        ),
        (
            "version = 2\nrefuse_runs_below_free_disk = 40\n",
            "a size with a unit",
        ),
        (
            "version = 2\nrefuse_runs_below_free_disk = '40'\n",
            "a size with a unit",
        ),
        (
            "version = 1\nmin_free_bytes = 1\nmax_run_time = '1h'\n",
            "needs version = 2",
        ),
        (
            "version = 2\nrefuse_runs_below_free_disk = '1 B'\nmax_timeout = 60\n",
            "`max_timeout` is version 1: use `max_run_time`",
        ),
        (
            "version = 2\nrefuse_runs_below_free_disk = '1 B'\nmax_run_time = '0s'\n",
            "from 1s to 7d",
        ),
        (
            "version = 2\nrefuse_runs_below_free_disk = '1 B'\nmax_run_time = '8d'\n",
            "from 1s to 7d",
        ),
    ] {
        with_host_policy(dir.path(), policy);
        let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
        let record = receipt(&out);
        assert_eq!(out.status.code(), Some(125), "{policy}: {record}");
        assert!(
            record["reason"].as_str().unwrap_or_default().contains(said),
            "{policy}: {record}"
        );
    }
}

/// murail-4a's seam: the policy read at admission, under the seat lock,
/// decides a run's timeout, and is the one its receipt records. A run that
/// waits at the host lease while `host.toml` lowers `max_run_time` from 6h
/// to 1h is refused at admission, naming the new limit.
#[test]
fn a_policy_lowered_while_a_run_waits_refuses_it_at_admission() {
    use std::time::{Duration, Instant};
    let dir = fixture("true", "");
    let root = local_root(dir.path());
    let policy = |time: &str| {
        with_host_policy(
            dir.path(),
            &format!("version = 2\nrefuse_runs_below_free_disk = '1 B'\nmax_run_time = '{time}'\n"),
        );
    };
    policy("6h");
    assert_eq!(
        run(dir.path(), &["run", "pass", "--local", "--json"])
            .status
            .code(),
        Some(0)
    );
    let held = fs::File::open(root.join("locks/host")).unwrap();
    fs2::FileExt::try_lock_exclusive(&held).unwrap();
    let waiting = command(dir.path())
        .args([
            "run",
            "pass",
            "--local",
            "--json",
            "--timeout-seconds",
            "7200",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let guard = OwnedPid(i32::try_from(waiting.id()).unwrap());
    // Submitted under 6h, it now waits for the host lease.
    let until = Instant::now() + common::slack();
    while !fs::read_dir(root.join("records"))
        .unwrap()
        .flatten()
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".reserved.json")
        })
    {
        assert!(Instant::now() < until, "the run was never submitted");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300));
    policy("1h");
    drop(held);
    let out = waiting.wait_with_output().unwrap();
    drop(guard);
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("max_run_time (3600 s)"),
        "{record}"
    );
    assert_eq!(record["admission"]["result"], "refused", "{record}");

    // herald-1b's case: allowed under 7d at submit, the policy removed while
    // it waits. The default limit (6h) applies at admission all the same.
    policy("7d");
    let held = fs::File::open(root.join("locks/host")).unwrap();
    fs2::FileExt::try_lock_exclusive(&held).unwrap();
    let reserved = fs::read_dir(root.join("records")).unwrap().count();
    let waiting = command(dir.path())
        .args([
            "run",
            "pass",
            "--local",
            "--json",
            "--timeout-seconds",
            "172800",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let guard = OwnedPid(i32::try_from(waiting.id()).unwrap());
    let until = Instant::now() + common::slack();
    while fs::read_dir(root.join("records")).unwrap().count() == reserved {
        assert!(Instant::now() < until, "the run was never submitted");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300));
    fs::remove_file(dir.path().join("config/ferry/host.toml")).unwrap();
    drop(held);
    let out = waiting.wait_with_output().unwrap();
    drop(guard);
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("max_run_time (21600 s)"),
        "{record}"
    );
    assert_eq!(record["admission"]["policy"], "not-configured", "{record}");
}
