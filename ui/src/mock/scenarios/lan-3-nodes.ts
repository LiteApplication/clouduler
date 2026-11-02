// The reference homelab: three managers on one LAN, a handful of stacks, a
// served bulk drive, proxy routes with certificates.

import { encodeText } from '../../api/client';
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
    // A WireGuard network imported on atlas only, with one port let in through it.
    (db.config.networks as Record<string, unknown>).vpn = { type: 'wireguard', subnet: null, domain: null };
    (db.config.nodes.atlas.network as Record<string, unknown>).vpn = 'wg-vpn';
    db.state.nodes.atlas.networks.push('vpn');
    db.state.secrets['_clouduler_wg_vpn_atlas'] = {
      name: '_clouduler_wg_vpn_atlas',
      sealed: '',
      revision: 1,
      key_fingerprint: 'a1b2c3d4e5f60718',
      owner: null,
    };
    db.wireguardIngress.push({ id: 'wgin-1', network: 'vpn', port: 51820, protocol: 'udp', target_port: 51820 });

    // Reserved `_clouduler_*` configs, exactly as the server stores them (base64 json).
    const reserved = (name: string, value: unknown) => {
      db.state.configs[name] = { name, content: encodeText(JSON.stringify(value)), revision: 1, owner: null };
    };
    const exportedAt = db.now - 1800;
    db.config.self_update = { release_url: 'https://github.com/example/clouduler', check_updates: true, check_interval: '24h' };
    db.selfUpdate.repo = db.config.self_update;
    db.selfUpdate.run = {
      id: 'upd-1',
      release_tag: 'v0.9.10',
      owner: 'example',
      repo: 'clouduler',
      assets: {
        x86_64: { id: 1001, name: 'clouduler-x86_64' },
        aarch64: { id: 1002, name: 'clouduler-aarch64' },
      },
      arch_hashes: { x86_64: 'b3f1c07a5d2e48c9a6e0f4d17b8a92c35e61d0f7a4b82c93e5d10f6a7b8c9d0e' },
      per_node: { atlas: 'done', boreas: 'done', cirrus: 'done' },
      started_at: db.now - 86400 * 4,
      finished_at: db.now - 86400 * 4 + 240,
      failure_reason: null,
      restart_started_at: db.now - 86400 * 4 + 120,
    };
    reserved('_clouduler_self_update_run', db.selfUpdate.run);
    reserved('_clouduler_self_update_check', { checked_at: db.now - 3600, tag: 'v0.9.10', error: null });

    db.config.backup = { repo_url: 'git@git.example.com:homelab/clouduler-state.git', age_recipient: 'age1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqsxk7uzv', branch: 'main' };
    db.gitBackup = {
      configured: true,
      deploy_public_key: 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleDeployKeyOnlyForDocsXXXXXXXXXXXX clouduler-git-backup',
      last_export: { at: exportedAt, summary: 'update stack nextcloud', commit: '9c2e7b1', error: null },
      last_pull: { at: db.now - 86400, commit: null, rejected: null },
    };
    reserved('_clouduler_git_backup_last_export', db.gitBackup.last_export);
    reserved('_clouduler_git_backup_last_pull', db.gitBackup.last_pull);

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
