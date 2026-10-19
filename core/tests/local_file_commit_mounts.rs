//! Opt-in-Produktgegenlauf ausschließlich auf eigenen nativen/exFAT-Images.
use doppeldecker_core::vfs::FileCommitClass;
use doppeldecker_core::{LocalVfs, Vfs};
use std::path::PathBuf;

fn test_root() -> PathBuf {
    let root = PathBuf::from(std::env::var("DOPPELDECKER_METADATA_TEST_ROOT").unwrap());
    assert!(root
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("doppeldecker-metadata-e2e-"));
    assert_eq!(
        std::fs::read_to_string(root.join(".doppeldecker-metadata-test")).unwrap(),
        "doppeldecker-metadata-e2e-v1\n"
    );
    root
}

#[test]
#[cfg(debug_assertions)]
#[ignore = "benötigt eigene native/exFAT-Testmounts"]
fn local_file_commit_class_matches_actual_volume() {
    let root = test_root();
    let vfs = LocalVfs::new();
    assert_eq!(
        vfs.file_commit_class_at(&root.join("native")).unwrap(),
        FileCommitClass::Assured
    );
    #[cfg(target_os = "macos")]
    assert_eq!(
        vfs.file_commit_class_at(&root.join("exfat")).unwrap(),
        FileCommitClass::BestEffort
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        vfs.file_commit_class_at(&root.join("exfat")).unwrap(),
        FileCommitClass::Assured
    );
}

fn execution(root: &std::path::Path) -> doppeldecker_core::LocalQueueExecution {
    doppeldecker_core::LocalQueueExecution {
        sandbox: Some(root.to_path_buf()),
        sync_quarantine: None,
        max_delete_entries: 100,
        max_delete_fraction: 0.25,
    }
}

fn operation(
    source: &std::path::Path,
    target: &std::path::Path,
    kind: doppeldecker_core::FileQueueKind,
) -> doppeldecker_core::QueueOperation {
    doppeldecker_core::QueueOperation::File {
        kind,
        items: vec![source.to_string_lossy().into_owned()],
        destination: target.to_string_lossy().into_owned(),
        overwrite: false,
        expected_conflicts: vec![],
        keep_both_targets: Vec::new(),
        expected_conflict_proofs: vec![],
        local_replace_best_effort_expected: Some(false),
        local_copy_new_best_effort_expected: None,
    }
}

#[test]
#[cfg(debug_assertions)]
#[ignore = "benötigt eigene native/exFAT-Testmounts"]
fn local_file_queue_binds_actual_volume_before_writing() {
    use doppeldecker_core::{
        execute_local_queue_entry, Cancel, FileQueueKind, OperationQueue, QueueAttention,
        QueueExecutionResult, QueueState,
    };
    let root = test_root().canonicalize().unwrap();
    let source_root = tempfile::tempdir_in(root.join("native")).unwrap();
    let source = source_root.path().join("quelle.txt");
    let content = vec![b'x'; 2 * 1024 * 1024];
    std::fs::write(&source, &content).unwrap();
    for volume in ["native", "exfat"] {
        let target = tempfile::tempdir_in(root.join(volume)).unwrap();
        let state = tempfile::tempdir_in(root.join("native")).unwrap();
        let path = state.path().join("queue.json");
        let mut queue = OperationQueue::open(&path).unwrap();
        let entry = queue
            .enqueue(operation(&source, target.path(), FileQueueKind::Copy))
            .unwrap();
        let result = execute_local_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &execution(&root),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        let weak = cfg!(target_os = "macos") && volume == "exfat";
        let result = if weak {
            assert!(matches!(
                result,
                QueueExecutionResult::AwaitingConfirmation { .. }
            ));
            assert!(
                std::fs::read_dir(target.path()).unwrap().next().is_none(),
                "vor Zustimmung keine Tempdatei"
            );
            let snapshot = queue.snapshot();
            let QueueAttention::LocalFileCommit {
                confirmation,
                reason,
                target_identity,
            } = snapshot.entries[0].attention.as_ref().unwrap()
            else {
                panic!("volumengebundene Rückfrage fehlt")
            };
            assert!(reason.contains("gleichzeitig neu angelegtes Ziel"));
            assert_eq!(target_identity.display_path, target.path());
            assert!(queue
                .approve_attention(&entry.operation_id, "falsch")
                .is_err());
            queue
                .approve_attention(&entry.operation_id, confirmation)
                .unwrap();
            drop(queue);
            queue = OperationQueue::open(&path).unwrap();
            execute_local_queue_entry(
                &queue,
                queue.claim_next().unwrap().unwrap(),
                &execution(&root),
                &Cancel::new(),
                &mut |_| {},
            )
            .unwrap()
        } else {
            result
        };
        let QueueExecutionResult::File(stats) = result else {
            panic!("Dateiergebnis fehlt")
        };
        assert_eq!(stats.target_copy_new_best_effort, weak);
        assert_eq!(
            std::fs::read(target.path().join("quelle.txt")).unwrap(),
            content
        );
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
        let audit = std::fs::read_to_string(state.path().join("queue-audit.jsonl")).unwrap();
        let record: serde_json::Value = audit
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|record| record["status"] == "completed")
            .unwrap();
        assert_eq!(record["target_copy_new_best_effort_actual"], weak);
        println!("{volume}: Queue, Neustart und tatsächlicher Audit stimmen (Best Effort={weak})");
    }

    #[cfg(target_os = "macos")]
    {
        let target = tempfile::tempdir_in(root.join("exfat")).unwrap();
        let state = tempfile::tempdir_in(root.join("native")).unwrap();
        let queue = OperationQueue::open(state.path().join("queue.json")).unwrap();
        let entry = queue
            .enqueue(operation(&source, target.path(), FileQueueKind::Copy))
            .unwrap();
        execute_local_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &execution(&root),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        let snapshot = queue.snapshot();
        let QueueAttention::LocalFileCommit { confirmation, .. } =
            snapshot.entries[0].attention.as_ref().unwrap()
        else {
            panic!()
        };
        queue
            .approve_attention(&entry.operation_id, confirmation)
            .unwrap();
        let old = target.path().with_extension("old");
        std::fs::rename(target.path(), &old).unwrap();
        std::fs::create_dir(target.path()).unwrap();
        let result = execute_local_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &execution(&root),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert!(matches!(
            result,
            QueueExecutionResult::AwaitingConfirmation { .. }
        ));
        assert!(std::fs::read_dir(target.path()).unwrap().next().is_none());
        assert!(std::fs::read_dir(&old).unwrap().next().is_none());
        std::fs::remove_dir(&old).unwrap();
        println!("ausgetauschter exFAT-Root benötigt neue Bestätigung vor jeder Wirkung");
    }
}

#[test]
#[cfg(debug_assertions)]
#[ignore = "benötigt eigene native/exFAT-Testmounts"]
fn local_file_commit_handles_collision_cancel_move_and_strict_sync() {
    use doppeldecker_core::{
        copy_items, move_items, Cancel, FileOperationRequest, OverwritePolicy,
    };
    let root = test_root().canonicalize().unwrap();
    let source_root = tempfile::tempdir_in(root.join("native")).unwrap();
    let source = source_root.path().join("quelle.txt");
    let content = vec![b'x'; 2 * 1024 * 1024];
    std::fs::write(&source, &content).unwrap();
    let target = tempfile::tempdir_in(root.join("exfat")).unwrap();
    let vfs = LocalVfs::new();
    let items = vec![source.clone()];
    let sandbox = Some(root.clone());
    let request = || FileOperationRequest {
        src: &vfs,
        dst: &vfs,
        items: &items,
        dst_dir: target.path(),
        policy: OverwritePolicy::Skip,
        sandbox: &sandbox,
    };
    let mut collided = false;
    let error = copy_items(request(), &Cancel::new(), &mut |p| {
        if p.bytes_done > 0 && !collided {
            std::fs::write(target.path().join("quelle.txt"), b"fremd").unwrap();
            collided = true;
        }
    })
    .unwrap_err();
    assert!(collided);
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
    assert_eq!(
        std::fs::read(target.path().join("quelle.txt")).unwrap(),
        b"fremd"
    );
    assert_eq!(std::fs::read(&source).unwrap(), content);
    std::fs::remove_file(target.path().join("quelle.txt")).unwrap();
    let cancel = Cancel::new();
    let stats = copy_items(request(), &cancel, &mut |p| {
        if p.bytes_done > 0 {
            cancel.cancel();
        }
    })
    .unwrap();
    assert!(stats.cancelled);
    assert!(!target.path().join("quelle.txt").exists());
    assert_eq!(std::fs::read(&source).unwrap(), content);
    let stats = move_items(request(), &Cancel::new(), &mut |_| {}).unwrap();
    assert_eq!(stats.target_copy_new_best_effort, cfg!(target_os = "macos"));
    assert!(stats.source_removal_best_effort);
    assert!(!source.exists());
    assert_eq!(
        std::fs::read(target.path().join("quelle.txt")).unwrap(),
        content
    );
    let directory = source_root.path().join("ordner");
    std::fs::create_dir_all(directory.join("tief")).unwrap();
    std::fs::write(directory.join("tief/datei.txt"), b"Ordnerinhalt").unwrap();
    let directories = vec![directory.clone()];
    let stats = move_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &directories,
            dst_dir: target.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &sandbox,
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(!directory.exists());
    assert_eq!(
        std::fs::read(target.path().join("ordner/tief/datei.txt")).unwrap(),
        b"Ordnerinhalt"
    );
    assert_eq!(stats.target_copy_new_best_effort, cfg!(target_os = "macos"));
    println!("exFAT: späte Kollision, Abbruch und Move-Readback bestanden");

    #[cfg(target_os = "macos")]
    {
        use doppeldecker_core::{execute, plan, Compare, ExecuteRequest, Safety, SyncOptions};
        let strict_target = tempfile::tempdir_in(root.join("exfat")).unwrap();
        std::fs::write(&source, b"strikter Sync bleibt exklusiv").unwrap();
        let options = SyncOptions {
            compare: Compare::ContentHash,
            ..Default::default()
        };
        let preview = plan(
            &vfs,
            source_root.path(),
            &vfs,
            strict_target.path(),
            &options,
        )
        .unwrap();
        assert!(execute(
            ExecuteRequest {
                src: &vfs,
                src_root: source_root.path(),
                dst: &vfs,
                dst_root: strict_target.path(),
                plan: &preview,
                safety: &Safety::headless_test()
            },
            &Cancel::new(),
            &mut |_| {}
        )
        .is_err());
        assert!(!strict_target.path().join("quelle.txt").exists());
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"strikter Sync bleibt exklusiv"
        );
        println!("strikter Sync erhält keinen gewöhnlichen Rename-Fallback");
    }
}

#[test]
#[cfg(debug_assertions)]
#[ignore = "benötigt eigene native/exFAT-Testmounts"]
fn provider_file_queue_checks_local_target_without_duplicate_confirmation() {
    use doppeldecker_core::{
        execute_provider_file_queue_entry, BidirectionalQueueEndpoint, Cancel, FileQueueKind,
        OperationQueue, ProviderEndpoint, QueueAttention, QueueExecutionResult,
    };
    struct NoSecrets;
    impl doppeldecker_core::SecretResolver for NoSecrets {
        fn resolve(
            &self,
            _: &doppeldecker_core::KeychainSecretReference,
        ) -> std::io::Result<doppeldecker_core::SecretString> {
            panic!("lokaler Test benötigt kein Secret")
        }
    }
    let root = test_root().canonicalize().unwrap();
    let source = tempfile::tempdir_in(root.join("native")).unwrap();
    let target = tempfile::tempdir_in(root.join("exfat")).unwrap();
    let state = tempfile::tempdir_in(root.join("native")).unwrap();
    let item = source.path().join("provider.txt");
    std::fs::write(&item, b"lokales Downloadziel").unwrap();
    let caps = ProviderEndpoint::local(target.path())
        .unwrap()
        .probe_file_capabilities()
        .unwrap();
    let weak = cfg!(target_os = "macos");
    assert_eq!(caps.copy_new, !weak);
    assert_eq!(caps.copy_new_best_effort, weak);
    let queue = OperationQueue::open(state.path().join("queue.json")).unwrap();
    let entry = queue
        .enqueue(doppeldecker_core::QueueOperation::ProviderFile {
            kind: FileQueueKind::Copy,
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: source.path().to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::Local {
                root: target.path().to_string_lossy().into_owned(),
            }),
            items: vec![item.to_string_lossy().into_owned()],
            destination: target.path().to_string_lossy().into_owned(),
            overwrite: false,
            expected_conflicts: vec![],
            source_tree_expected: None,
            keep_both_targets: Vec::new(),
            expected_conflict_proofs: vec![],
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: Some(false),
            replace_best_effort_expected: Some(false),
            remove_source_best_effort_expected: Some(false),
        })
        .unwrap();
    let result = execute_provider_file_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &execution(&root),
        std::sync::Arc::new(NoSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    let result = if weak {
        assert!(matches!(
            result,
            QueueExecutionResult::AwaitingConfirmation { .. }
        ));
        assert!(std::fs::read_dir(target.path()).unwrap().next().is_none());
        let snapshot = queue.snapshot();
        let QueueAttention::LocalFileCommit { confirmation, .. } =
            snapshot.entries[0].attention.as_ref().unwrap()
        else {
            panic!()
        };
        queue
            .approve_attention(&entry.operation_id, confirmation)
            .unwrap();
        execute_provider_file_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &execution(&root),
            std::sync::Arc::new(NoSecrets),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap()
    } else {
        result
    };
    let QueueExecutionResult::File(stats) = result else {
        panic!("unnötige zweite Rückfrage")
    };
    assert_eq!(stats.target_copy_new_best_effort, weak);
    assert_eq!(
        std::fs::read(target.path().join("provider.txt")).unwrap(),
        b"lokales Downloadziel"
    );
}

#[test]
#[cfg(debug_assertions)]
#[ignore = "benötigt eigene native/exFAT-Testmounts"]
fn local_file_queue_rejects_target_root_swap_during_transfer() {
    use doppeldecker_core::{
        execute_local_queue_entry, Cancel, FileQueueKind, OperationQueue, QueueAttention,
        QueueExecutionResult,
    };
    let root = test_root().canonicalize().unwrap();
    let source_root = tempfile::tempdir_in(root.join("native")).unwrap();
    let source = source_root.path().join("quelle.txt");
    std::fs::write(&source, vec![b'x'; 2 * 1024 * 1024]).unwrap();
    let target = tempfile::tempdir_in(root.join("exfat")).unwrap();
    let state = tempfile::tempdir_in(root.join("native")).unwrap();
    let queue = OperationQueue::open(state.path().join("queue.json")).unwrap();
    let entry = queue
        .enqueue(operation(&source, target.path(), FileQueueKind::Copy))
        .unwrap();
    if cfg!(target_os = "macos") {
        assert!(matches!(
            execute_local_queue_entry(
                &queue,
                queue.claim_next().unwrap().unwrap(),
                &execution(&root),
                &Cancel::new(),
                &mut |_| {}
            )
            .unwrap(),
            QueueExecutionResult::AwaitingConfirmation { .. }
        ));
        let snapshot = queue.snapshot();
        let QueueAttention::LocalFileCommit { confirmation, .. } =
            snapshot.entries[0].attention.as_ref().unwrap()
        else {
            panic!()
        };
        queue
            .approve_attention(&entry.operation_id, confirmation)
            .unwrap();
    }
    let old = target.path().with_extension("old");
    let mut swapped = false;
    let result = execute_local_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &execution(&root),
        &Cancel::new(),
        &mut |progress| {
            if progress.bytes_done > 0 && !swapped {
                std::fs::rename(target.path(), &old).unwrap();
                std::fs::create_dir(target.path()).unwrap();
                std::fs::write(target.path().join("fremd.txt"), b"fremder Nachfolger").unwrap();
                swapped = true;
            }
        },
    );
    assert!(swapped);
    assert!(
        result.is_err(),
        "Roottausch darf nicht als erfolgreicher Pfadcommit gelten: {result:?}"
    );
    assert_eq!(
        std::fs::read(target.path().join("fremd.txt")).unwrap(),
        b"fremder Nachfolger"
    );
    assert!(!target.path().join("quelle.txt").exists());
    assert!(source.exists());
}
