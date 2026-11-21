//! Kurzlebige SMB-Stapel mit gemeinsamer Remote-Ausführung.

crate::remote_batch_rename::define_remote_batch_rename!(
    PreparedSmbBatchRename,
    validate_smb_batch_rename_request,
    Smb,
    "Dieser Stapel benötigt SMB"
);
