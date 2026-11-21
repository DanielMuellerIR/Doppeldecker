// @ts-check

import { invoke, listen } from "./tauri-api.js";

/** @typedef {import("./desktop-preferences-wire.js").PreferencesSnapshotWire} PreferencesSnapshotWire */

export const desktopPreferences = () => invoke("desktop_preferences");
export const diffToolStatus = () => invoke("diff_tool_status");

/**
 * @param {import("./desktop-preferences-wire.js").PreferencePatchWire} patch
 * @param {boolean} [onlyIfUninitialized]
 */
export function updateDesktopPreferences(patch, onlyIfUninitialized) {
  return invoke("update_desktop_preferences", onlyIfUninitialized === undefined
    ? { patch }
    : { patch, onlyIfUninitialized });
}

/** @param {(snapshot: PreferencesSnapshotWire) => void} accept */
export function listenDesktopPreferences(accept) {
  return listen("desktop-preferences-changed", ({ payload }) => accept(payload));
}

/** @param {import("./desktop-preferences-wire.js").Theme} preference @param {boolean} dark */
export const syncNativeTheme = (preference, dark) => invoke("sync_native_theme", { preference, dark });
