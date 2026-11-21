//! Gewöhnlicher Dateisync mit wirkungsgebundener Bestätigung in der Fensterqueue.

use crate::bidirectional_commands::{
    resolve_endpoint, resolve_validated_registry_pair, validate_registry_profile,
    BidirectionalEndpointRequest,
};
use crate::frontend_payload::FrontendQueueSnapshot;
use crate::{
    emit_queue_snapshot, operation_queue, start_queue_worker, with_queue_extension,
    with_queue_extension_access, with_reserved_operation, AppError, AppState, MutexExt,
};
use doppeldecker_core::{
    Cancel, ContentSyncPreview, OperationQueue, PreparedContentSync, Progress, QueueEntry,
    QueueExecutionResult, QueueOperation, QueueOperationClass, RemoteProfileBinding, SyncOptions,
};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Manager, WebviewWindow};

pub(crate) struct PendingContentSync {
    prepared: PreparedContentSync,
    sandbox: Option<PathBuf>,
    test_binding: Option<crate::content_sync_sandbox::Binding>,
    operation: QueueOperation,
    pub(crate) profile_bindings: Vec<RemoteProfileBinding>,
}
pub(crate) struct QueuedContentSync {
    prepared: PreparedContentSync,
    effects_confirmed: bool,
    removal_phrase: Option<String>,
    sandbox: Option<PathBuf>,
    test_binding: Option<crate::content_sync_sandbox::Binding>,
    pub(crate) profile_bindings: Vec<RemoteProfileBinding>,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrepareContentSyncRequest {
    source: BidirectionalEndpointRequest,
    target: BidirectionalEndpointRequest,
    options: SyncOptions,
}
fn audit_path(app: &tauri::AppHandle, label: &str) -> Result<PathBuf, AppError> {
    Ok(
        crate::app_config_dir(app)?.join(crate::window_data_file_name(
            "content-sync-runs.jsonl",
            label,
        )?),
    )
}
#[tauri::command]
pub(crate) async fn prepare_content_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: PrepareContentSyncRequest,
) -> Result<ContentSyncPreview, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let ws = state.window(label);
        doppeldecker_core::validate_content_sync_options(&request.options).map_err(AppError::from)?;
        let sandbox = validate_sandbox_endpoints(
            request.source.provider_kind(),
            request.source.root(),
            request.target.provider_kind(),
            request.target.root(),
        )?;
        crate::bidirectional_commands::validate_endpoint_request_root(&request.source)?;
        crate::bidirectional_commands::validate_endpoint_request_root(&request.target)?;
        with_reserved_operation(&app, label, &ws, || {
            let _ = crate::emit_to_window(&window, "sync-preparation-started", "Dateisync: Inhalte, Bestand und Zielwirkungen prüfen. Abbrechen stoppt die Vorbereitung vor der Einreihung.");
            let result = (|| {
            let generation = {
                let mut slot = ws.pending_content_sync.lock_recover();
                slot.take();
                ws.content_sync_generation
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1
            };
            let ((source, source_queue, source_profiles), (target, target_queue, target_profiles)) =
                resolve_validated_registry_pair(
                    &request.source,
                    &request.target,
                    |e| validate_registry_profile(&app, e),
                    |e| resolve_endpoint(&app, &state, e),
                )?;
            let bindings =
                crate::queue_runtime::merge_profile_bindings([source_profiles, target_profiles]);
            let queue = operation_queue(&app, &ws, label)?;
            let source = queue
                .bind_provider_preparation(&source)
                .map_err(AppError::from)?;
            let target = queue
                .bind_provider_preparation(&target)
                .map_err(AppError::from)?;
            let cancel = ws
                .active_cancel
                .lock_recover()
                .as_ref()
                .expect("reserviert")
                .clone();
            let test_binding = crate::content_sync_sandbox::Binding::prepare(sandbox.as_deref(), &target, &cancel)?;
            let prepared = doppeldecker_core::prepare_content_sync_for_queue(
                &queue,
                source,
                target,
                request.options.clone(),
                &cancel,
            )
            .map_err(AppError::from)?;
            let preview = prepared.preview().clone();
            let operation = QueueOperation::ContentSync {
                source: source_queue,
                target: target_queue,
                options: request.options,
                expected_plan: Box::new(preview.plan.clone()),
                binding_digest: preview.digest,
            };
            let _profiles = state.profile_secret_guard.lock_recover();
            crate::queue_runtime::ensure_profile_bindings_current(&app, &bindings)?;
            let mut slot = ws.pending_content_sync.lock_recover();
            if ws
                .content_sync_generation
                .load(std::sync::atomic::Ordering::SeqCst)
                != generation
            {
                return Err(AppError::new(
                    "plan_drift",
                    "Die Sync-Vorbereitung wurde inzwischen verworfen",
                ));
            }
            *slot = Some(PendingContentSync {
                prepared,
                sandbox,
                test_binding,
                operation,
                profile_bindings: bindings,
            });
            Ok(preview)
            })();
            let _ = crate::emit_to_window(&window, "sync-preparation-done", ());
            result
        })
    })
    .await
    .map_err(|_| AppError::new("app", "Sync-Vorbereitung wurde unterbrochen"))?
}
#[tauri::command]
pub(crate) fn discard_content_sync(window: WebviewWindow, state: tauri::State<AppState>) {
    let ws = state.window(window.label());
    let mut slot = ws.pending_content_sync.lock_recover();
    ws.content_sync_generation
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    slot.take();
}
#[tauri::command]
pub(crate) async fn enqueue_content_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    preparation_digest: ContentSyncPreparationDigest,
    effects_confirmed: bool,
    removal_phrase: Option<String>,
) -> Result<String, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let ws = state.window(label);
        let (queue, id) = with_queue_extension_access(&state, &ws, |extension| {
            let mut slot = ws.pending_content_sync.lock_recover();
            let pending = slot
                .as_ref()
                .ok_or_else(|| AppError::new("plan_drift", "Sync-Vorbereitung fehlt"))?;
            crate::queue_runtime::ensure_profile_bindings_current(&app, &pending.profile_bindings)?;
            check_pending_sandbox(&pending.operation, pending.sandbox.as_ref())?;
            if let Some(binding) = &pending.test_binding {
                binding.validate(&Cancel::new())?;
            }
            check_confirmation(
                &pending.prepared,
                preparation_digest,
                effects_confirmed,
                removal_phrase.as_deref(),
            )?;
            let mut runs = ws.content_sync_runs.lock_recover();
            let (queue, entry) = extension.enqueue_with_profile_bindings(
                &app,
                label,
                &pending.operation,
                &pending.profile_bindings,
            )?;
            let pending = slot.take().expect("zuvor geprüft");
            let id = entry.operation_id;
            runs.insert(
                id.clone(),
                QueuedContentSync {
                    prepared: pending.prepared,
                    sandbox: pending.sandbox,
                    test_binding: pending.test_binding,
                    effects_confirmed,
                    removal_phrase,
                    profile_bindings: pending.profile_bindings,
                },
            );
            Ok((queue, id))
        })?;
        emit_queue_snapshot(&app, label, &queue);
        start_queue_worker(app.clone(), label.to_owned());
        Ok(id)
    })
    .await
    .map_err(|_| AppError::new("app", "Sync-Einreihen wurde unterbrochen"))?
}
fn check_confirmation(
    prepared: &PreparedContentSync,
    digest: [u8; 32],
    confirmed: bool,
    removal_phrase: Option<&str>,
) -> Result<(), AppError> {
    if prepared.preview().digest != digest {
        return Err(AppError::new(
            "plan_drift",
            "Eine neuere Sync-Vorschau muss bestätigt werden",
        ));
    }
    if !confirmed {
        return Err(AppError::new(
            "confirmation_mismatch",
            "Die angezeigte Sync-Wirkung wurde nicht bestätigt",
        ));
    }
    if let Some(expected) = prepared.preview().removal_confirmation.as_deref() {
        if removal_phrase != Some(expected) {
            return Err(AppError::new(
                "confirmation_mismatch",
                "Der angezeigte Entfernungstext stimmt nicht überein",
            ));
        }
    }
    Ok(())
}
pub(crate) type ContentSyncQueuePreview = ContentSyncPreview;
pub(crate) type ContentSyncPreparationDigest = [u8; 32];

#[tauri::command]
pub(crate) fn content_sync_queue_preview(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
) -> Result<ContentSyncQueuePreview, AppError> {
    let state = app.state::<AppState>();
    let ws = state.window(window.label());
    let runs = ws.content_sync_runs.lock_recover();
    let pending = runs.get(&operation_id).ok_or_else(|| {
        AppError::new(
            "plan_drift",
            "Sync-Queue-Vorbereitung fehlt; Auftrag neu prüfen",
        )
    })?;
    crate::queue_runtime::ensure_profile_bindings_current(&app, &pending.profile_bindings)?;
    Ok(pending.prepared.preview().clone())
}
#[tauri::command]
pub(crate) fn approve_content_sync_queue_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
    preparation_digest: ContentSyncPreparationDigest,
    effects_confirmed: bool,
    removal_phrase: Option<String>,
) -> Result<FrontendQueueSnapshot, AppError> {
    let state = app.state::<AppState>();
    let label = window.label();
    let ws = state.window(label);
    let queue = operation_queue(&app, &ws, label)?;
    with_queue_extension(&state, &ws, || {
        let mut runs = ws.content_sync_runs.lock_recover();
        let pending = runs
            .get_mut(&operation_id)
            .ok_or_else(|| AppError::new("plan_drift", "Sync-Queue-Vorbereitung fehlt"))?;
        crate::queue_runtime::ensure_profile_bindings_current(&app, &pending.profile_bindings)?;
        let entry = queue
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.operation_id == operation_id)
            .ok_or_else(|| AppError::new("plan_drift", "Queueeintrag fehlt"))?;
        check_pending_sandbox(&entry.operation, pending.sandbox.as_ref())?;
        if let Some(binding) = &pending.test_binding {
            binding.validate(&Cancel::new())?;
        }
        check_confirmation(
            &pending.prepared,
            preparation_digest,
            effects_confirmed,
            removal_phrase.as_deref(),
        )?;
        queue
            .confirm_content_sync_preparation(&operation_id, &pending.prepared)
            .map_err(AppError::from)?;
        pending.effects_confirmed = effects_confirmed;
        pending.removal_phrase = removal_phrase;
        Ok(())
    })?;
    emit_queue_snapshot(&app, label, &queue);
    start_queue_worker(app.clone(), label.to_owned());
    Ok(queue.snapshot().into())
}
pub(crate) fn execute_queue_entry(
    app: &tauri::AppHandle,
    label: &str,
    queue: &OperationQueue,
    entry: &QueueEntry,
    cancel: &Cancel,
    progress: &mut dyn FnMut(&Progress),
) -> std::io::Result<QueueExecutionResult> {
    let state = app.state::<AppState>();
    let ws = state.window(label);
    let pending = ws
        .content_sync_runs
        .lock_recover()
        .remove(&entry.operation_id);
    let Some(pending) = pending else {
        let sandbox = check_pending_sandbox(
            &entry.operation,
            crate::sandbox_root().map_err(core_error)?.as_ref(),
        )
        .map_err(core_error)?;
        let prepared = doppeldecker_core::queue::prepare_content_sync_queue_entry(
            queue,
            entry,
            Arc::new(crate::sftp_commands::KeyringResolver),
            cancel,
        )?;
        let test_binding = crate::content_sync_sandbox::Binding::prepare(
            sandbox.as_deref(),
            prepared.target_endpoint(),
            cancel,
        )
        .map_err(core_error)?;
        let _profiles = state.profile_secret_guard.lock_recover();
        crate::queue_runtime::ensure_profile_bindings_current(app, &entry.profile_bindings)
            .map_err(core_error)?;
        ws.content_sync_runs.lock_recover().insert(
            entry.operation_id.clone(),
            QueuedContentSync {
                prepared,
                sandbox,
                test_binding,
                effects_confirmed: false,
                removal_phrase: None,
                profile_bindings: entry.profile_bindings.clone(),
            },
        );
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::Sync,
            reason:
                "Dateisync ist frisch vorbereitet; die vollständige Wirkung in der Queue bestätigen"
                    .into(),
        });
    };
    crate::queue_runtime::ensure_profile_bindings_current(app, &pending.profile_bindings)
        .map_err(core_error)?;
    check_pending_sandbox(&entry.operation, pending.sandbox.as_ref()).map_err(core_error)?;
    if let Some(binding) = &pending.test_binding {
        binding.validate(cancel).map_err(core_error)?;
    }
    doppeldecker_core::queue::execute_prepared_content_sync_queue_entry_with_removal_phrase(
        queue,
        entry,
        pending.prepared,
        pending.effects_confirmed,
        pending.removal_phrase.as_deref(),
        &audit_path(app, label).map_err(core_error)?,
        cancel,
        progress,
    )
}
fn core_error(error: AppError) -> std::io::Error {
    let code = serde_json::from_value(serde_json::Value::String(error.code))
        .unwrap_or(doppeldecker_core::ErrorCode::Io);
    doppeldecker_core::coded_error(std::io::ErrorKind::Other, code, error.message)
}

pub(crate) fn validate_sandbox_endpoints(
    source: doppeldecker_core::ProviderKind,
    source_root: &str,
    target: doppeldecker_core::ProviderKind,
    target_root: &str,
) -> Result<Option<PathBuf>, AppError> {
    let sandbox = crate::sandbox_root()?;
    if let Some(root) = sandbox.as_ref() {
        for (kind, path) in [(source, source_root), (target, target_root)] {
            if kind == doppeldecker_core::ProviderKind::Local {
                let path = std::fs::canonicalize(doppeldecker_core::expand_home_path(path))
                    .map_err(AppError::from)?;
                if path == *root || !path.starts_with(root) {
                    return Err(AppError::new(
                        "sandbox_violation",
                        "Lokale Sync-Wurzeln müssen unter der markierten Testsandbox liegen",
                    ));
                }
            }
        }
    }
    Ok(sandbox)
}
fn check_pending_sandbox(
    operation: &QueueOperation,
    expected: Option<&PathBuf>,
) -> Result<Option<PathBuf>, AppError> {
    let QueueOperation::ContentSync { source, target, .. } = operation else {
        unreachable!()
    };
    let current = validate_sandbox_endpoints(
        queue_endpoint_kind(source),
        source.root(),
        queue_endpoint_kind(target),
        target.root(),
    )?;
    if current.as_ref() != expected {
        return Err(AppError::new(
            "sandbox_violation",
            "Der markierte Testbereich änderte sich seit der Sync-Vorschau",
        ));
    }
    Ok(current)
}

pub(crate) fn queue_endpoint_kind(
    endpoint: &doppeldecker_core::BidirectionalQueueEndpoint,
) -> doppeldecker_core::ProviderKind {
    use doppeldecker_core::{BidirectionalQueueEndpoint as E, ProviderKind as K};
    match endpoint {
        E::Local { .. } => K::Local,
        E::Sftp { .. } => K::Sftp,
        E::Ftps { .. } => K::Ftps,
        E::WebDav { .. } => K::WebDav,
        E::Smb { .. } => K::Smb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_mode_previews_match_browser_wire_fixture() {
        use doppeldecker_core::{
            sync::SyncExecutionClass, ProviderKind, RealRunPreview, RootIdentity, SftpHostKeyPin,
            SftpRootIdentity, SftpServerIdentity, SyncPlan,
        };

        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../../tools/tests/real-mode-wire.json")).unwrap();
        let local: serde_json::Value =
            serde_json::from_str(include_str!("../../tools/tests/local-sync-wire.json")).unwrap();
        let content = &wire["content"];
        let request: PrepareContentSyncRequest =
            serde_json::from_value(content["request"].clone()).unwrap();
        assert_eq!(request.source.provider_kind(), ProviderKind::Sftp);
        assert_eq!(request.target.provider_kind(), ProviderKind::Local);
        assert_eq!(request.options, SyncOptions::default());
        let plan: SyncPlan = serde_json::from_value(local["plan"].clone()).unwrap();
        let target_identity: RootIdentity =
            serde_json::from_value(local["preview"]["target_identity"].clone()).unwrap();
        let digest: ContentSyncPreparationDigest = [31; 32];
        let preview = ContentSyncPreview {
            execution_class: SyncExecutionClass::BestEffort,
            source_root: "/".into(),
            target_root: "/mock/ziel".into(),
            source_provider: ProviderKind::Sftp,
            source_identity: RootIdentity::Sftp(SftpRootIdentity {
                server: SftpServerIdentity {
                    host: "sftp.example.invalid".into(),
                    port: 22,
                    host_key: SftpHostKeyPin {
                        algorithm: "ssh-ed25519".into(),
                        sha256: [9; 32],
                    },
                    canonical_root: "/daten".into(),
                    sftp_version: 3,
                    extensions_digest: [6; 32],
                },
                virtual_root: "/".into(),
                session_generation: 1,
            }),
            target_identity: target_identity.clone(),
            target_provider: ProviderKind::Local,
            options: request.options,
            plan: plan.clone(),
            digest,
            expires_unix_ms: 4_102_444_800_000,
            limitations: vec!["Künstlicher Hinweis für den gebundenen Dateisync.".into()],
            quarantined_replacements: 0,
            quarantined_removals: 0,
            quarantine_run: None,
            restore_probe_ok: false,
            local_execution: None,
            irreversible_replacements: 0,
            irreversible_removals: 0,
            irreversible_removal_bytes: 0,
            blocked_removals: 0,
            removal_confirmation: None,
            directory_stream_effects: Vec::new(),
            directory_dacl_effects: Vec::new(),
            file_dacl_effects: Vec::new(),
            created_directory_dacl_effects: Vec::new(),
            created_directory_before_children_effects: Vec::new(),
        };
        let mut expected_content = content["preview"].clone();
        expected_content["target_identity"] = local["preview"]["target_identity"].clone();
        expected_content["plan"] = local["plan"].clone();
        assert_eq!(serde_json::to_value(preview).unwrap(), expected_content);

        let normal = &local["preview"];
        let mut expected_legacy = normal.clone();
        expected_legacy["plan"] = local["plan"].clone();
        expected_legacy["prepared_at_unix_ms"] = wire["legacy"]["prepared_at_unix_ms"].clone();
        expected_legacy["expires_at_unix_ms"] = wire["legacy"]["expires_at_unix_ms"].clone();
        expected_legacy
            .as_object_mut()
            .unwrap()
            .remove("local_execution");
        let legacy = RealRunPreview {
            source_identity: serde_json::from_value(normal["source_identity"].clone()).unwrap(),
            target_identity,
            quarantine_identity: serde_json::from_value(normal["quarantine_identity"].clone())
                .unwrap(),
            source_listing: serde_json::from_value(normal["source_listing"].clone()).unwrap(),
            target_listing: serde_json::from_value(normal["target_listing"].clone()).unwrap(),
            plan,
            options: SyncOptions::default(),
            impact: serde_json::from_value(normal["impact"].clone()).unwrap(),
            plan_digest: [23; 32],
            confirmation_phrase: "REAL ziel ABC123".into(),
            scope_warnings: vec!["Künstlicher Bereichshinweis".into()],
            prepared_at_unix_ms: 4_102_444_200_000,
            expires_at_unix_ms: 4_102_444_800_000,
            restore_probe_ok: false,
            backup_confirmation_required: false,
            provider_capabilities: None,
            local_execution: None,
        };
        assert_eq!(serde_json::to_value(legacy).unwrap(), expected_legacy);
    }

    #[test]
    fn frontend_request_akzeptiert_profilrevision_und_sync_optionen() {
        let request: PrepareContentSyncRequest = serde_json::from_value(serde_json::json!({
            "source": { "provider": "local", "root": "/tmp/quelle" },
            "target": { "provider": "sftp", "root": "/ziel", "profileId": "profil", "profileRevision": 7 },
            "options": { "mode": "Update", "compare": "ContentHash", "metadata": "Portable", "mtime_tolerance": 2 }
        })).unwrap();
        assert_eq!(
            request.source.provider_kind(),
            doppeldecker_core::ProviderKind::Local
        );
        assert_eq!(
            request.target.provider_kind(),
            doppeldecker_core::ProviderKind::Sftp
        );
        assert_eq!(request.options.mode, doppeldecker_core::SyncMode::Update);
        assert_eq!(request.options.mtime_tolerance, 2);
    }
    #[test]
    fn generationszaehler_und_content_sync_slots_gehoeren_zum_fenster() {
        let app = AppState::default();
        let a = app.window("main");
        let b = app.window("window-2");
        a.content_sync_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            a.content_sync_generation
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            b.content_sync_generation
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(a.pending_content_sync.lock_recover().is_none());
        assert!(b.content_sync_runs.lock_recover().is_empty());
    }
}

/// Der Aufrufer bestätigt nur den Queueeintrag; Profil und Pfad stammen immer
/// aus dessen gespeicherter, revisionsgebundener SMB-Zielabsicht.
#[tauri::command]
pub(crate) async fn acknowledge_directory_dacl_outcome(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
    possible_effect_reviewed: bool,
) -> Result<FrontendQueueSnapshot, AppError> {
    if !possible_effect_reviewed {
        return Err(AppError::new(
            "confirmation_mismatch",
            "Der mögliche Rechtezustand wurde nicht ausdrücklich geprüft und bestätigt",
        ));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let ws = state.window(label);
        with_reserved_operation(&app, label, &ws, || {
            let queue = operation_queue(&app, &ws, label)?;
            let entry = queue
                .snapshot()
                .entries
                .into_iter()
                .find(|entry| entry.operation_id == operation_id)
                .ok_or_else(|| AppError::new("not_found", "Queueeintrag fehlt"))?;
            if !matches!(
                entry.state,
                doppeldecker_core::QueueState::Failed | doppeldecker_core::QueueState::Interrupted
            ) || !entry.directory_dacl_recovery
            {
                return Err(AppError::new(
                    "replan_required",
                    "Der Queueeintrag besitzt keine offene Verzeichnis-DACL-Recovery",
                ));
            }
            let QueueOperation::ContentSync { target, .. } = &entry.operation else {
                return Err(AppError::new(
                    "replan_required",
                    "Verzeichnis-DACL-Recovery benötigt den ursprünglichen Inhaltssync",
                ));
            };
            let doppeldecker_core::BidirectionalQueueEndpoint::Smb { connection, root } =
                target.as_ref()
            else {
                return Err(AppError::new(
                    "plan_drift",
                    "Das gespeicherte Rechteziel ist kein SMB-Endpunkt",
                ));
            };
            let profiles = doppeldecker_core::load_smb_profiles_versioned(
                &crate::app_config_dir(&app)?.join("smb-profiles.json"),
            )
            .map_err(AppError::from)?;
            let binding = profiles
                .iter()
                .filter(|profile| profile.profile.connection == **connection)
                .find_map(|profile| {
                    entry.profile_bindings.iter().find(|binding| {
                        binding.protocol == doppeldecker_core::RemoteProfileProtocol::Smb
                            && binding.profile_id == profile.profile.id
                    })
                })
                .ok_or_else(|| {
                    AppError::new(
                        "plan_drift",
                        "Das gespeicherte SMB-Ziel besitzt kein unverändertes gebundenes Profil",
                    )
                })?;
            let cached = crate::smb_commands::cached_connection_bound(
                &app,
                &state,
                &binding.profile_id,
                Some(binding.revision),
            )?;
            let _profiles = state.profile_secret_guard.lock_recover();
            crate::queue_runtime::ensure_profile_bindings_current(
                &app,
                std::slice::from_ref(binding),
            )?;
            let current = crate::smb_commands::find_versioned_profile(&app, &binding.profile_id)?;
            if current.profile.connection != **connection || cached.binding != *binding {
                return Err(AppError::new(
                    "plan_drift",
                    "SMB-Profil oder Pin änderte sich seit dem gespeicherten Inhaltssync",
                ));
            }
            let endpoint = doppeldecker_core::ProviderEndpoint::smb(
                cached.connection,
                std::path::Path::new(root),
            )
            .map_err(AppError::from)?;
            doppeldecker_core::directory_dacl_queue::acknowledge_directory_dacl_outcome(
                &queue,
                &operation_id,
                &endpoint,
                possible_effect_reviewed,
            )
            .map_err(AppError::from)?;
            emit_queue_snapshot(&app, label, &queue);
            Ok(queue.snapshot().into())
        })
    })
    .await
    .map_err(|_| AppError::new("app", "Rechteprüfung wurde unterbrochen"))?
}

/// Der Aufrufer bestätigt nur den Queueeintrag; Profil und Pfad stammen immer
/// aus dessen gespeicherter, revisionsgebundener SMB-Zielabsicht.
#[tauri::command]
pub(crate) async fn acknowledge_file_dacl_outcome(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
    possible_effect_reviewed: bool,
) -> Result<FrontendQueueSnapshot, AppError> {
    if !possible_effect_reviewed {
        return Err(AppError::new(
            "confirmation_mismatch",
            "Der mögliche Rechtezustand wurde nicht ausdrücklich geprüft und bestätigt",
        ));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let ws = state.window(label);
        with_reserved_operation(&app, label, &ws, || {
            let queue = operation_queue(&app, &ws, label)?;
            let entry = queue
                .snapshot()
                .entries
                .into_iter()
                .find(|entry| entry.operation_id == operation_id)
                .ok_or_else(|| AppError::new("not_found", "Queueeintrag fehlt"))?;
            if !matches!(
                entry.state,
                doppeldecker_core::QueueState::Failed | doppeldecker_core::QueueState::Interrupted
            ) || !entry.file_dacl_recovery
            {
                return Err(AppError::new(
                    "replan_required",
                    "Der Queueeintrag besitzt keine offene Datei-DACL-Recovery",
                ));
            }
            let QueueOperation::ContentSync { target, .. } = &entry.operation else {
                return Err(AppError::new(
                    "replan_required",
                    "Datei-DACL-Recovery benötigt den ursprünglichen Inhaltssync",
                ));
            };
            let doppeldecker_core::BidirectionalQueueEndpoint::Smb { connection, root } =
                target.as_ref()
            else {
                return Err(AppError::new(
                    "plan_drift",
                    "Das gespeicherte Rechteziel ist kein SMB-Endpunkt",
                ));
            };
            let profiles = doppeldecker_core::load_smb_profiles_versioned(
                &crate::app_config_dir(&app)?.join("smb-profiles.json"),
            )
            .map_err(AppError::from)?;
            let binding = profiles
                .iter()
                .filter(|profile| profile.profile.connection == **connection)
                .find_map(|profile| {
                    entry.profile_bindings.iter().find(|binding| {
                        binding.protocol == doppeldecker_core::RemoteProfileProtocol::Smb
                            && binding.profile_id == profile.profile.id
                    })
                })
                .ok_or_else(|| {
                    AppError::new(
                        "plan_drift",
                        "Das gespeicherte SMB-Ziel besitzt kein unverändertes gebundenes Profil",
                    )
                })?;
            let cached = crate::smb_commands::cached_connection_bound(
                &app,
                &state,
                &binding.profile_id,
                Some(binding.revision),
            )?;
            let _profiles = state.profile_secret_guard.lock_recover();
            crate::queue_runtime::ensure_profile_bindings_current(
                &app,
                std::slice::from_ref(binding),
            )?;
            let current = crate::smb_commands::find_versioned_profile(&app, &binding.profile_id)?;
            if current.profile.connection != **connection || cached.binding != *binding {
                return Err(AppError::new(
                    "plan_drift",
                    "SMB-Profil oder Pin änderte sich seit dem gespeicherten Inhaltssync",
                ));
            }
            let endpoint = doppeldecker_core::ProviderEndpoint::smb(
                cached.connection,
                std::path::Path::new(root),
            )
            .map_err(AppError::from)?;
            doppeldecker_core::file_dacl_queue::acknowledge_file_dacl_outcome(
                &queue,
                &operation_id,
                &endpoint,
                possible_effect_reviewed,
            )
            .map_err(AppError::from)?;
            emit_queue_snapshot(&app, label, &queue);
            Ok(queue.snapshot().into())
        })
    })
    .await
    .map_err(|_| AppError::new("app", "Rechteprüfung wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn acknowledge_created_directory_dacl_outcome(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
    possible_effect_reviewed: bool,
) -> Result<FrontendQueueSnapshot, AppError> {
    if !possible_effect_reviewed {
        return Err(AppError::new(
            "confirmation_mismatch",
            "Der mögliche Rechtezustand wurde nicht ausdrücklich geprüft und bestätigt",
        ));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let ws = state.window(label);
        with_reserved_operation(&app, label, &ws, || {
            let queue = operation_queue(&app, &ws, label)?;
            let entry = queue
                .snapshot()
                .entries
                .into_iter()
                .find(|entry| entry.operation_id == operation_id)
                .ok_or_else(|| AppError::new("not_found", "Queueeintrag fehlt"))?;
            if !matches!(
                entry.state,
                doppeldecker_core::QueueState::Failed | doppeldecker_core::QueueState::Interrupted
            ) || !entry.created_directory_dacl_recovery
            {
                return Err(AppError::new(
                    "replan_required",
                    "Der Queueeintrag besitzt keine offene Neuordner-DACL-Recovery",
                ));
            }
            let QueueOperation::ContentSync { target, .. } = &entry.operation else {
                return Err(AppError::new(
                    "replan_required",
                    "Neuordner-DACL-Recovery benötigt den ursprünglichen Inhaltssync",
                ));
            };
            let doppeldecker_core::BidirectionalQueueEndpoint::Smb { connection, root } =
                target.as_ref()
            else {
                return Err(AppError::new(
                    "plan_drift",
                    "Das gespeicherte Rechteziel ist kein SMB-Endpunkt",
                ));
            };
            let profiles = doppeldecker_core::load_smb_profiles_versioned(
                &crate::app_config_dir(&app)?.join("smb-profiles.json"),
            )
            .map_err(AppError::from)?;
            let binding = profiles
                .iter()
                .filter(|profile| profile.profile.connection == **connection)
                .find_map(|profile| {
                    entry.profile_bindings.iter().find(|binding| {
                        binding.protocol == doppeldecker_core::RemoteProfileProtocol::Smb
                            && binding.profile_id == profile.profile.id
                    })
                })
                .ok_or_else(|| {
                    AppError::new(
                        "plan_drift",
                        "Das gespeicherte SMB-Ziel besitzt kein unverändertes gebundenes Profil",
                    )
                })?;
            let cached = crate::smb_commands::cached_connection_bound(
                &app,
                &state,
                &binding.profile_id,
                Some(binding.revision),
            )?;
            let _profiles = state.profile_secret_guard.lock_recover();
            crate::queue_runtime::ensure_profile_bindings_current(
                &app,
                std::slice::from_ref(binding),
            )?;
            let current = crate::smb_commands::find_versioned_profile(&app, &binding.profile_id)?;
            if current.profile.connection != **connection || cached.binding != *binding {
                return Err(AppError::new(
                    "plan_drift",
                    "SMB-Profil oder Pin änderte sich seit dem gespeicherten Inhaltssync",
                ));
            }
            let endpoint = doppeldecker_core::ProviderEndpoint::smb(
                cached.connection,
                std::path::Path::new(root),
            )
            .map_err(AppError::from)?;
            doppeldecker_core::acknowledge_created_directory_dacl_outcome(
                &queue,
                &operation_id,
                &endpoint,
                possible_effect_reviewed,
            )
            .map_err(AppError::from)?;
            emit_queue_snapshot(&app, label, &queue);
            Ok(queue.snapshot().into())
        })
    })
    .await
    .map_err(|_| AppError::new("app", "Rechteprüfung wurde unterbrochen"))?
}
