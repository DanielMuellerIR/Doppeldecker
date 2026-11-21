import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

export async function runFavoriteCases(runCase, assertShape) {
  await runCase("Seitenleisten-Review alte Favoritenantwort erhält Entfernen", "", async page => {
    await page.locator("#fav-add").click();
    await page.locator("#fav-list .fav-item").waitFor();
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      let held = false;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await original(command, args);
        if (command === "add_favorite" && !held) {
          held = true;
          await new Promise(resolve => { window.__SIDEBAR_FAVORITE_RELEASE__ = resolve; });
        }
        return result;
      };
    });
    await page.locator("#fav-add").click();
    await page.waitForFunction(() => typeof window.__SIDEBAR_FAVORITE_RELEASE__ === "function");
    await page.locator("#fav-list .fav-del").click();
    await page.locator("#fav-list .fav-item").waitFor({ state: "detached" });
    await page.evaluate(async () => { window.__SIDEBAR_FAVORITE_RELEASE__(); await new Promise(resolve => setTimeout(resolve, 0)); });
    assert.equal(await page.locator("#fav-list .fav-item").count(), 0,
      "Die spätere Antwort einer älteren Änderung darf den bereits entfernten Favoriten nicht wieder anzeigen");
    assert.deepEqual(await page.evaluate(() => window.__TAURI__.core.invoke("list_favorites")), []);
  });

  await runCase("Seitenleisten-Review alte Favoritenliste erhält späteres Hinzufügen", "", async page => {
    await page.addInitScript(() => {
      Object.defineProperty(window, "__TAURI__", { configurable: true, set(tauri) {
        const original = tauri.core.invoke;
        let held = false;
        tauri.core.invoke = async (command, args) => {
          const result = await original(command, args);
          if (command === "list_favorites" && !held) {
            held = true;
            await new Promise(resolve => { window.__SIDEBAR_STARTUP_RELEASE__ = resolve; });
          }
          return result;
        };
        Object.defineProperty(window, "__TAURI__", { configurable: true, writable: true, value: tauri });
      } });
    });
    await page.reload({ waitUntil: "networkidle" });
    await page.waitForFunction(() => typeof window.__SIDEBAR_STARTUP_RELEASE__ === "function");
    await page.locator("#fav-add").click();
    await page.locator("#fav-list .fav-item").waitFor();
    await page.evaluate(async () => { window.__SIDEBAR_STARTUP_RELEASE__(); await new Promise(resolve => setTimeout(resolve, 0)); });
    assert.equal(await page.locator("#fav-list .fav-item").count(), 1,
      "Die ältere Startliste darf den neu hinzugefügten Favoriten nicht ausblenden");
    assert.equal((await page.evaluate(() => window.__TAURI__.core.invoke("list_favorites"))).length, 1);
  });

  await runCase("Seitenleisten-Review Favoriten-Readbackfehler nennt die bereits gespeicherte Änderung", "", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__SIDEBAR_READ_FAVORITES__ = () => original("list_favorites");
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "list_favorites") throw { code: "io", message: "Gezielter Favoriten-Lesefehler" };
        return original(command, args);
      };
    });
    await page.locator("#fav-add").click();
    await page.locator("#dock-summary").filter({ hasText: /Favoriten geändert, Ansicht nicht aktualisiert/ }).waitFor();
    assert.match(await page.locator("#dock-summary").innerText(), /Gezielter Favoriten-Lesefehler/);
    assert.equal((await page.evaluate(() => window.__SIDEBAR_READ_FAVORITES__())).length, 1);
  });

  await runCase("Seitenleisten-Review erster Favoriten-Lesefehler bleibt sichtbar", "", async page => {
    await page.addInitScript(() => {
      Object.defineProperty(window, "__TAURI__", { configurable: true, set(tauri) {
        const original = tauri.core.invoke;
        tauri.core.invoke = async (command, args) => {
          if (command === "list_favorites") {
            window.__SIDEBAR_STARTUP_FAILED__ = true;
            throw { code: "invalid_data", message: "Gezielte beschädigte Favoritendatei" };
          }
          return original(command, args);
        };
        Object.defineProperty(window, "__TAURI__", { configurable: true, writable: true, value: tauri });
      } });
    });
    await page.reload({ waitUntil: "networkidle" });
    await page.waitForFunction(() => window.__SIDEBAR_STARTUP_FAILED__);
    await page.evaluate(() => new Promise(resolve => setTimeout(resolve, 0)));
    assert.match(await page.locator("#dock-summary").innerText(), /Favoriten laden.*Gezielte beschädigte Favoritendatei/s,
      "Ein fehlgeschlagener erster Read darf keine kommentarlos leere Favoritenliste vortäuschen");
    assert.equal(await page.locator("#fav-list .fav-item").count(), 0);
  });

  await runCase("Lokale Favoriten hinzufügen und entfernen", "", async page => {
    await page.locator("#fav-add").click();
    assert.match(await page.locator("#fav-list").innerText(), /quelle/i);
    await page.locator("#fav-list .fav-del").click();
    assert.match(await page.locator("#fav-list").innerText(), /Noch keine Favoriten/);
  });

  await runCase("Remote-Favorit bindet Provider, Profil und Pfad", "", async page => {
    const profile = page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]');
    await profile.click();
    await page.locator("#fav-add").click();
    const favorite = page.locator("#fav-list .fav-item").first();
    await favorite.waitFor();
    assert.match(await favorite.getAttribute("title"), /SFTP · \//);
    assert.equal(await favorite.locator(".side-icon.sftp").count(), 1);

    // Ein Klick navigiert über die gespeicherte Profil-ID wieder in denselben
    // Remote-Endpoint; erst danach wird der Favorit endpointgenau entfernt.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "home"));
    await favorite.click();
    assert.match(await page.getByRole("textbox").first().getAttribute("title"), /SFTP · Mock-SFTP/);
    await favorite.locator(".fav-del").click();
    assert.match(await page.locator("#fav-list").innerText(), /Noch keine Favoriten/);
  });

  await runCase("Remote-Ordner aller Provider per Drag zu Favoriten", "", async page => {
    const providers = [
      ["sftp", "mock_sftp", "SFTP"],
      ["ftps", "mock_ftps", "FTPS"],
      ["webdav", "mock_webdav", "WEBDAV"],
      ["smb", "mock_smb", "SMB"],
    ];
    for (const [provider, profileId, titleProvider] of providers) {
      await page.locator(
        `#connection-list [data-protocol="${provider}"][data-id="${profileId}"]`,
      ).click();
      await page.waitForFunction(
        provider => {
          const path = document.querySelector('.pane[data-side="left"] .path');
          return path?.title?.toLowerCase().includes(provider)
            && path.value === "/"
            && document.querySelector('.pane[data-side="left"] .row');
        },
        provider,
      );
      await page.evaluate(() => {
        const row = document.querySelector('.pane[data-side="left"] .row');
        if (!row || row.getAttribute("draggable") !== "true") {
          throw new Error("Remote-Ordnerzeile ist nicht draggable.");
        }
        const dataTransfer = new DataTransfer();
        row.dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer }));
        const list = document.querySelector("#fav-list");
        list.dispatchEvent(new DragEvent("dragover", {
          bubbles: true, cancelable: true, dataTransfer,
        }));
        list.dispatchEvent(new DragEvent("drop", {
          bubbles: true, cancelable: true, dataTransfer,
        }));
        row.dispatchEvent(new DragEvent("dragend", { bubbles: true, dataTransfer }));
      });
      const favorite = page.locator(`#fav-list .fav-item:has(.side-icon.${provider})`);
      await favorite.waitFor();
      assert.equal(await favorite.getAttribute("title"), `${titleProvider} · /remote`);
      await favorite.click();
      await page.waitForFunction(
        provider => {
          const path = document.querySelector('.pane[data-side="left"] .path');
          return path?.title?.toLowerCase().includes(provider) && path.value === "/remote";
        },
        provider,
      );
    }
    assert.equal(await page.locator("#fav-list .fav-item").count(), providers.length);
  });

  await runCase("Finder-Favoriten übernehmen ist additiv und bilanziert sichtbar", "", async page => {
    // Erster Import über den Menüpunkt: der Mock liefert zwei Kandidaten.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "import_finder_favorites"));
    await page.locator("#fav-list .fav-item").first().waitFor();
    const list = await page.locator("#fav-list").innerText();
    assert.match(list, /Finder-Ordner/);
    assert.match(list, /quelle/);
    let dock = await page.locator("#dock-summary").innerText();
    assert.match(dock, /2 übernommen/);
    assert.match(dock, /1 kein \(vorhandener\) Ordner, 1 im Finder ausgeblendet/);
    // Zweiter Import: nichts wird doppelt angelegt, die Bilanz sagt das klar.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "import_finder_favorites"));
    await page.locator("#dock-summary").filter({ hasText: "2 bereits vorhanden" }).waitFor();
    dock = await page.locator("#dock-summary").innerText();
    assert.match(dock, /0 übernommen/);
    assert.equal(await page.locator("#fav-list .fav-item").count(), 2);
  });

  await runCase("Favoriten verwenden denselben IPC-Vertrag im Browser-Mock", "", async page => {
    const wire = JSON.parse(await readFile(new URL("./favorite-wire.json", import.meta.url)));
    const { local, remote, summary, moved, removed, invalid } = await page.evaluate(async () => {
      const api = await import("/favorites-api.js");
      const local = await api.addLocalFavorite({ name: "Fotos", path: "/mock/quelle" });
      const endpoint = { provider: "sftp", profileId: "mock_sftp", path: "/daten" };
      const remote = await api.addRemoteFavorite({ ...endpoint, name: "Archiv" });
      const summary = await api.importFinderFavorites();
      const moved = await api.moveFavorite({ ...endpoint, index: 0 });
      const removed = await api.removeFavorite(endpoint);
      const invalid = await api.removeFavorite({ ...endpoint, provider: "SFTP" })
        .then(() => null, error => error.code);
      return { local, remote, summary, moved, removed, invalid };
    });
    assertShape(local[0], wire.local);
    assertShape(remote[1], wire.remote);
    assert.equal(remote[1].profile_id, "mock_sftp");
    assertShape(summary, wire.finderImport);
    for (const item of summary.favorites) {
      assert.ok(wire.providers.includes(item.provider));
      assertShape(item, item.provider === "local" ? wire.local : wire.remote);
    }
    assert.equal(moved[0].name, "Archiv");
    assert.equal(removed.some(item => item.name === "Archiv"), false);
    assert.equal(invalid, "invalid_input");
  });
}
