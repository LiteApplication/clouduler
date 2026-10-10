//! `clouduler volume freeze` / `unfreeze`: pause a volume's replication so it
//! can be edited by hand on this node without a sync from another node
//! overwriting (or deleting) the edits.
//!
//! Both are CLI-invoked and one-shot, run on the node whose copy is edited,
//! like the restores in `backup_restore`. Freezing records the node and the
//! instances it stopped in raft (`VolumeState.frozen`), which is what makes
//! every node's sync scheduler skip the volume. Unfreezing clears that, bumps
//! the generation with the frozen node as primary, and pushes the edited
//! copy to every other holder at the next sync tick instead of waiting for the
//! volume's sync interval, and only then starts the containers it stopped.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use cl_config::ClusterConfig;
use cl_drive::DriveRegistry;
use cl_raft::client::{client_for, Client};
use cl_raft::ids::VolumeId;
use cl_raft::state::{DesiredState, Request};

use crate::backup_restore::{max_stop_grace, set_desired, wait_for_stopped};

/// How long unfreeze waits for the other nodes to pull the pushed copy before
/// giving up and leaving the stopped containers stopped.
const CATCH_UP_TIMEOUT: Duration = Duration::from_secs(3600);

const STOP_POLL_MARGIN: Duration = Duration::from_secs(15);

async fn submit(client: &Client, request: Request) -> Result<()> {
    let response = client.submit(request).await.map_err(|e| anyhow!("{e}"))?;
    match response.error {
        Some(error) => bail!("{error}"),
        None => Ok(()),
    }
}

pub async fn freeze(
    cfg: &ClusterConfig,
    node_name: &str,
    volume: &VolumeId,
    force: bool,
    via: Option<&str>,
) -> Result<()> {
    let registry = DriveRegistry::new(cfg, node_name)
        .map_err(|e| anyhow!("node {node_name} is not in the cluster config: {e}"))?;
    if !registry.implements(&volume.drive) {
        bail!("node {node_name} does not implement drive {}", volume.drive);
    }
    if registry.is_served(&volume.drive) {
        bail!(
            "drive {} is served, not replicated: there is no replication to pause",
            volume.drive
        );
    }
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
    let record = state
        .volumes
        .get(volume)
        .ok_or_else(|| anyhow!("unknown volume {volume}"))?;
    if let Some(frozen) = &record.frozen {
        bail!("{volume} is already frozen on {}", frozen.node);
    }

    let others = state.running_elsewhere(node_name, volume);
    if !others.is_empty() && !force {
        bail!(
            "{volume} is in use on other nodes ({}); pass --force to stop them",
            others
                .iter()
                .map(|i| i.container_name())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Freeze first so no sync is started while the other containers wind down.
    submit(
        &client,
        Request::FreezeVolume {
            id: volume.clone(),
            node: node_name.to_string(),
            stopped: others.clone(),
        },
    )
    .await
    .map_err(|e| anyhow!("freezing {volume}: {e}"))?;

    if !others.is_empty() {
        println!("stopping {} container(s) on other nodes", others.len());
        let stopped = async {
            set_desired(&client, &others, DesiredState::Stopped).await?;
            let deadline = max_stop_grace(&state, &others) + STOP_POLL_MARGIN;
            wait_for_stopped(&client, node_name, &others, deadline).await
        }
        .await;
        if let Err(e) = stopped {
            return Err(e.context(format!(
                "{volume} is frozen, but stopping its containers on other nodes failed; \
                 `clouduler volume unfreeze {volume}` undoes it"
            )));
        }
    }
    println!("{volume} is frozen: replication is paused, edit it on {node_name}");
    Ok(())
}

pub async fn unfreeze(
    cfg: &ClusterConfig,
    node_name: &str,
    volume: &VolumeId,
    start: bool,
    via: Option<&str>,
) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
    let frozen = state
        .volumes
        .get(volume)
        .ok_or_else(|| anyhow!("unknown volume {volume}"))?
        .frozen
        .clone()
        .ok_or_else(|| anyhow!("{volume} is not frozen"))?;
    if frozen.node != node_name {
        bail!(
            "{volume} was frozen on {0}, whose copy holds the edits; run unfreeze there",
            frozen.node
        );
    }

    submit(&client, Request::UnfreezeVolume { id: volume.clone() })
        .await
        .map_err(|e| anyhow!("unfreezing {volume}: {e}"))?;

    // The volume's source node pushes at its next sync tick (see
    // `cl_drive::sync`); starting the stopped containers before the others
    // hold the edited copy would run them on stale data that the push then
    // overwrites under them.
    println!("{volume} is unfrozen; waiting for the other nodes to receive it");
    let start_at = tokio::time::Instant::now();
    let behind = loop {
        let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
        let behind = state.replicas_behind(volume);
        if behind.is_empty() || start_at.elapsed() >= CATCH_UP_TIMEOUT {
            break behind;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    if !behind.is_empty() {
        println!(
            "{} had not received {volume} after {}s; {} container(s) were left stopped",
            behind.join(", "),
            CATCH_UP_TIMEOUT.as_secs(),
            frozen.stopped.len()
        );
        return Ok(());
    }
    if start && !frozen.stopped.is_empty() {
        println!("starting {} container(s)", frozen.stopped.len());
        set_desired(&client, &frozen.stopped, DesiredState::Running).await?;
    }
    println!("{volume} is pushed to every node");
    Ok(())
}
