// Two sites joined over a headscale tailnet; the public edge node runs the
// headscale control server and the only public-facing routes.

import { ScenarioBuilder } from '../builders';
import { headscale, notes } from './stacks';

export default function multisiteHeadscale() {
  const b = new ScenarioBuilder({
    name: 'multisite-headscale',
    description: 'Three sites (a cloud VPS, home and an office) joined over a tailnet, running headscale.',
    cluster: 'mesh',
  })
    .network('tailnet', { type: 'private', subnet: '100.64.0.0/10', domain: 'mesh.example.com', access: ['office', 'public'] })
    .network('office', { type: 'private', subnet: '10.20.0.0/24', access: ['tailnet'] })
    .network('public', { type: 'public', domain: 'example.com' })
    .drive('fast', { mode: 'replicated', syncInterval: '5m' })
    .node({
      name: 'atlas',
      ip: '100.64.0.1',
      site: 'cloud',
      cpus: 2,
      memGiB: 4,
      cpuPercent: 18,
      memUsedPercent: 52,
      extraNetworks: {
        public: { ipv4: '203.0.113.10', ipv6: null, domain: null, open_ports: [{ port: 80, protocol: 'tcp' }, { port: 443, protocol: 'tcp' }] },
      },
    })
    .node({ name: 'boreas', ip: '100.64.0.2', site: 'home', cpus: 8, memGiB: 16, cpuPercent: 12, memUsedPercent: 41 })
    .node({ name: 'cirrus', ip: '100.64.0.3', site: 'office', cpus: 4, memGiB: 8, cpuPercent: 6, memUsedPercent: 33 })
    .stack('headscale', headscale('public'), { placement: { headscale: 'atlas', ui: 'atlas' } })
    .stack('notes', notes('tailnet'), { placement: { app: 'boreas' } });
  b.tweak((db) => {
    db.state.nodes.atlas.open_ports = { public: [['tcp', 80], ['tcp', 443]] };
    db.state.nodes.atlas.labels = { site: 'cloud' };
    db.state.nodes.boreas.labels = { site: 'home' };
    db.state.nodes.cirrus.labels = { site: 'office' };
  });
  return b.build();
}
