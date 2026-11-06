//! Host devices a service asks for: the compose `deploy.resources.reservations
//! .devices` device requests (NVIDIA GPUs) and the service-level `devices:`
//! list (`/dev/dri` and friends, for Intel and AMD iGPUs).

use serde::{Deserialize, Serialize};

/// The only driver a device request may name. Placement can only check
/// NVIDIA GPUs, so any other driver is refused instead of being passed to
/// docker unchecked.
pub const NVIDIA_DRIVER: &str = "nvidia";

/// `count:` of a device request: a number, or `all` (`-1` also reads as all).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "CountRepr", into = "CountRepr")]
pub enum DeviceCount {
    All,
    Exactly(u32),
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum CountRepr {
    Number(i64),
    Word(String),
}

impl TryFrom<CountRepr> for DeviceCount {
    type Error = String;

    fn try_from(repr: CountRepr) -> Result<Self, Self::Error> {
        match repr {
            CountRepr::Word(w) if w.eq_ignore_ascii_case("all") => Ok(DeviceCount::All),
            CountRepr::Word(w) => Err(format!("count must be a number or \"all\", got \"{w}\"")),
            CountRepr::Number(-1) => Ok(DeviceCount::All),
            CountRepr::Number(n) if n >= 1 => u32::try_from(n)
                .map(DeviceCount::Exactly)
                .map_err(|_| format!("count {n} is too large")),
            CountRepr::Number(n) => Err(format!("count must be at least 1, got {n}")),
        }
    }
}

impl From<DeviceCount> for CountRepr {
    fn from(count: DeviceCount) -> Self {
        match count {
            DeviceCount::All => CountRepr::Word("all".to_string()),
            DeviceCount::Exactly(n) => CountRepr::Number(i64::from(n)),
        }
    }
}

/// One entry of `deploy.resources.reservations.devices`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct DeviceRequest {
    /// Only `nvidia` is supported; unset means `nvidia`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// How many devices. Defaults to all when neither this nor `device_ids`
    /// is given. Not allowed together with `device_ids`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<DeviceCount>,
    /// GPU indexes or uuids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_ids: Vec<String>,
    /// Capability sets, as docker takes them: a list of lists. The compose
    /// shorthand `[gpu]` (one flat list) is one set.
    #[serde(
        default,
        deserialize_with = "capabilities",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub capabilities: Vec<Vec<String>>,
}

fn capabilities<'de, D>(deserializer: D) -> Result<Vec<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Item {
        One(String),
        Set(Vec<String>),
    }

    let items = Vec::<Item>::deserialize(deserializer)?;
    let mut flat = Vec::new();
    let mut sets = Vec::new();
    for item in items {
        match item {
            Item::One(s) => flat.push(s),
            Item::Set(s) => sets.push(s),
        }
    }
    match (flat.is_empty(), sets.is_empty()) {
        (_, true) => Ok(if flat.is_empty() { vec![] } else { vec![flat] }),
        (true, false) => Ok(sets),
        (false, false) => Err(serde::de::Error::custom(
            "capabilities mixes plain names and lists, use one form",
        )),
    }
}

impl DeviceRequest {
    /// The driver docker is asked for.
    pub fn driver_name(&self) -> &str {
        self.driver.as_deref().unwrap_or(NVIDIA_DRIVER)
    }

    /// `count` as the docker API takes it: -1 means all. Unset is all, unless
    /// `device_ids` names the devices.
    pub fn docker_count(&self) -> i64 {
        match (self.count, self.device_ids.is_empty()) {
            (Some(DeviceCount::Exactly(n)), _) => i64::from(n),
            (Some(DeviceCount::All), _) | (None, true) => -1,
            (None, false) => 0,
        }
    }

    /// The least GPUs a node must have for this request: the number asked
    /// for, the number of ids, or one for "all".
    pub fn min_gpus(&self) -> usize {
        match self.count {
            Some(DeviceCount::Exactly(n)) => n as usize,
            _ if !self.device_ids.is_empty() => self.device_ids.len(),
            _ => 1,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Some(driver) = &self.driver {
            if driver != NVIDIA_DRIVER {
                return Err(format!(
                    "driver \"{driver}\" is not supported, only \"{NVIDIA_DRIVER}\""
                ));
            }
        }
        if self.count.is_some() && !self.device_ids.is_empty() {
            return Err("count and device_ids cannot both be set".to_string());
        }
        if self.device_ids.iter().any(|id| id.trim().is_empty()) {
            return Err("device_ids has an empty entry".to_string());
        }
        if self.capabilities.is_empty() || self.capabilities.iter().any(Vec::is_empty) {
            return Err("capabilities is required, e.g. [gpu]".to_string());
        }
        Ok(())
    }
}

/// A parsed service-level `devices:` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceMapping {
    pub host: String,
    pub container: String,
    /// Any of `r`, `w`, `m`. `rwm` when the entry gives none.
    pub permissions: String,
}

impl DeviceMapping {
    /// Parse `HOST[:CONTAINER[:PERMISSIONS]]`. The host path must be under
    /// `/dev/`: host binds are deliberately unsupported, and this must not
    /// become a way around that.
    pub fn parse(entry: &str) -> Result<Self, String> {
        let parts: Vec<&str> = entry.split(':').collect();
        let (host, container, permissions) = match parts.as_slice() {
            [host] => (*host, *host, "rwm"),
            [host, second] if is_permissions(second) && !second.starts_with('/') => {
                (*host, *host, *second)
            }
            [host, container] => (*host, *container, "rwm"),
            [host, container, perms] => (*host, *container, *perms),
            _ => return Err(format!("\"{entry}\" is not HOST[:CONTAINER[:PERMISSIONS]]")),
        };
        let components_ok = |p: &str| !p.split('/').any(|c| c == ".." || c == ".");
        if !host.starts_with("/dev/") || host.len() <= "/dev/".len() || !components_ok(host) {
            return Err(format!("host device \"{host}\" must be a path under /dev/"));
        }
        if !container.starts_with('/') || !components_ok(container) {
            return Err(format!("container path \"{container}\" must be absolute"));
        }
        if !is_permissions(permissions) {
            return Err(format!(
                "permissions \"{permissions}\" must be some of r, w and m"
            ));
        }
        Ok(DeviceMapping {
            host: host.trim_end_matches('/').to_string(),
            container: container.to_string(),
            permissions: permissions.to_string(),
        })
    }
}

fn is_permissions(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 3
        && s.chars().all(|c| matches!(c, 'r' | 'w' | 'm'))
        && s.chars().enumerate().all(|(i, c)| !s[..i].contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(yaml: &str) -> DeviceRequest {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn count_accepts_a_number_all_and_minus_one() {
        assert_eq!(
            request("capabilities: [gpu]\ncount: 2").count,
            Some(DeviceCount::Exactly(2))
        );
        assert_eq!(
            request("capabilities: [gpu]\ncount: all").count,
            Some(DeviceCount::All)
        );
        assert_eq!(
            request("capabilities: [gpu]\ncount: -1").count,
            Some(DeviceCount::All)
        );
        assert!(serde_yaml::from_str::<DeviceRequest>("count: 0").is_err());
        assert!(serde_yaml::from_str::<DeviceRequest>("count: some").is_err());
    }

    #[test]
    fn capabilities_take_the_flat_and_the_nested_form() {
        assert_eq!(
            request("capabilities: [gpu, utility]").capabilities,
            vec![vec!["gpu".to_string(), "utility".to_string()]]
        );
        assert_eq!(
            request("capabilities: [[gpu], [compute]]").capabilities,
            vec![vec!["gpu".to_string()], vec!["compute".to_string()]]
        );
        assert!(serde_yaml::from_str::<DeviceRequest>("capabilities: [gpu, [x]]").is_err());
    }

    #[test]
    fn an_unset_count_means_all_unless_ids_are_named() {
        assert_eq!(request("capabilities: [gpu]").docker_count(), -1);
        assert_eq!(request("capabilities: [gpu]").min_gpus(), 1);
        let ids = request("capabilities: [gpu]\ndevice_ids: ['0', 'GPU-abc']");
        assert_eq!(ids.docker_count(), 0);
        assert_eq!(ids.min_gpus(), 2);
    }

    #[test]
    fn validation_refuses_other_drivers_and_count_with_ids() {
        assert!(request("capabilities: [gpu]\ndriver: nvidia")
            .validate()
            .is_ok());
        assert!(request("capabilities: [gpu]\ndriver: amd")
            .validate()
            .is_err());
        assert!(request("capabilities: [gpu]\ncount: 1\ndevice_ids: ['0']")
            .validate()
            .is_err());
        assert!(request("driver: nvidia").validate().is_err());
    }

    #[test]
    fn a_device_entry_parses_with_defaults() {
        let d = DeviceMapping::parse("/dev/dri").unwrap();
        assert_eq!(
            (
                d.host.as_str(),
                d.container.as_str(),
                d.permissions.as_str()
            ),
            ("/dev/dri", "/dev/dri", "rwm")
        );
        let d = DeviceMapping::parse("/dev/dri/renderD128:/dev/dri/card0").unwrap();
        assert_eq!(d.container, "/dev/dri/card0");
        let d = DeviceMapping::parse("/dev/video0:/dev/video0:rw").unwrap();
        assert_eq!(d.permissions, "rw");
        let d = DeviceMapping::parse("/dev/video0:rw").unwrap();
        assert_eq!(
            (d.container.as_str(), d.permissions.as_str()),
            ("/dev/video0", "rw")
        );
    }

    #[test]
    fn a_device_entry_outside_dev_is_refused() {
        for bad in [
            "/etc/shadow",
            "/dev",
            "/dev/",
            "dev/dri",
            "/dev/../etc/passwd",
            "/dev/dri:relative",
            "/dev/dri:/dev/dri:x",
            "/dev/dri:/a:rw:z",
            "",
        ] {
            assert!(DeviceMapping::parse(bad).is_err(), "{bad:?}");
        }
    }
}
