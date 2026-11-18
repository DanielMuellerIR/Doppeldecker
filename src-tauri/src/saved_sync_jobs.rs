//! Prozessweit gespeicherte Einweg-Sync-Konfigurationen.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

use doppeldecker_core::{FavoriteProvider, SyncOptions};
use tauri::WebviewWindow;

use crate::desktop_settings::{read_config_bytes, write_config_bytes, MAX_CONFIG_BYTES};
use crate::{app_config_dir, AppError, MutexExt};

const FILE_NAME: &str = "saved-sync-jobs.json";
const VERSION: u32 = 1;
// Der GUI-Prozess hält bereits den gemeinsamen Prozesslock. Diese Sperre schützt
// den vollständigen Lese-/Änderungs-/Schreibvorgang zwischen seinen Fenstern.
static JOBS_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SyncJobEndpoint {
    provider: FavoriteProvider,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_id: Option<String>,
    path: String,
}

impl SyncJobEndpoint {
    fn validate(&self) -> Result<(), AppError> {
        let path = Path::new(&self.path);
        let normalized =
            doppeldecker_core::validate_file_operation_root(path).map_err(AppError::from)?;
        if normalized != path || self.path.contains('\0') {
            return Err(AppError::new(
                "invalid_input",
                "Der Ordnerpfad muss absolut und lexikalisch normalisiert sein",
            ));
        }

        match (&self.provider, self.profile_id.as_deref()) {
            (FavoriteProvider::Local, None) => Ok(()),
            (FavoriteProvider::Local, Some(_)) => Err(AppError::new(
                "invalid_input",
                "Ein lokaler Ordner darf keine Profil-ID enthalten",
            )),
            (_, Some(id))
                if !id.is_empty()
                    && id.len() <= 64
                    && id.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
                    }) =>
            {
                Ok(())
            }
            _ => Err(AppError::new(
                "invalid_input",
                "Ein Remote-Ordner braucht eine gültige Profil-ID",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum SyncJobDirection {
    Lr,
    Rl,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SaveSyncJobRequest {
    id: Option<String>,
    revision: Option<u64>,
    name: String,
    left: SyncJobEndpoint,
    right: SyncJobEndpoint,
    direction: SyncJobDirection,
    options: SyncOptions,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SavedSyncJob {
    id: String,
    revision: u64,
    name: String,
    left: SyncJobEndpoint,
    right: SyncJobEndpoint,
    direction: SyncJobDirection,
    options: SyncOptions,
}

impl SavedSyncJob {
    fn validate(&self) -> Result<(), AppError> {
        if self.name.trim().is_empty() || self.name.chars().any(char::is_control) {
            return Err(AppError::new(
                "invalid_input",
                "Der Sync-Auftrag braucht einen gültigen Namen",
            ));
        }
        self.left.validate()?;
        self.right.validate()?;
        // Die lokale Mirror-Vorschau erlaubt den Schnellvergleich. Erst der
        // Übergang zur bestätigten Ausführung verlangt den Inhaltsvergleich.
        self.options.validate().map_err(AppError::from)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncJobDocument {
    version: u32,
    jobs: Vec<SavedSyncJob>,
}

fn load_jobs(path: &Path) -> Result<SyncJobDocument, AppError> {
    let Some(bytes) = read_config_bytes(path, "Sync-Auftragsdatei")? else {
        return Ok(SyncJobDocument {
            version: VERSION,
            jobs: Vec::new(),
        });
    };
    let document: SyncJobDocument = serde_json::from_slice(&bytes).map_err(|error| {
        AppError::new(
            "invalid_data",
            format!("Sync-Aufträge konnten nicht gelesen werden: {error}"),
        )
    })?;
    if document.version != VERSION {
        return Err(AppError::new(
            "invalid_data",
            "Unbekanntes Dateiformat der Sync-Aufträge",
        ));
    }
    let mut ids = BTreeSet::new();
    for job in &document.jobs {
        if job.id.len() != 32
            || !job.id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !ids.insert(&job.id)
            || job.revision == 0
        {
            return Err(AppError::new(
                "invalid_data",
                "Ungültige ID oder Revision in der Sync-Auftragsdatei",
            ));
        }
        job.validate()?;
    }
    Ok(document)
}

fn store_job(
    path: &Path,
    request: SaveSyncJobRequest,
) -> Result<(SavedSyncJob, Option<AppError>), AppError> {
    let _guard = JOBS_LOCK.lock_recover();
    let mut document = load_jobs(path)?;
    let (id, revision, index) = match (request.id, request.revision) {
        (None, None) => {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(|error| AppError::new("io", error.to_string()))?;
            let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
            if document.jobs.iter().any(|job| job.id == id) {
                return Err(AppError::new(
                    "conflict",
                    "Die neue Auftrags-ID kollidiert. Erneut speichern.",
                ));
            }
            (id, 1, None)
        }
        (Some(id), Some(revision)) => {
            let index = document
                .jobs
                .iter()
                .position(|job| job.id == id)
                .ok_or_else(|| AppError::new("not_found", "Der Sync-Auftrag fehlt"))?;
            if document.jobs[index].revision != revision {
                return Err(AppError::new(
                    "conflict",
                    "Der Sync-Auftrag wurde inzwischen geändert. Öffnen Sie ihn erneut.",
                ));
            }
            let next_revision = revision
                .checked_add(1)
                .ok_or_else(|| AppError::new("invalid_data", "Ungültige Auftragsrevision"))?;
            (id, next_revision, Some(index))
        }
        _ => {
            return Err(AppError::new(
                "invalid_input",
                "Beim Bearbeiten sind Auftrags-ID und Revision erforderlich",
            ));
        }
    };
    let job = SavedSyncJob {
        id,
        revision,
        name: request.name.trim().to_owned(),
        left: request.left,
        right: request.right,
        direction: request.direction,
        options: request.options,
    };
    job.validate()?;
    if let Some(index) = index {
        document.jobs[index] = job.clone();
    } else {
        document.jobs.push(job.clone());
    }
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| AppError::new("io", error.to_string()))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(AppError::new(
            "invalid_input",
            "Sync-Auftragsdatei überschreitet die zulässige Größe",
        ));
    }
    let warning = write_config_bytes(path, &bytes)?;
    Ok((job, warning))
}

#[tauri::command]
pub(crate) async fn list_saved_sync_jobs(
    app: tauri::AppHandle,
) -> Result<Vec<SavedSyncJob>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let path = app_config_dir(&app)?.join(FILE_NAME);
        let _guard = JOBS_LOCK.lock_recover();
        Ok(load_jobs(&path)?.jobs)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn save_sync_job(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: SaveSyncJobRequest,
) -> Result<SavedSyncJob, AppError> {
    let (job, warning) = tauri::async_runtime::spawn_blocking(move || {
        store_job(&app_config_dir(&app)?.join(FILE_NAME), request)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))??;
    if let Some(warning) = warning {
        let _ = crate::emit_to_window(
            &window,
            "settings-error",
            AppError::new(
                &warning.code,
                format!(
                    "Der Sync-Auftrag wurde gespeichert. Der Konfigurationsordner konnte nicht synchronisiert werden: {}",
                    warning.message
                ),
            ),
        );
    }
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn gui_request() -> serde_json::Value {
        json!({
            "name": "Ordner abgleichen",
            "left": {"provider": "sftp", "profileId": "test-profile", "path": "/remote"},
            "right": {"provider": "local", "path": "/nicht-benoetigter-ordner"},
            "direction": "rl",
            "options": {
                "mode": "Update", "compare": "ContentHash",
                "metadata": "Portable", "mtime_tolerance": 2, "exclude": ["*.tmp"]
            }
        })
    }

    #[test]
    fn gui_request_survives_reopen_and_stale_edit_preserves_current_revision() {
        let area = tempfile::tempdir().unwrap();
        let path = area.path().join(FILE_NAME);
        assert!(load_jobs(&path).unwrap().jobs.is_empty());

        let (job, warning) =
            store_job(&path, serde_json::from_value(gui_request()).unwrap()).unwrap();
        assert!(warning.is_none());
        assert_eq!(job.revision, 1);
        let reopened = load_jobs(&path).unwrap().jobs;
        assert_eq!(reopened, vec![job.clone()]);
        let mut expected = gui_request();
        expected["id"] = json!(job.id);
        expected["revision"] = json!(1);
        assert_eq!(serde_json::to_value(&reopened[0]).unwrap(), expected);

        let mut edit = expected;
        edit["name"] = json!("Neuer Name");
        edit["left"] = json!({"provider": "local", "path": "/lokale-quelle"});
        edit["options"]["mode"] = json!("Mirror");
        edit["options"]["compare"] = json!("SizeAndTime");
        let (updated, _) = store_job(&path, serde_json::from_value(edit.clone()).unwrap()).unwrap();
        assert_eq!(updated.id, job.id);
        assert_eq!(updated.revision, 2);

        edit["name"] = json!("Veralteter Name");
        let stale = store_job(&path, serde_json::from_value(edit).unwrap()).unwrap_err();
        assert_eq!(stale.code, "conflict");
        assert_eq!(load_jobs(&path).unwrap().jobs, vec![updated]);
    }

    #[test]
    fn damaged_or_oversized_document_is_not_overwritten() {
        let area = tempfile::tempdir().unwrap();
        let path = area.path().join(FILE_NAME);
        for bytes in [
            b"ungueltiges JSON".to_vec(),
            vec![b' '; MAX_CONFIG_BYTES as usize + 1],
        ] {
            std::fs::write(&path, &bytes).unwrap();
            assert!(store_job(&path, serde_json::from_value(gui_request()).unwrap()).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }
}
