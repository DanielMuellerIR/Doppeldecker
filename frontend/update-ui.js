import { choiceModal, isModalOpen } from "./modal.js";
import { invoke, listen } from "./tauri-api.js";
import { escapeHtml } from "./ui-utils.js";

/** Manuelle, zweistufige Update-Bedienung. Das Modul startet keine Prüfung selbst:
 * Nur der native Menüeintrag ruft `check` auf, und erst ein zweiter ausdrücklicher
 * Klick darf den im Backend gebundenen Release installieren. */
export function createUpdateUi({ beginActivity, running, showDock, showError }) {
  let busy = false;
  let pendingResult = null;

  async function install(version) {
    const finishActivity = beginActivity("update-install");
    if (!finishActivity) {
      showDock(
        `<span class="pill none">Update ${escapeHtml(version)} vorgemerkt</span>` +
          "Zuerst die laufende Dateioperation abschließen. Danach genügt ein erneuter Aufruf der Updateprüfung; der vorbereitete Fund bleibt erhalten.",
        { closable: true },
      );
      return false;
    }
    showDock(
      `<span class="pill update">Update ${escapeHtml(version)}</span>Signierter Release wird geladen und geprüft …`,
      {},
    );
    // Wirkungslose Vorbelegung: Schlägt schon das Registrieren des
    // Fortschritts-Listeners fehl, muss der Fehlerweg trotzdem durch `finally`
    // laufen und die Aktionssperre wieder freigeben. Stünde das `await` vor dem
    // `try`, bliebe das Fenster bis zum Neuladen gesperrt.
    let unlisten = () => {};
    try {
      unlisten = await listen("update-progress", event => {
        const downloaded = Number(event.payload?.downloaded || 0);
        const total = Number(event.payload?.total || 0);
        const detail = total > 0
          ? `${Math.min(100, Math.floor((downloaded / total) * 100))} %`
          : `${(downloaded / 1_000_000).toFixed(1)} MB`;
        // Während des Downloads ist ausschließlich der Ladestand belegt: Die
        // Signaturprüfung kann erst nach der vollständigen Nutzlast stattfinden.
        showDock(
          `<span class="pill update">Update ${escapeHtml(version)}</span>${detail} geladen …`,
          {},
        );
      });
      try {
        await invoke("install_update", { expectedVersion: version });
        pendingResult = null;
      } catch (error) {
        // Ein abgewiesenes Invoke kann bedeuten, dass der Command nie zugestellt
        // wurde, oder dass Download/Signatur erst nach dem Verbrauch scheiterten.
        // Nur der Backend-Readback darf diesen Unterschied entscheiden. Ist auch
        // er nicht erreichbar, bleibt der lokale Fund vorsichtshalber erhalten.
        try {
          const status = await invoke("prepared_update_status");
          if (!status.available || status.version !== version) pendingResult = null;
        } catch {
          // Unklarer Transportzustand: Beim nächsten Klick erneut nachfragen.
        }
        throw error;
      }
      showDock(
        `<span class="pill ok">Update installiert</span>Signatur geprüft; Version ${escapeHtml(version)} wird beim nächsten Start verwendet. Bitte Doppeldecker neu starten.`,
        { closable: true },
      );
    } finally {
      unlisten();
      finishActivity();
    }
    return true;
  }

  async function check() {
    if (busy) return;
    // Die native Menüaktion kann jederzeit kommen, auch während ein Rename-,
    // Konflikt- oder Realmodus-Dialog offen ist. Beide Dialoge teilen sich das
    // eine `#modal`: Ein Update-Dialog würde den offenen ersetzen und dessen
    // wartende Dateioperation dauerhaft hängen lassen.
    if (isModalOpen()) {
      showDock(
        "<span class=\"pill none\">Updateprüfung verschoben</span>Zuerst den offenen Dialog entscheiden, dann erneut prüfen.",
        { closable: true },
      );
      return;
    }
    busy = true;
    if (!pendingResult) {
      showDock("<span class=\"pill update\">Updateprüfung</span>Signierter Releasekanal wird abgefragt …", {});
    }
    try {
      // Ein bereits vom Backend vorbereiteter Updatefund bleibt lokal gebunden,
      // falls zwischen Prüfung und Installationsklick eine Dateioperation beginnt.
      const result = pendingResult || await invoke("check_for_update");
      if (!result.available) {
        pendingResult = null;
        showDock(
          `<span class="pill ok">Aktuell</span>Version ${escapeHtml(result.current_version)} ist die neueste verfügbare Version.`,
          { closable: true },
        );
        return;
      }
      pendingResult = result;
      const version = String(result.version || "");
      const notes = result.notes
        ? `<p>${escapeHtml(result.notes).replaceAll("\n", "<br>")}</p>`
        : "<p>Für diese Version liegen keine zusätzlichen Hinweise vor.</p>";
      // Zwischen Menüaufruf und HTTPS-Antwort kann beispielsweise F2, ein
      // Konflikt oder der Realmodus einen Dialog belegen. Die zentrale Modal-Queue
      // hält den Updatefund dann fest und zeigt ihn nach der laufenden Entscheidung,
      // ohne deren Promise oder Inhalt zu verdrängen.
      if (running()) {
        showDock(
          `<span class="pill none">Update ${escapeHtml(version)} vorgemerkt</span>` +
            "Die inzwischen gestartete Dateioperation läuft zuerst zu Ende. Danach genügt ein erneuter Aufruf der Updateprüfung.",
          { closable: true },
        );
        return;
      }
      if (isModalOpen()) {
        showDock(
          `<span class="pill none">Update ${escapeHtml(version)} vorgemerkt</span>Der Update-Dialog erscheint nach der offenen Entscheidung. Es wurde noch nichts geladen.`,
          { closable: true },
        );
      }
      const choice = await choiceModal(
        `Update ${version} verfügbar`,
        `<p>Installiert ist ${escapeHtml(result.current_version)}. Der Download wird vor der Installation mit dem fest eingebauten öffentlichen Schlüssel geprüft.</p>${notes}<p>Dateioperationen dieses Fensters bleiben während Download und Installation gesperrt.</p>`,
        [
          { label: "Später", value: "later", cls: "ghost" },
          { label: "Herunterladen und installieren", value: "install", cls: "primary" },
        ],
      );
      if (choice === "install") {
        const installed = await install(version);
        if (installed) pendingResult = null;
      } else {
        pendingResult = null;
        showDock("<span class=\"pill none\">Update verschoben</span>Es wurde nichts geladen oder installiert.", { closable: true });
      }
    } catch (error) {
      showError("Update", error);
    } finally {
      busy = false;
    }
  }

  return { check };
}
