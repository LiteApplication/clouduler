//! Cluster consensus: the replicated state, its raft storage, the node RPC
//! transport and the quorum gate the rest of the daemon consults.

pub mod backup_validate;
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

pub use client::{client_for, Client};
pub use contact::Contact;
pub use error::{ClusterError, Result};
pub use ids::{node_id, InstanceId, NodeId, VolumeId};
pub use liveness::{Fence, FenceAction, FenceState, LivenessTracker};
pub use network::{fixed_key, throwaway_key, KeyWatch};
pub use node::{ClusterNode, Options};
pub use quorum::{QuorumStatus, QuorumWatch};
pub use state::{
    base64_decode, base64_encode, dns_key, instances_using_volume, Assignment, Availability,
    CertificateState, ClusterConfigEntry, ClusterState, ConfigEntry, DesiredState, DnsRecordState,
    InstanceUpdate, Migration, MigrationPhase, NodeState, PortProtocol, ReconcileStatus, Request,
    Response, Role, SecretEntry, StackState, SyncRefusal, UpdatePhase, VolumeState,
    MAX_OBJECT_BYTES,
};
pub use store::Store;
