export interface QuarantineRunWire {
  id: string;
  created_unix_ms: number;
  expires_unix_ms: number;
  target_root: string;
  state: "active" | "complete";
  status: "live" | "crash_recovery" | "purge_recovery" | "complete" | "purged";
  items_total: number;
  items_available: number;
  items_restorable: number;
  items_manual_recovery: number;
  items_restored: number;
  items_recovery_pending: number;
  entries_available: number;
  bytes_available: number;
}

export interface QuarantineEntryWire {
  relative_path: string;
  is_dir: boolean;
  reason: "replaced" | "mirror_removal" | "user_delete";
  restored: boolean;
  available: boolean;
  automatic_restore_available: boolean;
  manual_recovery_required: boolean;
  recovery_pending: boolean;
  entries: number;
  bytes: number;
}

export interface QuarantineRestoreWire {
  path: string;
  outcome: "restored" | "reconciled";
}
