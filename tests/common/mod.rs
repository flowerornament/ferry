//! Shared by the contract tests.
use std::{
    path::Path,
    process::{Command, Output},
    time::{Duration, Instant},
};

/// Runs git in a test's own repository; never inherits another one's.
#[allow(dead_code)]
pub fn git(root: &Path, args: &[&str]) {
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

/// The receipt in a `--json` run's events.
#[allow(dead_code)]
pub fn receipt(out: &Output) -> serde_json::Value {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["event"] == "receipt")
        .map_or_else(
            || {
                panic!(
                    "no receipt: {} {}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                )
            },
            |value| value["receipt"].clone(),
        )
}

/// The ferry binary under test, in `dir`, with its own configuration.
#[allow(dead_code)]
pub fn ferry(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("FERRY_ID")
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"));
    command
}

/// When a control stops waiting for something to start or finish and calls
/// it a hang. The wait ends as soon as the thing happens, so a generous
/// budget costs nothing on a fast machine; it only bounds a hang, and keeps
/// a loaded builder (the Darwin Nix check) from failing a correct run.
/// 300 s, scaled by `FERRY_TEST_DEADLINE_SCALE` (for example 0.1 locally).
#[allow(dead_code)]
pub fn startup_deadline() -> Instant {
    Instant::now() + Duration::from_secs_f64(300.0 * scale())
}

/// Scheduling slack added to a promptness bound that the design derives
/// from its constants: what a loaded machine adds on top. 30 s, scaled by
/// `FERRY_TEST_DEADLINE_SCALE`. A hang still exceeds it.
#[allow(dead_code)]
pub fn slack() -> Duration {
    Duration::from_secs_f64(30.0 * scale())
}

#[allow(dead_code)]
fn scale() -> f64 {
    std::env::var("FERRY_TEST_DEADLINE_SCALE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|scale| *scale > 0.0)
        .unwrap_or(1.0)
}
