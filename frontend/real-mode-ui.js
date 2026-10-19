import { fingerprint } from "./connections-ui.js";
import { executionChoiceModal, hideModal, showOwnedModal } from "./modal.js";
import { invoke } from "./tauri-api.js";
import { $, digestHex, escapeHtml, fmtTransferBytes } from "./ui-utils.js";

function stableKey(value) {
  if (Array.isArray(value)) return `[${value.map(stableKey).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${stableKey(value[key])}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

function identityDisplay(root) {
  const identity = root?.identity || {};
  if (root?.provider === "Sftp") {
    return `sftp://${identity.server?.host || "?"}:${identity.server?.port || 22}${identity.virtual_root || "/"}`;
  }
  if (root?.provider === "Ftps") {
    const scheme = identity.server?.transport_security === "plaintext" ? "ftp" : "ftps";
    return `${scheme}://${identity.server?.host || "?"}:${identity.server?.port || 21}${identity.virtual_root || "/"}`;
  }
  if (root?.provider === "WebDav") {
    return `${identity.server?.origin || "https://?"}${identity.virtual_root || "/"}`;
  }
  if (root?.provider === "Smb") {
    return `smb://${identity.server?.host || "?"}/${identity.server?.share || "?"}${identity.virtual_root || "/"}`;
  }
  return identity.display_path || "unbekanntes Ziel";
}

function mountSummary(identity) {
  if (!identity?.mount) {
    const server = identity?.server || {};
    if (server.transport_security === "plaintext") {
      return `FTP unverschlüsselt · Sitzung ${identity?.session_generation || "?"}`;
    }
    if (server.certificate) {
      const label = server.origin ? "WebDAV" : "FTPS";
      return `${label} · Zertifikat ${fingerprint(server.certificate.sha256)} · Sitzung ${identity?.session_generation || "?"}`;
    }
    if (server.server_guid) {
      return `SMB ${Number(server.dialect || 0).toString(16)} · GUID ${digestHex(server.server_guid)} · Signing + Verschlüsselung aktiv · Sitzung ${identity?.session_generation || "?"}`;
    }
    return `SFTP ${server.sftp_version || "?"} · Hostkey ${server.host_key ? fingerprint(server.host_key.sha256) : "?"} · Sitzung ${identity?.session_generation || "?"}`;
  }
  const mount = identity.mount;
  const volume = mount.volume_id ? ` · Volume ${digestHex(mount.volume_id)}` : "";
  return `${mount.fs_type} · Mount ${mount.mount_id}${volume} · Flags 0x${Number(mount.mount_flags).toString(16)}`;
}

/** Verwaltet ausschließlich den sichtbaren Realmodus-Assistenten und dessen
 * kurzlebigen Frontendzustand. Das nicht serialisierbare Permit bleibt im Backend. */
export function createRealModeUi({
  currentSourceTarget,
  syncScope,
  endProgress,
  ensureContentHash,
  mode,
  onProgressStart,
  remoteSyncContext,
  running,
  showDock,
  showError,
  showPlan,
  syncOptions,
}) {
  let available = false;
  let prepared = null;
  let remoteProtocol = null;
  let preparedKey = null;
  let runActive = false;
  let bannerTimer = null;
  let preparedScope = null;
  let preparationRevision = 0;
  let preparationTail = Promise.resolve();
  let starting = false;

  // Vorbereitung, Dialog und Discard teilen einen Besitzer. Ein Discard einer
  // verspäteten Antwort muss fertig sein, bevor die nächste Vorbereitung beginnt.
  function withPreparation(action) {
    const result = preparationTail.then(action);
    preparationTail = result.catch(() => {});
    return result;
  }

  function withStart(action) {
    if (starting || running()) return;
    starting = true;
    return withPreparation(action).finally(() => { starting = false; });
  }

  function currentAction() {
    const scope = syncScope();
    const revision = preparationRevision;
    return () => scope === syncScope() && revision === preparationRevision;
  }

  async function checkCurrent(current) {
    if (current()) return true;
    await discardPrepared();
    return false;
  }

  function hideBanner() {
    if (bannerTimer) clearInterval(bannerTimer);
    bannerTimer = null;
    $("#real-mode-banner").hidden = true;
  }

  function discard() {
    preparationRevision += 1;
    return withPreparation(discardPrepared);
  }

  async function discardPrepared() {
    if (!prepared || runActive) return;
    const command = remoteProtocol ? `discard_${remoteProtocol}_sync` : "discard_real_sync";
    try {
      await invoke(command);
      prepared = null;
      remoteProtocol = null;
      preparedKey = null;
      preparedScope = null;
      hideBanner();
    } catch (_) {
      // Ein Busy-/Transportfehler beweist nicht, dass das Backend-Permit fort ist.
      // Der sichtbare Zustand bleibt deshalb für Retry oder erneutes Discard erhalten.
    }
  }

  function showBanner(preview, active = false) {
    if (bannerTimer) clearInterval(bannerTimer);
    const banner = $("#real-mode-banner");
    const target = identityDisplay(preview.target_identity);
    const render = () => {
      const remaining = Math.max(0, Math.ceil((Number(preview.expires_at_unix_ms) - Date.now()) / 1000));
      const time = active
        ? "Permit wird einmalig verbraucht"
        : `noch ${Math.floor(remaining / 60)}:${String(remaining % 60).padStart(2, "0")}`;
      banner.textContent = `REALMODUS ${active ? "AKTIV" : "VORBEREITET"} · ${preview.options.mode} · Ziel ${target} · ${time}`;
      banner.hidden = false;
      if (!active && remaining === 0) {
        discard();
        showDock("<span class=\"pill delete\">Realmodus abgelaufen</span>Bitte vollständige Vorprüfung wiederholen.", { closable: true });
      }
    };
    render();
    if (!active) bannerTimer = setInterval(render, 1000);
  }

  function setAvailable(value) {
    available = Boolean(value);
    const button = $("#btn-real");
    button.disabled = !available;
    if (!available) button.title = "Im markierten Testmodus absichtlich getrennt und deaktiviert";
  }

  function finishActive() {
    if (!runActive) return;
    // Das Terminalevent beweist, dass der Backendstart angenommen und die
    // Einmalvorbereitung verbraucht wurde. Es kann noch vor der Auflösung des
    // Invoke-Promises eintreffen; deshalb muss auch dieser Pfad den gehaltenen
    // Frontendverweis löschen, bevor das Schließen des Sync-Modus `discard` ruft.
    prepared = null;
    remoteProtocol = null;
    preparedKey = null;
    preparedScope = null;
    runActive = false;
    hideBanner();
  }

  function prepareRemotePreview(remote) {
    if (remote.protocol === "webdav"
      || remote.source?.provider === "webdav"
      || remote.target?.provider === "webdav") ensureContentHash();
    const current = currentAction();
    return withPreparation(async () => {
      if (!current()) return;
      await discardPrepared();
      if (prepared) throw new Error("Die vorige Backendvorbereitung konnte nicht verworfen werden");
      if (!current()) return;
      const scope = syncScope();
      const { protocol, ...request } = remote;
      const requestWithOptions = { ...request, options: syncOptions() };
      const args = { request: requestWithOptions };
      const next = await invoke(`prepare_${protocol}_sync`, args);
      prepared = next;
      remoteProtocol = protocol;
      preparedKey = stableKey({ protocol, request: requestWithOptions });
      preparedScope = scope;
      if (!await checkCurrent(current)) return;
      showBanner(next);
      showPlan(next.plan);
      return next;
    });
  }

  function runRemote(remote) {
    if (mode() !== "AddOnly" || remote.protocol === "webdav"
      || remote.source?.provider === "webdav"
      || remote.target?.provider === "webdav") ensureContentHash();
    const current = currentAction();
    return withStart(() => runRemoteCurrent(remote, current));
  }

  async function runRemoteCurrent(remote, current) {
    if (running() || !current()) return;
    const protocol = remote.protocol;
    const scope = syncScope();
    const label = protocol === "provider" ? "REMOTE↔REMOTE" : protocol.toUpperCase();
    const { protocol: _protocol, ...request } = remote;
    const identityCheck = protocol === "sftp"
      ? "Hostkey"
      : protocol === "ftps"
        ? "FTP-/FTPS-Transportstatus, Provideridentität und Datenkanal"
        : protocol === "webdav"
          ? "Zertifikat-Pin, HTTPS-Origin und starke ETags"
          : protocol === "smb"
            ? "Server-GUID-Pin, SMB 3.1.1, Signing und Verschlüsselung"
            : "beide öffentlichen Provideridentitäten, Capabilities und vollständige Listings";
    showDock(`<span class="pill delete">${label}-Sicherheitsprüfung</span>${identityCheck}, Provideridentität und vollständige Listings werden gebunden …`, {});
    const requestWithOptions = { ...request, options: syncOptions() };
    const actionKey = stableKey({ protocol, request: requestWithOptions });
    let next = preparedKey === actionKey && remoteProtocol === protocol
      && preparedScope === syncScope() ? prepared : null;
    if (!next) {
      await discardPrepared();
      if (prepared) {
        return showError(`${label} gesperrt`, new Error("Die vorige Backendvorbereitung konnte nicht verworfen werden"));
      }
      if (!current()) return;
      try {
        next = await invoke(`prepare_${protocol}_sync`, { request: requestWithOptions });
      } catch (error) {
        return showError(`${label} gesperrt`, error);
      }
      prepared = next;
      remoteProtocol = protocol;
      preparedKey = actionKey;
      preparedScope = scope;
    }
    if (!await checkCurrent(current)) return;
    showBanner(next);
    showPlan(next.plan);
    if (!next.plan.actions.length) {
      await discardPrepared();
      return;
    }
    const execution = await executionChoiceModal(
      `${label}-Ausführung`,
      "Der sichtbare Plan ist vollständig gebunden. Direkter Start verbraucht das Permit sofort; die persistente Queue speichert nur die geheimnisfreie Absicht und verlangt vor Ausführung eine neue Prüfung samt neuer Zufallsphrase.",
    );
    if (!await checkCurrent(current)) return;
    if (execution === "queue") {
      try {
        if (protocol === "provider") {
          await invoke("enqueue_provider_sync");
        } else {
          await invoke(`enqueue_${protocol}_sync`, { request: { ...requestWithOptions, expectedPlan: next.plan } });
        }
        prepared = null;
        remoteProtocol = null;
        preparedKey = null;
        preparedScope = null;
        hideBanner();
        showDock(`<span class="pill copy">${label} eingereiht</span>Die Queue verbindet frisch und fordert danach die einmalige Laufphrase an.`, { closable: true });
        return true;
      } catch (error) {
        showError(`${label}-Queue`, error);
      }
      return;
    }
    if (execution !== "direct") {
      await discardPrepared();
      return;
    }
    const confirmation = await confirmationModal(next, `${label}-Realmodus bestätigen`);
    if (!await checkCurrent(current)) return;
    if (!confirmation) {
      await discardPrepared();
      return showDock(`<span class="pill none">${label}-Lauf verworfen</span>Es wurde nichts verändert.`, { closable: true });
    }
    runActive = true;
    showBanner(next, true);
    onProgressStart(next.plan);
    try {
      await invoke(`run_${protocol}_sync`, confirmation);
      prepared = null;
      remoteProtocol = null;
      preparedKey = null;
      preparedScope = null;
    } catch (error) {
      runActive = false;
      showBanner(next);
      endProgress();
      $("#progress").hidden = true;
      showError(`${label}-Fehler`, error);
    }
  }

  function confirmationModal(preview, title = "Lokalen Realmodus bestätigen") {
    return new Promise(resolve => {
      showOwnedModal("real-mode", leaseToken => {
      const modal = $("#modal .modal");
      modal.classList.remove("quarantine-modal");
      modal.classList.add("real-mode-modal");
      $("#modal-title").textContent = title;
      const source = preview.source_identity.identity;
      const target = preview.target_identity.identity;
      const impact = preview.impact;
      const restoreText = preview.restore_probe_ok
        ? "bestanden"
        : (preview.options.mode === "AddOnly" ? "für AddOnly nicht nötig" : "fehlgeschlagen");
      const spaceText = impact.available_bytes_known
        ? `${fmtTransferBytes(impact.required_available_bytes)} benötigt / ${fmtTransferBytes(impact.available_bytes)} verfügbar`
        : `${fmtTransferBytes(impact.required_available_bytes)} benötigt / Provider meldet keine verlässliche Quota`;
      const backupHtml = preview.backup_confirmation_required
        ? '<label class="real-check"><input id="real-backup" type="checkbox" />' +
          "<span>Ich bestätige ein unabhängiges Backup und eine jüngste erfolgreiche Restore-Probe dieses Backups. Doppeldecker hat dieses externe Backup nicht technisch geprüft.</span></label>"
        : '<input id="real-backup" type="checkbox" checked hidden />';
      const capabilities = preview.provider_capabilities;
      const capabilityHtml = capabilities
        ? `<div class="real-card"><strong>Verbindungsfähigkeiten</strong><small>` +
          `AddOnly ${capabilities.add_only ? "verfügbar" : "gesperrt"} · ` +
          `Update ${capabilities.update ? "verfügbar" : "gesperrt"} · ` +
          `Mirror ${capabilities.mirror ? "verfügbar" : "gesperrt"} · ` +
          `Quarantäne ${capabilities.quarantine_verified ? "geprüft" : "nicht geprüft"} · ` +
          `Verzeichnis-Dauerhaftigkeit ${capabilities.durable_directory_flush ? "geprüft" : "nicht belegt"}` +
          `</small></div>`
        : "";
      $("#modal-body").innerHTML = `
        <div class="real-warning">Dies ist kein Testlauf. Das Permit gilt genau einmal, höchstens zehn Minuten und ausschließlich für den unten gebundenen Plan.</div>
        <div class="real-grid">
          <div class="real-card"><strong>Quelle</strong><code title="${escapeHtml(identityDisplay(preview.source_identity))}">${escapeHtml(identityDisplay(preview.source_identity))}</code><small>${escapeHtml(mountSummary(source))} · ${preview.source_listing.entries_seen} Einträge vollständig</small></div>
          <div class="real-card"><strong>Ziel</strong><code title="${escapeHtml(identityDisplay(preview.target_identity))}">${escapeHtml(identityDisplay(preview.target_identity))}</code><small>${escapeHtml(mountSummary(target))} · ${preview.target_listing.entries_seen} Einträge vollständig</small></div>
          <div class="real-card"><strong>Wirkung</strong><small>${impact.new_files} neu (${fmtTransferBytes(impact.new_bytes)}) · ${impact.replacements} ersetzen (${fmtTransferBytes(impact.replacement_old_bytes)} gesichert) · ${impact.quarantine_entries} quarantänisieren (${fmtTransferBytes(impact.quarantine_bytes)})</small></div>
          <div class="real-card"><strong>Schutzprüfungen</strong><small>Listing Quelle/Ziel vollständig · Restore-Probe ${restoreText} · Platz ${spaceText}</small></div>
          ${capabilityHtml}
        </div>
        <div class="real-card"><strong>Plan-Digest</strong><code>${digestHex(preview.plan_digest)}</code></div>
        ${backupHtml}
        <label class="real-phrase-label" for="real-phrase">Exakt eingeben: <code>${escapeHtml(preview.confirmation_phrase)}</code></label>
        <input id="real-phrase" class="modal-input" spellcheck="false" autocomplete="off" />`;
      $("#modal-body").onclick = null;
      const bar = $("#modal .modal-buttons");
      bar.innerHTML = "";
      const cancel = document.createElement("button");
      cancel.className = "btn ghost";
      cancel.textContent = "Vorbereitung verwerfen";
      const start = document.createElement("button");
      start.className = "btn danger";
      start.textContent = "Einmalig real starten";
      start.disabled = true;
      bar.append(cancel, start);
      const phrase = $("#real-phrase");
      const backup = $("#real-backup");
      const update = () => {
        start.disabled = phrase.value !== preview.confirmation_phrase || !backup.checked;
      };
      let finished = false;
      const finish = value => {
        if (finished || !leaseToken || !hideModal(leaseToken)) return;
        finished = true;
        if ($("#modal").hidden) modal.classList.remove("real-mode-modal");
        resolve(value);
      };
      phrase.addEventListener("input", update);
      backup.addEventListener("change", update);
      phrase.addEventListener("keydown", event => {
        if (event.key === "Enter" && !start.disabled) {
          event.preventDefault();
          finish({ confirmation: phrase.value, backupConfirmed: backup.checked });
        }
      });
      cancel.addEventListener("click", () => finish(null));
      start.addEventListener("click", () => finish({ confirmation: phrase.value, backupConfirmed: backup.checked }));
      return { focusTarget: phrase, onEscape: () => finish(null) };
      });
    });
  }

  function run() {
    if (running()) return;
    const remote = remoteSyncContext();
    if (remote?.error) return showError("Remote gesperrt", new Error(remote.error));
    if (remote) {
      return runRemote(remote);
    }
    if (mode() !== "AddOnly") ensureContentHash();
    const current = currentAction();
    return withStart(() => runLocal(current));
  }

  async function runLocal(current) {
    if (running() || !current()) return;
    const scope = syncScope();
    if (!available) {
      return showDock("<span class=\"pill delete\">Realmodus nicht verfügbar</span>Der markierte Testmodus und der echte Datenmodus sind technisch getrennt.", { closable: true });
    }
    const { source, target } = currentSourceTarget();
    if (!source || !target || source === target) {
      return showDock("<span class=\"pill delete\">Ungültige Wurzeln</span>Quelle und Ziel müssen verschiedene vorhandene Ordner sein.", { closable: true });
    }
    showDock("<span class=\"pill delete\">Realmodus-Prüfung</span>Mount-Identitäten, vollständige Listings, BLAKE3, Speicher und Restore-Pfad werden geprüft …", {});
    const request = { source, target, options: syncOptions() };
    const actionKey = stableKey({ protocol: "local", request });
    let next = preparedKey === actionKey && remoteProtocol == null
      && preparedScope === syncScope() ? prepared : null;
    if (!next) {
      await discardPrepared();
      if (prepared) {
        return showError("Realmodus gesperrt", new Error("Die vorige Backendvorbereitung konnte nicht verworfen werden"));
      }
      if (!current()) return;
      try {
        next = await invoke("prepare_real_sync", request);
      } catch (error) {
        return showError("Realmodus gesperrt", error);
      }
      prepared = next;
      remoteProtocol = null;
      preparedKey = actionKey;
      preparedScope = scope;
    }
    if (!await checkCurrent(current)) return;
    showBanner(next);
    showPlan(next.plan);
    const confirmation = await confirmationModal(next);
    if (!await checkCurrent(current)) return;
    if (!confirmation) {
      await discardPrepared();
      return showDock("<span class=\"pill none\">Realmodus verworfen</span>Es wurde nichts an Nutzerdaten verändert.", { closable: true });
    }
    runActive = true;
    showBanner(next, true);
    onProgressStart(next.plan);
    try {
      await invoke("run_real_sync", confirmation);
      prepared = null;
      preparedKey = null;
      preparedScope = null;
    } catch (error) {
      runActive = false;
      showBanner(next);
      endProgress();
      $("#progress").hidden = true;
      showError("Realmodus-Fehler", error);
    }
  }

  return {
    discard,
    finishActive,
    isActive: () => runActive,
    prepareRemotePreview,
    run,
    runRemote,
    setAvailable,
  };
}
