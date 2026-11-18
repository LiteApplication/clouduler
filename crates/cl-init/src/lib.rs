//! `clouduler init`: set this machine up as a node.
//!
//! Run bare it asks about each step (enter skips). With `--yes` and enough
//! arguments it asks nothing, for provisioning scripts:
//!
//! ```text
//! clouduler init --yes --join 10.10.0.1 --token-file /run/cl.token --role worker
//! ```
//!
//! init writes the config file and optionally the systemd unit. Later edits
//! from the web ui come back to this file through raft, so init is only a
//! starting point.

pub mod detect;
pub mod docker;
pub mod join;
pub mod permissions;
pub mod plan;
pub mod prompt;
pub mod systemd;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cl_config::{ClusterConfig, DriveMode, NodeRole, DEFAULT_RPC_PORT};
use cl_secret::ClusterKey;
use ipnet::IpNet;

use plan::{ClusterPlan, NodePlan};
use prompt::Prompt;

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

    /// A copy of the cluster's secret key to install, instead of fetching it
    /// from the node being joined. Joining only.
    #[arg(long)]
    pub key_file: Option<PathBuf>,

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
    pub subnet: Option<IpNet>,

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

    /// Where this node's docker daemon stores images and containers. Left
    /// unset, dockerd keeps its own default (/var/lib/docker).
    #[arg(long)]
    pub docker_data_dir: Option<PathBuf>,

    /// Install systemd service unit.
    #[arg(long, overrides_with = "no_systemd")]
    pub systemd: bool,

    #[arg(long, overrides_with = "systemd")]
    pub no_systemd: bool,

    /// Start systemd service immediately after setup.
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

/// Where the service runs the daemon from; init copies itself here.
const DEFAULT_BINARY_PATH: &str = "/usr/bin/clouduler";

/// Default for the data directory question, matching `cl_config`'s own default.
const DEFAULT_DATA_DIR: &str = "/var/lib/clouduler";

pub async fn run(args: InitArgs, config_path: PathBuf) -> Result<()> {
    let mut prompt = prompt::stdio(!args.yes);
    prompt.say("clouduler init")?;
    prompt.say("press enter to take the value in brackets, which skips the step\n")?;

    if config_path.exists() && !args.force {
        bail!(
            "{} already exists. This machine looks set up already; pass --force to start over",
            config_path.display()
        );
    }

    let node_name = match &args.node {
        Some(name) => name.clone(),
        None => {
            let default = detect::hostname().ok();
            prompt.ask("node name", default.as_deref())?
        }
    };

    let (config, peer) = match &args.join {
        Some(address) => {
            let (config, peer) = join_cluster(&mut prompt, &args, &node_name, address).await?;
            (config, Some(peer))
        }
        None => match choose_mode(&mut prompt)? {
            Mode::Create => (create_cluster(&mut prompt, &args, &node_name)?, None),
            Mode::Join => {
                let address = prompt.ask("address of a node already in the cluster", None)?;
                let (config, peer) = join_cluster(&mut prompt, &args, &node_name, &address).await?;
                (config, Some(peer))
            }
        },
    };

    write_config(&config_path, &config)?;
    prompt.say(&format!("\nwrote {}", config_path.display()))?;

    setup_data_directory(&mut prompt, &config, &node_name)?;
    setup_drive_directories(&mut prompt, &config, &node_name)?;
    setup_docker_data_dir(&mut prompt, &config, &node_name)?;
    check_nfs_availability(&mut prompt, &config, &node_name)?;
    check_ethtool_availability(&mut prompt)?;
    check_restic_availability(&mut prompt)?;
    setup_secret_key(&mut prompt, &args, &config, &node_name, peer.as_ref()).await?;
    setup_systemd(&mut prompt, &args, &config, &config_path, &node_name)?;
    summarize(&mut prompt, &config, &node_name)?;
    Ok(())
}

enum Mode {
    Create,
    Join,
}

fn choose_mode<R: BufRead, W: Write>(prompt: &mut Prompt<R, W>) -> Result<Mode> {
    prompt.heading("cluster")?;
    let options = [
        "create a new cluster with this node as its first manager",
        "join a cluster that already exists",
    ];
    match prompt.choose("is this the first node?", &options, 0)? {
        0 => Ok(Mode::Create),
        _ => Ok(Mode::Join),
    }
}

fn create_cluster<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    args: &InitArgs,
    node_name: &str,
) -> Result<ClusterConfig> {
    prompt.heading("new cluster")?;

    let cluster_name = ask_or(
        prompt,
        args.cluster_name.clone(),
        "cluster name",
        Some("home"),
    )?;

    let subnet: IpNet = match args.subnet {
        Some(subnet) => subnet,
        None => prompt
            .ask(
                "internal network subnet, the one every node talks over",
                Some("10.10.0.0/24"),
            )?
            .parse()
            .context("that is not a subnet in cidr form, e.g. 10.10.0.0/24")?,
    };

    let internal_domain = ask_optional_or(
        prompt,
        args.internal_domain.clone(),
        "domain whose subdomains resolve to internal addresses, blank for none",
        None,
    )?;

    let web_address = ask_or(
        prompt,
        args.web_address.clone(),
        "hostname the web ui answers on",
        Some(if internal_domain.is_some() {
            "cloud"
        } else {
            "cloud.example.com"
        }),
    )?;
    let web_port = ask_parsed(prompt, args.web_port, "port the web ui listens on", &8443)?;

    prompt.heading("this node")?;
    let ip = ask_ip(prompt, args.ip, subnet)?;
    let mut node = NodePlan::new(node_name, NodeRole::Manager, ip);
    node.rpc_port = ask_parsed(
        prompt,
        args.rpc_port,
        "node to node rpc port",
        &DEFAULT_RPC_PORT,
    )?;
    node.data_dir = ask_optional_or(
        prompt,
        args.node_data_dir.as_deref().map(path_string),
        "directory for this node's database and working files",
        Some(DEFAULT_DATA_DIR),
    )?
    .map(PathBuf::from);
    node.docker_data_dir = ask_optional_or(
        prompt,
        args.docker_data_dir.as_deref().map(path_string),
        "directory for docker's own data (images, containers), blank to leave docker's default",
        None,
    )?
    .map(PathBuf::from);

    let mode = drive_mode(args.drive_mode);
    node.drives = drives_from_args(&args.drive)?;
    if node.drives.is_empty() {
        node.drives = ask_drives(prompt, &[])?;
    }
    let drives = node
        .drives
        .keys()
        .map(|name| (name.clone(), plan::drive_definition(mode, node_name)))
        .collect();

    plan::new_cluster(&ClusterPlan {
        cluster_name,
        subnet,
        internal_domain,
        web_address,
        web_port,
        drives,
        node,
    })
}

async fn join_cluster<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    args: &InitArgs,
    node_name: &str,
    address: &str,
) -> Result<(ClusterConfig, join::Peer)> {
    prompt.heading("joining")?;

    let token = read_token(prompt, args)?;
    let peer = join::Peer::connect(address, token, args.tls_fingerprint.clone())?;
    let existing = peer.fetch_config().await?;

    if let Some(seen) = peer.peer_fingerprint() {
        if args.tls_fingerprint.is_none() {
            prompt.say(&format!(
                "the node presented certificate {seen}\n\
                 it is self signed, so nothing vouches for it. Compare it against that node before trusting the ui,\n\
                 and pass it as --tls-fingerprint on the next node you add"
            ))?;
        }
    }

    let (internal_name, internal) = existing.internal_network();
    let subnet = internal
        .subnet
        .context("the cluster's internal network declares no subnet")?;
    prompt.say(&format!(
        "joining cluster {} on {internal_name} {subnet}, {} node(s) so far",
        existing.cluster.name,
        existing.nodes.len()
    ))?;

    let role = match args.role {
        Some(role) => to_role(role),
        None => {
            let options = [
                "worker, runs containers and follows the quorum",
                "manager, also votes and can lead the cluster",
            ];
            match prompt.choose("role for this node", &options, 0)? {
                0 => NodeRole::Worker,
                _ => NodeRole::Manager,
            }
        }
    };

    let ip = ask_ip(prompt, args.ip, subnet)?;
    let mut node = NodePlan::new(node_name, role, ip);
    node.rpc_port = ask_parsed(
        prompt,
        args.rpc_port,
        "node to node rpc port",
        &DEFAULT_RPC_PORT,
    )?;
    node.data_dir = ask_optional_or(
        prompt,
        args.node_data_dir.as_deref().map(path_string),
        "directory for this node's database and working files, blank to use the cluster's",
        None,
    )?
    .map(PathBuf::from);
    node.docker_data_dir = ask_optional_or(
        prompt,
        args.docker_data_dir.as_deref().map(path_string),
        "directory for docker's own data (images, containers), blank to leave docker's default",
        None,
    )?
    .map(PathBuf::from);

    node.drives = drives_from_args(&args.drive)?;
    if node.drives.is_empty() {
        let known: Vec<&str> = existing.drives.keys().map(String::as_str).collect();
        node.drives = ask_drives(prompt, &known)?;
    }

    // A node joining an existing cluster never invents its own key. It gets
    // the real one further down, from the cluster over this same connection or
    // from a copy the operator provides.
    node.generate_secret_key = Some(false);

    let amended = plan::add_node(&existing, &node, args.force)?;
    peer.push_config(&amended)
        .await
        .context("the cluster refused the join")?;
    prompt.say("the cluster accepted this node")?;
    Ok((amended, peer))
}

fn read_token<R: BufRead, W: Write>(prompt: &mut Prompt<R, W>, args: &InitArgs) -> Result<String> {
    if let Some(token) = &args.token {
        return Ok(token.clone());
    }
    if let Some(path) = &args.token_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading the api key from {}", path.display()))?;
        return Ok(raw.trim().to_string());
    }
    prompt.say("the cluster prints an api key on its first launch; it is also in the ui")?;
    prompt.ask("api key", None)
}

/// Ask which drives this node implements and where they live locally.
///
/// A non-empty `known` restricts a joining node to it: drives are a cluster
/// wide namespace created from the ui.
fn ask_drives<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    known: &[&str],
) -> Result<BTreeMap<String, PathBuf>> {
    prompt.heading("drives")?;
    let mut drives = BTreeMap::new();

    if known.is_empty() {
        prompt.say("a drive is a cluster wide namespace this node backs with a local directory")?;
    } else {
        prompt.say(&format!("this cluster has: {}", known.join(", ")))?;
    }

    loop {
        let question = if drives.is_empty() {
            "drive name, blank to skip"
        } else {
            "another drive name, blank when done"
        };
        let Some(name) = prompt.ask_optional(question, None)? else {
            return Ok(drives);
        };
        if !known.is_empty() && !known.contains(&name.as_str()) {
            prompt.say(&format!(
                "  this cluster has no drive called {name}. Create it from the web ui first"
            ))?;
            continue;
        }

        let default = format!("/srv/clouduler/{name}");
        let path = prompt.ask(&format!("local directory backing {name}"), Some(&default))?;
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            prompt.say("  that has to be an absolute path")?;
            continue;
        }
        drives.insert(name, path);
    }
}

fn drives_from_args(args: &[String]) -> Result<BTreeMap<String, PathBuf>> {
    args.iter()
        .map(|arg| plan::parse_drive_arg(arg))
        .collect::<Result<BTreeMap<_, _>>>()
}

/// Create the directory for the daemon's database and working files. Doing it
/// here turns a permission error into a terminal line, not a systemd restart loop.
fn setup_data_directory<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    config: &ClusterConfig,
    node_name: &str,
) -> Result<()> {
    let Some(node) = config.node(node_name) else {
        return Ok(());
    };
    let path = node.data_dir(config);
    if path.is_dir() {
        prompt.say(&format!(
            "data directory {} is already there",
            path.display()
        ))?;
        return Ok(());
    }
    match std::fs::create_dir_all(path) {
        Ok(()) => prompt.say(&format!("created {} for this node's files", path.display()))?,
        Err(e) => prompt.say(&format!(
            "could not create {}: {e}. Create it before starting the daemon",
            path.display()
        ))?,
    }
    Ok(())
}

/// Create the directories the declared drives map to.
fn setup_drive_directories<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    config: &ClusterConfig,
    node_name: &str,
) -> Result<()> {
    let Some(node) = config.node(node_name) else {
        return Ok(());
    };
    for (name, path) in &node.drives {
        if path.is_dir() {
            continue;
        }
        match std::fs::create_dir_all(path) {
            Ok(()) => prompt.say(&format!("created {} for drive {name}", path.display()))?,
            Err(e) => prompt.say(&format!(
                "could not create {} for drive {name}: {e}. Create it before starting the daemon",
                path.display()
            ))?,
        }
    }
    Ok(())
}

/// Point dockerd's `data-root` at the configured directory, if any.
///
/// Only the config file is written and the target created: docker may not be
/// installed yet, and an existing `/var/lib/docker` is the operator's call.
fn setup_docker_data_dir<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    config: &ClusterConfig,
    node_name: &str,
) -> Result<()> {
    let Some(node) = config.node(node_name) else {
        return Ok(());
    };
    let Some(data_root) = &node.docker_data_dir else {
        return Ok(());
    };

    if let Err(e) = std::fs::create_dir_all(data_root) {
        prompt.say(&format!(
            "could not create {}: {e}. Create it before starting docker",
            data_root.display()
        ))?;
        return Ok(());
    }

    let daemon_json = docker::default_path();
    match docker::set_data_root(&daemon_json, data_root) {
        Ok(true) => prompt.say(&format!(
            "set data-root to {} in {}. Restart docker (systemctl restart docker) to pick it up; \
             anything already under docker's old data-root stays there and is not moved",
            data_root.display(),
            daemon_json.display(),
        ))?,
        Ok(false) => prompt.say(&format!(
            "{} already points docker at {}",
            daemon_json.display(),
            data_root.display()
        ))?,
        Err(e) => prompt.say(&format!(
            "could not write {}: {e}. Set \"data-root\": \"{}\" there by hand before starting docker",
            daemon_json.display(),
            data_root.display()
        ))?,
    }
    Ok(())
}

/// Advisory only: any node in a cluster with a served drive might end up
/// serving or mounting one (see `clouduler::drives`), declared here or not.
/// Blocking would take out nodes that are fine otherwise, e.g. a worker with
/// no local storage.
fn check_nfs_availability<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    config: &ClusterConfig,
    node_name: &str,
) -> Result<()> {
    if !config.drives.values().any(cl_config::Drive::is_served) {
        return Ok(());
    }
    if let Err(e) = cl_drive::exports_available() {
        prompt.say(&format!(
            "warning: this cluster has a served drive, but this node ({node_name}) does not look ready to serve NFS: {e}. \
             Install and enable the kernel NFS server (nfs-kernel-server/nfs-utils) before it needs to serve or mount one"
        ))?;
    }
    // Any node might mount a served drive it never declared (see
    // `clouduler::drives`), so the client-side check applies as broadly.
    if let Err(e) = cl_drive::client_available() {
        prompt.say(&format!(
            "warning: this cluster has a served drive, but this node ({node_name}) cannot mount one yet: {e}"
        ))?;
    }
    Ok(())
}

/// Advisory: any node running a stack with its own network needs `ethtool` once
/// a container lands on its overlay bridge (see
/// `cl_net::vxlan::disable_tx_checksum_offload`), so this is not gated on config.
fn check_ethtool_availability<R: BufRead, W: Write>(prompt: &mut Prompt<R, W>) -> Result<()> {
    if !cl_net::ethtool_available() {
        prompt.say(
            "warning: ethtool is not on PATH. Every container on a stack's own overlay network \
             will silently drop real request/response traffic until it's installed",
        )?;
    }
    Ok(())
}

/// Advisory: whether a backup target gets enabled is unknown at `init` time, so
/// this checks unconditionally.
fn check_restic_availability<R: BufRead, W: Write>(prompt: &mut Prompt<R, W>) -> Result<()> {
    if !cl_restic::dependency::restic_available() {
        prompt.say(
            "warning: restic is not on PATH. Scheduled drive backups will fail until it's installed \
             on every node that ends up running one",
        )?;
    }
    Ok(())
}

/// Put the cluster key in place before the daemon first starts.
///
/// Every node to node connection proves it holds the cluster key, so a joining
/// node without it is refused by every peer and cannot be told it through raft.
/// It arrives over the admin-authenticated https api that accepted the join,
/// wrapped for this node's keypair (`join::Peer::fetch_cluster_key`). A copy the
/// operator holds (`--key-file`, or the prompt) works too. A founding node has
/// neither; the daemon generates the key there.
async fn setup_secret_key<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    args: &InitArgs,
    config: &ClusterConfig,
    node_name: &str,
    peer: Option<&join::Peer>,
) -> Result<()> {
    let path = &config.secrets.key_file;
    let node = config
        .node(node_name)
        .context("node missing from its own config")?;

    if path.exists() {
        match ClusterKey::load(path) {
            Ok(key) => prompt.say(&format!(
                "cluster key already present at {} ({})",
                path.display(),
                key.fingerprint()
            ))?,
            Err(e) => prompt.say(&format!("the key at {} is unusable: {e}", path.display()))?,
        }
        return Ok(());
    }

    if node.generates_secret_key(config) {
        prompt.say(&format!(
            "the daemon will generate the cluster key at {} on first launch. Back that file up:\n\
             without it every stored secret is unrecoverable",
            path.display()
        ))?;
        return Ok(());
    }

    prompt.heading("secrets")?;
    prompt.say(
        "this cluster's secrets are sealed with a key every node holds, and every node to node\n\
         connection is authenticated with it, so this node needs it before its daemon starts.",
    )?;

    let source = match (&args.key_file, peer) {
        (Some(file), _) => Some(path_string(file)),
        (None, Some(_)) => {
            let options = [
                "fetch it from the cluster, wrapped so only this node can open it",
                "copy it from a file I already have",
                "skip, I will put it in place myself",
            ];
            match prompt.choose("how should this node get the cluster key?", &options, 0)? {
                0 => None,
                1 => Some(prompt.ask("path to a copy of the cluster key", None)?),
                _ => {
                    prompt.say(&format!(
                        "skipped. Put the cluster key at {} before starting the daemon, or every peer will refuse it",
                        path.display()
                    ))?;
                    return Ok(());
                }
            }
        }
        (None, None) => {
            let Some(source) =
                prompt.ask_optional("path to a copy of the cluster key, blank to skip", None)?
            else {
                prompt.say(&format!(
                    "skipped. Put the cluster key at {} before starting the daemon, or every peer will refuse it",
                    path.display()
                ))?;
                return Ok(());
            };
            Some(source)
        }
    };

    let key = match (source, peer) {
        (Some(source), _) => {
            ClusterKey::load(&source).with_context(|| format!("reading the key from {source}"))?
        }
        (None, Some(peer)) => {
            let (keypair, _) =
                cl_secret::NodeKeypair::load_or_create(&config.secrets.node_key_file)
                    .with_context(|| {
                        format!(
                            "preparing this node's keypair at {}",
                            config.secrets.node_key_file.display()
                        )
                    })?;
            match peer.fetch_cluster_key(node_name, &keypair).await {
                Ok(key) => key,
                Err(e) => {
                    // The cluster has already accepted this node, so failing
                    // outright would strand it half set up. Say what to do.
                    prompt.say(&format!(
                        "could not fetch the cluster key: {e:#}\n\
                         Put a copy at {} (or re-run init with --key-file) before starting the daemon",
                        path.display()
                    ))?;
                    return Ok(());
                }
            }
        }
        (None, None) => return Ok(()),
    };

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    key.save(path)
        .with_context(|| format!("writing the key to {}", path.display()))?;
    prompt.say(&format!(
        "installed the cluster key ({})",
        key.fingerprint()
    ))?;
    Ok(())
}

fn setup_systemd<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    args: &InitArgs,
    config: &ClusterConfig,
    config_path: &Path,
    node_name: &str,
) -> Result<()> {
    prompt.heading("service")?;

    if args.no_systemd {
        prompt.say("skipping the systemd unit")?;
        return Ok(());
    }
    if !systemd::is_available() {
        prompt.say("this machine is not running systemd, so no unit was installed")?;
        return Ok(());
    }
    if !detect::is_root() {
        prompt.say(&format!(
            "this is not running as root, so the unit cannot be written to {}.\n\
             Re-run init with sudo to install the service, or pass --no-systemd to skip it",
            systemd::UNIT_DIR
        ))?;
        if !prompt.confirm("try anyway?", false)? {
            prompt.say(&format!(
                "skipped. Later: sudo {} init --force",
                std::env::current_exe()
                    .map(|exe| path_string(&exe))
                    .unwrap_or_else(|_| "clouduler".into())
            ))?;
            return Ok(());
        }
    }
    if !args.systemd && !prompt.confirm("install a systemd unit for the daemon?", true)? {
        return Ok(());
    }

    let current = std::env::current_exe().context("finding this binary's path")?;
    let binary = PathBuf::from(prompt.ask(
        "path the service should run",
        Some(&path_string(Path::new(DEFAULT_BINARY_PATH))),
    )?);

    // Put the binary where the unit expects it before the unit exists, so the
    // service is never pointed at a path holding nothing.
    systemd::install_binary(&current, &binary)?;
    if binary != current {
        prompt.say(&format!(
            "copied {} to {}",
            current.display(),
            binary.display()
        ))?;
    }

    let unit = systemd::Unit {
        binary,
        config: config_path.to_path_buf(),
        node: node_name.to_string(),
        proxy_ports: config
            .proxy
            .enabled
            .then_some((config.proxy.http_port, config.proxy.https_port)),
        proxy_v6: config.proxy.enabled && config.node_has_public_v6(node_name),
    };

    let path = systemd::install(&unit)?;
    prompt.say(&format!("wrote {}", path.display()))?;
    if unit.render_socket().is_some() {
        prompt.say(&format!(
            "wrote {} (holds the proxy ports across restarts)",
            systemd::socket_path().display()
        ))?;
    }

    if args.no_start {
        prompt.say(&format!(
            "start it with: systemctl enable --now {}",
            systemd::UNIT_NAME
        ))?;
        return Ok(());
    }
    if !args.start && !prompt.confirm("enable and start it now?", true)? {
        prompt.say(&format!(
            "start it with: systemctl enable --now {}",
            systemd::UNIT_NAME
        ))?;
        return Ok(());
    }

    systemd::enable_and_start(&unit)?;
    prompt.say("the daemon is running")?;
    Ok(())
}

fn summarize<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    config: &ClusterConfig,
    node_name: &str,
) -> Result<()> {
    prompt.heading("done")?;
    let node = config
        .node(node_name)
        .context("node missing from its own config")?;
    prompt.say(&format!(
        "{node_name} is a {} in cluster {} at {}",
        node.role,
        config.cluster.name,
        config
            .internal_ip(node)
            .map(|ip| ip.to_string())
            .unwrap_or_default()
    ))?;
    prompt.say(&format!(
        "the web ui is at https://{}:{}",
        config.web_fqdn(),
        config.web.port
    ))?;
    prompt.say(
        "networks, drives and everything else in the config are editable from there, and an edit\n\
         is written back to this file on every node",
    )?;
    Ok(())
}

fn write_config(path: &Path, config: &ClusterConfig) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let yaml = serde_yaml::to_string(config).context("serializing the config")?;
    std::fs::write(path, yaml).with_context(|| format!("writing {}", path.display()))
}

/// Ask for this node's internal address, defaulting to the one the routing table
/// would use to reach that subnet.
fn ask_ip<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    given: Option<IpAddr>,
    subnet: IpNet,
) -> Result<IpAddr> {
    if let Some(ip) = given {
        return Ok(ip);
    }
    let detected = detect::address_in(subnet).map(|ip| ip.to_string());
    if detected.is_none() {
        prompt.say(&format!(
            "no local address inside {subnet} was found, so this one has to be given"
        ))?;
    }
    prompt
        .ask(
            &format!("this node's address on the internal network {subnet}"),
            detected.as_deref(),
        )?
        .parse()
        .context("that is not an ip address")
}

fn ask_or<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    given: Option<String>,
    question: &str,
    default: Option<&str>,
) -> Result<String> {
    match given {
        Some(value) => Ok(value),
        None => prompt.ask(question, default),
    }
}

fn ask_optional_or<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    given: Option<String>,
    question: &str,
    default: Option<&str>,
) -> Result<Option<String>> {
    match given {
        Some(value) => Ok(Some(value)),
        None => prompt.ask_optional(question, default),
    }
}

fn ask_parsed<T, R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    given: Option<T>,
    question: &str,
    default: &T,
) -> Result<T>
where
    T: std::str::FromStr + std::fmt::Display,
    T::Err: std::fmt::Display,
{
    if let Some(value) = given {
        return Ok(value);
    }
    let answer = prompt.ask(question, Some(&default.to_string()))?;
    answer
        .parse()
        .map_err(|e| anyhow::anyhow!("{question}: {answer} does not parse: {e}"))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn to_role(role: RoleArg) -> NodeRole {
    match role {
        RoleArg::Manager => NodeRole::Manager,
        RoleArg::Worker => NodeRole::Worker,
    }
}

fn drive_mode(mode: DriveModeArg) -> DriveMode {
    match mode {
        DriveModeArg::Replicated => DriveMode::Replicated,
        DriveModeArg::Served => DriveMode::Served,
    }
}

#[cfg(test)]
impl InitArgs {
    /// `init --yes`, every other argument left out.
    pub(crate) fn parse_from_yes() -> Self {
        use clap::Parser;
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: InitArgs,
        }
        Wrapper::parse_from(["init", "--yes"]).args
    }
}
