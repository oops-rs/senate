//! `senated --config <path>`.
//!
//! Serves the log and the bridge API from `senated.toml`; with `[speaker]`
//! also runs the built-in Speaker, with `[work]` its hands. No chat platform
//! is mounted: that is a private layer's, built on the `senated` library.

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use senated::{Assembly, SenatedConfig};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "senated", version, about = "The Senate daemon")]
struct Cli {
    /// Path to senated.toml.
    #[arg(long, env = "SENATED_CONFIG")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let config = SenatedConfig::load(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;
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
