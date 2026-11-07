import { createdDirectoryDaclReportHtml, createdDirectoryBeforeChildrenPendingHtml } from "./created-directory-dacl-effects.js";
import { fileDaclReportHtml } from "./file-dacl-effects.js";
import { directoryDaclReportHtml } from "./directory-dacl-effects.js";
import { directoryStreamReportHtml } from "./directory-stream-effects.js";
import { archiveExtractSourceLabel, archiveExtractFailureMessage, archiveExtractScope, archiveExtractSummary, confirmArchiveExtractPreview } from "./archive-extract-ui.js";
import { archivePackSummary, confirmArchivePackPreview, bindArchivePackPages, archiveIssuesHtml } from "./archive-pack-ui.js";
import {
  cancelOwnedModal,
  confirmModal,
  hideModal,
  showLatestOwnedModal,
  showOwnedModal,
} from "./modal.js";
import { invoke } from "./tauri-api.js";
import { confirmContentSync } from "./content-sync-ui.js";
import { confirmLocalSync } from "./local-sync-ui.js";
import { $, escapeHtml, fmtTransferBytes } from "./ui-utils.js";

const STATE_LABELS = {
  Pending: "wartet",
  Validating: "wird geprüft",
  Running: "läuft",
  AwaitingConfirmation: "neue Bestätigung nötig",
  Completed: "fertig",
  Failed: "fehlgeschlagen",
  Cancelled: "abgebrochen",
  Interrupted: "nach Neustart unterbrochen",
};
const LEGACY_UNBOUND_WEBDAV_ETAG = "__doppeldecker_legacy_unbound__";

function renameCycleRecoveryHtml(operation) {
  if (operation.type !== "RenameCycleRecovery") return "";
  const record = operation.record;
  const names = record.old_names.map((name, index) => {
    const current = record.pending?.member === index
      ? `${JSON.stringify(record.pending.from)} oder ${JSON.stringify(record.pending.to)} (Ausgang unklar)`
      : JSON.stringify(record.current_names[index]);
    return `${JSON.stringify(name)} → Ziel ${JSON.stringify(record.new_names[index])}; letzter Stand: ${current}`;
  }).join("\n");
  return `<details class="queue-cycle-details"><summary>Namenslage · ${record.old_names.length} Einträge</summary>
    <p>Letzter bestätigter Stand; nach einem Abbruch vor Ort prüfen. Zwischenname: <code>${escapeHtml(record.temp_name)}</code></p>
    <textarea readonly rows="4" aria-label="Gespeicherte Namenslage des Dateinamenzyklus">${escapeHtml(names)}</textarea></details>`;
}

function legacyWebdavDelete(entry) {
  return entry.operation?.type === "ProviderDelete"
    && entry.operation.expected_plan?.root_identity?.identity?.root_etag
      === LEGACY_UNBOUND_WEBDAV_ETAG;
}

function endpointLabel(endpoint, includeServer = false) {
  if (!endpoint) return "?";
  const provider = endpoint.provider === "Local"
    ? "Lokal"
    : endpoint.provider === "Ftps" && endpoint.connection?.transport_security === "plaintext"
      ? "FTP"
      : ({ Sftp: "SFTP", Ftps: "FTPS", WebDav: "WebDAV", Smb: "SMB" }[endpoint.provider] || endpoint.provider);
  const connection = endpoint.connection;
  const server = includeServer && connection ? connection.origin || `${connection.host || ""}${connection.port ? `:${connection.port}` : ""}${connection.share ? `/${connection.share}` : ""}` : "";
  return `${provider}${server ? ` · ${server}` : ""} · ${endpoint.root}`;
}

function archiveOperation(entry) {
  return entry.operation.type === "ArchiveUpload" ? entry.operation.generation : entry.operation;
}
function operationLabel(entry) {
  const operation = entry.operation;
  if (operation.type === "RenameCycleRecovery") {
    const record = operation.record;
    const server = record.identity.identity?.server;
    const provider = record.identity.provider === "Ftps"
      ? server?.transport_security === "plaintext" ? "FTP" : "FTPS"
      : ({ Sftp: "SFTP", Smb: "SMB", WebDav: "WebDAV" }[record.identity.provider] || record.identity.provider);
    const serverLabel = server?.origin || `${server?.host || ""}${server?.port ? `:${server.port}` : ""}${server?.share ? `/${server.share}` : ""}`;
    return `${record.finished ? "Dateinamenzyklus abgeschlossen" : "Entfernten Namenstausch prüfen"} · ${provider} · ${serverLabel} · ${record.root}`;
  }
  if (operation.type === "ArchiveUpload") {
    return `${operation.generation.kind === "pack" ? "ZIP erstellen" : "ZIP entpacken"} und hochladen → ${endpointLabel(operation.target,true)}`;
  }
  if (entry.generated_source) {
    return `${entry.generated_source.kind === "zip" ? "ZIP erstellt" : "ZIP lokal entpackt"} · Upload ${entry.generated_source.label} → ${endpointLabel(operation.target,true)}`;
  }
  if (operation.type === "ArchiveExtract") {
    const preview = (entry.attention?.type === "ArchiveExtractChanged" ? entry.attention.current : operation.expected).preview;
    return `ZIP entpacken · ${archiveExtractScope(preview)} · ${archiveExtractSourceLabel(preview)} → ${operation.request.target_dir}${preview.relative_root ? ` · Zielname: ${preview.relative_root}` : ""}`;
  }
  if (operation.type === "ArchivePack") {
    return `ZIP erstellen · ${operation.request.options.compression === "stored" ? "Stored" : "Deflate"} · ${operation.expected.preview.target}`;
  }
  if (operation.type === "FtpsRecovery") {
    const identity = operation.identity;
    const provider = identity.server.transport_security === "plaintext" ? "FTP" : "FTPS";
    return `${provider}-Vorbereitung prüfen · ${identity.server.host}:${identity.server.port} · ${identity.virtual_root}`;
  }
  if (operation.type === "File" || operation.type === "ProviderFile") {
    const verb = operation.kind === "Copy" ? "Kopieren" : "Verschieben";
    const route = operation.type === "ProviderFile"
      ? `${endpointLabel(operation.source)} → ${endpointLabel(operation.target)}`
      : operation.destination;
    const readClass = operation.type === "ProviderFile" && operation.source?.provider === "Sftp"
      ? " · SFTP-Inhalte: Best Effort, additiv" : "";
    return `${verb}${readClass} · ${operation.items.length} Eintrag/Einträge → ${route}`;
  }
  if (operation.type === "ProviderDelete") {
    // `RemoteDeleteMode` serialisiert snake_case ("quarantine"/"permanent").
    const mode = operation.mode === "quarantine"
      ? "Doppeldecker-Quarantäne"
      : "Dauerhaft löschen";
    return `${mode} · ${operation.expected_plan.items.length} Auswahl(en) · ` +
      `${operation.expected_plan.entries} Objekte · ${endpointLabel(operation.endpoint)}`;
  }
  if (["Bidirectional", "BidirectionalForward"].includes(operation.type)) {
    return `Bidirektional${operation.type === "BidirectionalForward" ? " · Best Effort" : ""} · ${operation.pair_name} · ${endpointLabel(operation.left)} ↔ ${endpointLabel(operation.right)}`;
  }
  if (["ProviderSync", "ContentSync"].includes(operation.type)) {
    return `${operation.type === "ContentSync" ? "Dateisync" : "Provider"} ${operation.options.mode} · ${endpointLabel(operation.source)} → ${endpointLabel(operation.target)}`;
  }
  const plan = operation.expected_plan;
  let provider = {
    SftpSync: "SFTP",
    FtpsSync: "FTPS",
    WebDavSync: "WebDAV",
    SmbSync: "SMB",
  }[operation.type];
  if (operation.type === "FtpsSync" && operation.connection?.transport_security === "plaintext") {
    provider = "FTP";
  }
  if (provider) {
    const arrow = operation.direction === "Upload"
      ? `${operation.local_root} → ${provider} ${operation.remote_root}`
      : `${provider} ${operation.remote_root} → ${operation.local_root}`;
    return `${provider} ${operation.options.mode} · ${plan.copy_count} neu · ${plan.update_count} ersetzen · ${plan.delete_count} Quarantäne · ${arrow}`;
  }
  const profile = operation.execution_hint;
  const bestEffort = profile && (profile.new_file_commit === "best_effort" || !profile.file_flush || !profile.directory_flush);
  const execution = operation.type === "LocalSync" ? `Lokaler Sync · ${profile ? bestEffort ? "Best Effort" : "abgesichert" : "erneut prüfen"}` : operation.sandbox ? "Test-Sync" : "Simulierter Testlauf";
  return `${execution} ${operation.options.mode} · ${plan.copy_count} neu · ${plan.update_count} ersetzen · ${plan.delete_count} Quarantäne → ${operation.target}`;
}

/** Verwaltet ausschließlich Darstellung und Bedienung des persistenten Queue-
 * Journals. Ausführung, Driftprüfung und Freigaben bleiben vollständig im Backend. */
export function createQueueUi({
  queueErrorActive = () => false,
  realRunActive,
  recoverBidirectional,
  showDock,
  showError,
  startFileProgress,
  startSyncProgress,
  resumeGeneratedUpload,
  showGeneratedSource,
}) {
  let snapshot = { revision: 0, paused: false, stop_on_error: false, entries: [] };
  let activeOperationId = null;
  let exitGeneration = 0;
  let exitModalToken = null;

  function openCount(current = snapshot) {
    return current.entries.filter(entry => Boolean(entry.bidirectional_completion_recovery)
      || !["Completed", "Cancelled"].includes(entry.state)).length;
  }

  function applySnapshot(next) {
    const currentRevision = Number.isSafeInteger(snapshot.revision) ? snapshot.revision : 0;
    const nextRevision = Number.isSafeInteger(next?.revision) ? next.revision : 0;
    if (nextRevision < currentRevision) {
      // Command-Antworten und Events laufen über getrennte IPC-Wege. Ein bereits
      // sichtbarer terminaler Eventstand darf nie durch die ältere Startantwort
      // desselben Workers zurückgesetzt werden.
      return false;
    }
    snapshot = next;
    const count = openCount(next);
    // Ein beim Start gefundenes beschädigtes Journal ist ein eigener Befund. Ein
    // späterer normaler Queue-Snapshot darf sein sichtbares Fehlerzeichen nicht
    // wieder durch eine Eintragszahl ersetzen.
    $("#queue-count").textContent = queueErrorActive() ? "!" : String(count);
    $("#btn-queue").title = `${count} offene/zu prüfende Operation(en) · ${next.paused ? "pausiert" : "aktiv"}`;

    const active = next.entries.find(entry => entry.state === "Running"
      || entry.state === "Validating" && ["ArchivePack", "ArchiveExtract", "ArchiveUpload"].includes(entry.operation.type));
    const activeRun = active ? `${active.operation_id}/${active.attempts || 0}/${active.operation.type}` : null;
    if (active && activeRun !== activeOperationId && !realRunActive()) {
      activeOperationId = activeRun;
      const context = { operationId: active.operation_id, attempt: active.attempts || 0 };
      // Alle einseitigen Sync-Provider verwenden dieselbe Plan-/Fortschrittsanzeige.
      // SMB fehlte früher in dieser Matrix und erschien dadurch fälschlich als Move.
      if (["Sync", "LocalSync", "SftpSync", "FtpsSync", "WebDavSync", "SmbSync", "ProviderSync", "ContentSync"].includes(active.operation.type)) {
        startSyncProgress(active.operation.expected_plan, { ...context, localExecution: active.operation.execution_hint, bestEffort: active.operation.type === "ContentSync", simulated: active.operation.type === "Sync" && !active.operation.sandbox });
      } else if (["Bidirectional", "BidirectionalForward"].includes(active.operation.type)) {
        // Der bidirektionale Eintrag besitzt zwei Teilpläne statt eines
        // `expected_plan`. Bis zum ersten Progress-Event zeigen wir deshalb einen
        // neutralen Starttext und niemals fälschlich „Verschieben“.
        startFileProgress(active.operation.type === "BidirectionalForward" ? "Bidirektionaler Sync als Best Effort aus Queue …" : "Bidirektionaler Sync aus Queue …", context);
      } else {
        if (active.operation.type === "ArchiveUpload") {
          startFileProgress(active.operation.generation.kind === "pack" ? "Erstelle ZIP vor dem Upload …" : "Entpacke ZIP vor dem Upload …", context);
        } else if (active.operation.type === "ArchiveExtract") {
          startFileProgress(active.state === "Validating" ? "Prüfe ZIP und Entpackziel …" : "Entpacke ZIP aus Queue …", context);
        } else if (active.operation.type === "ArchivePack") {
          startFileProgress(active.state === "Validating" ? "Prüfe ZIP-Quellen aus Queue …" : "Erstelle ZIP-Archiv aus Queue …", context);
        } else if (active.operation.type === "ProviderDelete") {
          // `RemoteDeleteMode` serialisiert snake_case ("quarantine"/"permanent").
          startFileProgress(active.operation.mode === "quarantine"
            ? "Verschiebe in Doppeldecker-Quarantäne …"
            : "Lösche Remote-Auswahl dauerhaft …", context);
        } else {
          startFileProgress(active.operation.kind === "Copy"
            ? active.operation.source?.provider === "Sftp" ? "SFTP-Inhalte herunterladen · Best Effort …" : "Kopiere aus Queue …"
            : "Verschiebe aus Queue …", context);
        }
      }
    } else if (!active && activeOperationId) {
      activeOperationId = null;
    }
    if (!$("#queue-popover").hidden) renderPopover();
  }

  async function refresh() {
    try {
      applySnapshot(await invoke("list_operation_queue"));
    } catch (error) {
      $("#queue-count").textContent = "!";
      throw error;
    }
  }

  async function action(command, args = {}) {
    try {
      applySnapshot(await invoke(command, args));
      return true;
    } catch (error) {
      showError("Queue-Fehler", error);
      return false;
    }
  }

  function entryHtml(entry) {
    const progress = entry.progress || {};
    const detail = progress.bytes_total
      ? ` · ${fmtTransferBytes(progress.bytes_done || 0)}/${fmtTransferBytes(progress.bytes_total)}`
      : "";
    const attention = entry.attention
      ? `<div class="quarantine-meta queue-attention">${escapeHtml(entry.error || "Neue Bestätigung nötig")}</div>`
      : entry.error
        ? entry.operation.type === "ArchiveExtract"
          ? `<div class="quarantine-meta queue-attention">${escapeHtml(archiveExtractFailureMessage(entry.error_code))}</div><details class="operation-error-details"><summary>Vollständiger Entpackfehler</summary><div class="operation-error-body" tabindex="0">${escapeHtml(entry.error)}</div></details>`
          : `<div class="quarantine-meta queue-attention">${escapeHtml(entry.error)}</div>`
        : "";
    const sourceTree = entry.attention?.type === "SourceTreeChanged"
      ? entry.attention.current_tree : entry.operation.source_tree_expected;
    const sourceTreeDetail = sourceTree
      ? `<div class="quarantine-meta">${entry.attention?.type === "SourceTreeChanged" ? "Neuer" : "Bestätigter"} Quellbaumumfang: ${sourceTree.files} Datei(en), ${sourceTree.directories} Ordner, ${fmtTransferBytes(sourceTree.bytes)}; ${sourceTree.omitted.length} Eintrag/Teilbaum ausgelassen</div>`
      : "";
    const retainedBackups = entry.retained_backup_paths || [];
    const backupLabel = entry.state === "Completed" ? "Übertragung abgeschlossen; Sicherung erhalten" : "Sicherung erhalten";
    const backupRecovery = retainedBackups.map(path => `<div class="quarantine-meta queue-attention">${backupLabel}: <code>${escapeHtml(path)}</code></div>`).join("");
    const tempRecovery = backupRecovery + (entry.temp_recovery_paths || [])
      .filter(path => !retainedBackups.includes(path))
      .map(path => `<div class="quarantine-meta queue-attention">${(entry.directory_dacl_recovery || entry.file_dacl_recovery || entry.created_directory_dacl_recovery) ? "Prüfpfad der Rechteänderung, nicht automatisch löschen:" : "Temp-Recoverypfad prüfen, nicht automatisch löschen:"} <code>${escapeHtml(path)}</code></div>`)
      .join("");
    const createdDaclRecovery=entry.created_directory_dacl_recovery === true && ["Failed", "Interrupted"].includes(entry.state);
    const fileDaclRecovery = entry.file_dacl_recovery === true && ["Failed", "Interrupted"].includes(entry.state);
    const daclRecovery = entry.directory_dacl_recovery === true && ["Failed", "Interrupted"].includes(entry.state);
    const hasDaclEffects = [entry.directory_dacl_report, progress.directory_dacl_report, entry.file_dacl_report, progress.file_dacl_report,entry.created_directory_dacl_report,progress.created_directory_dacl_report].some(report => report?.completed?.length || report?.uncertain?.length);
    const pending = entry.state === "Pending";
    const legacyDelete = legacyWebdavDelete(entry);
    const completionMarker = entry.bidirectional_completion_recovery;
    const completionRecovery = ["Bidirectional", "BidirectionalForward"].includes(entry.operation.type)
      && (entry.state === "Interrupted"
        || (entry.state === "Completed" && completionMarker === "complete")
        || (entry.state === "Cancelled" && completionMarker === "rolled_back"))
      && Boolean(completionMarker);
    const recovery = !completionRecovery
      && ["Bidirectional", "BidirectionalForward"].includes(entry.operation.type)
      && entry.recovery_available === true
      && ["Failed", "Interrupted"].includes(entry.state);
    const retry = !createdDaclRecovery && !daclRecovery && !fileDaclRecovery && !hasDaclEffects && !completionRecovery
      && !recovery
      && !legacyDelete
      && entry.operation.type !== "ArchiveExtract"
      && entry.operation.type !== "ArchiveUpload"
      && entry.operation.type !== "RenameCycleRecovery"
      && entry.error_code !== "outcome_unclear"
      && entry.error_code !== "replan_required"
      && ["Failed", "Cancelled", "Interrupted"].includes(entry.state);
    const replanRequired = !createdDaclRecovery && !daclRecovery && !fileDaclRecovery && !completionRecovery
      && !recovery
      && entry.error_code === "replan_required"
      && entry.state === "Failed";
    const unclearOutcome = !createdDaclRecovery && !daclRecovery && !fileDaclRecovery && !completionRecovery
      && !recovery
      && !legacyDelete
      && entry.operation.type !== "ArchiveExtract"
      && entry.operation.type !== "ArchiveUpload"
      && entry.error_code === "outcome_unclear"
      && ["Failed", "Interrupted"].includes(entry.state);
    const approve = entry.state === "AwaitingConfirmation";
    // Eine wartende Remote-Vorbereitung hält Permit und Verbindung im Backend und
    // wird über ihren eigenen Weg abgebaut; alle anderen Rückfragen darf der
    // Nutzer hier ablehnen, ohne die Lage zu bestätigen, die er ablehnen will.
    const discard = approve && (entry.attention?.type !== "RemoteRun" || ["LocalSync", "BidirectionalForward"].includes(entry.operation.type));
    const label = operationLabel(entry);
    const incompleteNotice = entry.file_incomplete ? `<div class="quarantine-meta queue-attention">Dateiinhalt nicht vollständig übertragen${entry.operation.kind === "Move" ? "; betroffene Quellen erhalten" : ""}.</div>` : "";
    const archive = archiveOperation(entry);
    const generated = Boolean(entry.generated_source);
    const parent = entry.operation.type === "ArchiveUpload";
    const generatedConflict = generated && approve && entry.attention?.type === "ConflictsChanged" && Boolean(resumeGeneratedUpload);
    const cycleRecovery = entry.operation.type === "RenameCycleRecovery";
    return `<div class="quarantine-row queue-row${["ArchivePack", "ArchiveExtract", "ArchiveUpload"].includes(entry.operation.type) || generated ? " queue-archive-row" : ""}${cycleRecovery ? " queue-cycle-row" : ""}" data-queue-id="${escapeHtml(entry.operation_id)}">
      <div class="quarantine-main">
        <div class="quarantine-name" title="${escapeHtml(label)}">${escapeHtml(label)}</div>
        <div class="quarantine-meta">${entry.operation.type === "RenameCycleRecovery" && entry.state === "Interrupted" ? "manuell prüfen" : entry.file_incomplete && entry.state === "Completed" ? "mit Auslassungen abgeschlossen" : STATE_LABELS[entry.state] || entry.state} · Versuch ${entry.attempts}${detail}</div>
        ${attention}${incompleteNotice}
        ${generated ? `<div class="quarantine-meta">Lokale ${entry.generated_source.kind === "zip" ? "ZIP-Erzeugung" : "Entpackphase"} abgeschlossen · ${entry.generated_source.files} Datei(en), ${entry.generated_source.directories} Ordner, ${fmtTransferBytes(entry.generated_source.bytes)}. ${entry.state === "Completed" ? "Upload abgeschlossen." : "Upload noch nicht vollständig abgeschlossen."}</div>` : ""}
        ${parent ? `<div class="quarantine-meta">Zuerst lokal ${archive.kind === "pack" ? "packen" : "entpacken"}, anschließend denselben Zwischenstand hochladen. Der Upload hat noch nicht begonnen.</div>` : ""}
        ${sourceTreeDetail}
        ${renameCycleRecoveryHtml(entry.operation)}
        ${entry.operation.type === "ArchivePack" || parent && archive.kind === "pack" ? `<div class="quarantine-meta">${escapeHtml(archivePackSummary((entry.attention?.type === "ArchivePackChanged" ? entry.attention.current : archive.expected).preview))}</div>` : ""}
        ${entry.operation.type === "ArchiveExtract" || parent && archive.kind === "extract" ? `<div class="quarantine-meta">${escapeHtml(archiveExtractSummary((entry.attention?.type === "ArchiveExtractChanged" ? entry.attention.current : archive.expected).preview))}</div>` : ""}
        ${entry.archive_extract_report ? `<div class="quarantine-meta queue-attention">Entpackbilanz: ${entry.archive_extract_report.files} Dateien, ${entry.archive_extract_report.directories} neue Ordner, ${entry.archive_extract_report.skipped} übersprungen, ${entry.archive_extract_report.failed} fehlgeschlagen, ${entry.archive_extract_report.unstarted} noch offen (ohne abgeschlossenen Commit). Bereits entpackte Inhalte bleiben erhalten.</div>` : ""}
        ${directoryStreamReportHtml(entry.directory_stream_report, progress.directory_stream_report)}
        ${directoryDaclReportHtml(entry.directory_dacl_report, progress.directory_dacl_report)}
        ${fileDaclReportHtml(entry.file_dacl_report, progress.file_dacl_report)}
        ${createdDirectoryDaclReportHtml(entry.created_directory_dacl_report, progress.created_directory_dacl_report)}
        ${createdDirectoryBeforeChildrenPendingHtml(entry.operation, createdDaclRecovery && ![entry.created_directory_dacl_report, progress.created_directory_dacl_report].some(report => report?.created?.length || report?.completed?.length || report?.uncertain?.length))}
        ${cycleRecovery && tempRecovery ? `<details class="queue-cycle-details"><summary>Vollständige Prüfpfade</summary>${tempRecovery}</details>` : tempRecovery}
      </div>
      <div class="quarantine-actions">
        ${entry.operation.type === "ArchiveExtract" || parent && archive.kind === "extract" ? '<button class="btn ghost small" data-queue-action="extract-preview">Entpackhinweise</button>' : ""}
        ${entry.operation.type === "ArchivePack" || parent && archive.kind === "pack" ? '<button class="btn ghost small" data-queue-action="archive-preview">ZIP-Hinweise</button>' : ""}
        ${generated && entry.generated_source.issues?.length ? '<button class="btn ghost small" data-queue-action="generated-hints">Archivhinweise</button>' : ""}
        ${(generated || parent) && (entry.state !== "Completed" || entry.temp_recovery_paths?.includes(entry.operation.source?.root)) && showGeneratedSource ? '<button class="btn ghost small" data-queue-action="generated-show">Zwischenstand zeigen</button>' : ""}
        ${generatedConflict ? '<button class="btn primary small" data-queue-action="generated-conflicts">Konflikte prüfen …</button>' : ""}
        ${pending ? '<button class="btn ghost small" data-queue-action="up" aria-label="Nach oben" title="Nach oben">↑</button><button class="btn ghost small" data-queue-action="down" aria-label="Nach unten" title="Nach unten">↓</button><button class="btn ghost small" data-queue-action="remove">Entfernen</button>' : ""}
        ${createdDaclRecovery ? '<button class="btn primary small" data-queue-action="created-dacl-recovery">Rechte neuer Ordner prüfen</button>' : ""}
        ${fileDaclRecovery ? '<button class="btn primary small" data-queue-action="file-dacl-recovery">Objektweite Dateirechte prüfen</button>' : ""}
        ${daclRecovery ? '<button class="btn primary small" data-queue-action="dacl-recovery">Zugriffsrechte prüfen</button>' : ""}
        ${retry ? '<button class="btn primary small" data-queue-action="retry">Neu prüfen</button>' : ""}
        ${replanRequired ? '<button class="btn danger small" data-queue-action="replan-discard">Altplan verwerfen</button>' : ""}
        ${["ArchiveExtract", "ArchiveUpload"].includes(entry.operation.type) && ["Failed", "Interrupted"].includes(entry.state) ? '<button class="btn ghost small" data-queue-action="extract-ack">Bilanz geprüft und schließen</button>' : ""}
        ${unclearOutcome ? '<button class="btn danger small" data-queue-action="unclear-ack">Wirkung geprüft und schließen</button>' : ""}
        ${legacyDelete && entry.state === "Failed" ? '<button class="btn danger small" data-queue-action="legacy-ack">Nicht gestarteten Altplan verwerfen</button>' : ""}
        ${legacyDelete && entry.state === "Interrupted" ? '<button class="btn danger small" data-queue-action="legacy-ack">Wirkung geprüft und schließen</button>' : ""}
        ${recovery ? `<button class="btn danger small" data-queue-action="recovery">${entry.operation.type === "BidirectionalForward" ? "Neu abgleichen" : "Recovery"}</button>` : ""}
        ${completionRecovery ? '<button class="btn primary small" data-queue-action="recovery">Abschluss prüfen</button>' : ""}
        ${approve && !generatedConflict ? '<button class="btn danger small" data-queue-action="approve">Plan bestätigen</button>' : ""}
        ${discard ? '<button class="btn ghost small" data-queue-action="discard">Verwerfen</button>' : ""}
      </div>
    </div>`;
  }

  function capturePopoverFocus() {
    const focused = document.activeElement;
    const row = focused?.closest?.("[data-queue-id]");
    if (!row || !$("#queue-popover").contains(focused)) return null;
    const rows = [...$("#queue-popover").querySelectorAll("[data-queue-id]")];
    return {
      operationId: row.dataset.queueId,
      action: focused.dataset.queueAction || null,
      rowIndex: rows.indexOf(row),
    };
  }

  function restorePopoverFocus(previous) {
    if (!previous || $("#queue-popover").hidden) return;
    const rows = [...$("#queue-popover").querySelectorAll("[data-queue-id]")];
    const sameOperation = rows.find(row => row.dataset.queueId === previous.operationId);
    // Nach „Entfernen“ existiert die alte ID nicht mehr. Dann bleibt der Fokus
    // an derselben Listenposition oder wandert auf den letzten verbliebenen
    // Eintrag, statt unbemerkt auf den Dokumentanfang zurückzufallen.
    const fallbackIndex = Math.min(Math.max(previous.rowIndex, 0), rows.length - 1);
    const targetRow = sameOperation || rows[fallbackIndex];
    const buttons = targetRow
      ? [...targetRow.querySelectorAll("[data-queue-action]")]
      : [];
    const target = buttons.find(button => button.dataset.queueAction === previous.action)
      || buttons[0]
      || $("#queue-popover-close");
    target.focus();
  }

  function renderPopover() {
    const previousFocus = capturePopoverFocus();
    const active = snapshot.entries.find(entry => entry.state === "Running");
    $("#queue-popover-state").textContent = `${openCount()} offene/zu prüfende Operation(en) · ${snapshot.paused ? "pausiert" : "aktiv"}`;
    $("#queue-toggle-pause").textContent = snapshot.paused ? "Fortsetzen" : "Pausieren";
    $("#queue-toggle-pause").classList.toggle("primary", snapshot.paused);
    $("#queue-toggle-pause").classList.toggle("ghost", !snapshot.paused);
    $("#queue-toggle-stop").textContent = snapshot.stop_on_error
      ? "Bei Fehler: anhalten"
      : "Bei Fehler: weiter";

    $("#queue-popover-pinned").hidden = !active;
    $("#queue-popover-running").innerHTML = active ? entryHtml(active) : "";
    const remaining = active
      ? snapshot.entries.filter(entry => entry.operation_id !== active.operation_id)
      : snapshot.entries;
    $("#queue-popover-list").innerHTML = remaining.length
      ? remaining.map(entryHtml).join("")
      : active
        ? ""
        : '<div class="quarantine-empty">Die Operations-Queue ist leer.</div>';
    restorePopoverFocus(previousFocus);
  }

  function close({ restoreFocus = true } = {}) {
    $("#queue-popover").hidden = true;
    $("#btn-queue").setAttribute("aria-expanded", "false");
    if (restoreFocus) $("#btn-queue").focus();
  }

  async function open(operationId = null) {
    operationId = typeof operationId === "string" ? operationId : null;
    if (!$("#queue-popover").hidden && !operationId) {
      close();
      return;
    }
    try {
      await refresh();
    } catch (error) {
      return showError("Queue-Fehler", error);
    }
    renderPopover();
    $("#queue-popover").hidden = false;
    $("#btn-queue").setAttribute("aria-expanded", "true");
    const row = operationId ? [...$("#queue-popover").querySelectorAll("[data-queue-id]")]
      .find(candidate => candidate.dataset.queueId === operationId) : null;
    if (row) row.scrollIntoView({ block: "nearest" });
    (row?.querySelector("button") || $("#queue-popover-close")).focus({ preventScroll: true });
  }

  async function handleRowAction(event) {
    const button = event.target.closest("[data-queue-action]");
    const row = event.target.closest("[data-queue-id]");
    if (!button || !row) return;
    const operationId = row.dataset.queueId;
    const entry = snapshot.entries.find(candidate => candidate.operation_id === operationId);
    if (!entry) return;
    const selectedAction = button.dataset.queueAction;
    const selectedFocus = {
      operationId,
      action: selectedAction || null,
      rowIndex: [...$("#queue-popover").querySelectorAll("[data-queue-id]")].indexOf(row),
    };
    let succeeded = false;
    if (selectedAction === "generated-hints") {
      await new Promise(resolve => showOwnedModal("generated-source-hints", token => {
        $("#modal .modal").classList.add("archive-pack-modal");
        $("#modal-title").textContent = "Archivhinweise";
        const body = $("#modal-body"); body.innerHTML = `<p>${escapeHtml(entry.generated_source.label)}</p>` + archiveIssuesHtml("Vollständige Archivhinweise seitenweise");
        bindArchivePackPages(body, entry.generated_source);
        const buttons = $("#modal .modal-buttons"); buttons.replaceChildren();
        const close = document.createElement("button"); close.className = "btn ghost"; close.textContent = "Schließen";
        const finish = () => { if (hideModal(token)) resolve(); }; close.addEventListener("click", finish); buttons.append(close);
        return { focusTarget: close, onEscape: finish };
      }));
      await restoreAfterQueueModal(selectedFocus); return;
    }
    if (selectedAction === "generated-show") {
      try { await showGeneratedSource(await invoke("generated_source_path", { operationId })); }
      catch (error) { showError("Zwischenstand", error); }
      return;
    }
    if (selectedAction === "generated-conflicts") {
      await resumeGeneratedUpload(entry);
      await refresh(); await restoreAfterQueueModal(selectedFocus); return;
    }
    if (selectedAction === "extract-ack") {
      const parent = entry.operation.type === "ArchiveUpload";
      const confirmed = await confirmModal(parent ? "Archivzwischenstand geprüft?" : "Entpackbilanz geprüft?", parent
        ? "Der lokale Zwischenstand bleibt erhalten. Der Upload hat noch nicht begonnen. Bitte Bilanz und Prüfpfade durchsehen; dieser Auftrag wird ohne Wiederholung geschlossen."
        : "Bereits entpackte Inhalte bleiben erhalten. Bitte Ziel und angezeigte Recoverypfade prüfen. Der Auftrag wird ohne Wiederholung geschlossen; Inhalte ohne abgeschlossenen Commit werden nicht nachträglich entpackt.");
      if (confirmed) await action("acknowledge_archive_extract_outcome", { operationId, effectsReviewed: true });
      await restoreAfterQueueModal(selectedFocus); return;
    }
    if (selectedAction === "extract-preview") {
      const binding = entry.attention?.type === "ArchiveExtractChanged" ? entry.attention.current : archiveOperation(entry).expected;
      await confirmArchiveExtractPreview(binding.preview, { confirm: false, overwrite: archiveOperation(entry).request.options.overwrite, ...(entry.operation.type === "ArchiveUpload" ? {upload:true,targetLabel:endpointLabel(entry.operation.target,true)} : {}) });
      await restoreAfterQueueModal(selectedFocus); return;
    }
    if (selectedAction === "archive-preview") {
      const binding = entry.attention?.type === "ArchivePackChanged" ? entry.attention.current : archiveOperation(entry).expected;
      await confirmArchivePackPreview(binding.preview, { confirm: false, ...(entry.operation.type === "ArchiveUpload" ? {upload:true,targetLabel:endpointLabel(entry.operation.target,true)} : {}) });
      await restoreAfterQueueModal(selectedFocus); return;
    }
    if (selectedAction === "up" || selectedAction === "down") {
      succeeded = await action("move_queue_entry", {
        operationId,
        delta: selectedAction === "up" ? -1 : 1,
      });
    } else if (selectedAction === "remove") {
      succeeded = await action("remove_queue_entry", { operationId });
    } else if (selectedAction === "retry") {
      succeeded = await action("retry_queue_entry", { operationId });
    } else if (selectedAction === "replan-discard") {
      succeeded = await action("discard_replan_required_queue_entry", { operationId });
    } else if (selectedAction === "created-dacl-recovery") {
      const confirmed = await confirmModal("Rechte neuer Ordner prüfen und abschließen", "Es werden nur Identität und aktueller Rechtezustand geprüft. Der Eintrag kann nur bei einem bekannten Vorher- oder Nachherzustand geschlossen werden. Doppeldecker setzt keine Rechte erneut, stellt keinen früheren Zustand wieder her und setzt keine Kinderarbeit fort.");
      if (confirmed) await action("acknowledge_created_directory_dacl_outcome", { operationId, possibleEffectReviewed: true });
      await restoreAfterQueueModal(selectedFocus); return;
    } else if (selectedAction === "file-dacl-recovery") {
      const confirmed = await confirmModal("Objektweite Dateirechte prüfen und abschließen",
        "Es werden nur Identität und aktueller Rechtezustand geprüft. Der Eintrag kann nur bei einem bekannten Vorher- oder Nachherzustand geschlossen werden. Die Rechteänderung betrifft alle bestehenden und neuen Namen dieses Dateiobjekts, auch außerhalb der ausgewählten Ordner. Die Liste zeigt nur die hier bekannten Namen. Doppeldecker setzt keine Rechte erneut und stellt keinen früheren Zustand wieder her.");
      if (confirmed) await action("acknowledge_file_dacl_outcome", { operationId, possibleEffectReviewed: true });
      await restoreAfterQueueModal(selectedFocus); return;
    } else if (selectedAction === "dacl-recovery") {
      const confirmed = await confirmModal("Ordnerrechte prüfen und abschließen",
        "Doppeldecker liest den aktuellen Rechtezustand der gebundenen Ordner frisch. Wenn er einem gespeicherten Vorher- oder Nachherzustand entspricht, wird der Eintrag ohne erneutes Setzen geschlossen. Andere Zustände bleiben zur Prüfung offen.");
      if (confirmed) await action("acknowledge_directory_dacl_outcome", { operationId, possibleEffectReviewed: true });
      await restoreAfterQueueModal(selectedFocus);
      return;
    } else if (selectedAction === "unclear-ack") {
      await acknowledgeUnclearOutcome(entry, selectedFocus);
      return;
    } else if (selectedAction === "legacy-ack") {
      const possibleEffectReviewed = entry.state === "Interrupted";
      if (possibleEffectReviewed) {
        const confirmed = await confirmModal(
          "Alten WebDAV-Löschlauf schließen",
          "Dieser migrierte Lauf kann bereits eine Teilwirkung gehabt haben. Bestätigen Sie nur, wenn Ziel und Quarantäne manuell geprüft wurden. Der alte Plan wird niemals wiederholt.",
        );
        if (!confirmed) return;
      }
      succeeded = await action("acknowledge_legacy_webdav_delete", {
        operationId,
        possibleEffectReviewed,
      });
    } else if (selectedAction === "recovery") {
      close({ restoreFocus: false });
      if (entry.operation.type === "BidirectionalForward") {
        try { await recoverBidirectional(entry); } finally { await restoreAfterQueueModal(selectedFocus); }
      } else await recoverBidirectional(entry);
      return;
    } else if (selectedAction === "approve") {
      succeeded = await approveEntry(entry, selectedFocus);
    } else if (selectedAction === "discard") {
      await discardEntry(entry, selectedFocus);
      return;
    }
    if (succeeded) renderPopover();
  }

  async function restoreAfterQueueModal(selectedFocus) {
    if ($("#queue-popover").hidden) await open();
    else renderPopover();
    restorePopoverFocus(selectedFocus);
  }

  async function acknowledgeUnclearOutcome(entry, selectedFocus) {
    const remainingTemp = entry.operation.type === "RenameCycleRecovery"
      ? " Ein noch vorhandener Zwischenname bleibt erhalten und sperrt weiterhin den davon betroffenen Sync. Erst nach bewusstem Sichern oder Auflösen außerhalb der Sync-Wurzeln wird dieser wieder planbar." : "";
    const confirmed = await confirmModal(
      "Unklaren Ausgang schließen",
      "Die letzte Mutation kann bereits ganz oder teilweise gewirkt haben. Bestätigen Sie nur, wenn Quelle, Ziel und angezeigte Recoverypfade manuell geprüft wurden. Doppeldecker wiederholt die Operation nicht." + remainingTemp,
    );
    if (confirmed) {
      const succeeded = await action("acknowledge_unclear_queue_outcome", {
        operationId: entry.operation_id,
        possibleEffectReviewed: true,
      });
      if (succeeded) {
        showDock(`<span class="pill none">Manuell geprüft</span>Der unklare Lauf wurde ohne Wiederholung geschlossen.`, { closable: true });
      }
    }
    await restoreAfterQueueModal(selectedFocus);
  }

  async function discardEntry(entry, selectedFocus) {
    const earlierAttemptWarning = entry.operation.type === "BidirectionalForward"
      ? " Die aktuelle Vorbereitung wird verworfen. Bereits ausgeführte Teilwirkungen bleiben erhalten und können einen Neuabgleich erfordern."
      : entry.attempts > 1
      ? " Dieser Versuch wurde vor seiner ersten Mutation angehalten. Ein früherer Versuch kann Quelle oder Ziel bereits verändert haben; prüfen Sie den sichtbaren Zustand."
      : " In diesem Versuch wurde vor der ersten Mutation nichts geschrieben.";
    const schedulerMessage = snapshot.paused
      ? " Die Queue bleibt pausiert."
      : " Die nächste wartende Operation kann danach anlaufen.";
    const confirmed = await confirmModal(
      "Operation verwerfen",
      `Die neu entdeckte Lage wird nicht übernommen.${earlierAttemptWarning}${schedulerMessage}`,
    );
    if (confirmed) {
      const succeeded = await action("discard_queue_confirmation", {
        operationId: entry.operation_id,
      });
      if (succeeded) {
        const dockState = snapshot.paused
          ? "Die Queue bleibt pausiert."
          : "Die nächste wartende Operation kann anlaufen.";
        showDock(`<span class="pill none">Verworfen</span>Dieser Versuch wurde vor seiner ersten Mutation geschlossen. ${dockState}`, { closable: true });
      }
    }
    await restoreAfterQueueModal(selectedFocus);
  }

  async function approveEntry(entry, selectedFocus) {
    const attention = structuredClone(entry.attention);
    if (entry.operation.type === "BidirectionalForward" && attention?.type === "RemoteRun") {
      try { return await recoverBidirectional(entry); }
      finally { await restoreAfterQueueModal(selectedFocus); }
    }
    if (entry.operation.type === "ContentSync" && attention?.type === "RemoteRun") {
      try {
        const preview = await invoke("content_sync_queue_preview", { operationId: entry.operation_id });
        const confirmation = await confirmContentSync(preview);
        if (!confirmation) return false;
        return await action("approve_content_sync_queue_entry", { operationId: entry.operation_id,
          preparationDigest: preview.digest, ...confirmation });
      } catch (error) { showError("Dateisync", error); return false; }
      finally { await restoreAfterQueueModal(selectedFocus); }
    }
    if (entry.operation.type === "LocalSync" && attention?.type === "RemoteRun") {
      try {
        const preview = await invoke("local_sync_queue_preview", { operationId: entry.operation_id });
        if (!await confirmLocalSync(preview)) return false;
        return await action("approve_local_sync_queue_entry", {
          operationId: entry.operation_id, preparationDigest: preview.plan_digest,
          replacementsConfirmed: true,
        });
      } catch (error) {
        showError("Lokaler Sync", error);
        return false;
      } finally {
        // Ein Klick im getrennten Wirkungsdialog schließt das Popover als
        // Außenklick. Der Rückweg gehört weiterhin zum gewählten Queueeintrag.
        await restoreAfterQueueModal(selectedFocus);
      }
    }
    if (attention?.type === "RemoteRun") {
      const confirmed = await confirmModal("Sync ausführen",
        `<p>${escapeHtml(operationLabel(entry))}</p><p>${escapeHtml(attention.reason || entry.error || "Frisch geprüften Sync ausführen.")}</p>`);
      if (!confirmed) {
        await restoreAfterQueueModal(selectedFocus);
        return false;
      }
      const succeeded = await action(approvalCommand(attention, entry.operation.type), {
        operationId: entry.operation_id, effectsConfirmed: true,
      });
      await restoreAfterQueueModal(selectedFocus);
      return succeeded;
    }
    if (attention?.type === "ArchiveExtractChanged") {
      if (!attention.current.preview.catalog_complete && !archiveOperation(entry).request.options.allow_partial) {
        showError("ZIP entpacken", new Error("Das Archiv ist jetzt unvollständig lesbar. Bitte diesen Auftrag verwerfen und den bekannten Teil ausdrücklich neu auswählen."));
        await restoreAfterQueueModal(selectedFocus); return false;
      }
      const confirmed = await confirmArchiveExtractPreview(attention.current.preview, { overwrite: archiveOperation(entry).request.options.overwrite, ...(entry.operation.type === "ArchiveUpload" ? {upload:true,targetLabel:endpointLabel(entry.operation.target,true)} : {}) });
      if (!confirmed) { await restoreAfterQueueModal(selectedFocus); return false; }
      const succeeded = await action("approve_queue_entry", { operationId: entry.operation_id, expectedAttention: attention, effectsConfirmed: true });
      await restoreAfterQueueModal(selectedFocus); return succeeded;
    }
    if (attention?.type === "ArchivePackChanged") {
      const confirmed = await confirmArchivePackPreview(attention.current.preview, entry.operation.type === "ArchiveUpload" ? {upload:true,targetLabel:endpointLabel(entry.operation.target,true)} : {});
      if (!confirmed) { await restoreAfterQueueModal(selectedFocus); return false; }
      const succeeded = await action("approve_queue_entry", { operationId: entry.operation_id,
        expectedAttention: attention, effectsConfirmed: true });
      await restoreAfterQueueModal(selectedFocus); return succeeded;
    }
    let description = entry.error || "Die Operation hat sich geändert.";
    if (attention?.type === "PlanChanged") {
      const plan = attention.current_plan;
      description += ` Neuer Plan: ${plan.copy_count} neu, ${plan.update_count} ersetzen, ${plan.delete_count} quarantänisieren.`;
    } else if (attention?.type === "ConflictsChanged") {
      description += ` ${attention.current_conflicts.length} aktuelle Konflikt(e): ${attention.current_conflicts.join(", ")}.`;
    } else if (attention?.type === "SourceTreeChanged") {
      const tree = attention.current_tree;
      description += tree
        ? ` Neuer Quellbaum: ${tree.files} Datei(en), ${tree.directories} Ordner, ${tree.bytes} Byte. ${tree.omitted.length} Eintrag/Teilbaum ausgelassen: ${tree.omitted.map(item => `${item.path}: ${item.reason}`).join("; ")}.`
        : " Die Quelle ist jetzt eine direkte Dateiauswahl ohne Baumbeleg.";
    } else if (attention?.type === "BidirectionalPlanChanged") {
      description += " Baseline, vollständige Bäume oder Konfliktentscheidungen besitzen jetzt einen anderen Digest.";
    }
    const title = attention?.type === "SourceTreeChanged" ? "Quellbaumumfang bestätigen"
      : ["LocalFileCommit", "BestEffortCommitDowngrade"].includes(attention?.type) ? "Best-Effort-Kopie bestätigen"
      : "Queue-Wirkung bestätigen";
    const selectedSources = entry.operation.items?.length
      ? `\nAusgewählte Quellen:\n${entry.operation.items.join("\n")}` : "";
    const boundSource = typeof entry.operation.source === "string"
      ? `\nQuelle: ${entry.operation.source}` : "";
    const confirmed = await confirmModal(title,
      `<div class="modal-description modal-description-scroll" tabindex="0" role="region" aria-label="Wirkung und Einschränkungen">` +
        `<p>${escapeHtml(operationLabel(entry))}</p><p>${escapeHtml(description + selectedSources + boundSource)}</p></div>`);
    if (!confirmed) {
      await restoreAfterQueueModal(selectedFocus);
      return false;
    }
    const succeeded = await action("approve_queue_entry", {
      operationId: entry.operation_id,
      expectedAttention: attention,
      effectsConfirmed: true,
    });
    if (succeeded) {
      showDock(`<span class="pill ok">Neu bestätigt</span>Der sichtbare Queue-Plan wurde übernommen und wird vor Mutation nochmals geprüft.`, { closable: true });
    }
    await restoreAfterQueueModal(selectedFocus);
    return succeeded;
  }

  function approvalCommand(attention, operationType) {
    if (attention?.type !== "RemoteRun") return "approve_queue_entry";
    return {
      Bidirectional: "approve_bidirectional_queue_entry",
      ProviderSync: "approve_provider_queue_entry",
      FtpsSync: "approve_ftps_queue_entry",
      WebDavSync: "approve_webdav_queue_entry",
      SmbSync: "approve_smb_queue_entry",
      SftpSync: "approve_sftp_queue_entry",
    }[operationType] || "approve_sftp_queue_entry";
  }

  function warnBeforeExit(value) {
    const generation = ++exitGeneration;
    const unreadable = Array.isArray(value?.unreadableQueues)
      ? value.unreadableQueues.filter(label => typeof label === "string") : [];
    const reported = Number(value?.openEntries);
    const count = Number.isSafeInteger(reported) && reported >= 0
      ? reported : Math.max(0, openCount());
    const confirmationToken = value?.token;
    if (!confirmationToken) {
      if (exitModalToken) cancelOwnedModal(exitModalToken);
      exitModalToken = null;
      showError("Beenden", { code: "confirmation_mismatch", message: "Der Beenden-Hinweis besitzt keine gültige Backend-Bestätigung." });
      return;
    }
    close({ restoreFocus: false });
    exitModalToken = showLatestOwnedModal("queue-exit", leaseToken => {
      $("#modal .modal").classList.remove("quarantine-modal", "real-mode-modal");
      $("#modal-title").textContent = "Doppeldecker beenden?";
      $("#modal-body").innerHTML =
        (count > 0 ? `<p>${count} offene oder zu prüfende Operation(en) liegen in der persistenten Queue.</p>` +
          "<p>Die Queue bleibt auf diesem Gerät gespeichert. Ein laufender Eintrag wird beim nächsten Start als unterbrochen angezeigt und muss sicher neu geprüft oder wiederhergestellt werden.</p>" : "") +
        (unreadable.length ? `<p>Die Queue-Journale dieser Fenster konnten nicht gelesen werden: ${unreadable.map(escapeHtml).join(", ")}. Ihr offener Arbeitsstand ist unbekannt. Die Journaldateien bleiben beim Beenden unverändert erhalten.</p>` : "");
      $("#modal-body").onclick = null;
      const bar = $("#modal .modal-buttons");
      bar.innerHTML = "";
      const stay = document.createElement("button");
      stay.className = "btn ghost";
      stay.textContent = "Hier bleiben";
      const quit = document.createElement("button");
      quit.className = "btn danger";
      quit.textContent = "Trotzdem beenden";
      const closeLatest = () => {
        if (generation !== exitGeneration || exitModalToken !== leaseToken) return false;
        const closed = hideModal(leaseToken);
        if (closed) exitModalToken = null;
        return closed;
      };
      stay.addEventListener("click", closeLatest);
      quit.addEventListener("click", async () => {
        if (!closeLatest()) return;
        try {
          await invoke("quit_after_queue_warning", { token: confirmationToken });
        } catch (error) {
          showError("Beenden", error);
        }
      });
      bar.append(stay, quit);
      return { focusTarget: stay, onEscape: closeLatest };
    });
  }

  $("#queue-popover").addEventListener("click", handleRowAction);
  $("#queue-popover-close").addEventListener("click", () => close());
  $("#queue-toggle-pause").addEventListener("click", () => {
    void action("set_queue_paused", { paused: !snapshot.paused });
  });
  $("#queue-toggle-stop").addEventListener("click", () => {
    void action("set_queue_stop_on_error", { stop: !snapshot.stop_on_error });
  });
  $("#queue-popover").addEventListener("keydown", event => {
    if (event.key !== "Escape") return;
    event.preventDefault();
    event.stopPropagation();
    close();
  });
  document.addEventListener("pointerdown", event => {
    if ($("#queue-popover").hidden) return;
    if ($("#queue-popover").contains(event.target) || $("#btn-queue").contains(event.target)) return;
    close({ restoreFocus: false });
  });

  return { applySnapshot, open, refresh, warnBeforeExit };
}
