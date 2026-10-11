//! Locks (SPEC §3 Lock): flock files under `locks/`. Held means busy; the
//! work inherits the descriptors; release is by `close()`, never `LOCK_UN`.
use crate::execution::Request;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The seat lock, tried once (SPEC §5): its file, or `None` while another
/// run holds it. The holder writes its run id into the file once it holds
/// it, and never clears it: a lock that outlives its run still names it.
pub(crate) fn try_seat(path: &Path, id: &str) -> Result<Option<fs::File>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            crate::execution::pause_at("seat-lock-write", path);
            file.set_len(0)?;
            (&file).write_all(id.as_bytes())?;
            Ok(Some(file))
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Polls for a lock until it is granted, `stop` is set, `waiting` (run on
/// every contended attempt) refuses, or the run's one wait budget ends
/// (`waits_end_ms`, else `wait` from now); then it refuses naming the holder. An exclusive holder writes its run id into
/// the lock file for that.
pub(crate) fn acquire_as(
    path: &Path,
    stop: &AtomicBool,
    shared: bool,
    waiting: &dyn Fn() -> Result<()>,
    request: &Request,
) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let started = Instant::now();
    let ends_ms = request
        .waits_end_ms
        .unwrap_or_else(|| crate::receipt::now_ms() + request.wait().as_millis());
    let mut said = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err("stopped while waiting for a lock".into());
        }
        let locked = if shared {
            FileExt::try_lock_shared(&file)
        } else {
            file.try_lock_exclusive()
        };
        match locked {
            Ok(()) => {
                if !shared {
                    file.set_len(0)?;
                    (&file).write_all(request.id.as_bytes())?;
                }
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                waiting()?;
                if crate::receipt::now_ms() >= ends_ms {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let holder = fs::read_to_string(path).unwrap_or_default();
                    let holder = match holder.trim() {
                        "" => "shared runs".to_owned(),
                        id => format!("run {id}"),
                    };
                    return Err(format!(
                        "waited {:?} for lock {name}, the end of the run's wait: held by {holder}",
                        started.elapsed()
                    )
                    .into());
                }
                if !said && started.elapsed() >= Duration::from_secs(3) {
                    said = true;
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    crate::execution::warn(&format!(
                        "ferry: waiting for lock {name}: another run holds it"
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    }
}
impl Drop for HostLease {
    /// An exclusive holder clears its name before the lock drops, so a
    /// later wait never names a run that is gone.
    fn drop(&mut self) {
        if self.record.mode == "exclusive" {
            for file in &self.held {
                let _ = file.set_len(0);
            }
        }
    }
}

/// The host-wide lease every session holds while it runs. Ordinary runs
/// share it. A recipe that requires `exclusive` holds it alone: it first
/// closes the gate, so no new run starts, then waits for in-flight runs to
/// release their shared side. New runs pass the gate briefly before taking
/// their shared side, so once the exclusive run holds the gate, later runs
/// cannot extend its wait; before that it competes for the gate, unqueued. The lease
/// covers sessions under this state root only: not other users, processes
/// outside Ferry, or other machines sharing the hardware.
///
/// Locks are released only by closing their descriptors (dropping the
/// files), never by `LOCK_UN`: a session may share the open file
/// description through inherited descriptors, and an explicit unlock would
/// free it under that session.
pub struct HostLease {
    held: Vec<fs::File>,
    pub record: Lease,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lease {
    /// `shared` or `exclusive`.
    pub mode: String,
    /// How long the session waited for the lease.
    pub wait_ms: u128,
    /// When the lease was granted and released (ms since the epoch).
    #[serde(default)]
    pub acquired_ms: Option<u128>,
    #[serde(default)]
    pub released_ms: Option<u128>,
}

impl HostLease {
    /// The lock descriptors this lease holds, for a session to inherit.
    pub fn files(&self) -> impl Iterator<Item = &fs::File> {
        self.held.iter()
    }
    /// The lease record, stamped with the release time while the lease is
    /// still held; the lease itself is released when this value is dropped.
    pub fn stamp(&self) -> Lease {
        Lease {
            released_ms: Some(crate::receipt::now_ms()),
            ..self.record.clone()
        }
    }
    pub(crate) fn acquire(root: &Path, request: &Request, stop: &AtomicBool) -> Result<Self> {
        let started = std::time::Instant::now();
        let gate = root.join("locks/host-gate");
        // The name earlier binaries used for their exclusive lock, so an old and
        // a new exclusive run on one state root still exclude each other.
        let host = root.join("locks/host-exclusive");
        let exclusive = request.recipe.requires.contains("exclusive");
        let local = root.ends_with("local");
        let held = if exclusive {
            // A tainted seat's survivors may hold the lease: no exclusive claim.
            let tainted = || crate::custody::check_exclusive(root, local);
            tainted()?;
            let gate = acquire_as(&gate, stop, false, &tainted, request)?;
            let host = acquire_as(&host, stop, false, &tainted, request)?;
            tainted()?;
            vec![gate, host]
        } else {
            // A tainted exclusive run's survivors may hold the gate or lease.
            let tainted = || crate::custody::check_host(root, local);
            // Before the first try too, not only once contended.
            tainted()?;
            let gate = acquire_as(&gate, stop, true, &tainted, request)?;
            let host = acquire_as(&host, stop, true, &tainted, request)?;
            drop(gate);
            vec![host]
        };
        Ok(Self {
            held,
            record: Lease {
                mode: if exclusive { "exclusive" } else { "shared" }.into(),
                wait_ms: started.elapsed().as_millis(),
                acquired_ms: Some(crate::receipt::now_ms()),
                released_ms: None,
            },
        })
    }
}

/// Sets or clears close-on-exec on a lock descriptor.
pub(crate) fn close_on_exec(file: &fs::File, close: bool) -> Result<()> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    let flags = if close {
        FdFlag::FD_CLOEXEC
    } else {
        FdFlag::empty()
    };
    fcntl(file, FcntlArg::F_SETFD(flags))?;
    Ok(())
}

#[cfg(test)]
mod lock_tests {
    /// Release is by close: while any descriptor of the open file
    /// description lives (an inherited copy, here a clone), the lock is held,
    /// which is why Ferry never unlocks explicitly.
    #[test]
    fn a_lock_is_released_only_when_every_descriptor_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let held = std::fs::File::create(&path).unwrap();
        fs2::FileExt::try_lock_exclusive(&held).unwrap();
        let inherited = held.try_clone().unwrap();
        drop(held);
        let probe = std::fs::File::open(&path).unwrap();
        assert!(
            fs2::FileExt::try_lock_exclusive(&probe).is_err(),
            "freed while a copy lived"
        );
        drop(inherited);
        assert!(fs2::FileExt::try_lock_exclusive(&probe).is_ok());
    }
}
