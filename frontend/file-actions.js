import { conflictChoiceModal } from "./conflict-dialog.js";
// Interaktive Standard-Dateiaktionen. Der Controller hält keinen eigenen Pane-Zustand;
// er erhält den jeweils aktuellen Zustand und die sichere Backendbrücke von außen.

import { invoke } from "./tauri-api.js";
import { choiceModal, promptModal } from "./modal.js";
import { escapeHtml, isEntryNameSafe, joinPath, errCode, sourceReadReport } from "./ui-utils.js";

export function createFileActions({ panes, activeSide, running, enqueueBlocked = running, loadPane, showDock, showError, dockRevision = () => 0 }) {
  let actionGeneration = 0;

  function directMutationBlocked(action) {
    if (!running()) return false;
    showDock(`<span class="pill none">${escapeHtml(action)} derzeit belegt</span>` +
      "Diese direkte Dateiaktion ist nach dem laufenden Auftrag wieder verfügbar. " +
      "Navigation sowie weitere Copy-/Move-Aufträge bleiben erreichbar.", { closable: true });
    return true;
  }

  /** Ein Dateiaktions-Scope ist ein unveränderliches Abbild genau der Pane, die
   * der Nutzer gesehen hat. Nur Navigation entwertet die Absicht; ein passiver
   * Refresh während eines Queueabschlusses besitzt dieselbe Navigationsrevision. */
  function paneScope(side) {
    const pane = panes[side];
    return Object.freeze({
      side,
      navigationRevision: pane.navigationRevision,
      provider: pane.provider,
      profileId: pane.profileId,
      profileRevision: pane.profileRevision,
      path: pane.path,
    });
  }

  function sameScope(scope) {
    const pane = panes[scope.side];
    return pane.navigationRevision === scope.navigationRevision
      && sameEndpoint(pane, scope);
  }

  function sameEndpoint(pane, scope) {
    return pane.provider === scope.provider
      && pane.profileId === scope.profileId
      && pane.profileRevision === scope.profileRevision
      && pane.path === scope.path;
  }

  function beginAction(scopes) {
    return Object.freeze({ generation: ++actionGeneration, scopes });
  }

  /** Aktion mit festem Ziel und Navigationsrevision, unabhängig von der
   * Aktionsgeneration und einem passiven `loadSeq`. Ein Finder-Drop aus
   * mehreren Quellordnern reiht seine Gruppen nacheinander ein; schließt die Queue
   * die erste Gruppe ab, lädt `fileop-done` beide Panes neu und hätte die
   * strenge Aktion still entwertet — die restlichen Gruppen wären ohne Meldung
   * verloren. Das Backend prüft die erwarteten Konflikte vor jedem Schreiben
   * ohnehin erneut. Echte Navigation entwertet dagegen die verbleibenden Gruppen,
   * auch wenn danach wieder derselbe Zielordner geöffnet wird. */
  function beginEndpointAction(scope) {
    const endpointScope = Object.freeze({
      side: scope.side,
      provider: scope.provider,
      profileId: scope.profileId,
      profileRevision: scope.profileRevision,
      path: scope.path,
      navigationRevision: scope.navigationRevision,
    });
    actionGeneration += 1;
    return Object.freeze({ scopes: [endpointScope], endpointOnly: true });
  }

  function sameEndpointScope(scope) {
    return sameEndpoint(panes[scope.side], scope)
      && panes[scope.side].navigationRevision === scope.navigationRevision;
  }

  function actionIsCurrent(action) {
    if (action.endpointOnly) return action.scopes.every(sameEndpointScope);
    return action.generation === actionGeneration && action.scopes.every(sameScope);
  }

  /** Auswahl der Pane; ohne Mehrfachauswahl gilt der Eintrag unter dem Cursor. */
  function selectedEntries(side) {
    const pane = panes[side];
    const indices = pane.sel.size
      ? [...pane.sel]
      : pane.entries[pane.cursor]
        ? [pane.cursor]
        : [];
    return indices.map((index) => pane.entries[index]).filter(Boolean);
  }

  function safeSelectedEntries(side, action) {
    const entries = selectedEntries(side);
    if (entries.some((entry) => !isEntryNameSafe(entry))) {
      showDock(
        `<span class="pill delete">Unsicherer Dateiname</span>${escapeHtml(action)} ist für Namen ohne eindeutige UTF-8-Abbildung gesperrt. Der Eintrag bleibt unverändert.`,
        { closable: true },
      );
      return [];
    }
    return entries;
  }

  function paneEndpoint(scope) {
    const endpoint = { provider: scope.provider, root: scope.path };
    if (scope.provider !== "local") {
      endpoint.profileId = scope.profileId;
      endpoint.profileRevision = scope.profileRevision;
    }
    return endpoint;
  }

  async function refreshBothPanes(context) {
    const requests = ["left", "right"].map((side) => {
      const scope = paneScope(side);
      const selected = panes[side].entries[panes[side].cursor];
      return { scope, keepSelName: selected ? selected.name : null };
    });
    const affected = context?.affectedEndpoints;
    await Promise.all(requests.map(({ scope, keepSelName }) => {
      if (!sameScope(scope) || !scope.path) {
        return Promise.resolve();
      }
      if (Array.isArray(affected) && !affected.some(endpoint => (
        endpoint.provider === scope.provider
        && endpoint.profileId === scope.profileId
        && endpoint.profileRevision === scope.profileRevision
        && (scope.path === endpoint.path || scope.path.startsWith(endpoint.path.replace(/\/$/, "") + "/"))
      ))) {
        return Promise.resolve();
      }
      return loadPane(scope.side, scope.path, {
        passive: true,
        push: false,
        keepSelName,
        preserveView: true,
      });
    }));
  }

  async function refreshMatchingPanes(mutationScope, keepSelName = null) {
    // Ab dem schreibenden IPC-Aufruf ist eine Dateiaktion nicht mehr abbrechbar.
    // Ein später Abschluss darf keine inzwischen anders verwendete Pane
    // zurücknavigieren. Panes, die JETZT noch denselben Endpunkt und Ordner
    // zeigen, lesen dagegen den tatsächlichen Bestand neu — auch wenn eine andere
    // Aktion den ursprünglichen Generationstoken inzwischen abgelöst hat.
    const matching = ["left", "right"]
      .map(paneScope)
      .filter(scope => sameEndpoint(scope, mutationScope));
    await Promise.all(matching.map(scope => (
      sameScope(scope)
        ? loadPane(scope.side, scope.path, { passive: true, push: false, keepSelName })
        : Promise.resolve()
    )));
  }

  async function transferToOther(kind) {
    if (enqueueBlocked()) return;
    const side = activeSide();
    const destinationSide = side === "left" ? "right" : "left";
    const source = paneScope(side);
    const target = paneScope(destinationSide);
    const entries = safeSelectedEntries(side, kind === "copy" ? "Kopieren" : "Verschieben");
    return transfer(kind, source, target, entries.map(entry => joinPath(source.path, entry.name)),
      beginAction([source, target]));
  }

  function captureDrag(side, index) {
    const pane = panes[side];
    const entries = pane.sel.has(index) ? selectedEntries(side) : [pane.entries[index]];
    if (!entries.length || entries.some(entry => !entry || !isEntryNameSafe(entry))) return null;
    return { source: paneScope(side), items: entries.map(entry => joinPath(pane.path, entry.name)) };
  }

  async function copyDrop(side, drag, paths = []) {
    if (enqueueBlocked()) return;
    const target = paneScope(side);
    if (drag) {
      if (!sameScope(drag.source)) return;
      return transfer("copy", drag.source, target, drag.items, beginAction([drag.source, target]));
    }
    return pastePaths("copy", target, paths, undefined, "Drop");
  }

  async function pastePaths(kind, target, paths, onEnqueued, origin = "Zwischenablage") {
    if (enqueueBlocked() || !sameScope(target)) return false;
    // Finder kann Dateien aus mehreren Ordnern liefern. Jeder Elternordner bleibt
    // ein eigener Quell-Root; die bestehende Backendprüfung bindet die echten Pfade.
    const groups = new Map();
    for (const path of new Set(paths)) {
      if (typeof path !== "string" || !path.startsWith("/") || path.endsWith("/")) continue;
      const parent = path.slice(0, path.lastIndexOf("/")) || "/";
      if (!groups.has(parent)) groups.set(parent, []);
      groups.get(parent).push(path);
    }
    if (groups.size === 0) {
      showDock(
        `<span class="pill delete">${escapeHtml(origin)} nicht eingereiht</span>Die Übergabe enthielt keine regulären absoluten Dateipfade.`,
        { closable: true },
      );
      return;
    }
    const action = beginEndpointAction(target);
    let handled = 0;
    let unchangedMove = false;
    let stopped;
    for (const [path, items] of groups) {
      if (!actionIsCurrent(action)) break;
      const source = { provider: "local", profileId: null, path };
      const result = await transfer(kind, source, target, items, action, onEnqueued);
      // „skipped" (Quelle ist der Zielordner) lässt die übrigen Gruppen weiterlaufen;
      // Abbruch, Sperre oder Fehler beenden den Drop für alle folgenden Gruppen.
      if (result !== true && result !== "skipped") {
        stopped = result;
        break;
      }
      if (kind === "move" && result === "skipped") unchangedMove = true;
      handled += 1;
    }
    const remaining = groups.size - handled;
    if (remaining > 0) {
      // Ein stiller Teilabbruch wäre Datenverlust aus Nutzersicht: sichtbar machen,
      // wie viele Quellordner dieses Drops nicht eingereiht wurden.
      const detail = `${remaining} von ${groups.size} Quellordnern wurden nicht eingereiht.`;
      if (stopped?.error) {
        showError(`Übertragung unvollständig · ${detail}`, stopped.error);
      } else {
        showDock(
          `<span class="pill delete">${escapeHtml(origin)} unvollständig</span>${detail}${stopped?.blockerHtml ? `<br>${stopped.blockerHtml}` : ""}`,
          { closable: true },
        );
      }
    }
    return remaining === 0 && !unchangedMove;
  }

  /** Rückgabe: `true` eingereiht, `"skipped"` Quelle ist der Zielordner,
   * für Finder-Drops ein Sperrgrund/Fehler, sonst `undefined` bei Abbruch/Drift. */
  async function transfer(kind, source, target, items, action, onEnqueued, generated = null) {
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
    let expectedConflicts = [];
    let keepBothPreparationId = null;
    let conflictPreparationId = null;
    let copyNewBestEffortExpected;
    let replaceBestEffortExpected;
    let removeSourceBestEffortExpected;
    const bestEffortNotes = [];
    let sourceReport = {summary:"",details:""};
    const addBestEffortNotes = (notes, fallback) => {
      const values = notes?.length ? notes : [fallback];
      for (const note of values) {
        if (note && !bestEffortNotes.includes(note)) bestEffortNotes.push(note);
      }
    };
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
    function blocked(blockerHtml) {
      if (!action.endpointOnly) showDock(blockerHtml, { closable: true });
      return { blockerHtml };
    }
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
      const id = await invoke("enqueue_provider_file_intent", { request: providerOperation });
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
      let preparedConflictPolicy = null;
      async function prepareConflictPolicy(policy) {
        const prepared = await invoke("check_provider_file_operation", {
          request: providerOperation,
          choice: { id: preview.conflictPreparationId, policy },
        });
        if (!actionIsCurrent(action)) return false;
        preview = prepared;
        preparedConflictPolicy = policy;
        return true;
      }
      let selectedConflictChoice = null;
      const conflictChoices = value => {
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
      };
      if (preview?.requiresConflictChoice) {
        let preparation;
        selectedConflictChoice = await conflictChoiceModal({
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
            return preparation.then(current => current ? ({ choices: conflictChoices(preview), replacementBlockers: preview.targetCapabilities.blockers || [] }) : null);
          },
        });
        if (!actionIsCurrent(action) || selectedConflictChoice == null) return;
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
          if (!actionIsCurrent(action) || choice == null) return;
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
          request: {
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
          },
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
  }

  async function trashSelection() {
    if (directMutationBlocked("Löschen")) return;
    const side = activeSide();
    const scope = paneScope(side);
    const action = beginAction([scope]);
    const entries = safeSelectedEntries(side, "Papierkorb");
    if (!entries.length) return;
    const items = entries.map((entry) => joinPath(scope.path, entry.name));
    if (scope.provider !== "local") {
      const operation = { endpoint: paneEndpoint(scope), items };
      try {
        const preview = await invoke("check_provider_delete", { request: operation });
        if (!actionIsCurrent(action)) return;
        let confirmation = null;
        if (preview.mode === "quarantine") {
          const choice = await choiceModal(
            "In Doppeldecker-Quarantäne verschieben",
            `<b>${preview.plan.items.length}</b> ausgewählte Einträge mit insgesamt ` +
              `<b>${preview.plan.entries}</b> Objekten und <b>${preview.plan.bytes}</b> Byte werden ` +
              `unter ${escapeHtml(scope.provider.toUpperCase())} · ${escapeHtml(scope.profileId)} · ` +
              `<code>${escapeHtml(scope.path)}</code> wiederherstellbar geparkt.` +
              "<br><br>Dies ist nicht der lokale Systempapierkorb.",
            [
              { label: "Abbrechen", value: "cancel", cls: "ghost" },
              { label: "In Quarantäne verschieben", value: "quarantine", cls: "danger" },
            ],
          );
          if (!actionIsCurrent(action)) return;
          if (choice !== "quarantine") return;
        } else {
          const limitations = preview.capabilities?.limitations || [];
          const description = `${preview.plan.items.length} ausgewählte Einträge, ${preview.plan.entries} Objekte und ${preview.plan.bytes} Byte in ${scope.path}.\n\n`
            + "Die Einträge werden dauerhaft gelöscht. Es gibt keinen Papierkorb und keine Wiederherstellung durch Doppeldecker.\n\n"
            + limitations.join("\n\n");
          const detail = `<p>${preview.plan.items.length} ausgewählte Einträge, ${preview.plan.entries} Objekte und ${preview.plan.bytes} Byte in <code>${escapeHtml(scope.path)}</code>.</p>`
            + "<p>Die Einträge werden dauerhaft gelöscht. Es gibt keinen Papierkorb und keine Wiederherstellung durch Doppeldecker.</p>"
            + limitations.map(note => `<p>${escapeHtml(note)}</p>`).join("");
          if (preview.confirmation) {
            confirmation = await promptModal(`Große endgültige Löschung: exakt „${preview.confirmation}“ eingeben`, "", { description });
            if (!actionIsCurrent(action) || confirmation !== preview.confirmation) return;
          } else {
            const choice = await choiceModal("Dauerhaft löschen", detail,
              [{ label: "Abbrechen", value: "cancel", cls: "ghost" }, { label: "Dauerhaft löschen", value: "confirm", cls: "danger" }]);
            if (!actionIsCurrent(action) || choice !== "confirm") return;
          }
        }
        if (!actionIsCurrent(action)) return;
        await invoke("enqueue_provider_delete", {
          request: {
            ...operation,
            mode: preview.mode,
            expectedPlan: preview.plan,
            confirmation,
            effectsConfirmed: true,
          },
        });
        if (!actionIsCurrent(action)) return;
        const label = preview.mode === "quarantine"
          ? "Doppeldecker-Quarantäne"
          : "Dauerhaft löschen";
        showDock(
          `<span class="pill delete">${label}</span>${items.length} Eintrag/Einträge sicher in die persistente Queue eingereiht`,
          { closable: true },
        );
      } catch (error) {
        if (actionIsCurrent(action)) showError("Remote-Löschen", error);
      }
      return;
    }
    try {
      if (!actionIsCurrent(action)) return;
      const result = await invoke("trash_paths", { items });
      // Ein unklarer Ausgang ist weder Erfolg noch Fehlschlag und wird deshalb
      // eigens benannt: `trashed + failed + unclear` ergibt die Gesamtzahl.
      const bilanz = [
        result.failed ? `${result.failed} fehlgeschlagen` : "",
        result.unclear ? `${result.unclear} mit unklarem Ausgang` : "",
      ].filter(Boolean);
      const detail = result.errors.length
        ? ` · ${bilanz.length ? bilanz.join(", ") : "Warnung"}: ${escapeHtml(result.errors.join("; "))}`
        : "";
      showDock(
        `<span class="pill ok">Papierkorb</span>${result.trashed} Objekt(e) in den Papierkorb gelegt (wiederherstellbar)${detail}`,
        { closable: true },
      );
      await refreshMatchingPanes(scope);
    } catch (error) {
      // Der Aufruf kann nach Teilwirkung scheitern. Auch dann bleibt sein Fehler
      // sichtbar und jede noch passende Pane liest den tatsächlichen Bestand neu.
      showError("Fehler", error);
      await refreshMatchingPanes(scope).catch(() => {});
    }
  }

  async function renameCursor() {
    if (directMutationBlocked("Umbenennen")) return;
    const side = activeSide();
    const pane = panes[side];
    const entry = pane.entries[pane.cursor];
    if (!entry) return;
    const scope = paneScope(side);
    const action = beginAction([scope]);
    const entryName = entry.name;
    // F2 wirkt ausschließlich auf den Cursor-Eintrag — genau der muss sicher
    // dekodierbar sein. Die Auswahl kann nach Strg-Klick ein ANDERER, sicherer
    // Eintrag sein; eine Auswahlprüfung würde dann fälschlich passieren und der
    // verlustbehaftete Anzeigename einen fremden Servereintrag umbenennen.
    if (!isEntryNameSafe(entry)) {
      showDock(
        `<span class="pill delete">Unsicherer Dateiname</span>Umbenennen ist für Namen ohne eindeutige UTF-8-Abbildung gesperrt. Der Eintrag bleibt unverändert.`,
        { closable: true },
      );
      return;
    }
    let contract = { kind: "existing" };
    if (scope.provider !== "local") {
      try {
        contract = await invoke("provider_rename_contract", { endpoint: paneEndpoint(scope) });
      } catch (error) {
        if (actionIsCurrent(action)) showError("Fehler", error);
        return;
      }
      if (!actionIsCurrent(action)) return;
      if (contract.kind === "blocked") {
        showDock(`<span class="pill warn">Umbenennen</span>${escapeHtml((contract.blockers || []).join(" · "))}`, { closable: true });
        return;
      }
    }
    const name = await promptModal("Umbenennen", entryName, {
      copyableDescription: contract.kind === "confirmed",
      description: contract.kind === "confirmed"
        ? [contract.execution_class === "best_effort" ? "Best Effort" : "Abgesichert", ...(contract.limitations || [])].join(" · ")
        : "",
    });
    if (!actionIsCurrent(action)) return;
    if (name == null || name.trim() === "" || name === entryName) return;
    try {
      const path = joinPath(scope.path, entryName);
      let newPath;
      let resultDetail = "";
      if (!actionIsCurrent(action)) return;
      if (scope.provider === "local") {
        newPath = await invoke("rename_path", { path, newName: name });
      } else {
        // Der Core liefert ein eindeutiges, rückgelesenes Resultat samt
        // Ausführungsklasse. Das Frontend zeigt es nur an und erfindet keine
        // eigene Providerlogik: Eine Kollision ist keine Ausnahme, sondern ein
        // sichtbares Ergebnis ohne Mutation.
        const report = await invoke("rename_provider_path", {
          request: {
            endpoint: paneEndpoint(scope),
            path,
            newName: name,
            ...(contract.kind === "confirmed" ? { expectedExecutionClass: contract.execution_class } : {}),
          },
        });
        if (report.outcome === "collision") {
          // Dieselbe dreiwertige Auskunft wie im CLI: Ordner, Datei oder (bei
          // einer erst serverseitig erkannten späten Kollision) unbestimmt.
          const existing = report.existingIsDir === true
            ? "ein vorhandener Ordner"
            : report.existingIsDir === false
              ? "eine vorhandene Datei"
              : "ein vorhandener Eintrag";
          showDock(
            `<span class="pill warn">Umbenennen</span>„${escapeHtml(name.trim())}“ ist bereits belegt (${existing}) — es wurde nichts verändert`,
            { closable: true },
          );
          return;
        }
        newPath = report.newPath;
        // Einschränkungen hängen an JEDER verfügbaren Ausführungsklasse: Auch ein
        // abgesichertes Umbenennen schützt nur das Ziel, während die Quelle allein
        // über ihren Pfad gebunden bleibt (core/src/remote_real_mode.rs). Sie
        // deshalb immer anzeigen; Klasse und Reconnect-Bestätigung stehen nur als
        // zusätzliche Angabe davor. Ein kurzer Hinweis statt einer modalen Warnung.
        const notes = [];
        if (report.class === "best_effort") notes.push("Best Effort");
        if (report.verifiedAfterReconnect) {
          notes.push("Ergebnis nach Verbindungsabbruch per Listing bestätigt");
        }
        const limitations = Array.isArray(report.limitations) ? report.limitations : [];
        resultDetail = [...notes, ...limitations].join(" · ");
      }
      showDock(
        `<span class="pill ok">Umbenannt</span>${escapeHtml(newPath.split("/").pop())}`
          + (resultDetail ? ` · ${escapeHtml(resultDetail)}` : ""),
        { closable: true },
      );
      await refreshMatchingPanes(scope, newPath.split("/").pop());
    } catch (error) {
      const unclear = errCode(error) === "outcome_unclear";
      showError(unclear ? "Ausgang unklar" : "Fehler", error, { foldDetails: unclear });
      // Auch ein Fehler kann nach bereits vollzogener Wirkung eintreffen. Jede
      // noch passende Pane liest deshalb neu; eine andere Navigation bleibt stehen.
      await refreshMatchingPanes(scope).catch(() => {});
    }
  }

  async function newFolder() {
    if (directMutationBlocked("Ordner anlegen")) return;
    const side = activeSide();
    const scope = paneScope(side);
    const action = beginAction([scope]);
    // Eine Pane ohne Pfad ist keine gültige Grundlage: `setPaneLocation` leert
    // `pane.path` beim Endpunktwechsel, und scheitert das erste Listing, bleibt der
    // Leerstring stehen. Er würde als `parent` an das Backend gehen und dort als
    // relativer Name an der kanonischen Provider-Root landen — der Ordner entstünde
    // also an einer Stelle, die der Nutzer nie gesehen hat. Deshalb sichtbar
    // abbrechen statt still.
    if (!scope.path) {
      showDock(
        `<span class="pill warn">Ordner anlegen</span>Diese Pane zeigt gerade keinen Ordner an (das letzte Listing schlug fehl). Bitte zuerst einen Pfad öffnen.`,
        { closable: true },
      );
      return;
    }
    let contract = null;
    if (scope.provider !== "local") {
      try {
        contract = await invoke("provider_folder_contract", { endpoint: paneEndpoint(scope) });
      } catch (error) { if (actionIsCurrent(action)) showError("Ordner anlegen", error); return; }
      if (!actionIsCurrent(action)) return;
      if (!contract.execution_class) {
        showDock(`<span class="pill warn">Ordner anlegen gesperrt</span>${escapeHtml((contract.blockers || []).join("; "))}`, { closable: true });
        return;
      }
    }
    const name = await promptModal("Neuer Ordner", "Neuer Ordner", {
      copyableDescription: Boolean(contract),
      description: (contract?.limitations || []).join(" ") || null,
    });
    if (!actionIsCurrent(action)) return;
    if (name == null || name.trim() === "") return;
    try {
      // Lokal und Remote teilen denselben Kernpfad (Namensprüfung, späte Kollision,
      // reine AddOnly-Mutation). Remote geht immer als vollständiger Endpunkt an
      // EIN Backend-Kommando; das Frontend kennt keine providerspezifischen Commands.
      if (!actionIsCurrent(action)) return;
      const report = scope.provider === "local"
        ? await invoke("create_folder", { parent: scope.path, name })
        : await invoke("create_provider_folder", {
          endpoint: paneEndpoint(scope),
          name,
          expectedExecutionClass: contract.execution_class,
        });
      const path = typeof report === "string" ? report : report.path;
      const notes = typeof report === "string" ? [] : report.limitations || [];
      showDock(
        `<span class="pill ok">Ordner angelegt</span>${escapeHtml(path.split("/").pop())}${notes.length ? ` · ${escapeHtml(notes.join(" · "))}` : ""}`,
        { closable: true },
      );
      await refreshMatchingPanes(scope, path.split("/").pop());
    } catch (error) {
      const unclear = errCode(error) === "outcome_unclear";
      showError(unclear ? "Ausgang unklar" : "Fehler", error, { foldDetails: unclear });
      await refreshMatchingPanes(scope).catch(() => {});
    }
  }

  async function resumeGeneratedUpload(entry) {
    if (!entry.generated_source || entry.attention?.type !== "ConflictsChanged") return;
    return transfer("copy", { provider: "local", path: "" }, { provider: "local", path: "" }, [], beginAction([]), null,
      { operationId: entry.operation_id, label: entry.generated_source.label });
  }

  return {
    resumeGeneratedUpload,
    capturePasteTarget: paneScope,
    pastePaths,
    captureDrag,
    copyDrop,
    newFolder,
    refreshBothPanes,
    renameCursor,
    selectedEntries,
    transferToOther,
    trashSelection,
  };
}
