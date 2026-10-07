//! Declared artifacts are returned as an archive without filesystem links.
use crate::config;
use std::{
    fs,
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

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
pub fn collect(checkout: &Path, patterns: &[String], output: &Path) -> Result<()> {
    let mut found = vec![];
    for pattern in patterns {
        config::relative_path(pattern)?;
        let absolute = checkout.join(pattern);
        for matched in glob::glob(absolute.to_str().ok_or("artifact glob must be UTF-8")?)? {
            files(checkout, &matched?, &mut found)?;
        }
    }
    found.sort();
    found.dedup();
    let file = fs::File::create(output)?;
    let mut archive = tar::Builder::new(file);
    archive.follow_symlinks(false);
    for path in found {
        archive.append_path_with_name(&path, path.strip_prefix(checkout)?)?;
    }
    archive.finish()?;
    Ok(())
}
pub fn unpack(archive: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in tar::Archive::new(fs::File::open(archive)?).entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            return Err("returned artifact is not a regular file".into());
        }
        if !entry.unpack_in(destination)? {
            return Err("returned artifact escapes run directory".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn symlink_artifacts_refuse_instead_of_copying_external_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("secret")).unwrap();
        assert!(
            collect(
                dir.path(),
                &["secret".into()],
                &dir.path().join("artifact.tar")
            )
            .is_err()
        );
    }
}
