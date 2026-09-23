//! Docker networks backing the container overlay.
//!
//! Clouduler does not let docker build the network. The bridge already exists,
//! made by `cl-net` with a VXLAN device enslaved to it so it spans every node.
//! What docker is asked for is a network that *attaches to that bridge*, with
//! the subnet and gateway the cluster's IPAM handed out, so a container docker
//! places on it lands on the overlay rather than on a node-local island.
//!
//! That is what `com.docker.network.bridge.name` does: point docker's bridge
//! driver at a device someone else owns.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use bollard::models::{Ipam, IpamConfig, NetworkConnectRequest, NetworkCreateRequest};
use bollard::query_parameters::InspectNetworkOptions;

use crate::client::Docker;
use crate::error::{classify, DockerError, Result};

/// Label marking a network as ours, so a sweep can tell it from one the
/// operator made by hand.
pub const LABEL_MANAGED: &str = "clouduler.managed";
pub const LABEL_STACK: &str = "clouduler.stack";
pub const MANAGED_VALUE: &str = "true";

/// What a stack network needs to exist as far as docker is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSpec {
    /// Docker network name, `<stack>_<network>`.
    pub name: String,
    /// Linux bridge to attach to, already created and carrying the VXLAN.
    pub bridge: String,
    pub subnet: String,
    pub gateway: Ipv4Addr,
    /// Underlay MTU minus VXLAN overhead. Getting this wrong shows up as large
    /// packets silently vanishing between nodes, so it is not optional.
    pub mtu: u16,
    pub stack: String,
}

impl Docker {
    /// Create the network if it is missing. Idempotent.
    ///
    /// An existing network is left exactly as it is rather than reconciled:
    /// docker cannot change a network's subnet in place, and deleting one with
    /// containers attached fails anyway. A network whose spec drifted is
    /// reported so the caller can decide, which in practice means the operator
    /// removes the stack and redeploys it.
    pub async fn ensure_network(&self, spec: &NetworkSpec) -> Result<bool> {
        if self.network_exists(&spec.name).await? {
            return Ok(false);
        }

        let mut options = HashMap::new();
        options.insert(
            "com.docker.network.bridge.name".to_string(),
            spec.bridge.clone(),
        );
        // Docker would otherwise add its own masquerade and inter-container
        // rules for a bridge it does not own.
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
                    ..Default::default()
                }]),
                options: None,
            }),
            ..Default::default()
        };

        match self.inner.create_network(request).await {
            Ok(_) => Ok(true),
            // Another node's daemon, or our own previous pass, got there first.
            Err(e) if already_exists(&e) => Ok(false),
            Err(e) => Err(classify(e)),
        }
    }

    /// Create a nat-kind cluster network if missing. Idempotent, like
    /// `ensure_network`, but unlike it this asks docker to build and own the
    /// bridge itself rather than pointing at one `cl-net` already made:
    /// there is no VXLAN device to enslave and no fixed egress interface to
    /// pin traffic to, so ordinary docker bridge NAT — masqueraded out
    /// whatever the host's own routing table picks for a given destination
    /// — is exactly the behavior wanted.
    ///
    /// `subnet`/`gateway` are still pinned rather than left to docker's own
    /// IPAM: a client on this network reaching a route on the same node
    /// never actually leaves it (the kernel delivers the forwarded packet
    /// straight to the local proxy listener instead of routing it back out
    /// through POSTROUTING), so the proxy sees the container's raw address
    /// on this subnet rather than a masqueraded one. `cl-daemon`'s proxy
    /// wiring trusts that subnet for exactly this reason, which only works
    /// if every node derives the same one deterministically instead of
    /// asking docker to pick.
    pub async fn ensure_nat_network(
        &self,
        name: &str,
        owner: &str,
        subnet: &str,
        gateway: Ipv4Addr,
    ) -> Result<bool> {
        if self.network_exists(name).await? {
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

    /// Attach an already-created container to a second network. Docker only
    /// takes one network at create time (`HostConfig.network_mode`); a
    /// service's egress network, if it has one, is always this second
    /// attachment (see `Docker::set_default_route` for how it still ends up
    /// carrying the container's default route despite that).
    ///
    /// `ipv4` pins the address on this network the same way create-time
    /// `EndpointIpamConfig` does; `None` lets docker pick one from the
    /// network's subnet, which is normal for an egress network (its address
    /// there is not something anything else in the cluster needs to know).
    ///
    /// Reconnecting a container already on the network is a 403 from docker,
    /// not useful information here (the reconcile loop calls this on every
    /// create, not just the first), so it reads as success like the rest of
    /// this file's idempotent `ensure_*` calls.
    pub async fn connect_network(
        &self,
        container: &str,
        network: &str,
        ipv4: Option<Ipv4Addr>,
    ) -> Result<()> {
        let request = NetworkConnectRequest {
            container: container.to_string(),
            endpoint_config: ipv4.map(|ip| bollard::models::EndpointSettings {
                ipam_config: Some(bollard::models::EndpointIpamConfig {
                    ipv4_address: Some(ip.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        match self.inner.connect_network(network, request).await {
            Ok(()) => Ok(()),
            Err(e) if already_connected(&e) => Ok(()),
            Err(e) => Err(classify(e)),
        }
    }

    /// Point a container's default route at `network`'s gateway.
    ///
    /// Docker only gives the *primary* (create-time) network the default
    /// route; a network attached afterward with `connect_network` never gets
    /// it, and there is no version-independent flag to ask for it either
    /// (`GwPriority` exists only on newer daemons). So this does it directly:
    /// find the container's PID and, in its own network namespace, replace
    /// the default route — the same operation `ip route replace default via
    /// <gw>` would do run inside the container, but from the host, needing
    /// only the daemon's own privileges rather than a capability grant on
    /// the workload.
    pub async fn set_default_route(&self, container: &str, network: &str) -> Result<()> {
        let gateway = self.network_gateway(network).await?;
        let pid = self.container_pid(container).await?;

        let output = tokio::process::Command::new("nsenter")
            .args([
                "-t",
                &pid.to_string(),
                "-n",
                "ip",
                "route",
                "replace",
                "default",
                "via",
                &gateway.to_string(),
            ])
            .output()
            .await
            .map_err(|e| DockerError::Other(format!("running nsenter for {container}: {e}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(DockerError::Other(format!(
                "nsenter route replace for {container} failed: {}",
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

    pub async fn network_exists(&self, name: &str) -> Result<bool> {
        match self
            .inner
            .inspect_network(name, None::<InspectNetworkOptions>)
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => match classify(e) {
                DockerError::NotFound(_) => Ok(false),
                other => Err(other),
            },
        }
    }

    /// Every overlay network this daemon still has, by name. Lets a caller
    /// tell an orphan (a network from a stack that no longer exists) apart
    /// from one that's still wanted, so it can be swept: left behind, an
    /// orphan keeps its subnet allocated in docker's own IPAM forever, and a
    /// later stack can be handed that same subnet by the cluster's own
    /// allocator and fail to create its network with a pool-overlap error.
    ///
    /// `exclude_stacks` are `LABEL_STACK` values owned by another reconciler
    /// (egress, wireguard) rather than a deployed stack, so this sweep never
    /// mistakes their networks for orphans of its own.
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

    /// Remove a network. A missing one is success, and one still in use is
    /// reported so a caller sweeping after a stack removal can wait rather than
    /// tear a running container's network out from under it.
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

/// Docker answers a duplicate create with a 409 and a message, not a typed
/// error, so the string is the only thing to go on.
fn already_exists(err: &bollard::errors::Error) -> bool {
    match err {
        bollard::errors::Error::DockerResponseServerError {
            status_code,
            message,
        } => *status_code == 409 || message.contains("already exists"),
        _ => false,
    }
}

/// Reconnecting an already-connected container is a 403, with the container
/// id in the message rather than a typed field.
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
        }
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
        // The bridge is the part that makes this an overlay network rather
        // than a node-local one; without it docker makes its own bridge and
        // containers on two nodes cannot see each other.
        assert!(!spec.bridge.is_empty());
        assert!(spec.mtu < 1500, "must leave room for the vxlan header");
    }
}
