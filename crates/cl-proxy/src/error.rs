//! Error type for the whole crate.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, ProxyError>;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("no route matches {host}{path}")]
    NoRoute { host: String, path: String },

    #[error("backend {addr} unreachable: {source}")]
    BackendUnreachable {
        addr: std::net::SocketAddr,
        source: std::io::Error,
    },

    #[error("tls setup failed: {0}")]
    Tls(String),

    #[error("failed to parse certificate: {0}")]
    CertParse(String),

    #[error("acme issuance failed: {0}")]
    Acme(String),

    #[error("acme dns-01 challenge did not propagate in time")]
    ChallengeTimeout,

    #[error("no certificate available for {0}")]
    NoCertificate(String),

    #[error(transparent)]
    Dns(#[from] cl_dns::DnsError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
