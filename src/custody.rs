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
    /// The kernel reports a live child. The pids name what /proc showed of
    /// them, possibly nothing; the verdict does not depend on that list.
    Survivors(Vec<i32>),
    /// The census could not be taken; custody is unknown.
    Unknown(String),
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

/// Reaps adopted descendants that have already exited, so they do not linger
/// as zombies a project's own process-group accounting would still count.
/// `leader` is the session leader this process waits on itself; it is left
/// alone. Linux only: elsewhere nothing is adopted.
pub fn reap_adopted(leader: u32) {
    #[cfg(target_os = "linux")]
    {
        let me = std::process::id().to_string();
        let Ok(entries) = fs::read_dir("/proc") else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            if u32::try_from(pid).ok() == Some(leader) {
                continue;
            }
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some((_, fields)) = stat.rsplit_once(')') else {
                continue;
            };
            let mut fields = fields.split_whitespace();
            if fields.next() == Some("Z") && fields.next() == Some(me.as_str()) {
                let _ = nix::sys::wait::waitpid(
                    nix::unistd::Pid::from_raw(pid),
                    Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                );
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = leader;
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
    let census = census();
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

/// The kernel decides: after reaping every exited orphan, ECHILD proves no
/// child remains, and a live child proves a survivor. Because the supervisor
/// is a subreaper, every surviving descendant is, or sits under, a direct
/// child. Any other wait error leaves custody unknown.
#[cfg(target_os = "linux")]
fn census() -> Census {
    use nix::{
        errno::Errno,
        sys::wait::{WaitPidFlag, WaitStatus, waitpid},
    };
    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Err(Errno::ECHILD) => return Census::Empty,
            Ok(WaitStatus::StillAlive) => return Census::Survivors(children()),
            Ok(_) => {}
            Err(error) => return Census::Unknown(error.to_string()),
        }
    }
}

/// Diagnostic only: the supervisor's live direct children, as far as /proc
/// shows them. An empty or partial list never weakens a kernel survivor.
#[cfg(target_os = "linux")]
fn children() -> Vec<i32> {
    let me = std::process::id().to_string();
    let proc_root = std::env::var_os("FERRY_DIAGNOSTIC_PROC").unwrap_or_else(|| "/proc".into());
    let mut pids: Vec<i32> = fs::read_dir(&proc_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_string_lossy().parse::<i32>().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let (_, fields) = stat.rsplit_once(')')?;
            let mut fields = fields.split_whitespace();
            let state = fields.next()?;
            (state != "Z" && fields.next()? == me).then_some(pid)
        })
        .collect();
    pids.sort_unstable();
    pids
}

#[cfg(not(target_os = "linux"))]
fn census() -> Census {
    Census::Unsupported
}
