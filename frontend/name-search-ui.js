// Namenssuche besitzt einen eigenen Modal-Lease und eigene Lauf-Token. Treffer
// sind Navigationsziele aus dem Backend, keine Ausführungsbelege für Dateiaktionen.
import { invoke, listen } from "./tauri-api.js";
import { cancelOwnedModal, showOwnedModal } from "./modal.js";
import { $, escapeHtml, errText } from "./ui-utils.js";

const PAGE_SIZE = 100;
const statusLabels = {
  running: "Suche läuft", complete: "Suche abgeschlossen", partial: "Unvollständige Suche",
  limited: "Suchgrenze erreicht · unvollständig", cancelled: "Suche abgebrochen · Teilbilanz",
};

export function createNameSearchUi({ panes, endpoint, revealHit, activate }) {
  let current = null;
  // Ein Webview-Neuladen darf alte Backendereignisse nicht mit erneut bei 1
  // beginnenden Clienttokens an einen frischen Suchdialog binden.
  let sequence = Date.now() * 1000;
  let listenersReady = null;

  function scopeMatches(context) {
    const pane = panes[context.side];
    return pane && pane.provider === context.provider && pane.profileId === context.profileId
      && pane.profileRevision === context.profileRevision && pane.sessionGeneration === context.sessionGeneration
      && pane.path === context.root && pane.navigationRevision === context.navigationRevision;
  }

  function cancel(context) {
    if (!context.running || context.cancelling) return;
    context.cancelling = true;
    const clientToken = context.clientToken;
    renderStatus(context);
    void invoke("cancel_name_search", { clientToken }).catch(error => {
      if (current !== context || context.clientToken !== clientToken) return;
      context.error = `Abbruch konnte nicht übermittelt werden: ${errText(error)}`;
      renderStatus(context);
    });
  }

  function close(context) {
    if (current !== context) return;
    cancel(context);
    cancelOwnedModal(context.modalToken);
    current = null;
  }

  function renderStatus(context) {
    if (current !== context || !context.statusElement?.isConnected) return;
    const stats = context.stats;
    const number = value => Number(value || 0).toLocaleString("de-DE");
    context.statusElement.textContent = `${context.cancelling && context.running ? "Abbruch angefordert; laufendes Listing endet noch" : statusLabels[context.status] || "Bereit"}`
      + ` · ${number(stats.hits)} Treffer · ${number(stats.entries)} Einträge · ${number(stats.directories)} Verzeichnisse`
      + ` · ${number(stats.skipped)} ausgelassen · ${number(stats.issues)} Fehler/Hinweise`;
    context.noteElement.textContent = [context.error, context.limit].filter(Boolean).join(" · ");
    context.startButton.disabled = context.running || !scopeMatches(context);
    context.stopButton.disabled = !context.running || context.cancelling;
    context.input.disabled = context.running;
    context.caseInput.disabled = context.running;
    context.hiddenInput.disabled = context.running;
    // Ein gerade deaktivierter Auslöser verliert seinen Fokus sonst auf body.
    // Escape und die gemeinsame Fokusfalle müssen auch vor der Startantwort wirken.
    const modal = context.statusElement.closest(".modal");
    if (!modal.contains(document.activeElement) || document.activeElement.matches?.(":disabled")) {
      (context.running && !context.cancelling ? context.stopButton : context.closeButton)?.focus({ preventScroll: true });
    }
    context.issuesSummary.textContent = `Fehler und ausgelassene Namen (${context.issues.length} von ${Number(context.stats.issues || 0).toLocaleString("de-DE")}; höchstens 100 Details)`;
  }

  function renderResults(context) {
    if (current !== context || !context.resultsElement?.isConnected) return;
    const start = context.page * PAGE_SIZE;
    const list = context.resultsElement;
    if (context.renderedPage !== context.page) {
      list.replaceChildren();
      context.renderedPage = context.page;
    }
    for (const hit of context.hits.slice(start + list.children.length, start + PAGE_SIZE)) {
      const item = document.createElement("li");
      const button = document.createElement("button");
      button.type = "button";
      button.className = "name-search-hit";
      button.textContent = `${hit.is_symlink ? "Link" : hit.is_dir ? "Ordner" : "Datei"} · ${hit.relative_path}`;
      button.title = `Übergeordneten Ordner öffnen und ${hit.name} auswählen`;
      button.addEventListener("click", () => {
        if (!scopeMatches(context)) {
          cancel(context);
          context.error = "Die Ausgangspane hat sich geändert. Bitte die Suche vom aktuellen Ort neu öffnen.";
          renderStatus(context);
          return;
        }
        close(context);
        void revealHit(context.side, hit.parent, hit.name, Boolean(hit.is_hidden));
      });
      item.appendChild(button);
      list.appendChild(item);
    }
    const total = context.hits.length;
    context.pageLabel.textContent = total
      ? `${start + 1}–${Math.min(start + PAGE_SIZE, total)} von ${total.toLocaleString("de-DE")} Treffern`
      : "Keine Treffer";
    context.previousButton.disabled = context.page === 0;
    context.nextButton.disabled = start + PAGE_SIZE >= total;
    context.issuesElement.textContent = context.issues.map(issue => `${issue.path}: ${issue.message}`).join("\n");
    context.issuesDetails.hidden = !context.issues.length;
  }

  function receive(payload, failed = false) {
    const context = current;
    if (!context || payload.client_token !== context.clientToken || payload.side !== context.side
      || (context.runId != null && payload.run_id !== context.runId)) return;
    context.runId = payload.run_id;
    if (!scopeMatches(context)) {
      cancel(context);
      context.error = "Die Ausgangspane oder ihre Verbindung hat sich geändert. Diese Suche ist unvollständig. Bitte die Suche vom aktuellen Ort neu öffnen.";
      context.running = false;
      context.status = "partial";
      renderStatus(context);
      return;
    }
    if (failed) {
      context.running = false;
      context.status = "partial";
      context.error = errText(payload.result);
    } else {
      const update = payload.result;
      context.hits.push(...update.hits);
      context.issues.push(...update.issues);
      context.stats = update.stats;
      context.status = update.status;
      context.limit = update.limit;
      context.running = update.status === "running";
    }
    renderStatus(context);
    if (failed || payload.result.hits.length || payload.result.issues.length) renderResults(context);
  }

  async function start(context) {
    if (current !== context || context.running || !scopeMatches(context)) return;
    const query = context.input.value;
    if (!query.trim()) { context.input.focus(); return; }
    context.clientToken = ++sequence;
    const clientToken = context.clientToken;
    context.runId = null;
    context.running = true;
    context.cancelling = false;
    context.status = "running";
    context.hits = [];
    context.issues = [];
    context.stats = {};
    context.page = 0;
    context.renderedPage = null;
    context.error = null;
    context.limit = null;
    renderStatus(context);
    renderResults(context);
    try {
      listenersReady ||= Promise.all([
        listen("name-search-update", event => receive(event.payload)),
        listen("name-search-error", event => receive(event.payload, true)),
      ]);
      await listenersReady;
      if (current !== context || context.cancelling) {
        context.running = false;
        context.status = "cancelled";
        renderStatus(context);
        return;
      }
      const started = await invoke("start_name_search", { request: {
        side: context.side, endpoint: context.endpoint, sessionGeneration: context.sessionGeneration,
        clientToken, options: { query, caseSensitive: context.caseInput.checked,
          includeHidden: context.hiddenInput.checked },
      } });
      // Events können vor der Startantwort eintreffen. Sie besitzen bereits das
      // Clienttoken und dürfen durch die späte Antwort nicht zurückgesetzt werden.
      if (current !== context || context.clientToken !== clientToken) {
        void invoke("cancel_name_search", { clientToken });
        return;
      }
      context.runId = started.run_id;
      if (context.cancelling) void invoke("cancel_name_search", { clientToken: context.clientToken });
    } catch (error) {
      if (current !== context || context.clientToken !== clientToken) return;
      context.running = false;
      context.status = "partial";
      context.error = errText(error);
      renderStatus(context);
    }
  }

  function open(side) {
    const pane = panes[side];
    if (!pane?.path || pane.navigationPending) return;
    activate(side);
    const context = {
      side, provider: pane.provider, profileId: pane.profileId, profileRevision: pane.profileRevision,
      sessionGeneration: pane.sessionGeneration, root: pane.path, navigationRevision: pane.navigationRevision,
      endpoint: endpoint(pane), clientToken: null, runId: null, running: false, cancelling: false,
      hits: [], issues: [], stats: {}, page: 0, status: null,
    };
    context.modalToken = showOwnedModal("name-search", token => {
      context.modalToken = token;
      current = context;
      $("#modal .modal").classList.add("name-search-modal");
      $("#modal-title").textContent = "Namen in Unterordnern suchen";
      const body = $("#modal-body");
      body.innerHTML = `<p class="name-search-root">Ab <code>${escapeHtml(context.root)}</code></p>
        <form class="name-search-form">
          <label class="modal-input-label" for="name-search-query">Name enthält</label>
          <input id="name-search-query" class="modal-input" type="search" maxlength="1024" autocomplete="off" spellcheck="false" placeholder="Zum Beispiel rechnung oder .pdf">
          <div class="name-search-options"><label><input type="checkbox" data-search-case> Groß-/Kleinschreibung beachten</label>
          <label><input type="checkbox" data-search-hidden> Versteckte Namen und Ordner einbeziehen</label></div>
          <p class="name-search-help">Durchsucht Namen im Ausgangsordner und seinen Unterordnern, keine Dateiinhalte. Symbolische Links werden angezeigt, aber nicht betreten. Änderungen während der Suche sind möglich.${context.provider === "sftp" ? " SFTP: pfadbasiertes Best Effort; keine atomare Momentaufnahme. Ein laufendes Listing kann bis zu 60 Sekunden zum Abbruch brauchen." : " Abbruch wirkt nach dem laufenden Providerlisting."}</p>
          <p class="name-search-help">Technische Suchgrenzen: 5.000 Treffer, 250.000 Einträge, 25.000 Verzeichnisanfragen. Eine erreichte Speicher- oder Tiefengrenze beendet die Suche ebenfalls mit sichtbarer Teilbilanz.</p>
          <div class="name-search-actions"><button class="btn primary" type="submit" data-search-start>Suchen</button>
          <button class="btn ghost" type="button" data-search-stop disabled>Suche abbrechen</button></div>
        </form>
        <p class="name-search-status" role="status" aria-live="polite"></p><p class="name-search-note" role="alert"></p>
        <p class="name-search-help">Treffer öffnen den übergeordneten Ordner und wählen den Namen aus.</p>
        <ul class="name-search-results" aria-label="Suchtreffer"></ul>
        <div class="name-search-pages"><button class="btn ghost" type="button" data-search-previous>Zurück</button>
          <span data-search-page></span><button class="btn ghost" type="button" data-search-next>Weiter</button></div>
        <details class="name-search-issues" hidden><summary></summary><pre tabindex="0"></pre></details>`;
      context.input = $("#name-search-query", body);
      context.caseInput = $("[data-search-case]", body);
      context.hiddenInput = $("[data-search-hidden]", body);
      context.startButton = $("[data-search-start]", body);
      context.stopButton = $("[data-search-stop]", body);
      context.statusElement = $(".name-search-status", body);
      context.noteElement = $(".name-search-note", body);
      context.resultsElement = $(".name-search-results", body);
      context.pageLabel = $("[data-search-page]", body);
      context.previousButton = $("[data-search-previous]", body);
      context.nextButton = $("[data-search-next]", body);
      context.issuesDetails = $(".name-search-issues", body);
      context.issuesSummary = $("summary", context.issuesDetails);
      context.issuesElement = $("pre", context.issuesDetails);
      $("form", body).addEventListener("submit", event => { event.preventDefault(); void start(context); });
      context.stopButton.addEventListener("click", () => cancel(context));
      for (const [button, delta] of [[context.previousButton, -1], [context.nextButton, 1]]) {
        button.addEventListener("click", () => { context.page += delta; renderResults(context); });
      }
      const bar = $("#modal .modal-buttons");
      const closeButton = document.createElement("button");
      closeButton.type = "button";
      closeButton.className = "btn ghost";
      closeButton.textContent = "Schließen";
      context.closeButton = closeButton;
      closeButton.addEventListener("click", () => close(context));
      bar.replaceChildren(closeButton);
      renderStatus(context);
      renderResults(context);
      return { focusTarget: context.input, onEscape: () => close(context) };
    });
  }

  return { open };
}
