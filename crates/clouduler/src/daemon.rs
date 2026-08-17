//! Daemon bootstrap. Subsystems get wired in here as later phases land.

use anyhow::{bail, Result};
use cl_config::ClusterConfig;
use tracing::info;

pub async fn run(cfg: ClusterConfig, node_name: String) -> Result<()> {
    let Some(node) = cfg.node(&node_name) else {
        bail!(
            "node {node_name} is not declared in the config (known: {})",
            cfg.nodes.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    };

    info!(
        cluster = %cfg.cluster.name,
        node = %node_name,
        role = %node.role,
        rpc = %node.rpc_addr(),
        drives = node.drives.len(),
        "starting"
    );

    // Raft, scheduler, proxy and api start here once implemented.
    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    Ok(())
}
