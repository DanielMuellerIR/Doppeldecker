//! Echte Fensterqueue mit Ordner-DACL, Kindkopie und getrennten Ordner-ADS.
use super::*;
use crate::{
    Cancel, Compare, MetadataProfile, OperationQueue, ProviderEndpoint, QueueExecutionResult,
    QueueState, SyncMode, SyncOptions,
};

fn set_ads(provider: &SmbVfs, path: &Path, value: &[u8]) -> io::Result<()> {
    let remote = format!("{}:named:$DATA", provider.remote_path(path)?);
    let (tree, mut connection) = {
        let state = lock(&provider.state);
        (state.session.tree.clone(), state.session.connection.clone())
    };
    let id = provider.runtime.block_on(open_handle(
        &mut connection,
        &tree,
        &remote,
        FileAccessMask::FILE_WRITE_DATA | FileAccessMask::SYNCHRONIZE,
        CreateDisposition::FileOverwriteIf,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
    ))?;
    let mut writer = SmbWrite {
        runtime: provider.runtime.clone(),
        tree,
        connection,
        file_id: id,
        offset: 0,
        max_write: 65536,
        finished: false,
    };
    writer.write_all(value)?;
    writer.flush()
}

#[test]
#[ignore = "benötigt frischen privaten Samba mit optionalem acl_xattr"]
fn directory_dacl_actual_content_queue_roundtrip() -> io::Result<()> {
    let required = |name| std::env::var(name).expect("Privater Directory-DACL-Queuerunner fehlt");
    let physical = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(physical.starts_with("/tmp/doppeldecker-smb-acl-write-probe-20261029"));
    assert!(std::fs::read_dir(&physical)?.next().is_none());
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    assert_eq!(host, "127.0.0.1");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    let connection = SmbConnectionConfig {
        server_guid: discover_smb_server(&host, port)?.server_guid,
        host,
        port,
        share: required("DOPPELDECKER_SMB_TEST_SHARE"),
        username: required("DOPPELDECKER_SMB_TEST_USER"),
        domain: String::new(),
        requested_root: "/".into(),
        password_reference: crate::KeychainSecretReference::new(
            "Doppeldecker-Test",
            "directory-dacl-queue",
        )?,
    };
    let source = Arc::new(SmbVfs::connect(connection.clone(), Arc::new(TestSecrets))?);
    let target = Arc::new(SmbVfs::connect(connection, Arc::new(TestSecrets))?);
    for provider in [&source, &target] {
        let state = lock(&provider.state);
        assert!(state.session.identity.signing_active && state.session.identity.encryption_active);
    }
    assert_ne!(
        lock(&source.state).session.connection.session_id(),
        lock(&target.state).session.connection.session_id()
    );
    for dir in [
        "queue",
        "queue/source",
        "queue/target",
        "queue/source/sub",
        "queue/target/sub",
    ] {
        target.create_dir_unchecked(&Path::new("/").join(dir))?;
    }
    let source_root = Path::new("/queue/source");
    let target_root = Path::new("/queue/target");
    let source_dir = source_root.join("sub");
    let target_dir = target_root.join("sub");
    for dir in ["source", "target"] {
        std::fs::write(
            physical.join("queue").join(dir).join("sub/child"),
            b"same-child",
        )?;
    }
    let config = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let queue_path = config.path().join("queue.json");
    let queue = OperationQueue::open(&queue_path)?;
    let audit = config.path().join("audit.jsonl");
    let seed_owner = || {
        Some(InteractiveFileOwner {
            staging: crate::local_staging::StagingOwner {
                queue: config.path().join("seed-queue.json"),
                operation: "directory-queue-seed".into(),
            },
            profiles: vec![],
        })
    };
    let target_scope = target.content_sync_mutations(target_root, &Cancel::new(), seed_owner())?;
    target_scope.prepare_directory_dacl_write()?;
    let exact = std::env::var("DOPPELDECKER_SMB_STREAM_MODULE")
        .unwrap_or_default()
        .contains("acl_xattr");
    let context = match target_scope.directory_dacl_write_support()? {
        Support::Ready(context) => context,
        Support::Unavailable(Issue::Normalized) if !exact => {
            // Kontext dient ausschließlich dem künstlichen Seed, keine Writerfreigabe.
            let remote = target.remote_path(&target_dir)?;
            let (tree, mut connection) = {
                let state = lock(&target.state);
                (state.session.tree.clone(), state.session.connection.clone())
            };
            target.runtime.block_on(async {
                let id = wire::open(
                    &mut connection,
                    &tree,
                    &remote,
                    DIRECTORY_ACCESS,
                    true,
                    false,
                )
                .await?;
                let context = wire::context(
                    &wire::query(&mut connection, &tree, id, target.config.server_guid).await?,
                )?;
                tree.close_handle(&mut connection, id)
                    .await
                    .map_err(smb_error)?;
                Ok::<_, io::Error>(context)
            })?
        }
        support => panic!("Directory support: {support:?}"),
    };
    let mut before = probe_acl(&context);
    let WindowsDaclState::Acl { aces, .. } = &mut before.state else {
        panic!()
    };
    let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
        panic!()
    };
    *mask = 0x0017_01ff;
    let mut after = before.clone();
    let WindowsDaclState::Acl { aces, .. } = &mut after.state else {
        panic!()
    };
    aces.swap(1, 2);
    let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
        panic!()
    };
    *mask = DIRECTORY_ACCESS;
    for provider in [&source, &target] {
        let dir = if Arc::ptr_eq(provider, &source) {
            &source_dir
        } else {
            &target_dir
        };
        set_ads(provider, dir, b"initial-ads")?;
        seed_directory(provider, dir, &before, &context)?;
    }
    seed_directory(&source, &source_dir, &after, &context)?;
    let source_reader = source.content_sync_reader(source_root, &Cancel::new(), None)?;
    let target_reader = target.content_sync_reader(target_root, &Cancel::new(), None)?;
    let inode = target.stat(&target_dir)?.inode;
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
        let result = crate::execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &audit,
            &Cancel::new(),
            &mut |_| {},
        );
        // Wie der aufrufende Fensterworker frühe Fehler vor mark_running sichern.
        if let Err(error) = &result {
            if queue.snapshot().entries.last().unwrap().state == QueueState::Validating {
                queue.fail_coded(&entry.operation_id, crate::Progress::default(), error)?;
            }
        }
        result
    };
    let prepared = prepare()?;
    assert_eq!(
        prepared.preview().plan.copy_count + prepared.preview().plan.update_count,
        0
    );
    assert_eq!(
        prepared.preview().directory_dacl_effects.len(),
        usize::from(exact)
    );
    let QueueExecutionResult::Sync(stats) = execute(prepared)? else {
        panic!()
    };
    assert_eq!((stats.files_copied, stats.bytes_copied), (0, 0));
    assert_eq!(
        stats.directory_dacl_report.completed.len(),
        usize::from(exact)
    );
    assert_eq!(target.stat(&target_dir)?.inode, inode);
    println!("DIRECTORY_DACL_ACTUAL_QUEUE_ACL_ONLY_ZERO_FILES_BYTES_OK exact={exact}");
    let prepared = prepare()?;
    assert_eq!(prepared.preview().directory_dacl_effects.len(), 0);
    let QueueExecutionResult::Sync(stats) = execute(prepared)? else {
        panic!()
    };
    assert_eq!((stats.files_copied, stats.bytes_copied), (0, 0));
    assert!(stats.directory_dacl_report.is_empty());
    assert_eq!(stats.metadata_updated, 0);
    println!("DIRECTORY_DACL_ACTUAL_QUEUE_NOOP_ZERO_FILES_BYTES_METADATA_OK");
    for case in [
        "children-and-ads",
        "source-drift",
        "target-drift",
        "target-swap",
        "known-denied",
        "uncertain",
    ] {
        if !exact && case != "children-and-ads" {
            continue;
        }
        seed_directory(&source, &source_dir, &before, &context)?;
        seed_directory(&target, &target_dir, &before, &context)?;
        let bytes = format!("child-content-{case}");
        std::fs::write(physical.join("queue/source/sub/child"), bytes.as_bytes())?;
        set_ads(&source, &source_dir, case.as_bytes())?;
        seed_directory(&source, &source_dir, &after, &context)?;
        let prepared = prepare()?;
        assert_eq!(
            prepared.preview().plan.copy_count + prepared.preview().plan.update_count,
            1
        );
        assert_eq!(
            prepared.preview().directory_dacl_effects.len(),
            usize::from(exact)
        );
        if case == "target-swap" {
            let previous_child = std::fs::read(physical.join("queue/target/sub/child"))?;
            let previous_streams = target_reader.metadata_manifest(&target_dir)?.named_streams;
            let retained = target_root.join("sub-original");
            target.rename_noreplace_unchecked(&target_dir, &retained)?;
            target.create_dir_unchecked(&target_dir)?;
            let replacement = target_reader.metadata_manifest(&target_dir)?;
            let error = execute(prepared).unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
            assert!(crate::error::directory_dacl_report(&error).is_empty());
            assert!(target.list(&target_dir)?.is_empty());
            assert_eq!(target_reader.metadata_manifest(&target_dir)?, replacement);
            assert_eq!(
                std::fs::read(physical.join("queue/target/sub-original/child"))?,
                previous_child
            );
            assert_eq!(
                target_reader.metadata_manifest(&retained)?.named_streams,
                previous_streams
            );
            target.remove_dir_unchecked(&target_dir)?;
            target.rename_noreplace_unchecked(&retained, &target_dir)?;
            println!("DIRECTORY_DACL_ACTUAL_QUEUE_FOREIGN_SWAP_NO_CHILD_ADS_OR_SET_OK");
            continue;
        }
        if case.ends_with("-drift") {
            let previous_child = std::fs::read(physical.join("queue/target/sub/child"))?;
            let previous_streams = target_reader.metadata_manifest(&target_dir)?.named_streams;
            if case == "source-drift" {
                seed_directory(&source, &source_dir, &before, &context)?;
            } else {
                seed_directory(&target, &target_dir, &after, &context)?;
            }
            let error = execute(prepared).unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
            assert!(crate::error::directory_dacl_report(&error).is_empty());
            assert_eq!(
                std::fs::read(physical.join("queue/target/sub/child"))?,
                previous_child
            );
            assert_eq!(
                target_reader.metadata_manifest(&target_dir)?.named_streams,
                previous_streams
            );
            println!("DIRECTORY_DACL_ACTUAL_QUEUE_DRIFT_BEFORE_CHILD_AND_ADS_OK case={case}");
            continue;
        }
        TEST_FAULT.with(|fault| {
            fault.set(match case {
                "known-denied" => Some("before_open"),
                "uncertain" => Some("after_set"),
                _ => None,
            })
        });
        let result = execute(prepared);
        TEST_FAULT.with(|fault| fault.set(None));
        if case == "uncertain" {
            let error = result.unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            assert_eq!(
                crate::error::directory_dacl_report(&error).uncertain.len(),
                1
            );
            let snapshot = queue.snapshot();
            let entry = snapshot.entries.last().unwrap();
            assert_eq!(entry.state, QueueState::Failed);
            assert_eq!(entry.directory_dacl_report.uncertain.len(), 1);
            assert!(!entry.temp_recovery_paths.is_empty());
            println!("DIRECTORY_DACL_ACTUAL_QUEUE_UNCERTAIN_TYPED_PERSISTED_OK");
        } else {
            let QueueExecutionResult::Sync(stats) = result? else {
                panic!()
            };
            assert_eq!(stats.files_copied, 1);
            assert_eq!(
                stats.directory_dacl_report.completed.len(),
                usize::from(exact && case != "known-denied")
            );
            assert_eq!(
                queue.snapshot().entries.last().unwrap().state,
                QueueState::Completed
            );
            println!("DIRECTORY_DACL_ACTUAL_QUEUE_CHILD_COPY_DIRECTORY_ADS_BEFORE_DACL_OK case={case} exact={exact}");
        }
        assert_eq!(
            std::fs::read(physical.join("queue/target/sub/child"))?,
            bytes.as_bytes()
        );
        let mut ads = Vec::new();
        target_reader
            .open_named_stream_read(&target_dir, b"named")?
            .read_to_end(&mut ads)?;
        assert_eq!(ads, case.as_bytes());
        assert_eq!(target.stat(&target_dir)?.inode, inode);
        if exact && case != "known-denied" {
            assert_eq!(
                source_reader.metadata_manifest(&source_dir)?.acl,
                target_reader.metadata_manifest(&target_dir)?.acl
            );
        }
    }
    drop(queue);
    let reopened = OperationQueue::open(&queue_path)?;
    if exact {
        let snapshot = reopened.snapshot();
        let entry = snapshot.entries.last().unwrap();
        assert_eq!(entry.state, QueueState::Interrupted);
        assert_eq!(entry.directory_dacl_report.uncertain.len(), 1);
        assert!(entry.directory_dacl_recovery);
        let operation_id = entry.operation_id.clone();
        assert!(reopened.retry(&operation_id).is_err());
        assert!(reopened
            .acknowledge_unclear_outcome(&operation_id, true)
            .is_err());
        println!("DIRECTORY_DACL_ACTUAL_QUEUE_RESTART_UNCERTAIN_PRESERVED_OK");
        let endpoint = ProviderEndpoint::smb(target.clone(), target_root)?;
        let before = reopened.snapshot();
        let error = crate::directory_dacl_queue::acknowledge_directory_dacl_outcome(
            &reopened,
            &operation_id,
            &endpoint,
            false,
        )
        .unwrap_err();
        assert_eq!(
            crate::error_code(&error),
            crate::ErrorCode::ConfirmationMismatch
        );
        assert_eq!(reopened.snapshot(), before);
        let target_before = target_reader.metadata_manifest(&target_dir)?;
        let original_physical = physical.join("queue/target/sub");
        let retained_physical = physical.join("queue/target/sub-recovery-retained");
        std::fs::rename(&original_physical, &retained_physical)?;
        target.create_dir_unchecked(&target_dir)?;
        seed_directory(&target, &target_dir, &after, &context)?;
        let blocked = crate::directory_dacl_queue::acknowledge_directory_dacl_outcome(
            &reopened,
            &operation_id,
            &endpoint,
            true,
        );
        std::fs::remove_dir(&original_physical)?;
        std::fs::rename(&retained_physical, &original_physical)?;
        assert_eq!(
            crate::error_code(&blocked.unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(reopened.snapshot(), before);
        println!("DIRECTORY_DACL_ACTUAL_QUEUE_RECOVERY_FOREIGN_ID_SAME_DACL_BLOCKED_OK");
        crate::directory_dacl_queue::acknowledge_directory_dacl_outcome(
            &reopened,
            &operation_id,
            &endpoint,
            true,
        )?;
        assert_eq!(target_reader.metadata_manifest(&target_dir)?, target_before);
        assert_eq!(target.stat(&target_dir)?.inode, inode);
        let snapshot = reopened.snapshot();
        let entry = snapshot.entries.last().unwrap();
        assert_eq!(entry.state, QueueState::Cancelled);
        assert!(!entry.directory_dacl_recovery);
        assert_eq!(entry.directory_dacl_report.completed.len(), 1);
        assert!(entry.directory_dacl_report.uncertain.is_empty());
        assert!(entry.temp_recovery_paths.is_empty());
        drop(reopened);
        let reopened_again = OperationQueue::open(&queue_path)?;
        let snapshot = reopened_again.snapshot();
        let entry = snapshot.entries.last().unwrap();
        assert_eq!(entry.state, QueueState::Cancelled);
        assert_eq!(entry.directory_dacl_report.completed.len(), 1);
        assert!(!entry.directory_dacl_recovery);
        println!("DIRECTORY_DACL_ACTUAL_QUEUE_EXPLICIT_RECOVERY_NO_SET_TERMINAL_COMPLETED_REPORT_RELOAD_OK");
    }
    Ok(())
}
