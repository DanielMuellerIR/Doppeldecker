import { hideModal, showOwnedModal } from "./modal.js";
import { $, escapeHtml } from "./ui-utils.js";

export function duplicateDialog(preview) {
  return new Promise(resolve => {
    showOwnedModal("duplicate", token => {
      $("#modal .modal").classList.add("duplicate-modal");
      $("#modal-title").textContent = "Lokal duplizieren";
      $("#modal-body").innerHTML =
        `<p>Die Originale bleiben erhalten. Die neuen Kopien entstehen im selben Ordner.</p>` +
        `<code class="duplicate-parent">${escapeHtml(preview.parent)}</code>` +
        `<div class="duplicate-mapping-scroll" role="region" aria-label="Namenszuordnung" tabindex="0"><table class="duplicate-mappings"><thead><tr><th scope="col">Original</th><th scope="col">Duplikat</th></tr></thead>` +
        `<tbody>${preview.mappings.map(mapping => `<tr><td>${escapeHtml(mapping.sourceName)}</td><td>${escapeHtml(mapping.targetName)}</td></tr>`).join("")}</tbody></table></div>`;
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
      cancel.addEventListener("click", () => finish(false));
      const confirm = document.createElement("button");
      confirm.className = "btn primary";
      confirm.textContent = "Duplizieren";
      confirm.addEventListener("click", () => finish(true));
      bar.append(cancel, confirm);
      return { focusTarget: cancel, onEscape: () => finish(false) };
    });
  });
}
