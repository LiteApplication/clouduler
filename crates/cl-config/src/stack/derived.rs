//! Data the scheduler needs derived from a parsed stack, so callers don't
//! recompute it themselves.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::cluster::{ClusterConfig, NetworkKind};

use super::{Service, StackFile, DEFAULT_SYNC_INTERVAL};

impl Service {
    /// Drives this service mounts a volume from.
    pub fn drives_used(&self) -> BTreeSet<String> {
        self.volumes.iter().filter_map(|v| v.drive.clone()).collect()
    }

    fn auto_update(&self) -> Option<bool> {
        self.x_clouduler.as_ref().and_then(|x| x.auto_update)
    }

    fn node(&self) -> Option<&str> {
        self.x_clouduler.as_ref().and_then(|x| x.node.as_deref())
    }

    fn volume_sync_interval(&self, volume: &str) -> Option<Duration> {
        self.x_clouduler
            .as_ref()
            .and_then(|x| x.volume_sync.get(volume))
            .and_then(|o| o.interval)
    }
}

impl StackFile {
    /// Union of every drive any service mounts a volume from.
    pub fn drives_used(&self) -> BTreeSet<String> {
        self.services.values().flat_map(Service::drives_used).collect()
    }

    fn stack_auto_update(&self) -> Option<bool> {
        self.x_clouduler.as_ref().and_then(|x| x.auto_update)
    }

    fn stack_node(&self) -> Option<&str> {
        self.x_clouduler.as_ref().and_then(|x| x.node.as_deref())
    }

    fn stack_volume_sync_interval(&self, volume: &str) -> Option<Duration> {
        self.x_clouduler
            .as_ref()
            .and_then(|x| x.volume_sync.get(volume))
            .and_then(|o| o.interval)
    }

    /// Whether `:latest` images are followed automatically for this service:
    /// the service's own setting, else the stack wide default, else false.
    pub fn effective_auto_update(&self, service: &str) -> bool {
        self.services
            .get(service)
            .and_then(Service::auto_update)
            .or_else(|| self.stack_auto_update())
            .unwrap_or(false)
    }

    /// The node this service is pinned to, if any: the service's own pin,
    /// else the stack wide default.
    pub fn effective_node(&self, service: &str) -> Option<String> {
        self.services
            .get(service)
            .and_then(Service::node)
            .map(str::to_string)
            .or_else(|| self.stack_node().map(str::to_string))
    }

    /// The egress or wireguard kind network this service attaches to, if
    /// any. A service attaches to at most one, so the scheduler can pin it
    /// to whichever node implements that network the same way it pins to a
    /// drive.
    pub fn egress_network(&self, service: &str, cluster: &ClusterConfig) -> Option<String> {
        let svc = self.services.get(service)?;
        svc.networks
            .iter()
            .find(|net| {
                matches!(
                    cluster.networks.get(net.as_str()).map(|n| n.kind),
                    Some(NetworkKind::Egress | NetworkKind::Wireguard)
                )
            })
            .cloned()
    }

    /// How often `volume` on `service` should replicate: the per-volume
    /// override on the service, then the stack wide default for that volume,
    /// then the backing drive's own default.
    pub fn sync_interval(&self, service: &str, volume: &str, cluster: &ClusterConfig) -> Duration {
        let Some(svc) = self.services.get(service) else {
            return DEFAULT_SYNC_INTERVAL;
        };
        if let Some(interval) = svc.volume_sync_interval(volume) {
            return interval;
        }
        if let Some(interval) = self.stack_volume_sync_interval(volume) {
            return interval;
        }
        svc.volumes
            .iter()
            .find(|v| v.volume.as_deref() == Some(volume))
            .and_then(|v| v.drive.as_ref())
            .and_then(|drive| cluster.drives.get(drive))
            .map(|d| d.sync_interval)
            .unwrap_or(DEFAULT_SYNC_INTERVAL)
    }
}
