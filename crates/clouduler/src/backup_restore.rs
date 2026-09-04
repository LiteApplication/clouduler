//! `clouduler backup restore-{drive,volume,service}`: restore a restic
//! snapshot back onto a drive's real directory.
//!
//! Always stops the affected containers first - unlike backup, this isn't
//! configurable: restoring under a live container is unsafe, with no
//! acceptable degraded mode. The three `restore_*` functions are CLI-invoked
//! and one-shot, run directly on the node that holds the drive (matching
//! `folder_import`'s convention of operating on the local machine).
//!
//! [`start_restore_watcher`] is the api's path to the same three functions:
//! `cl-api` cannot call into this binary crate directly (wrong dependency
//! direction - it's a library `clouduler` depends on, not the reverse), so an
//! api handler instead submits a `RestoreRequest` through raft and this
//! per-node watcher claims and executes whichever ones it can serve, the same
//! "submit an intent, the node that can act on it does" pattern
//! `backup_runner` uses for scheduled/manual backups.
//!
//! Must run on a node that both implements the target drive (somewhere to
//! write the restored files) and, for a `NodeLocal` destination, is that
//! destination's own node - restic needs a real local path to read the
//! repository from, the same operational requirement backups themselves
//! have (see `cl-restic`'s crate doc comment and `backup_runner`'s module
//! doc comment on the `NodeLocal`-elsewhere handoff).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::{ClusterConfig, StackFile};
use cl_drive::DriveRegistry;
use cl_raft::ids::{InstanceId, VolumeId};
use cl_raft::node::ClusterNode;
use cl_raft::state::{BackupDestination, ClusterState, DesiredState, Request, RestoreScope, RestoreStatus};
use cl_restic::restic::{ResticRepo, SnapshotInfo};
use cl_restic::restore::include_path_for_volume;
use cl_secret::ClusterKey;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::backup_runner::instances_using_drive;
use crate::backup_secrets;
use crate::folder_import::instances_using;
use crate::stack::client_for;

/// How often every node checks for `Pending` restore requests it can serve.
const RESTORE_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Per-node: claims and executes `Pending` `RestoreRequest`s whose drive this
/// node implements - see the module doc comment. Runs on every node, not
/// just the leader, the same as `backup_runner::start_runner`: "can serve
/// this drive" is a local fact each node checks about itself, not a decision
/// that needs a single writer.
pub fn start_restore_watcher(cfg: ClusterConfig, cluster: Arc<ClusterNode>, node_name: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RESTORE_POLL_INTERVAL);
        loop {
            ticker.tick().await;
            let state = cluster.state().await;
            let Some(node) = cfg.node(&node_name) else { continue };

            for request in state.restore_requests.values() {
                if !matches!(request.status, RestoreStatus::Pending) {
                    continue;
                }
                let drive = match &request.scope {
                    RestoreScope::Drive { drive } => drive.clone(),
                    RestoreScope::Volume { volume } => volume.drive.clone(),
                    RestoreScope::Service { stack, service } => {
                        match service_volumes(&state, stack, service) {
                            Ok(volumes) if !volumes.is_empty() => volumes[0].drive.clone(),
                            _ => continue,
                        }
                    }
                };
                if !node.drives.contains_key(&drive) {
                    continue;
                }

                let claim = cluster
                    .submit(Request::SetRestoreStatus {
                        id: request.id.clone(),
                        status: RestoreStatus::Running,
                        at: unix_now(),
                    })
                    .await;
                let Ok(response) = claim else { continue };
                if response.error.is_some() {
                    // Already claimed by another node, or no longer pending.
                    continue;
                }

                let outcome = execute_restore(&cfg, &node_name, request).await;
                let status = match outcome {
                    Ok(()) => RestoreStatus::Succeeded,
                    Err(e) => {
                        warn!(request = %request.id, error = %e, "restore request failed");
                        RestoreStatus::Failed { error: e.to_string() }
                    }
                };
                let _ = cluster
                    .submit(Request::SetRestoreStatus { id: request.id.clone(), status, at: unix_now() })
                    .await;
            }
        }
    })
}

async fn execute_restore(cfg: &ClusterConfig, node_name: &str, request: &cl_raft::state::RestoreRequest) -> Result<()> {
    let snapshot = request.snapshot.as_deref();
    match &request.scope {
        RestoreScope::Drive { drive } => {
            let destination = request
                .destination_id
                .as_deref()
                .ok_or_else(|| anyhow!("restore request {} has no destination", request.id))?;
            restore_drive(cfg, node_name, drive, destination, snapshot, None).await
        }
        RestoreScope::Volume { volume } => {
            let destination = request
                .destination_id
                .as_deref()
                .ok_or_else(|| anyhow!("restore request {} has no destination", request.id))?;
            restore_volume(cfg, node_name, volume, destination, snapshot, None).await
        }
        RestoreScope::Service { stack, service } => {
            restore_service(cfg, node_name, stack, service, request.destination_id.as_deref(), snapshot, None).await
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const STOP_POLL_MARGIN: Duration = Duration::from_secs(15);
const STOP_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Generous outer bound on top of a service's own `stop_grace_period` -
/// mirrors `folder_import`'s hardcoded fallback.
const DEFAULT_STOP_GRACE: Duration = cl_sched::reconcile::DEFAULT_STOP_GRACE;

/// Restore an entire drive from its most recent snapshot at `destination_id`
/// (or a specific one, if `snapshot` is given), stopping and restarting
/// every instance that mounts anything on it.
pub async fn restore_drive(
    cfg: &ClusterConfig,
    node_name: &str,
    drive: &str,
    destination_id: &str,
    snapshot: Option<&str>,
    via: Option<&str>,
) -> Result<()> {
    let ctx = Ctx::new(cfg, node_name, via)?;
    let state = ctx.client.state().await.map_err(|e| anyhow!("{e}"))?;
    let instances = instances_using_drive(&state, drive);

    println!("stopping {} instance(s) that use {drive}", instances.len());
    let stopped = ctx.stop(&state, &instances).await?;

    println!("restoring {drive} from {destination_id}");
    let target_dir = ctx.registry.root(drive)?.to_path_buf();
    ctx.restore(&state, destination_id, snapshot, &target_dir, None).await?;

    ctx.restart(&stopped).await?;
    println!("restored {drive}");
    Ok(())
}

/// Restore a single volume, scoped with `--include` off the drive's
/// whole-drive snapshot, stopping and restarting only the instances that
/// mount that volume.
pub async fn restore_volume(
    cfg: &ClusterConfig,
    node_name: &str,
    volume: &VolumeId,
    destination_id: &str,
    snapshot: Option<&str>,
    via: Option<&str>,
) -> Result<()> {
    let ctx = Ctx::new(cfg, node_name, via)?;
    let state = ctx.client.state().await.map_err(|e| anyhow!("{e}"))?;
    let instances = instances_using(&state, volume);

    println!("stopping {} instance(s) that use {volume}", instances.len());
    let stopped = ctx.stop(&state, &instances).await?;

    println!("restoring {volume} from {destination_id}");
    let target_dir = ctx.registry.root(&volume.drive)?.to_path_buf();
    let include = include_path_for_volume(&volume.volume);
    ctx.restore(&state, destination_id, snapshot, &target_dir, Some(&include)).await?;

    ctx.restart(&stopped).await?;
    println!("restored {volume}");
    Ok(())
}

/// Restore every volume a service uses, grouped by drive (a service can span
/// drives). Stops every instance of the service once, up front, and restarts
/// them once at the end, restoring drive by drive in between.
/// `destination_id` disambiguates when a drive backs up to more than one
/// destination; a drive with only one is restored from it regardless.
pub async fn restore_service(
    cfg: &ClusterConfig,
    node_name: &str,
    stack: &str,
    service: &str,
    destination_id: Option<&str>,
    snapshot: Option<&str>,
    via: Option<&str>,
) -> Result<()> {
    let ctx = Ctx::new(cfg, node_name, via)?;
    let state = ctx.client.state().await.map_err(|e| anyhow!("{e}"))?;

    let volumes = service_volumes(&state, stack, service)?;
    if volumes.is_empty() {
        println!("{stack}/{service} mounts no volumes; nothing to restore");
        return Ok(());
    }
    let instances = instances_of_service(&state, stack, service);

    println!("stopping {} instance(s) of {stack}/{service}", instances.len());
    let stopped = ctx.stop(&state, &instances).await?;

    let mut drives: Vec<&str> = volumes.iter().map(|v| v.drive.as_str()).collect();
    drives.sort();
    drives.dedup();

    for drive in drives {
        let destination = resolve_service_destination(&state, drive, destination_id)?;
        println!("restoring {drive} (for {stack}/{service}) from {destination}");
        let target_dir = ctx.registry.root(drive)?.to_path_buf();
        ctx.restore(&state, &destination, snapshot, &target_dir, None).await?;
    }

    ctx.restart(&stopped).await?;
    println!("restored {stack}/{service}");
    Ok(())
}

/// The destination to restore `drive` from: the caller's explicit choice if
/// given, otherwise the sole destination any enabled target on that drive
/// backs up to - ambiguous only when more than one exists, in which case the
/// caller must say which.
fn resolve_service_destination(state: &ClusterState, drive: &str, explicit: Option<&str>) -> Result<String> {
    if let Some(id) = explicit {
        return Ok(id.to_string());
    }
    let mut destinations: Vec<&str> = state
        .restic_targets
        .values()
        .filter(|t| t.drive == drive)
        .map(|t| t.destination_id.as_str())
        .collect();
    destinations.sort();
    destinations.dedup();
    match destinations.as_slice() {
        [] => bail!("drive {drive} has no backup target on record; pass --destination explicitly"),
        [only] => Ok(only.to_string()),
        many => bail!(
            "drive {drive} backs up to {} destinations ({}); pass --destination to pick one",
            many.len(),
            many.join(", ")
        ),
    }
}

fn instances_of_service(state: &ClusterState, stack: &str, service: &str) -> Vec<InstanceId> {
    state
        .assignments
        .values()
        .map(|a| &a.instance)
        .filter(|i| i.stack == stack && i.service == service)
        .cloned()
        .collect()
}

fn service_volumes(state: &ClusterState, stack: &str, service: &str) -> Result<Vec<VolumeId>> {
    let stack_state = state.stacks.get(stack).ok_or_else(|| anyhow!("stack {stack} not found"))?;
    let file = StackFile::parse(&stack_state.spec).with_context(|| format!("parsing stack {stack}"))?;
    let svc = file
        .services
        .get(service)
        .ok_or_else(|| anyhow!("service {service} not found in stack {stack}"))?;
    Ok(svc
        .volumes
        .iter()
        .filter_map(|mount| Some(VolumeId::new(mount.drive.as_ref()?.clone(), mount.volume.as_ref()?.clone())))
        .collect())
}

/// Shared plumbing every restore flow needs: the cluster client, this node's
/// drive registry, and the cluster key to open sealed backup secrets with.
struct Ctx {
    client: cl_raft::client::Client,
    registry: DriveRegistry,
    key: ClusterKey,
    node_name: String,
    data_dir: std::path::PathBuf,
}

impl Ctx {
    fn new(cfg: &ClusterConfig, node_name: &str, via: Option<&str>) -> Result<Self> {
        let node = cfg
            .node(node_name)
            .ok_or_else(|| anyhow!("node {node_name} is not declared in the config"))?;
        let registry = DriveRegistry::new(cfg, node_name)
            .with_context(|| format!("node {node_name} is not in the cluster config"))?;
        let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
            format!(
                "restoring needs the cluster key at {}. Run this from a node that holds it",
                cfg.secrets.key_file.display()
            )
        })?;
        let client = client_for(cfg, via)?;
        Ok(Self { client, registry, key, node_name: node_name.to_string(), data_dir: node.data_dir(cfg).to_path_buf() })
    }

    async fn stop(&self, state: &ClusterState, instances: &[InstanceId]) -> Result<Vec<InstanceId>> {
        if instances.is_empty() {
            return Ok(Vec::new());
        }
        set_desired(&self.client, instances, DesiredState::Stopped).await?;
        let deadline = max_stop_grace(state, instances) + STOP_POLL_MARGIN;
        wait_for_stopped(state, &self.node_name, instances, deadline).await;
        Ok(instances.to_vec())
    }

    async fn restart(&self, stopped: &[InstanceId]) -> Result<()> {
        if stopped.is_empty() {
            return Ok(());
        }
        set_desired(&self.client, stopped, DesiredState::Running).await
    }

    async fn restore(
        &self,
        state: &ClusterState,
        destination_id: &str,
        snapshot: Option<&str>,
        target_dir: &Path,
        include: Option<&str>,
    ) -> Result<()> {
        let destination: BackupDestination = state
            .restic_destinations
            .get(destination_id)
            .ok_or_else(|| anyhow!("unknown backup destination {destination_id}"))?
            .clone();
        let cred_dir = self.data_dir.join("restic").join("creds").join(&destination.id);
        let (repo, _guards) = backup_secrets::build_repo(state, &self.key, &cred_dir, &destination)?;

        let target_dir = target_dir.to_path_buf();
        let include = include.map(str::to_string);
        let requested = snapshot.map(str::to_string);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let snapshot_id = resolve_snapshot(&repo, requested.as_deref())?;
            debug!(snapshot = %snapshot_id, target = %target_dir.display(), "restic restore");
            repo.restore(&snapshot_id, &target_dir, include.as_deref())
                .map_err(|e| anyhow!("restic restore: {e}"))
        })
        .await
        .map_err(|e| anyhow!("the restic task panicked: {e}"))?
    }
}

/// `requested` as a literal snapshot id, or - when `None` or `"latest"` -
/// the most recent snapshot on record (comparing restic's RFC3339 `time`
/// strings, which sort correctly as plain text).
fn resolve_snapshot(repo: &ResticRepo, requested: Option<&str>) -> Result<String> {
    if let Some(id) = requested {
        if id != "latest" {
            return Ok(id.to_string());
        }
    }
    let snapshots = repo.snapshots().map_err(|e| anyhow!("listing snapshots: {e}"))?;
    latest_of(&snapshots)
        .map(|s| s.id.clone())
        .ok_or_else(|| anyhow!("this repository has no snapshots yet"))
}

fn latest_of(snapshots: &[SnapshotInfo]) -> Option<&SnapshotInfo> {
    snapshots.iter().max_by(|a, b| a.time.cmp(&b.time))
}

async fn set_desired(client: &cl_raft::client::Client, instances: &[InstanceId], desired: DesiredState) -> Result<()> {
    for instance in instances {
        let response = client
            .submit(Request::SetDesiredState { instance: instance.clone(), desired })
            .await
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(error) = response.error {
            bail!("could not update the desired state of {instance}: {error}");
        }
    }
    Ok(())
}

fn max_stop_grace(state: &ClusterState, instances: &[InstanceId]) -> Duration {
    instances
        .iter()
        .filter_map(|i| {
            let stack = state.stacks.get(&i.stack)?;
            let file = StackFile::parse(&stack.spec).ok()?;
            file.services.get(&i.service)?.stop_grace_period
        })
        .max()
        .unwrap_or(DEFAULT_STOP_GRACE)
}

/// Same shape as `folder_import`'s helper of the same name.
async fn wait_for_stopped(state: &ClusterState, node_name: &str, instances: &[InstanceId], deadline: Duration) {
    let local: Vec<String> = state
        .assignments
        .values()
        .filter(|a| a.node == node_name && instances.contains(&a.instance))
        .map(|a| a.instance.container_name())
        .collect();
    let remote = instances.len() > local.len();

    let Ok(docker) = cl_docker::Docker::connect() else {
        tokio::time::sleep(deadline).await;
        return;
    };

    let start = tokio::time::Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= deadline {
            return;
        }
        let mut all_stopped = true;
        for name in &local {
            match docker.inspect(name).await {
                Ok(Some(state)) if state.running => all_stopped = false,
                _ => {}
            }
        }
        if all_stopped && !remote {
            return;
        }
        tokio::time::sleep(STOP_POLL_INTERVAL.min(deadline - elapsed)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, time: &str) -> SnapshotInfo {
        SnapshotInfo { id: id.into(), time: time.into(), tags: Vec::new() }
    }

    #[test]
    fn latest_of_picks_the_most_recent_by_time() {
        let snapshots = vec![snap("a", "2024-01-01T00:00:00Z"), snap("b", "2024-06-01T00:00:00Z"), snap("c", "2024-03-01T00:00:00Z")];
        assert_eq!(latest_of(&snapshots).unwrap().id, "b");
    }

    #[test]
    fn latest_of_an_empty_list_is_none() {
        assert!(latest_of(&[]).is_none());
    }

    #[test]
    fn resolve_service_destination_passes_through_an_explicit_choice() {
        let state = ClusterState::default();
        assert_eq!(resolve_service_destination(&state, "fast", Some("d2")).unwrap(), "d2");
    }

    #[test]
    fn resolve_service_destination_requires_disambiguation_when_more_than_one_exists() {
        let mut state = ClusterState::default();
        state.restic_targets.insert(
            "t1".into(),
            cl_raft::state::DriveBackupTarget {
                id: "t1".into(),
                drive: "fast".into(),
                destination_id: "d1".into(),
                enabled: true,
                frequency_secs: 86_400,
                time_of_day: None,
                keep_last: None,
                keep_within_secs: None,
                stop_containers: None,
                last_run: None,
            },
        );
        state.restic_targets.insert(
            "t2".into(),
            cl_raft::state::DriveBackupTarget {
                id: "t2".into(),
                drive: "fast".into(),
                destination_id: "d2".into(),
                enabled: true,
                frequency_secs: 86_400,
                time_of_day: None,
                keep_last: None,
                keep_within_secs: None,
                stop_containers: None,
                last_run: None,
            },
        );
        assert!(resolve_service_destination(&state, "fast", None).is_err());
        assert!(resolve_service_destination(&state, "other", None)
            .unwrap_err()
            .to_string()
            .contains("no backup target"));
    }
}
