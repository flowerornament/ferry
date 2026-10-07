//! Host admission: the disk floor a session must clear before it runs.
//!
//! Policy lives in one file, `$XDG_CONFIG_HOME/ferry/host.toml` (else
//! `~/.config/ferry/host.toml`), read on the machine that runs the session:
//!
//! - absent: not configured; the run proceeds and the receipt says so;
//! - `version = 1`, `min_free_bytes`: a guard; below the floor, refuse;
//! - `version = 2`, `min_free_bytes`, `evict = { mode = "idle-seat-targets",
//!   targets = [...] }`: below the floor, first reclaim the named targets
//!   under `caches/<seat>/` of idle seats, oldest release first, until the
//!   floor is met; refuse if candidates run out.
//!
//! Anything else (malformed TOML, another version, an unknown key, an evict
//! block in version 1) refuses. A seat is a candidate only with a custody
//! idle record, no taint, no owner marker, its lock taken without waiting,
//! and its idle record unchanged under that lock. Before removing a target,
//! every path from the state root down is checked: a real directory, not a
//! symlink, owned by the state root's owner, on the state root's filesystem, with no other
//! filesystem mounted inside it. Removal uses the standard library's
//! `remove_dir_all`, which never follows symlinks.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
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
    evict: Option<Evict>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Evict {
    mode: String,
    targets: Vec<String>,
}

/// The admission evidence a session leaves in its host result and receipt.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Admission {
    /// The policy file as read, or `"not-configured"`.
    pub policy: Option<serde_json::Value>,
    pub floor_bytes: Option<u64>,
    pub filesystem: Option<Filesystem>,
    pub available_before: Option<u64>,
    pub evicted: Vec<Evicted>,
    pub skipped: Skipped,
    pub available_after: Option<u64>,
    /// `admitted` or `refused`.
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Filesystem {
    pub path: String,
    pub device: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Evicted {
    pub seat: String,
    pub released_ms: u128,
    pub path: String,
    /// Sum of regular-file lengths.
    pub logical_bytes: u64,
    /// Sum of allocated blocks (512-byte units), each inode counted once.
    pub allocated_bytes: u64,
    /// Measured by statvfs around this removal; the difference may include
    /// unrelated activity on the filesystem.
    pub available_before: u64,
    pub available_after: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Skipped {
    pub locked: u32,
    pub owners: u32,
    pub tainted: u32,
    pub no_idle_record: u32,
    pub changed: u32,
    pub unsafe_path: u32,
    /// A custody marker could not be read, so the seat's state is unknown.
    #[serde(default)]
    pub unknown: u32,
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
pub fn admit(root: &Path, seat: &str, admission: &mut Admission) -> Result<()> {
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
    let policy: Policy = toml::from_str(std::str::from_utf8(&bytes)?)
        .map_err(|e| format!("host policy {} is malformed: {e}", path.display()))?;
    let targets = match (policy.version, &policy.evict) {
        (1, None) => vec![],
        (2, Some(evict)) if evict.mode == "idle-seat-targets" && !evict.targets.is_empty() => {
            for target in &evict.targets {
                crate::config::identifier(target).map_err(|_| {
                    format!("host policy evict target {target:?} is not a plain name")
                })?;
            }
            evict.targets.clone()
        }
        _ => {
            return Err(format!(
                "host policy {} is not a supported version 1 or 2 policy",
                path.display()
            )
            .into());
        }
    };
    if let Some(serde_json::Value::Object(fields)) = admission.policy.as_mut() {
        fields.insert("version".into(), policy.version.into());
    }
    admission.floor_bytes = Some(policy.min_free_bytes);
    let device = fs::metadata(root)?.dev();
    admission.filesystem = Some(Filesystem {
        path: root.display().to_string(),
        device,
    });
    let before = available(root)?;
    admission.available_before = Some(before);
    let mut now = before;
    if now < policy.min_free_bytes && !targets.is_empty() {
        now = evict(
            root,
            seat,
            device,
            &targets,
            policy.min_free_bytes,
            admission,
        )?;
    }
    admission.available_after = Some(now);
    if now < policy.min_free_bytes {
        return Err(format!(
            "below host floor: {now} bytes available, {} required{}",
            policy.min_free_bytes,
            if targets.is_empty() {
                ""
            } else {
                " after eviction"
            }
        )
        .into());
    }
    admission.result = "admitted".into();
    Ok(())
}

/// Reclaims idle seats' targets, oldest release first, until `floor` is met.
fn evict(
    root: &Path,
    admitting: &str,
    device: u64,
    targets: &[String],
    floor: u64,
    admission: &mut Admission,
) -> Result<u64> {
    let mut candidates = vec![];
    for entry in fs::read_dir(root.join("idle"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(seat) = name.strip_suffix(".json") else {
            continue;
        };
        if seat == admitting || crate::config::identifier(seat).is_err() {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        let released = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|record| record["released_ms"].as_u64())
            .map(u128::from);
        if let Some(released) = released {
            candidates.push((released, seat.to_owned(), bytes));
        }
    }
    candidates.sort();
    let idle: HashSet<&str> = candidates
        .iter()
        .map(|(_, seat, _)| seat.as_str())
        .collect();
    admission.skipped.no_idle_record = u32::try_from(
        fs::read_dir(root.join("caches"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name != admitting && !idle.contains(name.as_str())
            })
            .count(),
    )?;
    let mut now = available(root)?;
    for (released_ms, seat, sorted) in &candidates {
        if now >= floor {
            break;
        }
        now = reclaim(root, seat, *released_ms, sorted, device, targets, admission)?;
    }
    Ok(now)
}

/// One candidate seat, under its own lock. Returns the measured availability.
fn reclaim(
    root: &Path,
    seat: &str,
    released_ms: u128,
    sorted: &[u8],
    device: u64,
    targets: &[String],
    admission: &mut Admission,
) -> Result<u64> {
    let Ok(lock) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("locks").join(seat))
    else {
        admission.skipped.changed += 1;
        return available(root);
    };
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        admission.skipped.locked += 1;
        return available(root);
    }
    let file = |kind: &str| root.join(kind).join(format!("{seat}.json"));
    // Present, absent, or unknown: only a confirmed absence lets eviction go on.
    let marker = |kind: &str| match fs::symlink_metadata(file(kind)) {
        Ok(_) => Some(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    };
    let (owners, taint) = (marker("owners"), marker("taint"));
    if owners.is_none() || taint.is_none() {
        admission.skipped.unknown += 1;
    } else if owners == Some(true) {
        admission.skipped.owners += 1;
    } else if taint == Some(true) {
        admission.skipped.tainted += 1;
    } else if fs::read(file("idle")).ok().as_deref() != Some(sorted) {
        admission.skipped.changed += 1;
    } else {
        let mut safe = true;
        let mut removals = vec![];
        for target in targets {
            let path = root.join("caches").join(seat).join(target);
            match custody(root, &path, device) {
                Ok(Some(sizes)) => removals.push((path, sizes)),
                Ok(None) => {}
                Err(_) => safe = false,
            }
        }
        if safe {
            for (path, (logical_bytes, allocated_bytes)) in removals {
                let before = available(root)?;
                fs::remove_dir_all(&path)?;
                admission.evicted.push(Evicted {
                    seat: seat.to_owned(),
                    released_ms,
                    path: path.display().to_string(),
                    logical_bytes,
                    allocated_bytes,
                    available_before: before,
                    available_after: available(root)?,
                });
            }
            fs::remove_file(file("idle"))?;
        } else {
            admission.skipped.unsafe_path += 1;
        }
    }
    drop(lock);
    available(root)
}

/// Checks every directory from the state root to `target` and everything
/// under it, without following symlinks. `None` means the target is absent.
/// Returns (logical, allocated) bytes.
fn custody(root: &Path, target: &Path, device: u64) -> Result<Option<(u64, u64)>> {
    // The state root is this user's own; everything below must share its owner.
    let uid = fs::symlink_metadata(root)?.uid();
    let owned_dir = |path: &Path| -> Result<()> {
        let meta = fs::symlink_metadata(path)?;
        if !meta.file_type().is_dir() || meta.uid() != uid || meta.dev() != device {
            return Err(format!(
                "{} is not an owned directory on the state filesystem",
                path.display()
            )
            .into());
        }
        Ok(())
    };
    owned_dir(root)?;
    owned_dir(&root.join("caches"))?;
    owned_dir(target.parent().ok_or("target has no parent")?)?;
    match fs::symlink_metadata(target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => owned_dir(target)?,
    }
    let (mut logical, mut allocated, mut seen) = (0, 0, HashSet::new());
    let mut pending = vec![target.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            let meta = fs::symlink_metadata(&path)?;
            if meta.dev() != device {
                return Err(format!("{} is on another filesystem", path.display()).into());
            }
            if meta.uid() != uid {
                return Err(
                    format!("{} is not owned by the state root's owner", path.display()).into(),
                );
            }
            if meta.file_type().is_dir() {
                pending.push(path);
            } else if seen.insert((meta.dev(), meta.ino())) {
                if meta.file_type().is_file() {
                    logical += meta.len();
                }
                allocated += meta.blocks() * 512;
            }
        }
    }
    Ok(Some((logical, allocated)))
}
