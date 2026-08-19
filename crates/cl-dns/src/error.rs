//! Errors the Cloudflare client and reconciler can raise.

pub type Result<T> = std::result::Result<T, DnsError>;

/// Cloudflare's own error code for "this record already exists". Not fatal:
/// the reconciler treats it as evidence the record is already in the state
/// we wanted it in.
pub const CF_ERR_RECORD_EXISTS: u32 = 81057;
/// Cloudflare's error codes for duplicate record content on create/update.
pub const CF_ERR_DUPLICATE: &[u32] = &[81044, 81058];

#[derive(Debug, thiserror::Error)]
pub enum DnsError {
    #[error("request to {url} failed: {source}")]
    Transport {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// One error entry from a Cloudflare API response. Cloudflare returns an
    /// array of these; we surface the first as the primary error.
    #[error("cloudflare api error {code}: {message}")]
    Api { code: u32, message: String },

    #[error("cloudflare auth failed: {0}")]
    Auth(String),

    /// Desired state couldn't be computed from the config, e.g. a node with
    /// no address on the requested network. Not a Cloudflare error.
    #[error("{0}")]
    Invalid(String),

    #[error("zone {0} not found")]
    ZoneNotFound(String),

    #[error("record {0} not found")]
    RecordNotFound(String),

    #[error("cannot read api token from {path}: {source}")]
    TokenRead {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("api token file {0} is world readable, refusing to use it")]
    TokenPermissions(std::path::PathBuf),

    #[error("api token file {0} is empty")]
    TokenEmpty(std::path::PathBuf),

    #[error("no api token configured")]
    TokenMissing,

    #[error("gave up after {attempts} attempts: {source}")]
    RetriesExhausted {
        attempts: u32,
        #[source]
        source: Box<DnsError>,
    },

    #[error("timed out waiting for {0} to propagate")]
    PropagationTimeout(String),

    #[error("dns resolution failed: {0}")]
    Resolve(String),

    #[error("refused to delete unowned record {0}")]
    NotOwned(String),
}

impl DnsError {
    /// True for the "already exists" family of Cloudflare error codes, which
    /// the reconciler should swallow rather than fail on.
    pub fn is_benign_duplicate(&self) -> bool {
        match self {
            DnsError::Api { code, .. } => {
                *code == CF_ERR_RECORD_EXISTS || CF_ERR_DUPLICATE.contains(code)
            }
            _ => false,
        }
    }
}
