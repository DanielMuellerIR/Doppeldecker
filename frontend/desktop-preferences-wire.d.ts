export type IconChoice = "blue" | "sunset" | "aurora" | "turquoise" | "rose" | "storm";
export type Theme = "auto" | "light" | "dark";
export type ByteUnits = "decimal" | "binary";
export type DiffTool = "xcode" | "fastra" | "bbedit";

export interface DesktopPreferencesWire {
  icon: IconChoice;
  theme: Theme;
  byteUnits: ByteUnits;
  localFolderSizes: boolean;
  networkDiscovery: boolean;
  restoreSession: boolean;
  diffTool: DiffTool;
}

export type PreferencePatchWire = { [Field in keyof DesktopPreferencesWire]?: DesktopPreferencesWire[Field] | null };

export interface DesktopPreferencesLoadErrorWire {
  code: string;
  message: string;
}

export interface PreferencesSnapshotWire {
  preferences: DesktopPreferencesWire;
  revision: number;
  needsMigration: boolean;
  loadError: DesktopPreferencesLoadErrorWire | null;
}

export interface DiffToolAvailabilityWire {
  id: DiffTool;
  available: boolean;
  detail: string;
}
