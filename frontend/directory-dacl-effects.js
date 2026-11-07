import { escapeHtml } from "./ui-utils.js";

function directoryHtml(outcome) {
  return `<li>Ordner <code>${escapeHtml(outcome.directory || "(unbekannt)")}</code></li>`;
}

export function directoryDaclEffectsHtml(effects = []) {
  if (!effects.length) return "";
  return `<div class="real-card"><strong>Zugriffsrechte bestehender Ordner · ${effects.length} Änderung(en)</strong>` +
    `<p class="real-warning">Die Zielordner übernehmen die expliziten Zugriffsrechte der Quelle. Diese Änderung ist dauerhaft, ohne Quarantäne oder automatische Wiederherstellung.</p>` +
    `<p>Die Rechte werden nach allen Inhalten und übrigen Metadaten gesetzt. Eigentümer, Gruppe, Ordner und ihre Inhalte bleiben erhalten.</p>` +
    `<details class="operation-error-details"><summary>Alle Rechteänderungen anzeigen (${effects.length})</summary><ul>${effects.map(directoryHtml).join("")}</ul></details></div>`;
}

export function directoryDaclReportHtml(...reports) {
  const completed = new Map();
  const uncertain = new Map();
  for (const report of reports) {
    for (const item of report?.completed || []) completed.set(JSON.stringify(item), item);
    for (const item of report?.uncertain || []) uncertain.set(JSON.stringify(item), item);
  }
  for (const key of completed.keys()) uncertain.delete(key);
  if (!completed.size && !uncertain.size) return "";
  const group = (items, label) => items.size
    ? `<details class="operation-error-details"><summary>${label}: ${items.size} Ordner</summary><ul>${[...items.values()].map(directoryHtml).join("")}</ul></details>` : "";
  return `<div class="directory-dacl-report quarantine-meta"><strong>Ordnerrechte: ${completed.size} bestätigt · ${uncertain.size} Ausgang unklar</strong>` +
    `<p>Bestätigte Rechteänderungen bleiben auch bei einem Abbruch bestehen. Einen unklaren Zustand vor weiteren Änderungen frisch prüfen; es gibt keine automatische Wiederholung oder Wiederherstellung.</p>` +
    group(completed, "Bestätigte Rechteänderungen") + group(uncertain, "Ungeklärte Rechteänderungen") + `</div>`;
}
