import type { MiningJob } from '@/lib/api';

export function MiningStatus({ job }: { job: MiningJob }) {
  const count = `${job.completed_blocks.toLocaleString()} of ${job.requested_blocks.toLocaleString()} blocks confirmed`;
  return (
    <div className="text-ink-muted space-y-2 text-[13px]">
      <p>
        {job.progress_uncertain
          ? `At least ${count}; the last mining result is uncertain.`
          : `${count}.`}
      </p>
      <progress
        aria-label="Mining progress"
        value={job.completed_blocks}
        max={job.requested_blocks}
        className="accent-accent-strong w-full"
      />
      {job.state === 'mining' && <p>Mining blocks…</p>}
      {job.state === 'syncing' && <p>Synchronizing wallet…</p>}
      {job.state === 'completed' && <p>Mining completed. Wallet synchronized.</p>}
      {job.state === 'failed' && (
        <p role="alert" className="text-negative">
          {job.error ?? 'Mining failed.'}
        </p>
      )}
      {job.progress_uncertain && (
        <p>The result is a lower bound; extra blocks may have committed.</p>
      )}
    </div>
  );
}
