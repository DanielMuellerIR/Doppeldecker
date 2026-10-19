import { hideModal, showOwnedModal } from "./modal.js";
import { invoke, listen } from "./tauri-api.js";
import { $, escapeHtml } from "./ui-utils.js";

const ICONS = [
  ["blue", "Blauer Himmel"], ["sunset", "Sonnenuntergang"],
  ["aurora", "Polarlichter"], ["turquoise", "Türkiser Himmel"],
  ["rose", "Rosa-lila Himmel"], ["storm", "Gewitter und Goldlicht"],
];
const TOOLS = [["xcode", "Xcode FileMerge"], ["fastra", "Fastra"], ["bbedit", "BBEdit"]];
const RADIO_PREFERENCES = { icon: "icon", "diff-tool": "diffTool" };

export function createSettingsUi({
  applyByteUnits, applyLocalFolderSizes, applyNetworkDiscovery, applyTheme,
  currentWindow, legacyPreferences, showError,
}) {
  let snapshot = null;
  let statuses = [];
  let modalToken = null;
  let opening = false;
  let rememberTimer = null;
  // Bis der Start die gespeicherte aktive Pane übernommen hat, darf kein
  // Zwischenstand gespeichert werden; siehe `startRemembering`.
  let rememberEnabled = false;

  function accept(next) {
    // Die Revision wächst unter der Backend-Mutex streng monoton. Nur auf
    // Ungleichheit zu prüfen genügt deshalb nicht: Trifft die eigene, ältere
    // Antwort nach einem Ereignis aus einem anderen Fenster ein, würde dieses
    // Fenster auf den überholten Stand zurückspringen.
    if (!next || (snapshot && next.revision <= snapshot.revision)) return;
    const previous = snapshot?.preferences;
    snapshot = next;
    // Bei einem Lesefehler trägt der Snapshot die Vorgabewerte, nicht die Wahl des
    // Nutzers — das Backend kann sie ja gerade nicht lesen. Sie anzuwenden hieße,
    // sie über `applyTheme` und die drei anderen Wege in genau die lokalen
    // Schlüssel zu schreiben, die dann die letzte Kopie dieser Wahl sind. Der
    // Dialog zeigt sie weiterhin an; ändern lässt sich dort ohnehin nichts,
    // solange das Backend jeden Schreibversuch mit demselben Fehler ablehnt.
    if (!snapshot.loadError) apply(snapshot.preferences, previous);
    // Kein Neuaufbau des Dialogs: `innerHTML` würde das gerade bediente Select oder
    // Radio samt Tastaturfokus verwerfen. Nur die Werte werden nachgezogen — auch
    // bei einer Änderung aus einem anderen Fenster.
    if (modalToken) syncBody();
  }

  function apply(preferences, previous = null) {
    if (!preferences) return;
    // Theme-Boot und frühe Spaltenmessung lesen weiterhin die bisherigen lokalen
    // Schlüssel. Sie spiegeln den Backendwert, damit der nächste Start nicht kurz
    // mit einem veralteten Theme oder Einheitenkürzel zeichnet.
    if (!previous || preferences.theme !== previous.theme) {
      applyTheme(preferences.theme, true);
    }
    if (!previous || preferences.byteUnits !== previous.byteUnits) {
      applyByteUnits(preferences.byteUnits, true);
    }
    if (!previous || preferences.localFolderSizes !== previous.localFolderSizes) {
      applyLocalFolderSizes(preferences.localFolderSizes, true);
    }
    if (!previous || preferences.networkDiscovery !== previous.networkDiscovery) {
      applyNetworkDiscovery(preferences.networkDiscovery, true);
    }
  }

  async function update(patch) {
    try {
      accept(await invoke("update_desktop_preferences", { patch }));
    } catch (error) {
      // Menübefehle stellen Theme und Einheit für direkte Rückmeldung zunächst
      // optimistisch dar. Ein Schreibfehler setzt sie auf den bestätigten Stand.
      if (snapshot && !snapshot.loadError) {
        // Nur Felder des fehlgeschlagenen Patches zurückspiegeln. Ohne diesen
        // Vergleich würden unbeteiligte bestätigte Werte erneut in localStorage
        // geschrieben; bei einem Lesefehler sind selbst die Snapshotwerte bloße
        // Vorgaben und dürfen die letzte lokale Nutzerwahl gar nicht berühren.
        apply(snapshot.preferences, { ...snapshot.preferences, ...patch });
      }
      // Auch die Bedienelemente zurückziehen: Sonst zeigt der offene Dialog
      // weiterhin den abgelehnten Wert, während Anzeige und gespeicherter Stand
      // längst wieder der alte sind.
      if (modalToken) syncBody();
      showError("Einstellungen", error);
    }
  }

  function option(id, label, checked, disabled, detail) {
    return `<label class="settings-tool${disabled ? " disabled" : ""}">
      <input type="radio" name="diff-tool" value="${id}" ${checked ? "checked" : ""} ${disabled ? "disabled" : ""}>
      <span><strong>${escapeHtml(label)}</strong><small>${escapeHtml(detail)}</small></span></label>`;
  }

  function radioChecked(preferences, name, value) {
    return preferences[RADIO_PREFERENCES[name]] === value;
  }

  function renderBody() {
    const body = $("#modal-body");
    if (!body || !snapshot) return;
    const p = snapshot.preferences;
    const status = Object.fromEntries(statuses.map(item => [item.id, item]));
    body.innerHTML = `<div class="settings-sections">
      <fieldset><legend>Erscheinungsbild</legend>
        <label>Darstellung <select data-setting="theme">
          <option value="auto" ${p.theme === "auto" ? "selected" : ""}>Automatisch</option>
          <option value="light" ${p.theme === "light" ? "selected" : ""}>Hell</option>
          <option value="dark" ${p.theme === "dark" ? "selected" : ""}>Dunkel</option>
        </select></label>
        <div class="icon-choices" role="radiogroup" aria-label="App-Icon">${ICONS.map(([id, label]) => `
          <label class="icon-choice"><input type="radio" name="icon" value="${id}" ${radioChecked(p, "icon", id) ? "checked" : ""}>
            <img src="icons/preview/${id}.png" alt=""><span>${escapeHtml(label)}</span></label>`).join("")}</div>
      </fieldset>
      <fieldset><legend>Dateien und Ordner</legend>
        <label>Größen <select data-setting="byteUnits">
          <option value="decimal" ${p.byteUnits === "decimal" ? "selected" : ""}>Dezimal (KB, MB, GB)</option>
          <option value="binary" ${p.byteUnits === "binary" ? "selected" : ""}>Binär (KiB, MiB, GiB)</option>
        </select></label>
        <label><input type="checkbox" data-setting="localFolderSizes" ${p.localFolderSizes ? "checked" : ""}> Lokale Ordnergrößen automatisch berechnen</label>
        <label><input type="checkbox" data-setting="networkDiscovery" ${p.networkDiscovery ? "checked" : ""}> Unterstützte Freigaben im LAN suchen</label>
      </fieldset>
      <fieldset><legend>Sitzung</legend>
        <label><input type="checkbox" data-setting="restoreSession" ${p.restoreSession ? "checked" : ""}> Fenster und Pane-Ziele nach einem Neustart wiederherstellen</label>
        <small>Gespeicherte Verbindungsprofile werden dabei neu verbunden. Laufende Operationen folgen weiterhin der Queue-Recovery.</small>
      </fieldset>
      <fieldset><legend>Dateivergleich</legend>${TOOLS.map(([id, label]) => {
        const item = status[id] || { available: false, detail: "Status wird ermittelt …" };
        return option(id, label, radioChecked(p, "diff-tool", id), !item.available, item.detail);
      }).join("")}</fieldset>
    </div>
    <nav class="settings-links" aria-label="Projektinformationen">
      <a href="https://github.com/DanielMuellerIR/Doppeldecker/issues" data-project-page="support">Support</a>
      <a href="https://dm0.de/impressum.html" data-project-page="imprint">Impressum</a>
      <a href="https://dm0.de/datenschutz.html#doppeldecker" data-project-page="privacy">Datenschutz</a>
    </nav>`;
    body.querySelector(".settings-links").addEventListener("click", event => {
      const link = event.target.closest("[data-project-page]");
      if (!link) return;
      event.preventDefault();
      void invoke("open_project_page", { page: link.dataset.projectPage })
        .catch(error => showError("Projektinformationen", error));
    });
    body.onchange = event => {
      const target = event.target;
      if (RADIO_PREFERENCES[target.name]) {
        void update({ [RADIO_PREFERENCES[target.name]]: target.value });
      }
      else if (target.dataset.setting) {
        const value = target.type === "checkbox" ? target.checked : target.value;
        void update({ [target.dataset.setting]: value });
      }
    };
  }

  /** Zieht die Steuerelemente eines offenen Dialogs auf den Snapshot nach,
   * ohne das DOM neu zu bauen. */
  function syncBody() {
    const body = $("#modal-body");
    if (!body || !snapshot) return;
    const p = snapshot.preferences;
    for (const control of body.querySelectorAll("[data-setting]")) {
      const value = p[control.dataset.setting];
      if (control.type === "checkbox") control.checked = Boolean(value);
      else if (control.value !== String(value)) control.value = value;
    }
    for (const [name, preference] of Object.entries(RADIO_PREFERENCES)) {
      for (const radio of body.querySelectorAll(`input[name="${name}"]`)) {
        radio.checked = radio.value === p[preference];
      }
    }
  }

  function close() {
    if (modalToken && hideModal(modalToken)) modalToken = null;
  }

  async function open() {
    // Der Menüeintrag „Einstellungen …" ist auch bei offenem Dialog erreichbar.
    // Ein zweiter Durchlauf würde `modalToken` durch das Token eines nur
    // eingereihten Leases ersetzen; danach schlösse weder „Fertig" noch Escape den
    // sichtbaren Dialog, und das Fenster bliebe bis zum Schließen unbedienbar.
    // Auch während der asynchronen Abfragen darf nur ein Öffnen laufen.
    if (modalToken || opening) return;
    opening = true;
    try {
      const [current, found] = await Promise.all([
        invoke("desktop_preferences"), invoke("diff_tool_status"),
      ]);
      accept(current);
      statuses = found;
    } catch (error) {
      showError("Einstellungen", error);
      return;
    } finally {
      opening = false;
    }
    modalToken = showOwnedModal("settings", () => {
      $("#modal .modal").className = "modal settings-modal";
      $("#modal-title").textContent = "Einstellungen";
      renderBody();
      const buttons = $("#modal .modal-buttons");
      buttons.replaceChildren();
      const done = document.createElement("button");
      done.type = "button"; done.className = "btn primary"; done.textContent = "Fertig";
      done.addEventListener("click", close); buttons.appendChild(done);
      return { focusTarget: $("#modal-body select"), onEscape: close };
    });
  }

  function rememberSoon() {
    if (!rememberEnabled) return;
    clearTimeout(rememberTimer);
    rememberTimer = setTimeout(() => {
      const current = currentWindow();
      if (current.panes.left.path && current.panes.right.path) {
        void invoke("remember_window", current).catch(error => showError("Sitzungsstand", error));
      }
    }, 250);
  }

  /** Erst nach dem vollständigen Start aufrufen. Während der Start-Listings hat
   * `activeSide` noch den Vorgabewert „links"; ein früher `remember_window`
   * überschriebe die gespeicherte aktive Pane, bevor `restored_active_side` sie
   * gelesen hat. Der erste Stand wird hier bewusst sofort erfasst. */
  function startRemembering() {
    rememberEnabled = true;
    rememberSoon();
  }

  async function init() {
    const [initial] = await Promise.all([
      invoke("desktop_preferences"),
      listen("desktop-preferences-changed", event => {
        accept(event.payload);
      }),
    ]);
    // Die bisherigen globalen Werte lagen pro Fenster in localStorage. Beim ersten
    // Start mit der Backend-Datei übernehmen wir sie einmal, statt eine vorhandene
    // Wahl auf die neuen Vorgaben zurückzusetzen. Scheitert das Schreiben (etwa
    // ein nicht beschreibbarer Konfigurationsordner), bleibt es beim gelesenen
    // Stand: Die Migration darf den Pane-Start nicht blockieren.
    let migrated = initial;
    if (initial.needsMigration && !initial.loadError) {
      const legacy = legacyPreferences();
      try {
        migrated = await invoke("update_desktop_preferences", { patch: legacy, onlyIfUninitialized: true });
      } catch (error) {
        // Noch nicht gespeicherte Vorgaben dürfen die letzte lokale Nutzerwahl
        // nicht verdrängen. Ein späterer Start kann genau diese Werte erneut migrieren.
        migrated = { ...initial, preferences: { ...initial.preferences, ...legacy } };
        showError("Einstellungen", error);
      }
    }
    accept(migrated);
    if (snapshot.loadError) showError("Einstellungen", snapshot.loadError);
    const current = window.__TAURI__?.window?.getCurrentWindow?.();
    if (current) {
      await Promise.all([current.onMoved(rememberSoon), current.onResized(rememberSoon)]);
    }
  }

  return { init, open, rememberSoon, set: update, startRemembering };
}
