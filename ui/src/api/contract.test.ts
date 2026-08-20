// The api/ui contract, checked by the compiler.
//
// `api-shapes.json` is generated from real responses by a Rust test
// (`api_shapes_match_the_fixture_the_ui_typechecks_against` in
// crates/cl-api/src/tests.rs), which fails if the server's output drifts from
// it. Here each fixture is *assigned to the interface the ui uses*, so `tsc`
// reports an error when an interface declares a field the server does not
// send. Between the two, neither side can be renamed alone.
//
// This is a compile-time check first and a runtime test second: most of the
// value is in `npx tsc --noEmit` failing, which is why the assignments below
// exist even though they look like dead code. The runtime assertions catch the
// other direction — a field the server sends that the ui has not declared —
// which the compiler cannot see, because assigning a wider object to a
// narrower type is legal.
//
// After an intentional api change:
//     UPDATE_API_FIXTURES=1 cargo test -p cl-api api_shapes
//
// Deliberately NOT covered: endpoints whose bodies are arrays of items keyed
// by cluster contents the harness has none of (stacks, instances, drives,
// certificates). A fixture of `[]` proves nothing about the item shape, so
// pretending otherwise would be worse than the gap.

import { describe, expect, it } from 'vitest';
import shapes from './__fixtures__/api-shapes.json';
import type {
  ClusterConfig,
  ClusterKeyStatus,
  ClusterState,
  ClusterStatus,
  CloudflareTokenStatus,
  Identity,
  PendingConfig,
} from './types';

// If any of these stops compiling, the interface above it claims a field the
// server does not send. That is the bug that shipped three times.
const status: ClusterStatus = shapes.status;
const state: ClusterState = shapes.state as unknown as ClusterState;
const identity: Identity = shapes.identity as Identity;
const pending: PendingConfig = shapes.pendingConfig;
const config: ClusterConfig = shapes.clusterConfig as unknown as ClusterConfig;
const cloudflare: CloudflareTokenStatus = shapes.cloudflareTokenStatus;
const clusterKey: ClusterKeyStatus = shapes.clusterKeyStatus;

/** Keys the ui reads by name, per endpoint. */
const READ_BY_UI: Record<string, [object, string[]]> = {
  status: [status, ['has_quorum', 'leader', 'managers_total', 'managers_reachable', 'version', 'state_version']],
  identity: [identity, ['subject', 'role']],
  pendingConfig: [pending, ['armed', 'seconds_left']],
  cloudflareTokenStatus: [cloudflare, ['configured']],
  clusterKeyStatus: [clusterKey, ['fingerprint', 'held']],
  state: [state, ['nodes', 'stacks', 'assignments', 'unreachable', 'version']],
  clusterConfig: [config, ['cluster', 'networks', 'nodes', 'web', 'proxy']],
};

describe('api response shapes match what the ui reads', () => {
  for (const [endpoint, [body, keys]] of Object.entries(READ_BY_UI)) {
    it(`${endpoint} carries every field the ui reads`, () => {
      for (const key of keys) {
        expect(body, `${endpoint}.${key} is missing from the server response`).toHaveProperty(key);
      }
    });
  }

  // The specific regression: `quorum` was never a field the server sent, so
  // `!status.quorum` was `!undefined` — permanently true.
  it('reports quorum under the name the ui reads, and not the old one', () => {
    expect(status).toHaveProperty('has_quorum');
    expect(status).not.toHaveProperty('quorum');
    expect(typeof status.has_quorum).toBe('boolean');
  });

  // Same class, caught earlier: the config rollback countdown.
  it('reports the rollback countdown under the name the ui reads', () => {
    expect(pending).toHaveProperty('seconds_left');
    expect(pending).not.toHaveProperty('remaining_seconds');
  });
});
