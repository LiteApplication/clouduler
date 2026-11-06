//! Every alert Clouduler can raise. The engine refuses to raise a key that is
//! not listed here, and the UI's Events tab is rendered from this list.

use serde::Serialize;

use crate::settings::Severity;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Cluster,
    Nodes,
    Workloads,
    Storage,
    Backups,
    Edge,
    Deployments,
    Security,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Cluster => "cluster",
            Category::Nodes => "nodes",
            Category::Workloads => "workloads",
            Category::Storage => "storage",
            Category::Backups => "backups",
            Category::Edge => "edge",
            Category::Deployments => "deployments",
            Category::Security => "security",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct EventDef {
    pub key: &'static str,
    pub category: Category,
    pub title: &'static str,
    /// Severity before any escalation rule or override.
    pub severity: Severity,
    /// When the severity changes on its own, in plain words.
    pub note: &'static str,
    /// Has an open and a resolved state, as opposed to a one-shot event.
    pub stateful: bool,
    /// Belongs to a stack, so `x-clouduler.alerts` can mute it.
    pub stack_scoped: bool,
}

const fn ev(
    key: &'static str,
    category: Category,
    title: &'static str,
    severity: Severity,
    note: &'static str,
    stateful: bool,
    stack_scoped: bool,
) -> EventDef {
    EventDef {
        key,
        category,
        title,
        severity,
        note,
        stateful,
        stack_scoped,
    }
}

use Category::*;
use Severity::*;

pub const EVENTS: &[EventDef] = &[
    ev(
        "cluster.quorum_lost",
        Cluster,
        "Quorum lost",
        Critical,
        "Sent directly by the managers, since nothing can be written to raft without quorum",
        true,
        false,
    ),
    ev(
        "cluster.emergency_mode",
        Cluster,
        "Emergency mode is on",
        Critical,
        "",
        true,
        false,
    ),
    ev(
        "cluster.leader_changed",
        Cluster,
        "Raft leader changed",
        Info,
        "Warning if it happens more than 3 times in 10 minutes",
        false,
        false,
    ),
    ev(
        "cluster.node_joined",
        Cluster,
        "Node joined",
        Info,
        "",
        false,
        false,
    ),
    ev(
        "cluster.node_removed",
        Cluster,
        "Node removed",
        Info,
        "",
        false,
        false,
    ),
    ev(
        "node.unreachable",
        Nodes,
        "Node unreachable",
        Critical,
        "Warning for a worker",
        true,
        false,
    ),
    ev(
        "node.disk_low",
        Nodes,
        "Drive folder low on space",
        Warning,
        "Critical past the critical threshold",
        true,
        false,
    ),
    ev(
        "node.memory_pressure",
        Nodes,
        "Sustained memory pressure",
        Warning,
        "",
        true,
        false,
    ),
    ev(
        "instance.crash_loop",
        Workloads,
        "Container is crash looping",
        Critical,
        "",
        true,
        true,
    ),
    ev(
        "instance.oom_killed",
        Workloads,
        "Container killed for using too much memory",
        Warning,
        "Critical if it happens 3 times within an hour",
        false,
        true,
    ),
    ev(
        "instance.exited",
        Workloads,
        "Container not in its desired state",
        Warning,
        "",
        true,
        true,
    ),
    ev(
        "instance.unhealthy",
        Workloads,
        "Container failing its healthcheck",
        Warning,
        "",
        true,
        true,
    ),
    ev(
        "instance.unplaceable",
        Workloads,
        "No node can run this container",
        Warning,
        "",
        true,
        true,
    ),
    ev(
        "instance.unreachable_by_proxy",
        Workloads,
        "The proxy cannot reach a container",
        Warning,
        "",
        true,
        true,
    ),
    ev(
        "instance.pin_drift",
        Workloads,
        "Container runs on another node than its pin",
        Info,
        "",
        true,
        true,
    ),
    ev(
        "log.match",
        Workloads,
        "A watched line appeared in a container's logs",
        Warning,
        "Severity comes from the stack's log rule",
        false,
        true,
    ),
    ev(
        "volume.sync_refused",
        Storage,
        "Sync refused because it would wipe data",
        Critical,
        "",
        true,
        true,
    ),
    ev(
        "volume.migration_failed",
        Storage,
        "Volume migration failed",
        Critical,
        "",
        true,
        true,
    ),
    ev(
        "volume.empty",
        Storage,
        "A volume that should hold data is empty",
        Critical,
        "",
        true,
        true,
    ),
    ev(
        "volume.replica_error",
        Storage,
        "A replica has an error or is behind",
        Warning,
        "",
        true,
        true,
    ),
    ev(
        "volume.migration_completed",
        Storage,
        "Volume migration completed",
        Info,
        "",
        false,
        true,
    ),
    ev(
        "restore.failed",
        Backups,
        "A restore failed",
        Critical,
        "",
        true,
        false,
    ),
    ev(
        "backup.failed",
        Backups,
        "A backup failed",
        Warning,
        "Critical if it keeps failing past 24 hours",
        true,
        false,
    ),
    ev(
        "backup.overdue",
        Backups,
        "No backup succeeded within twice its interval",
        Warning,
        "",
        true,
        false,
    ),
    ev(
        "backup.succeeded_with_warning",
        Backups,
        "A backup succeeded with a warning",
        Info,
        "",
        false,
        false,
    ),
    ev(
        "cert.expiring",
        Edge,
        "Certificate expires soon",
        Warning,
        "Critical close to expiry",
        true,
        false,
    ),
    ev(
        "cert.issuance_failed",
        Edge,
        "Certificate issuance failing",
        Warning,
        "",
        true,
        false,
    ),
    ev(
        "dns.reconcile_failed",
        Edge,
        "DNS records could not be updated",
        Warning,
        "",
        true,
        false,
    ),
    ev(
        "self_update.rolled_back",
        Deployments,
        "A node went back to its previous binary",
        Critical,
        "It now runs a different version from the rest",
        false,
        false,
    ),
    ev(
        "self_update.completed",
        Deployments,
        "Clouduler updated on a node",
        Info,
        "",
        false,
        false,
    ),
    ev(
        "update.failed",
        Deployments,
        "An image update failed",
        Warning,
        "",
        true,
        true,
    ),
    ev(
        "update.stuck",
        Deployments,
        "An image update is stuck",
        Warning,
        "",
        true,
        true,
    ),
    ev(
        "update.available",
        Deployments,
        "A newer image is available",
        Info,
        "",
        false,
        true,
    ),
    ev(
        "update.completed",
        Deployments,
        "An image update completed",
        Info,
        "",
        false,
        true,
    ),
    ev(
        "stack.deployed",
        Deployments,
        "Stack deployed",
        Info,
        "",
        false,
        true,
    ),
    ev(
        "stack.removed",
        Deployments,
        "Stack removed",
        Info,
        "",
        false,
        true,
    ),
    ev(
        "auth.api_key_created",
        Security,
        "API key created",
        Info,
        "",
        false,
        false,
    ),
    ev(
        "auth.api_key_revoked",
        Security,
        "API key revoked",
        Info,
        "",
        false,
        false,
    ),
    ev(
        "secret.rotated",
        Security,
        "A secret was overwritten",
        Info,
        "",
        false,
        false,
    ),
    ev(
        "test",
        Cluster,
        "Test message",
        Info,
        "Sent from the Destinations tab",
        false,
        false,
    ),
];

pub fn find(key: &str) -> Option<&'static EventDef> {
    EVENTS.iter().find(|e| e.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique() {
        let mut keys: Vec<_> = EVENTS.iter().map(|e| e.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), EVENTS.len());
    }
}
