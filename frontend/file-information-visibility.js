import { invoke } from "./tauri-api.js";
import { $, escapeHtml, isEntryNameSafe } from "./ui-utils.js";

const PAGE_SIZE = 40;

export function createVisibilityEditor({ getCurrent, scopeMatches, isBusy, refresh, applyEdit }) {
  let generation = 0;
  const names = snapshot => snapshot.entries.filter(isEntryNameSafe).map(entry => entry.name);

  function html() {
    const snapshot = getCurrent();
    if (!snapshot.propertyCapabilities?.visibility) return "";
    const selected = names(snapshot);
    if (!selected.length) return "";
    const rows = snapshot.visibilityRows || [];
    const single = selected.length === 1 && rows[0]?.state;
    const page = snapshot.visibilityReadPage || 0;
    const pages = Math.ceil(selected.length / PAGE_SIZE);
    return `<details class="file-info-editor" data-file-info-editor="visibility"><summary>Sichtbarkeit</summary>
      <p>Hardlinks teilen die Markierung. Punktnamen bleiben verborgen. Links werden nicht verfolgt.</p>
      ${selected.length < snapshot.entries.length ? '<p class="file-info-note">Nicht eindeutig dekodierbare Namen werden ausgelassen.</p>' : ""}
      <p role="status">${escapeHtml(snapshot.visibilityReadStatus || "")}</p>
      ${single ? `<label class="file-info-recursive"><input id="file-info-hidden" type="checkbox"${single.marked ? " checked" : ""}>Verstecktmarkierung (macOS)</label>
        ${single.byName ? '<p class="file-info-note">Dieses Objekt besitzt einen Punktnamen und bleibt auch ohne Markierung ausgeblendet.</p>' : ""}
        <button type="button" class="btn primary" data-visibility-apply data-file-info-edit-action="visibility">Sichtbarkeit anwenden</button>`
        : `<ul class="file-info-tag-objects">${rows.map(row => `<li><strong>${escapeHtml(row.name)}</strong>: ${escapeHtml(row.state ? row.state.marked ? "Markiert als versteckt" : row.state.byName ? "Punktname; ohne Markierung" : "Sichtbar" : `${row.message} (${row.code || "io"})`)}</li>`).join("")}</ul>
        ${selected.length > 1 ? `<div class="file-info-tag-actions"><button type="button" class="btn primary" data-visibility-set="true">Alle ${selected.length} als versteckt markieren</button><button type="button" class="btn ghost" data-visibility-set="false">Verstecktmarkierung bei allen ${selected.length} entfernen</button></div>` : ""}`}
      ${pages > 1 ? `<nav class="file-info-tag-pages" aria-label="Sichtbarkeitsansicht"><button type="button" class="btn ghost" data-visibility-page="-1"${!page || snapshot.visibilityReading ? " disabled" : ""}>Vorige Objekte</button><span>Seite ${page + 1} von ${pages}</span><button type="button" class="btn ghost" data-visibility-page="1"${page + 1 === pages || snapshot.visibilityReading ? " disabled" : ""}>Weitere Objekte</button></nav>` : ""}
      <button type="button" class="btn ghost" data-file-info-property-cancel="visibility"${isBusy() && snapshot.propertyKind === "visibility" ? "" : " hidden"}>Änderung abbrechen</button>
      <p role="status" data-file-info-visibility-status>${escapeHtml(snapshot.visibilityStatus || "")}</p>
    </details>`;
  }

  async function load(page = 0) {
    const snapshot = getCurrent();
    if (!snapshot?.propertyCapabilities?.visibility || !scopeMatches()) return;
    const selected = names(snapshot);
    if (!selected.length) return;
    const requestGeneration = ++generation;
    snapshot.visibilityReadPage = Math.max(0, Math.min(Math.ceil(selected.length / PAGE_SIZE) - 1, page));
    snapshot.visibilityRows = [];
    snapshot.visibilityReading = true;
    snapshot.visibilityReadStatus = "Sichtbarkeit wird gelesen …";
    refresh();
    try {
      const rows = await invoke("get_file_visibility", { request: { provider: snapshot.provider,
        root: snapshot.parent, names: selected.slice(snapshot.visibilityReadPage * PAGE_SIZE, (snapshot.visibilityReadPage + 1) * PAGE_SIZE) } });
      if (getCurrent() !== snapshot || generation !== requestGeneration || !scopeMatches()) return;
      snapshot.visibilityRows = rows;
      snapshot.visibilityReadStatus = "";
    } catch (error) {
      if (getCurrent() !== snapshot || generation !== requestGeneration || !scopeMatches()) return;
      snapshot.visibilityReadStatus = error.message || String(error);
    } finally {
      if (getCurrent() === snapshot && generation === requestGeneration) { snapshot.visibilityReading = false; refresh(); }
    }
  }

  async function change(hidden) {
    const snapshot = getCurrent();
    const report = await applyEdit("visibility", "change_file_visibility", { hidden },
      "Sichtbarkeit wird geändert … Abbruch bleibt möglich.");
    if (report && getCurrent() === snapshot && scopeMatches()) await load(snapshot.visibilityReadPage || 0);
  }

  function click(event) {
    const button = event.target.closest("button");
    if (!button || !Object.keys(button.dataset).some(key => key.startsWith("visibility"))) return false;
    if (isBusy() || !scopeMatches()) return true;
    if (button.hasAttribute("data-visibility-page")) void load((getCurrent().visibilityReadPage || 0) + Number(button.dataset.visibilityPage));
    else if (button.hasAttribute("data-visibility-apply")) void change($("#file-info-hidden").checked);
    else if (button.hasAttribute("data-visibility-set")) void change(button.dataset.visibilitySet === "true");
    return true;
  }
  return { html, load, click };
}
