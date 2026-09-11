//! The quorum gate.
//!
//! A node that cannot see a majority of managers must not start containers.
//! Containers already running are left alone: killing them on a network blip
//! would turn a partition into an outage.

use std::collections::BTreeSet;
use std::time::Duration;

use tokio::sync::watch;

use crate::contact::Contact;
use crate::ids::NodeId;
use crate::types::Node;

/// Raft's own view of this node, the input to the gate.
pub type Metrics = openraft::RaftMetrics<NodeId, Node>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuorumStatus {
    /// True when a majority of voters is reachable.
    pub has_quorum: bool,
    /// Current leader, if one is known.
    pub leader: Option<NodeId>,
    /// Voters this node can currently reach, including itself.
    pub reachable: BTreeSet<NodeId>,
    /// Total voters in the cluster.
    pub voters: usize,
}

impl QuorumStatus {
    /// Nothing is known yet, so assume the worst and hold work back.
    pub fn unknown() -> Self {
        Self {
            has_quorum: false,
            leader: None,
            reachable: BTreeSet::new(),
            voters: 0,
        }
    }

    /// How many voters must be reachable.
    pub fn needed(&self) -> usize {
        self.voters / 2 + 1
    }

    pub fn is_leader(&self, me: NodeId) -> bool {
        self.leader == Some(me)
    }
}

/// Compute the gate from what raft reports.
pub fn evaluate(
    voters: &BTreeSet<NodeId>,
    reachable: BTreeSet<NodeId>,
    leader: Option<NodeId>,
) -> QuorumStatus {
    let reachable: BTreeSet<NodeId> = reachable.intersection(voters).copied().collect();
    let needed = voters.len() / 2 + 1;
    QuorumStatus {
        // A cluster with no voters is not yet formed, so it holds no quorum.
        has_quorum: !voters.is_empty() && reachable.len() >= needed,
        leader,
        reachable,
        voters: voters.len(),
    }
}

/// Derive the gate from what raft reports plus when the leader last got in touch.
///
/// A leader knows it holds quorum because a majority acknowledged it recently.
/// A follower cannot use `current_leader` alone: it keeps reporting the last
/// leader it saw long after that node has gone, and a learner never runs an
/// election to find out otherwise. So a follower needs both a known leader and
/// recent traffic from it. `reachable` is best effort and only there for the ui.
pub fn from_metrics(
    metrics: &Metrics,
    leader_contact: &Contact,
    quorum_ack_timeout: Duration,
) -> QuorumStatus {
    let voters: BTreeSet<NodeId> = metrics.membership_config.membership().voter_ids().collect();

    let (has_quorum, reachable) = if metrics.state.is_leader() {
        let acked = metrics
            .millis_since_quorum_ack
            .is_some_and(|ms| ms <= quorum_ack_timeout.as_millis() as u64);
        let mut reachable = BTreeSet::from([metrics.id]);
        if let Some(replication) = &metrics.replication {
            reachable.extend(replication.keys().filter(|id| voters.contains(id)));
        }
        (acked, reachable)
    } else {
        let mut reachable = BTreeSet::from([metrics.id]);
        let in_touch = leader_contact.is_fresh(quorum_ack_timeout);
        if let Some(leader) = metrics.current_leader {
            if in_touch {
                reachable.insert(leader);
            }
        }
        (metrics.current_leader.is_some() && in_touch, reachable)
    };

    QuorumStatus {
        has_quorum,
        leader: metrics.current_leader,
        reachable: reachable.intersection(&voters).copied().collect(),
        voters: voters.len(),
    }
}

/// Read side of the gate, cloned into every subsystem that needs to check it.
#[derive(Clone)]
pub struct QuorumWatch {
    rx: watch::Receiver<QuorumStatus>,
}

impl QuorumWatch {
    pub fn new(rx: watch::Receiver<QuorumStatus>) -> Self {
        Self { rx }
    }

    pub fn status(&self) -> QuorumStatus {
        self.rx.borrow().clone()
    }

    pub fn has_quorum(&self) -> bool {
        self.rx.borrow().has_quorum
    }

    pub fn leader(&self) -> Option<NodeId> {
        self.rx.borrow().leader
    }

    /// Wait until the gate opens. Used by subsystems that should idle rather
    /// than fail while the cluster is degraded.
    pub async fn wait_for_quorum(&mut self) {
        // The borrow must be dropped before awaiting, hence the explicit check.
        if self.rx.borrow_and_update().has_quorum {
            return;
        }
        while self.rx.changed().await.is_ok() {
            if self.rx.borrow_and_update().has_quorum {
                return;
            }
        }
    }

    /// Wait until this node is or is not the leader, whichever it is not now.
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[NodeId]) -> BTreeSet<NodeId> {
        ids.iter().copied().collect()
    }

    #[test]
    fn majority_of_three_is_two() {
        let voters = set(&[1, 2, 3]);
        assert!(!evaluate(&voters, set(&[1]), None).has_quorum);
        assert!(evaluate(&voters, set(&[1, 2]), Some(1)).has_quorum);
        assert!(evaluate(&voters, set(&[1, 2, 3]), Some(1)).has_quorum);
    }

    #[test]
    fn majority_of_five_is_three() {
        let voters = set(&[1, 2, 3, 4, 5]);
        assert!(!evaluate(&voters, set(&[1, 2]), None).has_quorum);
        assert!(evaluate(&voters, set(&[1, 2, 3]), Some(2)).has_quorum);
    }

    #[test]
    fn non_voters_do_not_count() {
        let voters = set(&[1, 2, 3]);
        // 9 is a worker, so reaching it does nothing for quorum.
        let status = evaluate(&voters, set(&[1, 9]), None);
        assert!(!status.has_quorum);
        assert_eq!(status.reachable, set(&[1]));
    }

    #[test]
    fn an_empty_cluster_holds_no_quorum() {
        assert!(!evaluate(&set(&[]), set(&[]), None).has_quorum);
        assert!(!QuorumStatus::unknown().has_quorum);
    }

    #[test]
    fn leader_is_reported_per_node() {
        let status = evaluate(&set(&[1, 2, 3]), set(&[1, 2]), Some(2));
        assert!(status.is_leader(2));
        assert!(!status.is_leader(1));
        assert_eq!(status.needed(), 2);
    }

    #[tokio::test]
    async fn waiting_returns_once_the_gate_opens() {
        let (tx, rx) = watch::channel(QuorumStatus::unknown());
        let mut watch = QuorumWatch::new(rx);
        assert!(!watch.has_quorum());

        tokio::spawn(async move {
            tx.send(evaluate(&set(&[1, 2, 3]), set(&[1, 2]), Some(1)))
                .expect("send");
        });

        watch.wait_for_quorum().await;
        assert!(watch.has_quorum());
        assert_eq!(watch.leader(), Some(1));
    }
}
