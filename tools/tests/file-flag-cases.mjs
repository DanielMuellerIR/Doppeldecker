import assert from "node:assert/strict";

export async function runFileFlagCases(runCase) {
  for (const [theme, width] of [["light", 650], ["dark", 390]]) {
    for (const [id, kind, title, label] of [["locked", "locked", "Dateisperre", "Gesperrt (macOS)"], ["extension", "hiddenExtension", "Dateierweiterung", "Erweiterung im Finder ausblenden"]]) {
      await runCase(`Dateiflags ${kind} lesen und ändern · ${theme} · ${width}`, "?keyboard=1&fileFlags=1", async page => {
        await page.setViewportSize({ width, height: 650 });
        await page.evaluate(async theme => {
          const { setTheme } = await import("/theme.js"); setTheme(theme);
          const original = window.__TAURI__.core.invoke;
          window.__FLAG_CALLS__ = []; window.__FLAG_VALUES__ = {locked:false,hiddenExtension:false};
          window.__TAURI__.core.invoke = async (command,args) => {
            if (command === "get_file_flags") return args.request.names.map(name=>({name,state:window.__FLAG_VALUES__[args.request.kind],changed:false,code:null,message:""}));
            if (command !== "change_file_flag") return original(command,args);
            window.__FLAG_CALLS__.push(structuredClone(args.request));window.__FLAG_VALUES__[args.request.kind]=args.request.enabled;
            return args.request.names.map(name=>({name,state:args.request.enabled,changed:true,code:null,message:"Eigenschaft bestätigt"}));
          };
        },theme);
        await page.getByRole("listbox",{name:/Linke Dateiliste/}).getByRole("option",{name:/Alpha.txt/}).click();
        await page.keyboard.press("Meta+I");
        const dialog=page.getByRole("dialog",{name:"Informationen · Alpha.txt"});
        const editor=dialog.locator(`[data-file-info-editor="${id}"]`);
        await editor.locator("summary").click();
        const checkbox=editor.getByRole("checkbox",{name:label,exact:true});
        assert.equal(await checkbox.isChecked(),false);
        await checkbox.check();
        if(process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.screenshot({path:`${process.env.DOPPELDECKER_SCREENSHOT_DIR}/fileflag-${id}-${theme}-${width}.png`});
        await editor.getByRole("button",{name:id==="locked"?"Dateisperre anwenden":"Erweiterungsanzeige anwenden",exact:true}).click();
        await dialog.getByRole("region",{name:`Ergebnis der Änderung der ${id==="locked"?"Dateisperre":"Erweiterungsanzeige"}`}).getByText("Eigenschaft bestätigt").waitFor();
        assert.equal(await checkbox.isChecked(),true);
        await checkbox.uncheck();await editor.getByRole("button",{name:id==="locked"?"Dateisperre anwenden":"Erweiterungsanzeige anwenden",exact:true}).click();
        await page.waitForFunction(()=>window.__FLAG_CALLS__.length===2);
        assert.equal(await checkbox.isChecked(),false);
        const calls=await page.evaluate(()=>window.__FLAG_CALLS__);
        assert.deepEqual(calls.map(({requestId,...request})=>request),[true,false].map(enabled=>({provider:"local",root:"/mock/quelle",names:["Alpha.txt"],kind,enabled})));
        assert(calls.every(call=>call.requestId));assert.equal(await dialog.evaluate(n=>n.scrollWidth<=n.clientWidth),true);
        assert.equal(await editor.locator("summary").innerText(),title);
      });
    }
  }
  await runCase("Dateiflags erhalten Entwürfe bei verspätetem anderen Lesebereich", "?keyboard=1&fileFlags=1",async page=>{
    await page.evaluate(()=>{
      const original=window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke=async(command,args)=>{
        if(command!=="get_file_flags")return original(command,args);
        const rows=args.request.names.map(name=>({name,state:false,changed:false,code:null,message:""}));
        if(args.request.kind==="hiddenExtension")return new Promise(resolve=>{window.__EXTENSION_READ__=()=>resolve(rows);});
        return rows;
      };
    });
    await page.getByRole("listbox",{name:/Linke Dateiliste/}).getByRole("option",{name:/Alpha.txt/}).click();await page.keyboard.press("Meta+I");
    const dialog=page.getByRole("dialog",{name:"Informationen · Alpha.txt"});
    await dialog.locator('[data-file-info-editor="locked"] summary').click();
    const checkbox=dialog.getByRole("checkbox",{name:"Gesperrt (macOS)",exact:true});await checkbox.check();
    await page.waitForFunction(()=>Boolean(window.__EXTENSION_READ__));await page.evaluate(()=>window.__EXTENSION_READ__());
    await dialog.locator('[data-file-info-editor="extension"] input').waitFor({state:"attached"});assert.equal(await checkbox.isChecked(),true);
  });
  await runCase("Dateiflags begrenzen Auswahlseiten und bilanzieren Abbruch", "?fileFlags=1",async page=>{
    await page.evaluate(async()=>{
      const original=window.__TAURI__.core.invoke;window.__FLAG_CALLS__=[];
      window.__TAURI__.core.invoke=async(command,args)=>{
        if(command==="get_file_flags"){
          window.__FLAG_CALLS__.push({command,...structuredClone(args)});
          return args.request.names.map(name=>({name,state:name==="Link"?null:false,changed:false,code:name==="Link"?"unsupported":null,message:"Link bleibt unverändert"}));
        }
        if(command==="change_file_flag"){window.__FLAG_CALLS__.push({command,...structuredClone(args)});return new Promise(resolve=>{window.__FLAG_RESOLVE__=resolve;});}
        if(command==="cancel_file_property_edit"){
          window.__FLAG_CANCEL__=args;const call=window.__FLAG_CALLS__.find(c=>c.command==="change_file_flag");
          window.__FLAG_RESOLVE__(call.request.names.map((name,index)=>({name,state:null,changed:false,code:index?"interrupted":"outcome_unclear",message:index?"Nicht begonnen":"Ausgang <b> unklar; prüfen"})));return null;
        }
        return original(command,args);
      };
      const {createFileInformationUi}=await import("/file-information.js");const entries=Array.from({length:41},(_,i)=>({name:`Objekt-${i}`,is_file:true}));entries.push({name:"Link",is_symlink:true},{name:"�",safely_decoded:false,is_file:true});
      createFileInformationUi({panes:{left:{provider:"local",path:"/mock/quelle"}},selectedEntries:()=>entries}).open("left");
    });
    const dialog=page.getByRole("dialog",{name:"Informationen · 43 Objekte"}),editor=dialog.locator('[data-file-info-editor="locked"]');
    await editor.locator("summary").click();await editor.getByRole("navigation",{name:"Dateisperre",exact:true}).getByText("Seite 1 von 2",{exact:true}).waitFor();assert.equal(await editor.locator(".file-info-tag-objects > li").count(),40);
    await editor.getByRole("button",{name:"Weitere Objekte",exact:true}).click();await editor.getByText(/Link bleibt unverändert/).waitFor();await editor.getByRole("button",{name:"Bei allen 42 aktivieren",exact:true}).click();await editor.getByRole("button",{name:"Änderung abbrechen",exact:true}).click();
    const report=dialog.getByRole("region",{name:"Ergebnis der Änderung der Dateisperre"});await report.getByText("0 von 42 bestätigt · Seite 1 von 2",{exact:true}).waitFor();assert.equal(await report.locator("b").count(),0);await report.getByRole("button",{name:"Weiter",exact:true}).click();await report.getByText("0 von 42 bestätigt · Seite 2 von 2",{exact:true}).waitFor();
    const calls=await page.evaluate(()=>window.__FLAG_CALLS__),edit=calls.find(c=>c.command==="change_file_flag");assert.equal(edit.request.kind,"locked");assert.equal(edit.request.names.length,42);assert(!edit.request.names.includes("�"));assert(calls.filter(c=>c.command==="get_file_flags").every(c=>c.request.names.length<=40));assert.deepEqual(await page.evaluate(()=>window.__FLAG_CANCEL__),{requestId:edit.request.requestId});
  });
}
