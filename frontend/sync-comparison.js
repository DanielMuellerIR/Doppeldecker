// Virtualisierte, zweispaltige Darstellung des plan-gebundenen Vergleichs-DTOs.
// Dieses Modul bewertet keine Datei neu: Pfad, Tiefe, Metadaten und Wirkung stammen
// vollständig aus dem Core-Plan, der auch vom Realmodus-Digest geschützt wird.

import {
  escapeHtml,
  fmtDate,
  fmtEntryPermissions,
  fmtEntrySize,
} from "./ui-utils.js";

const ROW_HEIGHT = 68;
const VIRTUALIZE_AFTER = 80;
const OVERSCAN = 6;

function virtualSpacer(height) {
  if (height <= 0) return "";
  // Die CSP verbietet Inline-Styles. Wie in den Pane-Listen hält deshalb ein
  // leeres SVG den nicht gerenderten Bereich frei.
  return `<div class="virtual-spacer" aria-hidden="true"><svg width="1" height="${Math.ceil(height)}" aria-hidden="true"></svg></div>`;
}

function kindLabel(entry) {
  return {
    Directory: "Ordner",
    File: entry?.extension ? `${String(entry.extension).toLocaleUpperCase("de-DE")}-Datei` : "Datei",
    Symlink: "Symbolischer Link",
    Unsupported: "Spezialdatei",
  }[entry?.kind] || "Eintrag";
}

function metadataText(entry) {
  if (!entry) return "Nicht vorhanden";
  const values = [kindLabel(entry)];
  if (entry.kind === "File") values.push(fmtEntrySize(entry.size));
  if (entry.mtime != null) values.push(fmtDate(entry.mtime));
  if (entry.owner) values.push(entry.owner);
  if (entry.group) values.push(entry.group);
  const permissions = fmtEntryPermissions(entry.mode);
  if (permissions) values.push(permissions);
  return values.filter(Boolean).join(" · ");
}

function actionLabel(action) {
  return {
    Add: "＋",
    Update: "⟳",
    Quarantine: "Quarantäne",
    Skip: "ausgelassen",
  }[action] || action;
}

function actionAriaLabel(action) {
  return {
    Add: "Hinzufügen",
    Update: "Aktualisieren",
    Quarantine: "In Quarantäne verschieben",
    Skip: "Sicher auslassen",
  }[action] || action;
}

function affectedAriaLabel(side) {
  return {
    Source: "Quelle betroffen",
    Target: "Ziel betroffen",
    Neither: "keine Seite wird verändert",
  }[side] || "betroffene Seite unbekannt";
}

function badgeHtml(row, side) {
  const affected = row.affected_side === side;
  // Eine Auslassung verändert keine Seite, braucht aber weiterhin einen sichtbaren
  // Status. Er steht neutral in der Zielspalte, nicht als behauptete Zielwirkung.
  const neutralSkip = row.action === "Skip" && side === "Target";
  if (!affected && !neutralSkip) return "";
  const css = row.action.toLowerCase();
  return `<span class="sync-diff-badge ${css}" aria-label="${escapeHtml(actionAriaLabel(row.action))}">${escapeHtml(actionLabel(row.action))}</span>`;
}

function indentHtml(depth) {
  const width = Math.max(0, Math.min(Number(depth) || 0, 64)) * 14;
  return width
    ? `<span class="sync-diff-indent" aria-hidden="true"><svg width="${width}" height="1" aria-hidden="true"></svg></span>`
    : "";
}

function sideHtml(row, side, issues, metadataClassLabels) {
  const entry = row[side.toLowerCase()] || null;
  const pathParts = String(row.path || "").split("/");
  const name = pathParts.at(-1) || row.path || "—";
  const context = (row.ancestors || []).join(" / ");
  const issueText = side === "Target" && issues.length
    ? issues.map(issue => `${metadataClassLabels[issue.class] || issue.class}: ${issue.reason}`).join(" · ")
    : "";
  return `<div class="sync-diff-side${entry ? "" : " ghost"}" data-side="${side.toLowerCase()}">
    ${indentHtml(row.depth)}
    <span class="sync-diff-entry">
      ${context ? `<small class="sync-diff-context">${escapeHtml(context)}</small>` : ""}
      <span class="sync-diff-name">${escapeHtml(name)}</span>
      <small class="sync-diff-meta">${escapeHtml(metadataText(entry))}</small>
      ${issueText ? `<small class="sync-diff-issue" title="${escapeHtml(issueText)}">⚠ ${escapeHtml(issueText)}</small>` : ""}
    </span>
    ${badgeHtml(row, side)}
  </div>`;
}

function rowHtml(row, index, total, selected, issues, metadataClassLabels) {
  const label = `${row.path}: ${actionAriaLabel(row.action)}, ${affectedAriaLabel(row.affected_side)}`;
  return `<div id="sync-comparison-option-${index}" class="sync-diff-row${selected ? " selected" : ""}"
    data-i="${index}" role="option" aria-selected="${selected}" aria-posinset="${index + 1}"
    aria-setsize="${total}" aria-label="${escapeHtml(label)}">
    ${sideHtml(row, "Source", issues, metadataClassLabels)}
    ${sideHtml(row, "Target", issues, metadataClassLabels)}
  </div>`;
}

export function createSyncComparisonView({ root, list, categorySummary, metadataClassLabels }) {
  let rows = [];
  let selected = 0;
  let issuesByPath = new Map();

  function ensureVisible(index) {
    const top = index * ROW_HEIGHT;
    const bottom = top + ROW_HEIGHT;
    if (top < list.scrollTop) list.scrollTop = top;
    else if (bottom > list.scrollTop + list.clientHeight) {
      list.scrollTop = Math.max(0, bottom - list.clientHeight);
    }
  }

  function renderRows() {
    const total = rows.length;
    if (!total) {
      list.innerHTML = "";
      list.removeAttribute("aria-activedescendant");
      list.dataset.virtualized = "false";
      return;
    }
    selected = Math.max(0, Math.min(selected, total - 1));
    const virtualized = total > VIRTUALIZE_AFTER;
    const firstVisible = Math.floor(list.scrollTop / ROW_HEIGHT);
    const visibleCount = Math.ceil((list.clientHeight || 420) / ROW_HEIGHT);
    const start = virtualized ? Math.max(0, firstVisible - OVERSCAN) : 0;
    const end = virtualized ? Math.min(total, firstVisible + visibleCount + OVERSCAN) : total;
    // `aria-activedescendant` darf nie auf eine wegvirtualisierte Zeile zeigen.
    // Beim freien Scrollen folgt der Cursor deshalb der ersten sichtbaren Position;
    // tastaturgesteuertes Scrollen hat sein Ziel bereits vorab ausgewählt.
    if (virtualized && (selected < start || selected >= end)) {
      selected = Math.max(0, Math.min(firstVisible, total - 1));
    }
    const rendered = [];
    for (let index = start; index < end; index += 1) {
      rendered.push(rowHtml(
        rows[index],
        index,
        total,
        index === selected,
        issuesByPath.get(rows[index].path) || [],
        metadataClassLabels,
      ));
    }
    list.dataset.virtualized = String(virtualized);
    list.innerHTML = virtualSpacer(start * ROW_HEIGHT)
      + rendered.join("")
      + virtualSpacer((total - end) * ROW_HEIGHT);
    list.setAttribute("aria-activedescendant", `sync-comparison-option-${selected}`);
  }

  function select(index, { scroll = true } = {}) {
    if (!rows.length) return;
    selected = Math.max(0, Math.min(index, rows.length - 1));
    if (scroll) ensureVisible(selected);
    renderRows();
  }

  function render(comparison, metadataReport = null) {
    rows = Array.isArray(comparison?.rows) ? comparison.rows : [];
    selected = 0;
    issuesByPath = new Map();
    for (const issue of metadataReport?.issues || []) {
      const existing = issuesByPath.get(issue.path) || [];
      existing.push(issue);
      issuesByPath.set(issue.path, existing);
    }
    list.scrollTop = 0;
    const counts = comparison?.categories || {};
    categorySummary.textContent = [
      `${counts.additions || 0} hinzufügen`,
      `${counts.updates || 0} aktualisieren`,
      `${counts.quarantines || 0} Quarantäne`,
      `${counts.skipped || 0} ausgelassen`,
    ].join(" · ");
    root.hidden = rows.length === 0;
    renderRows();
  }

  list.addEventListener("scroll", renderRows, { passive: true });
  list.addEventListener("click", event => {
    const option = event.target.closest('[role="option"][data-i]');
    if (option) select(Number(option.dataset.i), { scroll: false });
  });
  list.addEventListener("keydown", event => {
    const page = Math.max(1, Math.floor((list.clientHeight || 420) / ROW_HEIGHT));
    const next = {
      ArrowUp: selected - 1,
      ArrowDown: selected + 1,
      Home: 0,
      End: rows.length - 1,
      PageUp: selected - page,
      PageDown: selected + page,
    }[event.key];
    if (next == null) return;
    event.preventDefault();
    select(next);
  });

  return { render };
}
