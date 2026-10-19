import { invoke } from "./tauri-api.js";
import { $, $$, escapeHtml } from "./ui-utils.js";

/** Gemeinsame Dock- und Fortschrittsanzeige für Datei-, Sync- und Queue-Läufe.
 * Der fachliche Laufzustand bleibt im Orchestrator und wird nur über setRunning gesetzt. */
export function createOperationUi({ planSummaryHtml, setRunning, captureEndpoints = () => [] }) {
  const contexts = new Map();
  const completed = new Set();
  let activeContext = null;
  let dockRevision = 0;

  const runKey = context => `${context?.operationId}/${context?.attempt || 0}`;

  function bindContext(context) {
    if (!context?.operationId || !Array.isArray(context.affectedEndpoints)) return;
    if (completed.has(runKey(context))) return;
    const bound = { operationId: context.operationId, attempt: context.attempt || 0,
      affectedEndpoints: context.affectedEndpoints.map(endpoint => ({ ...endpoint })) };
    contexts.set(runKey(context), bound);
    if (contexts.size > 256) contexts.delete(contexts.keys().next().value);
    if (runKey(activeContext) === runKey(context)) activeContext = bound;
  }

  function acceptEvent(payload) {
    const id = payload?.operationId;
    if (!id) return true;
    if (completed.has(runKey(payload)) || (activeContext?.operationId && runKey(activeContext) !== runKey(payload))) return false;
    bindContext(payload);
    activeContext = contexts.get(runKey(payload)) || { operationId: id, attempt: payload.attempt || 0, affectedEndpoints: captureEndpoints() };
    return true;
  }
  // Die Dock-Knöpfe gehören zur Anzeige selbst und werden hier direkt verdrahtet.
  $("#btn-cancel").addEventListener("click", () => invoke("cancel_sync"));
  $("#btn-dock-close").addEventListener("click", () => { $("#dock").hidden = true; });
  $("#btn-toggle-list").addEventListener("click", () => {
    const list = $("#actions-list");
    list.hidden = !list.hidden;
    $("#btn-toggle-list").textContent = list.hidden ? "Details" : "Weniger";
  });

  function showDock(summaryHtml, options = {}) {
    dockRevision += 1;
    $("#dock").hidden = false;
    $("#dock-summary").innerHTML = summaryHtml;
    if (!options.keepList) {
      $("#actions-list").hidden = true;
      $("#btn-toggle-list").hidden = !options.showList;
    }
    if (options.showList) $("#btn-toggle-list").hidden = false;
  }

  function setLocked(locked) {
    $("#app").classList.toggle("operation-running", locked);
    // Der Lauf besitzt seinen vorbereiteten Plan. Nur dessen Bedienelemente
    // bleiben gesperrt; sichtbare Panes und weitere Queueaufträge sind unabhängig.
    const auswahl = $$(".sync-view button:not(#btn-sync-close), .sync-view input:not(#sync-add), #btn-quarantine");
    $("#btn-sync-close").textContent = locked ? "Dateiansicht" : "Schließen";
    if (locked) {
      // Beim Sperren merken, was schon aus fachlichen Gründen gesperrt war.
      auswahl.forEach(element => {
        if (element.dataset.lockedBefore === undefined) {
          element.dataset.lockedBefore = element.disabled ? "1" : "";
        }
        element.disabled = true;
      });
      return;
    }
    // Beim Freigeben nur die Laufsperre zurücknehmen. Zuvor hob das Laufende jede
    // Sperre auf — auch die von Quarantäne- und Realmodus-Knopf, die eine fehlende
    // Providerfähigkeit gesetzt hatte. Der Knopf war danach klickbar, während
    // Zähler und Titel weiter „gesperrt" sagten.
    auswahl.forEach(element => {
      element.disabled = element.dataset.lockedBefore === "1" || element.dataset.tabUnavailable === "true";
      delete element.dataset.lockedBefore;
    });
  }

  /**
   * Versetzt die Oberfläche in den Laufzustand: Abbruch und Fortschritt sichtbar,
   * Balken auf null, Planbedienelemente gesperrt. Der Text darüber unterscheidet sich
   * je nach Art des Laufs und kommt deshalb vom Aufrufer.
   */
  function startRun(phaseText, context) {
    activeContext = contexts.get(runKey(context))
      || { operationId: context?.operationId, attempt: context?.attempt || 0, affectedEndpoints: captureEndpoints() };
    setRunning(true);
    setLocked(true);
    $("#btn-cancel").hidden = false;
    $("#progress").hidden = false;
    $("#bar-fill").style.width = "0%";
    $("#prog-text").textContent = phaseText;
  }

  function startFile(label, context) {
    startRun(label, context);
    showDock(`<span class="pill copy">${escapeHtml(label)}</span>`, {});
  }

  function startSync(plan, context) {
    const execution = context?.localExecution;
    const bestEffort = execution && (execution.new_file_commit === "best_effort" || !execution.file_flush || !execution.directory_flush);
    startRun(context?.simulated ? "Simulierter Testlauf startet …" : "Sync-Ausführung startet …", context);
    showDock(`<span class="pill ${context?.simulated ? "none" : "copy"}">${context?.simulated ? "Simulation · keine Dateiänderungen" : bestEffort ? "Best-Effort-Sync-Ausführung" : execution ? "Abgesicherte Sync-Ausführung" : "Echte Sync-Ausführung"}</span>` + planSummaryHtml(plan), { keepList: true });
  }

  function update(progress) {
    if (!acceptEvent(progress)) return;
    const percent = progress.bytes_total > 0
      ? Math.min(100, (progress.bytes_done / progress.bytes_total) * 100)
      : progress.phase === "done" ? 100 : 0;
    $("#bar-fill").style.width = percent.toFixed(1) + "%";
    const phase = {
      copy: "Kopiere",
      move: "Verschiebe",
      metadata: "Übertrage Metadaten",
      quarantine: "Sichere",
      delete: "Lösche",
      done: "Fertig",
      cancelled: "Abgebrochen",
    }[progress.phase] || progress.phase;
    const current = progress.current ? ` · ${progress.current}` : "";
    $("#prog-text").textContent = `${phase} ${progress.files_done}/${progress.files_total}${current}`;
  }

  function end() {
    const finished = activeContext;
    activeContext = null;
    if (finished?.operationId) {
      contexts.delete(runKey(finished));
      completed.add(runKey(finished));
      if (completed.size > 256) completed.delete(completed.values().next().value);
    }
    setRunning(false);
    setLocked(false);
    $("#btn-cancel").hidden = true;
    return finished;
  }

  return { acceptEvent, bindContext, dockRevision: () => dockRevision, end, setLocked, showDock, startFile, startSync, update };
}
