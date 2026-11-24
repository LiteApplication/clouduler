use crate::cluster::{ClusterConfig, NetworkKind, NodeAddress, OpenPort};
use crate::error::{ConfigError, Result};
use crate::stack::{ExposePort, Headers, Middleware, ProxyRoute, StackFile};

impl StackFile {
    pub(super) fn validate_networks_exist(&self, cluster: &ClusterConfig) -> Result<()> {
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
    pub(super) fn validate_networks_implemented(&self, cluster: &ClusterConfig) -> Result<()> {
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

    /// A service may attach to at most one egress network: two would give it
    /// conflicting default routes. Layered on top of the other (overlay) networks.
    pub(super) fn validate_service_network_kinds(&self, cluster: &ClusterConfig) -> Result<()> {
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
    pub(super) fn validate_network_subnets(&self, cluster: &ClusterConfig) -> Result<()> {
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
    pub(super) fn validate_allowed(&self, cluster: &ClusterConfig) -> Result<()> {
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

    pub(super) fn validate_proxy_expose_networks(&self, cluster: &ClusterConfig) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(ext) = &svc.x_clouduler else {
                continue;
            };
            for (idx, route) in ext.proxy.iter().enumerate() {
                let at = format!("services.{name}.x-clouduler.proxy[{idx}]");
                check_proxy_route(cluster, &at, route)?;
            }
            for (idx, expose) in ext.expose.iter().enumerate() {
                let at = format!("services.{name}.x-clouduler.expose[{idx}]");
                check_expose(cluster, &at, expose)?;
            }
        }
        Ok(())
    }
}

fn check_proxy_route(cluster: &ClusterConfig, at: &str, route: &ProxyRoute) -> Result<()> {
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
    if route.cloudflare_proxy && net.kind != NetworkKind::Public {
        return Err(ConfigError::invalid(
            format!("{at}.cloudflare_proxy"),
            format!(
                "cloudflare_proxy needs a public network, and {} is not one",
                route.network
            ),
        ));
    }
    check_route_login(cluster, at, route)?;
    for m in &route.middlewares {
        if let Middleware::Headers(h) = m {
            check_headers(at, h)?;
        }
    }
    for cors in route.middlewares.iter().filter_map(|m| match m {
        Middleware::Cors(c) => Some(c),
        _ => None,
    }) {
        if cors.allow_credentials && cors.origins.iter().any(|o| o == "*") {
            return Err(ConfigError::invalid(
                format!("{at}.middlewares"),
                "cors: allow_credentials cannot be combined with a wildcard origin, browsers reject the pair",
            ));
        }
    }
    Ok(())
}

fn check_route_login(cluster: &ClusterConfig, at: &str, route: &ProxyRoute) -> Result<()> {
    if route
        .middlewares
        .iter()
        .any(|m| matches!(m, Middleware::Authenticated(_)))
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
    Ok(())
}

fn check_headers(at: &str, h: &Headers) -> Result<()> {
    let names = h
        .request
        .keys()
        .chain(h.response.keys())
        .chain(&h.remove_request)
        .chain(&h.remove_response);
    for name in names {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
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
    Ok(())
}

fn check_expose(cluster: &ClusterConfig, at: &str, expose: &ExposePort) -> Result<()> {
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
    if matches!(net.kind, NetworkKind::Public | NetworkKind::Wireguard) {
        let wanted = OpenPort {
            port: expose.published,
            protocol: expose.protocol,
        };
        let reachable = cluster.nodes.values().any(|n| {
            n.network
                .get(&expose.network)
                .is_some_and(|addr| match addr {
                    NodeAddress::Public(p) => p.open_ports.contains(&wanted),
                    NodeAddress::Interface(i) => i.open_ports.contains(&wanted),
                    NodeAddress::None | NodeAddress::Ip(_) => false,
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
    Ok(())
}
