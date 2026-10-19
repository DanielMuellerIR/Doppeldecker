//! Lokale Sync-Vorschau, Queue-Enqueue und Realmodus-Assistent.

use std::path::Path;
#[cfg(test)]
use std::sync::Mutex;
use std::time::Instant;

#[cfg(test)]
use doppeldecker_core::PreparedLocalRealRun;
use doppeldecker_core::{
    expand_home_path, validate_local_sync_roots, LocalVfs, QueueOperation, QueueState,
    RealRunPreview, SyncOptions, SyncPlan, QUARANTINE_RETENTION_DAYS,
};
use tauri::{Manager, State, WebviewWindow};

use crate::{
    app_config_dir, emit_queue_snapshot, enqueue_operation_with_readback, finish_operation,
    operation_queue, sandbox_root, start_queue_worker, take_pending_for_operation,
    with_queue_extension, with_reserved_operation, AppError, AppState, MutexExt,
    MAX_DELETE_ENTRIES, MAX_DELETE_FRACTION,
};

#[tauri::command]
pub(crate) fn plan_sync(
    source: String,
    target: String,
    options: SyncOptions,
) -> Result<SyncPlan, AppError> {
    let (source, target) =
        validate_local_sync_roots(&expand_home_path(&source), &expand_home_path(&target))
            .map_err(AppError::from)?;
    let sandbox = sandbox_root()?;
    let vfs = local_sync_vfs(sandbox.as_deref(), &source, &target)?;
    doppeldecker_core::plan(&vfs, &source, &vfs, &target, &options).map_err(AppError::from)
}

fn require_sandbox_child(root: &Path, path: &Path, role: &str) -> Result<(), AppError> {
    if path == root || !path.starts_with(root) {
        return Err(AppError::new(
            "sandbox_violation",
            format!(
                "{role} „{}“ muss ein Unterordner der Test-Sandbox „{}“ sein",
                path.display(),
                root.display()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn local_sync_vfs(
    sandbox: Option<&Path>,
    source: &Path,
    target: &Path,
) -> Result<LocalVfs, AppError> {
    match sandbox {
        Some(root) => {
            require_sandbox_child(root, source, "Quelle")?;
            require_sandbox_child(root, target, "Ziel")?;
            let vfs = LocalVfs::rooted(root).map_err(AppError::from)?;
            let target = LocalVfs::rooted(target).map_err(AppError::from)?;
            Ok(vfs.with_file_commit_root(&target))
        }
        None => Ok(LocalVfs::new()),
    }
}

#[tauri::command]
pub(crate) fn run_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    source: String,
    target: String,
    options: SyncOptions,
    expected_plan: SyncPlan,
    simulation: Option<bool>,
) -> Result<String, AppError> {
    let state = app.state::<AppState>();
    let (source, target) =
        validate_local_sync_roots(&expand_home_path(&source), &expand_home_path(&target))
            .map_err(AppError::from)?;
    let sandbox = sandbox_root()?;
    let vfs = local_sync_vfs(sandbox.as_deref(), &source, &target)?;
    let current =
        doppeldecker_core::plan(&vfs, &source, &vfs, &target, &options).map_err(AppError::from)?;
    if current != expected_plan {
        return Err(AppError::new(
            "plan_drift",
            "Quelle oder Ziel hat sich seit der Vorschau geändert; bitte neu bestätigen",
        ));
    }
    let label = window.label();
    let window_state = state.window(label);
    let (queue, entry) = with_queue_extension(&state, &window_state, || {
        let operation = QueueOperation::Sync {
            source: source.to_string_lossy().into_owned(),
            target: target.to_string_lossy().into_owned(),
            sandbox: if simulation == Some(true) {
                None
            } else {
                sandbox.map(|root| root.to_string_lossy().into_owned())
            },
            options,
            expected_plan: Box::new(expected_plan),
        };
        enqueue_operation_with_readback(&app, &window_state, label, &operation)
    })?;
    emit_queue_snapshot(&app, label, &queue);
    start_queue_worker(app, label.to_owned());
    Ok(entry.operation_id)
}

#[derive(serde::Serialize)]
pub(crate) struct SafetyInfo {
    real_enabled: bool,
    real_mode_available: bool,
    real_mode_pending: bool,
    sandbox: Option<String>,
    max_delete_entries: u64,
    max_delete_fraction: f64,
    quarantine_retention_days: u64,
}

#[tauri::command]
pub(crate) fn safety_info(
    window: WebviewWindow,
    state: State<AppState>,
) -> Result<SafetyInfo, AppError> {
    let sandbox = sandbox_root()?;
    let window_state = state.window(window.label());
    let real_mode_pending = window_state.pending_real.lock_recover().is_some();
    Ok(SafetyInfo {
        real_enabled: sandbox.is_some(),
        real_mode_available: sandbox.is_none(),
        real_mode_pending,
        sandbox: sandbox.map(|path| path.to_string_lossy().into_owned()),
        max_delete_entries: MAX_DELETE_ENTRIES,
        max_delete_fraction: MAX_DELETE_FRACTION,
        quarantine_retention_days: QUARANTINE_RETENTION_DAYS,
    })
}

#[tauri::command]
pub(crate) fn prepare_real_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    source: String,
    target: String,
    options: SyncOptions,
) -> Result<RealRunPreview, AppError> {
    if sandbox_root()?.is_some() {
        return Err(AppError::new(
            "app",
            "Realmodus und markierte Test-Sandbox sind technisch getrennt; Sandbox zuerst beenden",
        ));
    }
    // Auch der lokale Realmodus schreibt und entfernt eine zufällige Restore-Probe.
    // Die bisherige Vorab-Abfrage ließ zwischen Prüfung und Probe ein Rennen offen.
    let label = window.label();
    let window_state = state.window(label);
    with_reserved_operation(&app, label, &window_state, || {
        let queue = operation_queue(&app, &window_state, label)?;
        if queue.snapshot().entries.iter().any(|entry| {
            matches!(
                entry.state,
                QueueState::Pending
                    | QueueState::Validating
                    | QueueState::Running
                    | QueueState::AwaitingConfirmation
                    | QueueState::Interrupted
            )
        }) {
            return Err(AppError::new(
                "busy",
                "Realmodus verlangt eine abgearbeitete Queue; offene Einträge zuerst behandeln",
            ));
        }
        let mut pending = window_state.pending_real.lock_recover();
        *pending = None;
        let prepared = doppeldecker_core::prepare_local_real_run(
            &expand_home_path(&source),
            &expand_home_path(&target),
            options,
        )
        .map_err(AppError::from)?;
        let preview = prepared.preview().clone();
        *pending = Some(prepared);
        Ok(preview)
    })
}

#[tauri::command]
pub(crate) fn discard_real_sync(window: WebviewWindow, state: State<AppState>) {
    state
        .window(window.label())
        .pending_real
        .lock_recover()
        .take();
}

#[cfg(test)]
pub(crate) fn take_pending_real(
    pending: &Mutex<Option<PreparedLocalRealRun>>,
) -> Result<PreparedLocalRealRun, AppError> {
    pending.lock_recover().take().ok_or_else(|| {
        AppError::new(
            "app",
            "Keine backend-gehaltene Realmodus-Vorbereitung vorhanden",
        )
    })
}

#[tauri::command]
pub(crate) fn run_real_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    confirmation: String,
    backup_confirmed: bool,
) -> Result<(), AppError> {
    let label = window.label().to_owned();
    let window_state = state.window(&label);
    // Der Auditpfad kann durch ein ungeeignetes Fensterlabel oder einen
    // Konfigurationsfehler scheitern. Er wird deshalb vollständig bestimmt, bevor
    // die Einmalvorbereitung unter der zentralen Starttransaktion entnommen wird.
    let audit_path =
        app_config_dir(&app)?.join(crate::window_data_file_name("real-runs.jsonl", &label)?);
    operation_queue(&app, &window_state, &label)?;
    let (cancel, prepared, ()) = take_pending_for_operation(
        &state,
        &window_state,
        &window_state.pending_real,
        "Keine backend-gehaltene Realmodus-Vorbereitung vorhanden",
        |prepared| {
            let preview = prepared.preview();
            if confirmation != preview.confirmation_phrase
                || (preview.backup_confirmation_required && !backup_confirmed)
            {
                return Err(AppError::new(
                    "confirmation_mismatch",
                    "Lokale Realmodus-Bestätigung stimmt nicht exakt überein",
                ));
            }
            Ok(())
        },
    )?;
    std::thread::spawn(move || {
        let progress_window = window.clone();
        let mut last = Instant::now();
        let mut terminal_progress = None;
        let result = prepared.confirm_and_execute_with_audit(
            &confirmation,
            backup_confirmed,
            &audit_path,
            &cancel,
            &mut |progress| {
                let terminal = progress.phase == "done" || progress.phase == "cancelled";
                if terminal {
                    terminal_progress = Some(progress.clone());
                } else if last.elapsed().as_millis() >= 50 {
                    last = Instant::now();
                    let _ = crate::emit_to_window(&progress_window, "sync-progress", progress);
                }
            },
        );
        finish_operation(&app, &label, || {
            if let Some(progress) = terminal_progress {
                let _ = crate::emit_to_window(&window, "sync-progress", progress);
            }
            match result {
                Ok(stats) => {
                    let _ = crate::emit_to_window(&window, "sync-done", stats);
                }
                Err(error) => {
                    let _ = crate::emit_to_window(&window, "sync-error", AppError::from(error));
                }
            }
        });
    });
    Ok(())
}
