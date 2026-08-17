use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    /// Every validation failure carries the config path that broke, so the
    /// message points at the offending key instead of the whole file.
    #[error("invalid config at {at}: {msg}")]
    Invalid { at: String, msg: String },
}

impl ConfigError {
    pub fn invalid(at: impl Into<String>, msg: impl Into<String>) -> Self {
        ConfigError::Invalid {
            at: at.into(),
            msg: msg.into(),
        }
    }
}
