import assert from "node:assert/strict";

export async function runAdministratorCases(runCase) {
  await runCase("Linux-Administratorfreigabe verwendet Polkit ohne Diensteinrichtung", "?keyboard=1&administrator=1", async page => {
    await page.evaluate(() => {
      document.documentElement.dataset.platform = "linux";
      const original = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (command, args) => command === "get_metadata_admin_status"
        ? { available: true, state: "polkit", message: "Polkit fragt die Systemfreigabe für jede Änderung an." }
        : original(command, args);
    });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).getByRole("option", { name: /Alpha.txt/ }).click();
    await page.keyboard.press("Meta+I");
    const dialog = page.getByRole("dialog", { name: "Informationen · Alpha.txt" });
    const setup = dialog.locator('[data-file-info-editor="administrator"]');
    await setup.locator("summary").click();
    await setup.getByText("Polkit fragt die Systemfreigabe für jede Änderung an.", { exact: true }).waitFor();
    assert.equal(await setup.getByRole("button").count(), 0);
    const permission = dialog.locator('[data-file-info-editor="permission"]');
    await permission.locator("summary").click();
    await permission.getByRole("checkbox", { name: "Mit Administratorfreigabe ändern", exact: true }).check();
    await page.waitForFunction(() => {
      const note = [...document.querySelectorAll('[data-file-info-editor="permission"] .file-info-note')].find(node => node.textContent.startsWith("Drei Ziffern"));
      const body = document.querySelector("#modal-body").getBoundingClientRect();
      const bounds = note.getBoundingClientRect();
      return bounds.top >= body.top && bounds.bottom <= body.bottom;
    });
    if (process.env.DOPPELDECKER_SCREENSHOT_DIR) await page.screenshot({ path: `${process.env.DOPPELDECKER_SCREENSHOT_DIR}/linux-polkit.png` });
  });
  for (const state of ["notFound","invalidPackage"]) {
    await runCase(`Administratorfreigabe unterscheidet Systemstatus und beschädigtes Paket · ${state}`,"?keyboard=1&administrator=1",async page=>{
      await page.evaluate(state=>{
        const original=window.__TAURI__.core.invoke;
        window.__TAURI__.core.invoke=async(command,args)=>command==="get_metadata_admin_status"?{available:true,state,message:state==="notFound"?"macOS kennt den vorhandenen Helfer noch nicht":"App-Paket beschädigt"}:original(command,args);
      },state);
      await page.getByRole("listbox",{name:/Linke Dateiliste/}).getByRole("option",{name:/Alpha.txt/}).click();await page.keyboard.press("Meta+I");const dialog=page.getByRole("dialog",{name:"Informationen · Alpha.txt"}),setup=dialog.locator('[data-file-info-editor="administrator"]');await setup.locator("summary").click();await setup.getByText(state==="notFound"?"macOS kennt den vorhandenen Helfer noch nicht":"App-Paket beschädigt",{exact:true}).waitFor();assert.equal(await setup.getByRole("button",{name:"Administratorzugriff einrichten",exact:true}).count(),state==="notFound"?1:0);
    });
  }
  await runCase("Administratorfreigabe Einrichtung übernimmt keinen alten Änderungsabbruch", "?keyboard=1&administrator=1",async page=>{
    await page.evaluate(()=>{
      const original=window.__TAURI__.core.invoke;window.__ADMIN_SETUP_STATE__="notRegistered";
      window.__TAURI__.core.invoke=async(command,args)=>{
        if(command==="get_metadata_admin_status")return {available:true,state:window.__ADMIN_SETUP_STATE__,message:"Teststatus"};
        if(command==="change_file_permissions")return args.request.names.map(name=>({name,changed:true,code:null,mode:0o640,message:"Rechte bestätigt"}));
        if(command==="set_metadata_admin_service")return new Promise(resolve=>{window.__ADMIN_SETUP_RESOLVE__=value=>{window.__ADMIN_SETUP_STATE__=value.state;resolve(value);};});
        return original(command,args);
      };
    });
    await page.getByRole("listbox",{name:/Linke Dateiliste/}).getByRole("option",{name:/Alpha.txt/}).click();await page.keyboard.press("Meta+I");const dialog=page.getByRole("dialog",{name:"Informationen · Alpha.txt"}),editor=dialog.locator('[data-file-info-editor="permission"]');await editor.locator("summary").click();await editor.getByLabel("Unix-Rechte (oktal)").fill("640");await editor.getByRole("button",{name:"Rechte anwenden",exact:true}).click();await dialog.getByRole("region",{name:"Ergebnis der Rechteänderung"}).getByText("Rechte bestätigt").waitFor();const setup=dialog.locator('[data-file-info-editor="administrator"]');await setup.locator("summary").click();await setup.getByRole("button",{name:"Administratorzugriff einrichten",exact:true}).click();await setup.getByText("Administratorzugriff wird eingerichtet …",{exact:true}).waitFor();assert.equal(await dialog.getByRole("button",{name:"Änderung abbrechen",exact:true}).count(),0);await dialog.getByRole("button",{name:"Schließen",exact:true}).click();assert.equal(await dialog.count(),1);await page.evaluate(()=>window.__ADMIN_SETUP_RESOLVE__({available:true,state:"requiresApproval",message:"Freigabe nötig"}));await setup.getByRole("button",{name:"Systemeinstellungen öffnen",exact:true}).waitFor();await dialog.getByRole("button",{name:"Schließen",exact:true}).click();assert.equal(await dialog.count(),0);
  });
  await runCase("Administratorfreigabe erneute Prüfung ersetzt veralteten Einrichtungsfehler", "?keyboard=1&administrator=1", async page => {
    await page.evaluate(() => {
      const original = window.__TAURI__.core.invoke;
      window.__ADMIN_REFRESH_STATE__ = "notRegistered";
      window.__TAURI__.core.invoke = async (command, args) => {
        if (command === "get_metadata_admin_status") return { available: true, state: window.__ADMIN_REFRESH_STATE__, message: window.__ADMIN_REFRESH_STATE__ === "enabled" ? "Systemfreigabe bestätigt" : "Systemfreigabe ausstehend" };
        if (command === "set_metadata_admin_service") { window.__ADMIN_REFRESH_STATE__ = "requiresApproval"; throw new Error("Einrichtungsfehler der alten Version"); }
        return original(command, args);
      };
    });
    await page.getByRole("listbox", { name: /Linke Dateiliste/ }).getByRole("option", { name: /Alpha.txt/ }).click();
    await page.keyboard.press("Meta+I");
    const dialog = page.getByRole("dialog", { name: "Informationen · Alpha.txt" });
    const setup = dialog.locator('[data-file-info-editor="administrator"]');
    await setup.locator("summary").click();
    await setup.getByRole("button", { name: "Administratorzugriff einrichten", exact: true }).click();
    await setup.getByText("Einrichtungsfehler der alten Version", { exact: true }).waitFor();
    await page.evaluate(() => { window.__ADMIN_REFRESH_STATE__ = "enabled"; });
    await setup.getByRole("button", { name: "Freigabe erneut prüfen", exact: true }).click();
    await setup.getByText("Systemfreigabe bestätigt", { exact: true }).waitFor();
    assert.equal(await setup.getByText("Einrichtungsfehler der alten Version", { exact: true }).count(), 0);
    await setup.getByRole("button", { name: "Administratorzugriff aktualisieren", exact: true }).waitFor();
  });
  for (const [theme,width] of [["light",650],["dark",390]]) {
    await runCase(`Administratorfreigabe Einrichtung und lokale Änderungen · ${theme} · ${width}`, "?keyboard=1&administrator=1", async page=>{
      await page.setViewportSize({width,height:650});
      await page.evaluate(async theme=>{
        const {setTheme}=await import("/theme.js");setTheme(theme);
        const original=window.__TAURI__.core.invoke;window.__ADMIN_CALLS__=[];window.__ADMIN_STATE__="notRegistered";
        window.__TAURI__.core.invoke=async(command,args)=>{
          if(command==="get_metadata_admin_status")return {available:true,state:window.__ADMIN_STATE__,message:window.__ADMIN_STATE__==="enabled"?"Helfer verfügbar":"Helfer einrichten"};
          if(command==="set_metadata_admin_service"){window.__ADMIN_CALLS__.push({command,...structuredClone(args)});window.__ADMIN_STATE__=args.enabled?"requiresApproval":"notRegistered";return {available:true,state:window.__ADMIN_STATE__,message:"Systemfreigabe erforderlich"};}
          if(command==="open_metadata_admin_settings"){window.__ADMIN_CALLS__.push({command});window.__ADMIN_STATE__="enabled";return null;}
          if(["change_file_permissions","change_file_ownership"].includes(command)){window.__ADMIN_CALLS__.push({command,...structuredClone(args)});return args.request.names.map(name=>({name,changed:true,code:null,mode:0o640,owner:"root",group:"wheel",message:"Änderung bestätigt"}));}
          return original(command,args);
        };
      },theme);
      await page.getByRole("listbox",{name:/Linke Dateiliste/}).getByRole("option",{name:/Alpha.txt/}).click();await page.keyboard.press("Meta+I");
      const dialog=page.getByRole("dialog",{name:"Informationen · Alpha.txt"});
      const setup=dialog.locator('[data-file-info-editor="administrator"]');await setup.locator("summary").click();
      await setup.getByRole("button",{name:"Administratorzugriff einrichten",exact:true}).click();await setup.getByRole("button",{name:"Systemeinstellungen öffnen",exact:true}).click();await setup.getByRole("button",{name:"Freigabe erneut prüfen",exact:true}).click();await setup.getByRole("button",{name:"Administratorzugriff aktualisieren",exact:true}).waitFor();
      const permission=dialog.locator('[data-file-info-editor="permission"]');await permission.locator("summary").click();
      await permission.getByRole("checkbox",{name:"Mit Administratorfreigabe ändern",exact:true}).check();await permission.getByLabel("Unix-Rechte (oktal)").fill("640");
      if(process.env.DOPPELDECKER_SCREENSHOT_DIR)await page.screenshot({path:`${process.env.DOPPELDECKER_SCREENSHOT_DIR}/administrator-${theme}-${width}.png`});
      await permission.getByRole("button",{name:"Rechte anwenden",exact:true}).click();await dialog.getByRole("region",{name:"Ergebnis der Rechteänderung"}).getByText("Änderung bestätigt").waitFor();
      const ownership=dialog.locator('[data-file-info-editor="ownership"]');await ownership.locator("summary").click();await ownership.getByLabel("Eigentümer (Name oder UID)").fill("root");await ownership.getByRole("checkbox",{name:"Mit Administratorfreigabe ändern",exact:true}).check();await ownership.getByRole("button",{name:"Eigentümer/Gruppe anwenden",exact:true}).click();await dialog.getByRole("region",{name:"Ergebnis der Eigentümeränderung"}).getByText("Änderung bestätigt").waitFor();
      const calls=await page.evaluate(()=>window.__ADMIN_CALLS__);const edits=calls.filter(c=>c.request);assert.equal(edits.length,2);assert(edits.every(c=>c.request.administrator===true&&c.request.provider==="local"&&c.request.names[0]==="Alpha.txt"));assert.notEqual(edits[0].request.requestId,edits[1].request.requestId);
      assert.equal(calls.filter(c=>c.command==="set_metadata_admin_service").length,1);assert.equal(await dialog.evaluate(n=>n.scrollWidth<=n.clientWidth),true);
      await setup.getByRole("button",{name:"Administratorzugriff aktualisieren",exact:true}).click();await setup.getByRole("button",{name:"Systemeinstellungen öffnen",exact:true}).waitFor();
      assert.equal((await page.evaluate(()=>window.__ADMIN_CALLS__)).filter(c=>c.command==="set_metadata_admin_service"&&c.enabled).length,2);
      await setup.getByRole("button",{name:"Administratorzugriff entfernen",exact:true}).click();await setup.getByRole("button",{name:"Administratorzugriff einrichten",exact:true}).waitFor();
    });
  }
  await runCase("Administratorfreigabe Abbruch und unklarer Ausgang bleiben sichtbar", "?keyboard=1&administrator=1",async page=>{
    await page.evaluate(()=>{
      const original=window.__TAURI__.core.invoke;window.__ADMIN_EDITS__=[];
      window.__TAURI__.core.invoke=async(command,args)=>{
        if(command==="change_file_permissions"){window.__ADMIN_EDITS__.push(structuredClone(args.request));return new Promise(resolve=>{window.__ADMIN_RESOLVE__=resolve;});}
        if(command==="cancel_file_property_edit"){window.__ADMIN_CANCEL__=args;window.__ADMIN_RESOLVE__([{name:"Alpha.txt",changed:false,code:"outcome_unclear",message:"Antwort verloren; Teilwirkung prüfen"}]);return null;}
        return original(command,args);
      };
    });
    await page.getByRole("listbox",{name:/Linke Dateiliste/}).getByRole("option",{name:/Alpha.txt/}).click();await page.keyboard.press("Meta+I");const dialog=page.getByRole("dialog",{name:"Informationen · Alpha.txt"}),editor=dialog.locator('[data-file-info-editor="permission"]');await editor.locator("summary").click();await editor.getByRole("checkbox",{name:"Mit Administratorfreigabe ändern",exact:true}).check();await editor.getByLabel("Unix-Rechte (oktal)").fill("640");await editor.getByRole("button",{name:"Rechte anwenden",exact:true}).click();await editor.getByRole("button",{name:"Änderung abbrechen",exact:true}).click();await dialog.getByText("Antwort verloren; Teilwirkung prüfen (outcome_unclear)").waitFor();const edits=await page.evaluate(()=>window.__ADMIN_EDITS__);assert.equal(edits.length,1);assert.equal(edits[0].administrator,true);assert.deepEqual(await page.evaluate(()=>window.__ADMIN_CANCEL__),{requestId:edits[0].requestId});
  });
  await runCase("Administratorfreigabe verweigerter Start zeigt und fokussiert den Fehler", "?keyboard=1&administrator=1",async page=>{
    await page.setViewportSize({width:390,height:650});
    await page.evaluate(()=>{
      const original=window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke=async(command,args)=>{
        if(command==="change_file_permissions")throw Object.assign(new Error("Administratorhelfer benötigt das signierte App-Paket"),{code:"unsupported"});
        return original(command,args);
      };
    });
    await page.getByRole("listbox",{name:/Linke Dateiliste/}).getByRole("option",{name:/Alpha.txt/}).click();await page.keyboard.press("Meta+I");const dialog=page.getByRole("dialog",{name:"Informationen · Alpha.txt"}),editor=dialog.locator('[data-file-info-editor="permission"]');await editor.locator("summary").click();await editor.getByRole("checkbox",{name:"Mit Administratorfreigabe ändern",exact:true}).check();await editor.getByLabel("Unix-Rechte (oktal)").fill("640");await editor.getByRole("button",{name:"Rechte anwenden",exact:true}).click();
    const status=editor.locator('[data-file-info-permission-status]');await status.getByText("Administratorhelfer benötigt das signierte App-Paket",{exact:true}).waitFor();
    assert.equal(await status.evaluate(n=>document.activeElement===n),true);
    assert.equal(await status.evaluate(n=>{const r=n.getBoundingClientRect(),b=document.querySelector("#modal-body").getBoundingClientRect();return r.top>=b.top&&r.bottom<=b.bottom;}),true);
    await dialog.getByRole("button",{name:"Schließen",exact:true}).click();
    assert.equal(await dialog.count(),0);
  });
}
