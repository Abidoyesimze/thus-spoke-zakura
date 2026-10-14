import { useEffect, useRef, useState } from 'react';
import { zodResolver } from '@hookform/resolvers/zod';
import { useForm, useWatch } from 'react-hook-form';
import { Dialog } from '@/components/ui/Dialog';
import { Button } from '@/components/ui/Button';
import { Field } from '@/components/ui/Field';
import { useToast } from '@/components/ui/toast-context';
import { errorMessage, type Account } from '@/lib/api';
import { formatZec, formatZecAmount } from '@/lib/money';
import { type FaucetVariables, type PaymentResult } from '@/hooks/mutations';
import { paymentName, type PaymentIdentity } from '@/hooks/payment-identity';
import { isTerminalPayment, usePaymentStatus } from '@/hooks/queries';
import { useWalletPayments } from './payment-context';
import { faucetSchema, type FaucetInput, type FaucetValues } from './schemas';
import { controlStyles } from '@/components/ui/control-styles';
import { SelectField } from './fields';
import { POOL_OPTIONS, accountOptions } from './field-options';
import { PaymentWarning } from './PaymentStatus';

export function FaucetDialog({
  open,
  onOpenChange,
  accounts,
  defaultAccountId,
  draft,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  accounts: Account[];
  defaultAccountId?: number;
  draft?: PaymentIdentity | undefined;
}) {
  const toast = useToast();
  const {
    faucet,
    savedFaucet: payments,
    send: sendOperation,
    faucet: faucetOperation,
    viewPayment,
  } = useWalletPayments();
  const busy = sendOperation.isPending || faucetOperation.isPending;
  const mounted = useRef(true);
  const [submittedHere, setSubmittedHere] = useState(false);
  const [receipt, setReceipt] = useState<{
    text: string;
    kind: 'accepted' | 'error';
    key?: string | undefined;
  } | null>(null);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const previous = draft?.kind === 'faucet' ? draft.variables : undefined;
  const form = useForm<FaucetInput, unknown, FaucetValues>({
    resolver: zodResolver(faucetSchema),
    mode: 'onChange',
    // Defaults to 1 ZEC rather than the 5 ZEC ceiling: the treasury is funded
    // from block rewards, and requests at the maximum are the first to fail
    // once the subsidy has halved.
    defaultValues: {
      account_id: String(previous?.account_id ?? defaultAccountId ?? 1),
      pool: previous?.pool ?? 'ironwood',
      amount: previous ? formatZec(previous.amount_zatoshi, { grouping: false }) : '1',
    },
  });
  const variablesFor = (values: FaucetValues) => ({
    account_id: values.account_id,
    pool: values.pool,
    amount_zatoshi: values.amount,
  });
  const current = faucetSchema.safeParse(useWatch({ control: form.control }));
  const match = current.success
    ? payments.retained.find(
        (item) => item.name === paymentName('faucet', variablesFor(current.data)),
      )
    : undefined;
  const matchStatus = usePaymentStatus(match?.key ?? '', Boolean(match));
  const matchTerminal = isTerminalPayment(matchStatus.data);
  const retryLabel = match && !matchTerminal ? `Resume payment` : null;
  const announce = ({ activity, key }: PaymentResult) => {
    if (!faucet.claimAnnouncement(key)) return;
    toast.success(
      `Funded Account ${activity.to_account}`,
      `${formatZecAmount(activity.amount_zatoshi)} confirmed.`,
    );
  };
  const failed = (error: Error, _variables: FaucetVariables, key: string | undefined) => {
    if (!mounted.current) return;
    setReceipt({ text: errorMessage(error), kind: 'error', key });
  };

  const submit = (values: FaucetValues) => {
    if (busy || matchTerminal) return;
    setSubmittedHere(true);
    setReceipt(null);
    const variables = variablesFor(values);
    const retained = payments.retained.find(
      (item) => item.name === paymentName('faucet', variables),
    );
    const submitted = {
      onError: failed,
      // Finish only the submitted draft; inputs can change during the POST.
      onSuccess: (result: PaymentResult, variables: FaucetVariables) => {
        if (!mounted.current) return;
        if (result.activity.status === 'confirmed') announce(result);
        if (result.activity.status === 'failed') {
          setReceipt({
            text: `Payment failed: ${result.activity.last_error ?? 'The server rejected this payment.'}`,
            kind: 'error',
            key: result.key,
          });
          return;
        }
        if (result.cleanupError) return;
        const draft = faucetSchema.safeParse(form.getValues());
        if (
          !draft.success ||
          paymentName('faucet', variablesFor(draft.data)) !== paymentName('faucet', variables)
        ) {
          setReceipt({
            text: `Payment ${result.activity.status === 'confirmed' ? 'confirmed' : 'accepted'}. Your draft is unchanged. View status in Payments.`,
            kind: 'accepted',
          });
          return;
        }
        onOpenChange(false);
        form.reset();
      },
    };
    if (retained) faucet.resume(retained, submitted);
    else faucet.mutate(variables, submitted);
  };

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      eyebrow="DEV TOOLS"
      title="Fund an account"
      description="Requests test funds from the mining treasury."
    >
      {payments.error && (
        <PaymentWarning role="alert" className="mb-3">
          <p>Could not load saved payments: {errorMessage(payments.error)}</p>
          <p>Allow site storage and reload before submitting.</p>
        </PaymentWarning>
      )}
      {receipt &&
        (receipt.kind === 'accepted' ? (
          <p role="status" className="text-ink-muted mb-3 text-[12px]">
            {receipt.text}
          </p>
        ) : (
          <PaymentWarning role="alert" className="mb-3">
            <p>{receipt.text}</p>
            {receipt.key && (
              <Button size="sm" className="mt-2" onClick={() => viewPayment(receipt.key)}>
                View payment
              </Button>
            )}
          </PaymentWarning>
        ))}
      {match && (
        <p className="text-ink-muted mb-3 text-[12px]" role="status">
          {matchTerminal
            ? 'Previous payment finished. Clear its saved request in Payments before sending these details again.'
            : `A saved payment has these details (${matchStatus.data?.status ?? 'status unknown'}). Resume it, or change the details for a new payment.`}
        </p>
      )}
      <form onSubmit={(event) => void form.handleSubmit(submit)(event)} noValidate>
        <SelectField
          control={form.control}
          name="account_id"
          label="Destination account"
          options={accountOptions(accounts)}
          error={form.formState.errors.account_id?.message}
        />
        <SelectField
          control={form.control}
          name="pool"
          label="Destination pool"
          options={POOL_OPTIONS}
          error={form.formState.errors.pool?.message}
        />
        <Field
          label="Amount (ZEC)"
          hint="Maximum 5 ZEC per request."
          error={form.formState.errors.amount?.message}
        >
          {(aria) => (
            <input
              {...aria}
              {...form.register('amount')}
              inputMode="decimal"
              autoComplete="off"
              className={controlStyles}
            />
          )}
        </Field>

        <Button
          type="submit"
          variant="primary"
          size="block"
          loading={busy}
          disabled={!form.formState.isValid || busy || matchTerminal || Boolean(payments.error)}
        >
          {busy
            ? faucet.isPending && submittedHere
              ? 'Submitting payment…'
              : 'Another payment is submitting…'
            : (retryLabel ?? 'Add funds')}
        </Button>
      </form>
    </Dialog>
  );
}
