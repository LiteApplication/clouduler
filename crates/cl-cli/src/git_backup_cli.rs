//! `clouduler git-backup` commands: manual controls for the git mirror (see
//! `docs/git-backup.md`), e.g. triggering a pull immediately. Uses the same HMAC
//! node-to-node channel as `clouduler logs`/`exec`.

use anyhow::{anyhow, Context, Result};
use cl_config::ClusterConfig;
use cl_secret::ClusterKey;
use cl_update::internal_client;

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
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("building the http client")?;

    let resp =
        internal_client::authenticate(http.post(format!("{base}{path}")), &key, "POST", path, b"")
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

async fn call(
    cfg: &ClusterConfig,
    via: Option<&str>,
    method: reqwest::Method,
    path: &str,
) -> Result<String> {
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
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("building the http client")?;
    let resp = internal_client::authenticate(
        http.request(method.clone(), format!("{base}{path}")),
        &key,
        method.as_str(),
        path,
        b"",
    )
    .send()
    .await
    .with_context(|| format!("asking {node}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let reason = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or(body);
        anyhow::bail!("{node} answered {status}: {reason}");
    }
    Ok(body)
}

/// `git-backup commits`: the newest commits of the tracked branch.
pub async fn commits(cfg: &ClusterConfig, via: Option<&str>) -> Result<()> {
    let body = call(
        cfg,
        via,
        reqwest::Method::GET,
        "/api/git-backup/internal-commits",
    )
    .await?;
    let list: Vec<serde_json::Value> =
        serde_json::from_str(&body).context("decoding the commit list")?;
    for c in list {
        println!(
            "{}  {}  {}  {}",
            c["short"].as_str().unwrap_or_default(),
            c["time"].as_u64().unwrap_or_default(),
            c["author"].as_str().unwrap_or_default(),
            c["message"].as_str().unwrap_or_default()
        );
    }
    Ok(())
}

/// `git-backup rollback <commit>`: a new commit restoring that commit's configuration, applied
/// through the normal pull. Asks for confirmation unless `yes`.
pub async fn rollback(
    cfg: &ClusterConfig,
    via: Option<&str>,
    commit: &str,
    yes: bool,
) -> Result<()> {
    if !yes {
        eprintln!(
            "This rolls the cluster configuration (cluster.yaml, stacks, configs, terraform modules) back to commit {commit}.\n\
             Stacks that did not exist then are removed and changed ones are redeployed. Secrets and terraform state are NOT rolled back.\n\
             Type 'yes' to continue:"
        );
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading the answer")?;
        if line.trim() != "yes" {
            anyhow::bail!("aborted");
        }
    }
    let query = serde_urlencoded::to_string([("commit", commit)]).unwrap_or_default();
    let path = format!("/api/git-backup/internal-rollback?{query}");
    let body = call(cfg, via, reqwest::Method::POST, &path).await?;
    let new: serde_json::Value = serde_json::from_str(&body).context("decoding the response")?;
    println!(
        "rolled back; new head commit {}",
        new["commit"].as_str().unwrap_or_default()
    );
    Ok(())
}

/// `git-backup reset <commit>`: force the remote branch and every node back to that commit, with
/// a simultaneous restart of the whole cluster. Asks for confirmation unless `yes`.
pub async fn reset(
    cfg: &ClusterConfig,
    via: Option<&str>,
    commit: &str,
    release_tag: Option<&str>,
    keep_current_release: bool,
    yes: bool,
) -> Result<()> {
    if !yes {
        eprintln!(
            "This force-pushes the git branch back to commit {commit} (the current tip is kept on a rolled-back/ branch) and \
             resets the cluster to it WITHOUT the usual pull checks: cluster.yaml, stacks and configs are replaced on every node, \
             the chosen release (by default the one recorded at that commit) is installed everywhere, and ALL nodes restart at the same time (downtime).\n\
             Secrets and terraform state are kept. An older release may refuse the newer raft data.\n\
             Type 'yes' to continue:"
        );
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading the answer")?;
        if line.trim() != "yes" {
            anyhow::bail!("aborted");
        }
    }
    let mut query = vec![("commit", commit)];
    if let Some(tag) = release_tag {
        query.push(("release_tag", tag));
    }
    if keep_current_release {
        query.push(("keep_current_release", "true"));
    }
    let query = serde_urlencoded::to_string(query).unwrap_or_default();
    let path = format!("/api/git-backup/internal-reset?{query}");
    let body = call(cfg, via, reqwest::Method::POST, &path).await?;
    let done: serde_json::Value = serde_json::from_str(&body).context("decoding the response")?;
    println!(
        "reset to {}; the previous tip is on branch {}; release: {} ({}). The nodes download the release, then restart together.",
        done["commit"].as_str().unwrap_or_default(),
        done["branch"].as_str().unwrap_or_default(),
        done["release_tag"].as_str().unwrap_or("(unchanged)"),
        done["release_source"].as_str().unwrap_or("current")
    );
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
