import { listen } from "./tauri-api.js";
import { $, escapeHtml, fmtTransferBytes, errCode, sourceReadReport } from "./ui-utils.js";

/** Verdrahtet Tauri-Ereignisse mit vorhandenen UI-Aktionen. Plan- und
 * Freigabelogik bleiben vollständig außerhalb dieses Moduls. */
export function createEventController(actions) {
  async function wire() {
    await listen("operation-context", event => {
      actions.bindOperationContext(event.payload);
      actions.onOperationContext?.(event.payload);
    });
    await listen("queue-updated", event => actions.applyQueueSnapshot(event.payload));
    await listen("queue-error", event => {
      $("#queue-count").textContent = "!";
      actions.showError("Queue-Fehler", event.payload);
    });
    await listen("settings-error", event => {
      actions.showError("Einstellungen", event.payload);
    });
    await listen("queue-exit-requested", event => actions.warnBeforeQueueExit(event.payload));
    await listen("bidirectional-ready", event => {
      actions.showDock(`<span class="pill update">Bidirektional geprüft</span>${escapeHtml(event.payload)}`, { closable: true });
      actions.refreshQueue();
    });
    await listen("bidirectional-progress", event => actions.onProgress(event.payload));
    await listen("bidirectional-done", async event => {
      if (!actions.acceptOperationEvent(event.payload)) return;
      actions.setRunning(false);
      const context = actions.endProgress();
      $("#progress").hidden = true;
      actions.showDock(
        `<span class="pill ok">Bidirektional fertig</span>Baseline-Generation ${event.payload.baseline_generation} wurde atomar committed.`,
        { closable: true },
      );
      await actions.reloadPanes(context);
      actions.refreshQueue();
    });
    await listen("bidirectional-rollback-done", async event => {
      if (!actions.acceptOperationEvent(event.payload)) return;
      const context = actions.endProgress();
      $("#progress").hidden = true;
      actions.showDock(
        `<span class="pill ok">Rollback fertig</span>${event.payload.projections_rolled_back} Teilprojektion(en) wurden rückwärts wiederhergestellt.`,
        { closable: true },
      );
      await actions.reloadPanes(context);
    });
    await listen("bidirectional-error", event => {
      if (!actions.acceptOperationEvent(event.payload)) return;
      actions.setRunning(false);
      const context = actions.endProgress();
      $("#progress").hidden = true;
      actions.showError("Bidirektionaler Sync", event.payload);
      actions.refreshPanes(context);
      actions.refreshQueue();
    });
    await listen("sync-progress", event => actions.onProgress(event.payload));
    await listen("sync-done", event => syncDone(event.payload));
    await listen("sync-error", event => {
      if (!actions.acceptOperationEvent(event.payload)) return;
      actions.finishRealRun();
      const context = actions.endProgress();
      $("#progress").hidden = true;
      actions.showError("Fehler", event.payload);
      actions.refreshPanes(context);
      actions.refreshQuarantine();
    });
    await listen("fileop-progress", event => actions.onProgress(event.payload));
    await listen("fileop-done", event => fileOperationDone(event.payload));
    await listen("fileop-error", event => {
      actions.onFileOperationDone?.(event.payload, true);
      if (!actions.acceptOperationEvent(event.payload)) return;
      const context = actions.endProgress();
      $("#progress").hidden = true;
      const unclear = errCode(event.payload) === "outcome_unclear";
      actions.showError(unclear ? "Ausgang unklar" : "Fehler", event.payload, { foldDetails: unclear });
      // Ein Fehler kann nach bereits wirksamen Teilaktionen eintreffen.
      actions.refreshPanes(context);
    });
    await listen("fileop-preparation-started", event => {
      actions.startFileProgress("Dateioperation vorbereiten …");
      actions.showDock(`<span class="pill copy">Abbrechbare Vorbereitung</span>${escapeHtml(event.payload)}`, {});
    });
    await listen("fileop-preparation-done", () => {
      actions.endProgress();
      $("#progress").hidden = true;
    });
    await listen("file-diff-started", event => {
      const limitations = event.payload?.limitations || [];
      if (event.payload?.phase === "handoff") {
        $("#btn-cancel").hidden = true;
        $("#prog-text").textContent = "Vergleichswerkzeug übernimmt …";
        actions.showDock(`<span class="pill copy">Werkzeugübergabe</span>Die Momentaufnahmen sind geprüft. Die Übergabe lässt sich jetzt nicht mehr zurücknehmen. ${limitations.map(escapeHtml).join(" ")}`, {});
        return;
      }
      actions.startFileProgress("Dateivergleich vorbereiten …");
      actions.showDock(`<span class="pill copy">Abbrechbarer Dateivergleich${limitations.length ? " · Best Effort" : ""}</span>Geprüfte lokale Momentaufnahmen werden erstellt. ${limitations.map(escapeHtml).join(" ")}`, {});
    });
    await listen("file-diff-done", event => {
      actions.endProgress();
      $("#progress").hidden = true;
      const limitations = event.payload?.limitations || [];
      actions.showDock(`<span class="pill ok">Dateivergleich${limitations.length ? " · Best Effort" : ""}</span>Geprüfte Momentaufnahmen wurden an das gewählte Werkzeug übergeben. ${limitations.map(escapeHtml).join(" ")}`, { closable: true });
    });
    await listen("file-diff-error", event => {
      actions.endProgress();
      $("#progress").hidden = true;
      if (errCode(event.payload) === "interrupted") {
        actions.showDock('<span class="pill none">Dateivergleich abgebrochen</span>Es wurde kein Vergleichsfenster geöffnet.', { closable: true });
      } else actions.showError("Dateivergleich", event.payload);
    });
    await listen("remote-snapshot-started", event => {
      actions.startFileProgress("Remote-Datei wird geöffnet …");
      const limitations = event.payload?.limitations || [];
      if (limitations.length) actions.showDock(`<span class="pill copy">Remote-Momentaufnahme · Best Effort</span>${limitations.map(escapeHtml).join(" ")}`, {});
    });
    await listen("remote-snapshot-done", event => {
      actions.endProgress();
      $("#progress").hidden = true;
      actions.showDock(`<span class="pill ok">Lokale Momentaufnahme geöffnet</span>${escapeHtml(event.payload.name)} · ${event.payload.bytes} Byte · Änderungen werden nicht zum Server zurückgeschrieben. ${(event.payload.limitations || []).map(escapeHtml).join(" ")}`, { closable: true });
    });
    await listen("remote-snapshot-error", event => {
      actions.endProgress();
      $("#progress").hidden = true;
      if (errCode(event.payload) === "interrupted") {
        actions.showDock('<span class="pill none">Abgebrochen</span>Remote-Datei wurde nicht geöffnet.', { closable: true });
      } else actions.showError("Öffnen fehlgeschlagen", event.payload);
    });
    await listen("directory-size-result", event => actions.onDirectorySizeResult(event.payload));
    await listen("directory-size-done", event => actions.onDirectorySizeDone(event.payload));
    await listen("directory-size-error", event => actions.onDirectorySizeError(event.payload));
    await listen("connections-updated", event => actions.onConnectionsUpdated(event.payload));
    await listen("window-close-blocked", event => {
      actions.showDock(
        `<span class="pill update">Fenster bleibt offen</span>${escapeHtml(event.payload)} Queue abschließen oder offene Einträge entfernen.`,
        { closable: true },
      );
    });
    await listen("menu", event => actions.onMenu(event.payload));
  }

  async function syncDone(summary) {
    if (!actions.acceptOperationEvent(summary)) return;
    actions.finishRealRun();
    actions.onProgress({
      ...summary,
      phase: "done",
      files_done: summary.files_copied,
      files_total: summary.files_copied,
    });
    const context = actions.endProgress();
    let message;
    if (summary.dry_run) {
      message = `<span class="pill none">Simulierter Testlauf</span>${actions.metadataReportHtml(summary.metadata_report)}Simuliert: ${summary.files_copied} kopieren · ${summary.metadata_updated} Metadaten anpassen · ${summary.files_deleted} entfernen · nichts verändert`;
    } else if (summary.cancelled) {
      const execution = summary.local_execution;
      const bestEffort = execution && (execution.new_file_commit === "best_effort" || !execution.file_flush || !execution.directory_flush);
      message = `<span class="pill none">${bestEffort ? "Best-Effort-Sync abgebrochen" : "Abgebrochen"}</span>${summary.files_copied} kopiert · ${summary.metadata_updated} Metadaten angepasst`;
    } else {
      const execution = summary.local_execution;
      const bestEffort = execution && (execution.new_file_commit === "best_effort" || !execution.file_flush || !execution.directory_flush);
      message = `<span class="pill ok">${bestEffort ? "Best-Effort-Sync ausgeführt" : execution ? "Abgesicherter Sync ausgeführt" : "Sync ausgeführt"}</span>${actions.metadataReportHtml(summary.metadata_report)}${summary.files_copied} kopiert · ${summary.metadata_updated} Metadaten angepasst · ${summary.files_deleted} in Quarantäne · ${fmtTransferBytes(summary.bytes_copied)}`;
      if (summary.quarantine) {
        message += ` · Wiederherstellung: <code>${escapeHtml(summary.quarantine)}</code>`;
      }
    }
    if (summary.irreversible_replacements) message += ` · ${summary.irreversible_replacements} dauerhaft ersetzt, ohne Wiederherstellungsweg`;
    if (Array.isArray(summary.temp_recovery_paths) && summary.temp_recovery_paths.length) {
      message += `<div class="quarantine-meta queue-attention">Zwischenstände erhalten; vor Bereinigung prüfen: ${summary.temp_recovery_paths.map(path => `<code>${escapeHtml(path)}</code>`).join(" · ")}</div>`;
    }
    actions.showDock(message, { keepList: true });
    $("#progress").hidden = true;
    if (!summary.dry_run) actions.reloadTargetPane(context);
    await actions.refreshQuarantine();
  }

  function fileOperationDone(summary) {
    actions.onFileOperationDone?.(summary);
    if (!actions.acceptOperationEvent(summary)) return;
    $("#bar-fill").style.width = "100%";
    const context = actions.endProgress();
    $("#progress").hidden = true;
    const parts = [`${summary.files}${summary.source_tree ? " Datei(en)" : ""} übertragen`];
    if (summary.skipped) parts.push(`${summary.skipped} übersprungen`);
    if (summary.symlinks_skipped) parts.push(`${summary.symlinks_skipped} Symlink(s) ausgelassen`);
    const sourceReport = sourceReadReport(summary.source_read_limitations, summary.source_tree);
    if (sourceReport.summary) parts.push(sourceReport.summary);
    const metadataNotes = summary.target_write_limitations || [];
    if (metadataNotes.length) parts.push(`${metadataNotes.length} Einträge ohne native Zielmetadaten`);
    if (summary.source_removal_best_effort) {
      parts.push("Quellentfernung: Best Effort");
    }
    if (summary.target_copy_new_best_effort) {
      parts.push("Neuanlegen am Ziel: Best Effort");
    }
    if (summary.target_replace_best_effort) {
      parts.push("Ersetzen am Ziel: Best Effort");
    }
    const backups = Array.isArray(summary.retained_backup_paths) ? summary.retained_backup_paths : [];
    if (backups.length) parts.push(summary.cancelled ? "Sicherung erhalten" : "Übertragung abgeschlossen; Sicherung erhalten");
    const label = summary.cancelled ? "Abgebrochen" : "Fertig";
    const pill = summary.cancelled ? "none" : backups.length ? "update" : "ok";
    const recoveryPaths = Array.isArray(summary.temp_recovery_paths)
      ? summary.temp_recovery_paths
      : [];
    const remainderPaths = recoveryPaths.filter(path => !backups.includes(path));
    const backupNotice = backups.length ? `<div class="quarantine-meta queue-attention">Sicherung erhalten: ${backups.map(path => `<code>${escapeHtml(path)}</code>`).join(" · ")}</div>` : "";
    const metadataNotice = metadataNotes.length ? `<details class="operation-error-details"><summary>Ausgelassene Metadaten anzeigen</summary><textarea class="listing-omissions-text operation-error-body" aria-label="Ausgelassene Zielmetadaten" readonly>${escapeHtml(metadataNotes.join("\n"))}</textarea></details>` : "";
    const recovery = remainderPaths.length
      ? `<div class="quarantine-meta queue-attention">Mögliche Temp-Recoverypfade – nicht automatisch löschen: ${remainderPaths
        .map(path => `<code>${escapeHtml(path)}</code>`)
        .join(" · ")}</div>`
      : "";
    actions.showDock(
      `<span class="pill ${pill}">${label}</span>${parts.join(" · ")} · ${fmtTransferBytes(summary.bytes)}${backupNotice}${recovery}${sourceReport.details}${metadataNotice}`,
      { closable: true },
    );
    actions.refreshPanes(context);
  }

  return { wire };
}
