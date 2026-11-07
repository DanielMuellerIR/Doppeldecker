import { invoke } from "./tauri-api.js";
import { duplicateDialog } from "./duplicate-dialog.js";
import { escapeHtml, isEntryNameSafe, joinPath } from "./ui-utils.js";

export function createDuplicateActions({ panes, activeSide, selectedEntries, blocked, showDock, showError }) {
  let generation = 0;
  async function duplicate(side = activeSide()) {
    if (blocked()) return;
    const pane = panes[side];
    const entries = selectedEntries(side);
    if (!entries.length) return;
    if (!pane.path || entries.some(entry => !isEntryNameSafe(entry))) {
      showDock('<span class="pill warn">Duplizieren nicht vorbereitet</span>Bitte eine sicher benannte Auswahl in einem geöffneten Ordner verwenden.', { closable: true });
      return;
    }
    const scope = { provider: pane.provider, profileId: pane.profileId, profileRevision: pane.profileRevision,
      path: pane.path, navigationRevision: pane.navigationRevision };
    const requestGeneration = ++generation;
    const current = () => requestGeneration === generation && Object.entries(scope).every(([key, value]) => panes[side][key] === value);
    let preparationId;
    try {
      showDock('<span class="pill copy">Duplizieren vorbereiten</span>Quellen und freie Namen werden geprüft.', { closable: true });
      const preview = await invoke("prepare_duplicate", { request: {
        provider: scope.provider, root: scope.path, items: entries.map(entry => joinPath(scope.path, entry.name)),
      } });
      preparationId = preview.preparationId;
      if (!current()) return;
      if (!preview.supported) {
        showDock(`<span class="pill warn">Duplizieren nicht verfügbar</span>${escapeHtml(preview.blockers.join(" "))}`, { closable: true });
        return;
      }
      if (!preview.preparationId || preview.mappings.length !== entries.length) {
        throw new Error("Das Backend lieferte keine vollständig gebundene Duplizierauswahl.");
      }
      if (!await duplicateDialog(preview) || !current()) return;
      await invoke("confirm_duplicate", { preparationId: preview.preparationId });
      showDock(`<span class="pill copy">Duplizieren eingereiht</span>${preview.mappings.length} Kopie(n) im Ordner ${escapeHtml(preview.parent)}. Fortschritt und Ergebnis stehen in der Queue.`, { closable: true });
    } catch (error) {
      if (current()) showError("Duplizieren", error);
    } finally {
      if (preparationId) {
        try { await invoke("discard_duplicate", { preparationId }); } catch { /* Fensterabbau gibt den Plan ebenfalls frei. */ }
      }
    }
  }
  return { duplicate };
}
