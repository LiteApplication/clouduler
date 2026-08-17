//! Expanding stack files into the concrete instances the cluster should run.

use std::collections::BTreeSet;

use cl_config::{DeployMode, Service, StackFile};
use cl_raft::ids::InstanceId;

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
}

/// Expand a stack into its instances. Replica counts come from
/// `deploy.replicas`, defaulting to one; global services are expanded later,
/// once the eligible node set is known.
pub fn expand(stack: &str, file: &StackFile) -> Vec<Desired> {
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
        let desired = expand("site", &file);
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
        let desired = expand("site", &file);
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
        let desired = expand("site", &file);
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
        let desired = expand("site", &file);
        assert_eq!(
            desired[0].drives,
            ["bulk".to_string(), "fast".to_string()].into()
        );
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
        let desired = expand("site", &file);
        assert_eq!(desired[0].constraints, vec!["node.role == manager"]);
        assert_eq!(desired[0].pinned_node.as_deref(), Some("n2"));
    }
}
