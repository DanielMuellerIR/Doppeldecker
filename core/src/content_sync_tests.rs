use super::*;
use crate::queue::{BidirectionalQueueEndpoint, LocalQueueExecution};
use crate::{MetadataProfile, OperationQueue, QueueOperation, QueueState};
use std::fs;
thread_local! {
    static PREPARE_FIXTURES: std::cell::RefCell<Vec<(tempfile::TempDir, crate::OperationQueue)>> = const { std::cell::RefCell::new(Vec::new()) };
}
fn prepare_content_sync(
    source: ProviderEndpoint,
    target: ProviderEndpoint,
    options: SyncOptions,
    cancel: &Cancel,
) -> io::Result<PreparedContentSync> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let queue =
        crate::OperationQueue::open(fixture.path().canonicalize()?.join("operations-queue.json"))?;
    let (staging, _, _) = queue.ftps_owner(None)?;
    let prepared = super::prepare_content_sync_owned(
        source,
        target,
        options,
        cancel,
        Some(crate::remote_real_mode::InteractiveFileOwner {
            staging,
            profiles: Vec::new(),
        }),
    )?;
    PREPARE_FIXTURES.with(|fixtures| fixtures.borrow_mut().push((fixture, queue)));
    Ok(prepared)
}

fn endpoints(source: &Path, target: &Path) -> (ProviderEndpoint, ProviderEndpoint) {
    (
        ProviderEndpoint::local(source).unwrap(),
        ProviderEndpoint::test_sftp_local(target).unwrap(),
    )
}
fn options(mode: SyncMode) -> SyncOptions {
    SyncOptions {
        mode,
        compare: Compare::ContentHash,
        ..SyncOptions::default()
    }
}

#[test]
fn content_sync_hardlink_bilanziert_neueren_abweichenden_zielnamen() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("quelle");
    let target = root.join("ziel");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("a.bin"), b"quelle").unwrap();
    fs::hard_link(source.join("a.bin"), source.join("b.bin")).unwrap();
    fs::write(target.join("a.bin"), b"quelle").unwrap();
    fs::write(target.join("b.bin"), b"neueres ziel").unwrap();
    for (path, seconds) in [
        (source.join("a.bin"), 1_700_000_000),
        (target.join("a.bin"), 1_800_000_000),
        (target.join("b.bin"), 1_800_000_000),
    ] {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(seconds, 0)).unwrap();
    }
    let vfs = crate::LocalVfs::new();
    let options = SyncOptions {
        metadata: MetadataProfile::NativeBestEffort,
        ..options(SyncMode::Update)
    };
    let plan = crate::sync::plan_content_with_cancel(
        &vfs,
        &source,
        &vfs,
        &target,
        &options,
        &Cancel::new(),
        false,
    )
    .unwrap();
    assert_eq!(plan.skip_count, 1);
    assert!(plan
        .actions
        .iter()
        .any(|action| matches!(action, crate::Action::SkipConflict { path } if path == "b.bin")));
    let row = plan
        .comparison
        .rows
        .iter()
        .find(|row| row.path == "b.bin")
        .expect("Der ausgelassene Hardlink braucht eine gebundene Vergleichszeile");
    assert_eq!(row.action, crate::ComparisonAction::Skip);
    assert_eq!(row.affected_side, crate::ComparisonSide::Neither);
    assert_eq!(row.source.as_ref().unwrap().mtime, Some(1_700_000_000));
    assert_eq!(row.target.as_ref().unwrap().mtime, Some(1_800_000_000));
    let stats = crate::execute(
        crate::ExecuteRequest {
            src: &vfs,
            src_root: &source,
            dst: &vfs,
            dst_root: &target,
            plan: &plan,
            safety: &Safety::headless_test(),
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(stats.files_copied, 0);
    assert_eq!(fs::read(target.join("b.bin")).unwrap(), b"neueres ziel");
}

fn run(prepared: PreparedContentSync) -> SyncStats {
    let digest = prepared.preview().digest;
    prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap()
}
#[test]
fn content_sync_addonly_update_unterbaum_und_zweiter_lauf() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("quelle");
    let target = temp.path().join("ziel");
    fs::create_dir_all(source.join("ordner")).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("ordner/datei"), b"alpha").unwrap();
    let (s, t) = endpoints(&source, &target);
    let first =
        run(prepare_content_sync(s, t, options(SyncMode::AddOnly), &Cancel::new()).unwrap());
    assert_eq!(first.files_copied, 1);
    assert_eq!(fs::read(target.join("ordner/datei")).unwrap(), b"alpha");
    let modified = fs::metadata(source.join("ordner/datei"))
        .unwrap()
        .modified()
        .unwrap();
    fs::write(source.join("ordner/datei"), b"bravo").unwrap();
    filetime::set_file_mtime(
        source.join("ordner/datei"),
        filetime::FileTime::from_system_time(modified),
    )
    .unwrap();
    let (s, t) = endpoints(&source, &target);
    let prepared = prepare_content_sync(s, t, options(SyncMode::Update), &Cancel::new()).unwrap();
    assert_eq!(prepared.preview().irreversible_replacements, 1);
    let stats = run(prepared);
    assert_eq!(stats.irreversible_replacements, 1);
    assert_eq!(fs::read(target.join("ordner/datei")).unwrap(), b"bravo");
    let (s, t) = endpoints(&source, &target);
    let next = prepare_content_sync(s, t, options(SyncMode::Update), &Cancel::new()).unwrap();
    assert_eq!(next.preview().plan.update_count, 0);
    assert_eq!(run(next).files_copied, 0);
}
#[test]
fn content_sync_drift_stoppt_vor_erster_wirkung() {
    for change_target in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("a"), b"a").unwrap();
        let (s, t) = endpoints(&source, &target);
        let prepared =
            prepare_content_sync(s, t, options(SyncMode::AddOnly), &Cancel::new()).unwrap();
        let digest = prepared.preview().digest;
        fs::write(
            if change_target {
                target.join("fremd")
            } else {
                source.join("neu")
            },
            b"neu",
        )
        .unwrap();
        let error = match prepared.confirm_effects(digest, true) {
            Ok(_) => panic!("Drift muss stoppen"),
            Err(error) => error,
        };
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert!(!target.join("a").exists());
    }
}
#[test]
fn content_sync_spaete_kollision_und_abbruch_erhalten_fremdes_ziel() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("quelle");
    let target = temp.path().join("ziel");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("a"), vec![7u8; 200000]).unwrap();
    let (s, t) = endpoints(&source, &target);
    let prepared = prepare_content_sync(s, t, options(SyncMode::AddOnly), &Cancel::new()).unwrap();
    let digest = prepared.preview().digest;
    let run = prepared.confirm_effects(digest, true).unwrap();
    let mut collision = false;
    let result = run.execute(&Cancel::new(), &mut |progress| {
        if progress.bytes_done > 0 && !collision {
            fs::write(target.join("a"), b"fremd").unwrap();
            collision = true;
        }
    });
    assert!(result.is_err());
    assert_eq!(fs::read(target.join("a")).unwrap(), b"fremd");
    fs::remove_file(target.join("a")).unwrap();
    let (s, t) = endpoints(&source, &target);
    let prepared = prepare_content_sync(s, t, options(SyncMode::AddOnly), &Cancel::new()).unwrap();
    let digest = prepared.preview().digest;
    let run = prepared.confirm_effects(digest, true).unwrap();
    let cancel = Cancel::new();
    cancel.cancel();
    assert!(run.execute(&cancel, &mut |_| {}).unwrap().cancelled);
    assert!(!target.join("a").exists());
}
#[test]
fn content_sync_native_metadaten_bleiben_am_faehigen_ziel_erhalten() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("quelle");
    let target = temp.path().join("ziel");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("a"), b"inhalt").unwrap();
    fs::set_permissions(source.join("a"), fs::Permissions::from_mode(0o751)).unwrap();
    let (s, t) = endpoints(&source, &target);
    let mut opts = options(SyncMode::AddOnly);
    opts.metadata = MetadataProfile::NativeBestEffort;
    let prepared = prepare_content_sync(s, t, opts, &Cancel::new()).unwrap();
    assert!(prepared
        .preview()
        .plan
        .metadata_report
        .classes
        .iter()
        .any(|class| class.transferred > 0));
    run(prepared);
    assert_eq!(
        fs::metadata(target.join("a")).unwrap().permissions().mode() & 0o777,
        0o751
    );
}
#[test]
fn content_sync_queue_reopen_braucht_frische_bestaetigung() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("quelle");
    let target = temp.path().join("ziel");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("a"), b"a").unwrap();
    let (s, t) = endpoints(&source, &target);
    let prepared = prepare_content_sync(s, t, options(SyncMode::AddOnly), &Cancel::new()).unwrap();
    let journal = temp.path().join("state/operations-queue.json");
    private_queue_parent(&journal);
    let queue = OperationQueue::open(&journal).unwrap();
    let id = queue
        .enqueue(QueueOperation::ContentSync {
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: source.to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::Local {
                root: target.to_string_lossy().into_owned(),
            }),
            options: options(SyncMode::AddOnly),
            expected_plan: Box::new(prepared.preview().plan.clone()),
            binding_digest: prepared.preview().digest,
        })
        .unwrap();
    drop(queue);
    drop(prepared);
    private_queue_parent(&journal);
    let queue = OperationQueue::open(&journal).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let error = crate::execute_local_queue_entry(
        &queue,
        entry,
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(!target.join("a").exists());
    assert_eq!(queue.snapshot().entries[0].operation_id, id.operation_id);
    assert_ne!(queue.snapshot().entries[0].state, QueueState::Running);
}

#[test]
fn content_sync_mirror_kopiert_vor_geplanten_dauerhaften_entfernungen() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("quelle");
    let target = temp.path().join("ziel");
    fs::create_dir(&source).unwrap();
    fs::create_dir_all(target.join("alt/unter")).unwrap();
    fs::write(source.join("neu"), b"neu").unwrap();
    fs::write(target.join("alt/unter/datei"), b"alt").unwrap();
    let (s, t) = endpoints(&source, &target);
    let prepared = prepare_content_sync(s, t, options(SyncMode::Mirror), &Cancel::new()).unwrap();
    assert_eq!(prepared.preview().irreversible_removals, 3);
    let digest = prepared.preview().digest;
    let mut saw_delete = false;
    let stats = prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |progress| {
            if progress.phase == "delete" {
                saw_delete = true;
                assert_eq!(fs::read(target.join("neu")).unwrap(), b"neu");
            }
        })
        .unwrap();
    assert!(saw_delete);
    assert_eq!(stats.files_deleted, 3);
    assert!(!target.join("alt").exists());
    assert_eq!(fs::read(source.join("neu")).unwrap(), b"neu");
}
#[test]
fn content_sync_mirror_neue_quelle_und_zielmitglieder_blockieren_entfernungen() {
    for source_drift in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        fs::create_dir(&source).unwrap();
        fs::create_dir_all(target.join("alt")).unwrap();
        fs::write(source.join("neu"), vec![5u8; 200000]).unwrap();
        fs::write(target.join("alt/erhalten"), b"alt").unwrap();
        let (s, t) = endpoints(&source, &target);
        let prepared =
            prepare_content_sync(s, t, options(SyncMode::Mirror), &Cancel::new()).unwrap();
        let digest = prepared.preview().digest;
        let mut changed = false;
        let result = prepared.confirm_effects(digest, true).unwrap().execute(
            &Cancel::new(),
            &mut |progress| {
                if progress.bytes_done > 0 && !changed {
                    fs::write(
                        if source_drift {
                            source.join("neues_quellmitglied")
                        } else {
                            target.join("alt/fremd")
                        },
                        b"fremd",
                    )
                    .unwrap();
                    changed = true;
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read(target.join("alt/erhalten")).unwrap(), b"alt");
        if !source_drift {
            assert_eq!(fs::read(target.join("alt/fremd")).unwrap(), b"fremd");
        }
    }
}
#[test]
fn content_sync_mirror_auslassungen_sperren_nur_entfernungen() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("quelle");
    let target = temp.path().join("ziel");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("neu"), b"neu").unwrap();
    symlink("neu", source.join("link")).unwrap();
    fs::write(target.join("alt"), b"alt").unwrap();
    let (s, t) = endpoints(&source, &target);
    let prepared = prepare_content_sync(s, t, options(SyncMode::Mirror), &Cancel::new()).unwrap();
    assert_eq!(prepared.preview().blocked_removals, 1);
    assert_eq!(prepared.preview().irreversible_removals, 0);
    let stats = run(prepared);
    assert_eq!(stats.files_deleted, 0);
    assert_eq!(fs::read(target.join("alt")).unwrap(), b"alt");
    assert_eq!(fs::read(target.join("neu")).unwrap(), b"neu");
}
#[test]
fn content_sync_mirror_grosse_entfernung_braucht_exakte_phrase() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("quelle");
    let target = temp.path().join("ziel");
    fs::create_dir(&source).unwrap();
    fs::create_dir_all(target.join("alt")).unwrap();
    for i in 0..1001 {
        fs::write(target.join(format!("alt/{i}")), b"").unwrap();
    }
    let (s, t) = endpoints(&source, &target);
    let prepared = prepare_content_sync(s, t, options(SyncMode::Mirror), &Cancel::new()).unwrap();
    assert!(prepared.preview().removal_confirmation.is_some());
    let digest = prepared.preview().digest;
    assert!(prepared.confirm_effects(digest, true).is_err());
    assert!(target.join("alt/0").exists());
}

#[test]
fn content_sync_lokales_ziel_bevorzugt_echte_quarantaene_fuer_update_und_mirror() {
    for mode in [SyncMode::Update, SyncMode::Mirror] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        fs::create_dir(&source).unwrap();
        fs::create_dir_all(target.join("alt/unter")).unwrap();
        fs::write(source.join("datei"), b"neue Version").unwrap();
        fs::write(target.join("datei"), b"alte Version").unwrap();
        fs::write(target.join("alt/unter/extra"), b"extra").unwrap();
        let prepared = prepare_content_sync(
            ProviderEndpoint::local(&source).unwrap(),
            ProviderEndpoint::local(&target).unwrap(),
            options(mode),
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(prepared.preview().quarantined_replacements, 1);
        assert_eq!(prepared.preview().irreversible_replacements, 0);
        assert!(prepared.preview().restore_probe_ok);
        if mode == SyncMode::Mirror {
            assert_eq!(prepared.preview().quarantined_removals, 3);
            assert_eq!(prepared.preview().irreversible_removals, 0);
        }
        let run_path = PathBuf::from(prepared.preview().quarantine_run.as_ref().unwrap());
        let stats = run(prepared);
        assert_eq!(stats.irreversible_replacements, 0);
        assert_eq!(stats.quarantine.as_deref(), run_path.to_str());
        assert_eq!(fs::read(target.join("datei")).unwrap(), b"neue Version");
        assert_eq!(fs::read(run_path.join("datei")).unwrap(), b"alte Version");
        if mode == SyncMode::Mirror {
            assert_eq!(
                fs::read(run_path.join("alt/unter/extra")).unwrap(),
                b"extra"
            );
            assert!(!target.join("alt").exists());
        }
    }
}

#[test]
fn content_sync_lokale_schwaeche_bleibt_ausdruecklich_dauerhaft() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("quelle");
    let target = temp.path().join("ziel");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("datei"), b"neu").unwrap();
    fs::write(target.join("datei"), b"alt").unwrap();
    crate::local_fs::with_weak_file_capabilities_for_test(
        &fs::canonicalize(&target).unwrap(),
        || {
            let prepared = prepare_content_sync(
                ProviderEndpoint::local(&source).unwrap(),
                ProviderEndpoint::local(&target).unwrap(),
                options(SyncMode::Update),
                &Cancel::new(),
            )
            .unwrap();
            assert_eq!(prepared.preview().quarantined_replacements, 0);
            assert_eq!(prepared.preview().irreversible_replacements, 1);
            assert!(!prepared.preview().restore_probe_ok);
            let stats = run(prepared);
            assert_eq!(stats.irreversible_replacements, 1);
            assert_eq!(stats.quarantine, None);
            assert_eq!(fs::read(target.join("datei")).unwrap(), b"neu");
        },
    );
}

fn local_queue_operation(
    source: &Path,
    target: &Path,
    prepared: &PreparedContentSync,
) -> QueueOperation {
    QueueOperation::ContentSync {
        source: Box::new(BidirectionalQueueEndpoint::Local {
            root: source.to_string_lossy().into_owned(),
        }),
        target: Box::new(BidirectionalQueueEndpoint::Local {
            root: target.to_string_lossy().into_owned(),
        }),
        options: prepared.preview().options.clone(),
        expected_plan: Box::new(prepared.preview().plan.clone()),
        binding_digest: prepared.preview().digest,
    }
}
#[test]
fn content_sync_altes_journal_erfindet_keine_neue_klasse_und_running_bleibt_unklar() {
    for old_schema in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("a"), b"a").unwrap();
        let prepared = prepare_content_sync(
            ProviderEndpoint::local(&source).unwrap(),
            ProviderEndpoint::local(&target).unwrap(),
            options(SyncMode::AddOnly),
            &Cancel::new(),
        )
        .unwrap();
        let journal = root.join("state/operations-queue.json");
        private_queue_parent(&journal);
        let queue = OperationQueue::open(&journal).unwrap();
        let entry = queue
            .enqueue(local_queue_operation(&source, &target, &prepared))
            .unwrap();
        if old_schema {
            let mut snapshot = queue.snapshot();
            snapshot.version = 32;
            drop(queue);
            let bytes = serde_json::to_vec(&snapshot).unwrap();
            fs::write(&journal, &bytes).unwrap();
            assert!(OperationQueue::open(&journal).is_err());
            assert_eq!(fs::read(&journal).unwrap(), bytes);
        } else {
            queue.claim_next().unwrap().unwrap();
            queue.mark_running(&entry.operation_id).unwrap();
            drop(queue);
            private_queue_parent(&journal);
            let queue = OperationQueue::open(&journal).unwrap();
            let recovered = queue.snapshot().entries.remove(0);
            assert_eq!(recovered.state, QueueState::Interrupted);
            assert_eq!(recovered.error_code, Some(crate::ErrorCode::OutcomeUnclear));
            assert!(queue.retry(&entry.operation_id).is_err());
            assert!(!target.join("a").exists());
        }
    }
}
#[test]
fn content_sync_reopen_zeigt_auch_bei_gleichem_plan_neue_wirkungsbestaetigung() {
    struct NoSecrets;
    impl crate::SecretResolver for NoSecrets {
        fn resolve(&self, _: &crate::KeychainSecretReference) -> io::Result<crate::SecretString> {
            panic!("Lokaler Reopen benötigt keine Secrets")
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("quelle");
    let target = root.join("ziel");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("a"), b"a").unwrap();
    let prepared = prepare_content_sync(
        ProviderEndpoint::local(&source).unwrap(),
        ProviderEndpoint::local(&target).unwrap(),
        options(SyncMode::AddOnly),
        &Cancel::new(),
    )
    .unwrap();
    let journal = root.join("state/operations-queue.json");
    private_queue_parent(&journal);
    let queue = OperationQueue::open(&journal).unwrap();
    queue
        .enqueue(local_queue_operation(&source, &target, &prepared))
        .unwrap();
    drop(prepared);
    drop(queue);
    private_queue_parent(&journal);
    let queue = OperationQueue::open(&journal).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let prepared = crate::prepare_content_sync_queue_entry(
        &queue,
        &entry,
        Arc::new(NoSecrets),
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(
        queue.snapshot().entries[0].state,
        QueueState::AwaitingConfirmation
    );
    assert!(!target.join("a").exists());
    queue
        .confirm_content_sync_preparation(&entry.operation_id, &prepared)
        .unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    crate::execute_prepared_content_sync_queue_entry(
        &queue,
        &entry,
        prepared,
        true,
        &root.join("audit.jsonl"),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(fs::read(target.join("a")).unwrap(), b"a");
}
#[test]
fn content_sync_progress_persistence_failure_keeps_both_terminal_audits() {
    assert_content_sync_progress_audits(false);
}
#[test]
fn content_sync_progress_failure_keeps_primary_when_both_terminal_audits_fail() {
    assert_content_sync_progress_audits(true);
}
fn assert_content_sync_progress_audits(block_audits: bool) {
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("a"), b"independent-content").unwrap();
    let journal = root.join("state/operations-queue.json");
    private_queue_parent(&journal);
    let queue = OperationQueue::open(&journal).unwrap();
    let cancel = Cancel::new();
    let prepared = super::prepare_content_sync_for_queue(
        &queue,
        ProviderEndpoint::local(&source).unwrap(),
        ProviderEndpoint::local(&target).unwrap(),
        options(SyncMode::AddOnly),
        &cancel,
    )
    .unwrap();
    queue.enqueue(prepared.queue_operation()).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    queue.fail_nonterminal_progress_update_after_for_test(0);
    let audit = root.join("content-audit.jsonl");
    let mut blocked = false;
    let error = crate::execute_prepared_content_sync_queue_entry(
        &queue,
        &entry,
        prepared,
        true,
        &audit,
        &cancel,
        &mut |_| {
            if block_audits && !blocked {
                blocked = true;
                for path in [root.join("state/operations-audit.jsonl"), audit.clone()] {
                    fs::rename(&path, path.with_extension("started.jsonl")).unwrap();
                    fs::create_dir(&path).unwrap();
                }
            }
        },
    )
    .unwrap_err();
    assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
    if block_audits {
        let message = error.to_string();
        assert!(message.contains("Testfehler beim nichtterminalen Fortschritts-Commit"));
        assert!(message.contains("Queue-Audits"));
        assert!(message.contains("Inhaltssync-Audits"));
        assert_eq!(fs::read(source.join("a")).unwrap(), b"independent-content");
        return;
    }
    for path in [root.join("state/operations-audit.jsonl"), audit.clone()] {
        let records = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            records
                .iter()
                .filter(|record| record["operation_id"] == entry.operation_id
                    && record["status"] == "failed")
                .count(),
            1,
            "Terminales Fehleraudit fehlt: {}",
            path.display()
        );
    }
    let records = fs::read_to_string(audit)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let failure = records.last().unwrap();
    assert_eq!(failure["error_code"], "outcome_unclear");
    assert!(failure["metadata_report"].is_null());
    assert_eq!(fs::read(source.join("a")).unwrap(), b"independent-content");
}

#[test]
fn content_sync_audit_symlink_stoppt_vor_wirkung() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("quelle");
    let target = root.join("ziel");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("a"), b"a").unwrap();
    private_queue_parent(&root.join("state/operations-queue.json"));
    let queue = OperationQueue::open(root.join("state/operations-queue.json")).unwrap();
    let prepared = super::prepare_content_sync_for_queue(
        &queue,
        ProviderEndpoint::local(&source).unwrap(),
        ProviderEndpoint::local(&target).unwrap(),
        options(SyncMode::AddOnly),
        &Cancel::new(),
    )
    .unwrap();
    queue
        .enqueue(local_queue_operation(&source, &target, &prepared))
        .unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let victim = root.join("victim");
    fs::write(&victim, b"erhalten").unwrap();
    let audit = root.join("audit.jsonl");
    symlink(&victim, &audit).unwrap();
    assert!(crate::execute_prepared_content_sync_queue_entry(
        &queue,
        &entry,
        prepared,
        true,
        &audit,
        &Cancel::new(),
        &mut |_| {}
    )
    .is_err());
    assert!(!target.join("a").exists());
    assert_eq!(fs::read(victim).unwrap(), b"erhalten");
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Failed);
}
#[test]
fn content_sync_geschuetzter_ziel_elternordner_sperrt_additionen_und_bewusste_dauerwirkung_nicht() {
    use std::os::unix::fs::PermissionsExt;
    for mode in [SyncMode::AddOnly, SyncMode::Update, SyncMode::Mirror] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("quelle");
        let parent = root.join("locked");
        let target = parent.join("ziel");
        fs::create_dir(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(source.join("datei"), b"neu").unwrap();
        if mode != SyncMode::AddOnly {
            fs::write(target.join("datei"), b"alt").unwrap();
        }
        if mode == SyncMode::Mirror {
            fs::write(target.join("extra"), b"extra").unwrap();
        }
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
        let outcome = (|| {
            let prepared = prepare_content_sync(
                ProviderEndpoint::local(&source)?,
                ProviderEndpoint::local(&target)?,
                options(mode),
                &Cancel::new(),
            )?;
            assert_eq!(prepared.preview().quarantined_replacements, 0);
            if mode != SyncMode::AddOnly {
                assert_eq!(prepared.preview().irreversible_replacements, 1);
                assert!(prepared
                    .preview()
                    .limitations
                    .iter()
                    .any(|reason| reason.contains("Quarantäne ist")));
            }
            let digest = prepared.preview().digest;
            prepared
                .confirm_effects(digest, true)?
                .execute(&Cancel::new(), &mut |_| {})
        })();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        outcome.unwrap();
        assert_eq!(fs::read(target.join("datei")).unwrap(), b"neu");
        assert!(!parent.join(".doppeldecker-quarantine").exists());
        if mode == SyncMode::Mirror {
            assert!(!target.join("extra").exists());
        }
    }
}

fn private_queue_parent(journal: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let parent = journal.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn content_sync_quarantaenefehler_auditiert_nur_bestaetigte_entfernungen() {
    for completed in [0, 1] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        for name in ["a", "b", "c", "d"] {
            fs::write(source.join(name), name.as_bytes()).unwrap();
        }
        for name in ["extra-a", "extra-z"] {
            fs::write(target.join(name), b"extra").unwrap();
        }
        let journal = root.join("state/operations-queue.json");
        private_queue_parent(&journal);
        let queue = OperationQueue::open(journal).unwrap();
        let prepared = super::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::local(&source).unwrap(),
            ProviderEndpoint::local(&target).unwrap(),
            options(SyncMode::Mirror),
            &Cancel::new(),
        )
        .unwrap();
        let quarantine = PathBuf::from(prepared.preview().quarantine_run.clone().unwrap());
        queue
            .enqueue(local_queue_operation(&source, &target, &prepared))
            .unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        let audit = root.join("audit.jsonl");
        let changed = if completed == 0 { "extra-a" } else { "extra-z" };
        let mut drifted = false;
        let result = crate::execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &audit,
            &Cancel::new(),
            &mut |progress| {
                if !drifted && progress.phase == "quarantine" && progress.current == changed {
                    fs::write(target.join(changed), b"zwischenzeitlich veraendert").unwrap();
                    drifted = true;
                }
            },
        );
        assert!(result.is_err());
        assert!(drifted);
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Failed);
        for name in ["a", "b", "c", "d"] {
            assert_eq!(fs::read(target.join(name)).unwrap(), name.as_bytes());
        }
        assert_eq!(quarantine.join("extra-a").exists(), completed == 1);
        assert!(target.join(changed).exists());
        let records: Vec<serde_json::Value> = fs::read_to_string(audit)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.last().unwrap()["status"], "failed");
        assert_eq!(records.last().unwrap()["files_deleted"], completed);
    }
}

#[test]
fn content_sync_foreign_source_aba_stops_before_next_file() {
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    for name in ["a", "b"] {
        fs::write(source.join(name), name).unwrap();
    }
    let queue = crate::OperationQueue::open(root.join("config/operations-queue.json")).unwrap();
    let prepared = super::prepare_content_sync_for_queue(
        &queue,
        ProviderEndpoint::local(&source).unwrap(),
        ProviderEndpoint::local(&target).unwrap(),
        options(SyncMode::AddOnly),
        &Cancel::new(),
    )
    .unwrap();
    queue.enqueue(prepared.queue_operation()).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let prepared = crate::queue::bind_prepared_content_sync_queue_entry(
        &queue,
        &entry,
        prepared,
        &Cancel::new(),
    )
    .unwrap();
    let (other_owner, _, _) = queue.ftps_owner(None).unwrap();
    let digest = prepared.preview().digest;
    let mut injected = false;
    let error = prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |progress| {
            if progress.files_done == 1 && !injected {
                injected = true;
                let record = crate::local_staging::NeighborStaging::create(
                    &other_owner,
                    &source.join("transient"),
                    None,
                )
                .unwrap()
                .unwrap();
                let candidate = source.join(".foreign-transient");
                record.candidate(&candidate).unwrap();
                fs::write(&candidate, b"transient data").unwrap();
                fs::remove_file(&candidate).unwrap();
                record.finish().unwrap();
            }
        })
        .unwrap_err();
    assert!(injected);
    assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
    assert_eq!(fs::read(target.join("a")).unwrap(), b"a");
    assert!(!target.join("b").exists());
    assert!(!source.join(".foreign-transient").exists());
}

#[test]
fn content_sync_direct_local_prepare_stops_before_unowned_probe() {
    let area = tempfile::tempdir().unwrap();
    let source = area.path().join("source");
    let target = area.path().join("target");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("file"), b"new").unwrap();
    fs::write(target.join("file"), b"old").unwrap();
    let error = super::prepare_content_sync(
        ProviderEndpoint::local(&source).unwrap(),
        ProviderEndpoint::local(&target).unwrap(),
        options(SyncMode::Update),
        &Cancel::new(),
    )
    .err()
    .expect("Keine ownerlose Restoreprobe");
    assert_eq!(crate::error_code(&error), crate::ErrorCode::Unsupported);
    assert!(!area.path().join(".doppeldecker-quarantine").exists());
    assert_eq!(fs::read(target.join("file")).unwrap(), b"old");
}

#[test]
fn content_sync_preparation_cannot_change_window_queue() {
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("a"), b"expected").unwrap();
    let first = OperationQueue::open(root.join("config/operations-queue-a.json")).unwrap();
    let (_, lock, _) = first.ftps_owner(None).unwrap();
    let second =
        OperationQueue::open_with_process_lock(root.join("config/operations-queue-b.json"), lock)
            .unwrap();
    let prepared = super::prepare_content_sync_for_queue(
        &first,
        ProviderEndpoint::local(&source).unwrap(),
        ProviderEndpoint::local(&target).unwrap(),
        options(SyncMode::AddOnly),
        &Cancel::new(),
    )
    .unwrap();
    second.enqueue(prepared.queue_operation()).unwrap();
    let entry = second.claim_next().unwrap().unwrap();
    let error = match crate::queue::bind_prepared_content_sync_queue_entry(
        &second,
        &entry,
        prepared,
        &Cancel::new(),
    ) {
        Ok(_) => panic!("Andere Fensterqueue darf Vorbereitung nicht übernehmen"),
        Err(error) => error,
    };
    assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
    assert_eq!(second.snapshot().entries[0].state, QueueState::Validating);
    assert!(!target.join("a").exists());
}

#[test]
fn content_sync_probe_rebind_rejects_foreign_aba_at_both_snapshot_boundaries() {
    for point in [
        StagingRebindPoint::AfterGeneralSnapshot,
        StagingRebindPoint::AfterForeignCheck,
    ] {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let source = root.join("source");
        let target = root.join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("file"), b"source content").unwrap();
        let queue = OperationQueue::open(root.join("config/operations-queue.json")).unwrap();
        let (foreign_owner, _, _) = queue.ftps_owner(None).unwrap();
        let injected = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed = injected.clone();
        let foreign_path = target.join("foreign");
        STAGING_REBIND_HOOK.with(|slot| {
            *slot.borrow_mut() = Some((
                point,
                Box::new(move || {
                    // Ein vollständig abgeschlossener fremder Transfer hinterlässt
                    // keinen Kandidaten. Nur sein echter Registryverlauf belegt ABA.
                    let record = crate::local_staging::NeighborStaging::create(
                        &foreign_owner,
                        &foreign_path,
                        None,
                    )
                    .unwrap()
                    .unwrap();
                    record.finish().unwrap();
                    observed.set(true);
                }),
            ));
        });
        let result = super::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::local(&source).unwrap(),
            ProviderEndpoint::local(&target).unwrap(),
            options(SyncMode::AddOnly),
            &Cancel::new(),
        );
        let unused_hook = STAGING_REBIND_HOOK.with(|slot| slot.borrow_mut().take());
        assert!(unused_hook.is_none());
        assert!(injected.get());
        let error = match result {
            Ok(_) => panic!("Fremder ABA-Zyklus muss die Vorbereitung verwerfen"),
            Err(error) => error,
        };
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert!(error.to_string().contains(match point {
            StagingRebindPoint::AfterGeneralSnapshot => {
                "Fremde Zwischenstände änderten sich während der Inhaltssync-Vorbereitung"
            }
            StagingRebindPoint::AfterForeignCheck => {
                "Bekannte fremde Zwischenstände änderten sich seit der Inhaltssync-Planung"
            }
        }));
        assert_eq!(fs::read(source.join("file")).unwrap(), b"source content");
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    }
}

fn directory_dacl_completion_fixture() -> (
    tempfile::TempDir,
    ContentStaging,
    crate::local_staging::StagingOwner,
    PathBuf,
) {
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    let queue = OperationQueue::open(root.join("config/operations-queue.json")).unwrap();
    let (owner, _, _) = queue.ftps_owner(None).unwrap();
    let registry = crate::staging_guard::KnownStagingRegistry::for_queue(&queue).unwrap();
    let endpoints = [source, target.clone()]
        .into_iter()
        .map(|root| {
            let vfs: Arc<dyn Vfs> = Arc::new(crate::LocalVfs::rooted(&root).unwrap());
            let binding =
                crate::EndpointBinding::local(&vfs.root_identity(&root).unwrap()).unwrap();
            crate::staging_guard::StagingEndpoint { vfs, root, binding }
        })
        .collect::<Vec<_>>();
    let epoch = registry.check(&endpoints, Some(&owner)).unwrap();
    (
        area,
        ContentStaging {
            registry,
            endpoints,
            epoch,
        },
        owner,
        target,
    )
}

#[test]
fn content_sync_directory_dacl_completion_adopts_only_checked_own_history() {
    let (_area, mut staging, owner, target) = directory_dacl_completion_fixture();
    let before = staging.registry.check(&staging.endpoints, None).unwrap();
    let own =
        crate::local_staging::NeighborStaging::create(&owner, &target.join("own-transient"), None)
            .unwrap()
            .unwrap();
    own.finish().unwrap();
    assert_ne!(
        staging.registry.check(&staging.endpoints, None).unwrap(),
        before
    );
    staging.complete_directory_dacl(Some(&owner)).unwrap();
    staging.validate(None).unwrap();
    assert_eq!(fs::read_dir(target).unwrap().count(), 0);
}

#[test]
fn content_sync_directory_dacl_completion_rejects_foreign_aba_at_both_boundaries() {
    for point in [
        StagingRebindPoint::AfterGeneralSnapshot,
        StagingRebindPoint::AfterForeignCheck,
    ] {
        let (_area, mut staging, owner, target) = directory_dacl_completion_fixture();
        let mut foreign = owner.clone();
        foreign.operation = "e".repeat(32);
        assert_ne!(foreign.operation, owner.operation);
        let foreign_path = target.join("foreign-transient");
        let injected = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed = injected.clone();
        STAGING_REBIND_HOOK.with(|slot| {
            *slot.borrow_mut() = Some((
                point,
                Box::new(move || {
                    let record = crate::local_staging::NeighborStaging::create(
                        &foreign,
                        &foreign_path,
                        None,
                    )
                    .unwrap()
                    .unwrap();
                    record.finish().unwrap();
                    observed.set(true);
                }),
            ))
        });
        let error = staging.complete_directory_dacl(Some(&owner)).unwrap_err();
        assert!(injected.get());
        assert!(STAGING_REBIND_HOOK
            .with(|slot| slot.borrow_mut().take())
            .is_none());
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(fs::read_dir(target).unwrap().count(), 0);
    }
}

#[test]
fn typed_mtime_omission_preserves_privilege_report_and_counts_once() {
    use crate::sync::{MetadataClass, MetadataClassStats, MetadataIssueKind, MetadataReport};
    let mut report = MetadataReport::default();
    report.classes.push(MetadataClassStats {
        class: MetadataClass::Mtime,
        transferred: 2,
        omitted: 0,
    });
    let error = mtime_privilege_omission("Konkrete OPEN-Absage");
    let omission = mtime_omission_reason(&error).unwrap().clone();
    record_mtime_omission(&mut report, "parent", omission.clone());
    record_mtime_omission(&mut report, "parent", omission);
    let unsupported = mtime_omission("Protokollgrenze");
    record_mtime_omission(
        &mut report,
        "other",
        mtime_omission_reason(&unsupported).unwrap().clone(),
    );
    assert_eq!(report.privilege_failures, 1);
    assert_eq!(report.unsupported_metadata, 1);
    assert_eq!(
        (report.classes[0].transferred, report.classes[0].omitted),
        (0, 2)
    );
    assert_eq!(report.issues.len(), 2);
    assert_eq!(report.issues[0].kind, MetadataIssueKind::PrivilegeFailure);
    assert_eq!(report.issues[0].reason, "Konkrete OPEN-Absage");
    assert_eq!(report.issues[1].kind, MetadataIssueKind::Unsupported);
    assert!(mtime_omission_reason(&io::Error::new(
        io::ErrorKind::PermissionDenied,
        "nur Fehlertext"
    ))
    .is_none());
}

#[test]
fn mtime_omission_policy_keeps_native_strict_and_portable_privileges_strict() {
    let privilege = mtime_privilege_omission("OPEN ACCESS_DENIED");
    let unsupported = mtime_omission("Protokollgrenze");
    for profile in [
        MetadataProfile::Portable,
        MetadataProfile::NativeBestEffort,
        MetadataProfile::NativeStrict,
    ] {
        assert_eq!(
            mtime_omission_reason(&privilege)
                .unwrap()
                .allowed_in_profile(profile),
            profile == MetadataProfile::NativeBestEffort
        );
        assert_eq!(
            mtime_omission_reason(&unsupported)
                .unwrap()
                .allowed_in_profile(profile),
            profile != MetadataProfile::NativeStrict
        );
    }
}

#[test]
fn content_sync_audit_preserves_actual_metadata_report_and_absent_stats_stay_null() {
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    let (src, dst) = endpoints(&source, &target);
    let prepared =
        prepare_content_sync(src, dst, options(SyncMode::AddOnly), &Cancel::new()).unwrap();
    let audit = root.join("audit.jsonl");
    let mut stats = SyncStats::default();
    let error = mtime_privilege_omission("Konkrete OPEN-Absage vor SET");
    record_mtime_omission(
        &mut stats.metadata_report,
        "parent",
        mtime_omission_reason(&error).unwrap().clone(),
    );
    append_audit(
        &audit,
        "test",
        "completed",
        prepared.preview(),
        &Progress::default(),
        Some(&stats),
        None,
        0,
    )
    .unwrap();
    append_audit(
        &audit,
        "test",
        "failed",
        prepared.preview(),
        &Progress::default(),
        None,
        Some(&error),
        0,
    )
    .unwrap();
    let records = fs::read_to_string(&audit)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let report: crate::sync::MetadataReport =
        serde_json::from_value(records[0]["metadata_report"].clone()).unwrap();
    assert_eq!(report, stats.metadata_report);
    assert_eq!(report.privilege_failures, 1);
    assert_eq!(report.issues[0].path, "parent");
    assert_eq!(
        report.issues[0].kind,
        crate::sync::MetadataIssueKind::PrivilegeFailure
    );
    assert!(records[1]["metadata_report"].is_null());
}

fn windows_annotation_source(owner_present: bool, observation: bool) -> crate::MetadataManifest {
    use crate::metadata::{Acl, WindowsSecurityObservation};
    use crate::windows_security::{WindowsAce, WindowsDacl, WindowsDaclState, WindowsSid};
    let owner = WindowsSid {
        revision: 1,
        authority: 5,
        sub_authorities: vec![21, 1, 2, 3, 1000],
    };
    let acl = WindowsDacl {
        namespace: [7; 16],
        control: 0x1004,
        state: WindowsDaclState::Acl {
            revision: 2,
            aces: vec![WindowsAce::Allow {
                flags: 0,
                mask: 0x0017_01ff,
                sid: owner.clone(),
            }],
        },
    };
    crate::MetadataManifest::new(
        None,
        0o644,
        Some(Acl::WindowsDacl(acl)),
        vec![],
        None,
        None,
        None,
    )
    .unwrap()
    .with_windows_security(observation.then(|| WindowsSecurityObservation {
        namespace: [7; 16],
        owner: owner_present.then_some(owner),
        group: None,
        issue: None,
        invalid_descriptor_digest: None,
    }))
    .unwrap()
    .with_named_streams(Some(vec![]))
    .unwrap()
}

fn windows_annotation_plan(source: Option<crate::MetadataManifest>) -> SyncPlan {
    let has_acl = source.as_ref().is_some_and(|source| source.acl.is_some());
    let metadata = source.map(|source| crate::sync::MetadataTransfer {
        source,
        target: None,
        owner: false,
        acl: has_acl,
        incompatible_acl_model: false,
        windows_dacl: None,
        normal_mode: false,
        special_mode_bits: false,
        xattrs: false,
        resource_fork: false,
        named_streams: false,
        directory_streams: None,
        sparse_extents: false,
        hardlinks: false,
    });
    SyncPlan {
        actions: vec![crate::Action::CopyFile {
            path: "file.bin".into(),
            size: 1,
            source_mtime: Some(1_700_000_000),
            source_mode: Some(0o644),
            source_hash: None,
            metadata,
        }],
        comparison: crate::sync::SyncComparison {
            rows: vec![crate::sync::ComparisonRow {
                path: "file.bin".into(),
                depth: 0,
                ancestors: vec![],
                action: crate::sync::ComparisonAction::Add,
                affected_side: crate::sync::ComparisonSide::Target,
                source: Some(crate::sync::ComparisonEntry {
                    kind: crate::sync::ComparisonEntryKind::File,
                    size: 1,
                    mtime: Some(1_700_000_000),
                    created: None,
                    owner: None,
                    group: None,
                    extension: None,
                    mode: Some(0o644),
                }),
                target: None,
            }],
            ..Default::default()
        },
        metadata_report: crate::sync::MetadataReport {
            profile: MetadataProfile::NativeBestEffort,
            classes: vec![crate::sync::MetadataClassStats {
                class: crate::sync::MetadataClass::Acl,
                transferred: u64::from(has_acl),
                omitted: 0,
            }],
            ..Default::default()
        },
        ..Default::default()
    }
}
fn windows_annotation_capabilities() -> crate::metadata::MetadataCapabilities {
    use crate::metadata::{AclModel, Capability, MetadataCapabilities};
    MetadataCapabilities {
        owner: Capability::ReadOnly,
        special_mode_bits: Capability::Exact,
        acl_model: AclModel::WindowsSid,
        acl: Capability::ReadOnly,
        xattrs: Capability::Exact,
        resource_fork: Capability::Exact,
        named_streams: Capability::Exact,
        sparse_extents: Capability::Exact,
        hardlinks: Capability::Exact,
    }
}
fn annotate_windows_plan(plan: &mut SyncPlan) -> io::Result<()> {
    annotate_unreadable_metadata_with_capabilities(
        plan,
        &windows_annotation_capabilities(),
        ProviderKind::Smb,
        &SyncOptions {
            metadata: MetadataProfile::NativeBestEffort,
            ..options(SyncMode::AddOnly)
        },
    )
}

#[test]
fn windows_acl_annotation_preserves_exact_object_read_evidence() {
    for (owner_present, observation) in [(true, true), (false, true), (false, false)] {
        let mut plan =
            windows_annotation_plan(Some(windows_annotation_source(owner_present, observation)));
        annotate_windows_plan(&mut plan).unwrap();
        let acl = plan
            .metadata_report
            .classes
            .iter()
            .find(|class| class.class == crate::sync::MetadataClass::Acl)
            .unwrap();
        assert_eq!((acl.transferred, acl.omitted), (1, 0));
        assert!(!plan
            .metadata_report
            .issues
            .iter()
            .any(|issue| issue.class == crate::sync::MetadataClass::Acl));
        assert!(plan
            .metadata_report
            .issues
            .iter()
            .any(|issue| issue.class == crate::sync::MetadataClass::Owner));
    }
}

fn windows_annotation_omit_acl(plan: &mut SyncPlan, issue: crate::sync::MetadataIssue) {
    let crate::Action::CopyFile {
        metadata: Some(metadata),
        ..
    } = &mut plan.actions[0]
    else {
        panic!()
    };
    metadata.acl = false;
    let count = plan
        .metadata_report
        .classes
        .iter_mut()
        .find(|count| count.class == crate::sync::MetadataClass::Acl)
        .unwrap();
    count.transferred = 0;
    count.omitted = 1;
    match issue.kind {
        crate::sync::MetadataIssueKind::PrivilegeFailure => {
            plan.metadata_report.privilege_failures += 1
        }
        crate::sync::MetadataIssueKind::IncompatibleAclModel => {
            plan.metadata_report.incompatible_acl_models += 1
        }
        crate::sync::MetadataIssueKind::Unsupported => {
            plan.metadata_report.unsupported_metadata += 1
        }
    }
    plan.metadata_report.issues.push(issue);
}

#[test]
fn windows_acl_annotation_preserves_precise_source_read_failures() {
    use crate::metadata::{WindowsSecurityObservation, WindowsSecurityReadIssue};
    use crate::sync::{MetadataClass, MetadataClassStats, MetadataIssue, MetadataIssueKind};
    for issue in [
        WindowsSecurityReadIssue::ReadControlDenied,
        WindowsSecurityReadIssue::QueryUnsupported,
        WindowsSecurityReadIssue::InvalidDescriptor,
    ] {
        let source = crate::MetadataManifest::new(None, 0o644, None, vec![], None, None, None)
            .unwrap()
            .with_windows_security(Some(WindowsSecurityObservation {
                namespace: [7; 16],
                owner: None,
                group: None,
                issue: Some(issue),
                invalid_descriptor_digest: (issue == WindowsSecurityReadIssue::InvalidDescriptor)
                    .then_some([9; 32]),
            }))
            .unwrap()
            .with_named_streams(Some(vec![]))
            .unwrap();
        let mut plan = windows_annotation_plan(Some(source));
        let kind = if issue == WindowsSecurityReadIssue::ReadControlDenied {
            MetadataIssueKind::PrivilegeFailure
        } else {
            MetadataIssueKind::Unsupported
        };
        windows_annotation_omit_acl(
            &mut plan,
            MetadataIssue {
                path: "file.bin".into(),
                class: MetadataClass::Acl,
                kind,
                reason: issue.reason().into(),
            },
        );
        plan.metadata_report.classes.push(MetadataClassStats {
            class: MetadataClass::Owner,
            transferred: 0,
            omitted: 1,
        });
        plan.metadata_report.issues.push(MetadataIssue {
            path: "file.bin".into(),
            class: MetadataClass::Owner,
            kind,
            reason: format!(
                "Windows-Owner-/Group-SIDs wurden nicht beobachtet: {}",
                issue.reason()
            ),
        });
        if kind == MetadataIssueKind::PrivilegeFailure {
            plan.metadata_report.privilege_failures += 1;
        } else {
            plan.metadata_report.unsupported_metadata += 1;
        }
        let before = plan.metadata_report.clone();
        annotate_windows_plan(&mut plan).unwrap();
        assert_eq!(plan.metadata_report, before);
    }
}

#[test]
fn windows_acl_annotation_keeps_legitimate_writer_and_format_omissions() {
    use crate::sync::{MetadataClass, MetadataIssue, MetadataIssueKind};
    use crate::windows_dacl::{WindowsDaclOmission, WindowsDaclTargetIssue, WindowsDaclTransfer};
    use crate::windows_security::WindowsDaclWriteIssue;
    for omission in [
        WindowsDaclOmission::Target(WindowsDaclTargetIssue::RequiredAccessDenied),
        WindowsDaclOmission::Target(WindowsDaclTargetIssue::MissingOwner),
        WindowsDaclOmission::Source(WindowsDaclWriteIssue::UnsupportedControl),
    ] {
        let mut source = windows_annotation_source(false, true);
        if matches!(omission, WindowsDaclOmission::Source(_)) {
            let Some(crate::metadata::Acl::WindowsDacl(mut acl)) = source.acl.clone() else {
                panic!()
            };
            acl.control = 0x0004;
            source = crate::MetadataManifest::new(
                None,
                source.mode,
                Some(crate::metadata::Acl::WindowsDacl(acl)),
                vec![],
                None,
                None,
                None,
            )
            .unwrap()
            .with_windows_security(source.windows_security.clone())
            .unwrap()
            .with_named_streams(Some(vec![]))
            .unwrap();
        }
        let mut plan = windows_annotation_plan(Some(source));
        let kind = if matches!(
            omission,
            WindowsDaclOmission::Target(WindowsDaclTargetIssue::RequiredAccessDenied)
        ) {
            MetadataIssueKind::PrivilegeFailure
        } else {
            MetadataIssueKind::Unsupported
        };
        let issue = MetadataIssue {
            path: "file.bin".into(),
            class: MetadataClass::Acl,
            kind,
            reason: omission.reason().into(),
        };
        windows_annotation_omit_acl(&mut plan, issue.clone());
        let crate::Action::CopyFile {
            metadata: Some(metadata),
            ..
        } = &mut plan.actions[0]
        else {
            panic!()
        };
        metadata.windows_dacl = Some(WindowsDaclTransfer::Omitted(omission));
        let actions = plan.actions.clone();
        annotate_windows_plan(&mut plan).unwrap();
        assert_eq!(
            plan.actions, actions,
            "Lesebeobachtung darf keine Schreibfreigabe erfinden"
        );
        assert_eq!(
            plan.metadata_report
                .issues
                .iter()
                .filter(|value| value.class == MetadataClass::Acl)
                .collect::<Vec<_>>(),
            vec![&issue]
        );
        let count = plan
            .metadata_report
            .classes
            .iter()
            .find(|count| count.class == MetadataClass::Acl)
            .unwrap();
        assert_eq!((count.transferred, count.omitted), (0, 1));
    }
}

#[test]
fn windows_acl_annotation_requires_object_evidence_and_preserves_other_root_models() {
    use crate::metadata::{AclModel, Capability};
    use crate::sync::MetadataClass;
    let empty = crate::MetadataManifest::new(None, 0o644, None, vec![], None, None, None).unwrap();
    for source in [None, Some(empty)] {
        for model in [
            AclModel::WindowsSid,
            AclModel::Unknown,
            AclModel::Posix,
            AclModel::Nfs4,
        ] {
            let mut plan = windows_annotation_plan(source.clone());
            let capabilities = crate::metadata::MetadataCapabilities {
                acl_model: model,
                acl: Capability::Exact,
                ..windows_annotation_capabilities()
            };
            annotate_unreadable_metadata_with_capabilities(
                &mut plan,
                &capabilities,
                ProviderKind::Smb,
                &SyncOptions {
                    metadata: MetadataProfile::NativeBestEffort,
                    ..options(SyncMode::AddOnly)
                },
            )
            .unwrap();
            let missing = matches!(model, AclModel::WindowsSid | AclModel::Unknown);
            let acl = plan
                .metadata_report
                .classes
                .iter()
                .find(|count| count.class == MetadataClass::Acl)
                .unwrap();
            assert_eq!((acl.transferred, acl.omitted), (0, u64::from(missing)));
            assert_eq!(
                plan.metadata_report
                    .issues
                    .iter()
                    .filter(|issue| issue.class == MetadataClass::Acl)
                    .count(),
                usize::from(missing)
            );
        }
    }
}

#[test]
fn windows_acl_annotation_rejects_inconsistent_or_unbound_manifests() {
    for case in ["namespace", "missing-dacl", "digest"] {
        let mut source = windows_annotation_source(true, true);
        match case {
            "namespace" => source.windows_security.as_mut().unwrap().namespace = [8; 16],
            "missing-dacl" => source.acl = None,
            "digest" => source.digest[0] ^= 1,
            _ => unreachable!(),
        }
        assert!(source.validate().is_err());
        let mut plan = windows_annotation_plan(Some(source));
        let before = plan.metadata_report.clone();
        assert_eq!(
            annotate_windows_plan(&mut plan).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(plan.metadata_report, before);
    }
}

#[test]
fn windows_acl_annotation_does_not_change_portable_or_native_strict_contracts() {
    for profile in [MetadataProfile::Portable, MetadataProfile::NativeStrict] {
        let mut plan = windows_annotation_plan(Some(windows_annotation_source(false, false)));
        let before_actions = plan.actions.clone();
        let before_report = plan.metadata_report.clone();
        annotate_unreadable_metadata_with_capabilities(
            &mut plan,
            &windows_annotation_capabilities(),
            ProviderKind::Smb,
            &SyncOptions {
                metadata: profile,
                ..options(SyncMode::AddOnly)
            },
        )
        .unwrap();
        assert_eq!(plan.actions, before_actions);
        assert!(!plan.metadata_report.strict_verified);
        if profile == MetadataProfile::Portable {
            assert_eq!(plan.metadata_report, before_report);
        } else {
            assert!(!plan
                .metadata_report
                .issues
                .iter()
                .any(|issue| issue.class == crate::sync::MetadataClass::Acl));
        }
    }
}

struct SparseLayoutFixture;
impl Drop for SparseLayoutFixture {
    fn drop(&mut self) {
        crate::local_fs::TEST_SPARSE_LAYOUT.with(|value| *value.borrow_mut() = None);
    }
}
fn set_sparse_fixture(
    source: &Path,
    extents: Vec<crate::metadata::DataExtent>,
) -> SparseLayoutFixture {
    crate::local_fs::TEST_SPARSE_LAYOUT
        .with(|value| *value.borrow_mut() = Some((source.into(), extents)));
    SparseLayoutFixture
}
fn layout_queue_run(
    queue: &OperationQueue,
    source: &Path,
    target: &Path,
    mode: SyncMode,
    audit: &Path,
) -> io::Result<(crate::SyncStats, ContentSyncPreview)> {
    let prepared = crate::prepare_content_sync_for_queue(
        queue,
        ProviderEndpoint::local(source)?,
        ProviderEndpoint::local(target)?,
        SyncOptions {
            mode,
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeBestEffort,
            ..Default::default()
        },
        &Cancel::new(),
    )?;
    let preview = prepared.preview().clone();
    queue.enqueue(prepared.queue_operation())?;
    let entry = queue.claim_next()?.unwrap();
    let crate::QueueExecutionResult::Sync(stats) =
        crate::execute_prepared_content_sync_queue_entry(
            queue,
            &entry,
            prepared,
            true,
            audit,
            &Cancel::new(),
            &mut |_| {},
        )?
    else {
        panic!()
    };
    assert_eq!(
        queue.snapshot().entries.last().unwrap().state,
        QueueState::Completed
    );
    Ok((stats, preview))
}
#[test]
fn content_sync_fine_sparse_layout_converges_and_keeps_hardlink_groups() -> io::Result<()> {
    use crate::metadata::DataExtent;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for aliases in [false, true] {
        let area = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let root = area.path().canonicalize()?;
        let source = root.join("source");
        let target = root.join("target");
        fs::create_dir(&source)?;
        fs::create_dir(&target)?;
        let Some(unit) = crate::LocalVfs::new().sparse_allocation_unit(&target)? else {
            continue;
        };
        assert!(unit > 1 && unit <= 1024 * 1024);
        let length = unit as usize * 2;
        let half = unit as usize / 2;
        let mut data = vec![0; length];
        data[half..unit as usize].fill(0x41);
        fs::write(source.join("a"), &data)?;
        if aliases {
            fs::hard_link(source.join("a"), source.join("b"))?;
        }
        fs::File::open(source.join("a"))?.set_times(
            fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            ),
        )?;
        let _fixture = set_sparse_fixture(
            &source,
            vec![DataExtent {
                offset: half as u64,
                length: half as u64,
            }],
        );
        let config = root.join("config");
        fs::create_dir(&config)?;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o700))?;
        let queue = OperationQueue::open(config.join("operations-queue.json"))?;
        let audit = config.join("audit.jsonl");
        for (mode, step) in [
            (SyncMode::AddOnly, "add"),
            (SyncMode::Update, "noop"),
            (SyncMode::Update, "update"),
            (SyncMode::Mirror, "mirror"),
            (SyncMode::Update, "final-noop"),
        ] {
            if step == "update" {
                data[half..unit as usize].fill(0x42);
                fs::write(source.join("a"), &data)?;
                fs::File::open(source.join("a"))?.set_times(fs::FileTimes::new().set_modified(
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_005),
                ))?;
            }
            if step == "mirror" {
                fs::write(target.join("extra"), b"extra")?;
            }
            let (stats, preview) = layout_queue_run(&queue, &source, &target, mode, &audit)?;
            let count = if aliases { 2 } else { 1 };
            for report in [&preview.plan.metadata_report, &stats.metadata_report] {
                let issues: Vec<_> = report
                    .issues
                    .iter()
                    .filter(|issue| issue.class == crate::MetadataClass::SparseExtents)
                    .collect();
                assert_eq!(
                    issues.len(),
                    count,
                    "aliases={aliases} step={step}: {report:?}"
                );
                let class = report
                    .classes
                    .iter()
                    .find(|class| class.class == crate::MetadataClass::SparseExtents)
                    .unwrap();
                assert_eq!((class.transferred, class.omitted), (0, count as u64));
            }
            if matches!(step, "noop" | "final-noop") {
                assert!(preview.plan.actions.is_empty());
                assert_eq!(stats.bytes_copied, 0);
                assert_eq!(stats.files_copied, 0);
            }
            assert_eq!(fs::read(target.join("a"))?, data);
            if aliases {
                assert_eq!(fs::read(target.join("b"))?, data);
                assert_eq!(
                    fs::metadata(target.join("a"))?.ino(),
                    fs::metadata(target.join("b"))?.ino()
                );
            }
            assert!(!target.join("extra").exists());
            println!("LOCAL_SPARSE_GEOMETRY_ACTUAL_QUEUE_CONVERGENCE_OK aliases={aliases} step={step} unit={unit}");
        }
    }
    Ok(())
}
#[test]
fn content_sync_sparse_omission_noop_binds_metadata_before_mirror_removal() -> io::Result<()> {
    use crate::metadata::DataExtent;
    use std::os::unix::fs::PermissionsExt;
    let area = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let root = area.path().canonicalize()?;
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir(&source)?;
    fs::create_dir(&target)?;
    let Some(unit) = crate::LocalVfs::new().sparse_allocation_unit(&target)? else {
        return Ok(());
    };
    let mut bytes = vec![0; unit as usize * 2];
    bytes[unit as usize / 2..unit as usize].fill(0x41);
    fs::write(source.join("file"), &bytes)?;
    fs::write(target.join("file"), &bytes)?;
    fs::write(target.join("extra"), b"keep")?;
    let _fixture = set_sparse_fixture(
        &source,
        vec![DataExtent {
            offset: u64::from(unit / 2),
            length: u64::from(unit / 2),
        }],
    );
    let config = root.join("config");
    fs::create_dir(&config)?;
    fs::set_permissions(&config, fs::Permissions::from_mode(0o700))?;
    let queue = OperationQueue::open(config.join("operations-queue.json"))?;
    let prepared = crate::prepare_content_sync_for_queue(
        &queue,
        ProviderEndpoint::local(&source)?,
        ProviderEndpoint::local(&target)?,
        SyncOptions {
            mode: SyncMode::Mirror,
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeBestEffort,
            ..Default::default()
        },
        &Cancel::new(),
    )?;
    assert_eq!(
        prepared.preview().plan.copy_count + prepared.preview().plan.update_count,
        0
    );
    assert!(prepared
        .preview()
        .plan
        .metadata_observations
        .iter()
        .any(|value| value.path == "file"));
    crate::local_fs::TEST_SPARSE_LAYOUT.with(|value| {
        value.borrow_mut().as_mut().unwrap().1 = vec![DataExtent {
            offset: 0,
            length: u64::from(unit),
        }]
    });
    queue.enqueue(prepared.queue_operation())?;
    let entry = queue.claim_next()?.unwrap();
    let error = crate::execute_prepared_content_sync_queue_entry(
        &queue,
        &entry,
        prepared,
        true,
        &config.join("audit.jsonl"),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
    assert_eq!(fs::read(target.join("extra"))?, b"keep");
    assert_eq!(fs::read(target.join("file"))?, bytes);
    Ok(())
}
#[test]
fn content_sync_sparse_odd_eof_keeps_exact_ranges() -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let area = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let root = area.path().canonicalize()?;
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir(&source)?;
    fs::create_dir(&target)?;
    let Some(unit) = crate::LocalVfs::new().sparse_allocation_unit(&target)? else {
        return Ok(());
    };
    let holes = fs::File::create(source.join("holes"))?;
    holes.set_len(u64::from(unit) + 1)?;
    holes.sync_all()?;
    let mut tail = fs::File::create(source.join("tail"))?;
    std::io::Write::write_all(&mut tail, &vec![0x41; unit as usize])?;
    tail.set_len(u64::from(unit) + 1)?;
    crate::vfs::SparseWrite::punch_hole(&mut tail, u64::from(unit), u64::from(unit))?;
    tail.sync_all()?;
    assert_eq!(
        crate::LocalVfs::new()
            .metadata_manifest(&source.join("tail"))?
            .sparse_extents,
        Some(vec![crate::metadata::DataExtent {
            offset: 0,
            length: u64::from(unit),
        }])
    );
    let config = root.join("config");
    fs::create_dir(&config)?;
    fs::set_permissions(&config, fs::Permissions::from_mode(0o700))?;
    let queue = OperationQueue::open(config.join("operations-queue.json"))?;
    let (stats, _) = layout_queue_run(
        &queue,
        &source,
        &target,
        SyncMode::AddOnly,
        &config.join("audit.jsonl"),
    )?;
    assert!(stats
        .metadata_report
        .issues
        .iter()
        .all(|issue| issue.class != crate::MetadataClass::SparseExtents));
    for name in ["holes", "tail"] {
        assert_eq!(fs::read(source.join(name))?, fs::read(target.join(name))?);
        assert_eq!(
            crate::LocalVfs::new()
                .metadata_manifest(&source.join(name))?
                .sparse_extents,
            crate::LocalVfs::new()
                .metadata_manifest(&target.join(name))?
                .sparse_extents
        );
    }
    Ok(())
}
