import { invoke } from "./tauri-api.js";
import { cancelOwnedModal, showOwnedModal } from "./modal.js";
import { renderPassivePreviewContent } from "./file-preview.js";
import { $, errText, fmtEntrySize, isEntryNameSafe, joinPath } from "./ui-utils.js";

let dialogSequence = 0;
const scopeKeys = ["provider", "profileId", "profileRevision", "sessionGeneration", "path", "navigationRevision"];
const terminalCodes = new Set(["expired", "plan_drift", "not_found"]);
const element = (tag, className, text) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
};
const button = (text, action, className = "btn ghost") => {
  const node = element("button", className, text);
  node.type = "button";
  node.addEventListener("click", action);
  return node;
};

export function createArchiveReadUi({ panes, selectedEntries, showDock, blocked = () => false, extractSelectionReady = null }) {
  let current = null;
  const live = context => current === context && !context.closed;
  const scopeMatches = context => {
    const pane = panes[context.side];
    const selected = selectedEntries(context.side);
    return pane && scopeKeys.every(key => pane[key] === context.scope[key])
      && pane.tabs?.active?.id === context.tabId
      && selected.length === 1 && selected[0].name === context.name;
  };
  const cancelRequest = requestId => {
    if (requestId) void invoke("cancel_archive_read", { requestId }).catch(() => {});
  };
  const closeBackend = (context, sessionId = context.sessionId) => {
    void invoke("close_archive_read", { openRequestId: context.openRequestId, sessionId: sessionId || null }).catch(() => {});
  };
  function cancelPreview(context) {
    context.previewRevision += 1;
    if (context.read && !context.read.cancelled) {
      context.read.cancelled = true;
      cancelRequest(context.read.requestId);
    }
  }
  const discardExtract = id => { if (id) void invoke("discard_archive_extract", { preparationId: id }).catch(() => {}); };
  function cancelExtract(context) {
    if (context.extract) discardExtract(context.extract.requestId);
  }
  function close(context) {
    if (!live(context)) return;
    context.closed = true;
    clearInterval(context.scopeTimer);
    context.body?.removeEventListener("keydown", context.keyHandler);
    cancelRequest(context.openRequestId);
    cancelPreview(context);
    cancelExtract(context);
    closeBackend(context);
    cancelOwnedModal(context.modalToken);
    current = null;
  }
  function invalidate(context, message) {
    if (!live(context) || context.stale) return;
    context.stale = true;
    clearInterval(context.scopeTimer);
    cancelRequest(context.openRequestId);
    cancelPreview(context);
    cancelExtract(context);
    closeBackend(context);
    context.error = message;
    context.rows = [];
    context.selected = null;
    context.list.replaceChildren();
    context.list.removeAttribute("aria-activedescendant");
    context.preview.replaceChildren(element("p", "archive-read-note", message));
    context.breadcrumbs.replaceChildren();
    context.closeButton.focus({ preventScroll: true });
    renderState(context);
  }
  function ensureScope(context) {
    if (!live(context) || context.stale) return false;
    if (scopeMatches(context)) return true;
    invalidate(context, "Die Auswahl oder die Ausgangspane hat sich geändert. Bitte schließen und das Archiv erneut öffnen.");
    return false;
  }
  function renderState(context) {
    if (!live(context)) return;
    const busy = context.opening || context.listing || Boolean(context.extract);
    context.list.setAttribute("aria-busy", String(busy));
    context.preview.setAttribute("aria-busy", String(Boolean(context.read)));
    context.previous.disabled = busy || context.stale || !context.listingData || context.listingData.page === 0;
    context.next.disabled = busy || context.stale || !context.listingData
      || (context.listingData.page + 1) * context.listingData.pageSize >= context.listingData.totalRows;
    context.previewButton.disabled = busy || context.stale || Boolean(context.read) || !context.selected?.canPreview;
    for (const node of context.breadcrumbs.querySelectorAll("button")) node.disabled = busy || context.stale || node.dataset.current === "true";
    if (context.wholeExtractButton) context.wholeExtractButton.disabled = busy || context.stale || !context.canExtract;
    if (context.fileExtractButton) context.fileExtractButton.disabled = busy || context.stale || !context.canExtractFile;
    if (context.folderExtractButton) {
      context.folderExtractButton.hidden = !(context.listingData?.breadcrumbs.length > 1);
      context.folderExtractButton.disabled = busy || context.stale || !context.canExtract;
    }
    context.status.textContent = context.error || (context.extract ? "Archivauswahl und Ziel werden geprüft … Schließen bricht die Vorbereitung ab."
      : context.opening ? "Archiv wird vorbereitet und geprüft; entfernte Dateien werden vollständig als Momentaufnahme geladen … Schließen oder Escape bricht ab."
      : context.listing ? "Ordner wird angezeigt …"
        : context.read ? context.read.cancelled ? "Vorige Vorschau endet … Eine neue Vorschau startet erst danach." : "Ausgewählter Inhalt wird vollständig geprüft …"
          : context.listingData ? "Schreibgeschützte Archivansicht · Vorschau mit F3 oder dem Vorschauknopf." : "Archiv konnte nicht geöffnet werden.");
    const modal = context.status.closest(".modal");
    if (!modal.contains(document.activeElement) || document.activeElement.matches?.(":disabled")) {
      (context.rows.length && !context.stale ? context.list : context.closeButton).focus({ preventScroll: true });
    }
  }
  function appendFileAction(context) {
    context.fileExtractButton = null;
    if (extractSelectionReady && context.selected?.kind === "file" && context.selected.entryId) {
      context.fileExtractButton = button("Datei entpacken …", () => prepareSelection(context, "entry"));
      context.fileExtractButton.classList.add("archive-read-extract-action");
      context.preview.insertBefore(context.fileExtractButton, context.preview.children[1] || null);
    }
  }
  async function prepareSelection(context, kind) {
    if (!ensureScope(context) || !(kind === "entry" ? context.canExtractFile : context.canExtract) || context.opening || context.listing || context.extract) return;
    const targetSide = context.side === "left" ? "right" : "left";
    const target = panes[targetSide];
    const targetScope = Object.fromEntries(scopeKeys.map(key => [key, target[key]]));
    const tabId = target.tabs?.active?.id;
    const targetMatches = () => panes[targetSide].tabs?.active?.id === tabId && scopeKeys.every(key => panes[targetSide][key] === targetScope[key]);
    const work = { requestId: crypto.randomUUID(), targetMatches };
    const entryId = kind === "entry" ? context.selected?.entryId : null;
    const directoryId = kind === "all" ? context.listingData?.breadcrumbs[0]?.directoryId
      : kind === "subtree" ? context.listingData?.directoryId : null;
    if (!entryId && !directoryId) { context.error = "Die Archivauswahl ist nicht mehr verfügbar. Bitte erneut öffnen."; renderState(context); return; }
    context.extract = work;
    context.error = null;
    cancelPreview(context);
    context.closeButton.focus({ preventScroll: true });
    renderState(context);
    try {
      if (context.read) await context.read.settled;
      const validTarget = () => !work.invalid && targetMatches();
      if (!ensureScope(context) || context.extract !== work || !validTarget()) { discardExtract(work.requestId); return; }
      const response = await invoke("prepare_archive_extract_selection", { request: { requestId: work.requestId,
        sessionId: context.sessionId, ...(entryId ? { entryId } : { directoryId }),
        targetProvider: targetScope.provider, targetDir: targetScope.path,
        ...(targetScope.provider !== "local" ? { target: { provider: targetScope.provider, root: targetScope.path,
          profileId: targetScope.profileId, profileRevision: targetScope.profileRevision } } : {}) } });
      if (!ensureScope(context) || context.extract !== work || !validTarget()) { discardExtract(response.preparationId); return; }
      if (response.requestId !== work.requestId) { discardExtract(response.preparationId); throw new Error("Die Entpackantwort gehört zu einer anderen Anfrage."); }
      context.extract = null;
      close(context);
      extractSelectionReady(context.side, response);
    } catch (error) {
      if (ensureScope(context)) context.error = errText(error);
    } finally {
      if (context.extract === work) context.extract = null;
      renderState(context);
    }
  }
  function showSelection(context) {
    const row = context.selected;
    context.fileExtractButton = null;
    context.preview.replaceChildren();
    context.preview.append(element("h3", "archive-read-preview-title", row?.displayName || "Vorschau"));
    if (!row) {
      context.preview.append(element("p", "archive-read-note", "Eine Datei auswählen und mit F3 oder dem Vorschauknopf ansehen. Ordner mit Enter öffnen."));
      return;
    }
    if (row.uncompressedBytes != null) context.preview.append(element("p", "archive-read-note", fmtEntrySize(row.uncompressedBytes)));
    if (row.issues?.length) {
      const issues = element("ul", "archive-read-problems");
      for (const issue of row.issues) issues.append(element("li", "", issue.message));
      context.preview.append(issues);
    }
    context.preview.append(element("p", "archive-read-note", row.directoryId
      ? "Virtueller Ordner · mit Enter öffnen."
      : row.canPreview ? "Vorschau liest nur diese Auswahl. UTF-8-Text bis 1 MiB, PNG/JPEG bis 8 MiB und 24 Millionen Pixel."
        : "Für diesen Eintrag ist keine passive Vorschau verfügbar."));
    appendFileAction(context);
  }
  function selectRow(context, row) {
    if (!ensureScope(context) || context.listing || context.extract) return;
    if (context.selected?.rowId !== row.rowId) {
      cancelPreview(context);
      context.selected = row;
      context.error = null;
      showSelection(context);
    }
    for (const option of context.list.children) option.setAttribute("aria-selected", String(option.dataset.rowId === row.rowId));
    const index = context.rows.indexOf(row);
    const option = context.list.children[index];
    if (option) {
      context.list.setAttribute("aria-activedescendant", option.id);
      option.scrollIntoView({ block: "nearest" });
    }
    context.list.focus({ preventScroll: true });
    renderState(context);
  }
  function protocolError(context) {
    invalidate(context, "Die Archivantwort passt nicht zur aktuellen Sitzung. Bitte das Archiv erneut öffnen.");
  }
  function acceptListing(context, listing, expectedDirectory, expectedPage) {
    if (listing.sessionId !== context.sessionId || (expectedDirectory && listing.directoryId !== expectedDirectory)
      || listing.page !== expectedPage || listing.rows.length > 100) { protocolError(context); return; }
    context.listingData = listing;
    context.rows = listing.rows;
    context.selected = null;
    context.error = null;
    const catalog = listing.catalog;
    const omitted = catalog.entryCount - catalog.displayedEntryCount;
    context.summary.textContent = `${catalog.entryCount.toLocaleString("de-DE")} Archiveinträge`
      + (omitted > 0 ? ` · ${catalog.displayedEntryCount.toLocaleString("de-DE")} dargestellt · ${omitted.toLocaleString("de-DE")} nicht dargestellt` : "")
      + ` · ${catalog.problemCount.toLocaleString("de-DE")} mit Hinweisen`;
    context.partial.hidden = catalog.complete;
    context.partial.textContent = catalog.complete ? "" : `Teilkatalog: ${catalog.stopReason?.message || "Der Katalog konnte nicht vollständig gelesen werden."} Angezeigt wird nur der bekannte Teil.`;
    context.breadcrumbs.replaceChildren();
    for (let index = 0; index < listing.breadcrumbs.length; index += 1) {
      const crumb = listing.breadcrumbs[index];
      const control = button(crumb.label, () => navigate(context, crumb.directoryId, 0), "btn ghost small");
      control.dataset.current = String(index + 1 === listing.breadcrumbs.length);
      if (control.dataset.current === "true") control.setAttribute("aria-current", "location");
      context.breadcrumbs.append(control);
    }
    context.list.replaceChildren();
    context.list.removeAttribute("aria-activedescendant");
    for (const [index, row] of listing.rows.entries()) {
      const option = element("div", "archive-read-row");
      option.id = `archive-read-${context.uiId}-${index}`;
      option.dataset.rowId = row.rowId;
      option.setAttribute("role", "option");
      option.setAttribute("aria-selected", "false");
      const position = listing.page * listing.pageSize + index + 1;
      const kindLabel = row.kind === "directory" ? "Ordner" : row.kind === "problem" ? "Problem" : "Datei";
      option.setAttribute("aria-label", `${kindLabel} · ${row.displayName}${row.duplicateName ? ` · Eintrag ${position}` : ""}`);
      option.setAttribute("aria-posinset", String(position));
      option.setAttribute("aria-setsize", String(listing.totalRows));
      const identity = element("div", "archive-read-identity");
      identity.append(element("span", "archive-read-kind", row.kind === "directory" ? "Ordner" : row.kind === "problem" ? "Problem" : "Datei"));
      identity.append(element("span", "archive-read-name", row.displayName));
      if (row.duplicateName) identity.append(element("span", "archive-read-tag", "Gleicher Name"));
      if (row.issues?.length) identity.append(element("span", "archive-read-tag archive-read-issue", `${row.issues.length} Hinweis(e)`));
      option.append(identity, element("span", "archive-read-size", row.uncompressedBytes == null ? "" : fmtEntrySize(row.uncompressedBytes)));
      option.addEventListener("click", () => selectRow(context, row));
      option.addEventListener("dblclick", () => { selectRow(context, row); if (row.directoryId) navigate(context, row.directoryId, 0); });
      context.list.append(option);
    }
    if (!listing.rows.length) context.list.append(element("p", "archive-read-empty", "Dieser virtuelle Ordner ist leer."));
    const pages = Math.max(1, Math.ceil(listing.totalRows / listing.pageSize));
    context.pageLabel.textContent = `Seite ${listing.page + 1} von ${pages} · ${listing.totalRows.toLocaleString("de-DE")} Zeilen`;
    showSelection(context);
  }
  async function navigate(context, directoryId, page) {
    if (!ensureScope(context) || context.opening || context.listing || context.extract) return;
    cancelPreview(context);
    context.selected = null;
    showSelection(context);
    context.listing = true;
    const revision = ++context.navigationRevision;
    context.list.focus({ preventScroll: true });
    renderState(context);
    try {
      const listing = await invoke("list_archive_directory", { sessionId: context.sessionId, directoryId, page });
      if (!ensureScope(context) || revision !== context.navigationRevision) return;
      acceptListing(context, listing, directoryId, page);
    } catch (error) {
      if (!ensureScope(context) || revision !== context.navigationRevision) return;
      if (terminalCodes.has(error?.code)) invalidate(context, errText(error));
      else context.error = errText(error);
    } finally {
      context.listing = false;
      renderState(context);
    }
  }
  async function previewSelection(context) {
    if (!ensureScope(context) || context.opening || context.listing || context.extract || context.read || !context.selected?.canPreview) return;
    const row = context.selected;
    const read = { requestId: crypto.randomUUID(), revision: ++context.previewRevision, entryId: row.entryId, cancelled: false };
    read.settled = new Promise(resolve => { read.resolve = resolve; });
    context.read = read;
    context.error = null;
    context.list.focus({ preventScroll: true });
    renderState(context);
    try {
      const response = await invoke("preview_archive_entry", { requestId: read.requestId, sessionId: context.sessionId, entryId: read.entryId });
      if (!ensureScope(context) || read.cancelled || read.revision !== context.previewRevision || context.selected?.rowId !== row.rowId) return;
      if (response.requestId !== read.requestId || response.sessionId !== context.sessionId || response.entryId !== read.entryId) { protocolError(context); return; }
      const preview = response.preview;
      context.preview.replaceChildren(element("h3", "archive-read-preview-title", row.displayName));
      context.preview.append(element("p", "archive-read-note", `${fmtEntrySize(preview.bytes)} · Inhalt vollständig geprüft${preview.content.kind === "image" ? ` · ${preview.content.width} × ${preview.content.height} Pixel` : ""}`));
      if (!renderPassivePreviewContent(context.preview, preview.content, { name: row.displayName })) {
        context.preview.append(element("p", "archive-read-note", preview.content.reason || "Keine passive Vorschau verfügbar."));
      }
      if (preview.limitations?.length) context.preview.append(element("p", "archive-read-note", preview.limitations.join(" ")));
      appendFileAction(context);
    } catch (error) {
      if (!ensureScope(context) || read.cancelled || read.revision !== context.previewRevision) return;
      if (terminalCodes.has(error?.code)) invalidate(context, errText(error));
      else context.error = errText(error);
    } finally {
      if (context.read === read) context.read = null;
      read.resolve();
      renderState(context);
    }
  }
  function open(side) {
    if (blocked()) return;
    const entries = selectedEntries(side);
    if (entries.length !== 1 || !entries[0].is_file || entries[0].is_symlink || !isEntryNameSafe(entries[0])) {
      showDock('<span class="pill none">ZIP-Inhalt</span>Bitte genau eine reguläre Datei ohne Symlink auswählen.', { closable: true });
      return;
    }
    if (current) close(current);
    const pane = panes[side];
    const context = { side, name: entries[0].name, scope: Object.fromEntries(scopeKeys.map(key => [key, pane[key]])), tabId: pane.tabs?.active?.id,
      uiId: ++dialogSequence, openRequestId: crypto.randomUUID(), sessionId: null, canExtract: false, canExtractFile: false, opening: true, listing: false, closed: false, stale: false,
      rows: [], selected: null, read: null, previewRevision: 0, navigationRevision: 0 };
    current = context;
    context.modalToken = showOwnedModal("archive-read", token => {
      context.modalToken = token;
      $("#modal .modal").classList.add("archive-read-modal");
      $("#modal-title").textContent = `ZIP-Inhalt · ${context.name}`;
      const body = $("#modal-body");
      body.replaceChildren();
      context.body = body;
      context.keyHandler = event => {
        if (event.key === "F3" && !event.defaultPrevented) { event.preventDefault(); void previewSelection(context); }
      };
      body.addEventListener("keydown", context.keyHandler);
      context.summary = element("p", "archive-read-summary", "Schreibgeschützte Archivansicht");
      context.origin = element("p", "archive-read-note"); context.origin.hidden = true;
      context.partial = element("p", "archive-read-partial"); context.partial.hidden = true; context.partial.setAttribute("role", "status");
      context.breadcrumbs = element("nav", "archive-read-breadcrumbs"); context.breadcrumbs.setAttribute("aria-label", "Virtueller Archivpfad");
      const layout = element("div", "archive-read-layout");
      const browser = element("section", "archive-read-browser"); browser.setAttribute("aria-label", "Archivinhalt");
      context.list = element("div", "archive-read-list"); context.list.tabIndex = 0; context.list.setAttribute("role", "listbox"); context.list.setAttribute("aria-label", "Archiveinträge");
      context.list.addEventListener("keydown", event => {
        if (event.key === "F3") { event.preventDefault(); void previewSelection(context); return; }
        if (event.key === "Enter") { event.preventDefault(); if (context.selected?.directoryId) void navigate(context, context.selected.directoryId, 0); return; }
        if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key) || !context.rows.length) return;
        event.preventDefault();
        const index = context.rows.indexOf(context.selected);
        const next = event.key === "Home" ? 0 : event.key === "End" ? context.rows.length - 1 : event.key === "ArrowDown" ? Math.min(context.rows.length - 1, index + 1) : Math.max(0, index - 1);
        selectRow(context, context.rows[next]);
      });
      const paging = element("div", "archive-read-pages");
      context.previous = button("Zurück", () => navigate(context, context.listingData.directoryId, context.listingData.page - 1));
      context.next = button("Weiter", () => navigate(context, context.listingData.directoryId, context.listingData.page + 1));
      context.pageLabel = element("span", "archive-read-page-label");context.pageLabel.setAttribute("aria-live", "polite");
      paging.append(context.previous, context.pageLabel, context.next);
      browser.append(context.list, paging);
      context.preview = element("section", "archive-read-preview"); context.preview.setAttribute("aria-label", "Archivvorschau");
      layout.append(browser, context.preview);
      context.status = element("p", "archive-read-status");context.status.setAttribute("role", "status");
      context.folderExtractButton = extractSelectionReady ? button("Geöffneten Ordner entpacken …", () => prepareSelection(context, "subtree")) : null;
      if (context.folderExtractButton) { context.folderExtractButton.classList.add("archive-read-extract-action"); context.folderExtractButton.hidden = true; body.append(context.origin, context.summary, context.partial, context.breadcrumbs, context.folderExtractButton, layout, context.status); }
      else body.append(context.origin, context.summary, context.partial, context.breadcrumbs, layout, context.status);
      const buttons = $("#modal .modal-buttons");buttons.replaceChildren();
      context.closeButton = button("Schließen", () => close(context));
      context.previewButton = button("Vorschau (F3)", () => previewSelection(context), "btn primary");
      buttons.append(context.closeButton, context.previewButton);
      if (extractSelectionReady) { context.wholeExtractButton = button("Ganzes Archiv entpacken …", () => prepareSelection(context, "all")); buttons.append(context.wholeExtractButton); }
      showSelection(context);
      renderState(context);
      context.scopeTimer = setInterval(() => {
        if (!ensureScope(context)) return;
        const work = context.extract;
        if (work && !work.invalid && !work.targetMatches()) {
          work.invalid = true;
          discardExtract(work.requestId);
          context.error = "Die Zielpane hat sich geändert. Die Vorbereitung wird verworfen; danach bitte erneut wählen.";
          renderState(context);
        }
      }, 250);
      void (async () => {
        try {
          const endpoint = { provider: context.scope.provider, root: context.scope.path,
            ...(context.scope.provider === "local" ? {} : { profileId: context.scope.profileId, profileRevision: context.scope.profileRevision }) };
          const response = await invoke("open_archive_read", { requestId: context.openRequestId, request: { endpoint, path: joinPath(context.scope.path, context.name) } });
          if (!ensureScope(context)) { if (response.sessionId) closeBackend(context, response.sessionId); return; }
          if (response.requestId !== context.openRequestId || response.listing.sessionId !== response.sessionId) { if (response.sessionId) closeBackend(context, response.sessionId); protocolError(context); return; }
          context.sessionId = response.sessionId;
          context.canExtract = response.canExtract === true;
          context.canExtractFile = response.canExtractFile === true || context.canExtract;
          context.origin.hidden = !response.origin;
          context.origin.textContent = response.origin ? `Momentaufnahme von ${response.origin}. Änderungen am Remoteoriginal verändern diese Ansicht nicht. ${context.canExtractFile ? "Entpacken verwendet diese geladenen Bytes und den Zielordner der anderen Pane." : "Diese Ansicht bleibt lesend."}` : "";
          acceptListing(context, response.listing, null, 0);
        } catch (error) {
          if (ensureScope(context)) context.error = errText(error);
        } finally {
          context.opening = false;
          renderState(context);
        }
      })();
      return { focusTarget: context.closeButton, onEscape: () => close(context) };
    });
  }
  return { open };
}
