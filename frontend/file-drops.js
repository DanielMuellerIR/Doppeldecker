// Datei-Drops verwenden dieselbe Vorschau, Konfliktbehandlung und Queue wie F5.
// Die Auswahl bleibt im RAM dieses Fensters; Drag-Payloads erteilen keine Rechte.
import { listen } from "./tauri-api.js";

const FILE_DRAG_TYPE = "application/x-doppeldecker-files";

export async function wireFileDrops({ panes, fileActions, running, showError }) {
  let internalDrag = null;
  let clearTimer;
  let nativeFiles = false;
  const blocked = () => running() || !document.querySelector("#modal").hidden;
  const clearMark = () => {
    for (const pane of Object.values(panes)) pane.el.classList.remove("file-drop-target");
  };
  function mark(side) {
    clearMark();
    if (side) panes[side].el.classList.add("file-drop-target");
  }
  function targetAt(position) {
    if (!position || blocked()) return null;
    // Wry 0.55.1 reicht auf macOS NSPoint und auf Linux GTK-Widgetkoordinaten
    // unverändert durch. Beides sind logische Punkte, trotz PhysicalPosition im
    // Tauri-Typ. Durch devicePixelRatio zu teilen würde Retina-Ziele verfehlen.
    const element = document.elementFromPoint(position.x, position.y);
    return Object.keys(panes).find(side => panes[side].el.contains(element) && panes[side].path) || null;
  }
  function accept(side, paths = []) {
    const drag = internalDrag;
    nativeFiles = false;
    internalDrag = null; // DOM- und native Meldung dürfen nie zweimal einreihen.
    clearMark();
    if (!side || blocked() || (!drag && !paths.length)) return;
    void fileActions.copyDrop(side, drag, paths).catch(error => showError("Kopieren", error));
  }
  for (const [side, pane] of Object.entries(panes)) {
    pane.listing.addEventListener("dragstart", event => {
      clearTimeout(clearTimer);
      internalDrag = null;
      const row = event.target.closest(".row");
      if (!row || blocked()) {
        event.preventDefault();
        return;
      }
      internalDrag = fileActions.captureDrag(side, Number(row.dataset.i));
      if (!internalDrag) {
        event.preventDefault();
        return;
      }
      event.dataTransfer.setData(FILE_DRAG_TYPE, "copy");
      event.dataTransfer.effectAllowed = "copy";
    });
    // Die DOM-Handler `dragover`/`drop` greifen nur auf Linux und im Browser-Mock.
    // Auf macOS beantwortet tauri-runtime-wry den nativen Drag selbst, WebKit
    // erhält die DOM-Ereignisse nicht; der interne Drop kommt dort ausschließlich
    // über `tauri://drag-drop` mit leeren `paths` an (deshalb die Nachlaufzeit
    // nach `dragend` weiter unten).
    pane.el.addEventListener("dragover", event => {
      if (!internalDrag || blocked() || !pane.path) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = "copy";
      mark(side);
    });
    pane.el.addEventListener("dragleave", event => {
      if (!pane.el.contains(event.relatedTarget)) clearMark();
    });
    pane.el.addEventListener("drop", event => {
      // Externe Dateien nur aus dem nativen Event übernehmen. Browser-File.name
      // ist kein vollständiger Quellpfad und darf niemals als solcher dienen.
      event.preventDefault();
      if (internalDrag) accept(side);
    });
  }
  document.addEventListener("dragstart", event => {
    if (!event.dataTransfer.types.includes(FILE_DRAG_TYPE)) internalDrag = null;
  });
  document.addEventListener("dragend", () => {
    clearMark();
    // macOS kann dragend vor der asynchronen nativen Drop-Meldung zustellen.
    // Eine kurze Nachlaufzeit erhält deren Auswahl; die nächste Geste verwirft sie.
    clearTimer = setTimeout(() => { internalDrag = null; }, 1000);
  });
  document.addEventListener("mousedown", () => { internalDrag = null; });
  await listen("tauri://drag-enter", ({ payload }) => {
    nativeFiles = Boolean(payload.paths?.length);
    if (nativeFiles) internalDrag = null;
    if (internalDrag || payload.paths?.length) mark(targetAt(payload.position));
  });
  await listen("tauri://drag-over", ({ payload }) => {
    if (internalDrag || nativeFiles) mark(targetAt(payload.position));
  });
  await listen("tauri://drag-leave", () => { nativeFiles = false; clearMark(); });
  await listen("tauri://drag-drop", ({ payload }) => accept(targetAt(payload.position), payload.paths || []));
}
