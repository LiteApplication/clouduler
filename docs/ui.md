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
static assets into `ui/dist`. The Vite config sets `base: './'` so every
asset reference in the built `index.html` is relative, which lets the Rust
side serve `dist` from whatever path prefix the daemon is mounted at rather
than assuming it sits at the domain root.

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

## View structure

Routes are declared in `src/App.tsx`. The sidebar (`components/Layout.tsx`)
mirrors them; on phones it collapses into a drawer plus a bottom tab bar
(Home, Stacks, Instances, More). Every mutating control is rendered only for
admins; viewers see read-only pages.

**Cluster**

- `/` — **Overview**: leader, quorum (`reachable/total`), counts of nodes,
  stacks, instances and volumes; a healthy / quorum-lost / nodes-unreachable
  banner; donut charts for node availability (by role) and instance state; a
  node health table; a degraded-volumes table; empty-volume alerts.
- `/nodes`, `/nodes/:name` — list with a connectivity graph between nodes,
  drain/activate per row, a **Rebalance** action that moves instances to
  smooth load, and an **Add node** form (role, addresses) that generates the
  join command. Detail shows role, availability, reachability, instances,
  live resource usage, the node's own config (per-network addresses, open
  ports, drive paths, egress/WireGuard interfaces, scheduling flags, whether it
  may create the cluster key), and its daemon logs on demand. Admin actions:
  drain, activate, decommission (blocked while instances remain).

**Workloads**

- `/stacks`, `/stacks/:name`, `/stacks/_new` — list (revision, services,
  instances). Detail shows revision, config/secret counts, the instances table
  (desired, state, health, migration phase; a failing instance expands an
  inline log tail), a per-service **placement** table explaining why each node
  can or cannot run it (`ConstraintTable`), the read-only spec, and a CI
  **update webhook** URL (copy / regenerate). Deploy and redeploy use a
  CodeMirror YAML editor with schema-aware completion, file upload, validation
  against `POST /api/stacks/:name/validate` with errors shown per YAML path, a
  required-configs-and-secrets panel (upload or paste each file, standalone vs
  bundled marked), and a relation diagram. Also: copy spec, remove.
- `/instances`, `/instances/:id` — filterable list. Detail shows state, health,
  node, image and digest; actions restart, stop/start, update to `:latest`,
  move to an eligible node, and restore volumes (links into the restore
  wizard). Tabs: live **Logs** (follow, tail, search, ANSI colors), **Stats**
  (CPU, memory, network, disk IO) and **Placement**.
- `/updates` — every container's running digest against its registry, grouped
  into update available / not on its pin / up to date / unknown, with tile
  filters, text search, multi-select, per-row and bulk update, retry of failed
  updates, dismissable failures, per-instance progress (pulling, restarting,
  health watch), "pin running" for unpinned instances, and a registry accounts
  dialog (add, test, remove; passwords are write-only). Not polled: it
  refreshes on demand because each load costs a registry round trip.

**Data**

- `/configs-secrets` — configs (create, edit, delete) and secrets (paste,
  file upload, or generate with a length, character set and format preview;
  values are never shown), plus the **cluster key** panel: fingerprint, whether
  this node holds it, and upload after an outage.
- `/drives`, `/drives/:name` — drive modes (replicated or served), topology,
  per-node implementation and disk usage, per-volume generation, replica
  sync state and primary, and a manual **Sync now**. Drive detail lists
  implementing nodes, volumes and their backups. Empty-volume alerts offer to
  inspect (newest file age) and delete a suspicious empty volume.
- `/networks/:name` — a network's type, subnet, domain and the address each
  node has on it.
- `/backup/destinations`, `/backup/schedules`, `/backup/history`,
  `/backup/restore` — restic backups. Destinations are a node-local path, an
  SSH server or an S3 bucket (credentials write-only). Schedules set per-drive
  frequency, time of day, retention (keep last / keep within) and whether to
  stop containers, with run-now. History lists backup runs and restore
  requests. The restore wizard (what → source snapshot → confirm) restores a
  drive, a volume or a service, and can be deep-linked with `?scope=`.

**Edge**

- `/certificates` — certificate expiry, issuer and status with renew, and the
  DNS records clouduler owns with a manual reconcile.
- `/redirects` — three UI-managed rule sets that apply immediately and live
  outside `cluster.yaml`: host/path **redirects**, **static backends**
  (external targets with path prefix, response timeout, max body size) and
  **WireGuard ingress** rules (tunnel port to a target port on a node).

**Cluster configuration**

- `/configuration/*` — a form over the structured cluster config, split into
  pages grouped General (Cluster, Failover), Network & Access (Networks, Web
  UI, Reverse proxy, Cloudflare), Storage (Drives) and Security (Auth, Secrets,
  AI access, Git backup), with a per-page unsaved-change count. Networks cover
  the internal, private, public, egress, WireGuard and NAT kinds, plus rename
  of networks and drives across all references. Secrets for Cloudflare, OIDC
  and the git-backup webhook are entered here (write-only), and the git-backup
  deploy key is shown. `/configuration/review` shows a field-level diff linking
  back to each editor. Applying validates first, then arms an auto-rollback
  timer for network-affecting changes (a banner offers **Confirm** or
  **Roll back**).
- `/self-update` — upgrade the cluster binary from a GitHub release URL (with
  optional token), with run progress and cancel.
- `/ai-access` (shown only when MCP is enabled) — connected AI clients with
  their grant (read-only or admin) and revoke. `/oauth/consent` is the
  approval page an AI tool is sent to, where the user picks the grant.
- Login (`views/Login.tsx`) offers OIDC single sign-on or an API key. API keys
  are also created and revoked through the API.

Each view reads from the shared cluster state store rather than fetching its
own copy of `/api/state`; a few views (node/instance detail, drives) also
poll their own light endpoints (`/api/nodes/:name`, `/api/instances`,
`/api/drives`) on a short interval for data the state event doesn't carry,
like live resource usage. `/updates` is the exception that fetches but never
polls, for the reason given above.

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

## Auth, from the browser's side

On load, `AuthProvider` (`src/auth/AuthContext.tsx`) calls `GET /auth/me`.
Two outcomes:

- **200** — the identity and role (`admin` or `viewer`) come back, the app
  renders normally, and every admin-only control in the UI checks
  `useAuth().isAdmin` before rendering (not just before allowing the
  request — a viewer never sees a disabled-but-visible destructive button).
- **401** — `src/views/Login.tsx` renders instead. It offers `GET
  /auth/oidc/login` as a plain link (the browser leaves the SPA entirely for
  the provider's flow, then `/auth/oidc/callback` on the server sets a
  session cookie and redirects back), or an API key field. A key is stored
  in `sessionStorage` and attached as `Authorization: Bearer <key>` by a thin
  `fetch` wrapper installed at startup, since `EventSource` and normal
  `fetch` both need it and there's no cookie for the api-key path.

A `401` from any request, anywhere in the app, calls the same handler
(`setUnauthorizedHandler` in `src/api/client.ts`) that flips the identity
back to `null`, which drops the whole app back to the login screen — so a
session or key that stops working mid-use is handled once, centrally,
instead of being a special case every view has to think about.

## Styling and theme

Design tokens are CSS custom properties: palettes in `src/theme.css`, layout
and components in `src/styles.css`. `src/theme.ts` picks the active palette:
a sidebar switch offers light, dark or follow-system, the choice is kept in
`localStorage` (`clouduler-theme`), and "system" keeps tracking the OS while
the page is open. No utility-class framework and no component kit; views
compose a small shared set of classes (`.card`, `.table-wrap`, `.badge`,
`.field`, `.btn-row`, ...) plus real components in `src/components/` (Badge,
Donut, Meter, NodeGraph, RelationDiagram, ConstraintTable, FormFields,
StackSpecEditor, Icon, Logo, EmptyVolumeAlert, UpdatePrompt).

## Installable app

`src/pwa.ts` registers `public/sw.js`, which precaches only the app shell.
Cluster data is always fetched live, so an installed console opens offline
but never shows stale state. When a new build is waiting, `UpdatePrompt`
offers to reload. The manifest and icons are in `public/`.

## Dependencies

- `react` / `react-dom` — the framework.
- `react-router-dom` — client-side routing for the views above.
- `codemirror` and `@codemirror/*`, `@lezer/highlight` — the YAML editor for
  stack specs, with completion driven by `lib/stackSchema.ts`.
- `age-encryption` — generates the git-backup age keypair in the browser, so the private key never leaves the page.
- Dev-only: `vite` + `@vitejs/plugin-react`, `typescript`, `vitest` +
  Testing Library + `jsdom`. No chart library (donuts, meters and the node
  graph are small hand-written SVG/CSS components) and no state management
  library (context + `useReducer` covers the two stores).
