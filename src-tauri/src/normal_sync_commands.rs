//! Wirkungsbezogener lokaler AddOnly-/Update-Start über die Fensterqueue.

use std::path::PathBuf;

use doppeldecker_core::{
    Cancel, OperationQueue, PreparedLocalSyncRun, Progress, QueueEntry, QueueExecutionResult,
    QueueOperation, QueueOperationClass, RealRunPreview, SyncOptions,
};
use tauri::{Manager, WebviewWindow};

use crate::{
    app_config_dir, emit_queue_snapshot, enqueue_operation_with_readback, operation_queue,
    sandbox_root, start_queue_worker, with_queue_extension, with_reserved_operation, AppError,
    AppState, MutexExt,
};

pub(crate) struct LocalSyncPreparation {
    prepared: PreparedLocalSyncRun,
    sandbox: Option<PathBuf>,
}

#[derive(Default)]
pub(crate) struct LocalSyncPreparationSlot {
    generation: u64,
    preparation: Option<LocalSyncPreparation>,
}

impl LocalSyncPreparationSlot {
    fn begin(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.preparation = None;
        self.generation
    }

    fn finish(
        &mut self,
        generation: u64,
        preparation: LocalSyncPreparation,
    ) -> Result<(), AppError> {
        if generation != self.generation {
            return Err(AppError::new(
                "plan_drift",
                "Die lokale Vorbereitung wurde während der Prüfung verworfen",
            ));
        }
        self.preparation = Some(preparation);
        Ok(())
    }

    pub(crate) fn is_some(&self) -> bool {
        self.preparation.is_some()
    }
}

pub(crate) struct QueuedLocalSync {
    prepared: PreparedLocalSyncRun,
    replacements_confirmed: bool,
}

fn check_confirmation(prepared: &PreparedLocalSyncRun, confirmed: bool) -> Result<(), AppError> {
    let impact = &prepared.preview().impact;
    if !confirmed
        && (impact.replacements > 0
            || impact.metadata_changes > 0
            || prepared
                .preview()
                .local_execution
                .as_ref()
                .is_some_and(|p| p.is_best_effort()))
    {
        return Err(AppError::new(
            "confirmation_mismatch",
            "Ersetzungen und Metadatenänderungen müssen bestätigt werden",
        ));
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn prepare_local_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    source: String,
    target: String,
    options: SyncOptions,
) -> Result<RealRunPreview, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        with_reserved_operation(&app, label, &window_state, || {
            let generation = window_state.pending_local_sync.lock_recover().begin();
            let (source, target) = doppeldecker_core::validate_local_sync_roots(
                &doppeldecker_core::expand_home_path(&source),
                &doppeldecker_core::expand_home_path(&target),
            )
            .map_err(AppError::from)?;
            let sandbox = sandbox_root()?;
            crate::local_sync_commands::local_sync_vfs(sandbox.as_deref(), &source, &target)?;
            let prepared = doppeldecker_core::prepare_local_sync_run_with_sandbox(
                &source,
                &target,
                options,
                sandbox.as_deref(),
            )
            .map_err(AppError::from)?;
            let preview = prepared.preview().clone();
            window_state
                .pending_local_sync
                .lock_recover()
                .finish(generation, LocalSyncPreparation { prepared, sandbox })?;
            Ok(preview)
        })
    })
    .await
    .map_err(|_| AppError::new("app", "Lokale Sync-Vorbereitung wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn discard_local_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<AppState>()
            .window(window.label())
            .pending_local_sync
            .lock_recover()
            .begin();
    })
    .await
    .map_err(|_| AppError::new("app", "Lokale Vorbereitung konnte nicht verworfen werden"))
}

#[tauri::command]
pub(crate) async fn enqueue_local_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    preparation_digest: [u8; 32],
    replacements_confirmed: bool,
) -> Result<String, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        let (queue, entry) = with_queue_extension(&state, &window_state, || {
            let mut pending = window_state.pending_local_sync.lock_recover();
            let preparation = pending.preparation.as_ref().ok_or_else(|| {
                AppError::new(
                    "plan_drift",
                    "Lokale Sync-Vorbereitung fehlt; bitte neu vorbereiten",
                )
            })?;
            let preview = preparation.prepared.preview();
            if preparation_digest != preview.plan_digest || preparation.sandbox != sandbox_root()? {
                return Err(AppError::new(
                    "plan_drift",
                    "Die lokale Sync-Vorbereitung wurde inzwischen geändert",
                ));
            }
            check_confirmation(&preparation.prepared, replacements_confirmed)?;
            let (
                doppeldecker_core::RootIdentity::Local(source),
                doppeldecker_core::RootIdentity::Local(target),
            ) = (&preview.source_identity, &preview.target_identity)
            else {
                unreachable!()
            };
            let operation = QueueOperation::LocalSync {
                execution_hint: preview.local_execution.clone(),
                source: source.display_path.to_string_lossy().into_owned(),
                target: target.display_path.to_string_lossy().into_owned(),
                sandbox: preparation
                    .sandbox
                    .as_ref()
                    .map(|root| root.to_string_lossy().into_owned()),
                options: preview.options.clone(),
                expected_plan: Box::new(preview.plan.clone()),
            };
            // Den RAM-Beleg unter derselben Transaktion vor dem Workerstart ablegen.
            // Ein fehlgeschlagener Journalcommit lässt die Vorbereitung erhalten.
            let mut runs = window_state.local_sync_runs.lock_recover();
            let (queue, entry) =
                enqueue_operation_with_readback(&app, &window_state, label, &operation)?;
            let prepared = pending
                .preparation
                .take()
                .expect("unmittelbar zuvor geprüft")
                .prepared;
            runs.insert(
                entry.operation_id.clone(),
                QueuedLocalSync {
                    prepared,
                    replacements_confirmed,
                },
            );
            Ok((queue, entry))
        })?;
        emit_queue_snapshot(&app, label, &queue);
        start_queue_worker(app, label.to_owned());
        Ok(entry.operation_id)
    })
    .await
    .map_err(|_| AppError::new("app", "Lokaler Queue-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn local_sync_queue_preview(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
) -> Result<RealRunPreview, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state
            .window(window.label())
            .local_sync_runs
            .lock_recover()
            .get(&operation_id)
            .map(|pending| pending.prepared.preview().clone())
            .ok_or_else(|| {
                AppError::new(
                    "plan_drift",
                    "Die lokale Queue-Vorbereitung fehlt; Auftrag neu prüfen",
                )
            })
    })
    .await
    .map_err(|_| AppError::new("app", "Lokaler Queue-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn approve_local_sync_queue_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
    preparation_digest: [u8; 32],
    replacements_confirmed: bool,
) -> Result<doppeldecker_core::QueueSnapshot, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        let queue = operation_queue(&app, &window_state, label)?;
        with_queue_extension(&state, &window_state, || {
            let mut runs = window_state.local_sync_runs.lock_recover();
            let pending = runs
                .get_mut(&operation_id)
                .ok_or_else(|| AppError::new("plan_drift", "Lokale Queue-Vorbereitung fehlt"))?;
            if pending.prepared.preview().plan_digest != preparation_digest {
                return Err(AppError::new(
                    "plan_drift",
                    "Eine neuere lokale Queue-Vorschau muss bestätigt werden",
                ));
            }
            check_confirmation(&pending.prepared, replacements_confirmed)?;
            queue
                .approve_local_sync_preparation(&operation_id, &pending.prepared)
                .map_err(AppError::from)?;
            pending.replacements_confirmed = replacements_confirmed;
            Ok(())
        })?;
        emit_queue_snapshot(&app, label, &queue);
        start_queue_worker(app, label.to_owned());
        Ok(queue.snapshot())
    })
    .await
    .map_err(|_| AppError::new("app", "Lokaler Queue-Worker wurde unterbrochen"))?
}

pub(crate) fn execute_queue_entry(
    app: &tauri::AppHandle,
    label: &str,
    queue: &OperationQueue,
    entry: &QueueEntry,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> std::io::Result<QueueExecutionResult> {
    let state = app.state::<AppState>();
    let window_state = state.window(label);
    let sandbox = sandbox_root().map_err(core_error)?;
    let QueueOperation::LocalSync {
        sandbox: expected, ..
    } = &entry.operation
    else {
        unreachable!()
    };
    if expected.as_deref().map(std::path::Path::new) != sandbox.as_deref() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Der markierte Testbereich änderte sich seit der Vorschau",
        ));
    }
    let pending = window_state
        .local_sync_runs
        .lock_recover()
        .remove(&entry.operation_id);
    let Some(pending) = pending else {
        let prepared = doppeldecker_core::queue::prepare_local_sync_queue_entry(
            queue,
            entry,
            sandbox.as_deref(),
        )?;
        window_state.local_sync_runs.lock_recover().insert(
            entry.operation_id.clone(),
            QueuedLocalSync {
                prepared,
                replacements_confirmed: false,
            },
        );
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::Sync,
            reason:
                "Lokaler Sync ist frisch vorbereitet; die sichtbare Wirkung in der Queue bestätigen"
                    .into(),
        });
    };
    let audit = app_config_dir(app)
        .map_err(core_error)?
        .join(crate::window_data_file_name("local-sync-runs.jsonl", label).map_err(core_error)?);
    doppeldecker_core::queue::execute_prepared_local_sync_queue_entry(
        queue,
        entry,
        pending.prepared,
        pending.replacements_confirmed,
        &audit,
        cancel,
        on_progress,
    )
}

fn core_error(error: AppError) -> std::io::Error {
    let code = serde_json::from_value(serde_json::Value::String(error.code))
        .unwrap_or(doppeldecker_core::ErrorCode::Io);
    doppeldecker_core::coded_error(std::io::ErrorKind::Other, code, error.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verworfene_lokale_vorbereitung_kann_keinen_neueren_slot_ersetzen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let make = |name: &str| {
            let source = root.join(format!("quelle-{name}"));
            let target = root.join(format!("ziel-{name}"));
            std::fs::create_dir(&source).unwrap();
            std::fs::create_dir(&target).unwrap();
            LocalSyncPreparation {
                prepared: doppeldecker_core::prepare_local_sync_run(
                    &source,
                    &target,
                    SyncOptions::default(),
                )
                .unwrap(),
                sandbox: None,
            }
        };
        let mut slot = LocalSyncPreparationSlot::default();
        let old = slot.begin();
        let current = slot.begin();
        let fresh = make("neu");
        let digest = fresh.prepared.preview().plan_digest;
        slot.finish(current, fresh).unwrap();
        assert_eq!(
            slot.finish(old, make("alt")).unwrap_err().code,
            "plan_drift"
        );
        assert_eq!(
            slot.preparation
                .as_ref()
                .unwrap()
                .prepared
                .preview()
                .plan_digest,
            digest
        );
        slot.begin();
        assert!(!slot.is_some());
    }

    #[test]
    fn lokale_sync_vorbereitungen_und_fehlercodes_bleiben_fenstergebunden() {
        let app = AppState::default();
        let a = app.window("main");
        let b = app.window("window-2");
        a.pending_local_sync.lock_recover().begin();
        a.pending_local_sync.lock_recover().begin();
        assert_eq!(a.pending_local_sync.lock_recover().generation, 2);
        assert_eq!(b.pending_local_sync.lock_recover().generation, 0);
        let error = core_error(AppError::new(
            "sandbox_violation",
            "Außerhalb des Testbereichs",
        ));
        assert_eq!(
            doppeldecker_core::error_code(&error),
            doppeldecker_core::ErrorCode::SandboxViolation
        );
    }
}
