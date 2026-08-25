# Served drives

`docs/drives.md` covers replicated drives, the default mode. This is the other one: a served
drive has a single active copy, held by one of a list of candidate servers, and every other
implementing node reaches it over the network instead of keeping its own copy in step.

## Replicated vs served

|                       | Replicated                              | Served                                  |
|-----------------------|------------------------------------------|------------------------------------------|
| Copies                | One per implementing node                 | One active copy, plus a backup on each other `served_by` candidate |
| Consistency            | Eventual, up to `sync_interval` behind    | Immediate against the active server, minus a short client-side cache TTL |
| Node loses network     | Containers keep running on local data     | Containers using the drive stop working until a mount fails over or the server returns |
| Write path             | Local disk, synced out later              | Every write is a round trip to the active server |
| Good for               | Data every node should keep working with even when the network is down | Data that must never fork into two versions, with automatic failover if the active server goes down |

Pick served when a container's data must never exist in two divergent copies at once (a single
writer that other nodes only need to read, or where a stale replica would just be wrong). Pick
replicated when the workload needs to survive its own node losing contact with the rest of the
cluster.

## Configuring one

```yaml
drives:
  fast:
    mode: served
    served_by: [n1, n2, n3]
    sync_interval: 15m

nodes:
  n1:
    drives:
      fast: /srv/clouduler/fast    # a real directory: n1 is a candidate
  n2:
    drives:
      fast: /srv/clouduler/fast    # a real directory: n2 is a candidate too
  n3:
    drives:
      fast: /mnt/fast              # a mountpoint: n3 only mounts the drive
```

`served_by` is a priority-ordered list of the nodes that can hold this drive's data. The **active
server** is always the first node in that list that's currently reachable; validation requires
every node named there to map the drive to the real directory holding its copy. Any other node —
whether or not it declares the drive in `cluster.yaml` at all — can run a container that mounts one
of its volumes: the scheduler no longer requires a node to implement a served drive to be eligible
(unlike a replicated one, which still does), and the drive subsystem mounts just that volume there
over FUSE on demand. A node that does declare the drive purely to mount it (like `n3` above) gets
the same on-demand per-volume mount; the declared path is not used as a static mountpoint any more.

Volumes work the same way as on a replicated drive: `drive/volume` addressing, created on first
write. The scheduler still skips a served drive's volumes when deciding what to replicate as a
"primary" copy (`DriveRegistry::is_served`); a served drive's own sync path (below) handles moving
data between candidates instead.

## Failover and `sync_interval`

Every node in `served_by` runs the drive server (`served::serve`) against its own real directory,
not just the active one — that's what lets a takeover be instant instead of needing something to
start a process first. Only the active server is actually reachable through a mount, though:
every mounting node is pointed at whichever candidate is active, and a background watcher checks
this every few seconds and repoints the mount when it changes. The active server itself is derived
the same way, from the same replicated reachability state (`ClusterState::unreachable`) every node
already tracks — there's no separate election, so every node agrees on who's active without any
extra coordination.

`sync_interval` controls how often the active server pushes its data out to the other `served_by`
candidates, using the same delta-sync engine and raft-tracked generation bookkeeping a replicated
drive's volumes use (`SyncScheduler`, `Request::SetVolumeReplica`). This is what makes a backup
worth failing over to: without it, a freshly promoted candidate would have nothing.

Because the active server is simply "the first reachable candidate", a candidate that recovers
reclaims active duty automatically, even if a lower-priority candidate took over while it was
down. This is a deliberate simplicity tradeoff: it needs no extra raft state or leader-driven
promotion, at the cost of a brief cutover blip (and, since the sync interval is not instant, up to
one interval's worth of writes not yet mirrored to a promoted backup) every time a preferred server
comes back. List your most reliable node first if this matters to you.

A single-candidate `served_by: [n1]` still works exactly as before: no backups, no failover,
`sync_interval` has nothing to push and is effectively unused, same as a served drive always was.

## How it's implemented

Every `served_by` candidate runs `served::server`, which answers a small filesystem protocol
(`served::protocol`) against the drive's real directory: lookup, getattr, setattr, readdir, open,
read, write, create, mkdir, unlink, rmdir, rename, truncate, fsync, statfs, symlink, readlink and
link, plus release to close a handle. One TCP connection per mount, opened once and kept for as
long as the mount exists, reopened against the current active server's address if it changes.
Every path from the wire is resolved with `resolve_in_root`, which extends the same component-wise
checks `docs/drives.md` describes for volume names with a canonicalization pass, so a symlink
already inside the drive can't be used to walk a request back out of the drive root.

Every node that only uses the drive mounts it with `served::client`, a `fuser` filesystem that
translates kernel calls into protocol requests against whichever address it's currently pointed
at (`ClientFs::server_handle`/`MountManager::update_server`). Reads and writes are chunked (4 MiB
per request) so a large file is never required to fit in a single frame.

The server always answers for the drive's whole root; scoping to one volume happens client side.
`ClientFs::at` seeds the FUSE mount's root inode at the volume's subpath (`ClientFs::new`/
`with_config` mount the drive's whole root, for backward compatibility, by seeding it at the empty
path instead), so every request the mount sends is already prefixed with that volume's name and
the server needs no volume concept of its own. `MountSpec::key` (`drive` alone for a whole-drive
mount, `drive/volume` for a per-volume one) is what `MountManager` keys its mount table by, so
several volumes of the same drive can be mounted independently.

## Mount requirements

Mounting needs either the daemon to run as root, or `user_allow_other` set in `/etc/fuse.conf` on
that node. This isn't optional: a served drive's mountpoint has to be readable by whatever uid the
containers bind-mounting it run as, almost never the daemon's own uid, so every mount is made with
`allow_other`, and the kernel refuses that from a non-root process unless the sysadmin has opted
in system-wide. `served::mount::mount_permission` checks this before attempting to mount and fails
with a message naming exactly what's missing, rather than letting the mount syscall fail with a
bare `EPERM`.

`served::mount::MountManager` owns the lifecycle, keyed per volume rather than per drive:
`mount_one`/`unmount` bring one volume's mount up or down, `unmount_all` on shutdown, `update_server`
to repoint a mount when the active candidate changes, and a `supervise` loop that remounts a volume
if its FUSE session exits on its own. Losing the active server briefly does not kill the session or
need a remount: see below.

Which volumes are actually mounted on a node is not declared in `cluster.yaml` — it's read off the
cluster's own placement decisions. A background convergence loop (`clouduler::drives::
watch_volume_mounts`) polls `ClusterState.assignments` for this node's own running instances,
resolves each one's stack file to find any served-drive volumes it mounts, and mounts exactly those
(under `<data_dir>/volume-mounts/<drive>/<volume>`) — skipping any this node happens to be the
active server for, since those are read locally with no FUSE involved — then unmounts anything no
longer needed. This is also what makes a node that never declared a served drive in `cluster.yaml`
usable for it at all: the scheduler is free to place a container there, and this loop mounts what it
needs a few seconds later. A container whose volume mount isn't up yet simply isn't started until it
is (`cl_sched::run::volumes_ready`), rather than starting against an empty, not-yet-mounted
directory.

## Caching and consistency

`served::client` caches attributes and directory listings per path for a short TTL (one second by
default). Without it, walking a tree means a network round trip for every single `getattr`, which
makes an otherwise ordinary `ls -la` or build step unusably slow. The TTL is also handed back to
the kernel in every FUSE reply, so the kernel's own cache expires in step with the client's.

The cost is exactly what that implies: a write made on the active server, or through a different
node's mount, can take up to the TTL to become visible through this mount. A served drive is
described as "always consistent" relative to a replicated drive because there is only ever one
active copy being written to, not because every reader sees every write instantaneously. An
operation that changes a path locally (create, write, unlink, rename, and so on) invalidates that
path's own cache entry immediately, so a client always sees its own writes right away; it's only a
change made somewhere else that's bound by the TTL.

## When the active server is unreachable

When the active server can't be reached, `served::client` fails the in-flight operation with
`EIO` rather than hanging the calling process, and reconnects lazily on the next request rather
than needing anything to notice and retry. A container reading or writing through the mount sees
I/O errors for as long as no candidate is reachable, and resumes once the failover watcher points
the mount at a newly active candidate (or the original server comes back, if nothing else took
over first). Nothing needs to unmount or remount the drive for that: `MountManager::supervise`
only intervenes if the FUSE session itself has died, which server unavailability alone does not
cause.

With more than one candidate in `served_by`, a candidate going down is no longer a full outage:
another one takes over as soon as the mounting nodes' watcher notices, typically within a few
seconds. The remaining exposure is the same one-copy tradeoff a single-candidate served drive
always had, just against however many candidates are down at once: if every candidate in
`served_by` is unreachable, there's nothing left to fail over to.
