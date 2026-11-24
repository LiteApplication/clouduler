//! Volume, freeze and served-drive handlers.

use super::*;

impl ClusterState {
    pub(super) fn apply_volume(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::UpsertVolume { id, primary } => {
                if !self.node_implements(primary, &id.drive) {
                    return Err(format!(
                        "node {primary} does not implement drive {}",
                        id.drive
                    ));
                }
                self.volumes
                    .entry(id.clone())
                    .or_insert_with(|| VolumeState {
                        id: id.clone(),
                        generation: 1,
                        primary: primary.clone(),
                        replicas: BTreeMap::from([(primary.clone(), 1)]),
                        frozen: None,
                        sync_refused: None,
                        looks_empty: false,
                    });
                Ok(())
            }

            Request::SetVolumeReplica {
                id,
                node,
                generation,
            } => {
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                if *generation > volume.generation {
                    return Err(format!(
                        "replica generation {generation} is ahead of {} for {id}",
                        volume.generation
                    ));
                }
                let entry = volume.replicas.entry(node.clone()).or_insert(0);
                // Sync results can arrive out of order, so never move a replica back.
                *entry = (*entry).max(*generation);
                Ok(())
            }

            Request::TouchVolume { id, primary } => {
                if !self.node_implements(primary, &id.drive) {
                    return Err(format!(
                        "node {primary} does not implement drive {}",
                        id.drive
                    ));
                }
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                volume.generation += 1;
                volume.primary = primary.clone();
                volume.replicas.insert(primary.clone(), volume.generation);
                Ok(())
            }

            Request::SetVolumeSyncRefusal { id, refusal } => {
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                volume.sync_refused = refusal.clone();
                Ok(())
            }

            Request::SetVolumeEmpty { id, empty } => {
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                volume.looks_empty = *empty;
                Ok(())
            }

            Request::RemoveVolume { id } => {
                let volume = self
                    .volumes
                    .get(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                if id.volume == "." {
                    return Err("a whole-drive mount is not a volume that can be deleted".into());
                }
                if volume.frozen.is_some() {
                    return Err(format!("{id} is frozen"));
                }
                let users = instances_using_volume(self, id);
                if !users.is_empty() {
                    let names: Vec<String> = users.iter().map(ToString::to_string).collect();
                    return Err(format!(
                        "{id} is still mounted by {}; remove it from the stack first",
                        names.join(", ")
                    ));
                }
                let stacks = stacks_declaring_volume(self, id);
                if !stacks.is_empty() {
                    return Err(format!(
                        "{id} is used by stack {}; remove it from the stack first",
                        stacks.join(", ")
                    ));
                }
                let holders: BTreeSet<String> = self
                    .nodes
                    .keys()
                    .filter(|n| self.node_implements(n, &id.drive))
                    .cloned()
                    .collect();
                self.volumes.remove(id);
                if !holders.is_empty() {
                    self.volume_removals.insert(id.clone(), holders);
                }
                Ok(())
            }

            Request::AckVolumeRemoved { id, node } => {
                if let Some(pending) = self.volume_removals.get_mut(id) {
                    pending.remove(node);
                    if pending.is_empty() {
                        self.volume_removals.remove(id);
                    }
                }
                Ok(())
            }

            Request::FreezeVolume { id, node, stopped } => {
                if !self.node_implements(node, &id.drive) {
                    return Err(format!("node {node} does not implement drive {}", id.drive));
                }
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                if let Some(frozen) = &volume.frozen {
                    return Err(format!("{id} is already frozen on {}", frozen.node));
                }
                volume.frozen = Some(FrozenVolume {
                    node: node.clone(),
                    stopped: stopped.clone(),
                });
                Ok(())
            }

            Request::UnfreezeVolume { id } => {
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                let frozen = volume
                    .frozen
                    .take()
                    .ok_or_else(|| format!("{id} is not frozen"))?;
                volume.generation += 1;
                volume.primary = frozen.node.clone();
                volume.replicas.insert(frozen.node, volume.generation);
                Ok(())
            }

            Request::Served { drive, op } => {
                let next = crate::served::ServedAuthority::apply(
                    self.served.get(drive),
                    op,
                    &self.unreachable,
                )?;
                self.served.insert(drive.clone(), next);
                Ok(())
            }

            _ => unreachable!("apply_volume was handed a request routed elsewhere"),
        }
    }
}
