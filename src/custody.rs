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
    /// Why release could not complete. The work's verdict stands; custody is
    /// then unknown, and the seat is tainted if that could be written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The locks a taint's survivors may still hold through inherited
/// descriptors: the seat lock, and the host lease in the run's mode (an
/// exclusive run also held the gate). Waiters on them refuse.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeldLocks {
    pub seat: String,
    /// `shared` or `exclusive`.
    pub host: String,
    pub gate: bool,
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
    /// Absent in records from before it was kept: assume the seat lock only.
    #[serde(default)]
    pub locks: Option<HeldLocks>,
}

#[derive(Serialize, Deserialize)]
struct Idle<'a> {
    released_ms: u128,
    id: &'a str,
    commit: &'a str,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stopped: Vec<i32>,
}

/// Makes this process the reaper of its orphaned descendants (Linux). Only
/// a freshly spawned `ferry host-run` supervises: it has no children of its
/// own yet, so everything the census later finds is the run's.
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

/// Refuses a tainted seat, with the next step. Also called before waiting
/// for the seat lock and while waiting, since a survivor that kept the
/// inherited lock would otherwise make the run wait forever.
pub fn check(root: &Path, seat: &str, local: bool) -> Result<()> {
    let path = taint_path(root, seat);
    let next = |extra: &str| crate::seat::recover_hint(seat, local, extra);
    let taint: Taint = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "seat {seat} is tainted, but its record {} is unreadable ({error}); confirm nothing it ran is alive, then run {}",
                path.display(),
                next(" --accept-unknown --reason '<why>'")
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(format!(
                "seat {seat}: whether it is tainted is unknown ({}: {e}); fix access to it",
                path.display()
            )
            .into());
        }
    };
    Err(format!(
        "seat {seat} is tainted: run {} left survivors (pids {:?}); stop them, then run {}",
        taint.id,
        taint.pids,
        next("")
    )
    .into())
}

/// Refuses a run waiting for the host lease while a taint says its survivors
/// may hold it exclusively (or its gate): they would keep the wait going.
pub fn check_host(root: &Path, local: bool) -> Result<()> {
    let entries = match fs::read_dir(root.join("taint")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("whether any seat is tainted is unknown: {e}").into()),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(seat) = name.strip_suffix(".json") else {
            continue;
        };
        let holds_host = fs::read(entry.path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Taint>(&bytes).ok())
            // Unreadable: what its survivors hold is unknown.
            .is_none_or(|taint| {
                taint
                    .locks
                    .is_some_and(|locks| locks.host == "exclusive" || locks.gate)
            });
        if holds_host {
            return Err(format!(
                "the host lease is held by survivors of tainted seat {seat}; stop them, then run {}",
                crate::seat::recover_hint(seat, local, "")
            )
            .into());
        }
    }
    Ok(())
}

/// Refuses an exclusive run while any seat under this root is tainted: its
/// survivors may still hold the host lease, so exclusivity cannot be claimed.
pub fn check_exclusive(root: &Path, local: bool) -> Result<()> {
    let tainted = match fs::read_dir(root.join("taint")) {
        Ok(entries) => entries.flatten().find_map(|e| {
            e.file_name()
                .to_string_lossy()
                .strip_suffix(".json")
                .map(str::to_owned)
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("whether any seat is tainted is unknown: {e}").into()),
    };
    match tainted {
        None => Ok(()),
        Some(seat) => Err(format!(
            "an exclusive run cannot claim the host while seat {seat} is tainted; stop its survivors, then run {}",
            crate::seat::recover_hint(&seat, local, "")
        )
        .into()),
    }
}

/// Called under the seat lock after a session ends. `terminal` is true only
/// for ran-pass, ran-fail and timeout with confirmed session cleanup. Runs
/// after the collect hook, so collection still sees what the run left.
/// Never fails: a release that cannot complete is recorded as unknown
/// custody, with the error, and taints the seat if that can be written.
pub fn release(root: &Path, request: &crate::execution::Request, terminal: bool) -> Release {
    match try_release(root, request, terminal) {
        Ok(release) => release,
        Err(error) => {
            let error = error.to_string();
            let tainted = write(
                root,
                "taint",
                &request.seat,
                &Taint {
                    id: request.id.clone(),
                    reason: format!("custody release failed: {error}"),
                    pids: vec![],
                    survivors: vec![],
                    locks: held_locks(request),
                },
            );
            Release {
                census: Census::Unknown(error.clone()),
                record: if tainted.is_ok() { "taint" } else { "none" }.into(),
                stopped: vec![],
                after: None,
                error: Some(match tainted {
                    Ok(()) => error,
                    Err(taint) => {
                        format!("{error}; the taint could not be written either: {taint}")
                    }
                }),
            }
        }
    }
}

/// The locks a session inherited, so its survivors may hold: only where
/// sessions inherit them (Linux; see `execution::inherited`).
fn held_locks(request: &crate::execution::Request) -> Option<HeldLocks> {
    let exclusive = request.recipe.requires.contains("exclusive");
    cfg!(target_os = "linux").then(|| HeldLocks {
        seat: request.seat.clone(),
        host: if exclusive { "exclusive" } else { "shared" }.into(),
        gate: exclusive,
    })
}

fn try_release(
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
                    locks: held_locks(request),
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
                    locks: held_locks(request),
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
        error: None,
    })
}

/// Stops what this run's census attributes to it: each survivor and all of
/// its descendants. TERM goes to the whole set first (and to process groups
/// led by a member), the census is retaken through the grace period so newly
/// adopted descendants get TERM too, then the same with KILL, then one final
/// census. Returns the pids signalled and that final census. Survivors are
/// unreaped children of this supervisor, so their pids cannot be reused;
/// a deeper descendant is signalled only while its start time still matches.
fn stop(first: &[i32]) -> (Vec<i32>, Census) {
    use nix::sys::signal::Signal;
    let mut stopped: Vec<i32> = vec![];
    let mut current = Census::Survivors(first.to_vec());
    for (signal, grace) in [(Signal::SIGTERM, STOP_GRACE), (Signal::SIGKILL, KILL_GRACE)] {
        let deadline = Instant::now() + grace;
        let mut signalled: Vec<(i32, Option<u128>)> = vec![];
        loop {
            match &current {
                Census::Survivors(pids) if !pids.is_empty() => {
                    let members = attributed(pids);
                    for member in &members {
                        if !signalled.contains(member) {
                            signalled.push(*member);
                            send(*member, &members, signal);
                            if !stopped.contains(&member.0) {
                                stopped.push(member.0);
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
    // What the last KILL batch left: reap it, then the final word.
    std::thread::sleep(Duration::from_millis(50));
    (stopped, census())
}

/// The survivors and every descendant of theirs, with start times.
fn attributed(survivors: &[i32]) -> Vec<(i32, Option<u128>)> {
    let mut members: Vec<i32> = survivors.to_vec();
    let parents = parents();
    let mut frontier = members.clone();
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in &parents {
            if *ppid == parent && !members.contains(pid) {
                members.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    members
        .into_iter()
        .map(|pid| (pid, crate::seat::start_time(pid).ok()))
        .collect()
}

/// Every process's parent, as /proc shows it (Linux; elsewhere none).
fn parents() -> Vec<(i32, i32)> {
    #[cfg(target_os = "linux")]
    {
        fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_string_lossy().parse::<i32>().ok()?;
                let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
                let (_, fields) = stat.rsplit_once(')')?;
                let parent = fields.split_whitespace().nth(1)?.parse().ok()?;
                Some((pid, parent))
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    vec![]
}

/// Signals one member if it is still the process it was, and its process
/// group when the group's leader is itself a member.
fn send(
    member: (i32, Option<u128>),
    members: &[(i32, Option<u128>)],
    signal: nix::sys::signal::Signal,
) {
    use nix::{
        sys::signal::{kill, killpg},
        unistd::{Pid, getpgid},
    };
    let (pid, start) = member;
    if start.is_some() && crate::seat::start_time(pid).ok() != start {
        return;
    }
    let target = Pid::from_raw(pid);
    if let Ok(group) = getpgid(Some(target))
        && members.iter().any(|(leader, _)| *leader == group.as_raw())
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
/// is a subreaper with no children before the run, every surviving
/// descendant is, or sits under, a direct child, and all of them are the
/// run's. Any other wait error leaves custody unknown.
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
