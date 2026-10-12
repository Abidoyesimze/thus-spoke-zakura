import { useMemo, useState, type ReactNode } from 'react';
import { WalletActionsContext, type WalletActionsApi } from './wallet-actions-context';
import { useAccounts, useMiningJob } from '@/hooks/queries';
import { SendDialog } from './SendDialog';
import { FaucetDialog } from './FaucetDialog';
import { MineDialog } from './MineDialog';
import { MiningStatus } from './MiningStatus';
import { Button } from '@/components/ui/Button';

type OpenDialog = 'send' | 'faucet' | 'mine' | null;

/**
 * Owns the transaction dialogs so the header and the account cards can trigger
 * them without threading state through the tree.
 */
export function WalletActionsProvider({ children }: { children: ReactNode }) {
  const [dialog, setDialog] = useState<OpenDialog>(null);
  const [faucetAccount, setFaucetAccount] = useState<number | undefined>(undefined);
  const [sendAccount, setSendAccount] = useState<number | undefined>(undefined);
  const { data: accounts = [] } = useAccounts();
  const { data: mining } = useMiningJob();
  const job = mining?.job;

  const api = useMemo<WalletActionsApi>(
    () => ({
      openSend: (accountId) => {
        setSendAccount(accountId);
        setDialog('send');
      },
      openFaucet: (accountId) => {
        setFaucetAccount(accountId);
        setDialog('faucet');
      },
      openMine: () => setDialog('mine'),
    }),
    [],
  );

  const close = (open: boolean) => {
    if (!open) setDialog(null);
  };

  return (
    <WalletActionsContext value={api}>
      {children}
      {job && job.state !== 'completed' && (
        <section
          aria-label="Mining job"
          className="xp-raised bg-panel fixed right-4 bottom-20 z-20 w-[min(380px,calc(100vw-32px))] space-y-3 rounded-xs p-4 shadow-(--shadow-float) md:right-6 md:bottom-6"
        >
          <MiningStatus job={job} />
          <Button size="sm" onClick={() => setDialog('mine')}>
            View mining
          </Button>
        </section>
      )}
      {/* Keyed so each dialog remounts with fresh defaults per open. */}
      {dialog === 'send' && (
        <SendDialog
          key={`send-${sendAccount ?? 'any'}`}
          open
          onOpenChange={close}
          accounts={accounts}
          {...(sendAccount === undefined ? {} : { defaultAccountId: sendAccount })}
        />
      )}
      {dialog === 'faucet' && (
        <FaucetDialog
          key={`faucet-${faucetAccount ?? 'any'}`}
          open
          onOpenChange={close}
          accounts={accounts}
          {...(faucetAccount === undefined ? {} : { defaultAccountId: faucetAccount })}
        />
      )}
      {dialog === 'mine' && <MineDialog key="mine" open onOpenChange={close} />}
    </WalletActionsContext>
  );
}
