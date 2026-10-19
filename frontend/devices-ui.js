import { invoke } from "./tauri-api.js";
import { $, escapeHtml, fmtEntrySize } from "./ui-utils.js";

const KIND_LABELS = {
  internal: "Intern",
  external: "Extern",
  network: "Netzwerk",
  volume: "Volume",
};

function pathWithin(path, root) {
  if (!path || !root) return false;
  if (path === root) return true;
  const prefix = root.endsWith("/") ? root : `${root}/`;
  return path.startsWith(prefix);
}

/** Seitenleistenansicht der vom Core frisch erkannten Mounts. Die opaque ID ist die
 * einzige Auswurf-Eingabe; Pfad und Anzeigename dienen ausschließlich Navigation
 * beziehungsweise Erklärung und können keinen Systembefehl frei zusammensetzen. */
export function createDevicesUi({ activeSide, disablePane, panes, setPaneLocation, showDock, showError }) {
  let mounts = [];
  let loadGeneration = 0;

  function renderMountList(selector, visibleMounts, emptyText) {
    const list = $(selector);
    if (!list) return;
    if (!visibleMounts.length) {
      list.innerHTML = `<li class="device-empty">${emptyText}</li>`;
      return;
    }
    list.innerHTML = visibleMounts.map((mount) => {
      const pathTitle = mount.path ? ` · ${mount.path}` : " · Pfad nicht darstellbar";
      const capacity = mount.space_error
        ? "Speicher nicht abrufbar"
        : mount.total_bytes > 0
          ? `${fmtEntrySize(mount.available_bytes)} frei von ${fmtEntrySize(mount.total_bytes)}`
          : "Speichergröße unbekannt";
      const spaceErrorTitle = mount.space_error ? ` · ${mount.space_error.message}` : "";
      const disabled = mount.path ? "" : " path-unavailable";
      return `<li class="device-item${disabled}" data-mount-id="${escapeHtml(mount.id)}"
        title="${escapeHtml(`${KIND_LABELS[mount.kind] || "Volume"} · ${mount.filesystem}${pathTitle}${spaceErrorTitle}`)}">
        <span class="side-icon device-${escapeHtml(mount.kind)}" aria-hidden="true"></span>
        <span class="device-name">${escapeHtml(mount.name)}
          <small class="device-space">${escapeHtml(capacity)}</small>
        </span>
        ${mount.ejectable
          ? `<button class="device-eject icon-btn" type="button" data-eject-mount="${escapeHtml(mount.id)}" aria-label="${escapeHtml(mount.name)} auswerfen" title="Auswerfen">⏏</button>`
          : ""}
      </li>`;
    }).join("");
  }

  function render() {
    renderMountList(
      "#device-list",
      mounts.filter(mount => mount.kind !== "network"),
      "Keine sichtbaren Volumes.",
    );
    renderMountList(
      "#share-list",
      mounts.filter(mount => mount.kind === "network"),
      "Keine eingehängten Freigaben.",
    );
  }

  async function load({ silent = false } = {}) {
    const request = ++loadGeneration;
    try {
      const current = await invoke("list_system_mounts");
      if (request !== loadGeneration) return;
      mounts = current;
      render();
    } catch (error) {
      if (request !== loadGeneration) return;
      mounts = [];
      render();
      if (!silent) showError("Geräte und Freigaben", error);
    }
  }

  async function eject(mount) {
    try {
      mounts = await invoke("eject_system_mount", { id: mount.id });
    } catch (error) {
      showError(`${mount.name} auswerfen`, error);
      await load({ silent: true });
      return;
    }
    // Ab hier ist das Volume nachweislich ausgehängt. Ein Fehler des Pane-Rückfalls
    // darf deshalb nicht mehr als gescheiterter Auswurf erscheinen — sonst sucht der
    // Nutzer nach einem Volume, das längst weg ist. Dieselbe Trennung macht
    // connections-ui beim Profilsave.
    try {
      // Eine lokale Pane unter dem verschwundenen Mount darf keine alte Liste unter
      // einem nun gewöhnlichen Verzeichnisnamen weiterverwenden. Nach bestätigtem
      // Auswerfen kehrt nur die betroffene Pane zu ihrem lokalen Standard zurück.
      // Erst synchron entwerten: Nach dem Unmount bezeichnet derselbe Pfad
      // möglicherweise einen anderen Bestand. Auch ein fehlgeschlagener oder
      // langsamer Default-Abruf darf die alte Auswahl nicht bedienbar halten.
      const affected = ["left", "right"]
        .filter(side => panes[side].provider === "local" && pathWithin(panes[side].path, mount.path))
        .map(side => {
          disablePane(side);
          return { side, generation: panes[side].loadSeq };
        });
      // Ein schon vorher gestarteter Read oder ein später beantworteter anderer
      // Auswurf kann einen älteren Gesamtbestand liefern. Frisch lesen und dabei
      // alle früheren Anzeigeantworten entwerten; die Panes sind bereits gesperrt.
      await load({ silent: true });
      const defaults = affected.length ? await invoke("default_dirs") : null;
      await Promise.all(affected.map(({ side, generation }) => {
        const pane = panes[side];
        // Eine inzwischen gewählte Navigation besitzt den neueren Token und
        // darf vom verspäteten Rückfall nicht wieder überschrieben werden.
        if (pane.loadSeq !== generation || pane.provider !== "local"
          || pane.profileId !== null || pane.path !== "") return Promise.resolve();
        return setPaneLocation(side, { provider: "local", path: defaults[side] });
      }));
      render();
      showDock(
        `<span class="pill ok">Ausgeworfen</span>${escapeHtml(mount.name)} ist nicht mehr eingehängt.`,
        { closable: true },
      );
    } catch (error) {
      render();
      showError(
        `${mount.name} ausgeworfen, Pane-Rückfall fehlgeschlagen`,
        error,
      );
    }
  }

  function wireMountList(selector) {
    $(selector).addEventListener("click", async (event) => {
      const item = event.target.closest(".device-item");
      if (!item) return;
      const mount = mounts.find(candidate => candidate.id === item.dataset.mountId);
      if (!mount) return;
      const ejectButton = event.target.closest("[data-eject-mount]");
      if (ejectButton) {
        event.stopPropagation();
        ejectButton.disabled = true;
        await eject(mount);
        return;
      }
      if (mount.path) {
        await setPaneLocation(activeSide(), { provider: "local", path: mount.path });
      }
    });
  }

  function wire() {
    wireMountList("#device-list");
    wireMountList("#share-list");
  }

  return { load, render, wire };
}
