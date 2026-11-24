use super::*;

#[derive(Serialize, Deserialize)]
pub(crate) struct InstanceSummary {
    pub(crate) id: String,
    pub(crate) node: String,
    pub(crate) desired: String,
    pub(crate) state: String,
    pub(crate) running: Option<bool>,
    pub(crate) health: String,
    pub(crate) image: Option<String>,
    pub(crate) image_digest: Option<String>,
    /// Registry digest of what the container is actually running, vs `image_digest`
    /// (the pin). `None` when remote, no container, or a locally built image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) running_digest: Option<String>,
    pub(crate) started_at: Option<String>,
    pub(crate) migration: Option<MigrationSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) migration_problem: Option<crate::migration_problem::MigrationProblem>,
    /// What is happening now (redeploy sub-step or migration phase) with a
    /// `(step, total)` pair for a progress bar; see `compose_progress`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) step: Option<(u8, u8)>,
    /// Set only when `state` is `"pending_placement"`: why no node fits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) placement_error: Option<String>,
    /// The reverse proxy's own most recent active health check of this
    /// instance's service, when it publishes a route at all. `None` means
    /// either nothing is published for it or no check has landed yet, not
    /// that it's healthy — distinct from an empty `error` on a `Some`, which
    /// does mean healthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) proxy: Option<ProxyHealthSummary>,
    // Everything below crosses nodes inside `internal_summary`, so each is `default`:
    // an older peer leaves them out.
    /// Exit code of the container's last run, when it has stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) oom_killed: bool,
    #[serde(default)]
    pub(crate) restart_count: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) restart_limit: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) memory_limit_bytes: Option<i64>,
    #[serde(default)]
    pub(crate) crash_moves: u32,
}

pub(super) fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ProxyHealthSummary {
    pub(crate) host: String,
    pub(crate) checked_at: String,
    /// `None` when the last check reached the backend; else the reason, shaped like the proxy's "bad gateway" body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct MigrationSummary {
    pub(crate) to: String,
    pub(crate) phase: String,
}

/// State names the ui and test harness match on. Strings, not an enum: "cannot see it from here" differs from "not running".
pub(super) const STATE_RUNNING: &str = "running";
pub(super) const STATE_STOPPED: &str = "stopped";
pub(super) const STATE_STOPPING: &str = "stopping";
pub(super) const STATE_RESTARTING: &str = "restarting";
pub(super) const STATE_EXITED: &str = "exited";
pub(super) const STATE_PULLING: &str = "pulling";
pub(super) const STATE_WAITING: &str = "waiting";
pub(super) const STATE_PENDING_PLACEMENT: &str = "pending_placement";
pub(super) const STATE_UNKNOWN: &str = "unknown";

pub(super) fn internal_summary_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/summary")
}

/// Local instances come from docker, remote ones from the owning node over the internal api;
/// an unreachable node degrades to `summarize_local`'s unknown answer instead of failing the list.
pub(crate) async fn summarize(
    state: &AppState,
    cluster_state: &cl_raft::ClusterState,
    assignment: &cl_raft::Assignment,
) -> InstanceSummary {
    if assignment.node == state.node_name {
        return summarize_local(state, cluster_state, assignment).await;
    }
    match internal_peer::fetch_json::<InstanceSummary>(
        state,
        &assignment.node,
        &internal_summary_path(&assignment.instance),
    )
    .await
    {
        Ok(summary) => summary,
        Err(e) => {
            warn!(node = %assignment.node, instance = %assignment.instance, error = %e, "could not fetch instance summary from its node");
            summarize_local(state, cluster_state, assignment).await
        }
    }
}

/// Docker inspection, valid only on the node running the container; also the "unknown" answer for remote assignments.
pub(super) async fn summarize_local(
    state: &AppState,
    cluster_state: &cl_raft::ClusterState,
    assignment: &cl_raft::Assignment,
) -> InstanceSummary {
    let local = assignment.node == state.node_name;
    // Unreachable docker reads as unknown, not "stopped".
    let container = match (local, state.docker.get().await) {
        (true, Some(docker)) => docker
            .inspect(&assignment.instance.container_name())
            .await
            .ok()
            .flatten(),
        _ => None,
    };

    let running = container.as_ref().map(|c| c.running);
    // Looked up by image id, not tag: the tag has usually moved on, which is the drift this exposes.
    let running_digest = match (&container, state.docker.get().await) {
        (Some(c), Some(docker)) => {
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
        _ => None,
    };
    let proxy = proxy_health_of(state, assignment);
    let waiting_for = if local && container.is_none() {
        cl_sched::deps::unmet_dependency(cluster_state, &assignment.instance)
    } else {
        None
    };
    let instance_state = if !local {
        STATE_UNKNOWN
    } else {
        match &container {
            Some(c) => run_state_to_summary(c.status, assignment.desired),
            None if state
                .pulling
                .read()
                .await
                .contains_key(&assignment.instance) =>
            {
                STATE_PULLING
            }
            None if assignment.desired == DesiredState::Running && waiting_for.is_some() => {
                STATE_WAITING
            }
            None => STATE_STOPPED,
        }
    };

    let migration_sync = assignment.migration.as_ref().and_then(|_| {
        state
            .migration_sync
            .read()
            .get(&assignment.instance)
            .copied()
    });
    // Replicated, so real even when the node could not be asked.
    let migration_problem = crate::migration_problem::classify(cluster_state, &assignment.instance);
    let (mut reason, step) = compose_progress(
        cluster_state.instance_updates.get(&assignment.instance),
        assignment
            .migration
            .as_ref()
            .map(|m| (&m.phase, migration_sync)),
    );

    if let (Some(problem), Some(m)) = (&migration_problem, &assignment.migration) {
        reason = Some(format!("move to {} failed: {}", m.to, problem.title));
    }

    if instance_state == STATE_WAITING {
        reason = waiting_for.map(|dep| format!("waiting for {dep}"));
    }

    InstanceSummary {
        id: assignment.instance.to_string(),
        node: assignment.node.clone(),
        desired: format!("{:?}", assignment.desired).to_lowercase(),
        state: instance_state.to_string(),
        running,
        health: container
            .as_ref()
            .and_then(|c| c.health.as_ref())
            .map(|h| format!("{h:?}").to_lowercase())
            .unwrap_or_else(|| "none".to_string()),
        image: container.as_ref().map(|c| c.image.clone()),
        image_digest: assignment.image_digest.clone(),
        running_digest,
        started_at: container.as_ref().and_then(|c| c.started_at.clone()),
        migration: assignment.migration.as_ref().map(|m| MigrationSummary {
            to: m.to.clone(),
            phase: phase_name(&m.phase).to_string(),
        }),
        migration_problem,
        placement_error: None,
        proxy,
        reason,
        step,
        exit_code: container.as_ref().and_then(|c| c.exit_code),
        oom_killed: container.as_ref().is_some_and(|c| c.oom_killed),
        restart_count: container.as_ref().map_or(0, |c| c.restart_count),
        restart_limit: container.as_ref().and_then(|c| c.restart_limit),
        finished_at: container.as_ref().and_then(|c| c.finished_at.clone()),
        memory_limit_bytes: container.as_ref().and_then(|c| c.memory_limit_bytes),
        crash_moves: cluster_state
            .crash_moves
            .get(&assignment.instance)
            .map_or(0, |m| m.len() as u32),
    }
}

/// Keyed `stack.service`: one route load balances all replicas of a service.
pub(super) fn proxy_health_of(
    state: &AppState,
    assignment: &cl_raft::Assignment,
) -> Option<ProxyHealthSummary> {
    let key = format!(
        "{}.{}",
        assignment.instance.stack, assignment.instance.service
    );
    let health = state.proxy_health.get(&key)?;
    Some(ProxyHealthSummary {
        host: health.host,
        checked_at: humantime::format_rfc3339_seconds(health.checked_at).to_string(),
        error: health.error,
    })
}

/// `Exited` means "crashed" when the cluster wanted it running, "stopped" otherwise.
pub(super) fn run_state_to_summary(status: cl_docker::RunState, desired: DesiredState) -> &'static str {
    use cl_docker::RunState;
    match status {
        RunState::Running => STATE_RUNNING,
        RunState::Restarting => STATE_RESTARTING,
        RunState::Stopping | RunState::Removing => STATE_STOPPING,
        RunState::Exited | RunState::Dead => {
            if desired == DesiredState::Running {
                STATE_EXITED
            } else {
                STATE_STOPPED
            }
        }
        RunState::Created | RunState::Paused | RunState::Unknown => STATE_STOPPED,
    }
}

pub(super) fn compose_progress(
    update: Option<&cl_raft::InstanceUpdate>,
    migration: Option<(
        &cl_raft::state::MigrationPhase,
        Option<cl_drive::MigrationSyncProgress>,
    )>,
) -> (Option<String>, Option<(u8, u8)>) {
    if let Some((phase, sync_progress)) = migration {
        return compose_migration_reason(phase, sync_progress);
    }
    let Some(update) = update else {
        return (None, None);
    };
    use cl_raft::UpdatePhase;
    let (label, step) = match update.phase {
        UpdatePhase::Queued => return (Some("update queued".to_string()), None),
        UpdatePhase::Started => return (None, None),
        UpdatePhase::Failed => {
            let error = update.error.as_deref().unwrap_or("unknown error");
            return (Some(format!("update failed: {error}")), None);
        }
        UpdatePhase::Pulling => ("downloading image", 1),
        UpdatePhase::Stopping => ("stopping old container", 2),
        UpdatePhase::Starting => ("starting new container", 3),
    };
    (
        Some(format!(
            "updating ({label}, step {step}/{UPDATE_TOTAL_STEPS})"
        )),
        Some((step, UPDATE_TOTAL_STEPS)),
    )
}

pub(super) const UPDATE_TOTAL_STEPS: u8 = 4;

/// `Failed` carries a reason, kept out of the name; it lives in `migration_problem`.
pub(super) fn phase_name(phase: &cl_raft::state::MigrationPhase) -> &'static str {
    use cl_raft::state::MigrationPhase::*;
    match phase {
        Stopping => "stopping",
        Syncing => "syncing",
        Verifying => "verifying",
        Switching => "switching",
        Starting => "starting",
        Done => "done",
        Failed { .. } => "failed",
    }
}

pub(super) fn compose_migration_reason(
    phase: &cl_raft::state::MigrationPhase,
    sync_progress: Option<cl_drive::MigrationSyncProgress>,
) -> (Option<String>, Option<(u8, u8)>) {
    if *phase == cl_raft::state::MigrationPhase::Syncing {
        if let Some(p) = sync_progress {
            return (
                Some(format!(
                    "migrating (syncing volume {}/{}, {}/{} files)",
                    p.volume_index, p.volume_count, p.files_done, p.files_total
                )),
                Some((p.volume_index as u8, p.volume_count as u8)),
            );
        }
    }
    match phase {
        cl_raft::state::MigrationPhase::Done => (None, None),
        cl_raft::state::MigrationPhase::Failed { .. } => (Some("move failed".to_string()), None),
        _ => (Some(format!("migrating ({})", phase_name(phase))), None),
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;
    use cl_raft::state::MigrationPhase;

    fn update(phase: cl_raft::UpdatePhase) -> cl_raft::InstanceUpdate {
        cl_raft::InstanceUpdate {
            node: "n1".into(),
            phase,
            started_at: 0,
            at: 0,
            error: None,
        }
    }

    #[test]
    fn update_reason_names_the_step() {
        let (reason, step) = compose_progress(Some(&update(cl_raft::UpdatePhase::Stopping)), None);
        assert_eq!(
            reason.as_deref(),
            Some("updating (stopping old container, step 2/4)")
        );
        assert_eq!(step, Some((2, 4)));
    }

    #[test]
    fn a_started_update_is_steady_state() {
        assert_eq!(
            compose_progress(Some(&update(cl_raft::UpdatePhase::Started)), None),
            (None, None)
        );
    }

    #[test]
    fn migration_reason_without_sync_progress_names_just_the_phase() {
        let (reason, step) = compose_migration_reason(&MigrationPhase::Verifying, None);
        assert_eq!(reason.as_deref(), Some("migrating (verifying)"));
        assert_eq!(step, None);
    }

    #[test]
    fn migration_reason_with_sync_progress_names_the_volume_and_file_counts() {
        let progress = cl_drive::MigrationSyncProgress {
            volume_index: 1,
            volume_count: 2,
            files_done: 5,
            files_total: 12,
        };
        let (reason, step) = compose_migration_reason(&MigrationPhase::Syncing, Some(progress));
        assert_eq!(
            reason.as_deref(),
            Some("migrating (syncing volume 1/2, 5/12 files)")
        );
        assert_eq!(step, Some((1, 2)));
    }
}

pub(crate) async fn progress_snapshot(
    state: &AppState,
    cluster_state: &cl_raft::ClusterState,
) -> Vec<(String, crate::stream::InstanceProgressEvent)> {
    let summaries = futures::future::join_all(
        cluster_state
            .assignments
            .values()
            .map(|a| summarize(state, cluster_state, a)),
    )
    .await;
    summaries
        .into_iter()
        .map(|s| {
            (
                s.id.clone(),
                crate::stream::InstanceProgressEvent {
                    instance_id: s.id,
                    state: s.state,
                    reason: s.reason,
                    step: s.step,
                },
            )
        })
        .collect()
}

pub(super) async fn instance_events(_identity: Identity, State(state): State<AppState>) -> impl IntoResponse {
    stream::instance_progress_events(state)
}

pub(super) async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<InstanceSummary>> {
    Json(list_all(&state).await)
}

pub(crate) async fn list_all(state: &AppState) -> Vec<InstanceSummary> {
    let cluster_state = state.cluster.snapshot().await;
    // Concurrent so one unreachable node (`FETCH_TIMEOUT`) does not serialize the whole list.
    let mut out = futures::future::join_all(
        cluster_state
            .assignments
            .values()
            .map(|a| summarize(state, &cluster_state, a)),
    )
    .await;
    out.extend(pending_placements(state, &cluster_state).await);
    out
}

/// Computed live from the scheduler's placement logic; nothing in `assignments` represents these.
pub(super) async fn pending_placements(
    state: &AppState,
    cluster_state: &cl_raft::ClusterState,
) -> Vec<InstanceSummary> {
    let config = state.cluster_config.read().await;
    let colocate_proxy_hosts = config.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
    let desired = cl_sched::desired_instances(
        cluster_state,
        &config.egress_networks(),
        &config.port_gated_networks(),
        colocate_proxy_hosts,
    );
    let served_drives = config.served_drives();
    let everywhere_networks = config.everywhere_networks();
    drop(config);

    cl_sched::place(
        cluster_state,
        &desired,
        &served_drives,
        &everywhere_networks,
    )
    .unplaceable
    .into_iter()
    .map(|u| InstanceSummary {
        id: u.instance.to_string(),
        node: String::new(),
        desired: "running".to_string(),
        state: STATE_PENDING_PLACEMENT.to_string(),
        running: None,
        health: "none".to_string(),
        image: None,
        image_digest: None,
        running_digest: None,
        started_at: None,
        migration: None,
        migration_problem: None,
        reason: None,
        step: None,
        placement_error: Some(u.reason),
        proxy: None,
        exit_code: None,
        oom_killed: false,
        restart_count: 0,
        restart_limit: None,
        finished_at: None,
        memory_limit_bytes: None,
        crash_moves: 0,
    })
    .collect()
}

pub(super) async fn detail(
    _identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstanceSummary>, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.snapshot().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;

    Ok(Json(summarize(&state, &cluster_state, assignment).await))
}

/// Always answers about this node's docker; the caller already checked the assignment.
pub(super) async fn internal_summary(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstanceSummary>, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.snapshot().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;
    Ok(Json(
        summarize_local(&state, &cluster_state, assignment).await,
    ))
}

/// This node's own live usage for rebalancing; a node only answers for itself.
pub(super) async fn internal_live_usage(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Json<crate::live_usage::NodeLiveUsage> {
    Json(crate::live_usage::local(&state).await)
}

/// Live usage of every reachable node, gathered by this node, for `clouduler rebalance`.
pub(super) async fn internal_cluster_live_usage(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Json<cl_sched::rebalance::LiveUsage> {
    Json(crate::live_usage::gather(&state).await)
}
