import { wireFileDrops } from "./file-drops.js";
// Doppeldecker — Frontend-Logik (Grundgerüst: Navigation, Auswahl, Sortierung,
// Type-ahead, versteckte Dateien, native Menü-Events + Sync-Vorschau/Testlauf).
// Nutzt ES-Module ohne Bundler; Tauri stellt die native API weiterhin global bereit.

import { invoke } from "./tauri-api.js";
import { createBidirectionalUi } from "./bidirectional-ui.js";
import { createContextMenu } from "./context-menu.js";
import { createConnectionsUi } from "./connections-ui.js";
import { createDevicesUi } from "./devices-ui.js";
import { createConnectionEditors } from "./connection-editors.js";
import { createFileActions } from "./file-actions.js";
import { createFileClipboard, isMac } from "./file-clipboard.js";
import { createFileInformationUi } from "./file-information.js";
import { createFavoritesUi } from "./favorites-ui.js";
import { createEventController } from "./event-controller.js";
import { createInputController } from "./input-controller.js";
import { createOperationUi } from "./operation-ui.js";
import { createNetworkDiscoveryUi } from "./network-discovery-ui.js";
import { createOneWaySyncUi } from "./one-way-sync-ui.js";
import { createLocalSyncUi } from "./local-sync-ui.js";
import { createPaneController } from "./pane-controller.js";
import { choiceModal, onModalKeyDown } from "./modal.js";
import { byteUnitsMode, initByteUnits, setByteUnits } from "./byte-units.js";
import { initTheme, setTheme, themePreference } from "./theme.js";
import { createPlanView } from "./plan-view.js";
import { createQueueUi } from "./queue-ui.js";
import { createQuarantineUi } from "./quarantine-ui.js";
import { createRealModeUi } from "./real-mode-ui.js";
import { initSidebarSections } from "./sidebar-sections.js";
import { initSplitter } from "./splitter.js";
import { createSyncView } from "./sync-view.js";
import { createSettingsUi } from "./settings-ui.js";
import { createUpdateUi } from "./update-ui.js";
import { localFolderSizesEnabled } from "./view-options.js";
import {
  $,
  digestHex,
  errCode,
  errText,
  escapeHtml,
  fmtDate,
  isEntryNameSafe,
  joinPath,
} from "./ui-utils.js";

// -------------------------------------------------------------------------
// Zustand
// -------------------------------------------------------------------------
let activeSide = "left";
let operationRunning = false;
let startupQueueErrorActive = false;
const activityOwners = new Set();
let realModeUi = null;
let localSyncUi = null;
let fileInformationUi = null;
let settingsUi = null;

function changeTheme(preference) {
  if (settingsUi) {
    setTheme(preference);
    return settingsUi.set({ theme: preference });
  }
  return setTheme(preference);
}

function changeByteUnits(mode) {
  if (settingsUi) {
    applyByteUnits(mode);
    return settingsUi.set({ byteUnits: mode });
  }
  return applyByteUnits(mode);
}

function isRunning() {
  return operationRunning || activityOwners.size > 0;
}

function isBrowsingBlocked() {
  return activityOwners.size > 0;
}

function setOperationRunning(value) {
  operationRunning = Boolean(value);
}

/** Reserviert eine eigene Aktivitätsgrenze. Das zurückgegebene Ende entfernt nur
 * diesen Besitzer und kann deshalb keinen parallel gemeldeten Queue-Lauf freigeben. */
function beginActivity(owner) {
  if (isRunning()) return null;
  const token = Symbol(owner);
  activityOwners.add(token);
  $("#app").classList.add("activity-locked");
  let active = true;
  return () => {
    if (!active) return false;
    active = false;
    const removed = activityOwners.delete(token);
    $("#app").classList.toggle("activity-locked", activityOwners.size > 0);
    return removed;
  };
}

/** Bietet auf macOS den stabilen Terminalbefehl an, sobald die App aus einem
 * Programme-Ordner läuft. Der Backendzustand enthält absichtlich keine Pfade. */
async function offerCliInstall() {
  let status;
  try {
    status = await invoke("cli_install_status");
  } catch (error) {
    // Nur „auf dieser Plattform nicht vorhanden“ ist ein normaler stiller
    // Zustand. Konfigurations-, Rechte- und Dateisystemfehler müssen sichtbar sein.
    if (errCode(error) !== "unavailable") showError("CLI-Einrichtung", error);
    return;
  }
  if (status?.state !== "offer") return;

  const choice = await choiceModal(
    "Terminalbefehl einrichten?",
    "<p>Damit kannst du <code>doppeldecker-cli</code> direkt im Terminal und in Pipes verwenden.</p>" +
      "<p>Doppeldecker legt dafür einen Verweis unter <code>/usr/local/bin</code> an. " +
      "macOS fragt einmal nach deinem Anmeldepasswort.</p>",
    [
      { label: "Später", value: "later", cls: "ghost" },
      { label: "Nicht mehr fragen", value: "decline", cls: "ghost" },
      { label: "Einrichten", value: "install", cls: "primary" },
    ],
  );
  if (choice === "decline") {
    try {
      await invoke("decline_cli_install");
    } catch (error) {
      showError("CLI-Einrichtung", error);
    }
    return;
  }
  if (choice !== "install") return;
  try {
    await invoke("install_cli");
    showDock(
      "<span class=\"pill add\">CLI bereit</span><code>doppeldecker-cli</code> ist jetzt im Terminal verfügbar.",
      { closable: true },
    );
  } catch (error) {
    showError("CLI-Einrichtung", error);
  }
}

function discardRealPreparation() {
  return Promise.all([realModeUi?.discard(), localSyncUi?.discard()]);
}

const syncView = createSyncView({
  discardPreparation: discardRealPreparation,
  endpoints: () => currentSourceTarget(),
  onDirectionChanged: () => {
    refreshQuarantineBadge();
  },
  onMetadataChanged: () => updateMetadataNote(),
  remoteSyncContext: () => remoteSyncContext(),
  running: isRunning,
});

let connectionsUi;
const {
  acceptEvent: acceptOperationEvent,
  bindContext: bindOperationContext,
  dockRevision,
  end: endProgressUi,
  showDock,
  startFile: startOpUi,
  startSync: startProgressUi,
  update: onProgress,
} = createOperationUi({
  captureEndpoints: () => ["left", "right"].map(side => {
    const { provider, profileId, profileRevision, path } = panes[side];
    return { provider, profileId, profileRevision, path };
  }),
  planSummaryHtml: plan => planSummaryHtml(plan),
  setRunning: setOperationRunning,
});

/** Einheitliche Fehleranzeige im Dock (ersetzt die früher ~20-fach duplizierte
 *  showDock-Fehlerzeile). */
function showError(label, e, { foldDetails = false } = {}) {
  // Das Backend akzeptiert für das Log nur einen kurzen Kontext und stabilen Code.
  // Der Fehlertext selbst kann Providerantworten oder Pfade enthalten und wird nie
  // an den Logger gesendet.
  void invoke("record_app_error", {
    context: label,
    code: typeof e?.code === "string" ? e.code : "app",
  }).catch(() => {});
  // Wartet der Auftrag auf eine Bestätigung, führt ein Knopf direkt dorthin. Die
  // Queue hängt sonst nur an einem kleinen Symbol im Seitenleistenkopf; im
  // GUI-Test vom 2026-09-04 war der Weg dorthin nicht zu finden. Erkannt wird der
  // Fall am stabilen Code, nie am deutschen Text.
  const wartetAufBestaetigung = errCode(e) === "queue_confirmation_required";
  const aktion = wartetAufBestaetigung
    ? '<button class="dock-action" type="button" data-dock-action="open-queue">Queue öffnen</button>'
    : "";
  const text = errText(e);
  const preview = Array.from(text).slice(0, 180).join("");
  const content = foldDetails
    ? `${escapeHtml(preview)}${preview.length < text.length ? "…" : ""}<div class="quarantine-meta">Einträge oder Zwischenstände können erhalten sein. Vor Bereinigung oder erneutem Versuch die Details prüfen.</div><details class="operation-error-details"><summary>Vollständiger Fehler und Prüfpfade</summary><div class="operation-error-body">${escapeHtml(text)}</div></details>`
    : escapeHtml(text);
  return showDock(
    `<span class="pill delete">${escapeHtml(label)}</span>${content}${aktion}`,
    { closable: true },
  );
}

const paneController = createPaneController({
  activate: setActive,
  changeByteUnits,
  direction: syncView.direction,
  discardRealPreparation,
  hideContextMenu: (...args) => hideContextMenu(...args),
  invalidateQuarantineBadge: (...args) => invalidateQuarantineBadge(...args),
  onDirectorySizeChange: (...args) => fileInformationUi?.onDirectorySizeChange(...args),
  onStateChanged: () => settingsUi?.rememberSoon(),
  onGlobalPreferenceChange: patch => settingsUi?.set(patch),
  running: isBrowsingBlocked,
  resolveProfile: (protocol, profileId) => connectionsUi?.profileLocation(protocol, profileId),
  refreshQuarantineBadge: (...args) => refreshQuarantineBadge(...args),
  renderConnections: (...args) => renderConnections(...args),
  showDock,
  showError,
  showContextMenu: (...args) => showContextMenu(...args),
});
const {
  back,
  clearSelection,
  changeLocalFolderSizes,
  disablePane,
  invalidateProfileTabs,
  focusPath,
  forward,
  goHome,
  goUp,
  loadPane,
  moveCursor,
  moveCursorTo,
  onDirectorySizeDone,
  onDirectorySizeError,
  onDirectorySizeResult,
  openCursor,
  openViewOptions,
  optionId,
  pageSize,
  paneRowHeight,
  panes,
  refresh,
  refreshColumnDefaults,
  renderRows,
  selectAll,
  setPaneLocation,
  setSort,
  startDirectorySizes,
  swapPanes,
  toggleHidden,
  typeAhead,
  updateProfileName,
  wirePane,
} = paneController;

const devicesUi = createDevicesUi({
  activeSide: () => activeSide,
  disablePane,
  panes,
  setPaneLocation,
  showDock,
  showError,
});

const updateUi = createUpdateUi({
  beginActivity,
  running: isRunning,
  showDock,
  showError,
});


// -------------------------------------------------------------------------
// Pane-Aktivierung
// -------------------------------------------------------------------------
function setActive(side, { focus = true } = {}) {
  activeSide = side;
  for (const s of ["left", "right"]) panes[s].el.classList.toggle("active", s === side);
  if (focus) panes[side].listing.focus();
  settingsUi?.rememberSoon();
}

/**
 * Größeneinheit umschalten (Menü „Darstellung"). Die Wahl ändert nur die
 * Schreibweise, nie einen Wert. Sichtbar wirkt sie sofort in den Panes; bereits
 * gerenderte Nebenansichten (Plan, Queue, Quarantäne) zeigen die neue Einheit beim
 * nächsten Öffnen — sie werden dafür bewusst nicht im Hintergrund neu aufgebaut.
 */
function applyByteUnits(mode, persist = true) {
  setByteUnits(mode, persist);
  for (const side of ["left", "right"]) {
    refreshColumnDefaults(side);
    renderRows(side);
  }
  devicesUi.render();
}

// -------------------------------------------------------------------------
// Datei-Operationen (Kopieren/Verschieben/Papierkorb/Umbenennen/Neuer Ordner)
// Laufen über die Sicherheitsschicht im Kern; Löschen geht in den Papierkorb
// (recoverable). Vor dem Überschreiben wird immer gefragt.
// -------------------------------------------------------------------------

const fileActions = createFileActions({
  panes,
  dockRevision,
  activeSide: () => activeSide,
  running: isRunning,
  enqueueBlocked: isBrowsingBlocked,
  loadPane,
  showDock,
  showError,
});
const selectedEntries = (side) => fileActions.selectedEntries(side);
const refreshBothPanes = context => fileActions.refreshBothPanes(context);
const transferToOther = (kind) => fileActions.transferToOther(kind);
const trashSelection = () => fileActions.trashSelection();
const renameCursor = () => fileActions.renameCursor();
const newFolder = () => fileActions.newFolder();
const fileClipboard = createFileClipboard({ panes, activeSide: () => activeSide,
  selectedEntries, fileActions, blocked: isBrowsingBlocked, showDock, showError });
for (const event of ["copy", "cut", "paste"]) document.addEventListener(event, fileClipboard.onNativeEvent);
fileInformationUi = createFileInformationUi({
  panes,
  selectedEntries,
  requestDirectorySizes: startDirectorySizes,
});
const showInformation = side => fileInformationUi.open(side);
const showViewOptions = side => openViewOptions(side);

function paneEndpoint(pane) {
  return pane.provider === "local"
    ? { provider: "local", root: pane.path }
    : { provider: pane.provider, profileId: pane.profileId,
      profileRevision: pane.profileRevision, root: pane.path };
}

function selectedDiffFile(side) {
  const pane = panes[side];
  const [entry] = selectedEntries(side);
  if (selectedEntries(side).length !== 1 || !entry || entry.is_dir || !isEntryNameSafe(entry)) return null;
  return { endpoint: paneEndpoint(pane), path: joinPath(pane.path, entry.name), name: entry.name };
}

function canCompareFiles() { return Boolean(selectedDiffFile("left") && selectedDiffFile("right")); }

async function compareFiles() {
  const left = selectedDiffFile("left");
  const right = selectedDiffFile("right");
  if (!left || !right) {
    showDock('<span class="pill warn">Dateivergleich</span>Bitte in beiden Panes genau eine reguläre Datei auswählen.', { closable: true });
    return;
  }
  try {
    // Das fenstergebundene Terminalevent kommt vor dem nächsten Queueworker.
    // Die Command-Antwort darf dessen neuere Anzeige nicht wieder überschreiben.
    await invoke("compare_files", { request: { left, right } });
  } catch (error) { showError("Dateivergleich", error); }
}

function paneStartLocation(pane) {
  return {
    provider: pane.provider,
    profileId: pane.profileId,
    profileRevision: pane.profileRevision,
    profileName: pane.profileName,
    path: pane.path,
  };
}

function paneSessionLocation(pane) {
  // codereview-ok: Der Sitzungsvertrag speichert absichtlich nur Endpunkt-ID,
  // Revision und Pfad. Nach dem Löschen eines Profils ist dessen ID der sichere
  // Fallback; ein alter Anzeigename dürfte keinen fortbestehenden Treffer vortäuschen (2026-09-12).
  const { profileName, ...location } = paneStartLocation(pane);
  return location;
}

async function newWindow() {
  // Beide Orte werden synchron eingefroren. Würden wir sie erst nach dem
  // Default-Abruf lesen, könnte eine Navigation im alten Fenster unbemerkt den
  // Startzustand des neuen Fensters verändern.
  const frozen = {
    left: paneStartLocation(panes.left),
    right: paneStartLocation(panes.right),
  };
  try {
    const needsDefaults = ["left", "right"].filter(
      side => frozen[side].provider === "local" && !frozen[side].path,
    );
    const defaults = needsDefaults.length ? await invoke("default_dirs") : null;
    const location = side => needsDefaults.includes(side)
      ? { provider: "local", profileId: null, profileRevision: null,
        profileName: null, path: defaults[side] }
      : frozen[side];
    await invoke("new_app_window", {
      left: location("left"),
      right: location("right"),
    });
  } catch (error) {
    showError("Neues Fenster", error);
  }
}

const {
  add: addFavorite,
  load: loadFavorites,
  wire: wireFavorites,
  importFromFinder: importFinderFavorites,
} = createFavoritesUi({
  activeSide: () => activeSide,
  panes,
  resolveProfile: (protocol, profileId) => connectionsUi?.profileLocation(protocol, profileId),
  setPaneLocation,
  showDock,
  showError,
});

connectionsUi = createConnectionsUi({
  activeSide: () => activeSide,
  panes,
  disablePane,
  invalidateProfileTabs,
  setPaneLocation,
  showError,
  updateProfileName,
});
const renderConnections = (...args) => connectionsUi.render(...args);
const saveConnectionProfile = (...args) => connectionsUi.save(...args);
let loadingConnections = null;
const loadConnections = () => {
  // CLI und GUI teilen dieselben atomar geschriebenen Profildateien. Während eines
  // laufenden Abrufs bündelt dieser Promise weitere Fokusereignisse, damit nie zwei
  // Profilladevorgänge gegeneinander rendern.
  if (!loadingConnections) {
    loadingConnections = connectionsUi.loadAll().finally(() => {
      loadingConnections = null;
    });
  }
  return loadingConnections;
};
const { openDiscoveredService, openProfileEditor, openProtocolPicker } = createConnectionEditors({
  digestHex,
  reloadProfiles: loadConnections,
  saveProfile: saveConnectionProfile,
  showDock,
  showError,
});
const networkDiscoveryUi = createNetworkDiscoveryUi({
  onEnabledChange: enabled => settingsUi?.set({ networkDiscovery: enabled }),
  openService: openDiscoveredService,
  showError,
});
function wireConnections() {
  connectionsUi.wire({
    editProfile: openProfileEditor,
    openPicker: openProtocolPicker,
    openQuarantine: () => openQuarantineManager(),
    openQueue: () => openQueueManager(),
  });
}

// -------------------------------------------------------------------------
// Persistente Operations-Queue
// -------------------------------------------------------------------------
const quarantineUi = createQuarantineUi({
  currentSourceTarget,
  refreshPanes: refreshBothPanes,
  running: isRunning,
  showDock,
  showError,
});
const {
  closeModal: closeQuarantineModal,
  invalidateBadge: invalidateQuarantineBadge,
  open: openQuarantineManager,
  refreshBadge: refreshQuarantineBadge,
  setRetentionDays: setQuarantineRetentionDays,
  showModal: showQuarantineModal,
} = quarantineUi;

const bidirectionalUi = createBidirectionalUi({
  panes,
  dockRevision,
  syncScope,
  realRunActive: () => realModeUi?.isActive() ?? false,
  running: isRunning,
  startRun: label => startOpUi(label),
  endRun: () => endProgressUi(),
  showDock,
  showError,
});
const doBidirectionalSync = () => bidirectionalUi.run();
const recoverBidirectionalQueueEntry = entry => bidirectionalUi.recoverQueueEntry(entry);

const {
  applySnapshot: applyQueueSnapshot,
  open: openQueueManager,
  refresh: refreshQueue,
  warnBeforeExit: warnBeforeQueueExit,
} = createQueueUi({
  closeModal: closeQuarantineModal,
  queueErrorActive: () => startupQueueErrorActive,
  realRunActive: () => realModeUi?.isActive() ?? false,
  recoverBidirectional: recoverBidirectionalQueueEntry,
  showDock,
  showError,
  showModal: showQuarantineModal,
  startFileProgress: startOpUi,
  startSyncProgress: startProgressUi,
});

// Ein einziger Handler am Container statt eines Handlers je Meldung: `showDock`
// ersetzt seinen Inhalt bei jedem neuen Ereignis. Ein direkt am Knopf angehängter
// Handler überlebt nur, solange dieselbe Fehleranzeige ihn neu setzt — rendert
// eine andere Stelle das Dock neu, bliebe ein sichtbarer, aber toter Knopf übrig.
$("#dock-summary").addEventListener("click", event => {
  if (event.target.closest("[data-dock-action=\"open-queue\"]")) {
    void openQueueManager();
  }
});

// -------------------------------------------------------------------------
// Kontextmenü (Rechtsklick)
// -------------------------------------------------------------------------
const contextMenu = createContextMenu({
  selectedEntries,
  pane: (side) => panes[side],
  open: openCursor,
  addFavorite,
  transfer: transferToOther,
  clipboardCopy: side => fileClipboard.copy(false, side),
  clipboardCut: side => fileClipboard.copy(true, side),
  clipboardPaste: (side, move = false) => fileClipboard.paste(move, side),
  isMac,
  rename: renameCursor,
  trash: trashSelection,
  newFolder,
  information: showInformation,
  viewOptions: showViewOptions,
  canCompare: canCompareFiles,
  compare: compareFiles,
});
const hideContextMenu = (restoreFocus = false) => contextMenu.hide(restoreFocus);
const showContextMenu = (side, x, y, keyboard = false) => contextMenu.show(side, x, y, keyboard);

const { onKeyDown, onMenu } = createInputController({
  browsingBlocked: isBrowsingBlocked,
  clipboard: fileClipboard,
  addTab: side => paneController.addTab(side),
  closeTab: side => paneController.closeTab(side),
  cycleTab: (side, offset) => paneController.cycleTab(side, offset),
  activeSide: () => activeSide,
  back,
  clearSelection,
  checkForUpdates: updateUi.check,
  compareFiles,
  focusPath,
  forward,
  goHome,
  goUp,
  hideContextMenu,
  importFinderFavorites,
  moveCursor,
  moveCursorTo,
  newFolder,
  newWindow,
  openCursor,
  openQuarantineManager,
  optionId,
  pageSize,
  paneRowHeight,
  panes,
  refresh,
  renameCursor,
  running: isRunning,
  selectAll,
  setActive,
  setByteUnits: changeByteUnits,
  setSort,
  setTheme: changeTheme,
  showPreferences: () => settingsUi?.open(),
  showContextMenu,
  showInformation,
  showViewOptions,
  swapPanes,
  syncView,
  toggleHidden,
  transferToOther,
  trashSelection,
  typeAhead,
});

const eventController = createEventController({
  onOperationContext: context => fileClipboard.onContext(context),
  onFileOperationDone: (summary, failed) => fileClipboard.onDone(summary, failed),
  acceptOperationEvent,
  bindOperationContext,
  startFileProgress: startOpUi,
  applyQueueSnapshot,
  endProgress: endProgressUi,
  finishRealRun: () => realModeUi?.finishActive(),
  metadataReportHtml,
  onMenu,
  onProgress,
  onDirectorySizeDone,
  onDirectorySizeError,
  onDirectorySizeResult,
  refreshPanes: refreshBothPanes,
  refreshQuarantine: refreshQuarantineBadge,
  refreshQueue,
  onConnectionsUpdated: update => connectionsUi.handleConnectionUpdate(update),
  reloadPanes: refreshBothPanes,
  reloadTargetPane: context => {
    if (context) return refreshBothPanes(context);
    const side = syncView.direction() === "rl" ? "left" : "right";
    return loadPane(side, panes[side].path, { push: false });
  },
  setRunning: setOperationRunning,
  showDock,
  showError,
  warnBeforeQueueExit,
});
const wireEvents = () => eventController.wire();

// -------------------------------------------------------------------------
// Sync (Vorschau / Testlauf)
// -------------------------------------------------------------------------
function currentSourceTarget() {
  // „In beide Richtungen" hat keine Rollen; die Chips zeigen dann links zuerst.
  const srcSide = syncView.direction() === "rl" ? "right" : "left";
  const dstSide = srcSide === "left" ? "right" : "left";
  return { source: panes[srcSide].path, target: panes[dstSide].path, sourcePane: panes[srcSide], targetPane: panes[dstSide] };
}

function remoteSyncContext() {
  const current = currentSourceTarget();
  const remoteCount = [current.sourcePane, current.targetPane].filter((pane) => pane.provider !== "local").length;
  if (remoteCount === 0) return null;
  if (remoteCount === 2) {
    const endpoint = pane => ({
      provider: pane.provider,
      profileId: pane.profileId,
      profileRevision: pane.profileRevision,
      root: pane.path,
    });
    return {
      protocol: "provider",
      source: endpoint(current.sourcePane),
      target: endpoint(current.targetPane),
    };
  }
  const upload = current.targetPane.provider !== "local";
  const remotePane = upload ? current.targetPane : current.sourcePane;
  const localPane = upload ? current.sourcePane : current.targetPane;
  return {
    profileId: remotePane.profileId,
    profileRevision: remotePane.profileRevision,
    direction: upload ? "Upload" : "Download",
    localRoot: localPane.path,
    remoteRoot: remotePane.path,
    protocol: remotePane.provider,
  };
}
function syncOptions() {
  return syncView.options();
}

// Nur sichtbaren Bedienzustand binden; ausführbare Pläne und Permits bleiben im
// Backend. Die Zähler erkennen auch Weg-und-zurück-Navigation und erneut geöffnete
// Syncansichten mit denselben Pfaden und Optionen.
function syncScope() {
  return JSON.stringify({
    revision: syncView.revision(),
    options: syncOptions(),
    panes: [panes.left, panes.right].map(pane => ({
      provider: pane.provider, profileId: pane.profileId,
      profileRevision: pane.profileRevision, path: pane.path,
      loadSeq: pane.loadSeq, tab: pane.tabs?.active?.id,
    })),
  });
}

const metadataClassLabels = {
  Owner: "Eigentümer/Gruppe",
  Acl: "ACL",
  NormalMode: "normale Rechte",
  SpecialModeBits: "Sonderbits",
  Xattrs: "Extended Attributes",
  ResourceFork: "Ressourcen-Fork",
  SparseExtents: "Sparse-Struktur",
  Hardlinks: "Hardlinks",
};

function metadataReportHtml(report) {
  if (!report) return "";
  const transferred = (report.classes || []).reduce((sum, item) => sum + item.transferred, 0);
  const omitted = (report.classes || []).reduce((sum, item) => sum + item.omitted, 0);
  if (report.profile === "Portable") {
    return omitted ? `<span class="pill delete">${omitted} Rechte ausgelassen</span>` : "";
  }
  let html = `<span class="pill update">${transferred} native Merkmale</span>`;
  if (omitted) html += `<span class="pill delete">${omitted} nicht exakt darstellbar</span>`;
  if (report.strict_verified) html += `<span class="pill ok">Strikt vollständig geprüft</span>`;
  return html;
}

/** Klartext-Zeile unter der Metadaten-Profilwahl der Sync-Ansicht (früher die
 *  dauerhafte „Noch kein Vollbackup"-Box der Seitenleiste): benennt, was das
 *  gewählte Profil wirklich überträgt. Der Backup-Satz darunter steht statisch
 *  im Markup und bleibt in jedem Profil sichtbar. */
function updateMetadataNote(report = null) {
  const title = $("#sync-metadata-note-title");
  if (syncView.metadata() === "Portable") {
    const omittedRights = (report?.classes || []).some(item => item.class === "NormalMode" && item.omitted > 0);
    title.textContent = omittedRights
      ? "Überträgt Inhalt und Änderungszeit. Normale Rechte sind auf diesem Ziel nicht exakt darstellbar und werden ausgelassen."
      : "Überträgt Inhalt, Änderungszeit und normale Rechte (rwx), soweit am Ziel darstellbar.";
  } else if (syncView.metadata() === "NativeBestEffort") {
    title.textContent = "Überträgt zusätzlich native Merkmale; nicht Darstellbares wird einzeln bilanziert.";
  } else if (report && report.strict_verified) {
    title.textContent = "Metadaten strikt geprüft; Realmodus mit Restore-Probe verfügbar.";
  } else {
    title.textContent = "Alle Merkmale exakt; die vollständige Prüfung läuft mit der Vorschau.";
  }
}


const { planSummaryHtml, showPlan } = createPlanView({
  metadataClassLabels,
  metadataReportHtml,
  updateMetadataNote,
  showDock,
});
// Startbefüllung für das voreingestellte Profil; danach halten Profilwechsel
// (onMetadataChanged) und Planvorschauen (showPlan) die Zeile aktuell.
updateMetadataNote();

realModeUi = createRealModeUi({
  currentSourceTarget,
  syncScope,
  endProgress: endProgressUi,
  ensureContentHash: syncView.ensureContentHash,
  mode: syncView.mode,
  onProgressStart: startProgressUi,
  remoteSyncContext,
  running: isRunning,
  showDock,
  showError,
  showPlan,
  syncOptions,
});

const doRemoteSync = remote => realModeUi.runRemote(remote);
const doRealSync = () => realModeUi.run();

localSyncUi = createLocalSyncUi({ currentSourceTarget, syncOptions,
  ensureContentHash: syncView.ensureContentHash, showPlan, showDock, showError, dockRevision });

const oneWaySyncUi = createOneWaySyncUi({
  compare: syncView.compare,
  currentSourceTarget,
  syncScope,
  discardPreparation: discardRealPreparation,
  dockRevision,
  ensureContentHash: syncView.ensureContentHash,
  metadata: syncView.metadata,
  prepareRemotePreview: remote => realModeUi.prepareRemotePreview(remote),
  previewAllowed: () => syncView.isOpen() && syncView.direction() !== "both" && !isRunning(),
  remoteSyncContext,
  runRemote: doRemoteSync,
  runLocal: current => localSyncUi.run(current),
  running: isRunning,
  showDock,
  showError,
  showPlan,
  syncOptions,
});
const preview = () => oneWaySyncUi.preview();
const doSync = () => oneWaySyncUi.run();

async function init() {
  // Zuerst die Größeneinheit: Die Standardbreite der Größenspalte wird beim
  // Verdrahten der Panes gemessen und hängt am Einheitenkürzel („MB" oder „MiB").
  initByteUnits();
  initSidebarSections();
  wirePane("left");
  wirePane("right");
  devicesUi.wire();
  networkDiscoveryUi.wire();
  syncView.wire({
    bidirectional: doBidirectionalSync,
    preview,
    real: doRealSync,
    sync: doSync,
    simulate: () => oneWaySyncUi.run({ simulate: true }),
  });
  initTheme();
  // Eine schmalere Seitenleiste macht die Panes breiter: Die Listen müssen neu
  // gezeichnet werden, damit das Datumsformat der neuen Spaltenbreite folgt.
  initSplitter({ onResize: () => { for (const side of ["left", "right"]) renderRows(side); } });
  wireFavorites();
  wireConnections();
  settingsUi = createSettingsUi({
    applyByteUnits,
    applyLocalFolderSizes: enabled => changeLocalFolderSizes(enabled, true, false),
    applyNetworkDiscovery: (enabled, persist) => networkDiscoveryUi.setEnabled(enabled, persist),
    applyTheme: setTheme,
    // Die Sitzungsdatei enthält je Remote-Endpunkt nur Protokoll, Profil-ID,
    // Revision und Pfad. Der Profilname wird beim Start ohnehin aus dem Profil
    // aufgelöst und würde in der Datei nach einer Umbenennung nur veralten.
    currentWindow: () => ({
      panes: { left: paneSessionLocation(panes.left), right: paneSessionLocation(panes.right) },
      tabs: paneController.tabsSnapshot(),
      activeSide,
    }),
    legacyPreferences: () => ({
      theme: themePreference(),
      byteUnits: byteUnitsMode(),
      localFolderSizes: localFolderSizesEnabled(),
      networkDiscovery: networkDiscoveryUi.enabled(),
    }),
    showError,
  });
  $("#modal").addEventListener("keydown", onModalKeyDown);
  document.addEventListener("keydown", onKeyDown);
  window.addEventListener("resize", () => {
    for (const side of ["left", "right"]) renderRows(side);
  });
  // Jeder Klick ins Fenster schließt ein offenes Kontextmenü (die Menüpunkte selbst
  // schließen es vor ihrer Aktion, ihr Bubble-Klick trifft hier nur noch ins Leere).
  document.addEventListener("click", () => hideContextMenu());
  try {
    if (!window.__TAURI__ || !window.__TAURI__.core || !window.__TAURI__.webview) {
      const keys = window.__TAURI__ ? Object.keys(window.__TAURI__).join(", ") : "(kein __TAURI__)";
      throw new Error("Tauri-API fehlt — vorhandene Schlüssel: " + keys);
    }
    await wireEvents();
    await wireFileDrops({ panes, fileActions, running: isBrowsingBlocked, showError });
    await settingsUi.init();
    // Setup kann beschädigte Nebenfenster-Journale schon finden, bevor diese
    // Webview Ereignisse abonnieren kann. Erst jetzt wird der backendseitig
    // gehaltene Befund einmalig und ausschließlich für `main` abgeholt.
    const [startupQueueError, startupAppError] = await Promise.all([
      invoke("take_startup_queue_error"),
      invoke("take_startup_app_error"),
    ]);
    startupQueueErrorActive = Boolean(startupQueueError);
    if (startupQueueError) {
      $("#queue-count").textContent = "!";
      showError("Queue-Fehler", startupQueueError);
    }
    if (startupAppError) showError("Start-Fehler", startupAppError);
    // Verbindungsprofile werden vor dem Sitzungsstand gelesen, damit gespeicherte
    // Remote-Ziele die aktuelle monotone Profilrevision verwenden.
    await loadConnections();
    const initial = await invoke("take_window_start_state");
    const savedTabs = await invoke("restored_pane_tabs");
    if (savedTabs) {
      await paneController.restoreTabs(savedTabs);
    } else if (initial) {
      const current = location => {
        if (location.provider === "local") return location;
        const profile = connectionsUi.profileLocation(location.provider, location.profileId);
        return profile ? { ...location, ...profile } : location;
      };
      await Promise.all([
        setPaneLocation("left", current(initial.left)),
        setPaneLocation("right", current(initial.right)),
      ]);
    } else {
      const defaults = await invoke("default_dirs");
      await loadPane("left", defaults.left);
      await loadPane("right", defaults.right);
    }
    // Mount-Erkennung und Bonjour-Suche sind reine Seitenleistenanreicherung. Ein
    // träges Systemvolume oder die erste macOS-LAN-Freigabe darf weder Pane-Bedienung
    // noch den restlichen Startpfad aufhalten.
    void devicesUi.load();
    void networkDiscoveryUi.refresh({ force: true });
    const restoredSide = await invoke("restored_active_side");
    setActive(restoredSide === "right" ? "right" : "left");
    // Ab jetzt ist der Fensterstand vollständig; frühere `rememberSoon`-Aufrufe
    // aus den Start-Listings waren absichtlich wirkungslos.
    const safety = await invoke("safety_info");
    syncView.setSafetyInfo(safety);
    setQuarantineRetentionDays(safety.quarantine_retention_days);
    realModeUi.setAvailable(safety.real_mode_available);
    if (safety.real_enabled) {
      const banner = $("#test-mode-banner");
      banner.textContent = `REALER TESTMODUS · Schreibzugriffe ausschließlich in ${safety.sandbox}`;
      banner.hidden = false;
    }
    // Remote-Favoriten werden erst sichtbar, wenn ihre aktuelle Profilrevision
    // aufgelöst werden kann. So gibt es beim Start kein bedienbares Zwischenfenster
    // mit einem fälschlichen „Profil nicht gefunden“.
    await loadFavorites();
    // Nach einem Import im Terminal kehrt der Fokus zur App zurück. Dann werden die
    // geheimnisfreien Profile neu gelesen; ein Neustart von Doppeldecker ist unnötig.
    window.addEventListener("focus", () => {
      void loadConnections();
      void devicesUi.load({ silent: true });
      void networkDiscoveryUi.refresh({ silent: true });
    });
    await refreshQueue();
    // `refreshQueue()` schreibt den normalen Eintragszähler. Ein beim Start
    // gefundener Recovery-Fehler bleibt davon unabhängig offen und behält deshalb
    // das sichtbare Fehlerzeichen bis zum nächsten App-Start.
    if (startupQueueError) $("#queue-count").textContent = "!";
    await refreshQuarantineBadge();
    // Die optionale Einrichtung darf den App-Start nicht blockieren. Sie läuft
    // erst, nachdem Panes, Queue und Profile bereits vollständig bedienbar sind.
    void offerCliInstall();
  } catch (e) {
    showError("Start-Fehler", e);
  } finally {
    // Auch wenn ein Pane-Listing oder die Sandbox-Vorbereitung scheitert, müssen
    // spätere Navigation und Fensterbewegungen den Sitzungsstand wieder speichern
    // können. Solange beide Pane-Pfade fehlen, bleibt der sofortige Aufruf wirkungslos.
    settingsUi.startRemembering();
  }
  // Version klein im Fensterkopf zeigen (unkritisch: ein Fehler bleibt folgenlos).
  try {
    const version = await invoke("app_version");
    if (version) $("#side-version").textContent = `v${version}`;
  } catch (_) {
    // Ohne Versionsangabe bleibt der Kopf einfach leer.
  }
}

window.addEventListener("DOMContentLoaded", init);
