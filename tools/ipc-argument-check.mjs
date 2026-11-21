import { readFileSync } from "node:fs";
import ts from "typescript";

const unknownType = type => Boolean(type.flags & (ts.TypeFlags.Any | ts.TypeFlags.Unknown));
const members = type => type.isUnion() ? type.types : [type];
const desktopPreferenceContract = JSON.parse(readFileSync(new URL("./tests/desktop-preferences-wire.json", import.meta.url)));
const argumentEnums = new Map([
  ...Object.entries(desktopPreferenceContract.enums),
  ["FavoriteProvider", JSON.parse(readFileSync(new URL("./tests/favorite-wire.json", import.meta.url))).providers],
]);

argumentEnums.set("SftpAuthenticationKind", ["password", "private_key", "ssh_agent"]);
argumentEnums.set("FtpTransportSecurity", ["plaintext", "explicit_tls"]);
argumentEnums.set("FtpsCertificatePolicy", ["web_pki", "web_pki_and_pin", "pinned_certificate", "unverified_certificate"]);

argumentEnums.set("FinderTagEditKind", ["upsert", "remove", "clear", "clear_legacy"]);
argumentEnums.set("FileFlagKind", ["locked", "hiddenExtension"]);

const remoteProviders = ["sftp", "ftps", "webdav", "smb"];
argumentEnums.set("RemoteOpenMode", ["passive", "original", "edit"]);
argumentEnums.set("SnapshotVerification", ["streaming", "full_readback"]);
const endpointFields = { provider: "EndpointProvider", root: "String" };
argumentEnums.set("EndpointProvider", ["local", ...remoteProviders]);
argumentEnums.set("ArchivePackCompression", ["stored", "deflate"]);
argumentEnums.set("BatchRenameNumberPosition", ["before", "after"]);
argumentEnums.set("RemoteDeleteMode", ["quarantine", "permanent"]);
argumentEnums.set("RenameExecutionClass", ["assured", "best_effort"]);
argumentEnums.set("SyncMode", ["AddOnly", "Update", "Mirror"]);
argumentEnums.set("SyncCompare", ["SizeAndTime", "ContentHash"]);
argumentEnums.set("MetadataProfile", ["Portable", "NativeStrict", "NativeBestEffort"]);
argumentEnums.set("doppeldecker_core::DirectoryCreateExecutionClass", ["assured", "best_effort"]);

argumentEnums.set("SftpQueueDirection", ["Upload", "Download"]);
argumentEnums.set("doppeldecker_core::DirectoryCreateExecutionClass", ["assured", "best_effort"]);
argumentEnums.set("SyncJobDirection", ["lr", "rl"]);

const backendWireTypes = new Map([
  ["ContentSyncPreparationDigest", { name: "ContentSyncPreparationDigestWire", declaration: "/frontend/content-sync-wire.d.ts" }],
  ["doppeldecker_core::QueueAttention", { name: "QueueAttentionWire", declaration: "/frontend/queue-snapshot-wire.d.ts" }],
  ["SyncPlan", { name: "SyncPlanWire", declaration: "/frontend/local-sync-wire.d.ts" }],
  ["LocalSyncPreparationDigest", { name: "LocalSyncPreparationDigestWire", declaration: "/frontend/local-sync-wire.d.ts" }],
  ["RemoteDeletePlan", { name: "RemoteDeletePlanWire", declaration: "/frontend/routine-file-wire.d.ts" }],
]);
argumentEnums.set("BidirectionalRecoveryKind", ["resume", "rollback"]);
argumentEnums.set("ConflictDecisionTag", ["keep_left", "keep_right", "keep_both", "skip"]);
argumentEnums.set("EndpointSide", ["left", "right"]);
const prepareBidirectionalFields = {
  pairId: "PairId",
  pairName: "String",
  left: "BidirectionalEndpointRequest",
  right: "BidirectionalEndpointRequest",
  resolutions: "Vec<ConflictResolution>",
  expectedUnresolvedDigest: "Option<[u8;32]>",
};
const providerDeleteRequestFields = { endpoint: "BidirectionalEndpointRequest", items: "Vec<String>" };
argumentEnums.set("FileTransferKind", ["copy", "move"]);
argumentEnums.set("ProviderFilePolicy", ["skip", "overwrite", "both", "inspect"]);
const directorySizeFields = {
  side: "String",
  endpoint: "BidirectionalEndpointRequest",
  names: "Vec<String>",
  sessionGeneration: "Option<u64>",
  recalculate: "bool",
  clientToken: "u64",
};
const passwordProfileFields = {
  username: "String",
  requested_root: "String",
  password_reference: "KeychainSecretReference",
};
const propertyReadFields = { provider: "String", root: "String", names: "Vec<String>" };
const propertyEditFields = { ...propertyReadFields, requestId: "String" };
const archiveTargetFields = {
  requestId: "String",
  targetProvider: "String",
  targetDir: "String",
  target: "Option<BidirectionalEndpointRequest>",
};
const fileIntentFields = {
  kind: "FileTransferKind", source: "BidirectionalEndpointRequest",
  target: "BidirectionalEndpointRequest", items: "Vec<String>", destination: "String",
};
const providerFileFields = { ...fileIntentFields, generatedOperationId: "Option<String>" };
const syncEndpointPair = { source: "BidirectionalEndpointRequest", target: "BidirectionalEndpointRequest", options: "SyncOptions" };
const knownObjects = new Map([
  ["PrepareBidirectionalRequest", () => prepareBidirectionalFields],
  ["ForwardPrepareRequest", () => ({ ...prepareBidirectionalFields, recovery: "Default<bool>" })],
  ["ConflictResolution", () => ({ path: "Vec<Vec<u8>>", decision: "ConflictDecision" })],
  ["ConflictDecision", properties => ({
    decision: "ConflictDecisionTag",
    ...(properties.get("decision")?.type.isStringLiteral()
      && properties.get("decision").type.value === "keep_both"
      ? { primary: "EndpointSide", conflict_path: "Vec<Vec<u8>>" } : {}),
  })],
  ["PrepareContentSyncRequest", () => syncEndpointPair],
  ["PrepareProviderSyncRequest", () => syncEndpointPair],
  ["RemoteSyncRequest", () => ({
    profileId: "String", profileRevision: "Option<u64>", direction: "SftpQueueDirection",
    localRoot: "String", remoteRoot: "String", options: "SyncOptions",
  })],
  ["PaneStartLocation", () => ({
    provider: "EndpointProvider", path: "String", profileId: "Option<String>",
    profileRevision: "Option<u64>", profileName: "Option<String>",
  })],
  ["WindowStartState", () => ({ left: "PaneStartLocation", right: "PaneStartLocation" })],
  ["crate::pane_tabs::WindowTabs", () => ({ left: "PaneTabs", right: "PaneTabs" })],
  ["PaneTabs", () => ({ activeId: "String", tabs: "Vec<SavedTab>" })],
  ["SavedTab", () => ({
    id: "String", location: "PaneStartLocation", history: "Vec<String>", historyIndex: "i64",
    selectedNames: "Vec<String>", cursorName: "Option<String>", anchorName: "Option<String>",
    scrollTop: "f64", sort: "TabSort", viewOptions: "TabViewOptions",
  })],
  ["TabSort", () => ({ key: "String", dir: "i8" })],
  ["TabViewOptions", () => ({
    columns: "Default<TabColumns>", foldersFirst: "Default<bool>", foldersAlphabetical: "Default<bool>",
    boldFolders: "Default<bool>", zebraRows: "Default<bool>", relativeDates: "Default<bool>",
    showHidden: "Default<bool>", density: "Default<String>", textSize: "Default<String>",
  })],
  ["TabColumns", () => Object.fromEntries(
    ["size", "kind", "modified", "created", "owner", "group", "permissions"].map(key => [key, "Default<bool>"]),
  )],
  ["PermissionEditRequest", () => ({
    ...propertyEditFields, mode: "String", profileId: "Option<String>", profileRevision: "Option<u64>",
    recursive: "Default<bool>", administrator: "Default<bool>",
  })],
  ["OwnerEditRequest", () => ({
    ...propertyEditFields, owner: "String", group: "String", administrator: "Default<bool>",
  })],
  ["SaveSyncJobRequest", () => ({
    id: "Option<String>", revision: "Option<u64>", name: "String",
    left: "SyncJobEndpoint", right: "SyncJobEndpoint", direction: "SyncJobDirection", options: "SyncOptions",
  })],
  ["SyncJobEndpoint", () => ({ provider: "FavoriteProvider", profileId: "Option<String>", path: "String" })],
  ["OpenRemoteSnapshotRequest", () => ({
    endpoint: "BidirectionalEndpointRequest", path: "String",
    mode: "Default<RemoteOpenMode>", verification: "Default<SnapshotVerification>",
  })],
  ["DiffFileRequest", () => ({ endpoint: "BidirectionalEndpointRequest", path: "String", name: "String" })],
  ["CompareFilesRequest", () => ({ left: "DiffFileRequest", right: "DiffFileRequest" })],
  ["RemoteEditRequest", () => ({ id: "String" })],
  ["PaneStartLocation", () => ({
    provider: "EndpointProvider", path: "String", profileId: "Option<String>",
    profileRevision: "Option<u64>", profileName: "Option<String>",
  })],
  ["WindowStartState", () => ({ left: "PaneStartLocation", right: "PaneStartLocation" })],
  ["crate::pane_tabs::WindowTabs", () => ({ left: "PaneTabs", right: "PaneTabs" })],
  ["PaneTabs", () => ({ activeId: "String", tabs: "Vec<SavedTab>" })],
  ["SavedTab", () => ({
    id: "String", location: "PaneStartLocation", history: "Vec<String>", historyIndex: "i64",
    selectedNames: "Vec<String>", cursorName: "Option<String>", anchorName: "Option<String>",
    scrollTop: "f64", sort: "TabSort", viewOptions: "TabViewOptions",
  })],
  ["TabSort", () => ({ key: "String", dir: "i8" })],
  ["TabViewOptions", () => ({
    columns: "Default<TabColumns>", foldersFirst: "Default<bool>", foldersAlphabetical: "Default<bool>",
    boldFolders: "Default<bool>", zebraRows: "Default<bool>", relativeDates: "Default<bool>",
    showHidden: "Default<bool>", density: "Default<String>", textSize: "Default<String>",
  })],
  ["TabColumns", () => Object.fromEntries(
    ["size", "kind", "modified", "created", "owner", "group", "permissions"].map(key => [key, "Default<bool>"]),
  )],
  ["FileIntent", () => fileIntentFields],
  ["ProviderFileRequest", () => providerFileFields],
  ["ProviderFileChoiceRequest", () => ({ id: "String", policy: "ProviderFilePolicy" })],
  ["EnqueueProviderFileRequest", () => ({
    ...providerFileFields, overwrite: "bool", expectedConflicts: "Vec<String>",
    keepBothPreparationId: "Option<String>", conflictPreparationId: "Option<String>",
    copyNewBestEffortExpected: "Option<bool>", replaceBestEffortExpected: "Option<bool>",
    removeSourceBestEffortExpected: "Option<bool>",
  })],
  ["OpenRemoteSnapshotRequest", () => ({
    endpoint: "BidirectionalEndpointRequest", path: "String",
    mode: "Default<RemoteOpenMode>", verification: "Default<SnapshotVerification>",
  })],
  ["DiffFileRequest", () => ({ endpoint: "BidirectionalEndpointRequest", path: "String", name: "String" })],
  ["CompareFilesRequest", () => ({ left: "DiffFileRequest", right: "DiffFileRequest" })],
  ["RemoteEditRequest", () => ({ id: "String" })],
  ["SyncOptions", () => ({
    mode: "SyncMode", compare: "SyncCompare", metadata: "Default<MetadataProfile>",
    mtime_tolerance: "i64", exclude: "Default<Vec<String>>",
  })],
  ["KeychainSecretReference", () => ({ service: "String", account: "String" })],
  ["FtpsCertificatePin", () => ({ sha256: "[u8;32]" })],
  ["SftpHostKeyPin", () => ({ algorithm: "String", sha256: "[u8;32]" })],
  ["SftpAuthenticationReference", properties => {
    const tag = properties.get("type")?.type;
    const type = "SftpAuthenticationKind";
    if (tag?.isStringLiteral() && tag.value === "password") {
      return { type, secret: "KeychainSecretReference" };
    }
    if (tag?.isStringLiteral() && tag.value === "private_key") {
      return { type, path: "String", passphrase: "Option<KeychainSecretReference>", public_key_sha256: "[u8;32]" };
    }
    return { type, public_key_sha256: "[u8;32]" };
  }],
  ...["Sftp", "Ftps", "WebDav", "Smb"].map(provider => [
    `${provider}Profile`,
    () => ({ id: "String", name: "String", connection: `${provider}ConnectionConfig` }),
  ]),
  ["SftpConnectionConfig", () => ({
    host: "String", port: "u16", alternate_hosts: "Default<Vec<String>>",
    username: "String", requested_root: "String", host_key: "SftpHostKeyPin",
    authentication: "SftpAuthenticationReference",
  })],
  ["FtpsConnectionConfig", () => ({
    ...passwordProfileFields, host: "String", port: "u16", alternate_hosts: "Default<Vec<String>>",
    transport_security: "Default<FtpTransportSecurity>", certificate: "Option<FtpsCertificatePin>",
    certificate_policy: "Option<FtpsCertificatePolicy>",
  })],
  ["WebDavConnectionConfig", () => ({
    ...passwordProfileFields, origin: "String", certificate: "FtpsCertificatePin",
    certificate_policy: "FtpsCertificatePolicy",
  })],
  ["SmbConnectionConfig", () => ({
    ...passwordProfileFields, host: "String", port: "u16",
    server_guid: "[u8;16]", share: "String", domain: "String",
  })],
  ["FinderTagReadRequest", () => propertyReadFields],
  ["FinderTagEditRequest", () => ({ ...propertyEditFields, edit: "FinderTagEdit" })],
  ["FinderTagEdit", properties => {
    const tag = properties.get("type")?.type;
    const type = "FinderTagEditKind";
    if (tag?.isStringLiteral() && tag.value === "upsert") return { type, tag: "FinderTag", previousName: "Option<String>" };
    if (tag?.isStringLiteral() && tag.value === "remove") return { type, name: "String" };
    return { type };
  }],
  ["FinderTag", () => ({ name: "String", color: "u8" })],
  ["FinderCommentReadRequest", () => ({ ...propertyReadFields, requestPermission: "Default<bool>" })],
  ["FinderCommentEditRequest", () => ({ ...propertyEditFields, text: "String" })],
  ["FileFlagReadRequest", () => ({ ...propertyReadFields, kind: "FileFlagKind" })],
  ["FileFlagEditRequest", () => ({ ...propertyEditFields, kind: "FileFlagKind", enabled: "bool" })],
  ["VisibilityReadRequest", () => propertyReadFields],
  ["VisibilityEditRequest", () => ({ ...propertyEditFields, hidden: "bool" })],
  ["ProviderDeleteRequest", () => providerDeleteRequestFields],
  ["EnqueueProviderDeleteRequest", () => ({
    ...providerDeleteRequestFields,
    mode: "RemoteDeleteMode", expectedPlan: "RemoteDeletePlan",
    confirmation: "Option<String>", effectsConfirmed: "Default<bool>",
  })],
  ["RenameProviderPathRequest", () => ({
    endpoint: "BidirectionalEndpointRequest", path: "String", newName: "String",
    expectedExecutionClass: "Option<RenameExecutionClass>",
  })],
  ["PreferencePatch", () => ({
    icon: "Option<IconChoice>",
    theme: "Option<Theme>",
    byteUnits: "Option<ByteUnits>",
    localFolderSizes: "Option<bool>",
    networkDiscovery: "Option<bool>",
    restoreSession: "Option<bool>",
    diffTool: "Option<DiffTool>",
  })],
  ["ArchivePackPrepareRequest", () => ({
    requestId: "String", provider: "String", root: "String", items: "Vec<String>",
    targetName: "String", options: "ArchivePackOptions", target: "Option<BidirectionalEndpointRequest>",
  })],
  ["ArchivePackOptions", () => ({ compression: "ArchivePackCompression" })],
  ["ArchiveExtractPrepareRequest", () => ({
    ...archiveTargetFields, provider: "String", root: "String", sourcePath: "String",
  })],
  ["ArchiveExtractSelectionPrepareRequest", () => ({
    ...archiveTargetFields, sessionId: "String", entryId: "Option<String>", directoryId: "Option<String>",
  })],
  ["ArchiveOpenRequest", () => ({ endpoint: "BidirectionalEndpointRequest", path: "String" })],
  ["PrepareBatchRenameRequest", () => ({
    requestId: "Option<String>", endpoint: "BidirectionalEndpointRequest", names: "Vec<String>", options: "BatchRenameOptions",
  })],
  ["BatchRenameOptions", () => ({
    find: "String", replacement: "String", prefix: "String", suffix: "String",
    preserveExtension: "Default<bool>", trimOuterSpaces: "Default<bool>", numbering: "Option<BatchRenameNumbering>",
  })],
  ["BatchRenameNumbering", () => ({ start: "u32", step: "u32", minDigits: "u8", position: "BatchRenameNumberPosition" })],
  ["FilePreviewRequest", () => ({ endpoint: "BidirectionalEndpointRequest", path: "String" })],
  ["LocalFileTransferRequest", () => ({
    items: "Vec<String>",
    destDir: "String",
    overwrite: "bool",
    expectedConflicts: "Vec<String>",
    keepBothPreparationId: "Option<String>",
    conflictPreparationId: "Option<String>",
    replaceBestEffortExpected: "Option<bool>",
  })],
  ["BidirectionalEndpointRequest", (properties, path, open) => {
    const provider = properties.get("provider")?.type;
    const tags = provider ? members(provider) : [];
    const includesLocal = tags.some(tag => tag.isStringLiteral() && tag.value === "local");
    const isRemote = tag => tag.isStringLiteral() && remoteProviders.includes(tag.value);
    if (includesLocal && tags.length === 1) return endpointFields;
    if (includesLocal && tags.some(isRemote)) {
      open.add(`${path}: Profil-ID nicht eindeutig an die Endpunkt-Variante gebunden`);
    }
    return {
      ...endpointFields,
      profileId: tags.length > 0 && tags.every(isRemote) ? "String" : "Option<String>",
      profileRevision: "Option<u64>",
    };
  }],
  ["DirectorySizeRequest", () => directorySizeFields],
  ["DuplicateRequest", () => ({ provider: "String", root: "String", items: "Vec<String>" })],
  ["NameSearchRequest", () => ({
    side: "String", endpoint: "BidirectionalEndpointRequest", sessionGeneration: "Option<u64>",
    options: "NameSearchOptions", clientToken: "u64",
  })],
  ["NameSearchOptions", () => ({ query: "String", caseSensitive: "Default<bool>", includeHidden: "Default<bool>" })],
]);

function argumentType(rustType) {
  // Default<T> bezeichnet ein Serde-Standardfeld, dessen Wert kein null akzeptiert.
  const optionalType = /^(Option|Default)<(.+)>$/.exec(rustType);
  const optional = optionalType !== null;
  const nullable = optionalType?.[1] === "Option";
  const named = optionalType?.[2] ?? rustType;
  const inner = named === "PairId" ? "[u8;16]" : named
    .replace(/^crate::bidirectional_commands::(?=BidirectionalEndpointRequest$)/, "")
    .replace(/^crate::sftp_commands::(?=RemoteSyncRequest$)/, "");
  let kind = "opaque";
  if (inner === "String") kind = "string";
  else if (inner === "bool") kind = "boolean";
  else if (inner === "f32" || inner === "f64") kind = "float";
  else if (/^[ui](?:8|16|32|64|128|size)$/.test(inner)) kind = "integer";
  else if (inner.startsWith("Vec<") || inner.startsWith("[")) kind = "array";
  else if (argumentEnums.has(inner)) kind = "enum";
  else if (knownObjects.has(inner)) kind = "object";
  else if (backendWireTypes.has(inner)) kind = "backendWire";
  return { optional, nullable, kind, inner };
}

function integerLiteralFits(value, rustType) {
  if (!Number.isInteger(value)) return false;
  const signed = rustType.startsWith("i");
  const bits = Number(rustType.slice(1));
  if (!signed && value < 0) return false;
  if (!Number.isFinite(bits)) return true;
  const limit = 2 ** (bits - Number(signed));
  return value >= (signed ? -limit : 0) && value < limit;
}

function isObject(checker, type) {
  return Boolean(type.flags & (ts.TypeFlags.Object | ts.TypeFlags.Intersection))
    && !checker.isArrayType(type) && !checker.isTupleType(type);
}

function matchesType(checker, type, { kind, optional, nullable, inner }) {
  if (type.flags & ts.TypeFlags.Null) return nullable;
  if (type.flags & ts.TypeFlags.Undefined) return optional;
  switch (kind) {
    case "string": return Boolean(type.flags & ts.TypeFlags.StringLike);
    case "boolean": return Boolean(type.flags & ts.TypeFlags.BooleanLike);
    case "float": return Boolean(type.flags & ts.TypeFlags.NumberLike);
    case "integer": return type.isNumberLiteral()
      ? integerLiteralFits(type.value, inner) : Boolean(type.flags & ts.TypeFlags.NumberLike);
    case "array": return checker.isArrayType(type) || checker.isTupleType(type);
    case "enum": return type.isStringLiteral() && argumentEnums.get(inner).includes(type.value);
    case "object": return isObject(checker, type);
    case "backendWire": {
      // Das Frontend reicht den Beleg aus seiner typisierten IPC-Antwort durch.
      const { name, declaration } = backendWireTypes.get(inner);
      const symbol = type.getSymbol();
      return symbol?.getName() === name && symbol.getDeclarations()?.some(node =>
        node.getSourceFile().fileName.endsWith(declaration));
    }
    default: return true;
  }
}

function verifyType(checker, type, rustType, key, issues, open, expression = null) {
  const expected = argumentType(rustType);
  for (const member of members(type)) {
    if (unknownType(member)) open.add(`${key}: ${checker.typeToString(member)}`);
    else if (expected.inner === "EndpointProvider" && member.flags & ts.TypeFlags.String) {
      open.add(`${key}: Endpunkt-Variante nicht statisch belegt`);
    } else if (!matchesType(checker, member, expected)) {
      issues.add(`„${key}“ erwartet ${rustType}, erhält ${checker.typeToString(member)}.`);
    } else if (!(member.flags & (ts.TypeFlags.Null | ts.TypeFlags.Undefined))) {
      if (expected.kind === "opaque") open.add(`${key}: Serde-Typ ${expected.inner}`);
      if (expected.kind === "object") {
        const properties = propertiesOf(checker, member, unwrap(expression));
        const fields = knownObjects.get(expected.inner)(properties, key, open);
        verifyFields(checker, member, properties, fields, key, issues, open);
      }
      if (expected.kind !== "array") continue;
      const vector = /^Vec<(.+)>$/.exec(expected.inner);
      const fixed = /^\[(.+);(\d+)\]$/.exec(expected.inner);
      const elementType = vector?.[1] ?? fixed?.[1];
      if (!elementType) continue;

      const literal = expression && ts.isArrayLiteralExpression(expression)
        && !expression.elements.some(ts.isSpreadElement);
      if (literal) {
        if (fixed && expression.elements.length !== Number(fixed[2])) {
          issues.add(`„${key}“ erwartet genau ${fixed[2]} Array-Einträge.`);
        }
        for (const item of expression.elements) {
          verifyType(checker, checker.getTypeAtLocation(item), elementType, `${key}[]`, issues, open, item);
        }
      } else {
        const elements = checker.getIndexTypeOfType(member, ts.IndexKind.Number);
        if (elements && !(elements.flags & ts.TypeFlags.Never)) {
          verifyType(checker, elements, elementType, `${key}[]`, issues, open);
        }
        if (fixed) {
          const length = checker.getPropertyOfType(member, "length");
          const lengthType = length && checker.getTypeOfSymbol(length);
          if (!lengthType?.isNumberLiteral()) open.add(`${key}: Array-Länge nicht statisch belegt`);
          else if (lengthType.value !== Number(fixed[2])) issues.add(`„${key}“ erwartet genau ${fixed[2]} Array-Einträge.`);
        }
      }
    }
  }
}

function unwrap(node) {
  while (node && ts.isParenthesizedExpression(node)) node = node.expression;
  return node;
}

function explicitProperties(node, checker) {
  const properties = new Map();
  for (const property of node.properties) {
    if (ts.isSpreadAssignment(property)) continue;
    const name = property.name;
    if (!name || !(ts.isIdentifier(name) || ts.isStringLiteralLike(name))) continue;
    const value = ts.isPropertyAssignment(property) ? property.initializer : name;
    properties.set(name.text, { type: checker.getTypeAtLocation(value), optional: false, expression: value });
  }
  return properties;
}

function propertiesOf(checker, type, node) {
  const properties = new Map();
  if (type && !unknownType(type) && !(type.flags & ts.TypeFlags.Undefined)) {
    for (const symbol of checker.getPropertiesOfType(type)) {
      properties.set(symbol.getName(), {
        type: node ? checker.getTypeOfSymbolAtLocation(symbol, node) : checker.getTypeOfSymbol(symbol),
        optional: Boolean(symbol.flags & ts.SymbolFlags.Optional),
      });
    }
  }
  if (node && ts.isObjectLiteralExpression(node)
    && ((type && unknownType(type)) || !node.properties.some(ts.isSpreadAssignment))) {
    for (const [name, property] of explicitProperties(node, checker)) properties.set(name, property);
  }
  return properties;
}

function verifyFields(checker, variant, properties, signature, parent, issues, open) {
  const unknown = variant && unknownType(variant);
  const indexed = variant && checker.getIndexInfosOfType(variant).length > 0;
  if (unknown || indexed) open.add(`${parent || "Argumentobjekt"}: unbekannte Felder`);
  const fieldPath = field => parent ? `${parent}.${field}` : field;
  for (const [key, rustType] of Object.entries(signature)) {
    const expected = argumentType(rustType);
    const value = properties.get(key);
    const path = fieldPath(key);
    if (!value) {
      if (!expected.optional && !unknown && !indexed) issues.add(`Pflichtfeld „${path}“ fehlt.`);
      continue;
    }
    if (value.optional && !expected.optional) issues.add(`Pflichtfeld „${path}“ ist optional.`);
    verifyType(checker, value.type, rustType, path, issues, open, value.expression);
  }
  for (const key of properties.keys()) {
    if (!Object.hasOwn(signature, key)) issues.add(`Unbekanntes Argument „${fieldPath(key)}“.`);
  }
}

/** Prüft IPC-Argumente und die einzeln hinterlegten Serde-Verträge. */
export function checkIpcArguments(checker, payload, signature) {
  const issues = new Set();
  const open = new Set();
  const node = unwrap(payload);
  const type = node && checker.getTypeAtLocation(node);
  for (const variant of type ? members(type) : [null]) {
    const absent = !variant || Boolean(variant.flags & ts.TypeFlags.Undefined);
    if (!absent && !unknownType(variant) && !isObject(checker, variant)) {
      issues.add("Argumente müssen ein Objekt sein.");
      continue;
    }
    const properties = propertiesOf(checker, variant, node);
    verifyFields(checker, variant, properties, signature, "", issues, open);
  }
  return { issues, open };
}
