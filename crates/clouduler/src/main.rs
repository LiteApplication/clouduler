mod cli;
mod daemon;
mod report;

use anyhow::{Context, Result};
use clap::Parser;
use cl_config::ClusterConfig;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, DriveCommand, NodeCommand};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log)?;

    let cfg = ClusterConfig::load(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;

    match cli.command {
        Command::Daemon { node } => {
            let node = match node {
                Some(n) => n,
                None => hostname()?,
            };
            daemon::run(cfg, node, cli.data_dir).await
        }
        Command::Check => {
            report::summary(&cfg);
            Ok(())
        }
        Command::Node {
            command: NodeCommand::List,
        } => {
            report::nodes(&cfg);
            Ok(())
        }
        Command::Drive {
            command: DriveCommand::List,
        } => {
            report::drives(&cfg);
            Ok(())
        }
        Command::Status => {
            // Filled in once the raft layer can answer live queries.
            report::summary(&cfg);
            Ok(())
        }
    }
}

fn init_tracing(filter: &str) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(filter).context("invalid log filter")?)
        .with_target(false)
        .init();
    Ok(())
}

fn hostname() -> Result<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .context("reading hostname, pass --node instead")?;
    Ok(raw.trim().to_string())
}
