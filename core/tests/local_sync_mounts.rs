//! Opt-in-Abnahme des normalen Syncs auf ausschließlich eigenen Mounts.
use doppeldecker_core::{
    prepare_local_sync_run, Cancel, Compare, ErrorCode, LocalVfs, SyncMode, SyncOptions, Vfs,
};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    let root = PathBuf::from(std::env::var("DOPPELDECKER_METADATA_TEST_ROOT").unwrap())
        .canonicalize()
        .unwrap();
    assert!(root
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("doppeldecker-metadata-e2e-"));
    assert_eq!(
        std::fs::read(root.join(".doppeldecker-metadata-test")).unwrap(),
        b"doppeldecker-metadata-e2e-v1\n"
    );
    root
}

fn options(mode: SyncMode) -> SyncOptions {
    SyncOptions {
        mode,
        compare: Compare::ContentHash,
        ..Default::default()
    }
}

fn no_temps(path: &Path) {
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        assert!(
            !entry
                .file_name()
                .to_string_lossy()
                .contains("doppeldecker-tmp"),
            "Temp oder AppleDouble-Temp blieb erhalten: {:?}",
            entry.path()
        );
        if entry.file_type().unwrap().is_dir() {
            no_temps(&entry.path());
        }
    }
}

#[test]
#[ignore = "benötigt eigene native/exFAT-Testmounts"]
fn normal_local_sync_adds_and_updates_on_actual_mounts() {
    let root = root();
    for volume in ["native", "exfat"] {
        let source = tempfile::tempdir_in(root.join("native")).unwrap();
        let target = tempfile::tempdir_in(root.join(volume)).unwrap();
        std::fs::create_dir(source.path().join("Ordner")).unwrap();
        std::fs::write(source.path().join("Ordner/Ä %2F.txt"), b"INHALT").unwrap();
        std::fs::write(source.path().join("Ordner/Daten.txt"), b"INHALT").unwrap();
        let prepared =
            prepare_local_sync_run(source.path(), target.path(), options(SyncMode::AddOnly))
                .unwrap();
        let weak = cfg!(target_os = "macos") && volume == "exfat";
        assert_eq!(
            prepared
                .preview()
                .local_execution
                .as_ref()
                .unwrap()
                .is_best_effort(),
            weak
        );
        if weak {
            assert_eq!(
                doppeldecker_core::error_code(&prepared.confirm(false).err().unwrap()),
                ErrorCode::ConfirmationMismatch
            );
            assert!(std::fs::read_dir(target.path()).unwrap().next().is_none());
        }
        let stats =
            prepare_local_sync_run(source.path(), target.path(), options(SyncMode::AddOnly))
                .unwrap()
                .confirm(true)
                .unwrap()
                .execute(&Cancel::new(), &mut |_| {})
                .unwrap();
        assert_eq!(stats.files_copied, 2);
        assert_eq!(
            std::fs::read(target.path().join("Ordner/Ä %2F.txt")).unwrap(),
            b"INHALT"
        );
        no_temps(target.path());

        let source_file = source.path().join("Ordner/Daten.txt");
        let target_file = target.path().join("Ordner/Daten.txt");
        std::fs::write(&source_file, b"NEU123").unwrap();
        let vfs = LocalVfs::new();
        vfs.set_mtime(&source_file, 1_700_000_000).unwrap();
        vfs.set_mtime(&target_file, 1_700_000_000).unwrap();
        std::fs::write(source.path().join("Addition.txt"), "zusätzlich".as_bytes()).unwrap();
        let prepared =
            prepare_local_sync_run(source.path(), target.path(), options(SyncMode::Update))
                .unwrap();
        assert_eq!(
            prepared.preview().impact.replacements,
            1,
            "{volume}: {:?}",
            prepared.preview().plan.actions
        );
        assert_eq!(
            prepared
                .preview()
                .local_execution
                .as_ref()
                .unwrap()
                .irreversible_replacements,
            u64::from(weak)
        );
        assert_eq!(prepared.preview().restore_probe_ok, !weak);
        assert!(prepared.confirm(false).is_err());
        let stats = prepare_local_sync_run(source.path(), target.path(), options(SyncMode::Update))
            .unwrap()
            .confirm(true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.files_copied, 2);
        assert_eq!(std::fs::read(&source_file).unwrap(), b"NEU123");
        assert_eq!(std::fs::read(&target_file).unwrap(), b"NEU123");
        assert_eq!(
            std::fs::read(target.path().join("Addition.txt")).unwrap(),
            "zusätzlich".as_bytes()
        );
        assert_eq!(stats.quarantine.is_some(), !weak);
        assert_eq!(stats.irreversible_replacements, u64::from(weak));
        no_temps(target.path());
        let native_target = tempfile::tempdir_in(root.join(volume)).unwrap();
        let native_source = tempfile::tempdir_in(root.join("native")).unwrap();
        std::fs::write(native_source.path().join("Native.txt"), b"METADATA").unwrap();
        let mut native_options = options(SyncMode::AddOnly);
        native_options.metadata = doppeldecker_core::MetadataProfile::NativeStrict;
        if volume == "exfat" {
            assert!(prepare_local_sync_run(
                native_source.path(),
                native_target.path(),
                native_options.clone()
            )
            .is_err());
            assert!(std::fs::read_dir(native_target.path())
                .unwrap()
                .next()
                .is_none());
        }
        native_options.metadata = doppeldecker_core::MetadataProfile::NativeBestEffort;
        let prepared =
            prepare_local_sync_run(native_source.path(), native_target.path(), native_options)
                .unwrap();
        if volume == "exfat" {
            assert!(prepared
                .preview()
                .plan
                .metadata_report
                .issues
                .iter()
                .all(|issue| issue.path == "Native.txt"));
        }
        let stats = prepared
            .confirm(true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.files_copied, 1);
        assert_eq!(
            std::fs::read(native_target.path().join("Native.txt")).unwrap(),
            b"METADATA"
        );
        println!("{volume}: Addition, gleicher Zeit-/Größenzustand bei anderem Inhalt, Update, Zustimmung und Tempbilanz bestanden; Best Effort {weak}");
    }
}

#[test]
#[ignore = "benötigt eigene native/exFAT-Testmounts"]
fn normal_local_sync_stops_late_collisions_and_parent_swaps() {
    let root = root();
    for volume in ["native", "exfat"] {
        for swap in [false, true] {
            let source = tempfile::tempdir_in(root.join("native")).unwrap();
            let target = tempfile::tempdir_in(root.join(volume)).unwrap();
            std::fs::create_dir(target.path().join("Ordner")).unwrap();
            std::fs::create_dir(source.path().join("Ordner")).unwrap();
            std::fs::write(
                source.path().join("Ordner/Ziel.txt"),
                vec![b'N'; 512 * 1024],
            )
            .unwrap();
            let run =
                prepare_local_sync_run(source.path(), target.path(), options(SyncMode::AddOnly))
                    .unwrap()
                    .confirm(true)
                    .unwrap();
            let mut changed = false;
            let error = run
                .execute(&Cancel::new(), &mut |progress| {
                    if changed || progress.bytes_done == 0 {
                        return;
                    }
                    changed = true;
                    if swap {
                        std::fs::rename(
                            target.path().join("Ordner"),
                            target.path().join("gehalten"),
                        )
                        .unwrap();
                        std::fs::create_dir(target.path().join("Ordner")).unwrap();
                    } else {
                        let name = if cfg!(target_os = "linux") && volume == "native" {
                            "Ziel.txt"
                        } else {
                            "ziel.TXT"
                        };
                        std::fs::write(target.path().join("Ordner").join(name), b"FREMD").unwrap();
                    }
                })
                .unwrap_err();
            assert!(changed);
            if swap {
                assert!(std::fs::read_dir(target.path().join("Ordner"))
                    .unwrap()
                    .next()
                    .is_none());
            } else {
                let name = if cfg!(target_os = "linux") && volume == "native" {
                    "Ziel.txt"
                } else {
                    "ziel.TXT"
                };
                assert_eq!(
                    std::fs::read(target.path().join("Ordner").join(name)).unwrap(),
                    b"FREMD"
                );
            }
            assert_eq!(
                std::fs::read(source.path().join("Ordner/Ziel.txt"))
                    .unwrap()
                    .len(),
                512 * 1024
            );
            println!(
                "{volume}: {} gestoppt: {}",
                if swap {
                    "Elterntausch"
                } else {
                    "späte Case-Kollision"
                },
                doppeldecker_core::error_code(&error).as_str()
            );
        }
    }
}

#[test]
#[ignore = "benötigt eigene native/exFAT-Testmounts"]
fn normal_local_sync_crash_child() {
    let Ok(queue_path) = std::env::var("DOPPELDECKER_SYNC_CRASH_QUEUE") else {
        return;
    };
    let root = root();
    let queue = doppeldecker_core::OperationQueue::open(Path::new(&queue_path)).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let prepared =
        doppeldecker_core::queue::prepare_local_sync_queue_entry(&queue, &entry, None).unwrap();
    queue
        .approve_local_sync_preparation(&entry.operation_id, &prepared)
        .unwrap();
    let claimed = queue.claim_next().unwrap().unwrap();
    let result = doppeldecker_core::queue::execute_prepared_local_sync_queue_entry(
        &queue,
        &claimed,
        prepared,
        true,
        &root.join("crash-audit.jsonl"),
        &Cancel::new(),
        &mut |progress| {
            if progress.bytes_done > 0 {
                std::process::exit(91);
            }
        },
    );
    panic!("Prozessabbruch wurde nicht ausgelöst: {result:?}");
}

#[test]
#[ignore = "benötigt eigene native/exFAT-Testmounts"]
fn normal_local_sync_queue_crash_recovers_weak_volume_temps() {
    use doppeldecker_core::{OperationQueue, QueueOperation, QueueState};
    let root = root();
    let source = tempfile::tempdir_in(root.join("native")).unwrap();
    let target = tempfile::tempdir_in(root.join("exfat")).unwrap();
    // Der Queueordner bleibt auf dem geschützten Hostvolume. Ein Image mit
    // ignorierten Eigentümern kann selbst keinen privaten Recoveryindex tragen.
    let config = tempfile::tempdir_in(&root).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(config.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(source.path().join("Datei.txt"), vec![b'x'; 2 * 1024 * 1024]).unwrap();
    let path = config.path().join("queue.json");
    let queue = OperationQueue::open(&path).unwrap();
    let prepared =
        prepare_local_sync_run(source.path(), target.path(), options(SyncMode::AddOnly)).unwrap();
    let entry = queue
        .enqueue(QueueOperation::LocalSync {
            source: source.path().to_string_lossy().into_owned(),
            target: target.path().to_string_lossy().into_owned(),
            sandbox: None,
            options: options(SyncMode::AddOnly),
            expected_plan: Box::new(prepared.preview().plan.clone()),
            execution_hint: prepared.preview().local_execution.clone(),
        })
        .unwrap();
    drop(prepared);
    drop(queue);
    let exe = std::env::current_exe().unwrap();
    let child = "normal_local_sync_crash_child";
    let listed = std::process::Command::new(&exe)
        .args([child, "--list", "--ignored"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(listed.stdout)
            .unwrap()
            .lines()
            .filter(|line| *line == format!("{child}: test"))
            .count(),
        1
    );
    let status = std::process::Command::new(exe)
        .args([child, "--exact", "--ignored", "--nocapture"])
        .env("DOPPELDECKER_SYNC_CRASH_QUEUE", &path)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(91));
    let queue = OperationQueue::open(&path).unwrap();
    let recovered = &queue.snapshot().entries[0];
    assert_eq!(recovered.operation_id, entry.operation_id);
    assert_eq!(recovered.state, QueueState::Interrupted);
    assert_eq!(recovered.error_code, Some(ErrorCode::OutcomeUnclear));
    assert!(!recovered.temp_recovery_paths.is_empty());
    assert!(recovered
        .temp_recovery_paths
        .iter()
        .any(|path| Path::new(path).is_file()));
    assert!(queue.claim_next().unwrap().is_none());
    assert_eq!(
        std::fs::read(source.path().join("Datei.txt"))
            .unwrap()
            .len(),
        2 * 1024 * 1024
    );
    println!(
        "Echter Prozessabbruch: Queue unterbrochen, kein Retry, Prüfpfade {:?}",
        recovered.temp_recovery_paths
    );
}
