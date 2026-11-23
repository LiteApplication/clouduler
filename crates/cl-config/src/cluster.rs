use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ipnet::{IpNet, Ipv4Net};
use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::stack::Protocol;

/// Default port for the node to node RPC channel (raft, drive sync, proxy backhaul).
pub const DEFAULT_RPC_PORT: u16 = 7373;

/// Root of the cluster config file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    pub cluster: ClusterMeta,
    #[serde(deserialize_with = "deserialize_networks")]
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
    /// Git-based cluster self-update. Write-once through the web ui.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_update: Option<SelfUpdateConfig>,
    /// Mirrors cluster.yaml, stacks, configs and secrets into an admin-owned git repo.
    /// Absent means off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<GitBackupConfig>,
    /// The MCP endpoint and the OAuth server that fronts it.
    #[serde(default)]
    pub mcp: McpConfig,
    /// Registry checks for container image updates.
    #[serde(default)]
    pub updates: ImageUpdatesConfig,
    /// Per-service OpenTofu modules. Absent means the feature is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terraform: Option<TerraformConfig>,
}

pub const DEFAULT_TERRAFORM_IMAGE: &str = "ghcr.io/opentofu/opentofu:1.10";

/// How clouduler runs the OpenTofu modules stored with stacks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TerraformConfig {
    /// The OpenTofu image used for init, plan and apply.
    #[serde(default = "default_terraform_image")]
    pub image: String,
    /// Wall-clock limit for one run.
    #[serde(with = "humantime_serde", default = "default_terraform_timeout")]
    pub timeout: Duration,
    /// Docker network the `tofu init` container (provider downloads) runs on.
    #[serde(default = "default_terraform_init_network")]
    pub init_network: String,
}

fn default_terraform_image() -> String {
    DEFAULT_TERRAFORM_IMAGE.to_string()
}

fn default_terraform_timeout() -> Duration {
    Duration::from_secs(600)
}

fn default_terraform_init_network() -> String {
    "bridge".to_string()
}

impl Default for TerraformConfig {
    fn default() -> Self {
        Self {
            image: default_terraform_image(),
            timeout: default_terraform_timeout(),
            init_network: default_terraform_init_network(),
        }
    }
}

/// How often the leader checks registries for newer image digests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImageUpdatesConfig {
    /// Time between checks, raised to `MIN_IMAGE_CHECK_INTERVAL` if lower.
    #[serde(with = "humantime_serde", default = "default_image_check_interval")]
    pub check_interval: Duration,
}

/// The shortest effective `updates.check_interval`.
pub const MIN_IMAGE_CHECK_INTERVAL: Duration = Duration::from_secs(10 * 60);

impl Default for ImageUpdatesConfig {
    fn default() -> Self {
        Self {
            check_interval: default_image_check_interval(),
        }
    }
}

impl ImageUpdatesConfig {
    /// `check_interval`, raised to the minimum.
    pub fn effective_interval(&self) -> Duration {
        self.check_interval.max(MIN_IMAGE_CHECK_INTERVAL)
    }
}

fn default_image_check_interval() -> Duration {
    Duration::from_secs(6 * 60 * 60)
}

/// Whether AI clients may connect over MCP.
/// Whether AI clients may connect over MCP.
///
/// On by default; every grant still needs an admin or viewer approval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default = "default_true_bool")]
    pub enabled: bool,
}

/// Deriving this would default `enabled` to false, contradicting the serde
/// field default that existing cluster.yaml files get.
impl Default for McpConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/clouduler")
}

/// Where the key that decrypts stored secrets lives.
///
/// Secrets are encrypted before entering the replicated database; the key
/// itself sits in a file on at least one node. A node without it replicates
/// secrets but cannot start a container that uses one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    /// Path to the key file.
    #[serde(default = "default_key_file")]
    pub key_file: PathBuf,

    /// Generate a key on first launch when the file is missing. Turn off on
    /// nodes that should only receive an existing key.
    #[serde(default = "default_true_bool")]
    pub generate_if_missing: bool,

    /// Path to this node's own asymmetric keypair, generated on first launch.
    /// Its public half goes in the cluster roster so a `key_file` holder can wrap
    /// the cluster key for this node.
    #[serde(default = "default_node_key_file")]
    pub node_key_file: PathBuf,
}

fn default_key_file() -> PathBuf {
    PathBuf::from("/etc/clouduler/secrets.key")
}

fn default_node_key_file() -> PathBuf {
    PathBuf::from("/etc/clouduler/node.key")
}

fn default_true_bool() -> bool {
    true
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            key_file: default_key_file(),
            generate_if_missing: true,
            node_key_file: default_node_key_file(),
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

    /// Api keys always work as a fallback; one is generated and printed on first
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
    /// The client secret is a sealed replicated cluster secret (set from the web
    /// ui), not stored here.
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// Claim listing the user's groups or roles.
    #[serde(default = "default_admin_claim")]
    pub admin_claim: String,
    /// Value in that claim which grants admin. Without it, a successful login
    /// gets read only access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_value: Option<String>,
    /// Claim shown as the signed-in user's name. Falls back to `sub`.
    #[serde(default = "default_username_claim")]
    pub username_claim: String,
}

fn default_scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into(), "email".into()]
}

fn default_admin_claim() -> String {
    "groups".to_string()
}

fn default_username_claim() -> String {
    "preferred_username".to_string()
}

/// Timings for handling a node that drops out of the cluster.
///
/// `fence_timeout` (node stops its containers after losing quorum) must be
/// shorter than `node_timeout` (cluster reschedules them), or the same
/// container runs in two places and volumes corrupt.
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

    /// Time a container gets to stop cleanly before being killed, for services
    /// without their own `stop_grace_period`. Counts against the margin between
    /// `fence_timeout` and `node_timeout`.
    #[serde(with = "humantime_serde", default = "default_stop_grace")]
    pub stop_grace: Duration,

    /// How long a node that is rebooting or powering off may spend pushing its
    /// volumes to other nodes before it goes down. `0s` skips the push.
    #[serde(with = "humantime_serde", default = "default_shutdown_push_timeout")]
    pub shutdown_push_timeout: Duration,
}

/// Longest `failover.shutdown_push_timeout` accepted.
pub const MAX_SHUTDOWN_PUSH_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

fn default_shutdown_push_timeout() -> Duration {
    Duration::from_secs(15 * 60)
}

/// Docker's own default is 10 seconds, which is short for a database.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(30);

/// Longest `failover.stop_grace` accepted.
pub const MAX_STOP_GRACE: Duration = Duration::from_secs(600);

fn default_stop_grace() -> Duration {
    DEFAULT_STOP_GRACE
}

/// Long enough that the default `fence_timeout` (15s) plus the default
/// `stop_grace` (30s) still finish before the leader reschedules, which
/// `fence_margin_warning` requires.
fn default_node_timeout() -> Duration {
    Duration::from_secs(60)
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
            stop_grace: default_stop_grace(),
            shutdown_push_timeout: default_shutdown_push_timeout(),
        }
    }
}

impl FailoverConfig {
    /// A warning when a fence may not finish before the leader reschedules: stop
    /// takes up to the stop grace after `fence_timeout`, and must land inside
    /// `node_timeout`. Pass the longest per-service grace to
    /// `fence_margin_warning_for` to check that too.
    pub fn fence_margin_warning(&self) -> Option<String> {
        self.fence_margin_warning_for(self.stop_grace, "default stop grace")
    }

    /// As `fence_margin_warning`, for an arbitrary `stop_grace` described by
    /// `what` (e.g. a service's own `stop_grace_period`).
    pub fn fence_margin_warning_for(&self, stop_grace: Duration, what: &str) -> Option<String> {
        (self.fence_timeout + stop_grace >= self.node_timeout).then(|| {
            format!(
                "failover.fence_timeout ({:?}) plus the {what} ({:?}) reaches \
                 node_timeout ({:?}): a fence may still be stopping containers when the leader \
                 reschedules them, giving one volume two writers. Raise node_timeout or lower \
                 fence_timeout or the stop grace",
                self.fence_timeout, stop_grace, self.node_timeout
            )
        })
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
    /// Like `Egress`, but the tunnel is a WireGuard interface clouduler brings up
    /// from an imported config. The node address is the derived interface name.
    Wireguard,
    /// Like `Egress`, but outbound traffic leaves through the node's normal route
    /// (masqueraded). No node address is declared. For services that must reach
    /// addresses only the host routing table knows (a VPN peer, say).
    Nat,
    /// Traffic to the network's `subnet` is routed through a router container.
    /// No node address is declared; the subnet is the routed range.
    Container,
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
    /// Required for internal and private networks, where it must contain the
    /// node addresses, and for a container network, where it is the address
    /// space routed through the router (IPv4 only, `0.0.0.0/0` allowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnet: Option<IpNet>,
    /// Records under this domain resolve to node addresses on this network.
    /// On a container network it only qualifies the names the client DNS
    /// answers for the router side; nothing is published to a DNS provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Whether hostnames on this network are published to the DNS provider
    /// (Cloudflare). `false` keeps a private network's names out of public
    /// DNS; they are then answered only by `proxy.dns_server`. Always true
    /// for a public network.
    #[serde(default = "default_true_bool", skip_serializing_if = "is_true")]
    pub publish_dns: bool,
    /// Required for, and only allowed on, a `container` network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerNetwork>,
    /// Networks this one may reach (internal, private and container networks).
    /// Absent means no host firewall (unrestricted at L3) and only own network plus
    /// public networks at L7; `[]` or a list turns enforcement on. See
    /// [`ClusterConfig::reaches`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Vec<String>>,
    /// A cluster managed private network: clouduler builds one cluster-wide
    /// VXLAN overlay over `subnet` between the nodes that list an address on
    /// it, and allocates container addresses from it. Only valid on a
    /// `private` network with an IPv4 subnet.
    #[serde(default, skip_serializing_if = "is_false")]
    pub managed: bool,
}

/// A `container` network: the router and what its side may reach. The routed
/// range is the network's own `subnet`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContainerNetwork {
    /// Stack holding the router service.
    pub stack: String,
    /// Router service; it must run a single replica.
    pub service: String,
    /// Nodes whose host-level traffic to `subnet` goes through the router.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// Legacy: deserialize-only, never serialized. Folded into the network's
    /// `access` while parsing (see [`deserialize_networks`]) and then empty,
    /// so everything reads `access` instead.
    #[serde(default, skip_serializing)]
    pub allowed_networks: Vec<String>,
    /// Host ports the router side may reach.
    #[serde(default)]
    pub open_ports: Vec<ContainerOpenPort>,
    /// Let the router side reach the reverse proxy on 80/443.
    #[serde(default, skip_serializing_if = "is_false")]
    pub proxy: bool,
}

/// One host port opened to the router side of a container network.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContainerOpenPort {
    pub port: u16,
    #[serde(default)]
    pub protocol: Protocol,
    /// Port on the host the traffic lands on; defaults to `port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_port: Option<u16>,
}

/// A container network's block together with its routed range, which lives
/// on the owning [`Network`]. Derefs to the block.
#[derive(Debug, Clone, Copy)]
pub struct ContainerView<'a> {
    container: &'a ContainerNetwork,
    /// Address space behind the router, `0.0.0.0/0` allowed.
    pub subnet: Ipv4Net,
}

impl std::ops::Deref for ContainerView<'_> {
    type Target = ContainerNetwork;
    fn deref(&self) -> &ContainerNetwork {
        self.container
    }
}

/// Deserializes `networks`, folding the legacy `container.allowed_networks`
/// into network-level `access` (entry X of container network C is added to
/// `C.access`, deduplicated) and clearing the legacy list, so every
/// construction path is normalized.
///
/// Deliberately not folded in reverse: `X.access` is untouched.
fn deserialize_networks<'de, D>(d: D) -> std::result::Result<BTreeMap<String, Network>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut networks = BTreeMap::<String, Network>::deserialize(d)?;
    for net in networks.values_mut() {
        if let Some(c) = net.container.as_mut() {
            if c.allowed_networks.is_empty() {
                continue;
            }
            let legacy = std::mem::take(&mut c.allowed_networks);
            let access = net.access.get_or_insert_with(Vec::new);
            for x in legacy {
                if !access.contains(&x) {
                    access.push(x);
                }
            }
        }
    }
    Ok(networks)
}

/// Smallest and largest prefix a managed network's subnet may have.
pub const MANAGED_MIN_PREFIX: u8 = 16;
pub const MANAGED_MAX_PREFIX: u8 = 29;

/// The anycast gateway of a managed subnet: its first host address.
pub fn managed_gateway(subnet: Ipv4Net) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(subnet.network()).saturating_add(1))
}

impl Network {
    /// Whether clouduler manages this network's overlay (a validated managed
    /// network is always private with an IPv4 subnet).
    pub fn is_managed(&self) -> bool {
        self.managed && self.kind == NetworkKind::Private
    }

    /// The IPv4 subnet of a managed network.
    pub fn managed_subnet(&self) -> Option<Ipv4Net> {
        match (self.is_managed(), self.subnet) {
            (true, Some(IpNet::V4(v4))) => Some(v4),
            _ => None,
        }
    }

    /// The address space a container network routes through its router, when
    /// this is a container network with an IPv4 subnet.
    pub fn routed_subnet(&self) -> Option<Ipv4Net> {
        match (self.kind, self.subnet) {
            (NetworkKind::Container, Some(IpNet::V4(v4))) => Some(v4),
            _ => None,
        }
    }
}

impl ContainerOpenPort {
    pub fn effective_target_port(&self) -> u16 {
        self.target_port.unwrap_or(self.port)
    }
}

fn is_true(b: &bool) -> bool {
    *b
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

/// What a served drive does when the node it prefers comes back after a
/// failover.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Failback {
    /// Hand the drive back on its own once the preferred node has caught up.
    #[default]
    Auto,
    /// Keep serving from the standby until an operator asks for the handover.
    Manual,
}

impl Failback {
    pub fn is_default(&self) -> bool {
        *self == Failback::Auto
    }
}

/// A cluster wide directory namespace. Nodes opt in by mapping it to a local path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Drive {
    /// How often this drive syncs: each node's copy for a replicated drive, the
    /// active server's push to the `served_by` standbys for a served drive.
    #[serde(with = "humantime_serde", default = "default_sync_interval")]
    pub sync_interval: Duration,

    #[serde(default)]
    pub mode: DriveMode,

    /// Nodes that can hold the data for a served drive, in priority order. The
    /// first reachable one is active; the rest keep a synced backup. Each maps
    /// the drive to the real directory; other nodes get a mountpoint pointed at
    /// the active server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub served_by: Vec<String>,

    /// Whether a served drive returns to a preferred node (earlier in `served_by`)
    /// after a failover, once it has copied what was written meanwhile.
    #[serde(default, skip_serializing_if = "Failback::is_default")]
    pub failback: Failback,

    /// How many of this drive's volumes one node replicates at once; the rest
    /// wait, most important stack first.
    #[serde(
        default = "default_max_parallel_syncs",
        skip_serializing_if = "is_default_max_parallel_syncs"
    )]
    pub max_parallel_syncs: usize,
}

impl Drive {
    pub fn is_served(&self) -> bool {
        self.mode == DriveMode::Served
    }
}

fn default_max_parallel_syncs() -> usize {
    1
}

fn is_default_max_parallel_syncs(n: &usize) -> bool {
    *n == default_max_parallel_syncs()
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
    /// Identity for raft membership and the removed-node tombstone. Minted fresh
    /// by `clouduler init` on every provisioning (including a `--force` rejoin),
    /// so only the old id stays tombstoned and a reused name never collides.
    pub id: uuid::Uuid,
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

    /// Free-form labels, matched by `deploy.placement.constraints` as
    /// `node.labels.<key>`, so a stack can ask for a class of node instead of
    /// pinning to `node.hostname`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,

    #[serde(default = "default_rpc_port")]
    pub rpc_port: u16,
    /// Where this node keeps its database and working files. Overrides the
    /// cluster default, for nodes whose big disk is not in the usual place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,

    /// Where this node's docker daemon stores data (dockerd `data-root`); `None`
    /// leaves dockerd's default. Local only, so no cluster default. `clouduler
    /// init` writes it to `/etc/docker/daemon.json`; changing it later needs a
    /// dockerd restart and does not move existing data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_data_dir: Option<PathBuf>,

    /// Overrides `secrets.generate_if_missing` for this node.
    ///
    /// A joining node must not invent its own key (cluster secrets are sealed
    /// with the existing one). `clouduler init --join` sets this to false unless
    /// told otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_secret_key: Option<bool>,

    /// Host an instance only when no other eligible node can take it, so a
    /// large-drive but weak node is not piled with general workload.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub last_resort: bool,

    /// Whether this node times a small durable write in its data dir when the
    /// connectivity table asks. Turn off for disks that should not be touched
    /// (an SD card); the node then reports no disk figure.
    #[serde(default = "default_true_bool", skip_serializing_if = "is_true")]
    pub disk_probe: bool,
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
    Interface(InterfaceAddress),
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
            NodeAddress::Interface(addr) => Some(&addr.interface),
            NodeAddress::Ip(_) | NodeAddress::Public(_) => None,
        }
    }

    pub fn as_interface_address(&self) -> Option<&InterfaceAddress> {
        match self {
            NodeAddress::Interface(addr) => Some(addr),
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

/// A node's address on an egress or wireguard network: the local host
/// interface (a VPN tunnel, usually) a service egresses through.
/// `open_ports` names ports the tunnel's far end forwards back to this node,
/// so the scheduler only places a service exposing one where it will arrive.
///
/// Serializes as a bare string when `open_ports` is empty and `fallback` is
/// unset, so `vpn: wg0` round trips unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceAddress {
    pub interface: String,
    pub open_ports: BTreeSet<OpenPort>,
    /// Egress networks only: a second interface to route out of while
    /// `health_check_ip` stops answering pings sent out `interface`. Re-checked
    /// every reconcile tick, so it switches back when the primary recovers. See
    /// `docs/networking.md`.
    pub fallback: Option<EgressFallback>,
}

impl InterfaceAddress {
    pub fn new(interface: impl Into<String>) -> Self {
        Self {
            interface: interface.into(),
            open_ports: BTreeSet::new(),
            fallback: None,
        }
    }
}

/// A fallback interface for an `egress` network's node address, and the
/// address used to tell whether the primary interface still has a route to
/// the internet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EgressFallback {
    pub interface: String,
    pub health_check_ip: IpAddr,
}

impl Serialize for InterfaceAddress {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if self.open_ports.is_empty() && self.fallback.is_none() {
            serializer.serialize_str(&self.interface)
        } else {
            #[derive(Serialize)]
            struct Full<'a> {
                interface: &'a str,
                open_ports: &'a BTreeSet<OpenPort>,
                #[serde(skip_serializing_if = "Option::is_none")]
                fallback: &'a Option<EgressFallback>,
            }
            Full {
                interface: &self.interface,
                open_ports: &self.open_ports,
                fallback: &self.fallback,
            }
            .serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for InterfaceAddress {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Full {
            interface: String,
            #[serde(default)]
            open_ports: BTreeSet<OpenPort>,
            #[serde(default)]
            fallback: Option<EgressFallback>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bare(String),
            Full(Full),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Bare(interface) => InterfaceAddress {
                interface,
                open_ports: BTreeSet::new(),
                fallback: None,
            },
            Raw::Full(full) => InterfaceAddress {
                interface: full.interface,
                open_ports: full.open_ports,
                fallback: full.fallback,
            },
        })
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
    /// Ports this node's router forwards to it. A raw `x-clouduler.expose` port
    /// on this network is only placed on a node that lists it here.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub open_ports: BTreeSet<OpenPort>,
}

impl PublicAddress {
    pub fn is_empty(&self) -> bool {
        self.ipv4.is_none() && self.ipv6.is_none() && self.domain.is_none()
    }
}

/// One port a node's public router forwards to it, e.g. `25/tcp` for SMTP.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct OpenPort {
    pub port: u16,
    #[serde(default)]
    pub protocol: Protocol,
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

    /// Publish the dashboard as a route on the reverse proxy (standard HTTPS
    /// port, same certificate and DNS record as other services).
    ///
    /// The direct listener on `port` stays up either way, as the way back in when
    /// the proxy cannot bind or a config edit breaks routing.
    #[serde(default = "default_true_bool")]
    pub through_proxy: bool,

    /// Loopback port the proxy forwards dashboard traffic to.
    ///
    /// The proxy speaks plain HTTP to backends, so the api opens a second
    /// listener, bound to 127.0.0.1 only: the overlay is unencrypted and the
    /// dashboard carries session cookies. Each node's proxy reaches its own api.
    #[serde(default = "default_web_backend_port")]
    pub backend_port: u16,
}

/// Not 443: the reverse proxy owns the standard ports.
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
/// Both listeners bind on every node; a request landing anywhere is forwarded
/// to wherever the container runs. `dns_publish` only decides what the public
/// DNS answer says.
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
    /// How a proxied hostname's DNS record is published.
    #[serde(default)]
    pub dns_publish: DnsPublishMode,
    /// The client-facing DNS server, answering per the network the client is on.
    #[serde(default)]
    pub dns_server: DnsServerConfig,
    /// Login for routes carrying the `authenticated` middleware. A separate OIDC
    /// client (and group namespace) from `auth.oidc`, which is for operators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProxyAuthConfig>,
}

/// The login portal behind the `authenticated` route middleware.
///
/// The portal is a hostname the proxy answers itself, one per network in
/// `networks`. Logins are per network, and a route may only ask for a login
/// on a listed network. An app without a valid cookie redirects to the
/// portal, which runs the OIDC login and sends the browser back through a
/// one-time-token URL that sets the app's cookie. Sessions are replicated in
/// raft, so any node accepts any login.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProxyAuthConfig {
    /// Portal hostname. A bare label gets each network's domain appended.
    pub host: String,
    /// Networks that offer the login. A route with the `authenticated`
    /// middleware must be published on one of these.
    pub networks: Vec<String>,
    pub issuer: String,
    pub client_id: String,
    /// The client secret is a sealed cluster secret named
    /// `_clouduler_proxy_oidc_client_secret`, never part of this file.
    #[serde(default = "default_proxy_auth_scopes")]
    pub scopes: Vec<String>,
    /// Claim listing the user's groups. Routes restrict access by naming
    /// groups from here.
    #[serde(default = "default_admin_claim")]
    pub groups_claim: String,
    /// Claim shown as the user's name. Falls back to `sub`.
    #[serde(default = "default_username_claim")]
    pub username_claim: String,
    /// How long a login lasts, for the portal and every app cookie it hands out.
    /// An app cookie never outlives the portal session.
    #[serde(with = "humantime_serde", default = "default_session_ttl")]
    pub session_ttl: Duration,
    /// Heading of the login page.
    #[serde(default = "default_portal_title")]
    pub title: String,
    /// Colour set of the login page's shapes and background.
    #[serde(default)]
    pub palette: PortalPalette,
    /// Overrides the palette's main colour (buttons and the biggest shape),
    /// as a CSS hex colour like `#e5603c`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// Networks (subset of `networks`) whose login page offers "remember this IP
    /// address": a visitor inside that network may skip login until
    /// `remember_ip_ttl` passes. Empty means none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remember_ip: Vec<String>,
    /// How long a remembered address stays trusted, from the login that saved it.
    #[serde(with = "humantime_serde", default = "default_remember_ip_ttl")]
    pub remember_ip_ttl: Duration,
    /// Publish the portal hostname's DNS records through Cloudflare's proxy
    /// (orange cloud) on the public networks in `networks`; non-public ones
    /// are unaffected. Needs at least one public network listed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cloudflare_proxy: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn default_remember_ip_ttl() -> Duration {
    Duration::from_secs(30 * 24 * 3600)
}

/// Colour sets for the login page: a dark background with a few loud shapes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PortalPalette {
    /// Deep grape with pink and lime.
    #[default]
    Grape,
    /// Warm night with orange and sky blue.
    Sunset,
    /// Deep sea with cyan and coral.
    Ocean,
    /// Pine green with yellow and pink.
    Forest,
}

fn default_proxy_auth_scopes() -> Vec<String> {
    vec![
        "openid".into(),
        "profile".into(),
        "email".into(),
        "groups".into(),
    ]
}

fn default_portal_title() -> String {
    "Sign in to continue".to_string()
}

impl ProxyAuthConfig {
    /// The portal's fully qualified, lowercase hostname on `network`.
    pub fn fqdn(&self, cfg: &ClusterConfig, network: &str) -> String {
        qualify(
            &self.host,
            cfg.networks.get(network).and_then(|n| n.domain.as_deref()),
        )
    }

    /// Every portal hostname with the network it serves.
    pub fn portals(&self, cfg: &ClusterConfig) -> Vec<(String, String)> {
        self.networks
            .iter()
            .map(|n| (n.clone(), self.fqdn(cfg, n)))
            .collect()
    }

    /// Does this network offer the login?
    pub fn covers(&self, network: &str) -> bool {
        self.networks.iter().any(|n| n == network)
    }

    /// Does `network` let visitors save their IP address at login?
    pub fn remembers_ip(&self, network: &str) -> bool {
        self.covers(network) && self.remember_ip.iter().any(|n| n == network)
    }
}

impl ClusterConfig {
    /// The network `ip` is a client of: the subnetted network whose subnet
    /// holds it, else the first public network for a globally routable
    /// address. `None` for anything else (loopback, an unknown private range).
    pub fn client_network(&self, ip: std::net::IpAddr) -> Option<&str> {
        let subnetted = self
            .networks
            .iter()
            .find(|(name, net)| net.kind.is_subnetted() && self.ip_in_network(name, ip));
        subnetted
            .or_else(|| {
                self.networks.iter().find(|(name, net)| {
                    net.kind == NetworkKind::Public && self.ip_in_network(name, ip)
                })
            })
            .map(|(name, _)| name.as_str())
            .or_else(|| self.container_client_network(ip))
    }

    /// The container network whose routed range holds `ip`, for one that
    /// lets its router side reach the reverse proxy: those peers are served
    /// like a private network's clients. A default-route network
    /// (`0.0.0.0/0`) claims no client, since it would claim everyone.
    fn container_client_network(&self, ip: std::net::IpAddr) -> Option<&str> {
        let std::net::IpAddr::V4(ip) = ip.to_canonical() else {
            return None;
        };
        self.container_networks()
            .find(|(_, c)| c.proxy && c.subnet.prefix_len() > 0 && c.subnet.contains(&ip))
            .map(|(name, _)| name.as_str())
    }

    /// The container networks the client DNS server answers for, and so
    /// listens for on each node's segment address: those that let the router
    /// side reach the proxy, when both the proxy and the DNS server are on
    /// (and, if `proxy.dns_server.networks` is set, listed there).
    pub fn container_dns_networks(&self) -> impl Iterator<Item = (&String, ContainerView<'_>)> {
        let dns = &self.proxy.dns_server;
        let on = self.proxy.enabled && dns.enabled;
        self.container_networks().filter(move |(name, c)| {
            on && c.proxy && (dns.networks.is_empty() || dns.networks.contains(name))
        })
    }

    /// Is `ip` an address of `network`'s own clients? A subnetted network
    /// owns the addresses inside its subnet; a public network owns every
    /// globally routable address, i.e. one that is not private, loopback,
    /// link-local, CGNAT or inside any configured subnet. Other kinds have no
    /// clients of their own.
    pub fn ip_in_network(&self, network: &str, ip: std::net::IpAddr) -> bool {
        let ip = ip.to_canonical();
        let Some(net) = self.networks.get(network) else {
            return false;
        };
        match net.kind {
            NetworkKind::Internal | NetworkKind::Private => {
                net.subnet.is_some_and(|s| s.contains(&ip))
            }
            NetworkKind::Public => {
                !self
                    .networks
                    .values()
                    .filter(|n| n.kind.is_subnetted())
                    .filter_map(|n| n.subnet)
                    .any(|s| s.contains(&ip))
                    && is_globally_routable(ip)
            }
            _ => false,
        }
    }
}

fn is_globally_routable(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (o[0] == 100 && (64..128).contains(&o[1])))
        }
        std::net::IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            http_port: default_http_port(),
            https_port: default_https_port(),
            acme: AcmeConfig::default(),
            dns_publish: DnsPublishMode::default(),
            dns_server: DnsServerConfig::default(),
            auth: None,
        }
    }
}

/// A DNS server on the nodes' own addresses that answers cluster hostnames by
/// the network the client is on. Hostnames on a network the client cannot
/// reach are refused; other names are forwarded upstream for clients inside a
/// subnetted network and refused otherwise.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DnsServerConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Networks to listen on, at this node's address on each. Empty means every
    /// subnetted network the node has an address on plus every container network
    /// with `proxy: true`; a public network is only served when listed.
    #[serde(default)]
    pub networks: Vec<String>,
    #[serde(default = "default_dns_port")]
    pub port: u16,
    /// Upstream resolvers, tried in order on port 53. Empty means the host's
    /// (`/etc/resolv.conf`) minus Tailscale MagicDNS (split DNS can loop back to
    /// this server), or `1.1.1.1`/`1.0.0.1` when MagicDNS is all there is.
    #[serde(default)]
    pub upstream: Vec<std::net::IpAddr>,
}

fn default_dns_port() -> u16 {
    53
}

impl Default for DnsServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            networks: Vec::new(),
            port: default_dns_port(),
            upstream: Vec::new(),
        }
    }
}

/// Who a proxied hostname's DNS record names.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DnsPublishMode {
    /// Every node's address is published for every proxied hostname (like
    /// Swarm's routing mesh). Safe default; needs no placement co-location.
    #[default]
    AllNodes,
    /// Only the node(s) running the containers behind a hostname are published;
    /// stale records elsewhere are deleted. Requires every container publishing
    /// the same hostname to land on one node (an implicit scheduler constraint).
    OwningNode,
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
    /// Off by default: issuing against production Let's Encrypt from an
    /// unreachable cluster burns rate limit.
    #[serde(default)]
    pub enabled: bool,
    /// ACME directory. Left out for Let's Encrypt production.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_url: Option<String>,
    /// Contact address registered with the CA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_email: Option<String>,
}

/// Where the cluster downloads its own prebuilt binary to roll out updates.
/// A GitHub api token (private repo / rate limits) lives sealed in the cluster
/// secrets, not here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SelfUpdateConfig {
    /// Either a repo url (`https://github.com/OWNER/REPO`), which tracks
    /// that repo's latest release, or a specific release's url
    /// (`https://github.com/OWNER/REPO/releases/tag/TAG`), which pins it.
    pub release_url: String,
    /// Whether the leader periodically looks up the latest release.
    #[serde(default = "default_true_bool")]
    pub check_updates: bool,
    /// How often that lookup runs.
    #[serde(with = "humantime_serde", default = "default_update_check_interval")]
    pub check_interval: Duration,
}

fn default_update_check_interval() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}

/// Where and how the cluster mirrors its state into git.
///
/// clouduler generates its own ssh keypair; the admin adds its public half as
/// a deploy key (`GET /api/git-backup/status`). Secrets are re-encrypted for
/// `age_recipient`, so the private key can live outside the cluster.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GitBackupConfig {
    /// SSH url, e.g. `git@github.com:org/repo.git`. Only ssh push (deploy
    /// key) auth is implemented, so http(s) urls are rejected.
    pub repo_url: String,
    /// An `age1...` X25519 recipient public key. The matching private key is
    /// never given to clouduler.
    pub age_recipient: String,
    pub branch: String,
}

fn default_backup_branch() -> String {
    "main".to_string()
}

// Hand-rolled rather than `deny_unknown_fields`: `interval` (removed once
// exports started pushing on every change) may still be on disk in older
// cluster.yaml files. Accept and discard it so they keep loading.
impl<'de> Deserialize<'de> for GitBackupConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            repo_url: String,
            age_recipient: String,
            #[serde(default = "default_backup_branch")]
            branch: String,
            #[serde(default, rename = "interval")]
            _interval: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(GitBackupConfig {
            repo_url: raw.repo_url,
            age_recipient: raw.age_recipient,
            branch: raw.branch,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CloudflareConfig {
    /// Zones clouduler may write records into. The api token is a sealed cluster
    /// secret (set from the web ui), not stored here.
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
        let cfg: ClusterConfig =
            serde_yaml::from_str(raw).map_err(|source| ConfigError::Parse {
                path: PathBuf::from("<config>"),
                source,
            })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn node(&self, name: &str) -> Option<&Node> {
        self.nodes.get(name)
    }

    /// True when `node` has a public network address block with an `ipv6`,
    /// i.e. clients are given an AAAA record that points at it.
    pub fn node_has_public_v6(&self, node: &str) -> bool {
        self.node(node).is_some_and(|n| {
            n.network
                .values()
                .filter_map(|a| a.as_public())
                .any(|p| p.ipv6.is_some())
        })
    }

    /// The `access` list of network `name`; empty when absent or unknown.
    pub fn access_of(&self, name: &str) -> &[String] {
        self.networks
            .get(name)
            .and_then(|n| n.access.as_deref())
            .unwrap_or(&[])
    }

    /// Whether traffic from network `from` may reach network `to`: always for
    /// itself, always for a public `to`, otherwise when `from`'s `access`
    /// lists `to`. An absent `access` counts as empty here.
    pub fn reaches(&self, from: &str, to: &str) -> bool {
        from == to
            || self
                .networks
                .get(to)
                .is_some_and(|n| n.kind == NetworkKind::Public)
            || self.access_of(from).iter().any(|n| n == to)
    }

    /// Names of every `Egress`, `Wireguard` or `Nat` network, the kinds a
    /// service's `networks:` entry can join for outbound NAT.
    pub fn egress_networks(&self) -> std::collections::BTreeSet<String> {
        self.networks
            .iter()
            .filter(|(_, net)| {
                matches!(
                    net.kind,
                    NetworkKind::Egress | NetworkKind::Wireguard | NetworkKind::Nat
                )
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Every `container` network with its config and routed range. One whose
    /// block or IPv4 subnet is missing is skipped; validation rejects it.
    pub fn container_networks(&self) -> impl Iterator<Item = (&String, ContainerView<'_>)> {
        self.networks.iter().filter_map(|(name, net)| {
            let container = net.container.as_ref()?;
            let subnet = net.routed_subnet()?;
            Some((name, ContainerView { container, subnet }))
        })
    }

    /// The container network whose router is `stack`/`service`, if any.
    pub fn container_router_of(&self, stack: &str, service: &str) -> Option<&str> {
        self.container_networks()
            .find(|(_, c)| c.stack == stack && c.service == service)
            .map(|(name, _)| name.as_str())
    }

    /// Names of every network all nodes implement without per-node setup (`Nat`
    /// and `Container`); they take no entry under `nodes.<name>.network`.
    /// Placement treats membership as an automatic pass in
    /// `cl_sched::place::rejection`, like `served_drives`.
    pub fn everywhere_networks(&self) -> std::collections::BTreeSet<String> {
        self.networks
            .iter()
            .filter(|(_, net)| matches!(net.kind, NetworkKind::Nat | NetworkKind::Container))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Every managed network with its IPv4 subnet, in name order.
    pub fn managed_networks(&self) -> Vec<(&str, &Network, Ipv4Net)> {
        self.networks
            .iter()
            .filter_map(|(name, net)| Some((name.as_str(), net, net.managed_subnet()?)))
            .collect()
    }

    /// Whether `network` is a managed network.
    pub fn is_managed_network(&self, network: &str) -> bool {
        self.networks.get(network).is_some_and(Network::is_managed)
    }

    /// The addresses of managed network `network` no instance may take: the
    /// network and broadcast addresses, the gateway and every node address.
    pub fn managed_reserved(&self, network: &str) -> BTreeSet<Ipv4Addr> {
        let mut out = BTreeSet::new();
        let Some(subnet) = self.networks.get(network).and_then(Network::managed_subnet) else {
            return out;
        };
        out.insert(subnet.network());
        out.insert(subnet.broadcast());
        out.insert(managed_gateway(subnet));
        for node in self.nodes.values() {
            if let Some(IpAddr::V4(ip)) = node.ip_on(network) {
                out.insert(ip);
            }
        }
        out
    }

    /// The lowest host address of managed network `network` that is not the
    /// network, broadcast or gateway address nor held by any node, for
    /// autofilling a new node's address. `None` for a non managed network or
    /// a full subnet.
    pub fn next_free_node_address(&self, network: &str) -> Option<Ipv4Addr> {
        let subnet = self.networks.get(network)?.managed_subnet()?;
        let used = self.managed_reserved(network);
        subnet.hosts().find(|ip| !used.contains(ip))
    }

    /// Names of every public or wireguard network, whose outside reachability
    /// depends on a specific node, so a raw `x-clouduler.expose` port only
    /// schedules onto a node that has it open.
    pub fn port_gated_networks(&self) -> std::collections::BTreeSet<String> {
        self.networks
            .iter()
            .filter(|(_, net)| matches!(net.kind, NetworkKind::Public | NetworkKind::Wireguard))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Names of every drive whose mode is `Served`, so placement and mount
    /// resolution need not load the whole `Drive` record.
    pub fn served_drives(&self) -> std::collections::BTreeSet<String> {
        self.drives
            .iter()
            .filter(|(_, drive)| drive.is_served())
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn managers(&self) -> impl Iterator<Item = (&String, &Node)> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.role == NodeRole::Manager)
    }

    /// Number of managers that must be reachable to hold quorum.
    pub fn quorum(&self) -> usize {
        self.managers().count() / 2 + 1
    }

    /// The single internal network; validation rejects a config without one.
    pub fn internal_network(&self) -> Option<(&String, &Network)> {
        self.networks
            .iter()
            .find(|(_, n)| n.kind == NetworkKind::Internal)
    }

    /// The node address on the internal network, used for all node to node RPC.
    pub fn internal_ip(&self, node: &Node) -> Option<IpAddr> {
        node.network.get(self.internal_network()?.0)?.as_ip()
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

/// Append `domain` to a bare label. A name containing a dot is taken as full.
/// `@` names the apex (the domain alone).
///
/// A trailing dot is stripped and the name used verbatim regardless of
/// `domain`, to name a domain outside any network here. Callers needing to
/// detect that (ACME HTTP-01 fallback) check `name.ends_with('.')` on the raw
/// input; this only normalizes.
pub fn qualify(name: &str, domain: Option<&str>) -> String {
    // DNS names are case-insensitive and resolvers/Cloudflare hand them back
    // lowercased, so normalize here to keep every comparison downstream exact.
    let qualified = if let Some(stripped) = name.strip_suffix('.') {
        stripped.to_string()
    } else {
        match domain {
            Some(domain) if name == "@" => domain.to_string(),
            Some(domain) if !name.contains('.') => format!("{name}.{domain}"),
            _ => name.to_string(),
        }
    };
    qualified.to_ascii_lowercase()
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
            NetworkKind::Wireguard => f.write_str("wireguard"),
            NetworkKind::Nat => f.write_str("nat"),
            NetworkKind::Container => f.write_str("container"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_dot_is_stripped_and_used_verbatim() {
        assert_eq!(
            qualify("app.example.net.", Some("cluster.lan")),
            "app.example.net"
        );
    }

    #[test]
    fn a_trailing_dot_wins_over_a_bare_label() {
        assert_eq!(qualify("blog.", Some("cluster.lan")), "blog");
        assert_eq!(qualify("Blog", Some("Cluster.LAN")), "blog.cluster.lan");
        assert_eq!(qualify("App.Example.NET.", None), "app.example.net");
    }

    #[test]
    fn mcp_is_on_unless_a_config_turns_it_off() {
        assert!(McpConfig::default().enabled);
        assert!(serde_yaml::from_str::<McpConfig>("{}").unwrap().enabled);
        let off: McpConfig = serde_yaml::from_str("enabled: false").unwrap();
        assert!(!off.enabled);
        let round: McpConfig = serde_yaml::from_str(&serde_yaml::to_string(&off).unwrap()).unwrap();
        assert_eq!(round, off);
    }

    #[test]
    fn at_sign_names_the_apex_of_the_domain() {
        assert_eq!(qualify("@", Some("Cluster.lan")), "cluster.lan");
    }

    #[test]
    fn without_a_trailing_dot_a_bare_label_still_gets_the_domain() {
        assert_eq!(qualify("blog", Some("cluster.lan")), "blog.cluster.lan");
    }
}
