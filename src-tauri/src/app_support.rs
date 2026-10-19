use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use doppeldecker_core::validate_test_sandbox_root;
use tauri::Manager;

/// Name der Umgebungsvariable, die den Wegwerf-Sandbox-Ordner festlegt. Ist sie NICHT
/// gesetzt, bleibt der normale lokale AddOnly-/Update-Start separat gebunden;
/// Mirror benötigt den Realmodus-Assistenten. Eine gesetzte Sandbox begrenzt alle Tests.
const SANDBOX_ENV: &str = "DOPPELDECKER_SANDBOX";
const ERROR_LOG_FILE: &str = "errors.jsonl";
const ERROR_LOG_PREVIOUS: &str = "errors.previous.jsonl";
const MAX_ERROR_LOG_BYTES: u64 = 1024 * 1024;
static ERROR_LOG_LOCK: Mutex<()> = Mutex::new(());

/// Zusätzliche Sicherungsautomaten; sie sind bewusst hart und nicht per UI wegklickbar.
pub(crate) const MAX_DELETE_ENTRIES: u64 = 100;
pub(crate) const MAX_DELETE_FRACTION: f64 = 0.25;

/// Strukturierter Command-Fehler Richtung Frontend: ein stabiler Maschinencode
/// (`plan_drift`, `sandbox_violation`, `mirror_blocked`, `confirmation_mismatch`,
/// `capability_probe_required`, `busy`, `io`, `app`, …) plus der Anzeigetext und
/// optionale strukturierte Prüfpfade. Das Frontend zeigt `message` an und kann
/// auf `code` reagieren, statt fragil auf deutsche Fehlertexte zu matchen.
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct AppError {
    pub(crate) code: String,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) recovery_paths: Vec<String>,
}

impl AppError {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        AppError {
            code: code.into(),
            message: message.into(),
            recovery_paths: Vec::new(),
        }
    }
}

/// Kern-Fehler tragen ihren Code selbst (siehe `doppeldecker_core::error`);
/// gewöhnliche I/O-Fehler werden nach ihrer Fehlerart eingeordnet.
impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        AppError {
            code: doppeldecker_core::error_code(&error).as_str().into(),
            message: error.to_string(),
            recovery_paths: doppeldecker_core::error::recovery_paths(&error),
        }
    }
}

/// App-eigene Textfehler ohne speziellen Code (Übergangsform; wo das Frontend
/// reagieren soll, bekommt die Stelle einen expliziten Code über `AppError::new`).
impl From<String> for AppError {
    fn from(message: String) -> Self {
        AppError::new("app", message)
    }
}

impl From<&str> for AppError {
    fn from(message: &str) -> Self {
        AppError::new("app", message)
    }
}

/// Tauri-Rahmenfehler (Pfadauflösung, Fensterzugriff …) haben keinen Domänencode.
impl From<tauri::Error> for AppError {
    fn from(error: tauri::Error) -> Self {
        AppError::new("app", error.to_string())
    }
}

/// Ermittelt und validiert die aktuelle Sandbox-Wurzel. Eine ungültige gesetzte
/// Sandbox ist ein harter Fehler — sie darf niemals still wie „keine Sandbox" behandelt
/// werden, weil interaktive Dateioperationen sonst unbemerkt unbeschränkt liefen.
pub(crate) fn sandbox_root() -> Result<Option<PathBuf>, AppError> {
    match std::env::var(SANDBOX_ENV) {
        Ok(raw) => validate_sandbox_root(&raw).map(Some),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(AppError::new(
            "invalid_input",
            "Die gesetzte Testsandbox ist nicht als UTF-8 darstellbar",
        )),
    }
}

/// Einen konkret angegebenen Testbereich validieren. Die Trennung von der
/// Umgebungsvariable macht die gefährlichen Pfadfälle ohne Prozess-globales Env-Rennen
/// headless testbar.
pub(crate) fn validate_sandbox_root(raw: &str) -> Result<PathBuf, AppError> {
    validate_test_sandbox_root(Path::new(raw)).map_err(AppError::from)
}

/// Liefert für einen markierten Testlauf ausschließlich einen Konfigurationsordner
/// innerhalb derselben Sandbox. So verändern Queue, Favoriten, Profile, Audit und
/// Recovery bei GUI-Tests niemals die normale Nutzerkonfiguration.
pub(crate) fn app_config_dir(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    if let Some(root) = sandbox_root()? {
        return prepare_isolated_config_dir(&root).map_err(AppError::from);
    }
    app.path().app_config_dir().map_err(AppError::from)
}

fn prepare_isolated_config_dir(sandbox: &Path) -> io::Result<PathBuf> {
    // Produktiv kommt hier bereits die validierte kanonische Sandbox an. Die
    // erneute Bindung hält den kleinen Helfer auch in Tests und bei späteren
    // direkten Aufrufern fail-closed.
    let sandbox = std::fs::canonicalize(sandbox)?;
    prepare_direct_directory(&sandbox, "app-config")
}

pub(crate) fn prepare_direct_directory(parent: &Path, name: &str) -> io::Result<PathBuf> {
    let isolated = parent.join(name);
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(&isolated) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    // `create_dir_all` akzeptiert einen bereits vorhandenen Symlink auf einen
    // Ordner. In der markierten Testsandbox würde dann jede Queue-, Profil- und
    // Auditdatei außerhalb des Wegwerfbaums landen. Die Sandbox-Wurzel ist bereits
    // kanonisch validiert; ihr einzelnes festes Kind muss daher ein echter Ordner
    // sein und auch nach der Auflösung genau denselben Pfad bezeichnen.
    let metadata = std::fs::symlink_metadata(&isolated)?;
    if !metadata.file_type().is_dir() || std::fs::canonicalize(&isolated)? != isolated {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Sandbox-Konfigurationsordner ist kein direktes reguläres Verzeichnis",
        ));
    }
    Ok(isolated)
}

/// Schreibt nur Fehlerklasse und UI-Kontext, niemals den eigentlichen Fehlertext.
/// Providerantworten können Pfade oder Serverdetails enthalten; diese Werte bleiben
/// deshalb ausschließlich in der sichtbaren Fehlermeldung und landen nicht im Log.
pub(crate) fn record_app_error(
    app: &tauri::AppHandle,
    context: &str,
    code: Option<&str>,
) -> Result<(), AppError> {
    let directory = if let Some(root) = sandbox_root()? {
        let config = prepare_isolated_config_dir(&root).map_err(AppError::from)?;
        prepare_direct_directory(&config, "logs").map_err(AppError::from)?
    } else {
        let directory = app.path().app_log_dir().map_err(AppError::from)?;
        std::fs::create_dir_all(&directory).map_err(AppError::from)?;
        directory
    };
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AppError::new("app", "Systemzeit liegt vor der Unix-Epoche"))?
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    append_safe_error_record(&directory.join(ERROR_LOG_FILE), unix_ms, context, code)
        .map_err(AppError::from)
}

fn append_safe_error_record(
    path: &Path,
    unix_ms: u64,
    context: &str,
    code: Option<&str>,
) -> io::Result<()> {
    append_error_record(path, unix_ms, &safe_context(context), &safe_code(code))
}

fn safe_context(raw: &str) -> String {
    // Kein frei formulierter Frontendwert darf ins Log gelangen. Die kleine
    // Positivliste hält den Diagnosewert hoch, ohne Profilnamen, Pfade oder andere
    // versehentlich als Label verwendete Nutzerdaten zu persistieren.
    match raw {
        "Lokales-Listing"
        | "SFTP-Listing"
        | "FTP-Listing"
        | "FTPS-Listing"
        | "WEBDAV-Listing"
        | "SMB-Listing"
        | "SFTP-Verbindung"
        | "FTP-Verbindung"
        | "FTPS-Verbindung"
        | "WebDAV-Verbindung"
        | "SMB-Verbindung"
        | "SFTP-Profil"
        | "FTP-Profil"
        | "FTP-/FTPS-Profil"
        | "FTPS-Profil"
        | "WebDAV-Profil"
        | "SMB-Profil"
        | "Hostkey-Ermittlung"
        | "FTPS-Zertifikat"
        | "WebDAV-Zertifikat"
        | "SMB-Identität"
        | "Schlüsselbund"
        | "Start-Fehler"
        | "Geräte und Freigaben"
        | "LAN-Suche"
        | "Automatische Ordnergrößen"
        | "Ordnergröße"
        | "Ordner anlegen"
        | "Queue-Fehler"
        | "Quarantäne-Fehler"
        | "Remote-Löschen"
        | "Öffnen fehlgeschlagen"
        | "Realmodus-Fehler"
        | "Lokaler Sync"
        | "Bidirektionaler Sync"
        | "Bidirektionale Queue"
        | "Bidirektionalen Abschluss prüfen"
        | "Bidirektionaler Start"
        | "Bidirektionaler Sync gesperrt"
        | "Bidirektionales Recovery"
        | "Bidirektionales Recovery gesperrt"
        | "Beenden"
        | "CLI-Einrichtung"
        | "Dateivergleich"
        | "Datei-Zwischenablage"
        | "Einstellungen"
        | "Projektinformationen"
        | "Favorit öffnen"
        | "Fehler"
        | "Fenstericon"
        | "Finder-Favoriten"
        | "Kopieren"
        | "Leeren fehlgeschlagen"
        | "Neues Fenster"
        | "Profilstatus abschließen"
        | "Quarantäne-Status konnte nicht neu geladen werden"
        | "Realmodus gesperrt"
        | "Recovery-Vorbereitung verwerfen"
        | "Remote gesperrt"
        | "Remote-Pane"
        | "SFTP-Profilprüfung"
        | "Sichere Passworteingabe"
        | "Sitzungsstand"
        | "Tab öffnen"
        | "Update"
        | "Verbindung gespeichert, Pane-Rückfall fehlgeschlagen"
        | "Verbindungsstatus aktualisieren"
        | "Wiederherstellung fehlgeschlagen" => raw.to_owned(),
        _ => "App-Fehler".to_owned(),
    }
}

fn safe_code(raw: Option<&str>) -> String {
    let raw = raw.unwrap_or("app");
    // Core und Tauri dürfen nicht zwei von Hand gepflegte Listen derselben
    // Fehlercodes besitzen: Beim Ergänzen von `replan_required` und
    // `quarantine_busy` war genau diese Kopie veraltet. Serde prüft hier gegen
    // die typisierte Core-Aufzählung samt ihrer stabilen snake_case-Namen; freie
    // Frontendtexte bleiben dadurch weiterhin ausgeschlossen.
    if serde_json::from_value::<doppeldecker_core::ErrorCode>(serde_json::Value::String(
        raw.to_owned(),
    ))
    .is_ok()
    {
        return raw.to_owned();
    }
    match raw {
        "update_failed"
        | "keyring"
        | "busy"
        | "queue_confirmation_required"
        // Eigene Codes der App-Schicht: Fenster- und Bildfehler beim Iconwechsel,
        // und der Fall, dass eine AppKit-Closure nicht auf dem Hauptthread läuft.
        | "window"
        | "image"
        | "main_thread"
        | "app" => raw.to_owned(),
        _ => "app".to_owned(),
    }
}

#[derive(serde::Serialize)]
struct ErrorLogRecord<'value> {
    unix_ms: u64,
    context: &'value str,
    code: &'value str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

fn checked_log_metadata(path: &Path) -> io::Result<Option<(FileIdentity, u64)>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // Ein Hardlink auf eine andere Nutzerdatei ist ebenfalls eine reguläre Datei.
    // Ohne die Linkzahlprüfung würde der Diagnose-Append deren Inhalt verändern.
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Fehlerlogpfad ist keine alleinstehende reguläre Datei",
        ));
    }
    Ok(Some((
        FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        metadata.len(),
    )))
}

fn open_checked_log(path: &Path, expected: Option<FileIdentity>) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    if expected.is_some() {
        // Eine vorhandene Datei darf zwischen Prüfung und Öffnen nicht still als
        // Neuanlage ersetzt werden.
        options.create(false);
    } else {
        // Bei einer Neuanlage schließt `create_new` das umgekehrte Rennen: Ein
        // inzwischen eingesetzter Hardlink wird nie geöffnet.
        options.create_new(true);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    let actual = FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || expected.is_some_and(|identity| identity != actual)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Fehlerlogdatei änderte ihre Identität vor dem Öffnen",
        ));
    }
    Ok(file)
}

fn append_error_record(path: &Path, unix_ms: u64, context: &str, code: &str) -> io::Result<()> {
    let _guard = ERROR_LOG_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut current = checked_log_metadata(path)?;
    if let Some((identity, len)) = current {
        if len >= MAX_ERROR_LOG_BYTES {
            let previous = path.with_file_name(ERROR_LOG_PREVIOUS);
            if checked_log_metadata(&previous)?.is_some() {
                std::fs::remove_file(&previous)?;
            }
            std::fs::rename(path, &previous)?;
            let moved = checked_log_metadata(&previous)?
                .ok_or_else(|| io::Error::other("Rotiertes Fehlerlog fehlt"))?
                .0;
            if moved != identity {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Fehlerlogdatei änderte ihre Identität vor der Rotation",
                ));
            }
            current = None;
        } else {
            current = Some((identity, len));
        }
    }
    let mut file = open_checked_log(path, current.map(|(identity, _)| identity))?;
    serde_json::to_writer(
        &mut file,
        &ErrorLogRecord {
            unix_ms,
            context,
            code,
        },
    )
    .map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::{
        append_error_record, append_safe_error_record, prepare_direct_directory,
        prepare_isolated_config_dir, safe_code, safe_context, AppError, ERROR_LOG_PREVIOUS,
        MAX_ERROR_LOG_BYTES,
    };
    use std::os::unix::fs::symlink;

    #[test]
    fn sandbox_konfiguration_folgt_keinem_vorhandenen_symlink() {
        let sandbox = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), sandbox.path().join("app-config")).unwrap();

        let error = prepare_isolated_config_dir(sandbox.path())
            .expect_err("Ein app-config-Symlink darf die Testsandbox nicht verlassen");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn sandbox_logordner_folgt_keinem_vorhandenen_symlink() {
        let sandbox = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let config = prepare_isolated_config_dir(sandbox.path()).unwrap();
        symlink(outside.path(), config.join("logs")).unwrap();

        let error = prepare_direct_directory(&config, "logs")
            .expect_err("Ein Log-Symlink darf die Testsandbox nicht verlassen");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn fehlerlog_enthaelt_nur_feste_metadaten() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("errors.jsonl");
        append_error_record(&path, 123, "FTPS-Verbindung", "io").unwrap();
        let record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(record["unix_ms"], 123);
        assert_eq!(record["context"], "FTPS-Verbindung");
        assert_eq!(record["code"], "io");
        assert_eq!(record.as_object().unwrap().len(), 3);
    }

    #[test]
    fn queue_bestaetigung_bleibt_im_normalisierten_fehlerlog_erhalten() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("errors.jsonl");

        append_safe_error_record(
            &path,
            124,
            "Queue-Fehler",
            Some("queue_confirmation_required"),
        )
        .unwrap();

        let record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(record["context"], "Queue-Fehler");
        assert_eq!(record["code"], "queue_confirmation_required");
    }

    #[test]
    fn fehlerlog_beschreibt_keine_hardlink_datei() {
        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("fremde-datei");
        let log = temp.path().join("errors.jsonl");
        std::fs::write(&victim, b"unveraendert").unwrap();
        std::fs::hard_link(&victim, &log).unwrap();

        let error = append_error_record(&log, 123, "Queue-Fehler", "io")
            .expect_err("Ein Fehlerlog darf keine mehrfach verlinkte Datei beschreiben");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(victim).unwrap(), b"unveraendert");
    }

    #[test]
    fn volles_fehlerlog_rotiert_identitaetsgebunden() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("errors.jsonl");
        std::fs::write(&log, vec![b'x'; MAX_ERROR_LOG_BYTES as usize]).unwrap();

        append_error_record(&log, 456, "Queue-Fehler", "io").unwrap();

        let previous = temp.path().join(ERROR_LOG_PREVIOUS);
        assert_eq!(
            std::fs::metadata(previous).unwrap().len(),
            MAX_ERROR_LOG_BYTES
        );
        let record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(log).unwrap()).unwrap();
        assert_eq!(record["unix_ms"], 456);
        assert_eq!(record["context"], "Queue-Fehler");
        assert_eq!(record["code"], "io");
    }

    #[test]
    fn freie_logwerte_werden_nicht_uebernommen() {
        assert_eq!(safe_context("Pfad:/vertraulich"), "App-Fehler");
        assert_eq!(safe_context("Unbekannt aber harmlos"), "App-Fehler");
        assert_eq!(safe_code(Some("io\nvertraulich")), "app");
        assert_eq!(safe_code(Some("vertraulich")), "app");
        assert_eq!(
            safe_code(Some("queue_confirmation_required")),
            "queue_confirmation_required"
        );
        assert_eq!(safe_context("Geräte und Freigaben"), "Geräte und Freigaben");
        assert_eq!(safe_context("LAN-Suche"), "LAN-Suche");
        assert_eq!(safe_context("Start-Fehler"), "Start-Fehler");
        // Die Positivliste darf keinen stabilen Core-Code verlieren. Sonst zeigt
        // die UI zwar den richtigen Reaktionsgrund, das sichere Diagnoselog stuft
        // denselben Fehler aber unbemerkt zum unspezifischen `app` herab.
        for code in [
            doppeldecker_core::ErrorCode::PlanDrift,
            doppeldecker_core::ErrorCode::ReplanRequired,
            doppeldecker_core::ErrorCode::SandboxViolation,
            doppeldecker_core::ErrorCode::MirrorBlocked,
            doppeldecker_core::ErrorCode::ConfirmationMismatch,
            doppeldecker_core::ErrorCode::InvalidInput,
            doppeldecker_core::ErrorCode::InvalidData,
            doppeldecker_core::ErrorCode::NotFound,
            doppeldecker_core::ErrorCode::AlreadyExists,
            doppeldecker_core::ErrorCode::Unsupported,
            doppeldecker_core::ErrorCode::CapabilityProbeRequired,
            doppeldecker_core::ErrorCode::PermissionDenied,
            doppeldecker_core::ErrorCode::StorageFull,
            doppeldecker_core::ErrorCode::Interrupted,
            doppeldecker_core::ErrorCode::QuarantineBusy,
            doppeldecker_core::ErrorCode::ConnectionLost,
            doppeldecker_core::ErrorCode::OutcomeUnclear,
            doppeldecker_core::ErrorCode::TlsIdentityUnverified,
            doppeldecker_core::ErrorCode::Io,
        ] {
            assert_eq!(safe_code(Some(code.as_str())), code.as_str());
        }
    }

    /// Regression: Die Positivliste in `safe_context` verfällt still. Sie war auf
    /// rund zwanzig Labels stehengeblieben, während das Frontend inzwischen über
    /// vierzig verschiedene benutzt — der Rest landete undifferenziert als
    /// „App-Fehler" im Log, also genau dort, wo man später nachsieht, warum etwas
    /// schiefging. Der Test gleicht die Liste mit den tatsächlich im Quelltext
    /// verwendeten Labels ab.
    ///
    /// Erfasst werden nur statische Literale. Ein Label aus einer Variablen bleibt
    /// bewusst außen vor und fällt zur Laufzeit weiter auf „App-Fehler" — das ist
    /// der eigentliche Zweck der Schranke: Kein Profilname, kein Pfad, kein anderer
    /// Nutzerwert darf über ein Label ins Log geraten.
    fn call_arguments(source: &str) -> Option<Vec<&str>> {
        let mut arguments = Vec::new();
        let mut start = 0;
        let mut depth = 0_u32;
        let mut quote = None;
        let mut escaped = false;
        for (index, character) in source.char_indices() {
            if let Some(delimiter) = quote {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == delimiter {
                    quote = None;
                }
                continue;
            }
            match character {
                '"' | '\'' | '`' => quote = Some(character),
                '(' | '[' | '{' => depth += 1,
                ')' if depth == 0 => {
                    arguments.push(&source[start..index]);
                    return Some(arguments);
                }
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    arguments.push(&source[start..index]);
                    start = index + character.len_utf8();
                }
                _ => {}
            }
        }
        None
    }

    fn static_string_argument(argument: &str) -> Option<&str> {
        let argument = argument.trim();
        let remainder = argument.strip_prefix('"')?;
        let mut escaped = false;
        for (index, character) in remainder.char_indices() {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                return remainder[index + 1..]
                    .trim()
                    .is_empty()
                    .then_some(&remainder[..index]);
            }
        }
        None
    }

    #[test]
    fn jedes_benutzte_fehlerlabel_steht_in_der_positivliste() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("Projektwurzel");
        let mut gepruefte = 0;
        // Zusammengesetzt, damit dieser Test seinen eigenen Quelltext nicht trifft.
        for (verzeichnis, endung, aufruf, label_index) in [
            ("frontend", "js", concat!("showError", "("), 0),
            ("src-tauri/src", "rs", concat!("record_app_", "error("), 1),
        ] {
            let eintraege = std::fs::read_dir(root.join(verzeichnis)).expect("Verzeichnis");
            for eintrag in eintraege.flatten() {
                let pfad = eintrag.path();
                if pfad.extension().and_then(|e| e.to_str()) != Some(endung) {
                    continue;
                }
                let quelle = std::fs::read_to_string(&pfad).expect("Datei lesbar");
                for teil in quelle.split(aufruf).skip(1) {
                    // Nur das tatsächliche Label-Argument dieses Aufrufs ansehen.
                    // Verschachtelte Fehlermeldungen dürfen nicht versehentlich als
                    // Label gelten; dynamische Labels bleiben der Laufzeitschranke
                    // überlassen.
                    let Some(argumente) = call_arguments(teil) else {
                        continue;
                    };
                    let Some(label) = argumente
                        .get(label_index)
                        .and_then(|argument| static_string_argument(argument))
                    else {
                        continue;
                    };
                    if label.is_empty() {
                        continue;
                    }
                    assert_eq!(
                        safe_context(label),
                        label,
                        "{} benutzt das Fehlerlabel „{label}“, das safe_context nicht kennt; \
                         es landet als App-Fehler im Log",
                        pfad.display()
                    );
                    gepruefte += 1;
                }
            }
        }
        assert!(
            gepruefte >= 40,
            "nur {gepruefte} Labels gefunden; der Scanner greift offenbar nicht mehr"
        );
    }

    #[test]
    fn fehlerlabel_scanner_verwechselt_keine_verschachtelte_meldung_mit_dem_label() {
        let arguments =
            call_arguments("`${label} gesperrt`, new Error(\"interne Meldung\")) weiterer Text")
                .unwrap();
        assert_eq!(arguments.len(), 2);
        assert!(static_string_argument(arguments[0]).is_none());
        assert_eq!(static_string_argument(arguments[1]), None);

        let arguments = call_arguments("app, \"Fenstericon\", &error) Rest").unwrap();
        assert_eq!(static_string_argument(arguments[1]), Some("Fenstericon"));
    }

    #[test]
    fn gesetzte_ungueltige_sandboxvariable_bleibt_im_echten_prozess_ein_fehler() {
        const MODE: &str = "DOPPELDECKER_TEST_SANDBOX_ENV_MODE";
        if let Ok(mode) = std::env::var(MODE) {
            match mode.as_str() {
                "absent" => assert!(super::sandbox_root().unwrap().is_none()),
                "valid" => assert!(super::sandbox_root().unwrap().is_some()),
                "invalid" => {
                    assert!(super::sandbox_root().is_err(),
                    "Eine gesetzte ungültige Sandbox wurde als unbegrenzter Normalmodus behandelt")
                }
                _ => panic!("Unbekannter künstlicher Sandboxfall"),
            }
            return;
        }
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(doppeldecker_core::TEST_SANDBOX_MARKER),
            doppeldecker_core::TEST_SANDBOX_MARKER_CONTENT,
        )
        .unwrap();
        let exe = std::env::current_exe().unwrap();
        let test = "app_support::tests::gesetzte_ungueltige_sandboxvariable_bleibt_im_echten_prozess_ein_fehler";
        let list = std::process::Command::new(&exe)
            .args([test, "--list"])
            .env_remove(MODE)
            .output()
            .unwrap();
        assert!(list.status.success());
        assert_eq!(
            String::from_utf8(list.stdout)
                .unwrap()
                .lines()
                .filter(|line| *line == format!("{test}: test"))
                .count(),
            1
        );
        let cases = [
            ("absent", None, "absent"),
            ("valid", Some(root.path().as_os_str().to_owned()), "valid"),
            ("empty", Some(std::ffi::OsString::new()), "invalid"),
            ("whitespace", Some(" \t".into()), "invalid"),
            (
                "non-utf8",
                Some(std::ffi::OsString::from_vec(vec![0xff])),
                "invalid",
            ),
        ];
        let mut failed = Vec::new();
        for (name, raw, expected) in cases {
            let mut command = std::process::Command::new(&exe);
            command
                .args([test, "--exact", "--nocapture"])
                .env(MODE, expected)
                .env_remove(super::SANDBOX_ENV);
            if let Some(raw) = raw {
                command.env(super::SANDBOX_ENV, raw);
            }
            let output = command.output().unwrap();
            if !output.status.success() {
                failed.push(name);
            }
        }
        assert!(
            failed.is_empty(),
            "Sandbox-Vertrag im echten Kindprozess verletzt: {failed:?}"
        );
    }

    #[test]
    fn capability_probe_code_erreicht_den_tauri_fehler() {
        let error = AppError::from(doppeldecker_core::capability_probe_required(
            "Capability-Probe fehlt",
        ));
        assert_eq!(error.code, "capability_probe_required");
        assert_eq!(error.message, "Capability-Probe fehlt");
    }

    #[test]
    fn echter_core_cleanupfehler_erhaelt_pruefpfad_im_tauri_payload() {
        let area = tempfile::tempdir().unwrap();
        let retained = area.path().join("pruefbestand");
        std::fs::create_dir(&retained).unwrap();
        let file = retained.join("behalten.txt");
        std::fs::write(&file, b"behalten").unwrap();
        let probe = std::fs::read(&file).map(|_| ());
        let cleanup = std::fs::remove_dir(&retained);
        assert!(cleanup.is_err());
        let error = doppeldecker_core::remote::combine_probe_outcome(
            "Test",
            probe,
            cleanup,
            retained.to_str().unwrap(),
        )
        .unwrap_err();
        let expected_code = doppeldecker_core::error_code(&error).as_str();

        let payload = tauri::ipc::InvokeError::from(AppError::from(error)).0;

        assert_eq!(payload["code"], expected_code);
        assert_eq!(
            payload["recovery_paths"],
            serde_json::json!([retained.to_str().unwrap()])
        );
        assert_eq!(std::fs::read(file).unwrap(), b"behalten");
        assert_eq!(
            tauri::ipc::InvokeError::from(AppError::new("invalid_input", "Textfehler")).0,
            serde_json::json!({"code":"invalid_input", "message":"Textfehler"}),
        );
    }
}
