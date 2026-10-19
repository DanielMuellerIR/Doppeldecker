use std::path::{Path, PathBuf};

use doppeldecker_core::fileops::{prepare_keep_both_targets, verify_keep_both_targets};
use doppeldecker_core::{
    execute_local_queue_entry, find_recursive_conflicts, Cancel, FileQueueKind,
    LocalQueueExecution, LocalVfs, OperationQueue, QueueExecutionResult, QueueOperation,
};

fn selection(root: &Path, name: &str, directory: bool) -> (Vec<PathBuf>, PathBuf) {
    let source = root.join("quelle").join(name);
    let destination = root.join("ziel");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::create_dir(&destination).unwrap();
    if directory {
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("kind.txt"), b"Quelle").unwrap();
        std::fs::create_dir(destination.join(name)).unwrap();
        std::fs::write(destination.join(name).join("kind.txt"), b"Ziel").unwrap();
        std::fs::write(destination.join(name).join("extra.txt"), b"Extra").unwrap();
    } else {
        std::fs::write(&source, b"Quelle").unwrap();
        std::fs::write(destination.join(name), b"Ziel").unwrap();
    }
    (vec![source], destination)
}

#[test]
fn beide_behalten_copy_move_und_queue_reopen_erhalten_originale_und_unterbaeume() {
    for kind in [FileQueueKind::Copy, FileQueueKind::Move] {
        for directory in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let name = if directory {
                "Projekt.Ünicode"
            } else {
                "Bericht.Ünicode.txt"
            };
            let (items, destination) = selection(&root, name, directory);
            let vfs = LocalVfs::new();
            let conflicts = find_recursive_conflicts(&vfs, &vfs, &destination, &items).unwrap();
            let targets =
                prepare_keep_both_targets(&vfs, &vfs, &destination, &items, &conflicts).unwrap();
            assert_eq!(targets.len(), 1);
            let assigned = PathBuf::from(&targets[0].target);
            if !directory {
                assert_eq!(assigned.extension().unwrap(), "txt");
            }
            let journal = root.join("state/queue.json");
            let queue = OperationQueue::open(&journal).unwrap();
            queue
                .enqueue(QueueOperation::File {
                    kind,
                    items: items
                        .iter()
                        .map(|path| path.to_str().unwrap().to_owned())
                        .collect(),
                    destination: destination.to_str().unwrap().into(),
                    overwrite: false,
                    expected_conflicts: conflicts,
                    expected_conflict_proofs: Vec::new(),
                    keep_both_targets: targets.clone(),
                    local_replace_best_effort_expected: Some(false),
                    local_copy_new_best_effort_expected: None,
                })
                .unwrap();
            drop(queue);
            let queue = OperationQueue::open(journal).unwrap();
            let claimed = queue.claim_next().unwrap().unwrap();
            let result = execute_local_queue_entry(
                &queue,
                claimed,
                &LocalQueueExecution {
                    sandbox: Some(root.clone()),
                    sync_quarantine: None,
                    max_delete_entries: 100,
                    max_delete_fraction: 1.0,
                },
                &Cancel::new(),
                &mut |_| {},
            )
            .unwrap();
            assert!(
                matches!(result, QueueExecutionResult::File(ref stats) if stats.files == 1 && stats.skipped == 0 && !stats.cancelled)
            );
            let original = destination.join(name);
            let (original_file, assigned_file) = if directory {
                (original.join("kind.txt"), assigned.join("kind.txt"))
            } else {
                (original, assigned)
            };
            assert_eq!(std::fs::read(original_file).unwrap(), b"Ziel");
            assert_eq!(std::fs::read(assigned_file).unwrap(), b"Quelle");
            assert_eq!(items[0].exists(), kind == FileQueueKind::Copy);
            if directory {
                assert_eq!(
                    std::fs::read(destination.join(name).join("extra.txt")).unwrap(),
                    b"Extra"
                );
            }
        }
    }
}

#[test]
fn beide_behalten_spaete_case_unicode_kollision_und_quellwechsel_sind_drift() {
    for scenario in ["exact", "case", "unicode", "source"] {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let (items, destination) = selection(&root, "Änderung.txt", false);
        let vfs = LocalVfs::new();
        let conflicts = find_recursive_conflicts(&vfs, &vfs, &destination, &items).unwrap();
        let targets =
            prepare_keep_both_targets(&vfs, &vfs, &destination, &items, &conflicts).unwrap();
        let assigned = Path::new(&targets[0].target);
        let name = assigned.file_name().unwrap().to_str().unwrap();
        use unicode_normalization::UnicodeNormalization;
        let collision = match scenario {
            "case" => assigned.with_file_name(name.to_lowercase()),
            "unicode" => assigned.with_file_name(name.nfd().collect::<String>()),
            _ => assigned.to_owned(),
        };
        if scenario == "source" {
            std::fs::write(&items[0], b"Neue Quelle").unwrap();
        } else {
            std::fs::write(&collision, b"Fremd").unwrap();
        }
        let error =
            verify_keep_both_targets(&vfs, &vfs, &items, &destination, &targets).unwrap_err();
        assert_eq!(
            doppeldecker_core::error_code(&error),
            doppeldecker_core::ErrorCode::PlanDrift
        );
        assert_eq!(
            std::fs::read(destination.join("Änderung.txt")).unwrap(),
            b"Ziel"
        );
        if scenario != "source" {
            assert_eq!(std::fs::read(collision).unwrap(), b"Fremd");
        }
    }
}

#[test]
fn konfliktname_kuerzt_langes_unicode_an_zeichengrenzen_und_erhaelt_extension() {
    let name = format!("{}.txt", "🍰ä".repeat(40));
    let generated = doppeldecker_core::generate_conflict_path(
        &doppeldecker_core::BinaryRelativePath(vec![name.into_bytes()]),
        doppeldecker_core::EndpointSide::Right,
    )
    .unwrap();
    let text = std::str::from_utf8(&generated.0[0]).unwrap();
    assert!(text.len() <= 240);
    assert!(text.ends_with(".txt"));
    assert!(text.contains("Konflikt rechts"));
}

#[test]
fn spaete_kollision_beim_commit_erhaelt_fremdes_ziel_und_movequelle() {
    for kind in [FileQueueKind::Copy, FileQueueKind::Move] {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let (items, destination) = selection(&root, "Kollision.txt", false);
        let vfs = LocalVfs::new();
        let conflicts = find_recursive_conflicts(&vfs, &vfs, &destination, &items).unwrap();
        let targets =
            prepare_keep_both_targets(&vfs, &vfs, &destination, &items, &conflicts).unwrap();
        let assigned = PathBuf::from(&targets[0].target);
        let queue = OperationQueue::open(root.join("queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind,
                items: items
                    .iter()
                    .map(|path| path.to_str().unwrap().into())
                    .collect(),
                destination: destination.to_str().unwrap().into(),
                overwrite: false,
                expected_conflicts: conflicts,
                expected_conflict_proofs: Vec::new(),
                keep_both_targets: targets,
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let mut collided = false;
        let result = execute_local_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &LocalQueueExecution {
                sandbox: Some(root.clone()),
                sync_quarantine: None,
                max_delete_entries: 100,
                max_delete_fraction: 1.0,
            },
            &Cancel::new(),
            &mut |progress| {
                if !collided
                    && (progress.current.contains("Kopie")
                        || (kind == FileQueueKind::Move
                            && progress.phase == "move"
                            && !progress.current.is_empty()))
                {
                    std::fs::write(&assigned, b"Spaet fremd").unwrap();
                    collided = true;
                }
            },
        );
        assert!(collided);
        let error = result.unwrap_err();
        assert_eq!(
            doppeldecker_core::error_code(&error),
            doppeldecker_core::ErrorCode::PlanDrift
        );
        assert_eq!(std::fs::read(assigned).unwrap(), b"Spaet fremd");
        assert_eq!(std::fs::read(&items[0]).unwrap(), b"Quelle");
        assert_eq!(
            std::fs::read(destination.join("Kollision.txt")).unwrap(),
            b"Ziel"
        );
    }
}

#[test]
fn abbrechen_nach_erster_konfliktkopie_bilanziert_teilwirkung_ohne_restverlust() {
    for kind in [FileQueueKind::Copy, FileQueueKind::Move] {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let (mut items, destination) = selection(&root, "A.txt", false);
        let second = root.join("quelle/B.txt");
        std::fs::write(&second, b"Zweite Quelle").unwrap();
        std::fs::write(destination.join("B.txt"), b"Zweites Ziel").unwrap();
        items.push(second.clone());
        let vfs = LocalVfs::new();
        let conflicts = find_recursive_conflicts(&vfs, &vfs, &destination, &items).unwrap();
        let targets =
            prepare_keep_both_targets(&vfs, &vfs, &destination, &items, &conflicts).unwrap();
        let queue = OperationQueue::open(root.join("queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind,
                items: items
                    .iter()
                    .map(|path| path.to_str().unwrap().into())
                    .collect(),
                destination: destination.to_str().unwrap().into(),
                overwrite: false,
                expected_conflicts: conflicts,
                expected_conflict_proofs: Vec::new(),
                keep_both_targets: targets.clone(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let cancel = Cancel::new();
        let result = execute_local_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &LocalQueueExecution {
                sandbox: Some(root.clone()),
                sync_quarantine: None,
                max_delete_entries: 100,
                max_delete_fraction: 1.0,
            },
            &cancel,
            &mut |progress| {
                if progress.files_done >= 1 {
                    cancel.cancel();
                }
            },
        )
        .unwrap();
        assert!(
            matches!(result, QueueExecutionResult::File(ref stats) if stats.cancelled && stats.files == 1)
        );
        assert!(Path::new(&targets[0].target).exists());
        assert!(!Path::new(&targets[1].target).exists());
        assert_eq!(std::fs::read(second).unwrap(), b"Zweite Quelle");
        assert_eq!(std::fs::read(destination.join("A.txt")).unwrap(), b"Ziel");
        assert_eq!(
            std::fs::read(destination.join("B.txt")).unwrap(),
            b"Zweites Ziel"
        );
    }
}

#[test]
fn spaete_kollision_im_neuen_konfliktordner_verlangt_neuplanung() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let (items, destination) = selection(&root, "Projekt", true);
    std::fs::write(items[0].join("a.txt"), b"Vorher kopiert").unwrap();
    let vfs = LocalVfs::new();
    let conflicts = find_recursive_conflicts(&vfs, &vfs, &destination, &items).unwrap();
    let targets = prepare_keep_both_targets(&vfs, &vfs, &destination, &items, &conflicts).unwrap();
    let assigned = PathBuf::from(&targets[0].target);
    let queue = OperationQueue::open(root.join("queue.json")).unwrap();
    queue
        .enqueue(QueueOperation::File {
            kind: FileQueueKind::Copy,
            items: items
                .iter()
                .map(|path| path.to_str().unwrap().into())
                .collect(),
            destination: destination.to_str().unwrap().into(),
            overwrite: false,
            expected_conflicts: conflicts,
            expected_conflict_proofs: Vec::new(),
            keep_both_targets: targets,
            local_replace_best_effort_expected: Some(false),
            local_copy_new_best_effort_expected: None,
        })
        .unwrap();
    let mut foreign_target = None;
    let result = execute_local_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &LocalQueueExecution {
            sandbox: Some(root),
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 1.0,
        },
        &Cancel::new(),
        &mut |progress| {
            if foreign_target.is_none()
                && assigned.is_dir()
                && ["a.txt", "kind.txt"].contains(&progress.current.as_str())
            {
                let name = if progress.current == "a.txt" {
                    "kind.txt"
                } else {
                    "a.txt"
                };
                let path = assigned.join(name);
                std::fs::write(&path, b"Spaet fremd").unwrap();
                foreign_target = Some(path);
            }
        },
    );
    let foreign_target = foreign_target.expect("Spätes fremdes Kind angelegt");
    let error = result.unwrap_err();
    assert_eq!(
        doppeldecker_core::error_code(&error),
        doppeldecker_core::ErrorCode::OutcomeUnclear
    );
    assert!(
        error.to_string().contains("Konfliktkopie") && error.to_string().contains("neu planen"),
        "{error}"
    );
    assert_eq!(std::fs::read(foreign_target).unwrap(), b"Spaet fremd");
    assert_eq!(std::fs::read(items[0].join("kind.txt")).unwrap(), b"Quelle");
    assert_eq!(
        std::fs::read(destination.join("Projekt/kind.txt")).unwrap(),
        b"Ziel"
    );
}

#[test]
fn beide_behalten_ordner_bindet_kindinhalt_auch_nach_queue_neustart() {
    use doppeldecker_core::Vfs;
    use std::os::unix::fs::MetadataExt;
    for (kind, nested) in [
        (FileQueueKind::Copy, false),
        (FileQueueKind::Copy, true),
        (FileQueueKind::Move, false),
        (FileQueueKind::Move, true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (items, destination) = selection(&root, "Projekt", true);
        let relative_child = if nested {
            std::fs::create_dir(items[0].join("Unterordner")).unwrap();
            std::fs::create_dir(destination.join("Projekt/Unterordner")).unwrap();
            std::fs::rename(
                destination.join("Projekt/kind.txt"),
                destination.join("Projekt/Unterordner/kind.txt"),
            )
            .unwrap();
            std::fs::rename(
                items[0].join("kind.txt"),
                items[0].join("Unterordner/kind.txt"),
            )
            .unwrap();
            "Unterordner/kind.txt"
        } else {
            "kind.txt"
        };
        let child = items[0].join(relative_child);
        let vfs = LocalVfs::new();
        vfs.set_mtime(&child, 1_700_000_000).unwrap();
        let before = std::fs::metadata(&items[0]).unwrap();
        let conflicts = find_recursive_conflicts(&vfs, &vfs, &destination, &items).unwrap();
        let targets =
            prepare_keep_both_targets(&vfs, &vfs, &destination, &items, &conflicts).unwrap();
        let assigned = PathBuf::from(&targets[0].target);
        let journal = root.join("state/queue.json");
        let queue = OperationQueue::open(&journal).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind,
                items: items.iter().map(|p| p.to_str().unwrap().into()).collect(),
                destination: destination.to_str().unwrap().into(),
                overwrite: false,
                expected_conflicts: conflicts,
                expected_conflict_proofs: vec![],
                keep_both_targets: targets,
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        drop(queue);
        std::fs::write(&child, b"NEU123").unwrap();
        vfs.set_mtime(&child, 1_700_000_000).unwrap();
        let after = std::fs::metadata(&items[0]).unwrap();
        assert_eq!(
            (
                before.dev(),
                before.ino(),
                before.mtime(),
                before.mtime_nsec()
            ),
            (after.dev(), after.ino(), after.mtime(), after.mtime_nsec())
        );
        let queue = OperationQueue::open(journal).unwrap();
        let result = execute_local_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &LocalQueueExecution {
                sandbox: Some(root.clone()),
                sync_quarantine: None,
                max_delete_entries: 100,
                max_delete_fraction: 1.0,
            },
            &Cancel::new(),
            &mut |_| {},
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => {
                assert_eq!(
                    std::fs::read(assigned.join(relative_child)).unwrap(),
                    b"NEU123"
                );
                panic!("Die Konfliktkopie übernahm ungeprüft einen nach der Vorschau geänderten Kindinhalt");
            }
        };
        assert_eq!(
            doppeldecker_core::error_code(&error),
            doppeldecker_core::ErrorCode::PlanDrift
        );
        assert_eq!(
            std::fs::symlink_metadata(&assigned).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(std::fs::read(child).unwrap(), b"NEU123");
        assert_eq!(
            std::fs::read(destination.join("Projekt").join(relative_child)).unwrap(),
            b"Ziel"
        );
        assert_eq!(
            std::fs::read(destination.join("Projekt/extra.txt")).unwrap(),
            b"Extra"
        );
    }
}
