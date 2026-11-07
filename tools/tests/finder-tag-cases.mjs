import assert from "node:assert/strict";

async function setup(page, theme = "light") {
  await page.evaluate(async theme => {
    const { setTheme } = await import("/theme.js"); setTheme(theme);
    const original = window.__TAURI__.core.invoke;
    window.__TAG_CALLS__ = [];
    window.__TAG_STATE__ = { tags: [{ name: "Projekt <b>", color: 4 }, { name: "Archiv", color: 0 }], legacyColor: 6 };
    window.__TAURI__.core.invoke = async (command, args) => {
      if (command === "get_file_finder_tags") {
        window.__TAG_CALLS__.push({ command, ...args });
        return args.request.names.map(name => ({ name, state: structuredClone(window.__TAG_STATE__), code: null, message: "" }));
      }
      if (command !== "change_file_finder_tags") return original(command, args);
      window.__TAG_CALLS__.push({ command, ...args });
      if (window.__TAG_WAIT__) return new Promise(resolve => { window.__TAG_RESOLVE__ = resolve; });
      const state = window.__TAG_STATE__;
      const edit = args.request.edit;
      if (!window.__TAG_UNCLEAR__) {
        if (edit.type === "upsert") { state.tags = state.tags.filter(tag => tag.name !== edit.previousName && tag.name !== edit.tag.name); state.tags.push(edit.tag); }
        if (edit.type === "remove") state.tags = state.tags.filter(tag => tag.name !== edit.name);
        if (edit.type === "clear_legacy" || edit.type === "clear") state.legacyColor = 0;
        if (edit.type === "clear") state.tags = [];
      }
      return args.request.names.map(name => ({ name, changed: false,
        code: window.__TAG_UNCLEAR__ ? "outcome_unclear" : null,
        message: window.__TAG_UNCLEAR__ ? "Tagänderung <img src=x> unklar; prüfen" : "Tags bestätigt" }));
    };
  }, theme);
  const pane = page.getByRole("listbox", { name: /Linke Dateiliste/ });
  await pane.getByRole("option", { name: /Alpha.txt/ }).click();
  await page.keyboard.press("Meta+I");
  const dialog = page.getByRole("dialog", { name: "Informationen · Alpha.txt" });
  await dialog.locator(".file-info-tags summary").click();
  await dialog.getByText("Projekt <b> · Blau", { exact: true }).waitFor();
  return dialog;
}

export async function runFinderTagCases(runCase) {
  for (const [theme, width] of [["light", 650], ["dark", 650], ["light", 390], ["dark", 390]]) {
    await runCase(`Finder-Tags bearbeiten Namen, Farben und alte Markierungen · ${theme} · ${width}`, "?keyboard=1&finderTags=1", async page => {
      await page.setViewportSize({ width, height: 650 });
      const dialog = await setup(page, theme);
      if (process.env.DOPPELDECKER_SCREENSHOT_DIR) {
        await page.mouse.move(0, 0);
        await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/finder-tags-${theme}-${width}.png` });
        await dialog.getByLabel("Tagname", { exact: true }).scrollIntoViewIfNeeded();
        await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/finder-tags-${theme}-${width}-controls.png` });
      }
      assert.equal(await dialog.locator(".file-info-tag b").count(), 0);
      await dialog.getByRole("button", { name: "Projekt <b> bei Alpha.txt bearbeiten" }).click();
      assert.equal(await dialog.getByLabel("Tagname", { exact: true }).inputValue(), "Projekt <b>");
      await dialog.getByLabel("Tagname", { exact: true }).fill("Fertig ü");
      await dialog.getByLabel("Tagfarbe", { exact: true }).selectOption("2");
      await dialog.getByRole("button", { name: "Tag speichern", exact: true }).click();
      await dialog.getByText("Fertig ü · Grün", { exact: true }).waitFor();
      await dialog.getByRole("region", { name: "Ergebnis der Finder-Tagänderung" }).getByText("1 von 1 bestätigt · Seite 1 von 1", { exact: true }).waitFor();
      const edits = await page.evaluate(() => window.__TAG_CALLS__.filter(call => call.command === "change_file_finder_tags"));
      assert.deepEqual(edits[0].request.edit, { type: "upsert", tag: { name: "Fertig ü", color: 2 }, previousName: "Projekt <b>" });
      assert.deepEqual(edits[0].request.names, ["Alpha.txt"]);
      assert.match(edits[0].request.requestId, /^[a-f0-9-]{36}$/);
      await dialog.getByText("Alte Finder-Farbmarkierung: Rot").waitFor();
      await dialog.getByRole("button", { name: "Farbmarkierung entfernen", exact: true }).click();
      await dialog.getByText("Alte Finder-Farbmarkierung: Rot").waitFor({ state: "hidden" });
      await dialog.getByText("Fertig ü · Grün", { exact: true }).waitFor();
      await dialog.getByRole("button", { name: "Archiv bei Alpha.txt entfernen" }).click();
      await dialog.getByText("Archiv · Ohne Farbe", { exact: true }).waitFor({ state: "hidden" });
      await dialog.getByRole("button", { name: /Alle Tags und Farbmarkierungen entfernen/ }).click();
      await dialog.getByText("Keine Tags", { exact: true }).waitFor();
      assert.equal(await dialog.evaluate(element => element.scrollWidth <= element.clientWidth), true);
      const calls = await page.evaluate(() => window.__TAG_CALLS__);
      assert(calls.filter(call => call.command === "get_file_finder_tags").every(call => call.request.names.length <= 40));
      assert.deepEqual(calls.filter(call => call.command === "change_file_finder_tags").map(call => call.request.edit.type), ["upsert", "clear_legacy", "remove", "clear"]);
    });
  }

  await runCase("Finder-Tags behalten Entwürfe und melden unklaren Ausgang ohne Wiederholung", "?keyboard=1&finderTags=1", async page => {
    const dialog = await setup(page);
    await dialog.getByLabel("Unix-Rechte (oktal)").evaluate(input => { input.value = "640"; });
    await dialog.getByLabel("Tagname", { exact: true }).fill("Neu <img>");
    await dialog.getByLabel("Tagfarbe", { exact: true }).selectOption("3");
    await page.evaluate(() => { window.__TAG_UNCLEAR__ = true; });
    await dialog.getByRole("button", { name: "Tag hinzufügen", exact: true }).click();
    await dialog.getByRole("region", { name: "Ergebnis der Finder-Tagänderung" }).getByText(/outcome_unclear/).waitFor();
    assert.equal(await dialog.getByLabel("Tagname", { exact: true }).inputValue(), "Neu <img>");
    assert.equal(await dialog.getByLabel("Tagfarbe", { exact: true }).inputValue(), "3");
    assert.equal(await dialog.getByLabel("Unix-Rechte (oktal)").inputValue(), "640");
    assert.equal(await dialog.locator(".file-info-permission-report img").count(), 0);
    assert.equal(await page.evaluate(() => window.__TAG_CALLS__.filter(call => call.command === "change_file_finder_tags").length), 1);
    assert.equal(await dialog.getByRole("button", { name: "Schließen", exact: true }).isEnabled(), true);
  });

  await runCase("Finder-Tags binden Abbruch an die Anfrage und sperren parallele Editoren", "?keyboard=1&finderTags=1", async page => {
    const dialog = await setup(page);
    await page.evaluate(() => {
      window.__TAG_WAIT__ = true;
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = (command, args) => {
        if (command !== "cancel_file_property_edit") return original(command, args);
        window.__TAG_CANCEL__ = args;
        window.__TAG_RESOLVE__([{ name: "Alpha.txt", changed: false, code: "cancelled", message: "Vor der Änderung abgebrochen" }]);
        return Promise.resolve();
      };
    });
    await dialog.getByLabel("Tagname", { exact: true }).fill("Pause");
    await dialog.getByRole("button", { name: "Tag hinzufügen", exact: true }).click();
    assert.equal(await dialog.getByLabel("Tagfarbe", { exact: true }).isDisabled(), true);
    assert.equal(await dialog.getByRole("button", { name: "Schließen", exact: true }).isDisabled(), true);
    await dialog.locator('[data-file-info-property-cancel="tags"]').click();
    await dialog.getByRole("region", { name: "Ergebnis der Finder-Tagänderung" }).getByText(/Vor der Änderung abgebrochen/).waitFor();
    const result = await page.evaluate(() => ({ request: window.__TAG_CALLS__.find(call => call.command === "change_file_finder_tags").request, cancel: window.__TAG_CANCEL__ }));
    assert.deepEqual(result.cancel, { requestId: result.request.requestId });
    assert.equal(await dialog.getByRole("button", { name: "Schließen", exact: true }).isEnabled(), true);
  });

  await runCase("Finder-Tags blättern große Auswahlen und ändern nur das benannte Objekt", "?keyboard=1&finderTags=1", async page => {
    const first = await setup(page);
    await first.getByRole("button", { name: "Schließen", exact: true }).click();
    await page.evaluate(async () => {
      const { createFileInformationUi } = await import("/file-information.js");
      const pane = { provider: "local", path: "/mock/quelle" };
      const entries = Array.from({ length: 42 }, (_, i) => ({ name: `Objekt-${String(i).padStart(2, "0")}`, is_file: true, is_dir: false, size: 1 }));
      const ui = createFileInformationUi({ panes: { left: pane }, selectedEntries: () => entries, requestDirectorySizes: () => {} });
      window.__TAG_CALLS__ = [];
      ui.open("left");
    });
    const dialog = page.getByRole("dialog", { name: "Informationen · 42 Objekte" });
    await dialog.locator(".file-info-tags summary").click();
    await dialog.getByRole("navigation", { name: "Tagansicht" }).getByText("Seite 1 von 2", { exact: true }).waitFor();
    assert.equal(await dialog.locator(".file-info-tag-objects > li").count(), 40);
    await dialog.getByRole("button", { name: "Weitere Objekte", exact: true }).click();
    await dialog.getByRole("button", { name: "Projekt <b> bei Objekt-40 bearbeiten", exact: true }).waitFor();
    assert.equal(await dialog.locator(".file-info-tag-objects > li").count(), 2);
    await dialog.getByRole("button", { name: "Projekt <b> bei Objekt-40 bearbeiten", exact: true }).click();
    await dialog.getByLabel("Tagname", { exact: true }).fill("Nur hier");
    await dialog.getByRole("button", { name: "Tag speichern", exact: true }).click();
    await dialog.getByRole("button", { name: "Nur hier bei Objekt-40 bearbeiten", exact: true }).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__TAG_CALLS__.filter(call => call.command === "change_file_finder_tags").map(call => call.request.names)), [["Objekt-40"]]);
    await dialog.getByRole("button", { name: /Alle Tags und Farbmarkierungen entfernen \(42 Objekte\)/ }).click();
    const report = dialog.getByRole("region", { name: "Ergebnis der Finder-Tagänderung" });
    await report.getByText("42 von 42 bestätigt · Seite 1 von 2", { exact: true }).waitFor();
    await report.getByRole("button", { name: "Weiter", exact: true }).click();
    await report.getByText("42 von 42 bestätigt · Seite 2 von 2", { exact: true }).waitFor();
    const calls = await page.evaluate(() => window.__TAG_CALLS__);
    assert(calls.filter(call => call.command === "get_file_finder_tags").every(call => call.request.names.length <= 40));
    assert.equal(calls.filter(call => call.command === "change_file_finder_tags").at(-1).request.names.length, 42);
  });

  await runCase("Finder-Tags lassen problematische Namen aus und erhalten unabhängig bearbeitbare Objekte", "?keyboard=1&finderTags=1", async page => {
    const first = await setup(page);
    await first.getByRole("button", { name: "Schließen", exact: true }).click();
    await page.evaluate(async () => {
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command !== "get_file_finder_tags") return original(command, args);
        const rows = await original(command, args);
        return rows.map(row => row.name === "Link" ? { ...row, state: null, code: "unsupported", message: "Link bleibt unverändert" } : row);
      };
      const { createFileInformationUi } = await import("/file-information.js");
      const entries = [{ name: "Datei", is_file: true }, { name: "Link", is_symlink: true }, { name: "�", safely_decoded: false, is_file: true }];
      window.__TAG_CALLS__ = [];
      createFileInformationUi({ panes: { left: { provider: "local", path: "/mock/quelle" } }, selectedEntries: () => entries }).open("left");
    });
    const dialog = page.getByRole("dialog", { name: "Informationen · 3 Objekte" });
    await dialog.locator(".file-info-tags summary").click();
    await dialog.getByText("Link bleibt unverändert (unsupported)", { exact: true }).waitFor();
    assert.equal(await dialog.locator(".file-info-permissions summary").count(), 1);
    assert.equal(await dialog.locator(".file-info-owner summary").count(), 1);
    await dialog.getByLabel("Tagname", { exact: true }).fill("Gemeinsam");
    await dialog.getByRole("button", { name: "Tag hinzufügen", exact: true }).click();
    await dialog.getByRole("region", { name: "Ergebnis der Finder-Tagänderung" }).waitFor();
    const calls = await page.evaluate(() => window.__TAG_CALLS__);
    assert(calls.every(call => !call.request.names.includes("�")));
    assert.deepEqual(calls.find(call => call.command === "change_file_finder_tags").request.names, ["Datei", "Link"]);
  });
}
