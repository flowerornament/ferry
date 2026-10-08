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
//! At release, survivors the census attributes to this run are stopped: TERM
//! (and their process group, when it is theirs), a grace period, then KILL,
//! and a second census. Only an empty census after that leaves the seat idle;
//! survivors that remain, or a census that cannot be taken, taint it, and a
//! tainted seat refuses new runs until `ferry seat recover` proves the
//! recorded survivors gone. Recovery never kills.
//!
//! Outside this boundary nothing is claimed: processes not descended from the
//! session, other users, other hosts, and supervisors that die before release
//! (their seat keeps its owner marker and gains no idle record). Other
//! platforms have no census and never record idle.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
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

/// How long stopped survivors get between TERM and KILL.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// How long survivors sent SIGKILL get to be reaped before the final census.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// The custody evidence a release leaves in the receipt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Release {
    /// The census taken as the session ended, before anything was stopped.
    pub census: Census,
    /// `idle`, `taint`, or `none` when the outcome was not a normal terminal one.
    pub record: String,
    /// Survivors this release signalled (TERM, then KILL if they remained).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stopped: Vec<i32>,
    /// The census after stopping, when anything was stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Census>,
}

/// A survivor named in a taint record: pid and start time, so recovery can
/// tell it from a later process that reused the pid.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Survivor {
    pub pid: i32,
    pub start: Option<u128>,
}

#[derive(Serialize, Deserialize)]
pub struct Taint {
    pub id: String,
    pub reason: String,
    #[serde(default)]
    pub pids: Vec<i32>,
    #[serde(default)]
    pub survivors: Vec<Survivor>,
}

#[derive(Serialize, Deserialize)]
struct Idle<'a> {
    released_ms: u128,
    id: &'a str,
    commit: &'a str,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stopped: Vec<i32>,
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

pub fn taint_path(root: &Path, seat: &str) -> std::path::PathBuf {
    root.join("taint").join(format!("{seat}.json"))
}

/// Called under the seat lock before a session: a tainted seat refuses.
pub fn check(root: &Path, seat: &str, local: bool) -> Result<()> {
    let taint: Taint = match fs::read(taint_path(root, seat)) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let local = if local { " --local" } else { "" };
    Err(format!(
        "seat {seat} is tainted: run {} left survivors (pids {:?}); stop them, then run ferry seat recover {seat}{local}",
        taint.id, taint.pids
    )
    .into())
}

/// Called under the seat lock after a session ends. `terminal` is true only
/// for ran-pass, ran-fail and timeout with confirmed session cleanup. Runs
/// after the collect hook, so collection still sees what the run left.
pub fn release(
    root: &Path,
    request: &crate::execution::Request,
    terminal: bool,
) -> Result<Release> {
    let census = census();
    let (stopped, after) = match &census {
        Census::Survivors(pids) if !pids.is_empty() => {
            let (stopped, after) = stop(pids);
            (stopped, Some(after))
        }
        _ => (vec![], None),
    };
    let last = after.as_ref().unwrap_or(&census);
    let record = match (last, terminal) {
        (Census::Survivors(pids), _) => {
            write(
                root,
                "taint",
                &request.seat,
                &Taint {
                    id: request.id.clone(),
                    reason: "descendants survived the session".into(),
                    pids: pids.clone(),
                    survivors: pids
                        .iter()
                        .map(|pid| Survivor {
                            pid: *pid,
                            start: crate::seat::start_time(*pid).ok(),
                        })
                        .collect(),
                },
            )?;
            "taint"
        }
        (Census::Unknown(error), _) => {
            write(
                root,
                "taint",
                &request.seat,
                &Taint {
                    id: request.id.clone(),
                    reason: format!("custody census could not be taken: {error}"),
                    pids: vec![],
                    survivors: vec![],
                },
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
                    stopped: stopped.clone(),
                },
            )?;
            "idle"
        }
        _ => "none",
    };
    Ok(Release {
        census,
        record: record.into(),
        stopped,
        after,
    })
}

/// Stops survivors this run's census attributed to it: TERM each (and its
/// group, when it leads one), re-census through the grace period so newly
/// adopted descendants are stopped too, then KILL what remains. Returns the
/// pids signalled and the final census. Pids come only from our own census:
/// unreaped children of this supervisor, so none can have been reused.
fn stop(first: &[i32]) -> (Vec<i32>, Census) {
    use nix::sys::signal::Signal;
    let mut stopped: Vec<i32> = vec![];
    let mut current = Census::Survivors(first.to_vec());
    for (signal, grace) in [(Signal::SIGTERM, STOP_GRACE), (Signal::SIGKILL, KILL_GRACE)] {
        let deadline = Instant::now() + grace;
        let mut signalled: Vec<i32> = vec![];
        loop {
            match &current {
                Census::Survivors(pids) if !pids.is_empty() => {
                    for pid in pids {
                        if !signalled.contains(pid) {
                            signalled.push(*pid);
                            send(*pid, pids, signal);
                            if !stopped.contains(pid) {
                                stopped.push(*pid);
                            }
                        }
                    }
                }
                // Empty is done; unknown or unattributed cannot be stopped.
                _ => return (stopped, current),
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            current = census();
        }
    }
    (stopped, current)
}

/// Signals one survivor, and its process group when the group's leader is
/// itself one of this run's survivors.
fn send(pid: i32, survivors: &[i32], signal: nix::sys::signal::Signal) {
    use nix::{
        sys::signal::{kill, killpg},
        unistd::{Pid, getpgid},
    };
    let target = Pid::from_raw(pid);
    if let Ok(group) = getpgid(Some(target))
        && (group == target || survivors.contains(&group.as_raw()))
    {
        let _ = killpg(group, signal);
    }
    let _ = kill(target, signal);
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
