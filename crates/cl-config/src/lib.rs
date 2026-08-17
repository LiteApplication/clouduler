//! Cluster configuration: the single file that describes the network topology,
//! the nodes, and the drives available cluster wide.

mod cluster;
mod error;
mod validate;

pub use cluster::{
    qualify, CloudflareConfig, ClusterConfig, ClusterMeta, Drive, Network, NetworkKind, Node,
    NodeAddress, NodeRole, PublicAddress, WebConfig, DEFAULT_RPC_PORT,
};
pub use error::{ConfigError, Result};
