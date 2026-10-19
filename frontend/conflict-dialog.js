import { hideModal, showOwnedModal } from "./modal.js";
import { $, escapeHtml, fmtBytes } from "./ui-utils.js";

function entryHtml(entry, role) {
  const kind = { file: "Datei", directory: "Ordner", symlink: "Symlink", special: "Spezialdatei" };
  const time = entry?.mtime == null ? "Unbekannt" : new Date(entry.mtime * 1000).toLocaleString();
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
export function conflictChoiceModal({ title, conflicts, leftRole = "Quelle", rightRole = "Ziel", choices, note = "", replacementBlockers = [], sourceReadLimitations = [] }) {
  return new Promise(resolve => {
    showOwnedModal("conflict", token => {
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
      let finished = false;
      const finish = value => {
        if (finished || !hideModal(token)) return;
        finished = true;
        resolve(value);
      };
      const cancel = document.createElement("button");
      cancel.className = "btn ghost";
      cancel.textContent = "Abbrechen";
      cancel.addEventListener("click", () => finish(null));
      bar.append(cancel);
      for (const [index, choice] of choices.entries()) {
        const button = document.createElement("button");
        button.className = `conflict-decision ${choice.danger ? "danger" : ""} ${choice.wide ? "conflict-decision-wide" : ""}`;
        button.disabled = Boolean(choice.disabled);
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
      return { focusTarget: cancel, onEscape: () => finish(null) };
    });
  });
}
