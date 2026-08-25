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
/// `name`'s own local raft state to make it the cluster's sole voter.
///
/// Unlike every other command in this module, this never talks to a running
/// daemon - it edits `name`'s own data directory directly, so its daemon
/// must be stopped first, and `via` does not apply. See
/// `cl_raft::node::force_quorum` for what actually changes.
pub async fn force_quorum(
    cfg: &ClusterConfig,
    name: &str,
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
    if !prompt.confirm(&format!("force {name} to be the cluster's sole voter?"), yes)? {
        bail!("aborted");
    }

    cl_raft::node::force_quorum(&data_dir, node.id, rpc_addr)
        .await
        .map_err(|e| anyhow!("{e}"))?;

    println!(
        "{name} is now the cluster's sole voter. Start its daemon; once it is leading again, \
         any other manager still known to it is promoted back to a voter on its own as soon as \
         it is reachable - nothing to run there by hand unless its local state is gone too, in \
         which case it needs `clouduler init --join`. If a lost manager is never coming back, \
         `clouduler node remove` it instead."
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
