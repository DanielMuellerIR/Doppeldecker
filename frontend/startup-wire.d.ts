export interface DefaultDirsWire {
  left: string;
  right: string;
}

export type SafetyInfoWire = {
  real_mode_available: boolean;
  real_mode_pending: boolean;
  max_delete_entries: number;
  max_delete_fraction: number;
  quarantine_retention_days: number;
} & ({ real_enabled: true; sandbox: string } | { real_enabled: false; sandbox: null });
