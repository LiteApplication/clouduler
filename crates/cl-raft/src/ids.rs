use std::fmt;
use std::str::FromStr;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// Raft node id. Derived from the node's config-declared uuid (`cl_config::Node::id`)
/// so it stays stable across restarts and config edits, unlike an index into
/// the node list, but changes on every re-provisioning of that node - which is
/// the point: it's what lets a removed node's name be reused without the
/// new instance colliding with the old one's removed-tombstone.
pub type NodeId = u64;

/// FNV-1a over the uuid's bytes.
pub fn node_id(id: uuid::Uuid) -> NodeId {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    // Zero is reserved so an unset id is never mistaken for a real node.
    if hash == 0 {
        1
    } else {
        hash
    }
}

/// One running copy of a service. `index` is the replica number within the service.
///
/// Serialized as `stack.service.index` because these are used as map keys and
/// json only takes string keys.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstanceId {
    pub stack: String,
    pub service: String,
    pub index: u32,
}

impl InstanceId {
    pub fn new(stack: impl Into<String>, service: impl Into<String>, index: u32) -> Self {
        Self {
            stack: stack.into(),
            service: service.into(),
            index,
        }
    }

    /// Container name clouduler gives this instance.
    pub fn container_name(&self) -> String {
        format!("{}_{}_{}", self.stack, self.service, self.index)
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.stack, self.service, self.index)
    }
}

impl FromStr for InstanceId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Split from the right: stack and service are DNS labels, so the last
        // two dots are always the separators.
        let mut parts = s.rsplitn(3, '.');
        let index = parts.next().and_then(|i| i.parse().ok());
        let service = parts.next();
        let stack = parts.next();
        match (stack, service, index) {
            (Some(stack), Some(service), Some(index)) if !stack.is_empty() && !service.is_empty() => {
                Ok(InstanceId::new(stack, service, index))
            }
            _ => Err(format!("{s} is not a stack.service.index instance id")),
        }
    }
}

impl Serialize for InstanceId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for InstanceId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse().map_err(de::Error::custom)
    }
}

/// A volume, which always lives inside a drive. Serialized as `drive/volume`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VolumeId {
    pub drive: String,
    pub volume: String,
}

impl VolumeId {
    pub fn new(drive: impl Into<String>, volume: impl Into<String>) -> Self {
        Self {
            drive: drive.into(),
            volume: volume.into(),
        }
    }
}

impl fmt::Display for VolumeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.drive, self.volume)
    }
}

impl FromStr for VolumeId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once('/') {
            Some((drive, volume)) if !drive.is_empty() && !volume.is_empty() => {
                Ok(VolumeId::new(drive, volume))
            }
            _ => Err(format!("{s} is not a drive/volume id")),
        }
    }
}

impl Serialize for VolumeId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for VolumeId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse().map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ids_are_stable_and_distinct() {
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        assert_eq!(node_id(a), node_id(a));
        assert_ne!(node_id(a), node_id(b));
        assert_ne!(node_id(uuid::Uuid::nil()), 0);
    }

    #[test]
    fn instance_names_are_predictable() {
        let id = InstanceId::new("gitlab", "web", 0);
        assert_eq!(id.container_name(), "gitlab_web_0");
        assert_eq!(id.to_string(), "gitlab.web.0");
    }

    #[test]
    fn volume_ids_print_as_drive_paths() {
        assert_eq!(VolumeId::new("fast", "data").to_string(), "fast/data");
    }

    #[test]
    fn ids_round_trip_through_their_string_form() {
        let instance = InstanceId::new("gitlab", "web", 3);
        assert_eq!(instance.to_string().parse::<InstanceId>().unwrap(), instance);

        let volume = VolumeId::new("fast", "data");
        assert_eq!(volume.to_string().parse::<VolumeId>().unwrap(), volume);
    }

    #[test]
    fn malformed_ids_are_rejected() {
        for bad in ["gitlab.web", "gitlab.web.x", ".web.0", "gitlab..0"] {
            assert!(bad.parse::<InstanceId>().is_err(), "{bad} should not parse");
        }
        for bad in ["fast", "/data", "fast/"] {
            assert!(bad.parse::<VolumeId>().is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn ids_serialize_as_json_strings() {
        let instance = InstanceId::new("gitlab", "web", 0);
        assert_eq!(
            serde_json::to_string(&instance).unwrap(),
            "\"gitlab.web.0\""
        );
        assert_eq!(
            serde_json::from_str::<InstanceId>("\"gitlab.web.0\"").unwrap(),
            instance
        );
    }
}
