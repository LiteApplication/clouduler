//! Daemon bootstrap. Subsystems get wired in here as later phases land.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use cl_config::{ClusterConfig, FailoverConfig, Node, NodeRole};
use cl_raft::ids::node_id;
use cl_raft::liveness::{Fence, FenceAction};
use cl_raft::node::{ClusterNode, Options};
use cl_raft::state::{Availability, NodeState, Request, Role};
use openraft::BasicNode;
use tracing::{info, warn};

pub async fn run(cfg: ClusterConfig, node_name: String, data_dir: PathBuf) -> Result<()> {
    let Some(node) = cfg.node(&node_name) else {
        bail!(
            "node {node_name} is not declared in the config (known: {})",
            cfg.nodes.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    };

    let rpc_addr = cfg
        .rpc_addr(node)
        .context("node has no address on the internal network")?;

    info!(
        cluster = %cfg.cluster.name,
        node = %node_name,
        role = %node.role,
        rpc = %rpc_addr,
        drives = node.drives.len(),
        "starting"
    );

    let cluster = ClusterNode::start(Options {
        name: node_name.clone(),
        rpc_addr,
        data_dir: data_dir.join("raft"),
        cluster_name: cfg.cluster.name.clone(),
    })
    .await?;

    let cluster = Arc::new(cluster);
    form_cluster(&cfg, &node_name, &cluster).await?;

    let failover = cfg.failover.clone();
    let detector = tokio::spawn(cluster.clone().run_failure_detector(
        failover.probe_interval,
        failover.node_timeout,
    ));
    let fencing = tokio::spawn(run_fence(cluster.clone(), failover.clone()));
    let watcher = tokio::spawn(watch_leadership(cfg, cluster.clone()));

    // Scheduler, overlay, drives, proxy and api start here once implemented.
    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    watcher.abort();
    detector.abort();
    fencing.abort();
    cluster.shutdown().await?;
    Ok(())
}

/// Stop this node's containers once it has been without a quorum for longer
/// than `failover.fence_timeout`.
///
/// This is the other half of the failure handling: the leader will hand our
/// containers to another node after `node_timeout`, and we must have let go of
/// them before that happens, or the same volume ends up with two writers.
async fn run_fence(cluster: Arc<ClusterNode>, failover: FailoverConfig) {
    let quorum = cluster.quorum();
    let mut fence = Fence::new();
    let mut ticker = tokio::time::interval(failover.probe_interval);

    loop {
        ticker.tick().await;
        match fence.observe(quorum.has_quorum(), Instant::now(), failover.fence_timeout) {
            FenceAction::Fence => {
                warn!(
                    timeout = ?failover.fence_timeout,
                    "no quorum, stopping local containers"
                );
                // The reconcile loop consults the fence, so stopping the
                // containers happens there once it is wired up.
            }
            FenceAction::Release => info!("quorum restored, containers may start again"),
            FenceAction::None => {}
        }
    }
}

/// Keep the replicated roster in step with the config for as long as this node
/// leads. Seeding is done on every leadership change rather than once at
/// startup, because at startup no leader has been elected yet.
async fn watch_leadership(cfg: ClusterConfig, cluster: Arc<ClusterNode>) {
    let mut quorum = cluster.quorum();
    loop {
        let status = quorum.status();
        info!(
            quorum = status.has_quorum,
            leader = ?status.leader,
            reachable = status.reachable.len(),
            voters = status.voters,
            "cluster status"
        );

        if status.is_leader(cluster.id) {
            if let Err(e) = seed_roster(&cfg, &cluster).await {
                warn!(error = %e, "could not publish the node roster");
            }
        }

        if !quorum.changed().await {
            return;
        }
    }
}

/// Form the cluster on first boot.
///
/// The first manager by name is the one that forms it. That rule is
/// deterministic, so several managers starting at once cannot each form their
/// own cluster.
async fn form_cluster(cfg: &ClusterConfig, node_name: &str, cluster: &ClusterNode) -> Result<()> {
    let founder = cfg
        .managers()
        .next()
        .map(|(name, _)| name.clone())
        .context("config has no manager")?;

    if node_name != founder || cluster.is_initialized().await? {
        return Ok(());
    }

    let members: BTreeMap<_, _> = cfg
        .managers()
        .filter_map(|(name, node)| {
            Some((node_id(name), BasicNode::new(cfg.rpc_addr(node)?.to_string())))
        })
        .collect();
    info!(members = members.len(), "forming cluster");
    cluster.initialize(members).await?;
    Ok(())
}

/// Publish every configured node into the replicated roster, and attach the
/// workers as learners so they receive state without voting.
async fn seed_roster(cfg: &ClusterConfig, cluster: &ClusterNode) -> Result<()> {
    let known = cluster.state().await;

    for (name, node) in &cfg.nodes {
        let Some(rpc_addr) = cfg.rpc_addr(node) else {
            warn!(node = %name, "skipping node with no internal address");
            continue;
        };

        if node.role == NodeRole::Worker && !known.nodes.contains_key(name) {
            cluster.add_worker(node_id(name), rpc_addr).await?;
        }

        let desired = to_state(name, node, rpc_addr, known.nodes.get(name));
        if known.nodes.get(name) != Some(&desired) {
            let response = cluster.submit(Request::UpsertNode(desired)).await?;
            if let Some(error) = response.error {
                warn!(node = %name, %error, "could not publish node");
            }
        }
    }

    Ok(())
}

/// Build the roster entry from config, keeping any availability the cluster has
/// already set. Draining a node from the ui must not be undone by a restart.
fn to_state(
    name: &str,
    node: &Node,
    rpc_addr: std::net::SocketAddr,
    current: Option<&NodeState>,
) -> NodeState {
    NodeState {
        name: name.to_string(),
        role: match node.role {
            NodeRole::Manager => Role::Manager,
            NodeRole::Worker => Role::Worker,
        },
        rpc_addr,
        drives: node.drives.clone(),
        availability: current
            .map(|c| c.availability)
            .unwrap_or(Availability::Active),
    }
}
