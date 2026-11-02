import type { MockDb } from '../db';
import freshSingleNode from './fresh-single-node';
import lan3Nodes from './lan-3-nodes';
import multisiteHeadscale from './multisite-headscale';
import nodeDownFailover from './node-down-failover';
import unplaceableStack from './unplaceable-stack';
import volumeTrouble from './volume-trouble';

/** Each entry builds a fresh MockDb, so state never leaks between mounts. */
export const scenarios: Record<string, () => MockDb> = {
  'fresh-single-node': freshSingleNode,
  'lan-3-nodes': lan3Nodes,
  'multisite-headscale': multisiteHeadscale,
  'node-down-failover': nodeDownFailover,
  'unplaceable-stack': unplaceableStack,
  'volume-trouble': volumeTrouble,
};

export const DEFAULT_SCENARIO = 'lan-3-nodes';

export function buildScenario(name: string): MockDb {
  const make = scenarios[name];
  if (!make) throw new Error(`unknown mock scenario "${name}"; have: ${Object.keys(scenarios).join(', ')}`);
  return make();
}
