import type { Transaction } from '@/lib/api';

export interface ShieldingSummary {
  ironwoodActions: number;
  orchardActions: number;
  /** Ironwood and Orchard actions together. */
  shieldedActions: number;
  transparentInputs: number;
  transparentOutputs: number;
  saplingSpends: number;
  saplingOutputs: number;
  /**
   * Net value entering or leaving the shielded pools. Public, and non-zero on
   * a shielded transfer that pays a fee.
   */
  valueBalanceZat: number;
  /** True when the transfer touches no transparent input or output. */
  shieldedOnly: boolean;
  /** True when nothing about the transfer is visible on-chain. */
  fullyShielded: boolean;
  /** True when value crosses the shielded/transparent boundary. */
  mixed: boolean;
}

/**
 * Summarises what a transaction actually reveals publicly. An Ironwood or
 * Orchard transfer hides sender, recipient and amount inside its actions — but the
 * pool value balance stays public, so a transfer that pays a fee still
 * discloses that fee. Only a zero balance reveals nothing at all.
 */
export function summariseShielding(tx: Transaction): ShieldingSummary {
  const ironwoodActions = tx.ironwood?.actions.length ?? 0;
  const orchardActions = tx.orchard?.actions.length ?? 0;
  const shieldedActions = ironwoodActions + orchardActions;
  const transparentInputs = tx.vin.length;
  const transparentOutputs = tx.vout.length;
  const saplingSpends = tx.vShieldedSpend.length;
  const saplingOutputs = tx.vShieldedOutput.length;

  const shieldedPresent = shieldedActions > 0 || saplingSpends > 0 || saplingOutputs > 0;
  const transparentPresent = transparentInputs > 0 || transparentOutputs > 0;
  const valueBalanceZat =
    (tx.ironwood?.valueBalanceZat ?? 0) +
    (tx.orchard?.valueBalanceZat ?? 0) +
    (tx.valueBalanceZat ?? 0);

  return {
    ironwoodActions,
    orchardActions,
    shieldedActions,
    transparentInputs,
    transparentOutputs,
    saplingSpends,
    saplingOutputs,
    valueBalanceZat,
    shieldedOnly: shieldedPresent && !transparentPresent,
    fullyShielded: shieldedPresent && !transparentPresent && valueBalanceZat === 0,
    mixed: shieldedPresent && transparentPresent,
  };
}

function count(n: number, what: string): string {
  return `${n} ${what}${n === 1 ? '' : 's'}`;
}

/** Names the shielded parts by pool, e.g. "2 Ironwood actions" or "1 Sapling spend". */
export function actionSummary(summary: ShieldingSummary): string {
  const parts = [];
  if (summary.ironwoodActions > 0) parts.push(count(summary.ironwoodActions, 'Ironwood action'));
  if (summary.orchardActions > 0) parts.push(count(summary.orchardActions, 'Orchard action'));
  if (summary.saplingSpends > 0) parts.push(count(summary.saplingSpends, 'Sapling spend'));
  if (summary.saplingOutputs > 0) parts.push(count(summary.saplingOutputs, 'Sapling output'));
  const last = parts.pop();
  if (last === undefined) return count(0, 'shielded action');
  return parts.length === 0 ? last : `${parts.join(', ')} and ${last}`;
}
