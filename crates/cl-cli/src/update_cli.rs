//! `clouduler update`: trigger the pull-and-recreate the web ui's Updates page runs.
//! Uses the node-to-node channel `git-backup pull` uses (HMAC over the cluster key);
//! it is an imperative action, not a raft state change.

use anyhow::{Context, Result};
use cl_config::ClusterConfig;
use serde::{Deserialize, Serialize};

use crate::node_channel::NodeChannel;

const APPLY_PATH: &str = "/api/updates/internal/apply";

#[derive(Serialize, Default)]
struct InstanceSelection {
    instances: Vec<String>,
}

#[derive(Deserialize)]
struct Accepted {
    accepted: Vec<String>,
}

/// Trigger a pull-and-recreate for `instances` (each `stack.service.index`), or for
/// every instance with an update available when empty. Returns once the work is
/// scheduled, not done.
pub async fn apply(cfg: &ClusterConfig, instances: Vec<String>, via: Option<&str>) -> Result<()> {
    let channel = NodeChannel::open(cfg, via, None)?;

    // Serialized by hand so the exact bytes sent are the bytes the signature
    // commits to.
    let body = serde_json::to_vec(&InstanceSelection { instances })
        .context("encoding the update selection")?;
    let resp = channel
        .send(reqwest::Method::POST, APPLY_PATH, Some(body))
        .await?;
    let resp = channel.ensure_success(resp).await?;

    let accepted: Accepted = resp
        .json()
        .await
        .context("decoding the response from the daemon")?;
    if accepted.accepted.is_empty() {
        println!("nothing to update");
    } else {
        println!("updating {} instance(s):", accepted.accepted.len());
        for id in accepted.accepted {
            println!("  {id}");
        }
    }
    Ok(())
}
