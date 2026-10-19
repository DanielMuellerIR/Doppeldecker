import { $ } from "./ui-utils.js";

/** Eigenständiger Sync-Modus: ein bewusst betretener Overlay-Zustand mit
 * Optionen-Panel und Plan-Vorschau. Hält ausschließlich die sichtbare Auswahl und
 * die Ansicht-Ereignisse; der Kern validiert Optionen, Capabilities und Plan
 * unabhängig von diesem UI-Zustand erneut. */
export function createSyncView({
  discardPreparation,
  endpoints,
  onDirectionChanged,
  onMetadataChanged,
  remoteSyncContext,
  running,
}) {
  let direction = "lr";
  let mode = "AddOnly";
  let compare = "SizeAndTime";
  let metadata = "Portable";
  let wiredActions = null;
  let previewTimer = null;
  let revision = 0;
  let realTestEnabled = false;

  const view = () => $("#sync-view");
  const isOpen = () => !view().hidden;

  // ---------------------------------------------------------------------
  // Sichtbare Auswahl <-> interner Zustand
  // ---------------------------------------------------------------------
  function readEffects() {
    const mirror = $("#sync-mirror").checked;
    const update = $("#sync-update").checked;
    mode = mirror ? "Mirror" : update ? "Update" : "AddOnly";
    // Spiegeln schließt Aktualisieren fachlich ein; die Checkbox spiegelt das
    // sichtbar und bleibt gesperrt, solange Verwaiste entfernt werden sollen.
    $("#sync-update").checked = mirror || update;
    $("#sync-update").disabled = mirror;
  }

  function applyMode() {
    $("#sync-mirror").checked = mode === "Mirror";
    $("#sync-update").checked = mode !== "AddOnly";
    $("#sync-update").disabled = mode === "Mirror";
  }

  function applyMetadata() {
    const radio = $(`#sync-metadata-group [value=${metadata}]`);
    if (radio) radio.checked = true;
    onMetadataChanged();
  }

  function ensureContentHash() {
    if (compare === "ContentHash") return;
    compare = "ContentHash";
    const radio = $("#sync-compare-group [value=ContentHash]");
    if (radio) radio.checked = true;
  }

  // ---------------------------------------------------------------------
  // Kopf-Chips: Wer ist Quelle, wer ist Ziel?
  // ---------------------------------------------------------------------
  function protoLabel(pane) {
    if (!pane || pane.provider === "local" || !pane.provider) return "Lokal";
    if (pane.provider === "ftps" && pane.securityStatus === "plaintext") return "FTP · unverschlüsselt";
    if (pane.provider === "ftps" && pane.securityStatus === "tls_identity_unverified") {
      return "FTPS · Identität ungeprüft";
    }
    return { sftp: "SFTP", ftps: "FTPS", webdav: "WebDAV", smb: "SMB" }[pane.provider] || pane.provider;
  }

  function renderExecution() {
    const fast = $("#sync-compare-group [value=SizeAndTime]");
    fast.disabled = metadata !== "Portable" || mode === "Update";
    fast.parentElement.title = fast.disabled ? "Update und native Profile benötigen den vollständigen Inhaltsvergleich." : "";
    const remote = remoteSyncContext();
    const both = direction === "both";
    // Der bidirektionale Request besitzt keine Einwegoptionen. Seine festen
    // Inhalts-/Metadatenbelege dürfen keine hier wirkungslose Auswahl vortäuschen.
    for (const id of ["sync-effects-group", "sync-compare-group", "sync-metadata-group", "sync-metadata-note-title", "btn-real"]) {
      $(`#${id}`).hidden = both;
    }
    const normalLocal = !both && !remote && mode !== "Mirror";
    $("#btn-sync-simulate").hidden = both || Boolean(remote) || (mode === "Mirror" && !realTestEnabled);
    $("#btn-sync").textContent = both ? "Bidirektionalen Sync vorbereiten" : remote || normalLocal ? "Sync-Ausführung vorbereiten" : realTestEnabled ? "Test-Sync ausführen" : "Mirror-Testlauf simulieren";
    $("#sync-execution-note").textContent = both || remote || normalLocal
      ? "Vorschau und Bestätigung folgen vor der echten Ausführung."
      : realTestEnabled ? "Echte Schreibwirkung ausschließlich in der markierten Testsandbox."
      : "Vorschau: nur vergleichen. Testlauf: simuliert den Plan, ohne Dateien zu verändern.";
  }

  function renderChips() {
    renderExecution();
    const { source, target, sourcePane, targetPane } = endpoints();
    const both = direction === "both";
    // Bei „in beide Richtungen" gibt es keine Quelle/Ziel-Rollen; die Chips
    // zeigen dann neutral die beiden Panes (endpoints() liefert links zuerst).
    $("#sync-source-role").textContent = both ? "Links" : "Quelle";
    $("#sync-target-role").textContent = both ? "Rechts" : "Ziel";
    $("#sync-arrow").textContent = both ? "↔" : "→";
    // Die Chips ellipsieren lange Pfade vorn (CSS direction: rtl), damit das
    // aussagekräftige Ende sichtbar bleibt. Damit dabei führende/abschließende
    // Slashes nicht umsortiert werden, bleibt der Pfad ein expliziter LTR-Lauf.
    const ltr = text => "\u202A" + (text || "") + "\u202C";
    $("#sync-source-path").textContent = ltr(source);
    $("#sync-source-path").title = source || "";
    $("#sync-target-path").textContent = ltr(target);
    $("#sync-target-path").title = target || "";
    $("#sync-source-proto").textContent = protoLabel(sourcePane);
    $("#sync-target-proto").textContent = protoLabel(targetPane);
  }

  // ---------------------------------------------------------------------
  // Vorschau-Planung: lokal automatisch, remote bewusst per Knopf (die
  // Remote-Vorschau bindet Identität, Listings und ein einmaliges Permit).
  // ---------------------------------------------------------------------
  function clearPlanArea() {
    $("#sync-summary").innerHTML = "";
    $("#sync-actions").innerHTML = "";
    $("#sync-actions").hidden = false;
    $("#sync-comparison").hidden = true;
    $("#sync-comparison-list").innerHTML = "";
    $("#sync-comparison-categories").textContent = "";
  }

  function showHint(text, { remoteButton = false } = {}) {
    clearPlanArea();
    $("#sync-hint-text").textContent = text;
    $("#sync-preview-remote").hidden = !remoteButton;
    $("#sync-hint").hidden = false;
  }

  function schedulePreview() {
    if (!isOpen() || running()) return;
    clearTimeout(previewTimer);
    previewTimer = setTimeout(() => {
      if (!isOpen() || running()) return;
      if (direction === "both") {
        showHint(
          "In beide Richtungen gleicht beide Panes gegen die gemeinsame Baseline ab. " +
            "Alle Dateiinhalte werden vollständig mit BLAKE3 verglichen; das Metadatenprofil ist Portable. " +
            "Ersetzungen und Entfernungen ergeben sich aus Baseline und bestätigten Konfliktentscheidungen. " +
            "Umfang und Plan-Digest zeigt der Bestätigungsdialog unmittelbar vor der Ausführung.",
        );
        return;
      }
      const remote = remoteSyncContext();
      if (remote) {
        showHint(
          "Mindestens ein Endpunkt ist ein Server. Die Vorschau verbindet sich, bindet " +
            "Identität und vollständige Listings und erzeugt ein einmaliges, kurzlebiges Permit — " +
            "deshalb startet sie hier bewusst per Knopf.",
          { remoteButton: true },
        );
        return;
      }
      $("#sync-hint").hidden = true;
      wiredActions?.preview();
    }, 160);
  }

  function optionChanged() {
    revision += 1;
    renderExecution();
    discardPreparation();
    schedulePreview();
  }

  // ---------------------------------------------------------------------
  // Öffnen/Schließen des Modus
  // ---------------------------------------------------------------------
  function open() {
    if (isOpen() || running()) return;
    revision += 1;
    $("#app").classList.add("sync-mode");
    view().hidden = false;
    renderChips();
    clearPlanArea();
    schedulePreview();
    $("#sync-direction-group input:checked")?.focus();
  }

  function close({ restoreFocus = true } = {}) {
    if (!isOpen()) return;
    revision += 1;
    clearTimeout(previewTimer);
    view().hidden = true;
    $("#app").classList.remove("sync-mode");
    discardPreparation();
    if (restoreFocus) $(".pane.active .listing")?.focus();
  }

  // ---------------------------------------------------------------------
  // Verdrahtung
  // ---------------------------------------------------------------------
  function wire(actions) {
    wiredActions = actions;
    const start = async action => {
      if (running()) return;
      const ownerRevision = revision;
      const queued = await action();
      if (queued && ownerRevision === revision) close();
    };

    $("#sync-direction-group").addEventListener("change", event => {
      direction = event.target.value;
      onDirectionChanged();
      renderChips();
      optionChanged();
    });

    $("#sync-effects-group").addEventListener("change", () => {
      readEffects();
      // Dieselben zwei Kerninvarianten wie im Metadaten-Handler, nur in der anderen
      // Richtung. `NativeBestEffort` ist für Mirror gesperrt (core sync/plan.rs), und
      // native Profile verlangen den vollständigen Inhaltsvergleich. Ohne diese
      // Korrektur bot die Ansicht beide Optionen weiter als gültig an, während jede
      // Vorschau und jeder Lauf danach mit einem Fehler endete.
      if (mode === "Mirror" && metadata === "NativeBestEffort") {
        metadata = "NativeStrict";
        applyMetadata();
      }
      if (metadata !== "Portable") ensureContentHash();
      if (mode === "Update") ensureContentHash();
      renderExecution();
      optionChanged();
    });

    $("#sync-compare-group").addEventListener("change", event => {
      compare = event.target.value;
      if (metadata !== "Portable" || mode === "Update") ensureContentHash();
      optionChanged();
    });

    $("#sync-metadata-group").addEventListener("change", event => {
      metadata = event.target.value;
      // Native Profile sind ohne vollständigen Inhaltsvergleich nicht ausführbar.
      // Der Kern prüft dieselbe Invariante unabhängig von dieser sichtbaren Auswahl.
      if (metadata !== "Portable") ensureContentHash();
      if (metadata === "NativeBestEffort" && mode === "Mirror") {
        mode = "Update";
        applyMode();
      }
      onMetadataChanged();
      optionChanged();
    });

    $("#sync-preview-remote").addEventListener("click", () => {
      $("#sync-hint").hidden = true;
      actions.preview();
    });

    $("#btn-sync-close").addEventListener("click", () => close());

    $("#btn-sync").addEventListener("click", async () => {
      if (running()) return;
      if (direction === "both") return actions.bidirectional();
      // Nach erfolgreichem Einreihen in die Queue schließt sich der Modus; bei
      // abgebrochenen Bestätigungen bleibt er mit dem sichtbaren Plan offen.
      await start(actions.sync);
    });

    $("#btn-real").addEventListener("click", () => {
      if (direction === "both") return;
      // Die beim Öffnen geplante Vorschau darf den gerade begonnenen
      // Bestätigungsdialog nicht durch discardPreparation entwerten.
      clearTimeout(previewTimer);
      actions.real();
    });
    $("#btn-sync-simulate").addEventListener("click", () => start(actions.simulate));
  }

  return {
    close,
    setSafetyInfo: info => { realTestEnabled = Boolean(info.real_enabled); renderExecution(); },
    compare: () => compare,
    direction: () => direction,
    ensureContentHash,
    isOpen,
    metadata: () => metadata,
    mode: () => mode,
    open,
    revision: () => revision,
    options: () => ({ mode, compare, metadata, mtime_tolerance: 2 }),
    toggle: () => (isOpen() ? close() : open()),
    wire,
  };
}
