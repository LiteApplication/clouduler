use super::*;

/// Rerolls placement rather than restarting in place: moves the instance to wherever it
/// would land if freshly scheduled now (possibly the same node). An in-place restart would
/// retry forever on a node that can no longer host it.
///
/// Done inline, not by unassigning and waiting for the scheduler: the effect is immediate,
/// and `Unassign` would drop the `image_digest` pin (`Assign` resets it to `None`), letting a
/// pinned instance drift to whatever its tag now resolves to.
pub(super) async fn restart(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    restart_instance(&state, parse_id(&id)?).await?;
    Ok(StatusCode::OK)
}

pub(crate) async fn restart_instance(
    state: &AppState,
    instance: InstanceId,
) -> Result<(), ApiError> {
    let id = instance.to_string();
    let cluster_state = state.cluster.snapshot().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;

    let config = state.cluster_config.read().await;
    let colocate_proxy_hosts = config.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
    let desired = cl_sched::desired_instances(
        &cluster_state,
        &config.egress_networks(),
        &config.port_gated_networks(),
        colocate_proxy_hosts,
    );
    let served_drives = config.served_drives();
    let everywhere_networks = config.everywhere_networks();
    drop(config);

    let item = desired
        .into_iter()
        .find(|d| d.id == instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;

    // Placement leaves a valid assignment alone, so judge against a copy with it cleared.
    let mut probe = (*cluster_state).clone();
    probe.assignments.remove(&instance);
    let outcome = cl_sched::place::place(
        &probe,
        std::slice::from_ref(&item),
        &served_drives,
        &everywhere_networks,
    );
    let target = outcome
        .decisions
        .into_iter()
        .find_map(|d| match d {
            cl_sched::place::Decision::Place { node, .. } => Some(node),
            cl_sched::place::Decision::Remove { .. } => None,
        })
        .ok_or_else(|| {
            ApiError::Conflict("no node in the cluster can run this instance right now".to_string())
        })?;

    // Unassign + Assign to the same node leaves replicated state unchanged, so the node would
    // see nothing to do; removing the container makes reconcile rebuild it.
    if target == assignment.node {
        return remove_container_on(state, &instance, &assignment.node).await;
    }

    let response = state
        .cluster
        .submit(ClusterRequest::Unassign {
            instance: instance.clone(),
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }

    let response = state
        .cluster
        .submit(ClusterRequest::Assign {
            instance: instance.clone(),
            node: target,
            drives: item.drives,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }

    if let Some(digest) = assignment.image_digest {
        let response = state
            .cluster
            .submit(ClusterRequest::SetImageDigest { instance, digest })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::Conflict(msg));
        }
    }

    Ok(())
}

#[derive(Deserialize)]
pub(super) struct MoveRequest {
    node: String,
}

pub(super) async fn move_instance(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<MoveRequest>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.snapshot().await;
    let drives = required_drives(&cluster_state, &instance);

    let response = state
        .cluster
        .submit(ClusterRequest::BeginMigration {
            instance,
            to: body.node,
            drives,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::OK)
}

pub(super) async fn retry_migration(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.snapshot().await;
    let to = failed_move_target(&cluster_state, &instance)?;
    let drives = required_drives(&cluster_state, &instance);
    let response = state
        .cluster
        .submit(ClusterRequest::BeginMigration {
            instance,
            to,
            drives,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::OK)
}

pub(super) async fn dismiss_migration(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    let response = state
        .cluster
        .submit(ClusterRequest::ClearMigration { instance })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::OK)
}

pub(super) fn failed_move_target(
    state: &cl_raft::ClusterState,
    instance: &InstanceId,
) -> Result<String, ApiError> {
    let assignment = state
        .assignments
        .get(instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {instance}")))?;
    match &assignment.migration {
        Some(m) if matches!(m.phase, cl_raft::state::MigrationPhase::Failed { .. }) => {
            Ok(m.to.clone())
        }
        Some(_) => Err(ApiError::Conflict(format!("{instance} is still moving"))),
        None => Err(ApiError::Conflict(format!("{instance} has no failed move"))),
    }
}

/// Drives the instance's service mounts, read from the stored stack spec.
///
/// Checked before the migration is recorded: discovering a missing drive at `Switching`
/// is past the point of no return (the volume's primary has already moved). An unknown
/// stack or service yields an empty set; a removed stack fails on the missing assignment instead.
pub(super) fn required_drives(
    state: &cl_raft::ClusterState,
    instance: &InstanceId,
) -> std::collections::BTreeSet<String> {
    state
        .stacks
        .get(&instance.stack)
        .and_then(|stack| cl_config::StackFile::parse(&stack.spec).ok())
        .and_then(|file| {
            file.services
                .get(&instance.service)
                .map(Service::drives_used)
        })
        .unwrap_or_default()
}

#[derive(Serialize, Deserialize)]
pub(super) struct RebalanceMove {
    instance: String,
    from: String,
    to: String,
}

#[derive(Serialize)]
pub(super) struct RebalanceResponse {
    moves: Vec<RebalanceMove>,
}

/// Reserved config holding the latest rebalance, so any node can answer `GET /api/rebalance/last`.
pub(super) const LAST_REBALANCE_CONFIG: &str = "_clouduler_last_rebalance";

#[derive(Serialize, Deserialize)]
pub(super) struct LastRebalance {
    at: i64,
    moves: Vec<RebalanceMove>,
}

pub(super) async fn last_rebalance(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Option<LastRebalance>> {
    let cluster_state = state.cluster.snapshot().await;
    Json(
        cluster_state
            .configs
            .get(LAST_REBALANCE_CONFIG)
            .and_then(|c| serde_json::from_str(&c.content).ok()),
    )
}

/// Operator-requested rebalance; see `cl_sched::rebalance::plan`. Each move is a
/// `BeginMigration` submitted one at a time like the ui's "move" button, so placement and
/// drive checks apply; a move lost to a race does not fail the rest of the batch.
pub(super) async fn rebalance(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<Json<RebalanceResponse>, ApiError> {
    let cluster_state = state.cluster.snapshot().await;
    let config = state.cluster_config.read().await;
    let colocate_proxy_hosts = config.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
    let desired = cl_sched::desired_instances(
        &cluster_state,
        &config.egress_networks(),
        &config.port_gated_networks(),
        colocate_proxy_hosts,
    );
    let served_drives = config.served_drives();
    let everywhere_networks = config.everywhere_networks();
    drop(config);

    let live = crate::live_usage::gather(&state).await;
    let planned = cl_sched::rebalance::plan(
        &cluster_state,
        &desired,
        &served_drives,
        &everywhere_networks,
        &live,
    );

    let mut moves = Vec::new();
    for m in planned {
        let drives = required_drives(&cluster_state, &m.instance);
        let response = state
            .cluster
            .submit(ClusterRequest::BeginMigration {
                instance: m.instance.clone(),
                to: m.to.clone(),
                drives,
            })
            .await
            .map_err(ApiError::not_leader)?;
        match response.error {
            None => moves.push(RebalanceMove {
                instance: m.instance.to_string(),
                from: m.from,
                to: m.to,
            }),
            Some(error) => {
                warn!(instance = %m.instance, from = %m.from, to = %m.to, %error, "rebalance move was rejected")
            }
        }
    }
    let record = LastRebalance {
        at: crate::auth::oauth::now_secs(),
        moves,
    };
    // Best effort: only the ui's "last rebalance" line depends on it.
    if let Ok(content) = serde_json::to_string(&record) {
        if let Err(e) = state
            .cluster
            .submit(ClusterRequest::PutConfig {
                name: LAST_REBALANCE_CONFIG.to_string(),
                content,
                owner: None,
            })
            .await
        {
            warn!(error = %e, "could not record the rebalance");
        }
    }
    Ok(Json(RebalanceResponse {
        moves: record.moves,
    }))
}

#[derive(Deserialize)]
pub(super) struct PatchInstanceRequest {
    desired: DesiredState,
}

pub(super) async fn patch_instance(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchInstanceRequest>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    let response = state
        .cluster
        .submit(ClusterRequest::SetDesiredState {
            instance,
            desired: body.desired,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::OK)
}
