//! Resolving a node name to the https origin its api answers on, the same way
//! other node-to-node lookups in this codebase read it out of the config
//! rather than the replicated state, which has no notion of an api port.

use cl_config::ClusterConfig;

/// `https://<internal-ip>:<web-port>` for a node, or `None` if it has no
/// address on the internal network.
pub fn base_url(cfg: &ClusterConfig, node_name: &str) -> Option<String> {
    let node = cfg.node(node_name)?;
    let ip = cfg.internal_ip(node)?;
    Some(format!("https://{ip}:{}", cfg.web.port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ClusterConfig {
        cl_config::ClusterConfig::parse(
            r#"
cluster:
  name: home
networks:
  internal:
    type: internal
    subnet: 10.10.0.0/24
    domain: int.example.com
nodes:
  n1:
    id: 3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f01
    role: manager
    network:
      internal: 10.10.0.1
web:
  address: cloud
  network: internal
  port: 8443
"#,
        )
        .expect("valid config")
    }

    #[test]
    fn resolves_the_internal_address_and_web_port() {
        let cfg = cfg();
        assert_eq!(
            base_url(&cfg, "n1").as_deref(),
            Some("https://10.10.0.1:8443")
        );
    }

    #[test]
    fn an_unknown_node_resolves_to_nothing() {
        let cfg = cfg();
        assert_eq!(base_url(&cfg, "ghost"), None);
    }
}
