//! Docker networks backing the container overlay.
//!
//! Docker does not build the network. `cl-net` already made the bridge with a
//! VXLAN device enslaved to it; docker is asked for a network that attaches to
//! it (`com.docker.network.bridge.name`) with the subnet and gateway from the
//! cluster IPAM, so containers land on the overlay, not a node-local island.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use bollard::models::{Ipam, IpamConfig, NetworkConnectRequest, NetworkCreateRequest};
use bollard::query_parameters::InspectNetworkOptions;

use crate::client::Docker;
use crate::error::{classify, DockerError, Result};

/// Label marking a network as ours, so sweeps skip hand-made ones.
pub const LABEL_MANAGED: &str = "clouduler.managed";
pub const LABEL_STACK: &str = "clouduler.stack";
pub const MANAGED_VALUE: &str = "true";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSpec {
    /// Docker network name, `<stack>_<network>`.
    pub name: String,
    /// Linux bridge to attach to, already created and carrying the VXLAN.
    pub bridge: String,
    pub subnet: String,
    pub gateway: Ipv4Addr,
    /// Underlay MTU minus VXLAN overhead. A wrong value silently drops large
    /// packets between nodes.
    pub mtu: u16,
    pub stack: String,
    /// Addresses docker's own IPAM must never hand out (a managed network's
    /// node addresses), reserved as auxiliary addresses.
    pub excluded: Vec<Ipv4Addr>,
}

impl Docker {
    /// Create the network if it is missing. Idempotent.
    ///
    /// An existing network is left as is: docker cannot change a subnet in place
    /// and deleting one with containers attached fails. Drift is reported so the
    /// operator can remove the stack and redeploy.
    pub async fn ensure_network(&self, spec: &NetworkSpec) -> Result<bool> {
        if self.network_exists(&spec.name).await? {
            return Ok(false);
        }

        let mut options = HashMap::new();
        options.insert(
            "com.docker.network.bridge.name".to_string(),
            spec.bridge.clone(),
        );
        // Docker would otherwise add masquerade and inter-container rules for a
        // bridge it does not own.
        options.insert(
            "com.docker.network.bridge.enable_ip_masquerade".to_string(),
            "false".to_string(),
        );
        options.insert(
            "com.docker.network.driver.mtu".to_string(),
            spec.mtu.to_string(),
        );

        let labels = HashMap::from([
            (LABEL_MANAGED.to_string(), MANAGED_VALUE.to_string()),
            (LABEL_STACK.to_string(), spec.stack.clone()),
        ]);

        let request = NetworkCreateRequest {
            name: spec.name.clone(),
            driver: Some("bridge".to_string()),
            options: Some(options),
            labels: Some(labels),
            ipam: Some(Ipam {
                driver: Some("default".to_string()),
                config: Some(vec![IpamConfig {
                    subnet: Some(spec.subnet.clone()),
                    gateway: Some(spec.gateway.to_string()),
                    auxiliary_addresses: (!spec.excluded.is_empty()).then(|| {
                        spec.excluded
                            .iter()
                            .enumerate()
                            .map(|(i, ip)| (format!("reserved{i}"), ip.to_string()))
                            .collect()
                    }),
                    ..Default::default()
                }]),
                options: None,
            }),
            ..Default::default()
        };

        match self.inner.create_network(request).await {
            Ok(_) => Ok(true),
            // Another node's daemon or our previous pass got there first.
            Err(e) if already_exists(&e) => Ok(false),
            Err(e) => Err(classify(e)),
        }
    }

    /// [`Docker::ensure_network`] for a managed private network, whose subnet and
    /// reserved addresses can change in the cluster config. Docker cannot alter
    /// them in place, so a network whose IPAM no longer matches the spec is
    /// removed and created anew, but only while no container is attached; with
    /// containers on it, it is left alone and the drift logged (the config PUT
    /// refuses a subnet change in that case, so this is a stale or hand edited
    /// network).
    pub async fn ensure_managed_network(&self, spec: &NetworkSpec) -> Result<bool> {
        if let Some(existing) = self.inspect_network_opt(&spec.name).await? {
            let drift = ipam_drift(&existing, spec);
            if drift.is_none() {
                forget_drift(&spec.name);
            }
            if let Some(drift) = drift {
                if network_in_use(&existing) {
                    // Reconciled every pass: say it once per distinct drift.
                    if first_report(&spec.name, &drift) {
                        tracing::warn!(
                            network = %spec.name,
                            expected_subnet = %spec.subnet,
                            %drift,
                            "the docker network of a managed network no longer matches its subnet or \
                             reserved addresses, but containers are attached; recreate it once they \
                             are stopped"
                        );
                    }
                    return Ok(false);
                }
                forget_drift(&spec.name);
                tracing::info!(
                    network = %spec.name,
                    subnet = %spec.subnet,
                    "recreating the docker network of a managed network whose subnet changed"
                );
                self.remove_network(&spec.name).await?;
            }
        }
        self.ensure_network(spec).await
    }

    /// Create a nat-kind cluster network if missing. Idempotent, but unlike
    /// `ensure_network` docker builds and owns the bridge: there is no VXLAN
    /// device or fixed egress interface, so plain bridge NAT is wanted.
    ///
    /// `subnet`/`gateway` are still pinned: a client reaching a route on the same
    /// node never leaves it (the kernel delivers the forwarded packet to the local
    /// proxy listener, skipping POSTROUTING), so the proxy sees the container's
    /// raw address. `cl-daemon`'s proxy wiring trusts that subnet, which only
    /// works if every node derives the same one deterministically.
    pub async fn ensure_nat_network(
        &self,
        name: &str,
        owner: &str,
        subnet: &str,
        gateway: Ipv4Addr,
    ) -> Result<bool> {
        if let Some(existing) = self.inspect_network_opt(name).await? {
            warn_on_nat_subnet_drift(name, subnet, &existing);
            return Ok(false);
        }

        let labels = HashMap::from([
            (LABEL_MANAGED.to_string(), MANAGED_VALUE.to_string()),
            (LABEL_STACK.to_string(), owner.to_string()),
        ]);

        let request = NetworkCreateRequest {
            name: name.to_string(),
            driver: Some("bridge".to_string()),
            labels: Some(labels),
            ipam: Some(Ipam {
                driver: Some("default".to_string()),
                config: Some(vec![IpamConfig {
                    subnet: Some(subnet.to_string()),
                    gateway: Some(gateway.to_string()),
                    ..Default::default()
                }]),
                options: None,
            }),
            ..Default::default()
        };

        match self.inner.create_network(request).await {
            Ok(_) => Ok(true),
            Err(e) if already_exists(&e) => Ok(false),
            Err(e) => Err(classify(e)),
        }
    }

    /// Attach an already-created container to a second network. Docker takes one
    /// network at create time (`HostConfig.network_mode`), so a service's egress
    /// network is always this second attachment (see `Docker::set_default_route`).
    ///
    /// `ipv4` pins the address like create-time `EndpointIpamConfig`; `None` lets
    /// docker pick, normal for an egress network.
    ///
    /// Reconnecting is a 403 from docker, which reads as success because the
    /// reconcile loop calls this on every create.
    pub async fn connect_network(
        &self,
        container: &str,
        network: &str,
        ipv4: Option<Ipv4Addr>,
    ) -> Result<()> {
        self.connect_network_named(container, network, ipv4, None)
            .await
    }

    /// `connect_network`, optionally naming the interface
    /// (`com.docker.network.endpoint.ifname`, docker 28+).
    pub async fn connect_network_named(
        &self,
        container: &str,
        network: &str,
        ipv4: Option<Ipv4Addr>,
        ifname: Option<&str>,
    ) -> Result<()> {
        let endpoint_config = if ipv4.is_none() && ifname.is_none() {
            None
        } else {
            Some(bollard::models::EndpointSettings {
                ipam_config: ipv4.map(|ip| bollard::models::EndpointIpamConfig {
                    ipv4_address: Some(ip.to_string()),
                    ..Default::default()
                }),
                driver_opts: ifname.map(|n| {
                    HashMap::from([(
                        "com.docker.network.endpoint.ifname".to_string(),
                        n.to_string(),
                    )])
                }),
                ..Default::default()
            })
        };
        let request = NetworkConnectRequest {
            container: container.to_string(),
            endpoint_config,
        };
        match self.inner.connect_network(network, request).await {
            Ok(()) => Ok(()),
            Err(e) if already_connected(&e) => Ok(()),
            Err(e) => Err(classify(e)),
        }
    }

    /// Point a container's default route at `network`'s gateway.
    ///
    /// Docker gives only the primary (create-time) network the default route, and
    /// `GwPriority` exists only on newer daemons. So this finds the container's PID
    /// and replaces the default route in its network namespace from the host,
    /// needing only the daemon's own privileges, not a capability on the workload.
    pub async fn set_default_route(&self, container: &str, network: &str) -> Result<()> {
        let gateway = self.network_gateway(network).await?;
        self.ip_route_in_netns(
            container,
            &["replace", "default", "via", &gateway.to_string()],
        )
        .await
    }

    /// Keep traffic to `destinations` on the primary (overlay) attachment, via
    /// `gateway`, instead of the default route.
    ///
    /// After `set_default_route` moved the default to an egress network, its
    /// kill-switch drops anything not leaving through its interface, including the
    /// node addresses the reverse proxy answers on. A more specific `/32` route
    /// wins, keeping those reachable.
    pub async fn pin_routes(
        &self,
        container: &str,
        gateway: Ipv4Addr,
        destinations: &[Ipv4Addr],
    ) -> Result<()> {
        for destination in destinations {
            self.ip_route_in_netns(
                container,
                &[
                    "replace",
                    &format!("{destination}/32"),
                    "via",
                    &gateway.to_string(),
                ],
            )
            .await?;
        }
        Ok(())
    }

    /// Run each `ip route` argument list inside the container's network namespace.
    /// Stops at the first failure.
    pub async fn apply_netns_routes(&self, container: &str, routes: &[Vec<String>]) -> Result<()> {
        for route in routes {
            let args: Vec<&str> = route
                .iter()
                .map(String::as_str)
                .skip_while(|a| *a == "route")
                .collect();
            self.ip_route_in_netns(container, &args).await?;
        }
        Ok(())
    }

    /// `ip route <args>` run inside the container's network namespace from the host.
    async fn ip_route_in_netns(&self, container: &str, args: &[&str]) -> Result<()> {
        let pid = self.container_pid(container).await?;

        let output = tokio::process::Command::new("nsenter")
            .args(["-t", &pid.to_string(), "-n", "ip", "route"])
            .args(args)
            .output()
            .await
            .map_err(|e| DockerError::Other(format!("running nsenter for {container}: {e}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(DockerError::Other(format!(
                "nsenter route {} for {container} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            )))
        }
    }

    async fn network_gateway(&self, network: &str) -> Result<Ipv4Addr> {
        let details = self
            .inner
            .inspect_network(network, None::<InspectNetworkOptions>)
            .await
            .map_err(classify)?;
        details
            .ipam
            .and_then(|ipam| ipam.config)
            .and_then(|configs| configs.into_iter().find_map(|c| c.gateway))
            .and_then(|gw| gw.parse().ok())
            .ok_or_else(|| DockerError::Other(format!("network {network} has no ipv4 gateway")))
    }

    async fn container_pid(&self, container: &str) -> Result<i64> {
        let details = self
            .inner
            .inspect_container(container, None)
            .await
            .map_err(classify)?;
        details
            .state
            .and_then(|s| s.pid)
            .filter(|&pid| pid > 0)
            .ok_or_else(|| {
                DockerError::Other(format!("container {container} has no pid, is it running?"))
            })
    }

    /// The containers attached to `name` as `(container id, address)`. Empty when
    /// the network does not exist. Tells stacks sharing an egress bridge apart.
    pub async fn network_members(&self, name: &str) -> Result<Vec<(String, Ipv4Addr)>> {
        let Some(network) = self.inspect_network_opt(name).await? else {
            return Ok(Vec::new());
        };
        Ok(network
            .containers
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(id, member)| {
                let addr = member.ipv4_address?;
                let addr = addr.split('/').next()?.parse().ok()?;
                Some((id, addr))
            })
            .collect())
    }

    pub async fn network_exists(&self, name: &str) -> Result<bool> {
        Ok(self.inspect_network_opt(name).await?.is_some())
    }

    /// `inspect_network`, but `Ok(None)` when it does not exist.
    async fn inspect_network_opt(
        &self,
        name: &str,
    ) -> Result<Option<bollard::models::NetworkInspect>> {
        match self
            .inner
            .inspect_network(name, None::<InspectNetworkOptions>)
            .await
        {
            Ok(network) => Ok(Some(network)),
            Err(e) => match classify(e) {
                DockerError::NotFound(_) => Ok(None),
                other => Err(other),
            },
        }
    }

    /// Every overlay network this daemon still has, by name, so orphans (networks
    /// of removed stacks) can be swept: left behind they keep their subnet
    /// allocated in docker's IPAM, and a later stack handed the same subnet fails
    /// with a pool-overlap error.
    ///
    /// `exclude_stacks` are `LABEL_STACK` values owned by another reconciler
    /// (egress, wireguard), whose networks must not be mistaken for orphans.
    pub async fn list_managed_networks(&self, exclude_stacks: &[&str]) -> Result<Vec<String>> {
        let mut filters = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![format!("{LABEL_MANAGED}={MANAGED_VALUE}")],
        );
        let options = bollard::query_parameters::ListNetworksOptionsBuilder::default()
            .filters(&filters)
            .build();

        let networks = self
            .inner
            .list_networks(Some(options))
            .await
            .map_err(classify)?;
        Ok(networks
            .into_iter()
            .filter(|n| {
                n.labels
                    .as_ref()
                    .and_then(|labels| labels.get(LABEL_STACK))
                    .is_none_or(|stack| !exclude_stacks.contains(&stack.as_str()))
            })
            .filter_map(|n| n.name)
            .collect())
    }

    /// Names of the networks labelled as belonging to `stack` (`LABEL_STACK`), for
    /// reconcilers with their own label (egress and nat use `_cluster`) to find
    /// their leftovers.
    pub async fn list_networks_of_stack(&self, stack: &str) -> Result<Vec<String>> {
        let mut filters = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![
                format!("{LABEL_MANAGED}={MANAGED_VALUE}"),
                format!("{LABEL_STACK}={stack}"),
            ],
        );
        let options = bollard::query_parameters::ListNetworksOptionsBuilder::default()
            .filters(&filters)
            .build();
        let networks = self
            .inner
            .list_networks(Some(options))
            .await
            .map_err(classify)?;
        Ok(networks.into_iter().filter_map(|n| n.name).collect())
    }

    /// Remove a network. A missing one is success; one still in use is reported so
    /// a sweeper can wait rather than pull it from under a running container.
    pub async fn remove_network(&self, name: &str) -> Result<()> {
        match self.inner.remove_network(name).await {
            Ok(()) => Ok(()),
            Err(e) => match classify(e) {
                DockerError::NotFound(_) => Ok(()),
                other => Err(other),
            },
        }
    }
}

/// Logs loudly when an existing nat network lacks the subnet clouduler would now
/// pin it to.
///
/// Docker cannot change a subnet in place, so this cannot self heal: the network
/// predates the pinning (see `ensure_nat_network`) and every route gated on the
/// same-node trust will 403 a same-node peer until an operator removes the stack,
/// deletes the docker network by hand, and lets it be recreated.
fn warn_on_nat_subnet_drift(
    name: &str,
    expected: &str,
    existing: &bollard::models::NetworkInspect,
) {
    let actual = existing
        .ipam
        .as_ref()
        .and_then(|ipam| ipam.config.as_ref())
        .and_then(|configs| configs.first())
        .and_then(|c| c.subnet.as_deref());
    if actual != Some(expected) {
        tracing::warn!(
            network = name,
            expected_subnet = expected,
            actual_subnet = actual.unwrap_or("unknown"),
            "nat network's live subnet does not match the one clouduler now pins nat networks \
             to; same-node peers on it will be rejected by gated proxy routes until the docker \
             network is removed and recreated"
        );
    }
}

/// Whether the live IPAM (subnet, gateway, auxiliary addresses) differs from
/// what `spec` would create.
#[cfg(test)]
fn ipam_drifted(existing: &bollard::models::NetworkInspect, spec: &NetworkSpec) -> bool {
    ipam_drift(existing, spec).is_some()
}

/// Drifts already logged, per docker network name, so a persistent one is
/// reported once instead of on every reconcile pass.
static REPORTED_DRIFT: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// True the first time `drift` is seen for `network` (or when it changed).
fn first_report(network: &str, drift: &str) -> bool {
    let mut seen = REPORTED_DRIFT.lock().unwrap_or_else(|e| e.into_inner());
    match seen.iter_mut().find(|(n, _)| n == network) {
        Some((_, d)) if d == drift => false,
        Some((_, d)) => {
            *d = drift.to_string();
            true
        }
        None => {
            seen.push((network.to_string(), drift.to_string()));
            true
        }
    }
}

fn forget_drift(network: &str) {
    let mut seen = REPORTED_DRIFT.lock().unwrap_or_else(|e| e.into_inner());
    seen.retain(|(n, _)| n != network);
}

/// What differs between the live IPAM and `spec`, or `None` when they match.
fn ipam_drift(existing: &bollard::models::NetworkInspect, spec: &NetworkSpec) -> Option<String> {
    let config = existing
        .ipam
        .as_ref()
        .and_then(|ipam| ipam.config.as_ref())
        .and_then(|configs| configs.first());
    let Some(config) = config else {
        return Some("no ipam config".to_string());
    };
    let reserved: std::collections::BTreeSet<String> = config
        .auxiliary_addresses
        .iter()
        .flat_map(|aux| aux.values().cloned())
        .collect();
    let wanted: std::collections::BTreeSet<String> =
        spec.excluded.iter().map(|ip| ip.to_string()).collect();
    let mut diffs = Vec::new();
    if config.subnet.as_deref() != Some(spec.subnet.as_str()) {
        diffs.push(format!("subnet {:?} != {}", config.subnet, spec.subnet));
    }
    let gateway = spec.gateway.to_string();
    if config.gateway.as_deref() != Some(gateway.as_str()) {
        diffs.push(format!("gateway {:?} != {gateway}", config.gateway));
    }
    if reserved != wanted {
        diffs.push(format!("reserved {reserved:?} != {wanted:?}"));
    }
    (!diffs.is_empty()).then(|| diffs.join("; "))
}

fn network_in_use(existing: &bollard::models::NetworkInspect) -> bool {
    existing.containers.as_ref().is_some_and(|c| !c.is_empty())
}

/// Docker answers a duplicate create with a 409 and an untyped message.
fn already_exists(err: &bollard::errors::Error) -> bool {
    match err {
        bollard::errors::Error::DockerResponseServerError {
            status_code,
            message,
        } => *status_code == 409 || message.contains("already exists"),
        _ => false,
    }
}

/// Reconnecting a connected container is a 403 with the container id only in the message.
fn already_connected(err: &bollard::errors::Error) -> bool {
    match err {
        bollard::errors::Error::DockerResponseServerError {
            status_code,
            message,
        } => *status_code == 403 || message.contains("already exists in network"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> NetworkSpec {
        NetworkSpec {
            name: "blog_default".into(),
            bridge: "cl-br-default".into(),
            subnet: "10.244.1.0/24".into(),
            gateway: Ipv4Addr::new(10, 244, 1, 1),
            mtu: 1450,
            stack: "blog".into(),
            excluded: Vec::new(),
        }
    }

    fn inspected(
        subnet: &str,
        gateway: &str,
        aux: &[&str],
        attached: usize,
    ) -> bollard::models::NetworkInspect {
        bollard::models::NetworkInspect {
            ipam: Some(Ipam {
                config: Some(vec![IpamConfig {
                    subnet: Some(subnet.into()),
                    gateway: Some(gateway.into()),
                    auxiliary_addresses: Some(
                        aux.iter()
                            .enumerate()
                            .map(|(i, a)| (format!("reserved{i}"), a.to_string()))
                            .collect(),
                    ),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            containers: Some(
                (0..attached)
                    .map(|i| (format!("c{i}"), Default::default()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn a_managed_network_drifts_when_its_subnet_gateway_or_reserved_addresses_change() {
        let mut s = spec();
        s.excluded = vec![Ipv4Addr::new(10, 244, 1, 2)];
        assert!(!ipam_drifted(
            &inspected("10.244.1.0/24", "10.244.1.1", &["10.244.1.2"], 0),
            &s
        ));
        assert!(ipam_drifted(
            &inspected("10.244.2.0/24", "10.244.1.1", &["10.244.1.2"], 0),
            &s
        ));
        assert!(ipam_drifted(
            &inspected("10.244.1.0/24", "10.244.1.9", &["10.244.1.2"], 0),
            &s
        ));
        assert!(ipam_drifted(
            &inspected("10.244.1.0/24", "10.244.1.1", &["10.244.1.3"], 0),
            &s
        ));
        assert!(ipam_drifted(
            &inspected("10.244.1.0/24", "10.244.1.1", &[], 0),
            &s
        ));
        assert!(ipam_drifted(
            &bollard::models::NetworkInspect::default(),
            &s
        ));
    }

    #[test]
    fn a_persistent_drift_is_reported_once_until_it_changes() {
        let n = "MNG_test-dedupe";
        forget_drift(n);
        assert!(first_report(n, "a"));
        assert!(!first_report(n, "a"));
        assert!(first_report(n, "b"));
        forget_drift(n);
        assert!(first_report(n, "b"));
        forget_drift(n);
    }

    #[test]
    fn the_drift_names_what_differs() {
        let mut s = spec();
        s.excluded = vec![Ipv4Addr::new(10, 244, 1, 2)];
        let d = ipam_drift(&inspected("10.244.1.0/24", "10.244.1.1", &[], 1), &s).unwrap();
        assert!(d.contains("reserved") && !d.contains("subnet"), "{d}");
    }

    #[test]
    fn a_network_with_containers_attached_is_in_use() {
        assert!(!network_in_use(&inspected(
            "10.0.0.0/24",
            "10.0.0.1",
            &[],
            0
        )));
        assert!(network_in_use(&inspected(
            "10.0.0.0/24",
            "10.0.0.1",
            &[],
            1
        )));
    }

    #[test]
    fn a_conflict_response_reads_as_already_existing() {
        let err = bollard::errors::Error::DockerResponseServerError {
            status_code: 409,
            message: "network with name blog_default already exists".into(),
        };
        assert!(already_exists(&err));
    }

    #[test]
    fn other_server_errors_do_not_read_as_already_existing() {
        let err = bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "boom".into(),
        };
        assert!(!already_exists(&err));
    }

    #[test]
    fn the_spec_carries_everything_needed_to_reach_the_overlay() {
        let spec = spec();
        // The bridge makes this an overlay network; without it docker makes its
        // own bridge and containers on two nodes cannot see each other.
        assert!(!spec.bridge.is_empty());
        assert!(spec.mtu < 1500, "must leave room for the vxlan header");
    }
}
