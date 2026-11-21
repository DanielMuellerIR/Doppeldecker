import type { BidirectionalEndpointRequest } from "./pane-endpoint.js";

export interface FilePreviewRequest {
  endpoint: BidirectionalEndpointRequest;
  path: string;
}

export type PreviewContent =
  | { kind: "text"; text: string }
  | { kind: "image"; data_url: string; width: number; height: number }
  | { kind: "unsupported"; reason: string };

export interface FilePreviewWire {
  bytes: number;
  content: PreviewContent;
  limitations: string[];
}
