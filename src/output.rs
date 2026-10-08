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
//! - waits after the run are bounded by a stall (no progress), not by total
//!   time, so a slow client still gets everything;
//! - the supervisor's own events wait a bounded time for room, then are
//!   dropped. The record on disk is the truth.
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

/// How long a wait after the run may see no progress before giving up.
pub const STALL: Duration = Duration::from_secs(5);
/// The longest line relayed as one: longer lines are split.
pub const MAX_LINE: usize = 1 << 20;

enum Message {
    /// A line of recipe output (counted), or one of the host's events.
    Line { bytes: Vec<u8>, output: bool },
    /// Answered once every message queued before it is written or dropped.
    Mark(Sender<()>),
}

static WRITER: OnceLock<SyncSender<Message>> = OnceLock::new();
/// Set once the client cannot or will not take more: later lines are dropped.
static DEAD: AtomicBool = AtomicBool::new(false);
/// Output lines readers read, queued, and written to the client.
static OFFERED: AtomicU64 = AtomicU64::new(0);
static QUEUED: AtomicU64 = AtomicU64::new(0);
static WRITTEN: AtomicU64 = AtomicU64::new(0);
/// Messages the writer has finished with, written or not.
static PROCESSED: AtomicU64 = AtomicU64::new(0);
/// Bytes readers have read: progress on the recipe's side.
static READ: AtomicU64 = AtomicU64::new(0);
/// Readers that have not reached EOF.
static OPEN: AtomicUsize = AtomicUsize::new(0);

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
                if !DEAD.load(Ordering::SeqCst) {
                    let mut out = std::io::stdout().lock();
                    let written = out
                        .write_all(&bytes)
                        .and_then(|()| out.write_all(b"\n"))
                        .and_then(|()| out.flush());
                    match written {
                        Ok(()) if output => {
                            WRITTEN.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(()) => {}
                        Err(_) => DEAD.store(true, Ordering::SeqCst),
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

/// Hands one output line from a reader to the writer, waiting while the
/// channel is full: backpressure reaches the recipe, not the supervisor.
/// Once the writer has given up, the line is not queued: it is dropped.
pub fn line(bytes: Vec<u8>) {
    OFFERED.fetch_add(1, Ordering::SeqCst);
    let mut message = Message::Line {
        bytes,
        output: true,
    };
    loop {
        if DEAD.load(Ordering::SeqCst) {
            return;
        }
        match sender().try_send(message) {
            Ok(()) => {
                QUEUED.fetch_add(1, Ordering::SeqCst);
                return;
            }
            Err(TrySendError::Disconnected(_)) => return,
            Err(TrySendError::Full(back)) => {
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
    let message = Message::Line {
        bytes: value.to_string().into_bytes(),
        output: false,
    };
    if offer(message, bound) {
        QUEUED.fetch_add(1, Ordering::SeqCst);
    }
}

/// Waits while `pending` holds and progress (bytes read, messages
/// processed) keeps moving; false once nothing moved for `stall`.
fn wait_with_progress(stall: Duration, pending: impl Fn() -> bool) -> bool {
    let progress = || READ.load(Ordering::SeqCst) + PROCESSED.load(Ordering::SeqCst);
    let mut last = progress();
    let mut moved = Instant::now();
    while pending() {
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
/// the writer to deliver everything they queued, for as long as bytes keep
/// moving. On a stall the writer gives up, so blocked readers drop the
/// rest and can still reach EOF. False if output was cut short, with lines
/// unread (a survivor still holds the pipe) or undelivered.
pub fn settle(stall: Duration) -> bool {
    let read_all = wait_with_progress(stall, || OPEN.load(Ordering::SeqCst) > 0);
    let delivered = read_all
        && wait_with_progress(stall, || {
            PROCESSED.load(Ordering::SeqCst) < QUEUED.load(Ordering::SeqCst)
        });
    if !delivered {
        DEAD.store(true, Ordering::SeqCst);
        // Readers that were waiting for room now drop their lines and go on.
        wait_with_progress(stall, || OPEN.load(Ordering::SeqCst) > 0);
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

/// Lines of recipe output read but not delivered to the client.
pub fn dropped() -> u64 {
    OFFERED.load(Ordering::SeqCst)
        - WRITTEN
            .load(Ordering::SeqCst)
            .min(OFFERED.load(Ordering::SeqCst))
}
