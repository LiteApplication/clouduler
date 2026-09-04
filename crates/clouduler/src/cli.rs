use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::engine::ArgValueCompleter;

use crate::completion;

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
    /// Disaster recovery for a cluster that has lost quorum for good (a
    /// manager's raft state gone with no backup, so a normal membership
    /// change can never be driven through consensus again since that itself
    /// needs the quorum that is missing): make this node the cluster's sole
    /// voter, bring only `stack` back up trusting this node's own local data
    /// for it, and hold every other stack off until an operator has had a
    /// chance to confirm what else is genuinely still running out there.
    ///
    /// Bypasses raft consensus entirely by rewriting this node's own local
    /// log and state machine, the way etcd's `--force-new-cluster` or
    /// Consul's `peers.json` recovery do. Every other node this node already
    /// knew about stays as a learner - including any lost co-manager, now
    /// demoted rather than dropped - only the voter set shrinks to this node
    /// alone; once a demoted manager's daemon is reachable again, this node
    /// promotes it back to a voter on its own, the same timer that adds any
    /// other new node.
    ///
    /// `stack`'s own instances get placed and started normally, on this data:
    /// every volume it uses is stamped as authoritative on this node, and
    /// every served drive it needs picks this node as the active server
    /// (every other currently known node is marked unreachable, so the
    /// existing first-reachable-candidate logic in `drives.rs` naturally
    /// prefers this one). Every other stack's own `desired` state is left
    /// completely untouched - not stopped, just held off - and it resumes on
    /// its own, with no restore step, the moment another manager is reachable
    /// again and emergency mode ends automatically.
    ///
    /// This node's daemon MUST be stopped first: it writes straight to the
    /// data directory's raft.redb, and the running daemon holding that file
    /// open would make this fail, or race it. Once it is done, start the
    /// daemon normally.
    Emergency {
        /// Stack to bring back up, trusting this node's own local data for it.
        #[arg(add = ArgValueCompleter::new(completion::stack_names))]
        stack: String,
        /// Name of this node in the config. Defaults to the system hostname.
        #[arg(long, env = "CLOUDULER_NODE")]
        node: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Disaster recovery for a cluster whose replicated content - cluster
    /// config, stack specs, cluster configs and secrets - is gone from every
    /// node, using the git repo `backup:` mirrors it to (see `docs/git-backup.md`).
    ///
    /// This restores content only, never raft membership/quorum: on a
    /// cluster that has also lost quorum for good, run `emergency` or the
    /// equivalent `node force-quorum` step first to get a reachable cluster,
    /// then `restore` to refill it. Running `restore` alone leaves this
    /// node's existing membership and log untouched.
    ///
    /// `source` is normally a local path to an already-cloned copy of the
    /// backup repo (`git clone` it yourself first, using whatever SSH
    /// credentials you have - clouduler's own deploy key is stored inside
    /// the very state this command is trying to recover, so it cannot be
    /// relied on to exist yet). A `git@`/`ssh://` URL is also accepted and
    /// cloned directly, using the ambient SSH agent.
    ///
    /// Secrets in the repo are encrypted for the admin's age identity, never
    /// the cluster's own key, so restoring them needs that identity: pass it
    /// with `--age-identity-file` or pipe it on stdin with
    /// `--age-identity-stdin`. It is read once, used in memory to decrypt
    /// and immediately re-seal each secret under this node's own cluster
    /// key, and never written anywhere.
    ///
    /// This node's daemon MUST be stopped first, the same as `emergency`.
    Restore {
        /// Path to an already-cloned working copy of the backup repo, or a
        /// `git@`/`ssh://` url to clone directly.
        source: String,
        /// File holding the admin's age identity (`AGE-SECRET-KEY-1...`),
        /// the same format `age-keygen` writes.
        #[arg(long, conflicts_with = "age_identity_stdin")]
        age_identity_file: Option<PathBuf>,
        /// Read the age identity from stdin instead of a file.
        #[arg(long)]
        age_identity_stdin: bool,
        /// Branch to read when `source` is a url to clone. Ignored for an
        /// already-cloned local path, which is read as checked out.
        #[arg(long, default_value = "main")]
        branch: String,
        /// Name of this node in the config. Defaults to the system hostname.
        #[arg(long, env = "CLOUDULER_NODE")]
        node: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Actively relocate instances to smooth the number of instances per
    /// node, unlike the regular scheduler pass, which never moves an
    /// already validly-placed instance.
    ///
    /// Never touches an instance pinned to a node, published on a proxy
    /// hostname, already mid-migration, or that has nowhere else it could
    /// legally run - see `cl_sched::rebalance::plan`. Each move it decides on
    /// goes through the same `BeginMigration` path as a manual move from the
    /// ui, so ordinary placement and drive checks still apply.
    Rebalance {
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Show cluster status.
    Status,
    /// Stream a container's output, or every container in a stack.
    ///
    /// The target is either an instance id (`stack.service.index`, as shown
    /// by `clouduler stack show` or the web ui) or a stack name, which
    /// streams every instance currently running for that stack, each line
    /// prefixed with which instance it came from.
    Logs {
        /// Instance id (`stack.service.index`) or stack name.
        #[arg(add = ArgValueCompleter::new(completion::stack_names))]
        target: String,
        /// Keep streaming new lines as they are written.
        #[arg(short = 'f', long)]
        follow: bool,
        /// Number of lines to show from the end of each container's log.
        #[arg(short = 'n', long, default_value_t = 200)]
        tail: u32,
        /// Node to ask for the cluster's current state. Defaults to any
        /// manager in the config; the container's own node is then found
        /// from that state and asked directly for its logs.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Run a command inside a running container, wherever it runs.
    ///
    /// Works from any manager: the container's own node is found from the
    /// cluster state automatically and talked to directly, the same as
    /// `clouduler logs`. Allocates a pseudo-tty and bridges it to this
    /// terminal when both stdin and stdout are one, so an interactive shell
    /// behaves like `docker exec -it`.
    ///
    /// A command with its own flags needs `--` first, same as any clap
    /// command with a trailing argument list:
    ///
    ///   clouduler exec site.web.0 -- sh -c "ls -la"
    Exec {
        /// Instance id (`stack.service.index`) or stack name. A stack name
        /// only resolves when it currently has exactly one running instance.
        #[arg(add = ArgValueCompleter::new(completion::stack_names))]
        target: String,
        /// User to run as inside the container: `user`, `user:group`, `uid`
        /// or `uid:gid`. Defaults to the image's own user.
        #[arg(short = 'u', long)]
        user: Option<String>,
        /// Node to ask for the cluster's current state. Defaults to any
        /// manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
        /// Command to run and its arguments. Defaults to `sh`.
        #[arg(trailing_var_arg = true)]
        cmd: Vec<String>,
    },
    /// Capture traffic on the overlay network a stack's containers run on.
    ///
    /// Runs `tcpdump` on the node that owns the target instance, on the
    /// bridge for one of the stack's declared networks - the same interface
    /// you would otherwise have to SSH in, find via `docker network
    /// inspect` and `ip link`, and run `tcpdump` on by hand, as root, to get
    /// to. Defaults to the network the reverse proxy actually forwards to.
    ///
    /// With no `-w`, prints `tcpdump`'s normal decoded text live, same as
    /// running it locally. With `-w <file>`, saves raw pcap bytes instead,
    /// to open later in Wireshark or `tcpdump -r`.
    ///
    /// A BPF filter (`tcpdump`'s own expression syntax) goes after `--`:
    ///
    ///   clouduler capture immich.server.0 -- port 2283
    Capture {
        /// Instance id (`stack.service.index`) or stack name. A stack name
        /// only resolves when it currently has exactly one running instance.
        #[arg(add = ArgValueCompleter::new(completion::stack_names))]
        target: String,
        /// Which of the stack's declared networks to capture on. Defaults to
        /// the one the reverse proxy forwards to.
        #[arg(long)]
        network: Option<String>,
        /// Save raw pcap bytes to this file instead of printing decoded text.
        #[arg(short = 'w', long)]
        write: Option<std::path::PathBuf>,
        /// Node to ask for the cluster's current state. Defaults to any
        /// manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
        /// A BPF filter expression, e.g. `port 2283` or `host 10.244.7.11`.
        #[arg(trailing_var_arg = true)]
        filter: Vec<String>,
    },
    /// Print or install shell completions.
    ///
    /// Completions are dynamic: past the fixed set of subcommands and flags,
    /// arguments that take a stack or node name are completed by asking a
    /// live manager (or reading the local config) at completion time, not
    /// from a static list baked in when the script was generated.
    Completions {
        #[command(subcommand)]
        command: CompletionsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum CompletionsCommand {
    /// Print a completion script for the given shell to stdout.
    ///
    ///   clouduler completions print bash > /etc/bash_completion.d/clouduler
    Print {
        #[arg(value_enum)]
        shell: ShellArg,
    },
    /// Install completions into this user's shell profile, so a new shell
    /// picks them up automatically. Safe to run more than once: it does not
    /// add a second line if one is already there.
    ///
    /// Detects the shell from $SHELL unless one is given.
    Install {
        #[arg(value_enum)]
        shell: Option<ShellArg>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ShellArg {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
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
    #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
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
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
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
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Remove a node from the live roster and, if it is declared there, from
    /// the config file too.
    Remove {
        #[arg(add = ArgValueCompleter::new(completion::node_names))]
        name: String,
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Set whether a node may take new work.
    SetAvailability {
        #[arg(add = ArgValueCompleter::new(completion::node_names))]
        name: String,
        #[arg(value_enum)]
        availability: AvailabilityArg,
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
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
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// List the stacks stored in the cluster.
    List {
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Print a stored stack's spec.
    Show {
        #[arg(add = ArgValueCompleter::new(completion::stack_names))]
        name: String,
        /// Also list the bundled files. Secret contents are never printed.
        #[arg(long)]
        files: bool,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Remove a stack and everything it runs.
    Remove {
        #[arg(add = ArgValueCompleter::new(completion::stack_names))]
        name: String,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Stop every instance of a stack, wherever it runs. The assignment is
    /// kept, so `stack start` brings it back without redeploying.
    Stop {
        #[arg(add = ArgValueCompleter::new(completion::stack_names))]
        name: String,
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Start every instance of a stack that isn't already running.
    Start {
        #[arg(add = ArgValueCompleter::new(completion::stack_names))]
        name: String,
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Restart every currently running instance of a stack in place, the
    /// same as `docker restart` on each of its containers. Unlike
    /// `stack stop`+`stack start`, this never changes desired state, so a
    /// stack that was stopped stays stopped.
    Restart {
        #[arg(add = ArgValueCompleter::new(completion::stack_names))]
        name: String,
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Stream every instance of a stack's output together, merged and
    /// ordered by timestamp rather than by whichever node answers first.
    Logs {
        #[arg(add = ArgValueCompleter::new(completion::stack_names))]
        name: String,
        /// Keep streaming new lines as they are written.
        #[arg(short = 'f', long)]
        follow: bool,
        /// Number of lines to show from the end of each container's log.
        #[arg(short = 'n', long, default_value_t = 200)]
        tail: u32,
        /// Node to ask for the cluster's current state. Defaults to any
        /// manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// At a glance: every stack instance whose actual state doesn't match
    /// what's desired - crashed, restart-looping, failing its healthcheck,
    /// migrating (with the phase, so one that is stuck mid-move is visible
    /// rather than looking like a crash), on an unreachable node, assigned
    /// but with no container at all, running an image other than the one the
    /// cluster pinned, wanted but unable to land anywhere, or stopped (shown
    /// with --placement so a deliberate `stack stop` isn't mistaken for a
    /// crash).
    ///
    /// Ends with any cluster-wide faults that belong to no single instance:
    /// unreachable nodes, and volumes whose primary is gone or whose replicas
    /// have fallen behind.
    ///
    /// Bare output is deliberately terse (instance, state, health); the
    /// flags below add detail per instance.
    Unhealthy {
        /// Show which node each instance runs on.
        #[arg(long)]
        node: bool,
        /// Show how long each instance has been up.
        #[arg(long)]
        uptime: bool,
        /// Show why a pending instance could not be placed on any node, or
        /// why a migration failed.
        #[arg(long)]
        placement: bool,
        /// Print this many lines from the end of each unhealthy container's
        /// log, indented underneath it.
        #[arg(long, value_name = "N")]
        logs: Option<u32>,
        /// Show everything: --node, --uptime, --placement and the last 10
        /// log lines, as if all four were given.
        #[arg(long)]
        all: bool,
        /// Node to ask for the cluster's current state. Defaults to any
        /// manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Get or set a stack's raw source (the same yaml a `stack deploy` file
    /// takes), so it can be piped out, edited and piped back in without
    /// keeping a local copy of the file.
    Source {
        #[arg(add = ArgValueCompleter::new(completion::stack_names))]
        name: String,
        #[command(subcommand)]
        action: Option<SourceCommand>,
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SourceCommand {
    /// Print the stack's current source to stdout. The default action.
    Get,
    /// Replace the stack's source, the same as redeploying it with a new
    /// file. Fails if the stack does not exist yet - use `stack deploy` to
    /// create one.
    Set {
        /// Read the new source from this file. Defaults to stdin.
        file: Option<PathBuf>,
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
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// List the secrets stored in the cluster. Values are never shown.
    List {
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum DriveCommand {
    /// List the drives and the nodes implementing them.
    List,
}
