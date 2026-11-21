// @ts-check

// Vorschau, Konfliktentscheidung und Queuecommit für eine einzelne Übertragung.
import { conflictChoiceModal } from "./conflict-dialog.js";
import { invoke } from "./tauri-api.js";
import { escapeHtml, errCode, sourceReadReport } from "./ui-utils.js";

/** @typedef {import("./file-transfer-wire.js").ProviderFileRequest} ProviderFileRequest */
/** @typedef {import("./file-transfer-wire.js").ProviderFilePolicy} ProviderFilePolicy */
/** @typedef {import("./file-transfer-wire.js").FileTransferChoice} FileTransferChoice */

/** @param {unknown} value @returns {value is FileTransferChoice} */
function isTransferChoice(value) {
  return value === "skip" || value === "overwrite" || value === "both";
}

/** @param {import("./file-transfer-wire.js").ProviderFilePreviewWire} value */
function providerConflictChoices(value) {
  const canReplace = value.targetCapabilities.replace || value.targetCapabilities.replaceBestEffort;
  const bestEffort = !value.targetCapabilities.replace && value.targetCapabilities.replaceBestEffort;
  return [
    { label: "Überspringen", value: "skip", explanation: "Vorhandene Ziele und übersprungene Move-Quellen bleiben erhalten. Nur übrige Einträge werden übertragen." },
    ...(canReplace ? [{ label: bestEffort ? "Dauerhaft ersetzen" : "Ersetzen", value: "overwrite", danger: true,
      explanation: bestEffort ? "Die angezeigten Zieldateien werden ohne Quarantäne dauerhaft ersetzt (Best Effort). Ein unklarer Ausgang wird nicht automatisch wiederholt."
        : "Zieldateien durch die Quelle ersetzen. Ordner werden zusammengeführt; zusätzliche Zielinhalte bleiben erhalten." }] : []),
    { label: "Beide behalten", value: "both", wide: true, disabled: !value.keepBothPreparationId,
      explanation: value.keepBothPreparationId ? `Neue Namen: ${value.keepBothNames.join("; ")}` : value.keepBothBlocker || "Für diese Auswahl nicht verfügbar." },
  ];
}

/** @typedef {import("./pane-endpoint.js").PaneEndpointLocation & {profileId?: string | null}} TransferEndpoint */

/**
 * @template {{endpointOnly?: boolean}} Action
 * @param {{
 * paneEndpoint: typeof import("./pane-endpoint.js").paneEndpoint,
 * actionIsCurrent: (action: Action) => boolean, running: () => boolean,
 * showDock: (html: string, options: {closable: boolean}) => void,
 * showError: (title: string, error: unknown) => void, dockRevision: () => number
 * }} callbacks
 */
export function createFileTransfer({ paneEndpoint, actionIsCurrent, running, showDock, showError, dockRevision }) {
  /** Rückgabe: `true` eingereiht, `"skipped"` Quelle ist der Zielordner,
   * für Finder-Drops ein Sperrgrund/Fehler, sonst `undefined` bei Abbruch/Drift.
   * @param {"copy" | "move"} kind
   * @param {TransferEndpoint} source
   * @param {TransferEndpoint} target
   * @param {string[]} items
   * @param {Action} action
   * @param {((id: string) => void) | null} [onEnqueued]
   * @param {{operationId: string, label: string} | null} [generated]
   */
  return async function transfer(kind, source, target, items, action, onEnqueued, generated = null) {
    const destDir = target.path;
    if (!generated && (!items.length || !destDir)) return;
    const sameEndpoint = source.provider === target.provider
      && source.profileId === target.profileId
      && destDir === source.path;
    if (!generated && sameEndpoint) {
      showDock('<span class="pill delete">Quelle und Ziel sind derselbe Ordner</span>', { closable: true });
      return "skipped";
    }

    let overwrite = false;
    /** @type {string[]} */
    let expectedConflicts = [];
    /** @type {string | null} */
    let keepBothPreparationId = null;
    /** @type {string | null} */
    let conflictPreparationId = null;
    let copyNewBestEffortExpected;
    let replaceBestEffortExpected;
    let removeSourceBestEffortExpected;
    /** @type {string[]} */
    const bestEffortNotes = [];
    let sourceReport = {summary:"",details:""};
    /** @param {string[]} notes @param {string} fallback */
    const addBestEffortNotes = (notes, fallback) => {
      const values = notes?.length ? notes : [fallback];
      for (const note of values) {
        if (note && !bestEffortNotes.includes(note)) bestEffortNotes.push(note);
      }
    };
    /** @type {ProviderFileRequest} */
    const providerOperation = generated ? {
      kind: "copy", source: { provider: "local", root: "" }, target: { provider: "local", root: "" },
      items: [], destination: "", generatedOperationId: generated.operationId,
    } : {
      kind,
      source: paneEndpoint(source),
      target: paneEndpoint(target),
      items,
      destination: destDir,
    };
    const providerTransfer = Boolean(generated) || source.provider !== "local" || target.provider !== "local";
    // Finder-Drops ergänzen ihre Gruppenbilanz, bevor sie den konkreten Grund
    // anzeigen. Eine zweite allgemeine Dockmeldung würde ihn sonst überschreiben.
    /** @param {string} blockerHtml */
    function blocked(blockerHtml) {
      if (!action.endpointOnly) showDock(blockerHtml, { closable: true });
      return { blockerHtml };
    }
    /** @param {unknown} error */
    function preparationWasCancelled(error) {
      if (errCode(error) !== "interrupted") return false;
      if (actionIsCurrent(action)) showDock('<span class="pill none">Abgebrochen</span>Die Datei-Vorbereitung wurde beendet.', { closable: true });
      return true;
    }
    let intentAttempted = false;
    async function enqueueIntent() {
      if (!actionIsCurrent(action)) return;
      showDock('<span class="pill copy">Weitere Übertragung</span>Die Queue prüft Verbindung, Fähigkeiten und Konflikte vor der Ausführung. Vorhandene Ziele werden nicht ersetzt. Neuanlegen kann als Best Effort ohne atomaren Kollisionsschutz erfolgen.'
        + (source.provider === "sftp" || target.provider === "sftp" ? ' SFTP-Dateiaktionen laufen als Best Effort: Pfadbefehle sind nicht atomar an den Pane-Root gebunden. Ein unbemerkter gleichzeitiger Elternpfadtausch kann auch außerhalb des gewählten Bereichs im selben Serverkonto wirken.' : '')
        + (kind === "move" ? ' Passende SFTP-Panes derselben Verbindung verwenden gewöhnliches Rename. Sonst wird die Quelle nach vollständiger Zielkopie und frischem Readback entfernt; die Entfernung kann dauerhaft sein.' : ''), { closable: true });
      intentAttempted = true;
      const { generatedOperationId: _, ...request } = providerOperation;
      const id = await invoke("enqueue_provider_file_intent", { request });
      onEnqueued?.(id);
      return true;
    }
    try {
      if (providerTransfer && running()) {
        if (generated) throw new Error("Ein anderer Auftrag läuft. Die offene Archivquelle bleibt erhalten; Konflikte anschließend erneut prüfen.");
        return await enqueueIntent();
      }
      let preview = providerTransfer
        ? await invoke("check_provider_file_operation", { request: providerOperation })
        : await invoke("check_local_file_operation", { kind, items, destDir });
      if (!actionIsCurrent(action)) return;
      /** @type {ProviderFilePolicy | null} */
      let preparedConflictPolicy = null;
      /** @param {ProviderFilePolicy} policy */
      async function prepareConflictPolicy(policy) {
        const id = preview.conflictPreparationId;
        if (id == null) throw new Error("Die Bindung der Konfliktvorschau fehlt.");
        const prepared = await invoke("check_provider_file_operation", {
          request: providerOperation,
          choice: /** @satisfies {import("./file-transfer-wire.js").ProviderFileChoiceRequest} */ ({ id, policy }),
        });
        if (!actionIsCurrent(action)) return false;
        preview = prepared;
        preparedConflictPolicy = policy;
        return true;
      }
      /** @type {FileTransferChoice | null} */
      let selectedConflictChoice = null;
      if (preview?.requiresConflictChoice) {
        /** @type {Promise<boolean> | undefined} */
        let preparation;
        const decision = await conflictChoiceModal({
          title: kind === "copy" ? "Konflikte beim Kopieren" : "Konflikte beim Verschieben",
          conflicts: preview.conflictDetails,
          choices: [
            { label: "Überspringen", value: "skip", explanation: "Vorhandene Ziele und übersprungene Move-Quellen bleiben erhalten." },
            { label: "Ersetzen", value: "overwrite", disabled: true, explanation: "Verfügbarkeit wird geprüft …" },
            { label: "Beide behalten", value: "both", disabled: true, wide: true, explanation: "Neue Namen werden vorbereitet …" },
          ],
          sourceReadLimitations: preview.sourceReadLimitations || [],
          note: `${preview.conflicts.length} Konflikt(e). Die Entscheidung gilt für die gesamte Auswahl.`,
          loadChoices: () => {
            preparation = prepareConflictPolicy("inspect");
            return preparation.then(current => current ? ({ choices: providerConflictChoices(preview), replacementBlockers: preview.targetCapabilities.blockers || [] }) : null);
          },
        });
        if (!actionIsCurrent(action) || !isTransferChoice(decision)) return;
        selectedConflictChoice = decision;
        // Die laufende Vorbereitung hält die Fensterschranke. Erst nach ihrem
        // Ende kann die endgültige Entscheidung gebunden werden.
        await preparation?.catch(() => {});
        if (!actionIsCurrent(action)) return;
        if (selectedConflictChoice === "skip") {
          if (!await prepareConflictPolicy("skip")) return;
        }
      }
      for (;;) {
        bestEffortNotes.length = 0;
        copyNewBestEffortExpected = undefined;
        replaceBestEffortExpected = undefined;
        removeSourceBestEffortExpected = undefined;
        if (preview) {
          sourceReport = sourceReadReport(preview.sourceReadLimitations, preview.sourceTree);
          // Diese beiden Werte stammen ausschließlich aus der sichtbaren Vorschau.
          // Der zweite Backend-Probelauf darf sie nicht selbst auf Best Effort
          // hochsetzen, weil das eine nie angezeigte Bestätigung wäre.
          copyNewBestEffortExpected = !preview.targetCapabilities.copyNew
            && preview.targetCapabilities.copyNewBestEffort;
          replaceBestEffortExpected = false;
        }
        if (preview && !preview.skipOnly && !preview.targetCapabilities.copyNew
          && !preview.targetCapabilities.copyNewBestEffort) {
          return blocked(
            `<span class="pill none">Kopieren gesperrt</span>${escapeHtml(preview.targetCapabilities.blockers.join("; "))}`,
          );
        }
        if (preview && !preview.skipOnly && !preview.targetCapabilities.copyNew
          && preview.targetCapabilities.copyNewBestEffort) {
          // Best Effort ist ein reguläres Ergebnis: Die konkrete Einschränkung wird
          // in der Einreih-Meldung sichtbar, ohne eine modale Frage je Übertragung.
          addBestEffortNotes(
            preview.targetCapabilities.limitations,
            "ohne atomaren Kollisionsschutz beim Commit",
          );
        }
        const selectedReplacementUnavailable = preparedConflictPolicy === "overwrite"
          && !preview.targetCapabilities.replace && !preview.targetCapabilities.replaceBestEffort;
        if (providerTransfer && preview && !preview.skipOnly && !selectedReplacementUnavailable && preview.sourceRemovalRequired !== false && kind === "move") {
          const sourceCapabilities = preview.sourceCapabilities;
          if (!sourceCapabilities?.removeSource
            && !sourceCapabilities?.removeSourceBestEffort) {
            return blocked(
              '<span class="pill none">Verschieben gesperrt</span>Die Zielkopie wäre möglich, aber die Quellentfernung ist für diese Verbindung nicht gebunden. Es erfolgt kein stiller Copy-Fallback.',
            );
          }
          removeSourceBestEffortExpected = !sourceCapabilities.removeSource
            && sourceCapabilities.removeSourceBestEffort;
          if (removeSourceBestEffortExpected) {
            addBestEffortNotes(
              sourceCapabilities.limitations,
              "Quelle wird nach frischem Readback einzeln entfernt",
            );
          }
        }
        if (kind === "move" && preview?.sourceRemovalRequired === false && !preview.skipOnly) {
          addBestEffortNotes(["Quellbäume mit übersprungenen Einträgen bleiben vollständig erhalten."], "Quelle bleibt wegen übersprungener Einträge erhalten");
        }
        const conflicts = preview?.conflicts
          || await invoke("check_conflicts", { items, destDir });
        if (!actionIsCurrent(action)) return;
        expectedConflicts = conflicts.slice();
        conflictPreparationId = preview.conflictPreparationId;
        if (conflicts.length) {
          const canReplace = preview.targetCapabilities.replace || preview.targetCapabilities.replaceBestEffort;
          const bestEffortReplace = !preview.targetCapabilities.replace && preview.targetCapabilities.replaceBestEffort;
          const choices = [
            { label: "Überspringen", value: "skip", explanation: "Vorhandene Ziele bleiben unverändert. Nur die übrigen Einträge werden übertragen; übersprungene Move-Quellen bleiben erhalten." },
            ...(canReplace ? [{ label: bestEffortReplace ? "Dauerhaft ersetzen" : "Ersetzen", value: "overwrite", danger: true,
              explanation: bestEffortReplace
                ? "Die angezeigten Zieldateien werden ohne Quarantäne unwiederbringlich verworfen (Best Effort). Ein unklarer Ausgang wird nicht automatisch wiederholt."
                : "Die angezeigten Zieldateien durch die Quelle ersetzen. Ordner werden zusammengeführt; zusätzliche Zielinhalte bleiben erhalten." }] : []),
            { label: "Beide behalten", value: "both", wide: true, disabled: !preview.keepBothPreparationId,
              explanation: preview.keepBothPreparationId
                ? `Die Quelle unter den angezeigten neuen Namen übertragen. Bei Ordnern wird die vorbereitete ausgewählte Kopie daneben angelegt. Neue Namen: ${preview.keepBothNames.join("; ")}`
                : preview.keepBothBlocker || "Für diese Auswahl ist kein kollisionsfreier Commit gebunden." },
          ];
          const choice = selectedConflictChoice || (preparedConflictPolicy === "skip" ? "skip" : await conflictChoiceModal({
            title: kind === "copy" ? "Konflikte beim Kopieren" : "Konflikte beim Verschieben",
            conflicts: preview.conflictDetails, choices,
            replacementBlockers: canReplace ? [] : (preview.targetCapabilities.blockers || []),
            sourceReadLimitations: preview.sourceReadLimitations || [],
            note: `${conflicts.length} Konflikt(e). Die Entscheidung gilt für die gesamte Auswahl. Quelle und Ziel werden unmittelbar vor der Ausführung erneut geprüft.`,
          }));
          if (!actionIsCurrent(action) || !isTransferChoice(choice)) return;
          if (preparedConflictPolicy !== null && preparedConflictPolicy !== "inspect" && choice !== preparedConflictPolicy) {
            if (!await prepareConflictPolicy(choice)) return;
            continue;
          }
          overwrite = choice === "overwrite";
          if (choice === "both") keepBothPreparationId = preview.keepBothPreparationId;
          replaceBestEffortExpected = overwrite && bestEffortReplace;
          if (overwrite && bestEffortReplace) addBestEffortNotes([], "Altversion ohne Quarantäne dauerhaft ersetzt");
        }
        break;
      }

    } catch (error) {
      if (preparationWasCancelled(error)) return;
      if (providerTransfer && !generated && !intentAttempted && errCode(error) === "busy") {
        try { return await enqueueIntent(); }
        catch (enqueueError) { error = enqueueError; }
      }
      if (actionIsCurrent(action) && action.endpointOnly) return { error };
      if (actionIsCurrent(action)) showError("Fehler", error);
      return;
    }

    try {
      // Zwischen Vorschau, Konfliktdialog und Mutation kann beliebig viel Zeit
      // liegen. Unmittelbar vor dem schreibenden Command gilt deshalb dieselbe
      // Scope-Prüfung noch einmal.
      if (!actionIsCurrent(action)) return;
      const presentation = dockRevision();
      if (providerTransfer) {
        const id = await invoke("enqueue_provider_file_operation", {
          request: /** @satisfies {import("./file-transfer-wire.js").EnqueueProviderFileRequest} */ ({
            ...providerOperation,
            overwrite,
            expectedConflicts,
            keepBothPreparationId,
            conflictPreparationId,
            copyNewBestEffortExpected,
            replaceBestEffortExpected,
            ...(removeSourceBestEffortExpected === undefined
              ? {}
              : { removeSourceBestEffortExpected }),
          }),
        });
        onEnqueued?.(id);
      } else {
        const id = await invoke(kind === "copy" ? "copy_paths" : "move_paths", { request: {
          items, destDir, overwrite, expectedConflicts, keepBothPreparationId, conflictPreparationId, replaceBestEffortExpected,
        } });
        onEnqueued?.(id);
      }
      // Der erfolgreiche Queuecommit bleibt auch bei späterer Zielnavigation
      // eingereiht. Nur die alte Dockanzeige entfällt; die Dropbilanz zählt ihn.
      // Ein Worker kann schon vor der IPC-Antwort Fortschritt oder Abschluss
      // melden. Dessen neuere Anzeige darf nicht wieder „Eingereiht“ werden.
      if (!actionIsCurrent(action) || dockRevision() !== presentation) return true;
      const bestEffortDetail = bestEffortNotes
        .map(note => ` · Best Effort: ${note}`)
        .join("");
      showDock(
        `<span class="pill copy">Eingereiht</span>${generated ? escapeHtml(generated.label) + " · Upload fortgesetzt" : items.length + " Eintrag/Einträge"} · Queue arbeitet persistent in Reihenfolge${sourceReport.summary ? ` · ${sourceReport.summary}` : ""}${escapeHtml(bestEffortDetail)}${sourceReport.details}`,
        { closable: true },
      );
      return true;
    } catch (error) {
      if (preparationWasCancelled(error)) return;
      if (actionIsCurrent(action) && action.endpointOnly) return { error };
      if (actionIsCurrent(action)) showError("Fehler", error);
    }
  };
}
