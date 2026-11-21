import type { ArchiveReadFormat } from "./archive-read-wire.js";
import type { BidirectionalEndpointRequest, PaneEndpointLocation } from "./pane-endpoint.js";
import type { RemoteProvider } from "./provider-command-names.js";

export type ArchivePaneSide = "left" | "right";
export type ArchivePane = PaneEndpointLocation & {
  profileId?: string | null;
  profileRevision?: number | null;
  sessionGeneration?: number | null;
  navigationRevision: number;
  tabs?: { active?: { id: string } | null };
};

export type ArchivePackCompression = "stored" | "deflate";

export interface ArchivePackPrepareRequest {
  requestId: string;
  provider: string;
  root: string;
  items: string[];
  targetName: string;
  options: { compression: ArchivePackCompression };
  target?: BidirectionalEndpointRequest | null;
}

interface ArchiveExtractTargetRequest {
  requestId: string;
  targetProvider: string;
  targetDir: string;
  target?: BidirectionalEndpointRequest | null;
}

export interface ArchiveExtractPrepareRequest extends ArchiveExtractTargetRequest {
  provider: string;
  root: string;
  sourcePath: string;
}

export type ArchiveExtractSelectionPrepareRequest = ArchiveExtractTargetRequest & { sessionId: string } & (
  { entryId: string; directoryId?: null } | { directoryId: string; entryId?: null }
);

interface ArchivePreviewIssueWire {
  path: string;
  reason: string;
  content_omitted: boolean;
}

interface ArchiveOperationPreviewWire {
  source_root: string;
  files: number;
  directories: number;
  source_bytes: number;
  commit_class: "assured" | "best_effort";
  file_flush: boolean;
  directory_flush: boolean;
  issues: ArchivePreviewIssueWire[];
}

export interface ArchivePackPreviewWire extends ArchiveOperationPreviewWire {
  target: string;
}

interface ArchiveSnapshotWire {
  reference: {
    id: string;
    manifest_digest: number[];
    bytes: number;
    blake3: string;
    created_unix_ms: number;
  };
  origin: string;
  profile: { protocol: RemoteProvider; profileId: string; revision: number };
}

type ArchiveExtractSelectionWire =
  | { kind: "entry"; ordinal: number; raw_name: number[] }
  | { kind: "subtree"; prefix: string };

export interface ArchiveExtractPreviewWire extends ArchiveOperationPreviewWire {
  source_path: string;
  snapshot?: ArchiveSnapshotWire;
  format?: Exclude<ArchiveReadFormat, "zip">;
  target_dir: string;
  selection?: ArchiveExtractSelectionWire;
  relative_root?: string;
  selection_path?: string;
  catalog_complete: boolean;
  known_entries: number;
  omitted_entries: number;
  conflicts: string[];
  type_conflicts: string[];
  conflict_details: {
    path: string;
    relative_name: string;
    source_bytes: number;
    target_bytes: number;
    target_mtime: number | null;
  }[];
  replace_best_effort: boolean;
}

interface ArchivePreparedWire<Preview> {
  preparationId: string;
  binding: { digest: number[]; preview: Preview };
  uploadTarget: string | null;
  uploadNotice: string | null;
}

export type ArchivePackPreparedWire = ArchivePreparedWire<ArchivePackPreviewWire>;

export interface ArchiveExtractPreparedWire extends ArchivePreparedWire<ArchiveExtractPreviewWire> {
  requestId: string;
}
