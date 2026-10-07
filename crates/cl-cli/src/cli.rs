use std::path::PathBuf;

use cl_raft::ids::VolumeId;
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
    #[arg(
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
    /// it logs every raft message at info, and hickory_server because it logs
    /// every DNS query and response at info regardless of outcome.
    #[arg(
        long,
        global = true,
        env = "CLOUDULER_LOG",
        default_value = "info,openraft=warn,hickory_server=warn"
    )]
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
    /// Manage container registry logins, similar to `docker login`.
    ///
    /// A login is checked against the registry before it is stored, sealed
    /// with the cluster key, and replicated through raft, the same way the
    /// web ui's registry accounts panel stores one. These talk to a running
    /// daemon.
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
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
    /// Live overview of the cluster, meant to be the only command needed to
    /// find out what is wrong: raft leader and quorum, every node (reachable,
    /// availability, instance count), then everything that is off - unhealthy
    /// instances with node, uptime, reason and the last log lines, stuck or
    /// failed updates and migrations, failed backups and restores, failing
    /// certificates or DNS records, unreachable nodes and lagging volumes.
    /// Prints "all clear" when there is nothing to report.
    Status {
        /// Node to ask for the cluster's current state. Defaults to any
        /// manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Stream a container's output, or every container in a stack.
    ///
    /// The target is either an instance id (`stack.service.index`, as shown
    /// by `clouduler stack show` or the web ui) or a stack name, which
    /// streams every instance currently running for that stack, each line
    /// prefixed with which instance it came from.
    Logs {
        /// Instance id (`stack.service.index`) or stack name.
        #[arg(add = ArgValueCompleter::new(completion::targets))]
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
        #[arg(add = ArgValueCompleter::new(completion::targets))]
        target: String,
        /// User to run as inside the container: `user`, `user:group`, `uid`
        /// or `uid:gid`. Defaults to the image's own user.
        #[arg(short = 'u', long)]
        user: Option<String>,
        /// Node to ask for the cluster's current state. Defaults to any
        /// manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
        /// Command to run and its arguments. Defaults to `bash`, falling
        /// back to `sh` if the image has no `bash`.
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
        #[arg(add = ArgValueCompleter::new(completion::targets))]
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
    /// Restic-based drive backups: destinations, per-drive schedules, and
    /// restore. Distinct from `restore`/`emergency`, which recover the
    /// cluster's own replicated metadata (config, stacks, secrets) - this is
    /// about the data *inside* drives, backed up with restic and restored by
    /// hand. These talk to a running daemon.
    Backup {
        #[command(subcommand)]
        command: Box<BackupCommand>,
    },
    /// Manual controls for the git backup feature's git -> cluster direction
    /// (see `docs/git-backup.md`): trigger the webhook's pull outside of a
    /// real webhook. Distinct from `restore`, which rebuilds a cluster's
    /// replicated state from a repo when the cluster itself has lost it -
    /// these instead operate on an already-healthy cluster's live mirror.
    GitBackup {
        #[command(subcommand)]
        command: GitBackupCommand,
    },
    /// Pull and recreate containers whose running image is behind what its
    /// registry currently serves - the same trigger the web ui's Updates
    /// page runs. Talks to a running daemon.
    Update {
        /// Instances to update (`stack.service.index`, as shown by `stack
        /// show`). Omit to update every instance with an update available,
        /// the same default the ui's "update all" uses.
        #[arg(add = ArgValueCompleter::new(completion::targets))]
        instances: Vec<String>,
        /// Node to ask. Defaults to any manager in the config; a non-leader
        /// forwards the trigger to whichever node currently leads.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
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
    #[arg(add = ArgValueCompleter::new(completion::folder_import_target))]
    pub target: String,

    /// Name of this node in the config. Defaults to the hostname, same as `daemon`.
    #[arg(long, env = "CLOUDULER_NODE")]
    pub node: Option<String>,

    /// Node to submit the volume bookkeeping through. Defaults to any manager.
    #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
    pub via: Option<String>,
}

pub use cl_init::{DriveModeArg, InitArgs, RoleArg};

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
        /// Keep a single trailing newline read from --file or stdin instead of
        /// trimming it. Editors and `echo` routinely append one, which is
        /// rarely meant to be part of the secret; pass this when it is.
        #[arg(long)]
        no_trim: bool,
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
pub enum RegistryCommand {
    /// Add or replace the login used to pull from a registry, like
    /// `docker login`.
    ///
    /// Checked against the registry before anything is stored: a wrong
    /// password is rejected here, on the spot, instead of failing later on
    /// some other node in the middle of a pull. Needs this node's cluster
    /// key, the same way `secret set` does.
    Login {
        /// Registry host, e.g. `docker.io`, `ghcr.io`, or a private
        /// `host[:port]`. Docker Hub's several spellings all fold to the
        /// same stored login.
        server: String,
        /// Username on that registry.
        #[arg(short, long)]
        username: String,
        /// Password or access token. Omit both this and --password-stdin to
        /// be prompted without echoing, which keeps it out of the shell
        /// history.
        #[arg(long, conflicts_with = "password_stdin")]
        password: Option<String>,
        /// Read the password from stdin instead of prompting, for scripts
        /// and CI where a prompt would just hang.
        #[arg(long)]
        password_stdin: bool,
        /// Node to submit through. Defaults to any manager in the config.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// List the registries with a login stored. Usernames only: a password
    /// is never readable back once stored.
    List {
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Check a stored login against its registry again, without changing it.
    Test {
        /// Registry host the login was stored under.
        #[arg(add = ArgValueCompleter::new(completion::registry_servers))]
        server: String,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Remove a stored registry login.
    #[command(alias = "rm")]
    Remove {
        /// Registry host the login was stored under.
        #[arg(add = ArgValueCompleter::new(completion::registry_servers))]
        server: String,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum DriveCommand {
    /// List the drives and the nodes implementing them.
    List,
}

/// Restic-based drive backups: where to keep them, when to run them, and how
/// to restore. See `Command::Backup`'s own doc comment for how this differs
/// from `Command::Restore`.
#[derive(Debug, Subcommand)]
pub enum BackupCommand {
    /// Manage backup destinations - the places restic keeps a drive's history.
    Destination {
        #[command(subcommand)]
        command: BackupDestinationCommand,
    },
    /// Manage a drive's schedule/retention against a destination.
    Target {
        #[command(subcommand)]
        command: BackupTargetCommand,
    },
    /// Trigger a backup run for a target immediately, outside its schedule.
    Run {
        /// Target id (see `backup target list`).
        #[arg(add = ArgValueCompleter::new(completion::backup_target_ids))]
        id: String,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Show recent backup runs, newest first.
    History {
        /// Only show runs for this target.
        #[arg(long, add = ArgValueCompleter::new(completion::backup_target_ids))]
        target: Option<String>,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// List the snapshots stored at a destination.
    Snapshots {
        /// Destination id (see `backup destination list`).
        #[arg(add = ArgValueCompleter::new(completion::backup_destination_ids))]
        destination: String,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Restore an entire drive, stopping and restarting every instance that
    /// uses it.
    RestoreDrive {
        /// Drive to restore.
        #[arg(add = ArgValueCompleter::new(completion::drive_names))]
        drive: String,
        /// Destination to restore from.
        #[arg(long, add = ArgValueCompleter::new(completion::backup_destination_ids))]
        destination: String,
        /// Snapshot id, or "latest" (the default) for the most recent one.
        #[arg(long)]
        snapshot: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Restore a single volume, scoped out of its drive's whole-drive
    /// snapshot, stopping and restarting only the instances that mount it.
    RestoreVolume {
        /// Volume to restore, as `drive/volume`.
        #[arg(add = ArgValueCompleter::new(completion::volume_ids))]
        volume: VolumeId,
        /// Destination to restore from.
        #[arg(long, add = ArgValueCompleter::new(completion::backup_destination_ids))]
        destination: String,
        /// Snapshot id, or "latest" (the default) for the most recent one.
        #[arg(long)]
        snapshot: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Restore every volume a service uses, stopping and restarting the
    /// service once.
    RestoreService {
        /// Stack the service belongs to.
        #[arg(add = ArgValueCompleter::new(completion::stack_names))]
        stack: String,
        /// Service within the stack.
        #[arg(add = ArgValueCompleter::new(completion::service_names))]
        service: String,
        /// Destination to restore from, needed only when the drive backs up
        /// to more than one.
        #[arg(long, add = ArgValueCompleter::new(completion::backup_destination_ids))]
        destination: Option<String>,
        /// Snapshot id, or "latest" (the default) for the most recent one.
        #[arg(long)]
        snapshot: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
}

/// See `Command::GitBackup`'s own doc comment.
#[derive(Debug, Subcommand)]
pub enum GitBackupCommand {
    /// Manually trigger the same git -> cluster pull the webhook normally
    /// runs on a push notification, without waiting for one (or having one
    /// configured at all).
    Pull {
        /// Node to ask. Defaults to any manager in the config; a non-leader
        /// forwards the trigger to whichever node currently leads.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Reconcile the git repository with current cluster state: builds
    /// cluster.yaml, stacks, configs, and secrets from Raft data, commits
    /// and pushes to git, and updates the last known good commit.
    Reconcile {
        /// Node to ask. Defaults to any manager in the config; a non-leader
        /// forwards the trigger to whichever node currently leads.
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum BackupDestinationCommand {
    /// Add or replace a backup destination.
    Add(Box<BackupDestinationAddArgs>),
    /// List backup destinations.
    List {
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Remove a backup destination. Refused while any target references it.
    Remove {
        #[arg(add = ArgValueCompleter::new(completion::backup_destination_ids))]
        id: String,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
}

#[derive(Debug, clap::Args)]
pub struct BackupDestinationAddArgs {
    /// Unique id for this destination. Completes existing ids too, since
    /// this command also replaces one.
    #[arg(add = ArgValueCompleter::new(completion::backup_destination_ids))]
    pub id: String,
    /// Human-readable name.
    #[arg(long)]
    pub name: String,
    #[arg(long, value_enum)]
    pub kind: BackupDestinationKindArg,
    /// `node-local`: the cluster node holding the backup directory.
    #[arg(long, required_if_eq("kind", "node-local"), add = ArgValueCompleter::new(completion::node_names))]
    pub node: Option<String>,
    /// `node-local`: directory on that node. `ssh`: directory on the
    /// remote host.
    #[arg(
        long,
        required_if_eq("kind", "node-local"),
        required_if_eq("kind", "ssh")
    )]
    pub path: Option<String>,
    /// `ssh`: remote host.
    #[arg(long, required_if_eq("kind", "ssh"))]
    pub host: Option<String>,
    /// `ssh`: port. Defaults to 22.
    #[arg(long, default_value_t = 22)]
    pub port: u16,
    /// `ssh`: user to connect as.
    #[arg(long, required_if_eq("kind", "ssh"))]
    pub user: Option<String>,
    /// `s3`: bucket name.
    #[arg(long, required_if_eq("kind", "s3"))]
    pub bucket: Option<String>,
    /// `s3`: endpoint (host, optionally with port).
    #[arg(long, required_if_eq("kind", "s3"))]
    pub endpoint: Option<String>,
    /// `s3`: region, if the endpoint needs one.
    #[arg(long)]
    pub region: Option<String>,
    /// `s3`: key prefix within the bucket.
    #[arg(long)]
    pub prefix: Option<String>,
    /// Repository password. Auto-generated and sealed if omitted -
    /// restic always needs one regardless of backend.
    #[arg(long)]
    pub password: Option<String>,
    /// `ssh`: path to a private key file to seal as this destination's
    /// credentials.
    #[arg(long, conflicts_with = "s3_secret_access_key")]
    pub ssh_key_file: Option<PathBuf>,
    /// `s3`: access key id. Requires --s3-secret-access-key.
    #[arg(long, requires = "s3_secret_access_key")]
    pub s3_access_key_id: Option<String>,
    /// `s3`: secret access key. Requires --s3-access-key-id.
    #[arg(long, requires = "s3_access_key_id")]
    pub s3_secret_access_key: Option<String>,
    #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
    pub via: Option<String>,
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum BackupDestinationKindArg {
    NodeLocal,
    Ssh,
    S3,
}

#[derive(Debug, Subcommand)]
pub enum BackupTargetCommand {
    /// Add or update a drive's schedule/retention against a destination.
    Set {
        /// Unique id for this target. Completes existing ids too, since
        /// this command also updates one.
        #[arg(add = ArgValueCompleter::new(completion::backup_target_ids))]
        id: String,
        /// Drive to back up.
        #[arg(long, add = ArgValueCompleter::new(completion::drive_names))]
        drive: String,
        /// Destination to back up to.
        #[arg(long, add = ArgValueCompleter::new(completion::backup_destination_ids))]
        destination: String,
        /// How often to run, e.g. "1d", "12h".
        #[arg(long)]
        frequency: String,
        /// Time of day (UTC, "HH:MM") to run at, once due. Any time of day
        /// if omitted.
        #[arg(long)]
        time_of_day: Option<String>,
        /// Keep at least this many recent snapshots.
        #[arg(long)]
        keep_last: Option<u32>,
        /// Keep every snapshot within this duration, e.g. "30d".
        #[arg(long)]
        keep_within: Option<String>,
        /// Stop containers using the drive before backing up. Defaults to
        /// the x-clouduler stack/service override, then true.
        #[arg(long)]
        stop_containers: Option<bool>,
        /// Create disabled - the target exists but never runs on its own
        /// until `--enabled` is passed on a later `set`.
        #[arg(long)]
        enabled: bool,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// List backup targets.
    List {
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
    /// Remove a backup target.
    Remove {
        #[arg(add = ArgValueCompleter::new(completion::backup_target_ids))]
        id: String,
        #[arg(long, add = ArgValueCompleter::new(completion::node_names))]
        via: Option<String>,
    },
}
