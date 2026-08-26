//! The `clouduler node` commands that change live cluster state. These talk
//! to a running daemon over the node RPC channel, the same way the `stack`
//! commands do, and mirror what the web ui's `/api/nodes` routes do to the
//! replicated roster and the config file.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_raft::state::{Availability, NodeState, Request, Role};
use tracing::debug;

use crate::cli::AvailabilityArg;
use crate::init::plan::parse_drive_arg;
use crate::init::prompt::Prompt;
use crate::stack::client_for;

pub async fn add(
    cfg: &ClusterConfig,
    name: String,
    role: Role,
    rpc_addr: SocketAddr,
    drives: &[String],
    labels: &[String],
    via: Option<&str>,
) -> Result<()> {
    let drives = drives
        .iter()
        .map(|arg| parse_drive_arg(arg))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let labels = labels
        .iter()
        .map(|arg| parse_label_arg(arg))
        .collect::<Result<BTreeMap<_, _>>>()?;

    let client = client_for(cfg, via)?;
    debug!(
        name = name.as_str(),
        ?role,
        %rpc_addr,
        drives = drives.len(),
        labels = labels.len(),
        via = via.unwrap_or("<any manager>"),
        "submitting UpsertNode"
    );
    let response = client
        .submit(Request::UpsertNode(NodeState {
            name: name.clone(),
            id: uuid::Uuid::new_v4(),
            role,
            rpc_addr,
            drives,
            availability: Availability::Active,
            labels,
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: String::new(),
            binary_hash: String::new(),
            node_pubkey: None,
        }))
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected the node: {error}");
    }

    println!(
        "added {name} to the live roster. This does not touch the config file: edit it and run \
         `clouduler config reload`, or a restarted manager will forget about it"
    );
    Ok(())
}

fn parse_label_arg(arg: &str) -> Result<(String, String)> {
    let Some((key, value)) = arg.split_once('=') else {
        bail!("--label takes key=value, got {arg}");
    };
    let key = key.trim();
    if key.is_empty() {
        bail!("--label {arg} has an empty key");
    }
    Ok((key.to_string(), value.trim().to_string()))
}

/// Removes the node from the live roster and, if the config still declares
/// it, from the config too. Mirrors `DELETE /api/nodes/{name}`: the config
/// edit is validated before the roster is touched, so a rejected removal
/// leaves everything untouched rather than tombstoning the node in raft with
/// no way to also drop it from the config.
pub async fn remove(cfg: &ClusterConfig, name: &str, via: Option<&str>) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;

    let mut config = match &state.cluster_config {
        Some(entry) => {
            ClusterConfig::parse(&entry.yaml).context("parsing the cluster's live config")?
        }
        None => cfg.clone(),
    };
    let expect_generation = state.cluster_config.map(|c| c.generation).unwrap_or(0);

    let had_entry = config.nodes.remove(name).is_some();
    if had_entry {
        config.validate().context("config without this node")?;
    }
    debug!(name, had_entry, "submitting RemoveNode");

    let response = client
        .submit(Request::RemoveNode { node: name.to_string() })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected the removal: {error}");
    }

    if had_entry {
        let yaml = serde_yaml::to_string(&config).context("re-encoding the config")?;
        debug!(expect_generation, "submitting PutClusterConfig to drop the node from the config");
        let response = client
            .submit(Request::PutClusterConfig { yaml, expect_generation })
            .await
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(error) = response.error {
            bail!(
                "removed {name} from the live roster, but could not drop it from the config: {error}. \
                 Remove it by hand and run `clouduler config reload`"
            );
        }
    }

    println!("removed {name}");
    Ok(())
}

/// Disaster recovery for a cluster that has lost quorum for good: force
/// `name`'s own local raft state to make it the cluster's sole voter, and let
/// only `stack` come back up, trusting this node's own local data for it.
///
/// Unlike every other command in this module, this never talks to a running
/// daemon - it edits `name`'s own data directory directly, so its daemon
/// must be stopped first, and `via` does not apply. See `cl_raft::node::emergency`
/// for what actually changes.
pub async fn emergency(
    cfg: &ClusterConfig,
    name: &str,
    stack: &str,
    data_dir_override: Option<PathBuf>,
    yes: bool,
) -> Result<()> {
    let node = cfg
        .node(name)
        .ok_or_else(|| anyhow!("node {name} is not declared in the config"))?;
    let rpc_addr = cfg
        .rpc_addr(node)
        .ok_or_else(|| anyhow!("node {name} has no address on the internal network"))?;
    let data_dir = data_dir_override
        .unwrap_or_else(|| node.data_dir(cfg).to_path_buf())
        .join("raft");

    // Read `stack`'s own spec straight out of this node's own local raft
    // state, before rewriting anything, to work out which volumes it uses.
    // Scoped so the store is closed again before `cl_raft::node::emergency`
    // reopens the same file for real.
    let spec = {
        let store = cl_raft::store::Store::open(data_dir.join("raft.redb")).map_err(|e| anyhow!("{e}"))?;
        let shared = store.shared_state();
        let guard = shared.read().await;
        guard
            .data
            .stacks
            .get(stack)
            .map(|entry| entry.spec.clone())
            .ok_or_else(|| {
                anyhow!(
                    "{stack} is not a stack this node's own local state knows about - check the \
                     name, or deploy it once quorum is otherwise recovered"
                )
            })?
    };
    let file = cl_config::StackFile::parse(&spec).map_err(|e| anyhow!("{e}"))?;
    let mut stack_volumes = std::collections::BTreeSet::new();
    for service in file.services.values() {
        for mount in &service.volumes {
            if let (Some(drive), Some(volume)) = (mount.drive.as_ref(), mount.volume.as_ref()) {
                stack_volumes.insert(cl_raft::ids::VolumeId::new(drive, volume));
            }
        }
    }

    let mut prompt = Prompt::new(std::io::stdin().lock(), std::io::stdout(), !yes);
    prompt.say(&format!(
        "this rewrites the raft log and state machine at {} directly, bypassing consensus, so \
         that {name} becomes the cluster's sole voter.",
        data_dir.join("raft.redb").display()
    ))?;
    prompt.say(
        "only do this once every other manager's raft state is confirmed lost for good: a \
         manager that comes back with its old state intact would now conflict with this node. \
         make sure this node's own daemon is stopped before continuing.",
    )?;
    prompt.say(&format!(
        "only {stack} is allowed to start once the daemon comes back up; every other stack's \
         own desired state is left exactly as it was and picks back up on its own, no restore \
         step needed, the moment another manager is reachable again and emergency mode ends. \
         every node but {name} is marked unreachable, so any served drive {stack} needs treats \
         {name} as the active server, and {name}'s own copy of every volume {stack} uses \
         ({}) is trusted as current, whether or not anything else agrees.",
        if stack_volumes.is_empty() {
            "none".to_string()
        } else {
            stack_volumes.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")
        }
    ))?;
    if !prompt.confirm(&format!("force {name} to be the cluster's sole voter and bring up {stack} on it?"), yes)? {
        bail!("aborted");
    }

    cl_raft::node::emergency(&data_dir, node.id, rpc_addr, name, stack, &stack_volumes)
        .await
        .map_err(|e| anyhow!("{e}"))?;

    println!(
        "{name} is now the cluster's sole voter, in emergency mode for {stack}. Start its \
         daemon; {stack} comes up normally, every other stack stays held off. Once another \
         manager is reachable again - promoted back to a voter on its own, the same as after \
         `clouduler node remove`'s opposite case, once its daemon is reachable - emergency mode \
         ends automatically and every other stack resumes on its own. If a lost manager is never \
         coming back, `clouduler node remove` it instead."
    );
    Ok(())
}

pub async fn set_availability(
    cfg: &ClusterConfig,
    name: &str,
    availability: AvailabilityArg,
    via: Option<&str>,
) -> Result<()> {
    let availability = match availability {
        AvailabilityArg::Active => Availability::Active,
        AvailabilityArg::Drain => Availability::Drain,
    };

    debug!(name, ?availability, "submitting SetAvailability");
    let response = client_for(cfg, via)?
        .submit(Request::SetAvailability {
            node: name.to_string(),
            availability,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected the change: {error}");
    }

    println!("{name} is now {availability:?}");
    Ok(())
}
