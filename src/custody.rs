//! Seat custody: whether a finished session left any process behind.
//!
//! On Linux the run's supervisor is a child subreaper, so a descendant that
//! leaves the recipe's session (`setsid`, double fork, closed descriptors)
//! is reparented to the supervisor rather than to init. While the supervisor
//! lives, its descendant census is therefore complete. At a normal terminal
//! release an empty census writes `idle/<seat>.json`; survivors write
//! `taint/<seat>.json` instead. A session deletes its seat's idle record as
//! soon as it holds the seat lock, so no record outlives a later run.
//!
//! Outside this boundary nothing is claimed: processes not descended from the
//! session, other users, other hosts, and supervisors that die before release
//! (their seat keeps its owner marker and gains no idle record). Other
//! platforms have no census and never record idle.
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// What the supervisor found when its session ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Census {
    /// No live descendant of the supervisor.
    Empty,
    /// Live descendants survived the session.
    Survivors(Vec<i32>),
    /// This platform cannot take a complete census.
    Unsupported,
}

/// The custody evidence a release leaves in the receipt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Release {
    pub census: Census,
    /// `idle`, `taint`, or `none` when the outcome was not a normal terminal one.
    pub record: String,
}

#[derive(Serialize, Deserialize)]
struct Idle<'a> {
    released_ms: u128,
    id: &'a str,
    commit: &'a str,
}

/// Makes this process the reaper of its orphaned descendants (Linux).
#[cfg_attr(not(target_os = "linux"), allow(clippy::unnecessary_wraps))]
pub fn supervise() -> Result<()> {
    #[cfg(target_os = "linux")]
    nix::sys::prctl::set_child_subreaper(true)?;
    Ok(())
}

/// Called under the seat lock as a session begins: the seat is in use.
pub fn begin(root: &Path, seat: &str) -> Result<()> {
    match fs::remove_file(root.join("idle").join(format!("{seat}.json"))) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// Called under the seat lock after a session ends. `terminal` is true only
/// for ran-pass, ran-fail and timeout with confirmed session cleanup.
pub fn release(
    root: &Path,
    request: &crate::execution::Request,
    terminal: bool,
) -> Result<Release> {
    let census = census()?;
    let record = match (&census, terminal) {
        (Census::Survivors(pids), _) => {
            write(
                root,
                "taint",
                &request.seat,
                &serde_json::json!({"id": request.id, "reason": "descendants survived the session", "pids": pids}),
            )?;
            "taint"
        }
        (Census::Empty, true) => {
            write(
                root,
                "idle",
                &request.seat,
                &Idle {
                    released_ms: crate::receipt::now_ms(),
                    id: &request.id,
                    commit: &request.commit,
                },
            )?;
            "idle"
        }
        _ => "none",
    };
    Ok(Release {
        census,
        record: record.into(),
    })
}

fn write(root: &Path, kind: &str, seat: &str, value: &impl Serialize) -> Result<()> {
    let dir = root.join(kind);
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{seat}.json.tmp"));
    fs::write(&tmp, serde_json::to_vec(value)?)?;
    fs::rename(tmp, dir.join(format!("{seat}.json")))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn census() -> Result<Census> {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    // Reap descendants that already exited and were reparented here.
    while let Ok(status) = waitpid(None, Some(WaitPidFlag::WNOHANG)) {
        if status == WaitStatus::StillAlive {
            break;
        }
    }
    let me = i32::try_from(std::process::id())?;
    let mut parents = std::collections::HashMap::new();
    for entry in fs::read_dir("/proc")? {
        let Ok(pid) = entry?.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        // A process that exits during the census simply drops out of it.
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let (_, fields) = stat.rsplit_once(')').ok_or("invalid process stat")?;
        let mut fields = fields.split_whitespace();
        let state = fields.next().ok_or("missing process state")?;
        let ppid: i32 = fields.next().ok_or("missing parent")?.parse()?;
        if state != "Z" {
            parents.insert(pid, ppid);
        }
    }
    let mut survivors: Vec<i32> = parents
        .keys()
        .copied()
        .filter(|pid| {
            let mut current = *pid;
            for _ in 0..parents.len() {
                match parents.get(&current) {
                    Some(&parent) if parent == me => return true,
                    Some(&parent) if parent > 1 => current = parent,
                    _ => return false,
                }
            }
            false
        })
        .collect();
    survivors.sort_unstable();
    Ok(if survivors.is_empty() {
        Census::Empty
    } else {
        Census::Survivors(survivors)
    })
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
fn census() -> Result<Census> {
    Ok(Census::Unsupported)
}
