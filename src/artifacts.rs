//! Declared artifacts (SPEC §8): archived at the place without filesystem
//! links, each file hashed into the record, and checked file by file where
//! the caller unpacks them.
use crate::{config, receipt::digest};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt::Display,
    fs,
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// One collected file and its digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub path: String,
    pub sha256: String,
}

/// Whether a run's declared artifacts reached the caller, separate from its
/// verdict: a consumer that needs the files reads this, never the reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The recipe declares none.
    None,
    /// Every declared file is present and verified.
    Fetched,
    Failed {
        kind: Kind,
        path: Option<String>,
        error: String,
    },
}

/// Why artifacts failed; README (Artifacts) says what each kind means.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Collection,
    EmptyPattern,
    Transfer,
    Digest,
    Archive,
}

pub fn failed(kind: Kind, path: Option<&str>, error: impl Display) -> Status {
    let (path, error) = (path.map(str::to_owned), error.to_string());
    Status::Failed { kind, path, error }
}

fn files(root: &Path, path: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err("artifact links are not supported".into());
    }
    if !path.canonicalize()?.starts_with(root.canonicalize()?) {
        return Err("artifact escapes checkout".into());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            files(root, &entry?.path(), found)?;
        }
    } else if metadata.is_file() {
        found.push(path.to_path_buf());
    } else {
        return Err("artifact is not a regular file".into());
    }
    Ok(())
}

/// Archives the files `patterns` match into `output` and lists each with its
/// digest. A pattern that matches nothing, or a file in a preserved path
/// (which the checkout reset keeps, so it may be an earlier run's), fails.
pub fn collect(
    checkout: &Path,
    patterns: &[String],
    preserve: &[String],
    output: &Path,
) -> std::result::Result<Vec<Artifact>, Status> {
    let mut found = vec![];
    for pattern in patterns {
        let before = found.len();
        let matched = (|| -> Result<()> {
            config::relative_path(pattern)?;
            let absolute = checkout.join(pattern);
            for path in glob::glob(absolute.to_str().ok_or("artifact glob must be UTF-8")?)? {
                files(checkout, &path?, &mut found)?;
            }
            Ok(())
        })();
        matched.map_err(|e| failed(Kind::Collection, None, e))?;
        if found.len() == before {
            let error = format!("{pattern} matched no file");
            return Err(failed(Kind::EmptyPattern, Some(pattern), error));
        }
    }
    found.sort();
    found.dedup();
    archive(checkout, &found, preserve, output).map_err(|e| failed(Kind::Collection, None, e))
}

fn archive(
    checkout: &Path,
    found: &[PathBuf],
    preserve: &[String],
    output: &Path,
) -> Result<Vec<Artifact>> {
    let mut archive = tar::Builder::new(fs::File::create(output)?);
    archive.follow_symlinks(false);
    let mut listed = vec![];
    for file in found {
        let path = file
            .strip_prefix(checkout)?
            .to_str()
            .ok_or("artifact path must be UTF-8")?;
        if let Some(kept) = preserve
            .iter()
            .find(|kept| Path::new(path).starts_with(kept))
        {
            return Err(
                format!("{path} is in preserved {kept}: it may be an earlier run's").into(),
            );
        }
        archive.append_path_with_name(file, path)?;
        let sha256 = digest(&fs::read(file)?);
        listed.push(Artifact {
            path: path.into(),
            sha256,
        });
    }
    archive.finish()?;
    Ok(listed)
}

/// Unpacks the place's archive into an emptied `destination` and checks it:
/// every entry a regular file inside it, then the files against the place's
/// list, path and digest, both ways. On a failure nothing is left there.
pub fn fetch(archive: Result<Vec<u8>>, listed: &[Artifact], destination: &Path) -> Status {
    let checked = (|| -> std::result::Result<(), Status> {
        let transfer = |error: &dyn Display| failed(Kind::Transfer, None, error);
        match fs::remove_dir_all(destination) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(transfer(&e)),
            _ => fs::create_dir_all(destination).map_err(|e| transfer(&e))?,
        }
        let bytes = archive.map_err(|e| transfer(&e))?;
        let entry_failed = |e: &dyn Display| failed(Kind::Archive, None, e);
        let mut unpacked = BTreeMap::new();
        let mut tar = tar::Archive::new(&bytes[..]);
        for entry in tar.entries().map_err(|e| entry_failed(&e))? {
            let mut entry = entry.map_err(|e| entry_failed(&e))?;
            let path = entry
                .path()
                .ok()
                .and_then(|p| p.to_str().map(str::to_owned));
            let path = path.unwrap_or_default();
            // A second entry onto one file (a case-insensitive disk folds
            // `Out.txt` onto `out.txt`) would leave other bytes than hashed.
            if !entry.header().entry_type().is_file()
                || fs::symlink_metadata(destination.join(&path)).is_ok()
                || !entry.unpack_in(destination).unwrap_or(false)
            {
                return Err(failed(
                    Kind::Archive,
                    Some(&path),
                    "not a regular file inside",
                ));
            }
            let bytes = fs::read(destination.join(&path)).map_err(|e| entry_failed(&e))?;
            unpacked.insert(path, digest(&bytes));
        }
        let wanted: BTreeMap<_, _> = listed
            .iter()
            .map(|a| (a.path.clone(), a.sha256.clone()))
            .collect();
        let paths = wanted.keys().chain(unpacked.keys());
        match paths
            .map(|path| (path, wanted.get(path), unpacked.get(path)))
            .find(|(_, w, u)| w != u)
        {
            Some((path, Some(_), Some(_))) => Err(failed(
                Kind::Digest,
                Some(path),
                "differs from the place's digest",
            )),
            Some((path, Some(_), None)) => Err(failed(
                Kind::Archive,
                Some(path),
                "listed but not in the archive",
            )),
            Some((path, ..)) => Err(failed(Kind::Archive, Some(path), "not in the place's list")),
            None => Ok(()),
        }
    })();
    checked.err().map_or(Status::Fetched, |status| {
        let _ = fs::remove_dir_all(destination);
        status
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn symlink_artifacts_refuse_instead_of_copying_external_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("secret")).unwrap();
        let output = dir.path().join("artifact.tar");
        let got = collect(dir.path(), &["secret".into()], &[], &output);
        assert!(matches!(
            got,
            Err(Status::Failed {
                kind: Kind::Collection,
                ..
            })
        ));
    }

    /// An archive of `files` (path, bytes), and its listing.
    fn archive(files: &[(&str, &[u8])]) -> (Vec<u8>, Vec<Artifact>) {
        let mut builder = tar::Builder::new(vec![]);
        for (path, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *bytes).unwrap();
        }
        let listed = files.iter().map(|(path, bytes)| Artifact {
            path: (*path).into(),
            sha256: digest(bytes),
        });
        (builder.into_inner().unwrap(), listed.collect())
    }

    fn kind(status: &Status) -> Option<Kind> {
        match status {
            Status::Failed { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// The destination is emptied first, so an earlier file never satisfies
    /// a run; an unlisted or missing entry fails, and leaves nothing.
    #[test]
    fn a_fetch_checks_every_entry_against_the_list_in_an_emptied_directory() {
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("run");
        fs::create_dir_all(&to).unwrap();
        fs::write(to.join("stale"), "earlier").unwrap();
        let (bytes, listed) = archive(&[("out.txt", b"built")]);
        assert_eq!(fetch(Ok(bytes.clone()), &listed, &to), Status::Fetched);
        assert!(!to.join("stale").exists());
        assert_eq!(fs::read(to.join("out.txt")).unwrap(), b"built");
        let (extra, _) = archive(&[("out.txt", b"built"), ("other", b"x")]);
        assert_eq!(kind(&fetch(Ok(extra), &listed, &to)), Some(Kind::Archive));
        assert!(!to.exists());
        let (_, two) = archive(&[("out.txt", b"built"), ("gone", b"y")]);
        assert_eq!(kind(&fetch(Ok(bytes), &two, &to)), Some(Kind::Archive));
        // Two entries onto one file: the first one's bytes would be gone.
        let (twice, _) = archive(&[("out.txt", b"built"), ("out.txt", b"built")]);
        assert_eq!(kind(&fetch(Ok(twice), &listed, &to)), Some(Kind::Archive));
        let (lying, _) = archive(&[("out.txt", b"forged")]);
        assert_eq!(kind(&fetch(Ok(lying), &listed, &to)), Some(Kind::Digest));
        assert!(!to.exists());
    }
}
