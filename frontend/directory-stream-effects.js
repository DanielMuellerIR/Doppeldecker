import { escapeHtml } from "./ui-utils.js";

function streamEffectHtml({ directory, effect }) {
  const before = effect.before;
  const after = effect.after;
  const name = new TextDecoder("utf-8").decode(new Uint8Array((after || before)?.name || []));
  const label = { add: "Hinzufügen", replace: "Dauerhaft ersetzen", remove: "Dauerhaft entfernen" }[effect.kind] || effect.kind;
  const bytes = effect.kind === "replace"
    ? `${before.value.size} → ${after.value.size} Bytes`
    : `${(after || before).value.size} Bytes`;
  return `<li>${escapeHtml(label)}: Ordner <code>${escapeHtml(directory || "(gewählter Wurzelordner)")}</code> · Stream <code>${escapeHtml(name || "(leerer Name)")}</code> · ${escapeHtml(bytes)}</li>`;
}

export function directoryStreamEffectsHtml(effects = []) {
  if (!effects.length) return "";
  const permanent = effects.some(({ effect }) => ["replace", "remove"].includes(effect.kind));
  return `<div class="real-card"><strong>Benannte Verzeichnisströme · ${effects.length} Einzelwirkung(en)</strong>` +
    (permanent ? `<p class="real-warning">Ersetzungen und Entfernungen sind dauerhaft, ohne Quarantäne und ohne automatische Wiederherstellung.</p>` : "") +
    (effects.some(({ effect }) => effect.kind === "replace") ? `<p>Ersetzen erfolgt als Best Effort: frische Prüfung des vollständigen Streambestands, eine Umbenennung und anschließende Ergebnisprüfung. Es gibt keinen atomaren Versionsvergleich gegen parallele Änderungen.</p>` : "") +
    `<details class="operation-error-details"><summary>Alle Streamwirkungen anzeigen (${effects.length})</summary><ul>${effects.map(streamEffectHtml).join("")}</ul></details></div>`;
}

export function directoryStreamReportHtml(...reports) {
  // Queue und letzter Fortschritt können dieselben bestätigten Wirkungen enthalten.
  const completed = new Map();
  const uncertain = new Map();
  for (const report of reports) {
    for (const item of report?.completed || []) completed.set(JSON.stringify(item), item);
    for (const item of report?.uncertain || []) uncertain.set(JSON.stringify(item), item);
  }
  for (const key of completed.keys()) uncertain.delete(key);
  if (!completed.size && !uncertain.size) return "";
  const group = (items, label) => items.size ? `<details class="operation-error-details"><summary>${label}: ${items.size} Streamwirkung(en)</summary><ul>${[...items.values()].map(streamEffectHtml).join("")}</ul></details>` : "";
  return `<div class="directory-stream-report quarantine-meta"><strong>Verzeichnisströme: ${completed.size} bestätigt · ${uncertain.size} Ausgang unklar</strong>` +
    `<p>Bestätigte Einzelwirkungen bleiben auch bei Fehler oder Abbruch bestehen. Unklare Versuche vor einem erneuten Lauf prüfen. Ersetzen und Entfernen haben keinen Quarantäne- oder Wiederherstellungsweg.</p>` +
    group(completed, "Bestätigte Einzelwirkungen") + group(uncertain, "Ungeklärte Einzelversuche") + `</div>`;
}
