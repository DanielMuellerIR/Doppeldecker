import { escapeHtml } from "./ui-utils.js";

function fileHtml(outcome) {
  const scope = outcome.alias_scope || {};
  const paths = scope.selected_paths || [];
  const count = scope.observed_link_count;
  return `<li>Dateiobjekt <code>${escapeHtml(outcome.file || "(unbekannt)")}</code> · ${paths.length} ausgewählte Namen` +
    ` · beobachtete Anzahl verknüpfter Namen: ${count == null ? "unbekannt" : escapeHtml(String(count))}` +
    `<ul>${paths.map(path => `<li><code>${escapeHtml(path)}</code></li>`).join("")}</ul></li>`;
}

export function fileDaclEffectsHtml(effects = []) {
  if (!effects.length) return "";
  const aliases = effects.reduce((count, effect) => count + (effect.alias_scope?.selected_paths?.length || 0), 0);
  return `<div class="real-card"><strong>Objektweite Dateirechte · ${effects.length} Dateiobjekt(e) · ${aliases} ausgewählte Namen</strong>` +
    `<p class="real-warning">Mehrere Dateinamen können auf dasselbe Dateiobjekt verweisen (Hardlinks). Die Rechteänderung gilt für alle bestehenden und neu angelegten Namen dieses Objekts, auch außerhalb der ausgewählten Ordner. Die Liste zeigt nur die hier bekannten Namen. Eine beobachtete Anzahl von 1 begrenzt die Wirkung nicht.</p>` +
    `<p>Die Änderung ist dauerhaft und kann nicht automatisch rückgängig gemacht werden. Inhalte, benannte Datenströme, Eigentümer und Gruppe bleiben erhalten. Es werden keine Dateien kopiert oder ersetzt.</p>` +
    `<details class="operation-error-details"><summary>Alle Objektwirkungen und bekannten Namen anzeigen (${effects.length})</summary><ul>${effects.map(fileHtml).join("")}</ul></details></div>`;
}

export function fileDaclReportHtml(...reports) {
  const completed = new Map();
  const uncertain = new Map();
  for (const report of reports) {
    for (const item of report?.completed || []) completed.set(JSON.stringify(item), item);
    for (const item of report?.uncertain || []) uncertain.set(JSON.stringify(item), item);
  }
  for (const key of completed.keys()) uncertain.delete(key);
  if (!completed.size && !uncertain.size) return "";
  const group = (items, label) => items.size ? `<details class="operation-error-details"><summary>${label}: ${items.size} Dateiobjekt(e)</summary><ul>${[...items.values()].map(fileHtml).join("")}</ul></details>` : "";
  return `<div class="file-dacl-report quarantine-meta"><strong>Objektweite Dateirechte: ${completed.size} bestätigt · ${uncertain.size} Ausgang unklar</strong>` +
    `<p>Alle bestehenden und neuen Namen desselben Objekts sind betroffen, auch außerhalb der ausgewählten Ordner. Die Liste zeigt nur die hier bekannten Namen. Bestätigte Änderungen bleiben bei Abbruch bestehen. Unklare Ausgänge benötigen eine frische Rechteprüfung; Doppeldecker setzt keine Rechte erneut und stellt keinen früheren Zustand wieder her.</p>` +
    group(completed, "Bestätigte Objektwirkungen") + group(uncertain, "Ungeklärte Objektwirkungen") + `</div>`;
}
