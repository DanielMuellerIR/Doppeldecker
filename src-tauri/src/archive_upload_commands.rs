//! Gemeinsame Zielbindung für ZIP-Erzeugung und Entpacken auf Remoteziele.
use crate::bidirectional_commands::{resolve_endpoint, BidirectionalEndpointRequest};
use crate::{AppError, AppState, WindowState};
use doppeldecker_core::generated_source::{GeneratedSourceStore, GeneratedWorkspaceReference};
use doppeldecker_core::{BidirectionalQueueEndpoint, RemoteProfileBinding};
use std::path::PathBuf;

#[derive(Clone)]
pub(crate) struct ArchiveUploadTarget {
    pub(crate) workspace: GeneratedWorkspaceReference,
    pub(crate) endpoint: Box<BidirectionalQueueEndpoint>,
    pub(crate) profiles: Vec<RemoteProfileBinding>,
    pub(crate) label: String,
    pub(crate) notice: Option<String>,
}
impl ArchiveUploadTarget {
    pub(crate) fn operation(
        &self,
        generation: doppeldecker_core::archive_upload::ArchiveGeneration,
    ) -> doppeldecker_core::QueueOperation {
        doppeldecker_core::QueueOperation::ArchiveUpload {
            generation: Box::new(generation),
            workspace: self.workspace.clone(),
            target: self.endpoint.clone(),
        }
    }
}
pub(crate) fn prepare_target(
    app: &tauri::AppHandle,
    state: &AppState,
    owner: &WindowState,
    label: &str,
    request: &BidirectionalEndpointRequest,
) -> Result<(PathBuf, ArchiveUploadTarget), AppError> {
    if request.provider_kind() == doppeldecker_core::ProviderKind::Local {
        return Err(AppError::new(
            "invalid_input",
            "Dieser Archivablauf benötigt eine entfernte Zielpane",
        ));
    }
    crate::bidirectional_commands::validate_registry_profile(app, request)?;
    let (_, endpoint, profiles) = resolve_endpoint(app, state, request)?;
    doppeldecker_core::fileops::guard(&crate::sandbox_root()?, &crate::app_config_dir(app)?)?;
    let queue = crate::operation_queue(app, owner, label)?;
    let workspace = GeneratedSourceStore::for_queue(&queue)?.begin()?;
    doppeldecker_core::fileops::guard(&crate::sandbox_root()?, workspace.path())?;
    let target = ArchiveUploadTarget {
        workspace: workspace.reference(),
        label: crate::file_commands::file_endpoint_label(&endpoint),
        notice: (request.provider_kind() == doppeldecker_core::ProviderKind::Sftp)
            .then(|| doppeldecker_core::sftp::SFTP_WRITE_LIMITATION.to_owned()),
        endpoint,
        profiles,
    };
    Ok((workspace.path().to_path_buf(), target))
}

pub(crate) fn validate_target<'a>(
    provider: &str,
    directory: &str,
    request: Option<&'a BidirectionalEndpointRequest>,
) -> Result<Option<&'a BidirectionalEndpointRequest>, AppError> {
    match (provider, request) {
        ("local", None) => Ok(None),
        ("local", Some(_)) => Err(AppError::new(
            "invalid_input",
            "Lokales Entpackziel darf kein Remoteprofil enthalten",
        )),
        (_, Some(request))
            if request.root() == directory
                && format!("{:?}", request.provider_kind()).to_lowercase() == provider
                && request.provider_kind() != doppeldecker_core::ProviderKind::Local =>
        {
            Ok(Some(request))
        }
        _ => Err(AppError::new(
            "invalid_input",
            "Entpackziel und gebundene Remote-Pane passen nicht zusammen",
        )),
    }
}

#[tauri::command]
pub(crate) async fn generated_source_path(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    operation_id: String,
) -> Result<String, AppError> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let owner = state.window(window.label());
        let queue = crate::operation_queue(&app, &owner, window.label())?;
        Ok(queue
            .generated_source_path(&operation_id)?
            .to_string_lossy()
            .into_owned())
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entfernte_entpackziele_binden_profilrevision_provider_und_root_in_camelcase() {
        for provider in ["sftp", "ftps", "webdav", "smb"] {
            let request:BidirectionalEndpointRequest=serde_json::from_value(serde_json::json!({"provider":provider,"root":"/pane","profileId":"target-profile","profileRevision":23})).unwrap();
            assert!(validate_target(provider, "/pane", Some(&request))
                .unwrap()
                .is_some());
            assert!(validate_target(provider, "/other", Some(&request)).is_err());
            assert!(validate_target("local", "/pane", Some(&request)).is_err());
            assert!(validate_target(provider, "/pane", None).is_err());
        }
        assert!(validate_target("local", "/pane", None).unwrap().is_none());
    }
}
