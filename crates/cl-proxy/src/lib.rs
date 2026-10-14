//! Reverse proxy, TLS termination and ACME certificate management.
//!
//! `route` builds the routing table from `x-clouduler.proxy` entries and
//! matches incoming requests to a backend. `middleware` implements the named
//! per-route behaviours. `certs` holds parsed certificates, keyed by fqdn,
//! with a self-signed fallback. `acme` drives issuance and renewal, normally
//! DNS-01 through `cl_dns`, falling back to HTTP-01 (answered out of
//! `http01`'s in-memory store by the plain-http listener) for a name the
//! caller marks as uncovered by any DNS provider zone. `server` wires all of
//! it into hyper 1.x HTTP and HTTPS listeners.

mod acme;
mod auth_gate;
mod certs;
mod connector;
mod error;
mod forward_auth;
mod health;
mod http01;
mod middleware;
mod route;
mod server;

pub use acme::{
    issue_certificate, issue_certificate_http01, load_or_create_account, needs_renewal,
    renewal_tick, run_renewal_loop, AcmeConfig, IssuedCertificate, ZoneResolver,
    DEFAULT_PROPAGATION_TIMEOUT, LETS_ENCRYPT_STAGING_URL, RENEWAL_WINDOW,
};
pub use auth_gate::AuthGate;
pub use certs::{ensure_crypto_provider, parse_certified_key, CertResolver, CertStore};
pub use error::{ProxyError, Result};
pub use health::{
    run as run_health_checks, BackendHealth, HealthTable, CHECK_INTERVAL as HEALTH_CHECK_INTERVAL,
};
pub use http01::Http01Store;
pub use middleware::{
    apply_headers, basic_auth_challenge, check_basic_auth, cors_origin_allowed,
    cors_response_headers, is_cors_preflight, is_upgrade, redirect_https, strip_hop_by_hop,
    strip_prefix, BasicAuthCreds, HeaderOps,
};
pub use route::{Picked, RedirectTarget, Route, RouteOptions, RouteTable, STICKY_COOKIE};
pub use server::{ProxyBody, ProxyServer};
