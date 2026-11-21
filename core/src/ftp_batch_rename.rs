//! Kurzlebige FTP-/FTPS-Stapel mit gemeinsamer Remote-Ausführung.

crate::remote_batch_rename::define_remote_batch_rename!(
    PreparedFtpBatchRename,
    validate_ftp_batch_rename_request,
    Ftps,
    "Dieser Stapel benötigt FTP-/FTPS"
);
