//! `/api/updates`: which containers are behind the image they deploy from.
//!
//! Three questions per instance, and they are genuinely three:
//!
//! * what the cluster has **pinned** (`Assignment::image_digest`),
//! * what the container is **running** (the registry digest behind its local
//!   image id),
//! * what the registry currently **serves** for the reference it deploys
//!   from.
//!
//! An update is available when the last differs from the second. Comparing
//! against the pin instead would be wrong in both directions: an unpinned
//! instance has nothing to compare, and a pinned one that has not been
//! rebuilt yet is behind its own pin, which is drift rather than an update.
//!
//! The registry lookups are the expensive part — one network round trip per
//! distinct image — so the fan-out is per *node*, not per instance: each node
//! scans everything it runs, deduplicating by image reference, and answers
//! once. A cluster running forty copies of the same three images makes three
//! registry calls, not forty.

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_raft::ids::InstanceId;
use cl_raft::state::{ClusterState, UpdatePhase};
use cl_raft::Request as ClusterRequest;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::auth::internal::InternalAuth;
use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::internal_peer;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/updates", get(list))
        .route("/api/updates/pin", post(pin))
        .route("/api/updates/apply", post(apply))
        // Node-to-node only, HMAC authenticated: one node's answer about the
        // containers it runs. See `internal_peer`.
        .route("/api/updates/internal/scan", get(internal_scan))
        // Internal-auth only: `clouduler update`. See `apply_internal`.
        .route("/api/updates/internal/apply", post(apply_internal))
}

const INTERNAL_SCAN_PATH: &str = "/api/updates/internal/scan";

/// The reference the stack file declares for an instance, e.g.
/// `nginx:1.25` — as opposed to `ContainerState::image`, which is what the
/// container was actually created from and, once an instance is pinned
/// (`spec::build`'s `pin`), is a `repo@sha256:...` reference with the tag
/// discarded entirely.
///
/// Asking the registry about that pinned reference instead of this one is a
/// no-op forever: a digest reference is immutable, so "what does the
/// registry serve for this exact digest" always answers with the same
/// digest, no matter how many new tags have been pushed since. The declared
/// reference is the one that can actually move.
fn declared_image(state: &ClusterState, instance: &InstanceId) -> Option<String> {
    state
        .stacks
        .get(&instance.stack)
        .and_then(|stack| cl_config::StackFile::parse(&stack.spec).ok())
        .and_then(|file| {
            file.services
                .get(&instance.service)
                .map(|s| s.image.clone())
        })
}

/// How many registry lookups one node runs at once. See `scan_local`.
///
/// Four rather than eight: at eight, ghcr.io and lscr.io started answering
/// 429 to a routine scan. `registry_digest` retries those, so the cap is
/// about making the retry rare rather than about correctness — and the
/// difference between four and eight in wall time is small next to the
/// difference between four and one.
const REGISTRY_LOOKUP_CONCURRENCY: usize = 4;

/// What one node can say about one of its containers without consulting raft.
#[derive(Serialize, Deserialize, Clone)]
struct ImageStatus {
    /// Instance id, as a string so this crosses the wire with no parse step.
    id: String,
    /// The reference the container was created from, e.g. `nginx:1.25`.
    image: Option<String>,
    running_digest: Option<String>,
    latest_digest: Option<String>,
    /// Why the registry could not be asked, when it could not be. Carried per
    /// instance rather than failing the scan: one image behind an expired
    /// login must not blank out the whole cluster's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// One row of the Updates view.
#[derive(Serialize)]
struct UpdateRow {
    id: String,
    stack: String,
    service: String,
    node: String,
    image: Option<String>,
    running_digest: Option<String>,
    latest_digest: Option<String>,
    /// What the cluster has pinned, if anything.
    pinned_digest: Option<String>,
    /// The registry serves something other than what is running.
    update_available: bool,
    /// A pin exists but the container is not on it. Distinct from
    /// `update_available`: this one is fixed by rebuilding the container, not
    /// by pulling anything new.
    pin_drift: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<UpdateRow>> {
    Json(rows(&state).await)
}

/// The whole cluster's answer, shared by the listing route and by `apply`,
/// which needs the same picture to decide what "everything out of date"
/// means.
async fn rows(state: &AppState) -> Vec<UpdateRow> {
    let cluster_state = state.cluster.state().await;

    let nodes: BTreeSet<String> = cluster_state
        .assignments
        .values()
        .map(|a| a.node.clone())
        .collect();

    // Every node in parallel: the slowest single node bounds the whole call,
    // rather than the sum of them.
    let scans = futures::future::join_all(nodes.iter().map(|node| scan_node(state, node))).await;

    let by_instance: BTreeMap<String, ImageStatus> = scans
        .into_iter()
        .flatten()
        .map(|status| (status.id.clone(), status))
        .collect();

    let mut rows: Vec<UpdateRow> = cluster_state
        .assignments
        .values()
        .map(|assignment| {
            let id = assignment.instance.to_string();
            let status = by_instance.get(&id);
            let running = status.and_then(|s| s.running_digest.clone());
            let latest = status.and_then(|s| s.latest_digest.clone());
            let pinned = assignment.image_digest.clone();

            // Both sides have to be known. An unknown running digest with a
            // known latest one is not evidence of an update: it is far more
            // often a locally built image that was never pulled from a
            // registry, and reporting those as perpetually out of date would
            // make "update all" a no-op that never stops asking.
            let update_available = match (&running, &latest) {
                (Some(running), Some(latest)) => running != latest,
                _ => false,
            };
            let pin_drift = match (&running, &pinned) {
                (Some(running), Some(pinned)) => running != pinned,
                _ => false,
            };

            UpdateRow {
                id,
                stack: assignment.instance.stack.clone(),
                service: assignment.instance.service.clone(),
                node: assignment.node.clone(),
                image: status.and_then(|s| s.image.clone()),
                running_digest: running,
                latest_digest: latest,
                pinned_digest: pinned,
                update_available,
                pin_drift,
                error: status.and_then(|s| s.error.clone()),
            }
        })
        .collect();

    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

/// Ask one node about everything it runs. A node that cannot be reached
/// contributes nothing rather than failing the request: the rest of the
/// cluster's answer is still worth having, and those rows simply show an
/// unknown running digest.
async fn scan_node(state: &AppState, node: &str) -> Vec<ImageStatus> {
    if node == state.node_name {
        return scan_local(state).await;
    }
    match internal_peer::fetch_json::<Vec<ImageStatus>>(state, node, INTERNAL_SCAN_PATH).await {
        Ok(statuses) => statuses,
        Err(e) => {
            warn!(%node, error = %e, "could not scan a node for image updates");
            Vec::new()
        }
    }
}

async fn internal_scan(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Json<Vec<ImageStatus>> {
    Json(scan_local(&state).await)
}

/// Inspect every container this node is assigned, then ask each distinct
/// registry reference once what it currently serves.
async fn scan_local(state: &AppState) -> Vec<ImageStatus> {
    let Some(docker) = state.docker.read().await.clone() else {
        return Vec::new();
    };
    let cluster_state = state.cluster.state().await;
    let registries = super::registries::load(state).await;

    // Pass one: what is running here, locally, with no network involved.
    let mut statuses = Vec::new();
    for assignment in cluster_state.assignments.values() {
        if assignment.node != state.node_name {
            continue;
        }
        let name = assignment.instance.container_name();
        let container = docker.inspect(&name).await.ok().flatten();
        let running_digest = match &container {
            Some(c) => {
                let repository = cl_docker::ImageRef::parse(&c.image).repository;
                match c.image_digest.as_deref() {
                    Some(image_id) => docker
                        .digest_of_image(image_id, &repository)
                        .await
                        .ok()
                        .flatten(),
                    None => None,
                }
            }
            None => None,
        };
        // The declared reference (the stack's tag), not `container.image`:
        // once pinned that is a digest, which is immutable and so can never
        // show as behind. See `declared_image`.
        let image = declared_image(&cluster_state, &assignment.instance)
            .or_else(|| container.map(|c| c.image));
        statuses.push(ImageStatus {
            id: assignment.instance.to_string(),
            image,
            running_digest,
            latest_digest: None,
            error: None,
        });
    }

    // Pass two: one registry lookup per distinct reference, however many
    // containers share it, and several in flight at once.
    //
    // Concurrency matters more than it looks. These are round trips to the
    // open internet, tens of them, and run one after another they added up to
    // well over the reverse proxy's 30s response timeout on a cluster this
    // size — the view 504'd rather than rendering. Bounded rather than
    // unlimited so a node with a hundred distinct images does not open a
    // hundred sockets, and because registries rate-limit anonymous pulls.
    let images: Vec<String> = statuses
        .iter()
        .filter_map(|s| s.image.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let latest: BTreeMap<String, Result<Option<String>, String>> = futures::stream::iter(images)
        .map(|image| {
            let docker = docker.clone();
            let credentials = registries.for_image(&image).cloned();
            async move {
                let answer = docker
                    .registry_digest(&image, credentials.as_ref())
                    .await
                    .map_err(|e| e.to_string());
                (image, answer)
            }
        })
        .buffer_unordered(REGISTRY_LOOKUP_CONCURRENCY)
        .collect()
        .await;

    for status in &mut statuses {
        let Some(image) = &status.image else { continue };
        match latest.get(image) {
            Some(Ok(digest)) => status.latest_digest.clone_from(digest),
            Some(Err(reason)) => status.error = Some(reason.clone()),
            None => {}
        }
    }
    statuses
}

#[derive(Deserialize, Default)]
struct InstanceSelection {
    /// Instances to act on. Empty or absent means "every instance this action
    /// applies to", which is what the view's bulk buttons send.
    #[serde(default)]
    instances: Vec<String>,
}

/// Outcome per instance, so a partial failure is legible rather than the
/// whole call reading as failed.
#[derive(Serialize)]
struct BulkResult {
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Record what each instance is running as its pin.
///
/// Only ever pins instances that have no pin yet: an instance already pinned
/// has had that decision made deliberately, and quietly repointing it at
/// whatever happens to be running now would undo a rollback. Instances whose
/// running digest is unknown are skipped, since there is nothing to pin.
async fn pin(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    body: Option<Json<InstanceSelection>>,
) -> Result<Json<Vec<BulkResult>>, ApiError> {
    let selection = body.map(|Json(b)| b).unwrap_or_default();
    let wanted: BTreeSet<String> = selection.instances.into_iter().collect();

    let mut results = Vec::new();
    for row in rows(&state).await {
        // Already pinned, not asked for, or nothing known to pin.
        if row.pinned_digest.is_some() {
            continue;
        }
        if !wanted.is_empty() && !wanted.contains(&row.id) {
            continue;
        }
        let Some(digest) = row.running_digest else {
            continue;
        };
        let Ok(instance) = row.id.parse::<InstanceId>() else {
            continue;
        };

        let outcome = state
            .cluster
            .submit(ClusterRequest::SetImageDigest { instance, digest })
            .await;
        results.push(match outcome {
            Ok(response) => BulkResult {
                id: row.id,
                ok: response.error.is_none(),
                error: response.error,
            },
            Err(e) => BulkResult {
                id: row.id,
                ok: false,
                error: Some(e.to_string()),
            },
        });
    }

    Ok(Json(results))
}

/// What `apply` took on. The work itself outlives the request.
#[derive(Serialize)]
struct Accepted {
    accepted: Vec<String>,
}

/// Start pulling and rebuilding, for the named instances or for every one
/// with an update available.
///
/// Answers `202` as soon as the work is scheduled rather than when it is
/// done, because it cannot be done in time. A pull is a download of hundreds
/// of megabytes over whatever link the node has; the reverse proxy gives a
/// backend 30 seconds. Holding the request open across that produced a
/// gateway timeout every time, on an update that was in fact running fine —
/// the worst possible reading, since the operator sees a failure and clicks
/// again, stacking a second pull on the first.
///
/// The caller watches progress through `ClusterState::instance_updates`,
/// which every step of every update is written into as it happens - so it
/// arrives over the ordinary `/api/events` state stream, and reads the same
/// from whichever node the browser is talking to.
///
/// The updates themselves run sequentially inside the spawned task, each
/// waiting for the previous one's new container to be up. Each one takes its
/// container down and lets the reconcile loop build a new one, so doing the
/// whole cluster at once would stop every replica of a service
/// simultaneously — the one thing a rolling update exists to avoid.
async fn apply(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    body: Option<Json<InstanceSelection>>,
) -> Result<(StatusCode, Json<Accepted>), ApiError> {
    let selection = body.map(|Json(b)| b).unwrap_or_default();
    Ok(apply_selection(&state, selection).await)
}

/// `clouduler update`: the same trigger as the ui's apply button, over the
/// internal-auth channel the cli already uses for git-backup's manual
/// triggers, since scheduling pulls is imperative action against the
/// running daemon rather than a raft state change - there is nothing for
/// `client.submit` to write.
async fn apply_internal(
    _auth: InternalAuth,
    State(state): State<AppState>,
    body: Option<Json<InstanceSelection>>,
) -> (StatusCode, Json<Accepted>) {
    let selection = body.map(|Json(b)| b).unwrap_or_default();
    apply_selection(&state, selection).await
}

async fn apply_selection(
    state: &AppState,
    selection: InstanceSelection,
) -> (StatusCode, Json<Accepted>) {
    // Only scan when the caller did not say what it wanted. The ui always
    // does — it is looking at the answer already — so the common path costs
    // nothing. Scanning regardless meant a single-row update paid for a
    // full cluster-wide registry sweep before it even started pulling.
    let instances: Vec<InstanceId> = if selection.instances.is_empty() {
        rows(state)
            .await
            .into_iter()
            .filter(|row| row.update_available)
            .filter_map(|row| row.id.parse().ok())
            .collect()
    } else {
        selection
            .instances
            .iter()
            .filter_map(|id| id.parse().ok())
            .collect()
    };

    // Everything accepted is queued in the replicated record before
    // answering, so every browser (on any node) sees the whole batch at once
    // rather than each instance only once its turn comes. One already mid-
    // update is left alone: stacking a second pull on it helps nobody.
    let cluster_state = state.cluster.state().await;
    let mut queued = Vec::new();
    for instance in instances {
        let Some(assignment) = cluster_state.assignments.get(&instance) else {
            continue;
        };
        let in_flight = cluster_state
            .instance_updates
            .get(&instance)
            .is_some_and(|u| !u.phase.is_terminal());
        if in_flight {
            continue;
        }
        cl_sched::progress::submit(
            &state.cluster,
            &instance,
            &assignment.node,
            UpdatePhase::Queued,
            None,
        )
        .await;
        queued.push((instance, assignment.node.clone()));
    }
    drop(cluster_state);

    let accepted: Vec<String> = queued.iter().map(|(i, _)| i.to_string()).collect();

    let worker = state.clone();
    tokio::spawn(async move {
        for (instance, node) in queued {
            match super::instances::update_instance(&worker, instance.clone()).await {
                Ok(()) => {
                    // `update_instance` returns once the old container is
                    // gone; the new one is the reconciler's to bring up. Wait
                    // for that before touching the next instance, so a
                    // service's replicas really do go down one at a time.
                    match wait_for_update(&worker, &instance).await {
                        Some(UpdatePhase::Started) => info!(%instance, "updated"),
                        Some(phase) => warn!(%instance, ?phase, "update did not finish"),
                        None => warn!(%instance, "gave up waiting for the update to finish"),
                    }
                }
                // The record is what the operator watches, so the failure
                // goes there too. The owning node records its own failures,
                // but not one it never heard about (it was unreachable).
                Err(e) => {
                    warn!(%instance, error = %e, "update failed");
                    let still_queued = worker
                        .cluster
                        .state()
                        .await
                        .instance_updates
                        .get(&instance)
                        .is_some_and(|u| u.phase == UpdatePhase::Queued);
                    if still_queued {
                        cl_sched::progress::submit(
                            &worker.cluster,
                            &instance,
                            &node,
                            UpdatePhase::Failed,
                            Some(e.to_string()),
                        )
                        .await;
                    }
                }
            }
        }
    });

    (StatusCode::ACCEPTED, Json(Accepted { accepted }))
}

/// How long one instance's update may take, from its old container going
/// away to its new one running, before the batch moves on regardless.
const UPDATE_FINISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Waits for an instance's update record to reach a terminal phase, and
/// returns it. `None` on timeout, or if the record disappeared (the instance
/// was removed or moved away).
async fn wait_for_update(state: &AppState, instance: &InstanceId) -> Option<UpdatePhase> {
    let deadline = tokio::time::Instant::now() + UPDATE_FINISH_TIMEOUT;
    loop {
        let phase = state
            .cluster
            .state()
            .await
            .instance_updates
            .get(instance)
            .map(|u| u.phase)?;
        if phase.is_terminal() {
            return Some(phase);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}
