//! Image update and redeploy progress, written into raft step by step.
//!
//! Kept in replicated state rather than a node-local tracker so that every
//! node's api answers the same thing: the browser is usually talking to a
//! different node than the one whose docker is doing the work.

use std::sync::Arc;

use cl_raft::ids::InstanceId;
use cl_raft::node::ClusterNode;
use cl_raft::state::{Request, UpdatePhase};
use tracing::warn;

/// Submits `Request::SetInstanceUpdate` on behalf of one node. Never fails
/// the caller: progress that cannot be recorded is logged, and the update
/// itself carries on.
#[derive(Clone)]
pub struct UpdateReporter {
    cluster: Arc<ClusterNode>,
    node: String,
}

impl UpdateReporter {
    pub fn new(cluster: Arc<ClusterNode>, node: impl Into<String>) -> Self {
        Self {
            cluster,
            node: node.into(),
        }
    }

    pub async fn report(&self, instance: &InstanceId, phase: UpdatePhase, error: Option<String>) {
        submit(&self.cluster, instance, &self.node, phase, error).await;
    }
}

/// Same as `UpdateReporter::report`, for a caller recording a step on behalf
/// of the node the instance runs on (e.g. queueing it from whichever node
/// the api request landed on).
pub async fn submit(
    cluster: &ClusterNode,
    instance: &InstanceId,
    node: &str,
    phase: UpdatePhase,
    error: Option<String>,
) {
    let request = Request::SetInstanceUpdate {
        instance: instance.clone(),
        node: node.to_string(),
        phase,
        error,
        at: unix_now(),
    };
    match cluster.submit(request).await {
        Ok(response) => {
            if let Some(error) = response.error {
                warn!(%instance, ?phase, %error, "update progress was rejected");
            }
        }
        Err(e) => warn!(%instance, ?phase, error = %e, "could not record update progress"),
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
