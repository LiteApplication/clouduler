# Served drives

`docs/drives.md` covers replicated drives, the default mode. This is the other one: a served
drive has a single copy, held by one node, and every other implementing node reaches it over the
network instead of keeping its own copy in step.

## Replicated vs served

|                       | Replicated                              | Served                                  |
|-----------------------|------------------------------------------|------------------------------------------|
| Copies                | One per implementing node                 | One, on the serving node                 |
| Consistency            | Eventual, up to `sync_interval` behind    | Immediate, minus a short client-side cache TTL |
| Node loses network     | Containers keep running on local data     | Containers using the drive stop working  |
| Write path             | Local disk, synced out later              | Every write is a round trip to the server |
| Good for               | Data every node should keep working with even when the network is down | Data that must never fork into two versions |

Pick served when a container's data must never exist in two divergent copies (a single writer
that other nodes only need to read, or where a stale replica would just be wrong). Pick replicated
when the workload needs to survive its node losing contact with the rest of the cluster.

There is no middle ground: a served drive's `sync_interval` is simply ignored, because there is
only ever one copy to be in or out of sync with.

## Configuring one

```yaml
drives:
  fast:
    mode: served
    served_by: n1

nodes:
  n1:
    drives:
      fast: /srv/clouduler/fast    # the real directory
  n2:
    drives:
      fast: /mnt/fast              # the mountpoint
```

`served_by` names the node holding the data; validation requires it to be set for a served drive
(and rejected for a replicated one), and requires that node to actually implement the drive.
Every other node that lists the drive gets a mountpoint instead of a real directory: the path is
where the drive appears, not where its files live on that node's own disk.

Volumes work the same way as on a replicated drive: `drive/volume` addressing, created on first
write. The scheduler skips a served drive's volumes entirely (`DriveRegistry::is_served`); there
is nothing for it to sync.

## How it's implemented

The serving node runs `served::server`, which answers a small filesystem protocol
(`served::protocol`) against the drive's real directory: lookup, getattr, setattr, readdir, open,
read, write, create, mkdir, unlink, rmdir, rename, truncate, fsync, statfs, symlink, readlink and
link, plus release to close a handle. One TCP connection per mount, opened once and kept for as
long as the mount exists. Every path from the wire is resolved with `resolve_in_root`, which
extends the same component-wise checks `docs/drives.md` describes for volume names with a
canonicalization pass, so a symlink already inside the drive can't be used to walk a request back
out of the drive root.

Every other implementing node mounts the drive with `served::client`, a `fuser` filesystem that
translates kernel calls into protocol requests. Reads and writes are chunked (4 MiB per request)
so a large file is never required to fit in a single frame.

## Mount requirements

Mounting needs either the daemon to run as root, or `user_allow_other` set in `/etc/fuse.conf` on
that node. This isn't optional: a served drive's mountpoint has to be readable by whatever uid the
containers bind-mounting it run as, almost never the daemon's own uid, so every mount is made with
`allow_other`, and the kernel refuses that from a non-root process unless the sysadmin has opted
in system-wide. `served::mount::mount_permission` checks this before attempting to mount and fails
with a message naming exactly what's missing, rather than letting the mount syscall fail with a
bare `EPERM`.

`served::mount::MountManager` owns the lifecycle: `mount_all` at daemon start for every drive this
node mounts (but does not serve), `unmount_all` on shutdown, and a `supervise` loop that remounts
a drive if its FUSE session exits on its own. Losing the serving node briefly does not kill the
session or need a remount: see below.

## Caching and consistency

`served::client` caches attributes and directory listings per path for a short TTL (one second by
default). Without it, walking a tree means a network round trip for every single `getattr`, which
makes an otherwise ordinary `ls -la` or build step unusably slow. The TTL is also handed back to
the kernel in every FUSE reply, so the kernel's own cache expires in step with the client's.

The cost is exactly what that implies: a write made on the serving node, or through a different
node's mount, can take up to the TTL to become visible through this mount. A served drive is
described as "always consistent" relative to a replicated drive because there is only one copy of
the data ever, not because every reader sees every write instantaneously. An operation that
changes a path locally (create, write, unlink, rename, and so on) invalidates that path's own
cache entry immediately, so a client always sees its own writes right away; it's only a change
made somewhere else that's bound by the TTL.

## When the serving node is unreachable

This is the tradeoff a served drive makes deliberately: one copy means no fork, but it also means
one point of failure. When the server can't be reached, `served::client` fails the in-flight
operation with `EIO` rather than hanging the calling process, and reconnects lazily on the next
request rather than needing anything to notice and retry. A container reading or writing through
the mount sees I/O errors for as long as the server is down and resumes on its own once the server
comes back; nothing needs to unmount or remount the drive for that. The `MountManager` supervisor
only intervenes if the FUSE session itself has died, which server unavailability alone does not
cause.

If a node can't tolerate its containers stopping when the network to one other node drops, that
workload wants a replicated drive instead.
