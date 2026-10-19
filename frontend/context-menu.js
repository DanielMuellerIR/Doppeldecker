// Tastatur- und Maus-Kontextmenü. Die Menü-Mechanik (Bauen, im Fenster platzieren,
// Tastatur-Navigation, Fokusrückgabe) ist wiederverwendbar; die Fachaktionen kommen
// als Item-Liste bzw. Callbacks herein. Dadurch kennt dieses Modul weder Queue noch
// Providerzustände und lässt sich auch von der Seitenleiste (Favoriten) nutzen.

import { $$, isEntryNameSafe, joinPath } from "./ui-utils.js";

// Es kann immer nur ein Menü offen sein; es lebt als einziges Element mit der ID
// „context-menu". Den Fokus vor dem Öffnen merken wir uns modulweit, damit Escape/Tab
// ihn an die auslösende Stelle zurückgeben können — egal, welcher Aufrufer öffnete.
let returnFocus = null;

/** Schließt ein offenes Menü und gibt optional den vorher aktiven Fokus zurück. */
export function hideMenu(restoreFocus = false) {
  document.getElementById("context-menu")?.remove();
  const target = returnFocus;
  returnFocus = null;
  if (restoreFocus && target?.isConnected) target.focus({ preventScroll: true });
}

/** Baut ein Menü aus einer Item-Liste und platziert es vollständig im Fenster.
 * items: Array aus { label, action } oder { separator: true }. Bei keyboard=true
 * bekommt der erste Eintrag direkt den Fokus (Aufruf per Tastatur). */
export function openMenu({ x, y, items, keyboard = false, ariaLabel = "Aktionen" }) {
  hideMenu();
  returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const menu = document.createElement("div");
  menu.className = "context-menu";
  menu.id = "context-menu";
  menu.setAttribute("role", "menu");
  menu.setAttribute("aria-label", ariaLabel);

  for (const entry of items) {
    if (entry.separator) {
      const element = document.createElement("div");
      element.className = "ctx-sep";
      element.setAttribute("role", "separator");
      menu.appendChild(element);
      continue;
    }
    const item = document.createElement("button");
    item.type = "button";
    item.className = "ctx-item";
    item.setAttribute("role", "menuitem");
    item.tabIndex = -1;
    item.textContent = entry.label;
    item.addEventListener("click", () => {
      // Dialoge übernehmen `document.activeElement` als Rücksprungziel. Deshalb
      // muss der gespeicherte Pane-Fokus wieder aktiv sein, bevor die Fachaktion
      // etwa den Informationsdialog öffnet.
      hideMenu(true);
      entry.action();
    });
    menu.appendChild(item);
  }

  // Unsichtbar messen, danach vollständig innerhalb des Fensters platzieren.
  menu.style.visibility = "hidden";
  document.body.appendChild(menu);
  const rect = menu.getBoundingClientRect();
  menu.style.left = Math.min(x, window.innerWidth - rect.width - 6) + "px";
  menu.style.top = Math.min(y, window.innerHeight - rect.height - 6) + "px";
  menu.style.visibility = "visible";
  const menuItems = $$(".ctx-item", menu);
  if (menuItems.length) menuItems[0].tabIndex = 0;
  menu.addEventListener("keydown", (event) => {
    const current = Math.max(0, menuItems.indexOf(document.activeElement));
    let next = null;
    if (event.key === "ArrowDown") next = (current + 1) % menuItems.length;
    else if (event.key === "ArrowUp") next = (current - 1 + menuItems.length) % menuItems.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = menuItems.length - 1;
    else if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      hideMenu(true);
      return;
    } else if (event.key === "Tab") {
      hideMenu();
      return;
    }
    if (next !== null) {
      event.preventDefault();
      menuItems[current].tabIndex = -1;
      menuItems[next].tabIndex = 0;
      menuItems[next].focus();
    }
  });
  if (keyboard && menuItems.length) menuItems[0].focus({ preventScroll: true });
}

/** Kontextmenü für die Dateipanes: baut aus der aktuellen Auswahl die Item-Liste
 * und zeigt sie über die wiederverwendbare Menü-Mechanik. */
export function createContextMenu(actions) {
  function show(side, x, y, keyboard = false) {
    const pane = actions.pane(side);
    // Auf Leerfläche ist der Cursor kein Aktionsziel; Tastaturmenüs dürfen ihn
    // weiterhin verwenden, auch wenn gerade keine Mehrfachauswahl besteht.
    const entries = keyboard || pane.sel.size ? actions.selectedEntries(side) : [];
    const single = entries.length === 1;
    const safeSelection = entries.every(isEntryNameSafe);
    const items = [];

    if (entries.length && safeSelection) {
      if (single) items.push({ label: "Öffnen", action: () => actions.open(side) });
      if (single && entries[0].is_dir) {
        items.push({
          label: "Zu Favoriten hinzufügen",
          action: () => actions.addFavorite(
            joinPath(pane.path, entries[0].name),
            entries[0].name,
            { provider: pane.provider, profileId: pane.profileId },
          ),
        });
      }
      items.push({ label: "Ins andere Pane kopieren  (F5)", action: () => actions.transfer("copy") });
      items.push({ label: "Ins andere Pane verschieben  (F6)", action: () => actions.transfer("move") });
      if (pane.provider === "local") {
        items.push({ label: "Kopieren", action: () => actions.clipboardCopy(side) });
        if (!actions.isMac) items.push({ label: "Ausschneiden", action: () => actions.clipboardCut(side) });
      }
      if (single) items.push({ label: "Umbenennen  (F2)", action: actions.rename });
      if (actions.canCompare()) items.push({ label: "Dateien vergleichen", action: actions.compare });
      items.push({
        label: pane.provider === "local"
          ? "In den Papierkorb  (⌘⌫)"
          : "Remote löschen …  (⌘⌫)",
        action: actions.trash,
      });
      items.push({ separator: true });
    }
    if (entries.length) {
      items.push({ label: "Informationen  (⌘I)", action: () => actions.information(side) });
      items.push({ separator: true });
    }
    // Die Operationen laufen für Remote-Ziele durch dieselben Capability- und
    // Konfliktprüfungen wie die Tastaturaktionen; das Menü erfindet keine Freigaben.
    items.push({ label: "Neuer Ordner  (⌘⇧N)", action: actions.newFolder });
    items.push({ label: "Dateien einfügen", action: () => actions.clipboardPaste(side) });
    if (actions.isMac) items.push({ label: "Dateien hierher bewegen  (⌥⌘V)", action: () => actions.clipboardPaste(side, true) });
    items.push({ label: "Darstellungsoptionen …  (⌘J)", action: () => actions.viewOptions(side) });

    openMenu({ x, y, items, keyboard, ariaLabel: "Dateiaktionen" });
  }

  return { hide: hideMenu, show };
}
