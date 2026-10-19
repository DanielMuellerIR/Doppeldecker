import { invoke } from "./tauri-api.js";
import { $ } from "./ui-utils.js";
import { isTextTarget, isMac } from "./file-clipboard.js";

// Tastaturereignisse werden schon am sichtbaren Modal abgefangen. Native
// Menüereignisse kommen dagegen direkt von Tauri und müssen dieselbe Schranke
// ausdrücklich beachten, damit sie keinen hinter dem Dialog liegenden Pane-Scope
// ändern. Darstellungseinstellungen und die Updateprüfung bleiben unabhängig.
const PANE_MENU_ACTIONS = new Set([
  "open", "toggle_hidden", "refresh", "sort_name", "sort_size", "sort_date",
  "swap_panes", "back", "forward", "up", "home", "goto", "quarantine",
  "sync_mode", "new_folder", "copy_over", "move_over", "rename", "trash",
  "information", "view_options",
  "compare_files",
  "paste_move",
  "files_copy", "files_cut", "files_paste", "files_move",
]);

/** Bündelt native Menüereignisse und globale Tastaturkürzel. Alle Fachaktionen
 * werden injiziert, damit dieses Modul weder Sync- noch Pane-Zustand dupliziert. */
export function createInputController(actions) {
  function onKeyDown(event) {
    if (event.defaultPrevented || !$("#modal").hidden) return;
    const modifier = event.metaKey || event.ctrlKey;
    // Ein neues Fenster besitzt seine eigene Queue und darf daher auch entstehen,
    // während dieses Fenster gerade arbeitet oder den Sync-Modus zeigt.
    if (modifier && !event.shiftKey && event.code === "KeyN") {
      actions.newWindow();
      return event.preventDefault();
    }
    if (modifier && !event.shiftKey && event.code === "Comma") {
      actions.showPreferences();
      return event.preventDefault();
    }
    if (actions.running() && event.key === "Escape") {
      // Nach der Übergabe an ein externes Werkzeug kann dessen Fenster bereits
      // offen sein. Tastatur und sichtbarer Abbruchknopf gelten für dieselbe Phase.
      if (!$("#btn-cancel").hidden) invoke("cancel_sync");
      event.preventDefault();
      return;
    }
    if (actions.browsingBlocked?.()) {
      event.preventDefault();
      return;
    }
    // Der Sync-Modus ist ein bewusst betretener Zustand: ⇧⌘S (Linux: Ctrl+Shift+S)
    // öffnet/schließt ihn, Escape verlässt ihn. Solange er offen ist, bleiben die
    // Pane-Kürzel (Navigation, F5/F6, Type-ahead) gezielt inaktiv.
    if (modifier && event.shiftKey && event.code === "KeyS") {
      actions.syncView.toggle();
      return event.preventDefault();
    }
    if (actions.syncView.isOpen()) {
      if (event.key === "Escape") {
        actions.syncView.close();
        event.preventDefault();
      }
      return;
    }

    if (modifier && !event.altKey && !event.shiftKey && ["KeyC", "KeyX"].includes(event.code) && isTextTarget(event.target)) {
      actions.clipboard.onTextCopy();
      return;
    }
    if (modifier && !event.shiftKey && !event.isComposing && !isTextTarget(event.target)
      && ["KeyC", "KeyX", "KeyV"].includes(event.code) && (!event.altKey || (isMac && event.code === "KeyV"))) {
      if (event.code === "KeyV") void actions.clipboard.paste(isMac && event.altKey);
      else void actions.clipboard.copy(event.code === "KeyX");
      return event.preventDefault();
    }
    if (modifier && !event.altKey && !event.shiftKey && event.code === "KeyT") {
      actions.addTab(actions.activeSide());
      return event.preventDefault();
    }
    if (modifier && !event.altKey && event.shiftKey && event.code === "KeyW") {
      actions.closeTab(actions.activeSide());
      return event.preventDefault();
    }
    if (event.ctrlKey && !event.metaKey && !event.altKey && event.key === "Tab") {
      actions.cycleTab(actions.activeSide(), event.shiftKey ? -1 : 1);
      return event.preventDefault();
    }

    // Informationen und Darstellungsoptionen sind echte globale Menüaktionen.
    // Sie bleiben deshalb auch erreichbar, wenn die Pfadleiste oder ein Pane-Knopf
    // den Fokus hält. Ohne Modifier behalten diese Bedienelemente ihre normale
    // Texteingabe und Tastatursteuerung.
    const exactInformationShortcut = modifier
      && !event.shiftKey
      && !event.altKey
      && !event.getModifierState?.("AltGraph")
      && !event.isComposing;
    if (exactInformationShortcut && (event.code === "KeyI" || event.code === "KeyJ")) {
      const side = actions.activeSide();
      if (event.code === "KeyI") actions.showInformation(side);
      else actions.showViewOptions(side);
      return event.preventDefault();
    }

    // Native Bedienelemente behalten Enter, Leertaste und ihre eigenen
    // Pfeiltasten. Andernfalls würde die globale Pane-Steuerung etwa einen
    // fokussierten Seitenleisten-Button als „Datei öffnen“ fehlinterpretieren und
    // zugleich dessen regulären Klick verhindern.
    if (event.target.closest?.("input, textarea, select, button, summary, a[href], [contenteditable=\"true\"]")) {
      return;
    }

    // Ein fokussierter Trenner (Seitenleiste oder Spaltenkopf) bedient sich selbst:
    // Pfeiltasten ändern dort die Breite, Tab wandert normal zum nächsten Element
    // weiter statt die Pane zu wechseln.
    if (event.target.getAttribute?.("role") === "separator") return;

    const side = actions.activeSide();
    const pane = actions.panes[side];

    if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) {
      const row = document.getElementById(actions.optionId(side, pane.cursor));
      const rect = (row || pane.listing).getBoundingClientRect();
      actions.showContextMenu(
        side,
        rect.left + Math.min(24, rect.width / 2),
        rect.top + Math.min(rect.height, actions.paneRowHeight(side)),
        true,
      );
      event.preventDefault();
      return;
    }

    if (modifier) {
      switch (event.code) {
        case "ArrowUp":
          actions.goUp(side);
          return event.preventDefault();
        case "ArrowDown":
          actions.openCursor(side);
          return event.preventDefault();
        case "BracketLeft":
          actions.back(side);
          return event.preventDefault();
        case "BracketRight":
          actions.forward(side);
          return event.preventDefault();
        case "KeyR":
          actions.refresh(side);
          return event.preventDefault();
        case "KeyA":
          actions.selectAll(side);
          return event.preventDefault();
        case "Digit1":
          actions.setSort(side, "name");
          return event.preventDefault();
        case "Digit2":
          actions.setSort(side, "size");
          return event.preventDefault();
        case "Digit3":
          actions.setSort(side, "modified");
          return event.preventDefault();
        case "Period":
          if (event.shiftKey) {
            actions.toggleHidden(side);
            return event.preventDefault();
          }
          break;
        case "KeyG":
          if (event.shiftKey) {
            actions.focusPath(side);
            return event.preventDefault();
          }
          break;
        case "KeyH":
          if (event.shiftKey) {
            actions.goHome(side);
            return event.preventDefault();
          }
          break;
        case "KeyN":
          if (event.shiftKey) {
            actions.newFolder();
            return event.preventDefault();
          }
          break;
        case "Backspace":
          actions.trashSelection();
          return event.preventDefault();
      }
      return;
    }

    switch (event.key) {
      case "ArrowDown":
        actions.moveCursor(side, 1, event.shiftKey);
        return event.preventDefault();
      case "ArrowUp":
        actions.moveCursor(side, -1, event.shiftKey);
        return event.preventDefault();
      case "Home":
        actions.moveCursorTo(side, 0, event.shiftKey);
        return event.preventDefault();
      case "End":
        actions.moveCursorTo(side, pane.entries.length - 1, event.shiftKey);
        return event.preventDefault();
      case "PageDown":
        actions.moveCursor(side, actions.pageSize(side), event.shiftKey);
        return event.preventDefault();
      case "PageUp":
        actions.moveCursor(side, -actions.pageSize(side), event.shiftKey);
        return event.preventDefault();
      case "Enter":
        actions.openCursor(side);
        return event.preventDefault();
      case "Backspace":
        actions.goUp(side);
        return event.preventDefault();
      case "Tab":
        actions.setActive(side === "left" ? "right" : "left");
        return event.preventDefault();
      case "F2":
        actions.renameCursor();
        return event.preventDefault();
      case "F5":
        actions.transferToOther("copy");
        return event.preventDefault();
      case "F6":
        actions.transferToOther("move");
        return event.preventDefault();
      case "Escape":
        actions.hideContextMenu();
        actions.clearSelection(side);
        return;
    }

    if (event.key.length === 1 && !event.altKey) {
      actions.typeAhead(side, event.key);
      event.preventDefault();
    }
  }

  function onMenu(id) {
    if (id === "new_window") return actions.newWindow();
    // Die Updateprüfung bleibt bewusst auch während einer laufenden Operation
    // erreichbar. Sie stellt sich aber selbst zurück, solange der gemeinsame
    // `#modal`-Dialog belegt ist — sonst ersetzte sie einen offenen Konflikt-
    // oder Rename-Dialog und ließe dessen Dateioperation hängen (update-ui.js).
    if (id === "check_updates") return actions.checkForUpdates();
    if (id === "preferences") return actions.showPreferences();
    if (!$("#modal").hidden && PANE_MENU_ACTIONS.has(id)) return;
    if (actions.browsingBlocked?.()) return;
    // Im offenen Sync-Modus wirken nur Darstellung und der Sync-Eintrag selbst;
    // Pane-Aktionen blieben sonst unsichtbar auf den verdeckten Listen hängen.
    if (actions.syncView.isOpen() && !["theme_auto", "theme_light", "theme_dark", "sync_mode"].includes(id)) return;
    const side = actions.activeSide();
    switch (id) {
      case "open": return actions.openCursor(side);
      case "information": return actions.showInformation(side);
      case "compare_files": return actions.compareFiles();
      case "theme_auto": return actions.setTheme("auto");
      case "theme_light": return actions.setTheme("light");
      case "theme_dark": return actions.setTheme("dark");
      case "units_decimal": return actions.setByteUnits("decimal");
      case "units_binary": return actions.setByteUnits("binary");
      case "view_options": return actions.showViewOptions(side);
      case "toggle_hidden": return actions.toggleHidden(side);
      case "refresh": return actions.refresh(side);
      case "sort_name": return actions.setSort(side, "name");
      case "sort_size": return actions.setSort(side, "size");
      case "sort_date": return actions.setSort(side, "modified");
      case "swap_panes": return actions.swapPanes();
      case "back": return actions.back(side);
      case "forward": return actions.forward(side);
      case "up": return actions.goUp(side);
      case "home": return actions.goHome(side);
      case "goto": return actions.focusPath(side);
      case "quarantine": return actions.openQuarantineManager();
      case "import_finder_favorites": return actions.importFinderFavorites();
      case "sync_mode": return actions.syncView.open();
      case "new_folder": return actions.newFolder();
      case "copy_over": return actions.transferToOther("copy");
      case "move_over": return actions.transferToOther("move");
      case "files_copy": return actions.clipboard.copy();
      case "files_cut": return actions.clipboard.copy(true);
      case "files_paste": return actions.clipboard.paste();
      case "files_move": return actions.clipboard.paste(true);
      case "paste_move":
        if (!isTextTarget(document.activeElement)) return actions.clipboard.paste(true);
        return;
      case "rename": return actions.renameCursor();
      case "trash": return actions.trashSelection();
    }
  }

  return { onKeyDown, onMenu };
}
