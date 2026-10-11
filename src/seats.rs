//! The caller's seat book (SPEC §9, §10): each seat this caller used, so
//! `ferry seats prune` can retire those whose workspace is gone. It is
//! append-only, one `O_APPEND` line per accepted run, plus a tombstone per
//! retired seat. It is never rewritten, so concurrent appends are never lost.
use crate::{config, transport};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
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
    #[serde(default)]
    pub last_used_ms: u128,
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

/// Records that a run on `place` used seat `key` of `repo_path`.
pub fn record(key: &str, place: &str, repo_path: &Path, seat: &str) -> Result<()> {
    append(&Line {
        key: key.to_owned(),
        place: place.to_owned(),
        repo_path: repo_path.display().to_string(),
        seat: seat.to_owned(),
        last_used_ms: crate::receipt::now_ms(),
        retired_ms: None,
    })
}

/// Records the seat unless the book already has it live: a completion
/// retries an append that failed at acceptance.
pub fn ensure(key: &str, place: &str, repo_path: &Path, seat: &str) -> Result<()> {
    if latest()?
        .get(key)
        .is_some_and(|line| line.retired_ms.is_none())
    {
        return Ok(());
    }
    record(key, place, repo_path, seat)
}

/// The latest line per key, in file order (later lines win). Unreadable
/// lines are skipped.
fn latest() -> Result<BTreeMap<String, Line>> {
    let text = match fs::read_to_string(book()?) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut seats = BTreeMap::new();
    for line in text.lines() {
        if let Ok(line) = serde_json::from_str::<Line>(line) {
            seats.insert(line.key.clone(), line);
        }
    }
    Ok(seats)
}

/// Retires this caller's seats whose workspace is gone, through each
/// place's `retire` verb, and prints one line per seat. 0 when every
/// candidate was retired or is busy; 125 if a place refused otherwise; 255
/// if a place could not be reached.
pub fn prune(only: Option<&str>, hosts: &config::Machines) -> Result<i32> {
    let mut code = 0;
    for line in latest()?.into_values() {
        if line.retired_ms.is_some()
            || only.is_some_and(|place| place != line.place)
            || Path::new(&line.repo_path).exists()
        {
            continue;
        }
        let said = if line.place == "local" {
            // A local seat lives in its workspace's .ferry/local: it went
            // with the workspace.
            Ok(serde_json::json!({"retired": line.key, "bytes": 0}))
        } else if let Some(machine) = hosts.host.get(&line.place) {
            transport::retire(&transport::Place::Remote(machine), &line.key)
        } else {
            Err(format!("unknown place {}", line.place).into())
        };
        match said {
            Ok(reply) if reply["retired"].is_string() => {
                println!(
                    "retired {} on {} ({} bytes; workspace {} is gone)",
                    line.seat, line.place, reply["bytes"], line.repo_path
                );
                append(&Line {
                    retired_ms: Some(crate::receipt::now_ms()),
                    ..line
                })?;
            }
            Ok(reply) => {
                let why = reply["why"]
                    .as_str()
                    .or(reply["refused"].as_str())
                    .unwrap_or("");
                println!("skipped {} on {}: {why}", line.seat, line.place);
                if !why.starts_with("seat busy") {
                    code = code.max(125);
                }
            }
            Err(error) => {
                println!("failed {} on {}: {error}", line.seat, line.place);
                code = 255;
            }
        }
    }
    Ok(code)
}
