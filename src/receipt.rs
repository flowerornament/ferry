//! Durable run evidence distinguishes recipe failures from transport refusals.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub const PROTOCOL: u32 = 1;
/// What this Ferry can do, as a project contract may require it under
/// `[provider] requires`. Documented in README; only ever extended.
pub const CAPABILITIES: &[&str] = &[
    "args",
    "refusal-event",
    "custody",
    "lease",
    "admission",
    "detach",
];

/// Refuses requirements this binary does not provide.
pub fn provides(
    requires: &std::collections::BTreeSet<String>,
    capabilities: &[String],
) -> Result<(), String> {
    let missing: Vec<&str> = requires
        .iter()
        .filter(|name| !capabilities.contains(name))
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Ferry lacks required capabilities {missing:?} (it provides {capabilities:?})"
        ))
    }
}
pub fn capabilities() -> Vec<String> {
    CAPABILITIES.iter().map(|name| (*name).to_owned()).collect()
}
/// The source revision a Nix build compiled in, or `unpinned` for a build
/// without one. Receipts that predate the field leave it null: unknown.
pub fn source_rev() -> String {
    option_env!("FERRY_SOURCE_REV")
        .unwrap_or("unpinned")
        .to_owned()
}

/// The path of the running executable, as the kernel resolved it.
pub fn exe_path() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|path| path.display().to_string())
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    RanPass,
    RanFail,
    Refused,
    Lost,
    Timeout,
    /// Stopped by `ferry stop` or a signal before its result: 128 + n.
    Stopped,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Receipt {
    pub protocol: u32,
    pub id: String,
    pub commit: Option<String>,
    pub recipe: String,
    pub seat: String,
    pub machine: String,
    pub outcome: Outcome,
    pub exit_code: i32,
    pub reason: Option<String>,
    pub caller_binary_sha256: String,
    pub host_binary_sha256: Option<String>,
    pub started_ms: u128,
    pub elapsed_ms: u128,
    pub requirements: Vec<String>,
    pub machine_features: Vec<String>,
    pub mirrors: Vec<crate::execution::Mirrored>,
    pub adapter_sha256: Option<String>,
    pub contract_sha256: Option<String>,
    #[serde(default)]
    pub forwarded_env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub companion_refs: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub companion_sources: std::collections::BTreeMap<String, crate::config::CompanionSource>,
    #[serde(default)]
    pub preserve: Vec<String>,
    #[serde(default)]
    pub collection: Option<crate::execution::Collection>,
    /// Recipe arguments given after `--`; older receipts default to none.
    #[serde(default)]
    pub args: Vec<String>,
    /// SHA-256 of the arguments as a JSON array of strings.
    #[serde(default)]
    pub args_sha256: Option<String>,
    /// Source revision (`unpinned` when the build had none) and executable
    /// path of the caller and host binaries; null on older receipts: unknown.
    #[serde(default)]
    pub caller_source_rev: Option<String>,
    #[serde(default)]
    pub caller_exe: Option<String>,
    #[serde(default)]
    pub host_source_rev: Option<String>,
    #[serde(default)]
    pub host_exe: Option<String>,
    /// What the session left behind at release; null when it never released.
    #[serde(default)]
    /// The place's custody record, as it was (its shape changed in slice
    /// 4b; a receipt keeps what its place wrote).
    pub custody: Option<serde_json::Value>,
    /// The host lease the session held: `shared` or `exclusive`, and its wait.
    #[serde(default)]
    pub lease: Option<crate::lock::Lease>,
    /// The host's disk admission: policy, measurements, evictions, verdict.
    #[serde(default)]
    pub admission: Option<crate::admission::Admission>,
    /// The provider capabilities the project required.
    #[serde(default)]
    pub provider_requires: Vec<String>,
    /// What reached the place's output log (`bytes`, `capped`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<crate::execution::Output>,
    /// When the run had to end by (ms since the epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u128>,
    /// Who stopped the run and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<serde_json::Value>,
    /// Binds the place's record to this receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// `submitted` while the run is under way (every caller fact, no
    /// verdict yet), then `final`. Absent on older receipts: final.
    #[serde(default)]
    pub state: Option<String>,
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
pub fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
pub fn file_digest(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hex(&hash.finalize()))
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                char::from(DIGITS[usize::from(byte >> 4)]),
                char::from(DIGITS[usize::from(byte & 15)]),
            ]
        })
        .collect()
}
pub fn run_id() -> std::io::Result<String> {
    use std::io::Read;
    let mut nonce = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    Ok(format!("{}-{}", now_ms(), hex(&nonce)))
}
fn path(root: &Path, id: &str) -> std::path::PathBuf {
    root.join(".ferry/receipts").join(format!("{id}.json"))
}

/// Stores a run's first receipt, complete or not at all, never over another.
pub fn submit(root: &Path, receipt: &Receipt) -> Result<(), Box<dyn std::error::Error>> {
    crate::record::write_new(
        &path(root, &receipt.id),
        &serde_json::to_vec_pretty(receipt)?,
        None,
    )
}

/// A stored receipt.
pub fn load(root: &Path, id: &str) -> Result<Receipt, Box<dyn std::error::Error>> {
    crate::config::identifier(id)?;
    Ok(serde_json::from_slice(&fs::read(path(root, id))?)?)
}

/// Stores a receipt, complete or not at all (SPEC §8). A submitted receipt
/// is written once, at submit, and never replaces a stored one, so it can
/// never undo a final one (verdict once). A final one replaces it; each
/// writer has its own temporary file, so concurrent `ferry record`s all
/// complete, and final over final is the same place record.
pub fn save(root: &Path, receipt: &Receipt) -> Result<(), Box<dyn std::error::Error>> {
    let path = path(root, &receipt.id);
    if receipt.state.as_deref() == Some("submitted") {
        return submit(root, receipt).or_else(|e| if path.exists() { Ok(()) } else { Err(e) });
    }
    let dir = root.join(".ferry/receipts");
    fs::create_dir_all(&dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    std::io::Write::write_all(&mut tmp, &serde_json::to_vec_pretty(receipt)?)?;
    tmp.as_file().sync_all()?;
    tmp.persist(&path)?;
    fs::File::open(&dir)?.sync_all()?;
    Ok(())
}

/// Adds a note to a reason, keeping what was already there.
pub fn note(reason: &mut Option<String>, text: String) {
    *reason = Some(match reason.take() {
        Some(previous) => format!("{previous}; {text}"),
        None => text,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recipe_125_is_still_a_recipe_failure() {
        assert_ne!(Outcome::RanFail, Outcome::Refused);
        assert_eq!(
            serde_json::to_string(&Outcome::RanFail).unwrap(),
            "\"ran-fail\""
        );
    }
    #[test]
    fn a_submitted_save_never_replaces_a_final_receipt() {
        let root = tempfile::tempdir().unwrap();
        let mut receipt: Receipt = serde_json::from_value(serde_json::json!({
            "protocol": PROTOCOL, "id": run_id().unwrap(), "commit": null,
            "recipe": "r", "seat": "s", "machine": "m", "outcome": "ran-pass",
            "exit_code": 0, "reason": null, "caller_binary_sha256": "c",
            "host_binary_sha256": null, "started_ms": 0, "elapsed_ms": 0,
            "requirements": [], "machine_features": [], "mirrors": [],
            "adapter_sha256": null, "contract_sha256": null, "state": "final"
        }))
        .unwrap();
        save(root.path(), &receipt).unwrap();
        // A caller still holding the submitted receipt saves it late.
        receipt.state = Some("submitted".into());
        receipt.outcome = Outcome::Lost;
        save(root.path(), &receipt).unwrap();
        let stored = load(root.path(), &receipt.id).unwrap();
        assert_eq!(stored.state.as_deref(), Some("final"));
        assert_eq!(stored.outcome, Outcome::RanPass);
    }
    #[test]
    fn ids_are_unique_and_safe() {
        let a = run_id().unwrap();
        let b = run_id().unwrap();
        assert_ne!(a, b);
        crate::config::identifier(&a).unwrap();
    }
}
