import { escapeHtml } from "./ui-utils.js";

const itemHtml = item => `<li><code>${escapeHtml(item.directory || "(unbekannt)")}</code></li>`;

export function createdDirectoryDaclEffectsHtml(effects = []) {
  if (!effects.length) return "";
  return `<div class="real-card"><strong>Rechte neuer leerer Ordner · ${effects.length} Ordner</strong>` +
    `<p>Nur tatsächlich neu angelegte, vollständig leere Ordner erhalten die bestätigten Quellrechte. Die Änderung ist dauerhaft und kann nicht automatisch rückgängig gemacht werden. Fehlen zusätzliche Zugriffe oder passende Beobachtungen, wird nur die Rechteänderung ausgelassen; die normale Anlage bleibt erhalten.</p>` +
    `<details class="operation-error-details"><summary>Betroffene neue Ordner anzeigen (${effects.length})</summary><ul>${effects.map(itemHtml).join("")}</ul></details></div>`;
}

export function createdDirectoryBeforeChildrenEffectsHtml(effects = []) {
  if (!effects.length) return "";
  return `<div class="real-card"><strong>Rechte vor den Kindern · ${effects.length} neue nichtleere Ordner</strong>` +
    `<p>Bestätigte Quellrechte werden vor den Kinderaktionen gesetzt und bleiben bei einem späteren Kind- oder Metadatenfehler bestehen. Kinder und gewöhnliche Ordner-Metadaten sind danach noch auszuführen. Keine automatische Wiederherstellung; technisch nicht mögliche Rechte werden konkret ausgelassen.</p>` +
    `<details class="operation-error-details"><summary>Betroffene neue Ordner anzeigen (${effects.length})</summary><ul>${effects.map(itemHtml).join("")}</ul></details></div>`;
}

const beforeChildrenOrigin = "ExclusiveMinimalCreateBeforeChildren";
const isBeforeChildren = item => (item.effect_key || item).origin === beforeChildrenOrigin;

export function createdDirectoryBeforeChildrenPendingHtml(operation, recovery) {
  const planned = operation?.type === "ContentSync" && operation.expected_plan?.actions?.some(action =>
    action.type === "CreateDir" && action.metadata?.windows_dacl?.CreatedDirectoryBeforeChildrenExact);
  if (!recovery || !planned) return "";
  return `<div class="created-directory-dacl-report quarantine-meta"><strong>Früher Rechteabschluss noch zu prüfen</strong><p>Die Anlageabsicht gehört zu einem neuen nichtleeren Ordner. Ein vollständiger Kinder- oder Metadatenabschluss ist damit nicht bestätigt. Die Prüfung setzt keine Rechte erneut und setzt keine Kinderarbeit fort.</p></div>`;
}

export function createdDirectoryDaclReportHtml(...reports) {
  return reportHtml(reports, false) + reportHtml(reports, true);
}

function reportHtml(reports, beforeChildren) {
  const created = new Map();
  const completed = new Map();
  const uncertain = new Map();
  for (const report of reports) {
    for (const item of report?.created || []) {
      if (isBeforeChildren(item) !== beforeChildren) continue;
      const key = JSON.stringify(item.effect_key);
      const previous = created.get(key);
      if (!previous || item.ordinary_metadata_applied) created.set(key, item);
    }
    for (const item of report?.completed || []) if (isBeforeChildren(item) === beforeChildren) completed.set(JSON.stringify(item), item);
    for (const item of report?.uncertain || []) if (isBeforeChildren(item) === beforeChildren) uncertain.set(JSON.stringify(item), item);
  }
  for (const key of completed.keys()) uncertain.delete(key);
  if (!created.size && !completed.size && !uncertain.size) return "";
  const group = (items, label) => items.size ? `<details class="operation-error-details"><summary>${label}: ${items.size} Ordner</summary><ul>${[...items.values()].map(itemHtml).join("")}</ul></details>` : "";
  const normalConfirmed = [...created.values()].filter(item => item.ordinary_metadata_applied).length;
  const heading = beforeChildren ? "Ordnerrechte vor den Kindern" : "Rechte neuer Ordner";
  const explanation = beforeChildren
    ? "Kinderinhalte und der gewöhnliche Ordnerabschluss werden getrennt bilanziert. Bestätigte Rechte bleiben bei Abbruch oder späteren Fehlern bestehen. Die Prüfung liest nur Identität und Rechtezustand; sie setzt keine Rechte erneut, stellt keinen früheren Zustand wieder her und setzt keine Kinderarbeit fort."
    : "Bestätigte Änderungen bleiben bei Abbruch bestehen. Es werden nur Identität und aktueller Rechtezustand geprüft. Doppeldecker setzt keine Rechte erneut und stellt keinen früheren Zustand wieder her.";
  return `<div class="created-directory-dacl-report quarantine-meta">${created.size ? `<details class="operation-error-details"><summary>${created.size} neue Ordner angelegt · gewöhnliche Metadaten bei ${normalConfirmed} bestätigt</summary><ul>${[...created.values()].map(item => itemHtml(item.effect_key)).join("")}</ul></details>` : ""}<strong>${heading}: ${completed.size} bestätigt · ${uncertain.size} Ausgang unklar</strong>` +
    `<p>${explanation}</p>` + group(completed, "Bestätigte Rechteänderungen") + group(uncertain, "Ungeklärte Rechteänderungen") + `</div>`;
}
