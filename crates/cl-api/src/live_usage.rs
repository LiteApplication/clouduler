//! Live resource readings for `POST /api/rebalance` and `clouduler rebalance`.
//!
//! Raft holds no live usage, so a rebalance asks each reachable node what it is using right now
//! (host memory and CPU, plus one docker stats reading per container of its instances) and hands
//! the result to `cl_sched::rebalance::plan`. Every failure here only means "no reading".

use std::collections::BTreeMap;
use std::time::Duration;

use cl_raft::ids::InstanceId;
use cl_sched::rebalance::{LiveUsage, Usage};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::internal_peer;
use crate::server::AppState;

/// One docker stats reading; a container that cannot answer in this long has no reading.
const CONTAINER_TIMEOUT: Duration = Duration::from_secs(4);

/// Per peer, retries included. Longer than `CONTAINER_TIMEOUT` so the peer can answer.
const PEER_TIMEOUT: Duration = Duration::from_secs(8);

pub const NODE_LIVE_USAGE_PATH: &str = "/api/instances/internal/live-usage";

/// What one node reports about itself.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct NodeLiveUsage {
    pub node: Usage,
    /// Instances assigned to the node whose container answered; the rest are left out.
    pub instances: BTreeMap<InstanceId, Usage>,
}

/// This node's own reading: host usage and one stats reading per container of its instances.
pub async fn local(state: &AppState) -> NodeLiveUsage {
    let cluster_state = state.cluster.snapshot().await;
    let host = crate::routes::nodes::local_resource_usage(&Default::default());
    let reads = cluster_state
        .assignments
        .iter()
        .filter(|(_, a)| a.node == state.node_name)
        .map(|(id, _)| async move {
            let read = tokio::time::timeout(
                CONTAINER_TIMEOUT,
                crate::docker_stats::read_once(id.container_name()),
            )
            .await;
            match read {
                Ok(Ok(sample)) => Some((
                    id.clone(),
                    Usage {
                        memory_used_bytes: sample.memory_used_bytes,
                        cpu_percent: sample.host_cpu_percent,
                    },
                )),
                Ok(Err(e)) => {
                    debug!(instance = %id, error = %e, "no live stats for instance");
                    None
                }
                Err(_) => {
                    debug!(instance = %id, "live stats for instance timed out");
                    None
                }
            }
        });
    NodeLiveUsage {
        node: Usage {
            memory_used_bytes: host.memory_used_bytes,
            cpu_percent: f64::from(host.cpu_percent),
        },
        instances: futures::future::join_all(reads)
            .await
            .into_iter()
            .flatten()
            .collect(),
    }
}

/// Readings from every reachable node, asked in parallel. A node that is unreachable, slow or
/// answers badly simply has no entry, so planning falls back to declared limits for it.
pub async fn gather(state: &AppState) -> LiveUsage {
    let cluster_state = state.cluster.snapshot().await;
    let reads = cluster_state
        .nodes
        .keys()
        .filter(|name| **name == state.node_name || !cluster_state.unreachable.contains(*name))
        .map(|name| async move {
            let reading = if *name == state.node_name {
                local(state).await
            } else {
                let fetched = tokio::time::timeout(
                    PEER_TIMEOUT,
                    internal_peer::fetch_json_within::<NodeLiveUsage>(
                        state,
                        name,
                        NODE_LIVE_USAGE_PATH,
                        PEER_TIMEOUT,
                    ),
                )
                .await;
                match fetched {
                    Ok(Ok(reading)) => reading,
                    Ok(Err(e)) => {
                        warn!(node = %name, error = %e, "no live usage from node, using declared limits");
                        return None;
                    }
                    Err(_) => {
                        warn!(node = %name, "live usage from node timed out, using declared limits");
                        return None;
                    }
                }
            };
            Some((name.clone(), reading))
        });

    let mut live = LiveUsage::default();
    for (name, reading) in futures::future::join_all(reads).await.into_iter().flatten() {
        live.nodes.insert(name, reading.node);
        live.instances.extend(reading.instances);
    }
    live
}
