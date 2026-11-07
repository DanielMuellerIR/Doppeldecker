//! Fenstergebundene ZIP-Vorbereitung; Bestandsbelege entstehen ausschließlich im Core.
use crate::{
    emit_queue_snapshot, enqueue_operation_with_readback, start_queue_worker, with_queue_extension,
    AppError, AppState, MutexExt,
};
use doppeldecker_core::archive_pack::{
    ArchivePackBinding, ArchivePackOptions, ArchivePackRequest, PreparedArchivePack,
};
use doppeldecker_core::{Cancel, QueueOperation};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tauri::{Manager, WebviewWindow};

struct Pending {
    id: String,
    created: Instant,
    request: ArchivePackRequest,
    binding: ArchivePackBinding,
    _held: PreparedArchivePack,
    upload: Option<crate::archive_upload_commands::ArchiveUploadTarget>,
}
#[derive(Default)]
pub(crate) struct ArchivePackCache {
    generation: u64,
    preparing: Option<(String, Cancel)>,
    pending: Option<Pending>,
    closed: bool,
}
impl ArchivePackCache {
    fn begin(&mut self, id: String) -> Result<(u64, Cancel), AppError> {
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
        self.preparing = Some((id, cancel.clone()));
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
        if self
            .preparing
            .as_ref()
            .is_some_and(|(current, _)| current == id)
        {
            if let Some((_, cancel)) = self.preparing.take() {
                cancel.cancel();
            }
            self.generation = self.generation.wrapping_add(1);
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == id)
        {
            self.pending = None;
        }
    }
    fn publish(&mut self, generation: u64, pending: Pending) -> Result<(), AppError> {
        if self.closed
            || generation != self.generation
            || self
                .preparing
                .as_ref()
                .is_none_or(|(_, cancel)| cancel.is_cancelled())
        {
            return Err(AppError::new(
                "interrupted",
                "ZIP-Vorbereitung wurde abgebrochen oder durch eine neuere Auswahl ersetzt",
            ));
        }
        self.preparing = None;
        self.pending = Some(pending);
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
    items: Vec<String>,
    target_name: String,
    options: ArchivePackOptions,
    target: Option<crate::bidirectional_commands::BidirectionalEndpointRequest>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchivePreview {
    preparation_id: String,
    binding: ArchivePackBinding,
    upload_target: Option<String>,
    upload_notice: Option<String>,
}

#[tauri::command]
pub(crate) async fn prepare_archive_pack(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: ArchiveRequest,
) -> Result<ArchivePreview, AppError> {
    if request.provider != "local" {
        return Err(AppError::new("unsupported", "ZIP-Erzeugung ist derzeit für lokale Panes verfügbar. Entfernte Archive können hier noch nicht erstellt werden."));
    }
    if request.request_id.is_empty() || request.request_id.len() > 128 {
        return Err(AppError::new(
            "invalid_input",
            "Ungültige ZIP-Anfragekennung",
        ));
    }
    let label = window.label().to_owned();
    let state = app.state::<AppState>().window(&label);
    let (generation, cancel) = state
        .archive_pack_preparation
        .lock_recover()
        .begin(request.request_id.clone())?;
    tauri::async_runtime::spawn_blocking(move || {
        request.options.validate()?;
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
            (PathBuf::from(&request.root), None)
        };
        let mut intent = ArchivePackRequest {
            source_root: PathBuf::from(&request.root),
            target_dir,
            items: request.items.into_iter().map(PathBuf::from).collect(),
            target_name: request.target_name,
            options: request.options,
            sandbox: crate::sandbox_root()?,
        };
        let held = intent.prepare(&cancel)?;
        intent.source_root = held.preview().source_root.clone();
        intent.target_dir = held
            .preview()
            .target
            .parent()
            .expect("geprüfter ZIP-Zielordner")
            .to_path_buf();
        intent.items = intent
            .items
            .iter()
            .map(|path| {
                intent
                    .source_root
                    .join(path.file_name().expect("direkte Auswahl"))
            })
            .collect();
        let binding = held.binding()?;
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(|error| AppError::new("app", error.to_string()))?;
        let id = nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let preview = ArchivePreview {
            preparation_id: id.clone(),
            binding: binding.clone(),
            upload_target: upload.as_ref().map(|target| target.label.clone()),
            upload_notice: upload.as_ref().and_then(|target| target.notice.clone()),
        };
        state.archive_pack_preparation.lock_recover().publish(
            generation,
            Pending {
                id,
                created: Instant::now(),
                request: intent,
                binding,
                _held: held,
                upload,
            },
        )?;
        Ok(preview)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}
#[tauri::command]
pub(crate) async fn confirm_archive_pack(
    app: tauri::AppHandle,
    window: WebviewWindow,
    preparation_id: String,
) -> Result<String, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let owner = state.window(label);
        let (queue, entry) = with_queue_extension(&state, &owner, || {
            let mut cache = owner.archive_pack_preparation.lock_recover();
            let pending = cache.bound(&preparation_id)?;
            let result = if let Some(upload) = &pending.upload {
                crate::queue_runtime::ensure_profile_bindings_current(&app, &upload.profiles)?;
                let operation =
                    upload.operation(doppeldecker_core::archive_upload::ArchiveGeneration::Pack {
                        request: Box::new(pending.request.clone()),
                        expected: Box::new(pending.binding.clone()),
                    });
                crate::queue_runtime::enqueue_operation_with_profile_bindings(
                    &app,
                    &owner,
                    label,
                    &operation,
                    &upload.profiles,
                )?
            } else {
                let operation = QueueOperation::ArchivePack {
                    request: Box::new(pending.request.clone()),
                    expected: Box::new(pending.binding.clone()),
                };
                enqueue_operation_with_readback(&app, &owner, label, &operation)?
            };
            cache.pending = None;
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
pub(crate) fn discard_archive_pack(
    app: tauri::AppHandle,
    window: WebviewWindow,
    preparation_id: String,
) {
    app.state::<AppState>()
        .window(window.label())
        .archive_pack_preparation
        .lock_recover()
        .discard(&preparation_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pending(id: &str) -> (tempfile::TempDir, Pending) {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::write(root.join("file"), b"content").unwrap();
        let request = ArchivePackRequest {
            source_root: root.clone(),
            target_dir: root.clone(),
            items: vec![root.join("file")],
            target_name: "new.zip".into(),
            options: Default::default(),
            sandbox: None,
        };
        let held = request.prepare(&Cancel::new()).unwrap();
        let binding = held.binding().unwrap();
        (
            temp,
            Pending {
                id: id.into(),
                created: Instant::now(),
                request,
                binding,
                _held: held,
                upload: None,
            },
        )
    }
    #[test]
    fn archive_pack_cache_binds_window_generation_expiry_and_cancel() {
        let mut cache = ArchivePackCache::default();
        let (generation, cancel) = cache.begin("request-1".into()).unwrap();
        cache.discard("other");
        assert!(!cancel.is_cancelled());
        let (_temp, ready) = pending("backend-id");
        cache.publish(generation, ready).unwrap();
        assert!(cache.bound("backend-id").is_ok());
        assert!(ArchivePackCache::default().bound("backend-id").is_err());
        assert!(cache.bound("request-1").is_err());
        cache.pending.as_mut().unwrap().created -= Duration::from_secs(601);
        assert!(cache.bound("backend-id").is_err());
        let (old, old_cancel) = cache.begin("old".into()).unwrap();
        let (_new, new_cancel) = cache.begin("new".into()).unwrap();
        assert!(old_cancel.is_cancelled());
        let (_temp, late) = pending("late");
        assert!(cache.publish(old, late).is_err());
        cache.close();
        assert!(new_cancel.is_cancelled());
        assert!(cache.begin("closed".into()).is_err());
    }
    #[test]
    fn archive_pack_nested_request_accepts_camel_case_and_rejects_forged_binding() {
        let request = serde_json::json!({"requestId":"request-1","provider":"local","root":"/pane","items":["/pane/file"],"targetName":"new.zip","options":{"compression":"deflate"}});
        assert!(serde_json::from_value::<ArchiveRequest>(request.clone()).is_ok());
        let mut forged = request.clone();
        forged["digest"] = serde_json::json!(vec![0; 32]);
        assert!(serde_json::from_value::<ArchiveRequest>(forged).is_err());
        let mut invalid = request;
        invalid["options"]["compression"] = serde_json::json!("unsupported");
        assert!(serde_json::from_value::<ArchiveRequest>(invalid).is_err());
    }
}
