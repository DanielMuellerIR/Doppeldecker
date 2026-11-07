//! Lokales Duplizieren: angezeigte Aliasnamen bleiben bis zum Queuecommit im Fenster gebunden.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use doppeldecker_core::{fileops, FileQueueKind, LocalVfs, QueueOperation};
use tauri::{Manager, WebviewWindow};

use crate::{
    emit_queue_snapshot, enqueue_operation_with_readback, start_queue_worker, with_queue_extension,
    AppError, AppState, MutexExt,
};

struct PreparedDuplicate {
    id: String,
    created: Instant,
    parent: PathBuf,
    items: Vec<PathBuf>,
    targets: Vec<fileops::FileKeepBothTarget>,
    vfs: LocalVfs,
}

#[derive(Default)]
pub(crate) struct DuplicateCache {
    generation: u64,
    pending: Option<PreparedDuplicate>,
    closed: bool,
}
impl DuplicateCache {
    pub(crate) fn close(&mut self) {
        self.closed = true;
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
    }

    fn begin(&mut self) -> Result<u64, AppError> {
        if self.closed {
            return Err(AppError::new(
                "interrupted",
                "Das Duplizierfenster wurde geschlossen",
            ));
        }
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
        Ok(self.generation)
    }
    fn publish(&mut self, generation: u64, pending: PreparedDuplicate) -> Result<(), AppError> {
        if self.closed {
            return Err(AppError::new(
                "interrupted",
                "Das Duplizierfenster wurde geschlossen",
            ));
        }
        if self.generation != generation {
            return Err(AppError::new(
                "plan_drift",
                "Eine neuere Duplizierauswahl hat diese Vorbereitung abgelöst",
            ));
        }
        self.pending = Some(pending);
        Ok(())
    }
    fn discard(&mut self, id: &str) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == id)
        {
            self.pending = None;
        }
    }

    fn bound(&self, id: &str) -> Result<&PreparedDuplicate, AppError> {
        self.pending.as_ref().filter(|pending| pending.id == id
            && pending.created.elapsed() < Duration::from_secs(600))
            .ok_or_else(|| AppError::new("plan_drift", "Die Dupliziervorbereitung ist abgelaufen oder gehört zu einem anderen Fenster; bitte erneut vorbereiten"))
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DuplicateRequest {
    provider: String,
    root: String,
    items: Vec<String>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DuplicateMapping {
    source_name: String,
    target_name: String,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DuplicatePreview {
    supported: bool,
    preparation_id: Option<String>,
    parent: String,
    mappings: Vec<DuplicateMapping>,
    blockers: Vec<String>,
}

fn prepare(
    request: DuplicateRequest,
) -> Result<(DuplicatePreview, Option<PreparedDuplicate>), AppError> {
    if request.provider != "local" {
        if !["sftp", "ftps", "webdav", "smb"].contains(&request.provider.as_str()) {
            return Err(AppError::new(
                "invalid_input",
                "Unbekannter Duplizierprovider",
            ));
        }
        return Ok((DuplicatePreview { supported: false, preparation_id: None,
            parent: request.root, mappings: Vec::new(), blockers: vec![
                "Duplizieren ist derzeit für lokale Dateien und Ordner umgesetzt. Entfernte Duplikate sind noch nicht verfügbar; normale Kopieraktionen bleiben erreichbar.".into()] }, None));
    }
    let lexical = request.items.iter().map(PathBuf::from).collect::<Vec<_>>();
    let (parent, items) =
        fileops::bind_local_file_operation_scope(Path::new(&request.root), &lexical)?;
    fileops::guard(&crate::sandbox_root()?, &parent)?;
    let vfs = LocalVfs::rooted(&parent)?;
    let targets = fileops::prepare_local_duplicate_targets(&vfs, &parent, &items)?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| AppError::new("app", error.to_string()))?;
    let id = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mappings = targets
        .iter()
        .map(|target| DuplicateMapping {
            source_name: Path::new(&target.source)
                .file_name()
                .expect("geprüfter Quellname")
                .to_string_lossy()
                .into_owned(),
            target_name: Path::new(&target.target)
                .file_name()
                .expect("geprüfter Zielname")
                .to_string_lossy()
                .into_owned(),
        })
        .collect();
    let preview = DuplicatePreview {
        supported: true,
        preparation_id: Some(id.clone()),
        parent: parent.to_string_lossy().into_owned(),
        mappings,
        blockers: Vec::new(),
    };
    Ok((
        preview,
        Some(PreparedDuplicate {
            id,
            created: Instant::now(),
            parent,
            items,
            targets,
            vfs,
        }),
    ))
}

#[tauri::command]
pub(crate) async fn prepare_duplicate(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: DuplicateRequest,
) -> Result<DuplicatePreview, AppError> {
    let window_state = app.state::<AppState>().window(window.label());
    let generation = window_state.duplicate_preparation.lock_recover().begin()?;
    tauri::async_runtime::spawn_blocking(move || {
        let (preview, pending) = prepare(request)?;
        if let Some(pending) = pending {
            window_state
                .duplicate_preparation
                .lock_recover()
                .publish(generation, pending)?;
        }
        Ok(preview)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn confirm_duplicate(
    app: tauri::AppHandle,
    window: WebviewWindow,
    preparation_id: String,
) -> Result<String, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        let (queue, entry) = with_queue_extension(&state, &window_state, || {
            let mut slot = window_state.duplicate_preparation.lock_recover();
            let pending = slot.bound(&preparation_id)?;
            fileops::verify_keep_both_targets(
                &pending.vfs,
                &pending.vfs,
                &pending.items,
                &pending.parent,
                &pending.targets,
            )?;
            let operation = QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: pending
                    .items
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
                destination: pending.parent.to_string_lossy().into_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                keep_both_targets: pending.targets.clone(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            };
            let result = enqueue_operation_with_readback(&app, &window_state, label, &operation)?;
            slot.pending = None;
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
pub(crate) fn discard_duplicate(
    app: tauri::AppHandle,
    window: WebviewWindow,
    preparation_id: String,
) {
    app.state::<AppState>()
        .window(window.label())
        .duplicate_preparation
        .lock_recover()
        .discard(&preparation_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dupliziervorschau_bindet_exakte_namen_fenster_generation_und_ablauf() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let original = root.join("Original.txt");
        std::fs::write(&original, b"Original").unwrap();
        let (preview, pending) = prepare(DuplicateRequest {
            provider: "local".into(),
            root: root.to_str().unwrap().into(),
            items: vec![original.to_str().unwrap().into()],
        })
        .unwrap();
        let pending = pending.unwrap();
        assert_eq!(preview.mappings[0].source_name, "Original.txt");
        assert_eq!(
            preview.mappings[0].target_name,
            Path::new(&pending.targets[0].target)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        );
        assert!(!Path::new(&pending.targets[0].target).exists());
        let id = preview.preparation_id.unwrap();
        let mut owner = DuplicateCache::default();
        let generation = owner.begin().unwrap();
        owner.publish(generation, pending).unwrap();
        assert!(owner.bound(&id).is_ok());
        assert!(DuplicateCache::default().bound(&id).is_err());
        assert!(owner.bound("fremde-id").is_err());
        owner.pending.as_mut().unwrap().created = Instant::now() - Duration::from_secs(601);
        assert!(owner.bound(&id).is_err());
        let stale = owner.pending.take().unwrap();
        owner.begin().unwrap();
        assert!(owner.publish(generation, stale).is_err());
        assert_eq!(std::fs::read(original).unwrap(), b"Original");
    }

    #[test]
    fn geschlossenes_duplizierfenster_verwirft_pending_und_spaeten_worker() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let original = root.join("Original.txt");
        std::fs::write(&original, b"Original").unwrap();
        let preparation = || {
            prepare(DuplicateRequest {
                provider: "local".into(),
                root: root.to_str().unwrap().into(),
                items: vec![original.to_str().unwrap().into()],
            })
            .unwrap()
            .1
            .unwrap()
        };
        let mut owner = DuplicateCache::default();
        let late_generation = owner.begin().unwrap();
        let late = preparation();
        let generation = owner.begin().unwrap();
        let pending = preparation();
        let id = pending.id.clone();
        owner.publish(generation, pending).unwrap();
        assert!(owner.bound(&id).is_ok());
        owner.close();
        assert!(owner.pending.is_none());
        assert!(owner.bound(&id).is_err());
        assert_ne!(owner.generation, late_generation);
        assert_eq!(
            owner.publish(late_generation, late).unwrap_err().code,
            "interrupted"
        );
        assert!(owner.pending.is_none());
        assert_eq!(owner.begin().unwrap_err().code, "interrupted");
        owner.close();
        assert!(owner.pending.is_none());
        assert_eq!(std::fs::read(original).unwrap(), b"Original");
    }

    #[test]
    fn duplizier_discard_entfernt_nur_exakt_seinen_plan_und_ist_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let original = root.join("Original.txt");
        std::fs::write(&original, b"Original").unwrap();
        let preparation = || {
            prepare(DuplicateRequest {
                provider: "local".into(),
                root: root.to_str().unwrap().into(),
                items: vec![original.to_str().unwrap().into()],
            })
            .unwrap()
            .1
            .unwrap()
        };
        let mut owner = DuplicateCache::default();
        let generation_a = owner.begin().unwrap();
        let a = preparation();
        let id_a = a.id.clone();
        owner.publish(generation_a, a).unwrap();
        let generation_b = owner.begin().unwrap();
        let b = preparation();
        let id_b = b.id.clone();
        owner.publish(generation_b, b).unwrap();
        owner.discard(&id_a);
        owner.discard("fremde-id");
        assert!(owner.bound(&id_b).is_ok());
        owner.discard(&id_b);
        assert!(owner.pending.is_none());
        assert!(owner.bound(&id_b).is_err());
        owner.discard(&id_b);
        assert!(owner.pending.is_none());
    }

    #[test]
    fn remote_duplizieren_liefert_backendvertrag_ohne_lokalen_pfadzugriff() {
        for provider in ["sftp", "ftps", "webdav", "smb"] {
            let (preview, pending) = prepare(DuplicateRequest {
                provider: provider.into(),
                root: "/kein-lokaler-pfad".into(),
                items: vec!["/kein-lokaler-pfad/Datei".into()],
            })
            .unwrap();
            assert!(!preview.supported);
            assert!(preview.preparation_id.is_none() && pending.is_none());
            assert!(!preview.blockers.is_empty());
        }
    }
    #[test]
    fn verschachtelte_duplizierpayloads_akzeptieren_genau_frontendschreibweise() {
        let request =
            serde_json::json!({"provider":"local", "root":"/quelle", "items":["/quelle/A"]});
        assert!(serde_json::from_value::<DuplicateRequest>(request).is_ok());
        let forged = serde_json::json!({"provider":"local", "root":"/quelle", "items":["/quelle/A"], "targets":["/fremd"]});
        assert!(serde_json::from_value::<DuplicateRequest>(forged).is_err());
    }
}
