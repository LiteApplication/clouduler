import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { useState } from 'react';
import { describe, expect, it, vi } from 'vitest';
import type { ClusterConfig } from '../../api/types';
import { NetworksSection, renameNetworkInDraft } from './sections';

vi.mock('../../state/store', () => ({ useClusterState: () => ({ stacks: {} }) }));

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
  nodes: { n1: { network: {}, drives: {} } },
} as unknown as ClusterConfig;

const openNet = (name: string) => {
  const node = screen.getByLabelText(`${name} network settings`);
  fireEvent.pointerDown(node, { clientX: 10, clientY: 10, button: 0, pointerId: 1 });
  fireEvent.pointerUp(node, { clientX: 10, clientY: 10, button: 0, pointerId: 1 });
};

describe('network access graph', () => {
  it('opens the settings sheet when a node is clicked', () => {
    render(<Harness initial={base} />);
    expect(screen.queryByRole('dialog')).toBeNull();
    openNet('lan');
    expect(screen.getByRole('dialog')).toBeTruthy();
    expect(document.querySelector('#net-lan-type')).toBeTruthy();
  });

  it('does not open the sheet after a drag', () => {
    render(<Harness initial={base} />);
    const node = screen.getByLabelText('lan network settings');
    // jsdom has no PointerEvent, so build mouse events carrying the pointer types.
    const fire = (type: string, x: number) =>
      fireEvent(node, new MouseEvent(type, { bubbles: true, clientX: x, clientY: 10, button: 0 }));
    fire('pointerdown', 10);
    fire('pointermove', 40);
    fire('pointerup', 40);
    expect(screen.queryByRole('dialog')).toBeNull();
    openNet('lan');
    expect(screen.getByRole('dialog')).toBeTruthy();
  });

  it('opens the sheet from the keyboard', () => {
    render(<Harness initial={base} />);
    fireEvent.keyDown(screen.getByLabelText('lan network settings'), { key: 'Enter' });
    expect(screen.getByRole('dialog')).toBeTruthy();
  });

  it('toggles the per-network DNS switch', () => {
    const initial = {
      ...base,
      proxy: { enabled: true, dns_server: { enabled: true, networks: [], port: 53 } },
    } as unknown as ClusterConfig;
    render(<Harness initial={initial} />);
    openNet('lan');
    const sw = screen.getByRole('switch', { name: 'Serve cluster DNS on this network' });
    expect(sw.getAttribute('aria-checked')).toBe('true');
    fireEvent.click(sw);
    expect(latest.proxy.dns_server!.networks).toEqual(['tun']);
  });

  it('adds an edge via Can reach and can return to unrestricted', () => {
    render(<Harness initial={base} />);
    openNet('lan');
    expect(latest.networks.lan.access).toBeUndefined();
    fireEvent.click(screen.getByLabelText('tun'));
    expect(latest.networks.lan.access).toEqual(['tun']);
    fireEvent.click(screen.getByLabelText('tun'));
    expect(latest.networks.lan.access).toEqual([]);
    fireEvent.click(screen.getByText('Unrestricted host firewall'));
    expect('access' in latest.networks.lan).toBe(false);
  });

  it('removes an edge when its line is clicked and confirmed', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    const initial = {
      ...base,
      networks: { ...base.networks, lan: { ...base.networks.lan, access: ['tun'] } },
    } as unknown as ClusterConfig;
    render(<Harness initial={initial} />);
    fireEvent.click(screen.getByTestId('edge-lan-tun'));
    await waitFor(() => expect(latest.networks.lan.access).toEqual([]));
  });

  it('rewrites access entries on rename', () => {
    const draft = {
      ...base,
      web: { network: 'lan' },
      networks: {
        lan: { ...base.networks.lan, access: ['tun'] },
        tun: { ...base.networks.tun, access: ['lan'] },
      },
    } as unknown as ClusterConfig;
    const next = renameNetworkInDraft(draft, 'tun', 'vpn');
    expect(next.networks.lan.access).toEqual(['vpn']);
    expect(next.networks.vpn.access).toEqual(['lan']);
    expect(next.networks.tun).toBeUndefined();
  });

  it('rewrites the DNS server list on rename', () => {
    const draft = {
      ...base,
      web: { network: 'lan' },
      proxy: { enabled: true, dns_server: { enabled: true, networks: ['tun'], port: 53 } },
    } as unknown as ClusterConfig;
    expect(renameNetworkInDraft(draft, 'tun', 'vpn').proxy.dns_server!.networks).toEqual(['vpn']);
  });
});
