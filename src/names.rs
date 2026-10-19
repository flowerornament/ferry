//! Version 2's plain names (SPEC §10). Ferry reads each file by its version 1
//! names; a version 2 file is renamed to them first, its sizes and durations
//! turned into bytes and seconds. A version 2 file that holds a version 1 key
//! is refused, naming the new key. Version 1 is read as before, for one
//! release, and then deleted with this module's other half.
use toml::{Table, Value};

/// A renamed key: its version 2 name, then the version 1 name Ferry reads.
type Rename = (&'static str, &'static str);

const PROJECT: &[Rename] = &[("keep_between_runs", "preserve"), ("also_send", "ship")];
const RECIPE: &[Rename] = &[("needs", "requires"), ("may_run_elsewhere", "routable")];
const ENVIRONMENT: &[Rename] = &[
    ("send", "forwards"),
    ("refuse_other_vars_starting_with", "watches"),
    ("keep_local", "local"),
];
const PROVIDER: &[Rename] = &[("needs", "requires")];
const MACHINES: &[Rename] = &[("machine", "host")];
const MACHINE: &[Rename] = &[("platform", "triple"), ("has", "features")];
const HOST: &[Rename] = &[
    ("refuse_runs_below_free_disk", "min_free_bytes"),
    ("max_cache_per_checkout", "seat_cap_bytes"),
];

/// A file's version: `version = <n>`, else `absent` (a version 1 file that
/// predates the key, as `machines.toml` does); -1 when it is not a whole
/// number.
pub fn version(value: &Value, absent: i64) -> i64 {
    value
        .get("version")
        .map_or(Some(absent), Value::as_integer)
        .unwrap_or(-1)
}

/// Which of the three files a refusal is about, so it is told in that
/// file's own names.
#[derive(Clone, Copy)]
pub enum File {
    Project,
    Machines,
    Host,
}

/// Refuses a version 1 key in an adapter's version 2 JSON, naming the new
/// key, whatever its value (null included), before it is read natively.
pub fn refuse_version_one_json(json: &serde_json::Value) -> Result<(), String> {
    let refuse = |table: Option<&serde_json::Value>, renames: &[Rename], at: &str| {
        let Some(table) = table.and_then(serde_json::Value::as_object) else {
            return Ok(());
        };
        match renames.iter().find(|(_, old)| table.contains_key(*old)) {
            Some((new, old)) => Err(format!("{at}`{old}` is version 1: use `{new}`")),
            None => Ok(()),
        }
    };
    refuse(Some(json), PROJECT, "")?;
    if let Some(recipes) = json.get("recipe").and_then(serde_json::Value::as_object) {
        for (name, recipe) in recipes {
            refuse(Some(recipe), RECIPE, &format!("recipe.{name}."))?;
        }
    }
    refuse(json.get("environment"), ENVIRONMENT, "environment.")?;
    refuse(json.get("provider"), PROVIDER, "provider.")
}

/// Renames `table`'s version 2 keys to version 1's, refusing a version 1
/// key, which names the new one.
fn rename(table: &mut Table, renames: &[Rename], at: &str) -> Result<(), String> {
    for (new, old) in renames {
        if table.contains_key(*old) {
            return Err(format!("{at}`{old}` is version 1: use `{new}`"));
        }
    }
    for (new, old) in renames {
        if let Some(value) = table.remove(*new) {
            table.insert((*old).into(), value);
        }
    }
    Ok(())
}

/// Each table under `key`, by name.
fn each<'a>(top: &'a mut Table, key: &str) -> impl Iterator<Item = (&'a String, &'a mut Table)> {
    top.get_mut(key)
        .and_then(Value::as_table_mut)
        .into_iter()
        .flat_map(|tables| tables.iter_mut())
        .filter_map(|(name, value)| Some((name, value.as_table_mut()?)))
}

/// A version 2 `ferry.toml`, or a catalog adapter's version 2 JSON, renamed.
pub fn project(value: &mut Value) -> Result<(), String> {
    let top = value.as_table_mut().ok_or("not a table")?;
    rename(top, PROJECT, "")?;
    for (name, recipe) in each(top, "recipe") {
        rename(recipe, RECIPE, &format!("recipe.{name}."))?;
    }
    for (key, renames) in [("environment", ENVIRONMENT), ("provider", PROVIDER)] {
        if let Some(table) = top.get_mut(key).and_then(Value::as_table_mut) {
            rename(table, renames, &format!("{key}."))?;
        }
    }
    Ok(())
}

/// A version 2 `machines.toml`, renamed. `cores` is gone (routing never
/// read it).
pub fn machines(value: &mut Value) -> Result<(), String> {
    let top = value.as_table_mut().ok_or("not a table")?;
    rename(top, MACHINES, "")?;
    for (name, machine) in each(top, "host") {
        if machine.contains_key("cores") {
            return Err(format!(
                "machine.{name}.`cores` is gone in version 2: routing never read it"
            ));
        }
        rename(machine, MACHINE, &format!("machine.{name}."))?;
    }
    Ok(())
}

/// A version 2 `host.toml`, renamed, its sizes in bytes and `max_run_time`
/// in seconds.
pub fn host(value: &mut Value) -> Result<(), String> {
    let top = value.as_table_mut().ok_or("not a table")?;
    if top.contains_key("evict") {
        return Err("`evict` is retired: seats are bounded by max_cache_per_checkout".into());
    }
    // SPEC §10's table: never read by Ferry, but named there as version 1's.
    if top.contains_key("max_timeout") {
        return Err("`max_timeout` is version 1: use `max_run_time`".into());
    }
    rename(top, HOST, "")?;
    for (_, old) in HOST {
        if let Some(size) = top.get(*old) {
            let bytes = size
                .as_str()
                .ok_or_else(|| format!("{}: {}", new_name(old), SIZE))
                .and_then(|text| parse_size(text).map_err(|e| format!("{}: {e}", new_name(old))))?;
            top.insert(
                (*old).into(),
                Value::Integer(
                    i64::try_from(bytes).map_err(|_| format!("{}: too large", new_name(old)))?,
                ),
            );
        }
    }
    if let Some(time) = top.get("max_run_time") {
        let seconds = time
            .as_str()
            .ok_or_else(|| "a duration, like \"6h\"".to_owned())
            .and_then(crate::config::parse_duration)
            .map(|duration| duration.as_secs())
            .and_then(|seconds| {
                // A run needs a moment; past a week, deadlines stop meaning
                // anything an operator would check.
                if (1..=MAX_RUN_TIME_SECONDS).contains(&seconds) {
                    Ok(seconds)
                } else {
                    Err("from 1s to 7d".to_owned())
                }
            })
            .map_err(|e| format!("max_run_time: {e}"))?;
        top.insert(
            "max_run_time".into(),
            Value::Integer(i64::try_from(seconds).map_err(|_| "max_run_time: too large")?),
        );
    }
    Ok(())
}

/// The version 2 name of a renamed `host.toml` key.
fn new_name(old: &str) -> &'static str {
    HOST.iter()
        .find(|(_, v1)| *v1 == old)
        .map_or("", |(v2, _)| v2)
}

/// The most `max_run_time` may be: seven days.
const MAX_RUN_TIME_SECONDS: u64 = 7 * 86_400;

const SIZE: &str = "a size with a unit, like \"40 GiB\" or \"512 MiB\" (or GB, MB: decimal)";

/// A size with its unit (SPEC §10): a whole number, an optional space, and
/// one of B, KiB, MiB, GiB, TiB (binary) or kB, MB, GB, TB (decimal),
/// case-sensitive.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let bad = || format!("{text:?}: {SIZE}");
    let digits = text.trim_end_matches(|c: char| !c.is_ascii_digit());
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let rest = &text[digits.len()..];
    let unit = rest.strip_prefix(' ').unwrap_or(rest);
    let multiple: u64 = match unit {
        "B" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        "kB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        _ => return Err(bad()),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|count| count.checked_mul(multiple))
        .ok_or_else(bad)
}

/// A refusal of a version 2 file in its own names: what serde said about the
/// version 1 names Ferry read it by.
pub fn spoken(message: &str, file: File) -> String {
    let mut message = message.to_owned();
    let renames = match file {
        File::Project => [PROJECT, RECIPE, ENVIRONMENT, PROVIDER].concat(),
        File::Machines => [MACHINES, MACHINE].concat(),
        File::Host => HOST.to_vec(),
    };
    for (new, old) in renames {
        message = message.replace(&format!("`{old}`"), &format!("`{new}`"));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn sizes_carry_their_unit() {
        assert_eq!(parse_size("40 GiB"), Ok(40 << 30));
        assert_eq!(parse_size("512MiB"), Ok(512 << 20));
        assert_eq!(parse_size("40 GB"), Ok(40_000_000_000));
        for bad in [
            "40", "40 gib", "1.5 GiB", "GiB", "-1 GiB", "40 XB", "", "+40 GiB", "40  GiB",
            "40\tGiB",
        ] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }
}
