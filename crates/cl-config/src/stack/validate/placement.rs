use crate::cluster::ClusterConfig;
use crate::error::{ConfigError, Result};
use crate::stack::StackFile;
use std::collections::BTreeSet;

impl StackFile {
    pub(super) fn validate_drives_exist(&self, cluster: &ClusterConfig) -> Result<()> {
        for (name, svc) in &self.services {
            for (idx, vol) in svc.volumes.iter().enumerate() {
                let Some(drive) = &vol.drive else { continue };
                if !cluster.drives.contains_key(drive) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.volumes[{idx}]"),
                        format!("no drive named {drive} declared in the cluster"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// A volume subpath of exactly `.` binds a drive's real root into the
    /// container (see `cl_drive::registry::resolve_relative`), bypassing the
    /// scoped per-volume path. Only safe with exactly one node holding the real
    /// directory and the service pinned there; otherwise the scheduler could
    /// place it against a different copy and silently diverge.
    pub(super) fn validate_whole_drive_mounts(&self, cluster: &ClusterConfig) -> Result<()> {
        for (name, svc) in &self.services {
            for (idx, vol) in svc.volumes.iter().enumerate() {
                let (Some(drive), Some(volume)) = (&vol.drive, &vol.volume) else {
                    continue;
                };
                if volume != "." {
                    continue;
                }
                let at = format!("services.{name}.volumes[{idx}]");
                let holders = drive_root_holders(cluster, drive);
                let Some(sole) = holders.iter().next().filter(|_| holders.len() == 1) else {
                    return Err(ConfigError::invalid(
                        at,
                        format!(
                            "drive {drive} can be served by more than one node ({}), so mounting {drive}/. \
                             (its whole root) is not allowed: pin a specific volume under it instead",
                            holders.iter().cloned().collect::<Vec<_>>().join(", ")
                        ),
                    ));
                };
                match self.effective_node(name) {
                    Some(node) if &node == sole => {}
                    Some(node) => {
                        return Err(ConfigError::invalid(
                            at,
                            format!(
                                "{name} is pinned to {node}, but only {sole} holds the real directory for drive {drive}; \
                                 mounting {drive}/. needs x-clouduler.node: {sole}"
                            ),
                        ));
                    }
                    None => {
                        return Err(ConfigError::invalid(
                            at,
                            format!(
                                "mounting {drive}/. (its whole root) requires pinning {name} with x-clouduler.node: {sole}, \
                                 the only node holding the real directory for drive {drive}"
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_node_pins(&self, cluster: &ClusterConfig) -> Result<()> {
        if let Some(defaults) = &self.x_clouduler {
            if let Some(node) = &defaults.node {
                if !cluster.nodes.contains_key(node) {
                    return Err(ConfigError::invalid(
                        "x-clouduler.node",
                        format!("no node named {node} in the cluster"),
                    ));
                }
            }
        }
        for name in self.services.keys() {
            let Some(node) = self.effective_node(name) else {
                continue;
            };
            if !cluster.nodes.contains_key(&node) {
                return Err(ConfigError::invalid(
                    format!("services.{name}.x-clouduler.node"),
                    format!("no node named {node} in the cluster"),
                ));
            }
            if let Some(net) = self.egress_network(name, cluster) {
                // A nat network needs no interface address; a pin to any node is fine.
                let is_nat = matches!(
                    cluster.networks.get(&net).map(|n| n.kind),
                    Some(crate::cluster::NetworkKind::Nat | crate::cluster::NetworkKind::Container)
                );
                let implements = is_nat
                    || cluster
                        .nodes
                        .get(&node)
                        .map(|n| {
                            n.network
                                .get(&net)
                                .is_some_and(|a| a.as_interface().is_some())
                        })
                        .unwrap_or(false);
                if !implements {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.x-clouduler.node"),
                        format!("{node} has no interface address on egress network {net}"),
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_placement_constraints(&self, cluster: &ClusterConfig) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(deploy) = &svc.deploy else { continue };
            let Some(placement) = &deploy.placement else {
                continue;
            };
            for (idx, constraint) in placement.constraints.iter().enumerate() {
                check_node_constraint(cluster, name, idx, constraint)?;
            }
        }
        Ok(())
    }
}

/// Only the `node.hostname==X` / `node==X` forms name a node, and that node
/// has to exist.
fn check_node_constraint(
    cluster: &ClusterConfig,
    service: &str,
    idx: usize,
    constraint: &str,
) -> Result<()> {
    let Some(node) = constraint
        .strip_prefix("node.hostname==")
        .or_else(|| constraint.strip_prefix("node=="))
    else {
        return Ok(());
    };
    if !cluster.nodes.contains_key(node) {
        return Err(ConfigError::invalid(
            format!("services.{service}.deploy.placement.constraints[{idx}]"),
            format!("no node named {node} in the cluster"),
        ));
    }
    Ok(())
}

/// Nodes holding a real local directory for `drive_name`: every implementing
/// node for a replicated drive, only the `served_by` candidates for a served
/// one (others get an NFS mountpoint).
fn drive_root_holders(cluster: &ClusterConfig, drive_name: &str) -> BTreeSet<String> {
    let Some(drive) = cluster.drives.get(drive_name) else {
        return BTreeSet::new();
    };
    if drive.is_served() {
        drive
            .served_by
            .iter()
            .filter(|n| {
                cluster
                    .nodes
                    .get(n.as_str())
                    .is_some_and(|node| node.implements(drive_name))
            })
            .cloned()
            .collect()
    } else {
        cluster
            .nodes
            .iter()
            .filter(|(_, node)| node.implements(drive_name))
            .map(|(name, _)| name.clone())
            .collect()
    }
}
