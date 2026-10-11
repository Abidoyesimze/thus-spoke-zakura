import { describe, expect, it, vi, afterEach } from 'vitest';
import { screen } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { renderWithProviders, requestUrl } from '@/test/utils';
import { BlockDetail } from './BlockDetail';

const IRONWOOD_ROOT = '1f'.repeat(32);
const ORCHARD_ROOT = 'ae'.repeat(32);

afterEach(() => {
  vi.restoreAllMocks();
});

function renderBlock(block: Record<string, unknown>) {
  vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
    const ok = requestUrl(input).includes('/blocks/106');
    return Promise.resolve(
      new Response(JSON.stringify(ok ? block : { error: { message: 'unexpected', status: 404 } }), {
        status: ok ? 200 : 404,
        headers: { 'content-type': 'application/json' },
      }),
    );
  });

  renderWithProviders(
    <MemoryRouter initialEntries={['/explorer/block/106']}>
      <Routes>
        <Route path="/explorer/block/:id" element={<BlockDetail />} />
      </Routes>
    </MemoryRouter>,
  );
}

const block = {
  hash: 'bc'.repeat(32),
  height: 106,
  time: 1_296_688_602,
  size: 9_500,
  nTx: 2,
  confirmations: 1,
  tx: [],
  finalorchardroot: ORCHARD_ROOT,
};

const SAPLING_ROOT = '5a'.repeat(32);

describe('BlockDetail', () => {
  it('shows the Ironwood root and hides the roots of empty pools', async () => {
    // A block on this chain: Orchard and Sapling trees are empty, so `trees` has only
    // Ironwood, yet the node still reports their (empty-tree) roots.
    renderBlock({
      ...block,
      finalironwoodroot: IRONWOOD_ROOT,
      finalsaplingroot: SAPLING_ROOT,
      trees: { ironwood: { size: 10 } },
    });

    expect(await screen.findByText('Ironwood root')).toBeInTheDocument();
    expect(screen.getByText(IRONWOOD_ROOT)).toBeInTheDocument();
    expect(screen.queryByText('Orchard root')).not.toBeInTheDocument();
    expect(screen.queryByText('Sapling root')).not.toBeInTheDocument();
  });

  it('shows Orchard and Sapling roots once their trees hold notes', async () => {
    renderBlock({
      ...block,
      finalsaplingroot: SAPLING_ROOT,
      trees: { sapling: { size: 2 }, orchard: { size: 4 } },
    });

    expect(await screen.findByText('Orchard root')).toBeInTheDocument();
    expect(screen.getByText(ORCHARD_ROOT)).toBeInTheDocument();
    expect(screen.getByText(SAPLING_ROOT)).toBeInTheDocument();
    expect(screen.queryByText('Ironwood root')).not.toBeInTheDocument();
  });
});
