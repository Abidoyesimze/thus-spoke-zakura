import { createContext, use } from 'react';
import type { useSend, useFaucet } from '@/hooks/mutations';
import type { useRetainedPayments } from '@/hooks/payment-identity';

export interface WalletPaymentsApi {
  send: ReturnType<typeof useSend>;
  faucet: ReturnType<typeof useFaucet>;
  savedSend: ReturnType<typeof useRetainedPayments>;
  savedFaucet: ReturnType<typeof useRetainedPayments>;
  isRestored: (key: string) => boolean;
  viewedPayment: { key: string | null } | null;
  viewPayment: (key?: string) => void;
  closePayments: () => void;
}

export const WalletPaymentsContext = createContext<WalletPaymentsApi | null>(null);

export function useWalletPayments() {
  const payments = use(WalletPaymentsContext);
  if (!payments) throw new Error('useWalletPayments must be used inside <WalletActionsProvider>');
  return payments;
}
