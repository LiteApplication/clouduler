//! Cluster configuration: the single file that describes the network topology,
//! the nodes, and the drives available cluster wide.

mod bundle;
mod cluster;
mod error;
mod stack;
mod validate;

pub use cluster::{
    qualify, AuthConfig, CloudflareConfig, ClusterConfig, DnsPublishMode, DriveMode, EgressFallback, FailoverConfig,
    GitBackupConfig, OidcConfig,
    SecretsConfig, ClusterMeta, Drive, InterfaceAddress, Network, NetworkKind, Node,
    NodeAddress, NodeRole, OpenPort, ProxyConfig, PublicAddress, SelfUpdateConfig, WebConfig,
    DEFAULT_RPC_PORT, DEFAULT_WEB_BACKEND_PORT,
};
pub use bundle::{BundledFile, StackBundle};
pub use error::{ConfigError, Result};
pub use stack::{
    ByteSize, ObjectRef, SECRET_DIR,
    Command, CorsPolicy, Deploy, DeployMode, DeployRestartPolicy, DockerSocket, ForwardAuth, Healthcheck, HealthcheckTest,
    Middleware, Placement, PortMapping, PortRange, ProxyRoute, PublishMode, RedirectRegex, ResourceSpec, Resources,
    RestartPolicy, ServiceExtensions, ExposePort, Service, StackConfig, StackDefaults, StackFile,
    StackNetwork, BoundPort, StackRequirements, StackSecret, StackVolume, Protocol, TmpfsOptions, UpdateConfig, VolumeMount,
    VolumeSyncOverride, DEFAULT_SYNC_INTERVAL,
};
