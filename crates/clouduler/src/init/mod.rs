//! `clouduler init`: set this machine up as a node.
//!
//! Two shapes of the same walk. Run bare it asks about each step, every one of
//! which can be skipped by pressing enter. Given `--yes` and enough arguments
//! it asks nothing, which is what a provisioning script wants:
//!
//! ```text
//! clouduler init --yes --join 10.10.0.1 --token-file /run/cl.token --role worker
//! ```
//!
//! What init writes is the config file and, optionally, the systemd unit.
//! Everything in that file is also editable from the web ui afterwards, and an
//! edit there comes back down to this file through raft, so init is a starting
//! point rather than the only way to change any of it.

pub mod detect;
pub mod join;
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

use crate::cli::{DriveModeArg, InitArgs, RoleArg};
use plan::{ClusterPlan, NodePlan};
use prompt::Prompt;

/// Where the service runs the daemon from. init copies itself here rather than
/// leaving the unit pointing at wherever the binary happened to be downloaded.
const DEFAULT_BINARY_PATH: &str = "/usr/bin/clouduler";

/// Offered as the answer to the data directory question. It matches
/// `cl_config`'s own default, so taking it writes the same path the daemon
/// would have picked anyway, only now visibly and created up front.
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

    let config = match &args.join {
        Some(address) => join_cluster(&mut prompt, &args, &node_name, address).await?,
        None => match choose_mode(&mut prompt)? {
            Mode::Create => create_cluster(&mut prompt, &args, &node_name)?,
            Mode::Join => {
                let address = prompt.ask("address of a node already in the cluster", None)?;
                join_cluster(&mut prompt, &args, &node_name, &address).await?
            }
        },
    };

    write_config(&config_path, &config)?;
    prompt.say(&format!("\nwrote {}", config_path.display()))?;

    setup_data_directory(&mut prompt, &config, &node_name)?;
    setup_drive_directories(&mut prompt, &config, &node_name)?;
    check_nfs_availability(&mut prompt, &config, &node_name)?;
    setup_secret_key(&mut prompt, &config, &node_name)?;
    setup_systemd(&mut prompt, &args, &config_path, &node_name)?;
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

/// Build the config for a cluster that starts here.
fn create_cluster<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    args: &InitArgs,
    node_name: &str,
) -> Result<ClusterConfig> {
    prompt.heading("new cluster")?;

    let cluster_name = ask_or(prompt, args.cluster_name.clone(), "cluster name", Some("home"))?;

    let subnet: IpNet = match args.subnet {
        Some(subnet) => subnet,
        None => prompt
            .ask("internal network subnet, the one every node talks over", Some("10.10.0.0/24"))?
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
        Some(if internal_domain.is_some() { "cloud" } else { "cloud.example.com" }),
    )?;
    let web_port = ask_parsed(prompt, args.web_port, "port the web ui listens on", 8443)?;

    prompt.heading("this node")?;
    let ip = ask_ip(prompt, args.ip, subnet)?;
    let mut node = NodePlan::new(node_name, NodeRole::Manager, ip);
    node.rpc_port = ask_parsed(prompt, args.rpc_port, "node to node rpc port", DEFAULT_RPC_PORT)?;
    node.data_dir = ask_optional_or(
        prompt,
        args.node_data_dir.clone().map(path_string),
        "directory for this node's database and working files",
        Some(DEFAULT_DATA_DIR),
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

/// Fetch the cluster's config, add this node to it, and push it back.
async fn join_cluster<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    args: &InitArgs,
    node_name: &str,
    address: &str,
) -> Result<ClusterConfig> {
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
    node.rpc_port = ask_parsed(prompt, args.rpc_port, "node to node rpc port", DEFAULT_RPC_PORT)?;
    node.data_dir = ask_optional_or(
        prompt,
        args.node_data_dir.clone().map(path_string),
        "directory for this node's database and working files, blank to use the cluster's",
        None,
    )?
    .map(PathBuf::from);


    node.drives = drives_from_args(&args.drive)?;
    if node.drives.is_empty() {
        let known: Vec<&str> = existing.drives.keys().map(String::as_str).collect();
        node.drives = ask_drives(prompt, &known)?;
    }

    // A node joining an existing cluster never invents its own key. Whether it
    // gets the real one is a separate question, answered further down.
    node.generate_secret_key = Some(false);

    let amended = plan::add_node(&existing, &node, args.force)?;
    peer.push_config(&amended)
        .await
        .context("the cluster refused the join")?;
    prompt.say("the cluster accepted this node")?;
    Ok(amended)
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
/// `known` lists the drives the cluster already has; when it is non empty a
/// joining node may only pick from it, because a drive is a cluster wide
/// namespace and is created from the ui, not from a node.
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

/// Create the directory the daemon keeps its database and working files in.
///
/// The daemon would create it on first launch anyway, but by then it is running
/// under systemd where a permission error is a restart loop in the journal
/// rather than a line on the terminal of the person doing the install.
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
        prompt.say(&format!("data directory {} is already there", path.display()))?;
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

/// Create the directories the declared drives are mapped to, so the first
/// container needing one does not fail on a missing path.
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

/// Advisory only, like the rest of init: a served drive's data is reachable
/// from any node over the internal network (see `clouduler::drives`), so any
/// node in a cluster with at least one served drive might end up serving or
/// mounting one, whether or not this node declares one itself. Not blocking
/// init over it would take out a node that is otherwise fine for everything
/// except served drives, which is a normal thing for a node to be (e.g. a
/// worker with no local storage at all).
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
    Ok(())
}

/// Put the cluster key in place, which is the one thing a joining node cannot
/// work out for itself.
///
/// Without it this node replicates sealed secrets like any other but cannot
/// start a container that uses one. That is a fine state for a worker you do not
/// want holding the keys to everything, so it is offered rather than pushed.
fn setup_secret_key<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    config: &ClusterConfig,
    node_name: &str,
) -> Result<()> {
    let path = &config.secrets.key_file;
    let node = config.node(node_name).context("node missing from its own config")?;

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
        "this cluster's secrets are sealed with a key held on another node. Copy that file here to\n\
         let this node run containers that use secrets, or skip and it simply will not run those.",
    )?;
    let Some(source) = prompt.ask_optional("path to a copy of the cluster key, blank to skip", None)?
    else {
        prompt.say("skipped, this node will not be able to open secrets")?;
        return Ok(());
    };

    let key = ClusterKey::load(&source).with_context(|| format!("reading the key from {source}"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    key.save(path)
        .with_context(|| format!("writing the key to {}", path.display()))?;
    prompt.say(&format!("installed the cluster key ({})", key.fingerprint()))?;
    Ok(())
}

fn setup_systemd<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    args: &InitArgs,
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
                    .map(path_string)
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
        Some(&path_string(PathBuf::from(DEFAULT_BINARY_PATH))),
    )?);

    // Put the binary where the unit expects it before the unit exists, so the
    // service is never pointed at a path holding nothing.
    systemd::install_binary(&current, &binary)?;
    if binary != current {
        prompt.say(&format!("copied {} to {}", current.display(), binary.display()))?;
    }

    let unit = systemd::Unit {
        binary,
        config: config_path.to_path_buf(),
        node: node_name.to_string(),
    };

    let path = systemd::install(&unit)?;
    prompt.say(&format!("wrote {}", path.display()))?;

    if args.no_start {
        prompt.say(&format!("start it with: systemctl enable --now {}", systemd::UNIT_NAME))?;
        return Ok(());
    }
    if !args.start && !prompt.confirm("enable and start it now?", true)? {
        prompt.say(&format!("start it with: systemctl enable --now {}", systemd::UNIT_NAME))?;
        return Ok(());
    }

    systemd::enable_and_start()?;
    prompt.say("the daemon is running")?;
    Ok(())
}

fn summarize<R: BufRead, W: Write>(
    prompt: &mut Prompt<R, W>,
    config: &ClusterConfig,
    node_name: &str,
) -> Result<()> {
    prompt.heading("done")?;
    let node = config.node(node_name).context("node missing from its own config")?;
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

/// Ask for this node's internal address, defaulting to the one the routing
/// table says this machine would use to reach that subnet. That default is
/// usually right and is the whole reason `--ip` is optional.
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
    default: T,
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

fn path_string(path: PathBuf) -> String {
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
