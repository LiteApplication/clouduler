//! Validation is split in two: syntax checks that only look at the stack file
//! itself (so a stack can be linted with no cluster at hand), and cross-checks
//! against a `ClusterConfig` for the fields that must resolve against it.

mod network;
mod placement;
mod service;
mod syntax;
#[cfg(test)]
mod tests;

use crate::cluster::ClusterConfig;
use crate::error::Result;
pub use crate::names::validate_stack_name;
use crate::stack::StackFile;

impl StackFile {
    /// Checks that only look at the stack file itself.
    pub fn validate_syntax(&self) -> Result<()> {
        self.validate_service_names()?;
        self.validate_depends_on()?;
        self.validate_env_file()?;
        self.validate_service_networks_declared()?;
        self.validate_no_duplicate_ports()?;
        self.validate_stop_signals()?;
        self.validate_stop_grace_periods()?;
        self.validate_colocate()?;
        self.validate_sync_hooks()?;
        self.validate_writable_secrets()?;
        self.validate_single_config_writer()?;
        self.validate_replicas()?;
        self.validate_memory_sizes()?;
        self.validate_devices()?;
        self.validate_docker_sockets()?;
        self.validate_alerts()?;
        self.validate_terraform()?;
        Ok(())
    }

    /// Full validation: syntax checks plus everything that needs to resolve
    /// against the cluster this stack would run on.
    pub fn validate(&self, cluster: &ClusterConfig) -> Result<()> {
        self.validate_syntax()?;
        self.validate_networks_exist(cluster)?;
        self.validate_networks_implemented(cluster)?;
        self.validate_drives_exist(cluster)?;
        self.validate_whole_drive_mounts(cluster)?;
        self.validate_proxy_expose_networks(cluster)?;
        self.validate_service_network_kinds(cluster)?;
        self.validate_allowed(cluster)?;
        self.validate_network_subnets(cluster)?;
        self.validate_node_pins(cluster)?;
        self.validate_placement_constraints(cluster)?;
        Ok(())
    }

    /// `validate`, plus the checks that need this stack's own name: a service
    /// may not list a container network it is the router of (the router is
    /// attached automatically), and a router service runs exactly one replica.
    pub fn validate_named(&self, name: &str, cluster: &ClusterConfig) -> Result<()> {
        self.validate(cluster)?;
        self.validate_container_router(name, cluster)
    }
}
