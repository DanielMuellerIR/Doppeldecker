//! Erzeugung zusätzlicher Dual-Pane-Fenster und Übergabe ihres Startzustands.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::{app_config_dir, operation_queue, with_app_extension, AppError, AppState, MutexExt};

static WINDOW_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PaneStartLocation {
    pub(crate) provider: String,
    pub(crate) profile_id: Option<String>,
    #[serde(default)]
    pub(crate) profile_revision: Option<u64>,
    pub(crate) profile_name: Option<String>,
    pub(crate) path: String,
}

impl PaneStartLocation {
    pub(crate) fn validate(&self) -> Result<(), AppError> {
        if !matches!(
            self.provider.as_str(),
            "local" | "sftp" | "ftps" | "webdav" | "smb"
        ) {
            return Err(AppError::new(
                "invalid_input",
                "Unbekannter Provider für den Fensterstart",
            ));
        }
        if self.path.is_empty() || !std::path::Path::new(&self.path).is_absolute() {
            return Err(AppError::new(
                "invalid_input",
                "Ein neues Fenster braucht absolute Pane-Pfade",
            ));
        }
        if self.provider == "local" {
            if self.profile_id.is_some() || self.profile_revision.is_some() {
                return Err(AppError::new(
                    "invalid_input",
                    "Ein lokaler Fensterstart darf keine Profil-ID enthalten",
                ));
            }
        } else {
            if self.profile_id.as_deref().is_none_or(str::is_empty) {
                return Err(AppError::new(
                    "invalid_input",
                    "Ein Remote-Fensterstart braucht eine Profil-ID",
                ));
            }
            if self.profile_revision.is_none() {
                return Err(AppError::new(
                    "plan_drift",
                    "Ein Remote-Fensterstart braucht eine Profilrevision",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WindowStartState {
    pub(crate) left: PaneStartLocation,
    pub(crate) right: PaneStartLocation,
}

impl WindowStartState {
    pub(crate) fn validate(&self) -> Result<(), AppError> {
        self.left.validate()?;
        self.right.validate()
    }
}

fn build_window(app: &tauri::AppHandle, label: &str) -> Result<WebviewWindow, AppError> {
    let builder = WebviewWindowBuilder::new(app, label, WebviewUrl::App("index.html".into()))
        .title("Doppeldecker")
        .inner_size(1180.0, 760.0)
        .min_inner_size(760.0, 480.0);
    // Die Overlay-Titelleiste entspricht dem statisch konfigurierten macOS-Fenster.
    // Auf Linux existieren diese beiden Builder-Methoden nicht; dort bleibt die
    // native Fenstertitelleiste sichtbar.
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true);
    let window = builder
        .build()
        .map_err(|error| AppError::new("window", error.to_string()))?;
    // Ab hier existiert die Webview sichtbar. Ein gescheitertes Fenster-Icon ist
    // rein kosmetisch und darf den Aufrufer nicht mit „Fehler“ zurücklassen,
    // während das Fenster längst steht; es bleibt im geheimnisfreien Log.
    if let Err(error) = crate::desktop_settings::apply_window_icon(&window) {
        let _ = crate::app_support::record_app_error(app, "Fenstericon", Some(&error.code));
    }
    apply_restored_geometry(&window);
    Ok(window)
}

/// Logisches Rechteck (x, y, Breite, Höhe) eines Monitors oder Fensters.
type LogicalRect = (f64, f64, f64, f64);

/// Mindestens so viel Fensterfläche muss auf einem Monitor liegen, damit die
/// gespeicherte Position übernommen wird: genug für Titelleiste und Griff, um das
/// Fenster per Maus zurückzuholen.
const MIN_VISIBLE_WIDTH: f64 = 120.0;
const MIN_VISIBLE_HEIGHT: f64 = 40.0;

/// Prüft, ob das Fenster mit dieser Geometrie auf einem der Monitore sichtbar
/// wäre. Eine auf einem inzwischen abgesteckten Display gespeicherte Position
/// würde das Fenster sonst komplett außerhalb des Bildschirms ablegen, und ohne
/// Abschalten der Wiederherstellung käme man nie wieder heran.
fn geometry_visible_on(window: LogicalRect, monitors: &[LogicalRect]) -> bool {
    let (wx, wy, ww, wh) = window;
    monitors.iter().any(|&(mx, my, mw, mh)| {
        let overlap_width = (wx + ww).min(mx + mw) - wx.max(mx);
        // Nur der obere Griffstreifen zählt. Ein Fenster, dessen unterer Rand von
        // einem früher oberhalb angeordneten Monitor hereinragt, besitzt zwar
        // sichtbare Fläche, aber keine erreichbare Titelleiste.
        let grip_bottom = (wy + MIN_VISIBLE_HEIGHT).min(wy + wh);
        let overlap_height = grip_bottom.min(my + mh) - wy.max(my);
        overlap_width >= MIN_VISIBLE_WIDTH && overlap_height >= MIN_VISIBLE_HEIGHT
    })
}

fn available_monitor_rects(window: &WebviewWindow) -> Vec<LogicalRect> {
    window
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|monitor| {
            let scale = monitor.scale_factor();
            let position = monitor.position().to_logical::<f64>(scale);
            let size = monitor.size().to_logical::<f64>(scale);
            (position.x, position.y, size.width, size.height)
        })
        .collect()
}

pub(crate) fn apply_restored_geometry(window: &WebviewWindow) {
    let Some(geometry) =
        crate::desktop_settings::take_restored_geometry(window.app_handle(), window.label())
    else {
        return;
    };
    // Unsere Formatprüfung hält absurde und nicht endliche Werte aus der
    // Einstellungsdatei vom Fenster-Backend fern. Scheitert eine einzelne
    // Geometrieoperation, bleibt dessen sichere Standarddarstellung erhalten.
    let rect = (geometry.x, geometry.y, geometry.width, geometry.height);
    let monitors = available_monitor_rects(window);
    // Ohne Monitorliste (etwa in einer Headless-Umgebung) gilt die Position als
    // verwendbar; nur ein nachweislich unsichtbares Rechteck wird verworfen und
    // behält Größe und Maximierung.
    if monitors.is_empty() || geometry_visible_on(rect, &monitors) {
        let _ = window.set_position(tauri::LogicalPosition::new(geometry.x, geometry.y));
    }
    let _ = window.set_size(tauri::LogicalSize::new(geometry.width, geometry.height));
    if geometry.maximized {
        let _ = window.maximize();
    }
}

fn next_window_label(app: &tauri::AppHandle) -> Result<String, AppError> {
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AppError::new("window", "Systemzeit liegt vor der Unix-Epoche"))?
        .as_millis();
    let config = app_config_dir(app)?;
    loop {
        let sequence = WINDOW_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let label = format!("window-{epoch:x}-{sequence:x}");
        let journal = config.join(format!("operations-queue-{label}.json"));
        // Projektvertrag: Nur ein echtes `NotFound` zählt als Abwesenheit. Ein
        // Berechtigungs- oder IO-Fehler würde mit `exists()` wie „frei“ wirken
        // und das Label eines fremden, nicht lesbaren Journals übernehmen.
        let journal_absent = match std::fs::symlink_metadata(&journal) {
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(AppError::from(error)),
        };
        if app.get_webview_window(&label).is_none() && journal_absent {
            return Ok(label);
        }
    }
}

#[tauri::command]
pub(crate) fn new_app_window(
    app: tauri::AppHandle,
    state: State<AppState>,
    left: PaneStartLocation,
    right: PaneStartLocation,
) -> Result<String, AppError> {
    let initial = WindowStartState { left, right };
    initial.validate()?;
    with_app_extension(&state, || {
        let label = next_window_label(&app)?;
        *state.window(&label).initial_panes.lock_recover() = Some(initial);
        build_window(&app, &label)?;
        Ok(label)
    })
}

pub(crate) fn new_default_app_window(app: &tauri::AppHandle) -> Result<String, AppError> {
    let defaults = crate::browser_commands::default_dirs()?;
    let local = |path| PaneStartLocation {
        provider: "local".into(),
        profile_id: None,
        profile_revision: None,
        profile_name: None,
        path,
    };
    let initial = WindowStartState {
        left: local(defaults.left),
        right: local(defaults.right),
    };
    let state = app.state::<AppState>();
    with_app_extension(&state, || {
        let label = next_window_label(app)?;
        *state.window(&label).initial_panes.lock_recover() = Some(initial);
        build_window(app, &label)?;
        Ok(label)
    })
}

#[tauri::command]
pub(crate) fn take_window_start_state(
    window: WebviewWindow,
    state: State<AppState>,
) -> Option<WindowStartState> {
    state
        .window(window.label())
        .initial_panes
        .lock_recover()
        .take()
}

/// Wählt das Fenster, das zum neuen `main` wird, wenn das frühere Hauptfenster
/// bereits einzeln geschlossen war. Nur ein Nebenfenster ohne offenes Queue-Journal
/// kommt in Frage: Sein Journal bleibt unter dem alten Label, und ein Recovery-
/// Fenster für dieses Label zeigte sonst die offene Queue mit Standardpanes,
/// während `main` dessen Panes an der falschen Queue anzeigte. Gibt es kein
/// sicher beendetes Nebenfenster, startet `main` mit Standardpanes.
fn promote_session_main(
    mut saved: BTreeMap<String, crate::desktop_settings::SavedWindow>,
    labels_with_unavailable_queue: &BTreeSet<String>,
) -> BTreeMap<String, crate::desktop_settings::SavedWindow> {
    if saved.is_empty() || saved.contains_key("main") {
        return saved;
    }
    let candidate = saved
        .keys()
        .find(|label| !labels_with_unavailable_queue.contains(*label))
        .cloned();
    if let Some(label) = candidate {
        let window = saved.remove(&label).unwrap();
        saved.insert("main".into(), window);
    }
    saved
}

/// Bindet die gespeicherten Pane-Ziele vor dem ersten Fensterbau an die jeweilige
/// Fensterqueue. War das frühere Hauptfenster bereits einzeln geschlossen, wird
/// ein sicher beendetes Nebenfenster zum neuen `main`, damit kein zusätzliches
/// leeres Fenster entsteht. Die gespeicherten Fenster sind beim Laden der
/// Einstellungsdatei bereits vollständig validiert.
pub(crate) fn prepare_session_windows(
    app: &tauri::AppHandle,
    labels_with_unavailable_queue: &BTreeSet<String>,
) -> Result<Vec<String>, AppError> {
    let saved = promote_session_main(
        crate::desktop_settings::restore_windows(app),
        labels_with_unavailable_queue,
    );
    let labels = saved.keys().cloned().collect::<Vec<_>>();
    for (label, window) in saved {
        *app.state::<AppState>()
            .window(&label)
            .initial_panes
            .lock_recover() = Some(window.panes.clone());
        crate::desktop_settings::seed_live_window(app, &label, window);
    }
    Ok(labels)
}

pub(crate) fn show_session_windows(app: &tauri::AppHandle, labels: &[String]) {
    for label in labels {
        if label == "main" || app.get_webview_window(label).is_some() {
            continue;
        }
        if let Err(error) = build_window(app, label) {
            append_startup_app_error(
                &app.state::<AppState>(),
                format!(
                    "Gespeichertes Fenster {label} konnte nicht angezeigt werden: {}",
                    error.message
                ),
            );
            // Das Fenster wurde vor dem Bau in die lebende Sitzung eingetragen.
            // Bliebe es dort, schriebe jede Fensteränderung es erneut in die Datei
            // und jeder Start meldete denselben Fehler. Es fällt deshalb aus der
            // Sitzung, genau wie ein bewusst geschlossenes Fenster.
            let state = app.state::<AppState>();
            let _barrier = state.exit_barrier.lock_recover();
            crate::desktop_settings::schedule_forget_closed_window(app, label, false);
        }
    }
}

fn take_startup_queue_error_for_label(state: &AppState, label: &str) -> Option<String> {
    // Beschädigte Nebenfenster-Journale gehören in genau einen sichtbaren Hinweis
    // des Hauptfensters. Ein Nebenfenster darf den prozessweiten Startbefund weder
    // anzeigen noch verbrauchen.
    if label != "main" {
        return None;
    }
    state.startup_queue_error.lock_recover().take()
}

/// Wird vom Frontend erst nach der vollständigen Eventverdrahtung aufgerufen. Der
/// Wert ist absichtlich einmalig: Ein Neuladen derselben Webview darf denselben
/// Recovery-Hinweis nicht mehrfach ausgeben.
#[tauri::command]
pub(crate) fn take_startup_queue_error(
    window: WebviewWindow,
    state: State<AppState>,
) -> Option<String> {
    take_startup_queue_error_for_label(&state, window.label())
}

fn take_startup_app_error_for_label(state: &AppState, label: &str) -> Option<String> {
    if label != "main" {
        return None;
    }
    state.startup_app_error.lock_recover().take()
}

/// Liefert nach der Listener-Registrierung einmalig die nicht zur Queue gehörenden
/// Startfehler an das Hauptfenster.
#[tauri::command]
pub(crate) fn take_startup_app_error(
    window: WebviewWindow,
    state: State<AppState>,
) -> Option<String> {
    take_startup_app_error_for_label(&state, window.label())
}

#[derive(Debug)]
pub(crate) struct IncompleteQueueRecovery {
    labels: Vec<String>,
    journal_errors: Vec<(String, String)>,
}

impl IncompleteQueueRecovery {
    /// Weder eine unfertige noch eine unlesbare Queue darf beim Umbenennen eines
    /// Sitzungsfensters von ihrem bisherigen Label getrennt werden.
    pub(crate) fn unavailable_labels(&self) -> BTreeSet<String> {
        self.labels
            .iter()
            .cloned()
            .chain(self.journal_errors.iter().map(|(label, _)| label.clone()))
            .collect()
    }
}

fn recovery_window_label(name: &std::ffi::OsStr) -> Option<String> {
    let name = name.to_str()?;
    let label = name
        .strip_prefix("operations-queue-")?
        .strip_suffix(".json")?;
    // codereview-ok: Produktfenster und Tauri-Capability erlauben ausschließlich
    // `window-*`; andere gültige Journalnamen gehören nicht zu einer GUI-Recovery
    // (2026-09-12).
    if !label.starts_with("window-") {
        return None;
    }
    let expected = crate::queue_runtime::queue_journal_name(label).ok()?;
    (expected == name).then(|| label.to_owned())
}

/// Liest und öffnet alle Nebenfensterjournale vollständig, ohne dabei eine Webview
/// zu erzeugen. Ein später `read_dir`-Fehler kann den Start deshalb beenden, ohne
/// ein bereits sichtbares halbfertiges Fenster zurückzulassen.
fn plan_recovery_entries(
    entries: impl IntoIterator<Item = Result<std::ffi::OsString, AppError>>,
    mut inspect: impl FnMut(&str) -> Result<bool, AppError>,
) -> Result<IncompleteQueueRecovery, AppError> {
    let mut labels = Vec::new();
    let mut journal_errors = Vec::new();
    for entry in entries {
        let name = entry?;
        let Some(label) = recovery_window_label(&name) else {
            continue;
        };
        match inspect(&label) {
            Ok(true) => labels.push(label),
            Ok(false) => {}
            Err(error) => journal_errors.push((label, error.message)),
        }
    }
    labels.sort();
    labels.dedup();
    Ok(IncompleteQueueRecovery {
        labels,
        journal_errors,
    })
}

fn show_recovery_windows_using(
    plan: &IncompleteQueueRecovery,
    mut build: impl FnMut(&str) -> Result<(), AppError>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for label in &plan.labels {
        if let Err(error) = build(label) {
            errors.push(format!("{label}: {}", error.message));
        }
    }
    errors
}

pub(crate) fn append_startup_queue_error(state: &AppState, message: String) {
    let mut slot = state.startup_queue_error.lock_recover();
    *slot = Some(match slot.take() {
        Some(existing) => format!("{existing}; {message}"),
        None => message,
    });
}

pub(crate) fn append_startup_app_error(state: &AppState, message: String) {
    let mut slot = state.startup_app_error.lock_recover();
    *slot = Some(match slot.take() {
        Some(existing) => format!("{existing}; {message}"),
        None => message,
    });
}

/// Nach einem Absturz dürfen unfertige Nebenfenster-Queues nicht unsichtbar
/// bleiben. Diese erste Phase bereitet ausschließlich Queue und AppState vor;
/// Fenster werden erst nach dem erfolgreich gebauten Hauptfenster erzeugt.
pub(crate) fn prepare_incomplete_queue_windows(
    app: &tauri::AppHandle,
) -> Result<IncompleteQueueRecovery, AppError> {
    let config = app_config_dir(app)?;
    std::fs::create_dir_all(&config).map_err(AppError::from)?;
    let entries = std::fs::read_dir(&config)
        .map_err(AppError::from)?
        .map(|entry| entry.map(|entry| entry.file_name()).map_err(AppError::from));
    plan_recovery_entries(entries, |label| {
        let state = app.state::<AppState>().window(label);
        // Ein einzelnes beschädigtes, ungültiges oder aus einer neueren Version
        // stammendes Nebenfenster-Journal darf den App-Start nicht abbrechen.
        // Sonst verhinderte es auch das Hauptfenster — und damit jede
        // UI-gestützte Recovery. Der Fehler bleibt deshalb auf sein Journal
        // beschränkt und wird für das betroffene Fenster gemeldet.
        let queue = operation_queue(app, &state, label)?;
        Ok(queue.snapshot().entries.iter().any(|entry| {
            !matches!(
                entry.state,
                doppeldecker_core::QueueState::Completed | doppeldecker_core::QueueState::Cancelled
            )
        }))
    })
}

/// Zweite Phase nach dem Hauptfenster: Ein einzelner Darstellungsfehler darf den
/// bereits erfolgreichen Setup-Pfad nicht mehr zurücknehmen. Alle übrigen sicheren
/// Recovery-Fenster werden weiter versucht; Fehler bleiben für `main` im Backend.
pub(crate) fn show_incomplete_queue_windows(app: &tauri::AppHandle, plan: IncompleteQueueRecovery) {
    let state = app.state::<AppState>();
    if !plan.journal_errors.is_empty() {
        append_startup_queue_error(
            &state,
            format!(
                "Diese Fensterjournale ließen sich nicht öffnen und brauchen Recovery: {}",
                plan.journal_errors
                    .iter()
                    .map(|(label, message)| format!("{label}: {message}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        );
    }
    let build_errors =
        show_recovery_windows_using(&plan, |label| build_window(app, label).map(|_| ()));
    if !build_errors.is_empty() {
        append_startup_app_error(
            &state,
            format!(
                "Diese Recovery-Fenster konnten nicht angezeigt werden: {}",
                build_errors.join("; ")
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_start_braucht_profil_und_absoluten_pfad() {
        let missing_profile = PaneStartLocation {
            provider: "sftp".into(),
            profile_id: None,
            profile_revision: None,
            profile_name: None,
            path: "/daten".into(),
        };
        assert!(missing_profile.validate().is_err());

        let missing_revision = PaneStartLocation {
            provider: "sftp".into(),
            profile_id: Some("server".into()),
            profile_revision: None,
            profile_name: Some("Server".into()),
            path: "/daten".into(),
        };
        assert_eq!(missing_revision.validate().unwrap_err().code, "plan_drift");

        let relative = PaneStartLocation {
            provider: "local".into(),
            profile_id: None,
            profile_revision: None,
            profile_name: None,
            path: "daten".into(),
        };
        assert!(relative.validate().is_err());
    }

    #[test]
    fn startzustand_akzeptiert_verschachtelte_frontend_feldnamen() {
        let state: WindowStartState = serde_json::from_value(serde_json::json!({
            "left": {
                "provider": "local",
                "profileId": null,
                "profileName": null,
                "path": "/links"
            },
            "right": {
                "provider": "ftps",
                "profileId": "server",
                "profileRevision": 17,
                "profileName": "Server",
                "path": "/rechts"
            }
        }))
        .unwrap();
        assert_eq!(state.right.profile_id.as_deref(), Some("server"));
        assert_eq!(state.right.profile_revision, Some(17));
        assert_eq!(state.right.profile_name.as_deref(), Some("Server"));

        let state = WindowStartState {
            left: PaneStartLocation {
                provider: "local".into(),
                profile_id: None,
                profile_revision: None,
                profile_name: None,
                path: "/links".into(),
            },
            right: PaneStartLocation {
                provider: "ftps".into(),
                profile_id: Some("server".into()),
                profile_revision: Some(17),
                profile_name: Some("Server".into()),
                path: "/rechts".into(),
            },
        };
        let value = serde_json::to_value(state).unwrap();
        assert_eq!(value["right"]["profileId"], "server");
        assert_eq!(value["right"]["profileRevision"], 17);
        assert_eq!(value["right"]["profileName"], "Server");
    }

    #[test]
    fn gespeicherte_position_gilt_nur_auf_einem_vorhandenen_monitor() {
        let monitors = [(0.0, 0.0, 1920.0, 1080.0), (1920.0, 0.0, 2560.0, 1440.0)];
        // Fenster vollständig auf dem ersten Monitor.
        assert!(geometry_visible_on(
            (100.0, 100.0, 1180.0, 760.0),
            &monitors
        ));
        // Fenster ragt nach rechts über beide Monitore hinaus, Titelleiste sichtbar.
        assert!(geometry_visible_on(
            (4300.0, 200.0, 1180.0, 760.0),
            &monitors
        ));
        // Fenster lag auf einem inzwischen abgesteckten Display links davon.
        assert!(!geometry_visible_on(
            (-3000.0, 100.0, 1180.0, 760.0),
            &monitors
        ));
        // Nur ein schmaler Streifen ragt herein: zu wenig zum Anfassen.
        assert!(!geometry_visible_on(
            (-1100.0, 100.0, 1180.0, 760.0),
            &monitors
        ));
        // Fenster unterhalb aller Monitore.
        assert!(!geometry_visible_on(
            (100.0, 1500.0, 1180.0, 760.0),
            &monitors
        ));
        // Nur der untere Fensterrand ragt von einem abgesteckten oberen Monitor
        // herein; die Titelleiste bleibt unerreichbar.
        assert!(!geometry_visible_on(
            (100.0, -700.0, 1180.0, 760.0),
            &monitors
        ));
        assert!(!geometry_visible_on((100.0, 100.0, 1180.0, 760.0), &[]));
    }

    fn saved_window(path: &str) -> crate::desktop_settings::SavedWindow {
        serde_json::from_value(serde_json::json!({
            "panes": {
                "left": {"provider": "local", "path": path},
                "right": {"provider": "local", "path": path},
            },
            "geometry": null,
            "activeSide": "left",
        }))
        .unwrap()
    }

    #[test]
    fn nebenfenster_mit_offener_queue_wird_nicht_zum_hauptfenster() {
        let mut saved = BTreeMap::new();
        saved.insert("window-a".to_owned(), saved_window("/a"));
        saved.insert("window-b".to_owned(), saved_window("/b"));

        // Ohne offene Queue rückt das kleinste Label nach `main`.
        let promoted = promote_session_main(saved.clone(), &BTreeSet::new());
        assert_eq!(promoted["main"].panes.left.path, "/a");
        assert!(!promoted.contains_key("window-a"));
        assert!(promoted.contains_key("window-b"));

        // Hat `window-a` noch unfertige Einträge, behält es sein Label samt
        // Journal, und `window-b` wird zum Hauptfenster.
        let promoted =
            promote_session_main(saved.clone(), &BTreeSet::from(["window-a".to_owned()]));
        assert_eq!(promoted["main"].panes.left.path, "/b");
        assert!(promoted.contains_key("window-a"));

        // Sind alle Nebenfenster in Recovery, startet `main` mit Standardpanes.
        let promoted = promote_session_main(
            saved.clone(),
            &BTreeSet::from(["window-a".to_owned(), "window-b".to_owned()]),
        );
        assert!(!promoted.contains_key("main"));
        assert_eq!(promoted.len(), 2);

        // Ein vorhandenes `main` bleibt unangetastet.
        saved.insert("main".to_owned(), saved_window("/main"));
        let promoted = promote_session_main(saved, &BTreeSet::new());
        assert_eq!(promoted["main"].panes.left.path, "/main");
        assert_eq!(promoted.len(), 3);
    }

    #[test]
    fn startup_queuefehler_gehoert_einmalig_nur_dem_hauptfenster() {
        let state = AppState::default();
        *state.startup_queue_error.lock_recover() = Some("window-defekt".into());

        assert_eq!(
            take_startup_queue_error_for_label(&state, "window-1"),
            None,
            "Ein Nebenfenster darf den Befund nicht fehladressiert verbrauchen"
        );
        assert_eq!(
            take_startup_queue_error_for_label(&state, "main").as_deref(),
            Some("window-defekt")
        );
        assert_eq!(take_startup_queue_error_for_label(&state, "main"), None);
    }

    #[test]
    fn journal_und_fensterbaufehler_bleiben_getrennt_einmalig_fuer_main() {
        let state = AppState::default();
        append_startup_queue_error(&state, "window-journal: beschädigt".into());
        append_startup_app_error(&state, "window-bau: Darstellung fehlgeschlagen".into());

        assert_eq!(take_startup_queue_error_for_label(&state, "window-1"), None);
        assert_eq!(take_startup_app_error_for_label(&state, "window-1"), None);
        let queue_message = take_startup_queue_error_for_label(&state, "main").unwrap();
        let app_message = take_startup_app_error_for_label(&state, "main").unwrap();
        assert!(queue_message.contains("window-journal: beschädigt"));
        assert!(!queue_message.contains("window-bau"));
        assert!(app_message.contains("window-bau: Darstellung fehlgeschlagen"));
        assert!(!app_message.contains("window-journal"));
        assert_eq!(take_startup_queue_error_for_label(&state, "main"), None);
        assert_eq!(take_startup_app_error_for_label(&state, "main"), None);
    }

    #[test]
    fn recovery_planung_baut_vor_spaetem_verzeichnisfehler_keine_fenster() {
        let inspected = std::cell::RefCell::new(Vec::new());
        let result = plan_recovery_entries(
            [
                Ok(std::ffi::OsString::from(
                    "operations-queue-window-erster.json",
                )),
                Err(AppError::new("io", "später read_dir-Fehler")),
            ],
            |label| {
                inspected.borrow_mut().push(label.to_owned());
                Ok(true)
            },
        );

        assert_eq!(*inspected.borrow(), ["window-erster"]);
        assert_eq!(result.unwrap_err().message, "später read_dir-Fehler");
        let source = include_str!("window_commands.rs");
        let planning = &source[source.find("fn plan_recovery_entries").unwrap()
            ..source.find("fn show_recovery_windows_using").unwrap()];
        assert!(!planning.contains("build_window"));
    }

    #[test]
    fn recovery_darstellung_adressiert_spaeten_baufehler_und_faehrt_fort() {
        let plan = IncompleteQueueRecovery {
            labels: vec!["window-a".into(), "window-b".into(), "window-c".into()],
            journal_errors: vec![("window-kaputt".into(), "Journal beschädigt".into())],
        };
        assert_eq!(
            plan.unavailable_labels(),
            BTreeSet::from([
                "window-a".into(),
                "window-b".into(),
                "window-c".into(),
                "window-kaputt".into(),
            ])
        );
        let attempted = std::cell::RefCell::new(Vec::new());
        let errors = show_recovery_windows_using(&plan, |label| {
            attempted.borrow_mut().push(label.to_owned());
            if label == "window-b" {
                Err(AppError::new("window", "Mock-Fensterbau scheitert"))
            } else {
                Ok(())
            }
        });

        assert_eq!(*attempted.borrow(), ["window-a", "window-b", "window-c"]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("window-b"));
        assert!(errors[0].contains("Mock-Fensterbau scheitert"));
    }
}
