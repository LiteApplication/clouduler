pub type Result<T> = std::result::Result<T, BackupError>;

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("malformed age recipient: {0}")]
    BadRecipient(String),

    #[error("malformed age identity: {0}")]
    BadIdentity(String),

    #[error("age encryption failed: {0}")]
    Encrypt(String),

    #[error("age decryption failed: {0}")]
    Decrypt(String),

    #[error("failed to generate an ssh keypair: {0}")]
    KeyGen(String),

    #[error("git operation failed: {0}")]
    Git(#[from] git2::Error),

    #[error(
        "remote ssh host key for {host} does not match the one pinned on first connection - \
         possible man-in-the-middle, or the remote rotated its key (delete {known_hosts} to re-pin)"
    )]
    HostKeyMismatch { host: String, known_hosts: std::path::PathBuf },

    #[error("cluster state has no {0} deploy key entry")]
    MissingDeployKey(&'static str),

    #[error("no local export commit yet to force-push - wait for the first export tick to run")]
    NoLocalExportCommit,

    #[error("cannot decode {what}: {reason}")]
    Malformed { what: String, reason: String },

    #[error(transparent)]
    Secret(#[from] cl_secret::SecretError),

    #[error("io error at {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}
