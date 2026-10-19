//! Dünne Tauri-Brücke für beliebige Provider→Provider-Einwegtransfers.
//!
//! Beide Sitzungen, Handles und das Einmal-Permit bleiben im RAM. Die Queue
//! persistiert ausschließlich öffentliche Endpunktidentitäten und Secret-Referenzen.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use doppeldecker_core::{
    execute_prepared_sftp_queue_entry, prepare_provider_queue_entry, prepare_provider_transfer,
    OperationQueue, PreparedProviderRealRun, Progress, QueueEntry, QueueExecutionResult,
    QueueOperation, QueueSnapshot, SftpQueuePreparation, SyncOptions,
};
use tauri::{Manager, State, WebviewWindow};

use crate::bidirectional_commands::{
    resolve_endpoint, resolve_validated_registry_pair, validate_registry_profile,
    BidirectionalEndpointRequest,
};
use crate::queue_runtime::QueueTerminalEvent;
use crate::sftp_commands::KeyringResolver;
use crate::{
    app_config_dir, emit_queue_snapshot, finish_operation, operation_queue, start_queue_worker,
    take_pending_for_operation, with_queue_extension, with_reserved_operation, AppError, AppState,
    MutexExt,
};

pub(crate) struct PendingProviderRun {
    prepared: PreparedProviderRealRun,
    operation: QueueOperation,
    pub(crate) profile_bindings: Vec<doppeldecker_core::RemoteProfileBinding>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrepareProviderSyncRequest {
    source: BidirectionalEndpointRequest,
    target: BidirectionalEndpointRequest,
    options: SyncOptions,
}

fn audit_path(app: &tauri::AppHandle, label: &str) -> Result<PathBuf, AppError> {
    Ok(app_config_dir(app)?.join(crate::window_data_file_name(
        "provider-real-runs.jsonl",
        label,
    )?))
}

#[tauri::command]
pub(crate) fn prepare_provider_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    request: PrepareProviderSyncRequest,
) -> Result<doppeldecker_core::RealRunPreview, AppError> {
    let label = window.label();
    let window_state = state.window(label);
    request.options.validate().map_err(AppError::from)?;
    crate::bidirectional_commands::validate_endpoint_request_root(&request.source)?;
    crate::bidirectional_commands::validate_endpoint_request_root(&request.target)?;
    with_reserved_operation(&app, label, &window_state, || {
        if window_state.pending_provider_real.lock_recover().is_some() {
            return Err(AppError::new(
                "busy",
                "Es existiert bereits eine Provider-Vorbereitung",
            ));
        }
        let ((source, source_queue, source_profiles), (target, target_queue, target_profiles)) =
            resolve_validated_registry_pair(
                &request.source,
                &request.target,
                |endpoint| validate_registry_profile(&app, endpoint),
                |endpoint| resolve_endpoint(&app, &state, endpoint),
            )?;
        let profile_bindings =
            crate::queue_runtime::merge_profile_bindings([source_profiles, target_profiles]);
        let queue = crate::operation_queue(&app, &window_state, label)?;
        let source = queue
            .bind_provider_preparation(&source)
            .map_err(AppError::from)?;
        let target = queue
            .bind_provider_preparation(&target)
            .map_err(AppError::from)?;
        let prepared = prepare_provider_transfer(source, target, request.options.clone())
            .map_err(AppError::from)?;
        let preview = prepared.preview().clone();
        let operation = QueueOperation::ProviderSync {
            source: source_queue,
            target: target_queue,
            options: request.options,
            expected_plan: Box::new(preview.plan.clone()),
        };
        crate::queue_runtime::store_profile_bound_pending(
            &app,
            &state,
            &window_state.pending_provider_real,
            &profile_bindings,
            PendingProviderRun {
                prepared,
                operation,
                profile_bindings: profile_bindings.clone(),
            },
        )?;
        Ok(preview)
    })
}

#[tauri::command]
pub(crate) fn discard_provider_sync(window: WebviewWindow, state: State<AppState>) {
    state
        .window(window.label())
        .pending_provider_real
        .lock_recover()
        .take();
}

fn validate_confirmation(
    preview: &doppeldecker_core::RealRunPreview,
    confirmation: &str,
    backup_confirmed: bool,
) -> Result<(), AppError> {
    if confirmation != preview.confirmation_phrase {
        return Err(AppError::new(
            "confirmation_mismatch",
            "Provider-Laufphrase stimmt nicht exakt überein",
        ));
    }
    if preview.backup_confirmation_required && !backup_confirmed {
        return Err(AppError::new(
            "confirmation_mismatch",
            "Provider-Update/Mirror benötigt ein bestätigtes unabhängiges Backup",
        ));
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn run_provider_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    confirmation: String,
    backup_confirmed: bool,
) -> Result<(), AppError> {
    let label = window.label().to_owned();
    let window_state = state.window(&label);
    operation_queue(&app, &window_state, &label)?;
    let audit = audit_path(&app, &label)?;
    let (cancel, pending_run, ()) = take_pending_for_operation(
        &state,
        &window_state,
        &window_state.pending_provider_real,
        "Keine Provider-Vorbereitung vorhanden",
        |pending| {
            crate::queue_runtime::ensure_profile_bindings_current(&app, &pending.profile_bindings)?;
            validate_confirmation(pending.prepared.preview(), &confirmation, backup_confirmed)
        },
    )?;
    crate::queue_runtime::spawn_confirmed_remote_run(
        app,
        window,
        label,
        pending_run.profile_bindings.clone(),
        move |on_progress| {
            pending_run
                .prepared
                .confirm(&confirmation, backup_confirmed)?
                .execute_with_audit(&audit, &cancel, on_progress)
        },
    );
    Ok(())
}

#[tauri::command]
pub(crate) fn enqueue_provider_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
) -> Result<QueueSnapshot, AppError> {
    let label = window.label();
    let window_state = state.window(label);
    let queue = with_queue_extension(&state, &window_state, || {
        let mut pending = window_state.pending_provider_real.lock_recover();
        let operation = &pending
            .as_ref()
            .ok_or_else(|| AppError::new("app", "Keine Provider-Vorbereitung vorhanden"))?
            .operation;
        let profile_bindings = pending
            .as_ref()
            .expect("unmittelbar zuvor geprüft")
            .profile_bindings
            .clone();
        let (queue, _) = crate::queue_runtime::enqueue_operation_with_profile_bindings(
            &app,
            &window_state,
            label,
            operation,
            &profile_bindings,
        )?;
        pending.take();
        Ok(queue)
    })?;
    emit_queue_snapshot(&app, label, &queue);
    start_queue_worker(app, label.to_owned());
    Ok(queue.snapshot())
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
    match prepare_provider_queue_entry(queue, entry, Arc::new(KeyringResolver)) {
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
                &window_state.pending_provider_queue,
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
            QueueTerminalEvent::error("sync-error", AppError::new("app", "Provider-Lauf ist geprüft; Queue öffnen und die einmalige Laufphrase bestätigen"))
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
pub(crate) fn approve_provider_queue_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
    confirmation: String,
    backup_confirmed: bool,
) -> Result<QueueSnapshot, AppError> {
    let label = window.label().to_owned();
    let window_state = state.window(&label);
    let queue = operation_queue(&app, &window_state, &label)?;
    let audit = audit_path(&app, &label)?;
    crate::queue_runtime::ensure_queue_pending_current_or_reset(
        &app,
        &label,
        &queue,
        &window_state.pending_provider_queue,
        &operation_id,
        |pending| &pending.profile_bindings,
        |pending| pending.prepared.operation_id(),
    )?;
    let (cancel, prepared, ()) = take_pending_for_operation(
        &state,
        &window_state,
        &window_state.pending_provider_queue,
        "Provider-Queue-Vorbereitung fehlt im RAM",
        |prepared| {
            crate::queue_runtime::ensure_profile_bindings_current(
                &app,
                &prepared.profile_bindings,
            )?;
            if prepared.prepared.operation_id() != operation_id {
                return Err(AppError::new(
                    "app",
                    "Provider-Queue-Vorbereitung gehört zu einem anderen Eintrag",
                ));
            }
            validate_confirmation(prepared.prepared.preview(), &confirmation, backup_confirmed)
        },
    )?;
    let queue_for_worker = queue.clone();
    let operation_for_worker = operation_id.clone();
    std::thread::spawn(move || {
        if let Err(error) =
            crate::queue_runtime::ensure_profile_bindings_current(&app, &prepared.profile_bindings)
        {
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
        let result = execute_prepared_sftp_queue_entry(
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
                            "Provider-Queue lieferte einen unerwarteten Ergebnistyp",
                        ),
                    );
                }
                Err(error) => {
                    let mut error = AppError::from(error);
                    let still_waiting = queue_for_worker.snapshot().entries.iter().any(|entry| {
                        entry.operation_id == operation_for_worker
                            && entry.state == doppeldecker_core::QueueState::AwaitingConfirmation
                    });
                    if still_waiting {
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
}

#[cfg(test)]
mod tests {
    use super::PrepareProviderSyncRequest;
    use doppeldecker_core::{Compare, MetadataProfile, ProviderKind, SyncMode};

    /// Am riskantesten sind Core-Typen **ohne** eigenes `rename_all` in einer
    /// camelCase-Hülle: `SyncOptions.mtime_tolerance` bleibt snake_case, und genau so
    /// sendet es das Frontend (`sync-view.js`). Ein später im Core ergänztes
    /// `rename_all = "camelCase"` bräche diese Bindung still — der Test hält sie fest.
    #[test]
    fn prepare_request_liest_den_frontend_payload_wortgleich() {
        let request: PrepareProviderSyncRequest = serde_json::from_value(serde_json::json!({
            "source": {
                "provider": "local",
                "root": "/tmp/quelle"
            },
            "target": {
                "provider": "smb",
                "profileId": "profil-9",
                "profileRevision": 4,
                "root": "/freigabe"
            },
            "options": {
                "mode": "Update",
                "compare": "ContentHash",
                "metadata": "Portable",
                "mtime_tolerance": 2
            }
        }))
        .unwrap();

        assert_eq!(request.source.provider_kind(), ProviderKind::Local);
        assert_eq!(request.target.provider_kind(), ProviderKind::Smb);
        assert_eq!(request.options.mode, SyncMode::Update);
        assert_eq!(request.options.compare, Compare::ContentHash);
        assert_eq!(request.options.metadata, MetadataProfile::Portable);
        assert_eq!(request.options.mtime_tolerance, 2);
    }
}
