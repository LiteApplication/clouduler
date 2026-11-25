use super::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How a drive's data is made available to the nodes using it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum DriveMode {
    /// Every implementing node keeps its own copy, kept in step by periodic
    /// delta sync. Containers read local disk, so this survives a node losing
    /// the network, at the cost of the copies lagging each other.
    #[default]
    Replicated,
    /// One node holds the data and serves it to the others, who mount it over
    /// the internal network. One copy, always consistent, but a container using
    /// it stops working when the serving node goes away.
    Served,
}

/// What a served drive does when the node it prefers comes back after a
/// failover.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum Failback {
    /// Hand the drive back on its own once the preferred node has caught up.
    #[default]
    Auto,
    /// Keep serving from the standby until an operator asks for the handover.
    Manual,
}

impl Failback {
    pub fn is_default(&self) -> bool {
        *self == Failback::Auto
    }
}

/// A cluster wide directory namespace. Nodes opt in by mapping it to a local path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct Drive {
    /// How often this drive syncs: each node's copy for a replicated drive, the
    /// active server's push to the `served_by` standbys for a served drive.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "humantime_serde", default = "default_sync_interval")]
    pub sync_interval: Duration,

    #[serde(default)]
    pub mode: DriveMode,

    /// Nodes that can hold the data for a served drive, in priority order. The
    /// first reachable one is active; the rest keep a synced backup. Each maps
    /// the drive to the real directory; other nodes get a mountpoint pointed at
    /// the active server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub served_by: Vec<String>,

    /// Whether a served drive returns to a preferred node (earlier in `served_by`)
    /// after a failover, once it has copied what was written meanwhile.
    #[serde(default, skip_serializing_if = "Failback::is_default")]
    pub failback: Failback,

    /// How many of this drive's volumes one node replicates at once; the rest
    /// wait, most important stack first.
    #[serde(
        default = "default_max_parallel_syncs",
        skip_serializing_if = "is_default_max_parallel_syncs"
    )]
    pub max_parallel_syncs: usize,
}

impl Drive {
    pub fn is_served(&self) -> bool {
        self.mode == DriveMode::Served
    }
}

impl ClusterConfig {
    /// Names of every drive whose mode is `Served`, so placement and mount
    /// resolution need not load the whole `Drive` record.
    pub fn served_drives(&self) -> std::collections::BTreeSet<String> {
        self.drives
            .iter()
            .filter(|(_, drive)| drive.is_served())
            .map(|(name, _)| name.clone())
            .collect()
    }
}
