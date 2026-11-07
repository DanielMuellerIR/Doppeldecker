import { confirmForwardBidirectional } from "./forward-bidirectional-ui.js";
import { conflictChoiceModal } from "./conflict-dialog.js";
import { choiceModal } from "./modal.js";
import { invoke } from "./tauri-api.js";
import { escapeHtml } from "./ui-utils.js";

function endpointFromPane(pane) {
  const endpoint = { provider: pane.provider, root: pane.path };
  if (pane.provider !== "local") {
    endpoint.profileId = pane.profileId;
    endpoint.profileRevision = pane.profileRevision;
  }
  return endpoint;
}

function sameEndpoint(left, right) {
  return left?.provider === right?.provider
    && left?.root === right?.root
    && (left?.profileId || null) === (right?.profileId || null)
    && (left?.profileRevision ?? null) === (right?.profileRevision ?? null);
}

function visiblePairRoles(visibleLeft, visibleRight, pair) {
  if (pair.visibleLeftRole === "left" || pair.visibleLeftRole === "right") {
    return {
      left: pair.visibleLeftRole,
      right: pair.visibleLeftRole === "left" ? "right" : "left",
    };
  }
  const leftRole = sameEndpoint(visibleLeft, pair.left)
    ? "left"
    : sameEndpoint(visibleLeft, pair.right) ? "right" : null;
  const rightRole = sameEndpoint(visibleRight, pair.left)
    ? "left"
    : sameEndpoint(visibleRight, pair.right) ? "right" : null;
  if (!leftRole || !rightRole || leftRole === rightRole) {
    throw new Error("Die sichtbaren Panes stimmen nicht mit den registrierten Paarrollen überein");
  }
  return { left: leftRole, right: rightRole };
}

function impactOf(preview) {
  const counts = { copies: 0, replacements: 0, deletes: 0, conflictCopies: 0 };
  for (const action of preview.plan.actions) {
    if (action.type === "copy") {
      if (action.expected_target == null) counts.copies += 1;
      else counts.replacements += 1;
    } else if (action.type === "type_swap") counts.replacements += 1;
    else if (action.type === "delete") counts.deletes += 1;
    else if (action.type === "conflict_copy") counts.conflictCopies += 1;
  }
  return counts;
}

function impactHtml(preview) {
  const impact = impactOf(preview);
  return `<p><b>${impact.copies}</b> additive Pfade · <b>${impact.replacements}</b> Ersetzungen/Typwechsel · ` +
    `<b>${impact.deletes}</b> Löschpropagierungen in Quarantäne · <b>${impact.conflictCopies}</b> Konfliktkopien.</p>`;
}

function warningsHtml(preview) {
  return (preview.scope_warnings || []).map(warning => `<p class="real-warning">${escapeHtml(warning)}</p>`).join("");
}

/** Kapselt ausschließlich Assistent, Bestätigung und Recovery-Bedienung. Alle
 * Snapshots, Digests, Capability-Gates und Mutationen bleiben GUI-frei im Kern. */
export function createBidirectionalUi({
  panes,
  syncScope,
  realRunActive,
  running,
  startRun,
  endRun,
  dockRevision = () => 0,
  showDock,
  showError,
}) {
  let pendingAction = null;
  let pendingRecovery = null;
  let starting = false;
  let generation = 0;
  let preparingForward = false;

  async function discardPendingAction() {
    if (!pendingAction) return true;
    try {
      await invoke(pendingAction.forward ? "discard_forward_bidirectional_sync" : "discard_bidirectional_sync");
      pendingAction = null;
      return true;
    } catch (_) {
      // Ein Fehler beweist nicht, dass die Backendvorbereitung verschwunden ist.
      return false;
    }
  }

  async function decideConflicts(preview, visibleRoles, current, forward = false, operationId = null) {
    const resolutions = [];
    let conflicts = preview.conflict_details;
    const total = conflicts?.length || 0;
    let index = 0;
    if (!Array.isArray(conflicts)) throw new Error("Die Backendvorschau enthält keine gebundenen Konfliktdaten");
    while (conflicts.length) {
      const conflict = conflicts[0];
      if (!current()) return null;
      const visibleRole = role => visibleRoles.left === role ? "Links" : "Rechts";
      const choices = conflict.choices.map((choice, value) => {
        const decision = choice.decision;
        let label;
        if (decision.decision === "skip") label = "Überspringen";
        else if (decision.decision === "keep_both") label = `Beide · ${visibleRole(decision.primary).toLowerCase()} am Original`;
        else label = `${visibleRole(decision.decision === "keep_left" ? "left" : "right")} behalten`;
        const explanation = choice.explanation.replace(/Linke|Rechte|Links|Rechts|linke|rechte|linken|rechten/g, word => {
          const role = /^(L|l)/.test(word) ? "left" : "right";
          const visible = visibleRole(role).toLowerCase();
          const ending = word.endsWith("en") ? "en" : word.endsWith("e") ? "e" : "s";
          const text = visible.slice(0, -1) + ending;
          return word[0] === word[0].toUpperCase() ? text[0].toUpperCase() + text.slice(1) : text;
        }) + (choice.generated_name ? ` Neuer Name: ${choice.generated_name}` : "");
        return { label, value, explanation: choice.blocker ? `${explanation} Nicht ausführbar: ${choice.blocker}` : explanation, disabled: Boolean(choice.blocker), danger: decision.decision === "keep_left" || decision.decision === "keep_right" };
      });
      if (conflict.skip_blocker) choices.push({ label: "Überspringen nicht verfügbar", disabled: true, explanation: conflict.skip_blocker });
      const value = await conflictChoiceModal({
        title: `Konflikt ${index + 1} von ${total}`,
        conflicts: [{ ...conflict, left: conflict[visibleRoles.left], right: conflict[visibleRoles.right] }],
        leftRole: "Links", rightRole: "Rechts", choices,
        note: `Konflikt ${index + 1} von ${total}. Keine automatische Wahl nach Änderungszeit. Eine Ordnerentscheidung gilt für den vollständigen Unterbaum. Abbrechen verwirft alle noch nicht ausgeführten Entscheidungen.`,
      });
      if (value == null || !current()) return null;
      resolutions.push({ path: conflict.path, decision: conflict.choices[value].decision });
      index += 1;
      if (conflicts.length > 1) {
        conflicts = forward
          ? await invoke("preview_forward_bidirectional_conflicts", { resolutions, expectedUnresolvedDigest: preview.unresolved_digest, operationId })
          : await invoke("preview_bidirectional_conflicts", { resolutions, expectedPlanDigest: preview.plan.plan_digest });
        if (!current()) return null;
      } else conflicts = [];
    }
    return resolutions;
  }

  function sameRecovery(left, right) {
    return left?.operationId === right?.operationId
      && left?.recoveryPreparationId === right?.recoveryPreparationId
      && left?.recoveryKind === right?.recoveryKind;
  }

  function bindRecovery(entry, preview, decision) {
    const identity = {
      operationId: entry.operation_id,
      recoveryPreparationId: preview.recovery_preparation_id,
      recoveryKind: preview.recovery_kind,
    };
    if (!identity.recoveryPreparationId || identity.recoveryKind !== decision) {
      throw new Error("Die Backendvorbereitung besitzt keine gültige Recovery-Identität");
    }
    pendingRecovery = identity;
    return identity;
  }

  async function discardRecovery(identity = pendingRecovery) {
    if (!identity) return { confirmed: true, hadError: false };
    let hadError = false;
    // Ein Transportfehler kann vor oder nach der Backendwirkung entstehen. Der
    // zweite identische Aufruf dient deshalb zugleich als sicherer Readback: Das
    // Backend bestätigt auch eine bereits abwesende Vorbereitung. Eine fremde
    // oder neuere Identität wird niemals angerührt.
    for (let attempt = 0; attempt < 2; attempt += 1) {
      try {
        const status = await invoke("discard_bidirectional_recovery_preparation", identity);
        if (status?.absent !== true) {
          throw new Error("Das Backend hat die Abwesenheit der Recovery-Vorbereitung nicht bestätigt");
        }
        if (sameRecovery(pendingRecovery, identity)) pendingRecovery = null;
        return { confirmed: true, hadError };
      } catch (error) {
        hadError = true;
        showError("Recovery-Vorbereitung verwerfen", error);
      }
    }
    return { confirmed: false, hadError: true };
  }

  async function recoverForwardEntry(entry) {
    if (entry.bidirectional_completion_recovery) {
      try {
        await invoke("finish_forward_bidirectional_queue_completion", { operationId: entry.operation_id });
        showDock('<span class="pill ok">Baselineabschluss bestätigt</span>Der bereits ausgeführte Lauf wurde ohne erneute Dateioperation abgeschlossen.', { closable: true });
        return true;
      } catch (error) { showError("Bidirektionalen Abschluss prüfen", error); return false; }
    }
    let preview;
    const operationId = entry.operation_id;
    const discardQueuePreparation = () => invoke("discard_forward_bidirectional_queue_preparation", { operationId });
    try {
      if (entry.state === "AwaitingConfirmation") {
        preview = await invoke("forward_bidirectional_queue_preview", { operationId });
      } else {
        const replan = await choiceModal(`Neu abgleichen · ${entry.operation.pair_name}`,
          "Der unterbrochene Lauf kann beide Seiten bereits verändert haben. Der neue Abgleich liest beide Seiten gegen die letzte gemeinsame Baseline. Alte Konfliktentscheidungen werden verworfen; es gibt kein universelles Rollback und keinen blinden Retry.",
          [{ label: "Abbrechen", value: false, cls: "ghost" }, { label: "Neu abgleichen", value: true, cls: "danger" }]);
        if (!replan) return false;
        preview = await invoke("prepare_forward_bidirectional_queue_recovery", { operationId, resolutions: [], expectedUnresolvedDigest: null });
      }
      if (preview.plan.conflicts > 0) {
        const resolutions = await decideConflicts(preview, { left: "left", right: "right" }, () => true, true, operationId);
        if (!resolutions) { await discardQueuePreparation(); return false; }
        preview = await invoke("prepare_forward_bidirectional_queue_recovery", { operationId, resolutions, expectedUnresolvedDigest: preview.unresolved_digest });
      }
      if (preview.stage_blocker) {
        await discardQueuePreparation();
        showDock(`<span class="pill delete">Neuabgleich gesperrt</span>${escapeHtml(preview.stage_blocker)}`, { closable: true });
        return false;
      }
      const confirmation = await confirmForwardBidirectional(preview, { pairName: entry.operation.pair_name, left: entry.operation.left, right: entry.operation.right, queued: true });
      if (!confirmation) { await discardQueuePreparation(); return false; }
      await invoke("approve_forward_bidirectional_queue_entry", { operationId, ...confirmation });
      return true;
    } catch (error) { showError("Bidirektionales Recovery", error); return false; }
  }

  async function recoverQueueEntry(entry) {
    if (entry.operation.type === "BidirectionalForward") return recoverForwardEntry(entry);
    if (entry.bidirectional_completion_recovery) {
      try {
        await invoke("finish_bidirectional_completion_recovery", {
          operationId: entry.operation_id,
        });
        showDock(
          "<span class=\"pill update\">Abschluss bestätigt</span>Der bereits ausgeführte bidirektionale Lauf ist jetzt terminal im Queue-Journal gebunden.",
          { closable: true },
        );
      } catch (error) {
        showError("Bidirektionalen Abschluss prüfen", error);
      }
      return;
    }
    if (pendingRecovery) {
      const previousDiscard = await discardRecovery();
      if (!previousDiscard.confirmed) return;
    }
    const decision = await choiceModal(
      `Recovery · ${entry.operation.pair_name}`,
      "Der vorherige Lauf besitzt ein dauerhaftes Recovery-Journal. Fortsetzen prüft beide vollständigen Bäume erneut; Rollback stellt alle bereits committed Teilprojektionen rückwärts wieder her.",
      [
        { label: "Abbrechen", value: "cancel", cls: "ghost" },
        { label: "Rollback", value: "rollback", cls: "danger" },
        { label: "Fortsetzen", value: "resume", cls: "primary" },
      ],
    );
    if (decision === "cancel") return;
    let preview;
    try {
      preview = await invoke(
        decision === "resume" ? "prepare_bidirectional_resume" : "prepare_bidirectional_rollback",
        { operationId: entry.operation_id },
      );
    } catch (error) {
      return showError("Bidirektionales Recovery gesperrt", error);
    }
    let recoveryIdentity;
    try {
      recoveryIdentity = bindRecovery(entry, preview, decision);
    } catch (error) {
      return showError("Bidirektionales Recovery gesperrt", error);
    }
    if (decision === "resume" && preview.stage_blocker) {
      const discarded = await discardRecovery(recoveryIdentity);
      if (!discarded.confirmed || discarded.hadError) return;
      return showDock(`<span class="pill delete">Recovery gesperrt</span>${escapeHtml(preview.stage_blocker)}`, { closable: true });
    }
    const confirmed = await choiceModal(
      decision === "resume" ? "Bidirektionalen Sync fortsetzen" : "Vorherigen Bestand wiederherstellen",
      `<p>${escapeHtml(entry.operation.pair_name)} · ` +
        `${escapeHtml(entry.operation.left?.root || "links")} ↔ ${escapeHtml(entry.operation.right?.root || "rechts")}</p>` +
        (decision === "resume" ? impactHtml(preview) : `<p>${preview.committed_projections} bereits ausgeführte Teilprojektion(en) werden rückwärts wiederhergestellt. Vorhandene neuere Ziele werden geschützt.</p>`) +
        warningsHtml(preview),
      [{ label: "Abbrechen", value: false, cls: "ghost" },
        { label: decision === "resume" ? "Fortsetzen" : "Wiederherstellung starten", value: true, cls: "danger" }],
    );
    if (!confirmed) {
      const discarded = await discardRecovery(recoveryIdentity);
      if (!discarded.confirmed || discarded.hadError) return;
      return showDock("<span class=\"pill none\">Recovery nicht bestätigt</span>Journal und Queue-Sperre bleiben unverändert erhalten.", { closable: true });
    }
    // Fortschritt und Abbruch gehören zum Lauf; die Dateiansicht bleibt unabhängig.
    startRun("Bidirektionales Recovery läuft …");
    try {
      if (decision === "resume") {
        await invoke("run_bidirectional_sync", { effectsConfirmed: true });
      } else {
        await invoke("run_bidirectional_rollback", { effectsConfirmed: true });
      }
      // Der Worker kann sein Terminalevent schon vor der Command-Antwort senden.
      // Der Starttext stammt deshalb ausschließlich aus `startRun`; nach `await`
      // darf keine ältere „läuft“-Meldung einen bereits sichtbaren Abschluss ersetzen.
      if (sameRecovery(pendingRecovery, recoveryIdentity)) pendingRecovery = null;
    } catch (error) {
      endRun();
      showError("Bidirektionales Recovery", error);
    }
  }

  async function run() {
    if (running() || realRunActive() || starting) return;
    starting = true;
    const scope = syncScope();
    const owner = generation;
    const current = () => scope === syncScope() && owner === generation;
    try {
      return await runCurrent(scope, current);
    } finally {
      // Bis einschließlich Discard bleibt dieser Start alleiniger Besitzer der
      // Backendvorbereitung; ein später Cleanup darf keinen neuen Start treffen.
      starting = false;
    }
  }

  async function runCurrent(scope, current) {
    const left = endpointFromPane(panes.left);
    const right = endpointFromPane(panes.right);
    const forward = left.provider !== "local" || right.provider !== "local";
    const prepareCommand = forward ? "prepare_forward_bidirectional_sync" : "prepare_bidirectional_sync";
    const discardCommand = forward ? "discard_forward_bidirectional_sync" : "discard_bidirectional_sync";
    let recovery = false;
    let pair;
    let roles;
    try {
      // Paar-ID und Rollen stammen aus einer appweiten Backend-Transaktion. Zwei
      // Fenster und ein Pane-Tausch können deshalb niemals getrennte Baselines
      // oder Locks für dieselbe ungeordnete Endpunktmenge erzeugen.
      pair = await invoke(forward ? "register_forward_bidirectional_pair" : "register_bidirectional_pair", { left, right });
      if (!current()) {
        await discardPendingAction();
        return;
      }
      roles = visiblePairRoles(left, right, pair);
    } catch (error) {
      return showError("Bidirektionaler Sync gesperrt", error);
    }
    if (forward) {
      try {
        const status = await invoke("forward_bidirectional_pair_status", { pairId: pair.pairId });
        if (!current()) return;
        if (status?.baseline_completion_available) {
          if (await choiceModal("Baselineabschluss prüfen", "Die Dateiwirkung wurde bereits ausgeführt. Dieser Schritt schließt ausschließlich die gemeinsame Baseline ab und wiederholt keine Dateioperation.", [{ label: "Abbrechen", value: false, cls: "ghost" }, { label: "Baseline abschließen", value: true, cls: "primary" }]) && current()) {
            await invoke("finish_forward_bidirectional_pair_completion", { pairId: pair.pairId });
            showDock('<span class="pill ok">Baseline abgeschlossen</span>Der Dateibestand wurde nicht erneut verändert.', { closable: true });
          }
          return;
        }
        if (status) {
          recovery = await choiceModal("Bidirektionalen Bestand neu abgleichen", "Der vorherige Lauf kann bereits Änderungen enthalten. Der neue Abgleich liest beide Seiten gegen die letzte gemeinsame Baseline. Alte Konfliktentscheidungen werden verworfen; es gibt kein universelles Rollback und keinen blinden Retry.", [{ label: "Abbrechen", value: false, cls: "ghost" }, { label: "Neu abgleichen", value: true, cls: "danger" }]);
          if (!recovery || !current()) return;
        }
      } catch (error) { return showError("Bidirektionales Recovery gesperrt", error); }
    }
    let request = { ...pair, resolutions: [], ...(forward ? { recovery } : {}) };
    let actionKey = JSON.stringify(request);
    let preview = pendingAction?.key === actionKey && pendingAction.scope === scope
      ? pendingAction.preview : null;
    async function checkCurrent() {
      if (current()) return true;
      await discardPendingAction();
      return false;
    }
    try {
      if (!preview) {
        if (!await discardPendingAction()) {
          throw new Error("Die vorige bidirektionale Backendvorbereitung konnte nicht verworfen werden");
        }
        if (!current()) return;
        preparingForward = forward;
        try { preview = await invoke(prepareCommand, { request }); } finally { preparingForward = false; }
        pendingAction = { key: actionKey, request, preview, scope, forward };
      }
      if (!await checkCurrent()) return;
      if (preview.plan.conflicts > 0) {
        const resolutions = await decideConflicts(preview, roles, current, forward);
        await invoke(discardCommand);
        pendingAction = null;
        if (!resolutions || !current()) return;
        request = { ...request, resolutions, expectedUnresolvedDigest: forward ? preview.unresolved_digest : preview.plan.plan_digest };
        actionKey = JSON.stringify(request);
        preparingForward = forward;
        try { preview = await invoke(prepareCommand, { request }); } finally { preparingForward = false; }
        pendingAction = { key: actionKey, request, preview, scope, forward };
        if (!await checkCurrent()) return;
      }
    } catch (error) {
      return showError("Bidirektionaler Sync gesperrt", error);
    }
    if (preview.stage_blocker) {
      await discardPendingAction();
      return showDock(`<span class="pill delete">Gesperrt</span>${escapeHtml(preview.stage_blocker)}`, { closable: true });
    }
    const confirmed = forward ? await confirmForwardBidirectional(preview, { pairName: pair.pairName, left, right, visibleRoles: roles }) : await choiceModal(
      `Bidirektional · ${pair.pairName}`,
      `<div class="sync-bound-roots"><b>Links:</b> <code>${escapeHtml(left.root)}</code><br>` +
        `<b>Rechts:</b> <code>${escapeHtml(right.root)}</code></div>` + impactHtml(preview) + warningsHtml(preview) +
        "<p>Bestehende Fassungen und entfernte Einträge werden in Quarantäne gesichert. Beide Bäume und die Baseline werden vor der Ausführung erneut geprüft.</p>",
      [{ label: "Abbrechen", value: false, cls: "ghost" },
        { label: "Sync starten", value: true, cls: "danger" }],
    );
    if (!await checkCurrent()) return;
    if (!confirmed) {
      await discardPendingAction();
      return;
    }
    if (forward) {
      const presentation = dockRevision();
      try {
        await invoke("enqueue_forward_bidirectional_sync", confirmed);
        pendingAction = null;
        if (current() && dockRevision() === presentation) showDock('<span class="pill copy">Eingereiht</span>Bestätigter bidirektionaler Sync wartet in der Fensterqueue.', { closable: true });
        return true;
      } catch (error) { return showError("Bidirektionaler Start", error); }
    }
    startRun("Bidirektionaler Sync läuft …");
    try {
      await invoke("run_bidirectional_sync", { effectsConfirmed: true });
      pendingAction = null;
      // Siehe Recovery-Pfad oben: Das Terminalevent darf schneller als diese
      // Antwort sein und bleibt dann die neueste sichtbare Zustandsmeldung.
    } catch (error) {
      endRun();
      showError("Bidirektionaler Start", error);
    }
  }

  async function discard() {
    generation += 1;
    if (preparingForward) await invoke("discard_forward_bidirectional_sync");
    return discardPendingAction();
  }

  return { discard, recoverQueueEntry, run };
}
