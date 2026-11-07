//! Eigene wirkungsgebundene Forward-Klasse; strikte lokale Journale bleiben getrennt.
use crate::bidirectional_commands::{
    self as registry, BidirectionalEndpointRequest, BoundRegistryEndpoint,
    PrepareBidirectionalRequest, RegisteredBidirectionalPair,
};
use crate::content_sync_sandbox::Binding;
use crate::{
    emit_queue_snapshot, operation_queue, start_queue_worker, with_queue_extension, AppError,
    AppState, MutexExt,
};
use doppeldecker_core::bidirectional::forward::{
    ForwardBidirectionalPreview, PreparedForwardBidirectional,
};
use doppeldecker_core::queue::PreparedBidirectionalForwardQueueRun;
use doppeldecker_core::{
    BidirectionalQueueEndpoint, Cancel, ConflictResolution, OperationQueue, PairId, Progress,
    ProviderEndpoint, QueueEntry, QueueExecutionResult, QueueOperation, QueueOperationClass,
    RemoteProfileBinding,
};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Manager, WebviewWindow};

use crate::frontend_payload::FrontendQueueSnapshot;
pub(crate) struct PendingForwardRun {
    prepared: PreparedForwardBidirectional,
    operation: QueueOperation,
    sandbox: Option<PathBuf>,
    test_bindings: Vec<Binding>,
    pub(crate) profile_bindings: Vec<RemoteProfileBinding>,
}
enum QueuePreparation {
    Initial(Box<PreparedForwardBidirectional>),
    Ready(Box<PreparedBidirectionalForwardQueueRun>),
}
pub(crate) struct QueuedForwardRun {
    prepared: QueuePreparation,
    sandbox: Option<PathBuf>,
    test_bindings: Vec<Binding>,
    effects_confirmed: bool,
    removal_phrases: Vec<String>,
    pub(crate) profile_bindings: Vec<RemoteProfileBinding>,
}
impl QueuedForwardRun {
    fn preview(&self) -> &ForwardBidirectionalPreview {
        match &self.prepared {
            QueuePreparation::Initial(p) => p.preview(),
            QueuePreparation::Ready(p) => p.preview(),
        }
    }
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForwardPrepareRequest {
    #[serde(flatten)]
    request: PrepareBidirectionalRequest,
    #[serde(default)]
    recovery: bool,
}
fn backend_error(error: AppError) -> std::io::Error {
    let code = serde_json::from_value(serde_json::Value::String(error.code))
        .unwrap_or(doppeldecker_core::ErrorCode::Io);
    doppeldecker_core::error::with_recovery_paths(
        doppeldecker_core::coded_error(std::io::ErrorKind::Other, code, error.message),
        &error.recovery_paths,
    )
}
fn bound_endpoint(
    request: BidirectionalEndpointRequest,
    endpoint: &ProviderEndpoint,
    cancel: &Cancel,
) -> Result<BoundRegistryEndpoint, AppError> {
    let (binding,compatible_bindings)=doppeldecker_core::bidirectional::forward::bind_forward_bidirectional_endpoint_compatibility(endpoint,cancel).map_err(AppError::from)?;
    Ok(BoundRegistryEndpoint {
        endpoint: registry::endpoint_with_bound_root(request, &binding)?,
        binding,
        compatible_bindings,
    })
}
#[tauri::command]
pub(crate) async fn register_forward_bidirectional_pair(
    app: tauri::AppHandle,
    window: WebviewWindow,
    left: BidirectionalEndpointRequest,
    right: BidirectionalEndpointRequest,
) -> Result<RegisteredBidirectionalPair, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let ws = state.window(label);
        crate::queue_runtime::with_reserved_cancellable_operation(
            &app,
            label,
            &ws,
            |cancel| {
                let (_, left) = registry::normalize_registry_endpoint(left)?;
                let (_, right) = registry::normalize_registry_endpoint(right)?;
                crate::content_sync_commands::validate_sandbox_endpoints(
                    left.provider_kind(),
                    left.root(),
                    right.provider_kind(),
                    right.root(),
                )?;
                let ((lp, _, lb), (rp, _, rb)) = registry::resolve_validated_registry_pair(
                    &left,
                    &right,
                    |e| registry::validate_registry_profile(&app, e),
                    |e| registry::resolve_endpoint(&app, &state, e),
                )?;
                let bindings = crate::queue_runtime::merge_profile_bindings([lb, rb]);
                crate::queue_runtime::ensure_profile_bindings_current(&app, &bindings)?;
                let left = bound_endpoint(left, &lp, cancel)?;
                let right = bound_endpoint(right, &rp, cancel)?;
                registry::register_bidirectional_pair_at(
                    &mut state.bidirectional_pairs.lock_recover(),
                    &registry::pair_registry_path(&app)?,
                    left,
                    right,
                )
            },
            |_| {},
        )?
    })
    .await
    .map_err(|_| AppError::new("app", "Paarregistrierung wurde unterbrochen"))?
}
struct Resolved {
    left: ProviderEndpoint,
    right: ProviderEndpoint,
    left_queue: Box<BidirectionalQueueEndpoint>,
    right_queue: Box<BidirectionalQueueEndpoint>,
    bindings: Vec<RemoteProfileBinding>,
    baseline: PathBuf,
    sandbox: Option<PathBuf>,
    test_bindings: Vec<Binding>,
}
fn resolve_registered(
    app: &tauri::AppHandle,
    state: &AppState,
    request: &PrepareBidirectionalRequest,
    cancel: &Cancel,
) -> Result<Resolved, AppError> {
    let (_, left) = registry::normalize_registry_endpoint(request.left.clone())?;
    let (_, right) = registry::normalize_registry_endpoint(request.right.clone())?;
    if left != request.left || right != request.right {
        return Err(AppError::new(
            "plan_drift",
            "Die Paarwurzeln wurden inzwischen geändert",
        ));
    }
    let sandbox = crate::content_sync_commands::validate_sandbox_endpoints(
        left.provider_kind(),
        left.root(),
        right.provider_kind(),
        right.root(),
    )?;
    let ((lp, lq, lb), (rp, rq, rb)) = registry::resolve_validated_registry_pair(
        &left,
        &right,
        |e| registry::validate_registry_profile(app, e),
        |e| registry::resolve_endpoint(app, state, e),
    )?;
    let left_binding =
        doppeldecker_core::bidirectional::forward::bind_forward_bidirectional_endpoint(&lp, cancel)
            .map_err(AppError::from)?;
    let right_binding =
        doppeldecker_core::bidirectional::forward::bind_forward_bidirectional_endpoint(&rp, cancel)
            .map_err(AppError::from)?;
    registry::validate_registered_pair_at(
        &mut state.bidirectional_pairs.lock_recover(),
        &registry::pair_registry_path(app)?,
        request,
        &left_binding,
        &right_binding,
    )?;
    let test_bindings = prepare_test_bindings(sandbox.as_deref(), &lp, &rp, cancel)?;
    Ok(Resolved {
        left: lp,
        right: rp,
        left_queue: lq,
        right_queue: rq,
        bindings: crate::queue_runtime::merge_profile_bindings([lb, rb]),
        baseline: registry::baseline_path(app, request.pair_id)?,
        sandbox,
        test_bindings,
    })
}
fn prepare_test_bindings(
    sandbox: Option<&std::path::Path>,
    left: &ProviderEndpoint,
    right: &ProviderEndpoint,
    cancel: &Cancel,
) -> Result<Vec<Binding>, AppError> {
    let mut bindings = Vec::new();
    for endpoint in [left, right] {
        if let Some(binding) = Binding::prepare(sandbox, endpoint, cancel)? {
            bindings.push(binding);
        }
    }
    Ok(bindings)
}
fn check_test_bindings(
    sandbox: &Option<PathBuf>,
    bindings: &[Binding],
    cancel: &Cancel,
) -> Result<(), AppError> {
    if crate::sandbox_root()? != *sandbox {
        return Err(AppError::new(
            "sandbox_violation",
            "Der markierte Testbereich änderte sich seit der Vorschau",
        ));
    }
    for binding in bindings {
        binding.validate(cancel)?;
    }
    Ok(())
}
#[tauri::command]
pub(crate) async fn prepare_forward_bidirectional_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: ForwardPrepareRequest,
) -> Result<ForwardBidirectionalPreview, AppError> {
    tauri::async_runtime::spawn_blocking(move||{
        let state=app.state::<AppState>();let label=window.label();let ws=state.window(label);
        crate::queue_runtime::with_reserved_cancellable_operation(&app,label,&ws,|cancel|{
            if ws.pending_forward_bidirectional.lock_recover().is_some(){return Err(AppError::new("busy","Eine bidirektionale Vorbereitung ist noch vorhanden"));}
            ws.forward_bidirectional_preparing.store(true,std::sync::atomic::Ordering::SeqCst);
            let generation=ws.forward_bidirectional_generation.fetch_add(1,std::sync::atomic::Ordering::SeqCst)+1;
            let _=crate::emit_to_window(&window,"sync-preparation-started","Bidirektionalen Bestand und Wirkungen beider Seiten prüfen");
            let result=(||{
                let r=resolve_registered(&app,&state,&request.request,cancel)?;
                let queue=operation_queue(&app,&ws,label)?;
                let left=queue.bind_provider_preparation(&r.left).map_err(AppError::from)?;let right=queue.bind_provider_preparation(&r.right).map_err(AppError::from)?;
                let prepared=doppeldecker_core::bidirectional::forward::prepare_forward_bidirectional_for_queue(&queue,request.request.pair_id,left,right,&r.baseline,&request.request.resolutions,request.recovery,request.request.expected_unresolved_digest,cancel).map_err(AppError::from)?;
                let preview=prepared.preview().clone();
                if cancel.is_cancelled()||ws.forward_bidirectional_generation.load(std::sync::atomic::Ordering::SeqCst)!=generation{prepared.discard().map_err(AppError::from)?;return Err(AppError::new("interrupted","Bidirektionale Vorbereitung wurde verworfen"));}
                let operation=QueueOperation::BidirectionalForward{pair_id:request.request.pair_id,pair_name:request.request.pair_name,left:r.left_queue,right:r.right_queue,baseline_path:r.baseline.to_string_lossy().into_owned(),resolutions:request.request.resolutions,expected_baseline_digest:preview.plan.baseline_digest,expected_plan_digest:preview.plan.plan_digest,expected_unresolved_digest:preview.unresolved_digest,expected_preview_digest:preview.digest};
                let _profiles=state.profile_secret_guard.lock_recover();if let Err(error)=crate::queue_runtime::ensure_profile_bindings_current(&app,&r.bindings){prepared.discard().map_err(AppError::from)?;return Err(error);}
                let mut slot=ws.pending_forward_bidirectional.lock_recover();
                if ws.forward_bidirectional_generation.load(std::sync::atomic::Ordering::SeqCst)!=generation{prepared.discard().map_err(AppError::from)?;return Err(AppError::new("plan_drift","Die Vorbereitung wurde inzwischen verworfen"));}
                *slot=Some(PendingForwardRun{prepared,operation,sandbox:r.sandbox,test_bindings:r.test_bindings,profile_bindings:r.bindings});Ok(preview)
            })();
            ws.forward_bidirectional_preparing.store(false,std::sync::atomic::Ordering::SeqCst);
            let _=crate::emit_to_window(&window,"sync-preparation-done",());result
        },|_|{})?
    }).await.map_err(|_|AppError::new("app","Bidirektionale Vorbereitung wurde unterbrochen"))?
}
#[tauri::command]
pub(crate) fn discard_forward_bidirectional_sync(
    window: WebviewWindow,
    state: tauri::State<AppState>,
) -> Result<(), AppError> {
    discard_window_preparation(&state.window(window.label()))
}

fn discard_window_preparation(ws: &crate::queue_runtime::WindowState) -> Result<(), AppError> {
    let mut slot = ws.pending_forward_bidirectional.lock_recover();
    ws.forward_bidirectional_generation
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if ws
        .forward_bidirectional_preparing
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        if let Some(cancel) = ws.active_cancel.lock_recover().as_ref() {
            cancel.cancel();
        }
    }
    if let Some(pending) = slot.take() {
        pending.prepared.discard().map_err(AppError::from)?;
    }
    Ok(())
}
fn check_confirmation(
    preview: &ForwardBidirectionalPreview,
    digest: [u8; 32],
    confirmed: bool,
) -> Result<(), AppError> {
    if preview.digest != digest {
        return Err(AppError::new(
            "plan_drift",
            "Eine neuere bidirektionale Vorschau muss bestätigt werden",
        ));
    }
    if !confirmed {
        return Err(AppError::new(
            "confirmation_mismatch",
            "Die Wirkung beider Seiten wurde nicht bestätigt",
        ));
    }
    Ok(())
}
#[tauri::command]
pub(crate) async fn enqueue_forward_bidirectional_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    preparation_digest: [u8; 32],
    effects_confirmed: bool,
    removal_phrases: Vec<String>,
) -> Result<String, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let ws = state.window(label);
        let (queue, id) = with_queue_extension(&state, &ws, || {
            let mut slot = ws.pending_forward_bidirectional.lock_recover();
            let pending = slot
                .as_ref()
                .ok_or_else(|| AppError::new("plan_drift", "Bidirektionale Vorbereitung fehlt"))?;
            crate::queue_runtime::ensure_profile_bindings_current(&app, &pending.profile_bindings)?;
            check_test_bindings(&pending.sandbox, &pending.test_bindings, &Cancel::new())?;
            check_confirmation(
                pending.prepared.preview(),
                preparation_digest,
                effects_confirmed,
            )?;
            let mut runs = ws.forward_bidirectional_runs.lock_recover();
            let (queue, entry) = crate::queue_runtime::enqueue_operation_with_profile_bindings(
                &app,
                &ws,
                label,
                &pending.operation,
                &pending.profile_bindings,
            )?;
            let pending = slot.take().expect("zuvor geprüft");
            let id = entry.operation_id;
            runs.insert(
                id.clone(),
                QueuedForwardRun {
                    prepared: QueuePreparation::Initial(Box::new(pending.prepared)),
                    sandbox: pending.sandbox,
                    test_bindings: pending.test_bindings,
                    profile_bindings: pending.profile_bindings,
                    effects_confirmed,
                    removal_phrases,
                },
            );
            Ok((queue, id))
        })?;
        emit_queue_snapshot(&app, label, &queue);
        start_queue_worker(app.clone(), label.to_owned());
        Ok(id)
    })
    .await
    .map_err(|_| AppError::new("app", "Bidirektionaler Queueübergang wurde unterbrochen"))?
}
#[tauri::command]
pub(crate) fn preview_forward_bidirectional_conflicts(
    app: tauri::AppHandle,
    window: WebviewWindow,
    resolutions: Vec<ConflictResolution>,
    expected_unresolved_digest: [u8; 32],
    operation_id: Option<String>,
) -> Result<Vec<doppeldecker_core::bidirectional::BidirectionalConflictView>, AppError> {
    let state = app.state::<AppState>();
    let ws = state.window(window.label());
    if let Some(id) = operation_id {
        let runs = ws.forward_bidirectional_runs.lock_recover();
        let p = runs
            .get(&id)
            .ok_or_else(|| AppError::new("plan_drift", "Queue-Konfliktvorbereitung fehlt"))?;
        match &p.prepared {
            QueuePreparation::Initial(v) => {
                v.conflict_choices_after_decisions(&resolutions, expected_unresolved_digest)
            }
            QueuePreparation::Ready(v) => {
                v.conflict_choices_after_decisions(&resolutions, expected_unresolved_digest)
            }
        }
        .map_err(AppError::from)
    } else {
        let pending = ws.pending_forward_bidirectional.lock_recover();
        pending
            .as_ref()
            .ok_or_else(|| AppError::new("plan_drift", "Konfliktvorbereitung fehlt"))?
            .prepared
            .conflict_choices_after_decisions(&resolutions, expected_unresolved_digest)
            .map_err(AppError::from)
    }
}
#[tauri::command]
pub(crate) fn forward_bidirectional_pair_status(
    app: tauri::AppHandle,
    pair_id: PairId,
) -> Result<
    Option<doppeldecker_core::bidirectional::forward::ForwardBidirectionalJournalStatus>,
    AppError,
> {
    doppeldecker_core::bidirectional::forward::forward_bidirectional_journal_status(
        &registry::baseline_path(&app, pair_id)?,
    )
    .map_err(AppError::from)
}
#[tauri::command]
pub(crate) async fn finish_forward_bidirectional_pair_completion(
    app: tauri::AppHandle,
    window: WebviewWindow,
    pair_id: PairId,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let ws = state.window(window.label());
        crate::queue_runtime::with_reserved_operation(&app, window.label(), &ws, || {
            doppeldecker_core::bidirectional::forward::finish_forward_bidirectional_completion(
                &registry::baseline_path(&app, pair_id)?,
            )
            .map_err(AppError::from)
        })
    })
    .await
    .map_err(|_| AppError::new("app", "Baselineabschluss wurde unterbrochen"))?
}
#[tauri::command]
pub(crate) fn forward_bidirectional_queue_preview(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
) -> Result<ForwardBidirectionalPreview, AppError> {
    let state = app.state::<AppState>();
    let ws = state.window(window.label());
    let runs = ws.forward_bidirectional_runs.lock_recover();
    let pending = runs.get(&operation_id).ok_or_else(|| {
        AppError::new(
            "plan_drift",
            "Bidirektionale Queuevorbereitung fehlt; neu abgleichen",
        )
    })?;
    crate::queue_runtime::ensure_profile_bindings_current(&app, &pending.profile_bindings)?;
    check_test_bindings(&pending.sandbox, &pending.test_bindings, &Cancel::new())?;
    Ok(pending.preview().clone())
}
#[tauri::command]
pub(crate) async fn prepare_forward_bidirectional_queue_recovery(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
    resolutions: Vec<ConflictResolution>,
    expected_unresolved_digest: Option<[u8; 32]>,
) -> Result<ForwardBidirectionalPreview, AppError> {
    tauri::async_runtime::spawn_blocking(move||{
        let state=app.state::<AppState>();let label=window.label();let ws=state.window(label);
        crate::queue_runtime::with_reserved_cancellable_operation(&app,label,&ws,|cancel|{
            let queue=operation_queue(&app,&ws,label)?;let entry=queue.snapshot().entries.into_iter().find(|e|e.operation_id==operation_id).ok_or_else(||AppError::new("plan_drift","Queueeintrag fehlt"))?;
            crate::queue_runtime::ensure_profile_bindings_current(&app,&entry.profile_bindings)?;
            check_operation_sandbox(&entry.operation)?;
            if let Some(previous)=ws.forward_bidirectional_runs.lock_recover().remove(&operation_id){
                match previous.prepared{QueuePreparation::Initial(p)=>p.discard().map_err(AppError::from)?,QueuePreparation::Ready(p)=>doppeldecker_core::queue::discard_prepared_bidirectional_forward_queue_entry(&queue,*p).map_err(AppError::from)?};
            }
            let _=crate::emit_to_window(&window,"sync-preparation-started","Beide Seiten gegen die letzte gemeinsame Baseline frisch abgleichen; alte Entscheidungen werden verworfen");
            let result=(||{
                let prepared=if let Some(digest)=expected_unresolved_digest{
                    doppeldecker_core::queue::reprepare_bidirectional_forward_queue_entry_decisions(&queue,&operation_id,&resolutions,digest,Arc::new(crate::sftp_commands::KeyringResolver),cancel)
                }else{
                    if !resolutions.is_empty(){return Err(AppError::new("plan_drift","Frische Konfliktentscheidungen brauchen den Digest ihrer Vorschau"));}
                    doppeldecker_core::queue::prepare_bidirectional_forward_queue_recovery(&queue,&operation_id,&resolutions,None,Arc::new(crate::sftp_commands::KeyringResolver),cancel)
                }.map_err(AppError::from)?;
                let preview=prepared.preview().clone();store_ready(&app,&state,&ws,&queue,&entry,prepared,cancel)?;Ok(preview)
            })();
            let _=crate::emit_to_window(&window,"sync-preparation-done",());
            if let Err(error)=&result{let io=backend_error(error.clone());let _=queue.interrupt_bidirectional_forward(&operation_id,Progress::default(),&io);}
            emit_queue_snapshot(&app,label,&queue);result
        },|_|{})?
    }).await.map_err(|_|AppError::new("app","Bidirektionaler Neuabgleich wurde unterbrochen"))?
}
#[tauri::command]
pub(crate) async fn approve_forward_bidirectional_queue_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
    preparation_digest: [u8; 32],
    effects_confirmed: bool,
    removal_phrases: Vec<String>,
) -> Result<FrontendQueueSnapshot, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let ws = state.window(label);
        let queue = operation_queue(&app, &ws, label)?;
        with_queue_extension(&state, &ws, || {
            let mut runs = ws.forward_bidirectional_runs.lock_recover();
            let pending = runs.get_mut(&operation_id).ok_or_else(|| {
                AppError::new("plan_drift", "Bidirektionale Queuevorbereitung fehlt")
            })?;
            crate::queue_runtime::ensure_profile_bindings_current(&app, &pending.profile_bindings)?;
            check_test_bindings(&pending.sandbox, &pending.test_bindings, &Cancel::new())?;
            check_confirmation(pending.preview(), preparation_digest, effects_confirmed)?;
            let QueuePreparation::Ready(prepared) = &pending.prepared else {
                return Err(AppError::new(
                    "plan_drift",
                    "Dieser Lauf ist noch nicht frisch für die Queue vorbereitet",
                ));
            };
            queue
                .confirm_bidirectional_forward_preparation(&operation_id, prepared)
                .map_err(AppError::from)?;
            pending.effects_confirmed = effects_confirmed;
            pending.removal_phrases = removal_phrases;
            Ok(())
        })?;
        emit_queue_snapshot(&app, label, &queue);
        start_queue_worker(app.clone(), label.to_owned());
        Ok(queue.snapshot().into())
    })
    .await
    .map_err(|_| AppError::new("app", "Bidirektionale Zustimmung wurde unterbrochen"))?
}
#[tauri::command]
pub(crate) fn discard_forward_bidirectional_queue_preparation(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
) -> Result<(), AppError> {
    let state = app.state::<AppState>();
    let ws = state.window(window.label());
    let queue = operation_queue(&app, &ws, window.label())?;
    discard_queue_slot(&ws, &queue, &operation_id, false)?;
    emit_queue_snapshot(&app, window.label(), &queue);
    Ok(())
}
#[tauri::command]
pub(crate) fn finish_forward_bidirectional_queue_completion(
    app: tauri::AppHandle,
    window: WebviewWindow,
    operation_id: String,
) -> Result<FrontendQueueSnapshot, AppError> {
    let state = app.state::<AppState>();
    let ws = state.window(window.label());
    let queue = operation_queue(&app, &ws, window.label())?;
    with_queue_extension(&state, &ws, || {
        doppeldecker_core::queue::finish_bidirectional_forward_queue_completion(
            &queue,
            &operation_id,
        )
        .map_err(AppError::from)
    })?;
    emit_queue_snapshot(&app, window.label(), &queue);
    Ok(queue.snapshot().into())
}
fn check_operation_sandbox(operation: &QueueOperation) -> Result<Option<PathBuf>, AppError> {
    let QueueOperation::BidirectionalForward { left, right, .. } = operation else {
        return Err(AppError::new(
            "invalid_input",
            "Kein bidirektionaler Auftrag",
        ));
    };
    crate::content_sync_commands::validate_sandbox_endpoints(
        crate::content_sync_commands::queue_endpoint_kind(left),
        left.root(),
        crate::content_sync_commands::queue_endpoint_kind(right),
        right.root(),
    )
}
fn store_ready(
    app: &tauri::AppHandle,
    state: &AppState,
    ws: &crate::queue_runtime::WindowState,
    queue: &OperationQueue,
    entry: &QueueEntry,
    prepared: PreparedBidirectionalForwardQueueRun,
    cancel: &Cancel,
) -> Result<(), AppError> {
    let sandbox = check_operation_sandbox(&entry.operation)?;
    let test_bindings = prepare_test_bindings(
        sandbox.as_deref(),
        prepared.left_endpoint(),
        prepared.right_endpoint(),
        cancel,
    )?;
    let _profiles = state.profile_secret_guard.lock_recover();
    if let Err(error) =
        crate::queue_runtime::ensure_profile_bindings_current(app, &entry.profile_bindings)
    {
        let original = error;
        doppeldecker_core::queue::discard_prepared_bidirectional_forward_queue_entry(
            queue, prepared,
        )
        .map_err(AppError::from)?;
        return Err(original);
    }
    ws.forward_bidirectional_runs.lock_recover().insert(
        entry.operation_id.clone(),
        QueuedForwardRun {
            prepared: QueuePreparation::Ready(Box::new(prepared)),
            sandbox,
            test_bindings,
            profile_bindings: entry.profile_bindings.clone(),
            effects_confirmed: false,
            removal_phrases: Vec::new(),
        },
    );
    Ok(())
}
pub(crate) fn execute_queue_entry(
    app: &tauri::AppHandle,
    label: &str,
    queue: &OperationQueue,
    entry: &QueueEntry,
    cancel: &Cancel,
    progress: &mut dyn FnMut(&Progress),
) -> std::io::Result<QueueExecutionResult> {
    let state = app.state::<AppState>();
    let ws = state.window(label);
    check_operation_sandbox(&entry.operation).map_err(backend_error)?;
    let pending = ws
        .forward_bidirectional_runs
        .lock_recover()
        .remove(&entry.operation_id);
    let Some(pending) = pending else {
        let prepared = doppeldecker_core::queue::prepare_bidirectional_forward_queue_entry(
            queue,
            entry.clone(),
            Arc::new(crate::sftp_commands::KeyringResolver),
            cancel,
        )?;
        if let Err(error) = store_ready(app, &state, &ws, queue, entry, prepared, cancel) {
            let io = backend_error(error);
            queue.interrupt_bidirectional_forward(&entry.operation_id, Progress::default(), &io)?;
            return Err(io);
        }
        return Ok(QueueExecutionResult::AwaitingConfirmation{class:QueueOperationClass::Sync,reason:"Beide Seiten wurden frisch geprüft; die neue bidirektionale Wirkung in der Queue bestätigen".into()});
    };
    crate::queue_runtime::ensure_profile_bindings_current(app, &pending.profile_bindings)
        .map_err(backend_error)?;
    check_test_bindings(&pending.sandbox, &pending.test_bindings, cancel).map_err(backend_error)?;
    let prepared = match pending.prepared {
        QueuePreparation::Initial(p) => {
            doppeldecker_core::queue::bind_prepared_bidirectional_forward_queue_entry(
                queue, entry, *p, cancel,
            )?
        }
        QueuePreparation::Ready(p) => *p,
    };
    let digest = prepared.preview().digest;
    doppeldecker_core::queue::execute_prepared_bidirectional_forward_queue_entry(
        queue,
        prepared,
        digest,
        pending.effects_confirmed,
        &pending.removal_phrases,
        cancel,
        progress,
    )
}
pub(crate) fn interrupt_failed_preparation(
    queue: &OperationQueue,
    entry: &QueueEntry,
    error: &AppError,
) {
    let io = backend_error(error.clone());
    let _ = queue.interrupt_bidirectional_forward(&entry.operation_id, entry.progress.clone(), &io);
}

pub(crate) fn invalidate_profile(
    app: &tauri::AppHandle,
    label: &str,
    ws: &crate::queue_runtime::WindowState,
    queue: Option<&OperationQueue>,
    protocol: doppeldecker_core::RemoteProfileProtocol,
    profile_id: &str,
) {
    let affected = |bindings: &[RemoteProfileBinding]| {
        bindings
            .iter()
            .any(|b| b.protocol == protocol && b.profile_id == profile_id)
    };
    let mut pending = ws.pending_forward_bidirectional.lock_recover();
    if pending
        .as_ref()
        .is_some_and(|p| affected(&p.profile_bindings))
    {
        if let Some(p) = pending.take() {
            if let Err(error) = p.prepared.discard() {
                let _ = crate::queue_runtime::emit_to_window_label(
                    app,
                    label,
                    "queue-error",
                    AppError::from(error),
                );
            }
        }
    }
    drop(pending);
    let mut runs = ws.forward_bidirectional_runs.lock_recover();
    let ids: Vec<_> = runs
        .iter()
        .filter(|(_, r)| affected(&r.profile_bindings))
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        if let Some(run) = runs.remove(&id) {
            let result = match run.prepared {
                QueuePreparation::Initial(p) => p.discard(),
                QueuePreparation::Ready(p) => {
                    if let Some(q) = queue {
                        doppeldecker_core::queue::discard_prepared_bidirectional_forward_queue_entry(
                            q, *p,
                        )
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "Fensterqueue fehlt",
                        ))
                    }
                }
            };
            if let Some(queue) = queue {
                if let Some(entry) = queue
                    .snapshot()
                    .entries
                    .into_iter()
                    .find(|entry| entry.operation_id == id)
                {
                    let drift = doppeldecker_core::coded_error(std::io::ErrorKind::Other,doppeldecker_core::ErrorCode::PlanDrift,"Das Profil änderte sich; die bidirektionale Wirkung muss neu vorbereitet werden");
                    if let Err(error) =
                        queue.interrupt_bidirectional_forward(&id, entry.progress, &drift)
                    {
                        let _ = crate::queue_runtime::emit_to_window_label(
                            app,
                            label,
                            "queue-error",
                            AppError::from(error),
                        );
                    }
                }
            }
            if let Err(error) = result {
                let _ = crate::queue_runtime::emit_to_window_label(
                    app,
                    label,
                    "queue-error",
                    AppError::from(error),
                );
            }
        }
    }
    if let Some(queue) = queue {
        emit_queue_snapshot(app, label, queue);
    }
}

pub(crate) fn discard_queue_slot(
    ws: &crate::queue_runtime::WindowState,
    queue: &OperationQueue,
    id: &str,
    cancel_intent: bool,
) -> Result<(), AppError> {
    let entry = queue
        .snapshot()
        .entries
        .into_iter()
        .find(|entry| entry.operation_id == id)
        .ok_or_else(|| AppError::new("plan_drift", "Queueeintrag fehlt"))?;
    if !matches!(entry.operation, QueueOperation::BidirectionalForward { .. }) {
        return Ok(());
    }
    let pending = ws.forward_bidirectional_runs.lock_recover().remove(id);
    let Some(pending) = pending else {
        let QueueOperation::BidirectionalForward { baseline_path, .. } = &entry.operation else {
            unreachable!()
        };
        let active =
            doppeldecker_core::bidirectional::forward::forward_bidirectional_journal_status(
                std::path::Path::new(baseline_path),
            )
            .map_err(AppError::from)?
            .is_some();
        if active || cancel_intent {
            let error = doppeldecker_core::coded_error(
                std::io::ErrorKind::Other,
                doppeldecker_core::ErrorCode::ReplanRequired,
                "Die bidirektionale Vorbereitung fehlt; den belegten Bestand zuerst neu abgleichen",
            );
            queue
                .interrupt_bidirectional_forward(id, entry.progress, &error)
                .map_err(AppError::from)?;
            return Err(AppError::from(error));
        }
        return Ok(());
    };
    match pending.prepared {
        QueuePreparation::Ready(prepared) => if cancel_intent {
            doppeldecker_core::queue::cancel_prepared_bidirectional_forward_queue_entry(
                queue, *prepared,
            )
        } else {
            doppeldecker_core::queue::discard_prepared_bidirectional_forward_queue_entry(
                queue, *prepared,
            )
        }
        .map_err(AppError::from)?,
        QueuePreparation::Initial(prepared) => {
            prepared.discard().map_err(AppError::from)?;
            let QueueOperation::BidirectionalForward { baseline_path, .. } = &entry.operation
            else {
                unreachable!()
            };
            let active =
                doppeldecker_core::bidirectional::forward::forward_bidirectional_journal_status(
                    std::path::Path::new(baseline_path),
                )
                .map_err(AppError::from)?
                .is_some();
            if active {
                let error=doppeldecker_core::coded_error(std::io::ErrorKind::Other,doppeldecker_core::ErrorCode::ReplanRequired,"Vorhandene Teilwirkungen bleiben erhalten; den bidirektionalen Bestand neu abgleichen");
                queue
                    .interrupt_bidirectional_forward(id, entry.progress, &error)
                    .map_err(AppError::from)?;
            } else if cancel_intent {
                queue.remove_pending(id).map_err(AppError::from)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forward_request_behaelt_frontend_profilrevision_und_unresolved_digest() {
        let request:ForwardPrepareRequest=serde_json::from_value(serde_json::json!({
            "pairId":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"pairName":"Paar",
            "left":{"provider":"sftp","profileId":"profil","profileRevision":7,"root":"/links"},
            "right":{"provider":"local","root":"/tmp/rechts"},"resolutions":[],
            "expectedUnresolvedDigest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"recovery":true
        })).unwrap();
        assert!(request.recovery);
        assert_eq!(request.request.expected_unresolved_digest, Some([2; 32]));
        assert_eq!(
            request.request.left.provider_kind(),
            doppeldecker_core::ProviderKind::Sftp
        );
    }
    #[test]
    fn discard_eines_fensters_verbraucht_keine_fremde_forward_vorbereitung() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let queue = OperationQueue::open(root.join("operations-queue.json")).unwrap();
        let make = |id: u8| {
            let left = root.join(format!("links-{id}"));
            let right = root.join(format!("rechts-{id}"));
            std::fs::create_dir(&left).unwrap();
            std::fs::create_dir(&right).unwrap();
            let baseline = root.join(format!("baseline-{id}.json"));
            let prepared =
                doppeldecker_core::bidirectional::forward::prepare_forward_bidirectional_for_queue(
                    &queue,
                    PairId([id; 16]),
                    ProviderEndpoint::local(&left).unwrap(),
                    ProviderEndpoint::local(&right).unwrap(),
                    &baseline,
                    &[],
                    false,
                    None,
                    &Cancel::new(),
                )
                .unwrap();
            let preview = prepared.preview();
            let operation = QueueOperation::BidirectionalForward {
                pair_id: PairId([id; 16]),
                pair_name: "Test".into(),
                left: Box::new(BidirectionalQueueEndpoint::Local {
                    root: left.to_string_lossy().into_owned(),
                }),
                right: Box::new(BidirectionalQueueEndpoint::Local {
                    root: right.to_string_lossy().into_owned(),
                }),
                baseline_path: baseline.to_string_lossy().into_owned(),
                resolutions: Vec::new(),
                expected_baseline_digest: preview.plan.baseline_digest,
                expected_plan_digest: preview.plan.plan_digest,
                expected_unresolved_digest: preview.unresolved_digest,
                expected_preview_digest: preview.digest,
            };
            PendingForwardRun {
                prepared,
                operation,
                sandbox: None,
                test_bindings: Vec::new(),
                profile_bindings: Vec::new(),
            }
        };
        let state = AppState::default();
        let a = state.window("main");
        let b = state.window("window-2");
        *a.pending_forward_bidirectional.lock_recover() = Some(make(1));
        *b.pending_forward_bidirectional.lock_recover() = Some(make(2));
        let cancel_a = Cancel::new();
        let cancel_b = Cancel::new();
        *a.active_cancel.lock_recover() = Some(cancel_a.clone());
        *b.active_cancel.lock_recover() = Some(cancel_b.clone());
        a.forward_bidirectional_preparing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        b.forward_bidirectional_preparing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        discard_window_preparation(&a).unwrap();
        assert!(a.pending_forward_bidirectional.lock_recover().is_none());
        assert!(b.pending_forward_bidirectional.lock_recover().is_some());
        assert!(cancel_a.is_cancelled());
        assert!(!cancel_b.is_cancelled());
        assert_eq!(
            b.forward_bidirectional_generation
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        discard_window_preparation(&b).unwrap();
    }
}
