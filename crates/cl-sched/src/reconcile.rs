//! The per-node reconcile loop: make docker match what the cluster decided.
//!
//! The diff is a pure function so every branch is testable without a docker
//! daemon. Applying it is the only part that touches the engine.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use cl_docker::{ContainerSpec, ContainerState, Docker, LABEL_INSTANCE};
use cl_raft::ids::InstanceId;
use cl_raft::state::{Assignment, DesiredState};
use tracing::{info, warn};

use crate::error::Result;

/// What has to happen to one container to bring it in line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Not there at all, create and start it.
    Create(InstanceId),
    /// There but stopped, and it should be running.
    Start(InstanceId),
    /// Running but it should not be.
    Stop(InstanceId),
    /// Not ours any more, take it away.
    Remove(InstanceId),
    /// The spec changed, so it has to be replaced rather than restarted.
    Recreate(InstanceId),
}

impl Action {
    pub fn instance(&self) -> &InstanceId {
        match self {
            Action::Create(id)
            | Action::Start(id)
            | Action::Stop(id)
            | Action::Remove(id)
            | Action::Recreate(id) => id,
        }
    }
}

/// What this node should be running, keyed by instance.
#[derive(Debug, Clone)]
pub struct Target {
    pub assignment: Assignment,
    pub spec: ContainerSpec,
    /// Fingerprint of the spec. A change means the container is replaced.
    pub revision: u64,
    /// How long to wait for a clean shutdown, from `stop_grace_period`.
    pub stop_grace: Duration,
}

/// Docker's own default is 10 seconds, which is short for a database.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(30);

/// Label carrying the spec revision, so drift is detectable without inspecting
/// every field docker may have normalised.
pub const LABEL_REVISION: &str = "clouduler.revision";

/// Work out what to do, given what the cluster wants and what docker has.
///
/// `fenced` short circuits everything: a node that has lost quorum stops what
/// it runs and starts nothing, because the rest of the cluster is about to hand
/// its work to somebody else.
pub fn diff(targets: &[Target], actual: &[ContainerState], fenced: bool) -> Vec<Action> {
    let running: BTreeMap<InstanceId, &ContainerState> = actual
        .iter()
        .filter_map(|c| {
            let id = c.labels.get(LABEL_INSTANCE)?.parse::<InstanceId>().ok()?;
            Some((id, c))
        })
        .collect();

    if fenced {
        return running
            .iter()
            .filter(|(_, c)| c.running)
            .map(|(id, _)| Action::Stop(id.clone()))
            .collect();
    }

    let mut actions = Vec::new();
    let mut wanted = BTreeSet::new();

    for target in targets {
        let id = &target.assignment.instance;
        wanted.insert(id.clone());

        let Some(current) = running.get(id) else {
            // Nothing here yet. A stopped assignment needs no container at all.
            if target.assignment.desired == DesiredState::Running {
                actions.push(Action::Create(id.clone()));
            }
            continue;
        };

        // A changed spec cannot be patched onto a live container, so replace it.
        let current_revision = current
            .labels
            .get(LABEL_REVISION)
            .and_then(|r| r.parse::<u64>().ok());
        if current_revision != Some(target.revision) {
            actions.push(Action::Recreate(id.clone()));
            continue;
        }

        match (target.assignment.desired, current.running) {
            (DesiredState::Running, false) => actions.push(Action::Start(id.clone())),
            (DesiredState::Stopped, true) => actions.push(Action::Stop(id.clone())),
            _ => {}
        }
    }

    // Anything managed by clouduler that this node is no longer assigned.
    for id in running.keys() {
        if !wanted.contains(id) {
            actions.push(Action::Remove(id.clone()));
        }
    }

    actions
}

/// Carry out a diff against the local docker daemon.
pub struct Reconciler {
    docker: Docker,
}

impl Reconciler {
    pub fn new(docker: Docker) -> Self {
        Self { docker }
    }

    /// Apply the actions. One failure does not abort the rest: a single broken
    /// container must not stop the node reconciling everything else.
    pub async fn apply(&self, actions: &[Action], targets: &[Target]) -> Result<()> {
        let specs: BTreeMap<&InstanceId, &Target> = targets
            .iter()
            .map(|t| (&t.assignment.instance, t))
            .collect();

        for action in actions {
            let id = action.instance();
            if let Err(e) = self.apply_one(action, specs.get(id).copied()).await {
                warn!(instance = %id, error = %e, "reconcile step failed");
            }
        }
        Ok(())
    }

    async fn apply_one(&self, action: &Action, target: Option<&Target>) -> Result<()> {
        let name = action.instance().container_name();

        match action {
            Action::Create(id) => {
                let Some(target) = target else {
                    warn!(instance = %id, "no spec to create from");
                    return Ok(());
                };
                info!(instance = %id, "creating");
                self.docker.create(&target.spec).await?;
                self.docker.start(&name).await?;
            }
            Action::Start(id) => {
                info!(instance = %id, "starting");
                self.docker.start(&name).await?;
            }
            Action::Stop(id) => {
                info!(instance = %id, "stopping");
                self.docker.stop(&name, stop_timeout(target)).await?;
            }
            Action::Remove(id) => {
                info!(instance = %id, "removing");
                self.docker.stop(&name, stop_timeout(target)).await.ok();
                self.docker.remove(&name, true).await?;
            }
            Action::Recreate(id) => {
                let Some(target) = target else {
                    warn!(instance = %id, "no spec to recreate from");
                    return Ok(());
                };
                info!(instance = %id, "recreating after a spec change");
                self.docker.stop(&name, stop_timeout(Some(target))).await.ok();
                self.docker.remove(&name, true).await.ok();
                self.docker.create(&target.spec).await?;
                self.docker.start(&name).await?;
            }
        }
        Ok(())
    }
}

fn stop_timeout(target: Option<&Target>) -> Duration {
    target.map(|t| t.stop_grace).unwrap_or(DEFAULT_STOP_GRACE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_docker::{LABEL_MANAGED, MANAGED_VALUE};
    use cl_raft::state::Assignment;
    use std::collections::HashMap;

    fn target(service: &str, index: u32, desired: DesiredState, revision: u64) -> Target {
        let instance = InstanceId::new("site", service, index);
        Target {
            assignment: Assignment {
                instance: instance.clone(),
                node: "n1".into(),
                desired,
                image_digest: None,
                migration: None,
            },
            spec: ContainerSpec {
                name: instance.container_name(),
                image: "nginx".into(),
                stack: "site".into(),
                service: service.into(),
                instance: instance.to_string(),
                ..Default::default()
            },
            revision,
            stop_grace: DEFAULT_STOP_GRACE,
        }
    }

    fn container(service: &str, index: u32, running: bool, revision: u64) -> ContainerState {
        let instance = InstanceId::new("site", service, index);
        let mut labels = HashMap::new();
        labels.insert(LABEL_INSTANCE.to_string(), instance.to_string());
        labels.insert(LABEL_MANAGED.to_string(), MANAGED_VALUE.to_string());
        labels.insert(LABEL_REVISION.to_string(), revision.to_string());

        ContainerState {
            id: format!("id-{service}-{index}"),
            image: "nginx".into(),
            image_digest: None,
            running,
            health: None,
            exit_code: None,
            labels,
            started_at: None,
        }
    }

    #[test]
    fn a_missing_container_is_created() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let actions = diff(&targets, &[], false);
        assert_eq!(actions, vec![Action::Create(InstanceId::new("site", "web", 0))]);
    }

    #[test]
    fn a_stopped_assignment_creates_nothing() {
        let targets = vec![target("web", 0, DesiredState::Stopped, 1)];
        assert!(diff(&targets, &[], false).is_empty());
    }

    #[test]
    fn a_stopped_container_that_should_run_is_started() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let actual = vec![container("web", 0, false, 1)];
        assert_eq!(
            diff(&targets, &actual, false),
            vec![Action::Start(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn a_running_container_that_should_not_run_is_stopped() {
        let targets = vec![target("web", 0, DesiredState::Stopped, 1)];
        let actual = vec![container("web", 0, true, 1)];
        assert_eq!(
            diff(&targets, &actual, false),
            vec![Action::Stop(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn a_correct_container_needs_nothing() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let actual = vec![container("web", 0, true, 1)];
        assert!(diff(&targets, &actual, false).is_empty());
    }

    #[test]
    fn a_changed_spec_forces_a_recreate() {
        let targets = vec![target("web", 0, DesiredState::Running, 2)];
        let actual = vec![container("web", 0, true, 1)];
        assert_eq!(
            diff(&targets, &actual, false),
            vec![Action::Recreate(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn a_container_without_a_revision_label_is_recreated() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let mut actual = vec![container("web", 0, true, 1)];
        actual[0].labels.remove(LABEL_REVISION);
        assert_eq!(
            diff(&targets, &actual, false),
            vec![Action::Recreate(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn an_unassigned_container_is_removed() {
        let actual = vec![container("old", 0, true, 1)];
        assert_eq!(
            diff(&[], &actual, false),
            vec![Action::Remove(InstanceId::new("site", "old", 0))]
        );
    }

    #[test]
    fn fencing_stops_everything_and_starts_nothing() {
        let targets = vec![
            target("web", 0, DesiredState::Running, 1),
            target("db", 0, DesiredState::Running, 1),
        ];
        let actual = vec![container("web", 0, true, 1), container("db", 0, false, 1)];

        let actions = diff(&targets, &actual, true);
        // Only the running one needs stopping, and nothing gets created.
        assert_eq!(actions, vec![Action::Stop(InstanceId::new("site", "web", 0))]);
    }

    #[test]
    fn containers_with_unparseable_labels_are_ignored() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let mut actual = vec![container("web", 0, true, 1)];
        actual[0]
            .labels
            .insert(LABEL_INSTANCE.to_string(), "nonsense".to_string());

        // The bad container is invisible to us, so the real one gets created.
        assert_eq!(
            diff(&targets, &actual, false),
            vec![Action::Create(InstanceId::new("site", "web", 0))]
        );
    }
}
