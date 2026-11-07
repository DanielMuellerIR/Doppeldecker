//! Fenstergebundene Entpackvorbereitung; Bestandsbelege entstehen ausschließlich im Core.
use crate::{
    emit_queue_snapshot, enqueue_operation_with_readback, start_queue_worker, with_queue_extension,
    AppError, AppState, MutexExt,
};
use doppeldecker_core::archive_extract::{
    ArchiveExtractBinding, ArchiveExtractOptions, ArchiveExtractRequest, PreparedArchiveExtract,
};
use doppeldecker_core::{Cancel, QueueOperation};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tauri::{Manager, WebviewWindow};

struct Pending {
    id: String,
    request_id: String,
    created: Instant,
    request: ArchiveExtractRequest,
    held: PreparedArchiveExtract,
    upload: Option<crate::archive_upload_commands::ArchiveUploadTarget>,
}
#[derive(Default)]
pub(crate) struct ArchiveExtractCache {
    generation: u64,
    preparing: Option<(Vec<String>, Cancel)>,
    pending: Option<Pending>,
    closed: bool,
    // IPC-Abbruch darf die noch nicht registrierte eigene Anfrage überholen.
    // Die begrenzten Tombstones verhindern deren spätere Wiederbelebung.
    cancelled: std::collections::VecDeque<(String, Instant)>,
}
impl ArchiveExtractCache {
    fn begin(&mut self, id: String) -> Result<(u64, Cancel), AppError> {
        self.cancelled
            .retain(|(_, time)| time.elapsed() < Duration::from_secs(600));
        if self.cancelled.iter().any(|(cancelled, _)| cancelled == &id) {
            return Err(AppError::new(
                "interrupted",
                "Entpackvorbereitung wurde bereits abgebrochen",
            ));
        }
        if self.cancelled.len() >= 1024 {
            return Err(AppError::new(
                "resource_limit",
                "Zu viele jüngste Entpackabbrüche; bitte später erneut versuchen",
            ));
        }
        if self.closed {
            return Err(AppError::new(
                "interrupted",
                "Archivfenster wurde geschlossen",
            ));
        }
        if let Some((_, cancel)) = self.preparing.take() {
            cancel.cancel();
        }
        self.pending = None;
        self.generation = self.generation.wrapping_add(1);
        let cancel = Cancel::new();
        self.preparing = Some((vec![id], cancel.clone()));
        Ok((self.generation, cancel))
    }
    pub(crate) fn close(&mut self) {
        if let Some((_, cancel)) = self.preparing.take() {
            cancel.cancel();
        }
        self.pending = None;
        self.generation = self.generation.wrapping_add(1);
        self.closed = true;
    }
    fn discard(&mut self, id: &str) {
        if id.is_empty() || id.len() > 128 {
            return;
        }
        self.cancelled
            .retain(|(_, time)| time.elapsed() < Duration::from_secs(600));
        if self.cancelled.len() < 1024 && !self.cancelled.iter().any(|(value, _)| value == id) {
            self.cancelled.push_back((id.to_owned(), Instant::now()));
        }
        if self
            .preparing
            .as_ref()
            .is_some_and(|(current, _)| current.iter().any(|value| value == id))
        {
            if let Some((_, cancel)) = self.preparing.take() {
                cancel.cancel();
            }
            self.generation = self.generation.wrapping_add(1);
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == id || pending.request_id == id)
        {
            self.pending = None;
        }
    }
    fn publish(&mut self, generation: u64, pending: Pending) -> Result<(), AppError> {
        self.check_generation(generation)?;
        self.preparing = None;
        self.pending = Some(pending);
        Ok(())
    }
    fn check_generation(&self, generation: u64) -> Result<(), AppError> {
        if self.closed
            || generation != self.generation
            || self
                .preparing
                .as_ref()
                .is_none_or(|(_, cancel)| cancel.is_cancelled())
        {
            return Err(AppError::new(
                "interrupted",
                "Entpackvorbereitung wurde abgebrochen oder durch eine neuere Auswahl ersetzt",
            ));
        }
        Ok(())
    }
    fn bound(&self, id: &str) -> Result<&Pending, AppError> {
        self.pending.as_ref().filter(|pending| pending.id == id && pending.created.elapsed() < Duration::from_secs(600))
            .ok_or_else(|| AppError::new("plan_drift", "ZIP-Vorschau ist abgelaufen oder gehört zu einem anderen Fenster; bitte erneut vorbereiten"))
    }
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ArchiveRequest {
    request_id: String,
    provider: String,
    root: String,
    source_path: String,
    target_provider: String,
    target_dir: String,
    target: Option<crate::bidirectional_commands::BidirectionalEndpointRequest>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchivePreview {
    request_id: String,
    preparation_id: String,
    binding: ArchiveExtractBinding,
    upload_target: Option<String>,
    upload_notice: Option<String>,
}

#[tauri::command]
pub(crate) async fn prepare_archive_extract(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: ArchiveRequest,
) -> Result<ArchivePreview, AppError> {
    if request.provider != "local" {
        return Err(AppError::new("unsupported", "Entfernte ZIPs zuerst in der Archivansicht öffnen; Entpacken verwendet deren geladene Momentaufnahme."));
    }
    if request.root.is_empty() || request.target_dir.is_empty() {
        return Err(AppError::new("invalid_input", "Quelle oder Ziel zeigt keinen geöffneten Ordner. Bitte zuerst einen Ordner in beiden Panes öffnen."));
    }
    if request.request_id.is_empty() || request.request_id.len() > 128 {
        return Err(AppError::new(
            "invalid_input",
            "Ungültige Entpackanfragekennung",
        ));
    }
    crate::archive_upload_commands::validate_target(
        &request.target_provider,
        &request.target_dir,
        request.target.as_ref(),
    )?;
    let label = window.label().to_owned();
    let state = app.state::<AppState>().window(&label);
    let (generation, cancel) = state
        .archive_extract_preparation
        .lock_recover()
        .begin(request.request_id.clone())?;
    tauri::async_runtime::spawn_blocking(move || {
        let (target_dir, upload) = if let Some(target) = &request.target {
            let (directory, bound) = crate::archive_upload_commands::prepare_target(
                &app,
                &app.state::<AppState>(),
                &state,
                &label,
                target,
            )?;
            (directory, Some(bound))
        } else {
            (PathBuf::from(request.target_dir), None)
        };
        let mut intent = ArchiveExtractRequest {
            snapshot: None,
            source_root: PathBuf::from(request.root),
            source_path: PathBuf::from(request.source_path),
            target_dir,
            options: ArchiveExtractOptions::default(),
            sandbox: crate::sandbox_root()?,
            selection: Default::default(),
        };
        let held = intent.prepare(&cancel)?;
        intent.source_root = held.preview().source_root.clone();
        intent.source_path = held.preview().source_path.clone();
        intent.target_dir = held.preview().target_dir.clone();
        publish_prepared(state, generation, request.request_id, intent, held, upload)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ArchiveSelectionRequest {
    request_id: String,
    session_id: String,
    entry_id: Option<String>,
    directory_id: Option<String>,
    target_provider: String,
    target_dir: String,
    target: Option<crate::bidirectional_commands::BidirectionalEndpointRequest>,
}
#[tauri::command]
pub(crate) async fn prepare_archive_extract_selection(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: ArchiveSelectionRequest,
) -> Result<ArchivePreview, AppError> {
    crate::archive_upload_commands::validate_target(
        &request.target_provider,
        &request.target_dir,
        request.target.as_ref(),
    )?;
    if request.target_dir.is_empty()
        || request.request_id.is_empty()
        || request.request_id.len() > 128
        || request.entry_id.is_some() == request.directory_id.is_some()
    {
        return Err(AppError::new(
            "invalid_input",
            "Ungültige Archivauswahl oder kein Zielordner geöffnet",
        ));
    }
    let owner = app.state::<AppState>().window(window.label());
    let (generation, cancel) = owner
        .archive_extract_preparation
        .lock_recover()
        .begin(request.request_id.clone())?;
    tauri::async_runtime::spawn_blocking(move || {
        let source = app
            .state::<crate::archive_read_commands::ArchiveReadState>()
            .resolve_extract_selection(
                window.label(),
                &request.session_id,
                &request.request_id,
                request.entry_id.as_deref(),
                request.directory_id.as_deref(),
                &cancel,
            )?;
        let (target_dir, upload) = if let Some(target) = &request.target {
            let (directory, bound) = crate::archive_upload_commands::prepare_target(
                &app,
                &app.state::<AppState>(),
                &owner,
                window.label(),
                target,
            )?;
            (directory, Some(bound))
        } else {
            (PathBuf::from(request.target_dir), None)
        };
        let mut intent = ArchiveExtractRequest {
            snapshot: source.snapshot,
            source_root: source.root,
            source_path: source.path,
            target_dir,
            selection: source.selection,
            options: ArchiveExtractOptions::default(),
            sandbox: crate::sandbox_root()?,
        };
        let held = if let Some(snapshot) = source.validated {
            intent.prepare_with_snapshot(snapshot, &cancel)?
        } else {
            intent.prepare(&cancel)?
        };
        held.verify_selection_source(source.digest)?;
        intent.target_dir = held.preview().target_dir.clone();
        publish_prepared(owner, generation, request.request_id, intent, held, upload)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}
fn publish_prepared(
    state: std::sync::Arc<crate::WindowState>,
    generation: u64,
    request_id: String,
    intent: ArchiveExtractRequest,
    held: PreparedArchiveExtract,
    upload: Option<crate::archive_upload_commands::ArchiveUploadTarget>,
) -> Result<ArchivePreview, AppError> {
    let binding = held.binding()?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| AppError::new("app", error.to_string()))?;
    let id = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let preview = ArchivePreview {
        request_id: request_id.clone(),
        preparation_id: id.clone(),
        binding: binding.clone(),
        upload_target: upload.as_ref().map(|target| target.label.clone()),
        upload_notice: upload.as_ref().and_then(|target| target.notice.clone()),
    };
    state.archive_extract_preparation.lock_recover().publish(
        generation,
        Pending {
            id,
            request_id,
            created: Instant::now(),
            request: intent,
            held,
            upload,
        },
    )?;
    let weak = std::sync::Arc::downgrade(&state);
    let expiry_id = preview.preparation_id.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(600)).await;
        if let Some(state) = weak.upgrade() {
            state
                .archive_extract_preparation
                .lock_recover()
                .discard(&expiry_id);
        }
    });
    Ok(preview)
}
#[tauri::command]
pub(crate) async fn confirm_archive_extract(
    app: tauri::AppHandle,
    window: WebviewWindow,
    preparation_id: String,
    overwrite: bool,
    allow_partial: bool,
) -> Result<String, AppError> {
    let owner = app.state::<AppState>().window(window.label());
    let (pending, generation, cancel) = {
        let mut cache = owner.archive_extract_preparation.lock_recover();
        cache.bound(&preparation_id)?;
        let pending = cache.pending.take().expect("gebundene Vorbereitung");
        let (generation, cancel) = cache.begin(preparation_id)?;
        cache
            .preparing
            .as_mut()
            .expect("reservierter Start")
            .0
            .push(pending.request_id.clone());
        (pending, generation, cancel)
    };
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let mut request = pending.request;
        request.options = ArchiveExtractOptions {
            overwrite,
            allow_partial,
        };
        let held = pending
            .held
            .with_options(request.options.clone(), &cancel)?;
        let binding = held.binding()?;
        let (queue, entry) = with_queue_extension(&state, &owner, || {
            let mut cache = owner.archive_extract_preparation.lock_recover();
            cache.check_generation(generation)?;
            let result = if let Some(upload) = &pending.upload {
                crate::queue_runtime::ensure_profile_bindings_current(&app, &upload.profiles)?;
                let operation = upload.operation(
                    doppeldecker_core::archive_upload::ArchiveGeneration::Extract {
                        request: Box::new(request),
                        expected: Box::new(binding),
                    },
                );
                crate::queue_runtime::enqueue_operation_with_profile_bindings(
                    &app,
                    &owner,
                    label,
                    &operation,
                    &upload.profiles,
                )?
            } else {
                let operation = QueueOperation::ArchiveExtract {
                    request: Box::new(request),
                    expected: Box::new(binding),
                };
                enqueue_operation_with_readback(&app, &owner, label, &operation)?
            };
            cache.preparing = None;
            Ok(result)
        })?;
        emit_queue_snapshot(&app, label, &queue);
        start_queue_worker(app.clone(), label.to_owned());
        Ok(entry.operation_id)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}
#[tauri::command]
pub(crate) fn discard_archive_extract(
    app: tauri::AppHandle,
    window: WebviewWindow,
    preparation_id: String,
) {
    app.state::<AppState>()
        .window(window.label())
        .archive_extract_preparation
        .lock_recover()
        .discard(&preparation_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auswahl_request_akzeptiert_camelcase_aber_keine_frontend_quellpfade() {
        let json = serde_json::json!({"requestId":"request", "sessionId":"session", "entryId":"entry", "targetProvider":"local", "targetDir":"/artificial/target"});
        let request: ArchiveSelectionRequest = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(request.entry_id.as_deref(), Some("entry"));
        let mut injected = json;
        injected["sourcePath"] = serde_json::json!("/other/archive.zip");
        assert!(serde_json::from_value::<ArchiveSelectionRequest>(injected).is_err());
    }
    fn pending(id: &str, request_id: &str) -> (tempfile::TempDir, Pending) {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("source");
        let target = root.join("target");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(
            source.join("a.zip"),
            [
                80, 75, 3, 4, 20, 0, 0, 0, 0, 0, 250, 113, 87, 93, 194, 65, 36, 53, 3, 0, 0, 0, 3,
                0, 0, 0, 5, 0, 0, 0, 97, 46, 116, 120, 116, 97, 98, 99, 80, 75, 1, 2, 20, 3, 20, 0,
                0, 0, 0, 0, 250, 113, 87, 93, 194, 65, 36, 53, 3, 0, 0, 0, 3, 0, 0, 0, 5, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 128, 1, 0, 0, 0, 0, 97, 46, 116, 120, 116, 80, 75, 5, 6, 0,
                0, 0, 0, 1, 0, 1, 0, 51, 0, 0, 0, 38, 0, 0, 0, 0, 0,
            ],
        )
        .unwrap();
        let request = ArchiveExtractRequest {
            snapshot: None,
            source_root: source.clone(),
            source_path: source.join("a.zip"),
            target_dir: target,
            options: Default::default(),
            sandbox: None,
            selection: Default::default(),
        };
        let held = request.prepare(&Cancel::new()).unwrap();
        (
            temp,
            Pending {
                id: id.into(),
                request_id: request_id.into(),
                created: Instant::now(),
                request,
                held,
                upload: None,
            },
        )
    }
    #[test]
    fn extract_cancel_before_registration_and_after_publish_drops_exact_binding() {
        let mut cache = ArchiveExtractCache::default();
        cache.discard("early");
        assert!(cache.begin("early".into()).is_err());
        let (generation, cancel) = cache.begin("request-a".into()).unwrap();
        cache.discard("foreign");
        assert!(!cancel.is_cancelled());
        let (_temp, ready) = pending("plan-a", "request-a");
        cache.publish(generation, ready).unwrap();
        assert!(ArchiveExtractCache::default().bound("plan-a").is_err());
        cache.discard("request-a");
        assert!(cache.pending.is_none());
    }
    #[test]
    fn extract_late_a_cannot_replace_b_and_window_close_cancels_confirmation() {
        let mut cache = ArchiveExtractCache::default();
        let (a, cancel_a) = cache.begin("request-a".into()).unwrap();
        let (b, _) = cache.begin("request-b".into()).unwrap();
        assert!(cancel_a.is_cancelled());
        let (_temp_b, ready_b) = pending("plan-b", "request-b");
        cache.publish(b, ready_b).unwrap();
        let (_temp_a, ready_a) = pending("plan-a", "request-a");
        assert!(cache.publish(a, ready_a).is_err());
        assert!(cache.bound("plan-b").is_ok());
        cache.pending.as_mut().unwrap().created -= Duration::from_secs(601);
        assert!(cache.bound("plan-b").is_err());
        let (generation, cancel) = cache.begin("plan-c".into()).unwrap();
        cache.preparing.as_mut().unwrap().0.push("request-c".into());
        cache.discard("request-c");
        assert!(cancel.is_cancelled());
        assert!(cache.check_generation(generation).is_err());
        let (_, cancel) = cache.begin("last".into()).unwrap();
        cache.close();
        assert!(cancel.is_cancelled());
        assert!(cache.begin("closed".into()).is_err());
    }
    #[test]
    fn extract_request_uses_camel_case_and_rejects_client_proofs_and_policy() {
        let request = serde_json::json!({"requestId":"request-a","provider":"local","root":"/source","sourcePath":"/source/a.zip","targetProvider":"local","targetDir":"/target"});
        assert!(serde_json::from_value::<ArchiveRequest>(request.clone()).is_ok());
        for field in ["digest", "overwrite", "allowPartial", "binding"] {
            let mut forged = request.clone();
            forged[field] = serde_json::json!(true);
            assert!(serde_json::from_value::<ArchiveRequest>(forged).is_err());
        }
    }
}
