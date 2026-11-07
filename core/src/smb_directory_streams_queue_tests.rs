//! Gewöhnlicher Ordnerstream-Sync über Vorbereitung, persistente Queue und Audit.
use super::*;
use crate::{
    execute_prepared_content_sync_queue_entry, prepare_content_sync_for_queue, Cancel, Compare,
    ContentSyncPreview, MetadataClass, MetadataProfile, OperationQueue, ProviderEndpoint,
    QueueExecutionResult, QueueState, SyncMode, SyncOptions, SyncStats,
};

fn options(mode: SyncMode) -> SyncOptions {
    SyncOptions {
        mode,
        compare: Compare::ContentHash,
        metadata: MetadataProfile::NativeBestEffort,
        ..SyncOptions::default()
    }
}

fn clear_stream(provider: &SmbVfs, path: &Path, name: &[u8]) {
    let named = stream_path(&provider.remote_path(path).unwrap(), name).unwrap();
    let mut state = lock(&provider.state);
    let tree = state.session.tree.clone();
    provider
        .runtime
        .block_on(delete_path(
            &mut state.session.connection,
            &tree,
            &named,
            false,
        ))
        .unwrap();
}

fn run(
    queue: &OperationQueue,
    provider: &Arc<SmbVfs>,
    source: &Path,
    target: &Path,
    mode: SyncMode,
    audit: &Path,
) -> (SyncStats, ContentSyncPreview) {
    let prepared = prepare_content_sync_for_queue(
        queue,
        ProviderEndpoint::smb(provider.clone(), source).unwrap(),
        ProviderEndpoint::smb(provider.clone(), target).unwrap(),
        options(mode),
        &Cancel::new(),
    )
    .unwrap();
    let preview = prepared.preview().clone();
    queue.enqueue(prepared.queue_operation()).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let result = execute_prepared_content_sync_queue_entry(
        queue,
        &entry,
        prepared,
        true,
        audit,
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    let QueueExecutionResult::Sync(stats) = result else {
        panic!("Syncbilanz fehlt")
    };
    let snapshot = queue.snapshot();
    let entry = snapshot.entries.last().unwrap();
    assert_eq!(entry.state, QueueState::Completed);
    assert_eq!(entry.directory_stream_report, stats.directory_stream_report);
    let audit_contents = std::fs::read_to_string(audit).unwrap();
    let record: serde_json::Value =
        serde_json::from_str(audit_contents.lines().last().unwrap()).unwrap();
    assert_eq!(
        record["directory_stream_effects_planned"],
        serde_json::to_value(&preview.directory_stream_effects).unwrap()
    );
    assert_eq!(
        record["directory_stream_report"],
        serde_json::to_value(&stats.directory_stream_report).unwrap()
    );
    (stats, preview)
}

fn assert_effects(
    stats: &SyncStats,
    preview: &ContentSyncPreview,
    adds: usize,
    replaces: usize,
    removes: usize,
) {
    let count = |effects: &[DirectoryStreamOutcome]| {
        (
            effects
                .iter()
                .filter(|value| matches!(value.effect, DirectoryStreamEffect::Add { .. }))
                .count(),
            effects
                .iter()
                .filter(|value| matches!(value.effect, DirectoryStreamEffect::Replace { .. }))
                .count(),
            effects
                .iter()
                .filter(|value| matches!(value.effect, DirectoryStreamEffect::Remove { .. }))
                .count(),
        )
    };
    assert_eq!(
        count(&preview.directory_stream_effects),
        (adds, replaces, removes)
    );
    assert_eq!(
        count(&stats.directory_stream_report.completed),
        (adds, replaces, removes)
    );
    assert!(stats.directory_stream_report.uncertain.is_empty());
    let mut planned = preview.directory_stream_effects.clone();
    let mut completed = stats.directory_stream_report.completed.clone();
    planned.sort_by(|a, b| a.effect.name().cmp(b.effect.name()));
    completed.sort_by(|a, b| a.effect.name().cmp(b.effect.name()));
    assert_eq!(planned, completed);
}

#[test]
#[ignore = "benötigt privaten Samba-Loopback mit streams_xattr"]
fn directory_stream_content_queue_roundtrip() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("{name} fehlt"));
    let physical_root = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(physical_root.starts_with("/tmp/doppeldecker-directory-stream-write-probe-20261029"));
    assert_eq!(physical_root.file_name().unwrap(), "share");
    assert!(std::fs::read_dir(&physical_root).unwrap().next().is_none());
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    let config = SmbConnectionConfig {
        server_guid: discover_smb_server(&host, port).unwrap().server_guid,
        host,
        port,
        share: required("DOPPELDECKER_SMB_TEST_SHARE"),
        username: required("DOPPELDECKER_SMB_TEST_USER"),
        domain: String::new(),
        requested_root: "/".into(),
        password_reference: crate::KeychainSecretReference::new("Doppeldecker-Test", "smb")
            .unwrap(),
    };
    let provider = Arc::new(SmbVfs::connect(config, Arc::new(TestSecrets)).unwrap());
    assert!(provider.identity().encryption_active && provider.identity().signing_active);
    std::fs::create_dir_all(physical_root.join("queue/source/Ordner")).unwrap();
    std::fs::create_dir(physical_root.join("queue/target")).unwrap();
    let source = Path::new("/queue/source");
    let target = Path::new("/queue/target");
    let source_directory = source.join("Ordner");
    let target_directory = target.join("Ordner");
    seed(&provider, &source_directory, "Änderung".as_bytes(), b"eins");
    seed(&provider, &source_directory, b"leer", b"");
    seed(&provider, &source_directory, b"weg", b"alt");
    provider
        .set_mtime_unchecked(&source_directory, 1_700_000_000)
        .unwrap();
    let config_root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let queue_path = config_root.path().join("queue.json");
    let queue = OperationQueue::open(&queue_path).unwrap();
    let audit = config_root.path().join("audit.jsonl");
    let (first, preview) = run(&queue, &provider, source, target, SyncMode::AddOnly, &audit);
    assert_effects(&first, &preview, 3, 0, 0);
    assert_eq!(
        (
            first.files_copied,
            first.bytes_copied,
            first.metadata_updated
        ),
        (0, 7, 0)
    );
    assert_eq!(
        observed(&provider, &source_directory),
        observed(&provider, &target_directory)
    );
    assert!(first
        .metadata_report
        .classes
        .iter()
        .any(|class| class.class == MetadataClass::NamedStreams
            && class.transferred == 1
            && class.omitted == 0));
    eprintln!("DIRECTORY_QUEUE AddOnly neuer Ordner: Unicode/leer, 7 Streambytes, 3 konkrete Addwirkungen und persistierter Report");

    clear_stream(&provider, &source_directory, "Änderung".as_bytes());
    clear_stream(&provider, &source_directory, b"weg");
    seed(
        &provider,
        &source_directory,
        "Änderung".as_bytes(),
        b"zwei!",
    );
    seed(&provider, &source_directory, b"neu", b"neu");
    provider
        .set_mtime_unchecked(&source_directory, 1_700_000_001)
        .unwrap();
    let old_target = observed(&provider, &target_directory);
    let (add_only, preview) = run(&queue, &provider, source, target, SyncMode::AddOnly, &audit);
    assert_effects(&add_only, &preview, 0, 0, 0);
    assert_eq!(add_only.bytes_copied, 0);
    assert_eq!(observed(&provider, &target_directory), old_target);
    let (updated, preview) = run(&queue, &provider, source, target, SyncMode::Update, &audit);
    assert_effects(&updated, &preview, 1, 1, 1);
    let planned_metadata = preview
        .plan
        .actions
        .iter()
        .find_map(|action| match action {
            crate::Action::SetDirectoryMetadata { metadata, .. } => Some(metadata),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        planned_metadata.source.named_streams.as_ref().unwrap(),
        &observed(&provider, &source_directory)
    );
    assert_eq!(
        planned_metadata
            .target
            .as_ref()
            .unwrap()
            .named_streams
            .as_ref()
            .unwrap(),
        &old_target
    );
    assert_eq!(
        (
            updated.files_copied,
            updated.bytes_copied,
            updated.metadata_updated
        ),
        (0, 8, 1)
    );
    assert_eq!(
        observed(&provider, &source_directory),
        observed(&provider, &target_directory)
    );
    assert_eq!(
        (
            preview.irreversible_replacements,
            preview.irreversible_removals
        ),
        (0, 0),
        "Streamwirkungen besitzen ihre eigene konkrete Vorschau"
    );
    let (noop, preview) = run(&queue, &provider, source, target, SyncMode::Update, &audit);
    assert_effects(&noop, &preview, 0, 0, 0);
    assert_eq!(
        (noop.files_copied, noop.bytes_copied, noop.metadata_updated),
        (0, 0, 0)
    );
    eprintln!("DIRECTORY_QUEUE AddOnly bestehend unverändert; Update Add/Replace/Remove 8 Streambytes; anschließender Noop null");

    risk_cases(
        &provider,
        &physical_root,
        &queue,
        &audit,
        source,
        target,
        &source_directory,
        &target_directory,
    );

    let last_report = queue
        .snapshot()
        .entries
        .iter()
        .find(|entry| !entry.directory_stream_report.is_empty())
        .unwrap()
        .directory_stream_report
        .clone();
    drop(queue);
    let reloaded = OperationQueue::open(&queue_path).unwrap();
    assert_eq!(
        reloaded
            .snapshot()
            .entries
            .iter()
            .find(|entry| !entry.directory_stream_report.is_empty())
            .unwrap()
            .directory_stream_report,
        last_report
    );
    drop(reloaded);
    drop(provider);
    std::fs::remove_dir_all(physical_root.join("queue")).unwrap();
    assert!(std::fs::read_dir(&physical_root).unwrap().next().is_none());
    eprintln!("DIRECTORY_QUEUE Journalreload erhält Report; künstlicher Root vollständig leer");
}

#[allow(clippy::too_many_arguments)]
fn risk_cases(
    provider: &Arc<SmbVfs>,
    physical_root: &Path,
    queue: &OperationQueue,
    audit: &Path,
    source: &Path,
    target: &Path,
    source_directory: &Path,
    target_directory: &Path,
) {
    clear_stream(provider, source_directory, "Änderung".as_bytes());
    clear_stream(provider, source_directory, b"leer");
    seed(provider, source_directory, "Änderung".as_bytes(), b"mirror");
    seed(provider, source_directory, b"mirror-add", b"m");
    provider
        .set_mtime_unchecked(source_directory, 1_700_000_002)
        .unwrap();
    std::fs::write(physical_root.join("queue/source/new.txt"), b"new").unwrap();
    std::fs::write(physical_root.join("queue/target/extra.txt"), b"old").unwrap();
    let prepared = prepare_content_sync_for_queue(
        queue,
        ProviderEndpoint::smb(provider.clone(), source).unwrap(),
        ProviderEndpoint::smb(provider.clone(), target).unwrap(),
        options(SyncMode::Mirror),
        &Cancel::new(),
    )
    .unwrap();
    let preview = prepared.preview().clone();
    assert_eq!(preview.irreversible_removals, 1);
    assert_eq!(preview.directory_stream_effects.len(), 3);
    queue.enqueue(prepared.queue_operation()).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let mut saw_delete = false;
    let result = execute_prepared_content_sync_queue_entry(
        queue,
        &entry,
        prepared,
        true,
        audit,
        &Cancel::new(),
        &mut |progress| {
            if progress.phase == "delete" {
                saw_delete = true;
                assert_eq!(
                    std::fs::read(physical_root.join("queue/target/new.txt")).unwrap(),
                    b"new"
                );
                assert_eq!(
                    observed(provider, source_directory),
                    observed(provider, target_directory)
                );
                assert_eq!(progress.directory_stream_report.completed.len(), 3);
                assert_eq!(
                    queue
                        .snapshot()
                        .entries
                        .last()
                        .unwrap()
                        .directory_stream_report
                        .completed
                        .len(),
                    3
                );
            }
        },
    )
    .unwrap();
    let QueueExecutionResult::Sync(stats) = result else {
        panic!("Syncbilanz fehlt")
    };
    assert!(saw_delete);
    assert_effects(&stats, &preview, 1, 1, 1);
    assert_eq!(
        (stats.files_copied, stats.files_deleted, stats.bytes_copied),
        (1, 1, 10)
    );
    assert!(!physical_root.join("queue/target/extra.txt").exists());
    eprintln!("DIRECTORY_QUEUE Mirror: 7 Streambytes + 3 normale Bytes, 3 Streamwirkungen vor normaler Dateientfernung bestätigt");

    for case in ["source-drift", "target-drift", "cancel", "partial-error"] {
        let physical = physical_root.join("queue").join(case);
        std::fs::create_dir_all(physical.join("source/Ordner")).unwrap();
        std::fs::create_dir_all(physical.join("target/Ordner")).unwrap();
        let source = Path::new("/queue").join(case).join("source");
        let target = Path::new("/queue").join(case).join("target");
        let source_directory = source.join("Ordner");
        let target_directory = target.join("Ordner");
        seed(provider, &source_directory, b"a", b"first");
        seed(provider, &source_directory, b"b", b"second");
        provider
            .set_mtime_unchecked(&source_directory, 1_700_000_010)
            .unwrap();
        provider
            .set_mtime_unchecked(&target_directory, 1_700_000_000)
            .unwrap();
        let config = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let queue_path = config.path().join("queue.json");
        let queue = OperationQueue::open(&queue_path).unwrap();
        let cancel = Cancel::new();
        let prepared = prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::smb(provider.clone(), &source).unwrap(),
            ProviderEndpoint::smb(provider.clone(), &target).unwrap(),
            options(SyncMode::Update),
            &cancel,
        )
        .unwrap();
        assert_eq!(prepared.preview().directory_stream_effects.len(), 2);
        queue.enqueue(prepared.queue_operation()).unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        if case == "source-drift" {
            seed(provider, &source_directory, b"foreign", b"source");
        }
        if case == "target-drift" {
            seed(provider, &target_directory, b"foreign", b"target");
        }
        let mut triggered = false;
        let result = execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &config.path().join("audit.jsonl"),
            &cancel,
            &mut |progress| {
                if !triggered && progress.directory_stream_report.completed.len() == 1 {
                    triggered = true;
                    if case == "cancel" {
                        cancel.cancel();
                    }
                    if case == "partial-error" {
                        seed(provider, &target_directory, b"foreign", b"target");
                    }
                }
            },
        );
        if case.ends_with("drift") {
            let error = result.unwrap_err();
            assert_eq!(
                crate::error::error_code(&error),
                crate::ErrorCode::PlanDrift
            );
            assert!(crate::error::directory_stream_report(&error).is_empty());
            let target = observed(provider, &target_directory);
            assert_eq!(
                target,
                if case == "target-drift" {
                    vec![summary("foreign", b"target")]
                } else {
                    vec![]
                }
            );
            assert!(queue
                .snapshot()
                .entries
                .last()
                .unwrap()
                .directory_stream_report
                .is_empty());
            eprintln!("DIRECTORY_QUEUE {case}: nach Vorbereitung frisch erkannt, keine geplante Streamwirkung");
        } else {
            assert!(triggered);
            let report = if case == "cancel" {
                match result {
                    Ok(QueueExecutionResult::Sync(stats)) => {
                        assert!(stats.cancelled);
                        assert!(!stats.temp_recovery_paths.is_empty());
                        assert_eq!(
                            queue.snapshot().entries.last().unwrap().state,
                            QueueState::Cancelled
                        );
                        stats.directory_stream_report
                    }
                    Err(error) => {
                        assert_eq!(
                            crate::error::error_code(&error),
                            crate::ErrorCode::Interrupted
                        );
                        assert!(!crate::error::recovery_paths(&error).is_empty());
                        crate::error::directory_stream_report(&error)
                    }
                    _ => panic!("Syncbilanz fehlt"),
                }
            } else {
                let error = result.unwrap_err();
                assert_eq!(
                    queue.snapshot().entries.last().unwrap().state,
                    QueueState::Failed
                );
                crate::error::directory_stream_report(&error)
            };
            assert_eq!(report.completed.len(), 1);
            assert!(report.uncertain.is_empty());
            assert_eq!(
                report.completed[0].effect,
                DirectoryStreamEffect::Add {
                    after: summary("a", b"first")
                }
            );
            assert_eq!(
                queue
                    .snapshot()
                    .entries
                    .last()
                    .unwrap()
                    .directory_stream_report,
                report
            );
            let current = observed(provider, &target_directory);
            assert!(current.contains(&summary("a", b"first")));
            assert!(!current.iter().any(|stream| stream.name == b"b"));
            assert!(
                prepare_content_sync_for_queue(
                    &queue,
                    ProviderEndpoint::smb(provider.clone(), &source).unwrap(),
                    ProviderEndpoint::smb(provider.clone(), &target).unwrap(),
                    options(SyncMode::Update),
                    &Cancel::new()
                )
                .is_err(),
                "Offener konkreter Streamintent muss betroffenen Root sperren"
            );
            drop(queue);
            let reloaded = OperationQueue::open(&queue_path).unwrap();
            assert_eq!(
                reloaded
                    .snapshot()
                    .entries
                    .last()
                    .unwrap()
                    .directory_stream_report,
                report
            );
            eprintln!("DIRECTORY_QUEUE {case}: erste Einzelwirkung dauerhaft erhalten, Queue+Reload exakt, offener Intent sperrt Neuabgleich");
        }
    }
}
