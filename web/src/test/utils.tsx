import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { render, type RenderResult } from '@testing-library/react';
import type { ReactElement, ReactNode } from 'react';
import { ToastProvider } from '@/components/ui/Toast';
import type { Account } from '@/lib/api';
import { MemoryRouter } from 'react-router-dom';
import { WalletActionsProvider } from '@/features/wallet/WalletActions';
import { PaymentUpdates } from '@/features/wallet/PaymentUpdates';
import { queryKeys } from '@/hooks/queries';

export function requestUrl(input: RequestInfo | URL): string {
  if (typeof input === 'string') return input;
  if (input instanceof URL) return input.href;
  return input.url;
}

export function renderWithProviders(ui: ReactElement): RenderResult {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });

  function Wrapper({ children }: { children: ReactNode }) {
    return (
      <QueryClientProvider client={queryClient}>
        <ToastProvider>{children}</ToastProvider>
      </QueryClientProvider>
    );
  }

  return render(ui, { wrapper: Wrapper });
}

/** Dialog tests share the same persistent owner as the application. */
export function renderWithWalletActions(ui: ReactElement, client?: QueryClient): RenderResult {
  const queryClient =
    client ??
    new QueryClient({
      defaultOptions: {
        queries: { retry: false, staleTime: Infinity },
        mutations: { retry: false },
      },
    });
  if (!queryClient.getQueryData(queryKeys.accounts))
    queryClient.setQueryData(queryKeys.accounts, testAccounts);
  queryClient.setQueryData(queryKeys.mining, { job: null });
  return render(
    <QueryClientProvider client={queryClient}>
      <ToastProvider>
        <MemoryRouter>
          <WalletActionsProvider>
            <main>
              <h1 tabIndex={-1}>Wallet</h1>
              <PaymentUpdates />
              {ui}
            </main>
          </WalletActionsProvider>
        </MemoryRouter>
      </ToastProvider>
    </QueryClientProvider>,
  );
}

export const testAccounts: Account[] = [1, 2, 3, 4, 5].map((id) => ({
  id,
  name: `Account ${id}`,
  unified_address: `uregtest1account${id}`,
  transparent_address: `tmAccount${id}`,
  transparent_zatoshi: 0n,
  ironwood_zatoshi: id === 1 ? 500_000_000n : 0n,
}));
