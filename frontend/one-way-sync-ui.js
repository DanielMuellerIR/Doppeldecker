import { confirmModal, promptModal } from "./modal.js";
import { invoke } from "./tauri-api.js";
import { baseName, escapeHtml, fmtBytes } from "./ui-utils.js";

/** Kapselt Vorschau und lokale Einweg-Ausführung. Der Backend-Kern berechnet den
 * Plan erneut und bindet jede Queue-Mutation weiterhin an den bestätigten Digest. */
export function createOneWaySyncUi({
  compare,
  currentSourceTarget,
  syncScope,
  discardPreparation,
  dockRevision = () => 0,
  ensureContentHash,
  metadata,
  prepareRemotePreview,
  previewAllowed,
  remoteSyncContext,
  runRemote,
  runLocal,
  running,
  showDock,
  showError,
  showPlan,
  syncOptions,
}) {
  let generation = 0;
  let starting = false;
  let pendingPreview = false;

  async function preview() {
    if (starting) {
      pendingPreview = true;
      return;
    }
    const owner = ++generation;
    const remote = remoteSyncContext();
    if (remote?.protocol === "webdav"
      || remote?.source?.provider === "webdav"
      || remote?.target?.provider === "webdav") ensureContentHash();
    const scope = syncScope();
    const current = () => owner === generation && scope === syncScope();
    await discardPreparation();
    if (!current()) return;
    const { source, target } = currentSourceTarget();
    if (!source || !target) return;
    if (compare() === "ContentHash") {
      showDock("<span class=\"pill update\">BLAKE3</span>Inhalte werden vollständig gelesen …", {});
    }
    try {
      if (remote?.error) throw new Error(remote.error);
      if (remote) {
        await prepareRemotePreview(remote);
      } else {
        const plan = await invoke("plan_sync", { source, target, options: syncOptions() });
        if (current()) showPlan(plan);
      }
    } catch (error) {
      if (current()) showError("Fehler", error);
    }
  }

  async function run({ simulate = false } = {}) {
    if (running() || starting) return;
    starting = true;
    const owner = ++generation;
    const scope = syncScope();
    const current = () => owner === generation && scope === syncScope();
    let queued = false;
    try {
      queued = await runCurrent(current, simulate);
      return queued;
    } finally {
      starting = false;
      // Ein Optionswechsel kann den Startscan entwerten, während dessen genau
      // eine entprellte Vorschau ankommt. Für die weiterhin sichtbare Ansicht
      // muss sie danach den aktuellen Scope planen.
      const requested = pendingPreview;
      pendingPreview = false;
      if (requested && !queued && previewAllowed()) await preview();
    }
  }

  async function runCurrent(current, simulate) {
    const { source, target } = currentSourceTarget();
    const options = syncOptions();
    if (!source || !target) return;
    const remote = remoteSyncContext();
    if (remote?.error) return showError("Remote gesperrt", new Error(remote.error));
    // Ein fehlgeschlagenes Remote-Enqueue oder ein noch nicht gestarteter Lauf
    // besitzt weiterhin genau diese Backendvorbereitung. `runRemote` vergleicht sie
    // mit dem aktuellen Endpunkt und verwirft sie nur bei einer echten Abweichung.
    // Lokale Läufe können eine solche Remotevorbereitung dagegen nicht verwenden.
    if (remote) return runRemote(remote);
    if (!simulate && options.mode !== "Mirror") return runLocal(current);
    await discardPreparation();
    if (!current()) return;
    if (source === target) {
      showDock("<span class=\"pill delete\">Quelle und Ziel sind identisch</span>", { closable: true });
      return;
    }

    let plan;
    if (compare() === "ContentHash") {
      showDock("<span class=\"pill update\">BLAKE3</span>Inhalte werden vollständig gelesen …", {});
    }
    try {
      plan = await invoke("plan_sync", { source, target, options });
    } catch (error) {
      if (current()) showError("Fehler", error);
      return;
    }
    if (!current()) return;
    showPlan(plan);
    if (plan.actions.length === 0) return;

    if (options.metadata === "NativeBestEffort") {
      const omitted = (plan.metadata_report?.issues || []).length;
      const accepted = await confirmModal(
        "Natives Best-Effort-Profil bestätigen",
        "Darstellbare native Metadaten werden exakt übertragen und rückgelesen. " +
          `<b>${omitted}</b> nicht exakt darstellbare Merkmale werden ausgelassen und in Vorschau sowie Abschlussbilanz einzeln genannt. ` +
          "Es findet keine Übersetzung inkompatibler ACL-Modelle statt.",
      );
      if (!accepted || !current()) return;
    }

    const info = await invoke("safety_info");
    if (!current()) return;
    if (simulate || !info.real_enabled) {
      try {
        showDock("<span class=\"pill copy\">Einreihen</span>Simulierter Testlauf wird eingereiht. Es werden keine Dateien verändert.", { closable: true });
        const presentation = dockRevision();
        await invoke("run_sync", { source, target, options, expectedPlan: plan, simulation: true });
        // Ein Terminalevent oder eine neuere Meldung kann der Antwort vorauslaufen.
        if (current() && dockRevision() === presentation) {
          showDock("<span class=\"pill copy\">Eingereiht</span>Simulierter Testlauf wartet in der persistenten Queue. Es werden keine Dateien verändert.", { closable: true });
        }
        // Der Rückgabewert meldet dem Sync-Modus „erfolgreich eingereiht", damit
        // sich die Ansicht schließt; abgebrochene Gates lassen sie offen.
        return true;
      } catch (error) {
        showError("Fehler", error);
      }
      return;
    }

    // Diese sichtbaren Vorabhinweise sind keine Freigabe: Der Kern prüft dieselben
    // harten Gates unmittelbar vor jeder Mutation nochmals am gebundenen Plan.
    if (plan.delete_count > 0) {
      const fraction = plan.target_count > 0 ? plan.delete_count / plan.target_count : 0;
      if (plan.source_count === 0) {
        return showDock("<span class=\"pill delete\">Gesperrt</span>Eine leere Quelle darf keinen Zielbestand entfernen.", { closable: true });
      }
      if (plan.skip_count > 0) {
        return showDock(
          `<span class="pill delete">Gesperrt</span>${plan.skip_count} Konflikt-/Symlink-/Spezialeinträge wurden ausgelassen. ` +
            "Solange die Vorschau unvollständig ist, entfernt Mirror nichts.",
          { closable: true },
        );
      }
      if (plan.delete_count > info.max_delete_entries || fraction > info.max_delete_fraction) {
        return showDock(
          `<span class="pill delete">Löschgrenze</span>${plan.delete_count} von ${plan.target_count} Zieleinträgen ` +
            `(${(fraction * 100).toFixed(1)} %) überschreiten die harte Testgrenze.`,
          { closable: true },
        );
      }
    }

    if (plan.update_count > 0 || plan.metadata_count > 0 || plan.delete_count > 0) {
      const accepted = await confirmModal(
        "Zielbestand wird verändert",
        `<b>Quelle:</b> <code>${escapeHtml(source)}</code><br>` +
          `<b>Ziel:</b> <code>${escapeHtml(target)}</code><br><br>` +
          `<b>${plan.update_count}</b> Datei(en) werden ersetzt; ` +
          `<b>${plan.metadata_count}</b> bestehende Metadatensätze werden angepasst; ` +
          `<b>${plan.delete_count}</b> Einträge (${fmtBytes(plan.delete_bytes)}) werden wiederherstellbar in die Quarantäne verschoben.<br><br>` +
          `Test-Sandbox: <code>${escapeHtml(info.sandbox || "")}</code>`,
      );
      if (!accepted || !current()) return;
    }

    if (plan.delete_count > 0) {
      const expectedName = baseName(target);
      const typed = await promptModal(`Zur Bestätigung Zielnamen „${expectedName}“ eingeben`, "");
      if (!current()) return;
      if (typed !== expectedName) {
        return showDock("<span class=\"pill none\">Abgebrochen</span>Zielname wurde nicht bestätigt.", { closable: true });
      }
    }
    try {
      showDock("<span class=\"pill copy\">Einreihen</span>Bestätigter Test-Sync wird eingereiht.", { closable: true });
      const presentation = dockRevision();
      await invoke("run_sync", { source, target, options, expectedPlan: plan });
      if (current() && dockRevision() === presentation) {
        showDock("<span class=\"pill copy\">Eingereiht</span>Bestätigter Test-Sync wartet in der persistenten Queue.", { closable: true });
      }
      return true;
    } catch (error) {
      showError("Fehler", error);
    }
  }

  return { preview, run };
}
