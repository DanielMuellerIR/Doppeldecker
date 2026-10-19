// Lesender Informationsdialog für die aktuelle Pane-Auswahl.
//
// Das Frontend zeigt ausschließlich Metadaten, die das gebundene Listing geliefert
// hat. Ordnerinhalte kommen aus derselben fenster- und panegebundenen Größenmessung
// wie die Größenspalte; partielle Resultate werden nie zu einer exakten Summe erklärt.

import { hideModal, showOwnedModal } from "./modal.js";
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

export function createFileInformationUi({ panes, selectedEntries, requestDirectorySizes }) {
  let current = null;
  let modalToken = null;
  let active = false;

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
    ${detailEntries.length
      ? `<section class="file-info-folders" aria-label="Ausgewählte Einträge"><h3>${entries.length > 1 ? "Auswahl" : "Ordner"}</h3><ul>${detailEntries.map(entry => (
        `<li><span>${escapeHtml(entry.name)}</span><span>${escapeHtml(entryDetailText(entry))}</span></li>`
      )).join("")}</ul></section>`
      : ""}
    ${unsafe ? '<p class="file-info-note">Mindestens ein Name ist nicht eindeutig dekodierbar. Doppeldecker zeigt dafür keinen vollständigen Pfad und startet keine namensbasierte Größenberechnung.</p>' : ""}
    ${current.provider === "local" ? "" : '<p class="file-info-note">Remote-Provider zeigen nur Metadaten, die das jeweilige Protokoll verlässlich liefert. Fehlende Werte sind mit „—“ gekennzeichnet.</p>'}
    ${[...new Set(entries.flatMap(entry => entry.directorySize?.limitations || []))].map(note => `<p class="file-info-note">${escapeHtml(note)}</p>`).join("")}
    ${issueHtml(entries)}`;
  }

  function refresh() {
    if (!current || !modalToken || !active || $("#modal").hidden) return;
    const body = $("#modal-body");
    const restoreBodyFocus = body.contains(document.activeElement);
    body.innerHTML = bodyHtml();
    body.onclick = handleBodyClick;
    if (restoreBodyFocus) {
      // Der Größenstart ersetzt den gerade fokussierten Berechnen-Knopf durch
      // seinen deaktivierten Ladezustand. Der Fokus bleibt dann am nächsten
      // bedienbaren Element des Modals statt auf den Dokumentrumpf zu fallen.
      const target = body.querySelector("[data-file-info-calculate]:not([disabled])")
        || $("#modal .modal-buttons button:not([disabled])");
      target?.focus({ preventScroll: true });
    }
  }

  function close() {
    const token = modalToken;
    if (!token || !hideModal(token)) return false;
    active = false;
    modalToken = null;
    current = null;
    $("#modal .modal").classList.remove("file-information-modal");
    return true;
  }

  function handleBodyClick(event) {
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
  }

  function onDirectorySizeChange(side, entry) {
    if (!current || side !== current.side || !entry) return;
    if (!currentScopeMatches()) {
      close();
      return;
    }
    const index = current.entries.findIndex(item => item.name === entry.name);
    if (index < 0) return;
    current.entries[index] = cloneEntry(entry);
    refresh();
  }

  return { close, onDirectorySizeChange, open };
}
