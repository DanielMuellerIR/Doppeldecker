import { sanitizeViewOptions } from "./view-options-state.js";

// Navigationszustand je Tab. Provider-Handles, Queue und Permits bleiben im Backend.
function copyLocation(location) {
  return {
    provider: location.provider,
    profileId: location.profileId ?? null,
    profileRevision: location.profileRevision ?? null,
    profileName: null,
    path: location.path,
  };
}

function navigationSnapshot(tab) {
  return {
    id: tab.id,
    location: copyLocation(tab.location),
    history: [...tab.history],
    historyIndex: tab.historyIndex,
    selectedNames: [...tab.selectedNames],
    cursorName: tab.cursorName,
    anchorName: tab.anchorName,
    scrollTop: tab.scrollTop,
    sort: { key: tab.sort.key, dir: tab.sort.dir },
    viewOptions: sanitizeViewOptions(tab.viewOptions),
  };
}

function runtimeTab(saved) {
  return {
    ...navigationSnapshot(saved),
    runtime: { generation: 0, reads: new Map(), entries: [], status: "unloaded" },
  };
}

function freshTab(location, viewOptions, id) {
  return runtimeTab({
    id,
    location,
    history: [location.path],
    historyIndex: 0,
    selectedNames: [],
    cursorName: null,
    anchorName: null,
    scrollTop: 0,
    sort: { key: "name", dir: 1 },
    viewOptions,
  });
}

/** Ein Modell gehört genau einer Pane eines Fensters. Auch identische Tab-IDs in
 * anderen Modellen können wegen der Token-Objektbindung keine Antwort übernehmen. */
export function createPaneTabs(location, viewOptions, {
  saved = null,
  idFactory = () => crypto.randomUUID(),
} = {}) {
  let tabs = saved ? saved.tabs.map(runtimeTab) : [freshTab(location, viewOptions, idFactory())];
  let activeId = saved ? saved.activeId : tabs[0].id;
  if (!tabs.length || tabs.length > 32 || new Set(tabs.map(tab => tab.id)).size !== tabs.length
      || !tabs.some(tab => tab.id === activeId)) {
    throw new Error("Ungültiger Tab-Zustand");
  }
  const find = id => {
    const tab = tabs.find(item => item.id === id);
    if (!tab) throw new Error("Tab nicht vorhanden");
    return tab;
  };
  function invalidateReads(tab) {
    tab.runtime.generation += 1;
    tab.runtime.reads.clear();
    if (tab.runtime.status === "loading") tab.runtime.status = "unloaded";
  }
  return {
    get active() { return find(activeId); },
    get tabs() { return [...tabs]; },
    add(nextLocation, nextOptions = find(activeId).viewOptions) {
      if (tabs.length === 32) throw new Error("Höchstens 32 Tabs je Pane");
      const id = idFactory();
      if (tabs.some(tab => tab.id === id)) throw new Error("Doppelte Tab-ID");
      const tab = freshTab(nextLocation, nextOptions, id);
      tabs.push(tab);
      return tab;
    },
    activate(id) {
      const next = find(id);
      if (activeId !== id) {
        invalidateReads(find(activeId));
        activeId = id;
      }
      return next;
    },
    close(id) {
      const tab = find(id);
      if (tabs.length === 1) throw new Error("Der letzte Tab einer Pane bleibt offen");
      const index = tabs.indexOf(tab);
      invalidateReads(tab);
      tabs = tabs.filter(item => item !== tab);
      if (activeId === id) activeId = tabs[Math.min(index, tabs.length - 1)].id;
      return find(activeId);
    },
    beginRead(id, kind) {
      const tab = find(id);
      if (id !== activeId) throw new Error("Nur der aktive Tab startet Lesevorgänge");
      const token = Object.freeze({ tabId: id, kind, generation: ++tab.runtime.generation });
      tab.runtime.reads.set(kind, token);
      tab.runtime.status = "loading";
      return token;
    },
    finishRead(token, apply) {
      const tab = tabs.find(item => item.id === token.tabId);
      if (!tab || tab.id !== activeId || tab.runtime.reads.get(token.kind) !== token) return false;
      tab.runtime.reads.delete(token.kind);
      tab.runtime.status = tab.runtime.reads.size ? "loading" : "ready";
      apply(tab);
      return true;
    },
    invalidateProfile(provider, profileId) {
      for (const tab of tabs) {
        if (tab.location.provider === provider && tab.location.profileId === profileId) {
          invalidateReads(tab);
          tab.runtime.entries = [];
          tab.runtime.status = "unloaded";
        }
      }
    },
    snapshot() {
      return { activeId, tabs: tabs.map(navigationSnapshot) };
    },
  };
}
