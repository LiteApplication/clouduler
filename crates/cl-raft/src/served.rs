//! Which node holds a served drive's authoritative copy, and the handover
//! between nodes.
//!
//! `served_by` order says who is *preferred*; it does not say whose data is
//! current. A standby that took over has been written to, so when the
//! preferred node comes back, handing the drive straight back to it would
//! serve (and then sync out) the stale copy. This record is what stops that:
//! the holder stays active until a handover completes.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The authoritative holder of one served drive.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServedAuthority {
    /// The node serving the drive.
    pub holder: String,
    /// Bumped whenever the holder changes.
    pub epoch: u64,
    /// A handover in progress: nobody serves the drive until it ends.
    #[serde(default)]
    pub handover: Option<ServedHandover>,
    /// Both copies were written since they last agreed. Replication between
    /// them is stopped until an operator resolves it.
    #[serde(default)]
    pub conflict: Option<ServedConflict>,
    /// The last automatic fail-back that could not finish. The holder keeps
    /// serving; the next automatic attempt waits `FAILBACK_RETRY_SECS`.
    #[serde(default)]
    pub blocked: Option<FailbackBlock>,
    /// An operator asked for a fail-back (the only way to get one when the
    /// drive's `failback` is `manual`). Consumed when the handover begins.
    #[serde(default)]
    pub failback_requested: bool,
    /// A node whose copy the holder may overwrite without the dirtiness
    /// check, because an operator resolved a conflict in the holder's favour.
    #[serde(default)]
    pub overwrite: Option<String>,
}

/// Minimum seconds between automatic fail-back attempts after one failed.
pub const FAILBACK_RETRY_SECS: i64 = 3600;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServedHandover {
    pub to: String,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServedConflict {
    /// The node that was serving when the conflict was found.
    pub holder: String,
    /// The node whose copy diverged from it.
    pub other: String,
    pub reason: String,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailbackBlock {
    pub node: String,
    pub reason: String,
    pub at: i64,
}

/// One change to a served drive's `ServedAuthority`, sent as
/// `Request::Served`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServedOp {
    /// `node` serves the drive now. Only accepted when the current holder is
    /// unreachable or no longer in `served_by` (or there is none yet), so a
    /// holder cannot be replaced behind its back.
    Takeover {
        node: String,
        served_by: Vec<String>,
    },
    /// The holder starts handing the drive to `to`.
    BeginHandover { to: String, at: i64 },
    /// The holder has finished its final sync: `to` serves from now on.
    CompleteHandover { to: String },
    /// The handover could not finish. The holder serves again.
    AbortHandover { reason: String, at: i64 },
    /// Both copies were written. Replication is stopped.
    Conflict {
        holder: String,
        other: String,
        reason: String,
        at: i64,
    },
    /// An operator picked the copy to keep after a conflict.
    Resolve { keep: String },
    /// Ask for a fail-back now, whatever the drive's `failback` says.
    RequestFailback,
    /// The overwrite an operator allowed has been done.
    ClearOverwrite,
}

/// The node serving a drive, given its candidates in priority order, the
/// nodes known unreachable and the drive's recorded authority.
///
/// Without a record, or when the recorded holder is gone, this is the first
/// reachable candidate. With one, the holder keeps serving however many
/// higher-priority candidates are back, until a handover moves it; during a
/// handover nobody serves.
pub fn choose_active_server<'a>(
    served_by: &'a [String],
    unreachable: &BTreeSet<String>,
    authority: Option<&ServedAuthority>,
) -> Option<&'a str> {
    let first_reachable = || {
        served_by
            .iter()
            .map(String::as_str)
            .find(|n| !unreachable.contains(*n))
    };
    let Some(authority) = authority else {
        return first_reachable();
    };
    let holder = served_by
        .iter()
        .map(String::as_str)
        .find(|n| *n == authority.holder && !unreachable.contains(*n));
    match holder {
        None => first_reachable(),
        Some(_) if authority.handover.is_some() => None,
        Some(holder) => Some(holder),
    }
}

impl ServedAuthority {
    /// Applies `op` to the record for a drive. `current` is the existing
    /// record, if any; `unreachable` is the cluster's unreachable set.
    pub fn apply(
        current: Option<&ServedAuthority>,
        op: &ServedOp,
        unreachable: &BTreeSet<String>,
    ) -> Result<ServedAuthority, String> {
        let mut next = current.cloned();
        match op {
            ServedOp::Takeover { node, served_by } => {
                if unreachable.contains(node) {
                    return Err(format!("{node} is unreachable"));
                }
                match next.as_mut() {
                    None => {
                        next = Some(ServedAuthority {
                            holder: node.clone(),
                            epoch: 1,
                            ..Default::default()
                        })
                    }
                    Some(a) if a.holder == *node => {}
                    Some(a) => {
                        if !unreachable.contains(&a.holder) && served_by.contains(&a.holder) {
                            return Err(format!(
                                "{} is still reachable and holds the drive",
                                a.holder
                            ));
                        }
                        a.holder = node.clone();
                        a.epoch += 1;
                        a.handover = None;
                        a.blocked = None;
                        a.failback_requested = false;
                    }
                }
            }
            ServedOp::BeginHandover { to, at } => {
                let a = next
                    .as_mut()
                    .ok_or("no holder is recorded for this drive")?;
                if a.conflict.is_some() {
                    return Err("the drive has an unresolved conflict".into());
                }
                if a.handover.is_some() {
                    return Err("a handover is already in progress".into());
                }
                if a.holder == *to {
                    return Err(format!("{to} already holds the drive"));
                }
                if unreachable.contains(to) {
                    return Err(format!("{to} is unreachable"));
                }
                a.handover = Some(ServedHandover {
                    to: to.clone(),
                    at: *at,
                });
                a.failback_requested = false;
            }
            ServedOp::CompleteHandover { to } => {
                let a = next
                    .as_mut()
                    .ok_or("no holder is recorded for this drive")?;
                match &a.handover {
                    Some(h) if h.to == *to => {}
                    _ => return Err(format!("no handover to {to} is in progress")),
                }
                a.holder = to.clone();
                a.epoch += 1;
                a.handover = None;
                a.blocked = None;
            }
            ServedOp::AbortHandover { reason, at } => {
                let a = next
                    .as_mut()
                    .ok_or("no holder is recorded for this drive")?;
                let h = a.handover.take().ok_or("no handover is in progress")?;
                a.blocked = Some(FailbackBlock {
                    node: h.to,
                    reason: reason.clone(),
                    at: *at,
                });
            }
            ServedOp::Conflict {
                holder,
                other,
                reason,
                at,
            } => {
                let a = next.get_or_insert_with(|| ServedAuthority {
                    holder: holder.clone(),
                    epoch: 1,
                    ..Default::default()
                });
                a.handover = None;
                a.failback_requested = false;
                a.conflict = Some(ServedConflict {
                    holder: holder.clone(),
                    other: other.clone(),
                    reason: reason.clone(),
                    at: *at,
                });
            }
            ServedOp::Resolve { keep } => {
                let a = next
                    .as_mut()
                    .ok_or("no holder is recorded for this drive")?;
                let conflict = a.conflict.take().ok_or("the drive has no conflict")?;
                if *keep != conflict.holder && *keep != conflict.other {
                    a.conflict = Some(conflict.clone());
                    return Err(format!(
                        "keep must be {} or {}",
                        conflict.holder, conflict.other
                    ));
                }
                let loser = if *keep == conflict.holder {
                    conflict.other
                } else {
                    conflict.holder
                };
                if a.holder != *keep {
                    a.holder = keep.clone();
                    a.epoch += 1;
                }
                a.handover = None;
                a.blocked = None;
                a.overwrite = Some(loser);
            }
            ServedOp::RequestFailback => {
                let a = next
                    .as_mut()
                    .ok_or("no holder is recorded for this drive")?;
                a.blocked = None;
                a.failback_requested = true;
            }
            ServedOp::ClearOverwrite => {
                if let Some(a) = next.as_mut() {
                    a.overwrite = None;
                }
            }
        }
        next.ok_or_else(|| "no change".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn down(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn held_by(node: &str) -> ServedAuthority {
        ServedAuthority {
            holder: node.into(),
            epoch: 1,
            ..Default::default()
        }
    }

    #[test]
    fn without_a_record_the_first_reachable_candidate_serves() {
        let by = names(&["n1", "n2"]);
        assert_eq!(choose_active_server(&by, &down(&[]), None), Some("n1"));
        assert_eq!(choose_active_server(&by, &down(&["n1"]), None), Some("n2"));
        assert_eq!(choose_active_server(&by, &down(&["n1", "n2"]), None), None);
    }

    #[test]
    fn the_standby_keeps_serving_after_the_preferred_node_returns() {
        let by = names(&["n1", "n2"]);
        let a = held_by("n2");
        assert_eq!(
            choose_active_server(&by, &down(&[]), Some(&a)),
            Some("n2"),
            "n1 is back and first in served_by, but n2 holds the newer data"
        );
    }

    #[test]
    fn a_holder_that_goes_unreachable_is_replaced() {
        let by = names(&["n1", "n2"]);
        let a = held_by("n2");
        assert_eq!(
            choose_active_server(&by, &down(&["n2"]), Some(&a)),
            Some("n1")
        );
    }

    #[test]
    fn nobody_serves_during_a_handover() {
        let by = names(&["n1", "n2"]);
        let mut a = held_by("n2");
        a.handover = Some(ServedHandover {
            to: "n1".into(),
            at: 0,
        });
        assert_eq!(choose_active_server(&by, &down(&[]), Some(&a)), None);
        assert_eq!(
            choose_active_server(&by, &down(&["n2"]), Some(&a)),
            Some("n1"),
            "a holder that dies mid-handover is failed over from as usual"
        );
    }

    #[test]
    fn a_takeover_needs_the_holder_to_be_gone() {
        let a = held_by("n1");
        let err = ServedAuthority::apply(
            Some(&a),
            &ServedOp::Takeover {
                node: "n2".into(),
                served_by: names(&["n1", "n2"]),
            },
            &down(&[]),
        )
        .unwrap_err();
        assert!(err.contains("still reachable"), "{err}");

        let next = ServedAuthority::apply(
            Some(&a),
            &ServedOp::Takeover {
                node: "n2".into(),
                served_by: names(&["n1", "n2"]),
            },
            &down(&["n1"]),
        )
        .unwrap();
        assert_eq!((next.holder.as_str(), next.epoch), ("n2", 2));
    }

    #[test]
    fn the_first_takeover_creates_the_record() {
        let next = ServedAuthority::apply(
            None,
            &ServedOp::Takeover {
                node: "n1".into(),
                served_by: names(&["n1"]),
            },
            &down(&[]),
        )
        .unwrap();
        assert_eq!((next.holder.as_str(), next.epoch), ("n1", 1));
    }

    #[test]
    fn a_handover_runs_begin_then_complete() {
        let a = held_by("n2");
        let begun = ServedAuthority::apply(
            Some(&a),
            &ServedOp::BeginHandover {
                to: "n1".into(),
                at: 5,
            },
            &down(&[]),
        )
        .unwrap();
        assert!(begun.handover.is_some());
        assert_eq!(
            begun.holder, "n2",
            "the holder is unchanged until it completes"
        );

        let done = ServedAuthority::apply(
            Some(&begun),
            &ServedOp::CompleteHandover { to: "n1".into() },
            &down(&[]),
        )
        .unwrap();
        assert_eq!((done.holder.as_str(), done.epoch), ("n1", 2));
        assert!(done.handover.is_none());
    }

    #[test]
    fn an_aborted_handover_leaves_the_holder_and_records_why() {
        let mut a = held_by("n2");
        a.handover = Some(ServedHandover {
            to: "n1".into(),
            at: 5,
        });
        let next = ServedAuthority::apply(
            Some(&a),
            &ServedOp::AbortHandover {
                reason: "final sync failed".into(),
                at: 9,
            },
            &down(&[]),
        )
        .unwrap();
        assert_eq!(next.holder, "n2");
        assert!(next.handover.is_none());
        let block = next.blocked.unwrap();
        assert_eq!((block.node.as_str(), block.at), ("n1", 9));
    }

    #[test]
    fn no_handover_starts_over_a_conflict() {
        let mut a = held_by("n2");
        a.conflict = Some(ServedConflict {
            holder: "n2".into(),
            other: "n1".into(),
            reason: "both written".into(),
            at: 1,
        });
        assert!(ServedAuthority::apply(
            Some(&a),
            &ServedOp::BeginHandover {
                to: "n1".into(),
                at: 2
            },
            &down(&[]),
        )
        .is_err());
    }

    #[test]
    fn resolving_keeps_the_chosen_copy_and_allows_overwriting_the_other() {
        let mut a = held_by("n2");
        a.conflict = Some(ServedConflict {
            holder: "n2".into(),
            other: "n1".into(),
            reason: "both written".into(),
            at: 1,
        });
        let keep_other = ServedAuthority::apply(
            Some(&a),
            &ServedOp::Resolve { keep: "n1".into() },
            &down(&[]),
        )
        .unwrap();
        assert_eq!((keep_other.holder.as_str(), keep_other.epoch), ("n1", 2));
        assert_eq!(keep_other.overwrite.as_deref(), Some("n2"));
        assert!(keep_other.conflict.is_none());

        let keep_holder = ServedAuthority::apply(
            Some(&a),
            &ServedOp::Resolve { keep: "n2".into() },
            &down(&[]),
        )
        .unwrap();
        assert_eq!((keep_holder.holder.as_str(), keep_holder.epoch), ("n2", 1));
        assert_eq!(keep_holder.overwrite.as_deref(), Some("n1"));

        assert!(ServedAuthority::apply(
            Some(&a),
            &ServedOp::Resolve { keep: "n3".into() },
            &down(&[]),
        )
        .is_err());
    }
}
