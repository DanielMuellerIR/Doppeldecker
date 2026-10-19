import {
  confirmModal,
  hideModal,
  promptModal,
  replaceModal,
  showOwnedModal,
} from "./modal.js";
import { invoke } from "./tauri-api.js";
import { $, errCode, escapeHtml, fmtDate, fmtTransferBytes } from "./ui-utils.js";

/** Kapselt ausschließlich die Quarantäne-Ansicht. Manifestbindung, Restore ohne
 * Überschreiben und endgültiges Leeren werden weiterhin im Backend erzwungen. */
export function createQuarantineUi({
  currentSourceTarget,
  refreshPanes,
  running,
  showDock,
  showError,
}) {
  let retentionDays = 30;
  let modalToken = null;
  let navigationSequence = 0;
  let badgeSequence = 0;

  const sftpQuarantineLimitation = "Dateidownload, Sync und Quarantäneverwaltung gesperrt: SFTP v3 bindet READDIR, LSTAT und OPEN nicht atomar an den gewählten Pane-Root.";
  // Der Grund oben gehört zu SFTP v3. Das Backend meldet `listing: false` aber auch
  // für FTPS und WebDAV, und dort sind Dateidownload und Sync gerade nicht gesperrt.
  // Welche Provider betroffen sind, weiss allein der Core; das Frontend nennt
  // deshalb nur die Wirkung, die es tatsächlich sieht.
  const genericQuarantineLimitation = "Quarantäneverwaltung für diese Verbindung gesperrt: Der Provider bindet sein Listing nicht atomar an den gewählten Pane-Root.";

  function quarantineLimitation(scope) {
    return scope?.endpoint?.provider === "sftp"
      ? sftpQuarantineLimitation
      : genericQuarantineLimitation;
  }

  function quarantineCapabilities(pane) {
    const value = pane?.quarantineCapabilities;
    return Object.freeze({
      listing: value?.listing === true,
      restore: value?.restore === true,
      purge: value?.purge === true,
    });
  }

  function currentScope() {
    const current = currentSourceTarget();
    return current.targetPane.provider !== "local"
      ? Object.freeze({
        remote: true,
        capabilities: quarantineCapabilities(current.targetPane),
        endpoint: Object.freeze({
          provider: current.targetPane.provider,
          profileId: current.targetPane.profileId,
          profileRevision: current.targetPane.profileRevision,
          root: current.target,
        }),
      })
      : Object.freeze({ remote: false, target: current.target || null });
  }

  function sameScope(left, right) {
    if (left.remote !== right.remote) return false;
    if (!left.remote) return left.target === right.target;
    return left.endpoint.provider === right.endpoint.provider
      && left.endpoint.profileId === right.endpoint.profileId
      && left.endpoint.profileRevision === right.endpoint.profileRevision
      && left.endpoint.root === right.endpoint.root
      && left.capabilities.listing === right.capabilities.listing
      && left.capabilities.restore === right.capabilities.restore
      && left.capabilities.purge === right.capabilities.purge;
  }

  function call(boundScope, localCommand, remoteCommand, args = {}) {
    return boundScope.remote
      ? invoke(remoteCommand, {
        ...args,
        endpoint: boundScope.endpoint,
      })
      : invoke(localCommand, { ...args, target: boundScope.target });
  }

  function invalidateBadge() {
    badgeSequence += 1;
    $("#quarantine-count").textContent = "–";
    $("#btn-quarantine").title = "Quarantäne wird für den neuen Pane-Endpunkt neu geprüft";
  }

  function recoveryCounts(runs) {
    return runs.reduce((counts, run) => ({
      automatic: counts.automatic
        + (run.items_restorable || 0)
        + (run.items_recovery_pending || 0),
      manual: counts.manual + (run.items_manual_recovery || 0),
    }), { automatic: 0, manual: 0 });
  }

  async function refreshBadge() {
    const request = ++badgeSequence;
    const boundScope = currentScope();
    if (boundScope.remote && !boundScope.capabilities.listing) {
      $("#quarantine-count").textContent = "–";
      $("#btn-quarantine").disabled = true;
      $("#btn-quarantine").title = quarantineLimitation(boundScope);
      return;
    }
    $("#btn-quarantine").disabled = false;
    try {
      const badgeRuns = await call(
        boundScope,
        "list_quarantine_runs",
        "list_provider_quarantine_runs",
      );
      if (request !== badgeSequence || !sameScope(boundScope, currentScope())) return;
      const counts = recoveryCounts(badgeRuns);
      $("#quarantine-count").textContent = String(counts.automatic);
      $("#btn-quarantine").title = `${badgeRuns.length} Lauf/Läufe · ${counts.automatic} automatisch wiederherstellbare oder prüfbare Einträge · ${counts.manual} nur manuell rettbar`;
    } catch (error) {
      if (request !== badgeSequence || !sameScope(boundScope, currentScope())) return;
      // `busy` ist kein Quarantänefehler, sondern die Aussage „während einer Datei-
      // oder Sync-Operation ist die Quarantäne gesperrt". Ein Fehlerzeichen bliebe
      // danach stehen, bis zufällig ein Sync endet — der Abschluss einer
      // Dateioperation räumt es nicht weg. Der Zähler behält deshalb seinen Stand.
      if (errCode(error) === "busy") return;
      $("#quarantine-count").textContent = "!";
    }
  }

  function renderSpecialModal(title, bodyHtml, buttons, onBodyClick = null) {
    return token => {
      $("#modal .modal").classList.add("quarantine-modal");
      $("#modal-title").textContent = title;
      const body = $("#modal-body");
      body.innerHTML = bodyHtml;
      // Auch die delegierte Klickbehandlung entsteht innerhalb des Leases. Ein
      // später eingereihter Dialog kann sie deshalb nicht versehentlich erben.
      body.onclick = onBodyClick ? event => onBodyClick(event, token) : null;
      const bar = $("#modal .modal-buttons");
      bar.innerHTML = "";
      for (const button of buttons) {
        const element = document.createElement("button");
        element.className = "btn " + (button.cls || "ghost");
        element.textContent = button.label;
        element.disabled = Boolean(button.disabled);
        element.addEventListener("click", () => { void button.action(token); });
        bar.appendChild(element);
      }
      return {
        focusTarget: bar.querySelector("button"),
        onEscape: () => closeSpecialModal(token),
      };
    };
  }

  // Queue und Quarantäne teilen bewusst dieselbe fokussichere Spezialansicht.
  function showSpecialModal(title, bodyHtml, buttons, onBodyClick = null) {
    const render = renderSpecialModal(title, bodyHtml, buttons, onBodyClick);
    if (modalToken && replaceModal(modalToken, render)) return modalToken;
    modalToken = showOwnedModal("quarantine", render);
    return modalToken;
  }

  function closeSpecialModal(token) {
    if (!token || !hideModal(token)) return false;
    if (modalToken === token) modalToken = null;
    if ($("#modal").hidden) $("#modal .modal").classList.remove("quarantine-modal");
    return true;
  }

  function stillOwns(token) {
    return !token || modalToken === token;
  }

  function beginNavigation(token) {
    navigationSequence += 1;
    return { id: navigationSequence, token };
  }

  function stillOwnsNavigation(request) {
    return request.id === navigationSequence && stillOwns(request.token);
  }

  function stillOwnsScope(request, boundScope) {
    return stillOwnsNavigation(request) && sameScope(boundScope, currentScope());
  }

  async function open(expectedToken = null, inheritedScope = null) {
    if (running()) return;
    // Eine geöffnete Ansicht gehört unveränderlich zu genau dem Ziel, für das ihre
    // Liste geladen wurde. „Zurück" bleibt deshalb ebenfalls in diesem Scope.
    const boundScope = inheritedScope || currentScope();
    if (boundScope.remote && !boundScope.capabilities.listing) {
      showDock(`<span class="pill none">Quarantäne nicht verfügbar</span>${quarantineLimitation(boundScope)}`, { closable: true });
      return;
    }
    const request = beginNavigation(expectedToken);
    let nextRuns;
    try {
      nextRuns = await call(
        boundScope,
        "list_quarantine_runs",
        "list_provider_quarantine_runs",
      );
    } catch (error) {
      if (!stillOwnsScope(request, boundScope)) return;
      $("#quarantine-count").textContent = "!";
      return showError("Quarantäne-Fehler", error);
    }
    if (!stillOwnsScope(request, boundScope)) return;
    const visibleRuns = nextRuns;
    const counts = recoveryCounts(visibleRuns);
    $("#quarantine-count").textContent = String(counts.automatic);
    const rows = visibleRuns.length
      ? visibleRuns.map((run, index) => runRow(run, index)).join("")
      : `<div class="quarantine-empty">Keine manifestierten Quarantäneläufe vorhanden.</div>`;

    showSpecialModal(
      "Quarantäne",
      `<div class="quarantine-note">Aufbewahrungsziel: ${retentionDays} Tage. Es wird nichts automatisch gelöscht; endgültiges Leeren bleibt immer separat bestätigt.</div><div class="quarantine-list">${rows}</div>`,
      [{ label: "Schließen", cls: "ghost", action: token => closeSpecialModal(token) }],
      (event, token) => {
        const button = event.target.closest("[data-open-run]");
        if (!button) return;
        const run = visibleRuns[Number(button.dataset.openRun)];
        if (run) void openRun(run, token, boundScope);
      },
    );
  }

  function runRow(run, index) {
    const state = runStatusLabel(run.status);
    const expiry = Date.now() >= run.expires_unix_ms
      ? "Aufbewahrungsziel erreicht"
      : `Aufbewahrung bis ${fmtDate(run.expires_unix_ms / 1000)}`;
    return `<div class="quarantine-row">
      <div class="quarantine-main">
        <div class="quarantine-name">${escapeHtml(run.id)}</div>
        <div class="quarantine-meta">${fmtDate(run.created_unix_ms / 1000)} · ${state} · ${run.items_available}/${run.items_total} physisch vorhanden${run.items_manual_recovery ? ` · ${run.items_manual_recovery} nur manuell rettbar` : ""}${run.items_recovery_pending ? ` · ${run.items_recovery_pending} Abschlussprüfung` : ""} · ${fmtTransferBytes(run.bytes_available)}</div>
        <div class="quarantine-meta" title="${escapeHtml(run.target_root)}">Ziel: ${escapeHtml(run.target_root)} · ${expiry}</div>
      </div>
      <div class="quarantine-actions"><button class="btn ghost small" data-open-run="${index}">Anzeigen</button></div>
    </div>`;
  }

  function runStatusLabel(status) {
    switch (status) {
      case "live": return "laufender Lauf";
      case "crash_recovery": return "Recovery nach unvollständigem Lauf";
      case "purge_recovery": return "Recovery nach begonnenem Leeren";
      case "complete": return "abgeschlossen";
      case "purged": return "Inhalte geleert · Recovery-Tombstone bleibt sichtbar";
      default: return "unbekannter Laufstatus · Wirkungen gesperrt";
    }
  }

  function runAllowsRestore(run) {
    return run?.status === "complete" || run?.status === "crash_recovery";
  }

  function runAllowsPurge(run) {
    return runAllowsRestore(run) || run?.status === "purge_recovery";
  }

  async function openRun(run, expectedToken, boundScope) {
    const request = beginNavigation(expectedToken);
    let entries;
    try {
      entries = await call(
        boundScope,
        "list_quarantine_entries",
        "list_provider_quarantine_entries",
        { runId: run.id },
      );
    } catch (error) {
      if (!stillOwnsScope(request, boundScope)) return;
      showError("Quarantäne-Fehler", error);
      return open(expectedToken, boundScope);
    }
    if (!stillOwnsScope(request, boundScope)) return;
    const restoreAllowed = runAllowsRestore(run);
    const purgeAllowed = runAllowsPurge(run);
    const rows = entries.length
      ? entries.map((entry, index) => entryRow(
        entry,
        index,
        restoreAllowed && (!boundScope.remote || boundScope.capabilities.restore),
      )).join("")
      : `<div class="quarantine-empty">Dieser Lauf enthält keine manifestierten Pfade.</div>`;
    showSpecialModal(
      `Quarantäne · ${run.id}`,
      `<div class="quarantine-note">Ursprüngliches Ziel: <code>${escapeHtml(run.target_root)}</code>. Wiederherstellen ersetzt niemals einen inzwischen vorhandenen Namen.</div><div class="quarantine-list">${rows}</div>`,
      [
        { label: "Zurück", cls: "ghost", action: token => open(token, boundScope) },
        {
          label: "Lauf endgültig leeren",
          cls: "danger",
          disabled: !purgeAllowed || (boundScope.remote && !boundScope.capabilities.purge),
          action: token => purge(run, token, boundScope),
        },
        { label: "Schließen", cls: "ghost", action: token => closeSpecialModal(token) },
      ],
      (event, token) => { void restoreSelected(event, run, entries, token, boundScope); },
    );
  }

  function entryRow(entry, index, restoreAllowed) {
    const status = entry.restored
      ? "wiederhergestellt"
      : entry.manual_recovery_required
        ? "physisch vorhanden · nur manuelle Recovery möglich"
      : entry.recovery_pending && !entry.available
        ? "Restore-Abschluss muss geprüft werden"
      : entry.available
        ? `${entry.entries} Einträge · ${fmtTransferBytes(entry.bytes)}`
        : "Intent ohne vorhandenen Quarantäneeintrag";
    const reason = entry.reason === "replaced" ? "ersetzte Version" : "Mirror-Entfernung";
    const canRestore = entry.automatic_restore_available ?? (entry.available || entry.recovery_pending);
    const disabled = !restoreAllowed || !canRestore || entry.restored ? " disabled" : "";
    const action = entry.recovery_pending && !entry.available
      ? "Abschluss prüfen"
      : "Wiederherstellen";
    return `<div class="quarantine-row">
      <div class="quarantine-main">
        <div class="quarantine-name" title="${escapeHtml(entry.relative_path)}">${escapeHtml(entry.relative_path)}</div>
        <div class="quarantine-meta">${reason} · ${status}</div>
      </div>
      <div class="quarantine-actions"><button class="btn primary small" data-restore-entry="${index}"${disabled}>${action}</button></div>
    </div>`;
  }

  async function purge(run, parentToken, boundScope) {
    if (!runAllowsPurge(run)
        || (boundScope.remote && !boundScope.capabilities.purge)) return;
    const request = beginNavigation(parentToken);
    const expected = `LEEREN ${run.id}`;
    const typed = await promptModal(
      `Endgültig leeren: exakt „${expected}“ eingeben`,
      "",
      { parentToken },
    );
    if (!stillOwnsNavigation(request)) return;
    if (typed !== expected) return openRun(run, parentToken, boundScope);
    if (!sameScope(boundScope, currentScope())) {
      showDock(`<span class="pill none">Ziel gewechselt</span>Die Quarantäne wurde für ein anderes Pane geöffnet. Bitte die Quarantäne neu öffnen.`, { closable: true });
      return closeSpecialModal(parentToken);
    }
    try {
      await call(boundScope, "purge_quarantine_run", "purge_provider_quarantine_run", {
        runId: run.id,
        confirmation: typed,
      });
      showDock(`<span class="pill ok">Quarantäne</span>Inhalte von Lauf ${escapeHtml(run.id)} geleert; der Recovery-Tombstone bleibt sichtbar`, { closable: true });
      if (!stillOwnsScope(request, boundScope)) return undefined;
      // Der vom Prompt wieder eingesetzte Dialog beschreibt den Zustand vor der
      // endgültigen Mutation. Er muss verschwinden, bevor irgendein Reload
      // scheitern kann; erst ein erfolgreiches Listing darf neue Knöpfe anzeigen.
      closeSpecialModal(parentToken);
      if (!sameScope(boundScope, currentScope())) return undefined;
      await refreshBadge();
      if (!sameScope(boundScope, currentScope())) return undefined;
      return open(null, boundScope);
    } catch (error) {
      showError("Leeren fehlgeschlagen", error);
      if (!stillOwnsScope(request, boundScope)) return undefined;
      // Der Purge-Intent entsteht vor der eigentlichen Mutation. Nach einem Fehler
      // darf deshalb nie das vor dem Aufruf geladene Run-Objekt Restore wieder
      // freischalten. Die frische Core-Liste liefert insbesondere `purge_recovery`.
      try {
        const currentRuns = await call(
          boundScope,
          "list_quarantine_runs",
          "list_provider_quarantine_runs",
        );
        if (!stillOwnsScope(request, boundScope)) return undefined;
        const currentRun = currentRuns.find(candidate => candidate.id === run.id);
        return currentRun
          ? openRun(currentRun, parentToken, boundScope)
          : open(parentToken, boundScope);
      } catch (refreshError) {
        if (stillOwnsScope(request, boundScope)) {
          // Der wieder eingesetzte Eltern-Dialog enthält noch den Snapshot vor dem
          // Purge. Wenn auch der zwingende Reload scheitert, darf daraus keine
          // Restore- oder erneute Purge-Wirkung mehr angeboten werden.
          closeSpecialModal(parentToken);
          showError("Quarantäne-Status konnte nicht neu geladen werden", refreshError);
        }
        return undefined;
      }
    }
  }

  async function restoreSelected(event, run, entries, parentToken, boundScope) {
    const button = event.target.closest("[data-restore-entry]");
    if (!button) return;
    const entry = entries[Number(button.dataset.restoreEntry)];
    if (!runAllowsRestore(run)
        || (boundScope.remote && !boundScope.capabilities.restore)
        || !entry
        || !(entry.automatic_restore_available ?? (entry.available || entry.recovery_pending))
        || entry.restored) return;
    const request = beginNavigation(parentToken);
    const reconciliationOnly = entry.recovery_pending && !entry.available;
    const confirmed = await confirmModal(
      reconciliationOnly ? "Restore-Abschluss prüfen" : "Aus Quarantäne wiederherstellen",
      reconciliationOnly
        ? `Doppeldecker prüft, ob <code>${escapeHtml(entry.relative_path)}</code> bereits identitätsgleich unter <code>${escapeHtml(run.target_root)}</code> liegt, und schließt dann ausschließlich das Manifest ab.<br><br>Ein fremder oder fehlender Zielname bleibt unangetastet.`
        : `<code>${escapeHtml(entry.relative_path)}</code> wird nach <code>${escapeHtml(run.target_root)}</code> zurückgelegt.<br><br>Ein vorhandener Zielname wird nicht ersetzt.`,
      { parentToken },
    );
    if (!stillOwnsNavigation(request)) return;
    if (!confirmed) return openRun(run, parentToken, boundScope);
    if (!sameScope(boundScope, currentScope())) {
      showDock(`<span class="pill none">Ziel gewechselt</span>Die Quarantäne wurde für ein anderes Pane geöffnet. Bitte die Quarantäne neu öffnen.`, { closable: true });
      return closeSpecialModal(parentToken);
    }
    try {
      const restored = await call(
        boundScope,
        "restore_quarantine_entry",
        "restore_provider_quarantine_entry",
        { runId: run.id, relativePath: entry.relative_path },
      );
      if (!restored
          || typeof restored.path !== "string"
          || restored.path.length === 0
          || (restored.outcome !== "restored" && restored.outcome !== "reconciled")) {
        throw new Error("Ungültige Restore-Antwort des Backends");
      }
      const reconciled = restored.outcome === "reconciled";
      const label = reconciled ? "Restore-Abschluss bestätigt" : "Wiederhergestellt";
      showDock(`<span class="pill ok">${label}</span><code>${escapeHtml(restored.path)}</code>`, { closable: true });
      if (sameScope(boundScope, currentScope())) {
        await refreshPanes();
        await refreshBadge();
      }
    } catch (error) {
      showError("Wiederherstellung fehlgeschlagen", error);
    }
    if (!stillOwnsScope(request, boundScope)) return;
    return openRun(run, parentToken, boundScope);
  }

  function setRetentionDays(days) {
    retentionDays = days || 30;
  }

  return {
    closeModal: closeSpecialModal,
    open,
    invalidateBadge,
    refreshBadge,
    setRetentionDays,
    showModal: showSpecialModal,
  };
}
