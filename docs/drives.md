# Drives and volumes

## Drives vs volumes

A **drive** is a cluster-wide namespace, declared once in the cluster config:

```yaml
drives:
  fast:
    sync_interval: 5m
  bulk:
    sync_interval: 1h
```

A drive has no filesystem presence by itself. A node **implements** a drive by mapping its name
to a local directory:

```yaml
nodes:
  n1:
    drives:
      fast: /srv/clouduler/fast
      bulk: /srv/clouduler/bulk
  n2:
    drives:
      fast: /srv/clouduler/fast
```

`n1` and `n2` both implement `fast`, at whatever local path they choose; the paths do not need to
match between nodes. A node that does not list a drive cannot run anything that mounts a volume
from it, and the scheduler will not place such a service there (`cl-raft`'s `check_placement`
rejects it).

A **volume** is a subdirectory of a drive, addressed as `drive/volume` (`cl_raft::VolumeId`). A
stack mounts one with the `drive/volume:path` syntax on a service's `volumes:` entry. Volumes are
created on demand: the first write (`DriveRegistry::ensure_volume`) makes the directory if it
doesn't already exist.

The directory is only half of it: a volume also has to be *on record*, or it exists on whichever
node happens to hold the bytes and nowhere else — there is nothing for the sync scheduler to
replicate, nothing for `/api/volumes` to report and nothing for a migration to hand over. The
leader registers one (`Request::UpsertVolume`) on its scheduling pass for every volume a placed
instance mounts, with the instance's node as the first primary (`cl_sched::run::volume_requests`).
Only volumes the cluster does not already know about are submitted: an existing record has a
primary that migration moves, and re-asserting the assignment's node every sweep would fight it.

Volume names are validated component by component before ever touching the filesystem: empty,
`.`, `..` and NUL-containing components are rejected outright, and a name is joined onto the
drive's root by simple path segment concatenation. There is no canonicalization step, since the
target may not exist yet — the validation itself is what keeps a crafted volume name (`../../etc`,
an absolute path, an empty component from a doubled slash) from ever resolving outside the drive
root.

## How sync works

Every node that implements a drive keeps its own copy of that drive's volumes. One node is the
**primary** for a given volume (tracked in raft state as `VolumeState.primary`); every other
implementing node is a **replica**. The primary pushes changes out; replicas never pull.

The default transfer engine (`DeltaEngine`) does not diff the whole file on every sync. A scan
(`scan::scan`) walks a volume into a `Manifest`: size, mtime (nanosecond resolution — second
resolution is too coarse to tell a rewrite from a no-op when it lands in the same wall-clock
second), permission bits, and, only when asked, a blake3 content hash. An ordinary sync compares
manifests on size and mtime alone; hashing every file on every pass does not scale to a large
volume. Content hashing is reserved for two places that need it: rename detection (matching a
"file removed here" against a "file appeared there" by content) and migration verification
(confirming source and target are byte-identical after the final sync).

For each file that changed: if it's new, small, or a symlink, the whole file is sent — a signature
round trip costs more than it saves. Otherwise the receiver computes an `fast_rsync` signature of
what it already has, the sender diffs against that signature and ships only the delta, and the
receiver applies the delta and only then renames it into place. Every write lands in a temp file
next to its target first (`write_temp_then_rename`); the rename is the only step that makes the
new content visible under the real name, so a connection that dies mid-transfer never leaves a
half-written file where a service could see it. Deletions and renames are explicit protocol
messages, not inferred from a subsequent full rescan.

btrfs and zfs send/receive are wired as detectable fast paths (`delta::fast_path::detect`, probing
the drive path's filesystem type via `statfs`) behind the same `SyncEngine` trait, but no
maintained crate for either exists on crates.io as of this writing, so a real fast-path engine
would shell out to the `btrfs`/`zfs` binaries. That implementation is not built; every sync today
runs through `DeltaEngine` regardless of the backing filesystem.

## Sync intervals

Every drive has a default `sync_interval`. A stack can override it per volume:

```yaml
x-clouduler:
  volume_sync:
    gitlab-data:
      interval: 15m
```

`cl-config`'s `StackFile::sync_interval` resolves the effective interval: the service's own
`volume_sync` entry, then the stack-wide default, then the drive's own `sync_interval`. `cl-drive`
does not parse stack files itself — the daemon hands the scheduler (`sync::SyncScheduler`) a
resolved `IntervalSource`, keeping this crate decoupled from compose parsing.

A service can also run one of its `actions` before and after each replication of a volume
(`before:`/`after:` on that volume's service-level `volume_sync` entry), e.g. to checkpoint a
database first. `SyncScheduler::with_hooks` takes a `SyncHooks` implementation; the daemon's runs
the action in the service's containers on the primary node. A failed `before` skips that
replication, and `after` always runs. Migrations and manual syncs do not run hooks. See
`docs/stacks.md`.

The scheduler runs one loop per node, ticking periodically and, for each volume this node is
primary for, checking whether its interval has elapsed. A newly discovered volume gets a random
jitter within its own interval before its first tick, so a fleet of volumes on the same interval
does not all sync in the same second. A volume never has two syncs running at once — an in-flight
guard skips a tick rather than queuing a second sync behind a slow one. When quorum is lost the
scheduler skips its pass entirely rather than replicating against a cluster view it can't trust.

## The generation counter

`VolumeState.generation` is how a node's replica is judged current without hashing the whole tree.
The primary bumps `generation` on every write (`Request::TouchVolume`); a replica reports the
generation it has caught up to after a successful sync (`Request::SetVolumeReplica`). A replica
whose recorded generation equals the volume's current generation is in sync
(`VolumeState::in_sync`); anything lower is stale. Reports can arrive out of order — a slow sync's
result landing after a faster, newer one — so a replica's recorded generation only ever moves
forward, never back.

## Migration sequence

Moving a running instance's volumes to another node is a raft-recorded state machine
(`MigrationPhase`), so a crash mid-move resumes from whatever phase was last committed instead of
repeating or skipping a step:

1. **Stopping** — the container is stopped on the source, via `InstanceControl::stop_instance`.
2. **Syncing** — a final sync of every volume to the target, using the primary's current
   generation as the target of the sync; the reached generation is reported back through raft.
3. **Verifying** — source and target manifests are compared with content hashing on, over both
   ends (the target answers a manifest request without starting a full sync). Any difference fails
   the migration.
4. **Switching** — the point of no return. Every volume's primary moves to the target
   (`Request::TouchVolume`), which also bumps `generation` and marks the source stale as a
   replica.
5. **Starting** — the container starts on the target, via `InstanceControl::start_instance`.
6. **Done** — the migration record is cleared; the assignment now points at the target.

`InstanceControl` is the only place this module touches the outside world for the container
itself; docker lifecycle is the scheduler crate's job, not this one's, which is what lets
migration be tested with a fake instance control and no docker daemon.

## Who runs a migration

The api records the move and returns. `clouduler::migration` is the loop that carries it out: every
node polls for migrations naming it in `Migration::from` and hands each to `migration::run`.

Ownership is `from` rather than the assignment's node because **Switching** repoints the assignment
at the target halfway through — a claim keyed off the assignment would change hands mid-move, and
after a crash nobody could tell whose job an unfinished migration was. `from` is written once, when
the move is recorded, so one node drives a move for its whole life and recognises its own work when
it restarts.

**Starting** is the exception to that. The container has never existed on the target, so the source
cannot start it there and there is no rpc for starting a container on another node. `Switching` has
already pointed the assignment at the target, so the target's own reconcile loop creates and starts
it on its next sweep, like any other instance it has been given; `start_instance` for a remote node
does nothing but say so.

While a migration is live the source must not restart what it just stopped. `Scheduler::targets`
records an instance migrating away from this node as a **stopped** target rather than dropping it:
the reconcile loop then leaves it alone instead of racing the handover, the container stays in
place, and a migration that fails before **Switching** is one start away from where it began.

If any step before **Switching** fails, the migration is marked `Failed { reason }` and the
function returns; the assignment still points at the source and the source is still the volume's
primary, so the source copy stays authoritative and nothing has to be undone. Once **Switching**
has run, the target owns the volume regardless of what happens to the container start — failure
there is a container problem, not a data problem, and is out of scope for this module's guarantee.
