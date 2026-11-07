import { invoke } from "./tauri-api.js";
import { hideModal, showOwnedModal } from "./modal.js";
import { $, escapeHtml, fmtTransferBytes, isEntryNameSafe, joinPath } from "./ui-utils.js";

const PAGE_SIZE = 40;
export function archivePackSummary(preview) {
  const omitted = preview.issues.filter(issue => issue.content_omitted).length;
  const metadata = preview.issues.length - omitted;
  return `${preview.files} Datei(en), ${preview.directories} Ordner, ${fmtTransferBytes(preview.source_bytes)} Inhalt; ${omitted} Eintrag/Teilbaum ausgelassen, ${metadata} Metadatenhinweis(e).`;
}
export function archiveUploadDetailsHtml(notice = null) {
  return `<details class="operation-error-details"><summary>Grenzen und Zwischenstand</summary>` +
    `<p>Fehlt dem Ziel atomarer Kollisionsschutz, kann ein gleichzeitig neu angelegter Name ersetzt werden (Best Effort). Ohne vollständiges Flush kann ein Stromausfall zuletzt geschriebene Daten verlieren.</p>` +
    (notice ? `<p>${escapeHtml(notice)}</p>` : "") +
    `<p>Der lokale Zwischenstand bleibt bei Konflikten und Fehlern über die Queue erreichbar. Ein unklarer Upload wird nicht automatisch wiederholt.</p></details>`;
}
export function archiveIssuesHtml(label) {
  return `<div class="archive-pack-issues" role="region" tabindex="0" aria-label="${escapeHtml(label)}"></div>` +
    `<div class="archive-pack-pages"><button type="button" class="btn ghost" data-archive-page="previous">Zurück</button><span aria-live="polite" data-archive-page-label></span><button type="button" class="btn ghost" data-archive-page="next">Weiter</button></div>`;
}
export function archivePackPreviewHtml(preview, { targetLabel = preview.target, upload = false, notice = null } = {}) {
  if (upload) return `<p>Ziel: <code class="archive-pack-path">${escapeHtml(targetLabel)}</code></p>` +
    `<p>${escapeHtml(archivePackSummary(preview))}</p>` +
    `<p>Das ZIP wird einmal lokal erstellt und danach hochgeladen. Die Quellen bleiben erhalten. Zielkonflikte werden vor dem Upload entschieden.</p>` +
    archiveUploadDetailsHtml(notice) + archiveIssuesHtml("Vollständige Archivhinweise seitenweise");
  const bestEffort = preview.commit_class === "best_effort";
  return `<p>Ein neues Archiv wird erzeugt. Die Quellen bleiben erhalten. Bestehende Archive werden nicht ersetzt.</p>` +
    `<code class="archive-pack-path">${escapeHtml(targetLabel)}</code>` +
    `<p>${escapeHtml(archivePackSummary(preview))}</p>` +
    `<p class="${bestEffort ? "archive-pack-warning" : ""}">${bestEffort
      ? "Best Effort: Das Ziel bietet keinen atomaren exklusiven Commit. Nach frischem Zielcheck wird genau ein Commit versucht und zurückgelesen. Eine gleichzeitig neu angelegte Zieldatei kann dabei ersetzt werden."
      : "Der Zielname wird mit exklusivem Commit angelegt. Eine späte Kollision bricht ab."}</p>` +
    `${!preview.file_flush || !preview.directory_flush ? `<p class="archive-pack-warning">Best Effort bei Dauerhaftigkeit: ${!preview.file_flush ? "Datei-Flush nicht garantiert. " : ""}${!preview.directory_flush ? "Ordner-Flush nicht garantiert. " : ""}Ein Stromausfall kann den zuletzt geschriebenen Stand verlieren.</p>` : ""}` +
    `<div class="archive-pack-issues" role="region" tabindex="0" aria-label="Vollständige Hinweise seitenweise"></div>` +
    `<div class="archive-pack-pages"><button type="button" class="btn ghost" data-archive-page="previous">Zurück</button><span aria-live="polite" data-archive-page-label></span><button type="button" class="btn ghost" data-archive-page="next">Weiter</button></div>`;
}
export function bindArchivePackPages(container, preview) {
  let page = 0;
  const pages = Math.max(1, Math.ceil(preview.issues.length / PAGE_SIZE));
  const render = () => {
    container.querySelector(".archive-pack-issues").innerHTML = preview.issues.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE)
      .map(issue => `<p><strong>${issue.content_omitted ? "Ausgelassen" : "Metadaten"}</strong> · <code>${escapeHtml(issue.path)}</code><br>${escapeHtml(issue.reason)}</p>`).join("") || "<p>Keine Auslassungen.</p>";
    container.querySelector("[data-archive-page-label]").textContent = `Seite ${page + 1} von ${pages} · ${preview.issues.length} Hinweise`;
    const previous = container.querySelector("[data-archive-page=previous]");
    const next = container.querySelector("[data-archive-page=next]");
    const focused = document.activeElement;
    previous.disabled = page === 0;
    next.disabled = page + 1 === pages;
    if ((focused === previous || focused === next) && focused.disabled) {
      const available = previous.disabled ? next : previous;
      (available.disabled ? container.querySelector(".archive-pack-issues") : available).focus({ preventScroll: true });
    }
  };
  container.querySelector("[data-archive-page=previous]").addEventListener("click", () => { page = Math.max(0, page - 1); render(); });
  container.querySelector("[data-archive-page=next]").addEventListener("click", () => { page = Math.min(pages - 1, page + 1); render(); });
  render();
}
export function createArchivePackUi({ panes, activeSide, selectedEntries, blocked, showDock, showError }) {
  function open(side = activeSide()) {
    if (blocked()) return;
    const pane = panes[side];
    const entries = selectedEntries(side);
    if (!entries.length || !pane.path) return;
    const capture = value => ({ provider: value.provider, path: value.path, profileId: value.profileId,
      profileRevision: value.profileRevision, navigationRevision: value.navigationRevision, tabId: value.tabs?.active?.id });
    const scope = capture(pane);
    const otherSide = side === "left" ? "right" : "left";
    const other = capture(panes[otherSide]);
    const remoteAvailable = other.provider !== "local" && Boolean(other.path && other.profileId);
    if (entries.some(entry => !isEntryNameSafe(entry))) {
      showError("ZIP-Archiv", new Error("Die Auswahl enthält keinen bedienbaren Namen.")); return;
    }
    showOwnedModal("archive-pack", token => {
      $("#modal .modal").classList.add("archive-pack-modal");
      $("#modal-title").textContent = "ZIP-Archiv erstellen";
      const body = $("#modal-body");
      body.innerHTML = `<p>Auswahl: ${entries.length} Eintrag/Einträge. Quellordner:</p><code class="archive-pack-path">${escapeHtml(scope.path)}</code>` +
        `<div class="archive-pack-options">${remoteAvailable ? `<label for="archive-pack-target">Ziel</label><select id="archive-pack-target" class="modal-input"><option value="local">Dieser Ordner · lokal</option><option value="remote">Andere Pane · ${escapeHtml(other.provider.toUpperCase())} · ${escapeHtml(other.path)}</option></select>` : ""}<label for="archive-pack-name">Archivname</label><input id="archive-pack-name" class="modal-input" type="text" autocomplete="off" spellcheck="false">` +
        `<label for="archive-pack-compression">Kompression</label><select id="archive-pack-compression" class="modal-input"><option value="deflate">Deflate · komprimieren</option><option value="stored">Stored · unkomprimiert</option></select></div>` +
        `<p class="archive-pack-status" role="status">Die Vorschau prüft Quellen, Zielname und mögliche Auslassungen.</p><div class="archive-pack-preview"></div>`;
      const name = body.querySelector("#archive-pack-name"); name.value = `${entries.length === 1 ? entries[0].name : "Auswahl"}.zip`;
      const compression = body.querySelector("#archive-pack-compression");
      const target = body.querySelector("#archive-pack-target");
      const remote = () => target?.value === "remote";
      const status = body.querySelector(".archive-pack-status");
      const result = body.querySelector(".archive-pack-preview");
      const buttons = $("#modal .modal-buttons"); buttons.replaceChildren();
      const cancel = document.createElement("button"); cancel.className = "btn ghost"; cancel.textContent = "Abbrechen";
      const prepare = document.createElement("button"); prepare.className = "btn primary"; prepare.textContent = "Vorschau erstellen";
      buttons.append(cancel, prepare);
      let closed = false; let preparationId; let requestId; let busy = false;
      const discard = id => { if (id) void invoke("discard_archive_pack", { preparationId: id }).catch(() => {}); };
      const finish = () => { if (closed) return; closed = true; discard(preparationId); discard(requestId); hideModal(token); };
      const matches = (snapshot, currentPane) => snapshot.tabId === currentPane.tabs?.active?.id
        && Object.entries(snapshot).every(([key, value]) => key === "tabId" || currentPane[key] === value);
      const current = () => !closed && matches(scope, panes[side]) && (!remote() || matches(other, panes[otherSide]));
      cancel.addEventListener("click", finish);
      const reset = () => { discard(preparationId); preparationId = null; result.replaceChildren(); prepare.textContent = "Vorschau erstellen"; status.textContent = "Optionen geändert; Vorschau erneut erstellen."; };
      name.addEventListener("input", reset); compression.addEventListener("change", reset); target?.addEventListener("change", reset);
      prepare.addEventListener("click", async () => {
        if (busy) return;
        if (!current()) { status.textContent = "Das Pane-Ziel hat sich geändert. Dialog schließen und neu öffnen."; return; }
        busy = true; prepare.disabled = true; name.disabled = true; compression.disabled = true; if (target) target.disabled = true; cancel.focus({ preventScroll: true });
        try {
          if (preparationId) {
            await invoke("confirm_archive_pack", { preparationId });
            showDock(`<span class="pill copy">${remote() ? "ZIP-Erzeugung und Upload" : "ZIP-Erzeugung"} eingereiht</span>Fortschritt, Abbruch und Recovery stehen in der Queue.`, { closable: true });
            finish();
          } else {
            requestId = crypto.randomUUID(); status.textContent = "Quellen und Ziel werden geprüft … Abbrechen bleibt möglich.";
            const response = await invoke("prepare_archive_pack", { request: { requestId, provider: scope.provider, root: scope.path,
              items: entries.map(entry => joinPath(scope.path, entry.name)), targetName: name.value, options: { compression: compression.value },
              ...(remote() ? { target: { provider: other.provider, root: other.path, profileId: other.profileId, profileRevision: other.profileRevision } } : {}) } });
            if (!current()) { discard(response.preparationId); if (!closed) status.textContent = "Das Pane-Ziel hat sich geändert. Bitte neu öffnen."; return; }
            preparationId = response.preparationId;
            const preview = response.binding.preview;
            result.innerHTML = archivePackPreviewHtml(preview, response.uploadTarget ? { targetLabel: response.uploadTarget, upload: true, notice: response.uploadNotice } : {}); bindArchivePackPages(result, response.binding.preview);
            status.textContent = response.uploadTarget ? "Hinweise prüfen. Das ZIP wird einmal lokal erzeugt und anschließend hochgeladen." : "Konkrete Wirkung geprüft. Hinweise durchsehen, dann ZIP erstellen.";
            prepare.textContent = response.uploadTarget ? "ZIP erstellen und hochladen" : "ZIP erstellen";
          }
        } catch (error) { if (!closed) { status.textContent = error.message || String(error); result.replaceChildren(); discard(preparationId); preparationId = null; prepare.textContent = "Vorschau erstellen"; } }
        finally { busy = false; if (!closed) { prepare.disabled = false; name.disabled = false; compression.disabled = false; if (target) target.disabled = false; } }
      });
      return { focusTarget: name, onEscape: finish };
    });
  }
  return { open };
}

export function confirmArchivePackPreview(preview, { confirm = true, upload = false, targetLabel = preview.target } = {}) {
  return new Promise(resolve => {
    showOwnedModal("archive-pack-review", token => {
      $("#modal .modal").classList.add("archive-pack-modal");
      $("#modal-title").textContent = confirm ? "Geänderte ZIP-Wirkung bestätigen" : "ZIP-Wirkung und Hinweise";
      const body = $("#modal-body"); body.innerHTML = archivePackPreviewHtml(preview, { upload, targetLabel }); bindArchivePackPages(body, preview);
      const buttons = $("#modal .modal-buttons"); buttons.replaceChildren(); let closed = false;
      const finish = value => { if (closed || !hideModal(token)) return; closed = true; resolve(value); };
      const cancel = document.createElement("button"); cancel.className = "btn ghost"; cancel.textContent = confirm ? "Abbrechen" : "Schließen"; cancel.addEventListener("click", () => finish(false)); buttons.append(cancel);
      if (confirm) { const ok = document.createElement("button"); ok.className = "btn primary"; ok.textContent = upload ? "ZIP erstellen und hochladen" : "Geändertes ZIP erstellen"; ok.addEventListener("click", () => finish(true)); buttons.append(ok); }
      return { focusTarget: cancel, onEscape: () => finish(false) };
    });
  });
}
