//! Cluster configuration: the single file that describes the network topology,
//! the nodes, and the drives available cluster wide.

mod cluster;
mod error;
mod stack;
mod validate;

pub use cluster::{
    qualify, CloudflareConfig, ClusterConfig, ClusterMeta, Drive, Network, NetworkKind, Node,
    NodeAddress, NodeRole, PublicAddress, WebConfig, DEFAULT_RPC_PORT,
};
pub use error::{ConfigError, Result};
pub use stack::{
    Command, Deploy, DeployMode, DeployRestartPolicy, Healthcheck, HealthcheckTest, Middleware,
    Placement, PortMapping, PortRange, ProxyRoute, ResourceSpec, Resources, RestartPolicy,
    ServiceExtensions, ExposePort, Service, StackConfig, StackDefaults, StackFile, StackNetwork,
    StackSecret, StackVolume, Protocol, UpdateConfig, VolumeMount, VolumeSyncOverride,
    DEFAULT_SYNC_INTERVAL,
};
