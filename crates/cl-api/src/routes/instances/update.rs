use super::*;

pub(super) fn internal_update_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/update")
}

pub(super) async fn update(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    update_instance_guarded(&state, parse_id(&id)?).await?;
    Ok(StatusCode::OK)
}

/// `update_instance`, refusing with a conflict when one is already running.
pub(crate) async fn update_instance_guarded(
    state: &AppState,
    instance: InstanceId,
) -> Result<(), ApiError> {
    // A second update would pull again and remove the container the first is waiting on.
    // The bulk path (`updates::apply_selection`) queues its own record first.
    let in_flight = state
        .cluster
        .state()
        .await
        .instance_updates
        .get(&instance)
        .is_some_and(|u| !u.phase.is_terminal());
    if in_flight {
        return Err(ApiError::Conflict(format!(
            "an update of {instance} is already in progress"
        )));
    }
    update_instance(state, instance).await
}

/// Re-pins `digest` and removes the container so reconcile rebuilds on it: how a failed rolling update undoes itself.
pub(crate) async fn roll_back_instance(
    state: &AppState,
    instance: &InstanceId,
    digest: String,
) -> Result<(), ApiError> {
    state
        .cluster
        .submit(ClusterRequest::SetImageDigest {
            instance: instance.clone(),
            digest,
        })
        .await
        .map_err(ApiError::not_leader)?;
    let node = state
        .cluster
        .state()
        .await
        .assignments
        .get(instance)
        .map(|a| a.node.clone())
        .ok_or_else(|| ApiError::NotFound(format!("no instance {instance}")))?;
    remove_container_on(state, instance, &node).await
}

pub(crate) async fn remove_container_on(
    state: &AppState,
    instance: &InstanceId,
    node: &str,
) -> Result<(), ApiError> {
    if node != state.node_name {
        let path = format!("/api/instances/internal/{instance}/remove-container");
        return internal_peer::post_empty(state, node, &path).await;
    }
    remove_container(state, instance).await
}

pub(super) async fn remove_container(
    state: &AppState,
    instance: &InstanceId,
) -> Result<(), ApiError> {
    let name = instance.container_name();
    let docker = state.docker().await?;
    // Reconcile's `failover.stop_grace`, not docker's 10s, which is short for a database.
    let stop_grace = state.cluster_config.read().await.failover.stop_grace;
    docker.stop(&name, stop_grace).await.ok();
    docker
        .remove(&name, true)
        .await
        .map_err(ApiError::internal)?;
    Ok(())
}

pub(super) async fn internal_remove_container(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    remove_container(&state, &parse_id(&id)?).await?;
    Ok(StatusCode::OK)
}

/// Shared with `routes::updates`, which drives it across a list of instances.
pub(crate) async fn update_instance(
    state: &AppState,
    instance: InstanceId,
) -> Result<(), ApiError> {
    let instance = &instance;
    forward_or_local(
        state,
        instance,
        update_local(state, instance),
        |node| async move {
            internal_peer::post_empty(state, &node, &internal_update_path(instance)).await
        },
    )
    .await
}

/// First half of an update: downloads the image and returns the digest to pin; the old
/// container keeps serving. Split so a rolling update can pull the next instance during a restart.
pub(crate) async fn pull_instance(
    state: &AppState,
    instance: &InstanceId,
) -> Result<String, ApiError> {
    forward_or_local(
        state,
        instance,
        pull_local(state, instance),
        |node| async move {
            let path = format!("/api/instances/internal/{instance}/pull");
            let pulled: Pulled = internal_peer::post_json(state, &node, &path).await?;
            Ok(pulled.digest)
        },
    )
    .await
}

pub(crate) async fn replace_instance(
    state: &AppState,
    instance: &InstanceId,
    digest: String,
) -> Result<(), ApiError> {
    forward_or_local(
        state,
        instance,
        replace_local(state, instance, digest.clone()),
        |node| async move {
            let query = serde_urlencoded::to_string(Pulled { digest }).unwrap_or_default();
            let path = format!("/api/instances/internal/{instance}/replace?{query}");
            internal_peer::post_empty(state, &node, &path).await
        },
    )
    .await
}

#[derive(Deserialize, Serialize)]
pub(super) struct Pulled {
    digest: String,
}

/// Pulls the image here, records the digest (a raft write via `submit`), and removes the old
/// container so reconcile builds a new one on the new bits.
///
/// Removing rather than restarting is the point: `docker restart` reuses the container, bound
/// to its original image id, so the update would silently do nothing. Nor does a changed pin
/// trigger `Action::Recreate`, whose revision comes from the stack revision and mount shape
/// (`revision_with_mounts`), not the image.
///
/// Ordering against reconcile's `Action::Create` does not matter: if it runs before the pin
/// applies, it creates from the tag, which the pull just repointed at the same digest.
/// Each step is recorded in `ClusterState::instance_updates`; the reconciler records `Started`.
pub(super) async fn update_local(state: &AppState, instance: &InstanceId) -> Result<(), ApiError> {
    with_failure_report(state, instance, |progress| async move {
        update_local_steps(state, instance, &progress).await
    })
    .await
}

/// Runs `steps` with an update reporter, recording a `Failed` phase for the instance if it errs.
async fn with_failure_report<T, Fut>(
    state: &AppState,
    instance: &InstanceId,
    steps: impl FnOnce(cl_sched::UpdateReporter) -> Fut,
) -> Result<T, ApiError>
where
    Fut: std::future::Future<Output = Result<T, ApiError>>,
{
    let progress = cl_sched::UpdateReporter::new(state.cluster.clone(), state.node_name.clone());
    let result = steps(progress.clone()).await;
    if let Err(e) = &result {
        progress
            .report(instance, cl_raft::UpdatePhase::Failed, Some(e.to_string()))
            .await;
    }
    result
}

pub(super) async fn update_local_steps(
    state: &AppState,
    instance: &InstanceId,
    progress: &cl_sched::UpdateReporter,
) -> Result<(), ApiError> {
    let digest = pull_steps(state, instance, progress).await?;
    replace_steps(state, instance, digest, progress).await
}

pub(super) async fn pull_local(
    state: &AppState,
    instance: &InstanceId,
) -> Result<String, ApiError> {
    with_failure_report(state, instance, |progress| async move {
        pull_steps(state, instance, &progress).await
    })
    .await
}

pub(super) async fn replace_local(
    state: &AppState,
    instance: &InstanceId,
    digest: String,
) -> Result<(), ApiError> {
    with_failure_report(state, instance, |progress| async move {
        replace_steps(state, instance, digest, &progress).await
    })
    .await
}

pub(super) async fn pull_steps(
    state: &AppState,
    instance: &InstanceId,
    progress: &cl_sched::UpdateReporter,
) -> Result<String, ApiError> {
    use cl_raft::UpdatePhase;
    let docker = state.docker().await?;
    // A container that is absent (e.g. a failed migration left it unstartable on a pinned
    // digest the registry no longer serves) is still updatable.

    // The declared reference (`nginx:1.25`), not the running container's image: once pinned
    // that is `repo@sha256:...`, and pulling a digest is a no-op forever. See `updates::declared_image`.
    let cluster_state = state.cluster.snapshot().await;
    let image = cluster_state
        .stacks
        .get(&instance.stack)
        .and_then(|stack| StackFile::parse(&stack.spec).ok())
        .and_then(|file| {
            file.services
                .get(&instance.service)
                .map(|s| s.image.clone())
        })
        .ok_or_else(|| ApiError::NotFound(format!("no stack service for instance {instance}")))?;
    drop(cluster_state);

    progress.report(instance, UpdatePhase::Pulling, None).await;
    let registries = crate::routes::registries::load(state).await;
    // Marked as the reconcile loop marks its pulls: docker inspect cannot tell "fetching"
    // from "about to be created", and `apply` returns before the pull finishes.
    cl_sched::pull_started(&state.pulling, instance).await;
    let pulled = docker.pull_as(&image, registries.for_image(&image)).await;
    cl_sched::pull_finished(&state.pulling, instance).await;
    pulled.map_err(ApiError::internal)
}

pub(super) async fn replace_steps(
    state: &AppState,
    instance: &InstanceId,
    digest: String,
    progress: &cl_sched::UpdateReporter,
) -> Result<(), ApiError> {
    use cl_raft::UpdatePhase;
    let name = instance.container_name();
    let docker = state.docker().await?;
    let running = docker.inspect(&name).await.map_err(ApiError::internal)?;
    let Some(running) = running else {
        state
            .cluster
            .submit(ClusterRequest::SetImageDigest {
                instance: instance.clone(),
                digest,
            })
            .await
            .map_err(ApiError::not_leader)?;
        progress.report(instance, UpdatePhase::Started, None).await;
        return Ok(());
    };

    // Remember what is running so a failed update can restore it. An unpinned instance has no
    // recorded pin and the pull moved its tag, so resolve the running image's digest (by image
    // id, untouched by the pull) and pin that first.
    let pinned_before = state
        .cluster
        .state()
        .await
        .assignments
        .get(instance)
        .and_then(|a| a.image_digest.clone());
    if pinned_before.is_none() {
        let repository = cl_docker::ImageRef::parse(&running.image).repository;
        if let Some(image_id) = running.image_digest.as_deref() {
            if let Ok(Some(old)) = docker.digest_of_image(image_id, &repository).await {
                state
                    .cluster
                    .submit(ClusterRequest::SetImageDigest {
                        instance: instance.clone(),
                        digest: old,
                    })
                    .await
                    .map_err(ApiError::not_leader)?;
            }
        }
    }
    state
        .cluster
        .submit(ClusterRequest::SetImageDigest {
            instance: instance.clone(),
            digest,
        })
        .await
        .map_err(ApiError::not_leader)?;

    progress.report(instance, UpdatePhase::Stopping, None).await;
    // Reconcile's `failover.stop_grace`, not docker's 10s, which is short for a database.
    let stop_grace = state.cluster_config.read().await.failover.stop_grace;
    docker.stop(&name, stop_grace).await.ok();
    docker
        .remove(&name, true)
        .await
        .map_err(ApiError::internal)?;
    progress.report(instance, UpdatePhase::Starting, None).await;
    Ok(())
}

pub(super) async fn internal_pull(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Pulled>, ApiError> {
    let digest = pull_local(&state, &parse_id(&id)?).await?;
    Ok(Json(Pulled { digest }))
}

pub(super) async fn internal_replace(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(pulled): Query<Pulled>,
) -> Result<StatusCode, ApiError> {
    replace_local(&state, &parse_id(&id)?, pulled.digest).await?;
    Ok(StatusCode::OK)
}

pub(super) async fn internal_update(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    update_local(&state, &instance).await?;
    Ok(StatusCode::OK)
}
