import { invoke } from "./tauri-api.js";
import { isEntryNameSafe, joinPath } from "./ui-utils.js";

export const isMac = /Mac|iPhone|iPad/.test(navigator.platform);
export function isTextTarget(target) {
  return Boolean(target?.closest?.('input, textarea, select, [contenteditable]:not([contenteditable="false"])'));
}

/** Die Zwischenablage bleibt nativ. Ein Move verbraucht nur genau den gelesenen
 * Stand und erst nach vollständig erfolgreichen Queue-Abschlüssen aller Gruppen. */
export function createFileClipboard({ panes, activeSide, selectedEntries, fileActions, blocked, showDock, showError }) {
  const completed = new Map();
  const moves = new Set();
  const attempts = new Map();
  let preparing = false;
  let copyRevision = 0;
  let pendingCopy = null;

  async function copy(cut = false, side = activeSide()) {
    if (blocked()) return;
    const pane = panes[side];
    const entries = selectedEntries(side);
    if (pane.provider !== "local") {
      showDock('<span class="pill none">Datei-Zwischenablage</span>Diese Aktion unterstützt lokale Dateien. Remote-Dateien über F5/F6 übertragen.', { closable: true });
      return;
    }
    if (!entries.length || entries.some(entry => !isEntryNameSafe(entry))) return;
    const owner = ++copyRevision;
    let writing;
    try {
      writing = invoke("write_file_clipboard", { paths: entries.map(entry => joinPath(pane.path, entry.name)), cut });
      pendingCopy = writing;
      const published = await writing;
      if (published === false || owner !== copyRevision) return;
      showDock(`<span class="pill copy">${cut ? "Zum Verschieben vorgemerkt" : "Kopiert"}</span>${entries.length} Datei(en)/Ordner in der Systemzwischenablage${isMac && !cut ? " · ⌘V kopiert, ⌥⌘V verschiebt ins aktive Pane" : ""}.`, { closable: true });
    } catch (error) { if (owner === copyRevision) showError("Datei-Zwischenablage", error); }
    finally { if (pendingCopy === writing) pendingCopy = null; }
  }

  async function consume(move) {
    if (!move.ready || move.consuming || !move.ids.length || !move.ids.every(id => move.outcomes.has(id))) return;
    moves.delete(move);
    if (!move.ids.every(id => move.outcomes.get(id))) return;
    move.consuming = true;
    try { await invoke("clear_file_clipboard_if_unchanged", { version: move.version }); }
    catch (error) { showError("Datei-Zwischenablage", error); }
  }

  async function paste(forceMove = false, side = activeSide()) {
    if (blocked() || preparing) return;
    const target = fileActions.capturePasteTarget(side);
    const move = { ids: [], outcomes: new Map(), ready: false, version: null };
    preparing = true;
    try {
      // Ein unmittelbar folgendes Paste darf nicht die vorherige Auswahl lesen,
      // während der eigene Copy-Auftrag noch auf die native Übergabe wartet.
      while (pendingCopy) {
        const writing = pendingCopy;
        let published;
        try { published = await writing; }
        catch (_) { return; }
        if (pendingCopy && pendingCopy !== writing) continue;
        if (published === false) return;
        break;
      }
      const clipboard = await invoke("read_file_clipboard");
      if (!clipboard.paths.length) {
        showDock('<span class="pill none">Keine Dateien</span>Die Systemzwischenablage enthält keine lokalen Dateien.', { closable: true });
        return;
      }
      const moving = forceMove || clipboard.cut;
      move.version = clipboard.version;
      for (const prior of moves) if (prior.version !== clipboard.version) moves.delete(prior);
      if (moving) {
        moves.add(move);
        if (moves.size > 256) moves.delete(moves.values().next().value);
      }
      const accepted = await fileActions.pastePaths(moving ? "move" : "copy", target, clipboard.paths, id => {
        move.ids.push(id);
        if (completed.has(id)) move.outcomes.set(id, completed.get(id).success);
      });
      // Teilweise eingereihte Gruppen dürfen die vollständige Zwischenablage
      // nicht verbrauchen. Ihre erfolgreichen Wirkungen bleiben in der Queue.
      move.ready = accepted;
      if (!accepted || !moving) moves.delete(move);
      if (moving) await consume(move);
    } catch (error) {
      moves.delete(move);
      showError("Datei-Zwischenablage", error);
    } finally { preparing = false; }
  }

  function onDone(summary, failed = false) {
    if (!summary?.operationId) return;
    if (failed && summary.code === "queue_confirmation_required") return;
    const attempt = summary.attempt || 0;
    if ((attempts.get(summary.operationId) || 0) > attempt) return;
    if (completed.get(summary.operationId)?.attempt === attempt) return;
    const success = !failed && !summary.cancelled && !summary.skipped && !summary.symlinks_skipped;
    completed.set(summary.operationId, { attempt, success });
    if (completed.size > 256) completed.delete(completed.keys().next().value);
    for (const move of moves) {
      if (move.ids.includes(summary.operationId)) move.outcomes.set(summary.operationId, success);
      void consume(move);
    }
  }

  function onContext(context) {
    const attempt = context.attempt || 0;
    if (!context.operationId || attempt <= (attempts.get(context.operationId) || 0)) return;
    attempts.set(context.operationId, attempt);
    if (attempts.size > 256) attempts.delete(attempts.keys().next().value);
    completed.delete(context.operationId);
    for (const move of moves) move.outcomes.delete(context.operationId);
  }

  function onNativeEvent(event) {
    if (event.defaultPrevented) return;
    if (isTextTarget(event.target) || isTextTarget(document.activeElement)) {
      if (event.type === "copy" || event.type === "cut") copyRevision += 1;
      return;
    }
    const listing = document.querySelector(".pane.active .listing");
    if (!document.querySelector("#modal").hidden
      || (!listing?.contains(event.target) && !listing?.contains(document.activeElement))) return;
    event.preventDefault();
    if (event.type === "paste") void paste();
    else void copy(event.type === "cut");
  }

  return { copy, paste, onDone, onContext, onNativeEvent, onTextCopy: () => { copyRevision += 1; } };
}
