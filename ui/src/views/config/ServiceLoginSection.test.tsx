import { fireEvent, render, screen } from '@testing-library/react';
import { useState } from 'react';
import { describe, expect, it, vi } from 'vitest';
import type { ClusterConfig } from '../../api/types';
import { ServiceLoginSection } from './sections';

vi.mock('../../api/client', async (orig) => ({
  ...(await orig<typeof import('../../api/client')>()),
  api: { proxyAuthClientSecret: () => Promise.resolve({ configured: false }), setProxyAuthClientSecret: vi.fn() },
}));

let latest: ClusterConfig;
function Harness() {
  const [draft, setDraft] = useState({
    networks: {
      internal: { type: 'internal', subnet: null, domain: 'home.example' },
      bare: { type: 'private', subnet: null, domain: null },
    },
    proxy: { enabled: true },
  } as unknown as ClusterConfig);
  latest = draft;
  return <ServiceLoginSection draft={draft} setDraft={setDraft} disabled={false} original={draft} />;
}

describe('ServiceLoginSection', () => {
  it('enables, picks a network and a palette', () => {
    render(<Harness />);
    expect(screen.queryByLabelText('Issuer URL')).toBeNull();
    fireEvent.click(screen.getByRole('switch', { name: 'Enable service login' }));
    expect(latest.proxy.auth?.host).toBe('login');
    expect(latest.proxy.auth?.palette).toBe('grape');

    fireEvent.click(screen.getByLabelText('internal'));
    expect(latest.proxy.auth?.networks).toEqual(['internal']);
    expect(screen.getByTestId('svclogin-portals').textContent).toContain('login.home.example');
    expect((screen.getByLabelText(/^bare/) as HTMLInputElement).disabled).toBe(true);

    fireEvent.change(screen.getByLabelText('Palette'), { target: { value: 'ocean' } });
    expect(latest.proxy.auth?.palette).toBe('ocean');

    fireEvent.click(screen.getByRole('switch', { name: 'Enable service login' }));
    expect(latest.proxy.auth).toBeNull();
  });
});
