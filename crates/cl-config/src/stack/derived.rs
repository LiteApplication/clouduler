//! Data the scheduler needs derived from a parsed stack, so callers don't
//! recompute it themselves.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::cluster::{ClusterConfig, NetworkKind};

use super::{AutoUpdate, AutoUpdateSetting, Service, StackFile, DEFAULT_SYNC_INTERVAL};

impl Service {
    /// `deploy.resources.reservations.devices`.
    pub fn device_requests(&self) -> &[super::devices::DeviceRequest] {
        self.deploy
            .as_ref()
            .and_then(|d| d.resources.as_ref())
            .and_then(|r| r.reservations.as_ref())
            .map(|r| r.devices.as_slice())
            .unwrap_or_default()
    }

    /// `devices:` entries that parse. Invalid ones are skipped here;
    /// validation is what rejects them.
    pub fn device_mappings(&self) -> Vec<super::devices::DeviceMapping> {
        self.devices
            .iter()
            .filter_map(|e| super::devices::DeviceMapping::parse(e).ok())
            .collect()
    }

    /// Drives this service mounts a volume from.
    pub fn drives_used(&self) -> BTreeSet<String> {
        self.volumes
            .iter()
            .filter_map(|v| v.drive.clone())
            .collect()
    }

    /// `deploy.resources.limits.memory` in bytes. `None` when no limit is set;
    /// the rebalancer then balances by raw instance count.
    pub fn memory_limit_bytes(&self) -> Option<u64> {
        self.deploy
            .as_ref()?
            .resources
            .as_ref()?
            .limits
            .as_ref()?
            .memory_bytes()
    }

    fn auto_update(&self) -> Option<AutoUpdateSetting> {
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

    fn backup_stop_before(&self) -> Option<bool> {
        self.x_clouduler
            .as_ref()
            .and_then(|x| x.backup.as_ref())
            .and_then(|b| b.stop_before)
    }
}

impl StackFile {
    /// `x-clouduler.priority`: lower is more important, 0 when unset.
    pub fn priority(&self) -> i32 {
        self.x_clouduler
            .as_ref()
            .and_then(|x| x.priority)
            .unwrap_or(0)
    }

    /// Union of every drive any service mounts a volume from.
    pub fn drives_used(&self) -> BTreeSet<String> {
        self.services
            .values()
            .flat_map(Service::drives_used)
            .collect()
    }

    fn stack_auto_update(&self) -> Option<AutoUpdateSetting> {
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

    fn stack_backup_stop_before(&self) -> Option<bool> {
        self.x_clouduler
            .as_ref()
            .and_then(|x| x.backup.as_ref())
            .and_then(|b| b.stop_before)
    }

    /// Whether backing up a drive this service uses should stop it first: the
    /// service's override, else the stack default, else `None` (the caller
    /// resolves the rest; see `cl_raft::state::DriveBackupTarget::stop_containers`).
    pub fn effective_backup_stop_before(&self, service: &str) -> Option<bool> {
        self.services
            .get(service)
            .and_then(Service::backup_stop_before)
            .or_else(|| self.stack_backup_stop_before())
    }

    /// Alert settings for one service's containers: the stack block with the
    /// service's block merged over it.
    pub fn effective_alerts(&self, service: &str) -> super::StackAlerts {
        super::StackAlerts::merged(
            self.x_clouduler.as_ref().and_then(|x| x.alerts.as_ref()),
            self.services
                .get(service)
                .and_then(|s| s.x_clouduler.as_ref())
                .and_then(|x| x.alerts.as_ref()),
            service,
        )
    }

    /// Newer-image policy for this service: the stack default with the service's
    /// own setting layered over it per field; off with no delay when neither says.
    pub fn effective_auto_update(&self, service: &str) -> AutoUpdate {
        let stack = self.stack_auto_update();
        match self.services.get(service).and_then(Service::auto_update) {
            Some(own) => own.over(stack),
            None => stack.unwrap_or_default(),
        }
        .resolve()
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

    /// The egress, wireguard or nat network this service attaches to, if any (at
    /// most one), so the scheduler can pin it to a node implementing it.
    pub fn egress_network(&self, service: &str, cluster: &ClusterConfig) -> Option<String> {
        let svc = self.services.get(service)?;
        svc.networks
            .iter()
            .find(|net| {
                matches!(
                    cluster.networks.get(net.as_str()).map(|n| n.kind),
                    Some(NetworkKind::Egress | NetworkKind::Wireguard | NetworkKind::Nat)
                )
            })
            .cloned()
    }

    /// The container networks `service` lists, in declaration order.
    pub fn container_networks(&self, service: &str, cluster: &ClusterConfig) -> Vec<String> {
        let Some(svc) = self.services.get(service) else {
            return Vec::new();
        };
        svc.networks
            .iter()
            .filter(|net| {
                cluster.networks.get(net.as_str()).map(|n| n.kind) == Some(NetworkKind::Container)
            })
            .cloned()
            .collect()
    }

    /// Replication interval for `volume` on `service`: per-volume service
    /// override, then stack default, then the drive's own default.
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

    /// The `x-clouduler.volume_sync` override for volume `drive/volume`, if any.
    /// The shortest wins when services disagree (one schedule per volume). Not clamped.
    pub fn volume_sync_override(&self, drive: &str, volume: &str) -> Option<Duration> {
        self.services
            .iter()
            .filter(|(_, svc)| {
                svc.volumes.iter().any(|v| {
                    v.drive.as_deref() == Some(drive) && v.volume.as_deref() == Some(volume)
                })
            })
            .filter_map(|(_, svc)| {
                svc.volume_sync_interval(volume)
                    .or_else(|| self.stack_volume_sync_interval(volume))
            })
            .min()
    }

    /// The colocation group key for `service`, if `x-clouduler.colocate` groups it
    /// with another service. Transitively closed (`a: [b]`, `b: [c]` groups all
    /// three). `None` when ungrouped.
    pub fn colocate_group(&self, service: &str) -> Option<String> {
        self.colocate_groups().remove(service)
    }

    fn colocate_groups(&self) -> BTreeMap<String, String> {
        // Union-find over service names: every service starts as its own
        // root, then each `colocate` entry merges the two roots it names.
        let mut parent: BTreeMap<String, String> = self
            .services
            .keys()
            .map(|n| (n.clone(), n.clone()))
            .collect();

        fn find(parent: &mut BTreeMap<String, String>, x: &str) -> String {
            let p = parent.get(x).cloned().unwrap_or_else(|| x.to_string());
            if p == x {
                p
            } else {
                let root = find(parent, &p);
                parent.insert(x.to_string(), root.clone());
                root
            }
        }

        for (name, svc) in &self.services {
            let Some(ext) = &svc.x_clouduler else {
                continue;
            };
            for other in &ext.colocate {
                // An unknown target is caught by validation; here it's just
                // skipped rather than joined against nothing.
                if !self.services.contains_key(other) {
                    continue;
                }
                let ra = find(&mut parent, name);
                let rb = find(&mut parent, other);
                if ra != rb {
                    parent.insert(ra, rb);
                }
            }
        }

        let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for name in self.services.keys() {
            let root = find(&mut parent, name);
            groups.entry(root).or_default().insert(name.clone());
        }

        let mut keys = BTreeMap::new();
        for members in groups.values() {
            // A singleton needs no anchor: it colocates with nothing.
            if members.len() < 2 {
                continue;
            }
            let key = format!(
                "colocate:{}",
                members.iter().cloned().collect::<Vec<_>>().join(",")
            );
            for member in members {
                keys.insert(member.clone(), key.clone());
            }
        }
        keys
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn priority_defaults_to_zero_and_reads_the_stack_block() {
        let plain = StackFile::parse("services:\n  a:\n    image: x\n").unwrap();
        assert_eq!(plain.priority(), 0);
        let ranked =
            StackFile::parse("x-clouduler:\n  priority: -1\nservices:\n  a:\n    image: x\n")
                .unwrap();
        assert_eq!(ranked.priority(), -1);
    }

    use crate::stack::StackFile;

    fn parse(yaml: &str) -> StackFile {
        StackFile::parse(yaml).expect("valid stack")
    }

    #[test]
    fn backup_stop_before_defers_to_the_service_then_the_stack_then_none() {
        let file = parse(
            r#"
x-clouduler:
  backup:
    stop_before: true
services:
  a:
    image: x
    x-clouduler:
      backup:
        stop_before: false
  b:
    image: x
  c:
    image: x
"#,
        );
        assert_eq!(file.effective_backup_stop_before("a"), Some(false));
        assert_eq!(file.effective_backup_stop_before("b"), Some(true));
        assert_eq!(file.effective_backup_stop_before("c"), Some(true));

        let bare = parse("services:\n  a:\n    image: x\n");
        assert_eq!(bare.effective_backup_stop_before("a"), None);
    }

    #[test]
    fn a_service_naming_no_colocate_group_gets_no_key() {
        let file = parse("services:\n  a:\n    image: x\n");
        assert_eq!(file.colocate_group("a"), None);
    }

    #[test]
    fn colocated_services_share_one_key() {
        let file = parse(
            "services:\n  a:\n    image: x\n    x-clouduler:\n      colocate: [b]\n  b:\n    image: x\n",
        );
        let key = file.colocate_group("a").expect("a is grouped");
        assert_eq!(file.colocate_group("b"), Some(key));
    }

    /// `a` names `b`, `b` names `c`: all three group together.
    #[test]
    fn colocation_is_transitive() {
        let file = parse(
            r#"
services:
  a:
    image: x
    x-clouduler:
      colocate: [b]
  b:
    image: x
    x-clouduler:
      colocate: [c]
  c:
    image: x
"#,
        );
        let key = file.colocate_group("a").expect("grouped");
        assert_eq!(file.colocate_group("b"), Some(key.clone()));
        assert_eq!(file.colocate_group("c"), Some(key));
    }

    #[test]
    fn unrelated_services_get_different_keys() {
        let file = parse(
            r#"
services:
  a:
    image: x
    x-clouduler:
      colocate: [b]
  b:
    image: x
  c:
    image: x
    x-clouduler:
      colocate: [d]
  d:
    image: x
"#,
        );
        assert_ne!(file.colocate_group("a"), file.colocate_group("c"));
    }
}
