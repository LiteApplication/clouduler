import { fireEvent, render, screen } from '@testing-library/react';
import { useState } from 'react';
import { describe, expect, it } from 'vitest';
import type { ClusterConfig } from '../../api/types';
import { FailoverSection } from './sections';

function Harness({ failover }: { failover: ClusterConfig['failover'] }) {
  const [draft, setDraft] = useState({ failover } as ClusterConfig);
  return <FailoverSection draft={draft} setDraft={setDraft} disabled={false} original={draft} />;
}

describe('FailoverSection fence margin warning', () => {
  it('appears and disappears as the fields are edited', () => {
    render(<Harness failover={{ node_timeout: '60s', fence_timeout: '15s', probe_interval: '5s', stop_grace: '30s' }} />);
    expect(screen.queryByRole('alert')).toBeNull();

    fireEvent.change(screen.getByLabelText('Stop grace'), { target: { value: '45s' } });
    expect(screen.getByRole('alert').textContent).toMatch(/two writers/);

    fireEvent.change(screen.getByLabelText('Node timeout'), { target: { value: '2m' } });
    expect(screen.queryByRole('alert')).toBeNull();
  });
});
