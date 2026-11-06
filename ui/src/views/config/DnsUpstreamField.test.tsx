import { fireEvent, render, screen } from '@testing-library/react';
import { useState } from 'react';
import { describe, expect, it } from 'vitest';
import type { ClusterConfig } from '../../api/types';
import { ProxySection, parseUpstreamList } from './sections';

let latest: ClusterConfig;

function Harness({ upstream }: { upstream: string[] }) {
  const [draft, setDraft] = useState({
    networks: {},
    proxy: {
      enabled: false,
      acme: { enabled: false },
      dns_publish: 'all-nodes',
      dns_server: { enabled: true, networks: [], port: 53, upstream },
    },
  } as unknown as ClusterConfig);
  latest = draft;
  return <ProxySection draft={draft} setDraft={setDraft} disabled={false} original={draft} />;
}

describe('parseUpstreamList', () => {
  it('splits on commas and whitespace and drops blanks', () => {
    expect(parseUpstreamList('9.9.9.9, 1.1.1.1  8.8.8.8,')).toEqual(['9.9.9.9', '1.1.1.1', '8.8.8.8']);
    expect(parseUpstreamList('  ')).toEqual([]);
  });
});

describe('DNS server upstream field', () => {
  it('shows the saved resolvers and writes edits back as a list', () => {
    render(<Harness upstream={['9.9.9.9']} />);
    const input = screen.getByLabelText('DNS server upstream') as HTMLInputElement;
    expect(input.value).toBe('9.9.9.9');

    fireEvent.change(input, { target: { value: '9.9.9.9, ' } });
    // A trailing separator survives while typing.
    expect(input.value).toBe('9.9.9.9, ');
    expect(latest.proxy.dns_server?.upstream).toEqual(['9.9.9.9']);

    fireEvent.change(input, { target: { value: '9.9.9.9, 2606:4700:4700::1111' } });
    expect(latest.proxy.dns_server?.upstream).toEqual(['9.9.9.9', '2606:4700:4700::1111']);

    fireEvent.change(input, { target: { value: '' } });
    expect(latest.proxy.dns_server?.upstream).toEqual([]);
  });
});
