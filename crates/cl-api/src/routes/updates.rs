//! `/api/updates`: which containers are behind the image they deploy from.
//!
//! Three digests per instance: what the cluster has **pinned** (`Assignment::image_digest`),
//! what the container is **running** (the registry digest behind its local image id), and what
//! the registry currently **serves** for the reference it deploys from. An update is available
//! when served differs from running; comparing against the pin would be wrong both ways (an
//! unpinned instance has nothing to compare; a pinned one not yet rebuilt shows drift, not an update).
//!
//! Registry lookups are one round trip per distinct image, so the leader runs them on a timer
//! (`updates.check_interval`) and on Refresh (`check_images`), caching in
//! `ClusterState::image_checks`. GETs read that cache; only the running digest is gathered live.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_config::AutoUpdateMode;
use cl_raft::ids::InstanceId;
use cl_raft::state::{AutoUpdateOutcome, ClusterState, ImageCheck, UpdatePhase};
use cl_raft::Request as ClusterRequest;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::auth::internal::{InternalAuth, InternalBodyAuth};
use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::internal_peer;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/updates", get(list))
        .route("/api/updates/refresh", post(refresh))
        .route("/api/updates/pin", post(pin))
        .route("/api/updates/apply", post(apply))
        // Node-to-node only: one node's answer about its own containers.
        .route("/api/updates/internal/scan", get(internal_scan))
        .route("/api/updates/internal/apply", post(apply_internal))
}

const INTERNAL_SCAN_PATH: &str = "/api/updates/internal/scan";

/// The reference the stack file declares (`nginx:1.25`), not `ContainerState::image`, which
/// once pinned is `repo@sha256:...`. Asking the registry about a digest reference is a no-op
/// forever (immutable); the declared reference is the one that can move.
pub(crate) fn declared_image(state: &ClusterState, instance: &InstanceId) -> Option<String> {
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

/// Four, not eight: at eight ghcr.io and lscr.io answered 429 to a routine scan.
const REGISTRY_LOOKUP_CONCURRENCY: usize = 4;

/// Kept under the reverse proxy's 30s response timeout.
const SCAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

#[derive(Serialize, Deserialize, Clone)]
struct ImageStatus {
    id: String,
    image: Option<String>,
    running_digest: Option<String>,
    latest_digest: Option<String>,
    /// Per instance rather than failing the scan: one expired login must not blank the whole answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct UpdateRow {
    id: String,
    stack: String,
    service: String,
    node: String,
    image: Option<String>,
    running_digest: Option<String>,
    latest_digest: Option<String>,
    pinned_digest: Option<String>,
    update_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    checked_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_update: Option<AutoUpdateRow>,
    /// A pin exists but the container is not on it; fixed by rebuilding, not pulling.
    pin_drift: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct AutoUpdateRow {
    mode: &'static str,
    delay_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<AutoUpdateOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detected_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    apply_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<UpdateRow>> {
    Json(rows(&state).await)
}

const REFRESH_DEBOUNCE_SECS: i64 = 30;

static CHECKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Serialize)]
struct Refresh {
    started: bool,
}

async fn refresh(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> (StatusCode, Json<Refresh>) {
    let last = state.cluster.state().await.image_checks_at;
    let now = crate::alerts::unix_now();
    let recent = last != 0 && now - last < REFRESH_DEBOUNCE_SECS;
    let started = !recent && !CHECKING.load(std::sync::atomic::Ordering::SeqCst);
    if started {
        let worker = state.clone();
        tokio::spawn(async move { check_images(&worker).await });
    }
    (StatusCode::ACCEPTED, Json(Refresh { started }))
}

/// One lookup per distinct reference, a few at a time. A second call while one runs does nothing.
pub(crate) async fn check_images(state: &AppState) {
    use std::sync::atomic::Ordering;
    if CHECKING.swap(true, Ordering::SeqCst) {
        return;
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            CHECKING.store(false, Ordering::SeqCst);
        }
    }
    let _reset = Reset;

    let Some(docker) = state.docker.get().await else {
        warn!("cannot check image updates: no docker connection on this node");
        return;
    };
    let cluster_state = state.cluster.snapshot().await;
    let registries = super::registries::load(state).await;

    let images: BTreeSet<String> = cluster_state
        .assignments
        .keys()
        .filter_map(|i| declared_image(&cluster_state, i))
        .collect();
    let checks: BTreeMap<String, ImageCheck> = futures::stream::iter(images)
        .map(|image| {
            let docker = docker.clone();
            let credentials = registries.for_image(&image).cloned();
            async move {
                let answer = docker.registry_digest(&image, credentials.as_ref()).await;
                let check = ImageCheck {
                    latest_digest: answer.as_ref().ok().cloned().flatten(),
                    checked_at: crate::alerts::unix_now(),
                    error: answer.err().map(|e| e.to_string()),
                };
                (image, check)
            }
        })
        .buffer_unordered(REGISTRY_LOOKUP_CONCURRENCY)
        .collect()
        .await;
    drop(cluster_state);

    let at = crate::alerts::unix_now();
    if let Err(e) = state
        .cluster
        .submit(ClusterRequest::SetImageChecks { checks, at })
        .await
    {
        warn!(error = %e, "could not store the image check results");
        return;
    }

    let seen = rows(state)
        .await
        .into_iter()
        .filter_map(|row| {
            let id = row.id.parse::<InstanceId>().ok()?;
            Some((id, row.latest_digest, row.update_available))
        })
        .collect();
    crate::auto_update::record_detections(state, seen).await;
}

async fn rows(state: &AppState) -> Vec<UpdateRow> {
    let cluster_state = state.cluster.snapshot().await;

    let nodes: BTreeSet<String> = cluster_state
        .assignments
        .values()
        .map(|a| a.node.clone())
        // A drained node that is down is offline on purpose: scanning it only waits for a timeout.
        .filter(|node| {
            let drained = cluster_state
                .nodes
                .get(node)
                .is_some_and(|n| n.availability != cl_raft::state::Availability::Active);
            !(drained && cluster_state.unreachable.contains(node))
        })
        .collect();

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
            let check = status
                .and_then(|s| s.image.as_ref())
                .and_then(|image| cluster_state.image_checks.get(image));
            let latest = check.and_then(|c| c.latest_digest.clone());
            let pinned = assignment.image_digest.clone();

            // Both sides must be known: an unknown running digest is usually a locally built image,
            // and flagging those would make "update all" never stop asking. A pinned instance with no
            // container compares the pin instead: updating moves the pin without a restart.
            let update_available = match (&running, &latest, &pinned) {
                (Some(running), Some(latest), _) => running != latest,
                (None, Some(latest), Some(pinned)) => pinned != latest,
                _ => false,
            };
            let pin_drift = match (&running, &pinned) {
                (Some(running), Some(pinned)) => running != pinned,
                _ => false,
            };

            let auto_update =
                auto_update_row(&cluster_state, &assignment.instance, latest.as_deref());
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
                checked_at: check.map(|c| c.checked_at),
                auto_update,
                pin_drift,
                error: check.and_then(|c| c.error.clone()),
            }
        })
        .collect();

    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

fn auto_update_row(
    state: &ClusterState,
    instance: &InstanceId,
    latest: Option<&str>,
) -> Option<AutoUpdateRow> {
    let auto = crate::auto_update::setting(state, instance);
    let mode = match auto.mode {
        AutoUpdateMode::Off => return None,
        AutoUpdateMode::Apply => "apply",
        AutoUpdateMode::Notify => "notify",
    };
    let record = state
        .auto_updates
        .get(instance)
        .filter(|r| latest == Some(r.digest.as_str()));
    Some(AutoUpdateRow {
        mode,
        delay_secs: auto.delay.as_secs(),
        outcome: record.map(|r| r.outcome),
        detected_at: record.map(|r| r.detected_at),
        apply_at: record.and_then(|r| crate::auto_update::apply_at(auto, r)),
        error: record.and_then(|r| r.error.clone()),
    })
}

/// An unreachable node contributes nothing; its rows show an unknown running digest.
async fn scan_node(state: &AppState, node: &str) -> Vec<ImageStatus> {
    if node == state.node_name {
        return scan_local(state).await;
    }
    match internal_peer::fetch_json_within::<Vec<ImageStatus>>(
        state,
        node,
        INTERNAL_SCAN_PATH,
        SCAN_TIMEOUT,
    )
    .await
    {
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

async fn scan_local(state: &AppState) -> Vec<ImageStatus> {
    let Some(docker) = state.docker.get().await else {
        return Vec::new();
    };
    let cluster_state = state.cluster.snapshot().await;

    let mut statuses = Vec::new();
    for assignment in cluster_state.assignments.values() {
        if assignment.node != state.node_name {
            continue;
        }
        let name = assignment.instance.container_name();
        let container = docker.inspect(&name).await.ok().flatten();
        // The registry's current digest, when known: an image id can carry several digests and
        // the one matching the registry is the honest answer to "is this up to date".
        let served = declared_image(&cluster_state, &assignment.instance)
            .and_then(|image| cluster_state.image_checks.get(&image))
            .and_then(|check| check.latest_digest.clone());
        let running_digest = match &container {
            Some(c) => {
                let repository = cl_docker::ImageRef::parse(&c.image).repository;
                match c.image_digest.as_deref() {
                    Some(image_id) => docker
                        .digest_of_image_preferring(image_id, &repository, served.as_deref())
                        .await
                        .ok()
                        .flatten(),
                    None => None,
                }
            }
            None => None,
        };
        // The declared reference, not `container.image`: once pinned that is an immutable digest.
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

    statuses
}

#[derive(Deserialize, Default)]
pub(crate) struct InstanceSelection {
    #[serde(default)]
    pub(crate) instances: Vec<String>,
}

#[derive(Serialize)]
struct BulkResult {
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Only pins instances with no pin yet: repointing a deliberate pin at whatever is running
/// would undo a rollback. Unknown running digests are skipped.
async fn pin(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    body: Option<Json<InstanceSelection>>,
) -> Result<Json<Vec<BulkResult>>, ApiError> {
    let selection = body.map(|Json(b)| b).unwrap_or_default();
    let wanted: BTreeSet<String> = selection.instances.into_iter().collect();

    let mut results = Vec::new();
    for row in rows(&state).await {
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

#[derive(Serialize)]
pub(crate) struct Accepted {
    accepted: Vec<String>,
}

/// Answers `202` once the work is scheduled: a pull can take far longer than the reverse
/// proxy's 30s, and the resulting gateway timeout on a healthy update made operators click
/// again, stacking a second pull. Progress arrives via `ClusterState::instance_updates`.
///
/// Per node, one download and one restart at a time (`roll_batch`); replicas of one service
/// never restart together.
async fn apply(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    body: Option<Json<InstanceSelection>>,
) -> Result<(StatusCode, Json<Accepted>), ApiError> {
    let selection = body.map(|Json(b)| b).unwrap_or_default();
    Ok(apply_selection(&state, selection).await)
}

/// `clouduler update`: same trigger as the ui, over internal auth. Scheduling pulls acts on
/// the daemon, not raft state.
async fn apply_internal(
    auth: InternalBodyAuth,
    State(state): State<AppState>,
    body: Bytes,
) -> Result<(StatusCode, Json<Accepted>), ApiError> {
    // The signature covers a hash of the body: check it, or a replayer could swap the selection
    // (an empty one means every instance).
    auth.check(&body)?;
    let selection = if body.is_empty() {
        InstanceSelection::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::BadRequest(format!("invalid update selection: {e}")))?
    };
    Ok(apply_selection(&state, selection).await)
}

pub(crate) async fn apply_selection(
    state: &AppState,
    selection: InstanceSelection,
) -> (StatusCode, Json<Accepted>) {
    // Scan only when no selection was given; scanning regardless made a single-row update pay
    // for a cluster-wide registry sweep.
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

    // Queue everything in the replicated record before answering so every browser sees the
    // whole batch at once. An instance already mid-update is left alone.
    let cluster_state = state.cluster.snapshot().await;
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
        queued.push((
            instance,
            assignment.node.clone(),
            assignment.image_digest.clone(),
        ));
    }
    drop(cluster_state);

    let accepted: Vec<String> = queued.iter().map(|(i, _, _)| i.to_string()).collect();

    let worker = state.clone();
    tokio::spawn(async move { roll_batch(worker, queued).await });

    (StatusCode::ACCEPTED, Json(Accepted { accepted }))
}

type QueuedUpdate = (InstanceId, String, Option<String>);

/// Each node works through its instances in parallel; within a node one image downloads
/// while one instance restarts. Two instances of a service never restart together.
///
/// The first failure stops the batch (a bad image would otherwise take down every replica):
/// the failed instance returns to its previous digest (`rollback_target`), instances not yet
/// restarting are failed as cancelled, and restarts underway on other nodes finish on their own.
async fn roll_batch(worker: AppState, queued: Vec<QueuedUpdate>) {
    let mut lanes: std::collections::BTreeMap<String, Vec<QueuedUpdate>> = Default::default();
    let mut pending = std::collections::HashMap::new();
    for item in queued {
        pending.insert(item.0.clone(), item.1.clone());
        lanes.entry(item.1.clone()).or_default().push(item);
    }
    let batch = Arc::new(Batch {
        worker,
        inner: parking_lot::Mutex::new(BatchState {
            failed_by: None,
            pending,
        }),
        services: Default::default(),
    });

    let keepalive = tokio::spawn(keep_queued_alive(batch.clone()));
    let lanes: Vec<_> = lanes
        .into_values()
        .map(|items| roll_node(batch.clone(), items))
        .collect();
    futures::future::join_all(lanes).await;
    keepalive.abort();
    // The cached registry answers predate this batch; without a fresh look the page keeps
    // listing what was just updated as behind and "update all" would redo it.
    check_images(&batch.worker).await;
}

struct Batch {
    worker: AppState,
    inner: parking_lot::Mutex<BatchState>,
    services: parking_lot::Mutex<ServiceLocks>,
}

type ServiceLocks = std::collections::HashMap<(String, String), Arc<tokio::sync::Mutex<()>>>;

struct BatchState {
    failed_by: Option<InstanceId>,
    pending: std::collections::HashMap<InstanceId, String>,
}

impl Batch {
    fn halted(&self) -> bool {
        self.inner.lock().failed_by.is_some()
    }

    fn claim(&self, instance: &InstanceId) -> bool {
        self.inner.lock().pending.remove(instance).is_some()
    }

    fn service_lock(&self, instance: &InstanceId) -> Arc<tokio::sync::Mutex<()>> {
        self.services
            .lock()
            .entry((instance.stack.clone(), instance.service.clone()))
            .or_default()
            .clone()
    }

    async fn fail(
        &self,
        instance: &InstanceId,
        node: &str,
        pin_before: Option<&str>,
        reason: &str,
    ) {
        let cancelled: Vec<(InstanceId, String)> = {
            let mut inner = self.inner.lock();
            inner.failed_by.get_or_insert_with(|| instance.clone());
            inner.pending.drain().collect()
        };

        warn!(%instance, %reason, "update failed, halting the rolling update");
        let worker = &self.worker;
        let cluster_state = worker.cluster.state().await;
        let pin_now = cluster_state
            .assignments
            .get(instance)
            .and_then(|a| a.image_digest.clone());
        let previous = cluster_state.previous_image_digests.get(instance).cloned();
        drop(cluster_state);
        let note = match rollback_target(pin_before, pin_now.as_deref(), previous) {
            Some(digest) => {
                match super::instances::roll_back_instance(worker, instance, digest.clone()).await {
                    Ok(()) => format!("rolled back to {digest}"),
                    Err(e) => format!("could not roll back to {digest}: {e}"),
                }
            }
            None => "nothing to roll back".to_string(),
        };
        cl_sched::progress::submit(
            &worker.cluster,
            instance,
            node,
            UpdatePhase::Failed,
            Some(format!("{reason} ({note})")),
        )
        .await;

        for (cancelled, cancelled_node) in cancelled {
            cl_sched::progress::submit(
                &worker.cluster,
                &cancelled,
                &cancelled_node,
                UpdatePhase::Failed,
                Some(format!(
                    "cancelled: the update of {instance} failed, so the rolling update stopped"
                )),
            )
            .await;
        }
    }
}

/// A downloader and a restarter joined by a one-slot channel.
async fn roll_node(batch: Arc<Batch>, items: Vec<QueuedUpdate>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(QueuedUpdate, String)>(1);

    let downloader = async {
        for item in items {
            if batch.halted() {
                break;
            }
            let (instance, node, pin_before) = &item;
            match super::instances::pull_instance(&batch.worker, instance).await {
                Ok(digest) => {
                    // Downloaded, waiting for the restart slot: the record should say waiting, not downloading.
                    cl_sched::progress::submit(
                        &batch.worker.cluster,
                        instance,
                        node,
                        UpdatePhase::Queued,
                        None,
                    )
                    .await;
                    if tx.send((item, digest)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    if let ApiError::NotFound(why) = &e {
                        warn!(%instance, %why, "instance disappeared before its update");
                        batch.claim(instance);
                        continue;
                    }
                    if batch.claim(instance) {
                        batch
                            .fail(instance, node, pin_before.as_deref(), &e.to_string())
                            .await;
                    }
                    break;
                }
            }
        }
        drop(tx);
    };

    let restarter = async {
        while let Some(((instance, node, pin_before), digest)) = rx.recv().await {
            let lock = batch.service_lock(&instance);
            let _service = lock.lock().await;
            if !batch.claim(&instance) {
                continue;
            }
            let failure = match super::instances::replace_instance(&batch.worker, &instance, digest)
                .await
            {
                // `replace_instance` returns once the old container is gone; hold the restart slot and the
                // service until the reconciler has brought the new one up.
                Ok(()) => match wait_for_update(&batch.worker, &instance).await {
                    Some((UpdatePhase::Started, _)) => {
                        info!(%instance, "updated");
                        None
                    }
                    Some((_, error)) => Some(error.unwrap_or_else(|| "the update failed".into())),
                    None => Some("gave up waiting for the update to finish".to_string()),
                },
                Err(ApiError::NotFound(why)) => {
                    warn!(%instance, %why, "instance disappeared before its update");
                    continue;
                }
                Err(e) => Some(e.to_string()),
            };
            if let Some(reason) = failure {
                batch
                    .fail(&instance, &node, pin_before.as_deref(), &reason)
                    .await;
            }
        }
    };

    tokio::join!(downloader, restarter);
}

/// Keeps waiting records from looking abandoned (`Scheduler` fails ones queued too long).
async fn keep_queued_alive(batch: Arc<Batch>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        let waiting: Vec<(InstanceId, String)> = batch
            .inner
            .lock()
            .pending
            .iter()
            .map(|(i, n)| (i.clone(), n.clone()))
            .collect();
        let cluster_state = batch.worker.cluster.state().await;
        for (instance, node) in waiting {
            let still_queued = cluster_state
                .instance_updates
                .get(&instance)
                .is_some_and(|u| u.phase == UpdatePhase::Queued);
            if still_queued {
                cl_sched::progress::submit(
                    &batch.worker.cluster,
                    &instance,
                    &node,
                    UpdatePhase::Queued,
                    None,
                )
                .await;
            }
        }
    }
}

/// The digest to restore after a failed update. `pin_before == pin_now` means the update never
/// pinned (failed pull), so `previous`, possibly left from an older update, must not be applied.
/// `None` also when no earlier digest was ever recorded.
fn rollback_target(
    pin_before: Option<&str>,
    pin_now: Option<&str>,
    previous: Option<String>,
) -> Option<String> {
    if pin_before == pin_now {
        return None;
    }
    previous
}

const UPDATE_FINISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// `None` on timeout or if the record disappeared (instance removed or moved).
pub(crate) async fn wait_for_update(
    state: &AppState,
    instance: &InstanceId,
) -> Option<(UpdatePhase, Option<String>)> {
    let deadline = tokio::time::Instant::now() + UPDATE_FINISH_TIMEOUT;
    loop {
        let (phase, error) = state
            .cluster
            .state()
            .await
            .instance_updates
            .get(instance)
            .map(|u| (u.phase, u.error.clone()))?;
        if phase.is_terminal() {
            return Some((phase, error));
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod rollback_tests {
    use super::rollback_target;

    #[test]
    fn rolls_back_only_when_the_update_changed_the_pin() {
        let previous = Some("sha256:old".to_string());
        // Pull failed before pinning: the stale "previous" is not ours.
        assert_eq!(
            rollback_target(Some("sha256:a"), Some("sha256:a"), previous.clone()),
            None
        );
        assert_eq!(rollback_target(None, None, previous.clone()), None);
        // Pinned by the update.
        assert_eq!(
            rollback_target(Some("sha256:old"), Some("sha256:new"), previous.clone()),
            previous
        );
        assert_eq!(
            rollback_target(None, Some("sha256:new"), previous.clone()),
            previous
        );
        // Changed but nothing recorded to go back to.
        assert_eq!(rollback_target(None, Some("sha256:new"), None), None);
    }
}
