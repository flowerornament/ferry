//! Exact-commit resolution and committed declarations for git and colocated jj.
use crate::config::{Config, Recipe, identifier, relative_path};
use crate::execution::Shipped;
use std::process::Stdio;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};
type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

pub struct Declaration {
    pub config: Config,
    pub recipes: BTreeMap<String, Recipe>,
}
#[derive(Default)]
pub struct DeclarationEvidence {
    pub adapter_sha256: Option<String>,
    pub contract_sha256: Option<String>,
}
fn adapter(root: &Path, sha: &str, path: &str) -> Result<Vec<u8>> {
    relative_path(path)?;
    let checkout = tempfile::tempdir()?;
    let mut archive = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir(root)?)
        .args(["archive", sha])
        .stdout(Stdio::piped())
        .spawn()?;
    let status = Command::new("tar")
        .args(["-xf", "-", "-C"])
        .arg(checkout.path())
        .stdin(archive.stdout.take().ok_or("archive stream absent")?)
        .status()?;
    if !archive.wait()?.success() || !status.success() {
        return Err("adapter checkout failed".into());
    }
    let bytes = output(
        Command::new(checkout.path().join(path))
            .current_dir(checkout.path())
            .env("FERRY_COMMIT", sha),
    )?;
    Ok(bytes)
}

pub fn output(command: &mut Command) -> Result<Vec<u8>> {
    let result = command.output()?;
    if !result.status.success() {
        return Err(format!(
            "command failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        )
        .into());
    }
    Ok(result.stdout)
}
/// `git` run in `dir`, its working tree.
pub fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    output(Command::new("git").current_dir(dir).args(args))
}
/// `git` on the repository at `git_dir`.
pub fn git_at(git_dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    output(Command::new("git").arg("--git-dir").arg(git_dir).args(args))
}
/// A full commit id: 40 hexadecimal digits.
pub fn is_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
/// A command's output as trimmed text.
fn text(bytes: Vec<u8>) -> Result<String> {
    Ok(String::from_utf8(bytes)?.trim().to_owned())
}
pub fn root() -> Result<PathBuf> {
    if let Ok(bytes) = output(Command::new("jj").args(["root"])) {
        return Ok(PathBuf::from(text(bytes)?));
    }
    let top = output(Command::new("git").args(["rev-parse", "--show-toplevel"]))?;
    Ok(PathBuf::from(text(top)?))
}
pub fn git_dir(root: &Path) -> Result<PathBuf> {
    if root.join(".jj").exists() {
        let dir = output(Command::new("jj").current_dir(root).args(["git", "root"]))?;
        return Ok(PathBuf::from(text(dir)?));
    }
    Ok(root.join(".git"))
}
pub fn commit(root: &Path, revision: Option<&str>) -> Result<String> {
    if revision.is_some_and(|value| value.starts_with('-')) {
        return Err("revision cannot be a command option".into());
    }
    let value = if root.join(".jj").exists() {
        let rev = revision.unwrap_or("@");
        let bytes = output(Command::new("jj").current_dir(root).args([
            "log",
            "--no-graph",
            "-r",
            rev,
            "-T",
            "commit_id ++ \" \" ++ conflict ++ \"\\n\"",
        ]))?;
        let text = String::from_utf8(bytes)?;
        let lines: Vec<_> = text.lines().collect();
        if lines.len() != 1 || !lines[0].ends_with(" false") {
            return Err("revision must name one conflict-free commit".into());
        }
        lines[0].split_whitespace().next().unwrap_or("").to_owned()
    } else {
        let revision = format!("{}^{{commit}}", revision.unwrap_or("HEAD"));
        text(git(
            root,
            &["rev-parse", "--verify", "--end-of-options", &revision],
        )?)?
    };
    if !is_commit(&value) {
        return Err("invalid commit identity".into());
    }
    Ok(value)
}
pub fn committed_file(root: &Path, sha: &str, path: &str) -> Result<Vec<u8>> {
    relative_path(path)?;
    git_at(&git_dir(root)?, &["show", &format!("{sha}:{path}")])
}

/// Resolve once without fetching, peeling tags, or substituting a local branch.
pub fn source_commit(root: &Path, source: &crate::config::Source) -> Result<String> {
    let published = match source {
        crate::config::Source::Revision(revision) => return commit(root, Some(revision)),
        crate::config::Source::Published(published) => published,
    };
    identifier(&published.remote)?;
    let reference = format!("refs/remotes/{}/{}", published.remote, published.branch);
    ref_name(&reference)?;
    let dir = git_dir(root)?;
    let value = text(git_at(
        &dir,
        &["rev-parse", "--verify", "--end-of-options", &reference],
    )?)?;
    if !is_commit(&value) {
        return Err("invalid tracking commit identity".into());
    }
    let kind = git_at(&dir, &["cat-file", "-t", &value])?;
    if kind != b"commit\n" {
        return Err("tracking ref must name a commit object".into());
    }
    Ok(value)
}
/// A `[[ship]]` entry resolved: the object, its caller repository, and
/// its receipt line.
pub type Resolved = (Shipped, PathBuf, serde_json::Value);

/// Resolves each `[[ship]]` entry once: the object as the place sees it, the
/// caller repository holding it, and what the receipt records. A ref or URL
/// declared twice (by ferry.toml and the adapter together) is refused.
pub fn ship(
    root: &Path,
    entries: &[crate::config::Ship],
) -> Result<Vec<Resolved>, Box<dyn std::error::Error>> {
    let mut resolved: Vec<Resolved> = vec![];
    for entry in entries {
        let (name, url, repo, from, commit) = match entry {
            crate::config::Ship::Ref(e) => {
                ref_name(&e.name)?;
                let from = serde_json::to_value(&e.from)?;
                (
                    Some(&e.name),
                    None,
                    root.to_path_buf(),
                    from,
                    source_commit(root, &e.from)?,
                )
            }
            crate::config::Ship::Url(e) => {
                let repo = root.join(&e.path);
                let commit = commit(&repo, Some(&e.commit))?;
                (None, Some(&e.url), repo, e.path.clone().into(), commit)
            }
        };
        let (name, url) = (name.cloned(), url.cloned());
        if let Some(twice) = resolved.iter().find_map(|(o, _, _)| {
            (name.is_some() && o.name == name || url.is_some() && o.url == url).then_some(o)
        }) {
            let what = twice.name.as_ref().or(twice.url.as_ref()).cloned();
            let message = format!("[[ship]] declares {} twice", what.unwrap_or_default());
            return Err(message.into());
        }
        let object = Shipped { name, url, commit };
        let mut recorded = serde_json::to_value(&object)?;
        recorded["from"] = from;
        resolved.push((object, repo, recorded));
    }
    Ok(resolved)
}

pub fn declaration(
    root: &Path,
    sha: &str,
    evidence: &mut DeclarationEvidence,
) -> Result<Declaration, Box<dyn std::error::Error>> {
    let mut config: Config = toml::from_str(&String::from_utf8(committed_file(
        root,
        sha,
        "ferry.toml",
    )?)?)?;
    if config.version != 1 {
        return Err("unsupported ferry.toml version".into());
    }
    crate::config::retired([&config.companion_refs, &config.mirror])?;
    config.hooks.check()?;
    let recipes = if let Some(catalog) = &config.catalog {
        if !config.recipe.is_empty() {
            return Err("catalog and inline recipes duplicate ownership".into());
        }
        match (&catalog.path, &catalog.adapter) {
            (Some(path), None) => {
                let value: toml::Value =
                    toml::from_str(&String::from_utf8(committed_file(root, sha, path)?)?)?;
                crate::config::catalog_recipes(&value, &catalog.table)?
            }
            (None, Some(path)) => {
                evidence.adapter_sha256 =
                    Some(crate::receipt::digest(&committed_file(root, sha, path)?));
                let emitted = adapter(root, sha, path)?;
                evidence.contract_sha256 = Some(crate::receipt::digest(&emitted));
                if emitted != adapter(root, sha, path)? {
                    return Err("catalog adapter is nondeterministic".into());
                }
                let contract: crate::config::CatalogContract = serde_json::from_slice(&emitted)?;
                if contract.version != 1 {
                    return Err("unsupported catalog contract version".into());
                }
                // Both may ship: a ref or URL declared twice is refused
                // where the entries are resolved.
                crate::config::retired([&contract.companion_refs, &contract.mirror])?;
                config.ship.extend(contract.ship);
                if let Some(environment) = contract.environment {
                    if config.environment.is_some() {
                        return Err("environment declarations have two owners".into());
                    }
                    config.environment = Some(environment);
                }
                if !contract.preserve.is_empty() {
                    if !config.preserve.is_empty() {
                        return Err("preserve paths have two declaration owners".into());
                    }
                    config.preserve = contract.preserve;
                }
                if !contract.provider.requires.is_empty() {
                    if !config.provider.requires.is_empty() {
                        return Err("provider requirements have two owners".into());
                    }
                    config.provider = contract.provider;
                }
                contract.recipe
            }
            _ => return Err("catalog needs exactly one path or adapter".into()),
        }
    } else {
        config.recipe.clone()
    };
    for (name, recipe) in &recipes {
        identifier(name)?;
        if recipe.command.is_empty() || recipe.command[0].is_empty() {
            return Err(format!("recipe {name} has no command").into());
        }
        for artifact in &recipe.artifacts {
            relative_path(artifact)?;
        }
    }
    if evidence.contract_sha256.is_none() {
        evidence.contract_sha256 = Some(crate::receipt::digest(&serde_json::to_vec(&recipes)?));
    }
    Ok(Declaration { config, recipes })
}

pub fn ref_name(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !name.starts_with("refs/") {
        return Err("ship ref must be fully qualified".into());
    }
    output(Command::new("git").args(["check-ref-format", name]))?;
    Ok(())
}
