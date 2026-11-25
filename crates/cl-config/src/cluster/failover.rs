use super::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Timings for handling a node that drops out of the cluster.
///
/// `fence_timeout` (node stops its containers after losing quorum) must be
/// shorter than `node_timeout` (cluster reschedules them), or the same
/// container runs in two places and volumes corrupt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct FailoverConfig {
    /// How long a node may be unreachable before its containers are
    /// rescheduled elsewhere.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "humantime_serde", default = "default_node_timeout")]
    pub node_timeout: Duration,

    /// How long a node may run without a manager quorum before it stops its own
    /// containers. Must be shorter than `node_timeout`.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "humantime_serde", default = "default_fence_timeout")]
    pub fence_timeout: Duration,

    /// How often the leader probes the other nodes.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "humantime_serde", default = "default_probe_interval")]
    pub probe_interval: Duration,

    /// Time a container gets to stop cleanly before being killed, for services
    /// without their own `stop_grace_period`. Counts against the margin between
    /// `fence_timeout` and `node_timeout`.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "humantime_serde", default = "default_stop_grace")]
    pub stop_grace: Duration,

    /// How long a node that is rebooting or powering off may spend pushing its
    /// volumes to other nodes before it goes down. `0s` skips the push.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "humantime_serde", default = "default_shutdown_push_timeout")]
    pub shutdown_push_timeout: Duration,
}

/// Longest `failover.shutdown_push_timeout` accepted.
pub const MAX_SHUTDOWN_PUSH_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

/// Docker's own default is 10 seconds, which is short for a database.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(30);

/// Longest `failover.stop_grace` accepted.
pub const MAX_STOP_GRACE: Duration = Duration::from_secs(600);

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            node_timeout: default_node_timeout(),
            fence_timeout: default_fence_timeout(),
            probe_interval: default_probe_interval(),
            stop_grace: default_stop_grace(),
            shutdown_push_timeout: default_shutdown_push_timeout(),
        }
    }
}

impl FailoverConfig {
    /// A warning when a fence may not finish before the leader reschedules: stop
    /// takes up to the stop grace after `fence_timeout`, and must land inside
    /// `node_timeout`. Pass the longest per-service grace to
    /// `fence_margin_warning_for` to check that too.
    pub fn fence_margin_warning(&self) -> Option<String> {
        self.fence_margin_warning_for(self.stop_grace, "default stop grace")
    }

    /// As `fence_margin_warning`, for an arbitrary `stop_grace` described by
    /// `what` (e.g. a service's own `stop_grace_period`).
    pub fn fence_margin_warning_for(&self, stop_grace: Duration, what: &str) -> Option<String> {
        (self.fence_timeout + stop_grace >= self.node_timeout).then(|| {
            format!(
                "failover.fence_timeout ({:?}) plus the {what} ({:?}) reaches \
                 node_timeout ({:?}): a fence may still be stopping containers when the leader \
                 reschedules them, giving one volume two writers. Raise node_timeout or lower \
                 fence_timeout or the stop grace",
                self.fence_timeout, stop_grace, self.node_timeout
            )
        })
    }
}
