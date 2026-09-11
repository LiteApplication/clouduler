//! Read only views of the config, used by the check and list commands.

use cl_config::{ClusterConfig, Node, NodeAddress};

pub fn summary(cfg: &ClusterConfig) {
    println!("cluster {}", cfg.cluster.name);
    println!(
        "  {} nodes, {} managers, quorum {}",
        cfg.nodes.len(),
        cfg.managers().count(),
        cfg.quorum()
    );
    for (name, net) in &cfg.networks {
        let subnet = net.subnet.map(|s| s.to_string()).unwrap_or_default();
        let domain = net.domain.clone().unwrap_or_default();
        println!("  network {name} ({}) {subnet} {domain}", net.kind);
    }
    println!("  web https://{}:{}", cfg.web_fqdn(), cfg.web.port);
}

pub fn nodes(cfg: &ClusterConfig) {
    for (name, node) in &cfg.nodes {
        let rpc = cfg
            .rpc_addr(node)
            .map(|a| a.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{name}\t{}\t{rpc}\tnets={}\tdrives={}",
            node.role,
            addresses(node),
            list(node.drives.keys().map(String::as_str))
        );
    }
}

fn addresses(node: &Node) -> String {
    let mut out = Vec::new();
    for (net, addr) in &node.network {
        let value = match addr {
            NodeAddress::Ip(ip) => ip.to_string(),
            NodeAddress::Interface(iface) => iface.interface.clone(),
            NodeAddress::Public(p) => {
                let mut parts = Vec::new();
                if let Some(v4) = p.ipv4 {
                    parts.push(v4.to_string());
                }
                if let Some(v6) = p.ipv6 {
                    parts.push(v6.to_string());
                }
                if let Some(d) = &p.domain {
                    parts.push(d.clone());
                }
                parts.join("/")
            }
        };
        out.push(format!("{net}={value}"));
    }
    list(out.iter().map(String::as_str))
}

pub fn drives(cfg: &ClusterConfig) {
    for (name, drive) in &cfg.drives {
        let on = cfg
            .nodes
            .iter()
            .filter(|(_, n)| n.implements(name))
            .map(|(n, _)| n.as_str());
        println!("{name}\tsync={:?}\tnodes={}", drive.sync_interval, list(on));
    }
}

fn list<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let joined = items.collect::<Vec<_>>().join(",");
    if joined.is_empty() {
        "-".to_string()
    } else {
        joined
    }
}
