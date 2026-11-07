//! Fenstergebundene, nicht persistierbare lokale und Remote-Stapelumbenennung.

use std::collections::HashMap;
use std::sync::Mutex;

use doppeldecker_core::batch_rename::{
    BatchRenameOptions, BatchRenameReport, BatchRenameRow, PreparedBatchRename,
};
use doppeldecker_core::ftp_batch_rename::{
    validate_ftp_batch_rename_request, PreparedFtpBatchRename,
};
use doppeldecker_core::sftp_batch_rename::{
    validate_sftp_batch_rename_request, PreparedSftpBatchRename,
};
use doppeldecker_core::smb_batch_rename::{
    validate_smb_batch_rename_request, PreparedSmbBatchRename,
};
use doppeldecker_core::webdav_batch_rename::{
    validate_webdav_batch_rename_request, PreparedWebDavBatchRename,
};
use doppeldecker_core::{Cancel, RemoteProfileBinding, RenameExecutionClass};
use tauri::{Manager, WebviewWindow};

use crate::bidirectional_commands::BidirectionalEndpointRequest;
use crate::{AppError, AppState, MutexExt};

enum BatchPlan {
    Local(PreparedBatchRename),
    Sftp {
        plan: PreparedSftpBatchRename,
        profiles: Vec<RemoteProfileBinding>,
    },
    Ftp {
        plan: PreparedFtpBatchRename,
        profiles: Vec<RemoteProfileBinding>,
    },
    WebDav {
        plan: PreparedWebDavBatchRename,
        profiles: Vec<RemoteProfileBinding>,
    },
    Smb {
        plan: PreparedSmbBatchRename,
        profiles: Vec<RemoteProfileBinding>,
    },
}
impl BatchPlan {
    fn rows(&self) -> &[BatchRenameRow] {
        match self {
            Self::Local(plan) => plan.rows(),
            Self::Sftp { plan, .. } => plan.rows(),
            Self::Ftp { plan, .. } => plan.rows(),
            Self::WebDav { plan, .. } => plan.rows(),
            Self::Smb { plan, .. } => plan.rows(),
        }
    }
    fn is_expired(&self) -> bool {
        match self {
            Self::Local(plan) => plan.is_expired(),
            Self::Sftp { plan, .. } => plan.is_expired(),
            Self::Ftp { plan, .. } => plan.is_expired(),
            Self::WebDav { plan, .. } => plan.is_expired(),
            Self::Smb { plan, .. } => plan.is_expired(),
        }
    }
    fn execution_class(&self) -> Option<RenameExecutionClass> {
        match self {
            Self::Local(_) => None,
            Self::Sftp { plan, .. } => Some(plan.execution_class()),
            Self::Ftp { plan, .. } => Some(plan.execution_class()),
            Self::WebDav { plan, .. } => Some(plan.execution_class()),
            Self::Smb { plan, .. } => Some(plan.execution_class()),
        }
    }
    fn limitations(&self) -> Vec<String> {
        match self {
            Self::Local(_) => Vec::new(),
            Self::Sftp { plan, .. } => plan.limitations().to_vec(),
            Self::Ftp { plan, .. } => plan.limitations().to_vec(),
            Self::WebDav { plan, .. } => plan.limitations().to_vec(),
            Self::Smb { plan, .. } => plan.limitations().to_vec(),
        }
    }
    fn profiles(&self) -> &[RemoteProfileBinding] {
        match self {
            Self::Local(_) => &[],
            Self::Sftp { profiles, .. } => profiles,
            Self::Ftp { profiles, .. } => profiles,
            Self::WebDav { profiles, .. } => profiles,
            Self::Smb { profiles, .. } => profiles,
        }
    }
}
fn profile_check_error(error: AppError) -> std::io::Error {
    let code = serde_json::from_value(serde_json::Value::String(error.code))
        .unwrap_or(doppeldecker_core::ErrorCode::Io);
    let coded = doppeldecker_core::coded_error(std::io::ErrorKind::Other, code, error.message);
    doppeldecker_core::error::with_recovery_paths(coded, &error.recovery_paths)
}
fn new_id() -> Result<String, AppError> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| AppError::new("app", error.to_string()))?;
    Ok(nonce.iter().map(|byte| format!("{byte:02x}")).collect())
}
struct PendingBatch {
    id: String,
    request_id: String,
    plan: BatchPlan,
    cancel: Cancel,
}
struct RunningBatch {
    id: String,
    cancel: Cancel,
}
struct PreparingBatch {
    id: String,
    generation: u64,
    cancel: Cancel,
}
#[derive(Default)]
struct WindowBatch {
    pending: Option<PendingBatch>,
    preparing: Option<PreparingBatch>,
    running: Option<RunningBatch>,
    destroyed: bool,
    generation: u64,
    cancelled: std::collections::VecDeque<(String, std::time::Instant)>,
}
#[derive(Default)]
pub(crate) struct BatchRenameState {
    windows: Mutex<HashMap<String, WindowBatch>>,
}
const LIFETIME: std::time::Duration = std::time::Duration::from_secs(600);
impl WindowBatch {
    fn remember_cancel(&mut self, id: &str) {
        self.cancelled.retain(|(_, time)| time.elapsed() < LIFETIME);
        if self.cancelled.len() < 1024 && !self.cancelled.iter().any(|(value, _)| value == id) {
            self.cancelled
                .push_back((id.into(), std::time::Instant::now()));
        }
    }
}
impl BatchRenameState {
    pub(crate) fn destroy(&self, label: &str) {
        let mut windows = self.windows.lock_recover();
        let slot = windows.entry(label.into()).or_default();
        if let Some(work) = slot.preparing.take() {
            work.cancel.cancel();
        }
        if let Some(work) = slot.pending.take() {
            work.cancel.cancel();
        }
        if let Some(work) = slot.running.take() {
            work.cancel.cancel();
        }
        slot.destroyed = true;
    }
    #[cfg(test)]
    fn begin(&self, label: &str) -> Result<u64, AppError> {
        self.begin_request(label, "test")
            .map(|(generation, _)| generation)
    }
    fn begin_request(&self, label: &str, id: &str) -> Result<(u64, Cancel), AppError> {
        if id.is_empty() || id.len() > 128 {
            return Err(AppError::new(
                "invalid_input",
                "Ungültige Stapelanfragekennung",
            ));
        }
        let mut windows = self.windows.lock_recover();
        let slot = windows.entry(label.into()).or_default();
        slot.cancelled.retain(|(_, time)| time.elapsed() < LIFETIME);
        if slot.destroyed || slot.cancelled.iter().any(|(value, _)| value == id) {
            return Err(AppError::new(
                "interrupted",
                "Stapelvorbereitung wurde bereits geschlossen oder abgebrochen",
            ));
        }
        if slot.cancelled.len() >= 1024 {
            return Err(AppError::new(
                "resource_limit",
                "Zu viele jüngste Stapelabbrüche; bitte später erneut versuchen",
            ));
        }
        if slot.running.is_some() {
            return Err(AppError::new("busy", "Ein Stapel wird noch ausgeführt"));
        }
        if let Some(work) = slot.preparing.take() {
            work.cancel.cancel();
        }
        if let Some(work) = slot.pending.take() {
            work.cancel.cancel();
        }
        slot.generation = slot
            .generation
            .checked_add(1)
            .ok_or_else(|| AppError::new("app", "Die Vorschaugeneration ist erschöpft"))?;
        let cancel = Cancel::new();
        slot.preparing = Some(PreparingBatch {
            id: id.into(),
            generation: slot.generation,
            cancel: cancel.clone(),
        });
        Ok((slot.generation, cancel))
    }
    // Die vor Workerstart registrierte Kennung übernimmt denselben Token wie
    // die Fensterreservierung. Ein früher Abbruch bleibt beim Übergang erhalten.
    fn attach_prepare_cancel(
        &self,
        label: &str,
        generation: u64,
        cancel: &Cancel,
    ) -> Result<(), AppError> {
        let mut windows = self.windows.lock_recover();
        let slot = windows
            .get_mut(label)
            .ok_or_else(|| AppError::new("interrupted", "Stapelvorbereitung wurde geschlossen"))?;
        let work = slot
            .preparing
            .as_mut()
            .filter(|work| work.generation == generation)
            .ok_or_else(|| {
                AppError::new("plan_drift", "Eine neuere Stapelvorschau wurde angefordert")
            })?;
        if slot.destroyed || work.cancel.is_cancelled() {
            cancel.cancel();
        }
        work.cancel = cancel.clone();
        if cancel.is_cancelled() {
            return Err(AppError::new(
                "interrupted",
                "Stapelvorbereitung wurde abgebrochen",
            ));
        }
        Ok(())
    }
    fn finish_preparing(&self, label: &str, generation: u64) {
        if let Some(slot) = self.windows.lock_recover().get_mut(label) {
            if slot
                .preparing
                .as_ref()
                .is_some_and(|work| work.generation == generation)
            {
                slot.preparing = None;
            }
        }
    }
    fn publish(&self, label: &str, generation: u64, pending: PendingBatch) -> Result<(), AppError> {
        let mut windows = self.windows.lock_recover();
        let slot = windows
            .get_mut(label)
            .ok_or_else(|| AppError::new("plan_drift", "Die Vorschau wurde nicht angemeldet"))?;
        if slot.destroyed {
            return Err(AppError::new(
                "interrupted",
                "Das Vorschaufenster wurde geschlossen",
            ));
        }
        let work = slot
            .preparing
            .as_ref()
            .filter(|work| work.generation == generation)
            .ok_or_else(|| {
                AppError::new(
                    "plan_drift",
                    "Eine neuere Stapelvorschau wurde bereits angefordert",
                )
            })?;
        if slot.generation != generation || work.cancel.is_cancelled() {
            return Err(AppError::new(
                "interrupted",
                "Stapelvorbereitung wurde abgebrochen oder ersetzt",
            ));
        }
        slot.preparing = None;
        slot.pending = Some(pending);
        Ok(())
    }
    fn start(&self, label: &str, id: &str, cancel: &Cancel) -> Result<BatchPlan, AppError> {
        let mut windows = self.windows.lock_recover();
        let slot = windows.get_mut(label).ok_or_else(|| {
            AppError::new("plan_drift", "Keine passende Stapelvorschau vorhanden")
        })?;
        if slot.destroyed || slot.pending.as_ref().is_none_or(|pending| pending.id != id) {
            return Err(AppError::new(
                "plan_drift",
                "Die Stapelvorschau gehört nicht mehr zu diesem Fenster; erneut vorbereiten",
            ));
        }
        if slot
            .pending
            .as_ref()
            .is_some_and(|pending| pending.plan.is_expired())
        {
            slot.pending = None;
            return Err(AppError::new(
                "plan_drift",
                "Die Stapelvorschau ist abgelaufen; bitte neu vorbereiten",
            ));
        }
        let pending = slot.pending.take().unwrap();
        if pending.cancel.is_cancelled() {
            cancel.cancel();
        }
        slot.running = Some(RunningBatch {
            id: id.into(),
            cancel: cancel.clone(),
        });
        Ok(pending.plan)
    }
    fn finish(&self, label: &str, id: &str) {
        if let Some(slot) = self.windows.lock_recover().get_mut(label) {
            if slot
                .running
                .as_ref()
                .is_some_and(|running| running.id == id)
            {
                slot.running = None;
            }
        }
    }
    fn cancel(&self, label: &str, id: &str) -> bool {
        if id.is_empty() || id.len() > 128 {
            return false;
        }
        let mut windows = self.windows.lock_recover();
        let slot = windows.entry(label.into()).or_default();
        slot.remember_cancel(id);
        if let Some(work) = slot.preparing.as_ref().filter(|work| work.id == id) {
            work.cancel.cancel();
            return true;
        }
        if let Some(work) = slot.running.as_ref().filter(|work| work.id == id) {
            work.cancel.cancel();
            return true;
        }
        if let Some(work) = slot
            .pending
            .as_ref()
            .filter(|work| work.id == id || work.request_id == id)
        {
            work.cancel.cancel();
            return true;
        }
        false
    }
    fn discard(&self, label: &str, id: &str) {
        if id.is_empty() || id.len() > 128 {
            return;
        }
        let mut windows = self.windows.lock_recover();
        let slot = windows.entry(label.into()).or_default();
        slot.remember_cancel(id);
        if let Some(work) = slot.preparing.as_ref().filter(|work| work.id == id) {
            work.cancel.cancel();
        }
        if slot
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == id || pending.request_id == id)
        {
            if let Some(work) = slot.pending.take() {
                work.cancel.cancel();
            }
        }
    }
}
struct PreparingGuard<'a> {
    state: &'a BatchRenameState,
    label: &'a str,
    generation: u64,
}
impl Drop for PreparingGuard<'_> {
    fn drop(&mut self) {
        self.state.finish_preparing(self.label, self.generation);
    }
}

struct RunningGuard<'a> {
    state: &'a BatchRenameState,
    label: &'a str,
    id: &'a str,
}
impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.state.finish(self.label, self.id);
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrepareBatchRenameRequest {
    #[serde(default)]
    request_id: Option<String>,
    endpoint: BidirectionalEndpointRequest,
    names: Vec<String>,
    options: BatchRenameOptions,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BatchRenamePreview {
    request_id: String,
    plan_id: String,
    rows: Vec<BatchRenameRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_class: Option<RenameExecutionClass>,
    limitations: Vec<String>,
}

fn validate_request(request: &PrepareBatchRenameRequest) -> Result<std::path::PathBuf, AppError> {
    match request.endpoint.provider_kind() {
        doppeldecker_core::ProviderKind::Local | doppeldecker_core::ProviderKind::Sftp => {
            validate_sftp_batch_rename_request(&request.names, &request.options)
        }
        doppeldecker_core::ProviderKind::Ftps => {
            validate_ftp_batch_rename_request(&request.names, &request.options)
        }
        doppeldecker_core::ProviderKind::WebDav => {
            validate_webdav_batch_rename_request(&request.names, &request.options)
        }
        doppeldecker_core::ProviderKind::Smb => {
            validate_smb_batch_rename_request(&request.names, &request.options)
        }
    }
    .map_err(AppError::from)?;
    crate::bidirectional_commands::validate_endpoint_request_root(&request.endpoint)
}

#[tauri::command]
pub(crate) async fn prepare_batch_rename(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: PrepareBatchRenameRequest,
) -> Result<BatchRenamePreview, AppError> {
    let root = validate_request(&request)?;
    let request_id = request.request_id.clone().map(Ok).unwrap_or_else(new_id)?;
    // Die Reihenfolge und der eigene Cancel werden vor dem Worker registriert.
    let (generation, _) = app
        .state::<BatchRenameState>()
        .begin_request(window.label(), &request_id)?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let window_state = state.window(window.label());
        let batches = app.state::<BatchRenameState>();
        let _guard = PreparingGuard {
            state: &batches,
            label: window.label(),
            generation,
        };
        crate::queue_runtime::with_reserved_cancellable_operation(
            &app,
            window.label(),
            &window_state,
            |cancel| {
                batches.attach_prepare_cancel(window.label(), generation, cancel)?;
                let plan = if request.endpoint.provider_kind()
                    == doppeldecker_core::ProviderKind::Local
                {
                    BatchPlan::Local(PreparedBatchRename::prepare(
                        &root,
                        &request.names,
                        &request.options,
                        crate::sandbox_root()?,
                    )?)
                } else {
                    let (endpoint, _, profiles) = crate::bidirectional_commands::resolve_endpoint(
                        &app,
                        &state,
                        &request.endpoint,
                    )?;
                    crate::queue_runtime::ensure_profile_bindings_current(&app, &profiles)?;
                    let queue = crate::operation_queue(&app, &window_state, window.label())?;
                    let endpoint =
                        queue.bind_provider_preparation_with_cancel(&endpoint, cancel)?;
                    if request.endpoint.provider_kind() == doppeldecker_core::ProviderKind::Smb {
                        let plan = PreparedSmbBatchRename::prepare(
                            &endpoint,
                            &request.names,
                            &request.options,
                            cancel,
                        )?;
                        BatchPlan::Smb { plan, profiles }
                    } else if request.endpoint.provider_kind()
                        == doppeldecker_core::ProviderKind::Ftps
                    {
                        let plan = PreparedFtpBatchRename::prepare(
                            &endpoint,
                            &request.names,
                            &request.options,
                            cancel,
                        )?;
                        BatchPlan::Ftp { plan, profiles }
                    } else if request.endpoint.provider_kind()
                        == doppeldecker_core::ProviderKind::WebDav
                    {
                        let plan = PreparedWebDavBatchRename::prepare(
                            &endpoint,
                            &request.names,
                            &request.options,
                            cancel,
                        )?;
                        BatchPlan::WebDav { plan, profiles }
                    } else {
                        let plan = PreparedSftpBatchRename::prepare(
                            &endpoint,
                            &request.names,
                            &request.options,
                            cancel,
                        )?;
                        BatchPlan::Sftp { plan, profiles }
                    }
                };
                let id = new_id()?;
                let preview = BatchRenamePreview {
                    request_id: request_id.clone(),
                    plan_id: id.clone(),
                    rows: plan.rows().to_vec(),
                    execution_class: plan.execution_class(),
                    limitations: plan.limitations(),
                };
                crate::with_queue_extension(&state, &window_state, || {
                    let _profile_transaction = state.profile_secret_guard.lock_recover();
                    crate::queue_runtime::ensure_profile_bindings_current(&app, plan.profiles())?;
                    batches.publish(
                        window.label(),
                        generation,
                        PendingBatch {
                            id: id.clone(),
                            request_id,
                            plan,
                            cancel: Cancel::new(),
                        },
                    )
                })?;
                let expiry_app = app.clone();
                let label = window.label().to_owned();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(LIFETIME).await;
                    expiry_app.state::<BatchRenameState>().discard(&label, &id);
                });
                Ok(preview)
            },
            |_| {},
        )?
    })
    .await
    .map_err(|_| AppError::new("app", "Stapelvorbereitung wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) async fn execute_batch_rename(
    app: tauri::AppHandle,
    window: WebviewWindow,
    plan_id: String,
) -> Result<BatchRenameReport, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>(); let window_state = state.window(window.label());
        let batches = app.state::<BatchRenameState>();
        crate::queue_runtime::with_reserved_cancellable_operation(&app, window.label(), &window_state, |cancel| {
            let plan = batches.start(window.label(), &plan_id, cancel)?;
            let _guard = RunningGuard { state: &batches, label: window.label(), id: &plan_id };
            let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
            let mut progress = |done, total| {
                if last.elapsed() >= std::time::Duration::from_millis(50) {
                    last = std::time::Instant::now();
                    let _ = crate::emit_to_window(&window, "batch-rename-progress", serde_json::json!({ "planId": plan_id, "done": done, "total": total }));
                }
            };
            let queue = crate::operation_queue(&app, &window_state, window.label())?;
            let report = match plan {
                BatchPlan::Local(plan) => plan.execute(cancel, &mut progress),
                BatchPlan::Sftp { plan, profiles } => plan.execute_with_queue(cancel,
                    &mut || crate::queue_runtime::ensure_profile_bindings_current(&app, &profiles).map_err(profile_check_error),
                    &mut progress, &queue),
                BatchPlan::Ftp { plan, profiles } => plan.execute_with_queue(cancel,
                    &mut || crate::queue_runtime::ensure_profile_bindings_current(&app, &profiles).map_err(profile_check_error),
                    &mut progress, &queue),
                BatchPlan::WebDav { plan, profiles } => plan.execute_with_queue(cancel,
                    &mut || crate::queue_runtime::ensure_profile_bindings_current(&app, &profiles).map_err(profile_check_error),
                    &mut progress, &queue),
                BatchPlan::Smb { plan, profiles } => plan.execute_with_queue(cancel,
                    &mut || crate::queue_runtime::ensure_profile_bindings_current(&app, &profiles).map_err(profile_check_error),
                    &mut progress, &queue),
            };
            crate::emit_queue_snapshot(&app, window.label(), &queue);
            Ok(report)
        }, |_| {})?
    }).await.map_err(|_| AppError::new("app", "Stapelumbenennung wurde unterbrochen; mögliche Teilwirkungen vor erneutem Versuch prüfen"))?
}

#[tauri::command]
pub(crate) fn cancel_batch_rename(
    state: tauri::State<BatchRenameState>,
    window: WebviewWindow,
    plan_id: String,
) -> bool {
    state.cancel(window.label(), &plan_id)
}
#[tauri::command]
pub(crate) fn discard_batch_rename(
    state: tauri::State<BatchRenameState>,
    window: WebviewWindow,
    plan_id: String,
) {
    state.discard(window.label(), &plan_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pending(id: &str, root: &std::path::Path) -> PendingBatch {
        std::fs::write(root.join("alt"), b"x").unwrap();
        PendingBatch {
            id: id.into(),
            request_id: format!("request-{id}"),
            plan: BatchPlan::Local(
                PreparedBatchRename::prepare(
                    root,
                    &["alt".into()],
                    &BatchRenameOptions {
                        find: "".into(),
                        replacement: "".into(),
                        prefix: "neu-".into(),
                        suffix: "".into(),
                        preserve_extension: true,
                        trim_outer_spaces: false,
                        numbering: None,
                    },
                    None,
                )
                .unwrap(),
            ),
            cancel: Cancel::new(),
        }
    }
    #[test]
    fn smb_request_und_ausfuehrungsklasse_bleiben_typisiert_ohne_providerzugriff() {
        let mut request: PrepareBatchRenameRequest = serde_json::from_value(serde_json::json!({ "requestId":"smb-request", "endpoint": { "provider": "smb", "profileId": "not-existing", "profileRevision": 41, "root": "/artificial" }, "names": ["a"], "options": { "find": "", "replacement": "", "prefix": "x", "suffix": "", "preserveExtension": true } })).unwrap();
        assert!(validate_request(&request).is_ok());
        request.options.suffix = "x".repeat(4097);
        assert_eq!(
            validate_request(&request).unwrap_err().code,
            "invalid_input"
        );
        let view = serde_json::to_value(BatchRenamePreview {
            request_id: "smb-request".into(),
            plan_id: "opaque-plan".into(),
            rows: Vec::new(),
            execution_class: Some(RenameExecutionClass::Assured),
            limitations: vec!["Quellenpfad bleibt gebunden".into()],
        })
        .unwrap();
        assert_eq!(view["requestId"], "smb-request");
        assert_eq!(view["executionClass"], "assured");
        assert!(view.get("execution_class").is_none());
        assert_eq!(view["limitations"][0], "Quellenpfad bleibt gebunden");
    }
    #[test]
    fn ftp_request_validiert_vor_profilzugriff_und_liefert_best_effort_dto() {
        let mut request: PrepareBatchRenameRequest = serde_json::from_value(serde_json::json!({ "requestId":"ftp-request", "endpoint": { "provider": "ftps", "profileId": "not-existing", "profileRevision": 21, "root": "/artificial" }, "names": ["a"], "options": { "find": "", "replacement": "", "prefix": "x", "suffix": "", "preserveExtension": true } })).unwrap();
        assert!(validate_request(&request).is_ok());
        request.options.prefix = "x".repeat(4097);
        assert_eq!(
            validate_request(&request).unwrap_err().code,
            "invalid_input"
        );
        let view = serde_json::to_value(BatchRenamePreview {
            request_id: "ftp-request".into(),
            plan_id: "opaque-plan".into(),
            rows: Vec::new(),
            execution_class: Some(RenameExecutionClass::BestEffort),
            limitations: vec!["FTP-Zielschutz bleibt Best Effort".into()],
        })
        .unwrap();
        assert_eq!(view["requestId"], "ftp-request");
        assert_eq!(view["executionClass"], "best_effort");
        assert!(view.get("execution_class").is_none());
        assert_eq!(view["limitations"][0], "FTP-Zielschutz bleibt Best Effort");
    }
    #[test]
    fn webdav_request_validiert_vor_profilzugriff_und_liefert_best_effort_dto() {
        let mut request: PrepareBatchRenameRequest = serde_json::from_value(serde_json::json!({ "requestId":"webdav-request", "endpoint": { "provider": "webdav", "profileId": "not-existing", "profileRevision": 31, "root": "/artificial" }, "names": ["a"], "options": { "find": "", "replacement": "", "prefix": "x", "suffix": "", "preserveExtension": true } })).unwrap();
        assert!(validate_request(&request).is_ok());
        request.options.prefix = "x".repeat(4097);
        assert_eq!(
            validate_request(&request).unwrap_err().code,
            "invalid_input"
        );
        let view = serde_json::to_value(BatchRenamePreview {
            request_id: "webdav-request".into(),
            plan_id: "opaque-plan".into(),
            rows: Vec::new(),
            execution_class: Some(RenameExecutionClass::BestEffort),
            limitations: vec!["Bedingte MOVE-Wirkung bleibt Best Effort".into()],
        })
        .unwrap();
        assert_eq!(view["requestId"], "webdav-request");
        assert_eq!(view["executionClass"], "best_effort");
        assert!(view.get("execution_class").is_none());
        assert_eq!(
            view["limitations"][0],
            "Bedingte MOVE-Wirkung bleibt Best Effort"
        );
    }
    #[test]
    fn nummerierungs_dto_bindet_backendnamen_und_validiert_vor_profilzugriff() {
        let mut request: PrepareBatchRenameRequest = serde_json::from_value(serde_json::json!({ "requestId":"numbering-request", "endpoint": { "provider": "webdav", "profileId": "not-existing", "profileRevision": 31, "root": "/artificial" }, "names": ["Alpha.txt", "Beta.txt"], "options": { "find": "", "replacement": "", "prefix": "", "suffix": "", "preserveExtension": true, "numbering": { "start": 0, "step": 2, "minDigits": 3, "position": "after" } } })).unwrap();
        assert!(validate_request(&request).is_ok());
        let numbering = request.options.numbering.as_ref().unwrap();
        assert_eq!(numbering.start, 0);
        assert_eq!(numbering.step, 2);
        assert_eq!(numbering.min_digits, 3);
        assert_eq!(
            serde_json::to_value(numbering).unwrap()["position"],
            "after"
        );
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        for name in &request.names {
            std::fs::write(root.join(name), b"artificial").unwrap();
        }
        let plan =
            PreparedBatchRename::prepare(&root, &request.names, &request.options, None).unwrap();
        assert_eq!(plan.rows()[0].new_name, "Alpha_000.txt");
        assert_eq!(plan.rows()[1].new_name, "Beta_002.txt");
        request.options.numbering.as_mut().unwrap().step = 0;
        assert_eq!(
            validate_request(&request).unwrap_err().code,
            "invalid_input"
        );
        request.options.numbering = None;
        assert!(serde_json::to_value(&request.options)
            .unwrap()
            .get("numbering")
            .is_none());
    }
    #[test]
    fn trim_dto_behaelt_quellnamen_und_ueberlaesst_abbildung_dem_core() {
        let mut request: PrepareBatchRenameRequest = serde_json::from_value(serde_json::json!({ "requestId":"trim-request", "endpoint": { "provider": "webdav", "profileId": "not-existing", "profileRevision": 31, "root": "/artificial" }, "names": [" Alpha.txt ", " Beta.txt "], "options": { "find": "", "replacement": "", "prefix": " X ", "suffix": " Y ", "preserveExtension": true, "trimOuterSpaces": true, "numbering": { "start": 0, "step": 1, "minDigits": 1, "position": "after" } } })).unwrap();
        assert!(request.options.trim_outer_spaces);
        assert_eq!(request.names[0], " Alpha.txt ");
        assert!(validate_request(&request).is_ok());
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        for name in &request.names {
            std::fs::write(root.join(name), b"artificial").unwrap();
        }
        let plan =
            PreparedBatchRename::prepare(&root, &request.names, &request.options, None).unwrap();
        assert_eq!(plan.rows()[0].old_name, " Alpha.txt ");
        assert_eq!(plan.rows()[0].new_name, " X Alpha Y _0.txt");
        assert_eq!(plan.rows()[1].new_name, " X Beta Y _1.txt");
        request.options.trim_outer_spaces = false;
        assert!(serde_json::to_value(&request.options)
            .unwrap()
            .get("trimOuterSpaces")
            .is_none());
    }
    #[test]
    fn profilpruefung_erhaelt_maschinencode_und_recoverypfade() {
        let mut app = AppError::new("plan_drift", "Profilrevision geändert");
        app.recovery_paths.push("/artificial/recovery".into());
        let error = profile_check_error(app);
        assert_eq!(
            doppeldecker_core::error_code(&error),
            doppeldecker_core::ErrorCode::PlanDrift
        );
        assert_eq!(
            doppeldecker_core::error::recovery_paths(&error),
            ["/artificial/recovery"]
        );
        let permission =
            profile_check_error(AppError::new("permission_denied", "Profil nicht lesbar"));
        assert_eq!(
            doppeldecker_core::error_code(&permission),
            doppeldecker_core::ErrorCode::PermissionDenied
        );
    }
    #[test]
    fn fensterende_bricht_auch_die_reservierte_vorbereitung_ab() {
        let state = BatchRenameState::default();
        let (generation, _) = state.begin_request("one", "prepare-one").unwrap();
        let (other_generation, _) = state.begin_request("two", "prepare-two").unwrap();
        let one = Cancel::new();
        let two = Cancel::new();
        state
            .attach_prepare_cancel("one", generation, &one)
            .unwrap();
        state
            .attach_prepare_cancel("two", other_generation, &two)
            .unwrap();
        state.destroy("one");
        assert!(one.is_cancelled());
        assert!(!two.is_cancelled());
        assert!(state.begin_request("one", "late").is_err());
        assert!(state
            .attach_prepare_cancel("one", generation, &Cancel::new())
            .is_err());
    }
    #[test]
    fn validator_prueft_sftp_optionen_ohne_profildatei_oder_providerzugriff() {
        let mut request: PrepareBatchRenameRequest = serde_json::from_value(serde_json::json!({ "requestId":"own-request", "endpoint": { "provider": "sftp", "profileId": "not-existing", "profileRevision": 1, "root": "/artificial" }, "names": ["a"], "options": { "find": "a", "replacement": "b", "prefix": "", "suffix": "", "preserveExtension": true } })).unwrap();
        assert_eq!(request.request_id.as_deref(), Some("own-request"));
        assert!(validate_request(&request).is_ok());
        request.options.prefix = "x".repeat(4097);
        assert_eq!(
            validate_request(&request).unwrap_err().code,
            "invalid_input"
        );
        let other: PrepareBatchRenameRequest = serde_json::from_value(serde_json::json!({ "endpoint": { "provider": "webdav", "profileId": "not-existing", "profileRevision": 1, "root": "/artificial" }, "names": ["a"], "options": { "find": "", "replacement": "", "prefix": "x", "suffix": "" } })).unwrap();
        assert!(validate_request(&other).is_ok());
    }
    #[test]
    fn vorbereitungsabbruch_vor_workerstart_und_tokenuebergang_bleibt_exakt() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let state = BatchRenameState::default();
        state.cancel("window", "before-command");
        assert!(state.begin_request("window", "before-command").is_err());
        let (generation, early) = state.begin_request("window", "a").unwrap();
        assert!(state.cancel("window", "a"));
        assert!(early.is_cancelled());
        let reserved = Cancel::new();
        assert!(state
            .attach_prepare_cancel("window", generation, &reserved)
            .is_err());
        assert!(reserved.is_cancelled());
        assert!(state
            .publish("window", generation, pending("a", &root))
            .is_err());
        let (new_generation, _) = state.begin_request("window", "b").unwrap();
        let successor = Cancel::new();
        state
            .attach_prepare_cancel("window", new_generation, &successor)
            .unwrap();
        state.cancel("window", "a");
        assert!(!successor.is_cancelled());
        state.finish_preparing("window", generation);
        state
            .publish("window", new_generation, pending("b", &root))
            .unwrap();
        assert!(state.start("other", "b", &Cancel::new()).is_err());
        assert!(state.start("window", "b", &Cancel::new()).is_ok());
    }
    #[test]
    fn batch_abbruch_vor_execute_trifft_denselben_lauf_und_nie_nachfolger() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let state = BatchRenameState::default();
        state
            .publish("a", state.begin("a").unwrap(), pending("one", &root))
            .unwrap();
        assert!(!state.cancel("b", "one"));
        assert!(state.cancel("a", "one"));
        let current = Cancel::new();
        let plan = state.start("a", "one", &current).unwrap();
        assert!(current.is_cancelled());
        let BatchPlan::Local(plan) = plan else {
            panic!("lokaler Testplan");
        };
        assert_eq!(plan.execute(&current, &mut |_, _| {}).cancelled, 1);
        state.finish("a", "one");
        let successor = Cancel::new();
        state
            .publish("a", state.begin("a").unwrap(), pending("two", &root))
            .unwrap();
        let _next = state.start("a", "two", &successor).unwrap();
        assert!(!state.cancel("a", "one"));
        assert!(!successor.is_cancelled());
        state.destroy("a");
        assert!(successor.is_cancelled());
        assert!(state.publish("a", 0, pending("late", &root)).is_err());
    }
    #[test]
    fn batch_spaeter_alter_worker_ersetzt_keine_neuere_vorschau() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let state = BatchRenameState::default();
        let generation_a = state.begin("window").unwrap();
        let generation_b = state.begin("window").unwrap();
        state
            .publish("window", generation_b, pending("b", &root))
            .unwrap();
        let error = state
            .publish("window", generation_a, pending("a", &root))
            .unwrap_err();
        assert_eq!(error.code, "plan_drift");
        state.discard("window", "a");
        assert!(state.start("window", "a", &Cancel::new()).is_err());
        let b = state.start("window", "b", &Cancel::new()).unwrap();
        assert_eq!(
            b.rows()[0].status,
            doppeldecker_core::batch_rename::BatchRenameStatus::Ready
        );
    }

    #[test]
    fn batch_request_liest_camelcase_und_remote_bleibt_backendseitig_offen() {
        let request: PrepareBatchRenameRequest = serde_json::from_value(serde_json::json!({ "endpoint": { "provider": "sftp", "profileId": "p", "profileRevision": 1, "root": "/" }, "names": ["a"], "options": { "find": "a", "replacement": "b", "prefix": "", "suffix": "", "preserveExtension": true } })).unwrap();
        assert!(validate_request(&request).is_ok());
        assert!(request.options.preserve_extension);
    }
}
