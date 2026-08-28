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
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_raft::ids::InstanceId;
use cl_raft::Request as ClusterRequest;
use serde::{Deserialize, Serialize};
use tracing::warn;

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
}

const INTERNAL_SCAN_PATH: &str = "/api/updates/internal/scan";

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

async fn internal_scan(_auth: InternalAuth, State(state): State<AppState>) -> Json<Vec<ImageStatus>> {
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
        let (image, running_digest) = match &container {
            Some(c) => {
                let repository = cl_docker::ImageRef::parse(&c.image).repository;
                let digest = match c.image_digest.as_deref() {
                    Some(image_id) => docker.digest_of_image(image_id, &repository).await.ok().flatten(),
                    None => None,
                };
                (Some(c.image.clone()), digest)
            }
            None => (None, None),
        };
        statuses.push(ImageStatus {
            id: assignment.instance.to_string(),
            image,
            running_digest,
            latest_digest: None,
            error: None,
        });
    }

    // Pass two: one registry lookup per distinct reference, however many
    // containers share it.
    let images: BTreeSet<String> = statuses.iter().filter_map(|s| s.image.clone()).collect();
    let mut latest: BTreeMap<String, Result<Option<String>, String>> = BTreeMap::new();
    for image in images {
        let credentials = registries.for_image(&image);
        let answer = docker
            .registry_digest(&image, credentials)
            .await
            .map_err(|e| e.to_string());
        latest.insert(image, answer);
    }

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
            Err(e) => BulkResult { id: row.id, ok: false, error: Some(e.to_string()) },
        });
    }

    Ok(Json(results))
}

/// Pull and rebuild, for the named instances or for every one with an update
/// available.
///
/// Sequential on purpose. Each update takes its container down and lets the
/// reconcile loop build a new one, so running the whole cluster's worth at
/// once would stop every replica of a service simultaneously — the one thing
/// a rolling update exists to avoid. A handful of images pulled in series is
/// slower and survivable; a cluster-wide simultaneous restart is neither.
async fn apply(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    body: Option<Json<InstanceSelection>>,
) -> Result<Json<Vec<BulkResult>>, ApiError> {
    let selection = body.map(|Json(b)| b).unwrap_or_default();
    let wanted: BTreeSet<String> = selection.instances.into_iter().collect();

    let mut results = Vec::new();
    for row in rows(&state).await {
        let chosen = if wanted.is_empty() { row.update_available } else { wanted.contains(&row.id) };
        if !chosen {
            continue;
        }
        let Ok(instance) = row.id.parse::<InstanceId>() else {
            continue;
        };
        let outcome = super::instances::update_instance(&state, instance).await;
        results.push(match outcome {
            Ok(()) => BulkResult { id: row.id, ok: true, error: None },
            Err(e) => BulkResult { id: row.id, ok: false, error: Some(e.to_string()) },
        });
    }

    Ok(Json(results))
}
