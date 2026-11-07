import { invoke } from "./tauri-api.js";
import { hideModal, showOwnedModal } from "./modal.js";
import { $, errText, escapeHtml } from "./ui-utils.js";

export function createRemoteEditUi({ showDock, showError, refreshQueue }) {
  let entries = [];
  let refreshGeneration = 0;

  function updateBadge() {
    $("#remote-edit-count").textContent = String(entries.length);
    $("#btn-remote-edits").hidden = !entries.length;
  }

  async function refresh() {
    const generation = ++refreshGeneration;
    const result = await invoke("list_remote_edits");
    if (generation !== refreshGeneration) return;
    entries = result;
    updateBadge();
  }

  async function open() {
    try { await refresh(); }
    catch (error) { showError("Remote-Bearbeitung", error); return; }
    showOwnedModal("remote-edits", token => {
      $("#modal-title").textContent = "Remote-Bearbeitung";
      const body = $("#modal-body");
      body.innerHTML = '<p class="modal-description">In der externen Anwendung speichern, dann hier zurückspeichern. Zwischenzeitliche Serveränderungen werden als Konflikt gemeldet.</p><div class="remote-edit-list"></div><p role="status" class="modal-description"></p><p class="quarantine-meta">Diese Liste gilt bis zum Fensterende oder App-Neustart. Lokale Arbeitskopien bleiben im privaten Cache.</p>';
      const list = body.querySelector(".remote-edit-list");
      const status = body.querySelector('[role="status"]');
      let pending = false;
      const render = () => {
        list.innerHTML = entries.length ? entries.map((entry, index) => `<section class="remote-edit-entry"><h3>${escapeHtml(entry.name)}</h3><p class="remote-edit-endpoint">${escapeHtml(entry.endpointLabel)}</p><p>${entry.lastOperationId ? "Rückupload in der Queue verzeichnet" : "Noch kein Rückupload"}${entry.dirty ? " · Lokale Änderungen" : ""}</p>${entry.limitations?.length ? `<details class="operation-error-details"><summary>Verbindungsdetails</summary><textarea class="listing-omissions-text" aria-label="Verbindungsdetails" readonly>${escapeHtml(entry.limitations.join("\n"))}</textarea></details>` : ""}${entry.saveBlocker ? `<p>${escapeHtml(entry.saveBlocker)}</p>` : ""}<p>${entry.replaceBestEffort ? "Rückspeichern ersetzt die Altversion dauerhaft ohne Quarantäne (Best Effort)." : "Rückspeichern ersetzt die Serverdatei nach frischer Konfliktprüfung."}</p><div class="remote-edit-actions"><button class="btn primary" data-save="${index}" ${entry.saveAvailable === false ? "disabled" : ""}>Zurückspeichern</button><button class="btn ghost" data-close="${index}">Bearbeitung beenden</button></div></section>`).join("") : '<p>Keine geöffneten Remote-Bearbeitungen.</p>';
      };
      const busy = value => {
        pending = value;
        for (const button of list.querySelectorAll("button")) {
          const entry = button.hasAttribute("data-save") ? entries[Number(button.dataset.save)] : null;
          button.disabled = value || entry?.saveAvailable === false;
        }
      };
      list.addEventListener("click", async event => {
        if (pending) return;
        const button = event.target.closest("button");
        if (!button) return;
        const saving = button.hasAttribute("data-save");
        const entry = entries[Number(saving ? button.dataset.save : button.dataset.close)];
        if (!entry) return;
        busy(true);
        status.textContent = saving ? "Serveroriginal wird geprüft und Rückupload vorbereitet …" : "Bearbeitung wird beendet …";
        try {
          const result = await invoke(saving ? "save_remote_edit" : "close_remote_edit", { request: { id: entry.id } });
          if (saving) {
            entry.lastOperationId = result;
            entry.saveAvailable = false;
            entry.saveBlocker = "Rückupload eingereiht. Nach seinem Abschluss die Übersicht aktualisieren.";
          } else {
            entries = entries.filter(current => current.id !== entry.id);
            updateBadge();
          }
          refreshQueue();
          if (!status.isConnected) return;
          render();
          status.textContent = saving ? "Rückupload in Queue eingereiht. Fortschritt und Ergebnis stehen in der Operations-Queue." : "Bearbeitung beendet. Die lokale Arbeitskopie bleibt im privaten Cache.";
          (list.querySelector("button") || dismiss).focus();
        } catch (error) {
          if (status.isConnected) status.textContent = errText(error);
          else showError("Remote-Bearbeitung", error);
        } finally {
          busy(false);
        }
      });
      render();
      const bar = $("#modal .modal-buttons");
      bar.replaceChildren();
      const reload = document.createElement("button");
      reload.className = "btn ghost";
      reload.textContent = "Aktualisieren";
      reload.addEventListener("click", async () => {
        if (pending) return;
        busy(true);
        reload.disabled = true;
        try {
          await refresh();
          if (status.isConnected) {
            render();
            status.textContent = "Bearbeitungsstand aktualisiert.";
          }
        } catch (error) {
          if (status.isConnected) status.textContent = errText(error);
        } finally {
          busy(false);
          reload.disabled = false;
        }
      });
      const dismiss = document.createElement("button");
      dismiss.className = "btn ghost";
      dismiss.textContent = "Schließen";
      dismiss.addEventListener("click", () => hideModal(token));
      bar.append(reload, dismiss);
      return { focusTarget: dismiss, onEscape: () => hideModal(token) };
    });
  }

  async function onOpened(snapshot) {
    if (!snapshot.editId) return;
    try { await refresh(); }
    catch (error) { showError("Remote-Bearbeitung", error); return; }
    showDock(`<span class="pill ok">Bearbeitung geöffnet</span>${escapeHtml(snapshot.name)} · Nach dem Speichern in der Anwendung über „Remote-Bearbeitung“ zurückspeichern.`, { closable: true });
  }

  $("#btn-remote-edits").addEventListener("click", open);
  return { onOpened, refresh };
}
