# Web UI

The UI lives in `ui/`, a React 18 + TypeScript app built with Vite. It talks
to the HTTP API described in `docs/api.md` and nothing else; there is no
server-side rendering and no separate backend for the UI itself.

## Building

```
cd ui
npm install
npm run build
```

`npm run build` type-checks with `tsc` and then runs `vite build`, emitting
static assets into `ui/dist`. The Vite config sets `base: '/'`: the daemon
always serves `dist` from the domain root, and absolute asset references keep
a hard reload of a deep route (say `/stacks/web`) working, where relative ones
would resolve against the route.

`npm run dev` starts a Vite dev server on its own port and proxies `/api`
and `/auth` to `http://localhost:8443` (see `ui/vite.config.ts`) so the UI
can be developed against a locally running daemon without CORS.

`npm test` runs the Vitest suite (unit tests plus React Testing Library
component tests). Both `npm test` and `npm run build` are expected to pass
before a change lands.

## How it's served

The daemon serves `ui/dist` as static files at whatever address `web:` in
the cluster config points to, over HTTPS, on the current raft leader. A
request that lands on a follower gets a `503` with a `Location` header
pointing at the leader; the API client (`src/api/client.ts`) recognizes this
shape centrally and throws a typed `NotLeaderError` rather than leaking a
generic HTTP failure into every view.

## Screens and routes

The shell (`src/components/Layout.tsx`) is a side rail from 840px up and a
bottom bar below that. The rail groups Overview, Stacks, Nodes and Logs;
"Network & data" (Proxy, DNS, Storage); "System" (Updates, Settings); and a
collapsible "More" (Instances, one entry per network, and AI access when MCP
is on). On a phone the
bottom bar holds Overview, Stacks, Nodes, Logs and More, and `/more` lists
everything else with the account.

| Route | Screen |
| --- | --- |
| `/` | Overview: problems, cluster health, nodes and stacks |
| `/stacks`, `/stacks/:name` | Stacks, and one stack's containers, placement, spec and settings |
| `/stacks/new`, `/stacks/:name/deploy` | Deploy or redeploy, with live validation |
| `/instances`, `/instances/:id` | Every container, and one container |
| `/logs` | Container and node daemon logs |
| `/updates` | Image updates and registry accounts |
| `/nodes`, `/nodes/:name` | Nodes, who can reach whom, and one node |
| `/proxy/:tab?` | Routes, redirects, static backends, ingress rules |
| `/dns` | Certificates, DNS records, TLS fingerprint |
| `/storage/:tab?` | Volumes (drives and volumes) and Backups tabs |
| `/storage/drives/:name` | One drive |
| `/networks/:name` | One network |
| `/settings/:tab?` | Settings, see below |
| `/more` | The phone's list of everything else |

Settings is a menu grouped Cluster (General, Updates), Connectivity
(Network), Data (Storage, Secrets and configs) and Security (Sign-in and
backup, API keys and AI clients). The config tabs share one draft of
`cluster.yaml`; staged changes appear in a "Changes to apply" panel only
while there are some. Old URLs (`/configs-secrets`, `/drives`,
`/configuration/*`, `/certificates`, `/redirects`, `/backup/*`,
`/self-update`, `/ai-access`) redirect to their new place.

## Data, actions and vocabulary

This section lists what the UI can show and do, with no assumption about
layout. Everything is read from the HTTP API (`docs/api.md`); admin-only
actions are marked **(admin)**, and a viewer sees the data without the actions.
Write-only values (passwords, tokens, secret values) are never returned.

### Cluster

Data:
- Status: this node, leader, `has_quorum`, voters, managers reachable / total,
  binary version, state version.
- Nodes: `name`, `role` (manager | worker), `availability` (active | drain |
  decommissioned), reachable or in `unreachable`, `rpc_addr`, `arch`,
  `binary_hash`, `labels`, networks, `egress_networks`, `open_ports` per
  network, drives (name to path), memory total; live usage (CPU count, CPU %,
  1m load, memory used, uptime, container count, free and total space per
  drive); `removed` tombstones. Metrics for every node come in one call, and
  each node keeps a short history of CPU % and memory used.
- Last rebalance: when it ran, and the moves it made.
- Connectivity: per reporting node, a link to each peer with raft and API
  reachability and raft round-trip ms; nodes that have not reported.
- Daemon logs of a node (streamed).
- Emergency stack, when `clouduler emergency` is active.

Actions:
- Add node: name, role, addresses, generates the join command **(admin)**.
- Drain, activate **(admin)**; decommission and remove a node (refused while it
  still has instances) **(admin)**.
- Rebalance: moves instances to smooth load and returns the list of moves
  `instance from -> to` **(admin)**.
- Edit a node's config (see Cluster configuration).

### Stacks

Data:
- Stack: `name`, `spec` (compose yaml with `x-clouduler`), `revision`,
  `deployed_at`, the spec of the previous deploy, bundled `configs` and
  `secrets`, `used_configs`, `used_secrets`.
- Requirements, per stack and per service: networks, drives, named volumes
  (`drive/volume`), configs, secrets, file configs / file secrets, hosts,
  `pinned_node`, placement constraints, bound ports (network, protocol,
  published), whole-drive mounts; per stack, each network's `allowed` list as
  written.
- Placement: for each node, whether a service can run there and the reason it
  cannot (missing network, drive, port conflict, node pin, config or secret,
  availability, `colocate`).
- Validation result: valid flag plus errors as `path` and `message`.
- Update webhook path for the stack.

Actions:
- Deploy / redeploy a stack: spec, bundled config and secret files **(admin)**.
- Validate a spec without deploying.
- Remove a stack **(admin)**.
- Copy spec.
- Show and regenerate the stack's CI update webhook; a POST to it pulls and
  restarts every container of the stack **(admin)**.

Stack file vocabulary (`docs/stacks.md`):
- Top level: `networks`, `volumes`, `configs`, `secrets`, `services`.
- Network entry: optional `allowed` list restricting what the stack can reach.
  An entry is a `host` or `*.suffix` (checked by the reverse proxy, a `403` on
  any other host of a route published on that network), or `host:port`,
  `host:lo-hi`, `host:port/udp` (checked by the host firewall). Absent means
  unrestricted, `[]` means only the stack itself.
- Service: image, ports, volumes, `deploy`, `depends_on`, `entrypoint`,
  `working_dir`, `cap_add`, `cap_drop`, `sysctls`, `stop_grace_period`,
  config and secret mounts with `uid` / `gid`.
- `x-clouduler` (stack-wide or per service): `proxy` routes (`host`, `network`,
  `port`, `path_prefix`, `tls`, `middlewares`, `sticky`, `max_body_size`,
  `websockets`, `preserve_host`, `response_timeout`; a host of `@` publishes
  the network's apex domain; a trailing dot marks an absolute host outside
  Cloudflare), `expose`, `proxy_protocol`, `volume_sync` (`interval`,
  `enabled`, service-level `before` / `after` action), `auto_update`, `node`,
  `actions`, `docker_socket` (`target`, `read_only`, `mode`), `colocate`.
- Action definition: the key is the label; `cmd` (a string run as `sh -c`, or an
  exec array), `user`, `timeout` (default 30s, max 5m), `confirm`, `show_in_ui`.

### Instances

Data:
- Instance: `id` (stack, service, replica), `node`, `desired` (running |
  stopped), `state` (running, stopped, stopping, restarting, exited, pulling,
  pending_placement, unknown), `health` (healthy, unhealthy, starting, none),
  `image`, `image_digest`, running digest, `started_at`, `finished_at`,
  `exit_code`, `oom_killed`, memory limit, restart count and limit, crash
  moves, `placement_error`,
  proxy health (host, checked at, error), progress `reason` and `step` (n of m).
- Migration: target node and phase (stopping, syncing, verifying, switching,
  starting, done, failed with reason).
- Logs: timestamped lines tagged stdout or stderr, with follow, tail count and
  text search.
- Stats samples: CPU %, memory used / limit, network rx / tx, disk read / write.
- Available actions for the instance: name and whether it needs confirmation.

Actions:
- Restart; stop or start (set `desired`) **(admin)**.
- Update a `:latest` instance to its newest image **(admin)**.
- Move to another node, limited to schedulable, reachable nodes **(admin)**.
- Run a service action: executes the declared command in the container and
  returns `exit_code`, `stdout`, `stderr`, `duration_ms`, `timed_out` and truncation flags;
  asks first when `confirm` is set; hidden when `show_in_ui` is false **(admin)**.
- Restore the service's volumes from backup (starts a service-scope restore).

### Image updates

Data (per instance): stack, service, node, image, running digest, latest
registry digest, pinned digest, `update_available`, `pin_drift`, per-row
`error`. Derived status: update available, not on its pin, up to date, unknown.
Update progress per instance: phase (queued, pulling, stopping, starting,
started, failed), start time, last change, error. Registry accounts: server and
username.

Actions: update selected or all instances (accepted asynchronously, watched
through progress), retry failed updates, pin running digests for unpinned
instances, dismiss a failure, add / test / remove a registry account.

### Configs and secrets

Data: configs (`name`, `content`, `revision`, `owner` stack); secrets (`name`,
`revision`, `owner`, `key_fingerprint`; never the value); cluster key
fingerprint and whether this node holds it.

Actions: create, edit, delete a config; create, replace, delete a secret from a
pasted value, a file, or a generated value (`length` and `charset`:
alphanumeric, hex, base64, printable); upload the cluster key after an outage
**(admin)**.

### Drives and volumes

Data:
- Drive: `name`, `mode` (replicated | served), `served_by`, `active_server`,
  `sync_interval`, implementing nodes and their reachability.
- Volume (`drive/name`): `generation`, `primary`, `replicas` (node to
  generation, in sync when equal), `frozen` (node and the containers stopped for
  it), `sync_refused` (from, to, reason, time: replication refused to overwrite
  a populated copy), `looks_empty`.
- Volume inspection on a node (any holder, to compare copies): file count,
  bytes, newest file time.

Actions: sync a volume now **(admin)**; inspect an empty-looking volume;
freeze and unfreeze a volume (accepted asynchronously; the state shows the
result) **(admin)**; delete a volume everywhere **(admin)**; restore it from
backup. Choosing which copy of a sync-refused volume wins is deliberately not
offered.

### Backups (restic)

Data:
- Destination: `id`, `name`, kind `nodelocal` (node, path), `ssh` (host, port,
  user, path) or `s3` (bucket, endpoint, region, prefix); whether credentials
  are stored.
- Target (schedule): drive, destination, `enabled`, `frequency_secs`,
  `time_of_day`, `keep_last`, `keep_within_secs`, `stop_containers`, last run.
- Run: target, drive, destination, start and finish time, outcome (running with
  percent, succeeded with snapshot id and bytes and optional warning, failed
  with error), trigger (schedule or manual by whom).
- Snapshots of a destination: id, time, tags.
- Restore request: scope (drive, volume, or service), destination, snapshot,
  requested by and when, status (pending, running, succeeded, failed).

Actions: create, edit, delete destinations and targets; run a target now;
restore a drive, volume or service from a chosen snapshot or the latest **(admin)**.

### Networking, proxy and DNS

Data:
- Networks (kinds below), their subnet and domain, and each node's address.
- Proxy routes as the proxy currently serves them: host (`@` for the apex),
  path, kind (service, redirect, static, dashboard), stack, service, port,
  network, TLS, websockets, certificate expiry, and the last health check
  (node, time, healthy, error).
- Certificates: `fqdn`, issued and expiry (`not_after`) times, status (ready,
  pending, error with time). DNS records: `fqdn`, type, content, zone,
  provider id, status (ready, pending, error). The TLS fingerprint of the
  cluster.
- Redirect: `host`, `network`, `path_prefix`, `target`, `preserve_path`,
  `status` (301, 302, 307, 308).
- Static backend: `host`, `network`, `node`, `backend`, `path_prefix`, `tls`,
  `max_body_size`, `websockets`, `preserve_host`, `response_timeout`.
- WireGuard ingress rule: `network`, `port`, `protocol`, `target_port`.

Actions: renew a certificate, reconcile DNS; create, edit, delete redirects,
static backends and ingress rules (applied immediately, not part of
`cluster.yaml`); import or remove a WireGuard config per network and node
(stored sealed) **(admin)**.

### Cluster configuration

Data: the structured `cluster.yaml`, with a pending state (armed, seconds
left) while an auto-rollback timer runs, and a field-level diff between the
running and the edited config.

Sections: `cluster` (name); `failover` (`probe_interval`, `fence_timeout`,
`node_timeout`, `stop_grace`); `networks`; `web` (address, network, port,
`through_proxy`, `backend_port`); `proxy` (`enabled`, ports, ACME `enabled` /
`directory_url` / `contact_email`, `dns_publish` all-nodes | owning-node);
`cloudflare` (zones); `drives` (`mode`, `served_by`, `sync_interval`); `nodes`
(role, per-network addresses, ports, drive paths, `rpc_port`, data dirs,
`generate_secret_key`, `last_resort`); `auth` (API keys on or off, session TTL,
OIDC issuer, client id, scopes, admin claim and value, username claim);
`secrets` (key file, generate if missing); `mcp` (enabled); `backup` (git
repository URL, age recipient, branch); `self_update`.

Actions: validate, apply with a rollback window, confirm, roll back, discard
edits **(admin)**; rename a network or a drive across every reference; set
write-only secrets (Cloudflare API token, OIDC client secret, git-backup
webhook secret, GitHub token); generate the git-backup age keypair and show the
deploy public key; read git-backup status (last export with summary, commit and
error; last pull and any rejected pull with its preserved branch); export,
pull or reconcile the git backup now **(admin)**.

### Self-update

Data: release URL, GitHub token configured, the latest release (tag,
architectures, cached for five minutes) against the running version, run state (release tag, per-node
phase: idle, fetching, fetch failed, ready, restarting, done, restart failed;
start and finish time; failure reason). Actions: set repository, set token,
start, cancel **(admin)**.

### Access

Data: identity (`username`, `role` admin | viewer, `mcp_enabled`), API keys
(`id`, `name`, created), connected AI clients (`client_name`, `grant` read |
admin, approved by, created), pending OAuth requests (client name, redirect
host, requested and allowed grants; listed by the node that holds them).

Actions: sign in with OIDC or an API key; sign out; create (the key is shown
once) and delete API keys; approve or deny an AI client with a chosen grant;
revoke a client **(admin)**.

### Vocabulary

- **Manager / worker**: managers vote in raft and hold the database; workers
  only run containers.
- **Leader, quorum**: the manager accepting writes, and the majority needed
  for writes.
- **Drain / active / decommissioned**: whether a node takes work, is being
  emptied, or is retired.
- **Fenced, unreachable**: a node isolated from the cluster stops its own
  containers (`fence_timeout`); the leader reschedules them after `node_timeout`.
- **Emergency**: single-node recovery that restarts a chosen stack without quorum.
- **Stack, service, instance**: a deployed compose file, one service in it, and
  one running container of that service.
- **Revision, generation**: stack version; volume write version.
- **Desired vs actual state**: what should run against what Docker reports.
- **Placement**: choosing a node for an instance; **pinned** to a node by `node`,
  **colocated** with another service by `colocate`.
- **Migration / move**: relocating an instance and its volumes.
- **Pin (image)**: a recorded digest an instance must stay on; **drift** is
  running a different digest.
- **Action**: a named command declared by a service; **sync hook**: an action
  run before or after a volume replication.
- **Drive**: a named storage location configured on nodes; **replicated** copies
  data to several nodes, **served** exposes it from the nodes in `served_by`.
- **Volume**: a directory inside a drive; **primary** accepts writes,
  **replicas** follow; **frozen**: writes paused and containers stopped for
  manual edits; **sync refused**: replication blocked to avoid overwriting data.
- **Network kinds**: **internal** (the secure node-to-node network), **private**
  (overlay for stacks), **public** (published on the internet), **egress**
  (outbound through a chosen host interface), **wireguard** (outbound through a
  tunnel, with optional ingress), **nat** (outbound NAT).
- **Allowed list**: per-stack, per-network restriction on reachable hosts and ports.
- **Route, redirect, static backend**: proxy entries for stack services, for
  redirects, and for external targets.
- **Apex (`@`)**: the network's bare domain.
- **ACME / DNS-01**: automatic certificates proven through Cloudflare DNS.
- **Cluster key**: the key that seals secrets and authenticates nodes.
- **Sealed secret**: a secret stored encrypted with the cluster key.
- **Owner**: the stack that bundles a config or secret.
- **Git backup**: encrypted export of cluster state to a git repository;
  **restic backup**: volume snapshots to a destination.
- **Destination, target, run, snapshot, restore request**: restic's backup
  vocabulary above.
- **MCP**: the endpoint AI clients use; a **grant** is read-only or admin.
- **Auto-rollback**: a risky config change reverts unless confirmed in time.

## Live state

`src/state/store.tsx` opens exactly one `EventSource` against `/api/events`
for the whole app (`ClusterStoreProvider`, mounted once in `App.tsx`) and
keeps the latest `ClusterState` in a `useReducer` store
(`src/state/reducer.ts`). The API emits a `state` SSE event carrying the
full state body on every version bump; the reducer just swaps in the new
snapshot, ignoring anything with a lower `version` than what it already has
(guards against a stale redelivery on reconnect clobbering a newer one). No
view polls `/api/state` itself — `useClusterState()` just reads the
context.

Log and stats streams are different: they're per-instance/per-node, expensive
to keep open when nobody's looking, and use a different event shape (an
unnamed `message` event carrying one JSON line/sample, not a `state` event).
Those go through `src/hooks/useEventSource.ts`, opened only while the log or
stats panel is actually mounted, each keeping its own bounded ring buffer.

A second app-wide stream (`src/state/progressStore.tsx`) subscribes to
`/api/instances/events` and feeds per-instance update progress (pulling,
restarting, health watch) to the Updates and instance views.

Node metrics have no stream: `src/hooks/useNodeMetrics.ts` polls
`/api/nodes/metrics` every five seconds and keeps a short history per node
for the sparklines, and `src/hooks/useStatsHistory.ts` does the same for a
container's stats stream. Node daemon logs (`/api/nodes/:name/logs`, admin
only) replay the recent tail and then follow, one `log` event per line.

## Auth, from the browser's side

On load, `AuthProvider` (`src/auth/AuthContext.tsx`) calls `GET /auth/me`.
Two outcomes:

- **200** — the identity and role (`admin` or `viewer`) come back, the app
  renders normally, and every admin-only control in the UI checks
  `useAuth().isAdmin` before rendering (not just before allowing the
  request — a viewer never sees a disabled-but-visible destructive button).
- **401** — `src/views/workloads/Login.tsx` renders instead. It offers `GET
  /auth/oidc/login` as a plain link (the browser leaves the SPA entirely for
  the provider's flow, then `/auth/oidc/callback` on the server sets a
  session cookie and redirects back), or an API key field. The key is posted
  once to `POST /auth/login`, which answers with the same session cookie, so
  the browser never stores the key. Every `fetch` and `EventSource` then
  rides on that cookie; no `Authorization` header is set.

A `401` from any request, anywhere in the app, calls the same handler
(`setUnauthorizedHandler` in `src/api/client.ts`) that flips the identity
back to `null`, which drops the whole app back to the login screen — so a
session or key that stops working mid-use is handled once, centrally,
instead of being a special case every view has to think about.

## Styling and theme

Design tokens are CSS custom properties in `src/theme.css`: a dark grape and
a light cream palette, spacing, radii and type. `src/theme.ts` offers light,
dark and follow-system and remembers the choice in `localStorage`. Fonts
(Bricolage Grotesque, Hanken Grotesk, JetBrains Mono) are self-hosted, since
the UI must work offline.

The components live in `src/ds/`, a small in-house design system: buttons,
status badges, meters, cards, tables, tabs, chips, form fields, help tips,
sheets, confirm dialogs (`useConfirm`, with an optional text to type) and
toasts (`useToast`). `ds/ds.css` styles them and `ds/layout.css` holds the
page scaffolding (panels, rows, key-value lists, grids). A view's own CSS
sits next to it (`views/<area>/<area>.css`). `src/styles.css` only styles
the older config forms, scoped under a `.cfg` wrapper. There is no
utility-class framework. The app is an installable PWA whose service worker
precaches only the app shell; cluster data is always fetched live.

## Dependencies

- `react` / `react-dom` — the framework.
- `react-router-dom` — client-side routing for the views above.
- `codemirror` and `@codemirror/*`, `@lezer/highlight` — the YAML editor for
  stack specs, with completion driven by `lib/stackSchema.ts`.
- `age-encryption` — generates the git-backup age keypair in the browser, so the private key never leaves the page.
- `@fontsource/*` — the self-hosted fonts.
- Dev-only: `vite` + `@vitejs/plugin-react`, `typescript`, `vitest` +
  Testing Library + `jsdom`. No chart library (meters and sparklines are
  small hand-written SVG/CSS components) and no state management library
  (context + `useReducer` covers the stores).
