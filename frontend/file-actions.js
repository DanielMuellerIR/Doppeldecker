// Interaktive Standard-Dateiaktionen. Der Controller hält keinen eigenen Pane-Zustand;
// er erhält den jeweils aktuellen Zustand und die sichere Backendbrücke von außen.

import { createFileTransfer } from "./file-transfer.js";
import { paneEndpoint } from "./pane-endpoint.js";
import { invoke } from "./tauri-api.js";
import { choiceModal, promptModal } from "./modal.js";
import { escapeHtml, isEntryNameSafe, joinPath, errCode } from "./ui-utils.js";

const resumableProtocols = new Map([["webdav", "WebDAV"], ["ftps", "FTPS"], ["sftp", "SFTP"]]);

/** @typedef {"left" | "right"} PaneSide */
/** @typedef {import("./pane-endpoint.js").PaneEndpointLocation & {
 * profileId?: string | null, profileRevision?: number | null, navigationRevision: number
 * }} FileActionLocation */
/** @typedef {{name: string, is_file?: boolean, is_symlink?: boolean, safely_decoded?: boolean}} FileActionEntry */
/** @typedef {FileActionLocation & {sel: Set<number>, cursor: number, entries: FileActionEntry[]}} FileActionPane */

/** @param {{
 * panes: Record<PaneSide, FileActionPane>, activeSide: () => PaneSide, running: () => boolean,
 * enqueueBlocked?: () => boolean,
 * loadPane: (side: PaneSide, path: string, options: {passive: boolean, push: boolean, keepSelName: string | null, preserveView?: boolean}) => Promise<unknown>,
 * showDock: (message: string, options: {closable: boolean}) => void,
 * showDownloadPreparation: (message: string, cancel: () => Promise<null>) => () => void,
 * showError: (context: string, error: unknown, options?: {foldDetails: boolean}) => void,
 * dockRevision?: () => number
 * }} dependencies */
export function createFileActions({ panes, activeSide, running, enqueueBlocked = running, loadPane, showDock, showDownloadPreparation, showError, dockRevision = () => 0 }) {
  let actionGeneration = 0;
  let downloadPreparing = false;
  const transfer = createFileTransfer({ paneEndpoint, actionIsCurrent, running, showDock, showError, dockRevision });

  function directMutationBlocked(action) {
    if (!running()) return false;
    showDock(`<span class="pill none">${escapeHtml(action)} derzeit belegt</span>` +
      "Diese direkte Dateiaktion ist nach dem laufenden Auftrag wieder verfügbar. " +
      "Navigation sowie weitere Copy-/Move-Aufträge bleiben erreichbar.", { closable: true });
    return true;
  }

  /** Ein Dateiaktions-Scope ist ein unveränderliches Abbild genau der Pane, die
   * der Nutzer gesehen hat. Nur Navigation entwertet die Absicht; ein passiver
   * Refresh während eines Queueabschlusses besitzt dieselbe Navigationsrevision.
   * @param {PaneSide} side
   */
  function paneScope(side) {
    const pane = panes[side];
    return /** @type {Readonly<FileActionLocation & {side: PaneSide}>} */ (Object.freeze({
      side,
      navigationRevision: pane.navigationRevision,
      provider: pane.provider,
      profileId: pane.profileId,
      profileRevision: pane.profileRevision,
      path: pane.path,
    }));
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

  /** Auswahl der Pane; ohne Mehrfachauswahl gilt der Eintrag unter dem Cursor.
   * @param {PaneSide} side
   */
  function selectedEntries(side) {
    const pane = panes[side];
    const indices = pane.sel.size
      ? [...pane.sel]
      : pane.entries[pane.cursor]
        ? [pane.cursor]
        : [];
    return indices.map((index) => pane.entries[index]).filter(Boolean);
  }

  /** @param {PaneSide} side @param {string} action */
  function safeSelectedEntries(side, action) {
    const entries = selectedEntries(side);
    if (entries.some((entry) => !isEntryNameSafe(entry))) {
      showDock(
        `<span class="pill delete">Unsicherer Dateiname</span>${escapeHtml(action)} ist für Namen ohne eindeutige UTF-8-Abbildung gesperrt. Der Eintrag bleibt unverändert.`,
        { closable: true },
      );
      return null;
    }
    return entries;
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
    if (!entries) return;
    return transfer(kind, source, target, entries.map(entry => joinPath(source.path, entry.name)),
      beginAction([source, target]));
  }

  function canResumeCopy(side, entries) {
    const source = panes[side];
    const target = panes[side === "left" ? "right" : "left"];
    return (source.provider === "local" || resumableProtocols.has(source.provider))
      && target.provider === "local"
      && entries.length > 0 && entries.every(entry => entry.is_file && !entry.is_symlink);
  }

  async function resumableCopy(side = activeSide()) {
    if (enqueueBlocked()) return;
    const source = paneScope(side);
    const target = paneScope(side === "left" ? "right" : "left");
    const entries = safeSelectedEntries(side, "Fortsetzbares Kopieren");
    if (!entries) return;
    if (!canResumeCopy(side, entries)) {
      showDock('<span class="pill none">Fortsetzbar kopieren</span>Wähle reguläre Dateien lokal, über WebDAV, FTPS oder SFTP. Öffne im anderen Pane einen lokalen Zielordner.', { closable: true });
      return;
    }
    const remoteProtocol = resumableProtocols.get(source.provider);
    let finishPreparation;
    let requestId;
    if (remoteProtocol) {
      if (downloadPreparing) return;
      downloadPreparing = true;
      const clientId = crypto.randomUUID();
      requestId = clientId;
      const check = source.provider !== "webdav"
        ? "Der Server berechnet die Prüfsumme der gesamten Datei."
        : "Die Dateiversion und ein kurzer Bytebereich werden geprüft.";
      finishPreparation = showDownloadPreparation(
        `<span class="pill copy">${remoteProtocol} · Fortsetzbarer Download</span> ${check}`,
        () => invoke("cancel_resumable_copy_preparation", { requestId: clientId }),
      );
    }
    const revision = dockRevision();
    try {
      await invoke("enqueue_resumable_copy", {
        sourceRoot: source.path,
        sourcePaths: entries.map(entry => joinPath(source.path, entry.name)),
        targetRoot: target.path,
        ...(remoteProtocol ? { remoteSource: paneEndpoint(source), requestId } : {}),
      });
      if (revision === dockRevision()) {
        const selection = entries.length === 1 ? "Eine Datei" : `${entries.length} Dateien`;
        showDock(`<span class="pill copy">Fortsetzbare Kopie</span>${selection} eingereiht.`, { closable: true });
      }
    } catch (error) {
      if (remoteProtocol && errCode(error) === "interrupted") {
        if (revision === dockRevision()) showDock('<span class="pill none">Downloadvorbereitung abgebrochen</span>', { closable: true });
      } else {
        showError("Kopieren", error);
      }
    } finally {
      if (finishPreparation) {
        finishPreparation();
        downloadPreparing = false;
      }
    }
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

  async function trashSelection() {
    if (directMutationBlocked("Löschen")) return;
    const side = activeSide();
    const scope = paneScope(side);
    const action = beginAction([scope]);
    const entries = safeSelectedEntries(side, "Papierkorb");
    if (!entries?.length) return;
    const items = entries.map((entry) => joinPath(scope.path, entry.name));
    if (scope.provider !== "local") {
      const operation = /** @satisfies {import("./routine-file-wire.js").ProviderDeleteRequestWire} */ ({
        endpoint: paneEndpoint(scope), items,
      });
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
        const request = /** @satisfies {import("./routine-file-wire.js").EnqueueProviderDeleteRequestWire} */ ({
          ...operation,
          mode: preview.mode,
          expectedPlan: preview.plan,
          confirmation,
          effectsConfirmed: true,
        });
        await invoke("enqueue_provider_delete", { request });
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
    /** @type {import("./routine-file-wire.js").RenameInteractionContractWire} */
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
        const request = /** @satisfies {import("./routine-file-wire.js").RenameProviderPathRequestWire} */ ({
          endpoint: paneEndpoint(scope),
          path,
          newName: name,
          ...(contract.kind === "confirmed" ? { expectedExecutionClass: contract.execution_class } : {}),
        });
        const report = await invoke("rename_provider_path", { request });
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
      const report = contract?.execution_class
        ? await invoke("create_provider_folder", {
          endpoint: paneEndpoint(scope),
          name,
          expectedExecutionClass: contract.execution_class,
        })
        : await invoke("create_folder", { parent: scope.path, name });
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
    resumableCopy,
    canResumeCopy,
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
