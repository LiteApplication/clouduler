# Instance Progress Streaming Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the web UI real-time, step-by-step status for redeploys and migrations (e.g. `"redeploying (exited, step 2/3)"`, `"migrating (syncing volume 1/2, 5/12 files)"`) instead of only what the next 4-5s poll happens to catch.

**Architecture:** Two node-local progress trackers — a redeploy-step map in `cl-sched`'s reconciler and a migration-sync-progress map in `cl-drive`'s migration runner — feed a new `reason`/`step` field on the existing `InstanceSummary` payload (computed in `cl-api::routes::instances::summarize_local`, reusing the existing local/remote cross-node forwarding). A new `GET /api/instances/events` SSE endpoint polls that same computation every second and emits only the instances whose `(state, reason, step)` changed, and the UI overlays it on top of its existing polled views.

**Tech Stack:** Rust (axum, tokio, serde) across `cl-sched`, `cl-drive`, `cl-api`, `clouduler`; React/TypeScript in `ui/`.

## Global Constraints

- Follow this codebase's doc-comment style: prose comments explain *why*, not *what* — match the tone already in the files you're editing.
- Run `cargo clippy --workspace --all-targets -D warnings` before considering any Rust task done (project convention).
- Never leave a tracker entry behind on error: every `insert` this plan adds has a matching removal on every exit path (mirrors the existing `PullTracker` pattern in `ensure_image`).
- This plan does **not** touch the standalone `POST /api/volumes/{drive}/{volume}/sync` background replication path — only migration's own `Syncing` phase gets live progress. Attributing a background volume sync to one instance would need a volume→instance mapping that does not exist today; out of scope.
- At the end (Task 11), remove `docs/superpowers/specs/2026-09-04-instance-progress-streaming-design.md` from the repository — the user asked not to leave the design doc committed.

---

## Task 1: `cl-drive` progress-reporting primitive

**Files:**
- Create: `crates/cl-drive/src/progress.rs`
- Modify: `crates/cl-drive/src/lib.rs`

**Interfaces:**
- Produces: `pub struct ProgressSink` with `ProgressSink::none() -> Self`, `ProgressSink::new(f: impl Fn(u64, u64) + Send + Sync + 'static) -> Self`, `fn report(&self, done: u64, total: u64)`. Used by Task 2 (`SyncEngine::sync`) and Task 3 (`MigrationJob`).

- [ ] **Step 1: Write the failing test**

Create `crates/cl-drive/src/progress.rs`:

```rust
//! A plain callback for reporting "N of M done" from inside `SyncEngine`,
//! decoupled from *what* collects it. `delta.rs` calls `report` without
//! knowing whether anyone is listening; `migration.rs` (the one caller that
//! cares, for now) supplies a closure that writes into its own tracker.
//! Kept a synchronous `Fn` rather than an async callback so the per-file
//! send loop in `delta.rs` never has to `.await` just to report progress.

use std::sync::Arc;

type ProgressCallback = Arc<dyn Fn(u64, u64) + Send + Sync>;

#[derive(Clone, Default)]
pub struct ProgressSink(Option<ProgressCallback>);

impl ProgressSink {
    /// No one is listening — the cost of calling `report` is one branch.
    pub fn none() -> Self {
        Self(None)
    }

    pub fn new(f: impl Fn(u64, u64) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(f)))
    }

    /// `done` out of `total`, in whatever unit the caller and its listener
    /// agreed on (this crate always uses file counts, not bytes).
    pub fn report(&self, done: u64, total: u64) {
        if let Some(f) = &self.0 {
            f(done, total);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn none_reports_nothing_and_does_not_panic() {
        let sink = ProgressSink::none();
        sink.report(1, 2);
    }

    #[test]
    fn a_sink_forwards_done_and_total_to_its_callback() {
        let done = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let (d, t) = (done.clone(), total.clone());
        let sink = ProgressSink::new(move |done, total| {
            d.store(done, Ordering::SeqCst);
            t.store(total, Ordering::SeqCst);
        });
        sink.report(3, 7);
        assert_eq!(done.load(Ordering::SeqCst), 3);
        assert_eq!(total.load(Ordering::SeqCst), 7);
    }

    #[test]
    fn a_cloned_sink_calls_the_same_callback() {
        let calls = Arc::new(AtomicU64::new(0));
        let c = calls.clone();
        let sink = ProgressSink::new(move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        let cloned = sink.clone();
        sink.report(0, 1);
        cloned.report(0, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p cl-drive progress::`
Expected: 3 tests pass (the module has no prior implementation to be missing — this is new code, so write it and the tests together, then run once to confirm both compile and pass).

- [ ] **Step 3: Wire the module into the crate**

In `crates/cl-drive/src/lib.rs`, add `pub mod progress;` to the `pub mod` block and `pub use progress::ProgressSink;` alongside the other re-exports:

```rust
pub mod progress;
```
(insert next to `pub mod migration;`)

```rust
pub use progress::ProgressSink;
```
(insert next to `pub use migration::{InstanceControl, MigrationJob};`)

- [ ] **Step 4: Run the full crate test suite**

Run: `cargo test -p cl-drive`
Expected: PASS, no new warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/cl-drive/src/progress.rs crates/cl-drive/src/lib.rs
git commit -m "feat(cl-drive): add ProgressSink for reporting sync progress"
```

---

## Task 2: Thread `ProgressSink` through `SyncEngine`/`DeltaEngine`

**Files:**
- Modify: `crates/cl-drive/src/delta.rs`
- Modify: `crates/cl-drive/src/sync.rs`

**Interfaces:**
- Consumes: `cl_drive::ProgressSink` (Task 1).
- Produces: `SyncEngine::sync` now takes a 5th parameter `progress: ProgressSink`; `DeltaEngine::sync_impl` reports `(done, total)` in **file counts** (added + changed files) after computing the diff and after each file finishes. Task 3's `MigrationJob` and Task 4's callers rely on this exact signature.

- [ ] **Step 1: Update the trait and its one real implementation**

In `crates/cl-drive/src/delta.rs`, change the trait (around line 74-84):

```rust
pub trait SyncEngine: Send + Sync {
    fn name(&self) -> &str;

    fn sync<'a>(
        &'a self,
        src: &'a Path,
        peer: SocketAddr,
        volume: &'a VolumeId,
        generation: u64,
        progress: crate::progress::ProgressSink,
    ) -> BoxFuture<'a, Result<SyncStats>>;
}
```

and `impl SyncEngine for DeltaEngine` (around line 98-112):

```rust
impl SyncEngine for DeltaEngine {
    fn name(&self) -> &str {
        "delta"
    }

    fn sync<'a>(
        &'a self,
        src: &'a Path,
        peer: SocketAddr,
        volume: &'a VolumeId,
        generation: u64,
        progress: crate::progress::ProgressSink,
    ) -> BoxFuture<'a, Result<SyncStats>> {
        Box::pin(async move { self.sync_impl(src, peer, volume, generation, progress).await })
    }
}
```

- [ ] **Step 2: Report progress from `sync_impl`**

Still in `delta.rs`, change `sync_impl`'s signature (around line 144-150):

```rust
    async fn sync_impl(
        &self,
        src: &Path,
        peer: SocketAddr,
        volume: &VolumeId,
        generation: u64,
        progress: crate::progress::ProgressSink,
    ) -> Result<SyncStats> {
```

Right after `let diff = local.diff(&remote);` (line 170), report the total:

```rust
        let diff = local.diff(&remote);
        let total_files = (diff.added.len() + diff.changed.len()) as u64;
        let mut files_done: u64 = 0;
        progress.report(0, total_files);
```

In the `for path in &diff.added` loop, right after `stats.files_added += 1;` (line 296-297):

```rust
            self.send_whole(&mut conn, src, path, meta, &mut stats).await?;
            stats.files_added += 1;
            files_done += 1;
            progress.report(files_done, total_files);
```

In the `for path in &diff.changed` loop, right after `stats.files_changed += 1;` (line 309):

```rust
            stats.files_changed += 1;
            files_done += 1;
            progress.report(files_done, total_files);
```

- [ ] **Step 3: Update the one non-migration call site**

In `crates/cl-drive/src/sync.rs`, `run_one` calls `self.engine.sync(&src, peer, &id, generation).await` (line ~199). This path (the standalone/background per-drive replica sync) does not attribute progress to anything per this plan's scope — pass `crate::progress::ProgressSink::none()`:

```rust
            match self.engine.sync(&src, peer, &id, generation, crate::progress::ProgressSink::none()).await {
```

- [ ] **Step 4: Fix the now-broken call in `migration.rs` to compile (real wiring is Task 3)**

In `crates/cl-drive/src/migration.rs`, `sync_all` calls `job.engine.sync(&src_path, job.target_addr, volume, generation).await?;` (line 174). Temporarily pass `crate::progress::ProgressSink::none()` so the crate compiles; Task 3 replaces this with the real sink:

```rust
        job.engine.sync(&src_path, job.target_addr, volume, generation, crate::progress::ProgressSink::none()).await?;
```

- [ ] **Step 5: Run the crate's tests**

Run: `cargo test -p cl-drive`
Expected: PASS. Any fake `SyncEngine` implementations inside `#[cfg(test)]` modules (`grep -rn "impl SyncEngine" crates/cl-drive/src` to check for any missed by the earlier survey) need the same signature change — if the compiler flags one, add the `progress` parameter and ignore it (`let _ = progress;`) unless the test already asserts something about progress.

- [ ] **Step 6: Commit**

```bash
git add crates/cl-drive/src/delta.rs crates/cl-drive/src/sync.rs crates/cl-drive/src/migration.rs
git commit -m "feat(cl-drive): report file-count progress from DeltaEngine::sync"
```

---

## Task 3: Migration progress tracker

**Files:**
- Modify: `crates/cl-drive/src/progress.rs`
- Modify: `crates/cl-drive/src/migration.rs`
- Modify: `crates/cl-drive/src/lib.rs`

**Interfaces:**
- Produces: `pub struct MigrationSyncProgress { pub volume_index: u64, pub volume_count: u64, pub files_done: u64, pub files_total: u64 }`, `pub type MigrationSyncTracker = Arc<std::sync::RwLock<HashMap<InstanceId, MigrationSyncProgress>>>`. `MigrationJob` gains `pub sync_progress: Option<MigrationSyncTracker>`. Task 5 (daemon wiring) constructs the tracker and passes it in; Task 6 (`cl-api`) reads it.
- Consumes: `ProgressSink` (Task 1/2).

- [ ] **Step 1: Add the tracker type**

Append to `crates/cl-drive/src/progress.rs`:

```rust
use std::collections::HashMap;
use std::sync::RwLock;

use cl_raft::ids::InstanceId;

/// A migrating instance's current position: which of its volumes is being
/// synced right now, and that volume's own file-count progress. Plain
/// `std::sync::RwLock`, not `tokio`'s: it is only ever held for the instant
/// it takes to update or read one entry, from both a synchronous
/// `ProgressSink` callback (Task 2) and an async reader (`cl-api`), and a
/// blocking lock is simpler than making the callback async just to `.await`
/// a tokio lock it holds for microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationSyncProgress {
    /// 1-based: "syncing volume 2 of 3".
    pub volume_index: u64,
    pub volume_count: u64,
    pub files_done: u64,
    pub files_total: u64,
}

/// Shared with `cl-api` so it can report live migration-sync progress
/// without either crate depending on the other's internals — see
/// `cl_sched::PullTracker` for the same sharing pattern applied to image
/// pulls.
pub type MigrationSyncTracker = Arc<RwLock<HashMap<InstanceId, MigrationSyncProgress>>>;
```

- [ ] **Step 2: Give `MigrationJob` a tracker slot**

In `crates/cl-drive/src/migration.rs`, add a field to `MigrationJob` (around line 37-52):

```rust
    pub control: &'a dyn InstanceControl,
    /// Where to publish live sync progress for `job.instance`, if anyone is
    /// watching. `None` in every test and in any caller that doesn't care.
    pub sync_progress: Option<crate::progress::MigrationSyncTracker>,
}
```

- [ ] **Step 2: Write the failing test**

`sync_all` itself needs a real `ClusterNode` and transport listener to exercise (this module's existing test comment says as much: "full phase-ordering behaviour needs a running raft node and a real transport listener" and defers that to a heavier test than this file carries — see `grep -n "mod tests" -A 20 crates/cl-drive/src/migration.rs`). So rather than testing `sync_all` directly, factor its per-volume sink construction out into its own small pure function and test *that* — it's the only part of this task with real logic (the aggregation math), and it needs neither a node nor a transport:

```rust
/// Builds the closure `sync_all` hands to `ProgressSink` for one volume of
/// a migration: which volume this is (`volume_index`/`volume_count`) never
/// changes once the sync for it starts, so it's captured once here rather
/// than threaded through every `ProgressSink::report` call.
fn migration_progress_writer(
    tracker: crate::progress::MigrationSyncTracker,
    instance: InstanceId,
    volume_index: u64,
    volume_count: u64,
) -> crate::progress::ProgressSink {
    crate::progress::ProgressSink::new(move |files_done, files_total| {
        tracker.write().unwrap().insert(
            instance.clone(),
            crate::progress::MigrationSyncProgress { volume_index, volume_count, files_done, files_total },
        );
    })
}
```

Add to the `#[cfg(test)]` module at the bottom of `crates/cl-drive/src/migration.rs`:

```rust
    #[test]
    fn migration_progress_writer_records_the_volume_position_and_file_counts() {
        let tracker: crate::progress::MigrationSyncTracker = Arc::default();
        let instance = InstanceId::new("site", "web", 0);
        let sink = migration_progress_writer(tracker.clone(), instance.clone(), 2, 3);

        sink.report(5, 12);

        let recorded = tracker.read().unwrap().get(&instance).copied().unwrap();
        assert_eq!(recorded.volume_index, 2);
        assert_eq!(recorded.volume_count, 3);
        assert_eq!(recorded.files_done, 5);
        assert_eq!(recorded.files_total, 12);
    }

    #[test]
    fn a_later_report_overwrites_the_instance_s_entry() {
        let tracker: crate::progress::MigrationSyncTracker = Arc::default();
        let instance = InstanceId::new("site", "web", 0);
        let sink = migration_progress_writer(tracker.clone(), instance.clone(), 1, 1);

        sink.report(1, 4);
        sink.report(4, 4);

        assert_eq!(tracker.read().unwrap().get(&instance).unwrap().files_done, 4);
    }
```

- [ ] **Step 3: Run to see it fail**

Run: `cargo test -p cl-drive migration::`
Expected: FAIL to compile — `migration_progress_writer` doesn't exist yet.

- [ ] **Step 4: Implement `migration_progress_writer` and wire it into `sync_all`**

Add `migration_progress_writer` (shown in Step 2) to `crates/cl-drive/src/migration.rs`, above `sync_all`. Then replace `sync_all` (around line 164-186, up to its closing brace) — keep every line of its existing body that isn't shown as changed below, only touching the `engine.sync` call and adding the tracker cleanup at the end:

```rust
async fn sync_all(node: &ClusterNode, job: &MigrationJob<'_>) -> Result<()> {
    let volume_count = job.volumes.len() as u64;
    for (idx, volume) in job.volumes.iter().enumerate() {
        let state = node.state().await;
        let generation = state
            .volumes
            .get(volume)
            .ok_or_else(|| DriveError::VolumeNotFound { volume: volume.clone() })?
            .generation;

        let src_path = job.require_registry()?.volume_path(volume)?;
        let progress = match &job.sync_progress {
            Some(tracker) => {
                migration_progress_writer(tracker.clone(), job.instance.clone(), idx as u64 + 1, volume_count)
            }
            None => crate::progress::ProgressSink::none(),
        };
        job.engine.sync(&src_path, job.target_addr, volume, generation, progress).await?;

        node.submit(Request::SetVolumeReplica {
            id: volume.clone(),
            node: job.target_node.clone(),
            generation,
        })
        .await
        .map_err(|e| DriveError::Transfer { message: e.to_string() })?;
    }
    if let Some(tracker) = &job.sync_progress {
        tracker.write().unwrap().remove(&job.instance);
    }
    Ok(())
}
```

(If the original loop body — read it fresh with `sed -n '164,195p' crates/cl-drive/src/migration.rs` before editing — has lines after the `submit` call inside the loop that weren't visible in this plan's earlier read, preserve them unchanged; only the `engine.sync` call itself and the new post-loop cleanup block are new.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p cl-drive`
Expected: PASS.

- [ ] **Step 6: Export from lib.rs**

In `crates/cl-drive/src/lib.rs`, extend the progress re-export from Task 1:

```rust
pub use progress::{MigrationSyncProgress, MigrationSyncTracker, ProgressSink};
```

- [ ] **Step 7: Commit**

```bash
git add crates/cl-drive/src/progress.rs crates/cl-drive/src/migration.rs crates/cl-drive/src/lib.rs
git commit -m "feat(cl-drive): publish live per-volume progress during a migration's sync phase"
```

---

## Task 4: Redeploy sub-step tracking in `cl-sched`

**Files:**
- Modify: `crates/cl-sched/src/reconcile.rs`
- Modify: `crates/cl-sched/src/run.rs`

**Interfaces:**
- Produces: `pub enum RedeployPhase { Pulling, Stopping, Starting }` with `pub fn step_number(self) -> u8`, `pub const REDEPLOY_TOTAL_STEPS: u8 = 3`, `pub type RedeployTracker = Arc<tokio::sync::RwLock<HashMap<InstanceId, RedeployPhase>>>`. `Reconciler::new` gains a `redeploy_steps: RedeployTracker` parameter. `Scheduler` gains a `redeploy_steps: RedeployTracker` field, constructed by the caller (Task 5) and passed to `Scheduler::new`.
- Consumes: nothing new.

- [ ] **Step 1: Add the tracker type**

In `crates/cl-sched/src/reconcile.rs`, right after the existing `PullTracker` type alias (line 23), add:

```rust
/// Which real docker step a redeploy (`Action::Recreate`, or `Action::Start`'s
/// stale-network recovery — both go through `replace`) is currently on for
/// one instance. Shared with the api the same way `PullTracker` is, so
/// `GET /api/instances` can say "step 2/3" instead of just "exited".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedeployPhase {
    /// Fetching the new image, before anything about the running container
    /// changes — matches `replace`'s actual order: the old container stays
    /// up as long as possible.
    Pulling,
    /// Old container stopped and removed, new one not created yet.
    Stopping,
    /// New container created, starting.
    Starting,
}

impl RedeployPhase {
    pub fn step_number(self) -> u8 {
        match self {
            RedeployPhase::Pulling => 1,
            RedeployPhase::Stopping => 2,
            RedeployPhase::Starting => 3,
        }
    }
}

pub const REDEPLOY_TOTAL_STEPS: u8 = 3;

pub type RedeployTracker = Arc<RwLock<BTreeMap<InstanceId, RedeployPhase>>>;
```

(Uses the `BTreeMap` already imported at the top of the file, matching `PullTracker`'s `BTreeSet` style — add `BTreeMap` to the existing `use std::collections::{BTreeMap, BTreeSet};` line if it isn't already there; check line 6.)

- [ ] **Step 2: Add the field and constructor parameter**

In `Reconciler` (around line 142-155):

```rust
pub struct Reconciler {
    docker: Docker,
    pulling: PullTracker,
    redeploy_steps: RedeployTracker,
    registries: Registries,
}

impl Reconciler {
    pub fn new(docker: Docker, pulling: PullTracker, redeploy_steps: RedeployTracker) -> Self {
        Self { docker, pulling, redeploy_steps, registries: Registries::default() }
    }
```

- [ ] **Step 3: Set/clear the step around `replace`**

Replace the existing `replace` method (lines 314-327) with:

```rust
    /// Stops (if running), force-removes, then creates and starts a fresh
    /// container from `target`'s current spec. Shared by `Action::Recreate`
    /// (the spec itself changed) and `Action::Start`'s stale-network
    /// recovery: both need the old container gone before docker will let a
    /// new one attach cleanly, and `create` picks up whatever the network
    /// currently is rather than whatever it was when this container was
    /// last built.
    async fn replace(&self, id: &InstanceId, target: &Target, name: &str) -> Result<()> {
        let result = self.replace_inner(id, target, name).await;
        self.redeploy_steps.write().await.remove(id);
        result
    }

    async fn replace_inner(&self, id: &InstanceId, target: &Target, name: &str) -> Result<()> {
        self.redeploy_steps.write().await.insert(id.clone(), RedeployPhase::Pulling);
        self.ensure_image(id, &target.spec.image).await?;

        self.redeploy_steps.write().await.insert(id.clone(), RedeployPhase::Stopping);
        self.docker.stop(name, stop_timeout(Some(target))).await.ok();
        self.docker.remove(name, true).await.ok();

        self.redeploy_steps.write().await.insert(id.clone(), RedeployPhase::Starting);
        let container_id = self.docker.create(&target.spec).await?;
        if let Some(egress) = &target.spec.egress_network {
            self.docker.connect_network(&container_id, egress, None).await?;
        }
        self.docker.start(name).await?;
        if let Some(egress) = &target.spec.egress_network {
            self.docker.set_default_route(name, egress).await?;
        }
        Ok(())
    }
```

- [ ] **Step 4: Thread the tracker through `Scheduler`**

In `crates/cl-sched/src/run.rs`:

Import (line 26): change
```rust
use crate::reconcile::{self, Action, PullTracker, Reconciler, Target, DEFAULT_STOP_GRACE};
```
to
```rust
use crate::reconcile::{self, Action, PullTracker, RedeployTracker, Reconciler, Target, DEFAULT_STOP_GRACE};
```

Add a field to `Scheduler` (after `pulling: PullTracker,` around line 65):

```rust
    pulling: PullTracker,
    /// Which redeploy step each instance being recreated is on, shared with
    /// the api. See `cl_sched::RedeployTracker`.
    redeploy_steps: RedeployTracker,
```

Add a constructor parameter to `Scheduler::new` (around line 75-93):

```rust
    pub fn new(
        node: String,
        cluster: Arc<ClusterNode>,
        docker: Arc<RwLock<Option<Docker>>>,
        materializer: Materializer,
        boot_config: ClusterConfig,
        pulling: PullTracker,
        redeploy_steps: RedeployTracker,
    ) -> Self {
        Self {
            node,
            cluster,
            docker,
            materializer,
            zone: None,
            boot_config,
            pulling,
            redeploy_steps,
            restarts: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }
```

Update the `Reconciler::new` call in `reconcile` (around line 212-213):

```rust
        let reconciler =
            Reconciler::new(docker.clone(), self.pulling.clone(), self.redeploy_steps.clone())
                .with_registries(registries);
```

- [ ] **Step 5: Fix existing test call sites**

`cargo build -p cl-sched --tests 2>&1 | grep -n "Scheduler::new\|Reconciler::new"` will show every test constructing either type without the new argument. For each, pass `Arc::default()` for the new `redeploy_steps` parameter — the same way those tests already pass `Arc::default()` for `pulling`.

- [ ] **Step 6: Run tests**

Run: `cargo test -p cl-sched`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/cl-sched/src/reconcile.rs crates/cl-sched/src/run.rs
git commit -m "feat(cl-sched): track which redeploy sub-step each recreate is on"
```

---

## Task 5: Wire both trackers through the daemon

**Files:**
- Modify: `crates/clouduler/src/daemon.rs`
- Modify: `crates/clouduler/src/migration.rs`

**Interfaces:**
- Consumes: `cl_sched::RedeployTracker` (Task 4), `cl_drive::MigrationSyncTracker` (Task 3).
- Produces: both trackers reachable from `AppState` via `ApiServerOptions` (Task 6 adds the fields there).

- [ ] **Step 1: Create both trackers in `daemon.rs`**

Right after the existing `let pulling: cl_sched::PullTracker = Arc::default();` (line 142), add:

```rust
    // Which redeploy step each instance being recreated is on, and how far
    // along an in-progress migration's volume sync is — both shared with the
    // api the same way `pulling` is, so it can report live progress instead
    // of only a coarse docker state.
    let redeploy_steps: cl_sched::RedeployTracker = Arc::default();
    let migration_sync: cl_drive::MigrationSyncTracker = Arc::default();
```

- [ ] **Step 2: Pass `redeploy_steps` into the scheduler**

Update the `Scheduler::new` call (around line 201-208):

```rust
    let scheduler = Scheduler::new(
        node_name.clone(),
        cluster.clone(),
        docker.clone(),
        materializer,
        cfg.clone(),
        pulling.clone(),
        redeploy_steps.clone(),
    )
    .with_zone(cl_net::dns::zone_origin(&cfg));
```

- [ ] **Step 3: Pass `migration_sync` into `crate::migration::start`**

Update the `crate::migration::start` call (around line 229-235):

```rust
    let migrations = crate::migration::start(
        config_rx.clone(),
        &node_name,
        &cluster,
        docker.clone(),
        drives.registry(),
        migration_sync.clone(),
    );
```

In `crates/clouduler/src/migration.rs`, add a parameter to `start` (around line 43-49):

```rust
pub fn start(
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
    docker: Arc<tokio::sync::RwLock<Option<Docker>>>,
    registry: Arc<tokio::sync::RwLock<Option<Arc<DriveRegistry>>>>,
    sync_progress: cl_drive::MigrationSyncTracker,
) -> JoinHandle<()> {
```

Find where this function constructs `MigrationJob { ... }` (search `grep -n "MigrationJob {" crates/clouduler/src/migration.rs`) and add the new field, cloning `sync_progress` into the async block the same way `engine`/`registry` already are:

```rust
            sync_progress: Some(sync_progress.clone()),
```

(If `sync_progress` isn't already captured by the `tokio::spawn(async move { ... })` closure at the top of `start`, add it to that closure's captures the same way `node_name`/`cluster` are cloned in before the `tokio::spawn` at the top of the function.)

- [ ] **Step 4: Pass both trackers into `StartApiArgs`/`start_api`**

In `daemon.rs`, add fields to `StartApiArgs` (around line 290-305):

```rust
    pulling: cl_sched::PullTracker,
    redeploy_steps: cl_sched::RedeployTracker,
    migration_sync: cl_drive::MigrationSyncTracker,
```

Add them to the `start_api(StartApiArgs { ... })` call (around line 164-180):

```rust
    let api = start_api(StartApiArgs {
        cfg: &cfg,
        node,
        node_name: &node_name,
        config_path: &config_path,
        cluster: &cluster,
        data_dir: &data_dir,
        docker: docker.clone(),
        pulling: pulling.clone(),
        redeploy_steps: redeploy_steps.clone(),
        migration_sync: migration_sync.clone(),
        key: key.clone(),
        key_tx,
        shutdown: shutdown_tx,
        http: internal_http.clone(),
        node_keypair,
        proxy_health: proxy.health.clone(),
    })
    .await;
```

Note: this call happens *before* `migration_sync`/`redeploy_steps` are used by the scheduler/migration wiring below it in the function — since both are just `Arc` clones created at the top (Step 1), the ordering is fine; only clone, don't move, at each use site so later steps still have their own copy (matches how `pulling` is already cloned at every use site in this function).

Destructure and forward them in `start_api` itself (around line 307-323):

```rust
async fn start_api(args: StartApiArgs<'_>) -> Option<tokio::task::JoinHandle<()>> {
    let StartApiArgs {
        cfg,
        node: _node,
        node_name,
        config_path,
        cluster,
        data_dir,
        docker,
        pulling,
        redeploy_steps,
        migration_sync,
        key,
        key_tx,
        shutdown,
        http,
        node_keypair,
        proxy_health,
    } = args;
```

and into `ApiServerOptions` (around line 349-360, `let options = cl_api::ApiServerOptions { ... }`):

```rust
    let options = cl_api::ApiServerOptions {
        cluster: cluster.clone(),
        docker,
        pulling,
        redeploy_steps,
        migration_sync,
        node_name: node_name.to_string(),
        config_path: config_path.to_path_buf(),
        cluster_config: cfg.clone(),
        key_path: cfg.secrets.key_file.clone(),
        key,
        key_tx,
        tls_cert_pem: cert_pem.into_bytes(),
        // ...unchanged fields below this point...
```

(Keep every field already there past this point unchanged — only inserting the two new ones.)

- [ ] **Step 5: Build to confirm wiring compiles (cl-api changes land in Task 6)**

This task alone will not compile yet — `cl_api::ApiServerOptions` doesn't have `redeploy_steps`/`migration_sync` fields until Task 6. That's expected: run `cargo check -p clouduler` and confirm the *only* errors are about those two missing fields on `ApiServerOptions` (nothing else). Do not attempt to fix that here.

- [ ] **Step 6: Commit**

```bash
git add crates/clouduler/src/daemon.rs crates/clouduler/src/migration.rs
git commit -m "feat(clouduler): wire redeploy-step and migration-sync trackers through the daemon"
```

---

## Task 6: Surface `reason`/`step` on `InstanceSummary`

**Files:**
- Modify: `crates/cl-api/src/server.rs`
- Modify: `crates/cl-api/src/routes/instances.rs`

**Interfaces:**
- Consumes: `cl_sched::RedeployTracker`, `cl_drive::MigrationSyncTracker` (Tasks 4/5), `cl_raft::state::MigrationPhase` (existing).
- Produces: `InstanceSummary` (wire type) gains `reason: Option<String>` and `step: Option<(u8, u8)>`. This closes out `clouduler`'s build (Task 5's `ApiServerOptions` fields now exist).

- [ ] **Step 1: Add fields to `AppState` and `ApiServerOptions`**

In `crates/cl-api/src/server.rs`, add to `AppState` (after the existing `pulling` field, around line 47):

```rust
    pub pulling: cl_sched::PullTracker,
    /// Which redeploy step each instance being recreated is on. See
    /// `cl_sched::RedeployTracker`.
    pub redeploy_steps: cl_sched::RedeployTracker,
    /// Live per-volume progress for an instance's in-progress migration
    /// sync. See `cl_drive::MigrationSyncTracker`.
    pub migration_sync: cl_drive::MigrationSyncTracker,
```

Add the same two fields to `ApiServerOptions` (after its own `pulling` field, around line 117):

```rust
    pub pulling: cl_sched::PullTracker,
    pub redeploy_steps: cl_sched::RedeployTracker,
    pub migration_sync: cl_drive::MigrationSyncTracker,
```

In `ApiServer::new`, forward them into the `AppState { ... }` literal (around line 180-200):

```rust
        let state = AppState {
            cluster: options.cluster,
            docker: options.docker,
            pulling: options.pulling,
            redeploy_steps: options.redeploy_steps,
            migration_sync: options.migration_sync,
            node_name: options.node_name,
            // ...unchanged fields below this point...
```

Add `cl_drive` as a dependency of `cl-api` if it isn't already one: check `crates/cl-api/Cargo.toml` for `cl-drive.workspace = true` — from the earlier survey `cl-api` already depends on `cl-sched`, `cl-raft`, etc. but the survey did not list `cl-drive`. Run `grep cl-drive crates/cl-api/Cargo.toml`; if absent, add `cl-drive.workspace = true` to `[dependencies]`, matching the style of the other `cl-*.workspace = true` lines.

- [ ] **Step 2: Write the failing test**

Add near the bottom of `crates/cl-api/src/routes/instances.rs` (create a `#[cfg(test)]` module if none exists — check first with `grep -n "mod tests" crates/cl-api/src/routes/instances.rs`; this file has none today, so this is a new module):

```rust
#[cfg(test)]
mod progress_tests {
    use super::*;
    use cl_raft::state::MigrationPhase;

    #[test]
    fn redeploy_reason_names_the_step_and_the_docker_state() {
        let (reason, step) = compose_progress(
            STATE_EXITED,
            Some(reconcile_phase_for_test(cl_sched::RedeployPhase::Stopping)),
            None,
        );
        assert_eq!(reason.as_deref(), Some("redeploying (exited, step 2/3)"));
        assert_eq!(step, Some((2, 3)));
    }

    #[test]
    fn migration_reason_without_sync_progress_names_just_the_phase() {
        let (reason, step) = compose_migration_reason(&MigrationPhase::Verifying, None);
        assert_eq!(reason.as_deref(), Some("migrating (verifying)"));
        assert_eq!(step, None);
    }

    #[test]
    fn migration_reason_with_sync_progress_names_the_volume_and_file_counts() {
        let progress = cl_drive::MigrationSyncProgress {
            volume_index: 1,
            volume_count: 2,
            files_done: 5,
            files_total: 12,
        };
        let (reason, step) = compose_migration_reason(&MigrationPhase::Syncing, Some(progress));
        assert_eq!(reason.as_deref(), Some("migrating (syncing volume 1/2, 5/12 files)"));
        assert_eq!(step, Some((1, 2)));
    }

    fn reconcile_phase_for_test(phase: cl_sched::RedeployPhase) -> cl_sched::RedeployPhase {
        phase
    }
}
```

- [ ] **Step 3: Run to see it fail**

Run: `cargo test -p cl-api progress_tests`
Expected: FAIL to compile — `compose_progress`/`compose_migration_reason` don't exist yet.

- [ ] **Step 4: Implement the composition helpers**

In `crates/cl-api/src/routes/instances.rs`, add these functions near `run_state_to_summary` (after it, around line 248):

```rust
/// One instance's redeploy step, or migration phase, turned into the string
/// the ui shows verbatim and a `(step, total)` pair for anything that wants
/// to render a progress bar instead. `None` for a steady-state instance.
fn compose_progress(
    instance_state: &str,
    redeploy: Option<cl_sched::RedeployPhase>,
    migration: Option<(&cl_raft::state::MigrationPhase, Option<cl_drive::MigrationSyncProgress>)>,
) -> (Option<String>, Option<(u8, u8)>) {
    if let Some((phase, sync_progress)) = migration {
        return compose_migration_reason(phase, sync_progress);
    }
    let phase = redeploy?;
    let step = phase.step_number();
    (
        Some(format!("redeploying ({instance_state}, step {step}/{})", cl_sched::REDEPLOY_TOTAL_STEPS)),
        Some((step, cl_sched::REDEPLOY_TOTAL_STEPS)),
    )
}

fn compose_migration_reason(
    phase: &cl_raft::state::MigrationPhase,
    sync_progress: Option<cl_drive::MigrationSyncProgress>,
) -> (Option<String>, Option<(u8, u8)>) {
    if *phase == cl_raft::state::MigrationPhase::Syncing {
        if let Some(p) = sync_progress {
            return (
                Some(format!(
                    "migrating (syncing volume {}/{}, {}/{} files)",
                    p.volume_index, p.volume_count, p.files_done, p.files_total
                )),
                Some((p.volume_index as u8, p.volume_count as u8)),
            );
        }
    }
    let label = format!("{phase:?}").to_lowercase();
    (Some(format!("migrating ({label})")), None)
}
```

- [ ] **Step 5: Fix the test's first case to call the real signature**

The test in Step 2 called `compose_progress(STATE_EXITED, Some(reconcile_phase_for_test(...)), None)` — with the real signature above, that matches (`migration: None`). Remove the now-unused `reconcile_phase_for_test` helper (it was only there to spell out the type; `Some(cl_sched::RedeployPhase::Stopping)` works directly) — simplify the first test to:

```rust
    #[test]
    fn redeploy_reason_names_the_step_and_the_docker_state() {
        let (reason, step) = compose_progress(STATE_EXITED, Some(cl_sched::RedeployPhase::Stopping), None);
        assert_eq!(reason.as_deref(), Some("redeploying (exited, step 2/3)"));
        assert_eq!(step, Some((2, 3)));
    }
```

Drop the now-unused `reconcile_phase_for_test` function from the test module.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p cl-api progress_tests`
Expected: PASS (3 tests).

- [ ] **Step 7: Wire into `InstanceSummary` and `summarize_local`**

Add fields to `InstanceSummary` (around line 52-86, right after `migration`):

```rust
    migration: Option<MigrationSummary>,
    /// What is actively happening to this instance right now — a redeploy
    /// sub-step or a migration phase — or `None` when it is steady-state.
    /// `state`/`migration` above are the raw facts; this is the same
    /// information composed into one string plus a `(step, total)` pair for
    /// a progress bar. See `compose_progress`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    step: Option<(u8, u8)>,
```

In `summarize_local`, right before the final `InstanceSummary { ... }` literal (around line 191), compute the new fields:

```rust
    let migration_sync = assignment
        .migration
        .as_ref()
        .and_then(|m| state.migration_sync.read().unwrap().get(&assignment.instance).copied());
    let redeploy_step = if local {
        state.redeploy_steps.read().await.get(&assignment.instance).copied()
    } else {
        None
    };
    let (reason, step) = compose_progress(
        instance_state,
        redeploy_step,
        assignment.migration.as_ref().map(|m| (&m.phase, migration_sync)),
    );

    InstanceSummary {
        id: assignment.instance.to_string(),
        node: assignment.node.clone(),
        desired: format!("{:?}", assignment.desired).to_lowercase(),
        state: instance_state.to_string(),
        running,
        health: container
            .as_ref()
            .and_then(|c| c.health.as_ref())
            .map(|h| format!("{h:?}").to_lowercase())
            .unwrap_or_else(|| "none".to_string()),
        image: container.as_ref().map(|c| c.image.clone()),
        image_digest: assignment.image_digest.clone(),
        running_digest,
        started_at: container.as_ref().and_then(|c| c.started_at.clone()),
        migration: assignment.migration.as_ref().map(|m| MigrationSummary {
            to: m.to.clone(),
            phase: format!("{:?}", m.phase).to_lowercase(),
        }),
        placement_error: None,
        proxy,
        reason,
        step,
    }
```

(`compose_progress` is called for every instance, local or not; `redeploy_step` is deliberately `None` on the not-`local` branch since only the owning node's reconciler ever populates its own `redeploy_steps` map — matches how `instance_state` itself already degrades to `STATE_UNKNOWN` for a non-local instance a few lines above.)

- [ ] **Step 8: Run the full crate test suite**

Run: `cargo test -p cl-api`
Expected: PASS. Fix any other call site that constructs an `InstanceSummary` literal directly (grep `InstanceSummary {` in `crates/cl-api/src`) by adding `reason: None, step: None,` if any test does so.

- [ ] **Step 9: Build the whole workspace**

Run: `cargo build --workspace`
Expected: PASS — this is the point where `clouduler`'s Task 5 wiring finally compiles end to end.

- [ ] **Step 10: Commit**

```bash
git add crates/cl-api/src/server.rs crates/cl-api/src/routes/instances.rs crates/cl-api/Cargo.toml
git commit -m "feat(cl-api): compute a live reason/step for each instance's InstanceSummary"
```

---

## Task 7: `GET /api/instances/events` SSE stream

**Files:**
- Modify: `crates/cl-api/src/stream.rs`
- Modify: `crates/cl-api/src/routes/instances.rs`

**Interfaces:**
- Consumes: `summarize` (existing, extended by Task 6), `AppState` (existing).
- Produces: `GET /api/instances/events` — SSE, event name `progress`, payload `{ instance_id, state, reason, step }`, emitted only when an instance's `(state, reason, step)` changed since the last emission on this connection.

- [ ] **Step 1: Add the poll-and-diff stream function**

In `crates/cl-api/src/stream.rs`, add near `cluster_state_events`:

```rust
/// How often `/api/instances/events` recomputes every instance's summary and
/// checks for a change. Faster than `STATE_POLL_INTERVAL` on purpose:
/// unlike raft state (replicated, so every node's view moves in lockstep),
/// redeploy/migration progress is read straight off this node's own
/// in-memory trackers and a live docker inspect, so there is no replication
/// lag to wait out — the only cost of polling faster is the docker inspects
/// themselves, which `summarize_local` already does cheaply enough for the
/// existing 4-5s ui poll to call on every instance, every tick.
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// One instance's live progress, as sent over `/api/instances/events`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InstanceProgressEvent {
    pub instance_id: String,
    pub state: String,
    pub reason: Option<String>,
    pub step: Option<(u8, u8)>,
}

/// SSE stream of `progress` events, one per instance whose `(state, reason,
/// step)` changed since the last time this connection emitted anything for
/// it. A steady instance is sent once (its current value) and then never
/// again until something about it actually changes.
pub fn instance_progress_events(
    state: crate::server::AppState,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = futures::stream::unfold(
        (state, std::collections::HashMap::<String, InstanceProgressEvent>::new()),
        |(state, mut last)| async move {
            let mut interval = tokio::time::interval(PROGRESS_POLL_INTERVAL);
            loop {
                interval.tick().await;
                let cluster_state = state.cluster.state().await;
                for (id, event) in crate::routes::instances::progress_snapshot(&state, &cluster_state).await {
                    if last.get(&id) == Some(&event) {
                        continue;
                    }
                    last.insert(id, event.clone());
                    let sse_event = Event::default()
                        .event("progress")
                        .json_data(&event)
                        .unwrap_or_else(|_| Event::default().event("progress").data("{}"));
                    return Some((Ok(sse_event), (state, last)));
                }
                // Nothing changed this tick; loop back to `interval.tick()`
                // rather than returning, so the connection stays open with
                // no event sent — same shape as `cluster_state_events`.
            }
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}
```

- [ ] **Step 2: Add `progress_snapshot` to `routes::instances`**

In `crates/cl-api/src/routes/instances.rs`, add (near `list`, reusing its concurrent-fetch pattern):

```rust
/// Every instance's current `(state, reason, step)`, for `/api/instances/events`
/// to diff against what it last sent. Reuses `summarize`'s existing
/// local/remote split — a remote instance still gets a real answer via the
/// same internal forwarding `list` already relies on.
pub(crate) async fn progress_snapshot(
    state: &AppState,
    cluster_state: &cl_raft::ClusterState,
) -> Vec<(String, crate::stream::InstanceProgressEvent)> {
    let summaries = futures::future::join_all(cluster_state.assignments.values().map(|a| summarize(state, a))).await;
    summaries
        .into_iter()
        .map(|s| {
            (
                s.id.clone(),
                crate::stream::InstanceProgressEvent {
                    instance_id: s.id,
                    state: s.state,
                    reason: s.reason,
                    step: s.step,
                },
            )
        })
        .collect()
}
```

`InstanceSummary`'s fields (`id`, `state`, `reason`, `step`) are private (`struct InstanceSummary` has no `pub` on its fields) — either make those four fields `pub(crate)` or, simpler, keep them private and construct the tuple by pattern-destructuring is not possible on a private struct from another function in the *same* module (`progress_snapshot` lives in this same file, so it already has access) — no visibility change needed as long as `progress_snapshot` stays in `routes/instances.rs`. Confirm this compiles as-is before making anything `pub(crate)`.

- [ ] **Step 3: Register the route**

In `crates/cl-api/src/routes/instances.rs`, add to `router()` (around line 28-50, alongside the existing `/api/instances/{id}/logs`/`stats` SSE routes):

```rust
        .route("/api/instances/events", get(instance_events))
```

Add the handler near `logs`/`stats`:

```rust
async fn instance_events(_identity: Identity, State(state): State<AppState>) -> impl IntoResponse {
    stream::instance_progress_events(state)
}
```

- [ ] **Step 4: Build and run the crate's tests**

Run: `cargo build -p cl-api && cargo test -p cl-api`
Expected: PASS.

- [ ] **Step 5: Manual smoke check**

This needs a running daemon, which this plan's automated steps can't spin up standalone — note for whoever executes this plan interactively: after the daemon is running locally (see this project's own `run` skill/dev docs), `curl -N -H "Authorization: Bearer <token>" https://localhost:<port>/api/instances/events` should hang open and print nothing until an instance's state actually changes, then print an SSE `event: progress` block. Skip if no local cluster is available at plan-execution time; Task 11's final verification covers the build-level check.

- [ ] **Step 6: Commit**

```bash
git add crates/cl-api/src/stream.rs crates/cl-api/src/routes/instances.rs
git commit -m "feat(cl-api): add GET /api/instances/events, a diffed SSE stream of instance progress"
```

---

## Task 8: UI types

**Files:**
- Modify: `ui/src/api/types.ts`

**Interfaces:**
- Produces: `InstanceState` gains `reason?: string | null; step?: [number, number] | null;`. New `InstanceProgressEvent` interface for the SSE payload. Task 9 imports both.

- [ ] **Step 1: Extend `InstanceState`**

In `ui/src/api/types.ts`, add to the `InstanceState` interface (right after `proxy?: ProxyHealth;`, around line 121):

```typescript
  proxy?: ProxyHealth;
  /** What is actively happening right now — a redeploy sub-step or a
   *  migration phase, composed server-side into one string — or absent when
   *  the instance is steady-state. Only ever set on the node that actually
   *  runs the instance; a summary fetched for one running elsewhere leaves
   *  this absent even mid-operation. */
  reason?: string | null;
  /** `[step, total]` alongside `reason`, for a progress bar; absent when
   *  `reason` describes something with no fixed step count (e.g. a
   *  migration phase other than "syncing"). */
  step?: [number, number] | null;
}
```

- [ ] **Step 2: Add the SSE event type**

Add near `InstanceState`:

```typescript
/** One `progress` event from `GET /api/instances/events`: sent once when an
 *  instance's live status is first seen, then again only when it changes. */
export interface InstanceProgressEvent {
  instance_id: InstanceId;
  state: InstanceState['state'];
  reason: string | null;
  step: [number, number] | null;
}
```

- [ ] **Step 3: Type-check**

Run: `cd ui && npx tsc --noEmit`
Expected: PASS (no consumers reference the new fields yet, so nothing else should break).

- [ ] **Step 4: Commit**

```bash
git add ui/src/api/types.ts
git commit -m "feat(ui): add types for the instance progress SSE stream"
```

---

## Task 9: App-wide instance progress store

**Files:**
- Create: `ui/src/state/progressReducer.ts`
- Create: `ui/src/state/progressReducer.test.ts`
- Create: `ui/src/state/progressStore.tsx`
- Modify: `ui/src/App.tsx`

**Interfaces:**
- Consumes: `InstanceProgressEvent` (Task 8).
- Produces: `useInstanceProgress(instanceId: string): { reason: string | null; step: [number, number] | null } | null`, exported from `ui/src/state/progressStore.tsx`. Task 10 calls this in `InstanceDetail.tsx`/`StackDetail.tsx`.

- [ ] **Step 1: Write the failing reducer test**

Create `ui/src/state/progressReducer.test.ts`:

```typescript
import { describe, expect, it } from 'vitest';
import { initialProgressState, progressReducer } from './progressReducer';
import type { InstanceProgressEvent } from '../api/types';

const event = (over: Partial<InstanceProgressEvent> = {}): InstanceProgressEvent => ({
  instance_id: 'site.web.0',
  state: 'exited',
  reason: 'redeploying (exited, step 1/3)',
  step: [1, 3],
  ...over,
});

describe('progressReducer', () => {
  it('stores the first event for an instance', () => {
    const next = progressReducer(initialProgressState, { type: 'progress', event: event() });
    expect(next.byId['site.web.0']).toEqual({ reason: 'redeploying (exited, step 1/3)', step: [1, 3] });
  });

  it('overwrites a later event for the same instance', () => {
    const first = progressReducer(initialProgressState, { type: 'progress', event: event() });
    const second = progressReducer(first, {
      type: 'progress',
      event: event({ reason: 'redeploying (exited, step 2/3)', step: [2, 3] }),
    });
    expect(second.byId['site.web.0']).toEqual({ reason: 'redeploying (exited, step 2/3)', step: [2, 3] });
  });

  it('clears to steady-state when reason is null', () => {
    const first = progressReducer(initialProgressState, { type: 'progress', event: event() });
    const second = progressReducer(first, {
      type: 'progress',
      event: event({ state: 'running', reason: null, step: null }),
    });
    expect(second.byId['site.web.0']).toEqual({ reason: null, step: null });
  });

  it('resets everything on reconnect', () => {
    const first = progressReducer(initialProgressState, { type: 'progress', event: event() });
    const reset = progressReducer(first, { type: 'reset' });
    expect(reset.byId).toEqual({});
  });
});
```

- [ ] **Step 2: Run to see it fail**

Run: `cd ui && npx vitest run src/state/progressReducer.test.ts`
Expected: FAIL — `./progressReducer` does not exist yet.

- [ ] **Step 3: Implement the reducer**

Create `ui/src/state/progressReducer.ts`:

```typescript
// Pure reducer for the instance progress store. Kept apart from the hook
// that wires it to EventSource so it's trivial to unit test — mirrors
// state/reducer.ts, the cluster state store's own reducer.

import type { InstanceProgressEvent } from '../api/types';

export interface InstanceProgress {
  reason: string | null;
  step: [number, number] | null;
}

export interface ProgressState {
  byId: Record<string, InstanceProgress>;
}

export type ProgressAction = { type: 'progress'; event: InstanceProgressEvent } | { type: 'reset' };

export const initialProgressState: ProgressState = { byId: {} };

export function progressReducer(state: ProgressState, action: ProgressAction): ProgressState {
  switch (action.type) {
    case 'progress':
      return {
        byId: {
          ...state.byId,
          [action.event.instance_id]: { reason: action.event.reason, step: action.event.step },
        },
      };
    case 'reset':
      return initialProgressState;
    default:
      return state;
  }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd ui && npx vitest run src/state/progressReducer.test.ts`
Expected: PASS (4 tests).

- [ ] **Step 5: Write the provider/hook**

Create `ui/src/state/progressStore.tsx`:

```tsx
// One EventSource subscription for the whole app for instance progress,
// mirroring state/store.tsx's ClusterStoreProvider. /api/instances/events
// emits a `progress` event per instance whenever its (state, reason, step)
// changes; we just drop that into the reducer.

import { createContext, useContext, useEffect, useReducer, type ReactNode } from 'react';
import type { InstanceProgressEvent } from '../api/types';
import { initialProgressState, progressReducer, type InstanceProgress, type ProgressState } from './progressReducer';

const InstanceProgressContext = createContext<ProgressState | null>(null);

export function InstanceProgressProvider({ children }: { children: ReactNode }) {
  const [state, dispatch] = useReducer(progressReducer, initialProgressState);

  useEffect(() => {
    const source = new EventSource('/api/instances/events');

    source.addEventListener('progress', (evt) => {
      try {
        const event = JSON.parse((evt as MessageEvent).data) as InstanceProgressEvent;
        dispatch({ type: 'progress', event });
      } catch {
        // Not every line has to parse; ignore what we can't.
      }
    });
    // A reconnect can miss whatever changed while disconnected; starting
    // clean is safer than showing stale progress that never gets corrected
    // because nothing changes again after reconnecting.
    source.addEventListener('error', () => dispatch({ type: 'reset' }));

    return () => source.close();
  }, []);

  return <InstanceProgressContext.Provider value={state}>{children}</InstanceProgressContext.Provider>;
}

/** `null` when nothing is actively happening to this instance (or nothing
 *  has been received about it yet — the two are indistinguishable and that
 *  is fine, since the ui treats both as "show nothing extra"). */
export function useInstanceProgress(instanceId: string): InstanceProgress | null {
  const ctx = useContext(InstanceProgressContext);
  if (!ctx) throw new Error('useInstanceProgress must be used inside InstanceProgressProvider');
  return ctx.byId[instanceId] ?? null;
}
```

- [ ] **Step 6: Mount the provider**

In `ui/src/App.tsx`, import it next to `ClusterStoreProvider` (line 3):

```typescript
import { ClusterStoreProvider } from './state/store';
import { InstanceProgressProvider } from './state/progressStore';
```

Wrap it just inside `ClusterStoreProvider` (around lines 37-61):

```tsx
    <ClusterStoreProvider>
      <InstanceProgressProvider>
        <ConfigDraftProvider>
          {/* ...unchanged children... */}
        </ConfigDraftProvider>
      </InstanceProgressProvider>
    </ClusterStoreProvider>
```

(Only add the new wrapping tags; leave everything currently between `<ConfigDraftProvider>` and `</ConfigDraftProvider>` untouched.)

- [ ] **Step 7: Type-check and run all ui tests**

Run: `cd ui && npx tsc --noEmit && npx vitest run`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add ui/src/state/progressReducer.ts ui/src/state/progressReducer.test.ts ui/src/state/progressStore.tsx ui/src/App.tsx
git commit -m "feat(ui): add an app-wide store for the instance progress SSE stream"
```

---

## Task 10: Display progress in InstanceDetail and StackDetail

**Files:**
- Modify: `ui/src/views/InstanceDetail.tsx`
- Modify: `ui/src/views/StackDetail.tsx`

**Interfaces:**
- Consumes: `useInstanceProgress` (Task 9).

- [ ] **Step 1: `InstanceDetail.tsx` — subtext under the status badge**

(This is a display-only change with no isolated unit to test in advance — the "test" for this task is the ui build plus the manual check in Step 4, matching how the rest of this codebase's UI treats badge/status rendering.)

Add the import (near the other imports, line 5-9):

```typescript
import { useClusterState } from '../state/store';
import { useInstanceProgress } from '../state/progressStore';
```

Inside the `InstanceDetail` component, after `const parsed = parseInstanceId(id);` (line 35):

```typescript
  const progress = useInstanceProgress(id);
```

Change the badge block (lines 102-106) from:

```tsx
        <Badge tone={instanceStateTone(instance.state)} title={instance.placement_error}>
          {instance.state}
        </Badge>
```

to:

```tsx
        <div style={{ textAlign: 'right' }}>
          <Badge tone={instanceStateTone(instance.state)} title={instance.placement_error}>
            {instance.state}
          </Badge>
          {progress?.reason && <p className="page-sub">{progress.reason}</p>}
        </div>
```

(`instance` is guaranteed non-null at this point in the render — this block sits after the existing early-return-if-`!instance` check earlier in the file; verify that guard exists before this JSX, which it does per the file's existing structure.)

- [ ] **Step 2: `StackDetail.tsx` — subtext in the instance table's State column**

Add the import (near the top, alongside the existing `instanceStateTone` import on line 12):

```typescript
import { useInstanceProgress } from '../state/progressStore';
```

`useInstanceProgress` is a hook and must be called at the top level of a component, not inside the `instances.map((a) => ...)` callback (line 519). Extract the row into its own small component right above the `InstancesTable`-equivalent function this code lives in (find the enclosing component — it's the one containing the `useEffect` poll loop at lines 417-434; check its name with `grep -n "^function\|^const.*=.*(" ui/src/views/StackDetail.tsx | grep -B0 -A0 "417\|420"` or just look a few lines above line 400 for its signature).

Add a small row component near the top of the file (after the imports, before the main exported component):

```tsx
function InstanceStateCell({ instance, state, placementError }: { instance: string; state: string; placementError?: string }) {
  const progress = useInstanceProgress(instance);
  return (
    <>
      <Badge tone={instanceStateTone(state)} title={placementError}>
        {state}
      </Badge>
      {progress?.reason && <div className="page-sub">{progress.reason}</div>}
    </>
  );
}
```

Replace the two existing state-column bodies. First, line 535:

```tsx
                      <td>
                        {i ? <InstanceStateCell instance={a.instance} state={i.state} placementError={i.placement_error} /> : '-'}
                      </td>
```

Second, in the `pending_placement` block, lines 572-576:

```tsx
                    <td>
                      <InstanceStateCell instance={i.id} state={i.state} placementError={i.placement_error} />
                    </td>
```

- [ ] **Step 3: Run the ui build**

Run: `cd ui && npx tsc --noEmit && npm run build`
Expected: PASS.

- [ ] **Step 4: Manual check**

Note for interactive execution (not automatable in this plan): run the app locally (this project's `run` skill), trigger a stack redeploy from the UI, and confirm the state column / instance detail page shows `redeploying (..., step X/3)` text that advances without a manual page refresh, then clears once the container is running again. Trigger a migration (`move` an instance) and confirm `migrating (...)` text appears similarly. If no local cluster is available at plan-execution time, defer this check and say so explicitly rather than claiming it was verified.

- [ ] **Step 5: Commit**

```bash
git add ui/src/views/InstanceDetail.tsx ui/src/views/StackDetail.tsx
git commit -m "feat(ui): show live redeploy/migration progress on instance and stack views"
```

---

## Task 11: Final verification and spec-doc cleanup

**Files:**
- Delete: `docs/superpowers/specs/2026-09-04-instance-progress-streaming-design.md`

**Interfaces:** none — this task only verifies and cleans up.

- [ ] **Step 1: Full workspace check**

Run: `cargo clippy --workspace --all-targets -D warnings`
Expected: PASS, zero warnings (project convention — see Global Constraints).

- [ ] **Step 2: Full workspace test run**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 3: Full ui check**

Run: `cd ui && npx tsc --noEmit && npx vitest run && npm run build`
Expected: PASS.

- [ ] **Step 4: Remove the design doc from the repo**

The user asked not to leave the planning spec committed. Remove it in its own commit (keep the plan doc itself — only the *design spec* was called out):

```bash
git rm docs/superpowers/specs/2026-09-04-instance-progress-streaming-design.md
git commit -m "chore: remove instance progress streaming design spec (implemented)"
```

- [ ] **Step 5: Final summary**

Confirm with `git log --oneline -15` that every task landed as its own commit, and report to the user: what shipped (redeploy step tracking, migration sync progress, the new SSE endpoint, the UI overlay), what was explicitly cut from scope (standalone/background drive-replica-sync progress attribution), and that manual verification (Task 7 Step 5, Task 10 Step 3) still needs a live cluster if one wasn't available during implementation.
