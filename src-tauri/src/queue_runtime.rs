//! Gemeinsame Laufzeit für Queue, Single-Flight-Schranke und Fortschrittsereignisse.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use doppeldecker_core::{
    recover_trash_journals, Cancel, FtpsConnectionConfig, FtpsVfs, LocalBidirectionalStats,
    OpStats, OperationQueue, OperationQueueProcessLock, PreparedLocalRealRun, PreparedSftpQueueRun,
    PreparedSftpRealRun, Progress, QueueEntry, QueueOperation, QueueOperationClass,
    RemoteProfileBinding, RemoteProfileProtocol, SftpConnectionConfig, SftpQueueDirection, SftpVfs,
    SmbConnectionConfig, SmbVfs, SyncOptions, SyncPlan, SyncStats, VersionedProfile,
    WebDavConnectionConfig, WebDavVfs,
};
use tauri::{Emitter, Manager};
use zeroize::Zeroizing;

use crate::{
    app_config_dir, bidirectional_commands, ftps_commands, provider_sync_commands, sandbox_root,
    sftp_commands, smb_commands, webdav_commands, AppError, MutexExt, MAX_DELETE_ENTRIES,
    MAX_DELETE_FRACTION,
};

/// Prozessweiter Zustand. Nur der Live-Verbindungspool, der einmalige Startbefund
/// und die bewusst nach Fensterlabel schlüsselnden lesenden Laufzeiten werden
/// geteilt. Dateioperationen, vorbereitete Realmodus-Läufe und Queue-Journale liegen
/// dagegen in `windows`.
#[derive(Default)]
pub(crate) struct AppState {
    /// Beim Setup gefundene beschädigte Nebenfenster-Journale bleiben bis zur
    /// gezielten Abholung durch das Hauptfenster im Backend. Ein frühes Event wäre
    /// verloren, weil die Webview ihre Listener zu diesem Zeitpunkt noch nicht
    /// registriert hat.
    pub(crate) startup_queue_error: Mutex<Option<String>>,
    /// Rein kosmetische oder sitzungsbezogene Startfehler bleiben getrennt vom
    /// Queue-Befund. Nur ein wirklich unlesbares Queue-Journal darf das Queue-Badge
    /// auf `!` setzen und damit den Eintragszähler überdecken.
    pub(crate) startup_app_error: Mutex<Option<String>>,
    /// Lesende Namenssuchen besitzen pro Fenster ein eigenes Abbruchsignal.
    pub(crate) name_searches: crate::name_search_commands::NameSearchRuntime,
    /// Lesende Ordnergrößen besitzen pro Fenster und Pane eigene Sessions. Sie
    /// blockieren die jeweilige Fensterqueue ausdrücklich nicht.
    pub(crate) directory_sizes: crate::directory_size_commands::DirectorySizeRuntime,
    /// Ein verbundener Server erscheint wie ein Mount in allen Fenstern. Deshalb
    /// teilen alle Fenster dieselbe lebende Sitzung samt ihrem Capability-Stand.
    pub(crate) sftp_connections: Mutex<HashMap<String, CachedRemoteConnection<SftpVfs>>>,
    pub(crate) ftps_connections: Mutex<HashMap<String, CachedRemoteConnection<FtpsVfs>>>,
    pub(crate) webdav_connections: Mutex<HashMap<String, CachedRemoteConnection<WebDavVfs>>>,
    pub(crate) smb_connections: Mutex<HashMap<String, CachedRemoteConnection<SmbVfs>>>,
    windows: Mutex<HashMap<String, Arc<WindowState>>>,
    /// Alle Fensterqueues teilen eine einzige externe Prozesssperre. Damit dürfen sie
    /// parallel arbeiten, während ein zweiter GUI-/CLI-Prozess weiter ausgeschlossen ist.
    queue_process_lock: OnceLock<Arc<OperationQueueProcessLock>>,
    /// Serialisiert ausschließlich den noch nicht erfolgreichen Lock-Erwerb. Ein
    /// vorübergehender Fremdbesitz wird nicht gecacht; nach dessen Freigabe darf
    /// derselbe Appprozess den Versuch wiederholen.
    queue_process_lock_init: Mutex<()>,
    /// Papierkorb-Recovery betrifft gemeinsame lokale Pfade und muss vor dem ersten
    /// Queue-Worker genau einmal pro Prozess abgeschlossen sein.
    trash_recovery: OnceLock<Result<(), String>>,
    /// Prozessweite Sperre für den gemeinsamen Papierkorb-Journalordner.
    ///
    /// Die Single-Flight-Schranke eines Fensters reicht hier nicht: Alle Fenster
    /// benutzen denselben Journalordner, und `recover_trash_journals` darf dort
    /// auch einen noch als `Planned` markierten Vorgang zurücksetzen oder
    /// entfernen. Liefe sie parallel zur Papierkorboperation eines anderen
    /// Fensters, rissen Claim und Journal auseinander und Wiederherstellungs-
    /// informationen gingen verloren. Recovery **und** die vollständige
    /// Papierkorboperation laufen deshalb unter dieser Sperre.
    pub(crate) trash_journal_guard: Mutex<()>,
    /// Ein Profil und sein neuer Schlüsselbundwert bilden genau eine
    /// Speichertransaktion. Die Sperre verhindert, dass zwei Fenster ihre
    /// Rollback-Schritte ineinander verschachteln.
    pub(crate) profile_secret_guard: Mutex<()>,
    /// Geheimnisfreies Aktivitätszeichen für die laufende Profil-/Secret-
    /// Transaktion. Der Exit-Snapshot bindet nur dieses Bool, niemals Werte.
    pub(crate) profile_transaction_active: AtomicBool,
    /// Fensterlabel der laufenden Profiltransaktion. Das Label ist geheimnisfrei
    /// und dient nur dazu, genau das verursachende Fenster während der Wirkung
    /// offen zu halten; ein anderes sauberes Fenster bleibt unabhängig schließbar.
    profile_transaction_owner: Mutex<Option<String>>,
    /// Noch nicht verbrauchte native Secret-Eingaben. Der Webview kennt nur die
    /// zufällige Einmal-ID; der Wert bleibt als `Zeroizing` im Backend und wird
    /// beim Entfernen aus dieser Map sicher überschrieben.
    pub(crate) pending_connection_secrets: Mutex<HashMap<String, PendingConnectionSecret>>,
    /// Native Passwortdialoge sind appweit single-flight. Insbesondere der
    /// GTK-Nested-Loop darf keinem zweiten Fenster einen parallelen Prompt öffnen.
    pub(crate) secret_capture_active: AtomicBool,
    /// Der Beenden-Dialog bestätigt nur den exakt angezeigten appweiten Zustand.
    /// Token und anschließende Freigabe werden je nur einmal entnommen.
    pub(crate) pending_exit_confirmation:
        Mutex<Option<crate::app_commands::PendingExitConfirmation>>,
    pub(crate) authorized_exit: Mutex<Option<crate::app_commands::ExitSnapshot>>,
    pub(crate) exit_confirmation_sequence: AtomicU64,
    /// Sobald eine native Exitprüfung beginnt, dürfen in keinem Fenster neue
    /// Operationen reserviert werden. Bei einem verhinderten Exit wird das Gate
    /// wieder geöffnet; beim erlaubten Exit bleibt es bis zum Prozessende zu.
    pub(crate) exit_in_progress: Arc<AtomicBool>,
    /// Serialisiert den Wechsel „Exit-Gate setzen + vollständigen Snapshot lesen“
    /// mit allen Queue-Erweiterungen und neuen Fenstern. Das Atom allein schließt
    /// die Lücke zwischen deren letzter Prüfung und der Journalmutation nicht.
    pub(crate) exit_barrier: Mutex<()>,
    /// Eine ungeordnete Endpunktmenge besitzt appweit genau eine Paar-ID und feste
    /// Rollen. Der Mutex umfasst Lesen, Ergänzen und atomaren Registry-Commit.
    pub(crate) bidirectional_pairs: Mutex<bidirectional_commands::BidirectionalPairRegistry>,
}

pub(crate) struct PendingConnectionSecret {
    pub(crate) owner: String,
    pub(crate) protocol: String,
    pub(crate) profile_id: String,
    pub(crate) captured_at: Instant,
    pub(crate) value: Zeroizing<String>,
}

pub(crate) struct CachedRemoteConnection<T> {
    pub(crate) binding: RemoteProfileBinding,
    pub(crate) connection: Arc<T>,
}

#[derive(Clone, Copy)]
pub(crate) struct RemoteConnectionRequest<'a> {
    pub(crate) protocol: RemoteProfileProtocol,
    pub(crate) profile_id: &'a str,
    pub(crate) requested_revision: Option<u64>,
}

impl<T> Clone for CachedRemoteConnection<T> {
    fn clone(&self) -> Self {
        Self {
            binding: self.binding.clone(),
            connection: self.connection.clone(),
        }
    }
}

/// Führt einen bestätigten Remote-Realmodus-Lauf im Hintergrund aus.
///
/// SFTP, FTPS, WebDAV, SMB und der generische Providersync teilen diesen Ablauf
/// vollständig:
///
/// 1. Die Profilbindungen werden unmittelbar vor der Wirkung noch einmal geprüft.
///    Zwischen Vorbereitung und Bestätigung kann ein Profil geändert worden sein.
/// 2. `run` gibt den Lauf gegen die exakte Bestätigungsphrase frei und führt ihn
///    aus. Scheitert schon die Freigabe, ist das derselbe Fehlerweg wie ein
///    Ausführungsfehler: `sync-error` mit dem Fehlertext, kein Fortschritt.
/// 3. Fortschritt geht gedrosselt an genau das auslösende Fenster; der terminale
///    Fortschritt („done“/„cancelled“) wird zurückgehalten und danach ungedrosselt
///    nachgereicht, damit die Oberfläche nie auf einem alten Stand stehen bleibt.
/// 4. `finish_operation` gibt die Single-Flight-Schranke des Fensters frei — auf
///    jedem Weg, auch bei Fehlern, sonst bliebe das Fenster dauerhaft belegt.
pub(crate) fn spawn_confirmed_remote_run<S, F>(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    label: String,
    profile_bindings: Vec<RemoteProfileBinding>,
    run: F,
) where
    S: serde::Serialize + Clone + Send + 'static,
    F: FnOnce(&mut dyn FnMut(&Progress)) -> std::io::Result<S> + Send + 'static,
{
    std::thread::spawn(move || {
        if let Err(error) = ensure_profile_bindings_current(&app, &profile_bindings) {
            finish_operation(&app, &label, || {
                let _ = crate::emit_to_window(&window, "sync-error", error);
            });
            return;
        }
        let events = window.clone();
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut terminal_progress = None;
        let result = run(&mut |progress| {
            let terminal = matches!(progress.phase.as_str(), "done" | "cancelled");
            if terminal {
                terminal_progress = Some(progress.clone());
            } else if last.elapsed() >= Duration::from_millis(50) {
                last = Instant::now();
                let _ = crate::emit_to_window(&events, "sync-progress", progress);
            }
        });
        finish_operation(&app, &label, || {
            if let Some(progress) = terminal_progress {
                let _ = crate::emit_to_window(&window, "sync-progress", progress);
            }
            match result {
                Ok(stats) => {
                    let _ = crate::emit_to_window(&window, "sync-done", stats);
                }
                Err(error) => {
                    let _ = crate::emit_to_window(&window, "sync-error", AppError::from(error));
                }
            }
        });
    });
}

/// Meldet, ob ein Fehler nur die Verbindung betrifft und ein neuer Versuch
/// überhaupt sinnvoll ist.
///
/// Der Kern übersetzt providerspezifische Sonderfälle vorher hierher — etwa eine
/// nach Leerlauf abgelaufene FTP-Kontrollsitzung, die `suppaftp` als
/// Protokollfehler meldet.
pub(crate) fn reconnectable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::UnexpectedEof
    )
}

/// Wiederholt eine **lesende** Providerabfrage nach einem Verbindungsfehler genau
/// einmal.
///
/// Nur Lesen darf das. Eine mutierende Anfrage kann der Server bereits ausgeführt
/// haben, bevor die Antwort verlorenging; ein blinder zweiter Versuch würde die
/// Wirkung verdoppeln. Deshalb nimmt dieser Helfer bewusst keine Mutation entgegen.
pub(crate) fn retry_read_once<T>(
    mut read: impl FnMut() -> std::io::Result<T>,
    reconnect: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<T> {
    match read() {
        Ok(value) => Ok(value),
        Err(error) if reconnectable(&error) => {
            reconnect()?;
            read()
        }
        Err(error) => Err(error),
    }
}

/// Quarantänewurzel neben einem Remote-Ziel.
///
/// Die Sicherung liegt bewusst **neben** dem Zielbaum, nicht darin: Sonst würde
/// ein rekursiver Lauf seine eigenen Sicherungen wieder einsammeln. `label`
/// benennt den Provider im Fehlertext.
pub(crate) fn remote_quarantine_base(label: &str, target: &Path) -> Result<PathBuf, AppError> {
    let parent = target.parent().ok_or_else(|| {
        AppError::new(
            "sandbox_violation",
            format!("{label}-Ziel besitzt keinen Quarantäne-Namespace"),
        )
    })?;
    Ok(parent.join(".doppeldecker-quarantine"))
}

/// Eine protokollspezifische Remote-Vorbereitung besitzt neben dem lebenden Permit
/// bereits ihre vollständige, geheimnisfreie Queueoperation. Der spätere Enqueue-
/// Command darf dadurch weder einen anderen Endpunkt noch einen anderen Plan aus
/// erneut gelesenen Frontenddaten zusammensetzen.
pub(crate) struct PendingRemoteRealRun {
    pub(crate) prepared: PreparedSftpRealRun,
    pub(crate) operation: QueueOperation,
    pub(crate) profile_bindings: Vec<RemoteProfileBinding>,
}

pub(crate) struct PendingRemoteQueueRun {
    pub(crate) prepared: PreparedSftpQueueRun,
    pub(crate) profile_bindings: Vec<RemoteProfileBinding>,
}

pub(crate) struct PendingBidirectionalQueueRun {
    pub(crate) prepared: doppeldecker_core::PreparedBidirectionalQueueRun,
    pub(crate) profile_bindings: Vec<RemoteProfileBinding>,
}

/// Führt den langsamen Handshake außerhalb der Profiltransaktionssperre aus,
/// bindet seinen Cache-Commit aber wieder an exakt dieselbe persistente Revision.
/// Save/Remove verwenden dieselbe Sperre. Ein alter Handshake kann dadurch weder
/// einen neuen Cacheeintrag verdrängen noch nach einer Profiländerung zurückkehren.
pub(crate) fn cached_remote_connection<T, P>(
    app: &tauri::AppHandle,
    state: &AppState,
    pool: &Mutex<HashMap<String, CachedRemoteConnection<T>>>,
    request: RemoteConnectionRequest<'_>,
    load_profile: impl FnMut() -> Result<VersionedProfile<P>, AppError>,
    connect: impl FnOnce(P) -> Result<Arc<T>, AppError>,
) -> Result<(CachedRemoteConnection<T>, VersionedProfile<P>), AppError>
where
    P: Clone,
{
    let (connection, current, inserted) =
        cached_remote_connection_using(state, pool, request, load_profile, connect)?;
    if inserted {
        crate::connection_state_commands::emit_connection_update(
            app,
            &connection.binding,
            crate::connection_state_commands::ConnectionChangeKind::Connected,
        );
    }
    Ok((connection, current))
}

fn cached_remote_connection_using<T, P>(
    state: &AppState,
    pool: &Mutex<HashMap<String, CachedRemoteConnection<T>>>,
    request: RemoteConnectionRequest<'_>,
    mut load_profile: impl FnMut() -> Result<VersionedProfile<P>, AppError>,
    connect: impl FnOnce(P) -> Result<Arc<T>, AppError>,
) -> Result<(CachedRemoteConnection<T>, VersionedProfile<P>, bool), AppError>
where
    P: Clone,
{
    let RemoteConnectionRequest {
        protocol,
        profile_id,
        requested_revision,
    } = request;
    let profile = {
        let _transaction = state.profile_secret_guard.lock_recover();
        let profile = match load_profile() {
            Ok(profile) => profile,
            Err(error) => {
                // Auch eine Änderung aus einem zweiten Prozess wird spätestens
                // beim nächsten Zugriff sichtbar. Ein inzwischen fehlendes oder
                // unlesbares Profil darf dann nicht weiter als aktive Sitzung im
                // prozessweiten Pool erscheinen.
                pool.lock_recover().remove(profile_id);
                return Err(error);
            }
        };
        let binding = RemoteProfileBinding::new(protocol, profile_id, profile.revision)
            .map_err(AppError::from)?;
        // Die vom sichtbaren Panezustand gebundene Revision wird unter derselben
        // Profil-/Secret-Sperre geprüft wie der Cache. Eine alte Pane darf weder
        // einen Cachetreffer nutzen noch einen Handshake zum inzwischen geänderten
        // Host, Root oder Credential-Verweis beginnen.
        ensure_requested_profile_revision(requested_revision, &binding)?;
        let mut connections = pool.lock_recover();
        if let Some(cached) = connections.get(profile_id) {
            if cached.binding == binding {
                return Ok((cached.clone(), profile, false));
            }
            // Ein später Profildateifehler kann die Datei bereits ersetzt haben,
            // bevor der Save-Command seinen normalen Invalidierungspfad erreicht.
            connections.remove(profile_id);
        }
        profile
    };
    let expected = RemoteProfileBinding::new(protocol, profile_id, profile.revision)
        .map_err(AppError::from)?;
    let fresh = connect(profile.profile.clone())?;

    let (connection, inserted, current) = {
        let _transaction = state.profile_secret_guard.lock_recover();
        let current = load_profile()?;
        if current.revision != expected.revision {
            return Err(AppError::new(
                "plan_drift",
                "Das Verbindungsprofil änderte sich während des Verbindungsaufbaus",
            ));
        }
        let mut connections = pool.lock_recover();
        let (connection, inserted) = match connections.entry(profile_id.to_owned()) {
            std::collections::hash_map::Entry::Occupied(entry)
                if entry.get().binding == expected =>
            {
                (entry.get().clone(), false)
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let cached = CachedRemoteConnection {
                    binding: expected.clone(),
                    connection: fresh,
                };
                entry.insert(cached.clone());
                (cached, true)
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                let cached = CachedRemoteConnection {
                    binding: expected.clone(),
                    connection: fresh,
                };
                entry.insert(cached.clone());
                (cached, true)
            }
        };
        (connection, inserted, current)
    };
    Ok((connection, current, inserted))
}

pub(crate) fn binding_matches_profile(
    binding: &RemoteProfileBinding,
    protocol: RemoteProfileProtocol,
    profile_id: &str,
) -> bool {
    binding.protocol == protocol && binding.profile_id == profile_id
}

pub(crate) fn ensure_requested_profile_revision(
    requested: Option<u64>,
    binding: &RemoteProfileBinding,
) -> Result<(), AppError> {
    if requested == Some(binding.revision) {
        return Ok(());
    }
    Err(AppError::new(
        "plan_drift",
        "Die angeforderte Profilrevision fehlt oder ist nicht mehr aktuell",
    ))
}

fn ensure_pending_profile_binding(
    bindings: &[RemoteProfileBinding],
    protocol: RemoteProfileProtocol,
    profile_id: &str,
    requested_revision: Option<u64>,
) -> Result<(), AppError> {
    let binding = bindings
        .iter()
        .find(|binding| binding_matches_profile(binding, protocol, profile_id))
        .ok_or_else(|| AppError::new("plan_drift", "Remote-Profilbindung fehlt"))?;
    ensure_requested_profile_revision(requested_revision, binding)
}

fn take_profile_dependent<T>(
    slot: &Mutex<Option<T>>,
    protocol: RemoteProfileProtocol,
    profile_id: &str,
    bindings: impl FnOnce(&T) -> &[RemoteProfileBinding],
) -> Option<T> {
    let mut pending = slot.lock_recover();
    let affected = pending.as_ref().is_some_and(|value| {
        bindings(value)
            .iter()
            .any(|binding| binding_matches_profile(binding, protocol, profile_id))
    });
    affected.then(|| pending.take()).flatten()
}

fn discard_queue_profile_dependent<T>(
    slot: &Mutex<Option<T>>,
    protocol: RemoteProfileProtocol,
    profile_id: &str,
    bindings: impl FnOnce(&T) -> &[RemoteProfileBinding],
    operation_id: impl FnOnce(&T) -> &str,
    discard: impl FnOnce(&str) -> Result<(), AppError>,
) -> Result<bool, AppError> {
    let mut pending = slot.lock_recover();
    let Some(value) = pending.as_ref() else {
        return Ok(false);
    };
    if !bindings(value)
        .iter()
        .any(|binding| binding_matches_profile(binding, protocol, profile_id))
    {
        return Ok(false);
    }
    let operation_id = operation_id(value).to_owned();
    // Slot und Journal bilden gemeinsam die Vorbereitung. Solange der Slot
    // gesperrt bleibt, wird zuerst der persistierte Queuezustand zurückgesetzt;
    // nur nach dessen sicherem Commit darf das nicht serialisierbare Permit weg.
    discard(&operation_id)?;
    pending.take();
    Ok(true)
}

/// Baut eine bereits verbrauchte oder nicht veröffentlichte Remote-Vorbereitung
/// ab. Ein fehlgeschlagener Journalcommit darf nicht hinter dem fachlichen Fehler
/// verschwinden: Ohne RAM-Permit ist dieser Wartestand erst nach Neustart und
/// bewusster Neuprüfung wieder erreichbar. Ein Ersatzpermit entsteht hier nie.
pub(crate) fn reset_consumed_remote_preparation(
    queue: &OperationQueue,
    operation_id: &str,
    original_error: AppError,
) -> AppError {
    reset_consumed_remote_preparation_using(original_error, || {
        queue.discard_remote_preparation(operation_id)
    })
}

fn reset_consumed_remote_preparation_using(
    original_error: AppError,
    reset: impl FnOnce() -> std::io::Result<()>,
) -> AppError {
    match reset() {
        Ok(()) => original_error,
        Err(error) => {
            let mut reset_error = AppError::from(error);
            reset_error.message = format!(
                "{}; Queue-Vorbereitung konnte nicht zurückgesetzt werden: {}. Die Vorbereitung ist nicht mehr verfügbar. Schreibfehler beheben, App neu starten und den Queue-Eintrag mit „Neu prüfen“ erneut vorbereiten.",
                original_error.message, reset_error.message,
            );
            // Ein unklarer Probeausgang verbietet Wiederholungen auch dann,
            // wenn zusätzlich der mutationslose Journalreset scheitert.
            if original_error.code == doppeldecker_core::ErrorCode::OutcomeUnclear.as_str() {
                reset_error.code = original_error.code;
            }
            for path in original_error.recovery_paths {
                if !reset_error.recovery_paths.contains(&path) {
                    reset_error.recovery_paths.push(path);
                }
            }
            reset_error
        }
    }
}

fn reset_stale_queue_preparation<T>(
    slot: &Mutex<Option<T>>,
    requested_operation_id: &str,
    bindings: impl FnOnce(&T) -> &[RemoteProfileBinding],
    operation_id: impl FnOnce(&T) -> &str,
    ensure_current: impl FnOnce(&[RemoteProfileBinding]) -> Result<(), AppError>,
    discard: impl FnOnce(&str) -> Result<(), AppError>,
    after_discard: impl FnOnce(),
) -> Result<(), AppError> {
    let mut pending = slot.lock_recover();
    let value = pending
        .as_ref()
        .ok_or_else(|| AppError::new("app", "Queue-Vorbereitung fehlt im RAM"))?;
    if operation_id(value) != requested_operation_id {
        return Err(AppError::new(
            "plan_drift",
            "Queue-Vorbereitung gehört zu einem anderen Eintrag",
        ));
    }
    let stale = match ensure_current(bindings(value)) {
        Ok(()) => return Ok(()),
        Err(error) if error.code == "plan_drift" => error,
        // Ein Lesefehler beweist keinen Profilwechsel. Das Permit bleibt dann
        // unangetastet und derselbe Bestätigungsklick kann den Readback wiederholen.
        Err(error) => return Err(error),
    };
    // Der Klick auf die weiterhin sichtbare Bestätigung ist der erreichbare
    // Wiederholungsweg nach einem früheren Journalschreibfehler. Erst der sichere
    // Pending-Commit entfernt das RAM-Permit; bei erneutem Fehler bleibt es für den
    // nächsten Versuch vollständig erhalten.
    discard(requested_operation_id)?;
    pending.take();
    after_discard();
    Err(stale)
}

pub(crate) fn ensure_queue_pending_current_or_reset<T>(
    app: &tauri::AppHandle,
    label: &str,
    queue: &OperationQueue,
    slot: &Mutex<Option<T>>,
    requested_operation_id: &str,
    bindings: impl FnOnce(&T) -> &[RemoteProfileBinding],
    operation_id: impl FnOnce(&T) -> &str,
) -> Result<(), AppError> {
    reset_stale_queue_preparation(
        slot,
        requested_operation_id,
        bindings,
        operation_id,
        |bindings| ensure_profile_bindings_current(app, bindings),
        |operation_id| {
            queue
                .discard_remote_preparation(operation_id)
                .map_err(AppError::from)
        },
        || emit_queue_snapshot(app, label, queue),
    )
}

/// Verwirft ausschließlich noch nicht gestartete Vorbereitungen, die von dem
/// geänderten Profil abhängen. Andere Profile und Fenster dürfen weiterarbeiten.
/// Ein bereits bestätigungsbereiter Queueeintrag wird dabei atomar nach `Pending`
/// zurückgesetzt und nur an sein eigenes Fenster als neuer Snapshot gemeldet.
pub(crate) fn invalidate_remote_profile(
    app: &tauri::AppHandle,
    state: &AppState,
    protocol: RemoteProfileProtocol,
    profile_id: &str,
) {
    let windows: Vec<_> = state
        .windows
        .lock_recover()
        .iter()
        .map(|(label, state)| (label.clone(), state.clone()))
        .collect();
    for (label, window) in windows {
        take_profile_dependent(
            &window.pending_content_sync,
            protocol,
            profile_id,
            |pending| &pending.profile_bindings,
        );
        {
            let mut runs = window.content_sync_runs.lock_recover();
            let affected: Vec<_> = runs
                .iter()
                .filter(|(_, run)| {
                    run.profile_bindings
                        .iter()
                        .any(|binding| binding_matches_profile(binding, protocol, profile_id))
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in affected {
                if let Some(queue) = window.operation_queue.lock_recover().as_ref() {
                    let awaiting = queue.snapshot().entries.iter().any(|entry| {
                        entry.operation_id == id
                            && entry.state == doppeldecker_core::QueueState::AwaitingConfirmation
                    });
                    let reset = if awaiting {
                        queue.discard_remote_preparation(&id)
                    } else {
                        Ok(())
                    };
                    match reset {
                        Ok(()) => {
                            runs.remove(&id);
                            emit_queue_snapshot(app, &label, queue);
                        }
                        Err(error) => {
                            let _ = emit_to_window_label(
                                app,
                                &label,
                                "queue-error",
                                AppError::from(error),
                            );
                        }
                    }
                }
            }
        }
        take_profile_dependent(&window.pending_sftp_real, protocol, profile_id, |pending| {
            &pending.profile_bindings
        });
        take_profile_dependent(&window.pending_ftps_real, protocol, profile_id, |pending| {
            &pending.profile_bindings
        });
        take_profile_dependent(
            &window.pending_webdav_real,
            protocol,
            profile_id,
            |pending| &pending.profile_bindings,
        );
        take_profile_dependent(&window.pending_smb_real, protocol, profile_id, |pending| {
            &pending.profile_bindings
        });
        take_profile_dependent(
            &window.pending_provider_real,
            protocol,
            profile_id,
            |pending| &pending.profile_bindings,
        );
        take_profile_dependent(
            &window.pending_bidirectional_rollback,
            protocol,
            profile_id,
            |pending| &pending.profile_bindings,
        );

        let queue = window.operation_queue.lock_recover().clone();
        crate::forward_bidirectional_commands::invalidate_profile(
            app,
            &label,
            &window,
            queue.as_deref(),
            protocol,
            profile_id,
        );
        let mut queue_changed = false;
        macro_rules! discard_queue_preparation {
            ($field:ident) => {
                match discard_queue_profile_dependent(
                    &window.$field,
                    protocol,
                    profile_id,
                    |pending| &pending.profile_bindings,
                    |pending| pending.prepared.operation_id(),
                    |operation_id| {
                        let queue = queue.as_ref().ok_or_else(|| {
                            AppError::new(
                                "outcome_unclear",
                                "Remote-Vorbereitung war vorhanden, aber ihre Fensterqueue fehlt",
                            )
                        })?;
                        queue
                            .discard_remote_preparation(operation_id)
                            .map_err(AppError::from)
                    },
                ) {
                    Ok(changed) => queue_changed |= changed,
                    Err(error) => {
                        let _ = emit_to_window_label(app, &label, "queue-error", error);
                    }
                }
            };
        }
        discard_queue_preparation!(pending_sftp_queue);
        discard_queue_preparation!(pending_ftps_queue);
        discard_queue_preparation!(pending_webdav_queue);
        discard_queue_preparation!(pending_smb_queue);
        discard_queue_preparation!(pending_provider_queue);
        discard_queue_preparation!(pending_bidirectional_queue);

        {
            let mut pending = window.pending_bidirectional.lock_recover();
            let affected = pending.as_ref().is_some_and(|pending| {
                !pending.queue_transition_active()
                    && pending
                        .profile_bindings
                        .iter()
                        .any(|binding| binding_matches_profile(binding, protocol, profile_id))
            });
            if affected {
                pending.take();
            }
        }

        if queue_changed {
            emit_queue_snapshot(app, &label, queue.as_ref().unwrap());
        }
    }
}

pub(crate) fn ensure_profile_bindings_current(
    app: &tauri::AppHandle,
    bindings: &[RemoteProfileBinding],
) -> Result<(), AppError> {
    ensure_profile_bindings_current_using(bindings, |binding| match binding.protocol {
        RemoteProfileProtocol::Sftp => {
            crate::sftp_commands::current_profile_revision(app, &binding.profile_id)
        }
        RemoteProfileProtocol::Ftps => {
            crate::ftps_commands::current_profile_revision(app, &binding.profile_id)
        }
        RemoteProfileProtocol::Webdav => {
            crate::webdav_commands::current_profile_revision(app, &binding.profile_id)
        }
        RemoteProfileProtocol::Smb => {
            crate::smb_commands::current_profile_revision(app, &binding.profile_id)
        }
    })
}

/// Schließt das letzte Rennen zwischen Revisionsprüfung und Veröffentlichung eines
/// nicht serialisierbaren Permits. Profil-Save/Remove halten denselben Mutex bis
/// nach ihrer gezielten Invalidierung; daher kann entweder nur der alte Pending-
/// Stand sichtbar werden oder die Profilmutation zuerst gewinnen, niemals beides.
pub(crate) fn store_profile_bound_pending<T>(
    app: &tauri::AppHandle,
    state: &AppState,
    slot: &Mutex<Option<T>>,
    bindings: &[RemoteProfileBinding],
    pending: T,
) -> Result<(), AppError> {
    store_profile_bound_pending_using(state, slot, pending, None, || {
        ensure_profile_bindings_current(app, bindings)
    })
}

pub(crate) fn store_profile_bound_pending_for_generation<T>(
    app: &tauri::AppHandle,
    state: &AppState,
    slot: &Mutex<Option<T>>,
    bindings: &[RemoteProfileBinding],
    pending: T,
    generation: (&AtomicU64, u64),
) -> Result<(), AppError> {
    store_profile_bound_pending_using(state, slot, pending, Some(generation), || {
        ensure_profile_bindings_current(app, bindings)
    })
}

pub(crate) fn discard_sync_preparation<T>(slot: &Mutex<Option<T>>, generation: &AtomicU64) {
    let mut slot = slot.lock_recover();
    generation.fetch_add(1, Ordering::Release);
    slot.take();
}

fn store_profile_bound_pending_using<T>(
    state: &AppState,
    slot: &Mutex<Option<T>>,
    pending: T,
    generation: Option<(&AtomicU64, u64)>,
    validate: impl FnOnce() -> Result<(), AppError>,
) -> Result<(), AppError> {
    let _transaction = state.profile_secret_guard.lock_recover();
    validate()?;
    let mut slot = slot.lock_recover();
    if generation.is_some_and(|(current, expected)| current.load(Ordering::Acquire) != expected) {
        return Err(AppError::new(
            "plan_drift",
            "Die Sync-Vorbereitung wurde inzwischen verworfen",
        ));
    }
    if slot.is_some() {
        return Err(AppError::new(
            "busy",
            "Während der Vorbereitung entstand bereits ein anderer Pending-Lauf",
        ));
    }
    *slot = Some(pending);
    Ok(())
}

fn ensure_profile_bindings_current_using(
    bindings: &[RemoteProfileBinding],
    mut current_revision: impl FnMut(&RemoteProfileBinding) -> Result<Option<u64>, AppError>,
) -> Result<(), AppError> {
    for binding in bindings {
        let current = current_revision(binding)?;
        if current != Some(binding.revision) {
            return Err(AppError::new(
                "plan_drift",
                format!(
                    "Das {}-Profil änderte sich seit der Vorbereitung",
                    binding.protocol.as_str().to_uppercase()
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn merge_profile_bindings(
    groups: impl IntoIterator<Item = Vec<RemoteProfileBinding>>,
) -> Vec<RemoteProfileBinding> {
    let mut merged = Vec::new();
    for group in groups {
        for binding in group {
            if !merged.iter().any(|existing| existing == &binding) {
                merged.push(binding);
            }
        }
    }
    merged.sort_by(|left, right| {
        left.protocol
            .cmp(&right.protocol)
            .then(left.profile_id.cmp(&right.profile_id))
            .then(left.revision.cmp(&right.revision))
    });
    merged
}

/// Ergebnis einer Queueausführung, das erst nach Freigabe der fensterlokalen
/// Reservation an die Webview gesendet wird. Fortschrittsereignisse während des
/// Laufs bleiben erlaubt; die terminale Phase und das Fachereignis liegen hier.
pub(crate) struct QueueTerminalEvent {
    terminal_progress: Option<(QueueOperationClass, Progress)>,
    outcome: QueueTerminalOutcome,
}

enum QueueTerminalOutcome {
    FileDone(OpStats),
    SyncDone(SyncStats),
    BidirectionalDone(LocalBidirectionalStats),
    Message {
        event: &'static str,
        message: String,
    },
    Error {
        event: &'static str,
        error: AppError,
    },
    AppError(AppError),
}

impl QueueTerminalEvent {
    pub(crate) fn message(event: &'static str, message: impl Into<String>) -> Self {
        Self {
            terminal_progress: None,
            outcome: QueueTerminalOutcome::Message {
                event,
                message: message.into(),
            },
        }
    }

    pub(crate) fn error(event: &'static str, error: impl Into<AppError>) -> Self {
        Self {
            terminal_progress: None,
            outcome: QueueTerminalOutcome::Error {
                event,
                error: error.into(),
            },
        }
    }

    fn with_progress(
        terminal_progress: Option<(QueueOperationClass, Progress)>,
        outcome: QueueTerminalOutcome,
    ) -> Self {
        Self {
            terminal_progress,
            outcome,
        }
    }

    fn emit_to_window<R: tauri::Runtime>(
        self,
        app: &tauri::AppHandle<R>,
        label: &str,
        context: &crate::operation_events::OperationViewContext,
    ) {
        if let Some((class, progress)) = self.terminal_progress {
            let event = match class {
                QueueOperationClass::File => "fileop-progress",
                QueueOperationClass::Sync => "sync-progress",
            };
            let _ = emit_to_window_label(app, label, event, context.wrap(progress));
        }
        match self.outcome {
            QueueTerminalOutcome::FileDone(stats) => {
                let _ = emit_to_window_label(app, label, "fileop-done", context.wrap(stats));
            }
            QueueTerminalOutcome::SyncDone(stats) => {
                let _ = emit_to_window_label(app, label, "sync-done", context.wrap(stats));
            }
            QueueTerminalOutcome::BidirectionalDone(stats) => {
                let _ = emit_to_window_label(app, label, "bidirectional-done", context.wrap(stats));
            }
            QueueTerminalOutcome::Message { event, message } => {
                let _ = emit_to_window_label(app, label, event, message);
            }
            QueueTerminalOutcome::Error { event, error } => {
                let _ = emit_to_window_label(app, label, event, context.wrap(error));
            }
            QueueTerminalOutcome::AppError(error) => {
                let _ = emit_to_window_label(app, label, "queue-error", context.wrap(error));
            }
        }
    }
}

// Neue Vorbereitungen erhalten hier zugleich ihren Namen für den Exit-Snapshot.
// Die Reihenfolge der Vorbereitungsliste bestimmt die Reihenfolge der Locks.
macro_rules! window_state {
    (
        $(#[$state_attr:meta])*
        $vis:vis struct $name:ident {
            fields { $($fields:tt)* }
            preparations {
                $(
                    $(#[$prep_attr:meta])*
                    $field:ident: $slot:ty => $kind:literal,
                )*
            }
        }
    ) => {
        $(#[$state_attr])*
        $vis struct $name {
            $($fields)*
            $(
                $(#[$prep_attr])*
                pub(crate) $field: $slot,
            )*
        }

        impl $name {
            /// Geheimnisfreie Arten aller noch nicht gestarteten Vorbereitungen. Auch ein
            /// Queue-Bestätigungspermit bleibt hier sichtbar: Der zugehörige Queueeintrag
            /// blockiert zwar ebenfalls, doch der vollständige Snapshot darf einen Slotwechsel
            /// nicht mit demselben Queuezustand verwechseln.
            pub(crate) fn pending_preparation_kinds(&self) -> Vec<String> {
                let mut kinds = Vec::new();
                $(
                    if self.$field.lock_recover().is_some() {
                        kinds.push($kind.to_owned());
                    }
                )*
                kinds.sort();
                kinds
            }
        }
    };
}

window_state! {
    /// Ausschließlich einem Fenster gehörender, nicht serialisierbarer Laufzustand.
    /// Jedes Fenster hat damit seine eigene Single-Flight-Schranke, seine eigene Queue
    /// und seine eigenen Einmal-Permits. Andere Fenster dürfen parallel arbeiten.
    #[derive(Default)]
    pub(crate) struct WindowState {
        fields {
            /// Geteilte Referenz auf das appweite Exit-Gate. `AppState::window` ersetzt den
            /// Default durch dieselbe Instanz für jedes Fenster.
            exit_in_progress: Arc<AtomicBool>,
            /// Wird vor dem Close-Snapshot gesetzt. Danach dürfen weder neue Operationen
            /// noch neue Queueeinträge dieses Fensters beginnen.
            pub(crate) closing: AtomicBool,
            /// Wird beim dynamischen Erzeugen genau einmal vom neuen Frontend abgeholt.
            /// Danach lebt der aktuelle Pane-Zustand ausschließlich im jeweiligen Fenster.
            pub(crate) initial_panes: Mutex<Option<crate::window_commands::WindowStartState>>,
            pub(crate) active_cancel: Mutex<Option<Cancel>>,
            pub(crate) resumable_copy_preparation: Mutex<crate::file_commands::ResumePreparationSlot>,
            /// Auch ein bereits entfernter Queueeintrag kann noch lokale Daten bereinigen.
            /// Dieser Zähler blockiert nur Schließen/Beenden, nicht andere Dateiaktionen.
            queue_cleanups: AtomicUsize,
            pub(crate) property_edits: Mutex<crate::file_property_commands::PropertyEditCache>,
            pub(crate) remote_edits: Mutex<HashMap<String, crate::remote_edit_commands::RemoteEdit>>,
            pub(crate) real_sync_generation: AtomicU64,
            pub(crate) sftp_sync_generation: AtomicU64,
            pub(crate) provider_sync_generation: AtomicU64,
            pub(crate) content_sync_generation: AtomicU64,
            pub(crate) content_sync_runs:
                Mutex<HashMap<String, crate::content_sync_commands::QueuedContentSync>>,
            pub(crate) local_sync_runs:
                Mutex<HashMap<String, crate::normal_sync_commands::QueuedLocalSync>>,
            pub(crate) file_conflict_choices: Mutex<crate::file_commands::FileConflictChoiceCache>,
            pub(crate) archive_extract_preparation:
                Mutex<crate::archive_extract_commands::ArchiveExtractCache>,
            pub(crate) archive_pack_preparation: Mutex<crate::archive_pack_commands::ArchivePackCache>,
            pub(crate) duplicate_preparation: Mutex<crate::duplicate_commands::DuplicateCache>,
            pub(crate) forward_bidirectional_generation: AtomicU64,
            pub(crate) forward_bidirectional_preparing: AtomicBool,
            pub(crate) forward_bidirectional_runs:
                Mutex<HashMap<String, crate::forward_bidirectional_commands::QueuedForwardRun>>,
            /// Lazy geöffnet, damit ein beschädigtes Journal die App nicht unsichtbar beim
            /// Start beendet, sondern als klarer Command-Fehler in der Queue-UI erscheint.
            operation_queue: Mutex<Option<Arc<OperationQueue>>>,
            queue_worker_active: AtomicBool,
        }
        preparations {
            /// Nicht serialisierbare Realmodus-Vorbereitung samt gehaltenen Deskriptoren. Ein
            /// Neustart oder explizites Verwerfen lässt sie automatisch verschwinden.
            pending_real: Mutex<Option<PreparedLocalRealRun>> => "local-real",
            pending_local_sync: Mutex<crate::normal_sync_commands::LocalSyncPreparationSlot> => "local-sync",
            pending_content_sync: Mutex<Option<crate::content_sync_commands::PendingContentSync>> => "content-sync",
            /// Remote-Vorbereitungen und Verbindungen enthalten lebende Sitzungen und bleiben
            /// ausschließlich im Prozessspeicher. Profile auf Platte sind geheimnisfrei.
            pending_sftp_real: Mutex<Option<PendingRemoteRealRun>> => "sftp-real",
            pending_sftp_queue: Mutex<Option<PendingRemoteQueueRun>> => "sftp-queue-confirmation",
            pending_ftps_real: Mutex<Option<PendingRemoteRealRun>> => "ftps-real",
            pending_ftps_queue: Mutex<Option<PendingRemoteQueueRun>> => "ftps-queue-confirmation",
            pending_webdav_real: Mutex<Option<PendingRemoteRealRun>> => "webdav-real",
            pending_webdav_queue: Mutex<Option<PendingRemoteQueueRun>> => "webdav-queue-confirmation",
            pending_smb_real: Mutex<Option<PendingRemoteRealRun>> => "smb-real",
            pending_smb_queue: Mutex<Option<PendingRemoteQueueRun>> => "smb-queue-confirmation",
            /// Generische Provider→Provider-Vorbereitung. Auch hier bleiben beide lebenden
            /// Sitzungen und das Einmal-Permit ausschließlich im RAM.
            pending_provider_real: Mutex<Option<crate::provider_sync_commands::PendingProviderRun>> => "provider-real",
            pending_provider_queue: Mutex<Option<PendingRemoteQueueRun>> => "provider-queue-confirmation",
            pending_bidirectional: Mutex<Option<bidirectional_commands::PendingBidirectionalRun>> => "bidirectional-real",
            pending_forward_bidirectional: Mutex<Option<crate::forward_bidirectional_commands::PendingForwardRun>> => "forward-bidirectional",
            pending_bidirectional_queue: Mutex<Option<PendingBidirectionalQueueRun>> => "bidirectional-queue-confirmation",
            pending_bidirectional_rollback: Mutex<Option<bidirectional_commands::PendingBidirectionalRollback>> => "bidirectional-rollback",
        }
    }
}

impl AppState {
    /// Liefert für dasselbe Label stets denselben Fensterzustand. Der Zustand bleibt
    /// bis zum Prozessende erhalten, damit eine laufende Queue auch nach dem Schließen
    /// ihres Fensters sicher zu Ende laufen oder beim nächsten Start recovern kann.
    pub(crate) fn window(&self, label: &str) -> Arc<WindowState> {
        let mut windows = self.windows.lock_recover();
        windows
            .entry(label.to_owned())
            .or_insert_with(|| {
                Arc::new(WindowState {
                    exit_in_progress: self.exit_in_progress.clone(),
                    ..WindowState::default()
                })
            })
            .clone()
    }

    pub(crate) fn window_labels(&self) -> Vec<String> {
        self.windows.lock_recover().keys().cloned().collect()
    }
}

/// Lebt im Worker, nicht im wartenden Command-Future: Auch dessen Abbruch darf
/// den noch laufenden Cleanup nicht aus der Beenden-Erfassung entfernen.
pub(crate) struct QueueCleanupGuard(Arc<WindowState>);

impl Drop for QueueCleanupGuard {
    fn drop(&mut self) {
        self.0.queue_cleanups.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) fn reserve_queue_cleanup(
    state: &AppState,
    window: &Arc<WindowState>,
) -> Result<QueueCleanupGuard, AppError> {
    with_queue_extension(state, window, || {
        window.queue_cleanups.fetch_add(1, Ordering::AcqRel);
        Ok(QueueCleanupGuard(window.clone()))
    })
}

impl WindowState {
    pub(crate) fn has_active_operation(&self) -> bool {
        self.active_cancel.lock_recover().is_some()
            || self.queue_cleanups.load(Ordering::Acquire) != 0
            || self.resumable_copy_preparation.lock_recover().is_active()
    }

    fn operations_are_closing(&self) -> bool {
        self.exit_in_progress.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire)
    }
}

/// Lehnt eine neue Operation ab, solange dieses Fenster oder die App schließen.
fn reject_while_closing(state: &WindowState) -> Result<(), AppError> {
    if state.operations_are_closing() {
        return Err(AppError::new(
            "busy",
            "Fenster oder App werden gerade geschlossen; es kann keine neue Operation starten",
        ));
    }
    Ok(())
}

/// Genau eine mutierende Hintergrundoperation reservieren. Die Schranke sitzt im
/// Backend und kann daher nicht durch Doppelklicks oder direkte Command-Aufrufe umgangen
/// werden.
pub(crate) fn reserve_operation(state: &WindowState) -> Result<Cancel, AppError> {
    reject_while_closing(state)?;
    let mut active = state.active_cancel.lock_recover();
    // Das Gate kann zwischen der ersten Prüfung und dem Mutex-Lock gesetzt worden
    // sein. Die zweite Prüfung schließt genau dieses Rennen.
    reject_while_closing(state)?;
    if active.is_some() {
        return Err(AppError::new(
            "busy",
            "Es läuft bereits eine Datei- oder Sync-Operation",
        ));
    }
    let cancel = Cancel::new();
    *active = Some(cancel.clone());
    Ok(cancel)
}

fn ensure_extension_allowed(
    window: Option<&WindowState>,
    state: &AppState,
) -> Result<(), AppError> {
    if state.exit_in_progress.load(Ordering::Acquire)
        || window.is_some_and(|window| window.closing.load(Ordering::Acquire))
    {
        return Err(AppError::new(
            "busy",
            "Fenster oder App werden gerade geschlossen; die Queue wurde nicht erweitert",
        ));
    }
    Ok(())
}

/// Hält die appweite Exit-Barriere bis die Queue wirklich erweitert ist. Eine
/// Prüfung vor dem Lock reicht nicht, weil ein sauberer Exit sonst genau danach
/// seinen Snapshot lesen könnte.
pub(crate) fn with_queue_extension<T>(
    state: &AppState,
    window: &WindowState,
    operation: impl FnOnce() -> Result<T, AppError>,
) -> Result<T, AppError> {
    let _barrier = state.exit_barrier.lock_recover();
    ensure_extension_allowed(Some(window), state)?;
    operation()
}

/// Der Aufrufer erhält den Zugriff nur für die Dauer der gehaltenen Exit-Barriere.
pub(crate) struct QueueExtension<'a> {
    window: &'a WindowState,
}

pub(crate) fn with_queue_extension_access<T>(
    state: &AppState,
    window: &WindowState,
    operation: impl FnOnce(&QueueExtension<'_>) -> Result<T, AppError>,
) -> Result<T, AppError> {
    with_queue_extension(state, window, || operation(&QueueExtension { window }))
}

impl QueueExtension<'_> {
    /// Reiht eine bereits vollständig gebundene Operation ein. Meldet das atomare
    /// Schreiben erst nach dem Rename einen Fehler, klärt der Core den Stand anhand
    /// der vorab erzeugten Operation-ID und hält RAM und Journal zusammen. Die Tauri-
    /// Schicht deserialisiert das sicherheitskritische Queue-Journal nie selbst.
    pub(crate) fn enqueue_with_readback(
        &self,
        app: &tauri::AppHandle,
        label: &str,
        operation: &QueueOperation,
    ) -> Result<(Arc<OperationQueue>, QueueEntry), AppError> {
        let queue = operation_queue(app, self.window, label)?;
        let entry = queue
            .enqueue_reconciled(operation.clone())
            .map_err(AppError::from)?;
        Ok((queue, entry))
    }

    pub(crate) fn enqueue_with_profile_bindings(
        &self,
        app: &tauri::AppHandle,
        label: &str,
        operation: &QueueOperation,
        profile_bindings: &[RemoteProfileBinding],
    ) -> Result<(Arc<OperationQueue>, QueueEntry), AppError> {
        self.enqueue_with_profile_bindings_using(operation, profile_bindings, |bindings| {
            ensure_profile_bindings_current(app, bindings)?;
            operation_queue(app, self.window, label)
        })
    }

    pub(crate) fn enqueue_with_profile_bindings_using(
        &self,
        operation: &QueueOperation,
        profile_bindings: &[RemoteProfileBinding],
        prepare_queue: impl FnOnce(&[RemoteProfileBinding]) -> Result<Arc<OperationQueue>, AppError>,
    ) -> Result<(Arc<OperationQueue>, QueueEntry), AppError> {
        let queue = prepare_queue(profile_bindings)?;
        let entry = queue
            .enqueue_reconciled_with_profile_bindings(operation.clone(), profile_bindings.to_vec())
            .map_err(AppError::from)?;
        Ok((queue, entry))
    }

    pub(crate) fn enqueue_provider_file(
        &self,
        app: &tauri::AppHandle,
        label: &str,
        operation: QueueOperation,
        profile_bindings: Vec<RemoteProfileBinding>,
        generated_previous: Option<QueueEntry>,
        cancel: &Cancel,
    ) -> Result<(Arc<OperationQueue>, QueueEntry), AppError> {
        if let Some(previous) = generated_previous {
            let queue = operation_queue(app, self.window, label)?;
            let entry =
                queue.resume_generated_upload(&previous, operation, profile_bindings, cancel)?;
            Ok((queue, entry))
        } else {
            self.enqueue_with_profile_bindings(app, label, &operation, &profile_bindings)
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnqueueRemoteSyncRequest<C> {
    profile_id: String,
    #[serde(default)]
    profile_revision: Option<u64>,
    direction: SftpQueueDirection,
    local_root: String,
    remote_root: String,
    options: SyncOptions,
    expected_plan: SyncPlan,
    #[serde(skip)]
    connection_type: PhantomData<C>,
}

#[cfg_attr(test, derive(Clone))]
pub(crate) enum RemoteSyncConnection {
    Sftp(Box<SftpConnectionConfig>),
    Ftps(Box<FtpsConnectionConfig>),
    WebDav(Box<WebDavConnectionConfig>),
    Smb(Box<SmbConnectionConfig>),
}

impl From<SftpConnectionConfig> for RemoteSyncConnection {
    fn from(config: SftpConnectionConfig) -> Self {
        Self::Sftp(Box::new(config))
    }
}

impl From<FtpsConnectionConfig> for RemoteSyncConnection {
    fn from(config: FtpsConnectionConfig) -> Self {
        Self::Ftps(Box::new(config))
    }
}

impl From<WebDavConnectionConfig> for RemoteSyncConnection {
    fn from(config: WebDavConnectionConfig) -> Self {
        Self::WebDav(Box::new(config))
    }
}

impl From<SmbConnectionConfig> for RemoteSyncConnection {
    fn from(config: SmbConnectionConfig) -> Self {
        Self::Smb(Box::new(config))
    }
}

pub(crate) fn enqueue_protocol_sync<C: Into<RemoteSyncConnection>>(
    app: &tauri::AppHandle,
    label: &str,
    state: &AppState,
    window: &WindowState,
    request: EnqueueRemoteSyncRequest<C>,
    resolve_connection: impl FnOnce(&str) -> Result<C, AppError>,
) -> Result<Arc<OperationQueue>, AppError> {
    enqueue_protocol_sync_using(state, window, request, resolve_connection, |bindings| {
        ensure_profile_bindings_current(app, bindings)?;
        operation_queue(app, window, label)
    })
}

fn enqueue_protocol_sync_using<C: Into<RemoteSyncConnection>>(
    state: &AppState,
    window: &WindowState,
    request: EnqueueRemoteSyncRequest<C>,
    resolve_connection: impl FnOnce(&str) -> Result<C, AppError>,
    prepare_queue: impl FnOnce(&[RemoteProfileBinding]) -> Result<Arc<OperationQueue>, AppError>,
) -> Result<Arc<OperationQueue>, AppError> {
    with_queue_extension_access(state, window, |extension| {
        // Dieselbe typisierte Verbindung bestimmt den Slot und die angeforderte
        // Operation. Der Command kann keinen abweichenden Pending-Slot übergeben.
        macro_rules! bind_protocol {
            ($config:expr, $protocol:ident, $slot:ident, $name:literal, $operation:ident) => {
                (
                    RemoteProfileProtocol::$protocol,
                    &window.$slot,
                    $name,
                    QueueOperation::$operation {
                        direction: request.direction,
                        connection: $config,
                        local_root: request.local_root,
                        remote_root: request.remote_root,
                        options: request.options,
                        expected_plan: Box::new(request.expected_plan),
                    },
                )
            };
        }
        let (protocol, slot, name, requested_operation) =
            match resolve_connection(&request.profile_id)?.into() {
                RemoteSyncConnection::Sftp(config) => {
                    bind_protocol!(config, Sftp, pending_sftp_real, "SFTP", SftpSync)
                }
                RemoteSyncConnection::Ftps(config) => {
                    bind_protocol!(config, Ftps, pending_ftps_real, "FTPS", FtpsSync)
                }
                RemoteSyncConnection::WebDav(config) => {
                    bind_protocol!(config, Webdav, pending_webdav_real, "WebDAV", WebDavSync)
                }
                RemoteSyncConnection::Smb(config) => {
                    bind_protocol!(config, Smb, pending_smb_real, "SMB", SmbSync)
                }
            };
        let mut pending = slot.lock_recover();
        let prepared = pending.as_ref().ok_or_else(|| {
            AppError::new(
                "app",
                format!("Keine {name}-Vorbereitung zum Einreihen vorhanden"),
            )
        })?;
        ensure_pending_profile_binding(
            &prepared.profile_bindings,
            protocol,
            &request.profile_id,
            request.profile_revision,
        )?;
        if requested_operation != prepared.operation {
            return Err(AppError::new(
                "plan_drift",
                format!("{name}-Queueanfrage stimmt nicht exakt mit der vorbereiteten Operation überein"),
            ));
        }
        let (queue, _) = extension.enqueue_with_profile_bindings_using(
            &prepared.operation,
            &prepared.profile_bindings,
            prepare_queue,
        )?;
        pending.take();
        Ok(queue)
    })
}

/// Startet eine bereits vorbereitete Operation mit einheitlicher Lockreihenfolge:
/// zuerst Exit-Barriere, dann Pending-Slot, zuletzt die Single-Flight-Reservierung.
/// Die Prüfung läuft noch am geliehenen Wert. Erst wenn Prüfung und Reservierung
/// erfolgreich sind, wird die nicht serialisierbare Vorbereitung entnommen.
pub(crate) fn take_pending_for_operation<T, C>(
    state: &AppState,
    window: &WindowState,
    pending: &Mutex<Option<T>>,
    missing_message: &str,
    prepare_context: impl FnOnce(&T) -> Result<C, AppError>,
) -> Result<(Cancel, T, C), AppError> {
    with_queue_extension(state, window, || {
        let mut pending = pending.lock_recover();
        let prepared = pending
            .as_ref()
            .ok_or_else(|| AppError::new("app", missing_message))?;
        let context = prepare_context(prepared)?;
        let cancel = reserve_operation(window)?;
        let prepared = pending.take().expect("unmittelbar zuvor geprüft");
        Ok((cancel, prepared, context))
    })
}

/// Dasselbe Gate für appweite Erweiterungen ohne vorhandenen Fensterzustand,
/// derzeit das Erzeugen eines neuen Fensters.
pub(crate) fn with_app_extension<T>(
    state: &AppState,
    operation: impl FnOnce() -> Result<T, AppError>,
) -> Result<T, AppError> {
    let _barrier = state.exit_barrier.lock_recover();
    ensure_extension_allowed(None, state)?;
    operation()
}

pub(crate) struct ProfileTransactionGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    active: &'a AtomicBool,
    owner: &'a Mutex<Option<String>>,
}

impl Drop for ProfileTransactionGuard<'_> {
    fn drop(&mut self) {
        self.owner.lock_recover().take();
        self.active.store(false, Ordering::Release);
    }
}

pub(crate) fn profile_transaction_owned_by(state: &AppState, label: &str) -> bool {
    state.profile_transaction_owner.lock_recover().as_deref() == Some(label)
}

/// Beginnt die appweite Profil-/Secret-Transaktion ohne Exit-Rennen. Die
/// Exit-Barriere bleibt bis zum gesetzten Besitzer- und Aktivitätszeichen gehalten.
/// Deshalb sieht ein späterer Close/Exit die Transaktion vollständig; ein bereits
/// begonnenes Close/Exit setzt dagegen zuerst sein Gate und verhindert den Start.
pub(crate) fn begin_profile_transaction<'a>(
    state: &'a AppState,
    owner: &str,
    window: &WindowState,
) -> Result<ProfileTransactionGuard<'a>, AppError> {
    let busy = || {
        AppError::new(
            "busy",
            "Die App prüft gerade das Beenden; das Verbindungsprofil wurde nicht gespeichert",
        )
    };
    // Dieselbe Barriere setzt CloseRequested vor `closing` und ExitRequested vor
    // seinem Snapshot. Damit gewinnt genau eine Seite: Entweder ist die
    // Transaktion samt Besitzer sichtbar, oder sie beginnt nach dem Gate nicht.
    let _barrier = state.exit_barrier.lock_recover();
    ensure_extension_allowed(Some(window), state).map_err(|_| busy())?;
    let guard = match state.profile_secret_guard.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err(AppError::new("busy",
                "Ein anderes Fenster bearbeitet gerade ein Verbindungsprofil; bitte anschließend erneut versuchen"));
        }
    };
    ensure_extension_allowed(Some(window), state).map_err(|_| busy())?;
    *state.profile_transaction_owner.lock_recover() = Some(owner.to_owned());
    state
        .profile_transaction_active
        .store(true, Ordering::Release);
    Ok(ProfileTransactionGuard {
        _guard: guard,
        active: &state.profile_transaction_active,
        owner: &state.profile_transaction_owner,
    })
}

/// Direkte Mutation unter dieselbe Single-Flight-Schranke wie Sync und
/// Dateioperationen stellen.
pub(crate) fn with_reserved_operation<T>(
    app: &tauri::AppHandle,
    label: &str,
    state: &WindowState,
    operation: impl FnOnce() -> Result<T, AppError>,
) -> Result<T, AppError> {
    // Auch kurze Direktmutationen brauchen den app-ordnerweiten Prozessbesitz. Eine
    // zweite Doppeldecker-Instanz darf die Queue-Sperre nicht über Nebencommands umgehen.
    operation_queue(app, state, label)?;
    let result = run_reserved_cancellable(state, |_| operation(), |_| {})?;
    start_queue_worker(app.clone(), label.to_owned());
    result
}

/// Direkte, abbrechbare Arbeit: Fehler vor der Reservierung bleiben außen;
/// das innere Resultat wurde bereits an das verursachende Fenster gemeldet.
pub(crate) fn with_reserved_cancellable_operation<T>(
    app: &tauri::AppHandle,
    label: &str,
    state: &WindowState,
    operation: impl FnOnce(&Cancel) -> Result<T, AppError>,
    terminal: impl FnOnce(&Result<T, AppError>),
) -> Result<Result<T, AppError>, AppError> {
    operation_queue(app, state, label)?;
    let result = run_reserved_cancellable(state, operation, terminal)?;
    start_queue_worker(app.clone(), label.to_owned());
    Ok(result)
}

fn run_reserved_cancellable<T>(
    state: &WindowState,
    operation: impl FnOnce(&Cancel) -> Result<T, AppError>,
    terminal: impl FnOnce(&Result<T, AppError>),
) -> Result<Result<T, AppError>, AppError> {
    let cancel = reserve_operation(state)?;
    // Auch ein unerwarteter Worker-Panic darf die Fensterreservierung nicht
    // dauerhaft belegen. Der Panictext kann Providerdaten enthalten und wird
    // deshalb nicht in den fachlichen Fehler übernommen.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(&cancel)))
        .unwrap_or_else(|_| Err(AppError::new("app", "Hintergrundarbeit wurde unterbrochen")));
    state.active_cancel.lock_recover().take();
    terminal(&result);
    Ok(result)
}

fn finish_operation_in_order(
    release_reservation: impl FnOnce(),
    emit_terminal_event: impl FnOnce(),
    continue_queue: impl FnOnce(),
) {
    release_reservation();
    emit_terminal_event();
    continue_queue();
}

fn finish_queue_entry_in_order(
    release_reservation: impl FnOnce(),
    emit_terminal_snapshot: impl FnOnce(),
    emit_terminal_event: impl FnOnce(),
) {
    release_reservation();
    emit_terminal_snapshot();
    emit_terminal_event();
}

/// Beendet eine direkte Operation in einer festen, nicht vertauschbaren Reihenfolge.
/// Der nächste Queueworker darf erst starten, nachdem das Frontend das Terminalevent
/// des vorherigen Laufs erhalten hat; sonst kann dessen Fortschritt bereits den neuen
/// Eintrag zeigen, bevor der alte Abschluss sichtbar wurde.
pub(crate) fn finish_operation(
    app: &tauri::AppHandle,
    label: &str,
    emit_terminal_event: impl FnOnce(),
) {
    let state = app.state::<AppState>().window(label);
    finish_operation_in_order(
        || {
            state.active_cancel.lock_recover().take();
        },
        emit_terminal_event,
        || start_queue_worker(app.clone(), label.to_owned()),
    );
}

/// Fail-closed gegen einen leeren oder relativen Elternpfad.
///
/// `map_virtual_path` (core/src/sftp.rs) hängt einen relativen Pfad an die kanonische
/// Provider-Root; ein Leerstring träfe damit die Root selbst. Genau diesen Leerstring
/// liefert das Frontend, solange eine Pane nach einem gescheiterten Listing ohne Pfad
/// dasteht. Der neue Ordner entstünde dann an einer Stelle, die der Nutzer nie gesehen
/// hat. Die Absage gehört deshalb ins Backend und nicht nur ins Frontend.
pub(crate) fn require_absolute_remote_parent(parent: &str) -> Result<(), AppError> {
    if !Path::new(parent).is_absolute() {
        return Err(AppError::new(
            "invalid_input",
            "Ordner anlegen braucht einen absoluten Elternpfad auf dem Server",
        ));
    }
    Ok(())
}

/// Legt interaktiv einen Ordner auf einer bereits gebundenen Remote-Sitzung an.
///
/// Die gewöhnliche Ordneraddition besitzt dieselbe fenstereigene Reservierung
/// wie andere direkte Aktionen. Der Core wählt die konkrete Providerwirkung;
/// insbesondere darf WebDAV dafür keine künstliche Löschprobe erben.
pub(crate) fn create_remote_folder(
    app: &tauri::AppHandle,
    label: &str,
    state: &WindowState,
    endpoint: &doppeldecker_core::ProviderEndpoint,
    name: &str,
    expected_class: doppeldecker_core::DirectoryCreateExecutionClass,
    before: impl FnOnce() -> Result<(), AppError>,
) -> Result<doppeldecker_core::DirectoryCreateReport, AppError> {
    let name = doppeldecker_core::validate_directory_create_request(endpoint.root(), name)
        .map_err(AppError::from)?;
    doppeldecker_core::validate_directory_create_class(endpoint.kind(), expected_class)
        .map_err(AppError::from)?;
    with_reserved_operation(app, label, state, || {
        before()?;
        let queue = operation_queue(app, state, label)?;
        let endpoint = queue
            .bind_provider_preparation(endpoint)
            .map_err(AppError::from)?;
        endpoint
            .create_interactive_directory(&name, expected_class)
            .map_err(AppError::from)
    })
}

pub(crate) fn ready_queue_snapshot(
    state: &WindowState,
) -> Option<doppeldecker_core::QueueSnapshot> {
    let queue = state.operation_queue.lock_recover().clone()?;
    Some(queue.snapshot())
}

pub(crate) fn operation_queue(
    app: &tauri::AppHandle,
    state: &WindowState,
    label: &str,
) -> Result<Arc<OperationQueue>, AppError> {
    let mut slot = state.operation_queue.lock_recover();
    if let Some(queue) = slot.as_ref() {
        return Ok(queue.clone());
    }
    let path = app_config_dir(app)?.join(queue_journal_name(label)?);
    let process_lock = acquire_shared_process_lock(&app.state::<AppState>(), &path)?;
    let queue = Arc::new(
        OperationQueue::open_with_process_lock(path, process_lock).map_err(AppError::from)?,
    );
    *slot = Some(queue.clone());
    Ok(queue)
}

/// Erwirbt die gemeinsame GUI-/CLI-Prozesssperre bereits im Tauri-Setup. Der
/// Ankerpfad dient dem Core nur zur Bestimmung des Konfigurationsordners; weder ein
/// Hauptfensterjournal noch dessen Format wird dabei geöffnet oder migriert.
pub(crate) fn initialize_queue_process_lock(app: &tauri::AppHandle) -> Result<(), AppError> {
    let config_dir = app_config_dir(app)?;
    initialize_queue_process_lock_at(&app.state::<AppState>(), &config_dir)
}

fn initialize_queue_process_lock_at(state: &AppState, config_dir: &Path) -> Result<(), AppError> {
    let anchor = config_dir.join(".operations-queue-process-lock-anchor");
    acquire_shared_process_lock(state, &anchor).map(|_| ())
}

fn acquire_shared_process_lock(
    state: &AppState,
    queue_path: &Path,
) -> Result<Arc<OperationQueueProcessLock>, AppError> {
    if let Some(process_lock) = state.queue_process_lock.get() {
        return Ok(process_lock.clone());
    }
    let _initialization = state.queue_process_lock_init.lock_recover();
    if let Some(process_lock) = state.queue_process_lock.get() {
        return Ok(process_lock.clone());
    }
    let process_lock = Arc::new(
        OperationQueueProcessLock::acquire(queue_path)
            .map_err(|error| AppError::new("queue", error.to_string()))?,
    );
    // Unter der Initialisierungssperre kann kein zweiter erfolgreicher Set laufen.
    // Sollte sich diese Invariante später ändern, verwenden wir fail-closed den
    // bereits gecachten Besitzer und lassen den lokalen Arc sauber fallen.
    if state.queue_process_lock.set(process_lock.clone()).is_err() {
        return state.queue_process_lock.get().cloned().ok_or_else(|| {
            AppError::new(
                "queue",
                "Prozesslock konnte nicht eindeutig gebunden werden",
            )
        });
    }
    Ok(process_lock)
}

/// Das Hauptfenster übernimmt das bisherige Journal ohne Migration. Weitere
/// Fenster bekommen ein eigenes, stabiles Journal. Labels werden nicht als freie
/// Pfade akzeptiert, damit ein Fenstername nie den Konfigurationsordner verlassen kann.
pub(crate) fn queue_journal_name(label: &str) -> Result<String, AppError> {
    if label == "main" {
        return Ok("operations-queue.json".into());
    }
    if label.is_empty()
        || label.len() > 64
        || !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(AppError::new(
            "invalid_input",
            "Fensterlabel ist für ein Queue-Journal ungeeignet",
        ));
    }
    Ok(format!("operations-queue-{label}.json"))
}

/// Nebenfenster schreiben auch ihre Auditdateien getrennt. Das verhindert, dass
/// zwei parallele Läufe ihre JSON-Lines im selben Dateideskriptor verschachteln.
pub(crate) fn window_data_file_name(base: &str, label: &str) -> Result<String, AppError> {
    queue_journal_name(label)?;
    if label == "main" {
        return Ok(base.to_owned());
    }
    let (stem, extension) = base.rsplit_once('.').ok_or_else(|| {
        AppError::new(
            "invalid_input",
            "Fensterbezogene Datendatei braucht eine Erweiterung",
        )
    })?;
    Ok(format!("{stem}-{label}.{extension}"))
}

/// Sendet ein Ereignis an genau das Fenster, das es verursacht hat.
///
/// `Emitter::emit` ist in Tauri v2 **kein** fensterbezogener Versand: Auch auf einem
/// `WebviewWindow` läuft er über den prozessweiten `AppManager` und erreicht jeden
/// Webview (tauri 2.11.5, `Emitter::emit` → `manager().emit`). Für Doppeldecker ist
/// das ein Vertragsbruch — Fortschritt, Abschluss, Fehler und Menüaktionen gehören
/// ausschließlich zum verursachenden Fenster. Sonst entsperrt der Abschluss eines
/// fremden Laufs die Oberfläche eines noch arbeitenden Fensters, und ein Menübefehl
/// löst dieselbe Dateiaktion in jedem Fenster auf dessen eigener Auswahl aus.
///
/// Prozessweite Zustände sind die ausdrückliche Ausnahme und bleiben ein Rundruf:
/// die Verbindungsänderung in `connection_state_commands::emit_connection_update`.
pub(crate) fn emit_to_window<S>(
    window: &tauri::WebviewWindow,
    event: &str,
    payload: S,
) -> tauri::Result<()>
where
    S: serde::Serialize + Clone,
{
    emit_to_window_label(window.app_handle(), window.label(), event, payload)
}

/// Wie [`emit_to_window`], aber wenn nur App-Handle und Label vorliegen. Der
/// Fenster-Event-Handler bekommt ein `tauri::Window` statt des Webviews.
fn webview_event_target(label: &str) -> tauri::EventTarget {
    tauri::EventTarget::webview(label)
}

#[cfg(test)]
mod event_target_tests {
    #[test]
    fn fensterereignis_trifft_nur_das_webview_mit_diesem_namen() {
        assert!(matches!(
            super::webview_event_target("window-2"),
            tauri::EventTarget::Webview { label } if label == "window-2"
        ));
    }
}

pub(crate) fn emit_to_window_label<R, S>(
    app: &tauri::AppHandle<R>,
    label: &str,
    event: &str,
    payload: S,
) -> tauri::Result<()>
where
    R: tauri::Runtime,
    S: serde::Serialize + Clone,
{
    // Die Frontend-Brücke registriert passend dazu jeden Listener am aktuellen
    // Webview. Ein prozessweiter JavaScript-Listener würde auch einen gezielt
    // versandten Menübefehl in jedem geöffneten Fenster empfangen.
    app.emit_to(
        webview_event_target(label),
        event,
        crate::frontend_payload::FrontendPayload(payload),
    )
}

pub(crate) fn emit_queue_snapshot<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    label: &str,
    queue: &OperationQueue,
) {
    let _ = emit_to_window_label(app, label, "queue-updated", queue.snapshot());
}

/// Ein verhindertes Schließen öffnet das Gate und weckt bereits bestätigte Arbeit
/// wieder. Der Aufrufer hält die Exit-Barriere bis nach dieser Entscheidung.
pub(crate) fn reopen_window_operation_gate(
    app: &tauri::AppHandle,
    label: &str,
    state: &WindowState,
) {
    state.closing.store(false, Ordering::Release);
    wake_runnable_queue(app, label, state);
}

pub(crate) fn reopen_app_operation_gate(app: &tauri::AppHandle, state: &AppState) {
    state.exit_in_progress.store(false, Ordering::Release);
    let windows: Vec<_> = state
        .windows
        .lock_recover()
        .iter()
        .map(|(label, window)| (label.clone(), window.clone()))
        .collect();
    for (label, window) in windows {
        wake_runnable_queue(app, &label, &window);
    }
}

fn wake_runnable_queue(app: &tauri::AppHandle, label: &str, state: &WindowState) {
    let queue = state.operation_queue.lock_recover().clone();
    if !state.operations_are_closing() && queue.is_some_and(|queue| queue.has_runnable()) {
        start_queue_worker(app.clone(), label.to_owned());
    }
}

fn try_claim_queue_worker(state: &WindowState) -> bool {
    !state.operations_are_closing() && !state.queue_worker_active.swap(true, Ordering::SeqCst)
}

/// Genau einen Worker pro Fenster starten. Jeder Worker claimt ausschließlich sein
/// eigenes Journal; Worker anderer Fenster dürfen gleichzeitig mutieren.
pub(crate) fn start_queue_worker(app: tauri::AppHandle, label: String) {
    let state = app.state::<AppState>().window(&label);
    if !try_claim_queue_worker(&state) {
        return;
    }
    std::thread::spawn(move || {
        let queue = match operation_queue(&app, &state, &label) {
            Ok(queue) => queue,
            Err(error) => {
                let _ = emit_to_window_label(&app, &label, "queue-error", error);
                state.queue_worker_active.store(false, Ordering::SeqCst);
                return;
            }
        };
        let trash_journal_dir = match app_config_dir(&app) {
            Ok(path) => path.join("trash-journals"),
            Err(error) => {
                let _ = emit_to_window_label(&app, &label, "queue-error", error.message);
                state.queue_worker_active.store(false, Ordering::SeqCst);
                return;
            }
        };
        let app_state = app.state::<AppState>();
        let recovery = app_state.trash_recovery.get_or_init(|| {
            // Dieselbe prozessweite Sperre wie die Papierkorboperation selbst.
            let _journals = app_state.trash_journal_guard.lock_recover();
            match recover_trash_journals(&trash_journal_dir) {
                Ok(recovery) if recovery.is_clean() => Ok(()),
                Ok(recovery) => Err(format!(
                    "Papierkorb-Recovery ist vor Queue-Start ungeklärt: {}{}",
                    recovery.recovery_paths.join(", "),
                    recovery.errors.join("; ")
                )),
                Err(error) => Err(error.to_string()),
            }
        });
        if let Err(error) = recovery {
            let _ = emit_to_window_label(&app, &label, "queue-error", error);
            state.queue_worker_active.store(false, Ordering::SeqCst);
            return;
        }
        let mut stopped_on_error = false;
        loop {
            if !queue.has_runnable() {
                break;
            }
            let cancel = match reserve_operation(&state) {
                Ok(cancel) => cancel,
                Err(_) => break,
            };
            let entry = match queue.claim_next() {
                Ok(Some(entry)) => entry,
                Ok(None) => {
                    state.active_cancel.lock_recover().take();
                    break;
                }
                Err(error) => {
                    state.active_cancel.lock_recover().take();
                    stopped_on_error = true;
                    let _ =
                        emit_to_window_label(&app, &label, "queue-error", AppError::from(error));
                    break;
                }
            };
            let context = crate::operation_events::OperationViewContext::for_entry(
                trash_journal_dir.parent().expect("Konfigurationsordner"),
                &entry,
            );
            let _ = emit_to_window_label(&app, &label, "operation-context", &context);
            emit_queue_snapshot(&app, &label, &queue);
            let terminal = execute_queue_entry(&app, &label, &queue, entry, &cancel, &context);
            finish_queue_entry_in_order(
                || {
                    state.active_cancel.lock_recover().take();
                },
                || emit_queue_snapshot(&app, &label, &queue),
                || terminal.emit_to_window(&app, &label, &context),
            );
        }
        state.queue_worker_active.store(false, Ordering::SeqCst);
        emit_queue_snapshot(&app, &label, &queue);
        // Schließt das kleine Rennen zwischen letztem `has_runnable` und einem
        // gleichzeitigen Enqueue. Nach einem Claimfehler darf dagegen
        // kein unbegrenzter automatischer Retry entstehen. Ein ausdrücklicher
        // Bedienbefehl kann nach Fehlerbehebung wieder einen Worker starten.
        if queue_worker_restart_allowed(
            &state,
            stopped_on_error,
            queue.has_runnable(),
            state.active_cancel.lock_recover().is_some(),
        ) {
            start_queue_worker(app, label);
        }
    });
}

fn queue_worker_restart_allowed(
    state: &WindowState,
    stopped_on_error: bool,
    has_runnable: bool,
    operation_active: bool,
) -> bool {
    !state.operations_are_closing() && !stopped_on_error && has_runnable && !operation_active
}

fn execute_queue_entry(
    app: &tauri::AppHandle,
    label: &str,
    queue: &OperationQueue,
    entry: QueueEntry,
    cancel: &Cancel,
    context: &crate::operation_events::OperationViewContext,
) -> QueueTerminalEvent {
    let operation_id = entry.operation_id.clone();
    let local_file = matches!(
        entry.operation,
        QueueOperation::File { .. }
            | QueueOperation::ResumableCopy { .. }
            | QueueOperation::ArchivePack { .. }
            | QueueOperation::ArchiveExtract { .. }
    );
    let local_sync = matches!(entry.operation, QueueOperation::LocalSync { .. });
    let content_sync = matches!(entry.operation, QueueOperation::ContentSync { .. });
    let resumable_copy = matches!(entry.operation, QueueOperation::ResumableCopy { .. });
    let forward_bidirectional =
        matches!(entry.operation, QueueOperation::BidirectionalForward { .. });
    let provider_file = matches!(
        entry.operation,
        QueueOperation::ProviderFile { .. } | QueueOperation::ArchiveUpload { .. }
    ) || matches!(&entry.operation, QueueOperation::ResumableCopy { plan, .. } if plan.remote_endpoint().is_some());
    let provider_delete = matches!(entry.operation, QueueOperation::ProviderDelete { .. });
    if matches!(entry.operation, QueueOperation::SftpSync { .. }) {
        return sftp_commands::prepare_remote_queue_entry(app, label, queue, entry);
    }
    if matches!(entry.operation, QueueOperation::FtpsSync { .. }) {
        return ftps_commands::prepare_remote_queue_entry(app, label, queue, entry);
    }
    if matches!(entry.operation, QueueOperation::WebDavSync { .. }) {
        return webdav_commands::prepare_remote_queue_entry(app, label, queue, entry);
    }
    if matches!(entry.operation, QueueOperation::SmbSync { .. }) {
        return smb_commands::prepare_remote_queue_entry(app, label, queue, entry);
    }
    if matches!(entry.operation, QueueOperation::ProviderSync { .. }) {
        return provider_sync_commands::prepare_remote_queue_entry(app, label, queue, entry);
    }
    if matches!(entry.operation, QueueOperation::Bidirectional { .. }) {
        return bidirectional_commands::prepare_queue_entry(app, label, queue, entry);
    }
    // Teilkopien prüfen das Profil im Core, nachdem ein möglicher lokaler
    // Abschluss wiedergefunden wurde.
    if let Err(error) = if resumable_copy {
        Ok(())
    } else {
        ensure_profile_bindings_current(app, &entry.profile_bindings)
    } {
        if forward_bidirectional {
            crate::forward_bidirectional_commands::interrupt_failed_preparation(
                queue, &entry, &error,
            );
        } else {
            let _ = queue.fail(&operation_id, Progress::default(), error.message.clone());
        }
        return QueueTerminalEvent::with_progress(None, QueueTerminalOutcome::AppError(error));
    }
    let class = match &entry.operation {
        QueueOperation::RenameCycleRecovery { .. }
        | QueueOperation::FtpsRecovery { .. }
        | QueueOperation::File { .. }
        | QueueOperation::ResumableCopy { .. }
        | QueueOperation::ArchivePack { .. }
        | QueueOperation::ArchiveExtract { .. }
        | QueueOperation::ArchiveUpload { .. }
        | QueueOperation::ProviderFile { .. }
        | QueueOperation::ProviderDelete { .. } => doppeldecker_core::QueueOperationClass::File,
        QueueOperation::Sync { .. }
        | QueueOperation::LocalSync { .. }
        | QueueOperation::ContentSync { .. }
        | QueueOperation::BidirectionalForward { .. } => {
            doppeldecker_core::QueueOperationClass::Sync
        }
        QueueOperation::SftpSync { .. } => unreachable!("oben behandelt"),
        QueueOperation::FtpsSync { .. } => unreachable!("oben behandelt"),
        QueueOperation::WebDavSync { .. } => unreachable!("oben behandelt"),
        QueueOperation::SmbSync { .. } => unreachable!("oben behandelt"),
        QueueOperation::ProviderSync { .. } => unreachable!("oben behandelt"),
        QueueOperation::Bidirectional { .. } => unreachable!("oben behandelt"),
    };
    let sandbox = match sandbox_root() {
        Ok(sandbox) => sandbox,
        Err(error) => {
            // Das Queue-Journal speichert nur den Anzeigetext; der Code bleibt
            // ein Konzept der Command-Schnittstelle.
            if forward_bidirectional {
                crate::forward_bidirectional_commands::interrupt_failed_preparation(
                    queue, &entry, &error,
                );
            } else {
                let _ = queue.fail(
                    &entry.operation_id,
                    Progress::default(),
                    error.message.clone(),
                );
            }
            return QueueTerminalEvent::with_progress(None, QueueTerminalOutcome::AppError(error));
        }
    };
    // Ein persistierter Dry-Run bleibt auch im markierten Testprozess rein lesend.
    let execution_sandbox = if matches!(entry.operation, QueueOperation::Sync { sandbox: None, .. })
    {
        None
    } else {
        sandbox.clone()
    };
    let execution = doppeldecker_core::LocalQueueExecution {
        sync_quarantine: sandbox
            .as_ref()
            .map(|root| quarantine_for(root, label, &operation_id)),
        sandbox: execution_sandbox,
        max_delete_entries: MAX_DELETE_ENTRIES,
        max_delete_fraction: MAX_DELETE_FRACTION,
    };
    let test_copy_delay = sandbox.as_ref().and_then(|_| {
        std::env::var("DOPPELDECKER_TEST_COPY_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|milliseconds| *milliseconds <= 100)
            .map(Duration::from_millis)
    });
    let mut announced_running = false;
    let mut last_event = Instant::now() - Duration::from_secs(1);
    let mut terminal_progress = None;
    let mut progress_callback = |progress: &Progress| {
        let terminal = matches!(progress.phase.as_str(), "done" | "cancelled");
        if terminal {
            terminal_progress = Some((class, progress.clone()));
            return;
        }
        if !announced_running || progress.phase == "archive_upload_ready" {
            announced_running = true;
            emit_queue_snapshot(app, label, queue);
        }
        if class == doppeldecker_core::QueueOperationClass::File || local_sync {
            if let Some(delay) = test_copy_delay {
                std::thread::sleep(delay);
            }
        }
        if last_event.elapsed() >= Duration::from_millis(50) {
            last_event = Instant::now();
            let event = match class {
                doppeldecker_core::QueueOperationClass::File => "fileop-progress",
                doppeldecker_core::QueueOperationClass::Sync => "sync-progress",
            };
            let _ = emit_to_window_label(app, label, event, context.wrap(progress));
        }
    };
    let result = if forward_bidirectional {
        let result = crate::forward_bidirectional_commands::execute_queue_entry(
            app,
            label,
            queue,
            &entry,
            cancel,
            &mut progress_callback,
        );
        if let Err(error) = &result {
            if let Some(current) = queue.snapshot().entries.into_iter().find(|entry| {
                entry.operation_id == operation_id
                    && matches!(
                        entry.state,
                        doppeldecker_core::QueueState::Validating
                            | doppeldecker_core::QueueState::Running
                    )
            }) {
                let _ =
                    queue.interrupt_bidirectional_forward(&operation_id, current.progress, error);
            }
        }
        result
    } else if content_sync {
        crate::content_sync_commands::execute_queue_entry(
            app,
            label,
            queue,
            &entry,
            cancel,
            &mut progress_callback,
        )
    } else if local_sync {
        crate::normal_sync_commands::execute_queue_entry(
            app,
            label,
            queue,
            &entry,
            cancel,
            &mut progress_callback,
        )
    } else if provider_delete {
        doppeldecker_core::execute_provider_delete_queue_entry(
            queue,
            entry,
            std::sync::Arc::new(crate::sftp_commands::KeyringResolver),
            cancel,
            &mut progress_callback,
        )
    } else if provider_file || local_file {
        let snapshot_store = if matches!(&entry.operation, QueueOperation::ArchiveExtract { request, .. } if request.snapshot.is_some())
            || matches!(&entry.operation, QueueOperation::ArchiveUpload {generation,..} if generation.uses_snapshot())
        {
            let opened = crate::browser_commands::snapshot_cache(app, "archive-snapshots")
                .map_err(|error| {
                    let code = serde_json::from_value(serde_json::Value::String(error.code))
                        .unwrap_or(doppeldecker_core::ErrorCode::Io);
                    doppeldecker_core::coded_error(std::io::ErrorKind::Other, code, error.message)
                })
                .and_then(|root| doppeldecker_core::remote_snapshot::SnapshotStore::open(&root));
            match opened {
                Ok(store) => Some(store),
                Err(error) => {
                    let error = doppeldecker_core::coded_error(error.kind(), doppeldecker_core::error_code(&error),
                            format!("Archivmomentaufnahme ist nicht verfügbar; Archiv erneut öffnen: {error}"));
                    let error = match queue.fail_file_coded(&operation_id, Progress::default(), &error, &[]) {
                            Ok(()) => error,
                            Err(persistence_error) => doppeldecker_core::coded_error(persistence_error.kind(), doppeldecker_core::error_code(&persistence_error), format!("Cacheprüfung scheiterte ({error}); Queueabschluss ebenfalls fehlgeschlagen: {persistence_error}")),
                        };
                    return QueueTerminalEvent::with_progress(
                        None,
                        QueueTerminalOutcome::AppError(error.into()),
                    );
                }
            }
        } else {
            None
        };

        let detailed = if provider_file {
            doppeldecker_core::queue::execute_provider_file_queue_entry_detailed_with_snapshot_store(
                queue,
                entry,
                &execution,
                std::sync::Arc::new(crate::sftp_commands::KeyringResolver),
                snapshot_store.as_ref(),
                cancel,
                &mut progress_callback,
            )
        } else {
            doppeldecker_core::queue::execute_local_queue_entry_detailed_with_snapshot_store(
                queue,
                entry,
                &execution,
                snapshot_store.as_ref(),
                cancel,
                &mut progress_callback,
            )
        };
        match detailed {
            Ok(result) => Ok(result),
            Err(failure) => finalize_detailed_file_failure(queue, &operation_id, failure),
        }
    } else {
        doppeldecker_core::execute_local_queue_entry(
            queue,
            entry,
            &execution,
            cancel,
            &mut progress_callback,
        )
    };
    let outcome = match result {
        Ok(doppeldecker_core::QueueExecutionResult::File(stats)) => {
            QueueTerminalOutcome::FileDone(stats)
        }
        Ok(doppeldecker_core::QueueExecutionResult::Sync(stats)) => {
            QueueTerminalOutcome::SyncDone(stats)
        }
        Ok(doppeldecker_core::QueueExecutionResult::Bidirectional(stats)) => {
            QueueTerminalOutcome::BidirectionalDone(stats)
        }
        Ok(doppeldecker_core::QueueExecutionResult::AwaitingConfirmation { class, reason }) => {
            // Eigener Code statt `plan_drift`: Nur so kann die Oberfläche den Weg
            // zur Wartestelle anbieten, ohne auf den deutschen Text zu matchen.
            // Der Satz sagt, was als Nächstes zu tun ist; das Warum steht davor.
            queue_execution_error(
                class,
                AppError::new(
                    "queue_confirmation_required",
                    format!("{reason}. Der Auftrag wartet in der Queue auf deine Bestätigung."),
                ),
            )
        }
        Err(error) if provider_delete || local_sync || content_sync => {
            persist_queue_execution_error(queue, &operation_id, class, &error)
        }
        Err(error) => {
            let code = doppeldecker_core::error_code(&error);
            let message = if resumable_copy && code != doppeldecker_core::ErrorCode::OutcomeUnclear
            {
                // Die bestätigte Queuebilanz enthält den handlungsbezogenen
                // Fehler. Der Wrapper enthält zusätzlich technische Diagnosedaten.
                queue
                    .snapshot()
                    .entries
                    .into_iter()
                    .find(|entry| {
                        entry.operation_id == operation_id
                            && entry.state == doppeldecker_core::QueueState::Failed
                            && entry.error_code == Some(code)
                    })
                    .and_then(|entry| entry.error)
            } else {
                None
            };
            let mut app_error =
                AppError::new(code.as_str(), message.unwrap_or_else(|| error.to_string()));
            if forward_bidirectional {
                app_error.recovery_paths = doppeldecker_core::error::recovery_paths(&error);
            }
            queue_execution_error(class, app_error)
        }
    };
    QueueTerminalEvent::with_progress(terminal_progress, outcome)
}

fn queue_execution_error(class: QueueOperationClass, error: AppError) -> QueueTerminalOutcome {
    let event = match class {
        QueueOperationClass::File => "fileop-error",
        QueueOperationClass::Sync => "sync-error",
    };
    QueueTerminalOutcome::Error { event, error }
}

fn persist_queue_execution_error(
    queue: &OperationQueue,
    operation_id: &str,
    class: QueueOperationClass,
    error: &std::io::Error,
) -> QueueTerminalOutcome {
    let progress = queue
        .snapshot()
        .entries
        .into_iter()
        .find(|candidate| candidate.operation_id == operation_id)
        .map(|candidate| candidate.progress)
        .unwrap_or_default();
    let _ = queue.fail_coded(operation_id, progress, error);
    queue_execution_error(
        class,
        AppError::new(
            doppeldecker_core::error_code(error).as_str(),
            error.to_string(),
        ),
    )
}

fn finalize_detailed_file_failure(
    queue: &OperationQueue,
    operation_id: &str,
    mut failure: doppeldecker_core::queue::ProviderFileExecutionFailure,
) -> std::io::Result<doppeldecker_core::QueueExecutionResult> {
    let still_open = queue.snapshot().entries.iter().any(|candidate| {
        candidate.operation_id == operation_id
            && matches!(
                candidate.state,
                doppeldecker_core::QueueState::Validating | doppeldecker_core::QueueState::Running
            )
    });
    if still_open {
        if let Some(completion) = failure.pending_completion.as_deref().cloned() {
            match queue.retry_file_completion(operation_id, &completion) {
                Ok(()) => {
                    failure.journal_errors.clear();
                    failure.pending_completion = None;
                    match queue.append_file_completion_audit(operation_id, &completion) {
                        Ok(()) => {
                            return Ok(doppeldecker_core::QueueExecutionResult::File(
                                completion.stats,
                            ));
                        }
                        Err(audit_error) => failure.error = audit_error,
                    }
                }
                Err(error) => failure.journal_errors.push(error),
            }
        } else {
            match queue.fail_file_actuals_coded(
                operation_id,
                (*failure.progress).clone(),
                &failure.error,
                &failure.actuals,
            ) {
                Ok(()) => {
                    // Die früheren Journalfehler sind damit nachweislich behoben.
                    // Für die Oberfläche bleibt der fachliche Fehler maßgeblich;
                    // ein alter Commitfehler darf ihn nicht länger überstimmen.
                    failure.journal_errors.clear();
                }
                Err(error) => failure.journal_errors.push(error),
            }
        }
    }
    Err(failure.into_io_error())
}

/// Laufbezogener Quarantänepfad. Er liegt als Geschwister des Zielordners in der
/// Sandbox, damit ein späterer Mirror-Lauf ihn nicht selbst als Zielinhalt erfasst.
///
/// Prozess-ID und Millisekunde allein genügen nicht: Alle Fenster teilen den
/// Prozess und denselben Quarantäne-Root und dürfen ausdrücklich parallel
/// arbeiten. Zwei in derselben Millisekunde gestartete Operationen kollidierten
/// sonst beim Anlegen des Manifests, obwohl gar kein Dateikonflikt vorliegt.
/// Fensterlabel und Operations-ID machen den Laufnamen deshalb eindeutig; die
/// zufällige Operations-ID trägt zusätzlich die Exklusivität.
fn quarantine_for(root: &Path, label: &str, operation_id: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    root.join(".doppeldecker-quarantine").join(format!(
        "{}-{millis}-{}-{}",
        std::process::id(),
        path_safe_token(label, 32),
        path_safe_token(operation_id, 16),
    ))
}

/// Kürzt einen Bezeichner auf einen unbedenklichen Namensbestandteil.
///
/// Nur ASCII-Buchstaben, Ziffern, `-` und `_` bleiben stehen. Damit kann weder ein
/// Fensterlabel noch eine Operations-ID einen Pfadwechsel erzwingen.
fn path_safe_token(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verworfene_remote_vorbereitung_kann_keinen_slot_veroeffentlichen() {
        let state = AppState::default();
        let slot = Mutex::new(None);
        let generation = AtomicU64::new(0);
        let started = generation.load(Ordering::Acquire);
        discard_sync_preparation(&slot, &generation);
        let error = store_profile_bound_pending_using(
            &state,
            &slot,
            "verspätete Vorschau",
            Some((&generation, started)),
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.code, "plan_drift");
        assert!(slot.lock_recover().is_none());
        store_profile_bound_pending_using(
            &state,
            &slot,
            "neue Vorschau",
            Some((&generation, generation.load(Ordering::Acquire))),
            || Ok(()),
        )
        .unwrap();
        assert_eq!(*slot.lock_recover(), Some("neue Vorschau"));
    }

    #[test]
    fn exit_waits_for_removed_queue_entry_cleanup_and_error_reporting() {
        let app = AppState::default();
        let window = app.window("main");
        let other = app.window("window-zwei");
        let inspect = || {
            crate::app_commands::assess_exit(&["main".into()], false, |_| {
                (window.has_active_operation(), Ok(Vec::new()))
            })
        };
        assert!(!inspect().must_prevent_exit());
        let guard = reserve_queue_cleanup(&app, &window).unwrap();
        let second = reserve_queue_cleanup(&app, &window).unwrap();
        drop(second);
        let (at_cleanup, reached_cleanup) = std::sync::mpsc::channel();
        let (finish, may_finish) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _guard = guard;
            at_cleanup.send(()).unwrap();
            may_finish.recv().unwrap();
            // Simulierter Fehler: Die Recoverymeldung liegt noch im Worker.
            Err::<(), _>(AppError::new("io", "Cleanup fehlgeschlagen"))
        });
        reached_cleanup.recv().unwrap();
        let during = inspect();
        assert!(during.must_prevent_exit());
        assert!(!during.can_confirm_exit());
        assert!(!other.has_active_operation());
        // Keine zusätzliche Single-Flight-Sperre für normale Arbeit.
        reserve_operation(&window).unwrap();
        window.active_cancel.lock_recover().take();
        finish.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err().code, "io");
        assert!(!inspect().must_prevent_exit());

        window.closing.store(true, Ordering::Release);
        assert_eq!(
            reserve_queue_cleanup(&app, &window).err().unwrap().code,
            "busy"
        );
        window.closing.store(false, Ordering::Release);
        app.exit_in_progress.store(true, Ordering::Release);
        assert_eq!(
            reserve_queue_cleanup(&app, &window).err().unwrap().code,
            "busy"
        );
        assert!(!window.has_active_operation());
    }

    #[test]
    fn exit_cleanup_guard_is_released_on_worker_panic() {
        let app = AppState::default();
        let window = app.window("main");
        let guard = reserve_queue_cleanup(&app, &window).unwrap();
        let result = std::thread::spawn(move || {
            let _guard = guard;
            panic!("künstliche Cleanupunterbrechung");
        })
        .join();
        assert!(result.is_err());
        assert!(!window.has_active_operation());
    }

    #[test]
    fn direkter_download_nutzt_fenstercancel_und_erhaelt_andere_fenster() {
        let app = AppState::default();
        let own = app.window("main");
        let other = app.window("window-zwei");
        let other_cancel = reserve_operation(&other).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("quelle.txt");
        let cache = root.join("cache");
        std::fs::write(&source, vec![7; 2 * 1024 * 1024]).unwrap();
        let outcome = run_reserved_cancellable(
            &own,
            |cancel| {
                assert_eq!(reserve_operation(&own).err().unwrap().code, "busy");
                own.active_cancel.lock_recover().as_ref().unwrap().cancel();
                doppeldecker_core::download_remote_snapshot(
                    &doppeldecker_core::LocalVfs::new(),
                    &source,
                    &cache,
                    cancel,
                )
                .map_err(AppError::from)
            },
            |result| {
                assert!(own.active_cancel.lock_recover().is_none());
                assert_eq!(
                    result.as_ref().unwrap_err().code,
                    "interrupted",
                    "{}",
                    result.as_ref().unwrap_err().message
                );
            },
        )
        .unwrap();
        assert_eq!(outcome.unwrap_err().code, "interrupted");
        assert!(!other_cancel.is_cancelled());
        assert!(other.active_cancel.lock_recover().is_some());
        assert_eq!(std::fs::read_dir(cache).unwrap().count(), 0);
        assert_eq!(std::fs::read(source).unwrap(), vec![7; 2 * 1024 * 1024]);
        assert!(reserve_operation(&own).is_ok());
    }

    #[test]
    fn panischer_direktworker_meldet_fehler_und_gibt_reservierung_frei() {
        let state = WindowState::default();
        let outcome = run_reserved_cancellable::<()>(
            &state,
            |_| panic!("synthetischer Workerfehler"),
            |result| {
                assert_eq!(result.as_ref().unwrap_err().code, "app");
                assert!(state.active_cancel.lock_recover().is_none());
            },
        )
        .unwrap();
        assert_eq!(outcome.unwrap_err().code, "app");
        assert!(reserve_operation(&state).is_ok());
    }

    #[test]
    fn direktworker_lehnt_zweiten_start_ab_und_gibt_bei_fehler_frei() {
        let state = WindowState::default();
        let result = run_reserved_cancellable::<()>(
            &state,
            |_| {
                let busy = run_reserved_cancellable::<()>(
                    &state,
                    |_| panic!("Zweiter Worker wurde gestartet"),
                    |_| panic!("Zweiter Worker hat ein Terminalereignis ausgelöst"),
                )
                .unwrap_err();
                assert_eq!(busy.code, "busy");
                Err(AppError::new("plan_drift", "Testfehler"))
            },
            |result| assert_eq!(result.as_ref().unwrap_err().code, "plan_drift"),
        )
        .unwrap();
        assert_eq!(result.unwrap_err().code, "plan_drift");
        assert!(run_reserved_cancellable(&state, |_| Ok(()), |_| {})
            .unwrap()
            .is_ok());
    }

    #[test]
    fn queue_erweiterung_haelt_barriere_bis_zum_fehler() {
        let state = AppState::default();
        let window = state.window("main");

        let error = with_queue_extension_access::<()>(&state, &window, |_| {
            assert!(matches!(
                state.exit_barrier.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            Err(AppError::new("plan_drift", "Testfehler"))
        })
        .unwrap_err();
        assert_eq!(error.code, "plan_drift");
        assert!(state.exit_barrier.try_lock().is_ok());

        window.closing.store(true, Ordering::Release);
        let error = with_queue_extension_access::<()>(&state, &window, |_| {
            panic!("Schließendes Fenster durfte die Queue erweitern")
        })
        .unwrap_err();
        assert_eq!(error.code, "busy");

        window.closing.store(false, Ordering::Release);
        state.exit_in_progress.store(true, Ordering::Release);
        let error = with_queue_extension_access::<()>(&state, &window, |_| {
            panic!("App durfte beim Beenden die Queue erweitern")
        })
        .unwrap_err();
        assert_eq!(error.code, "busy");
    }

    #[test]
    fn schliessbarriere_erzeugt_keine_erneute_worker_dispatchkette() {
        let temp = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: doppeldecker_core::FileQueueKind::Copy,
                items: vec!["/synthetisch/quelle".into()],
                destination: "/synthetisch/ziel".into(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let app = AppState::default();
        let window = app.window("main");
        let other = app.window("window-zwei");
        let mut dispatches = Vec::new();
        for global_exit in [false, true] {
            window.closing.store(!global_exit, Ordering::Release);
            app.exit_in_progress.store(global_exit, Ordering::Release);
            assert_eq!(reserve_operation(&window).err().unwrap().code, "busy");
            assert!(queue.has_runnable());
            if !global_exit {
                reserve_operation(&other).unwrap();
                other.active_cancel.lock_recover().take();
            }
            // Genau diese Produktionsentscheidung folgt im Worker auf den
            // abgewiesenen Start; dieselbe unveränderte Lage bliebe endlos retryfähig.
            if queue_worker_restart_allowed(
                &window,
                false,
                queue.has_runnable(),
                window.active_cancel.lock_recover().is_some(),
            ) {
                dispatches.push(if global_exit {
                    "app-exit"
                } else {
                    "window-close"
                });
            }
        }
        app.exit_in_progress.store(false, Ordering::Release);
        window.closing.store(false, Ordering::Release);
        assert!(queue_worker_restart_allowed(
            &window,
            false,
            queue.has_runnable(),
            false
        ));
        assert_eq!(
            queue.snapshot().entries[0].state,
            doppeldecker_core::QueueState::Pending
        );
        assert!(
            dispatches.is_empty(),
            "Schließbarriere löst immer neue Worker aus: {dispatches:?}"
        );
    }

    #[test]
    fn gatefreigabe_und_workerende_verlieren_keine_wiederaufnahme() {
        for reopen_before_stop in [false, true] {
            for global_exit in [false, true] {
                let app = AppState::default();
                let state = app.window("main");
                assert!(try_claim_queue_worker(&state));
                state.closing.store(!global_exit, Ordering::Release);
                app.exit_in_progress.store(global_exit, Ordering::Release);
                assert!(!try_claim_queue_worker(&state));
                let mut dispatches = 0;
                if reopen_before_stop {
                    state.closing.store(false, Ordering::Release);
                    app.exit_in_progress.store(false, Ordering::Release);
                    // Der alte Worker besitzt sein Flag noch. Der Wakeup kann
                    // ihn deshalb nicht doppelt starten; sein Ende übernimmt.
                    dispatches += usize::from(try_claim_queue_worker(&state));
                }
                state.queue_worker_active.store(false, Ordering::SeqCst);
                if queue_worker_restart_allowed(&state, false, true, false) {
                    dispatches += usize::from(try_claim_queue_worker(&state));
                }
                if !reopen_before_stop {
                    state.closing.store(false, Ordering::Release);
                    app.exit_in_progress.store(false, Ordering::Release);
                    dispatches += usize::from(try_claim_queue_worker(&state));
                }
                assert_eq!(dispatches, 1);
                assert!(state.queue_worker_active.load(Ordering::SeqCst));
            }
        }
    }

    #[test]
    fn claimfehler_startet_keinen_automatischen_worker_retry() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        std::fs::create_dir(&config).unwrap();
        let queue = OperationQueue::open(config.join("queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: doppeldecker_core::FileQueueKind::Copy,
                items: vec!["/quelle/datei".into()],
                destination: "/ziel".into(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        std::fs::rename(&config, temp.path().join("saved-config")).unwrap();
        std::fs::write(&config, b"kein Verzeichnis").unwrap();

        let error = queue.claim_next().unwrap_err();
        assert_eq!(
            doppeldecker_core::error_code(&error),
            doppeldecker_core::ErrorCode::OutcomeUnclear
        );
        assert!(queue.has_runnable());
        assert!(!queue_worker_restart_allowed(
            &WindowState::default(),
            true,
            queue.has_runnable(),
            false
        ));

        // Der Core bleibt auch nach Reparatur der Pfade fail-closed, bis ein
        // bewusster Neustart den gespeicherten Stand neu eingelesen hat.
        std::fs::remove_file(&config).unwrap();
        std::fs::rename(temp.path().join("saved-config"), &config).unwrap();
        assert!(queue.set_paused(false).is_err());
        drop(queue);
        let reopened = OperationQueue::open(config.join("queue.json")).unwrap();
        reopened.set_paused(true).unwrap();
        assert!(!queue_worker_restart_allowed(
            &WindowState::default(),
            false,
            reopened.has_runnable(),
            false
        ));
        reopened.set_paused(false).unwrap();
        assert!(queue_worker_restart_allowed(
            &WindowState::default(),
            false,
            reopened.has_runnable(),
            false
        ));
        assert!(!queue_worker_restart_allowed(
            &WindowState::default(),
            false,
            reopened.has_runnable(),
            true
        ));
        assert!(reopened.claim_next().unwrap().is_some());
    }

    fn remote_confirmation_queue(
        path: &Path,
    ) -> (
        OperationQueue,
        String,
        Arc<doppeldecker_core::OperationQueueProcessLock>,
    ) {
        let process_lock =
            Arc::new(doppeldecker_core::OperationQueueProcessLock::acquire(path).unwrap());
        let queue = OperationQueue::open_with_process_lock(path, process_lock.clone()).unwrap();
        let entry = queue
            .enqueue_claimed(QueueOperation::File {
                kind: doppeldecker_core::FileQueueKind::Copy,
                items: vec!["/quelle/datei".into()],
                destination: "/ziel".into(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        queue
            .require_confirmation(
                &entry.operation_id,
                doppeldecker_core::queue::QueueAttention::RemoteRun {
                    reason: "Vorbereitung wartet".into(),
                    confirmation: "WEITER".into(),
                },
            )
            .unwrap();
        (queue, entry.operation_id, process_lock)
    }

    #[test]
    fn verbrauchtes_remote_permit_meldet_journalfehler_und_neustartweg() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let (queue, operation_id, process_lock) = remote_confirmation_queue(&path);
        // Sicherer Nichtcommit: Der persistierte Wartestand bleibt vollständig
        // erhalten, wie beim echten PermissionDenied vor dem atomaren Rename.
        let error = reset_consumed_remote_preparation_using(
            AppError::new("plan_drift", "Profil wurde geändert"),
            || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Journal schreibgeschützt",
                ))
            },
        );
        assert_eq!(error.code, "permission_denied");
        for expected in [
            "Profil wurde geändert",
            "Journal schreibgeschützt",
            "Schreibfehler beheben",
            "App neu starten",
            "Neu prüfen",
        ] {
            assert!(error.message.contains(expected), "{}", error.message);
        }
        assert_eq!(
            queue.snapshot().entries[0].state,
            doppeldecker_core::QueueState::AwaitingConfirmation
        );
        assert!(!queue.has_runnable());
        assert!(queue.retry(&operation_id).is_err());
        let unclear = reset_consumed_remote_preparation_using(
            AppError::new("plan_drift", "Profil wurde geändert"),
            || {
                Err(doppeldecker_core::coded_error(
                    std::io::ErrorKind::Other,
                    doppeldecker_core::ErrorCode::OutcomeUnclear,
                    "Journal-Readback unklar",
                ))
            },
        );
        assert_eq!(unclear.code, "outcome_unclear");
        assert!(unclear.message.contains("Journal-Readback unklar"));
        assert!(unclear.message.contains("App neu starten"));
        let original_path = temp.path().join("ursprung.txt");
        let reset_path = temp.path().join("journalrest.txt");
        std::fs::write(&original_path, b"ursprung").unwrap();
        std::fs::write(&reset_path, b"journalrest").unwrap();
        let error = reset_consumed_remote_preparation_using(
            AppError::from(doppeldecker_core::error::with_recovery_paths(
                doppeldecker_core::coded_error(
                    std::io::ErrorKind::Other,
                    doppeldecker_core::ErrorCode::OutcomeUnclear,
                    "Probeausgang unklar",
                ),
                &[original_path.to_string_lossy().into_owned()],
            )),
            || {
                Err(doppeldecker_core::error::with_recovery_paths(
                    std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Journal gesperrt"),
                    &[reset_path.to_string_lossy().into_owned()],
                ))
            },
        );
        assert_eq!(error.code, "outcome_unclear");
        for path in [&original_path, &reset_path] {
            assert!(error
                .recovery_paths
                .contains(&path.to_string_lossy().into_owned()));
        }
        assert_eq!(std::fs::read(&original_path).unwrap(), b"ursprung");
        assert_eq!(std::fs::read(&reset_path).unwrap(), b"journalrest");
        drop(queue);
        // Hier wird die Journal-Recovery geprüft. Die Prozesssperre bleibt gehalten,
        // damit parallele Testhelfer zwischen fork und exec keinen künstlichen
        // Lockwechsel auslösen. Ihre prozessübergreifende Exklusivität prüfen Core-Tests.
        let reopened = OperationQueue::open_with_process_lock(&path, process_lock).unwrap();
        assert_eq!(
            reopened.snapshot().entries[0].state,
            doppeldecker_core::QueueState::Interrupted
        );
        assert!(reopened.snapshot().entries[0].attention.is_none());
        reopened.retry(&operation_id).unwrap();
        assert!(reopened.has_runnable());
    }

    #[test]
    fn erfolgreicher_remote_reset_behaelt_den_fachfehler() {
        let temp = tempfile::tempdir().unwrap();
        let (queue, operation_id, _process_lock) =
            remote_confirmation_queue(&temp.path().join("queue.json"));
        let original = AppError::new("plan_drift", "Profil wurde geändert");
        let result = reset_consumed_remote_preparation(&queue, &operation_id, original.clone());
        assert_eq!(result.code, original.code);
        assert_eq!(result.message, original.message);
        assert_eq!(
            queue.snapshot().entries[0].state,
            doppeldecker_core::QueueState::Pending
        );
        assert!(queue.has_runnable());
    }

    fn queue_inventory_helper(path: &syn::ExprPath, helper: &str) -> bool {
        let parts = &path.path.segments;
        path.qself.is_none()
            && path.path.leading_colon.is_none()
            && parts.last().is_some_and(|part| part.ident == helper)
            && (parts.len() == 1
                || (parts.len() == 3
                    && parts[0].ident == "crate"
                    && parts[1].ident == "queue_runtime"))
    }

    #[derive(Default)]
    struct RemoteCleanupCalls {
        resets: usize,
        ignored_discards: usize,
    }

    impl<'ast> syn::visit::Visit<'ast> for RemoteCleanupCalls {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if matches!(call.func.as_ref(), syn::Expr::Path(path)
                if queue_inventory_helper(path, "reset_consumed_remote_preparation"))
            {
                self.resets += 1;
            }
            syn::visit::visit_expr_call(self, call);
        }

        fn visit_local(&mut self, local: &'ast syn::Local) {
            if matches!(local.pat, syn::Pat::Wild(_)) {
                if let Some(init) = &local.init {
                    let mut expr = init.expr.as_ref();
                    while let syn::Expr::Paren(parenthesized) = expr {
                        expr = &parenthesized.expr;
                    }
                    if matches!(expr, syn::Expr::MethodCall(call)
                        if call.method == "discard_remote_preparation")
                    {
                        self.ignored_discards += 1;
                    }
                }
            }
            syn::visit::visit_local(self, local);
        }
    }

    #[test]
    fn alle_remote_cleanup_fehlerzweige_behalten_journalfehler() {
        use syn::visit::Visit;
        for (name, source, expected) in [
            ("SFTP", include_str!("sftp_commands.rs"), 3),
            ("FTPS", include_str!("ftps_commands.rs"), 3),
            ("WebDAV", include_str!("webdav_commands.rs"), 3),
            ("SMB", include_str!("smb_commands.rs"), 3),
            ("Provider", include_str!("provider_sync_commands.rs"), 3),
            (
                "bidirektional",
                include_str!("bidirectional_commands.rs"),
                2,
            ),
        ] {
            let file = syn::parse_file(source).unwrap_or_else(|error| panic!("{name}: {error}"));
            let mut calls = RemoteCleanupCalls::default();
            calls.visit_file(&file);
            assert_eq!(calls.resets, expected, "{name}: Inventur der Cleanup-Pfade");
            assert_eq!(
                calls.ignored_discards, 0,
                "{name}: Fehler beim Verwerfen der Vorbereitung ignoriert"
            );
        }
    }

    #[test]
    fn erfolgreicher_dateiabschlussretry_priorisiert_wieder_den_fachfehler() {
        let temp = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let entry = queue
            .enqueue(QueueOperation::File {
                kind: doppeldecker_core::FileQueueKind::Copy,
                items: vec!["/quelle/datei".to_owned()],
                destination: "/ziel".to_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        queue.mark_running(&claimed.operation_id).unwrap();
        let failure = doppeldecker_core::queue::ProviderFileExecutionFailure {
            error: doppeldecker_core::coded_error(
                std::io::ErrorKind::Other,
                doppeldecker_core::ErrorCode::OutcomeUnclear,
                "fachlicher Ausgang unklar",
            ),
            journal_errors: vec![std::io::Error::other("erster Fehlercommit scheiterte")],
            actuals: Box::default(),
            progress: Box::new(Progress::default()),
            pending_completion: None,
            recovery: None,
        };

        let error = finalize_detailed_file_failure(&queue, &entry.operation_id, failure)
            .expect_err("fachlicher Fehler bleibt trotz erfolgreichem Journalretry sichtbar");

        assert_eq!(
            doppeldecker_core::error_code(&error),
            doppeldecker_core::ErrorCode::OutcomeUnclear
        );
        assert_eq!(
            queue.snapshot().entries[0].state,
            doppeldecker_core::QueueState::Failed
        );
    }

    fn test_profile(revision: u64) -> VersionedProfile<String> {
        VersionedProfile {
            profile: format!("profil-{revision}"),
            revision,
        }
    }

    #[test]
    fn langsamer_handshake_darf_geaendertes_profil_nicht_cachen() {
        let state = AppState::default();
        let pool = Mutex::new(HashMap::<String, CachedRemoteConnection<String>>::new());
        let revision = AtomicU64::new(1);
        let handshake = Arc::new(std::sync::Barrier::new(2));

        std::thread::scope(|scope| {
            let handshake_worker = handshake.clone();
            let result = scope.spawn(|| {
                cached_remote_connection_using(
                    &state,
                    &pool,
                    RemoteConnectionRequest {
                        protocol: RemoteProfileProtocol::Sftp,
                        profile_id: "profil",
                        requested_revision: Some(1),
                    },
                    || Ok(test_profile(revision.load(Ordering::Acquire))),
                    move |_| {
                        handshake_worker.wait();
                        handshake_worker.wait();
                        Ok(Arc::new("alte-verbindung".to_owned()))
                    },
                )
            });
            handshake.wait();
            revision.store(2, Ordering::Release);
            handshake.wait();
            let error = match result.join().unwrap() {
                Err(error) => error,
                Ok(_) => panic!("alter Handshake wurde trotz Profiländerung gecacht"),
            };
            assert_eq!(error.code, "plan_drift");
        });
        assert!(pool.lock_recover().is_empty());
    }

    #[test]
    fn langsamer_handshake_darf_entferntes_profil_nicht_cachen() {
        let state = AppState::default();
        let pool = Mutex::new(HashMap::<String, CachedRemoteConnection<String>>::new());
        let removed = AtomicBool::new(false);
        let handshake = Arc::new(std::sync::Barrier::new(2));

        std::thread::scope(|scope| {
            let handshake_worker = handshake.clone();
            let result = scope.spawn(|| {
                cached_remote_connection_using(
                    &state,
                    &pool,
                    RemoteConnectionRequest {
                        protocol: RemoteProfileProtocol::Sftp,
                        profile_id: "profil",
                        requested_revision: Some(1),
                    },
                    || {
                        if removed.load(Ordering::Acquire) {
                            Err(AppError::new("not_found", "Profil wurde entfernt"))
                        } else {
                            Ok(test_profile(1))
                        }
                    },
                    move |_| {
                        handshake_worker.wait();
                        handshake_worker.wait();
                        Ok(Arc::new("alte-verbindung".to_owned()))
                    },
                )
            });
            handshake.wait();
            removed.store(true, Ordering::Release);
            handshake.wait();
            let error = match result.join().unwrap() {
                Err(error) => error,
                Ok(_) => panic!("alter Handshake wurde trotz Profilentfernung gecacht"),
            };
            assert_eq!(error.code, "not_found");
        });
        assert!(pool.lock_recover().is_empty());
    }

    #[test]
    fn fehlendes_profil_verwirft_auch_einen_bestehenden_cacheeintrag() {
        let state = AppState::default();
        let binding = RemoteProfileBinding::new(RemoteProfileProtocol::Sftp, "profil", 4).unwrap();
        let pool = Mutex::new(HashMap::from([(
            "profil".to_owned(),
            CachedRemoteConnection {
                binding,
                connection: Arc::new("alte-verbindung".to_owned()),
            },
        )]));

        let error = match cached_remote_connection_using(
            &state,
            &pool,
            RemoteConnectionRequest {
                protocol: RemoteProfileProtocol::Sftp,
                profile_id: "profil",
                requested_revision: None,
            },
            || Err::<VersionedProfile<String>, _>(AppError::new("not_found", "entfernt")),
            |_| panic!("für ein fehlendes Profil darf kein Handshake starten"),
        ) {
            Err(error) => error,
            Ok(_) => panic!("fehlendes Profil verwendete den alten Cache"),
        };

        assert_eq!(error.code, "not_found");
        assert!(pool.lock_recover().is_empty());
    }

    #[test]
    fn veraltete_profilrevision_stoppt_vor_cache_und_handshake() {
        let state = AppState::default();
        let current = RemoteProfileBinding::new(RemoteProfileProtocol::Sftp, "profil", 8).unwrap();
        let pool = Mutex::new(HashMap::from([(
            "profil".to_owned(),
            CachedRemoteConnection {
                binding: current,
                connection: Arc::new("aktuelle-verbindung".to_owned()),
            },
        )]));
        let connects = std::sync::atomic::AtomicUsize::new(0);

        let error = match cached_remote_connection_using(
            &state,
            &pool,
            RemoteConnectionRequest {
                protocol: RemoteProfileProtocol::Sftp,
                profile_id: "profil",
                requested_revision: Some(7),
            },
            || Ok(test_profile(8)),
            |_| {
                connects.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new("unerlaubter-handshake".to_owned()))
            },
        ) {
            Err(error) => error,
            Ok(_) => panic!("eine alte Pane verwendete Cache oder Handshake"),
        };

        assert_eq!(error.code, "plan_drift");
        assert_eq!(connects.load(Ordering::SeqCst), 0);
        assert_eq!(pool.lock_recover().len(), 1);
    }

    #[test]
    fn profilinvalidierung_laesst_andere_fenster_und_profile_unberuehrt() {
        let target = RemoteProfileBinding::new(RemoteProfileProtocol::Sftp, "ziel", 3).unwrap();
        let other_profile =
            RemoteProfileBinding::new(RemoteProfileProtocol::Sftp, "anderes", 9).unwrap();
        let same_id_other_protocol =
            RemoteProfileBinding::new(RemoteProfileProtocol::Ftps, "ziel", 4).unwrap();
        let first_window = Mutex::new(Some(vec![target]));
        let second_window = Mutex::new(Some(vec![other_profile]));
        let third_window = Mutex::new(Some(vec![same_id_other_protocol]));

        assert!(take_profile_dependent(
            &first_window,
            RemoteProfileProtocol::Sftp,
            "ziel",
            Vec::as_slice,
        )
        .is_some());
        assert!(take_profile_dependent(
            &second_window,
            RemoteProfileProtocol::Sftp,
            "ziel",
            Vec::as_slice,
        )
        .is_none());
        assert!(take_profile_dependent(
            &third_window,
            RemoteProfileProtocol::Sftp,
            "ziel",
            Vec::as_slice,
        )
        .is_none());
        assert!(first_window.lock_recover().is_none());
        assert!(second_window.lock_recover().is_some());
        assert!(third_window.lock_recover().is_some());
    }

    #[test]
    fn queueprofilinvalidierung_entfernt_ram_permit_erst_nach_journalcommit() {
        #[derive(Debug)]
        struct Pending {
            bindings: Vec<RemoteProfileBinding>,
            operation_id: String,
        }
        let pending = || Pending {
            bindings: vec![
                RemoteProfileBinding::new(RemoteProfileProtocol::Sftp, "ziel", 3).unwrap(),
            ],
            operation_id: "operation-1".into(),
        };
        let slot = Mutex::new(Some(pending()));

        let error = discard_queue_profile_dependent(
            &slot,
            RemoteProfileProtocol::Sftp,
            "ziel",
            |pending| &pending.bindings,
            |pending| pending.operation_id.as_str(),
            |_| Err(AppError::new("io", "Journalcommit fehlgeschlagen")),
        )
        .unwrap_err();
        assert_eq!(error.code, "io");
        assert!(slot.lock_recover().is_some());

        let changed = discard_queue_profile_dependent(
            &slot,
            RemoteProfileProtocol::Sftp,
            "ziel",
            |pending| &pending.bindings,
            |pending| pending.operation_id.as_str(),
            |_| Ok(()),
        )
        .unwrap();
        assert!(changed);
        assert!(slot.lock_recover().is_none());
    }

    #[test]
    fn bestaetigung_retryt_fehlgeschlagene_queueprofilinvalidierung() {
        #[derive(Debug)]
        struct Pending {
            bindings: Vec<RemoteProfileBinding>,
            operation_id: String,
        }
        let slot = Mutex::new(Some(Pending {
            bindings: vec![
                RemoteProfileBinding::new(RemoteProfileProtocol::Sftp, "ziel", 3).unwrap(),
            ],
            operation_id: "operation-1".into(),
        }));
        let discard_attempts = std::cell::Cell::new(0);
        let snapshots = std::cell::Cell::new(0);

        let first = reset_stale_queue_preparation(
            &slot,
            "operation-1",
            |pending| &pending.bindings,
            |pending| pending.operation_id.as_str(),
            |_| Err(AppError::new("plan_drift", "Profil wurde geändert")),
            |_| {
                discard_attempts.set(discard_attempts.get() + 1);
                Err(AppError::new("io", "erster Journalcommit fehlgeschlagen"))
            },
            || snapshots.set(snapshots.get() + 1),
        )
        .unwrap_err();
        assert_eq!(first.code, "io");
        assert!(slot.lock_recover().is_some());

        let second = reset_stale_queue_preparation(
            &slot,
            "operation-1",
            |pending| &pending.bindings,
            |pending| pending.operation_id.as_str(),
            |_| Err(AppError::new("plan_drift", "Profil wurde geändert")),
            |_| {
                discard_attempts.set(discard_attempts.get() + 1);
                Ok(())
            },
            || snapshots.set(snapshots.get() + 1),
        )
        .unwrap_err();
        assert_eq!(second.code, "plan_drift");
        assert!(slot.lock_recover().is_none());
        assert_eq!(discard_attempts.get(), 2);
        assert_eq!(snapshots.get(), 1);
    }

    #[test]
    fn fehlende_angeforderte_profilrevision_ist_plan_drift() {
        let binding =
            RemoteProfileBinding::new(RemoteProfileProtocol::Webdav, "profil", 7).unwrap();
        assert_eq!(
            ensure_requested_profile_revision(None, &binding)
                .unwrap_err()
                .code,
            "plan_drift"
        );
        assert!(ensure_requested_profile_revision(Some(7), &binding).is_ok());
    }

    #[test]
    fn profilwechsel_nach_plan_und_konfliktliste_ist_plan_drift() {
        let binding =
            RemoteProfileBinding::new(RemoteProfileProtocol::Webdav, "profil", 7).unwrap();
        let current_revision = AtomicU64::new(7);

        // Revision 7 steht für den bereits erzeugten Plan samt Konfliktliste.
        assert!(
            ensure_profile_bindings_current_using(std::slice::from_ref(&binding), |_| Ok(Some(
                current_revision.load(Ordering::Acquire)
            )),)
            .is_ok()
        );
        // Ein Save zwischen Vorschau und Permit erhöht die persistente Revision.
        current_revision.store(8, Ordering::Release);
        let error = ensure_profile_bindings_current_using(std::slice::from_ref(&binding), |_| {
            Ok(Some(current_revision.load(Ordering::Acquire)))
        })
        .unwrap_err();
        assert_eq!(error.code, "plan_drift");
    }

    #[test]
    fn profilentfernung_nach_plan_und_konfliktliste_ist_plan_drift() {
        let binding = RemoteProfileBinding::new(RemoteProfileProtocol::Smb, "profil", 4).unwrap();
        let error =
            ensure_profile_bindings_current_using(std::slice::from_ref(&binding), |_| Ok(None))
                .unwrap_err();
        assert_eq!(error.code, "plan_drift");
    }

    #[test]
    fn save_zwischen_plan_und_pending_store_gewinnt_atomisch() {
        let state = AppState::default();
        let slot = Mutex::new(None::<String>);
        let revision = AtomicU64::new(7);
        let started = std::sync::Barrier::new(2);
        let save_transaction = state.profile_secret_guard.lock_recover();

        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                started.wait();
                store_profile_bound_pending_using(&state, &slot, "stale".into(), None, || {
                    if revision.load(Ordering::Acquire) == 7 {
                        Ok(())
                    } else {
                        Err(AppError::new("plan_drift", "Profil wurde gespeichert"))
                    }
                })
            });
            started.wait();
            // Der Save ändert die persistente Revision, während sein gemeinsamer
            // Transaktionsmutex einen späten Pending-Store noch ausschließt.
            revision.store(8, Ordering::Release);
            drop(save_transaction);
            let error = worker.join().unwrap().unwrap_err();
            assert_eq!(error.code, "plan_drift");
        });
        assert!(slot.lock_recover().is_none());
    }

    /// Zwei erlaubterweise parallele Fensteroperationen dürfen sich nicht am
    /// Quarantänelauf blockieren — auch nicht in derselben Millisekunde.
    #[test]
    fn quarantinelauf_bleibt_zwischen_fenstern_eindeutig() {
        let root = Path::new("/wegwerf/sandbox");
        let first = quarantine_for(root, "main", "0123456789abcdef");
        let second = quarantine_for(root, "window-2", "fedcba9876543210");
        assert_ne!(first, second);
        assert!(first.starts_with(root.join(".doppeldecker-quarantine")));
        assert!(second.starts_with(root.join(".doppeldecker-quarantine")));
        // Ein bösartiges Label kann den Laufpfad nicht verlassen.
        let hostile = quarantine_for(root, "../../etc", "0123456789abcdef");
        assert!(hostile.starts_with(root.join(".doppeldecker-quarantine")));
        assert_eq!(hostile.components().count(), first.components().count());
    }

    #[test]
    fn fensterzustand_ist_getrennt_verbindungspool_bleibt_geteilt() {
        let app = AppState::default();
        let first = app.window("main");
        let same = app.window("main");
        let second = app.window("window-2");

        assert!(Arc::ptr_eq(&first, &same));
        assert!(!Arc::ptr_eq(&first, &second));
        *first.active_cancel.lock_recover() = Some(Cancel::new());
        assert!(same.active_cancel.lock_recover().is_some());
        assert!(second.active_cancel.lock_recover().is_none());
        // Der Pool liegt absichtlich nicht im Fensterzustand. Sein einziger Speicher
        // ist `AppState`, sodass eine Anmeldung in allen Fenstern sichtbar wird.
        assert!(app.sftp_connections.lock_recover().is_empty());
    }

    #[test]
    fn voruebergehender_prozesslock_fehler_bleibt_retryfaehig() {
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path().join("operations-queue.json");
        let foreign = OperationQueueProcessLock::acquire(&journal).unwrap();
        let state = AppState::default();

        let first = match acquire_shared_process_lock(&state, &journal) {
            Ok(_) => panic!("gehaltener Prozesslock muss den ersten Versuch ablehnen"),
            Err(error) => error,
        };
        assert_eq!(first.code, "queue");
        assert!(state.queue_process_lock.get().is_none());

        drop(foreign);
        let acquired = acquire_shared_process_lock(&state, &journal)
            .expect("nach Lockfreigabe muss dieselbe AppState-Instanz retryen können");
        let cached = acquire_shared_process_lock(&state, &journal).unwrap();
        assert!(Arc::ptr_eq(&acquired, &cached));
    }

    #[test]
    fn setup_prozesslock_stoppt_fremdbesitz_ohne_hauptjournal_zu_oeffnen() {
        let temp = tempfile::tempdir().unwrap();
        let lock_anchor = temp.path().join(".operations-queue-process-lock-anchor");
        let foreign = OperationQueueProcessLock::acquire(&lock_anchor).unwrap();
        let state = AppState::default();

        let error = initialize_queue_process_lock_at(&state, temp.path()).unwrap_err();
        assert_eq!(error.code, "queue");
        assert!(state.queue_process_lock.get().is_none());
        assert!(!temp.path().join("operations-queue.json").exists());

        drop(foreign);
        initialize_queue_process_lock_at(&state, temp.path()).unwrap();
        assert!(state.queue_process_lock.get().is_some());
        assert!(!lock_anchor.exists());
        assert!(!temp.path().join("operations-queue.json").exists());
    }

    #[test]
    fn exit_inventar_meldet_nur_belegte_vorbereitungsslots_sortiert() {
        window_state! {
            #[derive(Default)]
            struct TestWindow {
                fields {}
                preparations {
                    pending_later: Mutex<Option<u8>> => "zweite",
                    pending_earlier: Mutex<Option<u16>> => "erste",
                }
            }
        }

        assert!(WindowState::default()
            .pending_preparation_kinds()
            .is_empty());
        let window = TestWindow::default();
        assert!(window.pending_preparation_kinds().is_empty());
        *window.pending_later.lock_recover() = Some(1);
        assert_eq!(window.pending_preparation_kinds(), ["zweite"]);
        *window.pending_earlier.lock_recover() = Some(2);
        assert_eq!(window.pending_preparation_kinds(), ["erste", "zweite"]);
        window.pending_later.lock_recover().take();
        assert_eq!(window.pending_preparation_kinds(), ["erste"]);
    }

    #[test]
    fn pending_start_nimmt_exit_barriere_vor_pending_mutex() {
        let app = Arc::new(AppState::default());
        let window = app.window("main");
        let pending = Arc::new(Mutex::new(Some(7_u8)));
        let pending_guard = pending.lock_recover();
        let start_app = app.clone();
        let start_window = window.clone();
        let start_pending = pending.clone();
        let start = std::thread::spawn(move || {
            take_pending_for_operation(
                &start_app,
                &start_window,
                &start_pending,
                "Vorbereitung fehlt",
                |_| Ok(()),
            )
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match app.exit_barrier.try_lock() {
                Err(std::sync::TryLockError::WouldBlock) => break,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    panic!("Exit-Barriere wurde vergiftet")
                }
                Ok(guard) => {
                    drop(guard);
                    assert!(
                        Instant::now() < deadline,
                        "Start wartete am Pending-Mutex, ohne zuerst die Exit-Barriere zu halten"
                    );
                    std::thread::yield_now();
                }
            }
        }

        drop(pending_guard);
        let (_, value, ()) = start.join().unwrap().unwrap();
        assert_eq!(value, 7);
    }

    #[test]
    fn pending_start_entnimmt_bei_pruef_oder_gatefehler_nichts() {
        let app = AppState::default();
        let window = app.window("main");
        let pending = Mutex::new(Some(7_u8));

        let error =
            match take_pending_for_operation(&app, &window, &pending, "Vorbereitung fehlt", |_| {
                Err::<(), _>(AppError::new("confirmation_mismatch", "Absichtlich falsch"))
            }) {
                Ok(_) => panic!("Fehlerhafte Bestätigung entnahm die Vorbereitung"),
                Err(error) => error,
            };
        assert_eq!(error.code, "confirmation_mismatch");
        assert_eq!(*pending.lock_recover(), Some(7));

        window.closing.store(true, Ordering::Release);
        let error =
            match take_pending_for_operation(&app, &window, &pending, "Vorbereitung fehlt", |_| {
                Ok(())
            }) {
                Ok(_) => panic!("Closing-Gate ließ die vorbereitete Operation starten"),
                Err(error) => error,
            };
        assert_eq!(error.code, "busy");
        assert_eq!(*pending.lock_recover(), Some(7));
    }

    #[test]
    fn jedes_fenster_bekommt_ein_eigenes_sicheres_queue_journal() {
        assert_eq!(queue_journal_name("main").unwrap(), "operations-queue.json");
        assert_eq!(
            queue_journal_name("window-17").unwrap(),
            "operations-queue-window-17.json"
        );
        assert_eq!(
            window_data_file_name("real-runs.jsonl", "window-17").unwrap(),
            "real-runs-window-17.jsonl"
        );
        for invalid in ["", "../fremd", "fenster/eins", "ä"] {
            assert!(queue_journal_name(invalid).is_err());
        }
    }

    fn reserves_current_preparation_window(call: &syn::ExprCall) -> bool {
        let mut args = call.args.iter();
        let name_is = |value: &syn::Expr, name| matches!(value, syn::Expr::Path(path) if path.path.is_ident(name));
        matches!(args.next(), Some(syn::Expr::Reference(value)) if name_is(&value.expr, "app"))
            && matches!(args.next(), Some(value) if name_is(value, "label"))
            && matches!(args.next(), Some(syn::Expr::Reference(value))
                if name_is(&value.expr, "window_state"))
    }

    struct ReservedPreparationCalls<'a> {
        effect: &'a str,
        in_reservation: bool,
        reserved: usize,
        unreserved: usize,
    }

    impl<'ast> syn::visit::Visit<'ast> for ReservedPreparationCalls<'_> {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if let syn::Expr::Path(path) = call.func.as_ref() {
                if let Some(part) = path.path.segments.last() {
                    if queue_inventory_helper(path, "with_reserved_operation")
                        || queue_inventory_helper(path, "with_reserved_cancellable_operation")
                    {
                        let enclosing_reservation = self.in_reservation;
                        for (index, argument) in call.args.iter().enumerate() {
                            self.in_reservation = enclosing_reservation
                                || index == 3
                                    && matches!(argument, syn::Expr::Closure(_))
                                    && reserves_current_preparation_window(call);
                            self.visit_expr(argument);
                        }
                        self.in_reservation = enclosing_reservation;
                        return;
                    }
                    if part.ident == self.effect {
                        if self.in_reservation {
                            self.reserved += 1;
                        } else {
                            self.unreserved += 1;
                        }
                    }
                }
            }
            syn::visit::visit_expr_call(self, call);
        }

        fn visit_item(&mut self, _: &'ast syn::Item) {}
    }

    #[test]
    fn vorbereitungsinventur_prueft_callback_und_ignoriert_kommentare() {
        use syn::visit::Visit;
        let file = syn::parse_file(
            r#"fn prepare() {
                // connect_fresh_bound();
                let _ = "connect_fresh_bound()";
                with_reserved_operation(&app, label, &window_state, || {
                    crate::connect_fresh_bound /* im Worker */ ();
                });
                with_reserved_cancellable_operation(&app, label, &window_state,
                   |_| {}, |_| { connect_fresh_bound(); });
                with_reserved_operation(&app, label, &other_window, || {
                    connect_fresh_bound();
                });
                connect_fresh_bound();
            }
            fn other() { connect_fresh_bound(); }
            "#,
        )
        .unwrap();
        let mut calls = ReservedPreparationCalls {
            effect: "connect_fresh_bound",
            in_reservation: false,
            reserved: 0,
            unreserved: 0,
        };
        calls.visit_block(&named_function(&file, "Test", "prepare").block);
        assert_eq!((calls.reserved, calls.unreserved), (1, 3));

        let cleanup = syn::parse_file(
            r#"fn cleanup() {
                // reset_consumed_remote_preparation();
                let _ = "let _ = queue.discard_remote_preparation()";
                let _ = (queue .discard_remote_preparation ());
                queue.discard_remote_preparation()?;
                crate :: queue_runtime :: reset_consumed_remote_preparation ();
            }"#,
        )
        .unwrap();
        let mut calls = RemoteCleanupCalls::default();
        calls.visit_file(&cleanup);
        assert_eq!((calls.resets, calls.ignored_discards), (1, 1));
    }

    /// Die mutierende Arbeit gehört in den reservierten Callback. Ein
    /// Reservierungsaufruf davor oder im Terminalcallback genügt nicht.
    #[test]
    fn mutierende_vorbereitungen_nutzen_die_backend_reservierung() {
        use syn::visit::Visit;
        let cases: [(&str, &str, &[&str]); 9] = [
            (
                "prepare_real_sync",
                include_str!("local_sync_commands.rs"),
                &["prepare_local_real_run_for_queue"],
            ),
            (
                "prepare_sftp_sync",
                include_str!("sftp_commands.rs"),
                &["connect_fresh_bound"],
            ),
            (
                "prepare_ftps_sync",
                include_str!("ftps_commands.rs"),
                &["connect_fresh_bound"],
            ),
            (
                "prepare_webdav_sync",
                include_str!("webdav_commands.rs"),
                &["connect_fresh_bound"],
            ),
            (
                "prepare_smb_sync",
                include_str!("smb_commands.rs"),
                &["connect_fresh_bound"],
            ),
            (
                "prepare_provider_sync",
                include_str!("provider_sync_commands.rs"),
                &["resolve_validated_registry_pair"],
            ),
            (
                "prepare_bidirectional_sync",
                include_str!("bidirectional_commands.rs"),
                &[
                    "prepare_local_bidirectional_with_decisions_bound_for_queue",
                    "prepare_provider_bidirectional_with_decisions_bound_for_queue",
                ],
            ),
            (
                "prepare_bidirectional_resume",
                include_str!("bidirectional_commands.rs"),
                &[
                    "prepare_resume_local_bidirectional_for_queue_operation",
                    "prepare_resume_provider_bidirectional_for_queue_operation",
                ],
            ),
            (
                "prepare_bidirectional_rollback",
                include_str!("bidirectional_commands.rs"),
                &[
                    "prepare_local_bidirectional_rollback_for_queue_operation",
                    "prepare_provider_bidirectional_rollback_for_queue_operation",
                ],
            ),
        ];

        for (name, source, effects) in cases {
            let file = syn::parse_file(source).unwrap_or_else(|error| panic!("{name}: {error}"));
            let command = named_function(&file, name, name);
            for effect in effects {
                let mut calls = ReservedPreparationCalls {
                    effect,
                    in_reservation: false,
                    reserved: 0,
                    unreserved: 0,
                };
                calls.visit_block(&command.block);
                assert_eq!(
                    calls.reserved, 1,
                    "{name}: reservierter Aufruf {effect} fehlt"
                );
                assert_eq!(
                    calls.unreserved, 0,
                    "{name}: {effect} läuft ohne Reservierung"
                );
            }
        }
    }

    #[derive(Clone, Copy)]
    enum QueueCall<'a> {
        Function(&'a str),
        Method(&'a str),
    }

    struct QueueCalls<'a> {
        expected: QueueCall<'a>,
        guarded: usize,
        unguarded: usize,
        in_extension: bool,
        include_items: bool,
    }

    impl<'a> QueueCalls<'a> {
        fn new(expected: QueueCall<'a>) -> Self {
            Self {
                expected,
                guarded: 0,
                unguarded: 0,
                in_extension: false,
                include_items: false,
            }
        }

        fn in_function(function: &syn::ItemFn, expected: QueueCall<'a>) -> Self {
            use syn::visit::Visit;
            let mut calls = Self::new(expected);
            calls.visit_block(&function.block);
            calls
        }

        fn count(&mut self) {
            if self.in_extension {
                self.guarded += 1;
            } else {
                self.unguarded += 1;
            }
        }

        fn total(&self) -> usize {
            self.guarded + self.unguarded
        }
    }

    impl<'ast> syn::visit::Visit<'ast> for QueueCalls<'_> {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if let syn::Expr::Path(path) = call.func.as_ref() {
                if let Some(last) = path.path.segments.last() {
                    if matches!(self.expected, QueueCall::Function(name) if last.ident == name) {
                        self.count();
                    }
                    if last.ident == "with_queue_extension" {
                        // Die Barriere gilt im dritten Argument, dem aufzurufenden
                        // Callback. Die übrigen Argumente werden vorher ausgewertet.
                        let enclosing = self.in_extension;
                        for (index, arg) in call.args.iter().enumerate() {
                            self.in_extension =
                                enclosing || index == 2 && matches!(arg, syn::Expr::Closure(_));
                            self.visit_expr(arg);
                        }
                        self.in_extension = enclosing;
                        return;
                    }
                }
            }
            syn::visit::visit_expr_call(self, call);
        }

        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if matches!(self.expected, QueueCall::Method(name) if call.method == name) {
                self.count();
            }
            syn::visit::visit_expr_method_call(self, call);
        }

        fn visit_item(&mut self, item: &'ast syn::Item) {
            if self.include_items {
                syn::visit::visit_item(self, item);
            }
        }
    }

    #[test]
    fn queue_inventur_unterscheidet_callback_und_argumentauswertung() {
        let file = syn::parse_file(
            r#"
            fn run() {
                // resume_queue();
                let _ = "queue.enqueue_resumable_copies()";
                let _ = with_queue_extension;
                with_queue_extension(queue.enqueue_resumable_copies(), window, || {
                    queue .enqueue_resumable_copies /* Kommentar */ ();
                });
                queue.enqueue_resumable_copies();
            }
            fn anderer_befehl() { resume_queue(); }
            "#,
        )
        .unwrap();
        let command = named_function(&file, "Test", "run");
        let mutation =
            QueueCalls::in_function(command, QueueCall::Method("enqueue_resumable_copies"));
        assert_eq!((mutation.guarded, mutation.unguarded), (1, 2));
        assert_eq!(
            QueueCalls::in_function(command, QueueCall::Function("resume_queue")).total(),
            0
        );
    }

    #[test]
    fn queue_wiederaufnahme_und_spezial_enqueue_laufen_unter_der_exit_barriere() {
        for (name, source, command, mutation) in [
            (
                "fortsetzbare Kopien",
                include_str!("file_commands.rs"),
                "enqueue_resumable_copy",
                QueueCall::Method("enqueue_resumable_copies"),
            ),
            (
                "direkter bidirektionaler Lauf",
                include_str!("bidirectional_commands.rs"),
                "run_bidirectional_sync",
                QueueCall::Method("enqueue_claimed_with_profile_bindings"),
            ),
            (
                "Queue-Wiederaufnahme",
                include_str!("queue_commands.rs"),
                "resume_queue",
                QueueCall::Function("change"),
            ),
        ] {
            let file = syn::parse_file(source).unwrap_or_else(|error| panic!("{name}: {error}"));
            let calls = QueueCalls::in_function(named_function(&file, name, command), mutation);
            assert!(
                calls.guarded > 0,
                "{name}: Queue-Mutation im Exit-Callback fehlt"
            );
            assert_eq!(
                calls.unguarded, 0,
                "{name}: Queue-Mutation außerhalb des Exit-Callbacks"
            );
        }

        // Diese Bedienbefehle teilen die Wiederaufnahme unter derselben Barriere.
        let commands = syn::parse_file(include_str!("queue_commands.rs")).unwrap();
        for command in [
            "retry_queue_entry",
            "discard_queue_confirmation",
            "discard_replan_required_queue_entry",
            "acknowledge_legacy_webdav_delete",
            "acknowledge_unclear_queue_outcome",
            "approve_queue_entry",
        ] {
            let function = named_function(&commands, "Queue-Bedienung", command);
            assert!(
                QueueCalls::in_function(function, QueueCall::Function("resume_queue")).total() > 0,
                "{command}: gemeinsamer Wiederaufnahme-Helfer fehlt"
            );
        }

        for (name, source) in [
            ("lokaler Sync", include_str!("local_sync_commands.rs")),
            ("Dateioperationen", include_str!("file_commands.rs")),
            ("SFTP", include_str!("sftp_commands.rs")),
            ("FTPS", include_str!("ftps_commands.rs")),
            ("WebDAV", include_str!("webdav_commands.rs")),
            ("SMB", include_str!("smb_commands.rs")),
            ("Provider-Sync", include_str!("provider_sync_commands.rs")),
            (
                "bidirektionaler Sync",
                include_str!("bidirectional_commands.rs"),
            ),
        ] {
            use syn::visit::Visit;
            let file = syn::parse_file(source).unwrap_or_else(|error| panic!("{name}: {error}"));
            let mut calls = QueueCalls::new(QueueCall::Method("enqueue"));
            calls.include_items = true;
            calls.visit_file(&file);
            assert_eq!(calls.total(), 0, "{name}: direkter enqueue-Aufruf");
        }

        for (name, source, identifier) in [
            (
                "SFTP-Workerfreigabe",
                include_str!("sftp_commands.rs"),
                "approve_sftp_queue_entry",
            ),
            (
                "FTPS-Workerfreigabe",
                include_str!("ftps_commands.rs"),
                "approve_ftps_queue_entry",
            ),
            (
                "WebDAV-Workerfreigabe",
                include_str!("webdav_commands.rs"),
                "approve_webdav_queue_entry",
            ),
            (
                "SMB-Workerfreigabe",
                include_str!("smb_commands.rs"),
                "approve_smb_queue_entry",
            ),
            (
                "Provider-Workerfreigabe",
                include_str!("provider_sync_commands.rs"),
                "approve_provider_queue_entry",
            ),
            (
                "bidirektionale Workerfreigabe",
                include_str!("bidirectional_commands.rs"),
                "approve_bidirectional_queue_entry",
            ),
        ] {
            let file = syn::parse_file(source).unwrap_or_else(|error| panic!("{name}: {error}"));
            let approval = named_function(&file, name, identifier);
            let start = if identifier == "approve_bidirectional_queue_entry" {
                let helper = "reserve_bidirectional_queue_approval";
                assert!(
                    QueueCalls::in_function(approval, QueueCall::Function(helper)).total() > 0,
                    "{name}: Aufruf der typisierten Queue-Freigabe fehlt"
                );
                named_function(&file, name, helper)
            } else {
                approval
            };
            assert!(
                uses_pending_start(start),
                "{name} muss Bestätigung, Pending-Entnahme und Reservierung unter der gemeinsamen Exit-Barriere binden"
            );
        }
    }

    #[derive(Default)]
    struct PendingStartCall(bool);

    impl<'ast> syn::visit::Visit<'ast> for PendingStartCall {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if let syn::Expr::Path(path) = call.func.as_ref() {
                let segments = &path.path.segments;
                let known_path = match segments.len() {
                    1 => true,
                    2 => segments[0].ident == "crate",
                    3 => segments[0].ident == "crate" && segments[1].ident == "queue_runtime",
                    _ => false,
                };
                self.0 |= path.qself.is_none()
                    && path.path.leading_colon.is_none()
                    && known_path
                    && segments
                        .last()
                        .is_some_and(|part| part.ident == "take_pending_for_operation");
            }
            syn::visit::visit_expr_call(self, call);
        }

        // Eine im Command deklarierte Hilfsfunktion führt ihren Rumpf nicht aus.
        fn visit_item(&mut self, _: &'ast syn::Item) {}
    }

    fn uses_pending_start(command: &syn::ItemFn) -> bool {
        use syn::visit::Visit;
        let mut call = PendingStartCall::default();
        call.visit_block(&command.block);
        call.0
    }

    #[test]
    fn pending_start_guard_prueft_nur_aufrufe_im_benannten_command() {
        let missing = syn::parse_file(
            r#"
            fn run_sync() {
                // take_pending_for_operation();
                let _ = "take_pending_for_operation()";
                let _ = take_pending_for_operation;
                audit_take_pending_for_operation();
                other::take_pending_for_operation();
                fn helper() { take_pending_for_operation(); }
            }
            fn approve_queue_entry() { take_pending_for_operation(); }
            fn later_helper() { take_pending_for_operation(); }
        "#,
        )
        .unwrap();
        let starts = |identifier| uses_pending_start(named_function(&missing, "Test", identifier));
        assert!(!starts("run_sync"));
        assert!(starts("approve_queue_entry"));
        assert!(starts("later_helper"));

        for source in [
            "fn run_sync() { take_pending_for_operation(); }",
            r#"pub(crate) async fn run_sync () {
                spawn_blocking(move || {
                    crate :: queue_runtime :: take_pending_for_operation::<_, _>
                        /* dieselbe Starttransaktion */ (state, window, pending);
                });
            }"#,
        ] {
            let file = syn::parse_file(source).unwrap();
            assert!(uses_pending_start(named_function(
                &file, "Test", "run_sync"
            )));
        }
    }

    #[test]
    fn alle_direkten_pending_starts_nutzen_die_zentrale_starttransaktion() {
        for (name, source, identifier) in [
            (
                "lokaler Realmodus",
                include_str!("local_sync_commands.rs"),
                "run_real_sync",
            ),
            (
                "SFTP-Realmodus",
                include_str!("sftp_commands.rs"),
                "run_sftp_sync",
            ),
            (
                "FTPS-Realmodus",
                include_str!("ftps_commands.rs"),
                "run_ftps_sync",
            ),
            (
                "WebDAV-Realmodus",
                include_str!("webdav_commands.rs"),
                "run_webdav_sync",
            ),
            (
                "SMB-Realmodus",
                include_str!("smb_commands.rs"),
                "run_smb_sync",
            ),
            (
                "bidirektionaler Rollback",
                include_str!("bidirectional_commands.rs"),
                "run_bidirectional_rollback",
            ),
        ] {
            let file = syn::parse_file(source).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(
                uses_pending_start(named_function(&file, name, identifier)),
                "{name} umgeht die zentrale Reihenfolge Exit-Barriere → Pending → Reservierung"
            );
        }
    }

    #[test]
    fn remote_queue_request_akzeptiert_verschachtelte_frontend_feldnamen() {
        let request: EnqueueRemoteSyncRequest<SftpConnectionConfig> =
            serde_json::from_value(serde_json::json!({
                "profileId": "server",
                "profileRevision": 21,
                "direction": "Download",
                "localRoot": "/lokal",
                "remoteRoot": "/remote",
                "options": serde_json::to_value(SyncOptions::default()).unwrap(),
                "expectedPlan": serde_json::to_value(SyncPlan::default()).unwrap()
            }))
            .unwrap();

        assert_eq!(request.profile_id, "server");
        assert_eq!(request.profile_revision, Some(21));
        assert_eq!(request.expected_plan, SyncPlan::default());
    }

    #[test]
    fn protokoll_enqueue_bewahrt_und_verbucht_exakt_den_jeweils_vorbereiteten_lauf() {
        use doppeldecker_core::{
            prepare_provider_transfer_for_queue, Compare, FtpTransportSecurity, FtpsCertificatePin,
            FtpsCertificatePolicy, KeychainSecretReference, ProviderEndpoint, QueueState,
            SftpAuthenticationReference, SftpHostKeyPin,
        };
        use std::fs;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("beispiel.txt"), b"vorbereiteter Inhalt").unwrap();
        let journal = root.join("config/operations-queue.json");
        let lock = Arc::new(OperationQueueProcessLock::acquire(&journal).unwrap());
        let queue =
            Arc::new(OperationQueue::open_with_process_lock(&journal, lock.clone()).unwrap());
        let state = AppState::default();
        let window = state.window("main");

        let options = SyncOptions {
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        };
        let profile_id = "testprofil";
        let secret = || KeychainSecretReference::new("Doppeldecker-Test", profile_id).unwrap();
        let local_root = source.to_str().unwrap();
        let remote_root = "/daten";
        let prepare = || {
            prepare_provider_transfer_for_queue(
                &queue,
                ProviderEndpoint::local(&source).unwrap(),
                ProviderEndpoint::local(&target).unwrap(),
                options.clone(),
            )
            .unwrap()
        };
        let plan = prepare().preview().plan.clone();

        macro_rules! prepared_case {
            ($protocol:ident, $slot:ident, $operation:ident, $config:expr) => {{
                let config = $config;
                (
                    RemoteProfileProtocol::$protocol,
                    &window.$slot,
                    RemoteSyncConnection::from(config.clone()),
                    QueueOperation::$operation {
                        direction: SftpQueueDirection::Upload,
                        connection: Box::new(config),
                        local_root: local_root.into(),
                        remote_root: remote_root.into(),
                        options: options.clone(),
                        expected_plan: Box::new(plan.clone()),
                    },
                )
            }};
        }
        let cases = [
            prepared_case!(
                Sftp,
                pending_sftp_real,
                SftpSync,
                SftpConnectionConfig {
                    host: "sftp.example.test".into(),
                    alternate_hosts: Vec::new(),
                    port: 22,
                    username: "test".into(),
                    requested_root: remote_root.into(),
                    host_key: SftpHostKeyPin {
                        algorithm: "ssh-ed25519".into(),
                        sha256: [1; 32],
                    },
                    authentication: SftpAuthenticationReference::Password { secret: secret() },
                }
            ),
            prepared_case!(
                Ftps,
                pending_ftps_real,
                FtpsSync,
                FtpsConnectionConfig {
                    host: "ftp.example.test".into(),
                    alternate_hosts: Vec::new(),
                    port: 21,
                    username: "test".into(),
                    requested_root: remote_root.into(),
                    transport_security: FtpTransportSecurity::Plaintext,
                    certificate: None,
                    certificate_policy: None,
                    password_reference: secret(),
                }
            ),
            prepared_case!(
                Webdav,
                pending_webdav_real,
                WebDavSync,
                WebDavConnectionConfig {
                    origin: "https://dav.example.test".into(),
                    username: "test".into(),
                    requested_root: remote_root.into(),
                    certificate: FtpsCertificatePin { sha256: [4; 32] },
                    certificate_policy: FtpsCertificatePolicy::WebPkiAndPin,
                    password_reference: secret(),
                }
            ),
            prepared_case!(
                Smb,
                pending_smb_real,
                SmbSync,
                SmbConnectionConfig {
                    host: "smb.example.test".into(),
                    port: 445,
                    server_guid: [7; 16],
                    share: "Daten".into(),
                    username: "test".into(),
                    domain: "WORKGROUP".into(),
                    requested_root: remote_root.into(),
                    password_reference: secret(),
                }
            ),
        ];
        for (protocol, slot, connection, operation) in cases {
            let prepared = prepare();
            let bindings = vec![RemoteProfileBinding::new(protocol, profile_id, 3).unwrap()];
            *slot.lock_recover() = Some(PendingRemoteRealRun {
                prepared,
                operation: operation.clone(),
                profile_bindings: bindings.clone(),
            });
            let request = |remote_root: &str, revision| EnqueueRemoteSyncRequest {
                profile_id: profile_id.into(),
                profile_revision: Some(revision),
                direction: SftpQueueDirection::Upload,
                local_root: local_root.into(),
                remote_root: remote_root.into(),
                options: options.clone(),
                expected_plan: plan.clone(),
                connection_type: PhantomData,
            };
            let resolve = |id: &str| {
                assert_eq!(id, profile_id);
                Ok(connection.clone())
            };
            let before = queue.snapshot();
            for mismatched in [request(remote_root, 2), request("/abweichend", 3)] {
                let failure =
                    enqueue_protocol_sync_using(&state, &window, mismatched, resolve, |_| {
                        panic!("Abweichende Anfragen dürfen die Queue nicht öffnen")
                    });
                assert_eq!(failure.err().unwrap().code, "plan_drift");
                assert!(slot.lock_recover().is_some());
            }
            let failure = enqueue_protocol_sync_using(
                &state,
                &window,
                request(remote_root, 3),
                resolve,
                |bound| {
                    assert_eq!(bound, bindings);
                    Err(AppError::new("busy", "Queue vorübergehend belegt"))
                },
            );
            assert_eq!(failure.err().unwrap().code, "busy");
            assert_eq!(slot.lock_recover().as_ref().unwrap().operation, operation);
            assert_eq!(queue.snapshot(), before);

            enqueue_protocol_sync_using(
                &state,
                &window,
                request(remote_root, 3),
                resolve,
                |bound| {
                    assert_eq!(bound, bindings);
                    Ok(queue.clone())
                },
            )
            .unwrap();
            assert!(slot.lock_recover().is_none());
            let after = queue.snapshot();
            assert_eq!(after.entries.len(), before.entries.len() + 1);
            let committed = after.entries.last().unwrap();
            assert_eq!(committed.operation, operation);
            assert_eq!(committed.profile_bindings, bindings);
            assert_eq!(committed.state, QueueState::Pending);
        }
        assert_eq!(
            OperationQueue::open_with_process_lock(&journal, lock)
                .unwrap()
                .snapshot(),
            queue.snapshot()
        );
        assert!(fs::read_dir(&target).unwrap().next().is_none());
    }

    #[test]
    fn queuefehler_erhaelt_code_und_fortschritt_in_journal_und_terminalereignis() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config/operations-queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let entry = queue
            .enqueue_claimed(QueueOperation::File {
                kind: doppeldecker_core::FileQueueKind::Copy,
                items: vec!["/quelle/datei".into()],
                destination: "/ziel".into(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        queue.mark_running(&entry.operation_id).unwrap();
        queue
            .update_progress(
                &entry.operation_id,
                &Progress {
                    files_done: 1,
                    ..Progress::default()
                },
            )
            .unwrap();
        let error = doppeldecker_core::coded_error(
            std::io::ErrorKind::Other,
            doppeldecker_core::ErrorCode::OutcomeUnclear,
            "Zielwirkung konnte nicht geprüft werden",
        );
        let QueueTerminalOutcome::Error { event, error } = persist_queue_execution_error(
            &queue,
            &entry.operation_id,
            QueueOperationClass::File,
            &error,
        ) else {
            panic!("Die Ausführung lieferte kein Fehlerereignis");
        };
        assert_eq!(event, "fileop-error");
        drop(queue);
        let queue = OperationQueue::open(&path).unwrap();
        let stored = &queue.snapshot().entries[0];
        assert_eq!(stored.state, doppeldecker_core::QueueState::Failed);
        assert_eq!(
            stored.error_code,
            Some(doppeldecker_core::ErrorCode::OutcomeUnclear)
        );
        assert_eq!(stored.error_code.unwrap().as_str(), error.code);
        assert_eq!(stored.error.as_deref(), Some(error.message.as_str()));
        assert_eq!(stored.progress.files_done, 1);
    }

    #[test]
    fn queue_done_handler_kann_nach_freigabe_sofort_neu_reservieren() {
        use std::cell::{Cell, RefCell};

        let reserved = Cell::new(true);
        let trace = RefCell::new(Vec::new());
        finish_queue_entry_in_order(
            || {
                reserved.set(false);
                trace.borrow_mut().push("Reservation freigegeben");
            },
            || {
                assert!(!reserved.get(), "Terminalsnapshot sah noch Busy");
                trace.borrow_mut().push("Terminalsnapshot");
            },
            || {
                assert!(!reserved.get(), "Done-Handler sah noch Busy");
                reserved.set(true);
                trace.borrow_mut().push("Done-Handler reserviert neu");
            },
        );
        assert!(reserved.get());
        assert_eq!(
            trace.into_inner(),
            [
                "Reservation freigegeben",
                "Terminalsnapshot",
                "Done-Handler reserviert neu",
            ]
        );
    }

    fn named_function<'a>(file: &'a syn::File, name: &str, identifier: &str) -> &'a syn::ItemFn {
        file.items
            .iter()
            .find_map(|item| match item {
                syn::Item::Fn(function) if function.sig.ident == identifier => Some(function),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name}: {identifier} fehlt"))
    }

    #[derive(Default)]
    struct TerminalCalls {
        emits: usize,
        finishes: usize,
        finishes_without_inline_callback: usize,
        snapshots_after_release: usize,
        snapshots_outside_finish: usize,
        in_finish_callback: bool,
    }

    impl<'ast> syn::visit::Visit<'ast> for TerminalCalls {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if let syn::Expr::Path(path) = call.func.as_ref() {
                if let Some(last) = path.path.segments.last() {
                    if last.ident == "finish_operation" {
                        self.finishes += 1;
                        if !matches!(call.args.iter().nth(2), Some(syn::Expr::Closure(_))) {
                            self.finishes_without_inline_callback += 1;
                        }
                        // Nur der dritte Parameter ist der Callback, den
                        // finish_operation nach Freigabe tatsächlich ausführt.
                        let enclosing_callback = self.in_finish_callback;
                        for (index, arg) in call.args.iter().enumerate() {
                            self.in_finish_callback = enclosing_callback
                                || index == 2 && matches!(arg, syn::Expr::Closure(_));
                            self.visit_expr(arg);
                        }
                        self.in_finish_callback = enclosing_callback;
                        return;
                    }
                    if last.ident == "emit_queue_snapshot" {
                        if self.in_finish_callback {
                            self.snapshots_after_release += 1;
                        } else {
                            self.snapshots_outside_finish += 1;
                        }
                    }
                    if last.ident == "emit_to"
                        || last.ident == "emit_to_window"
                        || last.ident == "emit_to_window_label"
                    {
                        self.emits += 1;
                    }
                }
            }
            syn::visit::visit_expr_call(self, call);
        }

        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if call.method == "emit_to" {
                self.emits += 1;
            }
            syn::visit::visit_expr_method_call(self, call);
        }
    }

    fn terminal_calls(command: &syn::ItemFn) -> TerminalCalls {
        use syn::visit::Visit;
        let mut calls = TerminalCalls::default();
        calls.visit_block(&command.block);
        calls
    }

    #[test]
    fn terminal_wiring_guard_distinguishes_calls_from_text_and_checks_callback() {
        let prepare = syn::parse_str(
            r#"fn prepare() {
                // emit_to_window(&window, "sync-error", error);
                let _ = "emit_to_window_label(...) .emit_to(...)";
                let _ = crate::emit_to_window;
                audit_emit_to_window(&window);
            }"#,
        )
        .unwrap();
        assert_eq!(terminal_calls(&prepare).emits, 0);
        for source in [
            r#"fn prepare() { event .emit_to (&window); }"#,
            r#"fn prepare() { tauri::Emitter::emit_to(&app, label); }"#,
            r#"fn prepare() { crate::emit_to_window /* gezieltes Ereignis */ (&window); }"#,
            r#"fn prepare() { emit_to_window_label::<_, _> (&app, label); }"#,
        ] {
            assert_eq!(terminal_calls(&syn::parse_str(source).unwrap()).emits, 1);
        }

        let approve: syn::ItemFn = syn::parse_quote! {
            fn approve() {
                emit_queue_snapshot(&app, label, &queue);
                finish_operation(&app, label, || {
                    if complete { crate::emit_queue_snapshot(&app, label, &queue); }
                });
            }
        };
        let calls = terminal_calls(&approve);
        assert_terminal_callbacks("Testfreigabe", &calls, 1);
        assert_eq!(calls.snapshots_after_release, 1);
        assert_eq!(calls.snapshots_outside_finish, 1);

        let misplaced: syn::ItemFn = syn::parse_quote! {
            fn approve() {
                finish_operation(|| emit_queue_snapshot(&app, label, &queue), label, callback);
            }
        };
        assert_eq!(
            terminal_calls(&misplaced).finishes_without_inline_callback,
            1
        );
    }

    #[test]
    fn remote_worker_und_approvals_emittieren_terminal_erst_nach_freigabe() {
        for (name, source, prepare, approve) in [
            (
                "SFTP",
                include_str!("sftp_commands.rs"),
                "prepare_remote_queue_entry",
                "approve_sftp_queue_entry",
            ),
            (
                "FTPS",
                include_str!("ftps_commands.rs"),
                "prepare_remote_queue_entry",
                "approve_ftps_queue_entry",
            ),
            (
                "WebDAV",
                include_str!("webdav_commands.rs"),
                "prepare_remote_queue_entry",
                "approve_webdav_queue_entry",
            ),
            (
                "SMB",
                include_str!("smb_commands.rs"),
                "prepare_remote_queue_entry",
                "approve_smb_queue_entry",
            ),
            (
                "Provider",
                include_str!("provider_sync_commands.rs"),
                "prepare_remote_queue_entry",
                "approve_provider_queue_entry",
            ),
            (
                "bidirektionaler Sync",
                include_str!("bidirectional_commands.rs"),
                "prepare_queue_entry",
                "approve_bidirectional_queue_entry",
            ),
        ] {
            let file = syn::parse_file(source).unwrap_or_else(|error| panic!("{name}: {error}"));
            // Der Rückgabetyp aller sechs Vorbereitungen wird bereits am
            // Produktionsaufruf durch execute_queue_entry erzwungen.
            assert_eq!(
                terminal_calls(named_function(&file, name, prepare)).emits,
                0,
                "{name}: Vorbereitung sendet Ereignisse unter der Workerreservation"
            );

            let approval = terminal_calls(named_function(&file, name, approve));
            assert!(approval.finishes > 0, "{name}: Freigabe fehlt");
            assert!(
                approval.snapshots_after_release > 0,
                "{name}: kein terminaler Snapshot im Freigabe-Callback"
            );
            assert_eq!(
                approval.snapshots_outside_finish, 0,
                "{name}: terminaler Snapshot außerhalb des Freigabe-Callbacks"
            );
        }
    }

    #[test]
    fn folgeworker_startet_erst_nach_terminalevent_des_vorigen_eintrags() {
        use std::cell::RefCell;
        use std::collections::VecDeque;

        let entries = RefCell::new(VecDeque::from(["erster", "zweiter"]));
        let trace = RefCell::new(Vec::new());
        finish_operation_in_order(
            || trace.borrow_mut().push("Reservation erster freigegeben"),
            || {
                assert_eq!(entries.borrow_mut().pop_front(), Some("erster"));
                trace.borrow_mut().push("Terminalevent erster");
            },
            || {
                assert_eq!(entries.borrow_mut().pop_front(), Some("zweiter"));
                trace.borrow_mut().push("Workerstart zweiter");
            },
        );

        assert_eq!(
            trace.into_inner(),
            [
                "Reservation erster freigegeben",
                "Terminalevent erster",
                "Workerstart zweiter",
            ]
        );
    }

    #[test]
    fn alle_terminalpfade_geben_das_event_vor_dem_folgeworker_weiter() {
        use syn::visit::Visit;
        // Die Signatur von `finish_operation` verhindert bereits neue Aufrufer ohne
        // Event-Callback. Diese Inventur hält zusätzlich fest, welche Commands heute
        // direkte Operationen beenden. So fällt bei einem neuen Pfad sofort auf, wenn
        // dessen Terminalevent nicht in die gemeinsame Reihenfolge aufgenommen wurde.
        for (name, source, expected_calls) in [
            ("Update", include_str!("update_commands.rs"), 3),
            ("lokaler Sync", include_str!("local_sync_commands.rs"), 1),
            // SFTP, FTPS, WebDAV, SMB und Provider-Sync führen ihren bestätigten
            // Realmodus-Lauf über `spawn_confirmed_remote_run` aus; dessen beide
            // Abschlusspfade werden unten eigens geprüft. Hier zählen deshalb nur
            // noch die übrigen direkten Abschlüsse dieser Dateien.
            ("SFTP", include_str!("sftp_commands.rs"), 4),
            ("FTPS", include_str!("ftps_commands.rs"), 2),
            ("WebDAV", include_str!("webdav_commands.rs"), 2),
            ("SMB", include_str!("smb_commands.rs"), 4),
            (
                "Provider-Sync",
                include_str!("provider_sync_commands.rs"),
                2,
            ),
            (
                "bidirektionaler Sync",
                include_str!("bidirectional_commands.rs"),
                5,
            ),
        ] {
            let file = syn::parse_file(source).unwrap_or_else(|error| panic!("{name}: {error}"));
            let mut calls = TerminalCalls::default();
            calls.visit_file(&file);
            assert_terminal_callbacks(name, &calls, expected_calls);
        }

        // Bindungsfehler und regulärer Abschluss nutzen denselben Remote-Worker.
        let name = "gemeinsamer Remote-Ablauf";
        let file = syn::parse_file(include_str!("queue_runtime.rs")).unwrap();
        let shared = named_function(&file, name, "spawn_confirmed_remote_run");
        assert_terminal_callbacks(name, &terminal_calls(shared), 2);
    }

    fn assert_terminal_callbacks(name: &str, calls: &TerminalCalls, expected_calls: usize) {
        assert_eq!(
            calls.finishes, expected_calls,
            "{name}: Inventur der direkten Abschlusspfade hat sich geändert"
        );
        assert_eq!(
            calls.finishes_without_inline_callback, 0,
            "{name}: finish_operation braucht den Terminal-Callback als drittes Argument"
        );
    }

    /// Regression: Eine Pane ohne Pfad schickte einen Leerstring als `parent`. Der
    /// wurde relativ zur kanonischen Provider-Root aufgelöst — der Ordner entstand
    /// also an einer für den Nutzer unsichtbaren Stelle statt sichtbar zu scheitern.
    #[test]
    fn leerer_oder_relativer_elternpfad_wird_abgewiesen() {
        for parent in ["", ".", "unterordner", "a/b"] {
            let error = require_absolute_remote_parent(parent)
                .expect_err("relativer Elternpfad muss abgewiesen werden");
            assert_eq!(error.code, "invalid_input");
        }
        assert!(require_absolute_remote_parent("/").is_ok());
        assert!(require_absolute_remote_parent("/ordner/tief").is_ok());
    }
}
