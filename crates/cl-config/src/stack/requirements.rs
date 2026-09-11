//! What a stack needs from the cluster to schedule: networks, drives, and
//! cluster configs/secrets its services reference. Used by the ui to explain
//! why a service cannot be placed, and to know which configs/secrets a stack
//! bundles its own file for (so redeploying can offer to replace them).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::port::Protocol;
use super::StackFile;
use crate::cluster::{qualify, ClusterConfig};

/// A raw `x-clouduler.expose` entry, carried through so the ui can check it
/// against a node's `open_ports` without re-deriving it from the stack yaml.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct BoundPort {
    pub network: String,
    pub protocol: Protocol,
    pub published: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StackRequirements {
    /// Networks any service attaches to.
    pub networks: BTreeSet<String>,
    /// Drives any service's volumes reference.
    pub drives: BTreeSet<String>,
    /// Cluster configs any service mounts, by name.
    pub configs: BTreeSet<String>,
    /// Cluster secrets any service mounts, by name.
    pub secrets: BTreeSet<String>,
    /// Top level configs this stack bundles its own file for, so it owns
    /// them: they came with the stack rather than a pre-existing object.
    pub file_configs: BTreeSet<String>,
    /// Top level secrets this stack bundles its own file for.
    pub file_secrets: BTreeSet<String>,
    /// Fully qualified hostnames this stack's services are proxied under.
    /// Empty until filled in from `StackFile::hosts`, which needs the
    /// cluster config to resolve a bare label's network domain - not
    /// available from the stack file alone.
    #[serde(default)]
    pub hosts: BTreeSet<String>,
    /// `x-clouduler.node`, this service's effective hard pin (its own, or
    /// the stack wide default), if any. Per-service only: a stack aggregate
    /// has no single meaningful pin when services disagree, so this is left
    /// `None` on `requirements()` and only filled in by
    /// `service_requirements()`.
    #[serde(default)]
    pub pinned_node: Option<String>,
    /// Raw `deploy.placement.constraints` entries (Swarm syntax, e.g.
    /// `node.hostname==n1` or `node.labels.fast==true`). Per-service only,
    /// same reasoning as `pinned_node`.
    #[serde(default)]
    pub placement_constraints: Vec<String>,
    /// `x-clouduler.expose` entries on a port-gated (public or wireguard)
    /// network: a node must have the port marked open to be a candidate.
    /// Per-service only, same reasoning as `pinned_node`.
    #[serde(default)]
    pub bound_ports: Vec<BoundPort>,
    /// Drives mounted whole (`drive/.`, see `cl_drive::registry::resolve_relative`)
    /// rather than as a named subvolume — a subset of `drives`. These need
    /// the stricter "pinned to the drive's one real holder" eligibility
    /// `validate_whole_drive_mounts` enforces, not the ordinary
    /// implements-or-served check every other drive gets. Per-service only,
    /// same reasoning as `pinned_node`.
    #[serde(default)]
    pub whole_drive_mounts: BTreeSet<String>,
}

impl StackFile {
    pub fn requirements(&self) -> StackRequirements {
        let mut r = StackRequirements::default();

        for svc in self.services.values() {
            r.networks.extend(svc.networks.iter().cloned());
            for vol in &svc.volumes {
                if let Some(drive) = &vol.drive {
                    r.drives.insert(drive.clone());
                }
            }
            r.configs
                .extend(svc.configs.iter().map(|c| c.source.clone()));
            r.secrets
                .extend(svc.secrets.iter().map(|s| s.source.clone()));
        }

        for (name, cfg) in &self.configs {
            if cfg.as_ref().and_then(|c| c.file.as_ref()).is_some() {
                r.file_configs.insert(name.clone());
            }
        }
        for (name, secret) in &self.secrets {
            if secret.as_ref().and_then(|s| s.file.as_ref()).is_some() {
                r.file_secrets.insert(name.clone());
            }
        }

        r
    }

    /// The same breakdown `requirements` computes, but one `StackRequirements`
    /// per service rather than unioned across the whole stack — what the ui
    /// needs to show a separate placement table per service instead of one
    /// table that can't tell you which constraint belongs to which container.
    /// `file_configs`/`file_secrets` are a stack-level concept (which object
    /// the stack bundles its own file for) and are left empty here; use
    /// `requirements()` for those.
    pub fn service_requirements(&self) -> BTreeMap<String, StackRequirements> {
        let mut out = BTreeMap::new();
        for (name, svc) in &self.services {
            let mut r = StackRequirements::default();
            r.networks.extend(svc.networks.iter().cloned());
            for vol in &svc.volumes {
                let Some(drive) = &vol.drive else { continue };
                r.drives.insert(drive.clone());
                if vol.volume.as_deref() == Some(".") {
                    r.whole_drive_mounts.insert(drive.clone());
                }
            }
            r.configs
                .extend(svc.configs.iter().map(|c| c.source.clone()));
            r.secrets
                .extend(svc.secrets.iter().map(|s| s.source.clone()));
            r.pinned_node = self.effective_node(name);
            if let Some(constraints) = svc.deploy.as_ref().and_then(|d| d.placement.as_ref()) {
                r.placement_constraints = constraints.constraints.clone();
            }
            if let Some(ext) = &svc.x_clouduler {
                r.bound_ports = ext
                    .expose
                    .iter()
                    .map(|e| BoundPort {
                        network: e.network.clone(),
                        protocol: e.protocol,
                        published: e.published,
                    })
                    .collect();
            }
            out.insert(name.clone(), r);
        }
        out
    }

    /// Every fully qualified hostname this stack's services are proxied
    /// under, resolved against `cfg`'s per-network domains the same way the
    /// proxy itself does. Used to show a stack's DNS/certificate status
    /// without the caller having to re-derive it.
    pub fn hosts(&self, cfg: &ClusterConfig) -> BTreeSet<String> {
        let mut hosts = BTreeSet::new();
        for svc in self.services.values() {
            let Some(extensions) = &svc.x_clouduler else {
                continue;
            };
            for route in &extensions.proxy {
                let domain = cfg
                    .networks
                    .get(&route.network)
                    .and_then(|n| n.domain.as_deref());
                hosts.insert(qualify(&route.host, domain));
            }
        }
        hosts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::ClusterConfig;

    const CLUSTER_YAML: &str = include_str!("../../../../examples/cluster.yaml");
    const GITLAB_YAML: &str = include_str!("../../../../examples/stacks/gitlab.yaml");

    #[test]
    fn hosts_qualifies_a_bare_label_with_its_networks_domain() {
        let cluster = ClusterConfig::parse(CLUSTER_YAML).expect("reference cluster is valid");
        let stack = StackFile::parse(GITLAB_YAML).expect("gitlab stack parses");
        assert_eq!(
            stack.hosts(&cluster),
            BTreeSet::from(["gitlab.example.com".to_string()])
        );
    }

    #[test]
    fn gathers_networks_drives_configs_and_secrets_across_services() {
        let spec = r#"
services:
  web:
    image: nginx
    networks: [internal]
    configs:
      - headscale_config
    secrets:
      - db_password
    volumes:
      - data/web:/data
  worker:
    image: worker
    networks: [internal, public]
networks:
  internal: {}
  public: {}
configs:
  headscale_config:
    file: ./config/config.yaml
  external_only: {}
secrets:
  db_password:
    file: ./db.secret
"#;
        let stack = StackFile::parse(spec).expect("parse");
        let r = stack.requirements();
        assert_eq!(
            r.networks,
            BTreeSet::from(["internal".into(), "public".into()])
        );
        assert_eq!(r.drives, BTreeSet::from(["data".into()]));
        assert_eq!(r.configs, BTreeSet::from(["headscale_config".into()]));
        assert_eq!(r.secrets, BTreeSet::from(["db_password".into()]));
        assert_eq!(r.file_configs, BTreeSet::from(["headscale_config".into()]));
        assert_eq!(r.file_secrets, BTreeSet::from(["db_password".into()]));
    }

    #[test]
    fn service_requirements_are_broken_out_per_service_rather_than_unioned() {
        let spec = r#"
services:
  web:
    image: nginx
    networks: [internal]
    secrets:
      - db_password
  worker:
    image: worker
    networks: [internal, public]
    volumes:
      - data/worker:/data
networks:
  internal: {}
  public: {}
secrets:
  db_password:
    file: ./db.secret
"#;
        let stack = StackFile::parse(spec).expect("parse");
        let per_service = stack.service_requirements();

        let web = &per_service["web"];
        assert_eq!(web.networks, BTreeSet::from(["internal".into()]));
        assert_eq!(web.secrets, BTreeSet::from(["db_password".into()]));
        assert!(web.drives.is_empty());

        let worker = &per_service["worker"];
        assert_eq!(
            worker.networks,
            BTreeSet::from(["internal".into(), "public".into()])
        );
        assert_eq!(worker.drives, BTreeSet::from(["data".into()]));
        assert!(worker.secrets.is_empty());
    }

    #[test]
    fn service_requirements_capture_pin_constraints_ports_and_whole_drive_mounts() {
        let spec = r#"
services:
  web:
    image: nginx
    x-clouduler:
      node: n1
      expose:
        - network: public
          protocol: tcp
          port: 8080
          published: 8080
    volumes:
      - fast/.:/data
    deploy:
      placement:
        constraints:
          - node.labels.fast==true
  worker:
    image: worker
    volumes:
      - fast/logs:/logs
networks: {}
"#;
        let stack = StackFile::parse(spec).expect("parse");
        let per_service = stack.service_requirements();

        let web = &per_service["web"];
        assert_eq!(web.pinned_node, Some("n1".to_string()));
        assert_eq!(
            web.placement_constraints,
            vec!["node.labels.fast==true".to_string()]
        );
        assert_eq!(
            web.bound_ports,
            vec![BoundPort {
                network: "public".into(),
                protocol: Protocol::Tcp,
                published: 8080
            }]
        );
        assert_eq!(web.whole_drive_mounts, BTreeSet::from(["fast".to_string()]));
        assert_eq!(web.drives, BTreeSet::from(["fast".to_string()]));

        let worker = &per_service["worker"];
        assert_eq!(worker.pinned_node, None);
        assert!(worker.placement_constraints.is_empty());
        assert!(worker.bound_ports.is_empty());
        assert!(worker.whole_drive_mounts.is_empty());
        assert_eq!(worker.drives, BTreeSet::from(["fast".to_string()]));
    }

    #[test]
    fn a_config_with_no_file_is_not_counted_as_bundled() {
        let spec = r#"
services:
  web:
    image: nginx
    configs:
      - shared_conf
configs:
  shared_conf: {}
"#;
        let stack = StackFile::parse(spec).expect("parse");
        let r = stack.requirements();
        assert!(r.configs.contains("shared_conf"));
        assert!(r.file_configs.is_empty());
    }
}
