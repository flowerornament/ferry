//! Custody (SPEC §7): at the end of a run, its session is stopped (TERM, a
//! grace, then KILL), and the record names the session's members still
//! alive after that. Ferry proves nothing beyond it: a process that left the
//! session is outside its knowledge. If such a process keeps a lock, the
//! next run's busy refusal names the run it outlived.
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
    /// Why a session stop could not complete; its survivors are unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What this run's session stops found (recipe, then collect).
static FOUND: Mutex<Option<Release>> = Mutex::new(None);

/// Adds one session stop's result: the members alive after it, or why
/// they could not be listed.
pub fn note(stopped: Result<Vec<Survivor>, String>) {
    let Ok(mut found) = FOUND.lock() else {
        return;
    };
    let release = found.get_or_insert_with(|| Release {
        tier: "session".into(),
        survivors: Survivors::Listed(vec![]),
        error: None,
    });
    match (stopped, &mut release.survivors) {
        (Ok(more), Survivors::Listed(list)) => list.extend(more),
        (Ok(_), Survivors::Unknown(_)) => {}
        (Err(error), _) => {
            release.survivors = Survivors::Unknown(Unknown::Unknown);
            release.error = Some(error);
        }
    }
}

/// The run's custody, taken once at its end. A run that spawned no session
/// stopped nothing and left nothing.
pub fn take() -> Release {
    FOUND
        .lock()
        .ok()
        .and_then(|mut found| found.take())
        .unwrap_or(Release {
            tier: "session".into(),
            survivors: Survivors::Listed(vec![]),
            error: None,
        })
}
