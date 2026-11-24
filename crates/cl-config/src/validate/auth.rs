use crate::cluster::ClusterConfig;
use crate::error::{ConfigError, Result};

impl ClusterConfig {
    pub(super) fn validate_secrets(&self) -> Result<()> {
        if !self.secrets.key_file.is_absolute() {
            return Err(ConfigError::invalid(
                "secrets.key_file",
                "must be an absolute path",
            ));
        }
        if !self.secrets.node_key_file.is_absolute() {
            return Err(ConfigError::invalid(
                "secrets.node_key_file",
                "must be an absolute path",
            ));
        }
        Ok(())
    }

    pub(super) fn validate_auth(&self) -> Result<()> {
        let Some(oidc) = &self.auth.oidc else {
            // With no identity provider, api keys are the only way in.
            if !self.auth.api_keys {
                return Err(ConfigError::invalid(
                    "auth.api_keys",
                    "cannot be disabled without an oidc provider, that would lock everyone out",
                ));
            }
            return Ok(());
        };

        if !oidc.issuer.starts_with("https://") {
            return Err(ConfigError::invalid(
                "auth.oidc.issuer",
                "must be an https url",
            ));
        }
        if oidc.client_id.trim().is_empty() {
            return Err(ConfigError::invalid(
                "auth.oidc.client_id",
                "must not be empty",
            ));
        }
        if !oidc.scopes.iter().any(|s| s == "openid") {
            return Err(ConfigError::invalid(
                "auth.oidc.scopes",
                "must include openid",
            ));
        }
        Ok(())
    }

    pub(super) fn validate_web(&self) -> Result<()> {
        if self.web.address.trim().is_empty() {
            return Err(ConfigError::invalid("web.address", "must not be empty"));
        }
        let Some(net) = self.networks.get(&self.web.network) else {
            return Err(ConfigError::invalid(
                "web.network",
                format!("no network named {}", self.web.network),
            ));
        };
        if net.is_managed() && self.addressed_nodes_on(&self.web.network).is_empty() {
            return Err(ConfigError::invalid(
                "web.network",
                format!(
                    "{} is a managed network and no node has an IP address on it; the dashboard is served from node addresses, so give at least one node an address there or pick another network",
                    self.web.network
                ),
            ));
        }
        if net.domain.is_none() && !self.web.address.contains('.') {
            return Err(ConfigError::invalid(
                "web.address",
                format!(
                    "network {} has no domain, so {} cannot be qualified",
                    self.web.network, self.web.address
                ),
            ));
        }
        // The api binds both, so they cannot be the same number. Caught here
        // rather than at bind time, where it surfaces as one listener silently
        // missing.
        if self.web.backend_port == self.web.port {
            return Err(ConfigError::invalid(
                "web.backend_port",
                "must differ from web.port",
            ));
        }
        Ok(())
    }
}
