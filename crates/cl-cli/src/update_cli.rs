//! `clouduler update`: trigger the pull-and-recreate the web ui's Updates page runs.
//! Uses the node-to-node channel `git-backup pull` uses (HMAC over the cluster key);
//! it is an imperative action, not a raft state change.

use anyhow::{anyhow, Context, Result};
use cl_config::ClusterConfig;
use cl_secret::ClusterKey;
use cl_update::internal_client;
use serde::{Deserialize, Serialize};

const APPLY_PATH: &str = "/api/updates/internal/apply";

pub(crate) fn pick_node<'a>(cfg: &'a ClusterConfig, via: Option<&'a str>) -> Result<&'a str> {
    match via {
        Some(name) => {
            cfg.node(name)
                .ok_or_else(|| anyhow!("no node named {name} in the config"))?;
            Ok(name)
        }
        None => cfg
            .managers()
            .next()
            .map(|(name, _)| name.as_str())
            .ok_or_else(|| anyhow!("config has no manager to talk to")),
    }
}

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
    let node = pick_node(cfg, via)?;
    let base = cl_update::peers::base_url(cfg, node)
        .ok_or_else(|| anyhow!("node {node} has no address on the internal network"))?;
    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "loading the cluster key from {} to authenticate to {node}",
            cfg.secrets.key_file.display()
        )
    })?;
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("building the http client")?;

    // Serialized by hand so the exact bytes sent are the bytes the signature
    // commits to.
    let body = serde_json::to_vec(&InstanceSelection { instances })
        .context("encoding the update selection")?;
    let resp = internal_client::authenticate(
        http.post(format!("{base}{APPLY_PATH}")),
        &key,
        "POST",
        APPLY_PATH,
        &body,
    )
    .header(reqwest::header::CONTENT_TYPE, "application/json")
    .body(body)
    .send()
    .await
    .with_context(|| format!("asking {node}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("{node} answered {status}: {body}");
    }

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
