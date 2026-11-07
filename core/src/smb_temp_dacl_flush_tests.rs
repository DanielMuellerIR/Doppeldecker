use super::*;
#[path = "smb_hardlink_observation_tests.rs"]
mod hardlink_observation;
use crate::windows_security::{WindowsAce, WindowsDaclState, WindowsSid};
use crate::{
    Cancel, Compare, MetadataProfile, OperationQueue, ProviderEndpoint, QueueExecutionResult,
    QueueState, SyncMode, SyncOptions,
};
use std::os::unix::fs::PermissionsExt;

pub(in crate::smb::interactive) struct TestSecrets;
impl SecretResolver for TestSecrets {
    fn resolve(&self, _: &crate::KeychainSecretReference) -> io::Result<crate::SecretString> {
        crate::SecretString::new("Doppeldecker-SMB-E2E-wertlos-2026".into())
    }
}

pub(in crate::smb::interactive) fn fixture() -> io::Result<(Arc<SmbVfs>, PathBuf)> {
    let required = |name| std::env::var(name).expect("Privater Temp-DACL-Runner fehlt");
    let physical = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(physical.starts_with("/tmp/doppeldecker-smb-acl-write-probe-20261029"));
    assert_eq!(physical.file_name().unwrap(), "share");
    assert!(std::fs::read_dir(&physical)?.next().is_none());
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    assert_eq!(host, "127.0.0.1");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    assert!(port > 1024);
    let provider = Arc::new(SmbVfs::connect(
        SmbConnectionConfig {
            server_guid: discover_smb_server(&host, port)?.server_guid,
            host,
            port,
            share: required("DOPPELDECKER_SMB_TEST_SHARE"),
            username: required("DOPPELDECKER_SMB_TEST_USER"),
            domain: String::new(),
            requested_root: "/".into(),
            password_reference: crate::KeychainSecretReference::new(
                "Doppeldecker-Test",
                "temp-dacl",
            )?,
        },
        Arc::new(TestSecrets),
    )?);
    Ok((provider, physical))
}

pub(in crate::smb::interactive) fn prepared(
    provider: &Arc<SmbVfs>,
    root: &Path,
    area: &Path,
    cancel: &Cancel,
) -> io::Result<(Arc<dyn Vfs>, WindowsDaclWriteContext)> {
    let scope = provider.content_sync_mutations(
        root,
        cancel,
        Some(InteractiveFileOwner {
            staging: crate::local_staging::StagingOwner {
                queue: area.join("seed-queue.json"),
                operation: "temp-dacl-seed".into(),
            },
            profiles: vec![],
        }),
    )?;
    scope.prepare_windows_dacl_write()?;
    let crate::windows_dacl::WindowsDaclWriteSupport::Ready(context) =
        scope.windows_dacl_write_support()?
    else {
        panic!("Künstlicher Exact-Samba blieb unvorbereitet")
    };
    Ok((scope, context))
}

pub(in crate::smb::interactive) fn desired(
    context: &WindowsDaclWriteContext,
    mask: u32,
) -> WindowsDacl {
    let mut acl = WindowsDacl {
        namespace: context.namespace,
        control: 0x1004,
        state: WindowsDaclState::Acl {
            revision: 2,
            aces: vec![
                WindowsAce::Deny {
                    flags: 0,
                    mask,
                    sid: WindowsSid {
                        revision: 1,
                        authority: 1,
                        sub_authorities: vec![0],
                    },
                },
                WindowsAce::Allow {
                    flags: 0,
                    mask: 0x0017_01ff,
                    sid: context.owner.clone(),
                },
            ],
        },
    };
    if mask == 0 {
        if let WindowsDaclState::Acl { aces, .. } = &mut acl.state {
            aces.retain(|ace| !matches!(ace, WindowsAce::Deny { .. }));
        }
    }
    acl
}

fn write_content(scope: &dyn Vfs, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut writer = scope.open_write_new(path)?;
    writer.write_all(bytes)?;
    writer.flush()?;
    drop(writer);
    let mut stream = scope.open_named_stream_write_new(path, b"named")?;
    stream.write_all(b"temp-dacl-ads")?;
    stream.flush()?;
    drop(stream);
    Ok(())
}

fn access_status(
    provider: &SmbVfs,
    path: &Path,
    access: u32,
    sharing: u32,
) -> io::Result<NtStatus> {
    let (tree, mut connection) = {
        let state = lock(&provider.state);
        (state.session.tree.clone(), state.session.connection.clone())
    };
    let request = CreateRequest {
        requested_oplock_level: OplockLevel::None,
        impersonation_level: ImpersonationLevel::Impersonation,
        desired_access: FileAccessMask::new(access),
        file_attributes: FILE_ATTRIBUTE_NORMAL,
        share_access: ShareAccess(sharing),
        create_disposition: CreateDisposition::FileOpen,
        create_options: FILE_OPEN_REPARSE_POINT | FILE_NON_DIRECTORY_FILE,
        name: provider.remote_path(path)?,
        create_contexts: Vec::new(),
    };
    provider.runtime.block_on(async {
        let frame = match connection
            .execute(Command::Create, &request, Some(tree.tree_id))
            .await
        {
            Err(smb2::Error::Protocol {
                status,
                command: Command::Create,
            }) => return Ok(status),
            Err(error) => return Err(smb_error(error)),
            Ok(frame) => frame,
        };
        assert_eq!(frame.header.command, Command::Create);
        if frame.header.status != NtStatus::SUCCESS {
            return Ok(frame.header.status);
        }
        let id = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(smb_error)?
            .file_id;
        tree.close_handle(&mut connection, id)
            .await
            .map_err(smb_error)?;
        Ok(NtStatus::SUCCESS)
    })
}

fn set_external_acl(
    provider: &SmbVfs,
    path: &Path,
    acl: &WindowsDacl,
    context: &WindowsDaclWriteContext,
) -> io::Result<()> {
    let (tree, mut connection) = {
        let state = lock(&provider.state);
        (state.session.tree.clone(), state.session.connection.clone())
    };
    let remote = provider.remote_path(path)?;
    let bytes = acl.encode_for_file_write(context.namespace, &context.owner)?;
    provider.runtime.block_on(async {
        let id = wire::open(
            &mut connection,
            &tree,
            &remote,
            FileAccessMask::READ_CONTROL
                | FileAccessMask::WRITE_DAC
                | FileAccessMask::FILE_READ_ATTRIBUTES,
            false,
            false,
        )
        .await?;
        let result = wire::set(&mut connection, &tree, id, bytes).await;
        let closed = tree
            .close_handle(&mut connection, id)
            .await
            .map_err(smb_error);
        result.and(closed)
    })
}

pub(in crate::smb::interactive) fn verify_file(
    provider: &Arc<SmbVfs>,
    root: &Path,
    path: &Path,
    bytes: &[u8],
    acl: &WindowsDacl,
    context: &WindowsDaclWriteContext,
    inode: u64,
) -> io::Result<()> {
    let reader = provider.content_sync_reader(root, &Cancel::new(), None)?;
    let manifest = reader.metadata_manifest(path)?;
    assert_eq!(
        manifest.acl,
        Some(crate::metadata::Acl::WindowsDacl(acl.clone()))
    );
    let security = manifest.windows_security.unwrap();
    assert_eq!(security.namespace, context.namespace);
    assert_eq!(security.owner, Some(context.owner.clone()));
    assert_eq!(security.group, context.group);
    assert_eq!(provider.stat(path)?.inode, Some(inode));
    assert_eq!(provider.stat(path)?.mtime, Some(1_700_000_003));
    let mut content = Vec::new();
    reader.open_read(path)?.read_to_end(&mut content)?;
    assert_eq!(
        crate::metadata::BlobSummary::from_bytes(&content),
        crate::metadata::BlobSummary::from_bytes(bytes)
    );
    let mut ads = Vec::new();
    reader
        .open_named_stream_read(path, b"named")?
        .read_to_end(&mut ads)?;
    assert_eq!(ads, b"temp-dacl-ads");
    for access in [
        FileAccessMask::FILE_WRITE_DATA,
        FileAccessMask::FILE_APPEND_DATA,
    ] {
        let mask = match &acl.state {
            WindowsDaclState::Acl { aces, .. } => aces
                .iter()
                .find_map(|a| {
                    if let WindowsAce::Deny { mask, .. } = a {
                        Some(*mask)
                    } else {
                        None
                    }
                })
                .unwrap_or(0),
            _ => panic!(),
        };
        assert_eq!(
            access_status(provider, path, access, 7)?,
            if mask & access != 0 {
                NtStatus::ACCESS_DENIED
            } else {
                NtStatus::SUCCESS
            }
        );
    }
    Ok(())
}

#[test]
#[ignore = "benötigt frischen künstlichen Samba mit acl_xattr und streams_xattr"]
fn temp_dacl_flush_lifecycle_roundtrip() -> io::Result<()> {
    let (provider, physical) = fixture()?;
    for case in [
        "success",
        "collision",
        "before_open",
        "grant_denied",
        "after_set",
        "reopen_denied",
        "drop_pending",
        "cancel",
        "before_flush",
        "after_flush",
        "close_reply",
        "identity_drift",
        "acl_drift",
        "mtime_drift",
    ] {
        std::fs::create_dir(physical.join(case))?;
        let root = Path::new("/").join(case);
        let area = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let cancel = Cancel::new();
        let (target, context) = prepared(&provider, &root, area.path(), &cancel)?;
        let path = root.join(".temp");
        let final_path = root.join("file.bin");
        let scope = target.staging_scope(&final_path, None)?.unwrap();
        write_content(scope.as_ref(), &path, b"temp-dacl-main")?;
        let inode = provider.stat(&path)?.inode.unwrap();
        let acl = desired(&context, 6);
        FLUSHES.with(|c| c.set(0));
        CLOSES.with(|c| c.set(0));
        if matches!(case, "before_open" | "after_set" | "reopen_denied") {
            wire::TEST_FAULT.with(|f| f.set(Some(case)));
        }
        if case == "grant_denied" {
            FAULT.with(|f| f.set(Some(case)));
        }
        let written = scope.write_windows_dacl_exact(&path, &acl, &context);
        wire::TEST_FAULT.with(|f| f.set(None));
        FAULT.with(|f| f.set(None));
        if matches!(case, "before_open" | "grant_denied") {
            assert_eq!(
                crate::windows_dacl::known_write_omission(&written.unwrap_err()),
                Some(crate::windows_dacl::WindowsDaclTargetIssue::RequiredAccessDenied)
            );
            assert!(scope.cleanup_owned_temp(&path)?);
            scope.finish_staging()?;
            CLOSES.with(|c| assert_eq!(c.get(), 0));
            println!("TEMP_DACL_PRESET_OMISSION_OWNED_CLEANUP_OK case={case}");
            continue;
        }
        if matches!(case, "after_set" | "reopen_denied") {
            assert_eq!(
                crate::error_code(&written.unwrap_err()),
                crate::ErrorCode::OutcomeUnclear
            );
            assert!(scope.sync_file(&path).is_err());
        } else {
            written?;
            assert!(scope.open_named_stream_write_new(&path, b"late").is_err());
            assert_eq!(
                access_status(
                    &provider,
                    &path,
                    FileAccessMask::FILE_READ_DATA | FileAccessMask::FILE_READ_ATTRIBUTES,
                    ShareAccess::FILE_SHARE_READ | ShareAccess::FILE_SHARE_DELETE
                )?,
                NtStatus::SHARING_VIOLATION
            );
            assert!(scope.rename_noreplace(&path, &final_path).is_err());
            assert!(scope.sync_file(&root.join("foreign")).is_err());
            if case == "mtime_drift" {
                let local = physical.join(case).join(".temp");
                std::fs::rename(&local, physical.join(case).join(".saved-original"))?;
                std::fs::write(&local, b"foreign-mtime-sentinel")?;
                let modified =
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
                std::fs::File::open(&local)?
                    .set_times(std::fs::FileTimes::new().set_modified(modified))?;
                assert!(scope.set_mtime(&path, 1_700_000_003).is_err());
                assert_eq!(std::fs::metadata(&local)?.modified()?, modified);
            } else {
                scope.set_mtime(&path, 1_700_000_003)?;
            }
            if case == "cancel" {
                cancel.cancel();
            }
            if matches!(case, "before_flush" | "after_flush" | "close_reply") {
                FAULT.with(|f| f.set(Some(case)));
            }
            if case == "identity_drift" {
                let local = physical.join(case).join(".temp");
                let saved = physical.join(case).join(".saved-original");
                BEFORE_FLUSH.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        std::fs::rename(&local, saved).unwrap();
                        std::fs::write(local, b"foreign-flush-sentinel").unwrap();
                    }))
                });
            }
            if case == "acl_drift" {
                set_external_acl(&provider, &path, &desired(&context, 2), &context)?;
            }
            if !matches!(case, "drop_pending" | "mtime_drift") {
                let flushed = scope.sync_file(&path);
                FAULT.with(|f| f.set(None));
                if matches!(case, "success" | "collision") {
                    flushed?;
                    assert!(scope.sync_file(&path).is_err());
                    assert!(scope.set_mtime(&path, 1_700_000_004).is_err());
                    if case == "success" {
                        scope.rename_noreplace(&path, &final_path)?;
                        scope.finish_staging()?;
                        verify_file(
                            &provider,
                            &root,
                            &final_path,
                            b"temp-dacl-main",
                            &acl,
                            &context,
                            inode,
                        )?;
                    } else {
                        write_probe(&provider, &final_path, b"old-target-sentinel")?;
                        assert_eq!(
                            scope
                                .rename_noreplace(&path, &final_path)
                                .unwrap_err()
                                .kind(),
                            io::ErrorKind::AlreadyExists
                        );
                        assert_eq!(
                            std::fs::read(physical.join(case).join("file.bin"))?,
                            b"old-target-sentinel"
                        );
                    }
                } else {
                    assert_eq!(
                        crate::error_code(&flushed.unwrap_err()),
                        crate::ErrorCode::OutcomeUnclear
                    );
                    assert!(scope.sync_file(&path).is_err());
                }
            }
        }
        if case != "success" {
            assert!(!matches!(scope.cleanup_owned_temp(&path), Ok(true)));
            assert!(scope.finish_staging().is_err());
            assert!(std::fs::symlink_metadata(physical.join(case).join(".temp")).is_ok());
        }
        let check = if case == "success" { final_path } else { path };
        drop(scope);
        CLOSES.with(|c| assert_eq!(c.get(), 1, "case={case}"));
        FLUSHES.with(|c| {
            assert_eq!(
                c.get(),
                u32::from(matches!(
                    case,
                    "success" | "collision" | "after_flush" | "close_reply"
                )),
                "case={case}"
            )
        });
        assert_eq!(
            access_status(
                &provider,
                &check,
                FileAccessMask::FILE_READ_DATA | FileAccessMask::FILE_READ_ATTRIBUTES,
                ShareAccess::FILE_SHARE_READ | ShareAccess::FILE_SHARE_DELETE
            )?,
            NtStatus::SUCCESS
        );
        if matches!(case, "identity_drift" | "mtime_drift") {
            let bytes = std::fs::read(physical.join(case).join(".temp"))?;
            assert!(bytes == b"foreign-flush-sentinel" || bytes == b"foreign-mtime-sentinel");
        }
        println!("TEMP_DACL_BOUND_LIFECYCLE_NO_RETRY_CLOSE_ONCE_RETAIN_OR_COMMIT_OK case={case}");
    }
    Ok(())
}

pub(in crate::smb::interactive) fn seed(
    provider: &Arc<SmbVfs>,
    root: &Path,
    area: &Path,
    bytes: &[u8],
    mask: u32,
) -> io::Result<(WindowsDacl, WindowsDaclWriteContext)> {
    let (parent, context) = prepared(provider, root, area, &Cancel::new())?;
    let path = root.join(".seed");
    let target = root.join("file.bin");
    let scope = parent.staging_scope(&target, None)?.unwrap();
    write_content(scope.as_ref(), &path, bytes)?;
    let acl = desired(&context, mask);
    scope.write_windows_dacl_exact(&path, &acl, &context)?;
    scope.set_mtime(&path, 1_700_000_003)?;
    scope.sync_file(&path)?;
    match provider.stat(&target) {
        Ok(_) => scope.rename_replace_plain_once(&path, &target)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => scope.rename_noreplace(&path, &target)?,
        Err(e) => return Err(e),
    }
    scope.finish_staging()?;
    verify_file(
        provider,
        root,
        &target,
        bytes,
        &acl,
        &context,
        provider.stat(&target)?.inode.unwrap(),
    )?;
    Ok((acl, context))
}

#[test]
#[ignore = "benötigt frischen künstlichen Samba mit acl_xattr und streams_xattr"]
fn temp_dacl_actual_content_queue_roundtrip() -> io::Result<()> {
    let (provider, physical) = fixture()?;
    for mask in [2, 4, 6, 0x0008_0006] {
        let name = format!("mask-{mask:x}");
        std::fs::create_dir_all(physical.join(&name).join("source"))?;
        std::fs::create_dir(physical.join(&name).join("target"))?;
        let source_root = Path::new("/").join(&name).join("source");
        let target_root = Path::new("/").join(&name).join("target");
        let area = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let queue = OperationQueue::open(area.path().join("queue.json"))?;
        let audit = area.path().join("audit.jsonl");
        let (mut acl, mut context) =
            seed(&provider, &source_root, area.path(), b"queue-first", mask)?;
        for step in ["add", "noop", "replace", "mirror", "final-noop", "unclear"] {
            let bytes: &[u8] = if matches!(step, "replace" | "mirror" | "final-noop") {
                b"queue-second-content"
            } else if step == "unclear" {
                b"queue-third-unclear"
            } else {
                b"queue-first"
            };
            if matches!(step, "replace" | "unclear") {
                (acl, context) = seed(&provider, &source_root, area.path(), bytes, mask)?;
            }
            if step == "mirror" {
                write_probe(&provider, &target_root.join("extra"), b"extra")?;
            }
            let mode = if step == "add" {
                SyncMode::AddOnly
            } else if step == "mirror" {
                SyncMode::Mirror
            } else {
                SyncMode::Update
            };
            let options = SyncOptions {
                mode,
                compare: Compare::ContentHash,
                metadata: MetadataProfile::NativeBestEffort,
                ..Default::default()
            };
            let prepared = crate::prepare_content_sync_for_queue(
                &queue,
                ProviderEndpoint::smb(provider.clone(), &source_root)?,
                ProviderEndpoint::smb(provider.clone(), &target_root)?,
                options.clone(),
                &Cancel::new(),
            )?;
            let copy = matches!(step, "add" | "replace" | "unclear");
            assert_eq!(
                prepared.preview().plan.copy_count + prepared.preview().plan.update_count,
                u64::from(copy)
            );
            assert!(
                prepared
                    .preview()
                    .plan
                    .metadata_report
                    .issues
                    .iter()
                    .all(|issue| issue.class != crate::sync::MetadataClass::Acl),
                "{:?}",
                prepared.preview().plan.metadata_report
            );
            wire::TEST_BEFORE_CHILDREN_FILE_IDS.with(|ids| ids.borrow_mut().clear());
            queue.enqueue(prepared.queue_operation())?;
            let entry = queue.claim_next()?.unwrap();
            let old = provider
                .stat(&target_root.join("file.bin"))
                .ok()
                .map(|e| e.inode);
            if step == "unclear" {
                FAULT.with(|f| f.set(Some("after_flush")));
            }
            let result = crate::execute_prepared_content_sync_queue_entry(
                &queue,
                &entry,
                prepared,
                true,
                &audit,
                &Cancel::new(),
                &mut |_| {},
            );
            FAULT.with(|f| f.set(None));
            if step == "unclear" {
                assert_eq!(
                    crate::error_code(&result.unwrap_err()),
                    crate::ErrorCode::OutcomeUnclear
                );
                assert_eq!(
                    provider.stat(&target_root.join("file.bin"))?.inode,
                    old.unwrap()
                );
                assert_eq!(
                    std::fs::read(physical.join(&name).join("target/file.bin"))?,
                    b"queue-second-content"
                );
                let blocked = crate::prepare_content_sync_for_queue(
                    &queue,
                    ProviderEndpoint::smb(provider.clone(), &source_root)?,
                    ProviderEndpoint::smb(provider.clone(), &target_root)?,
                    options,
                    &Cancel::new(),
                )
                .err()
                .expect("Bekannter Temp-Rest muss die nächste Vorbereitung sperren");
                assert_eq!(
                    crate::error_code(&blocked),
                    crate::ErrorCode::OutcomeUnclear
                );
                assert!(!crate::error::recovery_paths(&blocked).is_empty());
                println!("TEMP_DACL_QUEUE_UNCLEAR_FLUSH_RETAINS_TEMP_OLD_TARGET_AND_BLOCKS_NEXT_RUN_OK mask={mask:#x}");
                continue;
            }
            let QueueExecutionResult::Sync(stats) = result? else {
                panic!()
            };
            assert_eq!(stats.files_copied, u64::from(copy));
            assert!(stats
                .metadata_report
                .issues
                .iter()
                .all(|issue| issue.class != crate::sync::MetadataClass::Acl));
            assert_eq!(
                queue.snapshot().entries.last().unwrap().state,
                QueueState::Completed
            );
            let file = target_root.join("file.bin");
            let inode = provider.stat(&file)?.inode.unwrap();
            if copy {
                wire::TEST_BEFORE_CHILDREN_FILE_IDS
                    .with(|ids| assert_eq!(*ids.borrow(), vec![(file.clone(), inode)]));
            }
            verify_file(&provider, &target_root, &file, bytes, &acl, &context, inode)?;
            if step == "mirror" {
                assert!(
                    matches!(provider.stat(&target_root.join("extra")), Err(e) if e.kind()==io::ErrorKind::NotFound)
                );
            }
            println!("TEMP_DACL_ACTUAL_QUEUE_EXACT_ID_HASH_ADS_MTIME_SIDS_NOOP_OK mask={mask:#x} step={step}");
        }
    }
    Ok(())
}

#[path = "smb_sparse_queue_tests.rs"]
mod sparse_queue;

#[path = "smb_temp_optional_denials_tests.rs"]
mod optional_denials;
