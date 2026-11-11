//! Host L3 firewall for `Network.access`: derives the `CLDR-NET` / `CLDR-NETF`
//! plan from the live cluster config and applies it on this node.
//!
//! Every ten seconds the live config is re-read (see
//! [`crate::daemon::live_config`]) and the plan re-applied, so an edited
//! `access` list takes effect without a restart and a docker restart that
//! rewrites `FORWARD` is repaired by the jump check. An empty plan removes
//! both chains.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cl_config::{ClusterConfig, NetworkKind};
use cl_net::iptables::{apply_plan, list_chains, remove_chain};
use cl_net::netaccess::{
    net_access_plan, Denied, NetAccessInput, NetAccessPlan, SourceRules, FORWARD_CHAIN, INPUT_CHAIN,
};
use cl_raft::node::ClusterNode;
use ipnet::{IpNet, Ipv4Net};
use tracing::{info, warn};

const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

fn subnetted_v4(cfg: &ClusterConfig, name: &str) -> Option<Ipv4Net> {
    let net = cfg.networks.get(name)?;
    if !matches!(net.kind, NetworkKind::Internal | NetworkKind::Private) {
        return None;
    }
    match net.subnet? {
        IpNet::V4(subnet) => Some(subnet),
        // iptables only; IPv6 subnets are not enforced.
        IpNet::V6(_) => None,
    }
}

/// What this node must enforce: every subnetted source with an `access` list,
/// against each subnetted network it may not reach.
pub fn input_for(cfg: &ClusterConfig, node: &str) -> NetAccessInput {
    let me = cfg.nodes.get(node);
    let mut sources = Vec::new();
    for (name, net) in &cfg.networks {
        if net.access.is_none() {
            continue;
        }
        let Some(subnet) = subnetted_v4(cfg, name) else {
            continue;
        };
        let denied = cfg
            .networks
            .keys()
            .filter(|b| *b != name && !cfg.reaches(name, b))
            .filter_map(|b| {
                let target = subnetted_v4(cfg, b)?;
                let node_addr = me.and_then(|n| match n.ip_on(b) {
                    Some(std::net::IpAddr::V4(ip)) => Some(ip),
                    _ => None,
                });
                Some(Denied {
                    network: b.clone(),
                    subnet: target,
                    node_addr,
                })
            })
            .collect::<Vec<_>>();
        if !denied.is_empty() {
            sources.push(SourceRules {
                network: name.clone(),
                subnet,
                denied,
            });
        }
    }
    NetAccessInput {
        web_port: cfg.web.port,
        sources,
    }
}

fn apply(plan: &NetAccessPlan) -> Result<()> {
    if plan.plan.chains.is_empty() {
        let existing = list_chains("filter")?;
        for chain in [INPUT_CHAIN, FORWARD_CHAIN] {
            if existing.iter().any(|c| c == chain) {
                remove_chain("filter", chain)?;
                info!(chain, "removed the network access chain");
            }
        }
        return Ok(());
    }
    apply_plan(&plan.plan)?;
    Ok(())
}

pub fn start(
    cfg: &ClusterConfig,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
) -> tokio::task::JoinHandle<()> {
    let cluster = cluster.clone();
    let node_name = node_name.to_string();
    let boot_cfg = cfg.clone();

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
        let mut last_warnings: Vec<String> = Vec::new();
        loop {
            ticker.tick().await;
            let state = cluster.state().await;
            let cfg = crate::daemon::live_config(&state, &boot_cfg);
            let plan = net_access_plan(&input_for(&cfg, &node_name));
            if plan.warnings != last_warnings {
                for w in &plan.warnings {
                    warn!("{w}");
                }
                last_warnings = plan.warnings.clone();
            }
            let result = tokio::task::spawn_blocking(move || apply(&plan))
                .await
                .context("network access task panicked")
                .and_then(|r| r);
            if let Err(e) = result {
                warn!(error = %e, "cannot apply the network access firewall");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ClusterConfig {
        ClusterConfig::parse(include_str!("../../../examples/cluster.yaml")).unwrap()
    }

    #[test]
    fn netaccess_nothing_restricted_is_empty() {
        let mut c = cfg();
        for n in c.networks.values_mut() {
            n.access = None;
        }
        assert!(input_for(&c, "n1").sources.is_empty());
    }

    #[test]
    fn netaccess_denied_targets_carry_this_nodes_address() {
        let mut c = cfg();
        for n in c.networks.values_mut() {
            n.access = None;
        }
        let internal = c.internal_network().0.clone();
        let other = c
            .networks
            .iter()
            .find(|(n, net)| {
                **n != internal && matches!(net.kind, NetworkKind::Private) && net.subnet.is_some()
            })
            .map(|(n, _)| n.clone())
            .expect("example has a private network");
        c.networks.get_mut(&other).unwrap().access = Some(vec![]);
        let input = input_for(&c, "n1");
        assert_eq!(input.sources.len(), 1);
        assert_eq!(input.sources[0].network, other);
        let d = input.sources[0]
            .denied
            .iter()
            .find(|d| d.network == internal)
            .unwrap();
        assert!(d.node_addr.is_some());
    }
}
