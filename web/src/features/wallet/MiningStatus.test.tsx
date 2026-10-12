import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/react';
import type { MiningJob } from '@/lib/api';
import { MiningStatus } from './MiningStatus';

const running: MiningJob = {
  id: '00000000-0000-4000-8000-000000000001',
  requested_blocks: 9999,
  completed_blocks: 4966,
  state: 'mining',
  error: null,
  progress_uncertain: false,
};

describe('MiningStatus', () => {
  it('shows acknowledged progress with native accessible semantics', () => {
    render(<MiningStatus job={running} />);
    expect(screen.getByRole('progressbar', { name: 'Mining progress' })).toHaveAttribute(
      'value',
      '4966',
    );
    expect(screen.getByRole('progressbar')).toHaveAttribute('max', '9999');
    expect(screen.getByText(/4,966 of 9,999 blocks confirmed/)).toBeInTheDocument();
  });

  it.each(['syncing', 'completed'] as const)('describes %s after generation', (state) => {
    render(<MiningStatus job={{ ...running, state, completed_blocks: 9999 }} />);
    expect(
      screen.getByText(state === 'syncing' ? /Synchronizing wallet/ : /Mining completed/),
    ).toBeInTheDocument();
  });

  it('reports full-count synchronization failure without uncertainty', () => {
    render(
      <MiningStatus
        job={{
          ...running,
          state: 'failed',
          completed_blocks: 9999,
          error: 'Blocks mined, but wallet synchronization failed',
        }}
      />,
    );
    expect(screen.getByRole('alert')).toHaveTextContent(
      'Blocks mined, but wallet synchronization failed',
    );
    expect(screen.queryByText(/At least/)).not.toBeInTheDocument();
  });

  it('explains uncertain partial results without suggesting a retry', () => {
    render(
      <MiningStatus
        job={{ ...running, state: 'failed', progress_uncertain: true, error: 'RPC response lost' }}
      />,
    );
    expect(screen.getByText(/At least 4,966 of 9,999 blocks confirmed/)).toBeInTheDocument();
    expect(screen.getByText(/extra blocks may have committed/)).toBeInTheDocument();
    expect(screen.queryByText(/retry/i)).not.toBeInTheDocument();
  });
});
