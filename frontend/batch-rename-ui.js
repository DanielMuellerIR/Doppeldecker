// Der Dialog bearbeitet nur Regeln und zeigt Backendzeilen. Ausführbare Namen,
// Identitäten und die einmalige Freigabe bleiben im fenstergebundenen Backendplan.
import { invoke, listen } from "./tauri-api.js";
import { hideModal, showOwnedModal } from "./modal.js";
import { $, errText, isEntryNameSafe } from "./ui-utils.js";

const STATUS = { ready: "Bereit", unchanged: "Unverändert", blocked: "Nicht ausführbar", renamed: "Umbenannt", partially_renamed: "Teilweise umbenannt", failed_unchanged: "Fehler · unverändert", outcome_unclear: "Ausgang unklar", cancelled: "Abgebrochen · unverändert" };

export function createBatchRenameUi({ panes, selectedEntries, endpointFor, showDock, refreshPanes }) {
  let session = null;
  void listen("batch-rename-progress", ({ payload }) => {
    if (session?.running && session.planId === payload.planId) session.progress(payload);
  }).catch(() => {});

  function selectedInPaneOrder(side) {
    const selected = new Set(selectedEntries(side));
    return panes[side].entries.filter(entry => selected.has(entry));
  }
  function open(side) {
    const pane = panes[side];
    const selected = selectedInPaneOrder(side);
    const names = selected.filter(isEntryNameSafe).map(entry => entry.name);
    const omitted = selected.length - names.length;
    if (!names.length) {
      showDock('<span class="pill none">Stapelumbenennen</span>Bitte Einträge mit eindeutig bedienbaren Namen auswählen.', { closable: true });
      return;
    }
    const endpoint = endpointFor(pane);
    const scopeKeys = ["provider", "profileId", "profileRevision", "sessionGeneration", "path", "navigationRevision"];
    const scope = Object.fromEntries(scopeKeys.map(key => [key, pane[key]]));
    const tabId = pane.tabs?.active?.id;
    const scopeMatches = () => {
      const current = panes[side]; const selection = selectedInPaneOrder(side);
      const currentNames = selection.filter(isEntryNameSafe).map(entry => entry.name);
      return current && current.tabs?.active?.id === tabId && scopeKeys.every(key => current[key] === scope[key])
        && selection.length === selected.length && currentNames.length === names.length && currentNames.every((name, index) => name === names[index]);
    };
    showOwnedModal("batch-rename", token => {
      let closed = false;
      let generation = 0;
      let preparing = false;
      let running = false;
      let cancelling = false;
      let planId = null;
      let requestId = null;
      let stale = false;
      let completed = false;
      let rows = [];
      const ownSession = { running: false, planId: null, progress: payload => { status.textContent = `${payload.done} von ${payload.total} Einträgen geprüft. Bereits erfolgte Namensänderungen bleiben erhalten.`; } };
      session = ownSession;
      $("#modal .modal").classList.add("batch-rename-modal");
      $("#modal-title").textContent = "Stapelumbenennen";
      const body = $("#modal-body"); body.replaceChildren();
      const description = document.createElement("p");
      description.className = "batch-rename-description";
      description.textContent = `${names.length} ${names.length === 1 ? "ausgewählter Eintrag" : "ausgewählte Einträge"} · ${endpoint.root}. Der Stapel ist keine atomare Gesamtaktion. Bereits ausgeführte Namensänderungen bleiben bei Fehlern oder Abbruch erhalten. Mehrstufige lokale Zyklen können Einträge unter Zwischennamen hinterlassen; kein automatischer Rücktausch und keine automatische Wiederaufnahme nach Prozessende.${omitted ? ` ${omitted} nicht eindeutig dekodierte Namen werden ausgelassen.` : ""}`;
      body.append(description);
      const limitations = document.createElement("section"); limitations.className = "batch-rename-limitations";
      limitations.setAttribute("aria-label", "Ausführungsgrenzen"); limitations.hidden = true; body.append(limitations);
      const renderLimitations = value => {
        limitations.replaceChildren();
        limitations.hidden = !value.executionClass && !value.limitations?.length;
        if (value.executionClass) {
          const title = document.createElement("strong");
          title.textContent = value.executionClass === "best_effort" ? "Ausführungsklasse: Best Effort" : "Ausführungsklasse: abgesicherter Zielschutz";
          limitations.append(title);
        }
        const messages = value.limitations || [];
        const appendMessage = (parent, message) => { const line = document.createElement("p"); line.textContent = message; parent.append(line); };
        for (const message of messages.slice(0, 2)) appendMessage(limitations, message);
        if (messages.length > 2) {
          const details = document.createElement("details");
          const summary = document.createElement("summary"); summary.tabIndex = 0;
          summary.textContent = `Weitere Ausführungsgrenzen (${messages.length - 2})`; details.append(summary);
          for (const message of messages.slice(2)) appendMessage(details, message);
          limitations.append(details);
        }
      };
      const form = document.createElement("div"); form.className = "batch-rename-fields";
      const fields = {};
      for (const [key, label] of [["find", "Suchen"], ["replacement", "Ersetzen durch"], ["prefix", "Präfix"], ["suffix", "Suffix"]]) {
        const group = document.createElement("label"); group.textContent = label;
        const input = document.createElement("input"); input.type = "text"; input.className = "modal-input";
        input.maxLength = 4096; input.autocomplete = "off"; input.spellcheck = false;
        group.append(input); form.append(group); fields[key] = input;
      }
      body.append(form);
      const extensionLabel = document.createElement("label"); extensionLabel.className = "batch-rename-extension";
      const extension = document.createElement("input"); extension.type = "checkbox"; extension.checked = true;
      extensionLabel.append(extension, document.createTextNode("Dateiendung beibehalten")); body.append(extensionLabel);
      const trimLabel = document.createElement("label"); trimLabel.className = "batch-rename-extension";
      const trimOuterSpaces = document.createElement("input"); trimOuterSpaces.type = "checkbox";
      trimLabel.append(trimOuterSpaces, document.createTextNode("Äußere Leerzeichen entfernen")); body.append(trimLabel);
      const trimHelp = document.createElement("p"); trimHelp.className = "batch-rename-help";
      trimHelp.textContent = "Nur Leerzeichen am Anfang und Ende des ursprünglichen Namens."; body.append(trimHelp);
      const numberingGroup = document.createElement("fieldset"); numberingGroup.className = "batch-rename-numbering";
      const numberingLegend = document.createElement("legend"); numberingLegend.textContent = "Nummerierung"; numberingGroup.append(numberingLegend);
      const numberingLabel = document.createElement("label"); numberingLabel.className = "batch-rename-numbering-toggle";
      const numbering = document.createElement("input"); numbering.type = "checkbox";
      numberingLabel.append(numbering, document.createTextNode("Nummerierung verwenden")); numberingGroup.append(numberingLabel);
      const numberingDetails = document.createElement("div"); numberingDetails.hidden = true;
      const numberingForm = document.createElement("div"); numberingForm.className = "batch-rename-numbering-fields";
      const numberingFields = {};
      for (const [key, label, value, min, max] of [["start", "Start", 1, 0, 4294967295], ["step", "Schritt", 1, 1, 4294967295], ["minDigits", "Mindeststellen", 3, 1, 20]]) {
        const group = document.createElement("label"); group.textContent = label;
        const input = document.createElement("input"); input.type = "number"; input.className = "modal-input";
        input.min = min; input.max = max; input.step = 1; input.required = true; input.value = value; input.disabled = true;
        group.append(input); numberingForm.append(group); numberingFields[key] = input;
      }
      const positionLabel = document.createElement("label"); positionLabel.textContent = "Position";
      const position = document.createElement("select"); position.className = "modal-input"; position.disabled = true;
      for (const [value, title] of [["before", "Vor dem Namen"], ["after", "Nach dem Namen"]]) {
        const option = document.createElement("option"); option.value = value; option.textContent = title; position.append(option);
      }
      numberingFields.position = position; positionLabel.append(position); numberingForm.append(positionLabel); numberingDetails.append(numberingForm);
      const numberingHelp = document.createElement("p"); numberingHelp.className = "batch-rename-help";
      numberingHelp.textContent = "Die beim Öffnen sichtbare Reihenfolge bestimmt die Nummern. Jede Vorschauzeile zählt, auch nicht ausführbare. Die Nummer steht mit _ vor oder nach dem bearbeiteten Namen; die beibehaltene Dateiendung folgt zuletzt. Mindeststellen werden mit führenden Nullen aufgefüllt.";
      numberingDetails.append(numberingHelp); numberingGroup.append(numberingDetails); body.append(numberingGroup);
      const help = document.createElement("p"); help.className = "batch-rename-help";
      help.textContent = "Suchen/Ersetzen unterscheidet Groß-/Kleinschreibung und verwendet keine regulären Ausdrücke. Ein leeres Suchfeld ersetzt keinen Namensteil. Beibehalten wird bei Dateien die letzte Endung; Ordnernamen werden vollständig behandelt.";
      body.append(help);
      const status = document.createElement("p"); status.className = "batch-rename-status"; status.setAttribute("role", "status");
      status.textContent = "Regeln eingeben und die konkrete Vorschau erstellen."; body.append(status);
      const tableRegion = document.createElement("div"); tableRegion.className = "batch-rename-results";
      tableRegion.setAttribute("role", "region"); tableRegion.setAttribute("aria-label", "Umbenennungen"); tableRegion.tabIndex = 0;
      body.append(tableRegion);
      const bar = $("#modal .modal-buttons"); bar.replaceChildren();
      const closeButton = document.createElement("button"); closeButton.className = "btn ghost"; closeButton.textContent = "Schließen";
      const previewButton = document.createElement("button"); previewButton.className = "btn ghost"; previewButton.textContent = "Vorschau erstellen";
      const executeButton = document.createElement("button"); executeButton.className = "btn primary"; executeButton.textContent = "Bereite Einträge umbenennen"; executeButton.disabled = true;
      bar.append(closeButton, previewButton, executeButton);

      const discard = id => { if (id) void invoke("discard_batch_rename", { planId: id }).catch(() => {}); };
      const updateControls = () => {
        for (const field of [...Object.values(fields), extension, trimOuterSpaces, numbering]) field.disabled = running || stale;
        numberingDetails.hidden = !numbering.checked;
        for (const field of Object.values(numberingFields)) field.disabled = running || stale || !numbering.checked;
        previewButton.disabled = preparing || running || stale;
        executeButton.hidden = completed;
        executeButton.disabled = preparing || running || stale || !planId || !rows.some(row => row.status === "ready");
        const ready = rows.filter(row => row.status === "ready").length;
        executeButton.textContent = ready === 1 ? "1 Eintrag umbenennen" : `${ready || "Bereite"} Einträge umbenennen`;
        closeButton.textContent = running ? cancelling ? "Abbruch angefordert …" : "Abbrechen" : "Schließen";
        closeButton.disabled = cancelling;
        // Das Sperren des gerade betätigten Knopfs kann den Browserfokus auf
        // body zurücksetzen. Escape und Tab müssen im Modal bleiben.
        const modal = $("#modal .modal");
        if (!modal.contains(document.activeElement) || document.activeElement.matches?.(":disabled")) {
          (closeButton.disabled ? tableRegion : closeButton).focus({ preventScroll: true });
        }
      };
      const renderRows = () => {
        tableRegion.replaceChildren();
        if (!rows.length) return;
        const table = document.createElement("table");
        const head = document.createElement("thead"); const header = document.createElement("tr");
        for (const title of ["Bisheriger Name", "Neuer Name", "Ergebnis"]) { const cell = document.createElement("th"); cell.scope = "col"; cell.textContent = title; header.append(cell); }
        head.append(header); table.append(head); const content = document.createElement("tbody");
        for (const row of rows) {
          const line = document.createElement("tr"); line.dataset.status = row.status;
          for (const value of [row.oldName, row.newName]) { const cell = document.createElement("td"); cell.textContent = value; line.append(cell); }
          const result = document.createElement("td"); const label = document.createElement("strong"); label.textContent = STATUS[row.status] || row.status;
          const detail = document.createElement("span"); detail.textContent = row.message; result.append(label);
          if (row.currentName != null && row.status !== "outcome_unclear") {
            const current = document.createElement("span"); current.textContent = `Zuletzt bestätigter Name: ${row.currentName}`; result.append(current);
          }
          if (row.message && row.message !== label.textContent) result.append(detail);
          line.append(result); content.append(line);
        }
        table.append(content); tableRegion.append(table);
      };
      const invalidate = () => {
        generation += 1; discard(requestId); discard(planId); requestId = null; planId = null; completed = false; rows = []; renderRows();
        status.textContent = "Eingaben geändert. Vor dem Umbenennen die Vorschau erneut erstellen."; updateControls();
      };
      for (const field of [...Object.values(fields), extension, trimOuterSpaces, numbering, ...Object.values(numberingFields)]) {
        field.addEventListener(field === position ? "change" : "input", invalidate);
      }
      const cancel = async () => {
        if (!running || !planId || cancelling) return;
        cancelling = true; updateControls();
        status.textContent = `${stale ? "Auswahl, Pane oder Profil geändert. " : ""}Abbruch angefordert. Die gerade begonnene Einzelwirkung wird noch bilanziert; weitere Wirkungen werden gestoppt. Bereits bewegte Einträge können unter einem Zwischennamen bleiben. Die Bilanz zeigt bestätigte Namen; kein automatischer Rücktausch.`;
        try { await invoke("cancel_batch_rename", { planId }); }
        catch (error) { cancelling = false; status.textContent = `Abbruch nicht bestätigt: ${errText(error)}`; updateControls(); }
      };
      const close = () => {
        if (running) { void cancel(); return; }
        if (!hideModal(token)) return;
        closed = true; clearInterval(scopeTimer); generation += 1; discard(requestId); discard(planId); planId = null;
        if (session === ownSession) session = null;
      };
      const checkScope = () => {
        if (closed || stale || scopeMatches()) return !closed && !stale;
        stale = true; generation += 1; discard(requestId); discard(planId);
        if (running) { void cancel(); }
        else {
          planId = null;
          if (!completed) { rows = []; renderRows(); }
          status.textContent += " Auswahl, Pane oder Profil geändert. Für einen neuen Stapel bitte schließen und erneut öffnen.";
          updateControls();
        }
        return false;
      };
      const scopeTimer = setInterval(checkScope, 200);
      closeButton.addEventListener("click", close);
      previewButton.addEventListener("click", async () => {
        if (preparing || running || !checkScope()) return;
        const invalidNumber = numbering.checked && Object.values(numberingFields).find(field => !field.checkValidity());
        if (invalidNumber) {
          invalidate(); status.textContent = "Bitte die markierte Nummerierungseingabe korrigieren.";
          invalidNumber.reportValidity(); return;
        }
        discard(requestId); discard(planId); planId = null; completed = false; rows = []; renderRows();
        const requestGeneration = ++generation;
        const ownRequestId = crypto.randomUUID(); requestId = ownRequestId;
        preparing = true; status.textContent = "Konkrete Namen und Kollisionen werden geprüft …"; updateControls();
        const options = Object.fromEntries(Object.entries(fields).map(([key, input]) => [key, input.value])); options.preserveExtension = extension.checked;
        if (trimOuterSpaces.checked) options.trimOuterSpaces = true;
        if (numbering.checked) options.numbering = {
          start: numberingFields.start.valueAsNumber,
          step: numberingFields.step.valueAsNumber,
          minDigits: numberingFields.minDigits.valueAsNumber,
          position: position.value,
        };
        try {
          const preview = await invoke("prepare_batch_rename", { request: { requestId: ownRequestId, endpoint, names, options } });
          if (closed || requestGeneration !== generation || !checkScope()) { discard(preview.planId); return; }
          if (preview.requestId !== ownRequestId) { discard(preview.planId); throw new Error("Die Stapelantwort gehört zu einer anderen Anfrage. Bitte neu vorbereiten."); }
          renderLimitations(preview);
          planId = preview.planId; rows = preview.rows; renderRows();
          const ready = rows.filter(row => row.status === "ready").length;
          status.textContent = `${ready} von ${rows.length} Einträgen ${ready === 1 ? "kann" : "können"} umbenannt werden. Nicht ausführbare und unveränderte Zeilen werden ausgelassen. Vor jeder Einzelwirkung werden Quelle und Ziel erneut geprüft.`
            + (preview.executionClass ? " Für die Bindung und den Ergebnisschutz gelten die angezeigten Ausführungsgrenzen." : " Ein gleichzeitiger Quelltausch bleibt bis zum Identitäts-Readback möglich.");
          if (rows.length) { tableRegion.focus({ preventScroll: true }); tableRegion.scrollIntoView({ block: "nearest" }); }
        } catch (error) { if (!closed && requestGeneration === generation) status.textContent = errText(error); }
        finally { preparing = false; if (!closed) updateControls(); }
      });
      executeButton.addEventListener("click", async () => {
        if (running || preparing || !checkScope() || !planId || !rows.some(row => row.status === "ready")) return;
        const executingId = planId;
        running = true; ownSession.running = true; ownSession.planId = executingId; cancelling = false;
        status.textContent = "Stapel wird ausgeführt. Abbrechen stoppt vor der nächsten Einzelwirkung."; updateControls();
        try {
          const report = await invoke("execute_batch_rename", { planId: executingId });
          completed = true; rows = report.rows; renderRows(); renderLimitations(report);
          status.textContent = `${report.renamed} umbenannt · ${report.partiallyRenamed || 0} teilweise umbenannt · ${report.failedUnchanged} Fehler ohne Namensänderung · ${report.outcomeUnclear} unklare Ausgänge · ${report.cancelled} abgebrochen. Nicht ausführbare und unveränderte Vorschauzeilen bleiben erhalten.${report.outcomeUnclear ? " Die Namen in den Spalten und zusätzliche Namenshinweise der unklaren Zeilen vor erneutem Versuch prüfen." : ""}${stale ? " Auswahl, Pane oder Profil wurde geändert; für einen neuen Stapel bitte neu öffnen." : ""}`;
        } catch (error) {
          completed = true;
          status.textContent = `Abschluss nicht bestätigt: ${errText(error)}. Mögliche Teilwirkungen prüfen; es wird nicht automatisch wiederholt. Die Tabelle zeigt die letzte Vorschau.`;
        } finally {
          running = false; ownSession.running = false; planId = null; cancelling = false; updateControls();
          void refreshPanes({ affectedEndpoints: [{ ...endpoint, path: endpoint.root }] });
        }
      });
      return { focusTarget: fields.find, onEscape: close };
    });
  }
  return { open };
}
