import { createContext, use } from 'react';
import type { PaymentIdentity } from '@/hooks/payment-identity';

export interface WalletActionsApi {
  openSend: (accountId?: number, draft?: PaymentIdentity) => void;
  openFaucet: (accountId?: number, draft?: PaymentIdentity) => void;
  openRequest: () => void;
  openMine: () => void;
}

export const WalletActionsContext = createContext<WalletActionsApi | null>(null);

export function useWalletActions(): WalletActionsApi {
  const api = use(WalletActionsContext);
  if (!api) throw new Error('useWalletActions must be used inside <WalletActionsProvider>');
  return api;
}
