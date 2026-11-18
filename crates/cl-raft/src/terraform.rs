//! Raft model for per-service OpenTofu modules: the module text, its sealed
//! state, the latest run per service, manual run requests and the fingerprint
//! of the last successful apply.
//!
//! Everything is keyed by `terraform_key(stack, service)` except the modules,
//! which nest stack -> service so a stack's set can be replaced at once.
//! `terraform_applied` records the fingerprint of the last *successful Apply*;
//! it is set by `RecordTerraformRun` itself (a Plan never touches it), so no
//! separate request is needed. Removing a service, or its stack, drops all of
//! its entries (modules, state, run, request, applied): "forget only", never destroy.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::InstanceId;
use crate::state::{base64_decode, ClusterState, Request, MAX_OBJECT_BYTES};

/// Cap on a stored run's `log_tail` (and summary/error text).
pub const MAX_TERRAFORM_LOG_BYTES: usize = 64 * 1024;

/// `"stack/service"`, the key of `terraform_states`, `terraform_runs`,
/// `terraform_requests` and `terraform_applied`.
pub fn terraform_key(stack: &str, service: &str) -> String {
    format!("{stack}/{service}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerraformModule {
    pub source: String,
    /// Bumped by `SetStackTerraform` only when `source` changes.
    pub revision: u64,
}

/// The tofu state, sealed with the cluster key (base64, like `SecretEntry::sealed`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerraformState {
    pub sealed: String,
    /// Compare-and-set counter: starts at 1 on the first put.
    pub serial: u64,
    pub key_fingerprint: String,
}

/// Prints no ciphertext, like `SecretEntry`.
impl fmt::Debug for TerraformState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TerraformState")
            .field("serial", &self.serial)
            .field("key_fingerprint", &self.key_fingerprint)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerraformMode {
    Plan,
    Apply,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerraformOutcome {
    Running,
    Succeeded { summary: String },
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerraformRun {
    pub id: String,
    pub mode: TerraformMode,
    /// Why it ran: "change", "manual", ...
    pub trigger: String,
    /// Node executing the run.
    pub node: String,
    /// The instance whose network namespace the run uses.
    pub instance: InstanceId,
    pub fingerprint: String,
    /// Unix seconds.
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub outcome: TerraformOutcome,
    pub log_tail: String,
}

impl TerraformRun {
    pub fn key(&self) -> String {
        terraform_key(&self.instance.stack, &self.instance.service)
    }

    pub fn is_running(&self) -> bool {
        matches!(self.outcome, TerraformOutcome::Running)
    }
}

/// Keeps the last `max` bytes of `s`, on a char boundary.
fn cap_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

impl ClusterState {
    /// Forgets everything held for one service.
    pub(crate) fn drop_terraform_service(&mut self, stack: &str, service: &str) {
        let key = terraform_key(stack, service);
        self.terraform_states.remove(&key);
        self.terraform_runs.remove(&key);
        self.terraform_requests.remove(&key);
        self.terraform_applied.remove(&key);
    }

    /// Forgets everything held for a stack (`RemoveStack`).
    pub(crate) fn drop_terraform_stack(&mut self, stack: &str) {
        self.terraform_modules.remove(stack);
        let prefix = format!("{stack}/");
        self.terraform_states.retain(|k, _| !k.starts_with(&prefix));
        self.terraform_runs.retain(|k, _| !k.starts_with(&prefix));
        self.terraform_requests
            .retain(|k, _| !k.starts_with(&prefix));
        self.terraform_applied
            .retain(|k, _| !k.starts_with(&prefix));
    }

    /// After `DeployStack`: a service that left the spec takes its module and
    /// state with it. A spec that does not parse leaves them alone.
    pub(crate) fn prune_terraform_for_spec(&mut self, stack: &str, spec: &str) {
        if !self.terraform_modules.contains_key(stack) {
            return;
        }
        let Ok(parsed) = cl_config::StackFile::parse(spec) else {
            return;
        };
        let gone: Vec<String> = self.terraform_modules[stack]
            .keys()
            .filter(|s| !parsed.services.contains_key(*s))
            .cloned()
            .collect();
        for service in gone {
            if let Some(m) = self.terraform_modules.get_mut(stack) {
                m.remove(&service);
            }
            self.drop_terraform_service(stack, &service);
        }
        if self
            .terraform_modules
            .get(stack)
            .is_some_and(|m| m.is_empty())
        {
            self.terraform_modules.remove(stack);
        }
    }

    fn has_terraform_module(&self, key: &str) -> bool {
        key.split_once('/').is_some_and(|(stack, service)| {
            self.terraform_modules
                .get(stack)
                .is_some_and(|m| m.contains_key(service))
        })
    }

    pub(crate) fn apply_terraform(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::SetStackTerraform { stack, modules } => {
                let state = self
                    .stacks
                    .get(stack)
                    .ok_or_else(|| format!("unknown stack {stack}"))?;
                let services = cl_config::StackFile::parse(&state.spec).ok();
                for (service, source) in modules {
                    if source.len() > MAX_OBJECT_BYTES {
                        return Err(format!(
                            "terraform module {service} is over the {MAX_OBJECT_BYTES} byte limit"
                        ));
                    }
                    if service.is_empty() || service.contains('/') {
                        return Err(format!("{service:?} is not a service name"));
                    }
                    if let Some(parsed) = &services {
                        if !parsed.services.contains_key(service) {
                            return Err(format!("stack {stack} has no service {service}"));
                        }
                    }
                }
                let previous = self.terraform_modules.remove(stack).unwrap_or_default();
                let mut next = BTreeMap::new();
                for (service, source) in modules {
                    let revision = match previous.get(service) {
                        Some(old) if old.source == *source => old.revision,
                        Some(old) => old.revision + 1,
                        None => 1,
                    };
                    next.insert(
                        service.clone(),
                        TerraformModule {
                            source: source.clone(),
                            revision,
                        },
                    );
                }
                for service in previous.keys().filter(|s| !modules.contains_key(*s)) {
                    self.drop_terraform_service(stack, service);
                }
                if !next.is_empty() {
                    self.terraform_modules.insert(stack.clone(), next);
                }
                Ok(())
            }

            Request::PutTerraformState {
                key,
                sealed,
                key_fingerprint,
                expect_serial,
            } => {
                if !self.has_terraform_module(key) {
                    return Err(format!("no terraform module for {key}"));
                }
                if base64_decode(sealed).is_err() {
                    return Err(format!("terraform state for {key} is not valid base64"));
                }
                if sealed.len() / 4 * 3 > MAX_OBJECT_BYTES {
                    return Err(format!(
                        "terraform state for {key} is over the {MAX_OBJECT_BYTES} byte limit"
                    ));
                }
                let current = self.terraform_states.get(key).map_or(0, |s| s.serial);
                if current != *expect_serial {
                    return Err(format!(
                        "terraform state for {key} is at serial {current}, not {expect_serial}"
                    ));
                }
                self.terraform_states.insert(
                    key.clone(),
                    TerraformState {
                        sealed: sealed.clone(),
                        serial: current + 1,
                        key_fingerprint: key_fingerprint.clone(),
                    },
                );
                Ok(())
            }

            Request::RecordTerraformRun { run } => {
                let key = run.key();
                if !self.has_terraform_module(&key) {
                    return Err(format!("no terraform module for {key}"));
                }
                if let Some(existing) = self.terraform_runs.get(&key) {
                    if existing.id != run.id && existing.is_running() {
                        return Err(format!("a terraform run is already running for {key}"));
                    }
                    // A late finish for a run that a newer one has replaced must not overwrite it.
                    if existing.id != run.id && !run.is_running() {
                        return Err(format!("terraform run {} was replaced", run.id));
                    }
                    if existing.id == run.id && !existing.is_running() {
                        return Err(format!("terraform run {} already finished", run.id));
                    }
                }
                let mut run = run.clone();
                run.log_tail = cap_tail(&run.log_tail, MAX_TERRAFORM_LOG_BYTES);
                match &mut run.outcome {
                    TerraformOutcome::Running => {
                        if self.terraform_requests.get(&key) == Some(&run.mode) {
                            self.terraform_requests.remove(&key);
                        }
                    }
                    TerraformOutcome::Succeeded { summary } => {
                        *summary = cap_tail(summary, MAX_TERRAFORM_LOG_BYTES);
                        if run.mode == TerraformMode::Apply {
                            self.terraform_applied
                                .insert(key.clone(), run.fingerprint.clone());
                        }
                    }
                    TerraformOutcome::Failed { error } => {
                        *error = cap_tail(error, MAX_TERRAFORM_LOG_BYTES);
                    }
                }
                self.terraform_runs.insert(key, run);
                Ok(())
            }

            Request::RequestTerraformRun { key, mode } => {
                if !self.has_terraform_module(key) {
                    return Err(format!("no terraform module for {key}"));
                }
                self.terraform_requests.insert(key.clone(), *mode);
                Ok(())
            }

            _ => Err("not a terraform request".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::base64_encode;

    const SPEC: &str = "services:\n  web:\n    image: x\n  db:\n    image: y\n";

    fn state() -> ClusterState {
        let mut s = ClusterState::default();
        assert!(s
            .apply(&Request::DeployStack {
                name: "app".into(),
                spec: SPEC.into(),
                at: 0,
            })
            .error
            .is_none());
        s
    }

    fn set(s: &mut ClusterState, mods: &[(&str, &str)]) -> crate::state::Response {
        s.apply(&Request::SetStackTerraform {
            stack: "app".into(),
            modules: mods
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })
    }

    fn run(id: &str, outcome: TerraformOutcome, mode: TerraformMode) -> TerraformRun {
        TerraformRun {
            id: id.into(),
            mode,
            trigger: "manual".into(),
            node: "n1".into(),
            instance: InstanceId::new("app", "web", 0),
            fingerprint: format!("fp-{id}"),
            started_at: 1,
            finished_at: None,
            outcome,
            log_tail: String::new(),
        }
    }

    fn put_state(s: &mut ClusterState, expect: u64) -> crate::state::Response {
        s.apply(&Request::PutTerraformState {
            key: "app/web".into(),
            sealed: base64_encode(b"cipher"),
            key_fingerprint: "fp".into(),
            expect_serial: expect,
        })
    }

    #[test]
    fn key_format() {
        assert_eq!(terraform_key("a", "b"), "a/b");
    }

    #[test]
    fn set_replaces_and_bumps_revision_only_on_change() {
        let mut s = state();
        assert!(set(&mut s, &[("web", "a"), ("db", "b")]).error.is_none());
        assert_eq!(s.terraform_modules["app"]["web"].revision, 1);
        assert!(set(&mut s, &[("web", "a2"), ("db", "b")]).error.is_none());
        assert_eq!(s.terraform_modules["app"]["web"].revision, 2);
        assert_eq!(s.terraform_modules["app"]["db"].revision, 1);
    }

    #[test]
    fn set_rejects_unknown_stack_service_and_oversize() {
        let mut s = state();
        assert!(!s
            .apply(&Request::SetStackTerraform {
                stack: "nope".into(),
                modules: BTreeMap::new(),
            })
            .error
            .is_none());
        assert!(!set(&mut s, &[("ghost", "a")]).error.is_none());
        let big = "x".repeat(MAX_OBJECT_BYTES + 1);
        assert!(!set(&mut s, &[("web", &big)]).error.is_none());
        assert!(s.terraform_modules.is_empty());
    }

    #[test]
    fn set_drops_everything_for_removed_services() {
        let mut s = state();
        set(&mut s, &[("web", "a"), ("db", "b")]);
        put_state(&mut s, 0);
        s.apply(&Request::RequestTerraformRun {
            key: "app/web".into(),
            mode: TerraformMode::Plan,
        });
        s.apply(&Request::RecordTerraformRun {
            run: run(
                "1",
                TerraformOutcome::Succeeded {
                    summary: "ok".into(),
                },
                TerraformMode::Apply,
            ),
        });
        assert!(s.terraform_applied.contains_key("app/web"));
        assert!(set(&mut s, &[("db", "b")]).error.is_none());
        assert!(s.terraform_states.is_empty());
        assert!(s.terraform_runs.is_empty());
        assert!(s.terraform_requests.is_empty());
        assert!(s.terraform_applied.is_empty());
        assert!(set(&mut s, &[]).error.is_none());
        assert!(s.terraform_modules.is_empty());
    }

    #[test]
    fn state_put_is_compare_and_set() {
        let mut s = state();
        assert!(!put_state(&mut s, 0).error.is_none(), "no module yet");
        set(&mut s, &[("web", "a")]);
        assert!(put_state(&mut s, 0).error.is_none());
        assert_eq!(s.terraform_states["app/web"].serial, 1);
        assert!(!put_state(&mut s, 0).error.is_none());
        assert!(put_state(&mut s, 1).error.is_none());
        assert_eq!(s.terraform_states["app/web"].serial, 2);
        let rendered = format!("{:?}", s.terraform_states["app/web"]);
        assert!(!rendered.contains("cipher") && !rendered.contains(&base64_encode(b"cipher")));
    }

    #[test]
    fn state_put_rejects_bad_base64_and_oversize() {
        let mut s = state();
        set(&mut s, &[("web", "a")]);
        let bad = Request::PutTerraformState {
            key: "app/web".into(),
            sealed: "!!!".into(),
            key_fingerprint: "fp".into(),
            expect_serial: 0,
        };
        assert!(!s.apply(&bad).error.is_none());
        let big = Request::PutTerraformState {
            key: "app/web".into(),
            sealed: base64_encode(&vec![0u8; MAX_OBJECT_BYTES + 16]),
            key_fingerprint: "fp".into(),
            expect_serial: 0,
        };
        assert!(!s.apply(&big).error.is_none());
    }

    #[test]
    fn running_claim_is_exclusive_and_finished_runs_are_final() {
        let mut s = state();
        set(&mut s, &[("web", "a")]);
        let rec = |s: &mut ClusterState, r| s.apply(&Request::RecordTerraformRun { run: r });
        assert!(rec(
            &mut s,
            run("1", TerraformOutcome::Running, TerraformMode::Apply)
        )
        .error
        .is_none());
        // Same id may update (progress) but another Running is refused, and so is a stray finish.
        assert!(rec(
            &mut s,
            run("1", TerraformOutcome::Running, TerraformMode::Apply)
        )
        .error
        .is_none());
        assert!(!rec(
            &mut s,
            run("2", TerraformOutcome::Running, TerraformMode::Apply)
        )
        .error
        .is_none());
        assert!(!rec(
            &mut s,
            run(
                "2",
                TerraformOutcome::Failed { error: "e".into() },
                TerraformMode::Apply
            )
        )
        .error
        .is_none());
        assert!(rec(
            &mut s,
            run(
                "1",
                TerraformOutcome::Failed { error: "e".into() },
                TerraformMode::Apply
            )
        )
        .error
        .is_none());
        assert!(!s.terraform_applied.contains_key("app/web"));
        assert!(!rec(
            &mut s,
            run("1", TerraformOutcome::Running, TerraformMode::Apply)
        )
        .error
        .is_none());
        assert!(rec(
            &mut s,
            run("2", TerraformOutcome::Running, TerraformMode::Apply)
        )
        .error
        .is_none());
    }

    #[test]
    fn starting_a_run_clears_the_matching_request_and_apply_records_fingerprint() {
        let mut s = state();
        set(&mut s, &[("web", "a")]);
        s.apply(&Request::RequestTerraformRun {
            key: "app/web".into(),
            mode: TerraformMode::Apply,
        });
        assert_eq!(s.terraform_requests["app/web"], TerraformMode::Apply);
        // A Plan does not satisfy a pending Apply.
        s.apply(&Request::RecordTerraformRun {
            run: run("p", TerraformOutcome::Running, TerraformMode::Plan),
        });
        assert!(s.terraform_requests.contains_key("app/web"));
        s.apply(&Request::RecordTerraformRun {
            run: run(
                "p",
                TerraformOutcome::Succeeded {
                    summary: "s".into(),
                },
                TerraformMode::Plan,
            ),
        });
        assert!(!s.terraform_applied.contains_key("app/web"));
        s.apply(&Request::RecordTerraformRun {
            run: run("a", TerraformOutcome::Running, TerraformMode::Apply),
        });
        assert!(s.terraform_requests.is_empty());
        s.apply(&Request::RecordTerraformRun {
            run: run(
                "a",
                TerraformOutcome::Succeeded {
                    summary: "s".into(),
                },
                TerraformMode::Apply,
            ),
        });
        assert_eq!(s.terraform_applied["app/web"], "fp-a");
    }

    #[test]
    fn request_needs_a_module() {
        let mut s = state();
        assert!(!s
            .apply(&Request::RequestTerraformRun {
                key: "app/web".into(),
                mode: TerraformMode::Plan,
            })
            .error
            .is_none());
    }

    #[test]
    fn log_tail_is_capped_on_a_char_boundary() {
        let mut s = state();
        set(&mut s, &[("web", "a")]);
        let mut r = run("1", TerraformOutcome::Running, TerraformMode::Plan);
        r.log_tail = "é".repeat(MAX_TERRAFORM_LOG_BYTES);
        s.apply(&Request::RecordTerraformRun { run: r });
        let tail = &s.terraform_runs["app/web"].log_tail;
        assert!(tail.len() <= MAX_TERRAFORM_LOG_BYTES);
        assert!(tail.len() > MAX_TERRAFORM_LOG_BYTES - 4);
    }

    #[test]
    fn remove_stack_cascades() {
        let mut s = state();
        set(&mut s, &[("web", "a")]);
        put_state(&mut s, 0);
        s.apply(&Request::RecordTerraformRun {
            run: run("1", TerraformOutcome::Running, TerraformMode::Apply),
        });
        s.apply(&Request::RemoveStack { name: "app".into() });
        assert!(s.terraform_modules.is_empty());
        assert!(s.terraform_states.is_empty());
        assert!(s.terraform_runs.is_empty());
    }

    #[test]
    fn redeploy_dropping_a_service_forgets_its_module() {
        let mut s = state();
        set(&mut s, &[("web", "a"), ("db", "b")]);
        put_state(&mut s, 0);
        s.apply(&Request::DeployStack {
            name: "app".into(),
            spec: "services:\n  db:\n    image: y\n".into(),
            at: 0,
        });
        assert!(!s.terraform_modules["app"].contains_key("web"));
        assert!(s.terraform_modules["app"].contains_key("db"));
        assert!(s.terraform_states.is_empty());
    }

    #[test]
    fn redacted_blanks_the_sealed_state() {
        let mut s = state();
        set(&mut s, &[("web", "a")]);
        put_state(&mut s, 0);
        assert!(s.redacted().terraform_states["app/web"].sealed.is_empty());
    }
}
