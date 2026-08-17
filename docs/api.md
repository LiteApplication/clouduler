# HTTP API

Served by the raft leader on `web.address:web.port` over HTTPS. Non-leaders
answer `503` with a `Location` header pointing at the leader, so a client that
hit the wrong node can retry without extra machinery.

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
everyone else who authenticates gets `viewer`.

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
GET    /api/status                      read    quorum, leader, versions
GET    /api/state                       read    the whole applied state, redacted
GET    /api/events                      read    SSE stream of state changes
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

Node detail includes CPU count and load, memory total and used, disk usage per
implemented drive, container counts, uptime, and the reachability flag.

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

Instance ids are `stack.service.index`.

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

## Drives and volumes

```
GET    /api/drives                      read    mode, serving node, per-node status
GET    /api/volumes                     read    generation, replicas, sync state
POST   /api/volumes/:drive/:volume/sync trigger a sync now
```

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
GET    /api/certificates                read    fqdn, expiry, issuer, never keys
POST   /api/certificates/:fqdn/renew
GET    /api/dns                         read    records clouduler owns
POST   /api/dns/reconcile               force a reconcile pass
```
