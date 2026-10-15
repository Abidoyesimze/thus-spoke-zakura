import * as RadixDialog from '@radix-ui/react-dialog';
import { X } from 'lucide-react';
import { useRef, type ReactNode, type RefObject } from 'react';
import { cn } from '@/lib/cn';

/**
 * Uses Radix for modal semantics and focus trapping. Wallet actions open the
 * dialog without a Radix Trigger, so remember the focused opener ourselves.
 */
export function Dialog({
  open,
  onOpenChange,
  eyebrow,
  title,
  description,
  children,
  className,
  bodyClassName,
  fallbackFocus,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  eyebrow?: string;
  title: string;
  description?: string;
  children: ReactNode;
  className?: string;
  bodyClassName?: string;
  fallbackFocus?: RefObject<HTMLElement | null>;
}) {
  const opener = useRef<HTMLElement | null>(null);
  return (
    <RadixDialog.Root open={open} onOpenChange={onOpenChange}>
      <RadixDialog.Portal>
        <RadixDialog.Overlay className="bg-ink/35 animate-pop fixed inset-0 z-40" />
        <RadixDialog.Content
          onOpenAutoFocus={() => {
            const active = document.activeElement;
            if (active instanceof HTMLElement) {
              // A notification action disappears on use. Prefer a stable
              // opener, while preserving a form beneath a nested dialog.
              opener.current = active.closest('[role="dialog"]')
                ? active
                : (fallbackFocus?.current ?? active);
            }
          }}
          onCloseAutoFocus={(event) => {
            event.preventDefault();
            if (opener.current?.isConnected) opener.current.focus();
            else fallbackFocus?.current?.focus();
          }}
          className={cn(
            'fixed top-1/2 left-1/2 z-50 w-[min(430px,calc(100vw-36px))] -translate-x-1/2 -translate-y-1/2',
            'xp-window animate-pop rounded-xs shadow-(--shadow-modal)',
            'max-h-[calc(100vh-36px)] overflow-hidden overflow-y-auto',
            className,
          )}
        >
          <div className="xp-caption flex shrink-0 items-center justify-between gap-4 px-3 py-1.5">
            <RadixDialog.Title className="text-[13px] font-semibold tracking-wide">
              {title}
            </RadixDialog.Title>
            <RadixDialog.Close
              aria-label="Close dialog"
              className="xp-raised bg-raised text-ink xp-press grid size-5 cursor-pointer place-items-center rounded-xs"
            >
              <X className="size-3" />
            </RadixDialog.Close>
          </div>
          <div className={cn('p-5', bodyClassName)}>
            {eyebrow && (
              <span className="text-accent mb-1 block font-mono text-[11px] font-medium tracking-[0.13em]">
                {eyebrow}
              </span>
            )}
            {description ? (
              <RadixDialog.Description className="text-ink-muted mb-5 text-[12px]">
                {description}
              </RadixDialog.Description>
            ) : (
              <RadixDialog.Description className="sr-only">{title}</RadixDialog.Description>
            )}
            {children}
          </div>
        </RadixDialog.Content>
      </RadixDialog.Portal>
    </RadixDialog.Root>
  );
}
