import { invoke } from "./tauri-api.js";
import { createPaneTabs } from "./pane-tabs.js";
import { createPaneTabsView } from "./pane-tabs-ui.js";
import {
  DATE_WIDEST_SAMPLE,
  entrySizeSample,
  initPaneColumns,
  measureText,
} from "./column-layout.js";
import { FOLDER_DRAG_TYPE } from "./favorites-ui.js";
import {
  $, $$, escapeHtml, fmtEntryDate, fmtEntryKind, fmtEntryPermissions, fmtEntrySize,
  isEntryNameSafe, joinPath,
} from "./ui-utils.js";
import {
  defaultViewOptions,
  initViewOptionsPopover,
  localFolderSizesEnabled,
  loadViewOptions,
  saveFolderViewOptions,
  setLocalFolderSizesEnabled,
} from "./view-options.js";

const TYPE_RESET_MS = 500;
export const VIRTUALIZE_AFTER = 2000;
const VIRTUAL_OVERSCAN = 8;
const SORT_KEYS = new Set([
  "name", "size", "kind", "modified", "created", "owner", "group", "permissions",
]);
const NAME_COLLATOR = new Intl.Collator("de-DE", { numeric: true, sensitivity: "base" });

function makePane() {
  return {
    provider: "local",
    profileId: null,
    profileRevision: null,
    profileName: null,
    securityStatus: null,
    quarantineCapabilities: null,
    securityNoticeKey: null,
    path: "",
    parent: null,
    entries: [],
    listingOmissions: [],
    listingOmissionText: "",
    listingOmissionsExpanded: false,
    sel: new Set(),
    cursor: 0,
    anchor: 0,
    history: [],
    hpos: -1,
    typeBuffer: "",
    typeTime: 0,
    // Jede neue Ladeanforderung entwertet ältere Antworten. Langsame Remote-
    // Antworten können so niemals eine inzwischen neuere Navigation zurückdrehen.
    loadSeq: 0,
    navigationRevision: 0,
    navigationPending: false,
    el: null,
    listing: null,
    renderStart: 0,
    renderEnd: 0,
    // Spaltenbedienung dieser Pane und die daraus folgende Datumsstufe. Die Stufe
    // ist reine Darstellung; sortiert wird weiterhin numerisch über mtime.
    columns: null,
    dateStages: { modified: 3, created: 3 },
    sort: { key: "name", dir: 1 },
    viewOptions: defaultViewOptions(),
    viewOptionsRevision: 0,
    viewPopover: null,
    listGutter: -1,
    // Lauf-ID und Frontend-Token sind panegebunden. Das Token entwertet auch ein
    // Ergebnis, das nach einer Navigation noch aus dem Backend eintrifft.
    directorySizeToken: 0,
    directorySizeRunId: null,
    tabs: null,
    tabsView: null,
  };
}

/**
 * Kapselt Pane-Zustand, Navigation, Auswahl und Virtualisierung. Fachaktionen wie
 * Sync, Quarantäne und Verbindungslisten bleiben über kleine Rückrufe im Hauptmodul.
 */
export function createPaneController({
  activate,
  changeByteUnits,
  discardRealPreparation,
  direction,
  hideContextMenu,
  invalidateQuarantineBadge,
  onDirectorySizeChange = () => {},
  onStateChanged = () => {},
  onGlobalPreferenceChange = () => {},
  running = () => false,
  resolveProfile = () => null,
  refreshQuarantineBadge,
  renderConnections,
  showDock,
  showError,
  showContextMenu,
}) {
  const panes = { left: makePane(), right: makePane() };

  function saveActiveTab(pane) {
    if (!pane.path) return;
    // Nach einem fehlgeschlagenen Tabwechsel ist die sichtbare Liste leer.
    // Bis zum nächsten erfolgreichen Listing bleibt der letzte bewiesene Stand erhalten.
    if (pane.tabs?.active.runtime.reads.has("listing")
        || pane.tabs?.active.runtime.status === "failed") return;
    if (!pane.tabs) pane.tabs = createPaneTabs({
      provider: pane.provider, profileId: pane.profileId,
      profileRevision: pane.profileRevision, path: pane.path,
    }, pane.viewOptions);
    const tab = pane.tabs.active;
    tab.location = { provider: pane.provider, profileId: pane.profileId,
      profileRevision: pane.profileRevision, path: pane.path };
    const historyStart = Math.max(0, pane.hpos - 127);
    tab.history = pane.history.length ? pane.history.slice(historyStart, historyStart + 128) : [pane.path];
    tab.historyIndex = pane.history.length ? pane.hpos - historyStart : 0;
    tab.selectedNames = [...new Set([...pane.sel].map(index => pane.entries[index]?.name).filter(Boolean))];
    tab.cursorName = pane.entries[pane.cursor]?.name ?? null;
    tab.anchorName = pane.entries[pane.anchor]?.name ?? null;
    tab.scrollTop = pane.listing?.scrollTop || 0;
    tab.sort = { ...pane.sort };
    tab.viewOptions = structuredClone(pane.viewOptions);
    tab.runtime.entries = pane.entries;
  }

  function renderTabs(side) {
    const pane = panes[side];
    if (pane.tabs) pane.tabsView?.render(pane.tabs);
  }

  async function loadTab(side, tab) {
    const pane = panes[side];
    pane.navigationRevision += 1;
    // Vor jedem Await werden alte Listen und ihre Tokens entwertet. Auch ein
    // zuvor sichtbarer Remote-Tab erhält erst nach frischem Listing wieder Zeilen.
    const sequence = ++pane.loadSeq;
    pane.navigationPending = true;
    pane.directorySizeToken += 1;
    const location = tab.location;
    const profile = location.provider === "local" ? null
      : resolveProfile(location.provider, location.profileId);
    tab.location = { ...location, profileRevision: profile?.profileRevision ?? location.profileRevision };
    const restore = { ...tab, selectedNames: [...tab.selectedNames], sort: { ...tab.sort },
      viewOptions: structuredClone(tab.viewOptions) };
    pane.provider = location.provider;
    pane.profileId = location.profileId;
    pane.profileRevision = profile?.profileRevision ?? location.profileRevision;
    pane.profileName = profile?.profileName ?? null;
    pane.securityStatus = null;
    pane.securityNoticeKey = null;
    pane.quarantineCapabilities = location.provider === "local" ? null
      : { listing: false, restore: false, purge: false };
    clearPaneContents(pane);
    pane.path = location.path;
    pane.history = [...tab.history];
    pane.hpos = tab.historyIndex;
    pane.sort = { ...tab.sort };
    pane.viewOptions = structuredClone(tab.viewOptions);
    applyViewOptions(side, { renderNow: false });
    render(side);
    renderTabs(side);
    renderConnections();
    invalidateQuarantineBadge();
    if (location.provider !== "local" && (!profile || profile.unavailable)) {
      try {
        await discardRealPreparation();
        if (sequence !== pane.loadSeq) return;
        await cancelDirectorySizeRun(side);
        if (sequence !== pane.loadSeq) return;
        tab.runtime.status = "failed";
        renderTabs(side);
        showError("Tab öffnen", profile?.unavailable
          ? { code: "plan_drift", message: "Das Verbindungsprofil dieses Tabs wird aktualisiert. Bitte erneut öffnen." }
          : { code: "not_found", message: "Das Verbindungsprofil dieses Tabs ist nicht mehr vorhanden." });
        onStateChanged();
      } finally {
        if (sequence === pane.loadSeq) pane.navigationPending = false;
      }
      return;
    }
    await loadPane(side, location.path, { push: false, tabRestore: restore });
    onStateChanged();
  }

  function selectTab(side, id) {
    const pane = panes[side];
    if (running() || !pane.tabs || id === pane.tabs.active.id) return;
    saveActiveTab(pane);
    return loadTab(side, pane.tabs.activate(id));
  }

  function addTab(side) {
    const pane = panes[side];
    if (running() || !pane.path) return;
    saveActiveTab(pane);
    if (pane.tabs.tabs.length === 32) return;
    const tab = pane.tabs.add(pane.tabs.active.location);
    tab.sort = { ...pane.sort };
    return loadTab(side, pane.tabs.activate(tab.id));
  }

  function closeTab(side, id = panes[side].tabs?.active.id) {
    const pane = panes[side];
    if (running() || !pane.tabs || pane.tabs.tabs.length === 1) return;
    saveActiveTab(pane);
    const active = pane.tabs.active.id === id;
    const next = pane.tabs.close(id);
    if (active) return loadTab(side, next);
    renderTabs(side);
    onStateChanged();
  }

  function cycleTab(side, offset) {
    const model = panes[side].tabs;
    if (!model) return;
    const tabs = model.tabs;
    const index = tabs.findIndex(tab => tab.id === model.active.id);
    return selectTab(side, tabs[(index + offset + tabs.length) % tabs.length].id);
  }

  function invalidateProfileTabs(provider, profileId) {
    for (const side of ["left", "right"]) {
      const pane = panes[side];
      saveActiveTab(pane);
      pane.tabs?.invalidateProfile(provider, profileId);
      renderTabs(side);
    }
  }

  function tabsSnapshot() {
    for (const pane of Object.values(panes)) saveActiveTab(pane);
    return Object.fromEntries(["left", "right"].map(side => {
      const saved = panes[side].tabs?.snapshot();
      // Die persistierte Auswahl ist laut Sitzungsvertrag begrenzt. Im offenen
      // Tab bleibt die vollständige Auswahl im RAM erhalten.
      for (const tab of saved?.tabs || []) tab.selectedNames = tab.selectedNames.slice(0, 4096);
      return [side, saved];
    }));
  }

  async function restoreTabs(saved) {
    for (const side of ["left", "right"]) {
      const active = saved[side].tabs.find(tab => tab.id === saved[side].activeId);
      panes[side].tabs = createPaneTabs(active.location, active.viewOptions, { saved: saved[side] });
    }
    await Promise.all(["left", "right"].map(side => loadTab(side, panes[side].tabs.active)));
  }

  function paneLocation(pane, path = pane.path) {
    return { provider: pane.provider, profileId: pane.profileId, path };
  }

  function directorySizeEndpoint(pane) {
    return pane.provider === "local"
      ? { provider: "local", root: pane.path }
      : {
        provider: pane.provider,
        profileId: pane.profileId,
        profileRevision: pane.profileRevision,
        root: pane.path,
      };
  }

  function directorySizeEligible(entry) {
    return entry.is_dir && !entry.is_symlink && isEntryNameSafe(entry);
  }

  async function cancelDirectorySizeRun(side, { clear = true } = {}) {
    const pane = panes[side];
    pane.directorySizeToken += 1;
    pane.directorySizeRunId = null;
    if (clear) {
      for (const entry of pane.entries) delete entry.directorySize;
      if (pane.el) renderDirectorySizeChange(side);
    }
    // Ein fehlgeschlagener Abbruch darf Navigation nicht sperren. Das Frontend-
    // Token verwirft in diesem Fehlerfall weiterhin jedes verspätete Resultat.
    try {
      await invoke("cancel_directory_sizes", { side });
    } catch (_) {
      // Der nächste Backend-Start ersetzt denselben panegebundenen Lauf zusätzlich.
    }
  }

  async function startDirectorySizes(side, names, { recalculate = false, automatic = false } = {}) {
    const pane = panes[side];
    const requested = new Set(names);
    const entries = pane.entries.filter(
      entry => requested.has(entry.name) && directorySizeEligible(entry),
    );
    if (!entries.length || !pane.path) return;

    // Pro Pane existiert genau ein Größenlauf. Ein manueller Neulauf ersetzt einen
    // noch laufenden Batch; fertige Resultate anderer Ordner bleiben sichtbar.
    const token = ++pane.directorySizeToken;
    pane.directorySizeRunId = null;
    for (const entry of pane.entries) {
      if (entry.directorySize?.status === "loading") delete entry.directorySize;
    }
    for (const entry of entries) entry.directorySize = { status: "loading" };
    renderDirectorySizeChange(side);
    for (const entry of entries) onDirectorySizeChange(side, entry);
    const parent = pane.path;
    try {
      const started = await invoke("start_directory_sizes", {
        request: {
          side,
          endpoint: directorySizeEndpoint(pane),
          names: entries.map(entry => entry.name),
          ...(pane.provider === "local"
            ? {}
            : { sessionGeneration: pane.sessionGeneration }),
          recalculate,
          clientToken: token,
        },
      });
      if (token === pane.directorySizeToken && parent === pane.path) {
        pane.directorySizeRunId = started.run_id;
      }
    } catch (error) {
      if (token !== pane.directorySizeToken || parent !== pane.path) return;
      for (const entry of entries) entry.directorySize = { status: "failed", error };
      renderDirectorySizeChange(side);
      for (const entry of entries) onDirectorySizeChange(side, entry);
      showError(automatic ? "Automatische Ordnergrößen" : "Ordnergröße", error);
    }
  }

  function startAutomaticDirectorySizes(side) {
    const pane = panes[side];
    if (pane.provider !== "local" || !localFolderSizesEnabled()) return;
    const names = pane.entries.filter(directorySizeEligible).map(entry => entry.name);
    if (names.length) void startDirectorySizes(side, names, { automatic: true });
  }

  function changeLocalFolderSizes(enabled, persist = true, notify = true) {
    if (persist && notify) {
      onGlobalPreferenceChange({ localFolderSizes: Boolean(enabled) });
      return;
    }
    setLocalFolderSizesEnabled(enabled, persist);
    for (const side of ["left", "right"]) {
      const pane = panes[side];
      pane.viewPopover?.sync();
      if (pane.provider !== "local" || !pane.path) continue;
      if (enabled) startAutomaticDirectorySizes(side);
      else void cancelDirectorySizeRun(side);
    }
  }

  function directorySizeEventMatches(side, payload) {
    const pane = panes[side];
    return payload?.side === side
      && payload.client_token === pane.directorySizeToken
      && payload.parent === pane.path;
  }

  function onDirectorySizeResult(payload) {
    const side = payload?.side;
    if (!panes[side] || !directorySizeEventMatches(side, payload)) return;
    const entry = panes[side].entries.find(item => item.name === payload.name);
    if (!entry || !directorySizeEligible(entry)) return;
    entry.directorySize = payload.result;
    renderDirectorySizeChange(side);
    onDirectorySizeChange(side, entry);
  }

  /**
   * Beendet einen Messlauf und gibt jeder noch wartenden Zeile ihren Endzustand.
   *
   * Eine Zeile, die auf „lädt“ stehen bleibt, würde eine laufende Messung
   * vortäuschen, die es nicht mehr gibt. Das Ereignis wird nur angenommen, wenn
   * es zum aktuellen Lauf genau dieser Pane gehört.
   */
  function finishDirectorySizeRun(payload, pendingResult) {
    const side = payload?.side;
    if (!panes[side] || !directorySizeEventMatches(side, payload)) return false;
    const pane = panes[side];
    pane.directorySizeRunId = null;
    for (const entry of pane.entries) {
      if (entry.directorySize?.status === "loading") {
        entry.directorySize = pendingResult;
        onDirectorySizeChange(side, entry);
      }
    }
    renderDirectorySizeChange(side);
    return true;
  }

  function onDirectorySizeDone(payload) {
    finishDirectorySizeRun(payload, { status: "cancelled" });
  }

  function onDirectorySizeError(payload) {
    if (!finishDirectorySizeRun(payload, { status: "failed", error: payload.error })) return;
    showError("Ordnergröße", payload.error);
  }

  /**
   * Leert alles, was zur bisherigen Dateiliste einer Pane gehört.
   *
   * Bei einem Endpunktwechsel gehört die bisherige Liste zum ALTEN Provider.
   * Bliebe sie stehen und das erste Listing des neuen Endpunkts scheitert, würden
   * Dateiaktionen die alten Einträge als gleichnamige Pfade des NEUEN Providers
   * öffnen, umbenennen oder löschen. Erst ein erfolgreiches Listing füllt die
   * Pane wieder.
   */
  function clearPaneContents(pane) {
    pane.path = "";
    pane.parent = null;
    pane.entries = [];
    pane.listingOmissions = [];
    pane.listingOmissionText = "";
    pane.listingOmissionsExpanded = false;
    pane.sel = new Set();
    pane.cursor = 0;
    pane.anchor = 0;
  }

  function sortColumnVisible(pane, key) {
    return key === "name" || Boolean(pane.viewOptions.columns[key]);
  }

  function ensureVisibleSort(pane) {
    if (sortColumnVisible(pane, pane.sort.key)) return false;
    pane.sort = { key: "name", dir: 1 };
    return true;
  }

  function sortValue(entry, key) {
    if (key === "name") return entry.name;
    if (key === "size") {
      // Nur eine abgeschlossene Bilanz ohne Auslassungen wird nach Bytes sortiert.
      // Beobachtete SFTP-Größen bleiben dabei sichtbar als Best Effort markiert; laufende,
      // fehlgeschlagene und partielle Werte bleiben in beiden Richtungen am Ende.
      if (entry.is_dir) {
        return ["complete", "observed"].includes(entry.directorySize?.status)
          && Number.isFinite(entry.directorySize.bytes)
          ? entry.directorySize.bytes
          : null;
      }
      return Number.isFinite(entry.size) ? entry.size : null;
    }
    if (key === "kind") return fmtEntryKind(entry);
    if (key === "modified") return Number.isFinite(entry.mtime) ? entry.mtime : null;
    if (key === "created") return Number.isFinite(entry.created) ? entry.created : null;
    if (key === "owner") return entry.owner || null;
    if (key === "group") return entry.group || null;
    if (key === "permissions") return Number.isFinite(entry.mode) ? entry.mode : null;
    return null;
  }

  function sortEntries(entries, pane) {
    const { key, dir } = pane.sort;
    return entries.slice().sort((a, b) => {
      const options = pane.viewOptions;
      if (options.foldersFirst) {
        if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
        // Alphabetisch ist bewusst eine Unteroption des zusammenhängenden
        // Ordnerblocks. Ohne diesen Block muss dieselbe Spaltensortierung für alle
        // gemischten Paare gelten; sonst kann der Comparator Zyklen bilden.
        if (options.foldersAlphabetical && a.is_dir && b.is_dir) {
          return NAME_COLLATOR.compare(a.name, b.name);
        }
      }
      const left = sortValue(a, key);
      const right = sortValue(b, key);
      const leftMissing = left == null || left === "";
      const rightMissing = right == null || right === "";
      // Unbekannte Providerwerte bleiben in beiden Richtungen am Ende. Andernfalls
      // würden sie beim Richtungswechsel wie echte kleinste Werte an den Anfang
      // springen und damit eine nicht vorhandene Information vortäuschen.
      if (leftMissing !== rightMissing) return leftMissing ? 1 : -1;
      let cmp = 0;
      if (!leftMissing) {
        cmp = typeof left === "number"
          ? left - right
          : NAME_COLLATOR.compare(String(left), String(right));
      }
      if (cmp === 0) cmp = NAME_COLLATOR.compare(a.name, b.name);
      return cmp * dir;
    });
  }

  async function loadPane(side, path, opts = {}) {
    const pane = panes[side];
    // Ein Abschluss aktualisiert nur den sichtbaren Ort. Eine bereits angeforderte
    // Navigation besitzt Vorrang, auch solange Home noch seinen Zielpfad auflöst.
    if (opts.passive && pane.navigationPending) return;
    const seq = ++pane.loadSeq;
    pane.navigationPending = opts.push !== false || path !== pane.path
      || opts.historyPosition != null || Boolean(opts.tabRestore);
    if (opts.push !== false || path !== pane.path || opts.historyPosition != null) {
      pane.navigationRevision += 1;
    }
    if (path !== pane.path) invalidateQuarantineBadge();
    // Der gesamte Abruf gehört zu genau dem Endpunkt, der beim Start sichtbar war.
    // Während die beiden Vorbereitungs-Awaits laufen, kann ein anderes Fensterereignis
    // dieselbe Pane bereits auf ein anderes Profil umstellen. Deshalb lesen wir die
    // Providerdaten danach nicht nochmals aus dem veränderlichen Pane-Objekt.
    const requestedProvider = pane.provider;
    const requestedProfileId = pane.profileId;
    const requestedProfileRevision = pane.profileRevision;
    const requestedPath = path;
    const requestedLocation = {
      provider: requestedProvider,
      profileId: requestedProfileId,
      path: requestedPath,
    };
    const activeTab = pane.tabs?.active;
    const sameTabLocation = activeTab?.location.provider === requestedProvider
      && activeTab.location.profileId === requestedProfileId
      && activeTab.location.path === requestedPath;
    const tabRestore = opts.tabRestore || (sameTabLocation && activeTab.runtime.status === "failed"
      ? { selectedNames: [...activeTab.selectedNames], cursorName: activeTab.cursorName,
        anchorName: activeTab.anchorName, scrollTop: activeTab.scrollTop,
        viewOptions: structuredClone(activeTab.viewOptions) }
      : null);
    // Refresh behält die Darstellung dieses Tabs; Ordnerdefaults gelten beim Ortswechsel.
    const requestedOptions = structuredClone(tabRestore?.viewOptions
      || (sameTabLocation ? pane.viewOptions : loadViewOptions(requestedLocation)));
    const requestedOptionsRevision = pane.viewOptionsRevision;
    const refreshingVisibleLocation = sameTabLocation && requestedPath === pane.path;
    const tabRead = pane.tabs?.beginRead(pane.tabs.active.id, "listing");
    renderTabs(side);
    const { push = true, keepSelName = null, historyPosition = null } = opts;
    try {
      await discardRealPreparation();
      if (seq !== pane.loadSeq) return;
      await cancelDirectorySizeRun(side);
      if (seq !== pane.loadSeq) return;
      // Unmittelbar vor dem eigentlichen Provideraufruf gilt das Sequenz-Token noch
      // einmal. Ein veralteter Abruf darf den inzwischen gewählten Server nicht
      // einmal kurz ansprechen und seine Sitzung dabei lazy öffnen.
      if (seq !== pane.loadSeq) return;
      const listing = requestedProvider === "local"
        ? await invoke("scan_dir", { path: requestedPath, showHidden: requestedOptions.showHidden })
        : await invoke(`scan_${requestedProvider}_dir`, {
          profileId: requestedProfileId,
          profileRevision: requestedProfileRevision,
          path: requestedPath,
          showHidden: requestedOptions.showHidden,
        });
      // Die Antwort gehört erst nach dieser Prüfung noch zur Pane. Insbesondere
      // darf ein altes SFTP-Listing keine bereits getrennte Sitzung neu öffnen.
      if (seq !== pane.loadSeq) return;
      if (requestedProvider !== "local"
          && (!Number.isSafeInteger(listing.profile_revision)
            || listing.profile_revision < 0
            || listing.profile_revision !== requestedProfileRevision)) {
        throw {
          code: "plan_drift",
          message: "Das Listing gehört nicht mehr zur sichtbaren Profilrevision.",
        };
      }
      const quarantineCapabilities = requestedProvider !== "local"
        ? (await invoke(`connect_${requestedProvider}_profile`, {
          profileId: requestedProfileId,
          profileRevision: requestedProfileRevision,
        }))
          .quarantine_capabilities
        : null;
      if (seq !== pane.loadSeq) return;
      if (historyPosition !== null) pane.hpos = historyPosition;
      if (push && listing.path !== pane.path) {
        pane.history = pane.history.slice(0, pane.hpos + 1);
        pane.history.push(listing.path);
        pane.hpos = pane.history.length - 1;
      }
      pane.path = listing.path;
      pane.parent = listing.parent;
      pane.profileRevision = requestedProvider === "local"
        ? null
        : listing.profile_revision;
      pane.sessionGeneration = listing.session_generation || null;
      pane.securityStatus = listing.security || null;
      pane.listingOmissions = Array.isArray(listing.omitted) ? listing.omitted : [];
      pane.listingOmissionText = pane.listingOmissions.map(omission => `${omission.display_name}: ${omission.reason}`).join("\n");
      pane.listingOmissionsExpanded = false;
      pane.quarantineCapabilities = quarantineCapabilities;
      if (!refreshingVisibleLocation || requestedOptionsRevision === pane.viewOptionsRevision) {
        pane.viewOptions = requestedOptions;
      }
      ensureVisibleSort(pane);
      applyViewOptions(side, { renderNow: false });
      // Ein Hintergrundabschluss liest denselben sichtbaren Ort frisch, ohne
      // eine zwischenzeitlich geänderte Auswahl oder Scrollposition zurückzusetzen.
      const viewRestore = tabRestore || (opts.preserveView && refreshingVisibleLocation
        ? { selectedNames: [...pane.sel].map(index => pane.entries[index]?.name).filter(Boolean),
          cursorName: pane.entries[pane.cursor]?.name,
          anchorName: pane.entries[pane.anchor]?.name,
          scrollTop: pane.listing?.scrollTop || 0 }
        : null);
      pane.entries = sortEntries(listing.entries, pane);
      restoreCursor(pane, keepSelName);
      if (viewRestore) {
        const saved = viewRestore;
        const selectedNames = new Set(saved.selectedNames);
        pane.sel = new Set(pane.entries.flatMap((entry, index) => selectedNames.has(entry.name) ? [index] : []));
        pane.cursor = Math.max(0, pane.entries.findIndex(entry => entry.name === saved.cursorName));
        pane.anchor = Math.max(0, pane.entries.findIndex(entry => entry.name === saved.anchorName));
      }
      pane.typeBuffer = "";
      render(side);
      if (viewRestore) {
        pane.listing.scrollTop = viewRestore.scrollTop;
        renderRows(side);
      }
      if (tabRead) pane.tabs.finishRead(tabRead, tab => { tab.runtime.entries = pane.entries; });
      saveActiveTab(pane);
      renderTabs(side);
      onStateChanged();
      startAutomaticDirectorySizes(side);
      // Nach einem Remote-Listing existiert (lazy aufgebaut) eine lebende
      // Sitzung — die Seitenleiste zieht ihren Eject-Status nach.
      if (pane.provider !== "local") renderConnections();
      const sftpNoticeKey = `${pane.profileId || ""}\u0000sftp_path_best_effort`;
      if (pane.provider === "sftp"
          && pane.securityStatus === "sftp_path_best_effort"
          && pane.securityNoticeKey !== sftpNoticeKey) {
        pane.securityNoticeKey = sftpNoticeKey;
        showDock(
          `<span class="pill update">SFTP · pfadbasiertes Best Effort</span>Reguläre Dateien öffnen sich als geprüfte lokale Momentaufnahme; Dateien und begrenzte Verzeichnisbäume lassen sich additiv lokal herunterladen. Umfang und Auslassungen bleiben sichtbar. SFTP v3 bindet OPENDIR und OPEN nicht atomar an den Pane-Root; unbemerkter Pfadtausch bleibt möglich. Ersetzen, Sync und Remote-Mutationen bleiben gesperrt.`,
          { closable: true },
        );
      } else if (pane.provider === "ftps"
          && (pane.securityStatus === "plaintext" || pane.securityStatus === "tls_identity_unverified")) {
        // Wie im SFTP-Zweig darüber: einmal je Profil und Transportstufe. Ohne den
        // Schlüssel feuerte der Hinweis bei JEDEM Listing und ersetzte über
        // showDock den kompletten Dock-Inhalt — also auch die gerade gemeldete
        // „Ordner angelegt"-Bestätigung oder den Fehler eines gescheiterten
        // Umbenennens, den der Nutzer dann nie zu sehen bekam.
        const ftpsNoticeKey = `${pane.profileId || ""}\u0000${pane.securityStatus}`;
        if (pane.securityNoticeKey !== ftpsNoticeKey) {
          pane.securityNoticeKey = ftpsNoticeKey;
          showDock(
            pane.securityStatus === "plaintext"
              ? `<span class="pill update">FTP unverschlüsselt</span>${escapeHtml(pane.profileName || "Diese Verbindung")} überträgt auch Zugangsdaten und Dateiinhalte ohne TLS.`
              : `<span class="pill update">FTPS · Identität ungeprüft</span>${escapeHtml(pane.profileName || "Diese Verbindung")} ist verschlüsselt und gepinnt; das Zertifikat bestätigt den Servernamen nicht.`,
            { closable: true },
          );
        }
      }
      const dstSide = direction() === "lr" ? "right" : "left";
      if (side === dstSide && panes.left.path && panes.right.path) refreshQuarantineBadge();
    } catch (error) {
      if (seq !== pane.loadSeq) return;
      if (tabRead) pane.tabs.finishRead(tabRead, tab => { tab.runtime.status = "failed"; });
      renderTabs(side);
      $(".path", pane.el).value = pane.path;
      const provider = pane.provider === "local"
        ? "Lokales"
        : pane.provider === "ftps" && pane.securityStatus === "plaintext"
          ? "FTP"
          : pane.provider.toUpperCase();
      showError(`${provider}-Listing`, error);
    } finally {
      if (seq === pane.loadSeq) pane.navigationPending = false;
    }
  }

  function restoreCursor(pane, keepSelName) {
    let index = 0;
    if (keepSelName) {
      const found = pane.entries.findIndex(entry => entry.name === keepSelName);
      if (found >= 0) index = found;
    }
    pane.cursor = pane.entries.length ? Math.min(index, pane.entries.length - 1) : 0;
    pane.anchor = pane.cursor;
    pane.sel = pane.entries.length ? new Set([pane.cursor]) : new Set();
  }

  function paneRowHeight(side) {
    const root = side && panes[side]?.el ? panes[side].el : document.documentElement;
    const value = Number.parseFloat(
      getComputedStyle(root).getPropertyValue("--row-height"),
    );
    return Number.isFinite(value) && value > 0 ? value : 1;
  }

  function optionId(side, index) {
    return `pane-${side}-option-${index}`;
  }

  /**
   * Wunschbreite einer Spalte für den Doppelklick auf ihren Trenner.
   *
   * Größe: exakt der breiteste Wert, der in dieser Pane wirklich vorkommt.
   * Datum: die breiteste Variante der ausführlichsten Stufe. Nur so bleibt die volle
   * Form beim Scrollen garantiert vollständig — die Breite eines einzelnen Datums
   * sagt nichts über die restlichen Einträge (Monatsnamen sind unterschiedlich lang).
   */
  function fitColumnWidth(side, key) {
    const pane = panes[side];
    if (key === "date" || key === "created") return measureText(DATE_WIDEST_SAMPLE, pane.el);
    // Zwei Formen kommen vor: ganzzahlige Bytes und Werte mit zwei Nachkommastellen.
    // Innerhalb einer Form ist die längste Zeichenkette auch die breiteste (alle
    // Einheitskürzel sind gleich lang, Ziffern sind tabellarisch); zwischen den
    // Formen entscheidet die gemessene Breite.
    if (key === "size") {
      let longestBytes = "";
      let longestScaled = "";
      for (const entry of pane.entries) {
        const text = entry.is_dir
          ? directorySizeDisplayText(entry.directorySize)
          : (entry.is_file && entry.size == null ? "—" : fmtEntrySize(entry.size));
        if (!text) continue;
        if (text.includes(",")) {
          if (text.length > longestScaled.length) longestScaled = text;
        } else if (text.length > longestBytes.length) {
          longestBytes = text;
        }
      }
      if (!longestBytes && !longestScaled) return measureText(entrySizeSample(), pane.el);
      return Math.max(
        longestBytes ? measureText(longestBytes, pane.el) : 0,
        longestScaled ? measureText(longestScaled, pane.el) : 0,
      );
    }
    const text = entry => {
      if (key === "kind") return fmtEntryKind(entry);
      if (key === "owner") return entry.owner || "";
      if (key === "group") return entry.group || "";
      if (key === "permissions") return fmtEntryPermissions(entry.mode);
      return "";
    };
    return pane.entries.reduce(
      (width, entry) => Math.max(width, measureText(text(entry), pane.el)),
      0,
    );
  }

  /** Nach einem Wechsel der Größeneinheit passen die Standardbreiten nicht mehr. */
  function refreshColumnDefaults(side) {
    panes[side].columns?.refreshDefaults();
  }

  function resortPane(side) {
    const pane = panes[side];
    // `sortEntries` ordnet dieselben Eintragsobjekte neu. Ihre Referenzen sind
    // innerhalb dieses Listings eindeutig, auch wenn ein nicht sicher dekodierter
    // Rohname dieselbe sichtbare Ersatzzeichenfolge wie ein echter UTF-8-Name hat.
    // Der Anzeigename darf deshalb weder Cursor noch Auswahlidentität bilden.
    const cursorEntry = pane.entries[pane.cursor] || null;
    const selectedEntries = new Set(
      [...pane.sel].map(index => pane.entries[index]).filter(Boolean),
    );
    pane.entries = sortEntries(pane.entries, pane);
    const cursor = cursorEntry ? pane.entries.indexOf(cursorEntry) : -1;
    pane.cursor = cursor >= 0 ? cursor : (pane.entries.length ? 0 : 0);
    pane.anchor = pane.cursor;
    pane.sel = new Set();
    pane.entries.forEach((entry, index) => {
      if (selectedEntries.has(entry)) pane.sel.add(index);
    });
    if (!pane.sel.size && pane.entries.length) pane.sel.add(pane.cursor);
  }

  /** Bei aktiver Größensortierung muss jedes Streaming-Ergebnis die Rohwertreihenfolge
   * sofort neu bilden. In allen anderen Spalten reicht das billigere Zeilenrendern. */
  function renderDirectorySizeChange(side) {
    const pane = panes[side];
    if (pane.sort.key === "size") {
      resortPane(side);
      render(side);
    } else {
      renderRows(side);
    }
  }

  function applyViewOptions(side, { renderNow = true } = {}) {
    const pane = panes[side];
    if (!pane.el) return;
    pane.el.dataset.density = pane.viewOptions.density;
    pane.el.dataset.textSize = pane.viewOptions.textSize;
    pane.el.classList.toggle("view-bold-folders", pane.viewOptions.boldFolders);
    pane.el.classList.toggle("view-zebra", pane.viewOptions.zebraRows);
    pane.columns?.setVisibility(pane.viewOptions.columns);
    pane.viewPopover?.sync();
    if (renderNow) render(side);
  }

  function updateViewOptions(side, next, changedKey, { persist = true } = {}) {
    const pane = panes[side];
    const oldShowHidden = pane.viewOptions.showHidden;
    pane.viewOptionsRevision++;
    pane.viewOptions = next;
    if (persist) saveFolderViewOptions(paneLocation(pane), next);
    const sortReset = ensureVisibleSort(pane);
    applyViewOptions(side, { renderNow: false });
    if (changedKey === "textSize") pane.columns?.refreshDefaults();
    if (sortReset || changedKey === "foldersFirst" || changedKey === "foldersAlphabetical") {
      resortPane(side);
    }
    if (oldShowHidden !== next.showHidden && pane.path) {
      const selected = pane.entries[pane.cursor];
      return loadPane(side, pane.path, {
        push: false,
        keepSelName: selected ? selected.name : null,
      });
    }
    render(side);
    onStateChanged();
  }

  function rowHtml(side, pane, entry, index) {
    const safeName = isEntryNameSafe(entry);
    const iconType = entry.is_dir ? "folder" : "file";
    const linkBadge = entry.is_symlink
      ? `<span class="link-badge" aria-hidden="true">↗</span>`
      : "";
    const classes = [entry.is_dir ? "dir" : "file"];
    if (pane.viewOptions.zebraRows && index % 2 === 1) classes.push("stripe");
    if (pane.sel.has(index)) classes.push("selected");
    if (index === pane.cursor) classes.push("cursor");
    if (!safeName) classes.push("unsafe-name");
    // Sicher benannte Einträge sind Kopierquellen für die andere Pane. Echte
    // Ordner tragen zusätzlich den bisherigen Payload für die Favoritenliste.
    const draggable = safeName;
    const unsafeBadge = safeName
      ? ""
      : '<span class="unsafe-name-badge" aria-label="Name nicht bedienbar">!</span>';
    const unsafeTitle = safeName
      ? ""
      : ' title="Dateiname ist nicht eindeutig als UTF-8 darstellbar und bleibt deshalb unverändert"';
    const columns = pane.viewOptions.columns;
    const cells = [];
    // Jede optionale Spalte besitzt im gemeinsamen Raster davor eine schmale
    // Trennerspur. Im Kopf liegt dort der bedienbare Griff; die Zeile braucht an
    // derselben Stelle einen leeren Platzhalter, damit alle Werte bündig bleiben.
    const addCell = html => cells.push('<span class="r-col-separator" aria-hidden="true"></span>', html);
    if (columns.size) {
      addCell(`<span class="r-size">${entry.is_dir
        ? directorySizeHtml(pane, entry)
        : (entry.is_file && entry.size == null ? "—" : fmtEntrySize(entry.size))}</span>`);
    }
    if (columns.kind) addCell(`<span class="r-kind">${escapeHtml(fmtEntryKind(entry))}</span>`);
    if (columns.modified) {
      addCell(`<span class="r-date">${fmtEntryDate(
        entry.mtime,
        pane.dateStages.modified,
        pane.viewOptions.relativeDates,
      )}</span>`);
    }
    if (columns.created) {
      addCell(`<span class="r-created">${fmtEntryDate(
        entry.created,
        pane.dateStages.created,
        pane.viewOptions.relativeDates,
      )}</span>`);
    }
    if (columns.owner) addCell(`<span class="r-owner">${escapeHtml(entry.owner || "")}</span>`);
    if (columns.group) addCell(`<span class="r-group">${escapeHtml(entry.group || "")}</span>`);
    if (columns.permissions) {
      addCell(`<span class="r-permissions">${fmtEntryPermissions(entry.mode)}</span>`);
    }
    return `<div id="${optionId(side, index)}" class="row ${classes.join(" ")}" data-i="${index}"${draggable ? ' draggable="true"' : ""}${unsafeTitle}
      role="option" aria-selected="${pane.sel.has(index)}" aria-posinset="${index + 1}" aria-setsize="${pane.entries.length}">
      <span class="r-icon" aria-hidden="true"><span class="file-icon ${iconType}"></span>${linkBadge}</span>
      <span class="r-name">${escapeHtml(entry.name)}${unsafeBadge}</span>
      ${cells.join("")}
    </div>`;
  }

  function directorySizeDisplayText(value) {
    if (!value || value.status === "loading" || value.status === "cancelled") return "";
    if (value.status === "failed") return "Fehler";
    return `${value.limitations?.length ? "≈ " : ""}${fmtEntrySize(value.bytes)}`;
  }

  function directorySizeHtml(pane, entry) {
    if (!directorySizeEligible(entry)) return "";
    const value = entry.directorySize;
    if (!value) {
      if (pane.provider === "local") return "";
      return `<button class="folder-size-action" type="button" data-directory-size="calculate"
        aria-label="Größe von ${escapeHtml(entry.name)} berechnen">Berechnen</button>`;
    }
    if (value.status === "loading") {
      return `<span class="folder-size-loading" role="status"
        aria-label="Größe von ${escapeHtml(entry.name)} wird berechnet">…</span>`;
    }
    if (value.status === "cancelled") {
      return `<button class="folder-size-action" type="button" data-directory-size="calculate"
        aria-label="Größe von ${escapeHtml(entry.name)} berechnen">Berechnen</button>`;
    }
    const text = directorySizeDisplayText(value);
    const issueText = (value.issues || []).map(issue => issue.message).join(" · ");
    const partial = value.status === "partial";
    const failed = value.status === "failed";
    const resultTitle = failed
      ? issueText || value.error?.message || "Berechnung fehlgeschlagen; klicken für einen neuen Versuch"
      : partial
        ? `${issueText || "Bilanz unvollständig"}; klicken für eine neue Berechnung`
        : `${value.files} Dateien · ${value.directories} Unterordner; klicken für eine neue Berechnung`;
    const title = [resultTitle, ...(value.limitations || [])].join(" · ");
    const label = value.limitations?.length
      ? `Größe von ${entry.name}: ${text} · Best Effort; neu berechnen`
      : `Größe von ${entry.name} neu berechnen`;
    return `<button class="folder-size-value${partial || failed ? " incomplete" : ""}" type="button"
      data-directory-size="recalculate" title="${escapeHtml(title)}"
      aria-label="${escapeHtml(label)}">${escapeHtml(text)}${partial ? " ⚠" : ""}</button>`;
  }

  // Die CSP verbietet Inline-Styles. Ein leeres SVG mit numerischer Höhe hält den
  // nicht gerenderten Scrollbereich deshalb ohne Style-Attribut frei.
  function virtualSpacer(height) {
    if (height <= 0) return "";
    return `<div class="virtual-spacer" aria-hidden="true"><svg width="1" height="${Math.ceil(height)}" aria-hidden="true"></svg></div>`;
  }

  /** Die sichtbaren Zeilen dürfen klein bleiben, der semantisch aktive Eintrag
   * muss für Hilfstechniken aber im DOM bleiben. Diese knappe Ersatzoption liegt
   * nur außerhalb des sichtbaren Ausschnitts und verwendet dieselbe stabile ID. */
  function virtualActiveOptionHtml(side, pane, start, end) {
    if (pane.cursor < 0 || pane.cursor >= pane.entries.length
      || (pane.cursor >= start && pane.cursor < end)) {
      return "";
    }
    const entry = pane.entries[pane.cursor];
    return `<div id="${optionId(side, pane.cursor)}" class="virtual-active-option"
      role="option" aria-selected="${pane.sel.has(pane.cursor)}"
      aria-posinset="${pane.cursor + 1}" aria-setsize="${pane.entries.length}">${escapeHtml(entry.name)}</div>`;
  }

  /**
   * Der Listen-Scrollbalken verschiebt die Zeilen nach links, den Spaltenkopf aber
   * nicht. Seine gemessene Breite wandert deshalb als Variable an die Pane und wird
   * dort als zusätzliche rechte Polsterung des Kopfes wirksam — nur so bleiben
   * Trenner und Zellen bündig.
   */
  function syncListGutter(pane) {
    const gutter = Math.max(0, pane.listing.offsetWidth - pane.listing.clientWidth);
    if (gutter === pane.listGutter) return;
    pane.listGutter = gutter;
    pane.el.style.setProperty("--list-gutter", `${gutter}px`);
  }

  function renderRows(side) {
    const pane = panes[side];
    // Die Datumsstufe folgt der tatsächlich gerenderten Spaltenbreite und wird vor
    // dem Aufbau der Zeilen bestimmt — damit wirkt jedes Ziehen und jeder
    // Fensterresize sofort.
    if (pane.columns) {
      pane.dateStages.modified = pane.columns.dateStage("date");
      pane.dateStages.created = pane.columns.dateStage("created");
    }
    const total = pane.entries.length;
    if (total === 0) {
      pane.renderStart = 0;
      pane.renderEnd = 0;
      pane.listing.dataset.virtualized = "false";
      pane.listing.removeAttribute("aria-activedescendant");
      const emptyLabel = pane.listingOmissions.length ? "Keine anzeigbaren Einträge" : "Leerer Ordner";
      pane.listing.innerHTML = `<div class="empty">${emptyLabel}</div>`;
      syncListGutter(pane);
      return;
    }

    const virtualized = total > VIRTUALIZE_AFTER;
    const rowHeight = paneRowHeight(side);
    const visibleRows = Math.max(1, Math.ceil(pane.listing.clientHeight / rowHeight));
    const firstVisible = Math.max(0, Math.floor(pane.listing.scrollTop / rowHeight));
    const start = virtualized ? Math.max(0, firstVisible - VIRTUAL_OVERSCAN) : 0;
    const end = virtualized
      ? Math.min(total, firstVisible + visibleRows + VIRTUAL_OVERSCAN)
      : total;
    pane.renderStart = start;
    pane.renderEnd = end;
    pane.listing.dataset.virtualized = String(virtualized);
    pane.listing.dataset.renderStart = String(start);
    pane.listing.dataset.renderEnd = String(end);
    const rows = pane.entries
      .slice(start, end)
      .map((entry, offset) => rowHtml(side, pane, entry, start + offset))
      .join("");
    const semanticActive = virtualized
      ? virtualActiveOptionHtml(side, pane, start, end)
      : "";
    pane.listing.innerHTML = virtualSpacer(start * rowHeight)
      + rows
      + virtualSpacer((total - end) * rowHeight)
      + semanticActive;
    if (pane.cursor >= 0 && pane.cursor < total) {
      pane.listing.setAttribute("aria-activedescendant", optionId(side, pane.cursor));
    } else {
      pane.listing.removeAttribute("aria-activedescendant");
    }
    syncListGutter(pane);
  }

  function updatePaneFooter(pane) {
    const dirs = pane.entries.filter(entry => entry.is_dir).length;
    $(".count", pane.el).textContent = `${dirs} Ordner · ${pane.entries.length - dirs} Dateien`
      + (pane.sel.size > 1 ? ` · ${pane.sel.size} ausgewählt` : "");
    const button = $(".listing-omissions-toggle", pane.el);
    const reportField = $(".listing-omissions-text", pane.el);
    const omissions = pane.listingOmissions;
    if (!omissions.length && [button, reportField].includes(document.activeElement)) {
      pane.listing.focus();
    } else if (!pane.listingOmissionsExpanded && document.activeElement === reportField) {
      button.focus();
    }
    button.hidden = !omissions.length;
    button.textContent = `${omissions.length} ausgelassen`;
    button.setAttribute("aria-label", `${omissions.length} ausgelassene Einträge anzeigen`);
    button.setAttribute("aria-expanded", String(pane.listingOmissionsExpanded && omissions.length > 0));
    reportField.hidden = !pane.listingOmissionsExpanded || !omissions.length;
    if (reportField.value !== pane.listingOmissionText) reportField.value = pane.listingOmissionText;
  }

  function render(side) {
    const pane = panes[side];
    $(".path", pane.el).value = pane.path;
    const providerLabel = pane.provider === "ftps" && pane.securityStatus === "plaintext"
      ? "FTP"
      : pane.provider.toUpperCase();
    $(".path", pane.el).title = pane.provider !== "local"
      ? `${providerLabel} · ${pane.profileName || pane.profileId} · Sitzung ${pane.sessionGeneration || "–"}${pane.provider === "sftp" && pane.securityStatus === "sftp_path_best_effort"
        ? " · SFTP-Pane-Listing · pfadbasiertes Best Effort"
        : ""}`
      : pane.path;
    renderRows(side);
    $$(".list-header .col[data-sort]", pane.el).forEach((column) => {
      const selected = column.dataset.sort === pane.sort.key;
      column.classList.toggle("sorted", selected);
      column.classList.toggle("desc", selected && pane.sort.dir === -1);
      const label = column.dataset.sortLabel || column.textContent.trim();
      column.dataset.sortLabel = label;
      column.setAttribute(
        "aria-label",
        selected
          ? `${label}, ${pane.sort.dir === 1 ? "aufsteigend" : "absteigend"} sortiert`
          : `${label}, sortieren`,
      );
    });
    updatePaneFooter(pane);
  }

  function refreshSelection(side) {
    const pane = panes[side];
    const activeId = optionId(side, pane.cursor);
    const staleProxy = $(".virtual-active-option", pane.listing);
    if (staleProxy && staleProxy.id !== activeId) {
      // Ein Mausklick kann den Cursor auf eine bereits sichtbare Zeile setzen,
      // ohne den Scrollausschnitt zu verändern. Dann muss die Ersatzoption des
      // alten, wegvirtualisierten Cursors vor der ARIA-Aktualisierung verschwinden.
      renderRows(side);
    }
    $$(".row", pane.listing).forEach((row) => {
      const index = Number(row.dataset.i);
      row.classList.toggle("selected", pane.sel.has(index));
      row.classList.toggle("cursor", index === pane.cursor);
      row.setAttribute("aria-selected", String(pane.sel.has(index)));
    });
    const activeOption = pane.entries.length ? document.getElementById(activeId) : null;
    if (activeOption) {
      // Bei virtualisierten Listen kann der semantische aktive Eintrag ein eigener
      // unsichtbarer Optionsknoten sein. Seine Auswahl ändert sich trotzdem mit
      // Escape beziehungsweise Cmd/Ctrl+A und muss für Screenreader aktuell bleiben.
      activeOption.setAttribute("aria-selected", String(pane.sel.has(pane.cursor)));
      pane.listing.setAttribute("aria-activedescendant", activeId);
    } else {
      pane.listing.removeAttribute("aria-activedescendant");
    }
    updatePaneFooter(pane);
    onStateChanged();
  }

  function scrollCursorIntoView(side) {
    const pane = panes[side];
    if (pane.entries.length > VIRTUALIZE_AFTER) {
      const rowHeight = paneRowHeight(side);
      const itemTop = pane.cursor * rowHeight;
      const itemBottom = itemTop + rowHeight;
      const viewportTop = pane.listing.scrollTop;
      const viewportBottom = viewportTop + pane.listing.clientHeight;
      if (itemTop < viewportTop) pane.listing.scrollTop = itemTop;
      else if (itemBottom > viewportBottom) {
        pane.listing.scrollTop = Math.max(0, itemBottom - pane.listing.clientHeight);
      }
      renderRows(side);
      return;
    }
    const row = $(`.row[data-i="${pane.cursor}"]`, pane.listing);
    if (row) row.scrollIntoView({ block: "nearest" });
  }

  function rangeSet(a, b) {
    const set = new Set();
    const [low, high] = a <= b ? [a, b] : [b, a];
    for (let index = low; index <= high; index += 1) set.add(index);
    return set;
  }

  function moveCursorTo(side, index, extend) {
    const pane = panes[side];
    if (!pane.entries.length) return;
    index = Math.max(0, Math.min(index, pane.entries.length - 1));
    pane.cursor = index;
    if (extend) pane.sel = rangeSet(pane.anchor, index);
    else {
      pane.anchor = index;
      pane.sel = new Set([index]);
    }
    scrollCursorIntoView(side);
    refreshSelection(side);
  }

  function moveCursor(side, delta, extend) {
    moveCursorTo(side, panes[side].cursor + delta, extend);
  }

  function pageSize(side) {
    const pane = panes[side];
    return Math.max(1, Math.floor(pane.listing.clientHeight / paneRowHeight(side)) - 1);
  }

  function selectAll(side) {
    const pane = panes[side];
    pane.sel = new Set(pane.entries.map((_, index) => index));
    refreshSelection(side);
  }

  function clearSelection(side) {
    panes[side].sel = new Set();
    refreshSelection(side);
  }

  async function openCursor(side) {
    const pane = panes[side];
    const entry = pane.entries[pane.cursor];
    if (!entry) return;
    if (!isEntryNameSafe(entry)) {
      return showDock('<span class="pill delete">Unsicherer Dateiname</span>Der angezeigte Name ist keine eindeutige UTF-8-Abbildung. Öffnen und Dateiaktionen bleiben gesperrt.', { closable: true });
    }
    const path = joinPath(pane.path, entry.name);
    if (entry.is_dir) return loadPane(side, path);
    if (!entry.is_file || entry.is_symlink) {
      return showDock(`<span class="pill none">Nicht geöffnet</span>Nur reguläre Dateien ohne Symlink werden an die System-App übergeben.`, { closable: true });
    }
    try {
      if (pane.provider === "local") {
        await invoke("open_path", { path });
        showDock(`<span class="pill ok">Geöffnet</span>${escapeHtml(entry.name)} wurde an die System-App übergeben.`, { closable: true });
      } else {
        await invoke("open_remote_snapshot", {
          request: {
            endpoint: {
              provider: pane.provider,
              profileId: pane.profileId,
              profileRevision: pane.profileRevision,
              root: pane.path,
            },
            path,
          },
        });

      }
    } catch (error) {
      showError("Öffnen fehlgeschlagen", error);
    }
  }

  function goUp(side) {
    const pane = panes[side];
    if (pane.parent) {
      const cameFrom = pane.path.split("/").pop();
      loadPane(side, pane.parent, { keepSelName: cameFrom });
    }
  }

  function goHome(side) {
    const pane = panes[side];
    pane.navigationRevision += 1;
    if (pane.provider !== "local") return loadPane(side, "/");
    // Schon der Default-Abruf gehört zu dieser Navigation. Ohne das frühe Token
    // könnte ein dazwischen gestarteter manueller Pfadwechsel nach der langsamen
    // Antwort wieder vom Home-Pfad überschrieben werden.
    const request = ++pane.loadSeq;
    pane.navigationPending = true;
    invoke("default_dirs")
      .then(defaults => {
        if (request === pane.loadSeq) return loadPane(side, defaults[side]);
      })
      .catch(error => {
        if (request === pane.loadSeq) showError("Fehler", error);
      })
      .finally(() => {
        if (request === pane.loadSeq) pane.navigationPending = false;
      });
  }

  function refresh(side) {
    const pane = panes[side];
    const selected = pane.entries[pane.cursor];
    loadPane(side, pane.path, {
      push: false,
      keepSelName: selected ? selected.name : null,
    });
  }

  function back(side) {
    const pane = panes[side];
    if (pane.hpos > 0) {
      const historyPosition = pane.hpos - 1;
      loadPane(side, pane.history[historyPosition], { push: false, historyPosition });
    }
  }

  function forward(side) {
    const pane = panes[side];
    if (pane.hpos < pane.history.length - 1) {
      const historyPosition = pane.hpos + 1;
      loadPane(side, pane.history[historyPosition], { push: false, historyPosition });
    }
  }

  function focusPath(side) {
    const input = $(".path", panes[side].el);
    input.focus();
    input.select();
  }

  function swapPanes() {
    const left = {
      provider: panes.left.provider,
      profileId: panes.left.profileId,
      profileRevision: panes.left.profileRevision,
      profileName: panes.left.profileName,
      securityStatus: panes.left.securityStatus,
      quarantineCapabilities: panes.left.quarantineCapabilities,
      securityNoticeKey: panes.left.securityNoticeKey,
      path: panes.left.path,
    };
    const right = {
      provider: panes.right.provider,
      profileId: panes.right.profileId,
      profileRevision: panes.right.profileRevision,
      profileName: panes.right.profileName,
      securityStatus: panes.right.securityStatus,
      quarantineCapabilities: panes.right.quarantineCapabilities,
      securityNoticeKey: panes.right.securityNoticeKey,
      path: panes.right.path,
    };
    setPaneLocation("left", right);
    setPaneLocation("right", left);
  }

  function setPaneLocation(side, location) {
    const pane = panes[side];
    invalidateQuarantineBadge();
    // Zielpfad VOR dem Leeren bestimmen: der lokale Fallback nutzt den bisherigen Pfad.
    const provider = location.provider || "local";
    if (provider !== "local"
        && (!Number.isSafeInteger(location.profileRevision) || location.profileRevision < 0)) {
      showError("Remote-Pane", {
        code: "plan_drift",
        message: "Das gewählte Profil besitzt keine aktuelle Revision.",
      });
      return Promise.resolve(false);
    }
    const targetPath = location.path || (provider !== "local" ? "/" : pane.path);
    pane.navigationRevision += 1;
    pane.provider = provider;
    pane.profileId = location.profileId || null;
    pane.profileRevision = provider === "local" ? null : location.profileRevision;
    pane.profileName = location.profileName || null;
    pane.securityStatus = location.securityStatus || null;
    // Bis der Backendstatus nach dem ersten Listing gebunden ist, bleibt jeder Remote-Provider
    // fail-closed. So kann ein Klick im schmalen Endpunktwechsel-Fenster keine
    // Quarantäneoperation mit einer alten Capability starten.
    pane.quarantineCapabilities = location.quarantineCapabilities
      || (provider !== "local" ? { listing: false, restore: false, purge: false } : null);
    pane.securityNoticeKey = location.securityNoticeKey || null;
    pane.history = [];
    pane.hpos = -1;
    clearPaneContents(pane);
    pane.viewOptions = loadViewOptions(paneLocation(pane, targetPath));
    ensureVisibleSort(pane);
    if (pane.el) {
      applyViewOptions(side, { renderNow: false });
      render(side);
    }
    renderConnections();
    return loadPane(side, targetPath);
  }

  /** Macht eine Pane sofort wirkungslos, noch bevor ein langsamer lokaler
   * Standardpfad geladen wird. Alte Remote-Zeilen und laufende Listings besitzen
   * danach weder einen Endpunkt noch ein gültiges Requesttoken. */
  function disablePane(side) {
    const pane = panes[side];
    pane.loadSeq += 1;
    pane.navigationPending = false;
    pane.directorySizeToken += 1;
    pane.directorySizeRunId = null;
    pane.provider = "local";
    pane.profileId = null;
    pane.profileRevision = null;
    pane.profileName = null;
    pane.securityStatus = null;
    pane.quarantineCapabilities = null;
    pane.securityNoticeKey = null;
    clearPaneContents(pane);
    pane.history = [];
    pane.hpos = -1;
    invalidateQuarantineBadge();
    if (pane.el) render(side);
  }

  /** Ein reines Umbenennen ändert weder Endpunkt noch laufende Dateiliste. Der
   * sichtbare Pane-Titel kann deshalb ohne erneutes Remote-Listing nachziehen. */
  function updateProfileName(provider, profileId, profileName) {
    for (const side of ["left", "right"]) {
      const pane = panes[side];
      if (pane.provider === provider && pane.profileId === profileId) {
        pane.profileName = profileName;
        render(side);
      }
      renderTabs(side);
    }
  }

  function setSort(side, key) {
    const pane = panes[side];
    if (!pane || !SORT_KEYS.has(key) || !sortColumnVisible(pane, key)) return;
    if (pane.sort.key === key) pane.sort.dir = -pane.sort.dir;
    else pane.sort = { key, dir: 1 };
    resortPane(side);
    render(side);
    onStateChanged();
  }

  function toggleHidden(side) {
    const pane = panes[side];
    updateViewOptions(
      side,
      { ...pane.viewOptions, showHidden: !pane.viewOptions.showHidden },
      "showHidden",
    );
  }

  function typeAhead(side, char) {
    const pane = panes[side];
    if (!pane.entries.length) return;
    const now = performance.now();
    if (now - pane.typeTime > TYPE_RESET_MS) pane.typeBuffer = "";
    pane.typeTime = now;
    const next = char.toLowerCase();
    const repeated = pane.typeBuffer.length >= 1
      && [...pane.typeBuffer].every(value => value === pane.typeBuffer[0]);
    const cycling = repeated && next === pane.typeBuffer[0];
    let prefix;
    let start;
    if (cycling) {
      pane.typeBuffer += next;
      prefix = pane.typeBuffer[0];
      start = pane.cursor + 1;
    } else {
      pane.typeBuffer += next;
      prefix = pane.typeBuffer;
      start = pane.cursor < 0 ? 0 : pane.cursor;
    }
    for (let offset = 0; offset < pane.entries.length; offset += 1) {
      const index = (start + offset) % pane.entries.length;
      if (pane.entries[index].name.toLowerCase().startsWith(prefix)) {
        moveCursorTo(side, index, false);
        return;
      }
    }
  }

  function wirePane(side) {
    const pane = panes[side];
    pane.el = document.querySelector(`.pane[data-side="${side}"]`);
    pane.listing = $(".listing", pane.el);
    pane.tabsView = createPaneTabsView({ side, paneEl: pane.el,
      select: id => { activate(side); return selectTab(side, id); },
      close: id => { activate(side); return closeTab(side, id); },
      add: () => { activate(side); return addTab(side); }, locked: running,
      profileName: location => resolveProfile(location.provider, location.profileId)?.profileName,
    });
    // Spaltentrenner samt gemerkten Breiten. Jede Änderung zeichnet die Zeilen neu,
    // weil das Datumsformat von der verfügbaren Breite abhängt.
    pane.columns = initPaneColumns(side, pane.el, {
      onChange: () => renderRows(side),
      fitWidth: key => fitColumnWidth(side, key),
    });
    pane.viewPopover = initViewOptionsPopover({
      side,
      paneEl: pane.el,
      getLocation: () => paneLocation(pane),
      getOptions: () => pane.viewOptions,
      onOptionsChange: (next, key) => updateViewOptions(side, next, key),
      onReset: (next) => updateViewOptions(side, next, "reset", { persist: false }),
      onByteUnitsChange: changeByteUnits,
      onLocalFolderSizesChange: changeLocalFolderSizes,
    });
    applyViewOptions(side, { renderNow: false });

    const omissionsReport = $(".listing-omissions-text", pane.el);
    omissionsReport.id = `${side}-listing-omissions`;
    $(".listing-omissions-toggle", pane.el).setAttribute("aria-controls", omissionsReport.id);
    $(".listing-omissions-toggle", pane.el).addEventListener("click", event => {
      pane.listingOmissionsExpanded = !pane.listingOmissionsExpanded;
      updatePaneFooter(pane);
      // Die Pane-Aktivierung setzt zuvor Listenfokus; der betätigte Knopf
      // benötigt ihn auch nach nativer AX-/Mausbedienung ausdrücklich zurück.
      event.currentTarget.focus();
    });
    pane.el.addEventListener("mousedown", () => activate(side));
    pane.el.addEventListener("focusin", () => activate(side, { focus: false }));
    pane.listing.addEventListener("click", (event) => {
      const sizeAction = event.target.closest("[data-directory-size]");
      if (sizeAction) {
        const row = sizeAction.closest(".row");
        const entry = row ? pane.entries[Number(row.dataset.i)] : null;
        if (entry && directorySizeEligible(entry)) {
          void startDirectorySizes(side, [entry.name], {
            recalculate: sizeAction.dataset.directorySize === "recalculate",
          });
        }
        event.preventDefault();
        event.stopPropagation();
        return;
      }
      const row = event.target.closest(".row");
      if (!row) {
        clearSelection(side);
        return;
      }
      const index = Number(row.dataset.i);
      if (event.shiftKey) {
        pane.cursor = index;
        pane.sel = rangeSet(pane.anchor, index);
      } else if (event.metaKey || event.ctrlKey) {
        if (pane.sel.has(index)) pane.sel.delete(index);
        else pane.sel.add(index);
        pane.cursor = index;
        pane.anchor = index;
      } else {
        pane.cursor = index;
        pane.anchor = index;
        pane.sel = new Set([index]);
      }
      refreshSelection(side);
    });

    pane.listing.addEventListener("dblclick", (event) => {
      if (event.target.closest("[data-directory-size]")) {
        event.preventDefault();
        return;
      }
      const row = event.target.closest(".row");
      if (!row) return;
      const index = Number(row.dataset.i);
      pane.cursor = index;
      pane.anchor = index;
      pane.sel = new Set([index]);
      refreshSelection(side);
      openCursor(side);
    });

    // Rechtsklick auf eine fremde Zeile wählt sie zuerst aus. Auf leerer Fläche
    // bleibt nur die Ordneraktion verfügbar, wie bei nativen Dateimanagern.
    pane.listing.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      activate(side);
      const row = event.target.closest(".row");
      if (row) {
        const index = Number(row.dataset.i);
        if (!pane.sel.has(index)) {
          pane.cursor = index;
          pane.anchor = index;
          pane.sel = new Set([index]);
        } else {
          pane.cursor = index;
        }
        refreshSelection(side);
      } else {
        clearSelection(side);
      }
      showContextMenu(side, event.clientX, event.clientY);
    });

    // Ordnerzeilen als Drag-Quelle für die Favoriten-Seitenleiste. Der Payload trägt
    // Endpoint, Anzeigename und absoluten Providerpfad; die Sicherheitsprüfung bleibt
    // beim Backend. Profil-IDs sind Referenzen und enthalten keine Zugangsdaten.
    pane.listing.addEventListener("dragstart", (event) => {
      const row = event.target.closest(".row");
      const entry = row ? pane.entries[Number(row.dataset.i)] : null;
      if (!entry || !isEntryNameSafe(entry)) {
        event.preventDefault();
        return;
      }
      if (entry.is_dir && !entry.is_symlink) event.dataTransfer.setData(FOLDER_DRAG_TYPE, JSON.stringify({
        path: joinPath(pane.path, entry.name),
        name: entry.name,
        provider: pane.provider,
        profileId: pane.profileId,
      }));
      event.dataTransfer.effectAllowed = "copy";
    });

    pane.listing.addEventListener("scroll", () => {
      if (pane.entries.length > VIRTUALIZE_AFTER) renderRows(side);
      hideContextMenu();
      onStateChanged();
    });
    $(".up", pane.el).addEventListener("click", () => goUp(side));
    $(".nav-back", pane.el).addEventListener("click", () => back(side));
    $(".nav-fwd", pane.el).addEventListener("click", () => forward(side));
    $(".path", pane.el).addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        loadPane(side, event.target.value.trim());
        pane.listing.focus();
      } else if (event.key === "Escape") {
        event.target.value = pane.path;
        event.target.blur();
      }
    });
    $$(".list-header .col[data-sort]", pane.el).forEach((column) => {
      column.addEventListener("click", () => setSort(side, column.dataset.sort));
      column.addEventListener("keydown", (event) => {
        if (event.key !== "Enter" && event.key !== " ") return;
        setSort(side, column.dataset.sort);
        event.preventDefault();
      });
    });
  }

  return {
    addTab,
    closeTab,
    cycleTab,
    invalidateProfileTabs,
    restoreTabs,
    tabsSnapshot,
    back,
    clearSelection,
    changeLocalFolderSizes,
    disablePane,
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
    optionId,
    pageSize,
    paneRowHeight,
    panes,
    rangeSet,
    refresh,
    refreshColumnDefaults,
    refreshSelection,
    startDirectorySizes,
    renderRows,
    selectAll,
    setPaneLocation,
    setSort,
    swapPanes,
    toggleHidden,
    typeAhead,
    updateProfileName,
    openViewOptions: side => panes[side]?.viewPopover?.open(),
    wirePane,
  };
}
