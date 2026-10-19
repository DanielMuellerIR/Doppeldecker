//! Tauri-Adapter für lokale interaktive Dateioperationen.

use std::path::{Path, PathBuf};

use doppeldecker_core::{
    create_dir, expand_home_path, find_recursive_conflicts, recover_trash_journals, trash_items,
    FileConflictProof, FileQueueKind, LocalVfs, ProviderDeleteCapabilities,
    ProviderFileCapabilities, ProviderKind, QueueOperation, RemoteDeleteMode, RemoteDeletePlan,
    TrashStats,
};
use tauri::{Manager, State, WebviewWindow};

use crate::bidirectional_commands::{
    resolve_endpoint, resolve_validated_registry_pair, validate_registry_profile,
    BidirectionalEndpointRequest,
};
use crate::mutex::MutexExt;
use crate::{
    app_config_dir, emit_queue_snapshot, enqueue_operation_with_readback, sandbox_root,
    start_queue_worker, with_queue_extension, with_reserved_operation, AppError, AppState,
};

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderFileRequest {
    kind: String,
    source: BidirectionalEndpointRequest,
    target: BidirectionalEndpointRequest,
    items: Vec<String>,
    destination: String,
}

/// Tauri-Payload der Dateifähigkeiten in der Schreibweise, die das Frontend
/// tatsächlich liest (camelCase). Die Core-Struktur serialisiert snake_case —
/// verschachtelte Felder benennt das äußere `rename_all` NICHT mit um; genau
/// dieser stille Namensbruch hatte `copyNew`/`removeSource` im echten Backend
/// als `undefined` ankommen lassen und Remote-F5/F6 in der GUI pauschal
/// gesperrt. Der Regressionstest unten hält die Schreibweise fest.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderFileCapabilitiesPayload {
    provider: doppeldecker_core::ProviderKind,
    copy_new: bool,
    copy_new_best_effort: bool,
    replace: bool,
    replace_best_effort: bool,
    remove_source: bool,
    remove_source_best_effort: bool,
    rename_noreplace: bool,
    limitations: Vec<String>,
    blockers: Vec<String>,
}

impl From<ProviderFileCapabilities> for ProviderFileCapabilitiesPayload {
    fn from(value: ProviderFileCapabilities) -> Self {
        Self {
            provider: value.provider,
            copy_new: value.copy_new,
            copy_new_best_effort: value.copy_new_best_effort,
            replace: value.replace,
            replace_best_effort: value.replace_best_effort,
            remove_source: value.remove_source,
            remove_source_best_effort: value.remove_source_best_effort,
            rename_noreplace: value.rename_noreplace,
            limitations: value.limitations,
            blockers: value.blockers,
        }
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderFilePreview {
    source_read_limitations: Vec<String>,
    source_tree: Option<doppeldecker_core::vfs::InteractiveSourceTree>,
    conflicts: Vec<String>,
    conflict_details: Vec<doppeldecker_core::fileops::FileConflictView>,
    keep_both_preparation_id: Option<String>,
    conflict_preparation_id: Option<String>,
    keep_both_names: Vec<String>,
    keep_both_blocker: Option<String>,
    target_capabilities: ProviderFileCapabilitiesPayload,
    source_capabilities: Option<ProviderFileCapabilitiesPayload>,
    skip_only: bool,
    source_removal_required: bool,
    requires_conflict_choice: bool,
    #[serde(skip)]
    conflict_metadata_only: bool,
}

/// Eine Vorschau pro Fenster, ohne persistierbare Freigabe. Nur die ID geht
/// durch die UI zurück; ausführbare Aliasziele stammen aus diesem Backendstand.
pub(crate) struct FileConflictChoices {
    metadata_only: bool,
    id: String,
    key: String,
    targets: Vec<doppeldecker_core::fileops::FileKeepBothTarget>,
    created: std::time::Instant,
    source_proofs: Vec<FileConflictProof>,
    target_proofs: Vec<FileConflictProof>,
    source_tree: Option<doppeldecker_core::vfs::InteractiveSourceTree>,
}

#[derive(Default)]
pub(crate) struct FileConflictChoiceCache {
    generation: u64,
    choice: Option<FileConflictChoices>,
}

fn begin_file_choices(window: &crate::queue_runtime::WindowState) -> u64 {
    let mut cache = window.file_conflict_choices.lock_recover();
    cache.generation = cache.generation.wrapping_add(1);
    cache.choice = None;
    cache.generation
}

fn publish_file_choices(
    window: &crate::queue_runtime::WindowState,
    generation: u64,
    key: String,
    targets: Vec<doppeldecker_core::fileops::FileKeepBothTarget>,
    preview: &mut ProviderFilePreview,
) -> Result<(), AppError> {
    if preview.conflicts.is_empty() && preview.source_tree.is_none() {
        return Ok(());
    }
    if preview.conflict_metadata_only && !targets.is_empty() {
        return Err(AppError::new(
            "plan_drift",
            "Passive Metadaten dürfen keine Aliasziele binden",
        ));
    }
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| AppError::new("app", error.to_string()))?;
    let id = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    preview.keep_both_names = targets
        .iter()
        .map(|target| {
            let name = Path::new(&target.target)
                .file_name()
                .expect("validiertes Aliasziel");
            doppeldecker_core::conflict_view::display_relative_name(&[name
                .as_encoded_bytes()
                .to_vec()])
        })
        .collect();
    preview.conflict_preparation_id = Some(id.clone());
    if !targets.is_empty() {
        preview.keep_both_preparation_id = Some(id.clone());
        preview.keep_both_blocker = None;
    }
    let mut cache = window.file_conflict_choices.lock_recover();
    // Anfragebeginn und Veröffentlichung teilen dieselbe Schranke. Eine langsamere
    // ältere Antwort darf die inzwischen sichtbare neuere Bindung nicht ersetzen.
    if cache.generation != generation {
        return Err(AppError::new(
            "plan_drift",
            "Eine neuere Konfliktvorschau hat diese Anfrage abgelöst",
        ));
    }
    cache.choice = Some(FileConflictChoices {
        metadata_only: preview.conflict_metadata_only,
        id,
        key,
        targets,
        source_tree: preview.source_tree.clone(),
        source_proofs: preview
            .conflict_details
            .iter()
            .map(|view| view.source_proof.clone())
            .collect(),
        target_proofs: preview
            .conflict_details
            .iter()
            .map(|view| view.target_proof.clone())
            .collect(),
        created: std::time::Instant::now(),
    });
    Ok(())
}

fn bound_file_choices(
    window: &crate::queue_runtime::WindowState,
    key: &str,
    id: Option<&str>,
) -> Result<Vec<doppeldecker_core::fileops::FileKeepBothTarget>, AppError> {
    let Some(id) = id else {
        return Ok(Vec::new());
    };
    let slot = window.file_conflict_choices.lock_recover();
    slot.choice.as_ref().filter(|pending| pending.id == id && pending.key == key && pending.created.elapsed() < std::time::Duration::from_secs(600))
        .map(|pending| pending.targets.clone())
        .ok_or_else(|| AppError::new("plan_drift", "Die Konfliktvorschau ist abgelaufen oder gehört zu einer anderen Auswahl; bitte erneut öffnen"))
}

fn verify_file_choice_snapshot(
    window: &crate::queue_runtime::WindowState,
    key: &str,
    id: Option<&str>,
    src: &dyn doppeldecker_core::Vfs,
    dst: &dyn doppeldecker_core::Vfs,
    selected_metadata_only: bool,
) -> Result<(), AppError> {
    let fresh_tree = src.interactive_source_tree().map_err(AppError::from)?;
    let Some(id) = id else {
        return if fresh_tree.is_some() {
            Err(AppError::new(
                "plan_drift",
                "Quellbaum benötigt die zugehörige Backendvorschau",
            ))
        } else {
            Ok(())
        };
    };
    let (mut source_proofs, mut target_proofs, metadata_only) = {
        let slot = window.file_conflict_choices.lock_recover();
        let pending = slot
            .choice
            .as_ref()
            .filter(|pending| {
                pending.id == id
                    && pending.key == key
                    && pending.created.elapsed() < std::time::Duration::from_secs(600)
            })
            .ok_or_else(|| {
                AppError::new(
                    "plan_drift",
                    "Die Konfliktvorschau ist abgelaufen; bitte erneut öffnen",
                )
            })?;
        if pending.source_tree != fresh_tree {
            return Err(AppError::new("plan_drift", "Quellbaum oder Auslassungen änderten sich seit der Vorschau; bitte erneut vorbereiten"));
        }
        (
            pending.source_proofs.clone(),
            pending.target_proofs.clone(),
            pending.metadata_only || selected_metadata_only,
        )
    };
    if metadata_only {
        // Eine ausdrücklich gewählte Skip-Wirkung darf alte Inhaltsbelege nur
        // hinsichtlich ihrer Metadaten prüfen. Vor Ersetzen/neuen Namen wird im
        // selben Request anschließend wieder vollständig mit Inhalt vorbereitet.
        for proof in source_proofs.iter_mut().chain(target_proofs.iter_mut()) {
            proof.content_hash = None;
        }
        doppeldecker_core::fileops::verify_file_metadata_proofs(src, &source_proofs)
            .map_err(AppError::from)?;
        doppeldecker_core::fileops::verify_file_metadata_proofs(dst, &target_proofs)
            .map_err(AppError::from)?;
    } else {
        doppeldecker_core::fileops::verify_file_source_proofs(src, &source_proofs)
            .map_err(AppError::from)?;
        doppeldecker_core::fileops::verify_file_conflict_proofs(dst, &target_proofs)
            .map_err(AppError::from)?;
    }
    Ok(())
}

fn bound_replacement_proofs(
    window: &crate::queue_runtime::WindowState,
    key: &str,
    id: Option<&str>,
) -> Result<Vec<FileConflictProof>, AppError> {
    let slot = window.file_conflict_choices.lock_recover();
    let pending = slot
        .choice
        .as_ref()
        .filter(|pending| {
            Some(pending.id.as_str()) == id
                && pending.key == key
                && pending.created.elapsed() < std::time::Duration::from_secs(600)
        })
        .ok_or_else(|| {
            AppError::new(
                "plan_drift",
                "Ersetzen benötigt die aktuell angezeigte Konfliktvorschau",
            )
        })?;
    if pending.metadata_only {
        return Err(AppError::new(
            "plan_drift",
            "Ersetzen benötigt vollständig vorbereitete Inhaltsbelege",
        ));
    }
    Ok(pending.target_proofs.clone())
}

fn local_choice_key(kind: FileQueueKind, items: &[String], destination: &str) -> String {
    serde_json::to_string(&(kind, items, destination)).expect("String-Auswahl ist serialisierbar")
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderDeleteRequest {
    endpoint: BidirectionalEndpointRequest,
    items: Vec<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderDeletePreview {
    capabilities: ProviderDeleteCapabilities,
    mode: RemoteDeleteMode,
    plan: RemoteDeletePlan,
    confirmation: Option<String>,
}

fn permanent_delete_confirmation(
    request: &BidirectionalEndpointRequest,
    plan: &RemoteDeletePlan,
) -> String {
    let (provider, profile) = match request {
        BidirectionalEndpointRequest::Ftps { profile_id, .. } => ("FTPS", profile_id.as_str()),
        BidirectionalEndpointRequest::Webdav { profile_id, .. } => ("WEBDAV", profile_id.as_str()),
        BidirectionalEndpointRequest::Sftp { profile_id, .. } => ("SFTP", profile_id.as_str()),
        BidirectionalEndpointRequest::Smb { profile_id, .. } => ("SMB", profile_id.as_str()),
        BidirectionalEndpointRequest::Local { .. } => ("LOCAL", "-"),
    };
    format!(
        "DAUERHAFT LÖSCHEN {provider} {profile} {} {} {}",
        plan.root,
        plan.items.len(),
        plan.entries
    )
}

fn prepare_provider_delete_request(
    app: &tauri::AppHandle,
    state: &AppState,
    request: &ProviderDeleteRequest,
) -> Result<
    (
        Box<doppeldecker_core::BidirectionalQueueEndpoint>,
        ProviderDeletePreview,
        Vec<doppeldecker_core::RemoteProfileBinding>,
    ),
    AppError,
> {
    if request.endpoint.provider_kind() == ProviderKind::Local {
        return Err(AppError::new(
            "invalid_input",
            "Lokale Einträge verwenden weiterhin ausschließlich den Systempapierkorb",
        ));
    }
    let items = request.items.iter().map(PathBuf::from).collect::<Vec<_>>();
    doppeldecker_core::validate_remote_delete_request(Path::new(request.endpoint.root()), &items)
        .map_err(AppError::from)?;
    let (endpoint, queue_endpoint, profile_bindings) =
        resolve_endpoint(app, state, &request.endpoint)?;
    let capabilities = endpoint
        .probe_delete_capabilities()
        .map_err(AppError::from)?;
    let mode = if capabilities.quarantine {
        RemoteDeleteMode::Quarantine
    } else if capabilities.permanent {
        RemoteDeleteMode::Permanent
    } else {
        return Err(AppError::new(
            "unsupported",
            capabilities.blockers.join("; "),
        ));
    };
    let (vfs, root) = endpoint.file_context().map_err(AppError::from)?;
    let plan = doppeldecker_core::plan_remote_delete(vfs.as_ref(), &root, &items)
        .map_err(AppError::from)?;
    crate::queue_runtime::ensure_profile_bindings_current(app, &profile_bindings)?;
    let confirmation = (mode == RemoteDeleteMode::Permanent)
        .then(|| permanent_delete_confirmation(&request.endpoint, &plan));
    Ok((
        queue_endpoint,
        ProviderDeletePreview {
            capabilities,
            mode,
            plan,
            confirmation,
        },
        profile_bindings,
    ))
}

#[tauri::command]
pub(crate) async fn check_provider_delete(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: ProviderDeleteRequest,
) -> Result<ProviderDeletePreview, AppError> {
    let worker_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker_app.state::<AppState>();
        // Die Capability-Probe legt künstliche Einträge unter dem Pane-Root an und
        // entfernt sie wieder — eine echte Providermutation. Innerhalb dieses Fensters
        // darf sie deshalb nicht mit dessen Queue-/Sync-Lauf überlappen: erst die
        // Single-Flight-Schranke reservieren, dann proben. Ein paralleles anderes
        // Fenster bleibt erlaubt; sieht dessen frischer Scan den kurzlebigen Zufallsnamen,
        // führt das bei der Driftprüfung zu einem sichtbaren Fehler statt zu Überschreiben.
        let label = window.label();
        let window_state = state.window(label);
        let (_, preview, _) = with_reserved_operation(&app, label, &window_state, || {
            prepare_provider_delete_request(&app, &state, &request)
        })?;
        Ok(preview)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnqueueProviderDeleteRequest {
    endpoint: BidirectionalEndpointRequest,
    items: Vec<String>,
    mode: RemoteDeleteMode,
    expected_plan: RemoteDeletePlan,
    confirmation: Option<String>,
}

#[tauri::command]
pub(crate) async fn enqueue_provider_delete(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: EnqueueProviderDeleteRequest,
) -> Result<String, AppError> {
    let worker_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker_app.state::<AppState>();
        let operation = ProviderDeleteRequest {
            endpoint: request.endpoint,
            items: request.items,
        };
        // Auch der Enqueue-Weg probt mutierend (siehe check_provider_delete) und
        // läuft deshalb vollständig unter der Single-Flight-Schranke. Den Worker
        // startet `with_reserved_operation` nach der Freigabe selbst.
        let label = window.label();
        let window_state = state.window(label);
        with_reserved_operation(&app, label, &window_state, || {
            let (endpoint, preview, profile_bindings) =
                prepare_provider_delete_request(&app, &state, &operation)?;
            if preview.mode != request.mode || preview.plan != request.expected_plan {
                return Err(AppError::new(
                    "plan_drift",
                    "Remote-Löschplan oder Capability änderte sich seit der Vorschau",
                ));
            }
            if preview.mode == RemoteDeleteMode::Permanent
                && request.confirmation.as_deref() != preview.confirmation.as_deref()
            {
                return Err(AppError::new(
                    "confirmation_mismatch",
                    "Bestätigung für das endgültige Remote-Löschen stimmt nicht exakt überein",
                ));
            }
            let (queue, entry) = with_queue_extension(&state, &window_state, || {
                let operation = QueueOperation::ProviderDelete {
                    endpoint,
                    mode: preview.mode,
                    expected_plan: Box::new(preview.plan),
                };
                crate::queue_runtime::enqueue_operation_with_profile_bindings(
                    &app,
                    &window_state,
                    label,
                    &operation,
                    &profile_bindings,
                )
            })?;
            emit_queue_snapshot(&app, label, &queue);
            Ok(entry.operation_id)
        })
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

fn file_kind(value: &str) -> Result<FileQueueKind, AppError> {
    match value {
        "copy" => Ok(FileQueueKind::Copy),
        "move" => Ok(FileQueueKind::Move),
        _ => Err(AppError::new("invalid_input", "Unbekannte Dateioperation")),
    }
}

struct PreparedProviderFileRequest {
    kind: FileQueueKind,
    source: Box<doppeldecker_core::BidirectionalQueueEndpoint>,
    target: Box<doppeldecker_core::BidirectionalQueueEndpoint>,
    preview: ProviderFilePreview,
    keep_both_targets: Vec<doppeldecker_core::fileops::FileKeepBothTarget>,
    same_namespace_move: bool,
    profile_bindings: Vec<doppeldecker_core::RemoteProfileBinding>,
}

/// Fähigkeiten des No-Replace-Rename-Schnellwegs innerhalb desselben Namespace.
///
/// Der Schnellweg verschiebt ausschließlich exklusiv: Er legt neu an und entfernt
/// die Quelle, ersetzt aber nie. Quelle und Ziel beschreiben denselben Satz; nur
/// die Zielseite weist das Ersetzen zusätzlich als gesperrt aus.
fn same_namespace_rename_capabilities(
    provider: ProviderKind,
    blockers: Vec<String>,
) -> ProviderFileCapabilities {
    ProviderFileCapabilities {
        provider,
        copy_new: true,
        copy_new_best_effort: false,
        replace: false,
        replace_best_effort: false,
        remove_source: true,
        remove_source_best_effort: false,
        rename_noreplace: true,
        limitations: Vec::new(),
        blockers,
    }
}

fn file_endpoint_label(endpoint: &doppeldecker_core::BidirectionalQueueEndpoint) -> String {
    use doppeldecker_core::BidirectionalQueueEndpoint as Endpoint;
    match endpoint {
        Endpoint::Local { root } => format!("Lokal · {root}"),
        Endpoint::Sftp { connection, root } => {
            format!("SFTP · {}:{} · {root}", connection.host, connection.port)
        }
        Endpoint::Ftps { connection, root } => format!(
            "{} · {}:{} · {root}",
            if connection.transport_security == doppeldecker_core::FtpTransportSecurity::Plaintext {
                "FTP"
            } else {
                "FTPS"
            },
            connection.host,
            connection.port
        ),
        Endpoint::WebDav { connection, root } => format!("WebDAV · {} · {root}", connection.origin),
        Endpoint::Smb { connection, root } => format!(
            "SMB · {}:{}/{} · {root}",
            connection.host, connection.port, connection.share
        ),
    }
}

fn file_preparation_not_cancelled(cancel: &doppeldecker_core::Cancel) -> Result<(), AppError> {
    if cancel.is_cancelled() {
        Err(AppError::new(
            "interrupted",
            "Dateivorbereitung wurde abgebrochen",
        ))
    } else {
        Ok(())
    }
}

struct FilePreparationConfirmation<'a> {
    window: &'a crate::queue_runtime::WindowState,
    key: &'a str,
    id: Option<&'a str>,
    overwrite: bool,
    skip_conflicts: bool,
    prepare_aliases: bool,
}

fn prepare_provider_file_request(
    app: &tauri::AppHandle,
    window: &WebviewWindow,
    state: &AppState,
    request: &ProviderFileRequest,
    cancel: &doppeldecker_core::Cancel,
    confirmation: Option<FilePreparationConfirmation<'_>>,
) -> Result<PreparedProviderFileRequest, AppError> {
    file_preparation_not_cancelled(cancel)?;
    let kind = file_kind(&request.kind)?;
    let overwrite = confirmation.as_ref().is_some_and(|value| value.overwrite);
    let skip_conflicts_confirmed = confirmation
        .as_ref()
        .is_some_and(|value| value.skip_conflicts);
    crate::bidirectional_commands::validate_endpoint_request_root(&request.source)?;
    crate::bidirectional_commands::validate_endpoint_request_root(&request.target)?;
    let destination = Path::new(&request.destination);
    if destination != Path::new(request.target.root()) {
        return Err(AppError::new(
            "invalid_input",
            "Zielordner stimmt nicht mit dem gebundenen Ziel-Pane überein",
        ));
    }
    let requested_items = request.items.iter().map(PathBuf::from).collect::<Vec<_>>();
    if request.source.provider_kind() == ProviderKind::Sftp {
        if request.target.provider_kind() != ProviderKind::Local || kind != FileQueueKind::Copy {
            return Err(AppError::new(
                "unsupported",
                "SFTP-Inhalte erlauben nur additive Downloads in ein lokales Ziel",
            ));
        }
        doppeldecker_core::sftp::validate_interactive_content_selection(
            Path::new(request.source.root()),
            &requested_items,
        )
        .map_err(AppError::from)?;
    }
    doppeldecker_core::validate_file_operation_paths(&requested_items, destination)
        .map_err(AppError::from)?;
    let requested_items = if request.source.provider_kind() == ProviderKind::Local {
        let (_, bound_items) = doppeldecker_core::bind_local_file_operation_scope(
            Path::new(request.source.root()),
            &requested_items,
        )
        .map_err(AppError::from)?;
        bound_items
    } else {
        let (_, bound_items) = doppeldecker_core::validate_file_operation_scope(
            Path::new(request.source.root()),
            &requested_items,
        )
        .map_err(AppError::from)?;
        bound_items
    };
    // Beide Profilrevisionen kommen aus den atomaren Profildateien und müssen
    // feststehen, bevor auch nur die Quelle Cache, Secret oder Netzwerk berührt.
    doppeldecker_core::fileops::guard_local_transfer_roots(
        (request.source.provider_kind() == ProviderKind::Local)
            .then(|| Path::new(request.source.root())),
        (request.target.provider_kind() == ProviderKind::Local)
            .then(|| Path::new(request.target.root())),
        kind == FileQueueKind::Move,
        &sandbox_root()?,
    )
    .map_err(AppError::from)?;
    let ((source, source_queue, source_profiles), (target, target_queue, target_profiles)) =
        resolve_validated_registry_pair(
            &request.source,
            &request.target,
            |endpoint| validate_registry_profile(app, endpoint),
            |endpoint| resolve_endpoint(app, state, endpoint),
        )?;
    let profile_bindings =
        crate::queue_runtime::merge_profile_bindings([source_profiles, target_profiles]);
    let window_state = state.window(window.label());
    let queue = crate::operation_queue(app, &window_state, window.label())?;
    let source = queue
        .bind_provider_preparation_with_cancel(&source, cancel)
        .map_err(AppError::from)?;
    let target = queue
        .bind_provider_preparation_with_cancel(&target, cancel)
        .map_err(AppError::from)?;
    source
        .validate_interactive_file_transfer(&target, kind == FileQueueKind::Move, overwrite)
        .map_err(AppError::from)?;
    let doppeldecker_core::InteractiveFileContexts {
        source_vfs,
        target_vfs,
        target_capabilities: target_capabilities_override,
    } = source
        .interactive_file_contexts(
            &target,
            &requested_items,
            kind == FileQueueKind::Move,
            false,
            cancel,
        )
        .map_err(AppError::from)?;
    let source_tree = source_vfs
        .interactive_source_tree()
        .map_err(AppError::from)?;
    let item_paths = requested_items;
    if item_paths
        .iter()
        .any(|path| !path.starts_with(source.root()) || path == source.root())
    {
        return Err(AppError::new(
            "invalid_input",
            "Auswahl verlässt den gebundenen Quell-Pane-Root",
        ));
    }
    if let Some(value) = confirmation.as_ref() {
        verify_file_choice_snapshot(
            value.window,
            value.key,
            value.id,
            source_vfs.as_ref(),
            target_vfs.as_ref(),
            skip_conflicts_confirmed,
        )?;
    }
    let conflicts = find_recursive_conflicts(
        source_vfs.as_ref(),
        target_vfs.as_ref(),
        Path::new(&request.destination),
        &item_paths,
    )
    .map_err(AppError::from)?;
    let same_runtime_namespace = source
        .shares_runtime_namespace(&target)
        .map_err(AppError::from)?;
    let shared_session =
        same_runtime_namespace && source_queue.shares_connection_definition(&target_queue);
    let mut targets_missing = true;
    for path in &item_paths {
        let name = path.file_name().ok_or_else(|| {
            AppError::new("invalid_input", "Dateiquelle besitzt keinen sicheren Namen")
        })?;
        match target_vfs.stat(&destination.join(name)) {
            Ok(_) => targets_missing = false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(AppError::from(error)),
        }
    }
    file_preparation_not_cancelled(cancel)?;
    let conflict_metadata_only = (source.kind() == ProviderKind::Ftps
        || target.kind() == ProviderKind::Ftps)
        && (confirmation.is_none() || skip_conflicts_confirmed);
    let conflict_views = if conflict_metadata_only {
        doppeldecker_core::fileops::file_conflict_metadata_views
    } else {
        doppeldecker_core::fileops::file_conflict_views
    };
    let mut conflict_details = conflict_views(
        source_vfs.as_ref(),
        target_vfs.as_ref(),
        destination,
        &item_paths,
        &conflicts,
    )
    .map_err(AppError::from)?;
    for conflict in &mut conflict_details {
        conflict.source.endpoint = file_endpoint_label(&source_queue);
        conflict.target.endpoint = file_endpoint_label(&target_queue);
    }
    let prepared_target = source
        .probe_interactive_file_target(doppeldecker_core::ProviderFileTargetProbe {
            target: &target,
            items: &item_paths,
            kind,
            overwrite,
            skip_conflicts: skip_conflicts_confirmed,
            inspect_conflicts: confirmation.is_none(),
            expected_conflicts: Some(&conflicts),
            shared_session,
            targets_missing,
            capabilities_override: target_capabilities_override,
        })
        .map_err(AppError::from)?;
    file_preparation_not_cancelled(cancel)?;
    let requires_conflict_choice = prepared_target.choice_required;
    let skip_only = prepared_target.skip_only;
    let source_removal_required = prepared_target.source_removal_required;
    let same_namespace_move = prepared_target.rename_only;
    let mut target_capabilities = prepared_target.capabilities.unwrap_or_else(|| {
        same_namespace_rename_capabilities(
            target.kind(),
            vec!["Ersetzen ist für den No-Replace-Rename-Schnellweg nicht erlaubt".into()],
        )
    });
    if source.kind() == ProviderKind::Sftp {
        target_capabilities.replace = false;
        target_capabilities.replace_best_effort = false;
        target_capabilities.blockers.push(
            "SFTP-Inhaltsdownloads sind ausschließlich additiv; vorhandene Ziele bleiben erhalten"
                .into(),
        );
    }
    let target_transfer_available =
        target_capabilities.copy_new || target_capabilities.copy_new_best_effort;
    let selected_replacement_available =
        !overwrite || target_capabilities.replace || target_capabilities.replace_best_effort;
    let source_capabilities = if !source_removal_required
        || skip_only
        || requires_conflict_choice
        || !target_transfer_available
        || !selected_replacement_available
    {
        None
    } else if same_namespace_move {
        Some(same_namespace_rename_capabilities(
            source.kind(),
            Vec::new(),
        ))
    } else if kind == FileQueueKind::Move {
        Some(
            source
                .probe_file_source_removal_capabilities()
                .map_err(AppError::from)?,
        )
    } else {
        None
    };
    file_preparation_not_cancelled(cancel)?;
    let keep_both_targets = if (confirmation
        .as_ref()
        .is_some_and(|value| !value.prepare_aliases))
        || requires_conflict_choice
    {
        Vec::new()
    } else {
        doppeldecker_core::fileops::prepare_keep_both_targets(
            source_vfs.as_ref(),
            target_vfs.as_ref(),
            destination,
            &item_paths,
            &conflicts,
        )
        .map_err(AppError::from)?
    };
    crate::queue_runtime::ensure_profile_bindings_current(app, &profile_bindings)?;
    file_preparation_not_cancelled(cancel)?;
    Ok(PreparedProviderFileRequest {
        kind,
        source: source_queue,
        target: target_queue,
        preview: ProviderFilePreview {
            source_read_limitations: source
                .interactive_file_read_limitations()
                .into_iter()
                .chain(
                    source_tree
                        .as_ref()
                        .into_iter()
                        .flat_map(|tree| tree.limitations()),
                )
                .collect(),
            source_tree,
            conflict_details,
            keep_both_preparation_id: None,
            conflict_preparation_id: None,
            keep_both_names: Vec::new(),
            keep_both_blocker: Some(
                "Für diese Auswahl ist kein atomarer kollisionsfreier Commit gebunden.".into(),
            ),
            conflicts,
            target_capabilities: target_capabilities.into(),
            source_capabilities: source_capabilities.map(Into::into),
            skip_only,
            source_removal_required,
            requires_conflict_choice,
            conflict_metadata_only,
        },
        keep_both_targets,
        same_namespace_move,
        profile_bindings,
    })
}

fn with_provider_file_preparation<T>(
    app: &tauri::AppHandle,
    window: &WebviewWindow,
    state: &crate::queue_runtime::WindowState,
    notice: Option<&'static str>,
    operation: impl FnOnce(&doppeldecker_core::Cancel) -> Result<T, AppError>,
) -> Result<T, AppError> {
    let Some(notice) = notice else {
        return with_reserved_operation(app, window.label(), state, || {
            operation(&doppeldecker_core::Cancel::new())
        });
    };
    crate::queue_runtime::with_reserved_cancellable_operation(
        app,
        window.label(),
        state,
        |cancel| {
            let _ = crate::emit_to_window(window, "fileop-preparation-started", notice);
            operation(cancel)
        },
        |_| {
            // Wie bei der Dateiöffnung: Reservierung freigeben und Terminalevent
            // senden, bevor ein nächster Queueworker seine Anzeige startet.
            let _ = crate::emit_to_window(window, "fileop-preparation-done", ());
        },
    )?
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderFileChoiceRequest {
    id: String,
    policy: String,
}

#[tauri::command]
pub(crate) async fn check_provider_file_operation(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: ProviderFileRequest,
    choice: Option<ProviderFileChoiceRequest>,
) -> Result<ProviderFilePreview, AppError> {
    let worker_app = app.clone();
    let window_state = app.state::<AppState>().window(window.label());
    if choice
        .as_ref()
        .is_some_and(|value| !["skip", "overwrite", "both"].contains(&value.policy.as_str()))
    {
        return Err(AppError::new(
            "invalid_input",
            "Unbekannte Konfliktentscheidung",
        ));
    }
    let generation = if choice.is_some() {
        window_state.file_conflict_choices.lock_recover().generation
    } else {
        begin_file_choices(&window_state)
    };
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker_app.state::<AppState>();
        // Die erste FTPS-Konfliktvorschau bleibt passiv. Erst die am Fenster und
        // Snapshot gebundene Entscheidung erlaubt ihre benötigten Teilproben.
        // Auch diese Phase hält dieselbe Single-Flight-/Abbruchschranke; andere
        // Fenster behalten ihre eigene Queue und Vorbereitung.
        let mut prepared = with_provider_file_preparation(
            &app,
            &window,
            &window_state,
            doppeldecker_core::interactive_file_preparation_notice(
                request.source.provider_kind(),
                request.target.provider_kind(),
                request.kind == "move",
            ),
            |cancel| {
                let key = serde_json::to_string(&request)
                    .map_err(|error| AppError::new("app", error.to_string()))?;
                let confirmation = choice.as_ref().map(|value| FilePreparationConfirmation {
                    window: &window_state,
                    key: &key,
                    id: Some(&value.id),
                    overwrite: value.policy == "overwrite",
                    skip_conflicts: value.policy == "skip",
                    prepare_aliases: value.policy == "both",
                });
                prepare_provider_file_request(&app, &window, &state, &request, cancel, confirmation)
            },
        )?;
        publish_file_choices(
            &window_state,
            generation,
            serde_json::to_string(&request)
                .map_err(|error| AppError::new("app", error.to_string()))?,
            prepared.keep_both_targets,
            &mut prepared.preview,
        )?;
        Ok(prepared.preview)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnqueueProviderFileRequest {
    #[serde(flatten)]
    operation: ProviderFileRequest,
    overwrite: bool,
    expected_conflicts: Vec<String>,
    #[serde(default)]
    keep_both_preparation_id: Option<String>,
    #[serde(default)]
    conflict_preparation_id: Option<String>,
    #[serde(default)]
    copy_new_best_effort_expected: Option<bool>,
    #[serde(default)]
    replace_best_effort_expected: Option<bool>,
    #[serde(default)]
    remove_source_best_effort_expected: Option<bool>,
}

fn provider_execution_class(assured: bool, best_effort: bool) -> Option<bool> {
    if assured {
        Some(false)
    } else if best_effort {
        Some(true)
    } else {
        None
    }
}

/// Bindet ausschließlich die vom Dialog tatsächlich angezeigten Ausführungsklassen.
/// Ein frischer Probe-Befund darf insbesondere nicht selbst `Some(true)` ins Journal
/// schreiben: Das wäre eine Bestätigung, die der Nutzer nie gesehen hat.
fn validate_provider_file_execution_classes(
    request: &EnqueueProviderFileRequest,
    preview: &ProviderFilePreview,
) -> Result<(), AppError> {
    if preview.skip_only {
        if request.overwrite || request.keep_both_preparation_id.is_some() {
            return Err(AppError::new(
                "invalid_input",
                "Vollständiges Überspringen darf weder ersetzen noch neue Aliasziele binden",
            ));
        }
        return Ok(());
    }
    let fresh_copy = provider_execution_class(
        preview.target_capabilities.copy_new,
        preview.target_capabilities.copy_new_best_effort,
    );
    let fresh_replace = if request.overwrite {
        provider_execution_class(
            preview.target_capabilities.replace,
            preview.target_capabilities.replace_best_effort,
        )
    } else {
        Some(false)
    };
    if request.copy_new_best_effort_expected.is_none()
        || request.replace_best_effort_expected.is_none()
        || request.copy_new_best_effort_expected != fresh_copy
        || request.replace_best_effort_expected != fresh_replace
    {
        return Err(AppError::new(
            "plan_drift",
            "Die bestätigte Ausführungsklasse für Neuanlegen oder Ersetzen stimmt nicht mehr mit der frischen Vorschau überein",
        ));
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn enqueue_provider_file_operation(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: EnqueueProviderFileRequest,
) -> Result<String, AppError> {
    let worker_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker_app.state::<AppState>();
        // Wie check_provider_file_operation: Die mutierenden Capability-Proben und
        // das Einreihen laufen gemeinsam unter der Single-Flight-Schranke; den
        // Worker startet `with_reserved_operation` nach der Freigabe selbst.
        let label = window.label();
        let window_state = state.window(label);
        with_provider_file_preparation(&app, &window, &window_state, doppeldecker_core::interactive_file_preparation_notice(request.operation.source.provider_kind(), request.operation.target.provider_kind(), request.operation.kind == "move"), |cancel| {
            let key = serde_json::to_string(&request.operation).map_err(|error| AppError::new("app", error.to_string()))?;
            let keep_both_targets = bound_file_choices(&window_state, &key, request.keep_both_preparation_id.as_deref())?;
            if request.overwrite && !keep_both_targets.is_empty() { return Err(AppError::new("invalid_input", "Beide behalten darf nicht überschreiben")); }
            let PreparedProviderFileRequest {
                kind,
                source,
                target,
                preview,
                keep_both_targets: _,
                same_namespace_move,
                profile_bindings,
            } = prepare_provider_file_request(&app, &window, &state, &request.operation, cancel, Some(FilePreparationConfirmation { window: &window_state, key: &key, id: request.conflict_preparation_id.as_deref(), overwrite: request.overwrite, skip_conflicts: !request.overwrite && keep_both_targets.is_empty(), prepare_aliases: false }))?;
            let mut expected = request.expected_conflicts.clone();
            expected.sort();
            if preview.conflicts != expected {
                return Err(AppError::new(
                    "plan_drift",
                    "Die Provider-Zielkonflikte haben sich seit der Bestätigung geändert",
                ));
            }
            validate_provider_file_execution_classes(&request, &preview)?;
            if !preview.skip_only && !preview.target_capabilities.copy_new
                && !preview.target_capabilities.copy_new_best_effort
            {
                return Err(AppError::new(
                    "unsupported",
                    preview.target_capabilities.blockers.join("; "),
                ));
            }
            if !preview.skip_only && request.overwrite
                && !preview.target_capabilities.replace
                && !preview.target_capabilities.replace_best_effort
            {
                return Err(AppError::new(
                    "unsupported",
                    "Ersetzen ist für diese Zielverbindung weder mit Quarantäne noch als bestätigtes Best Effort gebunden",
                ));
            }
            let remove_source_best_effort_expected = if kind == FileQueueKind::Move
                && !same_namespace_move && preview.source_removal_required && !preview.skip_only
            {
                let source_capabilities = preview.source_capabilities.as_ref().ok_or_else(|| {
                    AppError::new(
                        "unsupported",
                        "Verschieben ist gesperrt, weil Quellfähigkeiten fehlen",
                    )
                })?;
                if !source_capabilities.remove_source && !source_capabilities.remove_source_best_effort
                {
                    return Err(AppError::new(
                        "unsupported",
                        "Verschieben ist gesperrt, weil die Quellentfernung nicht gebunden ist",
                    ));
                }
                let expected =
                    !source_capabilities.remove_source && source_capabilities.remove_source_best_effort;
                if request.remove_source_best_effort_expected != Some(expected) {
                    return Err(AppError::new(
                        "plan_drift",
                        "Die bestätigte Ausführungsklasse der Quellentfernung stimmt nicht mehr mit der frischen Vorschau überein",
                    ));
                }
                Some(expected)
            } else {
                if request.remove_source_best_effort_expected.is_some() && !preview.skip_only {
                    return Err(AppError::new(
                        "invalid_input",
                        "Diese Dateioperation darf keine Quellentfernungsklasse binden",
                    ));
                }
                None
            };
            let (queue, entry) = with_queue_extension(&state, &window_state, || {
                file_preparation_not_cancelled(cancel)?;
                let operation = QueueOperation::ProviderFile {
                    kind,
                    source,
                    target,
                    items: request.operation.items.clone(),
                    destination: request.operation.destination.clone(),
                    overwrite: request.overwrite,
                    expected_conflicts: expected,
                    source_tree_expected: preview.source_tree.clone().map(Box::new),
                    keep_both_targets: keep_both_targets.clone(),
                    expected_conflict_proofs: if request.overwrite && !preview.conflicts.is_empty() {
                        bound_replacement_proofs(&window_state, &key, request.conflict_preparation_id.as_deref())?
                    } else {
                        Vec::new()
                    },
                    best_effort_commit_expected: None,
                    local_copy_new_best_effort_expected: None,
                    copy_new_best_effort_expected: request.copy_new_best_effort_expected,
                    replace_best_effort_expected: request.replace_best_effort_expected,
                    remove_source_best_effort_expected,
                };
                crate::queue_runtime::enqueue_operation_with_profile_bindings(
                    &app,
                    &window_state,
                    label,
                    &operation,
                    &profile_bindings,
                )
            })?;
            emit_queue_snapshot(&app, label, &queue);
            Ok(entry.operation_id)
        })
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

/// Meldet die tatsächlich zu ersetzenden Ziele — rekursiv, wie beim Provider-Weg.
///
/// Zwei gleichnamige Ordner sind kein Konflikt: Sie werden zusammengeführt, und nur
/// kollidierende Kinder erscheinen in der Liste. Genau diese gebundene Menge
/// verlangt der Executor später als Nachweis.
#[tauri::command]
pub(crate) async fn check_conflicts(
    items: Vec<String>,
    dest_dir: String,
) -> Result<Vec<String>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let paths: Vec<PathBuf> = items.iter().map(expand_home_path).collect();
        let vfs = LocalVfs::new();
        find_recursive_conflicts(&vfs, &vfs, &expand_home_path(&dest_dir), &paths)
            .map_err(AppError::from)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn check_local_file_operation(
    app: tauri::AppHandle,
    window: WebviewWindow,
    kind: String,
    items: Vec<String>,
    dest_dir: String,
) -> Result<ProviderFilePreview, AppError> {
    let window_state = app.state::<AppState>().window(window.label());
    let generation = begin_file_choices(&window_state);
    tauri::async_runtime::spawn_blocking(move || {
        let kind = file_kind(&kind)?;
        let paths = items.iter().map(expand_home_path).collect::<Vec<_>>();
        let destination = expand_home_path(&dest_dir);
        doppeldecker_core::validate_file_operation_paths(&paths, &destination)
            .map_err(AppError::from)?;
        let vfs = LocalVfs::new();
        let conflicts =
            find_recursive_conflicts(&vfs, &vfs, &destination, &paths).map_err(AppError::from)?;
        let details = doppeldecker_core::fileops::file_conflict_views(
            &vfs,
            &vfs,
            &destination,
            &paths,
            &conflicts,
        )
        .map_err(AppError::from)?;
        let targets = doppeldecker_core::fileops::prepare_keep_both_targets(
            &vfs,
            &vfs,
            &destination,
            &paths,
            &conflicts,
        )
        .map_err(AppError::from)?;
        let mut preview = ProviderFilePreview {
            source_read_limitations: Vec::new(),
            source_tree: None,
            conflicts,
            conflict_details: details,
            keep_both_preparation_id: None,
            conflict_preparation_id: None,
            keep_both_names: Vec::new(),
            keep_both_blocker: Some(
                "Für diese Auswahl ist kein atomarer kollisionsfreier Commit gebunden.".into(),
            ),
            target_capabilities: ProviderFileCapabilitiesPayload {
                provider: ProviderKind::Local,
                copy_new: true,
                copy_new_best_effort: false,
                replace: doppeldecker_core::Vfs::file_replace_class(&vfs)
                    != doppeldecker_core::vfs::FileReplaceClass::BestEffort,
                replace_best_effort: doppeldecker_core::Vfs::file_replace_class(&vfs)
                    == doppeldecker_core::vfs::FileReplaceClass::BestEffort,
                remove_source: true,
                remove_source_best_effort: false,
                rename_noreplace: true,
                limitations: Vec::new(),
                blockers: Vec::new(),
            },
            source_capabilities: None,
            skip_only: false,
            source_removal_required: true,
            requires_conflict_choice: false,
            conflict_metadata_only: false,
        };
        publish_file_choices(
            &window_state,
            generation,
            local_choice_key(kind, &items, &dest_dir),
            targets,
            &mut preview,
        )?;
        Ok(preview)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

/// Persistentes Einreihen bindet die gerade bestätigte Konfliktliste ein zweites Mal.
struct LocalFileQueueRequest {
    kind: FileQueueKind,
    items: Vec<String>,
    destination: String,
    overwrite: bool,
    expected_conflicts: Vec<String>,
    keep_both_preparation_id: Option<String>,
    conflict_preparation_id: Option<String>,
    replace_best_effort_expected: Option<bool>,
}

fn enqueue_file_operation(
    app: tauri::AppHandle,
    window: &WebviewWindow,
    state: &State<AppState>,
    request: LocalFileQueueRequest,
) -> Result<String, AppError> {
    let window_state = state.window(window.label());
    let keep_both_targets = bound_file_choices(
        &window_state,
        &local_choice_key(request.kind, &request.items, &request.destination),
        request.keep_both_preparation_id.as_deref(),
    )?;
    if request.overwrite && !keep_both_targets.is_empty() {
        return Err(AppError::new(
            "invalid_input",
            "Beide behalten darf nicht überschreiben",
        ));
    }
    let item_paths: Vec<PathBuf> = request.items.iter().map(expand_home_path).collect();
    let destination = expand_home_path(&request.destination);
    let vfs = LocalVfs::new();
    verify_file_choice_snapshot(
        &window_state,
        &local_choice_key(request.kind, &request.items, &request.destination),
        request.conflict_preparation_id.as_deref(),
        &vfs,
        &vfs,
        false,
    )?;
    // Dieselbe rekursive Menge wie in `check_conflicts` und im Queue-Executor.
    let mut current =
        find_recursive_conflicts(&vfs, &vfs, &destination, &item_paths).map_err(AppError::from)?;
    current.sort();
    let mut expected_conflicts = request.expected_conflicts;
    expected_conflicts.sort();
    if request.overwrite && current != expected_conflicts {
        return Err(AppError::new(
            "plan_drift",
            "Die Zielkonflikte haben sich seit der Bestätigung geändert; bitte erneut prüfen",
        ));
    }
    // `find_recursive_conflicts` liefert bereits vollständige Zielpfade.
    let current_paths = current;
    let expected_conflict_proofs = if request.overwrite && !current_paths.is_empty() {
        bound_replacement_proofs(
            &window_state,
            &local_choice_key(request.kind, &request.items, &request.destination),
            request.conflict_preparation_id.as_deref(),
        )?
    } else {
        Vec::new()
    };
    // Nur die tatsächlich angezeigte Ersetzungsklasse darf in die Queue. Fehlt
    // dieser Beleg bei einem alten Aufrufer, fordert die Queue vor einer schwächeren
    // Wirkung weiterhin ihre eigene ausdrückliche Bestätigung.
    let local_replace_best_effort_expected = if request.overwrite && !current_paths.is_empty() {
        request.replace_best_effort_expected
    } else {
        Some(false)
    };
    let label = window.label();
    let window_state = state.window(label);
    let (queue, entry) = with_queue_extension(state, &window_state, || {
        let operation = QueueOperation::File {
            kind: request.kind,
            items: item_paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            destination: destination.to_string_lossy().into_owned(),
            overwrite: request.overwrite,
            expected_conflicts: current_paths,
            expected_conflict_proofs,
            keep_both_targets: keep_both_targets.clone(),
            local_replace_best_effort_expected,
            local_copy_new_best_effort_expected: None,
        };
        enqueue_operation_with_readback(&app, &window_state, label, &operation)
    })?;
    emit_queue_snapshot(&app, label, &queue);
    start_queue_worker(app, label.to_owned());
    Ok(entry.operation_id)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalFileTransferRequest {
    items: Vec<String>,
    dest_dir: String,
    overwrite: bool,
    expected_conflicts: Vec<String>,
    #[serde(default)]
    keep_both_preparation_id: Option<String>,
    #[serde(default)]
    conflict_preparation_id: Option<String>,
    #[serde(default)]
    replace_best_effort_expected: Option<bool>,
}

#[tauri::command]
pub(crate) async fn copy_paths(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: LocalFileTransferRequest,
) -> Result<String, AppError> {
    let worker_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker_app.state::<AppState>();
        enqueue_file_operation(
            app,
            &window,
            &state,
            LocalFileQueueRequest {
                kind: FileQueueKind::Copy,
                items: request.items,
                destination: request.dest_dir,
                overwrite: request.overwrite,
                expected_conflicts: request.expected_conflicts,
                keep_both_preparation_id: request.keep_both_preparation_id,
                conflict_preparation_id: request.conflict_preparation_id,
                replace_best_effort_expected: request.replace_best_effort_expected,
            },
        )
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn move_paths(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: LocalFileTransferRequest,
) -> Result<String, AppError> {
    let worker_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker_app.state::<AppState>();
        enqueue_file_operation(
            app,
            &window,
            &state,
            LocalFileQueueRequest {
                kind: FileQueueKind::Move,
                items: request.items,
                destination: request.dest_dir,
                overwrite: request.overwrite,
                expected_conflicts: request.expected_conflicts,
                keep_both_preparation_id: request.keep_both_preparation_id,
                conflict_preparation_id: request.conflict_preparation_id,
                replace_best_effort_expected: request.replace_best_effort_expected,
            },
        )
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn trash_paths(
    app: tauri::AppHandle,
    window: WebviewWindow,
    items: Vec<String>,
) -> Result<TrashStats, AppError> {
    let worker_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker_app.state::<AppState>();
        let sandbox = sandbox_root()?;
        let paths: Vec<PathBuf> = items.iter().map(expand_home_path).collect();
        let journal_dir = app_config_dir(&app)?.join("trash-journals");
        let label = window.label();
        let window_state = state.window(label);
        with_reserved_operation(&app, label, &window_state, || {
            // Prozessweit sperren: Der Journalordner gehört allen Fenstern gemeinsam,
            // die Single-Flight-Schranke aber nur diesem einen (AppState-Doku).
            let _journals = state.trash_journal_guard.lock_recover();
            let recovery = recover_trash_journals(&journal_dir).map_err(AppError::from)?;
            if !recovery.is_clean() {
                return Err(AppError::new(
                    "app",
                    format!(
                        "Papierkorb-Recovery ist noch ungeklärt: {}{}",
                        recovery.recovery_paths.join(", "),
                        recovery.errors.join("; ")
                    ),
                ));
            }
            trash_items(&paths, &sandbox, &journal_dir).map_err(AppError::from)
        })
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn create_folder(
    app: tauri::AppHandle,
    window: WebviewWindow,
    parent: String,
    name: String,
) -> Result<String, AppError> {
    let worker_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker_app.state::<AppState>();
        let vfs = LocalVfs::new();
        let sandbox = sandbox_root()?;
        let label = window.label();
        let window_state = state.window(label);
        with_reserved_operation(&app, label, &window_state, || {
            create_dir(&vfs, &expand_home_path(&parent), &name, &sandbox)
                .map(|path| path.to_string_lossy().into_owned())
                .map_err(AppError::from)
        })
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

/// Reine Core-Beschreibung für den Namensdialog, ohne Profil- oder Providerzugriff.
#[tauri::command]
pub(crate) fn provider_folder_contract(
    endpoint: BidirectionalEndpointRequest,
) -> doppeldecker_core::DirectoryCreateContract {
    doppeldecker_core::directory_create_contract(endpoint.provider_kind())
}

/// Providerneutrales Remote-mkdir. Der Endpunkt bindet Provider, Profil und den
/// tatsächlich sichtbaren Pane-Root gemeinsam; dadurch kann das Frontend weder
/// einen Commandnamen zusammensetzen noch einen davon abweichenden Elternpfad senden.
#[tauri::command]
pub(crate) async fn create_provider_folder(
    app: tauri::AppHandle,
    window: WebviewWindow,
    endpoint: BidirectionalEndpointRequest,
    name: String,
    expected_execution_class: doppeldecker_core::DirectoryCreateExecutionClass,
) -> Result<doppeldecker_core::DirectoryCreateReport, AppError> {
    let worker_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker_app.state::<AppState>();
        if endpoint.provider_kind() == ProviderKind::Local {
            return Err(AppError::new(
                "invalid_input",
                "Lokale Ordner verwenden den descriptor-relativen lokalen Command",
            ));
        }
        let name =
            doppeldecker_core::validate_directory_create_request(Path::new(endpoint.root()), &name)
                .map_err(AppError::from)?;
        doppeldecker_core::validate_directory_create_class(
            endpoint.provider_kind(),
            expected_execution_class,
        )
        .map_err(AppError::from)?;
        crate::queue_runtime::require_absolute_remote_parent(endpoint.root())?;
        let (endpoint, _, profile_bindings) = resolve_endpoint(&app, &state, &endpoint)?;
        let label = window.label();
        let window_state = state.window(label);
        crate::create_remote_folder(
            &app,
            label,
            &window_state,
            &endpoint,
            &name,
            expected_execution_class,
            || crate::queue_runtime::ensure_profile_bindings_current(&app, &profile_bindings),
        )
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn rename_path(
    app: tauri::AppHandle,
    window: WebviewWindow,
    path: String,
    new_name: String,
) -> Result<String, AppError> {
    let worker_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker_app.state::<AppState>();
        let vfs = LocalVfs::new();
        let sandbox = sandbox_root()?;
        let old = expand_home_path(&path);
        let label = window.label();
        let window_state = state.window(label);
        with_reserved_operation(&app, label, &window_state, || {
            doppeldecker_core::fileops::rename(&vfs, &old, &new_name, &sandbox)
                .map_err(AppError::from)?;
            let parent = old
                .parent()
                .ok_or_else(|| AppError::new("app", "Kein übergeordneter Ordner"))?;
            Ok(parent.join(new_name.trim()).to_string_lossy().into_owned())
        })
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RenameProviderPathRequest {
    endpoint: BidirectionalEndpointRequest,
    path: String,
    new_name: String,
    #[serde(default)]
    expected_execution_class: Option<doppeldecker_core::RenameExecutionClass>,
}

#[tauri::command]
pub(crate) fn provider_rename_contract(
    endpoint: BidirectionalEndpointRequest,
) -> Result<doppeldecker_core::RenameInteractionContract, AppError> {
    crate::bidirectional_commands::validate_endpoint_request_root(&endpoint)?;
    Ok(doppeldecker_core::ordinary_rename_contract(
        endpoint.provider_kind(),
    ))
}

/// Ergebnis einer Provider-Umbenennung für das Frontend: eindeutiges Resultat,
/// Ausführungsklasse und die konkreten Einschränkungen kommen unverändert aus dem
/// Core — das Frontend erfindet keine eigene Sperr- oder Klassenlogik.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RenameProviderPathResponse {
    outcome: doppeldecker_core::RenameOutcome,
    class: doppeldecker_core::RenameExecutionClass,
    limitations: Vec<String>,
    new_path: Option<String>,
    existing_is_dir: Option<bool>,
    verified_after_reconnect: bool,
}

#[tauri::command]
pub(crate) async fn rename_provider_path(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: RenameProviderPathRequest,
) -> Result<RenameProviderPathResponse, AppError> {
    let worker_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker_app.state::<AppState>();
        let request_root =
            crate::bidirectional_commands::validate_endpoint_request_root(&request.endpoint)?;
        let old = PathBuf::from(&request.path);
        let new_name = doppeldecker_core::validate_ordinary_rename_request(
            request.endpoint.provider_kind(),
            &request_root,
            &old,
            &request.new_name,
            request.expected_execution_class,
        )
        .map_err(AppError::from)?;
        let (endpoint, _, profile_bindings) = resolve_endpoint(&app, &state, &request.endpoint)?;
        let label = window.label();
        let window_state = state.window(label);
        with_reserved_operation(&app, label, &window_state, || {
            let sandbox = if endpoint.kind() == ProviderKind::Local {
                sandbox_root()?
            } else {
                None
            };
            crate::queue_runtime::ensure_profile_bindings_current(&app, &profile_bindings)?;
            let queue = crate::operation_queue(&app, &window_state, label)?;
            let endpoint = queue
                .bind_provider_preparation(&endpoint)
                .map_err(AppError::from)?;
            let report = endpoint
                .execute_ordinary_rename(
                    &old,
                    &new_name,
                    &sandbox,
                    request.expected_execution_class,
                )
                .map_err(AppError::from)?;
            Ok(RenameProviderPathResponse {
                outcome: report.outcome,
                class: report.class,
                limitations: report.limitations,
                new_path: report
                    .new_path
                    .map(|path| path.to_string_lossy().into_owned()),
                existing_is_dir: report.existing_is_dir,
                verified_after_reconnect: report.verified_after_reconnect,
            })
        })
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordnervertrag_deserialisiert_pane_endpunkt_und_liefert_die_frontend_schreibweise() {
        for (provider, expected) in [("webdav", Some("best_effort")), ("sftp", None)] {
            let endpoint = serde_json::from_value(serde_json::json!({"provider":provider,"profileId":"nur-test","profileRevision":1,"root":"/pane"})).unwrap();
            let value = serde_json::to_value(provider_folder_contract(endpoint)).unwrap();
            assert_eq!(value["execution_class"].as_str(), expected);
            assert_eq!(
                value["blockers"].as_array().unwrap().is_empty(),
                expected.is_some()
            );
        }
    }

    /// Tauri benennt **verschachtelte** JSON-Felder nicht zuverlässig so um wie
    /// Top-Level-Argumente. Genau daran scheiterte 2026-07-28 das Remote-Löschen mit
    /// einem fehlenden `profile_id`. Diese drei Request-Strukturen hatten seither
    /// keinen eigenen Regressionstest; geprüft wird deshalb der Payload, den das
    /// Frontend wirklich sendet.
    #[test]
    fn lokale_konfliktentscheidung_deserialisiert_backend_ids_in_frontend_schreibweise() {
        let request: LocalFileTransferRequest = serde_json::from_value(serde_json::json!({
            "items": ["/quelle/Datei.txt"], "destDir": "/ziel", "overwrite": false,
            "expectedConflicts": ["/ziel/Datei.txt"], "keepBothPreparationId": "alias-id",
            "conflictPreparationId": "snapshot-id", "replaceBestEffortExpected": false,
        }))
        .unwrap();
        assert_eq!(
            request.keep_both_preparation_id.as_deref(),
            Some("alias-id")
        );
        assert_eq!(
            request.conflict_preparation_id.as_deref(),
            Some("snapshot-id")
        );
        assert_eq!(request.dest_dir, "/ziel");
        assert_eq!(request.replace_best_effort_expected, Some(false));
    }

    #[test]
    fn passive_ftps_metadaten_erteilen_keine_ersetzungs_oder_aliasbindung() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("source");
        let target = root.join("target");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let item = source.join("file.txt");
        std::fs::write(&item, b"source").unwrap();
        std::fs::write(target.join("file.txt"), b"target").unwrap();
        let vfs = LocalVfs::new();
        let items = [item];
        let conflicts = find_recursive_conflicts(&vfs, &vfs, &target, &items).unwrap();
        let mut preview = ProviderFilePreview {
            source_read_limitations: Vec::new(),
            source_tree: None,
            conflict_details: doppeldecker_core::fileops::file_conflict_metadata_views(
                &vfs, &vfs, &target, &items, &conflicts,
            )
            .unwrap(),
            conflicts,
            keep_both_preparation_id: None,
            conflict_preparation_id: None,
            keep_both_names: Vec::new(),
            keep_both_blocker: None,
            target_capabilities: ProviderFileCapabilities {
                provider: ProviderKind::Ftps,
                copy_new: false,
                copy_new_best_effort: false,
                replace: false,
                replace_best_effort: false,
                remove_source: false,
                remove_source_best_effort: false,
                rename_noreplace: false,
                limitations: Vec::new(),
                blockers: Vec::new(),
            }
            .into(),
            source_capabilities: None,
            skip_only: false,
            source_removal_required: false,
            requires_conflict_choice: true,
            conflict_metadata_only: true,
        };
        let window = crate::queue_runtime::WindowState::default();
        let generation = begin_file_choices(&window);
        publish_file_choices(
            &window,
            generation,
            "scope".into(),
            Vec::new(),
            &mut preview,
        )
        .unwrap();
        let old_id = preview.conflict_preparation_id.clone();
        verify_file_choice_snapshot(&window, "scope", old_id.as_deref(), &vfs, &vfs, false)
            .unwrap();
        assert_eq!(
            bound_replacement_proofs(&window, "scope", old_id.as_deref())
                .unwrap_err()
                .code,
            "plan_drift"
        );
        let payload = serde_json::to_value(&preview).unwrap();
        assert!(payload.get("conflictMetadataOnly").is_none());
        let aliases = doppeldecker_core::fileops::prepare_keep_both_targets(
            &vfs,
            &vfs,
            &target,
            &items,
            &preview.conflicts,
        )
        .unwrap();
        assert!(
            publish_file_choices(&window, generation, "scope".into(), aliases, &mut preview)
                .is_err()
        );
        std::fs::write(target.join("file.txt"), b"changed target").unwrap();
        assert_eq!(
            verify_file_choice_snapshot(&window, "scope", old_id.as_deref(), &vfs, &vfs, false)
                .unwrap_err()
                .code,
            "plan_drift"
        );
        preview.conflict_metadata_only = false;
        preview.requires_conflict_choice = false;
        preview.conflict_details = doppeldecker_core::fileops::file_conflict_views(
            &vfs,
            &vfs,
            &target,
            &items,
            &preview.conflicts,
        )
        .unwrap();
        publish_file_choices(
            &window,
            generation,
            "scope".into(),
            Vec::new(),
            &mut preview,
        )
        .unwrap();
        assert!(verify_file_choice_snapshot(
            &window,
            "scope",
            old_id.as_deref(),
            &vfs,
            &vfs,
            false
        )
        .is_err());
        let full =
            bound_replacement_proofs(&window, "scope", preview.conflict_preparation_id.as_deref())
                .unwrap();
        assert!(full[0].content_hash.is_some());
        verify_file_choice_snapshot(
            &window,
            "scope",
            preview.conflict_preparation_id.as_deref(),
            &vfs,
            &vfs,
            false,
        )
        .unwrap();
    }

    #[test]
    fn konfliktvorschau_bindet_fenster_scope_ablauf_und_originale_zielbelege() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let item = source.join("Datei.txt");
        std::fs::write(&item, b"neu").unwrap();
        std::fs::write(target.join("Datei.txt"), b"alt").unwrap();
        let vfs = LocalVfs::new();
        let items = vec![item];
        let conflicts = find_recursive_conflicts(&vfs, &vfs, &target, &items).unwrap();
        let details = doppeldecker_core::fileops::file_conflict_views(
            &vfs, &vfs, &target, &items, &conflicts,
        )
        .unwrap();
        let targets = doppeldecker_core::fileops::prepare_keep_both_targets(
            &vfs, &vfs, &target, &items, &conflicts,
        )
        .unwrap();
        let mut preview = ProviderFilePreview {
            source_read_limitations: Vec::new(),
            source_tree: None,
            conflicts,
            conflict_details: details,
            keep_both_preparation_id: None,
            conflict_preparation_id: None,
            keep_both_names: Vec::new(),
            keep_both_blocker: None,
            target_capabilities: ProviderFileCapabilitiesPayload {
                provider: ProviderKind::Local,
                copy_new: true,
                copy_new_best_effort: false,
                replace: true,
                replace_best_effort: false,
                remove_source: true,
                remove_source_best_effort: false,
                rename_noreplace: true,
                limitations: Vec::new(),
                blockers: Vec::new(),
            },
            source_capabilities: None,
            skip_only: false,
            source_removal_required: true,
            requires_conflict_choice: false,
            conflict_metadata_only: false,
        };
        let window = crate::queue_runtime::WindowState::default();
        let other = crate::queue_runtime::WindowState::default();
        let generation = begin_file_choices(&window);
        publish_file_choices(
            &window,
            generation,
            "scope".into(),
            targets.clone(),
            &mut preview,
        )
        .unwrap();
        assert!(preview
            .keep_both_names
            .iter()
            .all(|name| !name.contains('/')));
        let id = preview.conflict_preparation_id.as_deref();
        assert!(bound_file_choices(&other, "scope", id).is_err());
        assert!(bound_file_choices(&window, "anderer-scope", id).is_err());
        verify_file_choice_snapshot(&window, "scope", id, &vfs, &vfs, false).unwrap();
        let original_proofs = bound_replacement_proofs(&window, "scope", id).unwrap();
        std::fs::write(target.join("Datei.txt"), b"fremd").unwrap();
        assert_eq!(
            verify_file_choice_snapshot(&window, "scope", id, &vfs, &vfs, false)
                .unwrap_err()
                .code,
            "plan_drift"
        );
        assert_eq!(
            bound_replacement_proofs(&window, "scope", id).unwrap(),
            original_proofs
        );
        window
            .file_conflict_choices
            .lock_recover()
            .choice
            .as_mut()
            .unwrap()
            .created = std::time::Instant::now() - std::time::Duration::from_secs(601);
        assert!(bound_file_choices(&window, "scope", id).is_err());

        preview.conflict_details = doppeldecker_core::fileops::file_conflict_views(
            &vfs,
            &vfs,
            &target,
            &items,
            &preview.conflicts,
        )
        .unwrap();
        let older = begin_file_choices(&window);
        let newer = begin_file_choices(&window);
        publish_file_choices(
            &window,
            newer,
            "scope".into(),
            targets.clone(),
            &mut preview,
        )
        .unwrap();
        let current_id = preview.conflict_preparation_id.clone();
        let error = publish_file_choices(&window, older, "old-scope".into(), targets, &mut preview)
            .unwrap_err();
        assert_eq!(error.code, "plan_drift");
        verify_file_choice_snapshot(&window, "scope", current_id.as_deref(), &vfs, &vfs, false)
            .unwrap();
        let still_bound = bound_file_choices(&window, "scope", current_id.as_deref()).unwrap();
        doppeldecker_core::fileops::verify_keep_both_targets(
            &vfs,
            &vfs,
            &items,
            &target,
            &still_bound,
        )
        .unwrap();
    }

    #[test]
    fn verschachtelte_request_felder_folgen_dem_frontend() {
        let delete: ProviderDeleteRequest = serde_json::from_value(serde_json::json!({
            "endpoint": {
                "provider": "webdav",
                "profileId": "profil-1",
                "profileRevision": 7,
                "root": "/httpdocs"
            },
            "items": ["/httpdocs/alt.txt"]
        }))
        .unwrap();
        assert_eq!(delete.items, vec!["/httpdocs/alt.txt".to_string()]);
        assert_eq!(delete.endpoint.provider_kind(), ProviderKind::WebDav);

        let rename: RenameProviderPathRequest = serde_json::from_value(serde_json::json!({
            "endpoint": {
                "provider": "sftp",
                "profileId": "profil-2",
                "profileRevision": 3,
                "root": "/daten"
            },
            "path": "/daten/alt.txt",
            "newName": "neu.txt"
        }))
        .unwrap();
        assert_eq!(rename.path, "/daten/alt.txt");
        assert_eq!(rename.new_name, "neu.txt");
        assert_eq!(rename.expected_execution_class, None);
        let confirmed: RenameProviderPathRequest = serde_json::from_value(serde_json::json!({
            "endpoint": {"provider":"webdav", "profileId":"profil-3", "profileRevision":4, "root":"/daten"},
            "path":"/daten/alt.txt", "newName":"neu.txt", "expectedExecutionClass":"best_effort"
        })).unwrap();
        assert_eq!(
            confirmed.expected_execution_class,
            Some(doppeldecker_core::RenameExecutionClass::BestEffort)
        );

        // Fehlt die Revision im verschachtelten Endpunkt, muss die Deserialisierung
        // scheitern und den Feldnamen nennen — nicht stillschweigend `None` liefern.
        let fehler = match serde_json::from_value::<RenameProviderPathRequest>(serde_json::json!({
            "endpoint": {
                "provider": "sftp",
                "profile_id": "profil-2",
                "root": "/daten"
            },
            "path": "/daten/alt.txt",
            "newName": "neu.txt"
        })) {
            Ok(_) => panic!("Ein Endpunkt ohne Revision darf nicht durchgehen"),
            Err(error) => error,
        };
        assert!(fehler.to_string().contains("profileId"), "{fehler}");
    }

    #[test]
    fn baumvorschau_ohne_zielkonflikte_bindet_id_fenster_und_frischen_quellumfang() {
        let tree = doppeldecker_core::vfs::InteractiveSourceTree {
            digest: "ab".repeat(32),
            entries: 3,
            files: 1,
            directories: 2,
            bytes: 11,
            omitted: Vec::new(),
        };
        let mut preview = ProviderFilePreview {
            source_read_limitations: tree.limitations(),
            source_tree: Some(tree.clone()),
            conflicts: Vec::new(),
            conflict_details: Vec::new(),
            keep_both_preparation_id: None,
            conflict_preparation_id: None,
            keep_both_names: Vec::new(),
            keep_both_blocker: None,
            target_capabilities: ProviderFileCapabilities {
                provider: ProviderKind::Local,
                copy_new: true,
                copy_new_best_effort: false,
                replace: false,
                replace_best_effort: false,
                remove_source: false,
                remove_source_best_effort: false,
                rename_noreplace: true,
                limitations: Vec::new(),
                blockers: Vec::new(),
            }
            .into(),
            source_capabilities: None,
            skip_only: false,
            source_removal_required: true,
            requires_conflict_choice: false,
            conflict_metadata_only: false,
        };
        let window = crate::queue_runtime::WindowState::default();
        let other = crate::queue_runtime::WindowState::default();
        let generation = begin_file_choices(&window);
        publish_file_choices(
            &window,
            generation,
            "baum-scope".into(),
            Vec::new(),
            &mut preview,
        )
        .unwrap();
        let id = preview.conflict_preparation_id.as_deref();
        assert!(id.is_some());
        assert!(preview.keep_both_preparation_id.is_none());
        assert!(bound_file_choices(&other, "baum-scope", id).is_err());
        assert_eq!(
            window
                .file_conflict_choices
                .lock_recover()
                .choice
                .as_ref()
                .unwrap()
                .source_tree,
            Some(tree)
        );
        let vfs = LocalVfs::new();
        assert_eq!(
            verify_file_choice_snapshot(&window, "baum-scope", id, &vfs, &vfs, false)
                .unwrap_err()
                .code,
            "plan_drift"
        );
        let value = serde_json::to_value(&preview).unwrap();
        assert_eq!(value["sourceTree"]["directories"], 2);
        assert!(value["conflictPreparationId"].is_string());
    }

    /// Regressionstest gegen den stillen Namensbruch verschachtelter Felder:
    /// Das äußere `rename_all = "camelCase"` benennt nur die eigenen Felder um.
    /// Das Frontend liest `copyNew`/`removeSource`/`copyNewBestEffort` — genau
    /// diese Schreibweise muss der echte Payload liefern, sonst sperrt die GUI
    /// Remote-Übertragungen mit `undefined`-Fähigkeiten.
    #[test]
    fn provider_file_preview_serialisiert_camel_case_fuer_das_frontend() {
        let preview = ProviderFilePreview {
            source_read_limitations: Vec::new(),
            source_tree: None,
            conflict_details: Vec::new(),
            keep_both_preparation_id: None,
            conflict_preparation_id: None,
            keep_both_names: Vec::new(),
            keep_both_blocker: None,
            conflicts: Vec::new(),
            target_capabilities: ProviderFileCapabilities {
                provider: doppeldecker_core::ProviderKind::Ftps,
                copy_new: false,
                copy_new_best_effort: true,
                replace: false,
                replace_best_effort: true,
                remove_source: true,
                remove_source_best_effort: false,
                rename_noreplace: false,
                limitations: vec!["Testeinschränkung".into()],
                blockers: Vec::new(),
            }
            .into(),
            source_capabilities: None,
            skip_only: false,
            source_removal_required: true,
            requires_conflict_choice: false,
            conflict_metadata_only: false,
        };
        let value = serde_json::to_value(&preview).unwrap();
        let target = &value["targetCapabilities"];
        assert_eq!(target["copyNew"], false);
        assert_eq!(target["copyNewBestEffort"], true);
        assert_eq!(target["replaceBestEffort"], true);
        assert_eq!(target["removeSource"], true);
        assert_eq!(target["removeSourceBestEffort"], false);
        assert_eq!(target["renameNoreplace"], false);
        assert_eq!(target["limitations"][0], "Testeinschränkung");
    }

    #[test]
    fn enqueue_payload_bindet_best_effort_quellentfernung_in_frontend_schreibweise() {
        let request: EnqueueProviderFileRequest = serde_json::from_value(serde_json::json!({
            "kind": "move",
            "source": { "provider": "sftp", "profileId": "quelle", "root": "/daten" },
            "target": { "provider": "local", "root": "/ziel" },
            "items": ["/daten/datei.txt"],
            "destination": "/ziel",
            "overwrite": false,
            "expectedConflicts": [],
            "removeSourceBestEffortExpected": true
        }))
        .unwrap();

        assert_eq!(request.remove_source_best_effort_expected, Some(true));
    }

    #[test]
    fn enqueue_payload_bindet_sichtbare_ziel_ausfuehrungsklassen() {
        let request: EnqueueProviderFileRequest = serde_json::from_value(serde_json::json!({
            "kind": "copy",
            "source": { "provider": "local", "root": "/quelle" },
            "target": { "provider": "ftps", "profileId": "ziel", "root": "/daten" },
            "items": ["/quelle/datei.txt"],
            "destination": "/daten",
            "overwrite": true,
            "expectedConflicts": ["datei.txt"],
            "copyNewBestEffortExpected": true,
            "replaceBestEffortExpected": false
        }))
        .unwrap();

        assert_eq!(request.copy_new_best_effort_expected, Some(true));
        assert_eq!(request.replace_best_effort_expected, Some(false));
    }

    #[test]
    fn frische_staerkere_schwaechere_oder_fehlende_klasse_ist_plan_drift() {
        let request_json = serde_json::json!({
            "kind": "copy",
            "source": { "provider": "local", "root": "/quelle" },
            "target": { "provider": "ftps", "profileId": "ziel", "root": "/daten" },
            "items": ["/quelle/datei.txt"],
            "destination": "/daten",
            "overwrite": true,
            "expectedConflicts": ["datei.txt"],
            "copyNewBestEffortExpected": true,
            "replaceBestEffortExpected": false
        });
        let preview =
            |copy_new, copy_new_best_effort, replace, replace_best_effort| ProviderFilePreview {
                source_read_limitations: Vec::new(),
                source_tree: None,
                conflict_details: Vec::new(),
                keep_both_preparation_id: None,
                conflict_preparation_id: None,
                keep_both_names: Vec::new(),
                keep_both_blocker: None,
                conflicts: vec!["datei.txt".into()],
                target_capabilities: ProviderFileCapabilities {
                    provider: ProviderKind::Ftps,
                    copy_new,
                    copy_new_best_effort,
                    replace,
                    replace_best_effort,
                    remove_source: true,
                    remove_source_best_effort: false,
                    rename_noreplace: copy_new,
                    limitations: Vec::new(),
                    blockers: Vec::new(),
                }
                .into(),
                source_capabilities: None,
                skip_only: false,
                source_removal_required: true,
                requires_conflict_choice: false,
                conflict_metadata_only: false,
            };

        let exact: EnqueueProviderFileRequest =
            serde_json::from_value(request_json.clone()).unwrap();
        assert!(validate_provider_file_execution_classes(
            &exact,
            &preview(false, true, true, false),
        )
        .is_ok());

        for fresh in [
            preview(true, false, true, false),
            preview(false, true, false, true),
            preview(false, false, true, false),
        ] {
            let request: EnqueueProviderFileRequest =
                serde_json::from_value(request_json.clone()).unwrap();
            assert_eq!(
                validate_provider_file_execution_classes(&request, &fresh)
                    .unwrap_err()
                    .code,
                "plan_drift"
            );
        }

        let missing: EnqueueProviderFileRequest = serde_json::from_value(serde_json::json!({
            "kind": "copy",
            "source": { "provider": "local", "root": "/quelle" },
            "target": { "provider": "ftps", "profileId": "ziel", "root": "/daten" },
            "items": ["/quelle/datei.txt"],
            "destination": "/daten",
            "overwrite": false,
            "expectedConflicts": []
        }))
        .unwrap();
        assert_eq!(
            validate_provider_file_execution_classes(
                &missing,
                &preview(false, true, false, false),
            )
            .unwrap_err()
            .code,
            "plan_drift"
        );
        let mut no_effects = preview(false, false, false, false);
        no_effects.skip_only = true;
        let mut no_effects_request: EnqueueProviderFileRequest =
            serde_json::from_value(request_json).unwrap();
        no_effects_request.overwrite = false;
        no_effects_request.copy_new_best_effort_expected = None;
        no_effects_request.replace_best_effort_expected = None;
        assert!(validate_provider_file_execution_classes(&no_effects_request, &no_effects).is_ok());
        let payload = serde_json::to_value(&no_effects).unwrap();
        assert_eq!(payload["skipOnly"], true);
        assert_eq!(payload["requiresConflictChoice"], false);
        no_effects_request.overwrite = true;
        assert_eq!(
            validate_provider_file_execution_classes(&no_effects_request, &no_effects)
                .unwrap_err()
                .code,
            "invalid_input"
        );
        no_effects_request.overwrite = false;
        no_effects_request.keep_both_preparation_id = Some("alias".into());
        assert_eq!(
            validate_provider_file_execution_classes(&no_effects_request, &no_effects)
                .unwrap_err()
                .code,
            "invalid_input"
        );
    }
}
