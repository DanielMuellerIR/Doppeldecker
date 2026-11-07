#!/usr/bin/env node
/** ZIP-Fachmodule mit echter Modal-/Queuebedienung und kontrollierter IPC. */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:net";
import { once } from "node:events";
import { mkdir } from "node:fs/promises";
import { chromium } from "playwright";
import { createBrowserCleanup } from "../browser-cleanup.mjs";
const probe = createServer(); probe.listen(0,"127.0.0.1"); await once(probe,"listening"); const port = probe.address().port; probe.close(); await once(probe,"close");
const server = spawn("python3",["tools/mock-ui-server.py","--port",String(port)],{cwd:new URL("../..",import.meta.url),stdio:["ignore","ignore","pipe"]});
// Assetzugriffe füllen sonst die Pipe und blockieren spätere Seitenaufrufe.
server.stderr.resume();
const cleanup = createBrowserCleanup(server); const url = `http://127.0.0.1:${port}`;
const mock = `
(() => {
 const original = window.__TAURI__.core.invoke;
 window.__ZIP_CALLS__=[]; window.__ZIP_CONFIG__={}; window.__ZIP_GLOBAL_CANCELS__=0;
 window.__ZIP_PREVIEW__ = root => ({source_root:root,target:root+"/exact.zip",files:2,directories:1,source_bytes:12345,commit_class:window.__ZIP_CONFIG__.weak?"best_effort":"assured",file_flush:true,directory_flush:!window.__ZIP_CONFIG__.weak,
 issues:Array.from({length:95},(_,i)=>({path:root+"/Hinweis-"+i+"-"+"LangerName".repeat(9),reason:"Pfadgenaue Grenze "+i,content_omitted:i<3}))});
 window.__TAURI__.core.invoke=async(command,args)=>{
  if(command==="cancel_sync")window.__ZIP_GLOBAL_CANCELS__+=1;
  if (!command.includes("archive_pack")) return original(command,args);
  window.__ZIP_CALLS__.push({command,args});
  if(command==="discard_archive_pack")return null;
  if(command==="prepare_archive_pack"){
   if(window.__ZIP_CONFIG__.hold)await new Promise(resolve=>{window.__ZIP_RELEASE__=resolve;});
   if(args.request.provider!=="local")throw {code:"unsupported",message:"ZIP-Erzeugung derzeit nur für lokale Panes"};
   const preview=window.__ZIP_PREVIEW__(args.request.root);
   if(args.request.target)preview.target="/private/artificial-workspace/content/result.zip";
   return {preparationId:"backend-zip-id",binding:{digest:Array(32).fill(9),preview},uploadTarget:args.request.target?args.request.target.provider.toUpperCase()+" · "+args.request.target.root:null};
  }
  if(args.preparationId!=="backend-zip-id")throw new Error("Ungebundene ZIP-Ausführung");
  if(window.__ZIP_CONFIG__.drift)throw {code:"plan_drift",message:"ZIP-Auswahl änderte sich; erneut vorbereiten"};
  return "archive-operation-1";
 };
})();`;
let browser;
try {
 for(let attempt=0;attempt<80;attempt++){cleanup.checkServer();try{if((await fetch(url)).ok)break;}catch{}await new Promise(resolve=>setTimeout(resolve,25));}
 browser=await cleanup.launch(chromium,{headless:true,executablePath:process.env.DOPPELDECKER_PLAYWRIGHT_EXECUTABLE||undefined});
 const evidence=process.env.DOPPELDECKER_ARCHIVE_SCREENSHOTS;if(evidence)await mkdir(evidence,{recursive:true});
 async function pageFor(theme="light",contextOptions={}){
  const page=await browser.newPage({viewport:{width:650,height:520},colorScheme:theme,...contextOptions});const errors=[];page.on("pageerror",error=>errors.push(error.message));
  await page.route("**/mock-tauri.js",async route=>{const response=await route.fetch();await route.fulfill({response,body:(await response.text())+mock});});
  await page.goto(url+"?keyboard=1",{waitUntil:"networkidle"});await page.getByRole("listbox",{name:/Linke Dateiliste/}).waitFor();
  await page.evaluate(async theme=>{const {setTheme}=await import("/theme.js");setTheme(theme);},theme);return {page,errors};
 }
 async function open(page){await page.getByRole("listbox",{name:/Linke Dateiliste/}).getByRole("option",{name:/Alpha.txt/}).click();await page.evaluate(()=>window.__MOCK_EMIT__("menu","archive_pack"));await page.getByRole("dialog").getByRole("button",{name:"Vorschau erstellen",exact:true}).waitFor();}
 for(const theme of ["light","dark"]){
  const {page,errors}=await pageFor(theme);await open(page);const dialog=page.getByRole("dialog");
  await page.getByLabel("Archivname",{exact:true}).fill("gewählt.zip");await page.getByLabel("Kompression",{exact:true}).selectOption("stored");
  if(evidence)await page.screenshot({path:evidence+"/archive-options-"+theme+".png"});
  await page.evaluate(()=>window.__ZIP_CONFIG__.weak=true);await dialog.getByRole("button",{name:"Vorschau erstellen",exact:true}).click();
  await dialog.getByRole("button",{name:"ZIP erstellen",exact:true}).waitFor();assert.match(await dialog.innerText(),/3 Eintrag\/Teilbaum ausgelassen, 92 Metadatenhinweis/);
  assert.match(await dialog.innerText(),/Eine gleichzeitig neu angelegte Zieldatei kann dabei ersetzt werden/);assert.match(await dialog.innerText(),/Ordner-Flush nicht garantiert/);
  if(evidence)await page.screenshot({path:evidence+"/archive-preview-"+theme+".png"});
  assert.equal(await dialog.locator(".archive-pack-issues p").count(),40);await dialog.getByRole("button",{name:"Weiter",exact:true}).click();await dialog.getByRole("button",{name:"Weiter",exact:true}).click();assert.equal(await dialog.locator(".archive-pack-issues p").count(),15);assert.match(await dialog.innerText(),/Seite 3 von 3 · 95 Hinweise/);
  assert.equal(await dialog.evaluate(element=>element.scrollWidth<=element.clientWidth),true);
  if(evidence)await page.screenshot({path:evidence+"/archive-"+theme+".png"});
  assert.equal(await page.evaluate(()=>window.__ZIP_CALLS__.some(call=>call.command==="confirm_archive_pack")),false);
  await dialog.getByRole("button",{name:"ZIP erstellen",exact:true}).click();await page.getByText("ZIP-Erzeugung eingereiht",{exact:true}).waitFor();
  const calls=await page.evaluate(()=>window.__ZIP_CALLS__);assert.equal(calls[0].args.request.targetName,"gewählt.zip");assert.equal(calls[0].args.request.options.compression,"stored");assert.deepEqual(calls.find(call=>call.command==="confirm_archive_pack"),{command:"confirm_archive_pack",args:{preparationId:"backend-zip-id"}});assert.deepEqual(errors,[]);await page.close();
 }
 {
  const {page}=await pageFor();await open(page);await page.evaluate(()=>window.__ZIP_CONFIG__.hold=true);await page.getByRole("button",{name:"Vorschau erstellen",exact:true}).click();await page.waitForFunction(()=>Boolean(window.__ZIP_RELEASE__));await page.keyboard.press("Escape");await page.evaluate(()=>window.__ZIP_RELEASE__());await page.waitForFunction(()=>window.__ZIP_CALLS__.some(call=>call.command==="discard_archive_pack"&&call.args.preparationId==="backend-zip-id"));assert.equal(await page.evaluate(()=>window.__ZIP_CALLS__.some(call=>call.command==="confirm_archive_pack")),false);assert.equal(await page.getByRole("dialog").isVisible(),false);await page.close();
 }
 {
  const {page}=await pageFor();await open(page);await page.getByRole("button",{name:"Vorschau erstellen",exact:true}).click();await page.getByRole("button",{name:"ZIP erstellen",exact:true}).waitFor();await page.getByLabel("Archivname",{exact:true}).fill("neu.zip");assert.equal(await page.getByRole("button",{name:"ZIP erstellen",exact:true}).count(),0);assert.equal(await page.locator(".archive-pack-issues").count(),0);await page.keyboard.press("Escape");await page.close();
 }
 {
  const {page}=await pageFor();await page.evaluate(async()=>{const {confirmArchivePackPreview}=await import("/archive-pack-ui.js");window.__ZIP_REVIEW__=confirmArchivePackPreview(window.__ZIP_PREVIEW__("/source"));});await page.getByRole("dialog").getByRole("button",{name:"Geändertes ZIP erstellen",exact:true}).click();assert.equal(await page.evaluate(()=>window.__ZIP_REVIEW__),true);await page.close();
 }
 {
  const {page,errors}=await pageFor();
  await page.evaluate(()=>{
   const queue=window.__DOPPELDECKER_MOCK_QUEUE__;queue.paused=true;queue.revision+=1;
   const preview=window.__ZIP_PREVIEW__("/mock/quelle");const binding={digest:Array(32).fill(7),preview};
   const request={source_root:"/mock/quelle",items:["/mock/quelle/Alpha.txt"],target_dir:"/mock/quelle",target_name:"exact.zip",options:{compression:"stored"},sandbox:null};
   queue.entries=[{operation_id:"archive-attention",operation:{type:"ArchivePack",request,expected:binding},state:"AwaitingConfirmation",attempts:1,progress:{},attention:{type:"ArchivePackChanged",reason:"ZIP-Bestand änderte sich",confirmation:"backend-only",current:binding}}];
   const invoke=window.__TAURI__.core.invoke;window.__TAURI__.core.invoke=async(command,args)=>{if(command==="approve_queue_entry")window.__ZIP_APPROVAL__=structuredClone(args);return invoke(command,args);};
   window.__MOCK_EMIT__("queue-updated",structuredClone(queue));
  });
  await page.locator("#btn-queue").click();await page.getByRole("button",{name:"ZIP-Hinweise",exact:true}).click();await page.getByRole("dialog").getByRole("button",{name:"Schließen",exact:true}).click();
  await page.getByRole("button",{name:"Plan bestätigen",exact:true}).click();const dialog=page.getByRole("dialog",{name:"Geänderte ZIP-Wirkung bestätigen",exact:true});await dialog.waitFor();assert.equal(await dialog.locator(".archive-pack-issues p").count(),40);
  await dialog.getByRole("button",{name:"Geändertes ZIP erstellen",exact:true}).click();await page.waitForFunction(()=>Boolean(window.__ZIP_APPROVAL__));
  const approval=await page.evaluate(()=>window.__ZIP_APPROVAL__);assert.equal(approval.effectsConfirmed,true);assert.equal(approval.expectedAttention.type,"ArchivePackChanged");assert.equal(Object.hasOwn(approval,"confirmation"),false);assert.deepEqual(errors,[]);await page.close();
 }
 {
  const {page,errors}=await pageFor();
  await page.evaluate(()=>{
   const queue=window.__DOPPELDECKER_MOCK_QUEUE__;queue.revision+=1;
   const preview=window.__ZIP_PREVIEW__("/mock/quelle");
   queue.entries=[{operation_id:"archive-validating",operation:{type:"ArchivePack",request:{source_root:"/mock/quelle",items:["/mock/quelle/Alpha.txt"],target_dir:"/mock/quelle",target_name:"exact.zip",options:{compression:"deflate"},sandbox:null},expected:{digest:Array(32).fill(8),preview}},state:"Validating",attempts:1,progress:{}}];
   const invoke=window.__TAURI__.core.invoke;window.__TAURI__.core.invoke=async(command,args)=>{if(command==="cancel_sync"){window.__ZIP_QUEUE_CANCEL__=true;return null;}return invoke(command,args);};
   window.__MOCK_EMIT__("queue-updated",structuredClone(queue));
  });
  await page.locator("#btn-cancel").waitFor({state:"visible"});assert.match(await page.locator("#prog-text").innerText(),/Prüfe ZIP-Quellen/);await page.locator("#btn-cancel").click();assert.equal(await page.evaluate(()=>window.__ZIP_QUEUE_CANCEL__),true);assert.deepEqual(errors,[]);await page.close();
 }
 for(const boundary of ["last","first"]){
  const {page,errors}=await pageFor();await open(page);await page.getByRole("button",{name:"Vorschau erstellen",exact:true}).click();const dialog=page.getByRole("dialog");await dialog.getByRole("button",{name:"ZIP erstellen",exact:true}).waitFor();
  await dialog.getByRole("button",{name:"Weiter",exact:true}).focus();await page.keyboard.press("Enter");await page.keyboard.press("Enter");
  assert.equal(await dialog.getByRole("button",{name:"Zurück",exact:true}).evaluate(element=>element===document.activeElement),true,"Letzte Seite behält den Fokus am bedienbaren Gegenknopf");
  if(boundary==="first"){
   await page.keyboard.press("Enter");await page.keyboard.press("Enter");
   assert.equal(await dialog.getByRole("button",{name:"Weiter",exact:true}).evaluate(element=>element===document.activeElement),true,"Erste Seite behält den Fokus am bedienbaren Gegenknopf");
  }
  await page.keyboard.press("Escape");assert.equal(await dialog.isVisible(),false);const calls=await page.evaluate(()=>window.__ZIP_CALLS__);
  assert.equal(calls.filter(call=>call.command==="discard_archive_pack"&&call.args.preparationId==="backend-zip-id").length,1);
  assert.equal(calls.some(call=>call.command==="confirm_archive_pack"),false);assert.equal(await page.evaluate(()=>window.__ZIP_GLOBAL_CANCELS__),0);assert.deepEqual(errors,[]);await page.close();
 }
 for(const confirm of [false,true])for(const boundary of ["last","first"]){
  const {page,errors}=await pageFor();await page.evaluate(async confirm=>{const {confirmArchivePackPreview}=await import("/archive-pack-ui.js");window.__ZIP_BOUNDARY_REVIEW__=confirmArchivePackPreview(window.__ZIP_PREVIEW__("/source"),{confirm});},confirm);
  const dialog=page.getByRole("dialog");await dialog.getByRole("button",{name:"Weiter",exact:true}).focus();await page.keyboard.press("Enter");await page.keyboard.press("Enter");
  assert.equal(await dialog.getByRole("button",{name:"Zurück",exact:true}).evaluate(element=>element===document.activeElement),true);
  if(boundary==="first"){await page.keyboard.press("Enter");await page.keyboard.press("Enter");assert.equal(await dialog.getByRole("button",{name:"Weiter",exact:true}).evaluate(element=>element===document.activeElement),true);}
  await page.keyboard.press("Escape");assert.equal(await page.evaluate(()=>window.__ZIP_BOUNDARY_REVIEW__),false);assert.equal(await dialog.isVisible(),false);assert.deepEqual(await page.evaluate(()=>window.__ZIP_CALLS__),[]);assert.equal(await page.evaluate(()=>window.__ZIP_GLOBAL_CANCELS__),0);assert.deepEqual(errors,[]);await page.close();
 }
 for (const [provider,theme,width] of [["sftp","light",800],["ftps","dark",390],["webdav","light",390],["smb","dark",800]]) {
  const {page,errors}=await pageFor(theme);await page.setViewportSize({width,height:850});
  await page.evaluate(async provider=>{
   const {createArchivePackUi}=await import("/archive-pack-ui.js");
   window.__ZIP_PANES__={left:{provider:"local",path:"/artificial/source",navigationRevision:1,tabs:{active:{id:"left"}}},right:{provider,path:"/remote/Archivziel",profileId:"target-profile",profileRevision:7,navigationRevision:2,tabs:{active:{id:"right"}}}};
   window.__ZIP_REMOTE__=createArchivePackUi({panes:window.__ZIP_PANES__,activeSide:()=>"left",selectedEntries:()=>[{name:"file.txt",is_file:true}],blocked:()=>false,showDock:()=>{},showError:()=>{}});window.__ZIP_REMOTE__.open();
  },provider);
  await page.getByLabel("Ziel",{exact:true}).selectOption("remote");await page.getByRole("button",{name:"Vorschau erstellen",exact:true}).click();
  const dialog=page.getByRole("dialog");await dialog.getByRole("button",{name:"ZIP erstellen und hochladen",exact:true}).waitFor();
  assert.match(await dialog.innerText(),/remote\/Archivziel/);assert.doesNotMatch(await dialog.innerText(),/private\/artificial-workspace/);
  assert.equal(await dialog.evaluate(element=>element.scrollWidth<=element.clientWidth),true);
  if(evidence)await page.screenshot({path:evidence+"/remote-pack-"+provider+"-"+theme+"-"+width+".png"});
  await dialog.getByRole("button",{name:"ZIP erstellen und hochladen",exact:true}).click();
  const calls=await page.evaluate(()=>window.__ZIP_CALLS__);const request=calls.find(call=>call.command==="prepare_archive_pack").args.request;
  assert.deepEqual(request.target,{provider,root:"/remote/Archivziel",profileId:"target-profile",profileRevision:7});
  assert.deepEqual(calls.find(call=>call.command==="confirm_archive_pack").args,{preparationId:"backend-zip-id"});assert.deepEqual(errors,[]);await page.close();
 }
 for(const change of ["profile","tab"]) {
  const {page,errors}=await pageFor();
  await page.evaluate(async()=>{
   const {createArchivePackUi}=await import("/archive-pack-ui.js");window.__ZIP_CONFIG__.hold=true;
   window.__ZIP_PANES__={left:{provider:"local",path:"/artificial/source",navigationRevision:1,tabs:{active:{id:"left"}}},right:{provider:"webdav",path:"/remote",profileId:"target-profile",profileRevision:7,navigationRevision:2,tabs:{active:{id:"right"}}}};
   createArchivePackUi({panes:window.__ZIP_PANES__,activeSide:()=>"left",selectedEntries:()=>[{name:"file.txt",is_file:true}],blocked:()=>false,showDock:()=>{},showError:()=>{}}).open();
  });
  await page.getByLabel("Ziel",{exact:true}).selectOption("remote");await page.getByRole("button",{name:"Vorschau erstellen",exact:true}).click();await page.waitForFunction(()=>Boolean(window.__ZIP_RELEASE__));
  await page.evaluate(change=>{if(change==="profile")window.__ZIP_PANES__.right.profileRevision++;else window.__ZIP_PANES__.right.tabs.active.id="replacement";window.__ZIP_RELEASE__();},change);
  await page.getByRole("dialog").getByText(/Pane-Ziel hat sich geändert/).waitFor();
  assert.equal(await page.evaluate(()=>window.__ZIP_CALLS__.some(call=>call.command==="confirm_archive_pack")),false);assert.deepEqual(errors,[]);await page.close();
 }
 for(const choice of ["overwrite","both","cancel"]) {
  const {page,errors}=await pageFor(choice === "both" ? "dark" : "light",{locale:"en-US",timezoneId:"UTC"});await page.setViewportSize({width:390,height:850});
  await page.evaluate(()=>{
   const queue=window.__DOPPELDECKER_MOCK_QUEUE__;queue.paused=true;queue.revision++;
   queue.entries=[{operation_id:"generated-upload",operation:{type:"ProviderFile",kind:"Copy",source:{provider:"Local",root:"/private/generated/content"},target:{provider:"WebDav",root:"/remote/Ein-langes-Zielverzeichnis",connection:{origin:"https://artificial.example.invalid"}},items:["/private/generated/content/Ergebnis.zip"],destination:"/remote/Ein-langes-Zielverzeichnis"},generated_source:{kind:"zip",label:"Ergebnis.zip",files:1,directories:0,bytes:42,issues:[{path:"source",reason:"Metadaten ausgelassen",content_omitted:false}]},state:"AwaitingConfirmation",attempts:1,progress:{},attention:{type:"ConflictsChanged",current_conflicts:["Ergebnis.zip"],current_conflict_proofs:[],reason:"Zielkonflikt",confirmation:"backend-only"}}];
   window.__ZIP_GENERATED_CALLS__=[];const original=window.__TAURI__.core.invoke;
   window.__TAURI__.core.invoke=async(command,args)=>{
    if(command==="check_provider_file_operation"||command==="enqueue_provider_file_operation") {
     window.__ZIP_GENERATED_CALLS__.push({command,args:structuredClone(args)});
     if(command==="enqueue_provider_file_operation"){queue.entries[0].state="Pending";queue.entries[0].attention=null;queue.revision++;window.__MOCK_EMIT__("queue-updated",structuredClone(queue));return "generated-upload";}
     return {conflicts:["Ergebnis.zip"],conflictDetails:[{relative_name:"Ergebnis.zip",source:{endpoint:"Erzeugtes ZIP",exists:true,kind:"file",size:42,mtime:1},target:{endpoint:"WebDAV-Ziel",exists:true,kind:"file",size:12,mtime:1}}],requiresConflictChoice:false,skipOnly:false,sourceRemovalRequired:false,sourceReadLimitations:[],sourceTree:null,targetCapabilities:{copyNew:true,copyNewBestEffort:false,replace:true,replaceBestEffort:false,blockers:[],limitations:[]},conflictPreparationId:"conflict-bound",keepBothPreparationId:"keep-bound",keepBothNames:["Ergebnis (Kopie).zip"]};
    }
    return original(command,args);
   };
   window.__MOCK_EMIT__("queue-updated",structuredClone(queue));
  });
  await page.locator("#btn-queue").click();const row=page.locator('[data-queue-id="generated-upload"]');await row.getByRole("button",{name:"Konflikte prüfen …",exact:true}).waitFor();
  assert.match(await row.innerText(),/Lokale ZIP-Erzeugung abgeschlossen/);assert.match(await row.innerText(),/artificial.example.invalid/);
  assert.equal(await row.evaluate(element=>element.scrollWidth<=element.clientWidth),true);
  if(evidence)await page.screenshot({path:evidence+"/generated-queue-"+choice+"-390.png"});
  await row.getByRole("button",{name:"Archivhinweise",exact:true}).click();await page.getByRole("dialog",{name:"Archivhinweise",exact:true}).getByText("Metadaten ausgelassen").waitFor();await page.keyboard.press("Escape");
  await row.getByRole("button",{name:"Konflikte prüfen …",exact:true}).click();const dialog=page.getByRole("dialog",{name:"Konflikte beim Kopieren",exact:true});await dialog.waitFor();assert.match(await dialog.innerText(),/01\.01\.1970/);
  await dialog.getByRole("button",{name:choice === "overwrite" ? "Ersetzen" : choice === "both" ? "Beide behalten" : "Abbrechen",exact:true}).click();
  await page.waitForFunction(()=>!document.querySelector('#modal')||document.querySelector('#modal').hidden);
  const calls=await page.evaluate(()=>window.__ZIP_GENERATED_CALLS__);const prepared=calls.find(call=>call.command==="check_provider_file_operation").args.request;
  assert.deepEqual(prepared,{kind:"copy",source:{provider:"local",root:""},target:{provider:"local",root:""},items:[],destination:"",generatedOperationId:"generated-upload"});
  const enqueued=calls.find(call=>call.command==="enqueue_provider_file_operation");
  if(choice === "cancel")assert.equal(enqueued,undefined);else {assert.equal(enqueued.args.request.generatedOperationId,"generated-upload");assert.equal(enqueued.args.request.overwrite,choice === "overwrite");assert.equal(enqueued.args.request.keepBothPreparationId,choice === "both" ? "keep-bound" : null);}
  const entries=await page.evaluate(()=>window.__DOPPELDECKER_MOCK_QUEUE__.entries);assert.equal(entries.length,1);assert.equal(entries[0].operation_id,"generated-upload");assert.equal(entries[0].generated_source.label,"Ergebnis.zip");
  for(const state of ["Cancelled", "Completed", "retained"]) {
   await page.evaluate(state=>{const queue=window.__DOPPELDECKER_MOCK_QUEUE__;queue.entries[0].state=state === "retained" ? "Completed" : state;queue.entries[0].attention=null;queue.entries[0].temp_recovery_paths=state === "retained" ? [queue.entries[0].operation.source.root] : [];queue.revision++;window.__MOCK_EMIT__("queue-updated",structuredClone(queue));},state);
   const show=row.getByRole("button",{name:"Zwischenstand zeigen",exact:true});
   if(state === "Completed")await show.waitFor({state:"detached"});else await show.waitFor();
   assert.equal(await row.evaluate(element=>element.scrollWidth<=element.clientWidth),true);
   if(evidence && choice === "overwrite")await page.screenshot({path:evidence+"/generated-cleanup-"+state+"-390.png"});
  }
  assert.deepEqual(errors,[]);await page.close();
 }
 console.log("ZIP-UI: Zielname/Kompression, gebundene Bestätigung, Best Effort, 95 Hinweise auf 3 Seiten, Queue-Drift, Prüf-/Laufabbruch, Optionsdrift und Pager-Randfokus/Escape grün.");
} finally {await cleanup.close(browser);}
