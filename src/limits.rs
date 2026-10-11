//! Ferry's timing and size bounds in one place, so the code and the tests
//! that check them share one definition (the tests include this file).
use std::time::Duration;

/// How long the session teardown waits for members to leave before KILL.
pub const SESSION_CLEANUP: Duration = Duration::from_secs(5);

/// The collect hook's own bound.
pub const COLLECT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long stopped survivors get between TERM and KILL.
pub const STOP_GRACE: Duration = Duration::from_secs(10);

/// How long survivors sent SIGKILL get to be reaped before the final census.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// How long a session's output writer gets after the session stop.
pub const DRAIN: Duration = Duration::from_secs(2);

/// The most a run's output log holds; later output is read and dropped.
pub const OUTPUT_CAP: u64 = 1 << 30;

/// The default bound on each lock wait (a request may name its own).
pub const WAIT: Duration = Duration::from_mins(30);

/// The timeout of a request that names none, and the most one may name.
/// Transitional until consumers declare timeouts (ferry-8kk.2).
pub const MAX_TIMEOUT: Duration = Duration::from_hours(6);

/// Everything after the recipe: its teardown, the collect hook and its
/// teardown, survivors' stop, the output drains, and a margin. Part of the
/// absolute deadline.
pub const TEARDOWN: Duration = Duration::from_secs(
    SESSION_CLEANUP.as_secs() * 2
        + COLLECT_TIMEOUT.as_secs()
        + STOP_GRACE.as_secs()
        + KILL_GRACE.as_secs()
        + DRAIN.as_secs() * 2
        + 10,
);

/// How long a caller keeps trying a place that does not answer.
pub const UNREACHABLE: Duration = Duration::from_mins(5);
