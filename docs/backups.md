# Volume backups (restic)

Clouduler can keep a point-in-time backup history of the data *inside* a
drive's volumes - container data, databases, uploads - using
[restic](https://restic.net). This is a different feature from
[git-backed backup](git-backup.md), which mirrors cluster *metadata* (config,
stack specs, configs, secrets) and never touches volume contents. The two are
independent and can both be running at once.

Restic runs as a plain host binary (`std::process::Command`), never inside a
container - checked for at startup the same way `ethtool`/the `cl-drive`
client binary are, but only warned about, not required, unless a backup
schedule is actually enabled.

## Concepts

- **Destination** - a place restic keeps a drive's repository: another
  cluster node's local disk, a remote host over SSH, or an S3-compatible
  bucket. A drive can back up to several destinations at once, each on its
  own schedule.
- **Schedule** (`DriveBackupTarget`) - one drive's relationship with one
  destination: how often to run, an optional time of day, and a retention
  policy (`keep_last` snapshots and/or `keep_within` a duration - restic's own
  `forget --prune`). A drive with multiple destinations gets one schedule per
  destination.
- **Run** (`BackupRun`) - one execution of a schedule, kept as history:
  started/finished time, outcome (running, succeeded with a snapshot id and
  byte count, or failed with an error), and whether it was triggered by the
  schedule or run manually.
- **Restore request** - a whole-drive, single-volume, or whole-service
  restore submitted through the api or CLI. Always stops the containers using
  the target first; there is no way to restore under a live container.

## Setup

1. `clouduler init` (or the daemon at startup) warns if `restic` isn't on the
   host's `PATH` and a schedule is enabled - install it from your
   distribution's package manager or https://restic.net first.
2. Web UI: **Backup > Destinations**, or CLI:
   ```
   clouduler backup destination add s3-eu --name "S3 (eu)" --kind s3 \
     --bucket my-backups --endpoint s3.eu-central-1.amazonaws.com \
     --s3-access-key-id ... --s3-secret-access-key ...
   ```
   The repository password is generated automatically if you don't supply
   `--password`. Both the password and any backend credentials (SSH private
   key, S3 keys) are sealed with the cluster key and stored as ordinary
   secrets under a reserved name - never shown again, never returned by
   `GET /api/restic/destinations`.
3. Web UI: **Backup > Schedules**, or CLI:
   ```
   clouduler backup target set postgres-nightly --drive postgres-data \
     --destination s3-eu --frequency 24h --time-of-day 02:00 \
     --keep-last 14 --keep-within 30d
   ```

## A node-local destination is a one-off sync, not a replica

If a destination is `NodeLocal` on a node that doesn't already hold the
drive, the runner syncs the drive's contents there once, immediately before
each backup, into a staging directory outside the normal volume tree
(`data_dir/restic-staging/<drive>/<destination_id>/`). It is **not**
registered as a standing replica (`served_by`) the way live drive replication
works. A backup needs restic's own history, which a live mirror doesn't give
you - keeping the two mechanisms separate means a destination node's disk
usage is bounded by how much restic itself needs to work locally, not by an
extra permanent copy of the whole drive.

## Stopping containers before a backup

Backing up a database file while its container is writing to it can produce
an inconsistent snapshot. By default the runner stops every instance using
the drive before backing it up and restarts them afterward. Override this
per-schedule (`stop_containers` on the target) or per-stack/service with
`x-clouduler`:

```yaml
services:
  postgres:
    x-clouduler:
      backup:
        stop_before: false   # this service's own writes are already safe to snapshot live
x-clouduler:
  backup:
    stop_before: true        # stack-wide default
```

Resolution order: the service's own override, then the stack's default, then
the schedule's `stop_containers`, then `true` if nothing says otherwise -
mirroring `auto_update`'s resolution chain.

## Restoring

Restoring is destructive and always stops the affected containers first, no
matter what `stop_before` says for backups - there is no "restore under a
live container" mode. Three granularities:

- **Whole drive** - every instance on the drive is stopped, the whole restic
  snapshot is restored, everything restarts.
- **Single volume** - only the instances using that volume are stopped;
  `restic restore --include /<volume>` off the drive's one whole-drive
  snapshot.
- **A service's volumes** - resolves every volume the service uses (which may
  span more than one drive), stops the service once, restores per-drive
  (pass `--destination`/`destination_id` if a drive backs up to more than one
  place), restarts once at the end.

Web UI: **Backup > History > Restore something**, or from a drive's own page
("Restore this drive") or an instance's page ("Restore volumes"). CLI:

```
clouduler backup restore-drive postgres-data --destination s3-eu [--snapshot ID] [--yes]
clouduler backup restore-volume postgres-data/pgdata --destination s3-eu [--snapshot ID] [--yes]
clouduler backup restore-service myapp postgres [--destination s3-eu] [--snapshot ID] [--yes]
```

Omitting `--snapshot` restores the latest one at the chosen destination.
Without `--yes` each command prints what it's about to do and asks for
confirmation.

### How a restore actually runs

Restoring needs real local access to the drive registry, docker, and the
cluster key - resources only the daemon process has, not `cl-api`. So the api
and CLI both just submit a `RestoreRequest` through raft (`Pending`); a
per-node watcher on whichever node actually implements the target drive
claims it (`Pending -> Running`, first claim wins) and runs the same
restore functions the CLI calls directly. `GET /api/restic/restore-requests`
and the general `/api/events` state stream both show its progress - there is
no separate progress endpoint for restores or backups, since a status
transition (not a percent-complete meter) is all there is to show.

## CLI reference

```
clouduler backup destination add|list|remove
clouduler backup target set|list|remove
clouduler backup run <target-id>
clouduler backup history [--target ID]
clouduler backup snapshots --destination ID
clouduler backup restore-drive NAME --destination ID [--snapshot ID] [--yes]
clouduler backup restore-volume DRIVE/VOLUME --destination ID [--snapshot ID] [--yes]
clouduler backup restore-service STACK SERVICE [--destination ID] [--snapshot ID] [--yes]
```

See also [`docs/api.md`](api.md#restic-backups) for the equivalent HTTP
endpoints.
