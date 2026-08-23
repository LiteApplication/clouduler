//! The `clouduler node` commands that change live cluster state. These talk
//! to a running daemon over the node RPC channel, the same way the `stack`
//! commands do, and mirror what the web ui's `/api/nodes` routes do to the
//! replicated roster and the config file.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_raft::state::{Availability, NodeState, Request, Role};

use crate::cli::AvailabilityArg;
use crate::init::plan::parse_drive_arg;
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
    let response = client
        .submit(Request::UpsertNode(NodeState {
            name: name.clone(),
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

    let response = client
        .submit(Request::RemoveNode { node: name.to_string() })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected the removal: {error}");
    }

    if had_entry {
        let yaml = serde_yaml::to_string(&config).context("re-encoding the config")?;
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
