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
    qualify, AuthConfig, CloudflareConfig, ClusterConfig, ClusterMeta, ContainerNetwork,
    ContainerOpenPort, DnsPublishMode, Drive, DriveMode, EgressFallback, Failback, FailoverConfig,
    GitBackupConfig, ImageUpdatesConfig, InterfaceAddress, McpConfig, Network, NetworkKind, Node,
    NodeAddress, NodeRole, OidcConfig, OpenPort, PortalPalette, ProxyAuthConfig, ProxyConfig,
    PublicAddress, SecretsConfig, SelfUpdateConfig, WebConfig, DEFAULT_RPC_PORT,
    DEFAULT_STOP_GRACE, DEFAULT_WEB_BACKEND_PORT, MAX_STOP_GRACE, MIN_IMAGE_CHECK_INTERVAL,
};
pub use error::{ConfigError, Result};
pub use netalloc::{
    check_interface_name, container_mark, container_table_ids, egress_table_id, fnv1a,
    stable_hasher, subnet_octet, CONTAINER_MARK_BASE, EGRESS_POOL_BASE, EGRESS_TABLE_BASE,
    IFNAME_MAX_BYTES, NAT_POOL_BASE,
};
pub use stack::{
    mute_matches, validate_stack_name, AlertSeverity, AllowedHost, AllowedPorts, AllowedTarget,
    Authenticated, AutoUpdate, AutoUpdateMode, AutoUpdateSetting, BoundPort, ByteSize, Command,
    CorsPolicy, Deploy, DeployMode, DeployRestartPolicy, DeviceCount, DeviceMapping, DeviceRequest,
    DockerSocket, DockerSocketMode, ExposePort, GpuRequirement, Headers, Healthcheck,
    HealthcheckTest, LogRule, LogStream, Middleware, ObjectRef, Placement, PortMapping, PortRange,
    Protocol, ProxyRoute, PublishMode, RedirectRegex, Reservations, ResourceSpec, Resources,
    RestartPolicy, Service, ServiceAction, ServiceExtensions, StackAlerts, StackConfig,
    StackDefaults, StackFile, StackNetwork, StackRequirements, StackSecret, StackVolume,
    TmpfsOptions, UpdateConfig, VolumeMount, VolumeSyncOverride, DEFAULT_SYNC_INTERVAL,
    MAX_REPLICAS, MIN_VOLUME_SYNC_INTERVAL, NVIDIA_DRIVER, SECRET_DIR,
};
