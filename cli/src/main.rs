//! Headless-Kommandozeile für denselben Sicherheitsvertrag wie die Desktop-App.
//!
//! Es existiert bewusst kein `--force` und kein serialisierbares Permit. Ein realer
//! Lauf bleibt im selben Prozess, hält seine Deskriptoren offen und liest die zufällige
//! Phrase sowie bei Update/Mirror die Backup-Bestätigung über stdin.

mod connection_import;

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};
use connection_import::{ImportProtocol, ResolvedConnection};
use doppeldecker_core::{
    capture_file_conflict_proofs, discover_ftps_certificate, discover_sftp_host_key,
    execute_provider_delete_queue_entry, expand_home_path, find_recursive_conflicts,
    load_ftps_profiles, load_sftp_profiles, load_smb_profiles, load_webdav_profiles,
    plan_remote_delete, recover_trash_journals, save_ftps_profile, save_sftp_profile,
    sftp_host_key_is_known, trash_items, BidirectionalQueueEndpoint, Cancel, Compare,
    ConflictResolution, ErrorCode, FileQueueKind, FtpTransportSecurity, FtpsCertificatePolicy,
    FtpsConnectionConfig, FtpsProfile, FtpsVfs, KeychainSecretReference, LocalQueueExecution,
    LocalVfs, MetadataProfile, OperationQueue, PairId, Progress, ProviderEndpoint, QuarantineScope,
    QueueExecutionResult, QueueOperation, RealRunPreview, RemoteDeleteMode, SecretResolver,
    SecretString, SftpAuthenticationReference, SftpConnectionConfig, SftpProfile, SftpReadSecurity,
    SftpVfs, SyncMode, SyncOptions, SyncStats, Vfs,
};
use zeroize::Zeroizing;

const BACKUP_CONFIRMATION: &str = "BACKUP UND RESTORE BESTÄTIGT";

/// Umgebungsvariable, die jeden stillen Rückfall auf die echten Nutzerpfade verbietet.
///
/// Ohne `--config-dir` benutzt die CLI absichtlich den Konfigurationsordner der
/// Desktop-App, ohne `--audit` das Standard-Auditjournal. Für echte Aufrufe ist das
/// richtig. In einer Testumgebung ist es gefährlich: Ein Testlauf hat am 2026-08-02
/// genau so das echte Queue-Journal geöffnet und dabei auf eine neuere Journalversion
/// migriert. Ist diese Variable gesetzt, wird ein fehlender Pfad deshalb zum sichtbaren
/// Fehler statt zu einer stillen Wirkung auf fremde Daten. Der Wert `0` und ein leerer
/// Wert gelten als „nicht gesetzt", damit sich die Sperre gezielt abschalten lässt.
const NO_DEFAULT_USER_PATHS_ENV: &str = "DOPPELDECKER_NO_DEFAULT_USER_PATHS";

/// Stabile Anwendungskennung der Desktop-App (`src-tauri/tauri.conf.json`).
///
/// Konfiguration **und** Realmodus-Audit gehören in denselben Anwendungsordner.
/// Sonst schreiben CLI und Desktop ihre sicherheitsrelevante Historie an zwei
/// Stellen, und jede spätere Prüfung sieht nur einen Teil der echten Läufe.
const APPLICATION_IDENTIFIER: &str = "com.doppeldecker.desktop";

#[derive(Parser)]
#[command(
    name = "doppeldecker-cli",
    version,
    about = "Sichere headless Planung sowie lokale und Remote-Synchronisation"
)]
struct Cli {
    /// JSON-Lines für Maschinensteuerung ausgeben. Prompts bleiben auf stderr.
    #[arg(long, global = true)]
    json: bool,
    /// Konfigurationsordner der Desktop-App. Ohne Angabe wird der plattformübliche
    /// Ordner für `com.doppeldecker.desktop` verwendet — außer die Umgebungsvariable
    /// `DOPPELDECKER_NO_DEFAULT_USER_PATHS` verbietet diesen Rückfall.
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Reinen Dry-Run-Plan berechnen; verändert nichts.
    Plan(SyncArgs),
    /// Lokalen Realmodus im selben Prozess vorbereiten, bestätigen und ausführen.
    Real(RealArgs),
    /// Einen Ordner headless anlegen (reine AddOnly-Mutation, dieselbe Kernfunktion
    /// wie die interaktive GUI-Aktion „Neuer Ordner").
    Mkdir(MkdirArgs),
    /// Einen lokalen oder Remote-Ordner ohne Mutation auflisten. SFTP v3 liefert
    /// dabei sichtbar nur eine pfadbasierte Best-Effort-Anzeige.
    List(EndpointArgs),
    /// Einen normalen Einweg-Sync zwischen lokalen oder gespeicherten Remote-Endpunkten
    /// vorbereiten, bestätigen und im selben Prozess ausführen.
    Sync(ProviderSyncArgs),
    /// Ausgewählte Dateien oder Ordner zwischen lokalen und Remote-Endpunkten kopieren.
    Copy(ProviderFileArgs),
    /// Ausgewählte Dateien oder Ordner sicher verschieben.
    Move(ProviderFileArgs),
    /// Einen Eintrag innerhalb eines gebundenen Endpunkts ohne Ersetzen umbenennen.
    Rename(RenameArgs),
    /// Lokale Dateien oder Ordner über den sicheren nativen Systempapierkorb entfernen.
    Trash(TrashArgs),
    /// Remote-Einträge nach gebundenem Vollscan quarantänisieren oder dauerhaft löschen.
    Delete(DeleteArgs),
    /// Verhaltensgeprüfte Fähigkeiten einer konkreten Verbindung ausgeben.
    Capabilities(EndpointArgs),
    /// Manifestierte Quarantäneläufe anzeigen, wiederherstellen oder leeren.
    Quarantine(QuarantineArgs),
    /// Lokalen bidirektionalen Plan, Lauf, Resume oder Rollback ausführen.
    Bidirectional(BidirectionalArgs),
    /// Gespeicherte Verbindungen ohne GUI verwalten.
    Connections(ConnectionsArgs),
}

#[derive(Args)]
struct ConnectionsArgs {
    #[command(subcommand)]
    command: ConnectionsCommand,
}

#[derive(Subcommand)]
enum ConnectionsCommand {
    /// Mehrzeilige Verbindungsdaten von stdin prüfen und sicher importieren.
    Import,
}

#[derive(Args)]
struct MkdirArgs {
    /// Bereits bestehender übergeordneter Ordner, in dem der neue Ordner entsteht.
    #[arg(long)]
    parent: PathBuf,
    /// Name des neuen Ordners; darf keinen Pfadtrenner enthalten.
    #[arg(long)]
    name: String,
    /// Endpunktart. Remote-Endpunkte lösen ausschließlich ein gespeichertes Profil auf.
    #[arg(long, value_enum, default_value_t = ProviderArg::Local)]
    provider: ProviderArg,
    /// ID eines gespeicherten Remote-Profils. Für `local` nicht zulässig.
    #[arg(long)]
    profile: Option<String>,
}

#[derive(Clone, Args)]
struct EndpointArgs {
    #[arg(long, value_enum, default_value_t = ProviderArg::Local)]
    provider: ProviderArg,
    #[arg(long)]
    profile: Option<String>,
    /// Lokaler Root beziehungsweise virtueller Unterroot des gespeicherten Profils.
    #[arg(long)]
    root: PathBuf,
}

#[derive(Clone, Args)]
struct ProviderFileArgs {
    #[arg(long, value_enum, default_value_t = ProviderArg::Local)]
    source_provider: ProviderArg,
    #[arg(long)]
    source_profile: Option<String>,
    #[arg(long)]
    source_root: PathBuf,
    #[arg(long, value_enum, default_value_t = ProviderArg::Local)]
    target_provider: ProviderArg,
    #[arg(long)]
    target_profile: Option<String>,
    #[arg(long)]
    target_root: PathBuf,
    /// Exakter ausgewählter Pfad; für mehrere Einträge wiederholen.
    #[arg(long = "item", required = true)]
    items: Vec<PathBuf>,
    /// Vorhandene Ziele überspringen. Ersetzen wird nicht still aktiviert.
    #[arg(long, default_value_t = false, conflicts_with = "replace")]
    skip_conflicts: bool,
    /// Exakt bestätigte Konflikte ersetzen. Starke Provider parken Altversionen in
    /// Quarantäne; FTPS/WebDAV dürfen nach sichtbarer Warnung dauerhaft ersetzen.
    #[arg(long, default_value_t = false)]
    replace: bool,
}

#[derive(Clone, Args)]
struct ProviderSyncArgs {
    #[arg(long, value_enum, default_value_t = ProviderArg::Local)]
    source_provider: ProviderArg,
    #[arg(long)]
    source_profile: Option<String>,
    #[arg(long)]
    source_root: PathBuf,
    #[arg(long, value_enum, default_value_t = ProviderArg::Local)]
    target_provider: ProviderArg,
    #[arg(long)]
    target_profile: Option<String>,
    #[arg(long)]
    target_root: PathBuf,
    #[arg(long, value_enum, default_value_t = ModeArg::AddOnly)]
    mode: ModeArg,
    #[arg(long, value_enum, default_value_t = CompareArg::Fast)]
    compare: CompareArg,
    #[arg(long, value_enum, default_value_t = MetadataArg::Portable)]
    metadata: MetadataArg,
    #[arg(long, default_value_t = 2)]
    mtime_tolerance: i64,
    /// Relative Einträge ausblenden; für mehrere Muster wiederholen.
    #[arg(long = "exclude")]
    exclude: Vec<String>,
    /// Append-only Auditdatei. Ohne Angabe liegt sie im gewählten App-Konfigurationsordner.
    #[arg(long)]
    audit: Option<PathBuf>,
}

#[derive(Args)]
struct RenameArgs {
    #[command(flatten)]
    endpoint: EndpointArgs,
    #[arg(long)]
    path: PathBuf,
    #[arg(long)]
    new_name: String,
}

#[derive(Args)]
struct DeleteArgs {
    #[command(flatten)]
    endpoint: EndpointArgs,
    #[arg(long = "item", required = true)]
    items: Vec<PathBuf>,
    #[arg(long, value_enum)]
    mode: DeleteModeArg,
}

#[derive(Args)]
struct TrashArgs {
    /// Exakter lokaler Pfad; für mehrere Einträge wiederholen.
    #[arg(long = "item", required = true)]
    items: Vec<PathBuf>,
}

#[derive(Clone, Args)]
struct SyncArgs {
    #[arg(long)]
    source: PathBuf,
    #[arg(long)]
    target: PathBuf,
    #[arg(long, value_enum, default_value_t = ModeArg::AddOnly)]
    mode: ModeArg,
    #[arg(long, value_enum, default_value_t = CompareArg::Fast)]
    compare: CompareArg,
    #[arg(long, value_enum, default_value_t = MetadataArg::Portable)]
    metadata: MetadataArg,
    #[arg(long, default_value_t = 2)]
    mtime_tolerance: i64,
    /// Relative Einträge ausblenden; für mehrere Muster wiederholen.
    #[arg(long = "exclude")]
    exclude: Vec<String>,
}

#[derive(Args)]
struct RealArgs {
    #[command(flatten)]
    sync: SyncArgs,
    /// Append-only Auditdatei. Ohne Angabe wird der lokale App-/State-Ordner genutzt.
    #[arg(long)]
    audit: Option<PathBuf>,
}

#[derive(Args)]
struct QuarantineArgs {
    #[command(subcommand)]
    command: QuarantineCommand,
}

#[derive(Args)]
struct BidirectionalArgs {
    #[command(subcommand)]
    command: BidirectionalCommand,
}

#[derive(Subcommand)]
enum BidirectionalCommand {
    /// Vollständig planen und das mutationslose Journal danach verwerfen.
    Plan(BidirectionalPlanArgs),
    /// Im selben Prozess planen, bestätigen und ausführen.
    Run(BidirectionalPlanArgs),
    /// Einen offenen Lauf mit frischen Handles und neuer Phrase fortsetzen.
    Resume(BidirectionalRecoveryArgs),
    /// Einen offenen Teilcommit rückwärts wiederherstellen.
    Rollback(BidirectionalRecoveryArgs),
}

#[derive(Args)]
struct BidirectionalPlanArgs {
    /// Dauerhafte 128-Bit-Paar-ID als 32 Hexzeichen.
    #[arg(long)]
    pair_id: String,
    #[arg(long)]
    pair_name: String,
    #[arg(long)]
    left: PathBuf,
    #[arg(long)]
    right: PathBuf,
    /// Dauerhafte Baseline-Datei außerhalb der beiden Datenroots.
    #[arg(long)]
    baseline: PathBuf,
    /// Optionales JSON-Array aus `ConflictResolution`-Objekten.
    #[arg(long)]
    resolutions: Option<PathBuf>,
}

#[derive(Args)]
struct BidirectionalRecoveryArgs {
    #[arg(long)]
    pair_name: String,
    #[arg(long)]
    left: PathBuf,
    #[arg(long)]
    right: PathBuf,
    #[arg(long)]
    baseline: PathBuf,
}

#[derive(Subcommand)]
enum QuarantineCommand {
    /// Läufe für exakt einen Zielroot anzeigen.
    List(TargetArg),
    /// Einträge eines Laufs anzeigen.
    Entries(RunArg),
    /// Einen Eintrag ohne Überschreiben wiederherstellen; benötigt stdin-Phrase.
    Restore(RestoreArg),
    /// Einen ganzen Lauf endgültig leeren; benötigt stdin-Phrase.
    Purge(RunArg),
}

#[derive(Args)]
struct TargetArg {
    #[arg(long)]
    target: PathBuf,
}

#[derive(Args)]
struct RunArg {
    #[arg(long)]
    target: PathBuf,
    #[arg(long)]
    run: String,
}

#[derive(Args)]
struct RestoreArg {
    #[arg(long)]
    target: PathBuf,
    #[arg(long)]
    run: String,
    #[arg(long)]
    path: String,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    AddOnly,
    Update,
    Mirror,
}

#[derive(Clone, Copy, ValueEnum)]
enum CompareArg {
    Fast,
    Content,
}

#[derive(Clone, Copy, ValueEnum)]
enum MetadataArg {
    Portable,
    NativeBestEffort,
    NativeStrict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ProviderArg {
    Local,
    Sftp,
    Ftps,
    Webdav,
    Smb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum DeleteModeArg {
    Quarantine,
    Permanent,
}

impl SyncArgs {
    fn options(&self) -> SyncOptions {
        sync_options(
            self.mode,
            self.compare,
            self.metadata,
            self.mtime_tolerance,
            self.exclude.clone(),
        )
    }
}

impl ProviderSyncArgs {
    fn options(&self) -> SyncOptions {
        sync_options(
            self.mode,
            self.compare,
            self.metadata,
            self.mtime_tolerance,
            self.exclude.clone(),
        )
    }
}

fn sync_options(
    mode: ModeArg,
    compare: CompareArg,
    metadata: MetadataArg,
    mtime_tolerance: i64,
    exclude: Vec<String>,
) -> SyncOptions {
    SyncOptions {
        mode: match mode {
            ModeArg::AddOnly => SyncMode::AddOnly,
            ModeArg::Update => SyncMode::Update,
            ModeArg::Mirror => SyncMode::Mirror,
        },
        compare: match compare {
            CompareArg::Fast => Compare::SizeAndTime,
            CompareArg::Content => Compare::ContentHash,
        },
        metadata: match metadata {
            MetadataArg::Portable => MetadataProfile::Portable,
            MetadataArg::NativeBestEffort => MetadataProfile::NativeBestEffort,
            MetadataArg::NativeStrict => MetadataProfile::NativeStrict,
        },
        mtime_tolerance,
        exclude,
    }
}

struct Output {
    json: bool,
}

impl Output {
    fn event(&mut self, value: serde_json::Value) -> io::Result<()> {
        if self.json {
            let stdout = io::stdout();
            let mut lock = stdout.lock();
            serde_json::to_writer(&mut lock, &value).map_err(io::Error::other)?;
            lock.write_all(b"\n")?;
            lock.flush()
        } else {
            Ok(())
        }
    }

    fn text(&self, value: impl AsRef<str>) {
        if !self.json {
            println!("{}", value.as_ref());
        }
    }

    fn error(&mut self, error: &io::Error) {
        // Der stabile Maschinencode (plan_drift, sandbox_violation, mirror_blocked,
        // confirmation_mismatch, io) macht Fehler für Agenten auswertbar, ohne auf
        // deutsche Texte zu matchen.
        if self.json {
            let _ = self.event(cli_error_event(error));
        } else {
            eprintln!("Fehler: {error}");
        }
    }

    fn provider_file_error(
        &mut self,
        execution_error: &io::Error,
        actuals: &doppeldecker_core::fileops::FileOperationActuals,
        progress: &Progress,
        recovery: Option<&doppeldecker_core::queue::ProviderFileRecovery>,
        journal_errors: &[io::Error],
    ) {
        if self.json {
            let _ = self.event(provider_file_error_event(
                execution_error,
                actuals,
                progress,
                recovery,
                journal_errors,
            ));
        } else {
            eprintln!("Ausführungsfehler: {execution_error}");
            for journal_error in journal_errors {
                eprintln!("Maßgeblicher Queue-Journalfehler: {journal_error}");
            }
            eprintln!(
                "Teilbilanz: {} Dateien, {} Bytes; bekannte Best-Effort-Wirkungen: neu={:?}, ersetzen={:?}, Quelle entfernen={:?}",
                progress.files_done,
                progress.bytes_done,
                actuals.target_copy_new_best_effort,
                actuals.target_replace_best_effort,
                actuals.source_removal_best_effort,
            );
            if !actuals.temp_recovery_paths.is_empty() {
                eprintln!(
                    "Temp-Recoverypfade zur manuellen Prüfung: {}",
                    actuals.temp_recovery_paths.join(", ")
                );
            }
            if let Some(recovery) = recovery {
                eprintln!(
                    "Recovery: {} ({} geparkte Einträge)",
                    recovery.quarantine_run, recovery.parked_entries
                );
            }
        }
    }
}

fn cli_error_event(error: &io::Error) -> serde_json::Value {
    serde_json::json!({
        "event": "error",
        "code": doppeldecker_core::error_code(error).as_str(),
        "message": error.to_string(),
        "recovery_paths": doppeldecker_core::error::recovery_paths(error),
    })
}

fn provider_file_error_event(
    execution_error: &io::Error,
    actuals: &doppeldecker_core::fileops::FileOperationActuals,
    progress: &Progress,
    recovery: Option<&doppeldecker_core::queue::ProviderFileRecovery>,
    journal_errors: &[io::Error],
) -> serde_json::Value {
    let primary = journal_errors.last().unwrap_or(execution_error);
    serde_json::json!({
        "event": "error",
        "code": doppeldecker_core::error_code(primary).as_str(),
        "message": primary.to_string(),
        "recovery_paths": doppeldecker_core::error::recovery_paths(primary),
        "execution_error": {
            "code": doppeldecker_core::error_code(execution_error).as_str(),
            "message": execution_error.to_string(),
            "recovery_paths": doppeldecker_core::error::recovery_paths(execution_error),
        },
        "journal_error": journal_errors.last().map(|error| serde_json::json!({
            "code": doppeldecker_core::error_code(error).as_str(),
            "message": error.to_string(),
            "recovery_paths": doppeldecker_core::error::recovery_paths(error),
        })),
        "journal_errors": journal_errors.iter().map(|error| serde_json::json!({
            "code": doppeldecker_core::error_code(error).as_str(),
            "message": error.to_string(),
            "recovery_paths": doppeldecker_core::error::recovery_paths(error),
        })).collect::<Vec<_>>(),
        "stats": {
            "files": progress.files_done,
            "bytes": progress.bytes_done,
            "source_removal_best_effort": actuals.source_removal_best_effort,
            "target_copy_new_best_effort": actuals.target_copy_new_best_effort,
            "target_replace_best_effort": actuals.target_replace_best_effort,
            "target_write_limitations": actuals.target_write_limitations,
            "temp_recovery_paths": actuals.temp_recovery_paths,
            "retained_backup_paths": actuals.retained_backup_paths,
        },
        "recovery": recovery,
    })
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    let mut output = Output { json };
    // Der Konfigurationsordner wird vor jedem Befehl aufgelöst, auch vor denen, die ihn
    // heute gar nicht brauchen. So kann kein später hinzugefügter Zugriff still auf den
    // echten Ordner zurückfallen.
    let result = match resolve_config_dir(cli.config_dir) {
        Err(error) => Err(error),
        Ok(config_dir) => match cli.command {
            Command::Plan(args) => plan_command(args, &mut output),
            Command::Real(args) => real_command(args, &config_dir, &mut output),
            Command::Mkdir(args) => mkdir_command(args, &config_dir, &mut output),
            Command::List(args) => list_command(args, &config_dir, &mut output),
            Command::Sync(args) => provider_sync_command(args, &config_dir, &mut output),
            Command::Copy(args) => {
                provider_file_command(FileQueueKind::Copy, args, &config_dir, &mut output)
            }
            Command::Move(args) => {
                provider_file_command(FileQueueKind::Move, args, &config_dir, &mut output)
            }
            Command::Rename(args) => rename_command(args, &config_dir, &mut output),
            Command::Trash(args) => trash_command(args, &config_dir, &mut output),
            Command::Delete(args) => delete_command(args, &config_dir, &mut output),
            Command::Capabilities(args) => capabilities_command(args, &config_dir, &mut output),
            Command::Quarantine(args) => quarantine_command(args, &config_dir, &mut output),
            Command::Bidirectional(args) => bidirectional_command(args, &config_dir, &mut output),
            Command::Connections(args) => connections_command(args, &config_dir, &mut output),
        },
    };
    let code = match result {
        Ok(code) => code,
        Err(error) => {
            output.error(&error);
            2
        }
    };
    if code != 0 {
        std::process::exit(code);
    }
}

fn plan_command(args: SyncArgs, output: &mut Output) -> io::Result<i32> {
    let options = args.options();
    options.validate()?;
    let source = expand_and_canonicalize(&args.source)?;
    let target = expand_and_canonicalize(&args.target)?;
    let (source, target) = doppeldecker_core::validate_local_sync_roots(&source, &target)?;
    let vfs = LocalVfs::new();
    let plan = doppeldecker_core::plan(&vfs, &source, &vfs, &target, &options)?;
    output.event(serde_json::json!({
        "event": "plan",
        "source": source,
        "target": target,
        "options": options,
        "plan": plan,
    }))?;
    output.text(format!(
        "Plan: {} kopieren, {} aktualisieren, {} Metadaten, {} quarantänisieren, {} Bytes übertragen",
        plan.copy_count,
        plan.update_count,
        plan.metadata_count,
        plan.delete_count,
        plan.bytes_to_transfer
    ));
    Ok(0)
}

fn mkdir_command(args: MkdirArgs, config_dir: &Path, output: &mut Output) -> io::Result<i32> {
    // Reine Syntaxprüfung vor Profilauflösung und möglicher Providerprobe. Der Core
    // prüft den normalisierten Namen bei der eigentlichen Anlage erneut.
    let name = doppeldecker_core::validate_directory_create_name(&args.name)?;
    // Lokale Eltern werden durch die Endpunktauflösung kanonisiert und danach
    // descriptor-relativ gehalten. Remote bleibt der ausdrücklich angegebene
    // Pane-Root maßgeblich; die gewöhnliche WebDAV-Addition erbt keine Löschprobe.
    let mut spec = EndpointSpec {
        provider: args.provider,
        profile: args.profile,
        root: args.parent,
    };
    // Reine Endpunktsyntax gehört noch vor das Queuejournal. So kann weder ein
    // lokales Profilargument noch ein relativer oder nicht normalisierter Root
    // bereits Prozesszustand erzeugen.
    spec.root = validate_endpoint_spec(&spec)?;
    if spec.provider != ProviderArg::Local {
        doppeldecker_core::validate_directory_create_request(&spec.root, &name)?;
    }
    let contract = doppeldecker_core::directory_create_contract(spec.provider.kind());
    let expected = contract.execution_class.ok_or_else(|| {
        doppeldecker_core::coded_error(
            io::ErrorKind::Unsupported,
            doppeldecker_core::ErrorCode::Unsupported,
            contract.blockers.join("; "),
        )
    })?;
    // Prozessbesitz bleibt vor Verbindung und jeder Providerwirkung. Nur die
    // bisherigen übrigen Provider verwenden weiterhin ihre getrennte mkdir-Probe.
    let _queue_lock = open_cli_queue(config_dir)?;
    let mut resolved = resolve_endpoint(&spec, config_dir)?;
    resolved.endpoint = _queue_lock.bind_provider_preparation(&resolved.endpoint)?;
    for note in &contract.limitations {
        eprintln!("{note}");
    }
    let report = resolved
        .endpoint
        .create_interactive_directory(&name, expected)?;
    output.event(serde_json::json!({
        "event": "mkdir",
        "provider": resolved.provider_label,
        "profile": resolved.profile_id,
        "path": report.path,
        "execution_class": report.execution_class,
        "limitations": report.limitations,
    }))?;
    output.text(format!("Ordner angelegt: {}", report.path.display()));
    Ok(0)
}

fn list_command(args: EndpointArgs, config_dir: &Path, output: &mut Output) -> io::Result<i32> {
    let spec = EndpointSpec {
        provider: args.provider,
        profile: args.profile,
        root: args.root,
    };
    let resolved = resolve_endpoint(&spec, config_dir)?;
    let root = resolved.endpoint.root().to_path_buf();
    let listing = resolved.endpoint.list_interactive()?;
    let complete = listing.complete;
    let security = listing.security;
    output.text(format!(
        "{} Einträge unter {}",
        listing.entries.len(),
        root.display()
    ));
    output.text(interactive_listing_status(complete, security));
    for listed in &listing.entries {
        let entry = &listed.entry;
        let kind = if entry.is_symlink {
            "Symlink"
        } else if entry.is_dir {
            "Ordner"
        } else if entry.is_file {
            "Datei"
        } else {
            "Spezialdatei"
        };
        let decoding = if listed.safely_decoded {
            ""
        } else {
            " · Name nicht sicher dekodierbar"
        };
        let size = if listed.size_known {
            format!("{} Byte", entry.size)
        } else {
            "—".into()
        };
        output.text(format!(
            "{kind}\t{}\t{size}{decoding}",
            entry.name.escape_debug()
        ));
    }
    for omitted in &listing.omitted {
        output.text(format!(
            "Ausgelassen\t{}\t{}",
            omitted.display_name, omitted.reason
        ));
    }
    let entries = listing
        .entries
        .into_iter()
        .map(|listed| {
            Ok(serde_json::json!({
                "entry": listed.display_entry()?,
                "safely_decoded": listed.safely_decoded,
            }))
        })
        .collect::<io::Result<Vec<_>>>()?;
    output.event(serde_json::json!({
        "event": "listing",
        "provider": resolved.provider_label,
        "profile": resolved.profile_id,
        "root": root,
        "complete": complete,
        "security": security,
        "entries": entries,
        "omitted": listing.omitted,
    }))?;
    Ok(0)
}

fn interactive_listing_status(complete: bool, security: Option<SftpReadSecurity>) -> String {
    match security {
        Some(SftpReadSecurity::SftpPathBestEffort) => format!(
            "Ausführungsklasse: Best Effort · Listing vollständig: {} · SFTP-Sicherheitsstatus: Pfadnamespace ist zwischen LSTAT, REALPATH und READDIR nicht atomar an ein Verzeichnishandle gebunden",
            if complete { "ja" } else { "nein" }
        ),
        None => format!(
            "Ausführungsklasse: abgesichert · Listing vollständig: {}",
            if complete { "ja" } else { "nein" }
        ),
    }
}

fn provider_sync_command(
    args: ProviderSyncArgs,
    config_dir: &Path,
    output: &mut Output,
) -> io::Result<i32> {
    // Syntaxfehler haben garantiert keine Verbindung, Capability-Probe oder
    // Queue-Mutation zur Folge.
    let options = args.options();
    options.validate()?;
    let source_root = validate_provider_sync_root(args.source_provider, &args.source_root)?;
    let target_root = validate_provider_sync_root(args.target_provider, &args.target_root)?;
    let source_spec = EndpointSpec {
        provider: args.source_provider,
        profile: args.source_profile.clone(),
        root: source_root.clone(),
    };
    let target_spec = EndpointSpec {
        provider: args.target_provider,
        profile: args.target_profile.clone(),
        root: target_root.clone(),
    };
    validate_endpoint_spec(&source_spec)?;
    validate_endpoint_spec(&target_spec)?;
    let audit = validate_audit_path(
        args.audit
            .clone()
            .map(expand_home_path)
            .unwrap_or_else(|| config_dir.join("provider-real-runs.jsonl")),
    )?;
    // Nur ein lokaler Root teilt den Namensraum mit der Auditdatei; ein Remote-Root
    // kann sie gar nicht enthalten.
    let lokale_roots: Vec<&Path> = [
        (args.source_provider == ProviderArg::Local).then_some(source_root.as_path()),
        (args.target_provider == ProviderArg::Local).then_some(target_root.as_path()),
    ]
    .into_iter()
    .flatten()
    .collect();
    reject_audit_inside_roots(&audit, &lokale_roots)?;
    // Beide gespeicherten Konfigurationen müssen gültig sein, bevor bereits die
    // Quellverbindung Secrets oder Netzwerk benötigt oder die Queue angelegt wird.
    let source_endpoint = resolve_queue_endpoint(&source_spec, config_dir)?;
    let target_endpoint = resolve_queue_endpoint(&target_spec, config_dir)?;
    // Prozessbesitz bleibt vor Verbindung, Proben und Lauf bestehen. Ein wartendes
    // oder pausiertes Journal darf nicht von einem direkten Sync überholt werden.
    let queue_lock = open_cli_queue(config_dir)?;
    require_cli_queue_idle(&queue_lock, "Provider-Sync")?;
    let source = connect_resolved_endpoint(&source_spec, source_endpoint)?;
    let target = connect_resolved_endpoint(&target_spec, target_endpoint)?;
    let bound_source_root =
        doppeldecker_core::fileops::validate_file_operation_root(source.endpoint.root())?;
    let bound_target_root =
        doppeldecker_core::fileops::validate_file_operation_root(target.endpoint.root())?;
    if bound_source_root != source_root || bound_target_root != target_root {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Provider-Sync-Roots stimmen nicht mit den frisch gebundenen Endpunkten überein",
        ));
    }
    output.event(serde_json::json!({ "event": "provider_sync_preparing" }))?;
    if !output.json {
        eprintln!("Provider-Sync-Vorprüfung: Identitäten, Listings, Fähigkeiten, Plan und Platz …");
    }
    let prepared = doppeldecker_core::prepare_provider_transfer(
        queue_lock.bind_provider_preparation(&source.endpoint)?,
        queue_lock.bind_provider_preparation(&target.endpoint)?,
        options,
    )?;
    let preview = prepared.preview();
    output.event(serde_json::json!({ "event": "provider_sync_preview", "preview": preview }))?;
    output.text(format!(
        "Provider-Sync vorbereitet: {} neu, {} Ersetzungen, {} Quarantäne-Einträge; {} Bytes",
        preview.impact.new_files,
        preview.impact.replacements,
        preview.impact.quarantine_entries,
        preview.impact.required_available_bytes
    ));

    let (confirmation, backup_confirmed) = read_run_confirmation(preview)?;
    let run = prepared.confirm(&confirmation, backup_confirmed)?;
    let cancel = install_cancel_handler()?;
    let mut last = Instant::now() - Duration::from_secs(1);
    let stats = run.execute_with_audit(&audit, &cancel, &mut |progress| {
        emit_throttled_progress(output, &mut last, progress);
    })?;
    emit_sync_result(output, "provider_sync_result", &stats, &audit)
}

/// Prüft einen CLI-Root vollständig, bevor Profile, Secrets oder Netzwerk überhaupt
/// berührt werden. Lokale Pfade werden zusätzlich kanonisiert, damit ein Symlink oder
/// Alias nicht erst nach der Providerauflösung einen anderen Baum bindet.
fn validate_provider_sync_root(provider: ProviderArg, root: &Path) -> io::Result<PathBuf> {
    let lexical = if provider == ProviderArg::Local {
        expand_home_path(root)
    } else {
        root.to_path_buf()
    };
    let normalized = doppeldecker_core::fileops::validate_file_operation_root(&lexical)?;
    if normalized != lexical {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Provider-Sync-Root muss lexikalisch normalisiert sein",
        ));
    }
    if provider == ProviderArg::Local {
        let canonical = std::fs::canonicalize(&normalized)?;
        doppeldecker_core::fileops::validate_file_operation_root(&canonical)
    } else {
        Ok(normalized)
    }
}

#[derive(Clone)]
struct EndpointSpec {
    provider: ProviderArg,
    profile: Option<String>,
    root: PathBuf,
}

struct ResolvedEndpoint {
    endpoint: ProviderEndpoint,
    queue_endpoint: BidirectionalQueueEndpoint,
    provider_label: &'static str,
    profile_id: Option<String>,
}

struct KeyringResolver;

impl SecretResolver for KeyringResolver {
    fn resolve(&self, reference: &KeychainSecretReference) -> io::Result<SecretString> {
        reference.validate()?;
        let entry = keyring::Entry::new(&reference.service, &reference.account)
            .map_err(|_| io::Error::other("Schlüsselbundzugriff nicht verfügbar"))?;
        let value = entry.get_password().map_err(|error| match error {
            keyring::Error::NoEntry => {
                io::Error::new(io::ErrorKind::NotFound, "Schlüsselbund-Eintrag fehlt")
            }
            keyring::Error::NoStorageAccess(_) => io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Zugriff auf den System-Schlüsselbund wurde abgelehnt",
            ),
            _ => io::Error::other("Schlüsselbundwert konnte nicht gelesen werden"),
        })?;
        SecretString::new(value)
    }
}

struct ImportSecretResolver {
    expected: KeychainSecretReference,
    secret: Zeroizing<String>,
}

impl SecretResolver for ImportSecretResolver {
    fn resolve(&self, reference: &KeychainSecretReference) -> io::Result<SecretString> {
        reference.validate()?;
        if *reference != self.expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Importprobe verlangte eine unerwartete Schlüsselbundreferenz",
            ));
        }
        SecretString::new(self.secret.to_string())
    }
}

enum ImportProfile {
    Sftp(SftpProfile),
    Ftps(FtpsProfile),
}

enum ProbeFailure {
    Unavailable(ProbeStage),
    Security(&'static str),
}

#[derive(Clone, Copy)]
enum ProbeStage {
    SftpUnavailable,
    SftpHostKeyUnknown,
    SftpLogin,
    SftpListing,
    FtpUnavailable,
    FtpLogin,
    FtpCapabilities,
    FtpRoot,
    FtpListing,
    FtpsUnavailable,
    FtpsLogin,
    FtpsCapabilities,
    FtpsRoot,
    FtpsSession,
    FtpsListing,
}

impl ProbeStage {
    /// Feste, geheimnisfreie Diagnose. Keine Bibliotheksfehlermeldung darf hier
    /// landen, weil sie Serverantworten oder Eingabewerte enthalten könnte.
    fn message(self) -> &'static str {
        match self {
            Self::SftpUnavailable => "SFTP: Dienst nicht erreichbar",
            Self::SftpHostKeyUnknown => "SFTP: Hostkey fehlt in OpenSSH-known_hosts",
            Self::SftpLogin => "SFTP: Anmeldung abgelehnt",
            Self::SftpListing => "SFTP: Root-Listing fehlgeschlagen",
            Self::FtpUnavailable => "FTP: Dienst nicht erreichbar",
            Self::FtpLogin => "FTP: Anmeldung abgelehnt",
            Self::FtpCapabilities => "FTP: MLSD- oder UTF-8-Unterstützung fehlt",
            Self::FtpRoot => "FTP: angegebener Remote-Root nicht verfügbar",
            Self::FtpListing => "FTP: vollständiges Root-Listing fehlgeschlagen",
            Self::FtpsUnavailable => "FTPS: explizites TLS nicht erreichbar",
            Self::FtpsLogin => "FTPS: Anmeldung abgelehnt",
            Self::FtpsCapabilities => "FTPS: MLSD- oder UTF-8-Unterstützung fehlt",
            Self::FtpsRoot => "FTPS: angegebener Remote-Root nicht verfügbar",
            Self::FtpsSession => "FTPS: TLS- oder Sitzungsaufbau fehlgeschlagen",
            Self::FtpsListing => "FTPS: vollständiges Root-Listing fehlgeschlagen",
        }
    }
}

fn connections_command(
    args: ConnectionsArgs,
    config_dir: &Path,
    output: &mut Output,
) -> io::Result<i32> {
    match args.command {
        ConnectionsCommand::Import => import_connection(config_dir, output),
    }
}

fn import_connection(config_dir: &Path, output: &mut Output) -> io::Result<i32> {
    let stdin = io::stdin();
    let parsed = connection_import::read_and_parse(stdin.lock())?;
    let mut attempted = Vec::new();
    let mut unavailable = Vec::new();
    for &protocol in parsed.candidate_protocols() {
        attempted.push(protocol);
        let resolved = parsed.resolve(protocol)?;
        let profile = match probe_import(&resolved) {
            Ok(profile) => profile,
            Err(ProbeFailure::Unavailable(stage)) => {
                unavailable.push(stage);
                continue;
            }
            Err(ProbeFailure::Security(message)) => {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, message));
            }
        };
        let (alternate_count, security) = match &profile {
            ImportProfile::Sftp(profile) => (
                profile.connection.alternate_hosts.len(),
                "host_key_verified",
            ),
            ImportProfile::Ftps(profile) => (
                profile.connection.alternate_hosts.len(),
                match profile.connection.security_status() {
                    doppeldecker_core::FtpSecurityStatus::Plaintext => "plaintext",
                    doppeldecker_core::FtpSecurityStatus::TlsIdentityUnverified => {
                        "tls_identity_unverified"
                    }
                    doppeldecker_core::FtpSecurityStatus::TlsIdentityExternallyVerified => {
                        "tls_identity_externally_verified"
                    }
                    doppeldecker_core::FtpSecurityStatus::TlsIdentityVerified => {
                        "tls_identity_verified"
                    }
                },
            ),
        };
        persist_import(config_dir, profile, &resolved.password)?;
        output.event(serde_json::json!({
            "event": "connection_imported",
            "protocol": protocol.label(),
            "alternate_count": alternate_count,
            "security": security,
        }))?;
        let protocol_label = protocol.label().to_ascii_uppercase();
        let message = match security {
            "plaintext" => format!(
                "{protocol_label}-Verbindung geprüft und importiert. Achtung: Zugangsdaten und Dateien werden unverschlüsselt übertragen."
            ),
            "tls_identity_unverified" => format!(
                "{protocol_label}-Verbindung geprüft und importiert. Achtung: TLS verschlüsselt, die Serveridentität ist nicht durch das Zertifikat bestätigt."
            ),
            _ => format!("{protocol_label}-Verbindung geprüft und importiert."),
        };
        output.text(message);
        return Ok(0);
    }
    let message = if attempted.len() == 1 {
        "Die angegebene Verbindung konnte nicht vollständig geprüft werden"
    } else {
        "Weder SFTP noch FTPS konnte vollständig und vertrauenswürdig geprüft werden"
    };
    let details = unavailable
        .into_iter()
        .map(ProbeStage::message)
        .collect::<Vec<_>>()
        .join("; ");
    let message = if details.is_empty() {
        message.to_owned()
    } else {
        format!("{message} ({details})")
    };
    Err(io::Error::new(io::ErrorKind::NotConnected, message))
}

fn probe_import(resolved: &ResolvedConnection) -> Result<ImportProfile, ProbeFailure> {
    match resolved.protocol {
        ImportProtocol::Sftp => probe_sftp_import(resolved),
        ImportProtocol::Ftp => probe_ftp_import(resolved),
        ImportProtocol::Ftps => probe_ftps_import(resolved),
    }
}

fn probe_sftp_import(resolved: &ResolvedConnection) -> Result<ImportProfile, ProbeFailure> {
    let pin = std::iter::once(resolved.host.as_str())
        .chain(resolved.alternate_hosts.iter().map(String::as_str))
        .find_map(|host| discover_sftp_host_key(host, resolved.port).ok())
        .ok_or(ProbeFailure::Unavailable(ProbeStage::SftpUnavailable))?;
    let mut known = false;
    for host in std::iter::once(resolved.host.as_str())
        .chain(resolved.alternate_hosts.iter().map(String::as_str))
    {
        match sftp_host_key_is_known(host, resolved.port, &pin) {
            Ok(true) => known = true,
            Ok(false) => {}
            Err(_) => {
                return Err(ProbeFailure::Security(
                    "SFTP-Hostkey widerspricht OpenSSH-known_hosts oder konnte nicht sicher geprüft werden",
                ));
            }
        }
    }
    if !known {
        return Err(ProbeFailure::Unavailable(ProbeStage::SftpHostKeyUnknown));
    }
    let alternate_hosts = bound_sftp_alternates(resolved, &pin);
    let reference = import_secret_reference(&resolved.id).map_err(|_| {
        ProbeFailure::Security("SFTP-Schlüsselbundreferenz konnte nicht sicher erzeugt werden")
    })?;
    let profile = SftpProfile {
        id: resolved.id.clone(),
        name: resolved.name.clone(),
        connection: SftpConnectionConfig {
            host: resolved.host.clone(),
            alternate_hosts,
            port: resolved.port,
            username: resolved.username.clone(),
            requested_root: resolved.requested_root.clone(),
            host_key: pin,
            authentication: SftpAuthenticationReference::Password {
                secret: reference.clone(),
            },
        },
    };
    let resolver = Arc::new(ImportSecretResolver {
        expected: reference,
        secret: resolved.password.clone(),
    });
    let connection = SftpVfs::connect(profile.connection.clone(), resolver).map_err(|error| {
        let stage = if error.kind() == io::ErrorKind::PermissionDenied {
            ProbeStage::SftpLogin
        } else {
            ProbeStage::SftpUnavailable
        };
        ProbeFailure::Unavailable(stage)
    })?;
    connection
        .list_interactive_best_effort(Path::new("/"))
        .map_err(|_| ProbeFailure::Unavailable(ProbeStage::SftpListing))?;
    Ok(ImportProfile::Sftp(profile))
}

fn bound_sftp_alternates(
    resolved: &ResolvedConnection,
    primary_pin: &doppeldecker_core::SftpHostKeyPin,
) -> Vec<String> {
    let mut bound = Vec::new();
    for host in &resolved.alternate_hosts {
        match discover_sftp_host_key(host, resolved.port) {
            Ok(pin) if pin == *primary_pin => bound.push(host.clone()),
            // Eine derzeit abweichende Route ist nicht dieselbe gepinnte Verbindung.
            // Die funktionierende Hauptroute darf sie trotzdem nicht blockieren.
            Ok(_) => {}
            // Eine ausdrücklich gelieferte, derzeit nicht geroutete Alternative
            // bleibt erhalten. Vor jeder späteren Anmeldung gilt derselbe Hostkey.
            Err(_) => bound.push(host.clone()),
        }
    }
    bound
}

fn probe_ftp_import(resolved: &ResolvedConnection) -> Result<ImportProfile, ProbeFailure> {
    let reference = import_secret_reference(&resolved.id).map_err(|_| {
        ProbeFailure::Security("FTP-Schlüsselbundreferenz konnte nicht erzeugt werden")
    })?;
    let profile = FtpsProfile {
        id: resolved.id.clone(),
        name: resolved.name.clone(),
        connection: FtpsConnectionConfig {
            host: resolved.host.clone(),
            alternate_hosts: resolved.alternate_hosts.clone(),
            port: resolved.port,
            username: resolved.username.clone(),
            requested_root: resolved.requested_root.clone(),
            transport_security: FtpTransportSecurity::Plaintext,
            certificate: None,
            certificate_policy: None,
            password_reference: reference.clone(),
        },
    };
    let resolver = Arc::new(ImportSecretResolver {
        expected: reference,
        secret: resolved.password.clone(),
    });
    let connection = FtpsVfs::connect(profile.connection.clone(), resolver)
        .map_err(|error| ProbeFailure::Unavailable(ftp_connection_stage(&error)))?;
    connection
        .list(Path::new("/"))
        .map_err(|_| ProbeFailure::Unavailable(ProbeStage::FtpListing))?;
    Ok(ImportProfile::Ftps(profile))
}

fn ftp_connection_stage(error: &io::Error) -> ProbeStage {
    match error.kind() {
        io::ErrorKind::PermissionDenied => ProbeStage::FtpLogin,
        io::ErrorKind::Unsupported => ProbeStage::FtpCapabilities,
        io::ErrorKind::NotFound => ProbeStage::FtpRoot,
        _ => ProbeStage::FtpUnavailable,
    }
}

fn probe_ftps_import(resolved: &ResolvedConnection) -> Result<ImportProfile, ProbeFailure> {
    let certificate = std::iter::once(resolved.host.as_str())
        .chain(resolved.alternate_hosts.iter().map(String::as_str))
        .find_map(|host| discover_ftps_certificate(host, resolved.port).ok())
        .ok_or(ProbeFailure::Unavailable(ProbeStage::FtpsUnavailable))?;
    let alternate_hosts = bound_ftps_alternates(resolved, &certificate);
    let reference = import_secret_reference(&resolved.id).map_err(|_| {
        ProbeFailure::Security("FTPS-Schlüsselbundreferenz konnte nicht sicher erzeugt werden")
    })?;
    let mut profile = FtpsProfile {
        id: resolved.id.clone(),
        name: resolved.name.clone(),
        connection: FtpsConnectionConfig {
            host: resolved.host.clone(),
            alternate_hosts,
            port: resolved.port,
            username: resolved.username.clone(),
            requested_root: resolved.requested_root.clone(),
            transport_security: FtpTransportSecurity::ExplicitTls,
            certificate: Some(certificate),
            certificate_policy: Some(FtpsCertificatePolicy::WebPkiAndPin),
            password_reference: reference.clone(),
        },
    };
    let resolver = Arc::new(ImportSecretResolver {
        expected: reference,
        secret: resolved.password.clone(),
    });
    let connection = match FtpsVfs::connect(profile.connection.clone(), resolver.clone()) {
        Ok(connection) => connection,
        Err(error) if doppeldecker_core::error_code(&error) == ErrorCode::TlsIdentityUnverified => {
            // Manche reale FTPES-Server liefern auf einer IP oder mit einem privaten
            // Zertifikat keine öffentlich prüfbare TLS-Identität. TLS und der zuerst
            // beobachtete Pin bleiben aktiv; nur die Identitätsbehauptung wird
            // sichtbar auf „ungeprüft“ zurückgestuft.
            profile.connection.certificate_policy =
                Some(FtpsCertificatePolicy::UnverifiedCertificate);
            FtpsVfs::connect(profile.connection.clone(), resolver)
                .map_err(|error| ProbeFailure::Unavailable(ftps_connection_stage(&error)))?
        }
        Err(error) => {
            return Err(ProbeFailure::Unavailable(ftps_connection_stage(&error)));
        }
    };
    connection
        .list(Path::new("/"))
        .map_err(|_| ProbeFailure::Unavailable(ProbeStage::FtpsListing))?;
    Ok(ImportProfile::Ftps(profile))
}

fn ftps_connection_stage(error: &io::Error) -> ProbeStage {
    match error.kind() {
        io::ErrorKind::PermissionDenied => ProbeStage::FtpsLogin,
        io::ErrorKind::Unsupported => ProbeStage::FtpsCapabilities,
        io::ErrorKind::NotFound => ProbeStage::FtpsRoot,
        _ => ProbeStage::FtpsSession,
    }
}

fn bound_ftps_alternates(
    resolved: &ResolvedConnection,
    primary_certificate: &doppeldecker_core::FtpsCertificatePin,
) -> Vec<String> {
    let mut bound = Vec::new();
    for host in &resolved.alternate_hosts {
        match discover_ftps_certificate(host, resolved.port) {
            Ok(certificate) if certificate == *primary_certificate => bound.push(host.clone()),
            // Eine andere Zertifikatsidentität ist keine Route derselben gepinnten
            // Verbindung. Sie wird nicht gespeichert, blockiert aber die
            // funktionierende Hauptroute nicht.
            Ok(_) => {}
            // Die ausdrücklich mitgelieferte Route bleibt auch ohne aktuelle
            // IPv6-Erreichbarkeit erhalten. Beim späteren Einsatz erzwingt der
            // Provider denselben Pin, bevor Anmeldung oder Dateizugriff erfolgen.
            Err(_) => bound.push(host.clone()),
        }
    }
    bound
}

fn import_secret_reference(id: &str) -> io::Result<KeychainSecretReference> {
    KeychainSecretReference::new("Doppeldecker", id)
}

fn persist_import(
    config_dir: &Path,
    profile: ImportProfile,
    password: &Zeroizing<String>,
) -> io::Result<()> {
    let reference = match &profile {
        ImportProfile::Sftp(profile) => match &profile.connection.authentication {
            SftpAuthenticationReference::Password { secret } => secret,
            _ => {
                return Err(io::Error::other(
                    "Importprofil enthält keine Passwortreferenz",
                ));
            }
        },
        ImportProfile::Ftps(profile) => &profile.connection.password_reference,
    };
    let entry = keyring::Entry::new(&reference.service, &reference.account)
        .map_err(|_| io::Error::other("Schlüsselbundzugriff nicht verfügbar"))?;
    let old_password = match entry.get_password() {
        Ok(value) => Some(Zeroizing::new(value)),
        Err(keyring::Error::NoEntry) => None,
        Err(keyring::Error::NoStorageAccess(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Zugriff auf den System-Schlüsselbund wurde abgelehnt",
            ));
        }
        Err(_) => {
            return Err(io::Error::other(
                "Vorhandener Schlüsselbundwert konnte nicht sicher gelesen werden",
            ));
        }
    };
    entry
        .set_password(password.as_str())
        .map_err(|_| io::Error::other("Schlüsselbundwert konnte nicht gespeichert werden"))?;

    let saved = match profile {
        ImportProfile::Sftp(profile) => {
            save_sftp_profile(&config_dir.join("sftp-profiles.json"), profile).map(|_| ())
        }
        ImportProfile::Ftps(profile) => {
            save_ftps_profile(&config_dir.join("ftps-profiles.json"), profile).map(|_| ())
        }
    };
    if saved.is_ok() {
        return Ok(());
    }

    let rollback = match old_password {
        Some(old_password) => entry.set_password(old_password.as_str()),
        None => entry.delete_credential(),
    };
    if rollback.is_err() {
        return Err(io::Error::other(
            "Profil konnte nicht gespeichert werden; Schlüsselbund-Rollback blieb unvollständig",
        ));
    }
    Err(io::Error::other(
        "Verbindungsprofil konnte nicht gespeichert werden",
    ))
}

fn resolve_queue_endpoint(
    spec: &EndpointSpec,
    config_dir: &Path,
) -> io::Result<BidirectionalQueueEndpoint> {
    // Jede Verbindung läuft durch dieselbe mutationsfreie Syntaxgrenze. Einzelne
    // Befehle prüfen zusätzlich vor dem Queueöffnen; dieser zweite Check verhindert,
    // dass ein späterer Aufrufer die Grenze versehentlich umgeht.
    let root = validate_endpoint_spec(spec)?;
    let root_string = root
        .to_str()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Endpunkt-Root ist nicht eindeutig als UTF-8 darstellbar",
            )
        })?
        .to_owned();
    let profile_id = spec.profile.as_deref();
    let queue_endpoint = match spec.provider {
        ProviderArg::Local => BidirectionalQueueEndpoint::Local { root: root_string },
        ProviderArg::Sftp => {
            let profile = load_sftp_profiles(&config_dir.join("sftp-profiles.json"))?
                .into_iter()
                .find(|profile| Some(profile.id.as_str()) == profile_id)
                .ok_or_else(|| missing_profile("SFTP"))?;
            BidirectionalQueueEndpoint::Sftp {
                connection: Box::new(profile.connection),
                root: root_string,
            }
        }
        ProviderArg::Ftps => {
            let profile = load_ftps_profiles(&config_dir.join("ftps-profiles.json"))?
                .into_iter()
                .find(|profile| Some(profile.id.as_str()) == profile_id)
                .ok_or_else(|| missing_profile("FTPS"))?;
            BidirectionalQueueEndpoint::Ftps {
                connection: Box::new(profile.connection),
                root: root_string,
            }
        }
        ProviderArg::Webdav => {
            let profile = load_webdav_profiles(&config_dir.join("webdav-profiles.json"))?
                .into_iter()
                .find(|profile| Some(profile.id.as_str()) == profile_id)
                .ok_or_else(|| missing_profile("WebDAV"))?;
            BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(profile.connection),
                root: root_string,
            }
        }
        ProviderArg::Smb => {
            let profile = load_smb_profiles(&config_dir.join("smb-profiles.json"))?
                .into_iter()
                .find(|profile| Some(profile.id.as_str()) == profile_id)
                .ok_or_else(|| missing_profile("SMB"))?;
            BidirectionalQueueEndpoint::Smb {
                connection: Box::new(profile.connection),
                root: root_string,
            }
        }
    };
    // Die gespeicherte Verbindung bleibt hier vollständig geheimnisfrei: Der
    // Vertrag prüft Host, Pin, Authentifizierungsreferenz und Root, greift aber noch
    // nicht auf Schlüsselbund oder Netzwerk zu.
    queue_endpoint.validate()?;
    Ok(queue_endpoint)
}

fn connect_resolved_endpoint(
    spec: &EndpointSpec,
    queue_endpoint: BidirectionalQueueEndpoint,
) -> io::Result<ResolvedEndpoint> {
    let endpoint = queue_endpoint.connect(Arc::new(KeyringResolver))?;
    Ok(ResolvedEndpoint {
        endpoint,
        queue_endpoint,
        provider_label: spec.provider.label(),
        profile_id: spec.profile.clone(),
    })
}

fn resolve_endpoint(spec: &EndpointSpec, config_dir: &Path) -> io::Result<ResolvedEndpoint> {
    let queue_endpoint = resolve_queue_endpoint(spec, config_dir)?;
    connect_resolved_endpoint(spec, queue_endpoint)
}

fn validate_endpoint_spec(spec: &EndpointSpec) -> io::Result<PathBuf> {
    match (spec.provider, spec.profile.as_deref()) {
        (ProviderArg::Local, Some(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Lokale Endpunkte besitzen kein Remote-Profil",
            ));
        }
        (ProviderArg::Local, None) => {}
        (_, Some(profile)) if !profile.trim().is_empty() => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Remote-Endpunkte benötigen eine nicht leere Profil-ID",
            ));
        }
    }
    validate_provider_sync_root(spec.provider, &spec.root)
}

fn missing_profile(provider: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{provider}-Profil-ID fehlt oder wurde nicht gefunden"),
    )
}

impl ProviderArg {
    fn kind(self) -> doppeldecker_core::ProviderKind {
        match self {
            Self::Local => doppeldecker_core::ProviderKind::Local,
            Self::Sftp => doppeldecker_core::ProviderKind::Sftp,
            Self::Ftps => doppeldecker_core::ProviderKind::Ftps,
            Self::Webdav => doppeldecker_core::ProviderKind::WebDav,
            Self::Smb => doppeldecker_core::ProviderKind::Smb,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Sftp => "sftp",
            Self::Ftps => "ftps",
            Self::Webdav => "webdav",
            Self::Smb => "smb",
        }
    }
}

fn lexical_provider_path(provider: ProviderArg, path: &Path) -> PathBuf {
    if provider == ProviderArg::Local {
        expand_home_path(path)
    } else {
        path.to_path_buf()
    }
}

fn provider_file_command(
    kind: FileQueueKind,
    args: ProviderFileArgs,
    config_dir: &Path,
    output: &mut Output,
) -> io::Result<i32> {
    let lexical_source_root = lexical_provider_path(args.source_provider, &args.source_root);
    let lexical_target_root = lexical_provider_path(args.target_provider, &args.target_root);
    let lexical_items = args
        .items
        .iter()
        .map(|path| lexical_provider_path(args.source_provider, path))
        .collect::<Vec<_>>();
    let (source_root, item_paths) = if args.source_provider == ProviderArg::Local {
        doppeldecker_core::fileops::bind_local_file_operation_scope(
            &lexical_source_root,
            &lexical_items,
        )?
    } else {
        doppeldecker_core::fileops::validate_file_operation_scope(
            &lexical_source_root,
            &lexical_items,
        )?
    };
    let mut target_root =
        doppeldecker_core::fileops::validate_file_operation_root(&lexical_target_root)?;
    // Nur der lokale Pane-Root wird kanonisiert. Die Auswahl selbst bleibt als
    // lstat-gebundener letzter Name erhalten; sonst würde ein F6 auf einen Symlink
    // unbemerkt dessen Ziel verschieben. Der Queue-Executor bindet dieselben Pfade
    // später erneut an seinen frisch geöffneten Root-Deskriptor.
    if args.target_provider == ProviderArg::Local {
        target_root = std::fs::canonicalize(&target_root)?;
        target_root = doppeldecker_core::fileops::validate_file_operation_root(&target_root)?;
    }
    let source_spec = EndpointSpec {
        provider: args.source_provider,
        profile: args.source_profile,
        root: source_root.clone(),
    };
    let target_spec = EndpointSpec {
        provider: args.target_provider,
        profile: args.target_profile,
        root: target_root.clone(),
    };
    validate_endpoint_spec(&source_spec)?;
    validate_endpoint_spec(&target_spec)?;
    doppeldecker_core::validate_interactive_file_transfer_request(
        source_spec.provider.kind(),
        target_spec.provider.kind(),
        kind == FileQueueKind::Move,
        args.replace,
    )?;
    if source_spec.provider == ProviderArg::Sftp {
        if target_spec.provider != ProviderArg::Local || kind != FileQueueKind::Copy || args.replace
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SFTP-Inhalte erlauben ausschließlich additive lokale Downloads",
            ));
        }
        doppeldecker_core::sftp::validate_interactive_content_selection(&source_root, &item_paths)?;
    }
    // Erst beide Profile prüfen, dann Prozessbesitz und Verbindungen erwerben.
    // Der additive WebDAV-Upload behält seine Bindung an die Profilrevision.
    let source_endpoint = resolve_queue_endpoint(&source_spec, config_dir)?;
    let (target_endpoint, profile_bindings) = if source_spec.provider == ProviderArg::Local
        && target_spec.provider == ProviderArg::Webdav
        && kind == FileQueueKind::Copy
        && !args.replace
    {
        let profile = doppeldecker_core::load_webdav_profiles_versioned(
            &config_dir.join("webdav-profiles.json"),
        )?
        .into_iter()
        .find(|profile| Some(profile.profile.id.as_str()) == target_spec.profile.as_deref())
        .ok_or_else(|| missing_profile("WebDAV"))?;
        let binding = doppeldecker_core::RemoteProfileBinding::new(
            doppeldecker_core::RemoteProfileProtocol::Webdav,
            &profile.profile.id,
            profile.revision,
        )?;
        let endpoint = BidirectionalQueueEndpoint::WebDav {
            connection: Box::new(profile.profile.connection),
            root: doppeldecker_core::fileops::path_to_utf8(&target_root)?,
        };
        endpoint.validate()?;
        (endpoint, vec![binding])
    } else {
        (
            resolve_queue_endpoint(&target_spec, config_dir)?,
            Vec::new(),
        )
    };
    // Dieselbe Queue-Instanz schützt Verbindung und Capability-Proben und trägt
    // anschließend Enqueue und Claim.
    let queue = open_cli_queue(config_dir)?;
    let mut source = connect_resolved_endpoint(&source_spec, source_endpoint)?;
    let mut target = connect_resolved_endpoint(&target_spec, target_endpoint)?;
    source.endpoint = queue.bind_provider_preparation(&source.endpoint)?;
    target.endpoint = queue.bind_provider_preparation(&target.endpoint)?;
    let (bound_source_root, bound_items) =
        doppeldecker_core::fileops::validate_file_operation_scope(
            source.endpoint.root(),
            &item_paths,
        )?;
    let bound_target_root =
        doppeldecker_core::fileops::validate_file_operation_root(target.endpoint.root())?;
    if bound_source_root != source_root
        || bound_items != item_paths
        || bound_target_root != target_root
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Dateipfade stimmen nicht mit den frisch gebundenen Pane-Roots überein",
        ));
    }
    source.endpoint.validate_interactive_file_transfer(
        &target.endpoint,
        kind == FileQueueKind::Move,
        args.replace,
    )?;
    let doppeldecker_core::InteractiveFileContexts {
        source_vfs,
        target_vfs,
        target_capabilities: target_capabilities_override,
    } = source.endpoint.interactive_file_contexts(
        &target.endpoint,
        &item_paths,
        kind == FileQueueKind::Move,
        args.replace,
        &Cancel::new(),
    )?;
    let target_root = target.endpoint.root().to_path_buf();
    let source_tree = source_vfs.interactive_source_tree()?;
    for limitation in source.endpoint.interactive_file_read_limitations() {
        eprintln!("{limitation}");
    }
    for limitation in source_tree
        .as_ref()
        .into_iter()
        .flat_map(|tree| tree.limitations())
    {
        eprintln!("{limitation}");
    }
    doppeldecker_core::fileops::validate_file_operation_paths(&item_paths, &target_root)?;
    let conflicts = find_recursive_conflicts(
        source_vfs.as_ref(),
        target_vfs.as_ref(),
        &target_root,
        &item_paths,
    )?;
    let conflict_proofs = if args.replace {
        capture_file_conflict_proofs(target_vfs.as_ref(), &conflicts)?
    } else {
        Vec::new()
    };
    // Namespace-Gleichheit stammt aus den lebenden Root-Identitäten. Profil-ID und
    // serialisierte Konfiguration entscheiden nur, ob die Queue dieselbe Sitzung als
    // Rename-Optimierung wiederverwenden kann.
    let same_runtime_namespace = source.endpoint.shares_runtime_namespace(&target.endpoint)?;
    let shared_rename_session = same_runtime_namespace
        && source
            .queue_endpoint
            .shares_connection_definition(&target.queue_endpoint);
    let mut all_top_level_targets_missing = true;
    for source_path in &item_paths {
        let name = source_path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Auswahl enthält einen Eintrag ohne sicheren Namen",
            )
        })?;
        match target_vfs.stat(&target_root.join(name)) {
            Ok(_) => all_top_level_targets_missing = false,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let prepared_target = source.endpoint.probe_interactive_file_target(
        doppeldecker_core::ProviderFileTargetProbe {
            target: &target.endpoint,
            items: &item_paths,
            kind,
            overwrite: args.replace,
            skip_conflicts: args.skip_conflicts && !args.replace,
            inspect_conflicts: !args.skip_conflicts && !args.replace,
            expected_conflicts: Some(&conflicts),
            shared_session: shared_rename_session,
            targets_missing: all_top_level_targets_missing,
            capabilities_override: target_capabilities_override,
        },
    )?;
    if prepared_target.choice_required {
        output.event(
            serde_json::json!({"event":"conflicts", "conflicts":conflicts, "requires_choice":true}),
        )?;
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Zielkonflikte vorhanden; mit --skip-conflicts überspringen oder Ersetzen ausdrücklich wählen; Fähigkeiten werden erst für die gewählte Wirkung geprüft"));
    }
    let skip_only = prepared_target.skip_only;
    let source_removal_required = prepared_target.source_removal_required;
    let same_namespace_move = prepared_target.rename_only;
    let mut target_capabilities = prepared_target.capabilities;
    if source.endpoint.kind() == doppeldecker_core::ProviderKind::Sftp {
        if let Some(capabilities) = &mut target_capabilities {
            capabilities.replace = false;
            capabilities.replace_best_effort = false;
        }
    }
    if !skip_only
        && target_capabilities.as_ref().is_some_and(|capabilities| {
            !capabilities.copy_new && !capabilities.copy_new_best_effort
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            target_capabilities
                .as_ref()
                .expect("zuvor als Some geprüft")
                .blockers
                .join("; "),
        ));
    }
    // Ein nur als Best Effort möglicher Commit ist ein reguläres Ergebnis, aber
    // sichtbar: Die konkrete Einschränkung geht vor der Ausführung an stderr.
    if let Some(capabilities) = target_capabilities
        .as_ref()
        .filter(|capabilities| !capabilities.copy_new && capabilities.copy_new_best_effort)
    {
        for limitation in &capabilities.limitations {
            eprintln!("Best Effort: {limitation}");
        }
    }
    let source_capabilities = if kind == FileQueueKind::Move
        && !same_namespace_move
        && source_removal_required
        && !skip_only
    {
        Some(source.endpoint.probe_file_source_removal_capabilities()?)
    } else {
        None
    };
    if source_capabilities.as_ref().is_some_and(|capabilities| {
        !capabilities.remove_source && !capabilities.remove_source_best_effort
    }) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Zielkopie wäre möglich, aber die Quellentfernung ist weder provideratomar noch als begrenzter Best-Effort-Pfad verfügbar",
        ));
    }
    if let Some(capabilities) = source_capabilities.as_ref().filter(|capabilities| {
        !capabilities.remove_source && capabilities.remove_source_best_effort
    }) {
        // Die Bestätigung folgt erst danach. So erfährt auch die menschliche
        // Ausgabe vor der Wirkung, welche schwächere Quellentfernung sie bestätigt.
        for limitation in &capabilities.limitations {
            eprintln!("Best Effort: {limitation}");
        }
    }
    if !skip_only
        && args.replace
        && target_capabilities
            .as_ref()
            .is_none_or(|capabilities| !capabilities.replace && !capabilities.replace_best_effort)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Ersetzen ist für diese Zielverbindung weder mit Quarantäne noch als bestätigtes Best Effort gebunden",
        ));
    }
    if !conflicts.is_empty() && !args.skip_conflicts && !args.replace {
        output.event(serde_json::json!({
            "event": "conflicts",
            "conflicts": conflicts,
            "replace": target_capabilities.as_ref().is_some_and(|value| value.replace || value.replace_best_effort),
        }))?;
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Zielkonflikte vorhanden; mit --skip-conflicts ausschließlich neue Namen kopieren",
        ));
    }
    // Die verlangten Bestätigungsphrasen entstehen vor der Vorschau und fahren in
    // ihr mit. Sonst müsste ein `--json`-Controller sie aus dem deutschen
    // stderr-Text lesen oder aus den eigenen Argumenten rekonstruieren; das
    // `file_preview`-Ereignis nannte bisher nicht einmal die Profil-IDs, aus denen
    // die Phrase gebildet wird.
    let best_effort_replace = target_capabilities
        .as_ref()
        .is_some_and(|value| !value.replace && value.replace_best_effort);
    let replace_confirmation = (args.replace && !conflicts.is_empty()).then(|| {
        format!(
            "{} {} {} {}",
            if best_effort_replace {
                "DAUERHAFT ERSETZEN"
            } else {
                "ERSETZEN"
            },
            target.provider_label,
            target.profile_id.as_deref().unwrap_or("-"),
            conflicts.len()
        )
    });
    let move_confirmation = (kind == FileQueueKind::Move && source_removal_required && !skip_only)
        .then(|| {
            format!(
                "VERSCHIEBEN {} {} {}",
                source.provider_label,
                source.profile_id.as_deref().unwrap_or("-"),
                item_paths.len()
            )
        });
    let local_copy_confirmation = if target.endpoint.kind()
        == doppeldecker_core::ProviderKind::Local
        && target_capabilities
            .as_ref()
            .is_some_and(|value| !value.copy_new && value.copy_new_best_effort)
    {
        Some((
            target_vfs.local_root_identity(&target_root)?,
            format!("BEST EFFORT LOKAL {}", target_root.display()),
        ))
    } else {
        None
    };
    output.event(serde_json::json!({
        "event": "file_preview",
        "skip_only": skip_only,
        "source_removal_required": source_removal_required,
        "kind": format!("{kind:?}").to_lowercase(),
        "source_provider": source.provider_label,
        "source_profile_id": source.profile_id,
        "target_provider": target.provider_label,
        "target_profile_id": target.profile_id,
        "items": item_paths,
        "conflicts": conflicts,
        "target_capabilities": target_capabilities,
        "source_capabilities": source_capabilities,
        "source_tree": source_tree,
        "replace_confirmation": replace_confirmation,
        "move_confirmation": move_confirmation,
        "local_copy_new_confirmation": local_copy_confirmation.as_ref().map(|(_, phrase)| phrase),
    }))?;
    if let Some((_, phrase)) = &local_copy_confirmation {
        require_stdin_confirmation(phrase)?;
    }
    if let Some(phrase) = &replace_confirmation {
        if best_effort_replace && !output.json {
            eprintln!(
                "Best Effort: Die {} bestätigte(n) Altdatei(en) werden ohne Quarantäne dauerhaft ersetzt; ein unklarer Ausgang wird nicht wiederholt.",
                conflicts.len()
            );
        }
        require_stdin_confirmation(phrase)?;
    }
    if let Some(phrase) = &move_confirmation {
        require_stdin_confirmation(phrase)?;
    }
    let operation = QueueOperation::ProviderFile {
        kind,
        source: Box::new(source.queue_endpoint),
        target: Box::new(target.queue_endpoint),
        items: item_paths
            .iter()
            .map(|path| doppeldecker_core::fileops::path_to_utf8(path))
            .collect::<io::Result<Vec<_>>>()?,
        destination: doppeldecker_core::fileops::path_to_utf8(&target_root)?,
        overwrite: args.replace,
        expected_conflicts: conflicts,
        source_tree_expected: source_tree.map(Box::new),
        keep_both_targets: Vec::new(),
        expected_conflict_proofs: conflict_proofs,
        best_effort_commit_expected: None,
        local_copy_new_best_effort_expected: local_copy_confirmation
            .map(|(identity, _)| Box::new(identity)),
        copy_new_best_effort_expected: target_capabilities
            .as_ref()
            .map(|capabilities| !capabilities.copy_new && capabilities.copy_new_best_effort),
        replace_best_effort_expected: target_capabilities.as_ref().map(|capabilities| {
            args.replace && !capabilities.replace && capabilities.replace_best_effort
        }),
        remove_source_best_effort_expected: source_capabilities.as_ref().map(|capabilities| {
            !capabilities.remove_source && capabilities.remove_source_best_effort
        }),
    };
    // Einreihen und Claim sind eine einzige persistente Zustandsänderung. So kann
    // selbst ein unklarer Fehler nach dem Journal-Rename nie `Pending` hinterlassen.
    let claimed = queue.enqueue_claimed_with_profile_bindings(operation, profile_bindings)?;
    let operation_id = claimed.operation_id.clone();
    let cancel = install_cancel_handler()?;
    let execution = LocalQueueExecution {
        sandbox: None,
        sync_quarantine: None,
        max_delete_entries: 100,
        max_delete_fraction: 0.25,
    };
    let result = doppeldecker_core::queue::execute_provider_file_queue_entry_detailed(
        &queue,
        claimed,
        &execution,
        Arc::new(KeyringResolver),
        &cancel,
        &mut |progress| emit_progress(output, progress),
    );
    let result = match result {
        Ok(result) => result,
        Err(mut failure) => {
            // Der Executor hält den letzten Callback-Wert unabhängig vom Journal.
            // Genau der Commit dieses Werts kann gescheitert sein; ein Snapshot
            // würde dann auf die ältere persistierte Teilbilanz zurückfallen.
            let progress = (*failure.progress).clone();
            if let Some(completion) = failure.pending_completion.as_deref().cloned() {
                match queue.retry_file_completion(&operation_id, &completion) {
                    Ok(()) => {
                        failure.journal_errors.clear();
                        failure.pending_completion = None;
                        match queue.append_file_completion_audit(&operation_id, &completion) {
                            Ok(()) => {
                                return emit_queue_result(
                                    output,
                                    &operation_id,
                                    QueueExecutionResult::File(completion.stats),
                                );
                            }
                            Err(audit_error) => failure.error = audit_error,
                        }
                    }
                    Err(journal_error) => failure.journal_errors.push(journal_error),
                }
            } else {
                match persist_executor_failure(
                    &queue,
                    &operation_id,
                    progress.clone(),
                    &failure.error,
                    Some(&failure.actuals),
                ) {
                    Ok(()) => {
                        // Ein zweiter, nachweislich erfolgreicher Commit macht
                        // frühere Journalfehler historisch. Der fachliche Fehler
                        // bleibt der primäre CLI-Code und die strukturierte Bilanz
                        // unverändert.
                        failure.journal_errors.clear();
                    }
                    Err(journal_error) => failure.journal_errors.push(journal_error),
                }
            }
            output.provider_file_error(
                &failure.error,
                &failure.actuals,
                &progress,
                failure.recovery.as_ref(),
                &failure.journal_errors,
            );
            return Ok(2);
        }
    };
    emit_queue_result(output, &operation_id, result)
}

/// Schreibt einen Executorfehler ins Queue-Journal und unterscheidet dabei zwei
/// grundverschiedene Ursachen für ein fehlgeschlagenes `fail`: Ein bereits
/// terminaler Eintrag braucht keinen zweiten Abschluss. Ein offener Eintrag mit
/// Schreibfehler ist dagegen nicht sicher abgeschlossen; dieser Journalfehler muss
/// den ursprünglichen Ausführungsfehler überstimmen.
fn persist_executor_failure(
    queue: &OperationQueue,
    operation_id: &str,
    progress: Progress,
    error: &io::Error,
    actuals: Option<&doppeldecker_core::fileops::FileOperationActuals>,
) -> io::Result<()> {
    let result = match actuals {
        Some(actuals) => queue.fail_file_actuals_coded(operation_id, progress, error, actuals),
        None => queue.fail_coded(operation_id, progress, error),
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let already_terminal = error.kind() == io::ErrorKind::InvalidInput
                && queue.snapshot().entries.iter().any(|entry| {
                    entry.operation_id == operation_id
                        && matches!(
                            entry.state,
                            doppeldecker_core::QueueState::Completed
                                | doppeldecker_core::QueueState::Failed
                                | doppeldecker_core::QueueState::Cancelled
                        )
                });
            if already_terminal {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

/// Finalisiert einen terminalen Executorfehler wie der GUI-Worker: Der eigene
/// Claim wird mit dem letzten Fortschritt als `Failed` geschlossen. Ohne diesen
/// Schritt bliebe der Eintrag `Validating`/`Running` liegen und würde beim
/// nächsten Öffnen der Queue zu einem blockierenden `Interrupted`.
fn finalize_executor_result(
    queue: &OperationQueue,
    operation_id: &str,
    result: io::Result<doppeldecker_core::QueueExecutionResult>,
) -> io::Result<doppeldecker_core::QueueExecutionResult> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            let progress = queue
                .snapshot()
                .entries
                .iter()
                .find(|entry| entry.operation_id == operation_id)
                .map(|entry| entry.progress.clone())
                .unwrap_or_default();
            persist_executor_failure(queue, operation_id, progress, &error, None)?;
            Err(error)
        }
    }
}

fn rename_command(args: RenameArgs, config_dir: &Path, output: &mut Output) -> io::Result<i32> {
    let lexical_root = lexical_provider_path(args.endpoint.provider, &args.endpoint.root);
    let lexical_path = lexical_provider_path(args.endpoint.provider, &args.path);
    // Namen und rein lexikalisch prüfbare Pfadgrenzen brauchen weder Queue noch
    // Provider. Eine ungültige Eingabe darf deshalb vorher kein Journal öffnen,
    // kein Secret lesen und keine Verbindung aufbauen.
    let contract = doppeldecker_core::ordinary_rename_contract(args.endpoint.provider.kind());
    let expected_class = match &contract {
        doppeldecker_core::RenameInteractionContract::Confirmed {
            execution_class, ..
        } => Some(*execution_class),
        _ => None,
    };
    let new_name = doppeldecker_core::validate_ordinary_rename_request(
        args.endpoint.provider.kind(),
        &lexical_root,
        &lexical_path,
        &args.new_name,
        expected_class,
    )?;
    let (bound_root, bound_path) = if args.endpoint.provider == ProviderArg::Local {
        let (root, mut paths) = doppeldecker_core::fileops::bind_local_file_operation_scope(
            &lexical_root,
            std::slice::from_ref(&lexical_path),
        )?;
        (root, paths.remove(0))
    } else {
        (lexical_root, lexical_path)
    };
    let spec = EndpointSpec {
        provider: args.endpoint.provider,
        profile: args.endpoint.profile,
        root: bound_root,
    };
    validate_endpoint_spec(&spec)?;
    // Prozessbesitz vor Verbindung, Probe und Mutation (siehe mkdir_command).
    let _queue_lock = open_cli_queue(config_dir)?;
    let mut resolved = resolve_endpoint(&spec, config_dir)?;
    resolved.endpoint = _queue_lock.bind_provider_preparation(&resolved.endpoint)?;
    let path = bound_path;
    if !path.starts_with(resolved.endpoint.root()) || path == resolved.endpoint.root() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Rename-Pfad verlässt den gebundenen Root",
        ));
    }
    if let doppeldecker_core::RenameInteractionContract::Confirmed { limitations, .. } = contract {
        eprintln!("Umbenennen · Best Effort");
        for limitation in limitations {
            eprintln!("{limitation}");
        }
    }
    // Der Kern verwendet genau die zuvor angezeigte Wirkungsklasse. WebDAV
    // benutzt seine bedingte Einzelwirkung, andere Provider die bisherigen Proben.
    let report =
        resolved
            .endpoint
            .execute_ordinary_rename(&path, &new_name, &None, expected_class)?;
    match report.outcome {
        doppeldecker_core::RenameOutcome::Collision => {
            let existing = match report.existing_is_dir {
                Some(true) => "ein vorhandener Ordner",
                Some(false) => "eine vorhandene Datei",
                None => "ein vorhandener Eintrag",
            };
            Err(doppeldecker_core::coded_error(
                io::ErrorKind::AlreadyExists,
                doppeldecker_core::ErrorCode::AlreadyExists,
                format!(
                    "„{}“ ist bereits belegt ({existing}); es wurde nichts verändert",
                    new_name
                ),
            ))
        }
        doppeldecker_core::RenameOutcome::Renamed => {
            let renamed = report.new_path.clone().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Rename-Report ohne neuen Pfad")
            })?;
            output.event(serde_json::json!({
                "event": "rename_result",
                "outcome": report.outcome,
                "class": report.class,
                "limitations": report.limitations,
                "verified_after_reconnect": report.verified_after_reconnect,
                "path": renamed,
            }))?;
            let class_note = match report.class {
                doppeldecker_core::RenameExecutionClass::Assured => String::new(),
                doppeldecker_core::RenameExecutionClass::BestEffort => {
                    format!(" (Best Effort: {})", report.limitations.join("; "))
                }
            };
            output.text(format!("Umbenannt: {}{class_note}", renamed.display()));
            Ok(0)
        }
    }
}

fn bind_delete_preview_around_probe<T>(
    vfs: &dyn doppeldecker_core::Vfs,
    root: &Path,
    items: &[PathBuf],
    probe: impl FnOnce() -> io::Result<T>,
) -> io::Result<(doppeldecker_core::RemoteDeletePlan, T)> {
    let plan = plan_remote_delete(vfs, root, items)?;
    let result = probe()?;
    if plan_remote_delete(vfs, root, items)? != plan {
        return Err(doppeldecker_core::coded_error(
            io::ErrorKind::Other,
            doppeldecker_core::ErrorCode::PlanDrift,
            "Provider-Löschroot oder Auswahl änderten sich während der Capability-Probe",
        ));
    }
    Ok((plan, result))
}

fn delete_command(args: DeleteArgs, config_dir: &Path, output: &mut Output) -> io::Result<i32> {
    let lexical_root = lexical_provider_path(args.endpoint.provider, &args.endpoint.root);
    let lexical_items = args
        .items
        .iter()
        .map(|path| lexical_provider_path(args.endpoint.provider, path))
        .collect::<Vec<_>>();
    let (root_input, item_paths) =
        doppeldecker_core::fileops::validate_file_operation_scope(&lexical_root, &lexical_items)?;
    let spec = EndpointSpec {
        provider: args.endpoint.provider,
        profile: args.endpoint.profile,
        root: root_input.clone(),
    };
    validate_endpoint_spec(&spec)?;
    if args.endpoint.provider == ProviderArg::Local {
        // Auch der lokale, anschließend bewusst abgelehnte Löschweg bindet den
        // letzten Namen per lstat. Ein Symlinkziel wird niemals durch
        // `canonicalize` zu einer anderen Auswahl umgedeutet.
        doppeldecker_core::fileops::bind_local_file_operation_scope(&root_input, &item_paths)?;
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Lokale Einträge verwenden ausschließlich den Systempapierkorb",
        ));
    }
    // Prozessbesitz vor Verbindung und mutierender Löschprobe (siehe
    // mkdir_command); dieselbe Queue-Instanz trägt anschließend Enqueue und Claim.
    let queue = open_cli_queue(config_dir)?;
    let resolved = resolve_endpoint(&spec, config_dir)?;
    let (vfs, root) = resolved.endpoint.file_context()?;
    let (bound_root, bound_items) =
        doppeldecker_core::fileops::validate_file_operation_scope(&root, &item_paths)?;
    if bound_root != root_input || bound_items != item_paths {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Löschpfade stimmen nicht mit dem frisch gebundenen Pane-Root überein",
        ));
    }
    // Rootidentität und Löschplan werden rein lesend gebunden, bevor eine
    // Capability-Probe kurzlebige Providernamen anlegen könnte.
    let (plan, capabilities) =
        bind_delete_preview_around_probe(vfs.as_ref(), &root, &item_paths, || {
            resolved.endpoint.probe_delete_capabilities()
        })?;
    let mode = match args.mode {
        DeleteModeArg::Quarantine if capabilities.quarantine => RemoteDeleteMode::Quarantine,
        DeleteModeArg::Permanent if capabilities.permanent => RemoteDeleteMode::Permanent,
        DeleteModeArg::Quarantine => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "Doppeldecker-Quarantäne ist nicht verfügbar; kein stiller Fallback: {}",
                    capabilities.blockers.join("; ")
                ),
            ))
        }
        DeleteModeArg::Permanent => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                capabilities.blockers.join("; "),
            ))
        }
    };
    let effect = if mode == RemoteDeleteMode::Quarantine {
        "QUARANTÄNISIEREN"
    } else {
        "DAUERHAFT LÖSCHEN"
    };
    let phrase = format!(
        "{effect} {} {} {} {} {}",
        resolved.provider_label.to_uppercase(),
        resolved.profile_id.as_deref().unwrap_or("-"),
        plan.root,
        plan.items.len(),
        plan.entries
    );
    output.event(serde_json::json!({
        "event": "delete_preview",
        "mode": mode,
        "provider": resolved.provider_label,
        "profile": resolved.profile_id,
        "plan": plan,
        "capabilities": capabilities,
        "confirmation": phrase,
    }))?;
    require_stdin_confirmation(&phrase)?;
    let operation = QueueOperation::ProviderDelete {
        endpoint: Box::new(resolved.queue_endpoint),
        mode,
        expected_plan: Box::new(plan),
    };
    let claimed = queue.enqueue_claimed(operation)?;
    let operation_id = claimed.operation_id.clone();
    let cancel = install_cancel_handler()?;
    let result = execute_provider_delete_queue_entry(
        &queue,
        claimed,
        Arc::new(KeyringResolver),
        &cancel,
        &mut |progress| emit_progress(output, progress),
    );
    let result = finalize_executor_result(&queue, &operation_id, result)?;
    emit_queue_result(output, &operation_id, result)
}

fn trash_command(args: TrashArgs, config_dir: &Path, output: &mut Output) -> io::Result<i32> {
    // Der Prozesslock wird vor Recovery und erster Pfadprüfung genommen. Damit kann
    // die GUI nicht gleichzeitig dasselbe Crash-Journal oder dieselben Quellnamen
    // beanspruchen. Der eigentliche Papierkorbvertrag bleibt vollständig im Core.
    let _queue_lock = open_cli_queue(config_dir)?;
    let current_dir = std::env::current_dir()?;
    let items = args
        .items
        .iter()
        .map(|item| -> io::Result<PathBuf> {
            let expanded = expand_home_path(item);
            let absolute = if expanded.is_absolute() {
                expanded
            } else {
                current_dir.join(expanded)
            };
            canonicalize_parent_preserving_final_entry(&absolute)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let journal_dir = config_dir.join("trash-journals");
    let recovery = recover_trash_journals(&journal_dir)?;
    if !recovery.is_clean() {
        return Err(io::Error::other(format!(
            "Papierkorb-Recovery ist noch ungeklärt: {}{}",
            recovery.recovery_paths.join(", "),
            recovery.errors.join("; ")
        )));
    }
    let stats = trash_items(&items, &None, &journal_dir)?;
    // Ein unklarer Ausgang ist genauso wenig ein Erfolg wie ein Fehlschlag.
    let complete = stats.failed == 0
        && stats.unclear == 0
        && stats.recovery_paths.is_empty()
        && stats.errors.is_empty();
    output.event(serde_json::json!({
        "event": "trash_result",
        "stats": stats,
        "recovery": recovery,
    }))?;
    // Die drei Zahlen ergeben zusammen die Gesamtzahl der Eintraege; ein unklarer
    // Ausgang bleibt sichtbar, statt in einer der beiden anderen Zahlen zu verschwinden.
    output.text(format!(
        "In den Papierkorb verschoben: {}; fehlgeschlagen: {}; Ausgang unklar: {}",
        stats.trashed, stats.failed, stats.unclear
    ));
    Ok(if complete { 0 } else { 2 })
}

fn capabilities_command(
    args: EndpointArgs,
    config_dir: &Path,
    output: &mut Output,
) -> io::Result<i32> {
    let mut spec = EndpointSpec {
        provider: args.provider,
        profile: args.profile,
        root: args.root,
    };
    // Provider/Profil und der absolute, normalisierte Root sind reine Eingaben.
    // Sie müssen scheitern, bevor das Queuejournal angelegt oder migriert wird und
    // lange bevor Profilauflösung, Secrets, Netzwerk oder Mutationsproben beginnen.
    spec.root = validate_endpoint_spec(&spec)?;
    // Auch das gespeicherte Profil und der vollständige Core-Endpunktvertrag sind
    // reine Daten. Fehlende oder ungültige Profile dürfen deshalb noch kein
    // Queuejournal erzeugen oder migrieren.
    let queue_endpoint = resolve_queue_endpoint(&spec, config_dir)?;
    // Auch reine Capability-Abfragen proben mutierend (künstliche Einträge unter
    // dem Root) und brauchen deshalb den Prozessbesitz (siehe mkdir_command).
    let _queue_lock = open_cli_queue(config_dir)?;
    let mut resolved = connect_resolved_endpoint(&spec, queue_endpoint)?;
    resolved.endpoint = _queue_lock.bind_provider_preparation(&resolved.endpoint)?;
    let mut probe_blocker = None;
    let directory = capability_result(run_capability_probe(&mut probe_blocker, || {
        resolved.endpoint.probe_directory_create_capability()
    }));
    // Rename meldet seine Ausführungsklasse wirkungsspezifisch: `available` heißt
    // „mindestens eine Klasse (abgesichert oder Best Effort) ist bewiesen".
    let rename = match run_capability_probe(&mut probe_blocker, || {
        resolved.endpoint.probe_rename_capabilities()
    }) {
        Ok(details) => {
            let available = details.execution_class().is_some();
            serde_json::json!({ "available": available, "details": details })
        }
        Err(error) => {
            capability_result::<doppeldecker_core::ProviderRenameCapabilities>(Err(error))
        }
    };
    let files = capability_result(run_capability_probe(&mut probe_blocker, || {
        resolved.endpoint.probe_file_capabilities()
    }));
    let delete = capability_result(run_capability_probe(&mut probe_blocker, || {
        resolved.endpoint.probe_delete_capabilities()
    }));
    output.event(serde_json::json!({
        "event": "capabilities",
        "provider": resolved.provider_label,
        "profile": resolved.profile_id,
        "root": resolved.endpoint.root(),
        "directory_create": directory,
        "rename": rename,
        "interactive_rename": doppeldecker_core::ordinary_rename_contract(resolved.endpoint.kind()),
        "files": files,
        "delete": delete,
    }))?;
    if let Some(error) = probe_blocker {
        return Err(error);
    }
    output.text(format!(
        "Fähigkeiten für {} {} geprüft",
        resolved.provider_label,
        resolved.profile_id.as_deref().unwrap_or("-")
    ));
    Ok(0)
}

fn run_capability_probe<T>(
    blocker: &mut Option<io::Error>,
    probe: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    use doppeldecker_core::{
        coded_error, error::recovery_paths, error::with_recovery_paths, error_code, ErrorCode,
    };
    if let Some(previous) = blocker.as_ref() {
        return Err(with_recovery_paths(
            coded_error(
                previous.kind(),
                error_code(previous),
                format!("Nicht geprüft: Vorherige Fähigkeitsprobe blieb unklar oder verlor ihre Verbindung: {previous}"),
            ),
            &recovery_paths(previous),
        ));
    }
    let result = probe();
    if let Err(error) = &result {
        if matches!(
            error_code(error),
            ErrorCode::OutcomeUnclear | ErrorCode::ConnectionLost
        ) {
            *blocker = Some(with_recovery_paths(
                coded_error(error.kind(), error_code(error), error.to_string()),
                &recovery_paths(error),
            ));
        }
    }
    result
}

fn capability_result<T: serde::Serialize>(result: io::Result<T>) -> serde_json::Value {
    match result {
        Ok(value) => serde_json::json!({ "available": true, "details": value }),
        Err(error) => serde_json::json!({
            "available": false,
            "code": doppeldecker_core::error_code(&error).as_str(),
            "reason": error.to_string(),
            "recovery_paths": doppeldecker_core::error::recovery_paths(&error),
        }),
    }
}

fn require_stdin_confirmation(phrase: &str) -> io::Result<()> {
    eprintln!("Exakt über stdin bestätigen: {phrase}");
    if read_stdin_line()? != phrase {
        return Err(doppeldecker_core::coded_error(
            io::ErrorKind::PermissionDenied,
            doppeldecker_core::ErrorCode::ConfirmationMismatch,
            "Bestätigung stimmt nicht exakt überein",
        ));
    }
    Ok(())
}

fn open_cli_queue(config_dir: &Path) -> io::Result<OperationQueue> {
    use std::os::unix::fs::DirBuilderExt;
    // Neu erzeugte Konfiguration enthält auch private Recovery-Belege. Die
    // Anlage mit 0700 verhindert fremde Schreibrechte auch bei großzügiger
    // Umask; bestehende Verzeichnisse und ihre ACLs bleiben unverändert.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(config_dir)?;
    OperationQueue::open(config_dir.join("operations-queue.json"))
}

/// Sperrt eine unmittelbar wirkende CLI-Operation, solange die Queue noch etwas
/// Offenes hält oder pausiert ist.
///
/// `wirkung` benennt den Aufrufer, damit die Meldung sagt, was gerade gesperrt ist.
/// Der Schutz gilt für **jede** unmittelbar mutierende CLI-Wirkung, nicht nur für den
/// lokalen Realmodus: Ein wartender Eintrag hat seine Wurzeln, Quarantänestände und
/// Plandigests bereits gebunden. Wer daran vorbei mutiert, verändert genau den
/// Zustand, den der wartende Auftrag beim Claim erwartet.
fn require_cli_queue_idle(queue: &OperationQueue, wirkung: &str) -> io::Result<()> {
    let snapshot = queue.snapshot();
    if snapshot.paused {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("{wirkung} ist gesperrt, solange die Operations-Queue pausiert ist"),
        ));
    }
    let open = snapshot
        .entries
        .into_iter()
        .filter(|entry| {
            !matches!(
                entry.state,
                doppeldecker_core::QueueState::Completed
                    | doppeldecker_core::QueueState::Failed
                    | doppeldecker_core::QueueState::Cancelled
            )
        })
        .count();
    if open == 0 {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "{wirkung} ist gesperrt, solange die Operations-Queue {open} offene oder zu prüfende Einträge enthält"
        ),
    ))
}

fn file_best_effort_summary(stats: &doppeldecker_core::OpStats) -> Option<String> {
    let mut effects = Vec::new();
    if !stats.source_read_limitations.is_empty() {
        effects.push("Quellinhalte geprüft; ausgewiesene Lesegrenzen bleiben");
    }
    if stats.target_copy_new_best_effort {
        effects.push("Zielinhalt neu angelegt");
    }
    if stats.target_replace_best_effort {
        effects.push("Zieldatei dauerhaft ersetzt");
    }
    if stats.source_removal_best_effort {
        effects.push("Quelle nach vollständigem Readback begrenzt entfernt");
    }
    (!effects.is_empty()).then(|| format!(" — Best Effort: {}", effects.join("; ")))
}

fn file_result_text(stats: &doppeldecker_core::OpStats) -> Vec<String> {
    let assurance = file_best_effort_summary(stats).unwrap_or_default();
    // Übersprungene Einträge gehören in die Textbilanz, nicht nur ins JSONL. Ein
    // `move` über einen Symlink meldete sonst „Fertig: 0 Dateien, 0 Bytes" und
    // Exitcode 0, obwohl nichts verschoben wurde; die GUI weist beide Zahlen aus.
    let mut skipped = Vec::new();
    if stats.skipped > 0 {
        skipped.push(format!("{} übersprungen", stats.skipped));
    }
    if stats.symlinks_skipped > 0 {
        skipped.push(format!("{} Symlink(s) ausgelassen", stats.symlinks_skipped));
    }
    let skipped = if skipped.is_empty() {
        String::new()
    } else {
        format!(", {}", skipped.join(", "))
    };
    let mut lines = vec![format!(
        "{}: {} Dateien, {} Bytes{}{}",
        if stats.cancelled {
            "Abgebrochen"
        } else {
            "Fertig"
        },
        stats.files,
        stats.bytes,
        skipped,
        assurance,
    )];
    lines.extend(stats.source_read_limitations.iter().cloned());
    lines.extend(stats.target_write_limitations.iter().cloned());
    lines.extend(stats.temp_recovery_paths.iter().map(|path| {
        if stats.retained_backup_paths.contains(path) {
            format!("Übertragung abgeschlossen; Sicherung erhalten: {path}")
        } else {
            format!("Möglicher Temp-Recoverypfad (nicht automatisch löschen): {path}")
        }
    }));
    lines
}

fn emit_queue_result(
    output: &mut Output,
    operation_id: &str,
    result: QueueExecutionResult,
) -> io::Result<i32> {
    match result {
        QueueExecutionResult::File(stats) => {
            output.event(serde_json::json!({
                "event": "result",
                "operation_id": operation_id,
                "stats": stats,
            }))?;
            for line in file_result_text(&stats) {
                output.text(line);
            }
            Ok(if stats.cancelled { 130 } else { 0 })
        }
        QueueExecutionResult::AwaitingConfirmation { reason, .. } => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Queue wartet auf manuelle Klärung: {reason}"),
        )),
        QueueExecutionResult::Sync(_) | QueueExecutionResult::Bidirectional(_) => {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Dateikommando erhielt einen fremden Queue-Ergebnistyp",
            ))
        }
    }
}

fn real_command(args: RealArgs, config_dir: &Path, output: &mut Output) -> io::Result<i32> {
    let options = args.sync.options();
    options.validate()?;
    // Der Auditpfad wird vor der teuren Vorprüfung aufgelöst: Ein verbotener Rückfall
    // soll scheitern, bevor irgendetwas vorbereitet oder bestätigt wurde.
    let audit = resolve_audit_path(args.audit)?;
    // Derselbe Prozessbesitz wie in der GUI gilt bereits vor der Realmodus-Probe und
    // bleibt bis zum terminalen Ergebnis offen. Ein vorhandener Queue-Auftrag darf
    // nicht durch einen direkten Realmoduslauf überholt werden: Seine gebundenen Roots
    // und Quarantänezustände könnten sonst vor dem späteren Claim verändert werden.
    let queue = open_cli_queue(config_dir)?;
    require_cli_queue_idle(&queue, "Lokaler Realmodus")?;
    let source = expand_and_canonicalize(&args.sync.source)?;
    let target = expand_and_canonicalize(&args.sync.target)?;
    reject_audit_inside_roots(&audit, &[&source, &target])?;
    output.event(serde_json::json!({ "event": "preparing" }))?;
    if !output.json {
        eprintln!("Realmodus-Vorprüfung: Mounts, Listings, Plan, Platz und Restore-Probe …");
    }
    let prepared = doppeldecker_core::prepare_local_real_run(&source, &target, options)?;
    let preview = prepared.preview();
    output.event(serde_json::json!({ "event": "real_preview", "preview": preview }))?;
    output.text(format!(
        "REAL vorbereitet: {} neu, {} Ersetzungen, {} Quarantäne-Einträge; {} / {} Bytes benötigt/verfügbar",
        preview.impact.new_files,
        preview.impact.replacements,
        preview.impact.quarantine_entries,
        preview.impact.required_available_bytes,
        preview.impact.available_bytes
    ));

    let (confirmation, backup_confirmed) = read_run_confirmation(preview)?;
    let cancel = install_cancel_handler()?;

    let mut last = Instant::now() - Duration::from_secs(1);
    let stats = prepared.confirm_and_execute_with_audit(
        &confirmation,
        backup_confirmed,
        &audit,
        &cancel,
        &mut |progress| {
            emit_throttled_progress(output, &mut last, progress);
        },
    )?;
    emit_sync_result(output, "result", &stats, &audit)
}

fn read_run_confirmation(preview: &RealRunPreview) -> io::Result<(String, bool)> {
    // Die CLI liest nur die beiden Zeilen. Der Core prüft sie anschließend nochmals
    // gegen die gebundene Vorschau und erzeugt erst danach das einmalige Permit.
    eprintln!(
        "Exakt über stdin bestätigen: {}",
        preview.confirmation_phrase
    );
    let confirmation = read_stdin_line()?;
    let backup_confirmed = if preview.backup_confirmation_required {
        eprintln!("Zusätzlich exakt über stdin bestätigen: {BACKUP_CONFIRMATION}");
        read_stdin_line()? == BACKUP_CONFIRMATION
    } else {
        false
    };
    Ok((confirmation, backup_confirmed))
}

fn emit_throttled_progress(output: &mut Output, last: &mut Instant, progress: &Progress) {
    let terminal = progress.phase == "done" || progress.phase == "cancelled";
    if terminal || last.elapsed() >= Duration::from_millis(100) {
        *last = Instant::now();
        emit_progress(output, progress);
    }
}

fn emit_sync_result(
    output: &mut Output,
    event: &str,
    stats: &SyncStats,
    audit: &Path,
) -> io::Result<i32> {
    output.event(serde_json::json!({ "event": event, "stats": stats, "audit": audit }))?;
    output.text(format!(
        "{}: {} kopiert, {} Metadaten, {} quarantänisiert, {} Bytes",
        if stats.cancelled {
            "Abgebrochen"
        } else {
            "Fertig"
        },
        stats.files_copied,
        stats.metadata_updated,
        stats.files_deleted,
        stats.bytes_copied
    ));
    for path in &stats.temp_recovery_paths {
        output.text(format!(
            "Möglicher Temp-Recoverypfad (nicht automatisch löschen): {path}"
        ));
    }
    Ok(if stats.cancelled { 130 } else { 0 })
}

fn emit_progress(output: &mut Output, progress: &Progress) {
    if output.json {
        let _ = output.event(serde_json::json!({ "event": "progress", "progress": progress }));
    } else {
        eprintln!(
            "{}: {}/{} Dateien, {}/{} Bytes{}",
            progress.phase,
            progress.files_done,
            progress.files_total,
            progress.bytes_done,
            progress.bytes_total,
            if progress.current.is_empty() {
                String::new()
            } else {
                format!(" · {}", progress.current)
            }
        );
    }
}

fn bidirectional_command(
    args: BidirectionalArgs,
    config_dir: &Path,
    output: &mut Output,
) -> io::Result<i32> {
    // Auch „Plan“ schreibt ein kurzlebiges Laufjournal und führt Realmodus-Proben aus.
    // Deshalb umfasst der Prozessbesitz absichtlich alle vier Unterbefehle und bleibt
    // über Bestätigung, Run/Resume beziehungsweise Rollback erhalten.
    let _queue_lock = open_cli_queue(config_dir)?;
    match args.command {
        BidirectionalCommand::Plan(args) => {
            let (pair_id, left, right, baseline, resolutions) =
                prepare_bidirectional_arguments(&args)?;
            let prepared = doppeldecker_core::prepare_local_bidirectional_with_decisions(
                pair_id,
                &args.pair_name,
                &left,
                &right,
                &baseline,
                &resolutions,
            )?;
            output.event(serde_json::json!({
                "event": "bidirectional_preview",
                "pair_id": pair_id,
                "preview": prepared.preview(),
            }))?;
            output.text(format!(
                "Bidirektionaler Plan: {} Aktionen, {} offene Konflikte",
                prepared.preview().plan.actions.len(),
                prepared.preview().plan.conflicts
            ));
            prepared.discard()?;
            Ok(0)
        }
        BidirectionalCommand::Run(args) => {
            let (pair_id, left, right, baseline, resolutions) =
                prepare_bidirectional_arguments(&args)?;
            let prepared = doppeldecker_core::prepare_local_bidirectional_with_decisions(
                pair_id,
                &args.pair_name,
                &left,
                &right,
                &baseline,
                &resolutions,
            )?;
            execute_bidirectional_prepared(prepared, &baseline, true, output)
        }
        BidirectionalCommand::Resume(args) => {
            let (left, right, baseline) = prepare_recovery_arguments(&args)?;
            let prepared = doppeldecker_core::prepare_resume_local_bidirectional(
                &args.pair_name,
                &left,
                &right,
                &baseline,
            )?;
            execute_bidirectional_prepared(prepared, &baseline, false, output)
        }
        BidirectionalCommand::Rollback(args) => {
            let (left, right, baseline) = prepare_recovery_arguments(&args)?;
            let prepared = doppeldecker_core::prepare_local_bidirectional_rollback(
                &args.pair_name,
                &left,
                &right,
                &baseline,
            )?;
            output.event(serde_json::json!({
                "event": "bidirectional_rollback_preview",
                "preview": prepared.preview(),
            }))?;
            eprintln!(
                "Exakt über stdin bestätigen: {}",
                prepared.preview().confirmation_phrase
            );
            let confirmation = read_stdin_line()?;
            let run = prepared.confirm(&confirmation)?;
            let cancel = install_cancel_handler()?;
            let stats = run.execute(&cancel)?;
            // Die Daten sind bereits zurückgerollt. Erst der mutationslose,
            // rücklesend gebundene Journalabschluss erlaubt eine Erfolgsausgabe.
            // So kann ein Absturz zwischen Wirkung und Ausgabe den alten Plan
            // niemals als erneut ausführbaren Retry erscheinen lassen.
            doppeldecker_core::bidirectional::finish_bidirectional_rollback_completion(&baseline)?;
            output.event(serde_json::json!({
                "event": "bidirectional_rollback_result",
                "stats": stats,
            }))?;
            output.text(format!(
                "Rollback abgeschlossen: {} Projektionen",
                stats.projections_rolled_back
            ));
            Ok(0)
        }
    }
}

fn execute_bidirectional_prepared(
    prepared: doppeldecker_core::PreparedLocalBidirectionalRun,
    baseline_path: &Path,
    discard_on_blocker: bool,
    output: &mut Output,
) -> io::Result<i32> {
    output.event(serde_json::json!({
        "event": "bidirectional_preview",
        "preview": prepared.preview(),
    }))?;
    if let Some(blocker) = &prepared.preview().stage_blocker {
        let blocker = blocker.clone();
        if discard_on_blocker {
            prepared.discard()?;
        }
        return Err(io::Error::new(io::ErrorKind::InvalidInput, blocker));
    }
    eprintln!(
        "Exakt über stdin bestätigen: {}",
        prepared.preview().confirmation_phrase
    );
    let confirmation = read_stdin_line()?;
    let backup_confirmed = if prepared.preview().backup_confirmation_required {
        eprintln!("Zusätzlich exakt über stdin bestätigen: {BACKUP_CONFIRMATION}");
        read_stdin_line()? == BACKUP_CONFIRMATION
    } else {
        false
    };
    let run = prepared.confirm(&confirmation, backup_confirmed)?;
    let cancel = install_cancel_handler()?;
    let mut last = Instant::now() - Duration::from_secs(1);
    let stats = run.execute(&cancel, &mut |progress| {
        if last.elapsed() >= Duration::from_millis(100) {
            last = Instant::now();
            emit_progress(output, progress);
        }
    })?;
    // Der Core hält das Abschlussjournal absichtlich bis zu diesem expliziten
    // Commit. Nur der erfolgreiche Readback-Cleanup macht die folgende
    // Ergebniszeile wahr; Resume und ein neuer Lauf teilen dieselbe Grenze.
    doppeldecker_core::bidirectional::finish_bidirectional_run_completion(baseline_path)?;
    output.event(serde_json::json!({
        "event": "bidirectional_result",
        "stats": stats,
    }))?;
    output.text(format!(
        "Bidirektionaler Lauf abgeschlossen: Baseline-Generation {}",
        stats.baseline_generation
    ));
    Ok(0)
}

/// Bindet beide Roots und die Baseline eines bidirektionalen Laufs.
///
/// Die Baseline darf niemals innerhalb eines synchronisierten Roots liegen: Sie
/// beschreibt den Vergleichsstand und würde sich sonst selbst mitsynchronisieren.
fn bidirectional_roots_and_baseline(
    left: &Path,
    right: &Path,
    baseline: &Path,
) -> io::Result<(PathBuf, PathBuf, PathBuf)> {
    let left = expand_and_canonicalize(left)?;
    let right = expand_and_canonicalize(right)?;
    let baseline = absolute_state_path(baseline)?;
    if baseline.starts_with(&left) || baseline.starts_with(&right) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Baseline-Datei darf nicht innerhalb eines synchronisierten Roots liegen",
        ));
    }
    Ok((left, right, baseline))
}

/// Der Auditpfad darf nicht innerhalb der synchronisierten Roots liegen.
///
/// `append_audit` öffnet die Datei je Datensatz neu. Liegt sie im Ziel, ist sie beim
/// zweiten Lauf ein überzähliger Eintrag: Mirror verschiebt sie mitten im Lauf in die
/// Quarantäne, der Abschlussdatensatz legt sie neu an — die Historie liegt danach
/// zerrissen in zwei Dateien. Die Baseline eines bidirektionalen Laufs hat dieselbe
/// Schranke bereits; das Audit hatte sie nur als Warnung in der Doku.
fn reject_audit_inside_roots(audit: &Path, roots: &[&Path]) -> io::Result<()> {
    // Beide Seiten müssen im selben aufgelösten Namensraum verglichen werden. Die
    // Roots sind kanonisiert; der Auditpfad ist nur normalisiert und existiert als
    // Datei oft noch gar nicht. Auf macOS liegt zwischen `/var/folders/…` und
    // `/private/var/folders/…` genau ein Symlink — ohne diese Auflösung ginge die
    // Prüfung an jeder Temp-Wurzel vorbei.
    let audit = canonicalize_existing_prefix(audit)?;
    for root in roots {
        if audit.starts_with(root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "Auditdatei darf nicht innerhalb eines synchronisierten Roots liegen: {} unter {}",
                    audit.display(),
                    root.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Löst den Pfad so weit auf, wie er schon existiert, und hängt den noch fehlenden
/// Rest wieder an.
///
/// Nur den direkten Elternordner zu kanonisieren reichte nicht: `append_audit` legt
/// fehlende Zwischenordner später selbst per `create_dir_all` an, also darf die
/// Prüfung nicht daran scheitern, dass sie noch fehlen. Auf macOS wurde ein Audit
/// unter `/tmp/lauf/neu/audit.jsonl` sonst gegen den kanonisierten Root
/// `/private/tmp/lauf` verglichen und rutschte durch die Schranke (Review-Fund
/// 2026-09-04).
///
/// Nur tatsächlich fehlende Komponenten werden angehängt. Zugriffsfehler und tote
/// Symlinks bleiben Fehler, statt einen unaufgelösten Alias als Außenpfad auszugeben.
fn canonicalize_existing_prefix(path: &Path) -> io::Result<PathBuf> {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut existing = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(mut resolved) => {
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&existing) {
                    Ok(_) => return Err(error),
                    Err(stat_error) if stat_error.kind() == io::ErrorKind::NotFound => {}
                    Err(stat_error) => return Err(stat_error),
                }
            }
            Err(error) => return Err(error),
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Pfad besitzt keinen auflösbaren Vorfahren",
                ))
            }
        }
    }
}

fn prepare_bidirectional_arguments(
    args: &BidirectionalPlanArgs,
) -> io::Result<(PairId, PathBuf, PathBuf, PathBuf, Vec<ConflictResolution>)> {
    let pair_id = parse_pair_id(&args.pair_id)?;
    let (left, right, baseline) =
        bidirectional_roots_and_baseline(&args.left, &args.right, &args.baseline)?;
    let resolutions = match &args.resolutions {
        Some(path) => read_resolutions(path)?,
        None => Vec::new(),
    };
    Ok((pair_id, left, right, baseline, resolutions))
}

fn prepare_recovery_arguments(
    args: &BidirectionalRecoveryArgs,
) -> io::Result<(PathBuf, PathBuf, PathBuf)> {
    bidirectional_roots_and_baseline(&args.left, &args.right, &args.baseline)
}

fn parse_pair_id(value: &str) -> io::Result<PairId> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Paar-ID muss aus genau 32 Hexzeichen bestehen",
        ));
    }
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Paar-ID ist ungültig"))?;
    }
    if bytes == [0; 16] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Paar-ID darf nicht Null sein",
        ));
    }
    Ok(PairId(bytes))
}

const MAX_RESOLUTION_BYTES: u64 = 16 * 1024 * 1024;

fn read_resolutions(path: &Path) -> io::Result<Vec<ConflictResolution>> {
    let path = expand_home_path(path);
    let metadata = std::fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_RESOLUTION_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Entscheidungsdatei ist keine kleine reguläre Datei",
        ));
    }
    let data = read_resolution_contents(&path)?;
    serde_json::from_slice(&data).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Entscheidungsdatei ist ungültig: {error}"),
        )
    })
}

fn read_resolution_contents(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    // Zwischen Pfadprüfung und Öffnen kann der letzte Name ausgetauscht werden.
    // NOFOLLOW verhindert den Symlinkzugriff; NONBLOCK lässt einen neuen FIFO
    // ohne Schreiber bis zur Prüfung des tatsächlich geöffneten Handles gelangen.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Entscheidungsdatei ist ein Symlink",
                )
            } else {
                error
            }
        })?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_RESOLUTION_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Entscheidungsdatei ist keine kleine reguläre Datei",
        ));
    }
    // Das Limit gilt auch bei Wachstum nach der Handleprüfung. Ein Zusatzbyte
    // unterscheidet eine exakt volle Datei von einer unvollständig gelesenen.
    let mut data = Vec::new();
    file.take(MAX_RESOLUTION_BYTES + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_RESOLUTION_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Entscheidungsdatei überschreitet 16 MiB",
        ));
    }
    Ok(data)
}

fn absolute_state_path(path: &Path) -> io::Result<PathBuf> {
    let path = expand_home_path(path);
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Baseline-Pfad darf keine Punkt- oder Elternkomponente enthalten",
        ));
    }
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    canonicalize_existing_prefix(&path)
}

fn install_cancel_handler() -> io::Result<Cancel> {
    let cancel = Cancel::new();
    let signal = cancel.clone();
    ctrlc::set_handler(move || signal.cancel()).map_err(io::Error::other)?;
    Ok(cancel)
}

fn quarantine_command(
    args: QuarantineArgs,
    config_dir: &Path,
    output: &mut Output,
) -> io::Result<i32> {
    match args.command {
        QuarantineCommand::List(args) => {
            let scope = QuarantineScope::for_target(&args.target)?;
            let runs = scope.runs()?;
            output.event(serde_json::json!({ "event": "quarantine_runs", "runs": runs }))?;
            for run in runs {
                output.text(quarantine_run_text(&run));
            }
        }
        QuarantineCommand::Entries(args) => {
            let scope = QuarantineScope::for_target(&args.target)?;
            scope.require_run(&args.run)?;
            let entries = scope.entries(&args.run)?;
            output.event(serde_json::json!({ "event": "quarantine_entries", "run": args.run, "entries": &entries }))?;
            for entry in entries {
                let status = quarantine_entry_status(&entry);
                output.text(format!(
                    "{} · {:?} · {status}",
                    entry.relative_path, entry.reason
                ));
            }
        }
        QuarantineCommand::Restore(args) => {
            // Restore verändert den Zielbestand. Das Lock wird vor Scope- und
            // Manifestprüfung genommen und bis nach dem exklusiven Rename gehalten.
            let _queue_lock = open_cli_queue(config_dir)?;
            let scope = QuarantineScope::for_target(&args.target)?;
            scope.require_run(&args.run)?;
            let expected = format!("RESTORE {} {}", args.run, args.path);
            eprintln!("Exakt über stdin bestätigen: {expected}");
            if read_stdin_line()? != expected {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Restore-Bestätigung stimmt nicht überein",
                ));
            }
            let restored = scope.restore(&args.run, &args.path)?;
            output.event(serde_json::json!({
                "event": "restored",
                "path": &restored.path,
                "outcome": restored.outcome,
            }))?;
            match restored.outcome {
                doppeldecker_core::QuarantineRestoreOutcome::Restored => {
                    output.text(format!("Wiederhergestellt: {}", restored.path.display()))
                }
                doppeldecker_core::QuarantineRestoreOutcome::Reconciled => output.text(format!(
                    "Restore-Abschluss bestätigt: {}",
                    restored.path.display()
                )),
            }
        }
        QuarantineCommand::Purge(args) => {
            // Endgültiges Leeren darf ebenso wenig parallel zum GUI-Worker laufen.
            let _queue_lock = open_cli_queue(config_dir)?;
            let scope = QuarantineScope::for_target(&args.target)?;
            scope.require_run(&args.run)?;
            let expected = format!("LEEREN {}", args.run);
            eprintln!("Exakt über stdin bestätigen: {expected}");
            if read_stdin_line()? != expected {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Leeren-Bestätigung stimmt nicht überein",
                ));
            }
            scope.purge(&args.run)?;
            output.event(serde_json::json!({
                "event": "purged",
                "run": args.run,
                "contents_empty": true,
                "recovery_tombstone_visible": true,
            }))?;
            output.text(format!(
                "Inhalte von Quarantänelauf {} geleert; Recovery-Tombstone bleibt sichtbar",
                args.run
            ));
        }
    }
    Ok(0)
}

fn quarantine_run_text(run: &doppeldecker_core::QuarantineRun) -> String {
    format!(
        "{} · {:?} · {}/{} physisch vorhanden · {} automatisch wiederherstellbar · {} nur manuell rettbar · {} Abschlussprüfung(en) · {} Bytes physisch vorhanden",
        run.id,
        run.status,
        run.items_available,
        run.items_total,
        run.items_restorable,
        run.items_manual_recovery,
        run.items_recovery_pending,
        run.bytes_available
    )
}

fn quarantine_entry_status(entry: &doppeldecker_core::QuarantineEntry) -> &'static str {
    if entry.restored {
        "wiederhergestellt"
    } else if entry.manual_recovery_required {
        "physisch vorhanden · nur manuell rettbar"
    } else if entry.recovery_pending && !entry.available {
        "physisch nicht vorhanden · Restore-Abschlussprüfung ausstehend"
    } else if entry.available && entry.automatic_restore_available {
        "physisch vorhanden · automatisch wiederherstellbar"
    } else if entry.available {
        "physisch vorhanden · automatische Wiederherstellung gesperrt"
    } else {
        "Quarantäneeintrag fehlt"
    }
}

fn read_stdin_line() -> io::Result<String> {
    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn expand_and_canonicalize(path: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(expand_home_path(path))
}

/// Normalisiert ausschließlich den vorhandenen Elternordner einer Papierkorbwahl.
/// Der letzte Eintrag bleibt unaufgelöst: Ein ausgewählter Symlink soll selbst in den
/// Papierkorb wechseln und darf niemals durch sein Ziel ersetzt werden.
fn canonicalize_parent_preserving_final_entry(path: &Path) -> io::Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Papierkorb-Pfad besitzt keinen Elternordner",
        )
    })?;
    let name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Papierkorb-Pfad besitzt keinen Eintragsnamen",
            )
        })?;
    Ok(std::fs::canonicalize(parent)?.join(name))
}

/// Wahr, sobald `DOPPELDECKER_NO_DEFAULT_USER_PATHS` gesetzt und weder leer noch `0` ist.
fn default_user_paths_forbidden() -> bool {
    match std::env::var_os(NO_DEFAULT_USER_PATHS_ENV) {
        Some(value) => {
            let value = value.to_string_lossy();
            !value.is_empty() && value != "0"
        }
        None => false,
    }
}

/// Baut den Fehler, mit dem ein verbotener Rückfall sichtbar scheitert.
///
/// `ErrorKind::InvalidInput` ergibt den stabilen Maschinencode `invalid_input`; die
/// Meldung nennt die fehlende Option und den Ordner, der sonst betroffen wäre.
fn missing_explicit_path_error(flag: &str, would_use: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{NO_DEFAULT_USER_PATHS_ENV} ist gesetzt: {flag} muss ausdrücklich angegeben \
             werden, weil der Rückfall auf {would_use} hier verboten ist"
        ),
    )
}

/// Löst den zu benutzenden Konfigurationsordner auf.
fn resolve_config_dir(explicit: Option<PathBuf>) -> io::Result<PathBuf> {
    match explicit {
        Some(dir) => Ok(expand_home_path(dir)),
        None if default_user_paths_forbidden() => Err(missing_explicit_path_error(
            "--config-dir",
            "den echten Konfigurationsordner der Desktop-App",
        )),
        None => Ok(default_config_dir()),
    }
}

/// Löst den zu benutzenden Auditpfad eines realen Laufs auf.
fn resolve_audit_path(explicit: Option<PathBuf>) -> io::Result<PathBuf> {
    let path = match explicit {
        Some(path) => Ok(expand_home_path(path)),
        None if default_user_paths_forbidden() => Err(missing_explicit_path_error(
            "--audit",
            "das echte Auditjournal des Nutzers",
        )),
        None => Ok(default_audit_path()),
    }?;
    validate_audit_path(path)
}

/// Prüft den Auditnamen rein lesend, bevor Queue, Secrets oder Provider berührt
/// werden. Die spätere append-only Öffnung bleibt die eigentliche atomare Wirkung;
/// hier lehnen wir nur Pfade ab, deren Zieltyp oder nächster vorhandener Elternordner
/// schon jetzt offensichtlich ungeeignet ist.
fn validate_audit_path(path: PathBuf) -> io::Result<PathBuf> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Auditpfad muss absolut und lexikalisch normalisiert sein",
        ));
    }
    if path.file_name().is_none_or(|name| name.to_str().is_none()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Auditdateiname ist nicht eindeutig als UTF-8 darstellbar",
        ));
    }
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Auditpfad muss eine reguläre Datei oder ein noch freier Dateiname sein",
            ));
        }
        Ok(metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o222 == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("Auditdatei ist nicht schreibbar: {}", path.display()),
                    ));
                }
            }
            #[cfg(not(unix))]
            if metadata.permissions().readonly() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Auditdatei ist nicht schreibbar: {}", path.display()),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut ancestor = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Auditpfad besitzt keinen Elternordner",
        )
    })?;
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "Audit-Elternpfad ist kein symlinkfreier Ordner: {}",
                            ancestor.display()
                        ),
                    ));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o222 == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "Audit-Elternordner ist nicht schreibbar: {}",
                                ancestor.display()
                            ),
                        ));
                    }
                }
                #[cfg(not(unix))]
                if metadata.permissions().readonly() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "Audit-Elternordner ist nicht schreibbar: {}",
                            ancestor.display()
                        ),
                    ));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ancestor = ancestor.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Auditpfad besitzt keinen vorhandenen Elternordner",
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(path)
}

fn default_audit_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        // Derselbe Anwendungsordner wie die Konfiguration: Die frühere Kennung
        // `com.doppeldecker.app` gehörte zu keiner ausgelieferten App und trennte
        // das CLI-Audit unbemerkt vom Audit der Desktop-App.
        return PathBuf::from(home)
            .join("Library/Application Support")
            .join(APPLICATION_IDENTIFIER)
            .join("real-runs.jsonl");
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(state).join("doppeldecker/real-runs.jsonl");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".local/state/doppeldecker/real-runs.jsonl");
        }
    }
    PathBuf::from("doppeldecker-real-runs.jsonl")
}

fn default_config_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library/Application Support")
            .join(APPLICATION_IDENTIFIER);
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(config).join(APPLICATION_IDENTIFIER);
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".config")
                .join(APPLICATION_IDENTIFIER);
        }
    }
    PathBuf::from("doppeldecker-config")
}

#[cfg(test)]
mod default_path_tests {
    use super::*;

    #[test]
    fn cli_delta_allgemeiner_fehler_erhaelt_core_pruefpfade() {
        let error = doppeldecker_core::error::with_recovery_paths(
            doppeldecker_core::coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                "Ausgang offen",
            ),
            &["/kuenstlich/zu-pruefen".into()],
        );
        let value = cli_error_event(&error);
        assert_eq!(value["code"], "outcome_unclear");
        assert_eq!(
            value["recovery_paths"],
            serde_json::json!(["/kuenstlich/zu-pruefen"])
        );
    }

    #[test]
    fn cli_delta_dateifehler_erhaelt_getrennte_executor_und_journalpfade() {
        let error = doppeldecker_core::error::with_recovery_paths(
            doppeldecker_core::coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                "Ausführung offen",
            ),
            &["/kuenstlich/temp".into()],
        );
        let journal = doppeldecker_core::error::with_recovery_paths(
            doppeldecker_core::coded_error(io::ErrorKind::Other, ErrorCode::Io, "Journal offen"),
            &["/kuenstlich/journal".into()],
        );
        let value = provider_file_error_event(
            &error,
            &Default::default(),
            &Default::default(),
            None,
            &[journal],
        );
        assert_eq!(value["code"], "io");
        assert_eq!(value["execution_error"]["code"], "outcome_unclear");
        assert_eq!(
            value["execution_error"]["recovery_paths"],
            serde_json::json!(["/kuenstlich/temp"])
        );
        assert_eq!(
            value["recovery_paths"],
            serde_json::json!(["/kuenstlich/journal"])
        );
        assert_eq!(
            value["journal_error"]["recovery_paths"],
            serde_json::json!(["/kuenstlich/journal"])
        );
        assert_eq!(
            value["journal_errors"][0]["recovery_paths"],
            serde_json::json!(["/kuenstlich/journal"])
        );
    }

    #[test]
    fn cli_delta_entscheidungsdatei_darf_das_groessenlimit_genau_erreichen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("resolutions.json");
        let mut data = vec![b' '; MAX_RESOLUTION_BYTES as usize];
        data[..2].copy_from_slice(b"[]");
        std::fs::write(&path, &data).unwrap();
        assert!(read_resolutions(&path).unwrap().is_empty());
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b" ")
            .unwrap();
        assert_eq!(
            read_resolutions(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn cli_delta_entscheidungsleser_lehnt_wachstum_nach_pfadpruefung_ab() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("resolutions.json");
        std::fs::write(&path, b"[]").unwrap();
        assert!(read_resolutions(&path).unwrap().is_empty());
        std::fs::write(&path, vec![b' '; 17 * 1024 * 1024]).unwrap();
        let result = read_resolution_contents(&path);
        assert!(
            matches!(result, Err(ref error) if error.kind()==io::ErrorKind::InvalidData),
            "Die tatsächliche I/O-Grenze liest nach der früheren Pfadprüfung mehr als16MiB: {:?}",
            result.as_ref().map(Vec::len)
        );
    }

    #[test]
    fn cli_delta_entscheidungsleser_folgt_keinem_eingewechselten_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("resolutions.json");
        std::fs::write(&path, b"[]").unwrap();
        assert!(read_resolutions(&path).unwrap().is_empty());
        let preserved = temp.path().join("urspruengliche-datei");
        std::fs::rename(&path, &preserved).unwrap();
        let other = temp.path().join("fremde-testdatei");
        std::fs::write(&other, b"anderer Inhalt").unwrap();
        std::os::unix::fs::symlink(&other, &path).unwrap();
        let result = read_resolution_contents(&path);
        assert_eq!(std::fs::read(&preserved).unwrap(), b"[]");
        assert_eq!(std::fs::read(&other).unwrap(), b"anderer Inhalt");
        assert!(
            result.is_err(),
            "Die tatsächliche I/O-Grenze folgte nach der früheren Pfadprüfung einem Symlink"
        );
    }

    #[test]
    #[ignore = "nur im isolierten FIFO-Gegenlauf mit eigener Markierung und Prozessfrist"]
    fn resolution_fifo_child() {
        let root =
            PathBuf::from(std::env::var_os("DOPPELDECKER_CLI_RESOLUTION_FIFO_ROOT").unwrap());
        assert!(root
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("doppeldecker-cli-resolutions-fifo-"));
        assert_eq!(
            std::fs::read(root.join("marker")).unwrap(),
            b"cli-resolution-fifo-v1"
        );
        std::fs::write(root.join("ready"), b"ready").unwrap();
        let error = read_resolution_contents(&root.join("resolutions.json")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn baseline_unter_fehlendem_ordner_hinter_root_alias_wird_abgelehnt() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("links");
        let right = temp.path().join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&left, &alias).unwrap();
        let baseline = alias.join("neu/tiefer/paar.json");
        let error = bidirectional_roots_and_baseline(&left, &right, &baseline).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!left.join("neu").exists());
        let outside = temp.path().join("aussen/neu/paar.json");
        let (_, _, resolved) = bidirectional_roots_and_baseline(&left, &right, &outside).unwrap();
        assert_eq!(
            resolved,
            temp.path()
                .canonicalize()
                .unwrap()
                .join("aussen/neu/paar.json")
        );
    }

    #[test]
    fn textlisting_zeigt_sftp_best_effort_und_unvollstaendigkeit() {
        let status = interactive_listing_status(false, Some(SftpReadSecurity::SftpPathBestEffort));

        assert!(status.contains("Best Effort"));
        assert!(status.contains("vollständig: nein"));
        assert!(status.contains("nicht atomar an ein Verzeichnishandle gebunden"));
    }

    #[cfg(unix)]
    #[test]
    fn provider_queue_pfad_wird_nicht_verlustbehaftet_serialisiert() {
        use std::os::unix::ffi::OsStringExt;

        let raw = PathBuf::from(std::ffi::OsString::from_vec(vec![b'd', b'a', b't', 0xff]));
        let error = doppeldecker_core::fileops::path_to_utf8(&raw).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            doppeldecker_core::error_code(&error),
            ErrorCode::InvalidData
        );
    }

    /// Der Auditpfad darf nicht daran vorbeirutschen, dass sein Unterordner noch
    /// fehlt: `append_audit` legt ihn später selbst per `create_dir_all` an.
    #[cfg(unix)]
    #[test]
    fn audit_unter_noch_fehlendem_unterordner_bleibt_im_root_erkennbar() {
        let temp = tempfile::tempdir().unwrap();
        let echt = temp.path().join("lauf");
        std::fs::create_dir(&echt).unwrap();
        // Der Verweis steht für jeden Pfad, der erst nach der Auflösung im Root
        // liegt — auf macOS ist `/tmp` gegenüber `/private/tmp` genau so einer.
        let ueber_verweis = temp.path().join("verweis");
        std::os::unix::fs::symlink(&echt, &ueber_verweis).unwrap();
        let root = std::fs::canonicalize(&echt).unwrap();

        let audit = ueber_verweis.join("neu").join("real-runs.jsonl");
        let error = reject_audit_inside_roots(&audit, &[root.as_path()]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("real-runs.jsonl"), "{error}");

        // Derselbe Aufbau außerhalb des Roots bleibt erlaubt.
        let daneben = temp.path().join("aussen").join("real-runs.jsonl");
        reject_audit_inside_roots(&daneben, &[root.as_path()]).unwrap();
    }

    #[test]
    fn delete_vorschau_bindet_plan_vor_und_nach_capability_probe() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let selected = root.join("datei.txt");
        std::fs::write(&selected, b"alt").unwrap();
        let vfs = doppeldecker_core::LocalVfs::new();
        let probe_saw_source = std::cell::Cell::new(false);

        let error =
            bind_delete_preview_around_probe(&vfs, &root, std::slice::from_ref(&selected), || {
                probe_saw_source.set(selected.exists());
                std::fs::write(&selected, b"waehrend probe").unwrap();
                Ok(())
            })
            .unwrap_err();

        assert!(
            probe_saw_source.get(),
            "der erste Plan lag vor der Probe vor"
        );
        assert_eq!(doppeldecker_core::error_code(&error), ErrorCode::PlanDrift);
    }

    #[test]
    fn dateiergebnis_benennt_jede_tatsaechliche_best_effort_wirkung() {
        let stats = doppeldecker_core::OpStats {
            target_copy_new_best_effort: true,
            target_replace_best_effort: true,
            source_removal_best_effort: true,
            ..Default::default()
        };
        let summary = file_best_effort_summary(&stats).unwrap();
        assert!(summary.contains("Zielinhalt neu angelegt"));
        assert!(summary.contains("Zieldatei dauerhaft ersetzt"));
        assert!(summary.contains("Quelle nach vollständigem Readback begrenzt entfernt"));
    }

    #[test]
    fn json_fehler_behaelt_fruehere_wirkung_und_markiert_nur_unklare_phase() {
        let error = doppeldecker_core::coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            "Quellabbau nach Zielcommit unklar",
        );
        let actuals = doppeldecker_core::fileops::FileOperationActuals {
            target_write_limitations: Vec::new(),
            source_removal_best_effort: None,
            target_copy_new_best_effort: Some(true),
            target_replace_best_effort: Some(false),
            temp_recovery_paths: Vec::new(),
            retained_backup_paths: Box::default(),
        };
        let progress = Progress {
            files_done: 2,
            bytes_done: 8,
            ..Default::default()
        };

        let recovery = doppeldecker_core::queue::ProviderFileRecovery {
            quarantine_run: "/ziel/.doppeldecker-quarantine/lauf".into(),
            parked_entries: 2,
        };
        let event = provider_file_error_event(&error, &actuals, &progress, Some(&recovery), &[]);

        assert_eq!(event["code"], "outcome_unclear");
        assert_eq!(event["execution_error"]["code"], "outcome_unclear");
        assert!(event["journal_error"].is_null());
        assert_eq!(event["stats"]["files"], 2);
        assert_eq!(event["stats"]["target_copy_new_best_effort"], true);
        assert!(event["stats"]["source_removal_best_effort"].is_null());
        assert_eq!(
            event["recovery"]["quarantine_run"],
            "/ziel/.doppeldecker-quarantine/lauf"
        );
        assert_eq!(event["recovery"]["parked_entries"], 2);
    }

    #[test]
    fn cli_finalizer_priorisiert_offenen_journalfehler_aber_ignoriert_terminalen_zustand() {
        let operation = || QueueOperation::File {
            kind: FileQueueKind::Copy,
            items: vec!["/tmp/quelle.txt".into()],
            destination: "/tmp/ziel".into(),
            overwrite: false,
            expected_conflicts: Vec::new(),
            keep_both_targets: Vec::new(),
            expected_conflict_proofs: Vec::new(),
            local_replace_best_effort_expected: Some(false),
            local_copy_new_best_effort_expected: None,
        };
        let original = || {
            doppeldecker_core::coded_error(
                io::ErrorKind::Other,
                ErrorCode::PlanDrift,
                "ursprünglicher Executorfehler",
            )
        };

        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("offen/queue.json");
        let queue = OperationQueue::open(&queue_path).unwrap();
        let queued = queue.enqueue(operation()).unwrap();
        queue.claim_next().unwrap().unwrap();
        std::fs::remove_file(&queue_path).unwrap();
        std::fs::create_dir(&queue_path).unwrap();
        let error = finalize_executor_result(&queue, &queued.operation_id, Err(original()))
            .err()
            .unwrap();
        assert_ne!(doppeldecker_core::error_code(&error), ErrorCode::PlanDrift);
        let execution_error = original();
        let actuals = doppeldecker_core::fileops::FileOperationActuals {
            target_write_limitations: Vec::new(),
            source_removal_best_effort: None,
            target_copy_new_best_effort: Some(true),
            target_replace_best_effort: Some(false),
            temp_recovery_paths: Vec::new(),
            retained_backup_paths: Box::default(),
        };
        let progress = Progress {
            files_done: 2,
            bytes_done: 8,
            ..Progress::default()
        };
        let recovery = doppeldecker_core::queue::ProviderFileRecovery {
            quarantine_run: "/ziel/.doppeldecker-quarantine/lauf".into(),
            parked_entries: 2,
        };
        let event = provider_file_error_event(
            &execution_error,
            &actuals,
            &progress,
            Some(&recovery),
            std::slice::from_ref(&error),
        );
        assert_ne!(event["code"], "plan_drift");
        assert_eq!(event["execution_error"]["code"], "plan_drift");
        assert_eq!(event["stats"]["target_copy_new_best_effort"], true);
        assert_eq!(event["recovery"]["parked_entries"], 2);
        assert!(event["journal_error"].is_object());

        let terminal_path = temp.path().join("terminal/queue.json");
        let terminal_queue = OperationQueue::open(&terminal_path).unwrap();
        let terminal = terminal_queue.enqueue(operation()).unwrap();
        terminal_queue.claim_next().unwrap().unwrap();
        terminal_queue.mark_running(&terminal.operation_id).unwrap();
        terminal_queue
            .complete(&terminal.operation_id, Progress::default())
            .unwrap();
        let error =
            finalize_executor_result(&terminal_queue, &terminal.operation_id, Err(original()))
                .err()
                .unwrap();
        assert_eq!(doppeldecker_core::error_code(&error), ErrorCode::PlanDrift);
    }

    #[test]
    fn cli_finalizer_persistiert_outcome_unclear_fuer_beide_executorwege() {
        let operation = || QueueOperation::File {
            kind: FileQueueKind::Copy,
            items: vec!["/tmp/quelle.txt".into()],
            destination: "/tmp/ziel".into(),
            overwrite: false,
            expected_conflicts: Vec::new(),
            keep_both_targets: Vec::new(),
            expected_conflict_proofs: Vec::new(),
            local_replace_best_effort_expected: Some(false),
            local_copy_new_best_effort_expected: None,
        };
        let original = || {
            doppeldecker_core::coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                "Antwort nach gesendeter Mutation verloren",
            )
        };
        let temp = tempfile::tempdir().unwrap();

        let generic = OperationQueue::open(temp.path().join("generic/queue.json")).unwrap();
        let generic_entry = generic.enqueue_claimed(operation()).unwrap();
        let returned =
            finalize_executor_result(&generic, &generic_entry.operation_id, Err(original()))
                .unwrap_err();
        assert_eq!(
            doppeldecker_core::error_code(&returned),
            ErrorCode::OutcomeUnclear
        );
        assert_eq!(
            generic.snapshot().entries[0].error_code,
            Some(ErrorCode::OutcomeUnclear)
        );
        assert_eq!(
            doppeldecker_core::error_code(&generic.retry(&generic_entry.operation_id).unwrap_err()),
            ErrorCode::OutcomeUnclear
        );

        let file = OperationQueue::open(temp.path().join("file/queue.json")).unwrap();
        let file_entry = file.enqueue_claimed(operation()).unwrap();
        persist_executor_failure(
            &file,
            &file_entry.operation_id,
            Progress::default(),
            &original(),
            Some(&doppeldecker_core::fileops::FileOperationActuals::default()),
        )
        .unwrap();
        assert_eq!(
            file.snapshot().entries[0].error_code,
            Some(ErrorCode::OutcomeUnclear)
        );
        assert_eq!(
            doppeldecker_core::error_code(&file.retry(&file_entry.operation_id).unwrap_err()),
            ErrorCode::OutcomeUnclear
        );
    }

    #[test]
    fn cli_fehlercommit_erhaelt_die_bekannte_sicherungsbilanz() {
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path().join("queue.json");
        let queue = OperationQueue::open(journal.clone()).unwrap();
        let entry = queue
            .enqueue_claimed(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec!["/tmp/quelle.txt".into()],
                destination: "/tmp/ziel".into(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let recovery = temp
            .path()
            .join("erhaltene-sicherung")
            .display()
            .to_string();
        let actuals = doppeldecker_core::fileops::FileOperationActuals {
            target_write_limitations: Vec::new(),
            temp_recovery_paths: vec![recovery.clone()],
            retained_backup_paths: Box::new(vec![recovery.clone()]),
            ..Default::default()
        };
        persist_executor_failure(
            &queue,
            &entry.operation_id,
            Progress::default(),
            &io::Error::other("Spätere Datei fehlgeschlagen"),
            Some(&actuals),
        )
        .unwrap();
        drop(queue);
        let reopened = OperationQueue::open(journal).unwrap();
        let persisted = &reopened.snapshot().entries[0];
        assert_eq!(persisted.temp_recovery_paths, vec![recovery.clone()]);
        assert_eq!(persisted.retained_backup_paths, vec![recovery]);
    }

    #[test]
    fn dateiergebnis_verwendet_nur_terminales_stats_cancelled() {
        let mut output = Output { json: false };
        let stats = doppeldecker_core::OpStats {
            cancelled: false,
            temp_recovery_paths: vec!["/tmp/möglicher-tempname".into()],
            retained_backup_paths: Vec::new(),
            ..Default::default()
        };
        let text = file_result_text(&stats);
        assert_eq!(text.len(), 2);
        assert!(text[1].contains("Möglicher Temp-Recoverypfad"));
        assert!(text[1].contains("nicht automatisch löschen"));
        let result = QueueExecutionResult::File(stats);

        assert_eq!(
            emit_queue_result(&mut output, "spaetes-signal", result).unwrap(),
            0
        );
    }

    #[test]
    fn unklare_faehigkeitsprobe_stoppt_folgeproben_und_erhaelt_pruefpfade() {
        use doppeldecker_core::{coded_error, error::with_recovery_paths, error_code, ErrorCode};
        for code in [ErrorCode::OutcomeUnclear, ErrorCode::ConnectionLost] {
            let mut blocker = None;
            let first = run_capability_probe::<()>(&mut blocker, || {
                Err(with_recovery_paths(
                    coded_error(io::ErrorKind::Other, code, "Erste Probe offen"),
                    &["/pane/unknown-stou".into()],
                ))
            });
            assert_eq!(error_code(&first.unwrap_err()), code);
            let next =
                run_capability_probe::<()>(&mut blocker, || panic!("Folgeprobe darf nicht wirken"));
            assert_eq!(error_code(next.as_ref().unwrap_err()), code);
            assert_eq!(
                capability_result(next)["recovery_paths"],
                serde_json::json!(["/pane/unknown-stou"])
            );
        }
        let mut blocker = None;
        assert!(
            run_capability_probe::<()>(&mut blocker, || Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Begrenzt abgelehnt"
            )))
            .is_err()
        );
        assert_eq!(run_capability_probe(&mut blocker, || Ok(7)).unwrap(), 7);
    }

    /// Hält den macOS-Plattformpfad fest: Audit und Konfiguration liegen im selben
    /// Anwendungsordner. Genau diese Bindung war gebrochen — das Audit zeigte auf
    /// die nie ausgelieferte Kennung `com.doppeldecker.app`.
    ///
    /// Unter Linux gilt die Trennung bewusst weiter: Dort ist das Auditjournal
    /// veränderlicher Zustand und gehört nach XDG in `XDG_STATE_HOME`.
    #[cfg(target_os = "macos")]
    #[test]
    fn audit_liegt_im_konfigurationsordner_der_desktop_app() {
        if std::env::var_os("HOME").is_none() {
            return;
        }
        let audit = default_audit_path();
        let config = default_config_dir();
        assert_eq!(audit.parent(), Some(config.as_path()));
        assert_eq!(audit.file_name().unwrap(), "real-runs.jsonl");
        assert!(
            config.to_string_lossy().ends_with(APPLICATION_IDENTIFIER),
            "Konfigurationsordner {} nennt die stabile Anwendungskennung nicht",
            config.display()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn audit_nutzt_unter_linux_den_xdg_state_ordner() {
        if std::env::var_os("HOME").is_none() && std::env::var_os("XDG_STATE_HOME").is_none() {
            return;
        }
        let audit = default_audit_path();
        assert!(
            audit.to_string_lossy().contains("doppeldecker"),
            "Auditpfad {} liegt außerhalb des eigenen Zustandsordners",
            audit.display()
        );
        assert_eq!(audit.file_name().unwrap(), "real-runs.jsonl");
    }
}
