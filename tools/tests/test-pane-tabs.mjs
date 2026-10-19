import test from "node:test";
import assert from "node:assert/strict";
import { createPaneTabs } from "../../frontend/pane-tabs.js";

const local = path => ({ provider: "local", path });
const remote = path => ({ provider: "ftps", profileId: "fixture", profileRevision: 7, path });
const options = { columns: { size: true }, density: "compact", textSize: "m" };
function model() {
  let serial = 0;
  return createPaneTabs(local("/quelle"), options, { idFactory: () => `tab-${++serial}` });
}

test("Zwei Panes und mehrere Tabs behalten Navigation, Auswahl und Darstellung nach Neustart", () => {
  const left = model();
  const right = model();
  const first = left.active;
  Object.assign(first, {
    history: ["/", "/quelle"], historyIndex: 1,
    selectedNames: ["eins.txt", "zwei.txt"], cursorName: "zwei.txt", anchorName: "eins.txt",
    scrollTop: 321, sort: { key: "modified", dir: -1 },
  });
  first.viewOptions.showHidden = true;
  const second = left.add(remote("/ziel"));
  left.activate(second.id);
  second.history = ["/", "/anderer", "/ziel"];
  second.historyIndex = 2;
  second.selectedNames = ["entfernt.txt"];
  second.viewOptions.columns.owner = true;
  right.active.location.path = "/rechts";
  const restored = createPaneTabs(null, null, { saved: JSON.parse(JSON.stringify(left.snapshot())) });
  assert.equal(restored.active.location.provider, "ftps");
  assert.equal(restored.active.location.profileRevision, 7);
  assert.deepEqual(restored.active.selectedNames, ["entfernt.txt"]);
  assert.equal(restored.active.viewOptions.columns.owner, true);
  restored.activate(first.id);
  assert.deepEqual(restored.active.history, ["/", "/quelle"]);
  assert.deepEqual(restored.active.selectedNames, ["eins.txt", "zwei.txt"]);
  assert.equal(restored.active.cursorName, "zwei.txt");
  assert.equal(restored.active.scrollTop, 321);
  assert.deepEqual(restored.active.sort, { key: "modified", dir: -1 });
  assert.equal(restored.active.viewOptions.columns.owner, false);
  assert.equal(right.active.location.path, "/rechts");
});

test("Verspätete Antworten nach Wechsel, Schließen und neuer Anfrage verändern keinen Tab", () => {
  const pane = model();
  const first = pane.active;
  const old = pane.beginRead(first.id, "listing");
  const newest = pane.beginRead(first.id, "listing");
  assert.equal(pane.finishRead(old, () => assert.fail("überholte Antwort")), false);
  assert.equal(pane.finishRead(newest, tab => { tab.runtime.entries = ["aktuell"]; }), true);
  const switched = pane.beginRead(first.id, "size");
  const second = pane.add(local("/zweiter"));
  pane.activate(second.id);
  assert.throws(() => pane.beginRead(first.id, "listing"), /aktive Tab/);
  pane.activate(first.id);
  assert.equal(pane.finishRead(switched, () => assert.fail("Antwort aus altem Aufenthalt")), false);
  const closed = pane.beginRead(first.id, "listing");
  pane.close(first.id);
  assert.equal(pane.finishRead(closed, () => assert.fail("geschlossener Tab")), false);
  assert.equal(pane.active.id, second.id);
  assert.throws(() => pane.close(second.id), /letzte Tab/);
});

test("Identische IDs in einem anderen Fenster verleihen keine Leseberechtigung", () => {
  const first = model();
  const second = model();
  const token = first.beginRead(first.active.id, "listing");
  second.beginRead(second.active.id, "listing");
  assert.equal(second.finishRead(token, () => assert.fail("fremdes Modell")), false);
  assert.equal(first.finishRead({ ...token }, () => assert.fail("kopierter Token")), false);
  assert.equal(first.finishRead(token, () => {}), true);
});

test("Profilwechsel entwertet auch inaktive Tabs, lässt Navigation und andere Provider bestehen", () => {
  const pane = model();
  const tab = pane.add(remote("/entfernt"));
  pane.activate(tab.id);
  const token = pane.beginRead(tab.id, "listing");
  tab.runtime.entries = ["alt"];
  pane.invalidateProfile("ftps", "fixture");
  assert.equal(pane.finishRead(token, () => assert.fail("altes Profil")), false);
  assert.deepEqual(tab.runtime.entries, []);
  assert.equal(tab.location.path, "/entfernt");
  assert.equal(tab.runtime.status, "unloaded");
  assert.equal(pane.tabs[0].location.path, "/quelle");
});

test("Persistenz enthält nur Navigation, keine Runtime, Handles oder zusätzliche Optionsfelder", () => {
  const pane = model();
  const tab = pane.active;
  tab.runtime.handle = { password: "synthetischer-testwert" };
  tab.permit = "synthetisch";
  tab.location.password = "synthetisch";
  tab.sort.password = "synthetisch";
  tab.viewOptions.password = "synthetisch";
  tab.viewOptions.columns.password = true;
  pane.beginRead(tab.id, "listing");
  const encoded = JSON.stringify(pane.snapshot());
  assert.doesNotMatch(encoded, /password|permit|runtime|synthetisch|generation/);
  const restored = createPaneTabs(null, null, { saved: JSON.parse(encoded) });
  assert.equal(restored.active.runtime.reads.size, 0);
  assert.equal(restored.active.runtime.status, "unloaded");
});

test("Tab-Grenze und ungültige Identitäten werden vor Zustandsänderung abgelehnt", () => {
  const pane = model();
  for (let i = 1; i < 32; i += 1) pane.add(local(`/ordner-${i}`));
  assert.throws(() => pane.add(local("/zu-viel")), /32 Tabs/);
  assert.equal(pane.tabs.length, 32);
  assert.throws(() => pane.activate("fehlt"), /nicht vorhanden/);
  const saved = pane.snapshot();
  saved.tabs[1].id = saved.tabs[0].id;
  assert.throws(() => createPaneTabs(null, null, { saved }), /Ungültiger/);
});
