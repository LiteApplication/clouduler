//! Active backend health checks.
//!
//! The routing table alone can't tell "nobody has hit this route yet" apart
//! from "the next real request would 502" — a route sits idle between
//! deploys as easily as a genuinely broken one does. This probes every
//! route's backends itself on a timer and remembers what it found, so
//! `clouduler stack unhealthy` and the ui can surface a dead backend before a
//! client discovers it the hard way.
//!
//! Deliberately independent of the request path in `server.rs`: reusing that
//! code would mean synthesizing a fake client request, and the failure modes
//! this cares about (connect refused, timeout, TLS on a route that expects
//! plain HTTP) are exactly what a plain probe request already reproduces.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tracing::debug;

use crate::route::{Route, RouteTable};

/// How long a single backend probe has to connect and get a response before
/// it's counted as unreachable. Generous relative to `BACKEND_CONNECT_TIMEOUT`
/// in `server.rs` (5s to connect) since this also waits for a full response.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often every route in the table gets re-probed.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// One route's most recent probe.
#[derive(Debug, Clone)]
pub struct BackendHealth {
    pub host: String,
    pub checked_at: SystemTime,
    /// `None` when the probe reached a backend and got a response, whatever
    /// its status; `Some(reason)` in the same shape a real client would have
    /// seen from the proxy's own `bad gateway: {reason}` response.
    pub error: Option<String>,
}

impl BackendHealth {
    pub fn healthy(&self) -> bool {
        self.error.is_none()
    }
}

/// Latest probe result per route, keyed by `Route::instance`. Cheap to clone:
/// every clone shares the same table.
#[derive(Clone, Default)]
pub struct HealthTable(Arc<Mutex<HashMap<String, BackendHealth>>>);

impl HealthTable {
    pub fn get(&self, instance: &str) -> Option<BackendHealth> {
        self.0
            .lock()
            .expect("proxy health lock")
            .get(instance)
            .cloned()
    }

    fn record(&self, instance: String, health: BackendHealth) {
        self.0
            .lock()
            .expect("proxy health lock")
            .insert(instance, health);
    }

    /// Drops entries for routes no longer in the table, so a removed stack
    /// doesn't linger as a stale unhealthy row forever.
    fn retain_known(&self, known: &std::collections::HashSet<String>) {
        self.0
            .lock()
            .expect("proxy health lock")
            .retain(|k, _| known.contains(k));
    }
}

/// Probes every route in `routes` every `interval`, forever.
///
/// `tokio::time::interval` fires its first tick immediately, which would mean
/// probing the whole cluster's routes the instant this node's proxy comes
/// up - exactly when the VXLAN FDB and ARP caches are coldest and a probe is
/// most likely to eat a one-off timeout that has nothing to do with the
/// backend actually being down. Waiting out one interval before the first
/// pass gives the overlay a chance to warm up first.
pub async fn run(routes: Arc<RouteTable>, health: HealthTable, interval: Duration) {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(PROBE_TIMEOUT)
        // Probes for one backend are `interval` apart - far too infrequent
        // for a pooled connection to earn its keep, and confirmed live to
        // actively hurt: a backend whose first probe landed mid-startup (TCP
        // accepted, then reset before a response) left a dead connection in
        // the pool that every retry and every later tick kept reusing,
        // reporting the same route unreachable indefinitely even after a
        // plain `curl` to the identical address succeeded instantly. Each
        // probe gets a fresh connection instead.
        .pool_max_idle_per_host(0)
        .build()
        .expect("building the proxy health check client");

    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let all = routes.all();
        let known = all.iter().map(|r| r.instance.clone()).collect();
        health.retain_known(&known);

        for route in all {
            // Redirects and routes with no configured backends (a redirect
            // rule, an unresolved static host) never proxy anywhere, so
            // there's nothing to probe.
            if route.redirect.is_some() || route.backends().is_empty() {
                continue;
            }
            // On its own task so a panic in one route's probe (a reqwest
            // internal bug, an unexpected header value, anything) can't take
            // this loop down with it. Losing the loop wedges every route's
            // health at whatever it last recorded, forever — confirmed live:
            // one route's `BackendHealth` sat unchanged for over an hour
            // (many `interval`s) reporting an address the backend hadn't
            // held since its last restart, and only a process restart
            // cleared it. `spawn` isolates the panic to this route's probe.
            let instance = route.instance.clone();
            let client = client.clone();
            let health = health.clone();
            tokio::spawn(async move {
                let result = probe_route(&client, &route).await;
                health.record(instance, result);
            });
        }
    }
}

/// A failed probe gets this many extra attempts before the backend is
/// actually recorded as unreachable. Head-end replication over the VXLAN
/// mesh (see `cl-net::vxlan`) means a cross-node probe occasionally eats a
/// one-off dropped packet on an otherwise healthy backend - confirmed live:
/// a route that failed its first post-restart probe came back healthy on
/// the very next attempt seconds later, with nothing on the backend having
/// changed. Retrying in-place tells that apart from a backend that is
/// actually down, which keeps failing every attempt.
const PROBE_RETRIES: u32 = 2;

/// Wait between retry attempts on the same backend.
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// Probes every backend behind one route; the route is healthy only if all
/// of them are, since a client can land on any of them.
async fn probe_route(client: &reqwest::Client, route: &Route) -> BackendHealth {
    let mut error = None;
    // Probe the route's own `path_prefix` rather than always `/`: a backend
    // mounted under a prefix (headplane's react-router basename is
    // `/admin/`, matching its route's `path_prefix`) has no route at all for
    // bare `/` and answers with its own "no route matches" error, which read
    // as the backend being unhealthy when it was actually fine.
    let path = route.path_prefix.as_deref().unwrap_or("/");
    for addr in route.backends() {
        // The proxy always speaks plain HTTP to a backend regardless of
        // whether the route terminates TLS for clients (see `server.rs`).
        let url = format!("http://{addr}{path}");
        let mut last_error = None;
        for attempt in 0..=PROBE_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            match client
                .get(&url)
                .header("host", route.host.clone())
                .send()
                .await
            {
                Ok(_) => {
                    last_error = None;
                    break;
                }
                Err(e) => {
                    debug!(host = %route.host, %addr, attempt, error = %e, "proxy health check attempt failed");
                    last_error = Some(e);
                }
            }
        }
        if let Some(e) = last_error {
            error = Some(format!("{addr} unreachable: {e}"));
            break;
        }
    }
    BackendHealth {
        host: route.host.clone(),
        checked_at: SystemTime::now(),
        error,
    }
}
