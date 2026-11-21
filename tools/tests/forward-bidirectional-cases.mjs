import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

export async function runForwardBidirectionalCases(runCase) {
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
  await runCase("Forward-Bidir Paarabschluss verwendet die registrierte ID ohne neue Vorbereitung", "?forwardPairCompletion=1", async page => {
    const wire = JSON.parse(await readFile(new URL("./bidirectional-wire.json", import.meta.url)));
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__FORWARD_PAIR_WIRE__ = [];
      window.__TAURI__.core.invoke = async (command, args) => {
        const result = await original(command, args);
        if (["register_forward_bidirectional_pair", "forward_bidirectional_pair_status", "finish_forward_bidirectional_pair_completion"].includes(command)) {
          window.__FORWARD_PAIR_WIRE__.push({ command, args: structuredClone(args), result: structuredClone(result) });
        }
        return result;
      };
      window.__MOCK_EMIT__("menu", "sync_mode");
    });
    await page.getByLabel("In beide Richtungen", { exact: true }).check();
    await page.locator("#btn-sync").click();
    await page.getByRole("dialog", { name: "Baselineabschluss prüfen" }).getByRole("button", { name: "Baseline abschließen" }).click();
    await page.locator("#dock-summary").filter({ hasText: "Baseline abgeschlossen" }).waitFor();
    const [registration, status, finish] = await page.evaluate(() => window.__FORWARD_PAIR_WIRE__);
    assert.deepEqual(Object.keys(registration.result).sort(), Object.keys(wire.registeredPair).sort());
    assert.deepEqual(Object.keys(status.result).sort(), Object.keys(wire.forwardPairStatus).sort());
    assert.equal(registration.result.pairId.length, 16);
    assert.equal(status.result.run_id.length, 16);
    assert.equal(status.result.baseline_completion_available, true);
    assert.deepEqual(status.result.plan.pair_id, registration.result.pairId);
    assert.deepEqual(status.args, { pairId: registration.result.pairId });
    assert.deepEqual(finish.args, status.args);
    assert.equal(finish.result, null);
    const { calls, entries } = await page.evaluate(() => ({
      calls: window.__MOCK_FORWARD_STATE__().calls,
      entries: window.__DOPPELDECKER_MOCK_QUEUE__.entries,
    }));
    assert.deepEqual(calls.map(call => call.command), ["forward_bidirectional_pair_status", "finish_forward_bidirectional_pair_completion"]);
    assert.deepEqual(entries, []);
  });
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
}
