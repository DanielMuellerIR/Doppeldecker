//! Lokale Pane-Navigation und shell-freies Öffnen mit der System-App.

use std::path::{Path, PathBuf};
use std::process::Command;

use doppeldecker_core::{
    cleanup_remote_snapshots, expand_home_path, Cancel, Entry, LocalVfs, Vfs, SNAPSHOT_RETENTION,
};
use tauri::{Manager, WebviewWindow};

use crate::bidirectional_commands::{
    resolve_endpoint, resolve_validated_registry_pair, validate_registry_profile,
    BidirectionalEndpointRequest,
};
use crate::desktop_settings::DiffTool;
use crate::{app_config_dir, sandbox_root, AppError, AppState};

#[derive(serde::Serialize)]
pub(crate) struct DirListing {
    path: String,
    parent: Option<String>,
    entries: Vec<BrowserEntry>,
}

/// Pane-Eintrag samt Aussage, ob der angezeigte Name den echten Dateisystemnamen
/// eindeutig wiedergibt. Bei lokalen Nicht-UTF-8-Namen ist `Entry::name` nur eine
/// verlustbehaftete Anzeige und darf deshalb nie wieder zu einem Pfad zusammengesetzt
/// werden.
#[derive(serde::Serialize)]
struct BrowserEntry {
    #[serde(flatten)]
    entry: Entry,
    safely_decoded: bool,
}

#[derive(serde::Serialize)]
pub(crate) struct DefaultDirs {
    pub(crate) left: String,
    pub(crate) right: String,
}

/// Die markierte GUI-Testsandbox verspricht, dass auch app-eigene
/// Momentaufnahmen ausschließlich dort entstehen. Im Normalbetrieb bleiben sie
/// im betriebssystemüblichen Cache und damit außerhalb der dauerhaften Profile.
fn snapshot_cache_path(
    sandbox_config: Option<&Path>,
    application_cache: &Path,
    leaf: &str,
) -> std::io::Result<PathBuf> {
    if let Some(config) = sandbox_config {
        // Jede neue Ebene muss direkt im bereits gebundenen Elternordner liegen:
        // create_dir_all würde einen vorhandenen cache-Symlink still verfolgen.
        let cache = crate::app_support::prepare_direct_directory(config, "cache")?;
        crate::app_support::prepare_direct_directory(&cache, leaf)
    } else {
        Ok(application_cache.join(leaf))
    }
}

fn snapshot_cache(app: &tauri::AppHandle, leaf: &str) -> Result<PathBuf, AppError> {
    let application_cache = app
        .path()
        .app_cache_dir()
        .map_err(|error| AppError::new("io", error.to_string()))?;
    let sandbox_config = if sandbox_root()?.is_some() {
        Some(app_config_dir(app)?)
    } else {
        None
    };
    snapshot_cache_path(sandbox_config.as_deref(), &application_cache, leaf).map_err(AppError::from)
}

#[tauri::command]
pub(crate) fn default_dirs() -> Result<DefaultDirs, AppError> {
    if let Some(root) = sandbox_root()? {
        let left = root.join("quelle");
        let right = root.join("ziel");
        std::fs::create_dir_all(&left).map_err(AppError::from)?;
        std::fs::create_dir_all(&right).map_err(AppError::from)?;
        return Ok(DefaultDirs {
            left: left.to_string_lossy().into_owned(),
            right: right.to_string_lossy().into_owned(),
        });
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    Ok(DefaultDirs {
        left: home.clone(),
        right: home,
    })
}

#[tauri::command]
pub(crate) fn scan_dir(path: String, show_hidden: bool) -> Result<DirListing, AppError> {
    let absolute = std::fs::canonicalize(expand_home_path(&path))
        .map_err(|error| AppError::new("app", format!("{path}: {error}")))?;
    let mut entries: Vec<_> = LocalVfs::new()
        .list_complete(&absolute)
        .map_err(AppError::from)?
        .entries
        .into_iter()
        .map(|listed| BrowserEntry {
            entry: listed.entry,
            safely_decoded: listed.safely_decoded,
        })
        .collect();
    if !show_hidden {
        entries.retain(|entry| !entry.entry.name.starts_with('.'));
    }
    entries.sort_by(|left, right| {
        right.entry.is_dir.cmp(&left.entry.is_dir).then_with(|| {
            left.entry
                .name
                .to_lowercase()
                .cmp(&right.entry.name.to_lowercase())
        })
    });
    let parent = absolute
        .parent()
        .map(|path| path.to_string_lossy().into_owned());
    Ok(DirListing {
        path: absolute.to_string_lossy().into_owned(),
        parent,
        entries,
    })
}

pub(crate) fn validate_openable_file(path: &Path) -> Result<PathBuf, AppError> {
    let metadata = std::fs::symlink_metadata(path).map_err(AppError::from)?;
    if metadata.file_type().is_symlink() {
        return Err(AppError::new(
            "app",
            "Symbolische Links werden nicht mit einer System-App geöffnet",
        ));
    }
    if !metadata.is_file() {
        return Err(AppError::new(
            "app",
            "Nur reguläre Dateien können mit einer System-App geöffnet werden",
        ));
    }
    std::fs::canonicalize(path).map_err(AppError::from)
}

/// Der Pfad bleibt ein eigenes Prozessargument und durchläuft keine Shell.
pub(crate) fn system_open_command(path: &Path) -> Result<Command, AppError> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err(AppError::new(
        "app",
        "System-App-Öffnen wird auf dieser Plattform nicht unterstützt",
    ));
    command.arg(path);
    Ok(command)
}

/// Startet einen Helfer, dessen Ende uns nicht interessiert, und räumt ihn im
/// Hintergrund ab. Ein verworfenes `Child` würde Rust nie `wait`en; der beendete
/// Prozess bliebe bis zum App-Ende als Zombie in der Prozesstabelle stehen.
pub(crate) fn spawn_detached(command: &mut Command) -> std::io::Result<()> {
    let child = std::sync::Arc::new(std::sync::Mutex::new(Some(command.spawn()?)));
    let child_for_reaper = std::sync::Arc::clone(&child);
    match std::thread::Builder::new()
        .name("reap-helper".into())
        .spawn(move || {
            if let Ok(mut child) = child_for_reaper.lock() {
                if let Some(mut child) = child.take() {
                    let _ = child.wait();
                }
            }
        }) {
        Ok(_) => Ok(()),
        Err(error) => {
            // Auch der seltene Fehler beim Anlegen des Reaper-Threads darf keinen
            // gestarteten Kindprozess als Zombie zurücklassen. In diesem Pfad ist
            // synchrones Warten die einzig verbleibende sichere Aufräumoption.
            if let Ok(mut child) = child.lock() {
                if let Some(mut child) = child.take() {
                    let _ = child.wait();
                }
            }
            Err(error)
        }
    }
}

#[tauri::command]
pub(crate) fn open_path(path: String) -> Result<(), AppError> {
    let path = validate_openable_file(&expand_home_path(&path))?;
    spawn_detached(&mut system_open_command(&path)?).map_err(|error| {
        AppError::new(
            "app",
            format!("System-App konnte nicht gestartet werden: {error}"),
        )
    })
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectPage {
    Support,
    Imprint,
    Privacy,
}

impl ProjectPage {
    fn url(self) -> &'static str {
        match self {
            Self::Support => "https://github.com/DanielMuellerIR/Doppeldecker/issues",
            Self::Imprint => "https://dm0.de/impressum.html",
            Self::Privacy => "https://dm0.de/datenschutz.html#doppeldecker",
        }
    }
}

#[tauri::command]
pub(crate) fn open_project_page(page: ProjectPage) -> Result<(), AppError> {
    spawn_detached(&mut system_open_command(Path::new(page.url()))?).map_err(AppError::from)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenRemoteSnapshotRequest {
    endpoint: BidirectionalEndpointRequest,
    path: String,
}

#[tauri::command]
pub(crate) async fn open_remote_snapshot(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: OpenRemoteSnapshotRequest,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        // Vor dem ersten Providerzugriff reservieren: Auch der Verbindungsaufbau
        // gehört zum angezeigten, abbrechbaren Lauf dieses Fensters.
        let _terminal_outcome = crate::queue_runtime::with_reserved_cancellable_operation(
            &app,
            label,
            &window_state,
            |cancel| {
                let limitations =
                    if request.endpoint.provider_kind() == doppeldecker_core::ProviderKind::Sftp {
                        vec![doppeldecker_core::sftp::SFTP_CONTENT_READ_LIMITATION.to_owned()]
                    } else {
                        Vec::new()
                    };
                let _ = crate::emit_to_window(
                    &window,
                    "remote-snapshot-started",
                    serde_json::json!({"limitations": limitations}),
                );
                open_remote_snapshot_blocking(&app, &state, request, cancel)
            },
            |result| match result {
                Ok(snapshot) => {
                    let _ = crate::emit_to_window(&window, "remote-snapshot-done", snapshot);
                }
                Err(error) => {
                    let _ = crate::emit_to_window(&window, "remote-snapshot-error", error);
                }
            },
        )?;
        // Das Terminalevent kommt vor dem nächsten Queueworker. Eine spätere
        // Command-Antwort darf dessen neue Fortschrittsanzeige nicht zurücksetzen.
        Ok(())
    })
    .await
    .map_err(|_| AppError::new("app", "Dateiöffnungs-Worker wurde unterbrochen"))?
}

#[derive(serde::Serialize)]
struct OpenedRemoteSnapshot {
    name: String,
    bytes: u64,
    limitations: Vec<String>,
}

fn snapshot_not_cancelled(cancel: &Cancel) -> Result<(), AppError> {
    if cancel.is_cancelled() {
        Err(AppError::new(
            "interrupted",
            "Dateivorbereitung wurde abgebrochen",
        ))
    } else {
        Ok(())
    }
}

fn open_remote_snapshot_blocking(
    app: &tauri::AppHandle,
    state: &AppState,
    request: OpenRemoteSnapshotRequest,
    cancel: &Cancel,
) -> Result<OpenedRemoteSnapshot, AppError> {
    snapshot_not_cancelled(cancel)?;
    crate::bidirectional_commands::validate_endpoint_request_root(&request.endpoint)?;
    let source = PathBuf::from(&request.path);
    if request.endpoint.provider_kind() == doppeldecker_core::ProviderKind::Sftp {
        doppeldecker_core::sftp::validate_interactive_content_selection(
            Path::new(request.endpoint.root()),
            std::slice::from_ref(&source),
        )
        .map_err(AppError::from)?;
    }
    let resolved = resolve_endpoint(app, state, &request.endpoint);
    snapshot_not_cancelled(cancel)?;
    let (endpoint, _, profile_bindings) = resolved?;
    if !source.starts_with(endpoint.root()) || source == endpoint.root() {
        return Err(AppError::new(
            "sandbox_violation",
            "Remote-Datei verlässt den gebundenen Pane-Root",
        ));
    }
    let cache = snapshot_cache(app, "remote-snapshots")?;
    cleanup_remote_snapshots(&cache, std::time::SystemTime::now(), SNAPSHOT_RETENTION)
        .map_err(AppError::from)?;
    snapshot_not_cancelled(cancel)?;
    let snapshot = endpoint
        .download_interactive_snapshot(&source, &cache, cancel)
        .map_err(AppError::from)?;
    snapshot_not_cancelled(cancel)?;
    crate::queue_runtime::ensure_profile_bindings_current(app, &profile_bindings)?;
    let local = validate_openable_file(Path::new(&snapshot.path))?;
    snapshot_not_cancelled(cancel)?;
    spawn_detached(&mut system_open_command(&local)?).map_err(|error| {
        AppError::new(
            "io",
            format!("System-App konnte nicht gestartet werden: {error}"),
        )
    })?;
    Ok(OpenedRemoteSnapshot {
        name: source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        bytes: snapshot.bytes,
        limitations: endpoint.interactive_file_read_limitations(),
    })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiffToolAvailability {
    id: DiffTool,
    available: bool,
    detail: String,
}

fn diff_program(tool: DiffTool) -> (&'static str, &'static [&'static str]) {
    match tool {
        DiffTool::Xcode => (
            "/Applications/Xcode.app/Contents/Developer/usr/bin/opendiff",
            &[],
        ),
        DiffTool::Bbedit => ("/Applications/BBEdit.app/Contents/Helpers/bbdiff", &[]),
        // Fastra-Protokoll 1: Der Bundle-Helfer übernimmt die lokale IPC-Übergabe.
        // Die beiden Dateipfade bleiben getrennte argv-Werte.
        DiffTool::Fastra => (
            "/Applications/Fastra.app/Contents/Helpers/fastra-diff",
            &["--read-only", "--focus-diff"],
        ),
    }
}

fn tool_status(tool: DiffTool) -> DiffToolAvailability {
    let (program, _) = diff_program(tool);
    let capability_error = if tool == DiffTool::Fastra && Path::new(program).is_file() {
        check_fastra_capabilities(Path::new(program)).err()
    } else {
        None
    };
    let available = Path::new(program).is_file() && capability_error.is_none();
    let detail = if let Some(error) = capability_error {
        error.message
    } else {
        match (tool, available) {
            (DiffTool::Xcode, true) => "Xcode FileMerge ist verfügbar.".into(),
            (DiffTool::Xcode, false) => "Xcode FileMerge wurde nicht gefunden.".into(),
            (DiffTool::Bbedit, true) => "BBEdit bbdiff ist verfügbar.".into(),
            (DiffTool::Bbedit, false) => "BBEdit bbdiff wurde nicht gefunden.".into(),
            (DiffTool::Fastra, true) => "Fastras externe Diff-Schnittstelle ist verfügbar.".into(),
            (DiffTool::Fastra, false) if Path::new("/Applications/Fastra.app").is_dir() => {
                "Fastra ist installiert; der Helfer fastra-diff fehlt noch.".into()
            }
            (DiffTool::Fastra, false) => "Fastra oder sein Helfer fastra-diff fehlt.".into(),
        }
    };
    DiffToolAvailability {
        id: tool,
        available,
        detail,
    }
}

#[tauri::command]
pub(crate) async fn diff_tool_status() -> Result<Vec<DiffToolAvailability>, AppError> {
    tauri::async_runtime::spawn_blocking(|| {
        [DiffTool::Xcode, DiffTool::Fastra, DiffTool::Bbedit]
            .into_iter()
            .map(tool_status)
            .collect()
    })
    .await
    .map_err(|e| AppError::new("app", e.to_string()))
}

/// Die Abfrage startet laut Fastra-Vertrag kein Fenster. Eine eigene Frist
/// begrenzt auch defekte/alte Helfer; nur explizit zugesagte Fähigkeiten gelten.
fn check_fastra_capabilities(program: &Path) -> Result<(), AppError> {
    check_fastra_capability_command(Command::new(program))
}

fn check_fastra_capability_command(mut command: Command) -> Result<(), AppError> {
    command.args(["--capabilities", "--json"]);
    let output = crate::system_mount_commands::command_output_with_timeout(
        command,
        std::time::Duration::from_secs(2),
    )
    .map_err(AppError::from)?;
    let valid = output
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .is_some_and(|v| {
            v["protocol"] == 1
                && [
                    "fileDiff",
                    "readOnly",
                    "focusDiff",
                    "labels",
                    "existingInstanceIpc",
                ]
                .iter()
                .all(|key| v[*key] == true)
        });
    if valid {
        Ok(())
    } else {
        Err(AppError::new(
            "unavailable",
            "Fastra bestätigt die benötigte Diff-Schnittstelle (Protokoll 1) nicht.",
        ))
    }
}

fn fastra_exit(code: Option<i32>) -> Result<(), AppError> {
    let (kind, message) = match code {
        Some(0) => return Ok(()),
        Some(2) => ("invalid_input", "Fastra hat die Vergleichsargumente abgelehnt oder die Anfrage ist zu groß."),
        Some(3) => ("io", "Fastra kann eine Vergleichsdatei nicht als reguläre Datei lesen."),
        Some(4) => ("unavailable", "Fastra unterstützt die angeforderte Diff-Schnittstelle nicht."),
        Some(5) => ("unavailable", "Fastra konnte nicht gestartet werden."),
        _ => ("app", "Fastra hat die Übergabe nicht bestätigt. Möglicherweise wurde bereits ein Vergleichsfenster geöffnet; vor erneutem Aufruf prüfen."),
    };
    Err(AppError::new(kind, message))
}

/// Zwei Sekunden Reserve über Fastras eigener 10-Sekunden-Frist.
const FASTRA_LAUNCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

fn launch_diff(tool: DiffTool, command: Command) -> Result<(), AppError> {
    launch_diff_with_timeout(tool, command, FASTRA_LAUNCH_TIMEOUT)
}

/// Die Frist ist ein Parameter, damit der Test den Timeout-Pfad in Sekunden statt
/// erst nach zwölf Sekunden belegen kann.
fn launch_diff_with_timeout(
    tool: DiffTool,
    mut command: Command,
    timeout: std::time::Duration,
) -> Result<(), AppError> {
    if tool == DiffTool::Fastra {
        // Exit 0 bestätigt ein angelegtes Fenster, nicht erst das Ende des
        // Vergleichs. Nach unklarer Übergabe niemals automatisch erneut starten.
        let output = crate::system_mount_commands::command_output_with_timeout(command, timeout)
            .map_err(AppError::from)?;
        fastra_exit(output.and_then(|o| o.status.code()))
    } else {
        spawn_detached(&mut command).map_err(AppError::from)
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DiffFileRequest {
    endpoint: BidirectionalEndpointRequest,
    path: String,
    name: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CompareFilesRequest {
    left: DiffFileRequest,
    right: DiffFileRequest,
}

fn validate_diff_path(root: &str, path: &str) -> Result<(), AppError> {
    let root = Path::new(root);
    let path = Path::new(path);
    if !path.is_absolute() || path == root || !path.starts_with(root) {
        return Err(AppError::new(
            "sandbox_violation",
            "Diff-Datei verlässt den gebundenen Pane-Root",
        ));
    }
    Ok(())
}

fn diff_command(
    tool: DiffTool,
    left: &Path,
    right: &Path,
    left_name: &str,
    right_name: &str,
) -> Result<Command, AppError> {
    let (program, prefix) = diff_program(tool);
    // Nur die Existenz des Helfers wird hier geprüft. Die Fastra-Fähigkeitsabfrage
    // (bis zu zwei Sekunden) gehört in den Einstellungsdialog; beim Start selbst
    // meldet Exit 4 ein nicht unterstütztes Protokoll ohnehin und ohne Wartezeit.
    if !Path::new(program).is_file() {
        return Err(AppError::new("unavailable", tool_status(tool).detail));
    }
    let mut command = Command::new(program);
    command.args(diff_arguments(
        tool, prefix, left, right, left_name, right_name,
    ));
    Ok(command)
}

fn diff_arguments(
    tool: DiffTool,
    prefix: &[&str],
    left: &Path,
    right: &Path,
    left_name: &str,
    right_name: &str,
) -> Vec<std::ffi::OsString> {
    let mut arguments = prefix
        .iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
    if tool == DiffTool::Fastra {
        arguments.extend([
            "--left-label".into(),
            left_name.into(),
            "--right-label".into(),
            right_name.into(),
            "--".into(),
        ]);
    }
    arguments.extend([left.as_os_str().into(), right.as_os_str().into()]);
    arguments
}

/// Beide Seiten werden zuerst in je eine geprüfte, app-eigene Momentaufnahme
/// übertragen. Ein Merge im externen Werkzeug kann deshalb keine Pane-Datei
/// überschreiben. SFTP verwendet dieselbe begrenzte Inhaltsklasse wie Öffnen;
/// daraus entsteht weder ein Plannerbeleg noch ein Schreibrecht.
#[tauri::command]
pub(crate) async fn compare_files(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: CompareFilesRequest,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        validate_compare_request(&request)?;
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        // Verbindung und beide Downloads gehören zum selben abbrechbaren Lauf.
        // Das Terminalevent geht vor dem nächsten Queueworker an dieses Fenster;
        // die spätere Command-Antwort setzt dessen Anzeige nicht wieder zurück.
        let _outcome = crate::queue_runtime::with_reserved_cancellable_operation(
            &app,
            label,
            &window_state,
            |cancel| {
                let limitations = diff_read_limitations(&request);
                let _ = crate::emit_to_window(
                    &window,
                    "file-diff-started",
                    serde_json::json!({"limitations": limitations}),
                );
                compare_files_blocking(&app, &window, &state, request, cancel)
            },
            |result| match result {
                Ok(result) => {
                    let _ = crate::emit_to_window(&window, "file-diff-done", result);
                }
                Err(error) => {
                    let _ = crate::emit_to_window(&window, "file-diff-error", error);
                }
            },
        )?;
        Ok(())
    })
    .await
    .map_err(|_| AppError::new("app", "Dateivergleichs-Worker wurde unterbrochen"))?
}

#[derive(serde::Serialize)]
struct ComparedFiles {
    limitations: Vec<String>,
}

fn diff_read_limitations(request: &CompareFilesRequest) -> Vec<String> {
    if [&request.left, &request.right]
        .iter()
        .any(|file| file.endpoint.provider_kind() == doppeldecker_core::ProviderKind::Sftp)
    {
        vec![doppeldecker_core::sftp::SFTP_CONTENT_READ_LIMITATION.to_owned()]
    } else {
        Vec::new()
    }
}

fn validate_compare_request(request: &CompareFilesRequest) -> Result<(), AppError> {
    for file in [&request.left, &request.right] {
        crate::bidirectional_commands::validate_endpoint_request_root(&file.endpoint)?;
        validate_diff_path(file.endpoint.root(), &file.path)?;
        if file.name.is_empty()
            || file.name.len() > 255
            || file.name.contains('/')
            || file.name.contains('\0')
        {
            return Err(AppError::new(
                "invalid_input",
                "Ungültiger Anzeigename für den Dateivergleich",
            ));
        }
        if file.endpoint.provider_kind() == doppeldecker_core::ProviderKind::Sftp {
            doppeldecker_core::sftp::validate_interactive_content_selection(
                Path::new(file.endpoint.root()),
                &[PathBuf::from(&file.path)],
            )
            .map_err(AppError::from)?;
        }
    }
    Ok(())
}

fn download_diff_snapshots(
    left: &doppeldecker_core::ProviderEndpoint,
    right: &doppeldecker_core::ProviderEndpoint,
    request: &CompareFilesRequest,
    cache: &Path,
    cancel: &Cancel,
) -> Result<
    (
        doppeldecker_core::RemoteSnapshot,
        doppeldecker_core::RemoteSnapshot,
    ),
    AppError,
> {
    snapshot_not_cancelled(cancel)?;
    let left = left
        .download_interactive_snapshot(Path::new(&request.left.path), cache, cancel)
        .map_err(AppError::from)?;
    snapshot_not_cancelled(cancel)?;
    let right = right
        .download_interactive_snapshot(Path::new(&request.right.path), cache, cancel)
        .map_err(AppError::from)?;
    snapshot_not_cancelled(cancel)?;
    Ok((left, right))
}

fn compare_files_blocking(
    app: &tauri::AppHandle,
    window: &WebviewWindow,
    state: &AppState,
    request: CompareFilesRequest,
    cancel: &Cancel,
) -> Result<ComparedFiles, AppError> {
    snapshot_not_cancelled(cancel)?;
    let ((left_endpoint, _, left_bindings), (right_endpoint, _, right_bindings)) =
        resolve_validated_registry_pair(
            &request.left.endpoint,
            &request.right.endpoint,
            |endpoint| {
                snapshot_not_cancelled(cancel)?;
                validate_registry_profile(app, endpoint)
            },
            |endpoint| {
                snapshot_not_cancelled(cancel)?;
                resolve_endpoint(app, state, endpoint)
            },
        )?;
    snapshot_not_cancelled(cancel)?;
    let cache = snapshot_cache(app, "diff-snapshots")?;
    cleanup_remote_snapshots(&cache, std::time::SystemTime::now(), SNAPSHOT_RETENTION)
        .map_err(AppError::from)?;
    let (left, right) =
        download_diff_snapshots(&left_endpoint, &right_endpoint, &request, &cache, cancel)?;
    crate::queue_runtime::ensure_profile_bindings_current(app, &left_bindings)?;
    crate::queue_runtime::ensure_profile_bindings_current(app, &right_bindings)?;
    snapshot_not_cancelled(cancel)?;
    let tool = crate::desktop_settings::preferences(app).diff_tool;
    let command = diff_command(
        tool,
        Path::new(&left.path),
        Path::new(&right.path),
        &request.left.name,
        &request.right.name,
    )?;
    // Nach dem Absenden kann der Helfer bereits ein Fenster geöffnet haben.
    // Ein später Abbruch liefert deshalb keinen behaupteten mutationslosen
    // Ausgang und führt auch bei unbestätigter Übergabe nie zu einem Blind-Retry.
    snapshot_not_cancelled(cancel)?;
    let _ = crate::emit_to_window(
        window,
        "file-diff-started",
        serde_json::json!({"phase": "handoff", "limitations": diff_read_limitations(&request)}),
    );
    launch_diff(tool, command).map_err(|error| {
        // Interrupted nach dem Absenden kann vom Helfer-I/O stammen. Es darf
        // nicht wie der Abbruch der noch wirkungsfreien Vorbereitung erscheinen.
        if error.code == "interrupted" {
            AppError::new(
                "outcome_unclear",
                format!("Werkzeugübergabe wurde unterbrochen; ein Vergleichsfenster kann bereits geöffnet sein. Vor erneutem Aufruf prüfen: {}", error.message),
            )
        } else {
            error
        }
    })?;
    Ok(ComparedFiles {
        limitations: diff_read_limitations(&request),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn projektlinks_akzeptieren_nur_benannte_feste_https_ziele() {
        for value in ["support", "imprint", "privacy"] {
            let page: super::ProjectPage =
                serde_json::from_value(serde_json::json!(value)).unwrap();
            let url = url::Url::parse(page.url()).unwrap();
            assert_eq!(url.scheme(), "https");
            assert!(url.username().is_empty());
            assert!(url.password().is_none());
            let command = super::system_open_command(std::path::Path::new(page.url())).unwrap();
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                vec![std::ffi::OsStr::new(page.url())]
            );
        }
        for value in [
            "https://example.com",
            "file:///tmp/probe",
            "--help",
            "other",
        ] {
            assert!(
                serde_json::from_value::<super::ProjectPage>(serde_json::json!(value)).is_err()
            );
        }
    }

    use super::*;
    use doppeldecker_core::ProviderKind;

    // APFS lehnt ungültiges UTF-8 in Dateinamen ab; Linux erlaubt den Gegenfall.
    // Der Test hängt als einzelner Test am Plattformzweig, nicht als eigenes Modul:
    // Zwei gleichnamige `mod tests` nebeneinander sind unter Linux beide zugleich
    // aktiv und brechen den Build mit E0428 ab (Review-Fund 2026-09-04).
    #[cfg(target_os = "linux")]
    #[test]
    fn scan_unterscheidet_rohen_nicht_utf8_namen_vom_echten_ersatzzeichen() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let temp = tempfile::tempdir().unwrap();
        let unsafe_name = OsString::from_vec(vec![0xff]);
        std::fs::write(temp.path().join(&unsafe_name), b"roh").unwrap();
        std::fs::write(temp.path().join("\u{fffd}"), b"utf8").unwrap();

        let listing = scan_dir(temp.path().to_string_lossy().into_owned(), true).unwrap();
        let collisions: Vec<_> = listing
            .entries
            .iter()
            .filter(|entry| entry.entry.name == "\u{fffd}")
            .map(|entry| entry.safely_decoded)
            .collect();

        assert_eq!(collisions.len(), 2);
        assert!(collisions.contains(&false));
        assert!(collisions.contains(&true));
    }

    /// Auch dieser verschachtelte Endpunkt hatte keinen eigenen Regressionstest.
    /// Tauri benennt verschachtelte Felder nicht zuverlässig um; 2026-07-28 fiel das
    /// beim Remote-Löschen erst zur Laufzeit auf.
    #[test]
    fn snapshot_request_liest_den_verschachtelten_endpunkt() {
        let request: OpenRemoteSnapshotRequest = serde_json::from_value(serde_json::json!({
            "endpoint": {
                "provider": "ftps",
                "profileId": "profil-5",
                "profileRevision": 11,
                "root": "/httpdocs"
            },
            "path": "/httpdocs/bild.png"
        }))
        .unwrap();

        assert_eq!(request.endpoint.provider_kind(), ProviderKind::Ftps);
        assert_eq!(request.path, "/httpdocs/bild.png");
    }

    #[test]
    fn diff_request_liest_beide_verschachtelten_endpunkte_und_begrenzt_pfade() {
        let request: CompareFilesRequest = serde_json::from_value(serde_json::json!({
            "left":{"endpoint":{"provider":"local","root":"/links"},"path":"/links/a.txt","name":"a.txt"},
            "right":{"endpoint":{"provider":"sftp","profileId":"p","profileRevision":3,"root":"/rechts"},"path":"/rechts/b.txt","name":"b.txt"}
        })).unwrap();
        assert_eq!(request.right.endpoint.provider_kind(), ProviderKind::Sftp);
        assert!(validate_diff_path(request.left.endpoint.root(), &request.left.path).is_ok());
        assert!(validate_diff_path("/links", "/anders/a.txt").is_err());
        assert!(
            serde_json::from_value::<CompareFilesRequest>(serde_json::json!({
                "left":{"endpoint":{"provider":"local","root":"/links"},"path":"/links/a","name":"a"},
                "right":{"endpoint":{"provider":"local","root":"/rechts"},"path":"/rechts/b","name":"b"},
                "shell":"rm"
            }))
            .is_err()
        );
    }

    #[test]
    fn diff_momentaufnahmen_sind_inhaltsgleich_und_abbruch_beginnt_keinen_cache() {
        let area = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(area.path()).unwrap();
        let left_root = root.join("left");
        let right_root = root.join("right");
        std::fs::create_dir(&left_root).unwrap();
        std::fs::create_dir(&right_root).unwrap();
        let left = left_root.join("Grüße %2F.txt");
        let right = right_root.join("Andere Datei.txt");
        std::fs::write(&left, b"links\n").unwrap();
        std::fs::write(&right, b"rechts\n").unwrap();
        let request: CompareFilesRequest = serde_json::from_value(serde_json::json!({
            "left":{"endpoint":{"provider":"local","root":left_root},"path":left,"name":"Grüße %2F.txt"},
            "right":{"endpoint":{"provider":"local","root":right_root},"path":right,"name":"Andere Datei.txt"}
        })).unwrap();
        validate_compare_request(&request).unwrap();
        let left_endpoint = doppeldecker_core::ProviderEndpoint::local(&left_root).unwrap();
        let right_endpoint = doppeldecker_core::ProviderEndpoint::local(&right_root).unwrap();
        let cancelled = Cancel::new();
        cancelled.cancel();
        let cache = root.join("cancelled-cache");
        assert_eq!(
            download_diff_snapshots(
                &left_endpoint,
                &right_endpoint,
                &request,
                &cache,
                &cancelled
            )
            .unwrap_err()
            .code,
            "interrupted"
        );
        assert!(!cache.exists());
        let (left_snapshot, right_snapshot) = download_diff_snapshots(
            &left_endpoint,
            &right_endpoint,
            &request,
            &root.join("cache"),
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(std::fs::read(&left_snapshot.path).unwrap(), b"links\n");
        assert_eq!(std::fs::read(&right_snapshot.path).unwrap(), b"rechts\n");
        assert_ne!(Path::new(&left_snapshot.path), left);
        assert_ne!(Path::new(&right_snapshot.path), right);
        assert_eq!(std::fs::read(left).unwrap(), b"links\n");
        assert_eq!(std::fs::read(right).unwrap(), b"rechts\n");
        assert!(diff_read_limitations(&request).is_empty());
    }

    #[test]
    fn diff_sftp_auswahl_wird_vor_profilzugriff_begrenzt_und_risiko_nicht_verdoppelt() {
        let mut request: CompareFilesRequest = serde_json::from_value(serde_json::json!({
            "left":{"endpoint":{"provider":"sftp","profileId":"synthetisch-links","profileRevision":1,"root":"/pane"},"path":"/pane/a.txt","name":"a.txt"},
            "right":{"endpoint":{"provider":"sftp","profileId":"synthetisch-rechts","profileRevision":1,"root":"/pane"},"path":"/pane/b.txt","name":"b.txt"}
        })).unwrap();
        validate_compare_request(&request).unwrap();
        assert_eq!(
            diff_read_limitations(&request),
            [doppeldecker_core::sftp::SFTP_CONTENT_READ_LIMITATION]
        );
        request.right.path = "/pane/unter/b.txt".into();
        assert_eq!(
            validate_compare_request(&request).unwrap_err().code,
            "invalid_input"
        );
        request.right.path = "/fremd/b.txt".into();
        assert_eq!(
            validate_compare_request(&request).unwrap_err().code,
            "sandbox_violation"
        );
    }

    #[test]
    fn diff_werkzeuge_bekommen_pfade_als_getrennte_argumente_ohne_shell() {
        let left = Path::new("/tmp/links mit leerzeichen-$(touch fremd).txt");
        let right = Path::new("/tmp/rechts;echo.txt");
        let (_, prefix) = diff_program(DiffTool::Fastra);
        let actual = diff_arguments(
            DiffTool::Fastra,
            prefix,
            left,
            right,
            "links.txt",
            "rechts.txt",
        );
        assert!(actual
            .iter()
            .all(|argument| argument != "sh" && argument != "-c"));
        assert_eq!(actual[actual.len() - 2], left.as_os_str());
        assert_eq!(actual[actual.len() - 1], right.as_os_str());
    }

    fn fake_fastra(body: &str) -> Command {
        // Frisch erzeugte ausführbare Skripte hängen unter macOS mitunter schon
        // vor ihrer ersten Zeile in der Ausführungsprüfung; unter Linux trat
        // ETXTBSY auf. Die künstliche Programmlogik läuft deshalb als Argument
        // des festen Systeminterpreters. Es bleiben echte Kindprozesse mit den
        // produktiven Pipe-, Status- und Fristprüfungen, aber keine neuen Binaries.
        let mut command = Command::new("/bin/sh");
        command.args(["-c", body, "fastra-diff"]);
        command
    }

    #[test]
    fn fastra_prueft_protokoll_faehigkeiten_und_prozessfehler() {
        let valid = serde_json::json!({"protocol":1,"fileDiff":true,"readOnly":true,
            "focusDiff":true,"labels":true,"existingInstanceIpc":true});
        let mut cases = vec![(valid.clone(), true)];
        for key in [
            "fileDiff",
            "readOnly",
            "focusDiff",
            "labels",
            "existingInstanceIpc",
        ] {
            let mut invalid = valid.clone();
            invalid.as_object_mut().unwrap().remove(key);
            cases.push((invalid, false));
        }
        let mut future = valid.clone();
        future["protocol"] = 2.into();
        cases.push((future, false));
        for (value, expected) in cases {
            let helper = fake_fastra(&format!(
                "[ \"$#\" = 2 ] && [ \"$1\" = --capabilities ] && [ \"$2\" = --json ] || exit 2\nprintf '%s\\n' '{value}'"
            ));
            assert_eq!(check_fastra_capability_command(helper).is_ok(), expected);
        }
        for body in ["printf 'kein JSON'", "exit 4", "exec sleep 5"] {
            let helper = fake_fastra(body);
            assert!(check_fastra_capability_command(helper).is_err());
        }
        assert!(check_fastra_capabilities(Path::new("/nicht-vorhanden/fastra-diff")).is_err());
    }

    #[test]
    fn fastra_wertet_bestaetigung_und_alle_exitcodes_aus() {
        for (exit, expected) in [
            (0, None),
            (2, Some("invalid_input")),
            (3, Some("io")),
            (4, Some("unavailable")),
            (5, Some("unavailable")),
            (6, Some("app")),
            (7, Some("app")),
        ] {
            let helper = fake_fastra(&format!("exit {exit}"));
            // Status und Fristausgang getrennt prüfen: Der bisherige Vergleich
            // nur des App-Fehlercodes verlor bei einem sporadischen Fehlschlag
            // sowohl den tatsächlichen Kindstatus als auch einen Timeout.
            let started = std::time::Instant::now();
            let output = crate::system_mount_commands::command_output_with_timeout(
                helper,
                FASTRA_LAUNCH_TIMEOUT,
            )
            .unwrap_or_else(|error| panic!("Fastra-Testhelfer exit {exit}: {error}"))
            .unwrap_or_else(|| {
                panic!(
                    "Fastra-Testhelfer exit {exit}: Fristablauf nach {:?}",
                    started.elapsed()
                )
            });
            assert_eq!(
                output.status.code(),
                Some(exit),
                "Fastra-Testhelfer exit {exit}: Status {:?}, Laufzeit {:?}, stdout {:?}",
                output.status,
                started.elapsed(),
                output.stdout,
            );
            let result = fastra_exit(output.status.code());
            assert_eq!(result.err().map(|e| e.code), expected.map(String::from));
        }
        assert!(fastra_exit(None).unwrap_err().message.contains("bereits"));
    }

    #[test]
    fn haengender_fastra_start_endet_als_unbestaetigte_uebergabe() {
        // Die Shell bleibt der direkte Helfer; `sleep` erbt als Nachkomme stdout.
        // Der gemeinsame Timeout darf deshalb nicht auf das Pipe-EOF warten.
        let helper = fake_fastra("sleep 5; exit 0");
        let started = std::time::Instant::now();
        let error = launch_diff_with_timeout(
            DiffTool::Fastra,
            helper,
            std::time::Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
        assert_eq!(error.code, "app", "{}", error.message);
        assert!(error.message.contains("bereits"));
    }

    #[test]
    fn fastra_start_blockiert_nicht_an_grosser_helferausgabe() {
        // Mehr als der 64-KiB-Pipe-Puffer: Ohne parallelen Leser bliebe der Helfer
        // im `write` hängen und liefe erst mit der Frist in den Timeout.
        let helper = fake_fastra("head -c 300000 /dev/zero | tr '\\0' x; exit 0");
        let started = std::time::Instant::now();
        assert!(launch_diff_with_timeout(
            DiffTool::Fastra,
            helper,
            std::time::Duration::from_secs(5),
        )
        .is_ok());
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
    }

    #[test]
    fn fastra_uebergibt_labels_und_sonderzeichen_unveraendert() {
        let mut helper = fake_fastra("printf '%s\\n' \"$@\"");
        let left = Path::new("/tmp/--ä $(touch fremd).txt");
        let right = Path::new("/tmp/右; datei.txt");
        let (_, prefix) = diff_program(DiffTool::Fastra);
        let output = helper
            .args(diff_arguments(
                DiffTool::Fastra,
                prefix,
                left,
                right,
                "--links ä",
                "rechts 右",
            ))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(),
            "--read-only\n--focus-diff\n--left-label\n--links ä\n--right-label\nrechts 右\n--\n/tmp/--ä $(touch fremd).txt\n/tmp/右; datei.txt\n");
    }

    #[test]
    fn testsandbox_nimmt_auch_alle_momentaufnahmen_auf() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox_config = std::fs::canonicalize(temp.path()).unwrap();
        let normal = Path::new("/normaler-cache/doppeldecker");
        for leaf in ["diff-snapshots", "remote-snapshots"] {
            let actual = snapshot_cache_path(Some(&sandbox_config), normal, leaf).unwrap();
            assert_eq!(actual, sandbox_config.join("cache").join(leaf));
            assert_eq!(std::fs::canonicalize(&actual).unwrap(), actual);
        }
        assert_eq!(
            snapshot_cache_path(None, normal, "remote-snapshots").unwrap(),
            normal.join("remote-snapshots")
        );
    }

    #[test]
    fn testsandbox_cache_verfolgt_keine_symlinks_in_neuen_ebenen() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        for linked_cache in [true, false] {
            for leaf in ["diff-snapshots", "remote-snapshots"] {
                let temp = tempfile::tempdir().unwrap();
                let root = std::fs::canonicalize(temp.path()).unwrap();
                let config = root.join("app-config");
                let foreign = root.join("fremd");
                std::fs::create_dir(&config).unwrap();
                std::fs::create_dir(&foreign).unwrap();
                std::fs::set_permissions(&foreign, std::fs::Permissions::from_mode(0o755)).unwrap();
                std::fs::write(foreign.join("erhalten.txt"), b"fremde Daten").unwrap();
                let link = if linked_cache {
                    config.join("cache")
                } else {
                    std::fs::create_dir(config.join("cache")).unwrap();
                    config.join("cache").join(leaf)
                };
                symlink(&foreign, &link).unwrap();
                let error =
                    snapshot_cache_path(Some(&config), Path::new("/unused"), leaf).unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
                assert_eq!(
                    std::fs::read(foreign.join("erhalten.txt")).unwrap(),
                    b"fremde Daten"
                );
                assert_eq!(std::fs::read_dir(&foreign).unwrap().count(), 1);
                assert_eq!(
                    std::fs::metadata(&foreign).unwrap().permissions().mode() & 0o777,
                    0o755
                );
            }
        }
    }
}
