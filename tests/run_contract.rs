//! Real CLI contracts exercise committed bytes and durable refusal receipts.
mod common;
use common::{git, receipt};
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
/// A caller, and the session of the recipe whose pid it wrote to `member`:
/// both stopped if the control ends early.
struct OwnedCaller {
    child: Option<std::process::Child>,
    member: std::path::PathBuf,
}
impl Drop for OwnedCaller {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Startup assertions can fail before the session guard is constructed.
        let session = fs::read_to_string(&self.member)
            .ok()
            .and_then(|pid| pid.trim().parse().ok())
            .and_then(|pid| getsid(Some(Pid::from_raw(pid))).ok());
        if let Some(session) = session {
            drop(OwnedGroup(session.as_raw()));
        }
    }
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
        .env("XDG_STATE_HOME", root.join("state"))
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
    assert!(String::from_utf8_lossy(&out.stderr).contains("committed"));
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
    assert!(String::from_utf8_lossy(&second.stderr).contains("warm"));
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
    let seat_lock = fs::read_dir(dir.path().join(".ferry/local/locks"))
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
    let locks = dir.path().join(".ferry/local/locks");
    let held = |name: &str| {
        let probe = fs::File::open(locks.join(name)).unwrap();
        match fs2::FileExt::try_lock_exclusive(&probe) {
            Ok(()) => false,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(e) => panic!("probing {name}: {e}"),
        }
    };
    // The session inherited the seat lock and host lease, on every
    // platform: they stay held while the recipe lives, though its
    // supervisor is gone.
    assert!(held(seat), "the seat lock freed while the recipe lives");
    assert!(
        held("host-exclusive"),
        "the host lease freed while the recipe lives"
    );
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
    let slot = fs::read_dir(dir.path().join(".ferry/local/slots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
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
/// not ship it; the seat clone's own config lists what Ferry made.
#[test]
fn a_ref_no_longer_shipped_is_removed_from_the_seat() {
    let dir = fixture();
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
    let slot = fs::read_dir(dir.path().join(".ferry/local/slots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
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
}

/// The keys `[[ship]]` replaced refuse, naming it, from ferry.toml and from a
/// catalog adapter alike; entries from both are combined, and the same ref
/// or URL declared twice refuses.
#[test]
fn retired_ship_keys_refuse_and_entries_from_both_owners_combine() {
    let dir = fixture();
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
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .args(["run", "hold", "--local", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut caller = OwnedCaller {
        child: Some(child),
        member: marker.clone(),
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
    // The caller asked the place to stop the run with its own signal.
    assert_eq!(output.status.code(), Some(code));
    assert_eq!(receipt(&output)["outcome"], "stopped");
    assert_eq!(receipt(&output)["exit_code"], code);
    assert!(receipt(&output)["stopped_by"]["why"].is_string());
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
    assert!(String::from_utf8_lossy(&out.stderr).contains("adapter-commit"));
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
    // By rename, never in place: macOS kills a process whose signed binary is
    // rewritten under it (seen as `signal: 9` on a later verb; ferry-xtx.15).
    let staged = host.join(".local/bin/.ferry.new");
    fs::copy(env!("CARGO_BIN_EXE_ferry"), &staged).unwrap();
    fs::rename(&staged, host.join(".local/bin/ferry")).unwrap();
    // FERRY_TEST_SLOW_PUSH names a marker: a git push touches it, then stalls.
    // A file `unreachable` on the host fails every follow, as a dead link;
    // a file `fail-record-once` fails the next `place record`, once.
    fs::write(tools.join("ssh"), "#!/bin/sh\nfor last do :; done\ncase \"$last\" in *git-receive-pack*) if [ -n \"$FERRY_TEST_SLOW_PUSH\" ]; then touch \"$FERRY_TEST_SLOW_PUSH\"; sleep 3; fi;; *\" place \"*\"follow\"*) if [ -e \"$FERRY_TEST_HOST/unreachable\" ]; then exit 255; fi;; *\" place \"*\"record\"*) if [ -e \"$FERRY_TEST_HOST/fail-record-once\" ]; then rm -f \"$FERRY_TEST_HOST/fail-record-once\"; exit 255; fi;; esac\ncd \"$FERRY_TEST_HOST\" || exit 125\nexport HOME=\"$FERRY_TEST_HOST\"\nexec sh -c \"$last\"\n").unwrap();
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
        .env("XDG_STATE_HOME", dir.join("state"))
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
        !stdout.contains("\"submitted\""),
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
    let dir = fixture();
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
            .env("XDG_STATE_HOME", dir.join("state"))
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
fn version_one_guards_the_floor() {
    let dir = fixture();
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
    assert!(
        record["reason"]
            .as_str()
            .unwrap()
            .contains("below host floor")
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("committed"));
}

/// A place record written by the 4a binary, with the old custody and
/// admission shapes, still completes its run: `ferry record` keeps both as
/// written (4b review R3, the critic's x1).
#[test]
fn ferry_record_completes_a_run_whose_record_has_the_old_shapes() {
    let dir = fixture();
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let id = receipt(&out)["id"].as_str().unwrap().to_owned();
    let old_custody = serde_json::json!({"census": "unsupported", "record": "none"});
    let old_admission = serde_json::json!({
        "policy": "not-configured", "floor_bytes": null, "filesystem": null,
        "available_before": 7, "evicted": [], "skipped": {"busy": 0},
        "available_after": 7, "result": "admitted",
    });
    let place = dir.path().join(format!(".ferry/local/records/{id}.json"));
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

/// A real version 2 policy, with its `evict` block, gets the retirement and
/// its remedy, not a schema error (murail-4a's db0c1892).
#[test]
fn an_actual_version_two_eviction_policy_names_the_version_one_remedy() {
    let dir = fixture();
    with_host_policy(
        dir.path(),
        "version = 2\nmin_free_bytes = 1\nevict = { mode = 'idle-seat-targets', targets = ['cargo-target'] }\n",
    );
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(125));
    let record = receipt(&out);
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains("version 2 eviction is retired")
            && reason.contains("keep min_free_bytes as version 1"),
        "{record}"
    );
}

/// Eviction is retired with the idle records it read: a version 2 policy
/// refuses, naming what to keep (slice 4b; seats are bounded by slice 5).
#[test]
fn a_version_two_policy_refuses_naming_version_one() {
    let dir = fixture();
    with_host_policy(dir.path(), "version = 2\nmin_free_bytes = 1\n");
    let out = run(dir.path(), &["run", "pass", "--local", "--json"]);
    assert_eq!(out.status.code(), Some(125));
    let reason = receipt(&out)["reason"].as_str().unwrap().to_owned();
    assert!(reason.contains("version 2 eviction is retired"), "{reason}");
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
    let dir = fixture();
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
    assert!(!dir.path().join("host/.local/state/ferry/records").exists());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("\"submitted\""));
}

#[test]
fn a_place_refuses_a_request_requiring_a_capability_it_lacks() {
    let dir = tempfile::tempdir().unwrap();
    let request = serde_json::json!({
        "protocol": 1, "id": "1-aa", "token": "tok", "commit": "0".repeat(40), "seat": "s",
        "recipe": {"command": ["true"]}, "hooks": {}, "caller_binary_sha256": "x",
        "timeout_seconds": null, "forwarded_env": {},
        "provider_requires": ["teleport"]
    });
    let (code, reply) = submit(dir.path(), &request);
    assert_eq!(code, Some(125), "{reply}");
    assert!(reply.contains("teleport"), "{reply}");
    assert!(!dir.path().join("records").exists());
    // A caller from before detached runs is refused, not half served.
    let old = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("host-run")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(old.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&old.stdout).contains("\"event\":\"refused\""));
}

/// Submits `request` to `ferry place --state <state> submit`; its exit code
/// and reply.
fn submit(state: &Path, request: &serde_json::Value) -> (Option<i32>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("place")
        .arg("--state")
        .arg(state)
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
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
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
            "recipe": {"command": ["true"]}, "hooks": {}, "caller_binary_sha256": "x",
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
    let dir = fixture();
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
    let dir = fixture();
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

#[test]
fn an_unreachable_place_leaves_the_receipt_submitted_until_ferry_record() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture();
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
        .join(format!("host/.local/state/ferry/records/{id}.json"));
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

/// A second workspace of the fixture's repository, at `dir/<name>`.
fn workspace(dir: &Path, name: &str) -> std::path::PathBuf {
    git(dir, &["clone", "--quiet", ".", name]);
    dir.join(name)
}

/// The caller's seat book, one JSON value per line.
fn seats_book(dir: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(dir.join("state/ferry/seats.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The key this caller recorded for its seat in `repo`.
fn key_of(dir: &Path, repo: &Path) -> String {
    let repo = repo.canonicalize().unwrap().display().to_string();
    seats_book(dir)
        .iter()
        .find(|line| line["repo_path"] == repo.as_str())
        .and_then(|line| line["key"].as_str())
        .expect("the seat was recorded")
        .to_owned()
}

fn remote_pass(dir: &Path, repo: &Path) -> Output {
    remote_command(
        dir,
        ".local/bin/ferry",
        &["run", "pass", "--on", "test-host", "--json"],
    )
    .current_dir(repo)
    .output()
    .unwrap()
}

fn prune(dir: &Path, binary: &str) -> (Option<i32>, String) {
    let out = remote_command(dir, binary, &["seats", "prune"])
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// `ferry seats prune` retires a seat whose workspace is gone, through the
/// place's `retire` verb, and spares one whose workspace exists. The lock
/// file survives; the place records the retire and the book a tombstone
/// (SPEC §9, slice 5).
#[test]
fn seats_prune_retires_a_gone_workspaces_seat_and_spares_a_live_one() {
    let dir = fixture();
    let gone = workspace(dir.path(), "gone");
    assert_eq!(remote_pass(dir.path(), dir.path()).status.code(), Some(0));
    assert_eq!(remote_pass(dir.path(), &gone).status.code(), Some(0));
    let live = key_of(dir.path(), dir.path());
    let retired = key_of(dir.path(), &gone);
    assert_ne!(live, retired);
    let place = dir.path().join("host/.local/state/ferry");
    assert!(place.join("slots").join(&retired).exists());
    fs::remove_dir_all(&gone).unwrap();
    let (code, said) = prune(dir.path(), ".local/bin/ferry");
    assert_eq!(code, Some(0), "{said}");
    assert!(said.contains("retired"), "{said}");
    assert!(!place.join("slots").join(&retired).exists(), "{said}");
    assert!(place.join("locks").join(&retired).exists());
    assert!(
        place
            .join(format!("records/retired-{retired}.json"))
            .exists()
    );
    assert!(place.join("slots").join(&live).exists());
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
    let dir = fixture();
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
    let place = dir.path().join("host/.local/state/ferry");
    assert!(place.join("slots").join(&key).exists());
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
    let dir = fixture();
    let gone = workspace(dir.path(), "gone");
    assert_eq!(remote_pass(dir.path(), &gone).status.code(), Some(0));
    let key = key_of(dir.path(), &gone);
    fs::remove_dir_all(&gone).unwrap();
    let lock = dir.path().join("host/.local/state/ferry/locks").join(&key);
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
    let dir = fixture();
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
    let place = dir.path().join("host/.local/state/ferry");
    assert!(place.join("slots").join(&key).exists());
}

/// The release points of retire: a failed delete or record is reported and
/// refused (125), and a later prune finishes the retire.
#[test]
fn a_failed_retire_is_reported_and_a_later_prune_finishes_it() {
    if !cfg!(debug_assertions) {
        return;
    }
    for point in ["retire-delete", "retire-record"] {
        let dir = fixture();
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
        let (code, said) = prune(dir.path(), ".local/bin/ferry");
        assert_eq!(code, Some(0), "{point}: {said}");
        assert!(said.contains("retired"), "{said}");
        let place = dir.path().join("host/.local/state/ferry");
        assert!(place.join(format!("records/retired-{key}.json")).exists());
    }
}

/// A failed append to the seat book is a note; the run is not the book's.
#[test]
fn a_failed_seat_book_append_is_a_note() {
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture();
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
    let dir = fixture();
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
    let caches = dir.path().join(".ferry/local/caches");
    for entry in fs::read_dir(&caches).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".measured-")
        {
            fs::remove_file(path).unwrap();
        }
    }
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
    let slot = fs::read_dir(dir.path().join(".ferry/local/slots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
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
    let dir = fixture();
    fs::write(dir.path().join(".gitignore"), "big/\nconfig/\nstate/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['big']\n[recipe.pass]\ncommand=['sh','-c','if test -f big/blob; then echo warm; else echo cold; mkdir -p big; head -c 4096 /dev/zero > big/blob; fi']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "a preserved path that grows"]);
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1\nseat_cap_bytes = 1000\n",
    );
    run(dir.path(), &["run", "pass", "--local", "--json"]);
    for entry in fs::read_dir(dir.path().join(".ferry/local/caches")).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".measured-")
        {
            fs::remove_file(path).unwrap();
        }
    }
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
    let dir = fixture();
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
    let slot = dir.path().join("host/.local/state/ferry/slots").join(&key);
    assert!(slot.exists(), "{said}");
}

/// A workspace whose parent directory is missing, as under an unmounted
/// volume, is unknown and kept.
#[test]
fn seats_prune_keeps_a_workspace_whose_parent_is_missing() {
    let dir = fixture();
    fs::create_dir(dir.path().join("volume")).unwrap();
    let live = workspace(dir.path(), "volume/live");
    assert_eq!(remote_pass(dir.path(), &live).status.code(), Some(0));
    let key = key_of(dir.path(), &live);
    fs::remove_dir_all(dir.path().join("volume")).unwrap();
    let (code, said) = prune(dir.path(), ".local/bin/ferry");
    assert_eq!(code, Some(0), "{said}");
    assert!(said.contains("kept") && said.contains("unknown"), "{said}");
    let slot = dir.path().join("host/.local/state/ferry/slots").join(&key);
    assert!(slot.exists(), "{said}");
}

/// The cap measures and wipes only what is proven inside the seat: a
/// preserve path behind a symbolic link is neither, and its target survives
/// (slice 5 review P1-a; murail-4a's repro).
#[test]
fn the_cap_never_follows_a_symbolic_link_out_of_the_seat() {
    let dir = fixture();
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
    let slot = fs::read_dir(dir.path().join(".ferry/local/slots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
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
        "recipe": {"command": ["true"]}, "hooks": {}, "caller_binary_sha256": "x",
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
    let dir = fixture();
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
    let dir = fixture();
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
    let slot = dir.path().join("host/.local/state/ferry/slots").join(&key);
    assert!(slot.exists(), "{said}");
}

/// Retire deletes only what is proven inside the place's root: through a
/// symbolic link it refuses, and the link's target survives (review P1-a).
#[test]
fn retire_never_follows_a_symbolic_link_out_of_the_place() {
    let dir = fixture();
    let gone = workspace(dir.path(), "gone");
    assert_eq!(remote_pass(dir.path(), &gone).status.code(), Some(0));
    let key = key_of(dir.path(), &gone);
    fs::remove_dir_all(&gone).unwrap();
    let place = dir.path().join("host/.local/state/ferry");
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(outside.path().join(&key)).unwrap();
    let sentinel = outside.path().join(&key).join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let _ = fs::remove_dir_all(place.join("caches"));
    std::os::unix::fs::symlink(outside.path(), place.join("caches")).unwrap();
    let (code, said) = prune(dir.path(), ".local/bin/ferry");
    assert_eq!(code, Some(125), "{said}");
    assert!(said.contains("symbolic link"), "{said}");
    assert!(sentinel.exists(), "{said}");
}

/// herald-1b's diagnostic (eedc813a): a workspace whose lookup is denied is
/// unknown, not gone, and its seat survives.
#[test]
fn consumer_prune_must_not_treat_permission_denied_as_workspace_gone() {
    let dir = fixture();
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
            .join("host/.local/state/ferry/slots")
            .join(key)
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
    let dir = fixture();
    fs::write(dir.path().join(".gitignore"), "big/\nconfig/\nstate/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['big']\n[recipe.pass]\ncommand=['sh','-c','mkdir -p big/sub; head -c 4096 /dev/zero > big/sub/blob']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "a preserved tree that grows"]);
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1\nseat_cap_bytes = 1000\n",
    );
    run(dir.path(), &["run", "pass", "--local", "--json"]);
    let state = dir.path().join(".ferry/local");
    let slot = fs::read_dir(state.join("slots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let key = slot.file_name().unwrap().to_string_lossy().into_owned();
    fs::remove_file(state.join(format!("caches/.measured-{key}"))).unwrap();
    // The cache's own delete is not held.
    fs::write(state.join(format!("caches/{key}.go")), "").unwrap();
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
    let dir = fixture();
    fs::write(dir.path().join(".gitignore"), "big/\nconfig/\nstate/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['big']\n[recipe.pass]\ncommand=['sh','-c','mkdir -p big/locked big/other; head -c 4096 /dev/zero > big/other/blob']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "a preserved tree"]);
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1\nseat_cap_bytes = 1000\n",
    );
    run(dir.path(), &["run", "pass", "--local", "--json"]);
    let state = dir.path().join(".ferry/local");
    let slot = fs::read_dir(state.join("slots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let key = slot.file_name().unwrap().to_string_lossy().into_owned();
    fs::remove_file(state.join(format!("caches/.measured-{key}"))).unwrap();
    fs::write(slot.join("big/other/keep"), "keep").unwrap();
    // After the preserve path fails, the wipe still reaches the cache.
    let cache = state.join(format!("caches/{key}"));
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
    let dir = fixture();
    fs::write(dir.path().join(".gitignore"), "big/\nconfig/\nstate/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['big']\n[recipe.pass]\ncommand=['sh','-c','mkdir -p big; head -c 4096 /dev/zero > big/blob']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "a preserved tree"]);
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1\nseat_cap_bytes = 1000\n",
    );
    run(dir.path(), &["run", "pass", "--local", "--json"]);
    let state = dir.path().join(".ferry/local");
    let slot = fs::read_dir(state.join("slots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let key = slot.file_name().unwrap().to_string_lossy().into_owned();
    fs::remove_file(state.join(format!("caches/.measured-{key}"))).unwrap();
    let cache = state.join(format!("caches/{key}"));
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
    let dir = fixture();
    fs::write(dir.path().join(".gitignore"), "big/\nconfig/\nstate/\n").unwrap();
    fs::write(dir.path().join("ferry.toml"), "version=1\npreserve=['big']\n[recipe.pass]\ncommand=['sh','-c','mkdir -p big; head -c 4096 /dev/zero > big/blob']\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "ferry.toml"]);
    git(dir.path(), &["commit", "-m", "a preserved tree"]);
    with_host_policy(
        dir.path(),
        "version = 1\nmin_free_bytes = 1\nseat_cap_bytes = 1000\n",
    );
    run(dir.path(), &["run", "pass", "--local", "--json"]);
    let state = dir.path().join(".ferry/local");
    let slot = fs::read_dir(state.join("slots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let key = slot.file_name().unwrap().to_string_lossy().into_owned();
    fs::remove_file(state.join(format!("caches/.measured-{key}"))).unwrap();
    fs::write(state.join(format!("caches/{key}.go")), "").unwrap();
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

/// A place refused for its capabilities receives nothing first: no store and
/// no shipped objects (SPEC §12.11; mirrors were pushed before the check).
#[test]
fn an_old_place_receives_no_objects_before_its_refusal() {
    let dir = fixture();
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
    let dir = fixture();
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

/// A dependency repository at `dir/<name>` with one commit; its hash.
fn dependency(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    fs::create_dir_all(&path).unwrap();
    git(&path, &["init", "-q"]);
    git(&path, &["config", "user.name", "Ferry Test"]);
    git(&path, &["config", "user.email", "ferry@example.invalid"]);
    fs::write(path.join("value"), name).unwrap();
    git(&path, &["add", "value"]);
    git(&path, &["commit", "-qm", name]);
    let out = Command::new("git")
        .current_dir(&path)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A recipe sees its commit and its own declared ship entries, nothing else:
/// another seat's shipped dependency is in none of its branches, refs or
/// served URLs (the critic's l1 on slice 6).
#[test]
fn a_recipe_sees_only_its_own_ship_entries() {
    let dir = fixture();
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
        let dir = fixture();
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
