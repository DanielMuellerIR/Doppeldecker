import { hideModal, showOwnedModal } from "./modal.js";
import { invoke } from "./tauri-api.js";
import { $, escapeHtml } from "./ui-utils.js";

/** @typedef {import("./saved-sync-job-wire.js").SyncJobDraftWire} SyncJobDraftWire */
/** @typedef {import("./saved-sync-job-wire.js").SyncJobEndpointWire} SyncJobEndpointWire */
/** @typedef {import("./saved-sync-job-wire.js").SavedSyncJobWire} SavedSyncJobWire */
/** @typedef {{name: string, existing: SavedSyncJobWire | null}} SyncJobChoice */

const modeLabels = { AddOnly: "Fehlendes hinzufügen", Update: "Vorhandenes aktualisieren", Mirror: "Spiegeln" };

/** @param {{
 * capture: () => SyncJobDraftWire | null,
 * load: (job: SavedSyncJobWire) => Promise<boolean>,
 * describeEndpoint: (endpoint: SyncJobEndpointWire) => string,
 * running: () => boolean,
 * showError: (title: string, error: unknown) => void,
 * showDock: (message: string, options: {closable: boolean}) => void,
 * }} dependencies */
export function createSyncJobsUi({ capture, load, describeEndpoint, running, showError, showDock }) {
  /** @type {SavedSyncJobWire | null} */
  let current = null;
  let busy = false;

  /** @param {SyncJobDraftWire} job */
  function endpointsHtml(job) {
    const [source, target] = job.direction === "rl" ? [job.right, job.left] : [job.left, job.right];
    return `<dl class="sync-job-endpoints"><dt>Quelle</dt><dd>${escapeHtml(describeEndpoint(source))}</dd>` +
      `<dt>Ziel</dt><dd>${escapeHtml(describeEndpoint(target))}</dd>` +
      `<dt>Wirkung</dt><dd>${modeLabels[job.options.mode]}</dd></dl>`;
  }

  /** @param {SavedSyncJobWire} job */
  function markCurrent(job) {
    current = job;
    const label = $("#sync-job-current");
    label.textContent = job.name;
    label.hidden = false;
    $("#sync-job-save").textContent = "Auftrag bearbeiten …";
  }

  /** @param {SavedSyncJobWire[]} jobs @returns {Promise<SavedSyncJobWire | null>} */
  function selectJob(jobs) {
    return new Promise(resolve => showOwnedModal("sync-job-open", token => {
      $("#modal-title").textContent = "Gespeicherte Sync-Aufträge";
      const body = $("#modal-body");
      body.innerHTML = jobs.length
        ? `<div class="sync-job-fields"><label for="saved-sync-job">Auftrag</label>` +
          `<select class="modal-input" id="saved-sync-job"></select><div id="saved-sync-job-endpoints"></div></div>`
        : `<p>Es gibt noch keine gespeicherten Aufträge. „Auswahl speichern …“ übernimmt die aktuellen Ordner und Sync-Optionen.</p>`;
      const select = $("#saved-sync-job");
      const buttons = $("#modal .modal-buttons");
      const finish = value => { if (hideModal(token)) resolve(value); };
      const cancel = document.createElement("button");
      cancel.className = "btn ghost";
      cancel.textContent = jobs.length ? "Abbrechen" : "Schließen";
      cancel.addEventListener("click", () => finish(null));
      buttons.replaceChildren(cancel);
      if (select) {
        for (const job of jobs) select.add(new Option(job.name, job.id));
        if (jobs.some(job => job.id === current?.id)) select.value = current.id;
        const selectedJob = () => jobs[select.selectedIndex];
        const describe = () => { $("#saved-sync-job-endpoints").innerHTML = endpointsHtml(selectedJob()); };
        select.addEventListener("change", describe);
        describe();
        const open = document.createElement("button");
        open.className = "btn primary";
        open.textContent = "Auftrag öffnen";
        open.addEventListener("click", () => finish(selectedJob()));
        buttons.append(open);
      }
      return { focusTarget: select || cancel, onEscape: () => finish(null) };
    }));
  }

  /** @param {SyncJobDraftWire} draft @returns {Promise<SyncJobChoice | null>} */
  function nameJob(draft) {
    return new Promise(resolve => showOwnedModal("sync-job-save", token => {
      $("#modal-title").textContent = current ? "Sync-Auftrag bearbeiten" : "Sync-Auftrag speichern";
      $("#modal-body").innerHTML = `<div class="sync-job-fields"><label for="saved-sync-job-name">Name</label>` +
        `<input class="modal-input" id="saved-sync-job-name" type="text" autocomplete="off" /></div>` +
        endpointsHtml(draft) + `<p>Gespeichert werden die gezeigten Ordner und die aktuellen Sync-Optionen. ` +
        `Für andere Ordner wechseln Sie in die Dateiansicht.</p>`;
      const input = $("#saved-sync-job-name");
      input.value = current?.name || "";
      const bar = $("#modal .modal-buttons");
      const finish = value => { if (hideModal(token)) resolve(value); };
      const cancel = document.createElement("button");
      cancel.className = "btn ghost";
      cancel.textContent = "Abbrechen";
      cancel.addEventListener("click", () => finish(null));
      bar.replaceChildren(cancel);
      const save = (label, cls, existing) => {
        const button = document.createElement("button");
        button.className = `btn ${cls}`;
        button.textContent = label;
        button.disabled = !input.value.trim();
        input.addEventListener("input", () => { button.disabled = !input.value.trim(); });
        button.addEventListener("click", () => finish({ name: input.value.trim(), existing }));
        bar.append(button);
        return button;
      };
      if (current) save("Als neuen Auftrag speichern", "ghost", null);
      const primary = save(current ? "Änderungen speichern" : "Auftrag speichern", "primary", current);
      input.addEventListener("keydown", event => {
        if (event.key === "Enter" && !event.isComposing) primary.click();
      });
      input.select();
      return { focusTarget: input, onEscape: () => finish(null) };
    }));
  }

  function wire() {
    $("#sync-job-open").addEventListener("click", async () => {
      if (busy || running()) return;
      busy = true;
      try {
        const jobs = await invoke("list_saved_sync_jobs");
        jobs.sort((a, b) => a.name.localeCompare(b.name, "de"));
        const selected = await selectJob(jobs);
        if (selected && await load(selected)) markCurrent(selected);
      } catch (error) {
        showError("Sync-Auftrag öffnen", error);
      } finally {
        busy = false;
      }
    });

    $("#sync-job-save").addEventListener("click", async () => {
      if (busy || running()) return;
      busy = true;
      try {
        const draft = capture();
        if (!draft) return;
        const choice = await nameJob(draft);
        if (!choice) return;
        const id = choice.existing?.id;
        const revision = choice.existing?.revision;
        const request = /** @satisfies {import("./saved-sync-job-wire.js").SaveSyncJobRequestWire} */ ({
          ...draft, name: choice.name, id, revision,
        });
        const job = await invoke("save_sync_job", { request });
        markCurrent(job);
        showDock(`<span class="pill ok">Sync-Auftrag gespeichert</span>${escapeHtml(job.name)}`, { closable: true });
      } catch (error) {
        showError("Sync-Auftrag speichern", error);
      } finally {
        busy = false;
      }
    });
  }

  return { wire };
}
