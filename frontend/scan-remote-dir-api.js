// @ts-check

import { remoteProviderCommands } from "./provider-command-names.js";
import { invoke } from "./tauri-api.js";

/** @typedef {import("./provider-command-names.js").RemoteProvider} RemoteProvider */
/** @typedef {import("./scan-dir-api.js").DirectoryEntry} DirectoryEntry */

// Die Tauri-Signaturen nehmen Option<u64>; die Profilbindung verlangt für
// jeden erfolgreichen Scan eine aktuelle Revision.
/** @typedef {{profileId: string, profileRevision: number, path: string, showHidden: boolean}} ScanRemoteDirRequest */

/** @typedef {Omit<DirectoryEntry, "size"> & {size: number | null}} SftpDirectoryEntry */
/** @typedef {{display_name: string, is_hidden: boolean, reason: string}} ListingOmission */
/** @typedef {"plaintext" | "tls_identity_unverified" | "tls_identity_externally_verified" | "tls_identity_verified"} FtpSecurityStatus */

/**
 * @template {DirectoryEntry | SftpDirectoryEntry} Entry
 * @typedef {{
 *   path: string,
 *   parent: string | null,
 *   entries: Entry[],
 *   profile_revision: number,
 *   session_generation: number,
 * }} RemoteDirListing
 */

/**
 * @template {RemoteProvider} Provider
 * @typedef {Provider extends "sftp"
 *   ? RemoteDirListing<SftpDirectoryEntry> & {omitted: ListingOmission[], security: "sftp_path_best_effort"}
 *   : Provider extends "ftps"
 *     ? RemoteDirListing<DirectoryEntry> & {security: FtpSecurityStatus}
 *     : RemoteDirListing<DirectoryEntry>} ProviderDirListing
 */

/**
 * @template {RemoteProvider} Provider
 * @param {Provider} provider
 * @param {ScanRemoteDirRequest} request
 * @returns {Promise<ProviderDirListing<Provider>>}
 */
export function scanRemoteDir(provider, request) {
  return invoke(remoteProviderCommands(provider).scan, /** @satisfies {ScanRemoteDirRequest} */ ({
    profileId: request.profileId,
    profileRevision: request.profileRevision,
    path: request.path,
    showHidden: request.showHidden,
  }));
}
