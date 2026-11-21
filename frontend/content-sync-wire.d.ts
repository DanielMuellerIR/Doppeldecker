declare const contentSyncPreparationDigest: unique symbol;

export interface ContentSyncPreparationDigestWire extends ReadonlyArray<number> {
  readonly [contentSyncPreparationDigest]: never;
  readonly length: 32;
}

export type ContentSyncQueuePreviewWire = import("./real-mode-wire.js").ContentSyncPreviewWire;
