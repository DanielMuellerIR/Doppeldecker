//! Dünne Tauri-Anbindung des GUI-freien SMB3-Kerns.
//!
//! Profile enthalten nur Server-, Freigabe- und Schlüsselbundreferenzen. Passwort,
//! verschlüsselte Sitzung, Tree-ID und Realmodus-Permit bleiben ausschließlich im RAM.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use doppeldecker_core::{
    list_quarantine_entries_for_target, list_quarantine_runs,
    load_smb_profiles as core_load_profiles, prepare_smb_download, prepare_smb_queue_entry,
    prepare_smb_upload, purge_quarantine_run_for_target, restore_quarantine_entry_for_target,
    Entry, OperationQueue, Progress, QuarantineEntry, QuarantineRun, QueueEntry,
    QueueExecutionResult, QueueOperation, QueueSnapshot, RealRunPreview, RemoteCapabilities,
    RemoteProfileBinding, RemoteProfileProtocol, SftpQueueDirection, SftpQueuePreparation,
    SmbProfile, SmbServerIdentity, SmbVfs, SyncOptions, SyncPlan, VersionedProfile,
};
use tauri::{Manager, State, WebviewWindow};

use crate::quarantine_commands::{restore_response, QuarantineRestoreResponse};

use crate::queue_runtime::{
    cached_remote_connection, enqueue_bound_pending, CachedRemoteConnection, PendingRemoteRealRun,
    QueueTerminalEvent,
};
use crate::sftp_commands::KeyringResolver;
use crate::{
    app_config_dir, enqueue_operation_with_profile_bindings, start_queue_worker,
    take_pending_for_operation, with_queue_extension, with_reserved_operation, AppError, AppState,
    MutexExt,
};
use crate::{emit_queue_snapshot, finish_operation, operation_queue, reserve_operation};

const PROFILE_FILE: &str = "smb-profiles.json";
const AUDIT_FILE: &str = "smb-real-runs.jsonl";

#[tauri::command]
pub(crate) async fn discover_smb_server(
    host: String,
    port: u16,
) -> Result<doppeldecker_core::SmbServerDiscovery, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        doppeldecker_core::discover_smb_server(&host, port).map_err(AppError::from)
    })
    .await
    .map_err(|_| AppError::new("app", "SMB-Worker wurde unterbrochen"))?
}

fn profile_path(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    Ok(app_config_dir(app)?.join(PROFILE_FILE))
}

fn audit_path(app: &tauri::AppHandle, label: &str) -> Result<PathBuf, AppError> {
    Ok(app_config_dir(app)?.join(crate::window_data_file_name(AUDIT_FILE, label)?))
}

pub(crate) fn find_profile(
    app: &tauri::AppHandle,
    profile_id: &str,
) -> Result<SmbProfile, AppError> {
    core_load_profiles(&profile_path(app)?)
        .map_err(AppError::from)?
        .into_iter()
        .find(|profile| profile.id == profile_id)
        .ok_or_else(|| AppError::new("not_found", "SMB-Profil wurde nicht gefunden"))
}

pub(crate) fn find_versioned_profile(
    app: &tauri::AppHandle,
    profile_id: &str,
) -> Result<VersionedProfile<SmbProfile>, AppError> {
    doppeldecker_core::load_smb_profiles_versioned(&profile_path(app)?)
        .map_err(AppError::from)?
        .into_iter()
        .find(|profile| profile.profile.id == profile_id)
        .ok_or_else(|| AppError::new("not_found", "SMB-Profil wurde nicht gefunden"))
}

fn connect_fresh_bound(
    app: &tauri::AppHandle,
    profile_id: &str,
    profile_revision: Option<u64>,
) -> Result<(Arc<SmbVfs>, VersionedProfile<SmbProfile>), AppError> {
    let profile = find_versioned_profile(app, profile_id)?;
    let binding =
        RemoteProfileBinding::new(RemoteProfileProtocol::Smb, profile_id, profile.revision)
            .map_err(AppError::from)?;
    crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &binding)?;
    let connection = SmbVfs::connect(
        profile.profile.connection.clone(),
        Arc::new(KeyringResolver),
    )
    .map_err(AppError::from)?;
    Ok((Arc::new(connection), profile))
}

pub(crate) fn current_profile_revision(
    app: &tauri::AppHandle,
    profile_id: &str,
) -> Result<Option<u64>, AppError> {
    doppeldecker_core::latest_smb_profile_revision(&profile_path(app)?, profile_id)
        .map_err(AppError::from)
}

pub(crate) fn cached_connection_bound(
    app: &tauri::AppHandle,
    state: &AppState,
    profile_id: &str,
    profile_revision: Option<u64>,
) -> Result<CachedRemoteConnection<SmbVfs>, AppError> {
    cached_remote_connection(
        app,
        state,
        &state.smb_connections,
        crate::queue_runtime::RemoteConnectionRequest {
            protocol: RemoteProfileProtocol::Smb,
            profile_id,
            requested_revision: profile_revision,
        },
        || find_versioned_profile(app, profile_id),
        |profile| {
            SmbVfs::connect(profile.connection, Arc::new(KeyringResolver))
                .map(Arc::new)
                .map_err(AppError::from)
        },
    )
    .map(|(cached, _)| cached)
}

#[tauri::command]
pub(crate) async fn list_smb_profiles(
    app: tauri::AppHandle,
) -> Result<Vec<VersionedProfile<SmbProfile>>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        doppeldecker_core::load_smb_profiles_versioned(&profile_path(&app)?).map_err(AppError::from)
    })
    .await
    .map_err(|_| AppError::new("app", "SMB-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn save_smb_profile(
    app: tauri::AppHandle,
    window: WebviewWindow,
    mut profile: SmbProfile,
    secret_id: Option<String>,
    profile_revision: Option<u64>,
) -> Result<Vec<VersionedProfile<SmbProfile>>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        profile.validate().map_err(AppError::from)?;
        let id = profile.id.clone();
        let reference = profile.connection.password_reference.clone();
        let path = profile_path(&app)?;
        let save_result = crate::sftp_commands::save_profile_with_secret(
            &state,
            crate::sftp_commands::ConnectionSecretPurpose {
                owner: window.label(),
                protocol: "smb",
                profile_id: &id,
            },
            crate::sftp_commands::ProfileSecretUpdate {
                reference: Some(&reference),
                secret_id,
            },
            |fresh| {
                if let Some(fresh) = fresh {
                    profile.connection.password_reference = fresh;
                }
                profile
            },
            |profile| {
                doppeldecker_core::save_smb_profile_versioned_if_current(
                    &path,
                    profile,
                    profile_revision,
                )
                .map_err(AppError::from)
            },
            || {
                core_load_profiles(&path)
                    .map_err(AppError::from)
                    .map(|profiles| profiles.into_iter().find(|profile| profile.id == id))
            },
            || {
                state.smb_connections.lock_recover().remove(&id);
                crate::queue_runtime::invalidate_remote_profile(
                    &app,
                    &state,
                    RemoteProfileProtocol::Smb,
                    &id,
                );
                if let Ok(Some(revision)) = current_profile_revision(&app, &id) {
                    let binding =
                        RemoteProfileBinding::new(RemoteProfileProtocol::Smb, &id, revision)
                            .expect("bereits validierte Profil-ID");
                    crate::connection_state_commands::emit_connection_update(
                        &app,
                        &binding,
                        crate::connection_state_commands::ConnectionChangeKind::Saved,
                    );
                }
            },
        );
        save_result.map(|mutation| mutation.profiles)
    })
    .await
    .map_err(|_| AppError::new("app", "SMB-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn remove_smb_profile(
    app: tauri::AppHandle,
    window: WebviewWindow,
    profile_id: String,
    profile_revision: u64,
) -> Result<Vec<VersionedProfile<SmbProfile>>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let path = profile_path(&app)?;
        let attempt = crate::sftp_commands::remove_profile_with_readback(
            &state,
            window.label(),
            || {
                core_load_profiles(&path)
                    .map_err(AppError::from)
                    .map(|profiles| {
                        profiles
                            .into_iter()
                            .find(|profile| profile.id == profile_id)
                    })
            },
            || {
                doppeldecker_core::remove_smb_profile_versioned_if_current(
                    &path,
                    &profile_id,
                    profile_revision,
                )
                .map_err(AppError::from)
            },
            || {
                state.smb_connections.lock_recover().remove(&profile_id);
                crate::queue_runtime::invalidate_remote_profile(
                    &app,
                    &state,
                    RemoteProfileProtocol::Smb,
                    &profile_id,
                );
                let binding = RemoteProfileBinding::new(
                    RemoteProfileProtocol::Smb,
                    &profile_id,
                    profile_revision,
                )
                .expect("bereits validierte Profil-ID");
                crate::connection_state_commands::emit_connection_update(
                    &app,
                    &binding,
                    crate::connection_state_commands::ConnectionChangeKind::Removed,
                );
            },
        );
        attempt.map(|mutation| mutation.profiles)
    })
    .await
    .map_err(|_| AppError::new("app", "SMB-Worker wurde unterbrochen"))?
}

#[derive(serde::Serialize)]
pub(crate) struct SmbConnectionStatus {
    profile_revision: u64,
    identity: SmbServerIdentity,
    capabilities: RemoteCapabilities,
    quarantine_capabilities:
        doppeldecker_core::remote_real_mode::ProviderQuarantineManagementCapabilities,
}

#[tauri::command]
pub(crate) async fn connect_smb_profile(
    app: tauri::AppHandle,
    profile_id: String,
    profile_revision: Option<u64>,
) -> Result<SmbConnectionStatus, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
        let connection = cached.connection;
        let status = SmbConnectionStatus {
            profile_revision: cached.binding.revision,
            identity: connection.identity(),
            capabilities: connection.capabilities(),
            quarantine_capabilities: doppeldecker_core::ProviderKind::Smb
                .quarantine_management_capabilities(),
        };
        crate::queue_runtime::ensure_profile_bindings_current(
            &app,
            std::slice::from_ref(&cached.binding),
        )?;
        Ok(status)
    })
    .await
    .map_err(|_| AppError::new("app", "SMB-Worker wurde unterbrochen"))?
}

#[derive(serde::Serialize)]
pub(crate) struct SmbDirListing {
    profile_revision: u64,
    path: String,
    parent: Option<String>,
    entries: Vec<Entry>,
    session_generation: u64,
}

#[tauri::command]
pub(crate) async fn scan_smb_dir(
    app: tauri::AppHandle,
    profile_id: String,
    profile_revision: Option<u64>,
    path: String,
    show_hidden: bool,
) -> Result<SmbDirListing, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
        let binding = cached.binding.clone();
        let connection = cached.connection;
        let remote_path = Path::new(&path);
        // Listing und Generation stammen aus demselben gehaltenen Zustand. Getrennt
        // gelesen könnte ein Reconnect aus einem anderen Fenster dazwischenliegen und
        // die Einträge der alten Sitzung mit der neuen Generation etikettieren.
        let listing = crate::queue_runtime::retry_read_once(
            || connection.list_interactive(remote_path),
            || connection.reconnect(),
        )
        .map_err(AppError::from)?;
        let mut entries = listing.entries;
        if !show_hidden {
            entries.retain(|entry| !entry.name.starts_with('.'));
        }
        entries.sort_by(|left, right| {
            right
                .is_dir
                .cmp(&left.is_dir)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        let parent = remote_path
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .filter(|parent| !parent.is_empty());
        crate::queue_runtime::ensure_profile_bindings_current(
            &app,
            std::slice::from_ref(&binding),
        )?;
        Ok(SmbDirListing {
            profile_revision: binding.revision,
            path,
            parent,
            entries,
            session_generation: listing.session_generation,
        })
    })
    .await
    .map_err(|_| AppError::new("app", "SMB-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn list_smb_quarantine_runs(
    app: tauri::AppHandle,
    profile_id: String,
    profile_revision: Option<u64>,
    target: String,
) -> Result<Vec<QuarantineRun>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
        let result = list_quarantine_runs(
            cached.connection.as_ref(),
            &crate::queue_runtime::remote_quarantine_base("SMB", Path::new(&target))?,
            &None,
        )
        .map_err(AppError::from)?;
        crate::queue_runtime::ensure_profile_bindings_current(&app, &[cached.binding])?;
        Ok(result)
    })
    .await
    .map_err(|_| AppError::new("app", "SMB-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn list_smb_quarantine_entries(
    app: tauri::AppHandle,
    profile_id: String,
    profile_revision: Option<u64>,
    target: String,
    run_id: String,
) -> Result<Vec<QuarantineEntry>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
        let result = list_quarantine_entries_for_target(
            cached.connection.as_ref(),
            &crate::queue_runtime::remote_quarantine_base("SMB", Path::new(&target))?,
            &run_id,
            &None,
            Some(Path::new(&target)),
        )
        .map_err(AppError::from)?;
        crate::queue_runtime::ensure_profile_bindings_current(&app, &[cached.binding])?;
        Ok(result)
    })
    .await
    .map_err(|_| AppError::new("app", "SMB-Worker wurde unterbrochen"))?
}

#[tauri::command]
// Bestehende Top-Level-Argumente bleiben für ältere IPC-Aufrufer kompatibel; die
// zusätzliche Revision darf nicht zugunsten einer künstlichen Kürzung entfallen.
#[allow(clippy::too_many_arguments)]
pub(crate) fn restore_smb_quarantine_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    profile_id: String,
    profile_revision: Option<u64>,
    target: String,
    run_id: String,
    relative_path: String,
) -> Result<QuarantineRestoreResponse, AppError> {
    if !doppeldecker_core::ProviderKind::Smb
        .quarantine_management_capabilities()
        .restore
    {
        return Err(AppError::new(
            "unsupported",
            "SMB-Restore ist ohne File-ID-gebundene Wirkung gesperrt",
        ));
    }
    let label = window.label();
    let window_state = state.window(label);
    operation_queue(&app, &window_state, label)?;
    reserve_operation(&window_state)?;
    let result = (|| {
        let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
        let connection = cached.connection;
        connection
            .probe_add_only_capabilities_at(Path::new(&target))
            .map_err(AppError::from)?;
        restore_quarantine_entry_for_target(
            connection.as_ref(),
            &crate::queue_runtime::remote_quarantine_base("SMB", Path::new(&target))?,
            &run_id,
            &relative_path,
            &None,
            Some(Path::new(&target)),
        )
        .map(restore_response)
        .map_err(AppError::from)
    })();
    let succeeded = result.is_ok();
    finish_operation(&app, label, || {
        let _ = crate::emit_to_window(&window, "quarantine-terminal", succeeded);
    });
    result
}

#[tauri::command]
// Siehe `restore_smb_quarantine_entry`: Der stabile Tauri-Vertrag ist absichtlich
// flach und bindet nun zusätzlich die Profilrevision.
#[allow(clippy::too_many_arguments)]
pub(crate) fn purge_smb_quarantine_run(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    profile_id: String,
    profile_revision: Option<u64>,
    target: String,
    run_id: String,
    confirmation: String,
) -> Result<(), AppError> {
    if !doppeldecker_core::ProviderKind::Smb
        .quarantine_management_capabilities()
        .purge
    {
        return Err(AppError::new(
            "unsupported",
            "SMB-Purge ist ohne File-ID-gebundene Wirkung gesperrt",
        ));
    }
    let expected = format!("LEEREN {run_id}");
    if confirmation != expected {
        return Err(AppError::new(
            "confirmation_mismatch",
            format!("Zum endgültigen Leeren exakt „{expected}“ eingeben"),
        ));
    }
    let label = window.label();
    let window_state = state.window(label);
    operation_queue(&app, &window_state, label)?;
    reserve_operation(&window_state)?;
    let result = (|| {
        let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
        let connection = cached.connection;
        connection
            .probe_delete_capability_at(Path::new(&target))
            .map_err(AppError::from)?;
        purge_quarantine_run_for_target(
            connection.as_ref(),
            &crate::queue_runtime::remote_quarantine_base("SMB", Path::new(&target))?,
            &run_id,
            &None,
            Some(Path::new(&target)),
        )
        .map_err(AppError::from)
    })();
    let succeeded = result.is_ok();
    finish_operation(&app, label, || {
        let _ = crate::emit_to_window(&window, "quarantine-terminal", succeeded);
    });
    result
}

#[tauri::command]
pub(crate) async fn prepare_smb_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: crate::sftp_commands::RemoteSyncRequest,
) -> Result<RealRunPreview, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        let crate::sftp_commands::RemoteSyncRequest {
            profile_id,
            profile_revision,
            direction,
            local_root,
            remote_root,
            options,
        } = request;
        options.validate().map_err(AppError::from)?;
        let (local_root, remote_root) =
            crate::sftp_commands::prevalidate_remote_sync_roots(&local_root, &remote_root)?;
        with_reserved_operation(&app, label, &window_state, || {
            let (connection, profile) = connect_fresh_bound(&app, &profile_id, profile_revision)?;
            let binding = RemoteProfileBinding::new(
                RemoteProfileProtocol::Smb,
                &profile_id,
                profile.revision,
            )
            .map_err(AppError::from)?;
            crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &binding)?;
            crate::queue_runtime::ensure_profile_bindings_current(
                &app,
                std::slice::from_ref(&binding),
            )?;
            let prepared = match direction {
                SftpQueueDirection::Upload => prepare_smb_upload(
                    Path::new(&local_root),
                    connection,
                    Path::new(&remote_root),
                    options.clone(),
                ),
                SftpQueueDirection::Download => prepare_smb_download(
                    connection,
                    Path::new(&remote_root),
                    Path::new(&local_root),
                    options.clone(),
                ),
            }
            .map_err(AppError::from)?;
            let preview = prepared.preview().clone();
            let operation = QueueOperation::SmbSync {
                direction,
                connection: Box::new(profile.profile.connection),
                local_root,
                remote_root,
                options,
                expected_plan: Box::new(preview.plan.clone()),
            };
            let profile_bindings = vec![binding];
            crate::queue_runtime::store_profile_bound_pending(
                &app,
                &state,
                &window_state.pending_smb_real,
                &profile_bindings,
                PendingRemoteRealRun {
                    prepared,
                    operation,
                    profile_bindings: profile_bindings.clone(),
                },
            )?;
            Ok(preview)
        })
    })
    .await
    .map_err(|_| AppError::new("app", "SMB-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) fn discard_smb_sync(window: WebviewWindow, state: State<AppState>) {
    state
        .window(window.label())
        .pending_smb_real
        .lock_recover()
        .take();
}

#[tauri::command]
pub(crate) async fn run_smb_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    confirmation: String,
    backup_confirmed: bool,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label().to_owned();
        let window_state = state.window(&label);
        operation_queue(&app, &window_state, &label)?;
        let audit = audit_path(&app, &label)?;
        let (cancel, prepared, ()) = take_pending_for_operation(
            &state,
            &window_state,
            &window_state.pending_smb_real,
            "Keine SMB-Vorbereitung vorhanden",
            |pending| {
                crate::queue_runtime::ensure_profile_bindings_current(
                    &app,
                    &pending.profile_bindings,
                )?;
                let preview = pending.prepared.preview();
                if confirmation != preview.confirmation_phrase
                    || (preview.backup_confirmation_required && !backup_confirmed)
                {
                    return Err(AppError::new(
                        "confirmation_mismatch",
                        "SMB-Laufbestätigung stimmt nicht exakt überein",
                    ));
                }
                Ok(())
            },
        )?;
        crate::queue_runtime::spawn_confirmed_remote_run(
            app,
            window,
            label,
            prepared.profile_bindings.clone(),
            move |on_progress| {
                prepared
                    .prepared
                    .confirm(&confirmation, backup_confirmed)?
                    .execute_with_audit(&audit, &cancel, on_progress)
            },
        );
        Ok(())
    })
    .await
    .map_err(|_| AppError::new("app", "SMB-Worker wurde unterbrochen"))?
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnqueueSmbRequest {
    profile_id: String,
    #[serde(default)]
    profile_revision: Option<u64>,
    direction: SftpQueueDirection,
    local_root: String,
    remote_root: String,
    options: SyncOptions,
    expected_plan: SyncPlan,
}

#[tauri::command]
pub(crate) async fn enqueue_smb_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: EnqueueSmbRequest,
) -> Result<QueueSnapshot, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        let queue = with_queue_extension(&state, &window_state, || {
            let connection = find_profile(&app, &request.profile_id)?.connection;
            let requested_operation = QueueOperation::SmbSync {
                direction: request.direction,
                connection: Box::new(connection),
                local_root: request.local_root,
                remote_root: request.remote_root,
                options: request.options,
                expected_plan: Box::new(request.expected_plan),
            };
            let mut pending_slot = window_state.pending_smb_real.lock_recover();
            let bindings = pending_slot
                .as_ref()
                .ok_or_else(|| {
                    AppError::new("app", "Keine SMB-Vorbereitung zum Einreihen vorhanden")
                })?
                .profile_bindings
                .clone();
            crate::queue_runtime::ensure_pending_profile_binding(
                &bindings,
                RemoteProfileProtocol::Smb,
                &request.profile_id,
                request.profile_revision,
            )?;
            enqueue_bound_pending(
                &mut pending_slot,
                &requested_operation,
                |pending| &pending.operation,
                "Keine SMB-Vorbereitung zum Einreihen vorhanden",
                "SMB-Queueanfrage stimmt nicht exakt mit der vorbereiteten Operation überein",
                |operation| {
                    enqueue_operation_with_profile_bindings(
                        &app,
                        &window_state,
                        label,
                        operation,
                        &bindings,
                    )
                    .map(|(queue, _)| queue)
                },
            )
        })?;
        emit_queue_snapshot(&app, label, &queue);
        start_queue_worker(app, label.to_owned());
        Ok(queue.snapshot())
    })
    .await
    .map_err(|_| AppError::new("app", "SMB-Worker wurde unterbrochen"))?
}

pub(crate) fn prepare_remote_queue_entry(
    app: &tauri::AppHandle,
    label: &str,
    queue: &OperationQueue,
    entry: QueueEntry,
) -> QueueTerminalEvent {
    let operation_id = entry.operation_id.clone();
    let profile_bindings = entry.profile_bindings.clone();
    if let Err(error) =
        crate::queue_runtime::ensure_profile_bindings_current(app, &profile_bindings)
    {
        let _ = queue.fail(&operation_id, Progress::default(), error.message.clone());
        return QueueTerminalEvent::error("sync-error", error);
    }
    match prepare_smb_queue_entry(queue, entry, Arc::new(KeyringResolver)) {
        Ok(SftpQueuePreparation::Ready(prepared)) => {
            let state = app.state::<AppState>();
            let window_state = state.window(label);
            let pending = crate::queue_runtime::PendingRemoteQueueRun {
                prepared: *prepared,
                profile_bindings: profile_bindings.clone(),
            };
            if let Err(error) = crate::queue_runtime::store_profile_bound_pending(
                app,
                &state,
                &window_state.pending_smb_queue,
                &profile_bindings,
                pending,
            ) {
                let error = crate::queue_runtime::reset_consumed_remote_preparation(
                    queue,
                    &operation_id,
                    error,
                );
                return QueueTerminalEvent::error("sync-error", error);
            }
            QueueTerminalEvent::error(
                "sync-error",
                AppError::new(
                    "app",
                    "SMB-Lauf ist geprüft; Queue öffnen und die einmalige Laufphrase bestätigen",
                ),
            )
        }
        Ok(SftpQueuePreparation::AwaitingConfirmation { reason }) => {
            QueueTerminalEvent::error("sync-error", AppError::new("app", reason))
        }
        Err(error) => {
            let _ = queue.fail_coded(&operation_id, Progress::default(), &error);
            QueueTerminalEvent::error("sync-error", AppError::from(error))
        }
    }
}

#[tauri::command]
pub(crate) async fn approve_smb_queue_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
    confirmation: String,
    backup_confirmed: bool,
) -> Result<QueueSnapshot, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label().to_owned();
        let window_state = state.window(&label);
        let queue = operation_queue(&app, &window_state, &label)?;
        let audit = audit_path(&app, &label)?;
        crate::queue_runtime::ensure_queue_pending_current_or_reset(
            &app,
            &label,
            &queue,
            &window_state.pending_smb_queue,
            &operation_id,
            |pending| &pending.profile_bindings,
            |pending| pending.prepared.operation_id(),
        )?;
        let (cancel, prepared, ()) = take_pending_for_operation(
            &state,
            &window_state,
            &window_state.pending_smb_queue,
            "SMB-Queue-Vorbereitung fehlt im RAM",
            |prepared| {
                crate::queue_runtime::ensure_profile_bindings_current(
                    &app,
                    &prepared.profile_bindings,
                )?;
                if prepared.prepared.operation_id() != operation_id {
                    return Err(AppError::new(
                        "app",
                        "SMB-Queue-Vorbereitung gehört zu einem anderen Eintrag",
                    ));
                }
                let preview = prepared.prepared.preview();
                if confirmation != preview.confirmation_phrase
                    || (preview.backup_confirmation_required && !backup_confirmed)
                {
                    return Err(AppError::new(
                        "confirmation_mismatch",
                        "SMB-Queue-Bestätigung stimmt nicht exakt überein",
                    ));
                }
                Ok(())
            },
        )?;
        let queue_for_worker = queue.clone();
        let operation_for_worker = operation_id.clone();
        std::thread::spawn(move || {
            if let Err(error) = crate::queue_runtime::ensure_profile_bindings_current(
                &app,
                &prepared.profile_bindings,
            ) {
                let error = crate::queue_runtime::reset_consumed_remote_preparation(
                    &queue_for_worker,
                    &operation_for_worker,
                    error,
                );
                finish_operation(&app, &label, || {
                    let _ = crate::emit_to_window(&window, "sync-error", error);
                });
                return;
            }
            let events = window.clone();
            let mut last = Instant::now() - Duration::from_secs(1);
            let mut terminal_progress = None;
            let result = doppeldecker_core::execute_prepared_sftp_queue_entry(
                &queue_for_worker,
                prepared.prepared,
                &confirmation,
                backup_confirmed,
                &audit,
                &cancel,
                &mut |progress| {
                    let terminal = matches!(progress.phase.as_str(), "done" | "cancelled");
                    if terminal {
                        terminal_progress = Some(progress.clone());
                    } else if last.elapsed() >= Duration::from_millis(50) {
                        last = Instant::now();
                        let _ = crate::emit_to_window(&events, "sync-progress", progress);
                    }
                },
            );
            finish_operation(&app, &label, || {
                if let Some(progress) = terminal_progress {
                    let _ = crate::emit_to_window(&window, "sync-progress", progress);
                }
                match result {
                    Ok(QueueExecutionResult::Sync(stats)) => {
                        emit_queue_snapshot(&app, &label, &queue_for_worker);
                        let _ = crate::emit_to_window(&window, "sync-done", stats);
                    }
                    Ok(_) => {
                        emit_queue_snapshot(&app, &label, &queue_for_worker);
                        let _ = crate::emit_to_window(
                            &window,
                            "sync-error",
                            AppError::new(
                                "app",
                                "SMB-Queue lieferte einen unerwarteten Ergebnistyp",
                            ),
                        );
                    }
                    Err(error) => {
                        let mut error = AppError::from(error);
                        let waiting = queue_for_worker.snapshot().entries.iter().any(|entry| {
                            entry.operation_id == operation_for_worker
                                && entry.state
                                    == doppeldecker_core::QueueState::AwaitingConfirmation
                        });
                        if waiting {
                            error = crate::queue_runtime::reset_consumed_remote_preparation(
                                &queue_for_worker,
                                &operation_for_worker,
                                error,
                            );
                        }
                        emit_queue_snapshot(&app, &label, &queue_for_worker);
                        let _ = crate::emit_to_window(&window, "sync-error", error);
                    }
                }
            });
        });
        Ok(queue.snapshot())
    })
    .await
    .map_err(|_| AppError::new("app", "SMB-Worker wurde unterbrochen"))?
}
