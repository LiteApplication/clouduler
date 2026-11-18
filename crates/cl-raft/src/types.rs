//! Raft type wiring, kept here so openraft generics do not leak.

// The macro's default snapshot type expands to a bare `Cursor`.
use std::io::Cursor;

use crate::state::{Request, Response};

openraft::declare_raft_types!(
    pub TypeConfig:
        D = Request,
        R = Response,
);

pub type Node = openraft::BasicNode;
pub type Entry = openraft::Entry<TypeConfig>;
pub type LogId = openraft::LogId<crate::ids::NodeId>;
pub type Vote = openraft::Vote<crate::ids::NodeId>;
pub type StoredMembership = openraft::StoredMembership<crate::ids::NodeId, Node>;
pub type SnapshotMeta = openraft::SnapshotMeta<crate::ids::NodeId, Node>;
pub type Snapshot = openraft::Snapshot<TypeConfig>;
pub type StorageError = openraft::StorageError<crate::ids::NodeId>;
pub type StorageIOError = openraft::StorageIOError<crate::ids::NodeId>;
pub type Raft = openraft::Raft<TypeConfig>;

pub type SnapshotData = Cursor<Vec<u8>>;
