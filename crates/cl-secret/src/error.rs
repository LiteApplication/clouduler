use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, SecretError>;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("cannot read the cluster key at {path}: {source}")]
    ReadKey {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot write the cluster key to {path}: {source}")]
    WriteKey {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("the cluster key at {path} is malformed: {reason}")]
    BadKey { path: PathBuf, reason: String },

    /// Refusing to read a key other users can see is worth an explicit error
    /// rather than a warning nobody notices. Group read is allowed (the
    /// `clouduler` group needs it to operate the cluster from the cli), so
    /// this only fires on stray group-write/execute bits or any bit for
    /// "other".
    #[error("the cluster key at {path} has excess permissions, run: chmod 640 {path}")]
    KeyPermissions { path: PathBuf },

    /// No key on this node, so sealed secrets cannot be opened here.
    #[error("no cluster key on this node, secrets cannot be decrypted")]
    NoKey,

    #[error("malformed sealed secret: {0}")]
    Malformed(String),

    /// Either the wrong key or tampered data. Deliberately does not say which.
    #[error("cannot decrypt the secret, wrong key or corrupted data")]
    Decrypt,
}
