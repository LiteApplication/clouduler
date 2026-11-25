//! Assembles storage, network, the RPC server and the quorum gate into the handle the daemon holds.

#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use openraft::{BasicNode, Config, ServerState};
use tokio::sync::{watch, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{info, warn};

use crate::contact::Contact;
use crate::error::{ClusterError, Result};
use crate::ids::{node_id, NodeId};
use crate::liveness::LivenessTracker;
use crate::network::{self, forward, Network, Peer};
use crate::quorum::{from_metrics, QuorumStatus, QuorumWatch};
use crate::server;
use crate::state::{
    ClusterConfigEntry, ClusterState, ConfigEntry, Request, Response, SecretEntry, StackState,
};
use crate::store::{StateMachine, Store};
use crate::types::Raft;

/// Max staleness of a quorum ack before the leader stops counting itself as
/// holding quorum (three heartbeats).
const QUORUM_ACK_TIMEOUT: Duration = Duration::from_millis(1500);

/// How long `add_manager` waits for a ping before deciding the prospective
/// voter is not up yet (`seed_roster` retries).
const CONNECT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Max time for a write forwarded to the leader, so a silently dead leader does
/// not hold the caller (possibly the scheduler loop that must fence this node)
/// for the OS TCP timeout.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Options {
    pub name: String,
    /// Config-declared identity (`cl_config::Node::id`).
    pub id: uuid::Uuid,
    pub rpc_addr: SocketAddr,
    pub data_dir: PathBuf,
    pub cluster_name: String,
    /// The cluster key, live; gates every rpc connection in both directions.
    pub keys: crate::network::KeyWatch,
}

pub struct ClusterNode {
    pub id: NodeId,
    pub name: String,
    raft: Arc<Raft>,
    network: Network,
    keys: crate::network::KeyWatch,
    shared: Arc<RwLock<StateMachine>>,
    quorum: QuorumWatch,
    /// The address the config gives this node; the rpc server listens here too.
    listen: watch::Sender<SocketAddr>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// Force `data_dir` (a stopped node's raft storage) to hold a cluster where this
/// node is the sole voter. Offline disaster recovery: the daemon must not be
/// running and this must run before `start`. See `Store::force_quorum`.
pub async fn force_quorum(
    data_dir: &std::path::Path,
    id: uuid::Uuid,
    addr: SocketAddr,
) -> Result<()> {
    let store = Store::open(data_dir.join("raft.redb"))?;
    store.force_quorum(node_id(id), addr).await?;
    Ok(())
}

/// Offline recovery for `clouduler emergency`: like `force_quorum`, plus only
/// `stack` may start with its `stack_volumes` trusted. See `Store::emergency`.
pub async fn emergency(
    data_dir: &std::path::Path,
    id: uuid::Uuid,
    addr: SocketAddr,
    this_name: &str,
    stacks: &std::collections::BTreeSet<String>,
    stack_volumes: &std::collections::BTreeSet<crate::ids::VolumeId>,
) -> Result<()> {
    let store = Store::open(data_dir.join("raft.redb"))?;
    store
        .emergency(node_id(id), addr, this_name, stacks, stack_volumes)
        .await?;
    Ok(())
}

/// Offline: refills `data_dir` content from a git backup, for `clouduler restore`.
/// See `Store::restore_state`.
pub async fn restore(
    data_dir: &std::path::Path,
    cluster_config: Option<ClusterConfigEntry>,
    stacks: BTreeMap<String, StackState>,
    configs: BTreeMap<String, ConfigEntry>,
    secrets: BTreeMap<String, SecretEntry>,
) -> Result<()> {
    let store = Store::open(data_dir.join("raft.redb"))?;
    store
        .restore_state(cluster_config, stacks, configs, secrets)
        .await?;
    Ok(())
}

/// Offline: see `Store::reset_to`.
pub async fn reset(
    data_dir: &std::path::Path,
    go: &crate::store::ResetGo,
    content: &crate::store::ResetContent,
) -> Result<()> {
    let store = Store::open(data_dir.join("raft.redb"))?;
    store.reset_to(go, content).await?;
    Ok(())
}

/// Offline, after `restore`: refills the terraform modules and sealed states.
pub async fn restore_terraform(
    data_dir: &std::path::Path,
    modules: BTreeMap<String, BTreeMap<String, crate::terraform::TerraformModule>>,
    states: BTreeMap<String, crate::terraform::TerraformState>,
) -> Result<()> {
    let store = Store::open(data_dir.join("raft.redb"))?;
    store.restore_terraform(modules, states).await?;
    Ok(())
}

impl ClusterNode {
    pub async fn start(options: Options) -> Result<Self> {
        let id = node_id(options.id);

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
            snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(1000),
            max_in_snapshot_log_to_keep: 100,
            // openraft's default (200ms) deadline for shipping a whole snapshot in one RPC
            // spuriously times out over a VPN link and retries forever.
            install_snapshot_timeout: 30_000,
            ..Default::default()
        };
        let config = Arc::new(config.validate()?);

        let network = Network::new(options.keys.clone());
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
        let shared = store.shared_state();
        let (quorum_tx, quorum_rx) = watch::channel(QuorumStatus::unknown());
        let quorum = QuorumWatch::new(quorum_rx);

        let addr = options.rpc_addr;
        let ctx = server::Context {
            name: options.name.clone(),
            id,
            raft: raft.clone(),
            shared: shared.clone(),
            quorum: quorum.clone(),
            contact: contact.clone(),
            keys: options.keys.clone(),
        };
        let (listen, listen_rx) = watch::channel(addr);
        tasks.push(tokio::spawn(supervise_listeners(
            id,
            raft.clone(),
            ctx,
            listen_rx,
        )));

        let mut metrics = raft.metrics();
        tasks.push(tokio::spawn(async move {
            // A node cut off from the cluster stops getting metrics changes, which is
            // exactly the case the gate must catch, hence the tick.
            let mut ticker = tokio::time::interval(QUORUM_ACK_TIMEOUT / 3);
            loop {
                let status =
                    from_metrics(&metrics.borrow_and_update(), &contact, QUORUM_ACK_TIMEOUT);
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
            keys: options.keys,
            shared,
            quorum,
            listen,
            tasks: Mutex::new(tasks),
        })
    }

    /// Form a new cluster from `members`; only called once, on the first manager.
    pub async fn initialize(&self, members: BTreeMap<NodeId, BasicNode>) -> Result<()> {
        self.raft
            .initialize(members)
            .await
            .map_err(ClusterError::initialize)?;
        Ok(())
    }

    /// Probe every other node and mark silent ones unreachable so the scheduler
    /// reassigns their containers. Runs only while this node leads. A new leader
    /// starts with a fresh tracker, buying another `node_timeout` before declaring
    /// anyone dead: probes are least trustworthy right after an election.
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
                // Not leading: drop state so the next term starts clean.
                tracker = LivenessTracker::new();
                continue;
            }

            // Owned copies of what the probes need: the shared snapshot is not held across
            // the (possibly seconds-long) pings.
            let (peers, roster, unreachable): (
                Vec<_>,
                std::collections::BTreeSet<_>,
                std::collections::BTreeSet<_>,
            ) = {
                let state = self.snapshot().await;
                (
                    state
                        .nodes
                        .iter()
                        .map(|(name, node)| (name.clone(), node.rpc_addr))
                        .collect(),
                    state.nodes.keys().cloned().collect(),
                    state.unreachable.iter().cloned().collect(),
                )
            };
            let now = Instant::now();
            let mut alive = Vec::new();

            for (name, rpc_addr) in &peers {
                if name == &self.name {
                    tracker.seen(name, now);
                    // Answering this loop proves the leader is up; without this a previous
                    // leader's unreachable verdict on it would never be revoked.
                    alive.push(name.clone());
                    continue;
                }
                tracker.track(name, now);
                // Bounded: the target failure is a node vanishing without closing sockets,
                // where an unbounded connect waits minutes for the OS TCP timeout and stalls
                // the loop. No answer within the probe interval is a miss.
                let peer = Peer::connect(&rpc_addr.to_string(), self.keys.clone());
                if matches!(
                    timeout(probe_interval, network::ping(&peer)).await,
                    Ok(true)
                ) {
                    tracker.seen(name, now);
                    alive.push(name.clone());
                }
            }

            // A node removed from the roster must not keep expiring forever.
            tracker.retain(|name| roster.contains(name));

            let expired = tracker.expired(now, node_timeout);
            for name in &expired {
                if !unreachable.contains(name) {
                    warn!(node = %name, "unreachable, rescheduling its containers");
                    self.set_reachable(name, false).await;
                }
            }
            for name in alive {
                if unreachable.contains(&name) {
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

    pub async fn is_initialized(&self) -> Result<bool> {
        self.raft
            .is_initialized()
            .await
            .map_err(ClusterError::Fatal)
    }

    /// Add a voting manager, but only once it is reachable. Promoting right after
    /// adding the learner is unsafe: openraft's catch-up wait compares lag against
    /// `replication_lag_threshold`, which a small log can sit under, so a learner
    /// that never connected can look caught up. A 1-voter cluster would become a
    /// 2-voter one and lock when the node never comes up. So this pings first and,
    /// on no answer, leaves it a learner and errors (`seed_roster` retries).
    pub async fn add_manager(&self, id: NodeId, addr: SocketAddr) -> Result<()> {
        self.raft
            .add_learner(id, BasicNode::new(addr.to_string()), false)
            .await
            .map_err(ClusterError::membership)?;

        let peer = Peer::connect(&addr.to_string(), self.keys.clone());
        if !matches!(
            timeout(CONNECT_PROBE_TIMEOUT, network::ping(&peer)).await,
            Ok(true)
        ) {
            return Err(ClusterError::Membership(format!(
                "{addr} did not answer, not promoting it to voter yet"
            )));
        }

        let mut voters: BTreeSet<NodeId> = self.voters().await;
        voters.insert(id);
        self.raft
            .change_membership(voters, false)
            .await
            .map_err(ClusterError::membership)?;
        Ok(())
    }

    /// Tell the rpc server the address the config now gives this node. It listens
    /// there at once and keeps the membership address until the leader moves it
    /// (see [`crate::listeners`]); the bind is retried if the address is not up yet.
    pub fn set_rpc_addr(&self, addr: SocketAddr) {
        self.listen.send_if_modified(|current| {
            let changed = *current != addr;
            *current = addr;
            changed
        });
    }

    /// The address raft membership holds for `id`, which peers dial.
    pub fn member_addr(&self, id: NodeId) -> Option<String> {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .get_node(&id)
            .map(|node| node.addr.clone())
    }

    /// Move a member to a new rpc address without changing who votes. Leader only;
    /// no-op if membership already holds `addr`. The node is first asked for its
    /// identity at `addr` and refused unless it answers as `id`: otherwise raft
    /// could dial an address nobody listens on, or one belonging to another node
    /// (a documented `SetNodes` split-brain risk). An unreachable address leaves
    /// membership untouched; the caller retries or the config is edited back.
    pub async fn readdress(&self, id: NodeId, addr: SocketAddr) -> Result<()> {
        let Some(current) = self.member_addr(id) else {
            return Err(ClusterError::Membership(format!(
                "{id} is not a member, nothing to re-address"
            )));
        };
        if current == addr.to_string() {
            return Ok(());
        }

        let client = crate::client::Client::connect(addr.to_string(), self.keys.clone());
        match timeout(CONNECT_PROBE_TIMEOUT, client.status()).await {
            Ok(Ok(status)) if status.id == id => {}
            Ok(Ok(status)) => {
                return Err(ClusterError::Membership(format!(
                    "{addr} answers as {}, not as {id}, refusing to move {id} there",
                    status.id
                )))
            }
            _ => {
                return Err(ClusterError::Membership(format!(
                    "{addr} did not answer, keeping {id} at {current}"
                )))
            }
        }

        self.raft
            .change_membership(
                openraft::ChangeMembers::SetNodes(BTreeMap::from([(
                    id,
                    BasicNode::new(addr.to_string()),
                )])),
                false,
            )
            .await
            .map_err(ClusterError::membership)?;
        info!(node = %id, from = %current, to = %addr, "moved a member to a new rpc address");
        Ok(())
    }

    /// Add a worker: replicates state but does not vote.
    pub async fn add_worker(&self, id: NodeId, addr: SocketAddr) -> Result<()> {
        self.raft
            .add_learner(id, BasicNode::new(addr.to_string()), true)
            .await
            .map_err(ClusterError::membership)?;
        Ok(())
    }

    /// Drop a node from the voter set, then from membership. No-op if not a member.
    pub async fn remove_node(&self, id: NodeId) -> Result<()> {
        let is_member = self
            .raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .nodes()
            .any(|(node_id, _)| *node_id == id);
        if !is_member {
            return Ok(());
        }

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

    /// Whether `id` holds a vote. `seed_roster` polls this so a node demoted by
    /// `force_quorum` or never promoted gets promoted once reachable.
    pub async fn is_voter(&self, id: NodeId) -> bool {
        self.voters().await.contains(&id)
    }

    /// Submit a write: applied here if leading, else forwarded to the leader.
    pub async fn submit(&self, request: Request) -> Result<Response> {
        if self.is_leader() {
            return Ok(self
                .raft
                .client_write(request)
                .await
                .map_err(ClusterError::write)?
                .data);
        }

        let leader = self.quorum.leader().ok_or(ClusterError::NoLeader)?;
        let addr = self
            .leader_addr(leader)
            .await
            .ok_or(ClusterError::NoLeader)?;

        let peer = Peer::connect(&addr, self.keys.clone());
        timeout(FORWARD_TIMEOUT, forward(&peer, request))
            .await
            .unwrap_or_else(|_| Err("the leader did not answer in time".to_string()))
            .map_err(|msg| ClusterError::Forward { leader, msg })
    }

    /// The leader's raft rpc address (`ip:port`), read from raft's live membership
    /// rather than replicated app state, which can lag or hold a stale id (e.g.
    /// after the leader was re-provisioned). `None` if no leader or no address.
    pub async fn leader_rpc_addr(&self) -> Option<String> {
        let leader = self.quorum.leader()?;
        self.leader_addr(leader).await
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

    /// Read the applied state; local, so it may lag the leader (use `submit` for linearizability).
    pub async fn state(&self) -> ClusterState {
        (*self.shared.read().await.data).clone()
    }

    /// A cheap shared snapshot of the applied state (an Arc clone, not a deep copy).
    /// Prefer this over `state()` when you only read.
    pub async fn snapshot(&self) -> std::sync::Arc<ClusterState> {
        self.shared.read().await.data.clone()
    }

    /// Run `f` over the applied state without cloning it.
    pub async fn read<R>(&self, f: impl FnOnce(&ClusterState) -> R) -> R {
        f(&self.shared.read().await.data)
    }

    pub fn quorum(&self) -> QuorumWatch {
        self.quorum.clone()
    }

    pub fn is_leader(&self) -> bool {
        self.raft.metrics().borrow().state == ServerState::Leader
    }

    /// Gives up leadership ahead of this node going away, so the cluster elects
    /// a new leader in about an election timeout instead of noticing a dead
    /// one. openraft 0.9 cannot hand leadership to a chosen node, and voters
    /// refuse a candidate while they still hear the leader, so the leader goes
    /// quiet: with its idle heartbeats off the followers' leases run out, one
    /// of them wins, and this node steps down on seeing the higher term.
    /// Returns whether this node is no longer leader within `wait`; `true`
    /// at once when it never was, and `false` at once when no other voter
    /// exists to take over.
    pub async fn step_down(&self, wait: Duration) -> bool {
        if !self.is_leader() {
            return true;
        }
        let others = self
            .raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .filter(|id| *id != self.id)
            .count();
        if others == 0 {
            return false;
        }
        self.raft.runtime_config().heartbeat(false);
        let mut metrics = self.raft.metrics();
        let stepped = timeout(wait, async {
            while metrics.borrow().state == ServerState::Leader {
                if metrics.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .is_ok();
        // Still leader after all: do not leave it silent.
        if !stepped {
            self.raft.runtime_config().heartbeat(true);
        }
        stepped && !self.is_leader()
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

/// Retry pace for reconciling the listener set when nothing wakes it (failed binds).
const LISTEN_RETRY: Duration = Duration::from_secs(5);

/// Serve rpc on the config's address and the one raft membership holds, binding
/// each as possible. See [`crate::listeners`].
async fn supervise_listeners(
    id: NodeId,
    raft: Arc<Raft>,
    ctx: server::Context,
    mut configured: watch::Receiver<SocketAddr>,
) {
    let mut wanted = crate::listeners::Wanted::default();
    let mut tasks = crate::listeners::Tasks::default();
    let mut metrics = raft.metrics();
    // Metrics change far more often than the retry pace; do not retry (and log) failed binds on each.
    let mut tried: BTreeMap<SocketAddr, Instant> = BTreeMap::new();

    loop {
        let mut desired = BTreeSet::new();
        desired.insert(*configured.borrow());
        let from_membership = metrics
            .borrow()
            .membership_config
            .membership()
            .get_node(&id)
            .and_then(|node| node.addr.parse::<SocketAddr>().ok());
        desired.extend(from_membership);

        let plan = wanted.plan(&desired, Instant::now());
        for addr in plan.drop {
            if let Some(task) = tasks.0.remove(&addr) {
                task.abort();
            }
            info!(%addr, "no longer serving rpc on this address");
        }
        for addr in plan.bind {
            if tried
                .get(&addr)
                .is_some_and(|at| at.elapsed() < LISTEN_RETRY)
            {
                continue;
            }
            tried.insert(addr, Instant::now());
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    info!(%addr, "serving rpc");
                    wanted.bound(addr);
                    let ctx = ctx.clone();
                    tasks.0.insert(
                        addr,
                        tokio::spawn(async move {
                            if let Err(e) = server::serve_listener(listener, ctx).await {
                                warn!(%addr, error = %e, "rpc server stopped");
                            }
                        }),
                    );
                }
                Err(e) => warn!(%addr, error = %e, "cannot bind the rpc address yet, retrying"),
            }
        }

        tokio::select! {
            changed = configured.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            changed = metrics.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = tokio::time::sleep(LISTEN_RETRY) => {}
        }
    }
}
