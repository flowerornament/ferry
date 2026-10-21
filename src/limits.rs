//! Ferry's timing and size bounds in one place, so the code and the tests
//! that check them share one definition (the tests include this file).
use std::time::Duration;

/// The collect hook's own bound.
pub const COLLECT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a stopped session's members get between TERM and KILL (SPEC §7).
pub const STOP_GRACE: Duration = Duration::from_secs(10);

/// How long a session's members get to be gone after KILL, before the ones
/// left are recorded as survivors.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// How long after a run's timeout or deadline its session owner stops the
/// session itself (ferry-8d6): late enough that a live supervisor's stop
/// normally comes first, so each member normally gets one TERM. An owner
/// descheduled past it overlaps that stop; neither writes a result.
pub const OWNER_LAG: Duration = Duration::from_secs(1);

/// How long a session's output writer gets after the session stop.
pub const DRAIN: Duration = Duration::from_secs(2);

/// The marker pass's bound (SPEC §7): past it, or past the run's deadline,
/// the survivors are `unknown` and the pass is abandoned.
pub const MARKER_BOUND: Duration = Duration::from_secs(10);

/// How often a caller's runs prune a place's idle seats by themselves, at
/// most (SPEC §9).
pub const AUTO_PRUNE_EVERY: Duration = Duration::from_hours(1);

/// After this, a prune a run makes by itself starts no new retire.
pub const AUTO_PRUNE_BUDGET: Duration = Duration::from_secs(10);

/// After this, a run's history pass (SPEC §9) starts no new listing, read
/// or delete: what is left waits for the next run.
pub const HISTORY_BUDGET: Duration = Duration::from_secs(1);

/// The most a run's output log holds; later output is read and dropped.
pub const OUTPUT_CAP: u64 = 1 << 30;

/// The default bound on each lock wait (a request may name its own).
pub const WAIT: Duration = Duration::from_mins(30);

/// The timeout of a request that names none, and the most one may name.
/// Transitional until consumers declare timeouts (ferry-8kk.2).
pub const MAX_TIMEOUT: Duration = Duration::from_hours(6);

/// Everything after the recipe: the stop of its session and of the collect
/// hook's, the collect hook, the output drains, the marker pass, and a
/// margin. Part of the
/// absolute deadline.
pub const TEARDOWN: Duration = Duration::from_secs(
    (STOP_GRACE.as_secs() + KILL_GRACE.as_secs()) * 2
        + COLLECT_TIMEOUT.as_secs()
        + DRAIN.as_secs() * 2
        + MARKER_BOUND.as_secs()
        + 10,
);

/// How long a caller waits for a short reply from a place before it submits
/// (`host-info`, a file read): a stalled SSH connect or jump host is refused
/// instead of holding the caller (PRINCIPLES 5).
pub const SSH_REPLY: Duration = Duration::from_secs(60);

/// How long the caller's object push may go without progress output before
/// it counts as stalled (SPEC §6): no wall-clock bound, so a large push that
/// keeps moving is never cut.
pub const PUSH_STALL: Duration = Duration::from_secs(60);

/// How long a caller keeps trying a place that does not answer.
pub const UNREACHABLE: Duration = Duration::from_mins(5);

/// How often a caller reads a quiet run's wait note (SPEC §4): with the
/// note written at 3 s of waiting, it is said within 5 s of being written.
pub const NARRATION: Duration = Duration::from_secs(2);
