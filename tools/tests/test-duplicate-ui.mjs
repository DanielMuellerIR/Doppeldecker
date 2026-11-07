#!/usr/bin/env node
/** Echte Pane-/Menü-/Modalmodule mit kontrollierter Duplizier-IPC. */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:net";
import { once } from "node:events";
import { mkdir } from "node:fs/promises";
import { chromium } from "playwright";
import { createBrowserCleanup } from "../browser-cleanup.mjs";

const probe = createServer();
probe.listen(0, "127.0.0.1");
await once(probe, "listening");
const port = probe.address().port;
probe.close();
await once(probe, "close");
const server = spawn("python3", ["tools/mock-ui-server.py", "--port", String(port)], {
  cwd: new URL("../..", import.meta.url), stdio: ["ignore", "ignore", "pipe"],
});
const cleanup = createBrowserCleanup(server);
const url = `http://127.0.0.1:${port}`;
const mock = `
(() => {
  const original = window.__TAURI__.core.invoke;
  window.__DUP_CALLS__ = [];
  window.__DUP_CONFIG__ = {};
  window.__TAURI__.core.invoke = async (command, args) => {
    if (!command.includes("duplicate")) return original(command, args);
    window.__DUP_CALLS__.push({command, args});
    if (command === "prepare_duplicate") {
      if (window.__DUP_CONFIG__.hold) await new Promise(resolve => { window.__DUP_RELEASE__ = resolve; });
      if (args.request.provider !== "local") return {supported:false, blockers:["Remote-Duplikate sind noch nicht verfügbar; normale Kopieraktionen bleiben erreichbar."], mappings:[]};
      return { supported:true, preparationId:"bound-duplicate-1", parent:args.request.root,
        mappings:args.request.items.map(path => ({sourceName:path.split("/").pop(), targetName:(window.__DUP_CONFIG__.long ? "SehrLangerName".repeat(18) : path.split("/").pop()) + " (Kopie exakt).txt"})) };
    }
    if (args.preparationId !== "bound-duplicate-1") throw new Error("Ungebundene Namen");
    if (command === "discard_duplicate") return null;
    if (window.__DUP_CONFIG__.drift) throw {code:"plan_drift", message:"Quelle oder Duplikatname hat sich geändert; bitte neu vorbereiten"};
    return "duplicate-operation-1";
  };
})();`;
let browser;
try {
  for (let attempt = 0; attempt < 80; attempt += 1) {
    cleanup.checkServer();
    try { if ((await fetch(url)).ok) break; } catch {}
    await new Promise(resolve => setTimeout(resolve, 25));
  }
  browser = await cleanup.launch(chromium, {headless:true, executablePath:process.env.DOPPELDECKER_PLAYWRIGHT_EXECUTABLE || undefined});
  const evidence = process.env.DOPPELDECKER_DUPLICATE_SCREENSHOTS;
  if (evidence) await mkdir(evidence, {recursive:true});
  async function pageFor(theme = "light", query = "?keyboard=1") {
    const page = await browser.newPage({viewport:{width:650, height:480}, colorScheme:theme});
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.route("**/mock-tauri.js", async route => {
      const response = await route.fetch();
      await route.fulfill({response, body:(await response.text()) + mock});
    });
    await page.goto(url + query, {waitUntil:"networkidle"});
    await page.getByRole("listbox", {name:/Linke Dateiliste/}).waitFor();
    await page.evaluate(async theme => { const {setTheme} = await import("/theme.js"); setTheme(theme); }, theme);
    return {page, errors};
  }
  for (const theme of ["light", "dark"]) {
    const {page, errors} = await pageFor(theme);
    const list = page.getByRole("listbox", {name:/Linke Dateiliste/});
    await list.getByRole("option", {name:/Alpha.txt/}).click();
    await list.getByRole("option", {name:/Beta.txt/}).click({modifiers:["ControlOrMeta"]});
    await page.evaluate(() => { window.__DUP_CONFIG__.long = true; window.__MOCK_EMIT__("menu", "duplicate"); });
    const dialog = page.getByRole("dialog");
    await dialog.getByRole("button", {name:"Duplizieren", exact:true}).waitFor();
    assert.equal(await dialog.getByRole("row").count(), 3);
    assert.equal(await dialog.getByRole("cell").count(), 4);
    assert.equal(await dialog.locator(".duplicate-mappings").evaluate(element => element.scrollWidth <= element.clientWidth), true);
    assert.equal(await page.evaluate(() => window.__DUP_CALLS__.filter(call => call.command === "confirm_duplicate").length), 0);
    if (evidence) await page.screenshot({path:`${evidence}/duplicate-${theme}.png`});
    const mappingRegion = dialog.getByRole("region", {name:"Namenszuordnung"});
    await mappingRegion.focus();
    await page.keyboard.press("PageDown");
    await page.waitForFunction(() => document.querySelector(".duplicate-mapping-scroll").scrollTop > 0);
    await page.keyboard.press("Shift+Tab");
    assert.equal(await dialog.getByRole("button", {name:"Duplizieren", exact:true}).evaluate(element => element === document.activeElement), true);
    await mappingRegion.evaluate(element => { element.scrollTop = 0; });
    await dialog.getByRole("button", {name:"Duplizieren", exact:true}).click();
    await page.getByText("Duplizieren eingereiht", {exact:true}).waitFor();
    const calls = await page.evaluate(() => window.__DUP_CALLS__);
    assert.equal(calls[0].args.request.items.length, 2);
    assert.deepEqual(calls[1], {command:"confirm_duplicate", args:{preparationId:"bound-duplicate-1"}});
    assert.deepEqual(errors, []);
    assert.deepEqual(await page.evaluate(() => window.__DOPPELDECKER_MOCK_ERRORS__), []);
    await page.close();
  }
  {
    const {page} = await pageFor();
    const row = page.getByRole("listbox", {name:/Linke Dateiliste/}).getByRole("option", {name:/Alpha.txt/});
    await row.click({button:"right"});
    await page.getByRole("menuitem", {name:"Duplizieren", exact:true}).click();
    await page.getByRole("dialog").getByRole("button", {name:"Duplizieren", exact:true}).waitFor();
    await page.keyboard.press("Escape");
    await page.waitForFunction(() => window.__DUP_CALLS__.length === 2);
    assert.deepEqual(await page.evaluate(() => window.__DUP_CALLS__[1]),
      {command:"discard_duplicate", args:{preparationId:"bound-duplicate-1"}});
    await row.click();
    await page.keyboard.press("ControlOrMeta+d");
    await page.getByRole("dialog").getByRole("button", {name:"Duplizieren", exact:true}).waitFor();
    await page.keyboard.press("Escape");
    await page.waitForFunction(() => window.__DUP_CALLS__.length === 4);
    assert.equal(await page.evaluate(() => window.__DUP_CALLS__.filter(call => call.command === "confirm_duplicate").length), 0);
    await page.close();
  }
  {
    const {page} = await pageFor();
    await page.getByRole("listbox", {name:/Linke Dateiliste/}).getByRole("option", {name:/Alpha.txt/}).click();
    await page.evaluate(() => { window.__DUP_CONFIG__.drift = true; window.__MOCK_EMIT__("menu", "duplicate"); });
    await page.getByRole("dialog").getByRole("button", {name:"Duplizieren", exact:true}).click();
    await page.getByText(/Quelle oder Duplikatname hat sich geändert/).waitFor();
    assert.equal(await page.evaluate(() => window.__DUP_CALLS__.filter(call => call.command === "confirm_duplicate").length), 1);
    await page.close();
  }
  {
    const {page} = await pageFor();
    await page.getByRole("listbox", {name:/Linke Dateiliste/}).getByRole("option", {name:/Alpha.txt/}).click();
    await page.evaluate(() => { window.__DUP_CONFIG__.hold = true; window.__MOCK_EMIT__("menu", "duplicate"); });
    await page.waitForFunction(() => typeof window.__DUP_RELEASE__ === "function");
    const path = page.locator('[data-side="left"] .path');
    await path.fill("/anders"); await path.press("Enter");
    await page.evaluate(() => window.__DUP_RELEASE__());
    await page.waitForFunction(() => document.querySelector('[data-side="left"] .path').value === "/anders");
    await page.waitForFunction(() => window.__DUP_CALLS__.some(call => call.command === "discard_duplicate"));
    assert.equal(await page.getByRole("dialog").count(), 0);
    assert.equal(await page.evaluate(() => window.__DUP_CALLS__.length), 2);
    await page.close();
  }
  {
    const {page} = await pageFor("light", "?remoteFile=1");
    await page.locator('#connection-list [data-protocol="sftp"]').first().click();
    const list = page.getByRole("listbox", {name:/Linke Dateiliste/});
    await list.getByRole("option").first().click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "duplicate"));
    await page.getByText("Duplizieren nicht verfügbar", {exact:true}).waitFor();
    const calls = await page.evaluate(() => window.__DUP_CALLS__);
    assert.equal(calls[0].args.request.provider, "sftp");
    assert.equal(calls.length, 1);
    await page.close();
  }
  console.log("ok  Duplizieren: beide Themes, konkrete Mehrfachzuordnung, Fokusfalle, Menü/Kürzel, Abbruch, Drift, Navigation und Remote-Backendvertrag");
} finally {
  await cleanup.close();
}
