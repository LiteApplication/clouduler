//! The routing table: maps an incoming host/path to a backend.
//!
//! Matching precedence, most specific wins:
//! 1. Exact host match beats a wildcard host match.
//! 2. Within a host match, the longest `path_prefix` wins. A route with no
//!    `path_prefix` matches every path under its host (prefix length zero).
//!
//! A wildcard host is written `*.example.com` and matches exactly one extra
//! label (`foo.example.com`, not `example.com` and not `a.b.example.com`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwap;
use cl_config::{Middleware, RedirectRegex};
use ipnet::IpNet;

/// Per-route proxy behaviour that is not about matching or backend choice.
///
/// Split out from `Route` so adding a knob does not mean touching every
/// `Route::new` call site, and so the defaults live in exactly one place.
#[derive(Debug, Clone, Default)]
pub struct RouteOptions {
    /// Reject a request body larger than this with 413. `None` is unlimited.
    pub max_body_size: Option<u64>,
    /// Allow `Upgrade` requests (websockets) through.
    pub websockets: bool,
    /// Forward the client's `Host` rather than rewriting it to the backend.
    pub preserve_host: bool,
    /// Cap on how long the backend has to return response headers.
    pub response_timeout: Option<std::time::Duration>,
    /// Restricts this route to clients whose TCP peer address falls inside
    /// this subnet. Set for a route published on an internal or private
    /// network; `None` for a public one, which is reachable from anywhere.
    pub allowed_subnet: Option<IpNet>,
    /// Additional subnets a client may connect from even when outside
    /// `allowed_subnet`: the overlay subnets of every stack published on
    /// this route's network. A same-node client and backend on that network
    /// can have their traffic delivered locally by the kernel rather than
    /// actually traversing the network, so the peer address `gate_by_network`
    /// sees is the client's raw overlay address, not one inside
    /// `allowed_subnet`. Always empty for a public route (`allowed_subnet` is
    /// `None`, so this is never consulted).
    pub trusted_overlay_subnets: Vec<IpNet>,
    /// The cluster network this route is published on. A caller whose stack
    /// restricted that network (see `PeerPolicies`) may only reach the hosts
    /// it listed. `None` skips that check.
    pub network: Option<String>,
}

impl RouteOptions {
    /// Websockets on, everything else unset — what a route that configures
    /// nothing gets.
    pub fn permissive() -> Self {
        Self {
            websockets: true,
            ..Self::default()
        }
    }
}

/// Where a redirect route sends the client, instead of a backend.
#[derive(Debug, Clone)]
pub struct RedirectTarget {
    /// Destination URL, e.g. `https://new.example.com`.
    pub url: String,
    /// Append the request's original path and query to `url`.
    pub preserve_path: bool,
    /// 301, 302, 307 or 308.
    pub status: u16,
}

/// One published HTTP route, with every replica backing it.
#[derive(Debug)]
pub struct Route {
    pub host: String,
    pub path_prefix: Option<String>,
    pub tls: bool,
    pub middlewares: Vec<Middleware>,
    pub sticky: bool,
    pub options: RouteOptions,
    /// Stack/service/instance label, for logging.
    pub instance: String,
    /// When set, this route answers with a redirect instead of proxying to a
    /// backend; `backends` is unused for it.
    pub redirect: Option<RedirectTarget>,
    backends: Vec<SocketAddr>,
    next: AtomicUsize,
    /// Lazily compiled from `middlewares` on first use rather than at
    /// construction: a `Route` is rebuilt from scratch every reconcile pass
    /// (`RECONCILE_INTERVAL`), so this compiles the pattern at most once per
    /// pass rather than once per request.
    redirect_regex: OnceLock<Option<(regex::Regex, RedirectRegex)>>,
}

impl Route {
    pub fn new(
        host: impl Into<String>,
        path_prefix: Option<String>,
        tls: bool,
        middlewares: Vec<Middleware>,
        sticky: bool,
        instance: impl Into<String>,
        backends: Vec<SocketAddr>,
    ) -> Self {
        Self {
            host: host.into(),
            path_prefix,
            tls,
            middlewares,
            sticky,
            options: RouteOptions::permissive(),
            instance: instance.into(),
            redirect: None,
            backends,
            next: AtomicUsize::new(0),
            redirect_regex: OnceLock::new(),
        }
    }

    /// The route's `redirect_regex` middleware, compiled and cached on first
    /// call. `None` when the route carries no such middleware or its pattern
    /// fails to compile (already validated at parse time in `cl-config`, so
    /// that should never actually happen here).
    pub fn redirect_regex(&self) -> Option<(&regex::Regex, &RedirectRegex)> {
        self.redirect_regex
            .get_or_init(|| {
                self.middlewares.iter().find_map(|m| match m {
                    Middleware::RedirectRegex(cfg) => regex::Regex::new(&cfg.regex)
                        .ok()
                        .map(|re| (re, cfg.clone())),
                    _ => None,
                })
            })
            .as_ref()
            .map(|(re, cfg)| (re, cfg))
    }

    pub fn with_options(mut self, options: RouteOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_redirect(mut self, redirect: RedirectTarget) -> Self {
        self.redirect = Some(redirect);
        self
    }

    fn prefix_len(&self) -> usize {
        self.path_prefix.as_deref().map(str::len).unwrap_or(0)
    }

    fn matches_path(&self, path: &str) -> bool {
        match &self.path_prefix {
            Some(prefix) => path.starts_with(prefix.as_str()),
            None => true,
        }
    }

    pub fn backends(&self) -> &[SocketAddr] {
        &self.backends
    }

    /// Round robin pick, ignoring stickiness.
    fn next_backend(&self) -> Option<SocketAddr> {
        if self.backends.is_empty() {
            return None;
        }
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.backends.len();
        Some(self.backends[i])
    }

    /// Pick a backend for this request. When `sticky` is set and the cookie
    /// names a backend still in the pool, use that one and return no new
    /// cookie to set; otherwise round robin and, if sticky, return the
    /// backend to remember in a cookie.
    pub fn pick(&self, sticky_cookie: Option<&str>) -> Option<Picked> {
        if self.sticky {
            if let Some(want) = sticky_cookie {
                if let Some(addr) = self.backends.iter().find(|a| a.to_string() == want) {
                    return Some(Picked {
                        addr: *addr,
                        set_cookie: None,
                    });
                }
            }
            let addr = self.next_backend()?;
            return Some(Picked {
                addr,
                set_cookie: Some(addr.to_string()),
            });
        }
        Some(Picked {
            addr: self.next_backend()?,
            set_cookie: None,
        })
    }
}

pub struct Picked {
    pub addr: SocketAddr,
    /// New sticky cookie value to set on the response, when the pick changed it.
    pub set_cookie: Option<String>,
}

/// The name of the cookie used to pin a client to a backend.
pub const STICKY_COOKIE: &str = "cl_sticky";

/// The immutable snapshot readers see. Rebuilt wholesale on reload.
#[derive(Debug, Default)]
struct Snapshot {
    exact: HashMap<String, Vec<Arc<Route>>>,
    /// Keyed by the base domain (`example.com` for `*.example.com`).
    wildcard: HashMap<String, Vec<Arc<Route>>>,
}

/// Where the api finds the live routing table. The proxy supervisor rebuilds
/// the listeners (and with them the table) when its config changes, so the
/// table the api should read changes over the process's life; the supervisor
/// publishes each new one here. Cheap to clone: every clone shares the slot.
#[derive(Clone, Default)]
pub struct SharedRoutes(Arc<arc_swap::ArcSwapOption<RouteTable>>);

impl SharedRoutes {
    pub fn set(&self, table: Arc<RouteTable>) {
        self.0.store(Some(table));
    }

    pub fn clear(&self) {
        self.0.store(None);
    }

    /// The current table, `None` while the proxy is off.
    pub fn get(&self) -> Option<Arc<RouteTable>> {
        self.0.load_full()
    }
}

/// Lock-free-for-readers routing table. Reload swaps in a whole new snapshot.
#[derive(Default)]
pub struct RouteTable {
    snapshot: ArcSwap<Snapshot>,
}

impl RouteTable {
    pub fn new(routes: Vec<Route>) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(build(routes)),
        }
    }

    /// Atomically replace the whole table.
    pub fn reload(&self, routes: Vec<Route>) {
        self.snapshot.store(Arc::new(build(routes)));
    }

    /// Every route currently published, exact and wildcard alike. Used by the
    /// health prober, which has to walk the whole table rather than look up
    /// one request's route.
    pub fn all(&self) -> Vec<Arc<Route>> {
        let snap = self.snapshot.load();
        snap.exact
            .values()
            .chain(snap.wildcard.values())
            .flatten()
            .cloned()
            .collect()
    }

    /// Every published route in a stable order (host, then path prefix), for
    /// listing. `all` makes no promise about order.
    pub fn snapshot(&self) -> Vec<Arc<Route>> {
        let mut routes = self.all();
        routes.sort_by(|a, b| {
            (&a.host, &a.path_prefix, &a.instance).cmp(&(&b.host, &b.path_prefix, &b.instance))
        });
        routes
    }

    /// Find the best matching route for a host and path.
    pub fn lookup(&self, host: &str, path: &str) -> Option<Arc<Route>> {
        let snap = self.snapshot.load();
        let host = host.split(':').next().unwrap_or(host);

        if let Some(candidates) = snap.exact.get(host) {
            if let Some(r) = best_path_match(candidates, path) {
                return Some(r);
            }
        }

        // Longest matching wildcard base wins.
        let mut best_base_len = 0usize;
        let mut best: Option<Arc<Route>> = None;
        for (base, candidates) in snap.wildcard.iter() {
            if !wildcard_matches(base, host) {
                continue;
            }
            if let Some(r) = best_path_match(candidates, path) {
                if base.len() > best_base_len {
                    best_base_len = base.len();
                    best = Some(r);
                }
            }
        }
        best
    }
}

fn best_path_match(candidates: &[Arc<Route>], path: &str) -> Option<Arc<Route>> {
    candidates
        .iter()
        .filter(|r| r.matches_path(path))
        .max_by_key(|r| r.prefix_len())
        .cloned()
}

/// `base` is `example.com` for a route registered as `*.example.com`. Matches
/// exactly one extra label: `foo.example.com` yes, `example.com` no,
/// `a.b.example.com` no.
pub(crate) fn wildcard_matches(base: &str, host: &str) -> bool {
    match host.strip_suffix(base) {
        Some(prefix) => {
            prefix.ends_with('.') && prefix.len() > 1 && !prefix[..prefix.len() - 1].contains('.')
        }
        None => false,
    }
}

fn build(routes: Vec<Route>) -> Snapshot {
    let mut snap = Snapshot::default();
    for route in routes {
        let route = Arc::new(route);
        if let Some(base) = route.host.strip_prefix("*.") {
            snap.wildcard
                .entry(base.to_string())
                .or_default()
                .push(route);
        } else {
            snap.exact
                .entry(route.host.clone())
                .or_default()
                .push(route);
        }
    }
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn route(host: &str, prefix: Option<&str>, backends: Vec<SocketAddr>) -> Route {
        Route::new(
            host,
            prefix.map(String::from),
            true,
            vec![],
            false,
            "test",
            backends,
        )
    }

    #[test]
    fn exact_host_wins_over_wildcard() {
        let table = RouteTable::new(vec![
            route("app.example.com", None, vec![addr(1)]),
            route("*.example.com", None, vec![addr(2)]),
        ]);
        let got = table.lookup("app.example.com", "/").unwrap();
        assert_eq!(got.backends(), &[addr(1)]);
    }

    #[test]
    fn wildcard_matches_single_label() {
        let table = RouteTable::new(vec![route("*.example.com", None, vec![addr(2)])]);
        assert!(table.lookup("foo.example.com", "/").is_some());
        assert!(table.lookup("example.com", "/").is_none());
        assert!(table.lookup("a.b.example.com", "/").is_none());
    }

    #[test]
    fn longest_path_prefix_wins() {
        let table = RouteTable::new(vec![
            route("app.example.com", None, vec![addr(1)]),
            route("app.example.com", Some("/api"), vec![addr(2)]),
            route("app.example.com", Some("/api/v2"), vec![addr(3)]),
        ]);
        assert_eq!(
            table
                .lookup("app.example.com", "/api/v2/things")
                .unwrap()
                .backends(),
            &[addr(3)]
        );
        assert_eq!(
            table
                .lookup("app.example.com", "/api/v1")
                .unwrap()
                .backends(),
            &[addr(2)]
        );
        assert_eq!(
            table
                .lookup("app.example.com", "/other")
                .unwrap()
                .backends(),
            &[addr(1)]
        );
    }

    #[test]
    fn host_with_no_prefix_matches_everything() {
        let table = RouteTable::new(vec![route("app.example.com", None, vec![addr(1)])]);
        assert!(table
            .lookup("app.example.com", "/anything/deep/path")
            .is_some());
    }

    #[test]
    fn no_match_returns_none() {
        let table = RouteTable::new(vec![route("app.example.com", None, vec![addr(1)])]);
        assert!(table.lookup("other.example.com", "/").is_none());
    }

    #[test]
    fn host_header_port_is_ignored() {
        let table = RouteTable::new(vec![route("app.example.com", None, vec![addr(1)])]);
        assert!(table.lookup("app.example.com:8443", "/").is_some());
    }

    #[test]
    fn round_robin_distributes_across_backends() {
        let r = route("app.example.com", None, vec![addr(1), addr(2), addr(3)]);
        let picks: Vec<SocketAddr> = (0..6).map(|_| r.pick(None).unwrap().addr).collect();
        assert_eq!(
            picks,
            vec![addr(1), addr(2), addr(3), addr(1), addr(2), addr(3)]
        );
    }

    #[test]
    fn sticky_cookie_pins_to_same_backend() {
        let r = Route::new(
            "app.example.com",
            None,
            true,
            vec![],
            true,
            "test",
            vec![addr(1), addr(2)],
        );
        let first = r.pick(None).unwrap();
        let cookie = first.set_cookie.expect("first pick sets a cookie");
        assert_eq!(cookie, first.addr.to_string());

        // Subsequent requests carrying that cookie stay pinned, no new cookie.
        for _ in 0..5 {
            let picked = r.pick(Some(&cookie)).unwrap();
            assert_eq!(picked.addr, first.addr);
            assert!(picked.set_cookie.is_none());
        }
    }

    #[test]
    fn sticky_cookie_for_unknown_backend_falls_back_to_round_robin() {
        let r = Route::new(
            "app.example.com",
            None,
            true,
            vec![],
            true,
            "test",
            vec![addr(1), addr(2)],
        );
        let picked = r.pick(Some("127.0.0.1:9999")).unwrap();
        assert!(picked.set_cookie.is_some());
    }

    #[test]
    fn reload_is_atomic_and_visible_immediately() {
        let table = RouteTable::new(vec![route("app.example.com", None, vec![addr(1)])]);
        table.reload(vec![route("app.example.com", None, vec![addr(2)])]);
        assert_eq!(
            table.lookup("app.example.com", "/").unwrap().backends(),
            &[addr(2)]
        );
    }
}
