//! One-off containers that stand in for a service's container while a
//! standalone action runs.

use std::collections::{BTreeSet, HashMap};

use bollard::models::{
    ContainerCreateBody, EndpointIpamConfig, EndpointSettings, HealthConfig, NetworkingConfig,
    RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{CreateContainerOptionsBuilder, ListContainersOptionsBuilder};

use crate::client::Docker;
use crate::container::{RunState, LABEL_INSTANCE};
use crate::error::{classify, DockerError, Result};

/// Label on an action container, carrying the instance it stands in for. The
/// container deliberately has no `LABEL_MANAGED`: the reconciler must not
/// see it as a second copy of the instance.
pub const LABEL_ACTION_HOLD: &str = "clouduler.action-hold";

/// How long after creation a hold still counts: a crashed daemon can leave a
/// created-but-never-started container that must not keep its service down.
const HOLD_MAX_AGE_SECS: i64 = 15 * 60;

impl Docker {
    /// Create (not start) a container named `name` that runs `cmd` as a copy of
    /// `main`: same image, environment, mounts, devices, limits, network and
    /// fixed address, but no published ports, restart policy or healthcheck, and
    /// the entrypoint cleared so `cmd` is the whole command. Only the primary
    /// network is carried over.
    pub async fn create_action_container(
        &self,
        main: &str,
        name: &str,
        cmd: Vec<String>,
        user: Option<String>,
    ) -> Result<String> {
        let resp = self.inner.inspect_container(main, None).await.map_err(classify)?;
        let config = resp.config.unwrap_or_default();
        let mut host = resp.host_config.unwrap_or_default();
        let instance = config
            .labels
            .as_ref()
            .and_then(|l| l.get(LABEL_INSTANCE))
            .cloned()
            .ok_or_else(|| DockerError::NotFound(format!("{main} has no instance label")))?;

        host.port_bindings = None;
        host.publish_all_ports = None;
        host.auto_remove = None;
        host.restart_policy = Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::NO),
            maximum_retry_count: None,
        });

        // Docker takes one network at create time. Keep the address and
        // aliases the service had so it is reachable the same way.
        let networking_config = host.network_mode.as_deref().and_then(|network| {
            let old = resp.network_settings.as_ref()?.networks.as_ref()?.get(network)?;
            let endpoint = EndpointSettings {
                aliases: old.aliases.clone(),
                ipam_config: old
                    .ipam_config
                    .as_ref()
                    .and_then(|c| c.ipv4_address.clone())
                    .map(|ip| EndpointIpamConfig {
                        ipv4_address: Some(ip),
                        ..Default::default()
                    }),
                ..Default::default()
            };
            Some(NetworkingConfig {
                endpoints_config: Some(HashMap::from([(network.to_string(), endpoint)])),
            })
        });

        let body = ContainerCreateBody {
            image: config.image,
            hostname: config.hostname,
            domainname: config.domainname,
            user: user.or(config.user).filter(|u| !u.is_empty()),
            env: config.env,
            working_dir: config.working_dir.filter(|d| !d.is_empty()),
            volumes: config.volumes,
            stop_signal: config.stop_signal,
            entrypoint: Some(vec![String::new()]),
            cmd: Some(cmd),
            healthcheck: Some(HealthConfig {
                test: Some(vec!["NONE".to_string()]),
                ..Default::default()
            }),
            labels: Some(HashMap::from([
                (LABEL_ACTION_HOLD.to_string(), instance),
                ("clouduler.action".to_string(), "true".to_string()),
            ])),
            host_config: Some(host),
            networking_config,
            ..Default::default()
        };
        let options = CreateContainerOptionsBuilder::new().name(name).build();
        let resp = self
            .inner
            .create_container(Some(options), body)
            .await
            .map_err(classify)?;
        Ok(resp.id)
    }

    /// Instances (as their `LABEL_INSTANCE` string) that an action container
    /// is currently standing in for. The reconciler leaves these alone.
    pub async fn action_holds(&self) -> Result<BTreeSet<String>> {
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("label".to_string(), vec![LABEL_ACTION_HOLD.to_string()]);
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        let summaries = self
            .inner
            .list_containers(Some(options))
            .await
            .map_err(classify)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        Ok(summaries
            .into_iter()
            .filter(|s| {
                let finished = matches!(
                    s.state.map(RunState::from),
                    Some(RunState::Exited | RunState::Dead)
                );
                !finished && now - s.created.unwrap_or(0) < HOLD_MAX_AGE_SECS
            })
            .filter_map(|s| s.labels?.remove(LABEL_ACTION_HOLD))
            .collect())
    }
}
