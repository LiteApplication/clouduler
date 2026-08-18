//! Container lifecycle: create, start, stop, remove, inspect, list.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use bollard::models::{
    ContainerCreateBody, ContainerState as BollardContainerState, ContainerStateStatusEnum,
    ContainerSummary, EndpointIpamConfig, EndpointSettings, HealthConfig as BollardHealthConfig,
    HealthStatusEnum, HostConfig, NetworkingConfig, PortBinding,
    RestartPolicy as BollardRestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, ListContainersOptionsBuilder, RemoveContainerOptionsBuilder,
    RestartContainerOptionsBuilder, StopContainerOptionsBuilder,
};

use crate::client::Docker;
use crate::error::{classify, DockerError, Result};

/// Label carrying the stack a container belongs to.
pub const LABEL_STACK: &str = "clouduler.stack";
/// Label carrying the service name within the stack.
pub const LABEL_SERVICE: &str = "clouduler.service";
/// Label carrying the instance identifier, for scaled services.
pub const LABEL_INSTANCE: &str = "clouduler.instance";
/// Label marking a container as owned by clouduler. Always set to `MANAGED_VALUE`.
pub const LABEL_MANAGED: &str = "clouduler.managed";
/// The value clouduler writes for `LABEL_MANAGED`. Presence and value both matter:
/// this is how clouduler tells its own containers apart from the user's.
pub const MANAGED_VALUE: &str = "true";

/// A bind mount from the host into the container.
#[derive(Debug, Clone)]
pub struct BindMount {
    pub host_path: String,
    pub container_path: String,
    pub read_only: bool,
}

/// TCP or UDP, for published ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortProtocol {
    Tcp,
    Udp,
}

impl PortProtocol {
    fn as_str(self) -> &'static str {
        match self {
            PortProtocol::Tcp => "tcp",
            PortProtocol::Udp => "udp",
        }
    }
}

/// A published port. Leave `host_port` unset to let docker pick an ephemeral port.
#[derive(Debug, Clone)]
pub struct PortMapping {
    pub container_port: u16,
    pub protocol: PortProtocol,
    pub host_port: Option<u16>,
    pub host_ip: Option<String>,
}

/// Restart policy for a container. Mirrors docker's own options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartPolicy {
    #[default]
    No,
    Always,
    UnlessStopped,
    OnFailure {
        max_retries: i64,
    },
}

/// CPU and memory limits. `None` means unlimited (docker's default).
#[derive(Debug, Clone, Default)]
pub struct ResourceLimits {
    pub memory_bytes: Option<i64>,
    pub nano_cpus: Option<i64>,
}

/// A container healthcheck, equivalent to docker's `HEALTHCHECK`.
#[derive(Debug, Clone)]
pub struct Healthcheck {
    pub test: Vec<String>,
    pub interval: Option<Duration>,
    pub timeout: Option<Duration>,
    pub retries: Option<i64>,
    pub start_period: Option<Duration>,
}

/// Everything needed to create a clouduler-managed container. The
/// stack/service/instance fields are used both as ownership labels and to
/// build the container name.
#[derive(Debug, Clone, Default)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub stack: String,
    pub service: String,
    pub instance: String,
    pub env: Vec<(String, String)>,
    pub command: Option<Vec<String>>,
    pub labels: HashMap<String, String>,
    pub mounts: Vec<BindMount>,
    pub ports: Vec<PortMapping>,
    pub network: Option<String>,
    /// Fixed address on `network`. Allocated cluster wide and replicated, so
    /// every node can name this container's address without asking the node
    /// running it.
    pub ip: Option<Ipv4Addr>,
    /// Resolvers written into the container's `/etc/resolv.conf`. The internal
    /// resolver goes first so `service.stack` resolves; it forwards anything
    /// outside the cluster zone upstream.
    pub dns: Vec<IpAddr>,
    /// Search domains, so a bare `service.stack` is tried under the cluster's
    /// internal zone before being sent upstream as written.
    pub dns_search: Vec<String>,
    pub restart_policy: RestartPolicy,
    pub resources: ResourceLimits,
    pub healthcheck: Option<Healthcheck>,
}

impl ContainerSpec {
    /// The full label set this container will carry: the caller's labels plus
    /// the clouduler ownership labels, which always win on conflict.
    fn all_labels(&self) -> HashMap<String, String> {
        let mut labels = self.labels.clone();
        labels.insert(LABEL_STACK.to_string(), self.stack.clone());
        labels.insert(LABEL_SERVICE.to_string(), self.service.clone());
        labels.insert(LABEL_INSTANCE.to_string(), self.instance.clone());
        labels.insert(LABEL_MANAGED.to_string(), MANAGED_VALUE.to_string());
        labels
    }

    fn bollard_restart_policy(&self) -> BollardRestartPolicy {
        let (name, max) = match self.restart_policy {
            RestartPolicy::No => (RestartPolicyNameEnum::NO, None),
            RestartPolicy::Always => (RestartPolicyNameEnum::ALWAYS, None),
            RestartPolicy::UnlessStopped => (RestartPolicyNameEnum::UNLESS_STOPPED, None),
            RestartPolicy::OnFailure { max_retries } => {
                (RestartPolicyNameEnum::ON_FAILURE, Some(max_retries))
            }
        };
        BollardRestartPolicy {
            name: Some(name),
            maximum_retry_count: max,
        }
    }

    fn bollard_healthcheck(&self) -> Option<BollardHealthConfig> {
        self.healthcheck.as_ref().map(|hc| BollardHealthConfig {
            test: Some(hc.test.clone()),
            interval: hc.interval.map(|d| d.as_nanos() as i64),
            timeout: hc.timeout.map(|d| d.as_nanos() as i64),
            retries: hc.retries,
            start_period: hc.start_period.map(|d| d.as_nanos() as i64),
            start_interval: None,
        })
    }

    fn host_config(&self) -> HostConfig {
        let binds = (!self.mounts.is_empty()).then(|| {
            self.mounts
                .iter()
                .map(|m| {
                    if m.read_only {
                        format!("{}:{}:ro", m.host_path, m.container_path)
                    } else {
                        format!("{}:{}", m.host_path, m.container_path)
                    }
                })
                .collect()
        });

        let port_bindings = (!self.ports.is_empty()).then(|| {
            self.ports
                .iter()
                .map(|p| {
                    let key = format!("{}/{}", p.container_port, p.protocol.as_str());
                    let binding = PortBinding {
                        host_ip: p.host_ip.clone(),
                        host_port: p.host_port.map(|hp| hp.to_string()),
                    };
                    (key, Some(vec![binding]))
                })
                .collect()
        });

        HostConfig {
            binds,
            port_bindings,
            dns: (!self.dns.is_empty()).then(|| self.dns.iter().map(|d| d.to_string()).collect()),
            dns_search: (!self.dns_search.is_empty()).then(|| self.dns_search.clone()),
            restart_policy: Some(self.bollard_restart_policy()),
            network_mode: self.network.clone(),
            memory: self.resources.memory_bytes,
            nano_cpus: self.resources.nano_cpus,
            ..Default::default()
        }
    }

    /// Endpoint configuration, only needed to pin the container's address.
    /// `network_mode` in the host config is what attaches it to the network;
    /// this adds the fixed address on top, and is left out entirely when none
    /// was allocated so docker picks one from the subnet as usual.
    fn networking_config(&self) -> Option<NetworkingConfig> {
        let (network, ip) = (self.network.as_ref()?, self.ip?);
        let endpoint = EndpointSettings {
            ipam_config: Some(EndpointIpamConfig {
                ipv4_address: Some(ip.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        Some(NetworkingConfig {
            endpoints_config: Some(HashMap::from([(network.clone(), endpoint)])),
        })
    }

    /// Convert to the body bollard expects for `POST /containers/create`.
    pub(crate) fn to_create_body(&self) -> ContainerCreateBody {
        let exposed_ports = (!self.ports.is_empty()).then(|| {
            self.ports
                .iter()
                .map(|p| format!("{}/{}", p.container_port, p.protocol.as_str()))
                .collect()
        });

        ContainerCreateBody {
            image: Some(self.image.clone()),
            cmd: self.command.clone(),
            env: (!self.env.is_empty())
                .then(|| self.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
            labels: Some(self.all_labels()),
            exposed_ports,
            healthcheck: self.bollard_healthcheck(),
            host_config: Some(self.host_config()),
            networking_config: self.networking_config(),
            ..Default::default()
        }
    }
}

/// Health status of a container, as reported by its healthcheck (if any).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    None,
    Starting,
    Healthy,
    Unhealthy,
}

impl From<HealthStatusEnum> for HealthState {
    fn from(value: HealthStatusEnum) -> Self {
        match value {
            HealthStatusEnum::STARTING => HealthState::Starting,
            HealthStatusEnum::HEALTHY => HealthState::Healthy,
            HealthStatusEnum::UNHEALTHY => HealthState::Unhealthy,
            HealthStatusEnum::NONE | HealthStatusEnum::EMPTY => HealthState::None,
        }
    }
}

/// A snapshot of a container's state, owned so callers don't have to hold onto
/// bollard's types.
#[derive(Debug, Clone, Default)]
pub struct ContainerState {
    pub id: String,
    /// The image reference the container was created from (e.g. `nginx:1.25`).
    pub image: String,
    /// The local image ID docker resolved `image` to, as a `sha256:...` string.
    /// This is not a registry content digest; use `image::resolve_digest` for that.
    pub image_digest: Option<String>,
    pub running: bool,
    pub health: Option<HealthState>,
    pub exit_code: Option<i64>,
    pub labels: HashMap<String, String>,
    pub started_at: Option<String>,
}

impl From<bollard::models::ContainerInspectResponse> for ContainerState {
    fn from(resp: bollard::models::ContainerInspectResponse) -> Self {
        let config = resp.config;
        let state: BollardContainerState = resp.state.unwrap_or_default();

        ContainerState {
            id: resp.id.unwrap_or_default(),
            image: config
                .as_ref()
                .and_then(|c| c.image.clone())
                .unwrap_or_default(),
            image_digest: resp.image,
            running: state.status == Some(ContainerStateStatusEnum::RUNNING),
            health: state.health.and_then(|h| h.status).map(HealthState::from),
            exit_code: state.exit_code,
            labels: config.and_then(|c| c.labels).unwrap_or_default(),
            started_at: state.started_at,
        }
    }
}

impl From<ContainerSummary> for ContainerState {
    // `list_containers` doesn't return started_at or a distinct image digest, so
    // those are left empty here. Use `inspect` when you need the full picture.
    fn from(summary: ContainerSummary) -> Self {
        ContainerState {
            id: summary.id.unwrap_or_default(),
            image: summary.image.unwrap_or_default(),
            image_digest: summary.image_id,
            running: summary.state == Some(bollard::models::ContainerSummaryStateEnum::RUNNING),
            health: summary.health.and_then(|h| h.status).map(|s| match s {
                bollard::models::ContainerSummaryHealthStatusEnum::STARTING => {
                    HealthState::Starting
                }
                bollard::models::ContainerSummaryHealthStatusEnum::HEALTHY => HealthState::Healthy,
                bollard::models::ContainerSummaryHealthStatusEnum::UNHEALTHY => {
                    HealthState::Unhealthy
                }
                _ => HealthState::None,
            }),
            exit_code: None,
            labels: summary.labels.unwrap_or_default(),
            started_at: None,
        }
    }
}

impl Docker {
    /// Create a container from `spec`. Does not start it.
    pub async fn create(&self, spec: &ContainerSpec) -> Result<String> {
        let body = spec.to_create_body();
        let options = CreateContainerOptionsBuilder::new()
            .name(&spec.name)
            .build();
        let resp = self
            .inner
            .create_container(Some(options), body)
            .await
            .map_err(classify)?;
        Ok(resp.id)
    }

    /// Start a previously created container.
    pub async fn start(&self, name: &str) -> Result<()> {
        self.inner
            .start_container(name, None)
            .await
            .map_err(classify)
    }

    /// Stop a running container, giving it up to `timeout` to exit gracefully
    /// before docker sends SIGKILL.
    pub async fn stop(&self, name: &str, timeout: Duration) -> Result<()> {
        let options = StopContainerOptionsBuilder::new()
            .t(timeout.as_secs() as i32)
            .build();
        self.inner
            .stop_container(name, Some(options))
            .await
            .map_err(classify)
    }

    /// Remove a container. `force` kills it first if it's still running.
    pub async fn remove(&self, name: &str, force: bool) -> Result<()> {
        let options = RemoveContainerOptionsBuilder::new().force(force).build();
        self.inner
            .remove_container(name, Some(options))
            .await
            .map_err(classify)
    }

    /// Restart a container, giving it up to `timeout` to stop gracefully first.
    pub async fn restart(&self, name: &str, timeout: Duration) -> Result<()> {
        let options = RestartContainerOptionsBuilder::new()
            .t(timeout.as_secs() as i32)
            .build();
        self.inner
            .restart_container(name, Some(options))
            .await
            .map_err(classify)
    }

    /// Inspect a container by name or id. `Ok(None)` means it doesn't exist,
    /// which is a normal condition for the reconcile loop, not an error.
    pub async fn inspect(&self, name: &str) -> Result<Option<ContainerState>> {
        match self.inner.inspect_container(name, None).await {
            Ok(resp) => Ok(Some(resp.into())),
            Err(e) => match classify(e) {
                DockerError::NotFound(_) => Ok(None),
                other => Err(other),
            },
        }
    }

    /// List every container carrying the clouduler ownership label, running or not.
    pub async fn list_managed(&self) -> Result<Vec<ContainerState>> {
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![format!("{LABEL_MANAGED}={MANAGED_VALUE}")],
        );
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        let summaries = self
            .inner
            .list_containers(Some(options))
            .await
            .map_err(classify)?;
        Ok(summaries.into_iter().map(ContainerState::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_spec() -> ContainerSpec {
        ContainerSpec {
            name: "web-1".to_string(),
            image: "nginx:1.25".to_string(),
            stack: "blog".to_string(),
            service: "web".to_string(),
            instance: "1".to_string(),
            ..Default::default()
        }
    }

    /// The address is decided cluster wide, so the create call has to carry
    /// it: letting docker pick would give every node a different answer for
    /// where a container is.
    #[test]
    fn an_allocated_address_is_pinned_on_the_network() {
        let mut spec = base_spec();
        spec.network = Some("blog_default".to_string());
        spec.ip = Some(Ipv4Addr::new(10, 244, 0, 5));

        let body = spec.to_create_body();
        let endpoints = body
            .networking_config
            .expect("networking config")
            .endpoints_config
            .expect("endpoints");
        let ipam = endpoints["blog_default"].ipam_config.clone().expect("ipam");
        assert_eq!(ipam.ipv4_address.as_deref(), Some("10.244.0.5"));
    }

    #[test]
    fn no_address_leaves_docker_to_pick_one() {
        let mut spec = base_spec();
        spec.network = Some("blog_default".to_string());
        assert!(spec.to_create_body().networking_config.is_none());
    }

    #[test]
    fn the_internal_resolver_is_written_into_the_container() {
        let mut spec = base_spec();
        spec.dns = vec![IpAddr::V4(Ipv4Addr::new(10, 244, 0, 1))];
        spec.dns_search = vec!["int.example.com".to_string()];

        let host = spec.to_create_body().host_config.expect("host config");
        assert_eq!(host.dns, Some(vec!["10.244.0.1".to_string()]));
        assert_eq!(host.dns_search, Some(vec!["int.example.com".to_string()]));
    }

    #[test]
    fn labels_include_ownership_and_user_labels() {
        let mut spec = base_spec();
        spec.labels.insert("team".to_string(), "infra".to_string());
        let body = spec.to_create_body();
        let labels = body.labels.expect("labels set");
        assert_eq!(labels.get(LABEL_STACK), Some(&"blog".to_string()));
        assert_eq!(labels.get(LABEL_SERVICE), Some(&"web".to_string()));
        assert_eq!(labels.get(LABEL_INSTANCE), Some(&"1".to_string()));
        assert_eq!(labels.get(LABEL_MANAGED), Some(&MANAGED_VALUE.to_string()));
        assert_eq!(labels.get("team"), Some(&"infra".to_string()));
    }

    #[test]
    fn user_labels_cannot_override_ownership_labels() {
        let mut spec = base_spec();
        spec.labels
            .insert(LABEL_MANAGED.to_string(), "false".to_string());
        let body = spec.to_create_body();
        let labels = body.labels.expect("labels set");
        assert_eq!(labels.get(LABEL_MANAGED), Some(&MANAGED_VALUE.to_string()));
    }

    #[test]
    fn env_vars_land_in_key_value_form() {
        let mut spec = base_spec();
        spec.env.push(("PORT".to_string(), "8080".to_string()));
        spec.env.push(("DEBUG".to_string(), "true".to_string()));
        let body = spec.to_create_body();
        let env = body.env.expect("env set");
        assert!(env.contains(&"PORT=8080".to_string()));
        assert!(env.contains(&"DEBUG=true".to_string()));
    }

    #[test]
    fn binds_render_host_and_container_paths() {
        let mut spec = base_spec();
        spec.mounts.push(BindMount {
            host_path: "/data/web".to_string(),
            container_path: "/var/www".to_string(),
            read_only: false,
        });
        spec.mounts.push(BindMount {
            host_path: "/etc/certs".to_string(),
            container_path: "/certs".to_string(),
            read_only: true,
        });
        let body = spec.to_create_body();
        let binds = body.host_config.unwrap().binds.expect("binds set");
        assert!(binds.contains(&"/data/web:/var/www".to_string()));
        assert!(binds.contains(&"/etc/certs:/certs:ro".to_string()));
    }

    #[test]
    fn port_bindings_map_container_to_host_port() {
        let mut spec = base_spec();
        spec.ports.push(PortMapping {
            container_port: 80,
            protocol: PortProtocol::Tcp,
            host_port: Some(8080),
            host_ip: Some("0.0.0.0".to_string()),
        });
        let body = spec.to_create_body();
        let host_config = body.host_config.unwrap();
        let bindings = host_config.port_bindings.expect("port bindings set");
        let entries = bindings
            .get("80/tcp")
            .expect("80/tcp present")
            .as_ref()
            .unwrap();
        assert_eq!(entries[0].host_port.as_deref(), Some("8080"));
        assert_eq!(entries[0].host_ip.as_deref(), Some("0.0.0.0"));

        let exposed = body.exposed_ports.expect("exposed ports set");
        assert!(exposed.contains(&"80/tcp".to_string()));
    }

    #[test]
    fn restart_policy_on_failure_carries_retry_count() {
        let mut spec = base_spec();
        spec.restart_policy = RestartPolicy::OnFailure { max_retries: 5 };
        let body = spec.to_create_body();
        let policy = body.host_config.unwrap().restart_policy.unwrap();
        assert_eq!(policy.name, Some(RestartPolicyNameEnum::ON_FAILURE));
        assert_eq!(policy.maximum_retry_count, Some(5));
    }

    #[test]
    fn resource_limits_land_in_host_config() {
        let mut spec = base_spec();
        spec.resources.memory_bytes = Some(256 * 1024 * 1024);
        spec.resources.nano_cpus = Some(500_000_000);
        let body = spec.to_create_body();
        let host_config = body.host_config.unwrap();
        assert_eq!(host_config.memory, Some(256 * 1024 * 1024));
        assert_eq!(host_config.nano_cpus, Some(500_000_000));
    }

    #[test]
    fn healthcheck_converts_durations_to_nanos() {
        let mut spec = base_spec();
        spec.healthcheck = Some(Healthcheck {
            test: vec![
                "CMD".to_string(),
                "curl".to_string(),
                "-f".to_string(),
                "http://localhost".to_string(),
            ],
            interval: Some(Duration::from_secs(5)),
            timeout: Some(Duration::from_secs(2)),
            retries: Some(3),
            start_period: Some(Duration::from_secs(10)),
        });
        let body = spec.to_create_body();
        let hc = body.healthcheck.expect("healthcheck set");
        assert_eq!(hc.interval, Some(5_000_000_000));
        assert_eq!(hc.timeout, Some(2_000_000_000));
        assert_eq!(hc.retries, Some(3));
        assert_eq!(hc.start_period, Some(10_000_000_000));
    }

    #[test]
    fn no_mounts_or_ports_leave_those_fields_unset() {
        let spec = base_spec();
        let body = spec.to_create_body();
        assert!(body.host_config.unwrap().binds.is_none());
        assert!(body.exposed_ports.is_none());
    }

    #[tokio::test]
    #[ignore = "needs a running docker daemon"]
    async fn create_start_stop_remove_roundtrip() {
        let docker = Docker::connect().expect("connect to docker socket");
        let mut spec = base_spec();
        spec.name = "cl-docker-test-roundtrip".to_string();
        spec.image = "alpine:latest".to_string();
        spec.command = Some(vec!["sleep".to_string(), "30".to_string()]);

        // clean up any leftover container from a previous failed run
        let _ = docker.remove(&spec.name, true).await;

        docker.create(&spec).await.expect("create");
        docker.start(&spec.name).await.expect("start");
        let state = docker
            .inspect(&spec.name)
            .await
            .expect("inspect")
            .expect("exists");
        assert!(state.running);
        docker
            .stop(&spec.name, Duration::from_secs(5))
            .await
            .expect("stop");
        docker.remove(&spec.name, true).await.expect("remove");
        assert!(docker.inspect(&spec.name).await.expect("inspect").is_none());
    }

    #[tokio::test]
    #[ignore = "needs a running docker daemon"]
    async fn inspect_missing_container_returns_none() {
        let docker = Docker::connect().expect("connect to docker socket");
        let result = docker
            .inspect("cl-docker-test-does-not-exist")
            .await
            .expect("inspect");
        assert!(result.is_none());
    }
}
