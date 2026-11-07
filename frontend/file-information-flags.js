import { invoke } from "./tauri-api.js";
import { $, escapeHtml, isEntryNameSafe } from "./ui-utils.js";
const PAGE_SIZE = 40;

export function createFileFlagEditor({ id, kind, title, label, actionLabel, explanation, getCurrent, scopeMatches, isBusy, refresh, applyEdit }) {
  let generation = 0;
  const names = snapshot => snapshot.entries.filter(isEntryNameSafe).map(entry => entry.name);
  const field = suffix => `${id}${suffix}`;
  function html() {
    const snapshot = getCurrent();
    if (!snapshot.propertyCapabilities?.nativeFlags) return "";
    const selected = names(snapshot);
    if (!selected.length) return "";
    const rows = snapshot[field("Rows")] || [];
    const single = selected.length === 1 && typeof rows[0]?.state === "boolean";
    const page = snapshot[field("ReadPage")] || 0;
    const pages = Math.ceil(selected.length / PAGE_SIZE);
    return `<details class="file-info-editor" data-file-info-editor="${id}"><summary>${title}</summary>
      <p>${explanation} Hardlinks teilen die Eigenschaft. Links werden nicht verfolgt.</p>
      ${selected.length < snapshot.entries.length ? '<p class="file-info-note">Nicht eindeutig dekodierbare Namen werden ausgelassen.</p>' : ""}
      <p role="status">${escapeHtml(snapshot[field("ReadStatus")] || "")}</p>
      ${single ? `<label class="file-info-recursive"><input id="file-info-${id}" type="checkbox"${rows[0].state ? " checked" : ""}>${label}</label><button type="button" class="btn primary" data-file-flag="${id}" data-flag-apply data-file-info-edit-action="${id}">${actionLabel}</button>`
        : `<ul class="file-info-tag-objects">${rows.map(row => `<li><strong>${escapeHtml(row.name)}</strong>: ${escapeHtml(typeof row.state === "boolean" ? row.state ? "Aktiv" : "Inaktiv" : `${row.message} (${row.code || "io"})`)}</li>`).join("")}</ul>
        ${selected.length > 1 ? `<div class="file-info-tag-actions"><button type="button" class="btn primary" data-file-flag="${id}" data-flag-set="true">Bei allen ${selected.length} aktivieren</button><button type="button" class="btn ghost" data-file-flag="${id}" data-flag-set="false">Bei allen ${selected.length} deaktivieren</button></div>` : ""}`}
      ${pages > 1 ? `<nav class="file-info-tag-pages" aria-label="${title}"><button type="button" class="btn ghost" data-file-flag="${id}" data-flag-page="-1"${!page || snapshot[field("Reading")] ? " disabled" : ""}>Vorige Objekte</button><span>Seite ${page + 1} von ${pages}</span><button type="button" class="btn ghost" data-file-flag="${id}" data-flag-page="1"${page + 1 === pages || snapshot[field("Reading")] ? " disabled" : ""}>Weitere Objekte</button></nav>` : ""}
      <button type="button" class="btn ghost" data-file-info-property-cancel="${id}"${isBusy() && snapshot.propertyKind === id ? "" : " hidden"}>Änderung abbrechen</button>
      <p role="status" data-file-info-${id}-status>${escapeHtml(snapshot[field("Status")] || "")}</p>
    </details>`;
  }
  async function load(page = 0) {
    const snapshot = getCurrent();
    if (!snapshot?.propertyCapabilities?.nativeFlags || !scopeMatches()) return;
    const selected = names(snapshot);
    if (!selected.length) return;
    const requestGeneration = ++generation;
    snapshot[field("ReadPage")] = Math.max(0, Math.min(Math.ceil(selected.length / PAGE_SIZE) - 1, page));
    snapshot[field("Rows")] = []; snapshot[field("Reading")] = true;
    snapshot[field("ReadStatus")] = "Dateieigenschaft wird gelesen …"; refresh();
    try {
      const rows = await invoke("get_file_flags", { request: { provider: snapshot.provider, root: snapshot.parent,
        kind, names: selected.slice(snapshot[field("ReadPage")] * PAGE_SIZE, (snapshot[field("ReadPage")] + 1) * PAGE_SIZE) } });
      if (getCurrent() !== snapshot || generation !== requestGeneration || !scopeMatches()) return;
      snapshot[field("Rows")] = rows; snapshot[field("ReadStatus")] = "";
    } catch (error) {
      if (getCurrent() !== snapshot || generation !== requestGeneration || !scopeMatches()) return;
      snapshot[field("ReadStatus")] = error.message || String(error);
    } finally {
      if (getCurrent() === snapshot && generation === requestGeneration) { snapshot[field("Reading")] = false; refresh(); }
    }
  }
  async function change(enabled) {
    const snapshot = getCurrent();
    const report = await applyEdit(id, "change_file_flag", { kind, enabled }, "Dateieigenschaft wird geändert … Abbruch bleibt möglich.");
    if (report && getCurrent() === snapshot && scopeMatches()) await load(snapshot[field("ReadPage")] || 0);
  }
  function click(event) {
    const button = event.target.closest(`button[data-file-flag="${id}"]`);
    if (!button) return false;
    if (isBusy() || !scopeMatches()) return true;
    if (button.hasAttribute("data-flag-page")) void load((getCurrent()[field("ReadPage")] || 0) + Number(button.dataset.flagPage));
    else if (button.hasAttribute("data-flag-apply")) void change($(`#file-info-${id}`).checked);
    else if (button.hasAttribute("data-flag-set")) void change(button.dataset.flagSet === "true");
    return true;
  }
  return { html, load, click };
}
