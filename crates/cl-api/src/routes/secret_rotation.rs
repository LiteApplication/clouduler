//! Secret usage and rotation.
//!
//! Overwriting a secret only changes the stored value. A running container
//! keeps what it was created with (a file bind-mounted by inode, or an
//! environment variable), so the cluster records which revision of each
//! secret every container started with (`clouduler.secrets` label, reported
//! into `ClusterState::container_status`) and this module compares that with
//! the secret's current revision.
//!
//! Restarting is the only way a container picks up a new value, for files and
//! `env:` alike: the file is a single-file bind mount, which keeps pointing at
//! the old inode when the host copy is replaced. The roll removes one
//! container at a time per service and lets the reconcile loop recreate it,
//! the same way an image update does, with progress in
//! `ClusterState::instance_updates`.
//!
//! Clouduler does not migrate anything derived from the old value: a database
//! password stored inside the database or an encryption key protecting data at
//! rest needs the application's own rotation procedure.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use cl_config::StackFile;
use cl_raft::ids::InstanceId;
use cl_raft::state::{ClusterState, DesiredState, UpdatePhase};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::objects::{is_reserved_secret, store_secret, PutSecretRequest};
use crate::auth::internal::InternalBodyAuth;
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/secrets/{name}/rotate", post(rotate))
        .route("/api/secrets/{name}/restart", post(restart))
        // Internal-auth only: `clouduler secret rotate --restart`.
        .route("/api/secrets/internal/restart", post(restart_internal))
}

/// How a service receives a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    File,
    Env,
}

/// Whether a container runs the secret's current revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Freshness {
    /// Created with the current revision.
    Current,
    /// Created with an older revision.
    Stale,
    /// Nothing recorded: the container predates revision tracking, or its
    /// node has not reported yet.
    Unknown,
}

/// One instance that references a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecretUse {
    pub instance: String,
    pub stack: String,
    pub service: String,
    pub node: String,
    pub delivery: Delivery,
    /// The environment variable name, or the path inside the container.
    pub target: String,
    pub running: bool,
    pub running_revision: Option<u64>,
    pub freshness: Freshness,
}

/// Freshness of a container given what it was created with.
pub fn freshness(running_revision: Option<u64>, current: u64) -> Freshness {
    match running_revision {
        Some(r) if r >= current => Freshness::Current,
        Some(_) => Freshness::Stale,
        None => Freshness::Unknown,
    }
}

/// Every instance whose service references `secret`, with the revision its
/// container started with.
pub fn usage(state: &ClusterState, secret: &str) -> Vec<SecretUse> {
    let Some(entry) = state.secrets.get(secret) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for stack in state.stacks.values() {
        let Ok(file) = StackFile::parse(&stack.spec) else {
            continue;
        };
        for (service, def) in &file.services {
            let refs: Vec<_> = def.secrets.iter().filter(|r| r.source == secret).collect();
            if refs.is_empty() {
                continue;
            }
            for assignment in state
                .assignments
                .values()
                .filter(|a| a.instance.stack == stack.name && &a.instance.service == service)
            {
                let status = state.container_status.get(&assignment.instance);
                let running_revision = status.and_then(|s| s.secret_revisions.get(secret).copied());
                for r in &refs {
                    let (delivery, target) = match &r.env {
                        Some(var) => (Delivery::Env, var.clone()),
                        None => (Delivery::File, r.secret_target().display().to_string()),
                    };
                    out.push(SecretUse {
                        instance: assignment.instance.to_string(),
                        stack: stack.name.clone(),
                        service: service.clone(),
                        node: assignment.node.clone(),
                        delivery,
                        target,
                        running: status.is_some_and(|s| s.running),
                        running_revision,
                        freshness: freshness(running_revision, entry.revision),
                    });
                }
            }
        }
    }
    out
}

/// Why an instance is not in a roll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub instance: String,
    pub reason: String,
}

/// What a roll would touch: instances grouped by (stack, service), each group
/// in index order and restarted one at a time.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RollPlan {
    pub lanes: BTreeMap<(String, String), Vec<(InstanceId, String)>>,
    pub skipped: Vec<Skipped>,
}

impl RollPlan {
    pub fn instances(&self) -> Vec<String> {
        self.lanes
            .values()
            .flatten()
            .map(|(i, _)| i.to_string())
            .collect()
    }
}

/// Which instances need restarting to pick up `secret` at revision `current`.
///
/// In: an instance that should be running whose container is not provably
/// current (an older revision, or none recorded). Out: one that already runs
/// the current revision, one stopped on purpose (it takes the new value
/// whenever it next starts), one moving between nodes, and one with an update
/// already in flight.
pub fn plan_roll(state: &ClusterState, secret: &str, current: u64) -> RollPlan {
    let mut plan = RollPlan::default();
    let mut seen = BTreeSet::new();
    for u in usage(state, secret) {
        if !seen.insert(u.instance.clone()) {
            continue;
        }
        let Ok(instance) = u.instance.parse::<InstanceId>() else {
            continue;
        };
        let assignment = &state.assignments[&instance];
        let skip = if freshness(u.running_revision, current) == Freshness::Current {
            Some("already runs the current revision")
        } else if assignment.desired != DesiredState::Running {
            Some("stopped; it starts with the new value")
        } else if assignment
            .migration
            .as_ref()
            .is_some_and(|m| !m.phase.is_terminal())
        {
            Some("moving between nodes")
        } else if state
            .instance_updates
            .get(&instance)
            .is_some_and(|up| !up.phase.is_terminal())
        {
            Some("an update is already in progress")
        } else {
            None
        };
        match skip {
            Some(reason) => plan.skipped.push(Skipped {
                instance: u.instance,
                reason: reason.into(),
            }),
            None => plan
                .lanes
                .entry((u.stack, u.service))
                .or_default()
                .push((instance, u.node)),
        }
    }
    for lane in plan.lanes.values_mut() {
        lane.sort_by_key(|(i, _)| i.index);
    }
    plan
}

#[derive(Serialize)]
pub struct RollStarted {
    pub revision: u64,
    pub restarting: Vec<String>,
    pub skipped: Vec<Skipped>,
}

/// Queue the roll and run it in the background. Answers when queued: a
/// restart waits for health and can take minutes, longer than a request may.
async fn start_roll(state: &AppState, name: &str, revision: u64) -> RollStarted {
    // The rotating request may land on a follower whose copy has not applied
    // the new revision yet; planning against the old one would find nothing.
    for _ in 0..50 {
        let applied = state
            .cluster
            .state()
            .await
            .secrets
            .get(name)
            .is_some_and(|s| s.revision >= revision);
        if applied {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let cluster_state = state.cluster.state().await;
    let plan = plan_roll(&cluster_state, name, revision);
    drop(cluster_state);

    for (instance, node) in plan.lanes.values().flatten() {
        cl_sched::progress::submit(&state.cluster, instance, node, UpdatePhase::Queued, None).await;
    }
    let started = RollStarted {
        revision,
        restarting: plan.instances(),
        skipped: plan.skipped.clone(),
    };
    if !plan.lanes.is_empty() {
        let worker = state.clone();
        let name = name.to_string();
        tokio::spawn(async move { roll(worker, name, plan).await });
    }
    started
}

struct Roll {
    worker: AppState,
    secret: String,
    inner: Mutex<RollState>,
}

struct RollState {
    failed_by: Option<InstanceId>,
    /// Not yet claimed for a restart, with their nodes.
    pending: BTreeMap<InstanceId, String>,
}

impl Roll {
    fn halted(&self) -> bool {
        self.inner.lock().expect("roll lock").failed_by.is_some()
    }

    fn claim(&self, instance: &InstanceId) -> bool {
        let mut inner = self.inner.lock().expect("roll lock");
        inner.failed_by.is_none() && inner.pending.remove(instance).is_some()
    }

    /// Record the failure and cancel every instance that has not started.
    async fn fail(&self, instance: &InstanceId, node: &str, reason: &str) {
        let cancelled = {
            let mut inner = self.inner.lock().expect("roll lock");
            inner.failed_by.get_or_insert_with(|| instance.clone());
            std::mem::take(&mut inner.pending)
        };
        warn!(%instance, secret = %self.secret, %reason, "restart after secret rotation failed, halting");
        let cluster = &self.worker.cluster;
        cl_sched::progress::submit(
            cluster,
            instance,
            node,
            UpdatePhase::Failed,
            Some(format!(
                "restart for the new revision of secret {} failed: {reason}",
                self.secret
            )),
        )
        .await;
        for (other, other_node) in cancelled {
            cl_sched::progress::submit(
                cluster,
                &other,
                &other_node,
                UpdatePhase::Failed,
                Some(format!(
                    "cancelled: restarting {instance} for secret {} failed, so the rotation stopped",
                    self.secret
                )),
            )
            .await;
        }
    }
}

async fn roll(worker: AppState, secret: String, plan: RollPlan) {
    let pending = plan
        .lanes
        .values()
        .flatten()
        .map(|(i, n)| (i.clone(), n.clone()))
        .collect();
    let roll = Arc::new(Roll {
        worker,
        secret,
        inner: Mutex::new(RollState {
            failed_by: None,
            pending,
        }),
    });
    let lanes = plan
        .lanes
        .into_values()
        .map(|lane| roll_lane(roll.clone(), lane));
    futures::future::join_all(lanes).await;
}

/// One service's instances, strictly one after another, each waiting for its
/// replacement to be up (and healthy, when it has a healthcheck).
async fn roll_lane(roll: Arc<Roll>, lane: Vec<(InstanceId, String)>) {
    for (instance, node) in lane {
        if roll.halted() || !roll.claim(&instance) {
            return;
        }
        let cluster = &roll.worker.cluster;
        cl_sched::progress::submit(cluster, &instance, &node, UpdatePhase::Stopping, None).await;
        if let Err(e) = super::instances::remove_container_on(&roll.worker, &instance, &node).await
        {
            roll.fail(&instance, &node, &e.to_string()).await;
            return;
        }
        cl_sched::progress::submit(cluster, &instance, &node, UpdatePhase::Starting, None).await;
        match super::updates::wait_for_update(&roll.worker, &instance).await {
            Some((UpdatePhase::Started, _)) => {
                info!(%instance, secret = %roll.secret, "restarted")
            }
            Some((_, error)) => {
                let why = error.unwrap_or_else(|| "the restart failed".into());
                roll.fail(&instance, &node, &why).await;
                return;
            }
            None => {
                roll.fail(
                    &instance,
                    &node,
                    "gave up waiting for the container to come up",
                )
                .await;
                return;
            }
        }
    }
}

#[derive(Deserialize)]
struct RotateRequest {
    #[serde(flatten)]
    value: PutSecretRequest,
    /// Restart the instances that run an older revision, one at a time per
    /// service. Without it the value is only stored.
    #[serde(default)]
    restart: bool,
}

async fn rotate(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<RotateRequest>,
) -> Result<Json<RollStarted>, ApiError> {
    if is_reserved_secret(&name) {
        return Err(ApiError::BadRequest(format!("{name} is a reserved name")));
    }
    let owner = state
        .cluster
        .state()
        .await
        .secrets
        .get(&name)
        .ok_or_else(|| ApiError::NotFound(format!("no secret {name}")))?
        .owner
        .clone();
    let revision = store_secret(&state, &name, body.value, owner).await?;
    if body.restart {
        Ok(Json(start_roll(&state, &name, revision).await))
    } else {
        Ok(Json(RollStarted {
            revision,
            restarting: Vec::new(),
            skipped: Vec::new(),
        }))
    }
}

/// Restart whatever still runs an older revision of an already stored secret.
async fn restart(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<RollStarted>), ApiError> {
    restart_named(&state, &name).await
}

async fn restart_named(
    state: &AppState,
    name: &str,
) -> Result<(StatusCode, Json<RollStarted>), ApiError> {
    if is_reserved_secret(name) {
        return Err(ApiError::BadRequest(format!("{name} is a reserved name")));
    }
    let revision = state
        .cluster
        .state()
        .await
        .secrets
        .get(name)
        .map(|s| s.revision)
        .ok_or_else(|| ApiError::NotFound(format!("no secret {name}")))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(start_roll(state, name, revision).await),
    ))
}

#[derive(Deserialize)]
struct RestartBody {
    name: String,
}

async fn restart_internal(
    auth: InternalBodyAuth,
    State(state): State<AppState>,
    body: Bytes,
) -> Result<(StatusCode, Json<RollStarted>), ApiError> {
    auth.check(&body)?;
    let RestartBody { name } = serde_json::from_slice(&body)
        .map_err(|e| ApiError::BadRequest(format!("invalid restart request: {e}")))?;
    restart_named(&state, &name).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::{
        Assignment, ContainerStatus, InstanceUpdate, Migration, MigrationPhase, SecretEntry,
        StackState,
    };

    const SPEC: &str = "secrets:\n  db:\n    file: ./db\n  unrelated:\n    file: ./u\nservices:\n  web:\n    image: nginx\n    secrets:\n      - source: db\n        env: DB_PASSWORD\n  worker:\n    image: nginx\n    secrets:\n      - db\n  other:\n    image: nginx\n    secrets:\n      - unrelated\n";

    fn state() -> ClusterState {
        let mut s = ClusterState::default();
        s.stacks.insert(
            "app".into(),
            StackState {
                name: "app".into(),
                spec: SPEC.into(),
                revision: 1,
                configs: Default::default(),
                secrets: Default::default(),
                used_configs: Default::default(),
                used_secrets: Default::default(),
                deployed_at: 0,
                previous_spec: None,
            },
        );
        s.secrets.insert(
            "db".into(),
            SecretEntry {
                name: "db".into(),
                sealed: String::new(),
                revision: 3,
                key_fingerprint: "fp".into(),
                owner: None,
            },
        );
        for (service, index, rev) in [
            ("web", 0, Some(3)),
            ("web", 1, Some(2)),
            ("web", 2, None),
            ("worker", 0, Some(1)),
            ("other", 0, Some(1)),
        ] {
            let id = InstanceId::new("app", service, index);
            s.assignments.insert(
                id.clone(),
                Assignment {
                    instance: id.clone(),
                    node: "n1".into(),
                    desired: DesiredState::Running,
                    image_digest: None,
                    migration: None,
                },
            );
            s.container_status.insert(
                id,
                ContainerStatus {
                    node: "n1".into(),
                    running: true,
                    secret_revisions: rev
                        .map(|r| BTreeMap::from([("db".to_string(), r)]))
                        .unwrap_or_default(),
                },
            );
        }
        s
    }

    #[test]
    fn freshness_compares_the_started_revision_with_the_current_one() {
        assert_eq!(freshness(Some(3), 3), Freshness::Current);
        assert_eq!(freshness(Some(2), 3), Freshness::Stale);
        assert_eq!(freshness(None, 3), Freshness::Unknown);
    }

    #[test]
    fn usage_lists_only_instances_of_services_that_reference_the_secret() {
        let u = usage(&state(), "db");
        let ids: Vec<&str> = u.iter().map(|u| u.instance.as_str()).collect();
        assert_eq!(ids, ["app.web.0", "app.web.1", "app.web.2", "app.worker.0"]);
        let web0 = &u[0];
        assert_eq!(web0.delivery, Delivery::Env);
        assert_eq!(web0.target, "DB_PASSWORD");
        assert_eq!(web0.freshness, Freshness::Current);
        assert_eq!(u[1].freshness, Freshness::Stale);
        assert_eq!(u[2].freshness, Freshness::Unknown);
        let worker = &u[3];
        assert_eq!(worker.delivery, Delivery::File);
        assert_eq!(worker.target, "/run/secrets/db");
        assert_eq!(worker.running_revision, Some(1));
    }

    #[test]
    fn usage_of_a_missing_or_unused_secret_is_empty() {
        assert!(usage(&state(), "nope").is_empty());
        let mut s = state();
        s.secrets.insert(
            "lonely".into(),
            SecretEntry {
                name: "lonely".into(),
                sealed: String::new(),
                revision: 1,
                key_fingerprint: "fp".into(),
                owner: None,
            },
        );
        assert!(usage(&s, "lonely").is_empty());
    }

    #[test]
    fn the_roll_takes_stale_and_unknown_and_leaves_current_ones() {
        let plan = plan_roll(&state(), "db", 3);
        assert_eq!(plan.instances(), ["app.web.1", "app.web.2", "app.worker.0"]);
        assert_eq!(plan.lanes.len(), 2, "one lane per service");
        assert_eq!(
            plan.skipped,
            [Skipped {
                instance: "app.web.0".into(),
                reason: "already runs the current revision".into()
            }]
        );
    }

    #[test]
    fn a_newer_revision_than_any_container_rolls_everything_in_index_order() {
        let plan = plan_roll(&state(), "db", 4);
        let web = &plan.lanes[&("app".to_string(), "web".to_string())];
        let indexes: Vec<u32> = web.iter().map(|(i, _)| i.index).collect();
        assert_eq!(indexes, [0, 1, 2]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn stopped_moving_and_updating_instances_are_left_out() {
        let mut s = state();
        s.assignments
            .get_mut(&InstanceId::new("app", "web", 1))
            .unwrap()
            .desired = DesiredState::Stopped;
        s.assignments
            .get_mut(&InstanceId::new("app", "web", 2))
            .unwrap()
            .migration = Some(Migration {
            from: "n1".into(),
            to: "n2".into(),
            phase: MigrationPhase::Syncing,
        });
        s.instance_updates.insert(
            InstanceId::new("app", "worker", 0),
            InstanceUpdate {
                node: "n1".into(),
                phase: UpdatePhase::Pulling,
                started_at: 0,
                at: 0,
                error: None,
            },
        );
        let plan = plan_roll(&s, "db", 4);
        assert_eq!(plan.instances(), ["app.web.0"]);
        let reasons: Vec<&str> = plan.skipped.iter().map(|s| s.reason.as_str()).collect();
        assert_eq!(reasons.len(), 3);
        assert!(reasons.iter().any(|r| r.starts_with("stopped")));
        assert!(reasons.iter().any(|r| r.starts_with("moving")));
        assert!(reasons.iter().any(|r| r.starts_with("an update")));
    }

    #[test]
    fn a_finished_update_record_does_not_block_a_roll() {
        let mut s = state();
        s.instance_updates.insert(
            InstanceId::new("app", "worker", 0),
            InstanceUpdate {
                node: "n1".into(),
                phase: UpdatePhase::Failed,
                started_at: 0,
                at: 0,
                error: Some("x".into()),
            },
        );
        assert!(plan_roll(&s, "db", 4)
            .instances()
            .contains(&"app.worker.0".to_string()));
    }
}
