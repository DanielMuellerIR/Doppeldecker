//! Fenster- und panegebundene Laufzeit für lesende Ordnergrößen.
//!
//! Die eigentliche Rekursion, Parallelisierung und der Sitzungscache leben im Core.
//! Diese Tauri-Schicht bindet einen Lauf an das auslösende Fenster und genau eine
//! Pane, prüft den sichtbaren Elternbestand frisch und sendet Resultate nur an dieses
//! Fenster. Sie benutzt absichtlich weder die Operations-Queue noch `active_cancel`:
//! Ordnergrößen sind reine Leseoperationen und dürfen Dateioperationen nicht sperren.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use doppeldecker_core::vfs::CompleteDirectoryListing;
use doppeldecker_core::{
    Cancel, DirectorySizeMode, DirectorySizeResult, DirectorySizeSession, DirectorySizeStats,
    LocalVfs, Vfs,
};
use tauri::{Manager, State};

use crate::bidirectional_commands::BidirectionalEndpointRequest;
use crate::{require_absolute_remote_parent, AppError, AppState, MutexExt};

const RESULT_EVENT: &str = "directory-size-result";
const DONE_EVENT: &str = "directory-size-done";
const ERROR_EVENT: &str = "directory-size-error";
const MAX_REQUESTED_FOLDERS: usize = 20_000;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct PaneKey {
    window_label: String,
    side: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum EndpointIdentity {
    Local,
    Sftp { profile_id: String, generation: u64 },
    Ftps { profile_id: String, generation: u64 },
    Webdav { profile_id: String, generation: u64 },
    Smb { profile_id: String, generation: u64 },
}

struct ActiveRun {
    run_id: u64,
    cancel: Cancel,
}

struct PaneRuntime {
    identity: EndpointIdentity,
    /// Zusätzlich zur Profil-ID bindet die Arc-Identität die konkrete lebende
    /// Provider-Verbindung. Nach Profiländerung oder Eject kann eine neue Sitzung
    /// wieder bei Generation 1 beginnen und darf dennoch nie den alten Cache nutzen.
    vfs: Arc<dyn Vfs>,
    session: Arc<DirectorySizeSession>,
    active: Option<ActiveRun>,
}

/// Eigene Laufzeit statt zusätzlicher Felder im prozessweiten Mutationszustand.
/// Der Schlüssel enthält bereits heute das Fensterlabel, damit ein zweites Fenster
/// weder Cache noch Abbruchsignal einer gleichnamigen Pane teilt.
pub(crate) struct DirectorySizeRuntime {
    panes: Mutex<HashMap<PaneKey, PaneRuntime>>,
    next_run_id: AtomicU64,
}

impl Default for DirectorySizeRuntime {
    fn default() -> Self {
        Self {
            panes: Mutex::new(HashMap::new()),
            next_run_id: AtomicU64::new(1),
        }
    }
}

impl DirectorySizeRuntime {
    fn start(
        &self,
        key: PaneKey,
        identity: EndpointIdentity,
        vfs: Arc<dyn Vfs>,
    ) -> (u64, Arc<DirectorySizeSession>, Cancel) {
        let run_id = self.next_run_id.fetch_add(1, Ordering::Relaxed);
        let cancel = Cancel::new();
        let mut panes = self.panes.lock_recover();
        let slot = panes.entry(key).or_insert_with(|| PaneRuntime {
            identity: identity.clone(),
            vfs: Arc::clone(&vfs),
            session: Arc::new(DirectorySizeSession::new(vfs.clone())),
            active: None,
        });
        let same_endpoint = slot.identity == identity
            && (matches!(identity, EndpointIdentity::Local) || Arc::ptr_eq(&slot.vfs, &vfs));
        if !same_endpoint {
            if let Some(active) = slot.active.take() {
                active.cancel.cancel();
            }
            *slot = PaneRuntime {
                identity,
                vfs: Arc::clone(&vfs),
                session: Arc::new(DirectorySizeSession::new(vfs)),
                active: None,
            };
        } else if let Some(active) = slot.active.take() {
            active.cancel.cancel();
        }
        slot.active = Some(ActiveRun {
            run_id,
            cancel: cancel.clone(),
        });
        (run_id, Arc::clone(&slot.session), cancel)
    }

    fn cancel(&self, key: &PaneKey) {
        if let Some(active) = self
            .panes
            .lock_recover()
            .get_mut(key)
            .and_then(|slot| slot.active.take())
        {
            active.cancel.cancel();
        }
    }

    fn is_current(&self, key: &PaneKey, run_id: u64) -> bool {
        self.panes
            .lock_recover()
            .get(key)
            .and_then(|slot| slot.active.as_ref())
            .is_some_and(|active| active.run_id == run_id)
    }

    /// Entfernt nur den Lauf, der tatsächlich endet. Ein inzwischen gestarteter
    /// Nachfolger behält sein eigenes Abbruchsignal und seine eigene Lauf-ID.
    fn finish(&self, key: &PaneKey, run_id: u64) -> bool {
        let mut panes = self.panes.lock_recover();
        let Some(slot) = panes.get_mut(key) else {
            return false;
        };
        if slot
            .active
            .as_ref()
            .is_none_or(|active| active.run_id != run_id)
        {
            return false;
        }
        slot.active.take();
        true
    }
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DirectorySizeRequest {
    side: String,
    endpoint: BidirectionalEndpointRequest,
    names: Vec<String>,
    session_generation: Option<u64>,
    recalculate: bool,
    client_token: u64,
}

#[derive(serde::Serialize)]
pub(crate) struct DirectorySizeStarted {
    run_id: u64,
}

#[derive(Clone, serde::Serialize)]
struct DirectorySizeResultEvent {
    side: String,
    run_id: u64,
    client_token: u64,
    parent: String,
    name: String,
    result: DirectorySizeResult,
}

#[derive(Clone, serde::Serialize)]
struct DirectorySizeDoneEvent {
    side: String,
    run_id: u64,
    client_token: u64,
    parent: String,
    stats: DirectorySizeStats,
}

#[derive(Clone, serde::Serialize)]
struct DirectorySizeErrorEvent {
    side: String,
    run_id: u64,
    client_token: u64,
    parent: String,
    error: AppError,
}

struct EndpointContext {
    identity: EndpointIdentity,
    vfs: Arc<dyn Vfs>,
    parent: PathBuf,
    sftp: Option<Arc<doppeldecker_core::SftpVfs>>,
}

fn validate_side(side: &str) -> Result<(), AppError> {
    if matches!(side, "left" | "right") {
        Ok(())
    } else {
        Err(AppError::new(
            "invalid_input",
            "Ordnergrößen benötigen die Pane 'left' oder 'right'",
        ))
    }
}

fn validate_requested_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\0')
        && Path::new(name).file_name().and_then(|value| value.to_str()) == Some(name)
}

fn require_generation(expected: Option<u64>, actual: u64) -> Result<(), AppError> {
    if expected == Some(actual) && actual > 0 {
        Ok(())
    } else {
        Err(AppError::new(
            "plan_drift",
            "Die Remote-Sitzung hat seit dem sichtbaren Listing gewechselt",
        ))
    }
}

/// Bindet einen Remote-Endpunkt an genau die Sitzung, die das sichtbare Listing
/// erzeugt hat.
///
/// Für alle vier Provider ist der Ablauf derselbe: absoluter Elternpfad, gecachte
/// Verbindung mit geprüfter Profilrevision und dieselbe Sitzungsgeneration wie in
/// der Anfrage. Nur das Providermodul und die Identitätsvariante unterscheiden
/// sich. Als vier Kopien hätte eine davon still eine dieser Prüfungen verlieren
/// können — deshalb steht der Ablauf hier einmal.
macro_rules! bind_remote_endpoint {
    (
        $app:expr,
        $state:expr,
        $request:expr,
        $module:ident,
        $identity:ident,
        $profile_id:expr,
        $profile_revision:expr,
        $root:expr,
        $sftp:expr $(,)?
    ) => {{
        require_absolute_remote_parent($root)?;
        let cached =
            crate::$module::cached_connection_bound($app, $state, $profile_id, $profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(
            $profile_revision,
            &cached.binding,
        )?;
        let connection = cached.connection;
        let generation = connection.session_generation();
        require_generation($request.session_generation, generation)?;
        let sftp: Option<Arc<doppeldecker_core::SftpVfs>> = ($sftp)(&connection);
        Ok(EndpointContext {
            identity: EndpointIdentity::$identity {
                profile_id: $profile_id.clone(),
                generation,
            },
            vfs: connection,
            parent: PathBuf::from($root),
            sftp,
        })
    }};
}

fn resolve_context(
    app: &tauri::AppHandle,
    state: &AppState,
    request: &DirectorySizeRequest,
) -> Result<EndpointContext, AppError> {
    match &request.endpoint {
        BidirectionalEndpointRequest::Local { root } => {
            let endpoint = doppeldecker_core::ProviderEndpoint::local(Path::new(root))
                .map_err(AppError::from)?;
            Ok(EndpointContext {
                identity: EndpointIdentity::Local,
                vfs: Arc::new(LocalVfs::new()),
                parent: endpoint.root().to_path_buf(),
                sftp: None,
            })
        }
        BidirectionalEndpointRequest::Sftp {
            profile_id,
            profile_revision,
            root,
        } => bind_remote_endpoint!(
            app,
            state,
            request,
            sftp_commands,
            Sftp,
            profile_id,
            *profile_revision,
            root,
            |connection: &Arc<doppeldecker_core::SftpVfs>| Some(connection.clone()),
        ),
        BidirectionalEndpointRequest::Ftps {
            profile_id,
            profile_revision,
            root,
        } => bind_remote_endpoint!(
            app,
            state,
            request,
            ftps_commands,
            Ftps,
            profile_id,
            *profile_revision,
            root,
            |_| None,
        ),
        BidirectionalEndpointRequest::Webdav {
            profile_id,
            profile_revision,
            root,
        } => bind_remote_endpoint!(
            app,
            state,
            request,
            webdav_commands,
            Webdav,
            profile_id,
            *profile_revision,
            root,
            |_| None,
        ),
        BidirectionalEndpointRequest::Smb {
            profile_id,
            profile_revision,
            root,
        } => bind_remote_endpoint!(
            app,
            state,
            request,
            smb_commands,
            Smb,
            profile_id,
            *profile_revision,
            root,
            |_| None,
        ),
    }
}

/// Vergleicht neben der Sitzungsgeneration auch die konkrete `Arc` im gemeinsamen
/// Verbindungspool. Die alte `Arc` wird vom Größenlauf gehalten; ihre Adresse kann
/// deshalb während des Vergleichs nicht für eine neue Verbindung wiederverwendet
/// werden. So bleibt auch eine Ersatzverbindung mit erneutem Zählerstand 1 eindeutig.
fn provider_connection_is_current<T>(
    current: Option<Arc<T>>,
    expected_vfs: &Arc<dyn Vfs>,
    expected_generation: u64,
    generation_of: impl FnOnce(&T) -> u64,
) -> bool
where
    T: Vfs + 'static,
{
    current.is_some_and(|connection| {
        if generation_of(&connection) != expected_generation {
            return false;
        }
        let current_vfs: Arc<dyn Vfs> = connection;
        Arc::ptr_eq(&current_vfs, expected_vfs)
    })
}

/// Liest den bestehenden Verbindungs-Slot ohne einen Reconnect oder einen neuen
/// Profilaufbau auszulösen. Ein Wechsel während der Rekursion entwertet den Lauf,
/// bevor ein Resultat als verlässlich an die Oberfläche gelangt.
fn endpoint_is_current(
    state: &AppState,
    identity: &EndpointIdentity,
    expected_vfs: &Arc<dyn Vfs>,
) -> bool {
    match identity {
        EndpointIdentity::Local => true,
        EndpointIdentity::Sftp {
            profile_id,
            generation,
        } => provider_connection_is_current(
            state
                .sftp_connections
                .lock_recover()
                .get(profile_id)
                .map(|cached| cached.connection.clone()),
            expected_vfs,
            *generation,
            |connection| connection.session_generation(),
        ),
        EndpointIdentity::Ftps {
            profile_id,
            generation,
        } => provider_connection_is_current(
            state
                .ftps_connections
                .lock_recover()
                .get(profile_id)
                .map(|cached| cached.connection.clone()),
            expected_vfs,
            *generation,
            |connection| connection.session_generation(),
        ),
        EndpointIdentity::Webdav {
            profile_id,
            generation,
        } => provider_connection_is_current(
            state
                .webdav_connections
                .lock_recover()
                .get(profile_id)
                .map(|cached| cached.connection.clone()),
            expected_vfs,
            *generation,
            |connection| connection.session_generation(),
        ),
        EndpointIdentity::Smb {
            profile_id,
            generation,
        } => provider_connection_is_current(
            state
                .smb_connections
                .lock_recover()
                .get(profile_id)
                .map(|cached| cached.connection.clone()),
            expected_vfs,
            *generation,
            |connection| connection.session_generation(),
        ),
    }
}

fn select_requested_folders(
    parent: &Path,
    names: &[String],
    listing: CompleteDirectoryListing,
) -> Result<Vec<(String, PathBuf)>, AppError> {
    if names.is_empty() || names.len() > MAX_REQUESTED_FOLDERS {
        return Err(AppError::new(
            "invalid_input",
            format!("Ein Ordnergrößenlauf braucht 1 bis {MAX_REQUESTED_FOLDERS} Ordner"),
        ));
    }
    let mut requested = HashSet::new();
    for name in names {
        if !validate_requested_name(name) {
            return Err(AppError::new(
                "invalid_input",
                "Ordnergrößen akzeptieren nur einzelne, eindeutig dekodierte Namen",
            ));
        }
        requested.insert(name.clone());
    }

    let mut found = HashMap::new();
    for listed in listing.entries {
        if !requested.contains(&listed.entry.name) || !listed.safely_decoded {
            continue;
        }
        if !listed.entry.is_dir || listed.entry.is_symlink {
            return Err(AppError::new(
                "invalid_input",
                format!("{} ist kein echter Ordner", listed.entry.name),
            ));
        }
        if found
            .insert(listed.entry.name.clone(), parent.join(&listed.entry.name))
            .is_some()
        {
            return Err(AppError::new(
                "invalid_data",
                format!(
                    "Ordnername {} ist im Listing nicht eindeutig",
                    listed.entry.name
                ),
            ));
        }
    }
    if found.len() != requested.len() {
        return Err(AppError::new(
            "plan_drift",
            "Mindestens ein angeforderter Ordner fehlt im frischen Elternlisting",
        ));
    }
    let mut selected: Vec<_> = requested
        .into_iter()
        .map(|name| {
            let path = found
                .remove(&name)
                .expect("geprüfter Ordner fehlt in der Ergebnismenge");
            (name, path)
        })
        .collect();
    selected.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(selected)
}

fn emit_error(
    window: &tauri::WebviewWindow,
    key: &PaneKey,
    run_id: u64,
    client_token: u64,
    parent: &str,
    error: AppError,
) {
    let _ = crate::emit_to_window(
        window,
        ERROR_EVENT,
        DirectorySizeErrorEvent {
            side: key.side.clone(),
            run_id,
            client_token,
            parent: parent.to_string(),
            error,
        },
    );
}

fn run_measurement(
    window: tauri::WebviewWindow,
    key: PaneKey,
    request: DirectorySizeRequest,
    context: EndpointContext,
    run_id: u64,
    session: Arc<DirectorySizeSession>,
    cancel: Cancel,
) {
    let app = window.app_handle().clone();
    let parent_display = context.parent.to_string_lossy().into_owned();
    let selected = if context.sftp.is_some() {
        let paths = request
            .names
            .iter()
            .map(|name| context.parent.join(name))
            .collect::<Vec<_>>();
        doppeldecker_core::sftp::validate_interactive_content_selection(&context.parent, &paths)
            .map(|_| request.names.iter().cloned().zip(paths).collect())
            .map_err(AppError::from)
    } else {
        context
            .vfs
            .list_complete(&context.parent)
            .map_err(AppError::from)
            .and_then(|listing| select_requested_folders(&context.parent, &request.names, listing))
    };
    let selected = match selected {
        Ok(selected) => selected,
        Err(error) => {
            let state = app.state::<AppState>();
            let endpoint_current = endpoint_is_current(&state, &context.identity, &context.vfs);
            if state.directory_sizes.finish(&key, run_id) {
                emit_error(
                    &window,
                    &key,
                    run_id,
                    request.client_token,
                    &parent_display,
                    if endpoint_current {
                        error
                    } else {
                        AppError::new(
                            "plan_drift",
                            "Die Remote-Sitzung wechselte während der Ordnerauswahl",
                        )
                    },
                );
            }
            return;
        }
    };

    let state = app.state::<AppState>();
    if !endpoint_is_current(&state, &context.identity, &context.vfs) {
        if state.directory_sizes.finish(&key, run_id) {
            emit_error(
                &window,
                &key,
                run_id,
                request.client_token,
                &parent_display,
                AppError::new(
                    "plan_drift",
                    "Die Remote-Sitzung wechselte während der Ordnerauswahl",
                ),
            );
        }
        return;
    }
    let names_by_path: HashMap<_, _> = selected
        .iter()
        .map(|(name, path)| (path.clone(), name.clone()))
        .collect();
    let paths: Vec<_> = selected.into_iter().map(|(_, path)| path).collect();
    let mode = if request.recalculate {
        DirectorySizeMode::Recalculate
    } else {
        DirectorySizeMode::UseCache
    };
    let mut generation_drift = false;
    let mut on_result = |result: &DirectorySizeResult| {
        let state = app.state::<AppState>();
        if !state.directory_sizes.is_current(&key, run_id) {
            return;
        }
        if !endpoint_is_current(&state, &context.identity, &context.vfs) {
            generation_drift = true;
            cancel.cancel();
            return;
        }
        let Some(name) = names_by_path.get(&result.path) else {
            return;
        };
        let _ = crate::emit_to_window(
            &window,
            RESULT_EVENT,
            DirectorySizeResultEvent {
                side: key.side.clone(),
                run_id,
                client_token: request.client_token,
                parent: parent_display.clone(),
                name: name.clone(),
                result: result.clone(),
            },
        );
    };
    let measured = if let Some(sftp) = &context.sftp {
        sftp.measure_interactive_directory_sizes(
            &context.parent,
            &request.names,
            &cancel,
            &mut on_result,
        )
        .map_err(AppError::from)
    } else {
        Ok(session.measure(&paths, mode, &cancel, &mut on_result))
    };
    let stats = match measured {
        Ok(stats) => stats,
        Err(error) => {
            let state = app.state::<AppState>();
            if state.directory_sizes.finish(&key, run_id) {
                emit_error(
                    &window,
                    &key,
                    run_id,
                    request.client_token,
                    &parent_display,
                    error,
                );
            }
            return;
        }
    };
    let state = app.state::<AppState>();
    if !state.directory_sizes.finish(&key, run_id) {
        return;
    }
    if generation_drift || !endpoint_is_current(&state, &context.identity, &context.vfs) {
        emit_error(
            &window,
            &key,
            run_id,
            request.client_token,
            &parent_display,
            AppError::new(
                "plan_drift",
                "Die Remote-Sitzung wechselte während der Größenberechnung",
            ),
        );
        return;
    }
    let _ = crate::emit_to_window(
        &window,
        DONE_EVENT,
        DirectorySizeDoneEvent {
            side: key.side,
            run_id,
            client_token: request.client_token,
            parent: parent_display,
            stats,
        },
    );
}

#[tauri::command]
pub(crate) fn start_directory_sizes(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<AppState>,
    request: DirectorySizeRequest,
) -> Result<DirectorySizeStarted, AppError> {
    validate_side(&request.side)?;
    let context = resolve_context(&app, &state, &request)?;
    let key = PaneKey {
        window_label: window.label().to_string(),
        side: request.side.clone(),
    };
    let (run_id, session, cancel) = state.directory_sizes.start(
        key.clone(),
        context.identity.clone(),
        Arc::clone(&context.vfs),
    );
    let thread_key = key.clone();
    let spawn = std::thread::Builder::new()
        .name(format!("directory-size-{}-{run_id}", request.side))
        .spawn(move || {
            run_measurement(
                window, thread_key, request, context, run_id, session, cancel,
            )
        });
    if let Err(error) = spawn {
        state.directory_sizes.finish(&key, run_id);
        return Err(AppError::new(
            "io",
            format!("Ordnergrößen-Thread konnte nicht gestartet werden: {error}"),
        ));
    }
    Ok(DirectorySizeStarted { run_id })
}

#[tauri::command]
pub(crate) fn cancel_directory_sizes(
    window: tauri::WebviewWindow,
    state: State<AppState>,
    side: String,
) -> Result<(), AppError> {
    validate_side(&side)?;
    state.directory_sizes.cancel(&PaneKey {
        window_label: window.label().to_string(),
        side,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use doppeldecker_core::vfs::ProofListEntry;
    use doppeldecker_core::Entry;

    fn directory(name: &str) -> ProofListEntry {
        ProofListEntry {
            raw_name: name.as_bytes().to_vec(),
            entry: Entry {
                name: name.to_string(),
                is_dir: true,
                is_file: false,
                is_symlink: false,
                size: 0,
                mtime: Some(0),
                created: None,
                extension: None,
                owner: None,
                group: None,
                mode: None,
                device: None,
                inode: None,
                hard_links: None,
            },
            safely_decoded: true,
            provider_token: None,
        }
    }

    #[test]
    fn request_akzeptiert_verschachtelte_frontend_feldnamen() {
        let request: DirectorySizeRequest = serde_json::from_value(serde_json::json!({
            "side": "right",
            "endpoint": { "provider": "sftp", "profileId": "p1", "root": "/daten" },
            "names": ["Fotos"],
            "sessionGeneration": 7,
            "recalculate": false,
            "clientToken": 3
        }))
        .unwrap();

        assert_eq!(request.side, "right");
        assert_eq!(request.session_generation, Some(7));
        assert_eq!(request.client_token, 3);
    }

    #[test]
    fn auswahl_nimmt_nur_frisch_belegte_echte_ordner() {
        let listing = CompleteDirectoryListing {
            entries: vec![directory("B"), directory("A")],
            provider_token: None,
            complete: true,
        };
        let selected = select_requested_folders(
            Path::new("/daten"),
            &["B".to_string(), "A".to_string()],
            listing,
        )
        .unwrap();

        assert_eq!(
            selected,
            vec![
                ("A".to_string(), PathBuf::from("/daten/A")),
                ("B".to_string(), PathBuf::from("/daten/B")),
            ]
        );
    }

    #[test]
    fn auswahl_lehnt_pfadbestandteile_und_unsichere_namen_ab() {
        let mut unsafe_entry = directory("Fotos");
        unsafe_entry.safely_decoded = false;
        let listing = CompleteDirectoryListing {
            entries: vec![unsafe_entry],
            provider_token: None,
            complete: true,
        };
        assert!(select_requested_folders(
            Path::new("/daten"),
            &["../Fotos".to_string()],
            listing.clone(),
        )
        .is_err());
        assert!(
            select_requested_folders(Path::new("/daten"), &["Fotos".to_string()], listing,)
                .is_err()
        );
    }

    #[test]
    fn neue_provider_verbindung_verwirft_cache_auch_bei_gleicher_generation() {
        let runtime = DirectorySizeRuntime::default();
        let key = PaneKey {
            window_label: "fenster-1".to_string(),
            side: "left".to_string(),
        };
        let identity = EndpointIdentity::Sftp {
            profile_id: "profil-1".to_string(),
            generation: 1,
        };
        let first_vfs: Arc<dyn Vfs> = Arc::new(LocalVfs::new());
        let second_vfs: Arc<dyn Vfs> = Arc::new(LocalVfs::new());

        let (_, first_session, _) =
            runtime.start(key.clone(), identity.clone(), Arc::clone(&first_vfs));
        let (_, reused_session, _) = runtime.start(key.clone(), identity.clone(), first_vfs);
        assert!(Arc::ptr_eq(&first_session, &reused_session));

        let (_, replaced_session, _) = runtime.start(key, identity, second_vfs);
        assert!(!Arc::ptr_eq(&first_session, &replaced_session));
    }

    #[test]
    fn neue_provider_verbindung_ist_auch_bei_gleicher_generation_nicht_aktuell() {
        let first = Arc::new(LocalVfs::new());
        let replacement = Arc::new(LocalVfs::new());
        let held_vfs: Arc<dyn Vfs> = first.clone();

        assert!(provider_connection_is_current(
            Some(first.clone()),
            &held_vfs,
            1,
            |_| 1,
        ));
        assert!(!provider_connection_is_current(
            Some(replacement),
            &held_vfs,
            1,
            |_| 1,
        ));
        assert!(!provider_connection_is_current(
            Some(first),
            &held_vfs,
            1,
            |_| 2,
        ));
    }
}
