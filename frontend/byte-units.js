// Anzeigeeinheit für Dateigrößen: dezimal (1000) oder binär (1024).
//
// Das ist ausschließlich Darstellung. Vergleich, Sortierung, Plan-Digests,
// Platzprüfungen und Prüfsummen rechnen unverändert mit den rohen Bytes aus dem
// Kern — die Wahl hier ändert nie einen Wert, nur seine Schreibweise.
//
// Voreinstellung nach Plattform: macOS zeigt seit Jahren die von den Festplatten-
// herstellern durchgesetzte Dezimalzählung, klassische Linux-Werkzeuge die binäre.
// Umgeschaltet wird über das native Menü „Darstellung"; die Wahl wird lokal gemerkt
// und gilt für alle Größen der Oberfläche gleichzeitig.
//
// Die binäre Einstellung beschriftet bewusst mit KiB/MiB/GiB. „1,00 MB" für
// 1.048.576 Byte wäre schlicht falsch, und eine falsch beschriftete Zahl ist
// schlimmer als eine ungewohnte.

import { readStoredValue, writeStoredValue } from "./ui-utils.js";

const STORAGE_KEY = "doppeldecker-byte-base";

const DECIMAL = { base: 1000, units: ["Byte", "KB", "MB", "GB", "TB", "PB"] };
const BINARY = { base: 1024, units: ["Byte", "KiB", "MiB", "GiB", "TiB", "PiB"] };

/** Ohne gemerkte Wahl entscheidet die Plattform (theme-boot.js setzt data-platform). */
function platformDefault() {
  return document.documentElement.dataset.platform === "linux" ? "binary" : "decimal";
}

let mode = platformDefault();

/** Stellt die gemerkte Wahl her; ohne sie gilt die Plattformvorgabe. */
export function initByteUnits() {
  // Ohne lokale Persistenz gilt die Plattformvorgabe für diese Sitzung.
  const saved = readStoredValue(STORAGE_KEY);
  mode = saved === "binary" || saved === "decimal" ? saved : platformDefault();
  return mode;
}

export function setByteUnits(next, persist = true) {
  mode = next === "binary" ? "binary" : "decimal";
  // Die Anzeige funktioniert auch ohne lokale Persistenz weiter.
  if (persist) writeStoredValue(STORAGE_KEY, mode);
  return mode;
}

export function byteUnitsMode() {
  return mode;
}

/** Basis und Einheitenkürzel der aktuellen Wahl. */
export function byteUnits() {
  return mode === "binary" ? BINARY : DECIMAL;
}
