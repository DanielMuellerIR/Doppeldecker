import type { ArchiveExtractPreviewWire, ArchivePackPreviewWire } from "./archive-mutation-wire.js";

// Form der Anzeigeprojektion FrontendQueueSnapshot.
// Verschachtelte Operationen, Pläne und andere Metadatenberichte bleiben opak.
type QueueState = "Pending" | "Validating" | "Running" | "AwaitingConfirmation"
  | "Completed" | "Failed" | "Cancelled" | "Interrupted";

interface CreatedDirectoryEffectKeyWire {
  origin: "ExclusiveMinimalCreate" | "ExclusiveMinimalCreateBeforeChildren";
  directory: string;
  file_id: string;
  object_namespace: number[];
}

interface CreatedDirectoryDaclOutcomeWire extends CreatedDirectoryEffectKeyWire {
  before_digest: number[];
  after_digest: number[];
}

interface CreatedDirectoryDaclReportWire {
  created?: Array<{
    effect_key: CreatedDirectoryEffectKeyWire;
    ordinary_metadata_applied: boolean;
  }>;
  completed: CreatedDirectoryDaclOutcomeWire[];
  uncertain: CreatedDirectoryDaclOutcomeWire[];
}

interface QueueMetadataReportsWire {
  directory_stream_report?: unknown;
  directory_dacl_report?: unknown;
  file_dacl_report?: unknown;
  created_directory_dacl_report?: CreatedDirectoryDaclReportWire;
}

interface QueueProgressWire extends QueueMetadataReportsWire {
  files_total: number;
  files_done: number;
  bytes_total: number;
  bytes_done: number;
  current: string;
  phase: string;
}

interface GeneratedSourceWire {
  id: string;
  manifest_digest: number[];
  kind: "zip" | "zip_extraction";
  label: string;
  files: number;
  directories: number;
  bytes: number;
  issues: Array<{ path: string; reason: string; content_omitted: boolean }>;
}

declare const queueAttention: unique symbol;

// Die Queue sendet den vollständigen Backendbeleg unverändert zurück. Die
// aufgeführten Felder beschreiben nur die von der Oberfläche gelesene Anzeige.
export interface QueueAttentionWire {
  readonly [queueAttention]: never;
  readonly type: string;
  readonly reason: string;
  readonly confirmation: string;
  readonly current?: { readonly preview: ArchiveExtractPreviewWire | ArchivePackPreviewWire };
  readonly current_plan?: { readonly copy_count: number; readonly update_count: number; readonly delete_count: number };
  readonly current_conflicts?: readonly string[];
  readonly current_tree?: {
    readonly files: number;
    readonly directories: number;
    readonly bytes: number;
    readonly omitted: readonly { readonly path: string; readonly reason: string }[];
  } | null;
}

interface QueueEntryWire extends QueueMetadataReportsWire {
  operation_id: string;
  operation: { type: string } & Record<string, unknown>;
  profile_bindings: unknown[];
  created_unix_ms: number;
  state: QueueState;
  attempts: number;
  progress: QueueProgressWire;
  attention: QueueAttentionWire | null;
  error: string | null;
  error_code: string | null;
  temp_recovery_paths: string[];
  retained_backup_paths: string[];
  file_incomplete: boolean;
  recovery_available: boolean;
  bidirectional_completion_recovery: "complete" | "rolled_back" | null;
  archive_extract_report: unknown;
  directory_dacl_recovery?: true;
  file_dacl_recovery: boolean;
  created_directory_dacl_recovery?: true;
}

type ProjectedQueueEntryWire = QueueEntryWire & (
  | { generated_source: GeneratedSourceWire; generated_source_recovery: boolean }
  | { generated_source?: never; generated_source_recovery?: never }
);

export interface QueueSnapshotWire {
  version: number;
  revision: number;
  paused: boolean;
  stop_on_error: boolean;
  entries: ProjectedQueueEntryWire[];
}
