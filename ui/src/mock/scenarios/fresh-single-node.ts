// A just-initialised cluster: one node, one drive, nothing deployed.

import { ScenarioBuilder } from '../builders';

export default function freshSingleNode() {
  return new ScenarioBuilder({
    name: 'fresh-single-node',
    description: 'A freshly initialised one-node cluster with no stacks.',
    cluster: 'homelab',
  })
    .network('lan', { type: 'private', subnet: '192.168.1.0/24', domain: 'home.example.com' })
    .drive('fast', { mode: 'replicated' })
    .node({ name: 'atlas', ip: '192.168.1.10', role: 'manager', cpus: 4, memGiB: 8, cpuPercent: 3, memUsedPercent: 14 })
    .build();
}
