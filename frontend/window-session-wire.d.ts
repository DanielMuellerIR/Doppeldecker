import type { RemoteProvider } from "./provider-command-names.js";

export type PaneSide = "left" | "right";

export interface PaneStartLocationWire {
  provider: "local" | RemoteProvider;
  path: string;
  profileId?: string | null;
  profileRevision?: number | null;
  profileName?: string | null;
}

export type WindowStartStateWire = Record<PaneSide, PaneStartLocationWire>;

export interface TabColumnsWire {
  size: boolean;
  kind: boolean;
  modified: boolean;
  created: boolean;
  owner: boolean;
  group: boolean;
  permissions: boolean;
}

export interface TabViewOptionsWire {
  columns: TabColumnsWire;
  foldersFirst: boolean;
  foldersAlphabetical: boolean;
  boldFolders: boolean;
  zebraRows: boolean;
  relativeDates: boolean;
  showHidden: boolean;
  density: string;
  textSize: string;
}

export interface SavedTabWire {
  id: string;
  location: PaneStartLocationWire;
  history: string[];
  historyIndex: number;
  selectedNames: string[];
  cursorName: string | null;
  anchorName: string | null;
  scrollTop: number;
  sort: { key: string; dir: number };
  viewOptions: TabViewOptionsWire;
}

export interface PaneTabsWire {
  activeId: string;
  tabs: SavedTabWire[];
}

export type WindowTabsWire = Record<PaneSide, PaneTabsWire>;
