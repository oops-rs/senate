//! `senated`: the Senate daemon, the same at every level.
//!
//! ```text
//! senated --config senated.toml     # a Senate server
//! senated --machine                 # a developer machine: $SENATE_HOME/senate.toml
//! senated flatten-log --database-url postgres://…   # convert an old Postgres log, once
//! ```
//!
//! With `--config` it serves the log and the bridge API from
//! `senated.toml`; with `[speaker]` it also runs the built-in Speaker, with
//! `[work]` its hands. No chat platform is mounted: that is a private
//! layer's, built on the `senated` library.
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
    /// Convert a Postgres log written when senators were paths to flat
    /// names, once, keeping a copy of each table as <table>_pre_flat_names.
    FlattenLog {
        /// The log's database URL (postgres://…).
        #[arg(long, env = "SENATE_DATABASE_URL")]
        database_url: String,
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
    if let Some(Command::FlattenLog { database_url }) = cli.command {
        return flatten_log(&database_url).await;
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
    if config.speaker.is_none() {
        tracing::info!("no [speaker]: serving the log and the bridge API only");
    }
    let daemon = Assembly::new(config)
        .start()
        .await
        .context("starting senated")?;
    tracing::info!(url = %daemon.base_url(), "serving");
    daemon.run_until_signal().await
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
