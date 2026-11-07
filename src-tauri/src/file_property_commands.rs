//! Gewöhnliche Metadatenänderungen im verursachenden Fenster.
use crate::bidirectional_commands::{
    resolve_endpoint, validate_registry_profile, BidirectionalEndpointRequest,
};
use crate::mutex::MutexExt;
use crate::{AppError, AppState, WindowState};
use doppeldecker_core::file_properties::{
    change_local_ownership, change_local_permissions_recursive, resolve_local_owner_edit,
    OwnerEditRow, PermissionEditRow,
};
use std::path::Path;
use std::{collections::VecDeque, sync::Arc};
use tauri::{Manager, WebviewWindow};

#[path = "file_flag_commands.rs"]
pub(crate) mod flags;

#[path = "file_comment_commands.rs"]
pub(crate) mod comments;
#[path = "file_tag_commands.rs"]
pub(crate) mod tags;
#[path = "file_visibility_commands.rs"]
pub(crate) mod visibility;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PermissionEditRequest {
    request_id: String,
    provider: String,
    root: String,
    names: Vec<String>,
    mode: String,
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    profile_revision: Option<u64>,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    administrator: bool,
}

#[derive(Default)]
pub(crate) struct PropertyEditCache {
    active: Option<(String, doppeldecker_core::Cancel)>,
    early_cancels: VecDeque<String>,
}
impl PropertyEditCache {
    fn begin(&mut self, id: &str) -> Result<(), AppError> {
        if self.active.is_some() {
            return Err(AppError::new(
                "busy",
                "Eine Metadatenänderung dieses Fensters läuft bereits",
            ));
        }
        let cancel = doppeldecker_core::Cancel::new();
        if let Some(index) = self.early_cancels.iter().position(|value| value == id) {
            self.early_cancels.remove(index);
            cancel.cancel();
        }
        self.active = Some((id.to_owned(), cancel));
        Ok(())
    }
    fn bind(&mut self, id: &str, cancel: &doppeldecker_core::Cancel) -> Result<(), AppError> {
        let Some((current, pending)) = &mut self.active else {
            return Err(AppError::new(
                "interrupted",
                "Metadatenänderung nicht mehr aktiv",
            ));
        };
        if current != id {
            return Err(AppError::new(
                "interrupted",
                "Andere Metadatenänderung aktiv",
            ));
        }
        if pending.is_cancelled() {
            cancel.cancel();
        }
        *pending = cancel.clone();
        Ok(())
    }
    fn cancel(&mut self, id: &str) {
        if let Some((current, cancel)) = &self.active {
            if current == id {
                cancel.cancel();
                return;
            }
        }
        if !self.early_cancels.iter().any(|value| value == id) {
            if self.early_cancels.len() == 64 {
                self.early_cancels.pop_front();
            }
            self.early_cancels.push_back(id.to_owned());
        }
    }
}
struct PropertyEditGuard {
    owner: Arc<WindowState>,
    id: String,
}
impl Drop for PropertyEditGuard {
    fn drop(&mut self) {
        let mut cache = self.owner.property_edits.lock_recover();
        if cache.active.as_ref().is_some_and(|(id, _)| id == &self.id) {
            cache.active = None;
        }
    }
}
async fn run_cancellable_property_edit<T: Send + 'static>(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request_id: String,
    interrupted_message: &'static str,
    edit: impl FnOnce(Option<&Path>, &doppeldecker_core::Cancel) -> Result<T, AppError> + Send + 'static,
) -> Result<T, AppError> {
    validate_request_id(&request_id)?;
    let sandbox = crate::sandbox_root()?;
    let state = app.state::<AppState>().window(window.label());
    state.property_edits.lock_recover().begin(&request_id)?;
    let guard = PropertyEditGuard {
        owner: state.clone(),
        id: request_id.clone(),
    };
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        crate::queue_runtime::with_reserved_cancellable_operation(
            &app,
            window.label(),
            &state,
            |cancel| {
                state
                    .property_edits
                    .lock_recover()
                    .bind(&request_id, cancel)?;
                edit(sandbox.as_deref(), cancel)
            },
            |_| {},
        )?
    })
    .await
    .map_err(|_| AppError::new("outcome_unclear", interrupted_message))?
}

fn validate_request_id(id: &str) -> Result<(), AppError> {
    if id.is_empty()
        || id.len() > 128
        || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(AppError::new(
            "invalid_input",
            "Ungültige Metadatenänderungs-ID",
        ));
    }
    Ok(())
}
#[tauri::command]
pub(crate) fn cancel_file_property_edit(
    window: WebviewWindow,
    state: tauri::State<AppState>,
    request_id: String,
) -> Result<(), AppError> {
    validate_request_id(&request_id)?;
    state
        .window(window.label())
        .property_edits
        .lock_recover()
        .cancel(&request_id);
    Ok(())
}

fn parse_mode(value: &str) -> Result<u32, AppError> {
    if value.len() != 3 || !value.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return Err(AppError::new(
            "invalid_input",
            "Berechtigungen benötigen genau drei Oktalziffern von 0 bis 7",
        ));
    }
    u32::from_str_radix(value, 8)
        .map_err(|_| AppError::new("invalid_input", "Ungültige Oktalrechte"))
}

fn property_provider(value: &str) -> Result<doppeldecker_core::ProviderKind, AppError> {
    use doppeldecker_core::ProviderKind;
    match value {
        "local" => Ok(ProviderKind::Local),
        "sftp" => Ok(ProviderKind::Sftp),
        "ftps" => Ok(ProviderKind::Ftps),
        "webdav" => Ok(ProviderKind::WebDav),
        "smb" => Ok(ProviderKind::Smb),
        _ => Err(AppError::new("invalid_input", "Unbekannter Dateiprovider")),
    }
}

#[tauri::command]
pub(crate) fn get_file_property_capabilities(
    provider: String,
) -> Result<doppeldecker_core::file_properties::FilePropertyCapabilities, AppError> {
    Ok(doppeldecker_core::file_properties::property_capabilities(
        property_provider(&provider)?,
    ))
}

fn permission_endpoint(
    request: &PermissionEditRequest,
) -> Result<BidirectionalEndpointRequest, AppError> {
    let kind = property_provider(&request.provider)?;
    if kind != doppeldecker_core::ProviderKind::Local
        && (request.profile_id.as_deref().is_none_or(str::is_empty)
            || request.profile_revision.is_none())
    {
        return Err(AppError::new(
            "invalid_input",
            "Entfernte Rechteänderung benötigt Profil-ID und Revision der Pane",
        ));
    }
    serde_json::from_value(serde_json::json!({"provider":request.provider,"root":request.root,"profileId":request.profile_id,"profileRevision":request.profile_revision}))
        .map_err(|error| AppError::new("invalid_input", error.to_string()))
}

#[tauri::command]
pub(crate) async fn change_file_permissions(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: PermissionEditRequest,
) -> Result<Vec<PermissionEditRow>, AppError> {
    validate_request_id(&request.request_id)?;
    let mode = parse_mode(&request.mode)?;
    doppeldecker_core::file_properties::validate_permission_selection(
        Path::new(&request.root),
        &request.names,
        mode,
        property_provider(&request.provider)?,
    )
    .map_err(AppError::from)?;
    if request.administrator && request.provider != "local" {
        return Err(AppError::new(
            "unsupported",
            "Administratorfreigabe gilt ausschließlich für lokale Metadaten",
        ));
    }
    let endpoint_request = permission_endpoint(&request)?;
    if !doppeldecker_core::file_properties::property_capabilities(endpoint_request.provider_kind())
        .permissions
    {
        return Err(AppError::new(
            "unsupported",
            "Dieser Provider bietet keinen interaktiven Unix-Rechteeditor",
        ));
    }
    let sandbox = crate::sandbox_root()?;
    let state = app.state::<AppState>().window(window.label());
    state
        .property_edits
        .lock_recover()
        .begin(&request.request_id)?;
    let guard = PropertyEditGuard {
        owner: state.clone(),
        id: request.request_id.clone(),
    };
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        crate::queue_runtime::with_reserved_cancellable_operation(
            &app,
            window.label(),
            &state,
            |cancel| {
                state
                    .property_edits
                    .lock_recover()
                    .bind(&request.request_id, cancel)?;
                if request.provider != "local" {
                    validate_registry_profile(&app, &endpoint_request)?;
                    let app_state = app.state::<AppState>();
                    let (endpoint, _, _) = resolve_endpoint(&app, &app_state, &endpoint_request)?;
                    let queue = crate::operation_queue(&app, &state, window.label())?;
                    let endpoint = queue
                        .bind_provider_preparation_with_cancel(&endpoint, cancel)
                        .map_err(AppError::from)?;
                    return doppeldecker_core::file_properties::change_provider_permissions(
                        &endpoint,
                        &request.names,
                        mode,
                        request.recursive,
                        cancel,
                        &|| {
                            validate_registry_profile(&app, &endpoint_request).map_err(|error| {
                                doppeldecker_core::coded_error(
                                    std::io::ErrorKind::Interrupted,
                                    doppeldecker_core::ErrorCode::PlanDrift,
                                    error.message,
                                )
                            })
                        },
                    )
                    .map_err(Into::into);
                }
                if request.administrator {
                    use doppeldecker_core::file_properties::admin::{
                        AdminMetadataAction, AdminMetadataRequest, AdminMetadataResult,
                    };
                    let bound = AdminMetadataRequest::capture(
                        request.request_id.clone(),
                        Path::new(&request.root),
                        request.names.clone(),
                        AdminMetadataAction::Permissions {
                            mode,
                            recursive: request.recursive,
                        },
                        sandbox.as_deref(),
                        cancel,
                    )?;
                    return match crate::metadata_admin::execute(bound, cancel)? {
                        AdminMetadataResult::Permissions(rows) => Ok(rows),
                        _ => Err(AppError::new(
                            "outcome_unclear",
                            "Falsche Administratorantwort; Teilwirkungen prüfen",
                        )),
                    };
                }
                change_local_permissions_recursive(
                    Path::new(&request.root),
                    &request.names,
                    mode,
                    request.recursive,
                    sandbox.as_deref(),
                    cancel,
                )
                .map_err(Into::into)
            },
            |_| {},
        )?
    })
    .await
    .map_err(|_| {
        AppError::new(
            "outcome_unclear",
            "Rechteänderung unterbrochen; mögliche Teilwirkungen vor einem neuen Versuch prüfen",
        )
    })?
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OwnerEditRequest {
    request_id: String,
    provider: String,
    root: String,
    names: Vec<String>,
    owner: String,
    group: String,
    #[serde(default)]
    administrator: bool,
}
#[tauri::command]
pub(crate) async fn change_file_ownership(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: OwnerEditRequest,
) -> Result<Vec<OwnerEditRow>, AppError> {
    validate_request_id(&request.request_id)?;
    if request.provider != "local" {
        return Err(AppError::new(
            "unsupported",
            "Eigentümer/Gruppe lassen sich derzeit für lokale Dateien und Ordner bearbeiten",
        ));
    }
    let sandbox = crate::sandbox_root()?;
    let state = app.state::<AppState>().window(window.label());
    state
        .property_edits
        .lock_recover()
        .begin(&request.request_id)?;
    let guard = PropertyEditGuard {
        owner: state.clone(),
        id: request.request_id.clone(),
    };
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        crate::queue_runtime::with_reserved_cancellable_operation(
            &app,
            window.label(),
            &state,
            |cancel| {
                state
                    .property_edits
                    .lock_recover()
                    .bind(&request.request_id, cancel)?;
                let edit = resolve_local_owner_edit(&request.owner, &request.group)?;
                if request.administrator {
                    use doppeldecker_core::file_properties::admin::{
                        AdminMetadataAction, AdminMetadataRequest, AdminMetadataResult,
                    };
                    let bound = AdminMetadataRequest::capture(
                        request.request_id.clone(),
                        Path::new(&request.root),
                        request.names.clone(),
                        AdminMetadataAction::Ownership {
                            uid: edit.uid,
                            gid: edit.gid,
                        },
                        sandbox.as_deref(),
                        cancel,
                    )?;
                    return match crate::metadata_admin::execute(bound, cancel)? {
                        AdminMetadataResult::Ownership(rows) => Ok(rows),
                        _ => Err(AppError::new(
                            "outcome_unclear",
                            "Falsche Administratorantwort; Teilwirkungen prüfen",
                        )),
                    };
                }
                change_local_ownership(
                    Path::new(&request.root),
                    &request.names,
                    edit,
                    sandbox.as_deref(),
                    cancel,
                )
                .map_err(Into::into)
            },
            |_| {},
        )?
    })
    .await
    .map_err(|_| {
        AppError::new(
            "outcome_unclear",
            "Eigentümeränderung unterbrochen; mögliche Teilwirkungen vor erneutem Versuch prüfen",
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_accepts_actual_frontend_payload_and_only_three_octal_digits() {
        let request: PermissionEditRequest = serde_json::from_value(serde_json::json!({
            "requestId":"direct", "provider": "local", "root": "/tmp/pane", "names": [" a "], "mode": "640"
        }))
        .unwrap();
        assert_eq!(parse_mode(&request.mode).unwrap(), 0o640);
        assert!(!request.recursive);
        let recursive: PermissionEditRequest = serde_json::from_value(serde_json::json!({
            "requestId":"recursive", "provider":"local", "root":"/tmp/pane", "names":["tree"], "mode":"750", "recursive":true
        }))
        .unwrap();
        assert!(recursive.recursive);
        for invalid in ["", "64", "0640", "7777", "888", " 640", "640 ", "rwx"] {
            assert!(parse_mode(invalid).is_err(), "{invalid}");
        }
    }
    #[test]
    fn cancel_is_bound_to_request_and_window_even_before_worker_reservation() {
        let mut first = PropertyEditCache::default();
        let mut other = PropertyEditCache::default();
        first.cancel("pending");
        first.begin("pending").unwrap();
        other.begin("pending").unwrap();
        let a = doppeldecker_core::Cancel::new();
        let b = doppeldecker_core::Cancel::new();
        first.bind("pending", &a).unwrap();
        other.bind("pending", &b).unwrap();
        assert!(a.is_cancelled());
        assert!(!b.is_cancelled());
        other.cancel("different");
        assert!(!b.is_cancelled());
        assert!(other.begin("new").is_err());
        other.cancel("pending");
        assert!(b.is_cancelled());
        for index in 0..100 {
            first.cancel(&format!("early-{index}"));
        }
        assert_eq!(first.early_cancels.len(), 64);
    }
    #[test]
    fn guard_releases_only_its_request_and_invalid_ids_are_rejected() {
        let owner = Arc::new(WindowState::default());
        owner.property_edits.lock_recover().begin("one").unwrap();
        drop(PropertyEditGuard {
            owner: owner.clone(),
            id: "another".into(),
        });
        assert!(owner.property_edits.lock_recover().active.is_some());
        drop(PropertyEditGuard {
            owner: owner.clone(),
            id: "one".into(),
        });
        assert!(owner.property_edits.lock_recover().active.is_none());
        for id in ["", "path/slash", "line\n", &"a".repeat(129)] {
            assert!(validate_request_id(id).is_err());
        }
    }
    #[test]
    fn ownership_request_accepts_real_camelcase_payload_and_rejects_identity_fields() {
        let request: OwnerEditRequest = serde_json::from_value(serde_json::json!({
            "requestId":"owner-request", "provider":"local", "root":"/tmp/pane", "names":["file"], "owner":"", "group":"20"
        })).unwrap();
        assert_eq!(request.request_id, "owner-request");
        assert_eq!(
            resolve_local_owner_edit(&request.owner, &request.group)
                .unwrap()
                .gid,
            Some(20)
        );
        assert!(serde_json::from_value::<OwnerEditRequest>(serde_json::json!({
            "requestId":"owner-request", "provider":"local", "root":"/tmp/pane", "names":["file"], "owner":"", "group":"20", "inode":42
        })).is_err());
    }
    #[test]
    fn remote_permissions_require_actual_pane_profile_revision() {
        let mut value = serde_json::json!({"requestId":"remote", "provider":"sftp", "root":"/pane", "names":["file"], "mode":"640", "profileId":"server", "profileRevision":11});
        let request: PermissionEditRequest = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(
            permission_endpoint(&request).unwrap(),
            BidirectionalEndpointRequest::Sftp {
                profile_revision: Some(11),
                ..
            }
        ));
        value.as_object_mut().unwrap().remove("profileRevision");
        let request: PermissionEditRequest = serde_json::from_value(value).unwrap();
        assert_eq!(
            permission_endpoint(&request).unwrap_err().code,
            "invalid_input"
        );
        assert!(property_provider("arbitrary").is_err());
    }
}
