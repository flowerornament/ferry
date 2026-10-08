//! Shared by the contract tests.
use std::time::{Duration, Instant};

/// When a control stops waiting for something to start or finish and calls
/// it a hang. The wait ends as soon as the thing happens, so a generous
/// budget costs nothing on a fast machine; it only bounds a hang, and keeps
/// a loaded builder (the Darwin Nix check) from failing a correct run.
/// 300 s, scaled by `FERRY_TEST_DEADLINE_SCALE` (for example 0.1 locally).
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

fn scale() -> f64 {
    std::env::var("FERRY_TEST_DEADLINE_SCALE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|scale| *scale > 0.0)
        .unwrap_or(1.0)
}
