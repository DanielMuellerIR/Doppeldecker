import { invoke } from "./tauri-api.js";

/** Kapselt Vorschau und lokale Einweg-Ausführung. Der Backend-Kern berechnet den
 * Plan erneut und bindet jede Queue-Mutation weiterhin an den bestätigten Digest. */
export function createOneWaySyncUi({
  compare,
  currentSourceTarget,
  syncScope,
  discardPreparation,
  dockRevision = () => 0,
  ensureContentHash,
  prepareRemotePreview,
  previewAllowed,
  remoteSyncContext,
  runRemote,
  runLocal,
  runMirror,
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
    if (!simulate) return options.mode === "Mirror" ? runMirror() : runLocal(current);
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

    try {
      showDock("<span class=\"pill copy\">Einreihen</span>Simulierter Testlauf wird eingereiht. Es werden keine Dateien verändert.", { closable: true });
      const presentation = dockRevision();
      await invoke("run_sync", { source, target, options, expectedPlan: plan, simulation: true });
      if (current() && dockRevision() === presentation) {
        showDock("<span class=\"pill copy\">Eingereiht</span>Simulierter Testlauf wartet in der persistenten Queue. Es werden keine Dateien verändert.", { closable: true });
      }
      return true;
    } catch (error) {
      showError("Fehler", error);
    }
  }

  return { preview, run };
}
