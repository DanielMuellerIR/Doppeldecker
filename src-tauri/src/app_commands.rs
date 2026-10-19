//! Allgemeine App-Metadaten und der appweite Beenden-Vertrag.

use std::sync::atomic::Ordering;

use tauri::{Manager, State};

use crate::{operation_queue, AppError, AppState, MutexExt};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExitFailure {
    pub(crate) label: String,
    pub(crate) code: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExitWindowSnapshot {
    pub(crate) label: String,
    pub(crate) active_operation: bool,
    pub(crate) pending_preparations: Vec<String>,
    pub(crate) open_entries: Option<Vec<ExitQueueEntry>>,
    pub(crate) failure: Option<ExitFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExitQueueEntry {
    operation_id: String,
    state: String,
}

/// Vollständiger, sortierter Zustand aller bekannten Fenster. Genau dieser Wert
/// wird an die Beenden-Bestätigung gebunden und unmittelbar vor `app.exit` sowie
/// im nativen Exit-Ereignis nochmals verglichen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExitSnapshot {
    pub(crate) windows: Vec<ExitWindowSnapshot>,
    /// Dieses Zeichen verrät weder Profil noch Secret. Es verhindert nur, dass
    /// der Prozess mitten zwischen Schlüsselbund- und Profildatei-Wirkung endet.
    pub(crate) profile_transaction_active: bool,
}

impl ExitSnapshot {
    pub(crate) fn open_entries(&self) -> usize {
        self.windows
            .iter()
            .filter_map(|window| window.open_entries.as_ref())
            .map(Vec::len)
            .sum()
    }

    pub(crate) fn has_active_operations(&self) -> bool {
        self.profile_transaction_active || self.windows.iter().any(|window| window.active_operation)
    }

    pub(crate) fn failures(&self) -> impl Iterator<Item = &ExitFailure> {
        self.windows
            .iter()
            .filter_map(|window| window.failure.as_ref())
    }

    pub(crate) fn has_failures(&self) -> bool {
        self.failures().next().is_some()
    }

    pub(crate) fn has_pending_preparations(&self) -> bool {
        self.windows
            .iter()
            .any(|window| !window.pending_preparations.is_empty())
    }

    pub(crate) fn must_prevent_exit(&self) -> bool {
        self.open_entries() > 0
            || self.has_active_operations()
            || self.has_pending_preparations()
            || self.has_failures()
    }

    pub(crate) fn can_confirm_open_queues(&self) -> bool {
        self.open_entries() > 0
            && !self.has_active_operations()
            && !self.has_pending_preparations()
            && !self.has_failures()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PendingExitConfirmation {
    token: String,
    snapshot: ExitSnapshot,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueueExitRequest {
    pub(crate) token: String,
    pub(crate) open_entries: usize,
}

/// Testbare Erfassung: Ein Journalfehler bleibt Bestandteil des Snapshots, statt
/// wie eine leere Queue behandelt zu werden.
#[cfg(test)]
pub(crate) fn assess_exit(
    labels: &[String],
    profile_transaction_active: bool,
    mut inspect: impl FnMut(&str) -> (bool, Result<Vec<(String, String)>, AppError>),
) -> ExitSnapshot {
    assess_exit_with_pending(labels, profile_transaction_active, |label| {
        let (active, queue) = inspect(label);
        (active, Vec::new(), queue)
    })
}

pub(crate) fn assess_exit_with_pending(
    labels: &[String],
    profile_transaction_active: bool,
    mut inspect: impl FnMut(&str) -> (bool, Vec<String>, Result<Vec<(String, String)>, AppError>),
) -> ExitSnapshot {
    let mut labels = labels.to_vec();
    labels.sort();
    labels.dedup();
    let windows = labels
        .into_iter()
        .map(|label| {
            let (active_operation, mut pending_preparations, queue) = inspect(&label);
            pending_preparations.sort();
            pending_preparations.dedup();
            match queue {
                Ok(mut entries) => {
                    entries.sort();
                    ExitWindowSnapshot {
                        label,
                        active_operation,
                        pending_preparations,
                        open_entries: Some(
                            entries
                                .into_iter()
                                .map(|(operation_id, state)| ExitQueueEntry {
                                    operation_id,
                                    state,
                                })
                                .collect(),
                        ),
                        failure: None,
                    }
                }
                Err(error) => ExitWindowSnapshot {
                    failure: Some(ExitFailure {
                        label: label.clone(),
                        code: error.code,
                        message: error.message,
                    }),
                    label,
                    active_operation,
                    pending_preparations,
                    open_entries: None,
                },
            }
        })
        .collect();
    ExitSnapshot {
        windows,
        profile_transaction_active,
    }
}

pub(crate) fn assess_app_exit(app: &tauri::AppHandle, state: &AppState) -> ExitSnapshot {
    let mut labels = state.window_labels();
    labels.extend(app.webview_windows().into_keys());
    if labels.is_empty() {
        labels.push("main".into());
    }
    assess_exit_with_pending(
        &labels,
        state.profile_transaction_active.load(Ordering::Acquire),
        |label| {
            let window_state = state.window(label);
            let active = window_state.active_cancel.lock_recover().is_some();
            let pending = window_state.pending_preparation_kinds();
            let queue = operation_queue(app, &window_state, label).map(|queue| {
                queue
                    .snapshot()
                    .entries
                    .iter()
                    .filter(|entry| crate::queue_state_needs_exit_warning(entry.state))
                    .map(|entry| (entry.operation_id.clone(), format!("{:?}", entry.state)))
                    .collect()
            });
            (active, pending, queue)
        },
    )
}

pub(crate) fn issue_exit_confirmation(
    state: &AppState,
    snapshot: ExitSnapshot,
) -> QueueExitRequest {
    let sequence = state
        .exit_confirmation_sequence
        .fetch_add(1, Ordering::Relaxed);
    let token = format!("queue-exit-{sequence}");
    *state.pending_exit_confirmation.lock_recover() = Some(PendingExitConfirmation {
        token: token.clone(),
        snapshot: snapshot.clone(),
    });
    QueueExitRequest {
        token,
        open_entries: snapshot.open_entries(),
    }
}

pub(crate) fn clear_exit_confirmation(state: &AppState, token: &str) {
    let mut pending = state.pending_exit_confirmation.lock_recover();
    if pending
        .as_ref()
        .is_some_and(|pending| pending.token == token)
    {
        pending.take();
    }
}

fn authorize_confirmed_exit(
    state: &AppState,
    token: &str,
    current: ExitSnapshot,
) -> Result<(), AppError> {
    // Nur das exakt passende Token wird atomar entnommen. Ein verspäteter Aufruf
    // eines älteren Dialogs darf die neuere gültige Bestätigung nicht zerstören.
    let mut slot = state.pending_exit_confirmation.lock_recover();
    let Some(pending) = slot.as_ref() else {
        return Err(AppError::new(
            "confirmation_mismatch",
            "Die Beenden-Bestätigung fehlt oder wurde bereits verwendet",
        ));
    };
    if pending.token != token {
        return Err(AppError::new(
            "confirmation_mismatch",
            "Die Beenden-Bestätigung gehört zu einem anderen Hinweis",
        ));
    }
    let pending = slot.take().expect("unmittelbar zuvor geprüft");
    drop(slot);
    if pending.snapshot != current || !current.can_confirm_open_queues() {
        return Err(AppError::new(
            "state_changed",
            "Queue- oder Operationszustand hat sich seit dem Beenden-Hinweis geändert",
        ));
    }
    *state.authorized_exit.lock_recover() = Some(current);
    Ok(())
}

pub(crate) fn take_matching_exit_authorization(state: &AppState, current: &ExitSnapshot) -> bool {
    state
        .authorized_exit
        .lock_recover()
        .take()
        .is_some_and(|authorized| authorized == *current && current.can_confirm_open_queues())
}

/// Produktversion (aus `Cargo.toml`), damit die Oberfläche sie anzeigen kann, ohne
/// die Nummer doppelt zu pflegen. Das Frontend zeigt sie klein im Fensterkopf.
#[tauri::command]
pub(crate) fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Beendet die App erst nach der ausdrücklichen Bestätigung im Frontend. Der
/// normale Beenden-Versuch bleibt in `main.rs` abgefangen. Auch der anschließende
/// `AppHandle::exit` wird dort nochmals gegen den einmal autorisierten, unveränderten
/// appweiten Snapshot geprüft.
#[tauri::command]
pub(crate) fn quit_after_queue_warning(
    app: tauri::AppHandle,
    state: State<AppState>,
    token: String,
) -> Result<(), AppError> {
    let barrier = state.exit_barrier.lock_recover();
    state.exit_in_progress.store(true, Ordering::Release);
    let current = assess_app_exit(&app, &state);
    if let Err(error) = authorize_confirmed_exit(&state, &token, current) {
        crate::queue_runtime::reopen_app_operation_gate(&app, &state);
        return Err(error);
    }
    drop(barrier);
    app.exit(0);
    Ok(())
}

/// Persistiert eine geheimnisfreie Fehlerklasse für spätere Diagnose. Der eigentliche
/// Fehlertext bleibt bewusst draußen, weil Providerantworten sensible Werte enthalten
/// können.
#[tauri::command]
pub(crate) fn record_app_error(
    app: tauri::AppHandle,
    context: String,
    code: Option<String>,
) -> Result<(), crate::AppError> {
    crate::app_support::record_app_error(&app, &context, code.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(ids: &[&str], active: bool) -> ExitSnapshot {
        assess_exit(&["main".into()], false, |_| {
            (
                active,
                Ok(ids
                    .iter()
                    .map(|id| ((*id).to_owned(), "Pending".to_owned()))
                    .collect()),
            )
        })
    }

    #[test]
    fn assessment_erfasst_queue_active_cancel_und_unsichtbaren_journalfehler() {
        let result = assess_exit(
            &["window-z".into(), "main".into()],
            false,
            |label| match label {
                "main" => (
                    true,
                    Ok(vec![
                        ("op-1".into(), "Running".into()),
                        ("op-2".into(), "Pending".into()),
                    ]),
                ),
                _ => (false, Err(AppError::new("io", "Journal beschädigt"))),
            },
        );

        assert_eq!(result.open_entries(), 2);
        assert!(result.has_active_operations());
        assert!(result.has_failures());
        assert_eq!(result.failures().next().unwrap().label, "window-z");
        assert!(!result.can_confirm_open_queues());
    }

    #[test]
    fn pending_vorbereitung_blockiert_exit_und_ist_nicht_bestaetigbar() {
        let snapshot = assess_exit_with_pending(&["main".into()], false, |_| {
            (false, vec!["bidirectional-real".to_owned()], Ok(Vec::new()))
        });

        assert!(snapshot.must_prevent_exit());
        assert!(snapshot.has_pending_preparations());
        assert!(!snapshot.can_confirm_open_queues());
        assert_eq!(
            snapshot.windows[0].pending_preparations,
            ["bidirectional-real"]
        );
    }

    #[test]
    fn fehlgeschlagenes_discard_laesst_pending_exit_fail_closed() {
        let mut pending = vec!["bidirectional-real".to_owned()];
        let discard: Result<(), AppError> = Err(AppError::new("io", "Journal blieb erhalten"));
        if discard.is_ok() {
            pending.clear();
        }
        let blocked = assess_exit_with_pending(&["main".into()], false, |_| {
            (false, pending.clone(), Ok(Vec::new()))
        });
        assert!(blocked.must_prevent_exit());

        pending.clear();
        let clean = assess_exit_with_pending(&["main".into()], false, |_| {
            (false, pending.clone(), Ok(Vec::new()))
        });
        assert!(!clean.must_prevent_exit());
    }

    #[test]
    fn bestaetigung_ist_an_snapshot_gebunden_und_nur_einmal_gueltig() {
        let state = AppState::default();
        let shown = snapshot(&["op-1", "op-2"], false);
        let request = issue_exit_confirmation(&state, shown.clone());

        assert_eq!(
            authorize_confirmed_exit(
                &state,
                &request.token,
                snapshot(&["op-1", "op-2", "op-3"], false)
            )
            .unwrap_err()
            .code,
            "state_changed"
        );
        assert!(authorize_confirmed_exit(&state, &request.token, shown.clone()).is_err());

        let request = issue_exit_confirmation(&state, shown.clone());
        authorize_confirmed_exit(&state, &request.token, shown.clone()).unwrap();
        assert!(take_matching_exit_authorization(&state, &shown));
        assert!(!take_matching_exit_authorization(&state, &shown));
    }

    #[test]
    fn aktive_operation_oder_journalfehler_kann_nicht_bestaetigt_werden() {
        let state = AppState::default();
        for current in [
            snapshot(&["op-1"], true),
            assess_exit(&["window-defekt".into()], false, |_| {
                (false, Err(AppError::new("io", "Journal beschädigt")))
            }),
        ] {
            let request = issue_exit_confirmation(&state, current.clone());
            assert_eq!(
                authorize_confirmed_exit(&state, &request.token, current)
                    .unwrap_err()
                    .code,
                "state_changed"
            );
        }
    }

    #[test]
    fn gleiche_anzahl_mit_anderen_operationen_entwertet_die_bestaetigung() {
        let state = AppState::default();
        let shown = snapshot(&["alt-1", "alt-2"], false);
        let request = issue_exit_confirmation(&state, shown);

        assert_eq!(
            authorize_confirmed_exit(&state, &request.token, snapshot(&["neu-1", "neu-2"], false),)
                .unwrap_err()
                .code,
            "state_changed"
        );
    }

    #[test]
    fn altes_falsches_token_verbraucht_neue_bestaetigung_nicht() {
        let state = AppState::default();
        let shown = snapshot(&["op-1"], false);
        let request = issue_exit_confirmation(&state, shown.clone());

        assert_eq!(
            authorize_confirmed_exit(&state, "queue-exit-alt", shown.clone())
                .unwrap_err()
                .code,
            "confirmation_mismatch"
        );
        authorize_confirmed_exit(&state, &request.token, shown.clone()).unwrap();
        assert!(take_matching_exit_authorization(&state, &shown));
    }

    #[test]
    fn profiltransaktion_ist_kein_bestaetigbarer_exit_zustand() {
        let state = AppState::default();
        let current = assess_exit(&["main".into()], true, |_| (false, Ok(Vec::new())));
        let request = issue_exit_confirmation(&state, current.clone());

        assert!(current.must_prevent_exit());
        assert!(current.has_active_operations());
        assert!(!current.can_confirm_open_queues());
        assert_eq!(
            authorize_confirmed_exit(&state, &request.token, current)
                .unwrap_err()
                .code,
            "state_changed"
        );
    }
}
