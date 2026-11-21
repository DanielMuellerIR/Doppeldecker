import type { FilePreviewWire } from "./file-preview-wire.js";
import type { BidirectionalEndpointRequest } from "./pane-endpoint.js";

export interface ArchiveOpenRequest {
  endpoint: BidirectionalEndpointRequest;
  path: string;
}

export type ArchiveReadFormat = "zip" | "tar" | "tar_gzip";

interface ArchiveIssueWire {
  code: string;
  message: string;
}

export interface ArchiveRowWire {
  rowId: string;
  entryId?: string;
  directoryId?: string;
  displayName: string;
  kind: "file" | "directory" | "problem";
  uncompressedBytes?: number;
  duplicateName: boolean;
  canPreview: boolean;
  issues: ArchiveIssueWire[];
}

export interface ArchiveListingWire {
  sessionId: string;
  directoryId: string;
  breadcrumbs: Array<{ directoryId: string; label: string }>;
  page: number;
  pageSize: number;
  totalRows: number;
  rows: ArchiveRowWire[];
  catalog: {
    format: ArchiveReadFormat;
    complete: boolean;
    entryCount: number;
    displayedEntryCount: number;
    problemCount: number;
    stopReason?: ArchiveIssueWire;
  };
}

export interface ArchiveOpenedWire {
  requestId: string;
  sessionId: string;
  listing: ArchiveListingWire;
  canExtract: boolean;
  canExtractFile: boolean;
  origin: string | null;
}

export interface ArchivePreviewedWire {
  requestId: string;
  sessionId: string;
  entryId: string;
  preview: FilePreviewWire;
}
