#!/usr/bin/env node

/** Reproduzierbare Browser-Suite gegen die echte Oberfläche und die kontrollierte
 * Tauri-Mockbrücke. Der Test verändert weder Nutzerdaten noch das echte App-Profil. */

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { createServer } from "node:net";
import { once } from "node:events";
import { createBrowserCleanup } from "./browser-cleanup.mjs";

const require = createRequire(import.meta.url);
let playwright;
try {
  playwright = require("playwright");
} catch (error) {
  throw new Error("Playwright fehlt. Zuerst `pnpm install` und `pnpm exec playwright install chromium` ausführen.", { cause: error });
}

async function freePort() {
  const probe = createServer();
  probe.listen(0, "127.0.0.1");
  await once(probe, "listening");
  const address = probe.address();
  const port = typeof address === "object" && address ? address.port : 0;
  probe.close();
  await once(probe, "close");
  return port;
}

async function waitForServer(url, cleanup) {
  for (let attempt = 0; attempt < 80; attempt += 1) {
    cleanup.checkServer();
    try {
      const response = await fetch(url);
      if (response.ok) return;
    } catch (_) {
      // Der Server braucht üblicherweise nur wenige Schleifendurchläufe zum Start.
    }
    await new Promise(resolve => setTimeout(resolve, 25));
  }
  throw new Error("Mock-Server wurde nicht rechtzeitig erreichbar.");
}

const port = await freePort();
const baseUrl = `http://127.0.0.1:${port}`;
const server = spawn("python3", ["tools/mock-ui-server.py", "--port", String(port)], {
  cwd: new URL("..", import.meta.url),
  stdio: ["ignore", "pipe", "pipe"],
});
let serverStderr = "";
server.stderr.setEncoding("utf8");
server.stderr.on("data", chunk => { serverStderr += chunk; });
const cleanup = createBrowserCleanup(server);

let browser;
let failed = false;
const results = [];
const caseArguments = process.argv.slice(2);
// pnpm reicht den üblichen Argumenttrenner `--` an das Skript selbst weiter.
// Er gehört nicht zum Namensausschnitt; weitere Bindestriche bleiben dagegen
// unverändert Teil des ausdrücklich angeforderten Filters.
if (caseArguments[0] === "--") caseArguments.shift();
const requestedCase = caseArguments.join(" ").trim();
let matchedRequestedCase = false;

async function runCase(name, query, test) {
  // Ein optionaler Namensausschnitt hält neue Gegenbelege gezielt und schnell
  // ausführbar. Ohne Argument läuft unverändert die vollständige Suite.
  if (requestedCase && !name.includes(requestedCase)) return;
  if (requestedCase) matchedRequestedCase = true;
  const context = await browser.newContext({ colorScheme: "light" });
  const page = await context.newPage();
  const browserErrors = [];
  page.on("pageerror", error => browserErrors.push(error.message));
  page.on("console", message => {
    if (message.type() === "error") browserErrors.push(message.text());
  });
  try {
    await page.goto(`${baseUrl}/${query}`, { waitUntil: "networkidle" });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).waitFor();
    await test(page);
    const mockErrors = await page.evaluate(() => window.__DOPPELDECKER_MOCK_ERRORS__ || []);
    assert.deepEqual(mockErrors, [], "Die Mockbrücke meldete Laufzeitfehler.");
    assert.deepEqual(browserErrors, [], "Der Browser meldete Konsolen- oder Seitenfehler.");
    results.push(`ok  ${name}`);
  } catch (error) {
    failed = true;
    results.push(`FEHLER  ${name}\n${error.stack || error}`);
  } finally {
    await context.close();
  }
}

async function captureNativeSecret(dialog) {
  await dialog.getByRole("button", { name: "Sicher eingeben …" }).click();
  await dialog.getByText(/sicherer Einmalwert.*vorgemerkt/).waitFor();
}

async function openFtpsConflict(page, theme, key) {
  await page.setViewportSize({ width: 800, height: 600 });
  await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme); }, theme);
  const connection = page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]');
  await connection.click();
  const source = page.locator('[data-side="left"] .path');
  await source.fill("/from"); await source.press("Enter");
  await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click(); await connection.click();
  const target = page.locator('[data-side="right"] .path');
  await target.fill("/to"); await target.press("Enter");
  const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
  await left.getByRole("option", { name: /remote\.txt/ }).click(); await left.focus(); await page.keyboard.press(key);
  await page.getByRole("dialog").getByRole("button", { name: "Ersetzen prüfen", exact: true }).waitFor();
}

try {
  await waitForServer(baseUrl, cleanup);
  const cspResponse = await fetch(baseUrl);
  assert.equal(
    cspResponse.headers.get("content-security-policy"),
    "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src ipc: http://ipc.localhost; object-src 'none'; base-uri 'none'; form-action 'none'",
  );
  results.push("ok  Mock-Server verwendet die produktionsgleiche Content-Security-Policy");
  // Normalerweise verwendet Playwright sein reproduzierbar installiertes Chromium.
  // Entwickler können für einen schnellen lokalen Gegenlauf bewusst einen vorhandenen
  // Chromium-Pfad setzen, ohne ihn in Projektdateien festzuschreiben.
  const executablePath = process.env.DOPPELDECKER_PLAYWRIGHT_EXECUTABLE || undefined;
  browser = await cleanup.launch(playwright.chromium, { headless: true, executablePath });

  for (const theme of ["light", "dark"]) for (const key of ["F5", "F6"]) {
    await runCase(`AP22 FTPS-Merge-Skip kopiert neue Kinder ohne Quellentfernung · ${theme} · ${key}`, "?remoteFile=1&ftpsConflictChoice=1&ftpsPartialMerge=1", async page => {
      await openFtpsConflict(page, theme, key);
      await page.getByRole("dialog").getByRole("button", { name: "Überspringen", exact: true }).click();
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
      const operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation);
      assert.equal(operation.overwrite, false);
      assert.equal(operation.copy_new_best_effort_expected, true);
      assert.equal(operation.replace_best_effort_expected, false);
      assert.equal(operation.remove_source_best_effort_expected, null);
      if (key === "F6") await page.getByText(/Quellbäume mit übersprungenen Einträgen bleiben vollständig erhalten/).waitFor();
    });
  }

  for (const theme of ["light", "dark"]) for (const key of ["F5", "F6"]) for (const policy of ["skip", "overwrite", "both", "cancel"]) {
    await runCase(`AP21 FTPS-Konfliktwahl vor Probe · ${theme} · ${key} · ${policy}`, "?remoteFile=1&ftpsConflictChoice=1", async page => {
      await openFtpsConflict(page, theme, key);
      const dialog = page.getByRole("dialog");
      assert.deepEqual(await page.evaluate(() => window.__AP21_POLICIES__), [null]);
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length),0);
      if (policy === "cancel") { await page.keyboard.press("Escape");await dialog.waitFor({ state:"hidden" });assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length),0);return; }
      await dialog.getByRole("button", { name: policy === "skip" ? "Überspringen" : policy === "overwrite" ? "Ersetzen prüfen" : "Beide behalten prüfen", exact:true }).click();
      if (policy !== "skip") {
        await dialog.getByRole("button", { name: policy === "overwrite" ? "Dauerhaft ersetzen" : "Beide behalten", exact:true }).waitFor();
        assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length),0);
        await dialog.getByRole("button", { name: policy === "overwrite" ? "Dauerhaft ersetzen" : "Beide behalten", exact:true }).click();
      }
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length===1);
      const state=await page.evaluate(() => ({ policies:window.__AP21_POLICIES__, entry:window.__DOPPELDECKER_MOCK_QUEUE__.entries[0] }));
      assert.deepEqual(state.policies,[null,policy]);assert.equal(state.entry.operation.overwrite,policy==="overwrite");
      assert.equal(state.entry.operation.remove_source_best_effort_expected, key==="F6" && policy!=="skip" ? true:null);
      assert.equal(state.entry.operation.copy_new_best_effort_expected,policy==="overwrite");
      assert.equal(state.entry.operation.replace_best_effort_expected,policy==="overwrite");
    });
  }

  for (const theme of ["light", "dark"]) {
    await runCase(`AP21 gesperrtes Ersetzen lässt Skip ohne Quellentfernung erreichbar · ${theme}`, "?remoteFile=1&ftpsConflictChoice=1&ftpsReplaceDenied=1", async page => {
      await openFtpsConflict(page, theme, "F6"); const dialog = page.getByRole("dialog");
      await dialog.getByRole("button", { name: "Ersetzen prüfen", exact: true }).click();
      await dialog.getByText("Ersetzen ist für diese Ordnerauswahl nicht gebunden", { exact: true }).waitFor();
      assert.equal(await dialog.getByRole("button", { name: "Dauerhaft ersetzen", exact: true }).count(), 0);
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
      await dialog.getByRole("button", { name: "Überspringen", exact: true }).click();
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
      const state = await page.evaluate(() => ({ policies: window.__AP21_POLICIES__, operation: window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation }));
      assert.deepEqual(state.policies, [null, "overwrite", "skip"]); assert.equal(state.operation.overwrite, false);
      assert.equal(state.operation.remove_source_best_effort_expected, null);
    });
    await runCase(`AP21 Wechsel von Ersetzen zu Skip bindet keine Quellentfernung · ${theme}`, "?remoteFile=1&ftpsConflictChoice=1", async page => {
      await openFtpsConflict(page, theme, "F6"); const dialog = page.getByRole("dialog");
      await dialog.getByRole("button", { name: "Ersetzen prüfen", exact: true }).click();
      await dialog.getByRole("button", { name: "Dauerhaft ersetzen", exact: true }).waitFor();
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
      await dialog.getByRole("button", { name: "Überspringen", exact: true }).click();
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
      const state = await page.evaluate(() => ({ policies: window.__AP21_POLICIES__, operation: window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation }));
      assert.deepEqual(state.policies, [null, "overwrite", "skip"]);
      assert.equal(state.operation.overwrite, false); assert.equal(state.operation.remove_source_best_effort_expected, null);
      assert.equal(state.operation.copy_new_best_effort_expected, false); assert.equal(state.operation.replace_best_effort_expected, false);
    });
    await runCase(`AP21 Beide behalten verlangt die konkreten Namen vor Enqueue · ${theme}`, "?remoteFile=1&ftpsConflictChoice=1", async page => {
      await openFtpsConflict(page, theme, "F5"); const dialog = page.getByRole("dialog");
      await dialog.getByRole("button", { name: "Beide behalten prüfen", exact: true }).click();
      await dialog.getByRole("button", { name: "Beide behalten", exact: true }).waitFor();
      await dialog.getByText(/remote \(Kopie\)\.txt/).waitFor();
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
      await page.keyboard.press("Escape"); await dialog.waitFor({ state: "hidden" });
      assert.deepEqual(await page.evaluate(() => window.__AP21_POLICIES__), [null, "both"]);
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
    });
    await runCase(`AP21 verspätete Wirkungsvorbereitung bleibt nach Navigation wirkungslos · ${theme}`, "?remoteFile=1&ftpsConflictChoice=1", async page => {
      await openFtpsConflict(page, theme, "F6");
      await page.evaluate(() => {
        const invoke = window.__TAURI__.core.invoke;
        window.__TAURI__.core.invoke = async (command, args) => {
          if (command === "check_provider_file_operation" && args.choice) {
            window.__AP21_POLICY_HELD__ = true;
            await new Promise(resolve => { window.__AP21_RELEASE_POLICY__ = resolve; });
            const result = await invoke(command, args); window.__AP21_POLICY_RETURNED__ = true; return result;
          }
          return invoke(command, args);
        };
      });
      await page.getByRole("dialog").getByRole("button", { name: "Überspringen", exact: true }).click();
      await page.waitForFunction(() => window.__AP21_POLICY_HELD__);
      const path = page.locator('[data-side="left"] .path'); await path.fill("/other"); await path.press("Enter");
      await page.evaluate(() => window.__AP21_RELEASE_POLICY__()); await page.waitForFunction(() => window.__AP21_POLICY_RETURNED__);
      await page.evaluate(() => new Promise(resolve => setTimeout(resolve, 0)));
      assert.equal(await path.inputValue(), "/other"); assert.equal(await page.getByRole("dialog").count(), 0);
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
    });
  }

  for (const [theme, key] of [["light", "F5"], ["light", "F6"], ["dark", "F5"], ["dark", "F6"]]) {
  await runCase(`AP19 Enqueue-Abbruch zeigt Abgebrochen · ${theme} · ${key}`, "?remoteFile=1&bestEffortSource=1&ftpsEnqueuePreparationGate=1", async page => {
    await page.setViewportSize({ width: 800, height: 600 });
    await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme); }, theme);
    const connection = page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]');
    await connection.click();
    const source = page.locator('[data-side="left"] .path');
    await source.fill("/from"); await source.press("Enter");
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await connection.click();
    const target = page.locator('[data-side="right"] .path');
    await target.fill("/to"); await target.press("Enter");
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__AP19_CANCELS__ = 0;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "cancel_sync") window.__AP19_CANCELS__ += 1;
        return invoke(command, args);
      };
    });
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.getByRole("option", { name: /remote\.txt/ }).click();
    await left.focus(); await page.keyboard.press(key);
    await page.waitForFunction(() => window.__AP19_PREPARATION_HELD__ === true);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
    await page.keyboard.press("Escape");
    await page.evaluate(() => window.__AP19_RELEASE_PREPARATION__());
    await page.waitForFunction(() => /Abgebrochen|Fehler/.test(document.querySelector("#dock-summary")?.innerText || ""));
    const state = await page.evaluate(() => ({ entries: window.__DOPPELDECKER_MOCK_QUEUE__.entries,
      cancels: window.__AP19_CANCELS__ }));
    assert.equal(state.entries.length, 0, `Escape reihte trotzdem ein: ${JSON.stringify(state)}`);
    assert.equal(state.cancels, 1);
    assert.match(await page.locator("#dock-summary").innerText(), /Abgebrochen/);
  });


  }
  for (const [theme, key] of [["light", "F5"], ["light", "F6"], ["dark", "F5"], ["dark", "F6"]]) {
  await runCase(`AP19 Escape beendet die FTPS-Vorbereitung vor Enqueue · ${theme} · ${key}`, "?remoteFile=1&bestEffortSource=1&ftpsPreparationGate=1", async page => {
    await page.setViewportSize({ width: 800, height: 600 });
    await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme); }, theme);
    const connection = page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]');
    await connection.click();
    const source = page.locator('[data-side="left"] .path');
    await source.fill("/from"); await source.press("Enter");
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await connection.click();
    const target = page.locator('[data-side="right"] .path');
    await target.fill("/to"); await target.press("Enter");
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__AP19_CANCELS__ = 0;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "cancel_sync") window.__AP19_CANCELS__ += 1;
        return invoke(command, args);
      };
    });
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.getByRole("option", { name: /remote\.txt/ }).click();
    await left.focus(); await page.keyboard.press(key);
    await page.waitForFunction(() => window.__AP19_PREPARATION_HELD__ === true);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
    assert.match(await page.locator("#dock-summary").innerText(), /Abbrechbare Vorbereitung.*FTP-/s);
    if (process.env.AP19_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.AP19_EVIDENCE_DIR}/preparation-${theme}-${key}.png` });
    if (theme === "light" && key === "F5") {
      await page.keyboard.press("F2");
      assert.equal(await page.getByRole("dialog", { name: "Umbenennen" }).count(), 0);
      assert.match(await page.locator("#dock-summary").innerText(), /Umbenennen derzeit belegt/);
    }
    await page.keyboard.press("Escape");
    await page.evaluate(() => window.__AP19_RELEASE_PREPARATION__());
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length > 0
      || (document.querySelector("#dock-summary")?.innerText || "").includes("Vorbereitung wurde beendet"));
    const state = await page.evaluate(() => ({ entries: window.__DOPPELDECKER_MOCK_QUEUE__.entries,
      cancels: window.__AP19_CANCELS__ }));
    assert.equal(state.entries.length, 0, `Escape reihte trotzdem ein: ${JSON.stringify(state)}`);
    assert.equal(state.cancels, 1);
    assert.match(await page.locator("#dock-summary").innerText(), /Abgebrochen/);
  });

  }

  await runCase("AP02 Konfliktvergleich bleibt in beiden Themes klein und per Tastatur bedienbar", "?conflicts=replace", async page => {
    for (const theme of ["light", "dark"]) {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js");
        setTheme(theme);
        const { conflictChoiceModal } = await import("/conflict-dialog.js");
        window.__AP02_DIALOG_RESULT__ = "pending";
        void conflictChoiceModal({ title: "Konflikte beim Kopieren", conflicts: [
          { relative_name: "Sehr langer 🍰-Pfad/".repeat(12) + "Bericht.txt", source: { endpoint: "/Quelle/" + "lang/".repeat(50), exists: true, kind: "file", size: null, mtime: null }, target: { endpoint: "/Ziel/" + "lang/".repeat(5), exists: true, kind: "directory", size: null, mtime: 1792108800 } },
          { relative_name: "Zweiter Konflikt", source: { endpoint: "/Quelle", exists: true, kind: "file", size: 0, mtime: 0 }, target: { endpoint: "/Ziel", exists: false, size: null, mtime: null } },
        ], choices: [{ label: "Beide behalten", value: "both", explanation: "Neuer Name: Bericht (Konflikt).txt" }], note: "Zwei gebundene Konflikte" }).then(value => { window.__AP02_DIALOG_RESULT__ = value; });
      }, theme);
      const dialog = page.getByRole("dialog", { name: "Konflikte beim Kopieren" });
      await dialog.waitFor();
      assert.match(await dialog.innerText(), /Unbekannt/);
      assert.equal(await dialog.getByRole("button", { name: "Abbrechen", exact: true }).evaluate(button => button === document.activeElement), true);
      const geometry = await dialog.evaluate(element => ({ height: element.getBoundingClientRect().height, scroll: element.scrollWidth, width: element.clientWidth }));
      assert.ok(geometry.height <= 456 && geometry.scroll <= geometry.width + 1, JSON.stringify(geometry));
      assert.equal(await page.locator("html").getAttribute("data-resolved-theme"), theme);
      assert.equal(await page.locator("#modal-body").evaluate(element => element.scrollTop), 0);
      assert.equal(await dialog.locator(".conflict-endpoint dl").count(), 4);
      const comparisonRows = await dialog.locator("details").first().locator(".conflict-endpoint dl").evaluateAll(elements => elements.map(element => element.getBoundingClientRect().top));
      assert.ok(Math.abs(comparisonRows[0] - comparisonRows[1]) < 1);
      if (process.env.AP02_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.AP02_EVIDENCE_DIR}/browser-conflict-${theme}.png` });
      await page.keyboard.press("Shift+Tab");
      assert.ok(await dialog.evaluate(element => element.contains(document.activeElement)));
      await page.keyboard.press("Tab");
      await page.keyboard.press("Tab");
      await page.keyboard.press("Tab");
      assert.equal(await dialog.locator("summary").nth(1).evaluate(element => element === document.activeElement), true);
      await page.keyboard.press("Enter");
      assert.equal(await dialog.locator("details").nth(1).evaluate(element => element.open), true);
      await page.keyboard.press("Escape");
      await dialog.waitFor({ state: "hidden" });
      assert.equal(await page.evaluate(() => window.__AP02_DIALOG_RESULT__), null);
    }
  });

  await runCase("AP02 Copy und Move senden nur die Backendidentität für Beide behalten", "?keyboard=1&conflicts=replace", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__AP02_TRANSFER_CALLS__ = [];
      window.__TAURI__.core.invoke = async (command, args) => {
        if (["copy_paths", "move_paths"].includes(command)) window.__AP02_TRANSFER_CALLS__.push({ command, args: structuredClone(args.request) });
        return original(command, args);
      };
    });
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    for (const key of ["F5", "F6"]) {
      await list.getByRole("option", { name: /Alpha.txt/ }).click();
      await page.keyboard.press(key);
      const dialog = page.getByRole("dialog", { name: key === "F5" ? "Konflikte beim Kopieren" : "Konflikte beim Verschieben" });
      await dialog.waitFor();
      assert.match(await dialog.innerText(), /Neue Namen:/);
      await dialog.getByRole("button", { name: /^Beide behalten/ }).click();
      await page.waitForFunction(count => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === count, key === "F5" ? 1 : 2);
    }
    const calls = await page.evaluate(() => window.__AP02_TRANSFER_CALLS__);
    const transfers = calls.filter(call => ["copy_paths", "move_paths"].includes(call.command));
    assert.equal(transfers.length, 2);
    for (const call of transfers) {
      assert.equal(call.args.keepBothPreparationId, "mock-keep-both");
      assert.equal(call.args.overwrite, false);
      assert.equal(call.args.keepBothTargets, undefined);
    }
  });

  await runCase("AP02 Erstbaseline zeigt fehlende Seite und sperrt nicht speicherbares Skip", "?bidirectionalConflict=1", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await original(command, args);
        if (command === "prepare_bidirectional_sync" && !args.request.resolutions.length) {
          const detail = result.conflict_details[0];
          detail.right = { endpoint: "/Ziel", exists: false, kind: null, size: null, mtime: null };
          detail.choices = detail.choices.filter(choice => choice.decision.decision === "keep_left");
          detail.skip_blocker = "Die Erstbaseline kann den einseitigen Unterschied nicht speichern.";
        }
        return result;
      };
    });
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator('#sync-direction-group input[value="both"]').check();
    await page.locator("#btn-sync").click();
    const dialog = page.getByRole("dialog", { name: "Konflikt 1 von 1" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /Vorhanden.*Nein/s);
    assert.equal(await dialog.getByRole("button", { name: /^Überspringen/ }).isDisabled(), true);
    assert.equal(await dialog.getByRole("button", { name: /^Beide/ }).count(), 0);
    await page.keyboard.press("Escape");
  });

  await runCase("AP02 Abbruch im späteren Konfliktdialog verwirft alle Entscheidungen", "?bidirectionalConflict=1", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__AP02_DECISIONS__ = [];
      let details;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "preview_bidirectional_conflicts") {
          window.__AP02_DECISIONS__.push(structuredClone(args));
          return [details[1]];
        }
        const result = await original(command, args);
        if (command === "prepare_bidirectional_sync" && !args.request.resolutions.length) {
          const first = result.conflict_details[0];
          details = [first, { ...structuredClone(first), relative_name: "zweiter.txt", path: [[...new TextEncoder().encode("zweiter.txt")]] }];
          result.conflict_details = details;
        }
        return result;
      };
    });
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Konflikt 1 von 2" }).getByRole("button", { name: "Links behalten", exact: true }).click();
    const later = page.getByRole("dialog", { name: "Konflikt 2 von 2" });
    await later.waitFor();
    await page.keyboard.press("Escape");
    await later.waitFor({ state: "hidden" });
    const state = await page.evaluate(() => ({ decisions: window.__AP02_DECISIONS__, pending: window.__MOCK_PREPARATION_ACTIONS__().pendingBidirectional, queue: window.__DOPPELDECKER_MOCK_QUEUE__.entries }));
    assert.equal(state.decisions.length, 1);
    assert.equal(state.decisions[0].resolutions.length, 1);
    assert.deepEqual(state.decisions[0].expectedPlanDigest, Array(32).fill(3));
    assert.equal(state.pending, false);
    assert.equal(state.queue.length, 0);
  });

  for (const theme of ["light", "dark"]) {
    await runCase(`AP03 lokale AddOnly-Bestätigung ohne Phrase in ${theme}`, "", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(theme => window.__MOCK_EMIT__("menu", `theme_${theme}`), theme);
      await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
      await page.locator("#btn-sync").click();
      const dialog = page.getByRole("dialog", { name: "Lokalen Sync ausführen" });
      await dialog.waitFor();
      assert.equal(await dialog.locator("input").count(), 0);
      assert.match(await dialog.innerText(), /\/mock\/quelle.*\/mock\/ziel/s);
      assert.equal(await dialog.getByRole("button", { name: "Abbrechen", exact: true }).evaluate(button => button === document.activeElement), true);
      const geometry = await dialog.locator("#modal-body").evaluate(element => ({ width: element.clientWidth, content: element.scrollWidth }));
      assert.ok(geometry.content <= geometry.width + 1);
      if (process.env.DOPPELDECKER_SCREENSHOT_DIR) {
        await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/ap03-addonly-${theme}.png` });
      }
      await dialog.getByRole("button", { name: "Dateien hinzufügen" }).click();
      await page.locator("#sync-view").waitFor({ state: "hidden" });
      const state = await page.evaluate(() => ({ ...window.__MOCK_LOCAL_SYNC_STATE__(), queue: window.__DOPPELDECKER_MOCK_QUEUE__ }));
      assert.equal(state.pending, false);
      assert.equal(state.queue.entries[0].operation.type, "LocalSync");
      assert.deepEqual(state.calls.find(call => call.command === "enqueue_local_sync").args,
        { preparationDigest: Array(32).fill(5), replacementsConfirmed: true });
    });
  }

  for (const theme of ["light", "dark"]) {
    for (const effect of ["weak", "flush", "replace"]) {
      await runCase(`AP03B ${effect} zeigt konkrete Risiken und echte Zustimmung in ${theme}`, `?localCommit=${effect}`, async page => {
        await page.setViewportSize({ width: 760, height: 480 });
        await page.evaluate(theme => window.__MOCK_EMIT__("menu", `theme_${theme}`), theme);
        await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
        if (effect === "replace") await page.getByLabel("Vorhandenes aktualisieren").check();
        await page.locator("#btn-sync").click();
        const dialog = page.getByRole("dialog", { name: "Lokalen Sync als Best Effort ausführen" });
        await dialog.waitFor();
        const body = await dialog.innerText();
        assert.match(await page.locator("#sync-metadata-note-title").innerText(), /Rechte.*nicht exakt darstellbar.*ausgelassen/);
        assert.equal(await dialog.locator("input").count(), 0);
        assert.match(body, effect === "flush" ? /keinen dauerhaften Datei-Flush.*keinen dauerhaften Verzeichnis-Flush/s : /gleichzeitig neu angelegtes Ziel/);
        if (effect === "flush") assert.doesNotMatch(body, /kein atomar kollisionsfreies Rename/);
        if (effect === "replace") {
          assert.match(body, /1 vorhandene Dateiversion.*dauerhaft ersetzt/s);
          assert.match(body, /alte-Version.txt/);
          assert.doesNotMatch(body, /alten Dateiversionen werden vor dem Ersetzen/);
        }
        const geometry = await dialog.evaluate(element => ({ width: element.clientWidth, scroll: element.scrollWidth, height: element.getBoundingClientRect().height }));
        assert.ok(geometry.scroll <= geometry.width + 1 && geometry.height <= 456, JSON.stringify(geometry));
        if (process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/ap03b-${effect}-${theme}.png` });
        await dialog.getByRole("button", { name: effect === "replace" ? "Dauerhaftes Ersetzen bestätigen" : "Best Effort bestätigen und hinzufügen" }).click();
        await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
        await page.evaluate(() => {
          const queue = window.__DOPPELDECKER_MOCK_QUEUE__; queue.entries[0].state = "Running"; queue.entries[0].attempts = 1;
          window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
        });
        assert.match(await page.locator("#dock-summary").innerText(), /Best-Effort-Sync-Ausführung/);
        await page.evaluate(() => {
          const entry = window.__DOPPELDECKER_MOCK_QUEUE__.entries[0];
          window.__MOCK_EMIT__("sync-done", { operationId: entry.operation_id, attempt: 1,
            dry_run: false, files_copied: 1, files_deleted: 0, metadata_updated: 0, bytes_copied: 12,
            metadata_report: entry.operation.expected_plan.metadata_report,
            local_execution: entry.operation.execution_hint });
        });
        await page.locator("#dock-summary").filter({ hasText: "1 Rechte ausgelassen" }).waitFor();
        assert.doesNotMatch(await page.locator("#dock-summary").innerText(), /native Merkmale/);
      });
    }
  }

  await runCase("AP03 lange gebundene Wurzeln scrollen bei erreichbaren Startknöpfen", "", async page => {
    await page.setViewportSize({ width: 760, height: 480 });
    const root = "/mock/" + "langer-Ordner🍰/".repeat(50);
    for (const [side, name] of [["left", "quelle"], ["right", "ziel"]]) {
      const path = page.locator(`.pane[data-side="${side}"] .path`);
      await path.fill(root + name); await path.press("Enter");
      await page.waitForFunction(expected => window.__MOCK_SCAN_CALLS__().some(call => call.path === expected), root + name);
    }
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#btn-sync").click();
    const dialog = page.getByRole("dialog", { name: "Lokalen Sync ausführen" });
    await dialog.waitFor();
    assert.deepEqual(await dialog.locator(".real-grid code").allTextContents(), [root + "quelle", root + "ziel"]);
    const before = await dialog.locator("#modal-body").evaluate(body => body.scrollTop);
    await page.keyboard.press("Shift+Tab");
    await page.keyboard.press("PageDown");
    await page.waitForTimeout(150);
    const geometry = await dialog.locator("#modal-body").evaluate(body => ({
      width: body.clientWidth, content: body.scrollWidth, after: body.scrollTop,
    }));
    assert.ok(geometry.content <= geometry.width + 1 && geometry.after > before,
      "Der lange Dialoginhalt muss über Shift+Tab und PageDown erreichbar sein.");
    const button = await dialog.getByRole("button", { name: "Dateien hinzufügen" }).boundingBox();
    assert.ok(button && button.y + button.height <= 480);
    if (process.env.DOPPELDECKER_SCREENSHOT_DIR) {
      await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/ap03-long-roots.png` });
    }
    await page.keyboard.press("Escape");
  });

  await runCase("AP03 Update erklärt Ersetzung und erzwingt ContentHash", "", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const preview = await original(command, args);
        if (command === "prepare_local_sync") {
          preview.impact.replacements = 1;
          preview.restore_probe_ok = true;
          preview.impact.replacement_old_bytes = 12;
          preview.plan.actions.push({ type: "UpdateFile", path: "alte-Version.txt", size: 12 });
        }
        return preview;
      };
      window.__MOCK_EMIT__("menu", "sync_mode");
    });
    await page.getByLabel("Vorhandenes aktualisieren").check();
    assert.equal(await page.getByLabel("Inhalt (BLAKE3)").isChecked(), true);
    assert.equal(await page.getByLabel("Schnell (Größe und Datum)").isDisabled(), true);
    assert.equal(await page.getByLabel("Inhalt (BLAKE3)").isChecked(), true);
    await page.locator("#btn-sync").click();
    const dialog = page.getByRole("dialog", { name: "Lokalen Sync ausführen" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /alte-Version.txt/);
    assert.match(await dialog.innerText(), /vor dem Ersetzen.*Quarantäne/s);
    assert.equal(await dialog.locator("input").count(), 0);
    await dialog.getByRole("button", { name: "Ersetzungen bestätigen und starten" }).click();
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation.options.compare), "ContentHash");
  });

  await runCase("AP03 reine Metadatenänderung behauptet keine Dateiersetzung", "", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const preview = await original(command, args);
        if (command === "prepare_local_sync") {
          preview.impact.metadata_changes = 1;
          preview.plan.actions = [{ type: "SetMode", path: "Ordner", source_mode: 493, target_mode: 448, is_dir: true }];
        }
        return preview;
      };
      window.__MOCK_EMIT__("menu", "sync_mode");
    });
    await page.getByLabel("Vorhandenes aktualisieren").check();
    await page.locator("#btn-sync").click();
    const dialog = page.getByRole("dialog", { name: "Lokalen Sync ausführen" });
    await dialog.getByRole("button", { name: "Metadatenänderungen bestätigen" }).waitFor();
    assert.doesNotMatch(await dialog.innerText(), /alten Dateiversionen/);
    assert.match(await dialog.innerText(), /0 Ersetzungen · 1 Metadatenänderungen/);
    await page.keyboard.press("Escape");
  });

  await runCase("AP03 Escape verwirft Vorbereitung und verspätete Antwort öffnet keinen alten Dialog", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#btn-sync").click();
    const dialog = page.getByRole("dialog", { name: "Lokalen Sync ausführen" });
    await dialog.waitFor();
    await page.keyboard.press("Escape");
    await page.waitForFunction(() => !window.__MOCK_LOCAL_SYNC_STATE__().pending);
    assert.equal(await page.locator("#sync-view").isVisible(), true);
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await original(command, args);
        if (command === "prepare_local_sync") await new Promise(resolve => { window.__AP03_RELEASE__ = resolve; });
        return result;
      };
    });
    await page.locator("#btn-sync").click();
    await page.waitForFunction(() => !!window.__AP03_RELEASE__);
    await page.locator("#btn-sync-close").click();
    await page.evaluate(() => window.__AP03_RELEASE__());
    await page.waitForFunction(() => !window.__MOCK_LOCAL_SYNC_STATE__().pending);
    assert.equal(await dialog.count(), 0);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });

  await runCase("AP03 simuliert auch in der markierten Sandbox ausdrücklich ohne Realauftrag", "?realEnabled=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#btn-sync-simulate").click();
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation.type), "Sync");
    assert.equal(await page.evaluate(() => window.__MOCK_LOCAL_SYNC_STATE__().calls.length), 0);
    assert.match(await page.locator("#dock-summary").innerText(), /Simulierter Testlauf.*keine Dateien verändert/s);
  });

  for (const remote of [false, true]) for (const failure of [false, true]) {
    await runCase(`Datei-Queue späte Einreihantwort erhält ${failure ? "Fehler" : "Abschluss"}: ${remote ? "WebDAV" : "lokal"}`, remote ? "?webdavUpload=1" : "", async page => {
      if (remote) {
        await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
        await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
      }
      await page.evaluate(({ remote, failure }) => {
        const original = window.__TAURI__.core.invoke;
        window.__TAURI__.core.invoke = async (command, args) => {
          const result = await original(command, args);
          if (command === (remote ? "enqueue_provider_file_operation" : "copy_paths")) {
            const entry = window.__DOPPELDECKER_MOCK_QUEUE__.entries.find(entry => entry.operation_id === result);
            entry.state = failure ? "Failed" : "Completed";
            window.__MOCK_EMIT__(failure ? "fileop-error" : "fileop-done", failure
              ? { operationId: result, code: "outcome_unclear", message: "Künstlicher unklarer Ausgang" }
              : { operationId: result, files: 1, bytes: 8 });
            window.__MOCK_EMIT__("queue-updated", structuredClone(window.__DOPPELDECKER_MOCK_QUEUE__));
            await new Promise(resolve => { window.__FILE_REPLY_RELEASE__ = resolve; });
          }
          return result;
        };
      }, { remote, failure });
      const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
      await list.getByRole("option", { name: /Beispiel\.txt/ }).click();
      await list.focus(); await page.keyboard.press("F5");
      await page.waitForFunction(() => typeof window.__FILE_REPLY_RELEASE__ === "function");
      const terminal = await page.locator("#dock-summary").innerText();
      assert.match(terminal, failure ? /Ausgang unklar/ : /Fertig/);
      await page.evaluate(async () => { window.__FILE_REPLY_RELEASE__(); await new Promise(resolve => setTimeout(resolve, 0)); });
      assert.equal(await page.locator("#dock-summary").innerText(), terminal, "Eine bestätigte Einreihantwort darf den neueren Laufzustand nicht ersetzen");
      assert.equal(await page.locator("#queue-count").innerText(), failure ? "1" : "0");
    });
  }

  for (const confirm of [false, true]) {
    await runCase(`Datei-Queue Rückkehr aus lokaler Bestätigung: ${confirm ? "bestätigt" : "abgebrochen"}`, "", async page => {
      await page.evaluate(() => { window.__MOCK_EMIT__("menu", "sync_mode"); });
      await page.locator("#btn-sync").click();
      await page.getByRole("dialog", { name: "Lokalen Sync ausführen" }).getByRole("button", { name: "Dateien hinzufügen" }).click();
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
      await page.evaluate(() => {
        const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
        queue.entries[0].state = "AwaitingConfirmation";
        queue.entries[0].attempts = 1;
        queue.entries[0].attention = { type: "RemoteRun", reason: "Frisch geprüft", confirmation: "BACKEND ONLY" };
        window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
      });
      await page.locator("#btn-queue").click();
      const popover = page.locator("#queue-popover");
      await popover.getByRole("button", { name: "Plan bestätigen" }).click();
      const dialog = page.getByRole("dialog", { name: "Lokalen Sync ausführen" });
      await dialog.getByRole("button", { name: confirm ? "Dateien hinzufügen" : "Abbrechen", exact: true }).click();
      await dialog.waitFor({ state: "hidden" });
      if (confirm) await page.waitForFunction(() => window.__MOCK_LOCAL_SYNC_STATE__().calls.some(call => call.command === "approve_local_sync_queue_entry"));
      assert.equal(await popover.isVisible(), true, "Der Rückweg muss die Queue wieder sichtbar machen");
      assert.equal(await page.evaluate(() => document.activeElement?.closest("[data-queue-id]")?.dataset.queueId), "mock-1", "Der Fokus gehört wieder zum gerade bearbeiteten Queueeintrag");
      const state = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state);
      assert.equal(state, confirm ? "Pending" : "AwaitingConfirmation");
    });
  }

  await runCase("AP03 lokaler Queue-Lauf zeigt Sync und bestätigt Reopen ohne Laufphrase", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Lokalen Sync ausführen" }).getByRole("button", { name: "Dateien hinzufügen" }).click();
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    await page.evaluate(() => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      queue.entries[0].state = "Running"; queue.entries[0].attempts = 1;
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    });
    assert.match(await page.locator("#prog-text").innerText(), /Sync-Ausführung startet/);
    assert.match(await page.locator("#dock-summary").innerText(), /Abgesicherte Sync-Ausführung/);
    await page.evaluate(() => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      queue.entries[0].state = "AwaitingConfirmation";
      queue.entries[0].attention = { type: "RemoteRun", reason: "Frisch geprüft", confirmation: "BACKEND ONLY" };
      window.__MOCK_EMIT__("sync-error", { code: "queue_confirmation_required", message: "Wirkung bestätigen" });
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    });
    await page.locator("#btn-queue").click();
    await page.getByRole("button", { name: "Plan bestätigen" }).click();
    const dialog = page.getByRole("dialog", { name: "Lokalen Sync ausführen" });
    await dialog.waitFor();
    assert.equal(await dialog.locator("input").count(), 0);
    await page.keyboard.press("Escape");
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state), "AwaitingConfirmation");
    await page.getByRole("button", { name: "Plan bestätigen" }).click();
    await dialog.getByRole("button", { name: "Dateien hinzufügen" }).click();
    await page.waitForFunction(() => window.__MOCK_LOCAL_SYNC_STATE__().calls.some(call => call.command === "approve_local_sync_queue_entry"));
    const approval = await page.evaluate(() => window.__MOCK_LOCAL_SYNC_STATE__().calls.find(call => call.command === "approve_local_sync_queue_entry").args);
    assert.deepEqual(approval, { operationId: "mock-1", preparationDigest: Array(32).fill(5), replacementsConfirmed: true });
  });

  await runCase("AP03 verspätete lokale Enqueue-Antwort erhält den echten Abschluss", "", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await original(command, args);
        if (command === "enqueue_local_sync") {
          window.__MOCK_EMIT__("sync-done", { operationId: result, attempt: 0, dry_run: false,
            files_copied: 1, files_deleted: 0, metadata_updated: 0, bytes_copied: 12 });
          await new Promise(resolve => { window.__AP03_ENQUEUE_RELEASE__ = resolve; });
        }
        return result;
      };
      window.__MOCK_EMIT__("menu", "sync_mode");
    });
    await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Lokalen Sync ausführen" }).getByRole("button", { name: "Dateien hinzufügen" }).click();
    await page.waitForFunction(() => !!window.__AP03_ENQUEUE_RELEASE__);
    await page.locator("#dock-summary").filter({ hasText: "Sync ausgeführt" }).waitFor();
    const completed = await page.locator("#dock-summary").innerText();
    await page.evaluate(async () => { window.__AP03_ENQUEUE_RELEASE__(); await new Promise(resolve => setTimeout(resolve, 0)); });
    assert.equal(await page.locator("#dock-summary").innerText(), completed);
  });

  await runCase("AP02 verspätete Sync-Startantwort erhält den bereits sichtbaren Abschluss", "", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await original(command, args);
        if (command === "run_sync") {
          window.__MOCK_EMIT__("sync-done", { operationId: result, attempt: 0, dry_run: true,
            files_copied: 1, files_deleted: 0, metadata_updated: 0, bytes_copied: 4 });
          await new Promise(resolve => { window.__AP02_RELEASE_ENQUEUE__ = resolve; });
        }
        return result;
      };
    });
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#sync-summary .pill").first().waitFor();
    await page.locator("#btn-sync-simulate").click();
    await page.waitForFunction(() => !!window.__AP02_RELEASE_ENQUEUE__);
    await page.locator("#dock-summary").filter({ hasText: /Simuliert.*nichts verändert/ }).waitFor();
    const completed = await page.locator("#dock-summary").innerText();
    await page.evaluate(async () => { window.__AP02_RELEASE_ENQUEUE__(); await new Promise(resolve => setTimeout(resolve, 0)); });
    assert.equal(await page.locator("#dock-summary").innerText(), completed);
  });

  await runCase("Hintergrundqueue erlaubt Navigation, Tabs und zweites Enqueue", "?keyboard=1", async page => {
    const left = page.locator('.pane[data-side="left"]');
    const right = page.locator('.pane[data-side="right"]');
    const source = await left.locator(".path").inputValue();
    const target = await right.locator(".path").inputValue();
    await page.evaluate(({ source, target }) => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      queue.entries.push({
        operation_id: "background-copy", state: "Running", attempts: 1,
        operation: { type: "File", kind: "Copy", items: [`${source}/Alpha.txt`], destination: target },
        progress: { phase: "copy", files_total: 1, files_done: 0, bytes_total: 1000000, bytes_done: 1 },
      });
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    }, { source, target });
    assert.equal(await left.locator(".path").isEnabled(), true);
    await left.getByRole("button", { name: "Neuer Tab", exact: true }).click();
    assert.equal(await left.getByRole("tab").count(), 2);
    await left.locator(".path").fill("/mock/fast");
    await left.locator(".path").press("Enter");
    await left.getByRole("option", { name: /Schnell\.txt/ }).click();
    await page.keyboard.press("F5");
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 2);
    const entries = await page.evaluate(() => structuredClone(window.__DOPPELDECKER_MOCK_QUEUE__.entries));
    assert.equal(entries[0].state, "Running");
    assert.deepEqual(entries[0].operation.items, [`${source}/Alpha.txt`]);
    assert.equal(entries[0].operation.destination, target);
    assert.deepEqual(entries[1].operation.items, ["/mock/fast/Schnell.txt"]);
    assert.equal(entries[1].operation.destination, target);
    assert.equal(await left.locator(".path").inputValue(), "/mock/fast");
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "rename"));
    await page.getByText(/Umbenennen derzeit belegt/).waitFor();
    assert.equal(await page.getByRole("dialog").count(), 0);
    assert.equal(await left.locator(".path").isEnabled(), true);
  });

  for (const navigation of ["Listing", "Home"]) {
    await runCase(`Pane-Review Hintergrundabschluss erhält laufende ${navigation}-Navigation`, "?keyboard=1", async page => {
      const left = page.locator('.pane[data-side="left"]');
      if (navigation === "Home") {
        await left.locator(".path").fill("/mock/fast");
        await left.locator(".path").press("Enter");
        await left.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
      }
      const originalPath = await left.locator(".path").inputValue();
      await page.evaluate(({ originalPath, navigation }) => {
        window.__MOCK_EMIT__("operation-context", { operationId: "held-navigation-copy", attempt: 1,
          affectedEndpoints: [{ provider: "local", profileId: null, profileRevision: null, path: originalPath }] });
        const original = window.__TAURI__.core.invoke;
        window.__TAURI__.core.invoke = async (command, args) => {
          const result = await original(command, args);
          if ((navigation === "Listing" && command === "scan_dir" && args.path === "/mock/slow")
              || (navigation === "Home" && command === "default_dirs")) {
            await new Promise(resolve => { window.__PANE_NAVIGATION_RELEASE__ = resolve; });
            window.__PANE_NAVIGATION_RETURNED__ = true;
          }
          return result;
        };
      }, { originalPath, navigation });
      if (navigation === "Home") await page.evaluate(() => { window.__MOCK_EMIT__("menu", "home"); });
      else {
        await left.locator(".path").fill("/mock/slow");
        await left.locator(".path").press("Enter");
      }
      await page.waitForFunction(() => typeof window.__PANE_NAVIGATION_RELEASE__ === "function");
      const before = await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length);
      await page.evaluate(async () => {
        window.__MOCK_EMIT__("fileop-done", { operationId: "held-navigation-copy", attempt: 1, files: 1, bytes: 8 });
        await new Promise(resolve => setTimeout(resolve, 0));
      });
      const completionReads = await page.evaluate(before => window.__MOCK_SCAN_CALLS__().slice(before), before);
      await page.evaluate(() => { window.__PANE_NAVIGATION_RELEASE__(); });
      await page.waitForFunction(() => window.__PANE_NAVIGATION_RETURNED__);
      assert.deepEqual(completionReads, [], "Ein passiver Abschluss darf die bereits laufende Navigation nicht durch ein altes Listing verdrängen");
      await left.getByRole("option", { name: navigation === "Home" ? /Alpha\.txt/ : /Langsam\.txt/ }).waitFor();
      assert.equal(await left.locator(".path").inputValue(), navigation === "Home" ? "/mock/quelle" : "/mock/slow");
    });
  }

  await runCase("Pane-Review Mutationsabschluss erhält laufende Navigation", "?keyboard=1", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("option", { name: /Alpha\.txt/ }).click();
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await original(command, args);
        if (command === "rename_path") {
          await new Promise(resolve => { window.__PANE_MUTATION_RELEASE__ = resolve; });
        } else if (command === "scan_dir" && args.path === "/mock/slow") {
          await new Promise(resolve => { window.__PANE_NAVIGATION_RELEASE__ = resolve; });
        }
        return result;
      };
    });
    await page.keyboard.press("F2");
    await page.locator("#modal-input").fill("Renamed.txt");
    await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK", exact: true }).click();
    await page.waitForFunction(() => typeof window.__PANE_MUTATION_RELEASE__ === "function");
    await left.locator(".path").fill("/mock/slow");
    await left.locator(".path").press("Enter");
    await page.waitForFunction(() => typeof window.__PANE_NAVIGATION_RELEASE__ === "function");
    const before = await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length);
    await page.evaluate(async () => { window.__PANE_MUTATION_RELEASE__(); await new Promise(resolve => setTimeout(resolve, 0)); });
    await page.locator("#dock-summary").filter({ hasText: /Umbenannt/ }).waitFor();
    const completionReads = await page.evaluate(before => window.__MOCK_SCAN_CALLS__().slice(before), before);
    await page.evaluate(() => { window.__PANE_NAVIGATION_RELEASE__(); });
    assert.deepEqual(completionReads, [], "Auch ein Mutationsabschluss darf die laufende Navigation nicht durch den alten Ort ersetzen");
    await left.getByRole("option", { name: /Langsam\.txt/ }).waitFor();
    assert.equal(await left.locator(".path").inputValue(), "/mock/slow");
  });

  for (const navigation of ["Listing", "Home"]) {
    await runCase(`Pane-Review ${navigation}-Fehler gibt Hintergrundrefresh wieder frei`, "?keyboard=1", async page => {
      const left = page.locator('.pane[data-side="left"]');
      const path = await left.locator(".path").inputValue();
      await page.evaluate(navigation => {
        const original = window.__TAURI__.core.invoke;
        window.__TAURI__.core.invoke = async (command, args) => {
          if ((navigation === "Home" && command === "default_dirs")
              || (navigation === "Listing" && command === "scan_dir" && args.path === "/mock/slow")) {
            throw { code: "io", message: "Gezielter Navigationsfehler" };
          }
          return original(command, args);
        };
      }, navigation);
      if (navigation === "Home") await page.evaluate(() => { window.__MOCK_EMIT__("menu", "home"); });
      else {
        await left.locator(".path").fill("/mock/slow");
        await left.locator(".path").press("Enter");
      }
      await page.locator("#dock-summary").filter({ hasText: /Gezielter Navigationsfehler/ }).waitFor();
      const before = await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length);
      await page.evaluate(path => {
        window.__MOCK_EMIT__("operation-context", { operationId: "after-navigation-error", attempt: 1,
          affectedEndpoints: [{ provider: "local", profileId: null, profileRevision: null, path }] });
        window.__MOCK_EMIT__("fileop-done", { operationId: "after-navigation-error", attempt: 1, files: 1, bytes: 8 });
      }, path);
      await page.waitForFunction(before => window.__MOCK_SCAN_CALLS__().length > before, before);
      assert.deepEqual(await page.evaluate(before => window.__MOCK_SCAN_CALLS__().slice(before).map(call => call.path), before), [path]);
      await left.getByRole("option", { name: /Alpha\.txt/ }).waitFor();
      assert.equal(await left.locator(".path").inputValue(), path);
    });
  }

  await runCase("Hintergrundrefresh erhält eine große Auswahl vollständig", "?entries=10000", async page => {
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.focus();
    await page.keyboard.press("Control+A");
    const before = await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length);
    await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", { files: 1, bytes: 8 }));
    await page.waitForFunction(before => window.__MOCK_SCAN_CALLS__().length >= before + 2, before);
    await list.focus();
    await page.keyboard.press("F5");
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation.items.length), 10000);
    assert.ok(await list.getByRole("option").count() < 100);
  });

  await runCase("Hintergrundqueue lässt Informationen und native Navigation erreichbar", "?keyboard=1", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("option", { name: /Alpha\.txt/ }).click();
    await page.evaluate(() => window.__MOCK_EMIT__("queue-updated", {
      revision: 1, paused: false, stop_on_error: false,
      entries: [{ operation_id: "background-copy", state: "Running",
        operation: { type: "File", kind: "Copy", items: ["/mock/quelle/Alpha.txt"], destination: "/mock/ziel" } }],
    }));
    await page.keyboard.press("Control+I");
    await page.getByRole("dialog", { name: /Information/ }).waitFor();
    await page.keyboard.press("Escape");
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "goto"));
    assert.equal(await left.locator(".path").evaluate(el => el === document.activeElement), true);
  });

  await runCase("Hintergrundqueue refresh prüft nur die ursprünglichen Endpunkte", "?keyboard=1", async page => {
    const left = page.locator('.pane[data-side="left"]');
    const right = page.locator('.pane[data-side="right"]');
    const roots = [await left.locator(".path").inputValue(), await right.locator(".path").inputValue()];
    await page.evaluate(roots => {
      window.__MOCK_EMIT__("operation-context", { operationId: "old-copy",
        affectedEndpoints: roots.map(path => ({ provider: "local", profileId: null, profileRevision: null, path })) });
      window.__MOCK_EMIT__("queue-updated", { revision: 1, paused: false, stop_on_error: false,
        entries: [{ operation_id: "old-copy", state: "Running",
          operation: { type: "File", kind: "Copy", items: [`${roots[0]}/Alpha.txt`], destination: roots[1] } }] });
    }, roots);
    for (const pane of [left, right]) {
      await pane.locator(".path").fill("/mock/fast");
      await pane.locator(".path").press("Enter");
      await pane.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    }
    const before = await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length);
    await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", {
      operationId: "old-copy", files: 1, skipped: 0, symlinks_skipped: 0, bytes: 8, cancelled: false,
    }));
    assert.equal(await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length), before);
    assert.equal(await left.locator(".path").inputValue(), "/mock/fast");
    assert.equal(await right.locator(".path").inputValue(), "/mock/fast");
  });

  await runCase("Hintergrundqueue refresh erfasst aktuell geöffnete Unterordner", "?keyboard=1", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await page.evaluate(() => window.__MOCK_EMIT__("operation-context", { operationId: "folder-copy", affectedEndpoints: [{ provider: "local", profileId: null, profileRevision: null, path: "/mock/quelle" }] }));
    await left.locator(".path").fill("/mock/quelle/sub");
    await left.locator(".path").press("Enter");
    await page.waitForFunction(() => window.__MOCK_SCAN_CALLS__().some(call => call.path === "/mock/quelle/sub"));
    const before = await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length);
    await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", { operationId: "folder-copy", files: 1, bytes: 8 }));
    await page.waitForFunction(before => window.__MOCK_SCAN_CALLS__().length > before, before);
    const scans = await page.evaluate(before => window.__MOCK_SCAN_CALLS__().slice(before), before);
    assert.deepEqual(scans.map(scan => scan.path), ["/mock/quelle/sub"]);
    assert.equal(await left.locator(".path").inputValue(), "/mock/quelle/sub");
  });

  await runCase("Hintergrundqueue ignoriert verspäteten Abschluss und Fortschritt", "?keyboard=1", async page => {
    await page.evaluate(() => {
      for (const [index, id] of ["old-copy", "new-copy"].entries()) {
        window.__MOCK_EMIT__("operation-context", { operationId: id, affectedEndpoints: [] });
        window.__MOCK_EMIT__("queue-updated", { revision: index + 1, paused: false, stop_on_error: false,
          entries: [{ operation_id: id, state: "Running",
            operation: { type: "File", kind: "Copy", items: ["/mock/Alpha.txt"], destination: "/mock/ziel" } }] });
      }
      window.__MOCK_EMIT__("fileop-progress", { operationId: "new-copy", phase: "copy",
        files_total: 1, files_done: 0, bytes_total: 1000, bytes_done: 100, current: "Neu.txt" });
      window.__MOCK_EMIT__("fileop-done", { operationId: "old-copy", files: 1, bytes: 8 });
      window.__MOCK_EMIT__("fileop-progress", { operationId: "old-copy", phase: "done",
        files_total: 1, files_done: 1, bytes_total: 8, bytes_done: 8, current: "Alt.txt" });
    });
    assert.equal(await page.locator("#progress").isVisible(), true);
    assert.match(await page.locator("#prog-text").innerText(), /Neu\.txt/);
    assert.equal(await page.locator("#app").evaluate(el => el.classList.contains("operation-running")), true);
  });

  await runCase("Hintergrundqueue unterscheidet Wiederholversuche desselben Auftrags", "?keyboard=1", async page => {
    await page.evaluate(() => {
      for (const attempt of [1, 2]) {
        window.__MOCK_EMIT__("operation-context", { operationId: "retry", attempt, affectedEndpoints: [] });
        window.__MOCK_EMIT__("queue-updated", { revision: attempt * 2, entries: [{ operation_id: "retry", attempts: attempt, state: "Running", operation: { type: "File", kind: "Copy", items: ["/a/file"], destination: "/b" } }] });
        if (attempt === 1) {
          window.__MOCK_EMIT__("fileop-error", { operationId: "retry", attempt, code: "io", message: "Erster Versuch fehlgeschlagen" });
          window.__MOCK_EMIT__("queue-updated", { revision: 3, entries: [] });
        }
      }
      window.__MOCK_EMIT__("fileop-progress", { operationId: "retry", attempt: 2, phase: "copy", files_done: 0, files_total: 1, bytes_done: 3, bytes_total: 10, current: "Retry.txt" });
      window.__MOCK_EMIT__("fileop-done", { operationId: "retry", attempt: 1, files: 1, bytes: 10 });
    });
    assert.match(await page.locator("#prog-text").innerText(), /Retry\.txt/);
    assert.equal(await page.locator("#progress").isVisible(), true);
    await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", { operationId: "retry", attempt: 2, files: 1, bytes: 10 }));
    assert.equal(await page.locator("#progress").isVisible(), false);
  });

  await runCase("Hintergrundqueue refresh erhält neue Auswahl und vorbereiteten Auftrag", "?keyboard=1", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("option", { name: /Alpha\.txt/ }).click();
    await left.getByRole("option", { name: /Beta\.txt/ }).click({ modifiers: ["Meta"] });
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "check_local_file_operation") {
          return new Promise(resolve => { window.__RELEASE_COPY_CHECK__ = () => resolve(original(command, args)); });
        }
        return original(command, args);
      };
    });
    await page.keyboard.press("F5");
    await page.waitForFunction(() => typeof window.__RELEASE_COPY_CHECK__ === "function");
    const before = await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length);
    await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", { files: 1, bytes: 8 }));
    await page.waitForFunction(before => window.__MOCK_SCAN_CALLS__().length >= before + 2, before);
    await page.evaluate(() => window.__RELEASE_COPY_CHECK__());
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    const items = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation.items);
    assert.equal(items.length, 2);
    assert.deepEqual(items.map(path => path.split("/").pop()).sort(), ["Alpha.txt", "Beta.txt"]);
    assert.equal(await left.locator('[role="option"][aria-selected="true"]').count(), 2);
  });

  for (const state of ["Running", "Validating"]) {
    await runCase(`Hintergrundqueue reiht Remote-Transfer ohne parallele Probe ein: ${state}`, "?remoteFile=1", async page => {
      await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
      const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
      await list.getByRole("option", { name: /remote\.txt/ }).click();
      await page.evaluate(state => {
        const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
        queue.entries.push({ operation_id: "active", state, operation: { type: "File", kind: "Copy", items: ["/old/file"], destination: "/old/target" } });
        window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
        const original = window.__TAURI__.core.invoke;
        window.__INTENT_CALLS__ = [];
        window.__TAURI__.core.invoke = async (command, args) => {
          window.__INTENT_CALLS__.push(command);
          if (command === "check_provider_file_operation") throw { code: "busy", message: "Anderer Auftrag prüft" };
          return original(command, args);
        };
      }, state);
      await page.keyboard.press("F5");
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 2);
      assert.match(await page.locator("#dock-summary").innerText(), /Best Effort.*atomaren Kollisionsschutz/);
      const calls = await page.evaluate(() => window.__INTENT_CALLS__);
      assert.equal(calls.filter(command => command === "enqueue_provider_file_intent").length, 1);
      if (state === "Running") assert.ok(!calls.includes("check_provider_file_operation"));
      const entry = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[1]);
      assert.equal(entry.operation.source.provider, "Sftp");
      assert.equal(entry.operation.destination, "/mock/ziel");
    });
  }

  await runCase("Datei-Zwischenablage zeigt nach später alter Copyantwort die neuere Auswahl", "?keyboard=1", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      let copies = 0;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "write_file_clipboard" && ++copies === 1) {
          return new Promise(resolve => { window.__AP01_RELEASE_OLD_COPY__ = () => resolve(false); });
        }
        return original(command, args);
      };
    });
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("option", { name: /Alpha\.txt/ }).click();
    await page.keyboard.press("Control+C");
    await page.waitForFunction(() => !!window.__AP01_RELEASE_OLD_COPY__);
    await left.getByRole("option", { name: /Beta\.txt/ }).click({ modifiers: ["Meta"] });
    await page.keyboard.press("Control+C");
    await page.waitForFunction(() => window.__MOCK_CLIPBOARD__().paths.length === 2);
    await page.evaluate(() => window.__AP01_RELEASE_OLD_COPY__());
    await page.waitForTimeout(30);
    assert.match(await page.locator("#dock-summary").innerText(), /2 Datei/);
  });

  await runCase("Datei-Zwischenablage erhält Textkopieren vor verspäteter Copyantwort", "?keyboard=1", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => command === "write_file_clipboard"
        ? new Promise(resolve => { window.__AP01_RELEASE_FILE_COPY__ = () => resolve(true); })
        : original(command, args);
    });
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("option", { name: /Alpha\.txt/ }).click();
    await page.keyboard.press("Control+C");
    await page.waitForFunction(() => !!window.__AP01_RELEASE_FILE_COPY__);
    await left.locator(".path").focus();
    await page.keyboard.press("Control+A");
    await page.keyboard.press("Control+C");
    await page.evaluate(() => window.__AP01_RELEASE_FILE_COPY__());
    await page.waitForTimeout(30);
    assert.doesNotMatch(await page.locator("#dock-summary").innerText(), /Kopiert1 Datei/);
  });

  for (const failedCopy of [false, true]) {
    await runCase(`Sofortiges Einfügen wartet auf die eigene Dateiauswahl: ${failedCopy ? "Copyfehler" : "Erfolg"}`, "?keyboard=1", async page => {
      await page.evaluate(failed => {
        window.__MOCK_CLIPBOARD__({ paths: ["/previous/alte-auswahl.txt"], cut: false });
        const original = window.__TAURI__.core.invoke;
        window.__AP01_CLIPBOARD_READS__ = 0;
        window.__TAURI__.core.invoke = async (command, args) => {
          if (command === "write_file_clipboard") {
            await new Promise(resolve => { window.__AP01_RELEASE_PENDING_COPY__ = resolve; });
            if (failed) throw { code: "io", message: "Test-Copy wurde abgelehnt" };
          }
          if (command === "read_file_clipboard") window.__AP01_CLIPBOARD_READS__ += 1;
          return original(command, args);
        };
      }, failedCopy);
      await page.locator('.pane[data-side="left"]').getByRole("option", { name: /Alpha\.txt/ }).click();
      await page.keyboard.press("Control+C");
      await page.waitForFunction(() => !!window.__AP01_RELEASE_PENDING_COPY__);
      await page.locator('.pane[data-side="right"] .listing').focus();
      await page.keyboard.press("Control+V");
      await page.waitForTimeout(100);
      assert.equal(await page.evaluate(() => window.__AP01_CLIPBOARD_READS__), 0);
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
      await page.evaluate(() => window.__AP01_RELEASE_PENDING_COPY__());
      if (failedCopy) {
        await page.getByText(/Test-Copy wurde abgelehnt/).waitFor();
        assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
      } else {
        await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
        assert.deepEqual(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation.items), ["/mock/quelle/Alpha.txt"]);
      }
    });
  }

  await runCase("Datei-Zwischenablage respektiert den offenen Sync-Modus", "?keyboard=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#btn-sync-close").focus();
    await page.keyboard.press("Control+C");
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "files_copy"));
    assert.equal((await page.evaluate(() => window.__MOCK_CLIPBOARD__())).writes, 0);
  });

  await runCase("Datei-Zwischenablage bietet native Dateimenüs und Kontextmenüs", "?keyboard=1", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("option", { name: /Alpha\.txt/ }).click({ button: "right" });
    await page.getByRole("menuitem", { name: "Kopieren", exact: true }).click();
    await page.waitForFunction(() => window.__MOCK_CLIPBOARD__().paths.length === 1);
    await page.locator('.pane[data-side="right"] .path').focus();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "files_paste"));
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    const entry = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0]);
    assert.equal(entry.operation.destination, "/mock/ziel");
    assert.deepEqual(entry.operation.items, ["/mock/quelle/Alpha.txt"]);
  });

  await runCase("Datei-Zwischenablage kopiert Mehrfachauswahl und lässt Textfelder nativ", "?keyboard=1", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("option", { name: /Alpha\.txt/ }).click();
    await left.getByRole("option", { name: /Beta\.txt/ }).click({ modifiers: ["Meta"] });
    await page.keyboard.press("Control+C");
    await page.waitForFunction(() => window.__MOCK_CLIPBOARD__().paths.length === 2);
    assert.deepEqual((await page.evaluate(() => window.__MOCK_CLIPBOARD__())).paths.sort(), ["/mock/quelle/Alpha.txt", "/mock/quelle/Beta.txt"]);
    await left.locator(".path").focus();
    await page.keyboard.press("Control+C");
    assert.equal((await page.evaluate(() => window.__MOCK_CLIPBOARD__())).writes, 1, "Textkopieren darf die Dateiauswahl nicht erneut schreiben.");
    await page.locator('.pane[data-side="right"] .listing').focus();
    await page.keyboard.press("Control+V");
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    const entry = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0]);
    assert.equal(entry.operation.kind, "Copy");
    assert.equal(entry.operation.destination, "/mock/ziel");
    assert.equal(entry.operation.items.length, 2);
    assert.equal((await page.evaluate(() => window.__MOCK_CLIPBOARD__())).paths.length, 2);
  });

  for (const scenario of ["vollständig", "teilweise", "geändert", "Fehler", "abgebrochen", "Symlink"]) {
    await runCase(`Datei-Zwischenablage verbraucht Move nur bei Erfolg: ${scenario}`, "?keyboard=1", async page => {
      await page.evaluate(() => window.__MOCK_CLIPBOARD__({ paths: ["/mock/quelle/Alpha.txt", "/external/Beta.txt"], cut: true }));
      await page.locator('.pane[data-side="right"] .listing').focus();
      await page.keyboard.press("Control+V");
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 2);
      const entries = await page.evaluate(() => structuredClone(window.__DOPPELDECKER_MOCK_QUEUE__.entries));
      assert.ok(entries.every(entry => entry.operation.kind === "Move"));
      assert.equal((await page.evaluate(() => window.__MOCK_CLIPBOARD__())).clears, 0, "Einreihen allein darf nichts verbrauchen.");
      await page.evaluate(({ entries, scenario }) => {
        if (scenario === "geändert") window.__MOCK_CLIPBOARD__({ paths: ["/newer/Datei.txt"], cut: false });
        entries.forEach((entry, index) => window.__MOCK_EMIT__(scenario === "Fehler" && index === 1 ? "fileop-error" : "fileop-done", {
          operationId: entry.operation_id, files: 1, bytes: 8, cancelled: scenario === "abgebrochen" && index === 1,
          skipped: scenario === "teilweise" && index === 1 ? 1 : 0,
          symlinks_skipped: scenario === "Symlink" && index === 1 ? 1 : 0, code: "io", message: "Transferfehler",
        }));
      }, { entries, scenario });
      if (scenario === "vollständig") await page.waitForFunction(() => window.__MOCK_CLIPBOARD__().clears === 1);
      const clipboard = await page.evaluate(() => window.__MOCK_CLIPBOARD__());
      assert.equal(clipboard.clears, scenario === "vollständig" ? 1 : 0);
      assert.equal(clipboard.paths.length, scenario === "vollständig" ? 0 : scenario === "geändert" ? 1 : 2);
    });
  }

  await runCase("Datei-Zwischenablage erhält Move-Auswahl bei gleichem Quellordner", "?keyboard=1", async page => {
    await page.evaluate(() => window.__MOCK_CLIPBOARD__({ paths: ["/mock/ziel/bleibt.txt", "/external/move.txt"], cut: true }));
    await page.locator('.pane[data-side="right"] .listing').focus();
    await page.keyboard.press("Control+V");
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", { operationId: window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation_id, files: 1, bytes: 8 }));
    assert.equal((await page.evaluate(() => window.__MOCK_CLIPBOARD__())).clears, 0);
  });

  await runCase("Datei-Zwischenablage meldet verschwundene Quellen und erhält Auswahl", "?keyboard=1", async page => {
    await page.evaluate(() => {
      window.__MOCK_CLIPBOARD__({ paths: ["/missing/datei.txt"], cut: true });
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "check_local_file_operation") throw { code: "not_found", message: "Quelle ist nicht mehr vorhanden" };
        return original(command, args);
      };
    });
    await page.locator('.pane[data-side="right"] .listing').focus();
    await page.keyboard.press("Control+V");
    await page.getByText(/Quelle ist nicht mehr vorhanden/).waitFor();
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
    assert.equal((await page.evaluate(() => window.__MOCK_CLIPBOARD__())).clears, 0);
  });

  await runCase("Datei-Zwischenablage verbraucht auch mehr als 256 abgeschlossene Gruppen", "?keyboard=1", async page => {
    await page.evaluate(() => {
      window.__MOCK_CLIPBOARD__({ paths: Array.from({ length: 257 }, (_, index) => `/external/${index}/file.txt`), cut: true });
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await original(command, args);
        if (command === "move_paths") window.__MOCK_EMIT__("fileop-done", { operationId: result, files: 1, bytes: 8 });
        return result;
      };
    });
    await page.locator('.pane[data-side="right"] .listing').focus();
    await page.keyboard.press("Control+V");
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 257);
    await page.waitForTimeout(100);
    assert.equal((await page.evaluate(() => window.__MOCK_CLIPBOARD__())).clears, 1);
  });

  await runCase("Datei-Zwischenablage stoppt Restgruppen nach Weg-und-zurück-Navigation", "?keyboard=1", async page => {
    await page.evaluate(() => {
      window.__MOCK_CLIPBOARD__({ paths: ["/external/a/A.txt", "/external/b/B.txt"], cut: true });
      const original = window.__TAURI__.core.invoke;
      let checks = 0;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "check_local_file_operation" && ++checks === 2) return new Promise(resolve => { window.__RELEASE_SECOND_GROUP__ = () => resolve(original(command, args)); });
        return original(command, args);
      };
    });
    await page.locator('.pane[data-side="right"] .listing').focus();
    await page.keyboard.press("Control+V");
    await page.waitForFunction(() => !!window.__RELEASE_SECOND_GROUP__);
    const path = page.locator('.pane[data-side="right"] .path');
    await path.fill("/mock/fast"); await path.press("Enter");
    await page.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    await path.fill("/mock/ziel"); await path.press("Enter");
    await page.evaluate(() => window.__RELEASE_SECOND_GROUP__());
    await page.getByText(/Zwischenablage unvollständig/).waitFor();
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 1);
    assert.equal((await page.evaluate(() => window.__MOCK_CLIPBOARD__())).clears, 0);
  });

  await runCase("Datei-Zwischenablage bindet Ziel vor langsamem Lesen", "?keyboard=1", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => command === "read_file_clipboard"
        ? new Promise(resolve => { window.__RELEASE_CLIPBOARD__ = () => resolve({ paths: ["/external/A.txt"], cut: false, version: "1" }); })
        : original(command, args);
    });
    await page.locator('.pane[data-side="right"] .listing').focus();
    await page.keyboard.press("Control+V");
    await page.waitForFunction(() => !!window.__RELEASE_CLIPBOARD__);
    const path = page.locator('.pane[data-side="right"] .path');
    await path.fill("/mock/fast"); await path.press("Enter");
    await page.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    await page.evaluate(() => window.__RELEASE_CLIPBOARD__());
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });

  await runCase("Pane-Tabs behalten Auswahl, Sortierung und getrennte Panes", "?keyboard", async page => {
    const left = page.locator('.pane[data-side="left"]');
    const right = page.locator('.pane[data-side="right"]');
    await left.getByRole("option", { name: /Alpha\.txt/ }).click();
    await left.locator('[data-sort="name"]').click();
    await left.getByRole("button", { name: "Neuer Tab", exact: true }).click();
    assert.equal(await left.getByRole("tab").count(), 2);
    assert.equal(await left.locator('[data-sort="name"]').evaluate(el => el.classList.contains("desc")), true);
    await left.locator('[data-sort="name"]').click();
    await left.locator(".path").fill("/mock/fast");
    await left.locator(".path").press("Enter");
    await left.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    await left.getByRole("tab").first().click();
    await left.getByRole("option", { name: /Alpha\.txt/ }).waitFor();
    assert.equal(await left.getByRole("option", { name: /Alpha\.txt/ }).getAttribute("aria-selected"), "true");
    assert.equal(await left.locator('[data-sort="name"]').evaluate(el => el.classList.contains("desc")), true);
    assert.equal(await right.getByRole("tab").count(), 1);
    assert.equal(await right.locator(".path").inputValue(), "/mock/ziel");
    await left.getByRole("tab").first().press("ArrowRight");
    await left.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    await right.getByRole("button", { name: "Neuer Tab", exact: true }).focus();
    await page.keyboard.press("Enter");
    assert.equal(await right.getByRole("tab").count(), 2);
    assert.equal(await right.evaluate(el => el.classList.contains("active")), true);
    await page.keyboard.press("Control+Shift+W");
    assert.equal(await right.getByRole("tab").count(), 1);
    assert.equal(await right.locator(".pane-tab-close").isDisabled(), true);
    assert.equal(await left.getByRole("tab").count(), 2);
    if (process.env.DOPPELDECKER_SCREENSHOT_DIR) {
      await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/pane-tabs.png` });
      await page.setViewportSize({ width: 1024, height: 700 });
      await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/pane-tabs-kompakt.png` });
    }
  });

  await runCase("Pane-Tabs behalten eigene Darstellung beim Refresh desselben Orts", "?keyboard&restoreTabs", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("button", { name: "Neuer Tab", exact: true }).click();
    const toggle = left.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" });
    await toggle.click();
    await left.getByRole("dialog", { name: "Darstellung" }).getByLabel("Versteckte Dateien", { exact: true }).check();
    await left.locator(".row", { hasText: ".verborgen" }).waitFor();
    await toggle.click();
    await left.getByRole("tab").first().click();
    await left.getByRole("option", { name: /Alpha\.txt/ }).waitFor();
    assert.equal(await left.locator(".row", { hasText: ".verborgen" }).count(), 0);
    const before = await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length);
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "refresh"));
    await page.waitForFunction(count => window.__MOCK_SCAN_CALLS__().length > count, before);
    await page.waitForFunction(() => document.querySelector('.pane[data-side="left"] [role="tab"][aria-selected="true"]')?.getAttribute("aria-busy") === "false");
    assert.equal(await left.locator(".row", { hasText: ".verborgen" }).count(), 0);
    await left.getByRole("tab").nth(1).click();
    await left.locator(".row", { hasText: ".verborgen" }).waitFor();
  });

  await runCase("Pane-Tabs behalten bewiesene Auswahl nach Listingfehler und direktem Retry", "?keyboard&restoreTabs&scanErrorOnSecond=slow", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("button", { name: "Neuer Tab", exact: true }).click();
    await left.locator(".path").fill("/mock/fast");
    await left.locator(".path").press("Enter");
    await left.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    await left.getByRole("tab").first().click();
    await left.getByRole("option", { name: /Alpha\.txt/ }).waitFor();
    await left.locator(".path").fill("/mock/slow");
    await left.locator(".path").press("Enter");
    await left.getByRole("option", { name: /Langsam\.txt/ }).click();
    await page.waitForFunction(() => window.__MOCK_DESKTOP__().rememberedWindows.at(-1)
      ?.tabs.left.tabs[0].selectedNames.includes("Langsam.txt"));
    await left.getByRole("tab").nth(1).click();
    await left.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    await left.getByRole("tab").first().click();
    await page.locator("#dock-summary").filter({ hasText: "Mock-Scanfehler" }).waitFor();
    await page.waitForTimeout(1000);
    const saved = await page.evaluate(() => window.__MOCK_DESKTOP__().rememberedWindows.at(-1).tabs.left.tabs[0]);
    assert.deepEqual(saved.selectedNames, ["Langsam.txt"]);
    assert.equal(saved.cursorName, "Langsam.txt");
    assert.equal(saved.anchorName, "Langsam.txt");
    assert.equal(await left.getByRole("option").count(), 0);
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "refresh"));
    await left.getByRole("option", { name: /Langsam\.txt/ }).waitFor();
    assert.equal(await left.getByRole("option", { name: /Langsam\.txt/ }).getAttribute("aria-selected"), "true");
  });

  await runCase("Pane-Tabs verwerfen Antworten geschlossener Tabs", "?keyboard&scanDelay=250", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("button", { name: "Neuer Tab", exact: true }).click();
    await left.locator(".path").fill("/mock/slow");
    await left.locator(".path").press("Enter");
    await page.waitForFunction(() => window.__MOCK_SCAN_CALLS__().some(call => call.path === "/mock/slow"));
    await left.locator(".pane-tab-close").nth(1).click();
    await left.getByRole("option", { name: /Alpha\.txt/ }).waitFor();
    await page.waitForTimeout(300);
    assert.equal(await left.getByRole("tab").count(), 1);
    assert.equal(await left.locator(".path").inputValue(), "/mock/quelle");
    assert.equal(await left.getByRole("option", { name: /Langsam\.txt/ }).count(), 0);
  });

  await runCase("Pane-Tabs beenden den Ladezustand nach Listingfehler", "?keyboard&scanErrorOnSecond=quelle", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("button", { name: "Neuer Tab", exact: true }).click();
    await page.locator("#dock-summary").filter({ hasText: "Mock-Scanfehler" }).waitFor();
    const failed = left.getByRole("tab").nth(1);
    assert.equal(await failed.getAttribute("aria-busy"), "false");
    assert.match(await failed.getAttribute("title"), /Laden fehlgeschlagen/);
    await left.getByRole("tab").first().click();
    await left.getByRole("option", { name: /Alpha\.txt/ }).waitFor();
  });

  await runCase("Pane-Tabs erhalten entfernte inaktive Profile ohne alte Zeilen oder neuen Zugriff", "", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("button", { name: "Neuer Tab", exact: true }).click();
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await left.getByRole("option", { name: /remote/ }).waitFor();
    await left.getByRole("tab").first().click();
    await left.getByRole("option", { name: /Beispiel\.txt/ }).waitFor();
    await page.evaluate(() => {
      window.__MOCK_REMOVE_PROFILE__("sftp", "mock_sftp");
      window.__MOCK_EMIT__("connections-updated", {
        protocol: "sftp", profileId: "mock_sftp", revision: 11, change: "removed",
      });
    });
    await page.locator('#connection-list [data-protocol="sftp"]').waitFor({ state: "detached" });
    const before = await page.evaluate(() => ({
      scans: window.__MOCK_SCAN_CALLS__().length,
      cancels: window.__MOCK_DIRECTORY_SIZES__().cancels.length,
    }));
    await left.getByRole("tab").nth(1).click();
    await page.locator("#dock-summary").filter({ hasText: "nicht mehr vorhanden" }).waitFor();
    assert.equal(await left.getByRole("option").count(), 0);
    assert.equal(await left.getByRole("tab").count(), 2);
    assert.equal(await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length), before.scans);
    assert.ok(await page.evaluate(() => window.__MOCK_DIRECTORY_SIZES__().cancels.length) > before.cancels);
    await left.getByRole("tab").first().click();
    await left.getByRole("option", { name: /Beispiel\.txt/ }).waitFor();
  });

  await runCase("Pane-Tabs stellen beide Tab-Leisten und aktiven Pane nach Neustart wieder her", "?keyboard&restoreTabs", async page => {
    const left = page.locator('.pane[data-side="left"]');
    const right = page.locator('.pane[data-side="right"]');
    await left.getByRole("option", { name: /Beta\.txt/ }).click();
    await page.keyboard.press("Control+T");
    await left.locator(".path").fill("/mock/fast");
    await left.locator(".path").press("Enter");
    await left.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    await right.getByRole("button", { name: "Neuer Tab", exact: true }).click();
    await page.waitForFunction(() => {
      const saved = window.__MOCK_DESKTOP__().rememberedWindows.at(-1);
      return saved?.tabs?.left.tabs.length === 2 && saved.tabs.right.tabs.length === 2 && saved.activeSide === "right";
    });
    await page.reload({ waitUntil: "networkidle" });
    await left.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    assert.equal(await left.getByRole("tab").count(), 2);
    assert.equal(await right.getByRole("tab").count(), 2);
    assert.equal(await right.evaluate(el => el.classList.contains("active")), true);
    await left.getByRole("tab").first().click();
    await left.getByRole("option", { name: /Beta\.txt/ }).waitFor();
    assert.equal(await left.getByRole("option", { name: /Beta\.txt/ }).getAttribute("aria-selected"), "true");
  });

  await runCase("Pane-Tabs speichern Tastaturauswahl und Sortierung ohne Ortswechsel", "?keyboard&restoreTabs", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await left.getByRole("option", { name: /Beta\.txt/ }).click();
    await page.waitForFunction(() => window.__MOCK_DESKTOP__().rememberedWindows.at(-1)
      ?.tabs.left.tabs[0].selectedNames.includes("Beta.txt"));
    await page.keyboard.press("ArrowUp");
    await page.waitForFunction(() => window.__MOCK_DESKTOP__().rememberedWindows.at(-1)
      ?.tabs.left.tabs[0].selectedNames.includes("Alpha.txt"), null, { timeout: 1500 });
    await page.keyboard.press("Control+Digit1");
    await page.waitForFunction(() => window.__MOCK_DESKTOP__().rememberedWindows.at(-1)
      ?.tabs.left.tabs[0].sort.dir === -1, null, { timeout: 1500 });
    await page.reload({ waitUntil: "networkidle" });
    assert.equal(await left.getByRole("option", { name: /Alpha\.txt/ }).getAttribute("aria-selected"), "true");
    assert.equal(await left.locator('[data-sort="name"]').evaluate(el => el.classList.contains("desc")), true);
  });

  await runCase("Pane-Tabs speichern Scrollposition ohne Ortswechsel", "?entries=5000&restoreTabs", async page => {
    const left = page.locator('.pane[data-side="left"]');
    await page.waitForFunction(() => window.__MOCK_DESKTOP__().rememberedWindows.at(-1)?.tabs);
    await left.getByRole("listbox").evaluate(el => { el.scrollTop = 4500; });
    await page.waitForFunction(() => window.__MOCK_DESKTOP__().rememberedWindows.at(-1)
      ?.tabs.left.tabs[0].scrollTop === 4500, null, { timeout: 1500 });
    await page.reload({ waitUntil: "networkidle" });
    assert.equal(await left.getByRole("listbox").evaluate(el => el.scrollTop), 4500);
  });

  await runCase("Pane-Tabs erhalten virtuelle Scrollposition und begrenzen neue Tabs", "?entries=5000", async page => {
    const left = page.locator('.pane[data-side="left"]');
    const listing = left.getByRole("listbox");
    await listing.evaluate(el => { el.scrollTop = 4500; });
    await page.waitForFunction(() => document.querySelector('.pane[data-side="left"] .listing').scrollTop === 4500);
    await left.getByRole("button", { name: "Neuer Tab", exact: true }).click();
    await left.getByRole("tab").first().click();
    await page.waitForFunction(() => document.querySelector('.pane[data-side="left"] .listing').scrollTop === 4500);
    await left.locator(".path").fill("/mock/fast");
    await left.locator(".path").press("Enter");
    await left.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    for (let n = 2; n < 32; n += 1) {
      await left.getByRole("button", { name: "Neuer Tab", exact: true }).click();
    }
    assert.equal(await left.getByRole("tab").count(), 32);
    assert.equal(await left.getByRole("button", { name: "Neuer Tab", exact: true }).isDisabled(), true);
    await page.keyboard.press("Control+T");
    assert.equal(await left.getByRole("tab").count(), 32);
    await left.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    await page.keyboard.press("Control+Tab");
    await page.waitForFunction(() => {
      const bar = document.querySelector('.pane[data-side="left"] .pane-tabs');
      const active = bar.querySelector('.pane-tab.selected').getBoundingClientRect();
      const bounds = bar.getBoundingClientRect();
      return active.left >= bounds.left && active.right <= bounds.right;
    }, null, { timeout: 1500 });
  });

  await runCase("Einstellungen bündeln globale Optionen und sechs App-Icons", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "preferences"));
    const dialog = page.getByRole("dialog", { name: "Einstellungen" });
    await dialog.waitFor();
    assert.equal(await dialog.locator('input[name="icon"]').count(), 6);
    for (const [label, pageId, url] of [
      ["Support", "support", "https://github.com/DanielMuellerIR/Doppeldecker/issues"],
      ["Impressum", "imprint", "https://dm0.de/impressum.html"],
      ["Datenschutz", "privacy", "https://dm0.de/datenschutz.html#doppeldecker"],
    ]) {
      const link = dialog.getByRole("link", { name: label, exact: true });
      assert.equal(await link.getAttribute("href"), url);
      await link.click();
      assert.equal(await page.evaluate(() => window.__MOCK_PROJECT_PAGE__), pageId);
    }
    assert.equal(await dialog.getByRole("radio", { name: /Fastra/ }).isDisabled(), true);
    await dialog.getByRole("radio", { name: /Sonnenuntergang/ }).check();
    await dialog.getByLabel(/Fenster und Pane-Ziele/).check();
    await dialog.getByLabel("Darstellung").selectOption("dark");
    await page.waitForFunction(() => window.__MOCK_DESKTOP__().preferences.icon === "sunset"
      && window.__MOCK_DESKTOP__().preferences.restoreSession === true
      && document.documentElement.dataset.resolvedTheme === "dark");
    if (process.env.DOPPELDECKER_SCREENSHOT_DIR) {
      await page.screenshot({
        path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/einstellungen.png`,
        fullPage: false,
      });
    }
  });

  await runCase("Ein zweiter Menübefehl lässt den Einstellungsdialog schließbar", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "preferences"));
    const dialog = page.getByRole("dialog", { name: "Einstellungen" });
    await dialog.waitFor();
    // Der Menüeintrag „Einstellungen …" bleibt auch bei offenem Dialog erreichbar.
    // Ein zweiter Durchlauf ersetzte früher das Modal-Token durch das eines nur
    // eingereihten Leases; danach schloss weder „Fertig" noch Escape den sichtbaren
    // Dialog, und das Fenster blieb bis zum Schließen unbedienbar.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "preferences"));
    await dialog.getByRole("button", { name: "Fertig" }).click();
    await dialog.waitFor({ state: "hidden" });
    // Und danach lässt er sich wie gewohnt wieder öffnen.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "preferences"));
    await dialog.waitFor();
    await page.keyboard.press("Escape");
    await dialog.waitFor({ state: "hidden" });
  });

  await runCase("Gleichzeitige Einstellungsabrufe lassen Fertig und Escape wirksam", "", async page => {
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      let release;
      const pending = new Promise(resolve => { release = resolve; });
      window.__RELEASE_SETTINGS__ = release;
      window.__SETTINGS_REQUESTS__ = 0;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (["desktop_preferences", "diff_tool_status"].includes(command)) {
          window.__SETTINGS_REQUESTS__ += 1;
          await pending;
        }
        return invoke(command, args);
      };
      window.__MOCK_EMIT__("menu", "preferences");
      window.__MOCK_EMIT__("menu", "preferences");
    });
    await page.waitForFunction(() => window.__SETTINGS_REQUESTS__ >= 2);
    await page.evaluate(() => window.__RELEASE_SETTINGS__());
    const dialog = page.getByRole("dialog", { name: "Einstellungen" });
    await dialog.waitFor();
    await dialog.getByRole("button", { name: "Fertig" }).click();
    await dialog.waitFor({ state: "hidden", timeout: 2_000 });
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "preferences"));
    await dialog.waitFor();
    await page.keyboard.press("Escape");
    await dialog.waitFor({ state: "hidden", timeout: 2_000 });
  });

  await runCase("Dateivergleich übergibt je eine sicher ausgewählte Pane-Datei", "?demo=1", async page => {
    const lists = page.getByRole("listbox");
    await lists.nth(0).getByRole("option", { name: /Reisebericht\.md/ }).click();
    await lists.nth(1).getByRole("option", { name: /Reisebericht\.md/ }).click();
    await page.evaluate(() => { window.__MOCK_EMIT__("menu", "compare_files"); });
    await page.waitForFunction(() => window.__MOCK_DESKTOP__().diffCalls.length === 1);
    const [call] = await page.evaluate(() => window.__MOCK_DESKTOP__().diffCalls);
    assert.equal(call.left.path, "/Users/demo/Projekte/Reisebericht.md");
    assert.equal(call.right.path, "/Volumes/Backup/Projekte/Reisebericht.md");
    assert.equal(call.left.name, "Reisebericht.md");
    assert.equal(call.right.name, "Reisebericht.md");
  });

  for (const theme of ["light", "dark"]) {
    await runCase(`Dateivergleich zeigt Best Effort bei 760px · ${theme}`, "?demo=1&diffBestEffort=1", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme); }, theme);
      const lists = page.getByRole("listbox");
      await lists.nth(0).getByRole("option", { name: /Reisebericht\.md/ }).click();
      await lists.nth(1).getByRole("option", { name: /Reisebericht\.md/ }).click();
      await page.evaluate(() => { window.__MOCK_EMIT__("menu", "compare_files"); });
      await page.locator("#dock-summary").filter({ hasText: "Dateivergleich · Best Effort" }).waitFor();
      assert.match(await page.locator("#dock-summary").innerText(), /pfadbasiertes Öffnungsrisiko/);
      assert.equal(await page.locator("#progress").isVisible(), false);
      const dock = await page.locator("#dock").boundingBox();
      assert.ok(dock && dock.x >= 0 && dock.x + dock.width <= 761 && dock.y + dock.height <= 481);
    });
    await runCase(`Dateivergleich ist während der Vorbereitung abbrechbar · ${theme}`, "?demo=1&diffBestEffort=1&diffHold=1", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme); }, theme);
      const lists = page.getByRole("listbox");
      await lists.nth(0).getByRole("option", { name: /Reisebericht\.md/ }).click();
      await lists.nth(1).getByRole("option", { name: /Reisebericht\.md/ }).click();
      await page.evaluate(() => { window.__MOCK_EMIT__("menu", "compare_files"); });
      await page.locator("#dock-summary").filter({ hasText: "Abbrechbarer Dateivergleich · Best Effort" }).waitFor();
      await page.locator("#btn-cancel").click();
      await page.locator("#dock-summary").filter({ hasText: "Dateivergleich abgebrochen" }).waitFor();
      assert.equal(await page.locator("#progress").isVisible(), false);
      const effects = await page.evaluate(() => window.__MOCK_IPC_EFFECTS__());
      assert.equal(effects.cancelSyncCalls, 1);
      assert.deepEqual(effects.appErrors, []);
    });
  }
  await runCase("Dateivergleich verspricht nach Werkzeugübergabe keinen Escape-Abbruch", "?demo=1&diffHold=1", async page => {
    const lists = page.getByRole("listbox");
    await lists.nth(0).getByRole("option", { name: /Reisebericht\.md/ }).click();
    await lists.nth(1).getByRole("option", { name: /Reisebericht\.md/ }).click();
    await page.evaluate(() => { window.__MOCK_EMIT__("menu", "compare_files"); });
    await page.locator("#btn-cancel").waitFor({ state: "visible" });
    await page.evaluate(() => { window.__MOCK_EMIT__("file-diff-started", { phase: "handoff", limitations: [] }); });
    await page.locator("#dock-summary").filter({ hasText: "Werkzeugübergabe" }).waitFor();
    assert.equal(await page.locator("#btn-cancel").isVisible(), false);
    await page.keyboard.press("Escape");
    assert.equal((await page.evaluate(() => window.__MOCK_IPC_EFFECTS__())).cancelSyncCalls, 0);
    await page.evaluate(() => { window.__MOCK_EMIT__("file-diff-done", { limitations: [] }); });
    await page.locator("#dock-summary").filter({ hasText: "wurden an das gewählte Werkzeug übergeben" }).waitFor();
  });
  await runCase("Dateivergleich erhält unbestätigten Helferausgang ohne Erfolgsmeldung", "?demo=1&diffFailure=1", async page => {
    const lists = page.getByRole("listbox");
    await lists.nth(0).getByRole("option", { name: /Reisebericht\.md/ }).click();
    await lists.nth(1).getByRole("option", { name: /Reisebericht\.md/ }).click();
    await page.evaluate(() => { window.__MOCK_EMIT__("menu", "compare_files"); });
    await page.locator("#dock-summary").filter({ hasText: "Helferübergabe unbestätigt" }).waitFor();
    await page.waitForFunction(() => window.__MOCK_IPC_EFFECTS__().appErrors.length > 0);
    assert.equal(await page.locator("#progress").isVisible(), false);
    assert.doesNotMatch(await page.locator("#dock-summary").innerText(), /wurden an das gewählte Werkzeug übergeben/);
    assert.deepEqual((await page.evaluate(() => window.__MOCK_IPC_EFFECTS__())).appErrors, [{ context: "Dateivergleich", code: "outcome_unclear" }]);
  });
  await runCase("Dateivergleichsantwort überschreibt keine folgende Queuevorbereitung", "?demo=1&diffNextPreparation=1", async page => {
    const lists = page.getByRole("listbox");
    await lists.nth(0).getByRole("option", { name: /Reisebericht\.md/ }).click();
    await lists.nth(1).getByRole("option", { name: /Reisebericht\.md/ }).click();
    await page.evaluate(() => { window.__MOCK_EMIT__("menu", "compare_files"); });
    await page.locator("#dock-summary").filter({ hasText: "Folgende Queuevorbereitung bleibt sichtbar" }).waitFor();
    await page.waitForTimeout(50);
    assert.match(await page.locator("#dock-summary").innerText(), /Folgende Queuevorbereitung bleibt sichtbar/);
    assert.equal(await page.locator("#progress").isVisible(), true);
  });

  await runCase("Vollständige Pane-Orte brauchen beim neuen Fenster keine Defaults", "?defaultErrorAfterStartup=1", async page => {
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).focus();
    await page.keyboard.press("Control+N");
    await page.waitForFunction(() => window.__MOCK_NEW_WINDOWS__().length === 1);
    let calls = await page.evaluate(() => window.__MOCK_NEW_WINDOWS__());
    assert.deepEqual(calls[0], {
      left: { provider: "local", profileId: null, profileRevision: null,
        profileName: null, path: "/mock/quelle" },
      right: { provider: "local", profileId: null, profileRevision: null,
        profileName: null, path: "/mock/ziel" },
    });

    await page.evaluate(() => window.__MOCK_EMIT__("menu", "new_window"));
    await page.waitForFunction(() => window.__MOCK_NEW_WINDOWS__().length === 2);
    calls = await page.evaluate(() => window.__MOCK_NEW_WINDOWS__());
    assert.deepEqual(calls[1], calls[0]);
    assert.equal(await page.evaluate(() => window.__MOCK_DEFAULT_DIR_CALLS__()), 1,
      "Nach dem Start darf kein Default-Abruf die vollständigen Orte blockieren.");
  });

  await runCase("Neues Fenster friert beide Pane-Orte vor langsamen Defaults ein", "?emptyWindowStart=1&defaultDelay=120", async page => {
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).focus();
    await page.keyboard.press("Control+N");
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.waitForFunction(() => window.__MOCK_NEW_WINDOWS__().length === 1);
    const [call] = await page.evaluate(() => window.__MOCK_NEW_WINDOWS__());
    assert.deepEqual(call, {
      left: { provider: "local", profileId: null, profileRevision: null,
        profileName: null, path: "/mock/quelle" },
      right: { provider: "local", profileId: null, profileRevision: null,
        profileName: null, path: "/mock/ziel" },
    }, "Die Navigation während des Default-Abrufs darf den eingefrorenen Startort nicht ersetzen.");
  });

  await runCase("Neues Fenster bindet den sichtbaren Remote-Profilstand", "", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /remote/ }).waitFor();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).focus();
    await page.keyboard.press("Control+N");
    await page.waitForFunction(() => window.__MOCK_NEW_WINDOWS__().length === 1);
    const [call] = await page.evaluate(() => window.__MOCK_NEW_WINDOWS__());
    assert.deepEqual(call.left, {
      provider: "sftp",
      profileId: "mock_sftp",
      profileRevision: 11,
      profileName: "Mock-SFTP",
      path: "/",
    });
  });

  await runCase("Startup-Queuefehler wird nach der Eventverdrahtung einmal angezeigt", "?startupQueueError=1", async page => {
    await page.getByText(/Nebenfenster-Journal window-defekt braucht Recovery/).waitFor();
    assert.equal(await page.locator("#queue-count").innerText(), "!");
    assert.deepEqual(await page.evaluate(() => window.__MOCK_STARTUP_STATE__()), {
      reads: 1,
      wiredAtRead: true,
      delivered: true,
    });
    assert.equal(
      await page.evaluate(() => window.__TAURI__.core.invoke("take_startup_queue_error")),
      null,
      "Der Startfehler darf nach der ersten Anzeige nicht nochmals geliefert werden.",
    );
    await page.evaluate(() => window.__MOCK_EMIT__("queue-updated", {
      version: 8, paused: false, stop_on_error: false, entries: [],
    }));
    assert.equal(await page.locator("#queue-count").innerText(), "!",
      "Ein später Queue-Snapshot darf den offenen Startup-Recoveryfehler nicht verdecken.");
  });

  await runCase("Startup-Queuefehler bleibt auf das Hauptfenster begrenzt", "?startupQueueError=1&windowLabel=window-1", async page => {
    await page.waitForFunction(() => window.__MOCK_STARTUP_STATE__().reads === 1);
    assert.deepEqual(await page.evaluate(() => window.__MOCK_STARTUP_STATE__()), {
      reads: 1,
      wiredAtRead: true,
      delivered: false,
    });
    assert.doesNotMatch(await page.locator("#dock-summary").innerText(), /window-defekt/);
  });

  await runCase("Kosmetischer Startfehler verdeckt den Queue-Zähler nicht", "?startupAppError=1", async page => {
    await page.getByText(/App-Icon konnte nicht gesetzt werden/).waitFor();
    assert.equal(await page.locator("#queue-count").innerText(), "0");
    assert.match(await page.locator("#dock-summary").innerText(), /Start-Fehler/);
  });

  await runCase("Einstellungsfehler verändert den Queue-Zähler nicht", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("settings-error", {
      code: "io", message: "Einstellungsdatei konnte nicht synchronisiert werden",
    }));
    await page.getByText(/Einstellungsdatei konnte nicht synchronisiert werden/).waitFor();
    assert.equal(await page.locator("#queue-count").innerText(), "0");
  });

  await runCase("Mock-IPC bildet Rename, Abbruch und Fehlerlog ab und lehnt Unbekanntes ab", "", async page => {
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await list.focus();
    await page.keyboard.press("F2");
    await page.locator("#modal-input").fill("Umbenannt.txt");
    await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK" }).click();
    await list.getByRole("option", { name: /Umbenannt\.txt/ }).waitFor();

    const unknown = await page.evaluate(async () => {
      await window.__TAURI__.core.invoke("cancel_sync");
      await window.__TAURI__.core.invoke("record_app_error", {
        context: "Mock-Vertrag", code: "test_code",
      });
      try {
        await window.__TAURI__.core.invoke("command_gibt_es_nicht");
        return null;
      } catch (error) {
        return { code: error?.code, message: error?.message };
      }
    });
    assert.deepEqual(unknown, {
      code: "mock_unknown_command",
      message: "Unbekannter Mock-Tauri-Command: command_gibt_es_nicht",
    });
    assert.deepEqual(await page.evaluate(() => window.__MOCK_IPC_EFFECTS__()), {
      appErrors: [{ context: "Mock-Vertrag", code: "test_code" }],
      cancelSyncCalls: 1,
      lastLocalRename: "/mock/quelle/Umbenannt.txt",
    });
  });

  await runCase("Fehler beim Merken der CLI-Ablehnung wird sichtbar", "?cliOffer=1&cliDeclineError=1", async page => {
    const dialog = page.getByRole("dialog", { name: "Terminalbefehl einrichten?" });
    await dialog.waitFor();
    await dialog.getByRole("button", { name: "Nicht mehr fragen" }).click();
    await page.getByText(/CLI-Entscheidung konnte nicht gespeichert werden/).waitFor();
    assert.notEqual(await page.locator("html").getAttribute("data-cli-declined"), "true");
  });

  await runCase("Echter Fehler beim Lesen des CLI-Status wird sichtbar", "?cliStatusError=1", async page => {
    await page.getByText(/CLI-Status konnte nicht gelesen werden/).waitFor();
  });

  await runCase("Updates werden nur manuell und zweistufig installiert", "?update=available", async page => {
    assert.deepEqual(await page.evaluate(() => window.__MOCK_UPDATES__()), [],
      "Der Start darf den Updatekanal nicht abfragen.");
    await page.evaluate(() => { void window.__MOCK_EMIT__("menu", "check_updates"); });
    const dialog = page.getByRole("dialog");
    await dialog.getByText("Update 0.1.124 verfügbar").waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_UPDATES__()), [
      { command: "check_for_update" },
    ]);
    assert.equal(await dialog.locator("img").count(), 0,
      "Release-Hinweise dürfen kein Markup einschleusen.");
    await dialog.getByRole("button", { name: "Herunterladen und installieren" }).click();
    await page.getByText(/Update installiert/).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_UPDATES__()), [
      { command: "check_for_update" },
      { command: "install_update", expectedVersion: "0.1.124" },
    ]);
    assert.match(await page.locator("#dock-summary").innerText(), /beim nächsten Start/);
  });

  await runCase("Aktueller Stand lädt kein Update", "", async page => {
    await page.evaluate(() => { void window.__MOCK_EMIT__("menu", "check_updates"); });
    await page.getByText(/ist die neueste verfügbare Version/).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_UPDATES__()), [
      { command: "check_for_update" },
    ]);
  });

  await runCase("Fehlgeschlagene Installation verlangt einen frisch vorbereiteten Updatefund", "?update=available&installFailsOnce=1", async page => {
    await page.evaluate(() => { void window.__MOCK_EMIT__("menu", "check_updates"); });
    const update = page.getByRole("dialog", { name: "Update 0.1.124 verfügbar" });
    await update.getByRole("button", { name: "Herunterladen und installieren" }).click();
    await page.getByText(/Mock-Download nach Invocation fehlgeschlagen/).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_UPDATES__()), [
      { command: "check_for_update" },
      { command: "install_update", expectedVersion: "0.1.124" },
      { command: "prepared_update_status" },
    ]);

    await page.evaluate(() => { void window.__MOCK_EMIT__("menu", "check_updates"); });
    await update.waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_UPDATES__()), [
      { command: "check_for_update" },
      { command: "install_update", expectedVersion: "0.1.124" },
      { command: "prepared_update_status" },
      { command: "check_for_update" },
    ], "Nach begonnenem Installationsfehler muss das Backend den Fund neu vorbereiten.");
    await update.getByRole("button", { name: "Herunterladen und installieren" }).click();
    await page.getByText(/Update installiert/).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_UPDATES__()), [
      { command: "check_for_update" },
      { command: "install_update", expectedVersion: "0.1.124" },
      { command: "prepared_update_status" },
      { command: "check_for_update" },
      { command: "install_update", expectedVersion: "0.1.124" },
    ]);
  });

  await runCase("Transportfehler vor Updatezustellung erhält den vorbereiteten Fund", "?update=available&installDispatchFailsOnce=1", async page => {
    await page.evaluate(() => { void window.__MOCK_EMIT__("menu", "check_updates"); });
    const update = page.getByRole("dialog", { name: "Update 0.1.124 verfügbar" });
    await update.getByRole("button", { name: "Herunterladen und installieren" }).click();
    await page.getByText(/Transportfehler vor Command-Zustellung/).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_UPDATES__()), [
      { command: "check_for_update" },
      { command: "prepared_update_status" },
    ]);

    await page.evaluate(() => { void window.__MOCK_EMIT__("menu", "check_updates"); });
    await update.waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_UPDATES__()), [
      { command: "check_for_update" },
      { command: "prepared_update_status" },
    ], "Der weiterhin backendseitig vorbereitete Fund braucht keinen zweiten HTTPS-Abruf.");
    await update.getByRole("button", { name: "Herunterladen und installieren" }).click();
    await page.getByText(/Update installiert/).waitFor();
  });

  await runCase("Verspätete Updateantwort wartet hinter einem F2-Dialog", "?update=available&updateDelay=120", async page => {
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await page.evaluate(() => { void window.__MOCK_EMIT__("menu", "check_updates"); });
    await list.focus();
    await page.keyboard.press("F2");
    const input = page.locator("#modal-input");
    await input.waitFor();
    await page.waitForTimeout(180);
    assert.match(await page.locator("#modal-title").innerText(), /Umbenennen/,
      "Die Updateantwort darf den offenen F2-Dialog nicht verdrängen.");
    await input.fill("nach-update.txt");
    await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
    const update = page.getByRole("dialog", { name: "Update 0.1.124 verfügbar" });
    await update.waitFor();
    await update.getByRole("button", { name: "Später" }).click();
  });

  await runCase("Verspätete Updateantwort wartet hinter einem Konfliktdialog", "?update=available&updateDelay=120&conflicts=replace", async page => {
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await page.evaluate(() => { void window.__MOCK_EMIT__("menu", "check_updates"); });
    await list.focus();
    await page.keyboard.press("F5");
    const conflict = page.getByRole("dialog", { name: "Konflikte beim Kopieren" });
    await conflict.waitFor();
    await page.waitForTimeout(180);
    assert.match(await page.locator("#modal-title").innerText(), /Konflikte beim Kopieren/);
    await conflict.getByRole("button", { name: "Überspringen" }).click();
    const update = page.getByRole("dialog", { name: "Update 0.1.124 verfügbar" });
    await update.waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_ACTION_ORDER__()), ["copy_paths"],
      "Die Konfliktfortsetzung muss vor dem nächsten unabhängigen Dialog laufen.");
    await update.getByRole("button", { name: "Später" }).click();
  });

  await runCase("Verspätete Updateantwort wartet hinter dem Realmodusdialog", "?update=available&updateDelay=120", async page => {
    await page.evaluate(() => { void window.__MOCK_EMIT__("menu", "check_updates"); });
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByRole("button", { name: "Realmodus" }).click();
    const realMode = page.getByRole("dialog", { name: "Lokalen Realmodus bestätigen" });
    await realMode.waitFor();
    await page.waitForTimeout(180);
    assert.match(await page.locator("#modal-title").innerText(), /Lokalen Realmodus bestätigen/);
    await realMode.getByRole("button", { name: "Vorbereitung verwerfen" }).click();
    const update = page.getByRole("dialog", { name: "Update 0.1.124 verfügbar" });
    await update.waitFor();
    await update.getByRole("button", { name: "Später" }).click();
  });

  await runCase("Eingereihter Verbindungseditor berührt den aktiven Modal-Knoten nicht", "", async page => {
    await page.evaluate(async () => {
      const modal = await import("/modal.js");
      void modal.promptModal("Bestehender Besitzer", "unverändert");
    });
    const owner = page.getByRole("dialog", { name: "Bestehender Besitzer" });
    const input = owner.locator("#modal-input");
    await input.waitFor();
    await page.evaluate(() => {
      const watched = document.querySelector("#modal-input");
      window.__MOCK_MODAL_DETACHES__ = 0;
      new MutationObserver(records => {
        for (const record of records) {
          if ([...record.removedNodes].some(node => node === watched || node.contains?.(watched))) {
            window.__MOCK_MODAL_DETACHES__ += 1;
          }
        }
      }).observe(document.querySelector("#modal-body"), { childList: true, subtree: true });
      document.querySelector("#conn-add").click();
    });
    await page.waitForTimeout(30);
    assert.equal(await page.evaluate(() => window.__MOCK_MODAL_DETACHES__), 0);
    assert.equal(await input.evaluate(element => document.activeElement === element), true);
    await owner.getByRole("button", { name: "Abbrechen" }).click();
    const picker = page.getByRole("dialog", { name: "Verbindung hinzufügen" });
    await picker.waitFor();
    await picker.getByRole("button", { name: "Abbrechen" }).click();
  });

  await runCase("Eingereihter Realmodus baut erst in seinem eigenen Lease", "", async page => {
    await page.evaluate(async () => {
      const modal = await import("/modal.js");
      void modal.promptModal("Bestehender Besitzer", "unverändert");
    });
    const owner = page.getByRole("dialog", { name: "Bestehender Besitzer" });
    const input = owner.locator("#modal-input");
    await input.waitFor();
    await page.evaluate(() => {
      const watched = document.querySelector("#modal-input");
      window.__MOCK_MODAL_DETACHES__ = 0;
      new MutationObserver(records => {
        for (const record of records) {
          if ([...record.removedNodes].some(node => node === watched || node.contains?.(watched))) {
            window.__MOCK_MODAL_DETACHES__ += 1;
          }
        }
      }).observe(document.querySelector("#modal-body"), { childList: true, subtree: true });
      window.__MOCK_EMIT__("menu", "sync_mode");
      // Der Sync-Modus fokussiert regulär seine Richtungswahl. Für diesen Test
      // beobachten wir ab hier nur noch den nachfolgenden Realmodus-Auftrag.
      watched.focus();
      document.querySelector("#btn-real").click();
    });
    await page.waitForTimeout(40);
    assert.equal(await page.evaluate(() => window.__MOCK_MODAL_DETACHES__), 0);
    assert.equal(await input.evaluate(element => document.activeElement === element), true);
    await owner.getByRole("button", { name: "Abbrechen" }).click();
    const realMode = page.getByRole("dialog", { name: "Lokalen Realmodus bestätigen" });
    await realMode.waitFor();
    await realMode.getByRole("button", { name: "Vorbereitung verwerfen" }).click();
  });

  await runCase("Modal-Fokusfalle fängt einen nicht mehr bedienbaren aktiven Knoten ab", "", async page => {
    await page.evaluate(async () => {
      const modal = await import("/modal.js");
      void modal.choiceModal("Fokuswechsel", "Drei Ziele", [
        { label: "Erstes Ziel", value: "first", cls: "ghost" },
        { label: "Wird deaktiviert", value: "disabled", cls: "ghost" },
        { label: "Letztes Ziel", value: "last", cls: "primary" },
      ]);
    });
    const dialog = page.getByRole("dialog", { name: "Fokuswechsel" });
    await dialog.waitFor();
    const result = await page.evaluate(async () => {
      const modal = await import("/modal.js");
      const active = [...document.querySelectorAll("#modal button")]
        .find(button => button.textContent === "Wird deaktiviert");
      active.focus();
      active.disabled = true;
      let forwardPrevented = false;
      modal.onModalKeyDown({
        key: "Tab",
        shiftKey: false,
        preventDefault() { forwardPrevented = true; },
        stopPropagation() {},
      });
      const forward = document.activeElement?.textContent;
      active.disabled = false;
      active.focus();
      active.disabled = true;
      let backwardPrevented = false;
      modal.onModalKeyDown({
        key: "Tab",
        shiftKey: true,
        preventDefault() { backwardPrevented = true; },
        stopPropagation() {},
      });
      return {
        forward,
        backward: document.activeElement?.textContent,
        forwardPrevented,
        backwardPrevented,
      };
    });
    assert.deepEqual(result, {
      forward: "Erstes Ziel",
      backward: "Letztes Ziel",
      forwardPrevented: true,
      backwardPrevented: true,
    });
    await dialog.getByRole("button", { name: "Erstes Ziel" }).click();
  });

  await runCase("Alle Verbindungseditoren binden Discovery an den unveränderten Endpunkt", "?editorDiscoveryDelay=80", async page => {
    const cases = [
      ["SFTP", "SFTP-Verbindung hinzufügen", ["#sftp-host", "#sftp-port"], ["erster.mock", "22"], "Hostkey ermitteln"],
      ["FTP / FTPS", "FTP-/FTPS-Verbindung hinzufügen", ["#ftps-host", "#ftps-port"], ["erster.mock", "21"], "Zertifikat ermitteln"],
      ["WebDAV", "HTTPS-WebDAV-Verbindung hinzufügen", ["#webdav-origin"], ["https://erster.mock"], "Zertifikat ermitteln"],
      ["SMB", "Verschlüsselte SMB3-Verbindung hinzufügen", ["#smb-host", "#smb-port"], ["erster.mock", "445"], "Server-GUID ermitteln"],
    ];
    for (const [protocol, title, selectors, values, initialLabel] of cases) {
      await page.locator("#conn-add").click();
      const picker = page.getByRole("dialog", { name: "Verbindung hinzufügen" });
      await picker.getByRole("button", { name: new RegExp(`^${protocol}`) }).click();
      const dialog = page.getByRole("dialog", { name: title });
      for (let index = 0; index < selectors.length; index += 1) {
        await dialog.locator(selectors[index]).fill(values[index]);
      }
      const primary = dialog.locator(".modal-buttons .primary");
      await primary.click();
      for (const selector of selectors) {
        assert.equal(await dialog.locator(selector).isDisabled(), true,
          `${protocol}: Endpunkt muss während Discovery gesperrt sein.`);
      }
      await dialog.locator(selectors[0]).evaluate(element => {
        element.value = element.value.includes("https://") ? "https://zweiter.mock" : "zweiter.mock";
        element.dispatchEvent(new Event("input", { bubbles: true }));
      });
      await page.waitForTimeout(110);
      assert.equal(await primary.innerText(), initialLabel,
        `${protocol}: Antwort des alten Endpunkts darf nicht übernommen werden.`);
      for (const selector of selectors) assert.equal(await dialog.locator(selector).isEnabled(), true);
      await dialog.getByRole("button", { name: "Abbrechen" }).click();
    }
  });

  await runCase("Verspätete Discovery-Antwort verändert keinen neuen Dialog", "?editorDiscoveryDelay=100", async page => {
    await page.locator("#conn-add").click();
    await page.getByRole("dialog", { name: "Verbindung hinzufügen" })
      .getByRole("button", { name: /^SFTP/ }).click();
    const editor = page.getByRole("dialog", { name: "SFTP-Verbindung hinzufügen" });
    await editor.locator("#sftp-host").fill("spaet.mock");
    await editor.getByRole("button", { name: "Hostkey ermitteln" }).click();
    await editor.getByRole("button", { name: "Abbrechen" }).click();
    await page.evaluate(async () => {
      const modal = await import("/modal.js");
      void modal.promptModal("Neuer Besitzer", "bleibt");
    });
    const next = page.getByRole("dialog", { name: "Neuer Besitzer" });
    await next.waitFor();
    await page.waitForTimeout(130);
    assert.equal(await page.locator("#modal-title").innerText(), "Neuer Besitzer");
    assert.equal(await next.locator("#modal-input").inputValue(), "bleibt");
    await next.getByRole("button", { name: "Abbrechen" }).click();
  });

  await runCase("Secret und Profil werden trotz Save-Doppelklick genau einmal gespeichert", "?editorDiscoveryDelay=10&editorSaveDelay=80", async page => {
    await page.locator("#conn-add").click();
    await page.getByRole("dialog", { name: "Verbindung hinzufügen" })
      .getByRole("button", { name: /^SFTP/ }).click();
    const dialog = page.getByRole("dialog", { name: "SFTP-Verbindung hinzufügen" });
    await dialog.locator("#sftp-host").fill("save.mock");
    await dialog.locator("#sftp-user").fill("tester");
    await dialog.locator("#sftp-name").fill("Save-Test");
    await captureNativeSecret(dialog);
    const primary = dialog.locator(".modal-buttons .primary");
    await primary.click();
    await primary.filter({ hasText: /binden und speichern/ }).waitFor();
    await primary.evaluate(button => { button.click(); button.click(); });
    assert.equal(await dialog.getByRole("button", { name: "Abbrechen" }).isDisabled(), true);
    await page.keyboard.press("Escape");
    assert.equal(await dialog.count(), 1, "Irreversible Speicherung darf nicht geschlossen werden.");
    await dialog.waitFor({ state: "detached" });
    const actions = await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__());
    assert.equal(actions.filter(action => action.command === "save_sftp_profile").length, 1);
    const save = actions.find(action => action.command === "save_sftp_profile");
    assert.match(save.secretId, /^mock-pending-secret-/);
    assert.equal(Object.hasOwn(save, "secret"), false,
      "Das serialisierbare IPC-Payload darf keinen rohen Secretwert enthalten.");
    const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
    assert.equal(state.secretTransactions.filter(entry => entry.protocol === "sftp").length, 1);
  });

  await runCase("Editor-Abbruch verwirft seine native Secret-Einmal-ID", "", async page => {
    await page.locator("#conn-add").click();
    await page.getByRole("dialog", { name: "Verbindung hinzufügen" })
      .getByRole("button", { name: /^FTP \/ FTPS/ }).click();
    const dialog = page.getByRole("dialog", { name: "FTP-/FTPS-Verbindung hinzufügen" });
    await captureNativeSecret(dialog);
    assert.equal((await page.evaluate(() => window.__MOCK_CONNECTION_STATE__())).pendingSecretCount, 1);
    await dialog.getByRole("button", { name: "Abbrechen" }).click();
    await page.waitForFunction(() => window.__MOCK_CONNECTION_STATE__().pendingSecretCount === 0);
    const actions = await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__());
    assert.equal(actions.at(-1).command, "discard_connection_secret");
  });

  const protocolEditors = {
    sftp: { picker: /^SFTP/, title: "SFTP-Verbindung" },
    ftps: { picker: /^FTP \/ FTPS/, title: "FTP-/FTPS-Verbindung" },
    webdav: { picker: /^WebDAV/, title: "WebDAV-Verbindung" },
    smb: { picker: /^SMB/, title: "Verschlüsselte SMB3-Verbindung" },
  };

  for (const [protocol, editor] of Object.entries(protocolEditors)) {
    await runCase(
      `${protocol.toUpperCase()}: Profilfehler stellt vorhandenes Secret wieder her`,
      `?profileSaveError=${protocol}`,
      async page => {
        await page.locator(`#connection-list [data-protocol="${protocol}"] .conn-edit`).click();
        const dialog = page.getByRole("dialog", { name: new RegExp(`${editor.title} bearbeiten`) });
        await captureNativeSecret(dialog);
        await dialog.locator(".modal-buttons .primary").click();
        await page.getByText(new RegExp(`Mock-${protocol}-Profildatei`)).waitFor();
        const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
        assert.equal(state[`${protocol}Profiles`].length, 1);
        assert.deepEqual(state.secretTransactions.map(entry => entry.outcome), ["old_unchanged_new_removed"]);
        assert.equal(state.pendingSecretCount, 0);
        assert.equal(
          (await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__()))
            .filter(action => action.command === "discard_connection_secret").length,
          1,
          "Auch ein Backend-seitig bereits verbrauchter Einmalwert wird ausdrücklich verworfen.",
        );
        assert.equal(
          (await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__()))
            .filter(action => action.command.startsWith("set_")).length,
          0,
          "Ein einzelner Secret-Command darf nicht mehr erreichbar sein.",
        );
      },
    );

    await runCase(
      `${protocol.toUpperCase()}: Profilfehler entfernt neu angelegtes Secret`,
      `?profileSaveError=${protocol}`,
      async page => {
        await page.locator("#conn-add").click();
        await page.getByRole("dialog", { name: "Verbindung hinzufügen" })
          .getByRole("button", { name: editor.picker }).click();
        const dialog = page.getByRole("dialog", { name: new RegExp(`${editor.title} hinzufügen`) });
        if (protocol === "sftp") {
          await dialog.locator("#sftp-name").fill("Neu");
          await dialog.locator("#sftp-host").fill("neu.mock");
          await dialog.locator("#sftp-user").fill("tester");
        } else if (protocol === "ftps") {
          await dialog.locator("#ftps-name").fill("Neu");
          await dialog.locator("#ftps-host").fill("neu.mock");
          await dialog.locator("#ftps-user").fill("tester");
        } else if (protocol === "webdav") {
          await dialog.locator("#webdav-name").fill("Neu");
          await dialog.locator("#webdav-origin").fill("https://neu.mock");
          await dialog.locator("#webdav-user").fill("tester");
        } else {
          await dialog.locator("#smb-name").fill("Neu");
          await dialog.locator("#smb-host").fill("neu.mock");
          await dialog.locator("#smb-share").fill("Daten");
          await dialog.locator("#smb-user").fill("tester");
        }
        await captureNativeSecret(dialog);
        const primary = dialog.locator(".modal-buttons .primary");
        await primary.click();
        await primary.filter({ hasText: /(?:binden und speichern|Profil speichern)/ }).waitFor();
        await primary.click();
        await page.getByText(new RegExp(`Mock-${protocol}-Profildatei`)).waitFor();
        const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
        assert.equal(state[`${protocol}Profiles`].length, 1);
        assert.deepEqual(state.secretTransactions.map(entry => entry.outcome), ["removed_new"]);
        assert.equal(state.pendingSecretCount, 0);
        assert.equal(
          (await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__()))
            .filter(action => action.command === "discard_connection_secret").length,
          1,
        );
      },
    );

    await runCase(
      `${protocol.toUpperCase()}: Savefehler vor Secretverbrauch verwirft den Einmalwert sofort`,
      `?profileSaveBeforeConsumeError=${protocol}`,
      async page => {
        await page.locator(`#connection-list [data-protocol="${protocol}"] .conn-edit`).click();
        const dialog = page.getByRole("dialog", { name: new RegExp(`${editor.title} bearbeiten`) });
        await captureNativeSecret(dialog);
        await dialog.locator(".modal-buttons .primary").click();
        await page.getByText(new RegExp(`Mock-${protocol}-Save wurde vor Secretverbrauch abgelehnt`)).waitFor();
        const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
        assert.equal(state.pendingSecretCount, 0);
        const actions = await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__());
        assert.equal(actions.filter(action => action.command === "discard_connection_secret").length, 1);
      },
    );

    await runCase(
      `${protocol.toUpperCase()}: Unklarer Saveausgang schließt den Editor und liest Profile neu`,
      `?profileSaveVisibleError=${protocol}`,
      async page => {
        await page.locator(`#connection-list [data-protocol="${protocol}"] .conn-edit`).click();
        const dialog = page.getByRole("dialog", { name: new RegExp(`${editor.title} bearbeiten`) });
        await captureNativeSecret(dialog);
        await dialog.locator(".modal-buttons .primary").click();
        await page.getByText(new RegExp(`Mock-${protocol}-Profil ist trotz spätem Speicherfehler sichtbar`)).waitFor();
        await dialog.waitFor({ state: "detached" });
        const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
        assert.equal(state.pendingSecretCount, 0);
        assert.deepEqual(state.secretTransactions.map(entry => entry.outcome), ["committed"]);
        const actions = await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__());
        assert.equal(actions.filter(action => action.command === `save_${protocol}_profile`).length, 1,
          "Die verbrauchte Einmal-ID darf nie aus demselben Editor erneut gespeichert werden.");
        assert.equal(actions.filter(action => action.command === "discard_connection_secret").length, 0,
          "Ein unklarer Ausgang hat die Einmal-ID bereits verbraucht; der Editor darf sie nicht verwerfen.");
      },
    );
  }

  await runCase("SFTP-Agentantwort darf ein inzwischen entferntes Profil nicht neu anlegen", "?holdSftpAgent=1&sftpProfileChangesDuringAgent=remove", async page => {
    await page.locator('#connection-list [data-protocol="sftp"] .conn-edit').click();
    const dialog = page.getByRole("dialog", { name: "SFTP-Verbindung bearbeiten" });
    await dialog.waitFor();
    await page.waitForFunction(() => window.__MOCK_REQUEST_PHASES__().holdSftpAgent?.phase === "pending");
    assert.equal((await page.evaluate(() => window.__MOCK_CONNECTION_STATE__())).sftpProfiles[0].revision, 11);
    await page.evaluate(() => window.__MOCK_RELEASE_REQUEST__("holdSftpAgent"));
    await page.getByText(/Profil wurde inzwischen.*entfernt/s).waitFor();
    assert.equal(await dialog.count(), 0);
    assert.deepEqual((await page.evaluate(() => window.__MOCK_CONNECTION_STATE__())).sftpProfiles, []);
    assert.deepEqual(await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__()), []);
  });

  await runCase("Veralteter Profileditor überschreibt keinen neueren Stand", "", async page => {
    await page.locator('#connection-list [data-protocol="ftps"] .conn-edit').click();
    const dialog = page.getByRole("dialog", { name: /FTP-\/FTPS-Verbindung bearbeiten/ });
    await captureNativeSecret(dialog);
    await page.evaluate(() => window.__MOCK_SET_PROFILE_REVISION__("ftps", "mock_ftps", 22));
    await dialog.locator(".modal-buttons .primary").click();
    await page.getByText(/anderen Fenster geändert/).waitFor();
    await dialog.waitFor();

    const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
    assert.equal(state.ftpsProfiles[0].revision, 22);
    assert.equal(state.ftpsProfiles[0].name, "Mock-FTPS");
    assert.equal(state.pendingSecretCount, 0);
    assert.deepEqual(state.secretTransactions.map(entry => entry.outcome), [
      "stale_save_new_removed",
    ]);
    const actions = await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__());
    const save = actions.find(action => action.command === "save_ftps_profile");
    assert.equal(save.profileRevision, 21);
  });

  await runCase("Veraltetes Entfernen löscht keinen neueren Profilstand", "", async page => {
    await page.locator('#connection-list [data-protocol="sftp"] [data-remove="mock_sftp"]').click();
    const dialog = page.getByRole("dialog", { name: /SFTP-Profil entfernen/ });
    await dialog.waitFor();
    await page.evaluate(() => window.__MOCK_SET_PROFILE_REVISION__("sftp", "mock_sftp", 12));
    await dialog.getByRole("button", { name: "Fortfahren" }).click();
    await page.getByText(/aktuelle Profilrevision/).waitFor();
    const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
    assert.equal(state.sftpProfiles[0].revision, 12);
    assert.equal(state.sftpProfiles[0].name, "Mock-SFTP");
  });

  await runCase("Vorbereitetes Update bleibt während einer fremden Operation erhalten", "?update=available", async page => {
    await page.evaluate(() => { void window.__MOCK_EMIT__("menu", "check_updates"); });
    const update = page.getByRole("dialog", { name: "Update 0.1.124 verfügbar" });
    await update.waitFor();
    await page.evaluate(() => {
      window.__MOCK_EMIT__("queue-updated", {
        version: 8,
        paused: false,
        stop_on_error: false,
        entries: [{
          operation_id: "foreign-running",
          operation: { type: "File", kind: "Copy", items: ["/mock/fremd.txt"], destination: "/mock/ziel" },
          state: "Running", attempts: 1,
          progress: { files_total: 1, files_done: 0, bytes_total: 8, bytes_done: 0 },
        }],
      });
    });
    await update.getByRole("button", { name: "Herunterladen und installieren" }).click();
    await page.getByText(/Update .*vorgemerkt|Operation.*zuerst/).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_UPDATES__()), [
      { command: "check_for_update" },
    ], "Während der fremden Operation darf die Installation noch nicht starten.");

    await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", {
      files: 1, skipped: 0, symlinks_skipped: 0, bytes: 8, cancelled: false,
    }));
    await page.evaluate(() => { void window.__MOCK_EMIT__("menu", "check_updates"); });
    await update.waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_UPDATES__()), [
      { command: "check_for_update" },
    ], "Der vorbereitete Release wird ohne zweiten HTTPS-Abruf erneut angeboten.");
    await update.getByRole("button", { name: "Später" }).click();
  });

  await runCase("Update-Freigabe löscht keinen späteren fremden Laufzustand", "?update=available&installDelay=80&foreignRunningDuringInstall=1", async page => {
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await page.evaluate(() => { void window.__MOCK_EMIT__("menu", "check_updates"); });
    const update = page.getByRole("dialog", { name: "Update 0.1.124 verfügbar" });
    await update.getByRole("button", { name: "Herunterladen und installieren" }).click();
    await page.getByText(/Update installiert/).waitFor();
    await list.focus();
    await page.keyboard.press("F2");
    assert.equal(await page.locator("#modal-input").count(), 0,
      "Der fremde Running-Zustand muss nach Ende der Update-Aktivität weiter sperren.");
  });

  await runCase("Tastatur, Type-ahead und Kontextmenü", "?keyboard=1", async page => {
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.focus();
    await page.keyboard.press("b");
    assert.match(await page.locator('[role="option"][aria-selected="true"]').innerText(), /Bilder/);
    await page.keyboard.press("b");
    assert.match(await page.locator('[role="option"][aria-selected="true"]').innerText(), /Beta\.txt/);
    await page.keyboard.press("Shift+F10");
    const menu = page.getByRole("menu", { name: "Dateiaktionen" });
    assert.equal(await menu.count(), 1);
    await page.keyboard.press("End");
    assert.equal(await page.evaluate(() => document.activeElement?.textContent?.trim()),
      "Darstellungsoptionen …  (⌘J)");
    await page.keyboard.press("Escape");
    assert.equal(await menu.count(), 0);
    assert.equal(await page.evaluate(() => document.activeElement?.getAttribute("role")), "listbox");

    const pathInput = page.locator('.pane[data-side="left"] input.path');
    await pathInput.focus();
    await page.keyboard.press("Control+I");
    const information = page.getByRole("dialog", { name: /Informationen · Beta\.txt/ });
    await information.waitFor();
    await information.getByRole("button", { name: "Schließen" }).click();
    assert.equal(await pathInput.evaluate(element => element === document.activeElement), true);

    const viewToggle = page.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" });
    await viewToggle.focus();
    await page.keyboard.press("Control+J");
    await page.locator('.pane[data-side="left"]')
      .getByRole("dialog", { name: "Darstellung" }).waitFor();
    await viewToggle.click();

    await list.focus();
    await page.keyboard.press("Shift+F10");
    await page.getByRole("menu", { name: "Dateiaktionen" })
      .getByRole("menuitem", { name: /Darstellungsoptionen/ }).click();
    await page.locator('.pane[data-side="left"]')
      .getByRole("dialog", { name: "Darstellung" }).waitFor();
  });

  await runCase("Leerflächen-Kontextmenü verwendet keine unmarkierte Cursor-Datei", "?keyboard=1", async page => {
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    const file = list.getByRole("option", { name: /Alpha\.txt/ });
    await file.click();
    await list.click({ button: "right", position: { x: 12, y: 300 } });
    const menu = page.getByRole("menu", { name: "Dateiaktionen" });
    assert.equal(await list.locator('[aria-selected="true"]').count(), 0);
    assert.deepEqual(await menu.getByRole("menuitem").allTextContents(), [
      "Neuer Ordner  (⌘⇧N)", "Dateien einfügen", ...(await page.evaluate(() => /Mac|iPhone|iPad/.test(navigator.platform)) ? ["Dateien hierher bewegen  (⌥⌘V)"] : []), "Darstellungsoptionen …  (⌘J)",
    ]);
    await page.keyboard.press("Escape");
    await file.click({ button: "right" });
    await menu.getByRole("menuitem", { name: /In den Papierkorb/ }).waitFor();
    assert.equal(await file.getAttribute("aria-selected"), "true");
    await page.keyboard.press("Escape");
    await list.focus();
    await page.keyboard.press("Shift+F10");
    await menu.getByRole("menuitem", { name: /Umbenennen/ }).waitFor();
  });

  await runCase("Teilwirkungsfehler aktualisieren beide sichtbaren Dateilisten", "?keyboard=1", async page => {
    for (const event of ["fileop-error", "sync-error", "bidirectional-error"]) {
      const before = await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length);
      await page.evaluate(name => window.__MOCK_EMIT__(name, {
        code: "outcome_unclear", message: `Teilwirkung ${name}: erste Datei übertragen, zweite fehlgeschlagen`,
        recovery_paths: ["/mock/ziel/Alpha.txt"],
      }), event);
      await page.waitForFunction(count => window.__MOCK_SCAN_CALLS__().length >= count + 2, before,
        { timeout: 3000 });
      const scans = await page.evaluate(count => window.__MOCK_SCAN_CALLS__().slice(count), before);
      assert.deepEqual(scans.map(scan => scan.path).sort(), ["/mock/quelle", "/mock/ziel"]);
      assert.match(await page.locator("#dock-summary").innerText(), new RegExp(`Teilwirkung ${event}`));
    }
  });

  await runCase("Nicht-UTF-8-Namen bleiben sichtbar und für Dateiaktionen gesperrt", "?unsafe=1", async page => {
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    const rows = list.locator('[role="option"]');
    assert.equal(await rows.count(), 2, "Anzeige-Kollision muss beide Einträge behalten.");
    const unsafeRow = rows.filter({ has: page.locator(".unsafe-name-badge") });
    assert.equal(await unsafeRow.count(), 1);
    assert.match(await unsafeRow.getAttribute("title"), /nicht eindeutig/);
    assert.equal(await unsafeRow.getAttribute("draggable"), null);

    await unsafeRow.click();
    await list.focus();
    await page.keyboard.press("Enter");
    await page.getByText(/Unsicherer Dateiname/).waitFor();
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_LAST_OPENED__ || null), null);

    await page.keyboard.press("F5");
    assert.equal(await page.locator("#queue-count").innerText(), "0");
    await page.keyboard.press("Shift+F10");
    const menu = page.getByRole("menu", { name: "Dateiaktionen" });
    assert.deepEqual(await menu.getByRole("menuitem").allTextContents(), [
      "Informationen  (⌘I)",
      "Neuer Ordner  (⌘⇧N)",
      "Dateien einfügen", ...(await page.evaluate(() => /Mac|iPhone|iPad/.test(navigator.platform)) ? ["Dateien hierher bewegen  (⌥⌘V)"] : []),
      "Darstellungsoptionen …  (⌘J)",
    ]);
    await menu.getByRole("menuitem", { name: /Informationen/ }).click();
    const information = page.getByRole("dialog", { name: /Informationen · �\.txt/ });
    await information.waitFor();
    assert.match(await information.innerText(), /Nicht eindeutig identifizierbarer Eintrag in \/mock\/quelle/);
    assert.doesNotMatch(await information.innerText(), /\/mock\/quelle\/�\.txt/);
    await information.getByRole("button", { name: "Schließen" }).click();
    assert.equal(await page.evaluate(() => document.activeElement?.getAttribute("role")), "listbox");

    const safeRow = rows.filter({ hasNot: page.locator(".unsafe-name-badge") });
    await safeRow.click();
    await list.focus();
    await page.keyboard.press("Enter");
    await page.waitForFunction(() => window.__DOPPELDECKER_LAST_OPENED__ === "/mock/quelle/�.txt");

    // Beide Einträge sehen gleich aus, sind aber verschiedene Zeilen. Eine
    // Spaltensortierung muss die sichere Einzelauswahl samt Cursor erhalten.
    await page.locator('.pane[data-side="left"] .col[data-sort="size"]').click();
    const selectedAfterSort = list.locator('[role="option"][aria-selected="true"]');
    assert.equal(await selectedAfterSort.count(), 1);
    assert.equal(await selectedAfterSort.locator(".unsafe-name-badge").count(), 0);
    const cursorAfterSort = list.locator(".row.cursor");
    assert.equal(await cursorAfterSort.count(), 1);
    assert.equal(await cursorAfterSort.locator(".unsafe-name-badge").count(), 0);

    // Regression: F2 wirkt auf den CURSOR-Eintrag. Der Cursor wird per
    // Meta-Klick auf den unsicheren Eintrag gestellt und dieser sofort wieder
    // abgewählt — die Auswahl enthält dann nur den sicheren Eintrag. Das
    // Umbenennen muss trotzdem gesperrt bleiben (kein Umbenennen-Dialog).
    await unsafeRow.click({ modifiers: ["Meta"] });
    await unsafeRow.click({ modifiers: ["Meta"] });
    await list.focus();
    await page.keyboard.press("F2");
    await page.getByText(/Umbenennen ist für Namen ohne eindeutige UTF-8-Abbildung gesperrt/).waitFor();
    assert.equal(await page.locator("#modal-input").count(), 0);
  });

  await runCase("Schlanker Fensterkopf: Version und Queue neben sicherer Drag-Fläche", "", async page => {
    // macOS-Realität erzwingen: Im Overlay sitzen die Ampelknöpfe über der
    // Seitenleiste, deshalb reserviert der Kopf oben nur ihren Platz.
    await page.evaluate(() => { document.documentElement.dataset.platform = "macos"; });
    // Kein Programmname und kein Logo mehr — die Version übernimmt den Ampelplatz.
    assert.equal(await page.locator(".side-brand .logo").count(), 0);
    assert.equal(await page.locator(".side-brand .brand-name").count(), 0);
    await page.waitForFunction(() => /^v\d/.test(document.querySelector("#side-version")?.textContent || ""));
    // Tauri startet das Ziehen nur, wenn das direkt getroffene Element selbst
    // data-tauri-drag-region trägt. Nur der echte Queue-Knopf ist ausgenommen.
    const uncovered = await page.evaluate(() => {
      const brand = document.querySelector(".side-brand");
      const rect = brand.getBoundingClientRect();
      const problems = [];
      for (let y = Math.ceil(rect.top) + 2; y < rect.bottom - 2; y += 5) {
        for (let x = Math.ceil(rect.left) + 2; x < rect.right - 2; x += 6) {
          const hit = document.elementFromPoint(x, y);
          if (!hit || !brand.contains(hit)) continue;
          if (hit.closest("#btn-queue")) continue;
          if (hit.hasAttribute("data-tauri-drag-region")) continue;
          problems.push(`<${hit.tagName.toLowerCase()} class="${hit.className}">`);
        }
      }
      // Kompakt melden: jede problematische Element-Klasse nur einmal.
      return [...new Set(problems)];
    });
    assert.deepEqual(uncovered, [], "Nicht ziehbare Punkte im Fensterkopf gefunden.");
    assert.equal(await page.locator("#btn-queue").getAttribute("aria-expanded"), "false");
    await page.locator("#btn-queue").click();
    await page.getByRole("dialog", { name: "Operations-Queue" }).waitFor();
    assert.equal(await page.locator("#btn-queue").getAttribute("aria-expanded"), "true");
    await page.locator("#queue-popover-close").click();
  });

  await runCase("Wartender Auftrag führt aus der Fehlermeldung in die Queue", "", async page => {
    // Der Fall aus dem GUI-Test vom 2026-09-04: Das Ersetzen wartet auf eine
    // Bestätigung, und die Queue hängt sonst nur an einem kleinen Symbol im
    // Seitenleistenkopf.
    await page.evaluate(() => {
      window.__MOCK_EMIT__("fileop-error", {
        code: "queue_confirmation_required",
        message: "Nichts überschrieben: Dieses Dateisystem kann beim Ersetzen die vorhandene "
          + "Fassung nicht sicher aufbewahren. Der Auftrag wartet in der Queue auf deine Bestätigung.",
      });
    });
    const meldung = page.locator("#dock-summary");
    await meldung.getByRole("button", { name: "Queue öffnen" }).waitFor();
    assert.match(await meldung.innerText(), /Nichts überschrieben/,
      "Die Meldung nennt zuerst die Wirkung.");
    // Eine zweite Meldung rendert das Dock neu; der Knopf muss danach weiter
    // führen und nicht bloß so aussehen.
    await page.evaluate(() => {
      window.__MOCK_EMIT__("fileop-error", {
        code: "queue_confirmation_required",
        message: "Nichts überschrieben: zweite Meldung. Der Auftrag wartet in der Queue.",
      });
    });
    await meldung.getByText(/zweite Meldung/).waitFor();
    assert.equal(await page.locator("#btn-queue").getAttribute("aria-expanded"), "false");
    await meldung.getByRole("button", { name: "Queue öffnen" }).click();
    await page.getByRole("dialog", { name: "Operations-Queue" }).waitFor();
    assert.equal(await page.locator("#btn-queue").getAttribute("aria-expanded"), "true");
    await page.locator("#queue-popover-close").click();
  });

  await runCase("Gewöhnlicher Fehler bekommt keinen Queue-Knopf", "", async page => {
    await page.evaluate(() => {
      window.__MOCK_EMIT__("fileop-error", { code: "io", message: "Platte weg" });
    });
    await page.getByText(/Platte weg/).waitFor();
    assert.equal(
      await page.locator("#dock-summary [data-dock-action=\"open-queue\"]").count(),
      0,
      "Nur ein wartender Auftrag führt in die Queue.",
    );
  });

  await runCase("Pane-Köpfe haben freie Fenster-Drag-Flächen ohne Rollen", "", async page => {
    const result = await page.evaluate(() => {
      const panes = [...document.querySelectorAll(".pane")];
      return panes.map((pane) => {
        const zone = pane.querySelector(".pane-drag-region");
        const rect = zone?.getBoundingClientRect();
        const hit = rect && document.elementFromPoint(
          rect.left + rect.width / 2,
          rect.top + rect.height / 2,
        );
        const controls = [...pane.querySelectorAll(".pane-head button, .pane-head .path")];
        return {
          side: pane.dataset.side,
          empty: zone?.textContent.trim() === "" && zone?.childElementCount === 0,
          directlyMarked: zone?.hasAttribute("data-tauri-drag-region") === true,
          freeOfLabel: zone?.getAttribute("aria-label") === null,
          hitIsZone: hit === zone,
          width: Math.round(rect?.width ?? 0),
          height: Math.round(rect?.height ?? 0),
          controlsRemainClickable: controls.every((control) => {
            const controlRect = control.getBoundingClientRect();
            const controlHit = document.elementFromPoint(
              controlRect.left + controlRect.width / 2,
              controlRect.top + controlRect.height / 2,
            );
            return !control.hasAttribute("data-tauri-drag-region")
              && (controlHit === control || control.contains(controlHit));
          }),
          listLabel: pane.querySelector(".listing")?.getAttribute("aria-label"),
        };
      });
    });
    // Tauri wertet nur das direkt getroffene Element aus. Der mittige Treffertest
    // verhindert deshalb, dass später ein überlagerndes Kind die Ziehfläche bricht.
    assert.deepEqual(result.map(({ side }) => side), ["left", "right"]);
    assert.ok(result.every(zone => zone.empty && zone.freeOfLabel && zone.directlyMarked && zone.hitIsZone));
    assert.ok(result.every(zone => zone.width >= 64 && zone.height >= 28), "Eine Pane-Ziehfläche ist zu klein.");
    assert.ok(result.every(zone => zone.controlsRemainClickable), "Ein Pane-Bedienelement wird von der Ziehfläche überdeckt.");
    assert.deepEqual(result.map(zone => zone.listLabel), ["Linke Dateiliste", "Rechte Dateiliste"]);
    assert.equal(await page.locator(".role-badge").count(), 0);

    // Die Pfadeingaben bleiben echte Eingabefelder und dürfen nicht vom Ziehen
    // abgefangen werden. Ein Klick muss deshalb jeweils den Fokus setzen.
    for (const side of ["left", "right"]) {
      const path = page.locator(`.pane[data-side="${side}"] .path`);
      await path.click();
      assert.equal(await path.evaluate(input => document.activeElement === input), true);
    }
  });

  await runCase("AltGr und IME in der Pfadleiste lösen keine globalen I/J-Kürzel aus", "", async page => {
    const path = page.locator('.pane[data-side="left"] .path');
    await path.focus();
    const prevented = await path.evaluate(input => {
      const altGr = new KeyboardEvent("keydown", {
        key: "i", code: "KeyI", ctrlKey: true, altKey: true,
        bubbles: true, cancelable: true,
      });
      input.dispatchEvent(altGr);
      const composing = new KeyboardEvent("keydown", {
        key: "j", code: "KeyJ", ctrlKey: true, isComposing: true,
        bubbles: true, cancelable: true,
      });
      input.dispatchEvent(composing);
      return [altGr.defaultPrevented, composing.defaultPrevented];
    });
    assert.deepEqual(prevented, [false, false]);
    assert.equal(await page.locator("#modal").isHidden(), true);

    // Das absichtlich globale exakte Kürzel bleibt auch mit fokussierter
    // Pfadleiste erreichbar.
    await path.press("Control+I");
    const information = page.getByRole("dialog", { name: /Informationen/ });
    await information.waitFor();
    await information.getByRole("button", { name: "Schließen" }).click();
  });

  await runCase("Mount-Abfragefehler erhält Freigabe, Fehlergrund und gesunde Volumes", "?mountSpaceError=1", async page => {
    const network = page.locator("#share-list .device-item").filter({ hasText: "Team" });
    await network.waitFor();
    assert.match(await network.innerText(), /Speicher nicht abrufbar/);
    assert.equal(await network.locator(".device-space").evaluate(element => element.scrollWidth <= element.clientWidth), true);
    assert.match(await network.getAttribute("title"), /Zeitlimit erreicht <img/);
    assert.equal(await network.locator("img").count(), 0);
    assert.equal(await network.locator(".device-eject").count(), 1);
    const system = page.locator("#device-list .device-item").filter({ hasText: "Systemvolume" });
    assert.match(await system.innerText(), /frei von/);
    assert.equal(await page.locator("#device-list .device-item").count(), 2);
  });

  await runCase("Geräte zeigen freien Speicher, navigieren und werfen gebunden aus", "", async page => {
    const devices = page.locator("#device-list .device-item");
    const shares = page.locator("#share-list .device-item");
    await devices.first().waitFor();
    await shares.first().waitFor();
    assert.equal(await devices.count(), 2);
    assert.equal(await shares.count(), 1);
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "units_decimal"));

    const system = devices.filter({ hasText: "Systemvolume" });
    const external = devices.filter({ hasText: "Backup" });
    const network = shares.filter({ hasText: "Team" });
    assert.match(await system.innerText(), /400,00 GB frei von 1,00 TB/);
    assert.match(await external.innerText(), /500,00 MB frei von 1,00 GB/);
    assert.equal(await system.locator(".device-eject").count(), 0);
    assert.equal(await external.locator(".device-eject").count(), 1);
    assert.equal(await network.locator(".device-eject").count(), 1);

    // Ein Klick navigiert die aktive Pane. Das Auswerfen übergibt danach nur die
    // opaque ID und setzt genau diese Pane auf ihren lokalen Standard zurück.
    await external.locator(".device-name").click();
    const leftPath = page.locator('.pane[data-side="left"] .path');
    await page.waitForFunction(() => document.querySelector('.pane[data-side="left"] .path')?.value === "/Volumes/Backup");
    await external.getByRole("button", { name: "Backup auswerfen" }).click();
    await page.getByText("Ausgeworfen", { exact: true }).waitFor();
    await page.waitForFunction(() => document.querySelector('.pane[data-side="left"] .path')?.value === "/mock/quelle");
    assert.equal(await page.locator("#device-list .device-item", { hasText: "Backup" }).count(), 0);
    assert.equal(await devices.count(), 1);
    assert.equal(await shares.count(), 1);
    assert.equal(await leftPath.inputValue(), "/mock/quelle");
    const state = await page.evaluate(() => window.__MOCK_MOUNTS__());
    assert.deepEqual(state.ejects, ["b".repeat(64)]);
  });

  await runCase("Ausgeworfenes Volume verliert seine alte Pane auch bei Defaultfehler", "?defaultErrorAfterStartup", async page => {
    // Der Mount hat im Mock keinen Bestand. Eine künstliche alte Zeile macht
    // sichtbar, ob der bestätigte Unmount die gesamte Auswahl entwertet.
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await original(command, args);
        if (command === "scan_dir" && args.path === "/Volumes/Backup") {
          result.entries = [{ name: "Alt.txt", is_file: true, is_dir: false,
            is_symlink: false, size: 5 }];
        }
        return result;
      };
    });
    const external = page.locator("#device-list .device-item").filter({ hasText: "Backup" });
    await external.locator(".device-name").click();
    const pane = page.locator('.pane[data-side="left"]');
    await pane.getByRole("option", { name: /Alt\.txt/ }).waitFor();
    await external.getByRole("button", { name: "Backup auswerfen" }).click();
    await page.getByText("Backup ausgeworfen, Pane-Rückfall fehlgeschlagen", { exact: true }).waitFor();
    assert.equal(await pane.locator(".path").inputValue(), "");
    assert.equal(await pane.getByRole("option").count(), 0);
  });

  await runCase("Verspäteter Auswurf-Rückfall erhält eine neuere Pane-Navigation", "", async page => {
    const external = page.locator("#device-list .device-item").filter({ hasText: "Backup" });
    await external.locator(".device-name").click();
    const path = page.locator('.pane[data-side="left"] .path');
    await page.waitForFunction(() => document.querySelector('.pane[data-side="left"] .path').value === "/Volumes/Backup");
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "default_dirs") {
          window.__DEVICE_DEFAULT_PENDING__ = true;
          await new Promise(resolve => { window.__RESOLVE_DEVICE_DEFAULT__ = resolve; });
        }
        return original(command, args);
      };
    });
    await external.getByRole("button", { name: "Backup auswerfen" }).click();
    await page.waitForFunction(() => window.__DEVICE_DEFAULT_PENDING__);
    assert.equal(await path.inputValue(), "");
    await path.fill("/mock/ziel");
    await path.press("Enter");
    await page.waitForFunction(() => document.querySelector('.pane[data-side="left"] .path').value === "/mock/ziel");
    await page.evaluate(() => window.__RESOLVE_DEVICE_DEFAULT__());
    await page.getByText("Ausgeworfen", { exact: true }).waitFor();
    assert.equal(await path.inputValue(), "/mock/ziel");
  });

  await runCase("Splitter ändert die Seitenleistenbreite und merkt sie sich", "", async page => {
    const width = () => page.evaluate(() =>
      Math.round(document.querySelector(".sidebar").getBoundingClientRect().width));
    const start = await width();
    const splitter = page.locator("#splitter");
    // Tastaturweg (deckt zugleich die separator-A11y ab).
    await splitter.focus();
    await page.keyboard.press("ArrowRight");
    assert.equal(await width(), start + 10);
    // Mausweg: Ziehen um +40px.
    const box = await splitter.boundingBox();
    await page.mouse.move(box.x + box.width / 2, box.y + 200);
    await page.mouse.down();
    await page.mouse.move(box.x + box.width / 2 + 40, box.y + 200);
    await page.mouse.up();
    assert.equal(await width(), start + 50);
    assert.equal(await page.evaluate(() => localStorage.getItem("doppeldecker-sidebar-width")), String(start + 50));
    // Die gemerkte Breite übersteht einen Neustart der Oberfläche.
    await page.reload({ waitUntil: "networkidle" });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).waitFor();
    assert.equal(await width(), start + 50);
    // Doppelklick stellt den Standard wieder her.
    await page.locator("#splitter").dblclick();
    assert.equal(await width(), 190);
  });

  await runCase("Größenspalte: festes Format und Anpassen an den Inhalt", "?keyboard=1", async page => {
    const rows = page.getByRole("listbox", { name: /Linke Dateiliste/ }).locator('[role="option"]');
    // Ordner zeigen bewusst keine Größe; Bytes bleiben ganzzahlig.
    assert.equal(await rows.nth(0).locator(".r-size").innerText(), "");
    assert.equal(await rows.nth(1).locator(".r-size").innerText(), "5 Byte");
    // Das Format wechselt die Einheit, bevor vier Vorkommastellen entstehen. Die
    // Basis ist hier festgelegt, damit der Fall auf beiden Plattformen gleich prüft.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "units_decimal"));
    const formatted = await page.evaluate(async () => {
      const module = await import("./ui-utils.js");
      return [999, 1000, 999990, 999999, 1073741824].map(value => module.fmtEntrySize(value));
    });
    // 999990 zeigt die breiteste Form; 999999 würde auf „1000,00 KB" aufrunden und
    // steigt deshalb eine Einheit auf — vier Vorkommastellen entstehen nie.
    assert.deepEqual(formatted, ["999 Byte", "1,00 KB", "999,99 KB", "1,00 MB", "1,07 GB"]);
    // Doppelklick auf den Trenner passt die Spalte einmalig an den Inhalt an.
    const width = () => page.evaluate(() => Math.round(
      document.querySelector('.pane[data-side="left"] .col-size').getBoundingClientRect().width));
    const handle = page.locator('.pane[data-side="left"] .col-resize[data-col="size"]');
    await handle.dblclick();
    const fitted = await width();
    assert.ok(fitted < 60, `Angepasste Größenspalte ist mit ${fitted}px zu breit für „5 Byte".`);
    assert.equal(await page.evaluate(() => localStorage.getItem("doppeldecker-col-left-size")), String(fitted));
  });

  await runCase("Größeneinheit ist umschaltbar, plattformrichtig und persistent", "?keyboard=1", async page => {
    const sizes = () => page.evaluate(async () => {
      const module = await import("./ui-utils.js");
      return {
        eintrag: [999, 1048576, 1500000].map(value => module.fmtEntrySize(value)),
        bilanz: module.fmtTransferBytes(1048576),
        spalte: getComputedStyle(document.querySelector('.pane[data-side="left"]'))
          .getPropertyValue("--col-size-w").trim(),
      };
    });
    // Ohne gemerkte Wahl gilt die Plattformvorgabe: macOS dezimal, Linux binär.
    const platform = await page.evaluate(() => document.documentElement.dataset.platform);
    const start = await sizes();
    assert.deepEqual(
      start.eintrag,
      platform === "linux"
        ? ["999 Byte", "1,00 MiB", "1,43 MiB"]
        : ["999 Byte", "1,05 MB", "1,50 MB"],
      `Plattformvorgabe für ${platform} stimmt nicht.`,
    );
    // Linux startet bereits binär. Für einen echten Wechsel dort zunächst auf
    // dezimal gehen; sonst würde der folgende Menüevent denselben Zustand erneut
    // setzen und die Breitenreaktion gar nicht prüfen.
    let decimal = start;
    if (platform === "linux") {
      await page.evaluate(() => window.__MOCK_EMIT__("menu", "units_decimal"));
      decimal = await sizes();
      assert.deepEqual(decimal.eintrag, ["999 Byte", "1,05 MB", "1,50 MB"]);
    }
    // Umschalten wirkt auf Panes und Bilanzen gleichzeitig — nie auf zwei Basen.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "units_binary"));
    const binary = await sizes();
    assert.deepEqual(binary.eintrag, ["999 Byte", "1,00 MiB", "1,43 MiB"]);
    assert.equal(binary.bilanz, "1,0 MiB");
    assert.equal(await page.locator('.pane[data-side="left"] .row').nth(1).locator(".r-size").innerText(), "5 Byte");
    // Die Standardbreite folgt dem längeren Kürzel „999,99 MiB".
    assert.ok(
      parseInt(binary.spalte, 10) > parseInt(decimal.spalte, 10),
      `Größenspalte wurde für MiB nicht breiter: ${decimal.spalte} → ${binary.spalte}.`,
    );
    // Die Wahl übersteht einen Neustart der Oberfläche.
    await page.reload({ waitUntil: "networkidle" });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).waitFor();
    const reloaded = await sizes();
    assert.deepEqual(reloaded.eintrag, binary.eintrag);
    assert.equal(reloaded.spalte, binary.spalte);
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "units_decimal"));
    assert.deepEqual((await sizes()).eintrag, ["999 Byte", "1,05 MB", "1,50 MB"]);
  });

  await runCase("Datumsspalte wechselt live zwischen den vier Formatstufen", "?keyboard=1", async page => {
    const cell = page.locator('.pane[data-side="left"] .row').first().locator(".r-date");
    const handle = page.locator('.pane[data-side="left"] .col-resize[data-col="date"]');
    // Startbreite zeigt Stufe 3 (vierstelliges Jahr mit Uhrzeit).
    assert.match(await cell.innerText(), /^\d{2}\.\d{2}\.\d{4}, \d{2}:\d{2}$/);
    // Tastaturweg (deckt zugleich die separator-A11y ab): schmaler bis Stufe 1.
    await handle.focus();
    for (let step = 0; step < 8; step += 1) await page.keyboard.press("ArrowRight");
    assert.equal(await handle.getAttribute("aria-valuenow"), "40");
    assert.match(await cell.innerText(), /^\d{2}\.\d{2}\.\d{2}$/);
    // Mausweg: den Trenner nach links ziehen macht die Spalte breiter. Das Ziel liegt
    // mittig zwischen den gemessenen Stufenbreiten, damit der Test nicht von den
    // Schriftmaßen einer bestimmten Plattform abhängt.
    const stageWidths = await page.evaluate(async () => {
      const module = await import("./column-layout.js");
      return module.DATE_STAGE_WIDTHS();
    });
    const target = Math.round((stageWidths[1] + stageWidths[2]) / 2);
    const box = await handle.boundingBox();
    const startX = box.x + box.width / 2;
    const centerY = box.y + box.height / 2;
    await page.mouse.move(startX, centerY);
    await page.mouse.down();
    await page.mouse.move(startX - (target - 40), centerY);
    await page.mouse.up();
    assert.equal(await handle.getAttribute("aria-valuenow"), String(target));
    assert.match(await cell.innerText(), /^\d{2}\.\d{2}\.\d{2}, \d{2}:\d{2}$/);
    // Doppelklick passt an den Inhalt an — das ist die ausführlichste Stufe.
    await handle.dblclick();
    assert.match(await cell.innerText(), /^\d{1,2}\. \p{L}+ \d{4} um \d{2}:\d{2}$/u);
    // Die Breite gilt nur für diese Pane und übersteht einen Neustart.
    const stored = await page.evaluate(() => localStorage.getItem("doppeldecker-col-left-date"));
    assert.ok(Number(stored) > 100, `Gemerkte Datumsbreite ist unerwartet: ${stored}`);
    assert.equal(await page.evaluate(() => localStorage.getItem("doppeldecker-col-right-date")), null);
    await page.reload({ waitUntil: "networkidle" });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).waitFor();
    assert.match(await cell.innerText(), /^\d{1,2}\. \p{L}+ \d{4} um \d{2}:\d{2}$/u);
    // Die rechte Pane wurde nie angefasst und steht deshalb weiterhin auf ihrer
    // Standardbreite — gerade breit genug für Stufe 3, aber nicht für Stufe 4.
    const right = Number(await page.locator('.pane[data-side="right"] .col-resize[data-col="date"]')
      .getAttribute("aria-valuenow"));
    assert.ok(
      right >= stageWidths[2] && right < stageWidths[3],
      `Rechte Pane hat ihre Standardbreite verloren: ${right}px.`,
    );
  });

  await runCase("Darstellungsoptionen gelten je Pane und Ordner und können Standard werden", "?keyboard=1", async page => {
    const leftPane = page.locator('.pane[data-side="left"]');
    const rightPane = page.locator('.pane[data-side="right"]');
    const leftList = leftPane.getByRole("listbox");
    const rightList = rightPane.getByRole("listbox");
    const toggle = leftPane.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" });
    await toggle.click();
    const panel = leftPane.getByRole("dialog", { name: "Darstellung" });
    await panel.getByText("Dieser Ordner", { exact: true }).waitFor();
    assert.equal(await panel.getByLabel("Größe", { exact: true }).isChecked(), true);
    assert.equal(await panel.getByLabel("Änderungsdatum", { exact: true }).isChecked(), true);
    assert.equal(await panel.getByLabel("Art", { exact: true }).isChecked(), false);

    // Neue Spalten erscheinen ausschließlich in der bearbeiteten Pane. Fehlende
    // Providerwerte bleiben dabei als leere Zelle sichtbar.
    for (const label of ["Art", "Erstellungsdatum", "Eigentümer", "Gruppe", "Berechtigungen"]) {
      await panel.getByLabel(label, { exact: true }).check();
    }
    assert.equal(await leftPane.locator(".col-kind").isVisible(), true);
    assert.equal(await rightPane.locator(".col-kind").isVisible(), false);
    const alpha = leftPane.locator(".row", { hasText: "Alpha.txt" });
    assert.equal(await alpha.locator(".r-kind").innerText(), "TXT-Datei");
    assert.match(await alpha.locator(".r-created").innerText(), /^01\.01\.(?:70|1970)/);
    assert.equal(await alpha.locator(".r-owner").innerText(), "alice");
    assert.equal(await alpha.locator(".r-group").innerText(), "staff");
    assert.equal(await alpha.locator(".r-permissions").innerText(), "rw-r--r--");
    const gamma = leftPane.locator(".row", { hasText: "Gamma" });
    assert.equal(await gamma.locator(".r-created").innerText(), "");
    assert.equal(await gamma.locator(".r-owner").innerText(), "");
    assert.equal(await gamma.locator(".r-group").innerText(), "");
    assert.equal(await gamma.locator(".r-permissions").innerText(), "");

    await panel.getByLabel("Ordner oben anzeigen", { exact: true }).uncheck();
    await page.waitForFunction(() => (
      document.querySelector('.pane[data-side="left"] .row .r-name')?.textContent === "Alpha.txt"
    ));
    await panel.getByLabel("Ordner oben anzeigen", { exact: true }).check();
    await page.waitForFunction(() => (
      document.querySelector('.pane[data-side="left"] .row .r-name')?.textContent === "Bilder"
    ));

    const compactHeight = Math.round((await alpha.boundingBox()).height);
    await panel.getByLabel("Normal", { exact: true }).check();
    await panel.getByLabel("L", { exact: true }).check();
    await panel.getByLabel("Abwechselnde Zeilenfarben", { exact: true }).check();
    await panel.getByLabel("Ordner fett darstellen", { exact: true }).uncheck();
    const normalHeight = Math.round((await alpha.boundingBox()).height);
    assert.ok(normalHeight > compactHeight, `Normale Dichte blieb bei ${normalHeight}px.`);
    assert.ok(Number.parseFloat(await alpha.evaluate(row => getComputedStyle(row).fontSize)) >= 14);
    assert.ok(await leftPane.locator(".row.stripe").count() > 0);
    const folderWeight = Number(await leftPane.locator(".row.dir .r-name").evaluate(
      name => getComputedStyle(name).fontWeight,
    ));
    assert.ok(folderWeight < 600, `Ordner blieb trotz Option fett (${folderWeight}).`);

    // Versteckte Dateien werden im Backend nur für diese Pane neu angefordert.
    await panel.getByLabel("Versteckte Dateien", { exact: true }).check();
    await leftPane.locator(".row", { hasText: ".verborgen" }).waitFor();
    assert.equal(await rightPane.locator(".row", { hasText: ".verborgen" }).count(), 0);

    // „Relativ“ ist eine reine Anzeigeform; die Rohsekunden bleiben unverändert.
    const relative = await page.evaluate(async () => {
      const { fmtEntryDate } = await import("./ui-utils.js");
      const now = new Date(2026, 7, 5, 12, 0);
      const today = new Date(2026, 7, 5, 9, 3).getTime() / 1000;
      const yesterday = new Date(2026, 7, 4, 18, 7).getTime() / 1000;
      return [fmtEntryDate(today, 3, true, now), fmtEntryDate(yesterday, 3, true, now)];
    });
    assert.deepEqual(relative, ["Heute, 09:03", "Gestern, 18:07"]);

    // Ordnerwert überlebt das Neuladen; die rechte Pane behält weiterhin den alten
    // globalen Standard, solange sie nicht neu geladen wird.
    await page.reload({ waitUntil: "networkidle" });
    await leftList.waitFor();
    assert.equal(await leftPane.locator(".col-kind").isVisible(), true);
    await leftPane.locator(".row", { hasText: ".verborgen" }).waitFor();
    assert.equal(await rightPane.locator(".col-kind").isVisible(), false);

    // Zurücksetzen entfernt die Ordnerüberschreibung. Danach wird derselbe Satz als
    // globaler Standard gespeichert und greift beim nächsten Ortswechsel der rechten Pane.
    await toggle.click();
    await panel.getByRole("button", { name: "Zurücksetzen" }).click();
    await page.waitForFunction(() => document.querySelector('.pane[data-side="left"] .col-kind')?.hidden);
    assert.equal(await leftPane.locator(".row", { hasText: ".verborgen" }).count(), 0);
    await panel.getByLabel("Art", { exact: true }).check();
    await panel.getByLabel("Normal", { exact: true }).check();
    await panel.getByRole("button", { name: "Als Standard verwenden" }).click();
    await panel.getByText("Als Standard gespeichert.", { exact: true }).waitFor();

    await rightList.click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "refresh"));
    assert.equal(await rightPane.locator(".col-kind").isVisible(), false);
    await rightPane.locator(".path").fill("/mock/fast");
    await rightPane.locator(".path").press("Enter");
    await rightPane.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    assert.equal(await rightPane.locator(".col-kind").isVisible(), true);
    assert.equal(await rightPane.getAttribute("data-density"), "normal");
  });

  await runCase("Darstellungsänderungen überstehen ein langsames Hidden-Listing", "?keyboard=1", async page => {
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command === "scan_dir" && args.path === "/mock/quelle" && args.showHidden) {
          await new Promise(resolve => { window.__RELEASE_HIDDEN_LISTING__ = resolve; });
        }
        return result;
      };
    });
    const pane = page.locator('.pane[data-side="left"]');
    await pane.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" }).click();
    const panel = pane.getByRole("dialog", { name: "Darstellung" });
    await panel.getByLabel("Versteckte Dateien", { exact: true }).check();
    await page.waitForFunction(() => typeof window.__RELEASE_HIDDEN_LISTING__ === "function");
    await panel.getByLabel("Art", { exact: true }).check();
    await panel.getByLabel("Normal", { exact: true }).check();
    await panel.getByLabel("L", { exact: true }).check();
    await page.evaluate(() => window.__RELEASE_HIDDEN_LISTING__());
    await pane.locator(".row", { hasText: ".verborgen" }).waitFor();
    assert.equal(await panel.getByLabel("Art", { exact: true }).isChecked(), true);
    assert.equal(await pane.locator(".col-kind").isVisible(), true);
    assert.equal(await pane.getAttribute("data-density"), "normal");
    assert.equal(await pane.getAttribute("data-text-size"), "l");
    await pane.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" }).click();
    await page.evaluate(() => { delete window.__RELEASE_HIDDEN_LISTING__; });
    await pane.locator(".path").fill("/mock/fast");
    await pane.locator(".path").press("Enter");
    await pane.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
  });

  await runCase("Lokale Ordnergrößen sind global schaltbar, sichtbar und bei Navigation abgebrochen", "?keyboard=1&sizeDelay=80", async page => {
    // Dieser Fall prüft Berechnung, Abbruch und Sichtbarkeit der Ordnergröße,
    // nicht die separat getestete Plattformvorgabe der Anzeigeeinheit.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "units_decimal"));
    const leftPane = page.locator('.pane[data-side="left"]');
    const rightPane = page.locator('.pane[data-side="right"]');
    const toggle = leftPane.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" });
    await toggle.click();
    const panel = leftPane.getByRole("dialog", { name: "Darstellung" });
    const automatic = panel.getByLabel("Lokale Ordner automatisch berechnen (global)", { exact: true });
    assert.equal(await automatic.isChecked(), false);
    await automatic.check();

    // Der laufende Zustand steht direkt in der Größenspalte. Die globale Wahl ist
    // gleichzeitig im Popover der zweiten lokalen Pane sichtbar.
    const folder = leftPane.locator('.row.dir', { hasText: "Bilder" });
    await folder.locator(".folder-size-loading").waitFor();
    await rightPane.getByRole("button", { name: "Darstellungsoptionen für die rechte Pane" }).click();
    assert.equal(
      await rightPane.getByLabel("Lokale Ordner automatisch berechnen (global)", { exact: true }).isChecked(),
      true,
    );
    await folder.getByRole("button", { name: "Größe von Bilder neu berechnen" }).waitFor();
    assert.equal(await folder.locator(".r-size").innerText(), "4,10 KB");

    let state = await page.evaluate(() => window.__MOCK_DIRECTORY_SIZES__());
    assert.equal(state.calls.length, 1);
    assert.deepEqual(state.calls[0], {
      side: "left",
      endpoint: { provider: "local", root: "/mock/quelle" },
      names: ["Bilder"],
      recalculate: false,
      clientToken: state.calls[0].clientToken,
    });

    // Klick auf einen fertigen Wert ist der ausdrückliche Neulauf und umgeht den
    // Sitzungscache. Danach navigieren wir weg, während dieser Lauf noch arbeitet.
    await folder.getByRole("button", { name: "Größe von Bilder neu berechnen" }).click();
    await folder.locator(".folder-size-loading").waitFor();
    state = await page.evaluate(() => window.__MOCK_DIRECTORY_SIZES__());
    assert.equal(state.calls.at(-1).recalculate, true);
    const path = leftPane.locator(".path");
    await path.fill("/mock/fast");
    await path.press("Enter");
    await leftPane.locator(".row", { hasText: "Schnell.txt" }).waitFor();
    await page.waitForTimeout(120);
    assert.equal(await leftPane.locator(".folder-size-value").count(), 0);
    state = await page.evaluate(() => window.__MOCK_DIRECTORY_SIZES__());
    assert.ok(state.cancels.filter(call => call.side === "left").length >= 2);
  });

  await runCase("Beobachtete SFTP-Ordnergrößen zeigen Best Effort in Spalte und Informationen", "?sizeDelay=40&directorySizeObserved=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "units_decimal"));
    const pane = page.locator('.pane[data-side="left"]');
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const folder = pane.locator('.row.dir', { hasText: "remote" });
    await folder.getByRole("button", { name: "Größe von remote berechnen" }).click();
    const size = folder.getByRole("button", { name: /Größe von remote:.*Best Effort.*neu berechnen/ });
    await size.waitFor();
    assert.match(await size.innerText(), /^≈ /);
    assert.match(await size.getAttribute("title"), /Best Effort.*1000 Einträge/);
    await folder.click({ button: "right" });
    await page.locator("#context-menu").getByRole("menuitem", { name: /Informationen/ }).click();
    const dialog = page.getByRole("dialog", { name: /Informationen · remote/ });
    const sizeRow = dialog.getByText("Größe", { exact: true }).locator("..");
    assert.match(await sizeRow.innerText(), /≈ .*Best Effort/);
    assert.match(await dialog.innerText(), /Beobachtet: 4 Dateien/);
    assert.match(await dialog.innerText(), /Kein Dateiinhalt wird geöffnet/);
    await sizeRow.getByRole("button", { name: "Neu berechnen", exact: true }).click();
    await page.waitForFunction(() => window.__MOCK_DIRECTORY_SIZES__().calls.length === 2);
  });

  await runCase("Beobachtete SFTP-Ordnergröße erscheint nach Navigation nicht in einer fremden Pane", "?sizeDelay=300&directorySizeObserved=1", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await pane.getByRole("button", { name: "Größe von remote berechnen" }).click();
    const path = pane.locator(".path"); await path.fill("/anderer-ordner"); await path.press("Enter");
    await page.waitForTimeout(400);
    assert.equal(await pane.locator(".folder-size-value").count(), 0);
    assert.equal(await pane.locator(".folder-size-loading").count(), 0);
  });

  await runCase("Remote-Ordnergrößen starten ausschließlich per Klick", "?sizeDelay=40", async page => {
    // Auf Linux wäre die korrekte Vorgabe sonst binär. Die feste Dezimalwahl hält
    // den Größenwert dieses Klick-/Netzwerktests auf beiden Plattformen identisch.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "units_decimal"));
    const leftPane = page.locator('.pane[data-side="left"]');
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const folder = leftPane.locator('.row.dir', { hasText: "remote" });
    await folder.waitFor();

    // Weder Listing noch die globale lokale Wahl lösen Netzrekursion aus.
    let state = await page.evaluate(() => window.__MOCK_DIRECTORY_SIZES__());
    assert.equal(state.calls.length, 0);
    await leftPane.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" }).click();
    const panel = leftPane.getByRole("dialog", { name: "Darstellung" });
    await panel.getByText(/Auf Servern wird eine Ordnergröße nur nach Klick/).waitFor();
    assert.equal(
      await panel.getByLabel("Lokale Ordner automatisch berechnen (global)", { exact: true }).isVisible(),
      false,
    );
    await leftPane.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" }).click();

    await folder.getByRole("button", { name: "Größe von remote berechnen" }).click();
    await folder.locator(".folder-size-loading").waitFor();
    await folder.getByRole("button", { name: "Größe von remote neu berechnen" }).waitFor();
    assert.equal(await folder.locator(".r-size").innerText(), "8,19 KB");
    state = await page.evaluate(() => window.__MOCK_DIRECTORY_SIZES__());
    assert.deepEqual(state.calls[0], {
      side: "left",
      endpoint: { provider: "sftp", profileId: "mock_sftp", profileRevision: 11, root: "/" },
      names: ["remote"],
      sessionGeneration: 1,
      recalculate: false,
      clientToken: state.calls[0].clientToken,
    });
    await folder.getByRole("button", { name: "Größe von remote neu berechnen" }).click();
    state = await page.evaluate(() => window.__MOCK_DIRECTORY_SIZES__());
    assert.equal(state.calls.at(-1).recalculate, true);
  });

  await runCase("Informationsdialog erhält Fokus während laufender Größenmessung", "?keyboard=1&sizeDelay=1000", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    await pane.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" }).click();
    await pane.getByLabel("Lokale Ordner automatisch berechnen (global)", { exact: true }).check();
    await pane.locator('.row.dir', { hasText: "Bilder" }).locator(".folder-size-loading").waitFor();
    await pane.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" }).click();
    await pane.locator('.row.dir', { hasText: "Bilder" }).click();
    await page.keyboard.press("Meta+I");
    const dialog = page.getByRole("dialog", { name: /Informationen · Bilder/ });
    await dialog.waitFor();
    assert.equal(await dialog.getByRole("button", { name: /Berechn/ }).isDisabled(), true);
    assert.equal(await dialog.evaluate(element => element.contains(document.activeElement)), true);
    await page.keyboard.press("Escape");
    await dialog.waitFor({ state: "hidden" });
  });

  await runCase("Informationen zeigen lokale Metadaten und berechnen Ordner erst auf Klick", "?keyboard=1&sizeDelay=40", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "units_decimal"));
    const pane = page.locator('.pane[data-side="left"]');
    const folder = pane.locator('.row.dir', { hasText: "Bilder" });
    await folder.click({ button: "right" });
    const menu = page.locator("#context-menu");
    await menu.getByRole("menuitem", { name: /Informationen/ }).click();

    const dialog = page.getByRole("dialog", { name: /Informationen · Bilder/ });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /\/mock\/quelle\/Bilder/);
    assert.match(await dialog.innerText(), /rwxr-xr-x \(755\)/);
    const sizeRow = dialog.getByText("Größe", { exact: true }).locator("..");
    assert.match(await sizeRow.innerText(), /nicht berechnet/i);
    await sizeRow.getByRole("button", { name: "Berechnen", exact: true }).click();
    assert.equal(
      await dialog.evaluate(element => element.contains(document.activeElement)),
      true,
      "Das Neurendern des Größenstatus darf den Fokus nicht aus dem Modal verlieren.",
    );
    await sizeRow.getByRole("button", { name: "Neu berechnen", exact: true }).waitFor();
    assert.match(await sizeRow.innerText(), /4,10 KB \(4\.096 Byte\)/);
    assert.match(await dialog.getByText("Enthalten", { exact: true }).locator("..").innerText(),
      /2 Dateien · 1 Unterordner/);
    let state = await page.evaluate(() => window.__MOCK_DIRECTORY_SIZES__());
    assert.deepEqual(state.calls.at(-1).endpoint, { provider: "local", root: "/mock/quelle" });
    await dialog.getByRole("button", { name: "Schließen" }).click();

    // Mehrfachauswahl bleibt eine gemeinsame, rein lesende Zusammenfassung. ⌘I
    // beziehungsweise Ctrl+I öffnet denselben Weg wie der Rechtsklick.
    const file = pane.locator('.row', { hasText: "Alpha.txt" });
    await file.click();
    await folder.click({ modifiers: ["Meta"] });
    await page.keyboard.press("Meta+I");
    const multi = page.getByRole("dialog", { name: /Informationen · 2 Objekte/ });
    await multi.waitFor();
    assert.match(await multi.innerText(), /1 Datei · 1 Ordner/);
    assert.deepEqual(await multi.locator(".file-info-folders li > span:first-child").allTextContents(),
      ["Alpha.txt", "Bilder"]);
    assert.match(await multi.getByText("Berechtigungen", { exact: true }).locator("..").innerText(),
      /Verschieden/);
    assert.match(await multi.getByText("Größe", { exact: true }).locator("..").innerText(),
      /4,10 KB \(4\.101 Byte\)/);
    await multi.getByRole("button", { name: "Schließen" }).click();

    // Der native Darstellungseintrag öffnet dasselbe panegebundene Popover wie der
    // bisher schwer erkennbare Drei-Linien-Knopf.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "view_options"));
    await pane.getByRole("dialog", { name: "Darstellung" }).waitFor();
    assert.equal(
      await pane.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" })
        .getAttribute("aria-expanded"),
      "true",
    );
  });

  await runCase("SFTP-Auslassungen bleiben panegebunden, zugänglich und als Text lesbar", "?listingOmissions=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const pane = page.locator('.pane[data-side="left"]');
    const row = pane.getByRole("option", { name: /nutzbar.txt/ });
    await row.waitFor();
    const toggle = pane.getByRole("button", { name: "2 ausgelassene Einträge anzeigen" });
    await toggle.waitFor();
    await toggle.focus();
    await toggle.press("Space");
    assert.equal(await toggle.getAttribute("aria-expanded"), "true");
    const details = pane.getByRole("textbox", { name: "Ausgelassene Einträge" });
    assert.match(await details.inputValue(), /Zeile\\nneu.txt/);
    assert.match(await details.inputValue(), /<img src=x onerror=alert\(1\)>/);
    assert.equal(await details.locator("img").count(), 0);
    await details.focus();
    await toggle.evaluate(element => {
      element.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
      element.click();
    });
    assert.equal(await toggle.evaluate(element => element === document.activeElement), true,
      "Beim Einklappen muss der Fokus aus dem verborgenen Bericht auf den Knopf zurückkehren.");
    await toggle.press("Space");
    await page.setViewportSize({ width: 960, height: 640 });
    const bounds = await pane.boundingBox();
    const detailBounds = await details.boundingBox();
    assert.ok(detailBounds.x >= bounds.x && detailBounds.x + detailBounds.width <= bounds.x + bounds.width);
    assert.ok(detailBounds.y + detailBounds.height <= bounds.y + bounds.height);
    assert.equal(await page.locator('.pane[data-side="right"] .listing-omissions-toggle').isVisible(), false);
    await row.click();
    await page.keyboard.press("Meta+I");
    const dialog = page.getByRole("dialog", { name: /Informationen · nutzbar.txt/ });
    await dialog.waitFor();
    assert.match(await dialog.getByText("Größe", { exact: true }).locator("..").innerText(), /7 Byte/);
    await page.keyboard.press("Escape");
    const path = pane.locator(".path");
    await path.fill("/anderer-ordner");
    await path.press("Enter");
    await row.waitFor({ state: "hidden" });
    assert.equal(await toggle.isVisible(), false);
    assert.equal(await details.isVisible(), false);
  });

  await runCase("Große SFTP-Auslassungen bleiben vollständig in einem kopierbaren Textfeld", "?listingOmissions=1&manyOmissions=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const pane = page.locator('.pane[data-side="left"]');
    await pane.getByRole("button", { name: "4000 ausgelassene Einträge anzeigen" }).click();
    const report = pane.getByRole("textbox", { name: "Ausgelassene Einträge" });
    await report.waitFor();
    assert.match(await report.inputValue(), /Eintrag-3999: Dateityp nicht belegt/);
    assert.equal(await report.getAttribute("readonly"), "");
    await report.focus();
    await report.press("ControlOrMeta+A");
    assert.equal(await report.evaluate(element => element.selectionEnd - element.selectionStart === element.value.length), true);
    assert.equal(await pane.locator(".listing-omissions-text").count(), 1);
  });

  await runCase("SFTP-Auslassungen bleiben bei einer sonst leeren Liste sichtbar", "?listingOmissions=1&onlyOmissions=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const pane = page.locator('.pane[data-side="left"]');
    await pane.getByRole("button", { name: "2 ausgelassene Einträge anzeigen" }).click();
    assert.equal(await pane.getByRole("option").count(), 0);
    assert.equal(await pane.locator(".empty").textContent(), "Keine anzeigbaren Einträge");
    assert.match(await pane.getByRole("textbox", { name: "Ausgelassene Einträge" }).inputValue(), /keinen belegten Dateityp/);
  });

  await runCase("Verspätete SFTP-Auslassungen erscheinen nach Providerwechsel nicht lokal", "?listingOmissions=1&sftpScanDelay=300", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.locator("#device-list .device-item").filter({ hasText: "Systemvolume" }).click();
    await page.waitForTimeout(400);
    const pane = page.locator('.pane[data-side="left"]');
    assert.equal(await pane.locator(".listing-omissions-toggle").isVisible(), false);
    assert.equal(await pane.getByRole("option", { name: /nutzbar.txt/ }).count(), 0);
  });

  await runCase("SFTP-Dateigrößen unterscheiden unbekannt, leer und bekannte Teilmenge", "?unknownSizes=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const pane = page.locator('.pane[data-side="left"]');
    const unknown = pane.getByRole("option", { name: /Unbekannt.txt/ });
    const empty = pane.getByRole("option", { name: /Leer.txt/ });
    const known = pane.getByRole("option", { name: /Bekannt.txt/ });
    assert.equal(await unknown.locator(".r-size").innerText(), "—");
    assert.equal(await empty.locator(".r-size").innerText(), "0 Byte");
    for (let direction = 0; direction < 2; direction += 1) {
      await pane.locator('[data-sort="size"]').click();
      assert.equal((await pane.locator(".row .r-name").allTextContents()).at(-1), "Unbekannt.txt");
    }
    await unknown.click();
    await page.keyboard.press("Meta+I");
    let dialog = page.getByRole("dialog", { name: /Informationen · Unbekannt.txt/ });
    await dialog.waitFor();
    assert.match(await dialog.getByText("Größe", { exact: true }).locator("..").innerText(), /Größe nicht verfügbar/);
    assert.doesNotMatch(await dialog.innerText(), /0 Byte/);
    await page.keyboard.press("Escape");
    await empty.click();
    await page.keyboard.press("Meta+I");
    dialog = page.getByRole("dialog", { name: /Informationen · Leer.txt/ });
    await dialog.waitFor();
    assert.match(await dialog.getByText("Größe", { exact: true }).locator("..").innerText(), /0 Byte/);
    await page.keyboard.press("Escape");
    await known.click();
    await unknown.click({ modifiers: ["Meta"] });
    await page.keyboard.press("Meta+I");
    dialog = page.getByRole("dialog", { name: /Informationen · 2 Objekte/ });
    await dialog.waitFor();
    assert.match(await dialog.getByText("Größe", { exact: true }).locator("..").innerText(), /5 Byte bekannt · 1 Datei ohne Größenangabe/);
    assert.match(await dialog.locator(".file-info-folders").innerText(), /Unbekannt.txt\s+—/);
    await page.keyboard.press("Escape");
    await pane.getByRole("option", { name: /Link.txt/ }).click();
    await page.keyboard.press("Meta+I");
    dialog = page.getByRole("dialog", { name: /Informationen · Link.txt/ });
    await dialog.waitFor();
    assert.match(await dialog.getByText("Größe", { exact: true }).locator("..").innerText(), /Größe nicht verfügbar/);
  });

  await runCase("SFTP-Dateigrößen behalten unbekannte Dateien neben beobachteten Ordnern sichtbar", "?unknownSizes=1&directorySizeObserved=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const pane = page.locator('.pane[data-side="left"]');
    const folder = pane.getByRole("option", { name: /^remote / });
    await folder.getByRole("button", { name: "Größe von remote berechnen" }).click();
    await folder.locator(".r-size").getByText(/≈/).waitFor();
    await folder.click();
    await pane.getByRole("option", { name: /Unbekannt.txt/ }).click({ modifiers: ["Meta"] });
    await page.keyboard.press("Meta+I");
    const dialog = page.getByRole("dialog", { name: /Informationen · 2 Objekte/ });
    await dialog.waitFor();
    const summary = await dialog.getByText("Größe", { exact: true }).locator("..").innerText();
    assert.match(summary, /≈.*bekannt · Best Effort · 1 Datei ohne Größenangabe/);
    assert.doesNotMatch(summary, /vollständig erfasst/);
  });

  await runCase("FTPS-Informationen verwenden die panegebundene Remote-Größenmessung", "?sizeDelay=40&remoteMissingMode=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "units_decimal"));
    const pane = page.locator('.pane[data-side="left"]');
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    const folder = pane.locator('.row.dir', { hasText: "remote" });
    await folder.click({ button: "right" });
    await page.locator("#context-menu").getByRole("menuitem", { name: /Informationen/ }).click();

    const dialog = page.getByRole("dialog", { name: /Informationen · remote/ });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /FTPS · Mock-FTPS/);
    assert.match(await dialog.innerText(), /\/remote/);
    assert.match(
      await dialog.getByText("Berechtigungen", { exact: true }).locator("..").innerText(),
      /—/,
      "Ein Provider ohne Unix-Modus darf nicht als Berechtigung 000 erscheinen.",
    );
    const sizeRow = dialog.getByText("Größe", { exact: true }).locator("..");
    await sizeRow.getByRole("button", { name: "Berechnen", exact: true }).click();
    await sizeRow.getByRole("button", { name: "Neu berechnen", exact: true }).waitFor();
    assert.match(await sizeRow.innerText(), /8,19 KB \(8\.192 Byte\)/);
    assert.match(await dialog.getByText("Enthalten", { exact: true }).locator("..").innerText(),
      /4 Dateien · 1 Unterordner/);
    const state = await page.evaluate(() => window.__MOCK_DIRECTORY_SIZES__());
    assert.deepEqual(state.calls.at(-1).endpoint, {
      provider: "ftps", profileId: "mock_ftps", profileRevision: 21, root: "/",
    });
    assert.equal(state.calls.at(-1).sessionGeneration, 1);
  });

  await runCase("Informationsdialog startet nach einem Pane-Wechsel keinen alten Größenlauf", "", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const folder = pane.locator('.row.dir', { hasText: "remote" });
    await folder.click({ button: "right" });
    await page.locator("#context-menu").getByRole("menuitem", { name: /Informationen/ }).click();
    const dialog = page.getByRole("dialog", { name: /Informationen · remote/ });
    await dialog.waitFor();

    await page.evaluate(() => {
      window.__MOCK_REMOVE_PROFILE__("sftp", "mock_sftp");
      window.__MOCK_EMIT__("connections-updated", {
        protocol: "sftp", profileId: "mock_sftp", revision: 11, change: "removed",
      });
    });
    await pane.getByRole("option", { name: /Beispiel\.txt/ }).waitFor();
    await dialog.getByRole("button", { name: "Berechnen", exact: true }).click();
    await dialog.waitFor({ state: "hidden" });
    const state = await page.evaluate(() => window.__MOCK_DIRECTORY_SIZES__());
    assert.equal(state.calls.length, 0);
  });

  await runCase("Informationen erklären eine partielle Ordnergröße ohne exakte Gesamtsumme", "?folderSort=1&sizeDelay=40", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    const folder = pane.locator('.row.dir', { hasText: "Unvollständig" });
    await folder.click({ button: "right" });
    await page.locator("#context-menu").getByRole("menuitem", { name: /Informationen/ }).click();
    const dialog = page.getByRole("dialog", { name: /Informationen · Unvollständig/ });
    const sizeRow = dialog.getByText("Größe", { exact: true }).locator("..");
    await sizeRow.getByRole("button", { name: "Berechnen", exact: true }).click();
    await sizeRow.getByRole("button", { name: "Neu berechnen", exact: true }).waitFor();
    assert.match(await sizeRow.innerText(), /1 Ordner unvollständig/);
    assert.doesNotMatch(await sizeRow.innerText(), /200 Byte/,
      "Eine Teilbilanz darf nicht als exakte Gesamtgröße in der Größenzeile stehen.");
    assert.match(await dialog.locator(".file-info-folders").innerText(), /200 Byte erfasst/);
    assert.match(await dialog.locator(".file-info-issues").innerText(),
      /Ein Unterordner konnte nicht gelesen werden/);
  });

  await runCase("Größensortierung verwendet nur vollständige Ordnerbilanzen", "?folderSort=1&sizeDelay=60", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    const toggle = pane.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" });
    await toggle.click();
    const panel = pane.getByRole("dialog", { name: "Darstellung" });
    // „Ordner immer alphabetisch“ ist eine bewusste Gegenoption. Erst ohne sie
    // darf die gewählte Größenspalte die Ordner untereinander nach Bytes ordnen.
    await panel.getByLabel("Ordner immer alphabetisch", { exact: true }).uncheck();
    await toggle.click();
    await pane.locator('.col-size[data-sort="size"]').click();
    await toggle.click();
    await panel.getByLabel("Lokale Ordner automatisch berechnen (global)", { exact: true }).check();

    const names = () => pane.locator(".row.dir .r-name").allTextContents();
    await pane.getByRole("button", { name: "Größe von Klein neu berechnen" }).waitFor();
    await pane.getByRole("button", { name: "Größe von Groß neu berechnen" }).waitFor();
    const partial = pane.getByRole("button", { name: "Größe von Unvollständig neu berechnen" });
    await partial.waitFor();
    assert.deepEqual(await names(), ["Klein", "Groß", "Unvollständig"]);
    assert.match(await partial.innerText(), /200 Byte.*⚠/);
    assert.match(await partial.getAttribute("title"), /Unterordner konnte nicht gelesen werden/);

    // Unvollständige 200 Byte sind keine belastbare Größe und bleiben deshalb auch
    // absteigend hinter den beiden vollständigen Werten.
    await toggle.click();
    await pane.locator('.col-size[data-sort="size"]').click();
    assert.deepEqual(await names(), ["Groß", "Klein", "Unvollständig"]);
  });

  await runCase("Gemischte Sortierung bleibt ohne Ordnerblock transitiv", "?mixedSort=1", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    const toggle = pane.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" });
    await toggle.click();
    const panel = pane.getByRole("dialog", { name: "Darstellung" });
    const foldersFirst = panel.getByLabel("Ordner oben anzeigen", { exact: true });
    const alphabetical = panel.getByLabel("Ordner immer alphabetisch", { exact: true });

    assert.equal(await alphabetical.isEnabled(), true);
    await foldersFirst.uncheck();
    assert.equal(await alphabetical.isChecked(), true, "Unteroption soll gespeichert bleiben.");
    assert.equal(await alphabetical.isDisabled(), true, "Ohne Ordnerblock ist die Unteroption inaktiv.");
    await toggle.click();

    const names = () => pane.locator(".row .r-name").allTextContents();
    await pane.locator('.col-date[data-sort="modified"]').click();
    assert.deepEqual(await names(), ["B-Ordner", "Mitte.txt", "A-Ordner"]);
    await pane.locator('.col-date[data-sort="modified"]').click();
    assert.deepEqual(await names(), ["A-Ordner", "Mitte.txt", "B-Ordner"]);

    await toggle.click();
    await foldersFirst.check();
    assert.equal(await alphabetical.isEnabled(), true);
    assert.deepEqual(await names(), ["A-Ordner", "B-Ordner", "Mitte.txt"]);
  });

  await runCase("Jede sichtbare Spalte sortiert Rohwerte unabhängig je Pane", "?keyboard=1", async page => {
    const leftPane = page.locator('.pane[data-side="left"]');
    const rightPane = page.locator('.pane[data-side="right"]');
    const leftToggle = leftPane.getByRole("button", { name: "Darstellungsoptionen für die linke Pane" });
    await leftToggle.click();
    const panel = leftPane.getByRole("dialog", { name: "Darstellung" });
    for (const label of ["Art", "Erstellungsdatum", "Eigentümer", "Gruppe", "Berechtigungen"]) {
      await panel.getByLabel(label, { exact: true }).check();
    }
    await leftToggle.click();

    // Beide Panes bekommen dieselben kontrollierten Einträge. Nur so belegt der
    // Test, dass ein Sortierklick links die rechte Pane nicht mehr mitändert.
    const rightPath = rightPane.locator(".path");
    await rightPath.fill("/mock/quelle");
    await rightPath.press("Enter");
    await rightPane.locator(".row", { hasText: "Gamma" }).waitFor();

    const fileNames = pane => pane.locator(".row.file .r-name").allTextContents();
    assert.deepEqual(await fileNames(leftPane), ["Alpha.txt", "Beta.txt", "Gamma"]);

    // Name ist schon aufsteigend aktiv. Enter und Leertaste belegen zusätzlich die
    // Tastaturbedienung der Sortierköpfe.
    const nameHeader = leftPane.locator('.col-name[data-sort="name"]');
    await nameHeader.focus();
    await nameHeader.press("Enter");
    assert.deepEqual(await fileNames(leftPane), ["Gamma", "Beta.txt", "Alpha.txt"]);
    await nameHeader.press("Space");
    assert.deepEqual(await fileNames(leftPane), ["Alpha.txt", "Beta.txt", "Gamma"]);

    const cases = [
      ["size", ["Beta.txt", "Alpha.txt", "Gamma"]],
      ["kind", ["Gamma", "Beta.txt", "Alpha.txt"]],
      ["modified", ["Alpha.txt", "Beta.txt", "Gamma"]],
      ["created", ["Alpha.txt", "Beta.txt", "Gamma"]],
      ["owner", ["Alpha.txt", "Beta.txt", "Gamma"]],
      ["group", ["Alpha.txt", "Beta.txt", "Gamma"]],
      ["permissions", ["Beta.txt", "Alpha.txt", "Gamma"]],
    ];
    for (const [key, expected] of cases) {
      await leftPane.locator(`.col[data-sort="${key}"]`).click();
      assert.deepEqual(await fileNames(leftPane), expected, `Sortierung nach ${key} ist falsch.`);
    }

    // Fehlende Providerwerte bleiben auch absteigend am Ende.
    await leftPane.locator('.col[data-sort="owner"]').click();
    await leftPane.locator('.col[data-sort="owner"]').click();
    assert.deepEqual(await fileNames(leftPane), ["Beta.txt", "Alpha.txt", "Gamma"]);

    // Links Größe, rechts Änderungsdatum: Markierung und Reihenfolge bleiben
    // unabhängig.
    await leftPane.locator('.col[data-sort="size"]').click();
    await rightPane.locator('.col[data-sort="modified"]').click();
    assert.deepEqual(await fileNames(leftPane), ["Beta.txt", "Alpha.txt", "Gamma"]);
    assert.deepEqual(await fileNames(rightPane), ["Alpha.txt", "Beta.txt", "Gamma"]);
    assert.equal(await leftPane.locator('.col-size.sorted').count(), 1);
    assert.equal(await rightPane.locator('.col-date.sorted').count(), 1);
    assert.equal(await rightPane.locator('.col-size.sorted').count(), 0);

    // Auch der native Menüweg wirkt ausschließlich auf die aktive rechte Pane.
    await leftPane.locator('.col[data-sort="kind"]').click();
    await rightPane.getByRole("listbox").click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sort_size"));
    await rightPane.locator('.col-size.sorted').waitFor();
    assert.equal(await leftPane.locator('.col-kind.sorted').count(), 1);
    assert.equal(await leftPane.locator('.col-size.sorted').count(), 0);

    // Wird die aktive Sortierspalte ausgeblendet, fällt diese Pane sichtbar und
    // deterministisch auf Name aufsteigend zurück.
    await leftToggle.click();
    await panel.getByLabel("Art", { exact: true }).uncheck();
    assert.deepEqual(await fileNames(leftPane), ["Alpha.txt", "Beta.txt", "Gamma"]);
    assert.equal(await leftPane.locator('.col-name.sorted').count(), 1);
  });

  await runCase("Spaltenkopf und Zeilen bleiben bündig", "?entries=5000", async page => {
    // Die Trenner im Kopf dürfen nicht über anderen Werten liegen als den Zellen,
    // deshalb gleicht der Kopf auch die Breite des Listen-Scrollbalkens aus.
    const offsets = await page.evaluate(() => {
      const pane = document.querySelector('.pane[data-side="left"]');
      const at = selector => Math.round(pane.querySelector(selector).getBoundingClientRect().left);
      return {
        size: [at(".col-size"), at(".r-size")],
        date: [at(".col-date"), at(".r-date")],
      };
    });
    assert.equal(offsets.size[0], offsets.size[1], "Größenspalte von Kopf und Zeile verschoben.");
    assert.equal(offsets.date[0], offsets.date[1], "Datumsspalte von Kopf und Zeile verschoben.");
  });

  await runCase("Virtualisierung eines großen Ordners", "?entries=5000", async page => {
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    assert.ok(await list.locator('[role="option"]').count() < 100, "Zu viele Zeilen liegen gleichzeitig im DOM.");
    await list.focus();
    await page.keyboard.press("End");
    const selected = page.locator('[role="option"][aria-selected="true"]');
    assert.match(await selected.innerText(), /Datei-05000\.txt/);
    assert.equal(await selected.getAttribute("aria-setsize"), "5000");
  });

  await runCase("Theme-Persistenz über das native Menü", "", async page => {
    // Der Header trägt keinen Theme-Switch mehr; umgeschaltet wird ausschließlich
    // über das Menü „Darstellung", dessen Klick als Tauri-Event ankommt.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "theme_dark"));
    assert.equal(await page.evaluate(() => localStorage.getItem("doppeldecker-theme")), "dark");
    await page.reload({ waitUntil: "networkidle" });
    assert.equal(await page.locator("html").getAttribute("data-theme"), "dark");
  });

  await runCase("Sync-Vorschau verzögert erfolgreiches Enqueue nicht durch einen weiteren Scan", "", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__AP01_PLAN_COUNT__ = 0;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "plan_sync") {
          window.__AP01_PLAN_COUNT__ += 1;
          await new Promise(resolve => { window.__AP01_RELEASE_PLAN__ = resolve; });
        }
        return original(command, args);
      };
      window.__MOCK_EMIT__("menu", "sync_mode");
      document.querySelector("#btn-sync-simulate").click();
    });
    await page.waitForFunction(() => window.__AP01_PLAN_COUNT__ === 1);
    await page.waitForTimeout(200);
    await page.evaluate(() => window.__AP01_RELEASE_PLAN__());
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    await page.locator("#sync-view").waitFor({ state: "hidden", timeout: 1000 });
    assert.equal(await page.evaluate(() => window.__AP01_PLAN_COUNT__), 1);
  });

  await runCase("Sync-Vorschau holt Optionswechsel während des Startscans nach", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#sync-summary .pill").first().waitFor();
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__AP01_PLANS__ = [];
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "plan_sync") {
          window.__AP01_PLANS__.push(args.options.compare);
          if (window.__AP01_PLANS__.length === 1) {
            await new Promise(resolve => { window.__AP01_RELEASE_PLAN__ = resolve; });
          }
        }
        return original(command, args);
      };
    });
    await page.locator("#btn-sync-simulate").click();
    await page.waitForFunction(() => window.__AP01_PLANS__.length === 1);
    await page.locator('#sync-compare-group [value="ContentHash"]').check();
    await page.waitForTimeout(200);
    await page.evaluate(() => window.__AP01_RELEASE_PLAN__());
    await page.waitForFunction(() => window.__AP01_PLANS__.length === 2);
    await page.locator("#sync-summary .pill").first().waitFor();
    assert.deepEqual(await page.evaluate(() => window.__AP01_PLANS__), ["SizeAndTime", "ContentHash"]);
    assert.equal(await page.locator("#sync-view").isVisible(), true);
  });

  for (const destination of ["Dateiansicht", "beide Richtungen"]) {
    await runCase(`Sync-Vorschau startet nach Wechsel zu ${destination} keinen Scan`, "", async page => {
      await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
      await page.locator("#sync-summary .pill").first().waitFor();
      await page.evaluate(() => {
        const original = window.__TAURI__.core.invoke;
        window.__AP01_PLAN_COUNT__ = 0;
        window.__TAURI__.core.invoke = async (command, args) => {
          if (command === "plan_sync") {
            window.__AP01_PLAN_COUNT__ += 1;
            await new Promise(resolve => { window.__AP01_RELEASE_PLAN__ = resolve; });
          }
          return original(command, args);
        };
      });
      await page.locator("#btn-sync-simulate").click();
      await page.waitForFunction(() => window.__AP01_PLAN_COUNT__ === 1);
      await page.locator('#sync-compare-group [value="ContentHash"]').check();
      await page.waitForTimeout(200);
      if (destination === "Dateiansicht") await page.keyboard.press("Escape");
      else await page.getByLabel("In beide Richtungen", { exact: true }).check();
      await page.evaluate(() => window.__AP01_RELEASE_PLAN__());
      await page.waitForTimeout(250);
      assert.equal(await page.evaluate(() => window.__AP01_PLAN_COUNT__), 1);
    });
  }

  await runCase("Sync-Modus öffnet über Menü, plant live und schließt per Escape", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    const view = page.locator("#sync-view");
    await view.waitFor({ state: "visible" });
    // Panes sind im Sync-Modus verdeckt; die Chips zeigen Quelle und Ziel.
    assert.equal(await page.locator(".panes").isVisible(), false);
    assert.match(await page.locator("#sync-source-path").innerText(), /quelle/);
    assert.match(await page.locator("#sync-target-path").innerText(), /ziel/);
    // Lokale Endpunkte planen automatisch; die Bilanz erscheint ohne Klick.
    await page.locator("#sync-summary .pill").first().waitFor();
    // Richtungswechsel tauscht die Rollen und plant erneut.
    await page.getByLabel("Von rechts nach links").check();
    assert.match(await page.locator("#sync-source-path").innerText(), /ziel/);
    // „Verwaiste in Quarantäne verschieben" schließt Aktualisieren sichtbar ein.
    await page.getByLabel("Verwaiste in Quarantäne verschieben").check();
    assert.equal(await page.getByLabel("Vorhandenes aktualisieren").isChecked(), true);
    assert.equal(await page.getByLabel("Vorhandenes aktualisieren").isDisabled(), true);
    await page.keyboard.press("Escape");
    await view.waitFor({ state: "hidden" });
    assert.equal(await page.locator(".panes").isVisible(), true);
  });

  await runCase("Sync ignoriert alte Planantwort nach Optionswechsel", "", async page => {
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      let calls = 0;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command !== "plan_sync") return result;
        result.copy_count = ++calls === 1 ? 111 : 222;
        if (calls === 1) await new Promise(resolve => { window.__RELEASE_PLAN__ = resolve; });
        return result;
      };
    });
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.waitForFunction(() => typeof window.__RELEASE_PLAN__ === "function");
    await page.getByLabel("Verwaiste in Quarantäne verschieben").check();
    await page.locator("#sync-summary").getByText("222 kopieren", { exact: true }).waitFor();
    await page.evaluate(async () => {
      window.__RELEASE_PLAN__();
      await new Promise(resolve => setTimeout(resolve, 0));
    });
    assert.match(await page.locator("#sync-summary").innerText(), /222 kopieren/);
    assert.doesNotMatch(await page.locator("#sync-summary").innerText(), /111 kopieren/);
  });

  await runCase("Sync startet nach Schließen und Navigation keinen alten lokalen Auftrag", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#sync-summary .pill").first().waitFor();
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__LOCAL_RUNS__ = [];
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command === "run_sync") window.__LOCAL_RUNS__.push(args);
        if (command === "plan_sync") await new Promise(resolve => { window.__RELEASE_START__ = resolve; });
        return result;
      };
    });
    await page.locator("#btn-sync-simulate").click();
    await page.waitForFunction(() => typeof window.__RELEASE_START__ === "function");
    await page.locator("#btn-sync-close").click();
    const location = page.locator('.pane[data-side="left"] .path');
    await location.fill("/mock/fast");
    await location.press("Enter");
    await page.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    await page.evaluate(async () => {
      window.__RELEASE_START__();
      await new Promise(resolve => setTimeout(resolve, 0));
    });
    assert.deepEqual(await page.evaluate(() => window.__LOCAL_RUNS__), []);
  });

  await runCase("Sync zeigt vollständige lange Wurzeln ohne abgeschnittene Pfade", "", async page => {
    const root = `/mock/${"langer-verzeichnisname-".repeat(8)}`;
    for (const side of ["left", "right"]) {
      const location = page.locator(`.pane[data-side="${side}"] .path`);
      await location.fill(`${root}/${side}`);
      await location.press("Enter");
      await page.waitForFunction(path => window.__MOCK_SCAN_CALLS__().some(call => call.path === path), `${root}/${side}`);
    }
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    await page.locator("#btn-sync").click();
    const dialog = page.getByRole("dialog", { name: /Bidirektional ·/ });
    await dialog.waitFor();
    assert.deepEqual(await dialog.locator(".sync-bound-roots code").allTextContents(),
      [`${root}/left`, `${root}/right`]);
    const geometry = await dialog.locator("#modal-body").evaluate(element => ({
      width: element.clientWidth, content: element.scrollWidth,
    }));
    assert.ok(geometry.content <= geometry.width + 1, `Wurzeln ragen aus dem Dialog: ${JSON.stringify(geometry)}`);
    if (process.env.DOPPELDECKER_SCREENSHOT_DIR) {
      await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/sync-long-roots.png` });
    }
    await dialog.getByRole("button", { name: "Abbrechen", exact: true }).click();
  });

  await runCase("Sync verwirft alte bidirektionale Rollen vor einem neuen Konfliktdialog", "?bidirectionalConflict=1", async page => {
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      let held = false;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command === "prepare_bidirectional_sync" && !held) {
          held = true;
          await new Promise(resolve => { window.__RELEASE_BIDI__ = resolve; });
        }
        return result;
      };
    });
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    await page.locator("#btn-sync").click();
    await page.waitForFunction(() => typeof window.__RELEASE_BIDI__ === "function");
    assert.equal(await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__().pendingBidirectional), true);
    await page.locator("#btn-sync-close").click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "swap_panes"));
    await page.waitForFunction(() => document.querySelector('.pane[data-side="left"] .path').value === "/mock/ziel");
    await page.evaluate(() => window.__RELEASE_BIDI__());
    await page.waitForFunction(() => !window.__MOCK_PREPARATION_ACTIONS__().pendingBidirectional, null, { timeout: 3000 });
    assert.equal(await page.getByRole("dialog", { name: "Konflikt 1 von 1" }).count(), 0);
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Konflikt 1 von 1" })
      .getByRole("button", { name: "Links behalten" }).click();
    const dialog = page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /Links:.*\/mock\/ziel/s);
    const calls = (await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__())).calls;
    const last = calls.filter(call => call.command === "prepare_bidirectional_sync").at(-1);
    assert.equal(last.args.request.resolutions[0].decision.decision, "keep_right");
    if (process.env.DOPPELDECKER_SCREENSHOT_DIR) {
      await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/bidirectional-bound-roots.png` });
    }
    await dialog.getByRole("button", { name: "Abbrechen" }).click();
  });

  await runCase("Sync erhält neu geöffnete Ansicht nach verspäteter Enqueue-Antwort", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#sync-summary .pill").first().waitFor();
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command === "run_sync") await new Promise(resolve => { window.__RELEASE_ENQUEUE__ = resolve; });
        return result;
      };
    });
    await page.locator("#btn-sync-simulate").click();
    await page.waitForFunction(() => typeof window.__RELEASE_ENQUEUE__ === "function");
    await page.locator("#btn-sync-close").click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.evaluate(async () => {
      window.__RELEASE_ENQUEUE__();
      await new Promise(resolve => setTimeout(resolve, 0));
    });
    assert.equal(await page.locator("#sync-view").isVisible(), true);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 1);
  });

  await runCase("Sync startet Realmodus bei Doppelklick nur einmal", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#sync-summary .pill").first().waitFor();
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__REAL_PREPARATIONS__ = 0;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command === "prepare_real_sync") {
          window.__REAL_PREPARATIONS__++;
          if (window.__REAL_PREPARATIONS__ === 1) {
            await new Promise(resolve => { window.__RELEASE_REAL__ = resolve; });
          }
        }
        return result;
      };
      document.querySelector("#btn-real").click();
      document.querySelector("#btn-real").click();
    });
    await page.waitForFunction(() => typeof window.__RELEASE_REAL__ === "function");
    await page.evaluate(() => window.__RELEASE_REAL__());
    await page.getByRole("dialog", { name: "Lokalen Realmodus bestätigen" })
      .getByRole("button", { name: "Vorbereitung verwerfen" }).click();
    await page.evaluate(() => new Promise(resolve => setTimeout(resolve, 25)));
    assert.equal(await page.evaluate(() => window.__REAL_PREPARATIONS__), 1);
    assert.equal(await page.getByRole("dialog", { name: "Lokalen Realmodus bestätigen" }).count(), 0);
  });

  await runCase("Sync serialisiert alte Remoteantwort und Discard vor neuer Vorbereitung", "?remoteFile=1", async page => {
    await page.locator('.pane[data-side="right"] .listing').click();
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      let held = false;
      window.__REMOTE_PREPARATION_ORDER__ = [];
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "prepare_ftps_sync" || command === "discard_ftps_sync") {
          window.__REMOTE_PREPARATION_ORDER__.push(command);
        }
        const result = await invoke(command, args);
        if (command === "prepare_ftps_sync" && !held) {
          held = true;
          await new Promise(resolve => { window.__RELEASE_REMOTE__ = resolve; });
        }
        return result;
      };
    });
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#sync-preview-remote").click();
    await page.waitForFunction(() => typeof window.__RELEASE_REMOTE__ === "function");
    await page.locator("#btn-sync-close").click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#sync-preview-remote").click();
    await page.evaluate(() => window.__RELEASE_REMOTE__());
    await page.waitForFunction(() => window.__REMOTE_PREPARATION_ORDER__.filter(name => name === "prepare_ftps_sync").length === 2);
    await page.locator("#real-mode-banner").waitFor({ state: "visible" });
    assert.deepEqual(await page.evaluate(() => window.__REMOTE_PREPARATION_ORDER__), [
      "prepare_ftps_sync", "discard_ftps_sync", "prepare_ftps_sync",
    ]);
    assert.doesNotMatch(await page.locator("#dock-summary").innerText(), /noch belegt|busy/i);
  });

  await runCase("Planvergleich zeigt Baumkontext, beide Seiten und Wirkung am Ziel", "?comparisonMixed", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    const list = page.getByRole("listbox", { name: "Geänderte Einträge im Planvergleich" });
    await list.getByRole("option").first().waitFor();
    assert.equal(await list.getByRole("option").count(), 5);
    assert.match(await page.locator("#sync-comparison-categories").innerText(),
      /2 hinzufügen · 1 aktualisieren · 1 Quarantäne · 1 ausgelassen/);

    const added = list.getByRole("option", { name: /Fotos\/neu\.jpg/ });
    assert.match(await added.locator('[data-side="source"]').innerText(), /Fotos[\s\S]*neu\.jpg/);
    assert.match(await added.locator('[data-side="target"]').innerText(), /Nicht vorhanden[\s\S]*＋/);
    assert.equal(await added.locator('[data-side="target"]').getAttribute("class"), "sync-diff-side ghost");

    const updated = list.getByRole("option", { name: /aktuell\.txt/ });
    assert.match(await updated.locator('[data-side="source"]').innerText(), /8 B/);
    assert.match(await updated.locator('[data-side="target"]').innerText(), /4 B[\s\S]*⟳/);

    const removed = list.getByRole("option", { name: /alt\.txt/ });
    assert.match(await removed.locator('[data-side="source"]').innerText(), /Nicht vorhanden/);
    assert.match(await removed.locator('[data-side="target"]').innerText(), /Quarantäne/);

    await list.focus();
    assert.notEqual(await list.evaluate(element => getComputedStyle(element).boxShadow), "none",
      "Tastaturfokus im Planvergleich muss sichtbar sein.");
    await page.keyboard.press("End");
    const selected = list.locator('[role="option"][aria-selected="true"]');
    assert.match(await selected.getAttribute("aria-label"), /konflikt: Sicher auslassen, keine Seite wird verändert/);
    assert.equal(await selected.getAttribute("aria-setsize"), "5");
  });

  await runCase("Planvergleich hält pfadgenaue Metadatenauslassungen sichtbar", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    const row = page.getByRole("option", { name: /Programm/ });
    await row.waitFor();
    assert.match(await row.locator(".sync-diff-issue").innerText(),
      /Extended Attributes: Quelle ist lesbar, Ziel kann dieses Merkmal nicht exakt darstellen/);
  });

  await runCase("Planvergleich virtualisiert große Änderungen", "?comparisonRows=5000", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    const list = page.getByRole("listbox", { name: "Geänderte Einträge im Planvergleich" });
    await list.getByRole("option").first().waitFor();
    assert.ok(await list.getByRole("option").count() < 100, "Zu viele Vergleichszeilen liegen gleichzeitig im DOM.");
    assert.equal(await list.getAttribute("data-virtualized"), "true");
    await list.focus();
    await page.keyboard.press("End");
    const selected = list.locator('[role="option"][aria-selected="true"]');
    assert.match(await selected.innerText(), /Datei-05000\.txt/);
    assert.equal(await selected.getAttribute("aria-posinset"), "5000");
    assert.equal(await selected.getAttribute("aria-setsize"), "5000");
  });

  await runCase("Virtualisierte Pane behält die aktive semantische Option beim Wegscrollen", "?entries=5000", async page => {
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.locator('[role="option"]').first().waitFor();
    const activeId = await list.getAttribute("aria-activedescendant");
    assert.ok(activeId, "Die geladene Pane braucht einen aktiven Nachfahren.");
    await list.evaluate(element => {
      element.scrollTop = element.scrollHeight;
      element.dispatchEvent(new Event("scroll"));
    });
    await page.waitForFunction(id => {
      const listbox = document.querySelector('.pane[data-side="left"] .listing');
      const active = id ? document.getElementById(id) : null;
      return listbox?.getAttribute("aria-activedescendant") === id
        && active?.getAttribute("role") === "option"
        && active.classList.contains("virtual-active-option");
    }, activeId);
    assert.equal(await page.locator(`#${activeId}`).count(), 1,
      "Die semantische Ersatzoption darf keine doppelte ID erzeugen.");
    assert.equal(await page.locator(`#${activeId}`).getAttribute("aria-posinset"), "1");
    assert.equal(await page.locator(`#${activeId}`).getAttribute("aria-setsize"), "5000");
    assert.equal(await page.locator(`#${activeId}`).getAttribute("style"), null,
      "Die CSP-sichere Ersatzoption verwendet ausschließlich ihre CSS-Klasse.");

    await list.focus();
    await page.keyboard.press("Escape");
    assert.equal(await page.locator(`#${activeId}`).getAttribute("aria-selected"), "false",
      "Escape muss auch die wegvirtualisierte aktive Option semantisch abwählen.");
    await page.keyboard.press("Control+A");
    assert.equal(await page.locator(`#${activeId}`).getAttribute("aria-selected"), "true",
      "Cmd/Ctrl+A muss die wegvirtualisierte aktive Option semantisch auswählen.");

    await page.keyboard.press("End");
    const movedId = await list.getAttribute("aria-activedescendant");
    assert.notEqual(movedId, activeId);
    assert.equal(await page.locator(`#${movedId}`).count(), 1);
    assert.equal(await page.locator(`#${movedId}`).getAttribute("role"), "option");
    assert.equal(await page.locator(`#${movedId}`).getAttribute("aria-posinset"), "5000");
    assert.equal(await list.locator(".virtual-active-option").count(), 0,
      "Sobald die aktive Zeile sichtbar ist, darf keine semantische Dublette bleiben.");
  });

  await runCase("Klick nach manuellem Wegscrollen entfernt die alte semantische Ersatzoption", "?entries=5000", async page => {
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.locator('[role="option"]').first().waitFor();
    const oldId = await list.getAttribute("aria-activedescendant");
    await list.evaluate(element => {
      element.scrollTop = element.scrollHeight;
      element.dispatchEvent(new Event("scroll"));
    });
    await list.locator(".virtual-active-option").waitFor();
    await list.locator(".row").last().click();
    const activeId = await list.getAttribute("aria-activedescendant");
    assert.notEqual(activeId, oldId);
    assert.equal(await page.locator(`#${oldId}`).count(), 0,
      "Der alte Cursor darf nicht als zweite Options-ID im DOM bleiben.");
    assert.equal(await page.locator(`#${activeId}`).count(), 1);
    assert.equal(await page.locator(`#${activeId}`).getAttribute("role"), "option");
    assert.equal(await page.locator(`#${activeId}`).getAttribute("aria-selected"), "true");
    assert.equal(await list.locator('[role="option"][aria-selected="true"]').count(), 1);
  });

  await runCase("Realmodusbestätigung überlebt die zuvor geplante automatische Vorschau", "", async page => {
    await page.clock.install({ time: new Date("2026-10-19T12:00:00Z") });
    await page.clock.pauseAt(new Date("2026-10-19T12:00:01Z"));
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByRole("button", { name: "Realmodus" }).dispatchEvent("click");
    const dialog = page.getByRole("dialog", { name: "Lokalen Realmodus bestätigen" });
    await dialog.waitFor();
    await page.clock.runFor(200);
    await page.clock.resume();
    await dialog.locator("#real-phrase").fill("REAL ziel K7M4Q2");
    await dialog.getByRole("button", { name: "Einmalig real starten" }).click();
    await page.waitForFunction(() => document.documentElement.dataset.realConfirmation === "REAL ziel K7M4Q2", null, { timeout: 3000 });
  });

  await runCase("Realmodus-Phrasengate und Modal-Fokus", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    const trigger = page.getByRole("button", { name: "Realmodus" });
    await trigger.click();
    const dialog = page.getByRole("dialog", { name: "Lokalen Realmodus bestätigen" });
    await dialog.waitFor();
    const phrase = dialog.locator("#real-phrase");
    const start = dialog.getByRole("button", { name: "Einmalig real starten" });
    assert.equal(await start.isDisabled(), true);
    await phrase.fill("falsch");
    assert.equal(await start.isDisabled(), true);
    await phrase.fill("REAL ziel K7M4Q2");
    assert.equal(await start.isEnabled(), true);
    await phrase.press("Escape");
    assert.equal(await page.evaluate(() => document.activeElement?.id), await trigger.getAttribute("id"));
    await trigger.click();
    await dialog.waitFor();
    await dialog.locator("#real-phrase").fill("REAL ziel K7M4Q2");
    await dialog.getByRole("button", { name: "Einmalig real starten" }).click();
    await page.waitForFunction(() => document.documentElement.dataset.realConfirmation === "REAL ziel K7M4Q2");
    assert.equal(await page.locator("html").getAttribute("data-real-confirmation"), "REAL ziel K7M4Q2");
  });

  await runCase("SFTP-Realmodus bindet Remote-Identität und Einmalphrase", "", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#btn-sync").click();
    const execution = page.getByRole("dialog", { name: "SFTP-Ausführung" });
    await execution.waitFor();
    await execution.getByRole("button", { name: "Direkt bestätigen" }).click();
    const confirmation = page.getByRole("dialog", { name: "SFTP-Realmodus bestätigen" });
    await confirmation.waitFor();
    assert.match(await confirmation.innerText(), /Hostkey .* Sitzung 1/);
    await confirmation.locator("#real-phrase").fill("REAL remote K7M4Q2");
    await confirmation.getByRole("button", { name: "Einmalig real starten" }).click();
    assert.equal(await page.locator("html").getAttribute("data-sftp-confirmation"), "REAL remote K7M4Q2");
    await page.locator("#dock-summary .pill.ok").filter({ hasText: "Sync ausgeführt" }).waitFor();
  });

  await runCase("Remote-zu-Remote nutzt generische Provider-Queue", "", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#btn-sync").click();
    const execution = page.getByRole("dialog", { name: "REMOTE↔REMOTE-Ausführung" });
    await execution.waitFor();
    assert.match(await execution.innerText(), /Plan ist vollständig gebunden/);
    await execution.getByRole("button", { name: "In Queue" }).click();
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    await page.locator("#btn-queue").click();
    assert.match(await page.locator("#queue-popover").innerText(), /Provider AddOnly · Sftp .* → WebDav/s);
  });

  await runCase("Provider-Enqueuefehler behält exakt dieselbe Vorbereitung für Retry", "?providerEnqueueFailsOnce=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));

    await page.locator("#btn-sync").click();
    let execution = page.getByRole("dialog", { name: "REMOTE↔REMOTE-Ausführung" });
    await execution.getByRole("button", { name: "In Queue" }).click();
    await page.getByText(/Provider-Mock-Enqueue ist vor dem Commit belegt/).waitFor();

    await page.locator("#btn-sync").click();
    execution = page.getByRole("dialog", { name: "REMOTE↔REMOTE-Ausführung" });
    await execution.getByRole("button", { name: "In Queue" }).click();
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    const state = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
    assert.deepEqual(state.calls.map(call => call.command), [
      "prepare_provider_sync",
      "enqueue_provider_sync",
      "enqueue_provider_sync",
    ]);
    assert.equal(state.pendingProvider, false);
  });

  await runCase("Remote-zu-Remote verbraucht direkt eine Provider-Einmalphrase", "", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#btn-sync").click();
    const execution = page.getByRole("dialog", { name: "REMOTE↔REMOTE-Ausführung" });
    await execution.getByRole("button", { name: "Direkt bestätigen" }).click();
    const confirmation = page.getByRole("dialog", { name: "REMOTE↔REMOTE-Realmodus bestätigen" });
    await confirmation.locator("#real-phrase").fill("REAL provider K7M4Q2");
    await confirmation.getByRole("button", { name: "Einmalig real starten" }).click();
    assert.equal(await page.locator("html").getAttribute("data-provider-confirmation"), "REAL provider K7M4Q2");
    await page.locator("#dock-summary .pill.ok").filter({ hasText: "Sync ausgeführt" }).waitFor();
  });

  await runCase("Provider-Startfehler behält Protokoll und Vorbereitung für Retry", "?providerRunFailsOnce=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));

    for (let attempt = 0; attempt < 2; attempt += 1) {
      await page.locator("#btn-sync").click();
      const execution = page.getByRole("dialog", { name: "REMOTE↔REMOTE-Ausführung" });
      await execution.getByRole("button", { name: "Direkt bestätigen" }).click();
      const confirmation = page.getByRole("dialog", { name: "REMOTE↔REMOTE-Realmodus bestätigen" });
      await confirmation.locator("#real-phrase").fill("REAL provider K7M4Q2");
      await confirmation.getByRole("button", { name: "Einmalig real starten" }).click();
      if (attempt === 0) {
        await page.getByText(/Provider-Mock-Start ist vor der Entnahme belegt/).waitFor();
      }
    }
    await page.locator("#dock-summary .pill.ok").filter({ hasText: "Sync ausgeführt" }).waitFor();
    const state = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
    assert.deepEqual(state.calls.map(call => call.command), [
      "prepare_provider_sync",
      "run_provider_sync",
      "run_provider_sync",
    ]);
    assert.equal(state.pendingProvider, false);
  });

  for (const theme of ["light", "dark"]) {
    await runCase(`AP05 WebDAV-Ordnervertrag ist vor dem Namen sichtbar: ${theme}`, "", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme, false); }, theme);
      await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
      await page.getByRole("listbox", { name: /Linke Dateiliste/ }).focus();
      await page.evaluate(() => { window.__MOCK_EMIT__("menu", "new_folder"); });
      const dialog = page.getByRole("dialog", { name: "Neuer Ordner" }); await dialog.waitFor();
      const limitations = dialog.getByRole("textbox", { name: "Wirkung und Einschränkungen", exact: true });
      assert.match(await limitations.inputValue(), /Best Effort.*bedingte Ordneranlage.*Readback.*Löschprobe/s);
      assert.match(await limitations.inputValue(), /unmittelbaren Elternordner.*Vorfahrenwechsel/s);
      await limitations.focus();
      await limitations.press(process.platform === "darwin" ? "Meta+A" : "Control+A");
      assert.equal(await limitations.evaluate(node => node.selectionEnd - node.selectionStart), (await limitations.inputValue()).length);
      assert.equal(await dialog.getByRole("button", { name: "OK", exact: true }).isVisible(), true);
      await page.locator("#modal-input").fill("Grüße %2F"); await dialog.getByRole("button", { name: "OK", exact: true }).click();
      await page.waitForFunction(() => document.documentElement.dataset.lastMkdir);
      const request = await page.evaluate(() => JSON.parse(document.documentElement.dataset.lastMkdir));
      assert.equal(request.expectedExecutionClass, "best_effort"); assert.equal(request.name, "Grüße %2F");
      await page.waitForFunction(() => (document.querySelector("#dock-summary")?.innerText || "").includes("Ordner angelegt"));
      assert.match(await page.locator("#dock-summary").innerText(), /Best Effort.*Readback/);
    });
    await runCase(`AP05 lange Recoverydiagnose bleibt vollständig aufklappbar: ${theme}`, "", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js"); setTheme(theme, false);
        window.__AP05_ERROR__ = "Abbruch nach neuer Ordneranlage; Zustand prüfen. " + "Prüfpfad /mock/ziel/LangerName_".repeat(100) + " <script>unerwünscht</script>";
        window.__MOCK_EMIT__("fileop-error", { code: "outcome_unclear", message: window.__AP05_ERROR__ });
      }, theme);
      const dock = page.locator("#dock-summary"); await dock.getByText("Ausgang unklar", { exact: true }).waitFor();
      const details = dock.locator("details"); assert.equal(await details.getAttribute("open"), null);
      assert.equal(await dock.evaluate(node => node.getBoundingClientRect().height < 180), true);
      await details.locator("summary").click();
      assert.equal(await details.locator(".operation-error-body").textContent(), await page.evaluate(() => window.__AP05_ERROR__));
      assert.equal(await details.locator("script").count(), 0);
      assert.equal(await details.locator(".operation-error-body").evaluate(node => node.scrollHeight > node.clientHeight), true);
      assert.equal(await page.locator("#btn-dock-close").isVisible(), true);
    });
    await runCase(`AP05 unklarer Ordnerausgang bleibt sichtbar ohne Retry: ${theme}`, "?mkdirOutcomeUnclear=1", async page => {
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme, false); }, theme);
      await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
      await page.getByRole("listbox", { name: /Linke Dateiliste/ }).focus();
      await page.evaluate(() => { window.__MOCK_EMIT__("menu", "new_folder"); });
      await page.locator("#modal-input").fill("Unklar"); await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK", exact: true }).click();
      await page.waitForFunction(() => (document.querySelector("#dock-summary")?.innerText || "").includes("unklar"));
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MKDIR_CALLS__), 1);
      assert.doesNotMatch(await page.locator("#dock-summary").innerText(), /Ordner angelegt/);
    });
  }
  await runCase("AP05 Backendabsage öffnet keinen Namensdialog und schreibt nichts", "?sftpMkdirBlocked=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).focus();
    await page.evaluate(() => { window.__MOCK_EMIT__("menu", "new_folder"); });
    await page.waitForFunction(() => (document.querySelector("#dock-summary")?.innerText || "").includes("SFTP-Elternbindung fehlt"));
    assert.equal(await page.getByRole("dialog", { name: "Neuer Ordner" }).isVisible(), false);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MKDIR_CALLS__ || 0), 0);
  });
  await runCase("AP05 Navigation verwirft verspäteten Ordnervertrag", "?mkdirContractDelay=150", async page => {
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).focus();
    await page.evaluate(() => { window.__MOCK_EMIT__("menu", "new_folder"); });
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    await page.waitForTimeout(220);
    assert.equal(await page.getByRole("dialog", { name: "Neuer Ordner" }).isVisible(), false);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MKDIR_CALLS__ || 0), 0);
  });

  await runCase("Ordner anlegen nutzt für alle Remote-Panes denselben Provider-Command", "", async page => {
    const providers = [
      ["sftp", "mock_sftp", 11],
      ["ftps", "mock_ftps", 21],
      ["webdav", "mock_webdav", 31],
      ["smb", "mock_smb", 41],
    ];
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    for (const [provider, profileId, profileRevision] of providers) {
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
      await list.focus();
      // Kein Promise zurückgeben: Die Aktion wartet selbst auf den Dialog, den der
      // Test erst in den folgenden Zeilen beantwortet.
      await page.evaluate(() => { window.__MOCK_EMIT__("menu", "new_folder"); });
      const name = `Neuer ${provider.toUpperCase()}-Ordner`;
      await page.locator("#modal-input").fill(name);
      await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK" }).click();

      const mkdir = await page.evaluate(() => (
        JSON.parse(document.documentElement.dataset.lastMkdir || "null")
      ));
      assert.deepEqual(mkdir, {
        command: "create_provider_folder",
        endpoint: { provider, profileId, profileRevision, root: "/" },
        parent: "/",
        name,
        path: `/${name}`,
        expectedExecutionClass: provider === "webdav" ? "best_effort" : "assured",
      });
    }
  });

  async function nativeDrop(page, side, paths, type = "drop") {
    await page.evaluate(({ side, paths, type }) => {
      const rect = document.querySelector(`.pane[data-side="${side}"] .listing`).getBoundingClientRect();
      window.__MOCK_EMIT__(`tauri://drag-${type}`, {
        paths, position: { x: rect.x + rect.width / 2, y: rect.y + 80 },
      });
    }, { side, paths, type });
  }

  await runCase("Drag-and-drop kopiert Dateien zwischen Panes", "", async page => {
    const source = page.locator('.pane[data-side="left"] .row').filter({ hasText: "Beispiel.txt" });
    const target = page.locator('.pane[data-side="right"] .listing');
    assert.equal(await source.getAttribute("draggable"), "true");
    await source.dragTo(target);
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    const operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation);
    assert.equal(operation.kind, "Copy");
    assert.equal(operation.items.length, 1);
    assert.match(operation.items[0], /Beispiel\.txt$/);
    // Zweite native Meldung derselben internen Geste darf nichts duplizieren.
    await nativeDrop(page, "right", []);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 1);
  });

  await runCase("Drag-and-drop Finder nach FTPS nutzt lokale Quelle und Konfliktdialog", "?remoteFile=1&conflicts=provider-replace", async page => {
    await page.locator('.pane[data-side="right"] .listing').click();
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    const paths = ["/synthetic/finder/Beispiel.txt", "/synthetic/finder/literal%2F.txt"];
    await nativeDrop(page, "right", paths, "enter");
    assert.equal(await page.locator('.pane[data-side="right"]').evaluate(el => el.classList.contains("file-drop-target")), true);
    await nativeDrop(page, "right", paths);
    const dialog = page.getByRole("dialog", { name: "Konflikte beim Kopieren" });
    await dialog.waitFor();
    await dialog.getByRole("button", { name: "Überspringen" }).click();
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    const operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation);
    assert.equal(operation.type, "ProviderFile");
    assert.equal(operation.kind, "Copy");
    assert.equal(operation.source.provider, "Local");
    assert.equal(operation.source.root, "/synthetic/finder");
    assert.equal(operation.target.provider, "Ftps");
    assert.deepEqual(operation.items, paths);
    assert.equal(operation.overwrite, false);
  });

  await runCase("Drag-and-drop Finder erhält Sperrgrund und Gruppenbilanz", "?remoteFile=1", async page => {
    await page.locator('.pane[data-side="right"] .listing').click();
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command === "check_provider_file_operation") {
          result.targetCapabilities.copyNew = false;
          result.targetCapabilities.copyNewBestEffort = false;
          result.targetCapabilities.blockers = ["Testserver ist schreibgeschützt"];
        }
        return result;
      };
    });
    await nativeDrop(page, "right", ["/synthetic/a/one.txt", "/synthetic/b/two.txt"]);
    await page.locator("#dock-summary").filter({ hasText: "2 von 2 Quellordnern" }).waitFor();
    assert.match(await page.locator("#dock-summary").innerText(), /Testserver ist schreibgeschützt/);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });

  await runCase("Drag-and-drop Finder erhält Providerfehler nach erster Gruppe", "?remoteFile=1", async page => {
    await page.locator('.pane[data-side="right"] .listing').click();
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "enqueue_provider_file_operation" && args.request.source.root === "/synthetic/b") {
          throw { code: "permission_denied", message: "Schreiben vom Server verweigert" };
        }
        return invoke(command, args);
      };
    });
    await nativeDrop(page, "right", ["/synthetic/a/one.txt", "/synthetic/b/two.txt", "/synthetic/c/three.txt"]);
    await page.locator("#dock-summary").filter({ hasText: "2 von 3 Quellordnern" }).waitFor();
    assert.match(await page.locator("#dock-summary").innerText(), /Schreiben vom Server verweigert/);
    const entries = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries);
    assert.equal(entries.length, 1);
    assert.deepEqual(entries[0].operation.items, ["/synthetic/a/one.txt"]);
  });

  await runCase("Drag-and-drop Finder verteilt mehrere Quellordner und trifft Retina-Pane", "", async page => {
    await page.evaluate(() => {
      document.documentElement.dataset.platform = "macos";
      Object.defineProperty(window, "devicePixelRatio", { value: 2 });
    });
    await nativeDrop(page, "right", ["/synthetic/a/one.txt", "/synthetic/b/two.txt"]);
    await page.locator("#queue-count").filter({ hasText: "2" }).waitFor();
    const operations = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.map(e => e.operation));
    assert.deepEqual(operations.map(o => o.items), [["/synthetic/a/one.txt"], ["/synthetic/b/two.txt"]]);
    const target = await page.locator('.pane[data-side="right"] .path').inputValue();
    assert(operations.every(o => o.destination === target));
  });

  await runCase("Drag-and-drop Finder meldet vollständig verworfene Pfade", "", async page => {
    await nativeDrop(page, "right", ["relativ.txt", "/synthetic/ordner/"]);
    await page.locator("#dock-summary").filter({ hasText: "keine regulären absoluten Dateipfade" }).waitFor();
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });

  await runCase("Drag-and-drop native interne Mehrfachauswahl nach FTPS", "?remoteFile=1&keyboard=1", async page => {
    await page.locator('.pane[data-side="right"] .listing').click();
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    const left = page.locator('.pane[data-side="left"] .listing');
    await left.locator(".row").first().click();
    await left.focus();
    await page.keyboard.press("ControlOrMeta+a");
    const selected = await left.locator('[aria-selected="true"]').count();
    assert(selected > 1);
    await page.evaluate(() => {
      const row = document.querySelector('.pane[data-side="left"] .row');
      const dataTransfer = new DataTransfer();
      row.dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer }));
      row.dispatchEvent(new DragEvent("dragend", { bubbles: true, dataTransfer }));
    });
    await nativeDrop(page, "right", []);
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    const operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation);
    assert.equal(operation.items.length, selected);
    assert.equal(operation.target.provider, "Ftps");
  });

  await runCase("Drag-and-drop Abbrechen kopiert keine weiteren Quellordner", "?conflicts=replace", async page => {
    await nativeDrop(page, "right", ["/synthetic/a/one.txt", "/synthetic/b/two.txt"]);
    const dialog = page.getByRole("dialog", { name: "Konflikte beim Kopieren" });
    await dialog.waitFor();
    // Während des Dialogs darf ein weiterer Drop die gebundene Aktion nicht ersetzen.
    await nativeDrop(page, "left", ["/synthetic/other.txt"]);
    await dialog.getByRole("button", { name: "Abbrechen" }).click();
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
    assert.equal(await dialog.isVisible(), false);
    await page.locator("#dock-summary").filter({ hasText: "2 von 2 Quellordnern" }).waitFor();
  });

  await runCase("Drag-and-drop Finder überlebt den Abschluss der ersten Quellordner-Gruppe", "?conflicts=replace", async page => {
    await nativeDrop(page, "right", ["/synthetic/a/one.txt", "/synthetic/b/two.txt"]);
    const dialog = page.getByRole("dialog", { name: "Konflikte beim Kopieren" });
    await dialog.waitFor();
    await dialog.getByRole("button", { name: "Überspringen" }).click();
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    // Die zweite Gruppe wartet im Konfliktdialog. Derweil meldet die Queue die
    // erste als fertig; beide Panes laden neu. Die Drop-Aktion darf dadurch nicht
    // still entwertet werden.
    await dialog.waitFor();
    const scanCount = (await page.evaluate(() => window.__MOCK_SCAN_CALLS__())).length;
    await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", {
      files: 1, skipped: 0, symlinks_skipped: 0, bytes: 8, cancelled: false,
    }));
    await page.waitForFunction(expected => window.__MOCK_SCAN_CALLS__().length >= expected, scanCount + 2);
    await dialog.getByRole("button", { name: "Überspringen" }).click();
    await page.locator("#queue-count").filter({ hasText: "2" }).waitFor();
    const operations = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.map(e => e.operation));
    assert.deepEqual(operations.map(o => o.items), [["/synthetic/a/one.txt"], ["/synthetic/b/two.txt"]]);
    assert.doesNotMatch(await page.locator("#dock-summary").innerText(), /unvollständig/);
  });

  await runCase("Drag-and-drop Finder überspringt den Zielordner selbst und reiht die übrigen ein", "", async page => {
    const target = await page.locator('.pane[data-side="right"] .path').inputValue();
    await nativeDrop(page, "right", [`${target}/eigene.txt`, "/synthetic/b/two.txt"]);
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    const operations = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.map(e => e.operation));
    assert.deepEqual(operations.map(o => o.items), [["/synthetic/b/two.txt"]]);
    assert.doesNotMatch(await page.locator("#dock-summary").innerText(), /unvollständig/);
  });

  await runCase("Drag-and-drop bilanziert erfolgreiches Enqueue trotz Zielnavigation", "", async page => {
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command === "copy_paths") {
          await new Promise(resolve => { window.__REVIEW_RELEASE_COPY__ = resolve; });
        }
        return result;
      };
    });
    await nativeDrop(page, "right", ["/synthetic/a/one.txt", "/synthetic/b/two.txt"]);
    await page.waitForFunction(() => typeof window.__REVIEW_RELEASE_COPY__ === "function");
    const targetPath = page.locator('.pane[data-side="right"] .path');
    await targetPath.fill("/synthetic/changed");
    await targetPath.press("Enter");
    await page.waitForFunction(() => window.__MOCK_SCAN_CALLS__().some(call => call.path === "/synthetic/changed"));
    await page.evaluate(() => window.__REVIEW_RELEASE_COPY__());
    await page.locator("#dock-summary").filter({ hasText: "Drop unvollständig" }).waitFor();
    assert.match(await page.locator("#dock-summary").innerText(), /1 von 2 Quellordnern wurden nicht eingereiht/);
    const entries = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries);
    assert.equal(entries.length, 1);
    assert.deepEqual(entries[0].operation.items, ["/synthetic/a/one.txt"]);
    assert.notEqual(entries[0].operation.destination, await targetPath.inputValue());
  });

  await runCase("Drag-and-drop Finder meldet nicht eingereihte Quellordner sichtbar", "?conflicts=replace", async page => {
    await nativeDrop(page, "right", ["/synthetic/a/one.txt", "/synthetic/b/two.txt"]);
    const dialog = page.getByRole("dialog", { name: "Konflikte beim Kopieren" });
    await dialog.waitFor();
    await dialog.getByRole("button", { name: "Überspringen" }).click();
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    await dialog.waitFor();
    await dialog.getByRole("button", { name: "Abbrechen" }).click();
    await page.locator("#dock-summary").filter({ hasText: "1 von 2 Quellordnern" }).waitFor();
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 1);
  });

  await runCase("Sitzungsstand speichert nur Endpunkt, Pfad und die wiederhergestellte aktive Pane", "?restoredSide=right", async page => {
    await page.waitForFunction(() => window.__MOCK_DESKTOP__().rememberedWindows.length >= 1);
    const [first] = await page.evaluate(() => window.__MOCK_DESKTOP__().rememberedWindows);
    assert.deepEqual(Object.keys(first.panes.left).sort(), ["path", "profileId", "profileRevision", "provider"]);
    assert.deepEqual(Object.keys(first.panes.right).sort(), ["path", "profileId", "profileRevision", "provider"]);
    // Die Start-Listings dürfen den Vorgabewert „links" nicht vor dem gelesenen
    // Sitzungswert speichern; der erste gespeicherte Stand trägt bereits „rechts".
    assert.equal(first.activeSide, "right");
    assert.equal(await page.locator('.pane[data-side="right"]').evaluate(el => el.classList.contains("active")), true);
    await page.locator('.pane[data-side="left"] .listing').click();
    await page.waitForFunction(() => window.__MOCK_DESKTOP__().rememberedWindows.at(-1).activeSide === "left");
  });

  await runCase("Modalhülle übernimmt keine Klasse oder Handler eines früheren Besitzers", "?keyboard=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "preferences"));
    await page.getByRole("dialog", { name: "Einstellungen" }).waitFor();
    await page.getByRole("button", { name: "Fertig", exact: true }).click();
    await page.keyboard.press("F2");
    await page.getByRole("dialog", { name: "Umbenennen" }).waitFor();
    assert.equal(await page.locator("#modal .modal").getAttribute("class"), "modal");
    assert.equal(await page.evaluate(() => document.querySelector("#modal-body").onchange), null);
    const width = await page.locator("#modal .modal").evaluate(el => el.getBoundingClientRect().width);
    assert(width < 500, `Normaler Dialog ist ${width}px breit.`);
    await page.keyboard.press("Escape");
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "information"));
    await page.getByRole("dialog", { name: /Informationen/ }).waitFor();
    await page.getByRole("button", { name: "Schließen", exact: true }).click();
    await page.keyboard.press("F2");
    await page.getByRole("dialog", { name: "Umbenennen" }).waitFor();
    assert.equal(await page.locator("#modal .modal").getAttribute("class"), "modal");
    await page.keyboard.press("Escape");
  });

  await runCase("Modal-Lease bewahrt onchange beim Kinddialog und isoliert seinen Handler", "", async page => {
    await page.evaluate(async () => {
      const api = await import("/modal.js");
      window.__REVIEW_MODAL_API__ = api;
      window.__REVIEW_PARENT_CHANGES__ = 0;
      window.__REVIEW_PARENT_TOKEN__ = api.showOwnedModal("review-parent", () => {
        document.querySelector("#modal .modal").className = "modal settings-modal";
        document.querySelector("#modal-title").textContent = "Elternbesitzer";
        const body = document.querySelector("#modal-body");
        body.innerHTML = '<input id="review-parent-input">';
        body.onchange = () => window.__REVIEW_PARENT_CHANGES__++;
        document.querySelector("#modal .modal-buttons").replaceChildren();
        return { focusTarget: body.querySelector("input") };
      });
      window.__REVIEW_CHILD__ = api.confirmModal("Kindbesitzer", '<input id="review-child-input">', { parentToken: window.__REVIEW_PARENT_TOKEN__ });
    });
    await page.getByRole("dialog", { name: "Kindbesitzer" }).waitFor();
    assert.equal(await page.locator("#modal .modal").getAttribute("class"), "modal");
    await page.locator("#review-child-input").evaluate(el => el.dispatchEvent(new Event("change", { bubbles: true })));
    assert.equal(await page.evaluate(() => window.__REVIEW_PARENT_CHANGES__), 0);
    await page.getByRole("button", { name: "Abbrechen", exact: true }).click();
    await page.getByRole("dialog", { name: "Elternbesitzer" }).waitFor();
    assert.equal(await page.locator("#modal .modal").getAttribute("class"), "modal settings-modal");
    await page.locator("#review-parent-input").evaluate(el => el.dispatchEvent(new Event("change", { bubbles: true })));
    assert.equal(await page.evaluate(() => window.__REVIEW_PARENT_CHANGES__), 1);
    await page.evaluate(() => window.__REVIEW_MODAL_API__.hideModal(window.__REVIEW_PARENT_TOKEN__));
  });

  await runCase("Gescheiterte Fenstermigration erhält lokale Einstellungen und erlaubt Retry", "", async page => {
    const before = await page.evaluate(async () => {
      const { createSettingsUi } = await import("/settings-ui.js");
      const original = window.__TAURI__.core.invoke;
      const initial = await original("desktop_preferences");
      initial.needsMigration = true;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "desktop_preferences") return structuredClone(initial);
        if (command === "update_desktop_preferences" && !window.__MIGRATION_RECOVERED__) {
          throw { code: "io", message: "Konfiguration ist voll" };
        }
        if (command === "update_desktop_preferences") {
          const next = { ...initial, revision: initial.revision + 1, needsMigration: false,
            preferences: { ...initial.preferences, ...args.patch } };
          window.__MIGRATION_SAVED__ = next.preferences;
          return next;
        }
        return original(command, args);
      };
      const local = { theme: "dark", byteUnits: "binary", localFolderSizes: true, networkDiscovery: false };
      window.__MIGRATION_VALUES__ = { ...local };
      const options = {
        applyTheme: value => { window.__MIGRATION_VALUES__.theme = value; },
        applyByteUnits: value => { window.__MIGRATION_VALUES__.byteUnits = value; },
        applyLocalFolderSizes: value => { window.__MIGRATION_VALUES__.localFolderSizes = value; },
        applyNetworkDiscovery: value => { window.__MIGRATION_VALUES__.networkDiscovery = value; },
        currentWindow() {}, legacyPreferences: () => ({ ...window.__MIGRATION_VALUES__ }),
        showError: (_title, error) => { window.__MIGRATION_ERROR__ = error.message; },
      };
      await createSettingsUi(options).init();
      window.__MIGRATION_OPTIONS__ = options;
      return local;
    });
    assert.deepEqual(await page.evaluate(() => window.__MIGRATION_VALUES__), before);
    assert.equal(await page.evaluate(() => window.__MIGRATION_ERROR__), "Konfiguration ist voll");
    await page.evaluate(async () => {
      window.__MIGRATION_RECOVERED__ = true;
      const { createSettingsUi } = await import("/settings-ui.js");
      await createSettingsUi(window.__MIGRATION_OPTIONS__).init();
    });
    const preferences = await page.evaluate(() => window.__MIGRATION_SAVED__);
    for (const [key, value] of Object.entries(before)) assert.equal(preferences[key], value);
  });

  await runCase("Verspätete Fenstermigration erhält eine inzwischen bestätigte Einstellung", "", async page => {
    await page.evaluate(async () => {
      const { createSettingsUi } = await import("/settings-ui.js");
      const invoke = window.__TAURI__.core.invoke;
      const stale = await invoke("desktop_preferences");
      stale.needsMigration = true;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "desktop_preferences") return structuredClone(stale);
        if (command === "update_desktop_preferences" && args.patch.theme === "light") {
          await new Promise(resolve => { window.__REVIEW_RELEASE_MIGRATION__ = resolve; });
        }
        return invoke(command, args);
      };
      window.__REVIEW_LATE_SETTINGS__ = createSettingsUi({
        applyByteUnits() {}, applyLocalFolderSizes() {}, applyNetworkDiscovery() {}, applyTheme() {},
        currentWindow() {}, legacyPreferences: () => ({ theme: "light" }), showError() {},
      });
      window.__REVIEW_LATE_INIT__ = window.__REVIEW_LATE_SETTINGS__.init();
    });
    await page.waitForFunction(() => typeof window.__REVIEW_RELEASE_MIGRATION__ === "function");
    await page.evaluate(() => window.__TAURI__.core.invoke("update_desktop_preferences", { patch: { theme: "dark" } }));
    const revision = await page.evaluate(() => window.__MOCK_DESKTOP__().revision);
    await page.evaluate(async () => { window.__REVIEW_RELEASE_MIGRATION__(); await window.__REVIEW_LATE_INIT__; });
    const current = await page.evaluate(() => window.__MOCK_DESKTOP__());
    assert.equal(current.preferences.theme, "dark");
    assert.equal(current.revision, revision, "Eine überholte Migration erzeugt weder Commit noch Revision.");
  });

  await runCase("Fremde Einstellungsänderung wirkt ohne Dialog und hält den Fokus im offenen Dialog", "", async page => {
    const emitForeign = (patch, revision) => page.evaluate(({ patch, revision }) => {
      const preferences = { ...window.__MOCK_DESKTOP__().preferences, ...patch };
      window.__MOCK_EMIT__("desktop-preferences-changed", { preferences, revision, needsMigration: false, loadError: null });
    }, { patch, revision });
    await emitForeign({ theme: "dark" }, 500);
    await page.waitForFunction(() => document.documentElement.dataset.resolvedTheme === "dark");
    assert.equal(await page.locator("#modal").isHidden(), true);

    await page.evaluate(() => window.__MOCK_EMIT__("menu", "preferences"));
    const dialog = page.getByRole("dialog", { name: "Einstellungen" });
    await dialog.waitFor();
    const theme = dialog.getByLabel("Darstellung");
    await theme.focus();
    await emitForeign({ theme: "dark", byteUnits: "binary", icon: "storm" }, 501);
    await page.waitForFunction(() => document.querySelector('#modal-body [data-setting="byteUnits"]').value === "binary");
    assert.equal(await dialog.locator('input[name="icon"]:checked').inputValue(), "storm");
    assert.equal(await page.evaluate(() => document.activeElement?.dataset.setting), "theme");
    // Auch die eigene Änderung baut den Dialog nicht neu: der Fokus bleibt im Select.
    await theme.selectOption("light");
    await page.waitForFunction(() => window.__MOCK_DESKTOP__().preferences.theme === "light");
    assert.equal(await page.evaluate(() => document.activeElement?.dataset.setting), "theme");
  });

  await runCase("Drag-and-drop ignoriert veraltete Auswahl und fremde Drop-Flächen", "", async page => {
    await page.evaluate(() => {
      const row = document.querySelector('.pane[data-side="left"] .row');
      row.dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer: new DataTransfer() }));
    });
    const path = page.locator('.pane[data-side="left"] .path');
    await path.fill("/synthetic/changed");
    await path.press("Enter");
    await nativeDrop(page, "right", []);
    await page.evaluate(() => window.__MOCK_EMIT__("tauri://drag-drop", {
      paths: ["/synthetic/file.txt"], position: { x: 1, y: 1 },
    }));
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });

  await runCase("Rename-Vertrag zeigt Best Effort vor der Namenseingabe und reicht die Klasse weiter", "?remoteFile=1&renameConfirmed=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote\.txt/ }).click();
    await list.focus(); await page.keyboard.press("F2");
    const dialog = page.getByRole("dialog", { name: "Umbenennen" });
    await dialog.waitFor();
    const limitations = dialog.getByRole("textbox", { name: "Wirkung und Einschränkungen", exact: true });
    assert.match(await limitations.inputValue(), /Best Effort.*Löschprobe.*unmittelbaren Elternordner/s);
    assert.equal(await limitations.getAttribute("readonly"), "");
    assert.equal(await page.evaluate(() => document.documentElement.dataset.lastRemoteRename ?? null), null);
    await page.locator("#modal-input").fill("Grüße %2F.txt");
    await dialog.getByRole("button", { name: "OK", exact: true }).click();
    await page.waitForFunction(() => !!document.documentElement.dataset.lastRemoteRename);
    const request = await page.evaluate(() => JSON.parse(document.documentElement.dataset.lastRemoteRename));
    assert.equal(request.expectedExecutionClass, "best_effort");
    assert.equal(request.newName, "Grüße %2F.txt");
  });

  await runCase("Rename-Vertrag sperrt konkret fehlende Elternbindung vor der Namenseingabe", "?remoteFile=1&renameBlocked=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote\.txt/ }).click();
    await list.focus(); await page.keyboard.press("F2");
    await page.getByText(/Mock: Elternbindung fehlt/).waitFor();
    assert.equal(await page.getByRole("dialog", { name: "Umbenennen" }).count(), 0);
    assert.equal(await page.evaluate(() => document.documentElement.dataset.lastRemoteRename ?? null), null);
  });

  await runCase("Später Rename-Vertrag öffnet nach Navigation keinen Namensdialog", "?remoteFile=1&renameConfirmed=1&renameContractDelay=400", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote\.txt/ }).click();
    await list.focus(); await page.keyboard.press("F2");
    const path = page.locator('.pane[data-side="left"] .path');
    await path.fill("/anderer-ordner"); await path.press("Enter");
    await page.waitForTimeout(500);
    assert.equal(await page.getByRole("dialog", { name: "Umbenennen" }).count(), 0);
    assert.equal(await page.evaluate(() => document.documentElement.dataset.lastRemoteRename ?? null), null);
  });

  await runCase("Remote-Pane führt F2, F5, F6 und Öffnen endpointgebunden aus", "?remoteFile=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    const file = list.getByRole("option", { name: /remote\.txt/ });
    await file.click();
    await list.focus();

    await page.keyboard.press("F5");
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    let operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.at(-1).operation);
    assert.equal(operation.type, "ProviderFile");
    assert.equal(operation.kind, "Copy");
    assert.equal(operation.source.provider, "Sftp");
    assert.equal(operation.target.provider, "Local");

    await page.keyboard.press("F6");
    await page.locator("#queue-count").filter({ hasText: "2" }).waitFor();
    operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.at(-1).operation);
    assert.equal(operation.kind, "Move");

    await page.keyboard.press("F2");
    await page.locator("#modal-input").fill("umbenannt.txt");
    await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK" }).click();
    const renamed = await page.evaluate(() =>
      JSON.parse(document.documentElement.dataset.lastRemoteRename || "null"));
    assert.equal(renamed.endpoint.profileId, "mock_sftp");
    assert.equal(renamed.path, "/remote.txt");
    assert.equal(renamed.result, "/umbenannt.txt");
    // Best Effort ist ein reguläres Ergebnis: kurzer Hinweis statt modaler Warnung.
    assert.match(await page.locator("#dock-summary").innerText(), /Best Effort/);

    // Ein bereits belegter Zielname ist ein sichtbares Resultat ohne Mutation —
    // kein Fehlerdialog und kein stilles Ersetzen.
    await file.click();
    await list.focus();
    await page.keyboard.press("F2");
    await page.locator("#modal-input").fill("kollision.txt");
    await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK" }).click();
    await page.waitForFunction(() =>
      (document.querySelector("#dock-summary")?.innerText || "").includes("bereits belegt"));
    const lastRename = await page.evaluate(() =>
      JSON.parse(document.documentElement.dataset.lastRemoteRename || "null"));
    assert.equal(lastRename.newName, "umbenannt.txt", "Kollision darf keine Mutation auslösen");

    // Das Mock-Listing bleibt absichtlich unverändert; erneut auswählen und öffnen.
    await file.click();
    await list.focus();
    await page.keyboard.press("Enter");
    await page.waitForFunction(() =>
      document.documentElement.dataset.lastRemoteOpened === "/remote.txt");
    assert.match(await page.locator("#dock-summary").innerText(), /Lokale Momentaufnahme/);
    assert.match(await page.locator("#dock-summary").innerText(), /nicht zum Server zurückgeschrieben/);
  });

  await runCase("Best-Effort-Ziel zeigt Einschränkung und reiht die Kopie ein", "?remoteFile=1", async page => {
    // FTPS in der rechten Pane öffnen: Der Mock meldet dort copyNew=false und
    // copyNewBestEffort=true — die Kopie darf nicht mehr pauschal sperren.
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await left.focus();
    await page.keyboard.press("F5");
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    // Die Einschränkung ist sichtbar, aber nicht modal.
    assert.match(await page.locator("#dock-summary").innerText(), /Best Effort/);
    const operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.at(-1).operation);
    assert.equal(operation.type, "ProviderFile");
    assert.equal(operation.kind, "Copy");
    assert.equal(operation.target.provider, "Ftps");
    assert.equal(operation.copy_new_best_effort_expected, true,
      "Die sichtbare Best-Effort-Neuanlage muss im Queue-Request gebunden bleiben.");
    assert.equal(operation.replace_best_effort_expected, false,
      "Ohne Ersetzen darf keine stärkere oder schwächere Klasse erfunden werden.");
  });

  for (const theme of ["light", "dark"]) {
    await runCase(`AP18 FTPS-F6 bindet Kopie und Quellentfernung auch im selben Profil: ${theme}`, "?remoteFile=1&bestEffortSource=1", async page => {
      await page.setViewportSize({ width: 800, height: 600 });
      await page.evaluate(async theme => (await import("/theme.js")).setTheme(theme), theme);
      const connection = page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]');
      await connection.click();
      await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
      await connection.click();
      const target = page.locator('[data-side="right"] .path');
      await target.fill("/to"); await target.press("Enter");
      await page.evaluate(() => {
        const invoke = window.__TAURI__.core.invoke;
        window.__TAURI__.core.invoke = async (command, args) => {
          if (command === "enqueue_provider_file_operation") window.__AP18_REQUEST__ = structuredClone(args.request);
          return invoke(command, args);
        };
      });
      const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
      await left.getByRole("option", { name: /remote\.txt/ }).click();
      await left.focus(); await page.keyboard.press("F6");
      await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
      const operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.at(-1).operation);
      const request = await page.evaluate(() => window.__AP18_REQUEST__);
      assert.equal(request.source.profileId, "mock_ftps");
      assert.equal(request.target.profileId, "mock_ftps");
      assert.equal(operation.source.root, "/");
      assert.equal(operation.target.root, "/to");
      assert.equal(operation.copy_new_best_effort_expected, true);
      assert.equal(operation.remove_source_best_effort_expected, true,
        "Gleiches Profil darf die vom Backend gemeldete Quellentfernung nicht auslassen");
      assert.match(await page.locator("#dock-summary").innerText(), /Best Effort.*frischem Readback/s);
      assert.equal(await page.getByRole("dialog").count(), 0);
      if (process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/ap18-f6-${theme}.png` });
    });
  }

  await runCase("AP11 WebDAV-F5 bindet die additive Best-Effort-Klasse", "?remoteFile=1&webdavUpload=1", async page => {
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await left.focus(); await page.keyboard.press("F5");
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    assert.match(await page.locator("#dock-summary").innerText(), /Best Effort.*PUT\/MOVE.*Collection-/s);
    const operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.at(-1).operation);
    assert.equal(operation.target.provider, "WebDav");
    assert.equal(operation.copy_new_best_effort_expected, true);
    assert.equal(operation.replace_best_effort_expected, false);
    assert.equal(operation.kind, "Copy");
    assert.equal(await page.getByRole("dialog").count(), 0);
  });

  for (const theme of ["light", "dark"]) {
    await runCase(`AP13 WebDAV-F5 erklärt die Ersetzungsgrenze kopierbar: ${theme}`, "?remoteFile=1&webdavUpload=1&conflicts=provider-replace&longSourceLimits=1", async page => {
      await page.setViewportSize({ width: 800, height: 600 });
      await page.evaluate(async theme => (await import("/theme.js")).setTheme(theme), theme);
      await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
      await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
      const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
      await left.getByRole("option", { name: /Beispiel\.txt/ }).click();
      await left.focus(); await page.keyboard.press("F5");
      const dialog = page.getByRole("dialog", { name: "Konflikte beim Kopieren" });
      await dialog.waitFor();
      const explanation = dialog.getByRole("textbox", { name: "Grenzen beim Ersetzen" });
      const sourceLimits = dialog.getByRole("textbox", { name: "Quellgrenzen und Auslassungen" });
      assert.equal(await sourceLimits.isVisible(), true);
      assert.equal(await dialog.locator(".conflict-name").isVisible(), true);
      assert.match(await explanation.inputValue(), /UND.*ODER/s);
      assert.match(await explanation.inputValue(), /<script>kein Code<\/script>/);
      assert.equal(await explanation.evaluate(element => element.readOnly), true);
      assert.equal(await dialog.locator("script").count(), 0);
      assert.equal(await dialog.getByRole("button", { name: /^(Dauerhaft )?Ersetzen$/ }).count(), 0);
      for (let step = 0; step < 8 && !await explanation.evaluate(element => element === document.activeElement); step++) await page.keyboard.press("Tab");
      assert.equal(await explanation.evaluate(element => element === document.activeElement), true);
      await page.keyboard.press("PageDown");
      await page.waitForFunction(() => document.querySelector('[aria-label="Grenzen beim Ersetzen"]').scrollTop > 0);
      await page.keyboard.press("ControlOrMeta+A");
      assert.equal(await explanation.evaluate(element => element.selectionEnd - element.selectionStart), (await explanation.inputValue()).length);
      const geometry = await dialog.evaluate(element => ({ height: element.getBoundingClientRect().height, width: element.clientWidth, scrollWidth: element.scrollWidth }));
      assert.ok(geometry.height <= 576 && geometry.scrollWidth <= geometry.width + 1, JSON.stringify(geometry));
      if (process.env.AP13_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.AP13_EVIDENCE_DIR}/browser-replace-${theme}.png` });
      await dialog.locator("summary").filter({ hasText: "Quellgrenzen und Auslassungen" }).click();
      assert.equal(await sourceLimits.isVisible(), false);
      await dialog.locator("summary").filter({ hasText: "Quellgrenzen und Auslassungen" }).click();
      await sourceLimits.focus();
      await page.keyboard.press("PageDown");
      await page.waitForFunction(() => document.querySelector('#modal [aria-label="Quellgrenzen und Auslassungen"]').scrollTop > 0);
      assert.match(await sourceLimits.inputValue(), /Auslassen-59/);
      const limitsGeometry = await sourceLimits.evaluate(element => ({ height: element.clientHeight, contentHeight: element.scrollHeight }));
      assert.ok(limitsGeometry.height <= 200 && limitsGeometry.contentHeight > limitsGeometry.height, JSON.stringify(limitsGeometry));
      await page.keyboard.press("ControlOrMeta+A");
      assert.equal(await sourceLimits.evaluate(element => element.selectionEnd - element.selectionStart), (await sourceLimits.inputValue()).length);
      await page.keyboard.press("Escape"); await dialog.waitFor({ state: "hidden" });
      assert.equal(await page.locator("#queue-count").innerText(), "0");
    });
  }

  for (const theme of ["light", "dark"]) {
    await runCase(`AP12 additive WebDAV-Baumvorschau bindet Umfang und Auslassungen: ${theme}`, "?remoteFile=1&webdavUpload=1&webdavTree=1", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme, false); }, theme);
      await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
      await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
      const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
      await left.getByRole("option", { name: /Baum/ }).click(); await left.focus(); await page.keyboard.press("F5");
      await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
      const op = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.at(-1).operation);
      assert.equal(op.source.provider, "Local"); assert.equal(op.target.provider, "WebDav");
      assert.equal(op.copy_new_best_effort_expected, true); assert.equal(op.overwrite, false);
      assert.equal(op.source_tree_expected.entries, 4); assert.equal(op.source_tree_expected.omitted.length, 1);
      assert.match(await page.locator("#dock-summary").innerText(), /Begrenzter Quellbaum.*ausgelassen/s);
      await page.getByText("Quellgrenzen und Auslassungen anzeigen", { exact: true }).click();
      assert.match(await page.getByRole("textbox", {name:"Quellgrenzen und Auslassungen",exact:true}).inputValue(), /Symlink/);
      assert.equal(await page.getByRole("dialog").count(), 0);
      await page.locator("#btn-queue").click();
      assert.match(await page.locator("#queue-popover").innerText(), /Bestätigter Quellbaumumfang.*2 Ordner/s);
    });
  }

  for (const theme of ["light", "dark"]) {
    await runCase(`AP12 lange Quell- und Metadatenbilanz bleibt kopierbar und begrenzt: ${theme}`, "", async page => {
      await page.setViewportSize({ width: 800, height: 600 });
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme, false); }, theme);
      await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", {
        files: 2, bytes: 123, target_copy_new_best_effort: true,
        source_tree: { files:2,directories:3,omitted:Array.from({length:80},(_,i)=>({path:`/quelle/Baum/Link-${i}`,reason:"Symlink"})) },
        source_read_limitations: ["Begrenzter Quellbaum: 2 Dateien, 3 Ordner; 80 Einträge ausgelassen", ...Array.from({length:80},(_,i)=>`Nicht übertragen: /quelle/Baum/Link-${i}-<img src=x onerror=window.ap12Injected=true>: Symlink`)],
        target_write_limitations: Array.from({length:60},(_,i)=>`/pane/Ordner-${i}: native Metadaten ausgelassen`),
      }));
      const toggle = page.getByText("Quellgrenzen und Auslassungen anzeigen", {exact:true});
      await toggle.focus(); await page.keyboard.press("Space");
      const report=page.getByRole("textbox",{name:"Quellgrenzen und Auslassungen",exact:true});
      assert.equal((await report.inputValue()).split("\n").length,81);
      await page.keyboard.press("Tab"); assert.equal(await report.evaluate(el=>el===document.activeElement),true);
      await page.keyboard.press("PageDown"); await page.waitForFunction(()=>document.querySelector('[aria-label="Quellgrenzen und Auslassungen"]').scrollTop>0);
      await page.getByText("Ausgelassene Metadaten anzeigen",{exact:true}).click();
      await page.getByRole("textbox",{name:"Ausgelassene Zielmetadaten"}).focus();
      assert.equal(await page.getByRole("button",{name:"Schließen",exact:true}).isVisible(),true);
      assert.equal(await page.locator("#dock").evaluate(el=>el.getBoundingClientRect().bottom<=innerHeight),true);
      assert.equal(await page.locator("#dock-summary").evaluate(el=>el.scrollHeight>el.clientHeight),true);
      assert.equal(await page.locator("#dock img").count(),0); assert.equal(await page.evaluate(()=>window.ap12Injected??false),false);
      if (process.env.AP12_EVIDENCE_DIR) await page.screenshot({path:`${process.env.AP12_EVIDENCE_DIR}/source-and-metadata-${theme}.png`});
    });
  }

  await runCase("AP11 WebDAV-Inhaltsvorbereitung lässt sich mit Escape abbrechen", "?remoteFile=1&webdavUpload=1&fileActionPreviewDelay=500", async page => {
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /Beispiel\.txt/ }).click(); await list.focus(); await page.keyboard.press("F5");
    await page.locator("#btn-cancel").waitFor({ state: "visible" });
    assert.match(await page.locator("#dock-summary").innerText(), /Abbrechbare Vorbereitung.*WebDAV/s);
    await page.keyboard.press("Escape");
    await page.locator("#btn-cancel").waitFor({ state: "hidden" });
    await page.locator("#dock-summary").filter({ hasText: /Abgebrochen/ }).waitFor();
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
    await list.focus(); await page.keyboard.press("F5");
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
  });

  await runCase("AP11 Metadatenbilanz bleibt vollständig tastaturbedienbar und escaped", "", async page => {
    await page.setViewportSize({ width: 800, height: 600 });
    await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", {
      files: 100, bytes: 12345, target_copy_new_best_effort: true,
      target_write_limitations: Array.from({ length: 100 }, (_, i) => `/pane/Datei-${i}-<img src=x onerror=window.ap11Injected=true>.txt: Rechte und Metadaten ausgelassen`),
    }));
    const summary = page.locator("#dock-summary");
    assert.match(await summary.innerText(), /100 Einträge ohne native Zielmetadaten/);
    const toggle = summary.locator("summary", { hasText: "Ausgelassene Metadaten anzeigen" });
    await toggle.focus(); await page.keyboard.press("Space");
    const report = page.getByRole("textbox", { name: "Ausgelassene Zielmetadaten" });
    await report.waitFor(); assert.equal((await report.inputValue()).split("\n").length, 100);
    assert.equal(await report.locator("img").count(), 0);
    assert.equal(await page.evaluate(() => window.ap11Injected ?? false), false);
    await page.keyboard.press("Tab"); assert.equal(await report.evaluate(el => el === document.activeElement), true);
    await page.keyboard.press("PageDown");
    await page.waitForFunction(() => document.querySelector('[aria-label="Ausgelassene Zielmetadaten"]').scrollTop > 0);
    assert.equal(await report.evaluate(el => el.scrollWidth <= el.clientWidth + 1), true);
    if (process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/ap11-metadata-report.png` });
  });

  await runCase("Best-Effort-Quellentfernung bleibt als F6 gebunden", "?remoteFile=1&bestEffortSource=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote\.txt/ }).click();
    await list.focus();
    await page.keyboard.press("F6");
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    assert.match(await page.locator("#dock-summary").innerText(), /Best Effort.*frischem Readback/s);
    const operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.at(-1).operation);
    assert.equal(operation.kind, "Move");
    assert.equal(operation.remove_source_best_effort_expected, true,
      "Die sichtbare schwache Quellentfernung muss im Queue-Request gebunden bleiben.");

    await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", {
      files: 1, skipped: 0, symlinks_skipped: 0, bytes: 8, cancelled: false,
      source_removal_best_effort: true,
    }));
    await page.getByText(/Quellentfernung: Best Effort/).waitFor();
  });

  await runCase("Abschluss zeigt alle tatsächlich genutzten Best-Effort-Zielwirkungen", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", {
      files: 2, skipped: 0, symlinks_skipped: 0, bytes: 16, cancelled: false,
      source_removal_best_effort: false,
      target_copy_new_best_effort: true,
      target_replace_best_effort: true,
    }));
    const summary = page.locator("#dock-summary");
    await summary.getByText(/Neuanlegen am Ziel: Best Effort/).waitFor();
    assert.match(await summary.innerText(), /Ersetzen am Ziel: Best Effort/);
  });

  await runCase("Best-Effort-Ersetzen warnt vor dauerhaftem Verlust", "?remoteFile=1&conflicts=provider-replace", async page => {
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await left.focus();
    await page.keyboard.press("F5");

    const dialog = page.getByRole("dialog", { name: "Konflikte beim Kopieren" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /Zieldateien werden ohne Quarantäne unwiederbringlich verworfen/);
    assert.match(await dialog.innerText(), /unklarer Ausgang.*nicht/);
    await dialog.getByRole("button", { name: "Dauerhaft ersetzen" }).click();
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    assert.match(await page.locator("#dock-summary").innerText(), /Altversion ohne Quarantäne dauerhaft ersetzt/);
    const operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.at(-1).operation);
    assert.equal(operation.overwrite, true);
    assert.deepEqual(operation.expected_conflicts, ["Beispiel.txt"]);
    assert.equal(operation.copy_new_best_effort_expected, true);
    assert.equal(operation.replace_best_effort_expected, true,
      "Die sichtbare irreversible Best-Effort-Ersetzung muss gebunden bleiben.");
  });

  await runCase("Best-Effort-Vorschau zeigt Neuanlegen, Ersetzen und Quellentfernung gemeinsam", "?remoteFile=1&bestEffortSource=1&conflicts=provider-replace", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.getByRole("option", { name: /remote\.txt/ }).click();
    await left.focus();
    await page.keyboard.press("F6");
    const dialog = page.getByRole("dialog", { name: "Konflikte beim Verschieben" });
    await dialog.getByRole("button", { name: "Dauerhaft ersetzen" }).click();
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    const summary = await page.locator("#dock-summary").innerText();
    assert.match(summary, /Neue Dateien laufen als Best-Effort-Commit/);
    assert.match(summary, /Altversion ohne Quarantäne dauerhaft ersetzt/);
    assert.match(summary, /Quelle wird nach frischem Readback einzeln entfernt/);
  });

  await runCase("Veraltete Transfer-Vorschau öffnet nach Pane-Tausch keinen Dialog", "?fileActionPreviewDelay=120&conflicts=provider-replace", async page => {
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await left.focus();
    await page.keyboard.press("F5");
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "swap_panes"));
    await page.waitForTimeout(170);
    assert.equal(await page.getByRole("dialog", { name: "Konflikte beim Kopieren" }).count(), 0);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });

  await runCase("Eine neuere F2-Aktion entwertet die noch laufende lokale Transfer-Vorschau", "?conflicts=replace", async page => {
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const response = await invoke(command, args);
        if (command !== "check_local_file_operation") return response;
        window.__F2_PREVIEW_HELD__ = true;
        await new Promise(resolve => { window.__F2_RELEASE_PREVIEW__ = resolve; });
        window.__F2_PREVIEW_RETURNED__ = true;
        return response;
      };
    });
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await left.focus();
    await page.keyboard.press("F5");
    await page.waitForFunction(() => window.__F2_PREVIEW_HELD__ === true);
    await page.keyboard.press("F2");
    const rename = page.getByRole("dialog", { name: "Umbenennen" });
    await rename.waitFor();
    await page.evaluate(() => window.__F2_RELEASE_PREVIEW__());
    await page.waitForFunction(() => window.__F2_PREVIEW_RETURNED__ === true);
    assert.equal(await rename.isVisible(), true, "Alte Vorschau verdrängte den neueren Rename-Dialog");
    assert.equal(await page.getByRole("dialog", { name: "Konflikte beim Kopieren" }).count(), 0);
    await page.keyboard.press("Escape");
    await rename.waitFor({ state: "detached" });
    // Ein wartender Modal-Lease wird erst im nächsten Timerturn aktiviert.
    // Diese Grenze macht auch eine fälschlich eingereihte alte Vorschau sichtbar.
    await page.evaluate(() => new Promise(resolve => setTimeout(resolve, 0)));
    assert.equal(await page.getByRole("dialog", { name: "Konflikte beim Kopieren" }).count(), 0);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });

  await runCase("Veraltete Delete-Vorschau wirkt nach Endpunktwechsel nicht", "?remoteFile=1&fileActionPreviewDelay=120", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.getByRole("option", { name: /remote\.txt/ }).click();
    await left.focus();
    await page.keyboard.press("Control+Backspace");
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    await page.waitForTimeout(170);
    assert.equal(await page.getByRole("dialog", { name: /Quarantäne|Irreversibel/ }).count(), 0);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });

  await runCase("F2 und Neuer Ordner verwerfen den vor dem Dialog gebundenen Endpunkt", "?remoteFile=1", async page => {
    const localList = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await localList.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await localList.focus();
    await page.keyboard.press("F2");
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]')
      .evaluate(element => element.click());
    await page.locator("#modal-input").fill("fremder-endpunkt.txt");
    await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK" }).click();
    assert.equal(
      await page.evaluate(() => document.documentElement.dataset.lastRemoteRename ?? null),
      null,
    );

    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /remote\.txt/ }).waitFor();
    await page.evaluate(() => { window.__MOCK_EMIT__("menu", "new_folder"); });
    await page.locator("#modal-input").waitFor();
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]')
      .evaluate(element => element.click());
    await page.locator("#modal-input").fill("fremder Ordner");
    await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK" }).click();
    assert.equal(
      await page.evaluate(() => document.documentElement.dataset.lastMkdir ?? null),
      null,
    );
  });

  await runCase("Spätes Rename-Ergebnis bleibt trotz neuerer Dateiaktion sichtbar", "?remoteFile=1&renameDelay=140", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.getByRole("option", { name: /remote\.txt/ }).click();
    await left.focus();
    await page.keyboard.press("F2");
    await page.locator("#modal-input").fill("spaet.txt");
    await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK" }).click();

    const right = page.getByRole("listbox", { name: /Rechte Dateiliste/ });
    await right.click();
    await left.focus();
    await page.keyboard.press("F2");
    await page.getByRole("dialog", { name: "Umbenennen" }).waitFor();
    await page.locator("#dock-summary").filter({ hasText: "Umbenannt" }).waitFor();
    await page.keyboard.press("Escape");
    const result = await page.evaluate(() =>
      JSON.parse(document.documentElement.dataset.lastRemoteRename || "null"));
    assert.equal(result.newName, "spaet.txt");
  });

  await runCase("Später unklarer Rename-Fehler bleibt nach Panewechsel sichtbar", "?remoteFile=1&renameDelay=140&renameOutcomeUnclear=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.getByRole("option", { name: /remote\.txt/ }).click();
    await left.focus();
    await page.keyboard.press("F2");
    await page.locator("#modal-input").fill("unklar.txt");
    await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK" }).click();
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();

    const failure = page.locator("#dock-summary").filter({ hasText: "Rename-Ausgang unklar" });
    await failure.waitFor();
    assert.match(await failure.innerText(), /Fehler/);
  });

  await runCase("Späte Ordneranlage bleibt trotz neuerer Dateiaktion sichtbar", "?mkdirDelay=140", async page => {
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.focus();
    await page.evaluate(() => { window.__MOCK_EMIT__("menu", "new_folder"); });
    await page.locator("#modal-input").fill("Spaeter Ordner");
    await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK" }).click();

    const right = page.getByRole("listbox", { name: /Rechte Dateiliste/ });
    await right.click();
    await left.focus();
    await page.keyboard.press("F2");
    await page.getByRole("dialog", { name: "Umbenennen" }).waitFor();
    await page.locator("#dock-summary").filter({ hasText: "Ordner angelegt" }).waitFor();
    await page.keyboard.press("Escape");
    const result = await page.evaluate(() =>
      JSON.parse(document.documentElement.dataset.lastMkdir || "null"));
    assert.equal(result.name, "Spaeter Ordner");
  });

  await runCase("Native Pane-Menüaktion bleibt während eines Modals gesperrt", "", async page => {
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await left.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await left.focus();
    const before = await page.getByRole("textbox").first().getAttribute("title");
    await page.keyboard.press("F2");
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "swap_panes"));
    assert.equal(await page.getByRole("textbox").first().getAttribute("title"), before);
    await page.keyboard.press("Escape");
  });

  await runCase("Später Papierkorbabschluss bleibt nach Weg-und-zurück-Navigation sichtbar", "?trashDelay=140", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    const path = pane.locator(".path");
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await list.focus();
    await page.keyboard.press("Control+Backspace");

    await path.fill("/mock/fast");
    await path.press("Enter");
    await list.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    await path.fill("/mock/quelle");
    await path.press("Enter");
    await list.getByRole("option", { name: /Beispiel\.txt/ }).waitFor();

    await page.locator("#dock-summary").filter({ hasText: "Papierkorb" }).waitFor();
    await list.getByRole("option", { name: /Beispiel\.txt/ }).waitFor({ state: "detached" });
    assert.equal(await path.inputValue(), "/mock/quelle",
      "Der Abschluss darf die aktuelle Navigation nicht verändern.");
  });

  await runCase("Papierkorb lädt beide aktuell gleichen lokalen Panes neu", "?trashDelay=60", async page => {
    const rightPane = page.locator('.pane[data-side="right"]');
    await rightPane.locator(".path").fill("/mock/quelle");
    await rightPane.locator(".path").press("Enter");
    const left = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    const right = page.getByRole("listbox", { name: /Rechte Dateiliste/ });
    await right.getByRole("option", { name: /Beispiel\.txt/ }).waitFor();
    await left.getByRole("option", { name: /Beispiel\.txt/ }).click();
    await left.focus();
    await page.keyboard.press("Control+Backspace");
    await left.getByRole("option", { name: /Beispiel\.txt/ }).waitFor({ state: "detached" });
    await right.getByRole("option", { name: /Beispiel\.txt/ }).waitFor({ state: "detached" });
    const scans = (await page.evaluate(() => window.__MOCK_SCAN_CALLS__()))
      .filter(call => call.command === "scan_dir" && call.path === "/mock/quelle");
    assert.ok(scans.length >= 4, "Start, zweite Pane und beide Abschluss-Reloads müssen sichtbar sein.");
  });

  await runCase("Temp-Recoverypfade bleiben in Abschluss und Queue sichtbar", "", async page => {
    const recoveryPath = "/mock/<temp>&-recovery.tmp";
    await page.evaluate(path => window.__MOCK_EMIT__("fileop-done", {
      files: 0,
      bytes: 0,
      skipped: 0,
      symlinks_skipped: 0,
      cancelled: true,
      temp_recovery_paths: [path],
      source_removal_best_effort: false,
      target_copy_new_best_effort: false,
      target_replace_best_effort: false,
    }), recoveryPath);
    await page.locator("#dock-summary").filter({ hasText: "Temp-Recoverypfade" }).waitFor();
    assert.equal(await page.locator("#dock-summary code").innerText(), recoveryPath);
    assert.equal(await page.locator("#dock-summary temp").count(), 0,
      "Der Recoverypfad muss als Text escaped bleiben.");

    await page.evaluate(path => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      queue.entries.push({
        operation_id: "temp-recovery-ui",
        operation: { type: "File", kind: "Copy", items: ["/mock/datei"],
          destination: "/mock/ziel", overwrite: false, expected_conflicts: [] },
        profile_bindings: [],
        created_unix_ms: Date.now(),
        state: "Cancelled",
        attempts: 1,
        progress: { files_total: 1, files_done: 0, bytes_total: 1, bytes_done: 0,
          current: "", phase: "cancelled" },
        attention: null,
        error: null,
        temp_recovery_paths: [path],
        recovery_available: false,
        bidirectional_completion_recovery: null,
      });
      queue.revision += 1;
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    }, recoveryPath);
    await page.locator("#btn-queue").click();
    const row = page.locator('[data-queue-id="temp-recovery-ui"]');
    await row.waitFor();
    assert.equal(await row.locator("code").innerText(), recoveryPath);
  });

  await runCase("Erhaltene Sicherung bleibt bei erfolgreicher Übertragung sichtbar", "", async page => {
    const backup = "/mock/<backup>&-alt";
    await page.evaluate(path => window.__MOCK_EMIT__("fileop-done", {
      files: 1, bytes: 3, skipped: 0, symlinks_skipped: 0, cancelled: false,
      retained_backup_paths: [path], temp_recovery_paths: [path],
      source_removal_best_effort: false, target_copy_new_best_effort: false,
      target_replace_best_effort: false,
    }), backup);
    await page.locator("#dock-summary").filter({ hasText: "Übertragung abgeschlossen; Sicherung erhalten" }).waitFor();
    assert.equal(await page.locator("#dock-summary code").innerText(), backup);
    assert.equal(await page.locator("#dock-summary backup").count(), 0);
    await page.evaluate(path => window.__MOCK_EMIT__("fileop-done", {
      files: 1, bytes: 3, skipped: 0, symlinks_skipped: 0, cancelled: true,
      retained_backup_paths: [path], temp_recovery_paths: [path],
    }), backup);
    await page.locator("#dock-summary").filter({ hasText: "Abgebrochen" }).waitFor();
    assert.doesNotMatch(await page.locator("#dock-summary").innerText(), /Übertragung abgeschlossen/);
    await page.evaluate(path => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      queue.entries.push({
        operation_id: "backup-success-ui", operation: { type: "File", kind: "Copy",
          items: ["/mock/datei"], destination: "/mock/ziel", overwrite: true,
          expected_conflicts: [] }, profile_bindings: [], created_unix_ms: Date.now(),
        state: "Completed", attempts: 1,
        progress: { files_total: 1, files_done: 1, bytes_total: 3, bytes_done: 3,
          current: "", phase: "completed" }, attention: null, error: null,
        retained_backup_paths: [path], temp_recovery_paths: [path],
        recovery_available: false, bidirectional_completion_recovery: null,
      });
      queue.revision += 1;
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    }, backup);
    await page.locator("#btn-queue").click();
    const row = page.locator('[data-queue-id="backup-success-ui"]');
    await row.waitFor();
    assert.match(await row.innerText(), /Sicherung erhalten/);
    assert.equal(await row.locator("code").innerText(), backup);
    await page.evaluate(() => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      const entry = queue.entries.find(entry => entry.operation_id === "backup-success-ui");
      entry.state = "Failed";
      entry.error = "Spätere Datei fehlgeschlagen";
      queue.revision += 1;
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    });
    await row.filter({ hasText: "Spätere Datei fehlgeschlagen" }).waitFor();
    assert.doesNotMatch(await row.innerText(), /Übertragung abgeschlossen/);
  });

  await runCase("Remote-Löschen trennt Quarantäne und irreversible Bestätigung", "?remoteFile=1", async page => {
    // SFTP besitzt im Mock die wiederherstellbare Doppeldecker-Quarantäne.
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    let list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote\.txt/ }).click();
    await list.focus();
    await page.keyboard.press("Control+Backspace");
    const quarantine = page.getByRole("dialog", { name: "In Doppeldecker-Quarantäne verschieben" });
    await quarantine.waitFor();
    assert.match(await quarantine.innerText(), /nicht der lokale Systempapierkorb/);
    await quarantine.getByRole("button", { name: "In Quarantäne verschieben" }).click();
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    let operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.at(-1).operation);
    assert.equal(operation.type, "ProviderDelete");
    // `RemoteDeleteMode` serialisiert snake_case — exakt wie das echte Backend.
    assert.equal(operation.mode, "quarantine");

    // Regression: Die Queue-Ansicht muss den wiederherstellbaren Lauf als
    // „Doppeldecker-Quarantäne" ausweisen — ein PascalCase-Vergleich hatte ihn
    // fälschlich als „Dauerhaft löschen" beschriftet.
    await page.locator("#btn-queue").click();
    await page.getByText(/Doppeldecker-Quarantäne · /).waitFor();
    assert.equal(await page.locator(".queue-row", { hasText: "Dauerhaft löschen" }).count(), 0);
    await page.locator("#queue-popover-close").click();

    // WebDAV besitzt im Mock keinen Restorepfad und verlangt deshalb die exakte
    // irreversible Phrase, die Provider, Profil, Root und Wirkung bindet.
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote\.txt/ }).click();
    await list.focus();
    await page.keyboard.press("Control+Backspace");
    const input = page.locator("#modal-input");
    await input.waitFor();
    const title = await page.locator("#modal-title").innerText();
    const phrase = title.match(/DAUERHAFT LÖSCHEN WEBDAV [^“]+/)?.[0];
    assert.ok(phrase);
    await input.fill(phrase);
    await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK" }).click();
    await page.locator("#queue-count").filter({ hasText: "2" }).waitFor();
    operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.at(-1).operation);
    assert.equal(operation.mode, "permanent");
  });

  await runCase("Fehlgeschlagener Endpunktwechsel hinterlässt keine fremde Dateiliste", "?remoteScanError=1", async page => {
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /Beispiel\.txt/ }).waitFor();
    // Wechsel auf einen Remote-Endpunkt, dessen erstes Listing scheitert.
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByText(/Mock-Remote-Scanfehler/).waitFor();
    // Regression: Die alten lokalen Einträge dürfen nicht stehen bleiben — sie
    // wären sonst als gleichnamige Pfade des NEUEN Providers bedienbar (öffnen,
    // umbenennen, löschen gegen den falschen Endpunkt).
    assert.equal(await list.locator('[role="option"]').count(), 0);
    await list.locator(".empty").waitFor();
  });

  await runCase("Veraltetes loadPane spricht nach Endpunktwechsel keinen alten Provider an", "?cancelSizeDelay=80", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    const path = pane.locator(".path");
    await path.fill("/mock/stale");
    await path.press("Enter");
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /remote/ }).waitFor();
    const calls = await page.evaluate(() => window.__MOCK_SCAN_CALLS__());
    assert.equal(calls.some(call => call.command === "scan_dir" && call.path === "/mock/stale"), false,
      "Der vor dem ersten Await gebundene, danach veraltete lokale Abruf darf nicht mehr starten.");
    assert.equal(calls.some(call => call.command === "scan_sftp_dir"
      && call.profileId === "mock_sftp" && call.profileRevision === 11
      && call.path === "/"), true);
    const connectionActions = await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__());
    assert.equal(connectionActions.some(call => call.command === "connect_sftp_profile"
      && call.profileId === "mock_sftp" && call.profileRevision === 11), true);
  });

  await runCase("Veraltetes SFTP-Listing startet keinen nachgelagerten Connect", "?holdSftpScan=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.waitForFunction(() => window.__MOCK_REQUEST_PHASES__().holdSftpScan?.phase === "pending");
    assert.deepEqual(await page.evaluate(() => window.__MOCK_REQUEST_PHASES__().holdSftpScan), {
      phase: "pending", profileId: "mock_sftp", profileRevision: 11, path: "/",
    });
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /remote/ }).waitFor();
    assert.match(await page.getByRole("textbox").first().getAttribute("title") || "", /WebDAV/);
    await page.evaluate(() => window.__MOCK_RELEASE_REQUEST__("holdSftpScan"));
    await page.waitForFunction(() => window.__MOCK_REQUEST_PHASES__().holdSftpScan.phase === "returned");
    const actions = await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__());
    assert.equal(
      actions.some(action => action.command === "connect_sftp_profile"),
      false,
      "Nach der neueren Navigation darf der alte SFTP-Abruf keine Connect-Nachwirkung starten.",
    );
    assert.match(await page.getByRole("textbox").first().getAttribute("title") || "", /WebDAV/);
  });

  await runCase("Remote-Favoriten erscheinen beim Start erst nach den Profilen", "?remoteFavorite=1&sftpProfileListDelay=1000", async page => {
    const favorite = page.locator("#fav-list .fav-item", { hasText: "Remote-Favorit" });
    assert.equal(await favorite.count(), 0,
      "Vor der aktuellen Profilrevision darf der Remote-Favorit nicht bedienbar sein.");
    await favorite.waitFor();
  });

  await runCase("Profil-Readback sperrt alte Zeile und Favorit synchron", "?remoteFavorite=1&sftpProfileListDelay=150", async page => {
    const profile = page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]');
    const favorite = page.locator("#fav-list .fav-item", { hasText: "Remote-Favorit" });
    await profile.waitFor();
    await favorite.waitFor();

    await page.evaluate(() => {
      window.__MOCK_SET_PROFILE_REVISION__("sftp", "mock_sftp", 12);
      window.__MOCK_EMIT__("connections-updated", {
        protocol: "sftp", profileId: "mock_sftp", revision: 12, change: "saved",
      });
    });
    await profile.waitFor({ state: "hidden" });
    await favorite.click();
    await page.locator("#dock-summary").filter({ hasText: "Profil wird aktualisiert" }).waitFor();
    assert.doesNotMatch(await page.getByRole("textbox").first().getAttribute("title") || "", /SFTP/);

    await profile.waitFor();
    await favorite.click();
    await page.getByRole("textbox").first().waitFor();
    assert.match(await page.getByRole("textbox").first().getAttribute("title") || "", /SFTP/);
    const calls = await page.evaluate(() => window.__MOCK_SCAN_CALLS__());
    assert.equal(calls.some(call => call.command === "scan_sftp_dir"
      && call.profileId === "mock_sftp" && call.profileRevision === 12
      && call.path === "/archiv"), true);
  });

  await runCase("Älterer Profilabruf überschreibt keine neuere Revision", "", async page => {
    const oldProfile = await page.evaluate(() => (
      structuredClone(window.__MOCK_CONNECTION_STATE__().sftpProfiles[0])
    ));
    const newProfile = { ...oldProfile, name: "Neuer Profilstand", revision: 12 };
    const staleProfile = { ...oldProfile, name: "Alter Profilstand", revision: 11 };
    await page.evaluate(({ staleProfile, newProfile }) => {
      window.__MOCK_SET_PROFILE_REVISION__("sftp", "mock_sftp", 12);
      window.__MOCK_QUEUE_PROFILE_READS__("sftp", [
        { delay: 160, profiles: [staleProfile] },
        { delay: 10, profiles: [newProfile] },
      ]);
      window.__MOCK_EMIT__("connections-updated", {
        protocol: "sftp", profileId: "mock_sftp", revision: 12, change: "saved",
      });
      window.__MOCK_EMIT__("connections-updated", {
        protocol: "sftp", profileId: "mock_sftp", revision: 12, change: "saved",
      });
    }, { staleProfile, newProfile });

    const row = page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]');
    await row.filter({ hasText: "Neuer Profilstand" }).waitFor();
    await page.waitForTimeout(220);
    assert.match(await row.innerText(), /Neuer Profilstand/);
    assert.doesNotMatch(await row.innerText(), /Alter Profilstand/);
    await row.click();
    const calls = await page.evaluate(() => window.__MOCK_SCAN_CALLS__());
    assert.equal(calls.some(call => call.command === "scan_sftp_dir"
      && call.profileRevision === 12), true);
  });

  await runCase("Remove-Ereignis eines anderen Fensters invalidiert Pane, Zeile und Favorit", "?remoteFavorite=1", async page => {
    const profile = page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]');
    const favorite = page.locator("#fav-list .fav-item", { hasText: "Remote-Favorit" });
    await profile.click();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /remote/ }).waitFor();

    await page.evaluate(() => {
      window.__MOCK_REMOVE_PROFILE__("sftp", "mock_sftp");
      window.__MOCK_EMIT__("connections-updated", {
        protocol: "sftp", profileId: "mock_sftp", revision: 11, change: "removed",
      });
    });

    await profile.waitFor({ state: "detached" });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /Beispiel\.txt/ }).waitFor();
    const before = (await page.evaluate(() => window.__MOCK_SCAN_CALLS__())).length;
    await favorite.click();
    await page.locator("#dock-summary")
      .filter({ hasText: /Profil.*nicht (?:mehr )?vorhanden|Profil wird aktualisiert/ })
      .waitFor();
    const after = (await page.evaluate(() => window.__MOCK_SCAN_CALLS__())).length;
    assert.equal(after, before, "Ein entfernter Favorit darf keinen Remote-Abruf mehr starten.");
  });

  await runCase("Profilereignis invalidiert nur Panes mit einer alten Revision", "", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /remote/ }).waitFor();
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ })
      .getByRole("option", { name: /remote/ }).waitFor();

    // Ein bloßes Connected-Ereignis derselben Revision darf die sichtbare Pane
    // nicht leeren. Ein neuer Profilstand dagegen muss fail-closed zurückfallen.
    await page.evaluate(() => window.__MOCK_EMIT__("connections-updated", {
      protocol: "sftp", profileId: "mock_sftp", revision: 11, change: "connected",
    }));
    await page.waitForTimeout(30);
    assert.match(await page.getByRole("textbox").nth(0).getAttribute("title") || "", /SFTP/);

    await page.evaluate(() => {
      window.__MOCK_SET_PROFILE_REVISION__("sftp", "mock_sftp", 12);
      window.__MOCK_EMIT__("connections-updated", {
        protocol: "sftp", profileId: "mock_sftp", revision: 12, change: "saved",
      });
    });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /Beispiel\.txt/ }).waitFor();
    assert.match(await page.getByRole("textbox").nth(1).getAttribute("title") || "", /FTPS/,
      "Das Ereignis eines anderen Profils darf die rechte Pane nicht zurücksetzen.");
  });

  for (const [suffix, query] of [
    ["Erfolg", "?remoteFile=1&snapshotDelay=400"],
    ["Fehler", "?remoteFile=1&snapshotDelay=400&snapshotError=1"],
    ["Abbruch", "?remoteFile=1&snapshotDelay=600"],
  ]) {
    await runCase(`Remote-Dateiöffnung bleibt abbrechbar: ${suffix}`, query, async page => {
      await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
      const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
      const file = list.getByRole("option", { name: /remote\.txt/ });
      await file.waitFor();
      await file.dblclick();
      await page.locator("#btn-cancel").waitFor({ state: "visible" });
      assert.equal(await page.locator("#progress").isVisible(), true);
      if (suffix === "Abbruch") await page.keyboard.press("Escape");
      await page.locator("#btn-cancel").waitFor({ state: "hidden" });
      const dock = await page.locator("#dock-summary").innerText();
      assert.match(dock, suffix === "Erfolg" ? /Momentaufnahme geöffnet/ : suffix === "Fehler" ? /Downloadfehler/ : /Abgebrochen/);
      assert.equal(await page.locator("#app").evaluate(node => node.classList.contains("operation-running")), false);
      const opened = await page.evaluate(() => window.__DOPPELDECKER_LAST_REMOTE_OPENED__ ?? null);
      assert.equal(opened !== null, suffix === "Erfolg");
    });
  }

  for (const theme of ["light", "dark"]) {
    await runCase(`AP04R Baumvorschau bindet Backend-ID und zeigt Auslassungen: ${theme}`, "?sftpContents=1&sftpTree=1", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme, false); }, theme);
      await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
      const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
      await list.getByRole("option", { name: /Baum/ }).click(); await list.focus(); await page.keyboard.press("F5");
      await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
      assert.match(await page.locator("#dock-summary").innerText(), /Quellumfang.*ausgelassen/s);
      await page.getByText("Quellgrenzen und Auslassungen anzeigen", { exact: true }).click();
      assert.match(await page.getByRole("textbox", {name:"Quellgrenzen und Auslassungen",exact:true}).inputValue(), /Symlink/);
      const operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation);
      assert.equal(operation.source_tree_expected.omitted.length, 1); assert.equal(operation.overwrite, false);
      await page.locator("#btn-queue").click();
      assert.match(await page.locator("#queue-popover").innerText(), /Bestätigter Quellbaumumfang.*2 Ordner.*1 Eintrag/s);
      if (process.env.AP04R_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.AP04R_EVIDENCE_DIR}/tree-queue-${theme}.png` });
    });
    await runCase(`AP04R Konfliktentscheidung zeigt Quellenrisiko und Auslassungen: ${theme}`, "?sftpContents=1&sftpTree=1&sftpTreeConflict=1", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme, false); }, theme);
      await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
      const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
      await list.getByRole("option", { name: /Baum/ }).click(); await list.focus(); await page.keyboard.press("F5");
      const dialog = page.getByRole("dialog", { name: "Konflikte beim Kopieren" }); await dialog.waitFor();
      const sourceLimits = dialog.getByRole("textbox", { name: "Quellgrenzen und Auslassungen", exact: true });
      assert.equal(await sourceLimits.isVisible(), true);
      assert.match(await sourceLimits.inputValue(), /Best Effort.*nicht atomar.*ausgelassen.*Symlink/s);
      assert.match(await dialog.innerText(), /vorbereitete ausgewählte Kopie/);
      assert.doesNotMatch(await dialog.innerText(), /vollständige ausgewählte Unterbaum/);
      assert.equal(await dialog.evaluate(node => node.getBoundingClientRect().height <= window.innerHeight - 20), true);
      assert.equal(await dialog.getByRole("button", { name: "Abbrechen", exact: true }).isVisible(), true);
      if (process.env.AP04R_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.AP04R_EVIDENCE_DIR}/tree-conflict-${theme}.png` });
      await dialog.getByRole("button", { name: "Abbrechen", exact: true }).click();
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
    });
    await runCase(`AP04R neue Baumwirkung verlangt richtige Phrase bei langer Auslassungsliste: ${theme}`, "", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js"); setTheme(theme, false);
        const omitted = Array.from({ length: 80 }, (_, i) => ({ path: "/Baum/" + "LangerPfad/".repeat(12) + i + ".txt", reason: "Symlink wird nicht verfolgt" }));
        const tree = { digest: "ab".repeat(32), entries: 83, files: 1, directories: 2, bytes: 11, omitted };
        const queue = window.__DOPPELDECKER_MOCK_QUEUE__;queue.paused=true;
        queue.entries=[{ operation_id: "mock-tree-attention", operation: { type: "ProviderFile", kind: "Copy", source: { provider: "Sftp", root: "/" }, target: { provider: "Local", root: "/mock/ziel" }, items: ["/Baum"], destination: "/mock/ziel", overwrite: false, expected_conflicts: [], source_tree_expected: null }, state: "AwaitingConfirmation", attempts: 1, created_unix_ms: Date.now(), progress: {}, error: "SFTP-Baum hat sich geändert", attention: { type: "SourceTreeChanged", reason: "SFTP-Baum hat sich geändert", confirmation: "BAUM MOCK 83", current_tree: tree } }];
        window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
      }, theme);
      await page.locator("#btn-queue").click();
      const row = page.locator('[data-queue-id="mock-tree-attention"]');
      await row.getByRole("button", { name: "Plan bestätigen" }).click();
      let dialog = page.getByRole("dialog", { name: "Quellbaumumfang bestätigen" });await dialog.waitFor();
      assert.match(await dialog.innerText(), /80 Eintrag.*ausgelassen/);
      assert.equal(await dialog.getByRole("button", { name: "OK", exact: true }).isVisible(), true);
      assert.equal(await dialog.evaluate(node => node.getBoundingClientRect().height <= window.innerHeight - 20), true);
      const explanation = dialog.getByRole("region", { name: "Wirkung und Einschränkungen", exact: true });
      assert.equal(await explanation.evaluate(node => node.scrollHeight > node.clientHeight), true);
      assert.match(await explanation.innerText(), /79\.txt/);
      await explanation.focus();await explanation.press("PageDown");
      await page.waitForFunction(() => document.querySelector('[role="region"][aria-label="Wirkung und Einschränkungen"]').scrollTop > 0);
      const inputBounds = await page.locator("#modal-input").boundingBox();
      assert.ok(inputBounds && inputBounds.y >= 0 && inputBounds.y + inputBounds.height <= 480);
      if (process.env.AP04R_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.AP04R_EVIDENCE_DIR}/tree-confirm-${theme}.png` });
      await page.locator("#modal-input").fill("falsch"); await dialog.getByRole("button", { name: "OK", exact: true }).click();
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state), "AwaitingConfirmation");
      if (await page.locator("#queue-popover").isHidden()) await page.locator("#btn-queue").click();
      await row.getByRole("button", { name: "Plan bestätigen" }).click();dialog = page.getByRole("dialog", { name: "Quellbaumumfang bestätigen" });
      await page.locator("#modal-input").fill("BAUM MOCK 83");await dialog.getByRole("button", { name: "OK", exact: true }).click();
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state === "Pending");
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation.source_tree_expected.omitted.length), 80);
    });
  }

  await runCase("AP04 SFTP-Vorbereitung bleibt vor dem Einreihen mit Escape abbrechbar", "?remoteFile=1&sftpContents=1&fileActionPreviewDelay=500", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote\.txt/ }).click();await list.focus();await page.keyboard.press("F5");
    await page.locator("#btn-cancel").waitFor({ state: "visible" });
    assert.match(await page.locator("#dock-summary").innerText(), /Abbrechbare Vorbereitung/);
    await page.keyboard.press("Escape");
    await page.locator("#btn-cancel").waitFor({ state: "hidden" });
    await page.locator("#dock-summary").filter({ hasText: /Abgebrochen/ }).waitFor();
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
    await list.focus();await page.keyboard.press("F5");
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
  });

  for (const theme of ["light", "dark"]) {
    await runCase(`AP04 SFTP-Inhalte zeigen Leserisiko, additive F5 und gesperrtes F6: ${theme}`, "?remoteFile=1&sftpContents=1&snapshotDelay=300", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme, false); }, theme);
      await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
      const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
      const file = list.getByRole("option", { name: /remote\.txt/ });
      await file.click(); await list.focus(); await page.keyboard.press("F5");
      await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
      assert.match(await page.locator("#dock-summary").innerText(), /SFTP-Inhalte.*Best Effort.*nicht atomar/s);
      await page.locator("#btn-queue").click();
      assert.match(await page.locator("#queue-popover").innerText(), /SFTP-Inhalte: Best Effort, additiv/);
      if (process.env.AP04_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.AP04_EVIDENCE_DIR}/browser-queue-${theme}.png` });
      await page.locator("#queue-popover-close").click();
      await file.click(); await list.focus(); await page.keyboard.press("F6");
      await page.locator("#dock-summary").filter({ hasText: /nur additive lokale/ }).waitFor();
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 1);
      await file.dblclick();
      await page.locator("#btn-cancel").waitFor({ state: "visible" });
      assert.match(await page.locator("#dock-summary").innerText(), /Remote-Momentaufnahme.*Best Effort.*Pfadtausch/s);
      await page.keyboard.press("Escape");
      await page.locator("#btn-cancel").waitFor({ state: "hidden" });
      assert.match(await page.locator("#dock-summary").innerText(), /Abgebrochen/);
      await file.dblclick();
      await page.locator("#dock-summary").filter({ hasText: /Momentaufnahme geöffnet/ }).waitFor();
      assert.match(await page.locator("#dock-summary").innerText(), /nicht atomar.*8 GiB/s);
      if (process.env.AP04_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.AP04_EVIDENCE_DIR}/browser-open-${theme}.png` });
      assert.equal(await page.locator("#app").evaluate(node => node.classList.contains("operation-running")), false);
    });
  }

  await runCase("SFTP-v3-Listing zeigt Pfad-Best-Effort und sperrt Quarantänewirkungen", "", async page => {
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ })
      .getByRole("option", { name: /remote/ }).waitFor();

    const pathTitle = await page.locator('.pane[data-side="right"] .path').getAttribute("title");
    assert.match(pathTitle || "", /SFTP-Pane-Listing · pfadbasiertes Best Effort/);
    assert.match(
      await page.locator("#dock-summary").innerText(),
      /Reguläre Dateien.*Momentaufnahme.*Verzeichnisbäume.*additiv lokal.*OPENDIR und OPEN nicht atomar/s,
    );

    const status = await page.evaluate(() => window.__TAURI__.core.invoke(
      "connect_sftp_profile",
      { profileId: "mock_sftp", profileRevision: 11 },
    ));
    assert.deepEqual(status.quarantine_capabilities, {
      listing: false,
      restore: false,
      purge: false,
    });
    const quarantine = page.locator("#btn-quarantine");
    assert.equal(await quarantine.isDisabled(), true);
    assert.match(await quarantine.getAttribute("title") || "", /Quarantäneverwaltung gesperrt/);
    await quarantine.click({ force: true });
    const quarantineCalls = await page.evaluate(() => window.__MOCK_QUARANTINE_CALLS__());
    assert.equal(
      quarantineCalls.some(call => call.command.startsWith("list_provider_quarantine")),
      false,
      "Listing=false darf keinen Provider-Quarantäneabruf starten.",
    );
  });

  await runCase("Langsamer Home-Default überschreibt keine neuere Navigation", "?defaultDelay=120", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "home"));
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.waitForTimeout(160);
    assert.match(await page.getByRole("textbox").first().getAttribute("title") || "", /SFTP · Mock-SFTP/);
  });

  await runCase("Fehlgeschlagenes Zurück blättert die History nicht weiter", "?scanErrorOnSecond=slow", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    const path = pane.locator(".path");
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await path.fill("/mock/slow");
    await path.press("Enter");
    await list.getByRole("option", { name: /Langsam\.txt/ }).waitFor();
    await path.fill("/mock/fast");
    await path.press("Enter");
    await list.getByRole("option", { name: /Schnell\.txt/ }).waitFor();

    await pane.locator(".nav-back").click();
    await page.getByText(/Zweiter Mock-Scanfehler/).waitFor();
    await pane.locator(".nav-back").click();
    await list.getByRole("option", { name: /Langsam\.txt/ }).waitFor();

    const historyCalls = (await page.evaluate(() => window.__MOCK_SCAN_CALLS__()))
      .filter(call => call.command === "scan_dir")
      .map(call => call.path);
    assert.deepEqual(historyCalls.slice(-2), ["/mock/slow", "/mock/slow"],
      "Nach dem Fehler muss Zurück denselben History-Eintrag erneut versuchen.");
  });

  await runCase("Ordner anlegen bricht sichtbar ab, solange die Pane keinen Pfad hat", "?remoteScanError=1", async page => {
    // Regression: Nach einem gescheiterten Endpunktwechsel bleibt `pane.path` leer.
    // Dieser Leerstring ging als `parent` an das Backend und wurde dort relativ zur
    // kanonischen Provider-Root aufgelöst — der Ordner entstand an einer Stelle, die
    // der Nutzer nie gesehen hat. Jetzt muss die Aktion sichtbar abbrechen.
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /Beispiel\.txt/ }).waitFor();
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByText(/Mock-Remote-Scanfehler/).waitFor();

    // Bewusst ohne Rückgabewert auslösen: `__MOCK_EMIT__` reicht das Promise der
    // Aktion durch, und `page.evaluate` würde sonst warten, bis ein etwaiger
    // Namensdialog beantwortet ist — bei einer Regression also endlos.
    await page.evaluate(() => { window.__MOCK_EMIT__("menu", "new_folder"); });
    await page.waitForFunction(() =>
      (document.querySelector("#dock-summary")?.innerText || "").includes("Ordner anlegen"));
    // Weder ein Namensdialog noch ein Backend-Aufruf: nichts wurde angelegt.
    assert.equal(await page.locator("#modal-input").count(), 0);
    assert.equal(
      await page.evaluate(() => document.documentElement.dataset.lastMkdir ?? null),
      null,
      "Ohne Pane-Pfad darf kein create_*_folder abgehen",
    );
  });

  await runCase("Abgesichertes Umbenennen zeigt seine Einschränkung ebenfalls an", "?remoteFile=1", async page => {
    // Regression: Die Einschränkungen hingen an der Klasse `best_effort`. Der Core
    // bindet sie aber an JEDE verfügbare Klasse — bei `assured` blieb die
    // Pfadbindung der Quelle deshalb unsichtbar.
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote\.txt/ }).click();
    await list.focus();
    await page.keyboard.press("F2");
    await page.locator("#modal-input").fill("abgesichert.txt");
    await page.locator("#modal .modal-buttons").getByRole("button", { name: "OK" }).click();
    const dock = page.locator("#dock-summary");
    await page.waitForFunction(() =>
      (document.querySelector("#dock-summary")?.innerText || "").includes("Umbenannt"));
    const text = await dock.innerText();
    assert.match(text, /ausschließlich über ihren Pfad gebunden/);
    assert.doesNotMatch(text, /Best Effort/, "Die Klasse steht nur als Zusatz, nicht falsch");
  });

  await runCase("Bidirektionale Paarvorbereitung bleibt dauerhaft und abbrechbar", "", async page => {
    // Bidirektional ist im Sync-Modus die dritte Richtungsoption.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    await page.locator("#btn-sync").click();
    const dialog = page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /vollständigen Bäume.*Baseline.*Plan-Digest/s);
    const preparation = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
    assert.equal(preparation.calls[0]?.command, "register_bidirectional_pair",
      "Die prozessweite Paarregistrierung muss vor jeder Vorbereitung im Backend liegen.");
    assert.equal(await page.evaluate(() => localStorage.getItem("doppeldecker-bidirectional-pairs-v1")), null,
      "localStorage darf keine konkurrierende Paar-ID-Registry mehr sein.");
    await dialog.getByRole("button", { name: "Abbrechen" }).click();
    assert.equal(await dialog.count(), 0);

    // Pane-Aktionen sind im offenen Sync-Modus absichtlich gesperrt. Für den
    // echten Tausch verlassen wir ihn kurz und öffnen dieselbe Richtung danach
    // erneut; genau dabei darf die dauerhafte Paarrolle nicht umkippen.
    await page.getByRole("button", { name: "Schließen" }).click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "swap_panes"));
    await page.waitForFunction(() =>
      document.querySelector('.pane[data-side="left"] .path')?.value === "/mock/ziel"
      && document.querySelector('.pane[data-side="right"] .path')?.value === "/mock/quelle");
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    assert.equal(await page.locator('.pane[data-side="left"] .path').inputValue(), "/mock/ziel");
    assert.equal(await page.locator('.pane[data-side="right"] .path').inputValue(), "/mock/quelle");
    await page.locator("#btn-sync").click();
    const swappedDialog = page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" });
    await swappedDialog.waitFor();
    const swapped = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
    const registrations = swapped.calls.filter(call => call.command === "register_bidirectional_pair");
    const preparations = swapped.calls.filter(call => call.command === "prepare_bidirectional_sync");
    assert.equal(registrations.length, 2);
    assert.equal(registrations[0].args.left.root, "/mock/quelle");
    assert.equal(registrations[1].args.left.root, "/mock/ziel");
    assert.deepEqual(preparations[0].args.request.pairId, preparations[1].args.request.pairId);
    assert.deepEqual(preparations[0].args.request.left, preparations[1].args.request.left);
    assert.deepEqual(preparations[0].args.request.right, preparations[1].args.request.right);
    await swappedDialog.getByRole("button", { name: "Abbrechen" }).click();
  });

  await runCase("Bidirektionale Konfliktseite folgt nach Pane-Tausch der sichtbaren Rolle", "?bidirectionalConflict=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "swap_panes"));
    await page.waitForFunction(() =>
      document.querySelector('.pane[data-side="left"] .path')?.value === "/mock/ziel");
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Konflikt 1 von 1" })
      .getByRole("button", { name: "Links behalten" }).click();
    const execution = page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" });
    await execution.waitFor();
    const preparations = (await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__()))
      .calls.filter(call => call.command === "prepare_bidirectional_sync");
    assert.equal(preparations.length, 2);
    assert.equal(
      preparations[1].args.request.resolutions[0].decision.decision,
      "keep_right",
      "Die sichtbar linke Pane ist nach dem Tausch die kanonische rechte Paarseite.",
    );
    await execution.getByRole("button", { name: "Abbrechen" }).click();
  });

  await runCase("Direkter bidirektionaler Lauf erlaubt Pane und Pfad bis zum Terminalevent", "?bidirectionalRunDelay=400", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" })
      .getByRole("button", { name: "Direkt bestätigen" }).click();
    const title = await page.locator("#modal-title").innerText();
    const phrase = title.match(/BEIDE [^“]+/)?.[0];
    assert.ok(phrase);
    await page.locator("#modal-input").fill(phrase);
    await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();

    await page.locator("#app.operation-running").waitFor();
    const path = page.locator('.pane[data-side="left"] .path');
    assert.equal(await path.isDisabled(), false);
    assert.equal(await page.locator("#sync-update").isDisabled(), true,
      "Die bearbeitbaren Dateipanes dürfen die Optionen des laufenden Plans nicht freigeben.");
    assert.ok(await page.locator(".pane-tabs button:enabled").count() > 0);
    await page.getByRole("button", { name: "Dateiansicht", exact: true }).click();
    await page.keyboard.press("Control+T");
    assert.equal(await page.locator(".pane-tab-select").count(), 3);
    await path.fill("/mock/fast");
    await path.press("Enter");
    await page.waitForFunction(() => document.querySelector('.pane[data-side="left"] .path').value === "/mock/fast");
    await page.locator("#app:not(.operation-running)").waitFor();
    assert.equal(await path.inputValue(), "/mock/fast", "Der Abschluss darf keine Navigation zurücksetzen.");

  });

  await runCase("Bidirektionales Resume löst Startfehler und erlaubt danach Navigation", "?bidiRecovery=1&bidirectionalRunFailsOnce=1&bidirectionalRunDelay=400", async page => {
    const startResume = async () => {
      await page.locator("#btn-queue").click();
      await page.getByRole("button", { name: "Recovery" }).click();
      await page.getByRole("dialog", { name: "Recovery · Recovery-Paar" })
        .getByRole("button", { name: "Fortsetzen" }).click();
      const title = await page.locator("#modal-title").innerText();
      const phrase = title.match(/BEIDE [^“]+/)?.[0];
      assert.ok(phrase);
      await page.locator("#modal-input").fill(phrase);
      await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
    };
    await startResume();
    await page.getByText(/Mock-Start ist vor der Entnahme belegt/).waitFor();
    assert.equal(await page.locator("#app").getAttribute("class").then(value => value?.includes("operation-running")), false);
    assert.equal(await page.locator('.pane[data-side="left"] .path').isDisabled(), false);

    await startResume();
    await page.locator("#app.operation-running").waitFor();
    assert.equal(await page.locator('.pane[data-side="left"] .path').isDisabled(), false);
    await page.locator("#app:not(.operation-running)").waitFor();
  });

  await runCase("Bidirektionaler Rollback löst Startfehler und erlaubt danach Navigation", "?bidiRecovery=1&bidirectionalRollbackFailsOnce=1&bidirectionalRunDelay=400", async page => {
    const startRollback = async () => {
      await page.locator("#btn-queue").click();
      await page.getByRole("button", { name: "Recovery" }).click();
      await page.getByRole("dialog", { name: "Recovery · Recovery-Paar" })
        .getByRole("button", { name: "Rollback" }).click();
      const title = await page.locator("#modal-title").innerText();
      const phrase = title.match(/ROLLBACK [^“]+/)?.[0];
      assert.ok(phrase);
      await page.locator("#modal-input").fill(phrase);
      await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
    };
    await startRollback();
    await page.getByText(/Mock-Rollbackstart ist belegt/).waitFor();
    assert.equal(await page.locator("#app").getAttribute("class").then(value => value?.includes("operation-running")), false);
    assert.equal(await page.locator('.pane[data-side="left"] .path').isDisabled(), false);

    await startRollback();
    await page.locator("#app.operation-running").waitFor();
    assert.equal(await page.locator('.pane[data-side="left"] .path').isDisabled(), false);
    await page.locator("#app:not(.operation-running)").waitFor();
  });

  await runCase("Schneller Direct-Abschluss wird nicht nachträglich als laufend überschrieben", "?bidirectionalFastDoneBeforeResponse=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" })
      .getByRole("button", { name: "Direkt bestätigen" }).click();
    const title = await page.locator("#modal-title").innerText();
    const phrase = title.match(/BEIDE [^“]+/)?.[0];
    assert.ok(phrase);
    await page.locator("#modal-input").fill(phrase);
    await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
    await page.getByText(/Bidirektional fertig/).waitFor();
    await page.waitForTimeout(50);
    assert.match(await page.locator("#dock").innerText(), /Bidirektional fertig/);
    assert.doesNotMatch(await page.locator("#dock").innerText(), /Bidirektional läuft/);
  });

  await runCase("Schnelles Resume wird nicht nachträglich als laufend überschrieben", "?bidiRecovery=1&bidirectionalFastDoneBeforeResponse=1", async page => {
    await page.locator("#btn-queue").click();
    await page.getByRole("button", { name: "Recovery" }).click();
    await page.getByRole("dialog", { name: "Recovery · Recovery-Paar" })
      .getByRole("button", { name: "Fortsetzen" }).click();
    const title = await page.locator("#modal-title").innerText();
    const phrase = title.match(/BEIDE [^“]+/)?.[0];
    assert.ok(phrase);
    await page.locator("#modal-input").fill(phrase);
    await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
    await page.getByText(/Bidirektional fertig/).waitFor();
    await page.waitForTimeout(50);
    assert.match(await page.locator("#dock").innerText(), /Bidirektional fertig/);
    assert.equal(await page.getByText("Bidirektionales Recovery läuft …", { exact: true }).isVisible(), false);
  });

  await runCase("Schneller Rollback wird nicht nachträglich als laufend überschrieben", "?bidiRecovery=1&bidirectionalFastRollbackBeforeResponse=1", async page => {
    await page.locator("#btn-queue").click();
    await page.getByRole("button", { name: "Recovery" }).click();
    await page.getByRole("dialog", { name: "Recovery · Recovery-Paar" })
      .getByRole("button", { name: "Rollback" }).click();
    const title = await page.locator("#modal-title").innerText();
    const phrase = title.match(/ROLLBACK [^“]+/)?.[0];
    assert.ok(phrase);
    await page.locator("#modal-input").fill(phrase);
    await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
    await page.getByText(/Rollback fertig/).waitFor();
    await page.waitForTimeout(50);
    assert.match(await page.locator("#dock").innerText(), /Rollback fertig/);
    assert.doesNotMatch(await page.locator("#dock").innerText(), /Rollback läuft/);
  });

  await runCase("Schnelle Queue-Freigabe verwirft ihre ältere Command-Antwort", "?bidirectionalFastApproveBeforeResponse=1", async page => {
    await page.locator("#btn-queue").click();
    await page.getByRole("button", { name: "Plan bestätigen" }).click();
    await page.getByRole("dialog", { name: "Unabhängiges Backup bestätigen" })
      .getByRole("button", { name: "Fortfahren" }).click();
    await page.locator("#modal-input").fill("BEIDE SCHNELL 0011223344556677");
    await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
    await page.waitForTimeout(50);
    assert.match(await page.locator("#dock").innerText(), /Bidirektional fertig/);
    assert.doesNotMatch(await page.locator("#dock").innerText(), /Neu bestätigt/);
    assert.equal(await page.getByRole("button", { name: "Plan bestätigen" }).count(), 0,
      "Der ältere AwaitingConfirmation-Snapshot darf den terminalen Eventstand nicht ersetzen.");
  });

  await runCase("Queue bietet Recovery nur für ein wirklich vorhandenes Laufjournal an", "", async page => {
    await page.evaluate(() => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      const operation = suffix => ({
        type: "Bidirectional",
        pair_id: Array(16).fill(suffix === "frueh" ? 8 : 9),
        pair_name: suffix === "frueh" ? "Früher Verbindungsfehler" : "Offenes Laufjournal",
        left: { provider: "Local", root: "/mock/quelle" },
        right: { provider: "Local", root: "/mock/ziel" },
        baseline_path: `/mock/state/${suffix}.json`,
        resolutions: [], expected_baseline_digest: null,
        expected_plan_digest: Array(32).fill(3),
      });
      queue.entries = [
        {
          operation_id: "mock-bidi-frueh", operation: operation("frueh"),
          created_unix_ms: Date.now(), state: "Failed", attempts: 1,
          progress: {}, attention: null, error: "Secret fehlt",
          recovery_available: false,
        },
        {
          operation_id: "mock-bidi-journal", operation: operation("journal"),
          created_unix_ms: Date.now(), state: "Interrupted", attempts: 1,
          progress: {}, attention: null, error: "Teilcommit ist journalisiert",
          recovery_available: true,
        },
      ];
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    });
    await page.locator("#btn-queue").click();
    const early = page.locator('[data-queue-id="mock-bidi-frueh"]');
    const journal = page.locator('[data-queue-id="mock-bidi-journal"]');
    await early.getByRole("button", { name: "Neu prüfen" }).waitFor();
    assert.equal(await early.getByRole("button", { name: "Recovery" }).count(), 0);
    await journal.getByRole("button", { name: "Recovery" }).waitFor();
    assert.equal(await journal.getByRole("button", { name: "Neu prüfen" }).count(), 0);
  });

  for (const theme of ["light", "dark"]) {
    await runCase(`Sync-Review bidirektionale Optionen versprechen nur gültige Wirkung: ${theme}`, "", async page => {
      await page.setViewportSize({ width: 800, height: 600 });
      await page.evaluate(async theme => (await import("/theme.js")).setTheme(theme, false), theme);
      await page.evaluate(() => { window.__MOCK_EMIT__("menu", "sync_mode"); });
      await page.getByLabel("Strikt", { exact: true }).check();
      await page.getByLabel("Vorhandenes aktualisieren", { exact: true }).check();
      await page.getByLabel("In beide Richtungen", { exact: true }).check();
      await page.locator("#btn-sync").click();
      const execution = page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" });
      await execution.waitFor();
      const state = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
      const request = state.calls.find(call => call.command === "prepare_bidirectional_sync").args.request;
      assert.equal(Object.hasOwn(request, "options"), false, "Der bidirektionale Vertrag übernimmt keine einseitigen Optionen");
      await execution.getByRole("button", { name: "Abbrechen", exact: true }).click();
      for (const id of ["sync-effects-group", "sync-compare-group", "sync-metadata-group", "sync-metadata-note-title", "btn-real"]) {
        assert.equal(await page.locator(`#${id}`).isVisible(), false, `${id} darf keine vom bidirektionalen Lauf ignorierte Auswahl versprechen`);
      }
      await page.locator("#sync-hint-text").filter({ hasText: /BLAKE3.*Portable/s }).waitFor();
      assert.match(await page.locator("#sync-metadata-note").innerText(), /Ein Sync ersetzt kein Backup/);
      if (process.env.SYNC_REVIEW_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.SYNC_REVIEW_EVIDENCE_DIR}/bidi-options-${theme}.png` });
      await page.getByLabel("Von links nach rechts", { exact: true }).check();
      for (const id of ["sync-effects-group", "sync-compare-group", "sync-metadata-group", "sync-metadata-note-title", "btn-real"]) assert.equal(await page.locator(`#${id}`).isVisible(), true);
      assert.equal(await page.getByLabel("Strikt", { exact: true }).isChecked(), true);
      assert.equal(await page.getByLabel("Vorhandenes aktualisieren", { exact: true }).isChecked(), true);
      assert.equal(await page.getByLabel("Inhalt (BLAKE3)", { exact: true }).isChecked(), true);
    });
  }

  for (const terminal of [false, true]) {
    await runCase(`Sync-Review bidirektionale Einreihantwort erhält ${terminal ? "Abschluss" : "Rückfrage"}`, "", async page => {
      await page.evaluate(terminal => {
        const original = window.__TAURI__.core.invoke;
        window.__TAURI__.core.invoke = async (command, args) => {
          const result = await original(command, args);
          if (command === "enqueue_bidirectional_sync") {
            const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
            const entry = queue.entries.at(-1);
            entry.attempts = 1;
            entry.state = terminal ? "Completed" : "AwaitingConfirmation";
            if (!terminal) entry.attention = { type: "RemoteRun", reason: "Wirkung erneut bestätigen", confirmation: "BACKEND ONLY" };
            window.__MOCK_EMIT__(terminal ? "bidirectional-done" : "bidirectional-ready", terminal
              ? { operationId: entry.operation_id, attempt: 1, baseline_generation: 17 }
              : "Bidirektionaler Lauf ist geprüft; Queue öffnen und Laufphrase bestätigen");
            window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
            await new Promise(resolve => { window.__BIDI_REPLY_RELEASE__ = resolve; });
          }
          return result;
        };
      }, terminal);
      await page.evaluate(() => { window.__MOCK_EMIT__("menu", "sync_mode"); });
      await page.getByLabel("In beide Richtungen").check();
      await page.locator("#btn-sync").click();
      await page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" }).getByRole("button", { name: "In Queue", exact: true }).click();
      await page.waitForFunction(() => typeof window.__BIDI_REPLY_RELEASE__ === "function");
      const latest = await page.locator("#dock-summary").innerText();
      assert.match(latest, terminal ? /Bidirektional fertig/ : /Bidirektional geprüft/);
      await page.evaluate(async () => { window.__BIDI_REPLY_RELEASE__(); await new Promise(resolve => setTimeout(resolve, 0)); });
      assert.equal(await page.locator("#dock-summary").innerText(), latest, "Die spätere Empfangsbestätigung darf den neueren Queuezustand nicht ersetzen");
      assert.equal(await page.locator("#queue-count").innerText(), terminal ? "0" : "1");
    });
  }

  await runCase("Bidirektionales Enqueue retryt dieselbe Backendvorbereitung", "?bidirectionalEnqueueFailsOnce=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    await page.locator("#btn-sync").click();
    let execution = page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" });
    await execution.getByRole("button", { name: "In Queue" }).click();
    await page.getByText(/Bidirektionales Mock-Enqueue ist vor dem Commit belegt/).waitFor();

    await page.locator("#btn-sync").click();
    execution = page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" });
    await execution.getByRole("button", { name: "In Queue" }).click();
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    const state = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
    assert.deepEqual(state.calls.map(call => call.command), [
      "register_bidirectional_pair",
      "prepare_bidirectional_sync",
      "enqueue_bidirectional_sync",
      "register_bidirectional_pair",
      "enqueue_bidirectional_sync",
    ]);
    assert.equal(state.pendingBidirectional, false);
  });

  await runCase("Bidirektionaler Start retryt nach Busy dieselbe Vorbereitung", "?bidirectionalRunFailsOnce=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    for (let attempt = 0; attempt < 2; attempt += 1) {
      await page.locator("#btn-sync").click();
      const execution = page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" });
      await execution.getByRole("button", { name: "Direkt bestätigen" }).click();
      const phrase = page.getByRole("dialog").locator("#modal-input");
      const title = await page.locator("#modal-title").innerText();
      const expected = title.match(/BEIDE [^“]+/)?.[0];
      assert.ok(expected);
      await phrase.fill(expected);
      await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
      if (attempt === 0) {
        await page.getByText(/Bidirektionaler Mock-Start ist vor der Entnahme belegt/).waitFor();
      }
    }
    const state = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
    assert.deepEqual(state.calls.map(call => call.command), [
      "register_bidirectional_pair",
      "prepare_bidirectional_sync",
      "run_bidirectional_sync",
      "register_bidirectional_pair",
      "run_bidirectional_sync",
    ]);
    assert.equal(state.pendingBidirectional, false);
  });

  await runCase("Direkter bidirektionaler Fehler bleibt als erreichbares Queue-Recovery erhalten", "?bidirectionalDirectJournalFailure=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    await page.locator("#btn-sync").click();
    const execution = page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" });
    await execution.getByRole("button", { name: "Direkt bestätigen" }).click();
    const title = await page.locator("#modal-title").innerText();
    const phrase = title.match(/BEIDE [^“]+/)?.[0];
    assert.ok(phrase);
    await page.locator("#modal-input").fill(phrase);
    await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
    await page.getByText(/offenen Teilcommit/).waitFor();
    await page.locator("#btn-queue").click();
    const row = page.locator('[data-queue-id="mock-1"]');
    await row.getByRole("button", { name: "Recovery" }).waitFor();
    assert.equal(await row.getByRole("button", { name: "Neu prüfen" }).count(), 0);
  });

  await runCase("Bidirektionaler Terminalfehler bietet nur den persistenten Queue-Abschluss an", "?bidirectionalTerminalCommitFailure=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    await page.locator("#btn-sync").click();
    const execution = page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" });
    await execution.getByRole("button", { name: "Direkt bestätigen" }).click();
    const title = await page.locator("#modal-title").innerText();
    const phrase = title.match(/BEIDE [^“]+/)?.[0];
    assert.ok(phrase);
    await page.locator("#modal-input").fill(phrase);
    await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();

    await page.getByText(/Datenwirkung ist abgeschlossen/).waitFor();
    await page.waitForTimeout(100);
    assert.equal(await page.getByText(/Bidirektional fertig/).count(), 0,
      "Ohne bewiesenen terminalen Queuecommit darf kein Erfolgsereignis erscheinen.");
    const before = await page.evaluate(() => structuredClone(window.__DOPPELDECKER_MOCK_QUEUE__));
    assert.equal(before.entries[0].state, "Interrupted");
    assert.equal(before.entries[0].recovery_available, false);
    assert.equal(before.entries[0].bidirectional_completion_recovery, "complete");

    await page.locator("#btn-queue").click();
    const row = page.locator('[data-queue-id="mock-1"]');
    await row.getByRole("button", { name: "Abschluss prüfen" }).waitFor();
    assert.equal(await row.getByRole("button", { name: "Neu prüfen" }).count(), 0);
    assert.equal(await row.getByRole("button", { name: "Recovery", exact: true }).count(), 0);
    await row.getByRole("button", { name: "Abschluss prüfen" }).click();
    await page.getByText(/bereits ausgeführte bidirektionale Lauf ist jetzt terminal/).waitFor();
    const after = await page.evaluate(() => structuredClone(window.__DOPPELDECKER_MOCK_QUEUE__));
    assert.equal(after.entries[0].state, "Completed");
    assert.equal(after.entries[0].bidirectional_completion_recovery, null);
  });

  await runCase("Terminale Abschlussmarker bleiben offen und mutationslos bedienbar", "", async page => {
    const completed = await page.evaluate(() =>
      window.__DOPPELDECKER_MOCK_ADD_BIDI_COMPLETION__("complete"));
    await page.waitForFunction(() =>
      document.querySelector("#queue-count")?.textContent === "1");
    await page.locator("#btn-queue").click();
    let row = page.locator(`[data-queue-id="${completed.operation_id}"]`);
    await row.getByRole("button", { name: "Abschluss prüfen" }).waitFor();
    assert.equal(await row.getByRole("button", { name: "Neu prüfen" }).count(), 0);
    await row.getByRole("button", { name: "Abschluss prüfen" }).click();
    await page.getByText(/bereits ausgeführte bidirektionale Lauf ist jetzt terminal/).waitFor();
    await page.waitForFunction(() =>
      document.querySelector("#queue-count")?.textContent === "0");

    const cancelled = await page.evaluate(() =>
      window.__DOPPELDECKER_MOCK_ADD_BIDI_COMPLETION__("rolled_back"));
    await page.waitForFunction(() =>
      document.querySelector("#queue-count")?.textContent === "1");
    await page.locator("#btn-queue").click();
    row = page.locator(`[data-queue-id="${cancelled.operation_id}"]`);
    await row.getByRole("button", { name: "Abschluss prüfen" }).waitFor();
    assert.equal(await row.getByRole("button", { name: "Neu prüfen" }).count(), 0);
    await row.getByRole("button", { name: "Abschluss prüfen" }).click();
    await page.getByText(/bereits ausgeführte bidirektionale Lauf ist jetzt terminal/).waitFor();
    await page.waitForFunction(() =>
      document.querySelector("#queue-count")?.textContent === "0");
  });

  await runCase(
    "Recovery-Discard vor Zustellung bleibt identitätsgebunden und retryfähig",
    "?bidiRecovery=1&bidirectionalRecoveryBackupRequired=1&bidirectionalRecoveryDiscardBeforeDeliveryFailures=2",
    async page => {
      await page.locator("#btn-queue").click();
      await page.getByRole("button", { name: "Recovery" }).click();
      await page.getByRole("dialog", { name: "Recovery · Recovery-Paar" })
        .getByRole("button", { name: "Fortsetzen" }).click();
      await page.getByRole("dialog", { name: "Unabhängiges Backup bestätigen" })
        .getByRole("button", { name: "Abbrechen" }).click();
      await page.getByText(/Mock-Recovery-Discard scheiterte vor Zustellung/).waitFor();
      let state = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
      assert.equal(state.pendingBidirectional, true,
        "Ein nicht zugestellter Discard darf den Backend-Slot nicht als frei behandeln.");

      await page.locator("#btn-queue").click();
      await page.getByRole("button", { name: "Recovery" }).click();
      await page.getByRole("dialog", { name: "Recovery · Recovery-Paar" })
        .getByRole("button", { name: "Fortsetzen" }).click();
      await page.getByRole("dialog", { name: "Unabhängiges Backup bestätigen" })
        .getByRole("button", { name: "Abbrechen" }).click();
      await page.waitForFunction(() => !window.__MOCK_PREPARATION_ACTIONS__().pendingBidirectional);
      state = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
      const discards = state.calls.filter(call =>
        call.command === "discard_bidirectional_recovery_preparation");
      assert.ok(discards.length >= 4);
      assert.equal(discards[0].args.recoveryPreparationId, discards[1].args.recoveryPreparationId);
      assert.equal(discards[1].args.recoveryPreparationId, discards[2].args.recoveryPreparationId,
        "Der Retry muss weiter exakt die unbestätigte alte Vorbereitung meinen.");
      assert.notEqual(discards.at(-1).args.recoveryPreparationId, discards[0].args.recoveryPreparationId,
        "Eine frisch vorbereitete Recovery braucht eine neue Identität.");
    },
  );

  await runCase(
    "Recovery-Discard mit verlorener Antwort liest idempotent die Abwesenheit zurück",
    "?bidiRecovery=1&bidirectionalRecoveryStageBlocker=1&bidirectionalRecoveryDiscardActsThenFailsOnce=1",
    async page => {
      await page.locator("#btn-queue").click();
      await page.getByRole("button", { name: "Recovery" }).click();
      await page.getByRole("dialog", { name: "Recovery · Recovery-Paar" })
        .getByRole("button", { name: "Fortsetzen" }).click();
      await page.getByText(/Mock-Recovery-Discard wirkte, Antwort ging verloren/).waitFor();
      const state = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
      assert.equal(state.pendingBidirectional, false);
      const discards = state.calls.filter(call =>
        call.command === "discard_bidirectional_recovery_preparation");
      assert.equal(discards.length, 2,
        "Nach verlorenem Ergebnis muss derselbe idempotente Discard die Abwesenheit bestätigen.");
      assert.equal(discards[0].args.recoveryPreparationId, discards[1].args.recoveryPreparationId);
    },
  );

  await runCase(
    "Recovery-Discard nach falscher Phrase zeigt den Transportfehler",
    "?bidiRecovery=1&bidirectionalRecoveryDiscardBeforeDeliveryFailures=1",
    async page => {
      await page.locator("#btn-queue").click();
      await page.getByRole("button", { name: "Recovery" }).click();
      await page.getByRole("dialog", { name: "Recovery · Recovery-Paar" })
        .getByRole("button", { name: "Fortsetzen" }).click();
      const confirmation = page.getByRole("dialog").locator("#modal-input");
      await confirmation.fill("falsche Phrase");
      await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
      await page.getByText(/Mock-Recovery-Discard scheiterte vor Zustellung/).waitFor();
      const state = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
      assert.equal(state.pendingBidirectional, false);
      const discards = state.calls.filter(call =>
        call.command === "discard_bidirectional_recovery_preparation");
      assert.equal(discards.length, 2);
      assert.equal(discards[0].args.recoveryPreparationId, discards[1].args.recoveryPreparationId);
    },
  );

  await runCase("Mirror-Bestätigung verwendet den sichtbaren Zielnamen", "?realEnabled=1&mirrorDelete=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("Verwaiste in Quarantäne verschieben").check();
    await page.locator("#btn-sync").click();
    const impact = page.getByRole("dialog", { name: "Zielbestand wird verändert" });
    await impact.waitFor();
    await impact.getByRole("button", { name: "Fortfahren" }).click();
    const phrase = page.getByRole("dialog", { name: "Zur Bestätigung Zielnamen „ziel“ eingeben" });
    await phrase.waitFor();
    await phrase.locator("#modal-input").fill("ziel");
    await phrase.getByRole("button", { name: "OK" }).click();
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
  });

  await runCase("Queue-Pause, Driftphrase und Entfernen", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#btn-sync-simulate").click();
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    await page.locator("#btn-queue").click();
    await page.getByRole("button", { name: "Bei Fehler: weiter" }).click();
    await page.getByRole("button", { name: "Bei Fehler: anhalten" }).waitFor();
    await page.getByRole("button", { name: "Pausieren" }).click();
    const resume = page.getByRole("button", { name: "Fortsetzen" });
    await resume.waitFor();
    await resume.click();
    const approve = page.getByRole("button", { name: "Plan bestätigen" });
    await approve.waitFor();
    await approve.click();
    let title = await page.locator("#modal-title").innerText();
    let phrase = title.match(/PLAN [^“]+/)?.[0];
    assert.ok(phrase, "Queue-Phrase fehlt im Dialogtitel.");
    await page.locator("#modal-input").fill("falsch");
    await page.getByRole("button", { name: "OK" }).click();
    await page.locator("#btn-queue").click();
    assert.match(await page.locator("#queue-popover").innerText(), /neue Bestätigung nötig/);
    await page.getByRole("button", { name: "Plan bestätigen" }).click();
    title = await page.locator("#modal-title").innerText();
    phrase = title.match(/PLAN [^“]+/)?.[0];
    assert.ok(phrase);
    await page.locator("#modal-input").fill(phrase);
    await page.getByRole("button", { name: "OK" }).click();
    await page.locator("#btn-queue").click();
    const remove = page.getByRole("button", { name: "Entfernen" });
    await remove.waitFor();
    await remove.click();
    assert.match(await page.locator("#queue-popover-list").innerText(), /Queue ist leer/);
  });

  for (const attentionType of ["LocalFileCommit", "BestEffortCommitDowngrade"]) {
    await runCase(`${attentionType}: Best-Effort-Bestätigung trennt Risiko und Eingabe`, "", async page => {
      await page.setViewportSize({ width: 800, height: 600 });
      await page.evaluate(attentionType => {
        const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
        const reason = "Noch nichts übertragen. Ziel: /mock/<fremd>. Ein gleichzeitig neu angelegtes Ziel kann ersetzt werden." + " Die Grenzen bleiben vollständig lesbar.".repeat(80);
        queue.entries = [{
          operation_id: "local-commit-proof",
          operation: { type: "File", kind: "Copy", items: ["/mock/quelle/datei.txt"], destination: "/mock/<fremd>", overwrite: false, expected_conflicts: [] },
          created_unix_ms: Date.now(), state: "AwaitingConfirmation", attempts: 1, progress: {},
          attention: { type: attentionType, reason, confirmation: "BEST EFFORT local-proof", target_identity: {} },
          error: reason,
        }];
        window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
      }, attentionType);
      await page.locator("#btn-queue").click();
      await page.getByRole("button", { name: "Plan bestätigen" }).click();
      const dialog = page.getByRole("dialog", { name: "Best-Effort-Kopie bestätigen", exact: true });
      await dialog.waitFor();
      assert.match(await dialog.locator(".modal-description").innerText(), /Ziel: \/mock\/<fremd>/);
      assert.equal(await dialog.locator("fremd").count(), 0, "Zieltext darf kein Markup erzeugen");
      const input = dialog.getByLabel('Exakt „BEST EFFORT local-proof“ eingeben');
      const title = await dialog.locator(".modal-title").boundingBox();
      const confirm = await dialog.getByRole("button", { name: "OK", exact: true }).boundingBox();
      const inputBounds = await input.boundingBox();
      assert.ok(title && title.height < 50, "Der Titel darf nicht die lange Risikobilanz aufnehmen");
      assert.ok(confirm && confirm.y >= 0 && confirm.y + confirm.height <= 600, "Bestätigung bleibt im Fenster erreichbar");
      assert.ok(inputBounds && inputBounds.y >= 0 && inputBounds.y + inputBounds.height <= 600, "Das fokussierte Eingabefeld bleibt erreichbar");
      await input.press("Shift+Tab");
      const explanation = dialog.getByRole("region", { name: "Wirkung und Einschränkungen", exact: true });
      assert.ok(await explanation.evaluate(element => element === document.activeElement));
      await explanation.press("PageDown");
      await page.waitForFunction(() => document.querySelector(".modal-description-scroll").scrollTop > 0);
      await explanation.press("Tab");
      assert.ok(await input.evaluate(element => element === document.activeElement));
      await input.fill("falsch");
      await input.press("Enter");
      const queue = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__);
      assert.equal(queue.entries[0].state, "AwaitingConfirmation");
    });
  }

  await runCase("Nicht gestarteter WebDAV-Altplan lässt sich typisiert verwerfen", "?legacyWebdav=failed", async page => {
    await page.locator("#btn-queue").click();
    const popover = page.getByRole("dialog", { name: "Operations-Queue" });
    assert.equal(await popover.getByRole("button", { name: "Neu prüfen" }).count(), 0);
    await popover.getByRole("button", { name: "Nicht gestarteten Altplan verwerfen" }).click();
    await popover.getByText("Queue ist leer").waitFor();
  });

  await runCase("Mögliche WebDAV-Altteilwirkung verlangt Prüfung und schließt terminal", "?legacyWebdav=interrupted", async page => {
    await page.locator("#btn-queue").click();
    const popover = page.getByRole("dialog", { name: "Operations-Queue" });
    assert.equal(await popover.getByRole("button", { name: "Neu prüfen" }).count(), 0);
    await popover.getByRole("button", { name: "Wirkung geprüft und schließen" }).click();
    const confirmation = page.getByRole("dialog", { name: "Alten WebDAV-Löschlauf schließen" });
    await confirmation.waitFor();
    await confirmation.getByRole("button", { name: "Fortfahren" }).click();
    // Der globale Außenklick schließt das Popover, sobald der getrennte
    // Bestätigungsdialog bedient wird. Danach den persistenten Terminalstand neu öffnen.
    await page.locator("#btn-queue").click();
    await popover.getByText(/Manuelle Prüfung wurde bestätigt/).waitFor();
    assert.match(await popover.innerText(), /abgebrochen/i);
    assert.equal(await popover.getByRole("button", { name: "Neu prüfen" }).count(), 0);
  });

  await runCase("Backup-Abbruch hält das offene Queue-Popover und seinen Fokus", "", async page => {
    await page.evaluate(() => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      queue.entries = [{
        operation_id: "mock-backup-attention",
        operation: {
          type: "ProviderSync",
          source: { provider: "Local", root: "/mock/quelle" },
          target: { provider: "Sftp", root: "/backup" },
          options: { mode: "Update" },
        },
        created_unix_ms: Date.now(),
        state: "AwaitingConfirmation",
        attempts: 1,
        progress: {},
        attention: { type: "RemoteRun", confirmation: "BACKUP MOCK" },
        error: "Backup muss erneut bestätigt werden",
      }];
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    });
    await page.locator("#btn-queue").click();
    const popover = page.getByRole("dialog", { name: "Operations-Queue" });
    const approve = popover.getByRole("button", { name: "Plan bestätigen" });
    await approve.click();
    await page.getByRole("dialog", { name: "Unabhängiges Backup bestätigen" })
      .getByRole("button", { name: "Abbrechen" }).click();
    await popover.waitFor();
    assert.equal(await page.locator("#queue-popover").getAttribute("hidden"), null);
    await page.waitForFunction(() => document.activeElement?.dataset.queueAction === "approve");
    assert.equal(await page.evaluate(() => document.activeElement?.dataset.queueAction), "approve");
  });

  await runCase("Verwerfen warnt vor früherer Wirkung, respektiert Pause und erhält Fokus", "", async page => {
    // Ohne einen Ablehnungsweg bliebe die Zustimmung der einzige Ausgang aus
    // `AwaitingConfirmation` — der Eintrag stünde dauerhaft vorne und hielte
    // jeden nachfolgenden auf.
    await page.evaluate(() => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      queue.paused = true;
      queue.entries = [{
        operation_id: "mock-conflict-attention",
        operation: {
          type: "File", kind: "Copy", items: ["/mock/quelle/datei.txt"],
          destination: "/mock/ziel", overwrite: true, expected_conflicts: [],
        },
        created_unix_ms: Date.now(),
        state: "AwaitingConfirmation",
        attempts: 2,
        progress: {},
        attention: {
          type: "ConflictsChanged",
          reason: "Die Konfliktliste hat sich geändert",
          confirmation: "KONFLIKT MOCK",
          current_conflicts: ["/mock/ziel/datei.txt"],
        },
        error: "Die Konfliktliste hat sich geändert",
      }, {
        operation_id: "mock-wartet-dahinter",
        operation: {
          type: "File", kind: "Copy", items: ["/mock/quelle/zweite.txt"],
          destination: "/mock/ziel", overwrite: false, expected_conflicts: [],
        },
        created_unix_ms: Date.now(),
        state: "Pending",
        attempts: 0,
        progress: {},
        attention: null,
        error: null,
      }];
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    });
    await page.locator("#btn-queue").click();
    const popover = page.getByRole("dialog", { name: "Operations-Queue" });
    const row = popover.locator('[data-queue-id="mock-conflict-attention"]');
    await row.getByRole("button", { name: "Verwerfen" }).click();
    let confirmation = page.getByRole("dialog", { name: "Operation verwerfen" });
    await confirmation.waitFor();
    assert.match(await confirmation.innerText(), /früherer Versuch kann Quelle oder Ziel bereits verändert haben/i);
    assert.match(await confirmation.innerText(), /Queue bleibt pausiert/i);
    await confirmation.getByRole("button", { name: "Abbrechen" }).click();
    await popover.waitFor();
    await page.waitForFunction(() => document.activeElement?.dataset.queueAction === "discard");
    assert.equal(await page.evaluate(() => document.activeElement?.dataset.queueAction), "discard");

    await row.getByRole("button", { name: "Verwerfen" }).click();
    confirmation = page.getByRole("dialog", { name: "Operation verwerfen" });
    await confirmation.waitFor();
    await confirmation.getByRole("button", { name: "Fortfahren" }).click();
    await page.waitForFunction(() =>
      window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state === "Cancelled");
    const entries = await page.evaluate(() =>
      window.__DOPPELDECKER_MOCK_QUEUE__.entries.map(entry => entry.state));
    assert.deepEqual(entries, ["Cancelled", "Pending"],
      "Nur der abgelehnte Eintrag wird verworfen; der wartende bleibt erhalten.");
    // Der verworfene Eintrag zählt nicht mehr als offen.
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
    assert.match(await page.locator("#dock").innerText(), /Queue bleibt pausiert/i);
  });

  for (const theme of ["light", "dark"]) {
    await runCase(`FTPS-Probenrest zeigt Server und Root ohne Wiederholung: ${theme}`, "", async page => {
      await page.evaluate(theme => {
        document.documentElement.dataset.theme = theme;
        const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
        queue.entries = [{
          operation_id: "ftps-preparation-recovery",
          operation: {
            type: "FtpsRecovery",
            identity: { server: { host: "test.example", port: 2121, transport_security: "explicit_tls" }, virtual_root: "/pane" },
            account_digest: Array(32).fill(1),
          },
          created_unix_ms: Date.now(), state: "Interrupted", attempts: 1, progress: {},
          attention: null, error_code: "outcome_unclear",
          error: "FTPS-Probe offen; STOU-Name unbekannt. Bestand manuell prüfen.",
          temp_recovery_paths: ["/pane/.doppeldecker-probe-own", "/private/config/.neighbor.recovery.json"],
        }];
        window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
      }, theme);
      await page.setViewportSize({ width: 800, height: 600 });
      await page.locator("#btn-queue").click();
      const popover = page.getByRole("dialog", { name: "Operations-Queue" });
      const row = popover.locator('[data-queue-id="ftps-preparation-recovery"]');
      await row.getByText("FTPS-Vorbereitung prüfen · test.example:2121 · /pane", { exact: true }).waitFor();
      assert.match(await row.innerText(), /STOU-Name unbekannt/);
      await row.getByText("/pane/.doppeldecker-probe-own", { exact: true }).waitFor();
      assert.equal(await row.getByRole("button", { name: "Neu prüfen" }).count(), 0);
      await row.getByRole("button", { name: "Wirkung geprüft und schließen" }).click();
      const confirmation = page.getByRole("dialog", { name: "Unklaren Ausgang schließen" });
      await confirmation.waitFor();
      assert.match(await confirmation.innerText(), /wiederholt die Operation nicht/i);
      await confirmation.getByRole("button", { name: "Abbrechen" }).click();
      await row.waitFor();
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state), "Interrupted");
    });
  }

  await runCase("Unklarer Queue-Ausgang lässt sich nicht wiederholen", "", async page => {
    await page.evaluate(() => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      queue.entries = [{
        operation_id: "mock-outcome-unclear",
        operation: {
          type: "File", kind: "Move", items: ["/mock/quelle/datei.txt"],
          destination: "/mock/ziel", overwrite: true, expected_conflicts: [],
        },
        created_unix_ms: Date.now(),
        state: "Failed",
        attempts: 1,
        progress: {},
        attention: null,
        error: "Antwort ging nach der Mutation verloren",
        error_code: "outcome_unclear",
      }];
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    });
    await page.locator("#btn-queue").click();
    const popover = page.getByRole("dialog", { name: "Operations-Queue" });
    assert.equal(await popover.getByRole("button", { name: "Neu prüfen" }).count(), 0);
    const acknowledge = popover.getByRole("button", { name: "Wirkung geprüft und schließen" });
    await acknowledge.click();
    const confirmation = page.getByRole("dialog", { name: "Unklaren Ausgang schließen" });
    await confirmation.waitFor();
    assert.match(await confirmation.innerText(), /wiederholt die Operation nicht/i);
    await confirmation.getByRole("button", { name: "Fortfahren" }).click();
    await popover.waitFor();
    await popover.getByText(/ohne Wiederholung geschlossen/).waitFor();
    assert.equal(await popover.getByRole("button", { name: "Neu prüfen" }).count(), 0);
  });

  await runCase("Migrierter Remote-Altplan lässt sich nur verwerfen", "", async page => {
    await page.evaluate(() => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      queue.entries = [{
        operation_id: "mock-replan-required",
        operation: {
          type: "ProviderSync",
          source: { provider: "Local", root: "/mock/quelle" },
          target: { provider: "Sftp", root: "/backup" },
          options: { mode: "AddOnly" },
        },
        created_unix_ms: Date.now(),
        state: "Failed",
        attempts: 0,
        progress: {},
        attention: null,
        error: "Der alte Remote-Plan braucht eine neue Vorbereitung.",
        error_code: "replan_required",
      }];
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    });
    await page.locator("#btn-queue").click();
    const popover = page.getByRole("dialog", { name: "Operations-Queue" });
    assert.equal(await popover.getByRole("button", { name: "Neu prüfen" }).count(), 0);
    await popover.getByRole("button", { name: "Altplan verwerfen" }).click();
    await popover.getByText("Die Operations-Queue ist leer.").waitFor();
  });

  await runCase("Wartende Remote-Vorbereitung bietet kein direktes Verwerfen", "", async page => {
    // Permit und Verbindung dieses Laufs leben im Backend-RAM. Sie werden über
    // ihren eigenen Weg abgebaut, nicht über ein Journal-Verwerfen.
    await page.evaluate(() => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      queue.entries = [{
        operation_id: "mock-remote-attention",
        operation: {
          type: "ProviderSync",
          source: { provider: "Local", root: "/mock/quelle" },
          target: { provider: "Sftp", root: "/backup" },
          options: { mode: "AddOnly" },
        },
        created_unix_ms: Date.now(),
        state: "AwaitingConfirmation",
        attempts: 1,
        progress: {},
        attention: { type: "RemoteRun", reason: "Lauf bestätigen", confirmation: "LAUF MOCK" },
        error: "Lauf bestätigen",
      }];
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    });
    await page.locator("#btn-queue").click();
    const popover = page.getByRole("dialog", { name: "Operations-Queue" });
    await popover.getByRole("button", { name: "Plan bestätigen" }).waitFor();
    assert.equal(await popover.getByRole("button", { name: "Verwerfen" }).count(), 0);
  });

  await runCase("Queue-Popover pinnt den laufenden Eintrag und bleibt schmal bedienbar", "?runningSmb=1", async page => {
    await page.setViewportSize({ width: 390, height: 620 });
    await page.locator("#btn-queue").click();
    const popover = page.getByRole("dialog", { name: "Operations-Queue" });
    await popover.waitFor();
    const bounds = await popover.boundingBox();
    assert.ok(bounds);
    assert.ok(bounds.x >= 0 && bounds.x + bounds.width <= 390, "Popover ragt seitlich aus dem Fenster.");
    assert.ok(bounds.y >= 0 && bounds.y + bounds.height <= 620, "Popover ragt vertikal aus dem Fenster.");
    assert.match(await page.locator("#queue-popover-running").innerText(), /SMB AddOnly.*läuft/s);
    assert.equal(await page.locator("#queue-popover-running .queue-row").count(), 1);
    assert.equal(await page.locator("#queue-popover-list .queue-row").count(), 0,
      "Der angeheftete laufende Eintrag darf nicht zusätzlich in der Scrollliste stehen.");
    await page.getByRole("button", { name: "Pausieren" }).waitFor();
    await page.getByRole("button", { name: "Bei Fehler: weiter" }).waitFor();
    await page.keyboard.press("Escape");
    assert.equal(await popover.count(), 0);
    assert.equal(await page.evaluate(() => document.activeElement?.id), "btn-queue");
  });

  await runCase("Queue-Umsortieren erhält den Tastaturfokus", "?pendingQueue=1", async page => {
    const operationIds = await page.evaluate(() =>
      window.__DOPPELDECKER_MOCK_QUEUE__.entries.map(entry => entry.operation_id));
    const movedId = operationIds[0];
    await page.locator("#btn-queue").click();
    const firstDown = page.locator(`[data-queue-id="${movedId}"] [data-queue-action="down"]`);
    await firstDown.focus();
    await firstDown.press("Enter");
    await page.waitForFunction(id => {
      const rows = [...document.querySelectorAll("#queue-popover-list [data-queue-id]")];
      return rows[1]?.dataset.queueId === id
        && document.activeElement?.dataset.queueAction === "down"
        && document.activeElement?.closest("[data-queue-id]")?.dataset.queueId === id;
    }, movedId);

    // Ohne einen erneuten Klick muss dieselbe Taste die Operation ein zweites
    // Mal verschieben können. Das belegt auch den verspäteten Queue-Event-Pfad.
    await page.keyboard.press("Enter");
    await page.waitForFunction(id => {
      const rows = [...document.querySelectorAll("#queue-popover-list [data-queue-id]")];
      return rows[2]?.dataset.queueId === id
        && document.activeElement?.dataset.queueAction === "down"
        && document.activeElement?.closest("[data-queue-id]")?.dataset.queueId === id;
    }, movedId);
  });

  await runCase("Beenden-Hinweis erklärt und erhält die persistente Queue", "?runningSmb=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("queue-exit-requested", {
      token: "exit-mock-1", openEntries: 1,
    }));
    const dialog = page.getByRole("dialog", { name: "Doppeldecker beenden?" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /persistenten Queue.*gespeichert.*unterbrochen/s);
    await dialog.getByRole("button", { name: "Hier bleiben" }).click();
    assert.equal(await dialog.count(), 0);

    await page.evaluate(() => window.__MOCK_EMIT__("queue-exit-requested", {
      token: "exit-mock-2", openEntries: 1,
    }));
    await dialog.waitFor();
    await dialog.getByRole("button", { name: "Trotzdem beenden" }).click();
    assert.equal(await page.locator("html").getAttribute("data-quit-after-queue-warning"), "true");
    await page.waitForFunction(() =>
      document.documentElement.dataset.quitAfterQueueToken === "exit-mock-2");
  });

  await runCase("Beenden verwendet ausschließlich den neuesten fachlichen Token", "", async page => {
    await page.evaluate(() => {
      window.__MOCK_EMIT__("queue-exit-requested", { token: "exit-alt", openEntries: 1 });
      window.__MOCK_EMIT__("queue-exit-requested", { token: "exit-neu", openEntries: 2 });
    });
    const dialog = page.getByRole("dialog", { name: "Doppeldecker beenden?" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /2 offene/);
    await dialog.getByRole("button", { name: "Trotzdem beenden" }).click();
    await page.waitForFunction(() =>
      document.documentElement.dataset.quitAfterQueueToken === "exit-neu");
  });

  await runCase("Verspätete Quarantäne-Antwort kann Exit weder verdrängen noch überschreiben", "?quarantineData=1&quarantineOpenDelay=1", async page => {
    await page.locator("#btn-quarantine").click();
    await page.evaluate(() => window.__MOCK_EMIT__("queue-exit-requested", {
      token: "exit-vor-quarantaene", openEntries: 1,
    }));
    const exit = page.getByRole("dialog", { name: "Doppeldecker beenden?" });
    await exit.waitFor();
    await page.waitForTimeout(190);
    assert.equal(await page.locator("#modal-title").innerText(), "Doppeldecker beenden?");
    await exit.getByRole("button", { name: "Hier bleiben" }).click();
    await page.getByRole("dialog", { name: "Quarantäne" }).waitFor();
  });

  await runCase("Laufender SMB-Queue-Eintrag nutzt Sync-Fortschritt", "?runningSmb=1", async page => {
    await page.locator("#prog-text").filter({ hasText: "Sync-Ausführung startet …" }).waitFor();
    assert.match(await page.locator("#dock-summary").innerText(), /1 kopieren/);
    assert.doesNotMatch(await page.locator("#prog-text").innerText(), /Verschiebe aus Queue/);
  });

  await runCase("Laufender bidirektionaler Queue-Eintrag wird nicht als Move angezeigt", "?runningBidirectional=1", async page => {
    await page.locator("#prog-text").filter({ hasText: "Bidirektionaler Sync aus Queue …" }).waitFor();
    assert.doesNotMatch(await page.locator("#prog-text").innerText(), /Verschiebe aus Queue/);
  });

  await runCase("Fortschritt trennt Metadaten, Quarantäne und endgültiges Löschen", "", async page => {
    for (const [phase, text] of [
      ["metadata", "Übertrage Metadaten"],
      ["quarantine", "Sichere"],
      ["delete", "Lösche"],
    ]) {
      await page.evaluate(value => window.__MOCK_EMIT__("fileop-progress", {
        phase: value, files_done: 1, files_total: 2, bytes_done: 0, bytes_total: 0, current: "beispiel.txt",
      }), phase);
      const expected = `${text} 1/2 · beispiel.txt`;
      await page.waitForFunction(value => document.querySelector("#prog-text")?.textContent === value, expected);
    }
  });

  await runCase("Quarantäne-Ansicht bleibt explizit und nicht automatisch löschend", "", async page => {
    await page.locator("#btn-quarantine").click();
    const dialog = page.getByRole("dialog", { name: "Quarantäne" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /Es wird nichts automatisch gelöscht/);
    assert.match(await dialog.innerText(), /Keine manifestierten Quarantäneläufe/);
    await dialog.getByRole("button", { name: "Schließen" }).click();
  });

  for (const [status, label] of [
    ["live", /laufender Lauf/],
    ["purged", /Recovery-Tombstone bleibt sichtbar/],
    ["zukuenftig", /unbekannter Laufstatus · Wirkungen gesperrt/],
  ]) {
    await runCase(`Quarantäne sperrt Wirkungen für Laufstatus ${status}`,
      `?quarantineData=1&quarantineStatus=${status}`, async page => {
        await page.locator("#btn-quarantine").click();
        const overview = page.getByRole("dialog", { name: "Quarantäne" });
        assert.match(await overview.innerText(), label);
        await overview.getByRole("button", { name: "Anzeigen" }).click();
        const run = page.getByRole("dialog", { name: "Quarantäne · run-1" });
        await run.waitFor();
        assert.equal(await run.getByRole("button", { name: "Wiederherstellen" }).isDisabled(), true);
        assert.equal(await run.getByRole("button", { name: "Lauf endgültig leeren" }).isDisabled(), true);
        await run.getByRole("button", { name: "Schließen" }).click();
      });
  }

  await runCase("Quarantäne-Purge-Recovery erlaubt nur erneutes Leeren",
    "?quarantineData=1&quarantineStatus=purge_recovery", async page => {
      await page.locator("#btn-quarantine").click();
      const overview = page.getByRole("dialog", { name: "Quarantäne" });
      assert.match(await overview.innerText(), /Recovery nach begonnenem Leeren/);
      await overview.getByRole("button", { name: "Anzeigen" }).click();
      const run = page.getByRole("dialog", { name: "Quarantäne · run-1" });
      await run.waitFor();
      assert.equal(await run.getByRole("button", { name: "Wiederherstellen" }).isDisabled(), true);
      assert.equal(await run.getByRole("button", { name: "Lauf endgültig leeren" }).isDisabled(), false);
      await run.getByRole("button", { name: "Schließen" }).click();
    });

  await runCase("Quarantäne-Version 2 bleibt sichtbar, aber nur manuell rettbar",
    "?quarantineData=1&quarantineManualRecovery=1", async page => {
      await page.waitForFunction(() => document.querySelector("#quarantine-count")?.textContent === "0");
      assert.match(await page.locator("#btn-quarantine").getAttribute("title"), /0 automatisch .*1 nur manuell rettbar/);
      await page.locator("#btn-quarantine").click();
      const overview = page.getByRole("dialog", { name: "Quarantäne" });
      assert.match(await overview.innerText(), /1 nur manuell rettbar/);
      await overview.getByRole("button", { name: "Anzeigen" }).click();
      const run = page.getByRole("dialog", { name: "Quarantäne · run-1" });
      await run.waitFor();
      assert.match(await run.innerText(), /nur manuelle Recovery möglich/);
      assert.equal(await run.getByRole("button", { name: "Wiederherstellen" }).isDisabled(), true);
    });

  await runCase("Fehlgeschlagener Purge lädt Recovery frisch und sperrt Restore",
    "?quarantineData=1&quarantinePurgeError=1", async page => {
      await page.locator("#btn-quarantine").click();
      await page.getByRole("dialog", { name: "Quarantäne" })
        .getByRole("button", { name: "Anzeigen" }).click();
      let run = page.getByRole("dialog", { name: "Quarantäne · run-1" });
      await run.getByRole("button", { name: "Lauf endgültig leeren" }).click();
      await page.locator("#modal-input").fill("LEEREN run-1");
      await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();

      run = page.getByRole("dialog", { name: "Quarantäne · run-1" });
      await page.waitForFunction(() => Array.from(document.querySelectorAll("#modal button"))
        .some(button => button.textContent === "Wiederherstellen" && button.disabled));
      assert.equal(await run.getByRole("button", { name: "Wiederherstellen" }).isDisabled(), true);
      assert.equal(await run.getByRole("button", { name: "Lauf endgültig leeren" }).isDisabled(), false);
    });

  await runCase("Purge- und Refresh-Fehler schließen die veraltete Laufansicht",
    "?quarantineData=1&quarantinePurgeError=1&quarantineListErrorAfterPurge=1", async page => {
      await page.locator("#btn-quarantine").click();
      await page.getByRole("dialog", { name: "Quarantäne" })
        .getByRole("button", { name: "Anzeigen" }).click();
      const run = page.getByRole("dialog", { name: "Quarantäne · run-1" });
      await run.getByRole("button", { name: "Lauf endgültig leeren" }).click();
      await page.locator("#modal-input").fill("LEEREN run-1");
      await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
      await run.waitFor({ state: "hidden" });
      await page.locator("#dock-summary")
        .filter({ hasText: /Quarantäne-Status konnte nicht neu geladen werden/ }).waitFor();
    });

  await runCase("Erfolgreicher Purge schließt die alte Laufansicht vor einem Refresh-Fehler",
    "?quarantineData=1&quarantineListErrorAfterPurge=1", async page => {
      await page.locator("#btn-quarantine").click();
      await page.getByRole("dialog", { name: "Quarantäne" })
        .getByRole("button", { name: "Anzeigen" }).click();
      const run = page.getByRole("dialog", { name: "Quarantäne · run-1" });
      await run.getByRole("button", { name: "Lauf endgültig leeren" }).click();
      await page.locator("#modal-input").fill("LEEREN run-1");
      await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
      await run.waitFor({ state: "hidden" });
      await page.locator("#dock-summary")
        .filter({ hasText: /Mock-Refresh nach Purge fehlgeschlagen/ }).waitFor();
      assert.equal(await page.getByRole("button", { name: "Wiederherstellen" }).count(), 0);
      assert.equal(await page.getByRole("button", { name: "Lauf endgültig leeren" }).count(), 0);
    });

  await runCase("Quarantäne kann einen ausgeführten Restore nach Manifestfehler abschließen",
    "?quarantineData=1&quarantineRecoveryPending=1", async page => {
      await page.locator("#btn-quarantine").click();
      await page.getByRole("dialog", { name: "Quarantäne" })
        .getByRole("button", { name: "Anzeigen" }).click();
      const run = page.getByRole("dialog", { name: "Quarantäne · run-1" });
      await run.waitFor();
      assert.match(await run.innerText(), /Restore-Abschluss muss geprüft werden/);
      const reconcile = run.getByRole("button", { name: "Abschluss prüfen" });
      assert.equal(await reconcile.isDisabled(), false);
      await reconcile.click();
      await page.getByRole("dialog", { name: "Restore-Abschluss prüfen" })
        .getByRole("button", { name: "Fortfahren" }).click();
      await page.getByText(/Restore-Abschluss bestätigt/).waitFor();
      assert.deepEqual(await page.evaluate(() => window.__MOCK_QUARANTINE_ACTIONS__()), [
        { command: "restore_quarantine_entry", runId: "run-1", relativePath: "alt.txt" },
      ]);
    });

  await runCase("Quarantäne akzeptiert keinen unbekannten Restore-Ausgang als Erfolg",
    "?quarantineData=1&quarantineInvalidRestoreResponse=1", async page => {
      await page.locator("#btn-quarantine").click();
      await page.getByRole("dialog", { name: "Quarantäne" })
        .getByRole("button", { name: "Anzeigen" }).click();
      await page.getByRole("dialog", { name: "Quarantäne · run-1" })
        .getByRole("button", { name: "Wiederherstellen" }).click();
      await page.getByRole("dialog", { name: "Aus Quarantäne wiederherstellen" })
        .getByRole("button", { name: "Fortfahren" }).click();
      await page.locator("#dock-summary")
        .filter({ hasText: "Wiederherstellung fehlgeschlagen" }).waitFor();
      assert.match(await page.locator("#dock-summary").innerText(), /Ungültige Restore-Antwort/);
      assert.doesNotMatch(await page.locator("#dock-summary").innerText(), /Wiederhergestellt/);
    });

  await runCase("Quarantäne pausiert für Restore und Purge ihre eigene Elternansicht", "?quarantineData=1", async page => {
    await page.locator("#btn-quarantine").click();
    let dialog = page.getByRole("dialog", { name: "Quarantäne" });
    await dialog.getByRole("button", { name: "Anzeigen" }).click();
    dialog = page.getByRole("dialog", { name: "Quarantäne · run-1" });
    await dialog.waitFor();
    await dialog.getByRole("button", { name: "Wiederherstellen" }).click();
    const restore = page.getByRole("dialog", { name: "Aus Quarantäne wiederherstellen" });
    await restore.waitFor();
    await restore.getByRole("button", { name: "Fortfahren" }).click();
    await page.getByText(/Wiederhergestellt/).waitFor();
    dialog = page.getByRole("dialog", { name: "Quarantäne · run-1" });
    await dialog.waitFor();
    assert.equal(await dialog.getByRole("button", { name: "Wiederherstellen" }).isDisabled(), true);

    await dialog.getByRole("button", { name: "Lauf endgültig leeren" }).click();
    const phrase = page.locator("#modal-input");
    await phrase.waitFor();
    assert.match(await page.locator("#modal-title").innerText(), /Endgültig leeren/);
    await phrase.fill("LEEREN run-1");
    await page.getByRole("dialog").getByRole("button", { name: "OK" }).click();
    dialog = page.getByRole("dialog", { name: "Quarantäne" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /Recovery-Tombstone bleibt sichtbar/);
    assert.deepEqual(await page.evaluate(() => window.__MOCK_QUARANTINE_ACTIONS__()), [
      { command: "restore_quarantine_entry", runId: "run-1", relativePath: "alt.txt" },
      { command: "purge_quarantine_run", runId: "run-1", confirmation: "LEEREN run-1" },
    ]);
    await dialog.getByRole("button", { name: "Schließen" }).click();
  });

  await runCase("Später Quarantäne-Restore bleibt nach geschlossenem Dialog sichtbar", "?quarantineData=1&quarantineMutationDelay=140", async page => {
    await page.locator("#btn-quarantine").click();
    await page.getByRole("dialog", { name: "Quarantäne" })
      .getByRole("button", { name: "Anzeigen" }).click();
    const run = page.getByRole("dialog", { name: "Quarantäne · run-1" });
    await run.getByRole("button", { name: "Wiederherstellen" }).click();
    await page.getByRole("dialog", { name: "Aus Quarantäne wiederherstellen" })
      .getByRole("button", { name: "Fortfahren" }).click();
    await page.keyboard.press("Escape");

    await page.locator("#dock-summary").filter({ hasText: "Wiederhergestellt" }).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_QUARANTINE_ACTIONS__()), [
      { command: "restore_quarantine_entry", runId: "run-1", relativePath: "alt.txt" },
    ]);
  });

  await runCase("Nur die neueste langsame Quarantäne-Navigation darf rendern", "?quarantineRace=1", async page => {
    await page.locator("#btn-quarantine").click();
    const root = page.getByRole("dialog", { name: "Quarantäne" });
    await root.waitFor();
    const buttons = root.getByRole("button", { name: "Anzeigen" });
    await buttons.nth(0).click();
    await buttons.nth(1).click();
    const latest = page.getByRole("dialog", { name: "Quarantäne · run-schnell" });
    await latest.waitFor();
    await page.waitForTimeout(200);
    assert.equal(await page.locator("#modal-title").innerText(), "Quarantäne · run-schnell");
    assert.match(await latest.innerText(), /run-schnell\.txt/);
    assert.doesNotMatch(await latest.innerText(), /run-langsam\.txt/);
    await latest.getByRole("button", { name: "Schließen" }).click();
  });

  await runCase("Quarantäne führt nach Pane-Wechsel keine alte Wirkung aus", "?quarantineData=1", async page => {
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator("#btn-quarantine").click();
    await page.getByRole("dialog", { name: "Quarantäne" })
      .getByRole("button", { name: "Anzeigen" }).click();
    const run = page.getByRole("dialog", { name: "Quarantäne · run-1" });
    await run.getByRole("button", { name: "Wiederherstellen" }).click();
    const confirm = page.getByRole("dialog", { name: "Aus Quarantäne wiederherstellen" });
    await confirm.waitFor();
    await page.evaluate(() => {
      document.querySelector('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    });
    await confirm.getByRole("button", { name: "Fortfahren" }).click();
    await page.getByText(/Ziel.*gewechselt|Quarantäne.*neu öffnen/).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__MOCK_QUARANTINE_ACTIONS__()), []);
  });

  await runCase("Verspätete Quarantäneliste öffnet in einem neuen Scope weder Badge noch Modal", "?quarantineData=1&quarantineOpenDelay=1", async page => {
    await page.locator("#btn-quarantine").click();
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.waitForTimeout(220);

    assert.equal(await page.locator("#modal").getAttribute("hidden"), "",
      "Die Antwort des alten lokalen Scopes darf kein Modal im neuen Remote-Scope öffnen.");
    assert.equal(await page.locator("#quarantine-count").innerText(), "–",
      "SFTP-Listing=false darf weder die alte Antwort noch einen neuen Providerabruf zulassen.");
  });

  await runCase("Verspätete Badge-Antwort ersetzt nicht die sichtbare Laufliste", "?quarantineData=1&quarantineBadgeRace=1", async page => {
    await page.locator("#btn-quarantine").click();
    const root = page.getByRole("dialog", { name: "Quarantäne" });
    await root.waitFor();
    await page.evaluate(() => window.__MOCK_ARM_QUARANTINE_BADGE_RACE__());
    const completed = {
      files_copied: 0, files_deleted: 0, metadata_updated: 0, bytes_copied: 0,
      cancelled: false, dry_run: false, quarantine: null,
      metadata_report: { classes: [] },
    };
    await page.evaluate(result => window.__MOCK_EMIT__("sync-done", result), completed);
    await page.waitForTimeout(10);
    await page.evaluate(result => window.__MOCK_EMIT__("sync-done", result), completed);
    await page.waitForTimeout(190);
    assert.equal(await page.locator("#quarantine-count").innerText(), "1",
      "Eine ältere Badge-Antwort darf den neueren Zähler nicht überschreiben.");
    await root.getByRole("button", { name: "Anzeigen" }).click();
    const run = page.getByRole("dialog", { name: "Quarantäne · run-1" });
    await run.waitFor();
    assert.doesNotMatch(await run.innerText(), /badge-run/);
    await run.getByRole("button", { name: "Schließen" }).click();
  });

  await runCase("Belegte Fensterqueue setzt das Quarantäne-Badge nicht auf Fehler", "?quarantineData=1", async page => {
    const completed = {
      files_copied: 0, files_deleted: 0, metadata_updated: 0, bytes_copied: 0,
      cancelled: false, dry_run: false, quarantine: null,
      metadata_report: { classes: [] },
    };
    await page.evaluate(result => window.__MOCK_EMIT__("sync-done", result), completed);
    await page.locator("#quarantine-count").filter({ hasText: "1" }).waitFor();

    // Ab hier antwortet das Backend mit `busy` — das ist kein Quarantänefehler,
    // sondern die Aussage „gerade läuft eine Datei- oder Sync-Operation".
    await page.evaluate(() => window.__MOCK_ARM_QUARANTINE_BUSY__());
    await page.evaluate(result => window.__MOCK_EMIT__("sync-done", result), completed);
    await page.waitForTimeout(120);
    assert.equal(await page.locator("#quarantine-count").innerText(), "1",
      "Eine belegte Fensterqueue ist kein Quarantänefehler; der Zähler behält seinen Stand.");
  });

  await runCase("Endpunktwechsel neutralisiert das Quarantäne-Badge auch bei Listingfehler", "?quarantineData=1&remoteScanError=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("sync-done", {
      files_copied: 0, files_deleted: 0, metadata_updated: 0, bytes_copied: 0,
      cancelled: false, dry_run: false, quarantine: null,
      metadata_report: { classes: [] },
    }));
    await page.locator("#quarantine-count").filter({ hasText: "1" }).waitFor();
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.locator("#quarantine-count").filter({ hasText: "–" }).waitFor();
    await page.getByText(/Mock-Remote-Scanfehler/).waitFor();
    assert.equal(await page.locator("#quarantine-count").innerText(), "–");
  });

  await runCase("Realmodus-Aktionen bleiben bei 480 Pixel Höhe erreichbar", "", async page => {
    await page.setViewportSize({ width: 760, height: 480 });
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByRole("button", { name: "Realmodus" }).click();
    const dialog = page.getByRole("dialog", { name: "Lokalen Realmodus bestätigen" });
    const lastAction = dialog.getByRole("button", { name: "Einmalig real starten" });
    await lastAction.waitFor();
    const box = await lastAction.boundingBox();
    assert.ok(box && box.y >= 0 && box.y + box.height <= 480,
      "Die letzte Aktion muss ohne Scrollen der gesamten Seite erreichbar bleiben.");
    await dialog.getByRole("button", { name: "Vorbereitung verwerfen" }).click();
  });

  await runCase("Tokenloses oder verspätetes Schließen trifft keinen fremden Dialog", "", async page => {
    const result = await page.evaluate(async () => {
      const modal = await import("/modal.js");
      document.querySelector("#modal-title").textContent = "Spezialdialog";
      document.querySelector("#modal-body").textContent = "Besitzer A";
      document.querySelector("#modal .modal-buttons").replaceChildren();
      const firstToken = modal.showModal(null, null, "test-special");
      const next = modal.choiceModal("Fremder Dialog", "Besitzer B", [
        { label: "Fertig", value: true, cls: "primary" },
      ]);
      void next;
      modal.hideModal(firstToken);
      await new Promise(resolve => setTimeout(resolve, 0));
      return {
        tokenlessResult: modal.hideModal(),
        staleResult: modal.hideModal(firstToken),
        title: document.querySelector("#modal-title").textContent,
        hidden: document.querySelector("#modal").hidden,
      };
    });
    assert.equal(result.tokenlessResult, false);
    assert.equal(result.staleResult, false);
    assert.equal(result.title, "Fremder Dialog");
    assert.equal(result.hidden, false);
    await page.getByRole("dialog", { name: "Fremder Dialog" })
      .getByRole("button", { name: "Fertig" }).click();
  });

  await runCase("Remote-Quarantäne folgt den getrennten Backend-Capabilities", "", async page => {
    const right = page.getByRole("listbox", { name: /Rechte Dateiliste/ });
    await right.click();
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    assert.equal(await page.locator("#btn-quarantine").isDisabled(), true);

    await right.click();
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    assert.equal(await page.locator("#btn-quarantine").isDisabled(), true);

    await right.click();
    await page.locator('#connection-list [data-protocol="smb"][data-id="mock_smb"]').click();
    assert.equal(await page.locator("#btn-quarantine").isDisabled(), false);
    await page.locator("#btn-quarantine").click();
    const dialog = page.getByRole("dialog", { name: "Quarantäne" });
    await dialog.waitFor();
    await dialog.getByRole("button", { name: "Anzeigen" }).click();
    assert.equal(
      await dialog.getByRole("button", { name: "Wiederherstellen" }).isDisabled(),
      true,
    );
    assert.equal(
      await dialog.getByRole("button", { name: "Lauf endgültig leeren" }).isDisabled(),
      true,
    );

    const calls = await page.evaluate(() => window.__MOCK_QUARANTINE_CALLS__());
    assert.deepEqual(calls.map(call => call.command), [
      "list_provider_quarantine_runs",
      "list_provider_quarantine_runs",
      "list_provider_quarantine_entries",
    ]);
    assert.deepEqual(calls.map(call => call.endpoint), [
      { provider: "smb", profileId: "mock_smb", profileRevision: 41, root: "/" },
      { provider: "smb", profileId: "mock_smb", profileRevision: 41, root: "/" },
      { provider: "smb", profileId: "mock_smb", profileRevision: 41, root: "/" },
    ]);
  });

  await runCase("Seitenleiste ordnet Geräte, Freigaben und vollständige Favoriten", "", async page => {
    const layout = await page.evaluate(() => {
      const scroll = document.querySelector(".side-scroll");
      const sections = [...scroll.querySelectorAll(":scope > [data-sidebar-section]")];
      const favorites = document.querySelector("#fav-list");
      const connections = document.querySelector("#connection-list");
      const favoriteSection = document.querySelector('[data-sidebar-section="favorites"]');
      const header = document.querySelector(".side-brand");
      const queue = document.querySelector("#btn-queue");
      const quarantine = document.querySelector("#btn-quarantine");
      return {
        order: sections.map(section => section.dataset.sidebarSection),
        favoritesContainBoth: favoriteSection.contains(favorites) && favoriteSection.contains(connections),
        scrollHost: getComputedStyle(scroll).overflowY,
        favoritesFlow: getComputedStyle(favorites).overflowY,
        connectionsFlow: getComputedStyle(connections).overflowY,
        queueInHeader: header.contains(queue),
        headerBeforeCategories: header.compareDocumentPosition(scroll)
          === Node.DOCUMENT_POSITION_FOLLOWING,
        quarantineAfterCategories: scroll.compareDocumentPosition(quarantine)
          === Node.DOCUMENT_POSITION_FOLLOWING,
      };
    });
    assert.deepEqual(layout.order, ["devices", "shares", "favorites"]);
    assert.equal(layout.favoritesContainBoth, true, "Ordner und gespeicherte Verbindungen gehören zu Favoriten.");
    assert.equal(layout.scrollHost, "auto", "Nur der gemeinsame Bereich scrollt.");
    assert.equal(layout.favoritesFlow, "visible", "Die Favoritenliste scrollt nicht mehr eigenständig.");
    assert.equal(layout.connectionsFlow, "visible", "Die Verbindungsliste scrollt nicht mehr eigenständig.");
    assert.equal(layout.queueInHeader, true, "Queue muss im dauerhaft sichtbaren Fensterkopf liegen.");
    assert.equal(layout.headerBeforeCategories, true, "Der Fensterkopf muss vor den Hauptkategorien liegen.");
    assert.equal(layout.quarantineAfterCategories, true, "Quarantäne muss nach den Hauptkategorien folgen.");
  });

  await runCase("Seitenleisten-Kategorien sind ziehbar, tastaturbedienbar und persistent", "", async page => {
    const readOrder = () => page.evaluate(() =>
      [...document.querySelectorAll("#sidebar-sections > [data-sidebar-section]")]
        .map(section => section.dataset.sidebarSection));
    assert.deepEqual(await readOrder(), ["devices", "shares", "favorites"]);

    const dragState = await page.evaluate(() => {
      const source = document.querySelector('[data-sidebar-section="favorites"] .side-section-handle');
      const target = document.querySelector('[data-sidebar-section="devices"]');
      const dataTransfer = new DataTransfer();
      source.dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer }));
      const rect = target.getBoundingClientRect();
      target.dispatchEvent(new DragEvent("dragover", {
        bubbles: true, cancelable: true, dataTransfer, clientY: rect.top + 1,
      }));
      const marker = target.classList.contains("drop-before");
      target.dispatchEvent(new DragEvent("drop", {
        bubbles: true, cancelable: true, dataTransfer, clientY: rect.top + 1,
      }));
      source.dispatchEvent(new DragEvent("dragend", { bubbles: true, dataTransfer }));
      return {
        marker,
        stored: JSON.parse(localStorage.getItem("doppeldecker-sidebar-section-order")),
      };
    });
    assert.equal(dragState.marker, true, "Beim Ziehen fehlt die Einfügelinie.");
    assert.deepEqual(await readOrder(), ["favorites", "devices", "shares"]);
    assert.deepEqual(dragState.stored, ["favorites", "devices", "shares"]);

    await page.reload({ waitUntil: "networkidle" });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).waitFor();
    assert.deepEqual(await readOrder(), ["favorites", "devices", "shares"]);

    await page.getByRole("button", { name: "Freigaben umsortieren" }).focus();
    await page.keyboard.press("ArrowUp");
    assert.deepEqual(await readOrder(), ["favorites", "shares", "devices"]);
    assert.deepEqual(
      await page.evaluate(() => JSON.parse(localStorage.getItem("doppeldecker-sidebar-section-order"))),
      ["favorites", "shares", "devices"],
    );

    await page.evaluate(() => localStorage.setItem(
      "doppeldecker-sidebar-section-order",
      JSON.stringify(["favorites", "unbekannt", "devices"]),
    ));
    await page.reload({ waitUntil: "networkidle" });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).waitFor();
    assert.deepEqual(await readOrder(), ["devices", "shares", "favorites"]);
  });

  await runCase("mDNS-Funde füllen nur den bestehenden Verbindungseditor vor", "", async page => {
    const services = page.locator("#network-service-list .network-service");
    await services.first().waitFor();
    assert.equal(await services.count(), 3);
    assert.equal((await page.evaluate(() => window.__MOCK_DISCOVERY__())).calls, 1);

    const sftpService = page.getByRole("button", { name: /Buildserver · SFTP .* im Verbindungseditor öffnen/ });
    await sftpService.focus();
    assert.equal(await sftpService.evaluate(element => element === document.activeElement), true);
    await sftpService.press("Enter");
    let dialog = page.getByRole("dialog", { name: "SFTP-Verbindung hinzufügen" });
    await dialog.waitFor();
    assert.equal(await dialog.locator("#sftp-name").inputValue(), "Buildserver");
    assert.equal(await dialog.locator("#sftp-host").inputValue(), "build.local");
    assert.equal(await dialog.locator("#sftp-port").inputValue(), "2222");
    assert.equal(await dialog.locator("#sftp-user").inputValue(), "");
    assert.equal(await dialog.getByRole("button", { name: "Hostkey ermitteln" }).count(), 1);
    await dialog.getByRole("button", { name: "Abbrechen" }).click();

    const smbService = page.getByRole("button", { name: /Studio-NAS · SMB .* im Verbindungseditor öffnen/ });
    await smbService.focus();
    await smbService.press("Space");
    dialog = page.getByRole("dialog", { name: "Verschlüsselte SMB3-Verbindung hinzufügen" });
    await dialog.waitFor();
    assert.equal(await dialog.locator("#smb-name").inputValue(), "Studio-NAS");
    assert.equal(await dialog.locator("#smb-host").inputValue(), "studio-nas.local");
    assert.equal(await dialog.locator("#smb-port").inputValue(), "445");
    assert.equal(await dialog.locator("#smb-share").inputValue(), "");
    assert.equal(await dialog.getByRole("button", { name: "Server-GUID ermitteln" }).count(), 1);
    await dialog.getByRole("button", { name: "Abbrechen" }).click();

    await services.filter({ hasText: "Cloud" }).click();
    dialog = page.getByRole("dialog", { name: "HTTPS-WebDAV-Verbindung hinzufügen" });
    await dialog.waitFor();
    assert.equal(await dialog.locator("#webdav-name").inputValue(), "Cloud");
    assert.equal(await dialog.locator("#webdav-origin").inputValue(), "https://cloud.local:8443");
    assert.equal(await dialog.locator("#webdav-user").inputValue(), "");
    assert.equal(await dialog.getByRole("button", { name: "Zertifikat ermitteln" }).count(), 1);
    await dialog.getByRole("button", { name: "Abbrechen" }).click();

    const toggle = page.locator("#network-discovery-enabled");
    await toggle.uncheck();
    assert.match(await page.locator("#network-service-list").innerText(), /ausgeschaltet/);
    assert.equal(
      await page.evaluate(() => localStorage.getItem("doppeldecker-network-discovery-enabled")),
      "false",
    );
    await page.reload({ waitUntil: "networkidle" });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).waitFor();
    assert.match(await page.locator("#network-service-list").innerText(), /ausgeschaltet/);
    assert.equal((await page.evaluate(() => window.__MOCK_DISCOVERY__())).calls, 0);

    await page.locator("#network-discovery-enabled").check();
    await page.locator("#network-service-list .network-service").first().waitFor();
    assert.equal((await page.evaluate(() => window.__MOCK_DISCOVERY__())).calls, 1);
  });

  for (const staleError of [false, true]) {
    await runCase(`Seitenleisten-Review alte Geräteantwort erhält Auswurf: ${staleError ? "Lesefehler" : "Altbestand"}`, "", async page => {
      await page.getByRole("button", { name: "Backup auswerfen", exact: true }).waitFor();
      await page.evaluate(staleError => {
        const original = window.__TAURI__.core.invoke;
        let held = false;
        window.__TAURI__.core.invoke = async (command, args) => {
          const result = await original(command, args);
          if (command === "list_system_mounts" && !held) {
            held = true;
            await new Promise(resolve => { window.__SIDEBAR_MOUNTS_RELEASE__ = resolve; });
            if (staleError) throw { code: "io", message: "Veralteter Gerätefehler" };
          }
          return result;
        };
        window.dispatchEvent(new Event("focus"));
      }, staleError);
      await page.waitForFunction(() => typeof window.__SIDEBAR_MOUNTS_RELEASE__ === "function");
      await page.getByRole("button", { name: "Backup auswerfen", exact: true }).click();
      await page.locator("#dock-summary").filter({ hasText: /Ausgeworfen/ }).waitFor();
      assert.equal(await page.getByRole("button", { name: "Backup auswerfen", exact: true }).count(), 0);
      await page.evaluate(async () => { window.__SIDEBAR_MOUNTS_RELEASE__(); await new Promise(resolve => setTimeout(resolve, 0)); });
      assert.equal(await page.getByRole("button", { name: "Backup auswerfen", exact: true }).count(), 0,
        "Eine alte Geräteantwort darf das nachweislich ausgeworfene Volume nicht zurückbringen");
      assert.equal(await page.evaluate(() => window.__MOCK_MOUNTS__().mounts.some(mount => mount.name === "Backup")), false);
      assert.equal(await page.locator("#device-list .device-item").count(), 1);
      assert.equal(await page.locator("#share-list .device-item").count(), 1);
    });
  }

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

  await runCase("Backup-Hinweis lebt in der Sync-Ansicht und folgt der Profilwahl", "", async page => {
    // Die frühere Dauerwarnung ist aus der Seitenleiste verschwunden.
    assert.equal(await page.locator(".backup-boundary").count(), 0);
    assert.doesNotMatch(await page.locator(".sidebar").innerText(), /Vollbackup/);
    // In der Sync-Ansicht steht der Klartext: Profilzeile + Backup-Satz.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    const note = page.locator("#sync-metadata-note");
    await note.waitFor();
    assert.match(await note.innerText(), /Überträgt Inhalt, Änderungszeit und normale Rechte/);
    assert.match(await note.innerText(), /Ein Sync ersetzt kein Backup/);
    // Profilwechsel aktualisiert die erste Zeile.
    await page.locator('#sync-metadata-group input[value="NativeBestEffort"]').check();
    assert.match(await note.innerText(), /native Merkmale/);
    await page.keyboard.press("Escape");
  });

  await runCase("Eject zeigt lebende Sitzung und trennt sie samt Pane-Rückkehr", "", async page => {
    const profile = page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]');
    // Vor dem Verbinden: keine Sitzung, kein Eject-Knopf.
    assert.equal(await page.locator(".conn-eject").count(), 0);
    await profile.click();
    // Nach dem Remote-Listing lebt die Sitzung → Eject erscheint am Profil.
    await profile.locator(".conn-eject").waitFor();
    // Pane wechselt zurück zu lokal — die Sitzung lebt weiter, Eject bleibt.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "home"));
    await profile.locator(".conn-eject").waitFor();
    // Erneut verbinden, dann auswerfen: Knopf verschwindet, Pane kehrt lokal zurück.
    await profile.click();
    await page.locator('#connection-list .connection-item.active .conn-eject').waitFor();
    await profile.locator(".conn-eject").click();
    await page.locator(".conn-eject").waitFor({ state: "detached" });
    const paneTitle = await page.getByRole("textbox").first().getAttribute("title");
    assert.doesNotMatch(paneTitle || "", /SFTP/);
  });

  await runCase("Verspäteter Eject-Default überschreibt keine neu geöffnete Verbindung", "?defaultDelay=120", async page => {
    const profile = page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]');
    await profile.click();
    await profile.locator(".conn-eject").waitFor();
    await profile.locator(".conn-eject").click();
    // Nach dem Trennen darf die Pane sofort leer und wirkungslos werden. Während
    // der lokale Standardpfad noch lädt, wird dasselbe Profil bewusst neu geöffnet.
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).locator(".empty").waitFor();
    await profile.click();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /remote/ }).waitFor();
    await page.waitForTimeout(160);
    assert.match(await page.getByRole("textbox").first().getAttribute("title") || "", /SFTP · Mock-SFTP/);
  });

  await runCase("CLI-importiertes Profil erscheint beim nächsten Fensterfokus", "", async page => {
    const imported = page.locator('#connection-list [data-protocol="ftps"][data-id="cli_ftps"]');
    assert.equal(await imported.count(), 0);
    await page.evaluate(() => {
      window.__MOCK_IMPORT_FTPS_PROFILE__();
      window.dispatchEvent(new Event("focus"));
    });
    await imported.waitFor();
    assert.match(await imported.innerText(), /CLI-FTPS.*Identität ungeprüft/s);
    await imported.click();
    assert.match(await page.locator("#dock").innerText(), /FTPS · Identität ungeprüft/);
  });

  await runCase("Klartext-FTP bleibt in Sidebar, Pane und Statuszeile sichtbar", "", async page => {
    const imported = page.locator('#connection-list [data-protocol="ftps"][data-id="cli_ftp"]');
    await page.evaluate(() => {
      window.__MOCK_IMPORT_FTP_PROFILE__();
      window.dispatchEvent(new Event("focus"));
    });
    await imported.waitFor();
    assert.match(await imported.innerText(), /FTP · unverschlüsselt/);
    await imported.click();
    assert.match(await page.locator("#dock").innerText(), /FTP unverschlüsselt.*Zugangsdaten/s);
    assert.match(await page.getByRole("textbox").first().getAttribute("title"), /^FTP · CLI-FTP/);

    // Der Transporthinweis gehört einmal je Profil und Stufe — wie bei SFTP. Ohne
    // den Merker feuerte er bei jedem Listing und ersetzte über showDock den
    // kompletten Dock-Inhalt: Die Meldung einer gerade abgeschlossenen oder
    // gescheiterten Dateiaktion war sofort wieder weg.
    await page.evaluate(() => {
      document.querySelector("#dock-summary").textContent = "MARKER-DATEIAKTION";
    });
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "refresh"));
    await page.waitForTimeout(200);
    assert.match(
      await page.locator("#dock").innerText(),
      /MARKER-DATEIAKTION/,
      "Ein wiederholtes Listing derselben FTP-Pane darf die Meldung einer Dateiaktion nicht ersetzen.",
    );
  });

  await runCase("App richtet den optionalen Terminalbefehl ausdrücklich ein", "?cliOffer=1", async page => {
    await page.getByRole("dialog", { name: "Terminalbefehl einrichten?" }).waitFor();
    await page.getByRole("button", { name: "Einrichten" }).click();
    await page.waitForFunction(() => document.documentElement.dataset.cliInstalled === "true");
    assert.match(await page.locator("#dock").innerText(), /CLI bereit.*doppeldecker-cli/s);
  });

  await runCase("Verbindungsprofil navigieren und geheimnisfrei entfernen", "", async page => {
    const profile = page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]');
    await profile.click();
    await profile.evaluate(element => element.classList.contains("active") || Promise.reject(new Error("SFTP-Profil wurde nicht aktiv markiert.")));
    assert.match(await page.getByRole("textbox").first().getAttribute("title"), /SFTP · Mock-SFTP/);
    await profile.locator('[data-remove="mock_sftp"]').click();
    const dialog = page.getByRole("dialog", { name: "SFTP-Profil entfernen" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /Secret bleibt absichtlich im System-Schlüsselbund/);
    await dialog.getByRole("button", { name: "Fortfahren" }).click();
    // Nur das SFTP-Profil verschwindet; die übrigen Protokolle bleiben gelistet.
    await page.locator('#connection-list [data-protocol="sftp"]').waitFor({ state: "detached" });
    assert.equal(await page.locator('#connection-list [data-protocol="smb"][data-id="mock_smb"]').count(), 1);
  });

  await runCase("Ein Verbindungs-Plus wählt das Protokoll vor dem Editor", "", async page => {
    const editors = [
      ["sftp", "SFTP-Verbindung hinzufügen", /keine Anmeldung/],
      ["ftps", "FTP-/FTPS-Verbindung hinzufügen", /weder Anmeldung noch Dateizugriff/],
      ["webdav", "HTTPS-WebDAV-Verbindung hinzufügen", /weder Anmeldung noch HTTP-Dateizugriff/],
      ["smb", "Verschlüsselte SMB3-Verbindung hinzufügen", /weder Anmeldung noch Freigabe- oder Dateizugriff/],
    ];
    for (const [protocol, title, firstStep] of editors) {
      await page.locator("#conn-add").click();
      const picker = page.getByRole("dialog", { name: "Verbindung hinzufügen" });
      await picker.waitFor();
      assert.match(await picker.innerText(), /ausschließlich im System-Schlüsselbund/);
      await picker.locator(`[data-pick="${protocol}"]`).click();
      const dialog = page.getByRole("dialog", { name: title });
      await dialog.waitFor();
      assert.equal(await dialog.locator('input[type="password"]').count(), 0,
        "Der Webview darf kein Secret-Eingabefeld enthalten.");
      assert.equal(await dialog.getByRole("button", { name: "Sicher eingeben …" }).count(), 1);
      assert.match(await dialog.innerText(), /nativen.*Eingabe|nativ.*außerhalb der Weboberfläche/s);
      assert.match(await dialog.innerText(), firstStep);
      if (protocol === "ftps") {
        await dialog.locator("#ftps-transport").selectOption("plaintext");
        assert.match(
          await dialog.innerText(),
          /Unverschlüsseltes FTP:.*Dateiinhalte.*ohne weitere Bestätigung/s,
        );
        assert.equal(
          await dialog.getByRole("button", { name: "FTP-Profil speichern" }).count(),
          1,
        );
      }
      await dialog.getByRole("button", { name: "Abbrechen" }).click();
    }
  });

  await runCase("Verbindungs-Rechtsklick bietet Öffnen, Bearbeiten und Entfernen", "", async page => {
    const id = "mock_sftp";
    const row = page.locator(`#connection-list [data-protocol="sftp"][data-id="${id}"]`);
    await row.click({ button: "right" });
    let menu = page.getByRole("menu", { name: "Verbindung" });
    await menu.waitFor();
    assert.deepEqual(await menu.getByRole("menuitem").allTextContents(), [
      "In aktiver Pane öffnen",
      "Auf der anderen Seite öffnen",
      "Bearbeiten",
      "Profil entfernen …",
    ]);

    await menu.getByRole("menuitem", { name: "Bearbeiten" }).click();
    const editor = page.getByRole("dialog", { name: "SFTP-Verbindung bearbeiten" });
    await editor.waitFor();
    assert.equal(await editor.locator("#sftp-name").inputValue(), "Mock-SFTP");
    await editor.getByRole("button", { name: "Abbrechen" }).click();

    // Die zweite Aktion prüft „Auf der anderen Seite öffnen":
    // Bei aktiver linker Pane muss ausschließlich rechts die Verbindung erscheinen.
    await row.click({ button: "right" });
    menu = page.getByRole("menu", { name: "Verbindung" });
    await menu.getByRole("menuitem", { name: "Auf der anderen Seite öffnen" }).click();
    assert.doesNotMatch(await page.getByRole("textbox").nth(0).getAttribute("title") || "", /SFTP/);
    assert.match(await page.getByRole("textbox").nth(1).getAttribute("title") || "", /SFTP · Mock-SFTP/);

    // Nach dem Remote-Listing ist die Sitzung wirklich offen; erst dann erscheint
    // der zusätzliche Trennen-Befehl im selben Menü.
    await row.locator(".conn-eject").waitFor();
    await row.click({ button: "right" });
    menu = page.getByRole("menu", { name: "Verbindung" });
    assert.deepEqual(await menu.getByRole("menuitem").allTextContents(), [
      "In aktiver Pane öffnen",
      "Auf der anderen Seite öffnen",
      "Bearbeiten",
      "Verbindung trennen",
      "Profil entfernen …",
    ]);
    await page.keyboard.press("Escape");
  });

  await runCase("Bestehende Verbindungen lassen sich mit gleicher ID umbenennen", "", async page => {
    const cases = [
      ["sftp", "mock_sftp", "SFTP-Verbindung bearbeiten", "#sftp-name", "SFTP umbenannt", "sftpProfiles"],
      ["ftps", "mock_ftps", "FTP-/FTPS-Verbindung bearbeiten", "#ftps-name", "FTPS umbenannt", "ftpsProfiles"],
      ["webdav", "mock_webdav", "HTTPS-WebDAV-Verbindung bearbeiten", "#webdav-name", "WebDAV umbenannt", "webdavProfiles"],
      ["smb", "mock_smb", "Verschlüsselte SMB3-Verbindung bearbeiten", "#smb-name", "SMB umbenannt", "smbProfiles"],
    ];
    for (const [protocol, id, title, nameInput, newName, stateKey] of cases) {
      const row = page.locator(`#connection-list [data-protocol="${protocol}"][data-id="${id}"]`);
      if (protocol === "sftp") await row.click();
      await row.locator(`[data-edit="${id}"]`).click();
      const dialog = page.getByRole("dialog", { name: title });
      await dialog.waitFor();
      assert.match(await dialog.innerText(), /vorhandene Passwort unverändert/);
      assert.equal(await dialog.locator('input[type="password"]').count(), 0);
      await dialog.locator(nameInput).fill(newName);
      await dialog.getByRole("button", { name: "Änderungen speichern" }).click();
      await dialog.waitFor({ state: "detached" });
      await page.locator(
        `#connection-list [data-protocol="${protocol}"][data-id="${id}"]`,
        { hasText: newName },
      ).waitFor();
      const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
      const stored = state[stateKey].filter(profile => profile.id === id);
      assert.equal(stored.length, 1, "Bearbeiten darf weder ID noch Profilanzahl ändern.");
      assert.equal(stored[0].name, newName);
      if (protocol === "sftp") {
        assert.match(
          await page.getByRole("textbox").first().getAttribute("title"),
          /SFTP · SFTP umbenannt/,
          "Ein reines Umbenennen muss den Titel einer offenen Pane nachziehen.",
        );
      }
    }
    const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
    assert.deepEqual(
      state.sftpProfiles[0].connection.alternate_hosts,
      ["2001:db8::8"],
      "Ein bloßes Umbenennen muss importierte Alternativrouten erhalten.",
    );
    assert.deepEqual(state.secretWrites, [], "Leere Passwortfelder dürfen kein Secret überschreiben.");
  });

  await runCase("Geänderter SFTP-Endpunkt verlangt einen neuen Hostkey", "", async page => {
    const id = "mock_sftp";
    const row = page.locator(`#connection-list [data-protocol="sftp"][data-id="${id}"]`);
    await row.click();
    await row.locator(`[data-edit="${id}"]`).click();
    const dialog = page.getByRole("dialog", { name: "SFTP-Verbindung bearbeiten" });
    await dialog.locator("#sftp-host").fill("sftp-neu.mock.test");
    const primary = dialog.locator(".modal-buttons .primary");
    assert.equal(await primary.innerText(), "Hostkey ermitteln");
    await primary.click();
    assert.equal(await primary.innerText(), "Geprüften Hostkey binden und speichern");
    await primary.click();
    await dialog.waitFor({ state: "detached" });
    const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
    const stored = state.sftpProfiles.filter(profile => profile.id === id);
    assert.equal(stored.length, 1);
    assert.equal(stored[0].connection.host, "sftp-neu.mock.test");
    assert.deepEqual(stored[0].connection.host_key.sha256, Array(32).fill(11));
    assert.deepEqual(
      stored[0].connection.alternate_hosts,
      [],
      "Alternativrouten des alten Endpunkts dürfen nicht auf einen neuen Host übergehen.",
    );
    assert.deepEqual(state.secretWrites, [], "Der vorhandene Schlüsselbundverweis muss genügen.");
    assert.doesNotMatch(
      await page.getByRole("textbox").first().getAttribute("title") || "",
      /SFTP/,
      "Nach einer Endpunktänderung darf keine alte Remote-Dateiliste sichtbar bleiben.",
    );
  });

  await runCase("Neues Passwort bekommt einen frischen Schlüsselbundverweis", "", async page => {
    const id = "mock_ftps";
    const row = page.locator(`#connection-list [data-protocol="ftps"][data-id="${id}"]`);
    await row.click();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /remote/ }).waitFor();
    await row.locator(`[data-edit="${id}"]`).click();
    const dialog = page.getByRole("dialog", { name: "FTP-/FTPS-Verbindung bearbeiten" });
    await captureNativeSecret(dialog);
    await dialog.getByRole("button", { name: "Änderungen speichern" }).click();
    await dialog.waitFor({ state: "detached" });
    const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
    assert.equal(state.secretWrites.length, 1);
    assert.equal(state.secretWrites[0].command, "save_ftps_profile");
    assert.notEqual(state.secretWrites[0].reference.account, id,
      "Ein direkter Secretwechsel darf den vorhandenen Schlüsselbundwert nicht überschreiben.");
    const stored = state.ftpsProfiles.filter(profile => profile.id === id);
    assert.equal(stored.length, 1);
    assert.deepEqual(stored[0].connection.password_reference, state.secretWrites[0].reference);
    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /Beispiel\.txt/ }).waitFor();
    assert.equal(await page.locator('.pane[data-side="left"] .path').inputValue(), "/mock/quelle",
      "Der frische Secretverweis muss eine offene Pane fail-closed lokal zurücksetzen.");
  });

  await runCase("Unvollständiges Verbindungsprofil meldet die fehlenden Felder", "", async page => {
    // Regression: Die Vollständigkeitsprüfung meldet über `showDock`. Läge sie
    // außerhalb des Editor-Gültigkeitsbereichs, wäre `showDock` dort undefiniert
    // und der Nutzer bekäme statt der Meldung einen stillen Skriptfehler.
    const id = "mock_ftps";
    const row = page.locator(`#connection-list [data-protocol="ftps"][data-id="${id}"]`);
    await row.click();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /remote/ }).waitFor();
    await row.locator(`[data-edit="${id}"]`).click();
    const dialog = page.getByRole("dialog", { name: "FTP-/FTPS-Verbindung bearbeiten" });
    await dialog.locator("#ftps-user").fill("");
    await dialog.getByRole("button", { name: "Änderungen speichern" }).click();
    await page.getByText(/Name, Nutzer und Server-Root werden benötigt/).waitFor();
    await dialog.waitFor({ state: "visible" });
    const actions = await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__());
    assert.equal(actions.filter(action => action.command === "save_ftps_profile").length, 0,
      "Ein unvollständiges Profil darf nicht gespeichert werden.");
  });

  await runCase("Erfolgreicher Profilsave bleibt trotz fehlerhaftem Pane-Rückfall erfolgreich", "?defaultErrorAfterStartup=1", async page => {
    const id = "mock_ftps";
    const row = page.locator(`#connection-list [data-protocol="ftps"][data-id="${id}"]`);
    await row.click();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ })
      .getByRole("option", { name: /remote/ }).waitFor();
    await row.locator(`[data-edit="${id}"]`).click();
    const dialog = page.getByRole("dialog", { name: "FTP-/FTPS-Verbindung bearbeiten" });
    await captureNativeSecret(dialog);
    await dialog.getByRole("button", { name: "Änderungen speichern" }).click();
    await dialog.waitFor({ state: "detached" });
    await page.getByText(/Mock-Standardordner nicht erreichbar/).waitFor();
    const actions = await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__());
    assert.equal(actions.filter(action => action.command === "save_ftps_profile").length, 1);
    assert.equal(actions.filter(action => action.command === "discard_connection_secret").length, 0,
      "Die bereits verbrauchte Secret-ID darf wegen einer bloßen Pane-Nachwirkung nicht verworfen werden.");
    assert.equal((await page.evaluate(() => window.__MOCK_CONNECTION_STATE__())).pendingSecretCount, 0);
  });

  await runCase("Später Profilfehler bleibt trotz Defaultfehler sichtbar und leert die Pane", "?profileSaveVisibleError=ftps&defaultErrorAfterStartup=1", async page => {
    const id = "mock_ftps";
    const row = page.locator(`#connection-list [data-protocol="ftps"][data-id="${id}"]`);
    await row.click();
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote/ }).waitFor();
    await row.locator(`[data-edit="${id}"]`).click();
    const dialog = page.getByRole("dialog", { name: "FTP-/FTPS-Verbindung bearbeiten" });
    await captureNativeSecret(dialog);
    await dialog.getByRole("button", { name: "Änderungen speichern" }).click();
    await page.getByText(/Profil ist trotz spätem Speicherfehler sichtbar/).waitFor();
    assert.equal(await list.locator('[role="option"]').count(), 0,
      "Die alte Remote-Liste muss schon vor dem fehlgeschlagenen Default-Abruf unbedienbar sein.");
    assert.equal(await page.locator('.pane[data-side="left"] .path').inputValue(), "");
  });

  await runCase("Sidebar-Icons unterscheiden Ordnerart und Protokoll", "", async page => {
    // Jedes Verbindungsprotokoll trägt seine eigene Icon-Klasse in der einen Liste.
    for (const protocol of ["sftp", "ftps", "webdav", "smb"]) {
      assert.equal(
        await page.locator(`#connection-list [data-protocol="${protocol}"] .side-icon.${protocol}`).count(),
        1,
        `Protokoll-Icon für ${protocol} fehlt.`,
      );
    }
    // Bekannte Systemordner erhalten ein eigenes Symbol, alles andere den Ordner.
    const kinds = await page.evaluate(async () => {
      const { favoriteIconKind } = await import("./favorites-ui.js");
      return [
        favoriteIconKind("/Users/demo"),
        favoriteIconKind("/home/demo/Downloads"),
        favoriteIconKind("/Users/demo/Desktop"),
        favoriteIconKind("/Users/demo/Documents"),
        favoriteIconKind("/Users/demo/Projekte"),
      ];
    });
    assert.deepEqual(kinds, ["home", "downloads", "desktop", "documents", "folder"]);
  });

  await runCase("Ordner per Drag & Drop zu Favoriten, dann umsortieren", "?keyboard=1", async page => {
    const dragFolderToFavorites = () => page.evaluate(() => {
      const rows = [...document.querySelectorAll('.pane[data-side="left"] .row')];
      const row = rows.find(candidate => candidate.textContent.includes("Bilder"));
      if (!row || row.getAttribute("draggable") !== "true") {
        throw new Error("Ordnerzeile ist nicht draggable.");
      }
      const dataTransfer = new DataTransfer();
      row.dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer }));
      const list = document.querySelector("#fav-list");
      const over = new DragEvent("dragover", { bubbles: true, cancelable: true, dataTransfer });
      list.dispatchEvent(over);
      if (!list.classList.contains("drop-target")) {
        throw new Error("Favoritenliste zeigt kein Drop-Ziel an.");
      }
      list.dispatchEvent(new DragEvent("drop", { bubbles: true, cancelable: true, dataTransfer }));
      row.dispatchEvent(new DragEvent("dragend", { bubbles: true, dataTransfer }));
    });
    await dragFolderToFavorites();
    await page.locator('#fav-list .fav-item', { hasText: "Bilder" }).waitFor();
    // Zweiten Favoriten über „+" anlegen, dann per Drag an die erste Position holen.
    await page.locator("#fav-add").click();
    await page.locator("#fav-list .fav-item", { hasText: "quelle" }).waitFor();
    await page.evaluate(() => {
      const items = [...document.querySelectorAll("#fav-list .fav-item")];
      const source = items.find(candidate => candidate.textContent.includes("quelle"));
      const target = items.find(candidate => candidate.textContent.includes("Bilder"));
      const dataTransfer = new DataTransfer();
      source.dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer }));
      const rect = target.getBoundingClientRect();
      const drop = new DragEvent("drop", {
        bubbles: true,
        cancelable: true,
        dataTransfer,
        clientY: rect.top + 2,
      });
      target.dispatchEvent(drop);
      source.dispatchEvent(new DragEvent("dragend", { bubbles: true, dataTransfer }));
    });
    await page.waitForFunction(() => {
      const names = [...document.querySelectorAll("#fav-list .fav-name")].map(el => el.textContent);
      return names.length === 2 && names[0] === "quelle" && names[1] === "Bilder";
    });
  });

  await runCase("Favoriten: Einfügelinie, Rechtsklick-Entfernen und Herausziehen", "?keyboard=1", async page => {
    // Zwei Favoriten anlegen: einen Pane-Ordner hineinziehen, einen über „+".
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.pane[data-side="left"] .row')];
      const row = rows.find(candidate => candidate.textContent.includes("Bilder"));
      const dataTransfer = new DataTransfer();
      row.dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer }));
      const list = document.querySelector("#fav-list");
      list.dispatchEvent(new DragEvent("dragover", { bubbles: true, cancelable: true, dataTransfer }));
      list.dispatchEvent(new DragEvent("drop", { bubbles: true, cancelable: true, dataTransfer }));
      row.dispatchEvent(new DragEvent("dragend", { bubbles: true, dataTransfer }));
    });
    await page.locator("#fav-list .fav-item", { hasText: "Bilder" }).waitFor();
    await page.locator("#fav-add").click();
    await page.locator("#fav-list .fav-item", { hasText: "quelle" }).waitFor();

    // (1) Umsortieren zeigt die dünne Einfügelinie an der getroffenen Kante und
    //     räumt sie nach dem Zug wieder ab.
    const marker = await page.evaluate(() => {
      const items = [...document.querySelectorAll("#fav-list .fav-item")];
      const dataTransfer = new DataTransfer();
      items[0].dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer }));
      const rect = items[1].getBoundingClientRect();
      items[1].dispatchEvent(new DragEvent("dragover", {
        bubbles: true, cancelable: true, dataTransfer, clientY: rect.top + 2,
      }));
      const marked = document.querySelector("#fav-list .drop-before, #fav-list .drop-after");
      const cls = marked ? [...marked.classList].find(name => name.startsWith("drop-")) : null;
      // Ohne echten Drop wieder loslassen (0/0 = Abbruch, entfernt nichts).
      items[0].dispatchEvent(new DragEvent("dragend", { bubbles: true, dataTransfer }));
      return cls;
    });
    assert.equal(marker, "drop-before", "Einfügelinie fehlt beim Umsortieren.");
    assert.equal(
      await page.locator("#fav-list .drop-before, #fav-list .drop-after").count(), 0,
      "Einfügelinie bleibt nach dem Zug stehen.",
    );

    // (2) Rechtsklick öffnet ein Menü mit genau dem Entfernen-Befehl.
    await page.locator("#fav-list .fav-item", { hasText: "Bilder" }).click({ button: "right" });
    const menu = page.locator("#context-menu");
    await menu.waitFor();
    assert.deepEqual(await menu.getByRole("menuitem").allTextContents(), ["Aus Favoriten entfernen"]);
    await menu.getByRole("menuitem").click();
    await page.locator("#fav-list .fav-item", { hasText: "Bilder" }).waitFor({ state: "detached" });
    assert.equal(await page.locator("#fav-list .fav-item").count(), 1);

    // (3) Herausziehen entfernt: dragend klar außerhalb der Liste (nicht 0/0) löscht.
    await page.evaluate(() => {
      const item = document.querySelector("#fav-list .fav-item");
      const dataTransfer = new DataTransfer();
      item.dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer }));
      const rect = document.querySelector("#fav-list").getBoundingClientRect();
      item.dispatchEvent(new DragEvent("dragend", {
        bubbles: true, dataTransfer, clientX: rect.right + 200, clientY: rect.top + 10,
      }));
    });
    await page.locator("#fav-list").filter({ hasText: "Noch keine Favoriten" }).waitFor();
  });

  await runCase("Scan-Race, Scanfehler und Konfliktdialog", "?scanDelay=250&scanError=broken&conflicts=replace", async page => {
    const path = page.getByRole("textbox").first();
    await path.fill("/mock/slow");
    await path.press("Enter");
    await path.fill("/mock/fast");
    await path.press("Enter");
    await page.getByRole("option", { name: /Schnell\.txt/ }).waitFor();
    await page.waitForTimeout(350);
    assert.equal(await path.inputValue(), "/mock/fast");
    assert.equal(await page.getByRole("option", { name: /Langsam\.txt/ }).count(), 0);
    await path.fill("/mock/broken");
    await path.press("Enter");
    await page.getByText(/Mock-Scanfehler/).waitFor();
    assert.equal(await path.inputValue(), "/mock/fast");
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.focus();
    await page.keyboard.press("F5");
    const dialog = page.getByRole("dialog", { name: "Konflikte beim Kopieren" });
    await dialog.waitFor();
    await dialog.getByRole("button", { name: "Überspringen" }).click();
    await page.locator("#queue-count").filter({ hasText: "1" }).waitFor();
  });
} finally {
  await cleanup.close();
}

for (const result of results) console.log(result);
if (requestedCase && !matchedRequestedCase) {
  console.error(`Kein UI-Test enthält „${requestedCase}“ im Namen.`);
  process.exitCode = 1;
}
if (serverStderr.trim() && failed) console.error(serverStderr.trim());
if (failed) process.exitCode = 1;
