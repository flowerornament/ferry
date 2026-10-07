//! Ferry runs committed repository recipes locally or over SSH.
mod admission;
mod artifacts;
mod config;
mod custody;
mod execution;
mod receipt;
mod repository;
mod seat;
mod transport;

use clap::{Args, Parser, Subcommand};
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
    Seat {
        #[command(subcommand)]
        command: SeatAction,
    },
    Receipt {
        id: String,
    },
    HostRun,
    #[command(hide = true)]
    HostInfo,
    #[command(hide = true)]
    ExecuteSession {
        spec: PathBuf,
        result: PathBuf,
    },
}
#[derive(Subcommand)]
enum SeatAction {
    Recover {
        seat: String,
        #[arg(long)]
        local: bool,
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
    interrupted()?;
    let config = declared.config;
    for path in &config.preserve {
        config::preserve_path(path)?;
    }
    record.preserve.clone_from(&config.preserve);
    record.companion_sources.clone_from(&config.companion_refs);
    for (name, revision) in &config.companion_refs {
        repository::ref_name(name)?;
        record
            .companion_refs
            .insert(name.clone(), repository::companion_commit(root, revision)?);
    }
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
    let caller = String::from_utf8(repository::output(&mut std::process::Command::new(
        "hostname",
    ))?)?;
    let identity = receipt::digest(
        format!(
            "{}\n{}\n{}",
            caller.trim(),
            root.canonicalize()?.display(),
            options.seat
        )
        .as_bytes(),
    );
    let mut request = execution::Request {
        protocol: receipt::PROTOCOL,
        id: record.id.clone(),
        token: receipt::run_id()?,
        commit: sha,
        seat: format!("{}-{}", &identity[..24], options.seat),
        recipe: declaration.clone(),
        hooks: config.hooks,
        caller_binary_sha256: record.caller_binary_sha256.clone(),
        timeout_seconds: options.timeout_seconds,
        mirrors: vec![],
        forwarded_env: record.forwarded_env.clone(),
        companion_refs: record.companion_refs.clone(),
        preserve: record.preserve.clone(),
        args: options.args.clone(),
    };
    if matches!(route, config::Route::Remote(_)) {
        record.outcome = receipt::Outcome::Lost;
        record.exit_code = 255;
    }
    request.mirrors = transport::mirror(
        root,
        &config.mirror,
        match route {
            config::Route::Local => None,
            config::Route::Remote(name) => Some(&hosts.host[name]),
        },
    )?;
    record.mirrors.clone_from(&request.mirrors);
    interrupted()?;
    match route {
        config::Route::Local => transport::local(root, &request, record, options.json),
        config::Route::Remote(name) => {
            transport::remote(root, &hosts.host[name], &request, record, options.json)
        }
    }
}
fn signal_name(signal: i32) -> String {
    execution::signal_name(signal)
}
fn interrupted() -> Result<()> {
    execution::interrupted()
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
    // An interrupted caller exits 128 plus its signal, unless the recipe had
    // already reached a verdict.
    if let Some(signal) = execution::caught_signal()
        && !matches!(
            record.outcome,
            receipt::Outcome::RanPass | receipt::Outcome::RanFail | receipt::Outcome::Timeout
        )
    {
        record.outcome = receipt::Outcome::Lost;
        record.exit_code = 128 + signal;
        if record.reason.is_none() {
            record.reason = Some(format!("caller received {}", signal_name(signal)));
        }
    }
    record.elapsed_ms = receipt::now_ms() - start;
    receipt::save(&root, &record)?;
    if options.json {
        println!(
            "{}",
            serde_json::json!({"protocol": receipt::PROTOCOL, "event": "receipt", "receipt": record})
        );
    } else {
        eprintln!(
            "ferry: {:?}: {} (receipt {})",
            record.outcome,
            record.reason.as_deref().unwrap_or(""),
            record.id
        );
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
        mirrors: vec![],
        adapter_sha256: None,
        contract_sha256: None,
        forwarded_env: BTreeMap::default(),
        companion_refs: BTreeMap::default(),
        companion_sources: BTreeMap::default(),
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
    })
}

fn recover(seat: &str, local: bool) -> Result<i32> {
    let root = if local {
        repository::root()?.join(".ferry/local")
    } else {
        PathBuf::from(std::env::var_os("HOME").ok_or("HOME missing")?).join(".local/state/ferry")
    };
    let mut record = new_record("seat-recover", seat)?;
    match crate::seat::recover(&root, seat) {
        Ok(()) => {
            record.outcome = receipt::Outcome::RanPass;
            record.exit_code = 0;
        }
        Err(error) => record.reason = Some(error.to_string()),
    }
    record.elapsed_ms = receipt::now_ms() - record.started_ms;
    receipt::save(&root, &record)?;
    println!(
        "{}",
        serde_json::json!({"protocol":receipt::PROTOCOL,"event":"receipt","receipt":record})
    );
    Ok(record.exit_code)
}
fn execute(cli: Cli) -> Result<i32> {
    match cli.command {
        Action::Run(options) => run(&options),
        Action::Seat {
            command: SeatAction::Recover { seat, local },
        } => recover(&seat, local),
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
        Action::HostRun => transport::host_run(),
        Action::HostInfo => {
            println!("{}", serde_json::to_string(&transport::host_info()?)?);
            Ok(0)
        }
        Action::ExecuteSession { spec, result } => {
            execution::execute_session(&spec, &result)?;
            Ok(0)
        }
    }
}
fn main() {
    let code = match execute(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ferry: {error}");
            125
        }
    };
    std::process::exit(code);
}
