//! Tauri-Commands für die journalisierte Sync-Quarantäne.

use std::path::PathBuf;

use doppeldecker_core::{
    expand_home_path, lease_quarantine_run_for_target, list_quarantine_entries_for_target,
    list_quarantine_runs as list_core_quarantine_runs, purge_quarantine_run_for_target_with_lease,
    restore_quarantine_entry_for_target_with_lease, ProviderEndpoint, QuarantineEntry,
    QuarantineLease, QuarantineRestoreOutcome, QuarantineRestoreResult, QuarantineRun,
    QuarantineScope,
};
use serde::Serialize;
use tauri::{Manager, WebviewWindow};

use crate::{
    bidirectional_commands::{resolve_endpoint, BidirectionalEndpointRequest},
    sandbox_root, with_reserved_operation, AppError, AppState, MutexExt, WindowState,
};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct QuarantineRestoreResponse {
    path: String,
    outcome: QuarantineRestoreOutcome,
}

pub(crate) fn restore_response(result: QuarantineRestoreResult) -> QuarantineRestoreResponse {
    QuarantineRestoreResponse {
        path: result.path.to_string_lossy().into_owned(),
        outcome: result.outcome,
    }
}

/// Test-Sandbox und Realmodus verwenden denselben Kern, aber getrennte Root-Schranken.
fn quarantine_scope(target: Option<String>) -> Result<QuarantineScope, AppError> {
    if let Some(root) = sandbox_root()? {
        return QuarantineScope::for_test_sandbox(root).map_err(AppError::from);
    }
    let target = target.ok_or_else(|| {
        AppError::new(
            "app",
            "Außerhalb der Test-Sandbox muss das aktuelle Realmodus-Ziel angegeben werden",
        )
    })?;
    QuarantineScope::for_target(expand_home_path(target)).map_err(AppError::from)
}

fn require_idle(state: &WindowState) -> Result<(), AppError> {
    if state.active_cancel.lock_recover().is_some() {
        return Err(AppError::new(
            "busy",
            "Während einer Datei- oder Sync-Operation ist die Quarantäne gesperrt",
        ));
    }
    Ok(())
}

/// Löst nur Provider auf, deren statischer Core-Vertrag eine vorhandene
/// Quarantäne verwalten kann. Für FTPS/WebDAV ist damit auch das bloße Öffnen der
/// Ansicht netzwerk- und mutationsfrei; das Frontend kennt diese Providerliste nicht.
fn resolve_remote_quarantine(
    app: &tauri::AppHandle,
    state: &AppState,
    request: &BidirectionalEndpointRequest,
) -> Result<
    Option<(
        ProviderEndpoint,
        PathBuf,
        Vec<doppeldecker_core::RemoteProfileBinding>,
    )>,
    AppError,
> {
    if !request.provider_kind().supports_quarantine_management() {
        return Ok(None);
    }
    let (endpoint, _, profile_bindings) = resolve_endpoint(app, state, request)?;
    let base = crate::queue_runtime::remote_quarantine_base("Remote", endpoint.root())?;
    Ok(Some((endpoint, base, profile_bindings)))
}

fn unsupported_remote_quarantine() -> AppError {
    AppError::new(
        "unsupported",
        "Dieser Provider besitzt keine verwaltbare Remote-Quarantäne",
    )
}

fn with_provider_quarantine_lease<T>(
    acquire: impl FnOnce() -> Result<QuarantineLease, AppError>,
    probe: impl FnOnce() -> Result<(), AppError>,
    action: impl FnOnce(&QuarantineLease) -> Result<T, AppError>,
) -> Result<T, AppError> {
    // Die Reihenfolge ist Sicherheitsvertrag: Eine mutierende Capability-Probe darf
    // nicht mehr starten, wenn ein anderes Fenster denselben Lauf noch verwendet.
    let lease = acquire()?;
    probe()?;
    action(&lease)
}

#[tauri::command]
pub(crate) async fn list_quarantine_runs(
    app: tauri::AppHandle,
    window: WebviewWindow,
    target: Option<String>,
) -> Result<Vec<QuarantineRun>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        require_idle(&state.window(window.label()))?;
        quarantine_scope(target)?.runs().map_err(AppError::from)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn list_quarantine_entries(
    app: tauri::AppHandle,
    window: WebviewWindow,
    run_id: String,
    target: Option<String>,
) -> Result<Vec<QuarantineEntry>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        require_idle(&state.window(window.label()))?;
        quarantine_scope(target)?
            .entries(&run_id)
            .map_err(AppError::from)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn restore_quarantine_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    run_id: String,
    relative_path: String,
    target: Option<String>,
) -> Result<QuarantineRestoreResponse, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let scope = quarantine_scope(target)?;
        let label = window.label();
        let window_state = state.window(label);
        with_reserved_operation(&app, label, &window_state, || {
            scope
                .restore(&run_id, &relative_path)
                .map(restore_response)
                .map_err(AppError::from)
        })
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn purge_quarantine_run(
    app: tauri::AppHandle,
    window: WebviewWindow,
    run_id: String,
    confirmation: String,
    target: Option<String>,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let expected = format!("LEEREN {run_id}");
        if confirmation != expected {
            return Err(AppError::new(
                "confirmation_mismatch",
                format!("Zum endgültigen Leeren exakt „{expected}“ eingeben"),
            ));
        }
        let scope = quarantine_scope(target)?;
        let label = window.label();
        let window_state = state.window(label);
        with_reserved_operation(&app, label, &window_state, || {
            scope.purge(&run_id).map_err(AppError::from)
        })
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn list_provider_quarantine_runs(
    app: tauri::AppHandle,
    window: WebviewWindow,
    endpoint: BidirectionalEndpointRequest,
) -> Result<Vec<QuarantineRun>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        require_idle(&state.window(window.label()))?;
        let Some((endpoint, base, profile_bindings)) =
            resolve_remote_quarantine(&app, &state, &endpoint)?
        else {
            return Ok(Vec::new());
        };
        let (vfs, _) = endpoint.file_context().map_err(AppError::from)?;
        let runs = list_core_quarantine_runs(vfs.as_ref(), &base, &None).map_err(AppError::from)?;
        crate::queue_runtime::ensure_profile_bindings_current(&app, &profile_bindings)?;
        Ok(runs)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn list_provider_quarantine_entries(
    app: tauri::AppHandle,
    window: WebviewWindow,
    endpoint: BidirectionalEndpointRequest,
    run_id: String,
) -> Result<Vec<QuarantineEntry>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        require_idle(&state.window(window.label()))?;
        let Some((endpoint, base, profile_bindings)) =
            resolve_remote_quarantine(&app, &state, &endpoint)?
        else {
            return Ok(Vec::new());
        };
        let (vfs, target) = endpoint.file_context().map_err(AppError::from)?;
        let entries =
            list_quarantine_entries_for_target(vfs.as_ref(), &base, &run_id, &None, Some(&target))
                .map_err(AppError::from)?;
        crate::queue_runtime::ensure_profile_bindings_current(&app, &profile_bindings)?;
        Ok(entries)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn restore_provider_quarantine_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    endpoint: BidirectionalEndpointRequest,
    run_id: String,
    relative_path: String,
) -> Result<QuarantineRestoreResponse, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        with_reserved_operation(&app, label, &window_state, || {
            let Some((endpoint, base, profile_bindings)) =
                resolve_remote_quarantine(&app, &state, &endpoint)?
            else {
                return Err(unsupported_remote_quarantine());
            };
            let (vfs, target) = endpoint.file_context().map_err(AppError::from)?;
            with_provider_quarantine_lease(
                || {
                    lease_quarantine_run_for_target(vfs.as_ref(), &base, &run_id, &target)
                        .map_err(AppError::from)
                },
                || {
                    endpoint
                        .probe_quarantine_restore_capability()
                        .map_err(AppError::from)?;
                    crate::queue_runtime::ensure_profile_bindings_current(&app, &profile_bindings)
                },
                |lease| {
                    restore_quarantine_entry_for_target_with_lease(
                        lease,
                        vfs.as_ref(),
                        &base,
                        &run_id,
                        &relative_path,
                        &target,
                    )
                    .map(restore_response)
                    .map_err(AppError::from)
                },
            )
        })
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[tauri::command]
pub(crate) async fn purge_provider_quarantine_run(
    app: tauri::AppHandle,
    window: WebviewWindow,
    endpoint: BidirectionalEndpointRequest,
    run_id: String,
    confirmation: String,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let expected = format!("LEEREN {run_id}");
        if confirmation != expected {
            return Err(AppError::new(
                "confirmation_mismatch",
                format!("Zum endgültigen Leeren exakt „{expected}“ eingeben"),
            ));
        }
        let label = window.label();
        let window_state = state.window(label);
        with_reserved_operation(&app, label, &window_state, || {
            let Some((endpoint, base, profile_bindings)) =
                resolve_remote_quarantine(&app, &state, &endpoint)?
            else {
                return Err(unsupported_remote_quarantine());
            };
            let (vfs, target) = endpoint.file_context().map_err(AppError::from)?;
            with_provider_quarantine_lease(
                || {
                    lease_quarantine_run_for_target(vfs.as_ref(), &base, &run_id, &target)
                        .map_err(AppError::from)
                },
                || {
                    endpoint
                        .probe_quarantine_purge_capability()
                        .map_err(AppError::from)?;
                    crate::queue_runtime::ensure_profile_bindings_current(&app, &profile_bindings)
                },
                |lease| {
                    purge_quarantine_run_for_target_with_lease(
                        lease,
                        vfs.as_ref(),
                        &base,
                        &run_id,
                        &target,
                    )
                    .map_err(AppError::from)
                },
            )
        })
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarantaene_verwendet_denselben_ipc_vertrag_wie_die_browserfaelle() {
        use doppeldecker_core::{
            quarantine::QuarantineReason, QuarantineRunStatus, QuarantineState,
        };
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../../tools/tests/quarantine-wire.json")).unwrap();
        let endpoint: BidirectionalEndpointRequest =
            serde_json::from_value(wire["remoteEndpoint"].clone()).unwrap();
        assert_eq!(
            endpoint,
            BidirectionalEndpointRequest::Smb {
                profile_id: "mock_smb".into(),
                profile_revision: Some(41),
                root: "/".into(),
            },
        );
        let run = QuarantineRun {
            id: "run-1".into(),
            created_unix_ms: 1,
            expires_unix_ms: 2,
            target_root: "/mock/ziel".into(),
            state: QuarantineState::Active,
            status: QuarantineRunStatus::CrashRecovery,
            items_total: 1,
            items_available: 1,
            items_restorable: 1,
            items_manual_recovery: 0,
            items_restored: 0,
            items_recovery_pending: 0,
            entries_available: 1,
            bytes_available: 8,
        };
        let entry = QuarantineEntry {
            relative_path: "alt.txt".into(),
            is_dir: false,
            reason: QuarantineReason::Replaced,
            restored: false,
            available: true,
            automatic_restore_available: true,
            manual_recovery_required: false,
            recovery_pending: false,
            entries: 1,
            bytes: 8,
        };
        let run = serde_json::to_value(run).unwrap();
        let entry = serde_json::to_value(entry).unwrap();
        crate::browser_commands::assert_wire_object(&run, &wire["run"]);
        crate::browser_commands::assert_wire_object(&entry, &wire["entry"]);
        assert_eq!(run["state"], "active");
        assert_eq!(run["status"], "crash_recovery");
        assert_eq!(entry["reason"], "replaced");
        for (outcome, expected) in [
            (QuarantineRestoreOutcome::Restored, "restored"),
            (QuarantineRestoreOutcome::Reconciled, "reconciled"),
        ] {
            let response = serde_json::to_value(restore_response(QuarantineRestoreResult {
                path: PathBuf::from("/mock/ziel/alt.txt"),
                outcome,
            }))
            .unwrap();
            crate::browser_commands::assert_wire_object(&response, &wire["restore"]);
            assert_eq!(response["outcome"], expected);
        }
    }

    #[test]
    fn live_lease_fehler_bleibt_bis_zum_tauri_payload_maschinenlesbar() {
        let core_error = doppeldecker_core::coded_error(
            std::io::ErrorKind::WouldBlock,
            doppeldecker_core::ErrorCode::QuarantineBusy,
            "Lauf ist belegt",
        );
        let app_error = AppError::from(core_error);

        assert_eq!(app_error.code, "quarantine_busy");
        assert_eq!(app_error.message, "Lauf ist belegt");
    }

    #[test]
    fn belegte_remote_quarantaene_stoppt_vor_capability_probe() {
        let probe_calls = std::cell::Cell::new(0_u32);
        let action_calls = std::cell::Cell::new(0_u32);

        let error = with_provider_quarantine_lease::<()>(
            || Err(AppError::new("quarantine_busy", "Lauf wird noch verwendet")),
            || {
                probe_calls.set(probe_calls.get() + 1);
                Ok(())
            },
            |_| {
                action_calls.set(action_calls.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.code, "quarantine_busy");
        assert_eq!(probe_calls.get(), 0);
        assert_eq!(action_calls.get(), 0);
    }
}
