export type SyncModeWire = "AddOnly" | "Update" | "Mirror";
export type SyncCompareWire = "SizeAndTime" | "ContentHash";
type SyncMetadataProfileWire = "Portable" | "NativeStrict" | "NativeBestEffort";

export interface SyncOptionsWire {
  mode: SyncModeWire;
  compare: SyncCompareWire;
  metadata?: SyncMetadataProfileWire;
  mtime_tolerance: number;
  exclude?: string[];
}

declare const syncPlan: unique symbol;
declare const localSyncPreparationDigest: unique symbol;

export interface LocalSyncPreparationDigestWire extends ReadonlyArray<number> {
  readonly [localSyncPreparationDigest]: never;
  readonly length: 32;
}

// Außer den Anzeigefeldern bleibt der Plan für die Simulation ein gebundener Backendbeleg.
export interface SyncPlanWire {
  readonly [syncPlan]: never;
  readonly actions: readonly { readonly type: string; readonly path: string; readonly target_size?: number | null }[];
  readonly skip_count: number;
  readonly exclusions?: {
    readonly source_count: number;
    readonly target_count: number;
    readonly source_paths: readonly string[];
    readonly target_paths: readonly string[];
  };
  readonly metadata_report: { readonly issues: readonly unknown[] };
}

interface LocalSyncRootIdentityWire {
  provider: "Local";
  identity: { display_path: string; mount: { fs_type: string } };
}

export interface LocalSyncPreviewWire {
  source_identity: LocalSyncRootIdentityWire;
  target_identity: LocalSyncRootIdentityWire;
  source_listing: { entries_seen: number };
  target_listing: { entries_seen: number };
  plan: SyncPlanWire;
  options: SyncOptionsWire;
  impact: {
    new_files: number;
    new_bytes: number;
    replacements: number;
    metadata_changes: number;
    required_available_bytes: number;
    available_bytes: number;
  };
  scope_warnings: string[];
  restore_probe_ok: boolean;
  local_execution?: {
    new_file_commit: "assured" | "best_effort";
    file_flush: boolean;
    directory_flush: boolean;
    irreversible_replacements: number;
  };
  plan_digest: LocalSyncPreparationDigestWire;
}
