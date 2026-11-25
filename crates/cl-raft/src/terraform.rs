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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::InstanceId;
use crate::state::{
    base64_decode, validate_object_name, ClusterState, ConfigEntry, Request, SecretEntry,
    MAX_OBJECT_BYTES,
};

/// Cap on a stored run's `log_tail` (and summary/error text).
pub const MAX_TERRAFORM_LOG_BYTES: usize = 64 * 1024;

/// `"stack/service"`, the key of `terraform_states`, `terraform_runs`,
/// `terraform_requests` and `terraform_applied`.
pub fn terraform_key(stack: &str, service: &str) -> String {
    format!("{stack}/{service}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct TerraformModule {
    pub source: String,
    /// Bumped by `SetStackTerraform` only when `source` changes.
    pub revision: u64,
}

/// The tofu state, sealed with the cluster key (base64, like `SecretEntry::sealed`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum TerraformMode {
    Plan,
    Apply,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum TerraformOutcome {
    Running,
    Succeeded { summary: String },
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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

/// Which store a module output is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum OutputKind {
    Secret,
    Config,
}

/// A cluster secret or config written by a module's `outputs:` mapping. The element of
/// `ClusterState::terraform_outputs`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct OutputObject {
    pub kind: OutputKind,
    pub name: String,
}

impl OutputObject {
    pub fn secret(name: impl Into<String>) -> Self {
        Self {
            kind: OutputKind::Secret,
            name: name.into(),
        }
    }

    pub fn config(name: impl Into<String>) -> Self {
        Self {
            kind: OutputKind::Config,
            name: name.into(),
        }
    }
}

/// A secret value from a module output, sealed by the runner (AAD = the secret name).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerraformSecretOutput {
    pub name: String,
    pub sealed: String,
    pub key_fingerprint: String,
}

/// Prints no ciphertext, like `SecretEntry`.
impl fmt::Debug for TerraformSecretOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TerraformSecretOutput")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A config value from a module output (base64, like `ConfigEntry::content`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerraformConfigOutput {
    pub name: String,
    pub content: String,
}

/// The outputs bound by each service of a stack spec; `None` when the spec does not parse.
fn bound_outputs(spec: &str) -> Option<BTreeMap<String, BTreeSet<OutputObject>>> {
    let parsed = cl_config::StackFile::parse(spec).ok()?;
    Some(
        parsed
            .services
            .iter()
            .map(|(name, svc)| {
                let set = svc
                    .x_clouduler
                    .as_ref()
                    .and_then(|x| x.terraform.as_ref())
                    .map(|tf| {
                        tf.outputs
                            .iter()
                            .filter_map(|o| match o.target()? {
                                cl_config::TfOutputTarget::Secret(n) => {
                                    Some(OutputObject::secret(n))
                                }
                                cl_config::TfOutputTarget::Config(n) => {
                                    Some(OutputObject::config(n))
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (name.clone(), set)
            })
            .collect(),
    )
}

fn kind_name(kind: OutputKind) -> &'static str {
    match kind {
        OutputKind::Secret => "secret",
        OutputKind::Config => "config",
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
    /// Whether a stack other than `except` references the object (its `used_*` sets).
    fn output_referenced(&self, obj: &OutputObject, except: Option<&str>) -> bool {
        self.stacks
            .values()
            .filter(|s| Some(s.name.as_str()) != except)
            .any(|s| match obj.kind {
                OutputKind::Secret => s.used_secrets.contains(&obj.name),
                OutputKind::Config => s.used_configs.contains(&obj.name),
            })
    }

    /// Stops `key` owning everything it owns that is not in `keep`, and deletes those objects
    /// unless a stack still references them (then only the ownership goes). `except` names a
    /// stack whose stored `used_*` are stale (it is being redeployed) and `own` its fresh
    /// `(secrets, configs)` references.
    fn release_outputs(
        &mut self,
        key: &str,
        keep: &BTreeSet<OutputObject>,
        except: Option<&str>,
        own: Option<(&BTreeSet<String>, &BTreeSet<String>)>,
    ) {
        let Some(owned) = self.terraform_outputs.get(key).cloned() else {
            return;
        };
        for obj in owned.difference(keep) {
            let own_use = own.is_some_and(|(secrets, configs)| match obj.kind {
                OutputKind::Secret => secrets.contains(&obj.name),
                OutputKind::Config => configs.contains(&obj.name),
            });
            if !own_use && !self.output_referenced(obj, except) {
                match obj.kind {
                    OutputKind::Secret => {
                        self.secrets.remove(&obj.name);
                    }
                    OutputKind::Config => {
                        self.configs.remove(&obj.name);
                    }
                }
            }
            if let Some(set) = self.terraform_outputs.get_mut(key) {
                set.remove(obj);
            }
        }
        if self
            .terraform_outputs
            .get(key)
            .is_some_and(|s| s.is_empty())
        {
            self.terraform_outputs.remove(key);
        }
    }

    /// A deploy changed `stack`'s spec: every service's owned objects that its bindings no
    /// longer name are released. A spec that does not parse leaves them alone.
    pub(crate) fn release_outputs_for_spec(&mut self, stack: &str, spec: &str) {
        let prefix = format!("{stack}/");
        let keys: Vec<String> = self
            .terraform_outputs
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        if keys.is_empty() {
            return;
        }
        let Some(bound) = bound_outputs(spec) else {
            return;
        };
        let (secrets, configs) = match cl_config::StackFile::parse(spec) {
            Ok(p) => {
                let r = p.requirements();
                (r.secrets, r.configs)
            }
            Err(_) => return,
        };
        for key in keys {
            let service = key[prefix.len()..].to_string();
            let keep = bound.get(&service).cloned().unwrap_or_default();
            self.release_outputs(&key, &keep, Some(stack), Some((&secrets, &configs)));
        }
    }

    /// A removed secret or config can no longer be owned by a module.
    pub(crate) fn forget_output_object(&mut self, obj: &OutputObject) {
        self.terraform_outputs.retain(|_, set| {
            set.remove(obj);
            !set.is_empty()
        });
    }

    /// Forgets everything held for one service.
    pub(crate) fn drop_terraform_service(&mut self, stack: &str, service: &str) {
        let key = terraform_key(stack, service);
        self.release_outputs(&key, &BTreeSet::new(), None, None);
        self.terraform_states.remove(&key);
        self.terraform_runs.remove(&key);
        self.terraform_last_apply.remove(&key);
        self.terraform_requests.remove(&key);
        self.terraform_applied.remove(&key);
    }

    /// Forgets everything held for a stack (`RemoveStack`).
    pub(crate) fn drop_terraform_stack(&mut self, stack: &str) {
        self.terraform_modules.remove(stack);
        let prefix = format!("{stack}/");
        let keys: Vec<String> = self
            .terraform_outputs
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        for key in keys {
            // The stack is already out of `stacks`, so only other stacks' references count.
            self.release_outputs(&key, &BTreeSet::new(), Some(stack), None);
        }
        self.terraform_states.retain(|k, _| !k.starts_with(&prefix));
        self.terraform_runs.retain(|k, _| !k.starts_with(&prefix));
        self.terraform_last_apply
            .retain(|k, _| !k.starts_with(&prefix));
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

    /// Writes a run's outputs. Everything is checked before anything is written, so a refusal
    /// leaves the state untouched. An object that already exists (by hand or from another
    /// module) is overwritten and changes owner; a stack's bundled `file:` object is refused.
    fn apply_terraform_outputs(
        &mut self,
        key: &str,
        run_id: &str,
        secrets: &[TerraformSecretOutput],
        configs: &[TerraformConfigOutput],
        claims: &[OutputObject],
    ) -> Result<(), String> {
        if !self.has_terraform_module(key) {
            return Err(format!("no terraform module for {key}"));
        }
        if !self
            .terraform_runs
            .get(key)
            .is_some_and(|r| r.id == run_id && r.is_running())
        {
            return Err(format!(
                "terraform run {run_id} is not the running run of {key}"
            ));
        }
        let (stack, service) = key
            .split_once('/')
            .ok_or_else(|| format!("bad key {key}"))?;
        let spec = &self
            .stacks
            .get(stack)
            .ok_or_else(|| format!("unknown stack {stack}"))?
            .spec;
        let bound = bound_outputs(spec)
            .ok_or_else(|| format!("the spec of stack {stack} does not parse"))?
            .remove(service)
            .unwrap_or_default();

        let mut targets: Vec<OutputObject> = Vec::new();
        for s in secrets {
            if base64_decode(&s.sealed).is_err() {
                return Err(format!("secret {} is not valid base64", s.name));
            }
            if s.sealed.len() / 4 * 3 > MAX_OBJECT_BYTES {
                return Err(format!(
                    "secret {} is over the {MAX_OBJECT_BYTES} byte limit",
                    s.name
                ));
            }
            targets.push(OutputObject::secret(&s.name));
        }
        for c in configs {
            if base64_decode(&c.content).is_err() {
                return Err(format!("config {} is not valid base64", c.name));
            }
            if c.content.len() / 4 * 3 > MAX_OBJECT_BYTES {
                return Err(format!(
                    "config {} is over the {MAX_OBJECT_BYTES} byte limit",
                    c.name
                ));
            }
            targets.push(OutputObject::config(&c.name));
        }
        for obj in &targets {
            validate_object_name(&obj.name)?;
        }
        for obj in targets.iter().chain(claims) {
            if !bound.contains(obj) {
                return Err(format!(
                    "{key} no longer maps an output to {} {}",
                    kind_name(obj.kind),
                    obj.name
                ));
            }
            if let Some(owner) = self.stacks.values().find(|s| match obj.kind {
                OutputKind::Secret => s.secrets.contains(&obj.name),
                OutputKind::Config => s.configs.contains(&obj.name),
            }) {
                return Err(format!(
                    "{} {} is a bundled file of stack {}, a module output cannot overwrite it",
                    kind_name(obj.kind),
                    obj.name,
                    owner.name
                ));
            }
        }
        for obj in claims {
            let exists = match obj.kind {
                OutputKind::Secret => self.secrets.contains_key(&obj.name),
                OutputKind::Config => self.configs.contains_key(&obj.name),
            };
            if !exists {
                return Err(format!(
                    "{} {} does not exist",
                    kind_name(obj.kind),
                    obj.name
                ));
            }
        }

        for s in secrets {
            let revision = self.secrets.get(&s.name).map_or(1, |e| e.revision + 1);
            self.secrets.insert(
                s.name.clone(),
                SecretEntry {
                    name: s.name.clone(),
                    sealed: s.sealed.clone(),
                    revision,
                    key_fingerprint: s.key_fingerprint.clone(),
                    owner: None,
                },
            );
        }
        for c in configs {
            let revision = self.configs.get(&c.name).map_or(1, |e| e.revision + 1);
            self.configs.insert(
                c.name.clone(),
                ConfigEntry {
                    name: c.name.clone(),
                    content: c.content.clone(),
                    revision,
                    owner: None,
                },
            );
        }
        for obj in targets.into_iter().chain(claims.iter().cloned()) {
            // Takeover: whoever owned it before no longer does.
            self.forget_output_object(&obj);
            self.terraform_outputs
                .entry(key.to_string())
                .or_default()
                .insert(obj);
        }
        Ok(())
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
                if run.mode == TerraformMode::Apply {
                    // Only the verdict is needed; the logs stay in `terraform_runs`.
                    let mut kept = run.clone();
                    kept.log_tail.clear();
                    if let TerraformOutcome::Failed { error } = &mut kept.outcome {
                        *error = cap_tail(error, 512);
                    }
                    if let TerraformOutcome::Succeeded { summary } = &mut kept.outcome {
                        summary.clear();
                    }
                    self.terraform_last_apply.insert(key.clone(), kept);
                }
                self.terraform_runs.insert(key, run);
                Ok(())
            }

            Request::PutTerraformOutputs {
                key,
                run_id,
                secrets,
                configs,
                claims,
            } => self.apply_terraform_outputs(key, run_id, secrets, configs, claims),

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

    const OUT_SPEC: &str = "services:\n  web:\n    image: x\n    x-clouduler:\n      terraform:\n        outputs:\n          - {output: id, config: oidc-id}\n          - {output: sec, secret: oidc-sec}\n  db:\n    image: y\n";

    fn deploy(s: &mut ClusterState, name: &str, spec: &str) {
        assert!(s
            .apply(&Request::DeployStack {
                name: name.into(),
                spec: spec.into(),
                at: 0,
            })
            .error
            .is_none());
    }

    fn outputs_state() -> ClusterState {
        let mut s = ClusterState::default();
        deploy(&mut s, "app", OUT_SPEC);
        assert!(set(&mut s, &[("web", "a")]).error.is_none());
        claim(&mut s, "app", "r1");
        s
    }

    fn claim(s: &mut ClusterState, stack: &str, id: &str) {
        let mut r = run(id, TerraformOutcome::Running, TerraformMode::Apply);
        r.instance = InstanceId::new(stack, "web", 0);
        assert!(s
            .apply(&Request::RecordTerraformRun { run: r })
            .error
            .is_none());
    }

    fn put_outputs(
        s: &mut ClusterState,
        key: &str,
        run_id: &str,
        secrets: &[(&str, &str)],
        configs: &[(&str, &str)],
        claims: Vec<OutputObject>,
    ) -> crate::state::Response {
        s.apply(&Request::PutTerraformOutputs {
            key: key.into(),
            run_id: run_id.into(),
            secrets: secrets
                .iter()
                .map(|(n, v)| TerraformSecretOutput {
                    name: n.to_string(),
                    sealed: base64_encode(v.as_bytes()),
                    key_fingerprint: "fp".into(),
                })
                .collect(),
            configs: configs
                .iter()
                .map(|(n, v)| TerraformConfigOutput {
                    name: n.to_string(),
                    content: base64_encode(v.as_bytes()),
                })
                .collect(),
            claims,
        })
    }

    fn owned(s: &ClusterState, key: &str) -> BTreeSet<OutputObject> {
        s.terraform_outputs.get(key).cloned().unwrap_or_default()
    }

    #[test]
    fn outputs_are_written_with_revisions_and_recorded_as_owned() {
        let mut s = outputs_state();
        let r = put_outputs(
            &mut s,
            "app/web",
            "r1",
            &[("oidc-sec", "one")],
            &[("oidc-id", "id1")],
            vec![],
        );
        assert!(r.error.is_none(), "{:?}", r.error);
        assert_eq!(s.secrets["oidc-sec"].revision, 1);
        assert_eq!(s.configs["oidc-id"].revision, 1);
        assert_eq!(s.configs["oidc-id"].decode().unwrap(), b"id1");
        assert_eq!(
            owned(&s, "app/web"),
            BTreeSet::from([
                OutputObject::secret("oidc-sec"),
                OutputObject::config("oidc-id")
            ])
        );
        assert!(
            put_outputs(&mut s, "app/web", "r1", &[("oidc-sec", "two")], &[], vec![])
                .error
                .is_none()
        );
        assert_eq!(s.secrets["oidc-sec"].revision, 2);
        assert_eq!(s.configs["oidc-id"].revision, 1, "untouched");
    }

    #[test]
    fn outputs_need_the_running_run_and_a_live_binding() {
        let mut s = outputs_state();
        assert!(put_outputs(
            &mut s,
            "app/web",
            "other-run",
            &[("oidc-sec", "x")],
            &[],
            vec![]
        )
        .error
        .is_some());
        // not mapped by the spec
        let r = put_outputs(&mut s, "app/web", "r1", &[("unbound", "x")], &[], vec![]);
        assert!(r.error.unwrap().contains("no longer maps"));
        // right name, wrong kind
        assert!(
            put_outputs(&mut s, "app/web", "r1", &[], &[("oidc-sec", "x")], vec![])
                .error
                .is_some()
        );
        // an unknown module
        assert!(
            put_outputs(&mut s, "app/db", "r1", &[("oidc-sec", "x")], &[], vec![])
                .error
                .is_some()
        );
        // a refusal writes nothing, not even the valid half
        let r = put_outputs(
            &mut s,
            "app/web",
            "r1",
            &[("oidc-sec", "x")],
            &[("nope", "x")],
            vec![],
        );
        assert!(r.error.is_some());
        assert!(s.secrets.is_empty() && s.configs.is_empty() && s.terraform_outputs.is_empty());
        // finished run: refused
        s.apply(&Request::RecordTerraformRun {
            run: {
                let mut r = run(
                    "r1",
                    TerraformOutcome::Succeeded {
                        summary: "ok".into(),
                    },
                    TerraformMode::Apply,
                );
                r.instance = InstanceId::new("app", "web", 0);
                r
            },
        });
        assert!(
            put_outputs(&mut s, "app/web", "r1", &[("oidc-sec", "x")], &[], vec![])
                .error
                .is_some()
        );
    }

    #[test]
    fn outputs_oversize_and_bad_base64_are_refused() {
        let mut s = outputs_state();
        let big = "x".repeat(MAX_OBJECT_BYTES + 1);
        assert!(
            put_outputs(&mut s, "app/web", "r1", &[], &[("oidc-id", &big)], vec![])
                .error
                .is_some()
        );
        let r = s.apply(&Request::PutTerraformOutputs {
            key: "app/web".into(),
            run_id: "r1".into(),
            secrets: vec![TerraformSecretOutput {
                name: "oidc-sec".into(),
                sealed: "***".into(),
                key_fingerprint: "fp".into(),
            }],
            configs: vec![],
            claims: vec![],
        });
        assert!(r.error.is_some());
        assert!(s.secrets.is_empty());
    }

    #[test]
    fn a_module_takes_over_a_hand_written_object_and_one_from_another_module() {
        let mut s = outputs_state();
        s.apply(&Request::PutConfig {
            name: "oidc-id".into(),
            content: base64_encode(b"by hand"),
            owner: Some("site".into()),
        });
        assert_eq!(s.configs["oidc-id"].revision, 1);
        assert!(
            put_outputs(&mut s, "app/web", "r1", &[], &[("oidc-id", "mod")], vec![])
                .error
                .is_none()
        );
        assert_eq!(s.configs["oidc-id"].revision, 2);
        assert_eq!(s.configs["oidc-id"].owner, None);
        assert_eq!(
            owned(&s, "app/web"),
            BTreeSet::from([OutputObject::config("oidc-id")])
        );

        // another stack's module binds the same name and takes it
        deploy(&mut s, "other", OUT_SPEC);
        s.apply(&Request::SetStackTerraform {
            stack: "other".into(),
            modules: BTreeMap::from([("web".to_string(), "m".to_string())]),
        });
        claim(&mut s, "other", "r2");
        assert!(put_outputs(
            &mut s,
            "other/web",
            "r2",
            &[],
            &[("oidc-id", "theirs")],
            vec![]
        )
        .error
        .is_none());
        assert_eq!(s.configs["oidc-id"].decode().unwrap(), b"theirs");
        assert!(owned(&s, "app/web").is_empty());
        assert!(!s.terraform_outputs.contains_key("app/web"));
        assert_eq!(
            owned(&s, "other/web"),
            BTreeSet::from([OutputObject::config("oidc-id")])
        );
    }

    #[test]
    fn claims_adopt_an_existing_object_without_rewriting_it() {
        let mut s = outputs_state();
        let r = put_outputs(
            &mut s,
            "app/web",
            "r1",
            &[],
            &[],
            vec![OutputObject::config("oidc-id")],
        );
        assert!(r.error.unwrap().contains("does not exist"));
        s.apply(&Request::PutConfig {
            name: "oidc-id".into(),
            content: base64_encode(b"same"),
            owner: None,
        });
        assert!(put_outputs(
            &mut s,
            "app/web",
            "r1",
            &[],
            &[],
            vec![OutputObject::config("oidc-id")]
        )
        .error
        .is_none());
        assert_eq!(s.configs["oidc-id"].revision, 1);
        assert_eq!(
            owned(&s, "app/web"),
            BTreeSet::from([OutputObject::config("oidc-id")])
        );
    }

    #[test]
    fn a_stacks_bundled_file_objects_are_never_overwritten() {
        let mut s = outputs_state();
        deploy(&mut s, "bundler", "services:\n  a:\n    image: x\n");
        s.apply(&Request::SetStackObjects {
            name: "bundler".into(),
            configs: BTreeSet::from(["oidc-id".to_string()]),
            secrets: BTreeSet::from(["oidc-sec".to_string()]),
            used_configs: BTreeSet::new(),
            used_secrets: BTreeSet::new(),
        });
        let r = put_outputs(&mut s, "app/web", "r1", &[("oidc-sec", "x")], &[], vec![]);
        assert!(r.error.unwrap().contains("bundled file of stack bundler"));
        assert!(
            put_outputs(&mut s, "app/web", "r1", &[], &[("oidc-id", "x")], vec![])
                .error
                .is_some()
        );
        assert!(s.secrets.is_empty() && s.configs.is_empty());
    }

    fn written(s: &mut ClusterState) {
        assert!(put_outputs(
            s,
            "app/web",
            "r1",
            &[("oidc-sec", "x")],
            &[("oidc-id", "y")],
            vec![]
        )
        .error
        .is_none());
    }

    #[test]
    fn dropping_a_binding_deletes_what_the_module_wrote() {
        let mut s = outputs_state();
        written(&mut s);
        // the config binding goes, the secret stays bound
        deploy(&mut s, "app", "services:\n  web:\n    image: x\n    x-clouduler:\n      terraform:\n        outputs:\n          - {output: sec, secret: oidc-sec}\n  db:\n    image: y\n");
        assert!(!s.configs.contains_key("oidc-id"));
        assert!(s.secrets.contains_key("oidc-sec"));
        assert_eq!(
            owned(&s, "app/web"),
            BTreeSet::from([OutputObject::secret("oidc-sec")])
        );
        // an unchanged redeploy releases nothing
        deploy(&mut s, "app", "services:\n  web:\n    image: x\n    x-clouduler:\n      terraform:\n        outputs:\n          - {output: sec, secret: oidc-sec}\n  db:\n    image: y\n");
        assert!(s.secrets.contains_key("oidc-sec"));
        // no bindings left
        deploy(&mut s, "app", SPEC);
        assert!(s.secrets.is_empty());
        assert!(s.terraform_outputs.is_empty());
    }

    #[test]
    fn a_still_referenced_object_is_kept_when_only_the_ownership_goes() {
        let mut s = outputs_state();
        written(&mut s);
        deploy(&mut s, "graf", "services:\n  g:\n    image: x\n");
        s.apply(&Request::SetStackObjects {
            name: "graf".into(),
            configs: BTreeSet::new(),
            secrets: BTreeSet::new(),
            used_configs: BTreeSet::new(),
            used_secrets: BTreeSet::from(["oidc-sec".to_string()]),
        });
        deploy(&mut s, "app", SPEC);
        assert!(s.secrets.contains_key("oidc-sec"), "grafana still uses it");
        assert!(!s.configs.contains_key("oidc-id"));
        assert!(s.terraform_outputs.is_empty(), "no dangling ownership");
    }

    #[test]
    fn the_producing_stacks_own_use_keeps_an_object_too() {
        let mut s = outputs_state();
        written(&mut s);
        // web no longer maps the secret but another service of the same stack mounts it
        deploy(
            &mut s,
            "app",
            "services:\n  web:\n    image: x\n  db:\n    image: y\n    secrets: [oidc-sec]\n",
        );
        assert!(s.secrets.contains_key("oidc-sec"));
        assert!(!s.configs.contains_key("oidc-id"));
        assert!(s.terraform_outputs.is_empty());
    }

    #[test]
    fn dropping_the_module_or_the_service_or_the_stack_deletes_the_objects() {
        for how in 0..3 {
            let mut s = outputs_state();
            written(&mut s);
            match how {
                0 => assert!(set(&mut s, &[]).error.is_none()),
                1 => deploy(&mut s, "app", "services:\n  db:\n    image: y\n"),
                _ => {
                    s.apply(&Request::RemoveStack { name: "app".into() });
                }
            }
            assert!(s.secrets.is_empty(), "case {how}");
            assert!(s.configs.is_empty(), "case {how}");
            assert!(s.terraform_outputs.is_empty(), "case {how}");
        }
    }

    #[test]
    fn removing_the_stack_keeps_an_object_another_stack_uses() {
        let mut s = outputs_state();
        written(&mut s);
        deploy(&mut s, "graf", "services:\n  g:\n    image: x\n");
        s.apply(&Request::SetStackObjects {
            name: "graf".into(),
            configs: BTreeSet::new(),
            secrets: BTreeSet::new(),
            used_configs: BTreeSet::from(["oidc-id".to_string()]),
            used_secrets: BTreeSet::new(),
        });
        s.apply(&Request::RemoveStack { name: "app".into() });
        assert!(s.configs.contains_key("oidc-id"));
        assert!(!s.secrets.contains_key("oidc-sec"));
        assert!(s.terraform_outputs.is_empty());
    }

    #[test]
    fn removing_an_owned_object_leaves_no_dangling_ownership() {
        let mut s = outputs_state();
        written(&mut s);
        assert!(s
            .apply(&Request::RemoveSecret {
                name: "oidc-sec".into()
            })
            .error
            .is_none());
        assert_eq!(
            owned(&s, "app/web"),
            BTreeSet::from([OutputObject::config("oidc-id")])
        );
        assert!(s
            .apply(&Request::RemoveConfig {
                name: "oidc-id".into()
            })
            .error
            .is_none());
        assert!(s.terraform_outputs.is_empty());
    }
}
