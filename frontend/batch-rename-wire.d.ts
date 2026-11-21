import type { BidirectionalEndpointRequest } from "./pane-endpoint.js";

export interface BatchRenameRequestWire {
  requestId?: string | null;
  endpoint: BidirectionalEndpointRequest;
  names: string[];
  options: {
    find: string;
    replacement: string;
    prefix: string;
    suffix: string;
    preserveExtension?: boolean;
    trimOuterSpaces?: boolean;
    numbering?: { start: number; step: number; minDigits: number; position: "before" | "after" } | null;
  };
}

interface BatchRenameRowWire {
  oldName: string;
  newName: string;
  currentName?: string;
  status: "ready" | "unchanged" | "blocked" | "renamed" | "partially_renamed" | "failed_unchanged" | "outcome_unclear" | "cancelled";
  message: string;
  code?: string;
}

interface BatchRenameResultWire {
  rows: BatchRenameRowWire[];
  executionClass?: "assured" | "best_effort";
  limitations?: string[];
}

export interface BatchRenamePreviewWire extends BatchRenameResultWire {
  requestId: string;
  planId: string;
  limitations: string[];
}

export interface BatchRenameReportWire extends BatchRenameResultWire {
  renamed: number;
  partiallyRenamed: number;
  failedUnchanged: number;
  outcomeUnclear: number;
  cancelled: number;
}

export interface BatchRenameProgressWire {
  planId: string;
  done: number;
  total: number;
}
