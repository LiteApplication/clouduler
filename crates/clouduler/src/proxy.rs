//! Wiring the reverse proxy into the daemon.
//!
//! Every node runs the listeners and every node holds the whole routing table,
//! because a route's hostname can resolve to any node and the backend it
//! points at is reachable from all of them over the overlay. Certificates come
//! out of replicated state for the same reason: whichever node a client lands
//! on has to be able to terminate TLS for the name it asked for.
//!
//! Issuance and renewal are the leader's alone. Several nodes ordering the
//! same certificate at once would burn the CA's rate limit and race each other
//! writing the result back.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cl_config::{qualify, ClusterConfig};
use cl_dns::Cloudflare;
use cl_proxy::{
    ensure_crypto_provider, issue_certificate, AcmeConfig, CertStore, Route, RouteTable,
    ZoneResolver, DEFAULT_PROPAGATION_TIMEOUT, LETS_ENCRYPT_STAGING_URL,
};
use cl_raft::node::ClusterNode;
use cl_raft::state::{CertificateState, ClusterState, Request};
use tracing::{info, warn};

/// How often routes and certificates are rebuilt from state.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// How often the leader looks for a name with no certificate, and for one
/// near expiry. Nothing is gained by checking a 30 day renewal window more
/// often than this, and a newly deployed route waits at most one pass.
const ISSUANCE_INTERVAL: Duration = Duration::from_secs(300);

/// Handles for everything the proxy runs, so shutdown can stop it.
pub struct Proxy {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Proxy {
    pub fn shutdown(self) {
        for task in self.tasks {
            task.abort();
        }
    }
}

pub fn start(cfg: &ClusterConfig, cluster: &Arc<ClusterNode>, data_dir: &Path) -> Option<Proxy> {
    if !cfg.proxy.enabled {
        info!("the reverse proxy is disabled");
        return None;
    }
    ensure_crypto_provider();

    let routes = Arc::new(RouteTable::default());
    let certs = Arc::new(CertStore::new());
    let server = Arc::new(cl_proxy::ProxyServer::new(routes.clone(), certs.clone()));
    let mut tasks = Vec::new();

    for (port, tls) in [(cfg.proxy.http_port, false), (cfg.proxy.https_port, true)] {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let server = server.clone();
        tasks.push(tokio::spawn(async move {
            let served = if tls {
                server.serve_https(addr).await
            } else {
                server.serve_http(addr).await
            };
            if let Err(e) = served {
                // Ports below 1024 need root, and that is the usual reason.
                warn!(%addr, tls, error = %e, "proxy listener stopped");
            }
        }));
        info!(%addr, tls, "proxy listening");
    }

    tasks.push(tokio::spawn(reconcile_loop(
        cfg.clone(),
        cluster.clone(),
        routes,
        certs,
    )));

    if let Some(acme) = acme_setup(cfg, data_dir) {
        tasks.push(tokio::spawn(run_acme(acme, cfg.clone(), cluster.clone())));
    }

    Some(Proxy { tasks })
}

/// Rebuild the routing table and the certificate store from replicated state.
async fn reconcile_loop(
    cfg: ClusterConfig,
    cluster: Arc<ClusterNode>,
    routes: Arc<RouteTable>,
    certs: Arc<CertStore>,
) {
    let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
    let mut published = 0;

    loop {
        ticker.tick().await;
        let state = cluster.state().await;

        let built = routes_for(&cfg, &state);
        if built.len() != published {
            info!(routes = built.len(), "reverse proxy routes changed");
            published = built.len();
        }
        routes.reload(built);

        let stored: Vec<CertificateState> = state.certificates.values().cloned().collect();
        certs.load(&stored);
    }
}

/// The routing table as replicated state describes it.
///
/// A route with no backend is still published: the proxy answering 502 for a
/// host it knows about is a better answer than 404, and it means a redeploy
/// does not make the hostname briefly look unconfigured.
fn routes_for(cfg: &ClusterConfig, state: &ClusterState) -> Vec<Route> {
    let mut routes = Vec::new();

    for placement in crate::placement::service_placements(state) {
        let instance = format!("{}.{}", placement.stack, placement.service);
        for entry in &placement.extensions.proxy {
            let domain = cfg.networks.get(&entry.network).and_then(|n| n.domain.as_deref());
            routes.push(Route::new(
                qualify(&entry.host, domain),
                entry.path_prefix.clone(),
                entry.tls,
                entry.middlewares.clone(),
                entry.sticky.unwrap_or(false),
                instance.clone(),
                placement
                    .addresses
                    .iter()
                    .map(|ip| SocketAddr::from((*ip, entry.port)))
                    .collect(),
            ));
        }
    }
    routes
}

/// Every hostname the proxy terminates TLS for, and so needs a certificate.
fn tls_hosts(cfg: &ClusterConfig, state: &ClusterState) -> BTreeSet<String> {
    let mut hosts: BTreeSet<String> = routes_for(cfg, state)
        .into_iter()
        .filter(|r| r.tls)
        .map(|r| r.host)
        .collect();
    // The ui is served by the api on its own port, but its certificate is
    // issued the same way and stored in the same place.
    hosts.insert(cfg.web_fqdn());
    hosts
}

struct Acme {
    config: AcmeConfig,
    credentials: PathBuf,
    cloudflare: Cloudflare,
    zones: Vec<String>,
}

/// Read the ACME setup out of the config, or say why there is none.
fn acme_setup(cfg: &ClusterConfig, data_dir: &Path) -> Option<Acme> {
    if !cfg.proxy.acme.enabled {
        return None;
    }
    let Some(cloudflare) = &cfg.cloudflare else {
        // Rejected by validation, so only reachable through a config that
        // was already stored when the rule did not exist.
        warn!("certificate issuance needs a cloudflare section");
        return None;
    };
    let token = match cl_dns::load_token(cloudflare) {
        Ok(Some(token)) => token,
        Ok(None) => {
            info!("no cloudflare token yet, certificates will not be issued");
            return None;
        }
        Err(e) => {
            warn!(error = %e, "cannot read the cloudflare token");
            return None;
        }
    };

    let mut config = AcmeConfig {
        contact_emails: cfg.proxy.acme.contact_email.clone().into_iter().collect(),
        ..AcmeConfig::default()
    };
    if let Some(url) = &cfg.proxy.acme.directory_url {
        config.directory_url = url.clone();
    }

    Some(Acme {
        config,
        credentials: data_dir.join("acme-account.json"),
        cloudflare: Cloudflare::new(token),
        zones: cloudflare.zones.clone(),
    })
}

/// Issue what is missing, then renew what is expiring, for as long as this
/// node leads. One loop rather than two, so a certificate is never ordered
/// twice in a pass by an issuing task and a renewing one disagreeing about
/// what is stored.
async fn run_acme(acme: Acme, cfg: ClusterConfig, cluster: Arc<ClusterNode>) {
    let account = match cl_proxy::load_or_create_account(&acme.config, &acme.credentials).await {
        Ok(account) => account,
        Err(e) => {
            warn!(error = %e, "cannot use the acme account, no certificates will be issued");
            return;
        }
    };
    if acme.config.directory_url == LETS_ENCRYPT_STAGING_URL {
        warn!("issuing against the acme staging directory, certificates will not be trusted");
    }

    let zones = match Zones::resolve(&acme.cloudflare, &acme.zones).await {
        Ok(zones) => Arc::new(zones),
        Err(e) => {
            warn!(error = %e, "cannot resolve the cloudflare zones");
            return;
        }
    };

    let mut ticker = tokio::time::interval(ISSUANCE_INTERVAL);
    loop {
        ticker.tick().await;
        if !cluster.is_leader() {
            continue;
        }
        issue_missing(&cfg, &cluster, &account, &acme.cloudflare, zones.as_ref()).await;
        cl_proxy::renewal_tick(
            &cluster,
            &account,
            &acme.cloudflare,
            zones.as_ref(),
            DEFAULT_PROPAGATION_TIMEOUT,
        )
        .await;
    }
}

/// Order a certificate for every tls hostname that has none yet. Leader only,
/// checked by the caller.
async fn issue_missing(
    cfg: &ClusterConfig,
    cluster: &Arc<ClusterNode>,
    account: &instant_acme::Account,
    cloudflare: &Cloudflare,
    zones: &Zones,
) {
    let state = cluster.state().await;
    for host in tls_hosts(cfg, &state) {
        if state.certificates.contains_key(&host) {
            continue;
        }
        let Some(zone_id) = zones.zone_id_for(&host) else {
            warn!(%host, "no cloudflare zone covers this name, not issuing");
            continue;
        };
        info!(%host, "ordering a certificate");
        match issue_certificate(
            account,
            cloudflare,
            &zone_id,
            std::slice::from_ref(&host),
            DEFAULT_PROPAGATION_TIMEOUT,
        )
        .await
        {
            Ok(issued) => {
                let request = Request::StoreCertificate(CertificateState {
                    fqdn: host.clone(),
                    chain_pem: issued.chain_pem,
                    key_pem: issued.key_pem,
                    not_after: issued.not_after,
                });
                if let Err(e) = cluster.submit(request).await {
                    warn!(%host, error = %e, "could not store the issued certificate");
                }
            }
            Err(e) => warn!(%host, error = %e, "certificate issuance failed"),
        }
    }
}

/// Cloudflare zone ids, resolved once at startup. `ZoneResolver` is
/// synchronous, so the lookup cannot happen while a certificate is being
/// ordered.
struct Zones {
    zones: Vec<(String, String)>,
}

impl Zones {
    async fn resolve(cf: &Cloudflare, names: &[String]) -> cl_dns::Result<Self> {
        let mut zones = Vec::new();
        for name in names {
            zones.push((name.clone(), cf.zone_id(name).await?));
        }
        Ok(Self { zones })
    }
}

impl ZoneResolver for Zones {
    /// Longest matching zone wins, so a delegated subdomain zone is preferred
    /// over the parent it sits under.
    fn zone_id_for(&self, fqdn: &str) -> Option<String> {
        self.zones
            .iter()
            .filter(|(name, _)| fqdn == name || fqdn.ends_with(&format!(".{name}")))
            .max_by_key(|(name, _)| name.len())
            .map(|(_, id)| id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placement::fixtures::{state_with, SPEC};

    fn cfg() -> ClusterConfig {
        ClusterConfig::parse(include_str!("../../../examples/cluster.yaml")).expect("example parses")
    }

    #[test]
    fn a_route_carries_every_replica_as_a_backend() {
        let routes = routes_for(&cfg(), &state_with(SPEC, 2));
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].backends().len(), 2);
        assert_eq!(routes[0].backends()[0].port(), 80);
    }

    /// A bare host is qualified with the domain of the network it is
    /// published on, which is how `blog` becomes a name a browser can reach.
    #[test]
    fn a_bare_host_gets_the_network_domain() {
        let routes = routes_for(&cfg(), &state_with(SPEC, 1));
        assert_eq!(routes[0].host, "blog.example.com");
    }

    /// The table must still name the host while nothing is running, or a
    /// redeploy turns a 502 into a 404 and clients cache the wrong thing.
    #[test]
    fn a_route_with_no_backends_is_still_published() {
        let mut state = state_with(SPEC, 1);
        state.assignments.clear();
        let routes = routes_for(&cfg(), &state);
        assert_eq!(routes.len(), 1);
        assert!(routes[0].backends().is_empty());
    }

    #[test]
    fn tls_hosts_include_the_web_ui() {
        let cfg = cfg();
        let hosts = tls_hosts(&cfg, &state_with(SPEC, 1));
        assert!(hosts.contains(&cfg.web_fqdn()));
        assert!(hosts.contains("blog.example.com"));
    }

    #[test]
    fn the_longest_matching_zone_wins() {
        let zones = Zones {
            zones: vec![
                ("example.com".into(), "parent".into()),
                ("int.example.com".into(), "child".into()),
            ],
        };
        assert_eq!(zones.zone_id_for("a.int.example.com").as_deref(), Some("child"));
        assert_eq!(zones.zone_id_for("a.example.com").as_deref(), Some("parent"));
        assert_eq!(zones.zone_id_for("example.org"), None);
    }
}
