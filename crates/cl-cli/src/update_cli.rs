//! `clouduler update`: manually trigger the same container-image
//! pull-and-recreate the web ui's Updates page runs, without a browser.
//!
//! Goes over the same node-to-node channel `git-backup pull` uses (see
//! `git_backup_cli`): scheduling pulls is imperative action against the
//! running daemon, not a raft state change, so there is nothing for
//! `client.submit` to write, and the internal-auth signature (HMAC over the
//! cluster key) is what the cli already has on disk instead of a separate
//! admin api key.

use anyhow::{anyhow, Context, Result};
use cl_config::ClusterConfig;
use cl_secret::ClusterKey;
use cl_update::internal_client::AUTH_HEADER;
use serde::{Deserialize, Serialize};

const APPLY_PATH: &str = "/api/updates/internal/apply";

fn pick_node<'a>(cfg: &'a ClusterConfig, via: Option<&'a str>) -> Result<&'a str> {
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

/// Trigger a pull-and-recreate for `instances` (each `stack.service.index`),
/// or for every instance with an update available when `instances` is
/// empty — the same default the ui's "update all" button uses.
///
/// Answers as soon as the work is scheduled, not once it is done: a pull can
/// take minutes, so this only reports what was accepted. `clouduler update`
/// again, or the web ui, shows progress.
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
    let signature = cl_secret::internal_auth::sign(&key, APPLY_PATH);
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("building the http client")?;

    let resp = http
        .post(format!("{base}{APPLY_PATH}"))
        .header(AUTH_HEADER, signature)
        .json(&InstanceSelection { instances })
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
