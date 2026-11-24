//! Read-only queries over the cluster state and the volume usage helpers.

use super::*;

impl ClusterState {
    pub(super) fn assignment_mut(
        &mut self,
        instance: &InstanceId,
    ) -> Result<&mut Assignment, String> {
        self.assignments
            .get_mut(instance)
            .ok_or_else(|| format!("{instance} is not assigned"))
    }

    pub(super) fn node_implements(&self, node: &str, drive: &str) -> bool {
        self.nodes.get(node).is_some_and(|n| n.implements(drive))
    }

    /// Whether a node may take new work: not held back by the operator and still heard from.
    pub fn can_schedule(&self, node: &str) -> bool {
        self.nodes.get(node).is_some_and(|n| n.schedulable()) && !self.unreachable.contains(node)
    }

    /// The core scheduling invariant: an instance only lands on a node that takes
    /// work and implements every *replicated* drive its volumes live on. A served
    /// drive has one authoritative copy reachable by any node, so it never narrows
    /// candidates; this mirrors the carve-out in `cl_sched::place::rejection` and
    /// the two must agree or a scheduler-chosen served-drive placement would be rejected here.
    pub(super) fn check_placement(
        &self,
        node: &str,
        drives: &BTreeSet<String>,
    ) -> Result<(), String> {
        let target = self
            .nodes
            .get(node)
            .ok_or_else(|| format!("unknown node {node}"))?;
        if !target.schedulable() {
            return Err(format!("node {node} is {:?}", target.availability));
        }
        if self.unreachable.contains(node) {
            return Err(format!("node {node} is unreachable"));
        }
        let served = self.served_drives();
        for drive in drives {
            if !served.contains(drive) && !target.implements(drive) {
                return Err(format!("node {node} does not implement drive {drive}"));
            }
        }
        Ok(())
    }

    /// Drive names whose mode is `served`, parsed from the replicated config's raw
    /// yaml with a minimal local shape (see `ClusterConfigEntry`). Empty if no
    /// config is published or it fails to parse, like `cl_sched::run::served_drives`.
    pub(super) fn served_drives(&self) -> BTreeSet<String> {
        #[derive(Deserialize)]
        struct Shape {
            #[serde(default)]
            drives: BTreeMap<String, DriveShape>,
        }
        #[derive(Deserialize, Default)]
        struct DriveShape {
            #[serde(default)]
            mode: String,
        }
        let Some(entry) = &self.cluster_config else {
            return BTreeSet::new();
        };
        let Ok(shape) = serde_yaml::from_str::<Shape>(&entry.yaml) else {
            return BTreeSet::new();
        };
        shape
            .drives
            .into_iter()
            .filter(|(_, d)| d.mode == "served")
            .map(|(name, _)| name)
            .collect()
    }

    /// Stacks referencing a config, so removing one in use is refused.
    pub(super) fn config_in_use(&self, name: &str) -> Option<String> {
        self.stacks
            .values()
            .find(|s| s.configs.iter().any(|c| c == name))
            .map(|s| format!("stack {}", s.name))
    }

    pub(super) fn secret_in_use(&self, name: &str) -> Option<String> {
        self.stacks
            .values()
            .find(|s| s.secrets.iter().any(|c| c == name))
            .map(|s| format!("stack {}", s.name))
    }

    /// Whether `drive` is declared in the replicated config at all (served or replicated).
    pub(super) fn drive_known(&self, drive: &str) -> bool {
        #[derive(Deserialize)]
        struct Shape {
            #[serde(default)]
            drives: BTreeMap<String, serde_yaml::Value>,
        }
        let Some(entry) = &self.cluster_config else {
            return false;
        };
        let Ok(shape) = serde_yaml::from_str::<Shape>(&entry.yaml) else {
            return false;
        };
        shape.drives.contains_key(drive)
    }

    /// Backup targets referencing a destination, so removing one in use is refused.
    pub(super) fn destination_in_use(&self, id: &str) -> Option<String> {
        self.restic_targets
            .values()
            .find(|t| t.destination_id == id)
            .map(|t| format!("backup target {}", t.id))
    }

    /// A copy with every secret blanked, for anything that leaves the node; lengths stay right so the ui can show a secret is set.
    pub fn redacted(&self) -> ClusterState {
        let mut copy = self.clone();
        for secret in copy.secrets.values_mut() {
            secret.sealed = String::new();
        }
        for tf in copy.terraform_states.values_mut() {
            tf.sealed = String::new();
        }
        for cert in copy.certificates.values_mut() {
            cert.key_pem = String::new();
        }
        // Login material, including saved client addresses: the ui never needs it and
        // the admin list reads the unredacted state.
        copy.proxy_sessions.clear();
        copy.proxy_tokens.clear();
        copy
    }

    /// A stack's priority for the scheduler and drive syncs: lower is more important; prioritized beats all.
    pub fn effective_priority(&self, stack: &str, own: i32) -> i32 {
        if self.prioritized.contains(stack) {
            i32::MIN
        } else {
            own
        }
    }

    pub fn in_emergency(&self) -> bool {
        !self.emergency_stacks.is_empty()
    }

    /// Whether `stack` may be scheduled: always outside emergency mode, only emergency stacks during it.
    pub fn emergency_allows(&self, stack: &str) -> bool {
        self.emergency_stacks.is_empty() || self.emergency_stacks.contains(stack)
    }

    pub fn assignments_on<'a>(&'a self, node: &'a str) -> impl Iterator<Item = &'a Assignment> {
        self.assignments.values().filter(move |a| a.node == node)
    }
}

/// Base64 helpers, so the attachment encoding is defined in one place.
pub fn base64_encode(raw: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(raw)
}

pub fn base64_decode(encoded: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| e.to_string())
}

pub fn dns_key(fqdn: &str, record_type: &str, content: &str) -> String {
    format!("{fqdn}|{record_type}|{content}")
}

/// Stacks whose spec mounts the volume (or the whole drive), whether or not an instance is assigned.
pub fn stacks_declaring_volume(state: &ClusterState, id: &crate::ids::VolumeId) -> Vec<String> {
    use cl_config::StackFile;

    state
        .stacks
        .iter()
        .filter(|(_, stack)| {
            StackFile::parse(&stack.spec).is_ok_and(|file| {
                file.services.values().any(|svc| {
                    svc.volumes.iter().any(|m| {
                        m.drive.as_deref() == Some(id.drive.as_str())
                            && matches!(m.volume.as_deref(), Some(v) if v == id.volume || v == ".")
                    })
                })
            })
        })
        .map(|(name, _)| name.clone())
        .collect()
}

pub fn instances_using_volume(
    state: &ClusterState,
    id: &crate::ids::VolumeId,
) -> Vec<crate::ids::InstanceId> {
    use cl_config::StackFile;

    let mut found = Vec::new();
    for assignment in state.assignments.values() {
        let instance = &assignment.instance;
        let Some(stack) = state.stacks.get(&instance.stack) else {
            continue;
        };
        let Ok(file) = StackFile::parse(&stack.spec) else {
            continue;
        };
        let Some(service) = file.services.get(&instance.service) else {
            continue;
        };
        let uses = service.volumes.iter().any(|m| {
            m.drive.as_deref() == Some(id.drive.as_str())
                && m.volume.as_deref() == Some(id.volume.as_str())
        });
        if uses {
            found.push(instance.clone());
        }
    }
    found
}

/// Instances that must be down while `volumes` is overwritten or edited: those
/// mounting one of them, plus those mounting the whole drive root (`.`) of the
/// same drive, which contains every volume on it.
pub fn instances_touching_volumes(
    state: &ClusterState,
    volumes: &[crate::ids::VolumeId],
) -> Vec<crate::ids::InstanceId> {
    let mut found: BTreeSet<crate::ids::InstanceId> = BTreeSet::new();
    for volume in volumes {
        found.extend(instances_using_volume(state, volume));
        if volume.volume != "." {
            found.extend(instances_using_volume(
                state,
                &crate::ids::VolumeId::new(volume.drive.clone(), ".".to_string()),
            ));
        }
    }
    found.into_iter().collect()
}

impl ClusterState {
    /// Running instances on nodes other than `node` that use `volume`
    /// (directly or through a whole-drive mount): what a freeze on `node`
    /// has to stop.
    pub fn running_elsewhere(
        &self,
        node: &str,
        volume: &crate::ids::VolumeId,
    ) -> Vec<crate::ids::InstanceId> {
        instances_touching_volumes(self, std::slice::from_ref(volume))
            .into_iter()
            .filter(|i| {
                self.assignments
                    .get(i)
                    .is_some_and(|a| a.desired == DesiredState::Running && a.node != node)
            })
            .collect()
    }

    /// Reachable nodes implementing the volume's drive whose copy is behind its current generation.
    pub fn replicas_behind(&self, volume: &crate::ids::VolumeId) -> Vec<String> {
        let Some(v) = self.volumes.get(volume) else {
            return Vec::new();
        };
        self.nodes
            .iter()
            .filter(|(name, n)| n.implements(&volume.drive) && !self.unreachable.contains(*name))
            .filter(|(name, _)| v.replicas.get(*name).copied().unwrap_or(0) < v.generation)
            .map(|(name, _)| name.clone())
            .collect()
    }
}
