//! Host admission: the disk floor a session must clear before it runs.
//!
//! Policy lives in one file, `$XDG_CONFIG_HOME/ferry/host.toml` (else
//! `~/.config/ferry/host.toml`), read on the machine that runs the session:
//!
//! - absent: not configured; the run proceeds and the receipt says so;
//! - `version = 1`, `min_free_bytes`: below the floor, refuse.
//!
//! Anything else (malformed TOML, another version, an unknown key) refuses.
//! Eviction (version 2) is gone with the idle records it read (slice 4b);
//! seats are bounded instead by `ferry seats prune` and `seat_cap_bytes`
//! (SPEC §9, slice 5).
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    version: u32,
    min_free_bytes: u64,
}

/// The admission evidence a session leaves in its host result and receipt.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Admission {
    /// The policy file as read, or `"not-configured"`.
    pub policy: Option<serde_json::Value>,
    pub floor_bytes: Option<u64>,
    pub filesystem: Option<Filesystem>,
    pub available: Option<u64>,
    /// `admitted` or `refused`.
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Filesystem {
    pub path: String,
    pub device: u64,
}

fn policy_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|base| base.join("ferry/host.toml"))
}

// statvfs field widths differ between platforms (u32 or u64), so the
// conversions are needed somewhere even where one is a no-op.
#[allow(clippy::useless_conversion)]
fn available(root: &Path) -> Result<u64> {
    let stat = nix::sys::statvfs::statvfs(root)?;
    Ok(u64::from(stat.blocks_available()) * u64::from(stat.fragment_size()))
}

/// Admits a session under the seat lock it already holds, or refuses.
/// `admission` is filled as far as the decision got, refused or not.
pub fn admit(root: &Path, admission: &mut Admission) -> Result<()> {
    admission.result = "refused".into();
    let Some(path) = policy_path() else {
        admission.policy = Some("not-configured".into());
        admission.result = "admitted".into();
        return Ok(());
    };
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            admission.policy = Some("not-configured".into());
            admission.result = "admitted".into();
            return Ok(());
        }
        Err(error) => return Err(format!("host policy {}: {error}", path.display()).into()),
    };
    admission.policy = Some(serde_json::json!({
        "path": path.display().to_string(),
        "sha256": crate::receipt::digest(&bytes),
    }));
    let text = std::str::from_utf8(&bytes)?;
    // The version first, whatever else the file holds: a version 2 policy
    // (with its `evict` block) gets the retirement and its remedy, not a
    // schema error. Unknown fields of a version 1 policy still refuse.
    let version = toml::from_str::<toml::Value>(text)
        .ok()
        .and_then(|value| value.get("version").and_then(toml::Value::as_integer));
    if version == Some(2) {
        return Err(format!(
            "host policy {}: version 2 eviction is retired: keep min_free_bytes as version 1",
            path.display()
        )
        .into());
    }
    let policy: Policy = toml::from_str(text)
        .map_err(|e| format!("host policy {} is malformed: {e}", path.display()))?;
    if policy.version != 1 {
        return Err(format!(
            "host policy {} is not a supported version 1 policy",
            path.display()
        )
        .into());
    }
    if let Some(serde_json::Value::Object(fields)) = admission.policy.as_mut() {
        fields.insert("version".into(), policy.version.into());
    }
    admission.floor_bytes = Some(policy.min_free_bytes);
    admission.filesystem = Some(Filesystem {
        path: root.display().to_string(),
        device: fs::metadata(root)?.dev(),
    });
    let now = available(root)?;
    admission.available = Some(now);
    if now < policy.min_free_bytes {
        return Err(format!(
            "below host floor: {now} bytes available, {} required",
            policy.min_free_bytes
        )
        .into());
    }
    admission.result = "admitted".into();
    Ok(())
}
