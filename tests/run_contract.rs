//! Real CLI contracts exercise committed bytes and durable refusal receipts.
mod common;
use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::{Pid, getsid},
};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

struct OwnedGroup(i32);
impl Drop for OwnedGroup {
    fn drop(&mut self) {
        if getsid(Some(Pid::from_raw(self.0))).is_ok_and(|sid| sid.as_raw() == self.0) {
            let _ = killpg(Pid::from_raw(self.0), Signal::SIGKILL);
        }
    }
}
struct OwnedCaller {
    child: Option<std::process::Child>,
    owners: std::path::PathBuf,
}
impl Drop for OwnedCaller {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Startup assertions can fail before the session guard is constructed.
        if let Ok(entries) = fs::read_dir(&self.owners) {
            for entry in entries.flatten() {
                let session = fs::read(entry.path())
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .and_then(|owner| owner["session"].as_i64())
                    .and_then(|pid| i32::try_from(pid).ok());
                if let Some(session) = session {
                    drop(OwnedGroup(session));
                }
            }
        }
    }
}

fn git(root: &Path, args: &[&str]) {
    // Each test owns its repository; never inherit another one's.
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
fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init"]);
    git(dir.path(), &["config", "user.name", "Ferry Test"]);
    git(
        dir.path(),
        &["config", "user.email", "ferry@example.invalid"],
    );
    fs::write(dir.path().join("ferry.toml"), "version=1\n[recipe.pass]\ncommand=['sh','-c','echo committed']\nroutable=true\n[recipe.fail]\ncommand=['sh','-c','echo refused; exit 125']\nroutable=true\n").unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "fixture"]);
    dir
}
fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .args(args)
        .output()
        .unwrap()
}
const SOURCE_REV: &str = match option_env!("FERRY_SOURCE_REV") {
    Some(rev) => rev,
    None => "unpinned",
};
fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            write!(hex, "{byte:02x}").unwrap();
            hex
        })
}
fn receipt(out: &Output) -> serde_json::Value {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v["event"] == "receipt")
        .unwrap()["receipt"]
        .clone()
}

#[test]
fn published_companion_uses_tracking_ref_on_git_jj_and_worktree_callers() {
    for backend in ["git", "jj", "worktree"] {
        let dir = fixture();
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
        fs::write(dir.path().join("ferry.toml"), "version=1\n[companion_refs]\n'refs/remotes/origin/master'={remote='origin',branch='master'}\n[recipe.pass]\ncommand=['git','rev-parse','refs/remotes/origin/master']\n").unwrap();
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
        assert_eq!(
            record["companion_refs"]["refs/remotes/origin/master"],
            first
        );
        assert_eq!(
            record["companion_sources"]["refs/remotes/origin/master"],
            serde_json::json!({"remote":"origin", "branch":"master"})
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains(&first));
        git(
            dir.path(),
            &["update-ref", "-d", "refs/remotes/origin/master"],
        );
        let out = run(&caller, &["run", "pass", "--local", "--json"]);
        assert_eq!(out.status.code(), Some(125));
        let refused = receipt(&out);
        assert_eq!(
            refused["companion_sources"]["refs/remotes/origin/master"],
            serde_json::json!({"remote":"origin", "branch":"master"})
        );
        assert!(refused["companion_refs"].as_object().unwrap().is_empty());
        assert_eq!(
            record["companion_refs"]["refs/remotes/origin/master"],
            first
        );
        for entry in fs::read_dir(caller.join(".ferry/local/slots")).unwrap() {
            let output = Command::new("git")
                .current_dir(entry.unwrap().path())
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
    let dir = fixture();
    for source in [
        "{remote='origin'}",
        "{branch='master'}",
        "{remote='origin',branch='master',revision='HEAD'}",
        "{remote='-origin',branch='master'}",
        "{remote='origin',branch='../master'}",
    ] {
        fs::write(dir.path().join("ferry.toml"), format!("version=1\n[companion_refs]\n'refs/remotes/origin/master'={source}\n[recipe.pass]\ncommand=['true']\n")).unwrap();
        git(dir.path(), &["add", "ferry.toml"]);
        git(dir.path(), &["commit", "-m", "invalid source"]);
        let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
        assert_eq!(out.status.code(), Some(125), "source: {source}");
        assert!(
            receipt(&out)["companion_refs"]
                .as_object()
                .unwrap()
                .is_empty()
        );
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
    fs::write(dir.path().join("ferry.toml"), "version=1\n[companion_refs]\n'refs/remotes/origin/master'={remote='origin',branch='master'}\n[recipe.pass]\ncommand=['true']\n").unwrap();
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
    let dir = fixture();
    fs::write(dir.path().join("ferry.toml"), "tampered").unwrap();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(receipt(&out)["outcome"], "ran-pass");
    assert!(String::from_utf8_lossy(&out.stdout).contains("committed"));
}

#[test]
fn preserved_ignored_directories_survive_but_tracked_files_reset() {
    let dir = fixture();
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
    let state = dir.path().join(".ferry/local/slots");
    let slot = fs::read_dir(state).unwrap().next().unwrap().unwrap().path();
    fs::write(slot.join("ferry.toml"), "tracked tampering").unwrap();
    let second = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stdout)
    );
    assert!(String::from_utf8_lossy(&second.stdout).contains("warm"));
    assert_ne!(
        fs::read_to_string(slot.join("ferry.toml")).unwrap(),
        "tracked tampering"
    );
}

#[test]
fn legacy_worktree_seats_migrate_under_the_existing_lease() {
    let dir = fixture();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(out.status.success());
    let state = dir.path().join(".ferry/local");
    let slot = fs::read_dir(state.join("slots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    // Recreate the old Ferry-owned layout, not an unrelated user checkout.
    fs::remove_dir_all(&slot).unwrap();
    let bare = state.join("repo.git");
    git(
        dir.path(),
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "worktree",
            "add",
            "--detach",
            slot.to_str().unwrap(),
            "FETCH_HEAD",
        ],
    );
    assert!(slot.join(".git").is_file());
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(receipt(&out)["outcome"], "ran-pass");
    assert!(slot.join(".git").is_dir());
}

#[test]
fn companion_refs_are_pinned_and_isolated_between_seats() {
    let dir = fixture();
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
    fs::write(dir.path().join("ferry.toml"), "version=1\n[companion_refs]\n'refs/remotes/origin/master'='companion'\n[recipe.pass]\ncommand=['git','rev-parse','refs/remotes/origin/master']\n").unwrap();
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
    assert_eq!(
        receipt(&out)["companion_refs"]["refs/remotes/origin/master"],
        first
    );
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
    assert_eq!(
        receipt(&out)["companion_refs"]["refs/remotes/origin/master"],
        second
    );
    for entry in fs::read_dir(dir.path().join(".ferry/local/slots")).unwrap() {
        let slot = entry.unwrap().path();
        let expected = if slot
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
    let dir = fixture();
    let out = run(dir.path(), &["run", "fail", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(125));
    assert_eq!(receipt(&out)["outcome"], "ran-fail");
}

#[test]
fn recipe_arguments_follow_the_command_and_are_recorded() {
    let dir = fixture();
    fs::write(dir.path().join("ferry.toml"), "version=1\n[recipe.pass]\ncommand=['sh','-c','printf \"<%s>\\\\n\" \"$@\"; [ \"$1\" = ok ]','recipe']\n").unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "argument fixture"]);
    let args = ["ok", "two words", "--on", "elsewhere", ""];
    let mut command_line = vec!["run", "pass", "--local", "--json", "--"];
    command_line.extend(args);
    let out = run(dir.path(), &command_line);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    let printed: Vec<String> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["event"] == "output")
        .map(|event| event["text"].as_str().unwrap().to_owned())
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
fn collection_failure_records_its_error_without_changing_recipe_verdict() {
    let dir = fixture();
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
        let dir = fixture();
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
        let slot = fs::read_dir(dir.path().join(".ferry/local/slots"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            fs::read_to_string(slot.join("collected")).unwrap(),
            "collected\n"
        );
    }
}

#[test]
fn environment_hook_cannot_replace_ferry_owned_mirror_configuration() {
    let dir = fixture();
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
    let dir = fixture();
    fs::write(dir.path().join("ferry.toml"), "version=1\n[environment]\nforwards=['APP_ALLOWED']\nwatches=['APP_']\nlocal=['APP_LOCAL']\n[recipe.pass]\ncommand=['sh','-c','echo forwarded-$APP_ALLOWED']\n").unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "declared caller environment"]);
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferry"));
        cmd.current_dir(dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
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
    assert!(String::from_utf8_lossy(&out.stdout).contains("forwarded-selected"));
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
fn unconfirmed_cleanup_quarantines_the_host_before_dispatch() {
    let dir = fixture();
    let locks = dir.path().join(".ferry/local/locks");
    fs::create_dir_all(&locks).unwrap();
    fs::write(locks.join("cleanup-pending-earlier-run"), "unconfirmed").unwrap();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(receipt(&out)["outcome"], "refused");
    assert!(
        receipt(&out)["reason"]
            .as_str()
            .unwrap()
            .contains("cleanup_pending")
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("committed"));
    assert!(locks.join("cleanup-pending-earlier-run").exists());
}

#[test]
#[allow(clippy::too_many_lines)]
fn dead_supervisor_refuses_seat_reuse_until_the_recorded_session_is_empty() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let dir = fixture();
    let marker = dir.path().join("recipe-started");
    let release = dir.path().join("recipe-release");
    fs::write(
        dir.path().join("ferry.toml"),
        format!(
            "version=1\n[recipe.pass]\ncommand=['sh','-c','echo started > {}; while test ! -e {}; do sleep 0.1; done']\n",
            marker.display(), release.display()
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
        .args(["run", "pass", "--local", "--json"])
        .stdout(fs::File::create(&caller_output).unwrap())
        .stderr(fs::File::create(&diagnostic).unwrap())
        .spawn()
        .unwrap();
    let mut caller = OwnedCaller {
        child: Some(child),
        owners: dir.path().join(".ferry/local/owners"),
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
    let ownership = fs::read_dir(dir.path().join(".ferry/local/owners"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let owner: serde_json::Value = serde_json::from_slice(&fs::read(&ownership).unwrap()).unwrap();
    let group = OwnedGroup(i32::try_from(owner["session"].as_u64().unwrap()).unwrap());
    let seat = ownership.file_stem().unwrap().to_str().unwrap();
    // Kill the supervisor itself: the host-run the caller spawned, named by
    // its `accepted` event. The caller then ends without a result.
    let supervisor = fs::read_to_string(&caller_output)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["event"] == "accepted")
        .and_then(|event| event["pid"].as_i64())
        .expect("the accepted event names the supervisor");
    kill(
        Pid::from_raw(i32::try_from(supervisor).unwrap()),
        Signal::SIGKILL,
    )
    .unwrap();
    caller.child.as_mut().unwrap().wait().unwrap();
    let locks = dir.path().join(".ferry/local/locks");
    let held = |name: &str| {
        let probe = fs::File::open(locks.join(name)).unwrap();
        match fs2::FileExt::try_lock_exclusive(&probe) {
            Ok(()) => false,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(e) => panic!("probing {name}: {e}"),
        }
    };
    if !cfg!(target_os = "linux") {
        // No census to stop survivors here, so sessions do not inherit the
        // locks: a killed supervisor frees them while the recipe lives (the
        // documented residue), and the owner marker refuses the seat.
        assert!(!held(seat), "the seat lock outlived its supervisor here");
        let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
        assert_eq!(out.status.code(), Some(125));
        assert!(
            receipt(&out)["reason"]
                .as_str()
                .unwrap()
                .contains("seat recover")
        );
        let out = run(dir.path(), &["seat", "recover", seat, "--local"]);
        assert_eq!(out.status.code(), Some(125));
        fs::write(&release, "finish").unwrap();
        let deadline = common::startup_deadline();
        while !run(dir.path(), &["seat", "recover", seat, "--local"])
            .status
            .success()
        {
            assert!(Instant::now() < deadline, "session did not exit");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!ownership.exists());
        drop(group);
        return;
    }
    // The session inherited the seat lock and host lease: they stay held
    // while the recipe lives, though its supervisor is gone.
    assert!(held(seat), "the seat lock freed while the recipe lives");
    assert!(
        held("host-exclusive"),
        "the host lease freed while the recipe lives"
    );
    // A new run on the seat waits for it rather than starting beside it.
    let mut next = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .args(["run", "pass", "--local", "--json"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(1000));
    assert!(
        next.try_wait().unwrap().is_none(),
        "a run started beside a live recipe"
    );
    let out = run(dir.path(), &["seat", "recover", seat, "--local"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(receipt(&out)["reason"].as_str().unwrap().contains("in use"));
    assert!(ownership.exists());
    fs::write(&release, "finish").unwrap();
    // Once the session is gone, the waiting run takes the seat and refuses:
    // the dead supervisor's owner marker remains until recovery.
    let out = next.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(125));
    assert!(
        receipt(&out)["reason"]
            .as_str()
            .unwrap()
            .contains("seat recover")
    );
    let deadline = common::startup_deadline();
    loop {
        let out = run(dir.path(), &["seat", "recover", seat, "--local"]);
        if out.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session did not exit: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!ownership.exists());
    // A killed supervisor never released: recovery clears the owner marker
    // but leaves no idle record, so the seat is not eligible for eviction.
    assert!(seat_record(dir.path(), "idle").is_none());
    drop(group);
}
#[test]
fn every_caller_refusal_has_a_receipt() {
    let dir = fixture();
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

#[test]
fn internal_push_skips_hooks_and_remote_execution_receives_declared_environment() {
    let dir = fixture();
    let host = dir.path().join("host");
    let tools = dir.path().join("transport-tools");
    fs::create_dir_all(host.join(".local/bin")).unwrap();
    fs::create_dir_all(&tools).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_ferry"), host.join(".local/bin/ferry")).unwrap();
    fs::write(tools.join("ssh"), "#!/bin/sh\ncase \"$*\" in *BatchMode=yes*ControlMaster=auto*ControlPersist=10*ControlPath=*/.cache/ferry/ssh/*|*BatchMode=yes*ControlMaster=no*ControlPath=none*) ;; *) exit 125;; esac\nfor last do :; done\ncd \"$FERRY_TEST_HOST\" || exit 125\nunset APP_ALLOWED\nexport HOME=\"$FERRY_TEST_HOST\"\nexec sh -c \"$last\"\n").unwrap();
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
    assert!(String::from_utf8_lossy(&out.stdout).contains("remote-selected [a b --seat]"));
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

#[test]
fn mirrors_are_local_to_the_seat_and_record_the_exact_commit() {
    let dir = fixture();
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
    let sha = String::from_utf8(out.stdout).unwrap();
    fs::write(dir.path().join("ferry.toml"), format!("version=1\n[[mirror]]\nurl='https://ferry.invalid/private.git'\npath='dependency'\ncommit='{}'\n[recipe.mirror]\ncommand=['git','ls-remote','https://ferry.invalid/private.git']\n", sha.trim())).unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "mirror"]);
    let out = run(dir.path(), &["run", "mirror", "--local", "--json"]);
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(receipt(&out)["mirrors"][0]["commit"], sha.trim());
    assert!(String::from_utf8_lossy(&out.stdout).contains(sha.trim()));
    let key = receipt(&out)["mirrors"][0]["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let store = dir
        .path()
        .join(".ferry/local/mirrors")
        .join(format!("{key}.git"));
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
    git(&clone, &["cat-file", "-e", sha.trim()]);
    let head = fs::read_to_string(store.join("HEAD")).unwrap();
    assert!(!head.contains("refs/heads/ferry/"));
    let pinned = store.join("refs/heads/ferry").join(sha.trim());
    assert_eq!(fs::read_to_string(&pinned).unwrap().trim(), sha.trim());
    // A conflicting pin refuses rather than silently moving a content address.
    fs::write(dependency.join("value"), "changed").unwrap();
    git(&dependency, &["commit", "-am", "changed dependency"]);
    git(
        dir.path(),
        &[
            "--git-dir",
            store.to_str().unwrap(),
            "fetch",
            dependency.to_str().unwrap(),
            "HEAD",
        ],
    );
    git(
        dir.path(),
        &[
            "--git-dir",
            store.to_str().unwrap(),
            "update-ref",
            &format!("refs/heads/ferry/{}", sha.trim()),
            "FETCH_HEAD",
        ],
    );
    let refused = run(dir.path(), &["run", "mirror", "--local", "--json"]);
    assert_eq!(receipt(&refused)["outcome"], "refused");
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
fn interrupted_caller_kills_children_and_exits_128_plus_signal(
    signal: nix::sys::signal::Signal,
    code: i32,
) {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let dir = fixture();
    let marker = dir.path().join("child-pid");
    let program = format!(
        "import os,time\nos.setpgrp()\np={:?}\nopen(p+'.tmp','w').write(str(os.getpid()))\nos.rename(p+'.tmp',p)\ntime.sleep(60)",
        marker.to_str().unwrap()
    );
    let config =
        serde_json::json!({"version":1,"recipe":{"hold":{"command":["python3","-c",program]}}});
    fs::write(
        dir.path().join("ferry.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "hold"]);
    let child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .args(["run", "hold", "--local", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut caller = OwnedCaller {
        child: Some(child),
        owners: dir.path().join(".ferry/local/owners"),
    };
    let child = caller.child.as_mut().unwrap();
    let deadline = common::startup_deadline();
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(marker.exists(), "recipe child never reached rendezvous");
    let pid: i32 = fs::read_to_string(&marker).unwrap().parse().unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        signal,
    )
    .unwrap();
    let deadline = common::startup_deadline();
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        panic!("caller did not finish bounded cancellation");
    }
    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
        "sibling process-group child survived"
    );
    let output = caller.child.take().unwrap().wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(code));
    assert_eq!(receipt(&output)["outcome"], "lost");
    assert_eq!(receipt(&output)["exit_code"], code);
}

#[test]
fn a_signal_before_dispatch_still_writes_a_receipt() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let dir = fixture();
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
#[test]
fn hidden_index_flags_refuse_before_resetting_the_seat() {
    let dir = fixture();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(out.status.success());
    let seat = fs::read_dir(dir.path().join(".ferry/local/slots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
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

fn with_adapter(root: &Path, script: &str) {
    fs::write(
        root.join("ferry.toml"),
        "version=1\n[catalog]\nadapter='catalog'\n",
    )
    .unwrap();
    fs::write(root.join("catalog"), script).unwrap();
    fs::set_permissions(root.join("catalog"), fs::Permissions::from_mode(0o755)).unwrap();
    git(root, &["add", "ferry.toml", "catalog"]);
    git(root, &["commit", "-m", "adapter"]);
}
#[test]
fn adapter_runs_from_the_pinned_commit_and_records_both_digests() {
    let dir = fixture();
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
    assert!(String::from_utf8_lossy(&out.stdout).contains("adapter-commit"));
}
#[test]
fn malformed_or_partial_adapter_contract_never_runs_a_valid_subset() {
    let dir = fixture();
    with_adapter(
        dir.path(),
        "#!/bin/sh\nprintf '%s\\n' '{\"version\":1,\"recipe\":{\"pass\":{\"command\":[\"true\"]},\"bad\":{\"requires\":[]}}}'\n",
    );
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(receipt(&out)["outcome"], "refused");
    assert!(receipt(&out)["adapter_sha256"].is_string());
    assert!(receipt(&out)["contract_sha256"].is_string());
    assert!(!dir.path().join(".ferry/local").exists());
}
#[test]
fn nondeterministic_adapter_is_a_refusal() {
    let dir = fixture();
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

/// A host reached through an SSH shim: `host` is its HOME, `binary` its
/// ferry path relative to that HOME.
fn remote_run(dir: &Path, binary: &str, args: &[&str]) -> Output {
    remote_command(dir, binary, args).output().unwrap()
}
fn remote_command(dir: &Path, binary: &str, args: &[&str]) -> Command {
    let host = dir.join("host");
    let tools = dir.join("transport-tools");
    fs::create_dir_all(host.join(".local/bin")).unwrap();
    fs::create_dir_all(&tools).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_ferry"), host.join(".local/bin/ferry")).unwrap();
    // FERRY_TEST_SLOW_PUSH names a marker: a git push touches it, then stalls.
    fs::write(tools.join("ssh"), "#!/bin/sh\nfor last do :; done\ncase \"$last\" in *git-receive-pack*) if [ -n \"$FERRY_TEST_SLOW_PUSH\" ]; then touch \"$FERRY_TEST_SLOW_PUSH\"; sleep 3; fi;; esac\ncd \"$FERRY_TEST_HOST\" || exit 125\nexport HOME=\"$FERRY_TEST_HOST\"\nexec sh -c \"$last\"\n").unwrap();
    fs::set_permissions(tools.join("ssh"), fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.join("config/ferry");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("machines.toml"),
        format!(
            "[host.test-host]\nssh='test-host'\nbinary='{binary}'\ntriple='{}-{}'\ncores=1\nfeatures=[]\n",
            std::env::consts::ARCH,
            if cfg!(target_os = "macos") {
                "apple-darwin"
            } else {
                "unknown-linux-gnu"
            }
        ),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("HOME", dir)
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("GIT_SSH", tools.join("ssh"))
        .env("FERRY_TEST_HOST", &host)
        .args(args);
    command
}

#[test]
fn a_signal_during_the_push_stops_the_run_before_dispatch() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let dir = fixture();
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
        !stdout.contains("\"accepted\""),
        "the host was asked to run: {stdout}"
    );
    assert!(!stdout.contains("committed"));
    assert!(!dir.path().join("host/.local/state/ferry/records").exists());
}

#[test]
fn a_host_refusal_before_acceptance_is_refused_not_lost() {
    let dir = fixture();
    // A file where the host keeps its run records: it cannot reserve the id.
    fs::create_dir_all(dir.path().join("host/.local/state/ferry")).unwrap();
    fs::write(dir.path().join("host/.local/state/ferry/records"), "").unwrap();
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
            .starts_with("host refused before acceptance: creating "),
        "{}",
        record["reason"]
    );
    // The receipt names the host binary that refused.
    assert_eq!(record["host_binary_sha256"].as_str().unwrap().len(), 64);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("committed"));
}

#[test]
fn an_older_host_exiting_125_before_acceptance_is_refused_not_lost() {
    let dir = fixture();
    let bin = dir.path().join("host/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    // An older host-run says nothing on the protocol and exits 125.
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
    assert_eq!(out.status.code(), Some(125));
    let record = receipt(&out);
    assert_eq!(record["outcome"], "refused");
    assert_eq!(record["exit_code"], 125);
}

fn seat_record(dir: &Path, kind: &str) -> Option<serde_json::Value> {
    let records = dir.join(".ferry/local").join(kind);
    let entry = fs::read_dir(records).ok()?.next()?.ok()?;
    serde_json::from_slice(&fs::read(entry.path()).ok()?).ok()
}

#[test]
fn a_normal_release_records_the_seat_idle_where_custody_is_supported() {
    let dir = fixture();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(out.status.success());
    let custody = &receipt(&out)["custody"];
    if cfg!(target_os = "linux") {
        assert_eq!(custody["census"], "empty");
        assert_eq!(custody["record"], "idle");
        assert!(seat_record(dir.path(), "idle").is_some());
    } else {
        // No complete census here, so the seat is never claimed idle.
        assert_eq!(custody["census"], "unsupported");
        assert_eq!(custody["record"], "none");
        assert!(seat_record(dir.path(), "idle").is_none());
    }
    assert!(seat_record(dir.path(), "taint").is_none());
}

/// Commits a recipe `escape` running `script` (and an optional collect hook),
/// runs it locally, and returns its receipt and elapsed time.
#[cfg(target_os = "linux")]
fn run_escape(
    dir: &Path,
    script: &str,
    collect: Option<&str>,
    diagnostic_proc: Option<&str>,
) -> (Output, std::time::Duration) {
    let mut config = String::from("version=1\n");
    if let Some(hook) = collect {
        fs::write(dir.join("collect"), hook).unwrap();
        fs::set_permissions(dir.join("collect"), fs::Permissions::from_mode(0o755)).unwrap();
        git(dir, &["add", "collect"]);
        config.push_str("[hooks]\ncollect='collect'\n");
    }
    let recipe = toml::to_string(
        &serde_json::json!({"recipe": {"escape": {"command": ["sh", "-c", script]}}}),
    )
    .unwrap();
    config.push_str(&recipe);
    fs::write(dir.join("ferry.toml"), config).unwrap();
    git(dir, &["add", "ferry.toml"]);
    git(dir, &["commit", "-m", "escape"]);
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .args(["run", "escape", "--local", "--json"]);
    if let Some(proc_root) = diagnostic_proc {
        command.env("FERRY_DIAGNOSTIC_PROC", proc_root);
    }
    let started = std::time::Instant::now();
    let out = command.output().unwrap();
    (out, started.elapsed())
}

/// A setsid escapee that drops environment, descriptors and directory, then
/// uses the cache by path after a delay.
#[cfg(target_os = "linux")]
const ESCAPEE: &str = "command -v setsid >/dev/null || exit 97; late=\"$FERRY_CACHE_DIR/late\"; nap=$(command -v sleep); mark=$(command -v touch); setsid env -i sh -c \"exec </dev/null >/dev/null 2>&1; cd /; $nap 5; $mark $late\" & exit 0";

#[cfg(target_os = "linux")]
fn late_mark(dir: &Path) -> bool {
    let caches = fs::read_dir(dir.join(".ferry/local/caches"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    caches.join("late").exists()
}

#[cfg(target_os = "linux")]
#[test]
fn an_escaped_descendant_is_stopped_at_release_and_the_seat_is_idle() {
    use std::{thread, time::Duration};
    let dir = fixture();
    let (out, _) = run_escape(dir.path(), ESCAPEE, None, None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let custody = receipt(&out)["custody"].clone();
    assert!(
        !custody["census"]["survivors"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{custody}"
    );
    assert!(
        !custody["stopped"].as_array().unwrap().is_empty(),
        "{custody}"
    );
    assert_eq!(custody["after"], "empty", "{custody}");
    assert_eq!(custody["record"], "idle", "{custody}");
    let idle = seat_record(dir.path(), "idle").unwrap();
    assert_eq!(idle["stopped"], custody["stopped"]);
    assert!(seat_record(dir.path(), "taint").is_none());
    // Stopped before it could use the cache.
    thread::sleep(Duration::from_secs(7));
    assert!(!late_mark(dir.path()));
}

#[cfg(target_os = "linux")]
#[test]
fn a_term_ignoring_survivor_is_killed_after_the_grace_period() {
    let dir = fixture();
    let script = "command -v setsid >/dev/null || exit 97; setsid sh -c 'trap \"\" TERM; exec </dev/null >/dev/null 2>&1; while :; do sleep 0.1; done' & exit 0";
    let (out, elapsed) = run_escape(dir.path(), script, None, None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let custody = receipt(&out)["custody"].clone();
    assert_eq!(custody["record"], "idle", "{custody}");
    assert_eq!(custody["after"], "empty", "{custody}");
    assert!(elapsed >= std::time::Duration::from_secs(10), "{elapsed:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_blind_diagnostic_cannot_attribute_survivors_so_the_seat_is_tainted() {
    use std::{thread, time::Duration};
    // /proc shows nothing, but the kernel still reports the live child: it
    // cannot be stopped by pid, so the seat is tainted and refuses.
    let dir = fixture();
    let (out, _) = run_escape(dir.path(), ESCAPEE, None, Some("/nonexistent"));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let custody = receipt(&out)["custody"].clone();
    assert_eq!(custody["record"], "taint", "{custody}");
    assert!(custody["stopped"].is_null(), "{custody}");
    assert!(seat_record(dir.path(), "idle").is_none());
    let taint = seat_record(dir.path(), "taint").unwrap();
    let again = run(dir.path(), &["run", "escape", "--local", "--json"]);
    assert_eq!(again.status.code(), Some(125));
    assert!(
        receipt(&again)["reason"]
            .as_str()
            .unwrap()
            .contains("tainted"),
        "{}",
        receipt(&again)
    );
    // Not stopped: the escapee did use the cache afterwards.
    thread::sleep(Duration::from_secs(7));
    assert!(late_mark(dir.path()));
    assert_eq!(taint["survivors"], serde_json::json!([]));
}

#[cfg(target_os = "linux")]
#[test]
fn collect_runs_before_survivors_are_stopped_and_its_failure_is_recorded() {
    let dir = fixture();
    let pidfile = dir.path().join("escapee.pid");
    let seen = dir.path().join("collect-saw");
    let script = format!(
        "command -v setsid >/dev/null || exit 97; setsid sh -c 'echo $$ > {pid}; exec </dev/null >/dev/null 2>&1; while :; do sleep 0.1; done' & while [ ! -s {pid} ]; do sleep 0.05; done; exit 0",
        pid = pidfile.display()
    );
    let hook = format!(
        "#!/bin/sh\nif kill -0 \"$(cat {pid})\" 2>/dev/null; then echo alive > {seen}; else echo gone > {seen}; fi\nexit 3\n",
        pid = pidfile.display(),
        seen = seen.display()
    );
    let (out, _) = run_escape(dir.path(), &script, Some(&hook), None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(fs::read_to_string(&seen).unwrap().trim(), "alive");
    let record = receipt(&out);
    assert_eq!(record["collection"]["exit_code"], 3, "{record}");
    assert_eq!(record["custody"]["record"], "idle", "{record}");
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        record["custody"]["stopped"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(pid)),
        "{record}"
    );
    assert!(
        kill(Pid::from_raw(pid), None).is_err(),
        "the escapee survived"
    );
}

/// The internal name of the only seat a local run has used.
fn local_seat(dir: &Path) -> String {
    fs::read_dir(dir.join(".ferry/local/locks"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|name| !name.starts_with("host-") && !name.starts_with("cleanup-pending-"))
        .unwrap()
}

#[test]
fn a_tainted_seat_refuses_until_recovery_proves_its_survivors_gone() {
    let dir = fixture();
    assert!(
        run(dir.path(), &["run", "pass", "--local", "--json"])
            .status
            .success()
    );
    let seat = local_seat(dir.path());
    let taint = dir.path().join(format!(".ferry/local/taint/{seat}.json"));
    fs::create_dir_all(taint.parent().unwrap()).unwrap();
    // The survivor is this test process itself: live, and fatal to the
    // control if recovery ever killed it. Spawning a long-lived helper here
    // could inherit other tests' output pipes on macOS and stall them.
    let pid = std::process::id();
    let write_taint = |start: serde_json::Value| {
        fs::write(
            &taint,
            serde_json::json!({"id": "earlier", "reason": "descendants survived the session",
                "pids": [pid], "survivors": [{"pid": pid, "start": start}]})
            .to_string(),
        )
        .unwrap();
    };
    // Without a start time, any live process with the pid is the survivor.
    write_taint(serde_json::Value::Null);
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(125));
    let record = receipt(&out);
    assert_eq!(record["outcome"], "refused");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains("tainted") && reason.contains("seat recover"),
        "{reason}"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("committed"));
    // Recovery refuses while the survivor lives, and never kills it.
    let out = run(dir.path(), &["seat", "recover", &seat, "--local"]);
    assert_eq!(out.status.code(), Some(125));
    assert!(taint.exists());
    // A recorded start time that differs means the pid was reused: gone.
    write_taint(serde_json::json!(1));
    let out = run(dir.path(), &["seat", "recover", &seat, "--local"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!taint.exists());
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(out.status.success());
}

/// Runs of the `hold` recipe that a lease test spawned. Dropping it releases
/// every run and then stops exactly the callers it spawned, so a failed
/// assertion never leaves a run waiting for a lock or a release file.
struct HoldRuns {
    log: std::path::PathBuf,
    pids: Vec<i32>,
}
impl HoldRuns {
    fn spawn(&mut self, dir: &Path, name: &str, exclusive: bool) -> std::process::Child {
        let mut args = vec!["run", "hold", "--local", "--json", "--seat", name];
        if exclusive {
            args.push("--exclusive");
        }
        args.extend(["--", name]);
        let child = Command::new(env!("CARGO_BIN_EXE_ferry"))
            .current_dir(dir)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .args(args)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        self.pids.push(i32::try_from(child.id()).unwrap());
        child
    }
    fn release(&self, name: &str) {
        fs::write(format!("{}.{name}", self.log.display()), "").unwrap();
    }
    fn logged(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}
impl Drop for HoldRuns {
    fn drop(&mut self) {
        for name in ["first", "alone", "later"] {
            let _ = fs::write(format!("{}.{name}", self.log.display()), "");
        }
        for pid in &self.pids {
            let _ = kill(Pid::from_raw(*pid), Signal::SIGTERM);
        }
    }
}

/// The three lease receipts, in the order first, alone, later.
fn assert_lease_windows(receipts: &[serde_json::Value], held_for: std::time::Duration, dir: &Path) {
    assert_eq!(receipts[0]["lease"]["mode"], "shared");
    assert_eq!(receipts[1]["lease"]["mode"], "exclusive");
    // It held the gate and waited at least from then until A was released.
    assert!(
        u128::from(receipts[1]["lease"]["wait_ms"].as_u64().unwrap()) >= held_for.as_millis(),
        "{} < {held_for:?}",
        receipts[1]["lease"]["wait_ms"]
    );
    assert_eq!(receipts[2]["lease"]["mode"], "shared");
    // The release and acquire times order the windows the same way.
    let at = |i: usize, field: &str| receipts[i]["lease"][field].as_u64().unwrap();
    assert!(at(0, "released_ms") <= at(1, "acquired_ms"));
    assert!(at(1, "released_ms") <= at(2, "acquired_ms"));
    // Older binaries used this name for their exclusive lock.
    assert!(dir.join(".ferry/local/locks/host-exclusive").exists());
    assert!(
        receipts[1]["requirements"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("exclusive"))
    );
}

#[test]
fn an_exclusive_run_waits_for_in_flight_runs_and_keeps_new_ones_out() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let dir = fixture();
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
    let gate = dir.path().join(".ferry/local/locks/host-gate");
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

fn with_host_policy(dir: &Path, policy: &str) {
    let config = dir.join("config/ferry");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("host.toml"), policy).unwrap();
}

/// A seat as an earlier session would have left it: a cache with a target and
/// a sibling, a lock file, and optionally an idle, taint or owner record.
fn synthetic_seat(state: &Path, seat: &str, released_ms: Option<u64>, extra: Option<&str>) {
    fs::create_dir_all(state.join("caches").join(seat).join("cargo-target/deps")).unwrap();
    fs::write(
        state
            .join("caches")
            .join(seat)
            .join("cargo-target/deps/blob"),
        vec![7u8; 4096],
    )
    .unwrap();
    fs::create_dir_all(state.join("caches").join(seat).join("keep")).unwrap();
    fs::write(state.join("caches").join(seat).join("keep/warm"), "warm").unwrap();
    fs::create_dir_all(state.join("locks")).unwrap();
    fs::write(state.join("locks").join(seat), "").unwrap();
    if let Some(released_ms) = released_ms {
        fs::create_dir_all(state.join("idle")).unwrap();
        fs::write(
            state.join("idle").join(format!("{seat}.json")),
            serde_json::json!({"released_ms": released_ms, "id": "earlier", "commit": "0"})
                .to_string(),
        )
        .unwrap();
    }
    if let Some(kind) = extra {
        fs::create_dir_all(state.join(kind)).unwrap();
        // A well-formed record: an unreadable taint is unknown, which any
        // run waiting for the host lease must refuse.
        let record = if kind == "taint" {
            serde_json::json!({"id": "earlier", "reason": "descendants survived the session",
                "pids": [], "survivors": [],
                "locks": {"seat": seat, "host": "shared", "gate": false}})
            .to_string()
        } else {
            "{}".to_owned()
        };
        fs::write(state.join(kind).join(format!("{seat}.json")), record).unwrap();
    }
}

#[test]
fn admission_without_a_policy_is_not_configured_and_claims_no_floor() {
    let dir = fixture();
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
        let dir = fixture();
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
fn version_one_guards_the_floor_and_never_evicts() {
    let dir = fixture();
    let state = dir.path().join(".ferry/local");
    synthetic_seat(&state, "idle-one", Some(1), None);
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
    assert!(admission["available_before"].as_u64().is_some());
    assert_eq!(admission["evicted"], serde_json::json!([]));
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("below host floor")
    );
    assert!(
        state
            .join("caches/idle-one/cargo-target/deps/blob")
            .exists()
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("committed"));
}

#[test]
fn version_two_admits_without_eviction_when_the_floor_is_met() {
    let dir = fixture();
    let state = dir.path().join(".ferry/local");
    synthetic_seat(&state, "idle-one", Some(1), None);
    with_host_policy(
        dir.path(),
        "version = 2\nmin_free_bytes = 1\nevict = { mode = 'idle-seat-targets', targets = ['cargo-target'] }\n",
    );
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert!(out.status.success());
    let admission = &receipt(&out)["admission"];
    assert_eq!(admission["result"], "admitted");
    assert_eq!(admission["evicted"], serde_json::json!([]));
    assert!(
        state
            .join("caches/idle-one/cargo-target/deps/blob")
            .exists()
    );
}

#[test]
fn version_two_evicts_only_designated_targets_of_eligible_idle_seats_oldest_first() {
    let dir = fixture();
    let state = dir.path().join(".ferry/local");
    synthetic_seat(&state, "older", Some(1), None);
    synthetic_seat(&state, "newer", Some(2), None);
    synthetic_seat(&state, "tainted", Some(0), Some("taint"));
    synthetic_seat(&state, "owned", Some(0), Some("owners"));
    synthetic_seat(&state, "busy", Some(0), None);
    synthetic_seat(&state, "unknown", None, None);
    synthetic_seat(&state, "linked", Some(0), None);
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("precious"), "keep").unwrap();
    fs::remove_dir_all(state.join("caches/linked/cargo-target")).unwrap();
    std::os::unix::fs::symlink(&outside, state.join("caches/linked/cargo-target")).unwrap();
    let busy = fs::File::open(state.join("locks/busy")).unwrap();
    fs2::FileExt::lock_exclusive(&busy).unwrap();
    with_host_policy(
        dir.path(),
        "version = 2\nmin_free_bytes = 1000000000000000000\nevict = { mode = 'idle-seat-targets', targets = ['cargo-target'] }\n",
    );
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    drop(busy);
    assert_eq!(out.status.code(), Some(125));
    let admission = receipt(&out)["admission"].clone();
    assert_eq!(admission["result"], "refused");
    let evicted: Vec<&str> = admission["evicted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seat"].as_str().unwrap())
        .collect();
    assert_eq!(evicted, ["older", "newer"], "{admission}");
    for entry in admission["evicted"].as_array().unwrap() {
        assert!(entry["path"].as_str().unwrap().ends_with("/cargo-target"));
        assert_eq!(entry["logical_bytes"], 4096);
        assert!(entry["allocated_bytes"].as_u64().is_some());
        assert!(entry["available_after"].as_u64().is_some());
    }
    let skipped = &admission["skipped"];
    assert_eq!(
        (
            &skipped["tainted"],
            &skipped["owners"],
            &skipped["locked"],
            &skipped["unsafe_path"]
        ),
        (
            &serde_json::json!(1),
            &serde_json::json!(1),
            &serde_json::json!(1),
            &serde_json::json!(1)
        ),
        "{admission}"
    );
    assert!(skipped["no_idle_record"].as_u64().unwrap() >= 1);
    for seat in ["older", "newer"] {
        assert!(
            !state
                .join("caches")
                .join(seat)
                .join("cargo-target")
                .exists()
        );
        assert!(state.join("caches").join(seat).join("keep/warm").exists());
        assert!(!state.join("idle").join(format!("{seat}.json")).exists());
    }
    for seat in ["tainted", "owned", "busy", "unknown"] {
        assert!(
            state
                .join("caches")
                .join(seat)
                .join("cargo-target/deps/blob")
                .exists()
        );
    }
    assert!(outside.join("precious").exists());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("committed"));
}

#[cfg(target_os = "linux")]
#[test]
fn a_real_idle_release_is_reclaimed_by_a_later_admission() {
    let dir = fixture();
    fs::write(
        dir.path().join("ferry.toml"),
        "version=1\n[recipe.build]\ncommand=['sh','-c','mkdir -p \"$FERRY_CACHE_DIR/cargo-target\" \"$FERRY_CACHE_DIR/keep\" && head -c 8192 /dev/zero > \"$FERRY_CACHE_DIR/cargo-target/out\" && echo warm > \"$FERRY_CACHE_DIR/keep/warm\"']\n",
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "build"]);
    let out = run(
        dir.path(),
        &["run", "build", "--local", "--json", "--seat", "producer"],
    );
    assert!(out.status.success());
    assert_eq!(receipt(&out)["custody"]["record"], "idle");
    with_host_policy(
        dir.path(),
        "version = 2\nmin_free_bytes = 1000000000000000000\nevict = { mode = 'idle-seat-targets', targets = ['cargo-target'] }\n",
    );
    let out = run(
        dir.path(),
        &["run", "build", "--local", "--json", "--seat", "consumer"],
    );
    assert_eq!(out.status.code(), Some(125));
    let admission = &receipt(&out)["admission"];
    let evicted = admission["evicted"].as_array().unwrap();
    assert_eq!(evicted.len(), 1, "{admission}");
    assert!(evicted[0]["seat"].as_str().unwrap().ends_with("-producer"));
    assert_eq!(evicted[0]["logical_bytes"], 8192);
    let cache = fs::read_dir(dir.path().join(".ferry/local/caches"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.to_string_lossy().ends_with("-producer"))
        .unwrap();
    assert!(!cache.join("cargo-target").exists());
    assert!(cache.join("keep/warm").exists());
}

#[test]
fn an_unreadable_custody_marker_leaves_the_seat_alone() {
    let dir = fixture();
    let state = dir.path().join(".ferry/local");
    synthetic_seat(&state, "hidden", Some(1), None);
    // The taint directory exists but cannot be searched: whether this seat is
    // tainted is unknown, which must not read as "not tainted".
    fs::create_dir_all(state.join("taint")).unwrap();
    fs::set_permissions(state.join("taint"), fs::Permissions::from_mode(0o000)).unwrap();
    with_host_policy(
        dir.path(),
        "version = 2\nmin_free_bytes = 1000000000000000000\nevict = { mode = 'idle-seat-targets', targets = ['cargo-target'] }\n",
    );
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    fs::set_permissions(state.join("taint"), fs::Permissions::from_mode(0o755)).unwrap();
    // The run's own seat cannot be confirmed untainted either, so it refuses
    // before admission measures or evicts anything.
    assert_eq!(out.status.code(), Some(125));
    let record = receipt(&out);
    assert_eq!(record["outcome"], "refused");
    assert!(record["admission"].is_null(), "{record}");
    assert!(state.join("caches/hidden/cargo-target/deps/blob").exists());
}

#[test]
fn a_project_can_require_provider_capabilities() {
    let dir = fixture();
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

#[test]
fn a_host_without_a_required_capability_is_refused_before_acceptance() {
    let dir = fixture();
    fs::write(
        dir.path().join("ferry.toml"),
        "version=1\n[provider]\nrequires=['admission']\n[recipe.pass]\ncommand=['sh','-c','echo committed']\nroutable=true\n",
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "requires admission"]);
    let bin = dir.path().join("host/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    // A host from before capabilities: its host-info names none.
    fs::write(
        bin.join("old-ferry"),
        "#!/bin/sh\ncase \"$1\" in host-info) \"$HOME/.local/bin/ferry\" host-info | sed 's/,\"capabilities\":\\[[^]]*\\]//';; *) exec \"$HOME/.local/bin/ferry\" \"$@\";; esac\n",
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
    assert!(!String::from_utf8_lossy(&out.stdout).contains("\"accepted\""));
}

#[test]
fn host_run_refuses_a_request_requiring_a_capability_it_lacks() {
    let dir = tempfile::tempdir().unwrap();
    let request = serde_json::json!({
        "protocol": 1, "id": "1-aa", "token": "tok", "commit": "0".repeat(40), "seat": "s",
        "recipe": {"command": ["true"]}, "hooks": {}, "caller_binary_sha256": "x",
        "timeout_seconds": null, "mirrors": [], "forwarded_env": {},
        "provider_requires": ["teleport"]
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("host-run")
        .env("HOME", dir.path())
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
    assert_eq!(out.status.code(), Some(125));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"event\":\"refused\""), "{stdout}");
    assert!(stdout.contains("teleport"), "{stdout}");
    assert!(!stdout.contains("\"accepted\""));
    assert!(!dir.path().join(".local/state/ferry/records").exists());
}

// Only Linux adopts descendants; Apple's ps cannot run in the Darwin Nix sandbox.
#[cfg(target_os = "linux")]
#[test]
fn a_recipe_can_empty_a_process_group_it_killed() {
    // Like a project's own teardown: kill a group whose members were adopted
    // by the supervisor, then require the group to be empty. Adopted zombies
    // must not linger as members.
    let dir = fixture();
    fs::write(
        dir.path().join("group.sh"),
        r#"python3 -c 'import os,subprocess; os.setpgid(0,0); subprocess.Popen(["sleep","60"]); subprocess.Popen(["sleep","60"]); os.execvp("sleep",["sleep","61"])' &
g=$!
# Ready only once the leader and both members are in the group.
i=0
until [ "$(ps -eo pgid= | awk -v g="$g" '$1 == g' | wc -l)" -ge 3 ]; do
  i=$((i + 1))
  [ "$i" -lt 300 ] || { echo "group never formed"; exit 3; }
  sleep 0.1
done
kill -s KILL -- "-$g"
wait "$g" 2>/dev/null
i=0
while [ "$i" -lt 20 ]; do
  [ -z "$(ps -eo pgid=,stat= | awk -v g="$g" '$1 == g')" ] && exit 7
  sleep 0.1
  i=$((i + 1))
done
ps -eo pid=,ppid=,pgid=,stat= | awk -v g="$g" '$3 == g'
exit 1
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("ferry.toml"),
        "version=1\n[recipe.group]\ncommand=['sh','group.sh']\n",
    )
    .unwrap();
    git(dir.path(), &["add", "ferry.toml", "group.sh"]);
    git(dir.path(), &["commit", "-m", "group teardown"]);
    let out = run(dir.path(), &["run", "group", "--local", "--json"]);
    // 7 means the group emptied; 3 means it never formed. The recipe's own
    // exit status is preserved either way.
    assert_eq!(
        out.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(receipt(&out)["outcome"], "ran-fail");
    assert_eq!(receipt(&out)["exit_code"], 7);
}
