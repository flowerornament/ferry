//! The supervisor's one output writer. Data never controls (Rule A) and the
//! supervisor never blocks on data (Rule C):
//!
//! - session reader threads hand each output event to a bounded channel, and
//!   block when it is full, so a slow client slows the recipe, never the
//!   supervisor loop;
//! - one writer thread writes to stdout. If a write fails, it stops writing
//!   and counts what it drops; that never stops the run (client loss is
//!   learned from stdin, not from stdout);
//! - the supervisor's own events wait a bounded time for room, then are
//!   dropped and counted. The record on disk is the truth.
use std::{
    io::Write,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

enum Message {
    Line(Vec<u8>),
    /// Answered once every message queued before it is written or dropped.
    Mark(Sender<()>),
}

static WRITER: OnceLock<SyncSender<Message>> = OnceLock::new();
static DEAD: AtomicBool = AtomicBool::new(false);
static DROPPED: AtomicU64 = AtomicU64::new(0);

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
            Message::Line(bytes) => {
                if DEAD.load(Ordering::SeqCst) {
                    DROPPED.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
                let mut out = std::io::stdout().lock();
                let written = out
                    .write_all(&bytes)
                    .and_then(|()| out.write_all(b"\n"))
                    .and_then(|()| out.flush());
                if written.is_err() {
                    DEAD.store(true, Ordering::SeqCst);
                    DROPPED.fetch_add(1, Ordering::SeqCst);
                }
            }
            Message::Mark(reply) => {
                let _ = reply.send(());
            }
        }
    }
}

/// Queues one line from a session reader thread, waiting while the channel
/// is full: backpressure reaches the recipe, not the supervisor.
pub fn line(bytes: Vec<u8>) {
    let _ = sender().send(Message::Line(bytes));
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

/// Queues one of the supervisor's own events, waiting at most `bound`; an
/// event that finds no room is dropped and counted.
pub fn event(value: &serde_json::Value, bound: Duration) {
    if !offer(Message::Line(value.to_string().into_bytes()), bound) {
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }
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

/// Lines and events not delivered to the client so far.
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::SeqCst)
}
