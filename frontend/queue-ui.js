import { archiveExtractFormatLabel, confirmArchiveExtractPreview } from "./archive-extract-ui.js";
import { confirmArchivePackPreview, bindArchivePackPages, archiveIssuesHtml } from "./archive-pack-ui.js";
import {
  cancelOwnedModal,
  choiceModal,
  confirmModal,
  hideModal,
  showLatestOwnedModal,
  showOwnedModal,
} from "./modal.js";
import { invoke } from "./tauri-api.js";
import { confirmContentSync } from "./content-sync-ui.js";
import { confirmLocalSync } from "./local-sync-ui.js";
import { $, escapeHtml } from "./ui-utils.js";

import { archiveOperation, endpointLabel, operationLabel, renderQueuePopover, resumableProtocol } from "./queue-view.js";

/** @typedef {import("./queue-snapshot-wire.js").QueueSnapshotWire} QueueSnapshot */
/** @typedef {QueueSnapshot["entries"][number]} QueueEntry */

export { operationLabel };

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
  closeCleanedGeneratedSources,
}) {
  /** @type {Omit<QueueSnapshot, "version">} */
  let snapshot = { revision: 0, paused: false, stop_on_error: false, entries: [] };
  let activeOperationId = null;
  let exitGeneration = 0;
  let exitModalToken = null;

  function openCount(current = snapshot) {
    return current.entries.filter(entry => Boolean(entry.bidirectional_completion_recovery)
      || !["Completed", "Cancelled"].includes(entry.state)).length;
  }

  /** @param {QueueSnapshot} next */
  function applySnapshot(next) {
    const currentRevision = Number.isSafeInteger(snapshot.revision) ? snapshot.revision : 0;
    const nextRevision = Number.isSafeInteger(next?.revision) ? next.revision : 0;
    if (nextRevision < currentRevision) {
      // Command-Antworten und Events laufen über getrennte IPC-Wege. Ein bereits
      // sichtbarer terminaler Eventstand darf nie durch die ältere Startantwort
      // desselben Workers zurückgesetzt werden.
      return false;
    }
    if (closeCleanedGeneratedSources) {
      const remaining = new Set(next.entries.map(entry => entry.operation_id));
      const cleaned = new Set(snapshot.entries.filter(entry => (
        entry.generated_source || entry.operation.type === "ArchiveUpload"
      ) && !remaining.has(entry.operation_id)).map(entry => entry.operation_id));
      closeCleanedGeneratedSources(cleaned);
    }
    snapshot = next;
    const count = openCount(next);
    // Ein beim Start gefundenes beschädigtes Journal ist ein eigener Befund. Ein
    // späterer normaler Queue-Snapshot darf sein sichtbares Fehlerzeichen nicht
    // wieder durch eine Eintragszahl ersetzen.
    $("#queue-count").textContent = queueErrorActive() ? "!" : String(count);
    $("#btn-queue").title = `${count} offene/zu prüfende Operation(en) · ${next.paused ? "pausiert" : "aktiv"}`;

    const active = next.entries.find(entry => entry.state === "Running"
      || entry.state === "Validating"
        && !["generated_cleanup", "resumable_discard"].includes(entry.progress?.phase)
        && ["ArchivePack", "ArchiveExtract", "ArchiveUpload", "ResumableCopy"].includes(entry.operation.type));
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
          const generation = active.operation.generation;
          const format = archiveExtractFormatLabel(generation.expected?.preview);
          startFileProgress(generation.kind === "pack" ? "Erstelle ZIP vor dem Upload …" : `Entpacke ${format} vor dem Upload …`, context);
        } else if (active.operation.type === "ArchiveExtract") {
          const format = archiveExtractFormatLabel(active.operation.expected?.preview);
          startFileProgress(active.state === "Validating" ? `Prüfe ${format} und Entpackziel …` : `Entpacke ${format} aus Queue …`, context);
        } else if (active.operation.type === "ArchivePack") {
          startFileProgress(active.state === "Validating" ? "Prüfe ZIP-Quellen aus Queue …" : "Erstelle ZIP-Archiv aus Queue …", context);
        } else if (active.operation.type === "ResumableCopy") {
          const protocol = resumableProtocol(active.operation.plan);
          startFileProgress(protocol ? `Fortsetzbarer ${protocol}-Download …` : "Fortsetzbare Kopie …", { ...context, pausable: true });
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

  function settleGeneratedSource(operationId) {
    const entry = snapshot.entries.find(entry => entry.operation_id === operationId);
    // Der terminale Datei-Event folgt dem Snapshot nach dem Cleanup. Ein früher
    // Journalabruf kann bereits Completed zeigen, während die Bereinigung läuft.
    if (entry?.generated_source && entry.state === "Completed" && entry.generated_source_recovery === false) {
      closeCleanedGeneratedSources?.(new Set([operationId]));
    }
  }

  async function refresh() {
    try {
      applySnapshot(await invoke("list_operation_queue"));
    } catch (error) {
      $("#queue-count").textContent = "!";
      throw error;
    }
  }

  async function action(request) {
    try {
      applySnapshot(await request());
      return true;
    } catch (error) {
      showError("Queue-Fehler", error);
      return false;
    }
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
    renderQueuePopover(snapshot, openCount(), {
      canResumeGeneratedUpload: Boolean(resumeGeneratedUpload),
      canShowGeneratedSource: Boolean(showGeneratedSource),
    });
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

  /** @param {Event} event */
  async function handleRowAction(event) {
    if (!(event.target instanceof Element)) return;
    const button = event.target.closest("[data-queue-action]");
    const row = event.target.closest("[data-queue-id]");
    if (!(button instanceof HTMLElement) || !(row instanceof HTMLElement)) return;
    const operationId = row.dataset.queueId;
    if (!operationId) return;
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
      try {
        const path = await invoke("generated_source_path", { operationId });
        const current = snapshot.entries.find(candidate => candidate.operation_id === operationId);
        if (current && (current.state !== "Completed" || current.generated_source_recovery)) {
          await showGeneratedSource(operationId, path);
        }
      }
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
      if (confirmed) await action(() => invoke("acknowledge_archive_extract_outcome", { operationId, effectsReviewed: true }));
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
      succeeded = await action(() => invoke("move_queue_entry", {
        operationId,
        delta: selectedAction === "up" ? -1 : 1,
      }));
    } else if (selectedAction === "remove") {
      succeeded = await action(() => invoke("remove_queue_entry", { operationId }));
    } else if (selectedAction === "retry") {
      succeeded = await action(() => invoke("retry_queue_entry", { operationId }));
    } else if (selectedAction === "discard-partial") {
      const choice = await choiceModal("Teilkopie verwerfen?",
        "<p>Doppeldecker löscht den gesicherten Abschnitt und schließt den Auftrag. Die Quelldatei und der endgültige Zielname bleiben erhalten.</p>",
        [{ label: "Abbrechen", value: false }, { label: "Teilkopie löschen", value: true, cls: "danger" }]);
      if (choice) await action(() => invoke("discard_resumable_copy", { operationId }));
      await restoreAfterQueueModal(selectedFocus);
      return;
    } else if (selectedAction === "replan-discard") {
      succeeded = await action(() => invoke("discard_replan_required_queue_entry", { operationId }));
    } else if (selectedAction === "created-dacl-recovery") {
      const confirmed = await confirmModal("Rechte neuer Ordner prüfen und abschließen", "Es werden nur Identität und aktueller Rechtezustand geprüft. Der Eintrag kann nur bei einem bekannten Vorher- oder Nachherzustand geschlossen werden. Doppeldecker setzt keine Rechte erneut, stellt keinen früheren Zustand wieder her und setzt keine Kinderarbeit fort.");
      if (confirmed) await action(() => invoke("acknowledge_created_directory_dacl_outcome", { operationId, possibleEffectReviewed: true }));
      await restoreAfterQueueModal(selectedFocus); return;
    } else if (selectedAction === "file-dacl-recovery") {
      const confirmed = await confirmModal("Objektweite Dateirechte prüfen und abschließen",
        "Es werden nur Identität und aktueller Rechtezustand geprüft. Der Eintrag kann nur bei einem bekannten Vorher- oder Nachherzustand geschlossen werden. Die Rechteänderung betrifft alle bestehenden und neuen Namen dieses Dateiobjekts, auch außerhalb der ausgewählten Ordner. Die Liste zeigt nur die hier bekannten Namen. Doppeldecker setzt keine Rechte erneut und stellt keinen früheren Zustand wieder her.");
      if (confirmed) await action(() => invoke("acknowledge_file_dacl_outcome", { operationId, possibleEffectReviewed: true }));
      await restoreAfterQueueModal(selectedFocus); return;
    } else if (selectedAction === "dacl-recovery") {
      const confirmed = await confirmModal("Ordnerrechte prüfen und abschließen",
        "Doppeldecker liest den aktuellen Rechtezustand der gebundenen Ordner frisch. Wenn er einem gespeicherten Vorher- oder Nachherzustand entspricht, wird der Eintrag ohne erneutes Setzen geschlossen. Andere Zustände bleiben zur Prüfung offen.");
      if (confirmed) await action(() => invoke("acknowledge_directory_dacl_outcome", { operationId, possibleEffectReviewed: true }));
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
      succeeded = await action(() => invoke("acknowledge_legacy_webdav_delete", {
        operationId,
        possibleEffectReviewed,
      }));
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

  /** @param {QueueEntry} entry */
  async function acknowledgeUnclearOutcome(entry, selectedFocus) {
    const remainingTemp = entry.operation.type === "RenameCycleRecovery"
      ? " Ein noch vorhandener Zwischenname bleibt erhalten und sperrt weiterhin den davon betroffenen Sync. Erst nach bewusstem Sichern oder Auflösen außerhalb der Sync-Wurzeln wird dieser wieder planbar." : "";
    const confirmed = await confirmModal(
      "Unklaren Ausgang schließen",
      "Die letzte Mutation kann bereits ganz oder teilweise gewirkt haben. Bestätigen Sie nur, wenn Quelle, Ziel und angezeigte Recoverypfade manuell geprüft wurden. Doppeldecker wiederholt die Operation nicht." + remainingTemp,
    );
    if (confirmed) {
      const succeeded = await action(() => invoke("acknowledge_unclear_queue_outcome", {
        operationId: entry.operation_id,
        possibleEffectReviewed: true,
      }));
      if (succeeded) {
        showDock(`<span class="pill none">Manuell geprüft</span>Der unklare Lauf wurde ohne Wiederholung geschlossen.`, { closable: true });
      }
    }
    await restoreAfterQueueModal(selectedFocus);
  }

  /** @param {QueueEntry} entry */
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
      const succeeded = await action(() => invoke("discard_queue_confirmation", {
        operationId: entry.operation_id,
      }));
      if (succeeded) {
        const dockState = snapshot.paused
          ? "Die Queue bleibt pausiert."
          : "Die nächste wartende Operation kann anlaufen.";
        showDock(`<span class="pill none">Verworfen</span>Dieser Versuch wurde vor seiner ersten Mutation geschlossen. ${dockState}`, { closable: true });
      }
    }
    await restoreAfterQueueModal(selectedFocus);
  }

  /** @param {QueueEntry} entry */
  async function approveEntry(entry, selectedFocus) {
    const attention = structuredClone(entry.attention);
    if (!attention) {
      showError("Queue-Fehler", { code: "plan_drift", message: "Für diesen Auftrag fehlt die zu bestätigende Wirkung." });
      return false;
    }
    if (entry.operation.type === "BidirectionalForward" && attention?.type === "RemoteRun") {
      try { return await recoverBidirectional(entry); }
      finally { await restoreAfterQueueModal(selectedFocus); }
    }
    if (entry.operation.type === "ContentSync" && attention?.type === "RemoteRun") {
      try {
        const preview = await invoke("content_sync_queue_preview", { operationId: entry.operation_id });
        const confirmation = await confirmContentSync(preview);
        if (!confirmation) return false;
        return await action(() => invoke("approve_content_sync_queue_entry", {
          operationId: entry.operation_id,
          preparationDigest: preview.digest,
          effectsConfirmed: confirmation.effectsConfirmed,
          removalPhrase: confirmation.removalPhrase,
        }));
      } catch (error) { showError("Dateisync", error); return false; }
      finally { await restoreAfterQueueModal(selectedFocus); }
    }
    if (entry.operation.type === "LocalSync" && attention?.type === "RemoteRun") {
      try {
        const preview = await invoke("local_sync_queue_preview", { operationId: entry.operation_id });
        if (!await confirmLocalSync(preview)) return false;
        return await action(() => invoke("approve_local_sync_queue_entry", {
          operationId: entry.operation_id, preparationDigest: preview.plan_digest,
          replacementsConfirmed: true,
        }));
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
      const succeeded = await action(() => approveRemoteQueueEntry(entry.operation.type, {
        operationId: entry.operation_id, effectsConfirmed: true,
      }));
      await restoreAfterQueueModal(selectedFocus);
      return succeeded;
    }
    if (attention?.type === "ArchiveExtractChanged") {
      if (!attention.current.preview.catalog_complete && !archiveOperation(entry).request.options.allow_partial) {
        showError("Archiv entpacken", new Error("Das Archiv ist jetzt unvollständig lesbar. Bitte diesen Auftrag verwerfen und den bekannten Teil ausdrücklich neu auswählen."));
        await restoreAfterQueueModal(selectedFocus); return false;
      }
      const confirmed = await confirmArchiveExtractPreview(attention.current.preview, { overwrite: archiveOperation(entry).request.options.overwrite, ...(entry.operation.type === "ArchiveUpload" ? {upload:true,targetLabel:endpointLabel(entry.operation.target,true)} : {}) });
      if (!confirmed) { await restoreAfterQueueModal(selectedFocus); return false; }
      const succeeded = await action(() => invoke("approve_queue_entry", { operationId: entry.operation_id, expectedAttention: attention, effectsConfirmed: true }));
      await restoreAfterQueueModal(selectedFocus); return succeeded;
    }
    if (attention?.type === "ArchivePackChanged") {
      const confirmed = await confirmArchivePackPreview(attention.current.preview, entry.operation.type === "ArchiveUpload" ? {upload:true,targetLabel:endpointLabel(entry.operation.target,true)} : {});
      if (!confirmed) { await restoreAfterQueueModal(selectedFocus); return false; }
      const succeeded = await action(() => invoke("approve_queue_entry", { operationId: entry.operation_id,
        expectedAttention: attention, effectsConfirmed: true }));
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
    const succeeded = await action(() => invoke("approve_queue_entry", {
      operationId: entry.operation_id,
      expectedAttention: attention,
      effectsConfirmed: true,
    }));
    if (succeeded) {
      showDock(`<span class="pill ok">Neu bestätigt</span>Der sichtbare Queue-Plan wurde übernommen und wird vor Mutation nochmals geprüft.`, { closable: true });
    }
    await restoreAfterQueueModal(selectedFocus);
    return succeeded;
  }

  /**
   * @param {string} operationType
   * @param {{operationId: string, effectsConfirmed: boolean}} args
   */
  function approveRemoteQueueEntry(operationType, args) {
    switch (operationType) {
      case "Bidirectional": return invoke("approve_bidirectional_queue_entry", args);
      case "ProviderSync": return invoke("approve_provider_queue_entry", args);
      case "FtpsSync": return invoke("approve_ftps_queue_entry", args);
      case "WebDavSync": return invoke("approve_webdav_queue_entry", args);
      case "SmbSync": return invoke("approve_smb_queue_entry", args);
      default: return invoke("approve_sftp_queue_entry", args);
    }
  }

  function warnBeforeExit(value) {
    const generation = ++exitGeneration;
    const unreadable = Array.isArray(value?.unreadableQueues)
      ? value.unreadableQueues.filter(label => typeof label === "string") : [];
    const reported = Number(value?.openEntries);
    const count = Number.isSafeInteger(reported) && reported >= 0
      ? reported : Math.max(0, openCount());
    const confirmationToken = value?.token;
    if (typeof confirmationToken !== "string" || !confirmationToken) {
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
    void action(() => invoke("set_queue_paused", { paused: !snapshot.paused }));
  });
  $("#queue-toggle-stop").addEventListener("click", () => {
    void action(() => invoke("set_queue_stop_on_error", { stop: !snapshot.stop_on_error }));
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

  return { applySnapshot, close, open, refresh, settleGeneratedSource, warnBeforeExit };
}
