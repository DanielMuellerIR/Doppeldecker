//! Einzelne native Dateieigenschaften im verursachenden Fenster.
use super::*;
use doppeldecker_core::file_properties::{
    change_local_file_flag, read_local_file_flags, FileFlagKind, FileFlagRow,
};
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FileFlagReadRequest {
    provider: String,
    root: String,
    names: Vec<String>,
    kind: FileFlagKind,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FileFlagEditRequest {
    request_id: String,
    provider: String,
    root: String,
    names: Vec<String>,
    kind: FileFlagKind,
    enabled: bool,
}
fn require_local(provider: &str) -> Result<(), AppError> {
    if provider != "local" {
        return Err(AppError::new(
            "unsupported",
            "Diese Dateieigenschaften benötigen lokale Objekte auf macOS",
        ));
    }
    Ok(())
}
#[tauri::command]
pub(crate) async fn get_file_flags(
    request: FileFlagReadRequest,
) -> Result<Vec<FileFlagRow>, AppError> {
    require_local(&request.provider)?;
    let sandbox = crate::sandbox_root()?;
    tauri::async_runtime::spawn_blocking(move || {
        read_local_file_flags(
            Path::new(&request.root),
            &request.names,
            request.kind,
            sandbox.as_deref(),
            &doppeldecker_core::Cancel::new(),
        )
        .map_err(Into::into)
    })
    .await
    .map_err(|_| AppError::new("io", "Dateieigenschaften konnten nicht gelesen werden"))?
}
#[tauri::command]
pub(crate) async fn change_file_flag(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: FileFlagEditRequest,
) -> Result<Vec<FileFlagRow>, AppError> {
    require_local(&request.provider)?;
    run_cancellable_property_edit(
        app,
        window,
        request.request_id.clone(),
        "Dateieigenschaftsänderung unterbrochen; mögliche Teilwirkungen vor erneutem Versuch prüfen",
        move |sandbox, cancel| {
            change_local_file_flag(
                Path::new(&request.root),
                &request.names,
                request.kind,
                request.enabled,
                sandbox,
                cancel,
            )
            .map_err(Into::into)
        },
    )
    .await
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_flags_accept_actual_camelcase_and_reject_free_flag_masks_and_remote_authority() {
        let value = serde_json::json!({"requestId":"flag-edit","provider":"local","root":"/tmp/pane","names":["file"],"kind":"hiddenExtension","enabled":true});
        let request: FileFlagEditRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(request.kind, FileFlagKind::HiddenExtension);
        assert_eq!(request.request_id, "flag-edit");
        assert!(request.enabled);
        let mut invalid = value.clone();
        invalid["flags"] = serde_json::json!(u32::MAX);
        assert!(serde_json::from_value::<FileFlagEditRequest>(invalid).is_err());
        let mut invalid = value;
        invalid["kind"] = serde_json::json!("systemImmutable");
        assert!(serde_json::from_value::<FileFlagEditRequest>(invalid).is_err());
        assert!(require_local("sftp").is_err());
        for provider in [
            doppeldecker_core::ProviderKind::Local,
            doppeldecker_core::ProviderKind::Sftp,
        ] {
            assert_eq!(
                doppeldecker_core::file_properties::property_capabilities(provider).native_flags,
                provider == doppeldecker_core::ProviderKind::Local && cfg!(target_os = "macos")
            );
        }
    }
}
