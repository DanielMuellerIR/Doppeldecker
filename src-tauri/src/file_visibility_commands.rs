//! Native Verstecktmarkierung über den gemeinsamen Fensterowner.
use super::*;
use doppeldecker_core::file_properties::{
    change_local_visibility, read_local_visibility, VisibilityEditRow, VisibilityReadRow,
};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct VisibilityReadRequest {
    provider: String,
    root: String,
    names: Vec<String>,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct VisibilityEditRequest {
    request_id: String,
    provider: String,
    root: String,
    names: Vec<String>,
    hidden: bool,
}

fn require_local(provider: &str) -> Result<(), AppError> {
    if provider != "local" {
        return Err(AppError::new(
            "unsupported",
            "Verstecktmarkierungen benötigen lokale Dateien oder Ordner auf macOS",
        ));
    }
    Ok(())
}
#[tauri::command]
pub(crate) async fn get_file_visibility(
    request: VisibilityReadRequest,
) -> Result<Vec<VisibilityReadRow>, AppError> {
    require_local(&request.provider)?;
    let sandbox = crate::sandbox_root()?;
    tauri::async_runtime::spawn_blocking(move || {
        read_local_visibility(
            Path::new(&request.root),
            &request.names,
            sandbox.as_deref(),
            &doppeldecker_core::Cancel::new(),
        )
        .map_err(Into::into)
    })
    .await
    .map_err(|_| AppError::new("io", "Verstecktmarkierungen konnten nicht gelesen werden"))?
}
#[tauri::command]
pub(crate) async fn change_file_visibility(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: VisibilityEditRequest,
) -> Result<Vec<VisibilityEditRow>, AppError> {
    require_local(&request.provider)?;
    run_cancellable_property_edit(
        app,
        window,
        request.request_id.clone(),
        "Sichtbarkeitsänderung unterbrochen; mögliche Teilwirkungen vor erneutem Versuch prüfen",
        move |sandbox, cancel| {
            change_local_visibility(
                Path::new(&request.root),
                &request.names,
                request.hidden,
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
    fn visibility_accepts_actual_nested_camelcase_request_and_rejects_extra_authority() {
        let value = serde_json::json!({"requestId":"visibility-edit","provider":"local","root":"/tmp/pane","names":["file"],"hidden":true});
        let request: VisibilityEditRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(request.request_id, "visibility-edit");
        assert!(request.hidden);
        let mut invalid = value;
        invalid["inode"] = serde_json::json!(7);
        assert!(serde_json::from_value::<VisibilityEditRequest>(invalid).is_err());
        assert!(require_local("sftp").is_err());
        let caps = doppeldecker_core::file_properties::property_capabilities(
            doppeldecker_core::ProviderKind::Local,
        );
        assert_eq!(caps.visibility, cfg!(target_os = "macos"));
        assert!(
            !doppeldecker_core::file_properties::property_capabilities(
                doppeldecker_core::ProviderKind::Sftp
            )
            .visibility
        );
    }
}
