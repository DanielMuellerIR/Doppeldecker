//! Kurzlebige SFTP-Stapel mit gemeinsamer Remote-Ausführung.
pub(crate) use crate::remote_batch_rename::ParentProof;

crate::remote_batch_rename::define_remote_batch_rename!(
    PreparedSftpBatchRename,
    validate_sftp_batch_rename_request,
    Sftp,
    "Dieser Stapel benötigt SFTP"
);
