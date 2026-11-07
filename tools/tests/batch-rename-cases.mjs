import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";

const webdavNumberingLimitations = [
  "Genau ein bedingtes MOVE verwendet Overwrite: F für den Zielnamen und If-Match für die Quelle. Die Elternkette wird frisch geprüft und das Ergebnis unabhängig zurückgelesen, ohne künstliche Schreib- oder Löschprobe.",
  "Die Serverbedingungen sind nicht verhaltensgeprüft: Missachtet der Server Overwrite: F, könnte ein gerade entstandenes Ziel unwiederbringlich ersetzt werden. Dauerhafte Speicherung ist nicht bewiesen. Bei unklarem Ausgang beide Namen prüfen; kein automatisches Wiederholen oder Entfernen.",
  "Der umzubenennende Eintrag ist über seinen Pfad gebunden: Zwischen dem frischen Elternlisting und der Umbenennung könnte ein gleichnamiger Eintrag ausgetauscht worden sein.",
  "Eine einzelne WebDAV-If-Bedingung bindet den unmittelbaren Elternordner; weitere Elternlisten würden sie durch ODER abschwächen. Die übrige Kette bis zum Profilroot wird nur vor und nach jeder Wirkung geprüft. Ein später Wechsel eines Vorfahren kann bereits wirken, bevor der Readback den Ausgang als unklar meldet. Es gibt keinen automatischen Rücktausch oder Wiederholversuch.",
  "Ein gleichzeitiger Austausch desselben Serverpfads mit identischen beobachtbaren Belegen ist nicht ausschließbar. Collection-ETags sind Versionsbelege und keine stabilen Ordneridentitäten; nur die vollständig belegte eigene Namensänderung erlaubt eine neue unmittelbare Elternversion.",
];
const rows = [
  { oldName: "Alpha.txt", newName: "neu-Alpha-fertig.txt", status: "ready", message: "Bereit" },
  { oldName: "Beta.txt", newName: "neu-Beta-fertig.txt", status: "blocked", message: "Zielname ist belegt" },
];
async function mockBatch(page, config = {}) {
  await page.evaluate(({ rows, config }) => {
    const original = window.__TAURI__.core.invoke;
    window.__BATCH_CALLS__ = [];
    window.__BATCH_PREVIEW_HOLDS__ = [];
    let sequence = 0;
    const transport = config.smb ? { executionClass: "assured", limitations: ["Der Server schützt vorhandene Ziele mit atomarem No-Replace-Rename.", "Quellen- und Elternpfade werden frisch geprüft. Ein gleichzeitiger Quelltausch bis zum Rename bleibt möglich; der anschließende FileID-Readback prüft das beobachtbare Ergebnis."] } : config.webdav ? { executionClass: "best_effort", limitations: config.limitations || ["Die bedingte MOVE-Wirkung bindet starke Quell- und Eltern-ETags und sendet Overwrite: F. Missachtet ein Server diese Bedingungen, kann ein spätes Ziel ersetzt werden.", "Sitzung und Elternkette werden frisch geprüft; ein gleichzeitiger Vorfahrentausch kann erst nach der Wirkung als unklarer Ausgang erkannt werden."] } : config.ftps ? { executionClass: "best_effort", limitations: ["FTP/FTPS prüft das Ziel frisch vor genau einem Renameversuch. Eine gleichzeitig angelegte Zieldatei kann ersetzt werden.", "Quellen- und Elternpfade werden frisch geprüft. Ein gleichzeitiger serverseitiger Pfadtausch bleibt möglich; der Readback prüft beobachtbare Attribute, keine vollständige Objektidentität."] } : config.sftp ? { executionClass: "best_effort", limitations: ["Das Ziel wird vor dem einzelnen Renameversuch frisch geprüft. Eine gleichzeitig neu angelegte Zieldatei kann ersetzt werden.", "SFTP bindet Root und Eltern pfadbasiert. Ein gleichzeitiger serverseitiger Pfadtausch kann auf einen anderen Ort desselben Kontos umleiten."] } : {};
    window.__TAURI__.core.invoke = async (command, args) => {
      if (command === "scan_sftp_dir" && config.sftp || command === "scan_smb_dir" && config.smb || command === "scan_ftps_dir" && config.ftps || command === "scan_webdav_dir" && config.webdav) { const listing = await original(command, args); return { ...listing, entries: rows.map(row => ({ name: row.oldName, is_file: true, is_dir: false, is_symlink: false, size: 12, mtime: 1 })) }; }
      if (!command.includes("batch_rename") && command !== "cancel_sync") return original(command, args);
      window.__BATCH_CALLS__.push({ command, args });
      if (command === "prepare_batch_rename") {
        const ownSequence = ++sequence;
        if (config.previewHold) await new Promise(resolve => { window.__FINISH_BATCH_PREVIEW__ = resolve; window.__BATCH_PREVIEW_HOLDS__.push(resolve); });
        if (config.remoteError) throw { code: "unsupported", message: "Remote-Stapel sind noch nicht umgesetzt" };
        return { requestId: config.foreignPreview ? "foreign" : args.request.requestId, planId: config.distinctPlans ? "batch-" + ownSequence : "batch-one", rows, ...transport };
      }
      if (command === "execute_batch_rename") {
        if (config.executeHold) await new Promise(resolve => { window.__FINISH_BATCH_EXECUTE__ = resolve; });
        if (config.executeError) throw { code: "outcome_unclear", message: "Die erste Einzelwirkung kann bereits ausgeführt sein" };
        if (config.report) return { ...config.report, ...transport };
        const cancelled = config.executeHold;
        return { ...transport, rows: rows.map((row, index) => ({ ...row, status: cancelled ? "cancelled" : index ? "blocked" : "renamed", message: cancelled ? "Vor Wirkung abgebrochen" : index ? row.message : "Umbenannt" })), renamed: cancelled ? 0 : 1, failedUnchanged: 0, outcomeUnclear: 0, cancelled: cancelled ? 2 : 0 };
      }
      if (command === "cancel_batch_rename") { window.__FINISH_BATCH_EXECUTE__?.(); return true; }
      return null;
    };
  }, { rows: config.rows || rows, config });
}

async function openBatch(page, contextMenu = false) {
  const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
  await list.getByRole("option", { name: /Alpha\.txt/ }).click();
  await list.getByRole("option", { name: /Beta\.txt/ }).click({ modifiers: ["Meta"] });
  await list.focus();
  if (contextMenu) {
    await page.keyboard.press("ContextMenu");
    await page.getByRole("menuitem", { name: "Stapelumbenennen …", exact: true }).click();
  } else await page.evaluate(() => window.__MOCK_EMIT__("menu", "batch_rename"));
  const dialog = page.getByRole("dialog", { name: "Stapelumbenennen", exact: true });
  await dialog.waitFor(); return { list, dialog };
}

async function openRemoteBatch(page, protocol) {
  await page.locator(`#connection-list [data-protocol="${protocol}"][data-id="mock_${protocol}"]`).click();
  await page.getByRole("listbox", { name: /Linke Dateiliste/ }).getByRole("option", { name: /Alpha\.txt/ }).waitFor();
  return openBatch(page);
}
async function changeRemoteProfile(page, protocol) {
  await page.evaluate(protocol => {
    const revision = protocol === "smb" ? 42 : protocol === "ftps" ? 22 : protocol === "webdav" ? 32 : 12;
    window.__MOCK_SET_PROFILE_REVISION__(protocol, `mock_${protocol}`, revision);
    window.__MOCK_EMIT__("connections-updated", { protocol, profileId: `mock_${protocol}`, revision, change: "saved" });
  }, protocol);
}

export async function runBatchRenameCases(runCase) {
  await runCase("Stapelumbenennen · unklarer Zyklus nennt zusätzliche Recoverynamen", "?keyboard=1", async page => {
    await page.setViewportSize({ width: 390, height: 760 });
    const cycleRows = [
      { oldName: "ab", newName: "aab", status: "ready", message: "Lokaler Mehrschrittzyklus" },
      { oldName: "aab", newName: "aaab", status: "ready", message: "Lokaler Mehrschrittzyklus" },
      { oldName: "aaab", newName: "ab", status: "ready", message: "Lokaler Mehrschrittzyklus" },
    ];
    const recovery = "Zyklusausgang unklar. Mögliche betroffene Namen: „ab“, „aab“, „aaab“. Kein automatischer Rücktausch";
    await mockBatch(page, { rows: cycleRows, report: { renamed: 1, partiallyRenamed: 0, failedUnchanged: 0,
      outcomeUnclear: 2, cancelled: 0, rows: [
        { ...cycleRows[0], status: "renamed", currentName: "aab", message: "Zielname bestätigt" },
        { ...cycleRows[1], status: "outcome_unclear", message: recovery },
        { ...cycleRows[2], status: "outcome_unclear", message: recovery },
      ] } });
    const { dialog } = await openBatch(page);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await dialog.getByRole("button", { name: "3 Einträge umbenennen", exact: true }).click();
    await dialog.getByText(/Die Namen in den Spalten und zusätzliche Namenshinweise der unklaren Zeilen vor erneutem Versuch prüfen/).waitFor();
    const unknown = dialog.locator('tr[data-status="outcome_unclear"]').first();
    assert.equal(await unknown.locator("td").nth(0).textContent(), "aab");
    assert.equal(await unknown.locator("td").nth(1).textContent(), "aaab");
    const named = [...(await unknown.locator("td").nth(2).textContent()).matchAll(/„([^“]*)“/g)].map(match => match[1]);
    assert.deepEqual(named, ["ab", "aab", "aaab"]);
    assert.equal(await unknown.getByText(/Zuletzt bestätigter Name:/).count(), 0);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    await page.keyboard.press("Escape");
  });
  for (const theme of ["light", "dark"]) for (const width of [1180, 390]) {
    await runCase(`Stapelumbenennen · lokaler Mehrschrittzyklus mit Teilbilanz · ${theme} · ${width}`, "?keyboard=1", async page => {
      await page.setViewportSize({ width, height: 760 });
      await page.evaluate(theme => window.__MOCK_EMIT__("menu", `theme_${theme}`), theme);
      const cycleRows = [
        { oldName: "ab", newName: "aab", status: "ready", message: "Bereit; lokaler Zyklus mit 2 atomaren Austauschen. Abbruch kann Teiländerungen hinterlassen; kein automatischer Rücktausch; keine automatische Wiederaufnahme nach Prozessende" },
        { oldName: "aab", newName: "aaab", status: "ready", message: "Bereit; lokaler Mehrschrittzyklus" },
        { oldName: "aaab", newName: "ab", status: "ready", message: "Bereit; lokaler Mehrschrittzyklus" },
      ];
      const report = { renamed: 1, partiallyRenamed: 1, failedUnchanged: 0, outcomeUnclear: 0, cancelled: 1,
        rows: [
          { ...cycleRows[0], status: "renamed", currentName: "aab", message: "Zielname und Identität bestätigt" },
          { ...cycleRows[1], status: "partially_renamed", currentName: "ab", message: "Bereits bewegt, endgültiger Zielname noch nicht erreicht. Kein automatischer Rücktausch" },
          { ...cycleRows[2], status: "cancelled", currentName: "aaab", message: "Vor dem nächsten Zyklusschritt abgebrochen" },
        ] };
      await mockBatch(page, { rows: cycleRows, report, executeHold: true });
      const { dialog, list } = await openBatch(page);
      await dialog.getByText(/keine automatische Wiederaufnahme nach Prozessende/).waitFor();
      await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
      await dialog.getByText(cycleRows[0].message, { exact: true }).waitFor();
      await dialog.getByRole("button", { name: "3 Einträge umbenennen", exact: true }).click();
      await page.waitForFunction(() => typeof window.__FINISH_BATCH_EXECUTE__ === "function");
      // Eine offene Cancelantwort macht den konkreten Abbruchtext überprüfbar,
      // bevor die kontrollierte Teilbilanz an den echten Dialog geliefert wird.
      await page.evaluate(() => {
        const original = window.__TAURI__.core.invoke;
        window.__TAURI__.core.invoke = (command, args) => command === "cancel_batch_rename" ? Promise.resolve(true) : original(command, args);
      });
      await page.keyboard.press("Escape");
      await dialog.getByText(/weitere Wirkungen werden gestoppt.*Zwischennamen/).waitFor();
      await page.evaluate(() => window.__FINISH_BATCH_EXECUTE__());
      await dialog.getByText(/1 umbenannt · 1 teilweise umbenannt/).waitFor();
      const partial = dialog.locator('tr[data-status="partially_renamed"]');
      assert.equal(await partial.count(), 1);
      await partial.getByText("Teilweise umbenannt", { exact: true }).waitFor();
      await partial.getByText("Zuletzt bestätigter Name: ab", { exact: true }).waitFor();
      assert.equal(await dialog.locator("thead th").count(), 3);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
      const rect = await dialog.boundingBox(); assert.ok(rect.x >= 0 && rect.x + rect.width <= width);
      if (process.env.DOPPELDECKER_BATCH_SCREENSHOTS) {
        await mkdir(process.env.DOPPELDECKER_BATCH_SCREENSHOTS, { recursive: true });
        await partial.scrollIntoViewIfNeeded();
        await page.screenshot({ path: `${process.env.DOPPELDECKER_BATCH_SCREENSHOTS}/cycle-partial-${theme}-${width}.png` });
      }
      await page.keyboard.press("Escape"); assert.equal(await list.evaluate(node => node === document.activeElement), true);
    });
  }
  await runCase("Stapelumbenennen · alte Bilanz ohne currentName und unklarer Name", "?keyboard=1", async page => {
    const report = { renamed: 0, failedUnchanged: 1, outcomeUnclear: 1, cancelled: 0,
      rows: [{ ...rows[0], status: "outcome_unclear", currentName: "vermeintlicher-name", message: "Abschluss nicht sicher bestätigt" },
        { ...rows[1], status: "failed_unchanged", message: "Keine eigene Namensänderung" }] };
    await mockBatch(page, { report }); const { dialog } = await openBatch(page);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).click();
    await dialog.getByText(/0 teilweise umbenannt/).waitFor();
    assert.equal(await dialog.getByText(/Zuletzt bestätigter Name:/).count(), 0);
    assert.equal(await dialog.getByText("vermeintlicher-name", { exact: true }).count(), 0);
    await page.keyboard.press("Escape");
  });
  for (const theme of ["light", "dark"]) for (const width of [1180, 390]) {
    await runCase(`Stapelumbenennen · äußere Leerzeichen als reine Backendoption · ${theme} · ${width}`, "?keyboard=1&remoteFile=1", async page => {
      await page.setViewportSize({ width, height: 760 });
      await page.evaluate(theme => window.__MOCK_EMIT__("menu", `theme_${theme}`), theme);
      const spacedRows = rows.map((row, index) => ({ ...row, oldName: ` ${index ? "Beta" : "Alpha"}.txt `, newName: ` X ${index ? "Beta" : "Alpha"}.txt` }));
      await mockBatch(page, { webdav: true, limitations: webdavNumberingLimitations, rows: spacedRows }); const { dialog, list } = await openRemoteBatch(page, "webdav");
      const trim = dialog.getByRole("checkbox", { name: "Äußere Leerzeichen entfernen", exact: true });
      assert.equal(await trim.isChecked(), false);
      await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
      await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).waitFor();
      let request = await page.evaluate(() => window.__BATCH_CALLS__.find(call => call.command === "prepare_batch_rename").args.request);
      assert.equal(Object.hasOwn(request.options, "trimOuterSpaces"), false);
      await trim.focus(); await page.keyboard.press("Space");
      assert.equal(await dialog.getByRole("button", { name: /Eintr(?:ag|äge) umbenennen/ }).isDisabled(), true);
      await dialog.getByText("Nur Leerzeichen am Anfang und Ende des ursprünglichen Namens.", { exact: true }).waitFor();
      await dialog.getByLabel("Präfix", { exact: true }).fill(" X ");
      await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
      await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).waitFor();
      request = await page.evaluate(() => window.__BATCH_CALLS__.filter(call => call.command === "prepare_batch_rename").at(-1).args.request);
      assert.deepEqual(request.names, [" Alpha.txt ", " Beta.txt "]);
      assert.equal(request.options.trimOuterSpaces, true); assert.equal(request.options.prefix, " X ");
      assert.equal(await dialog.locator("tbody tr").first().locator("td").nth(0).textContent(), " Alpha.txt ");
      assert.equal(await dialog.locator("tbody tr").first().locator("td").nth(1).textContent(), " X Alpha.txt");
      assert.equal(await dialog.locator("tbody tr").first().locator("td").nth(0).evaluate(node => getComputedStyle(node).whiteSpace), "pre-wrap");
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
      if (process.env.DOPPELDECKER_TRIM_SCREENSHOTS) {
        await mkdir(process.env.DOPPELDECKER_TRIM_SCREENSHOTS, { recursive: true });
        await trim.scrollIntoViewIfNeeded();
        await page.screenshot({ path: `${process.env.DOPPELDECKER_TRIM_SCREENSHOTS}/trim-controls-${theme}-${width}.png` });
        await dialog.getByRole("region", { name: "Umbenennungen", exact: true }).scrollIntoViewIfNeeded();
        await page.screenshot({ path: `${process.env.DOPPELDECKER_TRIM_SCREENSHOTS}/trim-results-${theme}-${width}.png` });
      }
      await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).click();
      await dialog.getByText(/1 umbenannt/).waitFor();
      await page.keyboard.press("Escape"); assert.equal(await list.evaluate(node => node === document.activeElement), true);
    });
  }
  await runCase("Stapelumbenennen · Leerzeichenoption verwirft späte Antwort und bleibt im Lauf gesperrt", "?keyboard=1", async page => {
    await mockBatch(page, { previewHold: true, executeHold: true }); const { dialog } = await openBatch(page);
    const trim = dialog.getByRole("checkbox", { name: "Äußere Leerzeichen entfernen", exact: true });
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click(); await page.waitForFunction(() => window.__BATCH_PREVIEW_HOLDS__.length === 1);
    await trim.check(); await page.evaluate(() => window.__BATCH_PREVIEW_HOLDS__[0]());
    await page.waitForFunction(() => window.__BATCH_CALLS__.some(call => call.command === "discard_batch_rename" && call.args.planId === "batch-one"));
    assert.equal(await dialog.getByRole("button", { name: /Eintr(?:ag|äge) umbenennen/ }).isDisabled(), true);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click(); await page.waitForFunction(() => window.__BATCH_PREVIEW_HOLDS__.length === 2);
    await page.evaluate(() => window.__BATCH_PREVIEW_HOLDS__[1]()); await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).click();
    await page.waitForFunction(() => typeof window.__FINISH_BATCH_EXECUTE__ === "function"); assert.equal(await trim.isDisabled(), true);
    await page.keyboard.press("Escape"); await dialog.getByText(/2 abgebrochen/).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__BATCH_CALLS__.filter(call => call.command === "cancel_batch_rename").map(call => call.args)), [{ planId: "batch-one" }]);
    await page.keyboard.press("Escape");
  });
  await runCase("Stapelumbenennen · Nummerierung friert sichtbare Reihenfolge statt Klickreihenfolge", "?keyboard=1", async page => {
    await mockBatch(page);
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /Beta\.txt/ }).click();
    await list.getByRole("option", { name: /Alpha\.txt/ }).click({ modifiers: ["Meta"] });
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "batch_rename"));
    const dialog = page.getByRole("dialog", { name: "Stapelumbenennen", exact: true });
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).waitFor();
    const request = await page.evaluate(() => window.__BATCH_CALLS__.find(call => call.command === "prepare_batch_rename").args.request);
    assert.deepEqual(request.names, ["Alpha.txt", "Beta.txt"]);
    assert.equal(Object.hasOwn(request.options, "numbering"), false);
    await page.keyboard.press("Escape");
  });
  for (const theme of ["light", "dark"]) for (const width of [1180, 390]) {
    await runCase(`Stapelumbenennen · Nummerierung und lange WebDAV-Grenzen · ${theme} · ${width}`, "?keyboard=1&remoteFile=1", async page => {
      await page.setViewportSize({ width, height: 760 });
      await page.evaluate(theme => window.__MOCK_EMIT__("menu", `theme_${theme}`), theme);
      const numberedRows = rows.map((row, index) => ({ ...row, newName: `neu-${index ? "Beta" : "Alpha"}-fertig_${index ? "0007" : "0000"}.txt` }));
      await mockBatch(page, { webdav: true, limitations: webdavNumberingLimitations, rows: numberedRows });
      const { dialog, list } = await openRemoteBatch(page, "webdav");
      const numbering = dialog.getByRole("group", { name: "Nummerierung", exact: true });
      const toggle = numbering.getByRole("checkbox", { name: "Nummerierung verwenden", exact: true });
      assert.equal(await toggle.isChecked(), false);
      assert.equal(await numbering.getByRole("spinbutton", { name: "Start", exact: true }).count(), 0);
      await toggle.focus(); await page.keyboard.press("Space");
      assert.equal(await numbering.getByRole("spinbutton", { name: "Start", exact: true }).inputValue(), "1");
      assert.equal(await numbering.getByRole("spinbutton", { name: "Schritt", exact: true }).inputValue(), "1");
      assert.equal(await numbering.getByRole("spinbutton", { name: "Mindeststellen", exact: true }).inputValue(), "3");
      await page.keyboard.press("Tab");
      assert.equal(await numbering.getByRole("spinbutton", { name: "Start", exact: true }).evaluate(node => node === document.activeElement), true);
      await numbering.getByRole("spinbutton", { name: "Start", exact: true }).fill("0");
      await numbering.getByRole("spinbutton", { name: "Schritt", exact: true }).fill("7");
      await numbering.getByRole("spinbutton", { name: "Mindeststellen", exact: true }).fill("4");
      await numbering.getByRole("combobox", { name: "Position", exact: true }).selectOption("after");
      await dialog.getByLabel("Präfix", { exact: true }).fill("neu-"); await dialog.getByLabel("Suffix", { exact: true }).fill("-fertig");
      await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
      const result = dialog.getByRole("region", { name: "Umbenennungen", exact: true });
      await dialog.getByRole("cell", { name: "neu-Alpha-fertig_0000.txt", exact: true }).waitFor();
      assert.equal(await result.evaluate(node => node === document.activeElement), true);
      assert.equal(await result.evaluate(node => {
        const body = node.closest(".modal-body").getBoundingClientRect(); const rect = node.getBoundingClientRect();
        return rect.bottom <= body.bottom + 1 && rect.top >= body.top - 1;
      }), true, "Die Ergebnisregion muss nach dem vollständigen Status im sichtbaren Modalbereich liegen");
      const request = await page.evaluate(() => window.__BATCH_CALLS__.find(call => call.command === "prepare_batch_rename").args.request);
      assert.deepEqual(request.options.numbering, { start: 0, step: 7, minDigits: 4, position: "after" });
      assert.deepEqual(request.names, ["Alpha.txt", "Beta.txt"]);
      const limits = dialog.getByRole("region", { name: "Ausführungsgrenzen", exact: true });
      assert.equal(await limits.getByText(webdavNumberingLimitations[0], { exact: true }).isVisible(), true);
      assert.equal(await limits.getByText(webdavNumberingLimitations[1], { exact: true }).isVisible(), true);
      const summary = limits.locator("summary"); assert.equal(await summary.innerText(), "Weitere Ausführungsgrenzen (3)");
      assert.equal(await limits.locator("details").evaluate(node => node.open), false);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
      const rect = await dialog.boundingBox(); assert.ok(rect.x >= 0 && rect.x + rect.width <= width);
      if (process.env.DOPPELDECKER_BATCH_SCREENSHOTS) {
        await mkdir(process.env.DOPPELDECKER_BATCH_SCREENSHOTS, { recursive: true });
        await page.screenshot({ path: `${process.env.DOPPELDECKER_BATCH_SCREENSHOTS}/numbering-results-${theme}-${width}.png` });
        await numbering.scrollIntoViewIfNeeded();
        await page.screenshot({ path: `${process.env.DOPPELDECKER_BATCH_SCREENSHOTS}/numbering-controls-${theme}-${width}.png` });
      }
      await summary.focus(); await page.keyboard.press("Enter"); assert.equal(await limits.locator("details").evaluate(node => node.open), true);
      for (const message of webdavNumberingLimitations) assert.equal(await limits.getByText(message, { exact: true }).isVisible(), true);
      await page.keyboard.press("Space"); assert.equal(await limits.locator("details").evaluate(node => node.open), false);
      await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).click();
      await dialog.getByText(/1 umbenannt/).waitFor();
      await page.keyboard.press("Escape"); assert.equal(await list.evaluate(node => node === document.activeElement), true);
    });
  }
  await runCase("Stapelumbenennen · Nummerierungseingaben verwerfen jeden vorbereiteten Plan", "?keyboard=1", async page => {
    await mockBatch(page, { distinctPlans: true }); const { dialog } = await openBatch(page);
    await dialog.getByRole("checkbox", { name: "Nummerierung verwenden", exact: true }).check();
    const changes = [
      () => dialog.getByRole("spinbutton", { name: "Start", exact: true }).fill("2"),
      () => dialog.getByRole("spinbutton", { name: "Schritt", exact: true }).fill("3"),
      () => dialog.getByRole("spinbutton", { name: "Mindeststellen", exact: true }).fill("4"),
      () => dialog.getByRole("combobox", { name: "Position", exact: true }).selectOption("after"),
      () => dialog.getByRole("checkbox", { name: "Nummerierung verwenden", exact: true }).uncheck(),
    ];
    for (let index = 0; index < changes.length; index++) {
      await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
      await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).waitFor();
      await changes[index]();
      assert.equal(await dialog.getByRole("button", { name: /Eintr(?:ag|äge) umbenennen/ }).isDisabled(), true);
      assert.ok(await page.evaluate(id => window.__BATCH_CALLS__.some(call => call.command === "discard_batch_rename" && call.args.planId === id), `batch-${index + 1}`));
    }
    await page.keyboard.press("Escape");
  });
  await runCase("Stapelumbenennen · Nummerierung korrigiert ungültige Zahlen vor neuer Anfrage", "?keyboard=1", async page => {
    await mockBatch(page); const { dialog } = await openBatch(page);
    await dialog.getByRole("checkbox", { name: "Nummerierung verwenden", exact: true }).check();
    const step = dialog.getByRole("spinbutton", { name: "Schritt", exact: true });
    await step.fill("0"); await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await dialog.getByText("Bitte die markierte Nummerierungseingabe korrigieren.", { exact: true }).waitFor();
    assert.equal(await step.evaluate(node => node === document.activeElement), true);
    assert.equal(await page.evaluate(() => window.__BATCH_CALLS__.filter(call => call.command === "prepare_batch_rename").length), 0);
    await step.fill("1"); await dialog.getByRole("spinbutton", { name: "Start", exact: true }).fill("0");
    await dialog.getByRole("spinbutton", { name: "Mindeststellen", exact: true }).fill("21");
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    assert.equal(await page.evaluate(() => window.__BATCH_CALLS__.filter(call => call.command === "prepare_batch_rename").length), 0);
    await dialog.getByRole("spinbutton", { name: "Mindeststellen", exact: true }).fill("20");
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).waitFor();
    await page.keyboard.press("Escape");
  });
  await runCase("Stapelumbenennen · Nummerierung verwirft späte Antwort und bleibt im Lauf gesperrt", "?keyboard=1", async page => {
    await mockBatch(page, { previewHold: true, executeHold: true }); const { dialog } = await openBatch(page);
    await dialog.getByRole("checkbox", { name: "Nummerierung verwenden", exact: true }).check();
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click(); await page.waitForFunction(() => window.__BATCH_PREVIEW_HOLDS__.length === 1);
    await dialog.getByRole("spinbutton", { name: "Start", exact: true }).fill("2");
    await page.evaluate(() => window.__BATCH_PREVIEW_HOLDS__[0]());
    await page.waitForFunction(() => window.__BATCH_CALLS__.some(call => call.command === "discard_batch_rename" && call.args.planId === "batch-one"));
    assert.equal(await dialog.getByRole("button", { name: /Eintr(?:ag|äge) umbenennen/ }).isDisabled(), true);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click(); await page.waitForFunction(() => window.__BATCH_PREVIEW_HOLDS__.length === 2);
    await page.evaluate(() => window.__BATCH_PREVIEW_HOLDS__[1]()); await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).click();
    await page.waitForFunction(() => typeof window.__FINISH_BATCH_EXECUTE__ === "function");
    for (const label of ["Start", "Schritt", "Mindeststellen"]) assert.equal(await dialog.getByRole("spinbutton", { name: label, exact: true }).isDisabled(), true);
    assert.equal(await dialog.getByRole("combobox", { name: "Position", exact: true }).isDisabled(), true);
    assert.equal(await dialog.getByRole("checkbox", { name: "Nummerierung verwenden", exact: true }).isDisabled(), true);
    await page.keyboard.press("Escape"); await dialog.getByText(/2 abgebrochen/).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__BATCH_CALLS__.filter(call => call.command === "cancel_batch_rename").map(call => call.args)), [{ planId: "batch-one" }]);
    await page.keyboard.press("Escape");
  });
  for (const theme of ["light", "dark"]) for (const width of [1180, 440]) {
    await runCase(`Stapelumbenennen · konkrete Vorschau und Teilbilanz · ${theme} · ${width}`, "?keyboard=1", async page => {
      await page.setViewportSize({ width, height: 760 });
      await page.evaluate(theme => window.__MOCK_EMIT__("menu", `theme_${theme}`), theme);
      await mockBatch(page);
      const { list, dialog } = await openBatch(page, true);
      await dialog.getByLabel("Suchen", { exact: true }).fill("alt");
      await dialog.getByLabel("Ersetzen durch", { exact: true }).fill("neu");
      await dialog.getByLabel("Präfix", { exact: true }).fill("neu-");
      await dialog.getByLabel("Suffix", { exact: true }).fill("-fertig");
      await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
      await dialog.getByRole("cell", { name: "neu-Alpha-fertig.txt", exact: true }).waitFor();
      const request = await page.evaluate(() => window.__BATCH_CALLS__.find(call => call.command === "prepare_batch_rename").args.request);
      assert.deepEqual(request.names, ["Alpha.txt", "Beta.txt"]);
      assert.deepEqual(request.options, { find: "alt", replacement: "neu", prefix: "neu-", suffix: "-fertig", preserveExtension: true });
      const rect = await dialog.boundingBox(); assert.ok(rect.x >= 0 && rect.x + rect.width <= width);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
      if (process.env.DOPPELDECKER_BATCH_SCREENSHOTS) {
        await mkdir(process.env.DOPPELDECKER_BATCH_SCREENSHOTS, { recursive: true });
        await page.screenshot({ path: `${process.env.DOPPELDECKER_BATCH_SCREENSHOTS}/batch-${theme}-${width}.png` });
      }
      await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).click();
      await dialog.getByText(/1 umbenannt · 0 teilweise umbenannt · 0 Fehler ohne Namensänderung/).waitFor();
      await dialog.locator('tr[data-status="renamed"]').waitFor();
      await page.keyboard.press("Escape");
      assert.equal(await dialog.isVisible(), false);
      assert.equal(await list.evaluate(node => node === document.activeElement), true);
    });
  }

  await runCase("Stapelumbenennen · veränderte Eingabe verwirft verspätete Vorschau", "?keyboard=1", async page => {
    await mockBatch(page, { previewHold: true });
    const { dialog } = await openBatch(page);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await page.waitForFunction(() => typeof window.__FINISH_BATCH_PREVIEW__ === "function");
    await dialog.getByLabel("Präfix", { exact: true }).fill("anderes-");
    await page.evaluate(() => window.__FINISH_BATCH_PREVIEW__());
    await page.waitForFunction(() => window.__BATCH_CALLS__.some(call => call.command === "discard_batch_rename"));
    assert.equal(await dialog.getByRole("button", { name: /Eintr(?:ag|äge) umbenennen/ }).isDisabled(), true);
    assert.equal(await dialog.getByRole("cell").count(), 0);
    await page.keyboard.press("Escape");
  });

  await runCase("Stapelumbenennen · Escape direkt nach Execute trifft nur eigene planId", "?keyboard=1", async page => {
    await mockBatch(page, { executeHold: true, rows: rows.map(row => ({ ...row, status: "ready", message: "Bereit" })) });
    const { dialog } = await openBatch(page);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await dialog.getByRole("button", { name: "2 Einträge umbenennen", exact: true }).click();
    await page.keyboard.press("Escape");
    await dialog.getByText(/0 umbenannt.*2 abgebrochen/).waitFor();
    const calls = await page.evaluate(() => window.__BATCH_CALLS__);
    assert.deepEqual(calls.filter(call => call.command === "cancel_batch_rename").map(call => call.args), [{ planId: "batch-one" }]);
    assert.equal(calls.filter(call => call.command === "cancel_sync").length, 0);
    assert.equal(calls.filter(call => call.command === "execute_batch_rename").length, 1);
    await page.keyboard.press("Escape");
  });

  await runCase("Stapelumbenennen · Teilabbruch erhält tatsächlichen ersten Erfolg", "?keyboard=1", async page => {
    const readyRows = rows.map(row => ({ ...row, status: "ready", message: "Bereit" }));
    await mockBatch(page, { rows: readyRows, report: { rows: readyRows.map((row, index) => ({ ...row, status: index ? "cancelled" : "renamed", message: index ? "Vor Einzelwirkung abgebrochen" : "Umbenannt" })), renamed: 1, failedUnchanged: 0, outcomeUnclear: 0, cancelled: 1 } });
    const { dialog } = await openBatch(page);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await dialog.getByRole("button", { name: "2 Einträge umbenennen", exact: true }).click();
    await dialog.getByText(/1 umbenannt.*1 abgebrochen/).waitFor();
    assert.equal(await dialog.locator('tr[data-status="renamed"]').count(), 1);
    assert.equal(await dialog.locator('tr[data-status="cancelled"]').count(), 1);
    await page.keyboard.press("Escape");
  });

  await runCase("Stapelumbenennen · Remoteumfang wird vom Backend erklärt", "?remoteFile=1", async page => {
    await mockBatch(page, { remoteError: true });
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote\.txt/ }).click();
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "batch_rename"));
    const dialog = page.getByRole("dialog", { name: "Stapelumbenennen", exact: true });
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await dialog.getByText("Remote-Stapel sind noch nicht umgesetzt", { exact: true }).waitFor();
    const request = await page.evaluate(() => window.__BATCH_CALLS__.find(call => call.command === "prepare_batch_rename").args.request);
    assert.equal(request.endpoint.provider, "sftp");
    assert.equal(await dialog.getByRole("button", { name: /Eintr(?:ag|äge) umbenennen/ }).isDisabled(), true);
    await page.keyboard.press("Escape");
  });

  await runCase("Stapelumbenennen · unbestätigter Abschluss behauptet keine unveränderte Gesamtwirkung", "?keyboard=1", async page => {
    await mockBatch(page, { executeError: true });
    const { dialog } = await openBatch(page);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).click();
    await dialog.getByText(/Abschluss nicht bestätigt.*Mögliche Teilwirkungen prüfen/).waitFor();
    assert.equal((await page.evaluate(() => window.__BATCH_CALLS__)).filter(call => call.command === "execute_batch_rename").length, 1);
    assert.equal(await dialog.getByRole("button", { name: /Eintr(?:ag|äge) umbenennen/ }).count(), 0);
    await page.keyboard.press("Escape");
  });

  await runCase("Stapelumbenennen · geschlossene Vorschau verdrängt keinen fremden Dialog", "?keyboard=1", async page => {
    await mockBatch(page, { previewHold: true });
    const { dialog, list } = await openBatch(page);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await page.waitForFunction(() => typeof window.__FINISH_BATCH_PREVIEW__ === "function");
    await page.keyboard.press("Escape"); await list.focus(); await page.keyboard.press("F2");
    await page.locator("#modal-input").waitFor();
    await page.evaluate(() => window.__FINISH_BATCH_PREVIEW__());
    await page.waitForFunction(() => window.__BATCH_CALLS__.some(call => call.command === "discard_batch_rename"));
    assert.match(await page.locator("#modal-title").innerText(), /Umbenennen/);
    await page.keyboard.press("Escape");
  });
  await runCase("Stapelumbenennen · Klartext-FTP nutzt gebundenes Profil und Best Effort", "?keyboard=1&remoteFile=1", async page => {
    await mockBatch(page, { ftps: true });
    await page.evaluate(() => { window.__MOCK_IMPORT_FTP_PROFILE__(); window.dispatchEvent(new Event("focus")); });
    const profile = page.locator('#connection-list [data-protocol="ftps"][data-id="cli_ftp"]');
    await profile.waitFor(); assert.match(await profile.innerText(), /FTP · unverschlüsselt/); await profile.click();
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).getByRole("option", { name: /Alpha\.txt/ }).waitFor();
    const { dialog } = await openBatch(page);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await dialog.getByText("Ausführungsklasse: Best Effort", { exact: true }).waitFor();
    const request = await page.evaluate(() => window.__BATCH_CALLS__.find(call => call.command === "prepare_batch_rename").args.request);
    assert.deepEqual(request.endpoint, { provider: "ftps", profileId: "cli_ftp", profileRevision: 52, root: "/" });
    await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).click();
    await dialog.getByText(/1 umbenannt/).waitFor(); await page.keyboard.press("Escape");
  });
  for (const protocol of ["ftps", "webdav"]) await runCase(`Stapelumbenennen · ${protocol.toUpperCase()} unklarer Ausgang bleibt Teilbilanz ohne Blind-Retry`, "?keyboard=1&remoteFile=1", async page => {
    const readyRows = rows.map(row => ({ ...row, status: "ready" }));
    await mockBatch(page, { [protocol]: true, rows: readyRows, report: { rows: readyRows.map((row, index) => ({ ...row, status: index ? "cancelled" : "outcome_unclear", message: index ? "Nach unklarem Ausgang gestoppt" : "Antwort der Einzelwirkung fehlt; Ergebnis nicht belegbar" })), renamed: 0, failedUnchanged: 0, outcomeUnclear: 1, cancelled: 1 } });
    const { dialog } = await openRemoteBatch(page, protocol);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
    await dialog.getByRole("button", { name: "2 Einträge umbenennen", exact: true }).click();
    await dialog.getByText(/0 umbenannt.*1 unklar.*1 abgebrochen/).waitFor();
    assert.equal(await dialog.locator('tr[data-status="outcome_unclear"]').count(), 1);
    assert.equal(await dialog.locator('tr[data-status="cancelled"]').count(), 1);
    assert.equal(await dialog.getByRole("button", { name: /Eintr(?:ag|äge) umbenennen/ }).count(), 0);
    assert.equal(await page.evaluate(() => window.__BATCH_CALLS__.filter(call => call.command === "execute_batch_rename").length), 1);
    await page.keyboard.press("Escape");
  });
  for (const protocol of ["sftp", "smb", "ftps", "webdav"]) {
  const remoteName = protocol.toUpperCase();
  const classLabel = protocol === "smb" ? "Ausführungsklasse: abgesicherter Zielschutz" : "Ausführungsklasse: Best Effort";
  for (const theme of ["light", "dark"]) for (const width of [1180, 390]) {
    await runCase(`Stapelumbenennen · ${remoteName} Ausführungsklasse Vorschau und Bilanz · ${theme} · ${width}`, "?keyboard=1&remoteFile=1", async page => {
      await page.setViewportSize({ width, height: 850 }); await page.evaluate(theme => window.__MOCK_EMIT__("menu", `theme_${theme}`), theme);
      await mockBatch(page, { [protocol]: true }); const { dialog, list } = await openRemoteBatch(page, protocol);
      await dialog.getByLabel("Präfix", { exact: true }).fill("neu-"); await dialog.getByLabel("Suffix", { exact: true }).fill("-fertig"); await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
      await dialog.getByText(classLabel, { exact: true }).waitFor();
      assert.doesNotMatch(await dialog.locator(".batch-rename-status").innerText(), /Identitäts-Readback/);
      const request = await page.evaluate(() => window.__BATCH_CALLS__.find(call => call.command === "prepare_batch_rename").args.request);
      assert.equal(request.endpoint.provider, protocol); assert.equal(request.endpoint.profileId, `mock_${protocol}`); assert.equal(request.endpoint.profileRevision, protocol === "smb" ? 41 : protocol === "ftps" ? 21 : protocol === "webdav" ? 31 : 11); assert.ok(request.requestId.length > 10);
      assert.equal(await dialog.getByRole("region", { name: "Ausführungsgrenzen", exact: true }).getByText(protocol === "smb" ? /FileID-Readback/ : protocol === "webdav" ? /bedingte MOVE-Wirkung/ : /serverseitiger Pfadtausch/).count(), 1);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
      if (process.env.DOPPELDECKER_BATCH_SCREENSHOTS) { await mkdir(process.env.DOPPELDECKER_BATCH_SCREENSHOTS, { recursive: true }); await page.mouse.move(0, 0); await page.screenshot({ path: `${process.env.DOPPELDECKER_BATCH_SCREENSHOTS}/${protocol}-preview-${theme}-${width}.png` }); }
      if (width < 600) {
        await dialog.getByRole("region", { name: "Umbenennungen", exact: true }).scrollIntoViewIfNeeded();
        await dialog.getByRole("region", { name: "Umbenennungen", exact: true }).focus();
        assert.equal(await dialog.locator('tr[data-status="ready"]').count(), 1);
        if (process.env.DOPPELDECKER_BATCH_SCREENSHOTS) await page.screenshot({ path: `${process.env.DOPPELDECKER_BATCH_SCREENSHOTS}/${protocol}-preview-rows-${theme}-${width}.png` });
      }
      await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).click(); await dialog.getByText(/1 umbenannt · 0 teilweise umbenannt · 0 Fehler ohne Namensänderung/).waitFor();
      await dialog.getByText(classLabel, { exact: true }).waitFor();
      assert.equal(await dialog.getByRole("button", { name: /Eintr(?:ag|äge) umbenennen/ }).count(), 0);
      assert.equal(await page.evaluate(() => document.activeElement.textContent), "Schließen");
      if (process.env.DOPPELDECKER_BATCH_SCREENSHOTS) await page.screenshot({ path: `${process.env.DOPPELDECKER_BATCH_SCREENSHOTS}/${protocol}-report-${theme}-${width}.png` });
      await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click();
      await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).waitFor();
      assert.equal(await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).isEnabled(), true);
      await page.keyboard.press("Escape"); assert.equal(await list.evaluate(node => node === document.activeElement), true);
    });
  }
  await runCase(`Stapelumbenennen · ${remoteName} früher Prepareabbruch und spätes A nach B`, "?keyboard=1&remoteFile=1", async page => {
    await mockBatch(page, { [protocol]: true, previewHold: true, distinctPlans: true }); let { dialog } = await openRemoteBatch(page, protocol);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click(); await page.waitForFunction(() => window.__BATCH_PREVIEW_HOLDS__.length === 1);
    const id = await page.evaluate(() => window.__BATCH_CALLS__.find(call => call.command === "prepare_batch_rename").args.request.requestId);
    await page.keyboard.press("Escape"); assert.ok(await page.evaluate(id => window.__BATCH_CALLS__.some(call => call.command === "discard_batch_rename" && call.args.planId === id), id));
    ({ dialog } = await openBatch(page)); await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click(); await page.waitForFunction(() => window.__BATCH_PREVIEW_HOLDS__.length === 2);
    await page.evaluate(() => window.__BATCH_PREVIEW_HOLDS__[1]()); await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).waitFor();
    await page.evaluate(() => window.__BATCH_PREVIEW_HOLDS__[0]()); await page.waitForFunction(() => window.__BATCH_CALLS__.some(call => call.command === "discard_batch_rename" && call.args.planId === "batch-1"));
    await dialog.getByRole("button", { name: "1 Eintrag umbenennen", exact: true }).click(); await dialog.getByText(/1 umbenannt/).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__BATCH_CALLS__.filter(call => call.command === "execute_batch_rename").map(call => call.args)), [{ planId: "batch-2" }]); await page.keyboard.press("Escape");
  });
  await runCase(`Stapelumbenennen · ${remoteName} Profilwechsel verwirft pending und späte Vorschau`, "?keyboard=1&remoteFile=1", async page => {
    await mockBatch(page, { [protocol]: true, previewHold: true }); const { dialog } = await openRemoteBatch(page, protocol);
    await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click(); await page.waitForFunction(() => window.__BATCH_PREVIEW_HOLDS__.length === 1);
    await changeRemoteProfile(page, protocol); await dialog.getByText(/Auswahl, Pane oder Profil geändert/).waitFor(); await page.evaluate(() => window.__BATCH_PREVIEW_HOLDS__[0]());
    await page.waitForFunction(() => window.__BATCH_CALLS__.some(call => call.command === "discard_batch_rename" && call.args.planId === "batch-one")); assert.equal(await dialog.getByRole("button", { name: /Eintr(?:ag|äge) umbenennen/ }).isDisabled(), true);
    assert.equal(await page.evaluate(() => window.__BATCH_CALLS__.filter(call => call.command === "execute_batch_rename").length), 0); await page.keyboard.press("Escape");
  });
  await runCase(`Stapelumbenennen · ${remoteName} Profilwechsel stoppt exakt eigenen Lauf und erhält ersten Erfolg`, "?keyboard=1&remoteFile=1", async page => {
    const readyRows = rows.map(row => ({ ...row, status: "ready" }));
    await mockBatch(page, { [protocol]: true, executeHold: true, rows: readyRows, report: { rows: readyRows.map((row, index) => ({ ...row, status: index ? "cancelled" : "renamed", message: index ? "Profilbindung geändert; Rest unverändert" : "Umbenannt" })), renamed: 1, failedUnchanged: 0, outcomeUnclear: 0, cancelled: 1 } });
    const { dialog } = await openRemoteBatch(page, protocol); await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click(); await dialog.getByRole("button", { name: "2 Einträge umbenennen", exact: true }).click(); await page.waitForFunction(() => typeof window.__FINISH_BATCH_EXECUTE__ === "function");
    await changeRemoteProfile(page, protocol); await dialog.getByText(/1 umbenannt.*1 abgebrochen/).waitFor(); assert.equal(await dialog.locator('tr[data-status="renamed"]').count(), 1); assert.equal(await dialog.locator('tr[data-status="cancelled"]').count(), 1);
    assert.deepEqual(await page.evaluate(() => window.__BATCH_CALLS__.filter(call => call.command === "cancel_batch_rename").map(call => call.args)), [{ planId: "batch-one" }]); assert.equal(await page.evaluate(() => window.__BATCH_CALLS__.filter(call => call.command === "cancel_sync").length), 0); await page.keyboard.press("Escape");
  });
  await runCase(`Stapelumbenennen · ${remoteName} fremde Antwort hat keine Ausführungsfreigabe`, "?keyboard=1&remoteFile=1", async page => {
    await mockBatch(page, { [protocol]: true, foreignPreview: true }); const { dialog } = await openRemoteBatch(page, protocol); await dialog.getByRole("button", { name: "Vorschau erstellen", exact: true }).click(); await dialog.getByText(/Stapelantwort gehört zu einer anderen Anfrage/).waitFor(); assert.equal(await dialog.getByRole("button", { name: /Eintr(?:ag|äge) umbenennen/ }).isDisabled(), true); await page.keyboard.press("Escape");
  });

  }
}
