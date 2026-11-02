// A node dropped off the cluster: its containers are unknown, one database was
// already rescheduled, another service is waiting for room.

import type { AlertView } from '../../api/types';
import { ScenarioBuilder } from '../builders';
import { immich, nextcloud, vaultwarden } from './stacks';

export default function nodeDownFailover() {
  const b = new ScenarioBuilder({
    name: 'node-down-failover',
    description: 'boreas is unreachable; instances are being rescheduled onto the survivors.',
    cluster: 'homelab',
  })
    .network('lan', { type: 'private', subnet: '192.168.1.0/24', domain: 'home.example.com' })
    .drive('fast', { mode: 'replicated', syncInterval: '15m' })
    .drive('bulk', { mode: 'served', servedBy: ['atlas', 'boreas'], syncInterval: '1h' })
    .node({ name: 'atlas', ip: '192.168.1.10', cpus: 8, memGiB: 32, cpuPercent: 41, memUsedPercent: 62, drives: { fast: '/mnt/fast', bulk: '/mnt/bulk' } })
    .node({ name: 'boreas', ip: '192.168.1.11', down: true, drives: { fast: '/mnt/fast', bulk: '/mnt/bulk' } })
    .node({ name: 'cirrus', ip: '192.168.1.12', cpus: 4, memGiB: 16, cpuPercent: 37, memUsedPercent: 55, drives: { fast: '/mnt/fast' } })
    .stack('vaultwarden', vaultwarden('lan'), { placement: { app: 'atlas' } })
    .stack('nextcloud', nextcloud('lan'), {
      placement: { app: 'atlas', db: 'boreas', redis: 'boreas' },
      secrets: ['nextcloud-db-password'],
    })
    .stack('immich', immich('lan'), {
      placement: { server: 'atlas', 'machine-learning': 'boreas', redis: 'cirrus', db: 'cirrus' },
    });
  b.tweak((db) => {
    // nextcloud's database was already fenced and moved to a survivor.
    db.state.assignments['nextcloud.db.0'].node = 'cirrus';
    db.state.volumes['fast/nextcloud-db'].primary = 'cirrus';
    db.state.volumes['fast/nextcloud-db'].replicas = { cirrus: 3, atlas: 3 };
    db.live['nextcloud.db.0'] = {
      ...db.live['nextcloud.db.0'],
      state: 'restarting',
      running: false,
      health: 'starting',
      reason: 'rescheduled from boreas: starting container',
      step: [3, 4],
    };
    // redis is next in line and still waiting out the fence timeout.
    db.live['nextcloud.redis.0'] = {
      ...db.live['nextcloud.redis.0'],
      state: 'unknown',
      reason: 'waiting for boreas to be fenced (12s left)',
    };
    // nothing has room for a second ml worker.
    db.pendingInstances.push({
      id: 'immich.machine-learning.1',
      node: '',
      desired: 'running',
      state: 'pending_placement',
      running: null,
      health: 'none',
      image: 'ghcr.io/immich-app/immich-machine-learning:v1.118.0',
      image_digest: null,
      started_at: null,
      migration: null,
      placement_error: 'no eligible node: boreas is unreachable and cirrus has too little free memory',
      restart_count: 0,
      crash_moves: 0,
    });
    const alert: Omit<AlertView, 'id'> = {
      event: 'node.unreachable',
      category: 'nodes',
      severity: 'critical',
      state: 'active',
      title: 'Node boreas is unreachable',
      detail: 'No heartbeat for 47s; 3 instances are being rescheduled.',
      subject: 'node:boreas',
      node: 'boreas',
      opened_at: db.now - 47,
      last_at: db.now - 5,
      count: 1,
      links: [{ label: 'Open node', path: '/nodes/boreas' }],
      excerpt: ['raft: boreas did not answer 9 consecutive heartbeats'],
      deliveries: [{ destination: 'ops', at: db.now - 45, ok: true, attempts: 1 }],
    };
    db.alerts.active.push({ id: 'alert-node-boreas', ...alert });
    db.alerts.counts.critical += 1;
  });
  return b.build();
}
