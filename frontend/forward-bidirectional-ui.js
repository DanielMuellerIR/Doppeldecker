import { hideModal, showOwnedModal } from "./modal.js";
import { metadataClassLabels as metadataLabels } from "./metadata-labels.js";
import { identityDisplay } from "./content-sync-ui.js";
import { $, escapeHtml, fmtTransferBytes } from "./ui-utils.js";

function displayPath(path) {
  if (typeof path === "string") return path;
  if (!Array.isArray(path)) return "Nicht darstellbarer Pfad";
  return path.map(segment => {
    try { return new TextDecoder("utf-8", { fatal: true }).decode(Uint8Array.from(segment)); }
    catch (_) { return "[Name nicht als UTF-8 darstellbar]"; }
  }).join("/");
}

export function confirmForwardBidirectional(preview, { pairName = "", left, right, visibleRoles = { left: "left", right: "right" }, queued = false } = {}) {
  return new Promise(resolve => showOwnedModal("forward-bidirectional", token => {
    const modal = $("#modal .modal");
    modal.classList.add("local-sync-modal");
    $("#modal-title").textContent = preview.recovery ? "Neu abgeglichenen Sync bestätigen" : "Bidirektionalen Sync als Best Effort ausführen";
    const effects = preview.effects || [];
    const actionLabels = { copy: "Übertragen", type_swap: "Typ ersetzen", delete: "Entfernen", conflict_copy: "Konfliktkopie", converged: "Bereits gleich", tombstone: "Löschmarker", conflict: "Konflikt", skipped_conflict: "Konflikt ausgelassen" };
    const sideLabel = role => {
      const side = visibleRoles.left === role ? "left" : "right";
      return queued ? (side === "left" ? "Paarseite links" : "Paarseite rechts") : (side === "left" ? "Links" : "Rechts");
    };
    const pathHtml = (path, side) => `${side ? `${sideLabel(side)}: ` : ""}<code>${escapeHtml(displayPath(path))}</code>`;
    const actionHtml = action => {
      const paths = action.type === "conflict_copy"
        ? `${pathHtml(action.source_path, action.source_side)} → ${pathHtml(action.target_path, action.target_side)}`
        : `${pathHtml(action.path)}${action.direction ? ` · ${sideLabel(action.direction === "left_to_right" ? "left" : "right")} → ${sideLabel(action.direction === "left_to_right" ? "right" : "left")}` : ""}`;
      const subtree = action.subtree || [];
      return `<li>${escapeHtml(actionLabels[action.type] || "Änderung")}: ${paths}` +
        (subtree.length ? `<ul>${subtree.map(entry => `<li>${pathHtml(entry.source_path)} → ${pathHtml(entry.target_path)}</li>`).join("")}</ul>` : "") + `</li>`;
    };
    // Nur die vom Backend gelieferten Wirkungszahlen summieren; keine Wirkung aus Aktionen ableiten.
    const totals = effects.reduce((sum, effect) => {
      for (const field of Object.keys(sum)) sum[field] += effect[field] || 0;
      return sum;
    }, { irreversible_replacements: 0, irreversible_removals: 0, quarantined_replacements: 0, quarantined_removals: 0, blocked_removals: 0 });
    const planTotals = effects.reduce((sum, effect) => {
      for (const field of Object.keys(sum)) sum[field] += effect.plan[field] || 0;
      return sum;
    }, { copy_count: 0, update_count: 0, delete_count: 0, bytes_to_transfer: 0 });
    const metadataCount = effects.reduce((count, effect) => count + (effect.plan.metadata_report?.issues?.length || 0), 0);
    const actions = preview.plan.actions || [];
    const rootDisplay = (side, endpoint) => {
      const identity = preview.plan[`${visibleRoles[side]}_root_identity`];
      return identity ? identityDisplay(identity) : endpoint?.root || "";
    };
    const irreversibleText = effect => [
      effect.irreversible_replacements > 0 ? `${effect.irreversible_replacements} Dateiversion(en) werden dauerhaft ersetzt` : "",
      effect.irreversible_removals > 0 ? `${effect.irreversible_removals} Einträge werden dauerhaft entfernt` : "",
    ].filter(Boolean).join(" · ");
    const quarantineText = effect => `${effect.quarantined_replacements || 0} alte Dateiversion(en) und ${effect.quarantined_removals || 0} zusätzliche Einträge werden in der lokalen Quarantäne gesichert.`;
    const metadataHtml = effect => {
      const issues = effect.plan.metadata_report?.issues || [];
      return issues.length ? `<strong>Metadatenhinweise (${issues.length})</strong><ul>${issues.map(issue => `<li><code>${escapeHtml(displayPath(issue.path))}</code>: ${escapeHtml(metadataLabels[issue.class] || issue.class)} · ${escapeHtml(issue.reason)}</li>`).join("")}</ul>` : "";
    };
    const effectHtml = effect => `<div class="real-card"><strong>Ziel: ${escapeHtml(effect.target_identity ? identityDisplay(effect.target_identity) : effect.target_root)}</strong>` +
      `<p>${effect.plan.copy_count} hinzufügen · ${effect.plan.update_count} ersetzen · ${effect.plan.delete_count} Entfernungsaktionen · ${fmtTransferBytes(effect.plan.bytes_to_transfer || 0)}</p>` +
      (effect.quarantined_replacements || effect.quarantined_removals ? `<p>${quarantineText(effect)}${effect.quarantine_run ? ` Lauf: <code>${escapeHtml(effect.quarantine_run)}</code>` : ""}</p>` : "") +
      (effect.irreversible_replacements || effect.irreversible_removals ? `<p class="real-warning"><strong>${irreversibleText(effect)}. Dafür gibt es keine Doppeldecker-Quarantäne und keine automatische Wiederherstellung.</strong></p>` : "") +
      (effect.blocked_removals ? `<p>${effect.blocked_removals} Entfernungen bleiben gesperrt.</p>` : "") +
      (effect.limitations || []).map(text => `<p>${escapeHtml(text)}</p>`).join("") + metadataHtml(effect) + `</div>`;
    $("#modal-body").innerHTML = `<section class="local-sync-content" tabindex="0" aria-label="Wirkung beider Seiten und Wiederherstellung">` +
      `<p><strong>${escapeHtml(pairName)}</strong></p><div class="real-grid"><div class="real-card"><strong>${queued ? "Paarseite links" : "Links"}</strong><code>${escapeHtml(rootDisplay("left", left))}</code></div><div class="real-card"><strong>${queued ? "Paarseite rechts" : "Rechts"}</strong><code>${escapeHtml(rootDisplay("right", right))}</code></div></div>` +
      `<p>Dieser bidirektionale Sync führt die unten gezeigten Änderungen als Best Effort aus. Frische Bestandsprüfungen und Ergebnisprüfungen begrenzen die Wirkung. Es gibt kein universelles automatisches Rollback; bereits ausgeführte Änderungen können bei einem Fehler erhalten bleiben. Verfügbare lokale Quarantäne ist unten ausdrücklich ausgewiesen.</p>` +
      (preview.recovery ? `<p class="real-warning">${escapeHtml(preview.recovery.warning || "Der vorherige Lauf wurde unterbrochen.")} Neuer Abgleich gegen die letzte gemeinsame Baseline; alte Konfliktentscheidungen wurden verworfen. Dieser neue Plan muss vollständig bestätigt werden.</p>` : "") +
      (preview.scope_warnings || []).map(text => `<p class="real-warning">${escapeHtml(text)}</p>`).join("") +
      (preview.limitations || []).map(text => `<p>${escapeHtml(text)}</p>`).join("") +
      `<div class="real-card"><strong>Gesamtwirkung aller ${effects.length} Teilwirkungen</strong>` +
      `<p>${planTotals.copy_count} hinzufügen · ${planTotals.update_count} ersetzen · ${planTotals.delete_count} Entfernungsaktionen · ${fmtTransferBytes(planTotals.bytes_to_transfer)} übertragen.</p>` +
      `<p>${totals.quarantined_replacements || 0} alte Dateiversion(en) und ${totals.quarantined_removals || 0} zusätzliche Einträge werden in der lokalen Quarantäne gesichert.</p>` +
      `<p>${totals.blocked_removals} Entfernungen bleiben gesperrt. ${metadataCount} Metadatenhinweise; Pfade, Merkmale und Gründe stehen bei den Teilwirkungen.</p></div>` +
      (totals.irreversible_replacements || totals.irreversible_removals ? `<p class="real-warning"><strong>${irreversibleText(totals)}. Dafür gibt es keine Doppeldecker-Quarantäne und keine automatische Wiederherstellung.</strong></p>` : `<p>Keine dauerhaften Ersetzungen oder Entfernungen.</p>`) +
      (effects.length ? `<details class="forward-sync-details"${effects.length <= 20 ? " open" : ""}><summary>Alle ${effects.length} Teilwirkungen</summary>${effects.map(effectHtml).join("")}</details>` : "") +
      (actions.length ? `<details class="forward-sync-details"${actions.length <= 50 ? " open" : ""}><summary>Alle ${actions.length} Planaktionen und Pfade</summary><ul>${actions.map(actionHtml).join("")}</ul></details>` : "") + `</section>`;
    $("#modal-body").onclick = null;
    const phrases = [...new Set(effects.map(effect => effect.removal_confirmation).filter(Boolean))];
    const inputs = phrases.map(phrase => {
      const label = document.createElement("label"); label.textContent = "Große dauerhafte Entfernung bestätigen: ";
      const code = document.createElement("code"); code.textContent = phrase;
      const input = document.createElement("input"); input.type = "text"; input.autocomplete = "off"; input.spellcheck = false;
      input.setAttribute("aria-label", `Exakten Entfernungstext eingeben: ${phrase}`);
      label.append(code, input); $("#modal-body section").append(label);
      return { input, phrase };
    });
    const buttons = $("#modal .modal-buttons"); buttons.replaceChildren();
    const cancel = document.createElement("button"); cancel.className = "btn ghost"; cancel.textContent = "Abbrechen";
    const start = document.createElement("button"); start.className = "btn danger"; start.textContent = "Wirkung beider Seiten bestätigen";
    const update = () => { start.disabled = inputs.some(({ input, phrase }) => input.value !== phrase); };
    for (const { input } of inputs) input.addEventListener("input", update);
    update(); buttons.append(cancel, start);
    let finished = false;
    const finish = value => { if (finished || !hideModal(token)) return; finished = true; modal.classList.remove("local-sync-modal"); resolve(value); };
    cancel.addEventListener("click", () => finish(null));
    start.addEventListener("click", () => finish({ effectsConfirmed: true, preparationDigest: preview.digest, removalPhrases: inputs.map(({ input }) => input.value) }));
    return { focusTarget: cancel, onEscape: () => finish(null) };
  }));
}
