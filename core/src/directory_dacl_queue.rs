//! Bewusster Queueabschluss einer bestehenden Verzeichnis-DACL nach frischem Readback.
use crate::{OperationQueue, ProviderEndpoint, QueueOperation, QueueState};
use std::io;

/// Erkennt ausschließlich den frisch geprüften Zustand an. Diese Funktion
/// wiederholt keinen SET und entfernt oder restauriert keinen entfernten Ordner.
pub fn acknowledge_directory_dacl_outcome(
    queue: &OperationQueue,
    operation_id: &str,
    target: &ProviderEndpoint,
    possible_effect_reviewed: bool,
) -> io::Result<()> {
    if !possible_effect_reviewed {
        return Err(crate::coded_error(
            io::ErrorKind::PermissionDenied,
            crate::ErrorCode::ConfirmationMismatch,
            "Der mögliche Ausgang der Verzeichnis-DACL wurde nicht ausdrücklich geprüft und bestätigt",
        ));
    }
    let snapshot = queue.snapshot();
    let entry = snapshot
        .entries
        .iter()
        .find(|entry| entry.operation_id == operation_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Queueeintrag fehlt"))?;
    if !matches!(entry.state, QueueState::Failed | QueueState::Interrupted)
        || !entry.directory_dacl_recovery
    {
        return Err(crate::coded_error(
            io::ErrorKind::InvalidInput,
            crate::ErrorCode::ReplanRequired,
            "Der Queueeintrag besitzt keine beendete offene Verzeichnis-DACL-Recovery",
        ));
    }
    let QueueOperation::ContentSync {
        target: expected, ..
    } = &entry.operation
    else {
        return Err(crate::coded_error(
            io::ErrorKind::InvalidInput,
            crate::ErrorCode::ReplanRequired,
            "Verzeichnis-DACL-Recovery benötigt den ursprünglichen Inhaltssync",
        ));
    };
    if !matches!(
        expected.as_ref(),
        crate::BidirectionalQueueEndpoint::Smb { .. }
    ) || !target.matches_queue_endpoint(expected)
    {
        return Err(crate::real_mode::plan_drift(
            "Zielprofil oder Zielroot änderte sich seit dem gespeicherten Inhaltssync",
        ));
    }
    // Owner und Prozesssperre bleiben bis zum Queuecheckpoint gehalten. Auch ein
    // Prepared-Absturz benötigt diesen Weg, obwohl sein Wirkungsbericht leer ist.
    let (owner, _process_lock, config) = queue.ftps_owner(Some(operation_id))?;
    let (vfs, root) = target.file_context()?;
    let identity = vfs.root_identity(&root)?;
    let binding = target.content_sync_endpoint_binding(&identity)?;
    let crate::EndpointBinding::Smb(endpoint) = binding else {
        return Err(crate::real_mode::plan_drift(
            "Verzeichnis-DACL-Recovery benötigt den frisch gebundenen SMB-Namespace",
        ));
    };
    let acknowledgement = crate::local_staging::finish_directory_dacl_recovery_beneath(
        &owner,
        vfs.as_ref(),
        &root,
        endpoint.account_digest,
        &config,
    )?;
    queue
        .commit_directory_dacl_acknowledgement(
            operation_id,
            &acknowledgement.report,
            &acknowledgement.paths,
        )
        .map_err(|error| crate::error::with_directory_dacl_report(error, &acknowledgement.report))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, OperationQueue, ProviderEndpoint) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let queue = OperationQueue::open(root.join("queue.json")).unwrap();
        let target = ProviderEndpoint::local(&root).unwrap();
        (temp, queue, target)
    }
    #[test]
    fn missing_confirmation_is_rejected_before_queue_or_endpoint_validation() {
        let (_temp, queue, target) = fixture();
        let before = queue.snapshot();
        let error =
            acknowledge_directory_dacl_outcome(&queue, "unknown", &target, false).unwrap_err();
        assert_eq!(
            crate::error_code(&error),
            crate::ErrorCode::ConfirmationMismatch
        );
        assert_eq!(queue.snapshot(), before);
    }
    #[test]
    fn missing_operation_cannot_acknowledge_an_unrelated_endpoint() {
        let (_temp, queue, target) = fixture();
        let before = queue.snapshot();
        let error =
            acknowledge_directory_dacl_outcome(&queue, "unknown", &target, true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(queue.snapshot(), before);
    }
}
