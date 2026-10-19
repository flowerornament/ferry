//! Run history (SPEC §9, "Bound on run history"): with
//! `delete_run_history_after_days = <n>` in `host.toml`, each run, once its
//! record is final and its own locks are closed, deletes the history of
//! final runs submitted more than `<n>` days ago, oldest first.
//!
//! - It passes the host's gate and takes a shared `host` lease by try, as a
//!   shared run does, so it never deletes under an exclusive run nor jumps
//!   ahead of one waiting: then it is skipped.
//! - A run's history is `runs/<id>/` and `records/<id>.json`; the folder goes
//!   first, so a run deleted in part keeps its record and is found again.
//! - An old reservation is reconciled first (a live supervisor's is never
//!   touched); a final record naming a survivor still alive is kept.
//! - `retired-<key>.json` ages by its `retired_ms`, `history-<ms>-<run>.json`
//!   by its `ms`, and a temporary file a crashed write left by its mtime.
//! - It lists names only, oldest first by the time a name carries (names
//!   with none last), never reads a name younger than the cutoff plus an hour,
//!   then reads and deletes entry by entry, checking [`HISTORY_BUDGET`]
//!   between entries; then it stops starting work, and the next run goes on.
//!
//! A pass that deleted anything writes `records/history-<ms>-<run>.json`.
use crate::{admission, execution, limits::HISTORY_BUDGET, receipt, record};
use std::{
    fmt::Write as _,
    fs,
    os::unix::fs::MetadataExt,
    path::Path,
    time::{Duration, Instant},
};

const DAY_MS: u128 = 86_400_000;

/// How far a caller's clock may run behind the host's before its runs'
/// names look younger than they are (SPEC §9).
const SKEW_MS: u128 = 3_600_000;

/// The pass's account: what it deleted, what it kept and why it stopped.
#[derive(Default)]
struct Pass {
    runs: u64,
    files: u64,
    live: u64,
    read: u64,
    failed: Option<String>,
    spent: bool,
}

/// Deletes the run history older than `days` under `root`, after run `own`
/// is final, and says what it did in one line for the supervisor's log.
pub fn delete_old(root: &Path, own: &str, days: u64) -> String {
    execution::pause_at(
        "history-start",
        &record::run_dir(root, own).join("history-start"),
    );
    let started = Instant::now();
    let budget = execution::debug_bound("FERRY_HISTORY_BUDGET_MS")
        .map_or(HISTORY_BUDGET, Duration::from_millis);
    let until = started + budget;
    let lease = match crate::lock::try_shared_host(root) {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            return "ferry: run history: skipped: an exclusive run holds or waits for this place"
                .into();
        }
        Err(error) => return format!("ferry: run history: skipped: {error}"),
    };
    let cutoff = receipt::now_ms().saturating_sub(u128::from(days) * DAY_MS);
    let mut pass = Pass::default();
    let boot = record::boot_id();
    let names = names(root).unwrap_or_else(|why| {
        pass.failed = Some(why);
        vec![]
    });
    // A name younger than this is never read: its entry is younger than
    // the cutoff, allowing an hour for a caller's clock behind the host's.
    let young = cutoff + SKEW_MS;
    // Debug builds: a budget in entries, so a control can make one pass's
    // work smaller than what is there without depending on time.
    let mut entries = execution::debug_bound::<u64>("FERRY_HISTORY_MAX_ENTRIES");
    for (made, name) in names {
        if Instant::now() >= until || entries == Some(0) {
            pass.spent = true;
            break;
        }
        if made.is_some_and(|ms| ms >= young) {
            continue;
        }
        entries = entries.map(|left| left - 1);
        let aged = Aged {
            root,
            own,
            cutoff,
            boot: boot.as_deref(),
            until,
        };
        if let Err(why) = aged.consider(&name, &mut pass) {
            pass.failed.get_or_insert(why);
        }
        execution::pause_at("history-pause", &record::run_dir(root, own).join("history"));
    }
    if pass.runs + pass.files > 0 {
        let ms = receipt::now_ms();
        let account = serde_json::json!({
            "ms": ms,
            "run": own,
            "deleted": pass.runs,
            "older_than_days": days,
        });
        let path = root
            .join("records")
            .join(format!("history-{ms}-{own}.json"));
        if let Err(error) = record::write_new(&path, account.to_string().as_bytes(), None) {
            pass.failed
                .get_or_insert(format!("writing {}: {error}", path.display()));
        }
    }
    drop(lease);
    let mut line = format!(
        "ferry: run history: deleted {} runs and {} other records older than {days} days, kept {} with a live survivor, read {}, in {:?}",
        pass.runs,
        pass.files,
        pass.live,
        pass.read,
        started.elapsed()
    );
    if pass.spent {
        let _ = write!(
            line,
            "; stopped at its {budget:?} budget, the rest waits for the next run"
        );
    }
    if let Some(why) = pass.failed {
        let _ = write!(line, "; failed: {why}");
    }
    line
}

/// The names in `records/`, each with the time it carries (a run id starts
/// with its submit time in ms, an account with its own), oldest first, so a
/// large `records/` is worked from its old end. A pass's budget can still be
/// spent on what it must keep (SPEC §9): a bound per pass, not a promise
/// that each deletes something. Names that carry no time come after the rest:
/// temporary files (judged by a stat, never read), then the others
/// (retirement records), so kept entries never hold up an old run. Only
/// names are listed here.
fn names(root: &Path) -> Result<Vec<(Option<u128>, String)>, String> {
    let entries = match fs::read_dir(root.join("records")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        entries => entries.map_err(|e| format!("listing records: {e}"))?,
    };
    let mut names: Vec<(Option<u128>, String)> = entries
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let digits: String = name
                .strip_prefix("history-")
                .unwrap_or(&name)
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            (digits.parse().ok(), name)
        })
        .collect();
    names.sort_by_key(|(made, name)| match made {
        Some(ms) => (0, *ms),
        None if name.starts_with('.') => (1, 0),
        None => (2, 0),
    });
    Ok(names)
}

/// One pass's view: what is old, on this boot, until when.
struct Aged<'a> {
    root: &'a Path,
    own: &'a str,
    cutoff: u128,
    boot: Option<&'a str>,
    until: Instant,
}

impl Aged<'_> {
    /// Reads one entry of `records/` and, if it is older than the cutoff,
    /// deletes it (a run: its folder, then its record). Anything unreadable
    /// or of no known kind is kept.
    fn consider(&self, name: &str, pass: &mut Pass) -> Result<(), String> {
        let path = self.root.join("records").join(name);
        let read = |pass: &mut Pass| {
            pass.read += 1;
            fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        };
        let old = |value: &serde_json::Value, field: &str| {
            value[field]
                .as_u64()
                .map(u128::from)
                .is_some_and(|ms| ms < self.cutoff)
        };
        let file = |pass: &mut Pass| {
            remove_file(&path)
                .map(|removed| pass.files += u64::from(removed))
                .map_err(|why| format!("deleting records/{name}: {why}"))
        };
        if name.starts_with('.') && path.extension().is_some_and(|ext| ext == "tmp") {
            // A crashed write's temporary file: never read as a record.
            let made = fs::symlink_metadata(&path)
                .ok()
                .and_then(|meta| u128::try_from(meta.mtime()).ok())
                .map(|seconds| seconds * 1000);
            if made.is_some_and(|ms| ms < self.cutoff) {
                file(pass)?;
            }
        } else if name.starts_with("retired-") || name.starts_with("history-") {
            let field = if name.starts_with("retired-") {
                "retired_ms"
            } else {
                "ms"
            };
            if read(pass).is_some_and(|value| old(&value, field)) {
                file(pass)?;
            }
        } else if let Some(id) = name.strip_suffix(".reserved.json") {
            // Reconciled by the ordinary rule: a live supervisor's
            // reservation stays; a gone one's run is final, and ages.
            if read(pass).is_some_and(|value| old(&value, "submitted_ms"))
                && crate::config::identifier(id).is_ok()
                && id != self.own
                && let Ok(Some(bytes)) = record::reconcile(self.root, id)
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            {
                self.run(id, &value, pass)?;
            }
        } else if let Some(id) = name.strip_suffix(".json")
            && let Some(value) = read(pass)
            && old(&value, "submitted_ms")
            && value["state"] == "final"
            && crate::config::identifier(id).is_ok()
            && id != self.own
        {
            self.run(id, &value, pass)?;
        }
        Ok(())
    }

    /// Deletes the old final run `id` (its `record`), unless a survivor it
    /// names is still alive.
    fn run(&self, id: &str, record: &serde_json::Value, pass: &mut Pass) -> Result<(), String> {
        if survivor_alive(record, self.boot) {
            pass.live += 1;
            return Ok(());
        }
        match delete_run(self.root, id, self.until)? {
            Some(removed) => pass.runs += u64::from(removed),
            None => pass.spent = true,
        }
        Ok(())
    }
}

/// Whether a final record names a survivor that is still alive: the same pid
/// with the same start time, or a live pid alone when its start was not
/// recorded (pid reuse then keeps a record, never deletes one). A record
/// from another boot names none alive; `"unknown"` survivors are no reason
/// to keep it.
fn survivor_alive(record: &serde_json::Value, boot: Option<&str>) -> bool {
    if let (Some(then), Some(now)) = (record["boot_id"].as_str(), boot)
        && then != now
    {
        return false;
    }
    let Some(survivors) = record["custody"]["survivors"].as_array() else {
        return false;
    };
    survivors.iter().any(|survivor| {
        let Some(pid) = survivor["pid"]
            .as_i64()
            .and_then(|pid| i32::try_from(pid).ok())
        else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        match survivor["start"].as_u64() {
            Some(start) => record::start_time(pid).is_ok_and(|now| now == u128::from(start)),
            // Alive if it can be signalled, or exists as another user's.
            None => matches!(
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
                Ok(()) | Err(nix::errno::Errno::EPERM)
            ),
        }
    })
}

/// Deletes run `id`'s folder, entry by entry, then any reservation a
/// crashed finalize left (else a reconcile would bring the run back as
/// `lost`), then its record: `Some(true)` when this pass removed the record
/// (the run counts for it), `Some(false)` when another pass already had,
/// `None` when `until` came first (the record stays, so the run is found
/// again). Each entry goes through `admission::remove`: proven inside the
/// root and on its device, a directory removed by its handles, so a link
/// swapped in is unlinked, never followed.
fn delete_run(root: &Path, id: &str, until: Instant) -> Result<Option<bool>, String> {
    let rel = Path::new("runs").join(id);
    let shown = |e: &dyn std::fmt::Display| format!("deleting runs/{id}: {e}");
    if let Some(why) = admission::outside(root, &rel) {
        return Err(shown(&format!("{why}: not deleted")));
    }
    match fs::read_dir(root.join(&rel)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(shown(&e)),
        Ok(entries) => {
            for entry in entries {
                if Instant::now() >= until {
                    return Ok(None);
                }
                let entry = entry.map_err(|e| shown(&e))?;
                admission::remove(root, &rel.join(entry.file_name())).map_err(|e| shown(&e))?;
            }
            admission::remove(root, &rel).map_err(|e| shown(&e))?;
        }
    }
    if execution::fail_at("history-record") {
        return Err("history-record: injected by FERRY_FAIL_AT".into());
    }
    remove_file(&record::reserved_path(root, id))
        .map_err(|why| format!("deleting the reservation of run {id}: {why}"))?;
    if execution::fail_at("history-final") {
        return Err("history-final: injected by FERRY_FAIL_AT".into());
    }
    remove_file(&record::final_path(root, id))
        .map(Some)
        .map_err(|why| format!("deleting the record of run {id}: {why}"))
}

/// Removes one file: `true` if this call removed it, `false` if it was
/// already gone.
fn remove_file(path: &Path) -> Result<bool, String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::names;
    use std::fs;

    /// Names with a time come first, oldest first (an account by its own
    /// `ms`); then temporary files, which need no read; then the rest.
    #[test]
    fn names_go_by_their_time_then_temporaries_then_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let records = root.path().join("records");
        fs::create_dir_all(&records).unwrap();
        // Twenty runs whose name order is not their time order, so no
        // listing order matches the sorted one by chance.
        let runs: Vec<String> = (1..=20).map(|n| format!("{}-r.json", n * 7)).collect();
        let others = ["retired-k.json", ".7-r.json.1.0.tmp", "history-50-x.json"];
        for name in runs.iter().map(String::as_str).chain(others) {
            fs::write(records.join(name), "").unwrap();
        }
        let listed: Vec<String> = names(root.path())
            .unwrap()
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        let mut expected: Vec<&str> = runs.iter().map(String::as_str).collect();
        expected.insert(7, "history-50-x.json");
        expected.extend([".7-r.json.1.0.tmp", "retired-k.json"]);
        assert_eq!(listed, expected);
    }
}
