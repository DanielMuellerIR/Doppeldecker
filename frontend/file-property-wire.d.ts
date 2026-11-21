export interface FilePropertyReadRequest {
  provider: string;
  root: string;
  names: string[];
}

export interface FilePropertyEditRequest extends FilePropertyReadRequest {
  requestId: string;
}

export interface FinderTagWire { name: string; color: number }

export type FinderTagEdit =
  | { type: "upsert"; tag: FinderTagWire; previousName?: string | null }
  | { type: "remove"; name: string }
  | { type: "clear" }
  | { type: "clear_legacy" };

export type FileFlagKind = "locked" | "hiddenExtension";

export type FinderTagEditRequest = FilePropertyEditRequest & { edit: FinderTagEdit };
export type FinderCommentReadRequest = FilePropertyReadRequest & { requestPermission?: boolean };
export type FinderCommentEditRequest = FilePropertyEditRequest & { text: string };
export type FileFlagReadRequest = FilePropertyReadRequest & { kind: FileFlagKind };
export type FileFlagEditRequest = FilePropertyEditRequest & { kind: FileFlagKind; enabled: boolean };
export type VisibilityEditRequest = FilePropertyEditRequest & { hidden: boolean };

export interface FilePropertyRowWire {
  name: string;
  code: string | null;
  message: string;
}

interface ReadRowWire<State> extends FilePropertyRowWire { state: State | null }
interface ChangedRowWire extends FilePropertyRowWire { changed: boolean }

interface FinderTagStateWire { tags: FinderTagWire[]; legacyColor: number }
interface FinderCommentStateWire { text: string; metadataDiffers: boolean }
interface VisibilityStateWire { marked: boolean; byName: boolean }

export type FinderTagReadRowWire = ReadRowWire<FinderTagStateWire>;
export type FinderTagEditRowWire = ChangedRowWire;
export type FinderCommentReadRowWire = ReadRowWire<FinderCommentStateWire> & { permissionRequired: boolean };
export type FinderCommentEditRowWire = ReadRowWire<FinderCommentStateWire> & ChangedRowWire;
export type FileFlagRowWire = ReadRowWire<boolean> & ChangedRowWire;
export type VisibilityReadRowWire = ReadRowWire<VisibilityStateWire>;
export type VisibilityEditRowWire = ReadRowWire<VisibilityStateWire> & ChangedRowWire;

export interface PermissionEditFields {
  mode: string;
  profileId?: string | null;
  profileRevision?: number | null;
  recursive?: boolean;
  administrator?: boolean;
}

export interface OwnerEditFields {
  owner: string;
  group: string;
  administrator?: boolean;
}

export interface PermissionEditRowWire extends ChangedRowWire {
  mode: number | null;
}

export interface OwnerEditRowWire extends ChangedRowWire {
  owner: string | null;
  group: string | null;
  uid: number | null;
  gid: number | null;
  specialBitsChanged: boolean | null;
}

export interface FilePropertyCapabilitiesWire {
  permissions: boolean;
  recursivePermissions: boolean;
  ownership: boolean;
  administration: boolean;
  tags: boolean;
  visibility: boolean;
  nativeFlags: boolean;
  comments: boolean;
  limitations: string[];
}

export interface AdminStatusWire {
  available: boolean;
  state: "notRegistered" | "enabled" | "requiresApproval" | "notFound"
    | "invalidPackage" | "unavailable" | "polkit" | "unsupported";
  message: string;
}
