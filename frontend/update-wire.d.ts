export type UpdateCheckWire = { current_version: string } & (
  | { available: true; version: string; notes: string | null; date: string | null }
  | { available: false; version: null; notes: null; date: null }
);

export type PreparedUpdateStatusWire =
  | { available: true; version: string }
  | { available: false; version: null };

export interface UpdateProgressWire {
  downloaded: number;
  total: number | null;
}
