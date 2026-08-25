# Live config

The goal: a running daemon should only need a restart to install a new
binary. Every other config or cluster-key change should reach a running node
on its own, without an operator remembering to restart it.

Most of the daemon has always worked this way — `overlay`, `egress`,
`internal_dns`, `expose` and the scheduler all re-derive what they need from
replicated state on a timer instead of freezing it at boot. The remaining
gaps are closed the same way: two `tokio::sync::watch` channels, built once
in `clouduler::daemon::run` and threaded into whatever needs them.

- `config_rx: watch::Receiver<Arc<ClusterConfig>>` — fed by
  `watch_cluster_config`, which already polls replicated state on
  `failover.probe_interval` to write the config file. It now also sends the
  parsed config on every generation bump.
- `key_rx: watch::Receiver<Option<Arc<ClusterKey>>>` — fed by `cl-api`'s
  existing in-process key delivery (`spawn_unwrap_own_key`, and a manual
  `POST /api/secrets/key`). `ClusterKey` is deliberately not `Clone` (see its
  doc comment), so every consumer of the shared value holds an `Arc<ClusterKey>`
  instead.

A consumer reads with `.borrow()` (synchronous, cheap — a `Receiver` is a
pointer and a generation counter) or reacts instantly with `.changed().await`.

## What's live now

- **The cluster key**, everywhere it used to be loaded once from disk: the
  materializer's secret decryption, the reverse proxy's Cloudflare token, and
  self-update's GitHub token/internal signing. A node started with no key
  still gets all of this working once a key is delivered to it automatically.
- **Which networks are egress- or public-kind**, for scheduling
  (`cl_sched::Scheduler::live_config`) — a network rename or kind change
  takes effect on the next scheduler sweep.
- **The reverse proxy's listeners, ACME issuance and Cloudflare DNS
  publishing** (`clouduler::proxy`). Toggling the proxy, changing its ports,
  turning on ACME, or adding a Cloudflare section all take effect without a
  restart — the supervisor only restarts the specific task group whose
  inputs actually changed (`Desired`, diffed every pass).
- **The web UI's own TLS certificate.** Once ACME issues or renews it, the
  already-bound listener reloads it (`RustlsConfig::reload_from_pem`) instead
  of serving the boot-time self-signed one indefinitely.
- **`cl-api`'s view of the cluster config**, on every node — not just the one
  whose own `PUT /api/config` handler ran. This also keeps `session_ttl` and
  the OIDC authenticator (issuer, client id, whether it's configured at all)
  live.
- **Migration targets**: a node added to the cluster after a source node
  booted can be a live migration target.
- **Drive topology**: adding a drive to `node.drives`, or changing who's in a
  served drive's `served_by` list, rebuilds just the affected part of the
  drive subsystem (`clouduler::drives`) — the transport/sync pair, one
  served-drive server task, or a single FUSE mount — rather than the whole
  subsystem or the daemon. A drive's `sync_interval` is read fresh on every
  sync pass and needs no rebuild at all. Removing a drive from config never
  deletes the directory it backed; it only stops mounting, serving or
  transporting it.
- **Docker connectivity**: `overlay`, `egress`, the scheduler and migration
  now hold the same live `Arc<RwLock<Option<Docker>>>` handle `cl-api` always
  has, instead of a one-time snapshot. A node whose dockerd was not up yet at
  boot gets a working client (and starts scheduling containers) once one
  connects, and all four are now always spawned regardless of whether docker
  was reachable at boot.

## What still needs a restart

- **Raft's own listen address, data dir, and cluster identity**
  (`ClusterNode::start`). Rebuilding raft membership or transport under a
  running cluster is a different, riskier kind of problem than reconfiguring
  a reconcile loop, and is out of scope here.
- **Drive topology changes** are live (above) but deliberately conservative:
  nothing about a drive's on-disk data is ever touched beyond mounting or
  unmounting it.
- **Sub-second reaction to a container someone stopped by hand.** The
  scheduler still polls `docker.list_managed()` on a fixed sweep
  (`cl_sched::run::SWEEP_INTERVAL`) rather than subscribing to
  `cl_docker::Docker::events()`, which exists but is unwired. This is a
  latency gap, not a "needs a restart" one, and is left for a follow-up.

## Adding a new live-reconfigurable subsystem

Prefer extending the existing idiom over inventing a new one:

- If the subsystem already fetches `ClusterState` on a timer for other
  reasons (most do), just derive what it needs from `config_rx.borrow()` (or
  the state it already has) on each pass — no new channel required.
- If it needs a synchronous read outside of an async loop, or needs the
  cluster key/config from a cold start with no reconcile loop of its own,
  take a `watch::Receiver` clone as a constructor argument and read fresh at
  the point of use.
- If the subsystem's own listeners or task set depend on config (ports,
  whether a feature is enabled at all), model it as a small `Desired` struct
  compared with `PartialEq` on every pass, and only tear down and rebuild the
  specific task group whose `Desired` fields changed — see
  `clouduler::proxy` and `clouduler::drives` for two shapes of this.
