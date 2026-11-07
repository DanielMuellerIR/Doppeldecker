import assert from "node:assert/strict";

export async function runVisibilityCases(runCase) {
  for (const [theme, width] of [["light", 650], ["dark", 390]]) {
    await runCase(`Sichtbarkeit liest zurück und trennt Punktnamen · ${theme} · ${width}`, "?keyboard=1&visibility=1", async page => {
      await page.setViewportSize({ width, height: 650 });
      await page.evaluate(async theme => {
        const { setTheme } = await import("/theme.js"); setTheme(theme);
        const original = window.__TAURI__.core.invoke;
        window.__VISIBILITY_CALLS__ = [];
        window.__HIDDEN_MARKED__ = false;
        window.__TAURI__.core.invoke = async (command, args) => {
          if (command === "get_file_visibility") return args.request.names.map(name => ({ name, state: {marked:window.__HIDDEN_MARKED__,byName:true},code:null,message:"" }));
          if (command !== "change_file_visibility") return original(command,args);
          window.__VISIBILITY_CALLS__.push(structuredClone(args.request));
          window.__HIDDEN_MARKED__ = args.request.hidden;
          return args.request.names.map(name => ({ name,state:{marked:args.request.hidden,byName:true},changed:true,code:null,message:"Markierung bestätigt" }));
        };
      }, theme);
      await page.getByRole("listbox", { name: /Linke Dateiliste/ }).getByRole("option", { name: /Alpha.txt/ }).click();
      await page.keyboard.press("Meta+I");
      const dialog = page.getByRole("dialog", { name: "Informationen · Alpha.txt" });
      await dialog.locator('[data-file-info-editor="visibility"] summary').click();
      await dialog.getByText(/bleibt auch ohne Markierung ausgeblendet/).waitFor();
      const checkbox = dialog.getByRole("checkbox",{name:"Verstecktmarkierung (macOS)",exact:true});
      assert.equal(await checkbox.isChecked(),false);
      await checkbox.check();
      if (process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.screenshot({path:`${process.env.DOPPELDECKER_SCREENSHOT_DIR}/visibility-${theme}-${width}.png`});
      await dialog.getByRole("button",{name:"Sichtbarkeit anwenden",exact:true}).click();
      await dialog.getByRole("region",{name:"Ergebnis der Sichtbarkeitsänderung"}).getByText("Markierung bestätigt").waitFor();
      assert.equal(await checkbox.isChecked(),true);
      await checkbox.uncheck();
      await dialog.getByRole("button",{name:"Sichtbarkeit anwenden",exact:true}).click();
      await page.waitForFunction(() => window.__VISIBILITY_CALLS__.length===2);
      await page.waitForFunction(() => !window.__HIDDEN_MARKED__);
      assert.equal(await checkbox.isChecked(),false,"Backend-Rücklesen muss den vorigen Checkboxwert ersetzen");
      const calls = await page.evaluate(() => window.__VISIBILITY_CALLS__);
      assert.deepEqual(calls.map(({requestId,...request})=>request),[true,false].map(hidden=>({provider:"local",root:"/mock/quelle",names:["Alpha.txt"],hidden})));
      assert(calls.every(call=>call.requestId));
      assert.equal(await dialog.evaluate(node=>node.scrollWidth<=node.clientWidth),true);
    });
  }
  await runCase("Sichtbarkeit bilanziert große Auswahlen, Fehler und Abbruch", "?visibility=1", async page => {
    await page.evaluate(async () => {
      const original = window.__TAURI__.core.invoke;
      window.__VISIBILITY_CALLS__=[];
      window.__TAURI__.core.invoke=async(command,args)=>{
        if(command==="get_file_visibility") {
          window.__VISIBILITY_CALLS__.push({command,...structuredClone(args)});
          return args.request.names.map(name=>({name,state:name==="Link"?null:{marked:false,byName:false},code:name==="Link"?"unsupported":null,message:"Link bleibt unverändert"}));
        }
        if(command==="change_file_visibility") {
          window.__VISIBILITY_CALLS__.push({command,...structuredClone(args)});
          return new Promise(resolve=>{window.__VISIBILITY_RESOLVE__=resolve;});
        }
        if(command==="cancel_file_property_edit") {
          window.__VISIBILITY_CANCEL__=args;
          const call=window.__VISIBILITY_CALLS__.find(call=>call.command==="change_file_visibility");
          window.__VISIBILITY_RESOLVE__(call.request.names.map((name,index)=>({name,state:null,changed:false,code:index?"interrupted":"outcome_unclear",message:index?"Nicht begonnen":"Ausgang <b> unklar; prüfen"})));
          return null;
        }
        return original(command,args);
      };
      const {createFileInformationUi}=await import("/file-information.js");
      const entries=Array.from({length:41},(_,i)=>({name:`Objekt-${String(i).padStart(2,"0")}`,is_file:true}));
      entries.push({name:"Link",is_symlink:true},{name:"�",safely_decoded:false,is_file:true});
      createFileInformationUi({panes:{left:{provider:"local",path:"/mock/quelle"}},selectedEntries:()=>entries}).open("left");
    });
    const dialog=page.getByRole("dialog",{name:"Informationen · 43 Objekte"});
    await dialog.locator('[data-file-info-editor="visibility"] summary').click();
    await dialog.getByRole("navigation",{name:"Sichtbarkeitsansicht"}).getByText("Seite 1 von 2",{exact:true}).waitFor();
    assert.equal(await dialog.locator('.file-info-tag-objects > li').count(),40);
    await dialog.getByRole("button",{name:"Weitere Objekte",exact:true}).click();
    await dialog.getByText(/Link bleibt unverändert/).waitFor();
    await dialog.getByRole("button",{name:"Alle 42 als versteckt markieren",exact:true}).click();
    await dialog.getByRole("button",{name:"Änderung abbrechen",exact:true}).click();
    const report=dialog.getByRole("region",{name:"Ergebnis der Sichtbarkeitsänderung"});
    await report.getByText("0 von 42 bestätigt · Seite 1 von 2",{exact:true}).waitFor();
    assert.equal(await report.locator("b").count(),0);
    await report.getByRole("button",{name:"Weiter",exact:true}).click();
    await report.getByText("0 von 42 bestätigt · Seite 2 von 2",{exact:true}).waitFor();
    const calls=await page.evaluate(()=>window.__VISIBILITY_CALLS__);
    const edit=calls.find(call=>call.command==="change_file_visibility");
    assert.equal(edit.request.names.length,42);
    assert(!edit.request.names.includes("�"));
    assert(calls.filter(call=>call.command==="get_file_visibility").every(call=>call.request.names.length<=40));
    assert.deepEqual(await page.evaluate(()=>window.__VISIBILITY_CANCEL__),{requestId:edit.request.requestId});
  });

}
