//! Turning init's answers into a cluster config.
//!
//! Kept free of IO and prompting so the interesting decisions, which node goes
//! where and what happens when two nodes claim one address, are testable
//! without a cluster.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use cl_config::{
    ClusterConfig, ClusterMeta, Drive, DriveMode, McpConfig, Network, NetworkKind, Node,
    NodeAddress, NodeRole, WebConfig, DEFAULT_RPC_PORT, DEFAULT_WEB_BACKEND_PORT,
};
use ipnet::IpNet;

/// Name given to the internal network of a cluster init creates. Nothing
/// depends on the name, it just has to be stable and obvious in the file.
pub const INTERNAL_NETWORK: &str = "internal";

/// What this machine becomes, in either mode.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePlan {
    pub name: String,
    /// Freshly minted every time a `NodePlan` is created, i.e. every
    /// provisioning event: forming a cluster, joining one, or rejoining
    /// under an old name with `--force`. See the doc comment on
    /// `cl_config::Node::id`.
    pub id: uuid::Uuid,
    pub role: NodeRole,
    pub ip: IpAddr,
    pub rpc_port: u16,
    /// Drives this node implements, mapped to the local directory backing them.
    pub drives: BTreeMap<String, PathBuf>,
    pub data_dir: Option<PathBuf>,
    /// Where this node's docker daemon should store images and containers.
    /// `None` leaves dockerd's own default in place.
    pub docker_data_dir: Option<PathBuf>,
    /// Whether this node may create the cluster key. A joining node must not.
    pub generate_secret_key: Option<bool>,
}

impl NodePlan {
    pub fn new(name: impl Into<String>, role: NodeRole, ip: IpAddr) -> Self {
        Self {
            name: name.into(),
            id: uuid::Uuid::new_v4(),
            role,
            ip,
            rpc_port: DEFAULT_RPC_PORT,
            drives: BTreeMap::new(),
            data_dir: None,
            docker_data_dir: None,
            generate_secret_key: None,
        }
    }

    fn to_node(&self, network: &str) -> Node {
        Node {
            id: self.id,
            role: self.role,
            network: [(network.to_string(), NodeAddress::Ip(self.ip))]
                .into_iter()
                .collect(),
            drives: self.drives.clone(),
            // init does not ask about labels; they are a day-two concern, set
            // from the config file or the ui once the cluster exists.
            labels: Default::default(),
            rpc_port: self.rpc_port,
            data_dir: self.data_dir.clone(),
            docker_data_dir: self.docker_data_dir.clone(),
            generate_secret_key: self.generate_secret_key,
            // init does not ask about this either; a day-two concern set
            // from the config file or the ui.
            last_resort: false,
        }
    }
}

/// What a brand new cluster looks like, before any other node joins it.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterPlan {
    pub cluster_name: String,
    pub subnet: IpNet,
    pub internal_domain: Option<String>,
    pub web_address: String,
    pub web_port: u16,
    /// Drives declared cluster wide. The node's own mapping lives in `node`.
    pub drives: BTreeMap<String, Drive>,
    pub node: NodePlan,
}

/// Build the config file for a cluster of one. Further nodes arrive through
/// [`add_node`], run against this config from the joining machine.
pub fn new_cluster(plan: &ClusterPlan) -> Result<ClusterConfig> {
    if plan.node.role != NodeRole::Manager {
        bail!("the first node of a cluster has to be a manager, nothing would lead it otherwise");
    }

    let network = Network {
        kind: NetworkKind::Internal,
        subnet: Some(plan.subnet),
        domain: plan.internal_domain.clone(),
    };

    let config = ClusterConfig {
        cluster: ClusterMeta {
            name: plan.cluster_name.clone(),
        },
        networks: [(INTERNAL_NETWORK.to_string(), network)]
            .into_iter()
            .collect(),
        drives: plan.drives.clone(),
        nodes: [(plan.node.name.clone(), plan.node.to_node(INTERNAL_NETWORK))]
            .into_iter()
            .collect(),
        web: WebConfig {
            address: plan.web_address.clone(),
            network: INTERNAL_NETWORK.to_string(),
            port: plan.web_port,
            // A new cluster publishes the dashboard through its own proxy, so
            // it gets a real certificate as soon as ACME is configured rather
            // than sitting on the self signed one.
            through_proxy: true,
            backend_port: DEFAULT_WEB_BACKEND_PORT,
        },
        data_dir: PathBuf::from("/var/lib/clouduler"),
        failover: Default::default(),
        secrets: Default::default(),
        auth: Default::default(),
        cloudflare: None,
        proxy: Default::default(),
        self_update: None,
        backup: None,
        // Off in a new cluster: letting an AI client run commands in containers
        // is an explicit opt-in. Written out rather than left to the serde
        // default, which stays `true` so clusters that predate the setting keep
        // the access they already had.
        mcp: McpConfig { enabled: false },
    };

    config
        .validate()
        .context("the answers do not make a valid config")?;
    Ok(config)
}

/// Add this machine to a config fetched from a node already in the cluster.
///
/// `replace` allows re-running init on a node that is already declared, which
/// is how you fix a wrong address or change a role. Without it a name clash is
/// an error, so init cannot quietly take over another machine's identity.
pub fn add_node(config: &ClusterConfig, plan: &NodePlan, replace: bool) -> Result<ClusterConfig> {
    let (internal_name, internal) = config.internal_network();
    let internal_name = internal_name.clone();

    if config.nodes.contains_key(&plan.name) && !replace {
        bail!(
            "the cluster already has a node called {}, pass --force to replace its entry",
            plan.name
        );
    }

    let subnet = internal
        .subnet
        .context("the internal network in the fetched config declares no subnet")?;
    if !subnet.contains(&plan.ip) {
        bail!(
            "{} is outside the internal subnet {subnet}, so the other nodes cannot reach this one",
            plan.ip
        );
    }

    if let Some((other, _)) = config
        .nodes
        .iter()
        .find(|(name, node)| *name != &plan.name && node.ip_on(&internal_name) == Some(plan.ip))
    {
        bail!("{} is already used by node {other}", plan.ip);
    }

    for drive in plan.drives.keys() {
        if !config.drives.contains_key(drive) {
            bail!(
                "the cluster has no drive called {drive} (it has: {}). Add it from the web ui first",
                drive_names(config)
            );
        }
    }

    let mut config = config.clone();
    config
        .nodes
        .insert(plan.name.clone(), plan.to_node(&internal_name));
    config
        .validate()
        .context("adding this node breaks the config")?;
    Ok(config)
}

fn drive_names(config: &ClusterConfig) -> String {
    if config.drives.is_empty() {
        return "none".to_string();
    }
    config.drives.keys().cloned().collect::<Vec<_>>().join(", ")
}

/// Parse a `--drive name=/path` argument.
pub fn parse_drive_arg(arg: &str) -> Result<(String, PathBuf)> {
    let Some((name, path)) = arg.split_once('=') else {
        bail!("--drive takes name=/path, got {arg}");
    };
    let (name, path) = (name.trim(), PathBuf::from(path.trim()));
    if name.is_empty() {
        bail!("--drive {arg} has an empty drive name");
    }
    if !path.is_absolute() {
        bail!("--drive {arg} needs an absolute path");
    }
    Ok((name.to_string(), path))
}

/// A drive declaration for a cluster being created. A served drive is held by
/// the only node there is, which is this one.
pub fn drive_definition(mode: DriveMode, server: &str) -> Drive {
    Drive {
        // Matches the config default. A drive created here is one someone will
        // tune from the ui later anyway.
        sync_interval: std::time::Duration::from_secs(15 * 60),
        mode,
        served_by: match mode {
            DriveMode::Served => vec![server.to_string()],
            DriveMode::Replicated => Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn plan() -> ClusterPlan {
        ClusterPlan {
            cluster_name: "home".into(),
            subnet: "10.10.0.0/24".parse().unwrap(),
            internal_domain: Some("int.example.com".into()),
            web_address: "cloud".into(),
            web_port: 8443,
            drives: BTreeMap::new(),
            node: NodePlan::new("n1", NodeRole::Manager, ip("10.10.0.1")),
        }
    }

    #[test]
    fn a_new_cluster_holds_one_manager_on_one_internal_network() {
        let config = new_cluster(&plan()).expect("valid");

        assert_eq!(config.cluster.name, "home");
        assert_eq!(config.nodes.len(), 1);
        assert_eq!(config.internal_network().0, INTERNAL_NETWORK);
        assert_eq!(
            config.internal_ip(config.node("n1").unwrap()),
            Some(ip("10.10.0.1"))
        );
        assert_eq!(config.web_fqdn(), "cloud.int.example.com");
    }

    /// The written file has to carry `enabled: false` itself: a file without
    /// the key parses as enabled, which is what keeps older clusters' access.
    #[test]
    fn a_new_cluster_starts_with_mcp_off_and_says_so_in_the_file() {
        let config = new_cluster(&plan()).expect("valid");
        assert!(!config.mcp.enabled);

        let yaml = serde_yaml::to_string(&config).unwrap();
        let reparsed: ClusterConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(!reparsed.mcp.enabled);

        let without_key: ClusterConfig =
            serde_yaml::from_str(&yaml.replace("mcp:\n  enabled: false\n", "")).unwrap();
        assert!(without_key.mcp.enabled);
    }

    #[test]
    fn a_first_node_that_is_a_worker_is_refused() {
        let mut plan = plan();
        plan.node.role = NodeRole::Worker;
        assert!(new_cluster(&plan).is_err());
    }

    #[test]
    fn a_node_address_outside_the_subnet_is_refused() {
        let mut plan = plan();
        plan.node.ip = ip("192.168.1.1");
        assert!(new_cluster(&plan).is_err());
    }

    #[test]
    fn a_bare_web_address_needs_a_domain_on_the_network() {
        let mut plan = plan();
        plan.internal_domain = None;
        assert!(new_cluster(&plan).is_err());

        // A fully qualified one stands on its own.
        plan.web_address = "cloud.example.com".into();
        assert!(new_cluster(&plan).is_ok());
    }

    #[test]
    fn a_created_drive_is_declared_and_implemented_by_the_first_node() {
        let mut plan = plan();
        plan.drives
            .insert("fast".into(), drive_definition(DriveMode::Replicated, "n1"));
        plan.node
            .drives
            .insert("fast".into(), PathBuf::from("/srv/fast"));

        let config = new_cluster(&plan).expect("valid");
        assert!(config.drives.contains_key("fast"));
        assert!(config.node("n1").unwrap().implements("fast"));
    }

    #[test]
    fn a_served_drive_names_its_holder() {
        let mut plan = plan();
        plan.drives
            .insert("bulk".into(), drive_definition(DriveMode::Served, "n1"));
        plan.node
            .drives
            .insert("bulk".into(), PathBuf::from("/srv/bulk"));

        let config = new_cluster(&plan).expect("valid");
        assert_eq!(config.drives["bulk"].served_by, vec!["n1".to_string()]);
    }

    #[test]
    fn joining_adds_the_node_and_leaves_the_others_alone() {
        let existing = new_cluster(&plan()).unwrap();
        let joiner = NodePlan::new("n2", NodeRole::Worker, ip("10.10.0.2"));

        let config = add_node(&existing, &joiner, false).expect("valid");
        assert_eq!(config.nodes.len(), 2);
        assert_eq!(config.node("n2").unwrap().role, NodeRole::Worker);
        assert_eq!(config.node("n1"), existing.node("n1"));
    }

    #[test]
    fn joining_with_an_address_another_node_already_holds_is_refused() {
        let existing = new_cluster(&plan()).unwrap();
        let joiner = NodePlan::new("n2", NodeRole::Worker, ip("10.10.0.1"));

        let error = add_node(&existing, &joiner, false).unwrap_err().to_string();
        assert!(error.contains("n1"), "{error}");
    }

    #[test]
    fn joining_with_an_address_outside_the_subnet_is_refused() {
        let existing = new_cluster(&plan()).unwrap();
        let joiner = NodePlan::new("n2", NodeRole::Worker, ip("10.99.0.2"));
        assert!(add_node(&existing, &joiner, false).is_err());
    }

    #[test]
    fn joining_under_a_name_already_in_the_cluster_needs_force() {
        let existing = new_cluster(&plan()).unwrap();
        let mut joiner = NodePlan::new("n1", NodeRole::Manager, ip("10.10.0.7"));
        joiner.rpc_port = 7000;

        assert!(add_node(&existing, &joiner, false).is_err());
        let config = add_node(&existing, &joiner, true).expect("replaced");
        assert_eq!(config.nodes.len(), 1);
        assert_eq!(config.node("n1").unwrap().rpc_port, 7000);
    }

    #[test]
    fn joining_while_claiming_a_drive_the_cluster_does_not_have_is_refused() {
        let existing = new_cluster(&plan()).unwrap();
        let mut joiner = NodePlan::new("n2", NodeRole::Worker, ip("10.10.0.2"));
        joiner
            .drives
            .insert("nope".into(), PathBuf::from("/srv/nope"));

        let error = add_node(&existing, &joiner, false).unwrap_err().to_string();
        assert!(error.contains("nope"), "{error}");
    }

    #[test]
    fn a_joining_node_can_implement_an_existing_drive() {
        let mut plan = plan();
        plan.drives
            .insert("fast".into(), drive_definition(DriveMode::Replicated, "n1"));
        plan.node
            .drives
            .insert("fast".into(), PathBuf::from("/srv/fast"));
        let existing = new_cluster(&plan).unwrap();

        let mut joiner = NodePlan::new("n2", NodeRole::Worker, ip("10.10.0.2"));
        joiner
            .drives
            .insert("fast".into(), PathBuf::from("/mnt/fast"));

        let config = add_node(&existing, &joiner, false).expect("valid");
        assert!(config.node("n2").unwrap().implements("fast"));
    }

    #[test]
    fn drive_arguments_parse_into_a_name_and_an_absolute_path() {
        assert_eq!(
            parse_drive_arg("fast=/srv/fast").unwrap(),
            ("fast".to_string(), PathBuf::from("/srv/fast"))
        );
        assert!(parse_drive_arg("fast").is_err());
        assert!(parse_drive_arg("=/srv/fast").is_err());
        assert!(parse_drive_arg("fast=srv/fast").is_err());
    }
}
