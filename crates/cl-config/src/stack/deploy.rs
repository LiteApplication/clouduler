//! Compose `deploy:` block: replica count, placement, resource limits and the
//! rolling update/restart policy.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// `deploy.mode`: replicated runs `replicas` copies, global runs one per node.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeployMode {
    Replicated,
    Global,
}

/// `deploy.placement`: constraints on which nodes a service may run on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
}

/// A cpu/memory limit or reservation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ResourceSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
}

impl ResourceSpec {
    /// `memory` parsed into bytes, `None` if unset or unparseable.
    pub fn memory_bytes(&self) -> Option<u64> {
        parse_memory_bytes(self.memory.as_deref()?)
    }
}

/// Compose memory sizes: `512M`, `2g`, `1024`. Bytes when there is no suffix.
pub fn parse_memory_bytes(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let (digits, multiplier) = match raw.chars().last()? {
        'b' | 'B' => (&raw[..raw.len() - 1], 1u64),
        'k' | 'K' => (&raw[..raw.len() - 1], 1024),
        'm' | 'M' => (&raw[..raw.len() - 1], 1024 * 1024),
        'g' | 'G' => (&raw[..raw.len() - 1], 1024 * 1024 * 1024),
        _ => (raw, 1),
    };
    digits.trim().parse::<u64>().ok().map(|n| n * multiplier)
}

/// `deploy.resources`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ResourceSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservations: Option<ResourceSpec>,
}

/// `deploy.restart_policy`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct DeployRestartPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub delay: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub window: Option<Duration>,
}

/// `deploy.update_config`: rolling update behaviour.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<u32>,
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub delay: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_action: Option<String>,
}

/// `deploy:` block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Deploy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<DeployMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<DeployRestartPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_config: Option<UpdateConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_block() {
        let d: Deploy = serde_yaml::from_str(
            r#"
replicas: 3
mode: replicated
placement:
  constraints:
    - "node.hostname==n1"
resources:
  limits: { cpus: "1.0", memory: 512M }
  reservations: { memory: 128M }
restart_policy:
  condition: on-failure
  delay: 5s
  max_attempts: 3
update_config:
  parallelism: 1
  delay: 10s
  order: start-first
"#,
        )
        .unwrap();
        assert_eq!(d.replicas, Some(3));
        assert_eq!(d.mode, Some(DeployMode::Replicated));
        assert_eq!(
            d.placement.unwrap().constraints,
            vec!["node.hostname==n1".to_string()]
        );
        assert_eq!(
            d.resources.unwrap().limits.unwrap().cpus.as_deref(),
            Some("1.0")
        );
        assert_eq!(d.restart_policy.unwrap().max_attempts, Some(3));
        assert_eq!(d.update_config.unwrap().parallelism, Some(1));
    }

    #[test]
    fn unknown_key_rejected() {
        let err = serde_yaml::from_str::<Deploy>("replicas: 1\nbogus: 1\n").unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn byte_sizes_parse_with_and_without_suffixes() {
        assert_eq!(parse_memory_bytes("1024"), Some(1024));
        assert_eq!(parse_memory_bytes("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("512M"), Some(512 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("nonsense"), None);
    }
}
