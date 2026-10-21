//! Ferry runs committed repository recipes locally or over SSH.
mod admission;
mod artifacts;
mod config;
mod custody;
mod execution;
mod history;
mod limits;
mod lock;
mod names;
mod place;
mod receipt;
mod record;
mod repository;
mod run;
mod seats;
mod status;
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
    /// Run a recipe at a commit, here or on a machine that has what it needs.
    #[command(after_help = RUN_HELP)]
    Run(Run),
    /// This repository's recipes at the current commit, one line each.
    Recipes,
    /// The machines in ~/.config/ferry/machines.toml, one line each.
    Machines,
    /// This caller's seats on its places.
    Seats {
        #[command(subcommand)]
        command: SeatsAction,
    },
    /// What a place sees now: disk, live runs, the gate, seats, leftovers.
    /// Read-only: it takes no lock and stops nothing.
    Status {
        /// The machine, by its name in machines.toml (default: this
        /// repository's local place).
        #[arg(long)]
        on: Option<String>,
        /// Print one JSON document instead of lines.
        #[arg(long)]
        json: bool,
    },
    /// Show a run's output from the start, from any terminal, until it ends.
    Follow {
        /// The run id, as `ferry run` printed it.
        id: String,
    },
    /// Stop a run: your own, or another caller's with --other --reason.
    Stop {
        /// The run id, as `ferry run` printed it.
        id: String,
        /// The run is another caller's: say so, with --reason.
        #[arg(long, requires = "reason")]
        other: bool,
        /// Why you stop another caller's run; its receipt records it.
        #[arg(long, requires = "other")]
        reason: Option<String>,
    },
    /// Print a run's receipt as JSON, completing it from the place once it
    /// is final (exit 3 while it is not).
    Record {
        /// The run id, as `ferry run` printed it.
        id: String,
    },
    /// The place side of a run.
    #[command(hide = true)]
    Place {
        /// The state root (default `~/.local/state/ferry/v1`); the local
        /// route passes the repository's `.ferry/local/v1`.
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
        /// When the session must have ended (unix ms), whatever happens to
        /// its supervisor.
        deadline_ms: u128,
        /// The supervisor's pid, so an owner whose supervisor died before
        /// it started knows it is unsupervised.
        supervisor: i32,
    },
}
#[derive(Subcommand)]
enum SeatsAction {
    /// Retire this caller's seats whose workspace is gone, and with
    /// `--idle <dur>` those it has not used for that long (SPEC §9).
    Prune {
        /// Only this machine's seats, by its name in machines.toml (default:
        /// every machine's).
        #[arg(long)]
        on: Option<String>,
        /// Also retire seats unused for this long (`1d`, `12h`, `0s`).
        #[arg(long)]
        idle: Option<String>,
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
        /// Refuse `used` if the seat ran within this many ms.
        #[arg(long)]
        idle_ms: Option<u128>,
        /// Refuse `used` unless the seat's latest run is this one.
        #[arg(long)]
        once_id: Option<String>,
    },
    Status,
}
#[derive(Args)]
// Each bool is one command-line flag, which clap reads as a bool.
#[allow(clippy::struct_excessive_bools)]
struct Run {
    /// The recipe's name (`ferry recipes` lists them).
    #[arg(allow_hyphen_values = true)]
    recipe: String,
    /// Run on this machine, by its name in machines.toml.
    #[arg(long, conflicts_with = "local")]
    on: Option<String>,
    /// Run here, in this repository's own place.
    #[arg(long)]
    local: bool,
    /// The commit or revision to run (default: HEAD in git, so uncommitted
    /// changes are not run; @ in jj, which holds them).
    #[arg(long)]
    commit: Option<String>,
    /// Which of this workspace's seats (checkouts) on the place to use.
    #[arg(long, default_value = "default", allow_hyphen_values = true)]
    seat: String,
    /// Print events as JSON lines on stdout; the recipe's output goes to
    /// stderr.
    #[arg(long)]
    json: bool,
    /// Stop the recipe after this many seconds (exit 124).
    #[arg(long)]
    timeout_seconds: Option<u64>,
    /// How long the run may wait for its locks, all together: every wait
    /// ends this long after submit (default 30 min).
    #[arg(long)]
    wait_seconds: Option<u64>,
    /// Hold the host alone: wait for in-flight runs, keep new ones out.
    #[arg(long)]
    exclusive: bool,
    /// The seat lives for this run only: retired once its record is final.
    #[arg(long)]
    once: bool,
    /// Arguments appended to the recipe's command, after `--`.
    #[arg(last = true)]
    args: Vec<String>,
}
/// What `ferry run --help` adds after its options.
const RUN_HELP: &str = "\
Without --on or --local, a recipe that may run elsewhere goes to the first
machine that has what it needs; any other recipe runs here.

Exit codes: the recipe's own code when it ran; 124 it timed out; 125 Ferry
refused it and nothing ran; 128+n stopped by signal n; 255 lost (the run may
go on: ferry record <id>).";

fn machines() -> Result<config::Machines> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|v| PathBuf::from(v).join(".config")))
        .ok_or("no configuration directory")?;
    match fs::read_to_string(base.join("ferry/machines.toml")) {
        Ok(text) => config::Machines::read(&text)
            .map_err(|e| format!("machines.toml is malformed: {e}").into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(config::Machines::default()),
        Err(e) => Err(e.into()),
    }
}
fn dispatch(root: &Path, options: &Run, record: &mut receipt::Receipt) -> Result<()> {
    config::identifier(&options.recipe)?;
    config::identifier(&options.seat)?;
    let sha = repository::commit(root, options.commit.as_deref())?;
    record.commit = Some(sha.clone());
    if repository::uncommitted(root, &sha) {
        execution::warn(&format!(
            "ferry: uncommitted changes are not in {}",
            &sha[..12]
        ));
    }
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
        timeout_seconds: options.timeout_seconds,
        forwarded_env: record.forwarded_env.clone(),
        ship: shipped
            .iter()
            .map(|(object, _, _)| object.clone())
            .collect(),
        preserve: record.preserve.clone(),
        args: options.args.clone(),
        provider_requires: {
            let mut requires = config.provider.requires.clone();
            // An older place would keep a one-run seat: refused instead.
            if options.once {
                requires.insert("seat-lifetime".into());
            }
            requires
        },
        wait_seconds: options.wait_seconds,
        waits_end_ms: None,
        once: options.once,
    };
    record.token = Some(request.token.clone());
    execution::interrupted()?;
    let place = match route {
        config::Route::Local => transport::Place::Local(root.join(".ferry/local/v1")),
        config::Route::Remote(name) => transport::Place::Remote(&hosts.host[name]),
    };
    let objects: Vec<_> = shipped
        .into_iter()
        .map(|(object, repo, _)| (repo, object.commit))
        .collect();
    // Inside a run, on that run's own place: it would wait on its locks.
    transport::not_inside(&place, &record.machine)?;
    // Until the place is asked to run it, nothing ran: any failure here is
    // a refusal (125), and a killed caller leaves no receipt claiming a run.
    transport::ship(root, &place, &request, &objects, record)?;
    execution::interrupted()?;
    // Every caller fact, stored just before the place is asked to run the
    // request: lost until the final receipt replaces it.
    let mut submitted = record.clone();
    submitted.state = Some("submitted".into());
    submitted.outcome = receipt::Outcome::Lost;
    submitted.exit_code = 255;
    submitted.reason = Some("submitted; no verdict yet".into());
    receipt::submit(root, &submitted)?;
    transport::run_on(root, &place, &request, record, options.json)
}

/// The place a stored receipt's run went to.
fn place_of<'a>(
    root: &Path,
    record: &receipt::Receipt,
    hosts: &'a config::Machines,
) -> Result<transport::Place<'a>> {
    if record.machine == "local" {
        return Ok(transport::Place::Local(root.join(".ferry/local/v1")));
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
    // A final run's output is replayed while its place still keeps it.
    if follow && record.state.as_deref() == Some("final") {
        let place = place_of(&root, &record, &hosts)?;
        if let Err(error) = transport::replay(&place, &record) {
            execution::warn(&format!("ferry: {error}"));
        }
    }
    if record.state.as_deref() == Some("submitted") {
        let place = place_of(&root, &record, &hosts)?;
        if follow {
            transport::finish(&root, &place, &mut record, false, None);
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
        Ok(_) => {
            execution::warn(&format!(
                "ferry: stop sent to run {id}; ferry follow {id} shows its end"
            ));
            Ok(0)
        }
        Err(error) => {
            // A place refusal is `{refused, why, next}`: said as a sentence.
            let said = serde_json::from_str::<serde_json::Value>(&error.to_string())
                .ok()
                .and_then(|reply| {
                    let why = reply["why"].as_str()?.to_owned();
                    Some(match reply["next"].as_str() {
                        Some(next) => format!("{why}; next: {next}"),
                        None => why,
                    })
                })
                .unwrap_or_else(|| error.to_string());
            execution::warn(&format!("ferry: {said}"));
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
        // The reason, then the verdict: the last line is always the verdict.
        if let Some(reason) = record.reason.as_deref().filter(|r| !r.is_empty()) {
            execution::warn(&format!("ferry: {reason}"));
        }
        let outcome = serde_json::to_value(record.outcome)?;
        execution::warn(&format!(
            "ferry: {}, exit {}, run {}",
            outcome.as_str().unwrap_or_default(),
            record.exit_code,
            record.id
        ));
    }
    // After the receipt and its output are complete, and never changing the
    // run's code: prune this place's idle seats, if the caller asked for it.
    if record.machine != "local"
        && execution::caught_signal().is_none()
        && let Ok(hosts) = machines()
    {
        seats::prune_by_itself(&record.machine, &hosts);
    }
    Ok(record.exit_code)
}

fn new_record(recipe: &str, seat: &str) -> Result<receipt::Receipt> {
    Ok(receipt::Receipt {
        protocol: receipt::PROTOCOL,
        id: receipt::run_id()?,
        recipe: recipe.to_owned(),
        seat: seat.to_owned(),
        machine: "local".into(),
        outcome: receipt::Outcome::Refused,
        exit_code: 125,
        state: None,
        caller_binary_sha256: receipt::digest(&fs::read(std::env::current_exe()?)?),
        started_ms: receipt::now_ms(),
        caller_source_rev: Some(receipt::source_rev()),
        caller_exe: receipt::exe_path(),
        ..Default::default()
    })
}

fn execute(cli: Cli) -> Result<i32> {
    match cli.command {
        Action::Run(options) => run(&options),
        Action::Seats {
            command: SeatsAction::Prune { on, idle },
        } => seats::prune(
            on.as_deref(),
            &machines()?,
            idle.as_deref().map(config::parse_duration).transpose()?,
        ),
        Action::Machines => {
            for (name, machine) in &machines()?.host {
                println!("{}", config::machine_line(name, machine));
            }
            Ok(0)
        }
        Action::Recipes => {
            let root = repository::root()?;
            let sha = repository::commit(&root, None)?;
            let declared = repository::declaration(
                &root,
                &sha,
                &mut repository::DeclarationEvidence::default(),
            )?;
            for (name, recipe) in &declared.recipes {
                println!("{}", config::recipe_line(name, recipe));
            }
            Ok(0)
        }
        Action::Status { on, json } => status::show(on.as_deref(), &machines()?, json),
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
                PlaceVerb::Retire {
                    key,
                    idle_ms,
                    once_id,
                } => place::retire(&root, &key, &place::Meant { idle_ms, once_id }),
                PlaceVerb::Status => {
                    println!("{}", status::place(&root));
                    Ok(0)
                }
            }
        }
        Action::HostInfo => {
            println!("{}", serde_json::to_string(&transport::host_info()?)?);
            Ok(0)
        }
        Action::ExecuteSession {
            spec,
            result,
            deadline_ms,
            supervisor,
        } => {
            let owned = crate::run::execute_session(&spec, &result, deadline_ms, supervisor);
            if let Err(error) = &owned {
                crate::run::left_error(&result, error.as_ref());
            }
            owned.map(|()| 0)
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
