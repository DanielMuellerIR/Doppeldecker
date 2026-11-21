import type { ContentSyncPreparationDigestWire } from "./content-sync-wire.js";
import type { SyncOptionsWire, SyncPlanWire } from "./local-sync-wire.js";
import type { BidirectionalEndpointRequest } from "./pane-endpoint.js";
import type { DirectSyncProtocol, RemoteProvider } from "./provider-command-names.js";

interface ProviderPair {
  source: BidirectionalEndpointRequest;
  target: BidirectionalEndpointRequest;
}

interface LegacyRemoteEndpoints {
  profileId: string;
  profileRevision?: number | null;
  direction: "Upload" | "Download";
  localRoot: string;
  remoteRoot: string;
}

export type RemoteSyncContext = { error?: string } & (
  | (ProviderPair & { protocol: "provider" })
  | (ProviderPair & { protocol: "content" })
  | (LegacyRemoteEndpoints & { protocol: RemoteProvider; source?: never; target?: never })
);

export type RemoteSyncPreparationRequest =
  | { protocol: "provider"; request: ProviderPair & { options: SyncOptionsWire } }
  | { protocol: "content"; request: ProviderPair & { options: SyncOptionsWire } }
  | { protocol: RemoteProvider; request: LegacyRemoteEndpoints & { options: SyncOptionsWire } };

type ProviderIdentityTag = "Local" | "Sftp" | "Ftps" | "WebDav" | "Smb";

interface RootIdentityWire {
  provider: ProviderIdentityTag;
  identity: {
    display_path?: string;
    virtual_root?: string;
    session_generation?: number;
    mount?: {
      fs_type: string;
      mount_id: number;
      mount_flags: number;
      volume_id: readonly number[] | null;
    };
    server?: {
      host?: string;
      port?: number;
      transport_security?: "plaintext" | "explicit_tls";
      certificate?: { sha256: readonly number[] };
      origin?: string;
      server_guid?: readonly number[];
      dialect?: number;
      host_key?: { sha256: readonly number[] };
      sftp_version?: number;
      canonical_root?: string;
      share?: string;
    };
  };
}

interface RealModePlanWire extends SyncPlanWire {
  readonly copy_count: number;
  readonly update_count: number;
  readonly delete_count: number;
  readonly delete_bytes: number;
  readonly bytes_to_transfer: number;
  readonly metadata_report: { readonly issues: readonly { path: string; class: string; reason: string }[] };
}

interface SyncPreviewWire {
  options: SyncOptionsWire;
  plan: RealModePlanWire;
  source_identity: RootIdentityWire;
  target_identity: RootIdentityWire;
}

export interface RealRunPreviewWire extends SyncPreviewWire {
  target_root?: never;
  expires_unix_ms?: never;
  expires_at_unix_ms: number;
  plan_digest: readonly number[];
  scope_warnings: string[];
  restore_probe_ok: boolean;
  source_listing: { entries_seen: number };
  target_listing: { entries_seen: number };
  impact: {
    new_files: number;
    new_bytes: number;
    replacements: number;
    replacement_old_bytes: number;
    quarantine_entries: number;
    quarantine_bytes: number;
    available_bytes_known: boolean;
    required_available_bytes: number;
    available_bytes: number;
  };
  provider_capabilities?: {
    add_only: boolean;
    update: boolean;
    mirror: boolean;
    quarantine_verified: boolean;
    durable_directory_flush: boolean;
  };
}

type ContentProviderKind = "local" | "sftp" | "ftps" | "web_dav" | "smb";

export interface ContentSyncPreviewWire extends SyncPreviewWire {
  expires_at_unix_ms?: never;
  source_root: string;
  target_root: string;
  source_provider: ContentProviderKind;
  target_provider: ContentProviderKind;
  expires_unix_ms: number;
  digest: ContentSyncPreparationDigestWire;
  limitations: string[];
  irreversible_replacements: number;
  irreversible_removals: number;
  irreversible_removal_bytes: number;
  blocked_removals: number;
  quarantined_replacements: number;
  quarantined_removals: number;
  quarantine_run: string | null;
  removal_confirmation: string | null;
  directory_stream_effects?: { effect: { kind: string } }[];
  directory_dacl_effects?: unknown[];
  file_dacl_effects?: unknown[];
  created_directory_dacl_effects?: unknown[];
  created_directory_before_children_effects?: unknown[];
}

export type PreparedRemoteSyncWire =
  | { protocol: "content"; preview: ContentSyncPreviewWire }
  | { protocol: DirectSyncProtocol; preview: RealRunPreviewWire };

export type PreparedRealModeWire = PreparedRemoteSyncWire | { protocol: null; preview: RealRunPreviewWire };
