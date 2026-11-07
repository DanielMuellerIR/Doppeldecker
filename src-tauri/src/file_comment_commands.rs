//! Finder-Kommentare mit ausdrücklicher Systemfreigabe und Fensterowner.
use super::*;
use doppeldecker_core::file_properties::{
    validate_finder_comment, FinderCommentEditRow, FinderCommentReadRow,
};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FinderCommentReadRequest {
    provider: String,
    root: String,
    names: Vec<String>,
    #[serde(default)]
    request_permission: bool,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FinderCommentEditRequest {
    request_id: String,
    provider: String,
    root: String,
    names: Vec<String>,
    text: String,
}
fn require_local(provider: &str) -> Result<(), AppError> {
    if provider != "local" {
        return Err(AppError::new(
            "unsupported",
            "Finder-Kommentare benötigen lokale Objekte auf macOS",
        ));
    }
    Ok(())
}
fn read(
    request: FinderCommentReadRequest,
    sandbox: Option<&Path>,
) -> std::io::Result<Vec<FinderCommentReadRow>> {
    #[cfg(target_os = "macos")]
    {
        let mut access =
            crate::finder_comments_native::NativeFinderComments::new(request.request_permission);
        doppeldecker_core::file_properties::read_local_finder_comments(
            Path::new(&request.root),
            &request.names,
            sandbox,
            &doppeldecker_core::Cancel::new(),
            &mut access,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (
            request.root,
            request.names,
            request.request_permission,
            sandbox,
        );
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Finder-Kommentare benötigen macOS",
        ))
    }
}
fn edit(
    request: &FinderCommentEditRequest,
    sandbox: Option<&Path>,
    cancel: &doppeldecker_core::Cancel,
) -> std::io::Result<Vec<FinderCommentEditRow>> {
    #[cfg(target_os = "macos")]
    {
        let mut access = crate::finder_comments_native::NativeFinderComments::new(false);
        doppeldecker_core::file_properties::change_local_finder_comments(
            Path::new(&request.root),
            &request.names,
            &request.text,
            sandbox,
            cancel,
            &mut access,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (&request.root, &request.names, sandbox, cancel);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Finder-Kommentare benötigen macOS",
        ))
    }
}
#[tauri::command]
pub(crate) async fn get_file_finder_comments(
    request: FinderCommentReadRequest,
) -> Result<Vec<FinderCommentReadRow>, AppError> {
    require_local(&request.provider)?;
    let sandbox = crate::sandbox_root()?;
    tauri::async_runtime::spawn_blocking(move || {
        read(request, sandbox.as_deref()).map_err(Into::into)
    })
    .await
    .map_err(|_| AppError::new("io", "Finder-Kommentare konnten nicht gelesen werden"))?
}
#[tauri::command]
pub(crate) async fn change_file_finder_comments(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: FinderCommentEditRequest,
) -> Result<Vec<FinderCommentEditRow>, AppError> {
    validate_finder_comment(&request.text).map_err(AppError::from)?;
    require_local(&request.provider)?;
    run_cancellable_property_edit(
        app,
        window,
        request.request_id.clone(),
        "Kommentaränderung unterbrochen; mögliche Teilwirkungen vor erneutem Versuch prüfen",
        move |sandbox, cancel| edit(&request, sandbox, cancel).map_err(Into::into),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn comments_accept_actual_nested_payload_without_caller_supplied_authority() {
        let value = serde_json::json!({"requestId":"comment-edit","provider":"local","root":"/tmp/pane","names":["file"],"text":"Kommentar\nmit Unicode ä"});
        let request: FinderCommentEditRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(request.request_id, "comment-edit");
        validate_finder_comment(&request.text).unwrap();
        let mut invalid = value;
        invalid["fileReference"] = serde_json::json!("file:///.file/id=1.2");
        assert!(serde_json::from_value::<FinderCommentEditRequest>(invalid).is_err());
        let read:FinderCommentReadRequest=serde_json::from_value(serde_json::json!({"provider":"local","root":"/tmp/pane","names":["file"],"requestPermission":true})).unwrap();
        assert!(read.request_permission);
        assert!(require_local("sftp").is_err());
    }
}
