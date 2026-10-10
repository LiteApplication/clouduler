//! Volume freeze tools: pause a volume's replication so it can be edited in
//! place, then resume and push it.

use std::time::Duration;

use cl_raft::state::DesiredState;
use cl_raft::{InstanceId, Request, VolumeId};
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use super::{Ctx, ToolResult};

const MAX_WAIT_SECS: u64 = 600;
const DEFAULT_WAIT_SECS: u64 = 120;
const POLL_EVERY: Duration = Duration::from_secs(2);

#[derive(Deserialize, JsonSchema)]
pub struct FreezeVolumeArgs {
    /// Volume as `drive/volume`.
    pub volume: String,
    /// Node whose copy will be edited. Defaults to the volume's primary.
    #[serde(default)]
    pub node: Option<String>,
    /// Stop containers using the volume on other nodes. Without it the call
    /// fails if there are any.
    #[serde(default)]
    pub force: bool,
    /// How long to wait for those containers to stop (default 120, max 600).
    #[serde(default)]
    pub wait_seconds: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct UnfreezeVolumeArgs {
    /// Volume as `drive/volume`.
    pub volume: String,
    /// Start the containers the freeze stopped once every node has the
    /// edited copy. Default true.
    #[serde(default = "yes")]
    pub start_containers: bool,
    /// How long to wait for the other nodes to receive the volume (default
    /// 120, max 600). Containers are only started once they have it.
    #[serde(default)]
    pub wait_seconds: Option<u64>,
}

fn yes() -> bool {
    true
}

fn parse_volume(raw: &str) -> Result<VolumeId, String> {
    raw.parse::<VolumeId>()
        .map_err(|_| format!("{raw:?} is not a volume id; ids look like drive/volume"))
}

async fn submit(ctx: &Ctx, request: Request) -> Result<(), String> {
    let response = ctx
        .state
        .cluster
        .submit(request)
        .await
        .map_err(|e| e.to_string())?;
    response.error.map_or(Ok(()), Err)
}

async fn set_desired(
    ctx: &Ctx,
    instances: &[InstanceId],
    desired: DesiredState,
) -> Result<(), String> {
    for instance in instances {
        submit(
            ctx,
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

pub(super) async fn freeze_volume(ctx: &Ctx, args: FreezeVolumeArgs) -> ToolResult {
    let volume = parse_volume(&args.volume)?;
    let state = ctx.state.cluster.state().await;
    let record = state
        .volumes
        .get(&volume)
        .ok_or_else(|| format!("unknown volume {volume}"))?;
    if let Some(frozen) = &record.frozen {
        return Err(format!("{volume} is already frozen on {}", frozen.node));
    }
    if ctx
        .state
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
    let node = args.node.unwrap_or_else(|| record.primary.clone());

    let others = state.running_elsewhere(&node, &volume);
    if !others.is_empty() && !args.force {
        let names: Vec<String> = others.iter().map(|i| i.to_string()).collect();
        return Err(format!(
            "{volume} is in use on nodes other than {node} ({}); pass force=true to stop them",
            names.join(", ")
        ));
    }

    // Freeze first so no sync starts while the other containers wind down.
    submit(
        ctx,
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
            set_desired(ctx, &others, DesiredState::Stopped).await?;
            wait_stopped(ctx, &others, args.wait_seconds).await
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
    ctx: &Ctx,
    instances: &[InstanceId],
    wait_seconds: Option<u64>,
) -> Result<(), String> {
    let wait = Duration::from_secs(wait_seconds.unwrap_or(DEFAULT_WAIT_SECS).min(MAX_WAIT_SECS));
    let started = tokio::time::Instant::now();
    loop {
        let state = ctx.state.cluster.state().await;
        let running: Vec<String> = instances
            .iter()
            .filter(|i| state.container_status.get(*i).is_none_or(|s| s.running))
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

pub(super) async fn unfreeze_volume(ctx: &Ctx, args: UnfreezeVolumeArgs) -> ToolResult {
    let volume = parse_volume(&args.volume)?;
    let state = ctx.state.cluster.state().await;
    let frozen = state
        .volumes
        .get(&volume)
        .ok_or_else(|| format!("unknown volume {volume}"))?
        .frozen
        .clone()
        .ok_or_else(|| format!("{volume} is not frozen"))?;

    submit(ctx, Request::UnfreezeVolume { id: volume.clone() })
        .await
        .map_err(|e| format!("unfreezing {volume}: {e}"))?;

    // The source node pushes at its next sync tick; containers only start
    // once every node holds the edited copy, or they would run on stale data
    // that the push then overwrites under them.
    let wait = Duration::from_secs(
        args.wait_seconds
            .unwrap_or(DEFAULT_WAIT_SECS)
            .min(MAX_WAIT_SECS),
    );
    let started = tokio::time::Instant::now();
    let behind = loop {
        let behind = ctx.state.cluster.state().await.replicas_behind(&volume);
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
    if args.start_containers && !frozen.stopped.is_empty() {
        set_desired(ctx, &frozen.stopped, DesiredState::Running).await?;
        return Ok(format!(
            "{volume} is unfrozen and on every node; started {} container(s)",
            frozen.stopped.len()
        ));
    }
    Ok(format!("{volume} is unfrozen and on every node"))
}
