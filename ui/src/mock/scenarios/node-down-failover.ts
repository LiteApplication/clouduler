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

    // A warning still inside its debounce window, and resolved/past entries.
    db.alertSettings.settings.display_pending_warnings = true;
    const startOfDay = new Date();
    startOfDay.setHours(0, 0, 0, 0);
    const dayStart = Math.floor(startOfDay.getTime() / 1000);
    db.alerts.active.push({
      id: 'alert-mem-atlas',
      event: 'node.memory_high',
      category: 'nodes',
      severity: 'warning',
      state: 'pending',
      title: 'Memory is high on atlas',
      detail: 'Memory at 94% for 3m, after taking over boreas workloads.',
      subject: 'node:atlas',
      node: 'atlas',
      opened_at: db.now - 180,
      last_at: db.now - 10,
      count: 1,
      links: [{ label: 'Open node', path: '/nodes/atlas' }],
      excerpt: [],
      deliveries: [],
    });
    db.alerts.recent.push(
      {
        id: 'alert-cert-resolved',
        event: 'cert.expiring',
        category: 'certificates',
        severity: 'warning',
        state: 'resolved',
        title: 'Certificate for photos.home.example.com renewed',
        detail: 'The certificate had 12 days left and was renewed.',
        subject: 'cert:photos.home.example.com',
        opened_at: db.now - 7200,
        last_at: db.now - 3600,
        resolved_at: db.now - 3600,
        count: 1,
        links: [],
        excerpt: [],
        deliveries: [{ destination: 'ops', at: db.now - 7190, ok: true, attempts: 1 }],
      },
      {
        id: 'alert-oom-yesterday',
        event: 'instance.oom_killed',
        category: 'instances',
        severity: 'critical',
        state: 'resolved',
        title: 'immich.redis.0 was killed for running out of memory',
        detail: 'The container hit its 128M limit and was restarted twice.',
        subject: 'instance:immich.redis.0',
        stack: 'immich',
        instance: 'immich.redis.0',
        node: 'cirrus',
        opened_at: dayStart - 5 * 3600,
        last_at: dayStart - 4 * 3600,
        resolved_at: dayStart - 4 * 3600,
        count: 2,
        links: [{ label: 'Open instance', path: '/instances/immich.redis.0' }],
        excerpt: ['1:M 01 Jan 03:12:07.101 # Out Of Memory', 'redis: killed by signal 9'],
        deliveries: [{ destination: 'ops', at: dayStart - 5 * 3600 + 2, ok: true, attempts: 1 }],
      },
    );
  });
  return b.build();
}
