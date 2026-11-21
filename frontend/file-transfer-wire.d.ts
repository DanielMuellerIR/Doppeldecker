import type { BidirectionalEndpointRequest } from "./pane-endpoint.js";

export type FileTransferKind = "copy" | "move";
export type FileTransferChoice = "skip" | "overwrite" | "both";
export type ProviderFilePolicy = FileTransferChoice | "inspect";

export interface FileIntent {
  kind: FileTransferKind;
  source: BidirectionalEndpointRequest;
  target: BidirectionalEndpointRequest;
  items: string[];
  destination: string;
}

export interface ProviderFileRequest extends FileIntent {
  generatedOperationId?: string | null;
}

export interface ProviderFileChoiceRequest {
  id: string;
  policy: ProviderFilePolicy;
}

export interface EnqueueProviderFileRequest extends ProviderFileRequest {
  overwrite: boolean;
  expectedConflicts: string[];
  keepBothPreparationId?: string | null;
  conflictPreparationId?: string | null;
  copyNewBestEffortExpected?: boolean | null;
  replaceBestEffortExpected?: boolean | null;
  removeSourceBestEffortExpected?: boolean | null;
}

export interface TransferSourceTreeWire {
  digest: string;
  entries: number;
  files: number;
  directories: number;
  bytes: number;
  omitted: { path: string; reason: string }[];
}

export interface ConflictEntryWire {
  endpoint: string;
  exists: boolean;
  kind: string | null;
  size: number | null;
  mtime: number | null;
}

interface FileConflictWire {
  relative_name: string;
  source: ConflictEntryWire;
  target: ConflictEntryWire;
}

interface ProviderFileCapabilitiesWire {
  provider: "local" | "sftp" | "ftps" | "web_dav" | "smb";
  copyNew: boolean;
  copyNewBestEffort: boolean;
  replace: boolean;
  replaceBestEffort: boolean;
  removeSource: boolean;
  removeSourceBestEffort: boolean;
  renameNoreplace: boolean;
  limitations: string[];
  blockers: string[];
}

export interface ProviderFilePreviewWire {
  sourceReadLimitations: string[];
  sourceTree: TransferSourceTreeWire | null;
  conflicts: string[];
  conflictDetails: FileConflictWire[];
  keepBothPreparationId: string | null;
  conflictPreparationId: string | null;
  keepBothNames: string[];
  keepBothBlocker: string | null;
  targetCapabilities: ProviderFileCapabilitiesWire;
  sourceCapabilities: ProviderFileCapabilitiesWire | null;
  skipOnly: boolean;
  sourceRemovalRequired: boolean;
  requiresConflictChoice: boolean;
}
