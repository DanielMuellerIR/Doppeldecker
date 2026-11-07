#!/usr/bin/env node
// Echte Oberfläche, kontrollierte Leseantworten; keine Nutzerdateien oder Appdaten.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { createServer } from "node:net";
import { chromium } from "playwright";
import { createBrowserCleanup } from "./browser-cleanup.mjs";

const probe = createServer(); probe.listen(0, "127.0.0.1"); await once(probe, "listening");
const port = probe.address().port; probe.close(); await once(probe, "close");
const server = spawn("python3", ["tools/mock-ui-server.py", "--port", String(port)], {
  cwd: new URL("..", import.meta.url), stdio: ["ignore", "ignore", "pipe"],
});
server.stderr.resume();
const cleanup = createBrowserCleanup(server);
let browser;
let failures = 0;
async function setup(page, query = "") {
  await page.goto(`http://127.0.0.1:${port}/${query}`, { waitUntil: "networkidle" });
  await page.getByRole("listbox", { name: /Linke Dateiliste/ }).waitFor();
  await page.waitForFunction(() => document.querySelector('[data-side="left"] .path').value.length > 0);
  await page.evaluate(() => {
    const original = window.__TAURI__.core.invoke;
    window.__SEARCH_CALLS__ = [];
    window.__SEARCH_CANCELS__ = [];
    window.__SEARCH_HOLD__ = false;
    window.__SEARCH_RELEASES__ = [];
    window.__TAURI__.core.invoke = async (command, args) => {
      if (command === "start_name_search") {
        const request = structuredClone(args.request);
        const runId = window.__SEARCH_CALLS__.push(request);
        if (window.__SEARCH_HOLD__) await new Promise(resolve => window.__SEARCH_RELEASES__.push(resolve));
        return { run_id: runId };
      }
      if (command === "cancel_name_search") {
        window.__SEARCH_CANCELS__.push(structuredClone(args)); return null;
      }
      return original(command, args);
    };
    window.__SEARCH_EMIT__ = (index, result, failed = false) => {
      const request = window.__SEARCH_CALLS__[index];
      window.__MOCK_EMIT__(failed ? "name-search-error" : "name-search-update", {
        run_id: index + 1, client_token: request.clientToken, side: request.side, result,
      });
    };
  });
}
const hit = (name, parent = "/mock/quelle") => ({ name, parent, relative_path: name, is_dir: false, is_symlink: false });
const update = (status, hits = [], issues = [], extra = {}) => ({ status, hits, issues, limit: null,
  stats: { entries: 101, directories: 3, hits: hits.length, skipped: issues.length, issues: issues.length }, ...extra });
async function emit(page, index, result) { await page.evaluate(([i, result]) => window.__SEARCH_EMIT__(i, result), [index, result]); }
async function open(page, side = "left") {
  await page.locator(`[data-side="${side}"] .pane-search`).click();
  return page.getByRole("dialog", { name: "Namen in Unterordnern suchen" });
}
async function start(page, dialog, query = "txt", count = 1) {
  await dialog.getByLabel("Name enthält", { exact: true }).fill(query);
  await dialog.getByRole("button", { name: "Suchen", exact: true }).click();
  await page.waitForFunction(count => window.__SEARCH_CALLS__.length === count, count);
}
async function test(name, action, query = "") {
  const context = await browser.newContext(); const page = await context.newPage();
  page.setDefaultTimeout(5000);
  const errors = []; page.on("pageerror", error => errors.push(error.message));
  page.on("console", message => { if (message.type() === "error") errors.push(message.text()); });
  try {
    await setup(page, query); await action(page);
    assert.deepEqual(errors, []);
    assert.deepEqual(await page.evaluate(() => window.__DOPPELDECKER_MOCK_ERRORS__), []);
    console.log(`ok ${name}`);
  } catch (error) { failures += 1; console.error(`FEHLER ${name}\n${error.stack}\nBrowser: ${errors.join("; ")}`); }
  finally { await context.close(); }
}
try {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    try { if ((await fetch(`http://127.0.0.1:${port}/`)).ok) break; } catch (_) { /* Serverstart */ }
    await new Promise(resolve => setTimeout(resolve, 25));
  }
  browser = await cleanup.launch(chromium, { headless: true, executablePath: process.env.DOPPELDECKER_PLAYWRIGHT_EXECUTABLE || undefined });
  for (const method of ["Klick", "Enter"]) {
    await test(`Start per ${method} hält den Dialogfokus und Escape cancelt vor der Lauf-ID`, async page => {
      await page.evaluate(() => { window.__SEARCH_HOLD__ = true; });
      const dialog = await open(page);
      const input = dialog.getByLabel("Name enthält", { exact: true });
      await input.fill("txt");
      if (method === "Klick") await dialog.getByRole("button", { name: "Suchen", exact: true }).click();
      else await input.press("Enter");
      await page.waitForFunction(() => window.__SEARCH_CALLS__.length === 1);
      assert.equal(await dialog.evaluate(element => element.contains(document.activeElement)), true,
        "Das Deaktivieren des Auslösers darf den Fokus nicht auf body verlieren.");
      await page.keyboard.press("Escape");
      await dialog.waitFor({ state: "hidden" });
      await page.waitForFunction(() => window.__SEARCH_CANCELS__.length === 1);
      const token = await page.evaluate(() => window.__SEARCH_CALLS__[0].clientToken);
      assert.equal(await page.evaluate(() => window.__SEARCH_CANCELS__[0].clientToken), token);
      await page.evaluate(() => window.__SEARCH_RELEASES__[0]());
      await page.waitForFunction(() => window.__SEARCH_CANCELS__.length === 2);
      assert.equal(await page.evaluate(token => window.__SEARCH_CANCELS__.every(call => call.clientToken === token), token), true);
    });
  }
  await test("Suche zeigt Teilbilanz, alle Ergebnisseiten und pfadgenaue Fehler", async page => {
    const dialog = await open(page); await start(page, dialog);
    const hits = Array.from({ length: 101 }, (_, index) => hit(`Treffer${index}.txt`));
    await emit(page, 0, update("running", hits));
    assert.equal(await dialog.locator(".name-search-hit").count(), 100);
    await dialog.getByRole("button", { name: "Weiter", exact: true }).click();
    await dialog.getByRole("button", { name: "Datei · Treffer100.txt", exact: true }).waitFor();
    await emit(page, 0, update("partial", [], [{ path: "/mock/quelle/gesperrt", message: "Kein Zugriff", code: "permission_denied" }], {
      stats: { entries: 150, directories: 4, hits: 101, skipped: 1, issues: 1 },
    }));
    await dialog.getByRole("status").filter({ hasText: "Unvollständige Suche" }).waitFor();
    await dialog.getByText(/Fehler und ausgelassene Namen/).click();
    assert.match(await dialog.locator("pre").textContent(), /\/mock\/quelle\/gesperrt: Kein Zugriff/);
    await dialog.getByRole("button", { name: "Zurück", exact: true }).click();
    assert.equal(await dialog.locator(".name-search-hit").count(), 100);
  });
  await test("Treffer navigiert zum Elternordner und leert den Schnellfilter", async page => {
    const filter = page.locator('[data-side="left"] .pane-filter-text');
    await filter.fill("kein Treffer");
    const dialog = await open(page); await start(page, dialog, "Beispiel");
    await emit(page, 0, update("complete", [hit("Beispiel.txt", "/mock/quelle")]));
    await dialog.getByRole("button", { name: "Datei · Beispiel.txt", exact: true }).click();
    await dialog.waitFor({ state: "hidden" });
    await page.waitForFunction(() => document.querySelector('[data-side="left"] .pane-filter-text').value === "");
    assert.equal(await page.locator('[data-side="left"] .path').inputValue(), "/mock/quelle");
    await page.locator('[data-side="left"] [role="option"][aria-selected="true"]').filter({ hasText: "Beispiel.txt" }).waitFor();
  });
  await test("Versteckter Treffer wird nach Navigation sichtbar ausgewählt", async page => {
    const dialog = await open(page); await start(page, dialog, "verborgen");
    await emit(page, 0, update("complete", [hit(".verborgen")]));
    await dialog.getByRole("button", { name: "Datei · .verborgen", exact: true }).click();
    await page.locator('[data-side="left"] [role="option"][aria-selected="true"]').filter({ hasText: ".verborgen" }).waitFor();
  }, "?keyboard=1");
  await test("Native Verstecktmarkierung ohne Punktnamen wird beim Öffnen angezeigt", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__HIDDEN_REVEAL_SCANS__ = [];
      window.__TAURI__.core.invoke = async (command,args) => {
        const result = await original(command,args);
        if (command === "scan_dir") {
          window.__HIDDEN_REVEAL_SCANS__.push(args.showHidden);
          if (!args.showHidden) result.entries = result.entries.filter(entry=>entry.name!=="Beispiel.txt");
        }
        return result;
      };
    });
    const dialog = await open(page); await start(page,dialog,"Beispiel");
    await emit(page,0,update("complete",[{...hit("Beispiel.txt"),is_hidden:true}]));
    await dialog.getByRole("button",{name:"Datei · Beispiel.txt",exact:true}).click();
    await page.locator('[data-side="left"] [role="option"][aria-selected="true"]').filter({hasText:"Beispiel.txt"}).waitFor();
    assert.equal(await page.evaluate(()=>window.__HIDDEN_REVEAL_SCANS__.at(-1)),true);
  });
  await test("Grenze bleibt sichtbare unvollständige Bilanz", async page => {
    const dialog = await open(page); await start(page, dialog);
    await emit(page, 0, update("limited", [hit("Beispiel.txt")], [], { limit: "Treffergrenze von 5000 Treffern erreicht" }));
    await dialog.getByRole("status").filter({ hasText: "Suchgrenze erreicht · unvollständig" }).waitFor();
    assert.match(await dialog.locator(".name-search-note").textContent(), /5000 Treffern/);
    assert.equal(await dialog.getByRole("button", { name: "Suchen", exact: true }).isEnabled(), true);
  });
  await test("Abbruch und Schließen verwerfen verspätete Treffer des alten Laufs", async page => {
    let dialog = await open(page); await start(page, dialog);
    await dialog.getByRole("button", { name: "Suche abbrechen", exact: true }).click();
    await page.waitForFunction(() => window.__SEARCH_CANCELS__.length === 1);
    assert.equal(await dialog.getByRole("button", { name: "Suchen", exact: true }).isDisabled(), true);
    await emit(page, 0, update("cancelled", [hit("Alt.txt")]));
    await dialog.getByRole("status").filter({ hasText: "Suche abgebrochen" }).waitFor();
    await dialog.getByRole("button", { name: "Schließen", exact: true }).click();
    dialog = await open(page); await start(page, dialog, "Neu", 2);
    await emit(page, 0, update("complete", [hit("Verspätet.txt")]));
    await emit(page, 1, update("complete", [hit("Neu.txt")]));
    assert.equal(await dialog.locator(".name-search-hit").count(), 1);
    assert.match(await dialog.locator(".name-search-hit").textContent(), /Neu.txt/);
  });
  await test("Späte Startantwort setzt Folgelauf und seinen Fokus nicht zurück", async page => {
    await page.evaluate(() => { window.__SEARCH_HOLD__ = true; });
    const dialog = await open(page); await start(page, dialog, "Alt");
    await emit(page, 0, update("complete", [hit("Alt.txt")]));
    await start(page, dialog, "Neu", 2);
    await page.evaluate(() => { window.__SEARCH_RELEASES__[0](); window.__SEARCH_RELEASES__[1](); });
    await emit(page, 1, update("running", [hit("Neu.txt")]));
    const button = dialog.getByRole("button", { name: "Datei · Neu.txt", exact: true });
    await button.focus();
    await emit(page, 1, update("running", [hit("Zusatz.txt")], [], { stats: { hits: 2, entries: 2, directories: 2, skipped: 0, issues: 0 } }));
    assert.equal(await button.evaluate(element => element === document.activeElement), true);
    await emit(page, 1, update("complete", [], [], { stats: { hits: 2, entries: 2, directories: 2, skipped: 0, issues: 0 } }));
    assert.equal(await dialog.locator(".name-search-hit").count(), 2);
    assert.doesNotMatch(await dialog.locator(".name-search-results").textContent(), /Alt.txt/);
  });
  for (const provider of ["sftp", "ftps", "webdav", "smb"]) {
    await test(`${provider}: Suche bindet aktive Pane, Profilrevision und Sitzung`, async page => {
      await page.locator(`#connection-list [data-protocol="${provider}"][data-id="mock_${provider}"]`).click();
      const path = page.locator('[data-side="left"] .path'); await path.fill("/from"); await path.press("Enter");
      await page.waitForFunction(() => document.querySelector('[data-side="left"] .path').value === "/from");
      const dialog = await open(page);
      await dialog.getByLabel("Groß-/Kleinschreibung beachten").check();
      await dialog.getByLabel("Versteckte Namen und Ordner einbeziehen").check();
      await start(page, dialog, "Remote");
      const [request] = await page.evaluate(() => window.__SEARCH_CALLS__);
      assert.equal(request.endpoint.provider, provider);
      assert.equal(request.endpoint.root, "/from");
      assert.equal(request.endpoint.profileId, `mock_${provider}`);
      assert.equal(request.sessionGeneration, 1);
      assert.equal(Number.isSafeInteger(request.endpoint.profileRevision), true);
      assert.equal(request.options.caseSensitive, true);
      assert.equal(request.options.includeHidden, true);
      await emit(page, 0, update("complete", [hit("remote.txt", "/from")]));
      await dialog.getByRole("button", { name: "Datei · remote.txt", exact: true }).click();
      await dialog.waitFor({ state: "hidden" });
      assert.equal(await path.inputValue(), "/from");
    });
  }
  await test("Schmaler dunkler Suchdialog bleibt ohne Überlauf bedienbar", async page => {
    await page.setViewportSize({ width: 800, height: 600 });
    await page.evaluate(async () => { (await import("/theme.js")).setTheme("dark"); });
    await page.locator('[data-side="right"] .listing').focus();
    await page.keyboard.press(process.platform === "darwin" ? "Meta+Shift+f" : "Control+Shift+f");
    const dialog = page.getByRole("dialog", { name: "Namen in Unterordnern suchen" }); await dialog.waitFor();
    await start(page, dialog);
    await emit(page, 0, update("complete", [hit(`${"LangerName".repeat(18)}.txt`, "/mock/ziel")]));
    const dimensions = await dialog.evaluate(element => ({ width: element.clientWidth, scroll: element.scrollWidth,
      bottom: element.getBoundingClientRect().bottom, height: window.innerHeight }));
    assert.ok(dimensions.scroll <= dimensions.width);
    assert.ok(dimensions.bottom <= dimensions.height);
    await dialog.getByRole("button", { name: "Schließen", exact: true }).waitFor();
    if (process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/namenssuche.png` });
  });
} finally { await cleanup.close(); }
process.exitCode = failures ? 1 : 0;
