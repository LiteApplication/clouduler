use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Container scheduler, reverse proxy and DNS manager for a small cluster.
#[derive(Debug, Parser)]
#[command(name = "clouduler", version, about)]
pub struct Cli {
    /// Cluster config file.
    ///
    /// No short flag: `-c` is `folder-import`'s copy method instead.
    #[arg(long, global = true, env = "CLOUDULER_CONFIG", default_value = "/etc/clouduler/cluster.yaml")]
    pub config: PathBuf,

    /// Directory holding this node's local state. Overrides the one in the
    /// config, which is where it normally belongs so the web ui can change it.
    #[arg(long, global = true, env = "CLOUDULER_DATA")]
    pub data_dir: Option<PathBuf>,

    /// Log filter, syntax as in RUST_LOG. openraft is quieted by default because
    /// it logs every raft message at info.
    #[arg(long, global = true, env = "CLOUDULER_LOG", default_value = "info,openraft=warn")]
    pub log: String,

    /// Log each step a command takes while it changes cluster state: requests
    /// submitted, nodes contacted, files touched. Raises this crate's own log
    /// level to debug on top of whatever `--log` already says.
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,

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
    /// Manage the replicated cluster config. These talk to a running daemon.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Inspect and manage nodes. These talk to a running daemon.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// Deploy and inspect stacks. These talk to a running daemon.
    Stack {
        #[command(subcommand)]
        command: StackCommand,
    },
    /// Manage cluster-wide secrets. These talk to a running daemon.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Inspect drives.
    Drive {
        #[command(subcommand)]
        command: DriveCommand,
    },
    /// Get a local file or folder into a drive volume, then push it out to
    /// every other node holding a copy right away.
    ///
    /// Only works from a node that implements the target drive: for a
    /// replicated drive that means any node backing it, for a served drive
    /// the active server or a node that mounts it (a standby candidate is
    /// refused, since the next sync from the real server would erase what
    /// was written there).
    ///
    ///   sudo clouduler folder-import ~/Downloads/dataset bulk/imports
    FolderImport(Box<FolderImportArgs>),
    /// Show cluster status.
    Status,
}

/// `clouduler folder-import [-mlcr] <folder> <drive>/<volume>[/path inside volume]`
#[derive(Debug, clap::Args)]
pub struct FolderImportArgs {
    /// Copy the source in. The default.
    #[arg(short = 'c', long, conflicts_with_all = ["move", "link"])]
    pub copy: bool,

    /// Move the source in rather than copying it. Falls back to a copy plus
    /// removing the source when the two are not on the same filesystem.
    #[arg(short = 'm', long, conflicts_with_all = ["copy", "link"])]
    pub r#move: bool,

    /// Symlink the source into the volume instead of duplicating it. Note
    /// that a symlink replicates as the same path string to every other
    /// node, so it only resolves there if that path exists there too.
    #[arg(short = 'l', long, conflicts_with_all = ["copy", "move"])]
    pub link: bool,

    /// Replace the whole volume with the source rather than placing it
    /// inside. Takes no sub-path: the target must be `<drive>/<volume>`.
    #[arg(short = 'r', long)]
    pub replace: bool,

    /// With --replace, stop any instances mounting the volume first and
    /// start them again once the swap and sync are done. Without this,
    /// --replace refuses to touch a volume that is in use.
    #[arg(short = 'f', long, requires = "replace")]
    pub force: bool,

    /// File or folder to import.
    pub source: PathBuf,

    /// Where to import it: `<drive>/<volume>[/path inside volume]`.
    pub target: String,

    /// Name of this node in the config. Defaults to the hostname, same as `daemon`.
    #[arg(long, env = "CLOUDULER_NODE")]
    pub node: Option<String>,

    /// Node to submit the volume bookkeeping through. Defaults to any manager.
    #[arg(long)]
    pub via: Option<String>,
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
pub enum ConfigCommand {
    /// Push the on-disk config file into raft, so it replicates to every
    /// node. Fails on a permission or read error before it ever contacts the
    /// cluster. Use after hand editing the file, to apply the change without
    /// going through the web ui.
    Reload {
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long)]
        via: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
    /// List the nodes declared in the config.
    List,
    /// Register a node in the live roster. Does not touch the config file;
    /// pair with an edit there (or `clouduler config reload`) so a restarted
    /// manager does not forget about it. Normally a node joins itself with
    /// `clouduler init --join` instead; this is for registering one by hand.
    Add {
        /// Name for the node.
        name: String,
        #[arg(long, value_enum)]
        role: RoleArg,
        /// Address other nodes use to reach it, host:port.
        #[arg(long)]
        rpc_addr: std::net::SocketAddr,
        /// A drive this node implements, as name=/local/path. Repeatable.
        #[arg(long, value_name = "NAME=PATH")]
        drive: Vec<String>,
        /// A label on this node, as key=value. Repeatable.
        #[arg(long, value_name = "KEY=VALUE")]
        label: Vec<String>,
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long)]
        via: Option<String>,
    },
    /// Remove a node from the live roster and, if it is declared there, from
    /// the config file too.
    Remove {
        name: String,
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long)]
        via: Option<String>,
    },
    /// Set whether a node may take new work.
    SetAvailability {
        name: String,
        #[arg(value_enum)]
        availability: AvailabilityArg,
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long)]
        via: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AvailabilityArg {
    /// Normal, schedulable.
    Active,
    /// Keeps running what it has, takes nothing new. Set before a migration.
    Drain,
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
pub enum SecretCommand {
    /// Seal a value and store it in the cluster database, so any node running
    /// a container that mounts it can decrypt it. Creates the secret if it
    /// does not exist yet, otherwise bumps its revision.
    ///
    /// Sealing happens on this node before anything is sent, so the plaintext
    /// never crosses the wire or reaches the raft log. That means it needs
    /// this node's cluster key.
    Set {
        /// Name of the secret.
        name: String,
        /// Value for the secret. Omit both this and --file to read the value
        /// from stdin instead, which keeps it out of the shell history.
        #[arg(long, conflicts_with = "file")]
        value: Option<String>,
        /// Read the secret's value from a file instead of stdin.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long)]
        via: Option<String>,
    },
    /// List the secrets stored in the cluster. Values are never shown.
    List {
        #[arg(long)]
        via: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum DriveCommand {
    /// List the drives and the nodes implementing them.
    List,
}
