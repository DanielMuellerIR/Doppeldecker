//! Paginierte Finder-Tags und direkte Änderungen über den gemeinsamen Fensterowner.
use super::*;
use doppeldecker_core::file_properties::{
    change_local_finder_tags, read_local_finder_tags, FinderTagEdit, FinderTagEditRow,
    FinderTagReadRow,
};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FinderTagReadRequest {
    provider: String,
    root: String,
    names: Vec<String>,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FinderTagEditRequest {
    request_id: String,
    provider: String,
    root: String,
    names: Vec<String>,
    edit: FinderTagEdit,
}

fn require_local(provider: &str) -> Result<(), AppError> {
    if provider != "local" {
        return Err(AppError::new(
            "unsupported",
            "Finder-Tags benötigen lokale Dateien oder Ordner auf macOS",
        ));
    }
    Ok(())
}
#[tauri::command]
pub(crate) async fn get_file_finder_tags(
    request: FinderTagReadRequest,
) -> Result<Vec<FinderTagReadRow>, AppError> {
    require_local(&request.provider)?;
    let sandbox = crate::sandbox_root()?;
    tauri::async_runtime::spawn_blocking(move || {
        read_local_finder_tags(
            Path::new(&request.root),
            &request.names,
            sandbox.as_deref(),
            &doppeldecker_core::Cancel::new(),
        )
        .map_err(Into::into)
    })
    .await
    .map_err(|_| AppError::new("io", "Finder-Tags konnten nicht gelesen werden"))?
}
#[tauri::command]
pub(crate) async fn change_file_finder_tags(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: FinderTagEditRequest,
) -> Result<Vec<FinderTagEditRow>, AppError> {
    request.edit.validate().map_err(AppError::from)?;
    require_local(&request.provider)?;
    run_cancellable_property_edit(
        app,
        window,
        request.request_id.clone(),
        "Finder-Tagänderung unterbrochen; mögliche Teilwirkungen vor erneutem Versuch prüfen",
        move |sandbox, cancel| {
            change_local_finder_tags(
                Path::new(&request.root),
                &request.names,
                &request.edit,
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
    fn actual_nested_camelcase_tag_edit_payload_is_accepted_and_unknown_fields_rejected() {
        let value = serde_json::json!({"requestId":"tag-edit","provider":"local","root":"/tmp/pane","names":["file"],"edit":{"type":"upsert","tag":{"name":"Projekt","color":4},"previousName":"Alt"}});
        let request: FinderTagEditRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(request.request_id, "tag-edit");
        request.edit.validate().unwrap();
        assert!(
            matches!(request.edit,FinderTagEdit::Upsert {previous_name:Some(name),..} if name=="Alt")
        );
        let mut invalid = value;
        invalid["edit"]["inode"] = serde_json::json!(7);
        assert!(serde_json::from_value::<FinderTagEditRequest>(invalid).is_err());
        assert!(require_local("sftp").is_err());
    }
}
