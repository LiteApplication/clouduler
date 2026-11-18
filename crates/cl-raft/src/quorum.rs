//! The quorum gate: a node that cannot see a majority of managers must not start
//! containers; running ones are left alone, since killing them on a blip would
//! turn a partition into an outage.

use std::collections::BTreeSet;
use std::time::Duration;

use tokio::sync::watch;

use crate::contact::Contact;
use crate::ids::NodeId;
use crate::types::Node;

pub type Metrics = openraft::RaftMetrics<NodeId, Node>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuorumStatus {
    pub has_quorum: bool,
    pub leader: Option<NodeId>,
    pub reachable: BTreeSet<NodeId>,
    pub voters: usize,
}

impl QuorumStatus {
    /// Nothing known yet: assume the worst.
    pub fn unknown() -> Self {
        Self {
            has_quorum: false,
            leader: None,
            reachable: BTreeSet::new(),
            voters: 0,
        }
    }

    pub fn needed(&self) -> usize {
        self.voters / 2 + 1
    }

    pub fn is_leader(&self, me: NodeId) -> bool {
        self.leader == Some(me)
    }
}

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

/// Derive the gate from raft's report plus when the leader last got in touch.
///
/// A leader holds quorum if a majority acknowledged it recently. A follower
/// cannot trust `current_leader` alone (it keeps naming a vanished leader, and
/// a learner never elects), so it needs a known leader and recent traffic.
/// `reachable` is best effort, for the ui.
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

    /// Wait until the gate opens.
    pub async fn wait_for_quorum(&mut self) {
        // The borrow must be dropped before awaiting.
        if self.rx.borrow_and_update().has_quorum {
            return;
        }
        while self.rx.changed().await.is_ok() {
            if self.rx.borrow_and_update().has_quorum {
                return;
            }
        }
    }

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
