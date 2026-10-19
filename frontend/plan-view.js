// Darstellung einer bereits vom Kern gebundenen Sync-Vorschau. Dieses Modul plant
// und mutiert nichts; es übersetzt nur DTOs in die sichtbare Bilanz.

import { $, escapeHtml, fmtBytes, fmtTransferBytes } from "./ui-utils.js";
import { createSyncComparisonView } from "./sync-comparison.js";

export function createPlanView({ metadataClassLabels, metadataReportHtml, updateMetadataNote, showDock }) {
  const comparisonView = createSyncComparisonView({
    root: $("#sync-comparison"),
    list: $("#sync-comparison-list"),
    categorySummary: $("#sync-comparison-categories"),
    metadataClassLabels,
  });

  function planSummaryHtml(plan) {
    const parts = [];
    if (plan.copy_count) parts.push(`<span class="pill copy">${plan.copy_count} kopieren</span>`);
    if (plan.update_count) parts.push(`<span class="pill update">${plan.update_count} aktualisieren</span>`);
    if (plan.metadata_count) parts.push(`<span class="pill update">${plan.metadata_count} Metadaten anpassen</span>`);
    if (plan.delete_count) parts.push(`<span class="pill delete">${plan.delete_count} entfernen · ${fmtBytes(plan.delete_bytes)}</span>`);
    if (plan.skip_count) parts.push(`<span class="pill none">${plan.skip_count} sicher ausgelassen</span>`);
    const hashPart = plan.hashed_files
      ? `<span class="pill none">${plan.hashed_files} Hash-Lesevorgänge · ${fmtTransferBytes(plan.bytes_hashed)}</span>`
      : "";
    const metadataPart = metadataReportHtml(plan.metadata_report);
    if (parts.length === 0) return `<span class="pill none">Bereits synchron</span>${hashPart}${metadataPart}`;
    if (hashPart) parts.push(hashPart);
    if (metadataPart) parts.push(metadataPart);
    const transfer = plan.bytes_to_transfer === 0 && plan.metadata_count
      ? "ohne Datenübertragung"
      : `${fmtTransferBytes(plan.bytes_to_transfer)} zu übertragen`;
    return parts.join("") + `<span class="dim">${transfer}</span>`;
  }

  function renderActionList(actions, report = null, list = $("#actions-list")) {
    const label = {
      CopyFile: "kopieren", UpdateFile: "aktual.", CreateHardlink: "Hardlink",
      DeleteFile: "Quarantäne", DeleteDir: "Quarantäne", CreateDir: "Ordner+",
      SetMode: "Rechte", SetDirectoryMetadata: "Metadaten",
      SkipSymlink: "Symlink ausgelassen", SkipConflict: "Konflikt ausgelassen",
      SkipUnsupported: "Spezialdatei ausgelassen",
    };
    const cssClass = {
      CopyFile: "copy", UpdateFile: "update", CreateHardlink: "link",
      DeleteFile: "delete", DeleteDir: "delete", CreateDir: "copy",
      SetMode: "update", SetDirectoryMetadata: "update", SkipSymlink: "skip",
      SkipConflict: "skip", SkipUnsupported: "skip",
    };
    const actionHtml = actions.slice(0, 500).map((action) => {
      const transfer = action.metadata;
      const classes = transfer ? [
        transfer.owner && "Owner", transfer.acl && "Acl",
        transfer.normal_mode && "NormalMode",
        transfer.special_mode_bits && "SpecialModeBits",
        transfer.xattrs && "Xattrs", transfer.resource_fork && "ResourceFork",
        transfer.sparse_extents && "SparseExtents", transfer.hardlinks && "Hardlinks",
      ].filter(Boolean).map((name) => metadataClassLabels[name]).join(", ") : "";
      const suffix = classes
        ? `<small class="metadata-detail">${escapeHtml(classes)}</small>`
        : "";
      return `<li><span class="tag ${cssClass[action.type] || "skip"}">${label[action.type] || action.type}</span>` +
        `<span class="a-path">${escapeHtml(action.path)}${suffix}</span></li>`;
    }).join("") + (actions.length > 500
      ? `<li><span class="a-path">… und ${actions.length - 500} weitere</span></li>`
      : "");
    const issueHtml = (report?.issues || []).map((issue) =>
      `<li><span class="tag skip">ausgelassen</span><span class="a-path">${escapeHtml(issue.path)}` +
      `<small class="metadata-detail">${escapeHtml(metadataClassLabels[issue.class] || issue.class)} · ${escapeHtml(issue.reason)}</small></span></li>`
    ).join("");
    list.innerHTML = actionHtml + issueHtml;
  }

  function showPlan(plan) {
    updateMetadataNote(plan.metadata_report);
    // Im offenen Sync-Modus ist die Vorschau die Hauptansicht; sonst fällt die
    // Anzeige wie bisher ins Dock (z. B. Realmodus-Vorbereitung aus der Queue).
    const syncView = $("#sync-view");
    if (syncView && !syncView.hidden) {
      $("#sync-hint").hidden = true;
      $("#sync-summary").innerHTML = planSummaryHtml(plan);
      const hasComparison = Array.isArray(plan.comparison?.rows) && plan.comparison.rows.length > 0;
      $("#sync-actions").hidden = hasComparison;
      if (hasComparison) {
        comparisonView.render(plan.comparison, plan.metadata_report);
      } else {
        comparisonView.render(null);
        renderActionList(plan.actions, plan.metadata_report, $("#sync-actions"));
      }
      return;
    }
    showDock(planSummaryHtml(plan), { showList: plan.actions.length > 0 });
    renderActionList(plan.actions, plan.metadata_report);
  }

  return { planSummaryHtml, showPlan };
}
