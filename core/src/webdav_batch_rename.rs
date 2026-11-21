//! Kurzlebige WebDAV-Stapel mit gemeinsamer Remote-Ausführung.

crate::remote_batch_rename::define_remote_batch_rename!(
    PreparedWebDavBatchRename,
    validate_webdav_batch_rename_request,
    WebDav,
    "Dieser Stapel benötigt WebDAV"
);
