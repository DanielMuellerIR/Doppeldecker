import type { BidirectionalEndpointRequest } from "./pane-endpoint.js";
import type { RemoteProvider } from "./provider-command-names.js";

export type RenameExecutionClass = "assured" | "best_effort";
export type DirectoryCreateExecutionClass = "assured" | "best_effort";

type RemoteDeleteMode = "quarantine" | "permanent";
declare const remoteDeletePlan: unique symbol;

// Nur der vom Backend erzeugte Plan geht unverändert zurück. Die Anzeige braucht keine Belegfelder.
interface RemoteDeletePlanWire {
  readonly [remoteDeletePlan]: never;
  readonly items: readonly unknown[];
  readonly entries: number;
  readonly bytes: number;
}

export interface ProviderDeleteRequestWire {
  endpoint: BidirectionalEndpointRequest;
  items: string[];
}

export interface EnqueueProviderDeleteRequestWire extends ProviderDeleteRequestWire {
  mode: RemoteDeleteMode;
  expectedPlan: RemoteDeletePlanWire;
  confirmation?: string | null;
  effectsConfirmed?: boolean;
}

export interface ProviderDeletePreviewWire {
  capabilities: {
    provider: Exclude<RemoteProvider, "webdav"> | "web_dav" | "local";
    quarantine: boolean;
    permanent: boolean;
    limitations: string[];
    blockers: string[];
  };
  mode: RemoteDeleteMode;
  plan: RemoteDeletePlanWire;
  confirmation: string | null;
}

export type RenameInteractionContractWire =
  | { kind: "existing" }
  | { kind: "confirmed"; execution_class: RenameExecutionClass; limitations: string[] }
  | { kind: "blocked"; blockers: string[] };

export interface RenameProviderPathRequestWire {
  endpoint: BidirectionalEndpointRequest;
  path: string;
  newName: string;
  expectedExecutionClass?: RenameExecutionClass | null;
}

export type RenameProviderPathResponseWire = {
  class: RenameExecutionClass;
  limitations: string[];
  verifiedAfterReconnect: boolean;
} & (
  | { outcome: "renamed"; newPath: string; existingIsDir: null }
  | { outcome: "collision"; newPath: null; existingIsDir: boolean | null }
);

export interface DirectoryCreateContractWire {
  execution_class: DirectoryCreateExecutionClass | null;
  limitations: string[];
  blockers: string[];
}

export interface DirectoryCreateReportWire {
  path: string;
  execution_class: DirectoryCreateExecutionClass;
  limitations: string[];
}
