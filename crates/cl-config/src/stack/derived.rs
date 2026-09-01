//! Data the scheduler needs derived from a parsed stack, so callers don't
//! recompute it themselves.

use std::collections::{BTreeMap, BTreeSet};
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

    /// The egress, wireguard or nat kind network this service attaches to,
    /// if any. A service attaches to at most one, so the scheduler can pin
    /// it to whichever node implements that network the same way it pins to
    /// a drive.
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

    /// The colocation group key for `service`, if `x-clouduler.colocate`
    /// puts it in one with at least one other service. Transitively closed
    /// over every service's `colocate` list, so `a: [b]`, `b: [c]` still
    /// groups `a`, `b` and `c` together. `None` for a service naming no
    /// group and named by none.
    pub fn colocate_group(&self, service: &str) -> Option<String> {
        self.colocate_groups().remove(service)
    }

    fn colocate_groups(&self) -> BTreeMap<String, String> {
        // Union-find over service names: every service starts as its own
        // root, then each `colocate` entry merges the two roots it names.
        let mut parent: BTreeMap<String, String> =
            self.services.keys().map(|n| (n.clone(), n.clone())).collect();

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
            let Some(ext) = &svc.x_clouduler else { continue };
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
            let key = format!("colocate:{}", members.iter().cloned().collect::<Vec<_>>().join(","));
            for member in members {
                keys.insert(member.clone(), key.clone());
            }
        }
        keys
    }
}

#[cfg(test)]
mod tests {
    use crate::stack::StackFile;

    fn parse(yaml: &str) -> StackFile {
        StackFile::parse(yaml).expect("valid stack")
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

    /// `a` names `b`, `b` names `c`: all three end up in one group even
    /// though `a` never named `c` directly.
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
