// @ts-check

import { paneEndpoint } from "./pane-endpoint.js";
import { directorySizeEligible } from "./pane-rows.js";
import { invoke } from "./tauri-api.js";
import { localFolderSizesEnabled, setLocalFolderSizesEnabled } from "./view-options.js";

/** @typedef {import("./pane-endpoint.js").PaneEndpointLocation} PaneEndpointLocation */
/** @typedef {import("./pane-endpoint.js").BidirectionalEndpointRequest} BidirectionalEndpointRequest */
/** @typedef {"left" | "right"} PaneSide */
/** @typedef {{ side: PaneSide, endpoint: BidirectionalEndpointRequest, names: string[], sessionGeneration?: number | null, recalculate: boolean, clientToken: number }} DirectorySizeRequest */
/** @typedef {{ status: string, error?: unknown }} DirectorySizeStatus */
/** @typedef {{ name: string, directorySize?: DirectorySizeStatus }} SizeEntry */
/** @typedef {{ side: PaneSide, parent: string, client_token: number }} DirectorySizeEvent */
/** @typedef {PaneEndpointLocation & {
 * entries: SizeEntry[], allEntries: SizeEntry[], directorySizeToken: number,
 * sessionGeneration?: number | null, el?: HTMLElement | null, viewPopover?: { sync: () => void } | null
 * }} DirectorySizePane */

/** @param {{
 * panes: Record<PaneSide, DirectorySizePane>,
 * onDirectorySizeChange: (side: PaneSide, entry: SizeEntry) => void,
 * onGlobalPreferenceChange: (change: {localFolderSizes: boolean}) => void,
 * renderDirectorySizeChange: (side: PaneSide) => void,
 * showError: (context: string, error: unknown) => void
 * }} dependencies */
export function createPaneDirectorySizes({
  panes, onDirectorySizeChange, onGlobalPreferenceChange, renderDirectorySizeChange, showError,
}) {
  /** @param {PaneSide} side */
  async function cancelDirectorySizeRun(side, { clear = true } = {}) {
    const pane = panes[side];
    pane.directorySizeToken += 1;
    if (clear) {
      for (const entry of pane.allEntries) delete entry.directorySize;
      if (pane.el) renderDirectorySizeChange(side);
    }
    // Ein fehlgeschlagener Abbruch darf Navigation nicht sperren. Das Frontend-
    // Token verwirft in diesem Fehlerfall weiterhin jedes verspätete Resultat.
    try {
      await invoke("cancel_directory_sizes", { side });
    } catch (_) {
      // Der nächste Backend-Start ersetzt denselben panegebundenen Lauf zusätzlich.
    }
  }

  /**
   * @param {PaneSide} side
   * @param {Iterable<string>} names
   */
  async function startDirectorySizes(side, names, { recalculate = false, automatic = false } = {}) {
    const pane = panes[side];
    const requested = new Set(names);
    const entries = pane.entries.filter(
      entry => requested.has(entry.name) && directorySizeEligible(entry),
    );
    if (!entries.length || !pane.path) return;

    // Pro Pane existiert genau ein Größenlauf. Ein manueller Neulauf ersetzt einen
    // noch laufenden Batch; fertige Resultate anderer Ordner bleiben sichtbar.
    const token = ++pane.directorySizeToken;
    for (const entry of pane.entries) {
      if (entry.directorySize?.status === "loading") delete entry.directorySize;
    }
    for (const entry of entries) entry.directorySize = { status: "loading" };
    renderDirectorySizeChange(side);
    for (const entry of entries) onDirectorySizeChange(side, entry);
    const parent = pane.path;
    try {
      await invoke("start_directory_sizes", {
        request: /** @satisfies {DirectorySizeRequest} */ ({
          side,
          endpoint: paneEndpoint(pane),
          names: entries.map(entry => entry.name),
          ...(pane.provider === "local"
            ? {}
            : { sessionGeneration: pane.sessionGeneration }),
          recalculate,
          clientToken: token,
        }),
      });
    } catch (error) {
      if (token !== pane.directorySizeToken || parent !== pane.path) return;
      for (const entry of entries) entry.directorySize = { status: "failed", error };
      renderDirectorySizeChange(side);
      for (const entry of entries) onDirectorySizeChange(side, entry);
      showError(automatic ? "Automatische Ordnergrößen" : "Ordnergröße", error);
    }
  }

  /** @param {PaneSide} side */
  function startAutomaticDirectorySizes(side) {
    const pane = panes[side];
    if (pane.provider !== "local" || !localFolderSizesEnabled()) return;
    const names = pane.entries.filter(directorySizeEligible).map(entry => entry.name);
    if (names.length) void startDirectorySizes(side, names, { automatic: true });
  }

  /** @param {boolean} enabled */
  function changeLocalFolderSizes(enabled, persist = true, notify = true) {
    if (persist && notify) {
      onGlobalPreferenceChange({ localFolderSizes: Boolean(enabled) });
      return;
    }
    setLocalFolderSizesEnabled(enabled, persist);
    for (const side of /** @type {const} */ (["left", "right"])) {
      const pane = panes[side];
      pane.viewPopover?.sync();
      if (pane.provider !== "local" || !pane.path) continue;
      if (enabled) startAutomaticDirectorySizes(side);
      else void cancelDirectorySizeRun(side);
    }
  }

  /** @param {PaneSide} side @param {DirectorySizeEvent} payload */
  function directorySizeEventMatches(side, payload) {
    const pane = panes[side];
    return payload?.side === side
      && payload.client_token === pane.directorySizeToken
      && payload.parent === pane.path;
  }

  /** @param {DirectorySizeEvent & {name: string, result: DirectorySizeStatus}} payload */
  function onDirectorySizeResult(payload) {
    const side = payload?.side;
    if (!panes[side] || !directorySizeEventMatches(side, payload)) return;
    const entry = panes[side].allEntries.find(item => item.name === payload.name);
    if (!entry || !directorySizeEligible(entry)) return;
    entry.directorySize = payload.result;
    renderDirectorySizeChange(side);
    onDirectorySizeChange(side, entry);
  }

  /**
   * Beendet einen Messlauf und gibt jeder noch wartenden Zeile ihren Endzustand.
   *
   * Eine Zeile, die auf „lädt“ stehen bleibt, würde eine laufende Messung
   * vortäuschen, die es nicht mehr gibt. Das Ereignis wird nur angenommen, wenn
   * es zum aktuellen Lauf genau dieser Pane gehört.
   * @param {DirectorySizeEvent} payload
   * @param {DirectorySizeStatus} pendingResult
   */
  function finishDirectorySizeRun(payload, pendingResult) {
    const side = payload?.side;
    if (!panes[side] || !directorySizeEventMatches(side, payload)) return false;
    const pane = panes[side];
    for (const entry of pane.allEntries) {
      if (entry.directorySize?.status === "loading") {
        entry.directorySize = pendingResult;
        onDirectorySizeChange(side, entry);
      }
    }
    renderDirectorySizeChange(side);
    return true;
  }

  /** @param {DirectorySizeEvent} payload */
  function onDirectorySizeDone(payload) {
    finishDirectorySizeRun(payload, { status: "cancelled" });
  }

  /** @param {DirectorySizeEvent & {error: unknown}} payload */
  function onDirectorySizeError(payload) {
    if (!finishDirectorySizeRun(payload, { status: "failed", error: payload.error })) return;
    showError("Ordnergröße", payload.error);
  }

  return {
    cancelDirectorySizeRun,
    startDirectorySizes,
    startAutomaticDirectorySizes,
    changeLocalFolderSizes,
    onDirectorySizeResult,
    onDirectorySizeDone,
    onDirectorySizeError,
  };
}
