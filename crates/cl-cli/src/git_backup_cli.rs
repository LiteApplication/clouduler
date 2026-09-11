//! `clouduler git-backup` commands: manual controls for the git <-> cluster
//! mirror (see `docs/git-backup.md`) that fall outside the schedule-driven
//! export loop and the webhook-driven pull - triggering a pull immediately.
//!
//! Goes over the same node-to-node https channel `clouduler logs`/`exec` use:
//! the normal api, authenticated by an HMAC signature over the shared cluster
//! key rather than a session or api key, since the cli already needs that key
//! on disk for other commands and there is no reason to also ask for the
//! separate webhook token.

use anyhow::{anyhow, Context, Result};
use cl_config::ClusterConfig;
use cl_secret::ClusterKey;
use cl_update::internal_client::AUTH_HEADER;

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

async fn trigger(cfg: &ClusterConfig, via: Option<&str>, path: &str) -> Result<()> {
    let node = pick_node(cfg, via)?;
    let base = cl_update::peers::base_url(cfg, node)
        .ok_or_else(|| anyhow!("node {node} has no address on the internal network"))?;
    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "loading the cluster key from {} to authenticate to {node}",
            cfg.secrets.key_file.display()
        )
    })?;
    let signature = cl_secret::internal_auth::sign(&key, path);
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("building the http client")?;

    let resp = http
        .post(format!("{base}{path}"))
        .header(AUTH_HEADER, signature)
        .send()
        .await
        .with_context(|| format!("asking {node}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("{node} answered {status}: {body}");
    }
    Ok(())
}

pub async fn pull(cfg: &ClusterConfig, via: Option<&str>) -> Result<()> {
    trigger(cfg, via, "/api/git-backup/pull").await?;
    println!("triggered a git backup pull");
    Ok(())
}

pub async fn reconcile(cfg: &ClusterConfig, via: Option<&str>) -> Result<()> {
    trigger(cfg, via, "/api/git-backup/reconcile").await?;
    println!("triggered a git backup reconcile");
    Ok(())
}
