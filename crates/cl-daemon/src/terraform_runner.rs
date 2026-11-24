//! OpenTofu runner: applies the per-service modules stored in raft once their
//! target container is up, and again whenever something they depend on changes.
//!
//! ## Who decides, who runs
//!
//! Instance health is only known to the node that runs the container (raft
//! carries just `running` through `container_status`), so the due-check is
//! **per node**: every node looks at the modules whose *target instance* (the
//! lowest-index assigned, running instance, see [`pick_target`]) lives on it,
//! checks that container's docker health locally, and *claims* the run with
//! `RecordTerraformRun{Running}`. Raft refuses a second `Running` run for the
//! same service, so two nodes can never both run one module (the claim is the
//! lock). The claiming node then executes the run itself.
//!
//! The leader additionally fails runs stuck in `Running` past
//! `timeout + STALE_GRACE` (the executing node crashed), and every node fails
//! its own orphaned runs (`Running`, claimed by it, not in flight here: the
//! daemon restarted mid-run).
//!
//! ## A run
//!
//! 1. a 0700 work dir `<data_dir>/tofu/work/<run-id>` gets `main.tf`, the
//!    unsealed previous state, and the `path:` configs;
//! 2. `tofu init` runs in a container on `terraform.init_network` (it needs the
//!    internet for providers) with a per-node plugin cache and **no** secrets;
//! 3. `tofu plan|apply` runs in a second container sharing the target's network
//!    namespace (`container:<name>`), with `TF_VAR_*` from secrets/configs/vars;
//! 4. after an Apply the state file is sealed with the cluster key and stored with
//!    a compare-and-set (also after a failed or timed-out apply, since partial
//!    state must not be lost);
//! 5. the log is redacted of injected secret values and capped; the outcome is
//!    recorded and, for a failed Apply, a `terraform.failed` alert is raised.
//!
//! Helper containers carry only the `clouduler.terraform-run` label (never
//! `clouduler.managed`), so the reconciler ignores them. Leftovers and work dirs
//! are swept at startup. All blocking fs work runs under `spawn_blocking`.

use crate::supervisor::log_err;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cl_config::{StackFile, TerraformConfig, TerraformService, TfOutputBinding, TfOutputTarget};
use cl_docker::{
    BindMount, ContainerSpec, Docker, DockerHandle, HealthState, LogOptions, RestartPolicy,
    LABEL_TERRAFORM_RUN,
};
use cl_raft::ids::InstanceId;
use cl_raft::node::ClusterNode;
use cl_raft::state::{base64_encode, ClusterState, DesiredState, Request, MAX_OBJECT_BYTES};
use cl_raft::terraform::{
    terraform_key, OutputKind, OutputObject, TerraformConfigOutput, TerraformMode, TerraformModule,
    TerraformOutcome, TerraformRun, TerraformSecretOutput, MAX_TERRAFORM_LOG_BYTES,
};
use cl_sched::reconcile::LABEL_STACK_REVISION;
use cl_secret::{ClusterKey, Sealed};
use futures::StreamExt;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// How long past `timeout` a `Running` run may live before the leader fails it.
const STALE_GRACE_SECS: i64 = 90;
/// A `Running` run claimed by this node and not in flight here is an orphan once it is this old.
const ORPHAN_GRACE_SECS: i64 = 20;
/// Time `tofu` gets to write its state after a SIGTERM on timeout.
const STOP_GRACE: Duration = Duration::from_secs(30);
/// Raw log bytes kept while a phase runs (the tail), before redaction and the final cap.
const LOG_CAPTURE_BYTES: usize = 256 * 1024;
const WORK_MOUNT: &str = "/work";
const CACHE_MOUNT: &str = "/plugin-cache";

// ---------------------------------------------------------------- decisions

/// The service's `x-clouduler.terraform` block (defaults when absent) and the image reference
/// it runs, from the stack's stored spec.
fn service_terraform(
    state: &ClusterState,
    stack: &str,
    service: &str,
) -> Option<(TerraformService, String)> {
    let parsed = StackFile::parse(&state.stacks.get(stack)?.spec).ok()?;
    let svc = parsed.services.get(service)?;
    let tf = svc
        .x_clouduler
        .as_ref()
        .and_then(|x| x.terraform.clone())
        .unwrap_or_default();
    Some((tf, svc.image.clone()))
}

/// The instance whose network namespace a run uses: the lowest-index instance of the service
/// that is assigned to run, not mid-migration, and reported running by its node.
pub fn pick_target<'a>(
    state: &'a ClusterState,
    stack: &str,
    service: &str,
) -> Option<(&'a InstanceId, &'a cl_raft::state::Assignment)> {
    state
        .assignments
        .iter()
        .filter(|(id, a)| {
            id.stack == stack
                && id.service == service
                && a.desired == DesiredState::Running
                && a.migration.is_none()
                && state
                    .container_status
                    .get(*id)
                    .is_some_and(|c| c.running && c.node == a.node)
        })
        .min_by_key(|(id, _)| id.index)
}

fn part(h: &mut blake3::Hasher, bytes: &[u8]) {
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

/// Hash of everything that should trigger a re-apply when it changes: the module text, the
/// service's `x-clouduler.terraform` block, the revision of each bound secret and config, and
/// the image. A container restart alone changes none of these.
pub fn fingerprint(
    module: &TerraformModule,
    tf: &TerraformService,
    state: &ClusterState,
    image: &str,
) -> String {
    let mut h = blake3::Hasher::new();
    part(&mut h, b"clouduler-terraform-fingerprint-v1");
    part(&mut h, module.source.as_bytes());
    part(&mut h, &serde_json::to_vec(tf).unwrap_or_default());
    for s in &tf.secrets {
        part(&mut h, s.source.as_bytes());
        let rev = state.secrets.get(&s.source).map(|e| e.revision);
        part(&mut h, format!("{rev:?}").as_bytes());
    }
    for c in &tf.configs {
        part(&mut h, c.source.as_bytes());
        let rev = state.configs.get(&c.source).map(|e| e.revision);
        part(&mut h, format!("{rev:?}").as_bytes());
    }
    part(&mut h, image.as_bytes());
    h.finalize().to_hex().to_string()
}

/// Error prefix set only by the orphan and stale-run paths (never by a real tofu failure).
const INTERRUPTED_PREFIX: &str = "interrupted:";
/// Trigger of the single automatic retry after an interrupted apply.
const RETRY_TRIGGER: &str = "retry-after-interrupt";

/// A run cut short by a daemon restart or a lost node, not failed by OpenTofu.
fn is_interrupted(run: &TerraformRun) -> bool {
    matches!(&run.outcome, TerraformOutcome::Failed { error } if error.starts_with(INTERRUPTED_PREFIX))
}

/// Whether a run is due, and why. A pending request always wins. Otherwise an Apply is due when
/// the fingerprint differs from the last successful Apply, unless the last Apply for this exact
/// fingerprint failed: no retry loop, and a plan in between never lifts that. A failed Plan never
/// blocks. An apply interrupted by a restart is retried once (trigger `retry-after-interrupt`);
/// a second interruption blocks like any failure.
pub fn due(
    state: &ClusterState,
    key: &str,
    fingerprint: &str,
) -> Option<(TerraformMode, &'static str)> {
    let last = state.terraform_runs.get(key);
    if last.is_some_and(|r| r.is_running()) {
        return None;
    }
    if let Some(mode) = state.terraform_requests.get(key) {
        return Some((*mode, "manual"));
    }
    if state.terraform_applied.get(key).map(String::as_str) == Some(fingerprint) {
        return None;
    }
    // The last apply, not the latest run: a plan in between must not lift the block. Snapshots
    // from before `terraform_last_apply` fall back to the latest run.
    let last_apply = state
        .terraform_last_apply
        .get(key)
        .or(last.filter(|r| r.mode == TerraformMode::Apply));
    if let Some(r) = last_apply.filter(|r| {
        r.fingerprint == fingerprint
            && r.mode == TerraformMode::Apply
            && matches!(r.outcome, TerraformOutcome::Failed { .. })
    }) {
        if !is_interrupted(r) || r.trigger == RETRY_TRIGGER {
            return None;
        }
        return Some((TerraformMode::Apply, RETRY_TRIGGER));
    }
    Some((TerraformMode::Apply, "change"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub key: String,
    /// `StackEntry::revision` when chosen; the target must carry it (see [`target_is_current`]).
    pub stack_revision: u64,
    pub instance: InstanceId,
    pub fingerprint: String,
    pub mode: TerraformMode,
    pub trigger: &'static str,
}

/// Whether any instance of `stack` is mid image update, redeploy or migration. A run started then
/// would be claimed against a container that is about to be replaced.
pub fn rollout_in_progress(state: &ClusterState, stack: &str) -> bool {
    state
        .instance_updates
        .iter()
        .any(|(id, u)| id.stack == stack && !u.phase.is_terminal())
        || state.assignments.iter().any(|(id, a)| {
            id.stack == stack && a.migration.as_ref().is_some_and(|m| !m.phase.is_terminal())
        })
}

/// Whether a container was created for the stack's current revision. A container without the label
/// (created before it existed) is given the benefit of the doubt.
pub fn target_is_current(
    labels: &std::collections::HashMap<String, String>,
    stack_revision: u64,
) -> bool {
    labels
        .get(LABEL_STACK_REVISION)
        .and_then(|r| r.parse::<u64>().ok())
        .is_none_or(|r| r >= stack_revision)
}

/// Modules that are due and whose target instance runs on `node`. Nothing is due while the stack is
/// rolling out (manual requests included: they stay pending and run once it settles).
pub fn candidates(state: &ClusterState, node: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (stack, modules) in &state.terraform_modules {
        for (service, module) in modules {
            let Some((tf, spec_image)) = service_terraform(state, stack, service) else {
                continue;
            };
            if !tf.enabled || rollout_in_progress(state, stack) {
                continue;
            }
            let Some((id, assignment)) = pick_target(state, stack, service) else {
                continue;
            };
            if assignment.node != node {
                continue;
            }
            let image = assignment.image_digest.clone().unwrap_or(spec_image);
            let fp = fingerprint(module, &tf, state, &image);
            let key = terraform_key(stack, service);
            if let Some((mode, trigger)) = due(state, &key, &fp) {
                out.push(Candidate {
                    key,
                    stack_revision: state.stacks.get(stack).map_or(0, |s| s.revision),
                    instance: id.clone(),
                    fingerprint: fp,
                    mode,
                    trigger,
                });
            }
        }
    }
    out
}

/// Runs still `Running` past `timeout + STALE_GRACE_SECS`.
pub fn stale_runs(state: &ClusterState, now: i64, timeout: Duration) -> Vec<TerraformRun> {
    let limit = timeout.as_secs() as i64 + STALE_GRACE_SECS;
    state
        .terraform_runs
        .values()
        .filter(|r| r.is_running() && now - r.started_at > limit)
        .cloned()
        .collect()
}

// ------------------------------------------------------------------- inputs

/// Everything a run needs, resolved from raft (secrets opened) before any container starts.
pub struct RunInputs {
    pub run_id: String,
    pub stack: String,
    pub service: String,
    pub instance: InstanceId,
    pub source: String,
    /// `(var, value)`
    pub secrets: Vec<(String, String)>,
    pub config_vars: Vec<(String, String)>,
    /// `(relative path, content)`
    pub config_files: Vec<(String, String)>,
    pub vars: BTreeMap<String, String>,
    pub state: Option<Vec<u8>>,
    pub state_serial: u64,
    /// `x-clouduler.terraform.outputs`: written back after a successful Apply.
    pub outputs: Vec<TfOutputBinding>,
}

impl RunInputs {
    pub fn secret_values(&self) -> Vec<String> {
        self.secrets.iter().map(|(_, v)| v.clone()).collect()
    }
}

pub fn build_inputs(
    state: &ClusterState,
    key: &ClusterKey,
    run: &TerraformRun,
) -> Result<RunInputs, String> {
    let (stack, service) = (&run.instance.stack, &run.instance.service);
    let tkey = terraform_key(stack, service);
    let module = state
        .terraform_modules
        .get(stack)
        .and_then(|m| m.get(service))
        .ok_or_else(|| format!("{tkey} has no terraform module"))?;
    let (tf, _) = service_terraform(state, stack, service)
        .ok_or_else(|| format!("stack {stack} has no service {service}"))?;

    let mut secrets = Vec::new();
    for b in &tf.secrets {
        let entry = state
            .secrets
            .get(&b.source)
            .ok_or_else(|| format!("secret {} does not exist", b.source))?;
        let plain = Sealed::from_blob(entry.sealed.clone())
            .open(key, &b.source)
            .map_err(|e| format!("secret {} could not be opened: {e}", b.source))?;
        let value = String::from_utf8(plain)
            .map_err(|_| format!("secret {} is not valid UTF-8", b.source))?;
        secrets.push((b.var.clone(), value));
    }
    let mut config_vars = Vec::new();
    let mut config_files = Vec::new();
    for b in &tf.configs {
        let entry = state
            .configs
            .get(&b.source)
            .ok_or_else(|| format!("config {} does not exist", b.source))?;
        let text = entry
            .decode()
            .map_err(|e| format!("config {} could not be decoded: {e}", b.source))
            .and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|_| format!("config {} is not valid UTF-8", b.source))
            })?;
        if let Some(var) = &b.var {
            config_vars.push((var.clone(), text.clone()));
        }
        if let Some(path) = &b.path {
            config_files.push((path.clone(), text));
        }
    }
    let (prior, serial) = match state.terraform_states.get(&tkey) {
        Some(s) => {
            let plain = Sealed::from_blob(s.sealed.clone())
                .open(key, &tkey)
                .map_err(|e| format!("the stored state could not be opened: {e}"))?;
            (Some(plain), s.serial)
        }
        None => (None, 0),
    };
    Ok(RunInputs {
        run_id: run.id.clone(),
        stack: stack.clone(),
        service: service.clone(),
        instance: run.instance.clone(),
        source: module.source.clone(),
        secrets,
        config_vars,
        config_files,
        vars: tf.vars.clone(),
        state: prior,
        state_serial: serial,
        outputs: tf.outputs.clone(),
    })
}

/// `TF_VAR_*` for the plan/apply container. The `clouduler_*` variables win over user ones.
pub fn tf_env(inputs: &RunInputs) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut put = |k: &str, v: &str| {
        env.insert(format!("TF_VAR_{k}"), v.to_string());
    };
    for (k, v) in &inputs.vars {
        put(k, v);
    }
    for (k, v) in inputs.config_vars.iter().chain(&inputs.secrets) {
        put(k, v);
    }
    put("clouduler_stack", &inputs.stack);
    put("clouduler_service", &inputs.service);
    put("clouduler_instance", &inputs.instance.to_string());
    env.into_iter().collect()
}

/// A binding path joined under `dir`, refusing anything that would escape it.
fn safe_join(dir: &Path, rel: &str) -> std::io::Result<PathBuf> {
    let p = Path::new(rel);
    let ok = !rel.is_empty() && p.components().all(|c| matches!(c, Component::Normal(_)));
    if !ok {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("config path {rel:?} is not a plain relative path"),
        ));
    }
    Ok(dir.join(p))
}

/// Creates the 0700 work dir and fills it. Blocking.
pub fn write_workdir(dir: &Path, inputs: &RunInputs) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let write_file = |path: &Path, bytes: &[u8]| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)
    };

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    write_file(&dir.join("main.tf"), inputs.source.as_bytes())?;
    if let Some(state) = &inputs.state {
        write_file(&dir.join("terraform.tfstate"), state)?;
    }
    for (rel, content) in &inputs.config_files {
        write_file(&safe_join(dir, rel)?, content.as_bytes())?;
    }
    Ok(())
}

// ----------------------------------------------------------------- the logs

/// Keeps the last `cap` bytes pushed to it.
pub struct LogTail {
    buf: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl LogTail {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            cap,
            truncated: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > self.cap * 2 {
            let drop = self.buf.len() - self.cap;
            self.buf.drain(..drop);
            self.truncated = true;
        }
    }

    pub fn push_line(&mut self, text: &str) {
        self.push(text.as_bytes());
        self.push(b"\n");
    }

    /// The tail as text; when bytes were dropped, the cut line is dropped too so no
    /// half of an injected value survives at the boundary.
    pub fn into_text(self) -> String {
        let mut buf = self.buf;
        let mut truncated = self.truncated;
        if buf.len() > self.cap {
            buf.drain(..buf.len() - self.cap);
            truncated = true;
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        if truncated {
            match text.find('\n') {
                Some(i) => text[i + 1..].to_string(),
                None => String::new(),
            }
        } else {
            text
        }
    }
}

/// Replaces every injected secret value (also in its JSON-escaped form) with a marker.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut needles: Vec<String> = Vec::new();
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        needles.push(s.clone());
        if let Ok(j) = serde_json::to_string(s) {
            let inner = j[1..j.len() - 1].to_string();
            if inner != *s {
                needles.push(inner);
            }
        }
    }
    needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
    needles.dedup();
    let mut out = text.to_string();
    for n in needles {
        out = out.replace(&n, "[REDACTED]");
    }
    out
}

/// A one-line result from tofu's output.
pub fn summarize(log: &str) -> String {
    for line in log.lines().rev() {
        let l = line.trim();
        if l.starts_with("Apply complete!")
            || l.starts_with("No changes.")
            || l.starts_with("Plan:")
        {
            return l.to_string();
        }
    }
    "completed".to_string()
}

fn first_error(log: &str) -> Option<String> {
    log.lines()
        .map(str::trim)
        .find(|l| l.starts_with("Error:"))
        .map(|l| l.chars().take(300).collect())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// -------------------------------------------------------------- the docker side

struct PhaseResult {
    exit_code: Option<i64>,
    timed_out: bool,
}

fn helper_spec(
    name: &str,
    run_id: &str,
    image: &str,
    network: String,
    env: Vec<(String, String)>,
    command: Vec<&str>,
    mounts: Vec<BindMount>,
) -> ContainerSpec {
    ContainerSpec {
        name: name.to_string(),
        image: image.to_string(),
        stack: "terraform".to_string(),
        service: "terraform".to_string(),
        instance: run_id.to_string(),
        env,
        entrypoint: Some(vec!["tofu".to_string()]),
        command: Some(command.into_iter().map(String::from).collect()),
        working_dir: Some(WORK_MOUNT.to_string()),
        cap_drop: vec!["ALL".to_string()],
        labels: [(LABEL_TERRAFORM_RUN.to_string(), run_id.to_string())].into(),
        mounts,
        network: Some(network),
        restart_policy: RestartPolicy::No,
        oneoff: true,
        no_new_privileges: true,
        ..Default::default()
    }
}

/// Runs one helper container to completion (or the deadline), appending its output to `log`.
/// The container is always removed.
async fn run_phase(
    docker: &Docker,
    spec: ContainerSpec,
    deadline: tokio::time::Instant,
    log: &mut LogTail,
) -> Result<PhaseResult, String> {
    let name = spec.name.clone();
    log_err(
        "removing a stale terraform helper container failed",
        docker.remove(&name, true).await,
    );
    docker
        .create(&spec)
        .await
        .map_err(|e| format!("could not create the {name} container: {e}"))?;
    let result = async {
        if let Err(e) = docker.start(&name).await {
            let mut msg = format!("could not start the {name} container: {e}");
            if let Some(line) = docker.start_failure_line(&name).await {
                msg = format!("{msg} ({})", line.text);
            }
            return Err(msg);
        }
        match tokio::time::timeout_at(deadline, docker.wait(&name)).await {
            Ok(Ok(code)) => Ok(PhaseResult {
                exit_code: Some(code),
                timed_out: false,
            }),
            Ok(Err(e)) => Err(format!("waiting for {name} failed: {e}")),
            Err(_) => {
                if let Err(e) = docker.stop(&name, STOP_GRACE).await {
                    warn!(container = %name, error = %e, "could not stop a timed-out terraform container");
                }
                Ok(PhaseResult {
                    exit_code: None,
                    timed_out: true,
                })
            }
        }
    }
    .await;

    let mut logs = Box::pin(docker.logs(&name, &LogOptions::default()));
    while let Some(line) = logs.next().await {
        match line {
            Ok(l) => log.push_line(&l.text),
            Err(_) => break,
        }
    }
    drop(logs);
    if let Err(e) = docker.remove(&name, true).await {
        debug!(container = %name, error = %e, "could not remove a terraform container");
    }
    result
}

// ------------------------------------------------------------------ outputs

/// `outputs` of a tfstate file as the text a binding writes: a string value verbatim, any other
/// value JSON-encoded.
pub fn state_outputs(state: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let v: serde_json::Value = serde_json::from_slice(state)
        .map_err(|e| format!("the tofu state is not valid JSON: {e}"))?;
    let mut out = BTreeMap::new();
    if let Some(map) = v.get("outputs").and_then(|o| o.as_object()) {
        for (name, entry) in map {
            let Some(value) = entry.get("value") else {
                continue;
            };
            let text = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.insert(name.clone(), text);
        }
    }
    Ok(out)
}

/// The values bound to secrets that the state holds (a missing one is reported elsewhere).
pub fn secret_output_values(bindings: &[TfOutputBinding], state: &[u8]) -> Vec<String> {
    let Ok(outputs) = state_outputs(state) else {
        return Vec::new();
    };
    bindings
        .iter()
        .filter(|b| b.secret.is_some())
        .filter_map(|b| outputs.get(&b.output).cloned())
        .collect()
}

/// Each binding's target object with the value to write. A mapped output the state lacks fails.
pub fn resolve_outputs(
    bindings: &[TfOutputBinding],
    state: &[u8],
) -> Result<Vec<(OutputObject, String)>, String> {
    let outputs = state_outputs(state)?;
    let mut out = Vec::new();
    for b in bindings {
        let target = match b.target() {
            Some(TfOutputTarget::Secret(n)) => OutputObject::secret(n),
            Some(TfOutputTarget::Config(n)) => OutputObject::config(n),
            None => {
                return Err(format!(
                    "output {} has no secret or config target",
                    b.output
                ))
            }
        };
        let value = outputs.get(&b.output).ok_or_else(|| {
            let have: Vec<&str> = outputs.keys().map(String::as_str).collect();
            format!(
                "the module has no output {:?} (the state holds: {})",
                b.output,
                if have.is_empty() {
                    "none".to_string()
                } else {
                    have.join(", ")
                }
            )
        })?;
        out.push((target, value.clone()));
    }
    Ok(out)
}

/// What to submit for a run's outputs, decided against the current secrets and configs.
#[derive(Debug, Default)]
pub struct OutputPlan {
    pub secrets: Vec<TerraformSecretOutput>,
    pub configs: Vec<TerraformConfigOutput>,
    /// Existing, unchanged objects this module does not yet own.
    pub claims: Vec<OutputObject>,
    /// Names only, for the run log.
    pub unchanged: Vec<String>,
}

impl OutputPlan {
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty() && self.configs.is_empty() && self.claims.is_empty()
    }

    pub fn changed_secrets(&self) -> Vec<String> {
        self.secrets.iter().map(|s| s.name.clone()).collect()
    }

    pub fn changed_configs(&self) -> Vec<String> {
        self.configs.iter().map(|c| c.name.clone()).collect()
    }
}

/// Compares each value with the stored object (secrets opened, configs by bytes) and keeps only
/// the ones that differ, so an unchanged output never bumps a revision.
pub fn plan_outputs(
    state: &ClusterState,
    key: &ClusterKey,
    tkey: &str,
    resolved: &[(OutputObject, String)],
) -> Result<OutputPlan, String> {
    let owned = state.terraform_outputs.get(tkey);
    let mut plan = OutputPlan::default();
    for (obj, value) in resolved {
        if value.len() > MAX_OBJECT_BYTES - 1024 {
            return Err(format!(
                "output for {} {} is over the {} byte limit",
                kind_label(obj.kind),
                obj.name,
                MAX_OBJECT_BYTES - 1024
            ));
        }
        let target = match obj.kind {
            OutputKind::Secret => TfOutputTarget::Secret(&obj.name),
            OutputKind::Config => TfOutputTarget::Config(&obj.name),
        };
        if cl_api::routes::objects::is_reserved_output_target(target) {
            return Err(format!(
                "{} {} is a reserved name, a module output cannot write it",
                kind_label(obj.kind),
                obj.name
            ));
        }
        let same = match obj.kind {
            OutputKind::Secret => state.secrets.get(&obj.name).is_some_and(|e| {
                Sealed::from_blob(e.sealed.clone())
                    .open(key, &obj.name)
                    .is_ok_and(|p| p == value.as_bytes())
            }),
            OutputKind::Config => state
                .configs
                .get(&obj.name)
                .is_some_and(|e| e.decode().is_ok_and(|b| b == value.as_bytes())),
        };
        if same {
            plan.unchanged
                .push(format!("{} {}", kind_label(obj.kind), obj.name));
            if !owned.is_some_and(|set| set.contains(obj)) {
                plan.claims.push(obj.clone());
            }
            continue;
        }
        match obj.kind {
            OutputKind::Secret => {
                let sealed = Sealed::seal(key, &obj.name, value.as_bytes())
                    .map_err(|e| format!("could not seal secret {}: {e}", obj.name))?;
                plan.secrets.push(TerraformSecretOutput {
                    name: obj.name.clone(),
                    sealed: sealed.blob,
                    key_fingerprint: key.fingerprint(),
                });
            }
            OutputKind::Config => plan.configs.push(TerraformConfigOutput {
                name: obj.name.clone(),
                content: base64_encode(value.as_bytes()),
            }),
        }
    }
    Ok(plan)
}

fn kind_label(kind: OutputKind) -> &'static str {
    match kind {
        OutputKind::Secret => "secret",
        OutputKind::Config => "config",
    }
}

/// What the docker side of a run produced.
pub struct TofuResult {
    /// `Ok(summary)` or `Err(error)`.
    pub outcome: Result<String, String>,
    /// Redacted, capped to `MAX_TERRAFORM_LOG_BYTES`.
    pub log: String,
    /// The state file after an Apply, when it exists and differs from the one given.
    pub new_state: Option<Vec<u8>>,
    /// The state file after an Apply (changed or not): where the outputs are read from.
    pub final_state: Option<Vec<u8>>,
}

fn cap_text(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

/// Prepares the work dir, then init + plan/apply, then reads the state back and removes the
/// work dir. Touches no raft state.
pub async fn run_tofu(
    docker: &Docker,
    data_dir: &Path,
    cfg: &TerraformConfig,
    mode: TerraformMode,
    inputs: &RunInputs,
) -> TofuResult {
    let started = tokio::time::Instant::now();
    let deadline = started + cfg.timeout;
    let work = data_dir.join("tofu").join("work").join(&inputs.run_id);
    let cache = data_dir.join("tofu").join("plugin-cache");
    let mut log = LogTail::new(LOG_CAPTURE_BYTES);

    let outcome =
        run_tofu_inner(
            docker,
            &TofuRun {
                work: &work,
                cache: &cache,
                deadline,
            },
            cfg,
            mode,
            inputs,
            &mut log,
        )
        .await;

    let mut new_state = None;
    let mut final_state = None;
    if mode == TerraformMode::Apply {
        let path = work.join("terraform.tfstate");
        let read = tokio::task::spawn_blocking(move || std::fs::read(path)).await;
        if let Ok(Ok(bytes)) = read {
            if !bytes.is_empty() {
                if inputs.state.as_deref() != Some(bytes.as_slice()) {
                    new_state = Some(bytes.clone());
                }
                final_state = Some(bytes);
            }
        }
    }
    // Values bound to secrets are hidden from the log like injected ones.
    let mut hidden = inputs.secret_values();
    if let Some(bytes) = &final_state {
        hidden.extend(secret_output_values(&inputs.outputs, bytes));
    }
    let cleanup = work.clone();
    log_err(
        "removing the terraform work dir failed",
        flatten_blocking(tokio::task::spawn_blocking(move || std::fs::remove_dir_all(cleanup)).await),
    );

    let text = redact(&log.into_text(), &hidden);
    let log = cap_text(text, MAX_TERRAFORM_LOG_BYTES);
    let outcome = match outcome {
        Ok(()) => Ok(summarize(&log)),
        Err(e) => Err(match first_error(&log) {
            Some(first) if !e.contains(&first) => format!("{e}: {first}"),
            _ => e,
        }),
    };
    let outcome = outcome.map_err(|e| redact(&e, &hidden));
    TofuResult {
        outcome,
        log,
        new_state,
        final_state,
    }
}

/// Where and until when one tofu run works.
struct TofuRun<'a> {
    work: &'a Path,
    cache: &'a Path,
    deadline: tokio::time::Instant,
}

/// A `spawn_blocking` result with the join error folded into the io one.
fn flatten_blocking<T>(
    result: Result<std::io::Result<T>, tokio::task::JoinError>,
) -> Result<T, String> {
    match result {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

async fn run_tofu_inner(
    docker: &Docker,
    tofu: &TofuRun<'_>,
    cfg: &TerraformConfig,
    mode: TerraformMode,
    inputs: &RunInputs,
    log: &mut LogTail,
) -> Result<(), String> {
    let TofuRun {
        work,
        cache,
        deadline,
    } = *tofu;
    {
        let (work, cache) = (work.to_path_buf(), cache.to_path_buf());
        let files = RunInputs {
            run_id: inputs.run_id.clone(),
            stack: inputs.stack.clone(),
            service: inputs.service.clone(),
            instance: inputs.instance.clone(),
            source: inputs.source.clone(),
            secrets: Vec::new(),
            config_vars: Vec::new(),
            config_files: inputs.config_files.clone(),
            vars: BTreeMap::new(),
            state: inputs.state.clone(),
            state_serial: inputs.state_serial,
            outputs: Vec::new(),
        };
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&cache)?;
            write_workdir(&work, &files)
        })
        .await
        .map_err(|e| format!("preparing the work dir failed: {e}"))?
        .map_err(|e| format!("could not prepare the work dir: {e}"))?;
    }
    // The containers drop every capability (no DAC override), so they must run as the
    // owner of the 0700 work dir.
    let owner = {
        use std::os::unix::fs::MetadataExt;
        let w = work.to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::metadata(w).map(|m| (m.uid(), m.gid())))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("could not stat the work dir: {e}"))?
    };
    let user = format!("{}:{}", owner.0, owner.1);

    match docker.image_exists(&cfg.image).await {
        Ok(true) => {}
        Ok(false) => {
            // Bounded by the run deadline so a hung pull cannot outlive the leader's stale check.
            tokio::time::timeout_at(deadline, docker.pull(&cfg.image))
                .await
                .map_err(|_| format!("pulling {} timed out after {:?}", cfg.image, cfg.timeout))?
                .map_err(|e| format!("could not pull {}: {e}", cfg.image))?;
        }
        Err(e) => return Err(format!("could not look up image {}: {e}", cfg.image)),
    }

    let mounts = || {
        vec![
            BindMount {
                host_path: work.display().to_string(),
                container_path: WORK_MOUNT.to_string(),
                read_only: false,
            },
            BindMount {
                host_path: cache.display().to_string(),
                container_path: CACHE_MOUNT.to_string(),
                read_only: false,
            },
        ]
    };
    let base_env = || {
        vec![
            ("TF_IN_AUTOMATION".to_string(), "1".to_string()),
            ("TF_PLUGIN_CACHE_DIR".to_string(), CACHE_MOUNT.to_string()),
            ("HOME".to_string(), "/tmp".to_string()),
        ]
    };
    let short = &inputs.run_id[..inputs.run_id.len().min(8)];

    // init: internet, no secrets
    log.push_line("--- tofu init");
    let init = helper_spec(
        &format!("clouduler-tf-{short}-init"),
        &inputs.run_id,
        &cfg.image,
        cfg.init_network.clone(),
        base_env(),
        vec!["init", "-input=false", "-no-color"],
        mounts(),
    );
    let mut init = init;
    init.user = Some(user.clone());
    let r = run_phase(docker, init, deadline, log).await?;
    if r.timed_out {
        return Err(format!("tofu init timed out after {:?}", cfg.timeout));
    }
    if r.exit_code != Some(0) {
        return Err(format!(
            "tofu init exited with code {}",
            r.exit_code.unwrap_or(-1)
        ));
    }

    // plan/apply: the target's network namespace
    let (verb, command): (&str, Vec<&str>) = match mode {
        TerraformMode::Apply => (
            "apply",
            vec!["apply", "-auto-approve", "-input=false", "-no-color"],
        ),
        TerraformMode::Plan => ("plan", vec!["plan", "-input=false", "-no-color"]),
    };
    log.push_line(&format!("--- tofu {verb}"));
    let mut env = base_env();
    env.extend(tf_env(inputs));
    let run = helper_spec(
        &format!("clouduler-tf-{short}-run"),
        &inputs.run_id,
        &cfg.image,
        format!("container:{}", inputs.instance.container_name()),
        env,
        command,
        mounts(),
    );
    let mut run = run;
    run.user = Some(user);
    let r = run_phase(docker, run, deadline, log).await?;
    if r.timed_out {
        return Err(format!("tofu {verb} timed out after {:?}", cfg.timeout));
    }
    if r.exit_code != Some(0) {
        return Err(format!(
            "tofu {verb} exited with code {}",
            r.exit_code.unwrap_or(-1)
        ));
    }
    Ok(())
}

/// Removes leftover helper containers and work dirs from a previous daemon.
async fn sweep(docker: &Docker, data_dir: &Path) {
    match docker.list_labelled(LABEL_TERRAFORM_RUN).await {
        Ok(list) => {
            for c in list {
                info!(container = %c.id, "removing a leftover terraform container");
                log_err(
                    "removing a leftover terraform container failed",
                    docker.remove(&c.id, true).await,
                );
            }
        }
        Err(e) => warn!(error = %e, "could not list leftover terraform containers"),
    }
    let work = data_dir.join("tofu").join("work");
    log_err(
        "removing leftover terraform work dirs failed",
        flatten_blocking(
            tokio::task::spawn_blocking(move || {
                if work.is_dir() {
                    std::fs::remove_dir_all(&work)?;
                }
                Ok(())
            })
            .await,
        ),
    );
}

// -------------------------------------------------------------------- the loop

struct Ctx {
    cluster: Arc<ClusterNode>,
    /// Lets a rewritten secret roll its consumers; `None` on a node whose api did not start.
    api: Option<cl_api::AppState>,
    node: String,
    data_dir: PathBuf,
    in_flight: Mutex<BTreeSet<String>>,
}

/// Everything `start` needs to run the terraform loop.
pub struct StartArgs {
    pub cluster: Arc<ClusterNode>,
    pub node_name: String,
    pub docker: DockerHandle,
    pub key_rx: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    pub config_rx: tokio::sync::watch::Receiver<Arc<cl_config::ClusterConfig>>,
    pub data_dir: PathBuf,
    pub nudge: Arc<Notify>,
    pub api: Option<cl_api::AppState>,
}

pub fn start(args: StartArgs) -> JoinHandle<()> {
    let StartArgs {
        cluster,
        node_name,
        docker,
        key_rx,
        config_rx,
        data_dir,
        nudge,
        api,
    } = args;
    tokio::spawn(async move {
        let ctx = Arc::new(Ctx {
            cluster,
            api,
            node: node_name,
            data_dir,
            in_flight: Mutex::new(BTreeSet::new()),
        });
        let mut swept = false;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
                _ = nudge.notified() => {}
            }
            let Some(docker) = docker.get().await else {
                continue;
            };
            if !swept {
                sweep(&docker, &ctx.data_dir).await;
                swept = true;
            }
            let Some(cfg) = config_rx.borrow().terraform.clone() else {
                continue;
            };
            let Some(key) = key_rx.borrow().clone() else {
                continue;
            };
            pass(&ctx, &docker, &cfg, &key).await;
        }
    })
}

async fn pass(ctx: &Arc<Ctx>, docker: &Docker, cfg: &TerraformConfig, key: &Arc<ClusterKey>) {
    let now = unix_now();
    let node = ctx.node.clone();
    let timeout = cfg.timeout;
    let (stale, orphans, due) = ctx
        .cluster
        .read(|s| {
            let stale = stale_runs(s, now, timeout);
            let orphans: Vec<TerraformRun> = s
                .terraform_runs
                .values()
                .filter(|r| {
                    r.is_running() && r.node == node && now - r.started_at > ORPHAN_GRACE_SECS
                })
                .cloned()
                .collect();
            (stale, orphans, candidates(s, &node))
        })
        .await;

    if ctx.cluster.is_leader() {
        for run in stale {
            finish_failed(
                ctx,
                run,
                format!("{INTERRUPTED_PREFIX} the node running it stopped responding"),
            )
            .await;
        }
    }
    for run in orphans {
        if !ctx.in_flight.lock().contains(&run.id) {
            finish_failed(
                ctx,
                run,
                format!("{INTERRUPTED_PREFIX} the daemon restarted during the run"),
            )
            .await;
        }
    }

    for c in due {
        let Some(target_id) = target_ready(docker, &c.instance, c.stack_revision).await else {
            continue;
        };
        claim_and_spawn(ctx, docker, cfg, key, c, target_id).await;
    }
}

/// The target's container id when it is running, healthy when it has a healthcheck, and created for
/// the stack's current revision.
async fn target_ready(
    docker: &Docker,
    instance: &InstanceId,
    stack_revision: u64,
) -> Option<String> {
    match docker.inspect(&instance.container_name()).await {
        Ok(Some(c))
            if c.running
                && matches!(
                    c.health,
                    None | Some(HealthState::None) | Some(HealthState::Healthy)
                )
                && target_is_current(&c.labels, stack_revision) =>
        {
            Some(c.id)
        }
        _ => None,
    }
}

/// Error recorded for an apply whose target container went away mid-run; it takes the one retry.
const REPLACED_ERROR: &str = "interrupted: the target container was replaced during the run";

/// Whether the claimed container is still the one running under the instance's name.
async fn target_still_there(docker: &Docker, instance: &InstanceId, id: &str) -> bool {
    matches!(docker.inspect(&instance.container_name()).await, Ok(Some(c)) if c.running && c.id == id)
}

async fn finish_failed(ctx: &Ctx, mut run: TerraformRun, error: String) {
    run.finished_at = Some(unix_now());
    run.outcome = TerraformOutcome::Failed { error };
    log_err(
        "recording a failed terraform run failed",
        ctx.cluster
            .submit(Request::RecordTerraformRun { run })
            .await,
    );
}

async fn claim_and_spawn(
    ctx: &Arc<Ctx>,
    docker: &Docker,
    cfg: &TerraformConfig,
    key: &Arc<ClusterKey>,
    c: Candidate,
    target_id: String,
) {
    let run = TerraformRun {
        id: uuid::Uuid::new_v4().to_string(),
        mode: c.mode,
        trigger: c.trigger.to_string(),
        node: ctx.node.clone(),
        instance: c.instance.clone(),
        fingerprint: c.fingerprint.clone(),
        started_at: unix_now(),
        finished_at: None,
        outcome: TerraformOutcome::Running,
        log_tail: String::new(),
    };
    ctx.in_flight.lock().insert(run.id.clone());
    let claimed = match ctx
        .cluster
        .submit(Request::RecordTerraformRun { run: run.clone() })
        .await
    {
        Ok(resp) => resp.error.is_none(),
        Err(_) => false,
    };
    if !claimed {
        ctx.in_flight.lock().remove(&run.id);
        return;
    }
    info!(key = %c.key, run = %run.id, mode = ?c.mode, trigger = c.trigger, "terraform run claimed");
    let (ctx, docker, cfg, key) = (ctx.clone(), docker.clone(), cfg.clone(), key.clone());
    tokio::spawn(async move {
        execute(&ctx, &docker, &cfg, &key, run.clone(), &target_id).await;
        ctx.in_flight.lock().remove(&run.id);
    });
}

/// Writes the run's mapped outputs to cluster secrets and configs, one raft request, only what
/// changed. Returns the log note and the secrets whose value changed.
async fn write_outputs(
    ctx: &Ctx,
    key: &ClusterKey,
    run: &TerraformRun,
    inputs: &RunInputs,
    state: Option<&[u8]>,
) -> Result<(String, Vec<String>, Vec<String>), String> {
    let tkey = run.key();
    let state = state.ok_or("the module left no state file to read its outputs from")?;
    let resolved = resolve_outputs(&inputs.outputs, state)?;
    let plan = ctx
        .cluster
        .read(|s| plan_outputs(s, key, &tkey, &resolved))
        .await?;
    let changed = plan.changed_secrets();
    let changed_configs = plan.changed_configs();
    let mut note = String::from("--- outputs\n");
    for name in &changed {
        note.push_str(&format!("wrote secret {name}\n"));
    }
    for c in &plan.configs {
        note.push_str(&format!("wrote config {}\n", c.name));
    }
    for name in &plan.unchanged {
        note.push_str(&format!("unchanged {name}\n"));
    }
    if plan.is_empty() {
        return Ok((note, changed, changed_configs));
    }
    let resp = ctx
        .cluster
        .submit(Request::PutTerraformOutputs {
            key: tkey,
            run_id: run.id.clone(),
            secrets: plan.secrets,
            configs: plan.configs,
            claims: plan.claims,
        })
        .await
        .map_err(|e| e.to_string())?;
    match resp.error {
        Some(e) => Err(e),
        None => Ok((note, changed, changed_configs)),
    }
}

/// A rewritten secret or config only reaches a container at its next start (a config mount is replaced
/// by rename, so a running container keeps the old file), so the services using it are rolled one at a
/// time (the same roll as `POST /api/secrets/{name}/restart`). Containers that already run the new
/// revision, and ones that predate config tracking, are left alone.
async fn roll_consumers(ctx: &Ctx, tkey: &str, secrets: Vec<String>, configs: Vec<String>) {
    let Some(api) = &ctx.api else {
        warn!(key = %tkey, "objects were rewritten but this node has no api to restart their consumers");
        return;
    };
    for name in secrets {
        let Some(revision) = ctx
            .cluster
            .read(|s| s.secrets.get(&name).map(|e| e.revision))
            .await
        else {
            continue;
        };
        let started = cl_api::routes::secret_rotation::start_roll(api, &name, revision).await;
        info!(key = %tkey, secret = %name, restarting = ?started.restarting, "restarting the users of a rewritten secret");
    }
    for name in configs {
        let Some(revision) = ctx
            .cluster
            .read(|s| s.configs.get(&name).map(|e| e.revision))
            .await
        else {
            continue;
        };
        let started =
            cl_api::routes::secret_rotation::start_config_roll(api, &name, revision).await;
        info!(key = %tkey, config = %name, restarting = ?started.restarting, "restarting the users of a rewritten config");
    }
}

async fn execute(
    ctx: &Ctx,
    docker: &Docker,
    cfg: &TerraformConfig,
    key: &ClusterKey,
    mut run: TerraformRun,
    target_id: &str,
) {
    let tkey = run.key();
    let inputs = ctx.cluster.read(|s| build_inputs(s, key, &run)).await;
    let inputs = match inputs {
        Ok(i) => i,
        Err(e) => {
            finish_failed(ctx, run, e).await;
            return;
        }
    };

    let result = run_tofu(docker, &ctx.data_dir, cfg, run.mode, &inputs).await;
    let mut outcome = result.outcome;

    if let Some(bytes) = result.new_state {
        let saved = if bytes.len() > MAX_OBJECT_BYTES - 1024 {
            Err("the tofu state is over the 1 MiB limit".to_string())
        } else {
            match Sealed::seal(key, &tkey, &bytes) {
                Err(e) => Err(format!("could not seal the state: {e}")),
                Ok(sealed) => ctx
                    .cluster
                    .submit(Request::PutTerraformState {
                        key: tkey.clone(),
                        sealed: sealed.blob,
                        key_fingerprint: key.fingerprint(),
                        expect_serial: inputs.state_serial,
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.error.map_or(Ok(()), Err)),
            }
        };
        if let Err(e) = saved {
            warn!(key = %tkey, error = %e, "terraform state could not be stored");
            let note = format!("the new state could not be stored ({e}); it is lost");
            outcome = match outcome {
                Ok(_) => Err(note),
                Err(prior) => Err(format!("{prior}; {note}")),
            };
        }
    }

    let mut log = result.log;
    if let Err(original) = &outcome {
        if run.mode == TerraformMode::Apply
            && !original.starts_with(INTERRUPTED_PREFIX)
            && !target_still_there(docker, &run.instance, target_id).await
        {
            log = cap_text(format!("{log}--- {original}\n"), MAX_TERRAFORM_LOG_BYTES);
            outcome = Err(REPLACED_ERROR.to_string());
        }
    }
    let mut changed_secrets = Vec::new();
    let mut changed_configs = Vec::new();
    if outcome.is_ok() && run.mode == TerraformMode::Apply && !inputs.outputs.is_empty() {
        match write_outputs(ctx, key, &run, &inputs, result.final_state.as_deref()).await {
            Ok((note, changed, configs)) => {
                log = cap_text(format!("{log}{note}"), MAX_TERRAFORM_LOG_BYTES);
                changed_secrets = changed;
                changed_configs = configs;
            }
            Err(e) => {
                warn!(key = %tkey, error = %e, "terraform outputs could not be written");
                outcome = Err(format!("the module outputs could not be written: {e}"));
            }
        }
    }

    run.finished_at = Some(unix_now());
    run.log_tail = log;
    run.outcome = match &outcome {
        Ok(summary) => TerraformOutcome::Succeeded {
            summary: summary.clone(),
        },
        Err(error) => TerraformOutcome::Failed {
            error: error.clone(),
        },
    };
    let failed_apply = outcome
        .as_ref()
        .err()
        .filter(|_| run.mode == TerraformMode::Apply)
        .cloned();
    let (stack, instance, node) = (
        run.instance.stack.clone(),
        run.instance.to_string(),
        run.node.clone(),
    );
    if let Err(e) = ctx
        .cluster
        .submit(Request::RecordTerraformRun { run })
        .await
    {
        warn!(key = %tkey, error = %e, "could not record the terraform run");
    }
    if !changed_secrets.is_empty() || !changed_configs.is_empty() {
        roll_consumers(ctx, &tkey, changed_secrets, changed_configs).await;
    }
    if let Some(error) = failed_apply {
        let mut s = cl_api::alerts::new_signal(
            "terraform.failed",
            format!("terraform.failed:{tkey}"),
            cl_raft::alerts::SignalAction::Once,
        );
        s.title = format!("Terraform apply failed for {tkey}");
        s.detail = error.chars().take(500).collect();
        s.subject = tkey;
        s.stack = Some(stack);
        s.instance = Some(instance);
        s.node = Some(node);
        cl_api::alerts::push_signal(&ctx.cluster, s).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::{Assignment, ConfigEntry, ContainerStatus, SecretEntry};

    const SPEC: &str = "services:\n  web:\n    image: nginx:1\n    x-clouduler:\n      terraform:\n        secrets:\n          - { source: pw, var: password }\n        configs:\n          - { source: cfg, var: cfg_text }\n          - { source: cfg, path: sub/settings.json }\n        vars: { base: \"http://localhost\" }\n";

    fn module(src: &str) -> TerraformModule {
        TerraformModule {
            source: src.to_string(),
            revision: 1,
        }
    }

    fn assignment(id: &InstanceId, node: &str) -> Assignment {
        Assignment {
            instance: id.clone(),
            node: node.to_string(),
            desired: DesiredState::Running,
            image_digest: None,
            migration: None,
        }
    }

    fn state_with(key: &ClusterKey) -> ClusterState {
        let mut s = ClusterState::default();
        let r = s.apply(&Request::DeployStack {
            name: "app".into(),
            spec: SPEC.into(),
            at: 0,
        });
        assert!(r.error.is_none(), "{:?}", r.error);
        s.configs.insert(
            "cfg".into(),
            ConfigEntry {
                name: "cfg".into(),
                content: base64_encode(b"a: 1\n"),
                revision: 1,
                owner: None,
            },
        );
        s.secrets.insert(
            "pw".into(),
            SecretEntry {
                name: "pw".into(),
                sealed: Sealed::seal(key, "pw", b"hunter2").unwrap().blob,
                revision: 1,
                key_fingerprint: key.fingerprint(),
                owner: None,
            },
        );
        let r = s.apply(&Request::SetStackTerraform {
            stack: "app".into(),
            modules: [("web".to_string(), "# tf".to_string())].into(),
        });
        assert!(r.error.is_none(), "{:?}", r.error);
        s
    }

    fn run_instance(s: &mut ClusterState, index: u32, node: &str, running: bool) -> InstanceId {
        let id = InstanceId::new("app", "web", index);
        s.assignments.insert(id.clone(), assignment(&id, node));
        s.container_status.insert(
            id.clone(),
            ContainerStatus {
                node: node.to_string(),
                running,
                secret_revisions: BTreeMap::new(),
                config_revisions: BTreeMap::new(),
            },
        );
        id
    }

    fn fp(s: &ClusterState) -> String {
        let (tf, image) = service_terraform(s, "app", "web").unwrap();
        fingerprint(&s.terraform_modules["app"]["web"], &tf, s, &image)
    }

    fn a_run(id: InstanceId) -> TerraformRun {
        TerraformRun {
            id: "r1".into(),
            mode: TerraformMode::Apply,
            trigger: "change".into(),
            node: "n1".into(),
            instance: id,
            fingerprint: "f".into(),
            started_at: 0,
            finished_at: None,
            outcome: TerraformOutcome::Running,
            log_tail: String::new(),
        }
    }

    #[test]
    fn fingerprint_is_stable_and_tracks_every_input() {
        let key = ClusterKey::generate();
        let mut s = state_with(&key);
        let base = fp(&s);
        assert_eq!(base, fp(&s), "stable");

        s.configs.get_mut("cfg").unwrap().revision = 2;
        let after_config = fp(&s);
        assert_ne!(base, after_config);

        s.secrets.get_mut("pw").unwrap().revision = 2;
        let after_secret = fp(&s);
        assert_ne!(after_config, after_secret);

        let (tf, _) = service_terraform(&s, "app", "web").unwrap();
        let m = &s.terraform_modules["app"]["web"];
        assert_ne!(
            fingerprint(m, &tf, &s, "nginx:1"),
            fingerprint(m, &tf, &s, "nginx:2"),
            "image"
        );
        assert_ne!(
            fingerprint(&module("a"), &tf, &s, "i"),
            fingerprint(&module("b"), &tf, &s, "i"),
            "source"
        );
        let mut tf2 = tf.clone();
        tf2.vars.insert("x".into(), "y".into());
        assert_ne!(
            fingerprint(m, &tf, &s, "i"),
            fingerprint(m, &tf2, &s, "i"),
            "block"
        );
        let mut tf3 = tf.clone();
        tf3.outputs.push(TfOutputBinding {
            output: "o".into(),
            secret: Some("s".into()),
            config: None,
        });
        assert_ne!(
            fingerprint(m, &tf, &s, "i"),
            fingerprint(m, &tf3, &s, "i"),
            "an outputs mapping change re-applies"
        );
    }

    const STATE: &str = r#"{"version":4,"outputs":{
        "id":{"value":"client-1","type":"string"},
        "sec":{"value":"p\"w\\d","type":"string","sensitive":true},
        "n":{"value":3,"type":"number"},
        "obj":{"value":{"a":[1,2],"b":"x"},"type":["object",{}]}}}"#;

    fn binding(output: &str, secret: Option<&str>, config: Option<&str>) -> TfOutputBinding {
        TfOutputBinding {
            output: output.into(),
            secret: secret.map(Into::into),
            config: config.map(Into::into),
        }
    }

    #[test]
    fn outputs_are_read_from_the_state_strings_raw_and_the_rest_as_json() {
        let all = state_outputs(STATE.as_bytes()).unwrap();
        assert_eq!(all["id"], "client-1");
        assert_eq!(all["sec"], "p\"w\\d");
        assert_eq!(all["n"], "3");
        assert_eq!(all["obj"], r#"{"a":[1,2],"b":"x"}"#);
        assert!(state_outputs(b"{}").unwrap().is_empty(), "no outputs key");
        assert!(state_outputs(b"not json").is_err());

        let bindings = [
            binding("id", None, Some("c")),
            binding("obj", Some("s"), None),
        ];
        let r = resolve_outputs(&bindings, STATE.as_bytes()).unwrap();
        assert_eq!(r[0], (OutputObject::config("c"), "client-1".to_string()));
        assert_eq!(r[1].0, OutputObject::secret("s"));
        let err =
            resolve_outputs(&[binding("ghost", None, Some("c"))], STATE.as_bytes()).unwrap_err();
        assert!(
            err.contains("ghost") && err.contains("id, n, obj, sec"),
            "{err}"
        );
        assert_eq!(
            secret_output_values(
                &[
                    binding("sec", Some("s"), None),
                    binding("id", None, Some("c"))
                ],
                STATE.as_bytes()
            ),
            vec!["p\"w\\d".to_string()],
            "only secret bound values are hidden"
        );
    }

    #[test]
    fn a_secret_bound_output_is_redacted_from_logs() {
        let hidden = secret_output_values(&[binding("sec", Some("s"), None)], STATE.as_bytes());
        let log = r#"sec = "p\"w\\d" and raw p"w\d"#;
        let out = redact(log, &hidden);
        assert!(!out.contains("w\\\\d") && !out.contains("p\"w"), "{out}");
    }

    #[test]
    fn only_changed_outputs_are_planned_and_unowned_ones_are_claimed() {
        let key = ClusterKey::generate();
        let mut s = state_with(&key);
        let tkey = "app/web";
        let resolved = vec![
            (OutputObject::secret("sec"), "v1".to_string()),
            (OutputObject::config("id"), "c1".to_string()),
        ];
        let plan = plan_outputs(&s, &key, tkey, &resolved).unwrap();
        assert_eq!(plan.changed_secrets(), vec!["sec".to_string()]);
        assert_eq!(plan.configs.len(), 1);
        assert!(plan.claims.is_empty() && plan.unchanged.is_empty());
        assert_ne!(
            plan.secrets[0].sealed,
            base64_encode(b"v1"),
            "sealed, not plain"
        );
        assert_eq!(
            Sealed::from_blob(plan.secrets[0].sealed.clone())
                .open(&key, "sec")
                .unwrap(),
            b"v1"
        );

        // the same values already stored by hand: nothing to write, ownership is claimed
        s.secrets.insert(
            "sec".into(),
            SecretEntry {
                name: "sec".into(),
                sealed: Sealed::seal(&key, "sec", b"v1").unwrap().blob,
                revision: 4,
                key_fingerprint: key.fingerprint(),
                owner: None,
            },
        );
        s.configs.insert(
            "id".into(),
            ConfigEntry {
                name: "id".into(),
                content: base64_encode(b"c1"),
                revision: 2,
                owner: None,
            },
        );
        let plan = plan_outputs(&s, &key, tkey, &resolved).unwrap();
        assert!(plan.secrets.is_empty() && plan.configs.is_empty());
        assert_eq!(plan.claims.len(), 2);
        assert_eq!(plan.unchanged.len(), 2);

        // owned and unchanged: nothing at all
        s.terraform_outputs.insert(
            tkey.into(),
            [OutputObject::secret("sec"), OutputObject::config("id")].into(),
        );
        assert!(plan_outputs(&s, &key, tkey, &resolved).unwrap().is_empty());

        // one value changes, a secret sealed under another key counts as changed
        let changed = vec![(OutputObject::secret("sec"), "v2".to_string())];
        assert_eq!(
            plan_outputs(&s, &key, tkey, &changed)
                .unwrap()
                .secrets
                .len(),
            1
        );
        let other = ClusterKey::generate();
        assert_eq!(
            plan_outputs(&s, &other, tkey, &resolved)
                .unwrap()
                .secrets
                .len(),
            1
        );
    }

    #[test]
    fn reserved_and_oversize_output_targets_are_refused() {
        let key = ClusterKey::generate();
        let s = state_with(&key);
        let reserved = vec![(
            OutputObject::config(cl_update::plan::RUN_CONFIG),
            "x".to_string(),
        )];
        assert!(plan_outputs(&s, &key, "app/web", &reserved)
            .unwrap_err()
            .contains("reserved"));
        let big = vec![(OutputObject::config("c"), "x".repeat(MAX_OBJECT_BYTES))];
        assert!(plan_outputs(&s, &key, "app/web", &big)
            .unwrap_err()
            .contains("limit"));
    }

    #[test]
    fn the_target_is_the_lowest_running_index() {
        let key = ClusterKey::generate();
        let mut s = state_with(&key);
        run_instance(&mut s, 0, "n1", false);
        run_instance(&mut s, 2, "n3", true);
        run_instance(&mut s, 1, "n2", true);
        let (id, a) = pick_target(&s, "app", "web").unwrap();
        assert_eq!((id.index, a.node.as_str()), (1, "n2"));

        s.assignments
            .get_mut(&InstanceId::new("app", "web", 1))
            .unwrap()
            .desired = DesiredState::Stopped;
        assert_eq!(pick_target(&s, "app", "web").unwrap().0.index, 2);
        s.container_status
            .get_mut(&InstanceId::new("app", "web", 2))
            .unwrap()
            .node = "elsewhere".into();
        assert!(pick_target(&s, "app", "web").is_none());
    }

    #[test]
    fn due_decisions() {
        let key = ClusterKey::generate();
        let mut s = state_with(&key);
        let id = InstanceId::new("app", "web", 0);
        let k = "app/web";
        assert_eq!(due(&s, k, "f1"), Some((TerraformMode::Apply, "change")));

        s.terraform_applied.insert(k.into(), "f1".into());
        assert_eq!(due(&s, k, "f1"), None);
        assert_eq!(due(&s, k, "f2"), Some((TerraformMode::Apply, "change")));
        s.terraform_requests.insert(k.into(), TerraformMode::Plan);
        assert_eq!(due(&s, k, "f1"), Some((TerraformMode::Plan, "manual")));
        s.terraform_requests.clear();

        let mut run = a_run(id);
        run.fingerprint = "f2".into();
        s.terraform_runs.insert(k.into(), run.clone());
        s.terraform_requests.insert(k.into(), TerraformMode::Apply);
        assert_eq!(due(&s, k, "f2"), None, "never two at once");
        s.terraform_requests.clear();

        run.outcome = TerraformOutcome::Failed { error: "x".into() };
        s.terraform_runs.insert(k.into(), run.clone());
        assert_eq!(due(&s, k, "f2"), None, "no retry loop");

        // A plan, failed or not, after the failed apply must not lift the block.
        s.terraform_last_apply.insert(k.into(), run.clone());
        let mut plan = run.clone();
        plan.mode = TerraformMode::Plan;
        s.terraform_runs.insert(k.into(), plan.clone());
        assert_eq!(due(&s, k, "f2"), None, "failed plan keeps the block");
        plan.outcome = TerraformOutcome::Succeeded {
            summary: "ok".into(),
        };
        s.terraform_runs.insert(k.into(), plan);
        assert_eq!(due(&s, k, "f2"), None, "successful plan keeps the block");
        assert_eq!(due(&s, k, "f3"), Some((TerraformMode::Apply, "change")));
        s.terraform_last_apply.clear();

        // An interrupted apply is retried once, then blocks.
        let mut cut = run.clone();
        cut.outcome = TerraformOutcome::Failed {
            error: format!("{INTERRUPTED_PREFIX} the daemon restarted during the run"),
        };
        s.terraform_runs.insert(k.into(), cut.clone());
        assert_eq!(
            due(&s, k, "f2"),
            Some((TerraformMode::Apply, RETRY_TRIGGER))
        );
        cut.trigger = RETRY_TRIGGER.into();
        s.terraform_runs.insert(k.into(), cut);
        assert_eq!(due(&s, k, "f2"), None, "second interruption blocks");
        s.terraform_runs.insert(k.into(), run);
        assert_eq!(due(&s, k, "f3"), Some((TerraformMode::Apply, "change")));
        s.terraform_requests.insert(k.into(), TerraformMode::Apply);
        assert_eq!(due(&s, k, "f2"), Some((TerraformMode::Apply, "manual")));
    }

    #[test]
    fn no_run_starts_while_the_stack_is_rolling_out() {
        use cl_raft::state::{InstanceUpdate, Migration, MigrationPhase, UpdatePhase};
        let key = ClusterKey::generate();
        let mut s = state_with(&key);
        let id = run_instance(&mut s, 0, "n1", true);
        run_instance(&mut s, 1, "n2", true);
        assert_eq!(candidates(&s, "n1").len(), 1);
        s.terraform_requests
            .insert("app/web".into(), TerraformMode::Apply);

        let update = |phase| InstanceUpdate {
            node: "n2".into(),
            phase,
            started_at: 0,
            at: 0,
            error: None,
        };
        // An update of a sibling instance blocks, a finished one does not.
        s.instance_updates.insert(
            InstanceId::new("app", "web", 1),
            update(UpdatePhase::Starting),
        );
        assert!(rollout_in_progress(&s, "app"));
        assert!(candidates(&s, "n1").is_empty(), "manual requests wait too");
        s.instance_updates.insert(
            InstanceId::new("app", "web", 1),
            update(UpdatePhase::Started),
        );
        assert!(!rollout_in_progress(&s, "app"));
        assert_eq!(candidates(&s, "n1").len(), 1);

        s.assignments.get_mut(&id).unwrap().migration = Some(Migration {
            from: "n1".into(),
            to: "n3".into(),
            phase: MigrationPhase::Syncing,
        });
        assert!(rollout_in_progress(&s, "app"));
        assert!(!rollout_in_progress(&s, "other"));
    }

    #[test]
    fn a_target_from_an_older_stack_revision_is_stale() {
        let mut labels = std::collections::HashMap::new();
        assert!(target_is_current(&labels, 3), "unlabelled containers pass");
        labels.insert(LABEL_STACK_REVISION.to_string(), "2".to_string());
        assert!(!target_is_current(&labels, 3));
        labels.insert(LABEL_STACK_REVISION.to_string(), "3".to_string());
        assert!(target_is_current(&labels, 3));
    }

    #[test]
    fn candidates_carry_the_stack_revision() {
        let key = ClusterKey::generate();
        let mut s = state_with(&key);
        run_instance(&mut s, 0, "n1", true);
        let rev = s.stacks["app"].revision;
        assert_eq!(candidates(&s, "n1")[0].stack_revision, rev);
    }

    #[test]
    fn candidates_belong_to_the_targets_node_and_respect_enabled() {
        let key = ClusterKey::generate();
        let mut s = state_with(&key);
        run_instance(&mut s, 0, "n1", true);
        run_instance(&mut s, 1, "n2", true);
        let on_n1 = candidates(&s, "n1");
        assert_eq!(on_n1.len(), 1);
        assert_eq!(on_n1[0].key, "app/web");
        assert_eq!(on_n1[0].instance.index, 0);
        assert!(
            candidates(&s, "n2").is_empty(),
            "only the target's node decides"
        );

        let spec = SPEC.replace("terraform:\n", "terraform:\n          enabled: false\n");
        s.stacks.get_mut("app").unwrap().spec = spec;
        assert!(candidates(&s, "n1").is_empty());
    }

    #[test]
    fn stale_runs_are_those_past_timeout_plus_grace() {
        let key = ClusterKey::generate();
        let mut s = state_with(&key);
        let id = run_instance(&mut s, 0, "n1", true);
        let timeout = Duration::from_secs(600);
        let mut run = a_run(id);
        run.started_at = 1000;
        s.terraform_runs.insert("app/web".into(), run);
        assert!(stale_runs(&s, 1000 + 600 + STALE_GRACE_SECS, timeout).is_empty());
        assert_eq!(
            stale_runs(&s, 1000 + 601 + STALE_GRACE_SECS, timeout).len(),
            1
        );
    }

    #[test]
    fn redaction_hides_values_and_their_json_escapes() {
        let out = redact(
            "pw=hunter2 json=\"a\\\"b\" again hunter2",
            &["hunter2".into(), "a\"b".into(), String::new()],
        );
        assert_eq!(out, "pw=[REDACTED] json=\"[REDACTED]\" again [REDACTED]");
        assert_eq!(
            redact("abcdef", &["abc".into(), "abcdef".into()]),
            "[REDACTED]"
        );
    }

    #[test]
    fn the_log_keeps_its_tail_without_a_cut_line() {
        let mut l = LogTail::new(20);
        for i in 0..50 {
            l.push_line(&format!("line {i:02}"));
        }
        let text = l.into_text();
        assert!(text.len() <= 20);
        assert!(text.ends_with("line 49\n"));
        assert!(text.starts_with("line "), "{text:?}");

        let mut small = LogTail::new(100);
        small.push_line("hello");
        assert_eq!(small.into_text(), "hello\n");
    }

    #[test]
    fn summaries_pick_tofus_verdict_line() {
        assert_eq!(
            summarize("x\nApply complete! Resources: 1 added, 0 changed, 0 destroyed.\n"),
            "Apply complete! Resources: 1 added, 0 changed, 0 destroyed."
        );
        assert!(summarize("No changes. Your infrastructure matches.").starts_with("No changes."));
        assert_eq!(summarize("nothing"), "completed");
        assert_eq!(
            first_error("a\nError: boom\nmore").as_deref(),
            Some("Error: boom")
        );
    }

    #[test]
    fn inputs_open_secrets_and_the_env_carries_them() {
        let key = ClusterKey::generate();
        let mut s = state_with(&key);
        let id = run_instance(&mut s, 0, "n1", true);
        s.terraform_states.insert(
            "app/web".into(),
            cl_raft::terraform::TerraformState {
                sealed: Sealed::seal(&key, "app/web", b"{\"v\":4}").unwrap().blob,
                serial: 7,
                key_fingerprint: key.fingerprint(),
            },
        );
        let run = a_run(id);
        let inputs = build_inputs(&s, &key, &run).unwrap();
        assert_eq!(inputs.state.as_deref(), Some(&b"{\"v\":4}"[..]));
        assert_eq!(inputs.state_serial, 7);
        assert_eq!(inputs.secret_values(), vec!["hunter2".to_string()]);
        let env: BTreeMap<_, _> = tf_env(&inputs).into_iter().collect();
        assert_eq!(env["TF_VAR_password"], "hunter2");
        assert_eq!(env["TF_VAR_cfg_text"], "a: 1\n");
        assert_eq!(env["TF_VAR_base"], "http://localhost");
        assert_eq!(env["TF_VAR_clouduler_stack"], "app");
        assert_eq!(env["TF_VAR_clouduler_service"], "web");
        assert_eq!(env["TF_VAR_clouduler_instance"], "app.web.0");
        assert_eq!(
            inputs.config_files,
            vec![("sub/settings.json".to_string(), "a: 1\n".to_string())]
        );

        let err = build_inputs(&s, &ClusterKey::generate(), &run)
            .err()
            .unwrap();
        assert!(err.contains("could not be opened"), "{err}");
        s.secrets.clear();
        assert!(build_inputs(&s, &key, &run)
            .err()
            .unwrap()
            .contains("secret pw does not exist"));
    }

    fn bare_inputs(config_files: Vec<(String, String)>) -> RunInputs {
        RunInputs {
            run_id: "r".into(),
            stack: "app".into(),
            service: "web".into(),
            instance: InstanceId::new("app", "web", 0),
            source: "# module".into(),
            secrets: vec![],
            config_vars: vec![],
            config_files,
            vars: BTreeMap::new(),
            state: Some(b"state".to_vec()),
            state_serial: 1,
            outputs: Vec::new(),
        }
    }

    #[test]
    fn the_work_dir_is_private_and_confines_config_paths() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let inputs = bare_inputs(vec![("sub/settings.json".into(), "{}".into())]);
        let dir = tmp.path().join("w");
        write_workdir(&dir, &inputs).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("main.tf")).unwrap(),
            "# module"
        );
        assert_eq!(
            std::fs::read(dir.join("terraform.tfstate")).unwrap(),
            b"state"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/settings.json")).unwrap(),
            "{}"
        );
        assert_eq!(
            std::fs::metadata(dir.join("main.tf"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        for bad in ["../x", "/etc/x", "a/../../x", ""] {
            let inputs = bare_inputs(vec![(bad.into(), "x".into())]);
            assert!(
                write_workdir(&tmp.path().join("w2"), &inputs).is_err(),
                "{bad}"
            );
        }
        assert!(!tmp.path().join("x").exists());
    }

    /// Needs docker and network access to the registries. Run with
    /// cargo test -p cl-daemon terraform_runner -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "needs docker and network access to registries"]
    async fn a_module_runs_in_a_targets_netns_and_a_rerun_is_a_noop() {
        let docker = Docker::connect().expect("docker");
        let target = InstanceId::new("clouduler-tf-it", "web", 0);
        let name = target.container_name();
        let _ = docker.remove(&name, true).await;
        let image = "nginx:alpine";
        if !docker.image_exists(image).await.unwrap() {
            docker.pull(image).await.unwrap();
        }
        docker
            .create(&ContainerSpec {
                name: name.clone(),
                image: image.into(),
                stack: "it".into(),
                service: "web".into(),
                instance: "0".into(),
                oneoff: true,
                ..Default::default()
            })
            .await
            .unwrap();
        docker.start(&name).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        let tmp = tempfile::tempdir().unwrap();
        let cfg = TerraformConfig {
            timeout: Duration::from_secs(300),
            ..TerraformConfig::default()
        };
        let module = r#"
terraform {
  required_providers {
    http = { source = "hashicorp/http" }
  }
}
variable "monitors_yaml" {}
variable "secret_word" { sensitive = true }
locals {
  monitors = yamldecode(var.monitors_yaml)
  settings = jsondecode(file("<<PM>>/settings.json"))
}
data "http" "self" { url = "http://localhost:80/" }
resource "terraform_data" "x" { input = data.http.self.status_code }
output "status" { value = data.http.self.status_code }
output "count" { value = length(local.monitors) }
output "theme" { value = local.settings.theme }
output "echo" {
  value     = var.secret_word
  sensitive = true
}
"#
        .replace("<<PM>>", &["$", "{path.module}"].concat());
        let mut inputs = RunInputs {
            run_id: uuid::Uuid::new_v4().to_string(),
            stack: "clouduler-tf-it".into(),
            service: "web".into(),
            instance: target.clone(),
            source: module,
            secrets: vec![("secret_word".into(), "hunter2-it".into())],
            config_vars: vec![("monitors_yaml".into(), "- a\n- b\n".into())],
            config_files: vec![("settings.json".into(), "{\"theme\":\"dark\"}".into())],
            vars: BTreeMap::new(),
            state: None,
            state_serial: 0,
            outputs: vec![
                binding("theme", None, Some("it-theme")),
                binding("echo", Some("it-echo"), None),
            ],
        };

        let first = run_tofu(&docker, tmp.path(), &cfg, TerraformMode::Apply, &inputs).await;
        println!("{}", first.log);
        assert!(first.outcome.is_ok(), "{:?}", first.outcome);
        let state1 = first.new_state.clone().expect("state after apply");
        let text = String::from_utf8_lossy(&state1).into_owned();
        assert!(
            text.contains("\"status\"") && text.contains("200"),
            "{text}"
        );
        assert!(text.contains("dark"));
        assert!(!first.log.contains("hunter2-it"));
        let resolved = resolve_outputs(
            &inputs.outputs,
            first.final_state.as_deref().expect("final state"),
        )
        .unwrap();
        assert_eq!(
            resolved,
            vec![
                (OutputObject::config("it-theme"), "dark".to_string()),
                (OutputObject::secret("it-echo"), "hunter2-it".to_string()),
            ]
        );

        inputs.run_id = uuid::Uuid::new_v4().to_string();
        inputs.state = Some(state1.clone());
        let second = run_tofu(&docker, tmp.path(), &cfg, TerraformMode::Apply, &inputs).await;
        println!("{}", second.log);
        let summary = second.outcome.clone().expect("second run");
        assert!(
            summary.contains("0 added, 0 changed, 0 destroyed")
                || summary.starts_with("No changes"),
            "{summary}"
        );
        // a no-op apply still yields the outputs
        let again = resolve_outputs(
            &inputs.outputs,
            second
                .final_state
                .as_deref()
                .expect("final state of a no-op apply"),
        )
        .unwrap();
        assert_eq!(again[0].1, "dark");

        inputs.run_id = uuid::Uuid::new_v4().to_string();
        let plan = run_tofu(&docker, tmp.path(), &cfg, TerraformMode::Plan, &inputs).await;
        assert!(plan.outcome.is_ok(), "{:?}", plan.outcome);
        assert!(plan.new_state.is_none());

        let _ = docker.remove(&name, true).await;
        assert!(docker
            .list_labelled(LABEL_TERRAFORM_RUN)
            .await
            .unwrap()
            .is_empty());
        assert!(!tmp.path().join("tofu/work").join(&inputs.run_id).exists());
    }
}
