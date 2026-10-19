//! Dünne Tauri-Anbindung der GUI-freien FTP-/FTPS-Providerfamilie.
//!
//! Profile enthalten bei TLS nur öffentliche Zertifikat-Pins und immer ausschließlich
//! Schlüsselbundreferenzen. Passwortwerte, Sitzungen und Realmodus-Permits bleiben im RAM.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use doppeldecker_core::{
    load_ftps_profiles as core_load_profiles, prepare_ftps_download, prepare_ftps_queue_entry,
    prepare_ftps_upload, Entry, FtpSecurityStatus, FtpsProfile, FtpsServerIdentity, FtpsVfs,
    OperationQueue, Progress, QueueEntry, QueueExecutionResult, QueueOperation, QueueSnapshot,
    RealRunPreview, RemoteCapabilities, RemoteProfileBinding, RemoteProfileProtocol,
    SftpQueueDirection, SftpQueuePreparation, SyncOptions, SyncPlan, VersionedProfile,
};
use tauri::{Manager, State, WebviewWindow};

use crate::queue_runtime::{
    cached_remote_connection, enqueue_bound_pending, CachedRemoteConnection, PendingRemoteRealRun,
    QueueTerminalEvent,
};
use crate::sftp_commands::KeyringResolver;
use crate::{
    app_config_dir, enqueue_operation_with_profile_bindings, finish_operation, operation_queue,
    take_pending_for_operation, with_queue_extension, with_reserved_operation, AppError, AppState,
    MutexExt,
};
use crate::{emit_queue_snapshot, start_queue_worker};

const PROFILE_FILE: &str = "ftps-profiles.json";
const AUDIT_FILE: &str = "ftps-real-runs.jsonl";

fn profile_path(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    Ok(app_config_dir(app)?.join(PROFILE_FILE))
}

fn audit_path(app: &tauri::AppHandle, label: &str) -> Result<PathBuf, AppError> {
    Ok(app_config_dir(app)?.join(crate::window_data_file_name(AUDIT_FILE, label)?))
}

pub(crate) fn find_profile(
    app: &tauri::AppHandle,
    profile_id: &str,
) -> Result<FtpsProfile, AppError> {
    core_load_profiles(&profile_path(app)?)
        .map_err(AppError::from)?
        .into_iter()
        .find(|profile| profile.id == profile_id)
        .ok_or_else(|| AppError::new("not_found", "FTP-/FTPS-Profil wurde nicht gefunden"))
}

pub(crate) fn find_versioned_profile(
    app: &tauri::AppHandle,
    profile_id: &str,
) -> Result<VersionedProfile<FtpsProfile>, AppError> {
    doppeldecker_core::load_ftps_profiles_versioned(&profile_path(app)?)
        .map_err(AppError::from)?
        .into_iter()
        .find(|profile| profile.profile.id == profile_id)
        .ok_or_else(|| AppError::new("not_found", "FTP-/FTPS-Profil wurde nicht gefunden"))
}

fn connect_fresh_bound(
    app: &tauri::AppHandle,
    profile_id: &str,
    profile_revision: Option<u64>,
) -> Result<(Arc<FtpsVfs>, VersionedProfile<FtpsProfile>), AppError> {
    let profile = find_versioned_profile(app, profile_id)?;
    let binding =
        RemoteProfileBinding::new(RemoteProfileProtocol::Ftps, profile_id, profile.revision)
            .map_err(AppError::from)?;
    crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &binding)?;
    let connection = FtpsVfs::connect(
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
    doppeldecker_core::latest_ftps_profile_revision(&profile_path(app)?, profile_id)
        .map_err(AppError::from)
}

pub(crate) fn cached_connection_bound(
    app: &tauri::AppHandle,
    state: &AppState,
    profile_id: &str,
    profile_revision: Option<u64>,
) -> Result<CachedRemoteConnection<FtpsVfs>, AppError> {
    cached_remote_connection(
        app,
        state,
        &state.ftps_connections,
        crate::queue_runtime::RemoteConnectionRequest {
            protocol: RemoteProfileProtocol::Ftps,
            profile_id,
            requested_revision: profile_revision,
        },
        || find_versioned_profile(app, profile_id),
        |profile| {
            FtpsVfs::connect(profile.connection, Arc::new(KeyringResolver))
                .map(Arc::new)
                .map_err(AppError::from)
        },
    )
    .map(|(cached, _)| cached)
}

#[tauri::command]
pub(crate) async fn discover_ftps_certificate(
    host: String,
    port: u16,
) -> Result<doppeldecker_core::FtpsCertificatePin, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        doppeldecker_core::discover_ftps_certificate(&host, port).map_err(AppError::from)
    })
    .await
    .map_err(|_| AppError::new("app", "FTPS-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn list_ftps_profiles(
    app: tauri::AppHandle,
) -> Result<Vec<VersionedProfile<FtpsProfile>>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        doppeldecker_core::load_ftps_profiles_versioned(&profile_path(&app)?)
            .map_err(AppError::from)
    })
    .await
    .map_err(|_| AppError::new("app", "FTPS-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn save_ftps_profile(
    app: tauri::AppHandle,
    window: WebviewWindow,
    mut profile: FtpsProfile,
    secret_id: Option<String>,
    profile_revision: Option<u64>,
) -> Result<Vec<VersionedProfile<FtpsProfile>>, AppError> {
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
                protocol: "ftps",
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
                doppeldecker_core::save_ftps_profile_versioned_if_current(
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
                state.ftps_connections.lock_recover().remove(&id);
                crate::queue_runtime::invalidate_remote_profile(
                    &app,
                    &state,
                    RemoteProfileProtocol::Ftps,
                    &id,
                );
                if let Ok(Some(revision)) = current_profile_revision(&app, &id) {
                    let binding =
                        RemoteProfileBinding::new(RemoteProfileProtocol::Ftps, &id, revision)
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
    .map_err(|_| AppError::new("app", "FTPS-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn remove_ftps_profile(
    app: tauri::AppHandle,
    window: WebviewWindow,
    profile_id: String,
    profile_revision: u64,
) -> Result<Vec<VersionedProfile<FtpsProfile>>, AppError> {
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
                doppeldecker_core::remove_ftps_profile_versioned_if_current(
                    &path,
                    &profile_id,
                    profile_revision,
                )
                .map_err(AppError::from)
            },
            || {
                state.ftps_connections.lock_recover().remove(&profile_id);
                crate::queue_runtime::invalidate_remote_profile(
                    &app,
                    &state,
                    RemoteProfileProtocol::Ftps,
                    &profile_id,
                );
                let binding = RemoteProfileBinding::new(
                    RemoteProfileProtocol::Ftps,
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
    .map_err(|_| AppError::new("app", "FTPS-Worker wurde unterbrochen"))?
}

#[derive(serde::Serialize)]
pub(crate) struct FtpsConnectionStatus {
    profile_revision: u64,
    identity: FtpsServerIdentity,
    capabilities: RemoteCapabilities,
    security: FtpSecurityStatus,
    quarantine_capabilities:
        doppeldecker_core::remote_real_mode::ProviderQuarantineManagementCapabilities,
}

#[tauri::command]
pub(crate) async fn connect_ftps_profile(
    app: tauri::AppHandle,
    profile_id: String,
    profile_revision: Option<u64>,
) -> Result<FtpsConnectionStatus, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
        let connection = cached.connection;
        let status = FtpsConnectionStatus {
            profile_revision: cached.binding.revision,
            identity: connection.identity(),
            capabilities: connection.capabilities(),
            security: connection.security_status(),
            quarantine_capabilities: doppeldecker_core::ProviderKind::Ftps
                .quarantine_management_capabilities(),
        };
        crate::queue_runtime::ensure_profile_bindings_current(
            &app,
            std::slice::from_ref(&cached.binding),
        )?;
        Ok(status)
    })
    .await
    .map_err(|_| AppError::new("app", "FTPS-Worker wurde unterbrochen"))?
}

#[derive(serde::Serialize)]
pub(crate) struct FtpsDirListing {
    profile_revision: u64,
    path: String,
    parent: Option<String>,
    entries: Vec<Entry>,
    session_generation: u64,
    security: FtpSecurityStatus,
}

#[tauri::command]
pub(crate) async fn scan_ftps_dir(
    app: tauri::AppHandle,
    profile_id: String,
    profile_revision: Option<u64>,
    path: String,
    show_hidden: bool,
) -> Result<FtpsDirListing, AppError> {
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
        Ok(FtpsDirListing {
            profile_revision: binding.revision,
            path,
            parent,
            entries,
            session_generation: listing.session_generation,
            security: connection.security_status(),
        })
    })
    .await
    .map_err(|_| AppError::new("app", "FTPS-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn prepare_ftps_sync(
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
                RemoteProfileProtocol::Ftps,
                &profile_id,
                profile.revision,
            )
            .map_err(AppError::from)?;
            crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &binding)?;
            crate::queue_runtime::ensure_profile_bindings_current(
                &app,
                std::slice::from_ref(&binding),
            )?;
            let queue = crate::operation_queue(&app, &window_state, label)?;
            let connection = Arc::new(
                connection
                    .for_queue_preparation(&queue, Path::new(&remote_root))
                    .map_err(AppError::from)?,
            );
            let prepared = match direction {
                doppeldecker_core::SftpQueueDirection::Upload => prepare_ftps_upload(
                    Path::new(&local_root),
                    connection,
                    Path::new(&remote_root),
                    options.clone(),
                ),
                doppeldecker_core::SftpQueueDirection::Download => prepare_ftps_download(
                    connection,
                    Path::new(&remote_root),
                    Path::new(&local_root),
                    options.clone(),
                ),
            }
            .map_err(AppError::from)?;
            let preview = prepared.preview().clone();
            let operation = QueueOperation::FtpsSync {
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
                &window_state.pending_ftps_real,
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
    .map_err(|_| AppError::new("app", "FTPS-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) fn discard_ftps_sync(window: WebviewWindow, state: State<AppState>) {
    state
        .window(window.label())
        .pending_ftps_real
        .lock_recover()
        .take();
}

#[tauri::command]
pub(crate) async fn run_ftps_sync(
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
            &window_state.pending_ftps_real,
            "Keine FTPS-Vorbereitung vorhanden",
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
                        "FTPS-Laufbestätigung stimmt nicht exakt überein",
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
    .map_err(|_| AppError::new("app", "FTPS-Worker wurde unterbrochen"))?
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnqueueFtpsRequest {
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
pub(crate) async fn enqueue_ftps_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: EnqueueFtpsRequest,
) -> Result<QueueSnapshot, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        let queue = with_queue_extension(&state, &window_state, || {
            let connection = find_profile(&app, &request.profile_id)?.connection;
            let requested_operation = QueueOperation::FtpsSync {
                direction: request.direction,
                connection: Box::new(connection),
                local_root: request.local_root,
                remote_root: request.remote_root,
                options: request.options,
                expected_plan: Box::new(request.expected_plan),
            };
            let mut pending_slot = window_state.pending_ftps_real.lock_recover();
            let bindings = pending_slot
                .as_ref()
                .ok_or_else(|| {
                    AppError::new("app", "Keine FTPS-Vorbereitung zum Einreihen vorhanden")
                })?
                .profile_bindings
                .clone();
            crate::queue_runtime::ensure_pending_profile_binding(
                &bindings,
                RemoteProfileProtocol::Ftps,
                &request.profile_id,
                request.profile_revision,
            )?;
            enqueue_bound_pending(
                &mut pending_slot,
                &requested_operation,
                |pending| &pending.operation,
                "Keine FTPS-Vorbereitung zum Einreihen vorhanden",
                "FTPS-Queueanfrage stimmt nicht exakt mit der vorbereiteten Operation überein",
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
    .map_err(|_| AppError::new("app", "FTPS-Worker wurde unterbrochen"))?
}

/// Der Worker hält die frisch aufgebaute FTPS-Sitzung samt Permit nur im RAM. Nach
/// einem Neustart wird der Eintrag erneut vollständig geprüft und braucht eine neue Phrase.
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
    match prepare_ftps_queue_entry(queue, entry, Arc::new(KeyringResolver)) {
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
                &window_state.pending_ftps_queue,
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
                    "FTPS-Lauf ist geprüft; Queue öffnen und die einmalige Laufphrase bestätigen",
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
pub(crate) async fn approve_ftps_queue_entry(
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
            &window_state.pending_ftps_queue,
            &operation_id,
            |pending| &pending.profile_bindings,
            |pending| pending.prepared.operation_id(),
        )?;
        let (cancel, prepared, ()) = take_pending_for_operation(
            &state,
            &window_state,
            &window_state.pending_ftps_queue,
            "FTPS-Queue-Vorbereitung fehlt im RAM",
            |prepared| {
                crate::queue_runtime::ensure_profile_bindings_current(
                    &app,
                    &prepared.profile_bindings,
                )?;
                if prepared.prepared.operation_id() != operation_id {
                    return Err(AppError::new(
                        "app",
                        "FTPS-Queue-Vorbereitung gehört zu einem anderen Eintrag",
                    ));
                }
                let preview = prepared.prepared.preview();
                if confirmation != preview.confirmation_phrase
                    || (preview.backup_confirmation_required && !backup_confirmed)
                {
                    return Err(AppError::new(
                        "confirmation_mismatch",
                        "FTPS-Queue-Bestätigung stimmt nicht exakt überein",
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
                                "FTPS-Queue lieferte einen unerwarteten Ergebnistyp",
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
    .map_err(|_| AppError::new("app", "FTPS-Worker wurde unterbrochen"))?
}
