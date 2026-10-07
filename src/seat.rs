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

pub fn check(root: &Path, seat: &str, local: bool) -> Result<()> {
    let owner = match fs::read(path(root, seat)) {
        Ok(bytes) => serde_json::from_slice::<Owner>(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let local = if local { " --local" } else { "" };
    Err(format!(
        "cleanup_pending: seat {seat}, session {:?}; run ferry seat recover {seat}{local}",
        owner.session
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

pub fn recover(root: &Path, seat: &str) -> Result<()> {
    config::identifier(seat)?;
    fs::create_dir_all(root.join("locks"))?;
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("locks").join(seat))?;
    fs2::FileExt::try_lock_exclusive(&lease)?;
    let marker = path(root, seat);
    let owner: Owner = serde_json::from_slice(&fs::read(&marker)?)?;
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
            "cleanup_pending: session {session} still contains {held:?}; recovery never kills"
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

#[cfg(target_os = "macos")]
fn start_time(pid: i32) -> Result<u128> {
    let info = libproc::proc_pid::pidinfo::<libproc::bsd_info::BSDInfo>(pid, 0)?;
    Ok(u128::from(info.pbi_start_tvsec) * 1_000_000 + u128::from(info.pbi_start_tvusec))
}

#[cfg(not(target_os = "macos"))]
fn start_time(pid: i32) -> Result<u128> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = text.rsplit_once(')').ok_or("invalid process stat")?;
    Ok(fields
        .split_whitespace()
        .nth(19)
        .ok_or("missing process start time")?
        .parse()?)
}
