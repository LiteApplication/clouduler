//! Validation is split in two: syntax checks that only look at the stack file
//! itself (so a stack can be linted with no cluster at hand), and cross-checks
//! against a `ClusterConfig` for the fields that must resolve against it.

use std::collections::{BTreeMap, BTreeSet};

use crate::cluster::ClusterConfig;
use crate::error::{ConfigError, Result};

use super::deploy::{parse_memory_bytes, MAX_REPLICAS};
use super::port::Protocol;
use super::StackFile;

impl StackFile {
    /// Checks that only look at the stack file itself.
    pub fn validate_syntax(&self) -> Result<()> {
        self.validate_service_names()?;
        self.validate_depends_on()?;
        self.validate_service_networks_declared()?;
        self.validate_no_duplicate_ports()?;
        self.validate_stop_signals()?;
        self.validate_colocate()?;
        self.validate_writable_secrets()?;
        self.validate_replicas()?;
        self.validate_memory_sizes()?;
        Ok(())
    }

    /// Replica counts are capped at `MAX_REPLICAS`: the scheduler materializes
    /// one record per replica every pass, so a typo like `replicas: 4000000000`
    /// would otherwise take down whichever node is leader.
    fn validate_replicas(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let replicas = svc.deploy.as_ref().and_then(|d| d.replicas);
            if let Some(n) = replicas.filter(|n| *n > MAX_REPLICAS) {
                return Err(ConfigError::invalid(
                    format!("services.{name}.deploy.replicas"),
                    format!("{n} is more than the maximum of {MAX_REPLICAS}"),
                ));
            }
        }
        Ok(())
    }

    /// A memory size whose digits parse but overflow a u64 once the unit is
    /// applied is rejected. (Other unparseable sizes are still ignored, as
    /// they always were.)
    fn validate_memory_sizes(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(resources) = svc.deploy.as_ref().and_then(|d| d.resources.as_ref()) else {
                continue;
            };
            for (kind, spec) in [
                ("limits", &resources.limits),
                ("reservations", &resources.reservations),
            ] {
                let Some(raw) = spec.as_ref().and_then(|s| s.memory.as_deref()) else {
                    continue;
                };
                if parse_memory_bytes(raw).is_none() && memory_overflows(raw) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.deploy.resources.{kind}.memory"),
                        format!("\"{raw}\" is too large to be a byte count"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// `write: true` feeds a container's edits back into the cluster's copy
    /// of the object — fine for a config, never for a secret: nothing should
    /// be able to overwrite a secret's plaintext by editing its own mounted
    /// copy of it.
    fn validate_writable_secrets(&self) -> Result<()> {
        for (name, svc) in &self.services {
            for (idx, secret) in svc.secrets.iter().enumerate() {
                if secret.write {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.secrets[{idx}]"),
                        "write is not allowed on a secret, only on a config",
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_colocate(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(ext) = &svc.x_clouduler else {
                continue;
            };
            for other in &ext.colocate {
                if other == name {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.x-clouduler.colocate"),
                        "a service cannot colocate with itself",
                    ));
                }
                if !self.services.contains_key(other) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.x-clouduler.colocate"),
                        format!("no service named {other} in this stack"),
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_stop_signals(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(signal) = &svc.stop_signal else {
                continue;
            };
            let valid = signal.strip_prefix("SIG").is_some_and(|rest| {
                !rest.is_empty() && rest.chars().all(|c| c.is_ascii_uppercase())
            }) || (!signal.is_empty() && signal.chars().all(|c| c.is_ascii_digit()));
            if !valid {
                return Err(ConfigError::invalid(
                    format!("services.{name}.stop_signal"),
                    format!("\"{signal}\" is not a signal name (e.g. SIGINT) or number"),
                ));
            }
        }
        Ok(())
    }

    /// Full validation: syntax checks plus everything that needs to resolve
    /// against the cluster this stack would run on.
    pub fn validate(&self, cluster: &ClusterConfig) -> Result<()> {
        self.validate_syntax()?;
        self.validate_networks_exist(cluster)?;
        self.validate_networks_implemented(cluster)?;
        self.validate_drives_exist(cluster)?;
        self.validate_whole_drive_mounts(cluster)?;
        self.validate_proxy_expose_networks(cluster)?;
        self.validate_service_network_kinds(cluster)?;
        self.validate_node_pins(cluster)?;
        self.validate_placement_constraints(cluster)?;
        Ok(())
    }

    fn validate_service_names(&self) -> Result<()> {
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

    fn validate_depends_on(&self) -> Result<()> {
        for (name, svc) in &self.services {
            for dep in &svc.depends_on {
                if !self.services.contains_key(dep) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.depends_on"),
                        format!("no service named {dep} in this stack"),
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_service_networks_declared(&self) -> Result<()> {
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

    fn validate_no_duplicate_ports(&self) -> Result<()> {
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
                    // Rejected rather than ignored: silently dropping it
                    // would leave someone believing their backend is seeing
                    // real client addresses when it never will.
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

    fn validate_networks_exist(&self, cluster: &ClusterConfig) -> Result<()> {
        for name in self.networks.keys() {
            if !cluster.networks.contains_key(name) {
                return Err(ConfigError::invalid(
                    format!("networks.{name}"),
                    format!("no network named {name} declared in the cluster"),
                ));
            }
        }
        Ok(())
    }

    /// A network no node implements can never host a placement, so a service
    /// naming one could never schedule. Same idiom as `validate_drives` in
    /// the cluster config, but per stack since that's where a service's
    /// `networks:` list lives.
    fn validate_networks_implemented(&self, cluster: &ClusterConfig) -> Result<()> {
        use crate::cluster::NetworkKind;

        for (name, svc) in &self.services {
            for net in &svc.networks {
                // A nat network declares no per-node address at all: every
                // node implements it identically, so there is nothing to
                // check here the way there is for an egress or wireguard
                // interface.
                if cluster.networks.get(net).map(|n| n.kind) == Some(NetworkKind::Nat) {
                    continue;
                }
                if !cluster.nodes.values().any(|n| n.network.contains_key(net)) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.networks"),
                        format!(
                            "no node implements network {net}, this service could never be placed"
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_drives_exist(&self, cluster: &ClusterConfig) -> Result<()> {
        for (name, svc) in &self.services {
            for (idx, vol) in svc.volumes.iter().enumerate() {
                let Some(drive) = &vol.drive else { continue };
                if !cluster.drives.contains_key(drive) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.volumes[{idx}]"),
                        format!("no drive named {drive} declared in the cluster"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// A volume mount whose subpath is exactly `.` binds a drive's real root
    /// directory straight into the container (`cl_drive::registry::resolve_relative`
    /// resolves it to the drive root itself), bypassing the scoped,
    /// per-volume path every other mount goes through. That is only safe when
    /// there is exactly one node holding the real directory and this service
    /// is pinned there: unpinned, or with more than one node able to hold the
    /// data, the scheduler could place the container against a different
    /// node's copy (or, for a served drive, an NFS mountpoint of somebody
    /// else's copy) than the last time it ran, silently diverging from
    /// whatever else touches that drive.
    fn validate_whole_drive_mounts(&self, cluster: &ClusterConfig) -> Result<()> {
        for (name, svc) in &self.services {
            for (idx, vol) in svc.volumes.iter().enumerate() {
                let (Some(drive), Some(volume)) = (&vol.drive, &vol.volume) else {
                    continue;
                };
                if volume != "." {
                    continue;
                }
                let at = format!("services.{name}.volumes[{idx}]");
                let holders = drive_root_holders(cluster, drive);
                let Some(sole) = holders.iter().next().filter(|_| holders.len() == 1) else {
                    return Err(ConfigError::invalid(
                        at,
                        format!(
                            "drive {drive} can be served by more than one node ({}), so mounting {drive}/. \
                             (its whole root) is not allowed: pin a specific volume under it instead",
                            holders.iter().cloned().collect::<Vec<_>>().join(", ")
                        ),
                    ));
                };
                match self.effective_node(name) {
                    Some(node) if &node == sole => {}
                    Some(node) => {
                        return Err(ConfigError::invalid(
                            at,
                            format!(
                                "{name} is pinned to {node}, but only {sole} holds the real directory for drive {drive}; \
                                 mounting {drive}/. needs x-clouduler.node: {sole}"
                            ),
                        ));
                    }
                    None => {
                        return Err(ConfigError::invalid(
                            at,
                            format!(
                                "mounting {drive}/. (its whole root) requires pinning {name} with x-clouduler.node: {sole}, \
                                 the only node holding the real directory for drive {drive}"
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_proxy_expose_networks(&self, cluster: &ClusterConfig) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(ext) = &svc.x_clouduler else {
                continue;
            };
            for (idx, route) in ext.proxy.iter().enumerate() {
                let at = format!("services.{name}.x-clouduler.proxy[{idx}]");
                let Some(net) = cluster.networks.get(&route.network) else {
                    return Err(ConfigError::invalid(
                        format!("{at}.network"),
                        format!("no network named {} declared in the cluster", route.network),
                    ));
                };
                if net.domain.is_none() {
                    return Err(ConfigError::invalid(
                        format!("{at}.network"),
                        format!(
                            "network {} has no domain, so it cannot serve an HTTP route",
                            route.network
                        ),
                    ));
                }
                for cors in route.middlewares.iter().filter_map(|m| match m {
                    crate::stack::Middleware::Cors(c) => Some(c),
                    _ => None,
                }) {
                    if cors.allow_credentials && cors.origins.iter().any(|o| o == "*") {
                        return Err(ConfigError::invalid(
                            format!("{at}.middlewares"),
                            "cors: allow_credentials cannot be combined with a wildcard origin, browsers reject the pair",
                        ));
                    }
                }
            }
            for (idx, expose) in ext.expose.iter().enumerate() {
                let at = format!("services.{name}.x-clouduler.expose[{idx}]");
                let Some(net) = cluster.networks.get(&expose.network) else {
                    return Err(ConfigError::invalid(
                        format!("{at}.network"),
                        format!(
                            "no network named {} declared in the cluster",
                            expose.network
                        ),
                    ));
                };
                // Only a public network's router, or a wireguard tunnel's far
                // end forwarding a port back in (e.g. a VPN provider's port
                // forwarding feature), can make a raw port reachable from
                // outside. At least one node must have it marked open, or
                // this could never actually be reached.
                use crate::cluster::NetworkKind;
                if matches!(net.kind, NetworkKind::Public | NetworkKind::Wireguard) {
                    let wanted = crate::cluster::OpenPort {
                        port: expose.published,
                        protocol: expose.protocol,
                    };
                    let reachable = cluster.nodes.values().any(|n| {
                        n.network
                            .get(&expose.network)
                            .is_some_and(|addr| match addr {
                                crate::cluster::NodeAddress::Public(p) => {
                                    p.open_ports.contains(&wanted)
                                }
                                crate::cluster::NodeAddress::Interface(i) => {
                                    i.open_ports.contains(&wanted)
                                }
                                crate::cluster::NodeAddress::Ip(_) => false,
                            })
                    });
                    if !reachable {
                        return Err(ConfigError::invalid(
                            at,
                            format!(
                                "no node has {}/{} open on network {}, this port could never be reached",
                                expose.published, expose.protocol, expose.network
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// A service may attach to at most one egress network: two would mean
    /// two conflicting default routes for its outbound traffic, which is not
    /// a thing. This is layered on top of whatever the stack already allows
    /// for its other (overlay) networks.
    fn validate_service_network_kinds(&self, cluster: &ClusterConfig) -> Result<()> {
        use crate::cluster::NetworkKind;

        for (name, svc) in &self.services {
            let mut egress = None;
            for net in &svc.networks {
                // Already checked to exist by validate_networks_exist.
                if !matches!(
                    cluster.networks.get(net).map(|n| n.kind),
                    Some(NetworkKind::Egress | NetworkKind::Wireguard | NetworkKind::Nat)
                ) {
                    continue;
                }
                if let Some(existing) = &egress {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.networks"),
                        format!("only one egress network is allowed per service, found {existing} and {net}"),
                    ));
                }
                egress = Some(net.clone());
            }
        }
        Ok(())
    }

    fn validate_node_pins(&self, cluster: &ClusterConfig) -> Result<()> {
        if let Some(defaults) = &self.x_clouduler {
            if let Some(node) = &defaults.node {
                if !cluster.nodes.contains_key(node) {
                    return Err(ConfigError::invalid(
                        "x-clouduler.node",
                        format!("no node named {node} in the cluster"),
                    ));
                }
            }
        }
        for name in self.services.keys() {
            let Some(node) = self.effective_node(name) else {
                continue;
            };
            if !cluster.nodes.contains_key(&node) {
                return Err(ConfigError::invalid(
                    format!("services.{name}.x-clouduler.node"),
                    format!("no node named {node} in the cluster"),
                ));
            }
            if let Some(net) = self.egress_network(name, cluster) {
                // A nat network needs no interface address anywhere: every
                // node implements it the same way, so a pin to any node is
                // always fine.
                let is_nat = cluster.networks.get(&net).map(|n| n.kind)
                    == Some(crate::cluster::NetworkKind::Nat);
                let implements = is_nat
                    || cluster
                        .nodes
                        .get(&node)
                        .map(|n| {
                            n.network
                                .get(&net)
                                .is_some_and(|a| a.as_interface().is_some())
                        })
                        .unwrap_or(false);
                if !implements {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.x-clouduler.node"),
                        format!("{node} has no interface address on egress network {net}"),
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_placement_constraints(&self, cluster: &ClusterConfig) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(deploy) = &svc.deploy else { continue };
            let Some(placement) = &deploy.placement else {
                continue;
            };
            for (idx, constraint) in placement.constraints.iter().enumerate() {
                let Some(node) = constraint
                    .strip_prefix("node.hostname==")
                    .or_else(|| constraint.strip_prefix("node=="))
                else {
                    continue;
                };
                if !cluster.nodes.contains_key(node) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.deploy.placement.constraints[{idx}]"),
                        format!("no node named {node} in the cluster"),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Nodes holding a real, non-mountpoint local directory for `drive_name`: for
/// a replicated drive, every node that implements it; for a served drive,
/// only its `served_by` candidates (every other implementing node gets an NFS
/// mountpoint of whichever candidate is currently active, not real data).
fn drive_root_holders(cluster: &ClusterConfig, drive_name: &str) -> BTreeSet<String> {
    let Some(drive) = cluster.drives.get(drive_name) else {
        return BTreeSet::new();
    };
    if drive.is_served() {
        drive
            .served_by
            .iter()
            .filter(|n| {
                cluster
                    .nodes
                    .get(n.as_str())
                    .is_some_and(|node| node.implements(drive_name))
            })
            .cloned()
            .collect()
    } else {
        cluster
            .nodes
            .iter()
            .filter(|(_, node)| node.implements(drive_name))
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// Whether `raw` is all digits (plus an optional unit letter) but too big for
/// a u64 - the one way `parse_memory_bytes` fails that is worth an error.
fn memory_overflows(raw: &str) -> bool {
    let raw = raw.trim();
    let digits = raw.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &raw[digits.len()..];
    unit.len() <= 1
        && !digits.trim().is_empty()
        && digits.trim().chars().all(|c| c.is_ascii_digit())
}

/// A stack name ends up in container names (`<stack>_<service>_<index>`), in
/// overlay network names and in the git backup's `stacks/<name>.yaml`, so it
/// has to be safe in all of them: lowercase letters, digits, `-` and `_`,
/// starting and ending with a letter or digit, at most 63 bytes. `_` is
/// allowed because existing stacks use it; the ambiguity it creates in
/// `<stack>_<network>` is handled by `cl_sched::run::overlay_collision`.
pub fn validate_stack_name(name: &str) -> std::result::Result<(), String> {
    let invalid = |why: &str| Err(format!("invalid stack name {name:?}: {why}"));
    if name.is_empty() || name.len() > 63 {
        return invalid("must be 1 to 63 bytes long");
    }
    let edge_ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !name.starts_with(edge_ok) || !name.ends_with(edge_ok) {
        return invalid("must start and end with a lowercase letter or digit");
    }
    if !name.chars().all(|c| edge_ok(c) || c == '-' || c == '_') {
        return invalid("only lowercase letters, digits, `-` and `_` are allowed");
    }
    Ok(())
}

fn is_dns_label(s: &str) -> bool {
    if s.is_empty() || s.len() > 63 {
        return false;
    }
    if s.starts_with('-') || s.ends_with('-') {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::ClusterConfig;

    const CLUSTER_YAML: &str = include_str!("../../../../examples/cluster.yaml");

    fn cluster() -> ClusterConfig {
        ClusterConfig::parse(CLUSTER_YAML).unwrap()
    }

    fn stack(raw: &str) -> StackFile {
        StackFile::parse(raw).expect("stack parses")
    }

    #[test]
    fn stack_name_check() {
        for ok in [
            "a",
            "web",
            "my-stack",
            "my_stack",
            "a1",
            "0day",
            &"a".repeat(63),
        ] {
            assert!(validate_stack_name(ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "a/b",
            "/a",
            "..",
            "../x",
            "a b",
            "Web",
            "-web",
            "web-",
            "_web",
            "web_",
            "a:b",
            "a.b",
            &"a".repeat(64),
        ] {
            assert!(
                validate_stack_name(bad).is_err(),
                "{bad:?} should be invalid"
            );
        }
    }

    #[test]
    fn dns_label_check() {
        assert!(is_dns_label("gitlab"));
        assert!(is_dns_label("gitlab-ce"));
        assert!(!is_dns_label("Gitlab"));
        assert!(!is_dns_label("-gitlab"));
        assert!(!is_dns_label("gitlab-"));
        assert!(!is_dns_label("gitlab_ce"));
    }

    #[test]
    fn absurd_replica_counts_are_rejected() {
        let err = StackFile::parse(
            "services:\n  web:\n    image: x\n    deploy:\n      replicas: 4000000000\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("services.web.deploy.replicas"));
        assert!(err.to_string().contains("maximum"));
        assert!(StackFile::parse(&format!(
            "services:\n  web:\n    image: x\n    deploy:\n      replicas: {MAX_REPLICAS}\n"
        ))
        .is_ok());
    }

    #[test]
    fn an_overflowing_memory_limit_is_rejected() {
        let err = StackFile::parse(
            "services:\n  web:\n    image: x\n    deploy:\n      resources:\n        limits:\n          memory: 99999999999999999999g\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("deploy.resources.limits.memory"));
        let err = StackFile::parse(
            "services:\n  web:\n    image: x\n    deploy:\n      resources:\n        reservations:\n          memory: 17179869184g\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn invalid_service_name_rejected() {
        let raw = "services:\n  Bad_Name:\n    image: x\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("services.Bad_Name"));
        assert!(err.to_string().contains("DNS label"));
    }

    #[test]
    fn depends_on_target_must_exist() {
        let raw = "services:\n  a:\n    image: x\n    depends_on: [b]\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("services.a.depends_on"));
        assert!(err.to_string().contains("no service named b"));
    }

    #[test]
    fn service_network_must_be_declared_in_stack() {
        let raw = "services:\n  a:\n    image: x\n    networks: [wan]\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("services.a.networks"));
        assert!(err.to_string().contains("not declared"));
    }

    #[test]
    fn duplicate_published_ports_rejected() {
        let raw = r#"
services:
  a:
    image: x
    ports: ["8080:80"]
  b:
    image: x
    ports: ["8080:81"]
"#;
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("already published by"));
    }

    /// Silently ignoring it would leave someone believing their backend is
    /// seeing real client addresses when it never can.
    #[test]
    fn proxy_protocol_on_a_udp_port_is_rejected() {
        let raw = r#"
services:
  a:
    image: x
    x-clouduler:
      expose:
        - { port: 53, published: 53, protocol: udp, network: public, proxy_protocol: true }
"#;
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("only applies to tcp"), "{err}");
    }

    #[test]
    fn proxy_protocol_on_a_tcp_port_is_accepted() {
        let raw = r#"
services:
  a:
    image: x
    x-clouduler:
      expose:
        - { port: 25, published: 25, protocol: tcp, network: public, proxy_protocol: true }
"#;
        let file = StackFile::parse(raw).expect("tcp is the supported case");
        let expose = &file.services["a"].x_clouduler.as_ref().unwrap().expose[0];
        assert!(expose.proxy_protocol);
    }

    /// The default has to stay off: a backend that does not speak PROXY
    /// protocol sees a malformed request the moment one appears.
    #[test]
    fn proxy_protocol_defaults_to_off() {
        let raw = r#"
services:
  a:
    image: x
    x-clouduler:
      expose:
        - { port: 25, published: 25, network: public }
"#;
        let file = StackFile::parse(raw).expect("parse");
        assert!(!file.services["a"].x_clouduler.as_ref().unwrap().expose[0].proxy_protocol);
    }

    #[test]
    fn cluster_network_must_exist() {
        let raw = "networks:\n  bogus: {}\nservices:\n  a:\n    image: x\n";
        let err = stack(raw).validate(&cluster()).unwrap_err();
        assert!(err.to_string().contains("networks.bogus"));
    }

    #[test]
    fn drive_reference_must_exist() {
        let raw = "services:\n  a:\n    image: x\n    volumes:\n      - bogus/data:/data\n";
        let err = stack(raw).validate(&cluster()).unwrap_err();
        assert!(err.to_string().contains("services.a.volumes[0]"));
        assert!(err.to_string().contains("no drive named bogus"));
    }

    #[test]
    fn proxy_network_must_have_a_domain() {
        let raw = r#"
networks:
  tailscale: {}
services:
  a:
    image: x
    networks: [tailscale]
    x-clouduler:
      proxy:
        - host: a
          network: bogus
          port: 80
"#;
        let err = stack(raw).validate(&cluster()).unwrap_err();
        assert!(err
            .to_string()
            .contains("services.a.x-clouduler.proxy[0].network"));
    }

    #[test]
    fn node_pin_must_exist() {
        let raw = "services:\n  a:\n    image: x\n    x-clouduler: { node: bogus }\n";
        let err = stack(raw).validate(&cluster()).unwrap_err();
        assert!(err.to_string().contains("services.a.x-clouduler.node"));
        assert!(err.to_string().contains("no node named bogus"));
    }

    #[test]
    fn network_with_no_implementing_node_is_rejected() {
        let raw = r#"
networks:
  unreachable: {}
services:
  a:
    image: x
    networks: [unreachable]
"#;
        // Add a network to the reference cluster that no node opts into.
        let yaml = CLUSTER_YAML.replace(
            "  vpn:\n    type: egress\n",
            "  vpn:\n    type: egress\n  unreachable:\n    type: egress\n",
        );
        let cluster = ClusterConfig::parse(&yaml).expect("patched cluster is valid");
        let err = stack(raw).validate(&cluster).unwrap_err();
        assert!(err
            .to_string()
            .contains("no node implements network unreachable"));
    }

    #[test]
    fn a_nat_network_needs_no_implementing_node() {
        let raw = r#"
networks:
  home: {}
services:
  a:
    image: x
    networks: [home]
    x-clouduler: { node: n1 }
"#;
        let yaml = CLUSTER_YAML.replace(
            "  vpn:\n    type: egress\n",
            "  vpn:\n    type: egress\n  home:\n    type: nat\n",
        );
        let cluster = ClusterConfig::parse(&yaml).expect("patched cluster is valid");
        // No node has (or needs) an interface address on `home`, yet the
        // service, even pinned to a specific node, is still accepted.
        stack(raw)
            .validate(&cluster)
            .expect("a nat network is implemented by every node");
    }

    #[test]
    fn a_service_cannot_attach_to_both_an_egress_and_a_nat_network() {
        let raw = r#"
networks:
  vpn: {}
  home: {}
services:
  a:
    image: x
    networks: [vpn, home]
"#;
        let yaml = CLUSTER_YAML.replace(
            "  vpn:\n    type: egress\n",
            "  vpn:\n    type: egress\n  home:\n    type: nat\n",
        );
        let cluster = ClusterConfig::parse(&yaml).expect("patched cluster is valid");
        let err = stack(raw).validate(&cluster).unwrap_err();
        assert!(err
            .to_string()
            .contains("only one egress network is allowed per service"));
    }

    #[test]
    fn expose_port_nobody_has_open_is_rejected() {
        let raw = r#"
services:
  a:
    image: x
    x-clouduler:
      expose:
        - port: 25
          published: 25
          network: wan
"#;
        let err = stack(raw).validate(&cluster()).unwrap_err();
        assert!(err
            .to_string()
            .contains("no node has 25/tcp open on network wan"));
    }

    #[test]
    fn expose_port_a_node_has_open_is_accepted() {
        let raw = r#"
services:
  a:
    image: x
    x-clouduler:
      expose:
        - port: 25
          published: 25
          network: wan
"#;
        let mut cfg = cluster();
        cfg.nodes.get_mut("n1").unwrap().network.insert(
            "wan".to_string(),
            crate::cluster::NodeAddress::Public(crate::cluster::PublicAddress {
                ipv4: Some("203.0.113.9".parse().unwrap()),
                ipv6: None,
                domain: None,
                open_ports: [crate::cluster::OpenPort {
                    port: 25,
                    protocol: crate::stack::Protocol::Tcp,
                }]
                .into_iter()
                .collect(),
            }),
        );
        stack(raw).validate(&cfg).expect("a node has the port open");
    }

    #[test]
    fn expose_port_a_wireguard_node_has_open_is_accepted() {
        let raw = r#"
services:
  a:
    image: x
    x-clouduler:
      expose:
        - port: 22
          published: 51413
          network: vpn
"#;
        let yaml = CLUSTER_YAML.replace(
            "  vpn:\n    type: egress\n",
            "  vpn:\n    type: wireguard\n",
        );
        let mut cfg = ClusterConfig::parse(&yaml).expect("patched cluster is valid");

        let err = stack(raw).validate(&cfg).unwrap_err();
        assert!(err
            .to_string()
            .contains("no node has 51413/tcp open on network vpn"));

        cfg.nodes.get_mut("n1").unwrap().network.insert(
            "vpn".to_string(),
            crate::cluster::NodeAddress::Interface(crate::cluster::InterfaceAddress {
                interface: "wg-vpn".to_string(),
                open_ports: [crate::cluster::OpenPort {
                    port: 51413,
                    protocol: crate::stack::Protocol::Tcp,
                }]
                .into_iter()
                .collect(),
                fallback: None,
            }),
        );
        stack(raw)
            .validate(&cfg)
            .expect("n1's tunnel has the port open");
    }

    #[test]
    fn colocate_target_must_exist() {
        let raw = "services:\n  a:\n    image: x\n    x-clouduler:\n      colocate: [bogus]\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("services.a.x-clouduler.colocate"));
        assert!(err.to_string().contains("no service named bogus"));
    }

    #[test]
    fn colocate_with_self_is_rejected() {
        let raw = "services:\n  a:\n    image: x\n    x-clouduler:\n      colocate: [a]\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("cannot colocate with itself"));
    }

    #[test]
    fn colocate_with_a_real_service_is_accepted() {
        let raw = "services:\n  a:\n    image: x\n    x-clouduler:\n      colocate: [b]\n  b:\n    image: x\n";
        StackFile::parse(raw).expect("valid colocate target");
    }

    #[test]
    fn a_writable_secret_is_rejected() {
        let raw = "services:\n  a:\n    image: x\n    secrets:\n      - source: db_password\n        write: true\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("services.a.secrets[0]"));
        assert!(err.to_string().contains("not allowed on a secret"));
    }

    #[test]
    fn a_writable_config_is_accepted() {
        let raw = "services:\n  a:\n    image: x\n    configs:\n      - source: nginx_conf\n        write: true\n";
        StackFile::parse(raw).expect("a writable config is fine");
    }

    #[test]
    fn whole_drive_mount_on_a_multi_holder_drive_is_rejected() {
        // "bulk" is implemented by both n1 and n3 in the reference cluster.
        let raw = "services:\n  a:\n    image: x\n    volumes:\n      - bulk/.:/data\n    x-clouduler: { node: n1 }\n";
        let err = stack(raw).validate(&cluster()).unwrap_err();
        assert!(err.to_string().contains("services.a.volumes[0]"));
        assert!(err
            .to_string()
            .contains("can be served by more than one node"));
    }

    fn single_holder_cluster() -> ClusterConfig {
        // Drop n2's "fast" so n1 is the drive's only holder.
        let yaml = CLUSTER_YAML.replace(
            "      wan:\n        domain: n2.example.net # will use CNAME\n    drives:\n      fast: /srv/clouduler/fast\n",
            "      wan:\n        domain: n2.example.net # will use CNAME\n    drives: {}\n",
        );
        ClusterConfig::parse(&yaml).expect("patched cluster is valid")
    }

    #[test]
    fn whole_drive_mount_without_a_pin_is_rejected() {
        let raw = "services:\n  a:\n    image: x\n    volumes:\n      - fast/.:/data\n";
        let err = stack(raw).validate(&single_holder_cluster()).unwrap_err();
        assert!(err.to_string().contains("services.a.volumes[0]"));
        assert!(err.to_string().contains("requires pinning"));
        assert!(err.to_string().contains("x-clouduler.node: n1"));
    }

    #[test]
    fn whole_drive_mount_pinned_to_the_wrong_node_is_rejected() {
        let raw = "services:\n  a:\n    image: x\n    volumes:\n      - fast/.:/data\n    x-clouduler: { node: n3 }\n";
        let err = stack(raw).validate(&single_holder_cluster()).unwrap_err();
        assert!(err.to_string().contains("only n1 holds the real directory"));
    }

    #[test]
    fn whole_drive_mount_pinned_to_the_sole_holder_is_accepted() {
        let raw = "services:\n  a:\n    image: x\n    volumes:\n      - fast/.:/data\n    x-clouduler: { node: n1 }\n";
        stack(raw)
            .validate(&single_holder_cluster())
            .expect("pinned to the drive's only holder");
    }

    #[test]
    fn placement_constraint_node_must_exist() {
        let raw = r#"
services:
  a:
    image: x
    deploy:
      placement:
        constraints: ["node.hostname==bogus"]
"#;
        let err = stack(raw).validate(&cluster()).unwrap_err();
        assert!(err.to_string().contains("deploy.placement.constraints[0]"));
    }
}
