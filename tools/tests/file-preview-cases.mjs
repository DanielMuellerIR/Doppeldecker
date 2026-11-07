import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";

async function mockPreview(page, result, { hold = false, error = null } = {}) {
  await page.evaluate(({ result, hold, error }) => {
    const original = window.__TAURI__.core.invoke;
    window.__TAURI__.core.invoke = async (command, args) => {
      if (command !== "prepare_file_preview") return original(command, args);
      window.__PREVIEW_REQUEST__ = args.request;
      if (hold) await new Promise(resolve => { window.__FINISH_PREVIEW__ = resolve; });
      if (error) throw error;
      return result;
    };
  }, { result, hold, error });
}

async function selectFile(page) {
  const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
  await list.getByRole("option", { name: /Beispiel\.txt/ }).click();
  await list.focus();
  return list;
}

export async function runFilePreviewCases(runCase) {
  for (const theme of ["light", "dark"]) for (const width of [1180, 440]) {
    await runCase(`Integrierte Vorschau · passiver Text · ${theme} · ${width}`, "", async page => {
      await page.setViewportSize({ width, height: 760 });
      await page.evaluate(theme => window.__MOCK_EMIT__("menu", `theme_${theme}`), theme);
      const text = '<script>window.previewExecuted = true</script>\n<svg onload="alert(1)">\n' + "Lange Zeile ohne Umbruch ".repeat(40);
      await mockPreview(page, { bytes: text.length, content: { kind: "text", text }, limitations: [] });
      const list = await selectFile(page);
      await page.keyboard.press("F3");
      const dialog = page.getByRole("dialog", { name: /Vorschau/ });
      const content = dialog.getByLabel("Dateiinhalt als Text");
      await content.waitFor();
      assert.equal(await content.textContent(), text);
      assert.equal(await content.locator("script, svg").count(), 0);
      const request = await page.evaluate(() => window.__PREVIEW_REQUEST__);
      assert.equal(request.endpoint.provider, "local");
      assert.equal(request.path, `${request.endpoint.root}/Beispiel.txt`);
      const rect = await dialog.boundingBox();
      assert.ok(rect.x >= 0 && rect.x + rect.width <= width);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth), false);
      await dialog.getByRole("button", { name: "Mit System-App öffnen" }).focus();
      await page.keyboard.press("Tab");
      assert.equal(await dialog.getByLabel("Dateiinhalt als Text").evaluate(node => node === document.activeElement), true);
      if (process.env.DOPPELDECKER_PREVIEW_SCREENSHOTS) {
        await mkdir(process.env.DOPPELDECKER_PREVIEW_SCREENSHOTS, { recursive: true });
        await page.screenshot({ path: `${process.env.DOPPELDECKER_PREVIEW_SCREENSHOTS}/preview-text-${theme}-${width}.png` });
      }
      await page.keyboard.press("Escape");
      assert.equal(await dialog.isVisible(), false);
      assert.equal(await list.evaluate(node => node === document.activeElement), true);
    });
  }

  await runCase("Integrierte Vorschau · Rasterbild unter Produktions-CSP", "", async page => {
    const data_url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aV1kAAAAASUVORK5CYII=";
    await mockPreview(page, { bytes: 68, content: { kind: "image", data_url, width: 1, height: 1 }, limitations: [] });
    await selectFile(page);
    await page.keyboard.press("ContextMenu");
    await page.getByRole("menuitem", { name: "Vorschau (F3)", exact: true }).click();
    const image = page.getByRole("dialog").getByRole("img");
    await image.waitFor();
    await page.waitForFunction(() => document.querySelector(".file-preview-image img")?.naturalWidth === 1);
    await page.keyboard.press("Escape");
  });

  await runCase("Integrierte Vorschau · Fehler und System-App-Alternative", "", async page => {
    await mockPreview(page, null, { error: { code: "invalid_input", message: "Die integrierte Vorschau liest höchstens 8 MiB." } });
    await selectFile(page);
    await page.evaluate(() => window.__MOCK_EMIT__("menu", "file_preview"));
    const dialog = page.getByRole("dialog", { name: /Vorschau/ });
    await dialog.getByText(/höchstens 8 MiB/).waitFor();
    await dialog.getByRole("button", { name: "Mit System-App öffnen" }).click();
    await page.waitForFunction(() => window.__DOPPELDECKER_LAST_OPENED__?.endsWith("/Beispiel.txt"));
  });

  await runCase("Integrierte Vorschau · Remote-Endpunkt und sichtbare Einschränkung", "?remoteFile=1", async page => {
    await mockPreview(page, { bytes: 3, content: { kind: "text", text: "remote" }, limitations: ["Best Effort: keine atomare Rootbindung"] });
    await page.locator('#connection-list [data-protocol="sftp"][data-id="mock_sftp"]').click();
    const list = page.getByRole("listbox", { name: /Linke Dateiliste/ });
    await list.getByRole("option", { name: /remote\.txt/ }).click();
    await list.focus();
    await page.keyboard.press("F3");
    await page.getByRole("dialog").getByText("Best Effort: keine atomare Rootbindung").waitFor();
    const request = await page.evaluate(() => window.__PREVIEW_REQUEST__);
    assert.deepEqual(request, { endpoint: { provider: "sftp", profileId: "mock_sftp", profileRevision: 11, root: "/" }, path: "/remote.txt" });
    await page.keyboard.press("Escape");
  });

  await runCase("Integrierte Vorschau · ungeeigneter Inhalt bleibt erklärbar", "", async page => {
    await mockPreview(page, { bytes: 5, content: { kind: "unsupported", reason: "Keine passive Vorschau für diese Binärdatei verfügbar." }, limitations: [] });
    await selectFile(page);
    await page.keyboard.press("F3");
    const dialog = page.getByRole("dialog");
    await dialog.getByText(/Binärdatei/).waitFor();
    assert.equal(await dialog.getByRole("button", { name: "Mit System-App öffnen" }).isEnabled(), true);
    await page.keyboard.press("Escape");
  });

  await runCase("Integrierte Vorschau · geschlossener Dialog ignoriert späte Antwort", "", async page => {
    await mockPreview(page, { bytes: 3, content: { kind: "text", text: "alt" }, limitations: [] }, { hold: true });
    await selectFile(page);
    await page.keyboard.press("F3");
    await page.getByText("Momentaufnahme wird vorbereitet …").waitFor();
    await page.keyboard.press("Escape");
    await page.keyboard.press("F2");
    await page.locator("#modal-input").waitFor();
    await page.evaluate(() => window.__FINISH_PREVIEW__());
    await page.waitForTimeout(50);
    assert.match(await page.locator("#modal-title").innerText(), /Umbenennen/);
    await page.keyboard.press("Escape");
  });
}
