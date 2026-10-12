//! Repository declarations and feature routing; no host-specific setup lives here.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("{0}")]
    Declaration(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub command: Vec<String>,
    #[serde(default)]
    pub requires: BTreeSet<String>,
    #[serde(default)]
    pub routable: bool,
    #[serde(default)]
    pub artifacts: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    pub prepare: Option<String>,
    pub env: Option<String>,
    pub collect: Option<String>,
}

/// One object a run ships with its commit (SPEC §10): an extra ref in the
/// checkout, or a dependency served from the place's store at its URL.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Ship {
    Ref(ShipRef),
    Url(ShipUrl),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShipRef {
    #[serde(rename = "ref")]
    pub name: String,
    pub from: Source,
}

/// A dependency at a caller-local repository path, pinned to `commit`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShipUrl {
    pub url: String,
    pub path: String,
    pub commit: String,
}

/// The keys `[[ship]]` replaced (`companion_refs`, `mirror`): present,
/// they refuse, naming the new form.
pub fn retired(keys: [&Option<serde::de::IgnoredAny>; 2]) -> Result<(), Refusal> {
    if keys.iter().any(|key| key.is_some()) {
        return Err(Refusal::Declaration(
            "[companion_refs]/[[mirror]] are retired: use [[ship]] (see README)".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub recipe: BTreeMap<String, Recipe>,
    pub catalog: Option<Catalog>,
    #[serde(default)]
    pub hooks: Hooks,
    #[serde(default)]
    pub ship: Vec<Ship>,
    pub environment: Option<Environment>,
    #[serde(default)]
    pub companion_refs: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    pub mirror: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    pub preserve: Vec<String>,
    #[serde(default)]
    pub provider: Provider,
}

/// What a project requires of the Ferry that runs it. An older Ferry rejects
/// this table as an unknown key, so the requirement fails closed.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    #[serde(default)]
    pub requires: BTreeSet<String>,
}

/// A caller revision, or an explicitly selected locally known tracking ref.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Source {
    Revision(String),
    Published(PublishedRef),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedRef {
    pub remote: String,
    pub branch: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    #[serde(default)]
    pub forwards: BTreeSet<String>,
    #[serde(default)]
    pub watches: Vec<String>,
    #[serde(default)]
    pub local: BTreeSet<String>,
}

impl Environment {
    pub fn capture(
        &self,
        input: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    ) -> Result<BTreeMap<String, String>, Refusal> {
        for name in self.forwards.iter().chain(&self.local).chain(&self.watches) {
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(Refusal::Declaration(
                    "invalid environment declaration".into(),
                ));
            }
        }
        if !self.forwards.is_disjoint(&self.local)
            || self.forwards.iter().any(|name| reserved(name))
        {
            return Err(Refusal::Declaration(
                "conflicting environment declarations".into(),
            ));
        }
        let mut forwarded = BTreeMap::new();
        for (name, value) in input {
            let Some(name) = name.to_str() else { continue };
            if self.local.contains(name) {
                continue;
            }
            if self.forwards.contains(name) {
                let value = value.into_string().map_err(|_| {
                    Refusal::Declaration(format!("non-UTF-8 forwarded variable {name}"))
                })?;
                forwarded.insert(name.to_owned(), value);
            } else if !value.is_empty()
                && self.watches.iter().any(|prefix| name.starts_with(prefix))
            {
                return Err(Refusal::Declaration(format!(
                    "unlisted watched variable {name}"
                )));
            }
        }
        Ok(forwarded)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub path: Option<String>,
    pub adapter: Option<String>,
    #[serde(default = "recipe_table")]
    pub table: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogContract {
    pub version: u32,
    pub recipe: BTreeMap<String, Recipe>,
    #[serde(default)]
    pub ship: Vec<Ship>,
    pub environment: Option<Environment>,
    #[serde(default)]
    pub companion_refs: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    pub mirror: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    pub preserve: Vec<String>,
    #[serde(default)]
    pub provider: Provider,
}
fn recipe_table() -> String {
    "recipe".into()
}

pub fn catalog_recipes(
    value: &toml::Value,
    table: &str,
) -> Result<BTreeMap<String, Recipe>, Refusal> {
    let entries = value
        .get(table)
        .and_then(toml::Value::as_table)
        .ok_or_else(|| Refusal::Declaration("catalog table missing".into()))?;
    entries
        .iter()
        .map(|(name, entry)| {
            identifier(name)?;
            let source = entry
                .as_table()
                .ok_or_else(|| Refusal::Declaration("catalog entry must be a table".into()))?;
            let mut declaration = toml::Table::new();
            for key in ["command", "requires", "routable", "artifacts"] {
                if let Some(value) = source.get(key) {
                    declaration.insert(key.into(), value.clone());
                }
            }
            let recipe = toml::Value::Table(declaration)
                .try_into()
                .map_err(|e| Refusal::Declaration(format!("invalid catalog recipe {name}: {e}")))?;
            Ok((name.clone(), recipe))
        })
        .collect()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    pub ssh: String,
    pub triple: String,
    pub cores: Option<u32>,
    #[serde(default)]
    pub features: BTreeSet<String>,
    #[serde(default = "default_binary")]
    pub binary: String,
}
fn default_binary() -> String {
    ".local/bin/ferry".into()
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machines {
    #[serde(default)]
    pub host: BTreeMap<String, Machine>,
}

#[derive(Debug, PartialEq)]
pub enum Route<'a> {
    Local,
    Remote(&'a str),
}

impl Machine {
    pub fn capabilities(&self) -> BTreeSet<String> {
        let mut features = self.features.clone();
        let arch = self.triple.split('-').next().unwrap_or("");
        let os = if self.triple.contains("linux") {
            "linux"
        } else if self.triple.contains("darwin") {
            "macos"
        } else {
            "unknown"
        };
        features.insert(format!("{os}-{arch}"));
        features
    }
}

pub fn route<'a>(
    recipe: &Recipe,
    machines: &'a Machines,
    on: Option<&'a str>,
    local: bool,
) -> Result<Route<'a>, Refusal> {
    if local && on.is_some() {
        return Err(Refusal::Declaration("--local conflicts with --on".into()));
    }
    if local {
        return Ok(Route::Local);
    }
    if on.is_some() && !recipe.routable {
        return Err(Refusal::Declaration("recipe is not routable".into()));
    }
    if let Some(name) = on {
        let machine = machines
            .host
            .get(name)
            .ok_or_else(|| Refusal::Declaration(format!("unknown machine: {name}")))?;
        if supports(recipe, machine) {
            return Ok(Route::Remote(name));
        }
        return Err(Refusal::Declaration(format!(
            "machine {name} lacks required features"
        )));
    }
    if !recipe.routable || machines.host.is_empty() {
        return Ok(Route::Local);
    }
    machines
        .host
        .iter()
        .find(|(_, m)| supports(recipe, m))
        .map(|(name, _)| Route::Remote(name.as_str()))
        .ok_or_else(|| Refusal::Declaration("no configured machine meets requirements".into()))
}
fn supports(recipe: &Recipe, machine: &Machine) -> bool {
    let mut required = recipe.requires.clone();
    // Exclusivity is an execution lock, not a hardware capability.
    required.remove("exclusive");
    required.is_subset(&machine.capabilities())
}

pub fn identifier(value: &str) -> Result<(), Refusal> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(Refusal::Declaration(format!(
            "invalid identifier: {value:?}"
        )));
    }
    Ok(())
}
/// A name only Ferry sets: the run's identity and its git configuration.
pub fn reserved(name: &str) -> bool {
    name.starts_with("FERRY_") || name == "GIT_CONFIG_GLOBAL"
}
/// An environment entry a run never takes from a caller or a hook.
pub fn invalid_env(name: &str, value: &str) -> bool {
    name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') || reserved(name)
}
impl Hooks {
    /// Every hook is a plain relative path inside the checkout.
    pub fn check(&self) -> Result<(), Refusal> {
        let hooks = [&self.prepare, &self.env, &self.collect];
        hooks
            .into_iter()
            .flatten()
            .try_for_each(|hook| relative_path(hook))
    }
}
pub fn relative_path(value: &str) -> Result<(), Refusal> {
    if value.is_empty()
        || Path::new(value)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(Refusal::Declaration(format!(
            "path must stay inside checkout: {value:?}"
        )));
    }
    Ok(())
}

pub fn preserve_path(value: &str) -> Result<(), Refusal> {
    relative_path(value)?;
    if value
        .split('/')
        .any(|part| matches!(part, ".git" | ".jj" | ".ferry"))
    {
        return Err(Refusal::Declaration(
            "cannot preserve version-control or Ferry state".into(),
        ));
    }
    if value
        .chars()
        .any(|c| matches!(c, '*' | '?' | '[' | ']' | '\\' | '!' | '\n' | '\r'))
    {
        return Err(Refusal::Declaration(
            "preserve must name literal directories".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_catalog_metadata_is_not_duplicated_or_treated_as_an_environment() {
        let value = toml::from_str("[recipe.test]\ncommand=['just','test']\nkind='gate'\nrunner='just'\nrequires=['linux-x86_64']\nroutable=true\n[recipe.publish]\ncommand=['just','publish']\nkind='gate'\n").unwrap();
        let recipes = catalog_recipes(&value, "recipe").unwrap();
        assert_eq!(recipes["test"].command, ["just", "test"]);
        assert!(recipes["test"].routable);
        assert!(!recipes["publish"].routable);
    }
    #[test]
    fn catalog_commands_are_never_inferred_from_recipe_names() {
        let value = toml::from_str("[recipe.test]\nkind='gate'\n").unwrap();
        assert!(catalog_recipes(&value, "recipe").is_err());
    }
    fn recipe() -> Recipe {
        Recipe {
            command: vec!["true".into()],
            requires: BTreeSet::from(["linux-x86_64".into()]),
            routable: true,
            artifacts: vec![],
        }
    }
    fn machines() -> Machines {
        Machines {
            host: BTreeMap::from([(
                "bench".into(),
                Machine {
                    ssh: "bench".into(),
                    triple: "x86_64-unknown-linux-gnu".into(),
                    cores: Some(16),
                    features: BTreeSet::new(),
                    binary: default_binary(),
                },
            )]),
        }
    }
    #[test]
    fn absent_configuration_is_local() {
        assert_eq!(
            route(&recipe(), &Machines::default(), None, false).unwrap(),
            Route::Local
        );
    }
    #[test]
    fn architecture_is_reported_as_a_feature() {
        assert_eq!(
            route(&recipe(), &machines(), None, false).unwrap(),
            Route::Remote("bench")
        );
    }
    #[test]
    fn configured_mismatch_never_falls_back() {
        let mut r = recipe();
        r.requires.insert("gpu".into());
        assert!(route(&r, &machines(), None, false).is_err());
        assert_eq!(route(&r, &machines(), None, true).unwrap(), Route::Local);
    }
    #[test]
    fn publication_cannot_be_forced_remote() {
        let mut r = recipe();
        r.routable = false;
        assert!(route(&r, &machines(), Some("bench"), false).is_err());
    }
    #[test]
    fn unknown_machine_is_a_refusal() {
        assert!(route(&recipe(), &Machines::default(), Some("missing"), false).is_err());
    }
    #[test]
    fn exclusive_is_a_lock_not_hardware() {
        let mut r = recipe();
        r.requires.insert("exclusive".into());
        assert_eq!(
            route(&r, &machines(), None, false).unwrap(),
            Route::Remote("bench")
        );
    }
    #[test]
    fn names_and_paths_cannot_be_shell_arguments_or_escape() {
        for s in ["", "-recipe", "rv;touch", "../escape", "two seats"] {
            assert!(identifier(s).is_err());
        }
        for p in ["/absolute", "../escape", "a/../../b", ""] {
            assert!(relative_path(p).is_err());
        }
        assert!(relative_path("scripts/prepare").is_ok());
    }
}
