import {
  cancelOwnedModal,
  confirmModal,
  hideModal,
  promptModal,
  showLatestOwnedModal,
} from "./modal.js";
import { invoke } from "./tauri-api.js";
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

function legacyWebdavDelete(entry) {
  return entry.operation?.type === "ProviderDelete"
    && entry.operation.expected_plan?.root_identity?.identity?.root_etag
      === LEGACY_UNBOUND_WEBDAV_ETAG;
}

function endpointLabel(endpoint) {
  if (!endpoint) return "?";
  const provider = endpoint.provider === "Local"
    ? "Lokal"
    : endpoint.provider === "Ftps" && endpoint.connection?.transport_security === "plaintext"
      ? "FTP"
      : endpoint.provider;
  return `${provider} · ${endpoint.root}`;
}

function operationLabel(entry) {
  const operation = entry.operation;
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
  if (operation.type === "Bidirectional") {
    return `Bidirektional · ${operation.pair_name} · ${endpointLabel(operation.left)} ↔ ${endpointLabel(operation.right)}`;
  }
  if (operation.type === "ProviderSync") {
    return `Provider ${operation.options.mode} · ${endpointLabel(operation.source)} → ${endpointLabel(operation.target)}`;
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

    const active = next.entries.find(entry => entry.state === "Running");
    const activeRun = active ? `${active.operation_id}/${active.attempts || 0}` : null;
    if (active && activeRun !== activeOperationId && !realRunActive()) {
      activeOperationId = activeRun;
      const context = { operationId: active.operation_id, attempt: active.attempts || 0 };
      // Alle einseitigen Sync-Provider verwenden dieselbe Plan-/Fortschrittsanzeige.
      // SMB fehlte früher in dieser Matrix und erschien dadurch fälschlich als Move.
      if (["Sync", "LocalSync", "SftpSync", "FtpsSync", "WebDavSync", "SmbSync", "ProviderSync"].includes(active.operation.type)) {
        startSyncProgress(active.operation.expected_plan, { ...context, localExecution: active.operation.execution_hint, simulated: active.operation.type === "Sync" && !active.operation.sandbox });
      } else if (active.operation.type === "Bidirectional") {
        // Der bidirektionale Eintrag besitzt zwei Teilpläne statt eines
        // `expected_plan`. Bis zum ersten Progress-Event zeigen wir deshalb einen
        // neutralen Starttext und niemals fälschlich „Verschieben“.
        startFileProgress("Bidirektionaler Sync aus Queue …", context);
      } else {
        if (active.operation.type === "ProviderDelete") {
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
        ? `<div class="quarantine-meta queue-attention">${escapeHtml(entry.error)}</div>`
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
      .map(path => `<div class="quarantine-meta queue-attention">Temp-Recoverypfad prüfen, nicht automatisch löschen: <code>${escapeHtml(path)}</code></div>`)
      .join("");
    const pending = entry.state === "Pending";
    const legacyDelete = legacyWebdavDelete(entry);
    const completionMarker = entry.bidirectional_completion_recovery;
    const completionRecovery = entry.operation.type === "Bidirectional"
      && (entry.state === "Interrupted"
        || (entry.state === "Completed" && completionMarker === "complete")
        || (entry.state === "Cancelled" && completionMarker === "rolled_back"))
      && Boolean(completionMarker);
    const recovery = !completionRecovery
      && entry.operation.type === "Bidirectional"
      && entry.recovery_available === true
      && ["Failed", "Interrupted"].includes(entry.state);
    const retry = !completionRecovery
      && !recovery
      && !legacyDelete
      && entry.error_code !== "outcome_unclear"
      && entry.error_code !== "replan_required"
      && ["Failed", "Cancelled", "Interrupted"].includes(entry.state);
    const replanRequired = !completionRecovery
      && !recovery
      && entry.error_code === "replan_required"
      && entry.state === "Failed";
    const unclearOutcome = !completionRecovery
      && !recovery
      && !legacyDelete
      && entry.error_code === "outcome_unclear"
      && ["Failed", "Interrupted"].includes(entry.state);
    const approve = entry.state === "AwaitingConfirmation";
    // Eine wartende Remote-Vorbereitung hält Permit und Verbindung im Backend und
    // wird über ihren eigenen Weg abgebaut; alle anderen Rückfragen darf der
    // Nutzer hier ablehnen, ohne die Lage zu bestätigen, die er ablehnen will.
    const discard = approve && (entry.attention?.type !== "RemoteRun" || entry.operation.type === "LocalSync");
    const label = operationLabel(entry);
    return `<div class="quarantine-row queue-row" data-queue-id="${escapeHtml(entry.operation_id)}">
      <div class="quarantine-main">
        <div class="quarantine-name" title="${escapeHtml(label)}">${escapeHtml(label)}</div>
        <div class="quarantine-meta">${STATE_LABELS[entry.state] || entry.state} · Versuch ${entry.attempts}${detail}</div>
        ${attention}
        ${sourceTreeDetail}
        ${tempRecovery}
      </div>
      <div class="quarantine-actions">
        ${pending ? '<button class="btn ghost small" data-queue-action="up" aria-label="Nach oben" title="Nach oben">↑</button><button class="btn ghost small" data-queue-action="down" aria-label="Nach unten" title="Nach unten">↓</button><button class="btn ghost small" data-queue-action="remove">Entfernen</button>' : ""}
        ${retry ? '<button class="btn primary small" data-queue-action="retry">Neu prüfen</button>' : ""}
        ${replanRequired ? '<button class="btn danger small" data-queue-action="replan-discard">Altplan verwerfen</button>' : ""}
        ${unclearOutcome ? '<button class="btn danger small" data-queue-action="unclear-ack">Wirkung geprüft und schließen</button>' : ""}
        ${legacyDelete && entry.state === "Failed" ? '<button class="btn danger small" data-queue-action="legacy-ack">Nicht gestarteten Altplan verwerfen</button>' : ""}
        ${legacyDelete && entry.state === "Interrupted" ? '<button class="btn danger small" data-queue-action="legacy-ack">Wirkung geprüft und schließen</button>' : ""}
        ${recovery ? '<button class="btn danger small" data-queue-action="recovery">Recovery</button>' : ""}
        ${completionRecovery ? '<button class="btn primary small" data-queue-action="recovery">Abschluss prüfen</button>' : ""}
        ${approve ? '<button class="btn danger small" data-queue-action="approve">Plan bestätigen</button>' : ""}
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

  async function open() {
    if (!$("#queue-popover").hidden) {
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
    $("#queue-popover-close").focus();
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
      await recoverBidirectional(entry);
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
    const confirmed = await confirmModal(
      "Unklaren Ausgang schließen",
      "Die letzte Mutation kann bereits ganz oder teilweise gewirkt haben. Bestätigen Sie nur, wenn Quelle, Ziel und angezeigte Recoverypfade manuell geprüft wurden. Doppeldecker wiederholt die Operation nicht.",
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
    const earlierAttemptWarning = entry.attempts > 1
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
    const attention = entry.attention;
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
    const phrase = attention?.confirmation || "";
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
    let backupConfirmed = true;
    // Die semantische ID des angeklickten Knopfs ist stabiler als
    // `document.activeElement`: Pointer-Aktivierung fokussiert je Plattform nicht
    // zwingend den Knopf, der den Dialog ausgelöst hat.
    const backupReturnFocus = selectedFocus || capturePopoverFocus();
    if (attention?.type === "RemoteRun"
      && (entry.operation.type === "Bidirectional" || entry.operation.options.mode !== "AddOnly")) {
      backupConfirmed = await confirmModal(
        "Unabhängiges Backup bestätigen",
        "Ich bestätige ein unabhängiges Backup und eine jüngste erfolgreiche Restore-Probe dieses Backups. Doppeldecker hat dieses externe Backup nicht technisch geprüft.",
      );
    }
    if (!backupConfirmed) {
      // Der Bestätigungsdialog liegt über dem bereits offenen Popover. `open()`
      // würde es in diesem Fall als Toggle schließen. Ein erneutes Rendern erhält
      // stattdessen den fachlich gleichen Queue-Knopf und dessen Tastaturfokus.
      if ($("#queue-popover").hidden) {
        await open();
        restorePopoverFocus(backupReturnFocus);
      } else {
        renderPopover();
        restorePopoverFocus(backupReturnFocus);
      }
      return false;
    }
    const typed = ["LocalFileCommit", "SourceTreeChanged", "BestEffortCommitDowngrade"].includes(attention?.type)
      ? await promptModal(attention.type === "SourceTreeChanged" ? "Quellbaumumfang bestätigen" : "Best-Effort-Kopie bestätigen", "", {
        description,
        inputLabel: `Exakt „${phrase}“ eingeben`,
      })
      : await promptModal(`${description} Exakt „${phrase}“ eingeben`, "");
    if (typed !== phrase) {
      showDock(`<span class="pill none">Nicht bestätigt</span>Queue-Eintrag bleibt gesperrt.`, { closable: true });
      return false;
    }
    const command = approvalCommand(attention, entry.operation.type);
    const succeeded = await action(command, {
      operationId: entry.operation_id,
      confirmation: typed,
      backupConfirmed,
    });
    if (succeeded) {
      if (attention?.type !== "RemoteRun") {
        showDock(`<span class="pill ok">Neu bestätigt</span>Der sichtbare Queue-Plan wurde übernommen und wird vor Mutation nochmals geprüft.`, { closable: true });
      }
    }
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
    const count = Math.max(1, Number(value?.openEntries) || openCount());
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
        `<p>${count} offene oder zu prüfende Operation(en) liegen in der persistenten Queue.</p>` +
        "<p>Die Queue bleibt auf diesem Gerät gespeichert. Ein laufender Eintrag wird beim nächsten Start als unterbrochen angezeigt und muss sicher neu geprüft oder wiederhergestellt werden.</p>";
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
