//! Stack, assignment and container status handlers.

use super::*;

impl ClusterState {
    pub(super) fn apply_stack(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::ClearEmergency => {
                self.emergency_stacks.clear();
                Ok(())
            }

            Request::SetPrioritized { stack, on } => {
                if !on {
                    self.prioritized.remove(stack);
                } else if self.stacks.contains_key(stack) {
                    self.prioritized.insert(stack.clone());
                } else {
                    return Err(format!("no stack named {stack}"));
                }
                Ok(())
            }

            Request::DeployStack { name, spec, at } => {
                let revision = self.stacks.get(name).map(|s| s.revision + 1).unwrap_or(1);
                let previous = self.stacks.get(name);
                // A changed spec may have fixed the crash, so every node is worth retrying.
                if previous.is_some_and(|p| p.spec != *spec) {
                    self.crash_moves.retain(|id, _| &id.stack != name);
                }
                let configs = previous.map(|s| s.configs.clone()).unwrap_or_default();
                let secrets = previous.map(|s| s.secrets.clone()).unwrap_or_default();
                let used_configs = previous.map(|s| s.used_configs.clone()).unwrap_or_default();
                let used_secrets = previous.map(|s| s.used_secrets.clone()).unwrap_or_default();
                // An unchanged redeploy keeps the older "previous" so the diff shows the last real change.
                let previous_spec = match previous {
                    Some(p) if p.spec != *spec => Some(p.spec.clone()),
                    Some(p) => p.previous_spec.clone(),
                    None => None,
                };
                self.stacks.insert(
                    name.clone(),
                    StackState {
                        name: name.clone(),
                        spec: spec.clone(),
                        revision,
                        configs,
                        secrets,
                        used_configs,
                        used_secrets,
                        deployed_at: *at,
                        previous_spec,
                    },
                );
                // Objects whose output binding went away are released before the module itself.
                self.release_outputs_for_spec(name, spec);
                // A service that left the spec forgets its terraform module and state.
                self.prune_terraform_for_spec(name, spec);
                Ok(())
            }

            Request::RemoveStack { name } => {
                // Drop the stack first so the in-use check below sees it gone.
                let removed = self.stacks.remove(name);
                self.prioritized.remove(name);
                self.assignments.retain(|id, _| &id.stack != name);
                self.instance_updates.retain(|id, _| &id.stack != name);
                self.auto_updates.retain(|id, _| &id.stack != name);
                self.crash_moves.retain(|id, _| &id.stack != name);
                self.drop_terraform_stack(name);
                if let Some(stack) = removed {
                    for config in &stack.configs {
                        if self.config_in_use(config).is_none() {
                            self.configs.remove(config);
                        }
                    }
                    for secret in &stack.secrets {
                        if self.secret_in_use(secret).is_none() {
                            self.secrets.remove(secret);
                        }
                    }
                }
                Ok(())
            }

            Request::SetStackObjects {
                name,
                configs,
                secrets,
                used_configs,
                used_secrets,
            } => {
                let stack = self
                    .stacks
                    .get_mut(name)
                    .ok_or_else(|| format!("unknown stack {name}"))?;
                stack.configs = configs.clone();
                stack.secrets = secrets.clone();
                stack.used_configs = used_configs.clone();
                stack.used_secrets = used_secrets.clone();
                Ok(())
            }

            Request::Assign {
                instance,
                node,
                drives,
            } => {
                self.check_placement(node, drives)?;
                // Carry over from the assignment being replaced: the pin (a node move does not
                // change the image, and losing it would drift to whatever the tag resolves to
                // next restart) and `desired` (re-placing an operator-stopped instance must not
                // start it; a new assignment starts Running). `migration` is kept only when
                // the node is unchanged: a re-Assign elsewhere supersedes any move in flight,
                // and a stale one would let a driver keep advancing it (`Switching` would
                // repoint the assignment back at the old target).
                let previous = self.assignments.get(instance);
                let image_digest = previous.and_then(|a| a.image_digest.clone());
                let desired = previous.map(|a| a.desired).unwrap_or(DesiredState::Running);
                let migration = previous
                    .filter(|a| a.node == *node)
                    .and_then(|a| a.migration.clone());
                self.assignments.insert(
                    instance.clone(),
                    Assignment {
                        instance: instance.clone(),
                        node: node.clone(),
                        desired,
                        image_digest,
                        migration,
                    },
                );
                Ok(())
            }

            Request::Unassign { instance } => {
                self.assignments.remove(instance);
                self.crash_moves.remove(instance);
                self.instance_updates.remove(instance);
                self.auto_updates.remove(instance);
                self.previous_image_digests.remove(instance);
                Ok(())
            }

            Request::SetDesiredState { instance, desired } => {
                self.assignment_mut(instance)?.desired = *desired;
                Ok(())
            }

            Request::SetImageDigest { instance, digest } => {
                let assignment = self.assignment_mut(instance)?;
                let old = assignment.image_digest.replace(digest.clone());
                // Re-pinning the same digest must not erase what the last real change replaced.
                if let Some(old) = old.filter(|old| old != digest) {
                    self.previous_image_digests.insert(instance.clone(), old);
                }
                Ok(())
            }

            Request::ReportContainerStatus {
                node,
                running,
                secret_revisions,
                config_revisions,
            } => {
                self.container_status
                    .retain(|_, status| &status.node != node);
                for (instance, is_running) in running {
                    self.container_status.insert(
                        instance.clone(),
                        ContainerStatus {
                            node: node.clone(),
                            running: *is_running,
                            secret_revisions: secret_revisions
                                .get(instance)
                                .cloned()
                                .unwrap_or_default(),
                            config_revisions: config_revisions
                                .get(instance)
                                .cloned()
                                .unwrap_or_default(),
                        },
                    );
                }
                Ok(())
            }

            Request::RecordCrashMove { instance, from, at } => {
                if !self.assignments.contains_key(instance) {
                    return Err(format!("unknown instance {instance}"));
                }
                let moves = self.crash_moves.entry(instance.clone()).or_default();
                moves.retain(|_, t| at.saturating_sub(*t) <= CRASH_MOVE_RETENTION_SECS);
                moves.insert(from.clone(), *at);
                Ok(())
            }

            _ => unreachable!("apply_stack was handed a request routed elsewhere"),
        }
    }
}
