use super::*;

pub(super) fn instance_service(
    state: &cl_raft::ClusterState,
    instance: &InstanceId,
) -> Result<Service, ApiError> {
    let stack = state
        .stacks
        .get(&instance.stack)
        .ok_or_else(|| ApiError::NotFound(format!("stack {} not found", instance.stack)))?;
    let file = StackFile::parse(&stack.spec).map_err(|e| ApiError::Internal(e.to_string()))?;
    file.services
        .get(&instance.service)
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("no service {}", instance.service)))
}

#[derive(Serialize)]
pub(super) struct ActionSummary {
    name: String,
    confirm: bool,
}

pub(super) async fn list_actions(
    _identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ActionSummary>>, ApiError> {
    let service = instance_service(&state.cluster.state().await, &parse_id(&id)?)?;
    Ok(Json(
        service
            .x_clouduler
            .map(|x| {
                x.actions
                    .into_iter()
                    .filter(|(_, a)| a.show_in_ui)
                    .map(|(name, a)| ActionSummary {
                        name,
                        confirm: a.confirm,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    ))
}

/// Only commands written in the stack file run: the caller picks a name, never a command line.
pub(super) async fn run_action(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<ExecRunResult>, ApiError> {
    let instance = parse_id(&id)?;
    let service = instance_service(&state.cluster.state().await, &instance)?;
    let action = service
        .x_clouduler
        .and_then(|x| x.actions.get(&name).filter(|a| a.show_in_ui).cloned())
        .ok_or_else(|| ApiError::NotFound(format!("no action {name:?} on {}", instance.service)))?;
    Ok(Json(
        exec_run(&state, &instance, action_request(&action)).await?,
    ))
}

pub(super) fn action_request(action: &cl_config::ServiceAction) -> ExecRunRequest {
    ExecRunRequest {
        cmd: action.argv(),
        user: action.user.clone(),
        stdin: None,
        timeout_secs: action.timeout.map_or(30, |d| d.as_secs()),
        max_output: default_exec_output(),
        standalone: action.standalone,
        image: action.image.clone(),
    }
}

/// For callers outside the api (replication hooks). `Err` is a one-line reason.
pub async fn run_action_local(
    docker: &Docker,
    instance: &InstanceId,
    action: &cl_config::ServiceAction,
) -> Result<(), String> {
    if action.standalone {
        return Err("a standalone action cannot run as a hook".to_string());
    }
    let r = exec_run_docker(docker, instance, action_request(action))
        .await
        .map_err(|e| e.to_string())?;
    if r.timed_out {
        return Err("timed out".to_string());
    }
    match r.exit_code {
        Some(0) => Ok(()),
        code => Err(format!(
            "exit {}: {}",
            code.map_or("?".to_string(), |c| c.to_string()),
            r.stderr.trim()
        )),
    }
}
