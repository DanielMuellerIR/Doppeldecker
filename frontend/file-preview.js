// Passive Dateivorschau. Pfade, Lesegrenzen und Formatprüfung liefert das Backend;
// kein Inhalt wird als Markup, Frame oder ausführbares Dokument eingesetzt.
import { invoke } from "./tauri-api.js";
import { hideModal, replaceModal, showOwnedModal } from "./modal.js";
import { $, errText, fmtEntrySize, isEntryNameSafe, joinPath } from "./ui-utils.js";

export function createFilePreviewUi({ panes, selectedEntries, endpointFor, showDock, showError }) {
  function open(side) {
    const pane = panes[side];
    const entries = selectedEntries(side);
    const [entry] = entries;
    if (entries.length !== 1 || !entry?.is_file || entry.is_symlink || !isEntryNameSafe(entry)) {
      showDock('<span class="pill none">Dateivorschau</span>Bitte genau eine reguläre Datei ohne Symlink auswählen.', { closable: true });
      return;
    }
    const request = { endpoint: endpointFor(pane), path: joinPath(pane.path, entry.name) };
    const name = entry.name;
    let closed = false;
    showOwnedModal("file-preview", token => {
      const close = () => {
        if (!hideModal(token)) return false;
        closed = true;
        return true;
      };
      const openExternal = async () => {
        if (!close()) return;
        try {
          if (request.endpoint.provider === "local") await invoke("open_path", { path: request.path });
          else await invoke("open_remote_snapshot", { request: { ...request, mode: "original", verification: "streaming" } });
        } catch (error) { showError("Öffnen fehlgeschlagen", error); }
      };
      const render = (result = null, error = null) => {
        $("#modal .modal").classList.add("file-preview-modal");
        $("#modal-title").textContent = `Vorschau · ${name}`;
        const body = $("#modal-body");
        body.replaceChildren();
        const details = document.createElement("p");
        details.className = "file-preview-details";
        details.textContent = result
          ? `${fmtEntrySize(result.bytes)} · Schreibgeschützte Momentaufnahme`
          : error ? "Vorschau nicht verfügbar" : "Momentaufnahme wird vorbereitet …";
        details.setAttribute("role", "status");
        body.append(details);
        const content = result?.content;
        if (!renderPassivePreviewContent(body, content, { name,
          imageError: "Das Bild konnte nicht dekodiert werden. Öffnen mit der System-App bleibt möglich." })) {
          const note = document.createElement("p");
          note.className = "file-preview-note";
          note.textContent = error ? errText(error) : content?.reason
            || "UTF-8-Text bis 1 MiB, PNG/JPEG bis 8 MiB und 24 Millionen Pixel. Schließen blendet die Vorschau aus; der begrenzte Lesevorgang läuft bis zum Abschluss weiter.";
          body.append(note);
        }
        if (content?.kind === "image") details.textContent += ` · ${content.width} × ${content.height} Pixel`;
        if (result?.limitations?.length) {
          const limitations = document.createElement("p");
          limitations.className = "file-preview-note";
          limitations.textContent = result.limitations.join(" ");
          body.append(limitations);
        }
        const buttons = $("#modal .modal-buttons");
        buttons.replaceChildren();
        const closeButton = document.createElement("button");
        closeButton.className = "btn ghost";
        closeButton.textContent = "Schließen";
        closeButton.addEventListener("click", close);
        buttons.append(closeButton);
        if (result || error) {
          const external = document.createElement("button");
          external.className = "btn primary";
          external.textContent = "Mit System-App öffnen";
          external.addEventListener("click", openExternal);
          buttons.append(external);
        }
        return { focusTarget: closeButton, onEscape: close };
      };
      void Promise.resolve().then(() => invoke("prepare_file_preview", { request })).then(result => {
        if (!closed) replaceModal(token, () => render(result));
      }).catch(error => {
        if (!closed) replaceModal(token, () => render(null, error));
      });
      return render();
    });
  }
  return { open };
}

/** Gemeinsame passive Darstellung; Typ und Daten stammen aus der Backendprüfung. */
export function renderPassivePreviewContent(container, content, { name = "", imageError = "Das Bild konnte nicht dekodiert werden." } = {}) {
  if (content?.kind === "text") {
    const text = document.createElement("pre");
    text.className = "file-preview-text";
    text.tabIndex = 0;
    text.setAttribute("role", "region");
    text.setAttribute("aria-label", "Dateiinhalt als Text");
    text.textContent = content.text || "(Leere Datei)";
    container.append(text);
    return true;
  }
  if (content?.kind === "image") {
    const frame = document.createElement("div");
    frame.className = "file-preview-image";
    const image = document.createElement("img");
    image.alt = name;
    image.src = content.data_url;
    image.addEventListener("error", () => {
      const message = document.createElement("p");
      message.textContent = imageError;
      frame.replaceChildren(message);
    }, { once: true });
    frame.append(image);
    container.append(frame);
    return true;
  }
  return false;
}
