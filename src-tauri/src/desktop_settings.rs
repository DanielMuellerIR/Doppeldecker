//! Prozessweite Einstellungen und reine Navigationssitzung. Hier landen weder
//! Kennwörter noch Queue-Einträge, Provider-Handles oder Realmodus-Permits.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use tauri::{Manager, WebviewWindow};

use crate::window_commands::WindowStartState;
use crate::{app_config_dir, AppError, MutexExt};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IconChoice {
    #[default]
    Blue,
    Sunset,
    Aurora,
    Turquoise,
    Rose,
    Storm,
}

impl IconChoice {
    pub(crate) fn bytes(self) -> &'static [u8] {
        match self {
            Self::Blue => include_bytes!("../icons/variants/blue.png"),
            Self::Sunset => include_bytes!("../icons/variants/sunset.png"),
            Self::Aurora => include_bytes!("../icons/variants/aurora.png"),
            Self::Turquoise => include_bytes!("../icons/variants/turquoise.png"),
            Self::Rose => include_bytes!("../icons/variants/rose.png"),
            Self::Storm => include_bytes!("../icons/variants/storm.png"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Theme {
    #[default]
    Auto,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ByteUnits {
    Decimal,
    Binary,
}

impl Default for ByteUnits {
    fn default() -> Self {
        if cfg!(target_os = "macos") {
            Self::Decimal
        } else {
            Self::Binary
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffTool {
    #[default]
    Xcode,
    Fastra,
    Bbedit,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Preferences {
    pub(crate) icon: IconChoice,
    pub(crate) theme: Theme,
    pub(crate) byte_units: ByteUnits,
    pub(crate) local_folder_sizes: bool,
    pub(crate) network_discovery: bool,
    pub(crate) restore_session: bool,
    pub(crate) diff_tool: DiffTool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            icon: IconChoice::Blue,
            theme: Theme::Auto,
            byte_units: ByteUnits::default(),
            local_folder_sizes: false,
            network_discovery: true,
            restore_session: false,
            diff_tool: DiffTool::Xcode,
        }
    }
}

/// Nur ausdrücklich geänderte Felder ersetzen. Ein alter Dialog in Fenster A
/// darf die inzwischen in Fenster B getroffene Icon-Wahl nicht überschreiben.
#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PreferencePatch {
    icon: Option<IconChoice>,
    theme: Option<Theme>,
    byte_units: Option<ByteUnits>,
    local_folder_sizes: Option<bool>,
    network_discovery: Option<bool>,
    restore_session: Option<bool>,
    diff_tool: Option<DiffTool>,
}

impl Preferences {
    fn apply(&mut self, patch: PreferencePatch) {
        if let Some(v) = patch.icon {
            self.icon = v;
        }
        if let Some(v) = patch.theme {
            self.theme = v;
        }
        if let Some(v) = patch.byte_units {
            self.byte_units = v;
        }
        if let Some(v) = patch.local_folder_sizes {
            self.local_folder_sizes = v;
        }
        if let Some(v) = patch.network_discovery {
            self.network_discovery = v;
        }
        if let Some(v) = patch.restore_session {
            self.restore_session = v;
        }
        if let Some(v) = patch.diff_tool {
            self.diff_tool = v;
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WindowGeometry {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
    pub(crate) maximized: bool,
}

impl WindowGeometry {
    fn valid(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
            && self.x.abs() <= 100_000.0
            && self.y.abs() <= 100_000.0
            && (760.0..=20_000.0).contains(&self.width)
            && (480.0..=20_000.0).contains(&self.height)
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SavedWindow {
    pub(crate) panes: WindowStartState,
    #[serde(default)]
    pub(crate) tabs: Option<crate::pane_tabs::WindowTabs>,
    pub(crate) geometry: Option<WindowGeometry>,
    #[serde(default = "left_side")]
    pub(crate) active_side: String,
}

fn left_side() -> String {
    "left".into()
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DesktopDocument {
    version: u32,
    revision: u64,
    #[serde(default)]
    initialized: bool,
    preferences: Preferences,
    windows: BTreeMap<String, SavedWindow>,
}

impl Default for DesktopDocument {
    fn default() -> Self {
        Self {
            version: 2,
            revision: 0,
            initialized: false,
            preferences: Preferences::default(),
            windows: BTreeMap::new(),
        }
    }
}

impl DesktopDocument {
    fn validate(&self) -> Result<(), AppError> {
        if self.version != 2 || self.windows.len() > 64 {
            return Err(AppError::new(
                "invalid_input",
                "Unbekanntes oder zu großes Sitzungsformat",
            ));
        }
        for (label, window) in &self.windows {
            crate::queue_runtime::queue_journal_name(label)?;
            window.panes.validate()?;
            if let Some(tabs) = &window.tabs {
                tabs.validate(&window.panes)?;
            }
            if !matches!(window.active_side.as_str(), "left" | "right")
                || window.geometry.as_ref().is_some_and(|g| !g.valid())
            {
                return Err(AppError::new(
                    "invalid_input",
                    "Ungültige Fensterdarstellung in der Sitzung",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct DesktopState {
    inner: Mutex<DesktopStore>,
    // Native Icon-Tasks dürfen nie hinter einem Store-fsync auf dem UI-Thread warten.
    current_icon: Mutex<IconChoice>,
    // Vor dem ersten Fensterbau aus der validierten Sitzung gebunden. Der native
    // Geometriepfad darf nie hinter einem Store-fsync oder Settings-Rename warten.
    startup_geometry: Mutex<BTreeMap<String, WindowGeometry>>,
    remember_sequence: AtomicU64,
    latest_remember: Mutex<BTreeMap<String, u64>>,
    close_writes: Mutex<CloseWrites>,
}

impl DesktopState {
    fn seed_live_window(&self, label: &str, saved: SavedWindow) {
        let geometry = saved.geometry.clone();
        self.inner.lock_recover().live.insert(label.into(), saved);
        let mut startup = self.startup_geometry.lock_recover();
        if let Some(geometry) = geometry {
            startup.insert(label.into(), geometry);
        } else {
            startup.remove(label);
        }
    }

    fn take_startup_geometry(&self, label: &str) -> Option<WindowGeometry> {
        self.startup_geometry.lock_recover().remove(label)
    }
}

#[derive(Default)]
struct CloseWrites {
    pending: usize,
    deferred_exit: Option<i32>,
}

impl CloseWrites {
    fn defer_exit(&mut self, code: Option<i32>) -> bool {
        if self.pending == 0 {
            return false;
        }
        self.deferred_exit = Some(code.unwrap_or(0));
        true
    }

    fn finish(&mut self) -> Option<i32> {
        self.pending -= 1;
        if self.pending == 0 {
            self.deferred_exit.take()
        } else {
            None
        }
    }
}

#[derive(Default)]
struct DesktopStore {
    path: Option<PathBuf>,
    document: DesktopDocument,
    // Alle aktuellen Orte bleiben im RAM verfügbar, auch wenn das Wiederherstellen
    // aus ist. Einschalten erfasst damit sofort sämtliche lebenden Fenster.
    live: BTreeMap<String, SavedWindow>,
    // Ein bereits geschlossenes Fenster darf nicht durch einen verzögerten
    // Resize-/Navigations-Callback wieder in die nächste Sitzung geraten.
    closed: BTreeSet<String>,
    load_error: Option<AppError>,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreferencesSnapshot {
    preferences: Preferences,
    revision: u64,
    needs_migration: bool,
    load_error: Option<AppError>,
}

/// Gemeinsame Grenze für Lesen und Schreiben einschließlich der Tab-Navigation.
const MAX_DOCUMENT_BYTES: u64 = 2 * 1024 * 1024;

fn read_document(path: &Path) -> Result<DesktopDocument, AppError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(
            libc::O_NOFOLLOW
                // Ohne `O_NONBLOCK` hängt schon das Öffnen, wenn unter diesem Namen
                // eine FIFO oder ein Gerät ohne Gegenstelle liegt. `initialize` läuft
                // im ersten Startschritt, also vor dem ersten Fenster: Die App bliebe
                // ohne Fenster, ohne Meldung und ohne Frist stehen. Die Prüfung „echte
                // reguläre Datei" direkt danach lehnt so etwas ohnehin ab; sie kommt
                // nur nicht mehr zum Zug, wenn `open` selbst blockiert. Der
                // Profil-Loader in `core/src/connections.rs` löst es genauso.
                | libc::O_NONBLOCK,
        )
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DesktopDocument::default()),
        Err(e) => return Err(e.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(AppError::new(
            "invalid_input",
            "Einstellungsdatei ist keine reguläre Datei",
        ));
    }
    // Die Größe wird beim Lesen selbst begrenzt statt nur vorab geprüft: Eine
    // Datei, die zwischen `metadata` und `read_to_end` wächst, könnte sonst
    // beliebig viel Speicher belegen. Ein Byte über dem Limit reicht als Beweis.
    let mut bytes = Vec::new();
    file.take(MAX_DOCUMENT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(AppError::new(
            "invalid_input",
            "Einstellungsdatei überschreitet die zulässige Größe",
        ));
    }
    let mut document: DesktopDocument = serde_json::from_slice(&bytes).map_err(|e| {
        AppError::new(
            "invalid_input",
            format!("Einstellungen konnten nicht gelesen werden: {e}"),
        )
    })?;
    if document.version == 1 {
        if document
            .windows
            .values()
            .any(|window| window.tabs.is_some())
        {
            return Err(AppError::new(
                "invalid_input",
                "Format 1 darf keinen neuen Tab-Stand enthalten",
            ));
        }
        document.version = 2;
    }
    for window in document.windows.values_mut() {
        if window.tabs.is_none() {
            window.tabs = Some(crate::pane_tabs::WindowTabs::from_active(&window.panes));
        }
    }
    document.validate()?;
    Ok(document)
}

/// Ein exklusiver Tempname und Rename halten den alten Stand bei einem
/// Schreibfehler lesbar. Der Prozesslock der Queue ist vor dem Laden bereits
/// gehalten; die zusätzliche Mutex ordnet Änderungen aller Fenster.
///
/// Rückgabe `Ok(Some(fehler))` bedeutet: Die neue Datei liegt bereits an ihrem
/// Platz, nur der abschließende Verzeichnis-fsync ist gescheitert. Der Aufrufer
/// muss den neuen Stand dann trotzdem als gültig übernehmen — sonst zeigt der RAM
/// den alten Wert, während die Datei (und damit der nächste Start) den neuen hat.
fn write_document(path: &Path, document: &DesktopDocument) -> Result<Option<AppError>, AppError> {
    document.validate()?;
    let bytes =
        serde_json::to_vec_pretty(document).map_err(|e| AppError::new("io", e.to_string()))?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(AppError::new(
            "invalid_input",
            "Sitzungsstand überschreitet die zulässige Größe",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| AppError::new("io", "Einstellungsordner fehlt"))?;
    std::fs::create_dir_all(parent)?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| AppError::new("io", e.to_string()))?;
    let id = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let temp = parent.join(format!(".desktop-settings-{id}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    let result = (|| -> Result<(), AppError> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    // Ab hier ist der Rename vollzogen; der Verzeichnis-fsync sichert ihn nur noch
    // gegen einen Stromausfall ab und darf den Erfolg nicht mehr zurücknehmen.
    let directory_sync = File::open(parent).and_then(|dir| dir.sync_all());
    Ok(directory_sync.err().map(AppError::from))
}

impl DesktopStore {
    /// Schreibt den neuen Stand und übernimmt ihn.
    ///
    /// `Ok(Some(fehler))` heißt: Die Datei liegt gültig an ihrem Platz, nur der
    /// abschließende Verzeichnis-fsync ist gescheitert. Das ist eine Meldung, kein
    /// Fehlschlag — der Aufrufer muss den Stand trotzdem als gültig behandeln, sonst
    /// zeigen alle Fenster den alten Wert, während Datei und nächster Start den neuen
    /// tragen. Nur `Err` bedeutet, dass nichts übernommen wurde.
    fn commit(&mut self, next: DesktopDocument) -> Result<Option<AppError>, AppError> {
        if let Some(error) = &self.load_error {
            return Err(error.clone());
        }
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| AppError::new("io", "Einstellungen noch nicht geladen"))?;
        let directory_sync_error = write_document(path, &next)?;
        self.document = next;
        Ok(directory_sync_error)
    }
    fn snapshot(&self) -> PreferencesSnapshot {
        PreferencesSnapshot {
            preferences: self.document.preferences.clone(),
            revision: self.document.revision,
            needs_migration: !self.document.initialized,
            load_error: self.load_error.clone(),
        }
    }
}

pub(crate) fn initialize(app: &tauri::AppHandle) -> Result<(), AppError> {
    let path = app_config_dir(app)?.join("desktop-settings.json");
    let state = app.state::<DesktopState>();
    let mut store = state.inner.lock_recover();
    store.path = Some(path.clone());
    match read_document(&path) {
        Ok(document) => store.document = document,
        Err(error) => store.load_error = Some(error),
    }
    *state.current_icon.lock_recover() = store.document.preferences.icon;
    Ok(())
}

#[tauri::command]
pub(crate) async fn desktop_preferences(
    app: tauri::AppHandle,
) -> Result<PreferencesSnapshot, AppError> {
    // Auch reine RAM-Abfragen können hinter einem laufenden fsync warten.
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<DesktopState>().inner.lock_recover().snapshot()
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))
}

#[tauri::command]
pub(crate) async fn update_desktop_preferences(
    app: tauri::AppHandle,
    window: WebviewWindow,
    patch: PreferencePatch,
    only_if_uninitialized: Option<bool>,
) -> Result<PreferencesSnapshot, AppError> {
    let update_icon = patch.icon.is_some();
    let write_app = app.clone();
    let (snapshot, directory_sync_error, changed) =
        tauri::async_runtime::spawn_blocking(move || {
            let state = write_app.state::<DesktopState>();
            update_preferences(&state, patch, only_if_uninitialized.unwrap_or(false))
        })
        .await
        .map_err(|error| AppError::new("app", error.to_string()))??;
    if !changed {
        return Ok(snapshot);
    }
    // Der fehlende Verzeichnis-fsync macht den Stand nicht ungültig: Datei und RAM
    // tragen den neuen Wert. Er wird deshalb gemeldet, ohne den Command scheitern zu
    // lassen — sonst zeigte das Frontend „fehlgeschlagen“ und spränge auf die alten
    // Werte zurück, während der nächste Start längst die neuen hat.
    if let Some(error) = directory_sync_error {
        let _ = crate::emit_to_window(&window, "settings-error", error);
    }
    // Ab hier ist die Datei geschrieben. Ein Icon- oder Zustellfehler darf den
    // Command nicht mehr scheitern lassen: Das Frontend würde sonst „fehlgeschlagen“
    // anzeigen und auf den alten Stand zurückspringen, obwohl die Datei — und
    // damit jeder weitere Start — den neuen Wert längst hat.
    if update_icon {
        if let Err(error) = apply_icon(&app) {
            let _ = crate::emit_to_window(&window, "settings-error", error);
        }
    }
    // Einstellungen sind absichtlich prozessweit. Die gezielte Schleife erhält
    // den Vertrag, dass Datei-/Queue-Ereignisse niemals als Rundruf laufen. Ein
    // gerade schließendes Fenster darf die übrigen nicht um den Rundruf bringen.
    for target in app.webview_windows().values() {
        let _ = crate::emit_to_window(target, "desktop-preferences-changed", &snapshot);
    }
    Ok(snapshot)
}

fn update_preferences(
    state: &DesktopState,
    patch: PreferencePatch,
    only_if_uninitialized: bool,
) -> Result<(PreferencesSnapshot, Option<AppError>, bool), AppError> {
    let mut store = state.inner.lock_recover();
    // Alle Fenster prüfen und committen unter derselben Sperre. Eine verspätete
    // Altwertmigration darf weder eine frühere Migration noch eine inzwischen
    // bestätigte Nutzeränderung überschreiben oder eine neue Revision erzeugen.
    if only_if_uninitialized && store.document.initialized {
        return Ok((store.snapshot(), None, false));
    }
    let mut next = store.document.clone();
    next.preferences.apply(patch);
    next.initialized = true;
    next.revision += 1;
    next.windows = if next.preferences.restore_session {
        store.live.clone()
    } else {
        BTreeMap::new()
    };
    let directory_sync_error = store.commit(next)?;
    *state.current_icon.lock_recover() = store.document.preferences.icon;
    Ok((store.snapshot(), directory_sync_error, true))
}

pub(crate) fn preferences(app: &tauri::AppHandle) -> Preferences {
    app.state::<DesktopState>()
        .inner
        .lock_recover()
        .document
        .preferences
        .clone()
}

pub(crate) fn restore_windows(app: &tauri::AppHandle) -> BTreeMap<String, SavedWindow> {
    let state = app.state::<DesktopState>();
    let store = state.inner.lock_recover();
    if store.document.preferences.restore_session {
        store.document.windows.clone()
    } else {
        BTreeMap::new()
    }
}

pub(crate) fn take_restored_geometry(
    app: &tauri::AppHandle,
    label: &str,
) -> Option<WindowGeometry> {
    app.state::<DesktopState>().take_startup_geometry(label)
}

#[tauri::command]
pub(crate) async fn restored_active_side(
    app: tauri::AppHandle,
    window: WebviewWindow,
) -> Result<String, AppError> {
    let label = window.label().to_owned();
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<DesktopState>()
            .inner
            .lock_recover()
            .live
            .get(&label)
            .map(|saved| saved.active_side.clone())
            .unwrap_or_else(left_side)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))
}

#[tauri::command]
pub(crate) async fn restored_pane_tabs(
    app: tauri::AppHandle,
    window: WebviewWindow,
) -> Result<Option<crate::pane_tabs::WindowTabs>, AppError> {
    let label = window.label().to_owned();
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<DesktopState>()
            .inner
            .lock_recover()
            .live
            .get(&label)
            .and_then(|saved| saved.tabs.clone())
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))
}

/// Fenstermaße werden sofort gelesen, das Schreiben (zwei fsyncs plus Rename)
/// läuft aber auf einem Hintergrund-Thread: Der Command feuert 250 ms nach dem
/// letzten Move-/Resize-Ereignis und würde sonst dann den Fenster-Thread anhalten.
/// `State` lässt sich nicht in die Closure verschieben, deshalb kommt der
/// `AppHandle` mit und holt den Store erst dort.
#[tauri::command]
pub(crate) async fn remember_window(
    app: tauri::AppHandle,
    window: WebviewWindow,
    panes: WindowStartState,
    active_side: String,
    tabs: Option<crate::pane_tabs::WindowTabs>,
) -> Result<(), AppError> {
    let sequence = app
        .state::<DesktopState>()
        .remember_sequence
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    panes.validate()?;
    if let Some(tabs) = &tabs {
        tabs.validate(&panes)?;
    }
    if !matches!(active_side.as_str(), "left" | "right") {
        return Err(AppError::new("invalid_input", "Unbekannte aktive Pane"));
    }
    let scale = window
        .scale_factor()
        .map_err(|e| AppError::new("window", e.to_string()))?;
    let size = window
        .inner_size()
        .map_err(|e| AppError::new("window", e.to_string()))?
        .to_logical::<f64>(scale);
    let position = window
        .outer_position()
        .map_err(|e| AppError::new("window", e.to_string()))?
        .to_logical::<f64>(scale);
    let geometry = WindowGeometry {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
        maximized: window.is_maximized().unwrap_or(false),
    };
    let saved = SavedWindow {
        panes,
        tabs,
        geometry: geometry.valid().then_some(geometry),
        active_side,
    };
    let label = window.label().to_owned();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<DesktopState>();
        remember_ordered(&state, &label, sequence, saved)
    })
    .await
    .map_err(|e| AppError::new("app", e.to_string()))?
}

fn remember_ordered(
    state: &DesktopState,
    label: &str,
    sequence: u64,
    saved: SavedWindow,
) -> Result<(), AppError> {
    // Die Sequenzsperre bleibt bis nach dem Store-Commit gehalten. Dadurch gibt es
    // nur zwei mögliche Reihenfolgen: Alt schreibt vor Neu, oder Alt sieht die
    // bereits übernommene neuere Sequenz und fällt aus. Ein alter Blocking-Worker
    // kann den neueren Stand nie mehr nachträglich überschreiben.
    let mut latest = state.latest_remember.lock_recover();
    if latest.get(label).is_some_and(|current| *current > sequence) {
        return Ok(());
    }
    let mut store = state.inner.lock_recover();
    latest.insert(label.to_owned(), sequence);
    remember(&mut store, label, saved)
}

fn remember(store: &mut DesktopStore, label: &str, mut saved: SavedWindow) -> Result<(), AppError> {
    if saved.tabs.is_none() {
        saved.tabs = Some(crate::pane_tabs::WindowTabs::from_active(&saved.panes));
    }
    saved.panes.validate()?;
    saved
        .tabs
        .as_ref()
        .expect("Tab-Stand ergänzt")
        .validate(&saved.panes)?;
    if store.closed.contains(label) {
        return Ok(());
    }
    if store.live.get(label) == Some(&saved) {
        return Ok(());
    }
    if store.document.preferences.restore_session {
        let mut next = store.document.clone();
        next.windows = store.live.clone();
        next.windows.insert(label.to_owned(), saved.clone());
        // Ein gescheiterter Verzeichnis-fsync bleibt hier folgenlos: Der Sitzungsstand
        // ist geschrieben. Der Frontend-Debounce ruft diesen Weg einmal 250 ms nach
        // dem Ende einer Folge von Fensterbewegungen auf.
        store.commit(next)?;
    }
    store.live.insert(label.to_owned(), saved);
    Ok(())
}

/// Beim appweiten Beenden bleibt der fortlaufend geschriebene vollständige Stand
/// erhalten. Bewusst einzeln geschlossene Fenster verschwinden aus der nächsten
/// Sitzung; `preserve_for_exit` unterscheidet diesen Fall plattformspezifisch.
pub(crate) fn schedule_forget_closed_window(
    app: &tauri::AppHandle,
    label: &str,
    preserve_for_exit: bool,
) {
    // Appweiter Exit und Linux letzter Close behalten die vollständige Sitzung.
    // Nach der endgültigen Exitfreigabe dürfen keine neuen Schreibaufträge entstehen.
    if preserve_for_exit {
        return;
    }
    let state = app.state::<DesktopState>();
    // Der Aufrufer hält exit_barrier. Registrierung und Exitprüfung sehen dadurch
    // denselben Stand, ohne auf den Store-Mutex oder einen fsync warten zu müssen.
    state.close_writes.lock_recover().pending += 1;
    let handle = app.clone();
    let label = label.to_owned();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<DesktopState>();
        let result = forget_closed(&mut state.inner.lock_recover(), &label);
        if let Err(error) = result {
            let _ =
                crate::app_support::record_app_error(&handle, "Sitzungsstand", Some(&error.code));
            if let Some(window) = handle.webview_windows().into_values().next() {
                let _ = crate::emit_to_window(&window, "settings-error", error);
            } else {
                crate::window_commands::append_startup_app_error(
                    &handle.state::<crate::AppState>(),
                    error.message,
                );
            }
        }
        let exit = {
            let app_state = handle.state::<crate::AppState>();
            let _barrier = app_state.exit_barrier.lock_recover();
            state.close_writes.lock_recover().finish()
        };
        // Erneut durch das unveränderte native Exitgate gehen; nie unter der Barriere.
        if let Some(code) = exit {
            handle.exit(code);
        }
    });
}

pub(crate) fn defer_exit_for_session_writes(app: &tauri::AppHandle, code: Option<i32>) -> bool {
    app.state::<DesktopState>()
        .close_writes
        .lock_recover()
        .defer_exit(code)
}

fn forget_closed(store: &mut DesktopStore, label: &str) -> Result<(), AppError> {
    store.closed.insert(label.to_owned());
    store.live.remove(label);
    if store.document.preferences.restore_session {
        let mut next = store.document.clone();
        next.windows = store.live.clone();
        store.commit(next)?;
    }
    Ok(())
}

pub(crate) fn seed_live_window(app: &tauri::AppHandle, label: &str, saved: SavedWindow) {
    app.state::<DesktopState>().seed_live_window(label, saved);
}

/// Setzt das gewählte Icon: unter macOS dauerhaft am Bundle und im Dock, auf Linux das
/// Icon jedes offenen Fensters. Fenster-Icons gibt es auf macOS nicht — `tao`
/// implementiert `set_window_icon` dort bewusst als No-op —, deshalb entfällt dort
/// auch das Dekodieren des PNG je Fenster.
pub(crate) fn apply_icon(app: &tauri::AppHandle) -> Result<(), AppError> {
    #[cfg(not(target_os = "macos"))]
    {
        let choice = *app.state::<DesktopState>().current_icon.lock_recover();
        let image = linux_window_icon(choice)?;
        for window in app.webview_windows().values() {
            window
                .set_icon(image.clone())
                .map_err(|e| AppError::new("window", e.to_string()))?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        let handle = app.clone();
        app.run_on_main_thread(move || {
            apply_current_icon(&handle.state::<DesktopState>(), |choice| {
                use objc2::{AllocAnyThread, MainThreadMarker};
                use objc2_app_kit::{NSApplication, NSImage};
                use objc2_foundation::{NSBundle, NSData};
                // Die Closure läuft asynchron auf dem Hauptthread und kann kein
                // Ergebnis an den Command zurückgeben. Ein Fehlschlag bleibt deshalb
                // wenigstens im geheimnisfreien Fehlerlog nachvollziehbar.
                let Some(mtm) = MainThreadMarker::new() else {
                    let _ = crate::app_support::record_app_error(
                        &handle,
                        "Fenstericon",
                        Some("main_thread"),
                    );
                    return;
                };
                let data = NSData::with_bytes(choice.bytes());
                let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
                    let _ =
                        crate::app_support::record_app_error(&handle, "Fenstericon", Some("image"));
                    return;
                };
                // Nach Prozessende liest das Dock wieder das Findericon des Bundles.
                // NSWorkspace setzt dafür Custom-Icon-Metadaten, ohne signierte
                // Ressourcen in Contents zu ersetzen. Nackte Entwicklungsbinaries
                // besitzen kein App-Bundle und dürfen ihren Ordner nicht verändern.
                let bundle_path = NSBundle::mainBundle().bundlePath();
                if std::path::Path::new(&bundle_path.to_string())
                    .extension()
                    .is_some_and(|extension| extension == "app")
                {
                    if let Err(error) = set_bundle_icon(&bundle_path, &image) {
                        let _ = crate::app_support::record_app_error(
                            &handle,
                            "Fenstericon",
                            Some(&error.code),
                        );
                    }
                }
                unsafe {
                    NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image));
                }
            });
        })
        .map_err(|e| AppError::new("window", e.to_string()))?;
    }
    Ok(())
}

/// Erst im seriellen Hauptthread-Task lesen: Ein älterer Command kann seinen Task
/// erst nach einem neueren zustellen. Ein Revisionsvergleich vor dem Zustellen
/// schützt deshalb weder das Laufzeiticon noch das dauerhaft gesetzte Bundleicon.
#[cfg(any(target_os = "macos", test))]
fn apply_current_icon(state: &DesktopState, apply: impl FnOnce(IconChoice)) {
    let choice = *state.current_icon.lock_recover();
    apply(choice);
}

#[cfg(target_os = "macos")]
fn set_bundle_icon(
    bundle_path: &objc2_foundation::NSString,
    image: &objc2_app_kit::NSImage,
) -> Result<(), AppError> {
    use objc2_app_kit::{NSWorkspace, NSWorkspaceIconCreationOptions};
    if NSWorkspace::sharedWorkspace().setIcon_forFile_options(
        Some(image),
        bundle_path,
        NSWorkspaceIconCreationOptions::empty(),
    ) {
        Ok(())
    } else {
        Err(AppError::new(
            "image",
            "Das dauerhafte App-Icon konnte nicht gesetzt werden. Ist die App schreibgeschützt?",
        ))
    }
}

/// Fenster-Icon für ein neu gebautes Fenster. Auf macOS ein No-op (siehe
/// [`apply_icon`]); die Signatur bleibt plattformgleich, damit der Fensterbau
/// keinen eigenen Plattformzweig braucht.
pub(crate) fn apply_window_icon(window: &WebviewWindow) -> Result<(), AppError> {
    #[cfg(target_os = "macos")]
    {
        let _ = window;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let choice = *window
            .app_handle()
            .state::<DesktopState>()
            .current_icon
            .lock_recover();
        let image = linux_window_icon(choice)?;
        window
            .set_icon(image)
            .map_err(|e| AppError::new("window", e.to_string()))
    }
}

#[cfg(target_os = "linux")]
fn linux_window_icon(choice: IconChoice) -> Result<tauri::image::Image<'static>, AppError> {
    use gtk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf};

    let image = tauri::image::Image::from_bytes(choice.bytes())
        .map_err(|error| AppError::new("image", error.to_string()))?;
    // GTK begrenzt _NET_WM_ICON auf höchstens 262144 CARDINALs einschließlich
    // Breite und Höhe. Schon 512² + 2 wird still verworfen; das Dock bleibt leer.
    let size = 256;
    let pixbuf = Pixbuf::from_bytes(
        &gtk::glib::Bytes::from(image.rgba()),
        Colorspace::Rgb,
        true,
        8,
        image.width() as i32,
        image.height() as i32,
        image.width() as i32 * 4,
    )
    .scale_simple(size, size, InterpType::Bilinear)
    .ok_or_else(|| AppError::new("image", "Das Fenstericon konnte nicht skaliert werden"))?;
    Ok(tauri::image::Image::new_owned(
        pixbuf.read_pixel_bytes().as_ref().to_vec(),
        size as u32,
        size as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn alle_fenstericons_bleiben_unter_der_gtk_x11_propertygrenze() {
        for choice in [
            IconChoice::Blue,
            IconChoice::Sunset,
            IconChoice::Aurora,
            IconChoice::Turquoise,
            IconChoice::Rose,
            IconChoice::Storm,
        ] {
            let image = linux_window_icon(choice).unwrap();
            let pixels = image.width() as usize * image.height() as usize;
            assert!(pixels + 2 < 262_144, "{choice:?}");
            assert_eq!(image.rgba().len(), pixels * 4, "{choice:?}");
            assert!(image.rgba().chunks_exact(4).any(|pixel| pixel[3] != 0));
        }
    }

    #[test]
    fn exit_wartet_auf_alle_fensterschreibauftraege_und_wird_einmal_freigegeben() {
        let mut writes = CloseWrites {
            pending: 2,
            deferred_exit: None,
        };
        assert!(writes.defer_exit(Some(7)));
        assert_eq!(writes.finish(), None);
        assert_eq!(writes.finish(), Some(7));
        assert!(!writes.defer_exit(None));
        assert_eq!(writes.deferred_exit, None);
    }

    #[test]
    fn geschlossener_fensterstand_wird_durch_spaetes_remember_nicht_wiederbelebt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut store = DesktopStore {
            path: Some(path.clone()),
            ..Default::default()
        };
        store.document.preferences.restore_session = true;
        let saved = SavedWindow {
            panes: pane_state(),
            tabs: None,
            geometry: None,
            active_side: "left".into(),
        };
        remember(&mut store, "main", saved.clone()).unwrap();
        remember(&mut store, "window-2", saved.clone()).unwrap();
        forget_closed(&mut store, "window-2").unwrap();
        remember(&mut store, "window-2", saved).unwrap();
        assert!(!store.live.contains_key("window-2"));
        let document = read_document(&path).unwrap();
        assert_eq!(document.windows.len(), 1);
        assert!(document.windows.contains_key("main"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bundle_icon_bleibt_als_finder_metadaten_erhalten() {
        use objc2::AllocAnyThread;
        use objc2_app_kit::NSImage;
        use objc2_foundation::{NSData, NSString};
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("Iconprobe.app");
        let contents = bundle.join("Contents");
        std::fs::create_dir_all(&contents).unwrap();
        let resource = contents.join("unveraendert");
        std::fs::write(&resource, b"signierte Ressource").unwrap();
        let bundle_path = NSString::from_str(bundle.to_str().unwrap());
        let mut previous = Vec::new();
        for choice in [IconChoice::Storm, IconChoice::Blue] {
            let image =
                NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(choice.bytes()))
                    .unwrap();
            set_bundle_icon(&bundle_path, &image).unwrap();
            drop(image);
            // Ein separater Prozess liest die dauerhaften Finder-Metadaten,
            // nicht das NSApplication-Icon des aktuellen Prozesses.
            let result = Command::new("/usr/bin/xattr")
                .args(["-px", "com.apple.ResourceFork"])
                .arg(bundle.join("Icon\r"))
                .output()
                .unwrap();
            assert!(result.status.success());
            assert!(!result.stdout.is_empty());
            assert_ne!(previous, result.stdout);
            previous = result.stdout;
            assert_eq!(std::fs::read(&resource).unwrap(), b"signierte Ressource");
        }
        let image = NSImage::initWithData(
            NSImage::alloc(),
            &NSData::with_bytes(IconChoice::Blue.bytes()),
        )
        .unwrap();
        let missing = NSString::from_str(dir.path().join("fehlt/App.app").to_str().unwrap());
        assert!(set_bundle_icon(&missing, &image).is_err());
    }

    fn pane_state() -> WindowStartState {
        serde_json::from_value(
            serde_json::json!({"left":{"provider":"local","path":"/links"},
            "right":{"provider":"ftps","profileId":"server","profileRevision":7,"path":"/rechts"}}),
        )
        .unwrap()
    }

    #[test]
    fn identischer_fensterstand_wird_nach_schreibfehler_erneut_gespeichert() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("config");
        std::fs::write(&parent, b"blockiert Elternordner").unwrap();
        let path = parent.join("settings.json");
        let mut store = DesktopStore {
            path: Some(path.clone()),
            ..Default::default()
        };
        store.document.preferences.restore_session = true;
        let saved = SavedWindow {
            panes: pane_state(),
            tabs: None,
            geometry: None,
            active_side: "left".into(),
        };
        assert!(remember(&mut store, "main", saved.clone()).is_err());
        std::fs::remove_file(&parent).unwrap();
        remember(&mut store, "main", saved).unwrap();
        let document = read_document(&path).unwrap();
        assert_eq!(document.windows["main"].panes, pane_state());
    }

    #[test]
    fn einstellungen_und_zwei_fenster_ueberleben_neustart_und_abschalten_loescht_sitzung() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut store = DesktopStore {
            path: Some(path.clone()),
            ..Default::default()
        };
        store.document.preferences.restore_session = true;
        for label in ["main", "window-test"] {
            remember(
                &mut store,
                label,
                SavedWindow {
                    panes: pane_state(),
                    tabs: None,
                    geometry: None,
                    active_side: "right".into(),
                },
            )
            .unwrap();
        }
        let loaded = read_document(&path).unwrap();
        assert_eq!(loaded.windows.len(), 2);
        assert_eq!(loaded.windows["window-test"].active_side, "right");
        assert_eq!(loaded.preferences.icon, IconChoice::Blue);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("password"));
        assert!(!text.contains("permit"));
        let mut next = loaded;
        next.preferences.restore_session = false;
        next.windows.clear();
        store.commit(next).unwrap();
        assert!(read_document(&path).unwrap().windows.is_empty());
    }

    #[test]
    fn format_eins_migriert_im_ram_ohne_lesenden_dateiersatz() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut legacy = DesktopDocument {
            revision: 19,
            initialized: true,
            preferences: Preferences {
                restore_session: true,
                theme: Theme::Dark,
                ..Default::default()
            },
            ..Default::default()
        };
        legacy.windows.insert(
            "main".into(),
            SavedWindow {
                panes: pane_state(),
                tabs: None,
                geometry: Some(WindowGeometry {
                    x: 50.0,
                    y: 70.0,
                    width: 1200.0,
                    height: 800.0,
                    maximized: false,
                }),
                active_side: "right".into(),
            },
        );
        let mut json = serde_json::to_value(&legacy).unwrap();
        json["version"] = 1.into();
        json["windows"]["main"]
            .as_object_mut()
            .unwrap()
            .remove("tabs");
        let original = serde_json::to_vec_pretty(&json).unwrap();
        std::fs::write(&path, &original).unwrap();
        let migrated = read_document(&path).unwrap();
        assert_eq!(migrated.version, 2);
        assert_eq!(migrated.revision, 19);
        assert!(migrated.initialized);
        assert_eq!(migrated.preferences.theme, Theme::Dark);
        let window = &migrated.windows["main"];
        assert_eq!(window.active_side, "right");
        assert_eq!(window.geometry, legacy.windows["main"].geometry);
        let tabs = window.tabs.as_ref().unwrap();
        assert_eq!(tabs.left.tabs[0].location.path, "/links");
        assert_eq!(tabs.right.tabs[0].location.profile_revision, Some(7));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        write_document(&path, &migrated).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()
                ["version"],
            2
        );
    }

    #[test]
    fn zwei_fenster_erhalten_inaktive_tabs_beim_atomaren_speichern() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut store = DesktopStore {
            path: Some(path.clone()),
            ..Default::default()
        };
        store.document.preferences.restore_session = true;
        for (label, extra_path) in [("main", "/inaktiv-eins"), ("window-zwei", "/inaktiv-zwei")] {
            let panes = pane_state();
            let mut tabs = crate::pane_tabs::WindowTabs::from_active(&panes);
            let mut extra = tabs.right.tabs[0].clone();
            extra.id = "tab-zwei".into();
            extra.location.path = extra_path.into();
            extra.history = vec!["/".into(), extra_path.into()];
            extra.history_index = 1;
            extra.selected_names = vec!["behalten.txt".into()];
            tabs.right.tabs.push(extra);
            remember(
                &mut store,
                label,
                SavedWindow {
                    panes,
                    tabs: Some(tabs),
                    geometry: None,
                    active_side: "right".into(),
                },
            )
            .unwrap();
        }
        let restored = read_document(&path).unwrap();
        for (label, path) in [("main", "/inaktiv-eins"), ("window-zwei", "/inaktiv-zwei")] {
            let tabs = restored.windows[label].tabs.as_ref().unwrap();
            assert_eq!(tabs.right.active_id, "tab-initial");
            assert_eq!(tabs.right.tabs[0].location.path, "/rechts");
            assert_eq!(tabs.right.tabs[1].location.path, path);
            assert_eq!(tabs.right.tabs[1].selected_names, vec!["behalten.txt"]);
            assert_eq!(tabs.left.tabs.len(), 1);
        }
    }

    #[test]
    fn zu_grosser_tabstand_laesst_datei_und_tempnamensraum_unveraendert() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut document = DesktopDocument::default();
        write_document(&path, &document).unwrap();
        let before = std::fs::read(&path).unwrap();
        let panes = pane_state();
        let mut tabs = crate::pane_tabs::WindowTabs::from_active(&panes);
        // Gültige Einzelfelder können gemeinsam die Dateigrenze überschreiten.
        for index in 1..32 {
            let mut tab = tabs.left.tabs[0].clone();
            tab.id = format!("tab-{index}");
            tab.history = vec![format!("/{}", "x".repeat(4090)); 128];
            tabs.left.tabs.push(tab);
        }
        document.windows.insert(
            "main".into(),
            SavedWindow {
                panes,
                tabs: Some(tabs),
                geometry: None,
                active_side: "left".into(),
            },
        );
        assert_eq!(
            write_document(&path, &document).unwrap_err().code,
            "invalid_input"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn startgeometrie_bleibt_bei_gehaltenem_schreibstore_erreichbar_und_persistiert() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop-settings.json");
        let geometry = WindowGeometry {
            x: 43.0,
            y: 57.0,
            width: 1110.0,
            height: 730.0,
            maximized: false,
        };
        let saved = SavedWindow {
            panes: pane_state(),
            tabs: None,
            geometry: Some(geometry.clone()),
            active_side: "right".into(),
        };
        let mut document = DesktopDocument::default();
        document.preferences.restore_session = true;
        document.windows.insert("main".into(), saved);
        write_document(&path, &document).unwrap();
        let loaded = read_document(&path).unwrap();
        let state = std::sync::Arc::new(DesktopState::default());
        state.seed_live_window("main", loaded.windows["main"].clone());
        let held = state.inner.lock_recover();
        let reader = state.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let task =
            std::thread::spawn(move || sender.send(reader.take_startup_geometry("main")).unwrap());
        let result = receiver.recv_timeout(std::time::Duration::from_secs(1));
        drop(held);
        task.join().unwrap();
        assert_eq!(result.unwrap(), Some(geometry.clone()));
        assert!(state.take_startup_geometry("main").is_none());
        assert_eq!(
            state.inner.lock_recover().live["main"].geometry,
            Some(geometry.clone())
        );
        assert_eq!(
            read_document(&path).unwrap().windows["main"].geometry,
            Some(geometry)
        );
    }

    #[test]
    fn icon_task_wartet_nicht_auf_den_speichernden_store() {
        let state = std::sync::Arc::new(DesktopState::default());
        let store = state.inner.lock_recover();
        let reader = state.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            apply_current_icon(&reader, |choice| sender.send(choice).unwrap());
        });
        let result = receiver.recv_timeout(std::time::Duration::from_secs(1));
        drop(store);
        task.join().unwrap();
        assert_eq!(result.unwrap(), IconChoice::Blue);
    }

    #[test]
    fn verspaeteter_icon_task_uebernimmt_die_aktuelle_fensterwahl() {
        let temp = tempfile::tempdir().unwrap();
        let state = DesktopState::default();
        let path = temp.path().join("desktop-settings.json");
        state.inner.lock_recover().path = Some(path.clone());
        let choose = |icon| {
            update_preferences(
                &state,
                PreferencePatch {
                    icon: Some(icon),
                    ..Default::default()
                },
                false,
            )
            .unwrap()
            .0
        };
        let first = choose(IconChoice::Storm);
        // A besteht den bisherigen Vorabvergleich, wird aber noch nicht zugestellt.
        assert_eq!(first.revision, state.inner.lock_recover().document.revision);
        let delayed = || {
            let mut applied = None;
            apply_current_icon(&state, |icon| applied = Some(icon));
            applied.unwrap()
        };
        let second = choose(IconChoice::Aurora);
        assert_eq!(
            second.revision,
            state.inner.lock_recover().document.revision
        );
        // B läuft zuerst; danach trifft A ein. Beide nativen Wirkungen müssen
        // dieselbe jüngste Wahl verwenden, auch wenn die Snapshots verschieden sind.
        let mut applied = Vec::new();
        apply_current_icon(&state, |icon| applied.push(icon));
        applied.push(delayed());
        assert_eq!(applied, vec![IconChoice::Aurora, IconChoice::Aurora]);
        assert_eq!(
            read_document(&path).unwrap().preferences.icon,
            IconChoice::Aurora
        );
    }

    #[test]
    fn spaete_fenstermigration_ueberschreibt_keine_neue_nutzerwahl() {
        let temp = tempfile::tempdir().unwrap();
        let state = std::sync::Arc::new(DesktopState::default());
        state.inner.lock_recover().path = Some(temp.path().join("desktop-settings.json"));
        // Zwei Fenster lesen ihre Altwerte, bevor irgendeines die Migration committet.
        let start = std::sync::Arc::new(std::sync::Barrier::new(3));
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let state = state.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    let snapshot = state.inner.lock_recover().snapshot();
                    start.wait();
                    snapshot
                })
            })
            .collect();
        start.wait();
        for reader in readers {
            assert!(reader.join().unwrap().needs_migration);
        }
        let legacy = || PreferencePatch {
            theme: Some(Theme::Light),
            byte_units: Some(ByteUnits::Binary),
            ..Default::default()
        };
        let (first, _, changed) = update_preferences(&state, legacy(), true).unwrap();
        assert!(changed);
        assert_eq!(first.revision, 1);
        let (new_choice, _, changed) = update_preferences(
            &state,
            PreferencePatch {
                theme: Some(Theme::Dark),
                ..Default::default()
            },
            false,
        )
        .unwrap();
        assert!(changed);
        let late_state = state.clone();
        let (late, warning, changed) =
            std::thread::spawn(move || update_preferences(&late_state, legacy(), true).unwrap())
                .join()
                .unwrap();
        assert!(!changed);
        assert!(warning.is_none());
        assert_eq!(late.revision, new_choice.revision);
        assert_eq!(late.preferences.theme, Theme::Dark);
        assert_eq!(late.preferences.byte_units, ByteUnits::Binary);
        let persisted = read_document(&temp.path().join("desktop-settings.json")).unwrap();
        assert_eq!(persisted.revision, new_choice.revision);
        assert_eq!(persisted.preferences.theme, Theme::Dark);
    }

    #[test]
    fn spaeter_erfasster_fensterstand_gewinnt_auch_bei_umgekehrter_worker_reihenfolge() {
        let state = DesktopState::default();
        let saved = |path: &str| SavedWindow {
            tabs: None,
            panes: serde_json::from_value(serde_json::json!({
                "left":{"provider":"local","path":path},
                "right":{"provider":"local","path":path}
            }))
            .unwrap(),
            geometry: None,
            active_side: "left".into(),
        };

        remember_ordered(&state, "main", 2, saved("/neu")).unwrap();
        remember_ordered(&state, "main", 1, saved("/alt")).unwrap();

        assert_eq!(
            state.inner.lock_recover().live["main"].panes.left.path,
            "/neu"
        );
    }

    #[test]
    fn einzelne_felder_erhalten_fremde_fensteraenderungen() {
        let mut preferences = Preferences::default();
        preferences.apply(serde_json::from_value(serde_json::json!({"icon":"aurora"})).unwrap());
        preferences.apply(
            serde_json::from_value(serde_json::json!({"theme":"dark","byteUnits":"binary"}))
                .unwrap(),
        );
        assert_eq!(preferences.icon, IconChoice::Aurora);
        assert_eq!(preferences.theme, Theme::Dark);
        assert!(serde_json::from_value::<PreferencePatch>(
            serde_json::json!({"restoreSesion":true})
        )
        .is_err());
    }

    /// Regression: Tauri benennt verschachtelte JSON-Felder nicht zuverlässig so um
    /// wie Top-Level-Argumente. `PreferencePatch` muss deshalb genau die
    /// Schreibweisen annehmen, die `settings-ui.js` wirklich sendet — und weil
    /// `deny_unknown_fields` gilt, fällt ein umbenanntes Rust-Feld sonst erst zur
    /// Laufzeit als abgelehnter Patch auf. Der frühere Test deckte nur `icon`,
    /// `theme` und `byteUnits` ab; die vier übrigen Felder waren ungeschützt.
    #[test]
    fn jedes_frontend_feld_des_patches_wird_angenommen() {
        for (feld, wert) in [
            ("icon", serde_json::json!("storm")),
            ("theme", serde_json::json!("light")),
            ("byteUnits", serde_json::json!("decimal")),
            ("localFolderSizes", serde_json::json!(true)),
            ("networkDiscovery", serde_json::json!(false)),
            ("restoreSession", serde_json::json!(true)),
            ("diffTool", serde_json::json!("bbedit")),
        ] {
            let patch = serde_json::from_value::<PreferencePatch>(
                serde_json::json!({ feld.to_string(): wert }),
            );
            assert!(
                patch.is_ok(),
                "Feld {feld} wird nicht angenommen: {}",
                patch
                    .err()
                    .map_or_else(String::new, |error| error.to_string())
            );
        }
    }

    #[test]
    fn kaputte_oder_verlinkte_einstellungen_werden_nicht_ueberschrieben() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"kaputt").unwrap();
        let error = read_document(&path).err().unwrap();
        let mut store = DesktopStore {
            path: Some(path.clone()),
            load_error: Some(error),
            ..Default::default()
        };
        assert!(store.commit(DesktopDocument::default()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"kaputt");
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(path, &link).unwrap();
        assert!(read_document(&link).is_err());

        // Ein Symlink auf ein GÜLTIGES Dokument darf beim atomaren Commit ersetzt,
        // aber niemals verfolgt werden. So prüft der Test den Schreibpfad selbst:
        // Das fremde Linkziel bleibt unverändert, während am Einstellungsnamen eine
        // neue reguläre Datei erscheint.
        let valid_target = dir.path().join("gueltig.json");
        let mut valid = DesktopDocument::default();
        valid.preferences.icon = IconChoice::Storm;
        assert!(write_document(&valid_target, &valid).unwrap().is_none());
        assert_eq!(
            read_document(&valid_target).unwrap().preferences.icon,
            IconChoice::Storm
        );
        let valid_link = dir.path().join("gueltig-link.json");
        std::os::unix::fs::symlink(&valid_target, &valid_link).unwrap();
        assert!(read_document(&valid_link).is_err());
        assert!(write_document(&valid_link, &DesktopDocument::default())
            .unwrap()
            .is_none());
        assert!(std::fs::symlink_metadata(&valid_link)
            .unwrap()
            .file_type()
            .is_file());
        assert_eq!(
            read_document(&valid_target).unwrap().preferences.icon,
            IconChoice::Storm
        );
        assert_eq!(
            read_document(&valid_link).unwrap().preferences.icon,
            IconChoice::Blue
        );
    }

    #[test]
    fn zu_grosse_einstellungsdatei_wird_beim_lesen_abgelehnt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut bytes =
            b"{\"version\":1,\"revision\":0,\"preferences\":{},\"windows\":{},\"x\":\"".to_vec();
        bytes.resize(MAX_DOCUMENT_BYTES as usize + 16, b'a');
        std::fs::write(&path, &bytes).unwrap();
        let error = read_document(&path).err().unwrap();
        assert!(error.message.contains("Größe"), "{}", error.message);
    }
}
