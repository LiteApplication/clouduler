//! Node membership and availability handlers.

use super::*;

impl ClusterState {
    pub(super) fn apply_node(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::UpsertNode(node) => {
                self.removed.remove(&node.id);
                self.nodes.insert(node.name.clone(), node.clone());
                Ok(())
            }

            Request::SetAvailability { node, availability } => {
                let entry = self
                    .nodes
                    .get_mut(node)
                    .ok_or_else(|| format!("unknown node {node}"))?;
                entry.availability = *availability;
                Ok(())
            }

            Request::SetReachable { node, reachable } => {
                if !self.nodes.contains_key(node) {
                    return Err(format!("unknown node {node}"));
                }
                if *reachable {
                    self.unreachable.remove(node);
                } else {
                    self.unreachable.insert(node.clone());
                }
                Ok(())
            }

            Request::RemoveNode { node } => {
                if let Some(inst) = self.assignments.values().find(|a| &a.node == node) {
                    return Err(format!("node {node} still runs {}", inst.instance));
                }
                let removed = self.nodes.remove(node);
                self.unreachable.remove(node);
                self.wrapped_keys.remove(node);
                if let Some(removed) = removed {
                    self.removed.insert(removed.id);
                }
                for volume in self.volumes.values_mut() {
                    volume.replicas.remove(node);
                    // A removed node must not stay a volume's primary: hand it to the replica
                    // furthest along (highest generation, BTree order for ties, so all nodes agree).
                    // Never refused over this (that would wedge removing a dead node); with no
                    // replica left it stays, which health reports as "primary not a member".
                    if volume.primary == *node {
                        let mut best: Option<(&String, u64)> = None;
                        for (candidate, generation) in &volume.replicas {
                            if best.is_none_or(|(_, g)| *generation > g) {
                                best = Some((candidate, *generation));
                            }
                        }
                        if let Some((candidate, _)) = best {
                            volume.primary = candidate.clone();
                        }
                    }
                }
                self.container_status
                    .retain(|_, status| &status.node != node);
                Ok(())
            }

            Request::RetireNodeId { id } => {
                self.removed.insert(*id);
                Ok(())
            }

            _ => unreachable!("apply_node was handed a request routed elsewhere"),
        }
    }
}
