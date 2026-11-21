export interface DuplicateRequestWire {
  provider: string;
  root: string;
  items: string[];
}

export interface DuplicatePreviewWire {
  supported: boolean;
  preparationId: string | null;
  parent: string;
  mappings: { sourceName: string; targetName: string }[];
  blockers: string[];
}
