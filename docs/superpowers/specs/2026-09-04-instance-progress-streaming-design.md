# Instance progress streaming

## Problem

`StackDetail.tsx` and `InstanceDetail.tsx` poll `/api/instances` / `/api/instances/{id}` on a
`setInterval` (5s / 4s respectively). Multi-step in-progress operations — a redeploy,
a migration, a volume/disk sync — are invisible between polls, and even when caught they
show only a raw docker state (e.g. `exited`) with no indication that something is actively
happening or how far along it is. The daemon already tracks some of this internally
(`PullTracker`, `MigrationPhase`) but never exposes step-level detail, and none of it is
pushed to the UI in real time.

## Scope

Real-time, step-by-step status for three operations:

1. **Redeploy / recreate** — stack update triggering container stop → image pull → recreate → start.
2. **Migration** — instance moving to another node.
3. **Volume/drive sync** — disk transfer between nodes.

Cluster self-update rollout (`GET /api/updates`) is out of scope — it already has its own
polled view and isn't part of this request.

## Architecture

A new node-local "instance progress" tracker fed by three sources — the reconciler
(redeploy sub-steps), raft's existing `MigrationPhase`, and cl-drive's volume sync job —
plus a new poll-and-diff SSE endpoint (same pattern as the existing `/api/events` in
`crates/cl-api/src/stream.rs`) that streams only what changed to the UI.

This is kept as a **separate stream from `/api/events`**: `/api/events` carries the
raft-replicated `ClusterState` (polled every 500ms, emitted on version change); instance
progress is ephemeral, node-local, higher-churn data with a different shape. Mixing them
would couple two unrelated concerns in one channel and one poll loop.

## 1. Reconciler: real redeploy sub-steps

The actual redeploy path is `Reconciler::replace` (`crates/cl-sched/src/reconcile.rs:314-327`),
used by both `Action::Recreate` and the stale-network recovery branch of `Action::Start`.
Its real sequence is: ensure image → stop old → remove old → create new → start new.

Modeled as 3 user-facing steps:

1. **Stopping** — old container going down.
2. **Pulling/Creating** — image pull if needed (reuses the existing `ensure_image`/`pulling`
   detection), then container create.
3. **Starting** — new container starting, network/route setup.

Add to `Reconciler`:

```rust
pub current_step: Arc<RwLock<HashMap<InstanceId, RedeployStep>>>,
```

set/cleared around each phase inside `replace()`, mirroring the existing
`pulling: Arc<RwLock<BTreeSet<InstanceId>>>` (`PullTracker`) sharing pattern already used
to expose "pulling" to `cl-api`.

```rust
pub struct RedeployStep {
    pub step: RedeployPhase, // Stopping | PullingOrCreating | Starting
    pub total: u8,           // 3
}
```

Entry removed from the map once `replace()` returns (success or error), same lifecycle as
`PullTracker`.

## 2. Migration & volume sync — reuse existing phase state

- **Migration**: already has `MigrationPhase` (Stopping/Syncing/Verifying/Switching/Starting/Done)
  in raft state (`crates/cl-raft/src/state.rs:311-325`), attached to an `Assignment` via
  `Migration { from, to, phase }`. No reconciler change needed — the new poll loop reads it
  the same way `crates/cl-api/src/routes/instances.rs:100-103,206-209` already does for the
  polled response.
- **Volume sync**: `crates/cl-drive/src/migration.rs`'s `MigrationJob` currently exposes no
  percentage. Add:

  ```rust
  pub type SyncProgressMap = Arc<RwLock<HashMap<VolumeKey, SyncProgress>>>;
  pub struct SyncProgress { pub done: u64, pub total: u64 } // bytes or file count, whichever the job already tracks cheaply
  ```

  updated by the sync loop, same sharing pattern as `PullTracker` and `RedeployStep` above.
  Entry removed when the job finishes.

## 3. New SSE endpoint: `GET /api/instances/events`

New handler in `cl-api`, modeled on the existing poll loop in `crates/cl-api/src/stream.rs`.
Every ~500ms:

1. Recompute each instance's summary, reusing `summarize_local`/`run_state_to_summary`
   (`crates/cl-api/src/routes/instances.rs`), extended to fold in `RedeployStep`,
   `MigrationPhase`, and `SyncProgress`.
2. Compose a single human-readable `reason` string server-side (one place — the UI never
   reconstructs English from raw enums), e.g.:
   - `"redeploying (exited, step 1/3)"`
   - `"migrating (syncing)"`
   - `"syncing volume (42%)"`
   - `null` when the instance is steady-state (no active operation).
3. Diff against the last-sent value per instance; emit an SSE event only for instances whose
   `(state, reason, step)` changed since the last emission. Steady instances that never
   change don't reappear in the stream after their initial value — keeps traffic small.

### Payload

```ts
interface InstanceProgressEvent {
  instance_id: string;            // matches existing InstanceSummary.id
  state: InstanceState;           // reuse existing enum (running/exited/pulling/...)
  reason: string | null;          // composed label; null when idle/steady
  step: [number, number] | null;  // e.g. [1, 3]; null when not a stepped operation
}
```

## 4. UI wiring

- New hook (reusing the `useEventSource` pattern already used for logs/stats) opening one
  connection to `/api/instances/events`, keyed into a client-side map
  `instance_id → { reason, step }`.
- `StackDetail.tsx` and `InstanceDetail.tsx` read from this map to overlay the live
  `reason`/`step` on top of their existing polled summary — the poll stays the source of
  truth for full instance data (ports, mounts, etc.); the stream only adds the "what's
  happening right now" text. No replacement of the existing poll loops.
- Display: small subtext under the status pill, e.g. `Exited · redeploying (step 1/3)`,
  cleared when a progress event for that instance arrives with `reason: null`.

## Out of scope

- Cluster self-update rollout status streaming.
- Replacing the existing `/api/instances` / `/api/instances/{id}` poll loops — this adds a
  live overlay, not a new fetch model.
- Historical/audit log of past operations — this is current-state-only, matching how
  `PullTracker` and `MigrationPhase` behave today.
