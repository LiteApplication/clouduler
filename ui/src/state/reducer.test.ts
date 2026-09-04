import { describe, expect, it } from 'vitest';
import { initialStoreState, reducer } from './reducer';
import type { ClusterState } from '../api/types';

function state(version: number): ClusterState {
  return {
    nodes: {},
    stacks: {},
    assignments: {},
    volumes: {},
    configs: {},
    secrets: {},
    certificates: {},
    dns_records: {},
    cert_status: {},
    dns_status: {},
    wrapped_keys: {},
    cluster_config: null,
    emergency_stack: null,
    restic_destinations: {},
    restic_targets: {},
    restic_runs: {},
    restore_requests: {},
    unreachable: [],
    removed: [],
    overlay: null,
    version,
  };
}

describe('cluster store reducer', () => {
  it('starts connecting with no cluster state', () => {
    expect(initialStoreState.status).toBe('connecting');
    expect(initialStoreState.cluster).toBeNull();
  });

  it('adopts the first state event it receives', () => {
    const next = reducer(initialStoreState, { type: 'state', cluster: state(1) });
    expect(next.cluster?.version).toBe(1);
    expect(next.status).toBe('open');
  });

  it('replaces state on a newer version', () => {
    const s1 = reducer(initialStoreState, { type: 'state', cluster: state(1) });
    const s2 = reducer(s1, { type: 'state', cluster: state(2) });
    expect(s2.cluster?.version).toBe(2);
  });

  it('ignores a stale event with a lower version than what it already has', () => {
    const s1 = reducer(initialStoreState, { type: 'state', cluster: state(5) });
    const s2 = reducer(s1, { type: 'state', cluster: state(3) });
    expect(s2.cluster?.version).toBe(5);
  });

  it('records an error message without dropping the last known state', () => {
    const s1 = reducer(initialStoreState, { type: 'state', cluster: state(1) });
    const s2 = reducer(s1, { type: 'error', message: 'lost connection' });
    expect(s2.cluster?.version).toBe(1);
    expect(s2.error).toBe('lost connection');
  });

  it('clears a prior error once state flows again', () => {
    const s1 = reducer(initialStoreState, { type: 'error', message: 'oops' });
    const s2 = reducer(s1, { type: 'state', cluster: state(1) });
    expect(s2.error).toBeNull();
  });
});
