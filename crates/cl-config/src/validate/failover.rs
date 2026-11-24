use crate::cluster::ClusterConfig;
use crate::error::{ConfigError, Result};

impl ClusterConfig {
    pub(super) fn validate_failover(&self) -> Result<()> {
        let f = &self.failover;
        // Fencing must complete before anyone else takes the containers over,
        // otherwise the same volume gets two writers.
        if f.fence_timeout >= f.node_timeout {
            return Err(ConfigError::invalid(
                "failover.fence_timeout",
                format!(
                    "must be shorter than node_timeout ({:?}), or a container can run on two nodes at once",
                    f.node_timeout
                ),
            ));
        }
        if f.probe_interval >= f.fence_timeout {
            return Err(ConfigError::invalid(
                "failover.probe_interval",
                format!(
                    "must be shorter than fence_timeout ({:?}), or a node cannot be missed before it fences",
                    f.fence_timeout
                ),
            ));
        }
        if f.probe_interval.is_zero() {
            return Err(ConfigError::invalid(
                "failover.probe_interval",
                "must not be zero",
            ));
        }
        if f.stop_grace.is_zero() {
            return Err(ConfigError::invalid(
                "failover.stop_grace",
                "must not be zero",
            ));
        }
        if f.stop_grace > crate::cluster::MAX_STOP_GRACE {
            return Err(ConfigError::invalid(
                "failover.stop_grace",
                format!("must be at most {:?}", crate::cluster::MAX_STOP_GRACE),
            ));
        }
        if f.shutdown_push_timeout > crate::cluster::MAX_SHUTDOWN_PUSH_TIMEOUT {
            return Err(ConfigError::invalid(
                "failover.shutdown_push_timeout",
                format!(
                    "must be at most {:?}",
                    crate::cluster::MAX_SHUTDOWN_PUSH_TIMEOUT
                ),
            ));
        }
        Ok(())
    }
}
