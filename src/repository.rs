//! Exact-commit resolution and committed declarations for git and colocated jj.
use crate::config::{Config, Recipe, Refusal, identifier, relative_path};
use std::process::Stdio;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

pub struct Declaration {
    pub config: Config,
    pub recipes: BTreeMap<String, Recipe>,
}
#[derive(Default)]
pub struct DeclarationEvidence {
    pub adapter_sha256: Option<String>,
    pub contract_sha256: Option<String>,
}
fn adapter(root: &Path, sha: &str, path: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
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

pub fn output(command: &mut Command) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let result = command.output()?;
    if !result.status.success() {
        return Err(Refusal::Declaration(format!(
            "command failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ))
        .into());
    }
    Ok(result.stdout)
}
pub fn root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Ok(bytes) = output(Command::new("jj").args(["root"])) {
        return Ok(PathBuf::from(String::from_utf8(bytes)?.trim()));
    }
    Ok(PathBuf::from(
        String::from_utf8(output(
            Command::new("git").args(["rev-parse", "--show-toplevel"]),
        )?)?
        .trim(),
    ))
}
pub fn git_dir(root: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if root.join(".jj").exists() {
        return Ok(PathBuf::from(
            String::from_utf8(output(
                Command::new("jj").current_dir(root).args(["git", "root"]),
            )?)?
            .trim(),
        ));
    }
    Ok(root.join(".git"))
}
pub fn commit(root: &Path, revision: Option<&str>) -> Result<String, Box<dyn std::error::Error>> {
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
            return Err(
                Refusal::Declaration("revision must name one conflict-free commit".into()).into(),
            );
        }
        lines[0].split_whitespace().next().unwrap_or("").to_owned()
    } else {
        String::from_utf8(output(Command::new("git").current_dir(root).args([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{}^{{commit}}", revision.unwrap_or("HEAD")),
        ]))?)?
        .trim()
        .into()
    };
    if value.len() != 40 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Refusal::Declaration("invalid commit identity".into()).into());
    }
    Ok(value)
}
pub fn committed_file(
    root: &Path,
    sha: &str,
    path: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    relative_path(path)?;
    let dir = git_dir(root)?;
    output(
        Command::new("git")
            .arg("--git-dir")
            .arg(dir)
            .args(["show", &format!("{sha}:{path}")]),
    )
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
        return Err(Refusal::Declaration("unsupported ferry.toml version".into()).into());
    }
    for hook in [
        &config.hooks.prepare,
        &config.hooks.env,
        &config.hooks.collect,
    ]
    .into_iter()
    .flatten()
    {
        relative_path(hook)?;
    }
    let recipes = if let Some(catalog) = &config.catalog {
        if !config.recipe.is_empty() {
            return Err(Refusal::Declaration(
                "catalog and inline recipes duplicate ownership".into(),
            )
            .into());
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
                if !contract.mirror.is_empty() {
                    if !config.mirror.is_empty() {
                        return Err("mirror declarations have two owners".into());
                    }
                    config.mirror = contract.mirror;
                }
                if let Some(environment) = contract.environment {
                    if config.environment.is_some() {
                        return Err("environment declarations have two owners".into());
                    }
                    config.environment = Some(environment);
                }
                if !contract.companion_refs.is_empty() {
                    if !config.companion_refs.is_empty() {
                        return Err("companion refs have two owners".into());
                    }
                    config.companion_refs = contract.companion_refs;
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
            return Err(Refusal::Declaration(format!("recipe {name} has no command")).into());
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
        return Err("companion ref must be fully qualified".into());
    }
    output(Command::new("git").args(["check-ref-format", name]))?;
    Ok(())
}
