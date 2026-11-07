//! Bewusster Queueabschluss einer bestehenden Neuordner-DACL nach frischem Readback.
use crate::{OperationQueue, ProviderEndpoint, QueueOperation, QueueState};
use std::io;

/// Erkennt ausschließlich den frisch geprüften Zustand an. Diese Funktion
/// wiederholt keinen SET und entfernt oder restauriert keine entfernte Datei.
pub fn acknowledge_created_directory_dacl_outcome(
    queue: &OperationQueue,
    operation_id: &str,
    target: &ProviderEndpoint,
    possible_effect_reviewed: bool,
) -> io::Result<()> {
    if !possible_effect_reviewed {
        return Err(crate::coded_error(
            io::ErrorKind::PermissionDenied,
            crate::ErrorCode::ConfirmationMismatch,
            "Der mögliche Ausgang der Neuordner-DACL wurde nicht ausdrücklich geprüft und bestätigt",
        ));
    }
    let snapshot = queue.snapshot();
    let entry = snapshot
        .entries
        .iter()
        .find(|entry| entry.operation_id == operation_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Queueeintrag fehlt"))?;
    if !matches!(entry.state, QueueState::Failed | QueueState::Interrupted)
        || !entry.created_directory_dacl_recovery
    {
        return Err(crate::coded_error(
            io::ErrorKind::InvalidInput,
            crate::ErrorCode::ReplanRequired,
            "Der Queueeintrag besitzt keine beendete offene Neuordner-DACL-Recovery",
        ));
    }
    let QueueOperation::ContentSync {
        target: expected, ..
    } = &entry.operation
    else {
        return Err(crate::coded_error(
            io::ErrorKind::InvalidInput,
            crate::ErrorCode::ReplanRequired,
            "Neuordner-DACL-Recovery benötigt den ursprünglichen Inhaltssync",
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
            "Neuordner-DACL-Recovery benötigt den frisch gebundenen SMB-Namespace",
        ));
    };
    let acknowledgement = crate::created_directory_dacl_registry::acknowledge(
        &owner,
        vfs.as_ref(),
        &root,
        endpoint.account_digest,
        &config,
    )?;
    commit_acknowledgement(
        queue,
        operation_id,
        &entry.created_directory_dacl_report,
        &acknowledgement,
    )
}

pub(crate) fn commit_acknowledgement(
    queue: &OperationQueue,
    operation_id: &str,
    previous: &crate::created_directory_dacl::CreatedDirectoryDaclReport,
    acknowledgement: &crate::created_directory_dacl_registry::Acknowledgement,
) -> io::Result<()> {
    // Der Einzelbeleg enthält nach Retirement nicht mehr alle früheren Wirkungen.
    // Auch ein fehlgeschlagener Queuecommit muss den frisch aufgelösten Gesamtstand
    // melden: bestätigte frühere Objekte bleiben, erkannte Vorherzustände entfallen.
    let mut report = previous.clone();
    report.reconcile(&acknowledgement.resolved, &acknowledgement.report);
    queue
        .commit_created_directory_dacl_acknowledgement(
            operation_id,
            &acknowledgement.report,
            &acknowledgement.paths,
            &acknowledgement.resolved,
        )
        .map_err(|error| crate::error::with_created_directory_dacl_report(error, &report))
}
