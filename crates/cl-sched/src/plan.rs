//! Expanding stack files into the concrete instances the cluster should run.

use std::collections::BTreeSet;

use cl_config::{DeployMode, Protocol, Service, StackFile, MAX_REPLICAS};
use cl_raft::ids::{InstanceId, VolumeId};
use cl_raft::state::PortProtocol;
use tracing::warn;

fn to_port_protocol(protocol: Protocol) -> PortProtocol {
    match protocol {
        Protocol::Tcp => PortProtocol::Tcp,
        Protocol::Udp => PortProtocol::Udp,
    }
}

/// GPUs and host devices a service asks for, matched against `NodeState::gpus`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuNeeds {
    /// `deploy.resources.reservations.devices`, one entry per request:
    /// the least GPUs it needs, and the ids it names.
    pub nvidia: Vec<NvidiaNeed>,
    /// Host paths from `devices:`.
    pub devices: BTreeSet<String>,
}

impl GpuNeeds {
    pub fn is_empty(&self) -> bool {
        self.nvidia.is_empty() && self.devices.is_empty()
    }

    fn of(service: &Service) -> Self {
        GpuNeeds {
            nvidia: service
                .device_requests()
                .iter()
                .map(|r| NvidiaNeed {
                    count: r.min_gpus(),
                    device_ids: r.device_ids.clone(),
                })
                .collect(),
            devices: service
                .device_mappings()
                .into_iter()
                .map(|d| d.host)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvidiaNeed {
    pub count: usize,
    pub device_ids: Vec<String>,
}

/// One instance the cluster should run, with everything placement needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub id: InstanceId,
    pub image: String,
    /// Drives whose volumes this instance mounts. A node must implement all of
    /// them to be a candidate.
    pub drives: BTreeSet<String>,
    /// Individual volumes mounted. For a replicated drive each node holds its own copy and only
    /// one is authoritative (`VolumeState::primary`), so placement needs volumes, not just drives.
    /// See `place::rejection`.
    pub volumes: BTreeSet<VolumeId>,
    /// Raw `deploy.placement.constraints` entries.
    pub constraints: Vec<String>,
    /// `x-clouduler.node`, a hard pin to one node.
    pub pinned_node: Option<String>,
    /// Global services run one copy on every eligible node.
    pub global: bool,
    /// Egress-kind network the service attaches to; a candidate must implement it
    /// (`NodeState::implements_egress_network`).
    pub egress_network: Option<String>,
    /// Every network attached; a candidate must implement each (`NodeState::implements_network`).
    pub networks: BTreeSet<String>,
    /// `(network, protocol, published port)` per `x-clouduler.expose` on a public network; a
    /// candidate must have the port open (`NodeState::opens_port`).
    pub bound_ports: BTreeSet<(String, PortProtocol, u16)>,
    /// `x-clouduler.colocate` group: instances sharing the key must share a node.
    pub colocate_key: Option<String>,
    /// `deploy.resources.limits.memory` in bytes. `None` makes `rebalance::plan` weigh this
    /// instance by count.
    pub memory_bytes: Option<u64>,
    pub gpu_needs: GpuNeeds,
    /// `(stack, service)` pairs this instance prefers to share a node with (a container
    /// network's router and its users). Soft: only orders eligible nodes.
    pub near: BTreeSet<(String, String)>,
}

impl Desired {
    /// Anchor keys (the `x-clouduler.colocate` group): instances sharing one must land on
    /// the same node. Services sharing a proxy hostname are not anchored: the proxy on any
    /// node relays to wherever each route's container runs.
    pub fn anchor_keys(&self) -> impl Iterator<Item = &str> {
        self.colocate_key.as_deref().into_iter()
    }
}

/// Expand a stack into its instances (`deploy.replicas`, default one; global services
/// are expanded later once the node set is known). `egress_networks` names every
/// `Egress` cluster network so it can be told apart from plain `networks:` entries.
pub fn expand(
    stack: &str,
    file: &StackFile,
    egress_networks: &BTreeSet<String>,
    port_gated_networks: &BTreeSet<String>,
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
            let asked = service
                .deploy
                .as_ref()
                .and_then(|d| d.replicas)
                .unwrap_or(1);
            // Validation rejects this, but a stack stored before that check would crash every leader pass.
            if asked > MAX_REPLICAS {
                warn!(%stack, service = %service_name, asked, max = MAX_REPLICAS, "replica count is above the maximum, clamping");
            }
            asked.min(MAX_REPLICAS)
        };

        for index in 0..replicas {
            out.push(Desired {
                id: InstanceId::new(stack, service_name, index),
                image: service.image.clone(),
                drives: service.drives_used(),
                volumes: service
                    .volumes
                    .iter()
                    .filter_map(|mount| {
                        Some(VolumeId::new(mount.drive.as_ref()?, mount.volume.as_ref()?))
                    })
                    .collect(),
                constraints: service
                    .deploy
                    .as_ref()
                    .and_then(|d| d.placement.as_ref())
                    .map(|p| p.constraints.clone())
                    .unwrap_or_default(),
                pinned_node: file.effective_node(service_name),
                global,
                egress_network: service
                    .networks
                    .iter()
                    .find(|n| egress_networks.contains(*n))
                    .cloned(),
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
                colocate_key: file.colocate_group(service_name),
                memory_bytes: service.memory_limit_bytes(),
                gpu_needs: GpuNeeds::of(service),
                near: BTreeSet::new(),
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
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new());
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
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new());
        assert_eq!(desired.len(), 3);
        assert_eq!(
            desired.iter().map(|d| d.id.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn an_absurd_replica_count_in_a_stored_stack_is_clamped() {
        let mut file = parse("services:\n  web:\n    image: nginx\n");
        // Bypasses validation, like a stack stored before the cap existed.
        file.services.get_mut("web").unwrap().deploy = Some(cl_config::Deploy {
            replicas: Some(4_000_000_000),
            ..Default::default()
        });
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new());
        assert_eq!(desired.len(), MAX_REPLICAS as usize);
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
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new());
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
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new());
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
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new());
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
        let desired = expand("site", &file, &BTreeSet::new(), &port_gated_networks);
        assert_eq!(
            desired[0].bound_ports,
            [("wan".to_string(), PortProtocol::Tcp, 25)].into()
        );

        // Not in `port_gated_networks` (e.g. a private network) means no port
        // constraint, since only a public router forwards anything in.
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new());
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
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new());
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
    fn services_sharing_a_proxy_host_are_not_anchored() {
        let file = proxy_stack();
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new());
        assert_eq!(desired[0].anchor_keys().count(), 0);
    }

    #[test]
    fn colocated_instances_share_a_key_regardless_of_dns_mode() {
        let file = parse(
            r#"
services:
  a:
    image: x
    x-clouduler:
      colocate: [b]
  b:
    image: x
"#,
        );
        let desired = expand("site", &file, &BTreeSet::new(), &BTreeSet::new());
        let a = desired.iter().find(|d| d.id.service == "a").unwrap();
        let b = desired.iter().find(|d| d.id.service == "b").unwrap();
        assert!(a.colocate_key.is_some());
        assert_eq!(a.colocate_key, b.colocate_key);
    }
}
