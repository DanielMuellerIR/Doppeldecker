//! Kurzlebige SFTP-Stapel mit gemeinsamer Remote-Ausführung.
use crate::batch_rename::{BatchRenameOptions, BatchRenameReport, BatchRenameRow};
pub(crate) use crate::remote_batch_rename::ParentProof;
use crate::remote_batch_rename::PreparedRemoteBatchRename;
use crate::{Cancel, ProviderEndpoint, ProviderKind, RenameExecutionClass};
use std::io;
pub struct PreparedSftpBatchRename {
    inner: PreparedRemoteBatchRename,
}
pub fn validate_sftp_batch_rename_request(
    names: &[String],
    options: &BatchRenameOptions,
) -> io::Result<()> {
    crate::batch_rename::validate_batch_rename_request(names, options)
}
impl PreparedSftpBatchRename {
    pub fn prepare(
        endpoint: &ProviderEndpoint,
        names: &[String],
        options: &BatchRenameOptions,
        cancel: &Cancel,
    ) -> io::Result<Self> {
        validate_sftp_batch_rename_request(names, options)?;
        if endpoint.kind() != ProviderKind::Sftp {
            return Err(crate::coded_error(
                io::ErrorKind::Unsupported,
                crate::ErrorCode::Unsupported,
                "Dieser Stapel benötigt SFTP",
            ));
        }
        Ok(Self {
            inner: PreparedRemoteBatchRename::prepare(endpoint, names, options, cancel)?,
        })
    }
    pub fn rows(&self) -> &[BatchRenameRow] {
        self.inner.rows()
    }
    pub fn execution_class(&self) -> RenameExecutionClass {
        self.inner.execution_class()
    }
    pub fn limitations(&self) -> &[String] {
        self.inner.limitations()
    }
    pub fn is_expired(&self) -> bool {
        self.inner.is_expired()
    }
    pub fn execute(
        self,
        cancel: &Cancel,
        before_each: &mut dyn FnMut() -> io::Result<()>,
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> BatchRenameReport {
        self.inner.execute(cancel, before_each, on_progress)
    }
    pub fn execute_with_queue(
        self,
        cancel: &Cancel,
        before_each: &mut dyn FnMut() -> io::Result<()>,
        on_progress: &mut dyn FnMut(usize, usize),
        queue: &crate::OperationQueue,
    ) -> BatchRenameReport {
        self.inner
            .execute_with_queue(cancel, before_each, on_progress, Some(queue))
    }
}
