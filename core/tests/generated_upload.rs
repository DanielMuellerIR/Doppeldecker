//! Erzeugte ZIP-Quellen bleiben über Queue-Neustart und Konfliktpause unverändert.
#[path = "support/webdav_namespace.rs"]
mod server;
use doppeldecker_core::archive_pack::{prepare_archive_pack, ArchivePackOptions};
use doppeldecker_core::generated_source::{GeneratedSourceReference, GeneratedSourceStore};
use doppeldecker_core::{
    BidirectionalQueueEndpoint, Cancel, ErrorCode, KeychainSecretReference, LocalQueueExecution,
    OperationQueue, QueueExecutionResult, QueueState, SecretResolver, SecretString,
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

struct CountingSecrets(Arc<AtomicUsize>);
impl SecretResolver for CountingSecrets {
    fn resolve(&self, _: &KeychainSecretReference) -> io::Result<SecretString> {
        self.0.fetch_add(1, Ordering::SeqCst);
        SecretString::new("wertlos".into())
    }
}
fn fixture() -> (
    tempfile::TempDir,
    OperationQueue,
    GeneratedSourceReference,
    PathBuf,
    Vec<u8>,
) {
    let area = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let root = area.path().canonicalize().unwrap();
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("original"), b"Urspruenglicher Inhalt").unwrap();
    let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
    let store = GeneratedSourceStore::for_queue(&queue).unwrap();
    let workspace = store.begin().unwrap();
    let generated = prepare_archive_pack(
        &source,
        &[source.join("original")],
        workspace.path(),
        "result.zip",
        ArchivePackOptions::default(),
        None,
        &Cancel::new(),
    )
    .unwrap()
    .execute_generated(None, &mut |_| {})
    .unwrap();
    let path = workspace.path().join("result.zip");
    let bytes = fs::read(&path).unwrap();
    let reference = workspace.seal_archive(generated, &Cancel::new()).unwrap();
    (area, queue, reference, path, bytes)
}
fn target(server: &server::Server) -> BidirectionalQueueEndpoint {
    BidirectionalQueueEndpoint::WebDav {
        connection: Box::new(server.config.clone()),
        root: "/pane".into(),
    }
}
fn execute(queue: &OperationQueue, calls: &Arc<AtomicUsize>) -> io::Result<QueueExecutionResult> {
    doppeldecker_core::execute_provider_file_queue_entry(
        queue,
        queue.claim_next().unwrap().unwrap(),
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(CountingSecrets(calls.clone())),
        &Cancel::new(),
        &mut |_| {},
    )
}
#[test]
fn generated_zip_upload_uses_exact_bytes_after_queue_restart() {
    let server = server::Server::new();
    let (area, queue, reference, _path, bytes) = fixture();
    let entry = queue
        .enqueue_generated_upload(reference.clone(), target(&server), vec![], &Cancel::new())
        .unwrap();
    assert_eq!(entry.generated_source.as_ref(), Some(&reference));
    fs::write(
        area.path().join("source/original"),
        b"Spaetere Originalfassung",
    )
    .unwrap();
    drop(queue);
    let queue = OperationQueue::open(area.path().join("config/queue.json")).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(matches!(
        execute(&queue, &calls).unwrap(),
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    let attention = queue.snapshot().entries[0].attention.clone().unwrap();
    assert!(matches!(
        attention,
        doppeldecker_core::QueueAttention::BestEffortCommitDowngrade { .. }
    ));
    queue
        .approve_attention_effects(&entry.operation_id, &attention, true)
        .unwrap();
    let result = execute(&queue, &calls).unwrap();
    let QueueExecutionResult::File(stats) = result else {
        panic!("{result:?}")
    };
    assert_eq!(stats.files, 1);
    assert_eq!(stats.bytes, bytes.len() as u64);
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    assert_eq!(
        fs::read(server.root.path().join("pane/result.zip")).unwrap(),
        bytes
    );
    assert!(calls.load(Ordering::SeqCst) > 0);
}
#[test]
fn changed_generated_zip_is_rejected_before_credentials_or_provider_requests() {
    for change in ["bytes", "missing"] {
        let server = server::Server::new();
        let (area, queue, reference, path, mut bytes) = fixture();
        let entry = queue
            .enqueue_generated_upload(reference, target(&server), vec![], &Cancel::new())
            .unwrap();
        if change == "bytes" {
            let mtime = fs::metadata(&path).unwrap().modified().unwrap();
            bytes[0] ^= 1;
            fs::write(&path, bytes).unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(mtime))
                .unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
        drop(queue);
        let queue = OperationQueue::open(area.path().join("config/queue.json")).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let error = execute(&queue, &calls).unwrap_err();
        assert!(
            matches!(
                doppeldecker_core::error_code(&error),
                ErrorCode::PlanDrift | ErrorCode::NotFound
            ),
            "{error}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(server.state.lock().unwrap().requests.is_empty());
        assert!(!server.root.path().join("pane/result.zip").exists());
        queue.retry(&entry.operation_id).unwrap();
        assert!(execute(&queue, &calls).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(server.state.lock().unwrap().requests.is_empty());
    }
}
#[test]
fn generated_source_marker_cannot_be_ignored_by_old_or_edited_journals() {
    let server = server::Server::new();
    let (area, queue, reference, _path, _) = fixture();
    queue
        .enqueue_generated_upload(reference, target(&server), vec![], &Cancel::new())
        .unwrap();
    let journal = area.path().join("config/queue.json");
    drop(queue);
    let original: serde_json::Value = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    for mutation in ["old_version", "missing_marker", "changed_root", "move"] {
        let mut data = original.clone();
        match mutation {
            "old_version" => data["version"] = serde_json::json!(38),
            "missing_marker" => {
                data["entries"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("generated_source");
            }
            "changed_root" => {
                data["entries"][0]["operation"]["source"]["root"] =
                    serde_json::json!(area.path().join("source").to_str().unwrap())
            }
            "move" => data["entries"][0]["operation"]["kind"] = serde_json::json!("Move"),
            _ => unreachable!(),
        }
        let path = area
            .path()
            .join("config")
            .join(format!("invalid-{mutation}.json"));
        fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        let error = match OperationQueue::open(path) {
            Ok(_) => panic!("{mutation} akzeptiert"),
            Err(error) => error,
        };
        assert_eq!(
            error.kind(),
            io::ErrorKind::InvalidData,
            "{mutation}: {error}"
        );
    }
    assert!(server.state.lock().unwrap().requests.is_empty());
}

#[test]
fn class_confirmation_after_restart_does_not_release_changed_generated_bytes() {
    let server = server::Server::new();
    let (area, queue, reference, path, mut bytes) = fixture();
    let entry = queue
        .enqueue_generated_upload(reference.clone(), target(&server), vec![], &Cancel::new())
        .unwrap();
    drop(queue);
    let queue = OperationQueue::open(area.path().join("config/queue.json")).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(matches!(
        execute(&queue, &calls).unwrap(),
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    let attention = queue.snapshot().entries[0].attention.clone().unwrap();
    assert!(matches!(
        attention,
        doppeldecker_core::QueueAttention::BestEffortCommitDowngrade { .. }
    ));
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    bytes[0] ^= 1;
    fs::write(&path, bytes).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    queue
        .approve_attention_effects(&entry.operation_id, &attention, true)
        .unwrap();
    assert_eq!(
        queue.snapshot().entries[0].generated_source.as_ref(),
        Some(&reference)
    );
    server.state.lock().unwrap().requests.clear();
    calls.store(0, Ordering::SeqCst);
    assert!(execute(&queue, &calls).is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(server.state.lock().unwrap().requests.is_empty());
    assert!(!server.root.path().join("pane/result.zip").exists());
}

#[test]
fn conflict_pause_reuses_the_same_generated_archive_after_the_target_is_freed() {
    let server = server::Server::new();
    let (area, queue, reference, path, bytes) = fixture();
    let target_path = server.root.path().join("pane/result.zip");
    fs::write(&target_path, b"Vorhandenes Archiv").unwrap();
    let entry = queue
        .enqueue_generated_upload(reference.clone(), target(&server), vec![], &Cancel::new())
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(matches!(
        execute(&queue, &calls).unwrap(),
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    let attention = queue.snapshot().entries[0].attention.clone().unwrap();
    assert!(matches!(
        attention,
        doppeldecker_core::QueueAttention::ConflictsChanged { .. }
    ));
    assert_eq!(fs::read(&target_path).unwrap(), b"Vorhandenes Archiv");
    fs::write(
        area.path().join("source/original"),
        b"Original nach Konfliktpause geaendert",
    )
    .unwrap();
    let saved = server.root.path().join("saved-old.zip");
    fs::rename(&target_path, &saved).unwrap();
    queue
        .approve_attention_effects(&entry.operation_id, &attention, true)
        .unwrap();
    assert!(matches!(
        execute(&queue, &calls).unwrap(),
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    let fresh = queue.snapshot().entries[0].attention.clone().unwrap();
    assert!(
        matches!(&fresh,doppeldecker_core::QueueAttention::ConflictsChanged {current_conflicts,..} if current_conflicts.is_empty())
    );
    queue
        .approve_attention_effects(&entry.operation_id, &fresh, true)
        .unwrap();
    let result = execute(&queue, &calls).unwrap();
    assert!(
        matches!(result, QueueExecutionResult::File(_)),
        "{result:?}"
    );
    assert_eq!(fs::read(&target_path).unwrap(), bytes);
    assert!(!path.parent().unwrap().exists());
    assert_eq!(fs::read(&saved).unwrap(), b"Vorhandenes Archiv");
    assert_eq!(
        queue.snapshot().entries[0].generated_source.as_ref(),
        Some(&reference)
    );
}

#[test]
fn unclear_generated_upload_keeps_its_source_and_has_no_blind_retry() {
    let server = server::Server::new();
    let (_area, queue, reference, path, bytes) = fixture();
    queue
        .enqueue_generated_upload(reference.clone(), target(&server), vec![], &Cancel::new())
        .unwrap();
    server.state.lock().unwrap().fault = server::Fault::LostAfterPut;
    let calls = Arc::new(AtomicUsize::new(0));
    let error = execute(&queue, &calls).unwrap_err();
    assert_eq!(
        doppeldecker_core::error_code(&error),
        ErrorCode::OutcomeUnclear,
        "{error}"
    );
    let snapshot = queue.snapshot();
    let entry = &snapshot.entries[0];
    assert_eq!(entry.error_code, Some(ErrorCode::OutcomeUnclear));
    assert_eq!(entry.generated_source.as_ref(), Some(&reference));
    assert!(queue.retry(&entry.operation_id).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
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
fn prepared_overwrite_keeps_the_generated_binding_through_conflict_confirmation() {
    let server = server::Server::new();
    let (_area, queue, reference, path, bytes) = fixture();
    fs::write(server.root.path().join("pane/result.zip"), b"Vorher").unwrap();
    server.state.lock().unwrap().allow_file_overwrite = true;
    let initial = queue
        .enqueue_generated_upload(reference.clone(), target(&server), vec![], &Cancel::new())
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(matches!(
        execute(&queue, &calls).unwrap(),
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    let prepared = queue
        .prepare_generated_upload(&initial.operation_id, &Cancel::new())
        .unwrap();
    let mut operation = prepared.entry.operation.clone();
    let doppeldecker_core::QueueOperation::ProviderFile {
        overwrite,
        replace_best_effort_expected,
        ..
    } = &mut operation
    else {
        unreachable!()
    };
    *overwrite = true;
    *replace_best_effort_expected = Some(true);
    let entry = queue
        .resume_generated_upload(&prepared.entry, operation, vec![], &Cancel::new())
        .unwrap();
    drop(prepared);
    assert!(matches!(
        execute(&queue, &calls).unwrap(),
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    let attention = queue.snapshot().entries[0].attention.clone().unwrap();
    assert!(
        matches!(&attention,doppeldecker_core::QueueAttention::ConflictsChanged {current_conflict_proofs,..} if current_conflict_proofs.len()==1)
    );
    queue
        .approve_attention_effects(&entry.operation_id, &attention, true)
        .unwrap();
    let result = execute(&queue, &calls).unwrap();
    let QueueExecutionResult::File(stats) = result else {
        panic!("{result:?}")
    };
    assert_eq!(stats.files, 1);
    assert!(stats.target_replace_best_effort);
    assert_eq!(
        fs::read(server.root.path().join("pane/result.zip")).unwrap(),
        bytes
    );
    assert!(!path.parent().unwrap().exists());
    assert_eq!(
        queue.snapshot().entries[0].generated_source.as_ref(),
        Some(&reference)
    );
}

#[test]
fn prepared_generated_upload_cannot_change_the_source_or_become_a_move() {
    let server = server::Server::new();
    let (_area, queue, reference, path, _) = fixture();
    let initial = queue
        .enqueue_generated_upload(reference.clone(), target(&server), vec![], &Cancel::new())
        .unwrap();
    for mutation in ["move", "source", "selection"] {
        let mut operation = initial.operation.clone();
        let doppeldecker_core::QueueOperation::ProviderFile {
            kind,
            source,
            items,
            ..
        } = &mut operation
        else {
            unreachable!()
        };
        match mutation {
            "move" => *kind = doppeldecker_core::FileQueueKind::Move,
            "source" => {
                **source = BidirectionalQueueEndpoint::Local {
                    root: path
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                }
            }
            "selection" => {
                items[0] = path
                    .with_file_name("other.zip")
                    .to_string_lossy()
                    .into_owned()
            }
            _ => unreachable!(),
        }
        assert!(
            queue
                .enqueue_generated_upload_operation(
                    reference.clone(),
                    operation,
                    vec![],
                    &Cancel::new()
                )
                .is_err(),
            "{mutation}"
        );
        let saved = queue.snapshot().entries;
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].operation_id, initial.operation_id);
        GeneratedSourceStore::for_queue(&queue)
            .unwrap()
            .resolve(&reference, &Cancel::new())
            .unwrap();
    }
    assert!(server.state.lock().unwrap().requests.is_empty());
}

#[test]
fn generated_extraction_uploads_all_entry_or_subtree_without_reopening_the_zip() {
    use doppeldecker_core::archive_extract::{ArchiveExtractRequest, ArchiveExtractSelection};
    use doppeldecker_core::archive_read::{ArchiveRead, CatalogBudget};
    for scope in ["all", "entry", "subtree"] {
        let server = server::Server::new();
        let (area, queue, _unused, _path, _) = fixture();
        let source = area.path().canonicalize().unwrap().join("source");
        fs::create_dir_all(source.join("tree/empty")).unwrap();
        fs::write(source.join("tree/leaf"), b"Blatt").unwrap();
        fs::write(source.join("other"), b"Anderes").unwrap();
        let archives = source.parent().unwrap().join("archives");
        fs::create_dir(&archives).unwrap();
        prepare_archive_pack(
            &source,
            &[source.join("tree"), source.join("other")],
            &archives,
            "input.zip",
            ArchivePackOptions::default(),
            None,
            &Cancel::new(),
        )
        .unwrap()
        .execute(None, &mut |_| {})
        .unwrap();
        let selection = match scope {
            "all" => ArchiveExtractSelection::All,
            "subtree" => ArchiveExtractSelection::Subtree {
                prefix: "tree".into(),
            },
            "entry" => {
                let reader = ArchiveRead::open(
                    &archives,
                    std::path::Path::new("input.zip"),
                    CatalogBudget::default(),
                    &Cancel::new(),
                )
                .unwrap();
                let ordinal = reader
                    .entries()
                    .iter()
                    .position(|entry| entry.raw_name == b"other")
                    .unwrap();
                ArchiveExtractSelection::Entry {
                    ordinal,
                    raw_name: b"other".to_vec(),
                }
            }
            _ => unreachable!(),
        };
        let workspace = GeneratedSourceStore::for_queue(&queue)
            .unwrap()
            .begin()
            .unwrap();
        let generated = ArchiveExtractRequest {
            source_root: archives.clone(),
            source_path: archives.join("input.zip"),
            snapshot: None,
            target_dir: workspace.path().to_path_buf(),
            options: Default::default(),
            selection,
            sandbox: None,
        }
        .prepare(&Cancel::new())
        .unwrap()
        .execute_generated(None, &mut |_| {})
        .unwrap();
        let reference = workspace
            .seal_extraction(generated, "input.zip".into(), &Cancel::new())
            .unwrap();
        fs::remove_file(archives.join("input.zip")).unwrap();
        let entry = queue
            .enqueue_generated_upload(reference.clone(), target(&server), vec![], &Cancel::new())
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let result = execute(&queue, &calls).unwrap();
        let QueueExecutionResult::File(stats) = result else {
            panic!("{scope}: {result:?}")
        };
        assert_eq!(stats.files, if scope == "all" { 2 } else { 1 });
        let pane = server.root.path().join("pane");
        if scope != "subtree" {
            assert_eq!(fs::read(pane.join("other")).unwrap(), b"Anderes");
        }
        if scope != "entry" {
            assert_eq!(fs::read(pane.join("tree/leaf")).unwrap(), b"Blatt");
            assert!(pane.join("tree/empty").is_dir());
        }
        if scope == "entry" {
            assert!(!pane.join("tree").exists());
        }
        if scope == "subtree" {
            assert!(!pane.join("other").exists());
        }
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
        assert_eq!(queue.snapshot().entries[0].operation_id, entry.operation_id);
    }
}
