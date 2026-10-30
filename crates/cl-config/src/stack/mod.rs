//! Stack files: the Docker Swarm / Compose v3 subset clouduler schedules from,
//! plus the `x-clouduler` extensions documented in `docs/stacks.md`.

mod allowed;
mod bytesize;
mod de;
mod deploy;
mod derived;
mod extensions;
mod healthcheck;
mod objects;
mod port;
mod requirements;
mod validate;
mod volume;

pub use allowed::{AllowedHost, AllowedPorts, AllowedTarget};
pub use bytesize::ByteSize;
pub use deploy::{
    Deploy, DeployMode, DeployRestartPolicy, Placement, ResourceSpec, Resources, UpdateConfig,
    MAX_REPLICAS,
};
pub use extensions::{
    CorsPolicy, DockerSocket, DockerSocketMode, ExposePort, ForwardAuth, Middleware, ProxyRoute,
    RedirectRegex, ServiceAction, ServiceExtensions, StackDefaults, VolumeSyncOverride,
};
pub use healthcheck::{Healthcheck, HealthcheckTest};
pub use objects::{ObjectRef, SECRET_DIR};
pub use port::{PortMapping, PortRange, Protocol, PublishMode};
pub use requirements::{BoundPort, StackRequirements};
pub use validate::validate_stack_name;
pub use volume::{TmpfsOptions, VolumeMount};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};

/// Default sync interval used when neither the stack, the service nor the
/// drive itself specifies one. Mirrors `cluster::Drive`'s default.
pub const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Root of a stack file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StackFile {
    /// Accepted and ignored: clouduler doesn't distinguish compose schema versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub services: BTreeMap<String, Service>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub networks: BTreeMap<String, Option<StackNetwork>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub volumes: BTreeMap<String, Option<StackVolume>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub configs: BTreeMap<String, Option<StackConfig>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, Option<StackSecret>>,
    #[serde(
        rename = "x-clouduler",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub x_clouduler: Option<StackDefaults>,
}

/// A network a stack's services attach to. Clouduler doesn't create custom
/// docker networks: the name must match a network declared in the cluster.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct StackNetwork {
    /// What the stack may reach through this network. Absent means no
    /// restriction; present (even empty) denies everything not listed. See
    /// `AllowedTarget` for the entry syntax.
    #[serde(
        default,
        deserialize_with = "allowed::list",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed: Option<Vec<AllowedTarget>>,
}

/// Placeholder for a declared but unused top-level named volume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct StackVolume {}

/// A file-based compose `config`. Not referenceable from services yet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct StackConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
}

/// A file-based compose `secret`. Not referenceable from services yet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct StackSecret {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
}

/// One service in the stack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub image: String,
    /// Override the container's hostname. Independent of the service name,
    /// which is what other services on the same stack use to reach it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Command>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Command>,
    #[serde(
        default,
        deserialize_with = "de::map_or_list",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub environment: BTreeMap<String, String>,
    #[serde(
        default,
        deserialize_with = "de::path_or_list",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub env_file: Vec<PathBuf>,
    #[serde(
        default,
        deserialize_with = "de::map_or_list",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<VolumeMount>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortMapping>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<Healthcheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<RestartPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_add: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_drop: Vec<String>,
    #[serde(
        default,
        deserialize_with = "de::map_or_list",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub sysctls: BTreeMap<String, String>,
    /// Makes the container's root filesystem read-only. A service that writes
    /// outside its declared volumes will fail rather than silently
    /// accumulating state nothing backs up.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_grace_period: Option<Duration>,
    /// Signal sent on a graceful stop, e.g. `SIGINT`. Defaults to the image's
    /// own `STOPSIGNAL` (usually `SIGTERM`) when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
    /// Cluster configs mounted into this service.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub configs: Vec<ObjectRef>,
    /// Cluster secrets mounted into this service, at /run/secrets by default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ObjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<Deploy>,
    #[serde(
        rename = "x-clouduler",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub x_clouduler: Option<ServiceExtensions>,
}

/// `command:`/`entrypoint:`: shell string or exec array.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Command {
    Shell(String),
    Exec(Vec<String>),
}

/// `restart:`. `on-failure` optionally carries a retry limit as `on-failure:N`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub enum RestartPolicy {
    No,
    Always,
    OnFailure { max_attempts: Option<u32> },
    UnlessStopped,
}

impl TryFrom<String> for RestartPolicy {
    type Error = String;

    fn try_from(s: String) -> std::result::Result<Self, Self::Error> {
        match s.split_once(':') {
            Some(("on-failure", n)) => {
                let max_attempts = n.parse().map_err(|_| format!("invalid retry count \"{n}\""))?;
                Ok(RestartPolicy::OnFailure { max_attempts: Some(max_attempts) })
            }
            _ => match s.as_str() {
                "no" => Ok(RestartPolicy::No),
                "always" => Ok(RestartPolicy::Always),
                "on-failure" => Ok(RestartPolicy::OnFailure { max_attempts: None }),
                "unless-stopped" => Ok(RestartPolicy::UnlessStopped),
                other => Err(format!(
                    "unknown restart policy \"{other}\", expected no, always, on-failure[:N] or unless-stopped"
                )),
            },
        }
    }
}

impl From<RestartPolicy> for String {
    fn from(p: RestartPolicy) -> Self {
        match p {
            RestartPolicy::No => "no".to_string(),
            RestartPolicy::Always => "always".to_string(),
            RestartPolicy::OnFailure { max_attempts: None } => "on-failure".to_string(),
            RestartPolicy::OnFailure {
                max_attempts: Some(n),
            } => format!("on-failure:{n}"),
            RestartPolicy::UnlessStopped => "unless-stopped".to_string(),
        }
    }
}

impl StackFile {
    /// Read and parse a stack file from disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&raw).map_err(|e| match e {
            ConfigError::Parse { source, .. } => ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            },
            other => other,
        })
    }

    /// Parse without any cluster cross-checks, so a stack file can be linted
    /// on its own. Use `validate` for the full check against a cluster.
    ///
    /// Successful parses are memoised by the exact text: the daemon's
    /// reconcile loops each re-parse every stored stack on every pass, which
    /// on a small board costs far more than cloning the result (and delayed
    /// the proxy's first routes after a restart). Failures are not cached.
    pub fn parse(raw: &str) -> Result<Self> {
        static CACHE: std::sync::Mutex<Option<std::collections::HashMap<String, StackFile>>> =
            std::sync::Mutex::new(None);
        const CAPACITY: usize = 256;

        if let Some(hit) = CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|cache| cache.get(raw))
        {
            return Ok(hit.clone());
        }

        let stack: StackFile = serde_yaml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<stack>"),
            source,
        })?;
        stack.validate_syntax()?;

        let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let cache = guard.get_or_insert_with(Default::default);
        if cache.len() >= CAPACITY {
            // Stale specs are never revisited, so wholesale clearing is
            // enough to keep this bounded.
            cache.clear();
        }
        cache.insert(raw.to_string(), stack.clone());
        Ok(stack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::ClusterConfig;

    const GITLAB_YAML: &str = include_str!("../../../../examples/stacks/gitlab.yaml");
    const SIMPLE_YAML: &str = include_str!("../../../../examples/stacks/simple.yaml");
    const CLUSTER_YAML: &str = include_str!("../../../../examples/cluster.yaml");

    fn cluster() -> ClusterConfig {
        ClusterConfig::parse(CLUSTER_YAML).expect("reference cluster is valid")
    }

    #[test]
    fn parses_simple_stack() {
        let stack = StackFile::parse(SIMPLE_YAML).expect("simple stack parses");
        assert_eq!(stack.services.len(), 2);
    }

    #[test]
    fn parses_gitlab_stack() {
        let stack = StackFile::parse(GITLAB_YAML).expect("gitlab stack parses");
        let gitlab = &stack.services["gitlab"];
        assert!(gitlab
            .volumes
            .iter()
            .any(|v| v.drive.as_deref() == Some("fast")));

        let ext = gitlab.x_clouduler.as_ref().expect("gitlab has x-clouduler");
        assert_eq!(ext.proxy.len(), 1);
        assert_eq!(ext.proxy[0].port, 80);
        assert_eq!(ext.expose.len(), 1);
        assert_eq!(ext.expose[0].port, 22);
    }

    #[test]
    fn gitlab_stack_validates_against_reference_cluster() {
        let stack = StackFile::parse(GITLAB_YAML).unwrap();
        stack
            .validate(&cluster())
            .expect("gitlab stack matches the reference cluster");
    }

    #[test]
    fn simple_stack_validates_against_reference_cluster() {
        let stack = StackFile::parse(SIMPLE_YAML).unwrap();
        stack
            .validate(&cluster())
            .expect("simple stack matches the reference cluster");
    }

    /// The qemu suite's publishing scenario is the only end to end cover for
    /// the proxy, published ports and the internal resolver. A stack file that
    /// no longer validates would fail an hour into a vm run, so it is checked
    /// here too.
    #[test]
    fn the_publishing_scenario_stack_is_valid() {
        const PUBLISH_YAML: &str =
            include_str!("../../../../tests/qemu/scenarios/stacks/publish.yaml");
        const QEMU_CLUSTER: &str = "\
cluster:
  name: qemu-test
networks:
  cluster:
    type: internal
    subnet: 10.10.0.0/24
    domain: cl.test
  wan:
    type: public
    domain: wan.test
nodes:
  n1:
    id: 3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f01
    role: manager
    network:
      cluster: 10.10.0.1
      wan: { ipv4: 203.0.113.1 }
web:
  address: api
  network: cluster
  port: 8443
";
        let cluster = ClusterConfig::parse(QEMU_CLUSTER).expect("qemu cluster shape is valid");
        let stack = StackFile::parse(PUBLISH_YAML).expect("publish stack parses");
        stack
            .validate(&cluster)
            .expect("publish stack matches the qemu cluster");
    }

    #[test]
    fn command_accepts_both_forms() {
        let raw = "services:\n  a:\n    image: x\n    command: [\"echo\", \"hi\"]\n";
        let stack = StackFile::parse(raw).unwrap();
        assert_eq!(
            stack.services["a"].command,
            Some(Command::Exec(vec!["echo".into(), "hi".into()]))
        );

        let raw = "services:\n  a:\n    image: x\n    command: echo hi\n";
        let stack = StackFile::parse(raw).unwrap();
        assert_eq!(
            stack.services["a"].command,
            Some(Command::Shell("echo hi".into()))
        );
    }

    #[test]
    fn environment_accepts_map_and_list() {
        let raw = "services:\n  a:\n    image: x\n    environment:\n      FOO: bar\n";
        let stack = StackFile::parse(raw).unwrap();
        assert_eq!(stack.services["a"].environment["FOO"], "bar");

        let raw = "services:\n  a:\n    image: x\n    environment:\n      - FOO=bar\n";
        let stack = StackFile::parse(raw).unwrap();
        assert_eq!(stack.services["a"].environment["FOO"], "bar");
    }

    #[test]
    fn labels_accepts_map_and_list() {
        let raw = "services:\n  a:\n    image: x\n    labels:\n      - team=infra\n";
        let stack = StackFile::parse(raw).unwrap();
        assert_eq!(stack.services["a"].labels["team"], "infra");
    }

    #[test]
    fn env_file_accepts_single_and_list() {
        // `parse` rejects env_file during validation, so check the shape
        // through the raw deserializer.
        let raw = "services:\n  a:\n    image: x\n    env_file: .env\n";
        let stack: StackFile = serde_yaml::from_str(raw).unwrap();
        assert_eq!(stack.services["a"].env_file, vec![PathBuf::from(".env")]);

        let raw = "services:\n  a:\n    image: x\n    env_file: [.env, .env.local]\n";
        let stack: StackFile = serde_yaml::from_str(raw).unwrap();
        assert_eq!(stack.services["a"].env_file.len(), 2);
    }

    #[test]
    fn restart_policy_with_retry_count() {
        let raw = "services:\n  a:\n    image: x\n    restart: \"on-failure:5\"\n";
        let stack = StackFile::parse(raw).unwrap();
        assert_eq!(
            stack.services["a"].restart,
            Some(RestartPolicy::OnFailure {
                max_attempts: Some(5)
            })
        );
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let raw = "services:\n  a:\n    image: x\nbogus: 1\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn unknown_service_key_is_rejected() {
        let raw = "services:\n  a:\n    image: x\n    build: .\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
        assert!(err.to_string().contains("build"));
    }

    #[test]
    fn stack_wide_defaults_inherit_into_services() {
        let raw = r#"
x-clouduler:
  auto_update: true
  node: n1
services:
  a:
    image: x
  b:
    image: y
    x-clouduler:
      auto_update: false
"#;
        let stack = StackFile::parse(raw).unwrap();
        assert!(stack.effective_auto_update("a"));
        assert!(!stack.effective_auto_update("b"));
        assert_eq!(stack.effective_node("a").as_deref(), Some("n1"));
    }

    #[test]
    fn sync_interval_resolution_precedence() {
        let raw = r#"
x-clouduler:
  volume_sync:
    data: { interval: 30m }
services:
  a:
    image: x
    volumes:
      - fast/data:/data
    x-clouduler:
      volume_sync:
        data: { interval: 10m }
  b:
    image: x
    volumes:
      - fast/data:/data
"#;
        let stack = StackFile::parse(raw).unwrap();
        let cluster = cluster();

        // service level override wins
        assert_eq!(
            stack.sync_interval("a", "data", &cluster),
            Duration::from_secs(600)
        );
        // falls back to the stack wide default
        assert_eq!(
            stack.sync_interval("b", "data", &cluster),
            Duration::from_secs(1800)
        );
        // falls back to the drive default when nothing else applies
        let raw_no_defaults =
            "services:\n  c:\n    image: x\n    volumes:\n      - fast/data:/data\n";
        let stack2 = StackFile::parse(raw_no_defaults).unwrap();
        assert_eq!(
            stack2.sync_interval("c", "data", &cluster),
            cluster.drives["fast"].sync_interval
        );
    }

    #[test]
    fn stop_signal_accepts_a_sig_name_and_rejects_junk() {
        let raw = "services:\n  a:\n    image: x\n    stop_signal: SIGINT\n";
        let stack = StackFile::parse(raw).unwrap();
        assert_eq!(stack.services["a"].stop_signal.as_deref(), Some("SIGINT"));

        let raw = "services:\n  a:\n    image: x\n    stop_signal: sigint\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("stop_signal"));
    }

    #[test]
    fn drives_used_collects_across_services() {
        let raw = r#"
services:
  a:
    image: x
    volumes:
      - fast/one:/one
  b:
    image: x
    volumes:
      - bulk/two:/two
      - /anon
"#;
        let stack = StackFile::parse(raw).unwrap();
        assert_eq!(
            stack.drives_used(),
            ["bulk".to_string(), "fast".to_string()]
                .into_iter()
                .collect()
        );
        assert_eq!(
            stack.services["a"].drives_used(),
            ["fast".to_string()].into_iter().collect()
        );
    }
}
