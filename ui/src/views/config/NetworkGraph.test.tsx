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

const openNet = (name: string) => fireEvent.click(screen.getByLabelText(`${name} network settings`));

describe('network access graph', () => {
  it('opens the settings sheet when a node is clicked', () => {
    render(<Harness initial={base} />);
    expect(screen.queryByRole('dialog')).toBeNull();
    openNet('lan');
    expect(screen.getByRole('dialog')).toBeTruthy();
    expect(document.querySelector('#net-lan-type')).toBeTruthy();
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
});
