//! Cluster consensus: the replicated state, its raft storage, the node RPC
//! transport and the quorum gate the rest of the daemon consults.

pub mod contact;
pub mod error;
pub mod ids;
pub mod liveness;
pub mod network;
pub mod node;
pub mod quorum;
pub mod rpc;
pub mod server;
pub mod state;
pub mod store;
pub mod types;

pub use contact::Contact;
pub use error::{ClusterError, Result};
pub use ids::{node_id, InstanceId, NodeId, VolumeId};
pub use liveness::{Fence, FenceAction, LivenessTracker};
pub use node::{ClusterNode, Options};
pub use quorum::{QuorumStatus, QuorumWatch};
pub use state::{
    Assignment, Availability, CertificateState, ClusterState, DesiredState, DnsRecordState,
    Migration, MigrationPhase, NodeState, Request, Response, Role, StackState, VolumeState,
};
pub use store::Store;
