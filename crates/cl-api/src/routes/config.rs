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
    if let Some(why) = managed_network_conflict(&previous, &body.config, &cluster_state) {
        return Err(ApiError::Conflict(why));
    }
    apply_config(&state, &body.config).await?;

    Ok(StatusCode::OK)
}

/// Whether a stack (deployed spec) names `network` at the top level or in a service.
fn stack_lists_network(file: &StackFile, network: &str) -> bool {
    file.networks.contains_key(network)
        || file
            .services
            .values()
            .any(|svc| svc.networks.iter().any(|n| n == network))
}

/// Why this edit must not land because of managed networks, if it must not.
/// Only what the edit itself introduces blocks it, so a cluster that already
/// has a problem can be edited to fix it.
///
/// * a newly managed network, or one whose subnet changed, may not overlap an
///   allocated or pinned stack overlay subnet;
/// * flipping `managed` either way or changing a managed subnet needs every
///   deployed stack off the network first;
/// * a node may not drop its address on a managed network while it runs
///   instances attached to it, nor take an address an instance holds.
pub(crate) fn managed_network_conflict(
    previous: &ClusterConfig,
    new: &ClusterConfig,
    cluster_state: &cl_raft::ClusterState,
) -> Option<String> {
    let files: Vec<(&str, StackFile)> = cluster_state
        .stacks
        .iter()
        .filter_map(|(name, s)| Some((name.as_str(), StackFile::parse(&s.spec).ok()?)))
        .collect();
    let book = cl_net::Allocations::decode(cluster_state.overlay.as_deref());

    for (name, net) in &new.networks {
        let was = previous.networks.get(name);
        let now_managed = net.is_managed();
        let was_managed = was.is_some_and(|w| w.is_managed());
        let subnet_changed = was.is_some_and(|w| w.subnet != net.subnet);

        // A brand new network cannot be attached to anything yet.
        if was.is_some() && (now_managed != was_managed || (now_managed && subnet_changed)) {
            let users: Vec<&str> = files
                .iter()
                .filter(|(_, f)| stack_lists_network(f, name))
                .map(|(s, _)| *s)
                .collect();
            if !users.is_empty() {
                let what = if now_managed != was_managed {
                    "changing whether it is managed"
                } else {
                    "changing its subnet"
                };
                return Some(format!(
                    "{what} for network {name} would break the stacks that list it ({}): \
                     remove the network from those stacks (or undeploy them) first",
                    users.join(", ")
                ));
            }
        }

        let Some(subnet) = net.managed_subnet() else {
            continue;
        };
        if !(was_managed && !subnet_changed) {
            if let Some(book) = &book {
                for other in book.network_names().filter(|k| !cl_net::is_managed_key(k)) {
                    if let Some(taken) = book.subnet(other) {
                        if subnet.contains(&taken.network()) || taken.contains(&subnet.network()) {
                            return Some(format!(
                                "managed network {name} ({subnet}) overlaps the overlay subnet {taken} allocated to {other}"
                            ));
                        }
                    }
                }
            }
            for (stack, file) in &files {
                if let Some(pinned) = cl_sched::run::overlay_subnet_of(file, new) {
                    if subnet.contains(&pinned.network()) || pinned.contains(&subnet.network()) {
                        return Some(format!(
                            "managed network {name} ({subnet}) overlaps the subnet {pinned} stack {stack} pins in x-clouduler.subnet"
                        ));
                    }
                }
            }
        }

        // Node addresses against the instances using the network.
        let key = cl_net::managed_key(name);
        for (node, spec) in &new.nodes {
            let had = previous
                .nodes
                .get(node)
                .is_some_and(|n| n.network.contains_key(name));
            let has = spec.network.contains_key(name);
            if had && !has {
                for assignment in cluster_state.assignments.values() {
                    if assignment.node != *node {
                        continue;
                    }
                    let attached = files.iter().any(|(s, f)| {
                        *s == assignment.instance.stack
                            && f.managed_attachments(new, &assignment.instance.service)
                                .contains(&name.as_str())
                    });
                    if attached
                        || (was_managed && {
                            files.iter().any(|(s, f)| {
                                *s == assignment.instance.stack
                                    && f.managed_attachments(previous, &assignment.instance.service)
                                        .contains(&name.as_str())
                            })
                        })
                    {
                        return Some(format!(
                            "node {node} still runs {} attached to managed network {name}: \
                             move or stop it before removing the node's address on {name}",
                            assignment.instance
                        ));
                    }
                }
            }
            // Taking an address, or moving to another one, that an instance holds.
            let before = previous.nodes.get(node).and_then(|n| n.ip_on(name));
            if before != spec.ip_on(name) {
                if let (Some(std::net::IpAddr::V4(ip)), Some(book)) = (spec.ip_on(name), &book) {
                    if let Some((id, _)) = book.instances(&key).find(|(_, held)| *held == ip) {
                        return Some(format!(
                            "{ip} is held by instance {id} on managed network {name}, pick another address for node {node}"
                        ));
                    }
                }
            }
        }
    }
    None
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
    let is_managed = net.is_managed();
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
    if is_managed {
        // The same checks a config edit gets: the new name is a new managed
        // network as far as overlap and node address rules go.
        let previous = state.cluster_config.read().await.clone();
        if let Some(why) = managed_network_conflict(&previous, &config, &cluster_state) {
            return Err(ApiError::Conflict(why));
        }
    }
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

    // A managed network's overlay is rebuilt under the new name (new key, VNI, devices and
    // docker network) and its containers are recreated on it, so its addresses move to the new
    // key first and every container keeps its address. The leader's overlay pass would adopt
    // them by subnet anyway (`apply_managed`); doing it here just shortens the gap.
    if is_managed {
        move_managed_allocations(&state, &old_name, &new_name).await?;
    }

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

/// Re-key a renamed managed network's address book entry. A no-op when it is already moved.
async fn move_managed_allocations(state: &AppState, old: &str, new: &str) -> Result<(), ApiError> {
    let snapshot = state.cluster.snapshot().await;
    let Some(mut book) = cl_net::Allocations::decode(snapshot.overlay.as_deref()) else {
        return Ok(());
    };
    if !book.rename_managed_network(&cl_net::managed_key(old), &cl_net::managed_key(new)) {
        return Ok(());
    }
    let allocations = serde_json::to_string(&book).map_err(ApiError::internal)?;
    let response = state
        .cluster
        .submit(cl_raft::Request::SetOverlayAllocations { allocations })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Internal(format!(
            "moving the addresses of managed network {old}: {msg}"
        )));
    }
    Ok(())
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

    const MANAGED_YAML: &str = "
cluster: { name: t }
networks:
  int: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com }
  mgd: { type: private, subnet: 192.168.32.0/24, managed: true }
nodes:
  n1:
    id: 3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f01
    role: manager
    network: { int: 10.10.0.1, mgd: 192.168.32.2 }
  n2:
    id: 3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f02
    role: worker
    network: { int: 10.10.0.2, mgd: 192.168.32.3 }
web: { address: cloud, network: int }
";

    fn managed_cluster_state(spec: &str) -> cl_raft::ClusterState {
        let mut state = cl_raft::ClusterState::default();
        state.stacks.insert(
            "blog".into(),
            cl_raft::state::StackState {
                name: "blog".into(),
                spec: spec.into(),
                revision: 1,
                configs: Default::default(),
                secrets: Default::default(),
                used_configs: Default::default(),
                used_secrets: Default::default(),
                deployed_at: 0,
                previous_spec: None,
            },
        );
        let instance = cl_raft::ids::InstanceId::new("blog", "web", 0);
        state.assignments.insert(
            instance.clone(),
            cl_raft::state::Assignment {
                instance,
                node: "n2".into(),
                desired: cl_raft::state::DesiredState::Running,
                image_digest: None,
                migration: None,
            },
        );
        state
    }

    const ATTACHED: &str =
        "networks:\n  mgd: {}\nservices:\n  web:\n    image: x\n    networks: [mgd]\n";

    fn edited(from: &str, to: &str) -> ClusterConfig {
        ClusterConfig::parse(&MANAGED_YAML.replace(from, to)).expect("edited config parses")
    }

    #[test]
    fn flipping_or_moving_a_managed_network_needs_its_stacks_gone() {
        let base = ClusterConfig::parse(MANAGED_YAML).unwrap();
        let state = managed_cluster_state(ATTACHED);

        let mut moved = base.clone();
        moved.networks.get_mut("mgd").unwrap().subnet = Some("192.168.40.0/24".parse().unwrap());
        let why = managed_network_conflict(&base, &moved, &state).expect("refused");
        assert!(
            why.contains("changing its subnet") && why.contains("blog"),
            "{why}"
        );

        let unmanaged = edited(", managed: true", "");
        let why = managed_network_conflict(&base, &unmanaged, &state).expect("refused");
        assert!(why.contains("whether it is managed"), "{why}");
        // Nothing lists the network, so both edits are fine.
        let idle = managed_cluster_state("services:\n  web:\n    image: x\n");
        assert_eq!(managed_network_conflict(&base, &unmanaged, &idle), None);
        assert_eq!(managed_network_conflict(&base, &base, &state), None);
    }

    #[test]
    fn a_node_cannot_drop_its_managed_address_while_it_runs_attached_instances() {
        let base = ClusterConfig::parse(MANAGED_YAML).unwrap();
        let state = managed_cluster_state(ATTACHED);
        let mut dropped = base.clone();
        dropped.nodes.get_mut("n2").unwrap().network.remove("mgd");
        let why = managed_network_conflict(&base, &dropped, &state).expect("refused");
        assert!(
            why.contains("node n2") && why.contains("blog.web.0"),
            "{why}"
        );
        // n1 runs nothing, so it may drop it.
        let mut other = base.clone();
        other.nodes.get_mut("n1").unwrap().network.remove("mgd");
        assert_eq!(managed_network_conflict(&base, &other, &state), None);
    }

    #[test]
    fn a_new_managed_network_may_not_overlap_an_allocated_or_pinned_overlay() {
        let plain = edited(", managed: true", "");
        let base = ClusterConfig::parse(MANAGED_YAML).unwrap();

        let mut book = cl_net::Allocations::default();
        book.allocate_network_in("blog_int", Some("192.168.32.0/25".parse().unwrap()), &[])
            .unwrap();
        let state = cl_raft::ClusterState {
            overlay: Some(serde_json::to_string(&book).unwrap()),
            ..Default::default()
        };
        let why = managed_network_conflict(&plain, &base, &state).expect("refused");
        assert!(why.contains("allocated to blog_int"), "{why}");

        let pinned = managed_cluster_state(
            "x-clouduler: { subnet: 192.168.32.0/26 }\nnetworks:\n  int: {}\nservices:\n  web:\n    image: x\n",
        );
        let why = managed_network_conflict(&plain, &base, &pinned).expect("refused");
        assert!(why.contains("pins"), "{why}");
    }

    #[test]
    fn a_node_may_not_move_onto_an_address_an_instance_holds() {
        let base = ClusterConfig::parse(MANAGED_YAML).unwrap();
        let mut book = cl_net::Allocations::default();
        book.sync_managed_network(
            "@mgd",
            "192.168.32.0/24".parse().unwrap(),
            &base.managed_reserved("mgd"),
        );
        let id = cl_raft::ids::InstanceId::new("blog", "web", 0);
        let held = book.allocate_ip("@mgd", &id).unwrap();
        let state = cl_raft::ClusterState {
            overlay: Some(serde_json::to_string(&book).unwrap()),
            ..Default::default()
        };
        let mut moved = base.clone();
        moved.nodes.get_mut("n2").unwrap().network.insert(
            "mgd".into(),
            cl_config::NodeAddress::Ip(std::net::IpAddr::V4(held)),
        );
        let why = managed_network_conflict(&base, &moved, &state).expect("refused");
        assert!(why.contains("is held by instance"), "{why}");
        assert_eq!(managed_network_conflict(&base, &base, &state), None);
    }

    #[test]
    fn a_node_may_not_take_an_address_an_instance_holds() {
        let mut base = ClusterConfig::parse(MANAGED_YAML).unwrap();
        base.nodes.get_mut("n2").unwrap().network.remove("mgd");
        let full = ClusterConfig::parse(MANAGED_YAML).unwrap();
        let mut book = cl_net::Allocations::default();
        book.sync_managed_network(
            "@mgd",
            "192.168.32.0/24".parse().unwrap(),
            &std::collections::BTreeSet::new(),
        );
        let id = cl_raft::ids::InstanceId::new("blog", "web", 0);
        // .2 is n1's, so the first instance holds .3: exactly n2's new address.
        let _ = book.allocate_ip("@mgd", &id).unwrap();
        let _ = book.allocate_ip("@mgd", &cl_raft::ids::InstanceId::new("blog", "web", 1));
        let state = cl_raft::ClusterState {
            overlay: Some(serde_json::to_string(&book).unwrap()),
            ..Default::default()
        };
        let held = book.instance_ip("@mgd", &id).unwrap();
        let mut taking = full.clone();
        taking.nodes.get_mut("n2").unwrap().network.insert(
            "mgd".into(),
            cl_config::NodeAddress::Ip(std::net::IpAddr::V4(held)),
        );
        if taking.validate().is_ok() {
            let why = managed_network_conflict(&base, &taking, &state).expect("refused");
            assert!(why.contains("is held by instance"), "{why}");
        }
    }
}
