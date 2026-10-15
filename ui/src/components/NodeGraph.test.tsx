import { render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { NodeGraph } from './NodeGraph';

vi.mock('../api/client', () => ({
  api: {
    nodeConnectivity: () =>
      Promise.resolve({
        reports: [
          { node: 'a', reported: true, links: [{ peer: 'b', raft: true, api: true, raft_ms: 1 }, { peer: 'c', raft: false, api: false, raft_ms: null }] },
          { node: 'b', reported: true, links: [{ peer: 'a', raft: false, api: false, raft_ms: null }, { peer: 'c', raft: false, api: false, raft_ms: null }] },
          { node: 'c', reported: false, links: [] },
        ],
      }),
  },
}));

describe('NodeGraph', () => {
  it('draws one-sided and broken links distinctly, and flags a silent node', async () => {
    render(<NodeGraph names={['a', 'b', 'c']} />);
    await waitFor(() => expect(screen.getByTestId('edge-a-b')).toHaveAttribute('data-status', 'half'));
    expect(screen.getByTestId('edge-a-c')).toHaveAttribute('data-status', 'broken');
    expect(screen.getByTestId('edge-b-c')).toHaveAttribute('data-status', 'broken');
    expect(screen.getByTestId('vertex-c')).toHaveAttribute('data-reported', 'false');
    expect(screen.getByTestId('vertex-a')).toHaveAttribute('data-reported', 'true');
  });
});
