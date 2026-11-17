import { fireEvent, render, screen } from '@testing-library/react';
import { useState } from 'react';
import { describe, expect, it } from 'vitest';
import type { ClusterConfig } from '../../api/types';
import { DrivesSection } from './sections';

let latest: ClusterConfig['drives'] = {};

function Harness({ drives }: { drives: ClusterConfig['drives'] }) {
  const [draft, setDraft] = useState({ drives, nodes: {} } as unknown as ClusterConfig);
  latest = draft.drives;
  return <DrivesSection draft={draft} setDraft={setDraft} disabled={false} original={draft} />;
}

describe('DrivesSection parallel syncs', () => {
  it('shows 1 by default and omits the key again when set back to 1', () => {
    render(<Harness drives={{ fast: { sync_interval: '5m', mode: 'replicated', served_by: [] } }} />);
    const field = screen.getByLabelText('Parallel syncs') as HTMLInputElement;
    expect(field.value).toBe('1');

    fireEvent.change(field, { target: { value: '3' } });
    expect(latest.fast.max_parallel_syncs).toBe(3);

    fireEvent.change(screen.getByLabelText('Parallel syncs'), { target: { value: '1' } });
    expect(latest.fast.max_parallel_syncs).toBeUndefined();
  });

  it('is offered for a served drive too', () => {
    render(<Harness drives={{ bulk: { sync_interval: '1h', mode: 'served', served_by: [], max_parallel_syncs: 2 } }} />);
    expect((screen.getByLabelText('Parallel syncs') as HTMLInputElement).value).toBe('2');
  });
});
