//! Which addresses the rpc server listens on.
//!
//! A node's rpc address is not fixed for life: moving a cluster onto another
//! network (a VPN, a new subnet) re-addresses every node, managers included.
//! Raft peers dial the address stored in raft membership, and that only
//! changes once the leader commits it, after it has checked the node answers
//! at the new one. So for a while the address the config says and the address
//! membership says differ, and the node must answer on both: the new one so
//! the leader can verify and then use it, the old one so the cluster keeps
//! working until that commit lands. An address no longer wanted is kept a
//! little longer, since a lagging peer may still dial it.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

/// How long an address nobody asks for anymore keeps being served.
pub const RETIRE_GRACE: Duration = Duration::from_secs(120);

/// What a reconcile pass wants done.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub bind: Vec<SocketAddr>,
    pub drop: Vec<SocketAddr>,
}

/// The addresses currently served, and when each stopped being wanted.
#[derive(Debug, Default)]
pub struct Wanted {
    /// `None` while wanted, otherwise since when it has not been.
    served: BTreeMap<SocketAddr, Option<Instant>>,
}

impl Wanted {
    /// Compare `desired` against what is served. An address bound successfully
    /// must be reported through [`Wanted::bound`]; one that failed to bind is
    /// simply planned again on the next pass.
    pub fn plan(&mut self, desired: &BTreeSet<SocketAddr>, now: Instant) -> Plan {
        let mut plan = Plan::default();
        for addr in desired {
            match self.served.get_mut(addr) {
                Some(since) => *since = None,
                None => plan.bind.push(*addr),
            }
        }
        for (addr, since) in self.served.iter_mut() {
            if desired.contains(addr) {
                continue;
            }
            match since {
                None => *since = Some(now),
                Some(at) if now.duration_since(*at) >= RETIRE_GRACE => plan.drop.push(*addr),
                Some(_) => {}
            }
        }
        for addr in &plan.drop {
            self.served.remove(addr);
        }
        plan
    }

    pub fn bound(&mut self, addr: SocketAddr) {
        self.served.insert(addr, None);
    }

    pub fn addrs(&self) -> BTreeSet<SocketAddr> {
        self.served.keys().copied().collect()
    }
}

/// Aborts the serving tasks it owns when dropped, so stopping the supervisor
/// stops every listener it started.
#[derive(Default)]
pub struct Tasks(pub BTreeMap<SocketAddr, JoinHandle<()>>);

impl Drop for Tasks {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn set(items: &[&str]) -> BTreeSet<SocketAddr> {
        items.iter().map(|s| a(s)).collect()
    }

    #[test]
    fn a_new_address_is_bound_and_the_old_one_kept_during_the_grace() {
        let mut w = Wanted::default();
        let t0 = Instant::now();
        let first = w.plan(&set(&["10.0.0.1:7373"]), t0);
        assert_eq!(first.bind, vec![a("10.0.0.1:7373")]);
        w.bound(a("10.0.0.1:7373"));

        let both = set(&["10.0.0.1:7373", "100.64.0.1:7373"]);
        let second = w.plan(&both, t0);
        assert_eq!(second.bind, vec![a("100.64.0.1:7373")]);
        w.bound(a("100.64.0.1:7373"));

        // Only the new address is wanted from now on: the old one stays up.
        let only_new = set(&["100.64.0.1:7373"]);
        assert_eq!(w.plan(&only_new, t0), Plan::default());
        assert_eq!(
            w.plan(&only_new, t0 + RETIRE_GRACE - Duration::from_secs(1)),
            Plan::default()
        );
        assert_eq!(w.addrs(), both);
    }

    #[test]
    fn an_unwanted_address_is_dropped_once_the_grace_is_over() {
        let mut w = Wanted::default();
        let t0 = Instant::now();
        w.bound(a("10.0.0.1:7373"));
        w.bound(a("100.64.0.1:7373"));
        let only_new = set(&["100.64.0.1:7373"]);
        w.plan(&only_new, t0);
        let plan = w.plan(&only_new, t0 + RETIRE_GRACE);
        assert_eq!(plan.drop, vec![a("10.0.0.1:7373")]);
        assert_eq!(w.addrs(), only_new);
    }

    #[test]
    fn wanting_an_address_again_cancels_its_retirement() {
        let mut w = Wanted::default();
        let t0 = Instant::now();
        w.bound(a("10.0.0.1:7373"));
        w.bound(a("100.64.0.1:7373"));
        w.plan(&set(&["100.64.0.1:7373"]), t0);
        // The config was edited back before the grace ran out.
        w.plan(&set(&["10.0.0.1:7373"]), t0 + Duration::from_secs(30));
        let plan = w.plan(&set(&["10.0.0.1:7373"]), t0 + RETIRE_GRACE * 2);
        assert_eq!(plan.drop, vec![a("100.64.0.1:7373")]);
        assert!(plan.bind.is_empty());
        assert_eq!(w.addrs(), set(&["10.0.0.1:7373"]));
    }

    #[test]
    fn a_failed_bind_is_planned_again() {
        let mut w = Wanted::default();
        let t0 = Instant::now();
        let want = set(&["100.64.0.1:7373"]);
        assert_eq!(w.plan(&want, t0).bind.len(), 1);
        // The bind failed (the tailnet is not up yet), so it was never reported.
        assert_eq!(w.plan(&want, t0).bind.len(), 1);
    }
}
