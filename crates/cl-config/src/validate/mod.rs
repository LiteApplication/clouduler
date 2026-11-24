mod auth;
mod backup;
mod drive;
mod failover;
mod network;
mod node;
mod proxy;
#[cfg(test)]
mod tests;

use crate::cluster::ClusterConfig;
use crate::error::{ConfigError, Result};

impl ClusterConfig {
    /// Structural checks the rest of the daemon relies on.
    pub fn validate(&self) -> Result<()> {
        self.validate_meta()?;
        self.validate_networks()?;
        self.validate_nodes()?;
        self.validate_drives()?;
        self.validate_web()?;
        self.validate_data_dirs()?;
        self.validate_secrets()?;
        self.validate_auth()?;
        self.validate_failover()?;
        self.validate_cloudflare()?;
        self.validate_proxy()?;
        self.validate_backup()?;
        Ok(())
    }

    fn validate_meta(&self) -> Result<()> {
        if self.cluster.name.trim().is_empty() {
            return Err(ConfigError::invalid("cluster.name", "must not be empty"));
        }
        Ok(())
    }
}
