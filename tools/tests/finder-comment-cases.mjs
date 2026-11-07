import assert from "node:assert/strict";

async function setup(page, theme = "light") {
  await page.evaluate(async theme => {
    const { setTheme } = await import("/theme.js"); setTheme(theme);
    const original = window.__TAURI__.core.invoke;
    window.__COMMENT_CALLS__ = [];
    window.__COMMENT_TEXT__ = 'Alt <script>alert("x")</script> ä\nZweite Zeile';
    window.__COMMENT_PERMISSION__ = false;
    window.__TAURI__.core.invoke = async (command, args) => {
      if (command === "get_file_property_capabilities") return { ...await original(command, args), comments: args.provider === "local" };
      if (command === "get_file_finder_comments") {
        window.__COMMENT_CALLS__.push({ command, ...structuredClone(args) });
        if (args.request.requestPermission) window.__COMMENT_PERMISSION__ = true;
        return args.request.names.map(name => ({ name, state: window.__COMMENT_PERMISSION__ ? { text: window.__COMMENT_TEXT__, metadataDiffers: !window.__COMMENT_SAVED__ } : null,
          permissionRequired: !window.__COMMENT_PERMISSION__, code: window.__COMMENT_PERMISSION__ ? null : "permission_denied", message: "Finder-Zugriff ist nicht freigegeben" }));
      }
      if (command !== "change_file_finder_comments") return original(command, args);
      window.__COMMENT_CALLS__.push({ command, ...structuredClone(args) });
      if (window.__COMMENT_HOLD__) return new Promise(resolve => { window.__COMMENT_RESOLVE__ = resolve; });
      if (!window.__COMMENT_UNCLEAR__) { window.__COMMENT_TEXT__ = args.request.text; window.__COMMENT_SAVED__ = true; }
      return args.request.names.map((name, index) => ({ name, state: null, changed: !window.__COMMENT_UNCLEAR__,
        code: window.__COMMENT_UNCLEAR__ ? index ? "interrupted" : "outcome_unclear" : null,
        message: window.__COMMENT_UNCLEAR__ ? "Ausgang <b> unklar; prüfen" : "Finder-Kommentar und Metadaten bestätigt" }));
    };
  }, theme);
}
async function open(page) {
  await page.getByRole("listbox", { name: /Linke Dateiliste/ }).getByRole("option", { name: /Alpha.txt/ }).click();
  await page.keyboard.press("Meta+I");
  const dialog = page.getByRole("dialog", { name: "Informationen · Alpha.txt" });
  await dialog.locator('[data-file-info-editor="comments"] summary').waitFor();
  return dialog;
}
export async function runFinderCommentCases(runCase) {
  for (const [theme, width] of [["light", 650], ["dark", 390]]) {
    await runCase(`Finder-Kommentare lesen erst beim Öffnen und speichern echte Textwerte · ${theme}`, "?keyboard=1", async page => {
      await page.setViewportSize({ width, height: 650 }); await setup(page, theme);
      const dialog = await open(page);
      assert.equal(await page.evaluate(() => window.__COMMENT_CALLS__.length), 0);
      await dialog.locator('[data-file-info-editor="comments"] summary').click();
      await dialog.getByRole("button", { name: "Finder-Zugriff erlauben", exact: true }).waitFor();
      assert.deepEqual(await page.evaluate(() => window.__COMMENT_CALLS__.map(call => call.request.requestPermission)), [false]);
      await dialog.getByRole("button", { name: "Finder-Zugriff erlauben", exact: true }).click();
      const input = dialog.getByLabel("Kommentar", { exact: true });
      await input.waitFor();
      assert.equal(await input.inputValue(), 'Alt <script>alert("x")</script> ä\nZweite Zeile');
      assert.equal(await dialog.locator("script").count(), 0);
      await dialog.getByText(/Speichern gleicht beide Fassungen ab/).waitFor();
      const text = 'Neu ä " <&>\n';
      await input.fill(text.trimEnd()); await input.press("Enter");
      assert.equal(await input.inputValue(), text);
      assert.equal(await page.evaluate(() => window.__COMMENT_CALLS__.some(call => call.command === "change_file_finder_comments")), false);
      if (process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/finder-comments-${theme}-${width}.png` });
      await dialog.getByRole("button", { name: "Kommentar speichern", exact: true }).click();
      await dialog.getByRole("region", { name: "Ergebnis der Kommentaränderung" }).getByText(/Finder-Kommentar und Metadaten bestätigt/).waitFor();
      assert.equal(await input.inputValue(), text);
      await dialog.getByRole("button", { name: "Kommentar entfernen", exact: true }).click();
      await page.waitForFunction(() => window.__COMMENT_TEXT__ === "");
      await page.waitForFunction(() => document.querySelector("#file-info-comment-text")?.value === "");
      const writes = await page.evaluate(() => window.__COMMENT_CALLS__.filter(call => call.command === "change_file_finder_comments"));
      assert.deepEqual(writes.map(({request}) => ({...request, requestId:"bound"})), [text,""].map(text => ({requestId:"bound",provider:"local",root:"/mock/quelle",names:["Alpha.txt"],text})));
      assert.equal(await dialog.evaluate(node => node.scrollWidth <= node.clientWidth), true);
    });
  }
  await runCase("Finder-Kommentare erhalten bei unklarem Ausgang die überprüfbare alte Fassung", "?keyboard=1", async page => {
    await setup(page); const dialog = await open(page);
    await dialog.locator('[data-file-info-editor="comments"] summary').click();
    await dialog.getByRole("button", { name: "Finder-Zugriff erlauben", exact: true }).click();
    const input = dialog.getByLabel("Kommentar", {exact:true}); await input.waitFor();
    await page.evaluate(() => { window.__COMMENT_UNCLEAR__ = true; });
    await input.fill("Versuch"); await dialog.getByRole("button", { name:"Kommentar speichern",exact:true }).click();
    const report = dialog.getByRole("region", {name:"Ergebnis der Kommentaränderung"});
    await report.getByText(/Ausgang <b> unklar; prüfen/).waitFor();
    assert.equal(await report.locator("b").count(), 0);
    await page.waitForFunction(() => document.querySelector("#file-info-comment-text")?.value.startsWith("Alt <script>"));
    assert.equal(await page.evaluate(() => window.__COMMENT_CALLS__.filter(call => call.command === "change_file_finder_comments").length), 1);
  });
  await runCase("Finder-Kommentare verwechseln Dateirechte nicht mit einer Systemfreigabe", "?keyboard=1", async page => {
    await setup(page);
    await page.evaluate(()=>{
      const original=window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke=(command,args)=>command==="get_file_finder_comments"
        ? Promise.resolve(args.request.names.map(name=>({name,state:null,code:"permission_denied",permissionRequired:false,message:"Datei ohne Leserechte"}))) : original(command,args);
    });
    const dialog=await open(page); await dialog.locator('[data-file-info-editor="comments"] summary').click();
    await dialog.getByText(/Datei ohne Leserechte/).waitFor();
    assert.equal(await dialog.getByRole("button",{name:"Finder-Zugriff erlauben",exact:true}).count(),0);
  });

  await runCase("Finder-Kommentare blättern getrennt vom Entwurf und ändern die gesamte Auswahl", "?keyboard=1", async page => {
    await setup(page);
    await page.evaluate(async () => {
      window.__COMMENT_PERMISSION__ = true;
      const { createFileInformationUi } = await import("/file-information.js");
      const pane = { provider:"local", path:"/mock/quelle" };
      const entries = Array.from({length:42}, (_, i) => ({name:`Objekt-${String(i).padStart(2,"0")}`,is_file:true,is_dir:false,size:1}));
      createFileInformationUi({panes:{left:pane},selectedEntries:()=>entries,requestDirectorySizes:()=>{}}).open("left");
    });
    const dialog=page.getByRole("dialog",{name:"Informationen · 42 Objekte"});
    await dialog.locator('[data-file-info-editor="comments"] summary').click();
    const input=dialog.getByLabel("Neuer Kommentar für alle 42 ausgewählten Objekte",{exact:true});
    await input.fill("Gemeinsamer Entwurf <&> ä");
    await dialog.getByRole("button",{name:"Weitere Objekte",exact:true}).click();
    await dialog.getByRole("navigation",{name:"Kommentaransicht"}).getByText("Seite 2 von 2",{exact:true}).waitFor();
    assert.equal(await dialog.locator('[data-file-info-editor="comments"] .file-info-tag-objects > li').count(),2);
    assert.equal(await input.inputValue(),"Gemeinsamer Entwurf <&> ä");
    await dialog.getByRole("button",{name:"Kommentar speichern (42 Objekte)",exact:true}).click();
    const report=dialog.getByRole("region",{name:"Ergebnis der Kommentaränderung"});
    await report.getByText("42 von 42 bestätigt · Seite 1 von 2",{exact:true}).waitFor();
    const calls=await page.evaluate(()=>window.__COMMENT_CALLS__);
    assert(calls.filter(call=>call.command==="get_file_finder_comments").every(call=>call.request.names.length<=40));
    assert.equal(calls.filter(call=>call.command==="change_file_finder_comments").at(-1).request.names.length,42);
  });
  await runCase("Finder-Kommentare binden Abbruch an die eigene Anfrage", "?keyboard=1", async page => {
    await setup(page);const dialog=await open(page);
    await page.evaluate(()=>{window.__COMMENT_PERMISSION__=true;window.__COMMENT_HOLD__=true;
      const original=window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke=(command,args)=>{
        if(command!=="cancel_file_property_edit")return original(command,args);
        window.__COMMENT_CANCEL__=args;
        window.__COMMENT_RESOLVE__([{name:"Alpha.txt",changed:false,code:"interrupted",message:"Vor der Änderung abgebrochen"}]);
        return Promise.resolve();
      };
    });
    await dialog.locator('[data-file-info-editor="comments"] summary').click();
    const input=dialog.getByLabel("Kommentar",{exact:true});await input.fill("Abbruchprobe");
    await dialog.getByRole("button",{name:"Kommentar speichern",exact:true}).click();
    assert.equal(await input.isDisabled(),true);
    assert.equal(await dialog.getByRole("button",{name:"Schließen",exact:true}).isDisabled(),true);
    await dialog.locator('[data-file-info-property-cancel="comments"]').click();
    await dialog.getByRole("region",{name:"Ergebnis der Kommentaränderung"}).getByText(/Vor der Änderung abgebrochen/).waitFor();
    const result=await page.evaluate(()=>({request:window.__COMMENT_CALLS__.find(call=>call.command==="change_file_finder_comments").request,cancel:window.__COMMENT_CANCEL__}));
    assert.deepEqual(result.cancel,{requestId:result.request.requestId});
    assert.equal(await dialog.getByRole("button",{name:"Schließen",exact:true}).isEnabled(),true);
  });

}
