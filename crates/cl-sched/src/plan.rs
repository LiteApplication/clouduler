//! Expanding stack files into the concrete instances the cluster should run.

use std::collections::BTreeSet;

use cl_config::{DeployMode, Protocol, Service, StackFile};
use cl_raft::ids::InstanceId;
use cl_raft::state::PortProtocol;

fn to_port_protocol(protocol: Protocol) -> PortProtocol {
    match protocol {
        Protocol::Tcp => PortProtocol::Tcp,
        Protocol::Udp => PortProtocol::Udp,
    }
}

/// One instance the cluster should be running, with everything placement needs
/// to decide where it can go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub id: InstanceId,
    pub image: String,
    /// Drives whose volumes this instance mounts. A node must implement all of
    /// them to be a candidate.
    pub drives: BTreeSet<String>,
    /// Raw `deploy.placement.constraints` entries.
    pub constraints: Vec<String>,
    /// `x-clouduler.node`, a hard pin to one node.
    pub pinned_node: Option<String>,
    /// Global services run one copy on every eligible node.
    pub global: bool,
    /// The egress-kind network this instance's service attaches to, if any.
    /// A node must implement it (`NodeState::implements_egress_network`) to
    /// be a candidate, the same way `drives` works.
    pub egress_network: Option<String>,
    /// Every network this instance's service attaches to. A node must
    /// implement each of them (`NodeState::implements_network`) to be a
    /// candidate, the same way `drives` works.
    pub networks: BTreeSet<String>,
    /// `(network, protocol, published port)` for every `x-clouduler.expose`
    /// entry on a public-kind network. A node must have the port marked open
    /// on that network (`NodeState::opens_port`) to be a candidate.
    pub bound_ports: BTreeSet<(String, PortProtocol, u16)>,
    /// Hostnames this instance's `x-clouduler.proxy` routes publish, when the
    /// cluster's DNS routes at the DNS level instead of through the proxy's
    /// routing mesh. Populated only in that mode: another instance sharing
    /// one of these hostnames must land on the same node, since a hostname
    /// can only point at one place once DNS is the one deciding where a
    /// request goes. Empty (no constraint) under the swarm-style default.
    pub proxy_hosts: BTreeSet<String>,
}

/// Expand a stack into its instances. Replica counts come from
/// `deploy.replicas`, defaulting to one; global services are expanded later,
/// once the eligible node set is known.
///
/// `egress_networks` names every cluster network of kind `Egress`, so a
/// service's plain `networks: Vec<String>` list can be told apart without
/// this crate needing the full `ClusterConfig` (it only ever sees replicated
/// state and stack files otherwise).
pub fn expand(
    stack: &str,
    file: &StackFile,
    egress_networks: &BTreeSet<String>,
    port_gated_networks: &BTreeSet<String>,
    colocate_proxy_hosts: bool,
) -> Vec<Desired> {
    let mut out = Vec::new();

    for (service_name, service) in &file.services {
        let global = matches!(
            service.deploy.as_ref().and_then(|d| d.mode),
            Some(DeployMode::Global)
        );

        // A global service gets one placeholder here and is fanned out per node
        // during placement, since the node set is not known yet.
        let replicas = if global {
            1
        } else {
            service
                .deploy
                .as_ref()
                .and_then(|d| d.replicas)
                .unwrap_or(1)
        };

        let proxy_hosts: BTreeSet<String> = if colocate_proxy_hosts {
            service
                .x_clouduler
                .as_ref()
                .map(|x| x.proxy.iter().map(|r| r.host.clone()).collect())
                .unwrap_or_default()
        } else {
            BTreeSet::new()
        };

        for index in 0..replicas {
            out.push(Desired {
                id: InstanceId::new(stack, service_name, index),
                image: service.image.clone(),
                drives: service.drives_used(),
                constraints: service
                    .deploy
                    .as_ref()
                    .and_then(|d| d.placement.as_ref())
                    .map(|p| p.constraints.clone())
                    .unwrap_or_default(),
                pinned_node: file.effective_node(service_name),
                global,
                egress_network: service.networks.iter().find(|n| egress_networks.contains(*n)).cloned(),
                networks: service.networks.iter().cloned().collect(),
                bound_ports: service
                    .x_clouduler
                    .as_ref()
                    .map(|x| {
                        x.expose
                            .iter()
                            .filter(|e| port_gated_networks.contains(&e.network))
                            .map(|e| (e.network.clone(), to_port_protocol(e.protocol), e.published))
                            .collect()
                    })
                    .unwrap_or_default(),
                proxy_hosts: proxy_hosts.clone(),
            });
        }
    }

    out
}

/// The service backing an instance, for callers that need the full spec.
pub fn service_of<'a>(file: &'a StackFile, id: &InstanceId) -> Option<&'a Service> {
    file.services.get(&id.service)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_config::StackFile;

    fn parse(yaml: &str) -> StackFile {
        StackFile::parse(yaml).expect("valid stack")
    }

    #[test]
    fn a_service_defaults_to_one_replica() {
        let file = parse(
            r#"
services:
  web:
    image: nginx
"#,
        );
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new(), false);
        assert_eq!(desired.len(), 1);
        assert_eq!(desired[0].id, InstanceId::new("site", "web", 0));
        assert!(!desired[0].global);
    }

    #[test]
    fn replicas_expand_to_numbered_instances() {
        let file = parse(
            r#"
services:
  web:
    image: nginx
    deploy:
      replicas: 3
"#,
        );
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new(), false);
        assert_eq!(desired.len(), 3);
        assert_eq!(
            desired.iter().map(|d| d.id.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn global_services_are_marked_for_fan_out() {
        let file = parse(
            r#"
services:
  agent:
    image: agent
    deploy:
      mode: global
"#,
        );
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new(), false);
        assert_eq!(desired.len(), 1);
        assert!(desired[0].global);
    }

    #[test]
    fn drives_come_from_the_volume_mounts() {
        let file = parse(
            r#"
services:
  db:
    image: postgres
    volumes:
      - "fast/pgdata:/var/lib/postgresql/data"
      - "bulk/backups:/backups:ro"
"#,
        );
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new(), false);
        assert_eq!(
            desired[0].drives,
            ["bulk".to_string(), "fast".to_string()].into()
        );
    }

    #[test]
    fn networks_are_carried_through() {
        let file = parse(
            r#"
networks:
  wan: {}
services:
  a:
    image: x
    networks: [wan]
"#,
        );
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new(), false);
        assert_eq!(desired[0].networks, ["wan".to_string()].into());
    }

    #[test]
    fn expose_ports_on_a_public_network_become_bound_ports() {
        let file = parse(
            r#"
networks:
  wan: {}
services:
  a:
    image: x
    networks: [wan]
    x-clouduler:
      expose:
        - port: 25
          published: 25
          protocol: tcp
          network: wan
"#,
        );
        let port_gated_networks = ["wan".to_string()].into();
        let desired = expand("site", &file, &BTreeSet::new(), &port_gated_networks, false);
        assert_eq!(
            desired[0].bound_ports,
            [("wan".to_string(), PortProtocol::Tcp, 25)].into()
        );

        // Not in `port_gated_networks` (e.g. a private network) means no port
        // constraint, since only a public router forwards anything in.
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new(), false);
        assert!(desired[0].bound_ports.is_empty());
    }

    #[test]
    fn constraints_and_pins_are_carried_through() {
        let file = parse(
            r#"
services:
  db:
    image: postgres
    deploy:
      placement:
        constraints:
          - "node.role == manager"
    x-clouduler:
      node: n2
"#,
        );
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new(), false);
        assert_eq!(desired[0].constraints, vec!["node.role == manager"]);
        assert_eq!(desired[0].pinned_node.as_deref(), Some("n2"));
    }

    fn proxy_stack() -> StackFile {
        parse(
            r#"
networks:
  wan: {}
services:
  web:
    image: nginx
    networks: [wan]
    x-clouduler:
      proxy:
        - host: app.example.com
          network: wan
          port: 80
"#,
        )
    }

    #[test]
    fn proxy_hosts_are_populated_only_when_colocation_is_requested() {
        let file = proxy_stack();
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new(), false);
        assert!(desired[0].proxy_hosts.is_empty());

        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new(), true);
        assert_eq!(desired[0].proxy_hosts, ["app.example.com".to_string()].into());
    }
}
