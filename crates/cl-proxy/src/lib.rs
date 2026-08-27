//! Reverse proxy, TLS termination and ACME certificate management.
//!
//! `route` builds the routing table from `x-clouduler.proxy` entries and
//! matches incoming requests to a backend. `middleware` implements the named
//! per-route behaviours. `certs` holds parsed certificates, keyed by fqdn,
//! with a self-signed fallback. `acme` drives DNS-01 issuance and renewal
//! through `cl_dns`. `server` wires all of it into hyper 1.x HTTP and HTTPS
//! listeners.

mod acme;
mod certs;
mod error;
mod forward_auth;
mod health;
mod middleware;
mod route;
mod server;

pub use acme::{
    issue_certificate, load_or_create_account, needs_renewal, renewal_tick, run_renewal_loop,
    AcmeConfig, IssuedCertificate, ZoneResolver, DEFAULT_PROPAGATION_TIMEOUT, LETS_ENCRYPT_STAGING_URL,
    RENEWAL_WINDOW,
};
pub use certs::{ensure_crypto_provider, parse_certified_key, CertResolver, CertStore};
pub use error::{ProxyError, Result};
pub use health::{run as run_health_checks, BackendHealth, HealthTable, CHECK_INTERVAL as HEALTH_CHECK_INTERVAL};
pub use middleware::{
    apply_headers, basic_auth_challenge, check_basic_auth, cors_origin_allowed,
    cors_response_headers, is_cors_preflight, is_upgrade, redirect_https, strip_hop_by_hop,
    strip_prefix, BasicAuthCreds, HeaderOps,
};
pub use route::{Picked, RedirectTarget, Route, RouteOptions, RouteTable, STICKY_COOKIE};
pub use server::{ProxyBody, ProxyServer};
