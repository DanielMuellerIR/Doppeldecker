import { confirmModal } from "./modal.js";
import { openMenu } from "./context-menu.js";
import { invoke } from "./tauri-api.js";
import { $, escapeHtml } from "./ui-utils.js";

export const PROTOCOLS = ["sftp", "ftps", "webdav", "smb"];

const CONFIG = {
  sftp: {
    label: "SFTP",
    secretNote: "Ein möglicherweise vorhandenes Secret bleibt absichtlich im System-Schlüsselbund.",
  },
  ftps: {
    label: "FTP / FTPS",
    secretNote: "Das Secret bleibt absichtlich im System-Schlüsselbund.",
  },
  webdav: {
    label: "WebDAV",
    secretNote: "Das Secret bleibt absichtlich im System-Schlüsselbund.",
  },
  smb: {
    label: "SMB",
    secretNote: "Das Secret bleibt absichtlich im System-Schlüsselbund.",
  },
};

export function fingerprint(bytes) {
  return `SHA256:${btoa(String.fromCharCode(...bytes)).replace(/=+$/, "")}`;
}

function ftpDisplay(connection) {
  if (connection.transport_security === "plaintext") {
    return {
      label: "FTP",
      detail: "unverschlüsselt",
      warning: true,
    };
  }
  if (connection.certificate_policy === "unverified_certificate") {
    return {
      label: "FTPS",
      detail: "TLS · Identität ungeprüft",
      warning: true,
    };
  }
  return {
    label: "FTPS",
    detail: "TLS",
    warning: false,
  };
}

/** Hält ausschließlich die geheimnisfreien Profil-DTOs und deren Seitenleisten-
 * Bedienung. Passwörter bleiben in den nativen Editor-Commands und im Schlüsselbund.
 * Alle Protokolle teilen sich eine Liste; das Protokoll ist am Icon erkennbar. */
export function createConnectionsUi({
  activeSide,
  disablePane,
  invalidateProfileTabs,
  panes,
  setPaneLocation,
  showError,
  updateProfileName,
}) {
  const profiles = { sftp: [], ftps: [], webdav: [], smb: [] };
  let openProfileEditor = null;
  // Während ein Profilereignis den aktuellen Backendstand nachliest, darf weder
  // die alte Seitenleistenzeile noch ein Favorit dessen überholte Revision öffnen.
  // Das Token verhindert, dass zwei überlappende Ereignisse einander zu früh
  // wieder freigeben.
  const pendingProfileReads = new Map();
  let profileReadSequence = 0;
  // Jeder Profilabruf erhält pro Protokoll eine monotone Generation. Damit kann
  // weder ein langsamer älterer Event-Readback noch ein vor einem Save gestarteter
  // Startup-Abruf einen bereits sichtbaren neueren Profilstand zurückschreiben.
  const profileLoadSequences = Object.fromEntries(PROTOCOLS.map(protocol => [protocol, 0]));
  // Der zuletzt gestartete Profilabruf je Protokoll. Wer seinen eigenen Abruf
  // verworfen bekommen hat, muss auf diesen hier warten, bevor er eine gesperrte
  // Profilzeile wieder freigibt — sonst wäre sie mit der alten Liste bedienbar.
  const latestProfileLoads = Object.fromEntries(PROTOCOLS.map(protocol => [protocol, null]));

  function invalidateProfileLoads(protocol) {
    profileLoadSequences[protocol] += 1;
    return profileLoadSequences[protocol];
  }

  // Wartet, bis der aktuell jüngste Profilabruf dieses Protokolls fertig ist.
  // Startet währenddessen ein noch neuerer, wird auch auf diesen gewartet.
  async function awaitLatestProfileLoad(protocol) {
    let pending = latestProfileLoads[protocol];
    while (pending) {
      // Ein Fehler des fremden Abrufs ist dort schon gemeldet worden. Hier darf er
      // nur das Warten beenden, nicht den eigenen Fehlerweg überschreiben.
      await pending.catch(() => {});
      if (latestProfileLoads[protocol] === pending) return;
      pending = latestProfileLoads[protocol];
    }
  }

  // Lebende Backend-Sitzungen (`protokoll:id`). Sichtbar gemacht über den
  // Eject-Knopf: Er erscheint genau dann, wenn eine Sitzung wirklich offen ist —
  // auch wenn beide Panes längst wieder lokal unterwegs sind.
  let activeConnections = new Set();
  let activeKey = null;
  let refreshingActive = false;
  let refreshQueued = false;

  async function refreshActive() {
    // Kein Verwerfen paralleler Anfragen: Während ein Abruf läuft, wird ein
    // weiterer Wunsch gemerkt und danach nachgeholt — sonst bliebe der Status
    // stehen, wenn das Listing schneller fertig ist als der erste Abruf.
    if (refreshingActive) {
      refreshQueued = true;
      return;
    }
    refreshingActive = true;
    try {
      const list = await invoke("list_active_connections");
      const key = list.map(c => `${c.protocol}:${c.profile_id}`).sort().join("|");
      if (key !== activeKey) {
        activeKey = key;
        activeConnections = new Set(key ? key.split("|") : []);
        render();
      }
    } catch (_) {
      // Ohne Statusantwort bleibt die letzte bekannte Anzeige stehen.
    } finally {
      refreshingActive = false;
      if (refreshQueued) {
        refreshQueued = false;
        refreshActive();
      }
    }
  }

  async function load(protocol) {
    const token = invalidateProfileLoads(protocol);
    const running = (async () => {
      try {
        const loaded = await invoke(`list_${protocol}_profiles`);
        if (profileLoadSequences[protocol] !== token) return;
        profiles[protocol] = loaded;
      } catch (error) {
        if (profileLoadSequences[protocol] !== token) return;
        profiles[protocol] = [];
        showError(`${CONFIG[protocol].label}-Profile`, error);
      }
      render();
    })();
    latestProfileLoads[protocol] = running;
    return running;
  }

  async function loadAll() {
    for (const protocol of PROTOCOLS) await load(protocol);
  }

  function stableConnection(value) {
    if (Array.isArray(value)) return value.map(stableConnection);
    if (!value || typeof value !== "object") return value;
    return Object.fromEntries(
      Object.entries(value)
        .filter(([key, field]) => key !== "alternate_hosts" || field?.length)
        .sort(([left], [right]) => left.localeCompare(right))
        .map(([key, field]) => [key, stableConnection(field)]),
    );
  }

  function disableProfilePanes(protocol, profileId) {
    invalidateProfileTabs(protocol, profileId);
    const affectedSides = ["left", "right"].filter(
      side => panes[side].provider === protocol && panes[side].profileId === profileId,
    );
    // Zuerst synchron leeren: Zwischen Sitzungstrennung und langsamem
    // `default_dirs` darf keine alte Remote-Zeile mehr bedienbar sein.
    for (const side of affectedSides) disablePane(side);
    return affectedSides.map(side => ({ side, generation: panes[side].loadSeq }));
  }

  async function moveDisabledPanesToDefaults(affected) {
    if (!affected.length) return;
    const defaults = await invoke("default_dirs");
    await Promise.all(affected.map(({ side, generation }) => {
      const pane = panes[side];
      // Ein Klick auf ein Profil während des Default-Abrufs ist eine neuere
      // Navigation. Der alte Rückfall darf diesen Endpunkt nicht auf lokal setzen.
      if (pane.loadSeq !== generation
        || pane.provider !== "local"
        || pane.profileId !== null
        || pane.path !== "") {
        return Promise.resolve();
      }
      return setPaneLocation(side, { provider: "local", path: defaults[side] });
    }));
  }

  async function save(protocol, profile, secretId = null, profileRevision = null) {
    const previous = profiles[protocol].find(candidate => candidate.id === profile.id);
    // Profil und optionaler neuer Schlüsselbundwert gehen in denselben Command.
    // Nur das Backend kann beide Speicherwirkungen gemeinsam zurücksetzen.
    let savedProfiles;
    try {
      savedProfiles = await invoke(`save_${protocol}_profile`, {
        profile,
        secretId,
        profileRevision,
      });
    } catch (error) {
      if (previous && error?.code === "outcome_unclear") {
        // Der atomare Rename kann bereits sichtbar sein, obwohl das abschließende
        // Verzeichnis-fsync fehlschlug. Die alte Dateiliste darf dann keinesfalls
        // unter dem möglicherweise neuen Endpunkt bedienbar bleiben.
        const affected = disableProfilePanes(protocol, profile.id);
        // Der Originalfehler wird sofort weitergereicht. Ein langsamer oder selbst
        // fehlschlagender Default-Abruf darf ihn weder verzögern noch ersetzen.
        void moveDisabledPanesToDefaults(affected).catch(() => {});
      }
      throw error;
    }
    const saved = savedProfiles.find(candidate => candidate.id === profile.id);
    // Das Backend gibt einem echten Secretwechsel absichtlich einen frischen
    // Schlüsselbundverweis. Deshalb muss der Vergleich den zurückgegebenen,
    // tatsächlich gespeicherten Datensatz verwenden — nicht nur das eingereichte
    // Profil mit der alten Referenz.
    const connectionChanged = previous && saved
      && JSON.stringify(stableConnection(previous.connection))
        !== JSON.stringify(stableConnection(saved.connection));
    invalidateProfileLoads(protocol);
    profiles[protocol] = savedProfiles;
    if (previous && connectionChanged) {
      // Das Backend verwirft beim Speichern die gecachte Sitzung. Eine Pane darf
      // danach niemals die alte Dateiliste unter der geänderten Verbindung zeigen:
      // Nutzer-/Root-/Pin-Änderungen könnten sonst auf einen anderen Endpunkt wirken.
      const affected = disableProfilePanes(protocol, profile.id);
      // Der Profilsave ist an dieser Stelle bereits vollständig erfolgreich. Der
      // anschließende Pane-Rückfall ist nur eine UI-Nachwirkung: Sein Fehler darf
      // den Editor nicht erneut in den Secret-Save-Fehlerpfad schicken.
      void moveDisabledPanesToDefaults(affected).catch(error => {
        showError("Verbindung gespeichert, Pane-Rückfall fehlgeschlagen", error);
      });
    } else if (previous) {
      updateProfileName(protocol, profile.id, profile.name);
    }
    render();
    return profiles[protocol];
  }

  function render() {
    const list = $("#connection-list");
    if (!list) return;
    const rows = PROTOCOLS.flatMap(
      protocol => profiles[protocol]
        .filter(profile => !pendingProfileReads.has(`${protocol}:${profile.id}`))
        .map(profile => profileRow(protocol, profile)),
    );
    list.innerHTML = rows.length
      ? rows.join("")
      : `<li class="connection-empty">Noch keine Verbindung. „+" wählt das Protokoll; Geheimnisse bleiben im System-Schlüsselbund.</li>`;
    // Fire-and-forget: Der Sitzungsstatus zieht asynchron nach und rendert nur
    // bei tatsächlicher Änderung erneut (Schlüsselvergleich verhindert Schleifen).
    refreshActive();
  }

  function profileRow(protocol, profile) {
    const active = [panes.left, panes.right].some(
      pane => pane.provider === protocol && pane.profileId === profile.id,
    );
    const connection = profile.connection;
    const ftp = protocol === "ftps" ? ftpDisplay(connection) : null;
    const protocolLabel = ftp?.label || CONFIG[protocol].label;
    const host = protocol === "webdav"
      ? connection.origin.replace(/^https?:\/\//, "")
      : protocol === "smb"
        ? `${connection.host}/${connection.share}`
        : connection.host;
    const title = protocol === "webdav"
      ? `${CONFIG[protocol].label} · ${connection.origin}${connection.requested_root}`
      : protocol === "smb"
        ? `${CONFIG[protocol].label} · ${connection.username}@${connection.host}/${connection.share}`
        : `${protocolLabel} · ${connection.username}@${connection.host}:${connection.port}`;
    const connected = activeConnections.has(`${protocol}:${profile.id}`);
    return `<li class="connection-item${active ? " active" : ""}${ftp?.warning ? " security-warning" : ""}" data-protocol="${protocol}" data-id="${escapeHtml(profile.id)}" title="${escapeHtml(title)}">
      <span class="side-icon ${protocol}" aria-hidden="true"></span>
      <span class="connection-name">${escapeHtml(profile.name)}<small class="connection-host">${escapeHtml(protocolLabel)}${ftp ? ` · ${escapeHtml(ftp.detail)}` : ""} · ${escapeHtml(host)}</small></span>
      ${connected ? `<button class="conn-eject icon-btn" data-eject="${escapeHtml(profile.id)}" title="Verbindung trennen">⏏</button>` : ""}
      <button class="conn-edit icon-btn" data-edit="${escapeHtml(profile.id)}" title="Profil bearbeiten" aria-label="Profil bearbeiten">✎</button>
      <button class="fav-del icon-btn" data-remove="${escapeHtml(profile.id)}" title="Profil entfernen (Secret bleibt im Schlüsselbund)">×</button>
    </li>`;
  }

  function wire({ openPicker, editProfile, openQuarantine, openQueue }) {
    openProfileEditor = editProfile;
    $("#conn-add").addEventListener("click", openPicker);
    $("#connection-list").addEventListener("click", handleClick);
    $("#connection-list").addEventListener("contextmenu", handleContextMenu);
    $("#btn-quarantine").addEventListener("click", openQuarantine);
    $("#btn-queue").addEventListener("click", openQueue);
  }

  function findProfile(protocol, id) {
    if (pendingProfileReads.has(`${protocol}:${id}`)) return undefined;
    return profiles[protocol].find(candidate => candidate.id === id);
  }

  function editProfile(protocol, profile) {
    if (profile && openProfileEditor) openProfileEditor(protocol, profile);
  }

  function openProfile(protocol, profile, side) {
    if (!profile) return;
    const ftp = protocol === "ftps" ? ftpDisplay(profile.connection) : null;
    const securityStatus = ftp?.label === "FTP"
      ? "plaintext"
      : profile.connection.certificate_policy === "unverified_certificate"
        ? "tls_identity_unverified"
        : null;
    setPaneLocation(side, {
      provider: protocol,
      profileId: profile.id,
      profileRevision: profile.revision,
      profileName: profile.name,
      securityStatus,
      path: "/",
    });
  }

  async function disconnectProfile(protocol, id) {
    try {
      // Trennen ist gefahrlos reversibel: Der nächste Klick auf das Profil baut
      // die Sitzung lazy wieder auf. Panes, die das Profil noch zeigen, kehren
      // zu ihrem lokalen Standardordner zurück.
      const profile = findProfile(protocol, id);
      await invoke("disconnect_remote_profile", {
        protocol,
        profileId: id,
        profileRevision: profile?.revision,
      });
      const affected = disableProfilePanes(protocol, id);
      await moveDisabledPanesToDefaults(affected);
      activeKey = null; // Statusabgleich erzwingen, auch ohne Set-Änderung von außen.
      await refreshActive();
    } catch (error) {
      showError(`${CONFIG[protocol].label}-Verbindung`, error);
    }
  }

  function profileLocation(protocol, id) {
    if (pendingProfileReads.has(`${protocol}:${id}`)) {
      return { unavailable: true };
    }
    const profile = findProfile(protocol, id);
    if (!profile) return null;
    const ftp = protocol === "ftps" ? ftpDisplay(profile.connection) : null;
    return {
      provider: protocol,
      profileId: profile.id,
      profileRevision: profile.revision,
      profileName: profile.name,
      securityStatus: ftp?.label === "FTP"
        ? "plaintext"
        : profile.connection.certificate_policy === "unverified_certificate"
          ? "tls_identity_unverified"
          : null,
    };
  }

  async function handleConnectionUpdate(update) {
    const matching = [panes.left, panes.right].filter(
      pane => pane.provider === update?.protocol && pane.profileId === update?.profileId,
    );
    const invalidates = update?.change !== "connected"
      || matching.some(pane => pane.profileRevision !== update?.revision);
    const profileKey = `${update?.protocol}:${update?.profileId}`;
    const readToken = invalidates ? ++profileReadSequence : null;
    if (readToken !== null) pendingProfileReads.set(profileKey, readToken);
    let affected = [];
    try {
      if (readToken !== null) {
        // Die alte Profilzeile wird ohne Await unbedienbar, bevor das erste
        // Protokoll-Listing oder ein Defaultordner gelesen wird.
        render();
      }
      affected = invalidates
        ? disableProfilePanes(update.protocol, update.profileId)
        : [];
      await loadAll();
      await moveDisabledPanesToDefaults(affected);
      activeKey = null;
      await refreshActive();
    } catch (error) {
      // Die Pane ist bereits synchron wirkungslos. Ein fehlgeschlagener lokaler
      // Rückfall darf weder die alte Remote-Liste wiederbeleben noch als
      // unbehandelte Event-Promise verschwinden.
      showError("Verbindungsstatus aktualisieren", error);
    } finally {
      try {
        if (readToken !== null && pendingProfileReads.get(profileKey) === readToken) {
          // Ein neuerer Abruf desselben Protokolls — etwa durch ein `connected`-
          // Ereignis oder ein Ereignis für ein anderes Profil — verwirft den eigenen
          // Abruf. Dann steht in `profiles` noch die alte Liste. Würde die Sperre
          // schon jetzt fallen, wäre die überholte Profilzeile samt Favoriten wieder
          // bedienbar und arbeitete mit der alten Revision weiter, bis das Backend
          // sie als `plan_drift` abweist. Also erst den jüngsten Abruf abwarten.
          await awaitLatestProfileLoad(update?.protocol);
        }
      } catch (error) {
        showError("Profilstatus abschließen", error);
      } finally {
        if (readToken !== null && pendingProfileReads.get(profileKey) === readToken) {
          pendingProfileReads.delete(profileKey);
          render();
        }
      }
    }
  }

  async function removeProfile(protocol, profile) {
    if (!profile) return;
    const confirmed = await confirmModal(
      `${CONFIG[protocol].label}-Profil entfernen`,
      `Profil <b>${escapeHtml(profile.name)}</b> entfernen? ${CONFIG[protocol].secretNote}`,
    );
    if (!confirmed) return;
    try {
      const removedProfiles = await invoke(`remove_${protocol}_profile`, {
        profileId: profile.id,
        profileRevision: profile.revision,
      });
      invalidateProfileLoads(protocol);
      profiles[protocol] = removedProfiles;
      const affected = disableProfilePanes(protocol, profile.id);
      await moveDisabledPanesToDefaults(affected);
      render();
    } catch (error) {
      if (error?.code === "outcome_unclear") {
        const affected = disableProfilePanes(protocol, profile.id);
        void moveDisabledPanesToDefaults(affected).catch(() => {});
      }
      showError(`${CONFIG[protocol].label}-Profil`, error);
    }
  }

  function handleContextMenu(event) {
    const item = event.target.closest(".connection-item");
    if (!item) return;
    event.preventDefault();
    const protocol = item.dataset.protocol;
    const profile = findProfile(protocol, item.dataset.id);
    if (!profile) return;
    const side = activeSide();
    const otherSide = side === "left" ? "right" : "left";
    const connected = activeConnections.has(`${protocol}:${profile.id}`);
    const items = [
      {
        label: "In aktiver Pane öffnen",
        action: () => openProfile(protocol, profile, side),
      },
      {
        label: "Auf der anderen Seite öffnen",
        action: () => openProfile(protocol, profile, otherSide),
      },
      { separator: true },
      { label: "Bearbeiten", action: () => editProfile(protocol, profile) },
    ];
    if (connected) {
      items.push({
        label: "Verbindung trennen",
        action: () => disconnectProfile(protocol, profile.id),
      });
    }
    items.push({
      label: "Profil entfernen …",
      action: () => removeProfile(protocol, profile),
    });
    openMenu({
      x: event.clientX,
      y: event.clientY,
      ariaLabel: "Verbindung",
      items,
    });
  }

  async function handleClick(event) {
    const item = event.target.closest(".connection-item");
    if (!item) return;
    const protocol = item.dataset.protocol;
    const ejectButton = event.target.closest("[data-eject]");
    if (ejectButton) {
      event.stopPropagation();
      await disconnectProfile(protocol, ejectButton.dataset.eject);
      return;
    }
    const editButton = event.target.closest("[data-edit]");
    if (editButton) {
      event.stopPropagation();
      editProfile(protocol, findProfile(protocol, editButton.dataset.edit));
      return;
    }
    const removeButton = event.target.closest("[data-remove]");
    if (removeButton) {
      event.stopPropagation();
      await removeProfile(protocol, findProfile(protocol, removeButton.dataset.remove));
      return;
    }
    openProfile(protocol, findProfile(protocol, item.dataset.id), activeSide());
  }

  return { handleConnectionUpdate, loadAll, profileLocation, render, save, wire };
}
