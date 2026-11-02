# Mock backend

An in-memory fake of the Clouduler API, so the real UI can run with no server:
in the docs site, in screenshots, in tests. Nothing in `src/mock` is imported by
the production entry point (`src/main.tsx`).

## How it plugs in

All network access in the app goes through `src/api/transport.ts`
(`getTransport()`: `fetch` and `eventSource`). In production that is the browser
globals. `src/main.mock.tsx` calls `installMock(scenario)`, which swaps in the
transport from `transport.ts` here, backed by a `MockDb` built from a scenario.

- `routes.ts`: one entry per endpoint of the `api` object in `api/client.ts`,
  plus the SSE streams (`/api/events`, `/api/instances/events`, instance and
  node logs, instance stats). Writes mutate the `MockDb` and call `commit()`,
  which bumps `state.version` and pushes a fresh `state` event, so embedded
  screens are clickable (deploy a stack, add a secret, restart, move...).
- An unknown route logs `[mock] ...` to the console and answers 501 with the path.

## Query parameters (`mock.html`)

| param      | values                            | default       |
|------------|-----------------------------------|---------------|
| `scenario` | a key of `scenarios/index.ts`     | `lan-3-nodes` |
| `route`    | initial in-app route, e.g. `/stacks/nextcloud` | `/` |
| `theme`    | `light` or `dark`                 | follow the OS |
| `chrome`   | `1` full shell, `0` hide the nav  | `1`           |

Example: `/ui-mock/mock.html?scenario=node-down-failover&route=/nodes&theme=dark&chrome=0`

Build with `npm run build:mock` (output: `docs/static/ui-mock/`, relative asset
URLs, no service worker).

## Adding a scenario

1. Create `scenarios/my-scenario.ts` exporting a function that returns a `MockDb`:

   ```ts
   import { ScenarioBuilder } from '../builders';
   import { vaultwarden } from './stacks';

   export default function myScenario() {
     return new ScenarioBuilder({ name: 'my-scenario', cluster: 'demo', description: '...' })
       .network('lan', { type: 'private', subnet: '10.0.0.0/24', domain: 'demo.example.com' })
       .drive('fast')
       .node({ name: 'a', ip: '10.0.0.1' })
       .stack('vaultwarden', vaultwarden('lan'), { placement: { app: 'a' } })
       .tweak((db) => { /* anything the builder does not cover */ })
       .build();
   }
   ```
2. Register it in `scenarios/index.ts`.

The builder derives instances, volumes, proxy routes, certificates, DNS records,
update rows and connectivity from what you declare. Stack specs only need the
compose subset `spec.ts` reads (service names, `image`, `replicas`, `volumes:
drive/vol:/path`, `networks`, `x-clouduler.proxy` hosts). Use example domains
(`example.com`) and documentation IPs only. The contract and render tests pick
the new scenario up automatically.

## How the contract tests prevent drift

- `contract.test.ts` requests every endpoint covered by
  `api/__fixtures__/api-shapes.json` (generated from the real Rust server, and
  checked by a cl-api test) from the mock, in every scenario, and compares with
  `shape.ts`: same keys in both directions, same JSON types, recursively. Keys
  the server only sends when set are whitelisted per endpoint in `OPTIONS`; each
  entry is a claim that the real server omits it when empty. It also assigns the
  scenario data to the `api/types.ts` interfaces, so `tsc` fails when a UI
  interface and the mock disagree.
- A test fails when `api-shapes.json` gains an endpoint that has no mock request.
- `coverage.test.tsx` calls every method of the `api` object against the mock
  (no 501 allowed) and renders `<App/>` at the main routes for each scenario.

After an intentional API change: regenerate the fixture
(`UPDATE_API_FIXTURES=1 cargo test -p cl-api api_shapes`), run `npm test`, and
update the mock where it now fails.
