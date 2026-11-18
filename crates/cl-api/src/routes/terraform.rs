//! `/api/stacks/{name}/terraform`: per-service OpenTofu modules and their runs.
//!
//! Modules live in raft (`SetStackTerraform` replaces a stack's whole set); the runner in the daemon
//! applies them. Every writer validates the module text here (`validate_module`), since raft apply does not.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_raft::terraform::{terraform_key, TerraformMode, TerraformOutcome, TerraformRun};
use cl_raft::Request as ClusterRequest;
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/stacks/{name}/terraform", get(status))
        .route(
            "/api/stacks/{name}/terraform/{service}",
            axum::routing::put(put_module).delete(delete_module),
        )
        .route("/api/stacks/{name}/terraform/{service}/plan", post(plan))
        .route("/api/stacks/{name}/terraform/{service}/apply", post(apply))
}

#[derive(Serialize)]
pub(crate) struct RunView {
    pub id: String,
    pub mode: &'static str,
    pub trigger: String,
    pub node: String,
    pub instance: String,
    pub fingerprint: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// `running`, `succeeded` or `failed`.
    pub outcome: &'static str,
    /// The plan/apply summary on success, the error on failure.
    pub message: Option<String>,
    /// Redacted by the runner before it is stored.
    pub log_tail: String,
}

pub(crate) fn run_view(run: &TerraformRun) -> RunView {
    let (outcome, message) = match &run.outcome {
        TerraformOutcome::Running => ("running", None),
        TerraformOutcome::Succeeded { summary } => ("succeeded", Some(summary.clone())),
        TerraformOutcome::Failed { error } => ("failed", Some(error.clone())),
    };
    RunView {
        id: run.id.clone(),
        mode: mode_name(run.mode),
        trigger: run.trigger.clone(),
        node: run.node.clone(),
        instance: run.instance.to_string(),
        fingerprint: run.fingerprint.clone(),
        started_at: run.started_at,
        finished_at: run.finished_at,
        outcome,
        message,
        log_tail: run.log_tail.clone(),
    }
}

pub(crate) fn mode_name(mode: TerraformMode) -> &'static str {
    match mode {
        TerraformMode::Plan => "plan",
        TerraformMode::Apply => "apply",
    }
}

#[derive(Serialize)]
pub(crate) struct ServiceTerraform {
    pub service: String,
    pub source: String,
    pub revision: u64,
    /// `x-clouduler.terraform.enabled` of the stack's spec (true when the block is absent).
    pub enabled: bool,
    pub last_run: Option<RunView>,
    /// A manual run waiting for the runner.
    pub pending_request: Option<&'static str>,
    /// The secrets and configs this module's outputs wrote (values are never shown).
    pub outputs: Vec<OutputView>,
}

/// One cluster object written by a module output.
#[derive(Serialize)]
pub(crate) struct OutputView {
    /// `secret` or `config`.
    pub kind: &'static str,
    pub name: String,
    pub revision: u64,
}

fn output_views(state: &cl_raft::state::ClusterState, key: &str) -> Vec<OutputView> {
    state
        .terraform_outputs
        .get(key)
        .into_iter()
        .flatten()
        .filter_map(|o| {
            match o.kind {
                cl_raft::terraform::OutputKind::Secret => {
                    state.secrets.get(&o.name).map(|e| ("secret", e.revision))
                }
                cl_raft::terraform::OutputKind::Config => {
                    state.configs.get(&o.name).map(|e| ("config", e.revision))
                }
            }
            .map(|(kind, revision)| OutputView {
                kind,
                name: o.name.clone(),
                revision,
            })
        })
        .collect()
}

#[derive(Serialize)]
pub(crate) struct TerraformStatus {
    /// Whether cluster.yaml has a `terraform:` section at all.
    pub enabled_cluster: bool,
    pub services: Vec<ServiceTerraform>,
}

pub(crate) async fn terraform_status(
    state: &AppState,
    stack: &str,
) -> Result<TerraformStatus, ApiError> {
    let enabled_cluster = state.cluster_config.read().await.terraform.is_some();
    let cluster_state = state.cluster.state().await;
    let spec = cluster_state
        .stacks
        .get(stack)
        .ok_or_else(|| ApiError::NotFound(format!("no stack {stack}")))?;
    let parsed = cl_config::StackFile::parse(&spec.spec).ok();
    let mut services = Vec::new();
    for (service, module) in cluster_state
        .terraform_modules
        .get(stack)
        .into_iter()
        .flatten()
    {
        let key = terraform_key(stack, service);
        let enabled = parsed
            .as_ref()
            .and_then(|p| p.services.get(service))
            .and_then(|s| s.x_clouduler.as_ref())
            .and_then(|x| x.terraform.as_ref())
            .is_none_or(|t| t.enabled);
        services.push(ServiceTerraform {
            service: service.clone(),
            source: module.source.clone(),
            revision: module.revision,
            enabled,
            last_run: cluster_state.terraform_runs.get(&key).map(run_view),
            pending_request: cluster_state
                .terraform_requests
                .get(&key)
                .map(|m| mode_name(*m)),
            outputs: output_views(&cluster_state, &key),
        });
    }
    Ok(TerraformStatus {
        enabled_cluster,
        services,
    })
}

async fn status(
    _identity: Identity,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<TerraformStatus>, ApiError> {
    Ok(Json(terraform_status(&state, &name).await?))
}

/// Refuses a spec whose `x-clouduler.terraform.outputs` would write a reserved secret or config
/// (the names with dedicated write paths). The rest is `TerraformService::validate`.
pub(crate) fn check_output_targets(parsed: &cl_config::StackFile) -> Result<(), ApiError> {
    for (service, svc) in &parsed.services {
        let Some(tf) = svc.x_clouduler.as_ref().and_then(|x| x.terraform.as_ref()) else {
            continue;
        };
        for o in &tf.outputs {
            if let Some(target) = o.target() {
                if super::objects::is_reserved_output_target(target) {
                    let name = match target {
                        cl_config::TfOutputTarget::Secret(n)
                        | cl_config::TfOutputTarget::Config(n) => n,
                    };
                    return Err(ApiError::BadRequest(format!(
                        "services.{service}.x-clouduler.terraform.outputs: {name} is a reserved name"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Rejects a module that does not parse or declares a `backend`/`cloud` block.
pub(crate) fn validate_modules(modules: &BTreeMap<String, String>) -> Result<(), ApiError> {
    for (service, source) in modules {
        cl_config::validate_module(source)
            .map_err(|e| ApiError::BadRequest(format!("terraform module for {service}: {e}")))?;
    }
    Ok(())
}

/// Replaces the stack's whole module set (an empty map forgets them all).
pub(crate) async fn set_stack_modules(
    state: &AppState,
    stack: &str,
    modules: BTreeMap<String, String>,
) -> Result<(), ApiError> {
    validate_modules(&modules)?;
    let response = state
        .cluster
        .submit(ClusterRequest::SetStackTerraform {
            stack: stack.to_string(),
            modules,
        })
        .await
        .map_err(ApiError::not_leader)?;
    match response.error {
        Some(msg) => Err(ApiError::BadRequest(msg)),
        None => Ok(()),
    }
}

async fn current_modules(state: &AppState, stack: &str) -> BTreeMap<String, String> {
    state
        .cluster
        .state()
        .await
        .terraform_modules
        .get(stack)
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.source.clone()))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct PutModule {
    source: String,
}

async fn put_module(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((name, service)): Path<(String, String)>,
    Json(body): Json<PutModule>,
) -> Result<StatusCode, ApiError> {
    let mut modules = current_modules(&state, &name).await;
    modules.insert(service, body.source);
    set_stack_modules(&state, &name, modules).await?;
    state.nudges.terraform.notify_one();
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_module(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((name, service)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let mut modules = current_modules(&state, &name).await;
    if modules.remove(&service).is_none() {
        return Err(ApiError::NotFound(format!(
            "no terraform module for {name}/{service}"
        )));
    }
    set_stack_modules(&state, &name, modules).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Records a manual run request and wakes the runner. Refused while cluster terraform is off.
pub(crate) async fn request_run(
    state: &AppState,
    stack: &str,
    service: &str,
    mode: TerraformMode,
) -> Result<(), ApiError> {
    if state.cluster_config.read().await.terraform.is_none() {
        return Err(ApiError::Conflict(
            "terraform is not enabled: add a `terraform:` section to the cluster config".into(),
        ));
    }
    let enabled = state
        .cluster
        .state()
        .await
        .stacks
        .get(stack)
        .and_then(|s| cl_config::StackFile::parse(&s.spec).ok())
        .and_then(|p| p.services.get(service).cloned())
        .and_then(|s| s.x_clouduler)
        .and_then(|x| x.terraform)
        .is_none_or(|t| t.enabled);
    if !enabled {
        return Err(ApiError::Conflict(format!(
            "terraform is disabled for {stack}/{service}: x-clouduler.terraform has enabled: false"
        )));
    }
    let response = state
        .cluster
        .submit(ClusterRequest::RequestTerraformRun {
            key: terraform_key(stack, service),
            mode,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::NotFound(msg));
    }
    state.nudges.terraform.notify_one();
    Ok(())
}

async fn plan(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((name, service)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    request_run(&state, &name, &service, TerraformMode::Plan).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn apply(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((name, service)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    request_run(&state, &name, &service, TerraformMode::Apply).await?;
    Ok(StatusCode::ACCEPTED)
}
