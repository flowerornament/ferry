//! A marked process that cannot be read (SPEC §7, §12.16a). Its own test
//! binary: cargo runs binaries one at a time, so its unreadable process can
//! never make a parallel test's marker pass `unknown`. Linux, where the pass
//! is otherwise exact.
#![cfg(target_os = "linux")]
mod common;
use common::{ferry, git, receipt};
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use std::fs;

/// A non-dumpable process (ssh-agent makes itself so) that the recipe left
/// running, carrying the marker: the pass is `unknown`, never a list
/// without it.
#[test]
fn a_marked_process_that_cannot_be_read_makes_the_pass_unknown() {
    let scratch = tempfile::tempdir().unwrap();
    let pidfile = scratch.path().join("hidden.pid");
    let script = format!(
        "python3 -c 'import ctypes,os,time\nos.setsid()\nos.closerange(3, 4096)\nctypes.CDLL(None).prctl(4, 0, 0, 0, 0)\nopen(\"{p}.tmp\",\"w\").write(str(os.getpid()))\nos.rename(\"{p}.tmp\",\"{p}\")\ntime.sleep(60)' </dev/null >/dev/null 2>&1 & while [ ! -e {p} ]; do sleep 0.05; done",
        p = pidfile.display()
    );
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init"]);
    git(d, &["config", "user.name", "Ferry Test"]);
    git(d, &["config", "user.email", "ferry@example.invalid"]);
    let recipes = toml::to_string(&serde_json::json!({"recipe": {
        "probe": {"command": ["sh", "-c", script]},
    }}))
    .unwrap();
    fs::write(d.join("ferry.toml"), format!("version=1\n{recipes}")).unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-m", "fixture"]);
    let out = ferry(d)
        .args(["run", "probe", "--local", "--json"])
        .output()
        .unwrap();
    if let Some(pid) = fs::read_to_string(&pidfile)
        .ok()
        .and_then(|p| p.trim().parse().ok())
    {
        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
    let record = receipt(&out);
    assert_eq!(record["outcome"], "ran-pass", "{record}");
    assert_eq!(record["custody"]["survivors"], "unknown", "{record}");
    assert!(
        record["custody"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("unreadable")),
        "{record}"
    );
}
