import { createdDirectoryDaclEffectsHtml, createdDirectoryBeforeChildrenEffectsHtml } from "./created-directory-dacl-effects.js";
import { fileDaclEffectsHtml } from "./file-dacl-effects.js";
import { directoryDaclEffectsHtml } from "./directory-dacl-effects.js";
import { directoryStreamEffectsHtml } from "./directory-stream-effects.js";
import { hideModal, showOwnedModal } from "./modal.js";
import { metadataClassLabels as metadataLabels } from "./metadata-labels.js";
import { $, escapeHtml, fmtTransferBytes } from "./ui-utils.js";

export function identityDisplay(root) {
  const identity = root?.identity || {};
  if (root?.provider === "Sftp") {
    return `sftp://${identity.server?.host || "?"}:${identity.server?.port || 22}${identity.virtual_root || "/"}`;
  }
  if (root?.provider === "Ftps") {
    const scheme = identity.server?.transport_security === "plaintext" ? "ftp" : "ftps";
    return `${scheme}://${identity.server?.host || "?"}:${identity.server?.port || 21}${identity.virtual_root || "/"}`;
  }
  if (root?.provider === "WebDav") {
    return `${identity.server?.origin || "https://?"}${identity.virtual_root || "/"}`;
  }
  if (root?.provider === "Smb") {
    // Der virtuelle Pfad liegt unter dem Profilroot, nicht direkt unter der Freigabe.
    const path = "/" + [identity.server?.canonical_root || "/", identity.virtual_root || "/"]
      .flatMap(part => part.split("/").filter(Boolean)).join("/");
    const host = identity.server?.host || "?";
    const authority = host.includes(":") && !host.startsWith("[") ? `[${host}]` : host;
    const port = identity.server?.port;
    return `smb://${authority}${port && port !== 445 ? `:${port}` : ""}/${identity.server?.share || "?"}${path}`;
  }
  return identity.display_path || "unbekanntes Ziel";
}


/** Alle Wirkungsangaben stammen aus dem gebundenen Backendplan. */
export function confirmContentSync(preview) {
  return new Promise(resolve => showOwnedModal("content-sync", token => {
    const modal = $("#modal .modal");
    modal.classList.add("local-sync-modal");
    $("#modal-title").textContent = "Dateisync ausführen";
    const plan = preview.plan;
    const metadataIssues = plan.metadata_report?.issues || [];
    const replacements = preview.irreversible_replacements || 0;
    const removals = preview.irreversible_removals || 0;
    const streamEffects = preview.directory_stream_effects || [];
    const daclEffects = preview.directory_dacl_effects || [];
    const fileDaclEffects = preview.file_dacl_effects || [];
    const createdDaclEffects=preview.created_directory_dacl_effects || [];
    const beforeChildrenEffects = preview.created_directory_before_children_effects || [];
    const permanentStreams = streamEffects.some(({ effect }) => ["replace", "remove"].includes(effect.kind));
    const savedReplacements = preview.quarantined_replacements || 0;
    const savedRemovals = preview.quarantined_removals || 0;
    const changes = plan.actions.filter(action => ["UpdateFile", "DeleteFile", "DeleteDir"].includes(action.type));
    const providerLabel = provider => ({ Local: "Lokal", local: "Lokal", Sftp: "SFTP", sftp: "SFTP", Ftps: "FTP/FTPS", ftps: "FTP/FTPS", WebDav: "WebDAV", webdav: "WebDAV", Smb: "SMB", smb: "SMB" })[provider] || provider;
    const actionLabel = action => ({ UpdateFile: "Dateiversion ersetzen", DeleteFile: "Datei entfernen", DeleteDir: "Ordner entfernen" })[action] || action;
    const root = (role, provider, identity, path) => `<div class="real-card"><strong>${role} · ${escapeHtml(providerLabel(provider))}</strong><code>${escapeHtml(identity ? identityDisplay(identity) : path)}</code></div>`;
    $("#modal-body").innerHTML = `<section class="local-sync-content" tabindex="0" aria-label="Sync-Wirkung und Einschränkungen">` +
      `<p>Dieser Lauf verändert Dateien. Wurzeln und Bestand werden vor der Wirkung erneut geprüft.</p>` +
      `<div class="real-grid">${root("Quelle", preview.source_provider, preview.source_identity, preview.source_root)}${root("Ziel", preview.target_provider, preview.target_identity, preview.target_root)}</div>` +
      `<p>${plan.copy_count} neu · ${plan.update_count} ersetzen · ${plan.delete_count} entfernen · ${fmtTransferBytes(plan.bytes_to_transfer || 0)} übertragen.</p>` +
      (savedReplacements || savedRemovals ? `<div class="real-card"><strong>Doppeldecker-Quarantäne</strong><p>${savedReplacements} vorhandene Dateiversion(en) werden vor dem Ersetzen gesichert. ${savedRemovals} zusätzliche Zieleinträge werden wiederherstellbar in die Quarantäne verschoben.</p>${preview.quarantine_run ? `<small>Lauf: <code>${escapeHtml(preview.quarantine_run)}</code></small>` : ""}</div>` : "") +
      (replacements ? `<p class="real-warning"><strong>${replacements} vorhandene Dateiversion(en) werden dauerhaft ersetzt. Keine Doppeldecker-Quarantäne und keine automatische Wiederherstellung.</strong></p>` : "") +
      (removals ? `<p class="real-warning"><strong>${removals} Einträge (${fmtTransferBytes(preview.irreversible_removal_bytes ?? plan.delete_bytes ?? 0)}) werden dauerhaft entfernt. Keine Doppeldecker-Quarantäne und keine automatische Wiederherstellung.</strong></p>` : "") +
      createdDirectoryDaclEffectsHtml(createdDaclEffects) + createdDirectoryBeforeChildrenEffectsHtml(beforeChildrenEffects) + directoryStreamEffectsHtml(streamEffects) + directoryDaclEffectsHtml(daclEffects) + fileDaclEffectsHtml(fileDaclEffects) +
      (preview.blocked_removals ? `<p class="real-warning">${preview.blocked_removals} Entfernung(en) bleiben wegen unvollständigem oder ausgelassenem Bestand gesperrt. Die übrigen ausführbaren Änderungen werden getrennt bilanziert.</p>` : "") +
      (preview.limitations || []).map(text => `<p class="real-warning">${escapeHtml(text)}</p>`).join("") +
      (changes.length ? `<ul>${changes.slice(0, 50).map(action => `<li>${escapeHtml(actionLabel(action.type))}: <code>${escapeHtml(action.path)}</code></li>`).join("")}</ul>` : "") +
      (changes.length > 50 ? `<p>${changes.length - 50} weitere Änderungen stehen in der bestehenden Planliste.</p>` : "") +
      (metadataIssues.length ? `<div class="real-card"><strong>Ausgelassene Metadaten</strong><ul>${metadataIssues.slice(0, 50).map(issue => `<li><code>${escapeHtml(issue.path)}</code>: ${escapeHtml(metadataLabels[issue.class] || "Metadatenmerkmal")} · ${escapeHtml(issue.reason)}</li>`).join("")}</ul></div>` : "") +
      (metadataIssues.length > 50 ? `<p>${metadataIssues.length - 50} weitere Metadatenhinweise stehen in der bestehenden Planliste.</p>` : "") +
      (plan.skip_count ? `<p>${plan.skip_count} Einträge werden ausgelassen; die Gründe stehen in der Vorschau.</p>` : "") + `</section>`;
    let phraseInput = null;
    if (preview.removal_confirmation) {
      const label = document.createElement("label");
      label.textContent = "Große dauerhafte Entfernung bestätigen: ";
      const phrase = document.createElement("code"); phrase.textContent = preview.removal_confirmation;
      phraseInput = document.createElement("input"); phraseInput.type = "text";
      phraseInput.autocomplete = "off"; phraseInput.spellcheck = false;
      phraseInput.setAttribute("aria-label", "Exakten Entfernungstext eingeben");
      label.append(phrase, phraseInput); $("#modal-body section").append(label);
    }
    let objectScopeInput = null;
    if (fileDaclEffects.length) {
      const label = document.createElement("label");
      objectScopeInput = document.createElement("input"); objectScopeInput.type = "checkbox";
      label.append(objectScopeInput, document.createTextNode("Ich bestätige die dauerhafte Rechteänderung für alle bestehenden und neu angelegten Namen dieses Dateiobjekts, auch außerhalb der ausgewählten Ordner. Die Liste zeigt nur die hier bekannten Namen."));
      $("#modal-body section").append(label);
    }
    $("#modal-body").onclick = null;
    const buttons = $("#modal .modal-buttons"); buttons.replaceChildren();
    const cancel = document.createElement("button"); cancel.className = "btn ghost"; cancel.textContent = "Abbrechen";
    const start = document.createElement("button"); start.className = replacements || removals || permanentStreams || daclEffects.length || fileDaclEffects.length || createdDaclEffects.length || beforeChildrenEffects.length || savedReplacements || savedRemovals ? "btn danger" : "btn primary";
    start.textContent = replacements || removals || permanentStreams || daclEffects.length || fileDaclEffects.length || createdDaclEffects.length || beforeChildrenEffects.length ? "Dauerhafte Änderungen bestätigen" : savedReplacements || savedRemovals ? "Änderungen mit Quarantäne bestätigen" : "Sync bestätigen und starten";
    const updateStart = () => { start.disabled = Boolean((phraseInput && phraseInput.value !== preview.removal_confirmation) || (objectScopeInput && !objectScopeInput.checked)); };
    phraseInput?.addEventListener("input", updateStart); objectScopeInput?.addEventListener("change", updateStart); updateStart();
    buttons.append(cancel, start);
    let finished = false;
    const finish = value => { if (finished || !hideModal(token)) return; finished = true; modal.classList.remove("local-sync-modal"); resolve(value); };
    cancel.addEventListener("click", () => finish(false)); start.addEventListener("click", () => finish({ effectsConfirmed: true, removalPhrase: phraseInput?.value ?? null }));
    return { focusTarget: cancel, onEscape: () => finish(false) };
  }));
}

export function contentSyncPlanPresentation(preview) {
  const permanent = preview.irreversible_removals > 0;
  const quarantined = preview.quarantined_removals > 0;
  return { permanentRemovals: permanent === quarantined ? null : permanent };
}
