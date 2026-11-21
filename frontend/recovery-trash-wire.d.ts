import type { QueueSnapshotWire } from "./queue-snapshot-wire.js";

type QueueRecoveryReason = "outcome_unclear" | "settlement_pending" | "decision_required"
  | "interrupted" | "failed" | "retained" | "incomplete";

interface TrashJournalReviewWire {
  name: string;
  entry_count: number | null;
  effects_unclear: boolean;
  error: string | null;
}

export interface RecoveryOverviewWire {
  queue: Array<{
    reason: QueueRecoveryReason;
    entry: QueueSnapshotWire["entries"][number];
  }> | null;
  trash: { Ok: TrashJournalReviewWire[]; Err?: never }
    | { Err: { code: string; message: string }; Ok?: never };
}

export interface TrashStatsWire {
  trashed: number;
  failed: number;
  unclear: number;
  recovery_paths: string[];
  errors: string[];
}
