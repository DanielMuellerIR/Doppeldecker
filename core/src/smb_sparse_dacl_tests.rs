use super::*;
use crate::smb::interactive::temp_dacl_flush::tests as fixtures;

fn mark_joint_source(provider: &Arc<SmbVfs>, path: &Path) -> io::Result<()> {
    let remote = provider.remote_path(path)?;
    let session = lock(&provider.state);
    let tree = session.session.tree.clone();
    let mut connection = session.session.connection.clone();
    drop(session);
    provider.runtime.block_on(async {
        let id = wire::open(
            &mut connection,
            &tree,
            &remote,
            FileAccessMask::FILE_WRITE_ATTRIBUTES
                | FileAccessMask::FILE_READ_ATTRIBUTES
                | FileAccessMask::SYNCHRONIZE,
            false,
            false,
        )
        .await?;
        let result = async {
            let request = IoctlRequest {
                ctl_code: SET_SPARSE,
                file_id: id,
                max_input_response: 0,
                max_output_response: 0,
                flags: SMB2_0_IOCTL_IS_FSCTL,
                input_data: vec![1],
            };
            let frame = connection
                .execute(Command::Ioctl, &request, Some(tree.tree_id))
                .await
                .map_err(smb_error)?;
            status(&frame, Command::Ioctl)?;
            let response =
                IoctlResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
            assert_eq!(response.ctl_code, SET_SPARSE);
            assert_eq!(response.file_id, id);
            Ok::<(), io::Error>(())
        }
        .await;
        let closed = tree
            .close_handle(&mut connection, id)
            .await
            .map_err(smb_error);
        result.and(closed)
    })
}

#[test]
#[ignore = "frischer künstlicher Samba: begrenztes gemeinsames Sparse-/Daten-Deny-DACL-Anbindungsgate"]
fn sparse_windows_dacl_actual_binding_gate() -> io::Result<()> {
    let (provider, physical) = fixtures::fixture()?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    std::fs::write(physical.join("sentinel"), b"UNCHANGED")?;
    for (index, islands) in [vec![], vec![(4096, 4096)]].into_iter().enumerate() {
        let name = format!("case-{index}");
        std::fs::create_dir_all(physical.join(&name).join("source"))?;
        std::fs::create_dir(physical.join(&name).join("target"))?;
        let private = config.join(&name);
        std::fs::create_dir(&private)?;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))?;
        let source = Path::new("/").join(&name).join("source");
        let target = Path::new("/").join(&name).join("target");
        let mut bytes = vec![0; 1024 * 1024];
        for &(offset, length) in &islands {
            bytes[offset as usize..offset as usize + length].fill(0x41);
        }
        let (acl, context) = fixtures::seed(&provider, &source, &private, &bytes, 6)?;
        let source_path = source.join("file.bin");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(physical.join(&name).join("source/file.bin"))?;
        punch_source_holes(&file, bytes.len() as u64, &islands)?;
        file.sync_all()?;
        file.set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_003),
            ),
        )?;
        drop(file);
        println!("JOINT_BINDING_STAGE case={index} source-seeded");
        mark_joint_source(&provider, &source_path)?;
        println!("JOINT_BINDING_STAGE case={index} source-marked");
        let reader = provider.content_sync_reader(&source, &Cancel::new(), None)?;
        let expected = reader.metadata_manifest(&source_path)?.sparse_extents;
        assert_eq!(
            expected,
            Some(
                islands
                    .iter()
                    .map(|&(offset, length)| crate::metadata::DataExtent {
                        offset,
                        length: length as u64
                    })
                    .collect()
            )
        );
        println!("JOINT_BINDING_STAGE case={index} source-observed");
        let queue = OperationQueue::open(private.join("queue.json"))?;
        reset_counters();
        let prepared = crate::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::smb(provider.clone(), &source)?,
            ProviderEndpoint::smb(provider.clone(), &target)?,
            SyncOptions {
                compare: Compare::ContentHash,
                metadata: MetadataProfile::NativeBestEffort,
                ..Default::default()
            },
            &Cancel::new(),
        )?;
        println!("JOINT_BINDING_STAGE case={index} prepared");
        assert!(prepared.preview().plan.actions.iter().any(|action|matches!(action,crate::Action::CopyFile{metadata:Some(metadata),..} if metadata.sparse_extents && metadata.acl && metadata.named_streams)));
        queue.enqueue(prepared.queue_operation())?;
        let entry = queue.claim_next()?.unwrap();
        let QueueExecutionResult::Sync(stats) = crate::execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &private.join("audit.jsonl"),
            &Cancel::new(),
            &mut |_| {},
        )?
        else {
            panic!()
        };
        assert_eq!(stats.files_copied, 1);
        assert!(
            !stats.metadata_report.issues.iter().any(|issue| matches!(
                issue.class,
                crate::MetadataClass::SparseExtents
                    | crate::MetadataClass::Acl
                    | crate::MetadataClass::NamedStreams
            )),
            "{:?}",
            stats.metadata_report
        );
        let final_path = target.join("file.bin");
        fixtures::verify_file(
            &provider,
            &target,
            &final_path,
            &bytes,
            &acl,
            &context,
            provider.stat(&final_path)?.inode.unwrap(),
        )?;
        let target_reader = provider.content_sync_reader(&target, &Cancel::new(), None)?;
        assert_eq!(
            target_reader.metadata_manifest(&final_path)?.sparse_extents,
            expected
        );
        assert_eq!(
            crate::LocalVfs::new()
                .metadata_manifest(&physical.join(&name).join("target/file.bin"))?
                .sparse_extents,
            expected
        );
        assert_eq!(
            std::fs::read_dir(physical.join(&name).join("target"))?.count(),
            1
        );
        assert_eq!(std::fs::read(physical.join("sentinel"))?, b"UNCHANGED");
        assert_eq!(counters(), (3, 3, 1, 1));
        println!(
            "SMB_SPARSE_WINDOWS_DACL_BINDING_ACTUAL_QUEUE_OK case={index} counters={:?}",
            counters()
        );
    }
    Ok(())
}

fn seed_joint(
    provider: &Arc<SmbVfs>,
    physical_source: &Path,
    source: &Path,
    config: &Path,
    bytes: &[u8],
    islands: &[(u64, usize)],
    mask: u32,
) -> io::Result<(
    crate::windows_security::WindowsDacl,
    crate::windows_dacl::WindowsDaclWriteContext,
)> {
    let result = fixtures::seed(provider, source, config, bytes, mask)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(physical_source.join("file.bin"))?;
    punch_source_holes(&file, bytes.len() as u64, islands)?;
    file.sync_all()?;
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_003)),
    )?;
    drop(file);
    mark_joint_source(provider, &source.join("file.bin"))?;
    let observed = provider
        .content_sync_reader(source, &Cancel::new(), None)?
        .metadata_manifest(&source.join("file.bin"))?
        .sparse_extents;
    assert_eq!(
        observed,
        Some(
            islands
                .iter()
                .map(|&(offset, length)| crate::metadata::DataExtent {
                    offset,
                    length: length as u64
                })
                .collect()
        )
    );
    Ok(result)
}
fn joint_options(mode: crate::SyncMode) -> SyncOptions {
    SyncOptions {
        mode,
        compare: Compare::ContentHash,
        metadata: MetadataProfile::NativeBestEffort,
        ..Default::default()
    }
}
fn prepare_joint(
    provider: &Arc<SmbVfs>,
    queue: &OperationQueue,
    source: &Path,
    target: &Path,
    mode: crate::SyncMode,
) -> io::Result<crate::PreparedContentSync> {
    crate::prepare_content_sync_for_queue(
        queue,
        ProviderEndpoint::smb(provider.clone(), source)?,
        ProviderEndpoint::smb(provider.clone(), target)?,
        joint_options(mode),
        &Cancel::new(),
    )
}
fn execute_joint(
    queue: &OperationQueue,
    prepared: crate::PreparedContentSync,
    config: &Path,
) -> io::Result<crate::SyncStats> {
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
fn assert_joint_report(stats: &crate::SyncStats) {
    assert!(
        !stats.metadata_report.issues.iter().any(|issue| matches!(
            issue.class,
            crate::MetadataClass::SparseExtents
                | crate::MetadataClass::Acl
                | crate::MetadataClass::NamedStreams
        )),
        "{:?}",
        stats.metadata_report
    );
}
#[test]
#[ignore = "frischer künstlicher Samba: gemeinsame Sparse-/DACL-/ADS-Queue mit fünf Rechtemasken"]
fn sparse_windows_dacl_actual_queue_matrix() -> io::Result<()> {
    let (provider, physical) = fixtures::fixture()?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    std::fs::write(physical.join("sentinel"), b"UNCHANGED")?;
    for mask in [0, 2, 4, 6, 0x0008_0006] {
        let name = format!("mask-{mask:x}");
        let local = physical.join(&name);
        std::fs::create_dir_all(local.join("source"))?;
        std::fs::create_dir(local.join("target"))?;
        let source = Path::new("/").join(&name).join("source");
        let target = Path::new("/").join(&name).join("target");
        let private = config.join(&name);
        std::fs::create_dir(&private)?;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))?;
        let mut bytes = vec![0; 1024 * 1024];
        let mut islands = vec![];
        let (mut acl, mut context) = seed_joint(
            &provider,
            &local.join("source"),
            &source,
            &private,
            &bytes,
            &islands,
            mask,
        )?;
        let queue = OperationQueue::open(private.join("queue.json"))?;
        for (mode, phase) in [
            (crate::SyncMode::AddOnly, "add"),
            (crate::SyncMode::Update, "noop"),
            (crate::SyncMode::Update, "replace"),
            (crate::SyncMode::Update, "acl-only"),
            (crate::SyncMode::Mirror, "mirror"),
            (crate::SyncMode::Update, "final-noop"),
        ] {
            if phase == "replace" {
                bytes[4096..8192].fill(0x41);
                islands = vec![(4096, 4096)];
                (acl, context) = seed_joint(
                    &provider,
                    &local.join("source"),
                    &source,
                    &private,
                    &bytes,
                    &islands,
                    mask,
                )?;
            }
            if phase == "acl-only" {
                (acl, context) = seed_joint(
                    &provider,
                    &local.join("source"),
                    &source,
                    &private,
                    &bytes,
                    &islands,
                    if mask == 6 { 4 } else { 6 },
                )?;
            }
            if phase == "mirror" {
                std::fs::write(local.join("target/extra"), b"EXTRA")?;
            }
            let old = provider
                .stat(&target.join("file.bin"))
                .ok()
                .and_then(|entry| entry.inode);
            reset_counters();
            let prepared = prepare_joint(&provider, &queue, &source, &target, mode)?;
            if matches!(phase, "add" | "replace") {
                let provider = provider.clone();
                let actual = local.join("target");
                let root = target.clone();
                wire::TEST_BEFORE_SET.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        let name = std::fs::read_dir(&actual)
                            .unwrap()
                            .map(|entry| entry.unwrap().file_name())
                            .find(|name| {
                                name.to_string_lossy()
                                    .starts_with(".file.bin.doppeldecker-tmp-")
                            })
                            .unwrap();
                        assert_eq!(
                            data_write_status(&provider, &root.join(name)).unwrap(),
                            NtStatus(0xc000_0043)
                        );
                    }))
                });
            }
            let stats = execute_joint(&queue, prepared, &private)?;
            wire::TEST_BEFORE_SET.with(|hook| assert!(hook.borrow().is_none()));
            assert_joint_report(&stats);
            assert_eq!(
                stats.files_copied,
                u64::from(matches!(phase, "add" | "replace")),
                "mask={mask:x} phase={phase}"
            );
            let id = provider.stat(&target.join("file.bin"))?.inode.unwrap();
            if phase == "acl-only" {
                assert_eq!(Some(id), old);
                assert_eq!(stats.file_dacl_report.completed.len(), 1);
            }
            if phase == "replace" {
                assert_ne!(Some(id), old);
            }
            fixtures::verify_file(
                &provider,
                &target,
                &target.join("file.bin"),
                &bytes,
                &acl,
                &context,
                id,
            )?;
            let expected = provider
                .content_sync_reader(&source, &Cancel::new(), None)?
                .metadata_manifest(&source.join("file.bin"))?
                .sparse_extents;
            assert_eq!(
                provider
                    .content_sync_reader(&target, &Cancel::new(), None)?
                    .metadata_manifest(&target.join("file.bin"))?
                    .sparse_extents,
                expected,
                "mask={mask:x} phase={phase}"
            );
            assert_eq!(
                crate::LocalVfs::new()
                    .metadata_manifest(&local.join("target/file.bin"))?
                    .sparse_extents,
                expected
            );
            assert_eq!(
                METADATA_RESTORES.with(|v| v.get()),
                usize::from(matches!(phase, "add" | "replace"))
            );
            assert_eq!(std::fs::read_dir(local.join("target"))?.count(), 1);
            assert_eq!(std::fs::read(physical.join("sentinel"))?, b"UNCHANGED");
            println!("SMB_SPARSE_WINDOWS_DACL_QUEUE_MATRIX_OK mask={mask:x} phase={phase} copied={} id={id}",stats.files_copied);
        }
    }
    Ok(())
}

#[test]
#[ignore = "frischer künstlicher Samba: gemeinsame Sparse-/DACL-Fehler vor und nach dem Commit"]
fn sparse_windows_dacl_queue_failures() -> io::Result<()> {
    use crate::smb::interactive::temp_dacl_flush::FAULT as DACL_FAULT;
    let (provider, physical) = fixtures::fixture()?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    let cases = [
        ("sparse", "metadata-restore-before"),
        ("sparse", "metadata-restore-after"),
        ("sparse", "metadata-flush-before"),
        ("sparse", "metadata-flush-after"),
        ("temp", "before_flush"),
        ("temp", "after_flush"),
        ("dacl", "after_set"),
        ("dacl", "normalized"),
        ("dacl", "reopen_denied"),
        ("dacl", "invalid_frame"),
        ("sparse", "restore-before"),
        ("sparse", "restore-after"),
        ("sparse", "attribute-flush-before"),
        ("sparse", "attribute-flush-after"),
        ("sparse", "attribute-close-after"),
        ("sparse", "attribute-close-before"),
        ("known", "before_open"),
    ];
    for (index, (family, point)) in cases.into_iter().enumerate() {
        let name = format!("case-{index}");
        let local = physical.join(&name);
        std::fs::create_dir_all(local.join("source"))?;
        std::fs::create_dir(local.join("target"))?;
        let private = config.join(&name);
        std::fs::create_dir(&private)?;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))?;
        let source = Path::new("/").join(&name).join("source");
        let target = Path::new("/").join(&name).join("target");
        let bytes = vec![0; 65536];
        seed_joint(
            &provider,
            &local.join("source"),
            &source,
            &private,
            &bytes,
            &[],
            6,
        )?;
        std::fs::write(local.join("target/file.bin"), b"OLD")?;
        std::fs::write(local.join("target/extra"), b"EXTRA")?;
        let queue = OperationQueue::open(private.join("queue.json"))?;
        let prepared = prepare_joint(&provider, &queue, &source, &target, crate::SyncMode::Mirror)?;
        reset_counters();
        match family {
            "sparse" => FAULT.with(|value| value.set(Some(point))),
            "temp" => DACL_FAULT.with(|value| value.set(Some(point))),
            "known" => wire::TEST_FAULT.with(|value| value.set(Some(point))),
            "dacl" => wire::TEST_BEFORE_SET.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    wire::TEST_FAULT.with(|value| value.set(Some(point)))
                }))
            }),
            _ => unreachable!(),
        }
        let result = execute_joint(&queue, prepared, &private);
        FAULT.with(|v| assert!(v.get().is_none(), "unused {point}"));
        DACL_FAULT.with(|v| assert!(v.get().is_none(), "unused {point}"));
        wire::TEST_FAULT.with(|v| assert!(v.get().is_none(), "unused {point}"));
        if family == "known" {
            let stats = result?;
            assert_eq!(stats.files_copied, 1);
            assert!(stats
                .metadata_report
                .issues
                .iter()
                .any(|issue| issue.class == crate::MetadataClass::Acl));
            assert!(!stats.metadata_report.issues.iter().any(|issue| matches!(
                issue.class,
                crate::MetadataClass::SparseExtents | crate::MetadataClass::NamedStreams
            )));
            assert_eq!(std::fs::read(local.join("target/file.bin"))?, bytes);
            assert_eq!(
                provider
                    .content_sync_reader(&target, &Cancel::new(), None)?
                    .metadata_manifest(&target.join("file.bin"))?
                    .sparse_extents,
                Some(vec![])
            );
            assert_eq!(std::fs::read_dir(local.join("target"))?.count(), 1);
            println!("SMB_SPARSE_WINDOWS_DACL_KNOWN_OMISSION_KEEPS_OTHER_METADATA_OK");
            continue;
        }
        let error = result.unwrap_err();
        assert_eq!(
            crate::error_code(&error),
            crate::ErrorCode::OutcomeUnclear,
            "{point}: {error}"
        );
        assert_eq!(
            queue.snapshot().entries.last().unwrap().state,
            crate::QueueState::Failed
        );
        assert!(!crate::error::recovery_paths(&error).is_empty());
        let committed = point.starts_with("restore-") || point.starts_with("attribute-");
        assert_eq!(
            std::fs::read(local.join("target/file.bin"))?,
            if committed {
                bytes.clone()
            } else {
                b"OLD".to_vec()
            },
            "{point}"
        );
        assert_eq!(std::fs::read(local.join("target/extra"))?, b"EXTRA");
        assert_eq!(
            counters().0,
            2,
            "queue probe plus actual data handle: {point}"
        );
        assert_eq!(
            counters().2,
            1,
            "one shared handle CLOSE including Drop: {point}"
        );
        let before = (counters(), METADATA_RESTORES.with(|v| v.get()));
        let blocked = prepare_joint(&provider, &queue, &source, &target, crate::SyncMode::Mirror)
            .err()
            .expect("Offener gemeinsamer Abschluss muss vor einer weiteren Probe sperren");
        assert!(!crate::error::recovery_paths(&blocked).is_empty());
        assert_eq!((counters(), METADATA_RESTORES.with(|v| v.get())), before);
        println!("SMB_SPARSE_WINDOWS_DACL_QUEUE_FAILURE_NO_RETRY_OK case={point} committed={committed} counters={before:?}");
    }
    Ok(())
}

#[test]
#[ignore = "frischer künstlicher Samba: gemeinsame DACL bleibt beim Tempnamen-Tausch am eigenen FID"]
fn sparse_windows_dacl_temp_swap() -> io::Result<()> {
    let (provider, physical) = fixtures::fixture()?;
    std::fs::create_dir(physical.join("source"))?;
    std::fs::create_dir(physical.join("target"))?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    let source = Path::new("/source");
    let target = Path::new("/target");
    let bytes = vec![0; 65536];
    let (acl, _) = seed_joint(
        &provider,
        &physical.join("source"),
        source,
        &config,
        &bytes,
        &[],
        6,
    )?;
    std::fs::write(physical.join("target/file.bin"), b"OLD")?;
    let queue = OperationQueue::open(config.join("queue.json"))?;
    let prepared = prepare_joint(&provider, &queue, source, target, crate::SyncMode::Mirror)?;
    reset_counters();
    let actual = physical.join("target");
    let swapped = Arc::new(Mutex::new(None::<PathBuf>));
    let captured = swapped.clone();
    wire::TEST_BEFORE_SET.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let temp = std::fs::read_dir(&actual)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".file.bin.doppeldecker-tmp-")
                })
                .unwrap();
            std::fs::rename(&temp, actual.join("retained-original")).unwrap();
            std::fs::write(&temp, b"FOREIGN").unwrap();
            *lock(&captured) = Some(temp);
        }))
    });
    let error = execute_joint(&queue, prepared, &config).unwrap_err();
    assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
    let foreign = lock(&swapped).clone().unwrap();
    assert_eq!(std::fs::read(&foreign)?, b"FOREIGN");
    assert_eq!(std::fs::read(physical.join("target/file.bin"))?, b"OLD");
    let name = foreign.file_name().unwrap();
    let manifest = provider
        .content_sync_reader(target, &Cancel::new(), None)?
        .metadata_manifest(&target.join(name))?;
    assert_ne!(manifest.acl, Some(crate::metadata::Acl::WindowsDacl(acl)));
    assert_eq!(counters().2, 1);
    assert_eq!(METADATA_RESTORES.with(|v| v.get()), 0);
    assert!(prepare_joint(&provider, &queue, source, target, crate::SyncMode::Mirror).is_err());
    println!("SMB_SPARSE_WINDOWS_DACL_TEMP_SWAP_TOUCHES_ONLY_OWN_FID_AND_RETAINS_ORIGINAL_OK");
    Ok(())
}

pub(in crate::smb::interactive) fn seed_joint_remote(
    provider: &Arc<SmbVfs>,
    root: &Path,
    config: &Path,
    bytes: &[u8],
    islands: &[(u64, usize)],
    mask: u32,
) -> io::Result<(
    crate::windows_security::WindowsDacl,
    crate::windows_dacl::WindowsDaclWriteContext,
)> {
    use std::io::{Seek, SeekFrom, Write};
    let (parent, context) = fixtures::prepared(provider, root, config, &Cancel::new())?;
    parent.prepare_sparse_write(crate::metadata::AclModel::WindowsSid)?;
    let target = root.join("file.bin");
    let temp = root.join(".seed");
    let scope = parent.staging_scope(&target, None)?.unwrap();
    let mut writer = scope.open_sparse_write_new(&temp)?;
    writer.set_len(bytes.len() as u64)?;
    let mut cursor = 0;
    for &(offset, length) in islands {
        if offset > cursor {
            writer.punch_hole(cursor, offset - cursor)?;
        }
        writer.seek(SeekFrom::Start(offset))?;
        writer.write_all(&bytes[offset as usize..offset as usize + length])?;
        cursor = offset + length as u64;
    }
    if cursor < bytes.len() as u64 {
        writer.punch_hole(cursor, bytes.len() as u64 - cursor)?;
    }
    writer.flush()?;
    drop(writer);
    let mut ads = scope.open_named_stream_write_new(&temp, b"named")?;
    ads.write_all(b"temp-dacl-ads")?;
    ads.flush()?;
    drop(ads);
    let acl = fixtures::desired(&context, mask);
    scope.write_windows_dacl_exact(&temp, &acl, &context)?;
    scope.set_mtime(&temp, 1_700_000_003)?;
    scope.sync_file(&temp)?;
    match provider.stat(&target) {
        Ok(_) => scope.rename_replace_plain_once(&temp, &target)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            scope.rename_noreplace(&temp, &target)?
        }
        Err(error) => return Err(error),
    }
    scope.finish_staging()?;
    fixtures::verify_file(
        provider,
        root,
        &target,
        bytes,
        &acl,
        &context,
        provider.stat(&target)?.inode.unwrap(),
    )?;
    assert_eq!(
        provider
            .content_sync_reader(root, &Cancel::new(), None)?
            .metadata_manifest(&target)?
            .sparse_extents,
        Some(
            islands
                .iter()
                .map(|&(offset, length)| crate::metadata::DataExtent {
                    offset,
                    length: length as u64
                })
                .collect()
        )
    );
    Ok((acl, context))
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "privater Loopbacktunnel: tatsächliche gemeinsame SMB-Sparse-/DACL-Queue auf macOS"]
fn sparse_windows_dacl_macos_actual_queue() -> io::Result<()> {
    use sha2::Digest;
    use std::io::Read;
    let (provider, marker) = mac_bridge_fixture()?;
    let selected = Path::new("/selected");
    let setup = provider.ordinary_mutations(selected, &Cancel::new(), None)?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    for mask in [0, 6] {
        let name = format!("mask-{mask}");
        let root = selected.join(&name);
        setup.create_dir(&root)?;
        setup.create_dir(&root.join("source"))?;
        setup.create_dir(&root.join("target"))?;
        let source = root.join("source");
        let target = root.join("target");
        let private = config.join(&name);
        std::fs::create_dir(&private)?;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))?;
        let mut bytes = vec![0; 1024 * 1024];
        let mut islands = vec![];
        let (mut acl, mut context) =
            seed_joint_remote(&provider, &source, &private, &bytes, &islands, mask)?;
        let queue = OperationQueue::open(private.join("queue.json"))?;
        for (mode, phase) in [
            (crate::SyncMode::AddOnly, "add"),
            (crate::SyncMode::Update, "noop"),
            (crate::SyncMode::Update, "replace"),
            (crate::SyncMode::Update, "acl-only"),
            (crate::SyncMode::Mirror, "mirror"),
            (crate::SyncMode::Update, "final-noop"),
        ] {
            if phase == "replace" {
                bytes[4096..8192].fill(0x41);
                islands = vec![(4096, 4096)];
                (acl, context) =
                    seed_joint_remote(&provider, &source, &private, &bytes, &islands, mask)?;
            }
            if phase == "acl-only" {
                (acl, context) = seed_joint_remote(
                    &provider,
                    &source,
                    &private,
                    &bytes,
                    &islands,
                    if mask == 6 { 4 } else { 6 },
                )?;
            }
            if phase == "mirror" {
                let mut writer = setup.open_write_new(&target.join("extra"))?;
                std::io::Write::write_all(&mut writer, b"EXTRA")?;
                writer.flush()?;
                drop(writer);
            }
            let old = provider
                .stat(&target.join("file.bin"))
                .ok()
                .and_then(|entry| entry.inode);
            let stats = execute_joint(
                &queue,
                prepare_joint(&provider, &queue, &source, &target, mode)?,
                &private,
            )?;
            assert_joint_report(&stats);
            assert_eq!(
                stats.files_copied,
                u64::from(matches!(phase, "add" | "replace"))
            );
            let id = provider.stat(&target.join("file.bin"))?.inode.unwrap();
            if phase == "acl-only" {
                assert_eq!(Some(id), old);
                assert_eq!(stats.file_dacl_report.completed.len(), 1);
            }
            fixtures::verify_file(
                &provider,
                &target,
                &target.join("file.bin"),
                &bytes,
                &acl,
                &context,
                id,
            )?;
            assert_eq!(
                provider
                    .content_sync_reader(&target, &Cancel::new(), None)?
                    .metadata_manifest(&target.join("file.bin"))?
                    .sparse_extents,
                Some(
                    islands
                        .iter()
                        .map(|&(offset, length)| crate::metadata::DataExtent {
                            offset,
                            length: length as u64
                        })
                        .collect()
                )
            );
            assert_eq!(provider.list(&target)?.len(), 1);
            let mut observed = Vec::new();
            provider
                .open_read(Path::new("/sentinel"))?
                .read_to_end(&mut observed)?;
            assert_eq!(observed, marker.as_bytes());
            println!(
                "SMB_SPARSE_WINDOWS_DACL_MAC_ACTUAL_QUEUE_OK mask={mask} phase={phase} copied={}",
                stats.files_copied
            );
            if phase == "final-noop" {
                for side in ["source", "target"] {
                    println!(
                        "SMB_SPARSE_MAC_EXPECTED {}",
                        serde_json::json!({"path":format!("{name}/{side}/file.bin"),"size":bytes.len(),"sha256":format!("{:x}",sha2::Sha256::digest(&bytes)),"extents":islands.iter().map(|&(offset,length)|crate::metadata::DataExtent{offset,length:length as u64}).collect::<Vec<_>>()})
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
#[ignore = "frischer künstlicher Samba: Abbruch nach gemeinsamem DACL-SET erhält den offenen Temp"]
fn sparse_windows_dacl_cancel_before_metadata_flush() -> io::Result<()> {
    let (provider, physical) = fixtures::fixture()?;
    std::fs::create_dir(physical.join("source"))?;
    std::fs::create_dir(physical.join("target"))?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    let source = Path::new("/source");
    let target = Path::new("/target");
    let bytes = vec![0; 65536];
    seed_joint(
        &provider,
        &physical.join("source"),
        source,
        &config,
        &bytes,
        &[],
        6,
    )?;
    std::fs::write(physical.join("target/file.bin"), b"OLD")?;
    std::fs::write(physical.join("target/extra"), b"EXTRA")?;
    let queue = OperationQueue::open(config.join("queue.json"))?;
    let cancel = Cancel::new();
    let prepared = crate::prepare_content_sync_for_queue(
        &queue,
        ProviderEndpoint::smb(provider.clone(), source)?,
        ProviderEndpoint::smb(provider.clone(), target)?,
        joint_options(crate::SyncMode::Mirror),
        &cancel,
    )?;
    let trigger = cancel.clone();
    crate::smb::interactive::temp_dacl_flush::BEFORE_FLUSH
        .with(|hook| *hook.borrow_mut() = Some(Box::new(move || trigger.cancel())));
    reset_counters();
    queue.enqueue(prepared.queue_operation())?;
    let entry = queue.claim_next()?.unwrap();
    let error = crate::execute_prepared_content_sync_queue_entry(
        &queue,
        &entry,
        prepared,
        true,
        &config.join("audit.jsonl"),
        &cancel,
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
    assert!(!crate::error::recovery_paths(&error).is_empty());
    assert_eq!(std::fs::read(physical.join("target/file.bin"))?, b"OLD");
    assert_eq!(std::fs::read(physical.join("target/extra"))?, b"EXTRA");
    assert_eq!(counters().2, 1);
    assert_eq!(METADATA_RESTORES.with(|v| v.get()), 0);
    let before = counters();
    assert!(prepare_joint(&provider, &queue, source, target, crate::SyncMode::Mirror).is_err());
    assert_eq!(counters(), before);
    println!("SMB_SPARSE_WINDOWS_DACL_CANCEL_RETAINS_OLD_TARGET_EXTRA_AND_TEMP_WITHOUT_RETRY_OK");
    Ok(())
}

#[path = "smb_sparse_weak_dacl_tests.rs"]
mod weak;
