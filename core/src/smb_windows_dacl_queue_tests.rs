//! Tatsächliche Vorbereitung, Fensterqueue und Audit mit dem getrennten DACL-Vertrag.
use super::*;
use crate::{
    Cancel, Compare, MetadataClass, MetadataProfile, OperationQueue, ProviderEndpoint,
    QueueExecutionResult, QueueState, SyncMode, SyncOptions,
};
use std::os::unix::fs::PermissionsExt;

fn seed(
    provider: &Arc<SmbVfs>,
    root: &Path,
    owner: InteractiveFileOwner,
    content: &[u8],
    exact_server: bool,
) -> io::Result<()> {
    let context = provider.content_sync_mutations(root, &Cancel::new(), Some(owner))?;
    context.prepare_windows_dacl_write()?;
    let target = root.join("file.bin");
    let temp = root.join(".seed");
    let scope = context.staging_scope(&target, None)?.unwrap();
    let mut writer = scope.open_write_new(&temp)?;
    writer.write_all(content)?;
    writer.flush()?;
    drop(writer);
    let mut stream = scope.open_named_stream_write_new(&temp, b"named")?;
    stream.write_all(b"queue-ads")?;
    stream.flush()?;
    drop(stream);
    if exact_server {
        let Support::Ready(expected) = context.windows_dacl_write_support()? else {
            panic!()
        };
        scope.write_windows_dacl_exact(&temp, &probe_acl(&expected), &expected)?;
    }
    scope.set_mtime(&temp, 1_700_000_000)?;
    if provider.stat(&target).is_ok() {
        scope.rename_replace_plain_once(&temp, &target)?;
    } else {
        scope.rename_noreplace(&temp, &target)?;
    }
    scope.finish_staging()
}

#[test]
#[ignore = "benötigt frischen privaten Samba mit optionalem acl_xattr"]
fn windows_dacl_actual_content_queue_roundtrip() -> io::Result<()> {
    let required = |name| std::env::var(name).expect("Privater DACL-Queuerunner erforderlich");
    let physical = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(physical.starts_with("/tmp/doppeldecker-smb-acl-write-probe-20261029"));
    assert_eq!(physical.file_name().unwrap(), "share");
    assert!(std::fs::read_dir(&physical)?.next().is_none());
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    assert_eq!(host, "127.0.0.1");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    let config = SmbConnectionConfig {
        server_guid: discover_smb_server(&host, port)?.server_guid,
        host,
        port,
        share: required("DOPPELDECKER_SMB_TEST_SHARE"),
        username: required("DOPPELDECKER_SMB_TEST_USER"),
        domain: String::new(),
        requested_root: "/".into(),
        password_reference: crate::KeychainSecretReference::new(
            "Doppeldecker-Test",
            "smb-dacl-queue",
        )?,
    };
    let source = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?);
    let target = Arc::new(SmbVfs::connect(config, Arc::new(TestSecrets))?);
    assert_ne!(
        lock(&source.state).session.connection.session_id(),
        lock(&target.state).session.connection.session_id()
    );
    std::fs::create_dir_all(physical.join("queue/source"))?;
    std::fs::create_dir(physical.join("queue/target"))?;
    let source_root = Path::new("/queue/source");
    let target_root = Path::new("/queue/target");
    let source_file = source_root.join("file.bin");
    let target_file = target_root.join("file.bin");
    let config = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let queue = OperationQueue::open(config.path().join("queue.json"))?;
    let audit = config.path().join("audit.jsonl");
    let owner = || InteractiveFileOwner {
        staging: crate::local_staging::StagingOwner {
            queue: config.path().join("seed-queue.json"),
            operation: "dacl-queue-seed".into(),
        },
        profiles: vec![],
    };
    let exact_server = std::env::var("DOPPELDECKER_SMB_STREAM_MODULE")
        .unwrap_or_default()
        .contains("acl_xattr");
    seed(&source, source_root, owner(), b"main-one", exact_server)?;
    for (mode, content, expected_copies) in [
        (SyncMode::AddOnly, b"main-one".as_slice(), 1),
        (SyncMode::Update, b"main-one".as_slice(), 0),
        (SyncMode::Update, b"main-two-long".as_slice(), 1),
        (SyncMode::Update, b"main-two-long".as_slice(), 0),
    ] {
        if content == b"main-two-long" && expected_copies == 1 {
            seed(&source, source_root, owner(), content, exact_server)?;
        }
        let prepared = crate::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::smb(source.clone(), source_root)?,
            ProviderEndpoint::smb(target.clone(), target_root)?,
            SyncOptions {
                mode,
                compare: Compare::ContentHash,
                metadata: MetadataProfile::NativeBestEffort,
                ..SyncOptions::default()
            },
            &Cancel::new(),
        )?;
        let preview = prepared.preview().clone();
        assert_eq!(
            preview.plan.copy_count + preview.plan.update_count,
            expected_copies,
            "Geplante Dateiübertragungen: {mode:?}, {} Hauptdatenbytes",
            content.len()
        );
        queue.enqueue(prepared.queue_operation())?;
        let entry = queue.claim_next()?.unwrap();
        let result = crate::execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &audit,
            &Cancel::new(),
            &mut |_| {},
        )?;
        let QueueExecutionResult::Sync(stats) = result else {
            panic!()
        };
        assert_eq!(stats.files_copied, expected_copies);
        if expected_copies == 0 {
            assert_eq!(stats.bytes_copied, 0);
            assert_eq!(stats.metadata_updated, 0);
        }
        assert_eq!(
            queue.snapshot().entries.last().unwrap().state,
            QueueState::Completed
        );
        let reader_source = source.content_sync_reader(source_root, &Cancel::new(), None)?;
        let reader_target = target.content_sync_reader(target_root, &Cancel::new(), None)?;
        let source_manifest = reader_source.metadata_manifest(&source_file)?;
        let target_manifest = reader_target.metadata_manifest(&target_file)?;
        assert_eq!(source_manifest.named_streams, target_manifest.named_streams);
        if exact_server && expected_copies > 0 {
            assert_eq!(source_manifest.acl, target_manifest.acl);
            assert!(
                stats
                    .metadata_report
                    .classes
                    .iter()
                    .any(|class| class.class == MetadataClass::Acl && class.transferred >= 1),
                "ACL-Klassenbilanz fehlt: {:?}",
                stats.metadata_report
            );
        } else if !exact_server && expected_copies > 0 {
            assert!(
                stats
                    .metadata_report
                    .issues
                    .iter()
                    .any(|issue| issue.class == MetadataClass::Acl),
                "ACL-Auslassung fehlt"
            );
        }
        assert_eq!(
            std::fs::read(physical.join("queue/target/file.bin"))?,
            content
        );
        let mut ads = Vec::new();
        reader_target
            .open_named_stream_read(&target_file, b"named")?
            .read_to_end(&mut ads)?;
        assert_eq!(ads, b"queue-ads");
        let audit_contents = std::fs::read_to_string(&audit)?;
        let record: serde_json::Value =
            serde_json::from_str(audit_contents.lines().last().unwrap())?;
        assert!(record.is_object());
    }
    if exact_server {
        let prepare = || {
            crate::prepare_content_sync_for_queue(
                &queue,
                ProviderEndpoint::smb(source.clone(), source_root)?,
                ProviderEndpoint::smb(target.clone(), target_root)?,
                SyncOptions {
                    mode: SyncMode::Update,
                    compare: Compare::ContentHash,
                    metadata: MetadataProfile::NativeBestEffort,
                    ..SyncOptions::default()
                },
                &Cancel::new(),
            )
        };
        let execute = |prepared: crate::PreparedContentSync| {
            queue.enqueue(prepared.queue_operation())?;
            let entry = queue.claim_next()?.unwrap();
            crate::execute_prepared_content_sync_queue_entry(
                &queue,
                &entry,
                prepared,
                true,
                &audit,
                &Cancel::new(),
                &mut |_| {},
            )
        };
        let before_target = target
            .content_sync_reader(target_root, &Cancel::new(), None)?
            .metadata_manifest(&target_file)?;
        let remote = source.remote_path(&source_file)?;
        let (tree, mut connection) = {
            let state = lock(&source.state);
            (state.session.tree.clone(), state.session.connection.clone())
        };
        source.runtime.block_on(async {
            let id = open(&mut connection, &tree, &remote, FILE_ACCESS, false, false).await?;
            let observed = query(&mut connection, &tree, id, source.config.server_guid).await?;
            let expected = context(&observed)?;
            let mut acl = probe_acl(&expected);
            if let WindowsDaclState::Acl { aces, .. } = &mut acl.state {
                let WindowsAce::Allow { mask, .. } = &mut aces[2] else {
                    panic!()
                };
                *mask = 0x0012_00a9;
            }
            set(
                &mut connection,
                &tree,
                id,
                acl.encode_for_file_write(expected.namespace, &expected.owner)?,
            )
            .await?;
            exact(
                &query(&mut connection, &tree, id, expected.namespace).await?,
                &acl,
                &expected,
            )?;
            tree.close_handle(&mut connection, id)
                .await
                .map_err(smb_error)
        })?;
        let prepared = prepare()?;
        assert_eq!(prepared.preview().plan.copy_count, 0);
        assert_eq!(prepared.preview().file_dacl_effects.len(), 1);
        assert!(prepared
            .preview()
            .plan
            .metadata_report
            .issues
            .iter()
            .all(|issue| issue.class != MetadataClass::Acl));
        let QueueExecutionResult::Sync(stats) = execute(prepared)? else {
            panic!()
        };
        assert_eq!(
            (
                stats.files_copied,
                stats.bytes_copied,
                stats.metadata_updated
            ),
            (0, 0, 1)
        );
        assert_eq!(
            target
                .content_sync_reader(target_root, &Cancel::new(), None)?
                .metadata_manifest(&target_file)?
                .acl,
            source
                .content_sync_reader(source_root, &Cancel::new(), None)?
                .metadata_manifest(&source_file)?
                .acl
        );
        assert_ne!(
            before_target.acl,
            source
                .content_sync_reader(source_root, &Cancel::new(), None)?
                .metadata_manifest(&source_file)?
                .acl
        );
        std::fs::write(
            physical.join("queue/source/file.bin"),
            b"runtime-denied-content",
        )?;
        let prepared = prepare()?;
        TEST_FAULT.with(|fault| fault.set(Some("before_open")));
        let result = execute(prepared);
        TEST_FAULT.with(|fault| fault.set(None));
        let QueueExecutionResult::Sync(stats) = result? else {
            panic!()
        };
        assert_eq!(stats.files_copied, 1);
        assert!(stats
            .metadata_report
            .issues
            .iter()
            .any(|issue| issue.class == MetadataClass::Acl));
        assert!(stats
            .metadata_report
            .classes
            .iter()
            .any(|class| class.class == MetadataClass::Acl
                && class.transferred == 0
                && class.omitted >= 1));
        assert_eq!(
            std::fs::read(physical.join("queue/target/file.bin"))?,
            b"runtime-denied-content"
        );
        std::fs::write(
            physical.join("queue/source/file.bin"),
            b"runtime-unclear-new-content",
        )?;
        let prepared = prepare()?;
        let target_inode = target.stat(&target_file)?.inode;
        TEST_BEFORE_SET.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|| {
                TEST_FAULT.with(|fault| fault.set(Some("after_set")));
            }))
        });
        let result = execute(prepared);
        TEST_FAULT.with(|fault| fault.set(None));
        TEST_BEFORE_SET.with(|hook| *hook.borrow_mut() = None);
        let error = result.unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(!crate::error::recovery_paths(&error).is_empty());
        assert_eq!(
            queue.snapshot().entries.last().unwrap().state,
            QueueState::Failed
        );
        assert_eq!(target.stat(&target_file)?.inode, target_inode);
        assert_eq!(
            std::fs::read(physical.join("queue/target/file.bin"))?,
            b"runtime-denied-content"
        );
        let leftovers: Vec<_> = target
            .list(target_root)?
            .into_iter()
            .filter(|entry| entry.name != "file.bin")
            .collect();
        assert_eq!(
            leftovers.len(),
            1,
            "Unklarer eigener Temp muss erhalten bleiben"
        );
        assert!(leftovers[0].is_file && !leftovers[0].is_symlink);
        assert!(
            crate::error::recovery_paths(&error)
                .iter()
                .any(|path| Path::new(path).exists()),
            "Dauerhafter Intent muss erhalten bleiben"
        );
        target.remove_file_unchecked(&target_root.join(&leftovers[0].name))?;
        println!("DACL_ACTUAL_QUEUE_ACL_ONLY_APPLIED_RUNTIME_DENIED_CONTENT_CONTINUES_UNCLEAR_OLD_TARGET_INTENT_PRESERVED_OK");
    }
    let snapshot = queue.snapshot();
    drop(queue);
    let reopened = OperationQueue::open(config.path().join("queue.json"))?;
    assert_eq!(reopened.snapshot().entries.len(), snapshot.entries.len());
    for (before, after) in snapshot
        .entries
        .iter()
        .zip(reopened.snapshot().entries.iter())
    {
        if before.state == QueueState::Failed {
            assert_eq!(after.state, QueueState::Interrupted);
            assert_eq!(after.error_code, Some(crate::ErrorCode::OutcomeUnclear));
            assert!(!after.temp_recovery_paths.is_empty());
        } else {
            assert_eq!(before.state, QueueState::Completed);
            assert_eq!(after.state, QueueState::Completed);
        }
    }
    source.remove_file_unchecked(&source_file)?;
    target.remove_file_unchecked(&target_file)?;
    for path in [source_root, target_root, Path::new("/queue")] {
        target.remove_dir_unchecked(path)?;
    }
    assert!(std::fs::read_dir(&physical)?.next().is_none());
    println!("DACL_ACTUAL_QUEUE_OK exact_server={exact_server} AddOnly_Update_Noop=4 independent_sessions=1 data_ads=1 journal_reload=1 root_empty=1");
    Ok(())
}
