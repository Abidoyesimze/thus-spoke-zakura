import { describe, expect, it, vi, afterEach } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';
import { renderWithProviders, requestUrl } from '@/test/utils';
import { WalletActionsProvider } from './WalletActions';
import { WalletPage } from './WalletPage';

const TXID = 'ab'.repeat(32);
const PAYMENT_URI = 'zcash:uregtest1account2?amount=1.23456789&memo=cmVudCBmb3IgT2N0b2Jlcg';

function json(body: unknown): Promise<Response> {
  return Promise.resolve(
    new Response(JSON.stringify(body), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    }),
  );
}

function mockWalletApis(status = 'confirmed') {
  vi.spyOn(globalThis, 'fetch').mockImplementation((input, init) => {
    const url = requestUrl(input);
    if (url.includes('/accounts')) {
      return json([
        {
          id: 1,
          name: 'Account 1',
          unified_address: 'uregtest1account1verylongunifiedaddressfortruncation',
          transparent_address: 'tmAccount1',
          transparent_zatoshi: 0,
          ironwood_zatoshi: 500_000_000,
        },
        {
          id: 2,
          name: 'Account 2',
          unified_address: 'uregtest1account2',
          transparent_address: 'tmAccount2',
          transparent_zatoshi: 0,
          ironwood_zatoshi: 0,
        },
      ]);
    }
    if (url.includes('/activity')) {
      return json([
        {
          id: 'act-1',
          kind: 'faucet',
          from_account: null,
          to_account: 1,
          source_pool: 'ironwood',
          destination_pool: 'ironwood',
          amount_zatoshi: 500_000_000,
          txid: status === 'failed' ? '' : TXID,
          block_hash: 'cd'.repeat(32),
          status,
          last_error:
            status === 'failed'
              ? 'Insufficient funds'
              : status === 'broadcast'
                ? 'Lightwalletd unavailable'
                : null,
          created_at: '2026-09-18 10:00:00',
        },
      ]);
    }
    if (url.endsWith('/zip321/request')) return json({ uri: PAYMENT_URI });
    if (url.endsWith('/zip321/parse')) {
      const body = JSON.parse(typeof init?.body === 'string' ? init.body : '{}') as {
        uri?: string;
      };
      if (body.uri !== PAYMENT_URI) throw new Error(`Unexpected payment URI: ${body.uri}`);
      return json({
        address: 'uregtest1account2',
        destination_pool: 'ironwood',
        to_account: 2,
        amount_zatoshi: 123_456_789,
        memo: 'rent for October',
      });
    }
    if (url.endsWith('/send/quote')) {
      return json({
        available_zatoshi: 500_000_000,
        fee_zatoshi: 10_000,
        max_zatoshi: 499_990_000,
      });
    }
    return json({
      instance: 'default',
      node: null,
      account_count: 5,
      auto_mine: true,
      network: 'regtest',
    });
  });
}

function renderWallet(status = 'confirmed') {
  mockWalletApis(status);
  return renderWithProviders(
    <MemoryRouter>
      <WalletActionsProvider>
        <WalletPage />
      </WalletActionsProvider>
    </MemoryRouter>,
  );
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe('WalletPage', () => {
  it('uses the wrapping page rhythm the explorer and network surfaces share', async () => {
    const { container } = renderWallet();
    await waitFor(() => expect(screen.getByText('Account 1')).toBeInTheDocument());

    expect(container.firstElementChild).toHaveClass('grid', 'gap-4');
    expect(screen.getByRole('button', { name: 'Send ZEC' }).parentElement).toHaveClass('flex-wrap');
  });

  it('opens a payment request dialog beside the send action', async () => {
    renderWallet();
    const send = await screen.findByRole('button', { name: 'Send ZEC' });
    const request = screen.getByRole('button', { name: 'Request' });
    expect(request.parentElement).toBe(send.parentElement);

    await userEvent.click(request);
    expect(screen.getByRole('dialog', { name: 'Request ZEC' })).toBeInTheDocument();
  });

  it('fills Send from a generated payment request', async () => {
    Object.assign(Element.prototype, {
      hasPointerCapture: () => false,
      releasePointerCapture: () => undefined,
      scrollIntoView: () => undefined,
    });
    renderWallet();
    await screen.findByText('Account 2');

    await userEvent.click(screen.getByRole('button', { name: 'Request' }));
    await userEvent.click(screen.getByLabelText('Receiving account'));
    await userEvent.click(await screen.findByRole('option', { name: 'Account 2' }));
    await userEvent.type(screen.getByLabelText('Amount (ZEC, optional)'), '1.23456789');
    await userEvent.type(screen.getByLabelText('Memo (optional)'), 'rent for October');
    await userEvent.click(screen.getByRole('button', { name: 'Generate request' }));
    expect(
      (await screen.findByRole('region', { name: 'Generated payment request' })).querySelector(
        'code',
      ),
    ).toHaveTextContent(PAYMENT_URI);

    await userEvent.click(screen.getByRole('button', { name: 'Close dialog' }));
    await userEvent.click(screen.getByRole('button', { name: 'Send ZEC' }));
    const uri = screen.getByLabelText('Payment request (optional)');
    await userEvent.click(uri);
    await userEvent.paste(PAYMENT_URI);
    await userEvent.click(screen.getByRole('button', { name: 'Apply' }));

    await waitFor(() =>
      expect(screen.getByLabelText('Destination')).toHaveTextContent('Account 2'),
    );
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('1.23456789');
    expect(screen.getByLabelText('Memo (optional)')).toHaveValue('rent for October');
  });

  it('does not dump a full transaction id into the activity table', async () => {
    renderWallet();
    const tx = await screen.findByTitle(TXID);
    expect(tx).not.toHaveTextContent(TXID);
    expect(tx.textContent?.includes('…')).toBe(true);
  });

  it.each(['failed', 'broadcast'])(
    'describes %s payment errors and links only prepared transactions',
    async (status) => {
      renderWallet(status);
      expect(await screen.findByText('1 event')).toBeInTheDocument();
      const transfer = screen.getByRole('cell', { name: 'Faucet → Account 1' });
      const row = transfer.closest('tr')!;
      const detail = row.nextElementSibling as HTMLElement;
      const message =
        status === 'failed'
          ? 'Failed: Insufficient funds'
          : 'Latest execution error: Lightwalletd unavailable';
      expect(within(detail).getByText(message)).toBeInTheDocument();
      expect(transfer).toHaveAccessibleDescription(message);
      expect(within(detail).queryByRole('link')).not.toBeInTheDocument();
      if (status === 'failed') {
        expect(within(row).getByText('Not prepared')).toBeInTheDocument();
        expect(within(row).queryByRole('link')).not.toBeInTheDocument();
      } else {
        const link = within(transfer).getByRole('link');
        expect(link).toHaveAccessibleDescription(message);
        expect(link).toHaveAttribute('href', `/explorer/tx/${TXID}`);
      }
    },
  );
});
