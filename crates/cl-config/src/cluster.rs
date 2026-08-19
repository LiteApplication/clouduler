use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::stack::{ByteSize, Middleware};

/// Default port for the node to node RPC channel (raft, drive sync, proxy backhaul).
pub const DEFAULT_RPC_PORT: u16 = 7373;

/// Root of the cluster config file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    pub cluster: ClusterMeta,
    pub networks: BTreeMap<String, Network>,
    #[serde(default)]
    pub drives: BTreeMap<String, Drive>,
    pub nodes: BTreeMap<String, Node>,
    pub web: WebConfig,
    /// Cluster wide default for where nodes keep their data. Individual nodes
    /// can override it.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub failover: FailoverConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare: Option<CloudflareConfig>,
    #[serde(default)]
    pub proxy: ProxyConfig,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/clouduler")
}

/// Where the key that decrypts stored secrets lives.
///
/// Secrets are encrypted before they enter the database, so the replicated
/// state and its snapshots are safe to copy around. The key itself is not in
/// the database: it sits in a file on at least one node. A node without the key
/// replicates secrets fine but cannot start a container that uses one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    /// Path to the key file. Nodes that have it can decrypt; nodes that do not
    /// simply cannot run containers needing secrets.
    #[serde(default = "default_key_file")]
    pub key_file: PathBuf,

    /// Generate a key on first launch when the file is missing. Turn this off
    /// on nodes that should only ever receive an existing key, so a typo in the
    /// path cannot quietly create a second, useless key.
    #[serde(default = "default_true_bool")]
    pub generate_if_missing: bool,
}

fn default_key_file() -> PathBuf {
    PathBuf::from("/etc/clouduler/secrets.key")
}

fn default_true_bool() -> bool {
    true
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            key_file: default_key_file(),
            generate_if_missing: true,
        }
    }
}

/// How people authenticate to the web ui and the api.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// OIDC is the recommended way in. Without it, api keys are the only option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<OidcConfig>,

    /// Api keys always work as a fallback, so a broken identity provider cannot
    /// lock you out of your own cluster. One is generated and printed on first
    /// launch. Set to false only once OIDC is known to work.
    #[serde(default = "default_true_bool")]
    pub api_keys: bool,

    /// How long a ui session lasts.
    #[serde(with = "humantime_serde", default = "default_session_ttl")]
    pub session_ttl: Duration,
}

fn default_session_ttl() -> Duration {
    Duration::from_secs(12 * 3600)
}

/// Deriving this would leave api keys off, which the validator rejects. The
/// default has to match what the serde field defaults say.
impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            oidc: None,
            api_keys: true,
            session_ttl: default_session_ttl(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    /// Issuer URL. Discovery happens at `<issuer>/.well-known/openid-configuration`.
    pub issuer: String,
    pub client_id: String,
    /// Kept out of the config file so the config stays safe to commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_file: Option<PathBuf>,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// Claim listing the user's groups or roles.
    #[serde(default = "default_admin_claim")]
    pub admin_claim: String,
    /// Value in that claim which grants admin. Without it, a successful login
    /// gets read only access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_value: Option<String>,
}

fn default_scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into(), "email".into()]
}

fn default_admin_claim() -> String {
    "groups".to_string()
}

/// Timings for handling a node that drops out of the cluster.
///
/// The two timeouts work against each other and must not be set carelessly.
/// `fence_timeout` is how long a node keeps running containers after losing
/// quorum; `node_timeout` is how long the rest of the cluster waits before
/// giving that node's containers to someone else. Fencing has to happen first,
/// or the same container runs in two places at once, which for anything with a
/// volume means corruption.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FailoverConfig {
    /// How long a node may be unreachable before its containers are
    /// rescheduled elsewhere.
    #[serde(with = "humantime_serde", default = "default_node_timeout")]
    pub node_timeout: Duration,

    /// How long a node may run without a manager quorum before it stops its own
    /// containers. Must be shorter than `node_timeout`.
    #[serde(with = "humantime_serde", default = "default_fence_timeout")]
    pub fence_timeout: Duration,

    /// How often the leader probes the other nodes.
    #[serde(with = "humantime_serde", default = "default_probe_interval")]
    pub probe_interval: Duration,
}

fn default_node_timeout() -> Duration {
    Duration::from_secs(30)
}

fn default_fence_timeout() -> Duration {
    Duration::from_secs(15)
}

fn default_probe_interval() -> Duration {
    Duration::from_secs(5)
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            node_timeout: default_node_timeout(),
            fence_timeout: default_fence_timeout(),
            probe_interval: default_probe_interval(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClusterMeta {
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkKind {
    /// The one network every node talks to every other node on. Carries raft,
    /// drive sync and the container overlay. Trusted, so unencrypted.
    Internal,
    /// A further non public subnet a node may sit on. Records for its domain
    /// resolve to the node address on that network.
    Private,
    /// Reachable from the internet. Records resolve to the node public address.
    Public,
    /// Not an overlay: a node's address on this network names a local host
    /// interface (a VPN tunnel, usually) that a service attached to it
    /// egresses through, instead of the node's normal route out.
    Egress,
}

impl NetworkKind {
    /// Internal and private networks address nodes by IP inside a subnet.
    pub fn is_subnetted(self) -> bool {
        matches!(self, NetworkKind::Internal | NetworkKind::Private)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Network {
    #[serde(rename = "type")]
    pub kind: NetworkKind,
    /// Required for internal and private networks, must contain the node addresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnet: Option<IpNet>,
    /// Records under this domain resolve to node addresses on this network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

/// How a drive's data is made available to the nodes using it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DriveMode {
    /// Every implementing node keeps its own copy, kept in step by periodic
    /// delta sync. Containers read local disk, so this survives a node losing
    /// the network, at the cost of the copies lagging each other.
    #[default]
    Replicated,
    /// One node holds the data and serves it to the others, who mount it over
    /// the internal network. One copy, always consistent, but a container using
    /// it stops working when the serving node goes away.
    Served,
}

/// A cluster wide directory namespace. Nodes opt in by mapping it to a local path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Drive {
    /// How often volumes on this drive replicate to the other implementing
    /// nodes. Ignored for a served drive, which has only one copy.
    #[serde(with = "humantime_serde", default = "default_sync_interval")]
    pub sync_interval: Duration,

    #[serde(default)]
    pub mode: DriveMode,

    /// Node holding the data for a served drive. The others mount it from
    /// there; the path they map the drive to is the mountpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_by: Option<String>,
}

impl Drive {
    pub fn is_served(&self) -> bool {
        self.mode == DriveMode::Served
    }
}

fn default_sync_interval() -> Duration {
    Duration::from_secs(15 * 60)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    /// Votes in the raft quorum, can be elected leader.
    Manager,
    /// Runs containers, follows the quorum.
    Worker,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub role: NodeRole,
    /// Networks this node is attached to, keyed by network name. Subnetted
    /// networks take a bare IP, public networks take an address block.
    #[serde(default)]
    pub network: BTreeMap<String, NodeAddress>,
    /// Drives this node implements, mapped to the local directory backing them.
    /// For a served drive, this is the real directory on the serving node and
    /// the mountpoint everywhere else.
    #[serde(default)]
    pub drives: BTreeMap<String, PathBuf>,

    /// Free-form labels this node carries, matched by
    /// `deploy.placement.constraints` as `node.labels.<key>`.
    ///
    /// A label says what a node *is* — it has fast storage, it can send mail,
    /// its disk is external and replicated — so a stack can ask for a class of
    /// node instead of naming one. Pinning to `node.hostname` is the blunt
    /// alternative and it makes every stack file a record of today's topology.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,

    #[serde(default = "default_rpc_port")]
    pub rpc_port: u16,
    /// Where this node keeps its database and working files. Overrides the
    /// cluster default, for nodes whose big disk is not in the usual place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,

    /// Overrides `secrets.generate_if_missing` for this node.
    ///
    /// A node joining an existing cluster must not invent its own key: the
    /// cluster's secrets are sealed with the one that already exists, and a
    /// second key only produces a node that thinks it can open them. `clouduler
    /// init --join` sets this to false unless you say the node should hold the
    /// key, and then you copy the real one in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_secret_key: Option<bool>,
}

fn default_rpc_port() -> u16 {
    DEFAULT_RPC_PORT
}

/// A node address on one network. Bare IP on subnetted networks, an address
/// block on public ones, a host interface name on egress networks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum NodeAddress {
    Ip(IpAddr),
    // Tried after `Ip`, so a bare IP string still parses as one; only a
    // non-IP bare string falls through to here.
    Interface(String),
    Public(PublicAddress),
}

impl NodeAddress {
    pub fn as_ip(&self) -> Option<IpAddr> {
        match self {
            NodeAddress::Ip(ip) => Some(*ip),
            NodeAddress::Interface(_) | NodeAddress::Public(_) => None,
        }
    }

    pub fn as_interface(&self) -> Option<&str> {
        match self {
            NodeAddress::Interface(name) => Some(name),
            NodeAddress::Ip(_) | NodeAddress::Public(_) => None,
        }
    }

    pub fn as_public(&self) -> Option<&PublicAddress> {
        match self {
            NodeAddress::Public(p) => Some(p),
            NodeAddress::Ip(_) | NodeAddress::Interface(_) => None,
        }
    }
}

/// How the node is reached on a public network. At least one field must be set.
/// ipv4 publishes an A record, ipv6 an AAAA, domain a CNAME.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicAddress {
    #[serde(default)]
    pub ipv4: Option<Ipv4Addr>,
    #[serde(default)]
    pub ipv6: Option<Ipv6Addr>,
    #[serde(default)]
    pub domain: Option<String>,
}

impl PublicAddress {
    pub fn is_empty(&self) -> bool {
        self.ipv4.is_none() && self.ipv6.is_none() && self.domain.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Hostname the UI is served on, always over HTTPS. A bare label gets the
    /// network domain appended.
    pub address: String,
    /// Network the hostname resolves through, picking which node address the
    /// record points at.
    pub network: String,
    #[serde(default = "default_web_port")]
    pub port: u16,

    /// Publish the dashboard as a route on the reverse proxy, so it is reached
    /// on the standard HTTPS port with the same Let's Encrypt certificate and
    /// Cloudflare record as any other service.
    ///
    /// The direct listener on `port` stays up either way. That is deliberate:
    /// it is the way back in when the proxy cannot bind its ports or a config
    /// edit breaks routing, and locking the operator out of the ui that fixes
    /// the proxy would be the worst possible failure mode.
    #[serde(default = "default_true_bool")]
    pub through_proxy: bool,

    /// Loopback port the proxy forwards dashboard traffic to.
    ///
    /// The proxy speaks plain HTTP to its backends, so the api opens a second
    /// listener for it. Bound to 127.0.0.1 only, never the overlay: the
    /// dashboard carries session cookies and the overlay is not encrypted.
    /// Each node's proxy therefore reaches its own api, which is what we want
    /// anyway since a non-leader already redirects to the leader.
    #[serde(default = "default_web_backend_port")]
    pub backend_port: u16,
}

/// Not 443: the reverse proxy owns the standard ports, since those are where
/// the cluster's own services are published. The admin ui takes the odd one.
fn default_web_port() -> u16 {
    8443
}

/// Loopback port the api opens for the reverse proxy.
pub const DEFAULT_WEB_BACKEND_PORT: u16 = 8444;

fn default_web_backend_port() -> u16 {
    DEFAULT_WEB_BACKEND_PORT
}

/// The reverse proxy in front of every `x-clouduler.proxy` route.
///
/// Both listeners are bound on every node, on all addresses, because a route's
/// hostname resolves to whichever node is publishing it and that changes as
/// containers move.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    #[serde(default = "default_true_bool")]
    pub enabled: bool,
    /// Plain HTTP. Serves `tls: false` routes and redirects the rest.
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// TLS. Must differ from `web.port`, which the api holds on the same
    /// addresses; the https listener is skipped when they collide.
    #[serde(default = "default_https_port")]
    pub https_port: u16,
    #[serde(default)]
    pub acme: AcmeConfig,
    /// Routes published straight from a fixed address rather than a stack's
    /// containers, for a service clouduler does not run: another box on the
    /// network, a third party endpoint.
    #[serde(default)]
    pub static_routes: Vec<StaticRoute>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            http_port: default_http_port(),
            https_port: default_https_port(),
            acme: AcmeConfig::default(),
            static_routes: Vec::new(),
        }
    }
}

/// A proxy route whose backend is a fixed address instead of a stack's
/// containers. Resolved by DNS on every reconcile pass, so a hostname backend
/// picks up a changed address without the config being touched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StaticRoute {
    /// Bare label gets the network's domain appended, same as a stack route.
    pub host: String,
    /// Declared network this route is published on.
    pub network: String,
    /// `host:port` of the backend. A hostname is resolved fresh on every
    /// reconcile pass; every address it resolves to becomes a backend, round
    /// robined like a multi replica service.
    pub backend: String,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default = "default_true_bool")]
    pub tls: bool,
    #[serde(default)]
    pub middlewares: Vec<Middleware>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_size: Option<ByteSize>,
    #[serde(default = "default_true_bool")]
    pub websockets: bool,
    #[serde(default)]
    pub preserve_host: bool,
    #[serde(default, with = "humantime_serde::option")]
    pub response_timeout: Option<Duration>,
}

fn default_http_port() -> u16 {
    80
}

fn default_https_port() -> u16 {
    443
}

/// Certificate issuance for proxy routes. Needs `cloudflare`, since the only
/// challenge implemented is DNS-01 against Cloudflare.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct AcmeConfig {
    /// Off by default: issuing against the real Let's Encrypt from a cluster
    /// that is not reachable yet burns rate limit for nothing.
    #[serde(default)]
    pub enabled: bool,
    /// ACME directory. Left out for Let's Encrypt production; set it to a
    /// staging or Pebble directory for testing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_url: Option<String>,
    /// Contact address registered with the CA, which uses it for expiry
    /// warnings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CloudflareConfig {
    /// Path to a file holding the API token. Left out when the token is set
    /// from the web ui instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token_file: Option<PathBuf>,
    /// Zones clouduler is allowed to write records into.
    pub zones: Vec<String>,
}

impl ClusterConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&raw).map_err(|e| match e {
            ConfigError::Parse { source, .. } => ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            },
            other => other,
        })
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let cfg: ClusterConfig = serde_yaml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<config>"),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn node(&self, name: &str) -> Option<&Node> {
        self.nodes.get(name)
    }

    pub fn managers(&self) -> impl Iterator<Item = (&String, &Node)> {
        self.nodes.iter().filter(|(_, n)| n.role == NodeRole::Manager)
    }

    /// Number of managers that must be reachable to hold quorum.
    pub fn quorum(&self) -> usize {
        self.managers().count() / 2 + 1
    }

    /// The single internal network. Validation guarantees exactly one exists.
    pub fn internal_network(&self) -> (&String, &Network) {
        self.networks
            .iter()
            .find(|(_, n)| n.kind == NetworkKind::Internal)
            .expect("validated config has exactly one internal network")
    }

    /// The node address on the internal network, used for all node to node RPC.
    pub fn internal_ip(&self, node: &Node) -> Option<IpAddr> {
        node.network.get(self.internal_network().0)?.as_ip()
    }

    /// Address other nodes use to reach this one.
    pub fn rpc_addr(&self, node: &Node) -> Option<SocketAddr> {
        Some(SocketAddr::new(self.internal_ip(node)?, node.rpc_port))
    }

    /// Nodes implementing every one of the given drives.
    pub fn nodes_implementing<'a>(
        &'a self,
        drives: &'a [String],
    ) -> impl Iterator<Item = (&'a String, &'a Node)> {
        self.nodes
            .iter()
            .filter(move |(_, n)| drives.iter().all(|d| n.drives.contains_key(d)))
    }

    /// The UI hostname with the network domain appended when it is a bare label.
    pub fn web_fqdn(&self) -> String {
        qualify(
            &self.web.address,
            self.networks
                .get(&self.web.network)
                .and_then(|n| n.domain.as_deref()),
        )
    }
}

impl Node {
    pub fn implements(&self, drive: &str) -> bool {
        self.drives.contains_key(drive)
    }

    /// Where this node keeps its database and working files.
    pub fn data_dir<'a>(&'a self, cluster: &'a ClusterConfig) -> &'a Path {
        self.data_dir.as_deref().unwrap_or(&cluster.data_dir)
    }

    /// Whether this node may create the cluster key when its file is missing.
    pub fn generates_secret_key(&self, cluster: &ClusterConfig) -> bool {
        self.generate_secret_key
            .unwrap_or(cluster.secrets.generate_if_missing)
    }

    /// The IP this node holds on a subnetted network.
    pub fn ip_on(&self, network: &str) -> Option<IpAddr> {
        self.network.get(network)?.as_ip()
    }

    /// The address block this node holds on a public network.
    pub fn public_on(&self, network: &str) -> Option<&PublicAddress> {
        self.network.get(network)?.as_public()
    }
}

/// Append `domain` to a bare label. Anything already containing a dot is taken
/// as a full name.
pub fn qualify(name: &str, domain: Option<&str>) -> String {
    match domain {
        Some(domain) if !name.contains('.') => format!("{name}.{domain}"),
        _ => name.to_string(),
    }
}

impl fmt::Display for NodeRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeRole::Manager => f.write_str("manager"),
            NodeRole::Worker => f.write_str("worker"),
        }
    }
}

impl fmt::Display for NetworkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkKind::Internal => f.write_str("internal"),
            NetworkKind::Private => f.write_str("private"),
            NetworkKind::Public => f.write_str("public"),
            NetworkKind::Egress => f.write_str("egress"),
        }
    }
}
