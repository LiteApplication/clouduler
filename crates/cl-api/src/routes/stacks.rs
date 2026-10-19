//! `/api/stacks`.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_config::{validate_stack_name, StackFile, StackRequirements};
use cl_raft::Request as ClusterRequest;
use cl_secret::Sealed;
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/stacks", get(list))
        .route("/api/stacks/{name}", get(detail).put(deploy).delete(remove))
        .route("/api/stacks/{name}/validate", post(validate))
        .route("/api/stacks/{name}/update-webhook", get(update_webhook_url))
        .route(
            "/api/stacks/{name}/update-webhook/regenerate",
            post(regenerate_update_webhook),
        )
        // Public, authenticated by the token in the path (see `webhook_token`).
        .route(
            "/api/stacks/{name}/update-webhook/{token}",
            post(update_webhook),
        )
}

#[derive(Serialize)]
struct StackSummary {
    name: String,
    revision: u64,
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<StackSummary>> {
    let cluster_state = state.cluster.state().await;
    Json(
        cluster_state
            .stacks
            .values()
            .map(|s| StackSummary {
                name: s.name.clone(),
                revision: s.revision,
            })
            .collect(),
    )
}

#[derive(Serialize)]
pub(crate) struct StackDetail {
    pub(crate) name: String,
    pub(crate) spec: String,
    pub(crate) revision: u64,
    pub(crate) configs: Vec<String>,
    pub(crate) secrets: Vec<String>,
    pub(crate) used_configs: Vec<String>,
    pub(crate) used_secrets: Vec<String>,
    pub(crate) requirements: StackRequirements,
}

async fn detail(
    _identity: Identity,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<StackDetail>, ApiError> {
    Ok(Json(stack_detail(&state, &name).await?))
}

/// What `GET /api/stacks/{name}` returns, callable in-process. The spec is
/// returned verbatim; it holds secret *names* only, never their content.
pub(crate) async fn stack_detail(state: &AppState, name: &str) -> Result<StackDetail, ApiError> {
    let cluster_state = state.cluster.state().await;
    let stack = cluster_state
        .stacks
        .get(name)
        .ok_or_else(|| ApiError::NotFound(format!("no stack {name}")))?;
    // The spec was already validated at deploy time, so a parse failure here
    // would mean corrupted state, not a bad request.
    let requirements = match StackFile::parse(&stack.spec) {
        Ok(parsed) => {
            let mut r = parsed.requirements();
            let cfg = state.cluster_config.read().await;
            r.hosts = parsed.hosts(&cfg);
            r
        }
        Err(_) => StackRequirements::default(),
    };
    Ok(StackDetail {
        name: stack.name.clone(),
        spec: stack.spec.clone(),
        revision: stack.revision,
        configs: stack.configs.iter().cloned().collect(),
        secrets: stack.secrets.iter().cloned().collect(),
        used_configs: stack.used_configs.iter().cloned().collect(),
        used_secrets: stack.used_secrets.iter().cloned().collect(),
        requirements,
    })
}

#[derive(Deserialize)]
struct DeployRequest {
    spec: String,
    /// Config name to base64 content, for everything the spec's `configs`
    /// section references.
    #[serde(default)]
    configs: BTreeMap<String, String>,
    /// Secret name to base64 plaintext. Sealed here before anything is
    /// written to the log.
    #[serde(default)]
    secrets: BTreeMap<String, String>,
}

/// Refuse a deploy whose overlay would share a docker network, subnet and
/// bridge with a different stack's (see `cl_sched::run::overlay_collision`).
/// Checked here rather than in raft apply, which has no cluster config to
/// work out which network a stack's overlay is on.
async fn check_overlay_collision(
    state: &AppState,
    name: &str,
    parsed: &StackFile,
) -> Result<(), ApiError> {
    let cfg = state.cluster_config.read().await.clone();
    let cluster_state = state.cluster.state().await;
    let others: Vec<(&str, StackFile)> = cluster_state
        .stacks
        .iter()
        .filter(|(other, _)| other.as_str() != name)
        .filter_map(|(other, s)| Some((other.as_str(), StackFile::parse(&s.spec).ok()?)))
        .collect();
    let all = others
        .iter()
        .map(|(n, f)| (*n, f))
        .chain(std::iter::once((name, parsed)));
    match cl_sched::run::overlay_collision(&cfg, all) {
        Some((overlay, a, b)) => Err(ApiError::Conflict(format!(
            "stacks {a} and {b} would both use the overlay network {overlay} \
             (`<stack>_<network>`), which would break isolation between them: \
             rename one of the stacks or networks"
        ))),
        None => Ok(()),
    }
}

/// Refuse a stack name that is unsafe in container names, overlay networks
/// and the git export (see `validate_stack_name`). A stack already in the
/// cluster under exactly this name is let through, with a warning: it was
/// accepted before names were checked, and must stay redeployable. Done here
/// rather than in raft apply, which must keep replaying existing logs.
pub(crate) async fn check_stack_name(state: &AppState, name: &str) -> Result<(), ApiError> {
    let Err(reason) = validate_stack_name(name) else {
        return Ok(());
    };
    if state.cluster.state().await.stacks.contains_key(name) {
        tracing::warn!(stack = %name, %reason, "redeploying a stack whose name would no longer be accepted");
        return Ok(());
    }
    Err(ApiError::BadRequest(reason))
}

async fn deploy(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<DeployRequest>,
) -> Result<StatusCode, ApiError> {
    let decode = |m: BTreeMap<String, String>| -> Result<BTreeMap<String, Vec<u8>>, ApiError> {
        m.into_iter()
            .map(|(k, v)| Ok((k, cl_raft::base64_decode(&v).map_err(ApiError::BadRequest)?)))
            .collect()
    };
    let configs = decode(body.configs)?;
    let secrets = decode(body.secrets)?;
    deploy_stack(&state, &name, body.spec, configs, secrets).await?;
    Ok(StatusCode::OK)
}

/// The body of `deploy`, callable in-process: validates the spec against what
/// the cluster already holds, seals and stores the plaintext `secrets` the
/// spec owns, then hands the rest to `deploy_from_files`. `configs` and
/// `secrets` are plaintext bytes, already decoded from the wire's base64.
/// Secrets the spec does not reference are ignored, never written.
pub(crate) async fn deploy_stack(
    state: &AppState,
    name: &str,
    spec: String,
    configs: BTreeMap<String, Vec<u8>>,
    secrets: BTreeMap<String, Vec<u8>>,
) -> Result<(), ApiError> {
    check_stack_name(state, name).await?;
    let secrets_to_write = {
        let parsed = StackFile::parse(&spec).map_err(|e| ApiError::BadRequest(e.to_string()))?;
        check_overlay_collision(state, name, &parsed).await?;
        let requirements = parsed.requirements();
        let cluster_state = state.cluster.state().await;
        for config_name in &requirements.file_configs {
            if !configs.contains_key(config_name)
                && !cluster_state.configs.contains_key(config_name)
            {
                return Err(ApiError::BadRequest(format!(
                    "config {config_name} needs content: it has no existing value to keep"
                )));
            }
        }
        for secret_name in &requirements.file_secrets {
            if !secrets.contains_key(secret_name)
                && !cluster_state.secrets.contains_key(secret_name)
            {
                return Err(ApiError::BadRequest(format!(
                    "secret {secret_name} needs content: it has no existing value to keep"
                )));
            }
        }
        secrets
            .into_iter()
            .filter(|(k, _)| requirements.file_secrets.contains(k))
            .collect::<BTreeMap<_, _>>()
    };

    if !secrets_to_write.is_empty() {
        let key_guard = state.key.read().await;
        let key = key_guard
            .as_ref()
            .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
        for (secret_name, plaintext) in &secrets_to_write {
            let sealed = Sealed::seal(key, secret_name, plaintext).map_err(ApiError::internal)?;
            let response = state
                .cluster
                .submit(ClusterRequest::PutSecret {
                    name: secret_name.clone(),
                    sealed: sealed.blob,
                    key_fingerprint: key.fingerprint(),
                    owner: Some(name.to_string()),
                })
                .await
                .map_err(ApiError::not_leader)?;
            if let Some(msg) = response.error {
                return Err(ApiError::BadRequest(msg));
            }
        }
    }

    deploy_from_files(state, name, spec, configs).await
}

/// The reusable core of `deploy`: parses+validates the spec, submits it, then
/// writes every provided config it owns and records what it owns. Shared with
/// `routes::git_backup`'s reverse (git -> cluster) sync, which never has
/// secret content to supply (git-mirrored secrets are age-encrypted for the
/// admin, not this cluster's key, so they can't be written back this way) and
/// so only ever calls this, never the secrets branch above.
pub(crate) async fn deploy_from_files(
    state: &AppState,
    name: &str,
    spec: String,
    configs: BTreeMap<String, Vec<u8>>,
) -> Result<(), ApiError> {
    check_stack_name(state, name).await?;
    let parsed = StackFile::parse(&spec).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    check_overlay_collision(state, name, &parsed).await?;
    let requirements = parsed.requirements();

    // The names this stack owns come from its own spec, not from whatever the
    // caller happened to include in this request: a redeploy that doesn't
    // resend an unchanged config's content must not drop the stack's claim on
    // it, and a config renamed out of the spec must not linger as owned.
    let cluster_state = state.cluster.state().await;
    for config_name in &requirements.file_configs {
        if !configs.contains_key(config_name) && !cluster_state.configs.contains_key(config_name) {
            return Err(ApiError::BadRequest(format!(
                "config {config_name} needs content: it has no existing value to keep"
            )));
        }
    }

    let response = state
        .cluster
        .submit(ClusterRequest::DeployStack {
            name: name.to_string(),
            spec,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }

    for config_name in &requirements.file_configs {
        let Some(content) = configs.get(config_name) else {
            continue;
        };
        let response = state
            .cluster
            .submit(ClusterRequest::PutConfig {
                name: config_name.clone(),
                content: cl_backup::tree::encode_config_content(content),
                owner: Some(name.to_string()),
            })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::BadRequest(msg));
        }
    }

    let response = state
        .cluster
        .submit(ClusterRequest::SetStackObjects {
            name: name.to_string(),
            configs: requirements.file_configs,
            secrets: requirements.file_secrets,
            used_configs: requirements.configs,
            used_secrets: requirements.secrets,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }

    Ok(())
}

async fn remove(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    remove_stack(&state, name).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `DELETE /api/stacks/{name}`, callable in-process.
pub(crate) async fn remove_stack(state: &AppState, name: String) -> Result<(), ApiError> {
    let secret = webhook_secret_name(&name);
    let response = state
        .cluster
        .submit(ClusterRequest::RemoveStack { name })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    // Best effort: a leftover token would otherwise revive the old URL if a
    // stack of the same name were deployed again.
    let _ = state
        .cluster
        .submit(ClusterRequest::RemoveSecret { name: secret })
        .await;
    Ok(())
}

#[derive(Deserialize)]
struct ValidateRequest {
    spec: String,
}

#[derive(Serialize)]
pub(crate) struct ValidationError {
    pub(crate) path: String,
    pub(crate) message: String,
}

#[derive(Serialize)]
pub(crate) struct ValidateResponse {
    pub(crate) valid: bool,
    pub(crate) errors: Vec<ValidationError>,
    pub(crate) requirements: Option<StackRequirements>,
    /// The same breakdown as `requirements`, one entry per service: what the
    /// ui shows as a separate placement table per container instead of one
    /// table that can't say which constraint belongs to which service.
    pub(crate) per_service: BTreeMap<String, StackRequirements>,
}

async fn validate(
    _identity: Identity,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ValidateRequest>,
) -> Json<ValidateResponse> {
    Json(validate_stack(&state, &name, &body.spec).await)
}

/// What `POST /api/stacks/{name}/validate` returns, callable in-process.
/// Never fails: a bad spec or name is reported in the body.
pub(crate) async fn validate_stack(state: &AppState, name: &str, spec: &str) -> ValidateResponse {
    if let Err(e) = check_stack_name(state, name).await {
        return ValidateResponse {
            valid: false,
            errors: vec![ValidationError {
                path: String::new(),
                message: e.to_string(),
            }],
            requirements: None,
            per_service: BTreeMap::new(),
        };
    }
    match StackFile::parse(spec) {
        Ok(parsed) => {
            let mut requirements = parsed.requirements();
            let cfg = state.cluster_config.read().await;
            requirements.hosts = parsed.hosts(&cfg);
            ValidateResponse {
                valid: true,
                errors: Vec::new(),
                requirements: Some(requirements),
                per_service: parsed.service_requirements(),
            }
        }
        Err(e) => ValidateResponse {
            valid: false,
            errors: vec![ValidationError {
                path: String::new(),
                message: e.to_string(),
            }],
            requirements: None,
            per_service: BTreeMap::new(),
        },
    }
}

const WEBHOOK_SECRET_PREFIX: &str = "_clouduler_stack_webhook_";

fn webhook_secret_name(stack: &str) -> String {
    format!("{WEBHOOK_SECRET_PREFIX}{stack}")
}

pub(crate) fn is_reserved_webhook_secret(name: &str) -> bool {
    name.starts_with(WEBHOOK_SECRET_PREFIX)
}

/// The stack's current webhook token, if one has been issued.
async fn read_webhook_token(state: &AppState, stack: &str) -> Option<String> {
    let secret = webhook_secret_name(stack);
    let entry = state.cluster.state().await.secrets.get(&secret).cloned()?;
    let key_guard = state.key.read().await;
    let key = key_guard.as_ref()?;
    let opened = Sealed::from_blob(entry.sealed).open(key, &secret).ok()?;
    String::from_utf8(opened.to_vec()).ok()
}

/// Mints a fresh random token and replaces whatever was stored, which
/// invalidates the old URL immediately.
async fn issue_webhook_token(state: &AppState, stack: &str) -> Result<String, ApiError> {
    let mut buf = [0u8; 24];
    rand::fill(&mut buf);
    let token: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    let secret = webhook_secret_name(stack);

    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
    let sealed = Sealed::seal(key, &secret, token.as_bytes()).map_err(ApiError::internal)?;
    let key_fingerprint = key.fingerprint();
    drop(key_guard);

    let response = state
        .cluster
        .submit(ClusterRequest::PutSecret {
            name: secret,
            sealed: sealed.blob,
            key_fingerprint,
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }
    Ok(token)
}

#[derive(Serialize)]
struct UpdateWebhook {
    /// Path to POST to; the caller prepends the cluster's public origin.
    path: String,
}

fn webhook_path(stack: &str, token: &str) -> UpdateWebhook {
    UpdateWebhook {
        path: format!("/api/stacks/{stack}/update-webhook/{token}"),
    }
}

/// The current webhook, issuing the first token on demand.
async fn update_webhook_url(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<UpdateWebhook>, ApiError> {
    if !state.cluster.state().await.stacks.contains_key(&name) {
        return Err(ApiError::NotFound(format!("no stack {name}")));
    }
    let token = match read_webhook_token(&state, &name).await {
        Some(token) => token,
        None => issue_webhook_token(&state, &name).await?,
    };
    Ok(Json(webhook_path(&name, &token)))
}

async fn regenerate_update_webhook(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<UpdateWebhook>, ApiError> {
    if !state.cluster.state().await.stacks.contains_key(&name) {
        return Err(ApiError::NotFound(format!("no stack {name}")));
    }
    let token = issue_webhook_token(&state, &name).await?;
    Ok(Json(webhook_path(&name, &token)))
}

/// Pulls and restarts every instance of the stack, the same rolling update
/// the Updates view starts. Answers 202 once the work is queued.
async fn update_webhook(
    State(state): State<AppState>,
    Path((name, token)): Path<(String, String)>,
) -> Result<(StatusCode, Json<super::updates::Accepted>), ApiError> {
    let ok = read_webhook_token(&state, &name)
        .await
        .is_some_and(|expected| {
            expected.len() == token.len()
                && expected
                    .bytes()
                    .zip(token.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
        });
    if !ok {
        return Err(ApiError::Unauthenticated("bad webhook token".into()));
    }
    let cluster_state = state.cluster.state().await;
    if !cluster_state.stacks.contains_key(&name) {
        return Err(ApiError::NotFound(format!("no stack {name}")));
    }
    let instances: Vec<String> = cluster_state
        .assignments
        .keys()
        .filter(|i| i.stack == name)
        .map(|i| i.to_string())
        .collect();
    drop(cluster_state);
    if instances.is_empty() {
        return Err(ApiError::NotFound(format!("stack {name} has no instances")));
    }
    Ok(
        super::updates::apply_selection(&state, super::updates::InstanceSelection { instances })
            .await,
    )
}
