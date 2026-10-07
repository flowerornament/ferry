//! Durable run evidence distinguishes recipe failures from transport refusals.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub const PROTOCOL: u32 = 1;
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
pub fn save(root: &Path, receipt: &Receipt) -> Result<(), Box<dyn std::error::Error>> {
    let dir = root.join(".ferry/receipts");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", receipt.id));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    serde_json::to_writer_pretty(&mut file, receipt)?;
    file.sync_all()?;
    Ok(())
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
    fn ids_are_unique_and_safe() {
        let a = run_id().unwrap();
        let b = run_id().unwrap();
        assert_ne!(a, b);
        crate::config::identifier(&a).unwrap();
    }
}
