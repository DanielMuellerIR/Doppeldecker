#!/usr/bin/env node

/** Reproduzierbare Browser-Suite gegen die echte Oberfläche und die kontrollierte
 * Tauri-Mockbrücke. Der Test verändert weder Nutzerdaten noch das echte App-Profil. */

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { forwardConfirmationFixture } from "./tests/forward-confirmation-fixture.mjs";
import { runBatchRenameCases } from "./tests/batch-rename-cases.mjs";
import { runFilePreviewCases } from "./tests/file-preview-cases.mjs";
import { runFinderCommentCases } from "./tests/finder-comment-cases.mjs";
import { runAdministratorCases } from "./tests/administrator-cases.mjs";
import { runFileFlagCases } from "./tests/file-flag-cases.mjs";
import { runVisibilityCases } from "./tests/visibility-cases.mjs";
import { runFinderTagCases } from "./tests/finder-tag-cases.mjs";
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
  await page.getByRole("dialog").getByRole("button", { name: "Überspringen", exact: true }).waitFor();
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


  for (const theme of ["light", "dark"]) {
    await runCase(`Neuordnerrechte exakte große V7-IDs · ${theme}`, "", async page => {
      const fixtureFile = process.env.DOPPELDECKER_TEST_FRONTEND_REPORT_FIXTURE;
      const key = { origin: "ExclusiveMinimalCreate", directory: "/ziel/neu", object_namespace: Array(32).fill(1) };
      const fallback = {
        created: [
          { effect_key: { ...key, file_id: "9007199254740992" }, ordinary_metadata_applied: false },
          { effect_key: { ...key, file_id: "9007199254740993" }, ordinary_metadata_applied: true },
        ],
        completed: [{ ...key, file_id: "9007199254740993", before_digest: Array(32).fill(2), after_digest: Array(32).fill(3) }],
        uncertain: [{ ...key, file_id: "18446744073709551615", before_digest: Array(32).fill(2), after_digest: Array(32).fill(3) }],
      };
      const report = fixtureFile ? JSON.parse(await readFile(fixtureFile, "utf8")).entries[0].created_directory_dacl_report : fallback;
      await page.evaluate(async ({ report, theme }) => {
        const { setTheme } = await import("/theme.js"); setTheme(theme);
        const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
        queue.entries = [{ operation_id: "exact-v7-ids", operation: { type: "ContentSync", source: { provider: "Smb", root: "/quelle" }, target: { provider: "Smb", root: "/ziel" }, options: { mode: "Update" } },
          state: "Interrupted", attempts: 2, created_directory_dacl_recovery: true,
          created_directory_dacl_report: report, progress: { phase: "interrupted", created_directory_dacl_report: report } }];
        window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
        window.__MOCK_EMIT__("sync-progress", { phase: "metadata", current: "/ziel/neu", created_directory_dacl_report: report });
      }, { report, theme });
      const dock = page.locator("#dock-summary");
      assert.match(await dock.innerText(), /2 neue Ordner angelegt · gewöhnliche Metadaten bei 1 bestätigt/);
      assert.match(await dock.innerText(), /Rechte neuer Ordner: 1 bestätigt · 1 Ausgang unklar/);
      await page.locator("#btn-queue").click();
      const row = page.locator('[data-queue-id="exact-v7-ids"]');
      assert.match(await row.innerText(), /2 neue Ordner angelegt · gewöhnliche Metadaten bei 1 bestätigt/);
      assert.match(await row.innerText(), /Rechte neuer Ordner: 1 bestätigt · 1 Ausgang unklar/);
      await row.locator("summary").filter({ hasText: "2 neue Ordner angelegt" }).click();
      assert.equal(await row.locator("details").filter({ has: page.locator("summary", { hasText: "2 neue Ordner angelegt" }) }).locator("li").count(), 2);
      if (process.env.DOPPELDECKER_SCREENSHOT_DIR) {
        await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/v7-exact-ids-${theme}.png` });
      }
    });
  }
  for (const theme of ["light", "dark"]) {
    await runCase(`DACL-Berichte Fortschritt ersetzen · ${theme}`, "", async page => {
      await page.setViewportSize({ width: 1000, height: 760 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js"); setTheme(theme);
        window.__MOCK_EMIT__("sync-preparation-started", "Dateisync wird vorbereitet");
        const summary = document.querySelector("#dock-summary");
        summary.innerHTML = '<span class="retained-progress-context">Laufkontext bleibt</span>' +
          ["directory-stream-report", "directory-dacl-report", "file-dacl-report", "created-directory-dacl-report"]
            .flatMap(name => [`<div class="${name}">Alter Bericht 1</div>`, `<div class="${name}">Alter Bericht 2</div>`]).join("");
      }, theme);
      for (let index = 0; index < 3; index++) {
        await page.evaluate(index => {
          const path = `/aktuell-${index}`;
          window.__MOCK_EMIT__("sync-progress", {
            phase: "metadata", current: path, files_total: 3, files_done: index, bytes_total: 0, bytes_done: 0,
            directory_dacl_report: { completed: [{ directory: `${path}/ordner`, before_digest: [1], after_digest: [2] }], uncertain: [] },
            file_dacl_report: { completed: [{ file: `${path}/datei`, alias_scope: { selected_paths: [`${path}/datei`], observed_link_count: 1 } }], uncertain: [] },
            created_directory_dacl_report: { created: [{ effect_key: { origin: "ExclusiveMinimalCreate", directory: `${path}/neu`, file_id: 42 }, ordinary_metadata_applied: true }], completed: [], uncertain: [] },
          });
        }, index);
        for (const selector of [".directory-dacl-report", ".file-dacl-report", ".created-directory-dacl-report"]) {
          assert.equal(await page.locator(`#dock-summary ${selector}`).count(), 1, `${selector}: genau ein aktueller Bericht`);
          assert.match(await page.locator(`#dock-summary ${selector}`).textContent(), new RegExp(`/aktuell-${index}`));
          if (index > 0) assert.doesNotMatch(await page.locator(`#dock-summary ${selector}`).textContent(), new RegExp(`/aktuell-${index - 1}/`));
        }
        assert.equal(await page.locator("#dock-summary .directory-stream-report").count(), 0);
        assert.equal(await page.locator("#dock-summary .retained-progress-context").innerText(), "Laufkontext bleibt");
        assert.doesNotMatch(await page.locator("#dock-summary").innerText(), /Alter Bericht/);
      }
      assert.equal(await page.locator("#dock").isVisible(), true);
      if (process.env.DOPPELDECKER_SCREENSHOT_DIR) {
        await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/dacl-progress-replacement-${theme}.png` });
        await page.locator("#dock-summary").evaluate(summary => { summary.scrollTop = summary.scrollHeight; });
        await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/dacl-progress-replacement-${theme}-bottom.png` });
      }
      await page.evaluate(() => window.__MOCK_EMIT__("sync-progress", {
        phase: "metadata", current: "leer", files_total: 3, files_done: 3,
        directory_dacl_report: { completed: [], uncertain: [] },
        file_dacl_report: { completed: [], uncertain: [] },
        created_directory_dacl_report: { created: [], completed: [], uncertain: [] },
      }));
      assert.equal(await page.locator("#dock-summary .directory-stream-report, #dock-summary .directory-dacl-report, #dock-summary .file-dacl-report, #dock-summary .created-directory-dacl-report").count(), 0);
      assert.equal(await page.locator("#dock-summary .retained-progress-context").innerText(), "Laufkontext bleibt");
    });
  }
  for (const theme of ["light", "dark"]) {
    await runCase(`Neuordnerrechte Vorschau · ${theme}`, "", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js"); setTheme(theme);
        const { confirmContentSync } = await import("/content-sync-ui.js");
        window.__CREATED_CONFIRM__ = null;
        void confirmContentSync({ source_provider: "Smb", target_provider: "Smb", source_root: "/quelle", target_root: "/ziel",
          plan: { actions: [], copy_count: 0, update_count: 0, delete_count: 0, bytes_to_transfer: 0 },
          created_directory_dacl_effects: [{ directory: "/ziel/Leer<&>🌍", selection: {} }] }).then(value => { window.__CREATED_CONFIRM__ = value; });
      }, theme);
      const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" });
      assert.match(await dialog.innerText(), /Nur tatsächlich neu angelegte, vollständig leere Ordner/);
      assert.match(await dialog.innerText(), /normale Anlage bleibt erhalten/);
      await dialog.locator("summary").click(); assert.match(await dialog.innerText(), /Leer<&>🌍/);
      assert.equal(await dialog.locator("img").count(), 0);
      await dialog.getByRole("button", { name: "Dauerhafte Änderungen bestätigen", exact: true }).click();
      assert.deepEqual(await page.evaluate(() => window.__CREATED_CONFIRM__), { effectsConfirmed: true, removalPhrase: null });
    });
  }
  for (const prepared of [false, true]) {
    await runCase(`Neuordnerrechte Queue und lesende Recovery · Prepared ${prepared}`, "", async page => {
      await page.evaluate(prepared => {
        const item = { origin: "ExclusiveMinimalCreate", directory: "/ziel/Leer<&>", file_id: 42, object_namespace: [1], before_digest: [2], after_digest: [3] };
        const report = { created: [{ effect_key: item, ordinary_metadata_applied: true }], completed: [], uncertain: prepared ? [] : [item] };
        const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
        queue.entries = [{ operation_id: "created-dacl", operation: { type: "ContentSync", source: { provider: "Smb", root: "/quelle" }, target: { provider: "Smb", root: "/ziel" }, options: { mode: "Update" } }, state: "Interrupted", attempts: 1, error_code: "outcome_unclear", created_directory_dacl_recovery: true, created_directory_dacl_report: report, progress: { phase: "interrupted", created_directory_dacl_report: report } }];
        window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
      }, prepared);
      await page.locator("#btn-queue").click(); const row = page.locator('[data-queue-id="created-dacl"]');
      assert.match(await row.innerText(), /1 neue Ordner angelegt/);
      assert.equal(await row.locator('[data-queue-action="retry"]').count(), 0); assert.equal(await row.locator('[data-queue-action="unclear-ack"]').count(), 0);
      await row.getByRole("button", { name: "Rechte neuer Ordner prüfen", exact: true }).click();
      const dialog = page.getByRole("dialog", { name: "Rechte neuer Ordner prüfen und abschließen" });
      assert.match(await dialog.innerText(), /nur Identität und aktueller Rechtezustand/);
      assert.match(await dialog.innerText(), /setzt keine Rechte erneut/);
      await dialog.getByRole("button", { name: "Fortfahren", exact: true }).click();
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state === "Cancelled");
      assert.deepEqual(await page.evaluate(() => window.__MOCK_CREATED_DIRECTORY_DACL_ACKS__), [{ operationId: "created-dacl", possibleEffectReviewed: true }]);
    });
  }
  for (const event of ["sync-done", "sync-error"]) {
    await runCase(`Neuordnerrechte Abschlussbericht · ${event}`, "", async page => {
      await page.evaluate(event => { window.__MOCK_EMIT__(event, { dry_run: false, cancelled: false, files_copied: 0, files_deleted: 0, metadata_updated: 0, bytes_copied: 0, code: "outcome_unclear", message: "Rechte prüfen", created_directory_dacl_report: { created: [{ effect_key: { origin: "ExclusiveMinimalCreate", directory: "/ziel/Leer<&>", file_id: 42, object_namespace: [1] }, ordinary_metadata_applied: true }], completed: [], uncertain: [{ origin: "ExclusiveMinimalCreate", directory: "/ziel/Leer<&>", file_id: 42, object_namespace: [1], before_digest: [2], after_digest: [3] }] } }); }, event);
      const summary = page.locator("#dock-summary"); await summary.filter({ hasText: "Rechte neuer Ordner: 0 bestätigt · 1 Ausgang unklar" }).waitFor();
      assert.match(await summary.innerText(), /1 neue Ordner angelegt/); assert.match(await summary.innerText(), /setzt keine Rechte erneut/);
    });
  }
  for (const theme of ["light", "dark"]) {
    await runCase(`Frühe nichtleere Ordnerrechte Vorschau · ${theme}`, "", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js"); setTheme(theme);
        const { confirmContentSync } = await import("/content-sync-ui.js");
        window.__BEFORE_CHILDREN_CONFIRM__ = null;
        void confirmContentSync({ source_provider: "Smb", target_provider: "Smb", source_root: "/quelle", target_root: "/ziel",
          plan: { actions: [], copy_count: 2, update_count: 0, delete_count: 0, bytes_to_transfer: 59 },
          created_directory_before_children_effects: [{ directory: "/ziel/Nichtleer<&>🌍", selection: {} }] }).then(value => { window.__BEFORE_CHILDREN_CONFIRM__ = value; });
      }, theme);
      const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" });
      assert.match(await dialog.innerText(), /Rechte vor den Kindern/);
      assert.match(await dialog.innerText(), /späteren Kind- oder Metadatenfehler/);
      assert.doesNotMatch(await dialog.innerText(), /vollständig leere Ordner/);
      await dialog.locator("summary").click();
      assert.match(await dialog.innerText(), /Nichtleer<&>🌍/);
      assert.equal(await dialog.locator("img").count(), 0);
      const confirm = dialog.getByRole("button", { name: "Dauerhafte Änderungen bestätigen", exact: true });
      assert.match(await confirm.getAttribute("class"), /danger/);
      await confirm.click();
      assert.deepEqual(await page.evaluate(() => window.__BEFORE_CHILDREN_CONFIRM__), { effectsConfirmed: true, removalPhrase: null });
    });
  }
  for (const kind of ["completed", "child-error", "uncertain", "prepared", "prepared-without-history"]) {
    await runCase(`Frühe Ordnerrechte Queue · ${kind}`, "", async page => {
      await page.evaluate(kind => {
        const key = { origin: "ExclusiveMinimalCreateBeforeChildren", directory: "/ziel/Nichtleer<&>", file_id: "42", object_namespace: [1] };
        const effect = { ...key, before_digest: [2], after_digest: [3] };
        const report = { created: kind === "prepared-without-history" ? [] : [{ effect_key: key, ordinary_metadata_applied: kind === "completed" }],
          completed: ["completed", "child-error"].includes(kind) ? [effect] : [], uncertain: kind === "uncertain" ? [effect] : [] };
        const recovery = ["uncertain", "prepared", "prepared-without-history"].includes(kind);
        const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
        queue.entries = [{ operation_id: "before-children", operation: { type: "ContentSync", source: { provider: "Smb", root: "/quelle" }, target: { provider: "Smb", root: "/ziel" }, options: { mode: "Update" },
          expected_plan: { actions: [{ type: "CreateDir", metadata: { windows_dacl: { CreatedDirectoryBeforeChildrenExact: {} } } }] } },
          state: kind === "completed" ? "Completed" : kind === "child-error" ? "Failed" : "Interrupted", attempts: 1,
          error_code: kind === "child-error" ? "plan_drift" : recovery ? "outcome_unclear" : null,
          created_directory_dacl_recovery: recovery, created_directory_dacl_report: report, progress: { phase: "interrupted", created_directory_dacl_report: report } }];
        window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
      }, kind);
      await page.locator("#btn-queue").click();
      const row = page.locator('[data-queue-id="before-children"]');
      assert.equal(await row.locator('[data-queue-action="retry"]').count(), 0);
      assert.equal(await row.locator('[data-queue-action="unclear-ack"]').count(), 0);
      if (kind === "prepared-without-history") {
        assert.match(await row.innerText(), /Früher Rechteabschluss noch zu prüfen/);
      } else {
        assert.match(await row.innerText(), /Ordnerrechte vor den Kindern/);
        assert.match(await row.innerText(), new RegExp(`gewöhnliche Metadaten bei ${kind === "completed" ? 1 : 0} bestätigt`));
      }
      assert.doesNotMatch(await row.innerText(), /neuer leerer|\bleeren Ordner/);
      assert.match(await row.innerText(), /keine Kinderarbeit fort/);
      assert.equal(await row.locator("img").count(), 0);
      if (["uncertain", "prepared", "prepared-without-history"].includes(kind)) {
        await row.getByRole("button", { name: "Rechte neuer Ordner prüfen", exact: true }).click();
        const dialog = page.getByRole("dialog", { name: "Rechte neuer Ordner prüfen und abschließen" });
        assert.match(await dialog.innerText(), /keine Kinderarbeit fort/);
        await dialog.getByRole("button", { name: "Fortfahren", exact: true }).click();
        assert.deepEqual(await page.evaluate(() => window.__MOCK_CREATED_DIRECTORY_DACL_ACKS__), [{ operationId: "before-children", possibleEffectReviewed: true }]);
      }
    });
  }
  await runCase("Frühe Ordnerrechte behalten getrennte Objektidentitäten", "", async page => {
    const html = await page.evaluate(async () => {
      const { createdDirectoryDaclReportHtml } = await import("/created-directory-dacl-effects.js");
      const key = { origin: "ExclusiveMinimalCreateBeforeChildren", directory: "/gleich<&>", object_namespace: [1] };
      return createdDirectoryDaclReportHtml({ created: [{ effect_key: { ...key, file_id: "9007199254740992" }, ordinary_metadata_applied: false }], completed: [], uncertain: [] },
        { created: [{ effect_key: { ...key, file_id: "9007199254740993" }, ordinary_metadata_applied: true }], completed: [], uncertain: [] });
    });
    assert.match(html, /2 neue Ordner angelegt · gewöhnliche Metadaten bei 1 bestätigt/);
    assert.match(html, /gleich&lt;&amp;&gt;/);
  });
  await runCase("Neuordnerrechte alte leere Formen bleiben leer", "", async page => {
    assert.deepEqual(await page.evaluate(async () => { const { createdDirectoryDaclEffectsHtml: effects, createdDirectoryDaclReportHtml: report } = await import("/created-directory-dacl-effects.js"); return [effects(), report(), report({ completed: [], uncertain: [] })]; }), ["", "", ""]);
  });
  for (const theme of ["light", "dark"]) {
    await runCase(`Objektweite Dateirechte mit ausdrücklicher Namensbestätigung · ${theme}`, "", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js"); setTheme(theme);
        const { confirmContentSync } = await import("/content-sync-ui.js");
        window.__FILE_SCOPE_CONFIRM__ = null;
        void confirmContentSync({ source_provider: "Smb", target_provider: "Smb", source_root: "/quelle", target_root: "/ziel",
          plan: { actions: [], copy_count: 0, update_count: 0, delete_count: 0, bytes_to_transfer: 0 },
          file_dacl_effects: [{ file: "/ziel/Datei<&>🌍", file_id: 42, object_namespace: [1], before_digest: [2], after_digest: [3], alias_scope: { selected_paths: ["Datei<&>🌍", "zweiter Name"], observed_link_count: 1, all_existing_and_future_names_including_outside_root: true } }] }).then(value => { window.__FILE_SCOPE_CONFIRM__ = value; });
      }, theme);
      const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" });
      await dialog.locator("summary").click();
      assert.match(await dialog.innerText(), /1 Dateiobjekt.*2 ausgewählte Namen/);
      assert.match(await dialog.innerText(), /auch außerhalb der ausgewählten Ordner/);
      assert.match(await dialog.innerText(), /Liste zeigt nur die hier bekannten Namen/);
      assert.match(await dialog.innerText(), /Anzahl von 1 begrenzt die Wirkung nicht/);
      assert.match(await dialog.innerText(), /Datei<&>🌍/); assert.equal(await dialog.locator("img").count(), 0);
      const start = dialog.getByRole("button", { name: "Dauerhafte Änderungen bestätigen" });
      assert.equal(await start.isDisabled(), true); await dialog.getByRole("checkbox").check(); assert.equal(await start.isEnabled(), true);
      await dialog.getByRole("checkbox").uncheck(); assert.equal(await start.isDisabled(), true);
      await dialog.getByRole("checkbox").check(); await start.click();
      assert.deepEqual(await page.evaluate(() => window.__FILE_SCOPE_CONFIRM__), { effectsConfirmed: true, removalPhrase: null });
    });
  }
  for (const prepared of [false, true]) {
    await runCase(`Objektweite Dateirechte Queue und Recovery · Prepared ${prepared}`, "", async page => {
      await page.evaluate(prepared => {
        const item = { file: "/ziel/Datei<&>", file_id: 42, object_namespace: [1], before_digest: [2], after_digest: [3], alias_scope: { selected_paths: ["Datei<&>", "Alias"], observed_link_count: null, all_existing_and_future_names_including_outside_root: true } };
        const report = { completed: [], uncertain: prepared ? [] : [item] };
        const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
        queue.entries = [{ operation_id: "file-dacl-report", operation: { type: "ContentSync", source: { provider: "Smb", root: "/quelle" }, target: { provider: "Smb", root: "/ziel" }, options: { mode: "Update" } }, state: "Interrupted", attempts: 1, error_code: "outcome_unclear", file_dacl_recovery: true, file_dacl_report: report, progress: { phase: "interrupted", file_dacl_report: report }, temp_recovery_paths: ["/ziel/Datei<&>"] }];
        window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
      }, prepared);
      await page.locator("#btn-queue").click();
      const row = page.locator('[data-queue-id="file-dacl-report"]');
      const button = row.getByRole("button", { name: "Objektweite Dateirechte prüfen" }); await button.waitFor();
      assert.equal(await row.locator('[data-queue-action="retry"]').count(), 0); assert.equal(await row.locator('[data-queue-action="unclear-ack"]').count(), 0);
      if (!prepared) assert.match(await row.innerText(), /Objektweite Dateirechte: 0 bestätigt · 1 Ausgang unklar/);
      await button.click(); const dialog = page.getByRole("dialog", { name: "Objektweite Dateirechte prüfen und abschließen" });
      assert.match(await dialog.innerText(), /setzt keine Rechte erneut/);
      await dialog.getByRole("button", { name: "Fortfahren", exact: true }).click();
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state === "Cancelled");
      assert.deepEqual(await page.evaluate(() => window.__MOCK_FILE_DACL_ACKS__), [{ operationId: "file-dacl-report", possibleEffectReviewed: true }]);
    });
  }
  for (const theme of ["light", "dark"]) {
    await runCase(`Ordnerrechte Vorschau und klare Dauerwirkung · ${theme}`, "", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js"); setTheme(theme);
        const { confirmContentSync } = await import("/content-sync-ui.js");
        void confirmContentSync({ source_provider: "Smb", target_provider: "Smb", source_root: "/quelle", target_root: "/ziel",
          plan: { actions: [], copy_count: 0, update_count: 0, delete_count: 0 },
          directory_dacl_effects: [{ directory: "/ziel/Belege<&>🌍", before_digest: [1], after_digest: [2] }] });
      }, theme);
      const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" });
      await dialog.locator("summary").click();
      assert.match(await dialog.innerText(), /Belege<&>🌍/);
      assert.match(await dialog.innerText(), /dauerhaft, ohne Quarantäne/);
      assert.match(await dialog.innerText(), /Eigentümer, Gruppe, Ordner und ihre Inhalte bleiben erhalten/);
      assert.equal(await dialog.locator("img").count(), 0);
      const start = dialog.getByRole("button", { name: "Dauerhafte Änderungen bestätigen" });
      assert.equal(await start.getAttribute("class"), "btn danger");
      assert.equal(await dialog.getByRole("textbox").count(), 0);
      const box = await dialog.boundingBox(); assert.ok(box && box.x >= 0 && box.x + box.width <= 761);
      await page.keyboard.press("Escape");
    });
  }
  await runCase("Ordnerrechte Legacy bleibt leer", "", async page => {
    const html = await page.evaluate(async () => {
      const { directoryDaclEffectsHtml, directoryDaclReportHtml } = await import("/directory-dacl-effects.js");
      return [directoryDaclEffectsHtml(), directoryDaclEffectsHtml([]), directoryDaclReportHtml(), directoryDaclReportHtml({ completed: [], uncertain: [] })];
    });
    assert.deepEqual(html, ["", "", "", ""]);
  });
  for (const prepared of [false, true]) {
    await runCase(`Ordnerrechte Queue und gebundene Recovery · Prepared ${prepared}`, "", async page => {
      await page.evaluate(prepared => {
        const known = { directory: "/ziel/Bekannt<&>", before_digest: [1], after_digest: [2] };
        const unknown = { directory: "/ziel/Unklar", before_digest: [3], after_digest: [4] };
        const report = prepared ? { completed: [], uncertain: [] } : { completed: [known], uncertain: [unknown] };
        const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
        queue.entries = [{ operation_id: "dacl-report", operation: { type: "ContentSync", source: { provider: "Smb", root: "/quelle" }, target: { provider: "Smb", root: "/ziel" }, options: { mode: "Update" } },
          state: "Interrupted", attempts: 1, error_code: prepared ? "interrupted" : "outcome_unclear", error: "Rechte prüfen", directory_dacl_recovery: true,
          directory_dacl_report: report, progress: { phase: "interrupted", directory_dacl_report: report }, temp_recovery_paths: ["/ziel/Unklar"] }];
        window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
      }, prepared);
      await page.locator("#btn-queue").click();
      const row = page.locator('[data-queue-id="dacl-report"]');
      const button = row.getByRole("button", { name: "Zugriffsrechte prüfen" });
      await button.waitFor();
      assert.equal(await row.locator('[data-queue-action="retry"]').count(), 0);
      assert.equal(await row.locator('[data-queue-action="unclear-ack"]').count(), 0);
      if (!prepared) assert.match(await row.innerText(), /Ordnerrechte: 1 bestätigt · 1 Ausgang unklar/);
      else assert.equal(await row.locator(".directory-dacl-report").count(), 0);
      await button.click();
      let dialog = page.getByRole("dialog", { name: "Ordnerrechte prüfen und abschließen" });
      assert.match(await dialog.innerText(), /ohne erneutes Setzen/);
      await dialog.getByRole("button", { name: "Abbrechen", exact: true }).click();
      assert.deepEqual(await page.evaluate(() => window.__MOCK_DACL_ACKS__ || []), []);
      await button.click();
      dialog = page.getByRole("dialog", { name: "Ordnerrechte prüfen und abschließen" });
      await dialog.getByRole("button", { name: "Fortfahren", exact: true }).click();
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state === "Cancelled");
      assert.deepEqual(await page.evaluate(() => window.__MOCK_DACL_ACKS__), [{ operationId: "dacl-report", possibleEffectReviewed: true }]);
      assert.equal(await row.locator('[data-queue-action="dacl-recovery"]').count(), 0);
      assert.match(await row.innerText(), /abgebrochen/);
    });
  }
  for (const event of ["sync-done", "sync-error"]) {
    await runCase(`Ordnerrechte Abschlussbericht · ${event}`, "", async page => {
      await page.evaluate(event => {
        window.__MOCK_EMIT__(event, { dry_run: false, cancelled: false, files_copied: 0, files_deleted: 0, metadata_updated: 1, bytes_copied: 0,
          code: "outcome_unclear", message: "Rechte prüfen", directory_dacl_report: {
            completed: [{ directory: "/ziel/Bekannt<&>", before_digest: [1], after_digest: [2] }],
            uncertain: [{ directory: "/ziel/Unklar", before_digest: [3], after_digest: [4] }] } });
      }, event);
      const summary = page.locator("#dock-summary");
      await summary.filter({ hasText: "Ordnerrechte: 1 bestätigt · 1 Ausgang unklar" }).waitFor();
      assert.match(await summary.innerText(), /keine automatische Wiederholung/);
    });
  }

  for (const kind of ["add", "replace", "remove"]) {
    await runCase(`Verzeichnisstreams reine Vorschau: ${kind}`, "", async page => {
      await page.evaluate(async kind => {
        const { confirmContentSync } = await import("/content-sync-ui.js");
        const blob = (name, size) => ({ name: [...new TextEncoder().encode(name)], value: { size, digest: "fixture" } });
        const effect = { kind, ...(kind !== "add" ? { before: blob("Größe<&>🌍", 0) } : {}), ...(kind !== "remove" ? { after: blob("Größe<&>🌍", 19) } : {}) };
        void confirmContentSync({ source_provider: "Smb", target_provider: "Smb", source_root: "/quelle", target_root: "/ziel",
          plan: { actions: [], copy_count: 0, update_count: 0, delete_count: 0 },
          directory_stream_effects: Array.from({ length: 61 }, (_, index) => ({ directory: `ordner-${index}`, effect })) });
      }, kind);
      const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" });
      await dialog.locator("summary").click();
      const text = await dialog.innerText();
      assert.match(text, /0 neu · 0 ersetzen · 0 entfernen/);
      assert.match(text, /Größe<&>🌍/);
      assert.match(text, /ordner-60/);
      assert.equal(await dialog.locator("li").count(), 61);
      assert.match(text, kind === "replace" ? /0 → 19 Bytes/ : kind === "remove" ? /0 Bytes/ : /19 Bytes/);
      const button = dialog.getByRole("button", { name: kind === "add" ? "Sync bestätigen und starten" : "Dauerhafte Änderungen bestätigen" });
      assert.equal(await button.getAttribute("class"), kind === "add" ? "btn primary" : "btn danger");
      if (kind !== "add") assert.match(text, /dauerhaft, ohne Quarantäne/);
      if (kind === "replace") assert.match(text, /keinen atomaren Versionsvergleich gegen parallele Änderungen/);
      await page.keyboard.press("Escape");
    });
  }
  await runCase("Verzeichnisstreams Legacy und leere Wirkung", "", async page => {
    const html = await page.evaluate(async () => {
      const { directoryStreamEffectsHtml, directoryStreamReportHtml } = await import("/directory-stream-effects.js");
      return [directoryStreamEffectsHtml(), directoryStreamEffectsHtml([]), directoryStreamReportHtml(), directoryStreamReportHtml({ completed: [], uncertain: [] })];
    });
    assert.deepEqual(html, ["", "", "", ""]);
    await page.evaluate(async () => {
      const { confirmContentSync } = await import("/content-sync-ui.js");
      void confirmContentSync({ source_provider: "Smb", target_provider: "Smb", source_root: "/quelle", target_root: "/ziel", plan: { actions: [], copy_count: 0, update_count: 0, delete_count: 0 } });
    });
    const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" });
    await dialog.getByRole("button", { name: "Sync bestätigen und starten" }).waitFor();
    assert.equal(await dialog.getByRole("button", { name: "Sync bestätigen und starten" }).count(), 1);
    assert.doesNotMatch(await dialog.innerText(), /Verzeichnisströme/);
    await page.keyboard.press("Escape");
  });
  for (const state of ["Failed", "Cancelled", "Interrupted"]) {
    await runCase(`Verzeichnisstreams Queue-Teilbilanz: ${state}`, "", async page => {
      await page.evaluate(state => {
        const item = (kind, name, size) => ({ directory: "Ordner<&>", effect: { kind, [kind === "add" ? "after" : "before"]: { name: [...new TextEncoder().encode(name)], value: { size, digest: "fixture" } } } });
        const known = item("add", "bekannt🌍", 0);
        const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
        queue.entries = [{ operation_id: "stream-report", operation: { type: "File", kind: "Copy", items: ["/mock/quelle"], destination: "/mock/ziel", expected_conflicts: [] },
          created_unix_ms: Date.now(), state, attempts: 2, error: "Teilwirkung prüfen", error_code: "outcome_unclear", attention: null,
          directory_stream_report: { completed: [known], uncertain: [item("remove", "unklar", 8)] },
          progress: { directory_stream_report: { completed: [known], uncertain: [] } } }];
        window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
      }, state);
      await page.locator("#btn-queue").click();
      const row = page.locator('[data-queue-id="stream-report"]');
      assert.match(await row.innerText(), /Verzeichnisströme: 1 bestätigt · 1 Ausgang unklar/);
      for (const detail of await row.locator("details").all()) await detail.locator("summary").click();
      assert.match(await row.innerText(), /bekannt🌍.*0 Bytes/s);
      assert.match(await row.innerText(), /Dauerhaft entfernen.*unklar.*8 Bytes/s);
      assert.match(await row.innerText(), /Ordner<&>/);
      await page.locator("#btn-queue").click();
      await page.locator("#btn-queue").click();
      assert.match(await row.innerText(), /1 bestätigt · 1 Ausgang unklar/);
    });
  }
  for (const event of ["sync-done", "sync-error"]) {
    await runCase(`Verzeichnisstreams Sync-Teilbilanz: ${event}`, "", async page => {
      await page.evaluate(event => {
        const outcome = kind => ({ directory: "Belege", effect: { kind, [kind === "add" ? "after" : "before"]: { name: [...new TextEncoder().encode(kind)], value: { size: 0, digest: "fixture" } } } });
        window.__MOCK_EMIT__(event, { dry_run: false, cancelled: true, files_copied: 0, files_deleted: 0, metadata_updated: 0, bytes_copied: 0,
          code: "outcome_unclear", message: "Teilwirkung prüfen", directory_stream_report: { completed: [outcome("add")], uncertain: [outcome("remove")] } });
      }, event);
      const summary = page.locator("#dock-summary");
      await summary.filter({ hasText: "1 bestätigt · 1 Ausgang unklar" }).waitFor();
      await summary.getByText("Bestätigte Einzelwirkungen: 1 Streamwirkung(en)", { exact: true }).click();
      await summary.getByText("Ungeklärte Einzelversuche: 1 Streamwirkung(en)", { exact: true }).click();
      assert.match(await summary.innerText(), /Hinzufügen.*0 Bytes.*Dauerhaft entfernen.*0 Bytes/s);
    });
  }

  await runFilePreviewCases(runCase);
  await runBatchRenameCases(runCase);

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
    await runCase(`AP21 ein Remote-Konfliktdialog · ${theme} · ${key} · ${policy}`, "?remoteFile=1&ftpsConflictChoice=1", async page => {
      await openFtpsConflict(page, theme, key);
      const dialog = page.getByRole("dialog");
      await dialog.getByRole("button", { name: "Dauerhaft ersetzen", exact: true }).waitFor();
      assert.equal(await dialog.getByRole("button", { name: /prüfen/ }).count(), 0);
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
      if (policy === "cancel") {
        await page.keyboard.press("Escape");
        await dialog.waitFor({ state: "hidden" });
        assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
        return;
      }
      await dialog.getByRole("button", { name: policy === "skip" ? "Überspringen" : policy === "overwrite" ? "Dauerhaft ersetzen" : "Beide behalten", exact: true }).click();
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
      assert.equal(await dialog.count(), 0, "Die gewählte Wirkung darf keinen zweiten Konfliktdialog öffnen.");
      const state = await page.evaluate(() => ({ policies: window.__AP21_POLICIES__, operation: window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation }));
      assert.deepEqual(state.policies, policy === "skip" ? [null, "inspect", "skip"] : [null, "inspect"]);
      assert.equal(state.operation.overwrite, policy === "overwrite");
      assert.equal(state.operation.remove_source_best_effort_expected, key === "F6" && policy !== "skip" ? true : null);
      assert.equal(state.operation.replace_best_effort_expected, policy === "overwrite");
      assert.equal(Boolean(await page.evaluate(() => window.__LAST_PROVIDER_FILE_REQUEST__.keepBothPreparationId)), policy === "both");
    });
  }

  for (const theme of ["light", "dark"]) {
    await runCase(`AP21 nicht verfügbares Ersetzen lässt Skip erreichbar · ${theme}`, "?remoteFile=1&ftpsConflictChoice=1&ftpsReplaceDenied=1", async page => {
      await openFtpsConflict(page, theme, "F6");
      const dialog = page.getByRole("dialog");
      await dialog.getByText("Ersetzen ist für diese Ordnerauswahl nicht gebunden", { exact: true }).waitFor();
      assert.equal(await dialog.getByRole("button", { name: "Dauerhaft ersetzen", exact: true }).count(), 0);
      await dialog.getByRole("button", { name: "Überspringen", exact: true }).click();
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
      const operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation);
      assert.equal(operation.overwrite, false);
      assert.equal(operation.remove_source_best_effort_expected, null);
    });
    await runCase(`AP21 neue Namen sind vor der einzigen Entscheidung sichtbar · ${theme}`, "?remoteFile=1&ftpsConflictChoice=1", async page => {
      await openFtpsConflict(page, theme, "F5");
      const dialog = page.getByRole("dialog");
      await dialog.getByText(/remote \(Kopie\)\.txt/).waitFor();
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
      await page.keyboard.press("Escape");
      await dialog.waitFor({ state: "hidden" });
    });
  }

  for (const theme of ["light", "dark"]) for (const drift of [false, true]) {
    await runCase(`Audit Remote-Bearbeitung mit Rückupload · ${theme} · ${drift ? "Drift" : "Erfolg"}`, `?remoteFile=1${drift ? "&remoteEditDrift=1" : ""}`, async page => {
      await page.setViewportSize({ width: 900, height: 650 });
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme); }, theme);
      await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
      const file = page.getByRole("listbox", { name: /Linke Dateiliste/ }).getByRole("option", { name: /remote\.txt/ });
      await file.click({ button: "right" });
      await page.getByRole("menuitem", { name: "Bearbeiten und zurückspeichern", exact: true }).click();
      await page.locator("#btn-remote-edits").waitFor({ state: "visible" });
      assert.equal(await page.evaluate(() => window.__LAST_REMOTE_OPEN_MODE__), "edit");
      await page.locator("#btn-remote-edits").click();
      const dialog = page.getByRole("dialog");
      await dialog.getByText(/dauerhaft ohne Quarantäne/).waitFor();
      await dialog.getByRole("button", { name: "Zurückspeichern", exact: true }).click();
      if (drift) {
        await dialog.getByText(/Serverdatei wurde inzwischen verändert/).waitFor();
        assert.equal(await page.evaluate(() => window.__REMOTE_EDIT_SAVE_REQUEST__ || null), null);
      } else {
        await dialog.getByText(/Rückupload in Queue eingereiht/).waitFor();
        assert.deepEqual(await page.evaluate(() => window.__REMOTE_EDIT_SAVE_REQUEST__), { id: "mock-remote-edit" });
      }
      await dialog.getByRole("button", { name: "Bearbeitung beenden", exact: true }).click();
      await dialog.getByText(/Bearbeitung beendet/).waitFor();
      await dialog.getByRole("button", { name: "Schließen", exact: true }).click();
      await page.locator("#btn-remote-edits").waitFor({ state: "hidden" });
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
    assert.equal(await left.getByRole("listbox").getByRole("option").count(), 0);
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
    assert.equal(await left.getByRole("listbox").getByRole("option").count(), 0);
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
    const realMode = page.getByRole("dialog", { name: "Sync ausführen" });
    await realMode.waitFor();
    await page.waitForTimeout(180);
    assert.match(await page.locator("#modal-title").innerText(), /Sync ausführen/);
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
    const realMode = page.getByRole("dialog", { name: "Sync ausführen" });
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
      if (protocol === "FTP / FTPS") await dialog.locator("#ftps-policy").selectOption("web_pki_and_pin");
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
          await dialog.locator("#ftps-policy").selectOption("web_pki_and_pin");
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

  await runCase("SFTP-Agent-Schlüssel nachladen: neue Profile bieten die Anmeldung an", "?holdSftpAgent=1&sftpAgentKeys=1", async page => {
    await page.locator("#conn-add").click();
    await page.getByRole("dialog", { name: "Verbindung hinzufügen" })
      .getByRole("button", { name: /^SFTP/ }).click();
    const dialog = page.getByRole("dialog", { name: "SFTP-Verbindung hinzufügen" });
    await page.waitForFunction(() => window.__MOCK_REQUEST_PHASES__().holdSftpAgent?.phase === "pending");
    const auth = dialog.locator("#sftp-auth");
    assert.equal(await auth.locator('option[value="agent"]').count(), 0);
    await page.evaluate(() => window.__MOCK_RELEASE_REQUEST__("holdSftpAgent"));
    await page.waitForFunction(() => !document.querySelector("#sftp-auth").disabled);
    assert.equal(await auth.locator('option[value="agent"]').count(), 1);
    assert.equal(await auth.inputValue(), "password", "Nachladen ändert die gewählte Anmeldung nicht.");
    assert.equal(await dialog.locator("#sftp-password-row").isVisible(), true);
    await auth.selectOption("agent");
    assert.equal(await dialog.locator("#sftp-agent-row").isVisible(), true);
    assert.equal(await dialog.locator("#sftp-password-row").isVisible(), false);
    assert.match(await dialog.locator("#sftp-agent").innerText(), /Asynchroner Testschlüssel/);
    await dialog.locator("#sftp-name").fill("Agentprofil");
    await dialog.locator("#sftp-host").fill("agent.mock");
    await dialog.locator("#sftp-user").fill("tester");
    await dialog.getByRole("button", { name: "Hostkey ermitteln" }).click();
    await dialog.getByRole("button", { name: "Geprüften Hostkey binden und speichern" }).click();
    await dialog.waitFor({ state: "detached" });
    const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
    const profile = state.sftpProfiles.find(item => item.name === "Agentprofil");
    assert.deepEqual(profile.connection.authentication, { type: "ssh_agent", public_key_sha256: Array(32).fill(23) });
    assert.deepEqual(state.secretTransactions, []);
    const save = (await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__()))
      .find(action => action.command === "save_sftp_profile");
    assert.equal(save.secretId, null);
  });

  await runCase("SFTP-Agent-Schlüssel nachladen: geschlossener Editor verändert keinen Folgedialog", "?holdSftpAgent=1&sftpAgentKeys=1", async page => {
    await page.locator("#conn-add").click();
    await page.getByRole("dialog", { name: "Verbindung hinzufügen" })
      .getByRole("button", { name: /^SFTP/ }).click();
    const dialog = page.getByRole("dialog", { name: "SFTP-Verbindung hinzufügen" });
    await page.waitForFunction(() => window.__MOCK_REQUEST_PHASES__().holdSftpAgent?.phase === "pending");
    await dialog.getByRole("button", { name: "Abbrechen" }).click();
    await page.evaluate(async () => {
      const modal = await import("/modal.js");
      void modal.promptModal("Folgedialog", "unverändert");
    });
    const next = page.getByRole("dialog", { name: "Folgedialog" });
    await next.waitFor();
    await page.evaluate(() => window.__MOCK_RELEASE_REQUEST__("holdSftpAgent"));
    await page.waitForFunction(() => window.__MOCK_REQUEST_PHASES__().holdSftpAgent?.phase === "returned");
    assert.equal(await next.locator("#modal-input").inputValue(), "unverändert");
    assert.equal(await page.locator("#sftp-auth").count(), 0);
    assert.deepEqual(await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__()), []);
    await next.getByRole("button", { name: "Abbrechen" }).click();
  });

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
    assert.equal(await pane.getByRole("listbox").getByRole("option").count(), 0);
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

  await runFinderTagCases(runCase);
  await runVisibilityCases(runCase);
  await runFileFlagCases(runCase);
  await runAdministratorCases(runCase);
  await runFinderCommentCases(runCase);

  for (const theme of ["light", "dark"]) {
    await runCase(`Informationen ändern lokale Rechte mit Fehlerbilanz · ${theme}`, "?keyboard=1", async page => {
      await page.setViewportSize({ width: 650, height: 520 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js"); setTheme(theme);
        const original = window.__TAURI__.core.invoke;
        window.__PERMISSION_CALLS__ = [];
        window.__TAURI__.core.invoke = async (command, args) => {
          if (command !== "change_file_permissions") return original(command, args);
          window.__PERMISSION_CALLS__.push(args);
          return args.request.names.map(name => ({ name, mode: parseInt(args.request.mode, 8),
            changed: !window.__PERMISSION_UNCLEAR__, code: window.__PERMISSION_UNCLEAR__ ? "outcome_unclear" : null,
            message: window.__PERMISSION_UNCLEAR__ ? "Abschluss <img src=x> unklar; prüfen" : "Rechte bestätigt" }));
        };
      }, theme);
      const pane = page.getByRole("listbox", { name: /Linke Dateiliste/ });
      await pane.getByRole("option", { name: /Alpha.txt/ }).click();
      await page.keyboard.press("Meta+I");
      const dialog = page.getByRole("dialog", { name: /Informationen · Alpha.txt/ });
      await dialog.locator(".file-info-permissions summary").click();
      await dialog.getByLabel("Unix-Rechte (oktal)").fill("888");
      await dialog.getByRole("button", { name: "Rechte anwenden" }).click();
      await dialog.getByRole("status").getByText(/drei Ziffern/).waitFor();
      assert.deepEqual(await page.evaluate(() => window.__PERMISSION_CALLS__), []);
      await dialog.getByLabel("Unix-Rechte (oktal)").fill("640");
      await dialog.getByRole("button", { name: "Rechte anwenden" }).click();
      await dialog.getByRole("region", { name: "Ergebnis der Rechteänderung" }).getByText(/Rechte bestätigt/).waitFor();
      assert.match(await dialog.getByText("Berechtigungen", { exact: true }).locator("..").innerText(), /rw-r----- \(640\)/);
      const calls = await page.evaluate(() => window.__PERMISSION_CALLS__);
      assert.match(calls[0].request.requestId, /^[a-f0-9-]{36}$/);
      assert.deepEqual(calls, [{ request: {
        requestId: calls[0].request.requestId, provider: "local", root: "/mock/quelle", names: ["Alpha.txt"], mode: "640", recursive: false, profileId: null, profileRevision: null,
      } }]);
      await dialog.getByRole("button", { name: "Schließen" }).waitFor();
      await page.waitForFunction(() => document.querySelector("#modal .modal").contains(document.activeElement));
      assert.equal(await dialog.locator(".file-info-permissions").evaluate(element => element.open), true);
      assert.equal(await pane.getByRole("option", { name: /Alpha.txt/ }).getAttribute("aria-selected"), "true");
      await page.evaluate(() => { window.__PERMISSION_UNCLEAR__ = true; });
      await dialog.getByLabel("Unix-Rechte (oktal)").fill("600");
      await dialog.getByRole("button", { name: "Rechte anwenden" }).click();
      await dialog.getByRole("region", { name: "Ergebnis der Rechteänderung" }).getByText(/outcome_unclear/).waitFor();
      assert.match(await dialog.getByText("Berechtigungen", { exact: true }).locator("..").innerText(), /—/);
      assert.equal(await dialog.locator(".file-info-permission-report img").count(), 0);
      assert.equal(await dialog.evaluate(element => element.scrollWidth <= element.clientWidth), true);
      await dialog.getByRole("button", { name: "Schließen" }).click();
      await dialog.waitFor({ state: "hidden" });
    });
  }

  for (const theme of ["light", "dark"]) {
    await runCase(`Informationen rekursive Rechte mit gebundenem Abbruch und Ergebnisblättern · ${theme}`, "?keyboard=1", async page => {
      await page.setViewportSize({ width: 390, height: 760 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js"); setTheme(theme);
        const original = window.__TAURI__.core.invoke;
        window.__PERMISSION_CALLS__ = [];
        window.__TAURI__.core.invoke = (command, args) => {
          if (command === "change_file_permissions") {
            window.__PERMISSION_CALLS__.push({ command, ...args });
            return new Promise(resolve => { window.__PERMISSION_RESOLVE__ = resolve; });
          }
          if (command === "cancel_file_property_edit") {
            window.__PERMISSION_CALLS__.push({ command, ...args });
            window.__PERMISSION_RESOLVE__(Array.from({ length: 95 }, (_, index) => ({
              name: `Bilder/Ordner mit langem Namen/Datei-${index}.txt`, mode: index < 2 ? 0o750 : null,
              changed: index < 2, code: index < 2 ? null : "interrupted",
              message: index < 2 ? "Rechte bestätigt" : "Vor dieser Änderung abgebrochen; unverändert",
            })));
            return Promise.resolve();
          }
          return original(command, args);
        };
      }, theme);
      const pane = page.getByRole("listbox", { name: /Linke Dateiliste/ });
      await pane.getByRole("option", { name: /Bilder/ }).click();
      await page.keyboard.press("Meta+I");
      const dialog = page.getByRole("dialog", { name: "Informationen · Bilder" });
      await dialog.locator(".file-info-permissions summary").click();
      await dialog.getByLabel("Unix-Rechte (oktal)").fill("750");
      const recursive = dialog.getByLabel("Auch auf alle enthaltenen Dateien und Ordner anwenden");
      assert.equal(await recursive.isChecked(), false);
      await recursive.check();
      await dialog.getByRole("button", { name: "Rechte anwenden" }).click();
      await dialog.getByRole("button", { name: "Änderung abbrechen" }).click();
      const region = dialog.getByRole("region", { name: "Ergebnis der Rechteänderung" });
      await region.getByText("2 von 95 bestätigt · Seite 1 von 3").waitFor();
      const calls = await page.evaluate(() => window.__PERMISSION_CALLS__);
      assert.equal(calls.length, 2);
      assert.equal(calls[0].request.recursive, true);
      assert.deepEqual(calls[0].request.names, ["Bilder"]);
      assert.equal(calls[1].requestId, calls[0].request.requestId);
      assert.equal(await region.locator("li").count(), 40);
      assert.equal(await recursive.isChecked(), true);
      await region.getByRole("button", { name: "Weiter", exact: true }).click();
      await region.getByText(/Seite 2 von 3/).waitFor();
      await region.getByRole("button", { name: "Weiter", exact: true }).click();
      await region.getByText(/Seite 3 von 3/).waitFor();
      assert.equal(await region.locator("li").count(), 15);
      assert.equal(await region.getByRole("button", { name: "Zurück", exact: true }).evaluate(element => document.activeElement === element), true);
      assert.equal(await dialog.evaluate(element => element.scrollWidth <= element.clientWidth), true);
      if (process.env.DOPPELDECKER_PERMISSION_SCREENSHOTS) await page.screenshot({ path: `${process.env.DOPPELDECKER_PERMISSION_SCREENSHOTS}/${theme}.png` });
    });
  }

  for (const theme of ["light", "dark"]) {
    await runCase(`Informationen ändern lokale Eigentümer und erhalten beide Entwürfe · ${theme}`, "?keyboard=1", async page => {
      await page.setViewportSize({ width: 390, height: 760 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js"); setTheme(theme);
        const original = window.__TAURI__.core.invoke;
        window.__OWNER_CALLS__ = [];
        window.__TAURI__.core.invoke = async (command, args) => {
          if (command !== "change_file_ownership") return original(command, args);
          window.__OWNER_CALLS__.push(args.request);
          if (args.request.owner === "unknown") throw new Error("Lokaler Benutzer nicht gefunden (invalid_input)");
          return args.request.names.map(name => ({ name, owner: "lokal", group: "staff", uid: 501, gid: 20,
            specialBitsChanged: true, changed: !window.__OWNER_UNCLEAR__, code: window.__OWNER_UNCLEAR__ ? "outcome_unclear" : null,
            message: window.__OWNER_UNCLEAR__ ? "Abschluss <img src=x> unklar" : "Eigentümer/Gruppe bestätigt; das System hat Sonderbits geändert" }));
        };
      }, theme);
      const pane = page.getByRole("listbox", { name: /Linke Dateiliste/ });
      await pane.getByRole("option", { name: /Beta.txt/ }).click();
      await page.keyboard.press("Meta+I");
      const dialog = page.getByRole("dialog", { name: "Informationen · Beta.txt" });
      await dialog.locator(".file-info-permissions summary").click();
      await dialog.getByLabel("Unix-Rechte (oktal)").fill("632");
      await dialog.locator(".file-info-owner summary").click();
      await dialog.getByRole("button", { name: "Eigentümer/Gruppe anwenden" }).click();
      await dialog.getByRole("status").getByText(/Mindestens Eigentümer oder Gruppe/).waitFor();
      assert.deepEqual(await page.evaluate(() => window.__OWNER_CALLS__), []);
      await dialog.getByLabel("Eigentümer (Name oder UID)").fill("lokal");
      await dialog.getByLabel("Gruppe (Name oder GID)").fill("staff");
      await dialog.getByRole("button", { name: "Eigentümer/Gruppe anwenden" }).click();
      const report = dialog.getByRole("region", { name: "Ergebnis der Eigentümeränderung" });
      await report.getByText(/Sonderbits geändert/).waitFor();
      const calls = await page.evaluate(() => window.__OWNER_CALLS__);
      assert.deepEqual(calls, [{ requestId: calls[0].requestId, provider: "local", root: "/mock/quelle", names: ["Beta.txt"], owner: "lokal", group: "staff" }]);
      assert.match(calls[0].requestId, /^[a-f0-9-]{36}$/);
      assert.match(await dialog.getByText("Eigentümer", { exact: true }).locator("..").innerText(), /lokal/);
      assert.match(await dialog.getByText("Gruppe", { exact: true }).locator("..").innerText(), /staff/);
      assert.equal(await dialog.getByLabel("Unix-Rechte (oktal)").inputValue(), "632");
      assert.equal(await dialog.getByLabel("Gruppe (Name oder GID)").inputValue(), "staff");
      assert.equal(await pane.getByRole("option", { name: /Beta.txt/ }).getAttribute("aria-selected"), "true");
      await dialog.getByLabel("Eigentümer (Name oder UID)").fill("unknown");
      await dialog.getByRole("button", { name: "Eigentümer/Gruppe anwenden" }).click();
      await dialog.getByRole("status").getByText(/Lokaler Benutzer nicht gefunden/).waitFor();
      assert.equal(await dialog.getByLabel("Eigentümer (Name oder UID)").inputValue(), "unknown");
      assert.equal(await dialog.getByRole("button", { name: "Eigentümer/Gruppe anwenden" }).isEnabled(), true);
      await dialog.getByLabel("Eigentümer (Name oder UID)").fill("501");
      await page.evaluate(() => { window.__OWNER_UNCLEAR__ = true; });
      await dialog.getByRole("button", { name: "Eigentümer/Gruppe anwenden" }).click();
      await report.getByText(/outcome_unclear/).waitFor();
      await page.waitForFunction(() => !document.querySelector("[data-file-info-ownership]").disabled);
      assert.match(await dialog.getByText("Eigentümer", { exact: true }).locator("..").innerText(), /—/);
      assert.match(await dialog.getByText("Gruppe", { exact: true }).locator("..").innerText(), /—/);
      assert.equal(await report.locator("img").count(), 0);
      assert.equal(await dialog.evaluate(element => element.scrollWidth <= element.clientWidth), true);
      if (process.env.DOPPELDECKER_OWNER_SCREENSHOTS) await page.screenshot({ path: `${process.env.DOPPELDECKER_OWNER_SCREENSHOTS}/${theme}.png`, animations: "disabled" });
    });
  }

  await runCase("Informationen Eigentümerabbruch gehört zur aktiven Anfrage", "?keyboard=1", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__OWNER_CANCEL_CALLS__ = [];
      window.__TAURI__.core.invoke = (command, args) => {
        if (command === "change_file_ownership") {
          window.__OWNER_PENDING__ = args.request;
          return new Promise(resolve => { window.__OWNER_FINISH__ = resolve; });
        }
        if (command === "cancel_file_property_edit") {
          window.__OWNER_CANCEL_CALLS__.push(args);
          window.__OWNER_FINISH__([{ name: "Alpha.txt", changed: false, code: "interrupted", message: "Vor der Änderung abgebrochen" }]);
          return Promise.resolve();
        }
        return original(command, args);
      };
    });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).getByRole("option", { name: /Alpha.txt/ }).click();
    await page.keyboard.press("Meta+I");
    const dialog = page.getByRole("dialog", { name: "Informationen · Alpha.txt" });
    await dialog.locator(".file-info-owner summary").click();
    await dialog.getByLabel("Gruppe (Name oder GID)").fill("20");
    await dialog.getByRole("button", { name: "Eigentümer/Gruppe anwenden" }).click();
    await dialog.getByRole("button", { name: "Änderung abbrechen" }).click();
    await dialog.getByRole("region", { name: "Ergebnis der Eigentümeränderung" }).getByText(/Vor der Änderung abgebrochen/).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__OWNER_CANCEL_CALLS__), [{ requestId: await page.evaluate(() => window.__OWNER_PENDING__.requestId) }]);
    assert.equal(await dialog.getByRole("button", { name: "Eigentümer/Gruppe anwenden" }).isEnabled(), true);
  });

  await runCase("Informationen erhalten Rechteentwurf und unklaren Modus bei Größenereignissen", "?keyboard=1", async page => {
    await page.evaluate(async () => {
      const { createFileInformationUi } = await import("/file-information.js");
      const entry = { name: "Baum", is_dir: true, is_file: false, is_symlink: false,
        mode: 0o755, size: 0, mtime: 0, safely_decoded: true };
      const pane = { provider: "local", path: "/mock/quelle", profileId: undefined,
        profileRevision: undefined, sessionGeneration: undefined };
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = (command, args) => command === "change_file_permissions"
        ? Promise.resolve([{ name: "Baum", changed: false, code: "outcome_unclear", message: "Prüfen" }])
        : original(command, args);
      window.__INFO_SIZE_UI__ = createFileInformationUi({ panes: { left: pane }, selectedEntries: () => [entry] });
      window.__INFO_SIZE_UI__.open("left");
      window.__INFO_SIZE_EVENT__ = () => window.__INFO_SIZE_UI__.onDirectorySizeChange("left", {
        ...entry, mode: 0o777, directorySize: { status: "complete", bytes: 42, files: 1, directories: 0, limitations: [] },
      });
    });
    const dialog = page.getByRole("dialog", { name: "Informationen · Baum" });
    await dialog.locator(".file-info-permissions summary").click();
    const input = dialog.getByLabel("Unix-Rechte (oktal)");
    await input.fill("600");
    await dialog.getByRole("button", { name: "Rechte anwenden" }).click();
    await dialog.getByRole("region", { name: "Ergebnis der Rechteänderung" }).getByText(/outcome_unclear/).waitFor();
    await input.fill("632");
    await input.evaluate(element => element.setSelectionRange(1, 2));
    await page.evaluate(() => window.__INFO_SIZE_EVENT__());
    assert.equal(await input.inputValue(), "632");
    assert.deepEqual(await input.evaluate(element => [element.selectionStart, element.selectionEnd, document.activeElement === element]), [1, 2, true]);
    assert.equal(await dialog.locator(".file-info-permissions").evaluate(element => element.open), true);
    assert.match(await dialog.getByText("Berechtigungen", { exact: true }).locator("..").innerText(), /—/);
    assert.doesNotMatch(await dialog.getByText("Berechtigungen", { exact: true }).locator("..").innerText(), /777/);
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
    assert.equal(await pane.getByRole("listbox").getByRole("option").count(), 0);
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

  for (const [provider, revision, theme] of [["sftp", 11, "light"], ["ftps", 21, "dark"]]) {
    await runCase(`Informationen ändern entfernte Rechte mit Profilbindung · ${provider}`, "?remoteMissingMode=1", async page => {
      await page.setViewportSize({ width: 390, height: 650 });
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme); }, theme);
      await page.locator(`#connection-list [data-protocol="${provider}"][data-id="mock_${provider}"]`).click();
      const folder = page.locator('.pane[data-side="left"] .row.dir', { hasText: "remote" });
      await folder.click({ button: "right" });
      await page.locator("#context-menu").getByRole("menuitem", { name: /Informationen/ }).click();
      const dialog = page.getByRole("dialog", { name: /Informationen · remote/ });
      await dialog.locator(".file-info-permissions summary").click();
      assert.match(await dialog.locator(".file-info-permissions").innerText(), /Best Effort/);
      assert.equal(await dialog.locator(".file-info-owner").count(), 0);
      await page.evaluate(() => {
        const original = window.__TAURI__.core.invoke;
        window.__REMOTE_MODE_REQUESTS__ = [];
        window.__TAURI__.core.invoke = async (command, args) => {
          if (command !== "change_file_permissions") return original(command, args);
          window.__REMOTE_MODE_REQUESTS__.push(args.request);
          return [{ name: args.request.names[0], changed: false, code: "outcome_unclear", message: "Antwort verloren; Bestand vor neuem Versuch prüfen" },
            { name: `${args.request.names[0]}/file`, changed: false, code: "interrupted", message: "Nicht begonnen" }];
        };
      });
      await dialog.getByLabel("Unix-Rechte (oktal)").fill("750");
      await dialog.getByLabel("Auch auf alle enthaltenen Dateien und Ordner anwenden").check();
      if (process.env.DOPPELDECKER_REMOTE_MODE_SCREENSHOTS) await page.screenshot({ path: `${process.env.DOPPELDECKER_REMOTE_MODE_SCREENSHOTS}/${provider}-${theme}.png`, animations: "disabled" });
      await dialog.getByRole("button", { name: "Rechte anwenden" }).click();
      await dialog.getByRole("region", { name: "Ergebnis der Rechteänderung" }).getByText(/Antwort verloren/).waitFor();
      const [request] = await page.evaluate(() => window.__REMOTE_MODE_REQUESTS__);
      assert.deepEqual({ ...request, requestId: "bound" }, { requestId: "bound", provider, root: "/", names: ["remote"], mode: "750", recursive: true, profileId: `mock_${provider}`, profileRevision: revision });
      assert.match(await dialog.getByText("Berechtigungen", { exact: true }).locator("..").innerText(), /—/);
      assert.equal(await dialog.evaluate(element => element.scrollWidth <= element.clientWidth), true);
      assert.equal(await folder.getAttribute("aria-selected"), "true");
    });
  }

  await runCase("Informationen übernehmen die Editorfreigabe ausschließlich aus Backendfähigkeiten", "?keyboard=1", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = (command, args) => command === "get_file_property_capabilities"
        ? Promise.resolve({ permissions: false, recursivePermissions: false, ownership: false, limitations: ["Gezielt nicht verfügbar"] }) : original(command, args);
    });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).getByRole("option", { name: /Alpha.txt/ }).click();
    await page.keyboard.press("Meta+I");
    const dialog = page.getByRole("dialog", { name: "Informationen · Alpha.txt" });
    await dialog.getByText("Gezielt nicht verfügbar").waitFor();
    assert.equal(await dialog.locator(".file-info-editor").count(), 0);
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

  await runCase("Schnellfilter findet Unicode-Normalformen und erhält Aktionsnamen", "?unicodeFilter=1", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    const filter = pane.getByRole("searchbox", { name: "Name oder Endung links filtern" });
    const sourceRoot = await pane.locator(".path").inputValue();
    await filter.fill("CAFÉ");
    assert.equal(await list.getByRole("option").count(), 1);
    await list.getByRole("option").click();
    await list.focus();
    await page.keyboard.press("F5");
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation.items[0]), `${sourceRoot}/Cafe\u0301.txt`);
    await filter.fill("U\u0308BER");
    assert.equal(await list.getByRole("option").count(), 1);
    assert.match(await list.getByRole("option").innerText(), /Überblick/);
  });

  await runCase("Schnellfilter bindet Auswahl und Dateiaktionen an sichtbare Einträge", "?mixedSort=1", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    const filter = pane.getByRole("searchbox", { name: "Name oder Endung links filtern" });
    const kind = pane.getByRole("combobox", { name: "Dateityp links filtern" });
    const initialCalls = await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length);
    await list.focus();
    await page.keyboard.press("Control+A");
    await filter.fill(".TXT");
    assert.equal(await list.getByRole("option").count(), 1);
    assert.match(await list.getByRole("option", { selected: true }).innerText(), /Mitte.txt/);
    assert.match(await pane.locator(".count").innerText(), /1 von 3 Einträgen/);
    assert.equal(await page.evaluate(() => window.__MOCK_SCAN_CALLS__().length), initialCalls);
    await list.focus();
    await page.keyboard.press("F5");
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    const operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation);
    assert.equal(operation.items.length, 1);
    assert.match(operation.items[0], /\/Mitte\.txt$/);
    await kind.selectOption("folders");
    await list.getByText("Keine Treffer im geladenen Ordner").waitFor();
    assert.equal(await list.getByRole("option").count(), 0);
    await list.focus();
    await page.keyboard.press("F5");
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 1);
    await filter.fill("");
    assert.equal(await list.getByRole("option").count(), 2);
    await pane.locator('.col-name').click();
    assert.equal(await list.getByRole("option").count(), 2);
    if (process.env.DOPPELDECKER_UI_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_UI_EVIDENCE_DIR}/pane-filter-light-review.png` });
    await filter.press("Escape");
    assert.equal(await kind.inputValue(), "all");
    assert.equal(await list.getByRole("option").count(), 3);
    assert.equal(await list.evaluate(element => document.activeElement === element), true);
    await filter.fill("kein Treffer");
    await pane.getByRole("button", { name: "Linken Filter leeren" }).click();
    assert.equal(await list.getByRole("option").count(), 3);
    await kind.selectOption("folders");
    await pane.locator(".path").fill("/mock/anderer-ordner");
    await pane.locator(".path").press("Enter");
    await page.waitForFunction(() => document.querySelector('.pane[data-side="left"] .pane-filter-kind').value === "all");
    assert.equal(await filter.inputValue(), "");
  });

  await runCase("Schnellfilter erhält die sichere Objektidentität bei gleich aussehenden Namen", "?unsafe=1", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.locator(".row:not(.unsafe-name)").click();
    await pane.getByRole("searchbox").fill(".txt");
    await pane.locator(".col-size").click();
    await pane.locator(".col-size").click();
    const selected = list.getByRole("option", { selected: true });
    assert.equal(await selected.count(), 1);
    assert.equal(await selected.locator(".unsafe-name-badge").count(), 0);
    await list.focus();
    await page.keyboard.press("F5");
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].operation.items.length), 1);
  });

  await runCase("Schnellfilter bleibt bei großen Listen virtualisiert und je Pane getrennt", "?entries=5000", async page => {
    const pane = page.locator('.pane[data-side="left"]');
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    const filter = pane.getByRole("searchbox", { name: "Name oder Endung links filtern" });
    await filter.fill("Datei-0");
    assert.equal(await list.getAttribute("data-virtualized"), "true");
    assert.ok(await list.locator(".row").count() < 100);
    await list.focus();
    await page.keyboard.press("End");
    await filter.fill("05000");
    assert.equal(await list.getAttribute("data-virtualized"), "false");
    assert.equal(await list.getByRole("option").count(), 1);
    assert.match(await list.getByRole("option").innerText(), /Datei-05000.txt/);
    const active = await list.getAttribute("aria-activedescendant");
    assert.equal(await page.locator(`#${active}`).count(), 1);
    assert.equal(await page.getByRole("searchbox", { name: "Name oder Endung rechts filtern" }).inputValue(), "");
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "theme_dark"));
    assert.equal(await page.locator("html").getAttribute("data-resolved-theme"), "dark");
    await page.setViewportSize({ width: 1040, height: 700 });
    const controlRects = await pane.locator(".pane-filter input, .pane-filter select, .pane-filter button").evaluateAll(elements => elements.map(el => {
      const r = el.getBoundingClientRect(); return { width: r.width, height: r.height, right: r.right };
    }));
    assert.ok(controlRects.every(r => r.width >= 28 && r.height >= 28));
    assert.ok(controlRects.at(-1).right <= (await pane.boundingBox()).x + (await pane.boundingBox()).width);
    if (process.env.DOPPELDECKER_UI_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_UI_EVIDENCE_DIR}/pane-filter-dark-review.png` });
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

  await runCase("Linux-Menüs folgen Hell, Dunkel und dem lebenden Systemmodus", "", async page => {
    await page.emulateMedia({ colorScheme: "dark" });
    await page.evaluate(() => {
      document.documentElement.dataset.platform = "linux";
      window.__NATIVE_THEMES__ = [];
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "sync_native_theme") window.__NATIVE_THEMES__.push(args);
        return original(command, args);
      };
    });
    for (const [preference, dark] of [["light", false], ["dark", true], ["auto", true]]) {
      await page.evaluate(value => window.__MOCK_EMIT__("menu", `theme_${value}`), preference);
      await page.waitForFunction(({ preference, dark }) => {
        const last = window.__NATIVE_THEMES__.at(-1);
        return last?.preference === preference && last.dark === dark;
      }, { preference, dark });
    }
    await page.emulateMedia({ colorScheme: "light" });
    await page.waitForFunction(() => window.__NATIVE_THEMES__.at(-1)?.preference === "auto"
      && window.__NATIVE_THEMES__.at(-1)?.dark === false);
    assert.equal(await page.locator("html").getAttribute("data-resolved-theme"), "light");
    await page.evaluate(async () => {
      document.documentElement.dataset.platform = "macos";
      window.__NATIVE_THEMES__ = [];
      const { setTheme } = await import("/theme.js");
      setTheme("dark", false);
    });
    assert.equal(await page.evaluate(() => window.__NATIVE_THEMES__.length), 0);
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
    await page.getByRole("dialog", { name: "Sync ausführen" })
      .getByRole("button", { name: "Vorbereitung verwerfen" }).click();
    await page.evaluate(() => new Promise(resolve => setTimeout(resolve, 25)));
    assert.equal(await page.evaluate(() => window.__REAL_PREPARATIONS__), 1);
    assert.equal(await page.getByRole("dialog", { name: "Sync ausführen" }).count(), 0);
  });

  await runCase("Sync serialisiert alte Remoteantwort und Discard vor neuer Vorbereitung", "?remoteFile=1", async page => {
    await page.locator('.pane[data-side="right"] .listing').click();
    await page.locator('#connection-list [data-protocol="ftps"][data-id="mock_ftps"]').click();
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      let held = false;
      window.__REMOTE_PREPARATION_ORDER__ = [];
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "prepare_content_sync" || command === "discard_content_sync") {
          window.__REMOTE_PREPARATION_ORDER__.push(command);
        }
        const result = await invoke(command, args);
        if (command === "prepare_content_sync" && !held) {
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
    await page.waitForFunction(() => window.__REMOTE_PREPARATION_ORDER__.filter(name => name === "prepare_content_sync").length === 2);
    await page.locator("#real-mode-banner").waitFor({ state: "visible" });
    assert.deepEqual(await page.evaluate(() => window.__REMOTE_PREPARATION_ORDER__), [
      "prepare_content_sync", "discard_content_sync", "prepare_content_sync",
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
    const dialog = page.getByRole("dialog", { name: "Sync ausführen" });
    await dialog.waitFor();
    await page.clock.runFor(200);
    await page.clock.resume();
    await dialog.getByRole("button", { name: "Sync starten" }).click();
    await page.waitForFunction(() => document.documentElement.dataset.realConfirmation === "true", null, { timeout: 3000 });
  });

  await runCase("Sync-Wirkungsdialog ohne Phrase hält Fokus und verwirft mit Escape", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    const trigger = page.getByRole("button", { name: "Realmodus" });
    await trigger.click();
    const dialog = page.getByRole("dialog", { name: "Sync ausführen" });
    await dialog.waitFor();
    assert.equal(await dialog.locator("input").count(), 0);
    assert.match(await dialog.innerText(), /Quelle.*Ziel.*Wirkung/s);
    assert.equal(await dialog.getByRole("button", { name: "Sync starten" }).isEnabled(), true);
    await page.keyboard.press("Escape");
    assert.equal(await page.evaluate(() => document.activeElement?.id), await trigger.getAttribute("id"));
    assert.equal(await page.locator("html").getAttribute("data-real-confirmation"), null);
    await trigger.click();
    await dialog.getByRole("button", { name: "Sync starten" }).click();
    await page.waitForFunction(() => document.documentElement.dataset.realConfirmation === "true");
  });

  for (const theme of ["light", "dark"]) {
    await runCase(`Sync-Bereichswarnung bleibt bei kleinem Fenster erreichbar: ${theme}`, "", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js"); setTheme(theme, false);
        const invoke = window.__TAURI__.core.invoke;
        window.__TAURI__.core.invoke = async (command, args) => {
          const result = await invoke(command, args);
          if (command === "prepare_real_sync") result.scope_warnings = ["HOME-Ziel: Der interne Sicherungsbereich .doppeldecker-quarantine bleibt auf beiden Seiten vom Sync ausgeschlossen."];
          return result;
        };
      }, theme);
      await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
      await page.getByRole("button", { name: "Realmodus" }).click();
      const dialog = page.getByRole("dialog", { name: "Sync ausführen" });
      const warning = dialog.getByText(/HOME-Ziel:/);
      await warning.scrollIntoViewIfNeeded();
      assert.equal(await warning.isVisible(), true);
      const start = dialog.getByRole("button", { name: "Sync starten" });
      await start.scrollIntoViewIfNeeded();
      const bounds = await start.boundingBox();
      assert.ok(bounds && bounds.y >= 0 && bounds.y + bounds.height <= 480);
      await page.keyboard.press("Escape");
    });
  }

  await runCase("ContentSync Abbruch bilanziert tatsächliche Runtime-Dateizeitauslassung", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("sync-done", {
      dry_run: false, cancelled: true, execution_class: "best_effort", files_copied: 1,
      files_deleted: 0, metadata_updated: 0, bytes_copied: 12, quarantine: null,
      metadata_report: { profile: "Portable", classes: [{ class: "Mtime", transferred: 0, omitted: 1 }],
        issues: [{ path: "bericht.txt", class: "Mtime", kind: "Unsupported", reason: "Server hat die Dateizeit beim Versuch nicht exakt übernommen" }] },
    }));
    await page.locator("#dock-summary").filter({ hasText: "Best-Effort-Sync abgebrochen" }).waitFor();
    const text = await page.locator("#dock-summary").innerText();
    assert.match(text, /1 Dateizeit ausgelassen/); assert.doesNotMatch(text, /Rechte ausgelassen|Sync ausgeführt/);
  });
  await runCase("ContentSync SFTP-Navigation meldet gewöhnlichen Sync verfügbar", "", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.locator("#dock-summary").filter({ hasText: "Gewöhnlicher Dateisync ist verfügbar" }).waitFor();
    const text = await page.locator("#dock-summary").innerText();
    assert.match(text, /Bidirektionaler Dateisync ist als Best Effort ebenfalls verfügbar/);
    assert.match(text, /Strikter Sync mit atomarer Rootbindung.*strikter bidirektionaler Sync.*Providerquarantäne/s);
    assert.doesNotMatch(text, /Sync und Providerquarantäne bleiben gesperrt/);
  });
  for (const effect of ["permanent", "quarantine", "source_best_effort", "zero"]) {
    await runCase(`ContentSync Abschluss meldet tatsächliche Absicherung und Entfernung: ${effect}`, "", async page => {
      if (effect === "source_best_effort") await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
      await page.evaluate(effect => {
        window.__MOCK_EMIT__("sync-done", {
          dry_run: false, cancelled: false, files_copied: 1, bytes_copied: 12, metadata_updated: 0,
          files_deleted: ["permanent", "quarantine"].includes(effect) ? 1 : 0,
          quarantine: effect === "quarantine" ? "/mock/.doppeldecker-quarantine/run-1" : null,
          execution_class: ["permanent", "source_best_effort"].includes(effect) ? "best_effort" : undefined,
          local_execution: ["quarantine", "source_best_effort"].includes(effect)
            ? { new_file_commit: "assured", file_flush: true, directory_flush: true, irreversible_replacements: 0 } : null,
        });
      }, effect);
      await page.locator("#dock-summary").filter({ hasText: "Sync ausgeführt" }).waitFor();
      const text = await page.locator("#dock-summary").innerText();
      if (effect === "permanent") { assert.match(text, /1 dauerhaft entfernt/); assert.doesNotMatch(text, /in Quarantäne|Wiederherstellung/); }
      if (effect === "quarantine") { assert.match(text, /Abgesicherter Sync ausgeführt.*1 in Quarantäne.*Wiederherstellung:.*run-1/s); assert.doesNotMatch(text, /dauerhaft entfernt/); }
      if (effect === "source_best_effort") { assert.match(text, /Best-Effort-Sync ausgeführt/); assert.doesNotMatch(text, /Abgesicherter Sync/); }
      if (effect === "zero") { assert.match(text, /0 entfernt/); assert.doesNotMatch(text, /in Quarantäne|dauerhaft entfernt/); }
    });
  }
  for (const code of ["sandbox_violation", "permission_denied", "connection_lost"]) {
    for (const trigger of ["preview", "run"]) {
      await runCase(`ContentSync Preparefehler erlaubt erneute Vorschau und Start: ${code} ${trigger}`, `?contentPrepareFails=${code}`, async page => {
        await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
        await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
        const preview = page.locator("#sync-preview-remote"); await preview.waitFor({ state: "visible" });
        await page.locator(trigger === "preview" ? "#sync-preview-remote" : "#btn-sync").click();
        await page.locator("#dock-summary").filter({ hasText: "vorübergehende Testursache" }).waitFor();
        assert.equal(await page.locator("#sync-view").isVisible(), true);
        assert.equal(await preview.isVisible(), true);
        assert.equal(await preview.isEnabled(), true);
        assert.equal(await page.locator("#btn-sync").isEnabled(), true);
        assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
        await page.evaluate(() => window.__MOCK_REPAIR_CONTENT_PREPARATION__());
        await preview.click();
        await page.locator("#sync-summary .pill").first().waitFor();
        assert.equal(await page.locator("#sync-hint").isVisible(), false);
        await page.locator("#btn-sync").click();
        await page.getByRole("dialog", { name: "Dateisync ausführen" }).getByRole("button", { name: "Sync bestätigen und starten" }).click();
        await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
        const calls = await page.evaluate(() => window.__MOCK_CONTENT_SYNC_STATE__().calls);
        assert.equal(calls.filter(call => call.command === "prepare_content_sync").length, 2);
        assert.equal(calls.filter(call => call.command === "enqueue_content_sync").length, 1);
      });
    }
  }
  for (const mode of ["Update", "Mirror"]) {
    await runCase(`ContentSync lokales Ziel erhält Quarantäne: ${mode}`, "?contentQuarantine=1", async page => {
      await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
      await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
      await page.locator(mode === "Update" ? "#sync-update" : "#sync-mirror").check();
      await page.locator("#btn-sync").click();
      const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" }); await dialog.waitFor();
      assert.match(await dialog.innerText(), /Doppeldecker-Quarantäne.*gesichert.*wiederherstellbar.*run-mock-geprüft/s);
      assert.doesNotMatch(await dialog.innerText(), /dauerhaft ersetzt|dauerhaft entfernt|Keine Doppeldecker-Quarantäne|keine automatische Wiederherstellung/);
      assert.equal(await dialog.locator("input").count(), 0);
      await dialog.getByRole("button", { name: "Änderungen mit Quarantäne bestätigen" }).click();
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    });
  }
  await runCase("ContentSync zeigt benannte Datenströme in Plan und Bestätigung", "", async page => {
    await page.locator('#connection-list [data-protocol="smb"][data-id="mock_smb"]').click();
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command === "prepare_content_sync") {
          result.plan.actions = [
            { type: "CopyFile", path: "Datei.txt", metadata: { named_streams: true } },
            { type: "CreateDir", path: "Ordner", metadata: { named_streams: false } },
          ];
          result.plan.comparison = null;
          result.plan.metadata_report = { profile: "NativeBestEffort", classes: [{ class: "NamedStreams", transferred: 1, omitted: 1 }], issues: [{ path: "Ordner", class: "NamedStreams", reason: "SMB-Verzeichnisströme werden hier nicht beobachtet" }] };
        }
        return result;
      };
      window.__MOCK_EMIT__("menu", "sync_mode");
    });
    await page.locator('#sync-metadata-group [value="NativeBestEffort"]').check();
    await page.locator("#btn-sync").click();
    const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /Ordner.*Benannte Datenströme.*SMB-Verzeichnisströme werden hier nicht beobachtet/s);
    assert.match(await page.locator("#sync-actions").innerText(), /Benannte Datenströme/);
    assert.doesNotMatch(await dialog.innerText(), /NamedStreams|Metadatenmerkmal ·/);
    await page.keyboard.press("Escape");
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });
  await runCase("SMB-Wirkung zeigt Profilroot und ausgewählten Unterpfad", "", async page => {
    const displayed = await page.evaluate(async () => {
      const { identityDisplay, confirmContentSync } = await import("/content-sync-ui.js");
      const identity = (canonical, virtual, host = "smb.invalid", port = 445) => ({ provider: "Smb", identity: {
        server: { host, port, share: "Team", canonical_root: canonical }, virtual_root: virtual,
      } });
      const cases = [["/", "/"], ["/", "/eingang"], ["/team", "/"], ["/team", "/eingang"],
        ["/team", "/eingang", "smb.invalid", 1445], ["/team", "/eingang", "::1", 1445]];
      const preview = {
        source_provider: "Smb", target_provider: "Smb",
        source_identity: identity("/team", "/eingang"), target_identity: identity("/archiv", "/2026"),
        plan: { actions: [], copy_count: 0, update_count: 0, delete_count: 0, bytes_to_transfer: 0 },
        limitations: ["SMB-Quelle: Zugriffe auf benannte Datenströme halten die gewählte Wurzel und ihre Unterordner. Darüberliegende Ordner werden frisch geprüft; ihre serverseitige Pfadauflösung bleibt Best Effort."],
      };
      void confirmContentSync(preview);
      return cases.map(parts => identityDisplay(identity(...parts)));
    });
    assert.deepEqual(displayed, [
      "smb://smb.invalid/Team/", "smb://smb.invalid/Team/eingang",
      "smb://smb.invalid/Team/team", "smb://smb.invalid/Team/team/eingang",
      "smb://smb.invalid:1445/Team/team/eingang", "smb://[::1]:1445/Team/team/eingang",
    ]);
    const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /smb:\/\/smb.invalid\/Team\/team\/eingang/);
    assert.match(await dialog.innerText(), /smb:\/\/smb.invalid\/Team\/archiv\/2026/);
    assert.match(await dialog.innerText(), /Darüberliegende Ordner.*Best Effort/);
    await page.keyboard.press("Escape");
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });
  await runCase("Forward-Bidir benennt ausgelassene Datenströme im Wirkungsdetail", "", async page => {
    const preview = forwardConfirmationFixture();
    preview.effects = preview.effects.slice(-1);
    preview.effects[0].plan.metadata_report.issues = [{ path: "Datei.txt", class: "NamedStreams", reason: "Beobachtete Zielströme gehen beim Ersetzen verloren" }];
    await page.evaluate(async preview => {
      const { confirmForwardBidirectional } = await import("/forward-bidirectional-ui.js");
      void confirmForwardBidirectional(preview, { pairName: "SMB-Paar" });
    }, preview);
    const dialog = page.getByRole("dialog");
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /Datei.txt.*Benannte Datenströme.*Beobachtete Zielströme gehen beim Ersetzen verloren/s);
    assert.doesNotMatch(await dialog.innerText(), /NamedStreams/);
    await page.keyboard.press("Escape");
  });
  await runCase("ContentSync große Änderungs- und Metadatenlisten bleiben begrenzt", "", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command === "prepare_content_sync") {
          result.plan.actions = Array.from({ length: 200 }, (_, index) => ({ type: "UpdateFile", path: `Datei-${index}.txt` }));
          result.plan.metadata_report = { profile: "Portable", classes: [{ class: "Mtime", transferred: 0, omitted: 200 }], issues: Array.from({ length: 200 }, (_, index) => ({ path: `Datei-${index}.txt`, class: "Mtime", reason: "Ziel stellt diese Dateizeit nicht exakt dar" })) };
        }
        return result;
      };
      window.__MOCK_EMIT__("menu", "sync_mode");
    });
    await page.locator("#btn-sync").click();
    const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" }); await dialog.waitFor();
    assert.equal(await dialog.locator("li").count(), 100);
    assert.match(await page.locator("#sync-metadata-note-title").innerText(), /Dateizeiten.*ausgelassen/);
    assert.match(await dialog.innerText(), /150 weitere Änderungen.*Planliste/s);
    assert.match(await dialog.innerText(), /150 weitere Metadatenhinweise.*Planliste/s);
    assert.match(await dialog.innerText(), /Dateiversion ersetzen.*Dateizeit/s);
    assert.doesNotMatch(await dialog.innerText(), /UpdateFile|NormalMode|Mtime/);
    await page.keyboard.press("Escape");
  });
  await runCase("ContentSync Vorbereitung kann mit Escape abgebrochen werden", "?contentPrepareDelay=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#btn-sync").click();
    await page.locator("#btn-cancel").waitFor({ state: "visible" });
    await page.keyboard.press("Escape");
    await page.locator("#dock-summary").filter({ hasText: "abgebrochen" }).waitFor();
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
    assert.equal(await page.getByRole("dialog", { name: "Dateisync ausführen" }).count(), 0);
  });
  for (const large of [false, true]) {
    await runCase(`ContentSync Mirror bestätigt dauerhafte Entfernung: ${large ? "groß mit Phrase" : "klein ohne Phrase"}`, large ? "?contentLargeRemove=1" : "", async page => {
      await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
      await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
      await page.locator("#sync-mirror").check();
      assert.match(await page.locator("#sync-effects-group").innerText(), /Zusätzliche Zieldateien entfernen/);
      if (!large) {
        await page.locator('#sync-metadata-group [value="NativeBestEffort"]').check();
        assert.equal(await page.locator("#sync-mirror").isChecked(), true);
      }
      await page.locator("#btn-sync").click();
      const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" });
      assert.match(await dialog.innerText(), /Einträge.*dauerhaft entfernt.*Keine Doppeldecker-Quarantäne/s);
      const start = dialog.getByRole("button", { name: "Dauerhafte Änderungen bestätigen" });
      if (large) {
        const phrase = dialog.getByRole("textbox", { name: "Exakten Entfernungstext eingeben" });
        assert.equal(await start.isEnabled(), false);
        await phrase.fill("FALSCH"); assert.equal(await start.isEnabled(), false);
        await phrase.fill("ENTFERNEN 1001 MOCK");
      } else assert.equal(await dialog.locator("input").count(), 0);
      await start.click();
      await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
      const args = await page.evaluate(() => window.__MOCK_CONTENT_SYNC_STATE__().calls.find(call => call.command === "enqueue_content_sync").args);
      assert.equal(args.removalPhrase, large ? "ENTFERNEN 1001 MOCK" : null);
    });
  }
  await runCase("ContentSync verspätete Einreihantwort erhält den echten Abschluss", "", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command === "enqueue_content_sync") {
          window.__MOCK_EMIT__("sync-done", { operationId: result, attempt: 0, dry_run: false,
            files_copied: 1, files_deleted: 0, metadata_updated: 0, bytes_copied: 12 });
          await new Promise(resolve => { window.__RELEASE_CONTENT_ENQUEUE__ = resolve; });
        }
        return result;
      };
      window.__MOCK_EMIT__("menu", "sync_mode");
    });
    await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Dateisync ausführen" }).getByRole("button", { name: "Sync bestätigen und starten" }).click();
    await page.waitForFunction(() => typeof window.__RELEASE_CONTENT_ENQUEUE__ === "function");
    await page.locator("#dock-summary").filter({ hasText: "Sync ausgeführt" }).waitFor();
    const completed = await page.locator("#dock-summary").innerText();
    await page.evaluate(async () => { window.__RELEASE_CONTENT_ENQUEUE__(); await new Promise(resolve => setTimeout(resolve, 0)); });
    assert.equal(await page.locator("#dock-summary").innerText(), completed);
  });
  for (const theme of ["light", "dark"]) {
    await runCase(`ContentSync frische Queue-Wirkung und Digest nach Reopen: ${theme}`, "?contentReopen=1", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme, false); }, theme);
      await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
      await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
      await page.locator("#btn-sync").click();
      await page.getByRole("dialog", { name: "Dateisync ausführen" }).getByRole("button", { name: "Sync bestätigen und starten" }).click();
      await page.locator("#btn-queue").click();
      await page.getByRole("button", { name: "Plan bestätigen" }).click();
      const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" });
      assert.match(await dialog.innerText(), /2 vorhandene Dateiversion.*dauerhaft ersetzt/s);
      assert.match(await dialog.innerText(), /frisch-geändert.txt/);
      assert.equal(await dialog.locator("input").count(), 0);
      const start = dialog.getByRole("button", { name: "Dauerhafte Änderungen bestätigen" });
      await start.scrollIntoViewIfNeeded();
      const bounds = await start.boundingBox();
      assert.ok(bounds && bounds.y >= 0 && bounds.y + bounds.height <= 480);
      await page.keyboard.press("Escape");
      assert.equal(await page.locator("#queue-popover").isVisible(), true);
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state), "AwaitingConfirmation");
      await page.getByRole("button", { name: "Plan bestätigen" }).click();
      await dialog.getByRole("button", { name: "Dauerhafte Änderungen bestätigen" }).click();
      await page.waitForFunction(() => window.__MOCK_CONTENT_SYNC_STATE__().calls.some(call => call.command === "approve_content_sync_queue_entry"));
      const approval = await page.evaluate(() => window.__MOCK_CONTENT_SYNC_STATE__().calls.find(call => call.command === "approve_content_sync_queue_entry").args);
      assert.deepEqual(approval.preparationDigest, Array(32).fill(9));
      assert.equal(approval.effectsConfirmed, true);
      assert.equal(await page.locator("#queue-popover").isVisible(), true);
    });
  }
  await runCase("ContentSync verspätete Vorbereitung nach Scopewechsel wird verworfen", "", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command === "prepare_content_sync") await new Promise(resolve => { window.__RELEASE_CONTENT__ = resolve; });
        return result;
      };
      window.__MOCK_EMIT__("menu", "sync_mode");
    });
    await page.locator("#btn-sync").click();
    await page.waitForFunction(() => typeof window.__RELEASE_CONTENT__ === "function");
    await page.locator("#sync-update").check();
    await page.evaluate(() => window.__RELEASE_CONTENT__());
    await page.waitForFunction(() => window.__MOCK_CONTENT_SYNC_STATE__().calls.some(call => call.command === "discard_content_sync"));
    assert.equal(await page.getByRole("dialog", { name: "Dateisync ausführen" }).count(), 0);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });
  for (const mode of ["normal", "vertauscht", "recovery-queue"]) {
    await runCase(`Forward-Bidir vollständige Wirkungsbilanz und Pfade: ${mode}`, "", async page => {
      const preview = forwardConfirmationFixture();
      if (mode === "recovery-queue") preview.recovery = { warning: "Vorheriger Lauf unterbrochen" };
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async ({ preview, mode }) => {
        const { confirmForwardBidirectional } = await import("/forward-bidirectional-ui.js");
        window.__FORWARD_CONFIRMATION_RESULT__ = undefined;
        confirmForwardBidirectional(preview, { pairName: "Großes Konfliktpaar", queued: mode === "recovery-queue",
          visibleRoles: mode === "vertauscht" ? { left: "right", right: "left" } : { left: "left", right: "right" },
        }).then(value => { window.__FORWARD_CONFIRMATION_RESULT__ = value; });
      }, { preview, mode });
      const dialog = page.getByRole("dialog");
      await dialog.waitFor();
      const text = await dialog.innerText();
      assert.match(text, /22 hinzufügen · 5 ersetzen · 11 Entfernungsaktionen/);
      assert.match(text, /3 Dateiversion\(en\) werden dauerhaft ersetzt/);
      assert.match(text, /7 Einträge werden dauerhaft entfernt/);
      assert.match(text, /2 alte Dateiversion\(en\) und 4 zusätzliche Einträge/);
      assert.match(text, /2 Metadatenhinweise/);
      const roots = await dialog.locator(".real-grid .real-card").allTextContents();
      assert.match(roots[0], mode === "vertauscht" ? /paar-rechts/ : /paar-links/);
      assert.match(roots[1], mode === "vertauscht" ? /paar-links/ : /paar-rechts/);
      const effectsSummary = dialog.getByText("Alle 24 Teilwirkungen", { exact: true });
      const actionsSummary = dialog.getByText("Alle 55 Planaktionen und Pfade", { exact: true });
      if (mode === "normal") {
        for (let attempt = 0; attempt < 6; attempt += 1) {
          if (await effectsSummary.evaluate(element => document.activeElement === element)) break;
          await page.keyboard.press("Tab");
        }
        assert.equal(await effectsSummary.evaluate(element => document.activeElement === element), true);
        await page.keyboard.press("Enter");
        await page.keyboard.press("Tab");
        assert.equal(await actionsSummary.evaluate(element => document.activeElement === element), true);
        await page.keyboard.press("Space");
        await page.keyboard.press("Tab");
        assert.equal(await dialog.getByRole("button", { name: "Abbrechen" }).evaluate(element => document.activeElement === element), true);
        await page.keyboard.press("Shift+Tab");
        assert.equal(await actionsSummary.evaluate(element => document.activeElement === element), true);
      } else {
        await effectsSummary.click();
        await actionsSummary.click();
      }
      assert.match(await dialog.innerText(), /Dateizeit.*Ziel unterstützt keine prüfbare Dateizeit/s);
      assert.match(await dialog.innerText(), /letzter-ordner\/rechte.txt.*ACL.*ACL-Modell ist nicht darstellbar <Details>/s);
      const paths = await dialog.innerText();
      const sourceSide = mode === "vertauscht" ? "Rechts" : mode === "recovery-queue" ? "Paarseite links" : "Links";
      const targetSide = mode === "vertauscht" ? "Links" : mode === "recovery-queue" ? "Paarseite rechts" : "Rechts";
      assert.ok(paths.includes(`${sourceSide}: konflikt-53/ä.txt → ${targetSide}: konflikt-53/ä-Kopie.txt`));
      assert.match(paths, /dauerhaft-entfernen.txt/);
      assert.doesNotMatch(paths, /Nicht darstellbarer Pfad/);
      if (process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/forward-complete-${mode}.png` });
      const start = dialog.getByRole("button", { name: "Wirkung beider Seiten bestätigen" });
      assert.equal(await start.isEnabled(), true);
      const bounds = await start.boundingBox();
      assert.ok(bounds && bounds.y >= 0 && bounds.y + bounds.height <= 480);
      await start.click();
      await page.waitForFunction(() => window.__FORWARD_CONFIRMATION_RESULT__ !== undefined);
      assert.deepEqual(await page.evaluate(() => window.__FORWARD_CONFIRMATION_RESULT__), {
        effectsConfirmed: true, preparationDigest: preview.digest, removalPhrases: [],
      });
    });
  }
  for (const provider of ["sftp", "ftps", "webdav", "smb"]) {
    for (const direction of ["upload", "download"]) {
      for (const mode of ["AddOnly", "Update"]) {
        await runCase(`ContentSync ${provider} ${direction} ${mode} verwendet die Fensterqueue`, "", async page => {
          if (direction === "upload") await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
          await page.locator(`#connection-list [data-protocol="${provider}"][data-id="mock_${provider}"]`).click();
          await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
          if (mode === "Update") await page.locator("#sync-update").check();
          await page.locator("#btn-sync").click();
          const dialog = page.getByRole("dialog", { name: "Dateisync ausführen" });
          await dialog.waitFor();
          assert.match(await dialog.innerText(), /Quelle.*Ziel.*Best Effort/s);
          assert.equal(await dialog.locator("input").count(), 0);
          if (mode === "Update") assert.match(await dialog.innerText(), /dauerhaft ersetzt.*Keine Doppeldecker-Quarantäne.*keine automatische Wiederherstellung/s);
          await dialog.getByRole("button", { name: mode === "Update" ? "Dauerhafte Änderungen bestätigen" : "Sync bestätigen und starten" }).click();
          await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.some(entry => entry.operation.type === "ContentSync"));
          const state = await page.evaluate(() => window.__MOCK_CONTENT_SYNC_STATE__());
          const request = state.calls.find(call => call.command === "prepare_content_sync").args.request;
          assert.equal(request.source.provider, direction === "upload" ? "local" : provider);
          assert.equal(request.target.provider, direction === "upload" ? provider : "local");
          if (mode === "Update") assert.equal(request.options.compare, "ContentHash");
          assert.equal(state.calls.filter(call => call.command === "enqueue_content_sync").length, 1);
          if (provider === "sftp" && direction === "upload" && mode === "AddOnly") {
            await page.evaluate(() => {
              const queue = window.__DOPPELDECKER_MOCK_QUEUE__; queue.entries[0].state = "Running";
              window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
            });
            assert.match(await page.locator("#dock-summary").innerText(), /Best-Effort-Sync-Ausführung/);
            assert.match(await page.locator("#prog-text").innerText(), /Sync-Ausführung startet/);
          }
        });
      }
    }
  }
  await runCase("ContentSync Remote-zu-Remote bestätigt die Wirkung einmal", "", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.getByRole("listbox", { name: /Rechte Dateiliste/ }).click();
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Dateisync ausführen" }).getByRole("button", { name: "Sync bestätigen und starten" }).click();
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    const state = await page.evaluate(() => window.__MOCK_CONTENT_SYNC_STATE__());
    assert.deepEqual(state.calls.map(call => call.command), ["prepare_content_sync", "enqueue_content_sync"]);
  });
  await runCase("ContentSync Startfehler behält die Vorbereitung für Retry", "?providerRunFailsOnce=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    for (let attempt = 0; attempt < 2; attempt++) {
      await page.locator("#btn-sync").click();
      await page.getByRole("dialog", { name: "Dateisync ausführen" }).getByRole("button", { name: "Sync bestätigen und starten" }).click();
      if (!attempt) await page.getByText(/Provider-Mock-Start ist vor der Entnahme belegt/).waitFor();
    }
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length === 1);
    const state = await page.evaluate(() => window.__MOCK_CONTENT_SYNC_STATE__());
    assert.deepEqual(state.calls.map(call => call.command), ["prepare_content_sync", "enqueue_content_sync", "enqueue_content_sync"]);
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
    assert.match(await page.locator("#dock-summary").innerText(), /nicht automatisch zum Server zurückgeschrieben/);
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
      await page.getByText("Zielhinweise anzeigen",{exact:true}).click();
      await page.getByRole("textbox",{name:"Zielhinweise"}).focus();
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

  await runCase("AP11 Zielhinweise bleiben eindeutig, tastaturbedienbar und escaped", "", async page => {
    await page.setViewportSize({ width: 800, height: 600 });
    await page.evaluate(() => window.__MOCK_EMIT__("fileop-done", {
      files: 100, bytes: 12345, target_copy_new_best_effort: true,
      target_write_limitations: [
        ...Array(2).fill("Keine dauerhafte Temp-Recovery: Privater Recoveryindex fehlt <Recovery & Prüfung>. </textarea><img src=x onerror=window.ap11Injected=true>"),
        ...Array.from({ length: 100 }, (_, i) => `/pane/Datei-${i}-<img src=x onerror=window.ap11Injected=true>.txt: Rechte und Metadaten ausgelassen`),
      ],
    }));
    const summary = page.locator("#dock-summary");
    assert.match(await summary.innerText(), /101 Zielhinweise/);
    assert.doesNotMatch(await summary.innerText(), /Einträge ohne native Zielmetadaten/);
    const toggle = summary.locator("summary", { hasText: "Zielhinweise anzeigen" });
    await toggle.focus(); await page.keyboard.press("Space");
    const report = page.getByRole("textbox", { name: "Zielhinweise" });
    await report.waitFor();
    const notes = (await report.inputValue()).split("\n");
    assert.equal(notes.length, 101);
    assert.match(notes[0], /^Keine dauerhafte Temp-Recovery:.*<Recovery & Prüfung>/);
    assert.equal(notes.filter(note => note.startsWith("Keine dauerhafte Temp-Recovery:")).length, 1);
    assert.match(notes[1], /^\/pane\/Datei-0-<img/);
    assert.match(notes.at(-1), /^\/pane\/Datei-99-<img/);
    assert.equal(await report.evaluate(el => el.readOnly), true);
    assert.equal(await page.locator("#dock img").count(), 0);
    assert.equal(await page.evaluate(() => window.ap11Injected ?? false), false);
    await page.keyboard.press("Tab"); assert.equal(await report.evaluate(el => el === document.activeElement), true);
    if (process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.locator("#dock").screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/ap11-target-notes.png` });
    await page.keyboard.press("PageDown");
    await page.waitForFunction(() => document.querySelector('[aria-label="Zielhinweise"]').scrollTop > 0);
    assert.equal(await report.evaluate(el => el.scrollWidth <= el.clientWidth + 1), true);
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

  await runCase("Native Kopierauslassungen nennen erhaltene Quellen und bleiben in der Queue sichtbar", "", async page => {
    await page.setViewportSize({ width: 1000, height: 750 });
    const path = "/mock/<Quelle>&-mit-Resource-Fork.txt";
    await page.evaluate(path => window.__MOCK_EMIT__("fileop-done", {
      files: 1, bytes: 29, cancelled: false, incomplete_resource_forks: 1,
      retained_source_paths: [path], target_write_limitations: ["Resource Fork nicht übertragen"],
    }), path);
    const dock = page.locator("#dock-summary");
    await dock.getByText("Mit Auslassungen", { exact: true }).waitFor();
    assert.ok((await dock.innerText()).includes("Verschieben unvollständig"));
    assert.equal(await dock.locator("code").innerText(), path);
    assert.equal(await dock.locator("quelle").count(), 0);
    if (process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/native-copy-partial-dock.png` });
    await page.evaluate(() => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      queue.entries.push({ operation_id: "partial-native-copy", state: "Completed", file_incomplete: true,
        operation: { type: "File", kind: "Move", items: ["/mock/source"], destination: "/mock/target" },
        profile_bindings: [], attempts: 1, progress: { phase: "done", files_done: 1, bytes_done: 29 },
        error: null, attention: null, created_unix_ms: Date.now() });
      queue.revision += 1; window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    });
    await page.locator("#btn-queue").click();
    const row = page.locator('[data-queue-id="partial-native-copy"]');
    await row.getByText(/mit Auslassungen abgeschlossen/).waitFor();
    assert.ok((await row.innerText()).includes("betroffene Quellen erhalten"));
    assert.equal(await row.getByRole("button", { name: /erneut|wiederholen/i }).count(), 0);
    if (process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/native-copy-partial-queue.png` });
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

    // Gewöhnliches endgültiges Löschen braucht eine Wirkungsentscheidung.
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote\.txt/ }).click();
    await list.focus();
    await page.keyboard.press("Control+Backspace");
    const deletion = page.getByRole("dialog");
    await deletion.getByText(/Es gibt keinen Papierkorb/).waitFor();
    assert.equal(await deletion.locator("input").count(), 0);
    await deletion.getByRole("button", { name: "Dauerhaft löschen", exact: true }).click();
    await page.locator("#queue-count").filter({ hasText: "2" }).waitFor();
    operation = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.at(-1).operation);
    assert.equal(operation.mode, "permanent");
  });

  await runCase("Große endgültige Remote-Löschung zeigt Umfang und Providergrenzen vor der Phrase", "?remoteFile=1&largeRemoteDelete=1", async page => {
    await page.locator('#connection-list [data-protocol="webdav"][data-id="mock_webdav"]').click();
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote\.txt/ }).click();
    await list.focus();
    await page.keyboard.press("Control+Backspace");
    const dialog = page.getByRole("dialog");
    const description = dialog.getByRole("region", { name: "Wirkung und Einschränkungen" });
    await description.waitFor();
    assert.match(await description.innerText(), /1 ausgewählte Einträge, 1 Objekte und 8 Byte/);
    assert.match(await description.innerText(), /keine Wiederherstellung durch Doppeldecker/);
    assert.match(await description.innerText(), /Collection-DELETE bleibt Best Effort; <fremder>/);
    assert.equal(await description.locator("fremder").count(), 0);
    if (process.env.DOPPELDECKER_UI_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_UI_EVIDENCE_DIR}/remote-delete-large-review.png` });
    await dialog.locator("input").fill("falsche Phrase");
    await dialog.getByRole("button", { name: "OK", exact: true }).click();
    await page.waitForFunction(() => document.querySelector("#modal").hidden);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
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
    await runCase(`AP04R neue Baumwirkung bestätigt den Umfang ohne Phrase bei langer Auslassungsliste: ${theme}`, "", async page => {
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
      assert.equal(await dialog.getByRole("button", { name: "Fortfahren", exact: true }).isVisible(), true);
      assert.equal(await dialog.evaluate(node => node.getBoundingClientRect().height <= window.innerHeight - 20), true);
      const explanation = dialog.getByRole("region", { name: "Wirkung und Einschränkungen", exact: true });
      assert.equal(await explanation.evaluate(node => node.scrollHeight > node.clientHeight), true);
      assert.match(await explanation.innerText(), /79\.txt/);
      await explanation.focus();await explanation.press("PageDown");
      await page.waitForFunction(() => document.querySelector('[role="region"][aria-label="Wirkung und Einschränkungen"]').scrollTop > 0);
      assert.equal(await dialog.locator("input").count(), 0);
      const startBounds = await dialog.getByRole("button", { name: "Fortfahren", exact: true }).boundingBox();
      assert.ok(startBounds && startBounds.y >= 0 && startBounds.y + startBounds.height <= 480);
      if (process.env.AP04R_EVIDENCE_DIR) await page.screenshot({ path: `${process.env.AP04R_EVIDENCE_DIR}/tree-confirm-${theme}.png` });
      await dialog.getByRole("button", { name: "Abbrechen", exact: true }).click();
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state), "AwaitingConfirmation");
      if (await page.locator("#queue-popover").isHidden()) await page.locator("#btn-queue").click();
      await row.getByRole("button", { name: "Plan bestätigen" }).click();
      dialog = page.getByRole("dialog", { name: "Quellbaumumfang bestätigen" });
      await dialog.getByRole("button", { name: "Fortfahren", exact: true }).click();
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
      /Dateiaktionen.*Upload.*Verschieben.*Ersetzen.*Dauerlöschen.*verfügbar.*umgeleitete Elternpfade/s,
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
    assert.match(await quarantine.getAttribute("title") || "", /keine Providerquarantäne.*Dauerhaftes Löschen/s);
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

  for (const provider of ["sftp", "ftps", "webdav", "smb"]) {
    await runCase(`Forward-Bidir ${provider} drei Dateien verwendet eigene Fensterqueue`, "", async page => {
      await page.locator(`#connection-list [data-protocol="${provider}"][data-id="mock_${provider}"]`).click();
      await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
      await page.getByLabel("In beide Richtungen", { exact: true }).check();
      await page.locator("#btn-sync").click();
      const dialog = page.getByRole("dialog", { name: "Bidirektionalen Sync als Best Effort ausführen" }); await dialog.waitFor();
      assert.match(await dialog.innerText(), /kein universelles automatisches Rollback/);
      assert.match(await dialog.innerText(), /eins.txt.*zwei.txt.*drei.txt/s);
      assert.equal(await dialog.locator("input").count(), 0);
      await dialog.getByRole("button", { name: "Wirkung beider Seiten bestätigen" }).click();
      await page.locator("#dock-summary").filter({ hasText: "Bidirektional als Best Effort fertig" }).waitFor();
      const entries = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries);
      assert.equal(entries.length, 1); assert.equal(entries[0].operation.type, "BidirectionalForward"); assert.equal(entries[0].state, "Completed");
      const calls = await page.evaluate(() => window.__MOCK_FORWARD_STATE__().calls);
      assert.equal(calls.filter(call => call.command === "enqueue_forward_bidirectional_sync").length, 1);
      assert.deepEqual(calls.find(call => call.command === "enqueue_forward_bidirectional_sync").args.preparationDigest, Array(32).fill(42));
    });
  }
  for (const theme of ["light", "dark"]) {
    await runCase(`Forward-Bidir Wirkungsdialog Quarantäne und Dauerwirkung bleibt erreichbar: ${theme}`, "?forwardReplace=1", async page => {
      await page.setViewportSize({ width: 760, height: 480 });
      await page.evaluate(async theme => { const { setTheme } = await import("/theme.js"); setTheme(theme, false); }, theme);
      await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
      await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
      await page.getByLabel("In beide Richtungen", { exact: true }).check(); await page.locator("#btn-sync").click();
      const dialog = page.getByRole("dialog", { name: "Bidirektionalen Sync als Best Effort ausführen" }); await dialog.waitFor();
      assert.match(await dialog.innerText(), /lokalen Quarantäne gesichert/); assert.match(await dialog.innerText(), /dauerhaft ersetzt/);
      assert.match(await dialog.innerText(), /kein universelles automatisches Rollback/);
      const start = dialog.getByRole("button", { name: "Wirkung beider Seiten bestätigen" }); await start.scrollIntoViewIfNeeded();
      const bounds = await start.boundingBox(); assert.ok(bounds && bounds.y >= 0 && bounds.y + bounds.height <= 480);
      if (process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/forward-${theme}.png` });
      await page.keyboard.press("Escape");
      await page.waitForFunction(() => !window.__MOCK_FORWARD_STATE__().pending);
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
    });
  }
  await runCase("Forward-Bidir Startupjournal führt zu frischem Neuabgleich", "?forwardStartup=1", async page => {
    await page.locator("#btn-queue").click(); await page.getByRole("button", { name: "Neu abgleichen", exact: true }).click();
    await page.getByRole("dialog", { name: "Neu abgleichen · Forward-Startpaar" }).getByRole("button", { name: "Neu abgleichen", exact: true }).click();
    await page.getByRole("dialog", { name: "Neu abgeglichenen Sync bestätigen" }).getByRole("button", { name: "Wirkung beider Seiten bestätigen" }).click();
    await page.locator("#dock-summary").filter({ hasText: "Bidirektional als Best Effort fertig" }).waitFor();
    const calls = await page.evaluate(() => window.__MOCK_FORWARD_STATE__().calls);
    assert.deepEqual(calls.find(call => call.command === "prepare_forward_bidirectional_queue_recovery").args.resolutions, []);
    assert.equal(calls.filter(call => call.command === "approve_forward_bidirectional_queue_entry").length, 1);
  });
  await runCase("Forward-Bidir Completionjournal bietet nur Baselineabschluss", "?forwardCompletion=1", async page => {
    await page.locator("#btn-queue").click();
    assert.equal(await page.getByRole("button", { name: "Neu abgleichen", exact: true }).count(), 0);
    await page.getByRole("button", { name: "Abschluss prüfen", exact: true }).click();
    await page.locator("#dock-summary").filter({ hasText: "Baselineabschluss bestätigt" }).waitFor();
    const calls = await page.evaluate(() => window.__MOCK_FORWARD_STATE__().calls);
    assert.deepEqual(calls.map(call => call.command), ["finish_forward_bidirectional_queue_completion"]);
  });
  await runCase("Forward-Bidir Profilwechsel verwirft alte Konflikt- und Laufvorbereitung", "", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => {
      const invoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await invoke(command, args);
        if (command === "prepare_forward_bidirectional_sync") await new Promise(resolve => { window.__RELEASE_FORWARD_PROFILE__ = resolve; });
        return result;
      };
      window.__MOCK_EMIT__("menu", "sync_mode");
    });
    await page.getByLabel("In beide Richtungen", { exact: true }).check(); await page.locator("#btn-sync").click();
    await page.waitForFunction(() => typeof window.__RELEASE_FORWARD_PROFILE__ === "function");
    await page.evaluate(() => {
      window.__MOCK_SET_PROFILE_REVISION__("sftp", "mock_sftp", 12);
      window.__MOCK_EMIT__("connections-updated", { protocol: "sftp", profileId: "mock_sftp", revision: 12, change: "saved" });
    });
    await page.evaluate(() => window.__RELEASE_FORWARD_PROFILE__());
    await page.waitForFunction(() => !window.__MOCK_FORWARD_STATE__().pending);
    assert.equal(await page.getByRole("dialog", { name: "Bidirektionalen Sync als Best Effort ausführen" }).count(), 0);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });
  await runCase("Forward-Bidir zweites Fenster behält Vorbereitung nach Discard im ersten", "", async page => {
    const other = await page.context().newPage(); const errors = []; other.on("pageerror", error => errors.push(error.message));
    try {
      await other.goto(page.url(), { waitUntil: "networkidle" });
      await page.evaluate(() => {
        const original = window.__TAURI__.core.invoke;
        window.__TAURI__.core.invoke = async (command, args) => {
          if (command === "prepare_forward_bidirectional_sync") {
            await new Promise(resolve => { window.__RELEASE_FIRST_FORWARD_PREPARATION__ = resolve; });
          }
          return original(command, args);
        };
      });
      for (const target of [page, other]) {
        await target.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
        await target.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
        await target.getByLabel("In beide Richtungen", { exact: true }).check(); await target.locator("#btn-sync").click();
      }
      await page.locator("#btn-sync-close").click();
      await page.waitForFunction(() => typeof window.__RELEASE_FIRST_FORWARD_PREPARATION__ === "function");
      await page.evaluate(() => window.__RELEASE_FIRST_FORWARD_PREPARATION__());
      await page.waitForFunction(() => window.__MOCK_FORWARD_STATE__().calls.some(call => call.command === "discard_forward_bidirectional_sync"));
      const dialog = other.getByRole("dialog", { name: "Bidirektionalen Sync als Best Effort ausführen" }); await dialog.waitFor();
      await dialog.getByRole("button", { name: "Wirkung beider Seiten bestätigen" }).click();
      await other.locator("#dock-summary").filter({ hasText: "Bidirektional als Best Effort fertig" }).waitFor();
      assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
      assert.equal(await other.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 1);
      assert.deepEqual(errors, []);
    } finally { await other.close(); }
  });
  await runCase("Forward-Bidir Konfliktentscheidung bleibt an unresolved Digest gebunden", "?bidirectionalConflict=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen", { exact: true }).check(); await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Konflikt 1 von 1" }).getByRole("button", { name: "Links behalten", exact: true }).click();
    await page.getByRole("dialog", { name: "Bidirektionalen Sync als Best Effort ausführen" }).getByRole("button", { name: "Wirkung beider Seiten bestätigen" }).click();
    await page.locator("#dock-summary").filter({ hasText: "Bidirektional als Best Effort fertig" }).waitFor();
    const calls = await page.evaluate(() => window.__MOCK_FORWARD_STATE__().calls);
    const preparations = calls.filter(call => call.command === "prepare_forward_bidirectional_sync");
    assert.equal(preparations.length, 2); assert.equal(preparations[1].args.request.resolutions.length, 1);
    assert.deepEqual(preparations[1].args.request.expectedUnresolvedDigest, Array(32).fill(41));
    assert.deepEqual(calls.find(call => call.command === "enqueue_forward_bidirectional_sync").args.preparationDigest, Array(32).fill(43));
  });
  await runCase("Forward-Bidir Teilfehler bietet frischen Neuabgleich gegen B0", "?forwardPartial=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen", { exact: true }).check(); await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Bidirektionalen Sync als Best Effort ausführen" }).getByRole("button", { name: "Wirkung beider Seiten bestätigen" }).click();
    await page.locator("#dock-summary").filter({ hasText: "B0 bleibt erhalten" }).waitFor();
    await page.locator("#btn-queue").click();
    const queue = page.locator("#queue-popover");
    assert.equal(await queue.getByRole("button", { name: "Erneut versuchen", exact: true }).count(), 0);
    await queue.getByRole("button", { name: "Neu abgleichen", exact: true }).click();
    await page.getByRole("dialog", { name: /Neu abgleichen ·/ }).getByRole("button", { name: "Neu abgleichen", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "Neu abgeglichenen Sync bestätigen" });
    assert.match(await dialog.innerText(), /B0 blieb unverändert.*alte Konfliktentscheidungen wurden verworfen/s);
    assert.equal(await dialog.getByRole("button", { name: "Rollback", exact: true }).count(), 0);
    await dialog.getByRole("button", { name: "Wirkung beider Seiten bestätigen" }).click();
    await page.locator("#dock-summary").filter({ hasText: "Bidirektional als Best Effort fertig" }).waitFor();
    const calls = await page.evaluate(() => window.__MOCK_FORWARD_STATE__().calls);
    assert.deepEqual(calls.find(call => call.command === "prepare_forward_bidirectional_queue_recovery").args.resolutions, []);
    assert.deepEqual(calls.find(call => call.command === "approve_forward_bidirectional_queue_entry").args.preparationDigest, Array(32).fill(45));
  });
  await runCase("Forward-Bidir ungestartete Queuevorbereitung lässt sich sauber verwerfen", "?forwardReopen=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen", { exact: true }).check(); await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Bidirektionalen Sync als Best Effort ausführen" }).getByRole("button", { name: "Wirkung beider Seiten bestätigen" }).click();
    await page.locator("#btn-queue").click(); await page.getByRole("button", { name: "Verwerfen", exact: true }).click();
    await page.getByRole("dialog", { name: "Operation verwerfen" }).getByRole("button", { name: "Fortfahren", exact: true }).click();
    await page.waitForFunction(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state === "Cancelled");
    assert.equal(await page.evaluate(() => window.__MOCK_FORWARD_STATE__().calls.some(call => call.command === "approve_forward_bidirectional_queue_entry")), false);
  });
  await runCase("Forward-Bidir Reopen verlangt neuen Previewdigest", "?forwardReopen=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen", { exact: true }).check(); await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Bidirektionalen Sync als Best Effort ausführen" }).getByRole("button", { name: "Wirkung beider Seiten bestätigen" }).click();
    await page.locator("#btn-queue").click(); await page.getByRole("button", { name: "Plan bestätigen" }).click();
    const dialog = page.getByRole("dialog", { name: "Bidirektionalen Sync als Best Effort ausführen" });
    assert.match(await dialog.innerText(), /Neue Wirkung nach Neustart: frischer Digest/);
    await dialog.getByRole("button", { name: "Wirkung beider Seiten bestätigen" }).click();
    await page.locator("#dock-summary").filter({ hasText: "Bidirektional als Best Effort fertig" }).waitFor();
    const calls = await page.evaluate(() => window.__MOCK_FORWARD_STATE__().calls);
    assert.deepEqual(calls.find(call => call.command === "approve_forward_bidirectional_queue_entry").args.preparationDigest, Array(32).fill(44));
  });
  await runCase("Forward-Bidir Scopewechsel verwirft verspätete Vorbereitung", "?forwardPrepareDelay=1", async page => {
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen", { exact: true }).check(); await page.locator("#btn-sync").click();
    await page.locator("#btn-cancel").waitFor({ state: "visible" }); await page.locator("#btn-sync-close").click();
    await page.waitForFunction(() => window.__MOCK_FORWARD_STATE__().calls.some(call => call.command === "discard_forward_bidirectional_sync") && !window.__MOCK_FORWARD_STATE__().pending && document.querySelector("#btn-cancel").hidden);
    assert.equal(await page.getByRole("dialog", { name: "Bidirektionalen Sync als Best Effort ausführen" }).count(), 0);
    assert.equal(await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries.length), 0);
  });

  await runCase("Bidirektionale Paarvorbereitung bleibt dauerhaft und abbrechbar", "", async page => {
    // Bidirektional ist im Sync-Modus die dritte Richtungsoption.
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    await page.locator("#btn-sync").click();
    const dialog = page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /Beide Bäume.*Baseline.*erneut geprüft/s);
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
      .getByRole("button", { name: "Sync starten" }).click();

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
    await page.getByRole("dialog", { name: "Bidirektionalen Sync fortsetzen" }).getByRole("button", { name: "Fortsetzen" }).click();
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
    await page.getByRole("dialog", { name: "Vorherigen Bestand wiederherstellen" }).getByRole("button", { name: "Wiederherstellung starten" }).click();
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
      .getByRole("button", { name: "Sync starten" }).click();
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
    await page.getByRole("dialog", { name: "Bidirektionalen Sync fortsetzen" }).getByRole("button", { name: "Fortsetzen" }).click();
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
    await page.getByRole("dialog", { name: "Vorherigen Bestand wiederherstellen" }).getByRole("button", { name: "Wiederherstellung starten" }).click();
    await page.getByText(/Rollback fertig/).waitFor();
    await page.waitForTimeout(50);
    assert.match(await page.locator("#dock").innerText(), /Rollback fertig/);
    assert.doesNotMatch(await page.locator("#dock").innerText(), /Rollback läuft/);
  });

  await runCase("Schnelle Queue-Freigabe verwirft ihre ältere Command-Antwort", "?bidirectionalFastApproveBeforeResponse=1", async page => {
    await page.locator("#btn-queue").click();
    await page.getByRole("button", { name: "Plan bestätigen" }).click();
    await page.getByRole("dialog", { name: "Sync ausführen" })
      .getByRole("button", { name: "Fortfahren" }).click();
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


  await runCase("Bidirektionaler Start retryt nach Busy dieselbe Vorbereitung", "?bidirectionalRunFailsOnce=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("In beide Richtungen").check();
    for (let attempt = 0; attempt < 2; attempt += 1) {
      await page.locator("#btn-sync").click();
      const execution = page.getByRole("dialog", { name: "Bidirektional · quelle ↔ ziel" });
      await execution.getByRole("button", { name: "Sync starten" }).click();
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
    await execution.getByRole("button", { name: "Sync starten" }).click();
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
    await execution.getByRole("button", { name: "Sync starten" }).click();

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
      await page.getByRole("dialog", { name: "Bidirektionalen Sync fortsetzen" })
        .getByRole("button", { name: "Abbrechen" }).click();
      await page.getByText(/Mock-Recovery-Discard scheiterte vor Zustellung/).waitFor();
      let state = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
      assert.equal(state.pendingBidirectional, true,
        "Ein nicht zugestellter Discard darf den Backend-Slot nicht als frei behandeln.");

      await page.locator("#btn-queue").click();
      await page.getByRole("button", { name: "Recovery" }).click();
      await page.getByRole("dialog", { name: "Recovery · Recovery-Paar" })
        .getByRole("button", { name: "Fortsetzen" }).click();
      await page.getByRole("dialog", { name: "Bidirektionalen Sync fortsetzen" })
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
    "Recovery-Discard nach Wirkungsabbruch zeigt den Transportfehler",
    "?bidiRecovery=1&bidirectionalRecoveryDiscardBeforeDeliveryFailures=1",
    async page => {
      await page.locator("#btn-queue").click();
      await page.getByRole("button", { name: "Recovery" }).click();
      await page.getByRole("dialog", { name: "Recovery · Recovery-Paar" })
        .getByRole("button", { name: "Fortsetzen" }).click();
      await page.getByRole("dialog", { name: "Bidirektionalen Sync fortsetzen" })
        .getByRole("button", { name: "Abbrechen" }).click();
      await page.getByText(/Mock-Recovery-Discard scheiterte vor Zustellung/).waitFor();
      const state = await page.evaluate(() => window.__MOCK_PREPARATION_ACTIONS__());
      assert.equal(state.pendingBidirectional, false);
      const discards = state.calls.filter(call =>
        call.command === "discard_bidirectional_recovery_preparation");
      assert.equal(discards.length, 2);
      assert.equal(discards[0].args.recoveryPreparationId, discards[1].args.recoveryPreparationId);
    },
  );

  await runCase("Mirror bestätigt die sichtbare Richtung und Quarantänewirkung einmal", "?realEnabled=1&mirrorDelete=1", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
    await page.getByLabel("Verwaiste in Quarantäne verschieben").check();
    await page.locator("#btn-sync").click();
    const dialog = page.getByRole("dialog", { name: "Sync ausführen" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /Quelle.*\/mock\/quelle.*Ziel.*\/mock\/ziel.*quarantänisieren/s);
    assert.equal(await dialog.locator("input").count(), 0);
    await dialog.getByRole("button", { name: "Verschieben in Quarantäne bestätigen und starten" }).click();
    await page.waitForFunction(() => document.documentElement.dataset.realConfirmation === "true");
  });

  await runCase("Queue-Pause, Driftwirkung und Entfernen", "", async page => {
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
    let dialog = page.getByRole("dialog", { name: "Queue-Wirkung bestätigen" });
    assert.equal(await dialog.locator("input").count(), 0);
    await dialog.getByRole("button", { name: "Abbrechen" }).click();
    assert.match(await page.locator("#queue-popover").innerText(), /neue Bestätigung nötig/);
    await page.getByRole("button", { name: "Plan bestätigen" }).click();
    dialog = page.getByRole("dialog", { name: "Queue-Wirkung bestätigen" });
    await dialog.getByRole("button", { name: "Fortfahren" }).click();
    const remove = page.getByRole("button", { name: "Entfernen" });
    await remove.waitFor();
    await remove.click();
    assert.match(await page.locator("#queue-popover-list").innerText(), /Queue ist leer/);
  });

  for (const attentionType of ["LocalFileCommit", "BestEffortCommitDowngrade"]) {
    await runCase(`${attentionType}: Best-Effort-Wirkungsdialog bleibt scrollbar und ohne Phrase`, "", async page => {
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
      assert.equal(await dialog.locator("input").count(), 0);
      const title = await dialog.locator(".modal-title").boundingBox();
      const confirm = await dialog.getByRole("button", { name: "Fortfahren", exact: true }).boundingBox();
      assert.ok(title && title.height < 50, "Der Titel darf nicht die lange Risikobilanz aufnehmen");
      assert.ok(confirm && confirm.y >= 0 && confirm.y + confirm.height <= 600, "Bestätigung bleibt im Fenster erreichbar");
      const explanation = dialog.getByRole("region", { name: "Wirkung und Einschränkungen", exact: true });
      await explanation.focus();
      await explanation.press("PageDown");
      await page.waitForFunction(() => document.querySelector(".modal-description-scroll").scrollTop > 0);
      await dialog.getByRole("button", { name: "Abbrechen", exact: true }).click();
      const queue = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__);
      assert.equal(queue.entries[0].state, "AwaitingConfirmation");
    });
  }

  await runCase("Queue-Wirkungsdialog verwirft veralteten Plan bei gleicher Backendphrase", "", async page => {
    await page.evaluate(() => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__; queue.paused = true;
      const plan = { copy_count: 1, update_count: 0, metadata_count: 0, delete_count: 0, actions: [] };
      queue.entries = [{ operation_id: "stale-attention", operation: { type: "Sync", source: "/mock/quelle", target: "/mock/ziel", options: { mode: "AddOnly" }, expected_plan: plan }, state: "AwaitingConfirmation", attempts: 1, progress: {}, attention: { type: "PlanChanged", reason: "Ein neuer Pfad", confirmation: "gleiche-phrase", current_plan: plan } }];
      const invoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "approve_queue_entry") window.__QUEUE_EFFECT_ARGS__ = structuredClone(args);
        return invoke(command, args);
      };
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    });
    await page.locator("#btn-queue").click();
    await page.getByRole("button", { name: "Plan bestätigen" }).click();
    let dialog = page.getByRole("dialog", { name: "Queue-Wirkung bestätigen" });
    assert.match(await dialog.innerText(), /1 neu/);
    assert.equal(await dialog.locator("input").count(), 0);
    await page.evaluate(() => {
      const queue = window.__DOPPELDECKER_MOCK_QUEUE__;
      queue.entries[0].attention.current_plan = { ...queue.entries[0].attention.current_plan, copy_count: 7 };
      window.__MOCK_EMIT__("queue-updated", structuredClone(queue));
    });
    await dialog.getByRole("button", { name: "Fortfahren" }).click();
    await page.getByText(/Die Queue-Rückfrage hat sich seit der Anzeige geändert/).waitFor();
    let state = await page.evaluate(() => ({ entry: window.__DOPPELDECKER_MOCK_QUEUE__.entries[0], args: window.__QUEUE_EFFECT_ARGS__ }));
    assert.equal(state.entry.state, "AwaitingConfirmation");
    assert.equal(state.args.expectedAttention.current_plan.copy_count, 1);
    assert.equal(state.args.effectsConfirmed, true);
    assert.equal(Object.hasOwn(state.args, "confirmation"), false);
    await page.getByRole("button", { name: "Plan bestätigen" }).click();
    dialog = page.getByRole("dialog", { name: "Queue-Wirkung bestätigen" });
    assert.match(await dialog.innerText(), /7 neu/);
    await dialog.getByRole("button", { name: "Fortfahren" }).click();
    state = await page.evaluate(() => window.__DOPPELDECKER_MOCK_QUEUE__.entries[0]);
    assert.equal(state.state, "Pending");
    assert.equal(state.operation.expected_plan.copy_count, 7);
  });

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

  await runCase("Sync-Abbruch hält das offene Queue-Popover und seinen Fokus", "", async page => {
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
    await page.getByRole("dialog", { name: "Sync ausführen" })
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

  for (const theme of ["light", "dark"]) for (const width of [800, 390]) {
    await runCase(`Entfernter Dateinamenzyklus zeigt Recovery ohne Retry · ${theme} · ${width}`, "", async page => {
      await page.evaluate(theme => {
        document.documentElement.dataset.theme=theme;
        const queue=window.__DOPPELDECKER_MOCK_QUEUE__;
        const id="0123456789abcdef0123456789abcdef";
        queue.entries=[{operation_id:id,state:"Interrupted",attempts:1,created_unix_ms:Date.now(),progress:{},attention:null,
          error_code:"outcome_unclear",error:"Namenstausch teilweise ausgeführt; prüfen.",temp_recovery_paths:["/pane/.doppeldecker-rename-"+id],
          operation:{type:"RenameCycleRecovery",record:{id,root:"/pane",identity:{provider:"Ftps",identity:{server:{host:"test.example",port:2121}}},
            old_names:["ab","aab","aaab"],new_names:["aab","aaab","ab"],current_names:[".doppeldecker-rename-"+id,"aab","aaab"],
            temp_name:".doppeldecker-rename-"+id,pending:{member:2,from:"aaab",to:"ab"},finished:false}}}];
        window.__MOCK_EMIT__("queue-updated",structuredClone(queue));
      },theme);
      await page.setViewportSize({width,height:600});
      await page.locator("#btn-queue").click();
      const popover=page.getByRole("dialog",{name:"Operations-Queue"});
      const row=popover.locator('[data-queue-id="0123456789abcdef0123456789abcdef"]');
      await row.getByText(/Entfernten Namenstausch prüfen · FTPS · test.example:2121 · \/pane/).waitFor();
      assert.equal(await row.getByRole("button",{name:"Neu prüfen"}).count(),0);
      await row.getByText(/^Namenslage ·/).click();
      const names=row.getByRole("textbox",{name:"Gespeicherte Namenslage des Dateinamenzyklus"});
      assert.match(await names.inputValue(),/"aaab" → Ziel "ab"; letzter Stand: "aaab" oder "ab" \(Ausgang unklar\)/);
      assert.match(await row.innerText(),/manuell prüfen/);
      assert.equal(await names.getAttribute("readonly"),"");
      assert.equal(await row.evaluate(element=>element.scrollWidth<=element.clientWidth),true);
      await row.getByRole("button",{name:"Wirkung geprüft und schließen"}).click();
      const dialog=page.getByRole("dialog",{name:"Unklaren Ausgang schließen"});
      assert.match(await dialog.innerText(),/Zwischenname bleibt erhalten/);
      await dialog.getByRole("button",{name:"Abbrechen"}).click();
      await row.waitFor();
      assert.equal(await page.evaluate(()=>window.__DOPPELDECKER_MOCK_QUEUE__.entries[0].state),"Interrupted");
    });
  }

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

  await runCase("Beenden bei unlesbarer Queue erhält das Journal und erfindet keine Einträge", "", async page => {
    await page.evaluate(() => window.__MOCK_EMIT__("queue-exit-requested", {
      token: "exit-unlesbar", openEntries: 0, unreadableQueues: ["main"],
    }));
    const dialog = page.getByRole("dialog", { name: "Doppeldecker beenden?" });
    await dialog.waitFor();
    const text = await dialog.innerText();
    assert.match(text, /nicht gelesen.*main.*Arbeitsstand ist unbekannt.*unverändert erhalten/s);
    assert.doesNotMatch(text, /[01] offene/);
    await dialog.getByRole("button", { name: "Hier bleiben" }).click();
    assert.equal(await page.locator("html").getAttribute("data-quit-after-queue-warning"), null);
    await page.evaluate(() => window.__MOCK_EMIT__("queue-exit-requested", {
      token: "exit-unlesbar-neu", openEntries: 0, unreadableQueues: ["main"],
    }));
    await dialog.waitFor();
    await dialog.getByRole("button", { name: "Trotzdem beenden" }).click();
    await page.waitForFunction(() => document.documentElement.dataset.quitAfterQueueToken === "exit-unlesbar-neu");
  });

  await runCase("Beenden trennt offene Einträge von unlesbaren Fensterqueues", "", async page => {
    await page.setViewportSize({ width: 520, height: 780 });
    await page.evaluate(() => window.__MOCK_EMIT__("queue-exit-requested", {
      token: "exit-gemischt", openEntries: 2, unreadableQueues: ["window-<img>"],
    }));
    const dialog = page.getByRole("dialog", { name: "Doppeldecker beenden?" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /2 offene.*nicht gelesen.*window-<img>/s);
    assert.equal(await dialog.locator("img").count(), 0);
    await dialog.getByRole("button", { name: "Trotzdem beenden" }).click();
    await page.waitForFunction(() => document.documentElement.dataset.quitAfterQueueToken === "exit-gemischt");
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
    const dialog = page.getByRole("dialog", { name: "Sync ausführen" });
    const lastAction = dialog.getByRole("button", { name: "Sync starten" });
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

  await runCase("FTPS-Zertifikat: öffentlicher Standard speichert nach Prüfung ohne Pin", "", async page => {
    await page.locator("#conn-add").click();
    await page.getByRole("dialog", { name: "Verbindung hinzufügen" }).locator('[data-pick="ftps"]').click();
    const dialog = page.getByRole("dialog", { name: "FTP-/FTPS-Verbindung hinzufügen" });
    assert.equal(await dialog.locator("#ftps-policy").inputValue(), "web_pki");
    await dialog.locator("#ftps-name").fill("Öffentlich");
    await dialog.locator("#ftps-host").fill("public.mock.test");
    await dialog.locator("#ftps-user").fill("tester");
    await captureNativeSecret(dialog);
    await dialog.getByRole("button", { name: "Prüfen und speichern" }).click();
    await dialog.waitFor({ state: "detached" });
    const actions = await page.evaluate(() => window.__MOCK_CONNECTION_ACTIONS__());
    assert.equal(actions.filter(action => action.command === "verify_ftps_certificate").length, 1);
    assert.equal(actions.filter(action => action.command === "discover_ftps_certificate").length, 0);
    const state = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
    const saved = state.ftpsProfiles.find(profile => profile.name === "Öffentlich");
    assert.equal(saved.connection.certificate_policy, "web_pki");
    assert.equal(saved.connection.certificate, null);
  });

  for (const rejected of [false, true]) {
    await runCase(`FTPS-Zertifikat: Bestandsprofil auf öffentliche Prüfung umstellen ${rejected ? "Fehler" : "Erfolg"}`, rejected ? "?ftpsPublicVerifyError=1" : "", async page => {
      const before = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
      await page.locator('#connection-list [data-id="mock_ftps"] .conn-edit').click();
      const dialog = page.getByRole("dialog", { name: "FTP-/FTPS-Verbindung bearbeiten" });
      assert.equal(await dialog.locator("#ftps-policy").inputValue(), "pinned_certificate");
      await dialog.locator("#ftps-policy").selectOption("web_pki");
      if (rejected) await captureNativeSecret(dialog);
      await dialog.getByRole("button", { name: "Prüfen und speichern" }).click();
      if (rejected) {
        await page.getByText("Öffentliche Zertifikatsprüfung fehlgeschlagen", { exact: false }).waitFor();
        assert.equal(await dialog.locator("#ftps-policy").inputValue(), "web_pki");
        const after = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
        assert.deepEqual(after.ftpsProfiles, before.ftpsProfiles);
        assert.deepEqual(after.secretTransactions, before.secretTransactions);
        assert.equal(after.pendingSecretCount, 1, "Eine fehlgeschlagene TLS-Probe darf den nativen Einmalwert nicht verbrauchen.");
        await dialog.getByRole("button", { name: "Abbrechen" }).click();
      } else {
        await dialog.waitFor({ state: "detached" });
        const after = await page.evaluate(() => window.__MOCK_CONNECTION_STATE__());
        assert.equal(after.ftpsProfiles[0].id, "mock_ftps");
        assert.equal(after.ftpsProfiles[0].connection.certificate, null);
        assert.equal(after.ftpsProfiles[0].connection.certificate_policy, "web_pki");
        assert.deepEqual(after.ftpsProfiles[0].connection.password_reference, before.ftpsProfiles[0].connection.password_reference);
      }
    });
  }

  for (const policy of ["web_pki_and_pin", "pinned_certificate", "unverified_certificate"]) {
    await runCase(`FTPS-Zertifikat: Pin erneuern bewahrt Prüfstufe ${policy}`, "?ftpsRenewedCertificate=1", async page => {
      await page.locator('#connection-list [data-id="mock_ftps"] .conn-edit').click();
      const dialog = page.getByRole("dialog", { name: "FTP-/FTPS-Verbindung bearbeiten" });
      await dialog.locator("#ftps-policy").selectOption(policy);
      await dialog.getByRole("button", { name: "Zertifikat neu ermitteln" }).click();
      await dialog.getByRole("button", { name: "Neues Zertifikat binden und speichern" }).waitFor();
      assert.match(await dialog.locator("#ftps-certificate").innerText(), /Bisheriger Fingerabdruck:.*Neu ermittelter Fingerabdruck:/s);
      assert.equal(await dialog.locator("#ftps-policy").inputValue(), policy);
      assert.deepEqual((await page.evaluate(() => window.__MOCK_CONNECTION_STATE__())).ftpsProfiles[0].connection.certificate.sha256, Array(32).fill(7));
      await dialog.getByRole("button", { name: "Neues Zertifikat binden und speichern" }).click();
      await dialog.waitFor({ state: "detached" });
      const saved = (await page.evaluate(() => window.__MOCK_CONNECTION_STATE__())).ftpsProfiles[0];
      assert.equal(saved.connection.certificate_policy, policy);
      assert.deepEqual(saved.connection.certificate.sha256, Array(32).fill(9));
    });
  }

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
