import { fireEvent, render, screen } from '@testing-library/react';
import { useState } from 'react';
import { describe, expect, it, vi } from 'vitest';
import type { ClusterConfig } from '../../api/types';
import { NetworksSection, serviceNamesOf } from './sections';

vi.mock('../../state/store', () => ({
  useClusterState: () => ({ stacks: { netbird: { spec: 'services:\n  router: {}\n  other: {}\n' } } }),
}));

let latest: ClusterConfig;
function Harness({ initial }: { initial: ClusterConfig }) {
  const [draft, setDraft] = useState(initial);
  latest = draft;
  return <NetworksSection draft={draft} setDraft={setDraft} disabled={false} original={initial} />;
}

const base = {
  networks: {
    lan: { type: 'private', subnet: '192.168.1.0/24', domain: null },
    tun: { type: 'private', subnet: '10.9.0.0/24', domain: null },
  },
  nodes: { n1: { network: {}, drives: {} }, n2: { network: {}, drives: {} } },
} as unknown as ClusterConfig;

describe('NetworksSection container kind', () => {
  it('initialises the container block on switch and drops it on switching away', () => {
    render(<Harness initial={base} />);
    fireEvent.change(screen.getByLabelText('Network type', { selector: '#net-tun-type' }), { target: { value: 'container' } });
    expect(latest.networks.tun.type).toBe('container');
    expect(latest.networks.tun.container).toEqual({
      stack: '', service: '', hosts: [], allowed_networks: [], open_ports: [], proxy: false,
    });
    expect(screen.getAllByLabelText('DNS domain').length).toBeGreaterThan(0);
    fireEvent.change(document.querySelector('#net-tun-type')!, { target: { value: 'private' } });
    expect(latest.networks.tun.container).toBeUndefined();
  });

  it('writes the form fields into the draft', () => {
    render(<Harness initial={base} />);
    fireEvent.change(document.querySelector('#net-tun-type')!, { target: { value: 'container' } });
    fireEvent.change(document.querySelector('#net-tun-container-stack')!, { target: { value: 'netbird' } });
    fireEvent.change(screen.getByLabelText('Router service'), { target: { value: 'router' } });
    fireEvent.change(screen.getByLabelText('Routed address range'), { target: { value: '100.64.0.0/10' } });
    fireEvent.click(screen.getByLabelText('n1'));
    fireEvent.click(screen.getByLabelText('lan'));
    fireEvent.click(screen.getByLabelText('Let the router reach the reverse proxy (80/443)'));
    fireEvent.click(screen.getByText('Add port'));
    fireEvent.change(screen.getByLabelText('Port'), { target: { value: '2222' } });
    fireEvent.change(screen.getByLabelText('Target port (blank = same)'), { target: { value: '22' } });
    fireEvent.click(screen.getByText('Add'));
    expect(latest.networks.tun.container).toEqual({
      stack: 'netbird', service: 'router', hosts: ['n1'], allowed_networks: ['lan'],
      open_ports: [{ port: 2222, protocol: 'tcp', target_port: 22 }], proxy: true,
    });
    expect(latest.networks.tun.subnet).toBe('100.64.0.0/10');
  });

  it('parses service names from a stack spec', () => {
    expect(serviceNamesOf('services:\n  a: {}\n  b: {}\n')).toEqual(['a', 'b']);
    expect(serviceNamesOf('::: bad')).toBeNull();
  });
});
