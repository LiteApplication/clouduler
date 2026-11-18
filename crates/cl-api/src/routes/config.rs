//! `/api/config`: the cluster config file.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_config::{ClusterConfig, StackFile};
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::redirects::{RedirectStore, REDIRECTS_CONFIG};
use crate::server::AppState;
use crate::static_routes::{StaticRouteStore, STATIC_ROUTES_CONFIG};
use crate::wireguard::{WireguardIngressStore, WIREGUARD_INGRESS_CONFIG};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/config/validate", post(validate_config))
        .route("/api/config/networks/{name}/rename", post(rename_network))
        .route("/api/config/drives/{name}/rename", post(rename_drive))
        .route("/api/networks/{name}/status", get(network_status))
}

#[derive(Serialize)]
struct NetworkStatus {
    router: Option<crate::container_router::RouterStatus>,
    nodes: std::collections::BTreeMap<String, std::net::Ipv4Addr>,
}

async fn network_status(
    _identity: Identity,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<NetworkStatus>, ApiError> {
    let cfg = state.cluster_config.read().await.clone();
    if !cfg.networks.contains_key(&name) {
        return Err(ApiError::NotFound(format!("no network named {name}")));
    }
    let cluster_state = state.cluster.snapshot().await;
    let allocations = crate::container_router::allocations_of(&cluster_state);
    let router = crate::container_router::router_status(&cluster_state, &cfg, &allocations, &name);
    let nodes = if router.is_some() {
        allocations
            .node_ips(&cl_net::container_net::container_docker_name(&name))
            .map(|(node, ip)| (node.to_string(), ip))
            .collect()
    } else {
        Default::default()
    };
    Ok(Json(NetworkStatus { router, nodes }))
}

async fn get_config(_identity: Identity, State(state): State<AppState>) -> Json<ClusterConfig> {
    Json(state.cluster_config.read().await.clone())
}

#[derive(Deserialize)]
struct PutConfigRequest {
    config: ClusterConfig,
}

async fn put_config(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<PutConfigRequest>,
) -> Result<StatusCode, ApiError> {
    body.config
        .validate()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let previous = state.cluster_config.read().await.clone();

    // Only a collision this edit introduces blocks it, so a cluster that already has one can be edited to fix it.
    let cluster_state = state.cluster.snapshot().await;
    if let Some((overlay, a, b)) = deployed_overlay_collision(&body.config, &cluster_state) {
        if deployed_overlay_collision(&previous, &cluster_state).is_none() {
            return Err(ApiError::Conflict(format!(
                "this config would make stacks {a} and {b} share the overlay network {overlay}: \
                 rename one of the stacks or networks"
            )));
        }
    }
    apply_config(&state, &body.config).await?;

    Ok(StatusCode::OK)
}

fn deployed_overlay_collision(
    cfg: &ClusterConfig,
    cluster_state: &cl_raft::ClusterState,
) -> Option<(String, String, String)> {
    let files: Vec<(&str, StackFile)> = cluster_state
        .stacks
        .iter()
        .filter_map(|(name, s)| Some((name.as_str(), StackFile::parse(&s.spec).ok()?)))
        .collect();
    cl_sched::run::overlay_collision(cfg, files.iter().map(|(n, f)| (*n, f)))
}

/// Raft first, then locally so this node is current immediately. Raft goes first on purpose:
/// without a quorum the edit must not land anywhere, or a restart would bring back a config the
/// cluster never agreed to.
pub(super) async fn apply_config(state: &AppState, config: &ClusterConfig) -> Result<(), ApiError> {
    let yaml = serde_yaml::to_string(config).map_err(ApiError::internal)?;

    let expect_generation = state
        .cluster
        .state()
        .await
        .cluster_config
        .as_ref()
        .map(|c| c.generation)
        .unwrap_or(0);

    let response = state
        .cluster
        .submit(cl_raft::Request::PutClusterConfig {
            yaml: yaml.clone(),
            expect_generation,
        })
        .await
        .map_err(|e| ApiError::Internal(format!("replicating the config: {e}")))?;
    if let Some(error) = response.error {
        return Err(ApiError::Conflict(error));
    }

    std::fs::write(&state.config_path, yaml).map_err(ApiError::internal)?;
    *state.cluster_config.write().await = config.clone();

    Ok(())
}

#[derive(Deserialize)]
struct RenameNetworkRequest {
    new_name: String,
}

/// Rename a network everywhere: the config (`networks` key, address maps, `web.network`), every
/// stack naming it, and the stores holding a plain `network: String` (redirects, static routes,
/// wireguard ingress). Not atomic (one raft entry per piece), but a partial failure is surfaced.
async fn rename_network(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(old_name): Path<String>,
    Json(body): Json<RenameNetworkRequest>,
) -> Result<StatusCode, ApiError> {
    let new_name = body.new_name.trim().to_string();
    if new_name.is_empty() {
        return Err(ApiError::BadRequest("new_name must not be empty".into()));
    }
    if new_name == old_name {
        return Ok(StatusCode::OK);
    }

    let mut config = state.cluster_config.read().await.clone();
    let Some(net) = config.networks.remove(&old_name) else {
        return Err(ApiError::BadRequest(format!("no network named {old_name}")));
    };
    if config.networks.contains_key(&new_name) {
        return Err(ApiError::BadRequest(format!(
            "a network named {new_name} already exists"
        )));
    }

    let is_wireguard = net.kind == cl_config::NetworkKind::Wireguard;
    config.networks.insert(new_name.clone(), net);
    let node_names: Vec<String> = config.nodes.keys().cloned().collect();
    for node in config.nodes.values_mut() {
        if let Some(addr) = node.network.remove(&old_name) {
            node.network.insert(new_name.clone(), addr);
        }
    }
    if config.web.network == old_name {
        config.web.network = new_name.clone();
    }
    rename_in_access(&mut config, &old_name, &new_name);
    config
        .validate()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let cluster_state = state.cluster.snapshot().await;
    let renamed: Vec<(&str, StackFile)> = cluster_state
        .stacks
        .iter()
        .filter_map(|(name, stack)| {
            let mut file = StackFile::parse(&stack.spec).ok()?;
            rename_network_in_stack(&mut file, &old_name, &new_name);
            Some((name.as_str(), file))
        })
        .collect();
    if let Some((overlay, a, b)) =
        cl_sched::run::overlay_collision(&config, renamed.iter().map(|(n, f)| (*n, f)))
    {
        return Err(ApiError::Conflict(format!(
            "renaming {old_name} to {new_name} would make stacks {a} and {b} share the \
             overlay network {overlay}: rename one of the stacks first"
        )));
    }
    apply_config(&state, &config).await?;

    // The sealed wireguard config is keyed by network name, so it must move too.
    if is_wireguard {
        rename_wireguard_secrets(&state, &old_name, &new_name, &node_names).await?;
    }

    let cluster_state = state.cluster.snapshot().await;
    for (stack_name, stack) in &cluster_state.stacks {
        let Ok(mut file) = StackFile::parse(&stack.spec) else {
            continue;
        };
        if !rename_network_in_stack(&mut file, &old_name, &new_name) {
            continue;
        }
        let spec = serde_yaml::to_string(&file).map_err(ApiError::internal)?;
        let response = state
            .cluster
            .submit(cl_raft::Request::DeployStack {
                name: stack_name.clone(),
                spec,
                at: crate::auth::oauth::now_secs(),
            })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::Internal(format!(
                "renaming network in stack {stack_name}: {msg}"
            )));
        }
    }

    rename_in_redirects(&state, &old_name, &new_name).await?;
    rename_in_static_routes(&state, &old_name, &new_name).await?;
    rename_in_wireguard_ingress(&state, &old_name, &new_name).await?;

    Ok(StatusCode::OK)
}

fn rename_in_access(config: &mut cl_config::ClusterConfig, old: &str, new: &str) {
    for net in config.networks.values_mut() {
        let Some(access) = net.access.as_mut() else {
            continue;
        };
        for allowed in access.iter_mut() {
            if allowed == old {
                *allowed = new.to_string();
            }
        }
    }
}

fn rename_network_in_stack(file: &mut StackFile, old: &str, new: &str) -> bool {
    let mut changed = false;
    if let Some(net) = file.networks.remove(old) {
        file.networks.insert(new.to_string(), net);
        changed = true;
    }
    for svc in file.services.values_mut() {
        for n in svc.networks.iter_mut() {
            if n == old {
                *n = new.to_string();
                changed = true;
            }
        }
        let Some(ext) = svc.x_clouduler.as_mut() else {
            continue;
        };
        for route in ext.proxy.iter_mut() {
            if route.network == old {
                route.network = new.to_string();
                changed = true;
            }
        }
        for expose in ext.expose.iter_mut() {
            if expose.network == old {
                expose.network = new.to_string();
                changed = true;
            }
        }
    }
    changed
}

#[derive(Deserialize)]
struct RenameDriveRequest {
    new_name: String,
}

/// Rename a drive in the config (`drives` key, per-node paths) and every stack mounting it.
///
/// Replicated volume state is not touched: it is keyed by drive/volume id, so the next reconcile
/// registers a fresh entry under the new id. The data is untouched since the node's path mapping
/// moves with the rename.
async fn rename_drive(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(old_name): Path<String>,
    Json(body): Json<RenameDriveRequest>,
) -> Result<StatusCode, ApiError> {
    let new_name = body.new_name.trim().to_string();
    if new_name.is_empty() {
        return Err(ApiError::BadRequest("new_name must not be empty".into()));
    }
    if new_name == old_name {
        return Ok(StatusCode::OK);
    }

    let mut config = state.cluster_config.read().await.clone();
    let Some(drive) = config.drives.remove(&old_name) else {
        return Err(ApiError::BadRequest(format!("no drive named {old_name}")));
    };
    if config.drives.contains_key(&new_name) {
        return Err(ApiError::BadRequest(format!(
            "a drive named {new_name} already exists"
        )));
    }

    config.drives.insert(new_name.clone(), drive);
    for node in config.nodes.values_mut() {
        if let Some(path) = node.drives.remove(&old_name) {
            node.drives.insert(new_name.clone(), path);
        }
    }
    config
        .validate()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    apply_config(&state, &config).await?;

    let cluster_state = state.cluster.snapshot().await;
    for (stack_name, stack) in &cluster_state.stacks {
        let Ok(mut file) = StackFile::parse(&stack.spec) else {
            continue;
        };
        if !rename_drive_in_stack(&mut file, &old_name, &new_name) {
            continue;
        }
        let spec = serde_yaml::to_string(&file).map_err(ApiError::internal)?;
        let response = state
            .cluster
            .submit(cl_raft::Request::DeployStack {
                name: stack_name.clone(),
                spec,
                at: crate::auth::oauth::now_secs(),
            })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::Internal(format!(
                "renaming drive in stack {stack_name}: {msg}"
            )));
        }
    }

    Ok(StatusCode::OK)
}

fn rename_drive_in_stack(file: &mut StackFile, old: &str, new: &str) -> bool {
    let mut changed = false;
    for svc in file.services.values_mut() {
        for v in svc.volumes.iter_mut() {
            if v.drive.as_deref() == Some(old) {
                v.drive = Some(new.to_string());
                changed = true;
            }
        }
    }
    changed
}

/// Submitted still sealed (no key needed) as put-then-remove per node, since there is no rename request.
async fn rename_wireguard_secrets(
    state: &AppState,
    old: &str,
    new: &str,
    nodes: &[String],
) -> Result<(), ApiError> {
    for node in nodes {
        let old_name = crate::wireguard::wireguard_secret_name(old, node);
        let cluster_state = state.cluster.snapshot().await;
        let Some(entry) = cluster_state.secrets.get(&old_name).cloned() else {
            continue;
        };
        drop(cluster_state);

        let new_name = crate::wireguard::wireguard_secret_name(new, node);
        let response = state
            .cluster
            .submit(cl_raft::Request::PutSecret {
                name: new_name,
                sealed: entry.sealed,
                key_fingerprint: entry.key_fingerprint,
                owner: entry.owner,
            })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::Internal(format!(
                "moving wireguard config for {node}: {msg}"
            )));
        }

        let response = state
            .cluster
            .submit(cl_raft::Request::RemoveSecret { name: old_name })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::Internal(format!(
                "removing old wireguard config for {node}: {msg}"
            )));
        }
    }
    Ok(())
}

async fn rename_in_redirects(state: &AppState, old: &str, new: &str) -> Result<(), ApiError> {
    let cluster_state = state.cluster.snapshot().await;
    let mut store = RedirectStore::load(&cluster_state);
    let mut changed = false;
    for rule in store.rules.iter_mut() {
        if rule.network == old {
            rule.network = new.to_string();
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let response = state
        .cluster
        .submit(cl_raft::Request::PutConfig {
            name: REDIRECTS_CONFIG.to_string(),
            content: store.encode(),
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Internal(msg));
    }
    Ok(())
}

async fn rename_in_static_routes(state: &AppState, old: &str, new: &str) -> Result<(), ApiError> {
    let cluster_state = state.cluster.snapshot().await;
    let mut store = StaticRouteStore::load(&cluster_state);
    let mut changed = false;
    for rule in store.routes.iter_mut() {
        if rule.network == old {
            rule.network = new.to_string();
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let response = state
        .cluster
        .submit(cl_raft::Request::PutConfig {
            name: STATIC_ROUTES_CONFIG.to_string(),
            content: store.encode(),
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Internal(msg));
    }
    Ok(())
}

async fn rename_in_wireguard_ingress(
    state: &AppState,
    old: &str,
    new: &str,
) -> Result<(), ApiError> {
    let cluster_state = state.cluster.snapshot().await;
    let mut store = WireguardIngressStore::load(&cluster_state);
    let mut changed = false;
    for rule in store.rules.iter_mut() {
        if rule.network == old {
            rule.network = new.to_string();
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let response = state
        .cluster
        .submit(cl_raft::Request::PutConfig {
            name: WIREGUARD_INGRESS_CONFIG.to_string(),
            content: store.encode(),
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Internal(msg));
    }
    Ok(())
}

#[derive(Deserialize)]
struct ValidateRequest {
    config: ClusterConfig,
}

#[derive(Serialize)]
struct ValidationError {
    path: String,
    message: String,
}

#[derive(Serialize)]
struct ValidationResponse {
    valid: bool,
    errors: Vec<ValidationError>,
}

/// Same `{"config": ...}` envelope as `PUT /api/config`. Answers 200 with the outcome: a failing
/// config is an expected answer here. Short circuits on the first problem, so `errors` has at most one entry.
async fn validate_config(
    _identity: Identity,
    Json(body): Json<ValidateRequest>,
) -> Json<ValidationResponse> {
    match body.config.validate() {
        Ok(()) => Json(ValidationResponse {
            valid: true,
            errors: Vec::new(),
        }),
        Err(cl_config::ConfigError::Invalid { at, msg }) => Json(ValidationResponse {
            valid: false,
            errors: vec![ValidationError {
                path: at,
                message: msg,
            }],
        }),
        Err(other) => Json(ValidationResponse {
            valid: false,
            errors: vec![ValidationError {
                path: String::new(),
                message: other.to_string(),
            }],
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The top-level `networks:` key, a service's `networks:` list and the network inside
    /// `x-clouduler.proxy`/`expose` must all move together.
    #[test]
    fn rename_network_in_stack_updates_the_x_clouduler_block() {
        let mut file = StackFile::parse(
            r#"
services:
  app:
    image: nginx
    networks: [wan]
    x-clouduler:
      proxy:
        - host: app
          network: wan
          port: 80
      expose:
        - port: 22
          published: 2222
          network: wan
networks:
  wan: {}
"#,
        )
        .expect("parse");

        assert!(rename_network_in_stack(&mut file, "wan", "public"));

        assert!(file.networks.contains_key("public"));
        assert!(!file.networks.contains_key("wan"));

        let svc = &file.services["app"];
        assert_eq!(svc.networks, vec!["public".to_string()]);

        let ext = svc.x_clouduler.as_ref().expect("has x-clouduler");
        assert_eq!(ext.proxy[0].network, "public");
        assert_eq!(ext.expose[0].network, "public");

        assert!(!rename_network_in_stack(&mut file, "unrelated", "other"));
    }

    #[test]
    fn renaming_a_network_rewrites_access_lists() {
        let mut config =
            cl_config::ClusterConfig::parse(include_str!("../../../../examples/cluster.yaml"))
                .expect("example parses");
        assert_eq!(
            config.networks["tailnet"].access.clone().unwrap(),
            vec!["lan".to_string()]
        );
        rename_in_access(&mut config, "lan", "home");
        assert_eq!(
            config.networks["tailnet"].access.clone().unwrap(),
            vec!["home".to_string()]
        );
        config.networks.get_mut("lan").unwrap().access = Some(vec!["tailnet".to_string()]);
        rename_in_access(&mut config, "tailnet", "vpn");
        assert_eq!(
            config.networks["lan"].access.clone().unwrap(),
            vec!["vpn".to_string()]
        );
        rename_in_access(&mut config, "nothing", "x");
        assert_eq!(
            config.networks["tailnet"].access.clone().unwrap(),
            vec!["home".to_string()]
        );
    }

    /// Re-serializing after a rename must not pad the file with default values.
    #[test]
    fn rewritten_stack_file_stays_minimal() {
        let mut file = StackFile::parse(
            r#"
services:
  app:
    image: nginx
    networks: [wan]
networks:
  wan: {}
"#,
        )
        .expect("parse");
        rename_network_in_stack(&mut file, "wan", "public");

        let yaml = serde_yaml::to_string(&file).expect("encode");
        for absent in [
            "cap_add",
            "cap_drop",
            "depends_on",
            "env_file",
            "sysctls",
            "ports:",
            "configs:",
            "secrets:",
            "x-clouduler",
        ] {
            assert!(
                !yaml.contains(absent),
                "unexpected default field `{absent}` in:\n{yaml}"
            );
        }

        let reparsed = StackFile::parse(&yaml).expect("round trips");
        assert_eq!(reparsed, file);
    }
}
