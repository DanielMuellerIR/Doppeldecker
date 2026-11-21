import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import ts from "typescript";
import { checkIpcArguments } from "../ipc-argument-check.mjs";

function argumentPayloads(sourceText) {
  const path = "ipc-counterexamples.ts";
  const source = ts.createSourceFile(path, sourceText, ts.ScriptTarget.ES2022, true);
  const options = { strict: true, noEmit: true, types: [] };
  const host = ts.createCompilerHost(options);
  const readSource = host.getSourceFile.bind(host);
  host.getSourceFile = (file, ...args) => file === path ? source : readSource(file, ...args);
  const checker = ts.createProgram([path], options, host).getTypeChecker();
  const payloads = source.statements.filter(ts.isExpressionStatement)
    .map(statement => statement.expression.arguments[0]);
  return { checker, payloads };
}

test("Prüft bekannte verschachtelte IPC-Felder und meldet nicht belegte Varianten", () => {
  const { checker, payloads } = argumentPayloads(`
    declare const provider: "local" | "sftp";
    invoke({ request: {
      side: "left", endpoint: { provider: "smb", profileId: "smb-1", root: "/", profileRevision: null },
      names: ["A"], recalculate: false, clientToken: 3,
    } });
    invoke({ request: {
      side: "left", endpoint: { provider: "sftp", profile_id: "sftp-1", root: "/" },
      names: [7], sessionGeneration: 18446744073709551616, recalculate: false, clientToken: -1,
    } });
    invoke({ request: {
      side: "left", endpoint: { provider, root: "/" },
      names: ["A"], recalculate: false, clientToken: 3,
    } });
    invoke({ request: { endpoint: { provider: "sftp", root: "/" } } });
  `);
  const signature = { request: "DirectorySizeRequest" };
  const [valid, invalid, mixed] = payloads.slice(0, 3)
    .map(payload => checkIpcArguments(checker, payload, signature));
  assert.deepEqual([...valid.issues, ...valid.open], []);
  for (const expected of [
    "Pflichtfeld „request.endpoint.profileId“ fehlt.",
    "Unbekanntes Argument „request.endpoint.profile_id“.",
    "„request.names[]“ erwartet String",
    "„request.sessionGeneration“ erwartet Option<u64>",
    "„request.clientToken“ erwartet u64",
  ]) {
    assert.ok([...invalid.issues].some(issue => issue.startsWith(expected)), expected);
  }
  assert.deepEqual([...mixed.issues], []);
  assert.deepEqual([...mixed.open], ["request.endpoint: Profil-ID nicht eindeutig an die Endpunkt-Variante gebunden"]);
  const opaque = checkIpcArguments(checker, payloads[3], { request: "OtherRequest" });
  assert.deepEqual([...opaque.issues], []);
  assert.deepEqual([...opaque.open], ["request: Serde-Typ OtherRequest"]);
});


test("Namenssuche prüft Suchoptionen und unterscheidet Standardwerte von null", () => {
  const { checker, payloads } = argumentPayloads(`
    const endpoint = { provider: "local", root: "/" } as const;
    invoke({ request: { side: "left", endpoint, sessionGeneration: null, clientToken: 12,
      options: { query: "Probe" } } });
    invoke({ request: { side: "left", endpoint, clientToken: 12,
      options: { query: 7, caseSensitive: null, includeHidden: "ja", case_sensitive: true } } });
  `);
  const signature = { request: "NameSearchRequest" };
  const [valid, invalid] = payloads.map(payload => checkIpcArguments(checker, payload, signature));
  assert.deepEqual([...valid.issues, ...valid.open], []);
  assert.deepEqual([...invalid.open], []);
  assert.deepEqual([...invalid.issues], [
    "„request.options.query“ erwartet String, erhält 7.",
    "„request.options.caseSensitive“ erwartet Default<bool>, erhält null.",
    '„request.options.includeHidden“ erwartet Default<bool>, erhält "ja".',
    "Unbekanntes Argument „request.options.case_sensitive“.",
  ]);
});

test("Lokale Übertragung prüft Konfliktliste und optionale Backendbindungen", () => {
  const { request } = JSON.parse(readFileSync(new URL("./file-transfer-wire.json", import.meta.url)));
  const { checker, payloads } = argumentPayloads(`
    invoke({ request: ${JSON.stringify(request)} });
    invoke({ request: {
      items: [], destDir: "/ziel", overwrite: false, expectedConflicts: [],
      keepBothPreparationId: null, conflictPreparationId: null,
    } });
    invoke({ request: {
      items: [], dest_dir: "/ziel", overwrite: false, expectedConflicts: [false],
      keepBothTargets: [], conflictPreparationId: 11, replaceBestEffortExpected: "false",
    } });
  `);
  const signature = { request: "LocalFileTransferRequest" };
  const [bound, optional, invalid] = payloads.map(payload => checkIpcArguments(checker, payload, signature));
  assert.deepEqual([...bound.issues, ...bound.open, ...optional.issues, ...optional.open, ...invalid.open], []);
  assert.deepEqual([...invalid.issues], [
    "Pflichtfeld „request.destDir“ fehlt.",
    "„request.expectedConflicts[]“ erwartet String, erhält false.",
    "„request.conflictPreparationId“ erwartet Option<String>, erhält 11.",
    '„request.replaceBestEffortExpected“ erwartet Option<bool>, erhält "false".',
    "Unbekanntes Argument „request.dest_dir“.",
    "Unbekanntes Argument „request.keepBothTargets“.",
  ]);
});

test("Archivvorbereitung prüft Kompression, Datei-IDs und verschachtelte Zielprofile", () => {
  const { pack, extract } = JSON.parse(readFileSync(new URL("./archive-mutation-wire.json", import.meta.url)));
  const { checker, payloads } = argumentPayloads(`
    invoke({ request: ${JSON.stringify(pack.request)} });
    invoke({ request: ${JSON.stringify(extract.request)} });
    invoke({ request: ${JSON.stringify(extract.selectionRequest)} });
    invoke({ request: ${JSON.stringify({
      ...pack.request, items: [1], options: { compression: "gzip" },
      target: { provider: "sftp", root: "/remote", profile_id: "old-name" },
    })} });
    invoke({ request: ${JSON.stringify({ ...extract.selectionRequest, entryId: 1, sourcePath: "/foreign.zip" })} });
  `);
  const packSignature = { request: "ArchivePackPrepareRequest" };
  const selectionSignature = { request: "ArchiveExtractSelectionPrepareRequest" };
  for (const [index, signature] of [
    [0, packSignature], [1, { request: "ArchiveExtractPrepareRequest" }], [2, selectionSignature],
  ]) {
    const valid = checkIpcArguments(checker, payloads[index], signature);
    assert.deepEqual([...valid.issues, ...valid.open], []);
  }
  const invalidPack = checkIpcArguments(checker, payloads[3], packSignature);
  assert.deepEqual([...invalidPack.open], []);
  assert.deepEqual([...invalidPack.issues], [
    "„request.items[]“ erwartet String, erhält 1.",
    '„request.options.compression“ erwartet ArchivePackCompression, erhält "gzip".',
    "Pflichtfeld „request.target.profileId“ fehlt.",
    "Unbekanntes Argument „request.target.profile_id“.",
  ]);
  const injectedSelection = checkIpcArguments(checker, payloads[4], selectionSignature);
  assert.deepEqual([...injectedSelection.open], []);
  assert.deepEqual([...injectedSelection.issues], [
    "„request.entryId“ erwartet Option<String>, erhält 1.",
    "Unbekanntes Argument „request.sourcePath“.",
  ]);
});

test("Queue-Freigaben benötigen den unveränderten Backendbeleg des richtigen Modus", () => {
  const { checker, payloads } = argumentPayloads(`
    declare const local: import("./frontend/local-sync-wire.js").LocalSyncPreparationDigestWire;
    declare const content: import("./frontend/content-sync-wire.js").ContentSyncPreparationDigestWire;
    declare const attention: import("./frontend/queue-snapshot-wire.js").QueueAttentionWire;
    invoke({ preparationDigest: local });
    invoke({ preparationDigest: content });
    invoke({ expectedAttention: attention });
    invoke({ preparationDigest: content });
    invoke({ preparationDigest: local });
    invoke({ expectedAttention: { type: "PlanChanged", reason: "Plan neu", confirmation: "alte Phrase" } });
  `);
  const local = { preparationDigest: "LocalSyncPreparationDigest" };
  const content = { preparationDigest: "ContentSyncPreparationDigest" };
  const attention = { expectedAttention: "doppeldecker_core::QueueAttention" };
  const results = [local, content, attention, local, content, attention]
    .map((signature, index) => checkIpcArguments(checker, payloads[index], signature));
  assert.deepEqual(results.flatMap(({ open }) => [...open]), []);
  assert.deepEqual(results.slice(0, 3).flatMap(({ issues }) => [...issues]), []);
  assert.deepEqual(results.slice(3, 5).flatMap(({ issues }) => [...issues]), [
    "„preparationDigest“ erwartet LocalSyncPreparationDigest, erhält ContentSyncPreparationDigestWire.",
    "„preparationDigest“ erwartet ContentSyncPreparationDigest, erhält LocalSyncPreparationDigestWire.",
  ]);
  assert.equal(results[5].issues.size, 1);
  assert.match([...results[5].issues][0], /^„expectedAttention“ erwartet doppeldecker_core::QueueAttention/);
});

test("Desktop-Einstellungen prüfen die optionalen Patch-Felder und Werte", () => {
  const { patch } = JSON.parse(readFileSync(new URL("./desktop-preferences-wire.json", import.meta.url)));
  const { checker, payloads } = argumentPayloads(`
    invoke({ patch: ${JSON.stringify(patch)}, onlyIfUninitialized: true });
    invoke({ patch: { theme: null } });
    invoke({ patch: { restore_session: true, theme: "system", localFolderSizes: "ja" } });
  `);
  const signature = { patch: "PreferencePatch", onlyIfUninitialized: "Option<bool>" };
  const [migration, partial, invalid] = payloads.map(payload => checkIpcArguments(checker, payload, signature));
  assert.deepEqual([...migration.issues, ...migration.open, ...partial.issues, ...partial.open, ...invalid.open], []);
  assert.deepEqual([...invalid.issues], [
    '„patch.theme“ erwartet Option<Theme>, erhält "system".',
    '„patch.localFolderSizes“ erwartet Option<bool>, erhält "ja".',
    "Unbekanntes Argument „patch.restore_session“.",
  ]);
});

test("Dupliziervorbereitung akzeptiert Quellpfade und meldet mitgesandte Zielnamen", () => {
  const { local, remote } = JSON.parse(readFileSync(new URL("./duplicate-wire.json", import.meta.url)));
  const { checker, payloads } = argumentPayloads(`
    invoke({ request: ${JSON.stringify(local.request)} });
    invoke({ request: ${JSON.stringify(remote.request)} });
    invoke({ request: { provider: "local", root: "/quelle", items: [1], targets: ["/fremd"] } });
  `);
  const signature = { request: "DuplicateRequest" };
  const [localResult, remoteResult, forged] = payloads.map(payload => checkIpcArguments(checker, payload, signature));
  assert.deepEqual([...localResult.issues, ...localResult.open, ...remoteResult.issues, ...remoteResult.open, ...forged.open], []);
  assert.deepEqual([...forged.issues], [
    "„request.items[]“ erwartet String, erhält 1.",
    "Unbekanntes Argument „request.targets“.",
  ]);
});

test("Verbindungsprofile prüfen Protokollfelder, Authentifizierungsvarianten und Pin-Längen", () => {
  const fixture = JSON.parse(readFileSync(new URL("./connection-profile-wire.json", import.meta.url)));
  const providers = ["sftp", "ftps", "webdav", "smb"];
  const profileTypes = ["SftpProfile", "FtpsProfile", "WebDavProfile", "SmbProfile"];
  const requests = providers.map(provider => {
    const { revision, ...profile } = fixture[provider].profile;
    return { profile, profileRevision: revision, secretId: null };
  });
  const [{ profile: sftp }] = requests;
  const auth = sftp.connection.authentication;
  const privateKey = { type: "private_key", path: "/mock/key", public_key_sha256: Array(32).fill(7) };
  const agent = { type: "ssh_agent", public_key_sha256: Array(32).fill(5) };
  const withAuth = authentication => ({ profile: { ...sftp, connection: { ...sftp.connection, authentication } } });
  const invalidAuth = withAuth({ ...auth, secret: { ...auth.secret, password: "wire-probe" } });
  const invalidDigest = {
    profile: { ...sftp, connection: { ...sftp.connection, host_key: { ...sftp.connection.host_key, sha256: [7, 256] } } },
  };
  const { checker, payloads } = argumentPayloads(
    [...requests, withAuth(privateKey), withAuth(agent), invalidAuth, invalidDigest]
      .map(request => `invoke(${JSON.stringify(request)});`).join("\n"),
  );
  const signature = profile => ({ profile, profileRevision: "Option<u64>", secretId: "Option<String>" });
  for (const [index, profile] of [...profileTypes, "SftpProfile", "SftpProfile"].entries()) {
    const { issues, open } = checkIpcArguments(checker, payloads[index], signature(profile));
    assert.deepEqual([...issues, ...open], []);
  }
  const issues = index => [...checkIpcArguments(checker, payloads[index], signature("SftpProfile")).issues];
  assert.deepEqual(issues(6), ["Unbekanntes Argument „profile.connection.authentication.secret.password“."]);
  assert.deepEqual(issues(7), [
    "„profile.connection.host_key.sha256“ erwartet genau 32 Array-Einträge.",
    "„profile.connection.host_key.sha256[]“ erwartet u8, erhält 256.",
  ]);
  assert.ok([...checkIpcArguments(checker, payloads[3], signature("WebDavProfile")).issues]
    .includes("Unbekanntes Argument „profile.connection.server_guid“."));
});

test("Dateieigenschaften prüfen verschachtelte Editoren, Varianten und Freigabefelder", () => {
  const fixture = JSON.parse(readFileSync(new URL("./file-property-wire.json", import.meta.url)));
  const { tags, comments, flags, visibility } = fixture;
  const requests = [
    [tags.read.request, "FinderTagReadRequest"],
    [tags.edit.request, "FinderTagEditRequest"],
    [comments.read.request, "FinderCommentReadRequest"],
    [comments.edit.request, "FinderCommentEditRequest"],
    [flags.read.request, "FileFlagReadRequest"],
    [flags.edit.request, "FileFlagEditRequest"],
    [visibility.read.request, "VisibilityReadRequest"],
    [visibility.edit.request, "VisibilityEditRequest"],
    ...["clear", "clear_legacy", "remove"].map(type => [
      { ...tags.edit.request, edit: type === "remove" ? { type, name: "Tag" } : { type } },
      "FinderTagEditRequest",
    ]),
  ];
  const invalid = [
    [{ ...tags.edit.request, edit: { type: "clear_legacy", previousName: "Alt" } }, "FinderTagEditRequest"],
    [{ ...tags.edit.request, edit: { type: "upsert", tag: { name: "Neu", color: 256 }, previous_name: "Alt" } }, "FinderTagEditRequest"],
    [{ ...comments.read.request, requestPermission: null }, "FinderCommentReadRequest"],
    [{ ...flags.edit.request, kind: "systemImmutable" }, "FileFlagEditRequest"],
    [{ ...visibility.edit.request, hidden: "ja", inode: 7 }, "VisibilityEditRequest"],
  ];
  const all = [...requests, ...invalid];
  const { checker, payloads } = argumentPayloads(all
    .map(([request]) => `invoke({request: ${JSON.stringify(request)}});`).join("\n"));
  const results = all.map(([, request], index) => checkIpcArguments(checker, payloads[index], { request }));
  assert.deepEqual(results.flatMap(({ open }) => [...open]), []);
  assert.deepEqual(results.slice(0, requests.length).flatMap(({ issues }) => [...issues]), []);
  assert.deepEqual(results.slice(requests.length).flatMap(({ issues }) => [...issues]), [
    "Unbekanntes Argument „request.edit.previousName“.",
    "„request.edit.tag.color“ erwartet u8, erhält 256.",
    "Unbekanntes Argument „request.edit.previous_name“.",
    "„request.requestPermission“ erwartet Default<bool>, erhält null.",
    '„request.kind“ erwartet FileFlagKind, erhält "systemImmutable".',
    '„request.hidden“ erwartet bool, erhält "ja".',
    "Unbekanntes Argument „request.inode“.",
  ]);
});

test("Löschbestätigung benötigt den durchgereichten Backend-Plan, nicht einen gleichnamigen Typ", () => {
  const { endpoint, delete: deletion } = JSON.parse(
    readFileSync(new URL("./routine-file-wire.json", import.meta.url)),
  );
  const { checker, payloads } = argumentPayloads(`
    interface RemoteDeletePlanWire { readonly bytes: number; readonly entries: number; }
    declare const forgedPlan: RemoteDeletePlanWire;
    const endpoint = ${JSON.stringify(endpoint)} as const;
    const selection = { endpoint, items: ${JSON.stringify(deletion.items)} };
    invoke({ request: selection });
    invoke({ request: {
      ...selection, expectedPlan: forgedPlan, mode: "permanent" as const,
      confirmation: null, effectsConfirmed: true,
    } });
  `);
  const [check, enqueue] = [
    checkIpcArguments(checker, payloads[0], { request: "ProviderDeleteRequest" }),
    checkIpcArguments(checker, payloads[1], { request: "EnqueueProviderDeleteRequest" }),
  ];
  assert.deepEqual([...check.issues, ...check.open, ...enqueue.open], []);
  assert.deepEqual([...enqueue.issues], [
    "„request.expectedPlan“ erwartet RemoteDeletePlan, erhält RemoteDeletePlanWire.",
  ]);
});

test("Lokaler Sync prüft verschachtelte Optionen und akzeptiert keine nachgebauten Belege", () => {
  const {request} = JSON.parse(readFileSync(new URL("./local-sync-wire.json", import.meta.url)));
  const { checker, payloads } = argumentPayloads(`
    interface SyncPlanWire { readonly actions: readonly unknown[]; }
    interface LocalSyncPreparationDigestWire extends ReadonlyArray<number> { readonly length: 32; }
    declare const forgedPlan: SyncPlanWire;
    declare const forgedDigest: LocalSyncPreparationDigestWire;
    const options = { mode: "AddOnly" as const, compare: "SizeAndTime" as const, mtime_tolerance: 2 };
    invoke(${JSON.stringify(request)});
    invoke({ ...${JSON.stringify(request)}, options: { ...options, metadata: null, exclude: [7] } });
    invoke({ source: "/q", target: "/z", options, expectedPlan: forgedPlan, simulation: true });
    invoke({ preparationDigest: forgedDigest, replacementsConfirmed: true });
  `);
  const plan = { source: "String", target: "String", options: "SyncOptions" };
  const contracts = [
    plan, plan,
    { ...plan, expectedPlan: "SyncPlan", simulation: "Option<bool>" },
    { preparationDigest: "LocalSyncPreparationDigest", replacementsConfirmed: "bool" },
  ];
  const results = payloads.map((payload, index) => checkIpcArguments(checker, payload, contracts[index]));
  assert.deepEqual(results.flatMap(result => [...result.open]), []);
  assert.deepEqual(results.map(result => [...result.issues]), [
    [],
    [
      "„options.metadata“ erwartet Default<MetadataProfile>, erhält null.",
      "„options.exclude[]“ erwartet String, erhält number.",
    ],
    ["„expectedPlan“ erwartet SyncPlan, erhält SyncPlanWire."],
    ["„preparationDigest“ erwartet LocalSyncPreparationDigest, erhält LocalSyncPreparationDigestWire."],
  ]);
});

test("Dateiöffnung prüft den Endpunkt, Modus und Readback-Wert", () => {
  const { snapshot } = JSON.parse(readFileSync(new URL("./file-opening-wire.json", import.meta.url)));
  const { checker, payloads } = argumentPayloads(
    "invoke({request: " + JSON.stringify(snapshot) + "});" +
    "invoke({request: {endpoint: {provider: 'ftps', root: '/'}, path: '/remote.txt', mode: null, verification: 'full'}});"
  );
  const signature = { request: "OpenRemoteSnapshotRequest" };
  const [valid, invalid] = payloads.map(payload => checkIpcArguments(checker, payload, signature));
  assert.deepEqual([...valid.issues, ...valid.open, ...invalid.open], []);
  assert.deepEqual([...invalid.issues], [
    "Pflichtfeld „request.endpoint.profileId“ fehlt.",
    "„request.mode“ erwartet Default<RemoteOpenMode>, erhält null.",
    '„request.verification“ erwartet Default<SnapshotVerification>, erhält "full".',
  ]);
});

test("Bidirektionale Konfliktentscheidungen prüfen Bytepfade und gebundene Digest-Längen", () => {
  const { resolutions } = JSON.parse(readFileSync(new URL("./bidirectional-wire.json", import.meta.url)));
  const digest = Array(32).fill(7);
  const { checker, payloads } = argumentPayloads(`
    const digest = ${JSON.stringify(digest)} as const;
    const short = [7, 7] as const;
    invoke({ resolutions: ${JSON.stringify(resolutions)}, expectedPlanDigest: digest });
    invoke({ resolutions: [ { path: [[97, 256]], decision: { decision: "keep_both", primary: "left" } } ], expectedPlanDigest: short });
    invoke({ resolutions: [ { path: "a.txt", decision: { decision: "keep_right", primary: "right" } } ], expectedPlanDigest: digest });
  `);
  const signature = { resolutions: "Vec<ConflictResolution>", expectedPlanDigest: "[u8;32]" };
  const [valid, missingCopyName, unexpectedSide] = payloads.map(payload => checkIpcArguments(checker, payload, signature));
  assert.deepEqual([...valid.issues, ...valid.open], []);
  assert.deepEqual([...missingCopyName.open, ...unexpectedSide.open], []);
  assert.deepEqual([...missingCopyName.issues], [
    "„resolutions[].path[][]“ erwartet u8, erhält 256.",
    "Pflichtfeld „resolutions[].decision.conflict_path“ fehlt.",
    "„expectedPlanDigest“ erwartet genau 32 Array-Einträge.",
  ]);
  assert.deepEqual([...unexpectedSide.issues], [
    '„resolutions[].path“ erwartet Vec<Vec<u8>>, erhält "a.txt".',
    "Unbekanntes Argument „resolutions[].decision.primary“.",
  ]);
});

test("Bidirektionale Paarvorbereitung prüft kanonische Felder, Paar-ID und Forward-Option", () => {
  const { registeredPair, resolutions } = JSON.parse(readFileSync(new URL("./bidirectional-wire.json", import.meta.url)));
  const { visibleLeftRole, ...pair } = registeredPair;
  const request = { ...pair, resolutions };
  const { checker, payloads } = argumentPayloads(`
    const valid = ${JSON.stringify(request)} as const;
    invoke({ request: valid });
    invoke({ request: { ...valid, recovery: true } });
    invoke({ pairId: valid.pairId });
    invoke({ request: { ...valid, visibleLeftRole: ${JSON.stringify(visibleLeftRole)} } });
    invoke({ request: { ...valid, right: { provider: "sftp" as const, root: "/daten" }, recovery: null } });
    invoke({ pairId: [1] });
  `);
  const signatures = [
    { request: "PrepareBidirectionalRequest" },
    { request: "ForwardPrepareRequest" },
    { pairId: "PairId" },
    { request: "PrepareBidirectionalRequest" },
    { request: "ForwardPrepareRequest" },
    { pairId: "PairId" },
  ];
  const results = signatures.map((signature, index) => checkIpcArguments(checker, payloads[index], signature));
  assert.deepEqual(results.flatMap(({ open }) => [...open]), []);
  assert.deepEqual(results.slice(0, 3).flatMap(({ issues }) => [...issues]), []);
  assert.deepEqual([...results[3].issues], ["Unbekanntes Argument „request.visibleLeftRole“."]);
  assert.deepEqual([...results[4].issues], [
    "Pflichtfeld „request.right.profileId“ fehlt.",
    "„request.recovery“ erwartet Default<bool>, erhält null.",
  ]);
  assert.deepEqual([...results[5].issues], ["„pairId“ erwartet genau 16 Array-Einträge."]);
});

test("Provider-Übertragung prüft Profilbindung, Konfliktwahl und verschachtelte Queuefelder", () => {
  const { providerTransfer } = JSON.parse(readFileSync(new URL("./file-transfer-wire.json", import.meta.url)));
  const { request, choice, enqueue, intent, generated } = providerTransfer;
  const preview = { request: "ProviderFileRequest", choice: "Option<ProviderFileChoiceRequest>" };
  const cases = [
    [{ request }, preview],
    [{ request, choice }, preview],
    [{ request: generated }, preview],
    [{ request: enqueue }, { request: "EnqueueProviderFileRequest" }],
    [{ request: intent }, { request: "FileIntent" }],
    [{ request: generated }, { request: "FileIntent" }],
    [{ request: { ...request, source: { provider: "ftps", root: "/from", profile_revision: 21 } },
      choice: { id: null, policy: "replace" } }, preview],
    [{ request: { ...enqueue, keepBothTargets: ["/to/alias"], removeSourceBestEffortExpected: "ja" } },
      { request: "EnqueueProviderFileRequest" }],
  ];
  const { checker, payloads } = argumentPayloads(cases
    .map(([payload]) => `invoke(${JSON.stringify(payload)});`).join("\n"));
  const results = cases.map(([, signature], i) => checkIpcArguments(checker, payloads[i], signature));
  assert.deepEqual(results.flatMap(({ open }) => [...open]), []);
  assert.deepEqual(results.slice(0, 5).flatMap(({ issues }) => [...issues]), []);
  assert.deepEqual(results.slice(5).flatMap(({ issues }) => [...issues]), [
    "Unbekanntes Argument „request.generatedOperationId“.",
    "Pflichtfeld „request.source.profileId“ fehlt.",
    "Unbekanntes Argument „request.source.profile_revision“.",
    "„choice.id“ erwartet String, erhält null.",
    '„choice.policy“ erwartet ProviderFilePolicy, erhält "replace".',
    '„request.removeSourceBestEffortExpected“ erwartet Option<bool>, erhält "ja".',
    "Unbekanntes Argument „request.keepBothTargets“.",
  ]);
});

test("Realmodus prüft Providerpaare und Einzelprotokolle getrennt", () => {
  const { content } = JSON.parse(readFileSync(new URL("./real-mode-wire.json", import.meta.url)));
  const { checker, payloads } = argumentPayloads(`
    const options = {
      mode: "AddOnly" as const, compare: "SizeAndTime" as const, mtime_tolerance: 2,
    };
    const remote = {
      profileId: "mock_ftps", profileRevision: 21, direction: "Upload" as const,
      localRoot: "/mock/quelle", remoteRoot: "/", options,
    };
    declare const forged: ContentSyncPreparationDigestWire;
    interface ContentSyncPreparationDigestWire extends ReadonlyArray<number> { readonly length: 32; }
    invoke({ request: ${JSON.stringify(content.request)} });
    invoke({ request: ${JSON.stringify(content.request)} });
    invoke({ request: remote });
    invoke({ request: { ...remote, direction: "upload" as const } });
    invoke({ effectsConfirmed: true, removalPhrase: null, preparationDigest: forged });
  `);
  const signatures = [
    { request: "PrepareContentSyncRequest" },
    { request: "PrepareProviderSyncRequest" },
    { request: "crate::sftp_commands::RemoteSyncRequest" },
    { request: "RemoteSyncRequest" },
    { effectsConfirmed: "bool", removalPhrase: "Option<String>", preparationDigest: "ContentSyncPreparationDigest" },
  ];
  const results = payloads.map((payload, index) => checkIpcArguments(checker, payload, signatures[index]));
  assert.deepEqual(results.flatMap(result => [...result.open]), []);
  assert.deepEqual(results.map(result => [...result.issues]), [
    [], [], [],
    ['„request.direction“ erwartet SftpQueueDirection, erhält "upload".'],
    ["„preparationDigest“ erwartet ContentSyncPreparationDigest, erhält ContentSyncPreparationDigestWire."],
  ]);
});

test("Rechte und Eigentümer prüfen Profilbindung, Abbruch und optionale Admin-Freigabe", () => {
  const fixture = JSON.parse(readFileSync(new URL("./file-property-wire.json", import.meta.url)));
  const { permissions, ownership } = fixture;
  const permission = { request: "PermissionEditRequest" };
  const owner = { request: "OwnerEditRequest" };
  const cases = [
    [{ request: permissions.request }, permission],
    [{ request: ownership.request }, owner],
    [{ request: { ...permissions.request, provider: "sftp", profileId: "profile", profileRevision: 7, administrator: false } }, permission],
    [{ enabled: true }, { enabled: "bool" }],
    [{ requestId: "bound" }, { requestId: "String" }],
    [{ request: { ...permissions.request, administrator: null, profileRevision: "7", mode: 416 } }, permission],
    [{ request: { ...ownership.request, profileId: "profile", gid: 0 } }, owner],
    [{ requestId: null }, { requestId: "String" }],
  ];
  const { checker, payloads } = argumentPayloads(cases
    .map(([payload]) => `invoke(${JSON.stringify(payload)});`).join("\n"));
  const results = cases.map(([, signature], i) => checkIpcArguments(checker, payloads[i], signature));
  assert.deepEqual(results.flatMap(({ open }) => [...open]), []);
  assert.deepEqual(results.slice(0, 5).flatMap(({ issues }) => [...issues]), []);
  assert.deepEqual(results.slice(5).flatMap(({ issues }) => [...issues]), [
    "„request.mode“ erwartet String, erhält 416.",
    '„request.profileRevision“ erwartet Option<u64>, erhält "7".',
    "„request.administrator“ erwartet Default<bool>, erhält null.",
    "Unbekanntes Argument „request.profileId“.",
    "Unbekanntes Argument „request.gid“.",
    "„requestId“ erwartet String, erhält null.",
  ]);
});

test("Sync-Aufträge prüfen Richtungen, Revisionen und gespeicherte Endpunkte", () => {
  const {syncJob} = JSON.parse(readFileSync(new URL("./small-control-wire.json", import.meta.url)));
  const {checker, payloads} = argumentPayloads(`
    invoke({ request: ${JSON.stringify(syncJob.edited)} });
    invoke({ request: ${JSON.stringify({
      ...syncJob.edited,
      direction: "both",
      revision: "2",
      left: {provider: "webdav", path: "/", profile_id: "old-spelling", profileRevision: 42},
    })} });
  `);
  const [valid, invalid] = payloads.map(payload =>
    checkIpcArguments(checker, payload, {request: "SaveSyncJobRequest"}));
  assert.deepEqual([...valid.issues, ...valid.open, ...invalid.open], []);
  assert.deepEqual([...invalid.issues], [
    '„request.revision“ erwartet Option<u64>, erhält "2".',
    "Unbekanntes Argument „request.left.profile_id“.",
    "Unbekanntes Argument „request.left.profileRevision“.",
    '„request.direction“ erwartet SyncJobDirection, erhält "both".',
  ]);
});
