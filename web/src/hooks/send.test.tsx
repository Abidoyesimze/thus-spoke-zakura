import { act, renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type Activity } from '@/lib/api';
import { useSend } from './mutations';

const request = {
  from_account: 1,
  to_account: 2,
  source_pool: 'ironwood' as const,
  destination_pool: 'ironwood' as const,
  amount_zatoshi: 100_000_000n,
};
const testLocks = Object.getOwnPropertyDescriptor(navigator, 'locks')!;

function confirmed(id: string): Activity {
  return {
    id,
    kind: 'send',
    from_account: 1,
    to_account: 2,
    to_address: null,
    source_pool: 'ironwood',
    destination_pool: 'ironwood',
    amount_zatoshi: 100_000_000n,
    txid: id,
    block_hash: 'b'.repeat(64),
    status: 'confirmed',
    last_error: null,
    created_at: '2026-10-06 00:00:00',
  };
}

function pending(id: string): Activity {
  return { ...confirmed(id), status: 'broadcast', block_hash: null };
}

function tab() {
  const client = new QueryClient({ defaultOptions: { mutations: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return renderHook(useSend, { wrapper });
}

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});
afterEach(() => {
  vi.restoreAllMocks();
  Object.defineProperty(navigator, 'locks', testLocks);
});

describe('send operation ownership', () => {
  it.each([false, true])(
    'replays the clicked identity if terminal cleanup wins the queued lock (resume: %s)',
    async (resume) => {
      const name = 'ths:send:[1,2,"ironwood","ironwood","100000000",null]';
      const originalKey = 'original-retained-key';
      localStorage.setItem(name, originalKey);
      const post = vi.spyOn(api, 'send').mockResolvedValue(confirmed('original-payment'));
      let acquire!: () => void;
      const gate = new Promise<void>((resolve) => {
        acquire = resolve;
      });
      Object.defineProperty(navigator, 'locks', {
        configurable: true,
        value: {
          request: async (name: string, callback: (lock: Lock) => unknown) => {
            await gate;
            return callback({ name, mode: 'exclusive' });
          },
        },
      });
      const hook = tab();
      let result: Promise<unknown> | undefined;
      act(() => {
        if (resume)
          hook.result.current.resume(
            { kind: 'send', name, key: originalKey, variables: request },
            { onError: vi.fn() },
          );
        else result = hook.result.current.mutateAsync(request);
      });
      // A terminal GET clears the stored key while this action is waiting.
      localStorage.removeItem(name);
      sessionStorage.removeItem(`${name}:tab`);
      await act(async () => {
        acquire();
        await result;
      });
      await waitFor(() => expect(post).toHaveBeenCalled());
      expect(post.mock.calls[0]?.[0].idempotency_key).toBe(originalKey);
      expect(post).toHaveBeenCalledTimes(1);
      hook.unmount();
    },
  );
  it('keeps route, exact amount, and absent/empty memo fingerprints distinct', async () => {
    const post = vi.spyOn(api, 'send').mockResolvedValue(pending('payment'));
    const hook = tab();
    const variants = [
      request,
      { ...request, memo: '' },
      { ...request, memo: 'rent' },
      { ...request, amount_zatoshi: 100_000_001n },
      { ...request, from_account: 3 },
      { ...request, to_account: 3 },
      { ...request, source_pool: 'transparent' as const },
      { ...request, destination_pool: 'transparent' as const },
    ];
    for (const variables of variants) await act(() => hook.result.current.mutateAsync(variables));
    const keys = post.mock.calls.map(([input]) => input.idempotency_key);
    expect(new Set(keys).size).toBe(variants.length);
    await act(() => hook.result.current.mutateAsync(request));
    expect(post.mock.calls.at(-1)?.[0].idempotency_key).toBe(keys[0]);
    hook.unmount();
  });
});
