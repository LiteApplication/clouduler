//! Cluster consensus: the replicated state, its raft storage, the node RPC
//! transport and the quorum gate the rest of the daemon consults.

pub mod ids;
pub mod types;
pub mod state;
pub mod store;

pub use ids::{node_id, InstanceId, NodeId, VolumeId};
pub use state::{
    Assignment, Availability, CertificateState, ClusterState, DesiredState, DnsRecordState,
    Migration, MigrationPhase, NodeState, Request, Response, Role, StackState, VolumeState,
};
