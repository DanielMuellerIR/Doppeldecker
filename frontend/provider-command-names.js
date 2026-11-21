// @ts-check

/** @typedef {"sftp" | "ftps" | "webdav" | "smb"} RemoteProvider */
/** @typedef {RemoteProvider | "provider"} DirectSyncProtocol */
/** @typedef {DirectSyncProtocol | "content"} RemoteSyncProtocol */

/**
 * @template {RemoteProvider} Provider
 * @typedef {{
 *   list: `list_${Provider}_profiles`,
 *   save: `save_${Provider}_profile`,
 *   remove: `remove_${Provider}_profile`,
 *   connect: `connect_${Provider}_profile`,
 *   scan: `scan_${Provider}_dir`,
 * }} ProviderCommands
 */

/**
 * @template {RemoteSyncProtocol} Protocol
 * @typedef {{
 *   prepare: `prepare_${Protocol}_sync`,
 *   discard: `discard_${Protocol}_sync`,
 *   start: Protocol extends "content" ? "enqueue_content_sync" : `run_${Protocol}_sync`,
 * }} SyncCommands
 */

/** @type {{[Provider in RemoteProvider]: ProviderCommands<Provider>}} */
const providers = {
  sftp: {
    list: "list_sftp_profiles",
    save: "save_sftp_profile",
    remove: "remove_sftp_profile",
    connect: "connect_sftp_profile",
    scan: "scan_sftp_dir",
  },
  ftps: {
    list: "list_ftps_profiles",
    save: "save_ftps_profile",
    remove: "remove_ftps_profile",
    connect: "connect_ftps_profile",
    scan: "scan_ftps_dir",
  },
  webdav: {
    list: "list_webdav_profiles",
    save: "save_webdav_profile",
    remove: "remove_webdav_profile",
    connect: "connect_webdav_profile",
    scan: "scan_webdav_dir",
  },
  smb: {
    list: "list_smb_profiles",
    save: "save_smb_profile",
    remove: "remove_smb_profile",
    connect: "connect_smb_profile",
    scan: "scan_smb_dir",
  },
};

/** @type {{[Protocol in RemoteSyncProtocol]: SyncCommands<Protocol>}} */
const syncProtocols = {
  sftp: { prepare: "prepare_sftp_sync", discard: "discard_sftp_sync", start: "run_sftp_sync" },
  ftps: { prepare: "prepare_ftps_sync", discard: "discard_ftps_sync", start: "run_ftps_sync" },
  webdav: { prepare: "prepare_webdav_sync", discard: "discard_webdav_sync", start: "run_webdav_sync" },
  smb: { prepare: "prepare_smb_sync", discard: "discard_smb_sync", start: "run_smb_sync" },
  provider: { prepare: "prepare_provider_sync", discard: "discard_provider_sync", start: "run_provider_sync" },
  content: { prepare: "prepare_content_sync", discard: "discard_content_sync", start: "enqueue_content_sync" },
};

/**
 * @template {RemoteProvider} Provider
 * @param {Provider} provider
 * @returns {Readonly<ProviderCommands<Provider>>}
 */
export function remoteProviderCommands(provider) {
  if (!Object.hasOwn(providers, provider)) throw new Error("Unbekannter Remote-Provider");
  return providers[provider];
}

/**
 * @template {RemoteSyncProtocol} Protocol
 * @param {Protocol} protocol
 * @returns {Readonly<(typeof syncProtocols)[Protocol]>}
 */
export function remoteSyncCommands(protocol) {
  if (!Object.hasOwn(syncProtocols, protocol)) throw new Error("Unbekanntes Remote-Sync-Protokoll");
  return syncProtocols[protocol];
}
