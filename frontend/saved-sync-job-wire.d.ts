import type { RemoteProvider } from "./provider-command-names.js";
import type { SyncOptionsWire } from "./local-sync-wire.js";

export type SyncJobEndpointWire =
  | { provider: "local"; path: string; profileId?: null }
  | { provider: RemoteProvider; path: string; profileId: string };

export interface SyncJobDraftWire {
  left: SyncJobEndpointWire;
  right: SyncJobEndpointWire;
  direction: "lr" | "rl";
  options: SyncOptionsWire;
}

export interface SaveSyncJobRequestWire extends SyncJobDraftWire {
  name: string;
  id?: string | null;
  revision?: number | null;
}

export interface SavedSyncJobWire extends SyncJobDraftWire {
  id: string;
  revision: number;
  name: string;
}
