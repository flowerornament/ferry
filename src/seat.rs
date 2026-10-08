//! Durable session ownership prevents reuse after a supervisor dies uncleanly.
use crate::{config, execution};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Serialize, Deserialize)]
pub struct Owner {
    pub id: String,
    pub token: String,
    pub session: Option<u32>,
    pub start: Option<u128>,
}

pub fn path(root: &Path, seat: &str) -> PathBuf {
    root.join("owners").join(format!("{seat}.json"))
}

/// The recovery command for a seat, saying where to run it and which
/// `--seat` it is: seat keys are `<caller hash>-<seat name>`.
pub fn recover_hint(seat: &str, local: bool, extra: &str) -> String {
    let name = seat.split_once('-').map_or(seat, |(_, name)| name);
    if local {
        format!("`ferry seat recover {seat} --local{extra}` (--seat {name})")
    } else {
        let host = std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "the host".into());
        format!("`ferry seat recover {seat}{extra}` on {host} (--seat {name})")
    }
}

pub fn check(root: &Path, seat: &str, local: bool) -> Result<()> {
    let owner = match fs::read(path(root, seat)) {
        Ok(bytes) => serde_json::from_slice::<Owner>(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    Err(format!(
        "cleanup_pending: seat {seat}, session {:?}; run {}",
        owner.session,
        recover_hint(seat, local, "")
    )
    .into())
}

pub fn reserve(root: &Path, request: &execution::Request) -> Result<()> {
    fs::create_dir_all(root.join("owners"))?;
    let owner = Owner {
        id: request.id.clone(),
        token: request.token.clone(),
        session: None,
        start: None,
    };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path(root, &request.seat))?;
    serde_json::to_writer(&mut file, &owner)?;
    file.sync_all()?;
    Ok(())
}

pub fn bind(root: &Path, request: &execution::Request) -> Result<()> {
    let destination = path(root, &request.seat);
    let mut owner: Owner = serde_json::from_slice(&fs::read(&destination)?)?;
    if owner.id != request.id || owner.token != request.token || owner.session.is_some() {
        return Err("session ownership differs".into());
    }
    let pid = std::process::id();
    owner.session = Some(pid);
    owner.start = Some(start_time(i32::try_from(pid)?)?);
    let temporary = root.join("owners").join(format!("{}.tmp", request.id));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, &owner)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(temporary, destination)?;
    Ok(())
}

/// Clears what keeps a seat refused, only once it is proven safe: an owner
/// marker when its recorded session is empty, and a taint record when every
/// recorded survivor is gone. Never kills.
pub fn recover(root: &Path, seat: &str, accept_unknown: Option<&str>) -> Result<()> {
    config::identifier(seat)?;
    fs::create_dir_all(root.join("locks"))?;
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("locks").join(seat))?;
    match fs2::FileExt::try_lock_exclusive(&lease) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            return Err(format!(
                "seat {seat} is in use: a run, or a process it left that kept the seat lock, is still alive; stop it, then retry"
            )
            .into());
        }
        Err(e) => return Err(e.into()),
    }
    let marker = path(root, seat);
    let taint = crate::custody::taint_path(root, seat);
    if !marker.exists() && !taint.exists() {
        return Err(format!("seat {seat} has no owner marker or taint to recover").into());
    }
    if marker.exists() {
        recover_owner(root, seat, &marker)?;
    }
    if taint.exists() {
        recover_taint(seat, &taint, accept_unknown)?;
    }
    Ok(())
}

fn recover_owner(root: &Path, seat: &str, marker: &Path) -> Result<()> {
    let owner: Owner = serde_json::from_slice(&fs::read(marker)?)?;
    let (Some(session), Some(start)) = (owner.session, owner.start) else {
        return Err(
            "cleanup_pending: session birth was not recorded; recovery cannot prove absence".into(),
        );
    };
    let mut held = Vec::new();
    for pid in execution::session_members(session)? {
        match start_time(pid) {
            Ok(current) if current >= start => held.push(pid),
            Ok(_) => {}
            Err(error) => {
                // A race with exit permits another observation, never an inferred absence.
                if execution::session_members(session)?.contains(&pid) {
                    return Err(error);
                }
            }
        }
    }
    if !held.is_empty() {
        return Err(format!(
            "cleanup_pending: seat {seat} session {session} still contains {held:?}; recovery never kills"
        )
        .into());
    }
    fs::remove_file(marker)?;
    let quarantine = root
        .join("locks")
        .join(format!("cleanup-pending-{}", owner.id));
    if quarantine.exists() {
        fs::remove_file(quarantine)?;
    }
    Ok(())
}

/// A survivor is gone when no process has its pid, or the process that has
/// it started at another time. Without a recorded start time, any live
/// process with the pid counts as the survivor.
fn recover_taint(seat: &str, taint: &Path, accept_unknown: Option<&str>) -> Result<()> {
    let record: crate::custody::Taint = match serde_json::from_slice(&fs::read(taint)?) {
        Ok(record) => record,
        Err(error) if accept_unknown.is_some() => {
            // An explicit operator act, recorded in the recover receipt. The
            // free seat lock shows only that no process still holds it; a
            // survivor that closed its descriptors may still live.
            let _ = error;
            fs::remove_file(taint)?;
            return Ok(());
        }
        Err(error) => {
            return Err(format!(
                "seat {seat}: taint record {} is unreadable ({error}); confirm nothing it ran is alive, then add --accept-unknown --reason '<why>'",
                taint.display()
            )
            .into());
        }
    };
    let survivors: Vec<crate::custody::Survivor> = if record.survivors.is_empty() {
        record
            .pids
            .iter()
            .map(|pid| crate::custody::Survivor {
                pid: *pid,
                start: None,
            })
            .collect()
    } else {
        record.survivors
    };
    if survivors.is_empty() {
        if accept_unknown.is_some() {
            // An explicit operator act, recorded in the recover receipt. The
            // free seat lock shows only that no process still holds it; a
            // survivor that closed its descriptors may still live.
            fs::remove_file(taint)?;
            return Ok(());
        }
        return Err(format!(
            "seat {seat} is tainted ({}) but names no survivors, so recovery cannot prove them gone; confirm nothing it ran is alive, then add --accept-unknown --reason '<why>'",
            record.reason
        )
        .into());
    }
    let alive: Vec<i32> = survivors
        .iter()
        .filter(
            |survivor| match (start_time(survivor.pid), survivor.start) {
                (Ok(now), Some(then)) => now == then,
                (Ok(_), None) => true,
                (Err(_), _) => {
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(survivor.pid), None).is_ok()
                }
            },
        )
        .map(|survivor| survivor.pid)
        .collect();
    if !alive.is_empty() {
        return Err(format!(
            "seat {seat} is tainted: survivors {alive:?} of run {} are still alive; stop them first, recovery never kills",
            record.id
        )
        .into());
    }
    fs::remove_file(taint)?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn start_time(pid: i32) -> Result<u128> {
    let info = libproc::proc_pid::pidinfo::<libproc::bsd_info::BSDInfo>(pid, 0)?;
    Ok(u128::from(info.pbi_start_tvsec) * 1_000_000 + u128::from(info.pbi_start_tvusec))
}

#[cfg(not(target_os = "macos"))]
pub fn start_time(pid: i32) -> Result<u128> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = text.rsplit_once(')').ok_or("invalid process stat")?;
    Ok(fields
        .split_whitespace()
        .nth(19)
        .ok_or("missing process start time")?
        .parse()?)
}
