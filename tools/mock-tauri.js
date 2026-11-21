
(() => {
  window.__DOPPELDECKER_MOCK_ERRORS__ = [];
  document.documentElement.dataset.mockErrors = "[]";
  const recordError = value => {
    window.__DOPPELDECKER_MOCK_ERRORS__.push(String(value));
    document.documentElement.dataset.mockErrors = JSON.stringify(window.__DOPPELDECKER_MOCK_ERRORS__);
  };
  window.addEventListener("error", event => recordError(event.error?.stack || event.error || event.message));
  window.addEventListener("unhandledrejection", event => recordError(event.reason));
  window.addEventListener("securitypolicyviolation", event => {
    recordError(`CSP-Verstoß: ${event.violatedDirective} blockierte ${event.blockedURI || "Inline-Inhalt"}`);
  });
  const mockParams = new URLSearchParams(location.search);
  // Wettlauf-Gegenfälle halten die Antwort bis zur beobachteten Navigation
  // oder zum sichtbaren Editor. Millisekundenfristen würden unter
  // Last gerade die zu prüfende Reihenfolge wieder dem Scheduler überlassen.
  const heldRequests = new Map();
  const requestPhases = {};
  async function holdRequest(name, binding) {
    if (!mockParams.has(name)) return;
    requestPhases[name] = { phase: "pending", ...binding };
    await new Promise(resolve => heldRequests.set(name, resolve));
    requestPhases[name].phase = "returned";
  }
  window.__MOCK_REQUEST_PHASES__ = () => structuredClone(requestPhases);
  window.__MOCK_RELEASE_REQUEST__ = name => {
    const release = heldRequests.get(name);
    if (!release) throw new Error(`Keine gehaltene Anfrage: ${name}`);
    heldRequests.delete(name);
    release();
  };
  const requestedEntries = Number.parseInt(mockParams.get("entries") || "0", 10);
  const largeEntryCount = Number.isFinite(requestedEntries)
    ? Math.max(0, Math.min(requestedEntries, 10000))
    : 0;
  const requestedScanDelay = Number.parseInt(mockParams.get("scanDelay") || "0", 10);
  const scanDelayMs = Number.isFinite(requestedScanDelay)
    ? Math.max(0, Math.min(requestedScanDelay, 2000))
    : 0;
  const scanErrorName = mockParams.get("scanError") || "";
  const scanErrorOnSecond = mockParams.get("scanErrorOnSecond") || "";
  const scanCalls = [];
  const trashedPaths = new Set();
  // Lässt jedes Remote-Listing scheitern — für den Regressionsfall, dass ein
  // fehlgeschlagener Endpunktwechsel keine alte Dateiliste stehen lassen darf.
  const remoteScanError = mockParams.has("remoteScanError");
  const requestedSftpScanDelay = Number.parseInt(mockParams.get("sftpScanDelay") || "0", 10);
  const sftpScanDelayMs = Number.isFinite(requestedSftpScanDelay)
    ? Math.max(0, Math.min(requestedSftpScanDelay, 1000))
    : 0;
  const requestedSftpProfileListDelay = Number.parseInt(
    mockParams.get("sftpProfileListDelay") || "0",
    10,
  );
  const sftpProfileListDelayMs = Number.isFinite(requestedSftpProfileListDelay)
    ? Math.max(0, Math.min(requestedSftpProfileListDelay, 1000))
    : 0;
  const requestedTrashDelay = Number.parseInt(mockParams.get("trashDelay") || "0", 10);
  const trashDelayMs = Number.isFinite(requestedTrashDelay)
    ? Math.max(0, Math.min(requestedTrashDelay, 1000))
    : 0;
  const requestedRenameDelay = Number.parseInt(mockParams.get("renameDelay") || "0", 10);
  const renameDelayMs = Number.isFinite(requestedRenameDelay)
    ? Math.max(0, Math.min(requestedRenameDelay, 1000))
    : 0;
  const requestedMkdirDelay = Number.parseInt(mockParams.get("mkdirDelay") || "0", 10);
  const mkdirDelayMs = Number.isFinite(requestedMkdirDelay)
    ? Math.max(0, Math.min(requestedMkdirDelay, 1000))
    : 0;
  const requestedQuarantineMutationDelay = Number.parseInt(
    mockParams.get("quarantineMutationDelay") || "0",
    10,
  );
  const quarantineMutationDelayMs = Number.isFinite(requestedQuarantineMutationDelay)
    ? Math.max(0, Math.min(requestedQuarantineMutationDelay, 1000))
    : 0;
  const requestedSizeDelay = Number.parseInt(mockParams.get("sizeDelay") || "0", 10);
  const directorySizeDelayMs = Number.isFinite(requestedSizeDelay)
    ? Math.max(0, Math.min(requestedSizeDelay, 1000))
    : 0;
  const remoteMissingMode = mockParams.has("remoteMissingMode");
  const cancelSizeDelayMs = Number.parseInt(mockParams.get("cancelSizeDelay") || "0", 10);
  const conflictMode = mockParams.get("conflicts") || "none";
  const cliOffer = mockParams.has("cliOffer");
  const cliDeclineError = mockParams.has("cliDeclineError");
  const cliStatusError = mockParams.has("cliStatusError");
  const requestedDefaultDelay = Number.parseInt(mockParams.get("defaultDelay") || "0", 10);
  const defaultDelayMs = Number.isFinite(requestedDefaultDelay)
    ? Math.max(0, Math.min(requestedDefaultDelay, 1000))
    : 0;
  const defaultErrorAfterStartup = mockParams.has("defaultErrorAfterStartup");
  const emptyWindowStart = mockParams.has("emptyWindowStart");
  const bestEffortSource = mockParams.has("bestEffortSource");
  const requestedFileActionPreviewDelay = Number.parseInt(
    mockParams.get("fileActionPreviewDelay") || "0",
    10,
  );
  const fileActionPreviewDelayMs = Number.isFinite(requestedFileActionPreviewDelay)
    ? Math.max(0, Math.min(requestedFileActionPreviewDelay, 1000))
    : 0;
  const updateMode = mockParams.get("update") || "none";
  const requestedInstallDelay = Number.parseInt(mockParams.get("installDelay") || "0", 10);
  const installDelayMs = Number.isFinite(requestedInstallDelay)
    ? Math.max(0, Math.min(requestedInstallDelay, 1000))
    : 0;
  const foreignRunningDuringInstall = mockParams.has("foreignRunningDuringInstall");
  const installFailsOnce = mockParams.has("installFailsOnce");
  const installDispatchFailsOnce = mockParams.has("installDispatchFailsOnce");
  const providerEnqueueFailsOnce = mockParams.has("providerEnqueueFailsOnce");
  const providerRunFailsOnce = mockParams.has("providerRunFailsOnce");
  const bidirectionalEnqueueFailsOnce = mockParams.has("bidirectionalEnqueueFailsOnce");
  const bidirectionalRunFailsOnce = mockParams.has("bidirectionalRunFailsOnce");
  const bidirectionalRollbackFailsOnce = mockParams.has("bidirectionalRollbackFailsOnce");
  const bidirectionalConflict = mockParams.has("bidirectionalConflict");
  const requestedBidirectionalRunDelay = Number.parseInt(
    mockParams.get("bidirectionalRunDelay") || "0",
    10,
  );
  const bidirectionalRunDelayMs = Number.isFinite(requestedBidirectionalRunDelay)
    ? Math.max(0, Math.min(requestedBidirectionalRunDelay, 1000))
    : 0;
  const bidirectionalDirectJournalFailure = mockParams.has("bidirectionalDirectJournalFailure");
  const bidirectionalTerminalCommitFailure = mockParams.has("bidirectionalTerminalCommitFailure");
  const bidirectionalFastDoneBeforeResponse = mockParams.has("bidirectionalFastDoneBeforeResponse");
  const bidirectionalFastRollbackBeforeResponse = mockParams.has("bidirectionalFastRollbackBeforeResponse");
  const bidirectionalFastApproveBeforeResponse = mockParams.has("bidirectionalFastApproveBeforeResponse");
  const bidirectionalRecoveryDiscardBeforeDeliveryFailures = Number.parseInt(
    mockParams.get("bidirectionalRecoveryDiscardBeforeDeliveryFailures") || "0",
    10,
  );
  const bidirectionalRecoveryDiscardActsThenFailsOnce = mockParams.has(
    "bidirectionalRecoveryDiscardActsThenFailsOnce",
  );
  const bidirectionalRecoveryStageBlocker = mockParams.has("bidirectionalRecoveryStageBlocker");
  const bidirectionalRecoveryBackupRequired = mockParams.has("bidirectionalRecoveryBackupRequired");
  const quarantineData = mockParams.has("quarantineData");
  const quarantineRecoveryPending = mockParams.has("quarantineRecoveryPending");
  const quarantineInvalidRestoreResponse = mockParams.has("quarantineInvalidRestoreResponse");
  const quarantinePurgeError = mockParams.has("quarantinePurgeError");
  const quarantineListErrorAfterPurge = mockParams.has("quarantineListErrorAfterPurge");
  const quarantineManualRecovery = mockParams.has("quarantineManualRecovery");
  const quarantineStatus = mockParams.get("quarantineStatus") || "complete";
  const quarantineRace = mockParams.has("quarantineRace");
  const quarantineBadgeRace = mockParams.has("quarantineBadgeRace");
  const quarantineOpenDelay = mockParams.has("quarantineOpenDelay");
  const editorDiscoveryDelayMs = Number.parseInt(mockParams.get("editorDiscoveryDelay") || "0", 10);
  const editorAgentDelayMs = Number.parseInt(mockParams.get("editorAgentDelay") || "0", 10);
  const sftpProfileChangesDuringAgent = mockParams.get("sftpProfileChangesDuringAgent") || "";
  const editorSecretDelayMs = Number.parseInt(mockParams.get("editorSecretDelay") || "0", 10);
  const editorSaveDelayMs = Number.parseInt(mockParams.get("editorSaveDelay") || "0", 10);
  const profileSaveError = mockParams.get("profileSaveError") || "";
  const profileSaveBeforeConsumeError = mockParams.get("profileSaveBeforeConsumeError") || "";
  const profileSaveVisibleError = mockParams.get("profileSaveVisibleError") || "";
  const startupQueueError = mockParams.has("startupQueueError")
    ? "Nebenfenster-Journal window-defekt braucht Recovery"
    : null;
  const startupAppError = mockParams.has("startupAppError")
    ? "App-Icon konnte nicht gesetzt werden"
    : null;
  let startupAppErrorDelivered = false;
  const mockWindowLabel = mockParams.get("windowLabel") || "main";
  const requestedComparisonRows = Number.parseInt(mockParams.get("comparisonRows") || "0", 10);
  const comparisonRowCount = Number.isFinite(requestedComparisonRows)
    ? Math.max(0, Math.min(requestedComparisonRows, 10000))
    : 0;

  // Demo-Preset (nur bei ?demo aktiv, damit die Testfälle unberührt bleiben):
  // realistische, öffentlich unbedenkliche Verbindungen, Ordnerinhalte und ein
  // Sync-Plan für die README-Screenshots.
  const demoMode = mockParams.has("demo");
  const demoSource = [
    { name: "Fotos 2026", is_dir: true, is_file: false, is_symlink: false, size: 0, mtime: 1784000000, mode: 493 },
    { name: "Dokumente", is_dir: true, is_file: false, is_symlink: false, size: 0, mtime: 1782200000, mode: 493 },
    { name: "Reisebericht.md", is_dir: false, is_file: true, is_symlink: false, size: 12480, mtime: 1784600000, mode: 420 },
    { name: "Budget 2026.xlsx", is_dir: false, is_file: true, is_symlink: false, size: 48210, mtime: 1784100000, mode: 420 },
    { name: "Präsentation.pdf", is_dir: false, is_file: true, is_symlink: false, size: 2411520, mtime: 1783200000, mode: 420 },
    { name: "archiv.zip", is_dir: false, is_file: true, is_symlink: false, size: 134217728, mtime: 1780400000, mode: 420 },
    { name: "notiz.txt", is_dir: false, is_file: true, is_symlink: false, size: 812, mtime: 1784700000, mode: 420 },
  ];
  const demoTarget = [
    { name: "Fotos 2026", is_dir: true, is_file: false, is_symlink: false, size: 0, mtime: 1784000000, mode: 493 },
    { name: "Dokumente", is_dir: true, is_file: false, is_symlink: false, size: 0, mtime: 1782200000, mode: 493 },
    { name: "Reisebericht.md", is_dir: false, is_file: true, is_symlink: false, size: 11930, mtime: 1781000000, mode: 420 },
    { name: "alte-version.bak", is_dir: false, is_file: true, is_symlink: false, size: 4096, mtime: 1776000000, mode: 420 },
  ];
  const demoFavorites = [
    { name: "Benutzerordner", path: "/Users/demo" },
    { name: "Dokumente", path: "/Users/demo/Documents" },
    { name: "Projekte", path: "/Users/demo/Projekte" },
    { name: "Downloads", path: "/Users/demo/Downloads" },
    { name: "Bilder", path: "/Users/demo/Pictures" },
  ];
  const demoLeftRoot = "/Users/demo/Projekte";
  const demoRightRoot = "/Volumes/Backup/Projekte";
  const wait = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
  const sourceEntries = demoMode
    ? demoSource
    : largeEntryCount > 0
    ? Array.from({ length: largeEntryCount }, (_, index) => ({
        name: `Datei-${String(index + 1).padStart(5, "0")}.txt`, is_dir: false,
        is_file: true, is_symlink: false, size: index + 1, mtime: index + 1, mode: 420,
      }))
    : mockParams.has("unsafe")
      ? [
          { name: "�.txt", is_dir: false, is_file: true, is_symlink: false,
            safely_decoded: false, size: 3, mtime: 1, mode: 420 },
          { name: "�.txt", is_dir: false, is_file: true, is_symlink: false,
            safely_decoded: true, size: 4, mtime: 2, mode: 420 },
        ]
    : mockParams.has("folderSort")
      ? [
          { name: "Groß", is_dir: true, is_file: false, is_symlink: false,
            size: 0, mtime: 1, mode: 493 },
          { name: "Klein", is_dir: true, is_file: false, is_symlink: false,
            size: 0, mtime: 2, mode: 493 },
          { name: "Unvollständig", is_dir: true, is_file: false, is_symlink: false,
            size: 0, mtime: 3, mode: 493 },
        ]
    : mockParams.has("unicodeFilter")
      ? [
          { name: "Cafe\u0301.txt", is_dir: false, is_file: true, is_symlink: false,
            size: 5, mtime: 1, mode: 420 },
          { name: "Überblick.txt", is_dir: false, is_file: true, is_symlink: false,
            size: 7, mtime: 2, mode: 420 },
        ]
    : mockParams.has("mixedSort")
      ? [
          { name: "A-Ordner", is_dir: true, is_file: false, is_symlink: false,
            size: 0, mtime: 3, mode: 493 },
          { name: "B-Ordner", is_dir: true, is_file: false, is_symlink: false,
            size: 0, mtime: 1, mode: 493 },
          { name: "Mitte.txt", is_dir: false, is_file: true, is_symlink: false,
            size: 5, mtime: 2, mode: 420 },
        ]
    : mockParams.has("keyboard")
      ? [
          { name: "Alpha.txt", is_dir: false, is_file: true, is_symlink: false,
            size: 5, mtime: 1, created: 0, owner: "alice", group: "staff",
            extension: "txt", mode: 420 },
          { name: "Beta.txt", is_dir: false, is_file: true, is_symlink: false,
            size: 4, mtime: 2, created: 2, owner: "bob", group: "wheel",
            extension: "md", mode: 384 },
          { name: "Gamma", is_dir: false, is_file: true, is_symlink: false,
            size: 6, mtime: 3, created: null, owner: null, group: null,
            extension: null, mode: null },
          { name: "Bilder", is_dir: true, is_file: false, is_symlink: false,
            size: 0, mtime: 3, created: 0, owner: "alice", group: "staff",
            extension: null, mode: 493 },
          { name: ".verborgen", is_dir: false, is_file: true, is_symlink: false,
            size: 1, mtime: 4, created: 0, owner: "alice", group: "staff",
            extension: null, mode: 384 },
        ]
    : mockParams.has("webdavTree") ? [{ name: "Baum", is_dir:true, is_file:false, is_symlink:false, size:0, mtime:1, mode:448 }]
    : [{ name: "Beispiel.txt", is_dir: false, is_file: true,
        is_symlink: false, size: 8, mtime: 1, mode: 420 }];
  const wireDirectoryEntry = entry => ({
    name: entry.name,
    is_dir: entry.is_dir,
    is_file: entry.is_file,
    is_symlink: entry.is_symlink,
    size: entry.size,
    mtime: entry.mtime ?? null,
    created: entry.created ?? null,
    owner: entry.owner ?? null,
    group: entry.group ?? null,
    extension: entry.extension ?? null,
    mode: entry.mode ?? null,
    device: entry.device ?? null,
    inode: entry.inode ?? null,
    hard_links: entry.hard_links ?? null,
  });
  const listeners = new Map();
  let fileClipboard = { paths: [], cut: false, version: "0" };
  let clipboardWrites = 0;
  let clipboardClears = 0;
  window.__MOCK_CLIPBOARD__ = (value) => {
    if (value) fileClipboard = { ...structuredClone(value), version: String(Number(fileClipboard.version) + 1) };
    return structuredClone({ ...fileClipboard, writes: clipboardWrites, clears: clipboardClears });
  };
  const directorySizeCalls = [];
  const directorySizeCancels = [];
  const activeDirectorySizeRuns = new Map();
  let directorySizeSequence = 0;
  window.__MOCK_DIRECTORY_SIZES__ = () => structuredClone({
    calls: directorySizeCalls,
    cancels: directorySizeCancels,
  });
  const mountEjectCalls = [];
  let systemMounts = structuredClone(mockSmallControlWire.mounts.list);
  window.__MOCK_MOUNTS__ = () => structuredClone({ mounts: systemMounts, ejects: mountEjectCalls });
  if (mockParams.has("mountSpaceError")) {
    systemMounts[2] = structuredClone(mockSmallControlWire.mounts.spaceError);
  }
  let discoveryCalls = 0;
  const discoveredServices = [
    { protocol: "smb", name: "Studio-NAS", host: "studio-nas.local", port: 445 },
    { protocol: "sftp", name: "Buildserver", host: "build.local", port: 2222 },
    { protocol: "webdav", name: "Cloud", host: "cloud.local", port: 8443 },
  ];
  window.__MOCK_DISCOVERY__ = () => ({
    calls: discoveryCalls,
    services: structuredClone(discoveredServices),
  });
  const queue = { version: 46, revision: 0, paused: false, stop_on_error: false, entries: [] };
  const newWindowCalls = [];
  let desktopPreferences = {
    icon: "blue", theme: "auto", byteUnits: "decimal", localFolderSizes: false,
    networkDiscovery: true, restoreSession: false, diffTool: "xcode",
  };
  let desktopRevision = 0;
  let desktopInitialized = false;
  const desktopLoadError = mockParams.has("desktopLoadError")
    ? { code: "invalid_input", message: "Einstellungsdatei ist keine reguläre Datei" }
    : null;
  const rememberedWindows = [];
  const diffCalls = [];
  let diffPendingResolve = null;
  window.__MOCK_DESKTOP__ = () => structuredClone({
    preferences: desktopPreferences, revision: desktopRevision,
    rememberedWindows, diffCalls,
  });
  let defaultDirCalls = 0;
  window.__MOCK_NEW_WINDOWS__ = () => structuredClone(newWindowCalls);
  window.__MOCK_DEFAULT_DIR_CALLS__ = () => defaultDirCalls;
  const updateCalls = [];
  let preparedUpdate = false;
  let installAttempts = 0;
  window.__MOCK_UPDATES__ = () => structuredClone(updateCalls);
  const actionOrder = [];
  window.__MOCK_ACTION_ORDER__ = () => structuredClone(actionOrder);
  const appErrorRecords = [];
  let cancelSyncCalls = 0;
  window.__MOCK_IPC_EFFECTS__ = () => structuredClone({
    appErrors: appErrorRecords,
    cancelSyncCalls,
    lastLocalRename: document.documentElement.dataset.lastLocalRename || null,
  });
  const startupState = { reads: 0, wiredAtRead: false, delivered: false };
  window.__MOCK_STARTUP_STATE__ = () => structuredClone(startupState);
  let pendingForward = null;
  const forwardPreviews = new Map();
  const forwardRequests = new Map();
  const forwardStatuses = new Map();
  const forwardCalls = [];
  window.__MOCK_FORWARD_STATE__ = () => structuredClone({ calls: forwardCalls, pending: pendingForward != null });
  let pendingBidirectional = null;
  let pendingBidirectionalRollback = null;
  const bidirectionalPairs = new Map();
  let bidirectionalRecoveryPreparationSequence = 0;
  let bidirectionalRecoveryDiscardAttempts = 0;
  let pendingProvider = null;
  let contentPreparationFailure = mockParams.get("contentPrepareFails");
  window.__MOCK_REPAIR_CONTENT_PREPARATION__ = () => { contentPreparationFailure = null; };
  let pendingContentSync = null;
  const contentSyncPreviews = new Map();
  const contentSyncCalls = [];
  window.__MOCK_CONTENT_SYNC_STATE__ = () => structuredClone({ pending: pendingContentSync != null, calls: contentSyncCalls });
  let pendingLocalSync = null;
  const localSyncPreviews = new Map();
  const localSyncCalls = [];
  window.__MOCK_LOCAL_SYNC_STATE__ = () => structuredClone({ pending: pendingLocalSync != null, calls: localSyncCalls });
  const preparationActions = [];
  window.__MOCK_PREPARATION_ACTIONS__ = () => structuredClone({
    calls: preparationActions,
    pendingProvider: pendingProvider != null,
    pendingBidirectional: pendingBidirectional != null,
    pendingBidirectionalRollback: pendingBidirectionalRollback != null,
  });
  const secretWrites = [];
  const secretTransactions = [];
  const pendingSecrets = new Map();
  const connectionActions = [];
  const quarantineCalls = [];
  const quarantineActions = [];
  let quarantineRuns = quarantineRace ? [
    { id: "run-langsam", state: "complete", status: "complete", created_unix_ms: Date.now() - 2000,
      expires_unix_ms: Date.now() + 86_400_000, target_root: "/mock/ziel",
      items_available: 1, items_total: 1, bytes_available: 8 },
    { id: "run-schnell", state: "complete", status: "complete", created_unix_ms: Date.now() - 1000,
      expires_unix_ms: Date.now() + 86_400_000, target_root: "/mock/ziel",
      items_available: 1, items_total: 1, bytes_available: 8 },
  ] : quarantineData ? [{
      id: "run-1", state: "complete", status: quarantineStatus, created_unix_ms: Date.now() - 1000,
      expires_unix_ms: Date.now() + 86_400_000, target_root: "/mock/ziel",
      items_available: quarantineRecoveryPending ? 0 : 1,
      items_restorable: quarantineManualRecovery ? 0 : (quarantineRecoveryPending ? 0 : 1),
      items_manual_recovery: quarantineManualRecovery ? 1 : 0,
      items_recovery_pending: quarantineRecoveryPending ? 1 : 0,
      items_total: 1, items_restored: 0,
      entries_available: quarantineRecoveryPending ? 0 : 1,
      bytes_available: quarantineRecoveryPending ? 0 : 8,
    }] : [];
  let quarantineEntries = quarantineData ? [{
    relative_path: "alt.txt", is_dir: false, reason: "replaced", available: !quarantineRecoveryPending,
    recovery_pending: quarantineRecoveryPending,
    automatic_restore_available: !quarantineManualRecovery,
    manual_recovery_required: quarantineManualRecovery,
    restored: false, entries: quarantineRecoveryPending ? 0 : 1,
    bytes: quarantineRecoveryPending ? 0 : 8,
  }] : [];
  let quarantineBadgeRaceStep = 0;
  let quarantineBusy = false;
  let quarantinePurgeAttempted = false;
  let favorites = [];
  const activeRemote = new Set();
  let sftpProfiles = [{ id: "mock_sftp", name: "Mock-SFTP", revision: 11, connection: {
    host: "sftp.mock.test", port: 22, username: "tester", requested_root: "/daten",
    alternate_hosts: ["2001:db8::8"],
    host_key: { algorithm: "ssh-ed25519", sha256: Array(32).fill(9) },
    authentication: { type: "password", secret:
      { service: "Doppeldecker-Test", account: "mock_sftp" } },
  }}];
  let ftpsProfiles = [{ id: "mock_ftps", name: "Mock-FTPS", revision: 21, connection: {
    host: "ftps.mock.test", port: 21, username: "tester", requested_root: "/daten",
    transport_security: "explicit_tls",
    certificate: { sha256: Array(32).fill(7) }, certificate_policy: "pinned_certificate",
    password_reference: { service: "Doppeldecker-Test", account: "mock_ftps" },
  }}];
  let webdavProfiles = [{ id: "mock_webdav", name: "Mock-WebDAV", revision: 31, connection: {
    origin: "https://dav.mock.test", username: "tester", requested_root: "/daten",
    certificate: { sha256: Array(32).fill(5) }, certificate_policy: "pinned_certificate",
    password_reference: { service: "Doppeldecker-Test", account: "mock_webdav" },
  }}];
  let smbProfiles = [{ id: "mock_smb", name: "Mock-SMB", revision: 41, connection: {
    host: "smb.mock.test", port: 445, server_guid: Array(16).fill(6), share: "Daten",
    username: "tester", domain: "WORKGROUP", requested_root: "/daten",
    password_reference: { service: "Doppeldecker-Test", account: "mock_smb" },
  }}];
  const profileReadQueues = { sftp: [], ftps: [], webdav: [], smb: [] };
  window.__MOCK_QUEUE_PROFILE_READS__ = (protocol, replies) => {
    profileReadQueues[protocol] = structuredClone(replies);
  };
  if (mockParams.has("remoteFavorite")) {
    favorites = [{
      name: "Remote-Favorit", path: "/archiv", provider: "sftp", profile_id: "mock_sftp",
    }];
  }
  const profilesFor = protocol => protocol === "sftp" ? sftpProfiles
    : protocol === "ftps" ? ftpsProfiles
      : protocol === "webdav" ? webdavProfiles : smbProfiles;
  const ftpSecurityStatus = ({ transport_security: transport, certificate_policy: policy }) => {
    if (transport === "plaintext") return "plaintext";
    if (policy === "pinned_certificate") return "tls_identity_externally_verified";
    if (policy === "web_pki" || policy === "web_pki_and_pin") return "tls_identity_verified";
    return "tls_identity_unverified";
  };
  const requireProfileRevision = (protocol, profileId, profileRevision) => {
    const profile = profilesFor(protocol).find(candidate => candidate.id === profileId);
    if (!profile || profileRevision !== profile.revision) {
      throw { code: "plan_drift", message: "Der Mock verlangt die aktuelle Profilrevision" };
    }
    return profile;
  };
  const requireProfileMutationRevision = (protocol, profileId, expectedRevision) => {
    const current = profilesFor(protocol).find(candidate => candidate.id === profileId);
    if ((current?.revision ?? null) !== expectedRevision) {
      throw { code: "plan_drift", message: "Das Profil wurde in einem anderen Fenster geändert" };
    }
    return current;
  };
  const secretKey = reference => `${reference.service}\n${reference.account}`;
  const storedSecretReferences = new Set([
    sftpProfiles[0].connection.authentication.secret,
    ftpsProfiles[0].connection.password_reference,
    webdavProfiles[0].connection.password_reference,
    smbProfiles[0].connection.password_reference,
  ].map(secretKey));
  let mockSecretReferenceSequence = 0;
  let mockSecretCaptureSequence = 0;
  const replaceStoredProfile = (protocol, profile) => {
    const profiles = protocol === "sftp" ? sftpProfiles
      : protocol === "ftps" ? ftpsProfiles
        : protocol === "webdav" ? webdavProfiles : smbProfiles;
    const previous = profiles.find(candidate => candidate.id === profile.id);
    const stored = { ...profile, revision: (previous?.revision || 0) + 1 };
    if (protocol === "sftp") {
      sftpProfiles = sftpProfiles.filter(candidate => candidate.id !== profile.id);
      sftpProfiles.push(stored); return structuredClone(sftpProfiles);
    }
    if (protocol === "ftps") {
      ftpsProfiles = ftpsProfiles.filter(candidate => candidate.id !== profile.id);
      ftpsProfiles.push(stored); return structuredClone(ftpsProfiles);
    }
    if (protocol === "webdav") {
      webdavProfiles = webdavProfiles.filter(candidate => candidate.id !== profile.id);
      webdavProfiles.push(stored); return structuredClone(webdavProfiles);
    }
    smbProfiles = smbProfiles.filter(candidate => candidate.id !== profile.id);
    smbProfiles.push(stored); return structuredClone(smbProfiles);
  };
  window.__MOCK_CONNECTION_STATE__ = () => structuredClone({
    sftpProfiles, ftpsProfiles, webdavProfiles, smbProfiles, secretWrites,
    secretTransactions, pendingSecretCount: pendingSecrets.size,
  });
  window.__MOCK_SET_PROFILE_REVISION__ = (protocol, profileId, revision) => {
    const profiles = protocol === "sftp" ? sftpProfiles
      : protocol === "ftps" ? ftpsProfiles
        : protocol === "webdav" ? webdavProfiles : smbProfiles;
    const profile = profiles.find(candidate => candidate.id === profileId);
    if (profile) profile.revision = revision;
  };
  window.__MOCK_REMOVE_PROFILE__ = (protocol, profileId) => {
    if (protocol === "sftp") sftpProfiles = sftpProfiles.filter(profile => profile.id !== profileId);
    if (protocol === "ftps") ftpsProfiles = ftpsProfiles.filter(profile => profile.id !== profileId);
    if (protocol === "webdav") webdavProfiles = webdavProfiles.filter(profile => profile.id !== profileId);
    if (protocol === "smb") smbProfiles = smbProfiles.filter(profile => profile.id !== profileId);
  };
  window.__MOCK_SCAN_CALLS__ = () => structuredClone(scanCalls);
  window.__MOCK_CONNECTION_ACTIONS__ = () => structuredClone(connectionActions);
  window.__MOCK_QUARANTINE_CALLS__ = () => structuredClone(quarantineCalls);
  window.__MOCK_QUARANTINE_ACTIONS__ = () => structuredClone(quarantineActions);
  window.__MOCK_ARM_QUARANTINE_BADGE_RACE__ = () => {
    quarantineBadgeRaceStep = 1;
  };
  // Ab hier antwortet das Listing wie das Backend waehrend einer laufenden Datei-
  // oder Sync-Operation: mit dem Code `busy`, nicht mit einem Quarantaenefehler.
  window.__MOCK_ARM_QUARANTINE_BUSY__ = () => {
    quarantineBusy = true;
  };
  window.__MOCK_IMPORT_FTPS_PROFILE__ = () => {
    ftpsProfiles.push({
      id: "cli_ftps",
      name: "CLI-FTPS",
      revision: 51,
      connection: {
        ...ftpsProfiles[0].connection,
        host: "cli-import.mock.test",
        alternate_hosts: ["2001:db8::7"],
        certificate_policy: "unverified_certificate",
      },
    });
  };
  window.__MOCK_IMPORT_FTP_PROFILE__ = () => {
    ftpsProfiles.push({
      id: "cli_ftp",
      name: "CLI-FTP",
      revision: 52,
      connection: {
        host: "legacy-ftp.mock.test",
        port: 21,
        username: "tester",
        requested_root: "/daten",
        transport_security: "plaintext",
        password_reference: { service: "Doppeldecker-Test", account: "cli_ftp" },
      },
    });
  };
  if (demoMode) {
    favorites = demoFavorites.map(favorite => ({ ...favorite, provider: "local" }));
    sftpProfiles = [{ id: "demo_sftp", name: "Backup-Server", revision: 11, connection: {
      ...sftpProfiles[0].connection, host: "backup.example.com", username: "demo" } }];
    ftpsProfiles = [{ id: "demo_ftps", name: "Webhosting", revision: 21, connection: {
      ...ftpsProfiles[0].connection, host: "ftp.example.com", username: "web-demo" } }];
    webdavProfiles = [{ id: "demo_webdav", name: "Nextcloud", revision: 31, connection: {
      ...webdavProfiles[0].connection, origin: "https://cloud.example.com", username: "demo" } }];
    smbProfiles = [{ id: "demo_smb", name: "NAS (Arbeitszimmer)", revision: 41, connection: {
      ...smbProfiles[0].connection, host: "nas.example.com", share: "Medien" } }];
    // Eine lebende Sitzung, damit der Eject-Knopf am Backup-Server sichtbar ist.
    activeRemote.add("sftp:demo_sftp");
  }
  let queueSequence = 0;
  const queueEvent = () => queueMicrotask(() =>
    listeners.get("queue-updated")?.({ payload: structuredClone(queue) }));
  const queueEntry = operation => ({ operation_id: `mock-${++queueSequence}`,
    operation, created_unix_ms: Date.now(), state: "Pending", attempts: 0,
    progress: { files_total: 0, files_done: 0, bytes_total: 0, bytes_done: 0,
      current: "", phase: "" }, attention: null, error: null, error_code: null,
    archive_extract_report: null, profile_bindings: [], file_dacl_recovery: false,
    temp_recovery_paths: [], retained_backup_paths: [], file_incomplete: false,
    recovery_available: false, bidirectional_completion_recovery: null });
  window.__DOPPELDECKER_MOCK_QUEUE__ = queue;
  if (mockParams.has("queueWire")) {
    const id = "generated-11111111111111111111111111111111";
    const workspace = `/mock/generated-sources/${id}`;
    const root = `${workspace}/content`;
    const entry = queueEntry({
      type: "ProviderFile", kind: "Copy",
      source: { provider: "Local", root },
      target: { provider: "Smb", root: "/ziel", connection: { host: "smb.example.test", port: 445,
        server_guid: Array(16).fill(1), share: "Test", username: "tester", domain: "",
        requested_root: "/ziel", password_reference: { service: "mock", account: "tester" } } },
      items: [`${root}/neu`], destination: "/ziel", overwrite: false, expected_conflicts: [],
      expected_conflict_proofs: [], keep_both_targets: [], source_tree_expected: null,
      local_copy_new_best_effort_expected: null, copy_new_best_effort_expected: null,
      replace_best_effort_expected: null, remove_source_best_effort_expected: null,
    });
    const key = { origin: "ExclusiveMinimalCreate", directory: "/ziel/neu", object_namespace: Array(32).fill(1) };
    const outcome = { ...key, before_digest: Array(32).fill(2), after_digest: Array(32).fill(3) };
    const report = {
      created: [
        { effect_key: { ...key, file_id: "9007199254740992" }, ordinary_metadata_applied: false },
        { effect_key: { ...key, file_id: "9007199254740993" }, ordinary_metadata_applied: true },
      ],
      completed: [{ ...outcome, file_id: "9007199254740993" }],
      uncertain: [{ ...outcome, file_id: "18446744073709551615" }],
    };
    Object.assign(entry, {
      created_unix_ms: 1_800_000_000_000, state: "Interrupted", attempts: 2,
      error_code: "outcome_unclear", created_directory_dacl_recovery: true,
      temp_recovery_paths: [workspace],
      generated_source: { id, manifest_digest: Array(32).fill(1), kind: "zip_extraction",
        label: "Archivauswahl", files: 0, directories: 1, bytes: 0, issues: [] },
      generated_source_recovery: true,
      created_directory_dacl_report: report,
      progress: { ...entry.progress, phase: "interrupted", created_directory_dacl_report: report },
    });
    queue.entries.push(entry);
  }
  if (mockParams.has("pendingQueue")) {
    for (const [name, destination] of [
      ["Alpha.txt", "/mock/ziel-a"],
      ["Beta.txt", "/mock/ziel-b"],
      ["Gamma.txt", "/mock/ziel-c"],
    ]) {
      queue.entries.push(queueEntry({
        type: "File", kind: "Copy", items: [`/mock/${name}`], destination,
        overwrite: false, expected_conflicts: [],
      }));
    }
  }
  const legacyWebdavState = mockParams.get("legacyWebdav") || "";
  if (["failed", "interrupted"].includes(legacyWebdavState)) {
    const entry = queueEntry({
      type: "ProviderDelete",
      endpoint: { provider: "WebDav", root: "/", connection: {} },
      mode: "permanent",
      expected_plan: {
        root: "/",
        root_identity: { identity: { root_etag: "__doppeldecker_legacy_unbound__" } },
        items: [], entries: 0, bytes: 0, digest: Array(32).fill(0),
      },
    });
    entry.state = legacyWebdavState === "failed" ? "Failed" : "Interrupted";
    entry.error = legacyWebdavState === "failed"
      ? "Älterer WebDAV-Löschplan wurde nicht ausgeführt und darf nicht wiederholt werden."
      : "Älterer WebDAV-Löschplan kann eine Teilwirkung gehabt haben und muss manuell geprüft werden.";
    queue.entries.push(entry);
  }
  window.__DOPPELDECKER_MOCK_ADD_BIDI_RECOVERY__ = () => {
    const entry = queueEntry({ type: "Bidirectional", pair_id: Array(16).fill(4),
      pair_name: "Recovery-Paar", left: { provider: "Local", root: "/mock/quelle" },
      right: { provider: "Local", root: "/mock/ziel" },
      baseline_path: "/mock/state/paar.json", resolutions: [],
      expected_baseline_digest: null, expected_plan_digest: Array(32).fill(3) });
    entry.state = "Interrupted";
    entry.error = "Vorheriger Lauf besitzt gebundene Teilprojektionen";
    entry.recovery_available = true;
    queue.entries.push(entry); queueEvent(); return structuredClone(entry);
  };
  window.__DOPPELDECKER_MOCK_ADD_BIDI_COMPLETION__ = marker => {
    const entry = queueEntry({ type: "Bidirectional", pair_id: Array(16).fill(5),
      pair_name: "Abschluss-Paar", left: { provider: "Local", root: "/mock/quelle" },
      right: { provider: "Local", root: "/mock/ziel" },
      baseline_path: "/mock/state/abschluss.json", resolutions: [],
      expected_baseline_digest: null, expected_plan_digest: Array(32).fill(6) });
    entry.state = marker === "rolled_back" ? "Cancelled" : "Completed";
    entry.progress.phase = marker === "rolled_back" ? "rolled_back" : "completed";
    entry.bidirectional_completion_recovery = marker;
    queue.entries.push(entry); queueEvent(); return structuredClone(entry);
  };
  if (new URLSearchParams(location.search).has("bidiRecovery")) {
    window.__DOPPELDECKER_MOCK_ADD_BIDI_RECOVERY__();
  }
  if (mockParams.has("forwardStartup") || mockParams.has("forwardCompletion")) {
    const entry = queueEntry({ type: "BidirectionalForward", pair_id: Array(16).fill(17),
      pair_name: "Forward-Startpaar", left: { provider: "Sftp", root: "/" }, right: { provider: "Local", root: "/mock/ziel" },
      baseline_path: "/mock/state/forward-start.json", resolutions: [], expected_baseline_digest: Array(32).fill(1),
      expected_plan_digest: Array(32).fill(2), expected_unresolved_digest: Array(32).fill(3), expected_preview_digest: Array(32).fill(4) });
    entry.state = "Interrupted"; entry.attempts = 1; entry.recovery_available = !mockParams.has("forwardCompletion");
    entry.bidirectional_completion_recovery = mockParams.has("forwardCompletion") ? "complete" : null;
    entry.error = "Vorheriger Forwardlauf besitzt ein dauerhaftes Journal";
    queue.entries.push(entry);
  }
  if (bidirectionalFastApproveBeforeResponse) {
    const entry = queueEntry({ type: "Bidirectional", pair_id: Array(16).fill(7),
      pair_name: "Schnelles Queue-Paar", left: { provider: "Local", root: "/mock/quelle" },
      right: { provider: "Local", root: "/mock/ziel" },
      baseline_path: "/mock/state/schnell.json", resolutions: [],
      expected_baseline_digest: null, expected_plan_digest: Array(32).fill(3) });
    entry.state = "AwaitingConfirmation";
    entry.error = "Bidirektionaler Lauf ist frisch geprüft";
    entry.attention = { type: "RemoteRun", confirmation: "BEIDE SCHNELL 0011223344556677",
      backup_confirmation_required: false };
    queue.entries.push(entry);
  }
  const manifest = {
    owner: { uid: 501, gid: 20 }, mode: 493, acl: null,
    xattrs: [{ name: [117, 115, 101, 114, 46, 116, 101, 115, 116], value: { size: 4, digest: Array(32).fill(1) } }],
    resource_fork: null, sparse_extents: [{ offset: 0, length: 12 }],
    hardlink_group: null, digest: Array(32).fill(2),
  };
  const metadata = {
    source: manifest, target: null, owner: true, acl: false,
    incompatible_acl_model: false, normal_mode: true, special_mode_bits: true,
    xattrs: false, resource_fork: false, sparse_extents: true, hardlinks: false,
  };
  const comparisonEntry = (action, target = false) => {
    const directory = action.type === "CreateDir" || action.type === "DeleteDir";
    return {
      kind: directory ? "Directory" : "File",
      size: target ? (action.target_size ?? action.size ?? 0) : (action.size ?? 0),
      mtime: target ? (action.target_mtime ?? null) : (action.source_mtime ?? null),
      created: null, owner: null, group: null, extension: null,
      mode: target ? (action.target_mode ?? null) : (action.source_mode ?? null),
    };
  };
  const comparisonFromActions = actions => {
    const categories = { additions: 0, updates: 0, quarantines: 0, skipped: 0 };
    const rows = actions.map(action => {
      const parts = action.path.split("/");
      const ancestors = parts.slice(0, -1).map((_, index) => parts.slice(0, index + 1).join("/"));
      const isDelete = action.type === "DeleteFile" || action.type === "DeleteDir";
      const isSkip = action.type.startsWith("Skip");
      const isUpdate = action.type === "UpdateFile" || action.type === "SetMode"
        || action.type === "SetDirectoryMetadata" || (action.type === "CreateHardlink" && action.target_size != null);
      const effect = isDelete ? "Quarantine" : isSkip ? "Skip" : isUpdate ? "Update" : "Add";
      categories[{ Add: "additions", Update: "updates", Quarantine: "quarantines", Skip: "skipped" }[effect]] += 1;
      return {
        path: action.path, depth: ancestors.length, ancestors, action: effect,
        affected_side: isSkip ? "Neither" : "Target",
        source: isDelete ? null : comparisonEntry(action),
        target: isDelete ? comparisonEntry(action, true) : isUpdate ? comparisonEntry(action, true) : null,
      };
    });
    return { rows, categories };
  };
  const plan = {
    actions: [{ type: "CopyFile", path: "Programm", size: 12, source_mtime: 1,
      source_mode: 493, source_hash: "abc", metadata }],
    copy_count: 1, update_count: 0, metadata_count: 0, delete_count: 0,
    skip_count: 0, source_count: 1, target_count: 0, delete_bytes: 0,
    bytes_to_transfer: 12, hashed_files: 1, bytes_hashed: 12,
    metadata_report: {
      profile: "NativeBestEffort", content_changes: 1, metadata_changes: 1,
      hardlink_actions: 0, unsupported_metadata: 1, privilege_failures: 0,
      incompatible_acl_models: 0, strict_verified: false,
      classes: [
        { class: "Owner", transferred: 1, omitted: 0 },
        { class: "Xattrs", transferred: 0, omitted: 1 },
        { class: "SparseExtents", transferred: 1, omitted: 0 },
      ],
      issues: [{ path: "Programm", class: "Xattrs", kind: "Unsupported",
        reason: "Quelle ist lesbar, Ziel kann dieses Merkmal nicht exakt darstellen" }],
    },
  };
  plan.comparison = comparisonFromActions(plan.actions);
  if (demoMode) {
    const demoBytes = 48210 + 2411520 + 134217728 + 12480 + 812;
    // Portable-Plan: bewusst ohne native Metadaten-Detailzeile pro Eintrag.
    const act = (type, path, size) => ({ type, path, size, source_mtime: 1784600000,
      source_mode: 420, source_hash: "demo" });
    plan.actions = [
      act("CopyFile", "Budget 2026.xlsx", 48210),
      act("CopyFile", "Präsentation.pdf", 2411520),
      act("CopyFile", "archiv.zip", 134217728),
      act("UpdateFile", "Reisebericht.md", 12480),
      act("CopyFile", "notiz.txt", 812),
    ];
    Object.assign(plan, {
      copy_count: 4, update_count: 1, metadata_count: 0, delete_count: 0,
      skip_count: 0, source_count: 7, target_count: 4, delete_bytes: 0,
      bytes_to_transfer: demoBytes, hashed_files: 5, bytes_hashed: demoBytes,
    });
    plan.metadata_report = {
      profile: "Portable", content_changes: 5, metadata_changes: 0,
      hardlink_actions: 0, unsupported_metadata: 0, privilege_failures: 0,
      incompatible_acl_models: 0, strict_verified: false, classes: [], issues: [],
    };
    plan.comparison = comparisonFromActions(plan.actions);
  }
  if (mockParams.has("mirrorDelete")) {
    plan.actions = [{ type: "DeleteFile", path: "Alt.txt", size: 4 }];
    plan.copy_count = 0;
    plan.delete_count = 1;
    plan.target_count = 10;
    plan.delete_bytes = 4;
    plan.bytes_to_transfer = 0;
    plan.comparison = comparisonFromActions(plan.actions);
  }
  if (mockParams.has("comparisonMixed")) {
    plan.actions = [
      { type: "CreateDir", path: "Fotos", source_mtime: 1784700000, source_mode: 493 },
      { type: "CopyFile", path: "Fotos/neu.jpg", size: 1200, source_mtime: 1784700001, source_mode: 420 },
      { type: "UpdateFile", path: "aktuell.txt", size: 8, source_mtime: 1784700002,
        source_mode: 420, target_size: 4, target_mtime: 1784600000, target_mode: 420 },
      { type: "DeleteFile", path: "alt.txt", size: 6, target_mtime: 1784500000, target_mode: 420 },
      { type: "SkipConflict", path: "konflikt" },
    ];
    Object.assign(plan, {
      copy_count: 1, update_count: 1, metadata_count: 0, delete_count: 1,
      skip_count: 1, delete_bytes: 6, bytes_to_transfer: 1208,
    });
    plan.comparison = comparisonFromActions(plan.actions);
  }
  if (comparisonRowCount > 0) {
    plan.actions = Array.from({ length: comparisonRowCount }, (_, index) => ({
      type: "CopyFile",
      path: `Ordner-${String(Math.floor(index / 100) + 1).padStart(3, "0")}/Datei-${String(index + 1).padStart(5, "0")}.txt`,
      size: index + 1,
      source_mtime: 1784700000 + index,
      source_mode: 420,
    }));
    plan.copy_count = comparisonRowCount;
    plan.update_count = 0;
    plan.delete_count = 0;
    plan.skip_count = 0;
    plan.bytes_to_transfer = comparisonRowCount * (comparisonRowCount + 1) / 2;
    plan.comparison = comparisonFromActions(plan.actions);
  }
  if (mockParams.has("runningSmb")) {
    const entry = queueEntry({
      type: "SmbSync",
      direction: "Upload",
      local_root: "/mock/quelle",
      remote_root: "/daten",
      options: { mode: "AddOnly", compare: "ContentHash", metadata: "Portable" },
      expected_plan: structuredClone(plan),
    });
    entry.state = "Running";
    queue.entries.push(entry);
  }
  if (mockParams.has("runningBidirectional")) {
    const entry = queueEntry({
      type: "Bidirectional",
      pair_id: Array(16).fill(4),
      pair_name: "Laufendes Paar",
      left: { provider: "Local", root: "/mock/quelle" },
      right: { provider: "Local", root: "/mock/ziel" },
      baseline_path: "/mock/state/paar.json",
      resolutions: [],
      expected_baseline_digest: null,
      expected_plan_digest: Array(32).fill(3),
    });
    entry.state = "Running";
    queue.entries.push(entry);
  }
  const localIdentity = (path, inode) => ({ provider: "Local", identity: {
    device: 1, inode, is_directory: true, display_path: path,
    mount: { mount_id: 42, unique_mount_id: true, fsid: [1, 2, 3, 4],
      volume_id: Array(16).fill(7), fs_type: "mockfs", mount_flags: 0 },
  }});
  const sftpIdentity = path => ({ provider: "Sftp", identity: {
    server: { host: "sftp.mock.test", port: 22,
      host_key: { algorithm: "ssh-ed25519", sha256: Array(32).fill(9) },
      canonical_root: "/daten", sftp_version: 3, extensions_digest: Array(32).fill(6) },
    virtual_root: path, session_generation: 1,
  }});
  const ftpsIdentity = path => ({ provider: "Ftps", identity: {
    server: { host: "ftps.mock.test", port: 21,
      transport_security: "explicit_tls",
      certificate: { sha256: Array(32).fill(7) }, canonical_root: "/daten",
      features_digest: Array(32).fill(4), welcome_digest: Array(32).fill(5) },
    virtual_root: path, session_generation: 1,
  }});
  const webdavIdentity = path => ({ provider: "WebDav", identity: {
    server: { origin: "https://dav.mock.test",
      certificate: { sha256: Array(32).fill(5) }, account_digest: Array(32).fill(2),
      canonical_root: "/daten", dav_header_digest: Array(32).fill(3),
      server_header_digest: Array(32).fill(4) },
    virtual_root: path, session_generation: 1,
  }});
  const smbIdentity = path => ({ provider: "Smb", identity: {
    server: { host: "smb.mock.test", port: 445, server_guid: Array(16).fill(6),
      share: "Daten", account_digest: Array(32).fill(2), canonical_root: "/daten",
      dialect: 785, signing_active: true, encryption_active: true,
      signing_algorithm: "AesGmac", encryption_cipher: "Aes128Gcm" },
    virtual_root: path, root_file_id: 42, tree_id: 7, session_generation: 1,
  }});
  const remotePreview = (args, protocol = "sftp") => {
    const now = Date.now();
    const upload = args.direction === "Upload";
    const remoteIdentity = protocol === "ftps" ? ftpsIdentity : protocol === "webdav" ? webdavIdentity : protocol === "smb" ? smbIdentity : sftpIdentity;
    const source = upload ? localIdentity(args.localRoot, 10) : remoteIdentity(args.remoteRoot);
    const target = upload ? remoteIdentity(args.remoteRoot) : localIdentity(args.localRoot, 11);
    return { source_identity: source, target_identity: target,
      quarantine_identity: ["ftps", "webdav"].includes(protocol) && upload && args.options.mode === "AddOnly" ? target :
        (upload ? remoteIdentity("/.doppeldecker-quarantine") : localIdentity("/mock/.doppeldecker-quarantine", 12)),
      source_listing: { root_identity: source, directories_started: 1, directories_completed: 1,
        entries_seen: 1, listing_digest: Array(32).fill(3), provider_token: null, errors: [], complete: true },
      target_listing: { root_identity: target, directories_started: 1, directories_completed: 1,
        entries_seen: 0, listing_digest: Array(32).fill(4), provider_token: null, errors: [], complete: true },
      plan: structuredClone(plan), options: structuredClone(args.options),
      impact: { new_files: 1, new_bytes: 12, replacements: 0,
        replacement_new_bytes: 0, replacement_old_bytes: 0, metadata_changes: 0,
        quarantine_entries: 0, quarantine_bytes: 0, required_available_bytes: 4108,
        available_bytes: 1000000, available_bytes_known: !["ftps", "webdav"].includes(protocol) || !upload },
      plan_digest: Array(32).fill(5), confirmation_phrase: "REAL remote K7M4Q2",
      prepared_at_unix_ms: now, expires_at_unix_ms: now + 600000,
      restore_probe_ok: !(["ftps", "webdav"].includes(protocol) && upload && args.options.mode === "AddOnly"),
      backup_confirmation_required: args.options.mode !== "AddOnly" };
  };
  const providerIdentity = endpoint => ({
    sftp: sftpIdentity,
    ftps: ftpsIdentity,
    webdav: webdavIdentity,
    smb: smbIdentity,
  })[endpoint.provider](endpoint.root);
  const providerPreview = request => {
    const preview = remotePreview({
      direction: "Download", localRoot: "/mock/ziel",
      remoteRoot: request.source.root, options: request.options,
    });
    preview.source_identity = providerIdentity(request.source);
    preview.target_identity = providerIdentity(request.target);
    preview.quarantine_identity = providerIdentity(request.target);
    preview.source_listing.root_identity = preview.source_identity;
    preview.target_listing.root_identity = preview.target_identity;
    preview.confirmation_phrase = "REAL provider K7M4Q2";
    return preview;
  };
  const queueEndpoint = endpoint => ({
    provider: ({ local: "Local", sftp: "Sftp", ftps: "Ftps", webdav: "WebDav", smb: "Smb" })[endpoint.provider],
    root: endpoint.root,
  });
  const conflictEntry = (endpoint, size = 17) => ({ endpoint, exists: true, kind: "file", size, mtime: 1792108800 });
  const binaryPath = name => [[...new TextEncoder().encode(name)]];
  const conflictPath = binaryPath("konflikt.txt");
  const bidirectionalPreview = request => ({
    conflict_details: bidirectionalConflict && request.resolutions.length === 0 ? [{
      path: conflictPath, relative_name: "konflikt.txt", left: conflictEntry(request.left.root), right: conflictEntry(request.right.root, 23),
      skip_blocker: null,
      choices: [
        { decision: { decision: "keep_left" }, explanation: "Linke Fassung übernehmen.", generated_name: null, blocker: null },
        { decision: { decision: "keep_right" }, explanation: "Rechte Fassung übernehmen.", generated_name: null, blocker: null },
        { decision: { decision: "skip" }, explanation: "Beide Fassungen unverändert lassen.", generated_name: null, blocker: null },
        ...["left", "right"].map(primary => ({ decision: { decision: "keep_both", primary, conflict_path: binaryPath("konflikt (Konflikt).txt") }, generated_name: "konflikt (Konflikt).txt", explanation: "Beide Fassungen erhalten.", blocker: null })),
      ],
    }] : [],
    plan: { pair_id: request.pairId, baseline_generation: null,
      baseline_digest: null, left_root_identity: localIdentity(request.left.root, 10),
      right_root_identity: localIdentity(request.right.root, 11),
      left_snapshot_digest: Array(32).fill(1), right_snapshot_digest: Array(32).fill(2),
      actions: bidirectionalConflict && request.resolutions.length === 0
        ? [{ type: "conflict", path: conflictPath, kind: "initial_difference", baseline: null,
          left: Array(32).fill(1), right: Array(32).fill(2) }]
        : [],
      conflicts: bidirectionalConflict && request.resolutions.length === 0 ? 1 : 0,
      executable: true, plan_digest: Array(32).fill(3) },
    confirmation_phrase: `BEIDE ${request.pairName} 0011223344556677`,
    expires_at_unix_ms: Date.now() + 600000,
    backup_confirmation_required: false, stage_blocker: null, scope_warnings: [],
  });
  const forwardPreview = request => {
    const { plan: bidirectionalPlan, conflict_details, scope_warnings, stage_blocker, expires_at_unix_ms } = bidirectionalPreview(request);
    const preview = { plan: bidirectionalPlan, conflict_details, scope_warnings, stage_blocker, expires_at_unix_ms };
    preview.execution_class = "best_effort_forward";
    preview.unresolved_digest = Array(32).fill(41);
    preview.digest = Array(32).fill(request.resolutions.length ? 43 : 42);
    preview.recovery = request.recovery ? { previous_run_id: Array(16).fill(5), changed_left: [binaryPath("eins.txt")], changed_right: [binaryPath("zwei.txt")],
      running_projections: [1], warning: "Der vorherige Lauf kann bereits eine Datei geschrieben haben. B0 blieb unverändert." } : null;
    preview.limitations = ["Kein universelles Rollback; bei unklarem Ausgang kein automatischer Wiederholversuch."];
    if (!preview.plan.conflicts) preview.plan.actions = ["eins.txt", "zwei.txt", "drei.txt"].map(name => ({
      type: "copy", path: binaryPath(name), direction: "left_to_right", expected_source: Array(32).fill(1), expected_target: null,
    }));
    const identity = endpoint => endpoint.provider === "local" ? localIdentity(endpoint.root, 10) : providerIdentity(endpoint);
    preview.plan.left_root_identity = identity(request.left); preview.plan.right_root_identity = identity(request.right);
    preview.effects = preview.plan.conflicts ? [] : [[request.left, request.right], [request.right, request.left]].map(([source, target]) => ({
      source_root: source.root, target_root: target.root, source_provider: source.provider, target_provider: target.provider,
      source_identity: identity(source), target_identity: identity(target),
      options: { mode: "Update", compare: "ContentHash", metadata: "Portable", mtime_tolerance: 2 },
      plan: { ...structuredClone(plan), actions: [], local_parent_inheritance: [],
        comparison: comparisonFromActions([]), copy_count: target === request.right ? 2 : 1,
        update_count: mockParams.has("forwardReplace") ? 1 : 0,
        metadata_report: { profile: "Portable", content_changes: 0, metadata_changes: 0,
          hardlink_actions: 0, unsupported_metadata: 0, privilege_failures: 0,
          incompatible_acl_models: 0, strict_verified: false, classes: [], issues: [] } },
      execution_class: "best_effort", digest: Array(32).fill(8), expires_unix_ms: preview.expires_at_unix_ms,
      restore_probe_ok: target.provider === "local", local_execution: null, irreversible_removal_bytes: 0,
      irreversible_replacements: mockParams.has("forwardReplace") && target.provider !== "local" ? 1 : 0, irreversible_removals: 0,
      quarantined_replacements: mockParams.has("forwardReplace") && target.provider === "local" ? 1 : 0, quarantined_removals: 0,
      quarantine_run: target.provider === "local" ? "lokal-geprüft" : null, blocked_removals: 0,
      limitations: ["Best Effort: Bestand vor und nach der Wirkung prüfen."], removal_confirmation: null,
    }));
    return preview;
  };
  const forwardPairStatus = (plan, completionAvailable, runByte) => ({
    execution_class: "best_effort_forward",
    run_id: Array(16).fill(runByte),
    plan: structuredClone(plan),
    projections: [],
    baseline_matches_previous: !completionAvailable,
    baseline_completion_available: completionAvailable,
    previous_runs: 0,
  });
  const forwardRequestForEntry = entry => forwardRequests.get(entry.operation_id) || ({ pairId: entry.operation.pair_id,
    pairName: entry.operation.pair_name, left: { ...entry.operation.left, provider: entry.operation.left.provider.toLowerCase() },
    right: { ...entry.operation.right, provider: entry.operation.right.provider.toLowerCase() }, resolutions: [] });
  function finishForwardMock(entry) {
    entry.state = "Running"; entry.attempts = (entry.attempts || 0) + 1; queueEvent();
    queueMicrotask(() => {
      entry.state = "Completed"; entry.recovery_available = false; entry.bidirectional_completion_recovery = null;
      forwardStatuses.delete(JSON.stringify(entry.operation.pair_id)); queueEvent();
      window.__MOCK_EMIT__("bidirectional-done", { operationId: entry.operation_id, attempt: entry.attempts, execution_class: "best_effort", runs: [], baseline_generation: 1, baseline_committed: true });
    });
  }
  function acceptsCommandArgument(value, rustType) {
    if (rustType === "String") return typeof value === "string";
    if (rustType === "bool") return typeof value === "boolean";
    if (rustType === "FavoriteProvider") return mockFavoriteWire.providers.includes(value);
    if (rustType === "PairId") return acceptsCommandArgument(value, "[u8;16]");
    if (rustType === "LocalSyncPreparationDigest") return acceptsCommandArgument(value, "[u8;32]");
    if (rustType === "ContentSyncPreparationDigest") return acceptsCommandArgument(value, "[u8;32]");
    const integer = /^(u|i)(8|16|32|64|size)$/.exec(rustType);
    if (integer) {
      const bits = integer[2] === "size" ? 64 : Number(integer[2]);
      const signed = integer[1] === "i";
      const limit = 2 ** (signed ? bits - 1 : bits);
      return Number.isInteger(value) && value >= (signed ? -limit : 0) && value < limit;
    }
    const vector = /^Vec<(.+)>$/.exec(rustType);
    const fixedArray = /^\[(.+);(\d+)\]$/.exec(rustType);
    if (vector || fixedArray) {
      return Array.isArray(value) && (!fixedArray || value.length === Number(fixedArray[2]))
        && Array.from(value).every(item => acceptsCommandArgument(item, (vector || fixedArray)[1]));
    }
    // Eigene Rust-DTOs validiert der jeweilige Mock-Command bei Bedarf fachlich.
    return true;
  }

  function validateCommandArguments(command, args) {
    if (!Object.hasOwn(mockTauriCommandArgs, command)) return;
    const contract = mockTauriCommandArgs[command];
    for (const [name, rustType] of Object.entries(contract)) {
      const value = args != null && Object.hasOwn(args, name) ? args[name] : undefined;
      const optional = /^Option<(.+)>$/.exec(rustType);
      if (optional && value == null) continue;
      if (value === undefined || !acceptsCommandArgument(value, optional ? optional[1] : rustType)) {
        throw { code: "invalid_input", message: `Ungültiges Argument für ${command}: ${name}` };
      }
    }
    return contract;
  }

  async function invoke(command, args = {}) {
    const contract = validateCommandArguments(command, args);
    if (command === "get_metadata_admin_status") return { available: true, state: "notRegistered", message: "Administratorhelfer noch nicht eingerichtet." };
    if (command === "get_file_property_capabilities") {
      const permissions = ["local", "sftp", "ftps"].includes(args.provider);
      return { permissions, recursivePermissions: permissions, administration: args.provider === "local" && mockParams.has("administrator"), ownership: args.provider === "local", tags: args.provider === "local" && mockParams.has("finderTags"), visibility: args.provider === "local" && mockParams.has("visibility"), nativeFlags: args.provider === "local" && mockParams.has("fileFlags"), comments: false,
        limitations: args.provider === "local" ? [] : [permissions ? "Best Effort: Serverpfade werden geprüft; gleichzeitiger Pfadtausch bleibt möglich." : "Dieser Provider bietet keinen Unix-Rechteeditor."] };
    }
    // Remote-Endpunkte spiegeln den Backendvertrag: Ein fehlender oder veralteter
    // Profilstand scheitert, statt im Mock unbemerkt durchzurutschen.
    const request = contract?.request && command !== "start_directory_sizes" ? args.request : undefined;
    for (const endpoint of [
      contract?.endpoint ? args.endpoint : undefined,
      request?.endpoint,
      request?.source,
      request?.target,
      request?.left,
      request?.right,
    ]) {
      if (["sftp", "ftps", "webdav", "smb"].includes(endpoint?.provider)) {
        requireProfileRevision(endpoint.provider, endpoint.profileId, endpoint.profileRevision);
      }
    }
    if (command === "app_version") return "0.1.124";
    if (command === "check_for_update") {
      updateCalls.push({ command });
      if (updateMode === "error") {
        throw { code: "update_failed", message: "Mock-Updatekanal nicht erreichbar" };
      }
      if (updateMode === "available") {
        preparedUpdate = true;
        return structuredClone(mockSmallControlWire.update.available);
      }
      return structuredClone(mockSmallControlWire.update.unchanged);
    }
    if (command === "install_update") {
      if (installDispatchFailsOnce && installAttempts === 0) {
        installAttempts += 1;
        throw { code: "ipc", message: "Mock-Transportfehler vor Command-Zustellung" };
      }
      updateCalls.push({ command, expectedVersion: args.expectedVersion });
      actionOrder.push("install_update");
      // Wie das echte Backend wird der vorbereitete Handle beim Start des
      // Installations-Commands entnommen, also auch auf dessen Fehlerpfad.
      if (!preparedUpdate) {
        throw { code: "update_missing", message: "Kein vorbereiteter Updatefund" };
      }
      preparedUpdate = false;
      installAttempts += 1;
      if (installFailsOnce && installAttempts === 1) {
        throw { code: "update_failed", message: "Mock-Download nach Invocation fehlgeschlagen" };
      }
      if (foreignRunningDuringInstall) {
        const foreign = queueEntry({
          type: "File", kind: "Copy", items: ["/mock/fremd.txt"],
          destination: "/mock/ziel", overwrite: false, expected_conflicts: [],
        });
        foreign.state = "Running";
        listeners.get("queue-updated")?.({ payload: {
          version: queue.version, paused: false, stop_on_error: false, entries: [foreign],
        } });
      }
      if (installDelayMs) await wait(installDelayMs);
      for (const event of mockSmallControlWire.update.progress) {
        listeners.get("update-progress")?.({ payload: structuredClone(event) });
      }
      return null;
    }
    if (command === "prepared_update_status") {
      updateCalls.push({ command });
      return structuredClone(preparedUpdate ? mockSmallControlWire.update.prepared : mockSmallControlWire.update.missing);
    }
    if (command === "take_window_start_state") return emptyWindowStart ? {
      left: { provider: "local", profileId: null, profileName: null, path: "/mock/quelle" },
      right: { provider: "local", profileId: null, profileName: null, path: "" },
    } : null;
    if (command === "take_startup_queue_error") {
      startupState.reads += 1;
      startupState.wiredAtRead = listeners.has("queue-error");
      if (mockWindowLabel !== "main" || startupState.delivered) return null;
      startupState.delivered = startupQueueError !== null;
      return startupQueueError;
    }
    if (command === "take_startup_app_error") {
      if (mockWindowLabel !== "main" || startupAppErrorDelivered) return null;
      startupAppErrorDelivered = startupAppError !== null;
      return startupAppError;
    }
    if (command === "new_app_window") {
      newWindowCalls.push(structuredClone(args));
      return `window-mock-${newWindowCalls.length}`;
    }
    if (command === "desktop_preferences") {
      return { preferences: structuredClone(desktopPreferences), revision: desktopRevision, needsMigration: !desktopInitialized, loadError: desktopLoadError };
    }
    if (command === "sync_native_theme") return null;
    if (command === "update_desktop_preferences") {
      if (desktopLoadError) throw desktopLoadError;
      if (args.onlyIfUninitialized && desktopInitialized) {
        return { preferences: structuredClone(desktopPreferences), revision: desktopRevision, needsMigration: false, loadError: null };
      }
      const changes = Object.fromEntries(Object.entries(args.patch).filter(([, value]) => value !== null));
      desktopPreferences = { ...desktopPreferences, ...changes };
      desktopRevision += 1;
      desktopInitialized = true;
      const snapshot = { preferences: structuredClone(desktopPreferences), revision: desktopRevision, needsMigration: false, loadError: null };
      listeners.get("desktop-preferences-changed")?.({ payload: snapshot });
      return snapshot;
    }
    if (command === "remember_window") {
      rememberedWindows.push(structuredClone(args));
      if (mockParams.has("restoreTabs")) {
        sessionStorage.setItem("doppeldecker.mock.tabs", JSON.stringify(args.tabs));
        sessionStorage.setItem("doppeldecker.mock.activeSide", args.activeSide);
      }
      return null;
    }
    if (command === "restored_pane_tabs") return mockParams.has("restoreTabs")
      ? JSON.parse(sessionStorage.getItem("doppeldecker.mock.tabs") || "null") : null;
    if (command === "restored_active_side") return (mockParams.has("restoreTabs")
      ? sessionStorage.getItem("doppeldecker.mock.activeSide") : null) || mockParams.get("restoredSide") || "left";
    if (command === "diff_tool_status") return [
      { id: "xcode", available: true, detail: "Xcode FileMerge ist verfügbar." },
      { id: "fastra", available: false, detail: "Fastras Helfer fehlt noch." },
      { id: "bbedit", available: true, detail: "BBEdit bbdiff ist verfügbar." },
    ];
    if (command === "compare_files") {
      diffCalls.push(structuredClone(args.request));
      const limitations = mockParams.has("diffBestEffort")
        ? ["SFTP-Inhalte · Best Effort: pfadbasiertes Öffnungsrisiko bleibt; keine atomare Rootbindung."] : [];
      window.__MOCK_EMIT__("file-diff-started", { limitations });
      if (mockParams.has("diffHold")) {
        await new Promise(resolve => { diffPendingResolve = resolve; });
        return null;
      }
      if (mockParams.has("diffFailure")) {
        window.__MOCK_EMIT__("file-diff-error", { code: "outcome_unclear",
          message: "Helferübergabe unbestätigt; ein Vergleichsfenster kann bereits geöffnet sein." });
      } else {
        window.__MOCK_EMIT__("file-diff-done", { limitations });
        if (mockParams.has("diffNextPreparation")) {
          window.__MOCK_EMIT__("fileop-preparation-started", "Folgende Queuevorbereitung bleibt sichtbar.");
        }
      }
      return null;
    }
    if (command === "cli_install_status") {
      if (cliStatusError) throw { code: "io", message: "CLI-Status konnte nicht gelesen werden" };
      return { state: cliOffer ? "offer" : "unavailable" };
    }
    if (command === "install_cli") {
      document.documentElement.dataset.cliInstalled = "true";
      return null;
    }
    if (command === "decline_cli_install") {
      if (cliDeclineError) {
        throw { code: "io", message: "CLI-Entscheidung konnte nicht gespeichert werden" };
      }
      document.documentElement.dataset.cliDeclined = "true";
      return null;
    }
    if (command === "default_dirs") {
      defaultDirCalls += 1;
      if (defaultDelayMs) await wait(defaultDelayMs);
      if (defaultErrorAfterStartup && defaultDirCalls > 1) {
        throw { code: "io", message: "Mock-Standardordner nicht erreichbar" };
      }
      return demoMode
        ? { left: demoLeftRoot, right: demoRightRoot }
        : { left: "/mock/quelle", right: "/mock/ziel" };
    }
    if (command === "list_system_mounts") return structuredClone(systemMounts);
    if (command === "discover_network_services") {
      discoveryCalls += 1;
      return structuredClone(discoveredServices);
    }
    if (command === "eject_system_mount") {
      mountEjectCalls.push(args.id);
      systemMounts = systemMounts.filter(mount => mount.id !== args.id);
      return structuredClone(systemMounts);
    }
    if (command === "cancel_directory_sizes") {
      if (cancelSizeDelayMs) await wait(cancelSizeDelayMs);
      directorySizeCancels.push({ side: args.side });
      const active = activeDirectorySizeRuns.get(args.side);
      if (active) active.cancelled = true;
      activeDirectorySizeRuns.delete(args.side);
      return null;
    }
    if (command === "start_directory_sizes") {
      const request = structuredClone(args.request);
      const endpoint = request?.endpoint;
      const remote = ["sftp", "ftps", "webdav", "smb"].includes(endpoint?.provider);
      if (!["left", "right"].includes(request?.side) || !acceptsCommandArgument(endpoint?.root, "String")
        || (!remote && endpoint.provider !== "local")
        || (remote && (!acceptsCommandArgument(endpoint.profileId, "String")
          || (endpoint.profileRevision != null && !acceptsCommandArgument(endpoint.profileRevision, "u64"))))
        || (request.sessionGeneration != null && !acceptsCommandArgument(request.sessionGeneration, "u64"))
        || !acceptsCommandArgument(request.names, "Vec<String>")
        || !acceptsCommandArgument(request.recalculate, "bool") || !acceptsCommandArgument(request.clientToken, "u64")) {
        throw { code: "invalid_input", message: "Ungültige Ordnergrößen-Anfrage" };
      }
      const runId = ++directorySizeSequence;
      const run = { runId, cancelled: false, request };
      const previous = activeDirectorySizeRuns.get(request.side);
      if (previous) previous.cancelled = true;
      activeDirectorySizeRuns.set(request.side, run);
      directorySizeCalls.push(request);
      void (async () => {
        if (directorySizeDelayMs) await wait(directorySizeDelayMs);
        if (remote) {
          try {
            requireProfileRevision(endpoint.provider, endpoint.profileId, endpoint.profileRevision);
            // Die Mock-Listings verwenden für verbundene Profile Generation 1.
            if (!activeRemote.has(`${endpoint.provider}:${endpoint.profileId}`) || request.sessionGeneration !== 1) {
              throw { code: "plan_drift", message: "Die Remote-Sitzung ist nicht mehr aktuell" };
            }
          } catch (error) {
            if (activeDirectorySizeRuns.get(request.side) !== run) return;
            activeDirectorySizeRuns.delete(request.side);
            listeners.get("directory-size-error")?.({ payload: {
              side: request.side, run_id: runId, client_token: request.clientToken,
              parent: endpoint.root, error,
            } });
            return;
          }
        }
        // Der Mock sendet selbst nach einem Abbruch noch ein spätes Resultat. Nur so
        // belegt die UI-Suite, dass das Frontend-Token Navigation wirklich absichert.
        let completeCount = 0;
        let observedCount = 0;
        let partialCount = 0;
        for (const name of request.names) {
          const partial = name === "Unvollständig";
          const observed = mockParams.has("directorySizeObserved");
          const bytes = name === "Bilder" ? 4096
            : name === "Klein" ? 1000
              : name === "Groß" ? 9000
                : partial ? 200 : 8192;
          if (partial) partialCount += 1;
          else if (observed) observedCount += 1;
          else completeCount += 1;
          listeners.get("directory-size-result")?.({ payload: {
            side: request.side,
            run_id: runId,
            client_token: request.clientToken,
            parent: request.endpoint.root,
            name,
            result: {
              path: request.endpoint.root === "/" ? `/${name}` : `${request.endpoint.root}/${name}`,
              bytes,
              files: name === "Bilder" ? 2 : 4,
              directories: 1,
              skipped: 0,
              status: partial ? "partial" : observed ? "observed" : "complete",
              issues: partial ? [{
                path: `${request.endpoint.root}/${name}/gesperrt`,
                entry_name: null,
                kind: "listing",
                code: "permission_denied",
                message: "Ein Unterordner konnte nicht gelesen werden",
              }] : [],
              cached: observed ? false : !request.recalculate,
              limitations: observed ? ["SFTP-Ordnergröße · Best Effort: beobachteter Bestand; höchstens 1000 Einträge und 16 Unterordnerebenen. Kein Dateiinhalt wird geöffnet."] : [],
            },
          } });
        }
        listeners.get("directory-size-done")?.({ payload: {
          side: request.side,
          run_id: runId,
          client_token: request.clientToken,
          parent: request.endpoint.root,
          stats: {
            requested: request.names.length,
            finished: request.names.length,
            complete: completeCount,
            observed: observedCount,
            partial: partialCount,
            failed: 0,
            cancelled: run.cancelled ? request.names.length : 0,
            cached: mockParams.has("directorySizeObserved") || request.recalculate ? 0 : request.names.length,
          },
        } });
        if (activeDirectorySizeRuns.get(request.side) === run) {
          activeDirectorySizeRuns.delete(request.side);
        }
      })();
      return { run_id: runId };
    }
    if (command === "scan_dir") {
      scanCalls.push({ command, path: args.path });
      // Nur der explizite Langsam-Pfad wird verzögert. So kann ein Test eine alte
      // Antwort gezielt hinter eine neuere Antwort schieben, ohne den Start zu bremsen.
      if (args.path === "/mock/slow" && scanDelayMs) await wait(scanDelayMs);
      if (scanErrorName && args.path.endsWith(`/${scanErrorName}`)) {
        throw { code: "io", message: `Mock-Scanfehler für ${args.path}` };
      }
      const samePathCalls = scanCalls.filter(call => call.path === args.path).length;
      if (scanErrorOnSecond && args.path.endsWith(`/${scanErrorOnSecond}`)
          && samePathCalls === 2) {
        throw { code: "io", message: `Zweiter Mock-Scanfehler für ${args.path}` };
      }
      const entries = demoMode && args.path === demoLeftRoot
        ? demoSource
        : demoMode && args.path === demoRightRoot
        ? demoTarget
        : args.path === "/mock/quelle"
        ? sourceEntries
        : args.path === "/mock/slow"
          ? [{ name: "Langsam.txt", is_dir: false, is_file: true, is_symlink: false,
              size: 7, mtime: 4, mode: 420 }]
          : args.path === "/mock/fast"
            ? [{ name: "Schnell.txt", is_dir: false, is_file: true, is_symlink: false,
                size: 8, mtime: 5, mode: 420 }]
            : [];
      const present = entries.filter(entry => {
        const fullPath = args.path === "/" ? `/${entry.name}` : `${args.path}/${entry.name}`;
        return !trashedPaths.has(fullPath);
      });
      const displayEntries = present.map(entry => ({
        ...wireDirectoryEntry(entry),
        safely_decoded: entry.safely_decoded ?? true,
        is_hidden: entry.is_hidden ?? entry.name.startsWith("."),
      }));
      return {
        path: args.path,
        parent: args.path === "/" ? null : args.path.slice(0, args.path.lastIndexOf("/")) || "/",
        entries: args.showHidden ? displayEntries : displayEntries.filter(entry => !entry.is_hidden),
      };
    }
    if (["connect_sftp_profile", "connect_ftps_profile", "connect_webdav_profile", "connect_smb_profile"].includes(command)) {
      const protocol = command.split("_")[1];
      const profile = requireProfileRevision(protocol, args.profileId, args.profileRevision);
      connectionActions.push({ command, profileId: args.profileId,
        profileRevision: args.profileRevision });
      const identityFor = { sftp: sftpIdentity, ftps: ftpsIdentity, webdav: webdavIdentity, smb: smbIdentity };
      const identity = identityFor[protocol]("/").identity.server;
      const connection = profile.connection;
      identity.canonical_root = connection.requested_root;
      if (protocol === "webdav") identity.origin = connection.origin;
      else {
        identity.host = connection.host;
        identity.port = connection.port;
      }
      if (protocol === "sftp") identity.host_key = connection.host_key;
      if (protocol === "ftps") identity.transport_security = connection.transport_security;
      if (protocol === "ftps" || protocol === "webdav") {
        if (connection.certificate) identity.certificate = connection.certificate;
        else delete identity.certificate;
      }
      if (protocol === "smb") {
        identity.server_guid = connection.server_guid;
        identity.share = connection.share;
      }
      return {
        profile_revision: profile.revision,
        identity,
        capabilities: {
          complete_listing: false, exclusive_directory_create: false, exclusive_create: false,
          atomic_noreplace_rename: false, best_effort_rename: false, best_effort_file_commit: false,
          best_effort_file_replace: false, atomic_replace_rename: false, durable_file_flush: false,
          verified_file_commit: false, durable_directory_flush: false, quarantine: false,
          entry_delete: false, permanent_delete: false, available_space: false,
        },
        quarantine_capabilities: { listing: protocol === "smb", restore: false, purge: false },
        ...(protocol === "ftps" ? { security: ftpSecurityStatus(profile.connection) } : {}),
      };
    }
    if (["scan_sftp_dir", "scan_ftps_dir", "scan_webdav_dir", "scan_smb_dir"].includes(command)) {
      const protocol = command.split("_")[1];
      const profile = requireProfileRevision(protocol, args.profileId, args.profileRevision);
      scanCalls.push({ command, profileId: args.profileId,
        profileRevision: args.profileRevision, path: args.path });
      if (remoteScanError) {
        throw { code: "io", message: `Mock-Remote-Scanfehler für ${args.path}` };
      }
      // Wie im Backend entsteht die Sitzung lazy mit dem ersten Listing.
      if (protocol === "sftp" && mockParams.has("holdSftpScan")) await holdRequest("holdSftpScan", {
        profileId: args.profileId, profileRevision: args.profileRevision, path: args.path,
      });
      if (protocol === "sftp" && sftpScanDelayMs) await wait(sftpScanDelayMs);
      activeRemote.add(`${protocol}:${args.profileId}`);
      const security = ftpSecurityStatus(profile.connection);
      const omitted = protocol === "sftp" && mockParams.has("listingOmissions") && args.path === "/"
        ? mockParams.has("manyOmissions")
          ? Array.from({ length: 4000 }, (_, index) => ({ display_name: `Eintrag-${index}`, is_hidden: false, reason: "Dateityp nicht belegt" }))
          : [{ display_name: "Zeile\\nneu.txt", is_hidden: false, reason: "Kein sicher bedienbarer einzelner Dateiname" },
             { display_name: "<img src=x onerror=alert(1)>", is_hidden: false, reason: "SFTP-Listing liefert keinen belegten Dateityp" }]
        : [];
      const entries = (args.path === "/" || (protocol === "ftps" && (mockParams.has("ftpsPreparationGate") || mockParams.has("ftpsEnqueuePreparationGate") || mockParams.has("ftpsConflictChoice")) && args.path === "/from")) ? (protocol === "sftp" && mockParams.has("listingOmissions")
          ? (mockParams.has("onlyOmissions") ? [] : [{ name: "nutzbar.txt", is_dir: false, is_file: true, is_symlink: false, size: 7, mtime: 1 }])
          : protocol === "sftp" && mockParams.has("unknownSizes")
          ? [
            ...(mockParams.has("directorySizeObserved") ? [{ name: "remote", is_dir: true, is_file: false, is_symlink: false, size: 0, mtime: 1 }] : []),
            { name: "Unbekannt.txt", is_dir: false, is_file: true, is_symlink: false, size: null, mtime: 1 },
            { name: "Leer.txt", is_dir: false, is_file: true, is_symlink: false, size: 0, mtime: 1 },
            { name: "Bekannt.txt", is_dir: false, is_file: true, is_symlink: false, size: 5, mtime: 1 },
            { name: "Link.txt", is_dir: false, is_file: false, is_symlink: true, size: 0, mtime: 1 },
          ]
          : mockParams.has("sftpTree")
          ? [{ name: "Baum", is_dir: true, is_file: false, is_symlink: false, size: 0, mtime: 1, mode: 448 }]
          : mockParams.has("remoteFile")
          ? [{ name: "remote.txt", is_dir: false, is_file: true,
              is_symlink: false, size: 8, mtime: 1, mode: 420 }]
          : [{ name: "remote", is_dir: true, is_file: false,
              is_symlink: false, size: 0, mtime: 1,
              mode: remoteMissingMode ? null : 493 }]) : [];
      return {
        path: args.path,
        parent: args.path === "/" ? null : args.path.slice(0, args.path.lastIndexOf("/")) || "/",
        entries: entries.filter(entry => args.showHidden || !entry.name.startsWith(".")).map(wireDirectoryEntry),
        profile_revision: profile.revision,
        session_generation: 1,
        ...(protocol === "ftps" ? { security }
          : protocol === "sftp" ? { security: "sftp_path_best_effort", omitted } : {}),
      };
    }
    if (command === "list_active_connections" || command === "disconnect_remote_profile") {
      if (command === "disconnect_remote_profile") {
        requireProfileRevision(args.protocol, args.profileId, args.profileRevision);
        connectionActions.push({ command, protocol: args.protocol,
          profileId: args.profileId, profileRevision: args.profileRevision });
        activeRemote.delete(`${args.protocol}:${args.profileId}`);
      }
      return [...activeRemote].flatMap(key => {
        const [protocol, profile_id] = key.split(":");
        const profile = profilesFor(protocol).find(candidate => candidate.id === profile_id);
        return profile ? [{ protocol, profile_id, revision: profile.revision }] : [];
      });
    }
    if (command === "open_path") {
      window.__DOPPELDECKER_LAST_OPENED__ = args.path;
      document.documentElement.dataset.lastOpened = args.path;
      return null;
    }
    if (command === "open_project_page") {
      window.__MOCK_PROJECT_PAGE__ = args.page;
      return null;
    }
    if (command === "rename_path") {
      if (renameDelayMs) await wait(renameDelayMs);
      const separator = args.path.lastIndexOf("/");
      const parent = separator <= 0 ? "/" : args.path.slice(0, separator);
      const oldName = args.path.slice(separator + 1);
      const entries = parent === "/mock/quelle" ? sourceEntries : null;
      const entry = entries?.find(candidate => candidate.name === oldName);
      if (!entry) throw { code: "not_found", message: "Mock-Rename-Quelle fehlt" };
      if (entries.some(candidate => candidate.name === args.newName)) {
        throw { code: "already_exists", message: "Mock-Rename-Ziel ist belegt" };
      }
      entry.name = args.newName;
      const result = `${parent}/${args.newName}`;
      document.documentElement.dataset.lastLocalRename = result;
      actionOrder.push(command);
      return result;
    }
    if (command === "list_remote_edits") return window.__MOCK_REMOTE_EDITS__ || [];
    if (command === "save_remote_edit") {
      const entry = (window.__MOCK_REMOTE_EDITS__ || []).find(entry => entry.id === args.request.id);
      if (!entry) throw { code: "not_found", message: "Bearbeitung nicht gefunden" };
      if (mockParams.has("remoteEditDrift")) throw { code: "plan_drift", message: "Serverdatei wurde inzwischen verändert. Bearbeitung neu öffnen." };
      entry.lastOperationId = "mock-remote-edit-save";
      entry.dirty = false;
      window.__REMOTE_EDIT_SAVE_REQUEST__ = structuredClone(args.request);
      return entry.lastOperationId;
    }
    if (command === "close_remote_edit") {
      window.__MOCK_REMOTE_EDITS__ = (window.__MOCK_REMOTE_EDITS__ || []).filter(entry => entry.id !== args.request.id);
      return null;
    }
    if (command === "open_remote_snapshot") {
      const cancelsBefore = cancelSyncCalls;
      const limitations = args.request.endpoint.provider === "sftp" && mockParams.has("sftpContents")
        ? ["SFTP-Inhalte · Best Effort: Das erste Öffnen ist nicht atomar an den Pane-Root gebunden; unbemerkter Pfadtausch bleibt möglich. Reguläre Einzeldateien bis 8 GiB, keine Rekursion oder Ersetzung."] : [];
      window.__MOCK_EMIT__("remote-snapshot-started", { limitations });
      await wait(Number(mockParams.get("snapshotDelay") || "0"));
      if (cancelSyncCalls !== cancelsBefore) {
        window.__MOCK_EMIT__("remote-snapshot-error", { code: "interrupted", message: "Mock: abgebrochen" });
      } else if (mockParams.has("snapshotError")) {
        window.__MOCK_EMIT__("remote-snapshot-error", { code: "io", message: "Mock: Downloadfehler" });
      } else {
        window.__DOPPELDECKER_LAST_REMOTE_OPENED__ = args.request.path;
        document.documentElement.dataset.lastRemoteOpened = args.request.path;
        const name = args.request.path.split("/").pop();
        const editId = args.request.mode === "edit" ? "mock-remote-edit" : null;
        window.__LAST_REMOTE_OPEN_MODE__ = args.request.mode;
        if (editId) window.__MOCK_REMOTE_EDITS__ = [{ id: editId, name, endpointLabel: `${args.request.endpoint.provider.toUpperCase()} · ${args.request.path}`, dirty: true, lastOperationId: null, limitations: ["Pfadbindung als Best Effort"], replaceBestEffort: true, saveAvailable: true, saveBlocker: null }];
        window.__MOCK_EMIT__("remote-snapshot-done", { name, bytes: 8, limitations, editId });
      }
      if (mockParams.has("snapshotFollowup")) {
        window.__MOCK_EMIT__("fileop-progress", { phase: "copy", files_done: 0, files_total: 1, bytes_done: 0, bytes_total: 50, current: "Nachfolgelauf" });
      }
      return null;
    }
    if (command === "provider_rename_contract") {
      actionOrder.push(command);
      if (mockParams.has("renameContractDelay")) await wait(Number(mockParams.get("renameContractDelay")));
      if (mockParams.has("renameBlocked")) return { kind: "blocked", blockers: ["Mock: Elternbindung fehlt"] };
      if (mockParams.has("renameConfirmed")) return { kind: "confirmed", execution_class: "best_effort", limitations: ["Best Effort: bedingtes MOVE ohne künstliche Löschprobe; keine dauerhafte Speicherung bewiesen. Eine einzelne WebDAV-If-Bedingung bindet den unmittelbaren Elternordner. Vorfahren werden vor und nach der Wirkung geprüft; ein später Vorfahrenwechsel kann bereits wirken und erst im Readback als unklar auffallen."] };
      return { kind: "existing" };
    }
    if (command === "rename_provider_path") {
      const request = args.request;
      if (renameDelayMs) await wait(renameDelayMs);
      const parent = request.path.slice(0, request.path.lastIndexOf("/")) || "/";
      const path = parent === "/" ? `/${request.newName}` : `${parent}/${request.newName}`;
      // Der echte Command liefert ein eindeutiges Resultat samt Ausführungsklasse.
      // "kollision.txt" simuliert einen bereits belegten Zielnamen ohne Mutation.
      if (request.newName === "kollision.txt") {
        return {
          outcome: "collision",
          class: "best_effort",
          limitations: ["Der Server ersetzt bei RNTO vorhandene Ziele."],
          newPath: null,
          existingIsDir: false,
          verifiedAfterReconnect: false,
        };
      }
      document.documentElement.dataset.lastRemoteRename = JSON.stringify({
        endpoint: request.endpoint,
        path: request.path,
        newName: request.newName,
        expectedExecutionClass: request.expectedExecutionClass,
        result: path,
      });
      if (mockParams.has("renameOutcomeUnclear")) {
        throw { code: "outcome_unclear", message: "Mock: Rename-Ausgang unklar" };
      }
      // Auch die abgesicherte Klasse trägt eine Einschränkung: Sie schützt das Ziel,
      // die Quelle bleibt allein über ihren Pfad gebunden (siehe
      // core/src/remote_real_mode.rs). "abgesichert.txt" liefert genau diesen Fall.
      const assured = request.newName === "abgesichert.txt";
      return {
        outcome: "renamed",
        class: assured ? "assured" : "best_effort",
        limitations: [assured
          ? "Die Quelle ist ausschließlich über ihren Pfad gebunden."
          : "Der Server ersetzt bei RNTO vorhandene Ziele."],
        newPath: path,
        existingIsDir: null,
        verifiedAfterReconnect: false,
      };
    }
    if (command === "plan_sync") return structuredClone(window.__MOCK_LOCAL_SYNC_WIRE__?.plan ?? plan);
    if (["register_bidirectional_pair", "register_forward_bidirectional_pair"].includes(command)) {
      const endpointKey = endpoint =>
        `${endpoint.provider}\u0000${endpoint.profileId || ""}\u0000${endpoint.root}`;
      const ordered = [structuredClone(args.left), structuredClone(args.right)]
        .sort((left, right) => endpointKey(left).localeCompare(endpointKey(right)));
      const key = `${endpointKey(ordered[0])}\u0001${endpointKey(ordered[1])}`;
      if (!bidirectionalPairs.has(key)) {
        bidirectionalPairs.set(key, Array(16).fill(bidirectionalPairs.size + 6));
      }
      const name = endpoint => endpoint.root.split("/").filter(Boolean).at(-1)
        || (endpoint === ordered[0] ? "Links" : "Rechts");
      preparationActions.push({ command, args: structuredClone(args) });
      const pair = {
        pairId: structuredClone(bidirectionalPairs.get(key)),
        pairName: `${name(ordered[0])} ↔ ${name(ordered[1])}`,
        left: ordered[0],
        right: ordered[1],
        visibleLeftRole: endpointKey(args.left) === endpointKey(ordered[0]) ? "left" : "right",
      };
      if (command === "register_forward_bidirectional_pair"
          && (mockParams.has("forwardPairRecovery") || mockParams.has("forwardPairCompletion"))) {
        const pairKey = JSON.stringify(pair.pairId);
        if (!forwardStatuses.has(pairKey)) {
          const { plan } = forwardPreview({ ...pair, resolutions: [] });
          forwardStatuses.set(pairKey, forwardPairStatus(plan, mockParams.has("forwardPairCompletion"), 12));
        }
      }
      return pair;
    }
    if (command === "forward_bidirectional_pair_status") {
      forwardCalls.push({ command, args: structuredClone(args) });
      return structuredClone(forwardStatuses.get(JSON.stringify(args.pairId)) || null);
    }
    if (command === "prepare_forward_bidirectional_sync") {
      forwardCalls.push({ command, args: structuredClone(args) });
      const before = cancelSyncCalls;
      window.__MOCK_EMIT__("sync-preparation-started", "Beide Seiten frisch prüfen");
      if (mockParams.has("forwardPrepareDelay")) await wait(200);
      window.__MOCK_EMIT__("sync-preparation-done", null);
      if (before !== cancelSyncCalls) throw { code: "interrupted", message: "Bidirektionale Vorbereitung abgebrochen" };
      if (pendingForward) throw { code: "busy", message: "Forwardvorbereitung noch belegt" };
      pendingForward = { request: structuredClone(args.request), preview: forwardPreview(args.request) };
      return structuredClone(pendingForward.preview);
    }
    if (command === "discard_forward_bidirectional_sync") {
      forwardCalls.push({ command }); pendingForward = null; return null;
    }
    if (command === "preview_forward_bidirectional_conflicts") {
      forwardCalls.push({ command, args: structuredClone(args) });
      const preview = args.operationId ? forwardPreviews.get(args.operationId) : pendingForward?.preview;
      if (!preview || JSON.stringify(args.expectedUnresolvedDigest) !== JSON.stringify(preview.unresolved_digest)) {
        throw { code: "plan_drift", message: "Mock: gebundene Konfliktvorschau fehlt" };
      }
      const decided = new Set(args.resolutions.map(r => JSON.stringify(r.path)));
      return structuredClone(preview.conflict_details.filter(c => !decided.has(JSON.stringify(c.path))));
    }
    if (command === "enqueue_forward_bidirectional_sync") {
      forwardCalls.push({ command, args: structuredClone(args) });
      if (!pendingForward || !args.effectsConfirmed || JSON.stringify(args.preparationDigest) !== JSON.stringify(pendingForward.preview.digest)) throw { code: "plan_drift", message: "Frische Forwardwirkung fehlt" };
      if (mockParams.has("forwardEnqueueFailsOnce") && forwardCalls.filter(c => c.command === command).length === 1) throw { code: "busy", message: "Forward-Einreihen vor Commit belegt" };
      const { request, preview } = pendingForward;
      const entry = queueEntry({ type: "BidirectionalForward", pair_id: request.pairId, pair_name: request.pairName,
        left: queueEndpoint(request.left), right: queueEndpoint(request.right), baseline_path: "/mock/state/forward-pair.json",
        resolutions: structuredClone(request.resolutions), expected_baseline_digest: preview.plan.baseline_digest,
        expected_plan_digest: preview.plan.plan_digest, expected_unresolved_digest: preview.unresolved_digest,
        expected_preview_digest: preview.digest });
      forwardRequests.set(entry.operation_id, structuredClone(request)); forwardPreviews.set(entry.operation_id, structuredClone(preview));
      queue.entries.push(entry); pendingForward = null;
      if (mockParams.has("forwardReopen")) {
        entry.state = "AwaitingConfirmation"; entry.attention = { type: "RemoteRun", reason: "Nach Neustart frisch geprüft" };
        const fresh = forwardPreviews.get(entry.operation_id); fresh.digest = Array(32).fill(44);
        fresh.limitations.push("Neue Wirkung nach Neustart: frischer Digest."); queueEvent();
      } else if (mockParams.has("forwardPartial")) {
        entry.state = "Interrupted"; entry.attempts = 1; entry.recovery_available = true; entry.error_code = "outcome_unclear";
        entry.error = "Erste von drei Dateien committed; gemeinsamer Stand B0 bleibt erhalten.";
        forwardStatuses.set(JSON.stringify(request.pairId), forwardPairStatus(preview.plan, false, 13));
        queueEvent();
        window.__MOCK_EMIT__("bidirectional-error", { operationId: entry.operation_id, attempt: 1, code: "outcome_unclear", message: entry.error });
      } else finishForwardMock(entry);
      return entry.operation_id;
    }
    if (command === "forward_bidirectional_queue_preview") { forwardCalls.push({ command, args: structuredClone(args) }); return structuredClone(forwardPreviews.get(args.operationId)); }
    if (command === "prepare_forward_bidirectional_queue_recovery") {
      forwardCalls.push({ command, args: structuredClone(args) });
      const entry = queue.entries.find(e => e.operation_id === args.operationId);
      const previous = forwardPreviews.get(args.operationId);
      if (args.expectedUnresolvedDigest != null
        ? JSON.stringify(args.expectedUnresolvedDigest) !== JSON.stringify(previous?.unresolved_digest)
        : args.resolutions.length !== 0) {
        throw { code: "plan_drift", message: "Mock: Neuabgleich braucht den gebundenen Konfliktdigest" };
      }
      const request = { ...forwardRequestForEntry(entry), resolutions: args.resolutions, recovery: true };
      const preview = forwardPreview(request); preview.digest = Array(32).fill(args.resolutions.length ? 46 : 45);
      forwardPreviews.set(entry.operation_id, preview); entry.state = "AwaitingConfirmation"; entry.attention = { type: "RemoteRun", reason: "Neu gegen B0 abgeglichen" }; queueEvent(); return structuredClone(preview);
    }
    if (command === "discard_forward_bidirectional_queue_preparation") {
      forwardCalls.push({ command, args: structuredClone(args) }); forwardPreviews.delete(args.operationId);
      const entry = queue.entries.find(e => e.operation_id === args.operationId); entry.state = "Interrupted"; entry.attention = null; entry.recovery_available = true; queueEvent(); return null;
    }
    if (command === "approve_forward_bidirectional_queue_entry") {
      forwardCalls.push({ command, args: structuredClone(args) });
      const entry = queue.entries.find(e => e.operation_id === args.operationId); const preview = forwardPreviews.get(args.operationId);
      if (!args.effectsConfirmed || JSON.stringify(args.preparationDigest) !== JSON.stringify(preview.digest)) throw { code: "plan_drift", message: "Neuer Forwarddigest muss bestätigt werden" };
      finishForwardMock(entry); return structuredClone(queue);
    }
    if (["finish_forward_bidirectional_pair_completion", "finish_forward_bidirectional_queue_completion"].includes(command)) {
      forwardCalls.push({ command, args: structuredClone(args) });
      if (command === "finish_forward_bidirectional_pair_completion") {
        forwardStatuses.delete(JSON.stringify(args.pairId));
        return null;
      }
      const entry = queue.entries.find(e => e.operation_id === args.operationId);
      entry.state = "Completed";
      entry.bidirectional_completion_recovery = null;
      queueEvent();
      return structuredClone(queue);
    }
    if (command === "prepare_bidirectional_sync") {
      preparationActions.push({ command, args: structuredClone(args) });
      if (pendingBidirectional) {
        throw { code: "busy", message: "Bidirektionale Mock-Vorbereitung ist noch belegt" };
      }
      pendingBidirectional = { request: structuredClone(args.request),
        preview: bidirectionalPreview(args.request) };
      return structuredClone(pendingBidirectional.preview);
    }
    if (command === "discard_bidirectional_sync") {
      preparationActions.push({ command }); pendingBidirectional = null; return null;
    }
    if (command === "prepare_bidirectional_resume") {
      const entry = queue.entries.find(item => item.operation_id === args.operationId);
      const recoveryPreparationId = `mock-recovery-${++bidirectionalRecoveryPreparationSequence}`;
      pendingBidirectional = { recoveryId: args.operationId, recoveryPreparationId,
        preview: { ...bidirectionalPreview({ pairId: entry.operation.pair_id,
          pairName: entry.operation.pair_name, resolutions: [],
          left: { provider: entry.operation.left.provider.toLowerCase(), root: entry.operation.left.root },
          right: { provider: entry.operation.right.provider.toLowerCase(), root: entry.operation.right.root } }),
          recovery_preparation_id: recoveryPreparationId,
          recovery_kind: "resume" } };
      if (bidirectionalRecoveryStageBlocker) {
        pendingBidirectional.preview.stage_blocker = "Mock-Recovery ist gesperrt";
      }
      if (bidirectionalRecoveryBackupRequired) {
        pendingBidirectional.preview.backup_confirmation_required = true;
      }
      return structuredClone(pendingBidirectional.preview);
    }
    if (command === "prepare_bidirectional_rollback") {
      const recoveryPreparationId = `mock-recovery-${++bidirectionalRecoveryPreparationSequence}`;
      pendingBidirectionalRollback = { recoveryId: args.operationId, recoveryPreparationId,
        preview: { pair_id: Array(16).fill(4), committed_projections: 2,
          confirmation_phrase: "ROLLBACK Recovery-Paar 0011223344556677",
          expires_at_unix_ms: Date.now() + 600000,
          recovery_preparation_id: recoveryPreparationId,
          recovery_kind: "rollback" } };
      return structuredClone(pendingBidirectionalRollback.preview);
    }
    if (command === "discard_bidirectional_recovery_preparation") {
      preparationActions.push({ command, args: structuredClone(args) });
      bidirectionalRecoveryDiscardAttempts += 1;
      if (bidirectionalRecoveryDiscardAttempts <= bidirectionalRecoveryDiscardBeforeDeliveryFailures) {
        throw { code: "ipc", message: "Mock-Recovery-Discard scheiterte vor Zustellung" };
      }
      const pending = args.recoveryKind === "rollback"
        ? pendingBidirectionalRollback
        : pendingBidirectional;
      if (pending == null) return { absent: true };
      if (pending.recoveryId !== args.operationId
        || pending.recoveryPreparationId !== args.recoveryPreparationId) {
        throw { code: "conflict", message: "Eine andere Recovery-Vorbereitung ist aktiv" };
      }
      if (args.recoveryKind === "rollback") pendingBidirectionalRollback = null;
      else pendingBidirectional = null;
      if (bidirectionalRecoveryDiscardActsThenFailsOnce && bidirectionalRecoveryDiscardAttempts === 1) {
        throw { code: "ipc", message: "Mock-Recovery-Discard wirkte, Antwort ging verloren" };
      }
      return { absent: true };
    }
    if (command === "preview_bidirectional_conflicts") {
      if (!pendingBidirectional || JSON.stringify(args.expectedPlanDigest) !== JSON.stringify(pendingBidirectional.preview.plan.plan_digest)) {
        throw { code: "plan_drift", message: "Mock: gebundene Konfliktvorschau fehlt" };
      }
      const decided = new Set(args.resolutions.map(resolution => JSON.stringify(resolution.path)));
      return structuredClone(pendingBidirectional.preview.conflict_details.filter(conflict => !decided.has(JSON.stringify(conflict.path))));
    }
    if (command === "enqueue_bidirectional_sync") {
      preparationActions.push({ command });
      const attempts = preparationActions.filter(action => action.command === command).length;
      if (bidirectionalEnqueueFailsOnce && attempts === 1) {
        throw { code: "busy", message: "Bidirektionales Mock-Enqueue ist vor dem Commit belegt" };
      }
      const request = pendingBidirectional.request;
      const entry = queueEntry({ type: "Bidirectional", pair_id: request.pairId,
        pair_name: request.pairName, left: queueEndpoint(request.left), right: queueEndpoint(request.right),
        baseline_path: "/mock/state/paar.json", resolutions: structuredClone(request.resolutions),
        expected_baseline_digest: null,
        expected_plan_digest: structuredClone(pendingBidirectional.preview.plan.plan_digest) });
      queue.entries.push(entry); pendingBidirectional = null; queueEvent();
      return structuredClone(queue);
    }
    if (command === "run_bidirectional_sync") {
      preparationActions.push({ command });
      const attempts = preparationActions.filter(action => action.command === command).length;
      if (bidirectionalRunFailsOnce && attempts === 1) {
        throw { code: "busy", message: "Bidirektionaler Mock-Start ist vor der Entnahme belegt" };
      }
      if (args.effectsConfirmed !== true) throw { code: "confirmation_mismatch", message: "Wirkung nicht bestätigt" };
      document.documentElement.dataset.bidirectionalConfirmation = String(args.effectsConfirmed);
      if (bidirectionalRunDelayMs) await wait(bidirectionalRunDelayMs);
      const recoveryId = pendingBidirectional?.recoveryId;
      const directRequest = pendingBidirectional?.request;
      pendingBidirectional = null;
      if (recoveryId) {
        const entry = queue.entries.find(item => item.operation_id === recoveryId);
        if (entry) {
          entry.state = "Completed"; entry.error = null; entry.recovery_available = false;
          entry.bidirectional_completion_recovery = null;
        }
        queueEvent();
      } else if (directRequest) {
        const entry = queueEntry({ type: "Bidirectional", pair_id: directRequest.pairId,
          pair_name: directRequest.pairName,
          left: queueEndpoint(directRequest.left), right: queueEndpoint(directRequest.right),
          baseline_path: "/mock/state/direct-paar.json", resolutions: structuredClone(directRequest.resolutions),
          expected_baseline_digest: null, expected_plan_digest: Array(32).fill(3) });
        entry.attempts = 1;
        if (bidirectionalDirectJournalFailure) {
          entry.state = "Interrupted";
          entry.error = "Direkter Mock-Lauf besitzt einen offenen Teilcommit";
          entry.recovery_available = true;
          queue.entries.push(entry); queueEvent();
          setTimeout(() => listeners.get("bidirectional-error")?.({ payload: entry.error }));
          return null;
        }
        if (bidirectionalTerminalCommitFailure) {
          // Die simulierte Datenwirkung ist vollständig. Nur der anschließende
          // terminale Queuecommit scheitert sicher vor seinem ersten Schreiben;
          // der persistierte Marker darf deshalb ausschließlich den Abschluss,
          // niemals die Datenoperation erneut anbieten.
          entry.state = "Interrupted";
          entry.progress.phase = "completion_unclear";
          entry.error = "Datenwirkung ist abgeschlossen; Queue-Abschluss muss erneut committed werden";
          entry.recovery_available = false;
          entry.bidirectional_completion_recovery = "complete";
          queue.entries.push(entry); queueEvent();
          setTimeout(() => listeners.get("bidirectional-error")?.({ payload: {
            code: "outcome_unclear", message: entry.error } }));
          return null;
        }
        entry.state = "Completed";
        entry.progress.phase = "done";
        queue.entries.push(entry); queueEvent();
      }
      const emitDone = () => listeners.get("bidirectional-done")?.({ payload: {
        runs: [], baseline_generation: 1, baseline_committed: true } });
      if (bidirectionalFastDoneBeforeResponse) emitDone();
      else setTimeout(emitDone);
      return null;
    }
    if (command === "run_bidirectional_rollback") {
      preparationActions.push({ command });
      const attempts = preparationActions.filter(action => action.command === command).length;
      if (bidirectionalRollbackFailsOnce && attempts === 1) {
        throw { code: "busy", message: "Bidirektionaler Mock-Rollbackstart ist belegt" };
      }
      if (bidirectionalRunDelayMs) await wait(bidirectionalRunDelayMs);
      const recoveryId = pendingBidirectionalRollback?.recoveryId;
      pendingBidirectionalRollback = null;
      const entry = queue.entries.find(item => item.operation_id === recoveryId);
      if (entry) {
        entry.state = "Cancelled"; entry.error = null;
        entry.recovery_available = false;
        entry.bidirectional_completion_recovery = null;
      }
      queueEvent();
      const emitRollbackDone = () => listeners.get("bidirectional-rollback-done")?.({ payload: {
        projections_rolled_back: 2 } });
      if (bidirectionalFastRollbackBeforeResponse) emitRollbackDone();
      else setTimeout(emitRollbackDone);
      return null;
    }
    if (command === "prepare_real_sync" && window.__MOCK_REAL_MODE_WIRE__?.legacy) {
      return structuredClone(window.__MOCK_REAL_MODE_WIRE__.legacy);
    }
    if (command === "prepare_real_sync" || command === "prepare_local_sync") {
      const now = Date.now();
      let preview = { source_identity: localIdentity(args.source, 10),
        target_identity: localIdentity(args.target, 11),
        quarantine_identity: localIdentity("/mock/.doppeldecker-quarantine", 12),
        source_listing: { root_identity: localIdentity(args.source, 10), directories_started: 1,
          directories_completed: 1, entries_seen: 1, listing_digest: Array(32).fill(3),
          provider_token: null, errors: [], complete: true },
        target_listing: { root_identity: localIdentity(args.target, 11), directories_started: 1,
          directories_completed: 1, entries_seen: 0, listing_digest: Array(32).fill(4),
          provider_token: null, errors: [], complete: true },
        plan: structuredClone(plan), options: structuredClone(args.options),
        impact: { new_files: 1, new_bytes: 12, replacements: 0,
          replacement_new_bytes: 0, replacement_old_bytes: 0, metadata_changes: 0,
          quarantine_entries: plan.delete_count, quarantine_bytes: plan.delete_bytes, required_available_bytes: 4108,
          available_bytes: 1000000, available_bytes_known: true },
        plan_digest: Array(32).fill(5), confirmation_phrase: "REAL ziel K7M4Q2",
        scope_warnings: [],
        prepared_at_unix_ms: now, expires_at_unix_ms: now + 600000,
        restore_probe_ok: true, backup_confirmation_required: args.options.mode !== "AddOnly" };
      if (command === "prepare_local_sync") {
        preview.local_execution = { new_file_commit: "assured", file_flush: true, directory_flush: true, irreversible_replacements: 0 };
        const localClass = new URLSearchParams(location.search).get("localCommit");
        if (localClass === "weak" || localClass === "flush" || localClass === "replace") {
          preview.plan.metadata_report = { profile: "Portable", classes: [{ class: "NormalMode", transferred: 0, omitted: 1 }],
            issues: [{ path: "bericht.txt", class: "NormalMode", kind: "Unsupported", reason: "Ziel stellt POSIX-Rechte nicht exakt dar" }],
            unsupported_metadata: 1, strict_verified: false };
          preview.local_execution.new_file_commit = localClass === "flush" ? "assured" : "best_effort";
          preview.local_execution.file_flush = localClass !== "flush";
          preview.local_execution.directory_flush = localClass !== "flush";
          if (localClass === "replace") {
            preview.local_execution.irreversible_replacements = 1;
            preview.impact.replacements = 1;
            preview.plan.actions.push({ type: "UpdateFile", path: "alte-Version.txt", size: 12 });
          }
        }
        preview.restore_probe_ok = false;
        preview.backup_confirmation_required = false;
        const wire = window.__MOCK_LOCAL_SYNC_WIRE__;
        if (wire) preview = structuredClone({ ...wire.preview, plan: wire.plan });
        localSyncCalls.push({ command, args: structuredClone(args) });
        pendingLocalSync = structuredClone(preview);
      }
      return preview;
    }
    if (command === "discard_local_sync") { pendingLocalSync = null; return null; }
    if (command === "enqueue_local_sync") {
      if (!pendingLocalSync || JSON.stringify(args.preparationDigest) !== JSON.stringify(pendingLocalSync.plan_digest)) {
        throw { code: "plan_drift", message: "Lokale Vorbereitung fehlt" };
      }
      localSyncCalls.push({ command, args: structuredClone(args) });
      const preview = pendingLocalSync;
      const entry = queueEntry({ type: "LocalSync", source: preview.source_identity.identity.display_path,
        execution_hint: structuredClone(preview.local_execution),
        target: preview.target_identity.identity.display_path, sandbox: null,
        options: structuredClone(preview.options), expected_plan: structuredClone(preview.plan) });
      localSyncPreviews.set(entry.operation_id, structuredClone(preview));
      pendingLocalSync = null;
      queue.entries.push(entry); queueEvent(); return entry.operation_id;
    }
    if (command === "local_sync_queue_preview") return structuredClone(localSyncPreviews.get(args.operationId));
    if (command === "approve_local_sync_queue_entry") {
      localSyncCalls.push({ command, args: structuredClone(args) });
      const preview = localSyncPreviews.get(args.operationId);
      if (!preview || JSON.stringify(args.preparationDigest) !== JSON.stringify(preview.plan_digest)) {
        throw { code: "plan_drift", message: "Eine neuere lokale Queue-Vorschau muss bestätigt werden" };
      }
      const execution = preview.local_execution;
      if (!args.replacementsConfirmed && (preview.impact.replacements > 0 || preview.impact.metadata_changes > 0
          || execution && (execution.new_file_commit === "best_effort" || !execution.file_flush || !execution.directory_flush))) {
        throw { code: "confirmation_mismatch", message: "Ersetzungen und Metadatenänderungen müssen bestätigt werden" };
      }
      const entry = queue.entries.find(item => item.operation_id === args.operationId);
      entry.state = "Pending"; entry.attention = null; entry.error = null;
      queueEvent(); return structuredClone(queue);
    }
    if (command === "prepare_content_sync") {
      contentSyncCalls.push({ command, args: structuredClone(args) });
      const request = args.request;
      const cancelsBefore = cancelSyncCalls;
      window.__MOCK_EMIT__("sync-preparation-started", "Dateisync: Inhalte und Bestand prüfen");
      if (mockParams.has("contentPrepareDelay")) await new Promise(resolve => setTimeout(resolve, 300));
      window.__MOCK_EMIT__("sync-preparation-done", null);
      if (cancelSyncCalls !== cancelsBefore) throw { code: "interrupted", message: "Dateisync-Vorbereitung abgebrochen" };
      if (contentPreparationFailure) throw { code: contentPreparationFailure, message: "Dateisync-Vorbereitung: vorübergehende Testursache" };
      const currentPlan = structuredClone(plan);
      const replacing = request.options.mode === "Update";
      const removing = request.options.mode === "Mirror";
      if (removing) { currentPlan.delete_count = mockParams.has("contentLargeRemove") ? 1001 : 2; currentPlan.delete_bytes = 24; currentPlan.actions.push({ type: "DeleteFile", path: "nur-im-ziel.txt", size: 24 }); }
      if (replacing) { currentPlan.update_count = 1; currentPlan.actions.push({ type: "UpdateFile", path: "alte-Version.txt", size: 12 }); }
      const preview = { source_root: request.source.root, target_root: request.target.root,
        source_provider: request.source.provider === "webdav" ? "web_dav" : request.source.provider,
        target_provider: request.target.provider === "webdav" ? "web_dav" : request.target.provider,
        source_identity: request.source.provider === "local" ? localIdentity(request.source.root, 10) : providerIdentity(request.source),
        target_identity: request.target.provider === "local" ? localIdentity(request.target.root, 11) : providerIdentity(request.target),
        options: structuredClone(request.options), plan: currentPlan, digest: Array(32).fill(8),
        expires_unix_ms: Date.now() + 600000,
        limitations: ["Best Effort: Der Zielbestand wird vor dem Commit geprüft und der Inhalt danach zurückgelesen. Kein automatischer Wiederholversuch bei unklarem Ausgang."],
        irreversible_replacements: replacing ? 1 : 0, irreversible_removals: removing ? currentPlan.delete_count : 0,
        irreversible_removal_bytes: removing ? 24 : 0, blocked_removals: 0,
        removal_confirmation: removing && mockParams.has("contentLargeRemove") ? "ENTFERNEN 1001 MOCK" : null };
      if (mockParams.has("contentQuarantine") && request.target.provider === "local") {
        preview.quarantined_replacements = preview.irreversible_replacements;
        preview.quarantined_removals = preview.irreversible_removals;
        preview.irreversible_replacements = 0; preview.irreversible_removals = 0;
        preview.irreversible_removal_bytes = 0; preview.removal_confirmation = null;
        preview.quarantine_run = "run-mock-geprüft";
        preview.local_execution = { new_file_commit: "assured", file_flush: true, directory_flush: true, irreversible_replacements: 0 };
        preview.limitations = ["Die alte Zielversion wird vor dem Ersetzen in der geprüften lokalen Quarantäne gesichert."];
      }
      const preparedPreview = window.__MOCK_REAL_MODE_WIRE__?.content ?? preview;
      pendingContentSync = { request: structuredClone(request), preview: preparedPreview };
      return structuredClone(preparedPreview);
    }
    if (command === "discard_content_sync") {
      contentSyncCalls.push({ command }); pendingContentSync = null; return null;
    }
    if (command === "enqueue_content_sync") {
      contentSyncCalls.push({ command, args: structuredClone(args) });
      if (!pendingContentSync || JSON.stringify(args.preparationDigest) !== JSON.stringify(pendingContentSync.preview.digest) || !args.effectsConfirmed) {
        throw { code: "plan_drift", message: "Dateisync-Vorbereitung fehlt oder ist nicht bestätigt" };
      }
      const attempts = contentSyncCalls.filter(call => call.command === command).length;
      if (providerRunFailsOnce && attempts === 1) throw { code: "busy", message: "Provider-Mock-Start ist vor der Entnahme belegt" };
      const { request, preview } = pendingContentSync;
      if (preview.removal_confirmation && args.removalPhrase !== preview.removal_confirmation) throw { code: "confirmation_mismatch", message: "Entfernungstext stimmt nicht überein" };
      const entry = queueEntry({ type: "ContentSync", source: queueEndpoint(request.source), target: queueEndpoint(request.target),
        options: structuredClone(request.options), expected_plan: structuredClone(preview.plan), binding_digest: [...preview.digest] });
      contentSyncPreviews.set(entry.operation_id, structuredClone(preview));
      if (mockParams.has("contentReopen")) {
        entry.state = "AwaitingConfirmation"; entry.attention = { type: "RemoteRun", reason: "Nach Neustart frisch vorbereitet" };
        const fresh = contentSyncPreviews.get(entry.operation_id); fresh.digest = Array(32).fill(9);
        fresh.irreversible_replacements = 2; fresh.plan.update_count = 2;
        fresh.plan.actions.push({ type: "UpdateFile", path: "frisch-geändert.txt", size: 8 });
      }
      queue.entries.push(entry); pendingContentSync = null; queueEvent(); return entry.operation_id;
    }
    if (command === "content_sync_queue_preview") {
      contentSyncCalls.push({ command, args: structuredClone(args) });
      return structuredClone(contentSyncPreviews.get(args.operationId));
    }
    if (command === "approve_content_sync_queue_entry") {
      contentSyncCalls.push({ command, args: structuredClone(args) });
      const preview = contentSyncPreviews.get(args.operationId);
      if (!preview || JSON.stringify(args.preparationDigest) !== JSON.stringify(preview.digest)) {
        throw { code: "plan_drift", message: "Eine neuere Sync-Vorschau muss bestätigt werden" };
      }
      if (!args.effectsConfirmed || (preview.removal_confirmation && args.removalPhrase !== preview.removal_confirmation)) {
        throw { code: "confirmation_mismatch", message: "Die angezeigte Sync-Wirkung wurde nicht bestätigt" };
      }
      const entry = queue.entries.find(item => item.operation_id === args.operationId);
      entry.state = "Pending"; entry.attention = null; queueEvent(); return structuredClone(queue);
    }
    if (command === "discard_real_sync") return null;
    if (command === "discard_sftp_sync") return null;
    if (command === "discard_ftps_sync") return null;
    if (command === "discard_webdav_sync") return null;
    if (command === "discard_smb_sync") return null;
    if (command === "discard_provider_sync") {
      preparationActions.push({ command }); pendingProvider = null; return null;
    }
    if (command === "prepare_sftp_sync") return remotePreview(args.request);
    if (command === "prepare_ftps_sync") return remotePreview(args.request, "ftps");
    if (command === "prepare_webdav_sync") return remotePreview(args.request, "webdav");
    if (command === "prepare_smb_sync") return remotePreview(args.request, "smb");
    if (command === "prepare_provider_sync") {
      preparationActions.push({ command });
      if (pendingProvider) {
        throw { code: "busy", message: "Provider-Mock-Vorbereitung ist noch belegt" };
      }
      pendingProvider = { request: structuredClone(args.request), preview: providerPreview(args.request) };
      return structuredClone(pendingProvider.preview);
    }
    if (["run_sftp_sync", "run_ftps_sync", "run_webdav_sync", "run_smb_sync", "run_provider_sync"].includes(command)) {
      const protocol = command.replace("run_", "").replace("_sync", "");
      if (command === "run_provider_sync") {
        preparationActions.push({ command });
        const attempts = preparationActions.filter(action => action.command === command).length;
        if (providerRunFailsOnce && attempts === 1) {
          throw { code: "busy", message: "Provider-Mock-Start ist vor der Entnahme belegt" };
        }
        pendingProvider = null;
      }
      if (args.effectsConfirmed !== true) throw { code: "confirmation_mismatch", message: "Wirkung nicht bestätigt" };
      document.documentElement.dataset[`${protocol}Confirmation`] = String(args.effectsConfirmed);
      queueMicrotask(() => listeners.get("sync-done")?.({ payload: {
        files_copied: 1, files_deleted: 0, metadata_updated: 0, bytes_copied: 12,
        cancelled: false, dry_run: false, quarantine: null,
        metadata_report: structuredClone(plan.metadata_report) } }));
      return null;
    }
    if (command === "run_real_sync") {
      if (args.effectsConfirmed !== true) throw { code: "confirmation_mismatch", message: "Wirkung nicht bestätigt" };
      document.documentElement.dataset.realConfirmation = String(args.effectsConfirmed);
      queueMicrotask(() => listeners.get("sync-done")?.({ payload: {
        files_copied: 1, files_deleted: 0, metadata_updated: 0, bytes_copied: 12,
        cancelled: false, dry_run: false, quarantine: null,
        metadata_report: structuredClone(plan.metadata_report) } }));
      return null;
    }
    if (command === "safety_info") return { real_enabled: mockParams.has("realEnabled"), sandbox: mockParams.has("realEnabled") ? "/mock" : null,
      real_mode_available: true, real_mode_pending: false,
      max_delete_entries: 100, max_delete_fraction: 0.25, quarantine_retention_days: 30 };
    if (command === "list_favorites") return structuredClone(favorites);
    const favoriteEndpointMatches = (favorite, args) =>
      (favorite.provider || "local") === (args.provider || "local")
      && (favorite.profile_id || null) === (args.profileId || null)
      && favorite.path === args.path;
    if (command === "add_favorite" || command === "add_remote_favorite") {
      const provider = command === "add_favorite" ? "local" : args.provider;
      const endpoint = { ...args, provider };
      favorites = favorites.filter(favorite => !favoriteEndpointMatches(favorite, endpoint));
      favorites.push({
        name: args.name,
        path: args.path,
        provider,
        ...(provider === "local" ? {} : { profile_id: args.profileId }),
      });
      return structuredClone(favorites);
    }
    if (command === "remove_favorite") {
      favorites = favorites.filter(favorite => !favoriteEndpointMatches(favorite, args));
      return structuredClone(favorites);
    }
    if (command === "move_favorite") {
      const from = favorites.findIndex(favorite => favoriteEndpointMatches(favorite, args));
      if (from >= 0) {
        const [moved] = favorites.splice(from, 1);
        favorites.splice(Math.min(args.index, favorites.length), 0, moved);
      }
      return structuredClone(favorites);
    }
    if (command === "import_finder_favorites") {
      // Nachbildung des additiven Finder-Imports: ein neuer Ordner, ein Duplikat.
      const summary = { added: 0, skipped_existing: 0, skipped_missing: 1,
        skipped_hidden: 1, skipped_unreadable: 0, favorites: null };
      for (const candidate of [
        { name: "Finder-Ordner", path: "/mock/finder/Finder-Ordner" },
        { name: "quelle", path: "/mock/quelle" },
      ]) {
        if (favorites.some(favorite => favorite.path === candidate.path)) {
          summary.skipped_existing += 1;
        } else {
          favorites.push({ ...candidate, provider: "local" });
          summary.added += 1;
        }
      }
      summary.favorites = structuredClone(favorites);
      return summary;
    }
    if (command === "list_provider_quarantine_runs" ||
        command === "list_provider_quarantine_entries") {
      quarantineCalls.push({ command, endpoint: structuredClone(args.endpoint) });
      if (command === "list_provider_quarantine_runs" && args.endpoint?.provider === "smb") {
        return [{
          id: "smb-run", state: "complete", status: "complete",
          created_unix_ms: Date.now(), expires_unix_ms: Date.now() + 86_400_000,
          target_root: "/", items_total: 1, items_available: 1,
          items_restorable: 0, items_manual_recovery: 0, items_recovery_pending: 0,
          items_restored: 0, entries_available: 1, bytes_available: 8,
        }];
      }
      if (command === "list_provider_quarantine_entries" && args.endpoint?.provider === "smb") {
        return [{ relative_path: "alt.txt", is_dir: false, reason: "replaced",
          restored: false, available: true, automatic_restore_available: false,
          manual_recovery_required: false, recovery_pending: false, entries: 1, bytes: 8 }];
      }
      return [];
    }
    if (command === "restore_provider_quarantine_entry") {
      return { path: args.relativePath, outcome: "restored" };
    }
    if (command === "purge_provider_quarantine_run") return null;
    if (command === "list_quarantine_runs") {
      if (quarantineBusy) {
        throw {
          code: "busy",
          message: "Waehrend einer Datei- oder Sync-Operation ist die Quarantaene gesperrt",
        };
      }
      if (quarantineOpenDelay) await wait(160);
      if (quarantineListErrorAfterPurge && quarantinePurgeAttempted) {
        throw { code: "io", message: "Mock-Refresh nach Purge fehlgeschlagen" };
      }
      if (quarantineBadgeRace && quarantineBadgeRaceStep === 1) {
        quarantineBadgeRaceStep = 2;
        await wait(160);
        return [{
          id: "badge-run", state: "complete", created_unix_ms: Date.now(),
          expires_unix_ms: Date.now() + 86_400_000, target_root: "/mock/ziel",
          items_available: 9, items_total: 9, bytes_available: 72,
        }];
      }
      if (quarantineBadgeRace && quarantineBadgeRaceStep === 2) {
        quarantineBadgeRaceStep = 0;
        await wait(10);
      }
      return structuredClone(quarantineRuns);
    }
    if (command === "list_quarantine_entries") {
      if (quarantineRace) {
        if (args.runId === "run-langsam") {
          await holdRequest("quarantineRace", { runId: args.runId });
        }
        return [{
          relative_path: `${args.runId}.txt`, reason: "replaced", available: true,
          restored: false, entries: 1, bytes: 8,
        }];
      }
      return structuredClone(quarantineEntries);
    }
    if (command === "restore_quarantine_entry") {
      if (quarantineMutationDelayMs) await wait(quarantineMutationDelayMs);
      quarantineActions.push({ command, runId: args.runId, relativePath: args.relativePath });
      quarantineEntries = quarantineEntries.map(entry => entry.relative_path === args.relativePath
        ? { ...entry, available: false, restored: true }
        : entry);
      return quarantineInvalidRestoreResponse ? {} : {
        path: `/mock/ziel/${args.relativePath}`,
        outcome: quarantineRecoveryPending ? "reconciled" : "restored",
      };
    }
    if (command === "purge_quarantine_run") {
      if (quarantineMutationDelayMs) await wait(quarantineMutationDelayMs);
      quarantinePurgeAttempted = true;
      quarantineActions.push({ command, runId: args.runId, confirmation: args.confirmation });
      if (quarantinePurgeError) {
        quarantineRuns = quarantineRuns.map(run => ({ ...run, status: "purge_recovery" }));
        throw { code: "outcome_unclear", message: "Mock-Purge braucht Recovery" };
      }
      quarantineRuns = quarantineRuns.map(run => ({
        ...run,
        status: "purged",
        items_available: 0,
        entries_available: 0,
        bytes_available: 0,
      }));
      quarantineEntries = quarantineEntries.map(entry => ({
        ...entry,
        available: false,
      }));
      return null;
    }
    if (command === "list_sftp_agent_keys") {
      if (mockParams.has("holdSftpAgent")) await holdRequest("holdSftpAgent", {});
      if (editorAgentDelayMs) await wait(editorAgentDelayMs);
      if (sftpProfileChangesDuringAgent === "remove") sftpProfiles = [];
      if (sftpProfileChangesDuringAgent === "change" && sftpProfiles[0]) {
        sftpProfiles[0] = { ...sftpProfiles[0], name: "Extern geändert" };
      }
      return mockParams.has("sftpAgentKeys")
        ? [{ comment: "Asynchroner Testschlüssel", algorithm: "ssh-ed25519", sha256: Array(32).fill(23) }]
        : [];
    }
    if (["list_sftp_profiles", "list_ftps_profiles", "list_webdav_profiles", "list_smb_profiles"].includes(command)) {
      const protocol = command.slice(5, -9);
      const queued = profileReadQueues[protocol].shift();
      if (queued) {
        if (queued.delay) await wait(queued.delay);
        return structuredClone(queued.profiles);
      }
      if (protocol === "sftp" && sftpProfileListDelayMs) await wait(sftpProfileListDelayMs);
      return structuredClone(profilesFor(protocol));
    }
    if (command === "capture_connection_secret") {
      if (editorSecretDelayMs) await wait(editorSecretDelayMs);
      const secretId = `mock-pending-secret-${++mockSecretCaptureSequence}`;
      pendingSecrets.set(secretId, {
        protocol: args.protocol,
        profileId: args.profileId,
      });
      connectionActions.push({
        command,
        protocol: args.protocol,
        profileId: args.profileId,
        secretId,
      });
      return { secretId };
    }
    if (command === "discard_connection_secret") {
      connectionActions.push({ command, secretId: args.secretId });
      if (!pendingSecrets.has(args.secretId)) {
        throw { code: "secret_expired", message: "Mock-Einmalwert wurde bereits verbraucht" };
      }
      pendingSecrets.delete(args.secretId);
      return null;
    }
    if (["save_sftp_profile", "save_ftps_profile", "save_webdav_profile", "save_smb_profile"].includes(command)) {
      const protocol = command.slice(5, -8);
      connectionActions.push({
        command,
        profile: structuredClone(args.profile),
        secretId: args.secretId ?? null,
        profileRevision: args.profileRevision ?? null,
      });
      if (editorSaveDelayMs) await wait(editorSaveDelayMs);
      try {
        requireProfileMutationRevision(protocol, args.profile.id, args.profileRevision ?? null);
      } catch (error) {
        // Der echte Backendpfad hat die native Einmal-ID zu diesem Zeitpunkt
        // verbraucht und entfernt einen bereits frisch geschriebenen Zufallswert
        // nach der mutationsfreien CAS-Ablehnung wieder sicher.
        if (args.secretId != null) {
          pendingSecrets.delete(args.secretId);
          secretTransactions.push({ protocol, outcome: "stale_save_new_removed" });
        }
        throw error;
      }
      const originalReference = protocol === "sftp"
        ? args.profile.connection.authentication.secret
        : args.profile.connection.password_reference;
      let savedProfile = structuredClone(args.profile);
      let reference = originalReference;
      if (args.secretId != null) {
        const pending = pendingSecrets.get(args.secretId);
        if (!pending || pending.protocol !== protocol || pending.profileId !== args.profile.id) {
          throw { code: "secret_purpose_mismatch", message: "Mock-Secret gehört nicht zu diesem Profil" };
        }
        if (profileSaveBeforeConsumeError === protocol || profileSaveBeforeConsumeError === "all") {
          throw { code: "io", message: `Mock-${protocol}-Save wurde vor Secretverbrauch abgelehnt` };
        }
        pendingSecrets.delete(args.secretId);
        const existed = storedSecretReferences.has(secretKey(originalReference));
        reference = {
          service: originalReference.service,
          account: `mock-secret-${++mockSecretReferenceSequence}`,
        };
        if (protocol === "sftp") savedProfile.connection.authentication.secret = reference;
        else savedProfile.connection.password_reference = reference;
        if (profileSaveError === protocol || profileSaveError === "all") {
          secretTransactions.push({ protocol, reference: structuredClone(reference),
            outcome: existed ? "old_unchanged_new_removed" : "removed_new" });
          throw { code: "io", message: `Mock-${protocol}-Profildatei konnte nicht gespeichert werden` };
        }
        storedSecretReferences.add(secretKey(reference));
        secretWrites.push({ command, reference: structuredClone(reference) });
        secretTransactions.push({ protocol, reference: structuredClone(reference), outcome: "committed" });
      } else if (profileSaveError === protocol || profileSaveError === "all") {
        throw { code: "io", message: `Mock-${protocol}-Profildatei konnte nicht gespeichert werden` };
      }
      const connection = savedProfile.connection;
      if (connection.alternate_hosts?.length === 0) delete connection.alternate_hosts;
      if (protocol === "ftps") {
        connection.transport_security ??= "explicit_tls";
        if (connection.certificate == null) delete connection.certificate;
        if (connection.certificate_policy == null) delete connection.certificate_policy;
      }
      const savedProfiles = replaceStoredProfile(protocol, savedProfile);
      if (profileSaveVisibleError === protocol || profileSaveVisibleError === "all") {
        throw { code: "outcome_unclear",
          message: `Mock-${protocol}-Profil ist trotz spätem Speicherfehler sichtbar` };
      }
      return savedProfiles;
    }
    if (["remove_sftp_profile", "remove_ftps_profile", "remove_webdav_profile", "remove_smb_profile"].includes(command)) {
      const protocol = command.slice(7, -8);
      requireProfileRevision(protocol, args.profileId, args.profileRevision);
      if (protocol === "sftp") sftpProfiles = sftpProfiles.filter(profile => profile.id !== args.profileId);
      if (protocol === "ftps") ftpsProfiles = ftpsProfiles.filter(profile => profile.id !== args.profileId);
      if (protocol === "webdav") webdavProfiles = webdavProfiles.filter(profile => profile.id !== args.profileId);
      if (protocol === "smb") smbProfiles = smbProfiles.filter(profile => profile.id !== args.profileId);
      return structuredClone(profilesFor(protocol));
    }
    if (["discover_sftp_host_key", "discover_ftps_certificate", "verify_ftps_certificate", "discover_webdav_certificate", "discover_smb_server"].includes(command)) {
      connectionActions.push({ command, args: structuredClone(args) });
      if (editorDiscoveryDelayMs) await wait(editorDiscoveryDelayMs);
      if (command === "discover_sftp_host_key") return { algorithm: "ssh-ed25519", sha256: Array(32).fill(11) };
      if (command === "verify_ftps_certificate") {
        if (mockParams.has("ftpsPublicVerifyError")) throw { code: "tls_identity_unverified", message: "Öffentliche Zertifikatsprüfung fehlgeschlagen" };
        return { sha256: Array(32).fill(9) };
      }
      if (command === "discover_ftps_certificate") return { sha256: Array(32).fill(mockParams.has("ftpsRenewedCertificate") ? 9 : 7) };
      if (command === "discover_webdav_certificate") return { sha256: Array(32).fill(5) };
      return { server_guid: Array(16).fill(6), dialect: 785,
        encryption_capable: true, signing_required: true };
    }
    if (command === "quit_after_queue_warning") {
      if (!args.token) throw { code: "confirmation_mismatch", message: "Beenden-Token fehlt" };
      document.documentElement.dataset.quitAfterQueueWarning = "true";
      document.documentElement.dataset.quitAfterQueueToken = args.token;
      return null;
    }
    if (command === "list_operation_queue") return structuredClone(queue);
    if (command === "finish_bidirectional_completion_recovery") {
      const entry = queue.entries.find(item => item.operation_id === args.operationId);
      if (!entry?.bidirectional_completion_recovery) {
        throw { code: "invalid_input", message: "Abschluss-Recovery-Marker fehlt" };
      }
      entry.state = entry.bidirectional_completion_recovery === "rolled_back"
        ? "Cancelled"
        : "Completed";
      entry.progress.phase = entry.state === "Cancelled" ? "rolled_back" : "done";
      entry.error = null;
      entry.recovery_available = false;
      entry.bidirectional_completion_recovery = null;
      queueEvent();
      return structuredClone(queue);
    }
    if (command === "set_queue_paused") {
      queue.paused = args.paused;
      if (!args.paused) {
        const next = queue.entries.find(entry => entry.state === "Pending");
        if (next?.operation.type === "Sync") {
          const changed = structuredClone(next.operation.expected_plan);
          changed.copy_count += 1;
          next.state = "AwaitingConfirmation";
          next.error = "Quelle oder Ziel hat sich seit der Queue-Bestätigung geändert";
          next.attention = { type: "PlanChanged", reason: next.error,
            confirmation: `PLAN ${next.operation_id.slice(0, 8)} ${changed.copy_count} ${changed.update_count} ${changed.delete_count}`,
            current_plan: changed };
        }
      }
      queueEvent(); return structuredClone(queue);
    }
    if (command === "set_queue_stop_on_error") {
      queue.stop_on_error = args.stop; queueEvent(); return structuredClone(queue);
    }
    if (command === "move_queue_entry") {
      const index = queue.entries.findIndex(entry => entry.operation_id === args.operationId);
      const other = index + args.delta;
      if (index >= 0 && other >= 0 && other < queue.entries.length &&
          queue.entries[index].state === "Pending" && queue.entries[other].state === "Pending") {
        [queue.entries[index], queue.entries[other]] = [queue.entries[other], queue.entries[index]];
      }
      queueEvent(); return structuredClone(queue);
    }
    if (command === "remove_queue_entry") {
      queue.entries = queue.entries.filter(entry => entry.operation_id !== args.operationId);
      queueEvent(); return structuredClone(queue);
    }
    if (command === "retry_queue_entry") {
      const entry = queue.entries.find(item => item.operation_id === args.operationId);
      if (entry?.error_code === "outcome_unclear") {
        throw { code: "outcome_unclear", message: "Unklarer Ausgang darf nicht wiederholt werden" };
      }
      if (entry) { entry.state = "Pending"; entry.error = null; entry.error_code = null; }
      queueEvent(); return structuredClone(queue);
    }
    if (command === "discard_replan_required_queue_entry") {
      const index = queue.entries.findIndex(entry => entry.operation_id === args.operationId);
      if (index >= 0 && queue.entries[index].state === "Failed"
          && queue.entries[index].error_code === "replan_required") {
        queue.entries.splice(index, 1);
      }
      queueEvent(); return structuredClone(queue);
    }
    if (command === "acknowledge_directory_dacl_outcome") {
      (window.__MOCK_DACL_ACKS__ ||= []).push(structuredClone(args));
      if (window.__MOCK_DACL_ACK_FAIL__) throw { code: "plan_drift", message: "Rechtezustand hat sich geändert" };
      const entry = queue.entries.find(item => item.operation_id === args.operationId);
      if (entry && args.possibleEffectReviewed && entry.directory_dacl_recovery) {
        entry.state = "Cancelled"; entry.directory_dacl_recovery = false;
        entry.progress.phase = "recovery_acknowledged";
        const report = entry.directory_dacl_report || { completed: [], uncertain: [] };
        report.completed.push(...report.uncertain); report.uncertain = [];
        entry.directory_dacl_report = report; entry.progress.directory_dacl_report = structuredClone(report);
        entry.temp_recovery_paths = [];
      }
      queueEvent(); return structuredClone(queue);
    }
    if (command === "acknowledge_file_dacl_outcome") {
      (window.__MOCK_FILE_DACL_ACKS__ ||= []).push(structuredClone(args));
      if (window.__MOCK_FILE_DACL_ACK_FAIL__) throw { code: "plan_drift", message: "Rechtezustand hat sich geändert" };
      const entry = queue.entries.find(item => item.operation_id === args.operationId);
      if (entry && args.possibleEffectReviewed && entry.file_dacl_recovery) {
        entry.state = "Cancelled"; entry.file_dacl_recovery = false;
        entry.progress.phase = "recovery_acknowledged";
        const report = entry.file_dacl_report || { completed: [], uncertain: [] };
        report.completed.push(...report.uncertain); report.uncertain = [];
        entry.file_dacl_report = report; entry.progress.file_dacl_report = structuredClone(report);
        entry.temp_recovery_paths = [];
      }
      queueEvent(); return structuredClone(queue);
    }
    if (command === "acknowledge_created_directory_dacl_outcome") {
      (window.__MOCK_CREATED_DIRECTORY_DACL_ACKS__ ||= []).push(structuredClone(args));
      if (window.__MOCK_CREATED_DIRECTORY_DACL_ACK_FAIL__) throw { code: "plan_drift", message: "Rechtezustand hat sich geändert" };
      const entry = queue.entries.find(item => item.operation_id === args.operationId);
      if (entry && args.possibleEffectReviewed && entry.created_directory_dacl_recovery) {
        entry.state = "Cancelled"; entry.created_directory_dacl_recovery = false;
        entry.progress.phase = "recovery_acknowledged";
        const report = entry.created_directory_dacl_report || { completed: [], uncertain: [] };
        report.completed.push(...report.uncertain); report.uncertain = [];
        entry.created_directory_dacl_report = report; entry.progress.created_directory_dacl_report = structuredClone(report);
        entry.temp_recovery_paths = [];
      }
      queueEvent(); return structuredClone(queue);
    }
    if (command === "acknowledge_unclear_queue_outcome") {
      const entry = queue.entries.find(item => item.operation_id === args.operationId);
      if (entry && args.possibleEffectReviewed && entry.error_code === "outcome_unclear") {
        entry.state = "Cancelled";
        entry.progress.phase = "recovery_acknowledged";
        entry.error += "; Zielzustand wurde manuell geprüft und der Eintrag ohne Wiederholung geschlossen.";
      }
      queueEvent(); return structuredClone(queue);
    }
    if (command === "acknowledge_legacy_webdav_delete") {
      const index = queue.entries.findIndex(entry => entry.operation_id === args.operationId);
      if (index >= 0 && queue.entries[index].state === "Failed") {
        queue.entries.splice(index, 1);
      } else if (index >= 0 && args.possibleEffectReviewed) {
        queue.entries[index].state = "Cancelled";
        queue.entries[index].progress.phase = "recovery_acknowledged";
        queue.entries[index].error += " Manuelle Prüfung wurde bestätigt; der Altplan ist terminal geschlossen.";
      }
      queueEvent(); return structuredClone(queue);
    }
    if (command === "discard_queue_confirmation") {
      const entry = queue.entries.find(item => item.operation_id === args.operationId);
      if (entry?.operation.type === "BidirectionalForward" && entry.state === "AwaitingConfirmation") {
        forwardPreviews.delete(entry.operation_id);
        const active = forwardStatuses.has(JSON.stringify(entry.operation.pair_id));
        entry.state = active ? "Interrupted" : "Cancelled"; entry.attention = null; entry.recovery_available = active; queueEvent();
        if (active) throw { code: "replan_required", message: "Vorhandene Teilwirkungen bleiben erhalten; neu abgleichen" };
        return structuredClone(queue);
      }
      if (entry && entry.state === "AwaitingConfirmation" && entry.attention?.type !== "RemoteRun") {
        entry.state = "Cancelled"; entry.attention = null;
        entry.error = "Die neu entdeckte Lage wurde nicht übernommen; die Operation wurde verworfen";
      }
      queueEvent(); return structuredClone(queue);
    }
    if (command === "approve_queue_entry") {
      const entry = queue.entries.find(item => item.operation_id === args.operationId);
      if (!entry || entry.state !== "AwaitingConfirmation" || args.effectsConfirmed !== true
          || entry.attention?.type === "RemoteRun" || entry.error_code === "outcome_unclear"
          || entry.recovery_available || entry.bidirectional_completion_recovery
          || JSON.stringify(entry.attention) !== JSON.stringify(args.expectedAttention)) {
        throw { code: "plan_drift", message: "Die Queue-Rückfrage hat sich seit der Anzeige geändert" };
      }
      if (entry) {
        const attention = entry.attention;
        if (attention.type === "SourceTreeChanged") entry.operation.source_tree_expected = structuredClone(attention.current_tree);
        if (attention.type === "PlanChanged") entry.operation.expected_plan = structuredClone(attention.current_plan);
        if (attention.type === "ConflictsChanged") {
          entry.operation.expected_conflicts = structuredClone(attention.current_conflicts);
          entry.operation.expected_conflict_proofs = structuredClone(attention.current_conflict_proofs || []);
        }
        if (attention.type === "LocalFileCommit") {
          entry.operation.local_copy_new_best_effort_expected = structuredClone(attention.target_identity);
          if (entry.operation.type === "ProviderFile") entry.operation.copy_new_best_effort_expected = true;
        }
        if (attention.type === "BestEffortCommitDowngrade") {
          if (attention.copy_new_best_effort) entry.operation.copy_new_best_effort_expected = true;
          if (attention.replace_best_effort) entry.operation[entry.operation.type === "File" ? "local_replace_best_effort_expected" : "replace_best_effort_expected"] = true;
          if (attention.remove_source_best_effort) entry.operation.remove_source_best_effort_expected = true;
        }
        if (attention.type === "BidirectionalPlanChanged") {
          entry.operation.expected_baseline_digest = structuredClone(attention.current_baseline_digest);
          entry.operation.expected_plan_digest = structuredClone(attention.current_plan_digest);
        }
        entry.state = "Pending"; entry.error = null; entry.attention = null;
      }
      queueEvent(); return structuredClone(queue);
    }
    if (command === "run_sync") {
      const entry = queueEntry({ type: "Sync", source: args.source, target: args.target,
        sandbox: null, options: structuredClone(args.options),
        expected_plan: structuredClone(args.expectedPlan) });
      queue.entries.push(entry); queueEvent(); return entry.operation_id;
    }
    if (command === "enqueue_sftp_sync") {
      const request = args.request;
      const entry = queueEntry({ type: "SftpSync", direction: request.direction,
        connection: structuredClone(sftpProfiles.find(profile => profile.id === request.profileId).connection),
        local_root: request.localRoot, remote_root: request.remoteRoot,
        options: structuredClone(request.options), expected_plan: structuredClone(request.expectedPlan) });
      queue.entries.push(entry); queueEvent(); return structuredClone(queue);
    }
    if (command === "enqueue_ftps_sync") {
      const request = args.request;
      const entry = queueEntry({ type: "FtpsSync", direction: request.direction,
        connection: structuredClone(ftpsProfiles.find(profile => profile.id === request.profileId).connection),
        local_root: request.localRoot, remote_root: request.remoteRoot,
        options: structuredClone(request.options), expected_plan: structuredClone(request.expectedPlan) });
      queue.entries.push(entry); queueEvent(); return structuredClone(queue);
    }
    if (command === "enqueue_webdav_sync") {
      const request = args.request;
      const entry = queueEntry({ type: "WebDavSync", direction: request.direction,
        connection: structuredClone(webdavProfiles.find(profile => profile.id === request.profileId).connection),
        local_root: request.localRoot, remote_root: request.remoteRoot,
        options: structuredClone(request.options), expected_plan: structuredClone(request.expectedPlan) });
      queue.entries.push(entry); queueEvent(); return structuredClone(queue);
    }
    if (command === "enqueue_smb_sync") {
      const request = args.request;
      const entry = queueEntry({ type: "SmbSync", direction: request.direction,
        connection: structuredClone(smbProfiles.find(profile => profile.id === request.profileId).connection),
        local_root: request.localRoot, remote_root: request.remoteRoot,
        options: structuredClone(request.options), expected_plan: structuredClone(request.expectedPlan) });
      queue.entries.push(entry); queueEvent(); return structuredClone(queue);
    }
    if (command === "enqueue_provider_sync") {
      preparationActions.push({ command });
      const attempts = preparationActions.filter(action => action.command === command).length;
      if (providerEnqueueFailsOnce && attempts === 1) {
        throw { code: "busy", message: "Provider-Mock-Enqueue ist vor dem Commit belegt" };
      }
      const request = pendingProvider.request;
      const entry = queueEntry({ type: "ProviderSync",
        source: queueEndpoint(request.source), target: queueEndpoint(request.target),
        options: structuredClone(request.options), expected_plan: structuredClone(pendingProvider.preview.plan) });
      queue.entries.push(entry); pendingProvider = null; queueEvent(); return structuredClone(queue);
    }
    if (["approve_sftp_queue_entry", "approve_ftps_queue_entry", "approve_webdav_queue_entry", "approve_smb_queue_entry", "approve_provider_queue_entry", "approve_bidirectional_queue_entry"].includes(command)) {
      const before = structuredClone(queue);
      const entry = queue.entries.find(item => item.operation_id === args.operationId);
      if (entry) { entry.state = "Completed"; entry.error = null; entry.attention = null; }
      queue.revision += 1;
      queueEvent();
      if (bidirectionalFastApproveBeforeResponse && command === "approve_bidirectional_queue_entry") {
        listeners.get("bidirectional-done")?.({ payload: {
          runs: [], baseline_generation: 2, baseline_committed: true } });
        return before;
      }
      return structuredClone(queue);
    }
    if (command === "check_provider_file_operation") {
      const request = args.request;
      if (mockParams.has("sftpContents") && request.source.provider === "sftp"
          && (request.kind !== "copy" || request.target.provider !== "local")) {
        throw { code: "unsupported", message: "SFTP-Inhalte erlauben nur additive lokale Einzeldatei-Downloads" };
      }
      const webdavUpload = mockParams.has("webdavUpload") && request.source.provider === "local" && request.target.provider === "webdav" && request.kind === "copy";
      const ftpsPreparation = request.source.provider === "ftps" || request.target.provider === "ftps";
      const preparation = (request.source.provider === "sftp" && mockParams.has("sftpContents")) || webdavUpload || ftpsPreparation;
      const cancelsBefore = cancelSyncCalls;
      if (preparation) window.__MOCK_EMIT__("fileop-preparation-started", ftpsPreparation ? "FTP-/FTPS-Dateioperation wird vorbereitet. Abbruch verhindert die spätere Einreihung; ein bereits laufender Serverzugriff wird noch geprüft beendet." : webdavUpload ? "WebDAV-Dateiupload: lokale Inhalte und Zielbedingungen prüfen" : "SFTP-Inhalte · Best Effort: pfadbasiertes Öffnungsrisiko");
      if (mockParams.has("ftpsPreparationGate")) {
        window.__AP19_PREPARATION_HELD__ = true;
        await new Promise(resolve => { window.__AP19_RELEASE_PREPARATION__ = resolve; });
      }
      if (fileActionPreviewDelayMs) await wait(fileActionPreviewDelayMs);
      if (preparation) window.__MOCK_EMIT__("fileop-preparation-done", null);
      if (preparation && cancelsBefore !== cancelSyncCalls) throw { code: "interrupted", message: "Inhaltsvorbereitung wurde abgebrochen" };
      if (mockParams.has("ftpsConflictChoice")) {
        const policy = args.choice?.policy || null;
        if (policy && args.choice.id !== window.__AP21_CHOICE_ID__) throw { code: "plan_drift", message: "Veraltete Konfliktentscheidung" };
        (window.__AP21_POLICIES__ ||= []).push(policy);
        const id = `mock-ap21-${window.__AP21_POLICIES__.length}`;
        window.__AP21_CHOICE_ID__ = id;
        const skip = policy === "skip";
        const partialMerge = skip && mockParams.has("ftpsPartialMerge");
        const both = policy === "both" || policy === "inspect";
        const replacementDenied = ["overwrite", "inspect"].includes(policy) && mockParams.has("ftpsReplaceDenied");
        return {
          requiresConflictChoice: policy === null, skipOnly: skip && !partialMerge,
          sourceRemovalRequired: !skip,
          conflicts: ["/to/remote.txt"], conflictDetails: [{ relative_name: "remote.txt", source: conflictEntry("/from"), target: conflictEntry("/to", 23) }],
          conflictPreparationId: id, keepBothPreparationId: both ? id : null,
          keepBothNames: both ? ["remote (Kopie).txt"] : [], keepBothBlocker: "Kollisionsfreier Commit wird erst nach der Wahl geprüft.",
          sourceReadLimitations: [], sourceTree: null,
          targetCapabilities: { provider: "ftps", copyNew: policy === "both", copyNewBestEffort: partialMerge || policy !== null && !skip && policy !== "both",
            replace: false, replaceBestEffort: ["overwrite", "inspect"].includes(policy) && !replacementDenied, removeSource: false, removeSourceBestEffort: false, renameNoreplace: both, limitations: [], blockers: replacementDenied ? ["Ersetzen ist für diese Ordnerauswahl nicht gebunden"] : [] },
          sourceCapabilities: !skip && !replacementDenied && policy !== null && request.kind === "move" ? { provider: "ftps", copyNew: false, copyNewBestEffort: false, replace: false,
            replaceBestEffort: false, removeSource: false, removeSourceBestEffort: true,
            renameNoreplace: false, limitations: [], blockers: [] } : null,
        };
      }
      // Ein FTPS-Ziel simuliert den Best-Effort-Commit (überschreibendes RNTO):
      // copyNew bleibt false, copyNewBestEffort trägt die Wirkung samt sichtbarer
      // Einschränkung — exakt die Payload-Form des echten Backends.
      const bestEffortTarget = request.target.provider === "ftps" || webdavUpload;
      const sourceTree = (request.source.provider === "sftp" && mockParams.has("sftpTree")) || (webdavUpload && mockParams.has("webdavTree"))
        ? { digest: "ab".repeat(32), entries: 4, files: 1, directories: 2, bytes: 11, omitted: [{ path: `${request.items[0]}/Link`, reason: "Symlink wird nicht verfolgt" }] } : null;
      const bestEffortReplace = !webdavUpload && bestEffortTarget && conflictMode === "provider-replace";
      const fileConflict = bestEffortReplace || (webdavUpload && conflictMode === "provider-replace");
      const treeConflict = sourceTree && mockParams.has("sftpTreeConflict");
      return {
        skipOnly: false,
        sourceRemovalRequired: request.kind === "move",
        requiresConflictChoice: false,
        sourceReadLimitations: webdavUpload && mockParams.has("longSourceLimits")
          ? ["Begrenzter Quellbaum: 60 ausgelassene Symlinks.", ...Array.from({ length: 60 }, (_, index) => `Nicht übertragen: /Quelle/Auslassen-${index} — Symlink wird nicht verfolgt`)] : request.source.provider === "sftp" && mockParams.has("sftpContents")
          ? ["SFTP-Inhalte · Best Effort: Das erste Öffnen ist nicht atomar an den Pane-Root gebunden; additive lokale Inhalte bis 8 GiB.", ...(sourceTree ? [`Begrenzter Quellbaum: 1 Datei, 2 Ordner; 1 ausgelassener Eintrag: ${sourceTree.omitted[0].path} — Symlink wird nicht verfolgt`] : [])] : sourceTree ? [`Begrenzter Quellbaum: 1 Datei, 2 Ordner; 1 ausgelassener Eintrag: ${sourceTree.omitted[0].path} — Symlink wird nicht verfolgt`] : [],
        sourceTree,
        conflicts: treeConflict ? ["Baum/Kind.txt"] : fileConflict ? ["Beispiel.txt"] : [],
        conflictDetails: treeConflict ? [{ relative_name: "Baum/Kind.txt", source: conflictEntry(request.source.root), target: conflictEntry(request.target.root, 23) }] : fileConflict ? [{ relative_name: "Beispiel.txt", source: conflictEntry(request.source.root), target: conflictEntry(request.target.root, 23) }] : [],
        conflictPreparationId: sourceTree ? "mock-tree-v1" : fileConflict ? "mock-file-conflict" : null,
        keepBothPreparationId: treeConflict ? "mock-tree-v1" : null, keepBothNames: treeConflict ? ["Baum (Kopie)"] : [], keepBothBlocker: treeConflict ? null : "Mock: kein atomarer Kollisionsschutz",
        targetCapabilities: {
          provider: request.target.provider === "webdav" ? "web_dav" : request.target.provider,
          copyNew: !bestEffortTarget,
          copyNewBestEffort: bestEffortTarget,
          replace: false,
          replaceBestEffort: bestEffortReplace,
          removeSource: true,
          removeSourceBestEffort: false,
          renameNoreplace: !bestEffortTarget,
          limitations: webdavUpload ? ["WebDAV-Dateiupload: bedingtes PUT/MOVE ohne Collection-/Löschprobe; native Metadaten ausgelassen. Eine einzelne WebDAV-If-Bedingung bindet den unmittelbaren Elternordner. Vorfahren werden vor und nach der Wirkung geprüft; ein später Vorfahrenwechsel kann bereits wirken und erst im Readback als unklar auffallen."] : bestEffortTarget
            ? ["Neue Dateien laufen als Best-Effort-Commit: Der Server ersetzt bei RNTO vorhandene Ziele."]
            : [],
          blockers: webdavUpload
            ? ["WebDAV: Altziel und Eltern können nicht gemeinsam mit UND gebunden werden; getaggte If-Listen gelten mit ODER.", "Langer Serverbeleg <script>kein Code</script> & unveränderte Altversion. ".repeat(30)]
            : ["Mock: Ersetzen nicht gebunden"],
        },
        sourceCapabilities: request.kind === "move" ? {
          provider: request.source.provider === "webdav" ? "web_dav" : request.source.provider,
          copyNew: true,
          copyNewBestEffort: false,
          replace: false,
          replaceBestEffort: false,
          removeSource: !bestEffortSource,
          removeSourceBestEffort: bestEffortSource,
          renameNoreplace: true,
          limitations: bestEffortSource
            ? ["Quelle wird nach frischem Readback einzeln entfernt"]
            : [],
          blockers: [],
        } : null,
      };
    }
    if (command === "enqueue_provider_file_operation" || command === "enqueue_provider_file_intent") {
      const request = args.request;
      window.__LAST_PROVIDER_FILE_REQUEST__ = structuredClone(request);
      if (command === "enqueue_provider_file_operation" && mockParams.has("ftpsEnqueuePreparationGate")) {
        const before = cancelSyncCalls;
        window.__MOCK_EMIT__("fileop-preparation-started", "FTP-/FTPS-Dateioperation wird erneut vor Einreihung geprüft.");
        window.__AP19_PREPARATION_HELD__ = true;
        await new Promise(resolve => { window.__AP19_RELEASE_PREPARATION__ = resolve; });
        window.__MOCK_EMIT__("fileop-preparation-done", null);
        if (before !== cancelSyncCalls) throw { code: "interrupted", message: "Dateivorbereitung wurde abgebrochen" };
      }
      if (command === "enqueue_provider_file_operation" && (mockParams.has("sftpTree") || mockParams.has("webdavTree")) && request.conflictPreparationId !== "mock-tree-v1") throw { code: "plan_drift", message: "Backend-Baumvorschau fehlt" };
      const entry = queueEntry({
        type: "ProviderFile",
        kind: request.kind === "copy" ? "Copy" : "Move",
        source: queueEndpoint(request.source),
        target: queueEndpoint(request.target),
        items: structuredClone(request.items),
        destination: request.destination,
        overwrite: request.overwrite,
        expected_conflicts: structuredClone(request.expectedConflicts || []),
        source_tree_expected: (mockParams.has("sftpTree") || mockParams.has("webdavTree")) ? { digest: "ab".repeat(32), entries: 4, files: 1, directories: 2, bytes: 11, omitted: [{ path: `${request.items[0]}/Link`, reason: "Symlink wird nicht verfolgt" }] } : null,
        copy_new_best_effort_expected:
          request.copyNewBestEffortExpected ?? null,
        replace_best_effort_expected:
          request.replaceBestEffortExpected ?? null,
        remove_source_best_effort_expected:
          request.removeSourceBestEffortExpected ?? null,
      });
      queue.entries.push(entry); queueEvent(); return entry.operation_id;
    }
    if (command === "check_provider_delete") {
      const request = args.request;
      if (fileActionPreviewDelayMs) await wait(fileActionPreviewDelayMs);
      const mode = ["sftp", "smb"].includes(request.endpoint.provider)
        ? "quarantine"
        : "permanent";
      const records = request.items.map(path => ({
        path, is_dir: false, size: 8, provider_token: null,
        content_digest: Array(32).fill(7),
      }));
      const plan = {
        root: request.endpoint.root,
        root_identity: providerIdentity(request.endpoint),
        items: request.items.map((path, index) => ({
          path, is_dir: false, entries: 1, bytes: 8,
          digest: Array(32).fill(index + 1),
          source_proof: {
            raw_name: [...new TextEncoder().encode(path.split("/").at(-1))],
            provider_token: null, is_dir: false, is_file: true, is_symlink: false,
            size: 8, mtime: null, mode: null, device: null, inode: null,
          },
          records: [records[index]],
        })),
        entries: request.items.length,
        bytes: request.items.length * 8,
        digest: Array(32).fill(4),
      };
      const confirmation = mode === "permanent" && mockParams.has("largeRemoteDelete")
        ? `DAUERHAFT LÖSCHEN ${request.endpoint.provider.toUpperCase()} ${request.endpoint.profileId} ${plan.root} ${plan.items.length} ${plan.entries}`
        : null;
      return {
        capabilities: {
          provider: request.endpoint.provider === "webdav" ? "web_dav" : request.endpoint.provider,
          quarantine: mode === "quarantine",
          permanent: mode === "permanent",
          limitations: mode === "permanent" ? ["Collection-DELETE bleibt Best Effort; <fremder> Bestand kann hinzukommen."] : [],
          blockers: [],
        },
        mode, plan, confirmation,
      };
    }
    if (command === "enqueue_provider_delete") {
      const request = args.request;
      const entry = queueEntry({
        type: "ProviderDelete",
        endpoint: queueEndpoint(request.endpoint),
        // Wie die echte Rust-Serialisierung: `RemoteDeleteMode` ist snake_case
        // ("quarantine"/"permanent"). Ein PascalCase-Mock hatte den UI-Bug
        // maskiert, dass Quarantäneläufe als „Dauerhaft löschen" erschienen.
        mode: request.mode,
        expected_plan: structuredClone(request.expectedPlan),
      });
      queue.entries.push(entry); queueEvent(); return entry.operation_id;
    }
    if (command === "check_local_file_operation") {
      const names = conflictMode === "replace" ? (args.items || []).map(path => path.split("/").pop()) : [];
      const conflicts = names.map(name => (args.destDir.endsWith("/") ? args.destDir : args.destDir + "/") + name);
      return { conflicts, conflictPreparationId: conflicts.length ? "mock-file-conflict" : null,
        conflictDetails: names.map(name => ({ relative_name: name, source: conflictEntry(args.items.find(path => path.endsWith("/" + name)).split("/").slice(0, -1).join("/")), target: conflictEntry(args.destDir, 23) })),
        keepBothPreparationId: conflicts.length ? "mock-keep-both" : null,
        keepBothNames: names.map(name => name + " (Konflikt)"), keepBothBlocker: null,
        sourceReadLimitations: [], sourceTree: null, skipOnly: false, sourceRemovalRequired: true, requiresConflictChoice: false,
        targetCapabilities: { provider: "local", copyNew: true, copyNewBestEffort: false, replace: true, replaceBestEffort: false,
          removeSource: true, removeSourceBestEffort: false, renameNoreplace: true, limitations: [], blockers: [] }, sourceCapabilities: null };
    }
    if (command === "check_conflicts") {
      if (conflictMode !== "replace") return [];
      return (args.items || []).map(path => path.split("/").pop());
    }
    if (command === "write_file_clipboard") {
      clipboardWrites += 1;
      window.__MOCK_CLIPBOARD__({ paths: args.paths, cut: args.cut });
      return true;
    }
    if (command === "read_file_clipboard") return structuredClone(fileClipboard);
    if (command === "clear_file_clipboard_if_unchanged") {
      if (fileClipboard.version !== args.version) return false;
      clipboardClears += 1;
      window.__MOCK_CLIPBOARD__({ paths: [], cut: false });
      return true;
    }
    if (command === "copy_paths" || command === "move_paths") {
      args = args.request;
      actionOrder.push(command);
      const entry = queueEntry({ type: "File", kind: command === "copy_paths" ? "Copy" : "Move",
        items: structuredClone(args.items), destination: args.destDir, overwrite: args.overwrite,
        expected_conflicts: structuredClone(args.expectedConflicts || []) });
      queue.entries.push(entry); queueEvent(); return entry.operation_id;
    }
    if (command === "trash_paths") {
      actionOrder.push(command);
      if (trashDelayMs) await wait(trashDelayMs);
      for (const path of args.items) trashedPaths.add(path);
      return { trashed: args.items.length, failed: 0, unclear: 0, recovery_paths: [], errors: [] };
    }
    // „Ordner anlegen" — lokal wie Remote. Das echte Remote-Backend erhält einen
    // vollständigen Endpunkt statt eines dynamisch zusammengesetzten Commandnamens.
    if (command === "provider_folder_contract") {
      if (mockParams.has("mkdirContractDelay")) await wait(Number(mockParams.get("mkdirContractDelay")));
      if (args.endpoint.provider === "sftp" && mockParams.has("sftpMkdirBlocked")) return { execution_class: null, limitations: [], blockers: ["SFTP-Elternbindung fehlt"] };
      return args.endpoint.provider === "webdav"
        ? { execution_class: "best_effort", limitations: ["Best Effort: genau eine bedingte Ordneranlage, frischer Elternbestand und Readback; keine künstliche Löschprobe. Unklarer Ausgang wird nicht wiederholt. Eine einzelne WebDAV-If-Bedingung bindet den unmittelbaren Elternordner. Vorfahren werden vor und nach der Wirkung geprüft; ein später Vorfahrenwechsel kann bereits wirken und erst im Readback als unklar auffallen."], blockers: [] }
        : { execution_class: args.endpoint.provider === "smb" ? "assured" : "best_effort", limitations: [], blockers: [] };
    }
    if (command === "create_folder" || command === "create_provider_folder") {
      window.__DOPPELDECKER_MKDIR_CALLS__ = (window.__DOPPELDECKER_MKDIR_CALLS__ || 0) + 1;
      if (mkdirDelayMs) await wait(mkdirDelayMs);
      const endpoint = args.endpoint ? structuredClone(args.endpoint) : null;
      const parent = endpoint?.root || args.parent || "";
      const path = parent.endsWith("/") ? parent + args.name : `${parent}/${args.name}`;
      document.documentElement.dataset.lastMkdir = JSON.stringify({
        command, endpoint, parent, name: args.name, path, expectedExecutionClass: args.expectedExecutionClass ?? null });
      if (mockParams.has("mkdirOutcomeUnclear")) {
        throw { code: "outcome_unclear", message: "Mock: Ordneranlage-Ausgang unklar" };
      }
      if (command === "create_folder") return path;
      return {
        path, execution_class: args.expectedExecutionClass,
        limitations: endpoint.provider === "webdav" ? ["Best Effort: eine Ordneranlage mit Readback, keine Löschprobe."] : [],
      };
    }
    if (command === "cancel_sync") {
      cancelSyncCalls += 1;
      actionOrder.push(command);
      if (diffPendingResolve) {
        const resolve = diffPendingResolve;
        diffPendingResolve = null;
        window.__MOCK_EMIT__("file-diff-error", { code: "interrupted", message: "Dateivorbereitung wurde abgebrochen" });
        resolve();
      }
      return null;
    }
    if (command === "record_app_error") {
      appErrorRecords.push({ context: args.context, code: args.code ?? null });
      return null;
    }
    throw {
      code: "mock_unknown_command",
      message: `Unbekannter Mock-Tauri-Command: ${command}`,
    };
  }
  const currentWebview = {
    listen: async (name, callback) => {
      listeners.set(name, callback);
      return () => listeners.delete(name);
    },
  };
  window.__TAURI__ = {
    core: { invoke },
    webview: { getCurrentWebview: () => currentWebview },
  };
  // Testzugang: Die Suite löst damit native Ereignisse aus (z. B. Menüklicks),
  // ohne die App-Verdrahtung zu umgehen — genau wie ein echtes Tauri-Event.
  window.__MOCK_EMIT__ = (name, payload) => listeners.get(name)?.({ payload });
})();
