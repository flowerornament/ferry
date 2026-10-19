//! Real CLI contracts exercise committed bytes and durable refusal receipts.
mod common;
#[path = "../src/limits.rs"]
#[allow(dead_code)]
mod limits;

use common::{git, receipt};
use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::{Pid, getsid},
};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::BTreeMap,
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

/// The local route's one seat checkout (SPEC §9: `seats/<key>/checkout`).
fn only_checkout(repo: &Path) -> std::path::PathBuf {
    fs::read_dir(repo.join(".ferry/local/v1/seats"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("checkout")
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
    command(root).args(args).output().unwrap()
}
fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"));
    command
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
        let dir = fixture();
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
    let dir = fixture();
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

/// A process a control started (through a recipe), killed by its exact pid
/// when the control ends; its parent is gone, so nothing reaps it here.
struct OwnedPid(i32);
impl Drop for OwnedPid {
    fn drop(&mut self) {
        let _ = kill(Pid::from_raw(self.0), Signal::SIGKILL);
    }
}

/// A member of a session whose owner may be gone, killed when the control
/// ends while it is still in that session.
struct OwnedMember {
    pid: i32,
    session: i32,
}
impl Drop for OwnedMember {
    fn drop(&mut self) {
        if getsid(Some(Pid::from_raw(self.pid))).is_ok_and(|sid| sid.as_raw() == self.session) {
            let _ = kill(Pid::from_raw(self.pid), Signal::SIGKILL);
        }
    }
}

/// An unsupervised session (ferry-8d6): runs `pass` locally with `script`
/// as its `sh -c` recipe, which must write its pid to `$DIR/recipe`, waits
/// for that, then SIGKILLs the run's supervisor. Returns the run id, the
/// owner's pid (the session's id) and the guards on the recipe and caller.
fn unsupervised(
    dir: &Path,
    script: &str,
    fail_at: &str,
    timeout: &str,
) -> (String, i32, i32, OwnedMember, OwnedCaller) {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let marker = dir.join("recipe");
    fs::write(
        dir.join("ferry.toml"),
        format!(
            "version=1\n[recipe.pass]\ncommand=['sh','-c','{}']\n",
            script.replace("$DIR", &dir.display().to_string())
        ),
    )
    .unwrap();
    git(dir, &["add", "ferry.toml"]);
    git(dir, &["commit", "-m", "unsupervised recipe"]);
    let caller_output = dir.join("caller-stdout");
    let child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("FERRY_FAIL_AT", fail_at)
        .args([
            "run",
            "pass",
            "--local",
            "--json",
            "--timeout-seconds",
            timeout,
        ])
        .stdout(fs::File::create(&caller_output).unwrap())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut caller = OwnedCaller {
        child: Some(child),
        member: marker.clone(),
    };
    let deadline = common::startup_deadline();
    while !marker.exists() {
        // A caller that ended (a refusal) or a startup past the bound: say
        // what the caller printed.
        let ended = caller.child.as_mut().unwrap().try_wait().unwrap().is_some();
        assert!(
            Instant::now() < deadline && !ended,
            "recipe did not start (caller ended: {ended}): {}",
            fs::read_to_string(&caller_output).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(10));
    }
    let recipe: i32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    let owner = getsid(Some(Pid::from_raw(recipe))).unwrap().as_raw();
    let guard = OwnedMember {
        pid: recipe,
        session: owner,
    };
    let submitted = fs::read_to_string(&caller_output)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["event"] == "submitted")
        .expect("the caller prints its submitted event");
    let supervisor = i32::try_from(submitted["pid"].as_i64().unwrap()).unwrap();
    kill(Pid::from_raw(supervisor), Signal::SIGKILL).unwrap();
    caller.child.as_mut().unwrap().wait().unwrap();
    let id = submitted["id"].as_str().unwrap().to_owned();
    (id, owner, recipe, guard, caller)
}

fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// Waits for every pid to be gone, within `bound` from `since`.
fn gone_within(pids: &[i32], since: std::time::Instant, bound: std::time::Duration, what: &str) {
    while pids.iter().any(|pid| alive(*pid)) {
        assert!(since.elapsed() < bound, "{what}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// ferry-8d6 (PRINCIPLES 2): with its supervisor killed (`SIGKILL`), the
/// session owner itself stops the session at the run's end: the recipe, the
/// process it started and the owner are gone soon after the timeout, not
/// when they exit, and the record reconciles to lost.
#[test]
fn a_killed_supervisor_s_session_is_stopped_at_its_timeout() {
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    assert!(!dir.path().join(".ferry/local/v1").exists());
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
    // a file `fail-record-once` fails the next `place record`, once;
    // FERRY_TEST_SLOW_INFO names a file: while it exists, host-info first
    // sleeps the seconds it holds, then answers;
    // FERRY_TEST_HANG_INFO names a file: host-info writes its pid there and
    // hangs, as a stalled SSH connect or jump host; FERRY_TEST_HOLD_PIPE
    // names a file: host-info leaves a descendant holding its pipes (its
    // pid written there) and exits at once.
    fs::write(tools.join("ssh"), "#!/bin/sh\nfor last do :; done\ncase \"$last\" in *git-receive-pack*) if [ -n \"$FERRY_TEST_SLOW_PUSH\" ]; then touch \"$FERRY_TEST_SLOW_PUSH\"; sleep 3; fi;; *\" place \"*\"follow\"*) if [ -e \"$FERRY_TEST_HOST/unreachable\" ]; then exit 255; fi;; *\" place \"*\"record\"*) if [ -e \"$FERRY_TEST_HOST/fail-record-once\" ]; then rm -f \"$FERRY_TEST_HOST/fail-record-once\"; exit 255; fi;; *host-info*) if [ -n \"$FERRY_TEST_SLOW_INFO\" ] && [ -e \"$FERRY_TEST_SLOW_INFO\" ]; then sleep \"$(cat \"$FERRY_TEST_SLOW_INFO\")\"; fi; if [ -n \"$FERRY_TEST_HANG_INFO\" ]; then echo $$ > \"$FERRY_TEST_HANG_INFO\"; exec sleep 600; fi; if [ -n \"$FERRY_TEST_HOLD_PIPE\" ]; then sleep 600 & echo $! > \"$FERRY_TEST_HOLD_PIPE\"; exit 255; fi;; esac\ncd \"$FERRY_TEST_HOST\" || exit 125\nexport HOME=\"$FERRY_TEST_HOST\"\nexec sh -c \"$last\"\n").unwrap();
    fs::set_permissions(tools.join("ssh"), fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.join("config/ferry");
    fs::create_dir_all(&config).unwrap();
    // Top-level keys a control sets (`machines-top.toml`) come first.
    let top = fs::read_to_string(dir.join("machines-top.toml")).unwrap_or_default();
    fs::write(
        config.join("machines.toml"),
        format!(
            "{top}[host.test-host]\nssh='test-host'\nbinary='{binary}'\ntriple='{}-{}'\ncores=1\nfeatures=[]\n",
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

/// Every wait is bounded (PRINCIPLES 5, ferry-c72): a place whose SSH
/// stalls before submit (here host-info never answers) is refused within
/// the reply bound, final 125 with nothing run, and no receipt claims a run.
#[test]
fn a_stalled_place_before_submit_is_refused_within_its_bound() {
    use std::time::{Duration, Instant};
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture();
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

/// The reply bound covers the pipes too: an ssh that exits while a
/// descendant still holds its stdout and stderr (a `ProxyCommand`, a helper)
/// is refused within the bound, never waited on to the descendant's end.
#[test]
fn an_ssh_whose_pipes_outlive_it_is_refused_within_its_bound() {
    use std::time::{Duration, Instant};
    if !cfg!(debug_assertions) {
        return;
    }
    let dir = fixture();
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
    let dir = fixture();
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
    assert!(
        !dir.path()
            .join("host/.local/state/ferry/v1/records")
            .exists()
    );
}

#[test]
fn a_host_refusal_before_acceptance_is_refused_not_lost() {
    let dir = fixture();
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
    assert!(dir.join(".ferry/local/v1/locks/host").exists());
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
    let dir = fixture();
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
    let dir = fixture();
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

/// Makes `key` look last used `ago` ms back: on the caller's book (every
/// line of that key) and, unless `book_only`, on the place's `used` record.
fn age(dir: &Path, key: &str, ago: u128, book_only: bool) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let book = dir.join("state/ferry/seats.jsonl");
    let lines: Vec<String> = fs::read_to_string(&book)
        .unwrap()
        .lines()
        .map(|line| {
            let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
            if value["key"] == key {
                value["last_used_ms"] = serde_json::json!(now - ago);
            }
            value.to_string()
        })
        .collect();
    fs::write(&book, lines.join("\n") + "\n").unwrap();
    if !book_only {
        let used = dir
            .join("host/.local/state/ferry/v1/seats")
            .join(key)
            .join("used");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&used).unwrap()).unwrap();
        value["ms"] = serde_json::json!(now - ago);
        fs::write(&used, value.to_string()).unwrap();
    }
}

fn prune_with(dir: &Path, args: &[&str]) -> (Option<i32>, String) {
    let mut all = vec!["seats", "prune"];
    all.extend_from_slice(args);
    let out = remote_command(dir, ".local/bin/ferry", &all)
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

const DAY_MS: u128 = 86_400_000;

/// SPEC §12 item 20: `prune --idle` retires a seat idle on both clocks
/// (this caller's book and the place's `used`), keeps one used since by
/// either, keeps one whose age is unknown, skips a busy one, and reports
/// each; records and run folders stay. Item 21: a book that predates a run
/// on the seat (the place's `used` is newer) is refused `used`.
#[test]
fn seats_prune_idle_retires_only_what_is_idle_on_both_clocks() {
    let dir = fixture();
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
        let dir = fixture();
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

/// Waits for run `id`'s `--once` retire to be logged by its supervisor,
/// and returns that line.
fn once_logged(dir: &Path, id: &str) -> String {
    let log = dir
        .join("host/.local/state/ferry/v1/runs")
        .join(id)
        .join("supervisor.log");
    let until = common::startup_deadline();
    loop {
        if let Some(line) = fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .find(|line| line.starts_with("ferry: --once retire"))
        {
            return line.to_owned();
        }
        assert!(
            std::time::Instant::now() < until,
            "the --once retire never ran"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The host record of run `id` on the test place.
fn host_record(dir: &Path, id: &str) -> serde_json::Value {
    let path = dir
        .join("host/.local/state/ferry/v1/records")
        .join(format!("{id}.json"));
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// SPEC §12 item 19: a `--once` run ends with one final record saying
/// `once`, the seat retired and recorded, and no `seats/<key>`. With the
/// retire failing before the rename, the record is the same and final, the
/// seat stays, `status` lists it, and a later `prune --idle 0s` retires it.
#[test]
fn a_once_run_retires_its_seat_after_its_final_record() {
    let dir = fixture();
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
    let dir = fixture();
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
        let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
        let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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

/// M1 keeps its state in `v1` inside the old root, which pre-M1 binaries
/// never look into, and touches nothing of an old root it finds: old and
/// new never share a state root (SPEC §6, §9, §12.11).
#[test]
fn an_old_root_is_never_shared() {
    let dir = fixture();
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

/// ferry-8wq: the local place's state root.
fn local_root(dir: &Path) -> std::path::PathBuf {
    dir.join(".ferry/local/v1")
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

const HISTORY_DAY: &str = "version = 1\nmin_free_bytes = 1\ndelete_run_history_after_days = 1\n";

/// Plants run `id`'s history on the place at `root`, submitted `ago` ms
/// before now: its folder (a log and a nested file) and its record, final
/// or (`reserved`) a reservation, with `extra` fields over the defaults.
/// Returns the folder.
fn plant(
    root: &Path,
    id: &str,
    ago: u128,
    reserved: bool,
    extra: &serde_json::Value,
) -> std::path::PathBuf {
    let folder = root.join("runs").join(id);
    fs::create_dir_all(folder.join("artifacts/deep")).unwrap();
    fs::write(folder.join("output.log"), "old output\n").unwrap();
    fs::write(folder.join("artifacts/deep/file"), "old artifact\n").unwrap();
    let mut record = serde_json::json!({
        "state": if reserved { "reserved" } else { "final" },
        "id": id,
        "token": "t",
        "commit": "0000000000000000000000000000000000000000",
        "seat": "planted",
        "binary_sha256": "b",
        "supervisor": {"pid": 1, "start": null},
        "boot_id": null,
        "submitted_ms": now_ms() - ago,
    });
    if !reserved {
        record["outcome"] = "ran-pass".into();
        record["exit_code"] = 0.into();
    }
    for (key, value) in extra.as_object().unwrap() {
        record[key] = value.clone();
    }
    let name = if reserved {
        format!("{id}.reserved.json")
    } else {
        format!("{id}.json")
    };
    fs::create_dir_all(root.join("records")).unwrap();
    fs::write(root.join("records").join(name), record.to_string()).unwrap();
    folder
}

/// Sets `path`'s modification time to `ago` ms before now.
fn age_file(path: &Path, ago: u128) {
    let seconds = (now_ms() - ago) / 1000;
    let set = Command::new("python3")
        .args([
            "-c",
            "import os,sys; t=int(sys.argv[2]); os.utime(sys.argv[1], (t, t))",
            path.to_str().unwrap(),
            &seconds.to_string(),
        ])
        .status()
        .unwrap();
    assert!(set.success());
}

/// Waits for run `id`'s history pass to be logged by its supervisor, and
/// returns that line.
fn history_logged(root: &Path, id: &str) -> String {
    let log = root.join("runs").join(id).join("supervisor.log");
    let until = common::startup_deadline();
    loop {
        if let Some(line) = fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .find(|line| line.starts_with("ferry: run history:"))
        {
            return line.to_owned();
        }
        assert!(
            std::time::Instant::now() < until,
            "the history pass never ran"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Waits until run `id`'s supervisor has exited, so whatever it would do
/// after its record is done.
fn supervisor_exited(root: &Path, id: &str) {
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(format!("records/{id}.json"))).unwrap())
            .unwrap();
    let pid = i32::try_from(record["supervisor"]["pid"].as_u64().unwrap()).unwrap();
    let until = common::startup_deadline();
    while alive(pid) {
        assert!(std::time::Instant::now() < until, "the supervisor stayed");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The account run `id`'s history pass wrote, if it wrote one.
fn history_account(root: &Path, id: &str) -> Option<serde_json::Value> {
    let suffix = format!("-{id}.json");
    fs::read_dir(root.join("records"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("history-") && name.ends_with(&suffix))
        .map(|name| {
            serde_json::from_slice(&fs::read(root.join("records").join(name)).unwrap()).unwrap()
        })
}

/// The history passes from run `id`'s on: while the last one stopped at
/// its budget (a loaded host), the next run's pass goes on, as SPEC §9
/// promises. Each pass's run and line, and the runs their accounts deleted
/// in all.
fn settled(dir: &Path, root: &Path, id: &str) -> (Vec<(String, String)>, u64) {
    let mut passes = vec![(id.to_owned(), history_logged(root, id))];
    while passes.last().unwrap().1.contains("stopped at its") {
        assert!(passes.len() < 20, "{passes:?}");
        let out = run(dir, &["run", "pass", "--local", "--json"]);
        let next = receipt(&out)["id"].as_str().unwrap().to_owned();
        let line = history_logged(root, &next);
        passes.push((next, line));
    }
    let deleted = passes
        .iter()
        .filter_map(|(id, _)| history_account(root, id))
        .map(|account| account["deleted"].as_u64().unwrap())
        .sum();
    (passes, deleted)
}

/// How many entries a pass's line says it read.
fn read_of(line: &str) -> u64 {
    let (_, after) = line.split_once("read ").unwrap();
    after.split(',').next().unwrap().parse().unwrap()
}

/// A pid that was a process of this test and is gone.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
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
    let dir = fixture();
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

/// Plants the records of `records/` that are not a run's, for item 22: a
/// two-day-old and a fresh retirement record, one without `retired_ms`, a
/// two-day-old pass's account, and a two-day-old and a fresh temporary file
/// of a crashed write. Returns the old ones' time.
fn plant_other_records(records: &Path) -> u128 {
    let then = now_ms() - 2 * DAY_MS;
    fs::write(
        records.join("retired-oldkey.json"),
        serde_json::json!({"key": "oldkey", "retired_ms": then}).to_string(),
    )
    .unwrap();
    fs::write(
        records.join("retired-newkey.json"),
        serde_json::json!({"key": "newkey", "retired_ms": now_ms()}).to_string(),
    )
    .unwrap();
    fs::write(
        records.join("retired-ageless.json"),
        serde_json::json!({"key": "ageless"}).to_string(),
    )
    .unwrap();
    fs::write(
        records.join(format!("history-{then}-earlier.json")),
        serde_json::json!({"ms": then, "run": "earlier", "deleted": 1, "older_than_days": 1})
            .to_string(),
    )
    .unwrap();
    let crashed = records.join(".old-final.json.1.0.tmp");
    fs::write(&crashed, "half a record").unwrap();
    age_file(&crashed, 2 * DAY_MS);
    let fresh_tmp = records.join(".recent-final.json.1.0.tmp");
    fs::write(&fresh_tmp, "half a record").unwrap();
    then
}

fn host_record_at(root: &Path, id: &str) -> serde_json::Value {
    serde_json::from_slice(&fs::read(root.join(format!("records/{id}.json"))).unwrap()).unwrap()
}

/// SPEC §12 item 22: without the key nothing is deleted; `0` or `1.5`
/// refuses the run as a bad `host.toml`, naming the key.
#[test]
fn delete_run_history_after_days_is_whole_days_from_one_and_absent_keeps_all() {
    for (days, refused) in [(None, false), (Some("0"), true), (Some("1.5"), true)] {
        let dir = fixture();
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

/// This process's start time as the place records it on Linux (clock ticks
/// since boot); `None` elsewhere.
fn own_start() -> Option<u64> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// SPEC §12 item 22: an old final run whose recorded survivor is still
/// alive is kept, with or without a recorded start (the start only where
/// the test can read it: Linux); one whose pid lives with another start,
/// whose survivor is gone, or whose survivors are `unknown` is deleted.
#[test]
fn an_old_run_with_a_live_survivor_is_kept() {
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
    let dir = fixture();
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
