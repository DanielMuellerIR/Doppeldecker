use super::*;
use crate::{
    Cancel, Compare, MetadataProfile, OperationQueue, ProviderEndpoint, QueueExecutionResult,
    SyncOptions,
};
use std::os::unix::fs::PermissionsExt;

#[test]
#[ignore = "frischer künstlicher Samba für Sparse-Writer und besitzgebundene Probe"]
fn sparse_target_first_actual_queue() -> io::Result<()> {
    let (provider, physical) = super::super::temp_dacl_flush::tests::fixture()?;
    std::fs::create_dir(physical.join("selected"))?;
    std::fs::write(physical.join("sentinel"), b"UNCHANGED")?;
    let area = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let root = area.path().canonicalize()?;
    let source = root.join("source");
    let config = root.join("config");
    std::fs::create_dir(&source)?;
    std::fs::create_dir(&config)?;
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700))?;
    let holes = std::fs::File::create(source.join("holes"))?;
    holes.set_len(1024 * 1024)?;
    holes.sync_all()?;
    let queue = OperationQueue::open(config.join("operations-queue.json"))?;
    CLOSES.with(|value| value.set(0));
    FLUSHES.with(|value| value.set(0));
    let prepared = crate::prepare_content_sync_for_queue(
        &queue,
        ProviderEndpoint::local(&source)?,
        ProviderEndpoint::smb(provider.clone(), Path::new("/selected"))?,
        SyncOptions {
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeBestEffort,
            ..Default::default()
        },
        &Cancel::new(),
    )?;
    assert!(prepared
        .preview()
        .plan
        .actions
        .iter()
        .any(|action| matches!(action,
        crate::Action::CopyFile { metadata: Some(value), .. } if value.sparse_extents)));
    assert_eq!(std::fs::read_dir(physical.join("selected"))?.count(), 0);
    queue.enqueue(prepared.queue_operation())?;
    let entry = queue.claim_next()?.unwrap();
    let QueueExecutionResult::Sync(stats) = crate::execute_prepared_content_sync_queue_entry(
        &queue,
        &entry,
        prepared,
        true,
        &config.join("audit.jsonl"),
        &Cancel::new(),
        &mut |_| {},
    )?
    else {
        panic!()
    };
    assert_eq!(stats.files_copied, 1);
    assert!(!stats
        .metadata_report
        .issues
        .iter()
        .any(|issue| issue.class == crate::MetadataClass::SparseExtents));
    assert_eq!(
        std::fs::read(physical.join("selected/holes"))?,
        vec![0; 1024 * 1024]
    );
    assert_eq!(
        crate::LocalVfs::new()
            .metadata_manifest(&physical.join("selected/holes"))?
            .sparse_extents,
        Some(vec![])
    );
    assert_eq!(std::fs::read_dir(physical.join("selected"))?.count(), 1);
    assert_eq!(std::fs::read(physical.join("sentinel"))?, b"UNCHANGED");
    println!(
        "SMB_SPARSE_TARGET_FIRST_ACTUAL_QUEUE_OK closes={} flushes={}",
        CLOSES.with(|v| v.get()),
        FLUSHES.with(|v| v.get())
    );
    Ok(())
}

#[test]
#[ignore = "frischer künstlicher Samba zur Eingrenzung des Metadatenabschlusses"]
fn sparse_target_finalization_stages() -> io::Result<()> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let (provider, physical) = super::super::temp_dacl_flush::tests::fixture()?;
    std::fs::create_dir(physical.join("selected"))?;
    let area = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let config = area.path().canonicalize()?;
    let owner = crate::remote_real_mode::InteractiveFileOwner {
        staging: crate::local_staging::StagingOwner {
            queue: config.join("queue.json"),
            operation: "sparse-stages".into(),
        },
        profiles: vec![],
    };
    let root = Path::new("/selected");
    let scope = provider.content_sync_mutations(root, &Cancel::new(), Some(owner))?;
    scope.prepare_sparse_write(crate::metadata::AclModel::Posix)?;
    let target = root.join("final");
    let staged = scope.staging_scope(&target, None)?.unwrap();
    let temp = staged.staging_candidate(&target, "tmp")?;
    let mut writer = staged.open_sparse_write_new(&temp)?;
    writer.set_len(1024 * 1024)?;
    writer.punch_hole(0, 1024 * 1024)?;
    writer.flush()?;
    drop(writer);
    let trace = |label: &str, path: &Path| -> io::Result<()> {
        let ranges = staged.metadata_manifest(path)?.sparse_extents;
        assert_eq!(ranges, Some(vec![]), "{label}");
        let actual = physical.join(path.strip_prefix("/").unwrap());
        println!(
            "SPARSE_FINALIZATION_STAGE label={label} blocks={} ranges={ranges:?}",
            std::fs::metadata(actual)?.blocks()
        );
        Ok(())
    };
    trace("closed", &temp)?;
    assert_eq!(data_write_status(&provider, &temp)?, NtStatus(0xc000_0043));
    let mut bytes = Vec::new();
    staged.open_read(&temp)?.read_to_end(&mut bytes)?;
    trace("after-content-read", &temp)?;
    staged.set_mtime(&temp, 1_700_000_000)?;
    trace("after-mtime", &temp)?;
    staged.sync_file(&temp)?;
    trace("after-sync-file", &temp)?;
    staged.rename_noreplace(&temp, &target)?;
    trace("after-rename", &target)?;
    staged.finish_staging()?;
    assert_eq!(data_write_status(&provider, &target)?, NtStatus::SUCCESS);
    assert_eq!(bytes, vec![0; 1024 * 1024]);
    Ok(())
}

fn secure_area() -> io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
}

fn queue_run(
    provider: &Arc<SmbVfs>,
    queue: &OperationQueue,
    source: &Path,
    target: &Path,
    config: &Path,
    mode: crate::SyncMode,
) -> io::Result<crate::SyncStats> {
    let prepared = crate::prepare_content_sync_for_queue(
        queue,
        ProviderEndpoint::local(source)?,
        ProviderEndpoint::smb(provider.clone(), target)?,
        SyncOptions {
            mode,
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeBestEffort,
            ..Default::default()
        },
        &Cancel::new(),
    )?;
    queue.enqueue(prepared.queue_operation())?;
    let entry = queue.claim_next()?.unwrap();
    let QueueExecutionResult::Sync(stats) = crate::execute_prepared_content_sync_queue_entry(
        queue,
        &entry,
        prepared,
        true,
        &config.join("audit.jsonl"),
        &Cancel::new(),
        &mut |_| {},
    )?
    else {
        panic!()
    };
    assert_eq!(
        queue.snapshot().entries.last().unwrap().state,
        crate::QueueState::Completed
    );
    Ok(stats)
}

fn punch_source_holes(file: &std::fs::File, size: u64, islands: &[(u64, usize)]) -> io::Result<()> {
    let mut holes = Vec::new();
    let mut start = 0;
    for &(offset, length) in islands {
        if offset > start {
            holes.push((start, offset));
        }
        start = offset + length as u64;
    }
    if start < size {
        holes.push((start, size.div_ceil(4096) * 4096));
    }
    for (start, end) in holes {
        #[cfg(target_os = "linux")]
        rustix::fs::fallocate(
            file,
            rustix::fs::FallocateFlags::PUNCH_HOLE | rustix::fs::FallocateFlags::KEEP_SIZE,
            start,
            end - start,
        )?;
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            let mut hole = libc::fpunchhole_t {
                fp_flags: 0,
                reserved: 0,
                fp_offset: start as i64,
                fp_length: (end - start) as i64,
            };
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &mut hole) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

fn seed_local(source: &Path) -> io::Result<()> {
    assert_eq!(
        crate::LocalVfs::new().sparse_allocation_unit(source)?,
        Some(4096)
    );
    use std::io::{Seek, SeekFrom, Write};
    for (name, size, islands) in [
        ("empty", 0, vec![]),
        ("holes", 1024 * 1024, vec![]),
        (
            "islands",
            1024 * 1024,
            vec![(4096, 4096), (131072, 4096), (524288, 4096)],
        ),
        ("eof-hole", 8193, vec![(0, 4096)]),
        ("eof-data", 8193, vec![(8192, 1)]),
        ("dense", 1024 * 1024, vec![(0, 1024 * 1024)]),
    ] {
        let mut file = std::fs::File::create(source.join(name))?;
        file.set_len(size)?;
        for &(offset, length) in &islands {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&vec![0x41; length])?;
        }
        file.sync_all()?;
        punch_source_holes(&file, size, &islands)?;
        file.sync_all()?;
        let expected = islands
            .iter()
            .map(|&(offset, length)| crate::metadata::DataExtent {
                offset,
                length: length as u64,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            crate::LocalVfs::new()
                .metadata_manifest(&source.join(name))?
                .sparse_extents,
            Some(expected),
            "actual sparse source {name}"
        );
        file.set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            ),
        )?;
    }
    std::fs::hard_link(source.join("islands"), source.join("alias-a"))?;
    std::fs::hard_link(source.join("islands"), source.join("alias-b"))?;
    Ok(())
}

#[test]
#[ignore = "frischer künstlicher Samba: lokale Sparse-Formen durch fünf echte Queuephasen"]
fn sparse_target_actual_queue_matrix() -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;
    let (provider, physical) = super::super::temp_dacl_flush::tests::fixture()?;
    std::fs::create_dir(physical.join("selected"))?;
    std::fs::write(physical.join("sentinel"), b"UNCHANGED")?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    let source = config.join("source");
    std::fs::create_dir(&source)?;
    seed_local(&source)?;
    let queue = OperationQueue::open(config.join("queue.json"))?;
    let target = Path::new("/selected");
    let reader = provider.content_sync_reader(target, &Cancel::new(), None)?;
    for (mode, label, copies) in [
        (crate::SyncMode::AddOnly, "add", 8),
        (crate::SyncMode::Update, "noop", 0),
        (crate::SyncMode::Update, "update", 3),
        (crate::SyncMode::Mirror, "mirror", 0),
        (crate::SyncMode::Update, "final-noop", 0),
    ] {
        if label == "update" {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(source.join("islands"))?;
            file.seek(SeekFrom::Start(4096))?;
            file.write_all(b"NEW")?;
            file.sync_all()?;
            file.set_times(std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_005),
            ))?;
        }
        if label == "mirror" {
            std::fs::write(physical.join("selected/extra"), b"EXTRA")?;
        }
        let stats = queue_run(&provider, &queue, &source, target, &config, mode)?;
        assert_eq!(stats.files_copied, copies, "{label}");
        assert!(
            !stats
                .metadata_report
                .issues
                .iter()
                .any(|issue| issue.class == crate::MetadataClass::SparseExtents),
            "{label}: {:?}",
            stats.metadata_report
        );
        assert!(stats
            .metadata_report
            .issues
            .iter()
            .any(|issue| issue.class == crate::MetadataClass::Hardlinks));
        for name in [
            "empty", "holes", "islands", "eof-hole", "eof-data", "dense", "alias-a", "alias-b",
        ] {
            let original = source.join(name);
            let actual = physical.join("selected").join(name);
            assert_eq!(
                std::fs::read(&actual)?,
                std::fs::read(&original)?,
                "{label}/{name}"
            );
            let expected = crate::LocalVfs::new()
                .metadata_manifest(&original)?
                .sparse_extents;
            assert_eq!(
                reader.metadata_manifest(&target.join(name))?.sparse_extents,
                expected,
                "server {label}/{name}"
            );
            assert_eq!(
                crate::LocalVfs::new()
                    .metadata_manifest(&actual)?
                    .sparse_extents,
                expected,
                "physical {label}/{name}"
            );
            assert_eq!(std::fs::metadata(&actual)?.nlink(), 1);
        }
        assert_ne!(
            std::fs::metadata(physical.join("selected/islands"))?.ino(),
            std::fs::metadata(physical.join("selected/alias-a"))?.ino()
        );
        assert_eq!(std::fs::read_dir(physical.join("selected"))?.count(), 8);
        assert_eq!(std::fs::read(physical.join("sentinel"))?, b"UNCHANGED");
        println!(
            "SMB_SPARSE_TARGET_QUEUE_MATRIX_OK phase={label} files={} logical_bytes={}",
            stats.files_copied, stats.bytes_copied
        );
    }
    Ok(())
}

fn target_scope(
    provider: &Arc<SmbVfs>,
    root: &Path,
    config: &Path,
    cancel: &Cancel,
) -> io::Result<Arc<dyn Vfs>> {
    let owner = crate::remote_real_mode::InteractiveFileOwner {
        staging: crate::local_staging::StagingOwner {
            queue: config.join("queue.json"),
            operation: "sparse-lifecycle".into(),
        },
        profiles: vec![],
    };
    provider.content_sync_mutations(root, cancel, Some(owner))
}
fn counters() -> (usize, usize, usize, usize) {
    (
        CLOSES.with(|v| v.get()),
        FLUSHES.with(|v| v.get()),
        ATTRIBUTE_CLOSES.with(|v| v.get()),
        RESTORES.with(|v| v.get()),
    )
}
fn reset_counters() {
    CLOSES.with(|v| v.set(0));
    FLUSHES.with(|v| v.set(0));
    ATTRIBUTE_CLOSES.with(|v| v.set(0));
    RESTORES.with(|v| v.set(0));
    METADATA_RESTORES.with(|v| v.set(0));
}

#[test]
#[ignore = "frischer künstlicher Samba: einmaliger Sparse-Tempabschluss bei Antwortverlust"]
fn sparse_target_writer_lifecycle() -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let (provider, physical) = super::super::temp_dacl_flush::tests::fixture()?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    for (index, point) in [
        "success",
        "write-before",
        "write-after",
        "data-flush-before",
        "data-flush-after",
        "close-after",
        "grant-before",
        "metadata-flush-before",
        "metadata-flush-after",
        "abandon",
        "cancel",
        "close-before",
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("case-{index}");
        std::fs::create_dir(physical.join(&name))?;
        let local_config = config.join(&name);
        std::fs::create_dir(&local_config)?;
        std::fs::set_permissions(&local_config, std::fs::Permissions::from_mode(0o700))?;
        let root = Path::new("/").join(&name);
        let cancel = Cancel::new();
        let scope = target_scope(&provider, &root, &local_config, &cancel)?;
        scope.prepare_sparse_write(crate::metadata::AclModel::Posix)?;
        let target = root.join("final");
        let staged = scope.staging_scope(&target, None)?.unwrap();
        let temp = staged.staging_candidate(&target, "tmp")?;
        let mut writer = staged.open_sparse_write_new(&temp)?;
        writer.set_len(65536)?;
        reset_counters();
        if point == "abandon" {
            drop(writer);
            assert_eq!(counters(), (1, 0, 0, 0));
            staged.cleanup_owned_temp(&temp)?;
            assert!(std::fs::read_dir(physical.join(&name))?.next().is_none());
        } else if point == "success" {
            writer.punch_hole(0, 65536)?;
            writer.flush()?;
            writer.flush()?;
            assert_eq!(counters(), (1, 1, 0, 0));
            assert!(writer.write(b"x").is_err());
            assert!(writer.seek(SeekFrom::Start(0)).is_err());
            assert!(writer.set_len(1).is_err());
            assert!(writer.punch_hole(0, 1).is_err());
            drop(writer);
            // Der reine Datenabschluss reicht für den Rename noch nicht.
            assert!(staged.rename_noreplace(&temp, &target).is_err());
            staged.sync_file(&temp)?;
            staged.sync_file(&temp)?;
            assert!(staged.set_mtime(&temp, 1_700_000_099).is_err());
            staged.rename_noreplace(&temp, &target)?;
            staged.finish_staging()?;
            assert_eq!(counters(), (1, 1, 1, 1));
            assert_eq!(
                std::fs::read(physical.join(&name).join("final"))?,
                vec![0; 65536]
            );
        } else {
            let error = if point.starts_with("metadata-") {
                writer.flush()?;
                drop(writer);
                FAULT.with(|v| v.set(Some(point)));
                staged.sync_file(&temp).unwrap_err()
            } else {
                if point == "cancel" {
                    cancel.cancel();
                } else {
                    FAULT.with(|v| v.set(Some(point)));
                }
                let error = if point.starts_with("write-") {
                    writer.write(b"x").unwrap_err()
                } else {
                    writer.flush().unwrap_err()
                };
                let before = counters();
                assert!(writer.flush().is_err());
                assert!(writer.write(b"x").is_err());
                assert!(writer.seek(SeekFrom::Start(0)).is_err());
                assert!(writer.set_len(1).is_err());
                assert!(writer.punch_hole(0, 1).is_err());
                drop(writer);
                assert_eq!(counters(), before, "{point}");
                error
            };
            let before = counters();
            assert!(staged.rename_noreplace(&temp, &target).is_err());
            assert!(staged.cleanup_owned_temp(&temp).is_err());
            assert_eq!(counters(), before, "{point}");
            assert_eq!(before.0, 1, "{point}: {error}");
            assert!(!physical.join(&name).join("final").exists());
            assert!(staged.staging_recovery_path().unwrap().is_file());
            FAULT.with(|v| assert!(v.get().is_none(), "unused {point}"));
        }
        println!(
            "SMB_SPARSE_TARGET_LIFECYCLE_OK case={point} counters={:?}",
            counters()
        );
    }
    Ok(())
}

#[test]
#[ignore = "frischer künstlicher Samba: Queuefehler vor und nach Sparse-Rename behalten Teilwirkungen"]
fn sparse_target_queue_commit_failures() -> io::Result<()> {
    let (provider, physical) = super::super::temp_dacl_flush::tests::fixture()?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    for (index, point) in [
        "metadata-flush-after",
        "restore-before",
        "restore-after",
        "attribute-flush-before",
        "attribute-flush-after",
        "attribute-close-after",
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("case-{index}");
        let root = Path::new("/").join(&name);
        std::fs::create_dir(physical.join(&name))?;
        std::fs::write(physical.join(&name).join("holes"), b"OLD")?;
        std::fs::write(physical.join(&name).join("extra"), b"EXTRA")?;
        let local_config = config.join(&name);
        std::fs::create_dir(&local_config)?;
        std::fs::set_permissions(&local_config, std::fs::Permissions::from_mode(0o700))?;
        let source = local_config.join("source");
        std::fs::create_dir(&source)?;
        let file = std::fs::File::create(source.join("holes"))?;
        file.set_len(65536)?;
        file.sync_all()?;
        let queue = OperationQueue::open(local_config.join("queue.json"))?;
        reset_counters();
        FAULT.with(|v| v.set(Some(point)));
        let error = queue_run(
            &provider,
            &queue,
            &source,
            &root,
            &local_config,
            crate::SyncMode::Mirror,
        )
        .unwrap_err();
        FAULT.with(|v| assert!(v.get().is_none(), "unused {point}"));
        assert_eq!(
            queue.snapshot().entries.last().unwrap().state,
            crate::QueueState::Failed
        );
        assert!(
            !crate::error::recovery_paths(&error).is_empty(),
            "{point}: {error}"
        );
        assert_eq!(std::fs::read(physical.join(&name).join("extra"))?, b"EXTRA");
        assert_eq!(
            std::fs::read(physical.join(&name).join("holes"))?,
            if point == "metadata-flush-after" {
                b"OLD".to_vec()
            } else {
                vec![0; 65536]
            }
        );
        let before = counters();
        let blocked = queue_run(
            &provider,
            &queue,
            &source,
            &root,
            &local_config,
            crate::SyncMode::Mirror,
        )
        .unwrap_err();
        assert!(!crate::error::recovery_paths(&blocked).is_empty());
        assert_eq!(
            counters(),
            before,
            "pending intent must block before new probe: {point}"
        );
        println!(
            "SMB_SPARSE_TARGET_QUEUE_FAILURE_OK case={point} code={:?} counters={before:?}",
            crate::error_code(&error)
        );
    }
    Ok(())
}

fn data_write_status(provider: &SmbVfs, path: &Path) -> io::Result<NtStatus> {
    let remote = provider.remote_path(path)?;
    let session = lock(&provider.state);
    let tree = session.session.tree.clone();
    let mut connection = session.session.connection.clone();
    drop(session);
    provider.runtime.block_on(async {
        let frame = connection
            .execute(
                Command::Create,
                &CreateRequest {
                    requested_oplock_level: OplockLevel::None,
                    impersonation_level: ImpersonationLevel::Impersonation,
                    desired_access: FileAccessMask::new(FileAccessMask::FILE_WRITE_DATA),
                    file_attributes: FILE_ATTRIBUTE_NORMAL,
                    share_access: ShareAccess(
                        ShareAccess::FILE_SHARE_READ
                            | ShareAccess::FILE_SHARE_WRITE
                            | ShareAccess::FILE_SHARE_DELETE,
                    ),
                    create_disposition: CreateDisposition::FileOpen,
                    create_options: FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
                    name: remote,
                    create_contexts: vec![],
                },
                Some(tree.tree_id),
            )
            .await
            .map_err(smb_error)?;
        if frame.header.status == NtStatus::SUCCESS {
            let id = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
                .map_err(smb_error)?
                .file_id;
            tree.close_handle(&mut connection, id)
                .await
                .map_err(smb_error)?;
        } else {
            ErrorResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
        }
        Ok(frame.header.status)
    })
}

#[test]
#[ignore = "frischer künstlicher Samba: keine Sparse-Probe für andere Ausführungsklassen"]
fn sparse_target_capability_boundaries() -> io::Result<()> {
    let (provider, physical) = super::super::temp_dacl_flush::tests::fixture()?;
    std::fs::create_dir(physical.join("selected"))?;
    let root = Path::new("/selected");
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    let scope = target_scope(&provider, root, &config, &Cancel::new())?;
    reset_counters();
    scope.prepare_sparse_write(crate::metadata::AclModel::WindowsSid)?;
    assert_eq!(
        scope.metadata_capabilities(root)?.sparse_extents,
        Capability::Unsupported
    );
    assert_eq!(counters(), (0, 0, 0, 0));
    let source = config.join("source");
    std::fs::create_dir(&source)?;
    let holes = std::fs::File::create(source.join("holes"))?;
    holes.set_len(65536)?;
    holes.sync_all()?;
    let queue = OperationQueue::open(config.join("queue.json"))?;
    for profile in [MetadataProfile::Portable, MetadataProfile::NativeStrict] {
        let result = crate::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::local(&source)?,
            ProviderEndpoint::smb(provider.clone(), root)?,
            SyncOptions {
                compare: Compare::ContentHash,
                metadata: profile,
                ..Default::default()
            },
            &Cancel::new(),
        );
        if profile == MetadataProfile::NativeStrict {
            assert!(result.is_err());
        } else {
            result?;
        }
        assert_eq!(counters(), (0, 0, 0, 0));
        assert!(std::fs::read_dir(physical.join("selected"))?
            .next()
            .is_none());
    }
    println!("SMB_SPARSE_TARGET_WINDOWS_SID_PORTABLE_STRICT_NO_PROBE_OK");
    for (index, point) in ["probe-selected-stat", "probe-profile-stat"]
        .into_iter()
        .enumerate()
    {
        let name = format!("failed-probe-{index}");
        std::fs::create_dir(physical.join(&name))?;
        let private = config.join(&name);
        std::fs::create_dir(&private)?;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))?;
        let selected = Path::new("/").join(&name);
        let scope = target_scope(&provider, &selected, &private, &Cancel::new())?;
        reset_counters();
        PROBE_STATS.with(|v| v.set(0));
        FAULT.with(|v| v.set(Some(point)));
        let error = scope
            .prepare_sparse_write(crate::metadata::AclModel::Posix)
            .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(!crate::error::recovery_paths(&error).is_empty());
        assert_eq!(PROBE_STATS.with(|v| v.get()), 2);
        let before = counters();
        assert!(scope
            .prepare_sparse_write(crate::metadata::AclModel::Posix)
            .is_err());
        assert_eq!(counters(), before);
        assert_eq!(PROBE_STATS.with(|v| v.get()), 2);
        assert!(std::fs::read_dir(physical.join(&name))?.next().is_none());
        println!("SMB_SPARSE_TARGET_PROBE_STAT_ERROR_RETAINED_ONCE_OK case={point}");
    }
    let name = "readonly";
    let directory = physical.join(name);
    std::fs::create_dir(&directory)?;
    let selected = Path::new("/").join(name);
    let scope = target_scope(&provider, &selected, &config, &Cancel::new())?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o555))?;
    let denied = (|| {
        scope.prepare_sparse_write(crate::metadata::AclModel::Posix)?;
        assert_eq!(
            scope.metadata_capabilities(&selected)?.sparse_extents,
            Capability::Unsupported
        );
        assert!(std::fs::read_dir(&directory)?.next().is_none());
        Ok::<(), io::Error>(())
    })();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755))?;
    denied?;
    println!("SMB_SPARSE_TARGET_ACCESS_DENIED_IS_PREPLAN_UNSUPPORTED_OK");
    Ok(())
}

#[cfg(target_os = "macos")]
pub(in crate::smb::interactive) fn mac_bridge_fixture() -> io::Result<(Arc<SmbVfs>, String)> {
    use std::io::Read;
    let port: u16 = std::env::var("DOPPELDECKER_SPARSE_BRIDGE_PORT")
        .expect("Privater Tunnel fehlt")
        .parse()
        .unwrap();
    assert!(port > 1024);
    let marker =
        std::env::var("DOPPELDECKER_SPARSE_BRIDGE_MARKER").expect("Künstlicher Besitzmarker fehlt");
    assert!(marker.starts_with("DOPPELDECKER-SPARSE-BRIDGE-"));
    assert_eq!(marker.len(), 59);
    let host = "127.0.0.1";
    let provider = Arc::new(SmbVfs::connect(
        SmbConnectionConfig {
            server_guid: discover_smb_server(host, port)?.server_guid,
            host: host.into(),
            port,
            share: "DDTEST".into(),
            username: std::env::var("DOPPELDECKER_SPARSE_BRIDGE_USER")
                .expect("Künstlicher Servernutzer fehlt"),
            domain: String::new(),
            requested_root: "/".into(),
            password_reference: crate::KeychainSecretReference::new(
                "Doppeldecker-Test",
                "sparse-bridge",
            )?,
        },
        Arc::new(super::super::temp_dacl_flush::tests::TestSecrets),
    )?);
    let mut names: Vec<_> = provider
        .list(Path::new("/"))?
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["selected", "sentinel"]);
    let mut bytes = Vec::new();
    provider
        .open_read(Path::new("/sentinel"))?
        .read_to_end(&mut bytes)?;
    assert_eq!(bytes, marker.as_bytes());
    let target = Path::new("/selected");
    assert!(provider.list(target)?.is_empty());
    Ok((provider, marker))
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "privater Loopbacktunnel zu frischem künstlichem Samba, echter APFS-Quellbestand"]
fn sparse_target_macos_actual_queue() -> io::Result<()> {
    use sha2::Digest;
    use std::io::{Read, Seek, SeekFrom, Write};
    let (provider, marker) = mac_bridge_fixture()?;
    let target = Path::new("/selected");
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    let source = config.join("source");
    std::fs::create_dir(&source)?;
    seed_local(&source)?;
    let queue = OperationQueue::open(config.join("queue.json"))?;
    let reader = provider.content_sync_reader(target, &Cancel::new(), None)?;
    for (mode, label, copies) in [
        (crate::SyncMode::AddOnly, "add", 8),
        (crate::SyncMode::Update, "noop", 0),
        (crate::SyncMode::Update, "update", 3),
        (crate::SyncMode::Mirror, "mirror", 0),
        (crate::SyncMode::Update, "final-noop", 0),
    ] {
        if label == "update" {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(source.join("islands"))?;
            file.seek(SeekFrom::Start(4096))?;
            file.write_all(b"NEW")?;
            file.sync_all()?;
            file.set_times(std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_005),
            ))?;
        }
        if label == "mirror" {
            let scope = provider.ordinary_mutations(target, &Cancel::new(), None)?;
            let mut writer = scope.open_write_new(&target.join("extra"))?;
            writer.write_all(b"EXTRA")?;
            writer.flush()?;
        }
        let stats = queue_run(&provider, &queue, &source, target, &config, mode)?;
        assert_eq!(stats.files_copied, copies, "{label}");
        assert!(
            !stats
                .metadata_report
                .issues
                .iter()
                .any(|issue| issue.class == crate::MetadataClass::SparseExtents),
            "{label}: {:?}",
            stats.metadata_report
        );
        assert_eq!(
            stats
                .metadata_report
                .issues
                .iter()
                .filter(|issue| issue.class == crate::MetadataClass::Hardlinks)
                .count(),
            3
        );
        assert_eq!(provider.list(target)?.len(), 8);
        for name in [
            "empty", "holes", "islands", "eof-hole", "eof-data", "dense", "alias-a", "alias-b",
        ] {
            let original = source.join(name);
            let mut actual = Vec::new();
            reader
                .open_read(&target.join(name))?
                .read_to_end(&mut actual)?;
            assert_eq!(actual, std::fs::read(&original)?, "{label}/{name}");
            let expected = crate::LocalVfs::new()
                .metadata_manifest(&original)?
                .sparse_extents;
            assert_eq!(
                reader.metadata_manifest(&target.join(name))?.sparse_extents,
                expected,
                "{label}/{name}"
            );
            if label == "final-noop" {
                println!(
                    "SMB_SPARSE_MAC_EXPECTED {}",
                    serde_json::json!({"name":name,"size":actual.len(),"blake3":blake3::hash(&actual).to_hex().to_string(),"sha256":format!("{:x}", sha2::Sha256::digest(&actual)),"extents":expected})
                );
            }
        }
        let mut final_marker = Vec::new();
        provider
            .open_read(Path::new("/sentinel"))?
            .read_to_end(&mut final_marker)?;
        assert_eq!(final_marker, marker.as_bytes());
        println!("SMB_SPARSE_MAC_ACTUAL_QUEUE_OK phase={label} copies={copies}");
    }
    Ok(())
}

#[path = "smb_sparse_dacl_tests.rs"]
pub(in crate::smb::interactive) mod joint;

#[test]
#[ignore = "frischer künstlicher Samba: No-Replace-Kollision hält eigene Regular-/Sparse-Temps bereinigbar"]
fn noreplace_collision_keeps_known_temp_and_does_not_poison_namespace() -> io::Result<()> {
    use crate::vfs::MutationFailure;
    use std::io::Write;
    let (provider, physical) = super::super::temp_dacl_flush::tests::fixture()?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    for sparse in [false, true] {
        let name = if sparse { "sparse" } else { "regular" };
        std::fs::create_dir(physical.join(name))?;
        let private = config.join(name);
        std::fs::create_dir(&private)?;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))?;
        let queue = OperationQueue::open(private.join("queue.json"))?;
        let root = Path::new("/").join(name);
        let scope = target_scope(&provider, &root, &private, &Cancel::new())?;
        if sparse {
            scope.prepare_sparse_write(crate::metadata::AclModel::Posix)?;
        }
        let target = root.join("final");
        let staged = scope.staging_scope(&target, None)?.unwrap();
        let temp = staged.staging_candidate(&target, "tmp")?;
        if sparse {
            let mut writer = staged.open_sparse_write_new(&temp)?;
            writer.set_len(65536)?;
            writer.punch_hole(0, 65536)?;
            writer.flush()?;
        } else {
            let mut writer = staged.open_write_new(&temp)?;
            writer.write_all(b"owned temporary bytes")?;
            writer.flush()?;
        }
        staged.sync_file(&temp)?;
        std::fs::write(physical.join(name).join("final"), b"late foreign target")?;
        match staged.rename_noreplace_with_removal_proof(&temp, &target) {
            Err(MutationFailure::Unchanged(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists)
            }
            other => panic!("Kollision verlor den Unverändert-Beleg: {other:?}"),
        }
        assert_eq!(
            std::fs::read(physical.join(name).join("final"))?,
            b"late foreign target"
        );
        assert!(physical.join(temp.strip_prefix("/").unwrap()).is_file());
        staged.cleanup_owned_temp(&temp)?;
        staged.finish_staging()?;
        assert!(!physical.join(temp.strip_prefix("/").unwrap()).exists());
        let identity = scope.root_identity(&root)?;
        let binding =
            crate::EndpointBinding::remote(&identity, provider.identity().account_digest)?;
        let scopes = [(&binding, Some(&identity), root.as_path())];
        assert!(
            crate::local_staging::namespace_staging_view(&private, &scopes)?
                .candidates
                .is_empty()
        );
        drop(queue);
        let reopened = OperationQueue::open(private.join("queue.json"))?;
        assert!(reopened.snapshot().entries.is_empty());
    }
    Ok(())
}
