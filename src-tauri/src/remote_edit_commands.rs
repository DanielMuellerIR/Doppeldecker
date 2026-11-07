//! Bewusstes Remote-Bearbeiten. Lokale Arbeitsdatei und Rückupload sind getrennte
//! Wirkungen; der Rückupload benutzt ausschließlich die normale Fensterqueue.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use doppeldecker_core::{
    Cancel, FileQueueKind, LocalVfs, ProviderEndpoint, QueueOperation, QueueState,
    RemoteOriginalProof, RemoteProfileBinding, RemoteSnapshot, Vfs,
};
use tauri::{Manager, WebviewWindow};

use crate::bidirectional_commands::{resolve_endpoint, BidirectionalEndpointRequest};
use crate::{AppError, AppState, MutexExt};

#[derive(Clone)]
pub(crate) struct RemoteEdit {
    endpoint: BidirectionalEndpointRequest,
    source: PathBuf,
    profiles: Vec<RemoteProfileBinding>,
    local: PathBuf,
    original: RemoteOriginalProof,
    saved_version: Option<EditFileVersion>,
    pending: Option<PendingSave>,
    last_operation_id: Option<String>,
    displayed_replace_class: Option<bool>,
}

#[derive(Clone)]
struct PendingSave {
    operation_id: String,
    bytes: u64,
    hash: String,
    local_version: Option<EditFileVersion>,
}

#[derive(Clone, PartialEq, Eq)]
struct EditFileVersion {
    device: u64,
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

fn edit_file_version(path: &Path) -> Option<EditFileVersion> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    Some(EditFileVersion {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        mtime: (metadata.mtime(), metadata.mtime_nsec()),
        ctime: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteEditView {
    id: String,
    name: String,
    endpoint_label: String,
    dirty: bool,
    last_operation_id: Option<String>,
    limitations: Vec<String>,
    replace_best_effort: bool,
    save_available: bool,
    save_blocker: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteEditRequest {
    id: String,
}

fn endpoint_label(endpoint: &BidirectionalEndpointRequest) -> String {
    let (protocol, profile) = match endpoint {
        BidirectionalEndpointRequest::Local { .. } => ("Lokal", None),
        BidirectionalEndpointRequest::Sftp { profile_id, .. } => {
            ("SFTP", Some(profile_id.as_str()))
        }
        BidirectionalEndpointRequest::Ftps { profile_id, .. } => {
            ("FTP/FTPS", Some(profile_id.as_str()))
        }
        BidirectionalEndpointRequest::Webdav { profile_id, .. } => {
            ("WebDAV", Some(profile_id.as_str()))
        }
        BidirectionalEndpointRequest::Smb { profile_id, .. } => ("SMB", Some(profile_id.as_str())),
    };
    match profile {
        Some(profile) => format!("{protocol} · {profile} · {}", endpoint.root()),
        None => format!("{protocol} · {}", endpoint.root()),
    }
}

fn edit_id() -> Result<String, AppError> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|error| AppError::new("io", error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn not_cancelled(cancel: &Cancel) -> Result<(), AppError> {
    if cancel.is_cancelled() {
        Err(AppError::new(
            "interrupted",
            "Remote-Bearbeitung abgebrochen",
        ))
    } else {
        Ok(())
    }
}

fn copy_edit_version(
    source: &Path,
    cache: &Path,
    name: &str,
    cancel: &Cancel,
) -> Result<(PathBuf, u64, String), AppError> {
    let copy = doppeldecker_core::remote_edit::create_working_copy(source, cache, name, cancel)
        .map_err(AppError::from)?;
    Ok((copy.path, copy.bytes, copy.blake3))
}

fn edit_endpoint(
    mut endpoint: BidirectionalEndpointRequest,
    source: &Path,
) -> Result<BidirectionalEndpointRequest, AppError> {
    if !source.starts_with(endpoint.root()) || source == Path::new(endpoint.root()) {
        return Err(AppError::new(
            "invalid_input",
            "Bearbeitung verlässt den gewählten Root",
        ));
    }
    let parent = source
        .parent()
        .ok_or_else(|| AppError::new("invalid_input", "Dateieltern fehlen"))?;
    let parent = parent
        .to_str()
        .ok_or_else(|| AppError::new("invalid_input", "Dateieltern sind nicht darstellbar"))?
        .to_owned();
    match &mut endpoint {
        BidirectionalEndpointRequest::Local { root }
        | BidirectionalEndpointRequest::Sftp { root, .. }
        | BidirectionalEndpointRequest::Ftps { root, .. }
        | BidirectionalEndpointRequest::Webdav { root, .. }
        | BidirectionalEndpointRequest::Smb { root, .. } => *root = parent,
    }
    Ok(endpoint)
}

pub(crate) fn register_edit(
    app: &tauri::AppHandle,
    label: &str,
    endpoint: BidirectionalEndpointRequest,
    source: PathBuf,
    profiles: Vec<RemoteProfileBinding>,
    snapshot: &RemoteSnapshot,
    cancel: &Cancel,
) -> Result<(String, PathBuf), AppError> {
    let state = app.state::<AppState>();
    // Die Queue überträgt direkt in ihren Zielroot. Auch ein gültiger tieferer
    // Snapshotpfad wird deshalb auf seinen tatsächlichen Elternbereich gebunden.
    let endpoint = edit_endpoint(endpoint, &source)?;
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            AppError::new(
                "invalid_input",
                "Remote-Datei braucht einen darstellbaren Originalnamen",
            )
        })?;
    let cache = crate::browser_commands::snapshot_cache(app, "remote-edits")?;
    let (local, bytes, hash) = copy_edit_version(Path::new(&snapshot.path), &cache, name, cancel)?;
    if bytes != snapshot.bytes || hash != snapshot.blake3 {
        return Err(AppError::new(
            "plan_drift",
            "Lokale Arbeitskopie stimmt nicht mit dem geprüften Download überein",
        ));
    }
    let id = edit_id()?;
    state.window(label).remote_edits.lock_recover().insert(
        id.clone(),
        RemoteEdit {
            endpoint,
            source,
            profiles,
            local: local.clone(),
            original: snapshot.original.clone(),
            saved_version: edit_file_version(&local),
            pending: None,
            last_operation_id: None,
            displayed_replace_class: None,
        },
    );
    Ok((id, local))
}

fn refresh_completed(edit: &mut RemoteEdit, queue: &doppeldecker_core::OperationQueue) {
    if let Some(pending) = edit.pending.as_ref() {
        let snapshot = queue.snapshot();
        if let Some(entry) = snapshot
            .entries
            .iter()
            .find(|entry| entry.operation_id == pending.operation_id)
        {
            if entry.state == QueueState::Completed {
                edit.original.bytes = pending.bytes;
                edit.original.blake3 = pending.hash.clone();
                // Die Queue belegt den geschriebenen Inhalt. Neue Remotezeiten
                // werden beim nächsten Save frisch gelesen, kein alter Zeitwert erfunden.
                edit.original.mtime = None;
                edit.original.mode = None;
                edit.saved_version = pending.local_version.clone();
                edit.pending = None;
            } else if matches!(
                entry.state,
                QueueState::Failed | QueueState::Cancelled | QueueState::Interrupted
            ) {
                edit.pending = None;
            }
        } else {
            // Pending-Aufträge dürfen aus der Queue entfernt werden. Das ist
            // kein erfolgreicher Upload und ändert den Originalbeleg nicht.
            edit.pending = None;
        }
    }
}

fn resolve_edit(
    app: &tauri::AppHandle,
    state: &AppState,
    edit: &RemoteEdit,
) -> Result<
    (
        ProviderEndpoint,
        Box<doppeldecker_core::BidirectionalQueueEndpoint>,
        Vec<RemoteProfileBinding>,
    ),
    AppError,
> {
    crate::queue_runtime::ensure_profile_bindings_current(app, &edit.profiles)?;
    resolve_endpoint(app, state, &edit.endpoint)
}

#[tauri::command]
pub(crate) async fn list_remote_edits(
    app: tauri::AppHandle,
    window: WebviewWindow,
) -> Result<Vec<RemoteEditView>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let window_state = state.window(window.label());
        crate::queue_runtime::with_reserved_cancellable_operation(
            &app,
            window.label(),
            &window_state,
            |cancel| {
                let queue = crate::operation_queue(&app, &window_state, window.label())?;
                let sessions = window_state.remote_edits.lock_recover().clone();
                let mut result = Vec::new();
                for (id, mut edit) in sessions {
                    not_cancelled(cancel)?;
                    refresh_completed(&mut edit, &queue);
                    let capability =
                        resolve_edit(&app, &state, &edit).and_then(|(endpoint, _, _)| {
                            queue
                                .bind_provider_preparation_with_cancel(&endpoint, cancel)
                                .map_err(AppError::from)?
                                .probe_file_target_capabilities()
                                .map_err(AppError::from)
                        });
                    let (class, limitations, mut blocker) = match capability {
                        Ok(caps) if caps.replace || caps.replace_best_effort => (
                            Some(!caps.replace && caps.replace_best_effort),
                            caps.limitations,
                            None,
                        ),
                        Ok(caps) => (
                            None,
                            caps.limitations,
                            Some(if caps.blockers.is_empty() {
                                "Provider kann die Originaldatei gegenwärtig nicht ersetzen".into()
                            } else {
                                caps.blockers.join("; ")
                            }),
                        ),
                        Err(error) => (None, Vec::new(), Some(error.message)),
                    };
                    if blocker.is_none() && edit.pending.is_some() {
                        blocker = Some("Der Rückupload ist noch in der Queue. Nach seinem Abschluss die Übersicht aktualisieren.".into());
                    }
                    edit.displayed_replace_class = class;
                    let dirty = edit.saved_version.is_none()
                        || edit_file_version(&edit.local) != edit.saved_version;
                    result.push(RemoteEditView {
                        id: id.clone(),
                        name: edit
                            .source
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        endpoint_label: endpoint_label(&edit.endpoint),
                        dirty,
                        last_operation_id: edit.last_operation_id.clone(),
                        limitations,
                        replace_best_effort: class.unwrap_or(false),
                        save_available: class.is_some() && edit.pending.is_none(),
                        save_blocker: blocker,
                    });
                    if let Some(current) = window_state.remote_edits.lock_recover().get_mut(&id) {
                        *current = edit;
                    }
                }
                result.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
                Ok(result)
            },
            |_| {},
        )?
    })
    .await
    .map_err(|_| AppError::new("app", "Remote-Bearbeitungsübersicht wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn save_remote_edit(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: RemoteEditRequest,
) -> Result<String, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let window_state = state.window(window.label());
        crate::queue_runtime::with_reserved_cancellable_operation(&app, window.label(), &window_state, |cancel| {
            let queue = crate::operation_queue(&app, &window_state, window.label())?;
            let mut edit = window_state.remote_edits.lock_recover().get(&request.id).cloned()
                .ok_or_else(|| AppError::new("not_found", "Remote-Bearbeitung gehört nicht zu diesem Fenster"))?;
            refresh_completed(&mut edit, &queue);
            if edit.pending.is_some() { return Err(AppError::new("busy", "Der vorige Rückupload ist noch in der Queue")); }
            let (endpoint, target_queue, profiles) = resolve_edit(&app, &state, &edit)?;
            let name = edit.source.file_name().and_then(|name| name.to_str()).ok_or_else(|| AppError::new("invalid_input", "Originalname fehlt"))?;
            let source = ProviderEndpoint::local(edit.local.parent().unwrap()).map_err(AppError::from)?;
            let target = queue.bind_provider_preparation_with_cancel(&endpoint, cancel).map_err(AppError::from)?;
            let contexts = source.interactive_file_contexts(&target, std::slice::from_ref(&edit.local), false, true, cancel).map_err(AppError::from)?;
            let caps = target.probe_file_target_capabilities().map_err(AppError::from)?;
            let class = if caps.replace { Some(false) } else if caps.replace_best_effort { Some(true) } else { None };
            if class.is_none() || class != edit.displayed_replace_class { return Err(AppError::new("plan_drift", "Die Ersetzungsklasse hat sich geändert; Bearbeitungsübersicht erneut anzeigen")); }
            let conflicts = vec![edit.source.to_string_lossy().into_owned()];
            let proofs = doppeldecker_core::remote_edit::bind_original_replacement(contexts.target_vfs.as_ref(), &edit.source, &edit.original).map_err(AppError::from)?;
            let cache = crate::browser_commands::snapshot_cache(&app, "remote-edit-uploads")?;
            let before = edit_file_version(&edit.local);
            let (staged, bytes, hash) = copy_edit_version(&edit.local, &cache, name, cancel)?;
            let local_version = before.filter(|version| edit_file_version(&edit.local).as_ref() == Some(version));
            // Die editierbare Arbeitsdatei bleibt 0600. Nur die separate Queuequelle
            // erhält die frisch belegten Zielrechte; ihr privater Elternordner
            // bleibt 0700 und ist nicht für die externe Anwendung bestimmt.
            if let Some(mode) = proofs[0].mode {
                LocalVfs::rooted(staged.parent().unwrap()).map_err(AppError::from)?.set_mode(&staged, mode & 0o777).map_err(AppError::from)?;
            }
            not_cancelled(cancel)?;
            let operation = QueueOperation::ProviderFile {
                kind: FileQueueKind::Copy,
                source: Box::new(doppeldecker_core::BidirectionalQueueEndpoint::Local { root: staged.parent().unwrap().to_string_lossy().into_owned() }),
                target: target_queue,
                items: vec![staged.to_string_lossy().into_owned()],
                destination: edit.source.parent().unwrap().to_string_lossy().into_owned(),
                overwrite: true, expected_conflicts: conflicts, expected_conflict_proofs: proofs,
                source_tree_expected: None, keep_both_targets: Vec::new(), best_effort_commit_expected: None,
                local_copy_new_best_effort_expected: None,
                copy_new_best_effort_expected: if caps.copy_new { Some(false) } else if caps.copy_new_best_effort { Some(true) } else { None },
                replace_best_effort_expected: class, remove_source_best_effort_expected: None,
            };
            let (queue, entry) = crate::with_queue_extension(&state, &window_state, || {
                crate::queue_runtime::enqueue_operation_with_profile_bindings(&app, &window_state, window.label(), &operation, &profiles)
            })?;
            edit.pending = Some(PendingSave { operation_id: entry.operation_id.clone(), bytes, hash, local_version });
            edit.last_operation_id = Some(entry.operation_id.clone());
            window_state.remote_edits.lock_recover().insert(request.id, edit);
            crate::emit_queue_snapshot(&app, window.label(), &queue);
            Ok(entry.operation_id)
        }, |_| {})?
    }).await.map_err(|_| AppError::new("app", "Rückuploadvorbereitung wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) fn close_remote_edit(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: RemoteEditRequest,
) -> Result<(), AppError> {
    let state = app.state::<AppState>();
    let window_state = state.window(window.label());
    crate::with_reserved_operation(&app, window.label(), &window_state, || {
        window_state
            .remote_edits
            .lock_recover()
            .remove(&request.id)
            .ok_or_else(|| {
                AppError::new(
                    "not_found",
                    "Remote-Bearbeitung gehört nicht zu diesem Fenster",
                )
            })?;
        // Schließen beendet nur die Session. Auch ungespeicherte Arbeitsdateien
        // bleiben im privaten Cache; es gibt hier keine Verwerfen-Wirkung.
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn tiefere_originaldatei_bindet_den_queuezielroot_auf_ihre_eltern() {
        for provider in ["sftp", "ftps", "webdav", "smb"] {
            let endpoint: BidirectionalEndpointRequest =
                serde_json::from_value(serde_json::json!({
                    "provider": provider, "profileId": "edit", "profileRevision": 7, "root": "/root"
                }))
                .unwrap();
            let bound =
                edit_endpoint(endpoint.clone(), Path::new("/root/unter/Datei.txt")).unwrap();
            assert_eq!(bound.root(), "/root/unter");
            let value = serde_json::to_value(bound).unwrap();
            assert_eq!(value["profileId"], "edit");
            assert_eq!(value["profileRevision"], 7);
            assert!(edit_endpoint(endpoint, Path::new("/root-fremd/Datei.txt")).is_err());
        }
    }

    #[test]
    fn entfernter_pending_upload_sperrt_bearbeitung_nicht_und_bestaetigt_keinen_upload() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let local = root.join("Datei.txt");
        std::fs::write(&local, b"Original").unwrap();
        let destination = root.join("Ziel");
        std::fs::create_dir(&destination).unwrap();
        let queue = doppeldecker_core::OperationQueue::open(root.join("queue.json")).unwrap();
        let entry = queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec![local.to_string_lossy().into_owned()],
                destination: destination.to_string_lossy().into_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                keep_both_targets: Vec::new(),
                local_replace_best_effort_expected: None,
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let mut edit = RemoteEdit {
            endpoint: BidirectionalEndpointRequest::Local {
                root: destination.to_string_lossy().into_owned(),
            },
            source: destination.join("Datei.txt"),
            profiles: Vec::new(),
            local: local.clone(),
            original: RemoteOriginalProof {
                bytes: 8,
                blake3: "Originalbeleg".into(),
                mtime: Some(7),
                mode: Some(0o644),
            },
            saved_version: edit_file_version(&local),
            pending: Some(PendingSave {
                operation_id: entry.operation_id.clone(),
                bytes: 12,
                hash: "Nicht hochgeladen".into(),
                local_version: None,
            }),
            last_operation_id: Some(entry.operation_id.clone()),
            displayed_replace_class: Some(true),
        };
        queue.remove_pending(&entry.operation_id).unwrap();
        refresh_completed(&mut edit, &queue);
        assert!(edit.pending.is_none());
        assert_eq!(edit.original.blake3, "Originalbeleg");
        assert_eq!(edit.original.bytes, 8);
        assert_eq!(edit.original.mtime, Some(7));
    }
    #[test]
    fn dirtyanzeige_erkennt_gleichgrosse_neufassung_trotz_zurueckgesetzter_mtime() {
        let area = tempfile::tempdir().unwrap();
        let path = area.path().join("Arbeitsdatei");
        std::fs::write(&path, b"alt").unwrap();
        let modified = path.metadata().unwrap().modified().unwrap();
        let original = edit_file_version(&path).unwrap();
        std::fs::write(&path, b"neu").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert!(edit_file_version(&path).as_ref() != Some(&original));
        let replacement = area.path().join("Neu");
        std::fs::write(&replacement, b"neu").unwrap();
        let before = edit_file_version(&path).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(edit_file_version(&path).as_ref() != Some(&before));
    }

    #[test]
    fn editkopie_erhaelt_originalnamen_bleibt_privat_und_bindet_eine_dateiversion() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().canonicalize().unwrap().join("Quelle");
        std::fs::write(&source, b"Original").unwrap();
        let (copy, bytes, hash) =
            copy_edit_version(&source, root.path(), "Seite.html", &Cancel::new()).unwrap();
        assert_eq!(copy.file_name().unwrap(), "Seite.html");
        assert_eq!(bytes, 8);
        assert_eq!(
            hash,
            doppeldecker_core::remote_edit::working_copy_hash(&source, &Cancel::new()).unwrap()
        );
        assert_eq!(copy.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(&source, "Später".as_bytes()).unwrap();
        assert_eq!(std::fs::read(copy).unwrap(), b"Original");
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(copy_edit_version(&source, root.path(), "Abbruch", &cancel).is_err());
    }
    #[test]
    fn editrequests_verwenden_opaque_id_ohne_frontendpfade() {
        let request: RemoteEditRequest =
            serde_json::from_value(serde_json::json!({"id":"opaque"})).unwrap();
        assert_eq!(request.id, "opaque");
    }
}
