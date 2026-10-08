//! The supervisor's one output writer. Data never controls (Rule A), the
//! supervisor never blocks on data (Rule C), and every line the recipe
//! wrote is either delivered or counted (Rule D):
//!
//! - session reader threads hand each output line to a bounded channel and
//!   wait while it is full, so a slow client slows the recipe, never the
//!   supervisor loop;
//! - one writer thread writes to stdout. If a write fails, or the client
//!   makes no progress for a stall bound, it stops writing; every line not
//!   written is counted as dropped. That never stops the run (client loss
//!   is learned from stdin, not from stdout);
//! - delivery after the run is bounded twice: a stall (no progress) and a
//!   total cap, and a stop ends it. Termination wins over completeness: a
//!   consumer slower than the stall bound gets drops, counted, and the
//!   host's output.log keeps every line;
//! - the supervisor's own events are always attempted, whatever became of
//!   the output, within a bounded wait for room. The record on disk is the
//!   truth;
//! - the queue is bounded in bytes as well as lines.
use std::{
    io::Write,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

pub use crate::limits::CAP;
pub use crate::limits::MAX_LINE;
pub use crate::limits::MAX_QUEUE;
pub use crate::limits::STALL;

enum Message {
    /// A line of recipe output (counted), or one of the host's events.
    Line { bytes: Vec<u8>, output: bool },
    /// Answered once every message queued before it is written or dropped.
    Mark(Sender<()>),
}

static WRITER: OnceLock<SyncSender<Message>> = OnceLock::new();
/// Set once the client cannot or will not take more output: later output
/// lines are dropped (events are still attempted).
static DEAD: AtomicBool = AtomicBool::new(false);
/// Bytes waiting in the queue.
static QUEUED_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Output lines readers read, and messages queued.
static OFFERED: AtomicU64 = AtomicU64::new(0);
static QUEUED: AtomicU64 = AtomicU64::new(0);
/// Messages the writer has finished with, written or not.
static PROCESSED: AtomicU64 = AtomicU64::new(0);
/// Bytes readers have read: progress on the recipe's side.
static READ: AtomicU64 = AtomicU64::new(0);
/// Readers that have not reached EOF.
static OPEN: AtomicUsize = AtomicUsize::new(0);
/// Set once delivery has ended: readers drain what is already readable,
/// log and count it without relaying it, and stop.
static ENDED: AtomicBool = AtomicBool::new(false);
/// Closed after the drain: readers log and count nothing more, so the count
/// taken after it is final. Readers hold it shared while they log and
/// count one line.
static CLOSED: std::sync::RwLock<bool> = std::sync::RwLock::new(false);
/// Lines read whose output-log write failed: counted, not logged.
static LOG_ERRORS: AtomicU64 = AtomicU64::new(0);
pub use crate::limits::DRAIN;

fn sender() -> &'static SyncSender<Message> {
    WRITER.get_or_init(|| {
        if crate::execution::fail_at("output-write") {
            DEAD.store(true, Ordering::SeqCst);
        }
        let (sender, receiver) = sync_channel(1024);
        thread::spawn(move || write_all(&receiver));
        sender
    })
}

fn write_all(receiver: &Receiver<Message>) {
    for message in receiver {
        match message {
            Message::Line { bytes, output } => {
                QUEUED_BYTES.fetch_sub(bytes.len(), Ordering::SeqCst);
                if !(output && DEAD.load(Ordering::SeqCst)) {
                    let mut out = std::io::stdout().lock();
                    let written = out
                        .write_all(&bytes)
                        .and_then(|()| out.write_all(b"\n"))
                        .and_then(|()| out.flush());
                    if written.is_err() {
                        DEAD.store(true, Ordering::SeqCst);
                    }
                }
                PROCESSED.fetch_add(1, Ordering::SeqCst);
            }
            Message::Mark(reply) => {
                let _ = reply.send(());
            }
        }
    }
}

/// A reader thread starts: it counts as open until `reader_done`.
pub fn reader_open() {
    OPEN.fetch_add(1, Ordering::SeqCst);
}

/// A reader thread reached EOF.
pub fn reader_done() {
    OPEN.fetch_sub(1, Ordering::SeqCst);
}

/// Notes `bytes` read by a reader, as progress.
pub fn read(bytes: usize) {
    READ.fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::SeqCst);
}

/// What a reader does with a line it read.
#[derive(Debug, PartialEq, Eq)]
pub enum Admit {
    /// Logged and counted; relay it.
    Relay,
    /// Logged and counted while draining after delivery ended; not relayed.
    Drained,
    /// The count is final: stop, logging nothing more.
    Stop,
}

/// Whether delivery has ended (readers drain and stop).
pub fn ended() -> bool {
    ENDED.load(Ordering::SeqCst)
}

/// Admits one line a reader read: runs `log` (the output log write, which
/// says whether it succeeded) and counts the line, as one step, unless the
/// count is final. A failed log write is counted in `log_errors`.
pub fn admit(log: impl FnOnce() -> bool) -> Admit {
    let Ok(closed) = CLOSED.read() else {
        return Admit::Stop;
    };
    if *closed {
        return Admit::Stop;
    }
    if !log() {
        LOG_ERRORS.fetch_add(1, Ordering::SeqCst);
    }
    OFFERED.fetch_add(1, Ordering::SeqCst);
    if ended() {
        Admit::Drained
    } else {
        Admit::Relay
    }
}

/// Hands one admitted output line to the writer, waiting while the channel
/// is full: backpressure reaches the recipe, not the supervisor. Once the
/// writer has given up, the line is not queued: it is dropped.
pub fn line(bytes: Vec<u8>) {
    let size = bytes.len();
    let mut message = Message::Line {
        bytes,
        output: true,
    };
    loop {
        if DEAD.load(Ordering::SeqCst) {
            return;
        }
        // Room in bytes as well as lines (always room for one line).
        let queued = QUEUED_BYTES.load(Ordering::SeqCst);
        if queued > 0 && queued + size > MAX_QUEUE {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        QUEUED_BYTES.fetch_add(size, Ordering::SeqCst);
        match sender().try_send(message) {
            Ok(()) => {
                QUEUED.fetch_add(1, Ordering::SeqCst);
                return;
            }
            Err(TrySendError::Disconnected(_)) => {
                QUEUED_BYTES.fetch_sub(size, Ordering::SeqCst);
                return;
            }
            Err(TrySendError::Full(back)) => {
                QUEUED_BYTES.fetch_sub(size, Ordering::SeqCst);
                message = back;
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// Sends a message, waiting at most `bound` for room.
fn offer(message: Message, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    let mut message = message;
    loop {
        match sender().try_send(message) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(back)) => {
                if Instant::now() >= deadline {
                    return false;
                }
                message = back;
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// Queues one of the supervisor's own events, waiting at most `bound` for
/// room; an event that finds none is not sent.
pub fn event(value: &serde_json::Value, bound: Duration) {
    let bytes = value.to_string().into_bytes();
    let size = bytes.len();
    QUEUED_BYTES.fetch_add(size, Ordering::SeqCst);
    let message = Message::Line {
        bytes,
        output: false,
    };
    if offer(message, bound) {
        QUEUED.fetch_add(1, Ordering::SeqCst);
    } else {
        QUEUED_BYTES.fetch_sub(size, Ordering::SeqCst);
    }
}

/// Waits while `pending` holds and progress (bytes read, messages
/// processed) keeps moving; false once nothing moved for `stall`, the
/// `deadline` passed, or `stop` was set.
fn wait_with_progress(
    stall: Duration,
    deadline: Instant,
    stop: &AtomicBool,
    pending: impl Fn() -> bool,
) -> bool {
    let progress = || READ.load(Ordering::SeqCst) + PROCESSED.load(Ordering::SeqCst);
    let mut last = progress();
    let mut moved = Instant::now();
    while pending() {
        if stop.load(Ordering::SeqCst) || Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
        let now = progress();
        if now != last {
            last = now;
            moved = Instant::now();
        } else if moved.elapsed() >= stall {
            return false;
        }
    }
    true
}

/// Called after custody release: waits for every reader to reach EOF and
/// the writer to deliver what they queued, while bytes move, for at most
/// `cap` in all, and not past a stop. Then the writer stops taking output
/// and readers drop the rest. Bytes from a survivor never extend it past
/// the cap. False if output was cut short.
pub fn settle(stall: Duration, cap: Duration, stop: &AtomicBool) -> bool {
    let deadline = Instant::now() + cap;
    let read_all = wait_with_progress(stall, deadline, stop, || OPEN.load(Ordering::SeqCst) > 0);
    let delivered = read_all
        && wait_with_progress(stall, deadline, stop, || {
            PROCESSED.load(Ordering::SeqCst) < QUEUED.load(Ordering::SeqCst)
        });
    if !delivered {
        DEAD.store(true, Ordering::SeqCst);
    }
    // Readers still open (a survivor holds the pipe) drain what is already
    // readable into the log, without relaying it, and stop. Then the gate
    // closes: nothing more is logged or counted, so lines() is final.
    ENDED.store(true, Ordering::SeqCst);
    let drained = Instant::now() + DRAIN;
    while OPEN.load(Ordering::SeqCst) > 0 && Instant::now() < drained {
        thread::sleep(Duration::from_millis(10));
    }
    if let Ok(mut closed) = CLOSED.write() {
        *closed = true;
    }
    read_all && delivered
}

/// Waits at most `bound` until everything queued so far is written or
/// dropped. False if the bound passed first.
pub fn flush(bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    let (reply, answered) = channel();
    if !offer(Message::Mark(reply), bound) {
        return false;
    }
    answered
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .is_ok()
}

/// Lines of recipe output read: the lines in the output log plus
/// `log_errors`; final once `settle` has returned. The caller counts what it received, so each
/// line is counted exactly once: what was not received was dropped.
pub fn lines() -> u64 {
    OFFERED.load(Ordering::SeqCst)
}

/// Lines read and counted whose output-log write failed.
pub fn log_errors() -> u64 {
    LOG_ERRORS.load(Ordering::SeqCst)
}
