import { act, renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type Activity } from '@/lib/api';
import { useFaucet, useSend } from './mutations';
import { usePaymentStatus } from './queries';
import { useRetainedPayments } from './payment-identity';

const request = { account_id: 1, pool: 'ironwood' as const, amount_zatoshi: 100_000_000n };
const testLocks = Object.getOwnPropertyDescriptor(navigator, 'locks')!;

function confirmed(id: string): Activity {
  return {
    id,
    kind: 'faucet',
    from_account: null,
    to_account: 1,
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

function paymentTab(kind: 'send' | 'faucet') {
  const client = new QueryClient({ defaultOptions: { mutations: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return renderHook(
    () => {
      const faucet = useFaucet();
      const send = useSend();
      return {
        submit: () =>
          kind === 'faucet'
            ? faucet.mutateAsync(request)
            : send.mutateAsync({
                from_account: 1,
                to_account: 2,
                source_pool: 'ironwood',
                destination_pool: 'ironwood',
                amount_zatoshi: request.amount_zatoshi,
              }),
      };
    },
    { wrapper },
  );
}

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});
afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
  Object.defineProperty(navigator, 'locks', testLocks);
});

describe.each(['faucet', 'send'] as const)('%s operation ownership', (kind) => {
  const storageKey =
    kind === 'faucet'
      ? 'ths:faucet:1:ironwood:100000000'
      : 'ths:send:[1,2,"ironwood","ironwood","100000000",null]';
  const tabStorageKey = `${storageKey}:tab`;
  const tab = () => paymentTab(kind);
  it('a delayed confirmation cannot erase a newer payment in another tab', async () => {
    const replies: Array<(activity: Activity) => void> = [];
    const post = vi
      .spyOn(api, kind)
      .mockImplementation(() => new Promise<Activity>((resolve) => replies.push(resolve)));
    const first = tab();
    const second = tab();

    let firstResult!: Promise<unknown>;
    let secondResult!: Promise<unknown>;
    act(() => {
      firstResult = first.result.current.submit();
      secondResult = second.result.current.submit();
    });
    await waitFor(() => expect(post).toHaveBeenCalledTimes(2));
    const originalKey = post.mock.calls[0]?.[0].idempotency_key;
    expect(post.mock.calls[1]?.[0].idempotency_key).toBe(originalKey);

    await act(async () => {
      replies[1]?.(confirmed('first-payment'));
      await secondResult;
    });
    expect(localStorage.getItem(storageKey)).toBeNull();

    let nextResult!: Promise<unknown>;
    act(() => {
      nextResult = second.result.current.submit();
    });
    await waitFor(() => expect(post).toHaveBeenCalledTimes(3));
    const newKey = post.mock.calls[2]?.[0].idempotency_key;
    expect(newKey).not.toBe(originalKey);
    expect(localStorage.getItem(storageKey)).toBe(newKey);

    await act(async () => {
      replies[0]?.(confirmed('first-payment'));
      await firstResult;
    });
    expect(localStorage.getItem(storageKey)).toBe(newKey);

    await act(async () => {
      replies[2]?.(confirmed('second-payment'));
      await nextResult;
    });
    expect(localStorage.getItem(storageKey)).toBeNull();
  });

  it('a pending tab retries its own key after another tab starts a new payment', async () => {
    const replies: Array<(activity: Activity) => void> = [];
    const post = vi
      .spyOn(api, kind)
      .mockImplementation(() => new Promise<Activity>((resolve) => replies.push(resolve)));
    const first = tab();
    const second = tab();
    let firstResult!: Promise<unknown>;
    let secondResult!: Promise<unknown>;
    act(() => {
      firstResult = first.result.current.submit();
      secondResult = second.result.current.submit();
    });
    await waitFor(() => expect(post).toHaveBeenCalledTimes(2));
    const originalKey = post.mock.calls[0]?.[0].idempotency_key;
    expect(post.mock.calls[1]?.[0].idempotency_key).toBe(originalKey);

    await act(async () => {
      replies[0]?.(pending('first-payment'));
      await firstResult;
      replies[1]?.(confirmed('first-payment'));
      await secondResult;
    });
    expect(localStorage.getItem(storageKey)).toBeNull();

    let nextResult!: Promise<unknown>;
    act(() => {
      nextResult = second.result.current.submit();
    });
    await waitFor(() => expect(post).toHaveBeenCalledTimes(3));
    const newKey = post.mock.calls[2]?.[0].idempotency_key;
    expect(newKey).not.toBe(originalKey);

    let retryResult!: Promise<unknown>;
    act(() => {
      retryResult = first.result.current.submit();
    });
    await waitFor(() => expect(post).toHaveBeenCalledTimes(4));
    expect(post.mock.calls[3]?.[0].idempotency_key).toBe(originalKey);

    await act(async () => {
      replies[3]?.(confirmed('first-payment'));
      await retryResult;
      replies[2]?.(pending('second-payment'));
      await nextResult;
    });
    expect(localStorage.getItem(storageKey)).toBe(newKey);
  });

  it('a reloaded pending tab keeps its key when another tab owns the shared key', async () => {
    const pendingKey = crypto.randomUUID();
    const newKey = crypto.randomUUID();
    sessionStorage.setItem(tabStorageKey, pendingKey);
    localStorage.setItem(storageKey, newKey);
    const post = vi.spyOn(api, kind).mockResolvedValue(pending('first-payment'));
    const hook = tab();

    await act(async () => {
      await hook.result.current.submit();
    });
    expect(post.mock.calls[0]?.[0].idempotency_key).toBe(pendingKey);
    expect(localStorage.getItem(storageKey)).toBe(newKey);
  });

  it('retries a migrated legacy key until this tab sees confirmation', async () => {
    const legacyKey = crypto.randomUUID();
    sessionStorage.setItem(storageKey, legacyKey);
    const post = vi
      .spyOn(api, kind)
      .mockRejectedValueOnce(new TypeError('response lost'))
      .mockResolvedValueOnce(confirmed('legacy-payment'))
      .mockResolvedValueOnce(pending('new-payment'));
    const first = tab();

    await act(async () => {
      await expect(first.result.current.submit()).rejects.toThrow('response lost');
    });
    expect(post.mock.calls[0]?.[0].idempotency_key).toBe(legacyKey);
    expect(localStorage.getItem(storageKey)).toBe(legacyKey);
    expect(sessionStorage.getItem(storageKey)).toBeNull();
    expect(sessionStorage.getItem(tabStorageKey)).toBe(legacyKey);

    // Another tab clears the shared key after confirmation, leaving this tab's session intact.
    first.unmount();
    const reloaded = tab();
    localStorage.removeItem(storageKey);

    await act(async () => {
      await reloaded.result.current.submit();
    });
    expect(post.mock.calls[1]?.[0].idempotency_key).toBe(legacyKey);
    expect(sessionStorage.getItem(tabStorageKey)).toBeNull();

    await act(async () => {
      await reloaded.result.current.submit();
    });
    expect(post.mock.calls[2]?.[0].idempotency_key).not.toBe(legacyKey);
  });

  it('keeps a migrated legacy payment pending when another tab owns a newer shared key', async () => {
    const legacyKey = crypto.randomUUID();
    const sharedKey = crypto.randomUUID();
    sessionStorage.setItem(storageKey, legacyKey);
    localStorage.setItem(storageKey, sharedKey);
    const post = vi
      .spyOn(api, kind)
      .mockResolvedValueOnce(pending('legacy-payment'))
      .mockResolvedValueOnce(confirmed('legacy-payment'))
      .mockResolvedValueOnce(pending('shared-payment'));
    const hook = tab();

    await act(async () => {
      await hook.result.current.submit();
    });
    expect(post.mock.calls[0]?.[0].idempotency_key).toBe(legacyKey);
    expect(sessionStorage.getItem(storageKey)).toBeNull();
    expect(sessionStorage.getItem(tabStorageKey)).toBe(legacyKey);
    expect(localStorage.getItem(storageKey)).toBe(sharedKey);

    await act(async () => {
      await hook.result.current.submit();
    });
    expect(post.mock.calls[1]?.[0].idempotency_key).toBe(legacyKey);
    expect(sessionStorage.getItem(tabStorageKey)).toBeNull();
    expect(localStorage.getItem(storageKey)).toBe(sharedKey);

    await act(async () => {
      await hook.result.current.submit();
    });
    expect(post.mock.calls[2]?.[0].idempotency_key).toBe(sharedKey);
  });

  it('two tabs starting together share one key before either POST', async () => {
    let releaseFirst!: () => void;
    const gate = new Promise<void>((resolve) => {
      releaseFirst = resolve;
    });
    let tail = Promise.resolve();
    let acquisitions = 0;
    const lock = vi.fn((name: string, callback: (lock: Lock) => unknown) => {
      const previous = tail;
      let release!: () => void;
      tail = new Promise<void>((resolve) => {
        release = resolve;
      });
      return previous.then(async () => {
        try {
          if (++acquisitions === 1) await gate;
          return await callback({ name, mode: 'exclusive' });
        } finally {
          release();
        }
      });
    });
    Object.defineProperty(navigator, 'locks', { configurable: true, value: { request: lock } });
    const post = vi.spyOn(api, kind).mockResolvedValue(pending('pending-payment'));
    const first = tab();
    const second = tab();

    let firstResult!: Promise<unknown>;
    let secondResult!: Promise<unknown>;
    act(() => {
      firstResult = first.result.current.submit();
      secondResult = second.result.current.submit();
    });
    await waitFor(() => expect(lock).toHaveBeenCalledTimes(2));
    expect(lock.mock.calls[0]?.[0]).toBe(storageKey);
    expect(lock.mock.calls[1]?.[0]).toBe(storageKey);
    expect(post).not.toHaveBeenCalled();

    await act(async () => {
      releaseFirst();
      await Promise.all([firstResult, secondResult]);
    });
    expect(post).toHaveBeenCalledTimes(2);
    expect(post.mock.calls[0]?.[0].idempotency_key).toBe(post.mock.calls[1]?.[0].idempotency_key);
    expect(localStorage.getItem(storageKey)).toBe(post.mock.calls[0]?.[0].idempotency_key);
  });

  it.each(['locks', 'storage'])('does not POST when %s is unavailable', async (unavailable) => {
    if (unavailable === 'locks')
      Object.defineProperty(navigator, 'locks', { configurable: true, value: undefined });
    else
      vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
        throw new DOMException('Storage full', 'QuotaExceededError');
      });
    const post = vi.spyOn(api, kind);
    const hook = tab();

    await act(async () => {
      await expect(hook.result.current.submit()).rejects.toThrow(
        unavailable === 'locks' ? 'cannot coordinate payments' : 'Storage full',
      );
    });
    expect(post).not.toHaveBeenCalled();
    expect(localStorage.getItem(storageKey)).toBeNull();
    hook.unmount();

    if (unavailable === 'storage') {
      vi.restoreAllMocks();
      const setLocalItem = localStorage.setItem.bind(localStorage);
      for (const existingKey of [null, 'existing-payment']) {
        localStorage.clear();
        sessionStorage.clear();
        if (existingKey) {
          localStorage.setItem(storageKey, existingKey);
          localStorage.setItem(`ths:${kind}:retained:${existingKey}`, storageKey);
        }
        const before = { ...localStorage };
        const storage = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(function (
          this: Storage,
          item,
          value,
        ) {
          if (this === sessionStorage)
            throw new DOMException('Tab storage blocked', 'SecurityError');
          setLocalItem(item, value);
        });
        const submit = vi.spyOn(api, kind);
        const failed = tab();
        await act(async () => {
          await expect(failed.result.current.submit()).rejects.toThrow('Tab storage blocked');
        });
        expect(submit).not.toHaveBeenCalled();
        expect({ ...localStorage }).toEqual(before);
        expect(sessionStorage.length).toBe(0);
        failed.unmount();
        storage.mockRestore();
        submit.mockRestore();

        const restored = renderHook(() => useRetainedPayments(kind));
        await waitFor(() =>
          expect(restored.result.current.retained.map((identity) => identity.key)).toEqual(
            existingKey ? [existingKey] : [],
          ),
        );
        restored.unmount();
      }
    }
  });
});

describe('payment status polling', () => {
  it.each(['confirmed', 'failed'])(
    'follows replacement txids by the retained key and stops at %s',
    async (status) => {
      vi.useFakeTimers();
      const get = vi
        .spyOn(api, 'payment')
        .mockResolvedValueOnce({ ...pending('payment'), txid: 'old-tx' })
        .mockResolvedValueOnce({ ...pending('payment'), txid: 'replacement-tx' })
        .mockResolvedValue({ ...confirmed('payment'), status, txid: 'replacement-tx' });
      const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={client}>{children}</QueryClientProvider>
      );
      const hook = renderHook(() => usePaymentStatus('original-key', true), { wrapper });
      await act(() => vi.advanceTimersByTimeAsync(1));
      expect(hook.result.current.data?.txid).toBe('old-tx');
      await act(() => vi.advanceTimersByTimeAsync(2_000));
      expect(hook.result.current.data?.txid).toBe('replacement-tx');
      await act(() => vi.advanceTimersByTimeAsync(2_000));
      expect(hook.result.current.data?.status).toBe(status);
      await act(() => vi.advanceTimersByTimeAsync(8_000));
      expect(get.mock.calls.map(([key]) => key)).toEqual([
        'original-key',
        'original-key',
        'original-key',
      ]);
      hook.unmount();
      client.clear();
    },
  );
});
