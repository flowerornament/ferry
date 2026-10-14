//! Custody (SPEC §7): at the end of a run, its session is stopped (TERM, a
//! grace, then KILL); then one marker pass lists the processes still
//! carrying the run's `FERRY_ID`, however they left the session. The record
//! names both, or says `unknown`. It only reports: a survivor that keeps the
//! seat lock makes the next run's busy refusal name the run it outlived.
use crate::execution::fail_at;
use nix::{sys::signal::kill, unistd::Pid};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// A process of the run's session alive after the stop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Survivor {
    pub pid: i32,
    /// `u64`, not `u128`: an untagged enum cannot carry a `u128` through
    /// serde's buffering, and a start time fits.
    #[serde(default)]
    pub start: Option<u64>,
    /// Its executable as it was run (`argv[0]`, capped), when the marker pass
    /// read it: never its arguments, which may carry secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// The survivors, or `"unknown"` when the session could not be listed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Survivors {
    Listed(Vec<Survivor>),
    Unknown(Unknown),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Unknown {
    Unknown,
}

/// The custody evidence a run leaves in its record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Release {
    /// `session`: the stop reaches the run's session. A systemd scope tier
    /// is not built yet.
    pub tier: String,
    pub survivors: Survivors,
    /// What failed during a session stop (the stop went on regardless), and
    /// why the marker pass was unknown, with what it saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The session stops' failures alone: the record's reason names these,
    /// not an unknown marker pass, which is the survivors' own status.
    #[serde(skip)]
    pub stop_error: Option<String>,
}

impl Default for Release {
    /// A run that spawned no session stopped nothing and left nothing.
    fn default() -> Self {
        let survivors = Survivors::Listed(vec![]);
        Self {
            tier: "session".into(),
            survivors,
            error: None,
            stop_error: None,
        }
    }
}

/// What this run's session stops found (recipe, then collect).
static FOUND: Mutex<Option<Release>> = Mutex::new(None);

/// What one session stop came to: the members alive after it (`None` when
/// the last listing failed), and every error on the way.
pub struct Stopped {
    pub survivors: Option<Vec<Survivor>>,
    pub errors: Vec<String>,
}

/// Adds one session stop's result. Survivors are unknown only when a final
/// listing failed; errors on the way are the release's error either way.
pub fn note(stopped: Stopped) {
    let Ok(mut found) = FOUND.lock() else {
        return;
    };
    let release = found.get_or_insert_with(Release::default);
    match (stopped.survivors, &mut release.survivors) {
        (Some(more), Survivors::Listed(list)) => list.extend(more),
        (Some(_), Survivors::Unknown(_)) => {}
        (None, _) => release.survivors = Survivors::Unknown(Unknown::Unknown),
    }
    if !stopped.errors.is_empty() {
        crate::receipt::note(&mut release.error, stopped.errors.join("; "));
        crate::receipt::note(&mut release.stop_error, stopped.errors.join("; "));
    }
}

/// The run's custody, taken once at its end.
pub fn take() -> Release {
    FOUND
        .lock()
        .ok()
        .and_then(|mut found| found.take())
        .unwrap_or_default()
}

/// Adds the marker pass to this run's custody: its survivors merge with the
/// session's (one entry per pid and start), or the survivors become
/// `unknown`, with what was seen kept in the error. A read can block on a
/// process stuck in the kernel, so the pass runs on its own thread, bounded
/// by `MARKER_BOUND` and the run's deadline; past that it is abandoned (the
/// supervisor exits soon after) and the survivors are `unknown`.
pub fn mark(id: &str, deadline_ms: u128) {
    let left = deadline_ms.saturating_sub(crate::receipt::now_ms());
    let bound = crate::limits::MARKER_BOUND.min(std::time::Duration::from_millis(
        u64::try_from(left).unwrap_or(u64::MAX),
    ));
    let (sent, received) = std::sync::mpsc::channel();
    let id = id.to_owned();
    std::thread::spawn(move || sent.send(marked(&id)));
    let marked = received
        .recv_timeout(bound)
        .unwrap_or_else(|_| Err(format!("marker pass timed out after {bound:?}")));
    let Ok(mut found) = FOUND.lock() else {
        return;
    };
    let release = found.get_or_insert_with(Release::default);
    match (marked, &mut release.survivors) {
        (Ok(more), Survivors::Listed(list)) => {
            for survivor in more {
                let same = |s: &&mut Survivor| {
                    s.pid == survivor.pid && (s.start.is_none() || s.start == survivor.start)
                };
                match list.iter_mut().find(same) {
                    Some(listed) => listed.command = survivor.command,
                    None => list.push(survivor),
                }
            }
        }
        (Ok(more), Survivors::Unknown(_)) => {
            if !more.is_empty() {
                let pids: Vec<_> = more.iter().map(|s| s.pid).collect();
                crate::receipt::note(&mut release.error, format!("marked {pids:?}"));
            }
        }
        (Err(why), survivors) => {
            if let Survivors::Listed(list) = survivors
                && !list.is_empty()
            {
                let pids: Vec<_> = list.iter().map(|s| s.pid).collect();
                crate::receipt::note(&mut release.error, format!("session survivors {pids:?}"));
            }
            crate::receipt::note(&mut release.error, format!("marker pass: {why}"));
            release.survivors = Survivors::Unknown(Unknown::Unknown);
        }
    }
}

/// The marker pass (SPEC §7): the place user's processes born at or after
/// this supervisor (so everything the run started; a coarse clock rounds
/// toward including more) that carry `FERRY_ID=<id>`. An eligible process
/// that cannot be read, or whose start changes during the read, makes the
/// pass unknown: the error names it and what matched before.
fn marked(id: &str) -> Result<Vec<Survivor>, String> {
    let marker = format!("FERRY_ID={id}");
    let me = i32::try_from(std::process::id()).map_err(|e| e.to_string())?;
    let (uid, since) = owner(me).map_err(|e| format!("own start: {e}"))?;
    while fail_at("marker-hang") {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    let pids = if fail_at("marker-list") {
        Err("injected by FERRY_FAIL_AT".into())
    } else {
        crate::run::process_ids()
    }
    .map_err(|e| format!("list processes: {e}"))?;
    let mut found = vec![];
    // Each reason with the pids it was given for.
    let mut unreadable = std::collections::BTreeMap::<String, Vec<i32>>::new();
    for pid in pids {
        match seen(pid, uid, since, &marker) {
            Seen::Other => {}
            Seen::Marked(survivor) => found.push(survivor),
            Seen::Unreadable(why) => unreadable.entry(why).or_default().push(pid),
        }
    }
    if unreadable.is_empty() {
        return Ok(found);
    }
    let reasons: Vec<_> = unreadable
        .iter()
        .map(|(why, pids)| format!("{why}: {} {:?}", pids.len(), &pids[..pids.len().min(3)]))
        .collect();
    let matched: Vec<_> = found.iter().map(|s| s.pid).collect();
    Err(format!(
        "unreadable, {}; marked {matched:?}",
        reasons.join("; ")
    ))
}

enum Seen {
    Other,
    Unreadable(String),
    Marked(Survivor),
}

fn seen(pid: i32, uid: u32, since: u128, marker: &str) -> Seen {
    let gone = || kill(Pid::from_raw(pid), None) == Err(nix::errno::Errno::ESRCH);
    let Ok((owner_uid, start)) = owner(pid) else {
        return Seen::Other;
    };
    if owner_uid != uid || start < since || crate::run::zombie(pid) {
        return Seen::Other;
    }
    let read = if fail_at("marker-read") {
        Err("injected by FERRY_FAIL_AT".into())
    } else {
        environment(pid)
    };
    let unreadable = |why: String| {
        if gone() || foreign(pid, uid, parent, owner) {
            Seen::Other
        } else {
            Seen::Unreadable(why)
        }
    };
    let (environment, command) = match read {
        Ok(read) => read,
        Err(why) => return unreadable(why),
    };
    if fail_at("marker-restart") || owner(pid).map(|(_, again)| again).ok() != Some(start) {
        return unreadable("its start changed during the read".into());
    }
    if !environment.iter().any(|variable| variable == marker) {
        return Seen::Other;
    }
    let mut command = command;
    if let Some((cut, _)) = command.char_indices().nth(256) {
        command.truncate(cut);
    }
    Seen::Marked(Survivor {
        pid,
        start: u64::try_from(start).ok(),
        command: Some(command),
    })
}

/// A process's owner and start, as `record::start_time` counts it. The real
/// uid from `status`: `/proc/<pid>` itself shows root for a non-dumpable
/// process, which must read as unreadable, not as another user's.
#[cfg(not(target_os = "macos"))]
fn owner(pid: i32) -> std::io::Result<(u32, u128)> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().next()?.parse().ok())
        .ok_or_else(|| std::io::Error::other("no Uid in status"))?;
    let start = crate::record::start_time(pid).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok((uid, start))
}

#[cfg(target_os = "macos")]
fn owner(pid: i32) -> std::io::Result<(u32, u128)> {
    let info = libproc::proc_pid::pidinfo::<libproc::bsd_info::BSDInfo>(pid, 0)
        .map_err(std::io::Error::other)?;
    Ok((info.pbi_uid, crate::record::bsd_start(&info)))
}

/// Whether an unreadable process provably did not come from the run (SPEC
/// §7): its parent is neither pid 1 nor a process of this user. A run's
/// process has another of the run's, pid 1 or this user's subreaper as its
/// parent; sshd-session's is root's sshd. Only a parent read twice the same
/// (its pid, then its start) proves it: a failed or changing read counts the
/// process as unreadable.
fn foreign(
    pid: i32,
    uid: u32,
    parent: impl Fn(i32) -> std::io::Result<i32>,
    owner: impl Fn(i32) -> std::io::Result<(u32, u128)>,
) -> bool {
    let Ok(up) = parent(pid) else {
        return false;
    };
    let Ok((parent_uid, start)) = owner(up) else {
        return false;
    };
    let same = parent(pid).ok() == Some(up) && owner(up).ok() == Some((parent_uid, start));
    same && up != 1 && parent_uid != uid
}

/// A process's parent: readable even when the process is not dumpable.
#[cfg(not(target_os = "macos"))]
fn parent(pid: i32) -> std::io::Result<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(1)?.parse().ok())
        .ok_or_else(|| std::io::Error::other("no parent in stat"))
}

#[cfg(target_os = "macos")]
fn parent(pid: i32) -> std::io::Result<i32> {
    let info = libproc::proc_pid::pidinfo::<libproc::bsd_info::BSDInfo>(pid, 0)
        .map_err(std::io::Error::other)?;
    i32::try_from(info.pbi_ppid).map_err(std::io::Error::other)
}

/// A process's exec-time environment and `argv[0]`.
#[cfg(not(target_os = "macos"))]
fn environment(pid: i32) -> Result<(Vec<String>, String), String> {
    let read = |what: &str| {
        std::fs::read(format!("/proc/{pid}/{what}")).map_err(|e| format!("{what}: {e}"))
    };
    let strings = |bytes: &[u8]| -> Vec<String> {
        bytes
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    };
    let environment = strings(&read("environ")?);
    let command = strings(&read("cmdline")?).into_iter().next();
    Ok((environment, command.unwrap_or_default()))
}

#[cfg(target_os = "macos")]
fn environment(pid: i32) -> Result<(Vec<String>, String), String> {
    parse_procargs(&procargs(pid).map_err(|e| format!("procargs: {e}"))?)
}

/// `KERN_PROCARGS2` (what `ps -E` reads): `argc`, the exec path, padding,
/// `argc` arguments, then the environment. macOS withholds the environment
/// of its platform binaries, returning the arguments alone; that, and any
/// truncated or malformed buffer, is unreadable, never "no marker".
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_procargs(buffer: &[u8]) -> Result<(Vec<String>, String), String> {
    let malformed = || "malformed procargs".to_owned();
    let (count, rest) = buffer.split_first_chunk::<4>().ok_or_else(malformed)?;
    let count = usize::try_from(i32::from_ne_bytes(*count)).map_err(|_| malformed())?;
    let mut strings = rest.split(|b| *b == 0);
    strings
        .next()
        .filter(|path| !path.is_empty())
        .ok_or_else(malformed)?;
    let mut strings = strings.skip_while(|s| s.is_empty());
    let mut argv = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let argument = strings.next().ok_or_else(malformed)?;
        argv.push(String::from_utf8_lossy(argument).into_owned());
    }
    let environment: Vec<String> = strings
        .take_while(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if environment.is_empty() {
        return Err("its environment is withheld".into());
    }
    Ok((environment, argv.into_iter().next().unwrap_or_default()))
}

/// The one `unsafe` in Ferry (a test counts it): `sysctl(3)` for
/// `KERN_PROCARGS2`, which no safe binding offers, into a buffer sized by
/// `KERN_ARGMAX`.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn procargs(pid: i32) -> std::io::Result<Vec<u8>> {
    use nix::libc::{CTL_KERN, KERN_ARGMAX, KERN_PROCARGS2, c_int, c_uint, sysctl};
    let read = |mib: &mut [c_int], buffer: &mut [u8]| {
        let mut size = buffer.len();
        let length = c_uint::try_from(mib.len()).map_err(std::io::Error::other)?;
        // SAFETY: both pointers come from live, exclusively borrowed slices
        // whose lengths are passed with them (`length`, `size`); the kernel
        // writes at most `size` bytes and reports the count back in it; no
        // new value is set. Nothing keeps a pointer past the call, and the
        // count is checked against the buffer before it is used.
        let failed = unsafe {
            sysctl(
                mib.as_mut_ptr(),
                length,
                buffer.as_mut_ptr().cast(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        match (failed, size <= buffer.len()) {
            (0, true) => Ok(size),
            (0, false) => Err(std::io::Error::other(
                "sysctl reported more than its buffer",
            )),
            _ => Err(std::io::Error::last_os_error()),
        }
    };
    let mut argmax = [0_u8; size_of::<c_int>()];
    read(&mut [CTL_KERN, KERN_ARGMAX], &mut argmax)?;
    let argmax = usize::try_from(c_int::from_ne_bytes(argmax)).map_err(std::io::Error::other)?;
    let mut buffer = vec![0_u8; argmax];
    let size = read(&mut [CTL_KERN, KERN_PROCARGS2, pid], &mut buffer)?;
    buffer.truncate(size);
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::parse_procargs;

    fn procargs(argc: i32, strings: &[&str]) -> Vec<u8> {
        let mut bytes = argc.to_ne_bytes().to_vec();
        for (i, s) in strings.iter().enumerate() {
            bytes.extend_from_slice(s.as_bytes());
            bytes.push(0);
            if i == 0 {
                bytes.extend_from_slice(&[0, 0, 0]);
            }
        }
        bytes
    }

    /// An unreadable process is skipped only on a stable, foreign parent;
    /// a parent that cannot be read, or changes, never skips it.
    #[test]
    fn only_a_stable_foreign_parent_skips_an_unreadable_process() {
        use super::foreign;
        use std::{cell::Cell, io};
        let root_sshd = |_| Ok((0, 7));
        assert!(foreign(9, 501, |_| Ok(40), root_sshd));
        assert!(
            !foreign(9, 501, |_| Ok(1), root_sshd),
            "pid 1 adopts the run's"
        );
        assert!(
            !foreign(9, 501, |_| Ok(40), |_| Ok((501, 7))),
            "a subreaper of ours"
        );
        assert!(
            !foreign(9, 501, |_| Err(io::Error::other("no")), root_sshd),
            "no parent read"
        );
        assert!(
            !foreign(9, 501, |_| Ok(40), |_| Err(io::Error::other("no"))),
            "no parent owner"
        );
        let reads = Cell::new(0);
        let moving = |_| {
            reads.set(reads.get() + 1);
            Ok(if reads.get() > 1 { 41 } else { 40 })
        };
        assert!(!foreign(9, 501, moving, root_sshd), "re-parented mid-read");
        let starts = Cell::new(0_u128);
        let restarted = |_| {
            starts.set(starts.get() + 1);
            Ok((0, starts.get()))
        };
        assert!(!foreign(9, 501, |_| Ok(40), restarted), "parent pid reused");
    }

    #[test]
    fn procargs_with_an_environment_parse() {
        let bytes = procargs(2, &["/bin/x", "x", "-y", "FERRY_ID=r1", "HOME=/h"]);
        let (environment, command) = parse_procargs(&bytes).unwrap();
        assert_eq!(environment, ["FERRY_ID=r1", "HOME=/h"]);
        assert_eq!(command, "x");
    }

    /// macOS's platform binaries: the arguments, with no environment.
    #[test]
    fn procargs_without_an_environment_are_unreadable() {
        let bytes = procargs(2, &["/bin/sleep", "/bin/sleep", "30"]);
        assert!(parse_procargs(&bytes).unwrap_err().contains("withheld"));
    }

    #[test]
    fn truncated_or_malformed_procargs_are_unreadable() {
        let bytes = procargs(3, &["/bin/x", "x", "-y", "FERRY_ID=r1"]);
        for cut in [0, 2, 4, 8, 14] {
            assert!(parse_procargs(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        assert!(parse_procargs(&procargs(-1, &["/bin/x", "x", "E=1"])).is_err());
        assert!(parse_procargs(&procargs(5, &["/bin/x", "x", "E=1"])).is_err());
    }

    /// The `unsafe` exception stays one: any other mention of the lint in
    /// a source file (an `allow`, an `expect`, in any form) fails the check.
    #[test]
    fn there_is_one_unsafe_exception() {
        fn walk(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, found);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let lint = ["unsafe", "_code"].concat();
                    found.extend(text.matches(&lint).map(|_| path.clone()));
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut found = vec![];
        for dir in ["src", "tests", "build.rs"] {
            walk(&root.join(dir), &mut found);
        }
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].ends_with("custody.rs"), "{found:?}");
    }
}
