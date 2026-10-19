import { fingerprint } from "./connections-ui.js";
import { hideModal, showOwnedModal } from "./modal.js";
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

  const sameBytes = (left, right) => Array.isArray(left)
    && Array.isArray(right)
    && left.length === right.length
    && left.every((value, index) => value === right[index]);

  function wireSecureSecret(shell, protocol, profileId) {
    const button = shell.query(".native-secret-button");
    const status = shell.query(".native-secret-status");
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

  async function openSftpEditor(existingProfile = null, suggested = null) {
    const editing = Boolean(existingProfile);
    const editorProfileId = existingProfile?.id || `sftp_${crypto.randomUUID().replaceAll("-", "")}`;
    const existing = existingProfile?.connection || null;
    const existingAuthentication = existing?.authentication || null;
    // Der vorhandene Agent-Schlüssel ist schon synchron als Platzhalter verfügbar.
    // Die native Agent-Abfrage startet erst, nachdem der Editor seinen Modal-Lease
    // besitzt; so kann ihre verspätete Antwort keinen anderen Dialog verändern.
    let agentKeys = existingAuthentication?.type === "ssh_agent"
      ? [{
        comment: "Bisher gebundener SSH-Agent-Schlüssel",
        sha256: existingAuthentication.public_key_sha256,
      }]
      : [];
    const preserveUnsupportedAuthentication = editing
      && !["password", "ssh_agent"].includes(existingAuthentication?.type);
    const shell = editorShell(
      editing ? "SFTP-Verbindung bearbeiten" : "SFTP-Verbindung hinzufügen",
      `<div class="connection-form">
        <label class="wide">Anzeigename<input id="sftp-name" autocomplete="off" /></label>
        <label>Host<input id="sftp-host" autocomplete="off" /></label>
        <label>Port<input id="sftp-port" type="number" min="1" max="65535" value="22" /></label>
        <label>Nutzer<input id="sftp-user" autocomplete="username" /></label>
        <label>Server-Root<input id="sftp-root" value="/" autocomplete="off" /></label>
        <label class="wide">Anmeldung<select id="sftp-auth">
          <option value="password">Passwort im System-Schlüsselbund</option>
          ${agentKeys.length ? '<option value="agent">Gebundener SSH-Agent-Schlüssel</option>' : ""}
          ${preserveUnsupportedAuthentication ? '<option value="existing">Vorhandene Anmeldung unverändert lassen</option>' : ""}
        </select></label>
        <div id="sftp-password-row">${secureSecretControl(editing)}</div>
        <label class="wide" id="sftp-agent-row" hidden>SSH-Agent-Schlüssel<select id="sftp-agent">${agentKeys.map((key, index) => `<option value="${index}">${escapeHtml(key.comment || key.algorithm)} · ${escapeHtml(fingerprint(key.sha256))}</option>`).join("")}</select></label>
        <div class="real-warning wide" id="sftp-hostkey">Noch kein Hostkey ermittelt. Der erste Schritt liest ausschließlich den öffentlichen Serverschlüssel; es erfolgt keine Anmeldung.</div>
      </div>`,
      editing ? "Änderungen speichern" : "Hostkey ermitteln",
    );
    shell.show(() => {
    const nameInput = shell.query("#sftp-name");
    const hostInput = shell.query("#sftp-host");
    const portInput = shell.query("#sftp-port");
    const userInput = shell.query("#sftp-user");
    const rootInput = shell.query("#sftp-root");
    const secureSecret = wireSecureSecret(shell, "sftp", editorProfileId);
    const agentInput = shell.query("#sftp-agent");
    const passwordRow = shell.query("#sftp-password-row");
    const agentRow = shell.query("#sftp-agent-row");
    const renderAgentKeys = () => {
      agentInput.innerHTML = agentKeys.map((key, index) =>
        `<option value="${index}">${escapeHtml(key.comment || key.algorithm)} · ${escapeHtml(fingerprint(key.sha256))}</option>`,
      ).join("");
      const selectedIndex = existingAuthentication?.type === "ssh_agent"
        ? agentKeys.findIndex(key => sameBytes(
          key.sha256,
          existingAuthentication.public_key_sha256,
        ))
        : -1;
      if (selectedIndex >= 0) agentInput.value = String(selectedIndex);
    };
    if (editing) {
      nameInput.value = existingProfile.name;
      hostInput.value = existing.host;
      portInput.value = String(existing.port);
      userInput.value = existing.username;
      rootInput.value = existing.requested_root;
    } else if (suggested) {
      nameInput.value = suggested.name || "";
      hostInput.value = suggested.host || "";
      portInput.value = String(suggested.port || 22);
    }
    const auth = shell.query("#sftp-auth");
    if (existingAuthentication?.type === "ssh_agent") {
      auth.value = "agent";
    } else if (preserveUnsupportedAuthentication) {
      auth.value = "existing";
    }
    renderAgentKeys();
    const updateAuthentication = () => {
      passwordRow.hidden = auth.value !== "password";
      agentRow.hidden = auth.value !== "agent";
    };
    auth.addEventListener("change", () => {
      if (auth.value !== "password") secureSecret.discard();
      updateAuthentication();
    });
    updateAuthentication();

    const agentRequest = shell.beginRequest();
    shell.setEditingDisabled(true);
    void (async () => {
      let discoveredKeys = [];
      try {
        discoveredKeys = await invoke("list_sftp_agent_keys");
      } catch (_) {
        // Passwortauthentifizierung bleibt ohne laufenden SSH-Agenten verfügbar.
      }
      if (!shell.owns(agentRequest)) return;
      if (editing) {
        try {
          const currentProfiles = await invoke("list_sftp_profiles");
          if (!shell.owns(agentRequest)) return;
          const current = currentProfiles.find(profile => profile.id === existingProfile.id);
          if (!current || JSON.stringify(current) !== JSON.stringify(existingProfile)) {
            shell.setEditingDisabled(false);
            shell.close();
            showDock(
              "<span class=\"pill none\">Profil geändert</span>Das SFTP-Profil wurde inzwischen in einem anderen Fenster geändert oder entfernt. Bitte den Editor neu öffnen.",
              { closable: true },
            );
            return;
          }
        } catch (error) {
          if (!shell.owns(agentRequest)) return;
          shell.setEditingDisabled(false);
          shell.close();
          showError("SFTP-Profilprüfung", error);
          return;
        }
      }
      agentKeys = discoveredKeys.slice();
      if (
        existingAuthentication?.type === "ssh_agent"
        && !agentKeys.some(key => sameBytes(
          key.sha256,
          existingAuthentication.public_key_sha256,
        ))
      ) {
        // Ein momentan nicht geladener Agent-Schlüssel darf beim bloßen
        // Umbenennen nicht verloren gehen.
        agentKeys.unshift({
          comment: "Bisher gebundener SSH-Agent-Schlüssel",
          sha256: existingAuthentication.public_key_sha256,
        });
      }
      renderAgentKeys();
      if (shell.owns(agentRequest)) shell.setEditingDisabled(false);
    })();

    const originalEndpoint = existing ? `${existing.host}\n${existing.port}` : null;
    const originalPin = existing?.host_key || null;
    let pin = originalPin;
    let pinEndpoint = originalEndpoint;
    const endpoint = () => `${hostInput.value.trim()}\n${Number(portInput.value)}`;
    const hostkeyNote = shell.query("#sftp-hostkey");
    const updateHostkey = () => {
      const selectedEndpoint = endpoint();
      if (originalPin && selectedEndpoint === originalEndpoint) {
        pin = originalPin;
        pinEndpoint = originalEndpoint;
      } else if (selectedEndpoint !== pinEndpoint) {
        pin = null;
        pinEndpoint = null;
      }
      if (pin) {
        hostkeyNote.innerHTML = `<b>Gebundener Hostkey:</b><br><code>${escapeHtml(pin.algorithm)} · ${escapeHtml(fingerprint(pin.sha256))}</code><br>Bei einer Änderung von Host oder Port muss der Schlüssel neu ermittelt und geprüft werden.`;
        shell.primary.textContent = editing ? "Änderungen speichern" : "Geprüften Hostkey binden und speichern";
      } else {
        hostkeyNote.textContent = "Noch kein Hostkey für diesen Host und Port ermittelt. Der erste Schritt liest ausschließlich den öffentlichen Serverschlüssel; es erfolgt keine Anmeldung.";
        shell.primary.textContent = "Hostkey ermitteln";
      }
    };
    hostInput.addEventListener("input", updateHostkey);
    portInput.addEventListener("input", updateHostkey);
    updateHostkey();

    shell.primary.addEventListener("click", async () => {
      const host = hostInput.value.trim();
      const port = Number(portInput.value);
      if (!host || !port) {
        return showDock(`<span class="pill none">Unvollständig</span>Host und gültiger Port fehlen.`, { closable: true });
      }
      updateHostkey();
      if (!pin) {
        const requestedEndpoint = endpoint();
        const request = shell.beginRequest();
        shell.setEditingDisabled(true);
        try {
          const discoveredPin = await invoke("discover_sftp_host_key", { host, port });
          if (!shell.owns(request) || endpoint() !== requestedEndpoint) return;
          pin = discoveredPin;
          pinEndpoint = requestedEndpoint;
          hostkeyNote.innerHTML = `<b>Öffentlichen Hostkey außerhalb Doppeldeckers vergleichen:</b><br><code>${escapeHtml(pin.algorithm)} · ${escapeHtml(fingerprint(pin.sha256))}</code><br>Nur wenn er zum erwarteten Server gehört, im zweiten Schritt speichern.`;
          shell.primary.textContent = "Geprüften Hostkey binden und speichern";
        } catch (error) {
          if (!shell.owns(request)) return;
          showError("Hostkey-Ermittlung", error);
        } finally {
          if (shell.owns(request)) shell.setEditingDisabled(false);
        }
        return;
      }
      const name = nameInput.value.trim();
      const username = userInput.value.trim();
      const requestedRoot = rootInput.value.trim();
      if (!name || !username || !requestedRoot) {
        return showDock(`<span class="pill none">Unvollständig</span>Name, Nutzer und Server-Root fehlen.`, { closable: true });
      }
      const id = editorProfileId;
      let authentication;
      if (auth.value === "agent") {
        const key = agentKeys[Number(agentInput.value)];
        if (!key) return;
        authentication = { type: "ssh_agent", public_key_sha256: key.sha256 };
      } else if (auth.value === "existing" && existingAuthentication) {
        authentication = existingAuthentication;
      } else {
        const secretId = secureSecret.id();
        if (!secretId && existingAuthentication?.type !== "password") {
          return showDock(`<span class="pill none">Passwort fehlt</span>Es wird ausschließlich in den System-Schlüsselbund geschrieben.`, { closable: true });
        }
        const reference = existingAuthentication?.type === "password"
          ? existingAuthentication.secret
          : { service: "Doppeldecker", account: id };
        const request = shell.beginIrreversible();
        authentication = { type: "password", secret: reference };
        const profile = {
          id,
          name,
          connection: {
            host,
            alternate_hosts: `${host}\n${port}` === originalEndpoint
              ? (existing?.alternate_hosts || [])
              : [],
            port,
            username,
            requested_root: requestedRoot,
            host_key: pin,
            authentication,
          },
        };
        try {
          await saveProfile("sftp", profile, secretId, existingProfile?.revision ?? null);
          if (!shell.owns(request)) return;
          secureSecret.markConsumed();
          shell.endIrreversible(request);
          shell.close();
          showDock(`<span class="pill ok">SFTP-Profil ${editing ? "aktualisiert" : "gespeichert"}</span>${escapeHtml(name)} · nur Referenzen und öffentlicher Hostkey wurden persistiert.`, { closable: true });
        } catch (error) {
          if (await closeAfterOutcomeUnclear(
            shell, secureSecret, request, "SFTP-Profil", error,
          )) return;
          await discardAfterSaveFailure(secureSecret, error);
          if (!shell.endIrreversible(request)) return;
          showError("SFTP-Profil", error);
        }
        return;
      }
      const profile = {
        id,
        name,
        connection: {
          host,
          alternate_hosts: `${host}\n${port}` === originalEndpoint
            ? (existing?.alternate_hosts || [])
            : [],
          port,
          username,
          requested_root: requestedRoot,
          host_key: pin,
          authentication,
        },
      };
      const request = shell.beginIrreversible();
      try {
        await saveProfile("sftp", profile, null, existingProfile?.revision ?? null);
        if (!shell.owns(request)) return;
        shell.endIrreversible(request);
        shell.close();
        showDock(`<span class="pill ok">SFTP-Profil ${editing ? "aktualisiert" : "gespeichert"}</span>${escapeHtml(name)} · nur Referenzen und öffentlicher Hostkey wurden persistiert.`, { closable: true });
      } catch (error) {
        if (await closeAfterOutcomeUnclear(
          shell, secureSecret, request, "SFTP-Profil", error,
        )) return;
        if (!shell.endIrreversible(request)) return;
        showError("SFTP-Profil", error);
      }
    });
    }, "#sftp-name");
  }

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
          <option value="web_pki_and_pin">Öffentliche PKI plus exakter Pin</option>
          <option value="pinned_certificate">Extern geprüftes privates Zertifikat plus exakter Pin</option>
          <option value="unverified_certificate">Exakter Pin ohne Identitätsprüfung</option>
        </select><small>„Ohne Identitätsprüfung“ verschlüsselt die Verbindung und bindet das beobachtete Zertifikat, bestätigt aber nicht, wem der Server gehört.</small></label>
        ${secureSecretControl(editing)}
        <div class="real-warning wide" id="ftps-certificate">Noch kein Zertifikat ermittelt. Der erste Schritt führt nur AUTH TLS aus; es erfolgt weder Anmeldung noch Dateizugriff.</div>
      </div>`,
      "Zertifikat ermitteln",
    );
    shell.show(() => {
    const nameInput = shell.query("#ftps-name");
    const hostInput = shell.query("#ftps-host");
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
    const endpoint = () => `${hostInput.value.trim()}\n${Number(portInput.value)}\n${transport.value}`;
    const originalIdentityEndpoint = originalPin ? `${originalEndpoint}\n${originalTransport}` : null;
    if (originalPin) pinEndpoint = originalIdentityEndpoint;
    const updateTransport = () => {
      const plaintext = transport.value === "plaintext";
      policyRow.hidden = plaintext;
      if (plaintext) {
        certificateNote.innerHTML = "<b>Unverschlüsseltes FTP:</b> Nutzername, Passwort, Verzeichnisnamen und Dateiinhalte sind auf dem Transportweg lesbar. Doppeldecker verbindet trotzdem ohne weitere Bestätigung.";
        shell.primary.textContent = editing ? "Änderungen speichern" : "FTP-Profil speichern";
        return;
      }
      const selectedEndpoint = endpoint();
      if (originalPin && selectedEndpoint === originalIdentityEndpoint) {
        pin = originalPin;
        pinEndpoint = originalIdentityEndpoint;
      } else if (selectedEndpoint !== pinEndpoint) {
        pin = null;
        pinEndpoint = null;
      }
      if (pin) {
        certificateNote.innerHTML = `<b>Gebundenes Zertifikat:</b><br><code>${escapeHtml(fingerprint(pin.sha256))}</code><br>Bei einer Änderung von Host oder Port muss das Zertifikat neu ermittelt werden.`;
        shell.primary.textContent = editing ? "Änderungen speichern" : "FTPS-Profil speichern";
      } else {
        certificateNote.textContent = "Noch kein Zertifikat für diesen Host und Port ermittelt. Der erste Schritt führt nur AUTH TLS aus; es erfolgt weder Anmeldung noch Dateizugriff.";
        shell.primary.textContent = "Zertifikat ermitteln";
      }
    };
    transport.addEventListener("change", updateTransport);
    hostInput.addEventListener("input", updateTransport);
    portInput.addEventListener("input", updateTransport);
    shell.primary.addEventListener("click", async () => {
      const host = hostInput.value.trim();
      const port = Number(portInput.value);
      if (!host || !port) return showDock(`<span class="pill none">Unvollständig</span>Host und gültiger Port fehlen.`, { closable: true });
      updateTransport();
      const plaintext = transport.value === "plaintext";
      if (!plaintext && !pin) {
        const requestedEndpoint = endpoint();
        const request = shell.beginRequest();
        shell.setEditingDisabled(true);
        try {
          const discoveredPin = await invoke("discover_ftps_certificate", { host, port });
          if (!shell.owns(request) || endpoint() !== requestedEndpoint) return;
          pin = discoveredPin;
          pinEndpoint = requestedEndpoint;
          const ipLiteral = /^\d{1,3}(?:\.\d{1,3}){3}$/.test(host) || host.includes(":");
          // Früher wechselte die Auswahl hier still von „öffentliche PKI plus Pin"
          // auf „Zertifikat ungeprüft". Eine ausdrücklich gewählte Prüfstufe darf die
          // Oberfläche nicht ohne Rückfrage absenken; sichtbar wurde es nur, wenn der
          // Nutzer das Auswahlfeld danach noch einmal las. Die Wahl bleibt jetzt
          // stehen, und der Hinweistext benennt die Lage.
          const ipLiteralConflict = ipLiteral && policy.value === "web_pki_and_pin";
          certificateNote.innerHTML = policy.value === "unverified_certificate"
            ? `<b>TLS aktiv, Identität ungeprüft:</b><br><code>${escapeHtml(fingerprint(pin.sha256))}</code><br>Das Zertifikat wird gegen spätere Wechsel gepinnt; eine öffentliche Namensprüfung findet nicht statt.`
            : ipLiteralConflict
              ? `<b>SHA-256-Fingerabdruck außerhalb Doppeldeckers vergleichen:</b><br><code>${escapeHtml(fingerprint(pin.sha256))}</code><br><b>Achtung:</b> Für eine IP-Adresse ohne passenden Namenseintrag kann die öffentliche PKI-Prüfung nicht gelingen. Entweder einen Hostnamen eintragen oder die Prüfstufe bewusst auf „Zertifikat ungeprüft" stellen.`
              : `<b>SHA-256-Fingerabdruck außerhalb Doppeldeckers vergleichen:</b><br><code>${escapeHtml(fingerprint(pin.sha256))}</code><br>Nur wenn er zum erwarteten Server gehört, im zweiten Schritt binden.`;
          shell.primary.textContent = "FTPS-Profil speichern";
        } catch (error) {
          if (!shell.owns(request)) return;
          showError("FTPS-Zertifikat", error);
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
          certificate: plaintext ? null : pin,
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
