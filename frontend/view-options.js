// Darstellungsoptionen der beiden Dateilisten.
//
// Jede Änderung gilt zunächst nur für den aktuell geöffneten Ordner. Der Schlüssel
// bindet Provider, Profil und kanonischen Pane-Pfad, damit gleichlautende lokale und
// entfernte Ordner nie dieselben Einstellungen teilen. „Als Standard verwenden“
// speichert denselben geprüften Satz als Vorgabe für Ordner ohne eigene Einstellung.

import { byteUnitsMode } from "./byte-units.js";
import { $, readStoredValue, writeStoredValue } from "./ui-utils.js";
import { COLUMN_KEYS, BUILTIN_DEFAULTS, copyOptions, sanitizeViewOptions } from "./view-options-state.js";
export { sanitizeViewOptions } from "./view-options-state.js";

const STORAGE_KEY = "doppeldecker-view-options-v1";
const LOCAL_FOLDER_SIZES_KEY = "doppeldecker-local-folder-sizes-v1";
const STORAGE_VERSION = 1;
const MAX_FOLDER_OPTIONS = 500;
let localFolderSizesSessionValue = null;

function emptyStore() {
  return {
    version: STORAGE_VERSION,
    defaults: copyOptions(BUILTIN_DEFAULTS),
    folders: {},
    order: [],
  };
}

function readStore() {
  const raw = readStoredValue(STORAGE_KEY);
  if (!raw || raw.length > 2_000_000) return emptyStore();
  try {
    const parsed = JSON.parse(raw);
    if (parsed?.version !== STORAGE_VERSION) return emptyStore();
    const defaults = sanitizeViewOptions(parsed.defaults);
    const rawFolders = parsed.folders && typeof parsed.folders === "object"
      ? parsed.folders
      : {};
    const rawOrder = Array.isArray(parsed.order) ? parsed.order : [];
    // Auch ein syntaktisch gültiger, aber manipuliert großer Speicherstand bleibt
    // auf die jüngsten Einträge begrenzt. Nur Schlüssel aus der Reihenfolgeliste
    // gelten als vollständig; doppelte Einträge werden anhand des jüngsten Treffers
    // zusammengeführt.
    const seen = new Set();
    const order = [];
    for (let index = rawOrder.length - 1; index >= 0 && order.length < MAX_FOLDER_OPTIONS; index -= 1) {
      const key = rawOrder[index];
      if (typeof key !== "string" || seen.has(key) || !Object.hasOwn(rawFolders, key)) continue;
      seen.add(key);
      order.unshift(key);
    }
    const folders = Object.create(null);
    for (const key of order) {
      folders[key] = sanitizeViewOptions(rawFolders[key], defaults);
    }
    return { version: STORAGE_VERSION, defaults, folders, order };
  } catch (_) {
    return emptyStore();
  }
}

function writeStore(store) {
  // Darstellung bleibt für diese Sitzung funktionsfähig, auch wenn Persistenz
  // etwa in einer privaten WebView nicht verfügbar ist.
  return writeStoredValue(STORAGE_KEY, JSON.stringify(store));
}

function folderKey(location) {
  if (!location || typeof location.path !== "string" || !location.path) return null;
  const provider = typeof location.provider === "string" ? location.provider : "local";
  const profileId = typeof location.profileId === "string" ? location.profileId : "";
  return JSON.stringify([provider, profileId, location.path]);
}

export function defaultViewOptions() {
  return copyOptions(readStore().defaults);
}

export function loadViewOptions(location) {
  const store = readStore();
  const key = folderKey(location);
  if (!key || !Object.hasOwn(store.folders, key)) return copyOptions(store.defaults);
  return sanitizeViewOptions(store.folders[key], store.defaults);
}

export function saveFolderViewOptions(location, value) {
  const key = folderKey(location);
  if (!key) return false;
  const store = readStore();
  store.folders[key] = sanitizeViewOptions(value, store.defaults);
  store.order = store.order.filter(item => item !== key);
  store.order.push(key);
  while (store.order.length > MAX_FOLDER_OPTIONS) {
    const oldest = store.order.shift();
    if (oldest) delete store.folders[oldest];
  }
  return writeStore(store);
}

export function saveDefaultViewOptions(value) {
  const store = readStore();
  store.defaults = sanitizeViewOptions(value, store.defaults);
  return writeStore(store);
}

export function resetFolderViewOptions(location) {
  const key = folderKey(location);
  const store = readStore();
  if (key) {
    delete store.folders[key];
    store.order = store.order.filter(item => item !== key);
    writeStore(store);
  }
  return copyOptions(store.defaults);
}

/** Finder-artige globale Wahl für lokale Panes. Sie gehört bewusst nicht zu den
 * ordnergebundenen Darstellungsoptionen: Ein Einschalten gilt in beiden Panes und
 * über Navigation hinweg. Remote-Panes lesen diesen Wert nie automatisch aus. */
export function localFolderSizesEnabled() {
  if (typeof localFolderSizesSessionValue === "boolean") return localFolderSizesSessionValue;
  localFolderSizesSessionValue = readStoredValue(LOCAL_FOLDER_SIZES_KEY) === "true";
  return localFolderSizesSessionValue;
}

export function setLocalFolderSizesEnabled(enabled, persist = true) {
  localFolderSizesSessionValue = Boolean(enabled);
  return !persist || writeStoredValue(LOCAL_FOLDER_SIZES_KEY, String(localFolderSizesSessionValue));
}

function panelHtml(side) {
  const radio = name => `view-${side}-${name}`;
  return `
    <div class="view-options-title-row">
      <strong id="view-${side}-title">Darstellung</strong>
      <span class="view-options-scope">Dieser Ordner</span>
    </div>
    <fieldset>
      <legend>Spalten</legend>
      <label><input type="checkbox" data-view-option="columns.size">Größe</label>
      <label><input type="checkbox" data-view-option="columns.kind">Art</label>
      <label><input type="checkbox" data-view-option="columns.modified">Änderungsdatum</label>
      <label><input type="checkbox" data-view-option="columns.created">Erstellungsdatum</label>
      <label><input type="checkbox" data-view-option="columns.owner">Eigentümer</label>
      <label><input type="checkbox" data-view-option="columns.group">Gruppe</label>
      <label><input type="checkbox" data-view-option="columns.permissions">Berechtigungen</label>
    </fieldset>
    <fieldset>
      <legend>Reihenfolge und Zeilen</legend>
      <label><input type="checkbox" data-view-option="foldersFirst">Ordner oben anzeigen</label>
      <label><input type="checkbox" data-view-option="foldersAlphabetical">Ordner immer alphabetisch</label>
      <label><input type="checkbox" data-view-option="boldFolders">Ordner fett darstellen</label>
      <label><input type="checkbox" data-view-option="zebraRows">Abwechselnde Zeilenfarben</label>
      <label><input type="checkbox" data-view-option="relativeDates">Relatives Datum</label>
      <label><input type="checkbox" data-view-option="showHidden">Versteckte Dateien</label>
    </fieldset>
    <div class="view-options-pairs">
      <fieldset>
        <legend>Dichte</legend>
        <label><input type="radio" name="${radio("density")}" value="compact" data-view-option="density">Kompakt</label>
        <label><input type="radio" name="${radio("density")}" value="normal" data-view-option="density">Normal</label>
      </fieldset>
      <fieldset>
        <legend>Textgröße</legend>
        <label><input type="radio" name="${radio("text")}" value="s" data-view-option="textSize">S</label>
        <label><input type="radio" name="${radio("text")}" value="m" data-view-option="textSize">M</label>
        <label><input type="radio" name="${radio("text")}" value="l" data-view-option="textSize">L</label>
      </fieldset>
    </div>
    <fieldset>
      <legend>Größeneinheit</legend>
      <label><input type="radio" name="${radio("units")}" value="decimal" data-byte-units>Dezimal (KB, MB, GB)</label>
      <label><input type="radio" name="${radio("units")}" value="binary" data-byte-units>Binär (KiB, MiB, GiB)</label>
    </fieldset>
    <fieldset data-folder-size-options>
      <legend>Ordnergrößen</legend>
      <label data-local-folder-sizes><input type="checkbox" data-local-folder-sizes-toggle>Lokale Ordner automatisch berechnen (global)</label>
      <p class="view-options-note" data-remote-folder-sizes hidden>Auf Servern wird eine Ordnergröße nur nach Klick in der Größenspalte berechnet.</p>
    </fieldset>
    <div class="view-options-status" aria-live="polite"></div>
    <div class="view-options-actions">
      <button class="btn ghost small" type="button" data-reset-folder>Zurücksetzen</button>
      <button class="btn primary small" type="button" data-save-default>Als Standard verwenden</button>
    </div>`;
}

/** Baut das nicht-modale Popover einer Pane. Die Callback-Grenze hält Navigation,
 * Neu-Listing und Zeilenrendering im Pane-Controller. */
export function initViewOptionsPopover({
  side,
  paneEl,
  getLocation,
  getOptions,
  onOptionsChange,
  onReset,
  onByteUnitsChange,
  onLocalFolderSizesChange,
}) {
  const toggle = $(".view-options-toggle", paneEl);
  const panel = document.createElement("div");
  panel.className = "view-options-popover";
  panel.id = `view-options-${side}`;
  panel.setAttribute("role", "dialog");
  panel.setAttribute("aria-labelledby", `view-${side}-title`);
  panel.hidden = true;
  panel.innerHTML = panelHtml(side);
  paneEl.appendChild(panel);
  toggle.setAttribute("aria-controls", panel.id);

  const status = $(".view-options-status", panel);

  function sync() {
    const options = getOptions();
    for (const input of panel.querySelectorAll("[data-view-option]")) {
      const key = input.dataset.viewOption;
      const value = key.startsWith("columns.")
        ? options.columns[key.slice("columns.".length)]
        : options[key];
      input.checked = input.type === "checkbox" ? Boolean(value) : input.value === value;
    }
    // „Immer alphabetisch“ beschreibt nur die Reihenfolge innerhalb des
    // zusammenhängenden Ordnerblocks. Der gespeicherte Wert bleibt erhalten, ist
    // ohne „Ordner oben“ aber bewusst inaktiv statt widersprüchlich wirksam.
    $("[data-view-option=\"foldersAlphabetical\"]", panel).disabled = !options.foldersFirst;
    for (const input of panel.querySelectorAll("[data-byte-units]")) {
      input.checked = input.value === byteUnitsMode();
    }
    const local = getLocation()?.provider === "local";
    $("[data-local-folder-sizes]", panel).hidden = !local;
    $("[data-remote-folder-sizes]", panel).hidden = local;
    $("[data-local-folder-sizes-toggle]", panel).checked = localFolderSizesEnabled();
    status.textContent = "";
  }

  function close({ restoreFocus = false } = {}) {
    if (panel.hidden) return;
    panel.hidden = true;
    toggle.setAttribute("aria-expanded", "false");
    if (restoreFocus) toggle.focus();
  }

  function open() {
    // Ein Kontextmenü-Klick läuft nach seinem Fach-Callback noch bis zum
    // Dokument-Listener weiter. Dort würde der gerade geöffnete Popover als
    // Außenklick sofort wieder geschlossen. Der Aufbau beginnt deshalb erst
    // nach dem vollständig abgearbeiteten Klickereignis.
    setTimeout(() => {
      sync();
      panel.hidden = false;
      toggle.setAttribute("aria-expanded", "true");
      panel.querySelector("input")?.focus();
    }, 0);
  }

  toggle.addEventListener("click", () => {
    if (panel.hidden) open();
    else close({ restoreFocus: true });
  });
  panel.addEventListener("change", (event) => {
    const input = event.target;
    if (input.matches("[data-byte-units]")) {
      if (input.checked) onByteUnitsChange(input.value);
      return;
    }
    if (input.matches("[data-local-folder-sizes-toggle]")) {
      onLocalFolderSizesChange(input.checked);
      return;
    }
    if (!input.matches("[data-view-option]") || (input.type === "radio" && !input.checked)) return;
    const next = copyOptions(getOptions());
    const key = input.dataset.viewOption;
    if (key.startsWith("columns.")) {
      next.columns[key.slice("columns.".length)] = input.checked;
    } else {
      next[key] = input.type === "checkbox" ? input.checked : input.value;
    }
    status.textContent = "";
    onOptionsChange(sanitizeViewOptions(next), key);
  });
  $("[data-save-default]", panel).addEventListener("click", () => {
    const saved = saveDefaultViewOptions(getOptions());
    status.textContent = saved ? "Als Standard gespeichert." : "Standard konnte nicht gespeichert werden.";
  });
  $("[data-reset-folder]", panel).addEventListener("click", () => {
    const next = resetFolderViewOptions(getLocation());
    onReset(next);
    sync();
    status.textContent = "Ordner-Einstellungen zurückgesetzt.";
  });
  document.addEventListener("click", event => {
    if (!panel.hidden && !panel.contains(event.target) && !toggle.contains(event.target)) close();
  });
  document.addEventListener("keydown", event => {
    if (!panel.hidden && event.key === "Escape") {
      close({ restoreFocus: true });
      event.preventDefault();
    }
  });

  return { close, open, sync };
}
