import { fingerprint } from "./connections-ui.js";
import { invoke } from "./tauri-api.js";
import { escapeHtml } from "./ui-utils.js";

export function createSftpEditor({
  editorShell,
  secureSecretControl,
  wireSecureSecret,
  saveProfile,
  closeAfterOutcomeUnclear,
  discardAfterSaveFailure,
  showDock,
  showError,
}) {
  const sameBytes = (left, right) => Array.isArray(left)
    && Array.isArray(right)
    && left.length === right.length
    && left.every((value, index) => value === right[index]);

  /** @param {import("./connection-profile-wire.js").VersionedProfileWire<import("./connection-profile-wire.js").SftpProfileWire> | null} [existingProfile] */
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
    if (!(hostInput instanceof HTMLInputElement)) throw new Error("Eingabefeld #sftp-host fehlt.");
    const portInput = shell.query("#sftp-port");
    const userInput = shell.query("#sftp-user");
    const rootInput = shell.query("#sftp-root");
    const secureSecret = wireSecureSecret(shell, "sftp", editorProfileId);
    const auth = shell.query("#sftp-auth");
    const agentInput = shell.query("#sftp-agent");
    const passwordRow = shell.query("#sftp-password-row");
    const agentRow = shell.query("#sftp-agent-row");
    const renderAgentKeys = () => {
      if (agentKeys.length && !auth.querySelector('option[value="agent"]')) {
        const option = document.createElement("option");
        option.value = "agent";
        option.textContent = "Gebundener SSH-Agent-Schlüssel";
        auth.append(option);
      }
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
  return openSftpEditor;
}
