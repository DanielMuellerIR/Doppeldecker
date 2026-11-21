//! Optionale macOS-Einrichtung des mitgelieferten `doppeldecker-cli`.
//!
//! Das App-Bundle enthält das CLI direkt neben dem GUI-Binary. Auf ausdrückliche
//! Bestätigung legt die App nur einen Symlink unter `/usr/local/bin` an. Ein dort
//! bereits vorhandener Eintrag wird niemals ersetzt.

use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
#[cfg(target_os = "macos")]
use std::{fs, io, os::unix::fs::PermissionsExt, path::Path};

use crate::AppError;
#[cfg(any(test, target_os = "macos"))]
use crate::{AppState, MutexExt};

#[cfg(target_os = "macos")]
const CLI_NAME: &str = "doppeldecker-cli";
const DECLINED_MARKER: &str = "cli-install-declined";
#[cfg(target_os = "macos")]
const SYSTEM_CLI: &str = "/usr/local/bin/doppeldecker-cli";

// Einrichtung und Marker gelten für alle Fenster. Auch während eines offenen
// Administrator-Dialogs darf kein zweiter Worker den Zustand ändern.
static CLI_SETUP: Mutex<()> = Mutex::new(());
static CLI_INSTALL_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn cli_install_active() -> bool {
    CLI_INSTALL_ACTIVE.load(Ordering::Acquire)
}

#[cfg(any(test, target_os = "macos"))]
struct CliInstallGuard;

#[cfg(any(test, target_os = "macos"))]
impl CliInstallGuard {
    fn reserve(state: &AppState) -> Result<Self, AppError> {
        let _barrier = state.exit_barrier.lock_recover();
        if state.exit_in_progress.load(Ordering::Acquire)
            || CLI_INSTALL_ACTIVE.swap(true, Ordering::AcqRel)
        {
            return Err(AppError::new(
                "busy",
                "CLI-Einrichtung oder Beenden läuft bereits",
            ));
        }
        Ok(Self)
    }
}

#[cfg(any(test, target_os = "macos"))]
impl Drop for CliInstallGuard {
    fn drop(&mut self) {
        CLI_INSTALL_ACTIVE.store(false, Ordering::Release);
    }
}

async fn run_cli_setup<T: Send + 'static>(
    action: impl FnOnce() -> Result<T, AppError> + Send + 'static,
) -> Result<T, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = CLI_SETUP
            .lock()
            .map_err(|_| AppError::new("app", "Zustand der CLI-Einrichtung ist unklar"))?;
        action()
    })
    .await
    .map_err(|_| AppError::new("app", "Worker für die CLI-Einrichtung wurde unterbrochen"))?
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CliInstallState {
    #[cfg(target_os = "macos")]
    Installed,
    #[cfg(target_os = "macos")]
    Offer,
    #[cfg(target_os = "macos")]
    Declined,
    #[cfg(target_os = "macos")]
    Conflict,
    #[cfg(target_os = "macos")]
    OutsideApplications,
    Unavailable,
}

#[derive(Serialize)]
pub(crate) struct CliInstallStatus {
    state: CliInstallState,
}

#[cfg(test)]
#[test]
fn command_read_results_cli_install_status() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../tools/tests/command-read-results.json")).unwrap();
    let cases = [
        #[cfg(target_os = "macos")]
        (CliInstallState::Installed, "installed"),
        #[cfg(target_os = "macos")]
        (CliInstallState::Offer, "offer"),
        #[cfg(target_os = "macos")]
        (CliInstallState::Declined, "declined"),
        #[cfg(target_os = "macos")]
        (CliInstallState::Conflict, "conflict"),
        #[cfg(target_os = "macos")]
        (CliInstallState::OutsideApplications, "outside_applications"),
        (CliInstallState::Unavailable, "unavailable"),
    ];
    for (state, key) in cases {
        assert_eq!(
            serde_json::to_value(CliInstallStatus { state }).unwrap(),
            fixture["cliInstall"][key]
        );
    }
}

/// Meldet nur einen festen Zustand ans Frontend. Absolute Bundle- oder Nutzerpfade
/// verlassen das Backend nicht.
#[tauri::command]
pub(crate) async fn cli_install_status(
    app: tauri::AppHandle,
) -> Result<CliInstallStatus, AppError> {
    run_cli_setup(move || {
        Ok(CliInstallStatus {
            state: current_cli_install_state(&app)?,
        })
    })
    .await
}

/// Speichert die ausdrückliche Wahl „Nicht mehr fragen“ im normalen App-
/// Konfigurationsordner. GUI-Sandbox-Tests werden dadurch automatisch isoliert.
#[tauri::command]
pub(crate) async fn decline_cli_install(app: tauri::AppHandle) -> Result<(), AppError> {
    run_cli_setup(move || write_declined_marker(&app)).await
}

fn write_declined_marker(app: &tauri::AppHandle) -> Result<(), AppError> {
    let marker = declined_marker(app)?;
    let mut options = OpenOptions::new();
    options
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(marker).map_err(AppError::from)?;
    file.write_all(b"declined\n").map_err(AppError::from)?;
    file.flush().map_err(AppError::from)
}

/// Richtet den stabilen Terminalbefehl ein. Unter macOS übernimmt der native
/// Administrator-Dialog ausschließlich `mkdir` und eine nicht überschreibende
/// Symlinkanlage; Passwörter oder andere Secrets passieren diesen Code nie.
#[tauri::command]
pub(crate) async fn install_cli(app: tauri::AppHandle) -> Result<(), AppError> {
    #[cfg(target_os = "macos")]
    let reservation = {
        use tauri::Manager;
        CliInstallGuard::reserve(&app.state::<AppState>())?
    };
    run_cli_setup(move || {
        #[cfg(target_os = "macos")]
        let _reservation = reservation;
        install_cli_blocking(&app)
    })
    .await
}

fn install_cli_blocking(app: &tauri::AppHandle) -> Result<(), AppError> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err(AppError::new(
            "unsupported",
            "Die CLI-Einrichtung durch die App wird auf dieser Plattform nicht benötigt.",
        ))
    }

    #[cfg(target_os = "macos")]
    {
        let paths = cli_paths()?;
        match cli_install_state(&paths, declined_marker(app)?.is_file())? {
            CliInstallState::Installed => return Ok(()),
            CliInstallState::Offer => {}
            CliInstallState::Conflict => {
                return Err(AppError::new(
                    "already_exists",
                    "Unter /usr/local/bin liegt bereits ein anderer Eintrag namens doppeldecker-cli.",
                ));
            }
            _ => {
                return Err(AppError::new(
                    "invalid_input",
                    "Das CLI kann aus diesem App-Standort nicht eingerichtet werden.",
                ));
            }
        }

        // Der AppleScript-Quelltext ist fest. Der möglicherweise ungewöhnliche
        // Bundle-Pfad kommt ausschließlich als Umgebungswert hinein und wird vor
        // der Shellübergabe mit `quoted form of` maskiert.
        let status = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(install_applescript())
            .env("DOPPELDECKER_BUNDLE_CLI", &paths.bundle_cli)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(AppError::from)?;
        if !status.success() {
            return Err(AppError::new(
                "permission_denied",
                "Die CLI-Einrichtung wurde abgebrochen oder ist fehlgeschlagen.",
            ));
        }

        if cli_install_state(&paths, false)? != CliInstallState::Installed {
            return Err(AppError::new(
                "io",
                "Der Terminalbefehl wurde nicht korrekt eingerichtet.",
            ));
        }
        Ok(())
    }
}

fn declined_marker(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    Ok(crate::app_config_dir(app)?.join(DECLINED_MARKER))
}

#[cfg(target_os = "macos")]
struct CliPaths {
    app_bundle: PathBuf,
    bundle_cli: PathBuf,
    system_cli: PathBuf,
}

#[cfg(target_os = "macos")]
fn cli_paths() -> Result<CliPaths, AppError> {
    let executable = std::env::current_exe().map_err(AppError::from)?;
    let macos = executable
        .parent()
        .ok_or_else(|| AppError::new("invalid_data", "App-Binary hat keinen Elternordner"))?;
    let contents = macos
        .parent()
        .ok_or_else(|| AppError::new("invalid_data", "App-Bundle ist unvollständig"))?;
    let app_bundle = contents
        .parent()
        .ok_or_else(|| AppError::new("invalid_data", "App-Bundle ist unvollständig"))?
        .to_path_buf();
    Ok(CliPaths {
        bundle_cli: macos.join(CLI_NAME),
        app_bundle,
        system_cli: PathBuf::from(SYSTEM_CLI),
    })
}

fn current_cli_install_state(app: &tauri::AppHandle) -> Result<CliInstallState, AppError> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Ok(CliInstallState::Unavailable)
    }

    #[cfg(target_os = "macos")]
    {
        Ok(cli_install_state(
            &cli_paths()?,
            declined_marker(app)?.is_file(),
        )?)
    }
}

#[cfg(test)]
#[test]
fn cli_einrichtung_reserviert_bis_zum_ende_gegen_beenden() {
    let app = AppState::default();
    let reservation = CliInstallGuard::reserve(&app).unwrap();
    assert!(cli_install_active());
    assert!(CliInstallGuard::reserve(&app).is_err());
    drop(reservation);
    assert!(!cli_install_active());
    app.exit_in_progress.store(true, Ordering::Release);
    assert!(CliInstallGuard::reserve(&app).is_err());
    assert!(!cli_install_active());
}

#[cfg(test)]
#[test]
fn cli_einrichtung_haelt_das_ergebnis_bis_zum_worker_ende_zurueck() {
    use std::sync::mpsc::channel;
    use std::time::Duration;

    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let (result_tx, result_rx) = channel();
    let install = tauri::async_runtime::spawn(async move {
        let result = run_cli_setup(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Err::<(), _>(AppError::new("permission_denied", "Testabbruch"))
        })
        .await;
        result_tx.send(result.unwrap_err().code).unwrap();
    });
    let timeout = Duration::from_secs(5);
    started_rx.recv_timeout(timeout).unwrap();
    let (other_tx, other_rx) = channel();
    let other = tauri::async_runtime::spawn(async move { other_tx.send(()).unwrap() });
    other_rx.recv_timeout(timeout).unwrap();
    assert!(matches!(
        result_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    release_tx.send(()).unwrap();
    assert_eq!(
        result_rx.recv_timeout(timeout).unwrap(),
        "permission_denied"
    );
    tauri::async_runtime::block_on(async {
        install.await.unwrap();
        other.await.unwrap();
    });
}

#[cfg(target_os = "macos")]
fn cli_install_state(paths: &CliPaths, declined: bool) -> io::Result<CliInstallState> {
    if !is_regular_executable(&paths.bundle_cli)? {
        return Ok(CliInstallState::Unavailable);
    }
    if !is_stable_app_location(&paths.app_bundle) {
        return Ok(CliInstallState::OutsideApplications);
    }

    match fs::symlink_metadata(&paths.system_cli) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                && fs::read_link(&paths.system_cli)? == paths.bundle_cli
            {
                Ok(CliInstallState::Installed)
            } else {
                // Auch ein toter Symlink bleibt fremder Bestand. Die spätere
                // exklusive Anlage verweigert ebenfalls jede späte Kollision.
                Ok(CliInstallState::Conflict)
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if declined {
                Ok(CliInstallState::Declined)
            } else {
                Ok(CliInstallState::Offer)
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "macos")]
fn is_regular_executable(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            Ok(metadata.file_type().is_file() && metadata.permissions().mode() & 0o111 != 0)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "macos")]
fn is_stable_app_location(app_bundle: &Path) -> bool {
    if app_bundle.starts_with("/Applications") {
        return true;
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .is_some_and(|home| app_bundle.starts_with(home.join("Applications")))
}

#[cfg(target_os = "macos")]
fn install_javascript() -> &'static str {
    // `ln -s` behandelt einen spät erschienenen Ordner als Zielverzeichnis und
    // schreibt hinein. Foundation legt ausschließlich genau den benannten Link
    // an und verweigert auch Ordner und Ordner-Symlinks als Kollision.
    "ObjC.import('Foundation'); function run(args) { const error = Ref(); if (!$.NSFileManager.defaultManager.createSymbolicLinkAtPathWithDestinationPathError(args[1], args[0], error)) throw Error('Symlink konnte nicht exklusiv angelegt werden'); }"
}

#[cfg(target_os = "macos")]
fn install_applescript() -> String {
    let script = install_javascript()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!(
        r#"set bundleCli to system attribute "DOPPELDECKER_BUNDLE_CLI"
set systemCli to "/usr/local/bin/doppeldecker-cli"
set linkScript to "{script}"
set commandText to "/bin/mkdir -p /usr/local/bin && /usr/bin/osascript -l JavaScript -e " & quoted form of linkScript & " -- " & quoted form of bundleCli & " " & quoted form of systemCli
do shell script commandText with administrator privileges"#
    )
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{cli_install_state, install_applescript, CliInstallState, CliPaths, CLI_NAME};
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn paths(root: &std::path::Path) -> CliPaths {
        let app_bundle = root.join("Applications/Doppeldecker.app");
        CliPaths {
            bundle_cli: app_bundle.join("Contents/MacOS").join(CLI_NAME),
            app_bundle,
            system_cli: root.join("usr/local/bin").join(CLI_NAME),
        }
    }

    fn prepare_bundle(paths: &CliPaths) {
        std::fs::create_dir_all(paths.bundle_cli.parent().unwrap()).unwrap();
        std::fs::write(&paths.bundle_cli, b"test-cli").unwrap();
        let mut permissions = std::fs::metadata(&paths.bundle_cli).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&paths.bundle_cli, permissions).unwrap();
    }

    #[test]
    fn bietet_nur_fehlenden_eintrag_an_und_respektiert_ablehnung() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        prepare_bundle(&paths);
        assert_eq!(
            cli_install_state(&paths, false).unwrap(),
            CliInstallState::OutsideApplications
        );

        // Für die Zustandslogik darf der Testroot wie ein stabiler Programme-
        // Ordner behandelt werden, ohne das echte /Applications zu verändern.
        let paths = CliPaths {
            app_bundle: std::path::PathBuf::from("/Applications/Doppeldecker.app"),
            bundle_cli: paths.bundle_cli,
            system_cli: paths.system_cli,
        };
        assert_eq!(
            cli_install_state(&paths, false).unwrap(),
            CliInstallState::Offer
        );
        assert_eq!(
            cli_install_state(&paths, true).unwrap(),
            CliInstallState::Declined
        );
    }

    #[test]
    fn erkennt_eigenen_symlink_und_fasst_fremden_nie_an() {
        let temp = tempfile::tempdir().unwrap();
        let mut paths = paths(temp.path());
        prepare_bundle(&paths);
        paths.app_bundle = std::path::PathBuf::from("/Applications/Doppeldecker.app");
        std::fs::create_dir_all(paths.system_cli.parent().unwrap()).unwrap();

        symlink(&paths.bundle_cli, &paths.system_cli).unwrap();
        assert_eq!(
            cli_install_state(&paths, false).unwrap(),
            CliInstallState::Installed
        );
        std::fs::remove_file(&paths.system_cli).unwrap();
        symlink("/anderer/ort/doppeldecker-cli", &paths.system_cli).unwrap();
        assert_eq!(
            cli_install_state(&paths, false).unwrap(),
            CliInstallState::Conflict
        );
    }

    #[test]
    fn cli_einrichtung_behandelt_vorhandene_ordner_nicht_als_linkzielverzeichnis() {
        let mut failures = Vec::new();
        for kind in [
            "missing",
            "file",
            "directory",
            "directory_link",
            "dangling_link",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let bin = root.join("bin");
            std::fs::create_dir(&bin).unwrap();
            let source = root.join("bundle-cli mit ' und \" und $()");
            std::fs::write(&source, b"source").unwrap();
            let target = bin.join("doppeldecker-cli");
            let foreign = root.join("foreign");
            std::fs::create_dir(&foreign).unwrap();
            match kind {
                "file" => std::fs::write(&target, b"foreign").unwrap(),
                "directory" => std::fs::create_dir(&target).unwrap(),
                "directory_link" => symlink(&foreign, &target).unwrap(),
                "dangling_link" => symlink(root.join("missing"), &target).unwrap(),
                _ => {}
            }
            // Derselbe Produktionsablauf, ausschließlich in künstlichen Pfaden und
            // ohne Administratorrechte. Die beiden Sperrprüfungen verhindern einen
            // versehentlichen echten Installationsaufruf bei späterem Skriptumbau.
            let script = install_applescript()
                .replace("/usr/local/bin", bin.to_str().unwrap())
                .replace(" with administrator privileges", "");
            assert!(!script.contains("/usr/local/bin"));
            assert!(!script.contains("administrator privileges"));
            let status = std::process::Command::new("/usr/bin/osascript")
                .args(["-e", &script])
                .env("DOPPELDECKER_BUNDLE_CLI", &source)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            if status.success() != (kind == "missing") {
                failures.push(format!("{kind}: unerwarteter Erfolg={} ", status.success()));
            }
            if kind == "missing" {
                assert_eq!(std::fs::read_link(&target).unwrap(), source);
            } else if kind == "file" {
                assert_eq!(std::fs::read(&target).unwrap(), b"foreign");
            }
            if kind == "directory" && std::fs::read_dir(&target).unwrap().count() != 0 {
                failures.push("directory: fremder Ordner verändert".to_owned());
            }
            if std::fs::read_dir(&foreign).unwrap().count() != 0 {
                failures.push("directory_link: fremder Linkordner verändert".to_owned());
            }
            if kind == "directory_link" {
                assert_eq!(std::fs::read_link(&target).unwrap(), foreign);
            }
            if kind == "dangling_link" {
                assert_eq!(std::fs::read_link(&target).unwrap(), root.join("missing"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("; "));
    }

    #[test]
    fn applescript_maskiert_den_bundlepfad_als_daten() {
        let source = install_applescript();
        assert!(source.contains("system attribute \"DOPPELDECKER_BUNDLE_CLI\""));
        assert!(source.contains("quoted form of bundleCli"));
        assert!(!source.contains("ln -sfn"));
        assert!(!source.contains("ln -sf "));
    }
}
