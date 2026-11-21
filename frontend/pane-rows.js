import {
  fmtEntryDate, fmtEntryKind, fmtEntryPermissions, fmtEntrySize, isEntryNameSafe,
} from "./ui-utils.js";

export const VIRTUALIZE_AFTER = 2000;
const VIRTUAL_OVERSCAN = 8;

export function rowHeightForPane(element) {
  const root = element || document.documentElement;
  const value = Number.parseFloat(
    getComputedStyle(root).getPropertyValue("--row-height"),
  );
  return Number.isFinite(value) && value > 0 ? value : 1;
}

export function optionId(side, index) {
  return `pane-${side}-option-${index}`;
}

export function directorySizeEligible(entry) {
  return entry.is_dir && !entry.is_symlink && isEntryNameSafe(entry);
}

function span(className, text = "") {
  const node = document.createElement("span");
  node.className = className;
  node.textContent = text;
  return node;
}

function optionNode(side, pane, index, className) {
  const node = document.createElement("div");
  node.id = optionId(side, index);
  node.className = className;
  node.setAttribute("role", "option");
  node.setAttribute("aria-selected", String(pane.sel.has(index)));
  node.setAttribute("aria-posinset", String(index + 1));
  node.setAttribute("aria-setsize", String(pane.entries.length));
  return node;
}

function rowNode(side, pane, entry, index) {
  const safeName = isEntryNameSafe(entry);
  const iconType = entry.is_dir ? "folder" : "file";
  const classes = ["row", entry.is_dir ? "dir" : "file"];
  if (pane.viewOptions.zebraRows && index % 2 === 1) classes.push("stripe");
  if (pane.sel.has(index)) classes.push("selected");
  if (index === pane.cursor) classes.push("cursor");
  if (!safeName) classes.push("unsafe-name");

  const row = optionNode(side, pane, index, classes.join(" "));
  row.dataset.i = String(index);
  // Sicher benannte Einträge sind Kopierquellen für die andere Pane. Echte
  // Ordner tragen zusätzlich den bisherigen Payload für die Favoritenliste.
  if (safeName) row.draggable = true;
  else row.title = "Dateiname ist nicht eindeutig als UTF-8 darstellbar und bleibt deshalb unverändert";

  const icon = span("r-icon");
  icon.setAttribute("aria-hidden", "true");
  icon.append(span(`file-icon ${iconType}`));
  if (entry.is_symlink) {
    const badge = span("link-badge", "↗");
    badge.setAttribute("aria-hidden", "true");
    icon.append(badge);
  }
  const name = span("r-name", entry.name);
  if (!safeName) {
    const badge = span("unsafe-name-badge", "!");
    badge.setAttribute("aria-label", "Name nicht bedienbar");
    name.append(badge);
  }
  row.append(icon, name);

  const columns = pane.viewOptions.columns;
  // Die leere Trennerspur hält die Werte bündig mit den Griffen im Spaltenkopf.
  function addCell(cell) {
    const separator = span("r-col-separator");
    separator.setAttribute("aria-hidden", "true");
    row.append(separator, cell);
  }

  if (columns.size) {
    const sizeCell = span("r-size");
    if (entry.is_dir) {
      const sizeControl = directorySizeNode(pane, entry);
      if (sizeControl) sizeCell.append(sizeControl);
    } else {
      sizeCell.textContent = entry.is_file && entry.size == null ? "—" : fmtEntrySize(entry.size);
    }
    addCell(sizeCell);
  }
  if (columns.kind) addCell(span("r-kind", fmtEntryKind(entry)));
  if (columns.modified) {
    addCell(span("r-date", fmtEntryDate(
      entry.mtime,
      pane.dateStages.modified,
      pane.viewOptions.relativeDates,
    )));
  }
  if (columns.created) {
    addCell(span("r-created", fmtEntryDate(
      entry.created,
      pane.dateStages.created,
      pane.viewOptions.relativeDates,
    )));
  }
  if (columns.owner) addCell(span("r-owner", entry.owner || ""));
  if (columns.group) addCell(span("r-group", entry.group || ""));
  if (columns.permissions) addCell(span("r-permissions", fmtEntryPermissions(entry.mode)));
  return row;
}

export function directorySizeDisplayText(value) {
  if (!value || value.status === "loading" || value.status === "cancelled") return "";
  if (value.status === "failed") return "Fehler";
  return `${value.limitations?.length ? "≈ " : ""}${fmtEntrySize(value.bytes)}`;
}

function directorySizeButton(className, action, text, label) {
  const button = document.createElement("button");
  button.className = className;
  button.type = "button";
  button.dataset.directorySize = action;
  button.textContent = text;
  button.setAttribute("aria-label", label);
  return button;
}

function directorySizeNode(pane, entry) {
  if (!directorySizeEligible(entry)) return null;
  const value = entry.directorySize;
  if (!value && pane.provider === "local") return null;
  if (!value || value.status === "cancelled") {
    return directorySizeButton(
      "folder-size-action", "calculate", "Berechnen", `Größe von ${entry.name} berechnen`,
    );
  }
  if (value.status === "loading") {
    const status = span("folder-size-loading", "…");
    status.setAttribute("role", "status");
    status.setAttribute("aria-label", `Größe von ${entry.name} wird berechnet`);
    return status;
  }
  const text = directorySizeDisplayText(value);
  const issueText = (value.issues || []).map(issue => issue.message).join(" · ");
  const partial = value.status === "partial";
  const failed = value.status === "failed";
  const resultTitle = failed
    ? issueText || value.error?.message || "Berechnung fehlgeschlagen; klicken für einen neuen Versuch"
    : partial
      ? `${issueText || "Bilanz unvollständig"}; klicken für eine neue Berechnung`
      : `${value.files} Dateien · ${value.directories} Unterordner; klicken für eine neue Berechnung`;
  const title = [resultTitle, ...(value.limitations || [])].join(" · ");
  const label = value.limitations?.length
    ? `Größe von ${entry.name}: ${text} · Best Effort; neu berechnen`
    : `Größe von ${entry.name} neu berechnen`;
  const button = directorySizeButton(
    partial || failed ? "folder-size-value incomplete" : "folder-size-value",
    "recalculate",
    partial ? `${text} ⚠` : text,
    label,
  );
  button.title = title;
  return button;
}

// Die CSP verbietet Inline-Styles. Ein leeres SVG mit numerischer Höhe hält den
// nicht gerenderten Scrollbereich deshalb ohne Style-Attribut frei.
function appendVirtualSpacer(fragment, height) {
  if (height <= 0) return;
  const spacer = document.createElement("div");
  spacer.className = "virtual-spacer";
  spacer.setAttribute("aria-hidden", "true");
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("width", "1");
  svg.setAttribute("height", String(Math.ceil(height)));
  svg.setAttribute("aria-hidden", "true");
  spacer.append(svg);
  fragment.append(spacer);
}

/** Die sichtbaren Zeilen dürfen klein bleiben, der semantisch aktive Eintrag
 * muss für Hilfstechniken aber im DOM bleiben. Diese knappe Ersatzoption liegt
 * nur außerhalb des sichtbaren Ausschnitts und verwendet dieselbe stabile ID. */
function appendVirtualActiveOption(fragment, side, pane, start, end) {
  if (pane.cursor < 0 || pane.cursor >= pane.entries.length
    || (pane.cursor >= start && pane.cursor < end)) {
    return;
  }
  const option = optionNode(side, pane, pane.cursor, "virtual-active-option");
  option.textContent = pane.entries[pane.cursor].name;
  fragment.append(option);
}

/**
 * Der Listen-Scrollbalken verschiebt die Zeilen nach links, den Spaltenkopf aber
 * nicht. Seine gemessene Breite wandert deshalb als Variable an die Pane und wird
 * dort als zusätzliche rechte Polsterung des Kopfes wirksam — nur so bleiben
 * Trenner und Zellen bündig.
 */
function syncListGutter(pane) {
  const gutter = Math.max(0, pane.listing.offsetWidth - pane.listing.clientWidth);
  if (gutter === pane.listGutter) return;
  pane.listGutter = gutter;
  pane.el.style.setProperty("--list-gutter", `${gutter}px`);
}

export function renderPaneRows(side, pane) {
  // Die Datumsstufe folgt der tatsächlich gerenderten Spaltenbreite und wird vor
  // dem Aufbau der Zeilen bestimmt — damit wirkt jedes Ziehen und jeder
  // Fensterresize sofort.
  if (pane.columns) {
    pane.dateStages.modified = pane.columns.dateStage("date");
    pane.dateStages.created = pane.columns.dateStage("created");
  }
  const total = pane.entries.length;
  if (total === 0) {
    pane.renderStart = 0;
    pane.renderEnd = 0;
    pane.listing.dataset.virtualized = "false";
    pane.listing.removeAttribute("aria-activedescendant");
    const emptyLabel = pane.filterText.trim() || pane.filterKind !== "all"
      ? "Keine Treffer im geladenen Ordner"
      : pane.listingOmissions.length ? "Keine anzeigbaren Einträge" : "Leerer Ordner";
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = emptyLabel;
    pane.listing.replaceChildren(empty);
    syncListGutter(pane);
    return;
  }

  const virtualized = total > VIRTUALIZE_AFTER;
  const rowHeight = rowHeightForPane(pane.el);
  const visibleRows = Math.max(1, Math.ceil(pane.listing.clientHeight / rowHeight));
  const firstVisible = Math.max(0, Math.floor(pane.listing.scrollTop / rowHeight));
  const start = virtualized ? Math.max(0, firstVisible - VIRTUAL_OVERSCAN) : 0;
  const end = virtualized
    ? Math.min(total, firstVisible + visibleRows + VIRTUAL_OVERSCAN)
    : total;
  pane.renderStart = start;
  pane.renderEnd = end;
  pane.listing.dataset.virtualized = String(virtualized);
  pane.listing.dataset.renderStart = String(start);
  pane.listing.dataset.renderEnd = String(end);
  const rows = document.createDocumentFragment();
  appendVirtualSpacer(rows, start * rowHeight);
  for (let index = start; index < end; index += 1) {
    rows.append(rowNode(side, pane, pane.entries[index], index));
  }
  appendVirtualSpacer(rows, (total - end) * rowHeight);
  if (virtualized) appendVirtualActiveOption(rows, side, pane, start, end);
  pane.listing.replaceChildren(rows);
  if (pane.cursor >= 0 && pane.cursor < total) {
    pane.listing.setAttribute("aria-activedescendant", optionId(side, pane.cursor));
  } else {
    pane.listing.removeAttribute("aria-activedescendant");
  }
  syncListGutter(pane);
}
