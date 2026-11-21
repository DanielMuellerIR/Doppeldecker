import { invoke, listen } from "../../frontend/tauri-api.js";

export async function checkRemoteEditResponses() {
  const [entry] = await invoke("list_remote_edits");
  const operation: string | null = entry.lastOperationId;
  // @ts-expect-error Eine neue Bearbeitung kann noch keinen Rückupload haben.
  const definiteOperation: string = entry.lastOperationId;
  return { operation, definiteOperation };
}

export async function checkPrimitiveResponses() {
  const version: string = await invoke("app_version");
  const cancelled: boolean = await invoke("cancel_batch_rename");
  const accepted: null = await invoke("cancel_directory_sizes", { side: "left" });
  const startupError: string | null = await invoke("take_startup_app_error");
  const clipboardPaths: string[] = (await invoke("read_file_clipboard")).paths;
  // @ts-expect-error Die Versionsnummer ist ein String.
  const versionAsNumber: number = await invoke("app_version");
  // @ts-expect-error Ein noch nicht abgerufener Startfehler kann fehlen.
  const definiteError: string = await invoke("take_startup_app_error");
  // @ts-expect-error Die Betriebssystem-Version ist ein opaker String.
  const clipboardVersionAsNumber: number = (await invoke("read_file_clipboard")).version;
  return { version, cancelled, accepted, startupError, clipboardPaths, versionAsNumber, definiteError, clipboardVersionAsNumber };
}

export async function checkQueueResponses() {
  const snapshot = await invoke("list_operation_queue");
  const paused: boolean = (await invoke("set_queue_paused", { paused: true })).paused;
  const revision: number = snapshot.revision;
  const entry = snapshot.entries[0];
  const generatedLabel: string | undefined = entry.generated_source?.label;
  const generatedRecovery: boolean | undefined = entry.generated_source_recovery;
  const fileId: string | undefined = entry.progress.created_directory_dacl_report?.completed[0].file_id;
  // @ts-expect-error Die Anzeigeprojektion erhält große Objekt-IDs als Dezimaltext.
  const numericFileId: number | undefined = entry.progress.created_directory_dacl_report?.completed[0].file_id;
  // @ts-expect-error Die zusätzlichen Felder einer Operation benötigen ihre eigene Typprüfung.
  const operationSource: string = entry.operation.source;
  // @ts-expect-error Nicht jeder Queue-Eintrag hat eine generierte Quelle.
  const definiteRecovery: boolean = entry.generated_source_recovery;
  // @ts-expect-error Auch Queue-Bedienbefehle liefern einen typisierten Snapshot.
  const invalidField = (await invoke("set_queue_stop_on_error", { stop: true })).queue_entries;
  return { paused, revision, generatedLabel, generatedRecovery, fileId, numericFileId, operationSource, definiteRecovery, invalidField };
}

export function checkQueueEvent() {
  return listen("queue-updated", ({ payload }) => {
    const revision: number = payload.revision;
    // @ts-expect-error Ereignisse verwenden denselben Snapshot wie Commands.
    const paused: string = payload.paused;
    return { revision, paused };
  });
}

export async function checkActiveConnectionResponse() {
  const [session] = await invoke("list_active_connections");
  const revision: number = session.revision;
  // @ts-expect-error Der Verbindungsstatus trägt nur einen geheimnisfreien Profilbeleg.
  const connection = session.connection;
  return { revision, connection };
}

export async function checkStartupContext() {
  const dirs = await invoke("default_dirs");
  const safety = await invoke("safety_info");
  const left: string = dirs.left;
  const limit: number = safety.max_delete_fraction;
  const sandbox: string | null = safety.real_enabled ? safety.sandbox : null;
  // @ts-expect-error Im Normalbetrieb besitzt die App keinen Sandboxpfad.
  const definiteSandbox: string = safety.sandbox;
  return { left, limit, sandbox, definiteSandbox };
}

export async function checkArchiveReadResponses() {
  const opened = await invoke("open_archive_read");
  const listing = await invoke("list_archive_directory");
  const previewed = await invoke("preview_archive_entry");
  const origin: string | null = opened.origin;
  const directoryId: string | undefined = listing.rows[0].directoryId;
  const content = previewed.preview.content;
  const previewText: string | null = content.kind === "text" ? content.text : null;
  // @ts-expect-error Virtuelle Verzeichnisse haben keine Eintrags-ID.
  const entryId: string = listing.rows[0].entryId;
  // @ts-expect-error Ein vollständiger Katalog kann ohne Stoppgrund vorliegen.
  const stopMessage: string = opened.listing.catalog.stopReason.message;
  // @ts-expect-error Text- und nicht darstellbare Vorschauen haben keine Bildabmessungen.
  const width: number = content.width;
  return { origin, directoryId, previewText, entryId, stopMessage, width };
}

export async function checkBatchRenameResponses() {
  const preview = await invoke("prepare_batch_rename");
  const report = await invoke("execute_batch_rename", { planId: preview.planId });
  const limitations: string[] = preview.limitations;
  const partial: number = report.partiallyRenamed;
  // @ts-expect-error Nur Remote-Antworten liefern eine Ausführungsklasse.
  const definiteExecutionClass: string = preview.executionClass;
  // @ts-expect-error Auch die Ergebniszeilen tragen den zuletzt bestätigten Namen nur bei Bedarf.
  const definiteName: string = report.rows[0].currentName;
  return { limitations, partial, definiteExecutionClass, definiteName };
}

export async function checkArchivePreparationResponses() {
  const packed = await invoke("prepare_archive_pack");
  const extracted = await invoke("prepare_archive_extract");
  const selected = await invoke("prepare_archive_extract_selection");
  const queueId: string = await invoke("confirm_archive_extract");
  const uploadNotice: string | null = packed.uploadNotice;
  const targetDir: string = extracted.binding.preview.target_dir;
  const snapshotOrigin: string | undefined = selected.binding.preview.snapshot?.origin;
  // @ts-expect-error Die Packvorschau gibt keine Anfragekennung zurück.
  const packRequest = packed.requestId;
  // @ts-expect-error Nicht jede Entpackvorschau hat eine Momentaufnahme.
  const definiteOrigin: string = selected.binding.preview.snapshot.origin;
  // @ts-expect-error Zielpfade der Entpackvorschau verwenden die Serde-Schreibweise des Core.
  const wrongCase = extracted.binding.preview.targetDir;
  return { queueId, uploadNotice, targetDir, snapshotOrigin, packRequest, definiteOrigin, wrongCase };
}

export async function checkDesktopPreferences() {
  const { updateDesktopPreferences } = await import("../../frontend/desktop-preferences-api.js");
  const snapshot = await invoke("desktop_preferences");
  const update = await updateDesktopPreferences({ localFolderSizes: false, diffTool: null });
  const choices = await invoke("diff_tool_status");
  const errorCode: string | undefined = snapshot.loadError?.code;
  const migration: boolean = update.needsMigration;
  // @ts-expect-error Die verschachtelten Patch-Felder verwenden camelCase.
  await updateDesktopPreferences({ byte_units: "binary" });
  // @ts-expect-error Ein Theme muss aus der Rust-Wertemenge stammen.
  await updateDesktopPreferences({ theme: "system" });
  // @ts-expect-error Ein Lesefehler ist ein strukturiertes Objekt oder null.
  const errorText: string | null = snapshot.loadError;
  // @ts-expect-error Der Verfügbarkeitsstatus ist ein boolescher Wert.
  const available: string = choices[0].available;
  return { errorCode, migration, errorText, available };
}

export function checkDesktopPreferencesEvent() {
  return listen("desktop-preferences-changed", ({ payload }) => {
    const revision: number = payload.revision;
    // @ts-expect-error Das Ereignis enthält einen Snapshot in camelCase.
    const preferences = payload.preferences.byte_units;
    return { revision, preferences };
  });
}

export async function checkDuplicateResponses() {
  const preview = await invoke("prepare_duplicate");
  const preparationId: string | null = preview.preparationId;
  const targetName: string = preview.mappings[0].targetName;
  const queueId: string = await invoke("confirm_duplicate");
  const discarded: null = await invoke("discard_duplicate");
  // @ts-expect-error Nicht unterstützte Provider liefern keine Vorbereitungs-ID.
  const definiteId: string = preview.preparationId;
  // @ts-expect-error Namenszuordnungen verwenden camelCase.
  const wrongCase = preview.mappings[0].target_name;
  return { preparationId, targetName, queueId, discarded, definiteId, wrongCase };
}

export async function checkConnectionProfileResponses() {
  const { connectProfile, listProfiles, removeProfile } = await import("../../frontend/connection-profile-api.js");
  const binding = { profileId: "test", profileRevision: 1 };
  const listing = await listProfiles("sftp");
  const saved = await invoke("save_ftps_profile");
  const removed = await removeProfile("webdav", binding);
  const connected = await connectProfile("smb", binding);
  const revision: number = listing[0].revision;
  const hostKey: number = listing[0].connection.host_key.sha256[0];
  const certificate: number | undefined = saved[0].connection.certificate?.sha256[0];
  const origin: string = removed[0].connection.origin;
  const signing: boolean = connected.identity.signing_active;
  // @ts-expect-error Das öffentliche Profil enthält nur eine Schlüsselbundreferenz.
  const password = saved[0].connection.password;
  // @ts-expect-error Ein SMB-Status liefert die SMB-Identität und keinen SSH-Hostkey.
  const smbHostKey = connected.identity.host_key;
  // @ts-expect-error Versionierte Profile sind abgeflacht.
  const wrapped = listing[0].profile;
  return { revision, hostKey, certificate, origin, signing, password, smbHostKey, wrapped };
}

export async function checkFilePropertyResponses() {
  const tags = await invoke("get_file_finder_tags");
  const comments = await invoke("get_file_finder_comments");
  const flags = await invoke("change_file_flag");
  const visibility = await invoke("change_file_visibility");
  const legacyColor: number | undefined = tags[0].state?.legacyColor;
  const requestPermission: boolean = comments[0].permissionRequired;
  const hidden: boolean | undefined = visibility[0].state?.byName;
  const enabled: boolean | null = flags[0].state;
  // @ts-expect-error Fehlgeschlagene Zeilen besitzen keinen bekannten Zustand.
  const known: boolean = flags[0].state;
  // @ts-expect-error Legacy-Farbe wird in camelCase übertragen.
  const legacy = tags[0].state?.legacy_color;
  // @ts-expect-error Die Systemfreigabe gehört nur zur Leseantwort für Kommentare.
  const permission = (await invoke("change_file_finder_comments"))[0].permissionRequired;
  return { legacyColor, requestPermission, hidden, enabled, known, legacy, permission };
}

export async function checkRoutineFileResponses() {
  const { plan } = await invoke("check_provider_delete");
  const bytes: number = plan.bytes;
  const folder = await invoke("create_provider_folder");
  const folderPath: string = folder.path;
  const rename = await invoke("rename_provider_path");
  const newPath: string | null = rename.newPath;
  // @ts-expect-error Die Backend-Belegfelder stehen der UI nicht zur Verfügung.
  const digest = plan.digest;
  // @ts-expect-error Ein Frontendobjekt besitzt keine Backend-Planbindung.
  const fabricatedPlan: typeof plan = { items: [], bytes: 0, entries: 0 };
  // @ts-expect-error Die Antwort der lokalen Ordneranlage besteht nur aus dem Pfad.
  const localPath = (await invoke("create_folder")).path;
  return { bytes, folderPath, newPath, digest, fabricatedPlan, localPath };
}

export async function checkLocalSyncResponses() {
  const plan = await invoke("plan_sync");
  const preview = await invoke("prepare_local_sync");
  const path: string = plan.actions[0].path;
  const digestLength: 32 = preview.plan_digest.length;
  const fsType: string = preview.target_identity.identity.mount.fs_type;
  // @ts-expect-error Die sichtbare Auswahl kann keinen gebundenen Plan herstellen.
  const fabricatedPlan: typeof plan = { actions: [], skip_count: 0, metadata_report: {issues: []} };
  // @ts-expect-error Eine selbst gebildete Bytefolge besitzt keine Vorschau-Bindung.
  const fabricatedDigest: typeof preview.plan_digest = Array(32).fill(0);
  return { path, digestLength, fsType, fabricatedPlan, fabricatedDigest };
}

export async function checkConnectionEditorResponses() {
  const certificate = await invoke("verify_ftps_certificate");
  const hostKey = await invoke("discover_sftp_host_key");
  const discovery = await invoke("discover_smb_server");
  const agents = await invoke("list_sftp_agent_keys");
  const capture = await invoke("capture_connection_secret");
  const keyAlgorithm: string = hostKey.algorithm;
  const byte: number = certificate.sha256[31];
  const encryption: boolean = discovery.encryption_capable;
  const comment: string = agents[0].comment;
  const id: string = capture.secretId;
  // @ts-expect-error Die native Eingabe gibt ausschließlich die Einmal-ID zurück.
  const password = capture.password;
  // @ts-expect-error Die SMB-Antwort verwendet Snake Case.
  const signing = discovery.signingRequired;
  // @ts-expect-error Ein Zertifikat-Pin umfasst genau 32 Byte.
  const outOfRange = certificate.sha256[32];
  return { keyAlgorithm, byte, encryption, comment, id, password, signing, outOfRange };
}

export async function checkProviderTransferPreview() {
  const preview = await invoke("check_provider_file_operation");
  const digest: string | undefined = preview.sourceTree?.digest;
  const omission: string | undefined = preview.sourceTree?.omitted[0].reason;
  const mtime: number | null = preview.conflictDetails[0].target.mtime;
  // @ts-expect-error Proofs bleiben im Backend und fehlen in der Anzeigeprojektion.
  const proof = preview.conflictDetails[0].target_proof;
  // @ts-expect-error Eine Vorschau ohne Quellbaum hat keinen Digest.
  const requiredDigest: string = preview.sourceTree.digest;
  return { digest, omission, mtime, proof, requiredDigest };
}

export async function checkRealModeResponses() {
  const content = await invoke("prepare_content_sync");
  const legacy = await invoke("prepare_webdav_sync");
  const count: number = legacy.plan.delete_count;
  const bytes: 32 = content.digest.length;
  const reopenedDigest: typeof content.digest = (await invoke("content_sync_queue_preview")).digest;
  // @ts-expect-error Der streng gebundene Altpfad konsumiert das Backend-Permit ohne Content-Digest.
  const wrongDigest = legacy.digest;
  // @ts-expect-error Eine Bytefolge aus dem Frontend besitzt keine Vorschau-Bindung.
  const fabricatedDigest: typeof content.digest = Array(32).fill(0);
  return { count, bytes, reopenedDigest, wrongDigest, fabricatedDigest };
}

export async function checkPermissionResponses() {
  const status = await invoke("get_metadata_admin_status");
  const toggled = await invoke("set_metadata_admin_service");
  const capabilities = await invoke("get_file_property_capabilities");
  const [permission] = await invoke("change_file_permissions");
  const [owner] = await invoke("change_file_ownership");
  const mode: number | null = permission.mode;
  const uid: number | null = owner.uid;
  const specialBits: boolean | null = owner.specialBitsChanged;
  const recursive: boolean = capabilities.recursivePermissions;
  const available: boolean = toggled.available;
  // @ts-expect-error Eine ungeklärte Rechteänderung kann ohne Modus zurückkehren.
  const knownMode: number = permission.mode;
  // @ts-expect-error Eigentümer- und Rechteänderung liefern verschiedene Zeilen.
  const impossibleMode = owner.mode;
  // @ts-expect-error SIP ist kein eigener Dienststatus.
  const sip: typeof status.state = "sip";
  return { mode, uid, specialBits, recursive, available, knownMode, impossibleMode, sip };
}

export async function checkSmallControlResponses() {
  const [mount] = await invoke("list_system_mounts");
  const path: string | null = mount.path;
  const [job] = await invoke("list_saved_sync_jobs");
  const revision: number = job.revision;
  const checked = await invoke("check_for_update");
  const found: string | null = checked.version;
  const status = await invoke("prepared_update_status");
  const installed: null = await invoke("install_update");
  // @ts-expect-error Die Auswurfstrategie bleibt im Backend.
  const unmount = mount.unmount;
  // @ts-expect-error Ein gespeicherter Auftrag enthält keine Profilrevision des Endpunkts.
  const profileRevision = job.left.profileRevision;
  return { path, revision, found, status, installed, unmount, profileRevision };
}

export async function checkReadCommandResults() {
  const started = await invoke("start_name_search");
  const [service] = await invoke("discover_network_services");
  const cli = await invoke("cli_install_status");
  const run: number = started.run_id;
  const port: number = service.port;
  const offered: boolean = cli.state === "offer";
  // @ts-expect-error Die Lauf-ID verwendet Snake Case.
  const runId = started.runId;
  // @ts-expect-error mDNS-Funde belegen keine Serveridentität.
  const pin = service.certificate;
  // @ts-expect-error Der CLI-Status veröffentlicht keinen Bundle-Pfad.
  const path = cli.bundle_path;
  return { run, port, offered, runId, pin, path };
}

export async function checkRecoveryAndTrashResponses() {
  const overview = await invoke("inspect_recovery");
  const id: string | undefined = overview.queue?.[0]?.entry.operation_id;
  const journalCount: number | null | undefined = overview.trash.Ok?.[0]?.entry_count;
  const problem: string | undefined = overview.trash.Err?.message;
  const result = await invoke("trash_paths");
  const unclear: number = result.unclear;
  const paths: string[] = result.recovery_paths;
  // @ts-expect-error Eine noch nicht gelesene Fensterqueue kann null sein.
  const queueLength = overview.queue.length;
  // @ts-expect-error Das Papierkorbergebnis bilanziert die Wirkungen ohne separates requested-Feld.
  const requested = result.requested;
  return { id, journalCount, problem, unclear, paths, queueLength, requested };
}
