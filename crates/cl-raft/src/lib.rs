//! Cluster consensus: the replicated state, its raft storage, the node RPC
//! transport and the quorum gate the rest of the daemon consults.

pub mod client;
pub mod contact;
pub mod error;
pub mod ids;
pub mod liveness;
mod migrations;
pub mod network;
pub mod node;
pub mod quorum;
pub mod rpc;
pub mod server;
pub mod state;
pub mod store;
pub mod types;

pub use client::Client;
pub use contact::Contact;
pub use error::{ClusterError, Result};
pub use ids::{node_id, InstanceId, NodeId, VolumeId};
pub use liveness::{Fence, FenceAction, LivenessTracker};
pub use node::{ClusterNode, Options};
pub use quorum::{QuorumStatus, QuorumWatch};
pub use state::{
    base64_decode, base64_encode, dns_key, Assignment, Availability, CertificateState,
    ClusterConfigEntry, ClusterState, ConfigEntry, DesiredState, DnsRecordState, Migration,
    MigrationPhase, NodeState, PortProtocol, ReconcileStatus, Request, Response, Role, SecretEntry,
    StackState, VolumeState, MAX_OBJECT_BYTES,
};
pub use store::Store;
