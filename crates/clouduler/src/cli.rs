use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Container scheduler, reverse proxy and DNS manager for a small cluster.
#[derive(Debug, Parser)]
#[command(name = "clouduler", version, about)]
pub struct Cli {
    /// Cluster config file.
    #[arg(
        short,
        long,
        global = true,
        env = "CLOUDULER_CONFIG",
        default_value = "/etc/clouduler/cluster.yaml"
    )]
    pub config: PathBuf,

    /// Log filter, syntax as in RUST_LOG.
    #[arg(long, global = true, env = "CLOUDULER_LOG", default_value = "info")]
    pub log: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the node daemon.
    Daemon {
        /// Name of this node in the config. Defaults to the system hostname.
        #[arg(long, env = "CLOUDULER_NODE")]
        node: Option<String>,
    },
    /// Check the config file and print a summary of the resulting cluster.
    Check,
    /// Inspect nodes.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// Inspect drives.
    Drive {
        #[command(subcommand)]
        command: DriveCommand,
    },
    /// Show cluster status.
    Status,
}

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
    /// List the nodes declared in the config.
    List,
}

#[derive(Debug, Subcommand)]
pub enum DriveCommand {
    /// List the drives and the nodes implementing them.
    List,
}
