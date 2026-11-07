//! ZIP-Entpacken verwendet denselben Dateiabschluss wie Copy und ZIP-Erzeugung.
use super::*;
use crate::archive_extract::{
    ArchiveExtractBinding, ArchiveExtractOwner, ArchiveExtractRequest, ArchiveExtractStatus,
};
#[allow(clippy::too_many_arguments)]
pub(super) fn execute(
    queue: &OperationQueue,
    operation_id: &str,
    request: ArchiveExtractRequest,
    expected: ArchiveExtractBinding,
    execution: &LocalQueueExecution,
    snapshot_store: Option<&crate::remote_snapshot::SnapshotStore>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    actuals: &mut FileOperationActuals,
    pending_completion: &mut Option<Box<PendingFileCompletion>>,
) -> io::Result<QueueExecutionResult> {
    if request.sandbox != execution.sandbox {
        return Err(crate::real_mode::plan_drift("ZIP-Testbereich änderte sich"));
    }
    let context = ProviderAuditContext {
        source_provider: Some(crate::ProviderKind::Local),
        target_provider: crate::ProviderKind::Local,
        execution_class: if expected.preview.commit_class == crate::vfs::FileCommitClass::BestEffort
            || expected.preview.replace_best_effort && request.options.overwrite
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
        target_replace_best_effort_expected: expected.preview.replace_best_effort
            && request.options.overwrite,
        source_removal_best_effort_actual: None,
        target_copy_new_best_effort_actual: None,
        target_replace_best_effort_actual: None,
    };
    let preparation = if let Some(source) = &request.snapshot {
        snapshot_store.ok_or_else(|| crate::coded_error(io::ErrorKind::InvalidInput, crate::ErrorCode::InvalidInput,
            "Archivmomentaufnahme ist ohne bekannten Cachekontext nicht verfügbar; Archiv erneut öffnen"))
            .and_then(|store| store.resolve(&source.reference, cancel))
            .and_then(|snapshot| request.prepare_with_snapshot(snapshot, cancel))
    } else {
        request.prepare(cancel)
    };
    let prepared = match preparation {
        Ok(prepared) => prepared,
        Err(error)
            if cancel.is_cancelled()
                && crate::error_code(&error) == crate::ErrorCode::Interrupted =>
        {
            return cancel_validation(queue, operation_id, &expected, context, pending_completion)
        }
        Err(error) => return Err(error),
    };
    let _snapshot_guard = prepared.hold_snapshot();
    let current = match prepared.binding() {
        Ok(binding) => binding,
        Err(error)
            if cancel.is_cancelled()
                && crate::error_code(&error) == crate::ErrorCode::Interrupted =>
        {
            return cancel_validation(queue, operation_id, &expected, context, pending_completion)
        }
        Err(error) => return Err(error),
    };
    if current != expected {
        let reason="Archivquelle, bekannte Inhalte, Auslassungen oder Zielbestand änderten sich; noch nichts entpackt".to_string();
        queue.require_confirmation(
            operation_id,
            QueueAttention::ArchiveExtractChanged {
                reason: reason.clone(),
                confirmation: format!("ZIP ENTPACKEN {}", short_id(operation_id)),
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
        "local_archive_extract",
        "started",
        &Progress::default(),
        &context,
        None,
    )?;
    let mut progress = QueueProgress::new(queue, operation_id, cancel, on_progress);
    let result = prepared.execute(
        Some(ArchiveExtractOwner {
            queue: &queue.path,
            operation_id,
        }),
        &mut |event| {
            progress.report(&Progress {
                files_total: expected.preview.files,
                files_done: event.files,
                bytes_total: expected.preview.source_bytes,
                bytes_done: event.bytes,
                current: event.current.clone(),
                phase: format!("archive_extract_{:?}", event.phase).to_lowercase(),
                directory_stream_report: Default::default(),
                directory_dacl_report: Default::default(),
                file_dacl_report: Default::default(),
                created_directory_dacl_report: Default::default(),
            })
        },
    );
    let (latest, persistence_error) = progress.finish();
    let report = match &result {
        Ok(report) => report,
        Err(failure) => &failure.report,
    };
    actuals
        .temp_recovery_paths
        .extend(report.recovery_paths.iter().cloned());
    actuals.target_copy_new_best_effort = report.target_copy_new_best_effort;
    actuals.target_replace_best_effort = report.target_replace_best_effort;
    actuals.target_write_limitations = report
        .preview
        .issues
        .iter()
        .map(|i| format!("{}: {}", i.path, i.reason))
        .collect();
    queue.record_archive_extract_report(operation_id,report).map_err(|error|if report.has_effect {crate::coded_error(io::ErrorKind::Other,crate::ErrorCode::OutcomeUnclear,format!("ZIP-Teilwirkung vorhanden, ihre Queuebilanz konnte nicht gespeichert werden: {error}"))}else{error})?;
    if let Some(error) = persistence_error {
        return Err(possible_mutation_progress_error(
            error,
            &result
                .as_ref()
                .map(|_| ())
                .map_err(|e| io::Error::other(e.to_string())),
        ));
    }
    let report = result.map_err(|failure| failure.error)?;
    let stats = OpStats {
        files: report.files,
        bytes: report.bytes,
        skipped: report.skipped,
        cancelled: report.status == ArchiveExtractStatus::Cancelled,
        temp_recovery_paths: report.recovery_paths,
        target_copy_new_best_effort: report.target_copy_new_best_effort.unwrap_or(false),
        target_replace_best_effort: report.target_replace_best_effort.unwrap_or(false),
        source_read_limitations: report
            .preview
            .issues
            .iter()
            .filter(|i| i.content_omitted)
            .map(|i| format!("{}: {}", i.path, i.reason))
            .collect(),
        target_write_limitations: report
            .preview
            .issues
            .iter()
            .filter(|i| !i.content_omitted)
            .map(|i| format!("{}: {}", i.path, i.reason))
            .collect(),
        ..Default::default()
    };
    complete_file_queue_entry(
        queue,
        operation_id,
        latest,
        stats,
        "local_archive_extract",
        context,
        pending_completion,
    )
}
fn cancel_validation(
    queue: &OperationQueue,
    operation_id: &str,
    expected: &ArchiveExtractBinding,
    context: ProviderAuditContext,
    pending_completion: &mut Option<Box<PendingFileCompletion>>,
) -> io::Result<QueueExecutionResult> {
    queue.mark_running(operation_id)?;
    let report = crate::archive_extract::ArchiveExtractReport {
        preview: expected.preview.clone(),
        status: ArchiveExtractStatus::Cancelled,
        files: 0,
        directories: 0,
        bytes: 0,
        skipped: expected.preview.omitted_entries,
        failed: 0,
        unstarted: expected.preview.files,
        has_effect: false,
        recovery_paths: Vec::new(),
        target_copy_new_best_effort: Some(false),
        target_replace_best_effort: Some(false),
    };
    queue.record_archive_extract_report(operation_id, &report)?;
    complete_file_queue_entry(
        queue,
        operation_id,
        Progress::default(),
        OpStats {
            cancelled: true,
            skipped: report.skipped,
            ..Default::default()
        },
        "local_archive_extract",
        context,
        pending_completion,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    fn fixture() -> (
        tempfile::TempDir,
        OperationQueue,
        QueueOperation,
        LocalQueueExecution,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        for name in ["src", "dst", "state"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        std::fs::set_permissions(root.join("state"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let mut zip = zip::ZipWriter::new(std::fs::File::create(root.join("src/a.zip")).unwrap());
        for name in ["first", "second"] {
            zip.start_file(
                name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
            zip.write_all(&vec![7; 50000]).unwrap();
        }
        drop(zip.finish().unwrap());
        let request = ArchiveExtractRequest {
            snapshot: None,
            source_root: root.join("src"),
            source_path: root.join("src/a.zip"),
            target_dir: root.join("dst"),
            options: Default::default(),
            selection: crate::archive_extract::ArchiveExtractSelection::All,
            sandbox: None,
        };
        let expected = request.prepare(&Cancel::new()).unwrap().binding().unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        (
            temp,
            queue,
            QueueOperation::ArchiveExtract {
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
    fn roundtrip_report_und_terminaler_abschluss_ohne_zweite_datenwirkung() {
        let (_temp, queue, operation, execution) = fixture();
        let path = queue.path.clone();
        queue.enqueue(operation).unwrap();
        drop(queue);
        let queue = OperationQueue::open(&path).unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
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
        let snapshot = queue.snapshot();
        let report = snapshot.entries[0].archive_extract_report.as_ref().unwrap();
        assert_eq!(report.files, 2);
        assert_eq!(report.bytes, 100000);
        assert_eq!(snapshot.entries[0].state, QueueState::Completed);
        drop(queue);
        let queue = OperationQueue::open(path).unwrap();
        assert_eq!(
            queue.snapshot().entries[0]
                .archive_extract_report
                .as_ref()
                .unwrap()
                .files,
            2
        );
        assert!(queue.claim_next().unwrap().is_none());
    }
    #[test]
    fn abbruch_nach_erstem_commit_ist_persistent_und_nicht_wiederholbar() {
        let (_temp, queue, operation, execution) = fixture();
        let id = queue.enqueue(operation).unwrap().operation_id;
        let entry = queue.claim_next().unwrap().unwrap();
        let cancel = Cancel::new();
        let result =
            execute_local_queue_entry_detailed(&queue, entry, &execution, &cancel, &mut |p| {
                if p.files_done == 1 {
                    cancel.cancel();
                }
            })
            .unwrap();
        assert!(matches!(result, QueueExecutionResult::File(_)));
        let snapshot = queue.snapshot();
        let report = snapshot.entries[0].archive_extract_report.as_ref().unwrap();
        assert_eq!(report.files, 1);
        assert_eq!(report.bytes, 50000);
        assert!(report.has_effect);
        assert_eq!(report.status, ArchiveExtractStatus::Cancelled);
        assert!(queue.retry(&id).is_err());
    }
    #[test]
    fn quellendrift_verlangt_frische_bestaetigung_vor_mutation() {
        let (_temp, queue, operation, execution) = fixture();
        let QueueOperation::ArchiveExtract { request, .. } = &operation else {
            panic!()
        };
        let target = request.target_dir.clone();
        let source = request.source_path.clone();
        let id = queue.enqueue(operation).unwrap().operation_id;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(source)
            .unwrap();
        file.write_all(b"source changed").unwrap();
        drop(file);
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
        assert!(std::fs::read_dir(target).unwrap().next().is_none());
        assert!(matches!(
            queue.snapshot().entries[0].attention,
            Some(QueueAttention::ArchiveExtractChanged { .. })
        ));
        assert!(queue.approve_attention(&id, "falsch").is_err());
    }
    #[test]
    fn laufend_nach_neustart_ist_unklar_ohne_blinden_retry() {
        let (_temp, queue, operation, _execution) = fixture();
        let path = queue.path.clone();
        let id = queue.enqueue(operation).unwrap().operation_id;
        queue.claim_next().unwrap().unwrap();
        queue.mark_running(&id).unwrap();
        drop(queue);
        let queue = OperationQueue::open(path).unwrap();
        let entry = &queue.snapshot().entries[0];
        assert_eq!(entry.state, QueueState::Interrupted);
        assert_eq!(entry.error_code, Some(crate::ErrorCode::OutcomeUnclear));
        assert!(queue.retry(&id).is_err());
    }
    #[test]
    fn abbruch_waehrend_validierung_ist_cancelled_ohne_datenwirkung() {
        let (_temp, queue, operation, execution) = fixture();
        queue.enqueue(operation).unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(matches!(
            execute_local_queue_entry_detailed(&queue, entry, &execution, &cancel, &mut |_| {})
                .unwrap(),
            QueueExecutionResult::File(_)
        ));
        let snapshot = queue.snapshot();
        let entry = &snapshot.entries[0];
        assert_eq!(entry.state, QueueState::Cancelled);
        assert!(!entry.archive_extract_report.as_ref().unwrap().has_effect);
        assert_eq!(entry.archive_extract_report.as_ref().unwrap().unstarted, 2);
    }
    #[test]
    fn crc_queue_failed_und_bilanzabschluss_sind_mutationslos() {
        let (_temp, queue, operation, execution) = fixture();
        let QueueOperation::ArchiveExtract { request, .. } = &operation else {
            panic!()
        };
        let request = request.as_ref().clone();
        let mut bytes = std::fs::read(&request.source_path).unwrap();
        bytes[35] ^= 1;
        std::fs::write(&request.source_path, bytes).unwrap();
        let expected = request.prepare(&Cancel::new()).unwrap().binding().unwrap();
        let id = queue
            .enqueue(QueueOperation::ArchiveExtract {
                request: Box::new(request.clone()),
                expected: Box::new(expected),
            })
            .unwrap()
            .operation_id;
        let entry = queue.claim_next().unwrap().unwrap();
        assert!(execute_local_queue_entry_detailed(
            &queue,
            entry,
            &execution,
            &Cancel::new(),
            &mut |_| {}
        )
        .is_err());
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.entries[0].state, QueueState::Failed);
        assert!(!request.target_dir.join("first").exists());
        assert_eq!(
            snapshot.entries[0]
                .archive_extract_report
                .as_ref()
                .unwrap()
                .files,
            0
        );
        assert!(queue.retry(&id).is_err());
        queue.acknowledge_archive_extract_outcome(&id).unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Cancelled);
        assert!(queue.snapshot().entries[0].archive_extract_report.is_some());
    }
    #[test]
    fn auswahl_queue_reopen_behaelt_nur_eine_datei_und_sperrt_running_retry() {
        for running in [false, true] {
            let (_temp, queue, mut operation, execution) = fixture();
            let QueueOperation::ArchiveExtract { request, expected } = &mut operation else {
                panic!()
            };
            let reader = crate::archive_read::ArchiveRead::open(
                &request.source_root,
                Path::new("a.zip"),
                crate::archive_read::CatalogBudget::default(),
                &Cancel::new(),
            )
            .unwrap();
            request.selection = reader
                .extract_entry_selection(reader.entries()[0].id, &Cancel::new())
                .unwrap();
            **expected = request.prepare(&Cancel::new()).unwrap().binding().unwrap();
            let target = request.target_dir.clone();
            let id = queue.enqueue(operation).unwrap().operation_id;
            if running {
                queue.claim_next().unwrap().unwrap();
                queue.mark_running(&id).unwrap();
            }
            let path = queue.path.clone();
            drop(queue);
            // Version 30 besaß die lokale Einzelauswahl bereits; ihre Bedeutung
            // bleibt bei der neuen Snapshotquellenklasse unverändert erhalten.
            if !running {
                let mut json: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                json["version"] = serde_json::json!(30);
                std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
            }
            let queue = OperationQueue::open(path).unwrap();
            if running {
                assert_eq!(queue.snapshot().entries[0].state, QueueState::Interrupted);
                assert!(queue.retry(&id).is_err());
                assert!(queue.claim_next().unwrap().is_none());
                assert_eq!(std::fs::read_dir(target).unwrap().count(), 0);
            } else {
                let entry = queue.claim_next().unwrap().unwrap();
                execute_local_queue_entry_detailed(
                    &queue,
                    entry,
                    &execution,
                    &Cancel::new(),
                    &mut |_| {},
                )
                .unwrap();
                assert!(target.join("first").is_file());
                assert!(!target.join("second").exists());
                assert_eq!(
                    queue.snapshot().entries[0]
                        .archive_extract_report
                        .as_ref()
                        .unwrap()
                        .files,
                    1
                );
            }
        }
    }
    #[test]
    fn journal29_migriert_ganzes_archiv_ohne_neue_auswahl_oder_drift() {
        let (_temp, queue, operation, _) = fixture();
        let expected = match &operation {
            QueueOperation::ArchiveExtract { expected, .. } => expected.clone(),
            _ => panic!(),
        };
        queue.enqueue(operation).unwrap();
        let path = queue.path.clone();
        drop(queue);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        json["version"] = serde_json::json!(29);
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let queue = OperationQueue::open(&path).unwrap();
        assert_eq!(queue.snapshot().version, super::QUEUE_JOURNAL_VERSION);
        let snapshot = queue.snapshot();
        let QueueOperation::ArchiveExtract {
            request,
            expected: migrated,
        } = &snapshot.entries[0].operation
        else {
            panic!()
        };
        assert_eq!(
            request.selection,
            crate::archive_extract::ArchiveExtractSelection::All
        );
        assert_eq!(migrated, &expected);
        assert!(migrated.preview.selection_path.is_empty());
        assert!(serde_json::to_value(&expected.preview)
            .unwrap()
            .get("selection_path")
            .is_none());
        assert_eq!(
            request.prepare(&Cancel::new()).unwrap().binding().unwrap(),
            *expected
        );
        drop(queue);
        json["version"] = serde_json::json!(super::QUEUE_JOURNAL_VERSION + 1);
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(OperationQueue::open(&path).is_err());
    }

    #[test]
    fn journal29_kann_keine_nachtraeglich_eingefuegte_auswahl_freigeben() {
        let (_temp, queue, mut operation, _) = fixture();
        let QueueOperation::ArchiveExtract { request, expected } = &mut operation else {
            panic!()
        };
        request.selection = crate::archive_extract::ArchiveExtractSelection::Entry {
            ordinal: 0,
            raw_name: b"first".to_vec(),
        };
        **expected = request.prepare(&Cancel::new()).unwrap().binding().unwrap();
        let target = request.target_dir.clone();
        queue.enqueue(operation).unwrap();
        let path = queue.path.clone();
        drop(queue);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        json["version"] = serde_json::json!(29);
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(OperationQueue::open(&path).is_err());
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }
    fn snapshot_fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        OperationQueue,
        QueueOperation,
        LocalQueueExecution,
        crate::remote_snapshot::SnapshotStore,
    ) {
        let (temp, queue, mut operation, execution) = fixture();
        let cache = tempfile::tempdir().unwrap();
        let store =
            crate::remote_snapshot::SnapshotStore::open(&cache.path().canonicalize().unwrap())
                .unwrap();
        let QueueOperation::ArchiveExtract { request, expected } = &mut operation else {
            unreachable!()
        };
        let downloaded = crate::remote_snapshot::download_remote_snapshot(
            &crate::LocalVfs::new(),
            &request.source_path,
            cache.path(),
            &Cancel::new(),
        )
        .unwrap();
        let validated = store.capture(&downloaded, &Cancel::new()).unwrap();
        let reader = crate::archive_read::ArchiveRead::open_snapshot(
            validated.clone(),
            crate::archive_read::CatalogBudget::default(),
            &Cancel::new(),
        )
        .unwrap();
        request.selection = reader
            .extract_entry_selection(reader.entries()[0].id, &Cancel::new())
            .unwrap();
        // Der Originalserver ist für spätere Claims ausdrücklich irrelevant.
        std::fs::write(&request.source_path, b"new server bytes").unwrap();
        request.source_root = PathBuf::new();
        request.source_path = PathBuf::new();
        request.snapshot = Some(crate::archive_extract::ArchiveSnapshotSource {
            reference: validated.reference().clone(),
            origin: "SFTP · /artificial/a.zip".into(),
            profile: crate::RemoteProfileBinding::new(
                crate::RemoteProfileProtocol::Sftp,
                "artificial",
                1,
            )
            .unwrap(),
        });
        **expected = request
            .prepare_with_snapshot(validated, &Cancel::new())
            .unwrap()
            .binding()
            .unwrap();
        (temp, cache, queue, operation, execution, store)
    }
    #[test]
    fn snapshot_pending_reopen_liest_dieselben_bytes_und_nur_die_bestaetigte_datei() {
        let (temp, _cache, queue, operation, execution, store) = snapshot_fixture();
        queue.enqueue(operation).unwrap();
        let path = queue.path.clone();
        drop(queue);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        json["version"] = serde_json::json!(31);
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let queue = OperationQueue::open(path).unwrap();
        assert_eq!(queue.snapshot().version, super::QUEUE_JOURNAL_VERSION);
        let entry = queue.claim_next().unwrap().unwrap();
        execute_local_queue_entry_detailed_with_snapshot_store(
            &queue,
            entry,
            &execution,
            Some(&store),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            std::fs::read(temp.path().join("dst/first")).unwrap(),
            vec![7; 50000]
        );
        assert!(!temp.path().join("dst/second").exists());
        assert_eq!(
            queue.snapshot().entries[0]
                .archive_extract_report
                .as_ref()
                .unwrap()
                .files,
            1
        );
    }
    #[test]
    fn snapshot_ohne_store_oder_bereinigte_aufnahme_bleibt_ohne_zielwirkung() {
        for missing_store in [false, true] {
            let (temp, cache, queue, operation, execution, store) = snapshot_fixture();
            queue.enqueue(operation).unwrap();
            if !missing_store {
                assert_eq!(
                    crate::remote_snapshot::cleanup_remote_snapshots(
                        cache.path(),
                        std::time::SystemTime::now()
                            + crate::remote_snapshot::SNAPSHOT_RETENTION
                            + std::time::Duration::from_secs(1),
                        crate::remote_snapshot::SNAPSHOT_RETENTION
                    )
                    .unwrap(),
                    1
                );
            }
            let entry = queue.claim_next().unwrap().unwrap();
            let result = if missing_store {
                execute_local_queue_entry_detailed(
                    &queue,
                    entry,
                    &execution,
                    &Cancel::new(),
                    &mut |_| {},
                )
            } else {
                execute_local_queue_entry_detailed_with_snapshot_store(
                    &queue,
                    entry,
                    &execution,
                    Some(&store),
                    &Cancel::new(),
                    &mut |_| {},
                )
            };
            assert!(result.is_err());
            assert!(std::fs::read_dir(temp.path().join("dst"))
                .unwrap()
                .next()
                .is_none());
            assert_eq!(queue.snapshot().entries[0].state, QueueState::Failed);
            assert!(queue.snapshot().entries[0]
                .error
                .as_ref()
                .unwrap()
                .contains("Archiv erneut öffnen"));
        }
    }
    #[test]
    fn snapshot_journalversion_binding_und_running_recovery_sind_fail_closed() {
        for attack in ["old_schema", "binding", "running"] {
            let (temp, _cache, queue, operation, _execution, _store) = snapshot_fixture();
            let id = queue.enqueue(operation).unwrap().operation_id;
            if attack == "running" {
                queue.claim_next().unwrap();
                queue.mark_running(&id).unwrap();
            }
            let path = queue.path.clone();
            drop(queue);
            if attack != "running" {
                let mut json: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                if attack == "old_schema" {
                    json["version"] = serde_json::json!(30);
                } else {
                    json["entries"][0]["operation"]["request"]["snapshot"]["origin"] =
                        serde_json::json!("forged origin");
                }
                std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
                assert!(OperationQueue::open(&path).is_err(), "{attack}");
            } else {
                let queue = OperationQueue::open(path).unwrap();
                assert_eq!(queue.snapshot().entries[0].state, QueueState::Interrupted);
                assert_eq!(
                    queue.snapshot().entries[0].error_code,
                    Some(crate::ErrorCode::OutcomeUnclear)
                );
                assert!(queue.retry(&id).is_err());
                assert!(queue.claim_next().unwrap().is_none());
            }
            assert!(std::fs::read_dir(temp.path().join("dst"))
                .unwrap()
                .next()
                .is_none());
        }
    }
    struct SnapshotScopeFixture {
        _temp: tempfile::TempDir,
        cache: tempfile::TempDir,
        snapshot_path: PathBuf,
        queue: OperationQueue,
        operation: QueueOperation,
        execution: LocalQueueExecution,
    }
    fn snapshot_scope_fixture(subtree: bool) -> SnapshotScopeFixture {
        let (temp, queue, mut operation, mut execution) = fixture();
        let root = temp.path().canonicalize().unwrap();
        execution.sandbox = Some(root.clone());
        let QueueOperation::ArchiveExtract { request, expected } = &mut operation else {
            unreachable!()
        };
        let original = request.source_path.clone();
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&original).unwrap());
        for (name, content) in [
            ("outer/sub/file", b"old snapshot bytes".as_slice()),
            ("outer/sibling", b"snapshot sibling".as_slice()),
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(content).unwrap();
        }
        zip.add_directory("outer/sub/empty/", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.finish().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let store =
            crate::remote_snapshot::SnapshotStore::open(&cache.path().canonicalize().unwrap())
                .unwrap();
        let downloaded = crate::remote_snapshot::download_remote_snapshot(
            &crate::LocalVfs::new(),
            &original,
            cache.path(),
            &Cancel::new(),
        )
        .unwrap();
        let validated = store.capture(&downloaded, &Cancel::new()).unwrap();
        let reader = crate::archive_read::ArchiveRead::open_snapshot(
            validated.clone(),
            crate::archive_read::CatalogBudget::default(),
            &Cancel::new(),
        )
        .unwrap();
        request.selection = if subtree {
            reader
                .extract_subtree_selection("outer/sub", &Cancel::new())
                .unwrap()
        } else {
            Default::default()
        };
        std::fs::remove_file(&original).unwrap();
        drop(reader);
        request.source_root = PathBuf::new();
        request.source_path = PathBuf::new();
        request.sandbox = Some(root);
        request.snapshot = Some(crate::archive_extract::ArchiveSnapshotSource {
            reference: validated.reference().clone(),
            origin: "WebDAV · /artificial/archive.zip".into(),
            profile: crate::RemoteProfileBinding::new(
                crate::RemoteProfileProtocol::Webdav,
                "artificial",
                1,
            )
            .unwrap(),
        });
        let prepared = request
            .prepare_with_snapshot(validated, &Cancel::new())
            .unwrap();
        request.options.overwrite = true;
        let prepared = prepared
            .with_options(request.options.clone(), &Cancel::new())
            .unwrap();
        **expected = prepared.binding().unwrap();
        assert!(!original.exists());
        assert_eq!(expected.preview.files, if subtree { 1 } else { 2 });
        drop(prepared);
        SnapshotScopeFixture {
            _temp: temp,
            cache,
            snapshot_path: PathBuf::from(downloaded.path),
            queue,
            operation,
            execution,
        }
    }
    #[test]
    fn snapshot_all_und_subtree_offline_optionswechsel_pending_reopen_exaktes_mapping() {
        for subtree in [false, true] {
            for availability in ["ready", "no_store", "cleaned"] {
                let fixture = snapshot_scope_fixture(subtree);
                let target = match &fixture.operation {
                    QueueOperation::ArchiveExtract { request, .. } => request.target_dir.clone(),
                    _ => unreachable!(),
                };
                fixture.queue.enqueue(fixture.operation).unwrap();
                let journal = fixture.queue.path.clone();
                drop(fixture.queue);
                if availability == "cleaned" {
                    assert_eq!(
                        crate::remote_snapshot::cleanup_remote_snapshots(
                            fixture.cache.path(),
                            std::time::SystemTime::now()
                                + crate::remote_snapshot::SNAPSHOT_RETENTION
                                + std::time::Duration::from_secs(1),
                            crate::remote_snapshot::SNAPSHOT_RETENTION
                        )
                        .unwrap(),
                        1
                    );
                }
                let queue = OperationQueue::open(journal).unwrap();
                let store = crate::remote_snapshot::SnapshotStore::open(
                    &fixture.cache.path().canonicalize().unwrap(),
                )
                .unwrap();
                let entry = queue.claim_next().unwrap().unwrap();
                let result = execute_local_queue_entry_detailed_with_snapshot_store(
                    &queue,
                    entry,
                    &fixture.execution,
                    if availability == "no_store" {
                        None
                    } else {
                        Some(&store)
                    },
                    &Cancel::new(),
                    &mut |_| {},
                );
                if availability != "ready" {
                    assert!(result.is_err(), "{availability}");
                    assert!(std::fs::read_dir(&target).unwrap().next().is_none());
                    continue;
                }
                result.unwrap();
                let relative = if subtree { "sub" } else { "outer/sub" };
                assert_eq!(
                    std::fs::read(target.join(relative).join("file")).unwrap(),
                    b"old snapshot bytes"
                );
                assert!(target.join(relative).join("empty").is_dir());
                assert_eq!(
                    std::fs::read_dir(target.join(relative).join("empty"))
                        .unwrap()
                        .count(),
                    0
                );
                if subtree {
                    assert!(!target.join("outer").exists());
                    assert!(!target.join("sibling").exists());
                    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 1);
                } else {
                    assert_eq!(
                        std::fs::read(target.join("outer/sibling")).unwrap(),
                        b"snapshot sibling"
                    );
                    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 1);
                }
                assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
            }
        }
    }

    #[test]
    fn alte_journale_sperren_snapshot_all_und_subtree_auch_in_attention_und_report() {
        for version in [30, 31] {
            for selection in [
                serde_json::json!({"kind":"all"}),
                serde_json::json!({"kind":"subtree","prefix":"outer/sub"}),
            ] {
                for location in ["request", "expected", "attention", "report"] {
                    let (temp, _cache, queue, operation, _, _) = snapshot_fixture();
                    queue.enqueue(operation).unwrap();
                    let path = queue.path.clone();
                    drop(queue);
                    let mut json: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    json["version"] = serde_json::json!(version);
                    let entry = &mut json["entries"][0];
                    let mut current = entry["operation"]["expected"].clone();
                    current["preview"]["selection"] = selection.clone();
                    match location {
                        "request" => entry["operation"]["request"]["selection"] = selection.clone(),
                        "expected" => entry["operation"]["expected"] = current,
                        "attention" => {
                            entry["state"] = serde_json::json!("AwaitingConfirmation");
                            entry["attention"] = serde_json::json!({"type":"ArchiveExtractChanged","reason":"changed","confirmation":"bound","current":current});
                        }
                        "report" => {
                            entry["state"] = serde_json::json!("Completed");
                            entry["archive_extract_report"] = serde_json::json!({"preview":current["preview"],"status":"complete","files":0,"directories":0,"bytes":0,"skipped":0,"failed":0,"unstarted":0,"has_effect":false,"recovery_paths":[],"target_copy_new_best_effort":null,"target_replace_best_effort":null});
                        }
                        _ => unreachable!(),
                    }
                    std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
                    let error = OperationQueue::open(&path)
                        .err()
                        .expect("Alter Snapshotumfang muss vor jeder Wirkung abgewiesen werden");
                    if version == 31 {
                        assert!(
                            error.to_string().contains("Snapshot-Entpackumfang"),
                            "{location}: {error}"
                        );
                    }
                    assert!(std::fs::read_dir(temp.path().join("dst"))
                        .unwrap()
                        .next()
                        .is_none());
                }
            }
        }
    }
    #[test]
    fn snapshot_mehrdatei_abbruch_und_quellendrift_nach_commit_bleiben_teilbilanz_ohne_retry() {
        for fault in ["cancel", "source_change"] {
            let fixture = snapshot_scope_fixture(false);
            let target = match &fixture.operation {
                QueueOperation::ArchiveExtract { request, .. } => request.target_dir.clone(),
                _ => unreachable!(),
            };
            let id = fixture
                .queue
                .enqueue(fixture.operation)
                .unwrap()
                .operation_id;
            let store = crate::remote_snapshot::SnapshotStore::open(
                &fixture.cache.path().canonicalize().unwrap(),
            )
            .unwrap();
            let cancel = Cancel::new();
            let entry = fixture.queue.claim_next().unwrap().unwrap();
            let mut injected = false;
            let result = execute_local_queue_entry_detailed_with_snapshot_store(
                &fixture.queue,
                entry,
                &fixture.execution,
                Some(&store),
                &cancel,
                &mut |progress| {
                    if !injected && progress.files_done == 1 {
                        injected = true;
                        if fault == "cancel" {
                            cancel.cancel();
                        } else {
                            std::fs::write(&fixture.snapshot_path, b"foreign archive bytes")
                                .unwrap();
                        }
                    }
                },
            );
            assert!(injected);
            if fault == "source_change" {
                assert!(result.is_err());
            } else {
                result.unwrap();
            }
            let snapshot = fixture.queue.snapshot();
            let report = snapshot.entries[0].archive_extract_report.as_ref().unwrap();
            assert!(report.has_effect);
            assert_eq!(report.files, 1);
            assert_eq!(report.unstarted + report.failed, 1);
            assert_eq!(
                std::fs::read(target.join("outer/sub/file")).unwrap(),
                b"old snapshot bytes"
            );
            assert!(!target.join("outer/sibling").exists());
            assert!(fixture.queue.retry(&id).is_err());
            let journal = fixture.queue.path.clone();
            drop(fixture.queue);
            let queue = OperationQueue::open(journal).unwrap();
            assert!(queue.retry(&id).is_err());
            assert!(queue.claim_next().unwrap().is_none());
        }
    }
}
