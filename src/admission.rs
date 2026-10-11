//! Host admission: the disk floor a session must clear before it runs.
//!
//! Policy lives in one file, `$XDG_CONFIG_HOME/ferry/host.toml` (else
//! `~/.config/ferry/host.toml`), read on the machine that runs the session:
//!
//! - absent: not configured; the run proceeds and the receipt says so;
//! - `version = 1`, `min_free_bytes`: below the floor, refuse; optional
//!   `seat_cap_bytes`: a seat whose preserve paths and cache exceed it is
//!   wiped at run start and runs cold (SPEC §9).
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
    seat_cap_bytes: Option<u64>,
}

/// The admission evidence a session leaves in its host result and receipt.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Admission {
    /// The policy file as read, or `"not-configured"`.
    pub policy: Option<serde_json::Value>,
    pub floor_bytes: Option<u64>,
    pub filesystem: Option<Filesystem>,
    pub available: Option<u64>,
    /// The per-seat cap (SPEC §9), if the policy sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat_cap_bytes: Option<u64>,
    /// What the cap wiped, carried to the record's top-level `pruned`.
    #[serde(skip)]
    pub pruned: Option<Pruned>,
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
    admission.seat_cap_bytes = policy.seat_cap_bytes;
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

/// What the per-seat cap wiped at a run's start (SPEC §9).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pruned {
    pub bytes: u64,
    pub cap: u64,
}

/// How often a seat is measured against its cap: a walk of a large cache
/// costs seconds, and growth per run is bounded, so the cap may be exceeded
/// by at most this long of runs (coordinator, ferry-xtx.10 Q3).
const MEASURE_EVERY_MS: u128 = 60 * 60 * 1000;

/// The longest a seat's measurement may take before the cap gives up for
/// this run (review P2): it runs before the recipe, inside its deadline.
const MEASURE_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Bytes of regular files under `path` on device `dev`, never following
/// links nor crossing into another device (a mount); 0 if absent. `None`
/// once `until` has passed.
fn size_until(path: &Path, until: Option<std::time::Instant>, dev: u64) -> Option<u64> {
    if until.is_some_and(|until| std::time::Instant::now() >= until) {
        return None;
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Some(0);
    };
    if meta.dev() != dev {
        return Some(0);
    }
    if meta.is_dir() {
        let mut total = 0;
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            total += size_until(&entry.path(), until, dev)?;
        }
        Some(total)
    } else if meta.is_file() {
        Some(meta.len())
    } else {
        Some(0)
    }
}

/// Why `rel` (below `root`) may not be measured or deleted, or `None` when it
/// provably lies inside: every component a plain name, and none on the way a
/// symbolic link, by `lstat`, never following one. A path that ends early
/// is absent, so there is nothing to touch, and that is inside. Anything
/// that cannot be inspected is outside (slice 5 review P1-a).
pub fn outside(root: &Path, rel: &Path) -> Option<String> {
    let root_meta =
        fs::metadata(root).map_err(|e| format!("{} cannot be inspected: {e}", root.display()));
    root_meta.map_or_else(Some, |meta| outside_on(root, rel, meta.dev()))
}

/// As `outside`, with every component also on device `dev`: a volume
/// mounted inside the seat is not the seat's (ferry-xtx.23).
fn outside_on(root: &Path, rel: &Path, dev: u64) -> Option<String> {
    let mut at = root.to_path_buf();
    for part in rel.components() {
        let std::path::Component::Normal(name) = part else {
            return Some(format!("{} is not a plain relative path", rel.display()));
        };
        at.push(name);
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Some(format!("{} is behind a symbolic link", rel.display()));
            }
            Ok(meta) if meta.dev() != dev => {
                return Some(format!("{} crosses into another device", rel.display()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => return Some(format!("{} cannot be inspected: {e}", rel.display())),
        }
    }
    None
}

/// Bytes under `rel` below `root`, on `root`'s device.
pub fn size_in(root: &Path, rel: &Path) -> u64 {
    let dev = fs::metadata(root).map_or(0, |meta| meta.dev());
    size_until(&root.join(rel), None, dev).unwrap_or(0)
}

/// Deletes `rel` below `root`: containment proven again right before, and
/// nothing deleted if anything under it is on another device (a mount). std's
/// `remove_dir_all` walks directory handles, so a link swapped in is unlinked.
pub fn remove(root: &Path, rel: &Path) -> Result<()> {
    if let Some(why) = outside(root, rel) {
        return Err(format!("{why}: not deleted").into());
    }
    let path = root.join(rel);
    if let Some(mount) = foreign(&path, fs::metadata(root)?.dev()) {
        return Err(format!("{} is on another device: not deleted", mount.display()).into());
    }
    crate::execution::pause_at("remove-walk", &path);
    match fs::remove_dir_all(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// The first path under `path` on another device than `dev` (no link followed).
fn foreign(path: &Path, dev: u64) -> Option<std::path::PathBuf> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.dev() != dev {
        return Some(path.to_path_buf());
    }
    if !meta.is_dir() {
        return None;
    }
    let entries = fs::read_dir(path).ok()?;
    entries
        .flatten()
        .find_map(|entry| foreign(&entry.path(), dev))
}

/// Under the run's seat lock, before the checkout: when the seat was last
/// measured over an hour ago, measures its preserve paths and its cache;
/// over `cap`, wipes them and returns what was pruned. Source, logs,
/// receipts and records are never touched. A measurement that would eat
/// into the recipe's time is skipped, as a note.
pub fn cap(
    root: &Path,
    seat: &str,
    preserve: &[String],
    cap: u64,
    spent: bool,
) -> Result<Option<Pruned>> {
    let stamp = root.join("caches").join(format!(".measured-{seat}"));
    let now = crate::receipt::now_ms();
    let last = fs::read(&stamp)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|stamp| stamp["ms"].as_u64().map(u128::from));
    if last.is_some_and(|ms| now.saturating_sub(ms) < MEASURE_EVERY_MS) {
        return Ok(None);
    }
    if spent {
        crate::run::note_after("seat cap: not measured: the run's wait budget is spent".into());
        return Ok(None);
    }
    // Only paths proven inside the seat are measured or wiped.
    let paths: Vec<_> = preserve
        .iter()
        .map(|path| Path::new("slots").join(seat).join(path))
        .chain(std::iter::once(Path::new("caches").join(seat)))
        .filter(|rel| match outside(root, rel) {
            Some(why) => {
                crate::run::note_after(format!("seat cap: {why}: not measured or wiped"));
                false
            }
            None => true,
        })
        .collect();
    let dev = fs::metadata(root)?.dev();
    let until = std::time::Instant::now() + MEASURE_LIMIT;
    let Some(bytes) = paths
        .iter()
        .map(|rel| size_until(&root.join(rel), Some(until), dev))
        .sum::<Option<u64>>()
    else {
        crate::run::note_after(format!(
            "seat cap: not measured: over {MEASURE_LIMIT:?}; not wiped"
        ));
        // Stamped all the same, so the next try is an hour away.
        fs::create_dir_all(root.join("caches"))?;
        fs::write(
            &stamp,
            serde_json::json!({"ms": now, "bytes": null}).to_string(),
        )?;
        return Ok(None);
    };
    let over = bytes > cap;
    if over {
        if crate::execution::fail_at("cap-wipe") {
            return Err("cap-wipe: injected by FERRY_FAIL_AT".into());
        }
        for rel in &paths {
            remove(root, rel)?;
        }
    }
    fs::create_dir_all(root.join("caches"))?;
    let measured = serde_json::json!({"ms": now, "bytes": if over { 0 } else { bytes }});
    fs::write(&stamp, measured.to_string())?;
    Ok(over.then_some(Pruned { bytes, cap }))
}

#[cfg(test)]
mod tests {
    use super::{foreign, outside, outside_on, size_until};
    use std::{fs, os::unix::fs::MetadataExt, os::unix::fs::symlink, path::Path};

    /// At the point of deletion, independently of request validation, only a
    /// plain relative path without a link on the way is inside (review S2).
    #[test]
    fn only_plain_paths_without_links_are_inside() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::create_dir_all(root.join("slots/s/real")).unwrap();
        symlink("/", root.join("slots/s/link")).unwrap();
        assert_eq!(outside(root, Path::new("slots/s/real/x")), None);
        assert_eq!(outside(root, Path::new("slots/s/absent/x")), None);
        for rel in ["/etc", "slots/s/../../x", "slots/s/link/etc"] {
            assert!(outside(root, Path::new(rel)).is_some(), "{rel}");
        }
    }

    /// herald-1b's diagnostic (eedc813a): a preserve path whose ancestor in
    /// the slot is a symbolic link is never deleted through it.
    #[test]
    fn cap_must_not_delete_through_a_preserve_parent_symlink() {
        let owned = tempfile::tempdir().unwrap();
        let (root, outside) = (owned.path().join("ferry"), owned.path().join("outside"));
        fs::create_dir_all(root.join("slots/seat")).unwrap();
        fs::create_dir_all(outside.join("cache")).unwrap();
        let sentinel = outside.join("cache/not-ferry-owned");
        fs::write(&sentinel, "must survive").unwrap();
        symlink(&outside, root.join("slots/seat/link")).unwrap();
        crate::config::preserve_path("link/cache").unwrap();
        let _ = super::cap(&root, "seat", &["link/cache".into()], 1, false);
        assert!(sentinel.exists(), "cap deleted through an ancestor symlink");
    }

    /// Nothing on another device is inside, measured or deleted: a volume
    /// mounted in a seat is not the seat's. A different device stands in for
    /// a mount, which a test cannot make (ferry-xtx.23).
    #[test]
    fn another_device_is_never_inside_measured_or_deleted() {
        let root = tempfile::tempdir().unwrap();
        let (root, mount) = (root.path(), Path::new("slots/s/mount"));
        fs::create_dir_all(root.join(mount)).unwrap();
        fs::write(root.join(mount).join("data"), [0u8; 16]).unwrap();
        let dev = fs::metadata(root).unwrap().dev();
        let other = dev.wrapping_add(1);
        assert_eq!(outside_on(root, mount, dev), None);
        assert!(outside_on(root, mount, other).is_some_and(|why| why.contains("another device")));
        assert_eq!(size_until(&root.join("slots"), None, other), Some(0));
        assert_eq!(
            foreign(&root.join("slots"), other),
            Some(root.join("slots"))
        );
        assert_eq!(foreign(&root.join("slots"), dev), None);
    }

    /// A delete proves containment again at its own moment: a directory
    /// swapped for a symbolic link after an earlier check is not followed
    /// (ferry-xtx.23).
    #[test]
    fn a_delete_rechecks_containment_at_its_moment() {
        let (root, away) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (root, build) = (root.path(), Path::new("slots/s/build"));
        fs::create_dir_all(root.join(build)).unwrap();
        assert_eq!(outside(root, &build.join("cache")), None);
        fs::remove_dir(root.join(build)).unwrap();
        symlink(away.path(), root.join(build)).unwrap();
        fs::create_dir(away.path().join("cache")).unwrap();
        fs::write(away.path().join("cache/sentinel"), "keep").unwrap();
        assert!(super::remove(root, &build.join("cache")).is_err());
        assert!(away.path().join("cache/sentinel").exists());
    }

    /// A measurement gives up once its time is over (review P2).
    #[test]
    fn a_measurement_past_its_time_gives_up() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("blob"), [0u8; 16]).unwrap();
        let dev = fs::metadata(root.path()).unwrap().dev();
        assert_eq!(size_until(root.path(), None, dev), Some(16));
        let past = std::time::Instant::now();
        assert_eq!(size_until(root.path(), Some(past), dev), None);
    }
}
