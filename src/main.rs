//! Ferry runs committed repository recipes locally or over SSH.
mod admission;
mod artifacts;
mod config;
mod custody;
mod execution;
mod limits;
mod lock;
mod place;
mod receipt;
mod record;
mod repository;
mod run;
mod seats;
mod transport;

use clap::{Args, Parser, Subcommand};
use receipt::note;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(
    version,
    about = "Run this repo's recipe at this commit on a machine that can run it."
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    Run(Run),
    Recipes,
    Machines,
    Receipt {
        id: String,
    },
    /// This caller's seats on its places.
    Seats {
        #[command(subcommand)]
        command: SeatsAction,
    },
    /// Follow a run's output from any terminal until it ends.
    Follow {
        id: String,
    },
    /// Stop a run: your own, or another caller's with --other --reason.
    Stop {
        id: String,
        #[arg(long, requires = "reason")]
        other: bool,
        #[arg(long, requires = "other")]
        reason: Option<String>,
    },
    /// Print a run's receipt, completing it from the place once it is final.
    Record {
        id: String,
    },
    /// Refuses callers that predate detached runs.
    #[command(hide = true)]
    HostRun {
        #[arg(long)]
        state: Option<PathBuf>,
    },
    /// The place side of a run.
    #[command(hide = true)]
    Place {
        /// The state root (default `~/.local/state/ferry`); the local route
        /// passes the repository's `.ferry/local`.
        #[arg(long)]
        state: Option<PathBuf>,
        #[command(subcommand)]
        verb: PlaceVerb,
    },
    #[command(hide = true)]
    HostInfo,
    #[command(hide = true)]
    ExecuteSession {
        spec: PathBuf,
        result: PathBuf,
    },
}
#[derive(Subcommand)]
enum SeatsAction {
    /// Retire this caller's seats whose workspace is gone (SPEC §9).
    Prune {
        #[arg(long)]
        on: Option<String>,
    },
}
#[derive(Subcommand)]
enum PlaceVerb {
    Submit,
    Supervise {
        id: String,
    },
    Follow {
        id: String,
        offset: u64,
    },
    Stop {
        id: String,
        #[arg(long)]
        seat: String,
        #[arg(long)]
        who: String,
        #[arg(long)]
        why: String,
        #[arg(long)]
        other: bool,
        #[arg(long, default_value_t = 15)]
        signal: i32,
    },
    Record {
        id: String,
    },
    Retire {
        key: String,
    },
}
#[derive(Args)]
struct Run {
    #[arg(allow_hyphen_values = true)]
    recipe: String,
    #[arg(long)]
    on: Option<String>,
    #[arg(long)]
    local: bool,
    #[arg(long)]
    commit: Option<String>,
    #[arg(long, default_value = "default", allow_hyphen_values = true)]
    seat: String,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    timeout_seconds: Option<u64>,
    /// How long the run may wait for its locks, all together: every wait
    /// ends this long after submit (default 30 min).
    #[arg(long)]
    wait_seconds: Option<u64>,
    /// Hold the host alone: wait for in-flight runs, keep new ones out.
    #[arg(long)]
    exclusive: bool,
    /// Arguments appended to the recipe's command, after `--`.
    #[arg(last = true)]
    args: Vec<String>,
}
fn machines() -> Result<config::Machines> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|v| PathBuf::from(v).join(".config")))
        .ok_or("no configuration directory")?;
    match fs::read_to_string(base.join("ferry/machines.toml")) {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(config::Machines::default()),
        Err(e) => Err(e.into()),
    }
}
fn dispatch(root: &Path, options: &Run, record: &mut receipt::Receipt) -> Result<()> {
    config::identifier(&options.recipe)?;
    config::identifier(&options.seat)?;
    let sha = repository::commit(root, options.commit.as_deref())?;
    record.commit = Some(sha.clone());
    let mut evidence = repository::DeclarationEvidence::default();
    let declared = repository::declaration(root, &sha, &mut evidence);
    record.adapter_sha256 = evidence.adapter_sha256;
    record.contract_sha256 = evidence.contract_sha256;
    let declared = declared?;
    execution::interrupted()?;
    let config = declared.config;
    for path in &config.preserve {
        config::preserve_path(path)?;
    }
    record.preserve.clone_from(&config.preserve);
    record.provider_requires = config.provider.requires.iter().cloned().collect();
    receipt::provides(&config.provider.requires, &receipt::capabilities())
        .map_err(|e| format!("this caller: {e}"))?;
    let shipped = repository::ship(root, &config.ship)?;
    record.ship = shipped
        .iter()
        .map(|(_, _, recorded)| recorded.clone())
        .collect();
    record.forwarded_env = config.environment.as_ref().map_or_else(
        || Ok(BTreeMap::default()),
        |environment| environment.capture(std::env::vars_os()),
    )?;
    let mut declaration = declared
        .recipes
        .get(&options.recipe)
        .ok_or("unknown recipe")?
        .clone();
    if options.exclusive {
        declaration.requires.insert("exclusive".into());
    }
    let declaration = &declaration;
    let hosts = machines()?;
    let route = config::route(declaration, &hosts, options.on.as_deref(), options.local)?;
    record.requirements = declaration.requires.iter().cloned().collect();
    if let config::Route::Remote(name) = route {
        name.clone_into(&mut record.machine);
        record.machine_features = hosts.host[name].capabilities().into_iter().collect();
    }
    let request = execution::Request {
        protocol: receipt::PROTOCOL,
        id: record.id.clone(),
        token: receipt::run_id()?,
        commit: sha,
        seat: transport::seat_key(root, &options.seat)?,
        recipe: declaration.clone(),
        hooks: config.hooks,
        caller_binary_sha256: record.caller_binary_sha256.clone(),
        timeout_seconds: options.timeout_seconds,
        forwarded_env: record.forwarded_env.clone(),
        ship: shipped
            .iter()
            .map(|(object, _, _)| object.clone())
            .collect(),
        preserve: record.preserve.clone(),
        args: options.args.clone(),
        provider_requires: config.provider.requires.clone(),
        wait_seconds: options.wait_seconds,
        waits_end_ms: None,
    };
    record.token = Some(request.token.clone());
    if matches!(route, config::Route::Remote(_)) {
        record.outcome = receipt::Outcome::Lost;
        record.exit_code = 255;
    }
    execution::interrupted()?;
    // Every caller fact, stored before the host is asked to run anything:
    // lost until the final receipt replaces it.
    let mut submitted = record.clone();
    submitted.state = Some("submitted".into());
    submitted.outcome = receipt::Outcome::Lost;
    submitted.exit_code = 255;
    submitted.reason = Some("submitted; no verdict yet".into());
    receipt::submit(root, &submitted)?;
    let place = match route {
        config::Route::Local => transport::Place::Local(root.join(".ferry/local")),
        config::Route::Remote(name) => transport::Place::Remote(&hosts.host[name]),
    };
    let objects: Vec<_> = shipped
        .into_iter()
        .map(|(object, repo, _)| (repo, object.commit))
        .collect();
    transport::ship(root, &place, &request, &objects, record)?;
    transport::run_on(root, &place, &request, record, options.json)
}

/// The place a stored receipt's run went to.
fn place_of<'a>(
    root: &Path,
    record: &receipt::Receipt,
    hosts: &'a config::Machines,
) -> Result<transport::Place<'a>> {
    if record.machine == "local" {
        return Ok(transport::Place::Local(root.join(".ferry/local")));
    }
    Ok(transport::Place::Remote(
        hosts
            .host
            .get(&record.machine)
            .ok_or_else(|| format!("unknown machine {}", record.machine))?,
    ))
}

/// Re-attaches to a stored run (`follow`) or reads its record (`record`),
/// and stores the receipt as far as the place's record allows.
fn attach(id: &str, follow: bool) -> Result<i32> {
    execution::stopped()?;
    let root = repository::root()?;
    let hosts = machines()?;
    let mut record = receipt::load(&root, id)?;
    if record.state.as_deref() == Some("submitted") {
        let place = place_of(&root, &record, &hosts)?;
        if follow {
            transport::finish(&root, &place, &mut record, false);
            receipt::save(&root, &record)?;
        } else {
            match transport::complete(&root, &place, &mut record) {
                Ok(()) => receipt::save(&root, &record)?,
                Err(error) => execution::warn(&format!("ferry: {error}")),
            }
        }
    }
    execution::emit(serde_json::to_string_pretty(&record)?.as_bytes());
    Ok(match record.state.as_deref() {
        Some("submitted") if !follow => 3,
        _ => record.exit_code,
    })
}

fn stop(id: &str, reason: Option<&str>) -> Result<i32> {
    let root = repository::root()?;
    let hosts = machines()?;
    let record = receipt::load(&root, id)?;
    let place = place_of(&root, &record, &hosts)?;
    match transport::stop(
        &root,
        &place,
        &record,
        reason.unwrap_or("ferry stop"),
        reason.is_some(),
        15,
    ) {
        Ok(reply) => {
            execution::emit(reply.as_bytes());
            Ok(0)
        }
        Err(error) => {
            execution::warn(&format!("ferry: {error}"));
            Ok(125)
        }
    }
}
fn run(options: &Run) -> Result<i32> {
    // Catch INT, TERM and HUP from the start, so an early signal still ends
    // in a receipt rather than the default action.
    execution::stopped()?;
    let root = repository::root()?;
    let start = receipt::now_ms();
    let mut record = new_record(&options.recipe, &options.seat)?;
    record.args.clone_from(&options.args);
    record.args_sha256 = Some(receipt::digest(&serde_json::to_vec(&options.args)?));
    if let Err(error) = dispatch(&root, options, &mut record) {
        record.reason = Some(error.to_string());
    }
    // The place's verdict stands, whatever its outcome (a stop is its own
    // verdict); a caller signal after it is only noted. A run the caller
    // left keeps its submitted receipt. Only a signal before the place
    // decided anything is lost 128+n (ferry-xtx.14).
    let submitted = record.state.as_deref() == Some("submitted");
    let decided = record.state.as_deref() == Some("final");
    if let Some(signal) = execution::caught_signal()
        && !submitted
    {
        let cause = format!("caller received {}", execution::signal_name(signal));
        if decided {
            if record.outcome != receipt::Outcome::Stopped {
                note(&mut record.reason, format!("after the verdict: {cause}"));
            }
        } else {
            record.outcome = receipt::Outcome::Lost;
            record.exit_code = 128 + signal;
            record.reason = Some(match record.reason.take() {
                Some(reason) if reason.starts_with(&cause) => reason,
                Some(reason) => format!("{cause}; {reason}"),
                None => cause,
            });
        }
    }
    record.elapsed_ms = receipt::now_ms() - start;
    if !submitted {
        record.state = Some("final".into());
    }
    receipt::save(&root, &record)?;
    // The receipt is stored; a closed stdout or stderr never changes the code.
    if options.json {
        execution::emit(
            serde_json::json!({"protocol": receipt::PROTOCOL, "event": "receipt", "receipt": record})
                .to_string()
                .as_bytes(),
        );
    } else {
        execution::warn(&format!(
            "ferry: {:?}: {} (receipt {})",
            record.outcome,
            record.reason.as_deref().unwrap_or(""),
            record.id
        ));
    }
    Ok(record.exit_code)
}

fn new_record(recipe: &str, seat: &str) -> Result<receipt::Receipt> {
    Ok(receipt::Receipt {
        protocol: receipt::PROTOCOL,
        id: receipt::run_id()?,
        commit: None,
        recipe: recipe.to_owned(),
        seat: seat.to_owned(),
        machine: "local".into(),
        outcome: receipt::Outcome::Refused,
        exit_code: 125,
        reason: None,
        caller_binary_sha256: receipt::digest(&fs::read(std::env::current_exe()?)?),
        host_binary_sha256: None,
        started_ms: receipt::now_ms(),
        elapsed_ms: 0,
        requirements: vec![],
        machine_features: vec![],
        adapter_sha256: None,
        contract_sha256: None,
        forwarded_env: BTreeMap::default(),
        ship: vec![],
        preserve: vec![],
        collection: None,
        args: vec![],
        args_sha256: None,
        caller_source_rev: Some(receipt::source_rev()),
        caller_exe: receipt::exe_path(),
        host_source_rev: None,
        host_exe: None,
        custody: None,
        lease: None,
        admission: None,
        pruned: None,
        provider_requires: vec![],
        output: None,
        deadline_ms: None,
        stopped_by: None,
        token: None,
        state: None,
    })
}

fn execute(cli: Cli) -> Result<i32> {
    match cli.command {
        Action::Run(options) => run(&options),
        Action::Seats {
            command: SeatsAction::Prune { on },
        } => seats::prune(on.as_deref(), &machines()?),
        Action::Machines => {
            println!("{}", serde_json::to_string_pretty(&machines()?.host)?);
            Ok(0)
        }
        Action::Recipes => {
            let root = repository::root()?;
            let sha = repository::commit(&root, None)?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &repository::declaration(
                        &root,
                        &sha,
                        &mut repository::DeclarationEvidence::default()
                    )?
                    .recipes
                )?
            );
            Ok(0)
        }
        Action::Receipt { id } => {
            config::identifier(&id)?;
            print!(
                "{}",
                fs::read_to_string(
                    repository::root()?
                        .join(".ferry/receipts")
                        .join(format!("{id}.json"))
                )?
            );
            Ok(0)
        }
        Action::HostRun { .. } => Ok(place::host_run()),
        Action::Follow { id } => attach(&id, true),
        Action::Record { id } => attach(&id, false),
        Action::Stop { id, reason, .. } => stop(&id, reason.as_deref()),
        Action::Place { state, verb } => {
            let root = place::root(state)?;
            match verb {
                PlaceVerb::Submit => place::submit(&root),
                PlaceVerb::Supervise { id } => place::supervise(&root, &id),
                PlaceVerb::Follow { id, offset } => place::follow(&root, &id, offset),
                PlaceVerb::Stop {
                    id,
                    seat,
                    who,
                    why,
                    other,
                    signal,
                } => place::stop(&root, &id, &seat, &who, &why, other, signal),
                PlaceVerb::Record { id } => place::print_record(&root, &id),
                PlaceVerb::Retire { key } => place::retire(&root, &key),
            }
        }
        Action::HostInfo => {
            println!("{}", serde_json::to_string(&transport::host_info()?)?);
            Ok(0)
        }
        Action::ExecuteSession { spec, result } => {
            crate::run::execute_session(&spec, &result)?;
            Ok(0)
        }
    }
}
fn main() {
    let code = match execute(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            execution::warn(&format!("ferry: {error}"));
            125
        }
    };
    transport::close_connections();
    std::process::exit(code);
}
