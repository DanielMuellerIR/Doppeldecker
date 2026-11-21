import { createdDirectoryDaclReportHtml, createdDirectoryBeforeChildrenPendingHtml } from "./created-directory-dacl-effects.js";
import { fileDaclReportHtml } from "./file-dacl-effects.js";
import { directoryDaclReportHtml } from "./directory-dacl-effects.js";
import { directoryStreamReportHtml } from "./directory-stream-effects.js";
import {
  archiveExtractFormatLabel,
  archiveExtractSourceLabel,
  archiveExtractFailureMessage,
  archiveExtractScope,
  archiveExtractSummary,
} from "./archive-extract-ui.js";
import { archivePackSummary } from "./archive-pack-ui.js";
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

export function endpointLabel(endpoint, includeServer = false) {
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

export function archiveOperation(entry) {
  return entry.operation.type === "ArchiveUpload" ? entry.operation.generation : entry.operation;
}
export function resumableProtocol(plan) {
  if (plan.webdav) return "WebDAV";
  if (plan.ftps) return "FTPS";
  if (plan.sftp) return "SFTP";
  return null;
}

export function operationLabel(entry) {
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
    return `${operation.generation.kind === "pack" ? "ZIP erstellen" : `${archiveExtractFormatLabel(operation.generation.expected.preview)} entpacken`} und hochladen → ${endpointLabel(operation.target,true)}`;
  }
  if (entry.generated_source) {
    const { kind, label } = entry.generated_source;
    return `${kind === "zip" ? "ZIP erstellt" : "Archiv lokal entpackt"} · Upload ${label} → ${endpointLabel(operation.target,true)}`;
  }
  if (operation.type === "ArchiveExtract") {
    const preview = (entry.attention?.type === "ArchiveExtractChanged" ? entry.attention.current : operation.expected).preview;
    return `${archiveExtractFormatLabel(preview)} entpacken · ${archiveExtractScope(preview)} · ${archiveExtractSourceLabel(preview)} → ${operation.request.target_dir}${preview.relative_root ? ` · Zielname: ${preview.relative_root}` : ""}`;
  }
  if (operation.type === "ArchivePack") {
    return `ZIP erstellen · ${operation.request.options.compression === "stored" ? "Stored" : "Deflate"} · ${operation.expected.preview.target}`;
  }
  if (operation.type === "FtpsRecovery") {
    const identity = operation.identity;
    const provider = identity.server.transport_security === "plaintext" ? "FTP" : "FTPS";
    return `${provider}-Vorbereitung prüfen · ${identity.server.host}:${identity.server.port} · ${identity.virtual_root}`;
  }
  if (operation.type === "ResumableCopy") {
    const protocol = resumableProtocol(operation.plan);
    const source = protocol ? `${protocol} · ${operation.plan.source}` : operation.plan.source.split("/").at(-1);
    return `Fortsetzbare Kopie · ${source} → ${operation.plan.target}`;
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

function entryHtml(entry, { canResumeGeneratedUpload, canShowGeneratedSource }) {
  const progress = entry.progress || {};
  const resumable = entry.operation.type === "ResumableCopy";
  const checkpoint = resumable ? entry.operation.checkpoint : null;
  const partialStateLabel = resumable ? {
    resumable_paused: "pausiert",
    resumable_discarded: "Teilkopie verworfen",
  }[progress.phase] : null;
  const stateLabel = partialStateLabel || STATE_LABELS[entry.state] || entry.state;
  const detail = progress.bytes_total && (!resumable || entry.state === "Completed")
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
    .map(path => `<div class="quarantine-meta queue-attention">${resumable && !entry.error ? "Arbeitsordner der Teilkopie:" : (entry.directory_dacl_recovery || entry.file_dacl_recovery || entry.created_directory_dacl_recovery) ? "Prüfpfad der Rechteänderung, nicht automatisch löschen:" : "Temp-Recoverypfad prüfen, nicht automatisch löschen:"} <code>${escapeHtml(path)}</code></div>`)
    .join("");
  const createdDaclRecovery=entry.created_directory_dacl_recovery === true && ["Failed", "Interrupted"].includes(entry.state);
  const fileDaclRecovery = entry.file_dacl_recovery === true && ["Failed", "Interrupted"].includes(entry.state);
  const daclRecovery = entry.directory_dacl_recovery === true && ["Failed", "Interrupted"].includes(entry.state);
  const hasDaclEffects = [entry.directory_dacl_report, progress.directory_dacl_report, entry.file_dacl_report, progress.file_dacl_report,entry.created_directory_dacl_report,progress.created_directory_dacl_report].some(report => report?.completed?.length || report?.uncertain?.length);
  const pending = entry.state === "Pending";
  const discardPartial = resumable
    && (entry.error_code !== "outcome_unclear" || progress.phase === "resumable_discard")
    && (["Failed", "Interrupted"].includes(entry.state) || pending && checkpoint);
  const saved = checkpoint?.phase === "bytes"
    ? `<div class="quarantine-meta">Gesichert: ${fmtTransferBytes(checkpoint.written)} von ${fmtTransferBytes(entry.operation.plan.bytes)}.</div>` : "";
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
    && progress.phase !== "resumable_discarded"
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
    && progress.phase !== "resumable_discard"
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
  const generatedConflict = generated && approve && entry.attention?.type === "ConflictsChanged" && canResumeGeneratedUpload;
  const cycleRecovery = entry.operation.type === "RenameCycleRecovery";
  return `<div class="quarantine-row queue-row${["ArchivePack", "ArchiveExtract", "ArchiveUpload"].includes(entry.operation.type) || generated ? " queue-archive-row" : ""}${cycleRecovery ? " queue-cycle-row" : ""}" data-queue-id="${escapeHtml(entry.operation_id)}">
    <div class="quarantine-main">
      <div class="quarantine-name" title="${escapeHtml(label)}">${escapeHtml(label)}</div>
      <div class="quarantine-meta">${entry.operation.type === "RenameCycleRecovery" && entry.state === "Interrupted" ? "manuell prüfen" : entry.file_incomplete && entry.state === "Completed" ? "mit Auslassungen abgeschlossen" : stateLabel} · Versuch ${entry.attempts}${detail}</div>
      ${attention}${incompleteNotice}${saved}
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
      ${(generated || parent) && (entry.state !== "Completed" || entry.generated_source_recovery) && canShowGeneratedSource ? '<button class="btn ghost small" data-queue-action="generated-show">Zwischenstand zeigen</button>' : ""}
      ${generatedConflict ? '<button class="btn primary small" data-queue-action="generated-conflicts">Konflikte prüfen …</button>' : ""}
      ${pending ? '<button class="btn ghost small" data-queue-action="up" aria-label="Nach oben" title="Nach oben">↑</button><button class="btn ghost small" data-queue-action="down" aria-label="Nach unten" title="Nach unten">↓</button>' : ""}
      ${pending && !checkpoint ? '<button class="btn ghost small" data-queue-action="remove">Entfernen</button>' : ""}
      ${entry.state === "Failed" && progress.phase === "generated_cleanup" ? '<button class="btn ghost small" data-queue-action="remove">Bereinigung erneut versuchen</button>' : ""}
      ${createdDaclRecovery ? '<button class="btn primary small" data-queue-action="created-dacl-recovery">Rechte neuer Ordner prüfen</button>' : ""}
      ${fileDaclRecovery ? '<button class="btn primary small" data-queue-action="file-dacl-recovery">Objektweite Dateirechte prüfen</button>' : ""}
      ${daclRecovery ? '<button class="btn primary small" data-queue-action="dacl-recovery">Zugriffsrechte prüfen</button>' : ""}
      ${retry ? `<button class="btn primary small" data-queue-action="retry">${resumable ? "Fortsetzen" : "Neu prüfen"}</button>` : ""}
      ${discardPartial ? '<button class="btn ghost small" data-queue-action="discard-partial">Teilkopie verwerfen …</button>' : ""}
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

export function renderQueuePopover(snapshot, openCount, generatedSourceActions) {
  const active = snapshot.entries.find(entry => entry.state === "Running");
  $("#queue-popover-state").textContent = `${openCount} offene/zu prüfende Operation(en) · ${snapshot.paused ? "pausiert" : "aktiv"}`;
  $("#queue-toggle-pause").textContent = snapshot.paused ? "Fortsetzen" : "Pausieren";
  $("#queue-toggle-pause").classList.toggle("primary", snapshot.paused);
  $("#queue-toggle-pause").classList.toggle("ghost", !snapshot.paused);
  $("#queue-toggle-stop").textContent = snapshot.stop_on_error
    ? "Bei Fehler: anhalten"
    : "Bei Fehler: weiter";

  $("#queue-popover-pinned").hidden = !active;
  $("#queue-popover-running").innerHTML = active ? entryHtml(active, generatedSourceActions) : "";
  const remaining = active
    ? snapshot.entries.filter(entry => entry.operation_id !== active.operation_id)
    : snapshot.entries;
  $("#queue-popover-list").innerHTML = remaining.length
    ? remaining.map(entry => entryHtml(entry, generatedSourceActions)).join("")
    : active
      ? ""
      : '<div class="quarantine-empty">Die Operations-Queue ist leer.</div>';
}
