//! Ein Queueeintrag verbindet Archivphase und Upload ohne zweiten Erzeugungsversuch.
#[path = "support/webdav_namespace.rs"]
mod server;
use doppeldecker_core::archive_pack::{ArchivePackOptions, ArchivePackRequest};
use doppeldecker_core::archive_upload::ArchiveGeneration;
use doppeldecker_core::generated_source::GeneratedSourceStore;
use doppeldecker_core::{
    BidirectionalQueueEndpoint, Cancel, ErrorCode, KeychainSecretReference, LocalQueueExecution,
    OperationQueue, QueueAttention, QueueExecutionResult, QueueOperation, QueueState,
    SecretResolver, SecretString,
};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs, io,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
struct Secrets(Arc<AtomicUsize>);
impl SecretResolver for Secrets {
    fn resolve(&self, _: &KeychainSecretReference) -> io::Result<SecretString> {
        self.0.fetch_add(1, Ordering::SeqCst);
        SecretString::new("wertlos".into())
    }
}
fn fixture(
    server: &server::Server,
) -> (tempfile::TempDir, OperationQueue, QueueOperation, PathBuf) {
    let area = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let root = area.path().canonicalize().unwrap();
    fs::create_dir(root.join("source")).unwrap();
    fs::write(root.join("source/file"), b"original").unwrap();
    let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
    let workspace = GeneratedSourceStore::for_queue(&queue)
        .unwrap()
        .begin()
        .unwrap();
    let path = workspace.path().to_path_buf();
    let request = ArchivePackRequest {
        source_root: root.join("source"),
        items: vec![root.join("source/file")],
        target_dir: path.clone(),
        target_name: "result.zip".into(),
        options: ArchivePackOptions::default(),
        sandbox: None,
    };
    let expected = request.prepare(&Cancel::new()).unwrap().binding().unwrap();
    let operation = QueueOperation::ArchiveUpload {
        generation: Box::new(ArchiveGeneration::Pack {
            request: Box::new(request),
            expected: Box::new(expected),
        }),
        workspace: workspace.reference(),
        target: Box::new(BidirectionalQueueEndpoint::WebDav {
            connection: Box::new(server.config.clone()),
            root: "/pane".into(),
        }),
    };
    (area, queue, operation, path)
}
fn execute(
    queue: &OperationQueue,
    calls: &Arc<AtomicUsize>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&doppeldecker_core::Progress),
) -> io::Result<QueueExecutionResult> {
    doppeldecker_core::execute_provider_file_queue_entry(
        queue,
        queue.claim_next().unwrap().unwrap(),
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(Secrets(calls.clone())),
        cancel,
        on_progress,
    )
}
fn approve(queue: &OperationQueue) {
    let entry = queue.snapshot().entries[0].clone();
    queue
        .approve_attention_effects(&entry.operation_id, entry.attention.as_ref().unwrap(), true)
        .unwrap();
}
#[test]
fn archive_upload_transitions_one_entry_before_target_contact_and_keeps_bytes_on_restart() {
    let server = server::Server::new();
    let (area, queue, operation, path) = fixture(&server);
    let entry = queue.enqueue(operation).unwrap();
    fs::write(server.root.path().join("pane/result.zip"), b"occupied").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(matches!(
        execute(&queue, &calls, &Cancel::new(), &mut |_| {}).unwrap(),
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(snapshot.entries[0].operation_id, entry.operation_id);
    assert!(snapshot.entries[0].generated_source.is_some());
    assert!(matches!(
        snapshot.entries[0].operation,
        QueueOperation::ProviderFile { .. }
    ));
    assert_eq!(
        server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _)| method == "PUT")
            .count(),
        0
    );
    let bytes = fs::read(path.join("result.zip")).unwrap();
    fs::write(area.path().join("source/file"), b"changed original").unwrap();
    drop(snapshot);
    drop(queue);
    let queue = OperationQueue::open(area.path().join("config/queue.json")).unwrap();
    fs::remove_file(server.root.path().join("pane/result.zip")).unwrap();
    let prepared = queue
        .prepare_generated_upload(&entry.operation_id, &Cancel::new())
        .unwrap();
    let mut operation = prepared.entry.operation.clone();
    if let QueueOperation::ProviderFile {
        expected_conflicts,
        copy_new_best_effort_expected,
        ..
    } = &mut operation
    {
        expected_conflicts.clear();
        *copy_new_best_effort_expected = Some(true);
    }
    queue
        .resume_generated_upload(&prepared.entry, operation, vec![], &Cancel::new())
        .unwrap();
    drop(prepared);
    assert!(matches!(
        execute(&queue, &calls, &Cancel::new(), &mut |_| {}).unwrap(),
        QueueExecutionResult::File(_)
    ));
    assert_eq!(
        fs::read(server.root.path().join("pane/result.zip")).unwrap(),
        bytes
    );
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    assert!(!path.parent().unwrap().exists());
}
#[test]
fn archive_upload_pending_removal_releases_only_its_own_empty_workspace() {
    let server = server::Server::new();
    let (area, queue, operation, path) = fixture(&server);
    let entry = queue.enqueue(operation).unwrap();
    let other = GeneratedSourceStore::for_queue(&queue)
        .unwrap()
        .begin()
        .unwrap();
    queue.remove_pending(&entry.operation_id).unwrap();
    assert!(!path.parent().unwrap().exists());
    assert!(other.path().is_dir());
    assert!(queue.snapshot().entries.is_empty());
    assert_eq!(
        fs::read(area.path().join("source/file")).unwrap(),
        b"original"
    );
    assert!(server.state.lock().unwrap().requests.is_empty());
}

#[test]
fn archive_upload_success_with_an_active_reader_retains_data_until_reopen() {
    let server = server::Server::new();
    let (area, queue, operation, path) = fixture(&server);
    let entry = queue.enqueue(operation).unwrap();
    fs::write(server.root.path().join("pane/result.zip"), b"occupied").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    execute(&queue, &calls, &Cancel::new(), &mut |_| {}).unwrap();
    let prepared = queue
        .prepare_generated_upload(&entry.operation_id, &Cancel::new())
        .unwrap();
    fs::remove_file(server.root.path().join("pane/result.zip")).unwrap();
    let mut operation = prepared.entry.operation.clone();
    if let QueueOperation::ProviderFile {
        expected_conflicts,
        copy_new_best_effort_expected,
        ..
    } = &mut operation
    {
        expected_conflicts.clear();
        *copy_new_best_effort_expected = Some(true);
    }
    queue
        .resume_generated_upload(&prepared.entry, operation, vec![], &Cancel::new())
        .unwrap();
    execute(&queue, &calls, &Cancel::new(), &mut |_| {}).unwrap();
    let saved = queue.snapshot().entries[0].clone();
    assert_eq!(saved.state, QueueState::Completed);
    assert!(path.join("result.zip").is_file());
    assert!(saved
        .temp_recovery_paths
        .contains(&path.to_string_lossy().into_owned()));
    assert!(queue.retry(&entry.operation_id).is_err());
    let uploaded = fs::read(server.root.path().join("pane/result.zip")).unwrap();
    drop(prepared);
    drop(queue);
    let queue = OperationQueue::open(area.path().join("config/queue.json")).unwrap();
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    assert!(queue.snapshot().entries[0].temp_recovery_paths.is_empty());
    assert!(!path.parent().unwrap().exists());
    assert_eq!(
        fs::read(server.root.path().join("pane/result.zip")).unwrap(),
        uploaded
    );
}

#[test]
fn archive_upload_pending_removal_releases_the_sealed_source_after_conflict_resume() {
    let server = server::Server::new();
    let (_area, queue, operation, path) = fixture(&server);
    let entry = queue.enqueue(operation).unwrap();
    fs::write(server.root.path().join("pane/result.zip"), b"occupied").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    execute(&queue, &calls, &Cancel::new(), &mut |_| {}).unwrap();
    let prepared = queue
        .prepare_generated_upload(&entry.operation_id, &Cancel::new())
        .unwrap();
    queue
        .resume_generated_upload(
            &prepared.entry,
            prepared.entry.operation.clone(),
            vec![],
            &Cancel::new(),
        )
        .unwrap();
    drop(prepared);
    queue.remove_pending(&entry.operation_id).unwrap();
    assert!(queue.snapshot().entries.is_empty());
    assert!(!path.parent().unwrap().exists());
    assert_eq!(
        fs::read(server.root.path().join("pane/result.zip")).unwrap(),
        b"occupied"
    );
}

#[test]
fn archive_upload_source_drift_waits_before_generation_or_credentials() {
    let server = server::Server::new();
    let (area, queue, operation, path) = fixture(&server);
    queue.enqueue(operation).unwrap();
    fs::write(area.path().join("source/file"), b"changed").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(matches!(
        execute(&queue, &calls, &Cancel::new(), &mut |_| {}).unwrap(),
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _)| method == "PUT")
            .count(),
        0
    );
    assert!(!path.join("result.zip").exists());
    assert!(matches!(
        queue.snapshot().entries[0].attention,
        Some(QueueAttention::ArchivePackChanged { .. })
    ));
    approve(&queue);
    let _ = execute(&queue, &calls, &Cancel::new(), &mut |_| {}).unwrap();
    assert!(server.root.path().join("pane/result.zip").is_file());
    assert!(!path.parent().unwrap().exists());
}
#[test]
fn archive_upload_running_crash_has_no_retry_or_remote_access() {
    let server = server::Server::new();
    let (area, queue, operation, path) = fixture(&server);
    let entry = queue.enqueue(operation).unwrap();
    queue.claim_next().unwrap().unwrap();
    queue.mark_running(&entry.operation_id).unwrap();
    fs::write(path.join("result.zip"), b"partial").unwrap();
    drop(queue);
    let queue = OperationQueue::open(area.path().join("config/queue.json")).unwrap();
    let saved = queue.snapshot().entries[0].clone();
    assert_eq!(saved.state, QueueState::Interrupted);
    assert_eq!(saved.error_code, Some(ErrorCode::OutcomeUnclear));
    assert!(saved
        .temp_recovery_paths
        .iter()
        .any(|p| std::path::Path::new(p) == path));
    assert!(queue.retry(&entry.operation_id).is_err());
    assert_eq!(
        server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _)| method == "PUT")
            .count(),
        0
    );
    assert_eq!(fs::read(path.join("result.zip")).unwrap(), b"partial");
}

#[test]
#[ignore = "wird nur vom begrenzten Phasenabbruchtest gestartet"]
fn archive_upload_phase_crash_child() {
    let root = PathBuf::from(
        std::env::var_os("DOPPELDECKER_ARCHIVE_CRASH_ROOT").expect("künstlicher Testroot"),
    );
    assert_eq!(
        fs::read(root.join("marker")).unwrap(),
        b"archive-phase-crash-v1"
    );
    let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let result = execute(&queue, &calls, &Cancel::new(), &mut |progress| {
        if progress.phase == "archive_upload_ready" {
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            std::process::exit(73);
        }
    });
    panic!("Abbruchpunkt nicht erreicht: {result:?}");
}
#[test]
fn archive_upload_crash_after_transition_never_rebuilds_or_contacts_target() {
    let server = server::Server::new();
    let (area, queue, operation, path) = fixture(&server);
    fs::write(area.path().join("marker"), b"archive-phase-crash-v1").unwrap();
    let entry = queue.enqueue(operation).unwrap();
    drop(queue);
    let name = "archive_upload_phase_crash_child";
    let executable = std::env::current_exe().unwrap();
    let listed = std::process::Command::new(&executable)
        .args(["--exact", name, "--list"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert_eq!(
        String::from_utf8(listed.stdout)
            .unwrap()
            .lines()
            .filter(|line| *line == format!("{name}: test"))
            .count(),
        1
    );
    let mut child = std::process::Command::new(executable)
        .args(["--exact", name, "--ignored", "--nocapture"])
        .env("DOPPELDECKER_ARCHIVE_CRASH_ROOT", area.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Kindprozess überschritt Frist");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(73));
    assert!(server.state.lock().unwrap().requests.is_empty());
    let bytes = fs::read(path.join("result.zip")).unwrap();
    fs::write(
        area.path().join("source/file"),
        b"changed originals after crash",
    )
    .unwrap();
    let queue = OperationQueue::open(area.path().join("config/queue.json")).unwrap();
    let recovered = queue.snapshot().entries[0].clone();
    assert_eq!(recovered.state, QueueState::Interrupted);
    assert_eq!(recovered.error_code, Some(ErrorCode::Interrupted));
    assert!(recovered.generated_source.is_some());
    assert!(matches!(
        recovered.operation,
        QueueOperation::ProviderFile { .. }
    ));
    queue.retry(&entry.operation_id).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    for _ in 0..3 {
        match execute(&queue, &calls, &Cancel::new(), &mut |_| {}).unwrap() {
            QueueExecutionResult::AwaitingConfirmation { .. } => approve(&queue),
            QueueExecutionResult::File(stats) => {
                assert_eq!(stats.files, 1);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    assert_eq!(
        fs::read(server.root.path().join("pane/result.zip")).unwrap(),
        bytes
    );
}
#[test]
fn archive_upload_cancellation_preserves_the_local_phase_without_remote_effect() {
    for during_generation in [false, true] {
        let server = server::Server::new();
        let (_area, queue, operation, path) = fixture(&server);
        let entry = queue.enqueue(operation).unwrap();
        let cancel = Cancel::new();
        if !during_generation {
            cancel.cancel();
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let result = execute(&queue, &calls, &cancel, &mut |progress| {
            if during_generation && progress.phase.starts_with("archive_pack_") {
                cancel.cancel();
            }
        })
        .unwrap();
        assert!(matches!(result,QueueExecutionResult::File(stats) if stats.cancelled));
        let saved = queue.snapshot().entries[0].clone();
        assert_eq!(saved.state, QueueState::Cancelled);
        assert!(saved.generated_source.is_none());
        assert!(queue.retry(&entry.operation_id).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(server.state.lock().unwrap().requests.is_empty());
        assert!(path.is_dir());
    }
}

#[test]
fn archive_upload_extracts_local_and_snapshot_scopes_through_the_same_parent() {
    use doppeldecker_core::archive_extract::{
        ArchiveExtractRequest, ArchiveExtractSelection, ArchiveSnapshotSource,
    };
    use doppeldecker_core::archive_read::{ArchiveRead, CatalogBudget};
    use doppeldecker_core::remote_snapshot::{download_remote_snapshot, SnapshotStore};
    use std::io::Write;
    for snapshot in [false, true] {
        for scope in ["all", "entry", "subtree"] {
            let server = server::Server::new();
            let (area, queue, operation, path) = fixture(&server);
            let QueueOperation::ArchiveUpload {
                workspace, target, ..
            } = operation
            else {
                unreachable!()
            };
            let source_root = area.path().join("source").canonicalize().unwrap();
            let source_path = source_root.join("archive.zip");
            let mut zip = zip::ZipWriter::new(fs::File::create(&source_path).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            zip.add_directory("tree/", options).unwrap();
            zip.add_directory("tree/empty/", options).unwrap();
            zip.start_file("tree/leaf.txt", options).unwrap();
            zip.write_all(b"leaf").unwrap();
            zip.start_file("other.txt", options).unwrap();
            zip.write_all(b"other").unwrap();
            zip.finish().unwrap();
            let reader = ArchiveRead::open(
                &source_root,
                std::path::Path::new("archive.zip"),
                CatalogBudget::default(),
                &Cancel::new(),
            )
            .unwrap();
            let selection = match scope {
                "all" => ArchiveExtractSelection::All,
                "entry" => reader
                    .extract_entry_selection(
                        reader
                            .entries()
                            .iter()
                            .find(|entry| entry.raw_name == b"other.txt")
                            .unwrap()
                            .id,
                        &Cancel::new(),
                    )
                    .unwrap(),
                _ => ArchiveExtractSelection::Subtree {
                    prefix: "tree".into(),
                },
            };
            drop(reader);
            let mut request = ArchiveExtractRequest {
                source_root,
                source_path: source_path.clone(),
                snapshot: None,
                target_dir: path,
                options: Default::default(),
                selection,
                sandbox: None,
            };
            let store = if snapshot {
                let cache = area.path().join("snapshots");
                fs::create_dir(&cache).unwrap();
                let store = SnapshotStore::open(&cache).unwrap();
                let downloaded = download_remote_snapshot(
                    &doppeldecker_core::LocalVfs::new(),
                    &source_path,
                    &cache,
                    &Cancel::new(),
                )
                .unwrap();
                let validated = store.capture(&downloaded, &Cancel::new()).unwrap();
                request.snapshot = Some(ArchiveSnapshotSource {
                    reference: validated.reference().clone(),
                    origin: "SFTP · /artificial/archive.zip".into(),
                    profile: doppeldecker_core::RemoteProfileBinding::new(
                        doppeldecker_core::RemoteProfileProtocol::Sftp,
                        "source-profile",
                        1,
                    )
                    .unwrap(),
                });
                request.source_root = PathBuf::new();
                request.source_path = PathBuf::new();
                Some(store)
            } else {
                None
            };
            let prepared = if let Some(store) = &store {
                request
                    .prepare_with_snapshot(
                        store
                            .resolve(
                                &request.snapshot.as_ref().unwrap().reference,
                                &Cancel::new(),
                            )
                            .unwrap(),
                        &Cancel::new(),
                    )
                    .unwrap()
            } else {
                request.prepare(&Cancel::new()).unwrap()
            };
            let expected = prepared.binding().unwrap();
            drop(prepared);
            if snapshot {
                fs::remove_file(&source_path).unwrap();
            }
            let entry = queue
                .enqueue(QueueOperation::ArchiveUpload {
                    workspace,
                    target,
                    generation: Box::new(ArchiveGeneration::Extract {
                        request: Box::new(request),
                        expected: Box::new(expected),
                    }),
                })
                .unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            if scope == "subtree" {
                let pane = server.root.path().join("pane");
                fs::create_dir(pane.join("tree")).unwrap();
                fs::write(pane.join("tree/leaf.txt"), b"old target").unwrap();
                server.state.lock().unwrap().allow_file_overwrite = true;
                let waiting=doppeldecker_core::queue::execute_provider_file_queue_entry_detailed_with_snapshot_store(&queue,queue.claim_next().unwrap().unwrap(),&LocalQueueExecution{sandbox:None,sync_quarantine:None,max_delete_entries:100,max_delete_fraction:0.25},Arc::new(Secrets(calls.clone())),store.as_ref(),&Cancel::new(),&mut |_|{}).unwrap();
                assert!(matches!(
                    waiting,
                    QueueExecutionResult::AwaitingConfirmation { .. }
                ));
                let prepared = queue
                    .prepare_generated_upload(&entry.operation_id, &Cancel::new())
                    .unwrap();
                let Some(QueueAttention::ConflictsChanged {
                    current_conflicts,
                    current_conflict_proofs,
                    ..
                }) = &prepared.entry.attention
                else {
                    panic!("Dateikonflikt fehlt")
                };
                let mut operation = prepared.entry.operation.clone();
                let QueueOperation::ProviderFile {
                    overwrite,
                    expected_conflicts,
                    expected_conflict_proofs,
                    replace_best_effort_expected,
                    ..
                } = &mut operation
                else {
                    unreachable!()
                };
                *overwrite = true;
                *expected_conflicts = current_conflicts.clone();
                *expected_conflict_proofs = current_conflict_proofs.clone();
                *replace_best_effort_expected = Some(true);
                queue
                    .resume_generated_upload(&prepared.entry, operation, vec![], &Cancel::new())
                    .unwrap();
                drop(prepared);
                if !snapshot {
                    fs::write(&source_path, b"changed original after extraction").unwrap();
                }
            }
            let mut result=doppeldecker_core::queue::execute_provider_file_queue_entry_detailed_with_snapshot_store(&queue,queue.claim_next().unwrap().unwrap(),&LocalQueueExecution{sandbox:None,sync_quarantine:None,max_delete_entries:100,max_delete_fraction:0.25},Arc::new(Secrets(calls.clone())),store.as_ref(),&Cancel::new(),&mut |_|{}).unwrap();
            if scope == "subtree" {
                // Die erste (überspringende) Prüfung band nur den vorhandenen
                // Ordner. Ersetzen benötigt zusätzlich die konkrete Kinddatei.
                let saved = queue.snapshot().entries[0].clone();
                assert!(matches!(
                    result,
                    QueueExecutionResult::AwaitingConfirmation { .. }
                ));
                assert!(
                    matches!(&saved.attention, Some(QueueAttention::ConflictsChanged { current_conflicts, .. }) if current_conflicts == &["/pane/tree/leaf.txt"])
                );
                queue
                    .approve_attention_effects(
                        &entry.operation_id,
                        saved.attention.as_ref().unwrap(),
                        true,
                    )
                    .unwrap();
                result = execute(&queue, &calls, &Cancel::new(), &mut |_| {}).unwrap();
            }
            assert!(
                matches!(result,QueueExecutionResult::File(ref stats) if stats.files==if scope=="all"{2}else{1}),
                "{snapshot} {scope}: {result:?}"
            );
            let pane = server.root.path().join("pane");
            if scope != "entry" {
                assert_eq!(fs::read(pane.join("tree/leaf.txt")).unwrap(), b"leaf");
                assert!(pane.join("tree/empty").is_dir());
            } else {
                assert!(!pane.join("tree").exists());
            }
            if scope != "subtree" {
                assert_eq!(fs::read(pane.join("other.txt")).unwrap(), b"other");
            } else {
                assert!(!pane.join("other.txt").exists());
            }
            let saved = queue
                .snapshot()
                .entries
                .into_iter()
                .find(|candidate| candidate.operation_id == entry.operation_id)
                .unwrap();
            assert_eq!(saved.state, QueueState::Completed);
            assert!(saved.generated_source.is_some());
            assert_eq!(saved.archive_extract_report.as_ref().unwrap().failed, 0);
        }
    }
}

#[test]
fn archive_upload_replan_keeps_target_selection_and_result_in_one_entry() {
    let server = server::Server::new();
    let (_area, queue, operation, path) = fixture(&server);
    let entry = queue.enqueue(operation).unwrap();
    fs::write(server.root.path().join("pane/result.zip"), b"occupied").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(matches!(
        execute(&queue, &calls, &Cancel::new(), &mut |_| {}).unwrap(),
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    let prepared = queue
        .prepare_generated_upload(&entry.operation_id, &Cancel::new())
        .unwrap();
    server.state.lock().unwrap().requests.clear();
    for field in ["target", "source", "move", "selection", "destination"] {
        let mut changed = prepared.entry.operation.clone();
        let QueueOperation::ProviderFile {
            kind,
            source,
            target,
            items,
            destination,
            ..
        } = &mut changed
        else {
            unreachable!()
        };
        match field {
            "target" => {
                if let BidirectionalQueueEndpoint::WebDav { root, .. } = target.as_mut() {
                    *root = "/other".into();
                    *destination = "/other".into();
                }
            }
            "source" => {
                **source = BidirectionalQueueEndpoint::Local {
                    root: path.parent().unwrap().to_string_lossy().into_owned(),
                }
            }
            "move" => *kind = doppeldecker_core::FileQueueKind::Move,
            "selection" => items.clear(),
            _ => *destination = "/other".into(),
        }
        assert!(
            queue
                .resume_generated_upload(&prepared.entry, changed, vec![], &Cancel::new())
                .is_err(),
            "{field}"
        );
        assert_eq!(queue.snapshot().entries.len(), 1);
        assert_eq!(
            queue.snapshot().entries[0].state,
            QueueState::AwaitingConfirmation
        );
    }
    fs::write(path.join("result.zip"), b"changed generated bytes").unwrap();
    assert!(queue
        .resume_generated_upload(
            &prepared.entry,
            prepared.entry.operation.clone(),
            vec![],
            &Cancel::new()
        )
        .is_err());
    assert!(server.state.lock().unwrap().requests.is_empty());
    assert_eq!(
        queue.snapshot().entries[0].generated_source,
        prepared.entry.generated_source
    );
}
#[test]
fn archive_upload_old_schema_and_wrong_workspace_are_rejected_before_execution() {
    let server = server::Server::new();
    let (area, queue, operation, _path) = fixture(&server);
    let mut forged = serde_json::to_value(&operation).unwrap();
    forged["workspace"]["id"] = serde_json::json!("generated-ffffffffffffffffffffffffffffffff");
    let forged: QueueOperation = serde_json::from_value(forged).unwrap();
    assert!(queue.enqueue(forged).is_err());
    assert!(queue.snapshot().entries.is_empty());
    queue.enqueue(operation).unwrap();
    drop(queue);
    let journal = area.path().join("config/queue.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    value["version"] = serde_json::json!(39);
    fs::write(&journal, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(OperationQueue::open(journal).is_err());
    assert!(server.state.lock().unwrap().requests.is_empty());
    assert_eq!(
        fs::read(area.path().join("source/file")).unwrap(),
        b"original"
    );
}
#[test]
fn archive_upload_lost_put_response_keeps_result_and_never_repeats_mutation() {
    let server = server::Server::new();
    server.state.lock().unwrap().fault = server::Fault::LostAfterPut;
    let (_area, queue, operation, path) = fixture(&server);
    let entry = queue.enqueue(operation).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let error = execute(&queue, &calls, &Cancel::new(), &mut |_| {}).unwrap_err();
    assert_eq!(
        doppeldecker_core::error_code(&error),
        ErrorCode::OutcomeUnclear
    );
    let saved = queue.snapshot().entries[0].clone();
    assert!(saved.generated_source.is_some());
    assert!(matches!(
        saved.operation,
        QueueOperation::ProviderFile { .. }
    ));
    assert!(queue.retry(&entry.operation_id).is_err());
    assert!(path.join("result.zip").is_file());
    let state = server.state.lock().unwrap();
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(method, _)| method == "PUT")
            .count(),
        1
    );
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(method, _)| method == "MOVE")
            .count(),
        0
    );
}

#[test]
#[ignore = "begrenzter Server für echte native GUI-Prüfung im markierten Wegwerfbereich"]
fn archive_upload_live_native_fixture() {
    use std::io::Write;
    let root = PathBuf::from(
        std::env::var_os("DOPPELDECKER_NATIVE_ARCHIVE_ROOT")
            .expect("markierter künstlicher Testroot"),
    )
    .canonicalize()
    .unwrap();
    assert!(root.starts_with(std::path::Path::new("/tmp").canonicalize().unwrap()));
    assert_eq!(fs::metadata(&root).unwrap().permissions().mode() & 0o077, 0);
    assert_eq!(
        fs::read(root.join(".doppeldecker-test-sandbox")).unwrap(),
        b"DOPPELDECKER TEST SANDBOX\n"
    );
    let source = root.join("quelle");
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    let config = root.join("app-config");
    fs::create_dir_all(&config).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(fs::read_dir(&config).unwrap().count(), 0);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(source.join("Datei.txt"))
        .unwrap()
        .write_all(b"Native Archivquelle\n")
        .unwrap();
    let mut zip = zip::ZipWriter::new(
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(source.join("Archiv.zip"))
            .unwrap(),
    );
    let options = zip::write::SimpleFileOptions::default();
    zip.add_directory("Baum/", options).unwrap();
    zip.add_directory("Baum/Leer/", options).unwrap();
    zip.start_file("Baum/Inhalt.txt", options).unwrap();
    zip.write_all(b"Nativ entpackte Bytes\n").unwrap();
    zip.finish().unwrap();
    let server = server::Server::new();
    server.state.lock().unwrap().allow_file_overwrite = true;
    let mut connection = server.config.clone();
    connection.requested_root = "/pane".into();
    doppeldecker_core::save_webdav_profile(
        &config.join("webdav-profiles.json"),
        doppeldecker_core::WebDavProfile {
            id: "native-webdav".into(),
            name: "Archivziel (Test)".into(),
            connection,
        },
    )
    .unwrap();
    let value = serde_json::json!({"serverRoot":server.root.path(),"origin":server.config.origin,"secretService":server.config.password_reference.service,"secretAccount":server.config.password_reference.account});
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join("native-server.json"))
        .unwrap()
        .write_all(&serde_json::to_vec_pretty(&value).unwrap())
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        match fs::symlink_metadata(root.join("native-done")) {
            Ok(metadata) => {
                assert!(metadata.is_file());
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("{error}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Native GUI-Prüfung überschritt ihre Frist"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let requests = server.state.lock().unwrap().requests.clone();
    let result = serde_json::json!({"requests":requests,"zipPresent":server.root.path().join("pane/Datei.txt.zip").is_file(),"extractedContent":fs::read(server.root.path().join("pane/Baum/Inhalt.txt")).ok().map(|bytes|blake3::hash(&bytes).to_hex().to_string()),"emptyDirectory":server.root.path().join("pane/Baum/Leer").is_dir()});
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join("native-server-result.json"))
        .unwrap()
        .write_all(&serde_json::to_vec_pretty(&result).unwrap())
        .unwrap();
}
