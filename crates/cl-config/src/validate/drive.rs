use crate::cluster::{ClusterConfig, Drive};
use crate::error::{ConfigError, Result};

impl ClusterConfig {
    pub(super) fn validate_drives(&self) -> Result<()> {
        for (name, drive) in &self.drives {
            if drive.max_parallel_syncs == 0 {
                return Err(ConfigError::invalid(
                    format!("drives.{name}.max_parallel_syncs"),
                    "must be at least 1",
                ));
            }
            check_serving_nodes(self, name, drive)?;
        }
        Ok(())
    }

    pub(super) fn validate_data_dirs(&self) -> Result<()> {
        if !self.data_dir.is_absolute() {
            return Err(ConfigError::invalid("data_dir", "must be an absolute path"));
        }
        for (name, node) in &self.nodes {
            if let Some(dir) = &node.data_dir {
                if !dir.is_absolute() {
                    return Err(ConfigError::invalid(
                        format!("nodes.{name}.data_dir"),
                        "must be an absolute path",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// A drive nobody implements, or a served drive with no server named, is left
/// for the operator to fill in: scheduling and the daemon already skip them.
fn check_serving_nodes(cfg: &ClusterConfig, name: &str, drive: &Drive) -> Result<()> {
    match (drive.is_served(), drive.served_by.is_empty()) {
        (true, true) | (false, true) => Ok(()),
        (false, false) => Err(ConfigError::invalid(
            format!("drives.{name}.served_by"),
            "only a served drive has a serving node, set mode: served",
        )),
        (true, false) => {
            let mut seen = std::collections::HashSet::new();
            for server in &drive.served_by {
                if !seen.insert(server) {
                    return Err(ConfigError::invalid(
                        format!("drives.{name}.served_by"),
                        format!("{server} is listed more than once"),
                    ));
                }
                let Some(node) = cfg.nodes.get(server) else {
                    return Err(ConfigError::invalid(
                        format!("drives.{name}.served_by"),
                        format!("no node named {server}"),
                    ));
                };
                if !node.implements(name) {
                    return Err(ConfigError::invalid(
                        format!("drives.{name}.served_by"),
                        format!("{server} must map {name} to the directory holding its copy"),
                    ));
                }
            }
            Ok(())
        }
    }
}
