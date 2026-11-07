import { invoke } from "./tauri-api.js";
import { $, escapeHtml, isEntryNameSafe } from "./ui-utils.js";

const PAGE_SIZE = 40;
export function createFinderCommentEditor({ getCurrent, scopeMatches, isBusy, refresh, applyEdit }) {
  let generation = 0;
  const names = snapshot => snapshot.entries.filter(isEntryNameSafe).map(entry => entry.name);
  function html() {
    const snapshot = getCurrent();
    if (!snapshot.propertyCapabilities?.comments || !names(snapshot).length) return "";
    const selected = names(snapshot), rows = snapshot.commentRows || [];
    const page = snapshot.commentReadPage || 0, pages = Math.ceil(selected.length / PAGE_SIZE);
    const editable = !snapshot.commentReading && rows.some(row => row.state);
    return `<details class="file-info-editor" data-file-info-editor="comments"><summary>Finder-Kommentare</summary>
      <p>Kommentare werden mit Finder abgeglichen.</p>
      <p role="status">${escapeHtml(snapshot.commentReadStatus || (snapshot.commentLoaded ? "" : "Zum Lesen diesen Bereich öffnen."))}</p>
      ${snapshot.commentPermissionDenied ? '<button type="button" class="btn primary" data-comment-authorize>Finder-Zugriff erlauben</button>' : ""}
      ${selected.length === 1 && rows[0]?.state ? (rows[0].state.metadataDiffers ? '<p class="file-info-note">Das Kommentar-Attribut weicht von Finder ab. Speichern gleicht beide Fassungen ab.</p>' : "") : `<ul class="file-info-tag-objects">${rows.map(row => `<li><strong>${escapeHtml(row.name)}</strong>: ${row.state ? `${escapeHtml(row.state.text ? row.state.text.slice(0, 160) + (row.state.text.length > 160 ? " …" : "") : "Kein Kommentar")}${row.state.metadataDiffers ? '<p class="file-info-note">Das Kommentar-Attribut weicht von Finder ab. Speichern gleicht beide Fassungen ab.</p>' : ""}` : escapeHtml(`${row.message} (${row.code || "io"})`)}</li>`).join("")}</ul>`}
      ${pages > 1 ? `<nav class="file-info-tag-pages" aria-label="Kommentaransicht"><button type="button" class="btn ghost" data-comment-page="-1"${!page || snapshot.commentReading ? " disabled" : ""}>Vorige Objekte</button><span>Seite ${page + 1} von ${pages}</span><button type="button" class="btn ghost" data-comment-page="1"${page + 1 === pages || snapshot.commentReading ? " disabled" : ""}>Weitere Objekte</button></nav>` : ""}
      ${editable ? `<label for="file-info-comment-text">${selected.length === 1 ? "Kommentar" : `Neuer Kommentar für alle ${selected.length} ausgewählten Objekte`}</label>
        <textarea id="file-info-comment-text" class="modal-input file-info-comment-text" rows="4" maxlength="16384">${escapeHtml(snapshot.commentDraft || "")}</textarea>
        <div class="file-info-tag-actions"><button type="button" class="btn primary" data-comment-save data-file-info-edit-action="comments">Kommentar speichern${selected.length === 1 ? "" : ` (${selected.length} Objekte)`}</button><button type="button" class="btn ghost" data-comment-clear>Kommentar${selected.length === 1 ? "" : ` bei allen ${selected.length}`} entfernen</button></div>` : ""}
      ${snapshot.commentLoaded ? `<button type="button" class="btn ghost" data-comment-reload${snapshot.commentReading ? " disabled" : ""}>Erneut lesen</button>` : ""}
      <button type="button" class="btn ghost" data-file-info-property-cancel="comments"${isBusy() && snapshot.propertyKind === "comments" ? "" : " hidden"}>Änderung abbrechen</button>
      <p role="status" data-file-info-comments-status>${escapeHtml(snapshot.commentsStatus || "")}</p>
    </details>`;
  }
  async function load(page = 0, requestPermission = false) {
    const snapshot = getCurrent();
    if (!snapshot?.propertyCapabilities?.comments || !scopeMatches() || isBusy()) return;
    const selected = names(snapshot), requestGeneration = ++generation;
    snapshot.commentReadPage = Math.max(0, Math.min(Math.ceil(selected.length / PAGE_SIZE) - 1, page));
    snapshot.commentReading = true; snapshot.commentRows = []; snapshot.commentPermissionDenied = false;
    snapshot.commentReadStatus = "Finder-Kommentare werden gelesen …"; refresh();
    try {
      const rows = await invoke("get_file_finder_comments", { request: { provider: snapshot.provider, root: snapshot.parent,
        names: selected.slice(snapshot.commentReadPage * PAGE_SIZE, (snapshot.commentReadPage + 1) * PAGE_SIZE), requestPermission } });
      if (getCurrent() !== snapshot || generation !== requestGeneration || !scopeMatches()) return;
      snapshot.commentRows = rows;
      snapshot.commentPermissionDenied = rows.some(row => row.permissionRequired);
      snapshot.commentReadStatus = "";
      if (selected.length === 1) snapshot.commentDraft = rows[0]?.state?.text || "";
    } catch (error) {
      if (getCurrent() !== snapshot || generation !== requestGeneration || !scopeMatches()) return;
      snapshot.commentReadStatus = error.message || String(error);
    } finally {
      if (getCurrent() === snapshot && generation === requestGeneration) {
        snapshot.commentLoaded = true; snapshot.commentReading = false; refresh();
      }
    }
  }
  async function change(text) {
    const snapshot = getCurrent();
    const report = await applyEdit("comments", "change_file_finder_comments", { text }, "Finder-Kommentare werden gespeichert … Abbruch bleibt möglich.");
    if (report && getCurrent() === snapshot && scopeMatches()) await load(snapshot.commentReadPage || 0);
  }
  function click(event) {
    const button = event.target.closest("button");
    if (!button || !Object.keys(button.dataset).some(key => key.startsWith("comment"))) return false;
    if (isBusy() || !scopeMatches()) return true;
    const snapshot = getCurrent();
    if (button.hasAttribute("data-comment-authorize")) void load(snapshot.commentReadPage || 0, true);
    else if (button.hasAttribute("data-comment-reload")) void load(snapshot.commentReadPage || 0);
    else if (button.hasAttribute("data-comment-page")) void load((snapshot.commentReadPage || 0) + Number(button.dataset.commentPage));
    else if (button.hasAttribute("data-comment-clear")) void change("");
    else if (button.hasAttribute("data-comment-save")) {
      const text = $("#file-info-comment-text").value;
      if (!text.length) { snapshot.commentsStatus = "Zum Leeren bitte „Kommentar entfernen“ wählen."; refresh(); }
      else void change(text);
    }
    return true;
  }
  function connect(body) {
    const snapshot = getCurrent();
    body.querySelector('[data-file-info-editor="comments"]')?.addEventListener("toggle", event => {
      if (event.target.open && getCurrent() === snapshot && !snapshot.commentLoaded && !snapshot.commentReading) void load();
    });
    body.querySelector("#file-info-comment-text")?.addEventListener("input", event => { snapshot.commentDraft = event.target.value; });
  }
  return { html, load, click, connect };
}
