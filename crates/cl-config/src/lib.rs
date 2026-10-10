//! Cluster configuration: the single file that describes the network topology,
//! the nodes, and the drives available cluster wide.

mod bundle;
mod cluster;
mod error;
mod netalloc;
mod stack;
mod validate;

pub use bundle::{BundledFile, StackBundle};
pub use cluster::{
    qualify, AuthConfig, CloudflareConfig, ClusterConfig, ClusterMeta, DnsPublishMode, Drive,
    DriveMode, EgressFallback, FailoverConfig, GitBackupConfig, InterfaceAddress, McpConfig,
    Network, NetworkKind, Node, NodeAddress, NodeRole, OidcConfig, OpenPort, ProxyConfig,
    PublicAddress, SecretsConfig, SelfUpdateConfig, WebConfig, DEFAULT_RPC_PORT,
    DEFAULT_STOP_GRACE, DEFAULT_WEB_BACKEND_PORT, MAX_STOP_GRACE,
};
pub use error::{ConfigError, Result};
pub use netalloc::{
    check_interface_name, egress_table_id, fnv1a, stable_hasher, subnet_octet, EGRESS_POOL_BASE,
    EGRESS_TABLE_BASE, IFNAME_MAX_BYTES, NAT_POOL_BASE,
};
pub use stack::{
    validate_stack_name, BoundPort, ByteSize, Command, CorsPolicy, Deploy, DeployMode,
    DeployRestartPolicy, DockerSocket, ExposePort, ForwardAuth, Healthcheck, HealthcheckTest,
    Middleware, ObjectRef, Placement, PortMapping, PortRange, Protocol, ProxyRoute, PublishMode,
    RedirectRegex, ResourceSpec, Resources, RestartPolicy, Service, ServiceExtensions, StackConfig,
    StackDefaults, StackFile, StackNetwork, StackRequirements, StackSecret, StackVolume,
    TmpfsOptions, UpdateConfig, VolumeMount, VolumeSyncOverride, DEFAULT_SYNC_INTERVAL,
    MAX_REPLICAS, SECRET_DIR,
};
