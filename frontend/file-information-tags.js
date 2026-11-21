// @ts-check

import { editFinderTags, readFinderTags } from "./file-property-api.js";
import { $, errText, escapeHtml, isEntryNameSafe } from "./ui-utils.js";

const COLORS = ["Ohne Farbe", "Grau", "Grün", "Violett", "Blau", "Gelb", "Rot", "Orange"];
/** @typedef {import("./file-information.js").FilePropertySnapshot} FilePropertySnapshot */
/** @typedef {FilePropertySnapshot & {
 * tagRows?: import("./file-property-wire.js").FinderTagReadRowWire[],
 * tagReadPage?: number, tagReadStatus?: string, tagReading?: boolean,
 * tagEditName?: string | null, tagPreviousName?: string | null, tagsStatus?: string
 * }} EditorSnapshot */
/** @typedef {import("./file-information.js").FilePropertyEditorCallbacks<
 * EditorSnapshot, {edit: import("./file-property-wire.js").FinderTagEdit, names: string[]},
 * import("./file-property-wire.js").FinderTagEditRowWire>} EditorOptions */

const PAGE_SIZE = 40;

/** @param {EditorOptions} options */
export function createFinderTagEditor({ getCurrent, scopeMatches, isBusy, refresh, applyEdit }) {
  let readGeneration = 0;
  /** @param {EditorSnapshot} snapshot */
  const names = snapshot => snapshot.entries.filter(isEntryNameSafe).map(entry => entry.name);

  function html() {
    const snapshot = getCurrent();
    if (!snapshot?.propertyCapabilities?.tags) return "";
    const selected = names(snapshot);
    if (!selected.length) return "";
    const page = snapshot.tagReadPage || 0;
    const pages = Math.ceil(selected.length / PAGE_SIZE);
    const rows = snapshot.tagRows || [];
    const scope = snapshot.tagEditName
      ? `Tag bei „${snapshot.tagEditName}“ bearbeiten`
      : `Tag zu ${selected.length === 1 ? "diesem Objekt" : `allen ${selected.length} ausgewählten Objekten`} hinzufügen`;
    return `<details class="file-info-tags file-info-editor" data-file-info-editor="tags"><summary>Finder-Tags</summary>
      <p>Tags gelten für das jeweilige Objekt und weitere Hardlinknamen. Links werden nicht verfolgt.</p>
      ${selected.length < snapshot.entries.length ? '<p class="file-info-note">Nicht eindeutig dekodierbare Namen werden ausgelassen.</p>' : ""}
      <p role="status" data-file-info-tags-read-status>${escapeHtml(snapshot.tagReadStatus || "")}</p>
      <ul class="file-info-tag-objects">${rows.map((row, index) => `<li><strong>${escapeHtml(row.name)}</strong>
        ${row.state ? `<div class="file-info-tag-list">${row.state.tags.map((tag, tagIndex) => `<span class="file-info-tag" data-tag-color="${Number(tag.color)}"><span>${escapeHtml(tag.name)} · ${COLORS[tag.color]}</span><button type="button" class="btn ghost small" data-tag-edit="${index}:${tagIndex}" aria-label="${escapeHtml(`${tag.name} bei ${row.name} bearbeiten`)}">Bearbeiten</button><button type="button" class="btn ghost small" data-tag-remove="${index}:${tagIndex}" aria-label="${escapeHtml(`${tag.name} bei ${row.name} entfernen`)}">Entfernen</button></span>`).join("") || '<span class="file-info-note">Keine Tags</span>'}</div>
        ${row.state.legacyColor ? `<p class="file-info-tag-legacy">Alte Finder-Farbmarkierung: ${COLORS[row.state.legacyColor]} <button type="button" class="btn ghost small" data-tag-clear-legacy="${index}">Farbmarkierung entfernen</button></p>` : ""}`
          : `<p class="file-info-tag-error">${escapeHtml(row.message)} (${escapeHtml(row.code || "io")})</p>`}</li>`).join("")}</ul>
      ${pages > 1 ? `<nav class="file-info-tag-pages" aria-label="Tagansicht"><button type="button" class="btn ghost" data-tag-page="-1"${page === 0 || snapshot.tagReading ? " disabled" : ""}>Vorige Objekte</button><span>Seite ${page + 1} von ${pages}</span><button type="button" class="btn ghost" data-tag-page="1"${page + 1 === pages || snapshot.tagReading ? " disabled" : ""}>Weitere Objekte</button></nav>` : ""}
      <p class="file-info-tag-scope">${escapeHtml(scope)}</p>
      <div class="file-info-tag-fields"><div><label for="file-info-tag-name">Tagname</label><input id="file-info-tag-name" class="modal-input" type="text" autocomplete="off" maxlength="1024"></div><div><label for="file-info-tag-color">Tagfarbe</label><select id="file-info-tag-color" class="modal-input">${COLORS.map((color, index) => `<option value="${index}">${color}</option>`).join("")}</select></div></div>
      <div class="file-info-tag-actions"><button type="button" class="btn primary" data-tag-save data-file-info-edit-action="tags">${snapshot.tagEditName ? "Tag speichern" : "Tag hinzufügen"}</button>${snapshot.tagEditName ? '<button type="button" class="btn ghost" data-tag-reset>Neue Tagzuweisung</button>' : ""}<button type="button" class="btn ghost" data-tag-clear>Alle Tags und Farbmarkierungen entfernen (${selected.length} ${selected.length === 1 ? "Objekt" : "Objekte"})</button></div>
      <button type="button" class="btn ghost" data-file-info-property-cancel="tags"${isBusy() && snapshot.propertyKind === "tags" ? "" : " hidden"}>Änderung abbrechen</button>
      <p role="status" data-file-info-tags-status>${escapeHtml(snapshot.tagsStatus || "")}</p>
    </details>`;
  }

  async function load(page = 0) {
    const snapshot = getCurrent();
    if (!snapshot?.propertyCapabilities?.tags || !scopeMatches()) return;
    const selected = names(snapshot);
    if (!selected.length) return;
    const generation = ++readGeneration;
    snapshot.tagReadPage = Math.max(0, Math.min(Math.ceil(selected.length / PAGE_SIZE) - 1, page));
    snapshot.tagReading = true;
    snapshot.tagRows = [];
    snapshot.tagReadStatus = "Tags werden gelesen …";
    refresh();
    try {
      const rows = await readFinderTags({ provider: snapshot.provider,
        root: snapshot.parent, names: selected.slice(snapshot.tagReadPage * PAGE_SIZE, (snapshot.tagReadPage + 1) * PAGE_SIZE) });
      if (getCurrent() !== snapshot || generation !== readGeneration || !scopeMatches()) return;
      snapshot.tagRows = rows;
      snapshot.tagReadStatus = "";
    } catch (error) {
      if (getCurrent() !== snapshot || generation !== readGeneration || !scopeMatches()) return;
      snapshot.tagReadStatus = errText(error) || String(error);
    } finally {
      if (getCurrent() === snapshot && generation === readGeneration) { snapshot.tagReading = false; refresh(); }
    }
  }

  function reset() {
    const snapshot = getCurrent();
    if (!snapshot) return;
    snapshot.tagEditName = null;
    snapshot.tagPreviousName = null;
    const input = $("#file-info-tag-name");
    if (input instanceof HTMLInputElement) input.value = "";
    const color = $("#file-info-tag-color");
    if (color instanceof HTMLSelectElement) color.value = "0";
    refresh();
  }

  /** @param {import("./file-property-wire.js").FinderTagEdit} edit @param {string[]} selectedNames */
  async function change(edit, selectedNames) {
    const snapshot = getCurrent();
    if (!snapshot) return;
    const report = await applyEdit(
      "tags",
      editFinderTags,
      { edit, names: selectedNames },
      "Finder-Tags werden geändert … Abbruch bleibt möglich.",
    );
    if (!report || getCurrent() !== snapshot || !scopeMatches()) return;
    if (report.every(row => !row.code)) reset();
    await load(snapshot.tagReadPage || 0);
  }

  /** @param {Event} event */
  function click(event) {
    if (!(event.target instanceof Element)) return false;
    const button = event.target.closest("button");
    if (!(button instanceof HTMLButtonElement) || !Object.keys(button.dataset).some(key => key.startsWith("tag"))) return false;
    const snapshot = getCurrent();
    if (!snapshot || isBusy() || !scopeMatches()) return true;
    if (button.hasAttribute("data-tag-page")) { void load((snapshot.tagReadPage || 0) + Number(button.dataset.tagPage)); return true; }
    if (button.hasAttribute("data-tag-reset")) { reset(); return true; }
    if (button.hasAttribute("data-tag-clear")) { void change({ type: "clear" }, names(snapshot)); return true; }
    if (button.hasAttribute("data-tag-save")) {
      const input = $("#file-info-tag-name");
      const color = $("#file-info-tag-color");
      if (!(input instanceof HTMLInputElement) || !(color instanceof HTMLSelectElement)) return true;
      const name = input.value;
      if (!name.trim()) { snapshot.tagsStatus = "Bitte einen Tagnamen eingeben."; refresh(); $("#file-info-tag-name")?.focus(); return true; }
      void change({ type: "upsert", tag: { name, color: Number(color.value) },
        previousName: snapshot.tagPreviousName || null }, snapshot.tagEditName ? [snapshot.tagEditName] : names(snapshot));
      return true;
    }
    if (button.hasAttribute("data-tag-clear-legacy")) {
      const row = snapshot.tagRows?.[Number(button.dataset.tagClearLegacy)];
      if (row?.state) void change({ type: "clear_legacy" }, [row.name]);
      return true;
    }
    const [rowIndex, tagIndex] = (button.dataset.tagEdit || button.dataset.tagRemove || "").split(":").map(Number);
    const row = snapshot.tagRows?.[rowIndex];
    const tag = row?.state?.tags[tagIndex];
    if (!tag) return true;
    if (button.hasAttribute("data-tag-remove")) { void change({ type: "remove", name: tag.name }, [row.name]); return true; }
    snapshot.tagEditName = row.name;
    snapshot.tagPreviousName = tag.name;
    refresh();
    const input = $("#file-info-tag-name");
    const color = $("#file-info-tag-color");
    if (color instanceof HTMLSelectElement) color.value = String(tag.color);
    if (input instanceof HTMLInputElement) {
      input.value = tag.name;
      input.focus();
    }
    return true;
  }
  return { html, load, click };
}
