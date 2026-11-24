use crate::cluster::ClusterConfig;
use crate::error::{ConfigError, Result};

impl ClusterConfig {
    pub(super) fn validate_backup(&self) -> Result<()> {
        let Some(b) = &self.backup else {
            return Ok(());
        };
        if b.repo_url.trim().is_empty() {
            return Err(ConfigError::invalid("backup.repo_url", "must not be empty"));
        }
        if b.repo_url.starts_with("http://") || b.repo_url.starts_with("https://") {
            return Err(ConfigError::invalid(
                "backup.repo_url",
                "only ssh urls are supported (e.g. git@host:org/repo.git), not http(s)",
            ));
        }
        if b.branch.trim().is_empty() {
            return Err(ConfigError::invalid("backup.branch", "must not be empty"));
        }
        if !b.age_recipient.starts_with("age1") {
            return Err(ConfigError::invalid(
                "backup.age_recipient",
                "must be an age X25519 recipient, starting with age1",
            ));
        }
        Ok(())
    }
}
