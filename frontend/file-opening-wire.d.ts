import type { BidirectionalEndpointRequest } from "./pane-endpoint.js";

export type RemoteOpenMode = "passive" | "original" | "edit";
export type SnapshotVerification = "streaming" | "full_readback";

export interface OpenRemoteSnapshotRequestWire {
  endpoint: BidirectionalEndpointRequest;
  path: string;
  mode?: RemoteOpenMode;
  verification?: SnapshotVerification;
}

export interface DiffFileRequestWire {
  endpoint: BidirectionalEndpointRequest;
  path: string;
  name: string;
}

export interface RemoteEditViewWire {
  id: string;
  name: string;
  endpointLabel: string;
  dirty: boolean;
  lastOperationId: string | null;
  limitations: string[];
  replaceBestEffort: boolean;
  saveAvailable: boolean;
  saveBlocker: string | null;
}
