import { useRef, useState } from 'react';
import { useQueries } from '@tanstack/react-query';
import { AlertTriangle, Clock3, Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/Button';
import { Dialog } from '@/components/ui/Dialog';
import { api, errorMessage } from '@/lib/api';
import { cn } from '@/lib/cn';
import { queryKeys } from '@/hooks/queries';
import type { PaymentIdentity } from '@/hooks/payment-identity';
import { useWalletPayments } from './payment-context';
import { useWalletActions } from './wallet-actions-context';
import { PaymentStatus, PaymentWarning } from './PaymentStatus';

type PaymentOutcome = 'unresolved' | 'confirmed' | 'failed';

export function PaymentUpdates() {
  const {
    send,
    faucet,
    savedSend,
    savedFaucet,
    isRestored,
    viewedPayment,
    viewPayment,
    closePayments,
  } = useWalletPayments();
  const { openSend, openFaucet } = useWalletActions();
  const identities = [...savedSend.displayed, ...savedFaucet.displayed];
  const open = viewedPayment !== null;
  const [visibleKeys, setVisibleKeys] = useState<string[]>([]);
  const [container, setContainer] = useState<HTMLDivElement | null>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  // Observe the same Query snapshots for counts, without starting extra reads.
  const statuses = useQueries({
    queries: identities.map(({ key }) => ({
      queryKey: queryKeys.payment(key),
      queryFn: () => api.payment(key),
      enabled: false,
    })),
  });
  // Prioritize unresolved work when opening, then keep rows stable while the
  // panel is in use so a background confirmation cannot move focused controls.
  const ordered = identities
    .map((identity, index) => {
      const status = statuses[index]?.data?.status;
      const group: PaymentOutcome =
        status === 'confirmed' || status === 'failed' ? status : 'unresolved';
      return { identity, group };
    })
    .sort((a, b) => {
      const first = visibleKeys.indexOf(a.identity.key);
      const second = visibleKeys.indexOf(b.identity.key);
      return (first < 0 ? identities.length : first) - (second < 0 ? identities.length : second);
    });
  const counts = { all: identities.length, unresolved: 0, confirmed: 0, failed: 0 };
  for (const { group } of ordered) counts[group]++;
  const visible = ordered;
  const includedKeys = new Set(visible.map(({ identity }) => identity.key));
  const summary = Object.entries(counts)
    .filter(([state, count]) => state !== 'all' && count > 0)
    .map(([state, count]) => `${count} ${state}`)
    .join(' · ');
  const errors = [savedSend.error, savedFaucet.error].filter((error) => error !== null);
  const finished = visible.filter(
    ({ identity, group }) =>
      group !== 'unresolved' &&
      !(identity.kind === 'send' ? savedSend : savedFaucet).retained.some(
        ({ key }) => key === identity.key,
      ),
  );
  const attention = errors.length > 0 || counts.failed > 0;
  const active = send.isPending || faucet.isPending;
  const StatusIcon = attention ? AlertTriangle : active ? Loader2 : Clock3;
  const focusNext = (identity: PaymentIdentity) => {
    const headings = [
      ...(container?.querySelectorAll<HTMLHeadingElement>('article:not([hidden]) h3') ?? []),
    ];
    const index = ordered.findIndex((item) => item.identity.key === identity.key);
    const position = (heading: HTMLHeadingElement) =>
      ordered.findIndex(
        (item) => item.identity.key === heading.closest('article')?.dataset.paymentKey,
      );
    const next =
      headings.find((heading) => position(heading) > index) ??
      [...headings].reverse().find((heading) => position(heading) < index);
    (next ?? container)?.focus();
  };
  const dismiss = (identity: PaymentIdentity) => {
    (identity.kind === 'send' ? savedSend : savedFaucet).dismissKey(identity.key);
    requestAnimationFrame(() => focusNext(identity));
  };
  const startNew = (identity: PaymentIdentity) => {
    closePayments();
    // Use a connected opener so closing the next form returns focus to the
    // summary rather than the row removed when this panel closes.
    requestAnimationFrame(() => {
      trigger.current?.focus();
      if (identity.kind === 'send') openSend(identity.variables.from_account, identity);
      else openFaucet(identity.variables.account_id, identity);
    });
  };
  if (!identities.length && !errors.length && !open) return null;
  return (
    <>
      <Button
        ref={trigger}
        variant="ghost"
        aria-label={`View payments: ${summary || 'Storage unavailable'}`}
        aria-haspopup="dialog"
        onClick={() => {
          const priority = (status: string | undefined) =>
            status === 'confirmed' ? 2 : status === 'failed' ? 1 : 0;
          setVisibleKeys(
            identities
              .map((identity, index) => ({ identity, status: statuses[index]?.data?.status }))
              .sort((a, b) => priority(a.status) - priority(b.status))
              .map(({ identity }) => identity.key),
          );
          viewPayment();
        }}
      >
        <StatusIcon
          aria-hidden
          className={attention ? 'text-warning' : active ? 'motion-safe:animate-spin' : undefined}
        />
        Payments
        {identities.length > 0 && (
          <span className="bg-accent-soft text-accent-strong rounded-xs px-1.5 text-[11px] tabular-nums">
            {identities.length}
          </span>
        )}
      </Button>
      <Dialog
        open={open}
        onOpenChange={(next) => {
          if (!next) closePayments();
          if (!next && !identities.length && !errors.length) {
            requestAnimationFrame(() => {
              const heading = document.querySelector<HTMLElement>('main h1');
              if (heading) {
                heading.tabIndex = -1;
                heading.focus();
              }
            });
          }
        }}
        title="Payments"
        description={
          identities.length ? 'Status checks are automatic. Resume repeats the saved request.' : ''
        }
        className="flex max-h-[calc(100dvh-36px)] w-[min(540px,calc(100vw-36px))] flex-col overflow-y-hidden"
        bodyClassName="relative flex min-h-0 flex-col [&>p]:shrink-0"
        fallbackFocus={trigger}
      >
        <div
          ref={setContainer}
          role="region"
          aria-label="Payment updates"
          tabIndex={0}
          className={cn(
            'payment-scroll-area border-line relative max-h-[min(55dvh,440px)] min-h-0 overflow-y-auto overscroll-contain rounded-xs focus-visible:-outline-offset-2',
            (visible.length > 0 || errors.length > 0) && 'border',
          )}
        >
          {errors.length > 0 && (
            <PaymentWarning role="alert" className="m-3">
              <p>Couldn’t load saved payments: {errors.map(errorMessage).join('; ')}</p>
              <p>Allow site storage and reload.</p>
            </PaymentWarning>
          )}
          {!visible.length && (identities.length > 0 || errors.length === 0) && (
            <div className="text-ink-muted p-4 text-[12px]">
              <p>No payment updates</p>
            </div>
          )}
        </div>
        {finished.length > 0 && (
          <div className="mt-3 flex shrink-0 justify-end">
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                for (const { identity } of finished) {
                  (identity.kind === 'send' ? savedSend : savedFaucet).dismissKey(identity.key);
                }
                requestAnimationFrame(() => container?.focus());
              }}
            >
              Clear finished
            </Button>
          </div>
        )}
      </Dialog>
      {ordered.map(({ identity }) => {
        const operation = identity.kind === 'send' ? send : faucet;
        return (
          <PaymentStatus
            key={identity.key}
            container={container}
            hidden={!includedKeys.has(identity.key)}
            onHiddenFocus={() => focusNext(identity)}
            focusRequest={viewedPayment?.key === identity.key ? viewedPayment : null}
            onView={() => viewPayment(identity.key)}
            identity={identity}
            restored={isRestored(identity.key)}
            pendingKey={operation.pendingKey}
            busy={send.isPending || faucet.isPending}
            claimAnnouncement={operation.claimAnnouncement}
            onTerminal={operation.clear}
            onResume={(item) => operation.resume(item)}
            onDismiss={() => dismiss(identity)}
            onNew={() => startNew(identity)}
          />
        );
      })}
    </>
  );
}
