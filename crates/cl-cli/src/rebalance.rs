//! `clouduler rebalance` — actively relocate instances to smooth load across
//! nodes, on request. The cli counterpart to the ui's rebalance button
//! (`POST /api/rebalance` in cl-api); this one talks over the raft rpc
//! channel directly, the same as every other `clouduler` command, computing
//! the plan against this node's own copy of the cluster config rather than
//! the replicated one - the same trade `stack deploy` and `stack unhealthy`
//! already make (see `health::pending_rows`).

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Result};
use cl_config::{ClusterConfig, StackFile};
use cl_raft::ids::InstanceId;
use cl_raft::state::{ClusterState, Request};
use tracing::warn;

use cl_init::prompt::Prompt;
use cl_raft::client::client_for;

pub async fn run(cfg: &ClusterConfig, via: Option<&str>, yes: bool) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;

    let colocate_proxy_hosts = cfg.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
    let desired = cl_sched::desired_instances(
        &state,
        &cfg.egress_networks(),
        &cfg.port_gated_networks(),
        colocate_proxy_hosts,
    );
    let served_drives = cfg.served_drives();

    let planned = cl_sched::rebalance::plan(&state, &desired, &served_drives, &cfg.nat_networks());
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

/// Drives the instance's service mounts volumes from. Mirrors `cl-api`'s
/// `required_drives`: an unknown stack or service yields an empty set, which
/// reads as "no drive requirements" - the right answer for a stack with no
/// volumes, and harmless for one removed since this plan was computed, where
/// the move fails on the missing assignment instead.
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
