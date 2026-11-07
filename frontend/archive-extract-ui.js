import { invoke } from "./tauri-api.js";
import { cancelOwnedModal, hideModal, showOwnedModal } from "./modal.js";
import { conflictChoiceModal } from "./conflict-dialog.js";
import { bindArchivePackPages, archiveUploadDetailsHtml, archiveIssuesHtml } from "./archive-pack-ui.js";
import { $, escapeHtml, fmtTransferBytes, errText, isEntryNameSafe, joinPath } from "./ui-utils.js";

const keys = ["provider", "profileId", "profileRevision", "sessionGeneration", "path", "navigationRevision"];
export function archiveExtractFailureMessage(code) {
  if (code === "invalid_data") return "Die Archiv- oder Inhaltsprüfung ist fehlgeschlagen.";
  if (code === "outcome_unclear") return "Der Entpackausgang ist teilweise unklar.";
  return "Das ZIP-Archiv konnte nicht vollständig entpackt werden.";
}
export function archiveExtractFailureHtml(payload) {
  return `<span class="pill delete">ZIP entpacken fehlgeschlagen</span>${escapeHtml(archiveExtractFailureMessage(payload.code))} ` +
    'Die Queue zeigt die tatsächliche Teilbilanz, vollständige Fehlerdetails und erhaltene Recoverypfade. ' +
    `<button class="dock-action" type="button" data-dock-action="open-queue" data-operation-id="${escapeHtml(payload.operationId)}">Entpackbilanz in Queue öffnen</button>`;
}
export function archiveExtractSummary(preview) {
  return `${preview.files} Datei(en), ${preview.directories} Ordner, ${fmtTransferBytes(preview.source_bytes)} Inhalt · ${preview.omitted_entries} bekannte Einträge ausgelassen`;
}
export function archiveExtractScope(preview) {
  const kind = preview.selection?.kind;
  return kind === "entry" ? `Ausgewählte Datei: ${preview.selection_path || preview.relative_root}`
    : kind === "subtree" ? `Geöffneter Ordner samt Inhalt: ${preview.selection_path || preview.relative_root}` : "Ganzes Archiv";
}
export function archiveExtractSourceLabel(preview) {
  return preview.snapshot ? `Momentaufnahme von ${preview.snapshot.origin}` : preview.source_path;
}
export function archiveExtractPreviewHtml(preview, { targetLabel = preview.target_dir, upload = false, notice = null } = {}) {
  if (upload) return `<p>${escapeHtml(archiveExtractScope(preview))}</p>` +
    `<p>Quelle: <code class="archive-pack-path">${escapeHtml(archiveExtractSourceLabel(preview))}</code></p>` +
    `<p>Ziel: <code class="archive-pack-path">${escapeHtml(targetLabel)}</code></p>` +
    `<p>${escapeHtml(archiveExtractSummary(preview))}</p>` +
    `<p>Zuerst lokal entpacken, danach hochladen. Die Quelle bleibt erhalten. Zielkonflikte werden vor dem Upload entschieden.</p>` +
    `<p>Native Archivmetadaten werden nicht wiederhergestellt.${!preview.catalog_complete ? " Nur der bekannte Archivteil wird entpackt." : ""}</p>` +
    archiveUploadDetailsHtml(notice) + archiveIssuesHtml("Vollständige Entpackhinweise seitenweise");
  return `<p class="archive-pack-path">${escapeHtml(archiveExtractScope(preview))}</p><p>Archivquelle: <code class="archive-pack-path">${escapeHtml(archiveExtractSourceLabel(preview))}</code></p>` +
    (preview.snapshot ? '<p>Die bestätigten Bytes stammen aus der bereits geladenen Momentaufnahme. Spätere Serveränderungen werden nicht übernommen. Fehlt die Aufnahme beim Queue-Start, muss das Archiv erneut geöffnet werden.</p>' : "") +
    `<p>Zielordner: <code class="archive-pack-path">${escapeHtml(targetLabel)}</code></p>` +
    (preview.relative_root ? `<p>Name im Zielordner: <code class="archive-pack-path">${escapeHtml(preview.relative_root)}</code></p>` : "") +
    `<p>${escapeHtml(archiveExtractSummary(preview))}</p>` +
    (!preview.catalog_complete ? '<p class="archive-pack-warning">Teilkatalog: Nur der bekannte, ausführbare Teil kann entpackt werden. Der übrige Archivinhalt ist unbekannt.</p>' : "") +
    '<p>Die Archivquelle bleibt erhalten. Vorhandene Ordner werden zusammengeführt; zusätzliche Zielinhalte bleiben erhalten. Symlinks und Spezialdateien werden ausgelassen. Native Archivmetadaten werden nicht wiederhergestellt.</p>' +
    (preview.commit_class === "best_effort" ? '<p class="archive-pack-warning">Best Effort: Nach frischem Zielcheck wird ein Commit versucht und zurückgelesen. Ein gleichzeitig neu angelegtes Ziel kann dabei ersetzt werden.</p>' : "") +
    (!preview.file_flush || !preview.directory_flush ? '<p class="archive-pack-warning">Best Effort bei Dauerhaftigkeit: Datei- oder Ordner-Flush ist auf dem Ziel nicht vollständig garantiert.</p>' : "") +
    `<div class="archive-pack-issues" role="region" tabindex="0" aria-label="Vollständige Entpackhinweise seitenweise"></div>` +
    '<div class="archive-pack-pages"><button type="button" class="btn ghost" data-archive-page="previous">Zurück</button><span aria-live="polite" data-archive-page-label></span><button type="button" class="btn ghost" data-archive-page="next">Weiter</button></div>';
}
function conflictsFor(preview) {
  return preview.conflict_details.map(conflict => ({
    relative_name: conflict.relative_name,
    source: { endpoint: `ZIP · ${archiveExtractSourceLabel(preview)}`, exists: true, kind: "file", size: conflict.source_bytes, mtime: null },
    target: { endpoint: joinPath(preview.target_dir, conflict.relative_name), exists: true, kind: "file", size: conflict.target_bytes, mtime: conflict.target_mtime },
  }));
}
export async function chooseArchiveExtractConflicts(preview, signal = null) {
  if (!preview.conflicts.length && !preview.type_conflicts.length) return "skip";
  return conflictChoiceModal({
    title: "Konflikte beim Entpacken", conflicts: conflictsFor(preview), signal,
    sourceReadLimitations: preview.issues.filter(issue => issue.content_omitted).map(issue => `${issue.path}: ${issue.reason}`),
    note: `${preview.conflicts.length} reguläre Dateikonflikte und ${preview.type_conflicts.length} nicht ersetzbare Typkonflikt(e). Typkonflikte werden vollständig übersprungen: ${preview.type_conflicts.join("; ")}. Die Ersetzen-Wahl gilt ausschließlich für reguläre Dateien.`,
    choices: [
      { label: "Überspringen", value: "skip", explanation: "Vorhandene Zieldateien erhalten. Übrige ausführbare Einträge entpacken." },
      ...(preview.conflicts.length ? [{ label: preview.replace_best_effort ? "Dauerhaft ersetzen" : "Ersetzen", value: "overwrite", danger: true,
        explanation: preview.replace_best_effort ? "Die angezeigten Zieldateien werden ohne Wiederherstellung dauerhaft ersetzt (Best Effort)." : "Die angezeigten Zieldateien durch geprüften Archivinhalt ersetzen. Zusätzliche Zielinhalte bleiben erhalten." }] : []),
    ],
  });
}
export function createArchiveExtractUi({ panes, activeSide, selectedEntries, blocked, showDock, showError }) {
  let current = null;
  const snapshot = side => ({ side, tabId: panes[side].tabs?.active?.id, ...Object.fromEntries(keys.map(key => [key, panes[side][key]])) });
  const matches = scope => panes[scope.side].tabs?.active?.id === scope.tabId && keys.every(key => panes[scope.side][key] === scope[key]);
  const discard = id => { if (id) void invoke("discard_archive_extract", { preparationId: id }).catch(() => {}); };
  function finish(context) {
    if (context.closed) return;
    context.closed = true;
    clearInterval(context.timer);
    context.conflictAbort?.abort();
    discard(context.requestId); discard(context.preparationId);
    cancelOwnedModal(context.token);
    if (current === context) current = null;
  }
  const valid = context => !context.closed && !context.stale && current === context && matches(context.source) && matches(context.target)
    && selectedEntries(context.source.side).length === 1 && selectedEntries(context.source.side)[0]?.name === context.name;
  function check(context) {
    if (valid(context)) return true;
    if (!context.closed && !context.stale) {
      context.stale = true; discard(context.requestId); discard(context.preparationId);
      context.preparationId = null; context.start.disabled = true;
      if (context.conflictAbort) {
        context.conflictAbort.abort();
        showDock('<span class="pill none">Entpacken nicht eingereiht</span>Auswahl oder Zielpane geändert. Bitte neu öffnen.', { closable: true });
      }
      context.status.textContent = "Auswahl oder Pane-Ziel geändert. Bitte schließen und neu öffnen.";
      if (context.cancel.isConnected) context.cancel.focus({ preventScroll: true });
    }
    return false;
  }
  function open(side = activeSide(), prepared = null) {
    if (blocked()) { if (prepared) discard(prepared.preparationId); return; }
    const entries = selectedEntries(side);
    if (entries.length !== 1 || !entries[0].is_file || entries[0].is_symlink || !isEntryNameSafe(entries[0])) {
      if (prepared) discard(prepared.preparationId);
      showError("ZIP entpacken", new Error("Bitte genau eine reguläre Archivdatei auswählen.")); return;
    }
    if (current) finish(current);
    const context = { source: snapshot(side), target: snapshot(side === "left" ? "right" : "left"), name: entries[0].name,
      requestId: prepared?.requestId || crypto.randomUUID(), closed: false, stale: false, busy: true, preparationId: prepared?.preparationId || null };
    current = context;
    context.token = showOwnedModal("archive-extract", token => {
      context.token = token;
      $("#modal .modal").classList.add("archive-pack-modal");
      $("#modal-title").textContent = "ZIP entpacken";
      const body = $("#modal-body");
      body.innerHTML = `<div class="archive-extract-initial"><p>Ganzes Archiv: <code class="archive-pack-path">${escapeHtml(context.name)}</code></p><p>Ziel ist der Ordner der anderen Pane:</p><code class="archive-pack-path">${escapeHtml(context.target.path || "Kein Zielordner geöffnet")}</code></div><p class="archive-pack-status" role="status">Archiv und Ziel werden geprüft … Abbrechen bleibt möglich.</p><div class="archive-extract-result"></div>`;
      context.status = body.querySelector("[role=status]"); context.result = body.querySelector(".archive-extract-result");
      const buttons = $("#modal .modal-buttons"); buttons.replaceChildren();
      context.cancel = document.createElement("button"); context.cancel.className = "btn ghost"; context.cancel.textContent = "Abbrechen";
      context.start = document.createElement("button"); context.start.className = "btn primary"; context.start.textContent = "Entpacken"; context.start.disabled = true;
      buttons.append(context.cancel, context.start);
      context.cancel.addEventListener("click", () => finish(context));
      context.start.addEventListener("click", async () => {
        if (context.busy || !check(context) || !context.preparationId) return;
        context.busy = true; context.start.disabled = true; context.cancel.focus({ preventScroll: true });
        context.status.textContent = "Entpacken wird eingereiht … Ein bereits angenommener Auftrag bleibt in der Queue abbrechbar.";
        try {
          let choice = "skip";
          if (context.preview.conflicts.length || context.preview.type_conflicts.length) {
            hideModal(context.token);
            context.conflictAbort = new AbortController();
            choice = await chooseArchiveExtractConflicts(context.preview, context.conflictAbort.signal);
            context.conflictAbort = null;
            if (!check(context) || choice == null) { finish(context); return; }
          }
          const id = await invoke("confirm_archive_extract", { preparationId: context.preparationId,
            overwrite: choice === "overwrite", allowPartial: !context.preview.catalog_complete });
          // Ein erfolgreicher Queuecommit bleibt auch nach Schließen oder Navigation
          // wirksam. Die Rückmeldung behauptet deshalb keinen Abbruch der Wirkung.
          if (!context.closed) showDock(`<span class="pill copy">Entpacken eingereiht</span>Fortschritt, Teilbilanz und Abbruch stehen in der Fensterqueue.`, { closable: true });
          finish(context);
          return id;
        } catch (error) {
          if (!context.closed) {
            if (context.status.isConnected) { context.status.textContent = errText(error); context.cancel.textContent = "Schließen"; }
            else { showError("ZIP entpacken", error); finish(context); }
          }
        } finally { context.busy = false; }
      });
      context.timer = setInterval(() => check(context), 250);
      void (async () => {
        try {
          const response = prepared || await invoke("prepare_archive_extract", { request: { requestId: context.requestId,
            provider: context.source.provider, root: context.source.path, sourcePath: joinPath(context.source.path, context.name),
            targetProvider: context.target.provider, targetDir: context.target.path,
            ...(context.target.provider !== "local" ? { target: { provider: context.target.provider, root: context.target.path,
              profileId: context.target.profileId, profileRevision: context.target.profileRevision } } : {}) } });
          if (!check(context)) { discard(response.preparationId); return; }
          if (response.requestId !== context.requestId) { discard(response.preparationId); throw new Error("Die Entpackantwort gehört zu einer anderen Anfrage. Bitte neu öffnen."); }
          context.preparationId = response.preparationId; context.preview = response.binding.preview; context.uploadTarget = response.uploadTarget;
          body.querySelector(".archive-extract-initial")?.remove();
          context.result.innerHTML = archiveExtractPreviewHtml(context.preview, response.uploadTarget ? { targetLabel: response.uploadTarget, upload: true, notice: response.uploadNotice } : {}); bindArchivePackPages(context.result, context.preview);
          context.status.textContent = response.uploadTarget ? "Entpackumfang geprüft. Konflikte am entfernten Ziel werden nach der lokalen Phase entschieden." : (context.preview.conflicts.length || context.preview.type_conflicts.length) ? `${context.preview.conflicts.length + context.preview.type_conflicts.length} Zielkonflikt(e). Entpacken öffnet die konkrete Konfliktentscheidung.` : "Ziel und bekannte Einträge geprüft. Entpacken reiht die Ausführung ein.";
          context.start.textContent = response.uploadTarget ? (context.preview.catalog_complete ? "Entpacken und hochladen" : "Bekannten Teil entpacken und hochladen") : context.preview.catalog_complete ? "Entpacken" : "Bekannten Teil entpacken";
          context.start.disabled = context.preview.files + context.preview.directories === 0;
        } catch (error) {
          if (check(context)) { context.status.textContent = errText(error); context.cancel.textContent = "Schließen"; }
        } finally { context.busy = false; }
      })();
      return { focusTarget: context.cancel, onEscape: () => finish(context) };
    });
  }
  return { open, openPrepared: (side, response) => open(side, response) };
}
export function confirmArchiveExtractPreview(preview, { confirm = true, overwrite = false, upload = false, targetLabel = preview.target_dir } = {}) {
  return new Promise(resolve => {
    showOwnedModal("archive-extract-review", token => {
      $("#modal .modal").classList.add("archive-pack-modal");
      $("#modal-title").textContent = confirm ? "Geändertes Entpacken bestätigen" : "Entpackumfang und Hinweise";
      $("#modal-body").innerHTML = archiveExtractPreviewHtml(preview, {upload, targetLabel}) + (preview.conflicts.length ? `<p>${preview.conflicts.length} vorhandene Zieldatei(en) werden ${overwrite ? "ersetzt" : "übersprungen"}: ${preview.conflicts.map(name => `<code>${escapeHtml(name)}</code>`).join(" · ")}</p>` : ""); bindArchivePackPages($("#modal-body"), preview);
      const bar = $("#modal .modal-buttons"); bar.replaceChildren();
      const finish = value => { if (hideModal(token)) resolve(value); };
      const cancel = document.createElement("button"); cancel.className = "btn ghost"; cancel.textContent = confirm ? "Abbrechen" : "Schließen"; cancel.addEventListener("click", () => finish(false)); bar.append(cancel);
      if (confirm) { const ok = document.createElement("button"); ok.className = "btn primary"; ok.textContent = !preview.catalog_complete ? "Bekannten Teil entpacken" : overwrite && preview.conflicts.length ? "Zieldateien ersetzen und entpacken" : "Geänderten Umfang entpacken"; ok.addEventListener("click", () => finish(true)); bar.append(ok); }
      return { focusTarget: cancel, onEscape: () => finish(false) };
    });
  });
}
