//! Running the migrations the api records.
//!
//! `POST /api/instances/<id>/move` writes a `Migration` into raft and returns.
//! Something then has to carry it through the phases in `docs/drives.md`, and
//! this is that something: a loop on every node that looks for migrations it
//! owns and hands each one to `cl_drive::migration::run`.
//!
//! Ownership is `migration.from`, not the assignment's node, because
//! `Switching` repoints the assignment at the target halfway through. Keying
//! off `from` means exactly one node drives a move for its whole life, and a
//! node that died mid-move recognises its own unfinished job when it comes
//! back and resumes from the recorded phase.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cl_config::{ClusterConfig, StackFile};
use cl_docker::Docker;
use cl_drive::{drive_port, DeltaEngine, DriveRegistry, InstanceControl, MigrationJob};
use cl_raft::ids::{InstanceId, VolumeId};
use cl_raft::node::ClusterNode;
use cl_raft::state::ClusterState;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// How often to look for migrations to pick up. A move is started by hand and
/// then takes as long as the data takes, so there is nothing to gain from
/// looking more often than the scheduler sweeps.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long a container gets to stop cleanly before the migration takes its
/// volumes. The final sync cannot start until it has let go of them.
const STOP_GRACE: Duration = Duration::from_secs(30);

/// Watch for migrations this node owns and run them.
///
/// Always spawned now, even for a node that implements no drives at boot:
/// one added to `node.drives` later becomes a usable migration source or
/// target as soon as the drive subsystem's live registry picks it up,
/// without a restart. A node with no registry yet simply claims nothing.
pub fn start(
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
    docker: Arc<tokio::sync::RwLock<Option<Docker>>>,
    registry: Arc<tokio::sync::RwLock<Option<Arc<DriveRegistry>>>>,
) -> JoinHandle<()> {
    let node_name = node_name.to_string();
    let cluster = cluster.clone();

    tokio::spawn(async move {
        let engine = DeltaEngine::default();
        // A migration outlives many polls, so the loop has to know which ones
        // it already handed off or it would start a second run of each on
        // every tick.
        let running: Arc<Mutex<BTreeSet<InstanceId>>> = Arc::default();

        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        loop {
            ticker.tick().await;
            let state = cluster.state().await;

            for job in claimable(&state, &node_name) {
                if !running.lock().await.insert(job.instance.clone()) {
                    continue;
                }
                let cfg = config_rx.borrow().clone();
                let Some(target_addr) = target_addr(&cfg, &job.target_node) else {
                    warn!(
                        instance = %job.instance,
                        target = %job.target_node,
                        "cannot migrate: the target has no address on the internal network",
                    );
                    running.lock().await.remove(&job.instance);
                    continue;
                };
                // A dockerd that is not connected right now, or a drive
                // subsystem mid-rebuild, is not a reason to fail the
                // migration outright: leave it unclaimed and try again on
                // the next tick, same as a target with no address.
                let (Some(docker), Some(registry)) = (docker.read().await.clone(), registry.read().await.clone())
                else {
                    running.lock().await.remove(&job.instance);
                    continue;
                };
                let control = DockerControl { node: node_name.clone(), docker };

                info!(
                    instance = %job.instance,
                    to = %job.target_node,
                    volumes = job.volumes.len(),
                    "picking up a migration",
                );

                let migration = MigrationJob {
                    instance: job.instance.clone(),
                    source_node: job.source_node,
                    target_node: job.target_node,
                    target_addr,
                    volumes: job.volumes,
                    registry: &registry,
                    engine: &engine,
                    control: &control,
                };

                match cl_drive::migration::run(&cluster, &migration).await {
                    Ok(()) => info!(instance = %migration.instance, "migration finished"),
                    // The phase is recorded as `Failed` by the time this
                    // returns, so the move is over rather than retried: the
                    // source still owns the data and someone has to say why
                    // before it is worth trying again.
                    Err(e) => {
                        warn!(instance = %migration.instance, error = %e, "migration failed")
                    }
                }
                running.lock().await.remove(&migration.instance);
            }
        }
    })
}

/// One migration this node has to drive.
struct Claim {
    instance: InstanceId,
    source_node: String,
    target_node: String,
    volumes: Vec<VolumeId>,
}

/// The migrations on record that `node` owns and that are not over.
fn claimable(state: &ClusterState, node: &str) -> Vec<Claim> {
    let mut claims = Vec::new();
    for assignment in state.assignments.values() {
        let Some(migration) = &assignment.migration else {
            continue;
        };
        if migration.from != node || migration.phase.is_terminal() {
            continue;
        }
        claims.push(Claim {
            instance: assignment.instance.clone(),
            source_node: migration.from.clone(),
            target_node: migration.to.clone(),
            volumes: volumes_of(state, &assignment.instance),
        });
    }
    claims
}

/// Volumes an instance mounts, which all move together.
///
/// Read back out of the stack spec rather than the assignment, which records
/// where an instance runs but not what it holds.
fn volumes_of(state: &ClusterState, instance: &InstanceId) -> Vec<VolumeId> {
    let Some(stack) = state.stacks.get(&instance.stack) else {
        return Vec::new();
    };
    let Ok(file) = StackFile::parse(&stack.spec) else {
        return Vec::new();
    };
    let Some(service) = file.services.get(&instance.service) else {
        return Vec::new();
    };
    service
        .volumes
        .iter()
        .filter_map(|mount| Some(VolumeId::new(mount.drive.as_ref()?, mount.volume.as_ref()?)))
        .collect()
}

/// Where the target answers drive transport requests.
fn target_addr(cfg: &ClusterConfig, target: &str) -> Option<SocketAddr> {
    let node = cfg.node(target)?;
    Some(SocketAddr::new(
        cfg.internal_ip(node)?,
        drive_port(node.rpc_port),
    ))
}

/// `InstanceControl` over the local docker daemon.
struct DockerControl {
    node: String,
    docker: Docker,
}

impl InstanceControl for DockerControl {
    fn stop_instance<'a>(
        &'a self,
        instance: &'a InstanceId,
        node: &'a str,
    ) -> cl_drive::BoxFuture<'a, cl_drive::Result<()>> {
        Box::pin(async move {
            if node != self.node {
                return Err(cl_drive::DriveError::Config(format!(
                    "asked to stop {instance} on {node}, but this is {}",
                    self.node
                )));
            }
            self.docker
                .stop(&instance.container_name(), STOP_GRACE)
                .await
                .map_err(|e| cl_drive::DriveError::Transfer {
                    message: format!("stopping {instance}: {e}"),
                })
        })
    }

    fn start_instance<'a>(
        &'a self,
        instance: &'a InstanceId,
        node: &'a str,
    ) -> cl_drive::BoxFuture<'a, cl_drive::Result<()>> {
        Box::pin(async move {
            if node == self.node {
                return self
                    .docker
                    .start(&instance.container_name())
                    .await
                    .map_err(|e| cl_drive::DriveError::Transfer {
                        message: format!("starting {instance}: {e}"),
                    });
            }
            // The normal case: the migration is driven by the source, and the
            // container has never existed on the target, so there is nothing
            // here to start. `Switching` has already pointed the assignment at
            // the target, and the target's own reconcile loop creates and
            // starts it on its next sweep, the same way it would for any other
            // instance it has been given. There is no rpc for starting a
            // container on another node, and adding one would duplicate that.
            info!(
                instance = %instance,
                node = %node,
                "handing the container to the target's reconcile loop",
            );
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::{Assignment, DesiredState, Migration, MigrationPhase, StackState};

    fn state_with(phase: MigrationPhase, from: &str) -> ClusterState {
        let mut state = ClusterState::default();
        state.stacks.insert(
            "app".to_string(),
            StackState {
                name: "app".to_string(),
                spec: "services:\n  db:\n    image: postgres\n    volumes:\n      - fast/data:/var/lib\n"
                    .to_string(),
                revision: 1,
                configs: Default::default(),
                secrets: Default::default(),
                used_configs: Default::default(),
                used_secrets: Default::default(),
            },
        );
        let instance: InstanceId = "app.db.0".parse().expect("instance id");
        state.assignments.insert(
            instance.clone(),
            Assignment {
                instance,
                node: "n1".to_string(),
                desired: DesiredState::Running,
                image_digest: None,
                migration: Some(Migration {
                    from: from.to_string(),
                    to: "n2".to_string(),
                    phase,
                }),
            },
        );
        state
    }

    #[test]
    fn a_node_claims_only_its_own_migrations() {
        let state = state_with(MigrationPhase::Syncing, "n1");
        assert_eq!(claimable(&state, "n1").len(), 1);
        assert!(claimable(&state, "n2").is_empty());
    }

    #[test]
    fn the_source_keeps_its_claim_after_the_switch() {
        // `Switching` repoints the assignment at n2, so a claim keyed off the
        // assignment would change hands halfway through the move.
        let state = state_with(MigrationPhase::Starting, "n1");
        assert_eq!(claimable(&state, "n1").len(), 1);
        assert!(claimable(&state, "n2").is_empty());
    }

    #[test]
    fn a_finished_migration_is_not_claimed() {
        let state = state_with(MigrationPhase::Done, "n1");
        assert!(claimable(&state, "n1").is_empty());

        let state = state_with(MigrationPhase::Failed { reason: "x".into() }, "n1");
        assert!(claimable(&state, "n1").is_empty());
    }

    #[test]
    fn a_claim_carries_the_volumes_the_instance_mounts() {
        let state = state_with(MigrationPhase::Stopping, "n1");
        let claims = claimable(&state, "n1");
        assert_eq!(claims[0].volumes, vec![VolumeId::new("fast", "data")]);
    }
}
