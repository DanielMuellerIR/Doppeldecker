//! Produktpfad: echter ContentSync-Plan, Fensterqueue, Audit und lesender Neustartabschluss.
use super::*;
use crate::{
    Cancel, Compare, MetadataProfile, OperationQueue, ProviderEndpoint, QueueExecutionResult,
    QueueState, SyncMode, SyncOptions,
};

fn file_context(
    provider: &SmbVfs,
    path: &Path,
) -> io::Result<crate::windows_dacl::WindowsDaclWriteContext> {
    let manifest = provider.metadata_manifest(path)?;
    let security = manifest
        .windows_security
        .ok_or_else(|| invalid("Security fehlt"))?;
    Ok(crate::windows_dacl::WindowsDaclWriteContext {
        namespace: security.namespace,
        owner: security.owner.ok_or_else(|| invalid("Owner fehlt"))?,
        group: security.group,
    })
}
fn read_denied_acl(context: &crate::windows_dacl::WindowsDaclWriteContext) -> WindowsDacl {
    let mut denied = acl(context, false);
    let WindowsDaclState::Acl { aces, .. } = &mut denied.state else {
        panic!()
    };
    let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
        panic!()
    };
    *mask &= !FileAccessMask::FILE_READ_DATA;
    denied
}

/// Bewusst externer Fixtureakteur: READ_DATA entziehen, READ_CONTROL/WRITE_DAC
/// erhalten. Diese Rechteform liegt absichtlich außerhalb des Produktencoders.
fn set_external_read_denial(
    provider: &SmbVfs,
    path: &Path,
    context: &crate::windows_dacl::WindowsDaclWriteContext,
) -> io::Result<()> {
    let denied = read_denied_acl(context);
    let mut bytes =
        acl(context, false).encode_for_file_inplace_write(context.namespace, &context.owner)?;
    // Self-relative DACL-Offset, ACL-Header (8), ACE-Header (4), dann Allow-Maske.
    // Der Roundtripparser belegt den gesamten absichtlich geänderten Deskriptor.
    let dacl_offset =
        usize::try_from(read_u32(&bytes, 16)?).map_err(|_| invalid("Fixture-DACL-Offset"))?;
    let mask_offset = dacl_offset + 12;
    let mask = read_u32(&bytes, mask_offset)? & !FileAccessMask::FILE_READ_DATA;
    bytes[mask_offset..mask_offset + 4].copy_from_slice(&mask.to_le_bytes());
    assert_eq!(
        crate::windows_security::parse_security_descriptor(&bytes, context.namespace)?.dacl,
        denied
    );
    assert!(denied
        .encode_for_file_inplace_write(context.namespace, &context.owner)
        .is_err());
    let remote = provider.remote_path(path)?;
    let (tree, mut connection) = {
        let state = lock(&provider.state);
        (state.session.tree.clone(), state.session.connection.clone())
    };
    provider.runtime.block_on(async {
        let access = FILE_INPLACE_ACCESS & !FileAccessMask::FILE_READ_DATA;
        let id = wire::open(&mut connection, &tree, &remote, access, false, false).await?;
        let set = wire::set(&mut connection, &tree, id, bytes).await;
        tree.close_handle(&mut connection, id)
            .await
            .map_err(smb_error)?;
        set.map_err(|error| {
            crate::error::with_context(
                error,
                "Externe Fixture: Datenleserecht nach Preflight entziehen",
            )
        })?;
        // Frisch geöffnet bleibt der reine Identitäts-/Securityzugriff samt
        // WRITE_DAC möglich; nur die zusätzliche Datenleseanforderung fehlt.
        let id = wire::open(&mut connection, &tree, &remote, access, false, false).await?;
        let observed = wire::query(&mut connection, &tree, id, context.namespace).await;
        tree.close_handle(&mut connection, id)
            .await
            .map_err(smb_error)?;
        assert_eq!(observed?.acl, Some(Acl::WindowsDacl(denied)));
        Ok(())
    })
}

/// Die FILE_DELETE_CHILD-Freigabe des Elternordners kann einen DELETE-Deny der
/// Datei überstimmen. Die Fixture setzt am künstlichen Zielroot eine explizite
/// Rechteform ohne diese Freigabe; die Produktwirkung setzt keine Elternrechte.
fn restrict_parent_delete_child(provider: &SmbVfs, parent: &Path) -> io::Result<()> {
    let context = file_context(provider, parent)?;
    let mut desired = acl(&context, false);
    let WindowsDaclState::Acl { aces, .. } = &mut desired.state else {
        panic!()
    };
    let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
        panic!()
    };
    *mask = 0x0017_01ff & !0x40;
    let bytes = desired.encode_for_directory_write(context.namespace, &context.owner)?;
    let remote = provider.remote_path(parent)?;
    let (tree, mut connection) = {
        let state = lock(&provider.state);
        (state.session.tree.clone(), state.session.connection.clone())
    };
    provider.runtime.block_on(async {
        let id = wire::open(
            &mut connection,
            &tree,
            &remote,
            FILE_INPLACE_ACCESS,
            true,
            false,
        )
        .await?;
        let result = wire::set(&mut connection, &tree, id, bytes).await;
        tree.close_handle(&mut connection, id)
            .await
            .map_err(smb_error)?;
        result
    })?;
    assert_eq!(
        provider.metadata_manifest(parent)?.acl,
        Some(Acl::WindowsDacl(desired))
    );
    Ok(())
}

/// Nur FILE_OPEN; weder Daten noch Metadaten schreiben. Den CREATE-Status direkt
/// prüfen, damit eine spätere QUERY-Absage keinen erfolgreichen Schreib-OPEN verdeckt.
fn check_file_open_policy(provider: &SmbVfs, path: &Path, denied_mask: u32) -> io::Result<()> {
    let remote = provider.remote_path(path)?;
    let (tree, mut connection) = {
        let state = lock(&provider.state);
        (state.session.tree.clone(), state.session.connection.clone())
    };
    provider.runtime.block_on(async {
        for access in [
            FileAccessMask::FILE_WRITE_DATA,
            FileAccessMask::FILE_APPEND_DATA,
            FileAccessMask::FILE_READ_EA,
            FileAccessMask::FILE_WRITE_EA,
            FileAccessMask::FILE_EXECUTE,
            FileAccessMask::FILE_WRITE_ATTRIBUTES,
            FileAccessMask::DELETE,
        ] {
            let denied = denied_mask & access != 0;
            let request = CreateRequest {
                requested_oplock_level: OplockLevel::None,
                impersonation_level: ImpersonationLevel::Impersonation,
                desired_access: FileAccessMask::new(access),
                file_attributes: FILE_ATTRIBUTE_NORMAL,
                share_access: ShareAccess(
                    ShareAccess::FILE_SHARE_READ
                        | ShareAccess::FILE_SHARE_WRITE
                        | ShareAccess::FILE_SHARE_DELETE,
                ),
                create_disposition: CreateDisposition::FileOpen,
                create_options: FILE_OPEN_REPARSE_POINT | FILE_NON_DIRECTORY_FILE,
                name: remote.clone(),
                create_contexts: Vec::new(),
            };
            match connection
                .execute(Command::Create, &request, Some(tree.tree_id))
                .await
            {
                Err(smb2::Error::Protocol {
                    status: NtStatus::ACCESS_DENIED,
                    command: Command::Create,
                }) => assert!(
                    denied,
                    "Unverweigertes Dateirecht {access:#x} ist nicht verfügbar"
                ),
                Err(error) => return Err(smb_error(error)),
                Ok(frame) => {
                    assert_eq!(frame.header.command, Command::Create);
                    if frame.header.status == NtStatus::ACCESS_DENIED {
                        assert!(
                            denied,
                            "Unverweigertes Dateirecht {access:#x} ist nicht verfügbar"
                        );
                        continue;
                    }
                    wire::status(&frame, Command::Create)?;
                    let id = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
                        .map_err(smb_error)?
                        .file_id;
                    tree.close_handle(&mut connection, id)
                        .await
                        .map_err(smb_error)?;
                    assert!(
                        !denied,
                        "Explizit verweigertes Dateirecht {access:#x} wurde geöffnet"
                    );
                }
            }
        }
        Ok(())
    })
}

#[test]
#[ignore = "benötigt frischen privaten Samba mit acl_xattr und streams_xattr"]
fn file_dacl_actual_content_queue_roundtrip() -> io::Result<()> {
    let required = |name| std::env::var(name).expect("Privater Datei-DACL-Queuerunner fehlt");
    let physical = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(physical.starts_with("/tmp/doppeldecker-smb-acl-write-probe-20261029"));
    assert_eq!(physical.file_name().unwrap(), "share");
    assert!(std::fs::read_dir(&physical)?.next().is_none());
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    assert_eq!(host, "127.0.0.1");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    assert!(port > 1024);
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
            "file-dacl-queue",
        )?,
    };
    let source = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?);
    let target = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?);
    for case in [
        "success",
        "deny-write",
        "deny-append",
        "deny-write-append",
        "deny-write-append-owner",
        "deny-aliases",
        "deny-delete",
        "deny-write-attributes",
        "deny-read-ea",
        "deny-write-ea",
        "deny-execute",
        "deny-all-unused",
        "deny-all-aliases",
        "deny-all-lost-reply",
        "deny-all-applied-cancel",
        "aliases",
        "noop-conflict",
        "source-drift",
        "target-drift",
        "known-denied",
        "lost-reply",
        "applied-cancel",
    ] {
        let lost_reply = matches!(case, "lost-reply" | "deny-all-lost-reply");
        let applied_cancel = matches!(case, "applied-cancel" | "deny-all-applied-cancel");
        let denied_access_mask = match case {
            "deny-delete" => Some(FileAccessMask::DELETE),
            "deny-write-attributes" => Some(FileAccessMask::FILE_WRITE_ATTRIBUTES),
            "deny-read-ea" => Some(FileAccessMask::FILE_READ_EA),
            "deny-write-ea" => Some(FileAccessMask::FILE_WRITE_EA),
            "deny-execute" => Some(FileAccessMask::FILE_EXECUTE),
            "deny-all-unused"
            | "deny-all-aliases"
            | "deny-all-lost-reply"
            | "deny-all-applied-cancel" => Some(0x0009_013e),
            "deny-write" => Some(FileAccessMask::FILE_WRITE_DATA),
            "deny-append" => Some(FileAccessMask::FILE_APPEND_DATA),
            "deny-write-append" | "deny-aliases" => {
                Some(FileAccessMask::FILE_WRITE_DATA | FileAccessMask::FILE_APPEND_DATA)
            }
            "deny-write-append-owner" => Some(
                FileAccessMask::FILE_WRITE_DATA
                    | FileAccessMask::FILE_APPEND_DATA
                    | FileAccessMask::WRITE_OWNER,
            ),
            _ => None,
        };
        let alias_case = matches!(case, "aliases" | "deny-aliases" | "deny-all-aliases");
        let base = Path::new("/").join(case);
        target.create_dir_unchecked(&base)?;
        let source_root = base.join("source");
        let target_root = base.join("target");
        target.create_dir_unchecked(&source_root)?;
        target.create_dir_unchecked(&target_root)?;
        let source_file = source_root.join("a");
        let target_file = target_root.join("a");
        for (provider, path) in [(&source, &source_file), (&target, &target_file)] {
            write_probe(provider, path, b"unchanged-file-data")?;
            seed_ads(provider, path)?;
        }
        for role in ["source", "target"] {
            std::fs::set_permissions(
                physical.join(case).join(role).join("a"),
                std::fs::Permissions::from_mode(0o700),
            )?;
        }
        let context = file_context(&target, &target_file)?;
        assert_eq!(file_context(&source, &source_file)?, context);
        let before = acl(&context, false);
        let mut after = acl(&context, true);
        if let Some(mask) = denied_access_mask {
            let WindowsDaclState::Acl { aces, .. } = &mut after.state else {
                panic!()
            };
            let WindowsAce::Allow {
                mask: owner_access, ..
            } = &mut aces[0]
            else {
                panic!()
            };
            // Ohne Deny erlaubt das Owner-Allow jeden kontrollierten CREATE-Zugriff.
            *owner_access |= 0x0017_01ff;
            aces.insert(
                0,
                WindowsAce::Deny {
                    flags: 0,
                    mask,
                    sid: WindowsSid {
                        revision: 1,
                        authority: 1,
                        sub_authorities: vec![0],
                    },
                },
            );
        }
        seed_initial(&source, &source_file, &after, &context)?;
        seed_initial(&target, &target_file, &before, &context)?;
        if alias_case || case == "noop-conflict" {
            let source_b = source_root.join("b");
            write_probe(&source, &source_b, b"unchanged-file-data")?;
            seed_ads(&source, &source_b)?;
            std::fs::set_permissions(
                physical.join(case).join("source/b"),
                std::fs::Permissions::from_mode(0o700),
            )?;
            seed_initial(&source, &source_b, &after, &context)?;
            std::fs::hard_link(
                physical.join(case).join("target/a"),
                physical.join(case).join("target/b"),
            )?;
            if case == "noop-conflict" {
                seed(&source, &source_file, &before, &context)?;
            }
        }
        if case == "known-denied" {
            write_probe(
                &source,
                &source_root.join("independent"),
                b"new-independent-content",
            )?;
            for (provider, root) in [(&source, &source_root), (&target, &target_root)] {
                let path = root.join("b");
                write_probe(provider, &path, b"unchanged-file-data")?;
                seed_ads(provider, &path)?;
                let role = if Arc::ptr_eq(provider, &source) {
                    "source"
                } else {
                    "target"
                };
                std::fs::set_permissions(
                    physical.join(case).join(role).join("b"),
                    std::fs::Permissions::from_mode(0o700),
                )?;
                seed_initial(
                    provider,
                    &path,
                    if Arc::ptr_eq(provider, &source) {
                        &after
                    } else {
                        &before
                    },
                    &context,
                )?;
            }
        }
        let expected_streams = Some(vec![crate::metadata::NamedBlobSummary {
            name: b"named".to_vec(),
            value: BlobSummary::from_bytes(b"unchanged-file-ads"),
        }]);
        let original_denied_access_id = if denied_access_mask.is_some() {
            restrict_parent_delete_child(&target, &target_root)?;
            for (provider, path) in [(&source, &source_file), (&target, &target_file)] {
                assert_eq!(
                    provider.metadata_manifest(path)?.named_streams,
                    expected_streams
                );
                assert_eq!(provider.stat(path)?.mtime, Some(1_700_000_000));
            }
            assert_eq!(
                source.metadata_manifest(&source_file)?.acl,
                Some(Acl::WindowsDacl(after.clone()))
            );
            assert_eq!(file_context(&source, &source_file)?, context);
            assert_eq!(
                target.metadata_manifest(&target_file)?.acl,
                Some(Acl::WindowsDacl(before.clone()))
            );
            Some(target.stat(&target_file)?.inode.unwrap())
        } else {
            None
        };
        let config_dir = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let queue_path = config_dir.path().join("queue.json");
        let queue = OperationQueue::open(&queue_path)?;
        let audit = config_dir.path().join("audit.jsonl");
        let cancel = Cancel::new();
        let prepared = crate::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::smb(source.clone(), &source_root)?,
            ProviderEndpoint::smb(target.clone(), &target_root)?,
            SyncOptions {
                mode: SyncMode::Update,
                compare: Compare::ContentHash,
                metadata: MetadataProfile::NativeBestEffort,
                ..Default::default()
            },
            &cancel,
        )?;
        if case == "noop-conflict" {
            assert!(prepared.preview().file_dacl_effects.is_empty());
            assert_eq!(prepared.preview().plan.skip_count, 2);
        } else {
            assert_eq!(
                prepared.preview().file_dacl_effects.len(),
                if case == "known-denied" { 2 } else { 1 }
            );
        }
        if denied_access_mask.is_some() {
            assert!(
                prepared
                    .preview()
                    .plan
                    .metadata_report
                    .issues
                    .iter()
                    .all(|issue| issue.class != crate::sync::MetadataClass::Acl),
                "{:#?}",
                prepared.preview().plan.metadata_report.issues
            );
        }
        if alias_case {
            assert_eq!(
                prepared.preview().file_dacl_effects[0]
                    .alias_scope
                    .selected_paths,
                vec!["a", "b"]
            );
        }
        if case == "source-drift" || case == "target-drift" {
            let mut third = after.clone();
            let WindowsDaclState::Acl { aces, .. } = &mut third.state else {
                panic!()
            };
            let WindowsAce::Allow { mask, .. } = &mut aces[1] else {
                panic!()
            };
            *mask = 0x89;
            seed(
                if case == "source-drift" {
                    &source
                } else {
                    &target
                },
                if case == "source-drift" {
                    &source_file
                } else {
                    &target_file
                },
                &third,
                &context,
            )?;
        }
        let entry = queue.enqueue(prepared.queue_operation())?;
        let entry = queue
            .claim_next()?
            .filter(|entry2| entry2.operation_id == entry.operation_id)
            .unwrap();
        if lost_reply {
            TEST_FAULT.with(|fault| fault.set(Some("after_set")));
        }
        let denial_applied = std::rc::Rc::new(std::cell::Cell::new(false));
        if case == "known-denied" {
            let observed = denial_applied.clone();
            let provider = target.clone();
            let path = target_file.clone();
            let context = context.clone();
            TEST_BEFORE_FILE_OPEN.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    set_external_read_denial(&provider, &path, &context).unwrap();
                    observed.set(true);
                }))
            });
        }
        let result = crate::execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &audit,
            &cancel,
            &mut |progress| {
                if applied_cancel && !progress.file_dacl_report.completed.is_empty() {
                    cancel.cancel();
                }
            },
        );
        TEST_FAULT.with(|fault| fault.set(None));
        TEST_BEFORE_FILE_OPEN.with(|hook| hook.borrow_mut().take());
        if case == "known-denied" {
            assert!(
                denial_applied.get(),
                "Die echte Rechteabsage-Fixture muss vor dem Produkt-Open ausgeführt sein"
            );
        }
        if case == "source-drift" || case == "target-drift" {
            assert!(result.is_err());
            assert!(queue.snapshot().entries[0].file_dacl_report.is_empty());
            assert_eq!(
                source.metadata_manifest(&source_file)?.named_streams,
                target.metadata_manifest(&target_file)?.named_streams
            );
            println!("FILE_DACL_QUEUE_BOUND_SOURCE_TARGET_DRIFT_ZERO_SET_OK case={case}");
            continue;
        }
        if lost_reply || applied_cancel {
            if lost_reply {
                let error = result.unwrap_err();
                assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
                assert_eq!(crate::error::file_dacl_report(&error).uncertain.len(), 1);
            } else {
                let QueueExecutionResult::Sync(stats) = result? else {
                    panic!()
                };
                assert!(stats.cancelled);
                assert_eq!(stats.file_dacl_report.completed.len(), 1);
            }
            let current = queue.snapshot().entries.remove(0);
            assert!(current.file_dacl_recovery);
            assert_eq!(
                current.file_dacl_report.completed.len() + current.file_dacl_report.uncertain.len(),
                1
            );
            assert!(queue.retry(&entry.operation_id).is_err());
            drop(queue);
            let queue = OperationQueue::open(&queue_path)?;
            assert!(queue.snapshot().entries[0].file_dacl_recovery);
            // Eine echte neue SMB-Sitzung prüft denselben gepinnten Endpunkt und Root.
            let reconnected = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?);
            let endpoint = ProviderEndpoint::smb(reconnected, &target_root)?;
            crate::file_dacl_queue::acknowledge_file_dacl_outcome(
                &queue,
                &entry.operation_id,
                &endpoint,
                true,
            )?;
            let current = queue.snapshot().entries.remove(0);
            assert_eq!(current.state, QueueState::Cancelled);
            assert!(!current.file_dacl_recovery);
            assert_eq!(current.file_dacl_report.completed.len(), 1);
            assert!(current.file_dacl_report.uncertain.is_empty());
            println!(
                "FILE_DACL_QUEUE_TYPED_REPORT_RESTART_NEW_SESSION_READ_ONLY_ACK_OK case={case}"
            );
        } else {
            let QueueExecutionResult::Sync(stats) = result? else {
                panic!()
            };
            assert_eq!(stats.files_copied, u64::from(case == "known-denied"));
            assert_eq!(
                stats.file_dacl_report.completed.len(),
                usize::from(case != "noop-conflict")
            );
            assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
            if case == "known-denied" {
                assert!(stats
                    .metadata_report
                    .issues
                    .iter()
                    .any(|issue| issue.class == crate::sync::MetadataClass::Acl
                        && issue.kind == crate::sync::MetadataIssueKind::PrivilegeFailure));
                let observed = target.file_dacl_recovery_observation(
                    &target_file,
                    &target.root_identity(&target_root)?,
                )?;
                assert_eq!(observed.dacl, read_denied_acl(&context));
                assert_eq!(observed.context, context);
                assert_eq!(
                    std::fs::read(physical.join(case).join("target/independent"))?,
                    b"new-independent-content"
                );
                println!(
                    "FILE_DACL_QUEUE_ACTUAL_READ_DATA_DENIED_OTHER_OBJECT_AND_COPY_CONTINUE_OK"
                );
            }
            if case == "success" || alias_case || denied_access_mask.is_some() {
                assert_eq!(stats.files_copied, 0);
                assert_eq!(stats.bytes_copied, 0);
                let next = crate::prepare_content_sync_for_queue(
                    &queue,
                    ProviderEndpoint::smb(source.clone(), &source_root)?,
                    ProviderEndpoint::smb(target.clone(), &target_root)?,
                    SyncOptions {
                        mode: SyncMode::Update,
                        compare: Compare::ContentHash,
                        metadata: MetadataProfile::NativeBestEffort,
                        ..Default::default()
                    },
                    &Cancel::new(),
                )?;
                assert!(next.preview().file_dacl_effects.is_empty());
                if denied_access_mask.is_some() {
                    for report in [&stats.metadata_report, &next.preview().plan.metadata_report] {
                        assert!(
                            report
                                .issues
                                .iter()
                                .all(|issue| issue.class != crate::sync::MetadataClass::Acl),
                            "{:#?}",
                            report.issues
                        );
                    }
                }
                println!(
                    "FILE_DACL_ACTUAL_CONTENT_QUEUE_ZERO_COPY_ALIAS_DEDUP_NOOP_OK case={case}"
                );
            }
        }
        if let Some(mask) = denied_access_mask {
            let observed = target.file_dacl_recovery_observation(
                &target_file,
                &target.root_identity(&target_root)?,
            )?;
            assert_eq!(Some(observed.file_id), original_denied_access_id);
            assert_eq!(observed.context, context);
            assert_eq!(observed.dacl, after);
            assert_eq!(
                target.metadata_manifest(&target_file)?.named_streams,
                expected_streams
            );
            assert_eq!(target.stat(&target_file)?.mtime, Some(1_700_000_000));
            let mut content = Vec::new();
            target.open_read(&target_file)?.read_to_end(&mut content)?;
            assert_eq!(
                BlobSummary::from_bytes(&content),
                BlobSummary::from_bytes(b"unchanged-file-data")
            );
            check_file_open_policy(&target, &target_file, mask)?;
            if alias_case {
                check_file_open_policy(&target, &target_root.join("b"), mask)?;
            }
            println!("FILE_DACL_OPTIONAL_RIGHT_DENY_EXACT_OPEN_RIGHTS_OK case={case}");
        }
        assert_eq!(
            std::fs::read(physical.join(case).join("target/a"))?,
            b"unchanged-file-data"
        );
        let data = std::fs::read_to_string(&audit)?;
        assert!(data.contains("file_dacl_report"));
    }
    Ok(())
}
