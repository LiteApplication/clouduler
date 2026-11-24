//! Validation is split in two: syntax checks that only look at the stack file
//! itself (so a stack can be linted with no cluster at hand), and cross-checks
//! against a `ClusterConfig` for the fields that must resolve against it.

use std::collections::{BTreeMap, BTreeSet};

use crate::cluster::{ClusterConfig, MAX_STOP_GRACE};
use crate::error::{ConfigError, Result};

use super::deploy::{parse_memory_bytes, MAX_REPLICAS};
use super::devices::DeviceMapping;
use super::extensions::DockerSocketMode;
use super::port::Protocol;
use super::StackFile;

/// `volume_sync.VOL.enabled` was accepted but never did anything. Replication
/// cannot be off per volume: failover needs a current replica of every mounted
/// volume, so a never-copied volume would pin the service to its node forever.
fn enabled_rejected(path: String) -> ConfigError {
    ConfigError::invalid(
        path,
        "per-volume replication cannot be turned off, so `enabled` is no longer accepted. \
         Remove it, set a longer `interval` for a volume that changes rarely, or put \
         scratch data on a drive that only one node implements (such a drive has nowhere \
         to replicate to)",
    )
}

impl StackFile {
    /// Checks that only look at the stack file itself.
    pub fn validate_syntax(&self) -> Result<()> {
        self.validate_service_names()?;
        self.validate_depends_on()?;
        self.validate_env_file()?;
        self.validate_service_networks_declared()?;
        self.validate_no_duplicate_ports()?;
        self.validate_stop_signals()?;
        self.validate_stop_grace_periods()?;
        self.validate_colocate()?;
        self.validate_sync_hooks()?;
        self.validate_writable_secrets()?;
        self.validate_single_config_writer()?;
        self.validate_replicas()?;
        self.validate_memory_sizes()?;
        self.validate_devices()?;
        self.validate_docker_sockets()?;
        self.validate_alerts()?;
        self.validate_terraform()?;
        Ok(())
    }

    /// `x-clouduler.terraform`: variable names, duplicates and file paths.
    fn validate_terraform(&self) -> Result<()> {
        for (name, svc) in &self.services {
            if let Some(tf) = svc.x_clouduler.as_ref().and_then(|x| x.terraform.as_ref()) {
                let prefix = format!("services.{name}.x-clouduler.terraform");
                tf.validate(&prefix)?;
                // A bundled `file:` object belongs to the stack and is rewritten on every deploy.
                for (i, o) in tf.outputs.iter().enumerate() {
                    let bundled = match o.target() {
                        Some(super::TfOutputTarget::Secret(n)) => self
                            .secrets
                            .get(n)
                            .is_some_and(|s| s.as_ref().and_then(|s| s.file.as_ref()).is_some())
                            .then_some(("secret", n)),
                        Some(super::TfOutputTarget::Config(n)) => self
                            .configs
                            .get(n)
                            .is_some_and(|c| c.as_ref().and_then(|c| c.file.as_ref()).is_some())
                            .then_some(("config", n)),
                        None => None,
                    };
                    if let Some((kind, n)) = bundled {
                        return Err(ConfigError::invalid(
                            format!("{prefix}.outputs[{i}].{kind}"),
                            format!("{kind} {n:?} is a bundled file of this stack, so a module output cannot overwrite it"),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// `x-clouduler.alerts`: patterns compile, `services` only at stack level
    /// and naming real services, mute keys well formed.
    fn validate_alerts(&self) -> Result<()> {
        if let Some(alerts) = self.x_clouduler.as_ref().and_then(|x| x.alerts.as_ref()) {
            alerts.validate("x-clouduler.alerts", false)?;
            for (i, rule) in alerts.logs.iter().enumerate() {
                for name in &rule.services {
                    if !self.services.contains_key(name) {
                        return Err(ConfigError::invalid(
                            format!("x-clouduler.alerts.logs[{i}].services"),
                            format!("no service named {name:?} in this stack"),
                        ));
                    }
                }
            }
        }
        for (name, svc) in &self.services {
            if let Some(alerts) = svc.x_clouduler.as_ref().and_then(|x| x.alerts.as_ref()) {
                alerts.validate(&format!("services.{name}.x-clouduler.alerts"), true)?;
            }
        }
        Ok(())
    }

    /// The clouduler socket only serves read-only image endpoints; a read-write
    /// request is refused rather than silently downgraded.
    fn validate_docker_sockets(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(socket) = svc
                .x_clouduler
                .as_ref()
                .and_then(|x| x.docker_socket.as_ref())
            else {
                continue;
            };
            if socket.mode == DockerSocketMode::Clouduler && !socket.read_only {
                return Err(ConfigError::invalid(
                    format!("services.{name}.x-clouduler.docker_socket.read_only"),
                    "mode: clouduler is read-only; set read_only: true",
                ));
            }
        }
        Ok(())
    }

    /// Replica counts are capped at `MAX_REPLICAS`: the scheduler builds a record
    /// per replica every pass, so `replicas: 4000000000` would take down the leader.
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

    /// A memory size that overflows a u64 once the unit is applied is rejected
    /// (other unparseable sizes are still ignored).
    fn validate_memory_sizes(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(resources) = svc.deploy.as_ref().and_then(|d| d.resources.as_ref()) else {
                continue;
            };
            for (kind, raw) in [
                (
                    "limits",
                    resources.limits.as_ref().and_then(|s| s.memory.as_deref()),
                ),
                (
                    "reservations",
                    resources
                        .reservations
                        .as_ref()
                        .and_then(|s| s.memory.as_deref()),
                ),
            ] {
                let Some(raw) = raw else {
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

    /// GPU device requests and `devices:` entries are well formed, and host
    /// devices stay under `/dev/`.
    fn validate_devices(&self) -> Result<()> {
        for (name, svc) in &self.services {
            for (idx, entry) in svc.devices.iter().enumerate() {
                if let Err(msg) = DeviceMapping::parse(entry) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.devices[{idx}]"),
                        msg,
                    ));
                }
            }
            for (idx, request) in svc.device_requests().iter().enumerate() {
                if let Err(msg) = request.validate() {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.deploy.resources.reservations.devices[{idx}]"),
                        msg,
                    ));
                }
            }
        }
        Ok(())
    }

    /// `write: true` feeds container edits back into the cluster object: fine for
    /// a config, never for a secret (its plaintext must not be overwritable).
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

    /// A `write: true` config is copied per instance and every copy feeds back
    /// into one object; multiple writers overwrite each other, so only a single
    /// instance may hold it writable.
    fn validate_single_config_writer(&self) -> Result<()> {
        let mut writers: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, svc) in &self.services {
            let replicas = svc.deploy.as_ref().and_then(|d| d.replicas).unwrap_or(1);
            for (idx, config) in svc.configs.iter().enumerate().filter(|(_, c)| c.write) {
                let field = format!("services.{name}.configs[{idx}]");
                if replicas > 1 {
                    return Err(ConfigError::invalid(
                        field,
                        format!(
                            "{} is writable but this service runs {replicas} replicas; \
                             only one instance may write a config",
                            config.source
                        ),
                    ));
                }
                if let Some(other) = writers.insert(config.source.as_str(), name.as_str()) {
                    if other != name {
                        return Err(ConfigError::invalid(
                            field,
                            format!(
                                "{} is already writable in service {other}; \
                                 mount it read-only here so there is a single writer",
                                config.source
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// `before`/`after` on `volume_sync` run a service's action, so they only
    /// make sense on a service and must name an action it defines.
    fn validate_sync_hooks(&self) -> Result<()> {
        if let Some(defaults) = &self.x_clouduler {
            for (volume, o) in &defaults.volume_sync {
                if o.enabled.is_some() {
                    return Err(enabled_rejected(format!(
                        "x-clouduler.volume_sync.{volume}.enabled"
                    )));
                }
                if o.before.is_some() || o.after.is_some() {
                    return Err(ConfigError::invalid(
                        format!("x-clouduler.volume_sync.{volume}"),
                        "before/after run a service's action, set them on the service",
                    ));
                }
            }
        }
        for (name, svc) in &self.services {
            let Some(ext) = &svc.x_clouduler else {
                continue;
            };
            for (volume, o) in &ext.volume_sync {
                if o.enabled.is_some() {
                    return Err(enabled_rejected(format!(
                        "services.{name}.x-clouduler.volume_sync.{volume}.enabled"
                    )));
                }
                for (key, action) in [("before", &o.before), ("after", &o.after)] {
                    let Some(action) = action else { continue };
                    if !ext.actions.contains_key(action) {
                        return Err(ConfigError::invalid(
                            format!("services.{name}.x-clouduler.volume_sync.{volume}.{key}"),
                            format!("no action named {action:?} on this service"),
                        ));
                    }
                    if ext.actions[action].standalone {
                        return Err(ConfigError::invalid(
                            format!("services.{name}.x-clouduler.volume_sync.{volume}.{key}"),
                            format!(
                                "action {action:?} is standalone, which stops the service: a sync hook runs inside the running container"
                            ),
                        ));
                    }
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

    /// A service's `stop_grace_period` overrides `failover.stop_grace`, so it
    /// gets the same bounds: non-zero and at most `MAX_STOP_GRACE`.
    fn validate_stop_grace_periods(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(grace) = svc.stop_grace_period else {
                continue;
            };
            let field = format!("services.{name}.stop_grace_period");
            if grace.is_zero() {
                return Err(ConfigError::invalid(field, "must be greater than zero"));
            }
            if grace > MAX_STOP_GRACE {
                return Err(ConfigError::invalid(
                    field,
                    format!("must be at most {MAX_STOP_GRACE:?}"),
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
        self.validate_allowed(cluster)?;
        self.validate_network_subnets(cluster)?;
        self.validate_node_pins(cluster)?;
        self.validate_placement_constraints(cluster)?;
        Ok(())
    }

    /// `validate`, plus the checks that need this stack's own name: a service
    /// may not list a container network it is the router of (the router is
    /// attached automatically), and a router service runs exactly one replica.
    pub fn validate_named(&self, name: &str, cluster: &ClusterConfig) -> Result<()> {
        self.validate(cluster)?;
        self.validate_container_router(name, cluster)
    }

    fn validate_container_router(&self, stack: &str, cluster: &ClusterConfig) -> Result<()> {
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
                let global = deploy.and_then(|d| d.mode) == Some(super::DeployMode::Global);
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
    fn validate_env_file(&self) -> Result<()> {
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
    /// naming one could never schedule.
    fn validate_networks_implemented(&self, cluster: &ClusterConfig) -> Result<()> {
        use crate::cluster::NetworkKind;

        for (name, svc) in &self.services {
            for net in &svc.networks {
                // Nat and container networks are implemented by every node; no per-node
                // address to check. A managed network is checked like any other: a
                // node must list an address on it.
                if matches!(
                    cluster.networks.get(net).map(|n| n.kind),
                    Some(NetworkKind::Nat | NetworkKind::Container)
                ) {
                    continue;
                }
                // An address-less listing of an unmanaged network routes into it but hosts
                // nothing on it; on a managed network the address is optional.
                let managed = cluster.is_managed_network(net);
                if !cluster.nodes.values().any(|n| n.hosts_on(net, managed)) {
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

    /// A volume subpath of exactly `.` binds a drive's real root into the
    /// container (see `cl_drive::registry::resolve_relative`), bypassing the
    /// scoped per-volume path. Only safe with exactly one node holding the real
    /// directory and the service pinned there; otherwise the scheduler could
    /// place it against a different copy and silently diverge.
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
                if route.cloudflare_proxy && net.kind != crate::cluster::NetworkKind::Public {
                    return Err(ConfigError::invalid(
                        format!("{at}.cloudflare_proxy"),
                        format!(
                            "cloudflare_proxy needs a public network, and {} is not one",
                            route.network
                        ),
                    ));
                }
                if route
                    .middlewares
                    .iter()
                    .any(|m| matches!(m, crate::stack::Middleware::Authenticated(_)))
                    && !cluster
                        .proxy
                        .auth
                        .as_ref()
                        .is_some_and(|a| a.covers(&route.network))
                {
                    return Err(ConfigError::invalid(
                        format!("{at}.middlewares"),
                        format!(
                            "authenticated asks for a login on network {}, but proxy.auth does not list it in networks{}",
                            route.network,
                            if cluster.proxy.auth.is_none() {
                                " (the cluster has no proxy.auth at all)"
                            } else {
                                ""
                            }
                        ),
                    ));
                }
                for m in &route.middlewares {
                    if let crate::stack::Middleware::Headers(h) = m {
                        {
                            let names = h
                                .request
                                .keys()
                                .chain(h.response.keys())
                                .chain(&h.remove_request)
                                .chain(&h.remove_response);
                            for name in names {
                                if name.is_empty()
                                    || !name.bytes().all(|b| {
                                        b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
                                    })
                                {
                                    return Err(ConfigError::invalid(
                                        format!("{at}.middlewares"),
                                        format!("headers: {name:?} is not a valid header name"),
                                    ));
                                }
                            }
                            for value in h.request.values().chain(h.response.values()) {
                                if value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
                                    return Err(ConfigError::invalid(
                                        format!("{at}.middlewares"),
                                        "headers: a header value cannot contain control characters",
                                    ));
                                }
                            }
                        }
                    }
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
                // Only a public network's router or a wireguard tunnel's far end can make a
                // raw port reachable from outside; at least one node must have it open.
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
                                crate::cluster::NodeAddress::None
                                | crate::cluster::NodeAddress::Ip(_) => false,
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

    /// A service may attach to at most one egress network: two would give it
    /// conflicting default routes. Layered on top of the other (overlay) networks.
    fn validate_service_network_kinds(&self, cluster: &ClusterConfig) -> Result<()> {
        use crate::cluster::NetworkKind;

        for (name, svc) in &self.services {
            let mut egress = None;
            for net in &svc.networks {
                // Already checked to exist by validate_networks_exist.
                let counts = match cluster.networks.get(net) {
                    Some(n) if n.kind == NetworkKind::Container => {
                        // Only a default-route (0.0.0.0/0) container network
                        // competes with an egress network for the default route.
                        n.routed_subnet().is_some_and(|s| s.prefix_len() == 0)
                    }
                    Some(n) => matches!(
                        n.kind,
                        NetworkKind::Egress | NetworkKind::Wireguard | NetworkKind::Nat
                    ),
                    None => false,
                };
                if !counts {
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

    /// The one declared network this stack's containers get a per-stack overlay
    /// on, if any: internal or non managed private wins (its domain is what
    /// containers resolve on), else public. Managed networks never get a
    /// per-stack overlay (see [`StackFile::managed_attachments`]); egress,
    /// wireguard or nat need none either.
    pub fn overlay_network<'a>(&'a self, cluster: &ClusterConfig) -> Option<&'a str> {
        use crate::cluster::NetworkKind;

        let kind_of = |n: &str| cluster.networks.get(n).map(|net| net.kind);
        self.networks
            .keys()
            .find(|n| {
                kind_of(n).is_some_and(NetworkKind::is_subnetted) && !cluster.is_managed_network(n)
            })
            .or_else(|| {
                self.networks
                    .keys()
                    .find(|n| kind_of(n) == Some(NetworkKind::Public))
            })
            .map(String::as_str)
    }

    /// The stack's managed networks (declared top-level), alphabetical.
    pub fn managed_networks<'a>(&'a self, cluster: &ClusterConfig) -> Vec<&'a str> {
        self.networks
            .keys()
            .filter(|n| cluster.is_managed_network(n))
            .map(String::as_str)
            .collect()
    }

    /// The managed networks `service` lists, alphabetical. Each one is an extra
    /// attachment of the container beside its stack overlay.
    pub fn managed_attachments<'a>(
        &'a self,
        cluster: &ClusterConfig,
        service: &str,
    ) -> Vec<&'a str> {
        let Some(svc) = self.services.get(service) else {
            return Vec::new();
        };
        let mut out: Vec<&str> = svc
            .networks
            .iter()
            .map(String::as_str)
            .filter(|n| cluster.is_managed_network(n))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The network that is the container's primary one (its resolver is that
    /// network's gateway): the stack overlay, or when the stack has none its
    /// alphabetically first managed network.
    pub fn primary_network<'a>(&'a self, cluster: &ClusterConfig) -> Option<&'a str> {
        self.overlay_network(cluster)
            .or_else(|| self.managed_networks(cluster).into_iter().next())
    }

    /// A pinned `x-clouduler.subnet` needs an overlay network, must be a real
    /// network address no smaller than a /29, and must not overlap any cluster
    /// subnet. Collisions with other stacks are checked at deploy time.
    fn validate_network_subnets(&self, cluster: &ClusterConfig) -> Result<()> {
        use crate::cluster::NetworkKind;

        let Some(subnet) = self.x_clouduler.as_ref().and_then(|x| x.subnet) else {
            return Ok(());
        };
        {
            let at = "x-clouduler.subnet";
            if self.overlay_network(cluster).is_none() {
                return Err(ConfigError::invalid(
                    at,
                    "this stack has no overlay network (it needs an internal, private or public network) to give a subnet to",
                ));
            }
            if subnet.prefix_len() > 29 || subnet.prefix_len() < 8 {
                return Err(ConfigError::invalid(
                    at,
                    format!("{subnet}: the prefix length must be between /8 and /29"),
                ));
            }
            if subnet.network() != subnet.addr() {
                return Err(ConfigError::invalid(
                    at,
                    format!(
                        "{subnet} has host bits set, did you mean {}?",
                        subnet.trunc()
                    ),
                ));
            }
            for (cname, cnet) in &cluster.networks {
                if cnet.kind == NetworkKind::Container {
                    continue;
                }
                if let Some(ipnet::IpNet::V4(other)) = cnet.subnet {
                    if other.contains(&subnet.network()) || subnet.contains(&other.network()) {
                        return Err(ConfigError::invalid(
                            at,
                            format!(
                                "{subnet} overlaps the subnet {other} of cluster network {cname}"
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// `allowed` restricts what the stack reaches through a network.
    ///
    /// * On an internal, private or public network, an entry with a port is
    ///   enforced by the firewall on the stack's own overlay, so it can only
    ///   sit on the network the containers live on. Hostname-only entries are
    ///   enforced by the proxy on the routes published on that network, and
    ///   can sit on any usable network. A wildcard cannot take a port there.
    /// * On a wan (egress, wireguard or nat) network every entry is enforced
    ///   by the firewall on the stack's own containers: hostnames are allowed
    ///   on whatever the stack's DNS lookups of them return, which needs the
    ///   stack's resolver, so the stack must also have an overlay network.
    fn validate_allowed(&self, cluster: &ClusterConfig) -> Result<()> {
        use crate::cluster::NetworkKind;

        let overlay = self.overlay_network(cluster);
        for (name, net) in &self.networks {
            let Some(allowed) = net.as_ref().and_then(|n| n.allowed.as_ref()) else {
                continue;
            };
            let at = format!("networks.{name}.allowed");
            let kind = cluster.networks.get(name).map(|n| n.kind);
            if kind == Some(NetworkKind::Container) {
                return Err(ConfigError::invalid(
                    at,
                    format!(
                        "network {name} is a container network, the router container decides what is reachable: remove its allowed list"
                    ),
                ));
            }
            if matches!(
                kind,
                Some(NetworkKind::Egress | NetworkKind::Wireguard | NetworkKind::Nat)
            ) {
                if overlay.is_none() {
                    return Err(ConfigError::invalid(
                        at,
                        format!(
                            "network {name} has an allowed list, which is applied as the stack's containers look names up through the cluster resolver; the stack needs an internal, private or public network as well"
                        ),
                    ));
                }
                continue;
            }
            if let Some(entry) = allowed.iter().find(|e| e.is_wildcard_with_port()) {
                return Err(ConfigError::invalid(
                    at,
                    format!(
                        "{entry}: a wildcard host is checked by the proxy here and cannot take a port (a port on a wildcard is only supported on egress, wireguard and nat networks)"
                    ),
                ));
            }
            if overlay != Some(name.as_str()) {
                if let Some(entry) = allowed.iter().find(|e| e.ports.is_some()) {
                    return Err(ConfigError::invalid(
                        at,
                        format!(
                            "{entry} has a port, which the firewall enforces on the network this stack's containers live on ({}); keep only hostnames here",
                            overlay.unwrap_or("none")
                        ),
                    ));
                }
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
                // A nat network needs no interface address; a pin to any node is fine.
                let is_nat = matches!(
                    cluster.networks.get(&net).map(|n| n.kind),
                    Some(crate::cluster::NetworkKind::Nat | crate::cluster::NetworkKind::Container)
                );
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

/// Nodes holding a real local directory for `drive_name`: every implementing
/// node for a replicated drive, only the `served_by` candidates for a served
/// one (others get an NFS mountpoint).
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

/// Whether `raw` is all digits (plus an optional unit letter) but overflows a
/// u64, the one `parse_memory_bytes` failure worth an error.
fn memory_overflows(raw: &str) -> bool {
    let raw = raw.trim();
    let digits = raw.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &raw[digits.len()..];
    unit.len() <= 1
        && !digits.trim().is_empty()
        && digits.trim().chars().all(|c| c.is_ascii_digit())
}

/// A stack name ends up in container names, overlay network names and the git
/// backup's `stacks/<name>.yaml`: lowercase letters, digits, `-` and `_`,
/// starting and ending alphanumeric, at most 63 bytes. `_` is allowed for
/// existing stacks; the `<stack>_<network>` ambiguity is handled by
/// `cl_sched::run::overlay_collision`.
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
    fn tf_stack(tf: &str) -> std::result::Result<super::StackFile, crate::error::ConfigError> {
        super::StackFile::parse(&format!(
            "services:\n  a:\n    image: x\n    x-clouduler:\n      terraform:\n{tf}"
        ))
    }

    #[test]
    fn terraform_bindings_are_validated() {
        tf_stack("        secrets: [{source: s, var: pw}]\n        configs: [{source: c, var: cfg}, {source: d, path: d.json}]\n        vars: {url: x}\n").unwrap();
        for bad in [
            "        vars: {\"1x\": a}\n",
            "        secrets: [{source: s, var: pw}]\n        vars: {pw: a}\n",
            "        configs: [{source: c}]\n",
            "        configs: [{source: c, path: /etc/x}]\n",
            "        configs: [{source: c, path: ../x}]\n",
            "        configs: [{source: c, path: main.tf}]\n",
            "        configs: [{source: c, path: s.tfstate}]\n",
            "        configs: [{source: c, path: a}, {source: d, path: a}]\n",
            "        configs: [{source: c, var: v}, {source: d, var: v}]\n",
        ] {
            assert!(tf_stack(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn terraform_outputs_are_validated() {
        tf_stack("        outputs:\n          - {output: client_id, config: oidc-id}\n          - {output: client_secret, secret: oidc-secret}\n").unwrap();
        for bad in [
            // not an identifier
            "        outputs: [{output: \"1x\", secret: s}]\n",
            // neither / both targets
            "        outputs: [{output: o}]\n",
            "        outputs: [{output: o, secret: s, config: c}]\n",
            // bad object names
            "        outputs: [{output: o, secret: \"a/b\"}]\n",
            "        outputs: [{output: o, config: \".hidden\"}]\n",
            // duplicate output / duplicate target
            "        outputs: [{output: o, secret: a}, {output: o, secret: b}]\n",
            "        outputs: [{output: o, secret: a}, {output: p, secret: a}]\n",
            // feedback loop with an input binding
            "        secrets: [{source: s, var: pw}]\n        outputs: [{output: o, secret: s}]\n",
            "        configs: [{source: c, var: v}]\n        outputs: [{output: o, config: c}]\n",
            "        outputs: [{output: o, other: x}]\n",
        ] {
            assert!(tf_stack(bad).is_err(), "{bad}");
        }
        // a secret and a config may share a name: they are different kinds
        tf_stack("        outputs: [{output: o, secret: a}, {output: p, config: a}]\n").unwrap();
        // an input of the other kind is not a loop
        tf_stack(
            "        secrets: [{source: s, var: pw}]\n        outputs: [{output: o, config: s}]\n",
        )
        .unwrap();
    }

    #[test]
    fn terraform_outputs_may_not_overwrite_bundled_files() {
        let spec = |extra: &str| {
            super::StackFile::parse(&format!(
                "{extra}services:\n  a:\n    image: x\n    x-clouduler:\n      terraform:\n        outputs: [{{output: o, secret: s}}, {{output: p, config: c}}]\n"
            ))
        };
        spec("").unwrap();
        // declared without a file: an external reference, allowed
        spec("secrets:\n  s:\nconfigs:\n  c:\n").unwrap();
        assert!(spec("secrets:\n  s:\n    file: ./s\n").is_err());
        assert!(spec("configs:\n  c:\n    file: ./c\n").is_err());
    }

    #[test]
    fn terraform_outputs_do_not_count_as_requirements() {
        let s = tf_stack("        outputs: [{output: o, secret: s}, {output: p, config: c}]\n")
            .unwrap();
        let r = s.requirements();
        assert!(r.secrets.is_empty() && r.configs.is_empty());
    }

    #[test]
    fn terraform_defaults_to_enabled() {
        let s = tf_stack("        vars: {a: b}\n").unwrap();
        let tf = s.services["a"]
            .x_clouduler
            .as_ref()
            .unwrap()
            .terraform
            .as_ref()
            .unwrap();
        assert!(tf.enabled);
        let s = tf_stack("        enabled: false\n").unwrap();
        assert!(
            !s.services["a"]
                .x_clouduler
                .as_ref()
                .unwrap()
                .terraform
                .as_ref()
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn clouduler_docker_socket_must_be_read_only() {
        let socket = |extra: &str| {
            format!("services:\n  scan:\n    image: x\n    x-clouduler:\n      docker_socket:\n        target: /var/run/docker.sock\n        mode: clouduler\n{extra}")
        };
        let err = super::StackFile::parse(&socket("")).unwrap_err();
        assert!(err.to_string().contains("read_only: true"), "{err}");
        super::StackFile::parse(&socket("        read_only: true\n")).unwrap();
        // The default mode keeps allowing a read-write socket.
        super::StackFile::parse(
            "services:\n  ci:\n    image: x\n    x-clouduler:\n      docker_socket:\n        target: /var/run/docker.sock\n",
        )
        .unwrap();
    }

    #[test]
    fn sync_hooks_must_name_a_service_action() {
        let ok = "services:\n  db:\n    image: x\n    x-clouduler:\n      actions:\n        Flush: { cmd: sync }\n      volume_sync:\n        data: { before: Flush }\n";
        super::StackFile::parse(ok).unwrap();
        let bad = ok.replace("before: Flush", "before: Nope");
        assert!(super::StackFile::parse(&bad).is_err());
        let stack_level = "x-clouduler:\n  volume_sync:\n    data: { after: Flush }\nservices:\n  db:\n    image: x\n";
        assert!(super::StackFile::parse(stack_level).is_err());
    }

    #[test]
    fn a_standalone_action_cannot_be_a_sync_hook() {
        let ok = "services:\n  db:\n    image: x\n    x-clouduler:\n      actions:\n        Flush: { cmd: sync, standalone: true }\n      volume_sync:\n        data: { before: Flush }\n";
        assert!(super::StackFile::parse(ok).is_err());
        super::StackFile::parse(&ok.replace(", standalone: true", "")).unwrap();
    }

    #[test]
    fn volume_sync_enabled_is_rejected_with_an_actionable_error() {
        let svc = "services:\n  db:\n    image: x\n    x-clouduler:\n      volume_sync:\n        data: { enabled: false }\n";
        let err = super::StackFile::parse(svc).unwrap_err().to_string();
        assert!(err.contains("volume_sync.data.enabled"), "{err}");
        assert!(err.contains("interval"), "{err}");
        let stack = "x-clouduler:\n  volume_sync:\n    data: { enabled: true }\nservices:\n  db:\n    image: x\n";
        assert!(super::StackFile::parse(stack).is_err());
        // The interval alone is fine.
        let ok = "services:\n  db:\n    image: x\n    x-clouduler:\n      volume_sync:\n        data: { interval: 1h }\n";
        super::StackFile::parse(ok).unwrap();
    }

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
    fn stop_grace_period_must_be_nonzero_and_bounded() {
        let with = |g: &str| {
            StackFile::parse(&format!(
                "services:\n  web:\n    image: x\n    stop_grace_period: {g}\n"
            ))
        };
        for bad in ["0s", "11m"] {
            let err = with(bad).unwrap_err();
            assert!(
                err.to_string().contains("services.web.stop_grace_period"),
                "{bad}"
            );
        }
        assert!(with("1s").is_ok());
        assert!(with("10m").is_ok());
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
    fn device_entries_and_requests_are_checked() {
        let spec = |devices: &str, reservation: &str| {
            format!(
                "services:\n  web:\n    image: x\n    devices: {devices}\n    deploy:\n      resources:\n        reservations:\n          devices:\n            - {reservation}\n"
            )
        };
        StackFile::parse(&spec(
            "[\"/dev/dri:/dev/dri\"]",
            "{capabilities: [gpu], count: 1}",
        ))
        .unwrap();
        for (devices, reservation, at) in [
            (
                "[\"/etc/passwd\"]",
                "{capabilities: [gpu]}",
                "services.web.devices[0]",
            ),
            (
                "[\"/dev/../etc\"]",
                "{capabilities: [gpu]}",
                "services.web.devices[0]",
            ),
            (
                "[]",
                "{driver: amd, capabilities: [gpu]}",
                "reservations.devices[0]",
            ),
            (
                "[]",
                "{count: 1, device_ids: ['0'], capabilities: [gpu]}",
                "reservations.devices[0]",
            ),
            ("[]", "{count: 1}", "reservations.devices[0]"),
        ] {
            let err = StackFile::parse(&spec(devices, reservation)).unwrap_err();
            assert!(err.to_string().contains(at), "{err}");
        }
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
    fn env_file_is_rejected_in_a_stack_file() {
        let raw = "services:\n  a:\n    image: x\n    env_file: .env\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("services.a.env_file"));
        assert!(err.to_string().contains("import it"));

        let raw = "services:\n  a:\n    image: x\n    env_file: [.env, .env.local]\n";
        assert!(StackFile::parse(raw).is_err());
    }

    #[test]
    fn depends_on_self_rejected() {
        let raw = "services:\n  a:\n    image: x\n    depends_on: [a]\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("services.a.depends_on"));
        assert!(err.to_string().contains("cannot depend on itself"));
    }

    #[test]
    fn depends_on_cycle_rejected() {
        let raw = "services:\n  a:\n    image: x\n    depends_on: [b]\n  b:\n    image: x\n    depends_on: [c]\n  c:\n    image: x\n    depends_on: [a]\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err
            .to_string()
            .contains("dependency cycle: a -> b -> c -> a"));
    }

    #[test]
    fn depends_on_diamond_is_not_a_cycle() {
        let raw = "services:\n  a:\n    image: x\n    depends_on: [b, c]\n  b:\n    image: x\n    depends_on: [d]\n  c:\n    image: x\n    depends_on: [d]\n  d:\n    image: x\n";
        StackFile::parse(raw).unwrap();
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

    /// Silently ignoring it would mislead about real client addresses.
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
    fn private_shortcut_false_is_accepted_on_any_route_network() {
        let cluster = ClusterConfig::parse(include_str!("../../../../examples/cluster.yaml"))
            .expect("reference cluster is valid");
        let spec = |net: &str| {
            format!(
                "networks:\n  {net}: {{}}\nservices:\n  a:\n    image: x\n    networks: [{net}]\n    x-clouduler:\n      proxy:\n        - host: a\n          network: {net}\n          port: 80\n          private_shortcut: false\n"
            )
        };
        for net in ["wan", "lan"] {
            StackFile::parse(&spec(net))
                .unwrap()
                .validate(&cluster)
                .unwrap();
        }
    }

    #[test]
    fn cloudflare_proxy_is_public_networks_only() {
        let cluster = ClusterConfig::parse(include_str!("../../../../examples/cluster.yaml"))
            .expect("reference cluster is valid");
        let spec = |net: &str| {
            format!(
                "networks:\n  {net}: {{}}\nservices:\n  a:\n    image: x\n    networks: [{net}]\n    x-clouduler:\n      proxy:\n        - host: a\n          network: {net}\n          port: 80\n          cloudflare_proxy: true\n"
            )
        };
        StackFile::parse(&spec("wan"))
            .unwrap()
            .validate(&cluster)
            .unwrap();
        let err = StackFile::parse(&spec("lan"))
            .unwrap()
            .validate(&cluster)
            .unwrap_err();
        assert!(err.to_string().contains("cloudflare_proxy"), "{err}");
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

    /// Must default off: a backend not speaking PROXY protocol sees a malformed request.
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
    fn a_login_can_only_be_asked_for_on_a_network_that_offers_it() {
        let raw = |network: &str| {
            format!(
                "networks:\n  tailscale: {{}}\nservices:\n  a:\n    image: x\n    networks: [tailscale]\n    x-clouduler:\n      proxy:\n        - host: a\n          network: {network}\n          port: 80\n          middlewares: [authenticated]\n"
            )
        };
        let with_auth = |networks: &str| {
            let yaml = CLUSTER_YAML.replace(
                "proxy:\n  enabled: true\n",
                &format!(
                    "proxy:\n  enabled: true\n  auth:\n    host: login\n    networks: {networks}\n    issuer: https://sso.example.com/\n    client_id: apps\n"
                ),
            );
            ClusterConfig::parse(&yaml).expect("cluster with proxy.auth is valid")
        };

        // No proxy.auth at all.
        let err = stack(&raw("lan")).validate(&cluster()).unwrap_err();
        assert!(err.to_string().contains("authenticated"), "{err}");
        assert!(err.to_string().contains("no proxy.auth"), "{err}");

        // Offered on lan only: wan is refused, lan is fine.
        let lan_only = with_auth("[lan]");
        let err = stack(&raw("wan")).validate(&lan_only).unwrap_err();
        assert!(err.to_string().contains("does not list it"), "{err}");
        assert!(stack(&raw("lan")).validate(&lan_only).is_ok());
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
    fn two_services_may_not_both_write_one_config() {
        let raw = "services:\n  a:\n    image: x\n    configs:\n      - source: app_conf\n        write: true\n  b:\n    image: x\n    configs:\n      - source: app_conf\n        write: true\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("services.b.configs[0]"));
        assert!(err.to_string().contains("single writer"));
    }

    #[test]
    fn one_writer_and_a_read_only_reader_is_fine() {
        let raw = "services:\n  a:\n    image: x\n    configs:\n      - source: app_conf\n        write: true\n  b:\n    image: x\n    configs:\n      - source: app_conf\n";
        StackFile::parse(raw).expect("one writer, one reader");
    }

    #[test]
    fn a_replicated_service_may_not_write_a_config() {
        let raw = "services:\n  a:\n    image: x\n    deploy:\n      replicas: 2\n    configs:\n      - source: app_conf\n        write: true\n";
        let err = StackFile::parse(raw).unwrap_err();
        assert!(err.to_string().contains("2 replicas"));
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

    fn net_stack(allowed: &str) -> StackFile {
        stack(&format!(
            "services:\n  web:\n    image: x\n    networks: [tailscale]\nnetworks:\n  tailscale:\n{allowed}"
        ))
    }

    #[test]
    fn allowed_is_absent_by_default_and_empty_means_deny_all() {
        let f = net_stack("    {}\n");
        assert_eq!(f.networks["tailscale"].as_ref().unwrap().allowed, None);
        let f = net_stack("    allowed: []\n");
        assert_eq!(
            f.networks["tailscale"].as_ref().unwrap().allowed,
            Some(vec![])
        );
        f.validate(&cluster()).unwrap();
    }

    #[test]
    fn allowed_entries_validate_on_the_overlay_network() {
        let f = net_stack(
            "    allowed:\n      - cloud.home.liteapp.fr\n      - \"service.home.liteapp.fr:22\"\n      - \"*.example.org\"\n",
        );
        f.validate(&cluster()).unwrap();
    }

    #[test]
    fn a_bad_allowed_entry_is_a_parse_error() {
        let err = StackFile::parse(
            "services:\n  web:\n    image: x\nnetworks:\n  tailscale:\n    allowed: [\"bad host\"]\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a valid hostname"), "{err}");
    }

    #[test]
    fn allowed_is_accepted_on_egress_networks_with_an_overlay() {
        let f = stack(
            "services:\n  web:\n    image: x\n    networks: [tailscale, vpn]\nnetworks:\n  tailscale: {}\n  vpn:\n    allowed: [a.example.org, \"*.b.example.org:443\", 1.2.3.4]\n",
        );
        f.validate(&cluster()).unwrap();
    }

    #[test]
    fn allowed_on_egress_needs_an_overlay_for_the_resolver() {
        let f = stack(
            "services:\n  web:\n    image: x\n    networks: [vpn]\nnetworks:\n  vpn:\n    allowed: [a.example.org]\n",
        );
        let err = f.validate(&cluster()).unwrap_err();
        assert!(err.to_string().contains("networks.vpn.allowed"), "{err}");
    }

    #[test]
    fn a_wildcard_port_is_refused_off_wan_networks() {
        let f = stack(
            "services:\n  web:\n    image: x\n    networks: [tailscale]\nnetworks:\n  tailscale:\n    allowed: [\"*.example.org:443\"]\n",
        );
        let err = f.validate(&cluster()).unwrap_err();
        assert!(err.to_string().contains("cannot take a port"), "{err}");
    }

    #[test]
    fn port_entries_are_refused_off_the_overlay_network() {
        // tailscale (internal) is the overlay; wan only carries routes.
        let f = stack(
            "services:\n  web:\n    image: x\n    networks: [tailscale, wan]\nnetworks:\n  tailscale: {}\n  wan:\n    allowed: [\"auth.example.org:22\"]\n",
        );
        let err = f.validate(&cluster()).unwrap_err();
        assert!(err.to_string().contains("networks.wan.allowed"), "{err}");
        assert!(err.to_string().contains("tailscale"), "{err}");

        // The same list with hostnames only is fine.
        let f = stack(
            "services:\n  web:\n    image: x\n    networks: [tailscale, wan]\nnetworks:\n  tailscale: {}\n  wan:\n    allowed: [auth.example.org]\n",
        );
        f.validate(&cluster()).unwrap();
    }

    #[test]
    fn the_overlay_network_prefers_subnetted_over_public() {
        let f = stack(
            "services:\n  web:\n    image: x\nnetworks:\n  wan: {}\n  tailscale: {}\n  vpn: {}\n",
        );
        assert_eq!(f.overlay_network(&cluster()), Some("tailscale"));
        let f = stack("services:\n  web:\n    image: x\nnetworks:\n  wan: {}\n  vpn: {}\n");
        assert_eq!(f.overlay_network(&cluster()), Some("wan"));
        let f = stack("services:\n  web:\n    image: x\nnetworks:\n  vpn: {}\n");
        assert_eq!(f.overlay_network(&cluster()), None);
    }

    fn managed_cluster() -> ClusterConfig {
        let mut cluster = cluster();
        for name in ["mgd2", "mgd1"] {
            cluster.networks.insert(
                name.into(),
                serde_yaml::from_str("{ type: private, subnet: 192.168.32.0/24, managed: true }")
                    .unwrap(),
            );
        }
        cluster.networks.get_mut("mgd2").unwrap().subnet = Some("192.168.33.0/24".parse().unwrap());
        let node = cluster.nodes.values_mut().next().unwrap();
        node.network.insert(
            "mgd1".into(),
            crate::cluster::NodeAddress::Ip("192.168.32.2".parse().unwrap()),
        );
        node.network.insert(
            "mgd2".into(),
            crate::cluster::NodeAddress::Ip("192.168.33.2".parse().unwrap()),
        );
        cluster
    }

    #[test]
    fn managed_networks_are_never_the_stack_overlay() {
        let cluster = managed_cluster();
        let f = stack(
            "services:\n  web:\n    image: x\n    networks: [mgd2, lan, mgd1]\nnetworks:\n  mgd1: {}\n  mgd2: {}\n  lan: {}\n",
        );
        f.validate(&cluster).unwrap();
        assert_eq!(f.overlay_network(&cluster), Some("lan"));
        assert_eq!(f.primary_network(&cluster), Some("lan"));
        assert_eq!(f.managed_attachments(&cluster, "web"), vec!["mgd1", "mgd2"]);
        assert_eq!(f.managed_networks(&cluster), vec!["mgd1", "mgd2"]);
        assert!(f.managed_attachments(&cluster, "nope").is_empty());
    }

    #[test]
    fn a_managed_only_stack_uses_its_first_managed_network_as_primary() {
        let cluster = managed_cluster();
        let f = stack(
            "services:\n  web:\n    image: x\n    networks: [mgd2, mgd1]\nnetworks:\n  mgd1: {}\n  mgd2: {}\n",
        );
        f.validate(&cluster).unwrap();
        assert_eq!(f.overlay_network(&cluster), None);
        assert_eq!(f.primary_network(&cluster), Some("mgd1"));
        // Public wins over nothing but a managed network still is not an overlay.
        let f = stack("services:\n  web:\n    image: x\nnetworks:\n  mgd1: {}\n  wan: {}\n");
        assert_eq!(f.overlay_network(&cluster), Some("wan"));
    }

    #[test]
    fn a_managed_network_needs_no_node_address_to_host_a_stack() {
        let mut cluster = managed_cluster();
        let node = cluster.nodes.values_mut().next().unwrap();
        node.network
            .insert("mgd1".into(), crate::cluster::NodeAddress::None);
        let f =
            stack("services:\n  web:\n    image: x\n    networks: [mgd1]\nnetworks:\n  mgd1: {}\n");
        f.validate(&cluster)
            .expect("an address-less listing still hosts on a managed network");
        // An unmanaged private network does not: the listing is access only.
        let node = cluster.nodes.values_mut().next().unwrap();
        node.network
            .insert("lan".into(), crate::cluster::NodeAddress::None);
        for n in cluster.nodes.values_mut().skip(1) {
            n.network.remove("lan");
        }
        let node = cluster.nodes.values().next().unwrap();
        assert!(node.hosts_on("mgd1", true));
        assert!(!node.hosts_on("lan", false));
        assert!(!node.hosts_on("absent", true));
    }

    #[test]
    fn a_managed_network_nobody_implements_is_refused() {
        let mut cluster = managed_cluster();
        for node in cluster.nodes.values_mut() {
            node.network.remove("mgd1");
        }
        let f =
            stack("services:\n  web:\n    image: x\n    networks: [mgd1]\nnetworks:\n  mgd1: {}\n");
        let err = f.validate(&cluster).unwrap_err().to_string();
        assert!(err.contains("no node implements network mgd1"), "{err}");
    }

    fn container_cluster(subnet: &str) -> ClusterConfig {
        let yaml = CLUSTER_YAML.replace(
            "  vpn:\n    type: egress\n",
            &format!(
                "  vpn:\n    type: egress\n  ctn2:\n    type: container\n    subnet: {subnet}\n    container: {{ stack: rt, service: router }}\n"
            ),
        );
        ClusterConfig::parse(&yaml).expect("patched cluster is valid")
    }

    #[test]
    fn a_container_network_needs_no_implementing_node() {
        let raw = "networks:\n  ctn2: {}\nservices:\n  a:\n    image: x\n    networks: [ctn2]\n    x-clouduler: { node: n1 }\n";
        let cluster = container_cluster("100.64.0.0/10");
        let s = stack(raw);
        s.validate(&cluster).expect("every node implements it");
        assert_eq!(
            s.container_networks("a", &cluster),
            vec!["ctn2".to_string()]
        );
        assert!(s.container_networks("missing", &cluster).is_empty());
        // Never an overlay, never a pinning egress network.
        assert_eq!(s.overlay_network(&cluster), None);
        assert_eq!(s.egress_network("a", &cluster), None);
    }

    #[test]
    fn only_a_default_route_container_network_counts_as_an_egress_network() {
        let raw = "networks:\n  vpn: {}\n  ctn2: {}\nservices:\n  a:\n    image: x\n    networks: [vpn, ctn2]\n";
        stack(raw)
            .validate(&container_cluster("100.64.0.0/10"))
            .expect("a non-default container network is not an egress network");
        let err = stack(raw)
            .validate(&container_cluster("0.0.0.0/0"))
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("only one egress network is allowed per service"));
    }

    #[test]
    fn the_router_is_attached_automatically_and_runs_one_replica() {
        let cluster = container_cluster("100.64.0.0/10");
        let own =
            "networks:\n  ctn2: {}\nservices:\n  router:\n    image: x\n    networks: [ctn2]\n";
        let err = stack(own).validate_named("rt", &cluster).unwrap_err();
        assert!(err.to_string().contains("attached to it automatically"));
        // The same service in a different stack is an ordinary client.
        stack(own).validate_named("other", &cluster).unwrap();

        let ok = "services:\n  router:\n    image: x\n";
        stack(ok).validate_named("rt", &cluster).unwrap();
        for deploy in ["replicas: 2", "mode: global"] {
            let raw = format!("services:\n  router:\n    image: x\n    deploy:\n      {deploy}\n");
            let err = stack(&raw).validate_named("rt", &cluster).unwrap_err();
            assert!(err.to_string().contains("exactly one replica"), "{deploy}");
            stack(&raw).validate_named("other", &cluster).unwrap();
        }
    }

    #[test]
    fn a_container_network_takes_no_allowed_list() {
        let raw = "networks:\n  ctn2:\n    allowed: [example.com]\nservices:\n  a:\n    image: x\n    networks: [ctn2]\n";
        let err = stack(raw)
            .validate(&container_cluster("100.64.0.0/10"))
            .unwrap_err();
        assert!(err.to_string().contains("container network"), "{err}");
    }

    #[test]
    fn a_node_pin_is_fine_on_a_default_route_container_network() {
        let raw = "networks:\n  ctn2: {}\nservices:\n  a:\n    image: x\n    networks: [ctn2]\n    x-clouduler: { node: n1 }\n";
        stack(raw)
            .validate(&container_cluster("0.0.0.0/0"))
            .expect("every node implements it");
    }

    #[test]
    fn a_pinned_subnet_is_validated() {
        let cluster = container_cluster("100.64.0.0/10");
        let ok = "x-clouduler:\n  subnet: 172.30.5.0/24\nservices:\n  a:\n    image: x\n    networks: [tailscale]\nnetworks:\n  tailscale: {}\n";
        stack(ok).validate(&cluster).expect("a free subnet is fine");
        for (subnet, want) in [
            ("172.30.5.7/24", "host bits"),
            ("172.30.5.0/30", "between /8 and /29"),
            ("10.10.0.0/25", "overlaps"),
        ] {
            let raw = ok.replace("172.30.5.0/24", subnet);
            let err = stack(&raw).validate(&cluster).unwrap_err();
            assert!(err.to_string().contains(want), "{subnet}: {err}");
        }
        let none = "x-clouduler:\n  subnet: 172.30.5.0/24\nservices:\n  a:\n    image: x\n";
        let err = stack(none).validate(&cluster).unwrap_err();
        assert!(err.to_string().contains("no overlay network"), "{err}");
    }
}
