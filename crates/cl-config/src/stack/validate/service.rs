use crate::cluster::ClusterConfig;
use crate::error::{ConfigError, Result};
use crate::names::is_dns_label;
use crate::stack::port::Protocol;
use crate::stack::StackFile;
use std::collections::BTreeMap;

impl StackFile {
    pub(super) fn validate_container_router(
        &self,
        stack: &str,
        cluster: &ClusterConfig,
    ) -> Result<()> {
        for (name, svc) in &self.services {
            if let Some(net) = cluster.container_router_of(stack, name) {
                if svc.networks.iter().any(|n| n == net) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.networks"),
                        format!(
                            "{name} is the router of container network {net} and is attached to it automatically, remove it from networks"
                        ),
                    ));
                }
                let deploy = svc.deploy.as_ref();
                let global = deploy.and_then(|d| d.mode) == Some(crate::stack::DeployMode::Global);
                if global || deploy.and_then(|d| d.replicas).unwrap_or(1) > 1 {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.deploy"),
                        format!(
                            "{name} is the router of container network {net} and must run exactly one replica (no global mode, replicas at most 1)"
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_service_names(&self) -> Result<()> {
        for name in self.services.keys() {
            if !is_dns_label(name) {
                return Err(ConfigError::invalid(
                    format!("services.{name}"),
                    "service name must be a valid DNS label (lowercase letters, digits, hyphens, not starting or ending with a hyphen) since it becomes an overlay hostname",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn validate_depends_on(&self) -> Result<()> {
        for (name, svc) in &self.services {
            for dep in &svc.depends_on {
                if !self.services.contains_key(dep) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.depends_on"),
                        format!("no service named {dep} in this stack"),
                    ));
                }
                if dep == name {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.depends_on"),
                        "a service cannot depend on itself",
                    ));
                }
            }
        }
        // A cycle would leave every service in it waiting for the others forever.
        // Depth first, tracking the path so the message can name the loop.
        fn visit<'a>(
            file: &'a StackFile,
            name: &'a str,
            path: &mut Vec<&'a str>,
            done: &mut std::collections::BTreeSet<&'a str>,
        ) -> Option<String> {
            if let Some(at) = path.iter().position(|n| *n == name) {
                let mut cycle: Vec<&str> = path[at..].to_vec();
                cycle.push(name);
                return Some(cycle.join(" -> "));
            }
            if done.contains(name) {
                return None;
            }
            path.push(name);
            for dep in &file.services[name].depends_on {
                if let Some(found) = visit(file, dep, path, done) {
                    return Some(found);
                }
            }
            path.pop();
            done.insert(name);
            None
        }
        let mut done = std::collections::BTreeSet::new();
        for name in self.services.keys() {
            if let Some(cycle) = visit(self, name, &mut Vec::new(), &mut done) {
                return Err(ConfigError::invalid(
                    format!("services.{name}.depends_on"),
                    format!("dependency cycle: {cycle}"),
                ));
            }
        }
        Ok(())
    }

    /// `env_file` is parsed but never injected, so a service relying on it would
    /// start without its variables. The web UI rewrites it into `environment`
    /// and secrets before the spec is sent.
    pub(super) fn validate_env_file(&self) -> Result<()> {
        for (name, svc) in &self.services {
            if !svc.env_file.is_empty() {
                return Err(ConfigError::invalid(
                    format!("services.{name}.env_file"),
                    "env_file is not supported in a stack file; move its entries into environment, or paste it in the web UI's stack editor to import it",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn validate_service_networks_declared(&self) -> Result<()> {
        for (name, svc) in &self.services {
            for net in &svc.networks {
                if !self.networks.contains_key(net) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.networks"),
                        format!("network {net} is not declared in this stack's top-level networks"),
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_no_duplicate_ports(&self) -> Result<()> {
        let mut seen: BTreeMap<(Protocol, u16), String> = BTreeMap::new();

        for (name, svc) in &self.services {
            for (idx, port) in svc.ports.iter().enumerate() {
                let Some(published) = port.published else {
                    continue;
                };
                for p in published.start..=published.end {
                    let at = format!("services.{name}.ports[{idx}]");
                    if let Some(existing) = seen.insert((port.protocol, p), at.clone()) {
                        return Err(ConfigError::invalid(
                            at,
                            format!(
                                "port {p}/{} is already published by {existing}",
                                port.protocol
                            ),
                        ));
                    }
                }
            }
            if let Some(ext) = &svc.x_clouduler {
                for (idx, expose) in ext.expose.iter().enumerate() {
                    let at = format!("services.{name}.x-clouduler.expose[{idx}]");
                    if let Some(existing) =
                        seen.insert((expose.protocol, expose.published), at.clone())
                    {
                        return Err(ConfigError::invalid(
                            at,
                            format!(
                                "port {}/{} is already published by {existing}",
                                expose.published, expose.protocol
                            ),
                        ));
                    }
                    // Rejected rather than ignored: silently dropping it would leave someone
                    // believing the backend sees real client addresses.
                    if expose.proxy_protocol && expose.protocol == Protocol::Udp {
                        return Err(ConfigError::invalid(
                            at,
                            "proxy_protocol only applies to tcp: the header is written once at the \
                             head of a stream, and udp has no stream to put it at the head of",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}
