# HTTP API

Served on `web.address:web.port` over HTTPS by every node, but **writes only by
the leader**. A non-leader answers a write with `503` and a `Location` header
pointing at the leader, so a client that hit the wrong node can retry without
extra machinery.

Reads (`GET`) are answered by whichever node you ask. That is deliberate:
live container state, node resource usage and daemon logs exist only on the
node they describe, and there is no rpc path for the leader to fetch another
node's. If reads redirected too, that information could never be obtained at
all. The trade is that a lagging follower answers from slightly stale
replicated state; `/api/status` carries `state_version` so a caller that cares
can tell how far behind it is.

`/auth/oidc/*` is the one write path exempt from the redirect, or a client
stuck on a follower could never log in to discover anything.

Everything is JSON. Errors use this shape and the matching status code:

```json
{ "error": "human readable message", "detail": "optional context" }
```

## Authentication

Two ways in, both producing the same session context.

**API keys.** Always available unless `auth.api_keys` is false, which the config
validator only allows once OIDC is configured. One key is generated on first
launch and printed to the log once, because there is no other way to get into a
brand new cluster. Sent as `Authorization: Bearer clk_<id>_<secret>`. Only a hash
of the secret is stored, so a leaked database does not hand over working keys.

**OIDC.** The recommended path for people. `GET /auth/oidc/login` starts the
flow, `GET /auth/oidc/callback` finishes it and sets a session cookie. A user
holding `auth.oidc.admin_value` in `auth.oidc.admin_claim` gets `admin`;
everyone else who authenticates gets `viewer`. The web ui shows the value of
`auth.oidc.username_claim` (default `preferred_username`) as the signed-in
user's name, falling back to the opaque `sub` claim when that claim is absent.

Roles: `admin` may change anything, `viewer` may read everything except secret
values. Endpoints below are admin-only unless marked read.

```
GET    /auth/me                         read    current identity and role
POST   /auth/logout
GET    /auth/oidc/login                 unauth  redirects to the provider
GET    /auth/oidc/callback              unauth
GET    /api/keys                        read    list keys, never their secrets
POST   /api/keys                        create, returns the secret once
DELETE /api/keys/:id
```

## Cluster

```
GET    /api/status                      read    this node's view: quorum, leader, versions
GET    /api/state                       read    the whole applied state, redacted
GET    /api/events                      read    SSE stream of state changes
```

`/api/status` answers with:

```json
{
  "node": "n1",              // the node answering, not necessarily the leader
  "is_leader": true,
  "has_quorum": true,
  "leader": "n1",            // node name, not the internal raft id
  "voters": 3,
  "reachable": 3,
  "managers_total": 3,
  "managers_reachable": 3,
  "version": "0.1.0",        // daemon build version
  "state_version": 5         // replicated state version, moves on every write
}
```

`/api/events` emits `state` events carrying the same body as `/api/state`
whenever the state version moves, so the ui does not poll.

## Nodes

```
GET    /api/nodes                       read
GET    /api/nodes/:name                 read    includes live resource usage
POST   /api/nodes                       add a node to the cluster
PATCH  /api/nodes/:name                 { "availability": "active|drain" }
DELETE /api/nodes/:name                 decommission, refused while it runs work
GET    /api/nodes/:name/logs            read    SSE stream of daemon logs
```

Node detail includes CPU count and load, memory total and used, container
counts, and the reachability flag. Live resource usage (`usage`) is only
populated when the node answering the request is the node being described;
cl-raft carries no rpc call for one node to pull another's live resource
figures, so a remote node's `usage` comes back zeroed with `local: false`.
Disk usage per implemented drive is not currently reported.

`GET /api/nodes/:name/logs` similarly only streams the daemon log of the node
answering the request. Asking a node for another node's log returns `404`
with a message saying to connect to that node directly, since there is no rpc
path to fetch it remotely either. No in-process log ring buffer exists yet, so
even the local case currently streams nothing; wiring one up is future work.

## Stacks

```
GET    /api/stacks                      read
GET    /api/stacks/:name                read    spec plus owned object names
PUT    /api/stacks/:name                deploy or redeploy from yaml
DELETE /api/stacks/:name
POST   /api/stacks/:name/validate       read    lint without deploying
```

`PUT` takes `{ "spec": "<yaml>", "configs": {...}, "secrets": {...} }` where the
maps carry base64 contents for anything the spec references. Secrets are sealed
by the server before they reach the log.

## Services and instances

```
GET    /api/instances                   read    every assignment plus live state
GET    /api/instances/:id               read
POST   /api/instances/:id/restart
POST   /api/instances/:id/move          { "node": "n2" }
PATCH  /api/instances/:id               { "desired": "running|stopped" }
POST   /api/instances/:id/update        pull a newer :latest and recreate
GET    /api/instances/:id/logs          read    SSE, ?follow=&tail=&since=
GET    /api/instances/:id/stats         read    SSE, cpu, memory, network, io
```

Instance ids are `stack.service.index`. A summary looks like:

```json
{
  "id": "blog.web.0",
  "node": "n1",
  "desired": "running",       // what the cluster wants
  "state": "running",         // what the container is doing: running|stopped|unknown
  "running": true,            // null when state is unknown
  "health": "healthy",        // healthy|unhealthy|starting|none
  "image": "nginx:1.25",
  "image_digest": "sha256:...",   // what the cluster has pinned
  "running_digest": "sha256:...", // what the container is actually running
  "started_at": "2026-08-18T09:30:19Z",
  "migration": null           // { "to": "n2", "phase": "syncing" } while moving
}
```

`state` is `unknown`, and `running` null, for an instance on another node: there
is no rpc path for one node to inspect another's containers, and that is a
different answer from `stopped`.

`restart`, `update`, `logs` and `stats` only work when the instance runs on
the node currently answering the request; cl-docker has no rpc path for
fetching another node's container state. Hitting them for an instance that
runs elsewhere returns `409` naming the node it actually runs on.

`image_digest` and `running_digest` answer different questions and routinely
differ: the first is the pin recorded in raft, the second is the registry
digest behind the image id the container was actually created from. A pin that
has been recorded but not yet built onto shows up as a disagreement between
them, which is what `clouduler stack unhealthy` reports as pin drift.

`move`
currently begins the migration without re-deriving the drives the instance's
volumes need from its stack spec, so a target node's drive support is not
re-checked at move time the way it is at initial placement.

## Updates

```
GET    /api/updates                     read    per instance: running vs latest
POST   /api/updates/pin                 { "instances": [] }  pin what is running
POST   /api/updates/apply               { "instances": [] }  pull and rebuild
```

An empty (or absent) `instances` list means "everything this applies to":
every unpinned instance for `pin`, every instance with an update available for
`apply`. Both answer with one result per instance, so a partial failure is
legible rather than the whole call reading as failed:

```json
[{ "id": "blog.web.0", "ok": true }]
```

A row of the listing:

```json
{
  "id": "blog.web.0",
  "stack": "blog",
  "service": "web",
  "node": "n1",
  "image": "nginx:1.25",
  "running_digest": "sha256:...",   // the bits actually running
  "latest_digest": "sha256:...",    // what the registry serves for `image`
  "pinned_digest": null,            // what the cluster has pinned
  "update_available": true,         // latest differs from running
  "pin_drift": false,               // pinned differs from running
  "error": null                     // why the registry could not be asked
}
```

`update_available` is only ever true when both digests are known. An image
built locally has no registry digest at all, and reporting those as
perpetually out of date would make "update all" a no-op that never stops
asking.

The listing costs one registry round trip per *distinct image* across the
cluster — the fan-out is per node, not per instance — so it is not something
to poll. `apply` runs its updates sequentially on purpose: each one takes its
container down and lets the reconcile loop build a new one, so running them
concurrently would stop every replica of a service at once.

`pin` only ever pins instances that have none. An instance already pinned had
that decision made deliberately, and repointing it at whatever happens to be
running now would silently undo a rollback.

## Registry accounts

```
GET    /api/registries                  read    server and username, never a password
PUT    /api/registries                  { "server": "", "username": "", "password": "" }
POST   /api/registries/verify           same body, checks without storing
DELETE /api/registries/:server
```

Logins are sealed with the cluster key and stored as reserved cluster secrets,
so they are replicated to every node and hidden from `/api/secrets`. The
password is write-only: no route returns it.

`PUT` refuses a login the registry does not accept, using the ordinary
registry v2 handshake (`GET /v2/`, then the `Bearer` token realm it names).
Storing an unverified password does not fail here; it fails later, on another
node, mid-pull, as an image that will not come down.

Every spelling of Docker Hub (`hub.docker.com`, `index.docker.io`,
`registry-1.docker.io`) is folded to `docker.io`, which is the host docker
itself keys credentials on. A login is only ever offered to the registry named
in the image reference — handing one registry's password to another is a
disclosure, not a convenience.

## Configs and secrets

```
GET    /api/configs                     read    names, sizes, revisions, owners
GET    /api/configs/:name               read    includes content
PUT    /api/configs/:name               { "content": "<base64>" }
DELETE /api/configs/:name

GET    /api/secrets                     read    names and metadata only, never values
PUT    /api/secrets/:name               see below
DELETE /api/secrets/:name
```

A secret value is never returned by the API, at any role. `PUT` accepts three
shapes, matching the three ways the ui offers to make one:

```json
{ "value": "literal string" }
{ "content": "<base64 of a file>" }
{ "generate": { "length": 32, "charset": "alphanumeric|hex|base64|printable" } }
```

The server seals the value with the cluster key before writing it. If this node
holds no key, the request fails with `409` and a message saying so.

## Cluster key

```
GET    /api/secrets/key                 read    fingerprint and whether we hold it
POST   /api/secrets/key                 { "key": "<base64>" } upload after an outage
```

Uploading writes the key to `secrets.key_file` on this node. It is refused if a
key is already present, so an upload cannot silently orphan every stored secret.

## Git backup

```
GET    /api/git-backup/status                      read    configured, deploy key, last export/pull
GET    /api/git-backup/webhook-secret               read    whether a webhook token is set (write-only value)
PUT    /api/git-backup/webhook-secret               { "token": "..." } set/replace the webhook token
POST   /api/git-backup/webhook/:token               git host -> cluster: trigger a pull, token in the url
POST   /api/git-backup/pull                         internal auth: same pull, for `clouduler git-backup pull`
POST   /api/git-backup/reconcile                    internal auth: reconcile git repo from Raft, for `clouduler git-backup reconcile`
```

The `backup:` fields themselves (repo url, age recipient, branch) go
through the normal `/api/config` flow, same as any other config section.
`webhook/:token` authenticates by the token in the path rather than a header -
see `docs/git-backup.md` for why - while `pull` authenticates the same
HMAC-over-cluster-key way every other node-to-node call does, since the cli
already holds that key. See `docs/git-backup.md`.

## Restic backups

```
GET    /api/restic/destinations                    read    id, name, kind, has_credentials
PUT    /api/restic/destinations                     create/update a destination (id in body)
DELETE /api/restic/destinations/:id                 remove a destination
GET    /api/restic/targets                          read    every drive's backup schedules
PUT    /api/restic/targets                           create/update a schedule (id in body)
DELETE /api/restic/targets/:id                       remove a schedule
POST   /api/restic/targets/:id/run                    trigger a run now, outside its schedule
GET    /api/restic/runs[?target=ID]                  read    run history, newest first
GET    /api/restic/destinations/:id/snapshots         read    restic snapshots at a destination
POST   /api/restic/restore/drive                      restore a whole drive
POST   /api/restic/restore/volume                     restore one volume
POST   /api/restic/restore/service                    restore every volume a service uses
GET    /api/restic/restore-requests                   read    restore request history, newest first
```

See `docs/backups.md`. Destination/target `PUT` carries `id` in the request
body rather than the url, so the same call creates or edits: an unknown id
creates, a known one updates. `password`/`ssh_private_key`/
`s3_access_key_id`+`s3_secret_access_key` are only accepted on
`PUT /api/restic/destinations`, sealed immediately, and omitted (rather than
echoed back) on read.

Restore is not performed by the handler. It submits a `RestoreRequest`
through raft and returns its id right away; whichever node actually holds the
target drive claims and executes it. `/api/events` already streams
`restic_runs`/`restore_requests` on every state change, so poll those rather
than this endpoint for live status - there is no separate progress stream.

## Drives and volumes

```
GET    /api/drives                      read    mode, serving node, per-node status
GET    /api/volumes                     read    generation, replicas, sync state
POST   /api/volumes/:drive/:volume/sync trigger a sync now
```

`sync` bumps the volume's generation (`TouchVolume`) rather than signalling
`cl-drive` directly: there is no channel from the api into that crate's sync
loop today. The next pass picks up the bumped generation as if the primary
had just written to the volume, which triggers a sync but is one step removed
from "trigger a sync now" in the literal sense.

## Configuration

```
GET    /api/config                      read    the cluster config as structured json
PUT    /api/config                      apply an edited config
POST   /api/config/validate             read    check without applying
```

`PUT` takes `{ "config": {...}, "rollback_after": "60s" }`. The new config is
applied and armed with a rollback timer; the ui must call
`POST /api/config/confirm` before it expires or the previous config is restored
automatically. `rollback_after` defaults to 60 seconds and rollback is on by
default, because the quickest way to lose a cluster is a config edit that breaks
the network you are editing it over.

```
POST   /api/config/confirm              keep the pending config
POST   /api/config/rollback             restore the previous one now
GET    /api/config/pending              read    what is armed and how long is left
```

## Certificates and DNS

```
GET    /api/certificates                read    fqdn, expiry, never keys
POST   /api/certificates/:fqdn/renew
GET    /api/tls-fingerprint             read    fingerprint of the cert this node is serving now
GET    /api/dns                         read    records clouduler owns
POST   /api/dns/reconcile               force a reconcile pass
```

`cl_raft::CertificateState` does not track an issuer name, so certificate
listing reports `fqdn`, `not_after` and `has_chain` rather than an issuer.
Neither `renew` nor `reconcile` has a direct channel into `cl-dns`, which owns
the ACME account and the reconcile loop: `renew` removes the stored
certificate so the next pass reissues it, and `reconcile` currently just
accepts the request (`202`) without a way to force a pass early.
