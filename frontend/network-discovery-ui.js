import { invoke } from "./tauri-api.js";
import { $, escapeHtml, readStoredValue, writeStoredValue } from "./ui-utils.js";

const STORAGE_KEY = "doppeldecker-network-discovery-enabled";
const MIN_REFRESH_INTERVAL_MS = 5_000;
const PROTOCOL_LABELS = { smb: "SMB", sftp: "SFTP", webdav: "WebDAV" };

function initialEnabled() {
  // Ohne gemerkte Wahl ist die Suche eingeschaltet; nur ein ausdrückliches
  // "false" schaltet sie ab.
  return readStoredValue(STORAGE_KEY) !== "false";
}

/** Sichtbare LAN-Suche für DNS-SD-Funde. Ein Ergebnis öffnet nur den bestehenden
 * leeren Editor mit Vorschlagswerten; Verbinden, Secretzugriff und Identitäts-Pins
 * bleiben außerhalb dieser Komponente. */
export function createNetworkDiscoveryUi({ onEnabledChange, openService, showError }) {
  let enabled = initialEnabled();
  let services = [];
  let running = false;
  let generation = 0;
  let lastStarted = 0;

  function render() {
    const list = $("#network-service-list");
    const toggle = $("#network-discovery-enabled");
    const refreshButton = $("#network-discovery-refresh");
    toggle.checked = enabled;
    refreshButton.disabled = !enabled || running;
    if (!enabled) {
      list.innerHTML = '<li class="network-service-empty">LAN-Suche ausgeschaltet.</li>';
      return;
    }
    if (running) {
      list.innerHTML = '<li class="network-service-empty">LAN wird durchsucht …</li>';
      return;
    }
    if (!services.length) {
      list.innerHTML = '<li class="network-service-empty">Keine unterstützte Freigabe gefunden.</li>';
      return;
    }
    list.innerHTML = services.map((service, index) => {
      const protocol = PROTOCOL_LABELS[service.protocol] || service.protocol.toUpperCase();
      const action = `${service.name} · ${protocol} ${service.host}:${service.port} im Verbindungseditor öffnen`;
      return `<li><button class="network-service" type="button" data-service-index="${index}"
        aria-label="${escapeHtml(action)}"
        title="${escapeHtml(`${protocol} · unbestätigter LAN-Fund; öffnet nur den Verbindungseditor`)}">
          <span class="side-icon ${escapeHtml(service.protocol)}" aria-hidden="true"></span>
          <span class="network-service-name">${escapeHtml(service.name)}
            <small>${escapeHtml(protocol)} · ${escapeHtml(service.host)}:${service.port}</small>
          </span>
        </button></li>`;
    }).join("");
  }

  async function refresh({ silent = false, force = false } = {}) {
    if (!enabled || running) {
      render();
      return;
    }
    const now = Date.now();
    if (!force && now - lastStarted < MIN_REFRESH_INTERVAL_MS) return;
    lastStarted = now;
    const request = ++generation;
    running = true;
    render();
    try {
      const found = await invoke("discover_network_services");
      if (request === generation && enabled) services = Array.isArray(found) ? found : [];
    } catch (error) {
      if (request === generation && enabled && !silent) showError("LAN-Suche", error);
    } finally {
      if (request === generation) {
        running = false;
        render();
      }
    }
  }

  function setEnabled(value, persist = true) {
    const changed = enabled !== Boolean(value);
    enabled = Boolean(value);
    generation += 1;
    running = false;
    if (!enabled) services = [];
    // Ohne lokale Persistenz gilt die Wahl nur für dieses Fenster.
    if (persist) writeStoredValue(STORAGE_KEY, String(enabled));
    render();
    if (enabled && changed) void refresh({ force: true });
  }

  function wire() {
    $("#network-discovery-enabled").addEventListener("change", event => {
      if (onEnabledChange) onEnabledChange(event.target.checked);
      else setEnabled(event.target.checked);
    });
    $("#network-discovery-refresh").addEventListener("click", () => {
      void refresh({ force: true });
    });
    $("#network-service-list").addEventListener("click", event => {
      const row = event.target.closest("[data-service-index]");
      if (!row) return;
      const service = services[Number(row.dataset.serviceIndex)];
      if (service) openService(service);
    });
    render();
  }

  return { enabled: () => enabled, refresh, setEnabled, wire };
}
