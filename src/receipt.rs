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
pub const CAPABILITIES: &[&str] = &["args", "refusal-event", "custody", "lease", "admission"];

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
    pub custody: Option<crate::custody::Release>,
    /// The host lease the session held: `shared` or `exclusive`, and its wait.
    #[serde(default)]
    pub lease: Option<crate::execution::Lease>,
    /// The host's disk admission: policy, measurements, evictions, verdict.
    #[serde(default)]
    pub admission: Option<crate::admission::Admission>,
    /// The provider capabilities the project required.
    #[serde(default)]
    pub provider_requires: Vec<String>,
    /// Output lines that could not be delivered, by where they were lost:
    /// `stdout`/`stderr` (this caller's), `host` (the host's stream).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub output_dropped: std::collections::BTreeMap<String, u64>,
    /// Output lines this caller wrote, by stream; with `output_dropped`,
    /// every relayed line is accounted for.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub output_delivered: std::collections::BTreeMap<String, u64>,
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
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, for output bytes that are not UTF-8.
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(BASE64[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes standard base64; `None` for anything malformed.
pub fn unbase64(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for chunk in text.chunks(4) {
        let mut n = 0u32;
        let mut padding = 0;
        for &c in chunk {
            let value = if c == b'=' {
                padding += 1;
                0
            } else if padding > 0 {
                return None;
            } else {
                u32::try_from(BASE64.iter().position(|&b| b == c)?).ok()?
            };
            n = (n << 6) | value;
        }
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..4 - padding]);
    }
    Some(out)
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
/// Stores a receipt, complete or not at all. A submitted receipt never
/// replaces another; the final one replaces its submitted receipt.
pub fn save(root: &Path, receipt: &Receipt) -> Result<(), Box<dyn std::error::Error>> {
    let dir = root.join(".ferry/receipts");
    let path = dir.join(format!("{}.json", receipt.id));
    let bytes = serde_json::to_vec_pretty(receipt)?;
    if receipt.state.as_deref() == Some("submitted") {
        return crate::record::write_new(&path, &bytes, None);
    }
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{}.json.tmp", receipt.id));
    fs::write(&tmp, bytes)?;
    fs::File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, &path)?;
    fs::File::open(&dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_round_trips_every_length() {
        for len in 0..10 {
            let bytes: Vec<u8> = (0..len).map(|i| 0xff - i).collect();
            assert_eq!(super::unbase64(&super::base64(&bytes)), Some(bytes));
        }
        assert_eq!(super::base64(b"foob"), "Zm9vYg==");
        assert_eq!(super::unbase64("Zm9v!g=="), None);
    }

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
    fn ids_are_unique_and_safe() {
        let a = run_id().unwrap();
        let b = run_id().unwrap();
        assert_ne!(a, b);
        crate::config::identifier(&a).unwrap();
    }
}
