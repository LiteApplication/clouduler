//! Assembles storage, network, the RPC server and the quorum gate into one
//! handle the daemon holds for the life of the process.

#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use openraft::{BasicNode, Config, ServerState};
use tokio::sync::{watch, Mutex, RwLock};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::contact::Contact;
use crate::error::{ClusterError, Result};
use crate::ids::{node_id, NodeId};
use crate::liveness::LivenessTracker;
use crate::network::{self, forward, Network, Peer};
use crate::quorum::{from_metrics, QuorumStatus, QuorumWatch};
use crate::server;
use crate::state::{ClusterState, Request, Response};
use crate::store::{StateMachine, Store};
use crate::types::Raft;

/// How stale a quorum acknowledgement may be before the leader stops counting
/// itself as holding quorum. Three heartbeats gives one lost tick of slack.
const QUORUM_ACK_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct Options {
    pub name: String,
    pub rpc_addr: SocketAddr,
    /// Directory holding the raft database.
    pub data_dir: PathBuf,
    pub cluster_name: String,
}

/// A running cluster member.
pub struct ClusterNode {
    pub id: NodeId,
    pub name: String,
    raft: Arc<Raft>,
    network: Network,
    shared: Arc<RwLock<StateMachine>>,
    quorum: QuorumWatch,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl ClusterNode {
    pub async fn start(options: Options) -> Result<Self> {
        let id = node_id(&options.name);

        std::fs::create_dir_all(&options.data_dir).map_err(|source| ClusterError::Storage {
            path: options.data_dir.clone(),
            source,
        })?;
        let store = Store::open(options.data_dir.join("raft.redb"))?;

        let config = Config {
            cluster_name: options.cluster_name,
            heartbeat_interval: 500,
            election_timeout_min: 1500,
            election_timeout_max: 3000,
            // Small state, so snapshot often and keep the log short.
            snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(1000),
            max_in_snapshot_log_to_keep: 100,
            ..Default::default()
        };
        let config = Arc::new(config.validate()?);

        let network = Network::new();
        let raft = Raft::new(
            id,
            config,
            network.clone(),
            store.log_store(),
            store.state_machine(),
        )
        .await?;
        let raft = Arc::new(raft);

        let mut tasks = Vec::new();

        let contact = Contact::new();
        let listener = raft.clone();
        let addr = options.rpc_addr;
        let serve_contact = contact.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = server::serve(addr, listener, serve_contact).await {
                warn!(%addr, error = %e, "rpc server stopped");
            }
        }));

        let (quorum_tx, quorum_rx) = watch::channel(QuorumStatus::unknown());
        let mut metrics = raft.metrics();
        let gate_contact = contact.clone();
        tasks.push(tokio::spawn(async move {
            // Metrics changes drive this, but a node cut off from the cluster
            // stops getting them entirely, and that is exactly the case the
            // gate has to catch. Hence the tick as well.
            let mut ticker = tokio::time::interval(QUORUM_ACK_TIMEOUT / 3);
            loop {
                let status = from_metrics(
                    &metrics.borrow_and_update(),
                    &gate_contact,
                    QUORUM_ACK_TIMEOUT,
                );
                // Only wake watchers when something they care about moved.
                quorum_tx.send_if_modified(|current| {
                    let changed = *current != status;
                    if changed {
                        *current = status;
                    }
                    changed
                });

                tokio::select! {
                    changed = metrics.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {}
                }
            }
        }));

        info!(node = %options.name, %id, addr = %options.rpc_addr, "raft node started");

        Ok(Self {
            id,
            name: options.name,
            raft,
            network,
            shared: store.shared_state(),
            quorum: QuorumWatch::new(quorum_rx),
            tasks: Mutex::new(tasks),
        })
    }

    /// Form a new cluster from `members`. Only ever called once, on the first
    /// manager to come up; every other node joins through membership change.
    pub async fn initialize(&self, members: BTreeMap<NodeId, BasicNode>) -> Result<()> {
        self.raft
            .initialize(members)
            .await
            .map_err(ClusterError::initialize)?;
        Ok(())
    }

    /// Probe every other node and mark the silent ones unreachable, so the
    /// scheduler gives their containers to someone else.
    ///
    /// Runs only while this node leads. Every node keeps its own tracker, and a
    /// new leader starts with a fresh one, so a leader change buys the cluster
    /// another `node_timeout` before it declares anyone dead. That is
    /// deliberate: right after an election is exactly when probes are least
    /// trustworthy.
    pub async fn run_failure_detector(
        self: Arc<Self>,
        probe_interval: Duration,
        node_timeout: Duration,
    ) {
        let mut tracker = LivenessTracker::new();
        let mut ticker = tokio::time::interval(probe_interval);

        loop {
            ticker.tick().await;
            if !self.is_leader() {
                // Not our job right now. Drop what we know so the next term
                // starts from a clean slate.
                tracker = LivenessTracker::new();
                continue;
            }

            let state = self.state().await;
            let now = Instant::now();
            let mut alive = Vec::new();

            for (name, node) in &state.nodes {
                if name == &self.name {
                    tracker.seen(name, now);
                    continue;
                }
                tracker.track(name, now);
                if network::ping(&Peer::connect(&node.rpc_addr.to_string())).await {
                    tracker.seen(name, now);
                    alive.push(name.clone());
                }
            }

            // A node removed from the roster must not keep expiring forever.
            tracker.retain(|name| state.nodes.contains_key(name));

            let expired = tracker.expired(now, node_timeout);
            for name in &expired {
                if !state.unreachable.contains(name) {
                    warn!(node = %name, "unreachable, rescheduling its containers");
                    self.set_reachable(name, false).await;
                }
            }
            for name in alive {
                if state.unreachable.contains(&name) {
                    info!(node = %name, "reachable again");
                    self.set_reachable(&name, true).await;
                }
            }
        }
    }

    async fn set_reachable(&self, node: &str, reachable: bool) {
        let request = Request::SetReachable {
            node: node.to_string(),
            reachable,
        };
        if let Err(e) = self.submit(request).await {
            warn!(node = %node, error = %e, "could not record reachability");
        }
    }

    /// Whether this node already belongs to a formed cluster.
    pub async fn is_initialized(&self) -> Result<bool> {
        self.raft.is_initialized().await.map_err(ClusterError::Fatal)
    }

    /// Add a voting manager to the cluster.
    pub async fn add_manager(&self, id: NodeId, addr: SocketAddr) -> Result<()> {
        self.raft
            .add_learner(id, BasicNode::new(addr.to_string()), true)
            .await
            .map_err(ClusterError::membership)?;
        let mut voters: BTreeSet<NodeId> = self.voters().await;
        voters.insert(id);
        self.raft
            .change_membership(voters, false)
            .await
            .map_err(ClusterError::membership)?;
        Ok(())
    }

    /// Add a worker. Workers replicate state so they can reconcile locally, but
    /// they do not vote.
    pub async fn add_worker(&self, id: NodeId, addr: SocketAddr) -> Result<()> {
        self.raft
            .add_learner(id, BasicNode::new(addr.to_string()), true)
            .await
            .map_err(ClusterError::membership)?;
        Ok(())
    }

    /// Drop a node from the voter set, then from membership entirely.
    pub async fn remove_node(&self, id: NodeId) -> Result<()> {
        let mut voters = self.voters().await;
        if voters.remove(&id) {
            self.raft
                .change_membership(voters, false)
                .await
                .map_err(ClusterError::membership)?;
        }
        self.raft
            .change_membership(openraft::ChangeMembers::RemoveNodes([id].into()), false)
            .await
            .map_err(ClusterError::membership)?;
        Ok(())
    }

    async fn voters(&self) -> BTreeSet<NodeId> {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect()
    }

    /// Submit a write. Applied here when this node leads, otherwise forwarded
    /// to the leader over the same channel raft uses.
    pub async fn submit(&self, request: Request) -> Result<Response> {
        if self.is_leader() {
            return Ok(self
                .raft
                .client_write(request)
                .await
                .map_err(ClusterError::write)?
                .data);
        }

        let leader = self
            .quorum
            .leader()
            .ok_or(ClusterError::NoLeader)?;
        let addr = self.leader_addr(leader).await.ok_or(ClusterError::NoLeader)?;

        let peer = Peer::connect(&addr);
        forward(&peer, request)
            .await
            .map_err(|msg| ClusterError::Forward { leader, msg })
    }

    async fn leader_addr(&self, leader: NodeId) -> Option<String> {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .get_node(&leader)
            .map(|node| node.addr.clone())
    }

    /// Read the applied state. Reads are local, so they can lag the leader by
    /// the replication delay; anything needing linearizability must go through
    /// `submit`.
    pub async fn state(&self) -> ClusterState {
        self.shared.read().await.data.clone()
    }

    pub fn quorum(&self) -> QuorumWatch {
        self.quorum.clone()
    }

    pub fn is_leader(&self) -> bool {
        self.raft.metrics().borrow().state == ServerState::Leader
    }

    pub fn raft(&self) -> Arc<Raft> {
        self.raft.clone()
    }

    pub fn network(&self) -> Network {
        self.network.clone()
    }

    pub async fn shutdown(&self) -> Result<()> {
        for task in self.tasks.lock().await.drain(..) {
            task.abort();
        }
        self.raft
            .shutdown()
            .await
            .map_err(|e| ClusterError::Shutdown(e.to_string()))?;
        Ok(())
    }
}
