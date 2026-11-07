//! ZIP-Ausführung innerhalb des vorhandenen Datei-Queueabschlusses.
use super::*;
use crate::archive_pack::{
    ArchivePackBinding, ArchivePackOwner, ArchivePackPhase, ArchivePackRequest, ArchivePackStatus,
};

#[allow(clippy::too_many_arguments)]
pub(super) fn execute(
    queue: &OperationQueue,
    operation_id: &str,
    request: ArchivePackRequest,
    expected: ArchivePackBinding,
    execution: &LocalQueueExecution,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    actuals: &mut FileOperationActuals,
    pending_completion: &mut Option<Box<PendingFileCompletion>>,
) -> io::Result<QueueExecutionResult> {
    if request.sandbox != execution.sandbox {
        return Err(crate::real_mode::plan_drift(
            "ZIP-Testbereich hat sich seit der Vorschau geändert",
        ));
    }
    let context = ProviderAuditContext {
        source_provider: Some(crate::ProviderKind::Local),
        target_provider: crate::ProviderKind::Local,
        execution_class: if expected.preview.commit_class == crate::vfs::FileCommitClass::BestEffort
            || !expected.preview.file_flush
            || !expected.preview.directory_flush
        {
            ProviderAuditExecutionClass::BestEffort
        } else {
            ProviderAuditExecutionClass::Assured
        },
        source_removal_best_effort_expected: false,
        target_copy_new_best_effort_expected: expected.preview.commit_class
            == crate::vfs::FileCommitClass::BestEffort,
        target_replace_best_effort_expected: false,
        source_removal_best_effort_actual: None,
        target_copy_new_best_effort_actual: None,
        target_replace_best_effort_actual: None,
    };
    let prepared = match request.prepare(cancel) {
        Ok(prepared) => prepared,
        Err(error)
            if cancel.is_cancelled()
                && crate::error_code(&error) == crate::ErrorCode::Interrupted =>
        {
            queue.mark_running(operation_id)?;
            return complete_file_queue_entry(
                queue,
                operation_id,
                Progress::default(),
                OpStats {
                    cancelled: true,
                    ..Default::default()
                },
                "local_archive_pack",
                context,
                pending_completion,
            );
        }
        Err(error) => return Err(error),
    };
    let current = match prepared.binding() {
        Ok(current) => current,
        Err(error)
            if cancel.is_cancelled()
                && crate::error_code(&error) == crate::ErrorCode::Interrupted =>
        {
            queue.mark_running(operation_id)?;
            return complete_file_queue_entry(
                queue,
                operation_id,
                Progress::default(),
                OpStats {
                    cancelled: true,
                    ..Default::default()
                },
                "local_archive_pack",
                context,
                pending_completion,
            );
        }
        Err(error) => return Err(error),
    };
    if current != expected {
        let reason = "ZIP-Auswahl, Metadaten, Auslassungen oder Zielklasse haben sich geändert; noch kein Archiv erzeugt".to_string();
        queue.require_confirmation(
            operation_id,
            QueueAttention::ArchivePackChanged {
                reason: reason.clone(),
                confirmation: format!("ZIP {}", short_id(operation_id)),
                current: Box::new(current),
            },
        )?;
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::File,
            reason,
        });
    }
    queue.mark_running(operation_id)?;
    queue.append_file_audit(
        operation_id,
        "local_archive_pack",
        "started",
        &Progress::default(),
        &context,
        None,
    )?;
    let mut queue_progress = QueueProgress::new(queue, operation_id, cancel, on_progress);
    let result = prepared.execute(
        Some(ArchivePackOwner {
            queue: &queue.path,
            operation_id,
        }),
        &mut |event| {
            let phase = match event.phase {
                ArchivePackPhase::Packing => "archive_pack",
                ArchivePackPhase::Finalizing => "archive_finalize",
                ArchivePackPhase::Verifying => "archive_verify",
                ArchivePackPhase::Committing => "archive_commit",
            };
            queue_progress.report(&Progress {
                files_total: 1,
                files_done: 0,
                bytes_total: expected.preview.source_bytes,
                bytes_done: event.source_bytes,
                current: format!(
                    "{} · {} Datei(en), {} Ordner verarbeitet",
                    expected.preview.target.display(),
                    event.files,
                    event.directories
                ),
                phase: phase.into(),
                directory_stream_report: Default::default(),
                directory_dacl_report: Default::default(),
                file_dacl_report: Default::default(),
                created_directory_dacl_report: Default::default(),
            });
        },
    );
    let (latest, persistence_error) = queue_progress.finish();
    let report = match &result {
        Ok(report) => report,
        Err(failure) => &failure.report,
    };
    actuals
        .temp_recovery_paths
        .extend(report.recovery_paths.iter().cloned());
    actuals.target_copy_new_best_effort = report.target_copy_new_best_effort;
    actuals.target_write_limitations = report
        .preview
        .issues
        .iter()
        .map(|issue| format!("{}: {}", issue.path, issue.reason))
        .collect();
    let result = result.map_err(|failure| failure.error);
    if let Some(error) = persistence_error {
        return Err(possible_mutation_progress_error(error, &result));
    }
    let report = result?;
    let stats = OpStats {
        files: u64::from(report.status == ArchivePackStatus::Complete),
        bytes: report.source_bytes,
        skipped: report
            .preview
            .issues
            .iter()
            .filter(|issue| issue.content_omitted)
            .count() as u64,
        source_read_limitations: report
            .preview
            .issues
            .iter()
            .filter(|issue| issue.content_omitted)
            .map(|issue| format!("{}: {}", issue.path, issue.reason))
            .collect(),
        target_write_limitations: report
            .preview
            .issues
            .iter()
            .filter(|issue| !issue.content_omitted)
            .map(|issue| format!("{}: {}", issue.path, issue.reason))
            .collect(),
        cancelled: report.status == ArchivePackStatus::Cancelled,
        temp_recovery_paths: report.recovery_paths,
        target_copy_new_best_effort: report.target_copy_new_best_effort.unwrap_or(false),
        ..Default::default()
    };
    complete_file_queue_entry(
        queue,
        operation_id,
        latest,
        stats,
        "local_archive_pack",
        context,
        pending_completion,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    fn fixture() -> (
        tempfile::TempDir,
        OperationQueue,
        QueueOperation,
        LocalQueueExecution,
    ) {
        let temp = tempfile::Builder::new()
            .prefix("dd-zipq-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        for name in ["source", "target", "state"] {
            fs::create_dir(root.join(name)).unwrap();
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(root.join("source/file"), vec![8; 300_000]).unwrap();
        let request = ArchivePackRequest {
            source_root: root.join("source"),
            items: vec![root.join("source/file")],
            target_dir: root.join("target"),
            target_name: "archive.zip".into(),
            options: Default::default(),
            sandbox: None,
        };
        let expected = request.prepare(&Cancel::new()).unwrap().binding().unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        (
            temp,
            queue,
            QueueOperation::ArchivePack {
                request: Box::new(request),
                expected: Box::new(expected),
            },
            LocalQueueExecution {
                sandbox: None,
                sync_quarantine: None,
                max_delete_entries: 0,
                max_delete_fraction: 0.0,
            },
        )
    }
    #[test]
    fn archive_pack_queue_roundtrip_pending_reopen_and_terminal_completion_retry() {
        let (_temp, queue, operation, execution) = fixture();
        let path = queue.path.clone();
        queue.enqueue(operation).unwrap();
        drop(queue);
        let queue = OperationQueue::open(path).unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        let QueueOperation::ArchivePack { request, .. } = &entry.operation else {
            panic!()
        };
        let target = request.target_dir.join(&request.target_name);
        queue
            .fail_next_file_completion_commit
            .store(true, Ordering::SeqCst);
        let result = execute_local_queue_entry_detailed(
            &queue,
            entry,
            &execution,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert!(matches!(result, QueueExecutionResult::File(_)));
        assert!(target.is_file());
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
        assert!(queue.claim_next().unwrap().is_none());
    }
    #[test]
    fn archive_pack_queue_drift_requires_exact_effect_approval_before_mutation() {
        let (_temp, queue, operation, execution) = fixture();
        let QueueOperation::ArchivePack { request, .. } = &operation else {
            panic!()
        };
        let target = request.target_dir.join(&request.target_name);
        let source = request.items[0].clone();
        queue.enqueue(operation).unwrap();
        fs::write(source, b"new size").unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        assert!(matches!(
            execute_local_queue_entry_detailed(
                &queue,
                entry,
                &execution,
                &Cancel::new(),
                &mut |_| {}
            )
            .unwrap(),
            QueueExecutionResult::AwaitingConfirmation { .. }
        ));
        assert!(!target.exists());
        let waiting = &queue.snapshot().entries[0];
        let id = waiting.operation_id.clone();
        let attention = waiting.attention.clone().unwrap();
        assert!(matches!(
            attention,
            QueueAttention::ArchivePackChanged { .. }
        ));
        assert!(queue
            .approve_attention_effects(&id, &attention, false)
            .is_err());
        let mut forged = attention.clone();
        if let QueueAttention::ArchivePackChanged { current, .. } = &mut forged {
            current.digest[0] ^= 1;
        }
        assert!(queue.approve_attention_effects(&id, &forged, true).is_err());
        queue
            .approve_attention_effects(&id, &attention, true)
            .unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        execute_local_queue_entry_detailed(&queue, entry, &execution, &Cancel::new(), &mut |_| {})
            .unwrap();
        assert!(target.is_file());
    }
    #[test]
    fn archive_pack_queue_running_recovery_never_retries_mutation() {
        let (_temp, queue, operation, _execution) = fixture();
        let path = queue.path.clone();
        queue.enqueue(operation).unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        queue.mark_running(&entry.operation_id).unwrap();
        drop(queue);
        let reopened = OperationQueue::open(path).unwrap();
        let snapshot = reopened.snapshot();
        let entry = &snapshot.entries[0];
        assert_eq!(entry.state, QueueState::Interrupted);
        assert_eq!(entry.error_code, Some(crate::ErrorCode::OutcomeUnclear));
        assert!(entry
            .temp_recovery_paths
            .iter()
            .any(|path| path.ends_with("archive.zip")));
        assert!(reopened.retry(&entry.operation_id).is_err());
        assert!(reopened.claim_next().unwrap().is_none());
        assert!(reopened
            .acknowledge_unclear_outcome(&entry.operation_id, false)
            .is_err());
        reopened
            .acknowledge_unclear_outcome(&entry.operation_id, true)
            .unwrap();
        assert_eq!(reopened.snapshot().entries[0].state, QueueState::Cancelled);
        assert!(reopened.claim_next().unwrap().is_none());
    }
    #[test]
    fn archive_pack_queue_cancel_is_terminal_and_keeps_source() {
        let (_temp, queue, operation, execution) = fixture();
        let QueueOperation::ArchivePack { request, .. } = &operation else {
            panic!()
        };
        let target = request.target_dir.join(&request.target_name);
        let source = request.items[0].clone();
        queue.enqueue(operation).unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        let cancel = Cancel::new();
        execute_local_queue_entry_detailed(&queue, entry, &execution, &cancel, &mut |progress| {
            if progress.bytes_done > 0 {
                cancel.cancel();
            }
        })
        .unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Cancelled);
        assert!(!target.exists());
        assert_eq!(fs::metadata(source).unwrap().len(), 300_000);
    }
}
