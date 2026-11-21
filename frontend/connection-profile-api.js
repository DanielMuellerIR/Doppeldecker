// @ts-check

import { remoteProviderCommands } from "./provider-command-names.js";
import { invoke } from "./tauri-api.js";

/** @typedef {import("./provider-command-names.js").RemoteProvider} RemoteProvider */
/** @typedef {import("./connection-profile-wire.js").SftpProfileRequest} SftpProfileRequest */
/** @typedef {import("./connection-profile-wire.js").FtpsProfileRequest} FtpsProfileRequest */
/** @typedef {import("./connection-profile-wire.js").WebDavProfileWire} WebDavProfileWire */
/** @typedef {import("./connection-profile-wire.js").SmbProfileWire} SmbProfileWire */
/** @typedef {[secretId?: string | null, profileRevision?: number | null]} SaveProfileOptions */
/** @typedef {["sftp", SftpProfileRequest, ...SaveProfileOptions]
 * | ["ftps", FtpsProfileRequest, ...SaveProfileOptions]
 * | ["webdav", WebDavProfileWire, ...SaveProfileOptions]
 * | ["smb", SmbProfileWire, ...SaveProfileOptions]} SaveProfileArgs */
/** @typedef {{profileId: string, profileRevision: number | null}} ProfileBinding */

/** @template {RemoteProvider} Provider @param {Provider} provider */
export const listProfiles = provider => invoke(remoteProviderCommands(provider).list);

/** @param {SaveProfileArgs} args */
export function saveProfile(...args) {
  const [provider, profile, secretId = null, profileRevision = null] = args;
  switch (provider) {
    case "sftp": return invoke("save_sftp_profile", { profile, secretId, profileRevision });
    case "ftps": return invoke("save_ftps_profile", { profile, secretId, profileRevision });
    case "webdav": return invoke("save_webdav_profile", { profile, secretId, profileRevision });
    case "smb": return invoke("save_smb_profile", { profile, secretId, profileRevision });
  }
  throw new Error("Unbekannter Remote-Provider");
}

/** @template {RemoteProvider} Provider @param {Provider} provider @param {{profileId: string, profileRevision: number}} binding */
export const removeProfile = (provider, binding) => invoke(remoteProviderCommands(provider).remove, binding);

/** @template {RemoteProvider} Provider @param {Provider} provider @param {ProfileBinding} binding */
export const connectProfile = (provider, binding) => invoke(remoteProviderCommands(provider).connect, binding);

/** @param {RemoteProvider} protocol @param {string} profileId @param {number | undefined} profileRevision */
export const disconnectProfile = (protocol, profileId, profileRevision) => (
  invoke("disconnect_remote_profile", { protocol, profileId, profileRevision })
);
