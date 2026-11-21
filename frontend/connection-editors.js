import { fingerprint } from "./connections-ui.js";
import { hideModal, showOwnedModal } from "./modal.js";
import { createSftpEditor } from "./sftp-connection-editor.js";
import { invoke } from "./tauri-api.js";
import { $, errCode, escapeHtml } from "./ui-utils.js";

function editorShell(title, bodyHtml, primaryLabel) {
  let leaseToken = null;
  let body = null;
  let buttons = null;
  let cancel = null;
  let primary = null;
  let requestSequence = 0;
  let irreversible = false;
  const closeCallbacks = [];
  const close = () => {
    // Während Schlüsselbund und Profil gemeinsam gespeichert werden, darf weder
    // Escape noch ein verspäteter Klick den Dialog halb geschlossen zurücklassen.
    if (irreversible || !leaseToken || !hideModal(leaseToken)) return false;
    leaseToken = null;
    requestSequence += 1;
    for (const callback of closeCallbacks) callback();
    if ($("#modal").hidden) $("#modal .modal").classList.remove("quarantine-modal");
    return true;
  };
  const shell = {
    close,
    get primary() { return primary; },
    onClose: callback => closeCallbacks.push(callback),
    query: selector => body?.querySelector(selector) || null,
    beginRequest: () => ({ id: ++requestSequence, token: leaseToken }),
    owns: request => Boolean(
      request
      && request.id === requestSequence
      && request.token === leaseToken
      && body?.isConnected,
    ),
    setEditingDisabled: (disabled) => {
      for (const element of body.querySelectorAll("input, select, button")) {
        element.disabled = disabled;
      }
      primary.disabled = disabled;
    },
    beginIrreversible: () => {
      const request = { id: ++requestSequence, token: leaseToken };
      irreversible = true;
      for (const element of body.querySelectorAll("input, select, button")) {
        element.disabled = true;
      }
      for (const element of buttons.querySelectorAll("button")) element.disabled = true;
      return request;
    },
    endIrreversible: request => {
      if (!shell.owns(request)) return false;
      irreversible = false;
      for (const element of body.querySelectorAll("input, select, button")) {
        element.disabled = false;
      }
      for (const element of buttons.querySelectorAll("button")) element.disabled = false;
      return true;
    },
    show: (setup, focusSelector) => {
      leaseToken = showOwnedModal("connection-editor", token => {
        // Erst der aktivierte Lease darf die gemeinsame Dialoghülle verändern.
        // Alle profilspezifischen Listener entstehen ebenfalls in diesem Schritt.
        leaseToken = token;
        $("#modal .modal").classList.add("quarantine-modal");
        $("#modal-title").textContent = title;
        body = $("#modal-body");
        body.innerHTML = bodyHtml;
        body.onclick = null;
        buttons = $("#modal .modal-buttons");
        buttons.innerHTML = "";
        cancel = document.createElement("button");
        cancel.className = "btn ghost";
        cancel.textContent = "Abbrechen";
        primary = document.createElement("button");
        primary.className = "btn primary";
        primary.textContent = primaryLabel;
        buttons.append(cancel, primary);
        cancel.addEventListener("click", close);
        setup();
        return {
          focusTarget: body.querySelector(focusSelector),
          onEscape: close,
        };
      });
      return leaseToken;
    },
  };
  return shell;
}

function secureSecretControl(editing) {
  return `<div class="native-secret-control wide">
    <span>Passwort</span>
    <button type="button" class="btn ghost native-secret-button">Sicher eingeben …</button>
    <small class="native-secret-status" aria-live="polite">${editing
      ? "Ohne neue Eingabe bleibt das vorhandene Passwort unverändert."
      : "Die Eingabe öffnet sich nativ außerhalb der Weboberfläche."}</small>
  </div>`;
}

/** Native Verbindungseditoren. Die Weboberfläche erhält niemals den Secretwert:
 * Sie verwaltet nur eine zufällige, fenster- und profilgebundene Backend-Einmal-ID. */
export function createConnectionEditors({
  digestHex,
  reloadProfiles,
  saveProfile,
  showDock,
  showError,
}) {
  /**
   * Liest die Pflichtfelder eines passwortbasierten Verbindungsprofils.
   *
   * Ein neues Profil braucht zusätzlich ein Passwort; bei einem bestehenden genügt
   * der bereits vorhandene Schlüsselbundverweis, denn das Passwort selbst liegt nie
   * im Frontend. `rootLabel` benennt das Rootfeld des jeweiligen Protokolls.
   * Rückgabe `null` bedeutet: Es fehlt etwas und der Nutzer wurde bereits
   * informiert.
   */
  function readPasswordProfileFields({ nameInput, userInput, rootInput, secureSecret }, existing, rootLabel) {
    const name = nameInput.value.trim();
    const username = userInput.value.trim();
    const requestedRoot = rootInput.value.trim();
    const secretId = secureSecret.id();
    if (!name || !username || !requestedRoot || (!secretId && !existing?.password_reference)) {
      showDock(`<span class="pill none">Unvollständig</span>Name, Nutzer und ${rootLabel} werden benötigt; bei neuen Profilen auch das Passwort.`, { closable: true });
      return null;
    }
    return { name, username, requestedRoot, secretId };
  }

  /**
   * @param {ReturnType<typeof editorShell>} shell
   * @param {import("./provider-command-names.js").RemoteProvider} protocol
   * @param {string} profileId
   */
  function wireSecureSecret(shell, protocol, profileId) {
    const button = shell.query(".native-secret-button");
    const status = shell.query(".native-secret-status");
    /** @type {string | null} */
    let secretId = null;
    const discard = async () => {
      if (!secretId) return;
      const stale = secretId;
      secretId = null;
      try {
        await invoke("discard_connection_secret", { secretId: stale });
      } catch (error) {
        // Der Save-Command verbraucht die ID atomar mit der Profiltransaktion.
        // `secret_expired` beweist deshalb nur, dass kein Pending-Wert mehr
        // existiert; andere Transportfehler bleiben für den Aufrufer sichtbar.
        if (errCode(error) !== "secret_expired") throw error;
      }
    };
    shell.onClose(() => { void discard().catch(() => {}); });
    button.addEventListener("click", async () => {
      const request = shell.beginRequest();
      shell.setEditingDisabled(true);
      try {
        const captured = await invoke("capture_connection_secret", { protocol, profileId });
        if (!shell.owns(request)) {
          void invoke("discard_connection_secret", { secretId: captured.secretId }).catch(() => {});
          return;
        }
        void discard().catch(() => {});
        secretId = captured.secretId;
        button.textContent = "Neue Eingabe ersetzen …";
        status.textContent = "Ein sicherer Einmalwert ist für dieses Profil vorgemerkt.";
      } catch (error) {
        if (shell.owns(request) && errCode(error) !== "cancelled") {
          showError("Sichere Passworteingabe", error);
        }
      } finally {
        if (shell.owns(request)) shell.setEditingDisabled(false);
      }
    });
    return {
      discard,
      id: () => secretId,
      markConsumed: () => { secretId = null; },
    };
  }

  async function discardAfterSaveFailure(secureSecret, error) {
    if (errCode(error) === "busy") return;
    try {
      await secureSecret.discard();
    } catch (_) {
      // Der ursprüngliche Savefehler bleibt die Hauptmeldung. Der Backend-Speicher
      // besitzt zusätzlich einen Ablauf-Task; dieser feste Hinweis vermeidet rohe
      // Fehlerdetails und macht den verzögerten Recoveryweg trotzdem sichtbar.
      showDock(
        '<span class="pill warn">Sicherer Einmalwert</span>Der Wert konnte nicht sofort verworfen werden und wird backendseitig automatisch entfernt.',
        { closable: true },
      );
    }
  }

  async function closeAfterOutcomeUnclear(shell, secureSecret, request, title, error) {
    if (errCode(error) !== "outcome_unclear") return false;
    // Der Backend-Command hat die Einmal-ID vor der Profiltransaktion verbraucht.
    // Ein möglicherweise bereits sichtbarer neuer Schlüsselbundverweis darf nie
    // durch einen zweiten Save aus diesem alten Editor zurückgerollt werden.
    secureSecret?.markConsumed();
    if (!shell.endIrreversible(request)) return true;
    shell.close();
    showError(title, error);
    try {
      await reloadProfiles?.();
    } catch (readbackError) {
      showError(`${title} neu laden`, readbackError);
    }
    return true;
  }

  /** Ein „+" für alle Protokolle: Erst die Wahl, dann der jeweilige Editor mit
   * seinem unveränderten Discovery-/Pinning-Ablauf. */
  function openProtocolPicker() {
    const shell = editorShell(
      "Verbindung hinzufügen",
      `<div class="protocol-picker">
        <button class="protocol-choice" data-pick="sftp">
          <span class="side-icon sftp" aria-hidden="true"></span>
          <span class="protocol-text"><b>SFTP</b><small>Dateiübertragung über SSH · Hostkey wird zuerst geprüft und gepinnt</small></span>
        </button>
        <button class="protocol-choice" data-pick="ftps">
          <span class="side-icon ftps" aria-hidden="true"></span>
          <span class="protocol-text"><b>FTP / FTPS</b><small>Klartext oder explizites TLS · unsichere Transportstufen werden sichtbar ausgewiesen, aber nicht blockiert</small></span>
        </button>
        <button class="protocol-choice" data-pick="webdav">
          <span class="side-icon webdav" aria-hidden="true"></span>
          <span class="protocol-text"><b>WebDAV</b><small>HTTPS-Collections · Zertifikat wird zuerst geprüft und exakt gepinnt</small></span>
        </button>
        <button class="protocol-choice" data-pick="smb">
          <span class="side-icon smb" aria-hidden="true"></span>
          <span class="protocol-text"><b>SMB</b><small>Verschlüsselte SMB3-Freigaben · Server-GUID wird zuerst geprüft und gebunden</small></span>
        </button>
      </div>
      <small class="protocol-note">Passwörter landen ausschließlich im System-Schlüsselbund, nie im Profil.</small>`,
      "",
    );
    shell.show(() => {
      // Der Picker braucht keinen Primärknopf; die vier Wahlen sind die Aktionen.
      shell.primary.remove();
      const editors = {
        sftp: openSftpEditor,
        ftps: openFtpsEditor,
        webdav: openWebDavEditor,
        smb: openSmbEditor,
      };
      // Listener auf dem frisch erzeugten Container, nicht auf dem persistenten
      // #modal-body: so verschwindet er beim nächsten innerHTML-Tausch von selbst.
      $("#modal-body .protocol-picker").addEventListener("click", (event) => {
        const choice = event.target.closest("[data-pick]");
        if (!choice) return;
        if (!shell.close()) return;
        editors[choice.dataset.pick]();
      });
    }, '#modal-body [data-pick="sftp"]');
  }

  const openSftpEditor = createSftpEditor({
    editorShell,
    secureSecretControl,
    wireSecureSecret,
    saveProfile,
    closeAfterOutcomeUnclear,
    discardAfterSaveFailure,
    showDock,
    showError,
  });

  /** @param {import("./connection-profile-wire.js").VersionedProfileWire<import("./connection-profile-wire.js").FtpsProfileWire> | null} [existingProfile] */
  async function openFtpsEditor(existingProfile = null) {
    const editing = Boolean(existingProfile);
    const editorProfileId = existingProfile?.id || `ftps_${crypto.randomUUID().replaceAll("-", "")}`;
    const existing = existingProfile?.connection || null;
    const shell = editorShell(
      editing ? "FTP-/FTPS-Verbindung bearbeiten" : "FTP-/FTPS-Verbindung hinzufügen",
      `<div class="connection-form">
        <label class="wide">Anzeigename<input id="ftps-name" autocomplete="off" /></label>
        <label>Host<input id="ftps-host" autocomplete="off" /></label>
        <label>Port<input id="ftps-port" type="number" min="1" max="65535" value="21" /></label>
        <label>Nutzer<input id="ftps-user" autocomplete="username" /></label>
        <label>Server-Root<input id="ftps-root" value="/" autocomplete="off" /></label>
        <label class="wide">Transport<select id="ftps-transport">
          <option value="explicit_tls">FTPS · explizites TLS (FTPES)</option>
          <option value="plaintext">FTP · unverschlüsselt</option>
        </select></label>
        <label class="wide" id="ftps-policy-row">Zertifikatsprüfung<select id="ftps-policy">
          <option value="web_pki">Öffentliche Kette, Gültigkeit und Servername</option>
          <option value="web_pki_and_pin">Öffentliche PKI plus exakter Pin</option>
          <option value="pinned_certificate">Extern geprüftes privates Zertifikat plus exakter Pin</option>
          <option value="unverified_certificate">Exakter Pin ohne Identitätsprüfung</option>
        </select><small>„Ohne Identitätsprüfung“ verschlüsselt die Verbindung und bindet das beobachtete Zertifikat, bestätigt aber nicht, wem der Server gehört.</small></label>
        ${secureSecretControl(editing)}
        <button type="button" class="btn ghost wide" id="ftps-refresh-certificate" hidden>Zertifikat neu ermitteln</button>
        <div class="real-warning wide" id="ftps-certificate">Noch kein Zertifikat ermittelt. Der erste Schritt führt nur AUTH TLS aus; es erfolgt weder Anmeldung noch Dateizugriff.</div>
      </div>`,
      "Prüfen und speichern",
    );
    shell.show(() => {
    const nameInput = shell.query("#ftps-name");
    const hostInput = shell.query("#ftps-host");
    if (!(hostInput instanceof HTMLInputElement)) throw new Error("Eingabefeld #ftps-host fehlt.");
    const portInput = shell.query("#ftps-port");
    const userInput = shell.query("#ftps-user");
    const rootInput = shell.query("#ftps-root");
    const secureSecret = wireSecureSecret(shell, "ftps", editorProfileId);
    const policyRow = shell.query("#ftps-policy-row");
    if (editing) {
      nameInput.value = existingProfile.name;
      hostInput.value = existing.host;
      portInput.value = String(existing.port);
      userInput.value = existing.username;
      rootInput.value = existing.requested_root;
      shell.query("#ftps-transport").value = existing.transport_security || "explicit_tls";
      if (existing.certificate_policy) {
        shell.query("#ftps-policy").value = existing.certificate_policy;
      }
    }
    const originalTransport = existing?.transport_security || "explicit_tls";
    const originalEndpoint = existing ? `${existing.host}\n${existing.port}` : null;
    const originalPin = originalTransport === "explicit_tls"
      ? (existing?.certificate || null)
      : null;
    let pin = originalPin;
    let pinEndpoint = originalPin ? originalEndpoint : null;
    const transport = shell.query("#ftps-transport");
    const policy = shell.query("#ftps-policy");
    const certificateNote = shell.query("#ftps-certificate");
    const refreshCertificate = shell.query("#ftps-refresh-certificate");
    let replacedPin = null;
    const endpoint = () => `${hostInput.value.trim()}\n${Number(portInput.value)}\n${transport.value}`;
    const originalIdentityEndpoint = originalPin ? `${originalEndpoint}\n${originalTransport}` : null;
    if (originalPin) pinEndpoint = originalIdentityEndpoint;
    const updateTransport = () => {
      const plaintext = transport.value === "plaintext";
      policyRow.hidden = plaintext;
      refreshCertificate.hidden = plaintext || policy.value === "web_pki" || !pin;
      if (plaintext) {
        certificateNote.innerHTML = "<b>Unverschlüsseltes FTP:</b> Nutzername, Passwort, Verzeichnisnamen und Dateiinhalte sind auf dem Transportweg lesbar. Doppeldecker verbindet trotzdem ohne weitere Bestätigung.";
        shell.primary.textContent = editing ? "Änderungen speichern" : "FTP-Profil speichern";
        return;
      }
      const selectedEndpoint = endpoint();
      if (selectedEndpoint !== pinEndpoint) {
        pin = selectedEndpoint === originalIdentityEndpoint ? originalPin : null;
        pinEndpoint = pin ? originalIdentityEndpoint : null;
        replacedPin = null;
      }
      if (policy.value === "web_pki") {
        certificateNote.textContent = "Vor dem Speichern werden öffentliche Zertifikatskette, Gültigkeit und Servername geprüft; es erfolgt weder Anmeldung noch Dateizugriff. Gültige Zertifikatserneuerungen benötigen keinen neuen Profilpin.";
        shell.primary.textContent = "Prüfen und speichern";
        return;
      }
      if (pin) {
        certificateNote.innerHTML = replacedPin
          ? `<b>Bisheriger Fingerabdruck:</b><br><code>${escapeHtml(fingerprint(replacedPin.sha256))}</code><br><b>Neu ermittelter Fingerabdruck:</b><br><code>${escapeHtml(fingerprint(pin.sha256))}</code><br>Vor dem Speichern vergleichen. Die gewählte Zertifikatsprüfung bleibt erhalten.`
          : `<b>Gebundenes Zertifikat:</b><br><code>${escapeHtml(fingerprint(pin.sha256))}</code><br>Bei einer Änderung von Host oder Port muss das Zertifikat neu ermittelt werden.`;
        shell.primary.textContent = replacedPin ? "Neues Zertifikat binden und speichern" : editing ? "Änderungen speichern" : "FTPS-Profil speichern";
      } else {
        certificateNote.textContent = "Noch kein Zertifikat für diesen Host und Port ermittelt. Der erste Schritt führt nur AUTH TLS aus; es erfolgt weder Anmeldung noch Dateizugriff.";
        shell.primary.textContent = "Zertifikat ermitteln";
      }
    };
    policy.addEventListener("change", updateTransport);
    transport.addEventListener("change", updateTransport);
    hostInput.addEventListener("input", updateTransport);
    portInput.addEventListener("input", updateTransport);
    const discoverCertificate = async () => {
      const host = hostInput.value.trim();
      const port = Number(portInput.value);
      const requestedEndpoint = endpoint();
      const previousPin = pin;
      const request = shell.beginRequest();
      shell.setEditingDisabled(true);
      try {
        const discoveredPin = await invoke("discover_ftps_certificate", { host, port });
        if (!shell.owns(request) || endpoint() !== requestedEndpoint) return;
        replacedPin = requestedEndpoint === originalIdentityEndpoint ? originalPin : previousPin;
        pin = discoveredPin;
        pinEndpoint = requestedEndpoint;
        updateTransport();
        if (!previousPin) {
          certificateNote.innerHTML = `<b>SHA-256-Fingerabdruck:</b><br><code>${escapeHtml(fingerprint(pin.sha256))}</code><br>Vor dem Binden außerhalb Doppeldeckers vergleichen. Die gewählte Zertifikatsprüfung bleibt erhalten.`;
        }
      } catch (error) {
        if (shell.owns(request)) showError("FTPS-Zertifikat", error);
      } finally {
        if (shell.owns(request)) shell.setEditingDisabled(false);
      }
    };
    refreshCertificate.addEventListener("click", discoverCertificate);
    shell.primary.addEventListener("click", async () => {
      const host = hostInput.value.trim();
      const port = Number(portInput.value);
      if (!host || !port) return showDock(`<span class="pill none">Unvollständig</span>Host und gültiger Port fehlen.`, { closable: true });
      updateTransport();
      const plaintext = transport.value === "plaintext";
      if (!plaintext && policy.value === "web_pki") {
        const requestedEndpoint = endpoint();
        const request = shell.beginRequest();
        shell.setEditingDisabled(true);
        try {
          const observed = await invoke("verify_ftps_certificate", { host, port });
          if (!shell.owns(request) || endpoint() !== requestedEndpoint || policy.value !== "web_pki") return;
          certificateNote.innerHTML = `<b>Öffentliche Zertifikatsprüfung erfolgreich:</b><br><code>${escapeHtml(fingerprint(observed.sha256))}</code><br>Dieser Fingerabdruck wird nicht als dauerhafter Pin gespeichert.`;
        } catch (error) {
          if (shell.owns(request)) showError("FTPS-Zertifikat", error);
          return;
        } finally {
          if (shell.owns(request)) shell.setEditingDisabled(false);
        }
      } else if (!plaintext && !pin) {
        await discoverCertificate();
        return;
      }
      const credentials = readPasswordProfileFields(
        { nameInput, userInput, rootInput, secureSecret },
        existing,
        "Server-Root",
      );
      if (!credentials) return;
      const { name, username, requestedRoot, secretId } = credentials;
      const id = editorProfileId;
      const reference = existing?.password_reference || { service: "Doppeldecker", account: id };
      const selectedTransport = plaintext ? "plaintext" : "explicit_tls";
      const selectedPolicy = plaintext ? null : policy.value;
      const profile = {
        id,
        name,
        connection: {
          host,
          alternate_hosts: `${host}\n${port}` === originalEndpoint
            && selectedTransport === originalTransport
            ? (existing?.alternate_hosts || [])
            : [],
          port,
          username,
          requested_root: requestedRoot,
          transport_security: selectedTransport,
          certificate: plaintext || selectedPolicy === "web_pki" ? null : pin,
          certificate_policy: selectedPolicy,
          password_reference: reference,
        },
      };
      const request = shell.beginIrreversible();
      try {
        await saveProfile("ftps", profile, secretId, existingProfile?.revision ?? null);
        if (!shell.owns(request)) return;
        secureSecret.markConsumed();
        shell.endIrreversible(request);
        shell.close();
        if (plaintext) {
          showDock(`<span class="pill update">FTP unverschlüsselt</span>${escapeHtml(name)} · Passwort nur im Schlüsselbund, Transport ohne TLS.`, { closable: true });
        } else if (selectedPolicy === "web_pki") {
          showDock(`<span class="pill ok">FTPS-Profil ${editing ? "aktualisiert" : "gespeichert"}</span>${escapeHtml(name)} · Öffentliche Zertifikatskette, Gültigkeit und Servername geprüft.`, { closable: true });
        } else if (selectedPolicy === "unverified_certificate") {
          showDock(`<span class="pill update">FTPS · Identität ungeprüft</span>${escapeHtml(name)} · TLS und Zertifikatspin aktiv.`, { closable: true });
        } else {
          showDock(`<span class="pill ok">FTPS-Profil ${editing ? "aktualisiert" : "gespeichert"}</span>${escapeHtml(name)} · Passwort nur im Schlüsselbund, Zertifikat exakt gepinnt.`, { closable: true });
        }
      } catch (error) {
        if (await closeAfterOutcomeUnclear(
          shell, secureSecret, request, "FTP-/FTPS-Profil", error,
        )) return;
        await discardAfterSaveFailure(secureSecret, error);
        if (!shell.endIrreversible(request)) return;
        showError("FTP-/FTPS-Profil", error);
      }
    });
    updateTransport();
    }, "#ftps-name");
  }

  /** @param {import("./connection-profile-wire.js").VersionedProfileWire<import("./connection-profile-wire.js").WebDavProfileWire> | null} [existingProfile] */
  async function openWebDavEditor(existingProfile = null, suggested = null) {
    const editing = Boolean(existingProfile);
    const editorProfileId = existingProfile?.id || `webdav_${crypto.randomUUID().replaceAll("-", "")}`;
    const existing = existingProfile?.connection || null;
    const shell = editorShell(
      editing ? "HTTPS-WebDAV-Verbindung bearbeiten" : "HTTPS-WebDAV-Verbindung hinzufügen",
      `<div class="connection-form">
        <label class="wide">Anzeigename<input id="webdav-name" autocomplete="off" /></label>
        <label class="wide">HTTPS-Origin<input id="webdav-origin" value="https://" autocomplete="url" /><small>Nur Schema, Host und optionaler Port; der Collection-Root steht getrennt.</small></label>
        <label>Nutzer<input id="webdav-user" autocomplete="username" /></label>
        <label>Collection-Root<input id="webdav-root" value="/" autocomplete="off" /></label>
        <label class="wide">Zertifikatsprüfung<select id="webdav-policy">
          <option value="web_pki_and_pin">Öffentliche PKI plus exakter Pin</option>
          <option value="pinned_certificate">Extern geprüftes privates Zertifikat plus exakter Pin</option>
        </select><small>Die zweite Wahl ist nur nach unabhängiger Fingerabdruckprüfung sicher.</small></label>
        ${secureSecretControl(editing)}
        <div class="real-warning wide" id="webdav-certificate">Noch kein Zertifikat ermittelt. Der erste Schritt öffnet nur TLS; es erfolgt weder Anmeldung noch HTTP-Dateizugriff.</div>
      </div>`,
      editing ? "Änderungen speichern" : "Zertifikat ermitteln",
    );
    shell.show(() => {
    const nameInput = shell.query("#webdav-name");
    const originInput = shell.query("#webdav-origin");
    if (!(originInput instanceof HTMLInputElement)) throw new Error("Eingabefeld #webdav-origin fehlt.");
    const userInput = shell.query("#webdav-user");
    const rootInput = shell.query("#webdav-root");
    const policyInput = shell.query("#webdav-policy");
    const secureSecret = wireSecureSecret(shell, "webdav", editorProfileId);
    if (editing) {
      nameInput.value = existingProfile.name;
      originInput.value = existing.origin;
      userInput.value = existing.username;
      rootInput.value = existing.requested_root;
      policyInput.value = existing.certificate_policy;
    } else if (suggested) {
      const host = suggested.host || "";
      const authority = host.includes(":") ? `[${host}]` : host;
      const port = Number(suggested.port);
      nameInput.value = suggested.name || "";
      originInput.value = `https://${authority}${port && port !== 443 ? `:${port}` : ""}`;
    }
    const originalOrigin = existing?.origin || null;
    const originalPin = existing?.certificate || null;
    let pin = originalPin;
    let pinOrigin = originalPin ? originalOrigin : null;
    const certificateNote = shell.query("#webdav-certificate");
    const updateCertificate = () => {
      const origin = originInput.value.trim();
      if (originalPin && origin === originalOrigin) {
        pin = originalPin;
        pinOrigin = originalOrigin;
      } else if (origin !== pinOrigin) {
        pin = null;
        pinOrigin = null;
      }
      if (pin) {
        certificateNote.innerHTML = `<b>Gebundenes Zertifikat:</b><br><code>${escapeHtml(fingerprint(pin.sha256))}</code><br>Bei einer Änderung der HTTPS-Origin muss das Zertifikat neu ermittelt werden.`;
        shell.primary.textContent = editing ? "Änderungen speichern" : "Geprüftes Zertifikat binden und speichern";
      } else {
        certificateNote.textContent = "Noch kein Zertifikat für diese HTTPS-Origin ermittelt. Der erste Schritt öffnet nur TLS; es erfolgt weder Anmeldung noch HTTP-Dateizugriff.";
        shell.primary.textContent = "Zertifikat ermitteln";
      }
    };
    originInput.addEventListener("input", updateCertificate);
    updateCertificate();
    shell.primary.addEventListener("click", async () => {
      const origin = originInput.value.trim();
      if (!origin) return showDock(`<span class="pill none">Unvollständig</span>HTTPS-Origin fehlt.`, { closable: true });
      updateCertificate();
      if (!pin) {
        const requestedOrigin = origin;
        const request = shell.beginRequest();
        shell.setEditingDisabled(true);
        try {
          const discoveredPin = await invoke("discover_webdav_certificate", { origin });
          if (!shell.owns(request) || originInput.value.trim() !== requestedOrigin) return;
          pin = discoveredPin;
          pinOrigin = requestedOrigin;
          certificateNote.innerHTML = `<b>SHA-256-Fingerabdruck außerhalb Doppeldeckers vergleichen:</b><br><code>${escapeHtml(fingerprint(pin.sha256))}</code><br>Nur wenn er zur erwarteten Origin gehört, im zweiten Schritt binden.`;
          shell.primary.textContent = "Geprüftes Zertifikat binden und speichern";
        } catch (error) {
          if (!shell.owns(request)) return;
          showError("WebDAV-Zertifikat", error);
        } finally {
          if (shell.owns(request)) {
            shell.setEditingDisabled(false);
          }
        }
        return;
      }
      const credentials = readPasswordProfileFields(
        { nameInput, userInput, rootInput, secureSecret },
        existing,
        "Collection-Root",
      );
      if (!credentials) return;
      const { name, username, requestedRoot, secretId } = credentials;
      const id = editorProfileId;
      const reference = existing?.password_reference || { service: "Doppeldecker", account: id };
      const selectedPolicy = policyInput.value;
      const profile = {
        id,
        name,
        connection: {
          origin,
          username,
          requested_root: requestedRoot,
          certificate: pin,
          certificate_policy: selectedPolicy,
          password_reference: reference,
        },
      };
      const request = shell.beginIrreversible();
      try {
        await saveProfile("webdav", profile, secretId, existingProfile?.revision ?? null);
        if (!shell.owns(request)) return;
        secureSecret.markConsumed();
        shell.endIrreversible(request);
        shell.close();
        showDock(`<span class="pill ok">WebDAV-Profil ${editing ? "aktualisiert" : "gespeichert"}</span>${escapeHtml(name)} · Passwort nur im Schlüsselbund, Zertifikat exakt gepinnt.`, { closable: true });
      } catch (error) {
        if (await closeAfterOutcomeUnclear(
          shell, secureSecret, request, "WebDAV-Profil", error,
        )) return;
        await discardAfterSaveFailure(secureSecret, error);
        if (!shell.endIrreversible(request)) return;
        showError("WebDAV-Profil", error);
      }
    });
    }, "#webdav-name");
  }

  /** @param {import("./connection-profile-wire.js").VersionedProfileWire<import("./connection-profile-wire.js").SmbProfileWire> | null} [existingProfile] */
  async function openSmbEditor(existingProfile = null, suggested = null) {
    const editing = Boolean(existingProfile);
    const editorProfileId = existingProfile?.id || `smb_${crypto.randomUUID().replaceAll("-", "")}`;
    const existing = existingProfile?.connection || null;
    const shell = editorShell(
      editing ? "Verschlüsselte SMB3-Verbindung bearbeiten" : "Verschlüsselte SMB3-Verbindung hinzufügen",
      `<div class="connection-form">
        <label class="wide">Anzeigename<input id="smb-name" autocomplete="off" /></label>
        <label>Host<input id="smb-host" autocomplete="off" /></label>
        <label>Port<input id="smb-port" type="number" min="1" max="65535" value="445" /></label>
        <label>Freigabe<input id="smb-share" autocomplete="off" /></label>
        <label>Server-Root<input id="smb-root" value="/" autocomplete="off" /></label>
        <label>Nutzer<input id="smb-user" autocomplete="username" /></label>
        <label>Domäne (optional)<input id="smb-domain" autocomplete="off" /></label>
        ${secureSecretControl(editing)}
        <div class="real-warning wide" id="smb-identity">Noch keine Identität ermittelt. Der erste Schritt führt nur SMB NEGOTIATE aus; es erfolgt weder Anmeldung noch Freigabe- oder Dateizugriff.</div>
      </div>`,
      editing ? "Änderungen speichern" : "Server-GUID ermitteln",
    );
    shell.show(() => {
    const nameInput = shell.query("#smb-name");
    const hostInput = shell.query("#smb-host");
    if (!(hostInput instanceof HTMLInputElement)) throw new Error("Eingabefeld #smb-host fehlt.");
    const portInput = shell.query("#smb-port");
    const shareInput = shell.query("#smb-share");
    const rootInput = shell.query("#smb-root");
    const userInput = shell.query("#smb-user");
    const domainInput = shell.query("#smb-domain");
    const secureSecret = wireSecureSecret(shell, "smb", editorProfileId);
    if (editing) {
      nameInput.value = existingProfile.name;
      hostInput.value = existing.host;
      portInput.value = String(existing.port);
      shareInput.value = existing.share;
      rootInput.value = existing.requested_root;
      userInput.value = existing.username;
      domainInput.value = existing.domain;
    } else if (suggested) {
      nameInput.value = suggested.name || "";
      hostInput.value = suggested.host || "";
      portInput.value = String(suggested.port || 445);
    }
    const originalEndpoint = existing ? `${existing.host}\n${existing.port}` : null;
    const originalGuid = existing?.server_guid || null;
    let discovery = originalGuid ? { server_guid: originalGuid } : null;
    let discoveryEndpoint = originalGuid ? originalEndpoint : null;
    const endpoint = () => `${hostInput.value.trim()}\n${Number(portInput.value)}`;
    const identityNote = shell.query("#smb-identity");
    const updateIdentity = () => {
      const selectedEndpoint = endpoint();
      if (originalGuid && selectedEndpoint === originalEndpoint) {
        discovery = { server_guid: originalGuid };
        discoveryEndpoint = originalEndpoint;
      } else if (selectedEndpoint !== discoveryEndpoint) {
        discovery = null;
        discoveryEndpoint = null;
      }
      if (discovery) {
        identityNote.innerHTML = `<b>Gebundene Server-GUID:</b><br><code>${escapeHtml(digestHex(discovery.server_guid))}</code><br>Bei einer Änderung von Host oder Port muss die Identität neu ermittelt werden.`;
        shell.primary.textContent = editing ? "Änderungen speichern" : "Geprüfte GUID binden und speichern";
      } else {
        identityNote.textContent = "Noch keine Identität für diesen Host und Port ermittelt. Der erste Schritt führt nur SMB NEGOTIATE aus; es erfolgt weder Anmeldung noch Freigabe- oder Dateizugriff.";
        shell.primary.textContent = "Server-GUID ermitteln";
      }
    };
    hostInput.addEventListener("input", updateIdentity);
    portInput.addEventListener("input", updateIdentity);
    updateIdentity();
    shell.primary.addEventListener("click", async () => {
      const host = hostInput.value.trim();
      const port = Number(portInput.value);
      if (!host || !port) return showDock(`<span class="pill none">Unvollständig</span>Host und Port fehlen.`, { closable: true });
      updateIdentity();
      if (!discovery) {
        const requestedEndpoint = endpoint();
        const request = shell.beginRequest();
        shell.setEditingDisabled(true);
        try {
          const discoveredIdentity = await invoke("discover_smb_server", { host, port });
          if (!shell.owns(request) || endpoint() !== requestedEndpoint) return;
          const dialect = `0x${Number(discoveredIdentity.dialect).toString(16).padStart(4, "0")}`;
          if (Number(discoveredIdentity.dialect) !== 0x0311 || !discoveredIdentity.encryption_capable) {
            discovery = null;
            discoveryEndpoint = null;
            throw new Error(`Der Server bietet ${dialect} ohne die benötigte SMB-3.1.1-Verschlüsselung an.`);
          }
          discovery = discoveredIdentity;
          discoveryEndpoint = requestedEndpoint;
          identityNote.innerHTML = `<b>Server-GUID außerhalb Doppeldeckers vergleichen:</b><br><code>${escapeHtml(digestHex(discovery.server_guid))}</code><br>Dialekt ${escapeHtml(dialect)} · Verschlüsselung angeboten. Nur SMB 3.1.1 mit passender GUID wird gespeichert und verbunden.`;
          shell.primary.textContent = "Geprüfte GUID binden und speichern";
        } catch (error) {
          if (!shell.owns(request)) return;
          showError("SMB-Identität", error);
        } finally {
          if (shell.owns(request)) shell.setEditingDisabled(false);
        }
        return;
      }
      const name = nameInput.value.trim();
      const share = shareInput.value.trim();
      const requestedRoot = rootInput.value.trim();
      const username = userInput.value.trim();
      const domain = domainInput.value.trim();
      const secretId = secureSecret.id();
      if (!name || !share || !requestedRoot || !username || (!secretId && !existing?.password_reference)) {
        return showDock(`<span class="pill none">Unvollständig</span>Name, Freigabe, Root und Nutzer werden benötigt; bei neuen Profilen auch das Passwort.`, { closable: true });
      }
      const id = editorProfileId;
      const reference = existing?.password_reference || { service: "Doppeldecker", account: id };
      const profile = {
        id,
        name,
        connection: {
          host,
          port,
          server_guid: discovery.server_guid,
          share,
          username,
          domain,
          requested_root: requestedRoot,
          password_reference: reference,
        },
      };
      const request = shell.beginIrreversible();
      try {
        await saveProfile("smb", profile, secretId, existingProfile?.revision ?? null);
        if (!shell.owns(request)) return;
        secureSecret.markConsumed();
        shell.endIrreversible(request);
        shell.close();
        showDock(`<span class="pill ok">SMB-Profil ${editing ? "aktualisiert" : "gespeichert"}</span>${escapeHtml(name)} · Passwort nur im Schlüsselbund, Server-GUID exakt gepinnt.`, { closable: true });
      } catch (error) {
        if (await closeAfterOutcomeUnclear(
          shell, secureSecret, request, "SMB-Profil", error,
        )) return;
        await discardAfterSaveFailure(secureSecret, error);
        if (!shell.endIrreversible(request)) return;
        showError("SMB-Profil", error);
      }
    });
    }, "#smb-name");
  }

  const editors = {
    sftp: openSftpEditor,
    ftps: openFtpsEditor,
    webdav: openWebDavEditor,
    smb: openSmbEditor,
  };

  function openProfileEditor(protocol, profile) {
    editors[protocol]?.(profile);
  }

  /** DNS-SD-Funde sind ausschließlich Vorschläge. Der leere neue Editor übernimmt
   * Anzeigename, Host und Port; Nutzer, Secret, Share/Root und die öffentliche
   * Identitätsprüfung bleiben vollständig offen. */
  function openDiscoveredService(service) {
    editors[service?.protocol]?.(null, service);
  }

  return {
    openDiscoveredService,
    openFtpsEditor,
    openProfileEditor,
    openProtocolPicker,
    openSftpEditor,
    openSmbEditor,
    openWebDavEditor,
  };
}
