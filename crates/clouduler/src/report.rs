//! Read only views of the config, used by the check and list commands.

use cl_config::{ClusterConfig, NetworkKind};

pub fn summary(cfg: &ClusterConfig) {
    let managers = cfg.managers().count();
    println!("cluster {}", cfg.cluster.name);
    println!(
        "  {} nodes, {} managers, quorum {}",
        cfg.nodes.len(),
        managers,
        managers / 2 + 1
    );
    for (name, net) in &cfg.networks {
        let kind = match net.kind {
            NetworkKind::Private => "private",
            NetworkKind::Public => "public",
        };
        let subnet = net.subnet.map(|s| s.to_string()).unwrap_or_default();
        let domain = net.domain.clone().unwrap_or_default();
        println!("  network {name} ({kind}) {subnet} {domain}");
    }
    println!("  web {} on {}", cfg.web.address, cfg.web.network);
}

pub fn nodes(cfg: &ClusterConfig) {
    for (name, node) in &cfg.nodes {
        let public = node
            .public
            .as_ref()
            .map(|p| {
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
                parts.join(",")
            })
            .unwrap_or_else(|| "-".to_string());
        let drives: Vec<&str> = node.drives.keys().map(String::as_str).collect();
        println!(
            "{name}\t{}\t{}\tpublic={public}\tdrives={}",
            node.role,
            node.rpc_addr(),
            if drives.is_empty() {
                "-".to_string()
            } else {
                drives.join(",")
            }
        );
    }
}

pub fn drives(cfg: &ClusterConfig) {
    for (name, drive) in &cfg.drives {
        let on: Vec<&str> = cfg
            .nodes
            .iter()
            .filter(|(_, n)| n.implements(name))
            .map(|(n, _)| n.as_str())
            .collect();
        println!(
            "{name}\tsync={:?}\tnodes={}",
            drive.sync_interval,
            on.join(",")
        );
    }
}
