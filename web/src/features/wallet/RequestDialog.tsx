import { zodResolver } from '@hookform/resolvers/zod';
import { Copy } from 'lucide-react';
import { useState } from 'react';
import { useForm, useWatch } from 'react-hook-form';
import { QRCodeSVG } from 'qrcode.react';
import { Button } from '@/components/ui/Button';
import { CopyButton } from '@/components/ui/CopyButton';
import { Dialog } from '@/components/ui/Dialog';
import { Field } from '@/components/ui/Field';
import { controlStyles } from '@/components/ui/control-styles';
import { useToast } from '@/components/ui/toast-context';
import { usePaymentRequest } from '@/hooks/mutations';
import { errorMessage, type Account } from '@/lib/api';
import { SelectField } from './fields';
import { accountOptions } from './field-options';
import {
  MEMO_MAX_BYTES,
  memoByteLength,
  requestSchema,
  type RequestInput,
  type RequestValues,
} from './schemas';

const formKey = (accountId?: string, amount?: string, memo?: string) =>
  JSON.stringify([accountId, amount, memo]);

export function RequestDialog({
  open,
  onOpenChange,
  accounts,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  accounts: Account[];
}) {
  const toast = useToast();
  const request = usePaymentRequest();
  const [generated, setGenerated] = useState<{ form: string; uri: string }>();
  const form = useForm<RequestInput, unknown, RequestValues>({
    resolver: zodResolver(requestSchema),
    mode: 'onChange',
    defaultValues: { account_id: '1', amount: '', memo: '' },
  });
  const accountId = useWatch({ control: form.control, name: 'account_id' });
  const amount = useWatch({ control: form.control, name: 'amount' });
  const memo = useWatch({ control: form.control, name: 'memo' }) ?? '';
  const currentForm = formKey(accountId, amount, memo);
  const uri = generated?.form === currentForm ? generated.uri : undefined;

  const submit = form.handleSubmit((values) => {
    if (request.isPending) return;
    const submittedValues = form.getValues();
    const submitted = formKey(
      submittedValues.account_id,
      submittedValues.amount,
      submittedValues.memo,
    );
    setGenerated(undefined);
    request.mutate(
      {
        account_id: values.account_id,
        ...(values.amount === undefined ? {} : { amount_zatoshi: values.amount }),
        ...(values.memo === '' ? {} : { memo: values.memo }),
      },
      {
        onSuccess: (result) => setGenerated({ form: submitted, uri: result.uri }),
        onError: (error) => toast.error('Request generation failed', errorMessage(error)),
      },
    );
  });

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      eyebrow="PAYMENT REQUEST"
      title="Request ZEC"
      description="Create a ZIP-321 payment request for a development account."
    >
      <form onSubmit={(event) => void submit(event)} noValidate>
        <SelectField
          control={form.control}
          name="account_id"
          label="Receiving account"
          options={accountOptions(accounts)}
          error={form.formState.errors.account_id?.message}
        />
        <Field
          label="Amount (ZEC, optional)"
          hint="Leave blank to let the sender choose an amount."
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
        <Field
          label="Memo (optional)"
          hint={`${memoByteLength(memo)}/${MEMO_MAX_BYTES} bytes, encrypted to the recipient.`}
          error={form.formState.errors.memo?.message}
        >
          {(aria) => (
            <textarea
              {...aria}
              {...form.register('memo')}
              rows={2}
              autoComplete="off"
              className={controlStyles}
            />
          )}
        </Field>

        <Button
          type="submit"
          variant="primary"
          size="block"
          loading={request.isPending}
          disabled={!form.formState.isValid || request.isPending}
        >
          {request.isPending ? 'Generating…' : 'Generate request'}
        </Button>
      </form>

      {uri && (
        <section
          aria-label="Generated payment request"
          className="mt-5 grid justify-items-center gap-3"
        >
          <QRCodeSVG
            value={uri}
            title={uri}
            role="img"
            aria-label="Payment request QR code"
            size={192}
            className="bg-white p-2"
          />
          <div className="border-line bg-canvas flex w-full min-w-0 items-start gap-2 rounded-xs border p-3">
            <code className="min-w-0 flex-1 font-mono text-[11px] break-all">{uri}</code>
            <CopyButton
              value={uri}
              label="Copy payment request"
              icon={<Copy className="size-4" />}
            />
          </div>
        </section>
      )}
    </Dialog>
  );
}
