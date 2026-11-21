import { hideModal, replaceModal, showLatestOwnedModal } from "./modal.js";
import { operationLabel } from "./queue-ui.js";
import { invoke } from "./tauri-api.js";
import { $, errText, escapeHtml } from "./ui-utils.js";

/** @typedef {import("./recovery-trash-wire.js").RecoveryOverviewWire} RecoveryOverviewWire */

const queueReasons = {
  outcome_unclear: "Wirkung ungeklärt",
  settlement_pending: "Abschluss prüfen",
  decision_required: "Entscheidung nötig",
  interrupted: "Unterbrochen",
  failed: "Fehlgeschlagen",
  retained: "Zwischenstand oder Sicherung erhalten",
  incomplete: "Mit Auslassungen abgeschlossen",
};

function note(message) {
  return `<p class="quarantine-meta">${escapeHtml(message)}</p>`;
}

function row(title, detail, action = "") {
  return `<div class="quarantine-row">
    <div class="quarantine-main">
      <div class="quarantine-name">${escapeHtml(title)}</div>
      ${note(detail)}
    </div>${action}
  </div>`;
}

/** @param {RecoveryOverviewWire["queue"]} queue */
function queueRows(queue) {
  if (!queue) return note("Die Fensterqueue konnte nicht gelesen werden.");
  if (!queue.length) return note("Keine offenen Befunde.");
  return queue.map(({ entry, reason }, index) => row(
    operationLabel(entry),
    queueReasons[reason] || "Auftrag prüfen",
    `<div class="quarantine-actions"><button class="btn ghost small" type="button"
      data-recovery-queue="${index}">Auftrag öffnen</button></div>`,
  )).join("");
}

/** @param {RecoveryOverviewWire["trash"]} trash */
function trashRows(trash) {
  if (trash?.Err) return note(errText(trash.Err));
  if (!trash?.Ok?.length) return note("Keine verbliebenen Papierkorb-Journale.");
  return trash.Ok.map(journal => row(journal.name, journal.error
    ? `Journal nicht lesbar: ${journal.error}`
    : `${journal.entry_count} protokollierte Einträge · ${journal.effects_unclear ? "Wirkung ungeklärt" : "Journal noch vorhanden"}`)).join("");
}

function quarantineRows(overview) {
  if (overview.message) return note(overview.message);
  if (!overview.runs.length) return note("Keine offenen Quarantänebefunde für dieses Ziel.");
  return overview.runs.map(run => {
    const { automatic, manual } = run.available;
    return row(run.id, `${run.label} · ${automatic} wiederherstellbar oder prüfbar · ${manual} nur manuell rettbar`);
  }).join("") + `<button class="btn ghost small" id="recovery-quarantine" type="button">Quarantäne öffnen</button>`;
}

export function createRecoveryUi({ openQueue, inspectQuarantine }) {
  let generation = 0;

  function render({ data, quarantine, error, focusRefresh = false } = {}) {
    return token => {
      $("#modal .modal").classList.add("quarantine-modal");
      $("#modal-title").textContent = "Wiederherstellung";
      const body = $("#modal-body");
      const close = () => hideModal(token);
      if (error) {
        body.innerHTML = note(error);
      } else if (data) {
        body.innerHTML = `<section class="recovery-section">
            <h3>Operations-Queue dieses Fensters</h3>${queueRows(data.queue)}
          </section>
          <section class="recovery-section">
            <h3>Quarantäne am aktuellen Ziel</h3>
            ${quarantine.target ? note(quarantine.target) : ""}
            ${quarantineRows(quarantine)}
          </section>
          <section class="recovery-section">
            <h3>Papierkorb</h3>${trashRows(data.trash)}
          </section>`;
        body.onclick = event => {
          const button = event.target.closest("[data-recovery-queue]");
          if (button) {
            const selected = data.queue[Number(button.dataset.recoveryQueue)];
            if (selected && close()) void openQueue(selected.entry.operation_id);
          } else if (event.target.closest("#recovery-quarantine")) {
            if (!quarantine.current()) {
              reload(token);
            } else if (close()) {
              void quarantine.open();
            }
          }
        };
      } else {
        body.innerHTML = '<p role="status">Wiederherstellungsbefunde werden gelesen …</p>';
      }
      const bar = $("#modal .modal-buttons");
      bar.innerHTML = `<button class="btn ghost" id="recovery-refresh" type="button">Aktualisieren</button>
        <button class="btn primary" id="recovery-close" type="button">Schließen</button>`;
      $("#recovery-refresh").addEventListener("click", () => reload(token));
      $("#recovery-close").addEventListener("click", close);
      return { focusTarget: $(focusRefresh ? "#recovery-refresh" : "#recovery-close"), onEscape: close };
    };
  }

  function read(token, focusRefresh) {
    const request = ++generation;
    const quarantine = inspectQuarantine().catch(error => ({ message: errText(error) }));
    Promise.all([invoke("inspect_recovery"), quarantine]).then(([data, result]) => {
      if (request === generation) replaceModal(token, render({ data, quarantine: result, focusRefresh }));
    }).catch(error => {
      if (request === generation) replaceModal(token, render({ error: errText(error), focusRefresh }));
    });
  }

  function reload(token) {
    if (replaceModal(token, render({ focusRefresh: true }))) read(token, true);
  }

  function open() {
    showLatestOwnedModal("recovery", token => {
      queueMicrotask(() => read(token, false));
      return render()(token);
    });
  }

  return { open };
}
