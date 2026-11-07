import { cancelOwnedModal, showOwnedModal } from "./modal.js";
import { $, escapeHtml, fmtBytes, fmtEntryDate } from "./ui-utils.js";

function entryHtml(entry, role) {
  const kind = { file: "Datei", directory: "Ordner", symlink: "Symlink", special: "Spezialdatei" };
  const time = entry?.mtime == null ? "Unbekannt" : (fmtEntryDate(entry.mtime) || "Unbekannt");
  return `<section class="conflict-endpoint"><h3>${escapeHtml(role)}</h3>` +
    `<code class="conflict-root">${escapeHtml(entry?.endpoint ?? "Unbekannt")}</code>` +
    `<dl><dt>Vorhanden</dt><dd>${entry?.exists ? "Ja" : "Nein"}</dd>` +
    `<dt>Typ</dt><dd>${entry?.exists ? (kind[entry.kind] || "Unbekannt") : "—"}</dd>` +
    `<dt>Größe</dt><dd>${entry?.exists ? (entry.size == null ? "Unbekannt" : fmtBytes(entry.size)) : "—"}</dd>` +
    `<dt>Geändert</dt><dd>${entry?.exists ? escapeHtml(time) : "—"}</dd></dl></section>`;
}

function limitationsHtml(title, label, values, expanded = false) {
  if (!values.length) return "";
  return `<details class="operation-error-details" ${expanded ? "open" : ""}><summary tabindex="0">${escapeHtml(title)}</summary><textarea class="listing-omissions-text" aria-label="${escapeHtml(label)}" tabindex="0" readonly>${escapeHtml(values.join("\n"))}</textarea></details>`;
}

/** Gemeinsam für Sync und Copy/Move. Alle Pfade und Optionen kommen aus dem
 * Backend; Rollenwechsel betrifft ausschließlich die sichtbaren Spalten. */
export function conflictChoiceModal({ title, conflicts, leftRole = "Quelle", rightRole = "Ziel", choices, note = "", replacementBlockers = [], sourceReadLimitations = [], loadChoices = null, signal = null }) {
  return new Promise(resolve => {
    let modalToken = null;
    let finished = false;
    const finish = value => {
      if (finished) return;
      finished = true;
      signal?.removeEventListener("abort", abort);
      cancelOwnedModal(modalToken);
      resolve(value);
    };
    const abort = () => finish(null);
    if (signal?.aborted) { resolve(null); return; }
    modalToken = showOwnedModal("conflict", token => {
      modalToken = token;
      $("#modal .modal").classList.add("conflict-modal");
      $("#modal-title").textContent = title;
      const body = $("#modal-body");
      body.innerHTML = `<p class="modal-description">${escapeHtml(note)}</p>` +
        limitationsHtml("Quellgrenzen und Auslassungen", "Quellgrenzen und Auslassungen", sourceReadLimitations, true) +
        limitationsHtml("Ersetzen nicht verfügbar", "Grenzen beim Ersetzen", replacementBlockers, true) +
        `<div class="conflict-list">${conflicts.map((conflict, index) => {
          const content = `<div class="conflict-columns">${entryHtml(conflict.left || conflict.source, leftRole)}${entryHtml(conflict.right || conflict.target, rightRole)}</div>`;
          return conflicts.length === 1
            ? `<h3 class="conflict-name">${escapeHtml(conflict.relative_name)}</h3>${content}`
            : `<details ${index === 0 ? "open" : ""}><summary tabindex="0">${escapeHtml(conflict.relative_name)}</summary>${content}</details>`;
        }).join("")}</div><div class="conflict-decisions"></div>`;
      const presentation = body.firstElementChild;
      // Beim Rendern ist die Hülle noch verborgen. Erst nach ihrem Öffnen lässt
      // sich ein alter Scrollstand zuverlässig zurücksetzen; der Knotentest
      // verhindert, dass dies später einen anderen Dialog trifft.
      queueMicrotask(() => {
        if (presentation?.isConnected && body.firstElementChild === presentation) {
          body.scrollTop = 0;
          $("#modal .modal").scrollTop = 0;
        }
      });
      const bar = $("#modal .modal-buttons");
      bar.replaceChildren();
      const cancel = document.createElement("button");
      cancel.className = "btn ghost";
      cancel.textContent = "Abbrechen";
      cancel.addEventListener("click", () => finish(null));
      bar.append(cancel);
      const renderChoices = values => {
        const container = body.querySelector(".conflict-decisions");
        const focusedValue = container.contains(document.activeElement) ? document.activeElement.dataset.choice : null;
        container.replaceChildren();
        for (const [index, choice] of values.entries()) {
          const button = document.createElement("button");
          button.className = `conflict-decision ${choice.danger ? "danger" : ""} ${choice.wide ? "conflict-decision-wide" : ""}`;
          button.disabled = Boolean(choice.disabled);
          button.dataset.choice = choice.value;
          button.setAttribute("aria-label", choice.label);
          const label = document.createElement("strong");
          label.textContent = choice.label;
          const detail = document.createElement("span");
          detail.textContent = choice.explanation || "";
          detail.id = `conflict-choice-description-${index}`;
          button.setAttribute("aria-describedby", detail.id);
          button.append(label, detail);
          button.addEventListener("click", () => finish(choice.value));
          body.querySelector(".conflict-decisions").append(button);
        }
        if (focusedValue) {
          const replacement = [...container.children].find(button => button.dataset.choice === focusedValue && !button.disabled);
          (replacement || cancel).focus();
        }
      };
      renderChoices(choices);
      if (loadChoices) {
        const status = document.createElement("p");
        status.className = "modal-description";
        status.setAttribute("role", "status");
        status.textContent = "Verfügbare Aktionen und neue Namen werden geprüft …";
        body.querySelector(".conflict-decisions").before(status);
        Promise.resolve().then(loadChoices).then(updated => {
          if (finished || !status.isConnected) return;
          if (!updated) { finish(null); return; }
          status.textContent = "";
          renderChoices(updated.choices);
          if (updated.replacementBlockers?.length) {
            status.textContent = updated.replacementBlockers.join("; ");
          }
        }).catch(error => {
          if (!finished && status.isConnected) {
            status.textContent = `Prüfung fehlgeschlagen: ${error?.message || error}. Überspringen oder abbrechen bleibt möglich.`;
          }
        });
      }
      return { focusTarget: cancel, onEscape: () => finish(null) };
    });
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
  });
}
