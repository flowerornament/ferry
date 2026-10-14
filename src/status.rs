//! `ferry status` (SPEC §4): one read of a place's state root and process
//! table. It takes no lock (a probe could make a starting run's seat look
//! busy) and changes nothing: a report, never a gate input. A leftover is
//! stopped by its exact pid, by the operator.
use crate::{config, custody, execution::Request, record, transport};
use serde_json::{Value, json};
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
    let below = crate::admission::admit(root, &mut disk).err();
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
    let mut keys: Vec<_> = fs::read_dir(root.join("seats"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
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
    for seat in seen["seats"].as_array_mut().into_iter().flatten() {
        if let Some(line) = seat["key"].as_str().and_then(|key| book.get(key)) {
            seat["workspace"] = json!({"seat": line.seat, "repo_path": line.repo_path});
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&seen)?);
        return Ok(0);
    }
    let disk = &seen["disk"];
    println!(
        "disk: {} available, floor {}, seat cap {}{}",
        disk["available"],
        disk["floor_bytes"],
        disk["seat_cap_bytes"],
        disk["refusing"]
            .as_str()
            .map(|why| format!(" (refusing: {why})"))
            .unwrap_or_default()
    );
    let gate = &seen["gate"];
    println!(
        "gate: holder {}, requesters {}",
        text(&gate["holder"]),
        gate["requesters"]
    );
    for run in seen["runs"].as_array().into_iter().flatten() {
        println!(
            "run {} {} seat {} {}{} deadline_ms {}",
            text(&run["id"]),
            text(&run["phase"]),
            text(&run["seat"]),
            run["recipe"],
            if run["exclusive"] == true {
                " exclusive"
            } else {
                ""
            },
            run["deadline_ms"]
        );
    }
    for seat in seen["seats"].as_array().into_iter().flatten() {
        let name = seat["workspace"]["repo_path"].as_str().map_or_else(
            || seat["key"].to_string(),
            |repo| format!("{} {repo}", seat["workspace"]["seat"]),
        );
        println!(
            "seat {name}: {} bytes, last run {} {}",
            seat["bytes"], seat["last_run"], seat["outcome"]
        );
    }
    let left = &seen["leftovers"];
    for process in left["processes"].as_array().into_iter().flatten() {
        println!(
            "leftover pid {} of run {}: {}",
            process["pid"], process["run"], process["command"]
        );
    }
    println!(
        "marked with no record here: {}; unreadable processes: {}",
        left["record_none"].as_array().map_or(0, Vec::len),
        left["unreadable"]
    );
    Ok(0)
}

/// A JSON value as plain text: a string without its quotes.
fn text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}
