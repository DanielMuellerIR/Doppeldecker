import { hideModal, showOwnedModal } from "./modal.js";
import { invoke } from "./tauri-api.js";
import { $, escapeHtml, fmtTransferBytes } from "./ui-utils.js";

export function confirmLocalSync(preview) {
  return new Promise(resolve => {
    showOwnedModal("local-sync", token => {
      const modal = $("#modal .modal");
      modal.classList.add("local-sync-modal");
      $("#modal-title").textContent = "Lokalen Sync ausführen";
      const impact = preview.impact;
      const execution = preview.local_execution;
      const bestEffort = execution && (execution.new_file_commit === "best_effort" || !execution.file_flush || !execution.directory_flush);
      const irreversible = execution?.irreversible_replacements || 0;
      $("#modal-title").textContent = bestEffort ? "Lokalen Sync als Best Effort ausführen" : "Lokalen Sync ausführen";
      const changesExisting = impact.replacements > 0 || impact.metadata_changes > 0;
      const root = (role, identity, listing) => `<div class="real-card"><strong>${role}</strong>` +
        `<code>${escapeHtml(identity.identity.display_path)}</code>` +
        `<small>${listing.entries_seen} Einträge vollständig · ${escapeHtml(identity.identity.mount?.fs_type || "lokales Dateisystem")}</small></div>`;
      const changes = preview.plan.actions.filter(action => ["UpdateFile", "SetMode", "SetDirectoryMetadata"].includes(action.type)
        || (action.type === "CreateHardlink" && action.target_size != null));
      const content = `<p>Dieser Lauf verändert Dateien. Die gebundenen Wurzeln und der Plan werden unmittelbar vor dem Start erneut geprüft.</p>` +
        `<p><strong>${bestEffort ? "Best-Effort-Ausführung" : "Abgesicherte Ausführung"}</strong></p>` +
        (irreversible ? `<p><strong>${irreversible} vorhandene Dateiversion(en) werden dauerhaft ersetzt. Es gibt keine Doppeldecker-Quarantäne und keine automatische Wiederherstellung. Auch eine gleichzeitige Änderung im kurzen Commitfenster kann verloren gehen.</strong></p>` : "") +
        (execution?.new_file_commit === "best_effort" ? `<p>Dieses Ziel bietet kein atomar kollisionsfreies Rename. Der vollständige Elternbestand wird direkt vor dem Commit geprüft und der Inhalt danach zurückgelesen. Ein gleichzeitig neu angelegtes Ziel kann im kurzen Fenster dazwischen trotzdem ersetzt werden. Bei unklarem Ausgang wird nicht automatisch wiederholt.</p>` : "") +
        (execution && !execution.file_flush ? `<p>Das Ziel garantiert keinen dauerhaften Datei-Flush. Die Datei wird zurückgelesen; bei Stromausfall können bereits bestätigte Inhalte verloren gehen.</p>` : "") +
        (execution && !execution.directory_flush ? `<p>Das Ziel garantiert keinen dauerhaften Verzeichnis-Flush. Bei Stromausfall können bereits sichtbare neue Namen oder Ersetzungen verloren gehen.</p>` : "") +
        `<div class="real-grid">${root("Quelle", preview.source_identity, preview.source_listing)}${root("Ziel", preview.target_identity, preview.target_listing)}</div>` +
        `<div class="real-card"><strong>${preview.options.mode === "AddOnly" ? "Fehlendes hinzufügen" : "Hinzufügen und aktualisieren"}</strong>` +
        `<small>${impact.new_files} neue Dateien (${fmtTransferBytes(impact.new_bytes)}) · ${impact.replacements} Ersetzungen · ${impact.metadata_changes} Metadatenänderungen. Keine Mirror-Entfernung.</small></div>` +
        (!irreversible && impact.replacements > 0 ? `<p>Die alten Dateiversionen werden vor dem Ersetzen in der Doppeldecker-Quarantäne gesichert.</p>` : "") +
        (impact.metadata_changes > 0 ? `<p>Bestehende Metadaten werden wie in der Vorschau angepasst.</p>` : "") +
        (changes.length ? `<ul>${changes.map(action => `<li><code>${escapeHtml(action.path)}</code></li>`).join("")}</ul>` : "") +
        `<p>Platz: ${fmtTransferBytes(impact.required_available_bytes)} benötigt · ${fmtTransferBytes(impact.available_bytes)} verfügbar. ${preview.restore_probe_ok ? "Restore-Probe bestanden." : irreversible ? "Kein Wiederherstellungsweg für die Ersetzungen." : "Keine Dateiersetzung: keine Restore-Probe erforderlich."}</p>` +
        (preview.plan.skip_count ? `<p>${preview.plan.skip_count} Einträge werden ausgelassen; die Gründe stehen in der Vorschau.</p>` : "") +
        ((preview.plan.metadata_report?.issues || []).length ? `<p>${preview.plan.metadata_report.issues.length} nicht darstellbare Metadatenmerkmale werden gemäß dem gewählten Profil ausgelassen und im Ergebnis bilanziert.</p>` : "");
      $("#modal-body").innerHTML = `<section class="local-sync-content" tabindex="0" aria-label="Wirkung und gebundene Wurzeln">${content}</section>`;
      $("#modal-body").onclick = null;
      const buttons = $("#modal .modal-buttons");
      buttons.replaceChildren();
      const cancel = document.createElement("button");
      cancel.className = "btn ghost";
      cancel.textContent = "Abbrechen";
      const start = document.createElement("button");
      start.className = changesExisting ? "btn danger" : "btn primary";
      start.textContent = irreversible ? "Dauerhaftes Ersetzen bestätigen"
        : impact.replacements > 0 ? "Ersetzungen bestätigen und starten"
        : changesExisting ? "Metadatenänderungen bestätigen" : bestEffort ? "Best Effort bestätigen und hinzufügen" : "Dateien hinzufügen";
      buttons.append(cancel, start);
      let finished = false;
      const finish = value => {
        if (finished || !hideModal(token)) return;
        finished = true;
        modal.classList.remove("local-sync-modal");
        resolve(value);
      };
      cancel.addEventListener("click", () => finish(false));
      start.addEventListener("click", () => finish(true));
      return { focusTarget: cancel, onEscape: () => finish(false) };
    });
  });
}

export function createLocalSyncUi({ currentSourceTarget, syncOptions, ensureContentHash,
  showPlan, showDock, showError, dockRevision }) {
  let tail = Promise.resolve();
  const serial = action => {
    const result = tail.then(action);
    tail = result.catch(() => {});
    return result;
  };
  const discard = () => serial(() => invoke("discard_local_sync"));

  function run(current) {
    return serial(async () => {
      let transferred = false;
      try {
        await invoke("discard_local_sync");
        if (!current()) return;
        if (syncOptions().mode === "Update") ensureContentHash();
        const { source, target } = currentSourceTarget();
        showDock('<span class="pill copy">Sync-Prüfung</span>Roots, vollständige Listings, Inhalt, Platz und Wiederherstellung werden geprüft …', {});
        const preview = await invoke("prepare_local_sync", { source, target, options: syncOptions() });
        if (!current()) return;
        showPlan(preview.plan);
        if (!preview.plan.actions.some(action => action.type !== "Skip")) return;
        if (!await confirmLocalSync(preview) || !current()) return;
        showDock('<span class="pill copy">Einreihen</span>Bestätigte lokale Ausführung wird eingereiht.', { closable: true });
        const presentation = dockRevision();
        await invoke("enqueue_local_sync", { preparationDigest: preview.plan_digest, replacementsConfirmed: true });
        transferred = true;
        if (current() && dockRevision() === presentation) {
          showDock('<span class="pill copy">Eingereiht</span>Lokaler Sync wartet auf die echte Ausführung in der Fensterqueue.', { closable: true });
        }
        return true;
      } catch (error) {
        if (current()) showError("Lokaler Sync", error);
      } finally {
        if (!transferred) await invoke("discard_local_sync").catch(() => {});
      }
    });
  }
  return { discard, run };
}
