//! `senated`: the Senate daemon, the same at every level.
//!
//! ```text
//! senated --config senated.toml     # a Senate server
//! senated --machine                 # a developer machine: $SENATE_HOME/senate.toml
//! senated flatten-log --database-url postgres://…   # convert an old Postgres log, once
//! senated secretary show [--config senated.toml | --home DIR] [--committee ID]
//! senated check-model [--config senated.toml | --home DIR] [--committee ID] [--json]
//! ```
//!
//! With `--config` it serves the log and the bridge API from
//! `senated.toml`; with a model (`[model]`) and a reference to the built-in
//! session (the roster's `speaker`, or a secretary list) it also runs the
//! built-in session, reading `secretary.toml` beside the configuration
//! file; with `[work]` its hands. No chat platform is mounted: that is a
//! private layer's, built on the `senated` library.
//!
//! `secretary show` prints the built-in session's configuration in force
//! (for one committee with `--committee`), which file each value came from,
//! and the current problems, in the configured language. A model's key is
//! never printed: only the name of its variable and whether it is set.
//!
//! `check-model` probes the model the built-in session runs on (and its
//! `[model] fallback`): a tool round trip, the terminal tool, and the
//! intent, triage and pick outputs, one short run each. It prints pass or
//! fail with the exact reason per check and exits non-zero when one fails.
//! The daemon runs the same check once at start and logs it.
//!
//! With `--machine` it reads the developer machine's `senate.toml` in the
//! Senate home (`--home`, else `$SENATE_HOME`, else `~/.senate`) and runs
//! the Senate hosted there and the machine's side of the Senate it serves
//! ([`senated::machine`]).
//!
//! `flatten-log` converts a log written when senators were paths
//! (`senator::alice/claude`) to flat names, once, keeping a copy of each
//! table ([`senate_log::PgStore::flatten_names`]). An embedded (SQLite)
//! log is converted on its own when it is opened; a Postgres one only by
//! this command.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use senate_bridge::Paths;
use senate_bridge::daemon::Timing;
use senate_core::CommitteeId;
use senated::{Assembly, Machine, SenatedConfig};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "senated",
    version,
    about = "The Senate daemon: a Senate server (--config), or a developer machine (--machine)"
)]
struct Cli {
    /// Path to senated.toml: run a Senate server.
    #[arg(long, env = "SENATED_CONFIG", conflicts_with_all = ["machine", "home"])]
    config: Option<PathBuf>,
    /// Run as a developer machine, from senate.toml in the Senate home.
    #[arg(long)]
    machine: bool,
    /// The Senate home for --machine (default: $SENATE_HOME, else ~/.senate).
    #[arg(long, value_name = "DIR", requires = "machine")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// The built-in session's configuration.
    Secretary {
        #[command(subcommand)]
        command: SecretaryCommand,
    },
    /// Check a Senate server and say how to fix what is wrong: the
    /// configuration, the log (read only, never created or migrated),
    /// whether the server answers, its chat gateway, and the Speaker's model. Exits non-zero
    /// when a check fails.
    Doctor {
        /// The server's senated.toml.
        #[arg(long, env = "SENATED_CONFIG")]
        config: PathBuf,
        /// Skip the Speaker model check, which makes live provider calls.
        #[arg(long)]
        no_model: bool,
        /// Print JSON: {checks: [{id, status, found, fix?}], summary}.
        #[arg(long)]
        json: bool,
    },
    /// Check that the built-in session's model does what the Speaker asks
    /// of it: a tool round trip, the terminal tool, and the intent, triage
    /// and pick outputs, on the primary model and the fallback. Exits
    /// non-zero when a check fails.
    CheckModel {
        /// A Senate server's senated.toml (secretary.toml is beside it).
        #[arg(long, env = "SENATED_CONFIG", conflicts_with = "home")]
        config: Option<PathBuf>,
        /// A developer machine's Senate home (default: $SENATE_HOME, else
        /// ~/.senate).
        #[arg(long, value_name = "DIR")]
        home: Option<PathBuf>,
        /// The committee whose model to check (default: the Senate as a
        /// whole).
        #[arg(long, value_name = "ID")]
        committee: Option<CommitteeId>,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Convert a Postgres log written when senators were paths to flat
    /// names, once, keeping a copy of each table as <table>_pre_flat_names.
    FlattenLog {
        /// The log's database URL (postgres://…).
        #[arg(long, env = "SENATE_DATABASE_URL")]
        database_url: String,
    },
}

#[derive(Debug, Subcommand)]
enum SecretaryCommand {
    /// Print the configuration in force, where each value came from, and
    /// the current problems.
    Show {
        /// A Senate server's senated.toml (secretary.toml is beside it).
        #[arg(long, env = "SENATED_CONFIG", conflicts_with = "home")]
        config: Option<PathBuf>,
        /// A developer machine's Senate home (default: $SENATE_HOME, else
        /// ~/.senate).
        #[arg(long, value_name = "DIR")]
        home: Option<PathBuf>,
        /// The committee to show (default: the Senate as a whole).
        #[arg(long, value_name = "ID")]
        committee: Option<CommitteeId>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::FlattenLog { database_url }) => return flatten_log(&database_url).await,
        Some(Command::Doctor {
            config,
            json,
            no_model,
        }) => return doctor(&config, json, no_model).await,
        Some(Command::Secretary {
            command:
                SecretaryCommand::Show {
                    config,
                    home,
                    committee,
                },
        }) => return secretary_show(config, home, committee.as_ref()),
        Some(Command::CheckModel {
            config,
            home,
            committee,
            json,
        }) => return check_model(config, home, committee.as_ref(), json).await,
        None => {}
    }
    if cli.machine {
        let paths = match cli.home {
            Some(home) => Paths::new(home),
            None => Paths::from_env()?,
        };
        let machine = Machine::start_desktop(
            paths.clone(),
            Timing::default(),
            senate_bridge::notify::Desktop::system(),
        )
        .await
        .with_context(|| format!("starting the machine from {}", paths.config().display()))?;
        for senate in machine.senates() {
            tracing::info!(senate = %senate.key, url = %senate.url, "serving");
        }
        return machine.run_until_signal().await;
    }
    let Some(path) = cli.config else {
        anyhow::bail!(
            "say what to run: --config <senated.toml> for a Senate server, or --machine for a \
             developer machine"
        );
    };
    let config =
        SenatedConfig::load(&path).with_context(|| format!("loading {}", path.display()))?;
    std::fs::create_dir_all(&config.server.data_dir)
        .with_context(|| format!("creating data_dir {}", config.server.data_dir.display()))?;
    for section in config.other_sections() {
        tracing::warn!(%section, "senated does not read this section; ignoring it");
    }
    let daemon = Assembly::new(config)
        .config_file(&path)
        .start()
        .await
        .context("starting senated")?;
    tracing::info!(url = %daemon.base_url(), "serving");
    daemon.run_until_signal().await
}

/// `senated doctor`: every server check, printed; a failed one fails the
/// command.
async fn doctor(config: &std::path::Path, json: bool, no_model: bool) -> anyhow::Result<()> {
    let checks = senated::doctor::checks_with_model(config, !no_model).await;
    let locale = SenatedConfig::load(config)
        .map(|c| c.server.locale)
        .unwrap_or_default();
    let report = senate_bridge::doctor::Report::new(checks);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.render(locale));
    }
    if report.failed() {
        std::process::exit(1);
    }
    Ok(())
}

/// `senated flatten-log`: convert a Postgres log, once, and say what was
/// done.
async fn flatten_log(database_url: &str) -> anyhow::Result<()> {
    if !database_url.starts_with("postgres") {
        anyhow::bail!(
            "flatten-log converts a Postgres log; an embedded (sqlite:) log is converted when \
             senated opens it"
        );
    }
    let store = senate_log::PgStore::connect(database_url)
        .await
        .with_context(|| format!("connecting to {}", senate_server::redact_url(database_url)))?;
    match store.flatten_names().await? {
        senate_log::FlattenOutcome::AlreadyFlat => {
            println!("the log already uses flat senator names; nothing to do");
        }
        senate_log::FlattenOutcome::Converted { events, agents } => {
            println!(
                "converted {events} event(s) to flat senator names; a copy of each table is kept \
                 as <table>_pre_flat_names"
            );
            for (agent, person) in agents {
                println!("  {person}/{agent} -> {agent} (run by {person})");
            }
        }
    }
    Ok(())
}

/// `senated secretary show`: the configuration in force, in the configured
/// language.
fn secretary_show(
    config: Option<PathBuf>,
    home: Option<PathBuf>,
    committee: Option<&CommitteeId>,
) -> anyhow::Result<()> {
    let secretary = load_secretary(config, home)?;
    print!("{}", senated::secretary_show(&secretary, committee));
    Ok(())
}

/// `senated check-model`: probe the model in force, print the report, and
/// fail when a check does.
async fn check_model(
    config: Option<PathBuf>,
    home: Option<PathBuf>,
    committee: Option<&CommitteeId>,
    json: bool,
) -> anyhow::Result<()> {
    let secretary = std::sync::Arc::new(load_secretary(config, home)?);
    let report = senated::check_model(secretary, committee).await;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.render());
    }
    if !report.passed() {
        std::process::exit(1);
    }
    Ok(())
}

/// The built-in session's configuration, from a server's file or a
/// machine's Senate home.
fn load_secretary(
    config: Option<PathBuf>,
    home: Option<PathBuf>,
) -> anyhow::Result<senate_speaker::SecretaryConfigs> {
    Ok(match (config, home) {
        (Some(path), _) => {
            let config = SenatedConfig::load(&path)
                .with_context(|| format!("loading {}", path.display()))?;
            senated::config::load_secretary(&config, &path)
        }
        (None, home) => {
            let paths = match home {
                Some(home) => Paths::new(home),
                None => Paths::from_env()?,
            };
            let data_dir = paths.hosted(senate_bridge::paths::HOSTED_KEY).join("data");
            senated::config::load_secretary_home(paths.home(), &data_dir)?
        }
    })
}
