//! `depends_on` gating (compose `service_started`).
//!
//! The source of truth is the replicated `container_status`, which every node
//! reports for its own instances, so a dependency on another node is visible.

use cl_config::StackFile;
use cl_raft::ids::InstanceId;
use cl_raft::state::ClusterState;

/// The first service `instance` depends on that has no running instance, or
/// `None` when it may be created.
///
/// A service counts as up when at least one of its instances is reported
/// running by the node it is currently assigned to. A dependency that is
/// stopped on purpose or has no instances therefore still blocks. An instance
/// with an update in flight is never held, and a stack that does not parse or
/// a dependency naming no service never blocks, so this fails open.
pub fn unmet_dependency(state: &ClusterState, instance: &InstanceId) -> Option<String> {
    if state.instance_updates.contains_key(instance) {
        return None;
    }
    let stack = state.stacks.get(&instance.stack)?;
    let file = StackFile::parse(&stack.spec).ok()?;
    let service = file.services.get(&instance.service)?;
    service
        .depends_on
        .iter()
        .filter(|dep| file.services.contains_key(*dep) && *dep != &instance.service)
        .find(|dep| !service_running(state, &instance.stack, dep))
        .cloned()
}

fn service_running(state: &ClusterState, stack: &str, service: &str) -> bool {
    state
        .assignments
        .values()
        .filter(|a| a.instance.stack == stack && a.instance.service == service)
        .any(|a| {
            state
                .container_status
                .get(&a.instance)
                .is_some_and(|s| s.running && s.node == a.node)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::{diff, Action, Target, LABEL_REVISION};
    use cl_docker::{ContainerSpec, ContainerState, LABEL_INSTANCE};
    use cl_raft::state::{Assignment, ContainerStatus, DesiredState, StackState};
    use cl_raft::FenceState;

    const SPEC: &str = "services:\n  db:\n    image: x\n    deploy:\n      replicas: 2\n  web:\n    image: x\n    depends_on: [db]\n";

    fn state(db: &[(u32, &str, bool)]) -> ClusterState {
        let mut s = ClusterState::default();
        s.stacks.insert(
            "app".into(),
            StackState {
                name: "app".into(),
                spec: SPEC.into(),
                revision: 1,
                configs: Default::default(),
                secrets: Default::default(),
                used_configs: Default::default(),
                used_secrets: Default::default(),
            },
        );
        for (i, node, running) in db {
            let id = InstanceId::new("app", "db", *i);
            s.assignments.insert(
                id.clone(),
                Assignment {
                    instance: id.clone(),
                    node: node.to_string(),
                    desired: DesiredState::Running,
                    image_digest: None,
                    migration: None,
                },
            );
            s.container_status.insert(
                id,
                ContainerStatus {
                    node: node.to_string(),
                    running: *running,
                },
            );
        }
        s
    }

    fn web() -> InstanceId {
        InstanceId::new("app", "web", 0)
    }

    #[test]
    fn blocks_until_a_dependency_instance_runs() {
        assert_eq!(unmet_dependency(&state(&[]), &web()).as_deref(), Some("db"));
        let down = state(&[(0, "n1", false), (1, "n2", false)]);
        assert_eq!(unmet_dependency(&down, &web()).as_deref(), Some("db"));
    }

    #[test]
    fn one_running_instance_on_another_node_is_enough() {
        let s = state(&[(0, "n1", false), (1, "n2", true)]);
        assert_eq!(unmet_dependency(&s, &web()), None);
    }

    #[test]
    fn a_stopped_dependency_still_gates() {
        let mut s = state(&[(0, "n1", false)]);
        s.assignments
            .get_mut(&InstanceId::new("app", "db", 0))
            .unwrap()
            .desired = DesiredState::Stopped;
        assert_eq!(unmet_dependency(&s, &web()).as_deref(), Some("db"));
    }

    #[test]
    fn service_without_dependencies_is_never_gated() {
        assert_eq!(
            unmet_dependency(&state(&[]), &InstanceId::new("app", "db", 0)),
            None
        );
    }

    fn target(id: &InstanceId, revision: u64, spec: ContainerSpec) -> Target {
        Target {
            assignment: Assignment {
                instance: id.clone(),
                node: "n1".into(),
                desired: DesiredState::Running,
                image_digest: None,
                migration: None,
            },
            spec,
            revision,
            stop_grace: std::time::Duration::from_secs(10),
        }
    }

    /// Mirrors the filter in `Scheduler::reconcile`.
    fn gated(state: &ClusterState, actions: Vec<Action>) -> Vec<Action> {
        actions
            .into_iter()
            .filter(|a| match a {
                Action::Create(id) => unmet_dependency(state, id).is_none(),
                _ => true,
            })
            .collect()
    }

    #[test]
    fn only_create_is_gated_never_an_existing_container() {
        let id = web();
        let s = state(&[(0, "n1", false)]);
        let spec = || ContainerSpec::default();

        // No container yet: the create is held back.
        let actions = diff(&[target(&id, 1, spec())], &[], FenceState::Open);
        assert_eq!(actions, vec![Action::Create(id.clone())]);
        assert!(gated(&s, actions).is_empty());

        // An existing stopped container is restarted, and a stale-spec one
        // recreated, whatever state the dependency is in.
        let mut container = ContainerState::default();
        container
            .labels
            .insert(LABEL_INSTANCE.to_string(), id.to_string());
        container
            .labels
            .insert(LABEL_REVISION.to_string(), "1".to_string());
        let actions = diff(
            &[target(&id, 1, spec())],
            &[container.clone()],
            FenceState::Open,
        );
        assert_eq!(gated(&s, actions), vec![Action::Start(id.clone())]);
        let actions = diff(&[target(&id, 2, spec())], &[container], FenceState::Open);
        assert_eq!(gated(&s, actions), vec![Action::Recreate(id)]);
    }
}
