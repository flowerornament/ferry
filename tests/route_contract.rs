//! What may run, and where (SPEC §3, §10, §12): recipes read from the exact
//! commit, adapters and both config versions, the environment, needs ⊆ has on
//! every route, reserved machines, nesting, and the refusals a caller
//! records.
#![allow(clippy::wildcard_imports)]
mod common;
use common::*;

#[test]
fn published_companion_uses_tracking_ref_on_git_jj_and_worktree_callers() {
    for backend in ["git", "jj", "worktree"] {
        let dir = fixture("true", "");
        let first = Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let first = String::from_utf8(first.stdout).unwrap().trim().to_owned();
        git(
            dir.path(),
            &["update-ref", "refs/remotes/origin/master", &first],
        );
        fs::write(dir.path().join("ferry.toml"), "version=1\n[[ship]]\nref='refs/remotes/origin/master'\nfrom={remote='origin',branch='master'}\n[recipe.pass]\ncommand=['git','rev-parse','refs/remotes/origin/master']\n").unwrap();
        git(dir.path(), &["add", "ferry.toml"]);
        git(dir.path(), &["commit", "-m", "typed companion"]);
        let caller = if backend == "worktree" {
            let path = dir.path().join("worktree");
            git(
                dir.path(),
                &["worktree", "add", "--detach", path.to_str().unwrap()],
            );
            path
        } else {
            if backend == "jj" {
                let out = Command::new("jj")
                    .current_dir(dir.path())
                    .args(["git", "init", "--colocate"])
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            dir.path().to_owned()
        };
        let out = run(&caller, &["run", "pass", "--local", "--json"]);
        assert!(
            out.status.success(),
            "{backend}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let record = receipt(&out);
        let shipped = serde_json::json!([{"ref": "refs/remotes/origin/master", "commit": first,
            "from": {"remote": "origin", "branch": "master"}}]);
        assert_eq!(record["ship"], shipped);
        assert!(String::from_utf8_lossy(&out.stdout).contains(&first));
        git(
            dir.path(),
            &["update-ref", "-d", "refs/remotes/origin/master"],
        );
        let out = run(&caller, &["run", "pass", "--local", "--json"]);
        assert_eq!(out.status.code(), Some(125));
        assert_eq!(receipt(&out)["ship"], serde_json::json!([]));
        for entry in fs::read_dir(caller.join(".ferry/local/v1/seats")).unwrap() {
            let output = Command::new("git")
                .current_dir(entry.unwrap().path().join("checkout"))
                .args(["rev-parse", "refs/remotes/origin/master"])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), first);
        }
    }
}

#[test]
fn published_companion_refuses_malformed_sources_and_tag_objects() {
    let dir = fixture("true", "");
    for source in [
        "{remote='origin'}",
        "{branch='master'}",
        "{remote='origin',branch='master',revision='HEAD'}",
        "{remote='-origin',branch='master'}",
        "{remote='origin',branch='../master'}",
    ] {
        fs::write(dir.path().join("ferry.toml"), format!("version=1\n[[ship]]\nref='refs/remotes/origin/master'\nfrom={source}\n[recipe.pass]\ncommand=['true']\n")).unwrap();
        git(dir.path(), &["add", "ferry.toml"]);
        git(dir.path(), &["commit", "-m", "invalid source"]);
        let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
        assert_eq!(out.status.code(), Some(125), "source: {source}");
        assert_eq!(receipt(&out)["ship"], serde_json::json!([]));
    }
    git(dir.path(), &["tag", "-a", "not-a-commit", "-m", "tag"]);
    git(
        dir.path(),
        &[
            "update-ref",
            "refs/remotes/origin/master",
            "refs/tags/not-a-commit",
        ],
    );
    fs::write(dir.path().join("ferry.toml"), "version=1\n[[ship]]\nref='refs/remotes/origin/master'\nfrom={remote='origin',branch='master'}\n[recipe.pass]\ncommand=['true']\n").unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "tag refusal"]);
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(
        receipt(&out)["reason"]
            .as_str()
            .unwrap()
            .contains("commit object")
    );
}

#[test]
fn working_copy_edits_cannot_change_the_recipe() {
    let dir = fixture("true", "");
    fs::write(dir.path().join("ferry.toml"), "tampered").unwrap();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(receipt(&out)["outcome"], "ran-pass");
    assert!(String::from_utf8_lossy(&out.stderr).contains("committed"));
}

#[test]
fn preserved_ignored_directories_survive_but_tracked_files_reset() {
    let dir = fixture("true", "");
    fs::write(dir.path().join(".gitignore"), "cache/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['cache']\n[recipe.pass]\ncommand=['sh','-c','mkdir -p cache; if test -f cache/marker; then echo warm; else echo cold; touch cache/marker; fi']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "preserved cache"]);
    let first = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    assert_eq!(receipt(&first)["preserve"], serde_json::json!(["cache"]));
    let slot = only_checkout(dir.path());
    fs::write(slot.join("ferry.toml"), "tracked tampering").unwrap();
    let second = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stdout)
    );
    assert!(String::from_utf8_lossy(&second.stderr).contains("warm"));
    assert_ne!(
        fs::read_to_string(slot.join("ferry.toml")).unwrap(),
        "tracked tampering"
    );
}

#[test]
fn companion_refs_are_pinned_and_isolated_between_seats() {
    let dir = fixture("true", "");
    let resolve = |root: &Path, revision: &str| {
        let output = Command::new("git")
            .current_dir(root)
            .args(["rev-parse", revision])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let first = resolve(dir.path(), "HEAD");
    git(dir.path(), &["branch", "companion"]);
    fs::write(dir.path().join("ferry.toml"), "version=1\n[[ship]]\nref='refs/remotes/origin/master'\nfrom='companion'\n[recipe.pass]\ncommand=['git','rev-parse','refs/remotes/origin/master']\n").unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "companion declaration"]);
    let second = resolve(dir.path(), "HEAD");
    let out = run(
        dir.path(),
        &["run", "pass", "--local", "--seat", "first", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(receipt(&out)["ship"][0]["commit"], first);
    git(dir.path(), &["update-ref", "refs/heads/companion", &second]);
    let out = run(
        dir.path(),
        &["run", "pass", "--local", "--seat", "second", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(receipt(&out)["ship"][0]["commit"], second);
    for entry in fs::read_dir(dir.path().join(".ferry/local/v1/seats")).unwrap() {
        let seat = entry.unwrap().path();
        let slot = seat.join("checkout");
        let expected = if seat
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("-first")
        {
            &first
        } else {
            &second
        };
        assert_eq!(&resolve(&slot, "refs/remotes/origin/master"), expected);
        assert!(
            slot.join(".git").is_dir(),
            "refs cannot live in a shared worktree repository"
        );
    }
}

#[test]
fn recipe_exit_125_and_printed_refused_are_not_transport_refusals() {
    let dir = fixture("true", "");
    let out = run(dir.path(), &["run", "fail", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(125));
    assert_eq!(receipt(&out)["outcome"], "ran-fail");
}

#[test]
fn recipe_arguments_follow_the_command_and_are_recorded() {
    let dir = fixture("true", "");
    fs::write(dir.path().join("ferry.toml"), "version=1\n[recipe.pass]\ncommand=['sh','-c','printf \"<%s>\\\\n\" \"$@\"; [ \"$1\" = ok ]','recipe']\n").unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "argument fixture"]);
    let args = ["ok", "two words", "--on", "elsewhere", ""];
    let mut command_line = vec!["run", "pass", "--local", "--json", "--"];
    command_line.extend(args);
    let out = run(dir.path(), &command_line);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    // With --json, the recipe's output is on stderr, exactly as written.
    let printed: Vec<&str> = std::str::from_utf8(&out.stderr)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with('<'))
        .collect();
    assert_eq!(
        printed,
        ["<ok>", "<two words>", "<--on>", "<elsewhere>", "<>"]
    );
    let record = receipt(&out);
    assert_eq!(record["machine"], "local");
    // The binary reports the revision its build compiled in, or `unpinned`;
    // null is reserved for receipts that predate the field.
    assert_eq!(record["caller_source_rev"], SOURCE_REV);
    assert_eq!(record["caller_exe"], env!("CARGO_BIN_EXE_ferry"));
    assert_eq!(record["host_source_rev"], record["caller_source_rev"]);
    assert_eq!(record["host_exe"], record["caller_exe"]);
    assert_eq!(record["args"], serde_json::json!(args));
    assert_eq!(
        record["args_sha256"],
        sha256(&serde_json::to_vec(&args).unwrap())
    );

    let out = run(
        dir.path(),
        &["run", "pass", "--local", "--json", "--", "no"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(receipt(&out)["outcome"], "ran-fail");
    assert_eq!(receipt(&out)["args"], serde_json::json!(["no"]));

    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(receipt(&out)["args"], serde_json::json!([]));
    assert_eq!(receipt(&out)["args_sha256"], sha256(b"[]"));
}

#[test]
fn environment_hook_cannot_replace_ferry_owned_mirror_configuration() {
    let dir = fixture("true", "");
    fs::write(dir.path().join("ferry.toml"), "version=1\n[hooks]\nenv='environment'\n[recipe.pass]\ncommand=['sh','-c','echo attempted-recipe']\n").unwrap();
    fs::write(
        dir.path().join("environment"),
        "#!/bin/sh\nprintf '%s\\n' '{\"GIT_CONFIG_GLOBAL\":\"/tmp/not-the-seat-config\"}'\n",
    )
    .unwrap();
    fs::set_permissions(
        dir.path().join("environment"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml", "environment"]);
    git(
        dir.path(),
        &["commit", "-m", "attempt to replace owned config"],
    );
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(125));
    assert_eq!(receipt(&out)["outcome"], "refused");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("attempted-recipe"));
}

#[test]
fn caller_environment_is_declared_forwarded_and_recorded() {
    let dir = fixture("true", "");
    fs::write(dir.path().join("ferry.toml"), "version=1\n[environment]\nforwards=['APP_ALLOWED']\nwatches=['APP_']\nlocal=['APP_LOCAL']\n[recipe.pass]\ncommand=['sh','-c','echo forwarded-$APP_ALLOWED']\n").unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "declared caller environment"]);
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferry"));
        cmd.current_dir(dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env("APP_ALLOWED", "selected")
            .env("APP_LOCAL", "not-forwarded")
            .args(["run", "pass", "--local", "--json"]);
        cmd
    };
    let out = command().output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        receipt(&out)["forwarded_env"],
        serde_json::json!({"APP_ALLOWED":"selected"})
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("forwarded-selected"));
    let out = command()
        .env("APP_UNLISTED", "hidden-input")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(125));
    assert_eq!(receipt(&out)["outcome"], "refused");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("forwarded-selected"));
    fs::write(
        dir.path().join("environment"),
        "#!/bin/sh\nprintf '%s\\n' '{\"APP_ALLOWED\":\"host-replacement\"}'\n",
    )
    .unwrap();
    fs::set_permissions(
        dir.path().join("environment"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\n[environment]\nforwards=['APP_ALLOWED']\nwatches=['APP_']\nlocal=['APP_LOCAL']\n[hooks]\nenv='environment'\n[recipe.pass]\ncommand=['sh','-c','echo forwarded-$APP_ALLOWED']\n").unwrap();
    git(dir.path(), &["add", "ferry.toml", "environment"]);
    git(dir.path(), &["commit", "-m", "attempt host override"]);
    let out = command().output().unwrap();
    assert_eq!(out.status.code(), Some(125));
    assert_eq!(receipt(&out)["outcome"], "refused");
    assert_eq!(
        receipt(&out)["forwarded_env"],
        serde_json::json!({"APP_ALLOWED":"selected"})
    );
}

#[test]
fn every_caller_refusal_has_a_receipt() {
    let dir = fixture("true", "");
    for args in [
        vec!["run", "pass", "--commit", "no-such-revision", "--json"],
        vec!["run", "pass", "--seat", "rv;touch", "--json"],
        vec!["run", "pass", "--on", "missing", "--json"],
        vec!["run", "-bad", "--json"],
    ] {
        let out = run(dir.path(), &args);
        assert_eq!(out.status.code(), Some(125));
        let record = receipt(&out);
        assert_eq!(record["outcome"], "refused");
        assert!(
            dir.path()
                .join(".ferry/receipts")
                .join(format!("{}.json", record["id"].as_str().unwrap()))
                .exists()
        );
    }
}

/// The keys `[[ship]]` replaced refuse, naming it, from ferry.toml and from a
/// catalog adapter alike; entries from both are combined, and the same ref
/// or URL declared twice refuses.
#[test]
fn retired_ship_keys_refuse_and_entries_from_both_owners_combine() {
    let dir = fixture("true", "");
    for old in [
        "[companion_refs]\n'refs/heads/x'='HEAD'\n",
        "[[mirror]]\nurl='u'\npath='p'\ncommit='HEAD'\n",
    ] {
        fs::write(
            dir.path().join("ferry.toml"),
            format!("version=1\n{old}[recipe.pass]\ncommand=['true']\n"),
        )
        .unwrap();
        git(dir.path(), &["add", "ferry.toml"]);
        git(dir.path(), &["commit", "-m", "an old key"]);
        let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
        assert_eq!(out.status.code(), Some(125));
        let reason = receipt(&out)["reason"].as_str().unwrap().to_owned();
        assert!(reason.contains("are retired: use [[ship]]"), "{reason}");
    }
    fs::write(dir.path().join("adapter"), "#!/bin/sh\ncat contract.json\n").unwrap();
    fs::set_permissions(
        dir.path().join("adapter"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let recipe = serde_json::json!({"pass": {"command": ["git", "rev-parse", "refs/heads/a", "refs/heads/b"]}});
    for (emitted, combined) in [
        (
            serde_json::json!({"ship": [{"ref": "refs/heads/b", "from": "HEAD"}]}),
            true,
        ),
        (
            serde_json::json!({"ship": [{"ref": "refs/heads/a", "from": "HEAD"}]}),
            false,
        ),
        (serde_json::json!({"mirror": []}), false),
    ] {
        let mut contract = emitted.clone();
        contract["version"] = 1.into();
        contract["recipe"] = recipe.clone();
        fs::write(dir.path().join("contract.json"), contract.to_string()).unwrap();
        fs::write(
            dir.path().join("ferry.toml"),
            "version=1\n[catalog]\nadapter='adapter'\n[[ship]]\nref='refs/heads/a'\nfrom='HEAD'\n",
        )
        .unwrap();
        git(
            dir.path(),
            &["add", "ferry.toml", "adapter", "contract.json"],
        );
        git(dir.path(), &["commit", "-m", "ship from both owners"]);
        let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
        let record = receipt(&out);
        if combined {
            assert_eq!(out.status.code(), Some(0), "{record}");
            assert_eq!(record["ship"].as_array().unwrap().len(), 2, "{record}");
        } else {
            assert_eq!(out.status.code(), Some(125), "{emitted}: {record}");
        }
    }
}

#[test]
fn hidden_index_flags_refuse_before_resetting_the_seat() {
    let dir = fixture("true", "");
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(out.status.success());
    let seat = only_checkout(dir.path());
    git(&seat, &["update-index", "--assume-unchanged", "ferry.toml"]);
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(receipt(&out)["outcome"], "refused");
    assert!(
        receipt(&out)["reason"]
            .as_str()
            .unwrap()
            .contains("hidden index")
    );
}

#[test]
fn adapter_runs_from_the_pinned_commit_and_records_both_digests() {
    let dir = fixture("true", "");
    with_adapter(
        dir.path(),
        "#!/bin/sh\nprintf '%s\\n' '{\"version\":1,\"recipe\":{\"pass\":{\"command\":[\"sh\",\"-c\",\"echo adapter-commit\"],\"requires\":[],\"routable\":false}}}'\n",
    );
    fs::write(dir.path().join("catalog"), "not the committed adapter").unwrap();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let record = receipt(&out);
    assert_eq!(record["adapter_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(record["contract_sha256"].as_str().unwrap().len(), 64);
    assert!(String::from_utf8_lossy(&out.stderr).contains("adapter-commit"));
}

#[test]
fn malformed_or_partial_adapter_contract_never_runs_a_valid_subset() {
    let dir = fixture("true", "");
    with_adapter(
        dir.path(),
        "#!/bin/sh\nprintf '%s\\n' '{\"version\":1,\"recipe\":{\"pass\":{\"command\":[\"true\"]},\"bad\":{\"requires\":[]}}}'\n",
    );
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(receipt(&out)["outcome"], "refused");
    assert!(receipt(&out)["adapter_sha256"].is_string());
    assert!(receipt(&out)["contract_sha256"].is_string());
    assert!(!dir.path().join(".ferry/local/v1").exists());
}

#[test]
fn nondeterministic_adapter_is_a_refusal() {
    let dir = fixture("true", "");
    with_adapter(
        dir.path(),
        "#!/bin/sh\nprintf '{\"version\":1,\"recipe\":{},\"pid\":%s}\\n' \"$$\"\n",
    );
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(
        receipt(&out)["reason"]
            .as_str()
            .unwrap()
            .contains("nondeterministic")
    );
}

#[test]
fn a_project_can_require_provider_capabilities() {
    let dir = fixture("true", "");
    fs::write(
        dir.path().join("ferry.toml"),
        "version=1\n[provider]\nrequires=['admission']\n[recipe.pass]\ncommand=['sh','-c','echo committed']\nroutable=true\n",
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "requires admission"]);
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        receipt(&out)["provider_requires"],
        serde_json::json!(["admission"])
    );
    assert_eq!(receipt(&out)["admission"]["result"], "admitted");

    fs::write(
        dir.path().join("ferry.toml"),
        "version=1\n[provider]\nrequires=['teleport']\n[recipe.pass]\ncommand=['sh','-c','echo committed']\n",
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "requires the impossible"]);
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(
        receipt(&out)["reason"]
            .as_str()
            .unwrap()
            .contains("teleport")
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("committed"));
}

/// SPEC §12 item 24: each of the three files at version 2 is read with the
/// §10 keys, sizes and durations; a variable under a
/// `refuse_other_vars_starting_with` prefix that is listed in `send` or
/// `keep_local` does not refuse.
#[test]
fn version_two_files_are_read_by_their_plain_names() {
    let dir = fixture("true", "");
    with_project(
        dir.path(),
        "version = 2\nkeep_between_runs = ['_build']\n[environment]\nsend = ['APP_ALLOWED']\nrefuse_other_vars_starting_with = ['APP_']\nkeep_local = ['APP_LOCAL']\n[provider]\nneeds = ['admission']\n[recipe.pass]\ncommand = ['sh', '-c', 'echo forwarded-$APP_ALLOWED']\nneeds = []\nmay_run_elsewhere = true\n",
    );
    with_host_policy(
        dir.path(),
        "version = 2\nrefuse_runs_below_free_disk = '1 B'\nmax_cache_per_checkout = '1 GiB'\nmax_run_time = '1h'\n",
    );
    let out = command(dir.path())
        .args(["run", "pass", "--local", "--json"])
        .env("APP_ALLOWED", "selected")
        .env("APP_LOCAL", "kept")
        .output()
        .unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert_eq!(
        record["forwarded_env"],
        serde_json::json!({"APP_ALLOWED": "selected"})
    );
    assert_eq!(record["preserve"], serde_json::json!(["_build"]));
    assert_eq!(
        record["provider_requires"],
        serde_json::json!(["admission"])
    );
    assert_eq!(record["admission"]["floor_bytes"], 1, "{record}");
    assert_eq!(
        record["admission"]["seat_cap_bytes"],
        1u64 << 30,
        "{record}"
    );
    assert_eq!(record["admission"]["policy"]["version"], 2, "{record}");
    // max_run_time bounds what a run may ask for.
    let out = run(
        dir.path(),
        &[
            "run",
            "pass",
            "--local",
            "--json",
            "--timeout-seconds",
            "7200",
        ],
    );
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(125), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("3600 s"),
        "{record}"
    );
    // machines.toml at version 2: [machine.<name>], platform, has.
    let remote = remote_command(
        dir.path(),
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    );
    with_machines(
        dir.path(),
        &format!(
            "version = 2\n[machine.test-host]\nssh = 'test-host'\nbinary = '.local/bin/ferry'\nplatform = '{}'\nhas = ['quiet']\n",
            test_platform()
        ),
    );
    let out = { remote }.output().unwrap();
    let record = receipt(&out);
    assert_eq!(out.status.code(), Some(0), "{record}");
    assert_eq!(record["machine"], "test-host", "{record}");
    assert!(
        record["machine_features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "quiet"),
        "{record}"
    );
}

/// SPEC §12 item 24: a version 2 file holding a version 1 key is refused,
/// naming the new key; so is a size without a unit, and a version 2 file's
/// unknown key is named in version 2's words.
#[test]
fn a_version_two_file_with_a_version_one_key_names_the_new_key() {
    let dir = fixture("true", "");
    for (project, said) in [
        (
            "version = 2\n[recipe.pass]\ncommand = ['true']\nrequires = []\n",
            "recipe.pass.`requires` is version 1: use `needs`",
        ),
        (
            "version = 2\npreserve = ['_build']\n[recipe.pass]\ncommand = ['true']\n",
            "`preserve` is version 1: use `keep_between_runs`",
        ),
        (
            "version = 2\n[environment]\nforwards = ['A']\n[recipe.pass]\ncommand = ['true']\n",
            "environment.`forwards` is version 1: use `send`",
        ),
        (
            "version = 2\n[recipe.pass]\ncommand = ['true']\nneed = []\n",
            "`needs`",
        ),
        // A version 2 key in a version 1 file, and a version that is no
        // whole number.
        (
            "version = 1\n[recipe.pass]\ncommand = ['true']\nneeds = []\n",
            "unknown field `needs`",
        ),
        (
            "version = '2'\n[recipe.pass]\ncommand = ['true']\n",
            "ferry.toml at ",
        ),
    ] {
        with_project(dir.path(), project);
        let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(125), "{project}: {stdout}{stderr}");
        assert!(
            stdout.contains(said) || stderr.contains(said),
            "{project}: {stdout}{stderr}"
        );
    }
    with_project(
        dir.path(),
        "version = 1\n[recipe.pass]\ncommand = ['true']\nroutable = true\n",
    );
    for (machines, said) in [
        (
            "version = 2\n[machine.m]\nssh = 'm'\ntriple = 'x'\n",
            "machine.m.`triple` is version 1: use `platform`",
        ),
        (
            "version = 2\n[machine.m]\nssh = 'm'\nplatform = 'x'\ncores = 4\n",
            "`cores` is gone in version 2",
        ),
        (
            "version = 2\n[host.m]\nssh = 'm'\nplatform = 'x'\n",
            "`host` is version 1: use `machine`",
        ),
        (
            "[host.m]\nssh = 'm'\ntriple = 'x'\nhas = ['quiet']\n",
            "unknown field `has`",
        ),
    ] {
        with_machines(dir.path(), machines);
        let out = run(dir.path(), &["run", "pass", "--json"]);
        let both = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.status.code(), Some(125), "{machines}: {both}");
        assert!(both.contains(said), "{machines}: {both}");
    }
}

/// SPEC §12 item 24: a version 2 ferry.toml with a version 1 adapter, and
/// the reverse, run with the same merged contract as both at version 1. The
/// adapter carries every renamed part (recipe needs, here a capability
/// every place has, since --local checks needs too; the environment,
/// `keep_between_runs`, `[provider] needs`, `also_send`) and a JSON `null`,
/// which version 1 always read as absent.
#[test]
fn a_mixed_version_adapter_merges_the_same_contract() {
    let v1_json = r#"{"version":1,"recipe":{"pass":{"command":["sh","-c","echo forwarded-$APP_ALLOWED"],"requires":["admission"],"routable":false}},"environment":{"forwards":["APP_ALLOWED"],"watches":["APP_"],"local":[]},"preserve":["_build"],"provider":{"requires":["admission"]},"ship":[{"ref":"refs/heads/extra","from":"HEAD"}],"companion_refs":null}"#;
    let v2_json = r#"{"version":2,"recipe":{"pass":{"command":["sh","-c","echo forwarded-$APP_ALLOWED"],"needs":["admission"],"may_run_elsewhere":false}},"environment":{"send":["APP_ALLOWED"],"refuse_other_vars_starting_with":["APP_"],"keep_local":[]},"keep_between_runs":["_build"],"provider":{"needs":["admission"]},"also_send":[{"ref":"refs/heads/extra","from":"HEAD"}],"mirror":null}"#;
    let mut contracts = vec![];
    for (version, json) in [(1, v1_json), (2, v1_json), (1, v2_json), (2, v2_json)] {
        let dir = fixture("true", "");
        let project = format!("version = {version}\n[catalog]\nadapter = 'catalog'\n");
        fs::write(dir.path().join("ferry.toml"), &project).unwrap();
        // Kept build directories are ignored ones.
        fs::write(dir.path().join(".gitignore"), "_build/\n").unwrap();
        fs::write(
            dir.path().join("catalog"),
            format!("#!/bin/sh\nprintf '%s' '{json}'\n"),
        )
        .unwrap();
        fs::set_permissions(
            dir.path().join("catalog"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        git(dir.path(), &["add", "ferry.toml", "catalog", ".gitignore"]);
        git(dir.path(), &["commit", "-m", "adapter"]);
        let out = command(dir.path())
            .args(["run", "pass", "--local", "--json"])
            .env("APP_ALLOWED", "selected")
            .output()
            .unwrap();
        let record = receipt(&out);
        assert_eq!(out.status.code(), Some(0), "{project}{json}: {record}");
        let shipped: Vec<_> = record["ship"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["ref"].clone())
            .collect();
        contracts.push((
            record["forwarded_env"].clone(),
            record["preserve"].clone(),
            record["requirements"].clone(),
            record["provider_requires"].clone(),
            shipped,
        ));
    }
    assert_eq!(
        contracts[0],
        (
            serde_json::json!({"APP_ALLOWED": "selected"}),
            serde_json::json!(["_build"]),
            serde_json::json!(["admission"]),
            serde_json::json!(["admission"]),
            vec![serde_json::json!("refs/heads/extra")],
        )
    );
    assert!(
        contracts.windows(2).all(|pair| pair[0] == pair[1]),
        "{contracts:?}"
    );
}

/// SPEC §12 item 26: `reserved_for` keeps a machine out of the automatic
/// choice unless the recipe needs one of its listed features; --on still
/// chooses it, never past `may_run_elsewhere` or `needs`; with no other machine
/// a run is refused naming the reservation, never sent to it.
#[test]
fn a_reserved_machine_is_chosen_only_for_what_it_is_reserved_for() {
    let dir = fixture("true", "");
    with_project(
        dir.path(),
        "version = 2\n[recipe.pass]\ncommand = ['true']\nmay_run_elsewhere = true\n[recipe.timing]\ncommand = ['true']\nneeds = ['quiet']\nmay_run_elsewhere = true\n[recipe.gpu]\ncommand = ['true']\nneeds = ['gpu']\nmay_run_elsewhere = true\n[recipe.here]\ncommand = ['true']\n",
    );
    let platform = test_platform();
    let both = format!(
        "version = 2\n[machine.bench]\nssh = 'test-host'\nbinary = '.local/bin/ferry'\nplatform = '{platform}'\nhas = ['quiet']\nreserved_for = ['quiet']\n[machine.gates]\nssh = 'test-host'\nbinary = '.local/bin/ferry'\nplatform = '{platform}'\n"
    );
    let ran = |args: &[&str], machines: &str| {
        let mut all = vec!["run"];
        all.extend_from_slice(args);
        all.push("--json");
        let command = remote_command(dir.path(), ".local/bin/ferry", &all);
        with_machines(dir.path(), machines);
        let out = { command }.output().unwrap();
        (out.status.code(), receipt(&out))
    };
    let (code, record) = ran(&["pass"], &both);
    assert_eq!(
        (code, record["machine"].as_str()),
        (Some(0), Some("gates")),
        "{record}"
    );
    let (code, record) = ran(&["timing"], &both);
    assert_eq!(
        (code, record["machine"].as_str()),
        (Some(0), Some("bench")),
        "{record}"
    );
    let (code, record) = ran(&["pass", "--on", "bench"], &both);
    assert_eq!(
        (code, record["machine"].as_str()),
        (Some(0), Some("bench")),
        "{record}"
    );
    let (code, record) = ran(&["here", "--on", "bench"], &both);
    assert_eq!(code, Some(125), "{record}");
    let (code, record) = ran(&["gpu", "--on", "bench"], &both);
    assert_eq!(code, Some(125), "{record}");
    let (code, record) = ran(&["here"], &both);
    assert_eq!(
        (code, record["machine"].as_str()),
        (Some(0), Some("local")),
        "{record}"
    );
    // Only bench: never a fallback; the refusal names the reservation.
    let bench_only = both[..both.find("[machine.gates]").unwrap()].to_owned();
    let (code, record) = ran(&["pass"], &bench_only);
    assert_eq!(code, Some(125), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("bench is reserved_for quiet; use --on bench"),
        "{record}"
    );
    // One listed feature is enough: reserved for quiet and special.
    let two = bench_only
        .replace("has = ['quiet']", "has = ['quiet', 'special']")
        .replace(
            "reserved_for = ['quiet']",
            "reserved_for = ['quiet', 'special']",
        );
    let (code, record) = ran(&["timing"], &two);
    assert_eq!(
        (code, record["machine"].as_str()),
        (Some(0), Some("bench")),
        "{record}"
    );
    // A reserved_for name the machine does not have refuses machines.toml.
    let (code, record) = ran(
        &["timing"],
        &bench_only.replace("reserved_for = ['quiet']", "reserved_for = ['gpu']"),
    );
    assert_eq!(code, Some(125), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("reserved_for gpu, which it does not have"),
        "{record}"
    );
}

/// murail-4a's and herald-1b's returns on 9092e976: an adapter's version 2
/// JSON is read natively. A null where a field is required, a version 1 key
/// (even as null), an unknown key and a duplicate field all refuse, as at
/// version 1; null stays absent only where a field is optional.
#[test]
fn a_version_two_adapter_refuses_what_version_one_refused() {
    for (json, said) in [
        (
            r#"{"version":2,"recipe":{"pass":{"command":["true"],"needs":null}}}"#,
            "invalid type: null",
        ),
        (
            r#"{"version":2,"recipe":{"pass":{"command":["true"]}},"provider":{"needs":null}}"#,
            "invalid type: null",
        ),
        (
            r#"{"version":2,"recipe":{"pass":{"command":["true"]}},"unknown":null}"#,
            "unknown field `unknown`",
        ),
        (
            r#"{"version":2,"recipe":{"pass":{"command":["true"],"requires":null}}}"#,
            "recipe.pass.`requires` is version 1: use `needs`",
        ),
        (
            r#"{"version":2,"recipe":{"pass":{"command":["false"],"command":["true"]}}}"#,
            "duplicate field `command`",
        ),
    ] {
        let dir = fixture("true", "");
        with_adapter(dir.path(), &format!("#!/bin/sh\nprintf '%s' '{json}'\n"));
        let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
        let both = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.status.code(), Some(125), "{json}: {both}");
        assert!(both.contains(said), "{json}: {both}");
    }
    // An optional field as null is absent, as at version 1.
    let dir = fixture("true", "");
    with_adapter(
        dir.path(),
        "#!/bin/sh\nprintf '%s' '{\"version\":2,\"recipe\":{\"pass\":{\"command\":[\"true\"]}},\"environment\":null,\"mirror\":null}'\n",
    );
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", receipt(&out));
}

#[test]
fn a_run_inside_a_run_on_its_own_place_is_refused_at_once() {
    for exclusive in [false, true] {
        let dir = nesting(None);
        let mut command = ferry(dir.path());
        command.args(["run", "nest", "--local"]);
        if exclusive {
            command.arg("--exclusive");
        }
        let started = Instant::now();
        let out = bounded_output(&mut command);
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.status.code(), Some(0), "{said}");
        assert!(said.contains("inner=125"), "exclusive={exclusive}: {said}");
        assert!(
            said.contains("nested run on the same place"),
            "exclusive={exclusive}: {said}"
        );
        // At once: never the wait (900 s) for the locks its own run holds.
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "exclusive={exclusive}: took {:?}",
            started.elapsed()
        );
    }
}

#[test]
fn a_run_inside_a_run_on_another_place_goes_ahead() {
    let other = fixture("true", "");
    let dir = nesting(Some(other.path()));
    let out = bounded_output(ferry(dir.path()).args(["run", "nest", "--local", "--exclusive"]));
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{said}");
    assert!(
        said.contains("committed") && said.contains("inner=0"),
        "{said}"
    );
    assert!(!said.contains("nested run"), "{said}");
}

#[test]
fn needs_are_checked_on_the_local_route() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init"]);
    git(d, &["config", "user.name", "Ferry Test"]);
    git(d, &["config", "user.email", "ferry@example.invalid"]);
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    fs::write(
        d.join("ferry.toml"),
        format!(
            "version = 2\n[recipe.gpu]\ncommand = ['true']\nneeds = ['gpu']\n[recipe.here]\ncommand = ['echo', 'here']\nneeds = ['{os}', '{arch}', '{os}-{arch}', 'status', 'exclusive']\n"
        ),
    )
    .unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "fixture"]);
    // No machines, then --local: the same refusal, naming what is missing.
    for args in [
        &["run", "gpu", "--json"][..],
        &["run", "gpu", "--local", "--json"],
    ] {
        let out = bounded_output(ferry(d).args(args));
        let record = receipt(&out);
        assert_eq!(out.status.code(), Some(125), "{args:?}: {record}");
        let reason = record["reason"].as_str().unwrap();
        // What it detects, in order (a sorted set).
        let facts = std::collections::BTreeSet::from([
            os.to_owned(),
            arch.to_owned(),
            format!("{os}-{arch}"),
        ]);
        let facts = facts.into_iter().collect::<Vec<_>>().join(", ");
        assert!(
            reason.starts_with(&format!(
                "local does not have gpu (it has {facts}, and its Ferry's capabilities)"
            )),
            "{args:?}: {record}"
        );
    }
    // What it detects and what this Ferry can do; exclusive aside.
    let out = bounded_output(ferry(d).args(["run", "here", "--local"]));
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "here\n");
}
