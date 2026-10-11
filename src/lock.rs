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

/// A run's locks, from the moment each is taken until its record is final
/// (SPEC §5: final record, then close locks), on every path.
#[derive(Default)]
pub struct Held {
    pub seat: Option<fs::File>,
    /// Host locks taken before the lease is complete (the gate, while the
    /// host lock is still waited for).
    pub partial: Vec<fs::File>,
    pub host: Option<HostLease>,
}

/// Polls for a lock until it is granted, `stop` is set, or the run's one
/// wait budget ends (`waits_end_ms`, else `wait` from now); then it refuses
/// naming the holder. An exclusive holder writes its run id into the lock
/// file for that.
pub(crate) fn acquire_as(
    path: &Path,
    stop: &AtomicBool,
    shared: bool,
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
                if crate::receipt::now_ms() >= ends_ms {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    // The holder's name is never cleared: the work inherits
                    // the lock and may outlive its run (SPEC §3).
                    let named = fs::read_to_string(path).unwrap_or_default();
                    let root = path.parent().and_then(Path::parent).unwrap_or(path);
                    crate::execution::pause_at(
                        "host-probe",
                        &root.join("runs").join(&request.id).join("probe"),
                    );
                    return Err(format!(
                        "waited {:?} for lock {name}, the end of the run's wait: {}",
                        started.elapsed(),
                        crate::record::host_holder(root, path, named.trim())
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
    /// When the lease was granted, and when the run's use of it ended (ms
    /// since the epoch). The latter is stamped just before the record is
    /// final; the descriptors close after it (SPEC §5).
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
    /// Takes the lease; each lock taken goes to `partial` at once, so a
    /// failed wait after the gate leaves the gate to the supervisor.
    pub(crate) fn acquire(
        root: &Path,
        request: &Request,
        stop: &AtomicBool,
        partial: &mut Vec<fs::File>,
    ) -> Result<Self> {
        let started = std::time::Instant::now();
        let gate = root.join("locks/host.gate");
        let host = root.join("locks/host");
        let exclusive = request.recipe.requires.contains("exclusive");
        let held = if exclusive {
            partial.push(acquire_as(&gate, stop, false, request)?);
            partial.push(acquire_as(&host, stop, false, request)?);
            std::mem::take(partial)
        } else {
            partial.push(acquire_as(&gate, stop, true, request)?);
            let host = acquire_as(&host, stop, true, request)?;
            // A shared run passes the gate: it closes once the lease is held.
            partial.clear();
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
