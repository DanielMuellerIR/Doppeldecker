use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{Manager, State, WebviewWindow};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::{finish_operation, operation_queue, reserve_operation, AppError, AppState, MutexExt};

const UPDATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Ein geprüfter Release bleibt absichtlich nur im Speicher und gehört genau dem
/// Fenster, das ihn angefordert hat. Download-URL, Signatur und Zielart können so
/// weder vom Frontend erfunden noch über einen Neustart hinweg wiederverwendet werden.
#[derive(Default)]
pub(crate) struct UpdateState {
    inner: Mutex<UpdateStateInner>,
}

#[derive(Default)]
struct UpdateStateInner {
    pending: HashMap<String, PreparedUpdate>,
    busy: HashSet<String>,
    /// Download und Austausch betreffen dieselbe installierte App. Deshalb darf
    /// unabhängig vom Fensterlabel genau ein Installationslauf aktiv sein.
    installing_owner: Option<String>,
    /// Jede erfolgreiche Installation entwertet alle früher ausgegebenen Funde.
    /// Die Generation macht diese Grenze auch für verspätete Fensterantworten
    /// eindeutig, selbst wenn deren lokale Dialoge noch sichtbar sind.
    generation: u64,
}

struct PreparedUpdate {
    generation: u64,
    update: Update,
}

#[derive(Serialize)]
pub(crate) struct UpdateCheck {
    available: bool,
    current_version: String,
    version: Option<String>,
    notes: Option<String>,
    date: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct PreparedUpdateStatus {
    available: bool,
    version: Option<String>,
}

#[derive(Clone, Serialize)]
struct UpdateProgress {
    downloaded: u64,
    total: Option<u64>,
}

fn updater_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::new("update_failed", format!("{context}: {error}"))
}

fn begin(state: &UpdateState, label: &str) -> Result<u64, AppError> {
    let mut inner = state.inner.lock_recover();
    if inner.installing_owner.is_some() || !inner.busy.is_empty() {
        return Err(AppError::new(
            "busy",
            "In einem anderen Fenster läuft bereits eine Update-Aktion",
        ));
    }
    inner.busy.insert(label.to_owned());
    Ok(inner.generation)
}

fn begin_install(state: &UpdateState, label: &str) -> Result<u64, AppError> {
    let mut inner = state.inner.lock_recover();
    if !inner.busy.is_empty() {
        return Err(AppError::new(
            "busy",
            "In einem Fenster läuft noch eine Updateprüfung",
        ));
    }
    if inner.installing_owner.is_some() {
        return Err(AppError::new(
            "busy",
            "In einem anderen Fenster wird Doppeldecker bereits aktualisiert",
        ));
    }
    inner.busy.insert(label.to_owned());
    inner.installing_owner = Some(label.to_owned());
    Ok(inner.generation)
}

fn finish_install(state: &UpdateState, label: &str) {
    let mut inner = state.inner.lock_recover();
    if inner.installing_owner.as_deref() == Some(label) {
        inner.installing_owner = None;
    }
    inner.busy.remove(label);
}

fn require_current_generation(generation: u64, expected: u64) -> Result<(), AppError> {
    if generation == expected {
        Ok(())
    } else {
        Err(AppError::new(
            "plan_drift",
            "Eine erfolgreich installierte Version hat diese verspätete Updateprüfung entwertet",
        ))
    }
}

fn invalidate_pending<T>(pending: &mut HashMap<String, T>, generation: &mut u64) {
    pending.clear();
    *generation = generation.wrapping_add(1);
}

fn invalidate_installed_update(state: &UpdateState) {
    let mut inner = state.inner.lock_recover();
    let UpdateStateInner {
        pending,
        generation,
        ..
    } = &mut *inner;
    invalidate_pending(pending, generation);
}

/// Prüft nur auf ausdrücklichen Menüklick. Es gibt weder einen Start-Hook noch
/// einen Timer: Doppeldecker fragt den Updatekanal nie ungefragt im Hintergrund ab.
#[tauri::command]
pub(crate) async fn check_for_update(
    window: WebviewWindow,
    state: State<'_, UpdateState>,
) -> Result<UpdateCheck, AppError> {
    let label = window.label().to_owned();
    let check_generation = begin(&state, &label)?;
    let result = match window.updater_builder().timeout(UPDATE_TIMEOUT).build() {
        Ok(updater) => updater
            .check()
            .await
            .map_err(|error| updater_error("Updateprüfung fehlgeschlagen", error)),
        Err(error) => Err(updater_error(
            "Updatekonfiguration ist unvollständig",
            error,
        )),
    };

    let current_version = window.app_handle().package_info().version.to_string();
    let mut inner = state.inner.lock_recover();
    inner.busy.remove(&label);
    require_current_generation(inner.generation, check_generation)?;
    match result? {
        Some(update) => {
            let response = UpdateCheck {
                available: true,
                current_version,
                version: Some(update.version.clone()),
                notes: update.body.clone(),
                date: update.date.map(|date| date.to_string()),
            };
            inner.pending.insert(
                label,
                PreparedUpdate {
                    generation: check_generation,
                    update,
                },
            );
            Ok(response)
        }
        None => {
            inner.pending.remove(&label);
            Ok(UpdateCheck {
                available: false,
                current_version,
                version: None,
                notes: None,
                date: None,
            })
        }
    }
}

/// Klärt nach einem abgebrochenen IPC-Aufruf ausschließlich, ob der zuvor
/// vorbereitete Handle im Backend noch vorhanden ist. Das Frontend darf aus
/// einem Transportfehler allein nicht ableiten, ob `install_update` bereits lief.
#[tauri::command]
pub(crate) fn prepared_update_status(
    window: WebviewWindow,
    state: State<'_, UpdateState>,
) -> PreparedUpdateStatus {
    let inner = state.inner.lock_recover();
    let update = inner
        .pending
        .get(window.label())
        .filter(|prepared| prepared.generation == inner.generation);
    PreparedUpdateStatus {
        available: update.is_some(),
        version: update.map(|prepared| prepared.update.version.clone()),
    }
}

/// Lädt und installiert ausschließlich den zuvor in demselben Fenster geprüften
/// Release. Die Versionswiederholung bindet die sichtbare Bestätigung an genau
/// diesen Stand. Währenddessen hält die normale Fenster-Single-Flight-Schranke
/// Queue- und Dateioperationen fern; andere Fenster bleiben unabhängig bedienbar.
#[tauri::command]
pub(crate) async fn install_update(
    window: WebviewWindow,
    state: State<'_, UpdateState>,
    app_state: State<'_, AppState>,
    expected_version: String,
) -> Result<(), AppError> {
    let label = window.label().to_owned();
    let install_generation = begin_install(&state, &label)?;
    {
        let inner = state.inner.lock_recover();
        match inner.pending.get(&label) {
            Some(prepared)
                if prepared.generation == install_generation
                    && prepared.update.version == expected_version => {}
            Some(_) => {
                drop(inner);
                finish_install(&state, &label);
                return Err(AppError::new(
                    "plan_drift",
                    "Die bestätigte Updateversion stimmt nicht mehr mit der geprüften Version überein",
                ));
            }
            None => {
                drop(inner);
                finish_install(&state, &label);
                return Err(AppError::new(
                    "plan_drift",
                    "Vor der Installation muss die Updateprüfung erneut ausgeführt werden",
                ));
            }
        }
    };

    let window_state = app_state.window(&label);
    // Das Öffnen der Queue bindet zugleich den gemeinsamen externen Prozesslock.
    // Erst danach darf die Installation die fenstereigene Schranke reservieren.
    if let Err(error) = operation_queue(window.app_handle(), &window_state, &label)
        .and_then(|_| reserve_operation(&window_state).map(|_| ()))
    {
        finish_install(&state, &label);
        return Err(error);
    }

    // Erst die erfolgreich reservierte Installation verbraucht den Handle. Ein
    // Queue-/Single-Flight-Fehler davor lässt die sichtbare Vorbereitung intakt.
    let update = {
        let mut inner = state.inner.lock_recover();
        match inner.pending.remove(&label) {
            Some(prepared)
                if prepared.generation == install_generation
                    && prepared.update.version == expected_version =>
            {
                prepared.update
            }
            Some(prepared) => {
                inner.pending.insert(label.clone(), prepared);
                drop(inner);
                finish_operation(window.app_handle(), &label, || {
                    let _ = crate::emit_to_window(
                        &window,
                        "update-error",
                        "Die vorbereitete Updateversion hat sich unerwartet geändert",
                    );
                });
                finish_install(&state, &label);
                return Err(AppError::new(
                    "plan_drift",
                    "Die vorbereitete Updateversion hat sich unerwartet geändert",
                ));
            }
            None => {
                drop(inner);
                finish_operation(window.app_handle(), &label, || {
                    let _ = crate::emit_to_window(
                        &window,
                        "update-error",
                        "Der vorbereitete Updatefund ist nicht mehr vorhanden",
                    );
                });
                finish_install(&state, &label);
                return Err(AppError::new(
                    "plan_drift",
                    "Der vorbereitete Updatefund ist nicht mehr vorhanden",
                ));
            }
        }
    };

    let progress_window = window.clone();
    let mut downloaded = 0_u64;
    let result = update
        .download_and_install(
            move |chunk, total| {
                downloaded = downloaded.saturating_add(chunk as u64);
                let _ = crate::emit_to_window(
                    &progress_window,
                    "update-progress",
                    UpdateProgress { downloaded, total },
                );
            },
            || {},
        )
        .await
        .map_err(|error| {
            updater_error("Signierter Release konnte nicht installiert werden", error)
        });

    finish_operation(window.app_handle(), &label, || match &result {
        Ok(()) => {
            let _ = crate::emit_to_window(&window, "update-done", ());
        }
        Err(error) => {
            let _ = crate::emit_to_window(&window, "update-error", error.message.clone());
        }
    });
    if result.is_ok() {
        // Die installierte App ist prozessweit. Ein in einem anderen Fenster noch
        // sichtbarer alter Fund darf danach niemals einen zweiten Installstart
        // auslösen; sein Backend-Handle und seine Generation sind hier verbraucht.
        invalidate_installed_update(&state);
    }
    finish_install(&state, &label);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_schranke_ist_appweit_und_wird_freigegeben() {
        let state = UpdateState::default();
        begin(&state, "main").unwrap();
        assert_eq!(begin(&state, "main").unwrap_err().code, "busy");
        assert_eq!(begin(&state, "window-1").unwrap_err().code, "busy");

        state.inner.lock_recover().busy.remove("main");
        begin(&state, "main").unwrap();
    }

    #[test]
    fn pruefung_und_installation_sind_gemeinsam_appweit_single_flight() {
        let state = UpdateState::default();
        begin(&state, "window-1").unwrap();
        assert_eq!(begin_install(&state, "main").unwrap_err().code, "busy");
        state.inner.lock_recover().busy.remove("window-1");

        begin_install(&state, "main").unwrap();
        assert_eq!(begin_install(&state, "window-1").unwrap_err().code, "busy");
        assert_eq!(begin(&state, "window-1").unwrap_err().code, "busy");

        finish_install(&state, "main");
        begin_install(&state, "window-1").unwrap();
    }

    #[test]
    fn erfolgreiche_installation_entwertet_alle_alten_generationen() {
        let state = UpdateState::default();
        let late_generation = begin(&state, "verspaetetes-fenster").unwrap();
        state
            .inner
            .lock_recover()
            .busy
            .remove("verspaetetes-fenster");
        invalidate_installed_update(&state);
        let current = state.inner.lock_recover().generation;
        assert_eq!(
            require_current_generation(current, late_generation)
                .unwrap_err()
                .code,
            "plan_drift"
        );

        // Derselbe Produktionshelfer entfernt nicht nur den installierenden
        // Handle, sondern jeden Fund anderer Fenster in einem Schritt.
        let mut foreign_handles =
            HashMap::from([("window-1".to_owned(), ()), ("window-2".to_owned(), ())]);
        let mut generation = 7;
        invalidate_pending(&mut foreign_handles, &mut generation);
        assert!(foreign_handles.is_empty());
        assert_eq!(generation, 8);
    }
}
