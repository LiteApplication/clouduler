// A stack that can never be placed: it mounts a drive the cluster does not
// have and pins its service to a node. Used for the Placement tab docs.

import { ScenarioBuilder } from '../builders';
import { vaultwarden } from './stacks';

const scratch = `version: "3.8"

networks:
  lan: {}

services:
  app:
    image: nginx:1.27
    restart: always
    networks:
      - lan
    volumes:
      - fats/data:/data
    x-clouduler:
      node: atlas
`;

export default function unplaceableStack() {
  const b = new ScenarioBuilder({
    name: 'unplaceable-stack',
    description: 'A stack names a drive that does not exist and is pinned to a node, so it stays pending.',
    cluster: 'homelab',
  })
    .network('lan', { type: 'private', subnet: '192.168.1.0/24', domain: 'home.example.com' })
    .drive('fast', { mode: 'replicated', syncInterval: '15m' })
    .node({ name: 'atlas', ip: '192.168.1.10', cpus: 8, memGiB: 32, cpuPercent: 14, memUsedPercent: 46, drives: { fast: '/mnt/fast' } })
    .node({ name: 'boreas', ip: '192.168.1.11', cpus: 8, memGiB: 16, cpuPercent: 22, memUsedPercent: 58, drives: { fast: '/mnt/fast' } })
    .node({ name: 'cirrus', ip: '192.168.1.12', cpus: 4, memGiB: 16, cpuPercent: 9, memUsedPercent: 31, drives: { fast: '/mnt/fast' } })
    .stack('vaultwarden', vaultwarden('lan'), { placement: { app: 'cirrus' } })
    .stack('scratch', scratch);
  b.tweak((db) => {
    // Nothing was ever scheduled: drop what the builder derived for the stack.
    delete db.state.assignments['scratch.app.0'];
    delete db.live['scratch.app.0'];
    delete db.state.volumes['fats/data'];
    db.updates = db.updates.filter((u) => u.stack !== 'scratch');
    db.pendingInstances.push({
      id: 'scratch.app.0',
      node: '',
      desired: 'running',
      state: 'pending_placement',
      running: null,
      health: 'none',
      image: 'nginx:1.27',
      image_digest: null,
      started_at: null,
      migration: null,
      placement_error: 'no eligible node: drive fats does not exist',
      restart_count: 0,
      crash_moves: 0,
    });
  });
  return b.build();
}
