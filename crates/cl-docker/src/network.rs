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

use bollard::models::{Ipam, IpamConfig, NetworkCreateRequest};
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
