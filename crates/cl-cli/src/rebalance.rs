//! `clouduler rebalance`: relocate instances to smooth load across nodes.
//! Plans against this node's own copy of the cluster config, like `stack deploy`, and
//! weighs live node and container usage asked of a daemon (falling back to declared limits).

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::{ClusterConfig, StackFile};
use cl_raft::ids::InstanceId;
use cl_raft::state::{ClusterState, Request};
use cl_sched::rebalance::LiveUsage;
use cl_secret::ClusterKey;
use tracing::warn;

use cl_init::prompt::Prompt;
use cl_raft::client::client_for;

pub async fn run(cfg: &ClusterConfig, via: Option<&str>, yes: bool) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
    let desired =
        cl_sched::desired_instances(&state, &cfg.egress_networks(), &cfg.port_gated_networks());
    let served_drives = cfg.served_drives();

    let live = match fetch_live_usage(cfg, via).await {
        Ok(live) => live,
        Err(e) => {
            warn!(error = %format!("{e:#}"), "no live usage, planning from declared memory limits");
            LiveUsage::default()
        }
    };
    let planned = cl_sched::rebalance::plan(
        &state,
        &desired,
        &served_drives,
        &cfg.everywhere_networks(),
        &live,
    );
    if planned.is_empty() {
        println!("already balanced, nothing to move");
        return Ok(());
    }

    println!("{} move(s) planned:", planned.len());
    for m in &planned {
        println!("  {} : {} -> {}", m.instance, m.from, m.to);
    }

    let mut prompt = Prompt::new(std::io::stdin().lock(), std::io::stdout(), !yes);
    if !prompt.confirm("submit these moves?", yes)? {
        bail!("aborted");
    }

    for m in planned {
        let drives = required_drives(&state, &m.instance);
        let response = client
            .submit(Request::BeginMigration {
                instance: m.instance.clone(),
                to: m.to.clone(),
                drives,
            })
            .await
            .map_err(|e| anyhow!("{e}"))?;
        match response.error {
            None => println!("moving {} to {}", m.instance, m.to),
            Some(error) => warn!(instance = %m.instance, to = %m.to, %error, "move was rejected"),
        }
    }
    Ok(())
}

/// Asks one daemon (signed with the cluster key) for every reachable node's live usage.
async fn fetch_live_usage(cfg: &ClusterConfig, via: Option<&str>) -> Result<LiveUsage> {
    let node = crate::update_cli::pick_node(cfg, via)?;
    let base = cl_update::peers::base_url(cfg, node)
        .ok_or_else(|| anyhow!("node {node} has no address on the internal network"))?;
    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "loading the cluster key from {}",
            cfg.secrets.key_file.display()
        )
    })?;
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("building the http client")?;
    let path = "/api/instances/internal/cluster-live-usage";
    let resp = cl_update::internal_client::authenticate(
        http.get(format!("{base}{path}")),
        &key,
        "GET",
        path,
        b"",
    )
    .send()
    .await
    .with_context(|| format!("asking {node}"))?;
    if !resp.status().is_success() {
        bail!("{node} answered {}", resp.status());
    }
    resp.json()
        .await
        .context("decoding the daemon's live usage")
}

/// Mirrors `cl-api`'s `required_drives`: an unknown stack or service yields an
/// empty set (no drive requirements); a stale plan fails on the missing assignment.
fn required_drives(state: &ClusterState, instance: &InstanceId) -> BTreeSet<String> {
    state
        .stacks
        .get(&instance.stack)
        .and_then(|stack| StackFile::parse(&stack.spec).ok())
        .and_then(|file| {
            file.services
                .get(&instance.service)
                .map(cl_config::Service::drives_used)
        })
        .unwrap_or_default()
}
