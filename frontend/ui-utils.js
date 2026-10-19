// Zustandsfreie DOM-, Format- und Fehlerhelfer der Oberfläche. Einzige Ausnahme ist
// die Größeneinheit: Basis und Kürzel kommen aus `byte-units.js`, damit alle
// Größenanzeigen derselben Wahl folgen. Die Rohwerte selbst bleiben unangetastet.

import { byteUnits } from "./byte-units.js";

export function $(selector, root = document) {
  return root.querySelector(selector);
}

export function $$(selector, root = document) {
  return Array.from(root.querySelectorAll(selector));
}

export function fmtBytes(bytes) {
  if (bytes === 0) return "—";
  const { base, units } = byteUnits();
  let index = 0;
  let value = bytes;
  while (value >= base && index < units.length - 1) {
    value /= base;
    index++;
  }
  const formatted = index === 0 ? String(value) : value.toFixed(value < 10 ? 1 : 0);
  // In Bilanzen bleibt es beim knappen „B"; erst ab der nächsten Stufe zeigt das
  // Kürzel die gewählte Basis (KB/MB … dezimal, KiB/MiB … binär).
  return `${formatted.replace(".", ",")} ${index === 0 ? "B" : units[index]}`;
}

/** In Summen bedeutet null wirklich „0 B“; der Gedankenstrich bleibt nur für die
 * kompakte Größenanzeige leerer Dateien in den Panes. */
export function fmtTransferBytes(bytes) {
  return bytes === 0 ? "0 B" : fmtBytes(bytes);
}

/**
 * Größenformat der Pane-Spalte mit fester Breite. Die Einheit wechselt, sobald vier
 * Stellen vor dem Komma entstünden; die breiteste mögliche Anzeige ist deshalb
 * „999,99 MB“ beziehungsweise „999,99 MiB“ (siehe entrySizeSample in
 * column-layout.js). Bytes bleiben ganzzahlig („999 Byte“), alle größeren Einheiten
 * haben zwei Nachkommastellen mit Komma.
 *
 * Die Basis folgt der Einstellung aus `byte-units.js`. Die Vier-Stellen-Regel greift
 * unabhängig davon schon ab 1000, weil sonst binär 1000–1023 Byte vier
 * Vorkommastellen ergäben.
 *
 * Fehlt der Wert (ein Provider liefert keine Größe), bleibt die Zelle sichtbar leer.
 * Es wird nie ein Wert erfunden.
 */
export function fmtEntrySize(bytes) {
  if (bytes == null) return "";
  const raw = Number(bytes);
  if (!Number.isFinite(raw) || raw < 0) return "";
  const { base, units } = byteUnits();
  let value = raw;
  let index = 0;
  while (value >= 1000 && index < units.length - 1) {
    value /= base;
    index += 1;
  }
  if (index === 0) return `${Math.round(value)} ${units[0]}`;
  let text = value.toFixed(2);
  // Das Aufrunden der zweiten Nachkommastelle kann erneut vier Vorkommastellen
  // erzeugen (999,995 KB würde „1000,00“); dann gilt die nächstgrößere Einheit.
  if (Number(text) >= 1000 && index < units.length - 1) {
    value /= base;
    index += 1;
    text = value.toFixed(2);
  }
  return `${text.replace(".", ",")} ${units[index]}`;
}

const MONTHS_DE = [
  "Januar", "Februar", "März", "April", "Mai", "Juni",
  "Juli", "August", "September", "Oktober", "November", "Dezember",
];

/**
 * Datumsformat der Pane-Spalte in vier Stufen. Welche Stufe gilt, entscheidet allein
 * die tatsächlich verfügbare Spaltenbreite (siehe column-layout.js); der Wert selbst
 * bleibt unverändert, sortiert wird immer numerisch über mtime.
 *
 * 1: `17.07.26` · 2: `17.07.26, 12:23` · 3: `17.07.2026, 12:23`
 * 4: `17. Juli 2026 um 12:23`
 *
 * Ohne mtime (Provider liefert keinen Zeitstempel) bleibt die Zelle leer.
 */
export function fmtEntryDate(seconds, stage = 3, relative = false, now = new Date()) {
  if (seconds == null) return "";
  const date = new Date(seconds * 1000);
  if (!Number.isFinite(date.getTime())) return "";
  const pad = (value) => String(value).padStart(2, "0");
  const day = pad(date.getDate());
  const month = pad(date.getMonth() + 1);
  const year = date.getFullYear();
  const shortYear = pad(((year % 100) + 100) % 100);
  const time = `${pad(date.getHours())}:${pad(date.getMinutes())}`;
  if (relative) {
    // Kalenderdaten statt 24-Stunden-Abstände: Nach einer Zeitumstellung bleibt
    // „Gestern“ gestern. Ältere Werte fallen auf das normale Spaltenformat zurück.
    const dayStart = value => new Date(value.getFullYear(), value.getMonth(), value.getDate());
    const days = Math.round((dayStart(now).getTime() - dayStart(date).getTime()) / 86_400_000);
    if (days === 0) return `Heute, ${time}`;
    if (days === 1) return `Gestern, ${time}`;
    if (days === -1) return `Morgen, ${time}`;
  }
  if (stage <= 1) return `${day}.${month}.${shortYear}`;
  if (stage === 2) return `${day}.${month}.${shortYear}, ${time}`;
  if (stage === 3) return `${day}.${month}.${year}, ${time}`;
  return `${date.getDate()}. ${MONTHS_DE[date.getMonth()]} ${year} um ${time}`;
}

/** Dateityp für die sichtbare Art-Spalte. Die Endung stammt bereits geprüft aus
 * dem Core; aus einem bloßen Namen leitet das Frontend niemals selbst eine ab. */
export function fmtEntryKind(entry) {
  if (entry?.is_symlink) return "Symbolischer Link";
  if (entry?.is_dir) return "Ordner";
  if (entry?.is_file) {
    return entry.extension ? `${String(entry.extension).toLocaleUpperCase("de-DE")}-Datei` : "Datei";
  }
  return "Spezialdatei";
}

/** POSIX-Rechtebits als vertraute rwx-Folge. Fehlende Providerwerte bleiben leer. */
export function fmtEntryPermissions(mode) {
  if (mode == null || !Number.isInteger(Number(mode)) || Number(mode) < 0) return "";
  const value = Number(mode) & 0o777;
  const chars = [
    [0o400, "r"], [0o200, "w"], [0o100, "x"],
    [0o040, "r"], [0o020, "w"], [0o010, "x"],
    [0o004, "r"], [0o002, "w"], [0o001, "x"],
  ];
  return chars.map(([bit, label]) => value & bit ? label : "-").join("");
}

export function digestHex(bytes) {
  return (bytes || []).map(byte => Number(byte).toString(16).padStart(2, "0")).join("");
}

export function fmtDate(seconds) {
  if (seconds == null) return "";
  const date = new Date(seconds * 1000);
  const pad = (value) => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

export function joinPath(base, name) {
  return base.endsWith("/") ? base + name : `${base}/${name}`;
}

/** Lokale Namen ohne eindeutige UTF-8-Dekodierung sind nur Anzeige. Ältere und
 * Remote-Antworten ohne Flag bleiben kompatibel, weil deren Protokollnamen UTF-8 sind. */
export function isEntryNameSafe(entry) {
  return entry?.safely_decoded !== false;
}

/** Sichtbarer letzter Pfadteil für Bestätigungstexte. Leere Wurzeln bleiben leer;
 * der vollständige Pfad wird dadurch niemals verändert oder neu zusammengesetzt. */
export function baseName(path) {
  return String(path || "").replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "";
}

export function escapeHtml(value) {
  return value.replace(
    /[&<>"']/g,
    (character) => ({
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#39;",
    })[character],
  );
}

/** Anzeigetext eines strukturierten Command-Fehlers oder eines normalen JS-Fehlers. */
export function errText(error) {
  if (error && typeof error === "object" && typeof error.message === "string") {
    return error.message;
  }
  return String(error);
}

/** Stabiler Command-Fehlercode, ohne auf übersetzte Anzeigetexte zu matchen. */
export function errCode(error) {
  return error && typeof error === "object" && typeof error.code === "string"
    ? error.code
    : null;
}

/**
 * Liest einen gemerkten Wert oder `null`, wenn es keinen gibt.
 *
 * `localStorage` kann in restriktiven oder privaten WebViews gesperrt sein und
 * wirft dann schon beim Zugriff. Die Oberfläche muss trotzdem weiterlaufen: Ein
 * fehlender Wert ist derselbe Fall wie ein gesperrter Zugriff, und der Aufrufer
 * fällt auf seinen Default zurück.
 */
export function readStoredValue(key) {
  try {
    return localStorage.getItem(key);
  } catch (_) {
    return null;
  }
}

/**
 * Merkt einen Wert und meldet, ob das gelang.
 *
 * Ein gescheitertes Speichern ist nie ein Fehler der Aktion selbst — die Wahl
 * gilt dann eben nur für diese Sitzung.
 */
export function writeStoredValue(key, value) {
  try {
    localStorage.setItem(key, value);
    return true;
  } catch (_) {
    return false;
  }
}

export function sourceReadReport(notes = [], tree = null) {
  const parts = [];
  if (notes.length) parts.push(escapeHtml(notes[0]));
  if (tree) parts.push(`Quellumfang: ${tree.files} Datei(en), ${tree.directories} Ordner; ${tree.omitted.length} Eintrag/Teilbaum ausgelassen`);
  return {
    summary: parts.join(" · "),
    details: notes.length ? `<details class="operation-error-details"><summary>Quellgrenzen und Auslassungen anzeigen</summary><textarea class="listing-omissions-text operation-error-body" aria-label="Quellgrenzen und Auslassungen" readonly>${escapeHtml(notes.join("\n"))}</textarea></details>` : "",
  };
}
