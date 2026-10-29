//! Freezing and unfreezing replicated volumes, shared by the REST routes and
//! the MCP tools. Freezing pauses replication so one node's copy can be
//! edited in place; unfreezing makes that copy the primary and pushes it.

use std::time::Duration;

use cl_raft::state::{DesiredState, FrozenVolume};
use cl_raft::{InstanceId, Request, VolumeId};

use crate::server::AppState;

pub const MAX_WAIT_SECS: u64 = 600;
pub const DEFAULT_WAIT_SECS: u64 = 120;
const POLL_EVERY: Duration = Duration::from_secs(2);

pub fn parse_volume(raw: &str) -> Result<VolumeId, String> {
    raw.parse::<VolumeId>()
        .map_err(|_| format!("{raw:?} is not a volume id; ids look like drive/volume"))
}

/// A wait given in seconds, clamped to `MAX_WAIT_SECS`.
pub fn wait_of(seconds: Option<u64>) -> Duration {
    Duration::from_secs(seconds.unwrap_or(DEFAULT_WAIT_SECS).min(MAX_WAIT_SECS))
}

async fn submit(state: &AppState, request: Request) -> Result<(), String> {
    let response = state
        .cluster
        .submit(request)
        .await
        .map_err(|e| e.to_string())?;
    response.error.map_or(Ok(()), Err)
}

async fn set_desired(
    state: &AppState,
    instances: &[InstanceId],
    desired: DesiredState,
) -> Result<(), String> {
    for instance in instances {
        submit(
            state,
            Request::SetDesiredState {
                instance: instance.clone(),
                desired,
            },
        )
        .await
        .map_err(|e| format!("could not update the desired state of {instance}: {e}"))?;
    }
    Ok(())
}

/// Freeze `volume` for editing on `node` (default: its primary). Containers
/// using it on other nodes make this fail unless `force` stops them.
/// With `wait` of `None` the stop is requested but not waited for, so the
/// caller returns at once and watches `container_status` itself.
pub async fn freeze(
    state: &AppState,
    volume: &VolumeId,
    node: Option<String>,
    force: bool,
    wait: Option<Duration>,
) -> Result<String, String> {
    let cluster_state = state.cluster.state().await;
    let record = cluster_state
        .volumes
        .get(volume)
        .ok_or_else(|| format!("unknown volume {volume}"))?;
    if let Some(frozen) = &record.frozen {
        return Err(format!("{volume} is already frozen on {}", frozen.node));
    }
    if state
        .cluster_config
        .read()
        .await
        .served_drives()
        .contains(&volume.drive)
    {
        return Err(format!(
            "drive {} is served, not replicated: there is no replication to pause",
            volume.drive
        ));
    }
    let node = node.unwrap_or_else(|| record.primary.clone());

    let others = cluster_state.running_elsewhere(&node, volume);
    if !others.is_empty() && !force {
        let names: Vec<String> = others.iter().map(|i| i.to_string()).collect();
        return Err(format!(
            "{volume} is in use on nodes other than {node} ({}); pass force=true to stop them",
            names.join(", ")
        ));
    }

    // Freeze first so no sync starts while the other containers wind down.
    submit(
        state,
        Request::FreezeVolume {
            id: volume.clone(),
            node: node.clone(),
            stopped: others.clone(),
        },
    )
    .await
    .map_err(|e| format!("freezing {volume}: {e}"))?;

    if !others.is_empty() {
        let stopped = async {
            set_desired(state, &others, DesiredState::Stopped).await?;
            match wait {
                Some(wait) => wait_stopped(state, &others, wait).await,
                None => Ok(()),
            }
        }
        .await;
        if let Err(e) = stopped {
            return Err(format!(
                "{volume} is frozen, but stopping its containers on other nodes failed: {e}. \
                 unfreeze_volume undoes the freeze."
            ));
        }
    }
    Ok(format!(
        "{volume} is frozen: replication is paused and {} container(s) on other nodes are \
         stopped. Edit the copy on {node}, then call unfreeze_volume.",
        others.len()
    ))
}

async fn wait_stopped(
    state: &AppState,
    instances: &[InstanceId],
    wait: Duration,
) -> Result<(), String> {
    let started = tokio::time::Instant::now();
    loop {
        let cluster_state = state.cluster.state().await;
        let running: Vec<String> = instances
            .iter()
            .filter(|i| {
                cluster_state
                    .container_status
                    .get(*i)
                    .is_none_or(|s| s.running)
            })
            .map(|i| i.to_string())
            .collect();
        if running.is_empty() {
            return Ok(());
        }
        if started.elapsed() >= wait {
            return Err(format!(
                "{} still running after {}s",
                running.join(", "),
                wait.as_secs()
            ));
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// First half of an unfreeze: check the volume is frozen and lift the freeze.
/// The source node then pushes at its next sync tick. Returns what the freeze
/// had stopped, for `finish_unfreeze`.
pub async fn begin_unfreeze(state: &AppState, volume: &VolumeId) -> Result<FrozenVolume, String> {
    let cluster_state = state.cluster.state().await;
    let frozen = cluster_state
        .volumes
        .get(volume)
        .ok_or_else(|| format!("unknown volume {volume}"))?
        .frozen
        .clone()
        .ok_or_else(|| format!("{volume} is not frozen"))?;
    submit(state, Request::UnfreezeVolume { id: volume.clone() })
        .await
        .map_err(|e| format!("unfreezing {volume}: {e}"))?;
    Ok(frozen)
}

/// Second half: wait for every node to hold the edited copy, then start the
/// containers the freeze stopped. They only start once every node has it, or
/// they would run on stale data that the push then overwrites under them.
pub async fn finish_unfreeze(
    state: &AppState,
    volume: &VolumeId,
    frozen: &FrozenVolume,
    start_containers: bool,
    wait: Duration,
) -> Result<String, String> {
    let started = tokio::time::Instant::now();
    let behind = loop {
        let behind = state.cluster.state().await.replicas_behind(volume);
        if behind.is_empty() || started.elapsed() >= wait {
            break behind;
        }
        tokio::time::sleep(POLL_EVERY).await;
    };
    if !behind.is_empty() {
        return Ok(format!(
            "{volume} is unfrozen and being pushed from {}, but {} had not received it after {}s. \
             {} container(s) were left stopped; check the volume's replicas and start them with \
             restart_instance once it has synced.",
            frozen.node,
            behind.join(", "),
            wait.as_secs(),
            frozen.stopped.len()
        ));
    }
    if start_containers && !frozen.stopped.is_empty() {
        set_desired(state, &frozen.stopped, DesiredState::Running).await?;
        return Ok(format!(
            "{volume} is unfrozen and on every node; started {} container(s)",
            frozen.stopped.len()
        ));
    }
    Ok(format!("{volume} is unfrozen and on every node"))
}

/// Unfreeze and wait for the push to land.
pub async fn unfreeze(
    state: &AppState,
    volume: &VolumeId,
    start_containers: bool,
    wait: Duration,
) -> Result<String, String> {
    let frozen = begin_unfreeze(state, volume).await?;
    finish_unfreeze(state, volume, &frozen, start_containers, wait).await
}
