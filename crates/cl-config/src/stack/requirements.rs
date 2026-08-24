//! What a stack needs from the cluster to schedule: networks, drives, and
//! cluster configs/secrets its services reference. Used by the ui to explain
//! why a service cannot be placed, and to know which configs/secrets a stack
//! bundles its own file for (so redeploying can offer to replace them).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::StackFile;

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
            r.configs.extend(svc.configs.iter().map(|c| c.source.clone()));
            r.secrets.extend(svc.secrets.iter().map(|s| s.source.clone()));
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(r.networks, BTreeSet::from(["internal".into(), "public".into()]));
        assert_eq!(r.drives, BTreeSet::from(["data".into()]));
        assert_eq!(r.configs, BTreeSet::from(["headscale_config".into()]));
        assert_eq!(r.secrets, BTreeSet::from(["db_password".into()]));
        assert_eq!(r.file_configs, BTreeSet::from(["headscale_config".into()]));
        assert_eq!(r.file_secrets, BTreeSet::from(["db_password".into()]));
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
