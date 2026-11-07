//! Der Phasenwechsel erreicht die Platte, bevor ein Provider angesprochen wird.
use super::*;
use crate::archive_upload::ArchiveGeneration;
use crate::generated_source::GeneratedSourceStore;

pub(super) enum GenerationResult {
    Ready(Box<QueueEntry>),
    Waiting(String),
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn generate(
    queue: &OperationQueue,
    entry: QueueEntry,
    execution: &LocalQueueExecution,
    snapshot_store: Option<&crate::remote_snapshot::SnapshotStore>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    actuals: &mut FileOperationActuals,
) -> io::Result<GenerationResult> {
    validate_operation(&entry.operation)?;
    validate_generated_entry(&entry, queue._process_lock.root.path())?;
    let QueueOperation::ArchiveUpload {
        generation,
        workspace,
        ..
    } = &entry.operation
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Keine Archiverzeugung",
        ));
    };
    if generation.sandbox() != &execution.sandbox {
        return Err(crate::real_mode::plan_drift(
            "Archiv-Testbereich hat sich geändert",
        ));
    }
    if cancel.is_cancelled() {
        queue.mark_running(&entry.operation_id)?;
        queue.cancel(
            &entry.operation_id,
            Progress {
                phase: "cancelled".into(),
                ..Progress::default()
            },
        )?;
        return Ok(GenerationResult::Cancelled);
    }
    let context = generation_context(generation);
    let store = GeneratedSourceStore::for_queue(queue)?;
    let work = store.open_workspace(workspace)?;
    let operation_id = &entry.operation_id;
    let report_progress = |progress: &mut QueueProgress<'_>,
                           current: String,
                           phase: String,
                           files: u64,
                           bytes: u64,
                           total_files: u64,
                           total_bytes: u64| {
        progress.report(&Progress {
            current,
            phase,
            files_total: total_files,
            files_done: files,
            bytes_total: total_bytes,
            bytes_done: bytes,
            ..Progress::default()
        });
    };
    let reference = match generation.as_ref() {
        ArchiveGeneration::Pack { request, expected } => {
            let prepared = request.prepare(cancel)?;
            let current = prepared.binding()?;
            if &current != expected.as_ref() {
                let reason =
                    "ZIP-Auswahl oder Auslassungen geändert; noch kein Archiv erzeugt".to_owned();
                queue.require_confirmation(
                    operation_id,
                    QueueAttention::ArchivePackChanged {
                        reason: reason.clone(),
                        confirmation: format!("ZIP {}", short_id(operation_id)),
                        current: Box::new(current),
                    },
                )?;
                return Ok(GenerationResult::Waiting(reason));
            }
            queue.mark_running(operation_id)?;
            queue.append_file_audit(
                operation_id,
                "archive_upload_local_phase",
                "started",
                &Progress::default(),
                &context,
                None,
            )?;
            actuals
                .temp_recovery_paths
                .push(work.path().to_string_lossy().into_owned());
            let mut progress = QueueProgress::new(queue, operation_id, cancel, on_progress);
            let result = prepared.execute_generated(
                Some(crate::archive_pack::ArchivePackOwner {
                    queue: &queue.path,
                    operation_id,
                }),
                &mut |event| {
                    report_progress(
                        &mut progress,
                        request.target_name.clone(),
                        format!("archive_pack_{:?}", event.phase).to_lowercase(),
                        event.files,
                        event.source_bytes,
                        expected.preview.files,
                        expected.preview.source_bytes,
                    );
                },
            );
            let (latest, persistence_error) = progress.finish();
            if let Some(error) = persistence_error {
                return Err(crate::coded_error(
                    error.kind(),
                    crate::ErrorCode::OutcomeUnclear,
                    format!("Archivphase unterbrochen; Zwischenstand prüfen: {error}"),
                ));
            }
            let generated = result.map_err(|failure| failure.error)?;
            if generated.report.status == crate::archive_pack::ArchivePackStatus::Cancelled {
                actuals
                    .temp_recovery_paths
                    .extend(generated.report.recovery_paths);
                queue.finish_file(
                    operation_id,
                    true,
                    latest,
                    &actuals.temp_recovery_paths,
                    &[],
                    false,
                )?;
                return Ok(GenerationResult::Cancelled);
            }
            work.seal_archive(generated, cancel)?
        }
        ArchiveGeneration::Extract { request, expected } => {
            let prepared = if let Some(snapshot) = &request.snapshot {
                let store = snapshot_store.ok_or_else(|| {
                    crate::coded_error(
                        io::ErrorKind::InvalidInput,
                        crate::ErrorCode::InvalidInput,
                        "Archivmomentaufnahme benötigt den bekannten Cache",
                    )
                })?;
                request
                    .prepare_with_snapshot(store.resolve(&snapshot.reference, cancel)?, cancel)?
            } else {
                request.prepare(cancel)?
            };
            let _snapshot = prepared.hold_snapshot();
            let current = prepared.binding()?;
            if &current != expected.as_ref() {
                let reason =
                    "Archivquelle oder Entpackumfang geändert; noch nichts entpackt".to_owned();
                queue.require_confirmation(
                    operation_id,
                    QueueAttention::ArchiveExtractChanged {
                        reason: reason.clone(),
                        confirmation: format!("ZIP ENTPACKEN {}", short_id(operation_id)),
                        current: Box::new(current),
                    },
                )?;
                return Ok(GenerationResult::Waiting(reason));
            }
            queue.mark_running(operation_id)?;
            queue.append_file_audit(
                operation_id,
                "archive_upload_local_phase",
                "started",
                &Progress::default(),
                &context,
                None,
            )?;
            actuals
                .temp_recovery_paths
                .push(work.path().to_string_lossy().into_owned());
            let mut progress = QueueProgress::new(queue, operation_id, cancel, on_progress);
            let result = prepared.execute_generated(
                Some(crate::archive_extract::ArchiveExtractOwner {
                    queue: &queue.path,
                    operation_id,
                }),
                &mut |event| {
                    report_progress(
                        &mut progress,
                        event.current.clone(),
                        format!("archive_extract_{:?}", event.phase).to_lowercase(),
                        event.files,
                        event.bytes,
                        expected.preview.files,
                        expected.preview.source_bytes,
                    );
                },
            );
            let (latest, persistence_error) = progress.finish();
            let report = match &result {
                Ok(generated) => &generated.report,
                Err(failure) => &failure.report,
            };
            queue.record_archive_extract_report(operation_id, report)?;
            if let Some(error) = persistence_error {
                return Err(crate::coded_error(
                    error.kind(),
                    crate::ErrorCode::OutcomeUnclear,
                    format!("Entpackphase unterbrochen; Zwischenstand prüfen: {error}"),
                ));
            }
            let generated = result.map_err(|failure| failure.error)?;
            if generated.report.status == crate::archive_extract::ArchiveExtractStatus::Cancelled {
                actuals
                    .temp_recovery_paths
                    .extend(generated.report.recovery_paths);
                queue.finish_file(
                    operation_id,
                    true,
                    latest,
                    &actuals.temp_recovery_paths,
                    &[],
                    false,
                )?;
                return Ok(GenerationResult::Cancelled);
            }
            let label = expected
                .preview
                .source_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("ZIP-Auswahl");
            work.seal_extraction(generated, label.to_owned(), cancel)?
        }
    };
    queue.append_file_audit(
        operation_id,
        "archive_upload_local_phase",
        "completed",
        &Progress {
            files_total: reference.files,
            files_done: reference.files,
            bytes_total: reference.bytes,
            bytes_done: reference.bytes,
            current: reference.label.clone(),
            phase: "archive_upload_ready".into(),
            ..Progress::default()
        },
        &context,
        None,
    )?;
    let ready = queue.transition_archive_upload(&entry, reference, cancel)?;
    *actuals = FileOperationActuals::default();
    Ok(GenerationResult::Ready(Box::new(ready)))
}

fn generation_context(generation: &ArchiveGeneration) -> ProviderAuditContext {
    let (commit, file_flush, directory_flush) = match generation {
        ArchiveGeneration::Pack { expected, .. } => (
            expected.preview.commit_class,
            expected.preview.file_flush,
            expected.preview.directory_flush,
        ),
        ArchiveGeneration::Extract { expected, .. } => (
            expected.preview.commit_class,
            expected.preview.file_flush,
            expected.preview.directory_flush,
        ),
    };
    let weaker = commit == crate::vfs::FileCommitClass::BestEffort;
    ProviderAuditContext {
        source_provider: Some(crate::ProviderKind::Local),
        target_provider: crate::ProviderKind::Local,
        execution_class: if weaker || !file_flush || !directory_flush {
            ProviderAuditExecutionClass::BestEffort
        } else {
            ProviderAuditExecutionClass::Assured
        },
        source_removal_best_effort_expected: false,
        target_copy_new_best_effort_expected: weaker,
        target_replace_best_effort_expected: false,
        source_removal_best_effort_actual: None,
        target_copy_new_best_effort_actual: None,
        target_replace_best_effort_actual: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_pack::{ArchivePackOptions, ArchivePackRequest};
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn archivuebergabe_klaert_journalfehler_vor_jedem_providerzugriff() {
        for fault in ["before", "after", "durability"] {
            let area = tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .unwrap();
            let root = area.path().canonicalize().unwrap();
            std::fs::create_dir(root.join("source")).unwrap();
            std::fs::write(root.join("source/file"), b"content").unwrap();
            let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
            let store = GeneratedSourceStore::for_queue(&queue).unwrap();
            let workspace = store.begin().unwrap();
            let request = ArchivePackRequest {
                source_root: root.join("source"),
                items: vec![root.join("source/file")],
                target_dir: workspace.path().to_path_buf(),
                target_name: "result.zip".into(),
                options: ArchivePackOptions::default(),
                sandbox: None,
            };
            let prepared = request.prepare(&Cancel::new()).unwrap();
            let expected = prepared.binding().unwrap();
            let target = BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(crate::WebDavConnectionConfig {
                    origin: "https://artificial.example.invalid".into(),
                    username: "tester".into(),
                    requested_root: "/".into(),
                    certificate: crate::FtpsCertificatePin { sha256: [1; 32] },
                    certificate_policy: crate::FtpsCertificatePolicy::PinnedCertificate,
                    password_reference: crate::KeychainSecretReference::new("artificial", "unused")
                        .unwrap(),
                }),
                root: "/pane".into(),
            };
            let entry = queue
                .enqueue(QueueOperation::ArchiveUpload {
                    generation: Box::new(ArchiveGeneration::Pack {
                        request: Box::new(request),
                        expected: Box::new(expected),
                    }),
                    workspace: workspace.reference(),
                    target: Box::new(target),
                })
                .unwrap();
            queue.claim_next().unwrap().unwrap();
            queue.mark_running(&entry.operation_id).unwrap();
            let generated = prepared.execute_generated(None, &mut |_| {}).unwrap();
            let reference = workspace.seal_archive(generated, &Cancel::new()).unwrap();
            if fault == "durability" {
                queue.fail_reconcile_sync.store(true, Ordering::SeqCst);
            }
            let result = queue.transition_archive_upload_using(
                &entry,
                reference.clone(),
                &Cancel::new(),
                |path, snapshot| {
                    if fault != "before" {
                        write_snapshot_bound(
                            &queue._process_lock.root,
                            path,
                            snapshot,
                            AtomicWriteMode::Replace,
                        )?;
                    }
                    Err(io::Error::other("synthetic transition write failure"))
                },
            );
            if fault == "after" {
                let changed = result.unwrap();
                assert_eq!(changed.state, QueueState::Validating);
                assert_eq!(changed.generated_source, Some(reference.clone()));
            } else {
                assert!(result.is_err());
                assert!(matches!(
                    queue.snapshot().entries[0].operation,
                    QueueOperation::ArchiveUpload { .. }
                ));
                if fault == "durability" {
                    assert!(queue.claim_next().is_err());
                }
            }
            drop(store);
            drop(queue);
            let reopened = OperationQueue::open(root.join("config/queue.json")).unwrap();
            let saved = reopened.snapshot().entries[0].clone();
            assert_eq!(saved.state, QueueState::Interrupted);
            if fault == "before" {
                assert!(saved.generated_source.is_none());
                assert_eq!(saved.error_code, Some(crate::ErrorCode::OutcomeUnclear));
                assert!(reopened.retry(&entry.operation_id).is_err());
            } else {
                assert_eq!(saved.generated_source, Some(reference));
                assert!(matches!(
                    saved.operation,
                    QueueOperation::ProviderFile { .. }
                ));
            }
            assert_eq!(std::fs::read(root.join("source/file")).unwrap(), b"content");
        }
    }
}
