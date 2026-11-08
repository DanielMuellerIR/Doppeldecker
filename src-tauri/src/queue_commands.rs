//! Bediencommands der persistenten Operations-Queue.

use crate::frontend_payload::FrontendQueueSnapshot;
use std::sync::Arc;

use doppeldecker_core::OperationQueue;
use tauri::{State, WebviewWindow};

use crate::{
    emit_queue_snapshot, operation_queue, start_queue_worker, with_queue_extension, AppError,
    AppState, MutexExt,
};

/// Ändert die Queue des aufrufenden Fensters und meldet den neuen Stand.
///
/// Alle Bedienbefehle teilen diesen Ablauf: die Queue genau dieses Fensters holen,
/// eine einzige Journaländerung ausführen und den neuen Stand nur an dasselbe
/// Fenster senden. Andere Fenster haben eigene Queues und bleiben unberührt.
fn change_queue(
    app: &tauri::AppHandle,
    window: &WebviewWindow,
    state: &State<AppState>,
    change: impl FnOnce(&OperationQueue) -> Result<(), AppError>,
) -> Result<FrontendQueueSnapshot, AppError> {
    let label = window.label();
    let queue = operation_queue(app, &state.window(label), label)?;
    change(&queue)?;
    emit_queue_snapshot(app, label, &queue);
    Ok(queue.snapshot().into())
}

/// Wie [`change_queue`], aber für Änderungen, die einen wartenden Eintrag wieder
/// lauffähig machen.
///
/// Solche Änderungen verlängern die Arbeit dieses Fensters und laufen deshalb
/// unter der Erweiterungs-Schranke: Sie werden abgelehnt, während Fenster oder App
/// schließen. Erst danach startet der Worker dieses Fensters.
fn resume_queue(
    app: tauri::AppHandle,
    window: &WebviewWindow,
    state: &State<AppState>,
    change: impl FnOnce(&Arc<OperationQueue>) -> Result<(), AppError>,
) -> Result<FrontendQueueSnapshot, AppError> {
    let label = window.label();
    let window_state = state.window(label);
    let queue = with_queue_extension(state, &window_state, || {
        let queue = operation_queue(&app, &window_state, label)?;
        change(&queue)?;
        Ok(queue)
    })?;
    emit_queue_snapshot(&app, label, &queue);
    start_queue_worker(app, label.to_owned());
    Ok(queue.snapshot().into())
}

#[tauri::command]
pub(crate) fn cancel_sync(window: WebviewWindow, state: State<AppState>) {
    if let Some(cancel) = state
        .window(window.label())
        .active_cancel
        .lock_recover()
        .as_ref()
    {
        cancel.cancel();
    }
}

#[tauri::command]
pub(crate) fn list_operation_queue(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
) -> Result<FrontendQueueSnapshot, AppError> {
    let label = window.label();
    let queue = operation_queue(&app, &state.window(label), label)?;
    let snapshot = queue.snapshot();
    if queue.has_runnable() {
        start_queue_worker(app, label.to_owned());
    }
    Ok(snapshot.into())
}

#[tauri::command]
pub(crate) fn set_queue_paused(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    paused: bool,
) -> Result<FrontendQueueSnapshot, AppError> {
    let snapshot = change_queue(&app, &window, &state, |queue| {
        queue.set_paused(paused).map_err(AppError::from)
    })?;
    // Fortsetzen startet den Worker; Pausieren lässt den laufenden Eintrag zu Ende
    // laufen und nimmt danach keinen neuen mehr an.
    if !paused {
        start_queue_worker(app, window.label().to_owned());
    }
    Ok(snapshot)
}

#[tauri::command]
pub(crate) fn set_queue_stop_on_error(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    stop: bool,
) -> Result<FrontendQueueSnapshot, AppError> {
    change_queue(&app, &window, &state, |queue| {
        queue.set_stop_on_error(stop).map_err(AppError::from)
    })
}

#[tauri::command]
pub(crate) fn move_queue_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
    delta: i32,
) -> Result<FrontendQueueSnapshot, AppError> {
    change_queue(&app, &window, &state, |queue| {
        queue
            .move_pending(&operation_id, delta)
            .map_err(AppError::from)
    })
}

#[tauri::command]
pub(crate) async fn remove_queue_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
    operation_id: String,
) -> Result<FrontendQueueSnapshot, AppError> {
    let label = window.label().to_owned();
    let window_state = state.window(&label);
    let queue = operation_queue(&app, &window_state, &label)?;
    tauri::async_runtime::spawn_blocking(move || {
        crate::forward_bidirectional_commands::discard_queue_slot(
            &window_state,
            &queue,
            &operation_id,
            false,
        )?;
        queue
            .remove_pending(&operation_id)
            .map_err(AppError::from)?;
        window_state
            .content_sync_runs
            .lock_recover()
            .remove(&operation_id);
        window_state
            .local_sync_runs
            .lock_recover()
            .remove(&operation_id);
        emit_queue_snapshot(&app, &label, &queue);
        Ok(queue.snapshot().into())
    })
    .await
    .map_err(|_| AppError::new("outcome_unclear", "Queue-Bereinigung unterbrochen"))?
}

#[tauri::command]
pub(crate) fn retry_queue_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
) -> Result<FrontendQueueSnapshot, AppError> {
    resume_queue(app, &window, &state, |queue| {
        queue.retry(&operation_id).map_err(AppError::from)
    })
}

#[tauri::command]
pub(crate) fn discard_replan_required_queue_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
) -> Result<FrontendQueueSnapshot, AppError> {
    resume_queue(app, &window, &state, |queue| {
        queue
            .discard_replan_required(&operation_id)
            .map_err(AppError::from)
    })
}

#[tauri::command]
pub(crate) fn acknowledge_legacy_webdav_delete(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
    possible_effect_reviewed: bool,
) -> Result<FrontendQueueSnapshot, AppError> {
    // Beide zulässigen Abschlüsse beseitigen den vordersten Blocker: Ein sicher
    // ungestarteter Altplan wird entfernt, ein möglicherweise wirksamer Lauf nach
    // manueller Prüfung terminal. Danach darf der nächste Pending-Eintrag dieses
    // Fensters anlaufen; Schließbarriere und Workerstart gehören daher zwingend
    // zum selben Wiederaufnahmevertrag wie bei allgemeinen unklaren Ausgängen.
    resume_queue(app, &window, &state, |queue| {
        queue
            .acknowledge_legacy_webdav_delete(&operation_id, possible_effect_reviewed)
            .map_err(AppError::from)
    })
}

#[tauri::command]
pub(crate) fn acknowledge_archive_extract_outcome(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
    effects_reviewed: bool,
) -> Result<FrontendQueueSnapshot, AppError> {
    if !effects_reviewed {
        return Err(AppError::new(
            "confirmation_mismatch",
            "Entpackbilanz und mögliche Zielwirkung wurden nicht bestätigt",
        ));
    }
    resume_queue(app, &window, &state, |queue| {
        queue
            .acknowledge_archive_extract_outcome(&operation_id)
            .map_err(AppError::from)
    })
}

#[tauri::command]
pub(crate) fn acknowledge_unclear_queue_outcome(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
    possible_effect_reviewed: bool,
) -> Result<FrontendQueueSnapshot, AppError> {
    // Die manuelle Prüfung schließt den blockierenden Eintrag. Danach kann der
    // nächste Pending-Eintrag sofort laufen; deshalb gilt derselbe fensterlokale
    // Erweiterungs- und Worker-Vertrag wie bei Retry und Bestätigungsablehnung.
    resume_queue(app, &window, &state, |queue| {
        queue
            .acknowledge_unclear_outcome(&operation_id, possible_effect_reviewed)
            .map_err(AppError::from)
    })
}

#[tauri::command]
pub(crate) fn discard_queue_confirmation(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
) -> Result<FrontendQueueSnapshot, AppError> {
    // Ablehnen ist die Gegenrichtung zu `approve_queue_entry`: Der Eintrag wird
    // verworfen, statt die neue Lage zu übernehmen. Es läuft trotzdem über den
    // Wiederaufnahme-Helfer, weil danach der nächste wartende Eintrag anlaufen
    // darf — der Verworfene blockiert die Reihenfolge nicht mehr.
    resume_queue(app, &window, &state, |queue| {
        if queue.snapshot().entries.iter().any(|entry| {
            entry.operation_id == operation_id
                && matches!(
                    entry.operation,
                    doppeldecker_core::QueueOperation::BidirectionalForward { .. }
                )
        }) {
            crate::forward_bidirectional_commands::discard_queue_slot(
                &state.window(window.label()),
                queue,
                &operation_id,
                true,
            )?;
            if queue.snapshot().entries.iter().any(|entry| {
                entry.operation_id == operation_id
                    && entry.state == doppeldecker_core::QueueState::Interrupted
            }) {
                return Err(AppError::new("replan_required","Vorhandene Teilwirkungen bleiben im Journal erhalten; den bidirektionalen Bestand neu abgleichen"));
            }
            return Ok(());
        }

        queue
            .discard_attention(&operation_id)
            .map_err(AppError::from)?;
        state
            .window(window.label())
            .content_sync_runs
            .lock_recover()
            .remove(&operation_id);
        state
            .window(window.label())
            .local_sync_runs
            .lock_recover()
            .remove(&operation_id);
        Ok(())
    })
}

#[tauri::command]
pub(crate) fn approve_queue_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
    expected_attention: doppeldecker_core::QueueAttention,
    effects_confirmed: bool,
) -> Result<FrontendQueueSnapshot, AppError> {
    resume_queue(app, &window, &state, |queue| {
        queue
            .approve_attention_effects(&operation_id, &expected_attention, effects_confirmed)
            .map_err(AppError::from)
    })
}
