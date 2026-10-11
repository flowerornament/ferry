//! The host's record of a run: one authoritative account per run id.
//!
//! - `reserve`, before any effect: `records/<id>.reserved.json` with the
//!   supervisor's pid, start time and boot id. It is created complete (a
//!   temporary file linked into place), and a second submit of the same id
//!   finds it and is refused.
//! - `finalize`, once: `records/<id>.json`, the reservation's fields plus
//!   the run's, linked into place so it can never be replaced, the file and
//!   its directory synced, then the reservation removed.
//! - `reconcile`: a reservation whose supervisor is provably gone (another
//!   boot, a dead pid, or a different start time) is finalized as lost/255.
//!   A live supervisor is never reconciled.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// What a reservation holds: enough to tell whether its supervisor still
/// lives, and to finalize the run as lost if it does not.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reservation {
    /// Always `reserved`.
    pub state: String,
    pub id: String,
    pub token: String,
    pub commit: String,
    pub seat: String,
    /// The supervisor's binary, which a lost record names.
    pub binary_sha256: String,
    pub supervisor: Supervisor,
    /// The boot the supervisor ran in; absent where it cannot be read.
    pub boot_id: Option<String>,
    pub submitted_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Supervisor {
    pub pid: u32,
    /// Its start time; absent if it could not be read.
    pub start: Option<u128>,
}

fn records(root: &Path) -> PathBuf {
    root.join("records")
}

fn reserved_path(root: &Path, id: &str) -> PathBuf {
    records(root).join(format!("{id}.reserved.json"))
}

fn final_path(root: &Path, id: &str) -> PathBuf {
    records(root).join(format!("{id}.json"))
}

/// This boot's id: Linux `/proc/sys/kernel/random/boot_id`, macOS
/// `kern.bootsessionuuid`. `None` where it cannot be read (then only the
/// pid and start time decide).
pub fn boot_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    let id = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok();
    #[cfg(not(target_os = "linux"))]
    let id = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.bootsessionuuid"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok());
    id.map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
}

/// Writes `bytes` to `path` complete, or not at all, and never over an
/// existing file: a synced temporary file is linked into place (link(2)
/// fails if the name exists), then the directory is synced. `point` names
/// the link step for fault injection, and enables the `fsync` point too.
pub fn write_new(path: &Path, bytes: &[u8], point: Option<&str>) -> Result<()> {
    static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let injected = |at: &str| point.is_some() && crate::execution::fail_at(at);
    let dir = path.parent().ok_or("record path has no directory")?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let written = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        if injected("fsync") {
            return Err("fsync: injected by FERRY_FAIL_AT".into());
        }
        file.sync_all()?;
        if let Some(point) = point.filter(|point| injected(point)) {
            return Err(format!("{point}: injected by FERRY_FAIL_AT").into());
        }
        fs::hard_link(&tmp, path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!("{} already exists", path.display())
            } else {
                format!("linking {}: {e}", path.display())
            }
        })?;
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&tmp);
    written
}

/// Reserves `id` before any effect. Refused if the id was already submitted
/// (reserved or final): a retried submit never starts a second run.
pub fn reserve(root: &Path, reservation: &Reservation) -> Result<()> {
    let id = &reservation.id;
    let reserved = reserved_path(root, id);
    write_new(
        &reserved,
        &serde_json::to_vec(reservation)?,
        Some("reserve"),
    )
    .map_err(|e| {
        if reserved.exists() {
            format!("run {id} already submitted")
        } else {
            format!("reserving {}: {e}", reserved.display())
        }
    })?;
    // Checked after reserving: a run finalizes before it drops its
    // reservation, so a finished id can never be reserved unnoticed.
    if final_path(root, id).exists() {
        let _ = fs::remove_file(&reserved);
        return Err(format!("run {id} already finished; see its record").into());
    }
    Ok(())
}

/// Finalizes `id` with `record` (a JSON object) and the reservation's
/// fields, once: a second finalize fails loudly and leaves the first record
/// as it was. The reservation goes after the final record is in place.
pub fn finalize(root: &Path, id: &str, record: &serde_json::Value) -> Result<()> {
    let mut record = record.clone();
    let reserved = match fs::read(reserved_path(root, id)) {
        Err(e) if final_path(root, id).exists() => {
            return Err(format!("run {id} is already final ({e})").into());
        }
        read => serde_json::from_slice::<serde_json::Value>(&read?)?,
    };
    if let (Some(record), Some(reserved)) = (record.as_object_mut(), reserved.as_object()) {
        for (key, value) in reserved {
            record.entry(key).or_insert_with(|| value.clone());
        }
    }
    write_new(
        &final_path(root, id),
        &serde_json::to_vec(&record)?,
        Some("finalize-link"),
    )
    .map_err(|e| format!("finalizing run {id}: {e}"))?;
    match fs::remove_file(reserved_path(root, id)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("run {id} is final, but its reservation remains: {e}").into())
        }
        _ => Ok(()),
    }
}

/// Whether a reservation's supervisor is provably gone, given this boot's
/// id: another boot, no process with its pid, or a process with its pid
/// that started at another time. Without a recorded start time, a live
/// pid is never proof of anything: the supervisor counts as alive.
fn gone(reservation: &Reservation, boot: Option<&str>) -> bool {
    if let (Some(then), Some(now)) = (reservation.boot_id.as_deref(), boot)
        && then != now
    {
        return true;
    }
    let Ok(pid) = i32::try_from(reservation.supervisor.pid) else {
        return false;
    };
    match crate::seat::start_time(pid) {
        Ok(start) => reservation
            .supervisor
            .start
            .is_some_and(|then| then != start),
        Err(_) => nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
            .is_err_and(|e| e == nix::errno::Errno::ESRCH),
    }
}

/// The final record of `id`, reconciling first if its supervisor is gone.
/// `None` while a live supervisor holds the reservation, or if nothing is
/// known of `id`: a reservation alone is never a verdict.
pub fn reconcile(root: &Path, id: &str) -> Result<Option<Vec<u8>>> {
    reconcile_with(root, id, boot_id().as_deref())
}

fn reconcile_with(root: &Path, id: &str, boot: Option<&str>) -> Result<Option<Vec<u8>>> {
    if crate::execution::fail_at("reconcile") {
        return Err("reconcile: injected by FERRY_FAIL_AT".into());
    }
    if let Ok(bytes) = fs::read(final_path(root, id)) {
        return Ok(Some(bytes));
    }
    let reservation: Reservation = match fs::read(reserved_path(root, id)) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !gone(&reservation, boot) {
        return Ok(None);
    }
    // Another reconciler or the supervisor itself may finalize first: then
    // its record stands, exactly once.
    // The reservation supplies the binding: token, commit, binary.
    let lost = serde_json::json!({
        "protocol": crate::receipt::PROTOCOL,
        "state": "final",
        "outcome": "lost",
        "exit_code": 255,
        "reason": "supervisor gone (reconciled)",
    });
    if let Err(error) = finalize(root, id, &lost)
        && !final_path(root, id).exists()
    {
        return Err(error);
    }
    Ok(Some(fs::read(final_path(root, id))?))
}

/// Reconciles every reservation of `seat` whose supervisor is gone, as the
/// next run on that seat begins. Errors are left for the next reader.
pub fn reconcile_seat(root: &Path, seat: &str) {
    let boot = boot_id();
    let Ok(entries) = fs::read_dir(records(root)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".reserved.json") else {
            continue;
        };
        let belongs = fs::read(entry.path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Reservation>(&bytes).ok())
            .is_some_and(|r| r.seat == seat);
        if belongs {
            let _ = reconcile_with(root, id, boot.as_deref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Reservation, finalize, reconcile_with, reserve};
    use std::fs;

    fn reservation(pid: u32, start: Option<u128>, boot: &str) -> Reservation {
        Reservation {
            state: "reserved".into(),
            id: "run-1".into(),
            token: "token".into(),
            binary_sha256: "binary".into(),
            commit: "0".repeat(40),
            seat: "seat".into(),
            supervisor: super::Supervisor { pid, start },
            boot_id: Some(boot.into()),
            submitted_ms: 1,
        }
    }

    fn own(boot: &str) -> Reservation {
        let pid = std::process::id();
        let start = crate::seat::start_time(i32::try_from(pid).unwrap()).ok();
        reservation(pid, start, boot)
    }

    #[test]
    fn a_second_submit_of_an_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        reserve(dir.path(), &own("boot")).unwrap();
        let again = reserve(dir.path(), &own("boot")).unwrap_err().to_string();
        assert!(again.contains("already submitted"), "{again}");
        finalize(dir.path(), "run-1", &serde_json::json!({})).unwrap();
        let after = reserve(dir.path(), &own("boot")).unwrap_err().to_string();
        assert!(after.contains("already finished"), "{after}");
    }

    #[test]
    fn a_live_supervisor_is_never_reconciled() {
        let dir = tempfile::tempdir().unwrap();
        reserve(dir.path(), &own("boot")).unwrap();
        assert_eq!(
            reconcile_with(dir.path(), "run-1", Some("boot")).unwrap(),
            None
        );
        assert!(dir.path().join("records/run-1.reserved.json").exists());
        assert!(!dir.path().join("records/run-1.json").exists());
    }

    #[test]
    fn another_boot_means_the_supervisor_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        // Our own live pid and start: only the boot id differs.
        reserve(dir.path(), &own("earlier-boot")).unwrap();
        let bytes = reconcile_with(dir.path(), "run-1", Some("this-boot"))
            .unwrap()
            .unwrap();
        // The caller reads it as any host result, bound to the request.
        let result: crate::transport::HostResult = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result.outcome, crate::receipt::Outcome::Lost);
        assert_eq!(
            (result.token.as_str(), result.binary_sha256.as_str()),
            ("token", "binary")
        );
        let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record["outcome"], "lost");
        assert_eq!(record["exit_code"], 255);
        assert_eq!(record["reason"], "supervisor gone (reconciled)");
        assert!(!dir.path().join("records/run-1.reserved.json").exists());
    }

    #[test]
    fn a_dead_pid_or_another_start_means_gone_and_reconciles_exactly_once() {
        for start_differs in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let r = if start_differs {
                // Our own live pid, but a start time that is not ours.
                reservation(std::process::id(), Some(1), "boot")
            } else {
                // A pid far beyond any live process.
                reservation(i32::MAX.unsigned_abs() - 1, Some(1), "boot")
            };
            reserve(dir.path(), &r).unwrap();
            let first = reconcile_with(dir.path(), "run-1", Some("boot"))
                .unwrap()
                .unwrap();
            let second = reconcile_with(dir.path(), "run-1", Some("boot"))
                .unwrap()
                .unwrap();
            assert_eq!(first, second);
            let names: Vec<_> = fs::read_dir(dir.path().join("records"))
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, ["run-1.json"], "{start_differs}");
        }
    }

    #[test]
    fn of_two_finalizers_exactly_one_wins() {
        let dir = tempfile::tempdir().unwrap();
        reserve(dir.path(), &own("boot")).unwrap();
        let root = dir.path().to_path_buf();
        let racers: Vec<_> = (0..8)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    finalize(&root, "run-1", &serde_json::json!({ "winner": i })).is_ok()
                })
            })
            .collect();
        let wins = racers
            .into_iter()
            .map(|racer| racer.join().unwrap())
            .filter(|won| *won)
            .count();
        // Every racer links a complete file; only one name can exist.
        assert_eq!(wins, 1);
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("records/run-1.json")).unwrap()).unwrap();
        assert!(record["winner"].is_number());
        // The record carries the reservation's fields.
        assert_eq!(record["supervisor"]["pid"], std::process::id());
        // Its reservation gone, a later finalize has nothing to finalize.
        let again = finalize(&root, "run-1", &serde_json::json!({}))
            .unwrap_err()
            .to_string();
        assert!(again.contains("already final"), "{again}");
    }
}
