// The reference homelab: three managers on one LAN, a handful of stacks, a
// served bulk drive, proxy routes with certificates.

import { ScenarioBuilder } from '../builders';
import { immich, nextcloud, vaultwarden } from './stacks';

export default function lan3Nodes() {
  const b = new ScenarioBuilder({
    name: 'lan-3-nodes',
    description: 'Three nodes on one LAN running vaultwarden, nextcloud and immich.',
    cluster: 'homelab',
  })
    .network('lan', { type: 'private', subnet: '192.168.1.0/24', domain: 'home.example.com' })
    .drive('fast', { mode: 'replicated', syncInterval: '15m' })
    .drive('bulk', { mode: 'served', servedBy: ['atlas', 'boreas'], syncInterval: '1h' })
    .node({ name: 'atlas', ip: '192.168.1.10', cpus: 8, memGiB: 32, cpuPercent: 14, memUsedPercent: 46, drives: { fast: '/mnt/fast', bulk: '/mnt/bulk' } })
    .node({ name: 'boreas', ip: '192.168.1.11', cpus: 8, memGiB: 16, cpuPercent: 22, memUsedPercent: 58, drives: { fast: '/mnt/fast', bulk: '/mnt/bulk' } })
    .node({ name: 'cirrus', ip: '192.168.1.12', cpus: 4, memGiB: 16, cpuPercent: 9, memUsedPercent: 31, drives: { fast: '/mnt/fast' } })
    .stack('vaultwarden', vaultwarden('lan'), { placement: { app: 'cirrus' } })
    .stack('nextcloud', nextcloud('lan'), {
      placement: { app: 'atlas', db: 'boreas', redis: 'atlas' },
      secrets: ['nextcloud-db-password'],
      updateAvailable: ['app'],
    })
    .stack('immich', immich('lan'), {
      placement: { server: 'atlas', 'machine-learning': 'boreas', redis: 'cirrus', db: 'cirrus' },
      deployedAgoSecs: 86400 * 3,
    });
  b.tweak((db) => {
    db.state.cert_status['photos-old.home.example.com'] = 'Pending';
    db.backupDestinations.push({
      id: 'nas',
      name: 'Backup NAS',
      kind: { kind: 'ssh', host: 'nas.home.example.com', port: 22, user: 'backup', path: '/volume1/clouduler' },
      has_credentials: true,
    });
    db.state.restic_destinations.nas = {
      id: 'nas',
      name: 'Backup NAS',
      kind: { kind: 'ssh', host: 'nas.home.example.com', port: 22, user: 'backup', path: '/volume1/clouduler' },
      password_secret: '_clouduler_restic_nas',
    };
    db.state.restic_targets.t1 = { id: 't1', drive: 'fast', destination_id: 'nas', enabled: true, frequency_secs: 86400, time_of_day: '03:00', keep_last: 14, last_run: db.now - 7200 };
    const run = {
      id: 'run-1',
      target_id: 't1',
      drive: 'fast',
      destination_id: 'nas',
      started_at: db.now - 7200,
      finished_at: db.now - 7100,
      outcome: { Succeeded: { snapshot_id: '4f1a9c2e', bytes: 2_400_000_000 } },
      triggered_by: 'Schedule' as const,
    };
    db.backupRuns.push(run);
    db.state.restic_runs['run-1'] = run;
    db.snapshots.push({ id: '4f1a9c2e', time: new Date((db.now - 7100) * 1000).toISOString(), tags: ['fast'] });
  });
  return b.build();
}
