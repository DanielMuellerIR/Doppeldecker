#!/usr/bin/env node
/** Archivdialog mit echter Modal-/Tastaturbedienung und kontrollierten Backendantworten. */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:net";
import { once } from "node:events";
import { mkdir } from "node:fs/promises";
import { chromium } from "playwright";
import { createBrowserCleanup } from "../browser-cleanup.mjs";
const probe = createServer(); probe.listen(0,"127.0.0.1"); await once(probe,"listening"); const port=probe.address().port;probe.close();await once(probe,"close");
const server=spawn("python3",["tools/mock-ui-server.py","--port",String(port)],{cwd:new URL("../..",import.meta.url),stdio:["ignore","ignore","pipe"]});
// Das HTTP-Zugriffslog darf den Server bei vielen Szenarien nicht am vollen Pipepuffer blockieren.
server.stderr.resume();
const cleanup=createBrowserCleanup(server);const url=`http://127.0.0.1:${port}`;
const evidence=process.env.DOPPELDECKER_ARCHIVE_READ_SCREENSHOTS||"/tmp/doppeldecker-archive-read-check";
const mock=`
(() => {
 const original=window.__TAURI__.core.invoke;
 window.__ARCHIVE_CALLS__=[];window.__ARCHIVE_CONFIG__={};window.__ARCHIVE_HOLDS__={open:[],preview:[],list:[]};window.__ARCHIVE_ACTIVE__=0;window.__ARCHIVE_MAX_ACTIVE__=0;
 let sequence=0;
 const row=(rowId,displayName,extra={})=>({rowId,displayName,kind:'file',entryId:rowId,uncompressedBytes:24,duplicateName:false,issues:[],canPreview:true,...extra});
 const issue=(rowId,displayName,message)=>row(rowId,displayName,{kind:'problem',canPreview:false,issues:[{code:'unsupported_type',message}]});
 const roots=()=>[
  row('text','Text.txt'),row('dup1','dup.txt',{duplicateName:true}),row('dup2','dup.txt',{duplicateName:true}),
  row('empty-record','empty/',{kind:'directory',directoryId:'empty',canPreview:false}),
  row('folder-dup1','dup/',{kind:'directory',directoryId:'dup-dir',canPreview:false,duplicateName:true}),
  row('folder-dup2','dup/',{kind:'directory',directoryId:'dup-dir',canPreview:false,duplicateName:true}),
  row('same-file','same'),row('same-dir','same/',{kind:'directory',directoryId:'same-dir',canPreview:false}),
  issue('raw','Rohbytes: FF 6E 61 6D 65','Ungültiges UTF-8; Originalbytes bleiben erhalten.'),
  issue('unsafe','../außerhalb.txt','Unsicherer Archivpfad; kein Inhaltslesen.'),
  issue('link','link/','Symlink oder nicht unterstützter Typ.'),
  row('synthetic-a','a',{entryId:null,kind:'directory',directoryId:'a',canPreview:false}),
  row('nfd','e\\u0301.txt'),row('image','Bild.png'),row('html','Seite.html'),row('svg','Bild.svg'),row('unsupported','Groß.bin')
 ];
 window.__ARCHIVE_LISTING__=(sessionId,directoryId,page)=>{
  let rows=directoryId==='root'?roots():directoryId==='same-dir'?[row('child','child')]:directoryId==='a'?[row('synthetic-b','b',{entryId:null,kind:'directory',directoryId:'ab',canPreview:false})]:directoryId==='ab'?[row('deep-file','file')]:[];
  if(window.__ARCHIVE_CONFIG__.pages&&directoryId==='root')rows=Array.from({length:103},(_,i)=>row('page-'+i,'Datei-'+i+'.txt'));
  const crumbs=[{directoryId:'root',label:'Archiv'}];
  if(directoryId!=='root')crumbs.push({directoryId,label:directoryId==='same-dir'?'same/':directoryId});
  return {sessionId,directoryId,breadcrumbs:crumbs,page,pageSize:100,totalRows:rows.length,rows:rows.slice(page*100,(page+1)*100),catalog:{complete:!window.__ARCHIVE_CONFIG__.partial&&!window.__ARCHIVE_CONFIG__.viewLimit,entryCount:22,displayedEntryCount:window.__ARCHIVE_CONFIG__.viewLimit?18:22,problemCount:3,stopReason:window.__ARCHIVE_CONFIG__.viewLimit?{code:'resource_limit',message:'Ansichtsbudget erreicht: 18 abgebildet, 4 nicht dargestellt.'}:window.__ARCHIVE_CONFIG__.partial?{code:'invalid_input',message:'Metadatenbudget erreicht.'}:null}};
 };
 const hold=async(kind,args)=>{if(window.__ARCHIVE_CONFIG__['hold'+kind[0].toUpperCase()+kind.slice(1)])await new Promise(resolve=>window.__ARCHIVE_HOLDS__[kind].push({args,resolve}));};
 window.__TAURI__.core.invoke=async(command,args)=>{
  if(!['open_archive_read','list_archive_directory','preview_archive_entry','cancel_archive_read','close_archive_read'].includes(command))return original(command,args);
  window.__ARCHIVE_CALLS__.push({command,args:structuredClone(args)});
  if(command==='cancel_archive_read'||command==='close_archive_read')return null;
  if(command==='open_archive_read'){
   const sessionId='archive-session-'+(++sequence);await hold('open',args);
   const remote=args.request.endpoint.provider!=='local';
   if(window.__ARCHIVE_CONFIG__.openError)throw {code:'invalid_data',message:'Kein lesbares ZIP-Zentralverzeichnis.'};
   return {requestId:args.requestId,sessionId,canExtract:!window.__ARCHIVE_CONFIG__.denyExtract,canExtractFile:!window.__ARCHIVE_CONFIG__.denyExtract,origin:remote?(window.__ARCHIVE_CONFIG__.origin||'SFTP · '+args.request.path):null,listing:window.__ARCHIVE_LISTING__(sessionId,'root',0)};
  }
  if(command==='list_archive_directory'){
   await hold('list',args);
   if(window.__ARCHIVE_CONFIG__.expired)throw {code:'expired',message:'Archivsitzung ist abgelaufen; bitte neu öffnen.'};
   const value=window.__ARCHIVE_LISTING__(args.sessionId,args.directoryId,args.page);
   if(window.__ARCHIVE_CONFIG__.foreignList)value.sessionId='foreign';return value;
  }
  window.__ARCHIVE_ACTIVE__++;window.__ARCHIVE_MAX_ACTIVE__=Math.max(window.__ARCHIVE_MAX_ACTIVE__,window.__ARCHIVE_ACTIVE__);
  try{
   await hold('preview',args);
   if(window.__ARCHIVE_CONFIG__.previewError)throw {code:window.__ARCHIVE_CONFIG__.previewError,message:'Archivquelle hat sich geändert.'};
   let content={kind:'text',text:args.entryId==='dup1'?'Erster eigener Eintrag':args.entryId==='dup2'?'Zweiter eigener Eintrag':'Verifizierter Inhalt · '+args.entryId};
   if(args.entryId==='html'||args.entryId==='svg')content={kind:'text',text:'<script>window.ARCHIVE_PWN=true</script><svg onload="window.ARCHIVE_PWN=true"></svg>'};
   if(args.entryId==='image')content={kind:'image',data_url:window.__ARCHIVE_CONFIG__.brokenImage?'data:image/png;base64,AAAA':'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aV1kAAAAASUVORK5CYII=',width:1,height:1};
   if(args.entryId==='unsupported')content={kind:'unsupported',reason:'Inhalt überschreitet die begrenzte Archivvorschau.'};
   return {requestId:args.requestId,sessionId:window.__ARCHIVE_CONFIG__.foreignPreview?'foreign':args.sessionId,entryId:window.__ARCHIVE_CONFIG__.foreignEntry?'foreign':args.entryId,preview:{bytes:24,content,limitations:[]}};
  }finally{window.__ARCHIVE_ACTIVE__--;}
 };
})();`;
let browser;let count=0;
try {
 for(let attempt=0;attempt<80;attempt++){cleanup.checkServer();try{if((await fetch(url)).ok)break;}catch{}await new Promise(resolve=>setTimeout(resolve,25));}
 browser=await cleanup.launch(chromium,{headless:true,executablePath:process.env.DOPPELDECKER_PLAYWRIGHT_EXECUTABLE||undefined});await mkdir(evidence,{recursive:true});
 async function pageFor({theme='light',width=1100,height=800,config={}}={}){
  const page=await browser.newPage({viewport:{width,height},colorScheme:theme});const errors=[];page.on('pageerror',error=>errors.push(error.message));
  await page.route('**/mock-tauri.js',async route=>{const response=await route.fetch();await route.fulfill({response,body:(await response.text())+mock});});
  await page.goto(url+'?keyboard=1',{waitUntil:'networkidle'});await page.getByRole('listbox',{name:/Linke Dateiliste/}).waitFor();
  await page.evaluate(async({theme,config})=>{
   const {setTheme}=await import('/theme.js');setTheme(theme);Object.assign(window.__ARCHIVE_CONFIG__,config);
   const {createArchiveReadUi}=await import('/archive-read-ui.js');
   window.__ARCHIVE_PANES__={l:{provider:config.provider||'local',profileId:config.provider?'artificial-profile':undefined,profileRevision:config.provider?17:undefined,path:'/artificial/quelle',navigationRevision:1,tabs:{active:{id:'tab-a'}}}};
   window.__ARCHIVE_SELECTION__=[{name:'Archiv.zip',is_file:true,is_symlink:false}];
   window.__ARCHIVE_UI__=createArchiveReadUi({panes:window.__ARCHIVE_PANES__,selectedEntries:()=>window.__ARCHIVE_SELECTION__,showDock:()=>{},extractSelectionReady:config.extractActions?()=>window.__ARCHIVE_EXTRACT_ATTEMPTS__++:null});window.__ARCHIVE_EXTRACT_ATTEMPTS__=0;
  },{theme,config});return {page,errors};
 }
 const dialog=page=>page.getByRole('dialog',{name:/ZIP-Inhalt ·/});
 const list=page=>page.getByRole('listbox',{name:'Archiveinträge',exact:true});
 async function open(page){await page.getByRole('listbox',{name:/Linke Dateiliste/}).focus();await page.evaluate(()=>window.__ARCHIVE_UI__.open('l'));await dialog(page).waitFor();}
 async function ready(page){await dialog(page).getByText(/Archiveinträge ·/).waitFor();}
 async function choose(page,id){await dialog(page).locator('[data-row-id="'+id+'"]').click();}
 async function preview(page,id){await choose(page,id);await dialog(page).getByRole('button',{name:'Vorschau (F3)',exact:true}).click();}
 async function finish(page,errors){assert.deepEqual(errors,[]);await page.keyboard.press('Escape');assert.equal(await dialog(page).count(),0);assert.equal(await page.evaluate(()=>document.activeElement.getAttribute('role')),'listbox');await page.close();}
 async function test(name,body){await body();count++;console.log('OK '+name);}
 await test('Menü- und Kontextverdrahtung ohne Erweiterungsbeweis',async()=>{
  const {page,errors}=await pageFor();await page.getByRole('listbox',{name:/Linke Dateiliste/}).getByRole('option',{name:/Alpha.txt/}).click();
  await page.keyboard.press('Shift+F10');await page.getByRole('menuitem',{name:'ZIP-Inhalt anzeigen …',exact:true}).waitFor();
  await page.evaluate(()=>window.__MOCK_EMIT__('menu','archive_read'));await ready(page);assert.match(await dialog(page).innerText(),/Alpha.txt/);await page.keyboard.press('Escape');
  assert.equal(await page.locator('#context-menu').count(),0);assert.equal(await page.evaluate(()=>document.activeElement.getAttribute('role')),'listbox');
  await page.getByRole('listbox',{name:/Linke Dateiliste/}).getByRole('option',{name:/Alpha.txt/}).click({button:'right'});await page.getByRole('menuitem',{name:'ZIP-Inhalt anzeigen …',exact:true}).click();await ready(page);await finish(page,errors);
 });
 await test('Katalog, virtuelle und leere Ordner, explizite Duplikate und unverändertes NFD',async()=>{
  const {page,errors}=await pageFor();await open(page);await ready(page);assert.equal(await list(page).getByRole('option').count(),17);
  assert.equal(await dialog(page).locator('[data-row-id^="folder-dup"]').count(),2);assert.equal(await dialog(page).locator('[data-row-id^="dup"]').count(),2);
  assert.equal(await dialog(page).locator('[data-row-id="nfd"] .archive-read-name').textContent(),'e\u0301.txt');
  await choose(page,'same-dir');await page.keyboard.press('Enter');await dialog(page).locator('[data-row-id="child"]').waitFor();assert.equal(await list(page).getByRole('option').count(),1);
  await dialog(page).getByRole('button',{name:'Archiv',exact:true}).click();await choose(page,'empty-record');await page.keyboard.press('Enter');await dialog(page).getByText('Dieser virtuelle Ordner ist leer.').waitFor();
  await dialog(page).getByRole('button',{name:'Archiv',exact:true}).click();await choose(page,'synthetic-a');await page.keyboard.press('Enter');await choose(page,'synthetic-b');await page.keyboard.press('Enter');await dialog(page).locator('[data-row-id="deep-file"]').waitFor();
  const calls=await page.evaluate(()=>window.__ARCHIVE_CALLS__);assert.equal(calls.filter(c=>c.command==='preview_archive_entry').length,0);for(const c of calls.filter(c=>c.command==='list_archive_directory'))assert.deepEqual(Object.keys(c.args).sort(),['directoryId','page','sessionId']);await finish(page,errors);
 });
 await test('Duplikate werden ausschließlich mit eigenem Eintrags-ID gelesen; F3 und A11y',async()=>{
  const {page,errors}=await pageFor();await open(page);await ready(page);await choose(page,'dup1');await page.keyboard.press('F3');await dialog(page).getByText('Erster eigener Eintrag',{exact:true}).waitFor();
  await preview(page,'dup2');await dialog(page).getByText('Zweiter eigener Eintrag',{exact:true}).waitFor();
  assert.equal(await list(page).getAttribute('aria-activedescendant'),await dialog(page).locator('[data-row-id="dup2"]').getAttribute('id'));
  assert.equal(await list(page).locator('[aria-selected="true"]').count(),1);assert.equal(await dialog(page).getByRole('button',{name:/System-App/}).count(),0);
  assert.deepEqual(await page.evaluate(()=>window.__ARCHIVE_CALLS__.filter(c=>c.command==='preview_archive_entry').map(c=>c.args.entryId)),['dup1','dup2']);await finish(page,errors);
 });
 await test('Problematische Rohbytes, Pfade und Typen bleiben sichtbar und nicht lesbar',async()=>{
  const {page,errors}=await pageFor();await open(page);await ready(page);
  for(const [id,message] of [['raw','Ungültiges UTF-8'],['unsafe','Unsicherer Archivpfad'],['link','Symlink oder']]){await choose(page,id);assert.equal(await dialog(page).getByRole('button',{name:'Vorschau (F3)',exact:true}).isDisabled(),true);assert.match(await dialog(page).getByRole('region',{name:'Archivvorschau'}).innerText(),new RegExp(message));}
  assert.equal(await page.evaluate(()=>window.__ARCHIVE_CALLS__.filter(c=>c.command==='preview_archive_entry').length),0);await finish(page,errors);
 });
 await test('Teilkataloghinweis bleibt beim Ordnerwechsel erhalten',async()=>{
  const {page,errors}=await pageFor({config:{partial:true}});await open(page);await ready(page);assert.match(await dialog(page).innerText(),/Teilkatalog: Metadatenbudget erreicht/);await choose(page,'same-dir');await page.keyboard.press('Enter');await dialog(page).locator('[data-row-id="child"]').waitFor();assert.match(await dialog(page).innerText(),/Teilkatalog: Metadatenbudget erreicht/);await finish(page,errors);
 });
 await test('Ansichtsgrenze zählt dargestellte und ausgelassene Einträge auch nach Navigation',async()=>{
  const {page,errors}=await pageFor({config:{viewLimit:true}});await open(page);await ready(page);assert.match(await dialog(page).innerText(),/18 dargestellt · 4 nicht dargestellt/);assert.match(await dialog(page).innerText(),/18 abgebildet, 4 nicht dargestellt/);await choose(page,'same-dir');await page.keyboard.press('Enter');await dialog(page).locator('[data-row-id="child"]').waitFor();assert.match(await dialog(page).innerText(),/18 dargestellt · 4 nicht dargestellt/);await finish(page,errors);
 });
 await test('Text/HTML/SVG inert, statisches PNG, große und beschädigte Vorschau',async()=>{
  const {page,errors}=await pageFor();await open(page);await ready(page);
  for(const id of ['html','svg']){await preview(page,id);await dialog(page).getByRole('region',{name:'Dateiinhalt als Text'}).waitFor();assert.equal(await page.evaluate(()=>window.ARCHIVE_PWN),undefined);assert.equal(await dialog(page).locator('script,svg,iframe,object').count(),0);}
  await preview(page,'image');await dialog(page).getByRole('img',{name:'Bild.png',exact:true}).waitFor();assert.equal(await dialog(page).getByRole('img').evaluate(image=>image.complete&&image.naturalWidth===1),true);
  await preview(page,'unsupported');await dialog(page).getByText('Inhalt überschreitet die begrenzte Archivvorschau.',{exact:true}).waitFor();
  await page.evaluate(()=>window.__ARCHIVE_CONFIG__.brokenImage=true);await preview(page,'image');await dialog(page).getByText('Das Bild konnte nicht dekodiert werden.',{exact:true}).waitFor();await finish(page,errors);
 });
 await test('Frühes Escape cancelt exakten Öffnungstoken und verwirft späte A nach B',async()=>{
  const {page,errors}=await pageFor({config:{holdOpen:true}});await open(page);await page.waitForFunction(()=>window.__ARCHIVE_HOLDS__.open.length===1);const first=await page.evaluate(()=>window.__ARCHIVE_CALLS__.find(c=>c.command==='open_archive_read').args.requestId);await page.keyboard.press('Escape');
  assert.equal(await page.evaluate(id=>window.__ARCHIVE_CALLS__.some(c=>c.command==='cancel_archive_read'&&c.args.requestId===id),first),true);
  await page.evaluate(()=>window.__ARCHIVE_CONFIG__.holdOpen=false);await open(page);await ready(page);await page.evaluate(()=>window.__ARCHIVE_HOLDS__.open.shift().resolve());await page.waitForFunction(()=>window.__ARCHIVE_CALLS__.some(c=>c.command==='close_archive_read'&&c.args.sessionId==='archive-session-1'));
  await preview(page,'text');await dialog(page).getByText('Verifizierter Inhalt · text',{exact:true}).waitFor();assert.equal(await page.evaluate(()=>window.__ARCHIVE_CALLS__.some(c=>c.command==='cancel_sync')),false);await finish(page,errors);
 });
 await test('Preview A cancelt bei Auswahlwechsel; B startet erst nach Rückkehr und bleibt nutzbar',async()=>{
  const {page,errors}=await pageFor({config:{holdPreview:true}});await open(page);await ready(page);await preview(page,'dup1');await page.waitForFunction(()=>window.__ARCHIVE_HOLDS__.preview.length===1);const first=await page.evaluate(()=>window.__ARCHIVE_HOLDS__.preview[0].args.requestId);
  await choose(page,'dup2');assert.equal(await dialog(page).getByRole('button',{name:'Vorschau (F3)',exact:true}).isDisabled(),true);await page.keyboard.press('F3');assert.equal(await page.evaluate(()=>window.__ARCHIVE_CALLS__.filter(c=>c.command==='preview_archive_entry').length),1);
  assert.equal(await page.evaluate(id=>window.__ARCHIVE_CALLS__.some(c=>c.command==='cancel_archive_read'&&c.args.requestId===id),first),true);
  await page.evaluate(()=>{window.__ARCHIVE_CONFIG__.holdPreview=false;window.__ARCHIVE_HOLDS__.preview.shift().resolve();});await dialog(page).getByRole('button',{name:'Vorschau (F3)',exact:true}).waitFor();await page.waitForFunction(()=>!document.querySelector('.archive-read-modal .btn.primary').disabled);
  assert.equal(await dialog(page).getByText('Erster eigener Eintrag',{exact:true}).count(),0);await dialog(page).getByRole('button',{name:'Vorschau (F3)',exact:true}).click();await dialog(page).getByText('Zweiter eigener Eintrag',{exact:true}).waitFor();assert.equal(await page.evaluate(()=>window.__ARCHIVE_MAX_ACTIVE__),1);await finish(page,errors);
 });
 await test('Navigation cancelt Inhalt und ein neuer Ordnerread wartet auf alten Worker',async()=>{
  const {page,errors}=await pageFor({config:{holdPreview:true}});await open(page);await ready(page);await preview(page,'text');await page.waitForFunction(()=>window.__ARCHIVE_HOLDS__.preview.length===1);await choose(page,'same-dir');await page.keyboard.press('Enter');await dialog(page).locator('[data-row-id="child"]').waitFor();await choose(page,'child');await page.keyboard.press('F3');assert.equal(await page.evaluate(()=>window.__ARCHIVE_CALLS__.filter(c=>c.command==='preview_archive_entry').length),1);await page.evaluate(()=>{window.__ARCHIVE_CONFIG__.holdPreview=false;window.__ARCHIVE_HOLDS__.preview.shift().resolve();});await page.waitForFunction(()=>!document.querySelector('.archive-read-modal .btn.primary').disabled);await dialog(page).getByRole('button',{name:'Vorschau (F3)',exact:true}).click();await dialog(page).getByText('Verifizierter Inhalt · child',{exact:true}).waitFor();assert.equal(await page.evaluate(()=>window.__ARCHIVE_MAX_ACTIVE__),1);await finish(page,errors);
 });
 await test('Tabdrift während aktiver Vorschau cancelt exakt und zeigt alte Bytes nicht',async()=>{
  const {page,errors}=await pageFor({config:{holdPreview:true}});await open(page);await ready(page);await preview(page,'text');await page.waitForFunction(()=>window.__ARCHIVE_HOLDS__.preview.length===1);const id=await page.evaluate(()=>window.__ARCHIVE_HOLDS__.preview[0].args.requestId);await page.evaluate(()=>window.__ARCHIVE_PANES__.l.tabs.active.id='changed-tab');await dialog(page).getByText(/Die Auswahl oder die Ausgangspane hat sich geändert/).first().waitFor();assert.equal(await page.evaluate(id=>window.__ARCHIVE_CALLS__.some(c=>c.command==='cancel_archive_read'&&c.args.requestId===id),id),true);await page.evaluate(()=>window.__ARCHIVE_HOLDS__.preview.shift().resolve());assert.equal(await dialog(page).getByText('Verifizierter Inhalt · text',{exact:true}).count(),0);await finish(page,errors);
 });
 for(const drift of ['selection','pane','tab'])await test('Späte Öffnungsantwort bei '+drift+'-Drift wird verworfen',async()=>{
  const {page,errors}=await pageFor({config:{holdOpen:true}});await open(page);await page.waitForFunction(()=>window.__ARCHIVE_HOLDS__.open.length===1);
  await page.evaluate(drift=>{if(drift==='selection')window.__ARCHIVE_SELECTION__=[{name:'Andere.zip',is_file:true}];else if(drift==='pane')window.__ARCHIVE_PANES__.l.path='/different';else window.__ARCHIVE_PANES__.l.tabs.active.id='tab-b';window.__ARCHIVE_HOLDS__.open.shift().resolve();},drift);
  await dialog(page).getByText(/Die Auswahl oder die Ausgangspane hat sich geändert/).first().waitFor();assert.equal(await list(page).getByRole('option').count(),0);await finish(page,errors);
 });
 for(const fault of ['foreignPreview','foreignEntry'])await test('Fremde '+fault+'-Antwort wird nicht dargestellt',async()=>{
  const {page,errors}=await pageFor({config:{[fault]:true}});await open(page);await ready(page);await preview(page,'text');await dialog(page).getByText(/Die Archivantwort passt nicht/).first().waitFor();assert.equal(await dialog(page).getByText('Verifizierter Inhalt · text',{exact:true}).count(),0);await finish(page,errors);
 });
 await test('Fremde Listingantwort und abgelaufene Sitzung werden sichtbar beendet',async()=>{
  for(const fault of ['foreignList','expired']){const {page,errors}=await pageFor({config:{[fault]:true}});await open(page);await ready(page);await choose(page,'same-dir');await page.keyboard.press('Enter');await dialog(page).getByText(fault==='expired'?/Archivsitzung ist abgelaufen/:/Die Archivantwort passt nicht/).first().waitFor();assert.equal(await dialog(page).getByRole('button',{name:'Vorschau (F3)',exact:true}).isDisabled(),true);await finish(page,errors);}
 });
 await test('Seitenrand hält Fokus innen und Escape wirksam; höchstens100 Zeilen',async()=>{
  const {page,errors}=await pageFor({config:{pages:true}});await open(page);await ready(page);assert.equal(await list(page).getByRole('option').count(),100);await dialog(page).getByRole('button',{name:'Weiter',exact:true}).click();await dialog(page).getByText('Seite 2 von 2 · 103 Zeilen',{exact:true}).waitFor();assert.equal(await list(page).getByRole('option').count(),3);assert.equal(await dialog(page).evaluate(d=>d.contains(document.activeElement)),true);await dialog(page).getByRole('button',{name:'Zurück',exact:true}).click();await dialog(page).getByText('Seite 1 von 2 · 103 Zeilen',{exact:true}).waitFor();assert.equal(await dialog(page).evaluate(d=>d.contains(document.activeElement)),true);await finish(page,errors);
 });
 await test('Remote-Momentaufnahme bindet Profil; Navigation, Vorschau und alle gebundenen Entpackumfänge verfügbar',async()=>{
  for(const provider of ['sftp','ftps','webdav','smb']){
   const {page,errors}=await pageFor({config:{provider,extractActions:true}});await open(page);await ready(page);
   assert.match(await dialog(page).innerText(),/Momentaufnahme von SFTP · \/artificial\/quelle\/Archiv.zip/);
   assert.match(await dialog(page).innerText(),/Änderungen am Remoteoriginal verändern diese Ansicht nicht/);
   assert.match(await dialog(page).innerText(),/Entpacken verwendet diese geladenen Bytes/);
   assert.doesNotMatch(await dialog(page).innerText(),/Remote-Entpacken ist noch nicht verfügbar/);
   assert.equal(await dialog(page).getByRole('button',{name:'Ganzes Archiv entpacken …',exact:true}).isDisabled(),false);
   await choose(page,'text');assert.equal(await dialog(page).getByRole('button',{name:'Datei entpacken …',exact:true}).isDisabled(),false);
   await preview(page,'text');await dialog(page).getByText('Verifizierter Inhalt · text',{exact:true}).waitFor();
   await preview(page,'image');await dialog(page).getByRole('img',{name:'Bild.png',exact:true}).waitFor();
   await choose(page,'same-dir');await page.keyboard.press('Enter');await dialog(page).locator('[data-row-id="child"]').waitFor();
   assert.equal(await dialog(page).getByRole('button',{name:'Geöffneten Ordner entpacken …',exact:true}).isDisabled(),false);
   const request=await page.evaluate(()=>window.__ARCHIVE_CALLS__.find(c=>c.command==='open_archive_read').args.request);
   assert.deepEqual(request,{endpoint:{provider,profileId:'artificial-profile',profileRevision:17,root:'/artificial/quelle'},path:'/artificial/quelle/Archiv.zip'});
   assert.equal(await page.evaluate(()=>window.__ARCHIVE_EXTRACT_ATTEMPTS__),0);await finish(page,errors);
  }
 });
 await test('Remote-Laden lässt sich mit Escape abbrechen; verspätete Antwort verändert neue lokale Sitzung nicht',async()=>{
  const {page,errors}=await pageFor({config:{provider:'sftp',holdOpen:true,extractActions:true}});await open(page);
  await dialog(page).getByText(/entfernte Dateien werden vollständig als Momentaufnahme geladen/).waitFor();
  await page.waitForFunction(()=>window.__ARCHIVE_HOLDS__.open.length===1);
  for(let index=0;index<8;index++)await page.keyboard.press('Tab');
  assert.equal(await dialog(page).evaluate(d=>d.contains(document.activeElement)),true);
  await page.screenshot({path:evidence+'/remote-archive-loading.png'});
  assert.equal(await dialog(page).getByRole('button',{name:'Ganzes Archiv entpacken …',exact:true}).isDisabled(),true);
  await page.keyboard.press('Escape');await page.evaluate(()=>{window.__ARCHIVE_CONFIG__.holdOpen=false;window.__ARCHIVE_PANES__.l.provider='local';});
  await open(page);await ready(page);await page.evaluate(()=>window.__ARCHIVE_HOLDS__.open.shift().resolve());
  await page.waitForFunction(()=>window.__ARCHIVE_CALLS__.some(c=>c.command==='close_archive_read'&&c.args.sessionId==='archive-session-1'));
  assert.equal(await dialog(page).getByText(/Momentaufnahme von/).count(),0);
  assert.equal(await dialog(page).getByRole('button',{name:'Ganzes Archiv entpacken …',exact:true}).isDisabled(),false);await finish(page,errors);
 });
 await test('Entpackangebot folgt der Backendfähigkeit auch bei lokaler Pane',async()=>{
  const {page,errors}=await pageFor({config:{denyExtract:true,extractActions:true}});await open(page);await ready(page);await choose(page,'text');
  assert.equal(await dialog(page).getByRole('button',{name:'Ganzes Archiv entpacken …',exact:true}).isDisabled(),true);
  assert.equal(await dialog(page).getByRole('button',{name:'Datei entpacken …',exact:true}).isDisabled(),true);await finish(page,errors);
 });
 await test('Remote-Herkunft bleibt Text; Providerfehler ist sichtbar',async()=>{
  const {page,errors}=await pageFor({config:{provider:'sftp',origin:'SFTP · /daten/<script>window.ARCHIVE_PWN=true</script>.zip'}});await open(page);await ready(page);
  assert.match(await dialog(page).innerText(),/<script>/);assert.equal(await dialog(page).locator('script,iframe,object').count(),0);assert.equal(await page.evaluate(()=>window.ARCHIVE_PWN),undefined);await finish(page,errors);
  const failure=await pageFor({config:{provider:'sftp',openError:true}});await open(failure.page);await dialog(failure.page).getByText(/Kein lesbares ZIP/).waitFor();await finish(failure.page,failure.errors);
 });
 for(const theme of ['light','dark'])for(const width of [1100,390])await test('Remote visuell '+theme+' '+width+'px',async()=>{
  const {page,errors}=await pageFor({theme,width,height:850,config:{provider:'sftp',extractActions:true}});await open(page);await ready(page);await preview(page,'image');await dialog(page).getByRole('img',{name:'Bild.png',exact:true}).waitFor();
  assert.equal(await dialog(page).evaluate(d=>d.scrollWidth<=d.clientWidth),true);assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true);
  await page.mouse.move(0,0);await page.waitForTimeout(180);await page.screenshot({path:evidence+'/remote-archive-read-'+theme+'-'+width+'.png'});await finish(page,errors);
 });
 for(const theme of ['light','dark'])for(const width of [1100,390])await test('Visuell '+theme+' '+width+'px',async()=>{
  const {page,errors}=await pageFor({theme,width,height:850,config:{partial:true}});await open(page);await ready(page);await preview(page,'text');await dialog(page).getByText('Verifizierter Inhalt · text',{exact:true}).waitFor();assert.equal(await dialog(page).evaluate(d=>d.scrollWidth<=d.clientWidth),true);assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true);
  assert.equal(await dialog(page).evaluate(d=>getComputedStyle(d).backgroundColor.startsWith('rgb(')),true);await page.mouse.move(0,0);await page.waitForTimeout(180);await page.screenshot({path:evidence+'/archive-read-'+theme+'-'+width+'.png'});await finish(page,errors);
 });
 console.log('Archivübersicht: '+count+' Browserfälle grün. Screenshots: '+evidence);
}finally{await cleanup.close(browser);}
