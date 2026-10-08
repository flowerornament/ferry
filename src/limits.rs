//! Ferry's timing and size bounds in one place, so the code and the tests
//! that check them share one definition (the tests include this file).
use std::time::Duration;

/// How long the session teardown waits for members to leave before KILL.
pub const SESSION_CLEANUP: Duration = Duration::from_secs(5);

/// The collect hook's own bound.
pub const COLLECT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long one of the host's own events may wait for room in the output
/// channel before it is dropped and counted.
pub const EVENT_BOUND: Duration = Duration::from_secs(5);

/// How long host-run waits for its final events to reach the client.
pub const FINAL_EMIT: Duration = Duration::from_secs(5);

/// How long stopped survivors get between TERM and KILL.
pub const STOP_GRACE: Duration = Duration::from_secs(10);

/// How long survivors sent SIGKILL get to be reaped before the final census.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// How long delivery after the run may see no progress before giving up.
pub const STALL: Duration = Duration::from_secs(15);

/// The most delivery after the run may take, however bytes move.
pub const CAP: Duration = Duration::from_secs(60);

/// The most bytes waiting in the queue.
pub const MAX_QUEUE: usize = 8 << 20;

/// The longest line relayed as one: longer lines are split.
pub const MAX_LINE: usize = 1 << 20;

/// How long readers get to drain what was readable when delivery ended.
pub const DRAIN: Duration = Duration::from_secs(2);
