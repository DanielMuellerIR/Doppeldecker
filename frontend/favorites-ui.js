import { invoke } from "./tauri-api.js";
import { openMenu } from "./context-menu.js";
import { $, escapeHtml } from "./ui-utils.js";

/** MIME-Typ für Ordner, die aus einer Pane in die Seitenleiste gezogen werden. */
export const FOLDER_DRAG_TYPE = "application/x-doppeldecker-folder";
/** MIME-Typ für das Umsortieren innerhalb der Favoritenliste. */
const FAVORITE_DRAG_TYPE = "application/x-doppeldecker-favorite";

function baseName(path) {
  const parts = path.split("/").filter(Boolean);
  return parts.length ? parts[parts.length - 1] : path;
}

/** Leitet die Icon-Art rein aus dem Pfad ab (nur Darstellung, keine Logik):
 * bekannte Systemordner erhalten ein vertrautes, plattformgerechtes Symbol. */
export function favoriteIconKind(path) {
  const clean = path.replace(/\/+$/, "");
  const segments = clean.split("/").filter(Boolean);
  // Direkt unter /Users bzw. /home liegt der Benutzerordner selbst.
  if (segments.length === 2 && (segments[0] === "Users" || segments[0] === "home")) {
    return "home";
  }
  switch (segments[segments.length - 1]) {
    case "Desktop": return "desktop";
    case "Documents": case "Dokumente": return "documents";
    case "Downloads": return "downloads";
    case "Pictures": case "Bilder": return "pictures";
    case "Music": case "Musik": return "music";
    case "Movies": case "Videos": case "Filme": return "movies";
    default: return "folder";
  }
}

function endpointOf(favorite) {
  return {
    provider: favorite.provider || "local",
    profileId: favorite.profile_id || favorite.profileId || null,
    path: favorite.path,
  };
}

function endpointKey(favorite) {
  return JSON.stringify(endpointOf(favorite));
}

/** Verwaltet lokale und remote Schnellzugriffe. Remote-Favoriten enthalten nur die
 * stabile Profil-ID und den kanonischen Providerpfad, niemals Zugangsdaten. */
export function createFavoritesUi({
  activeSide,
  panes,
  resolveProfile,
  setPaneLocation,
  showDock,
  showError,
}) {
  let favorites = [];
  let loadGeneration = 0;

  function render() {
    const list = $("#fav-list");
    if (!favorites.length) {
      list.innerHTML = `<li class="fav-empty">Noch keine Favoriten. Ordner aus einer Pane hierher ziehen oder mit „+" ablegen.</li>`;
      return;
    }
    list.innerHTML = favorites.map((favorite, index) => {
      const endpoint = endpointOf(favorite);
      const icon = endpoint.provider === "local" ? favoriteIconKind(favorite.path) : endpoint.provider;
      const title = endpoint.provider === "local"
        ? favorite.path
        : `${endpoint.provider.toUpperCase()} · ${favorite.path}`;
      return `
      <li class="fav-item" data-i="${index}" draggable="true" title="${escapeHtml(title)}">
        <span class="side-icon ${icon}" aria-hidden="true"></span>
        <span class="fav-name">${escapeHtml(favorite.name)}</span>
        <button class="fav-del icon-btn" data-del="${index}" title="Aus Favoriten entfernen">×</button>
      </li>`;
    }).join("");
  }

  async function load({ afterChange = false } = {}) {
    const request = ++loadGeneration;
    try {
      const current = await invoke("list_favorites");
      if (request !== loadGeneration) return;
      favorites = current;
      render();
    } catch (error) {
      if (request !== loadGeneration) return;
      render();
      showError(afterChange ? "Favoriten geändert, Ansicht nicht aktualisiert" : "Favoriten laden", error);
    }
  }

  async function add(path, name, endpoint = { provider: "local" }) {
    if (!path) return;
    const displayName = (name || baseName(path) || path).trim();
    try {
      if ((endpoint.provider || "local") === "local") {
        await invoke("add_favorite", { name: displayName, path });
      } else {
        await invoke("add_remote_favorite", {
          name: displayName,
          provider: endpoint.provider,
          profileId: endpoint.profileId,
          path,
        });
      }
      // Mutationsantworten können nach einer späteren Änderung eintreffen.
      // Ihr alter Gesamtbestand darf diese nicht zurücknehmen; maßgeblich ist
      // ein frischer Read nach dem Abschluss, ebenso bei Entfernen und Umsortieren.
      await load({ afterChange: true });
    } catch (error) {
      showError("Fehler", error);
    }
  }

  async function remove(favorite) {
    const endpoint = endpointOf(favorite);
    try {
      await invoke("remove_favorite", endpoint);
      await load({ afterChange: true });
    } catch (error) {
      showError("Fehler", error);
    }
  }

  async function moveTo(favorite, index) {
    const endpoint = endpointOf(favorite);
    try {
      await invoke("move_favorite", { ...endpoint, index });
      await load({ afterChange: true });
    } catch (error) {
      showError("Fehler", error);
    }
  }

  /** Übernimmt die Ordner der Finder-Seitenleiste als Favoriten (nur macOS,
   * per Menü ausgelöst). Rein additiv: Vorhandenes bleibt, Duplikate werden
   * backend-seitig übersprungen; die Bilanz wird sichtbar gemeldet. */
  async function importFromFinder() {
    try {
      const summary = await invoke("import_finder_favorites");
      const skipped = [];
      if (summary.skipped_existing) skipped.push(`${summary.skipped_existing} bereits vorhanden`);
      if (summary.skipped_missing) skipped.push(`${summary.skipped_missing} kein (vorhandener) Ordner`);
      if (summary.skipped_hidden) skipped.push(`${summary.skipped_hidden} im Finder ausgeblendet`);
      if (summary.skipped_unreadable) skipped.push(`${summary.skipped_unreadable} nicht lesbar`);
      const detail = skipped.length ? ` · übersprungen: ${skipped.join(", ")}` : "";
      showDock(`<span class="pill ok">Finder-Favoriten</span>${summary.added} übernommen${detail}`, { closable: true });
      await load({ afterChange: true });
    } catch (error) {
      showError("Finder-Favoriten", error);
    }
  }

  /** Zielindex eines Drops: vor dem getroffenen Eintrag, hinter ihm bei unterer
   * Hälfte, ans Ende bei Drop auf die freie Fläche der Liste. */
  function dropIndex(event) {
    const item = event.target.closest(".fav-item");
    if (!item) return favorites.length;
    const rect = item.getBoundingClientRect();
    const index = Number(item.dataset.i);
    return event.clientY > rect.top + rect.height / 2 ? index + 1 : index;
  }

  function wireDragAndDrop() {
    const list = $("#fav-list");
    // Vollständiger Endpoint des gerade gezogenen Favoriten. Remote-Pfade sind nur
    // gemeinsam mit Provider und Profil-ID eindeutig.
    let draggingFavorite = null;

    // Entfernt alle Drop-Rückmeldungen: die Liste-Hervorhebung (Ordner-Import) und
    // die Einfügelinien-Markierung (Umsortieren).
    function clearDropMarkers() {
      list.classList.remove("drop-target");
      for (const marked of list.querySelectorAll(".drop-before, .drop-after")) {
        marked.classList.remove("drop-before", "drop-after");
      }
    }

    // Finder-artige Einfügelinie: markiert die Kante, an der beim Loslassen eingefügt
    // würde. Über einem Eintrag entscheidet die obere/untere Hälfte (wie dropIndex),
    // über der freien Fläche landet die Linie unter dem letzten Eintrag.
    function markInsertion(event) {
      const item = event.target.closest(".fav-item");
      if (!item) {
        list.querySelector(".fav-item:last-child")?.classList.add("drop-after");
        return;
      }
      const rect = item.getBoundingClientRect();
      const after = event.clientY > rect.top + rect.height / 2;
      item.classList.add(after ? "drop-after" : "drop-before");
    }

    // Umsortieren: der gezogene Favorit merkt sich seinen vollständigen Endpoint im
    // DataTransfer und lokal für das mögliche Entfernen beim Herausziehen.
    list.addEventListener("dragstart", (event) => {
      const item = event.target.closest(".fav-item");
      if (!item) return;
      const favorite = favorites[Number(item.dataset.i)];
      if (!favorite) return;
      draggingFavorite = favorite;
      event.dataTransfer.setData(FAVORITE_DRAG_TYPE, endpointKey(favorite));
      event.dataTransfer.effectAllowed = "move";
      item.classList.add("dragging");
    });
    list.addEventListener("dragend", (event) => {
      clearDropMarkers();
      list.querySelector(".dragging")?.classList.remove("dragging");
      // Finder-artig: Wird ein Favorit klar außerhalb der Liste losgelassen (nicht
      // umsortiert), entfernen wir ihn. Escape bricht den Zug ab und liefert 0/0 —
      // dann nichts tun, damit ein Abbruch keinen Favoriten löscht.
      if (draggingFavorite && (event.clientX !== 0 || event.clientY !== 0)) {
        const rect = list.getBoundingClientRect();
        const outside = event.clientX < rect.left || event.clientX > rect.right
          || event.clientY < rect.top || event.clientY > rect.bottom;
        if (outside) remove(draggingFavorite);
      }
      draggingFavorite = null;
    });

    // Beide Quellen (Pane-Ordner und Favoriten) landen auf derselben Liste.
    list.addEventListener("dragover", (event) => {
      const types = event.dataTransfer?.types || [];
      const isFavorite = types.includes(FAVORITE_DRAG_TYPE);
      const isFolder = types.includes(FOLDER_DRAG_TYPE);
      if (!isFavorite && !isFolder) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = isFavorite ? "move" : "copy";
      clearDropMarkers();
      // Umsortieren zeigt die dünne Einfügelinie; ein neuer Ordner hebt die ganze
      // Liste als Drop-Ziel hervor (er wird immer hinten angehängt).
      if (isFavorite) markInsertion(event);
      else list.classList.add("drop-target");
    });
    list.addEventListener("dragleave", (event) => {
      if (!list.contains(event.relatedTarget)) clearDropMarkers();
    });
    list.addEventListener("drop", (event) => {
      clearDropMarkers();
      const reorderKey = event.dataTransfer.getData(FAVORITE_DRAG_TYPE);
      if (reorderKey) {
        event.preventDefault();
        try {
          const endpoint = JSON.parse(reorderKey);
          const favorite = favorites.find(candidate => endpointKey(candidate) === JSON.stringify(endpoint));
          if (!favorite) return;
          const target = dropIndex(event);
          const from = favorites.indexOf(favorite);
          // Beim Ziehen nach unten rückt die Liste durch das Entfernen um eins auf.
          moveTo(favorite, from >= 0 && target > from ? target - 1 : target);
        } catch (_) {
          // Defekte interne Drag-Payloads verändern die Favoriten nicht.
        }
        return;
      }
      const payload = event.dataTransfer.getData(FOLDER_DRAG_TYPE);
      if (!payload) return;
      event.preventDefault();
      try {
        const { path, name, provider = "local", profileId = null } = JSON.parse(payload);
        add(path, name, { provider, profileId });
      } catch (_) {
        // Fremde/defekte Drops still ignorieren; es geht nur um eigene Pane-Ordner.
      }
    });
  }

  function wire() {
    $("#fav-add").addEventListener("click", () => {
      const side = activeSide();
      const pane = panes[side];
      if (pane.path) add(pane.path, null, { provider: pane.provider, profileId: pane.profileId });
    });

    $("#fav-list").addEventListener("click", (event) => {
      const removeButton = event.target.closest(".fav-del");
      if (removeButton) {
        event.stopPropagation();
        const favorite = favorites[Number(removeButton.dataset.del)];
        if (favorite) remove(favorite);
        return;
      }
      const item = event.target.closest(".fav-item");
      if (!item) return;
      const favorite = favorites[Number(item.dataset.i)];
      if (favorite) {
        const endpoint = endpointOf(favorite);
        const profile = endpoint.provider === "local"
          ? {}
          : resolveProfile(endpoint.provider, endpoint.profileId);
        if (profile?.unavailable) {
          showDock('<span class="pill">Profil wird aktualisiert</span>Der Favorit bleibt unverändert.');
          return;
        }
        if (!profile) {
          showError("Favorit öffnen", {
            code: "not_found",
            message: "Das zugehörige Remote-Profil ist nicht mehr vorhanden.",
          });
          return;
        }
        setPaneLocation(activeSide(), {
          ...profile,
          provider: endpoint.provider,
          profileId: endpoint.profileId,
          path: endpoint.path,
        });
      }
    });

    // Rechtsklick auf einen Favoriten öffnet das wiederverwendbare Kontextmenü mit
    // dem Entfernen-Befehl; ein Klick ins Fenster schließt es (globaler Handler).
    $("#fav-list").addEventListener("contextmenu", (event) => {
      const item = event.target.closest(".fav-item");
      if (!item) return;
      event.preventDefault();
      const favorite = favorites[Number(item.dataset.i)];
      if (!favorite) return;
      openMenu({
        x: event.clientX,
        y: event.clientY,
        ariaLabel: "Favorit",
        items: [{ label: "Aus Favoriten entfernen", action: () => remove(favorite) }],
      });
    });

    wireDragAndDrop();
  }

  return { add, load, wire, importFromFinder };
}
