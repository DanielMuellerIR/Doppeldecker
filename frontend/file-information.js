// Informationsdialog und gewöhnliche Metadatenänderungen für die aktuelle Pane-Auswahl.
//
// Metadaten stammen aus dem gebundenen Listing oder bestätigten Backendänderungen. Ordnerinhalte kommen aus derselben fenster- und panegebundenen Größenmessung
// wie die Größenspalte; partielle Resultate werden nie zu einer exakten Summe erklärt.

import { hideModal, showOwnedModal } from "./modal.js";
import { invoke } from "./tauri-api.js";
import { createFinderCommentEditor } from "./file-information-comments.js";
import { createFileFlagEditor } from "./file-information-flags.js";
import { createVisibilityEditor } from "./file-information-visibility.js";
import { createFinderTagEditor } from "./file-information-tags.js";
import {
  $, escapeHtml, fmtEntryDate, fmtEntryKind, fmtEntryPermissions, fmtEntrySize,
  isEntryNameSafe, joinPath,
} from "./ui-utils.js";

const PROVIDER_LABELS = Object.freeze({
  local: "Lokal",
  sftp: "SFTP",
  ftps: "FTPS",
  webdav: "WebDAV",
  smb: "SMB",
});

function cloneEntry(entry) {
  return {
    ...entry,
    directorySize: entry.directorySize
      ? {
        ...entry.directorySize,
        issues: Array.isArray(entry.directorySize.issues)
          ? entry.directorySize.issues.map(issue => ({ ...issue }))
          : [],
      }
      : undefined,
  };
}

function exactBytes(bytes) {
  if (bytes == null) return "—";
  const raw = Number(bytes);
  if (!Number.isFinite(raw) || raw < 0) return "—";
  const formatted = fmtEntrySize(raw);
  // JSON-Zahlen oberhalb dieser Grenze sind in JavaScript bereits gerundet. Die
  // kompakte Einheit darf dann weiter Orientierung geben, aber keine exakte
  // Bytezahl behaupten.
  if (!Number.isSafeInteger(raw)) return formatted;
  const exact = `${Math.round(raw).toLocaleString("de-DE")} Byte`;
  return formatted === exact ? exact : `${formatted} (${exact})`;
}

function plural(count, singular, pluralForm) {
  return `${count.toLocaleString("de-DE")} ${count === 1 ? singular : pluralForm}`;
}

function commonRaw(entries, getter) {
  if (!entries.length) return null;
  const first = getter(entries[0]);
  return entries.every(entry => getter(entry) === first) ? first : Symbol.for("different");
}

function commonText(entries, getter) {
  const value = commonRaw(entries, getter);
  if (value === Symbol.for("different")) return "Verschieden";
  return value == null || value === "" ? "—" : String(value);
}

function commonDate(entries, key) {
  const value = commonRaw(entries, entry => entry[key] ?? null);
  if (value === Symbol.for("different")) return "Verschieden";
  return value == null ? "—" : fmtEntryDate(value, 4);
}

function permissionsText(entries) {
  const value = commonRaw(entries, entry => entry.mode ?? null);
  if (value === Symbol.for("different")) return "Verschieden";
  if (value == null || value === "") return "—";
  const numericMode = Number(value);
  if (!Number.isInteger(numericMode)) return "—";
  const mode = numericMode & 0o777;
  return `${fmtEntryPermissions(mode)} (${mode.toString(8).padStart(3, "0")})`;
}

function selectionKind(entries) {
  if (entries.length === 1) return fmtEntryKind(entries[0]);
  const files = entries.filter(entry => entry.is_file).length;
  const folders = entries.filter(entry => entry.is_dir).length;
  const others = entries.length - files - folders;
  return [
    files ? plural(files, "Datei", "Dateien") : "",
    folders ? plural(folders, "Ordner", "Ordner") : "",
    others ? plural(others, "sonstiger Eintrag", "sonstige Einträge") : "",
  ].filter(Boolean).join(" · ");
}

function sizeSummary(entries) {
  const files = entries.filter(entry => entry.is_file);
  const folders = entries.filter(entry => entry.is_dir && !entry.is_symlink);
  const knownFiles = files.filter(entry => Number.isFinite(entry.size) && entry.size >= 0);
  const unknownFiles = files.length - knownFiles.length;
  const fileBytes = knownFiles.reduce((sum, entry) => sum + entry.size, 0);
  const unknownNote = unknownFiles ? `${plural(unknownFiles, "Datei", "Dateien")} ohne Größenangabe` : "";
  if (!folders.length) {
    if (!knownFiles.length) return "Größe nicht verfügbar";
    return unknownFiles ? `${exactBytes(fileBytes)} bekannt · ${unknownNote}` : exactBytes(fileBytes);
  }

  const complete = folders.filter(entry => entry.directorySize?.status === "complete");
  const observed = folders.filter(entry => entry.directorySize?.status === "observed");
  const loading = folders.filter(entry => entry.directorySize?.status === "loading");
  const failed = folders.filter(entry => entry.directorySize?.status === "failed");
  const incomplete = folders.filter(entry => (
    entry.directorySize && !["complete", "observed", "loading", "failed", "cancelled"].includes(entry.directorySize.status)
  ));
  const cancelled = folders.filter(entry => entry.directorySize?.status === "cancelled");
  const unresolved = folders.filter(entry => !entry.directorySize);
  const completeBytes = complete.reduce(
    (sum, entry) => sum + Number(entry.directorySize.bytes || 0),
    fileBytes,
  );
  if (complete.length + observed.length === folders.length) {
    const folderBytes = [...complete, ...observed].reduce((sum, entry) => sum + Number(entry.directorySize.bytes || 0), 0);
    return `${observed.length ? "≈ " : ""}${exactBytes(fileBytes + folderBytes)}${unknownFiles ? " bekannt" : ""}${observed.length ? " · Best Effort" : ""}${unknownNote ? ` · ${unknownNote}` : ""}`;
  }
  if (loading.length) {
    return fileBytes > 0 || complete.length
      ? `${exactBytes(completeBytes)} vollständig erfasst · weitere Ordner werden berechnet …${unknownNote ? ` · ${unknownNote}` : ""}`
      : `Wird berechnet …${unknownNote ? ` · ${unknownNote}` : ""}`;
  }
  const notes = unknownNote ? [unknownNote] : [];
  if (fileBytes > 0 || complete.length) notes.push(`${exactBytes(completeBytes)} vollständig erfasst`);
  if (incomplete.length) notes.push(`${plural(incomplete.length, "Ordner", "Ordner")} unvollständig`);
  if (observed.length) notes.push(`${plural(observed.length, "Ordner", "Ordner")} als Best Effort beobachtet`);
  if (failed.length) notes.push(`${plural(failed.length, "Ordner", "Ordner")} fehlgeschlagen`);
  if (unresolved.length || cancelled.length) {
    const missing = unresolved.length + cancelled.length;
    notes.push(`${plural(missing, "Ordner", "Ordner")} nicht berechnet`);
  }
  return notes.length ? notes.join(" · ") : "Nicht berechnet";
}

function containedSummary(entries) {
  const folders = entries.filter(entry => entry.is_dir && !entry.is_symlink);
  if (!folders.length) return null;
  if (!folders.every(entry => ["complete", "observed"].includes(entry.directorySize?.status))) {
    if (folders.some(entry => entry.directorySize?.status === "partial" && entry.directorySize.limitations?.length)) {
      return "Teilbilanz · ausgelassene Einträge in den Details";
    }
    return "Vollständige Größenberechnung nötig";
  }
  const directFiles = entries.filter(entry => entry.is_file).length;
  const files = folders.reduce((sum, entry) => sum + Number(entry.directorySize.files || 0), directFiles);
  const directories = folders.reduce(
    (sum, entry) => sum + Number(entry.directorySize.directories || 0),
    0,
  );
  const skipped = folders.reduce((sum, entry) => sum + Number(entry.directorySize.skipped || 0), 0);
  const parts = [plural(files, "Datei", "Dateien"), plural(directories, "Unterordner", "Unterordner")];
  if (skipped) parts.push(`${plural(skipped, "Eintrag", "Einträge")} ausgelassen`);
  return `${folders.some(entry => entry.directorySize?.status === "observed") ? "Beobachtet: " : ""}${parts.join(" · ")}`;
}

function folderResultText(entry) {
  const result = entry.directorySize;
  if (!result || result.status === "cancelled") return "Nicht berechnet";
  if (result.status === "loading") return "Wird berechnet …";
  if (result.status === "failed") {
    return errorMessage(result.error)
      ? `Berechnung fehlgeschlagen · ${errorMessage(result.error)}`
      : "Berechnung fehlgeschlagen";
  }
  const counts = `${plural(Number(result.files || 0), "Datei", "Dateien")} · `
    + plural(Number(result.directories || 0), "Unterordner", "Unterordner");
  const prefix = result.status === "complete" ? exactBytes(result.bytes)
    : result.status === "observed" ? `≈ ${exactBytes(result.bytes)} · Best Effort`
      : `${result.limitations?.length ? "≈ " : ""}${exactBytes(result.bytes)} erfasst`;
  return `${prefix} · ${counts}${result.status === "partial" ? " · unvollständig" : ""}`;
}

function errorMessage(error) {
  if (typeof error === "string") return error;
  return typeof error?.message === "string" ? error.message : "";
}

function entryDetailText(entry) {
  if (entry.is_dir && !entry.is_symlink) return folderResultText(entry);
  if (entry.is_file) return exactBytes(entry.size);
  return fmtEntryKind(entry);
}

function row(label, value, action = "") {
  return `<div class="file-info-row"><dt>${escapeHtml(label)}</dt><dd>${escapeHtml(value)}${action}</dd></div>`;
}

function issueHtml(entries) {
  const issues = entries.flatMap(entry => {
    const result = entry.directorySize;
    const listed = (result?.issues || []).map(issue => ({
      name: entry.name,
      message: issue.message || "Der Teilbaum konnte nicht vollständig gelesen werden.",
    }));
    if (result?.status === "failed" && errorMessage(result.error) && !listed.length) {
      listed.push({ name: entry.name, message: errorMessage(result.error) });
    }
    return listed;
  });
  if (!issues.length) return "";
  return `<section class="file-info-issues" aria-label="Unvollständige Ergebnisse">
    <h3>Unvollständige Ergebnisse</h3>
    <ul>${issues.map(issue => `<li><strong>${escapeHtml(issue.name)}:</strong> ${escapeHtml(String(issue.message))}</li>`).join("")}</ul>
  </section>`;
}

export function createFileInformationUi({ panes, selectedEntries, requestDirectorySizes, refreshPane, showDock }) {
  let current = null;
  let modalToken = null;
  let active = false;
  let propertyBusy = false;

  const tagEditor = createFinderTagEditor({ getCurrent: () => current, scopeMatches: currentScopeMatches,
    isBusy: () => propertyBusy, refresh, applyEdit: applyPropertyEdit });

  const visibilityEditor = createVisibilityEditor({ getCurrent: () => current, scopeMatches: currentScopeMatches,
    isBusy: () => propertyBusy, refresh, applyEdit: applyPropertyEdit });

  const commentEditor = createFinderCommentEditor({ getCurrent: () => current, scopeMatches: currentScopeMatches,
    isBusy: () => propertyBusy, refresh, applyEdit: applyPropertyEdit });

  const flagEditors = [
    { id: "locked", kind: "locked", title: "Dateisperre", actionLabel: "Dateisperre anwenden", label: "Gesperrt (macOS)", explanation: "Gesperrte Objekte lassen sich erst nach dem Entsperren ändern oder entfernen." },
    { id: "extension", kind: "hiddenExtension", title: "Dateierweiterung", actionLabel: "Erweiterungsanzeige anwenden", label: "Erweiterung im Finder ausblenden", explanation: "Der tatsächliche Dateiname bleibt unverändert. Nur reguläre Dateien sind bearbeitbar." },
  ].map(options => createFileFlagEditor({ ...options, getCurrent: () => current, scopeMatches: currentScopeMatches,
    isBusy: () => propertyBusy, refresh, applyEdit: applyPropertyEdit }));

  function hasEditableObject() {
    return current.entries.some(entry => isEntryNameSafe(entry) && !entry.is_symlink && (entry.is_file || entry.is_dir));
  }

  function administratorCheckbox(kind) {
    return current.propertyCapabilities?.administration
      ? `<label class="file-info-recursive"><input id="file-info-admin-${kind}" type="checkbox">Mit Administratorfreigabe ändern</label>` : "";
  }
  function administratorHtml() {
    if (!current.propertyCapabilities?.administration || !hasEditableObject()) return "";
    const status = current.adminStatus;
    return `<details class="file-info-editor" data-file-info-editor="administrator"><summary>Administratorfreigabe</summary>
      <p class="file-info-note">Optional für lokale Rechte und Eigentümer/Gruppe. Das Betriebssystem prüft die Administratorfreigabe für jede Änderung.</p>
      <p role="status" data-file-info-admin-status>${escapeHtml(current.adminMessage || status?.message || "Systemfreigabe wird geprüft …")}</p>
      ${["notRegistered", "notFound"].includes(status?.state) ? '<button type="button" class="btn ghost" data-file-info-admin-service="enable">Administratorzugriff einrichten</button>' : ""}
      ${status?.state === "requiresApproval" ? '<button type="button" class="btn ghost" data-file-info-admin-settings>Systemeinstellungen öffnen</button><button type="button" class="btn ghost" data-file-info-admin-refresh>Freigabe erneut prüfen</button>' : ""}
      ${status?.state === "enabled" ? '<button type="button" class="btn ghost" data-file-info-admin-service="enable">Administratorzugriff aktualisieren</button>' : ""}
      ${["enabled", "requiresApproval"].includes(status?.state) ? '<button type="button" class="btn ghost" data-file-info-admin-service="disable">Administratorzugriff entfernen</button>' : ""}
    </details>`;
  }
  async function loadAdministratorStatus(snapshot = current, { clearMessage = false } = {}) {
    try {
      const status = await invoke("get_metadata_admin_status");
      if (current !== snapshot || !active || !currentScopeMatches()) return;
      snapshot.adminStatus = status;
      if (clearMessage) snapshot.adminMessage = "";
      refresh();
    } catch (error) {
      if (current !== snapshot || !active) return;
      snapshot.adminMessage = error.message || String(error); refresh();
    }
  }
  async function changeAdministratorService(enabled) {
    if (propertyBusy || !currentScopeMatches()) return;
    const snapshot = current; propertyBusy = true;
    snapshot.propertyKind = "administrator";
    snapshot.propertyRequestId = null;
    snapshot.adminMessage = enabled ? "Administratorzugriff wird eingerichtet …" : "Administratorzugriff wird entfernt …";
    refresh();
    try { snapshot.adminStatus = await invoke("set_metadata_admin_service", { enabled }); snapshot.adminMessage = ""; }
    catch (error) { snapshot.adminMessage = error.message || String(error); }
    finally { propertyBusy = false; if (current === snapshot && active) { await loadAdministratorStatus(snapshot); refresh(); } }
  }

  function permissionEditorHtml() {
    if (!current.propertyCapabilities?.permissions || !hasEditableObject()) return "";
    const value = commonRaw(current.entries, entry => entry.mode ?? null);
    const mode = Number.isInteger(value) ? (value & 0o777).toString(8).padStart(3, "0") : "";
    return `<details class="file-info-permissions file-info-editor" data-file-info-editor="permission"><summary>Berechtigungen ändern</summary>
      <p>Die Rechte gelten für ${current.entries.length === 1 ? "das ausgewählte Objekt" : `die ${current.entries.length} ausgewählten Objekte`}. Weitere Hardlinknamen sind ebenfalls betroffen. Sonderbits bleiben erhalten. Links und Spezialdateien werden ausgelassen.</p>
      ${(current.propertyCapabilities.limitations || []).map(text => `<p class="file-info-note">${escapeHtml(text)}</p>`).join("")}
      <label for="file-info-mode">Unix-Rechte (oktal)</label>
      <div class="file-info-permission-controls"><input id="file-info-mode" class="modal-input" type="text" inputmode="numeric" maxlength="3" pattern="[0-7]{3}" autocomplete="off" spellcheck="false" placeholder="z. B. 640" value="${mode}">
      <button type="button" class="btn primary" data-file-info-permissions data-file-info-edit-action="permission">Rechte anwenden</button></div>
      ${current.propertyCapabilities.recursivePermissions && current.entries.some(entry => entry.is_dir) ? '<label class="file-info-recursive"><input id="file-info-recursive" type="checkbox">Auch auf alle enthaltenen Dateien und Ordner anwenden</label><p class="file-info-note">Dieselben Rechte gelten für Dateien und Ordner. Ordner brauchen Ausführen zum Betreten. Links werden nicht verfolgt; Abbruch behält bereits bestätigte Änderungen.</p>' : ""}
      ${administratorCheckbox("permission")}
      <button type="button" class="btn ghost" data-file-info-property-cancel="permission"${propertyBusy && current.propertyKind === "permission" ? "" : " hidden"}>Änderung abbrechen</button>
      <p class="file-info-note">Drei Ziffern für Eigentümer, Gruppe und andere: 4 = Lesen, 2 = Schreiben, 1 = Ausführen/Ordner betreten. Beispiel: 640.</p>
      <p role="status" data-file-info-permission-status>${escapeHtml(current.permissionStatus || "")}</p>
    </details>`;
  }

  function ownershipEditorHtml() {
    if (!current.propertyCapabilities?.ownership || !hasEditableObject()) return "";
    return `<details class="file-info-owner file-info-editor" data-file-info-editor="ownership"><summary>Eigentümer/Gruppe ändern</summary>
      <p>Lokaler Name oder numerische ID; leere Felder bleiben unverändert. Die Änderung gilt für die ausgewählten Dateien und Ordner sowie weitere Hardlinknamen. Links und Spezialdateien werden ausgelassen.</p>
      <div class="file-info-owner-fields"><div><label for="file-info-owner-name">Eigentümer (Name oder UID)</label><input id="file-info-owner-name" class="modal-input" type="text" autocomplete="off" spellcheck="false" placeholder="Unverändert"></div>
      <div><label for="file-info-group-name">Gruppe (Name oder GID)</label><input id="file-info-group-name" class="modal-input" type="text" autocomplete="off" spellcheck="false" placeholder="Unverändert"></div></div>
      <p class="file-info-note">Ein Eigentümerwechsel benötigt häufig Administratorrechte. Das System kann dabei setuid/setgid-Sonderbits entfernen.</p>
      ${administratorCheckbox("ownership")}
      <button type="button" class="btn primary" data-file-info-ownership data-file-info-edit-action="ownership">Eigentümer/Gruppe anwenden</button>
      <button type="button" class="btn ghost" data-file-info-property-cancel="ownership"${propertyBusy && current.propertyKind === "ownership" ? "" : " hidden"}>Änderung abbrechen</button>
      <p role="status" data-file-info-ownership-status>${escapeHtml(current.ownershipStatus || "")}</p>
    </details>`;
  }

  function providerText(snapshot) {
    let provider = PROVIDER_LABELS[snapshot.provider] || String(snapshot.provider).toUpperCase();
    if (snapshot.provider === "ftps" && snapshot.securityStatus === "plaintext") {
      provider = "FTP · unverschlüsselt";
    } else if (snapshot.provider === "ftps" && snapshot.securityStatus === "tls_identity_unverified") {
      provider = "FTPS · Identität ungeprüft";
    } else if (snapshot.provider === "sftp" && snapshot.securityStatus === "sftp_path_best_effort") {
      provider = "SFTP · pfadbasiertes Best Effort";
    }
    return snapshot.profileName ? `${provider} · ${snapshot.profileName}` : provider;
  }

  function eligibleFolders() {
    return current.entries.filter(entry => entry.is_dir && !entry.is_symlink && isEntryNameSafe(entry));
  }

  function calculateActionHtml() {
    const folders = eligibleFolders();
    if (!folders.length) return "";
    const loading = folders.some(entry => entry.directorySize?.status === "loading");
    const measured = folders.some(entry => (
      entry.directorySize && !["loading", "cancelled"].includes(entry.directorySize.status)
    ));
    return ` <button class="btn ${measured ? "ghost" : "primary"} small file-info-calculate" type="button"
      data-file-info-calculate${loading ? " disabled" : ""}>${loading ? "Berechnet …" : measured ? "Neu berechnen" : "Berechnen"}</button>`;
  }

  function bodyHtml() {
    const entries = current.entries;
    const single = entries.length === 1;
    const unsafe = entries.some(entry => !isEntryNameSafe(entry));
    const name = single
      ? `${entries[0].name}${unsafe ? " · nicht eindeutig dekodierbar" : ""}`
      : `${entries.length.toLocaleString("de-DE")} Objekte`;
    const icon = single && entries[0].is_dir ? "folder" : "file";
    // Ein verlustbehafteter Anzeigename darf niemals als eindeutiger Dateipfad
    // ausgegeben werden: Zwei verschiedene Byte-Namen können gleich aussehen.
    const path = single && !unsafe
      ? joinPath(current.parent, entries[0].name)
      : single
        ? `Nicht eindeutig identifizierbarer Eintrag in ${current.parent}`
        : current.parent;
    const contained = containedSummary(entries);
    const hardLinks = single && entries[0].hard_links != null
      ? row("Hardlinks", Number(entries[0].hard_links).toLocaleString("de-DE"))
      : "";
    const folderDetails = entries.filter(entry => entry.is_dir && !entry.is_symlink);
    const detailEntries = entries.length > 1
      ? entries
      : folderDetails.filter(entry => entry.directorySize?.status === "partial");
    return `<div class="file-info-heading">
      <span class="file-info-icon" aria-hidden="true"><span class="file-icon ${icon}"></span></span>
      <div><strong>${escapeHtml(name)}</strong><span>${escapeHtml(selectionKind(entries))}</span></div>
    </div>
    <dl class="file-info-grid">
      ${row("Pfad", path)}
      ${row("Ort", providerText(current))}
      ${current.provider === "local" ? "" : row("Sitzung", current.sessionGeneration == null ? "—" : String(current.sessionGeneration))}
      ${row("Größe", sizeSummary(entries), calculateActionHtml())}
      ${contained == null ? "" : row("Enthalten", contained)}
      ${row("Geändert", commonDate(entries, "mtime"))}
      ${row("Erstellt", commonDate(entries, "created"))}
      ${row("Eigentümer", commonText(entries, entry => entry.owner))}
      ${row("Gruppe", commonText(entries, entry => entry.group))}
      ${row("Berechtigungen", permissionsText(entries))}
      ${hardLinks}
    </dl>
    ${!current.propertyCapabilities ? `<p class="file-info-note" role="status">${escapeHtml(current.propertyCapabilitiesError || "Bearbeitbare Eigenschaften werden ermittelt …")}</p>` : !current.propertyCapabilities.permissions ? `<p class="file-info-note">${(current.propertyCapabilities.limitations || []).map(escapeHtml).join(" ")}</p>` : ""}
    ${permissionEditorHtml()}
    ${ownershipEditorHtml()}
    ${administratorHtml()}
    ${tagEditor.html()}
    ${visibilityEditor.html()}
    ${flagEditors.map(editor => editor.html()).join("")}
    ${commentEditor.html()}
    ${propertyReportHtml("permission", "Rechteänderung")}
    ${propertyReportHtml("ownership", "Eigentümeränderung")}
    ${propertyReportHtml("tags", "Finder-Tagänderung")}
    ${propertyReportHtml("visibility", "Sichtbarkeitsänderung")}
    ${propertyReportHtml("locked", "Änderung der Dateisperre")}
    ${propertyReportHtml("extension", "Änderung der Erweiterungsanzeige")}
    ${propertyReportHtml("comments", "Kommentaränderung")}
    ${detailEntries.length
      ? `<section class="file-info-folders" aria-label="Ausgewählte Einträge"><h3>${entries.length > 1 ? "Auswahl" : "Ordner"}</h3><ul>${detailEntries.map(entry => (
        `<li><span>${escapeHtml(entry.name)}</span><span>${escapeHtml(entryDetailText(entry))}</span></li>`
      )).join("")}</ul></section>`
      : ""}
    ${unsafe ? '<p class="file-info-note">Mindestens ein Name ist nicht eindeutig dekodierbar. Diese Einträge werden bei Änderungen und Größenberechnungen ausgelassen; ihr vollständiger Pfad bleibt verborgen.</p>' : ""}
    ${current.provider === "local" ? "" : '<p class="file-info-note">Remote-Provider zeigen nur Metadaten, die das jeweilige Protokoll verlässlich liefert. Fehlende Werte sind mit „—“ gekennzeichnet.</p>'}
    ${[...new Set(entries.flatMap(entry => entry.directorySize?.limitations || []))].map(note => `<p class="file-info-note">${escapeHtml(note)}</p>`).join("")}
    ${issueHtml(entries)}`;
  }

  function propertyReportHtml(kind, title) {
    const report = current[`${kind}Report`];
    if (!report) return "";
    const page = current[`${kind}Page`] || 0;
    const pages = Math.max(1, Math.ceil(report.length / 40));
    return `<section class="file-info-permission-report" aria-label="Ergebnis der ${title}" aria-live="polite"><h3>${title}</h3>
      <p>${report.filter(row => !row.code).length} von ${report.length} bestätigt · Seite ${page + 1} von ${pages}</p>
      <ul>${report.slice(page * 40, (page + 1) * 40).map(row => `<li><strong>${escapeHtml(row.name)}</strong>: ${escapeHtml(row.message)}${row.code ? ` (${escapeHtml(row.code)})` : ""}</li>`).join("")}</ul>
      ${pages > 1 ? `<button type="button" class="btn ghost" data-file-info-report="${kind}" data-file-info-permission-page="-1"${page === 0 ? " disabled" : ""}>Zurück</button><button type="button" class="btn ghost" data-file-info-report="${kind}" data-file-info-permission-page="1"${page + 1 === pages ? " disabled" : ""}>Weiter</button>` : ""}</section>`;
  }

  function refresh() {
    if (!current || !modalToken || !active || $("#modal").hidden) return;
    const body = $("#modal-body");
    const focused = document.activeElement;
    const restoreBodyFocus = body.contains(focused);
    const editors = [...body.querySelectorAll(".file-info-editor")].map(editor => ({
      kind: editor.dataset.fileInfoEditor, open: editor.open,
      fields: [...editor.querySelectorAll("input, select, textarea")].map(input => ({ id: input.id, value: input.value,
        checked: input.checked, start: input.selectionStart, end: input.selectionEnd })),
      focus: editor.contains(focused) ? focused.id ? `#${focused.id}` : focused.matches("summary") ? "summary" : "[data-file-info-edit-action]" : null,
    }));
    body.innerHTML = bodyHtml();
    body.onclick = handleBodyClick;
    commentEditor.connect(body);
    let restoredEditorFocus = null;
    for (const saved of editors) {
      const editor = body.querySelector(`[data-file-info-editor="${saved.kind}"]`);
      if (!editor) continue;
      editor.open = saved.open;
      for (const field of saved.fields) {
        const input = editor.querySelector(`#${field.id}`);
        if (!input) continue;
        input.value = field.value; input.checked = field.checked;
        if (field.start != null) input.setSelectionRange(field.start, field.end);
      }
      if (saved.focus) restoredEditorFocus = editor.querySelector(saved.focus);
    }
    if (propertyBusy) for (const control of body.querySelectorAll("button:not([data-file-info-property-cancel]), input, select, textarea")) control.disabled = true;
    if (restoredEditorFocus && !restoredEditorFocus.disabled) {
      restoredEditorFocus.focus({ preventScroll: true });
    } else if (restoreBodyFocus) {
      // Der Größenstart ersetzt den gerade fokussierten Berechnen-Knopf durch
      // seinen deaktivierten Ladezustand. Der Fokus bleibt dann am nächsten
      // bedienbaren Element des Modals statt auf den Dokumentrumpf zu fallen.
      const target = body.querySelector("[data-file-info-calculate]:not([disabled])")
        || $("#modal .modal-buttons button:not([disabled])");
      target?.focus({ preventScroll: true });
    }
  }

  function close() {
    if (propertyBusy) return false;
    const token = modalToken;
    if (!token || !hideModal(token)) return false;
    active = false;
    modalToken = null;
    current = null;
    $("#modal .modal").classList.remove("file-information-modal");
    return true;
  }

  function handleBodyClick(event) {
    const summary = event.target.closest("summary");
    const editor = summary?.parentElement;
    if (editor?.matches("details[data-file-info-editor]")) {
      const snapshot = current;
      const kind = editor.dataset.fileInfoEditor;
      requestAnimationFrame(() => {
        if (!active || current !== snapshot) return;
        const body = $("#modal-body");
        const expanded = body.querySelector(`details[data-file-info-editor="${kind}"]`);
        if (expanded?.open) {
          expanded.scrollIntoView({ block: expanded.offsetHeight <= body.clientHeight ? "nearest" : "start" });
        }
      });
      return;
    }
    const serviceButton = event.target.closest("[data-file-info-admin-service]");
    if (serviceButton) { void changeAdministratorService(serviceButton.dataset.fileInfoAdminService === "enable"); return; }
    if (event.target.closest("[data-file-info-admin-refresh]")) { void loadAdministratorStatus(current, { clearMessage: true }); return; }
    if (event.target.closest("[data-file-info-admin-settings]")) {
      const snapshot = current;
      void invoke("open_metadata_admin_settings").catch(error => { if (current !== snapshot || !active) return; snapshot.adminMessage = error.message || String(error); refresh(); }); return;
    }
    if (event.target.closest("[data-file-info-property-cancel]")) {
      if (propertyBusy) void invoke("cancel_file_property_edit", { requestId: current.propertyRequestId }).catch(error => showDock?.(error.message || String(error), { closable: true }));
      return;
    }
    if (tagEditor.click(event) || visibilityEditor.click(event) || flagEditors.some(editor => editor.click(event)) || commentEditor.click(event)) return;
    const pageButton = event.target.closest("[data-file-info-permission-page]");
    if (pageButton) {
      if (propertyBusy) return;
      const kind = pageButton.dataset.fileInfoReport;
      if (!["permission", "ownership", "tags", "visibility", "comments", "locked", "extension"].includes(kind)) return;
      current[`${kind}Page`] = Math.max(0, Math.min(Math.ceil(current[`${kind}Report`].length / 40) - 1,
        (current[`${kind}Page`] || 0) + Number(pageButton.dataset.fileInfoPermissionPage)));
      refresh();
      const report = $("#modal-body").querySelector(`[data-file-info-report="${kind}"]`)?.closest("section");
      const next = report?.querySelector(`[data-file-info-permission-page="${pageButton.dataset.fileInfoPermissionPage}"]:not([disabled])`)
        || report?.querySelector("[data-file-info-permission-page]:not([disabled])");
      next?.focus({ preventScroll: true });
      return;
    }
    if (event.target.closest("[data-file-info-permissions]")) {
      void changePermissions();
      return;
    }
    if (event.target.closest("[data-file-info-ownership]")) { void changeOwnership(); return; }
    if (!event.target.closest("[data-file-info-calculate]")) return;
    if (!currentScopeMatches()) {
      // Die Pane kann durch `connections-updated` synchron auf einen anderen
      // Endpunkt wechseln, während dieser Dialog offen bleibt. Der alte Dialog
      // darf den einzigen Größenlauf der neuen Pane nicht ersetzen.
      close();
      return;
    }
    const folders = eligibleFolders();
    const recalculate = folders.some(entry => (
      entry.directorySize && !["loading", "cancelled"].includes(entry.directorySize.status)
    ));
    void requestDirectorySizes(current.side, folders.map(entry => entry.name), { recalculate });
  }

  async function changePermissions() {
    if (propertyBusy || !currentScopeMatches()) return;
    const input = $("#file-info-mode");
    if (!/^[0-7]{3}$/.test(input.value)) {
      current.permissionStatus = "Bitte genau drei Ziffern von 0 bis 7 eingeben.";
      $("[data-file-info-permission-status]").textContent = current.permissionStatus;
      input.focus(); return;
    }
    const recursive = Boolean($("#file-info-recursive")?.checked);
    await applyPropertyEdit("permission", "change_file_permissions", { mode: input.value, recursive,
      ...($("#file-info-admin-permission")?.checked ? { administrator: true } : {}),
      profileId: current.profileId ?? null, profileRevision: current.profileRevision ?? null },
      recursive ? "Inhalte werden erfasst, danach Rechte geändert … Abbruch bleibt möglich." : "Berechtigungen werden geändert …",
      (entry, row) => { if (row.changed) entry.mode = row.mode; else if (["outcome_unclear", "plan_drift", "not_found"].includes(row.code)) entry.mode = null; });
  }
  async function changeOwnership() {
    if (propertyBusy || !currentScopeMatches()) return;
    const owner = $("#file-info-owner-name").value;
    const group = $("#file-info-group-name").value;
    if (!owner.trim() && !group.trim()) {
      current.ownershipStatus = "Mindestens Eigentümer oder Gruppe angeben.";
      $("[data-file-info-ownership-status]").textContent = current.ownershipStatus;
      $("#file-info-owner-name").focus(); return;
    }
    await applyPropertyEdit("ownership", "change_file_ownership", { owner, group, ...($("#file-info-admin-ownership")?.checked ? { administrator: true } : {}) },
      "Lokale Konten und Eigentümer/Gruppe werden geprüft … Abbruch bleibt möglich.",
      (entry, row) => {
        if (row.changed) { entry.owner = row.owner; entry.group = row.group; }
        else if (["outcome_unclear", "plan_drift", "not_found"].includes(row.code)) { entry.owner = null; entry.group = null; }
      });
  }
  async function applyPropertyEdit(kind, command, fields, progressText, updateEntry = () => {}) {
    if (propertyBusy || !currentScopeMatches()) return;
    const snapshot = current;
    propertyBusy = true;
    snapshot.propertyKind = kind;
    snapshot.propertyRequestId = crypto.randomUUID();
    const controls = [...$("#modal").querySelectorAll("button:not([data-file-info-property-cancel]), input, select, textarea")].map(control => [control, control.disabled]);
    for (const [control] of controls) control.disabled = true;
    $(`[data-file-info-property-cancel="${kind}"]`).hidden = false;
    snapshot[`${kind}Status`] = progressText;
    const status = $(`[data-file-info-${kind}-status]`);
    status.textContent = progressText; status.tabIndex = -1; status.focus({ preventScroll: true });
    try {
      const report = await invoke(command, { request: {
        requestId: snapshot.propertyRequestId, provider: snapshot.provider, root: snapshot.parent,
        names: snapshot.entries.filter(isEntryNameSafe).map(entry => entry.name), ...fields,
      } });
      for (const row of report) {
        const entry = snapshot.entries.find(entry => entry.name === row.name);
        if (entry) updateEntry(entry, row);
      }
      snapshot[`${kind}Report`] = report; snapshot[`${kind}Page`] = 0; snapshot[`${kind}Status`] = "";
      const label = kind === "permission" ? "Rechteänderungen" : kind === "tags" ? "Tagzuweisungen" : kind === "visibility" ? "Sichtbarkeitsänderungen" : kind === "comments" ? "Kommentaränderungen" : ["locked", "extension"].includes(kind) ? "Dateieigenschaftsänderungen" : "Eigentümer-/Gruppenänderungen";
      showDock?.(`${report.filter(row => !row.code).length} von ${report.length} ${label} bestätigt.${report.some(row => row.code === "outcome_unclear") ? " Ein Ausgang ist unklar; vor erneutem Versuch prüfen." : ""}`, { closable: true });
      if (current === snapshot && currentScopeMatches()) refresh();
      if (panes[snapshot.side].provider === snapshot.provider && panes[snapshot.side].path === snapshot.parent) await refreshPane?.(snapshot.side);
      return report;
    } catch (error) {
      if (current === snapshot && active) {
        snapshot[`${kind}Status`] = error.message || String(error);
        $(`[data-file-info-${kind}-status]`).textContent = snapshot[`${kind}Status`];
      }
    } finally {
      propertyBusy = false;
      if (current === snapshot && active) {
        for (const [control, disabled] of controls) if (control.isConnected) control.disabled = disabled;
        refresh();
        if (snapshot[`${kind}Status`]) {
          const failureStatus = $(`[data-file-info-${kind}-status]`);
          failureStatus.tabIndex = -1;
          failureStatus.focus();
          failureStatus.scrollIntoView({ block: "nearest" });
        } else {
          ($(`[data-file-info-editor="${kind}"] summary`) || $("#modal .modal-buttons button"))?.focus({ preventScroll: true });
        }
      }
    }
  }

  function currentScopeMatches() {
    if (!current) return false;
    const pane = panes[current.side];
    return Boolean(pane)
      && pane.provider === current.provider
      && pane.profileId === current.profileId
      && pane.profileRevision === current.profileRevision
      && pane.sessionGeneration === current.sessionGeneration
      && pane.path === current.parent;
  }

  function open(side) {
    const pane = panes[side];
    const entries = selectedEntries(side).map(cloneEntry);
    if (!pane || !entries.length) return;
    const snapshot = {
      side,
      provider: pane.provider,
      profileId: pane.profileId,
      profileRevision: pane.profileRevision,
      profileName: pane.profileName,
      securityStatus: pane.securityStatus,
      sessionGeneration: pane.sessionGeneration,
      parent: pane.path,
      entries,
    };
    current = snapshot;
    active = false;
    modalToken = showOwnedModal("file-information", token => {
      modalToken = token;
      current = snapshot;
      active = true;
      const modal = $("#modal .modal");
      modal.classList.remove("quarantine-modal", "real-mode-modal");
      modal.classList.add("file-information-modal");
      $("#modal-title").textContent = entries.length === 1
        ? `Informationen · ${entries[0].name}`
        : `Informationen · ${entries.length.toLocaleString("de-DE")} Objekte`;
      const body = $("#modal-body");
      body.innerHTML = bodyHtml();
      body.onclick = handleBodyClick;
      commentEditor.connect(body);
      const buttons = $("#modal .modal-buttons");
      buttons.replaceChildren();
      const closeButton = document.createElement("button");
      closeButton.type = "button";
      closeButton.className = "btn primary";
      closeButton.textContent = "Schließen";
      closeButton.addEventListener("click", close);
      buttons.appendChild(closeButton);
      return {
        focusTarget: body.querySelector("[data-file-info-calculate]:not([disabled])") || closeButton,
        onEscape: close,
      };
    });
    void invoke("get_file_property_capabilities", { provider: snapshot.provider }).then(capabilities => {
      if (current !== snapshot || !active || !currentScopeMatches()) return;
      snapshot.propertyCapabilities = capabilities;
      refresh();
      if (capabilities.administration) void loadAdministratorStatus(snapshot);
      if (capabilities.tags) void tagEditor.load();
      if (capabilities.visibility) void visibilityEditor.load();
      if (capabilities.nativeFlags) for (const editor of flagEditors) void editor.load();
    }).catch(error => {
      if (current !== snapshot || !active) return;
      snapshot.propertyCapabilitiesError = error.message || String(error);
      refresh();
    });
  }

  function onDirectorySizeChange(side, entry) {
    if (!current || side !== current.side || !entry) return;
    if (!currentScopeMatches()) {
      close();
      return;
    }
    const index = current.entries.findIndex(item => item.name === entry.name);
    if (index < 0) return;
    current.entries[index].directorySize = cloneEntry(entry).directorySize;
    refresh();
  }

  return { close, onDirectorySizeChange, open };
}
