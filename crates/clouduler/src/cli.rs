use std::net::IpAddr;
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

    /// Directory holding this node's local state. Overrides the one in the
    /// config, which is where it normally belongs so the web ui can change it.
    #[arg(long, global = true, env = "CLOUDULER_DATA")]
    pub data_dir: Option<PathBuf>,

    /// Log filter, syntax as in RUST_LOG. openraft is quieted by default because
    /// it logs every raft message at info.
    #[arg(long, global = true, env = "CLOUDULER_LOG", default_value = "info,openraft=warn")]
    pub log: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Set this machine up as a clouduler node: write a config, join or form a
    /// cluster, declare drives and install the systemd service.
    ///
    /// Run with no arguments it asks about each step and any step can be
    /// skipped. Given enough arguments it asks nothing, which is the form to
    /// use from a provisioning script:
    ///
    ///   clouduler init --yes --join 10.10.0.1 --token-file /run/cl.token
    Init(Box<InitArgs>),
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

/// Everything `clouduler init` can be told up front. Anything left out is
/// asked for, unless `--yes` is set, in which case the default is taken and a
/// question with no sensible default is an error.
#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Do not ask anything. Fails rather than guessing at a required answer.
    #[arg(short, long)]
    pub yes: bool,

    /// Overwrite an existing config file. Without it, init refuses to clobber
    /// a node that is already set up.
    #[arg(long)]
    pub force: bool,

    /// Name for this node. Defaults to the hostname.
    #[arg(long)]
    pub node: Option<String>,

    /// manager (votes, can lead, serves the ui) or worker (runs containers only).
    #[arg(long, value_enum)]
    pub role: Option<RoleArg>,

    /// This node's address on the internal network. Detected from the routing
    /// table when left out.
    #[arg(long)]
    pub ip: Option<IpAddr>,

    /// Port for the node to node channel.
    #[arg(long)]
    pub rpc_port: Option<u16>,

    /// A drive this node implements, as name=/local/path. Repeatable.
    #[arg(long, value_name = "NAME=PATH")]
    pub drive: Vec<String>,

    /// Mode for drives this node declares for the first time.
    #[arg(long, value_enum, default_value = "replicated")]
    pub drive_mode: DriveModeArg,

    /// Address of a node already in the cluster, to join it. Without this,
    /// init forms a new cluster with this node as its first manager.
    #[arg(long, value_name = "HOST[:PORT]")]
    pub join: Option<String>,

    /// Api key for the node being joined through. The cluster prints one on
    /// first launch.
    #[arg(long, conflicts_with = "token_file")]
    pub token: Option<String>,

    /// File holding that api key, so it stays out of the shell history.
    #[arg(long)]
    pub token_file: Option<PathBuf>,

    /// SHA-256 fingerprint of the certificate the joined node should present.
    /// A new cluster serves a self signed one, so without this the certificate
    /// is accepted unchecked and its fingerprint printed for you to compare.
    #[arg(long)]
    pub tls_fingerprint: Option<String>,

    /// Name of the new cluster. New cluster only.
    #[arg(long)]
    pub cluster_name: Option<String>,

    /// Subnet of the internal network every node talks over. New cluster only.
    #[arg(long)]
    pub subnet: Option<ipnet::IpNet>,

    /// Domain whose subdomains resolve to internal addresses. New cluster only.
    #[arg(long)]
    pub internal_domain: Option<String>,

    /// Hostname the web ui answers on. New cluster only.
    #[arg(long)]
    pub web_address: Option<String>,

    /// Port the web ui listens on. New cluster only.
    #[arg(long)]
    pub web_port: Option<u16>,

    /// Where this node keeps its database and working files.
    #[arg(long)]
    pub node_data_dir: Option<PathBuf>,

    /// Install a systemd unit for the daemon.
    #[arg(long, overrides_with = "no_systemd")]
    pub systemd: bool,
    #[arg(long, overrides_with = "systemd")]
    pub no_systemd: bool,

    /// Enable and start the service once it is installed.
    #[arg(long, overrides_with = "no_start")]
    pub start: bool,
    #[arg(long, overrides_with = "start")]
    pub no_start: bool,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum RoleArg {
    Manager,
    Worker,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DriveModeArg {
    /// Every implementing node keeps its own copy, synced periodically.
    Replicated,
    /// One node holds the data and serves it to the others over the internal
    /// network.
    Served,
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
