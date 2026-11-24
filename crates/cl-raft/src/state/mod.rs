//! The replicated cluster state and the commands that mutate it. `apply` is the
//! only way state changes and it never does IO, keeping the state machine
//! deterministic across nodes and its invariants unit testable.

mod alert;
mod apply;
mod backup;
mod config;
mod migration;
mod node;
mod queries;
mod reconcile;
mod request;
mod secret;
mod session;
mod stack;
mod types;
mod update;
mod volume;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::alerts::{
    AlertRecord, AlertSignal, DestinationHealth, MAX_ALERT_INBOX, MAX_ALERT_RECORDS,
    MAX_RECORD_DELIVERIES,
};
use crate::ids::{InstanceId, VolumeId};

pub use apply::Response;
pub use queries::*;
pub use request::Request;
pub use types::*;

#[cfg(test)]
mod tests;
