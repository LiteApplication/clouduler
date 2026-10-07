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

Routes are declared in `src/App.tsx`:

- `/` — **Overview**: quorum and leader status, per-node health, counts of
  stacks/instances/volumes, and banners for anything degraded (lost quorum,
  unreachable nodes, out-of-sync volumes).
- `/nodes`, `/nodes/:name` — list and detail. Detail shows role, addresses,
  implemented drives with disk usage, live CPU/memory/uptime, and streams
  the node's daemon logs on demand. Admin actions: drain, activate,
  decommission, add a node.
- `/stacks`, `/stacks/:name` — list, detail, and deploy/redeploy (also used
  for `/stacks/_new`). Deploy accepts a pasted or uploaded yaml spec, runs it
  through `POST /api/stacks/:name/validate` before submitting, and renders
  validation errors against the yaml path they name.
- `/instances`, `/instances/:id` — state, health, image/digest, node.
  Actions: restart, stop/start, move (with a node picker limited to
  schedulable, reachable nodes), and update a `:latest` image. Live log
  viewer with follow/tail/search and a live stats panel, both opened
  on-demand SSE subscriptions independent of the main state stream.
- `/updates` — every container's running image digest against what its
  registry currently serves, with filters, multi-select, per-row and bulk update
  actions (instances queue and roll as described under Updates in the API
  docs, and failed ones can be retried), a "pin
  running" action for instances that carry no pin, and a dialog for registry
  accounts (added, tested against the registry, and removed; passwords are
  write-only). Deliberately **not** polled: each load costs one registry
  round trip per distinct image across the cluster, so it refreshes on
  demand.
- `/configs-secrets` — configs and secrets lists, the three secret creation
  paths (paste, file, generate-with-preview), and the cluster key panel
  (fingerprint, whether this node holds it, upload-after-outage).
- `/drives` — drive mode/served-by/per-node implementation status, and
  per-volume generation/replica/sync-state with a manual sync trigger.
- `/configuration` — a real form over the structured cluster config (not a
  text box), with a diff preview and the auto-rollback confirm flow.
- `/certificates` — expiry, issuer status, renew action, and the DNS records
  clouduler owns, with a manual reconcile trigger.

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

## Styling

One token stylesheet, `src/styles.css`: CSS custom properties for color,
spacing and radius on `:root`, redefined under
`@media (prefers-color-scheme: dark)` so the app follows the OS theme with
no toggle or JS. No utility-class framework and no component kit; views
compose a small shared set of classes (`.card`, `.table-wrap`, `.badge`,
`.field`, `.btn-row`, ...) plus a few real components
(`src/components/Badge.tsx`, `src/components/FormFields.tsx`) for anything
with actual behavior, like the validated config-form inputs.

## Dependencies

- `react` / `react-dom` — the framework.
- `react-router-dom` — client-side routing for the view list above; the
  alternative was hand-rolling history/route matching, which routers exist
  specifically to not need.
- Dev-only: `vite` + `@vitejs/plugin-react` (build/dev server), `typescript`,
  `vitest` + `@testing-library/react` + `@testing-library/jest-dom` +
  `@testing-library/user-event` + `jsdom` (tests). Nothing else — no chart
  library (stats are shown as plain numeric tiles, per-sample), no state
  management library (context + `useReducer` covers one store), no CSS
  framework.
