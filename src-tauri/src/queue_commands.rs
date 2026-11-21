//! Bediencommands der persistenten Operations-Queue.

use crate::frontend_payload::FrontendQueueSnapshot;
use std::sync::Arc;

use doppeldecker_core::OperationQueue;
use tauri::{Manager, State, WebviewWindow};

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

#[derive(serde::Serialize)]
struct QueueRecoveryReview {
    reason: doppeldecker_core::queue::QueueRecoveryReason,
    entry: doppeldecker_core::QueueEntry,
}

#[derive(serde::Serialize)]
pub(crate) struct RecoveryOverview {
    queue: Option<Vec<crate::frontend_payload::FrontendPayload<QueueRecoveryReview>>>,
    trash: Result<Vec<doppeldecker_core::TrashJournalReview>, AppError>,
}

#[tauri::command]
pub(crate) async fn inspect_recovery(
    app: tauri::AppHandle,
    window: WebviewWindow,
) -> Result<RecoveryOverview, AppError> {
    let window_state = app.state::<AppState>().window(window.label());
    tauri::async_runtime::spawn_blocking(move || {
        // Den Blick in die Queue nicht mit dem normalen Öffnen der Queue
        // verwechseln: Dort dürfen Altjournale migriert und Worker gestartet werden.
        let queue = crate::queue_runtime::ready_queue_snapshot(&window_state).map(|snapshot| {
            snapshot
                .entries
                .into_iter()
                .filter_map(|entry| {
                    let reason = entry.recovery_reason()?;
                    Some(QueueRecoveryReview { reason, entry }.into())
                })
                .collect()
        });
        let trash = (|| {
            let state = app.state::<AppState>();
            let _guard = state.trash_journal_guard.try_lock().map_err(|_| {
                AppError::new(
                    "busy",
                    "Papierkorb wird gerade verarbeitet. Erneut aktualisieren.",
                )
            })?;
            let directory =
                crate::app_support::app_config_dir_for_inspection(&app)?.join("trash-journals");
            doppeldecker_core::inspect_trash_journals(&directory).map_err(AppError::from)
        })();
        RecoveryOverview { queue, trash }
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))
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
    let cleanup = crate::queue_runtime::reserve_queue_cleanup(&state, &window_state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _cleanup = cleanup;
        let result = (|| {
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
        })();
        // Vor Freigabe der Barriere auch den Fehler samt Recoverypfaden melden.
        // Das Command-Future könnte inzwischen vom Aufrufer verworfen worden sein.
        if let Err(error) = &result {
            let _ = crate::emit_to_window_label(&app, &label, "queue-error", error);
        }
        result
    })
    .await
    .map_err(|_| AppError::new("outcome_unclear", "Queue-Bereinigung unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn discard_resumable_copy(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
    operation_id: String,
) -> Result<FrontendQueueSnapshot, AppError> {
    let label = window.label().to_owned();
    let window_state = state.window(&label);
    let queue = operation_queue(&app, &window_state, &label)?;
    let sandbox = crate::sandbox_root()?;
    let cleanup = crate::queue_runtime::reserve_queue_cleanup(&state, &window_state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let result = queue.discard_resumable_copy(&operation_id, sandbox.as_deref());
        emit_queue_snapshot(&app, &label, &queue);
        drop(cleanup);
        start_queue_worker(app, label);
        result.map_err(AppError::from)?;
        Ok(queue.snapshot().into())
    })
    .await
    .map_err(|_| AppError::new("outcome_unclear", "Bereinigung der Teilkopie unterbrochen"))?
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

#[cfg(test)]
#[test]
fn queue_approval_uses_exact_projected_attention_and_mode_digest() {
    use crate::content_sync_commands::ContentSyncPreparationDigest;
    use crate::normal_sync_commands::LocalSyncPreparationDigest;
    use doppeldecker_core::{OperationQueue, QueueAttention, QueueOperation, QueueState, SyncPlan};
    use serde_json::{json, Value};

    let fixture: Value =
        serde_json::from_str(include_str!("../../tools/tests/queue-approval-wire.json")).unwrap();
    let local_digest: LocalSyncPreparationDigest =
        serde_json::from_value(fixture["local"]["preparationDigest"].clone()).unwrap();
    let content_digest: ContentSyncPreparationDigest =
        serde_json::from_value(fixture["content"]["preparationDigest"].clone()).unwrap();
    assert_eq!(local_digest, [5; 32]);
    assert_eq!(content_digest, [9; 32]);

    let plan_fixture: Value =
        serde_json::from_str(include_str!("../../tools/tests/local-sync-wire.json")).unwrap();
    let current_plan: SyncPlan = serde_json::from_value(plan_fixture["plan"].clone()).unwrap();
    let attention = QueueAttention::PlanChanged {
        reason: "Ein neuer Pfad".into(),
        confirmation: "gleiche-phrase".into(),
        current_plan: Box::new(current_plan.clone()),
    };
    let temp = tempfile::tempdir().unwrap();
    let queue = OperationQueue::open(temp.path().join("operations-queue.json")).unwrap();
    let entry = queue
        .enqueue_claimed(QueueOperation::Sync {
            source: "/mock/quelle".into(),
            target: "/mock/ziel".into(),
            sandbox: None,
            options: Default::default(),
            expected_plan: Box::default(),
        })
        .unwrap();
    queue
        .require_confirmation(&entry.operation_id, attention.clone())
        .unwrap();
    let payload: FrontendQueueSnapshot = queue.snapshot().into();
    let projected = serde_json::to_value(payload).unwrap();
    let mut expected = fixture["planChanged"].clone();
    expected["current_plan"] = plan_fixture["plan"].clone();
    let proof = &projected["entries"][0]["attention"];
    assert_eq!(proof, &expected);

    // Die Metadatenbilanz wird unverändert im Beleg mitgesendet, auch wenn der
    // kurze Queue-Dialog davon nur die Dateiwirkungszahlen darstellt.
    let mut stale = proof.clone();
    stale["current_plan"]["metadata_report"]["content_changes"] = json!(99);
    let stale: QueueAttention = serde_json::from_value(stale).unwrap();
    let error = queue
        .approve_attention_effects(&entry.operation_id, &stale, true)
        .unwrap_err();
    assert_eq!(
        doppeldecker_core::error_code(&error),
        doppeldecker_core::ErrorCode::PlanDrift
    );
    assert_eq!(
        queue.snapshot().entries[0].state,
        QueueState::AwaitingConfirmation
    );
    let proof: QueueAttention = serde_json::from_value(proof.clone()).unwrap();
    assert_eq!(proof, attention);
    queue
        .approve_attention_effects(&entry.operation_id, &proof, true)
        .unwrap();
    let after = queue.snapshot();
    assert_eq!(after.entries[0].state, QueueState::Pending);
    assert_eq!(after.entries[0].attention, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_overview_matches_the_browser_wire_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tools/tests/recovery-trash-wire.json"))
                .unwrap();
        let entry: doppeldecker_core::QueueEntry = serde_json::from_value(serde_json::json!({
            "operation_id": "review-copy",
            "operation": {
                "type": "File",
                "kind": "Copy",
                "items": ["/mock/quelle/Alpha.txt"],
                "destination": "/mock/ziel",
                "overwrite": false,
                "expected_conflicts": []
            },
            "created_unix_ms": 1_800_000_000_000_u64,
            "state": "Interrupted",
            "attempts": 1,
            "progress": doppeldecker_core::Progress {
                phase: "interrupted".into(),
                ..Default::default()
            },
            "error_code": "outcome_unclear"
        }))
        .unwrap();
        let available = RecoveryOverview {
            queue: Some(vec![QueueRecoveryReview {
                reason: entry.recovery_reason().unwrap(),
                entry,
            }
            .into()]),
            trash: Ok(vec![
                doppeldecker_core::TrashJournalReview {
                    name: "trash-available.json".into(),
                    entry_count: Some(2),
                    effects_unclear: false,
                    error: None,
                },
                doppeldecker_core::TrashJournalReview {
                    name: "trash-probe.json".into(),
                    entry_count: None,
                    effects_unclear: false,
                    error: Some("Prüfjournal nicht lesbar".into()),
                },
                doppeldecker_core::TrashJournalReview {
                    name: "trash-unclear.json".into(),
                    entry_count: Some(1),
                    effects_unclear: true,
                    error: None,
                },
            ]),
        };
        let empty = RecoveryOverview {
            queue: Some(vec![]),
            trash: Ok(vec![]),
        };
        let unavailable = RecoveryOverview {
            queue: None,
            trash: Err(AppError::new(
                "busy",
                "Papierkorb wird gerade verarbeitet. Erneut aktualisieren.",
            )),
        };
        for (name, overview) in [
            ("available", available),
            ("empty", empty),
            ("unavailable", unavailable),
        ] {
            assert_eq!(
                serde_json::to_value(overview).unwrap(),
                fixture["recovery"][name],
                "{name}"
            );
        }
    }
}
