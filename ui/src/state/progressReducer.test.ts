import { describe, expect, it } from 'vitest';
import { initialProgressState, progressReducer } from './progressReducer';
import type { InstanceProgressEvent } from '../api/types';

const event = (over: Partial<InstanceProgressEvent> = {}): InstanceProgressEvent => ({
  instance_id: 'site.web.0',
  state: 'exited',
  reason: 'redeploying (exited, step 1/3)',
  step: [1, 3],
  ...over,
});

describe('progressReducer', () => {
  it('stores the first event for an instance', () => {
    const next = progressReducer(initialProgressState, { type: 'progress', event: event() });
    expect(next.byId['site.web.0']).toEqual({ reason: 'redeploying (exited, step 1/3)', step: [1, 3] });
  });

  it('overwrites a later event for the same instance', () => {
    const first = progressReducer(initialProgressState, { type: 'progress', event: event() });
    const second = progressReducer(first, {
      type: 'progress',
      event: event({ reason: 'redeploying (exited, step 2/3)', step: [2, 3] }),
    });
    expect(second.byId['site.web.0']).toEqual({ reason: 'redeploying (exited, step 2/3)', step: [2, 3] });
  });

  it('clears to steady-state when reason is null', () => {
    const first = progressReducer(initialProgressState, { type: 'progress', event: event() });
    const second = progressReducer(first, {
      type: 'progress',
      event: event({ state: 'running', reason: null, step: null }),
    });
    expect(second.byId['site.web.0']).toEqual({ reason: null, step: null });
  });

  it('resets everything on reconnect', () => {
    const first = progressReducer(initialProgressState, { type: 'progress', event: event() });
    const reset = progressReducer(first, { type: 'reset' });
    expect(reset.byId).toEqual({});
  });
});
