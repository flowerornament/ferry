//! The caller's seat book (SPEC §9, §10): each seat this caller used, so
//! `ferry seats prune` can retire those whose workspace is gone or that are
//! idle. It is append-only, one `O_APPEND` line per accepted run, plus a
//! tombstone per retired seat. It is never rewritten, so concurrent appends
//! are never lost.
use crate::{config, transport};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// One line of the book: a seat used, or (with `retired_ms`) retired.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Line {
    pub key: String,
    #[serde(default)]
    pub place: String,
    #[serde(default)]
    pub repo_path: String,
    #[serde(default)]
    pub seat: String,
    /// When this caller last used the seat; absent on a line written
    /// without it, whose age is then unknown (SPEC §9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_ms: Option<u128>,
    /// The device of the workspace's parent directory when it was used: an
    /// unmounted volume leaves an empty mount point on another device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_device: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_ms: Option<u128>,
}

/// `$XDG_STATE_HOME/ferry/seats.jsonl`, else `~/.local/state/ferry/seats.jsonl`.
fn book() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or("neither XDG_STATE_HOME nor HOME is set")?;
    Ok(base.join("ferry/seats.jsonl"))
}

/// Appends one line in one write, so concurrent callers never interleave.
fn append(line: &Line) -> Result<()> {
    if crate::execution::fail_at("seats-append") {
        return Err("seats-append: injected by FERRY_FAIL_AT".into());
    }
    let path = book()?;
    fs::create_dir_all(path.parent().ok_or("seat book has no directory")?)?;
    let mut bytes = serde_json::to_vec(line)?;
    bytes.push(b'\n');
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?
        .write_all(&bytes)?;
    Ok(())
}

/// Records that a run on `place` used seat `key` of `repo_path`. A local
/// seat lives inside its workspace and goes with it: it is not recorded.
pub fn record(key: &str, place: &str, repo_path: &Path, seat: &str) -> Result<()> {
    if place == "local" {
        return Ok(());
    }
    let parent = repo_path
        .parent()
        .ok_or("the workspace has no parent directory")?;
    append(&Line {
        key: key.to_owned(),
        place: place.to_owned(),
        repo_path: repo_path.display().to_string(),
        seat: seat.to_owned(),
        last_used_ms: Some(crate::receipt::now_ms()),
        parent_device: Some(fs::metadata(parent)?.dev()),
        retired_ms: None,
    })
}

/// Whether the workspace is provably gone: its name is absent from a parent
/// directory that can be listed and is on the device recorded with it.
/// Anything else (a denied or failed lookup, a missing parent, another
/// device) cannot prove it, and the seat is kept: retiring a live
/// workspace's seat would destroy its warm state.
fn gone(line: &Line) -> std::result::Result<bool, String> {
    let path = Path::new(&line.repo_path);
    match fs::symlink_metadata(path) {
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let parent = path.parent().ok_or("no parent directory")?;
    let listed = fs::read_dir(parent).and_then(|_| fs::metadata(parent));
    let device = listed
        .map_err(|e| format!("parent {}: {e}", parent.display()))?
        .dev();
    if line.parent_device != Some(device) {
        return Err(format!(
            "parent {} is not on the device recorded with the workspace (unmounted?)",
            parent.display()
        ));
    }
    Ok(true)
}

/// Records the seat unless the book already has it live: a completion
/// retries an append that failed at acceptance.
pub fn ensure(key: &str, place: &str, repo_path: &Path, seat: &str) -> Result<()> {
    if latest()?
        .get(&(place.to_owned(), key.to_owned()))
        .is_some_and(|line| line.retired_ms.is_none())
    {
        return Ok(());
    }
    record(key, place, repo_path, seat)
}

/// The latest line per place and key, in file order (later lines win),
/// carrying the newest `last_used_ms` of that place and key's lines.
/// Unreadable lines are skipped.
pub(crate) fn latest() -> Result<BTreeMap<(String, String), Line>> {
    let text = match fs::read_to_string(book()?) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut seats: BTreeMap<(String, String), Line> = BTreeMap::new();
    for line in text.lines() {
        if let Ok(mut line) = serde_json::from_str::<Line>(line) {
            let at = (line.place.clone(), line.key.clone());
            if let Some(before) = seats.get(&at) {
                line.last_used_ms = line.last_used_ms.max(before.last_used_ms);
            }
            seats.insert(at, line);
        }
    }
    Ok(seats)
}

/// How one seat a prune considered came out (SPEC §9: retired, busy,
/// used, unknown or failed, plus exclusive and an old place).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    Retired,
    /// Nothing left under the key: an earlier retire took it.
    Gone,
    Busy,
    Used,
    Exclusive,
    Unknown,
    Failed,
    Unreachable,
}

/// What a prune did, seat by seat, and the bytes it retired.
#[derive(Debug, Default)]
pub struct Pruned {
    pub said: Vec<(Kept, String)>,
    pub bytes: u64,
}
impl Pruned {
    fn note(&mut self, kept: Kept, line: String) {
        self.said.push((kept, line));
    }
    fn count(&self, kept: Kept) -> usize {
        self.said.iter().filter(|(k, _)| *k == kept).count()
    }
    /// 0 when every seat was retired or kept by rule (busy, used,
    /// exclusive, unknown age); 125 if a place refused otherwise; 255 if a
    /// place could not be reached.
    pub fn code(&self) -> i32 {
        if self.count(Kept::Unreachable) > 0 {
            255
        } else if self.count(Kept::Failed) > 0 {
            125
        } else {
            0
        }
    }
}

/// Retires this caller's seats whose workspace is gone, and with `idle`
/// those it last used longer ago than that, through each place's `retire`
/// verb, and prints one line per seat it considered.
pub fn prune(only: Option<&str>, hosts: &config::Machines, idle: Option<Duration>) -> Result<i32> {
    let pruned = prune_until(only, hosts, idle, None)?;
    for (_, line) in &pruned.said {
        println!("{line}");
    }
    Ok(pruned.code())
}

/// The prune itself; past `stop` (if any) it starts no new retire.
pub fn prune_until(
    only: Option<&str>,
    hosts: &config::Machines,
    idle: Option<Duration>,
    stop: Option<Instant>,
) -> Result<Pruned> {
    let mut pruned = Pruned::default();
    let now = crate::receipt::now_ms();
    for line in latest()?.into_values() {
        if line.retired_ms.is_some() || only.is_some_and(|place| place != line.place) {
            continue;
        }
        let workspace = gone(&line);
        let age = line.last_used_ms.map(|ms| now.saturating_sub(ms));
        let meant = match (&workspace, idle) {
            (Ok(true), _) => crate::place::Meant::default(),
            (_, Some(idle)) if age.is_some_and(|age| age >= idle.as_millis()) => {
                crate::place::Meant {
                    idle_ms: Some(idle.as_millis()),
                    once_id: None,
                }
            }
            (_, Some(_)) if age.is_none() => {
                pruned.note(
                    Kept::Unknown,
                    format!(
                        "kept {} of {} on {}: its age is unknown (the book has no last use)",
                        line.seat, line.repo_path, line.place
                    ),
                );
                continue;
            }
            (Err(why), _) => {
                pruned.note(
                    Kept::Unknown,
                    format!(
                        "kept {} on {}: workspace {} unknown: {why}",
                        line.seat, line.place, line.repo_path
                    ),
                );
                continue;
            }
            _ => continue,
        };
        if stop.is_some_and(|stop| Instant::now() >= stop) {
            break;
        }
        let why_now = if meant.idle_ms.is_some() {
            format!(
                "unused for {}",
                crate::config::show_duration(Duration::from_millis(
                    u64::try_from(age.unwrap_or_default()).unwrap_or(u64::MAX)
                ))
            )
        } else {
            format!("workspace {} is gone", line.repo_path)
        };
        let said = if let Some(machine) = hosts.host.get(&line.place) {
            transport::retire(&transport::Place::Remote(machine), &line.key, &meant, stop)
        } else {
            Err(format!("unknown place {}", line.place).into())
        };
        let reply = match said {
            Ok(reply) => reply,
            Err(error) => {
                pruned.note(
                    Kept::Unreachable,
                    format!("failed {} on {}: {error}", line.seat, line.place),
                );
                continue;
            }
        };
        if reply["skipped"] == "budget" {
            break;
        }
        settle(&reply, line, &why_now, &mut pruned)?;
    }
    Ok(pruned)
}

/// Notes what a place's retire reply did to one seat, and books a seat
/// that is now retired (SPEC §9: retired, already gone, or kept and why).
fn settle(reply: &serde_json::Value, line: Line, why_now: &str, pruned: &mut Pruned) -> Result<()> {
    // Nothing under the key: an earlier retire took it (an older place
    // names it in its refusal's text).
    let absent = reply["refused"] == "absent"
        || reply["why"]
            .as_str()
            .is_some_and(|why| why.starts_with("no seat ") && why.ends_with(" on this place"));
    let said = if absent || reply["gone"] == true {
        (
            Kept::Gone,
            format!(
                "already gone {} of {} on {}",
                line.seat, line.repo_path, line.place
            ),
        )
    } else if reply["retired"].is_string() {
        let bytes = reply["bytes"].as_u64().unwrap_or_default();
        pruned.bytes += bytes;
        let mut said = format!(
            "retired {} of {} on {} ({bytes} bytes; {why_now})",
            line.seat, line.repo_path, line.place
        );
        if let Some(note) = reply["tombstones_left"].as_str() {
            let _ = write!(said, "; {note}");
        }
        (Kept::Retired, said)
    } else {
        let (kept, said) = refusal(reply, &line);
        pruned.note(kept, said);
        return Ok(());
    };
    pruned.note(said.0, said.1);
    append(&Line {
        retired_ms: Some(crate::receipt::now_ms()),
        ..line
    })
}

/// How a place's reply that retired nothing reads, and what it counts as.
fn refusal(reply: &serde_json::Value, line: &Line) -> (Kept, String) {
    let why = reply["why"].as_str().unwrap_or_default();
    let (kept, verb) = match reply["refused"].as_str() {
        Some("busy") => (Kept::Busy, "skipped"),
        Some("used") => (Kept::Used, "skipped (used since)"),
        Some("exclusive") => (Kept::Exclusive, "skipped (exclusive run)"),
        // An older place's busy refusal names the seat busy.
        Some(_) if why.starts_with("seat busy") => (Kept::Busy, "skipped"),
        _ if reply["unknown"].is_string() => (Kept::Unknown, "kept (unknown)"),
        _ => (Kept::Failed, "failed"),
    };
    (
        kept,
        format!(
            "{verb} {} of {} on {}: {why}",
            line.seat, line.repo_path, line.place
        ),
    )
}

/// Prunes `place`'s idle seats by itself after a run (SPEC §9, "idle,
/// without remembering"): at most once an hour per place, claimed under an
/// flock on the caller's `pruned.lock` before pruning, and starting no new
/// retire after `AUTO_PRUNE_BUDGET`. Prints one line on stderr when it
/// pruned or failed; never changes the run's exit code or output.
pub fn prune_by_itself(place: &str, hosts: &config::Machines) {
    let Some(days) = hosts.delete_unused_checkouts_after_days else {
        return;
    };
    let started = Instant::now();
    let said = match claim_hour(place) {
        Ok(false) => return,
        Ok(true) => prune_until(
            Some(place),
            hosts,
            Some(Duration::from_secs(days * 86_400)),
            Some(
                started
                    + crate::execution::debug_bound("FERRY_AUTO_PRUNE_BUDGET_MS")
                        .map_or(crate::limits::AUTO_PRUNE_BUDGET, Duration::from_millis),
            ),
        )
        .map(|pruned| {
            let failed = pruned.count(Kept::Failed) + pruned.count(Kept::Unreachable);
            let kept =
                pruned.said.len() - pruned.count(Kept::Retired) - pruned.count(Kept::Gone) - failed;
            format!(
                "retired {} unused for {days}d ({} bytes), kept {kept}, failed {failed}",
                pruned.count(Kept::Retired),
                pruned.bytes
            ) + &pruned
                .said
                .iter()
                .find(|(k, _)| matches!(k, Kept::Failed | Kept::Unreachable))
                .map(|(_, line)| format!("; {line}"))
                .unwrap_or_default()
        }),
        Err(error) => Err(error),
    };
    let line = match said {
        Ok(said) => format!("ferry: pruned {place}: {said}, in {:?}", started.elapsed()),
        Err(error) => format!("ferry: pruning {place} failed: {error}"),
    };
    crate::execution::warn(&line);
}

/// Claims this hour's prune of `place`: under a held flock on `pruned.lock`,
/// reads `pruned.json`, and only if `place`'s time is over an hour old writes
/// now (renamed into place whole). A lock held by another run, or a recent
/// time, is no claim.
fn claim_hour(place: &str) -> Result<bool> {
    use fs2::FileExt;
    let dir = book()?
        .parent()
        .ok_or("seat book has no directory")?
        .to_owned();
    fs::create_dir_all(&dir)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("pruned.lock"))?;
    match lock.try_lock_exclusive() {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    crate::execution::pause_at("prune-claim", &dir.join("prune-claim"));
    let path = dir.join("pruned.json");
    let mut times: BTreeMap<String, u128> = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(e.into()),
    };
    let now = crate::receipt::now_ms();
    if times
        .get(place)
        .is_some_and(|last| now.saturating_sub(*last) < crate::limits::AUTO_PRUNE_EVERY.as_millis())
    {
        return Ok(false);
    }
    times.insert(place.to_owned(), now);
    let tmp = dir.join("pruned.json.tmp");
    fs::write(&tmp, serde_json::to_vec(&times)?)?;
    fs::rename(&tmp, &path)?;
    Ok(true)
}
