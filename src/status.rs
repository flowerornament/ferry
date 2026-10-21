//! `ferry status` (SPEC §4): one read of a place's state root and process
//! table. It takes no lock (a probe could make a starting run's seat look
//! busy) and changes nothing: a report, never a gate input. A leftover is
//! stopped by its exact pid, by the operator.
use crate::{config, custody, execution::Request, record, transport};
use serde_json::{Value, json};
use std::fmt::Write;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::Path,
    time::{Duration, Instant},
};

/// One walk over every seat shares this budget; past it, sizes are null.
const SIZE_BUDGET: Duration = Duration::from_secs(5);

/// What the place sees now, as one JSON document.
pub fn place(root: &Path) -> Value {
    let mut disk = crate::admission::Admission::default();
    let below = crate::admission::admit(root, &mut disk, None).err();
    // One read of the live runs, shared by every part of the document.
    let live = record::live(root);
    let runs: Vec<Value> = live.iter().map(|r| run(root, r)).collect();
    let live: Vec<_> = live.into_iter().map(|r| r.id).collect();
    json!({
        "disk": {
            "available": crate::admission::available(root).ok(),
            "floor_bytes": disk.floor_bytes,
            "seat_cap_bytes": disk.seat_cap_bytes,
            "policy": disk.policy,
            "refusing": below.map(|e| e.to_string()),
        },
        "gate": gate(root, &runs),
        "runs": runs,
        "seats": seats(root),
        "tombstones": tombstones(root),
        "leftovers": leftovers(root, &live),
    })
}

/// A live run, its phase from the files its supervisor has written so far.
fn run(root: &Path, reservation: &record::Reservation) -> Value {
    let dir = record::run_dir(root, &reservation.id);
    let request = fs::read(dir.join("request.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Request>(&bytes).ok());
    let phase = if dir.join("result.attempted").exists() {
        "running"
    } else if dir.join("output.log").exists() {
        "preparing"
    } else {
        "waiting"
    };
    json!({
        "id": reservation.id,
        "seat": reservation.seat,
        "phase": phase,
        "recipe": request.as_ref().map(|r| &r.recipe.command),
        "exclusive": request.as_ref().map(|r| r.recipe.requires.contains("exclusive")),
        "submitted_ms": reservation.submitted_ms,
        "deadline_ms": request
            .as_ref()
            .map(|r| crate::place::deadline_ms(reservation.submitted_ms, r)),
        "supervisor": reservation.supervisor.pid,
    })
}

/// `host.gate` names the last exclusive run that took it, never cleared:
/// that run is the holder only while it is live; every other live
/// exclusive run is a requester.
fn gate(root: &Path, runs: &[Value]) -> Value {
    let named = fs::read_to_string(root.join("locks/host.gate")).unwrap_or_default();
    let named = named.trim();
    let exclusive: Vec<&str> = runs
        .iter()
        .filter(|run| run["exclusive"] == true)
        .filter_map(|run| run["id"].as_str())
        .collect();
    let holder = match named {
        "" => "none",
        named if exclusive.contains(&named) => named,
        _ => "unknown",
    };
    let requesters: Vec<_> = exclusive.into_iter().filter(|id| *id != holder).collect();
    json!({"holder": holder, "requesters": requesters})
}

/// Each seat: the run its lock file last named and that run's outcome, the
/// cap's last measurement, and its size within the shared walk budget.
fn seats(root: &Path) -> Vec<Value> {
    let until = Instant::now() + SIZE_BUDGET;
    let dev = fs::metadata(root).map(|m| m.dev()).unwrap_or_default();
    // A retire's tombstone is never a seat (SPEC §9): `tombstones` lists it.
    let mut keys: Vec<_> = fs::read_dir(root.join("seats"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    keys.sort();
    keys.into_iter()
        .map(|key| {
            let last = fs::read_to_string(root.join("locks").join(format!("seat.{key}")))
                .unwrap_or_default();
            let last = last.trim();
            let read = |path: &Path| {
                fs::read(path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            };
            let outcome = (config::identifier(last).is_ok())
                .then(|| read(&record::final_path(root, last)))
                .flatten()
                .map_or(Value::Null, |record| record["outcome"].clone());
            let seat = root.join("seats").join(&key);
            json!({
                "key": key,
                "last_run": (!last.is_empty()).then_some(last),
                "outcome": outcome,
                "measured": read(&seat.join("measured")),
                "bytes": crate::admission::size_until(&seat, Some(until), dev),
            })
        })
        .collect()
}

/// A retire's tombstones, left by a delete that failed: the next prune
/// deletes them (SPEC §9). Named only, never measured.
fn tombstones(root: &Path) -> Vec<Value> {
    let mut names: Vec<_> = fs::read_dir(root.join("seats"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(crate::place::TOMBSTONE))
        .collect();
    names.sort();
    names.into_iter().map(Value::from).collect()
}

/// Processes carrying a `FERRY_ID`: a leftover when that run is not live
/// here (final, or its supervisor gone), apart as `record: none` when this
/// state root does not know it; a live run's own are not listed.
/// The pass is bounded as the stop path's is (a read can block on a process
/// stuck in the kernel): past `MARKER_BOUND` it is abandoned, as an error.
fn leftovers(root: &Path, live: &[String]) -> Value {
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || sent.send(custody::census(true)));
    let bound = crate::limits::MARKER_BOUND;
    let census = match received.recv_timeout(bound) {
        Ok(Ok(census)) => census,
        Ok(Err(why)) => return json!({"error": why}),
        Err(_) => return json!({"error": format!("timed out after {bound:?}")}),
    };
    let known = |id: &String| {
        config::identifier(id).is_ok()
            && (record::final_path(root, id).exists() || record::reserved_path(root, id).exists())
    };
    let (mut leftovers, mut unknown) = (vec![], vec![]);
    for (ids, survivor) in census.marked {
        // The id this place knows, else the first, cut as `command` is.
        let mut id = ids.iter().find(|id| known(id)).unwrap_or(&ids[0]).clone();
        let known = known(&id);
        if let Some((cut, _)) = id.char_indices().nth(256) {
            id.truncate(cut);
        }
        let seen = json!({
            "pid": survivor.pid,
            "start": survivor.start,
            "command": survivor.command,
            "run": id,
        });
        if !known {
            unknown.push(seen);
        } else if !live.contains(&id) {
            leftovers.push(seen);
        }
    }
    let unreadable: usize = census.unreadable.values().map(Vec::len).sum();
    json!({"processes": leftovers, "record_none": unknown, "unreadable": unreadable})
}

/// Shows a place's status: this caller labels the seats it knows from its
/// own seat book; other callers' seats keep their key alone.
pub fn show(on: Option<&str>, hosts: &config::Machines, json: bool) -> Result<i32> {
    let place = match on {
        Some(name) => transport::Place::Remote(
            hosts
                .host
                .get(name)
                .ok_or_else(|| format!("unknown place {name}"))?,
        ),
        None => transport::Place::Local(crate::repository::root()?.join(".ferry/local/v1")),
    };
    let mut seen = transport::status(&place)?;
    let book = crate::seats::latest()?;
    // A local place's seats are this workspace's, never booked (SPEC §9):
    // a key is labelled when it is this workspace's key for its seat name.
    let root = on.is_none().then(crate::repository::root).transpose()?;
    for seat in seen["seats"].as_array_mut().into_iter().flatten() {
        let Some(key) = seat["key"].as_str().map(str::to_owned) else {
            continue;
        };
        if let Some(root) = &root {
            let name = key.split_once('-').map_or("", |(_, name)| name);
            if transport::seat_key(root, name).is_ok_and(|own| own == key) {
                seat["workspace"] = json!({"seat": name, "repo_path": root.display().to_string()});
            }
        } else if let Some(line) = on.and_then(|on| book.get(&(on.to_owned(), key))) {
            seat["workspace"] = json!({"seat": line.seat, "repo_path": line.repo_path});
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&seen)?);
        return Ok(0);
    }
    say(&seen);
    Ok(0)
}

/// Status as lines a person reads: sizes and ages, unknowns left out.
fn say(seen: &Value) {
    let disk = &seen["disk"];
    let mut line = format!("disk: {} free", size(&disk["available"]));
    for (name, key) in [("floor", "floor_bytes"), ("seat cap", "seat_cap_bytes")] {
        if !disk[key].is_null() {
            let _ = write!(line, ", {name} {}", size(&disk[key]));
        }
    }
    if let Some(why) = disk["refusing"].as_str() {
        let _ = write!(line, " (refusing: {why})");
    }
    println!("{line}");
    let gate = &seen["gate"];
    // `unknown`: the gate names a run that is no longer live (it is never
    // cleared), which says nothing about who holds it now.
    if gate["holder"] != "none" && gate["holder"] != "unknown" {
        println!("exclusive: held by run {}", text(&gate["holder"]));
    }
    for requester in gate["requesters"].as_array().into_iter().flatten() {
        println!("exclusive: run {} waits for it", text(requester));
    }
    let now = crate::receipt::now_ms();
    for run in seen["runs"].as_array().into_iter().flatten() {
        let command = run["recipe"]
            .as_array()
            .map(|words| words.iter().map(text).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let mut line = format!(
            "run {} {}: {command}",
            text(&run["id"]),
            text(&run["phase"])
        );
        if run["exclusive"] == true {
            line += " (exclusive)";
        }
        if let Some(at) = run["submitted_ms"].as_u64() {
            let _ = write!(
                line,
                ", submitted {} ago",
                age(now.saturating_sub(u128::from(at)))
            );
        }
        if let Some(at) = run["deadline_ms"].as_u64() {
            let _ = write!(
                line,
                ", deadline in {}",
                age(u128::from(at).saturating_sub(now))
            );
        }
        println!("{line}");
    }
    for seat in seen["seats"].as_array().into_iter().flatten() {
        let name = match seat["workspace"]["repo_path"].as_str() {
            Some(repo) => format!("{} of {repo}", text(&seat["workspace"]["seat"])),
            None => text(&seat["key"]),
        };
        let mut line = format!("seat {name}");
        if !seat["bytes"].is_null() {
            let _ = write!(line, ": {}", size(&seat["bytes"]));
        }
        if let Some(last) = seat["last_run"].as_str() {
            let _ = write!(line, ", last run {last}");
            if let Some(outcome) = seat["outcome"].as_str() {
                let _ = write!(line, " {outcome}");
            }
        }
        println!("{line}");
    }
    for tombstone in seen["tombstones"].as_array().into_iter().flatten() {
        println!(
            "tombstone {}: a retired seat whose delete failed; the next prune deletes it",
            text(tombstone)
        );
    }
    let left = &seen["leftovers"];
    if let Some(error) = left["error"].as_str() {
        println!("leftovers: not read ({error})");
    }
    for process in left["processes"].as_array().into_iter().flatten() {
        println!(
            "leftover pid {} of run {}: {}",
            process["pid"],
            text(&process["run"]),
            text(&process["command"])
        );
    }
    let unknown = left["record_none"].as_array().map_or(0, Vec::len);
    if unknown > 0 {
        println!(
            "{unknown} processes are marked with a run this place does not know (--json lists them)"
        );
    }
}

/// Bytes as a size a reader takes in at once: `12.3 GiB`; `unknown` for
/// none (a measurement past its budget).
// A size to one decimal: f64's precision is more than enough.
#[allow(clippy::cast_precision_loss)]
fn size(bytes: &Value) -> String {
    let Some(bytes) = bytes.as_u64() else {
        return "unknown".into();
    };
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

/// A span in the largest whole unit that fits: `40 s`, `12 min`, `3 h`.
pub(crate) fn age(ms: u128) -> String {
    let seconds = ms / 1000;
    match seconds {
        0..120 => format!("{seconds} s"),
        120..7200 => format!("{} min", seconds / 60),
        _ => format!("{} h", seconds / 3600),
    }
}

/// A JSON value as plain text: a string without its quotes.
fn text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}
