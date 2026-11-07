//! Rückupload benutzt den vorhandenen Queuevertrag einschließlich später Drift.
use doppeldecker_core::*;
use std::path::Path;
use std::sync::Arc;

struct NoSecrets;
impl SecretResolver for NoSecrets {
    fn resolve(&self, _: &KeychainSecretReference) -> std::io::Result<SecretString> {
        unreachable!("lokale Gegenprobe")
    }
}

fn operation(source: &Path, target: &Path, proofs: Vec<FileConflictProof>) -> QueueOperation {
    QueueOperation::ProviderFile {
        kind: FileQueueKind::Copy,
        source: Box::new(BidirectionalQueueEndpoint::Local {
            root: source.parent().unwrap().to_string_lossy().into_owned(),
        }),
        target: Box::new(BidirectionalQueueEndpoint::Local {
            root: target.parent().unwrap().to_string_lossy().into_owned(),
        }),
        items: vec![source.to_string_lossy().into_owned()],
        destination: target.parent().unwrap().to_string_lossy().into_owned(),
        overwrite: true,
        expected_conflicts: vec![target.to_string_lossy().into_owned()],
        expected_conflict_proofs: proofs,
        source_tree_expected: None,
        keep_both_targets: Vec::new(),
        best_effort_commit_expected: None,
        local_copy_new_best_effort_expected: None,
        copy_new_best_effort_expected: Some(false),
        replace_best_effort_expected: Some(true),
        remove_source_best_effort_expected: None,
    }
}

fn execute(queue: &OperationQueue) -> std::io::Result<QueueExecutionResult> {
    let entry = queue.claim_next()?.unwrap();
    queue::execute_provider_file_queue_entry(
        queue,
        entry,
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(NoSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
}

#[test]
fn edit_save_queue_bindet_original_und_erlaubt_nach_erfolg_einen_weiteren_save() {
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = root.join("server");
    std::fs::create_dir(&target).unwrap();
    let target_file = target.join("Seite.html");
    std::fs::write(&target_file, b"Original").unwrap();
    let target_vfs = LocalVfs::rooted(&target).unwrap();
    let original = download_remote_snapshot(
        &target_vfs,
        &target_file,
        &root.join("cache"),
        &Cancel::new(),
    )
    .unwrap();
    let edit = remote_edit::create_working_copy(
        Path::new(&original.path),
        &root.join("edit"),
        "Seite.html",
        &Cancel::new(),
    )
    .unwrap();
    let queue_dir = root.join("queue");
    std::fs::create_dir(&queue_dir).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&queue_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let queue = OperationQueue::open(queue_dir.join("queue.json")).unwrap();
    let mut proof = original.original;
    for content in [b"Erster Save".as_slice(), b"Zweiter Save".as_slice()] {
        std::fs::write(&edit.path, content).unwrap();
        verify_remote_original(&target_vfs, &target_file, &proof, &Cancel::new()).unwrap();
        let staged = remote_edit::create_working_copy(
            &edit.path,
            &root.join("uploads"),
            "Seite.html",
            &Cancel::new(),
        )
        .unwrap();
        let proofs =
            remote_edit::bind_original_replacement(&target_vfs, &target_file, &proof).unwrap();
        queue
            .enqueue(operation(&staged.path, &target_file, proofs))
            .unwrap();
        // Die Anwendung darf sofort weiterbearbeiten; der eingereihte Save bleibt exakt.
        std::fs::write(&edit.path, b"Noch nicht gespeichert").unwrap();
        let result = execute(&queue).unwrap();
        assert!(
            matches!(result, QueueExecutionResult::File(_)),
            "{result:?}\n{:?}",
            queue.snapshot().entries.last().unwrap()
        );
        assert_eq!(std::fs::read(&target_file).unwrap(), content);
        assert_eq!(
            queue.snapshot().entries.last().unwrap().state,
            QueueState::Completed
        );
        proof = RemoteOriginalProof {
            bytes: content.len() as u64,
            blake3: staged.blake3,
            mtime: None,
            mode: None,
        };
    }
}

#[test]
fn edit_save_erkennt_remoteaenderung_vor_save_und_am_queuekopf() {
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = root.join("server");
    std::fs::create_dir(&target).unwrap();
    let target_file = target.join("Seite.html");
    std::fs::write(&target_file, b"Original").unwrap();
    let target_vfs = LocalVfs::rooted(&target).unwrap();
    let original = download_remote_snapshot(
        &target_vfs,
        &target_file,
        &root.join("cache"),
        &Cancel::new(),
    )
    .unwrap();
    let staged = remote_edit::create_working_copy(
        Path::new(&original.path),
        &root.join("uploads"),
        "Seite.html",
        &Cancel::new(),
    )
    .unwrap();
    let proofs =
        remote_edit::bind_original_replacement(&target_vfs, &target_file, &original.original)
            .unwrap();
    let queue_dir = root.join("queue");
    std::fs::create_dir(&queue_dir).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&queue_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let queue = OperationQueue::open(queue_dir.join("queue.json")).unwrap();
    queue
        .enqueue(operation(&staged.path, &target_file, proofs))
        .unwrap();
    std::fs::write(&target_file, b"Fremde Aenderung").unwrap();
    assert_eq!(
        error_code(
            &verify_remote_original(
                &target_vfs,
                &target_file,
                &original.original,
                &Cancel::new()
            )
            .unwrap_err()
        ),
        ErrorCode::PlanDrift
    );
    assert_eq!(
        error_code(
            &remote_edit::bind_original_replacement(&target_vfs, &target_file, &original.original)
                .unwrap_err()
        ),
        ErrorCode::PlanDrift
    );
    assert!(matches!(
        execute(&queue).unwrap(),
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    assert_eq!(std::fs::read(target_file).unwrap(), b"Fremde Aenderung");
}
