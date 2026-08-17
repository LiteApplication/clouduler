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

    /// Directory holding this node's local state.
    #[arg(
        long,
        global = true,
        env = "CLOUDULER_DATA",
        default_value = "/var/lib/clouduler"
    )]
    pub data_dir: PathBuf,

    /// Log filter, syntax as in RUST_LOG. openraft is quieted by default because
    /// it logs every raft message at info.
    #[arg(long, global = true, env = "CLOUDULER_LOG", default_value = "info,openraft=warn")]
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
    /// Deploy and inspect stacks. These talk to a running daemon.
    Stack {
        #[command(subcommand)]
        command: StackCommand,
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
pub enum StackCommand {
    /// Store a stack in the cluster database and deploy it. The referenced
    /// config and secret files are bundled in, so every node can run it.
    Deploy {
        /// Path to the stack file. The stack is named after the file unless
        /// --name is given.
        file: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long)]
        via: Option<String>,
    },
    /// List the stacks stored in the cluster.
    List {
        #[arg(long)]
        via: Option<String>,
    },
    /// Print a stored stack's spec.
    Show {
        name: String,
        /// Also list the bundled files. Secret contents are never printed.
        #[arg(long)]
        files: bool,
        #[arg(long)]
        via: Option<String>,
    },
    /// Remove a stack and everything it runs.
    Remove {
        name: String,
        #[arg(long)]
        via: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum DriveCommand {
    /// List the drives and the nodes implementing them.
    List,
}
