//! Integrierte passive Vorschau; Pfad- und Providerbelege bleiben im Backend.

use std::path::Path;
use tauri::{Manager, WebviewWindow};

use crate::bidirectional_commands::{resolve_endpoint, BidirectionalEndpointRequest};
use crate::{AppError, AppState};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FilePreviewRequest {
    endpoint: BidirectionalEndpointRequest,
    path: String,
}

fn validate_request(request: &FilePreviewRequest) -> Result<(), AppError> {
    crate::bidirectional_commands::validate_endpoint_request_root(&request.endpoint)?;
    crate::browser_commands::validate_diff_path(request.endpoint.root(), &request.path)?;
    Ok(())
}

#[tauri::command]
pub(crate) async fn prepare_file_preview(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: FilePreviewRequest,
) -> Result<doppeldecker_core::preview::FilePreview, AppError> {
    // Ungültige Requests sprechen keinen Provider an und legen keinen Cache an.
    validate_request(&request)?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let window_state = state.window(window.label());
        crate::queue_runtime::with_reserved_cancellable_operation(
            &app,
            window.label(),
            &window_state,
            |cancel| {
                let (endpoint, _, bindings) = resolve_endpoint(&app, &state, &request.endpoint)?;
                let cache = crate::browser_commands::snapshot_cache(&app, "preview-snapshots")?;
                doppeldecker_core::cleanup_remote_snapshots(
                    &cache,
                    std::time::SystemTime::now(),
                    doppeldecker_core::SNAPSHOT_RETENTION,
                )
                .map_err(AppError::from)?;
                let preview = doppeldecker_core::preview::prepare_file_preview(
                    &endpoint,
                    Path::new(&request.path),
                    &cache,
                    cancel,
                )
                .map_err(AppError::from)?;
                crate::queue_runtime::ensure_profile_bindings_current(&app, &bindings)?;
                Ok(preview)
            },
            |_| {},
        )?
    })
    .await
    .map_err(|_| AppError::new("app", "Vorschau-Worker wurde unterbrochen"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_request_liest_camelcase_und_prueft_pfade_vor_providerzugriff() {
        let request: FilePreviewRequest = serde_json::from_value(serde_json::json!({
            "endpoint": { "provider": "sftp", "profileId": "test", "profileRevision": 11, "root": "/daten" },
            "path": "/daten/file.txt"
        })).unwrap();
        validate_request(&request).unwrap();
        for path in [
            "/fremd/file.txt",
            "/daten/../file.txt",
            "/daten/./file.txt",
            "/daten",
        ] {
            let invalid = FilePreviewRequest {
                endpoint: request.endpoint.clone(),
                path: path.into(),
            };
            assert!(validate_request(&invalid).is_err(), "{path}");
        }
    }
}
