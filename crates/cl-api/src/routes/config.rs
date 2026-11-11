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
    /// Only for a container network.
    router: Option<crate::container_router::RouterStatus>,
    /// Each node's address on the container network's shared segment.
    nodes: std::collections::BTreeMap<String, std::net::Ipv4Addr>,
}

/// The live side of a container network: where its router is and what it is
/// doing, and each node's segment address. Empty for any other kind.
async fn network_status(
    _identity: Identity,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<NetworkStatus>, ApiError> {
    let cfg = state.cluster_config.read().await.clone();
    if !cfg.networks.contains_key(&name) {
        return Err(ApiError::NotFound(format!("no network named {name}")));
    }
    let cluster_state = state.cluster.state().await;
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

    // Adding or renaming a network changes which network each deployed stack's
    // overlay sits on, which can make two stacks share one. Only a collision
    // this edit introduces blocks it, so a cluster that already has one can
    // still be edited to fix it.
    let cluster_state = state.cluster.state().await;
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

/// First pair of deployed stacks that share an overlay network under `cfg`,
/// see `cl_sched::run::overlay_collision`.
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

/// Put a config into effect: through raft first, so every node's daemon writes
/// its own copy, then locally so the node answering this request is current
/// straight away rather than after its next watcher tick.
///
/// Raft goes first on purpose. Without a quorum the edit must not land anywhere,
/// or this node comes back from a restart with a config the cluster never
/// agreed to.
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

/// Rename a network everywhere it is referenced: the cluster config (the
/// `networks` key, every node's address map, `web.network`), every deployed
/// stack that names it, and the three out-of-band stores that hold a plain
/// `network: String` (redirects, static routes, wireguard ingress).
///
/// Not one atomic transaction since each piece is its own raft log entry, but
/// each step is atomic on its own and a failure partway through is surfaced
/// immediately rather than silently leaving a mismatched reference.
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
    if !config.networks.contains_key(&old_name) {
        return Err(ApiError::BadRequest(format!("no network named {old_name}")));
    }
    if config.networks.contains_key(&new_name) {
        return Err(ApiError::BadRequest(format!(
            "a network named {new_name} already exists"
        )));
    }

    let net = config.networks.remove(&old_name).expect("checked above");
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

    // The rename moves every stack's overlay name too (`<stack>_<network>`),
    // so refuse it up front if that makes two stacks share one.
    let cluster_state = state.cluster.state().await;
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

    // Each node's imported wireguard config is sealed under a secret name
    // scoped by network name, so it must move along with the rename or the
    // tunnel silently stops finding its config.
    if is_wireguard {
        rename_wireguard_secrets(&state, &old_name, &new_name, &node_names).await?;
    }

    let cluster_state = state.cluster.state().await;
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

/// Every network's `access` list names the networks it may reach; a renamed
/// network must keep matching.
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

/// Renames `old` to `new` everywhere a stack file can name a network: the
/// top-level `networks` block, each service's `networks:` list, and every
/// `x-clouduler.proxy`/`expose` entry's `network` field. Returns whether
/// anything changed, so the caller can skip stacks that never mention it.
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

/// Rename a drive everywhere it is referenced: the cluster config (the
/// `drives` key, every node's drive-to-path map) and every deployed stack
/// whose volumes mount from it.
///
/// Unlike a network rename this never touches replicated volume state: a
/// volume's raft-tracked generation and replica set are keyed by
/// drive/volume id, so the next reconcile pass registers a fresh entry under
/// the new id the moment a stack's volume mount is renamed here, exactly
/// as it would if the drive had been renamed by hand across two applies. The
/// data itself is untouched, since a node's `drives` path mapping moves with
/// the rename rather than changing.
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
    if !config.drives.contains_key(&old_name) {
        return Err(ApiError::BadRequest(format!("no drive named {old_name}")));
    }
    if config.drives.contains_key(&new_name) {
        return Err(ApiError::BadRequest(format!(
            "a drive named {new_name} already exists"
        )));
    }

    let drive = config.drives.remove(&old_name).expect("checked above");
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

    let cluster_state = state.cluster.state().await;
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

/// Renames `old` to `new` in every service's volume mounts that source from
/// it. Returns whether anything changed, so the caller can skip stacks that
/// never mention it.
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

/// Moves every node's sealed wireguard config secret from the old network's
/// reserved name to the new one, so the tunnel keeps finding it after the
/// rename. Submitted as-is (still sealed, no key needed here) so that a
/// put-then-remove per node, since there is no rename request.
async fn rename_wireguard_secrets(
    state: &AppState,
    old: &str,
    new: &str,
    nodes: &[String],
) -> Result<(), ApiError> {
    for node in nodes {
        let old_name = crate::wireguard::wireguard_secret_name(old, node);
        let cluster_state = state.cluster.state().await;
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
    let cluster_state = state.cluster.state().await;
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
    let cluster_state = state.cluster.state().await;
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
    let cluster_state = state.cluster.state().await;
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
    /// Dotted config path of the offending key, empty when the failure is not
    /// tied to one.
    path: String,
    message: String,
}

#[derive(Serialize)]
struct ValidationResponse {
    valid: bool,
    errors: Vec<ValidationError>,
}

/// Check a config without applying it.
///
/// Takes the same `{"config": ...}` envelope as `PUT /api/config` so the two
/// can be called with one body, and answers 200 with the outcome rather than a
/// 4xx: a config that fails validation is an expected answer to this question,
/// not a bad request. Validation short circuits on the first problem, so
/// `errors` holds at most one entry.
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

    /// The stack-file half of a network rename: the top-level `networks:`
    /// key, a service's `networks:` list, and the network name buried inside
    /// `x-clouduler.proxy`/`expose` all have to move together, or the
    /// renamed network silently stops matching what the service asks for.
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

        // Renaming again for a network the stack never mentioned is a no-op.
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

    /// Re-serializing a stack file after a rename must not pad it with every
    /// field's default value: only what the stack actually set should
    /// reappear.
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
