//! Echte Produktqueue: V7 folgt auf normale Anlage/Zeit, Recovery bleibt nur lesend.
use super::*;
use crate::metadata::Acl;
use crate::windows_security::{WindowsAce, WindowsDacl, WindowsDaclState, WindowsSid};
use crate::{
    Cancel, Compare, MetadataProfile, OperationQueue, ProviderEndpoint, QueueExecutionResult,
    QueueState, SyncMode, SyncOptions,
};
use std::os::unix::fs::PermissionsExt;
struct TestSecrets;
impl SecretResolver for TestSecrets {
    fn resolve(&self, _: &crate::KeychainSecretReference) -> io::Result<crate::SecretString> {
        crate::SecretString::new("Doppeldecker-SMB-E2E-wertlos-2026".into())
    }
}
fn context(
    provider: &SmbVfs,
    path: &Path,
) -> io::Result<crate::windows_dacl::WindowsDaclWriteContext> {
    let m = provider.metadata_manifest(path)?;
    let s = m
        .windows_security
        .ok_or_else(|| invalid("Fixture ohne Security"))?;
    Ok(crate::windows_dacl::WindowsDaclWriteContext {
        namespace: s.namespace,
        owner: s.owner.ok_or_else(|| invalid("Fixture ohne Owner"))?,
        group: s.group,
    })
}
fn desired(context: &crate::windows_dacl::WindowsDaclWriteContext) -> WindowsDacl {
    WindowsDacl {
        namespace: context.namespace,
        control: 0x1004,
        state: WindowsDaclState::Acl {
            revision: 2,
            aces: vec![
                WindowsAce::Allow {
                    flags: 0,
                    mask: DIRECTORY_ACCESS,
                    sid: context.owner.clone(),
                },
                WindowsAce::Allow {
                    flags: 0,
                    mask: 0x0012_00a9,
                    sid: WindowsSid {
                        revision: 1,
                        authority: 1,
                        sub_authorities: vec![0],
                    },
                },
            ],
        },
    }
}
fn set_bytes(provider: &SmbVfs, path: &Path, bytes: Vec<u8>, unprotected: bool) -> io::Result<()> {
    let remote = provider.remote_path(path)?;
    let (tree, mut connection) = {
        let s = lock(&provider.state);
        (s.session.tree.clone(), s.session.connection.clone())
    };
    provider.runtime.block_on(async {
        let id = wire::open(
            &mut connection,
            &tree,
            &remote,
            DIRECTORY_ACCESS,
            true,
            false,
        )
        .await?;
        let request = SetInfoRequest {
            info_type: InfoType::Security,
            file_info_class: 0,
            additional_information: if unprotected {
                0x2000_0004
            } else {
                0x8000_0004
            },
            file_id: id,
            buffer: bytes,
        };
        let result = async {
            let frame = connection
                .execute(Command::SetInfo, &request, Some(tree.tree_id))
                .await
                .map_err(smb_error)?;
            wire::status(&frame, Command::SetInfo)?;
            smb2::msg::set_info::SetInfoResponse::unpack(&mut ReadCursor::new(&frame.body))
                .map_err(smb_error)?;
            Ok(())
        }
        .await;
        tree.close_handle(&mut connection, id)
            .await
            .map_err(smb_error)?;
        result
    })
}
fn seed_desired(provider: &SmbVfs, path: &Path) -> io::Result<WindowsDacl> {
    let c = context(provider, path)?;
    let acl = desired(&c);
    set_bytes(
        provider,
        path,
        acl.encode_for_directory_write(c.namespace, &c.owner)?,
        false,
    )?;
    assert_eq!(
        provider.metadata_manifest(path)?.acl,
        Some(Acl::WindowsDacl(acl.clone()))
    );
    Ok(acl)
}
fn seed_inheriting_parent(provider: &SmbVfs, path: &Path) -> io::Result<()> {
    seed_inheriting_parent_with_mask(provider, path, 0x0017_01ff)
}
fn seed_inheriting_parent_with_mask(
    provider: &SmbVfs,
    path: &Path,
    owner_mask: u32,
) -> io::Result<()> {
    let c = context(provider, path)?;
    let mut acl = desired(&c);
    let WindowsDaclState::Acl { aces, .. } = &mut acl.state else {
        panic!()
    };
    let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
        panic!()
    };
    *mask = owner_mask;
    let mut bytes = acl.encode_for_directory_write(c.namespace, &c.owner)?;
    bytes[2..4].copy_from_slice(&0x8504u16.to_le_bytes());
    let offset = read_u32(&bytes, 16)? as usize;
    let count = u16::from_le_bytes(bytes[offset + 4..offset + 6].try_into().unwrap());
    let mut cursor = offset + 8;
    for _ in 0..count {
        bytes[cursor + 1] = 3;
        cursor += usize::from(u16::from_le_bytes(
            bytes[cursor + 2..cursor + 4].try_into().unwrap(),
        ));
    }
    set_bytes(provider, path, bytes, true)?;
    let Some(Acl::WindowsDacl(actual)) = provider.metadata_manifest(path)?.acl else {
        panic!()
    };
    assert_eq!(actual.control & 0x1400, 0x0400);
    println!(
        "V7_PARENT_ACTUAL={}",
        serde_json::to_string(&actual).unwrap()
    );
    Ok(())
}
fn deny_only_optional_write_dac(provider: &SmbVfs, path: &Path) -> io::Result<u64> {
    let c = context(provider, path)?;
    let acl = desired(&c);
    let mut bytes = acl.encode_for_directory_write(c.namespace, &c.owner)?;
    let d = read_u32(&bytes, 16)? as usize;
    let count = u16::from_le_bytes(bytes[d + 4..d + 6].try_into().unwrap());
    let size = u16::from_le_bytes(bytes[d + 2..d + 4].try_into().unwrap());
    let mut deny = vec![1, 0, 20, 0];
    deny.extend_from_slice(&FileAccessMask::WRITE_DAC.to_le_bytes());
    deny.extend_from_slice(&[1, 1, 0, 0, 0, 0, 0, 3]);
    deny.extend_from_slice(&4u32.to_le_bytes());
    bytes[d + 2..d + 4].copy_from_slice(&(size + 20).to_le_bytes());
    bytes[d + 4..d + 6].copy_from_slice(&(count + 1).to_le_bytes());
    bytes.splice(d + 8..d + 8, deny);
    let expected = crate::windows_security::parse_security_descriptor(&bytes, c.namespace)?.dacl;
    set_bytes(provider, path, bytes, false)?;
    let remote = provider.remote_path(path)?;
    let (tree, mut connection) = {
        let s = lock(&provider.state);
        (s.session.tree.clone(), s.session.connection.clone())
    };
    provider.runtime.block_on(async {
        let id = wire::open(
            &mut connection,
            &tree,
            &remote,
            DIRECTORY_ACCESS & !FileAccessMask::WRITE_DAC,
            true,
            false,
        )
        .await?;
        let e = query_handle_entry(&mut connection, &tree, id, String::new()).await?;
        let actual = wire::query(&mut connection, &tree, id, c.namespace).await?;
        tree.close_handle(&mut connection, id)
            .await
            .map_err(smb_error)?;
        assert_eq!(wire::context(&actual)?, c);
        assert_eq!(actual.acl, Some(Acl::WindowsDacl(expected)));
        Ok(e.file_id)
    })
}
#[test]
#[ignore = "benötigt frischen privaten Samba mit acl_xattr und streams_xattr"]
fn created_empty_directory_actual_content_queue_roundtrip() -> io::Result<()> {
    created_directory_queue_cases(&[
        "success",
        "add-only",
        "create-reply",
        "cancel-after-create",
        "cancel-after-metadata",
        "retry-history",
        "excluded-child",
        "source-drift",
        "source-drift-before-set",
        "target-swap",
        "known-denied",
        "mtime-resource",
        "mtime-open-denied",
        "mtime-identity-read",
        "source-list-denied-before",
        "source-list-denied-after",
        "source-list-unknown-before",
        "source-list-unknown-after",
        "source-streams-denied-before",
        "source-streams-denied-after",
        "source-list-denied-before-set",
        "mtime-readback",
        "mtime-readback-drift",
        "mtime-set-reply",
        "lost-reply",
        "applied-cancel",
    ])
}
#[test]
#[ignore = "benötigt frischen privaten Samba; nur V7 mit echter mtime-Openabsage"]
fn created_empty_directory_mtime_open_denial_actual_content_queue_roundtrip() -> io::Result<()> {
    created_directory_queue_cases(&["mtime-open-denied"])
}
fn created_directory_queue_cases(cases: &[&'static str]) -> io::Result<()> {
    let required = |name| std::env::var(name).expect("Privater V7-Queuerunner fehlt");
    let physical = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(physical
        .to_string_lossy()
        .starts_with("/tmp/doppeldecker-smb-acl-write-probe-"));
    assert_eq!(physical.file_name().unwrap(), "share");
    assert_eq!(physical.canonicalize()?, physical);
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
            "created-directory-queue",
        )?,
    };
    let source = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?);
    let target = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?);
    for &case in cases {
        println!("V7_QUEUE_CASE_BEGIN={case}");
        let base = Path::new("/").join(case);
        target.create_dir_unchecked(&base)?;
        let source_root = base.join("source");
        let target_root = base.join("target");
        target.create_dir_unchecked(&source_root)?;
        target.create_dir_unchecked(&target_root)?;
        let folder = source_root.join("a");
        source.create_dir_unchecked(&folder)?;
        if case == "mtime-open-denied" {
            source.set_mtime_unchecked(&folder, 1_700_000_300)?;
        }
        let after = seed_desired(&source, &folder)?;
        if case == "mtime-open-denied" {
            seed_inheriting_parent_with_mask(&target, &target_root, 0x0017_00ff)?;
        } else {
            seed_inheriting_parent(&target, &target_root)?;
        }
        write_probe(
            &source,
            &source_root.join("z-independent"),
            b"independent-content",
        )?;
        if case == "excluded-child" {
            std::fs::write(
                physical.join(case).join("source/a/hidden"),
                b"excluded-still-not-empty",
            )?;
        }
        let area = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let queue_path = area.path().join("queue.json");
        let queue = OperationQueue::open(&queue_path)?;
        let audit = area.path().join("audit.jsonl");
        let cancel = Cancel::new();
        let options = SyncOptions {
            mode: if case == "add-only" || case == "retry-history" {
                SyncMode::AddOnly
            } else {
                SyncMode::Update
            },
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeBestEffort,
            exclude: if case == "excluded-child" {
                vec!["**/hidden".into()]
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        let prepared = crate::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::smb(source.clone(), &source_root)?,
            ProviderEndpoint::smb(target.clone(), &target_root)?,
            options.clone(),
            &cancel,
        )?;
        println!(
            "V7_QUEUE_PREVIEW={}",
            serde_json::json!({"case":case,"effects":prepared.preview().created_directory_dacl_effects,"issues":prepared.preview().plan.metadata_report.issues})
        );
        assert_eq!(
            prepared.preview().created_directory_dacl_effects.len(),
            usize::from(case != "excluded-child")
        );
        let expected_context = prepared
            .preview()
            .created_directory_dacl_effects
            .first()
            .map(|effect| effect.selection.probe_context.clone());
        let target_folder = target_root.join("a");
        let observed_id = Arc::new(std::sync::atomic::AtomicU64::new(0));
        if case == "source-drift" {
            std::fs::write(physical.join(case).join("source/a/late"), b"new-child")?;
        }
        if ["source-drift-before-set", "target-swap", "known-denied"].contains(&case) {
            let t = target.clone();
            let path = target_folder.clone();
            let physical = physical.clone();
            let id = observed_id.clone();
            TEST_BEFORE_SECURITY.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    id.store(t.stat(&path).unwrap().inode.unwrap(), Ordering::SeqCst);
                    match case {
                        "source-drift-before-set" => {
                            std::fs::write(physical.join(case).join("source/a/late"), b"new-child")
                                .unwrap()
                        }
                        "target-swap" => {
                            let p = physical.join(case).join("target/a");
                            std::fs::rename(&p, p.with_file_name("old-a")).unwrap();
                            std::fs::create_dir(p).unwrap();
                        }
                        "known-denied" => {
                            assert_eq!(
                                deny_only_optional_write_dac(&t, &path).unwrap(),
                                id.load(Ordering::SeqCst)
                            );
                        }
                        _ => unreachable!(),
                    }
                }))
            });
        }
        if case == "lost-reply" {
            TEST_FAULT.with(|f| f.set(Some("after_set")));
        }
        if case == "create-reply" {
            TEST_FAULT.with(|f| f.set(Some("after_create")));
        }
        super::super::super::mtime::FAULT.with(|fault| {
            fault.set(match case {
                "mtime-resource" => Some(super::super::super::mtime::Fault::ResourceOpen),
                "mtime-identity-read" => Some(super::super::super::mtime::Fault::IdentityRead),
                "mtime-readback" => Some(super::super::super::mtime::Fault::Readback),
                "mtime-set-reply" => Some(super::super::super::mtime::Fault::AfterSet),
                _ => None,
            })
        });
        if case == "mtime-readback-drift" {
            let expected_path = target_folder.clone();
            let physical_folder = physical.join(case).join("target/a");
            let saved = physical_folder.with_file_name("saved-original-a");
            let restored_folder = physical_folder.clone();
            let saved_original = saved.clone();
            super::super::super::mtime::BEFORE_READBACK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |path| {
                    assert_eq!(path, expected_path);
                    std::fs::rename(&physical_folder, &saved).unwrap();
                    std::fs::create_dir(&physical_folder).unwrap();
                    use std::os::unix::fs::MetadataExt;
                    assert_ne!(
                        std::fs::metadata(&physical_folder).unwrap().ino(),
                        std::fs::metadata(&saved).unwrap().ino()
                    );
                }));
            });
            super::super::super::mtime::AFTER_READBACK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |result| {
                    // Der erste echte SMB-Stat muss das fremde Objekt erkennen.
                    // Vor dem möglichen zweiten Lesen wird das Original zurückgesetzt.
                    let code = crate::error_code(result.as_ref().unwrap_err());
                    std::fs::remove_dir(&restored_folder).unwrap();
                    std::fs::rename(&saved_original, &restored_folder).unwrap();
                    assert_eq!(code, crate::ErrorCode::PlanDrift);
                }));
            });
        }
        crate::sync::CREATED_SOURCE_CHECKS.with(|count| count.set(0));
        crate::sync::CREATED_SOURCE_FAULT.with(|fault| {
            fault.set(match case {
                "source-list-denied-before" => {
                    Some((1, crate::sync::CreatedSourceFault::ListingDenied))
                }
                "source-list-denied-after" => {
                    Some((2, crate::sync::CreatedSourceFault::ListingDenied))
                }
                "source-list-unknown-before" => {
                    Some((1, crate::sync::CreatedSourceFault::ListingIncomplete))
                }
                "source-list-unknown-after" => {
                    Some((2, crate::sync::CreatedSourceFault::ListingIncomplete))
                }
                "source-streams-denied-before" => {
                    Some((1, crate::sync::CreatedSourceFault::StreamsDenied))
                }
                "source-streams-denied-after" => {
                    Some((2, crate::sync::CreatedSourceFault::StreamsDenied))
                }
                "source-list-denied-before-set" => {
                    Some((3, crate::sync::CreatedSourceFault::ListingDenied))
                }
                _ => None,
            })
        });
        super::super::super::mtime::SET_PATHS.with(|paths| paths.borrow_mut().clear());
        super::super::super::mtime::OPEN_ACCESS_DENIED_PATHS
            .with(|paths| paths.borrow_mut().clear());
        let enqueued = queue.enqueue(prepared.queue_operation())?;
        let entry = queue
            .claim_next()?
            .filter(|e| e.operation_id == enqueued.operation_id)
            .unwrap();
        let result = crate::execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &audit,
            &cancel,
            &mut |progress| {
                if (case == "cancel-after-create"
                    && progress
                        .created_directory_dacl_report
                        .created
                        .iter()
                        .any(|o| !o.ordinary_metadata_applied))
                    || (["cancel-after-metadata", "retry-history"].contains(&case)
                        && progress
                            .created_directory_dacl_report
                            .created
                            .iter()
                            .any(|o| o.ordinary_metadata_applied))
                {
                    cancel.cancel();
                }
                if case == "applied-cancel"
                    && !progress.created_directory_dacl_report.completed.is_empty()
                {
                    cancel.cancel();
                }
            },
        );
        TEST_FAULT.with(|f| f.set(None));
        crate::sync::CREATED_SOURCE_FAULT.with(|fault| fault.set(None));
        TEST_BEFORE_SECURITY.with(|hook| hook.borrow_mut().take());
        super::super::super::mtime::FAULT.with(|f| f.set(None));
        super::super::super::mtime::BEFORE_READBACK
            .with(|hook| assert!(hook.borrow_mut().take().is_none()));
        super::super::super::mtime::AFTER_READBACK
            .with(|hook| assert!(hook.borrow_mut().take().is_none()));
        let folder_sets = super::super::super::mtime::SET_PATHS.with(|paths| {
            paths
                .borrow()
                .iter()
                .filter(|path| **path == target.remote_path(&target_folder).unwrap())
                .count()
        });
        assert_eq!(
            folder_sets,
            usize::from(
                ![
                    "source-drift",
                    "create-reply",
                    "cancel-after-create",
                    "mtime-open-denied"
                ]
                .contains(&case)
            ),
            "mtime muss genau einmal oder vor jedem SET gescheitert sein: {case}"
        );
        let events = std::fs::read_to_string(area.path().join(".doppeldecker-temp-events.jsonl"))?;
        let finishes = events
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| {
                event[0] == "finish"
                    && event[1]["target"] == target_folder.to_string_lossy().as_ref()
            })
            .count();
        assert_eq!(
            finishes,
            usize::from(!["source-drift", "create-reply"].contains(&case)),
            "Normaler CREATE-Intent darf genau einmal abgeschlossen werden: {case}"
        );
        println!(
            "V7_NORMAL_MTIME_AND_STAGING_COUNTS case={case} sets={folder_sets} finishes={finishes}"
        );
        if ["source-drift", "source-drift-before-set", "target-swap"].contains(&case) {
            let error = result.unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
            let report = &queue.snapshot().entries[0].created_directory_dacl_report;
            assert!(!report.has_rights_effects());
            assert_eq!(report.created.len(), usize::from(case != "source-drift"));
            if case != "source-drift" {
                assert!(report.created[0].ordinary_metadata_applied);
            }
            assert!(!physical.join(case).join("target/z-independent").exists());
            println!("V7_QUEUE_DRIFT_ZERO_DACL_SET_OK case={case}");
            continue;
        }
        if case == "create-reply" {
            let error = result.unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            let current = &queue.snapshot().entries[0];
            assert!(!current.created_directory_dacl_recovery);
            assert!(!current.temp_recovery_paths.is_empty());
            assert!(current.created_directory_dacl_report.is_empty());
            assert!(physical.join(case).join("target/a").is_dir());
            assert!(queue.retry(&entry.operation_id).is_err());
            println!("V7_CREATE_REPLY_LOSS_NORMAL_STAGING_ONLY_ZERO_DACL_INTENT_OK");
            continue;
        }
        if case == "retry-history" {
            let QueueExecutionResult::Sync(stats) = result? else {
                panic!()
            };
            assert!(stats.cancelled);
            let history = stats.created_directory_dacl_report.as_ref().clone();
            assert_eq!(history.created.len(), 1);
            assert!(history.created[0].ordinary_metadata_applied);
            assert!(!history.has_rights_effects());
            queue.retry(&entry.operation_id)?;
            queue.claim_next()?.unwrap();
            let next = crate::prepare_content_sync_for_queue(
                &queue,
                ProviderEndpoint::smb(source.clone(), &source_root)?,
                ProviderEndpoint::smb(target.clone(), &target_root)?,
                options,
                &Cancel::new(),
            )?;
            assert!(next.preview().created_directory_dacl_effects.is_empty());
            queue.require_confirmation(
                &entry.operation_id,
                crate::QueueAttention::RemoteRun {
                    reason: "Neue Vorschau nach normaler Teilanlage".into(),
                    confirmation: "prüfen".into(),
                },
            )?;
            queue.confirm_content_sync_preparation(&entry.operation_id, &next)?;
            drop(next);
            drop(queue);
            let queue = OperationQueue::open(&queue_path)?;
            assert_eq!(
                queue.snapshot().entries[0].created_directory_dacl_report,
                history
            );
            assert!(!queue.snapshot().entries[0].created_directory_dacl_recovery);
            println!("V7_NORMAL_CREATION_HISTORY_RETRY_REPREPARE_CONFIRM_RELOAD_OK");
            continue;
        }
        if case == "cancel-after-create" || case == "cancel-after-metadata" {
            let QueueExecutionResult::Sync(stats) = result? else {
                panic!()
            };
            assert!(stats.cancelled);
            assert_eq!(stats.created_directory_dacl_report.created.len(), 1);
            assert_eq!(
                stats.created_directory_dacl_report.created[0].ordinary_metadata_applied,
                case == "cancel-after-metadata"
            );
            assert!(!stats.created_directory_dacl_report.has_rights_effects());
            let current = &queue.snapshot().entries[0];
            assert_eq!(current.state, QueueState::Cancelled);
            assert!(!current.created_directory_dacl_recovery);
            assert!(!physical.join(case).join("target/z-independent").exists());
            println!("V7_NORMAL_CREATE_METADATA_CANCEL_KNOWN_PARTIAL_WITHOUT_DACL_RECOVERY_OK case={case}");
            continue;
        }
        if case == "mtime-readback-drift" {
            let error = result.unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
            // Die tatsächlich beobachtete Fremd-ID ist bereits zurückgetauscht. Ein
            // zweites Lesen wäre erfolgreich, darf die erkannte Drift aber nicht löschen.
            let original = target.stat(&target_folder)?.inode.unwrap();
            target.verify_mtime_readback(
                &target_folder,
                source.stat(&folder)?.mtime.unwrap(),
                Some(original),
            )?;
            let snapshot = queue.snapshot();
            let report = &snapshot.entries[0].created_directory_dacl_report;
            assert_eq!(report.created.len(), 1);
            assert!(!report.created[0].ordinary_metadata_applied);
            assert!(!report.has_rights_effects());
            assert!(!physical.join(case).join("target/z-independent").exists());
            println!("V7_MTIME_KNOWN_DRIFT_SURVIVES_RESTORED_READBACK_OK");
            continue;
        }
        if case == "mtime-set-reply" {
            let error = result.unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            let report = &queue.snapshot().entries[0].created_directory_dacl_report;
            assert!(!report.has_rights_effects());
            assert_eq!(report.created.len(), 1);
            assert!(!report.created[0].ordinary_metadata_applied);
            assert!(!queue.snapshot().entries[0].created_directory_dacl_recovery);
            assert!(queue.retry(&entry.operation_id).is_err());
            println!("V7_NORMAL_MTIME_UNKNOWN_WITHOUT_DACL_RECOVERY_OK");
            continue;
        }
        if case == "lost-reply" || case == "applied-cancel" {
            if case == "lost-reply" {
                let error = result.unwrap_err();
                assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
                assert_eq!(
                    crate::error::created_directory_dacl_report(&error)
                        .uncertain
                        .len(),
                    1
                );
            } else {
                let QueueExecutionResult::Sync(stats) = result? else {
                    panic!()
                };
                assert!(stats.cancelled);
                assert_eq!(stats.created_directory_dacl_report.completed.len(), 1);
            }
            assert!(queue.snapshot().entries[0].created_directory_dacl_recovery);
            assert!(queue.retry(&entry.operation_id).is_err());
            let id = target.stat(&target_folder)?.inode.unwrap();
            drop(queue);
            let queue = OperationQueue::open(&queue_path)?;
            assert!(queue.snapshot().entries[0].created_directory_dacl_recovery);
            let fresh = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?);
            let endpoint = ProviderEndpoint::smb(fresh, &target_root)?;
            crate::acknowledge_created_directory_dacl_outcome(
                &queue,
                &entry.operation_id,
                &endpoint,
                true,
            )?;
            let current = queue.snapshot().entries.remove(0);
            assert_eq!(current.state, QueueState::Cancelled);
            assert!(!current.created_directory_dacl_recovery);
            assert_eq!(current.created_directory_dacl_report.completed.len(), 1);
            assert_eq!(
                current.created_directory_dacl_report.completed[0].file_id,
                id
            );
            assert!(current.created_directory_dacl_report.uncertain.is_empty());
            println!("V7_QUEUE_RESTART_NEW_SESSION_READ_ONLY_ACK_OK case={case}");
        } else {
            let QueueExecutionResult::Sync(stats) = result? else {
                panic!()
            };
            assert_eq!(stats.files_copied, 1);
            let omitted = [
                "excluded-child",
                "known-denied",
                "mtime-resource",
                "mtime-open-denied",
                "mtime-readback",
                "mtime-identity-read",
                "source-list-denied-before",
                "source-list-denied-after",
                "source-list-unknown-before",
                "source-list-unknown-after",
                "source-streams-denied-before",
                "source-streams-denied-after",
                "source-list-denied-before-set",
            ]
            .contains(&case);
            assert_eq!(
                stats.created_directory_dacl_report.completed.len(),
                usize::from(!omitted),
                "case={case}; metadata_report={:?}; created_report={:?}",
                stats.metadata_report,
                stats.created_directory_dacl_report,
            );
            assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
            assert_eq!(
                std::fs::read(physical.join(case).join("target/z-independent"))?,
                b"independent-content"
            );
            if case == "known-denied" {
                assert_ne!(observed_id.load(Ordering::SeqCst), 0);
                assert_eq!(
                    target.stat(&target_folder)?.inode,
                    Some(observed_id.load(Ordering::SeqCst))
                );
                assert!(stats
                    .metadata_report
                    .issues
                    .iter()
                    .any(|i| i.kind == crate::sync::MetadataIssueKind::PrivilegeFailure));
                println!("V7_REAL_OPTIONAL_WRITE_DAC_DENIAL_NORMAL_CREATE_TIME_COPY_CONTINUE_OK");
            }
            if case == "mtime-open-denied" {
                let remote = target.remote_path(&target_folder)?;
                let denials = super::super::super::mtime::OPEN_ACCESS_DENIED_PATHS.with(|paths| {
                    paths
                        .borrow()
                        .iter()
                        .filter(|path| **path == remote)
                        .count()
                });
                assert_eq!(
                    denials, 2,
                    "Receipt-OPEN und genau ein normaler Fallback, beide vor SET abgelehnt"
                );
                assert_eq!(folder_sets, 0);
                assert_eq!(stats.created_directory_dacl_report.created.len(), 1);
                assert!(!stats.created_directory_dacl_report.created[0].ordinary_metadata_applied);
                assert!(!stats.created_directory_dacl_report.has_rights_effects());
                assert!(stats
                    .metadata_report
                    .issues
                    .iter()
                    .any(|issue| issue.path == "a"
                        && issue.class == crate::sync::MetadataClass::Mtime
                        && issue.kind == crate::sync::MetadataIssueKind::PrivilegeFailure));
                assert_ne!(
                    target.stat(&target_folder)?.mtime,
                    source.stat(&folder)?.mtime
                );
                assert!(!queue.snapshot().entries[0].created_directory_dacl_recovery);
                let id = target.stat(&target_folder)?.inode;
                drop(queue);
                let reopened = OperationQueue::open(&queue_path)?;
                let fresh = SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?;
                assert_eq!(reopened.snapshot().entries[0].state, QueueState::Completed);
                assert_eq!(fresh.stat(&target_folder)?.inode, id);
                assert!(
                    !reopened.snapshot().entries[0]
                        .created_directory_dacl_report
                        .created[0]
                        .ordinary_metadata_applied
                );
                super::super::super::mtime::SET_PATHS.with(|paths| {
                    assert_eq!(
                        paths
                            .borrow()
                            .iter()
                            .filter(|path| **path == remote)
                            .count(),
                        0
                    )
                });
                let audit_report = completed_audit_metadata(&audit)?;
                assert!(audit_report.issues.iter().any(|issue| issue.path == "a"
                    && issue.class == crate::sync::MetadataClass::Mtime
                    && issue.kind == crate::sync::MetadataIssueKind::PrivilegeFailure));
                assert!(audit_report.privilege_failures >= 1);
                println!("V7_REAL_MTIME_OPEN_DENIAL_ABANDONED_RECEIPT_ZERO_SET_FALSE_METADATA_MARKER_NEW_SESSION_READ_ONLY_OK");
                continue;
            }
            if case == "success" || case == "add-only" {
                let observed = target.metadata_manifest(&target_folder)?;
                assert_eq!(
                    context(&target, &target_folder)?,
                    expected_context.clone().unwrap()
                );
                assert_eq!(observed.named_streams, Some(vec![]));
                assert_eq!(
                    target.stat(&target_folder)?.mtime,
                    source.stat(&folder)?.mtime
                );
                assert_eq!(stats.created_directory_dacl_report.created.len(), 1);
                assert!(stats.created_directory_dacl_report.created[0].ordinary_metadata_applied);
                assert_eq!(
                    stats.created_directory_dacl_report.created[0]
                        .effect_key
                        .file_id,
                    stats.created_directory_dacl_report.completed[0].file_id
                );

                assert_eq!(observed.acl, Some(Acl::WindowsDacl(after)));
                assert_eq!(
                    stats.created_directory_dacl_report.completed[0].file_id,
                    target.stat(&target_folder)?.inode.unwrap()
                );
                let next = crate::prepare_content_sync_for_queue(
                    &queue,
                    ProviderEndpoint::smb(source.clone(), &source_root)?,
                    ProviderEndpoint::smb(target.clone(), &target_root)?,
                    options,
                    &Cancel::new(),
                )?;
                assert!(next.preview().created_directory_dacl_effects.is_empty());
                println!("V7_NORMAL_CREATE_ORIGINAL_ID_EXACT_DACL_NOOP_OK case={case}");
            }
        }
        assert!(std::fs::read_to_string(&audit)?.contains("created_directory_dacl_report"));
    }
    Ok(())
}

#[test]
#[ignore = "benötigt frischen privaten Samba; echte Queue mit mtime-Openabsage und SET-Antwortverlust"]
fn ordinary_mtime_open_denial_actual_queue_roundtrip() -> io::Result<()> {
    use super::super::super::mtime as normal_time;
    use std::io::Read;
    let required = |name| std::env::var(name).expect("Privater SMB-Proberunner fehlt");
    let physical = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(physical
        .to_string_lossy()
        .starts_with("/tmp/doppeldecker-smb-acl-write-probe-"));
    assert_eq!(physical.file_name().unwrap(), "share");
    assert_eq!(physical.canonicalize()?, physical);
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
            "mtime-open-denial-probe",
        )?,
    };
    for (case, owner_mask, lost_reply) in [
        ("open-denied", 0x0016_00e7, false),
        ("after-set-unknown", 0x0016_01e7, true),
    ] {
        println!("MTIME_QUEUE_CASE_BEGIN case={case} owner_mask={owner_mask:#010x}");
        let source = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?);
        let target = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?);
        let base = Path::new("/").join(case);
        let source_root = base.join("source");
        let target_root = base.join("target");
        let source_parent = source_root.join("parent");
        let target_parent = target_root.join("parent");
        for path in [
            &base,
            &source_root,
            &target_root,
            &source_parent,
            &target_parent,
        ] {
            target.create_dir_unchecked(path)?;
        }
        let local_source = physical.join(case).join("source");
        std::fs::write(
            local_source.join("parent/child.bin"),
            b"child-content-confirmed",
        )?;
        std::fs::write(
            local_source.join("z-independent.bin"),
            b"independent-content-confirmed",
        )?;
        let source_time = 1_700_000_300;
        source.set_mtime_unchecked(&source_parent, source_time)?;
        target.set_mtime_unchecked(&target_parent, 1_700_000_100)?;
        let c = context(&source, &source_parent)?;
        assert_eq!(context(&target, &target_parent)?.owner, c.owner);
        let mut acl = desired(&c);
        let WindowsDaclState::Acl { aces, .. } = &mut acl.state else {
            panic!()
        };
        let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
            panic!()
        };
        *mask = owner_mask;
        let bytes = acl.encode_for_directory_write(c.namespace, &c.owner)?;
        set_bytes(&source, &source_parent, bytes.clone(), false)?;
        set_bytes(&target, &target_parent, bytes, false)?;
        assert_eq!(
            source.metadata_manifest(&source_parent)?.acl,
            Some(Acl::WindowsDacl(acl.clone()))
        );
        assert_eq!(
            target.metadata_manifest(&target_parent)?.acl,
            Some(Acl::WindowsDacl(acl.clone()))
        );
        let original_id = target.stat(&target_parent)?.inode.unwrap();
        let original_context = context(&target, &target_parent)?;
        let before_children_time = target.stat(&target_parent)?.mtime;
        println!("MTIME_QUEUE_SETUP_COMPLETE case={case} source_time={source_time} target_time_before_children={before_children_time:?} id={original_id} acl={}", serde_json::to_string(&acl).unwrap());
        let area = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let queue_path = area.path().join("queue.json");
        let audit = area.path().join("audit.jsonl");
        let queue = OperationQueue::open(&queue_path)?;
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
        println!(
            "MTIME_QUEUE_PREVIEW case={case} preview={}",
            serde_json::to_string(prepared.preview()).unwrap()
        );
        assert!(prepared.preview().directory_dacl_effects.is_empty());
        assert!(prepared.preview().created_directory_dacl_effects.is_empty());
        assert!(prepared.preview().file_dacl_effects.is_empty());
        let entry = queue.enqueue(prepared.queue_operation())?;
        let entry = queue
            .claim_next()?
            .filter(|e| e.operation_id == entry.operation_id)
            .unwrap();
        normal_time::FAULT.with(|f| f.set(None));
        normal_time::SET_PATHS.with(|paths| paths.borrow_mut().clear());
        normal_time::OPEN_ACCESS_DENIED_PATHS.with(|paths| paths.borrow_mut().clear());
        let mut before_parent_attempt = None;
        let mut armed = false;
        let result = crate::execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &audit,
            &cancel,
            &mut |progress| {
                if progress.phase == "metadata" && progress.current == "parent" {
                    if before_parent_attempt.is_none() {
                        before_parent_attempt = target.stat(&target_parent).unwrap().mtime;
                    }
                    if lost_reply && !armed {
                        normal_time::FAULT.with(|f| f.set(Some(normal_time::Fault::AfterSet)));
                        armed = true;
                    }
                }
            },
        );
        normal_time::FAULT.with(|f| f.set(None));
        let remote = target.remote_path(&target_parent)?;
        let parent_sets = normal_time::SET_PATHS
            .with(|paths| paths.borrow().iter().filter(|p| **p == remote).count());
        let parent_denials = normal_time::OPEN_ACCESS_DENIED_PATHS
            .with(|paths| paths.borrow().iter().filter(|p| **p == remote).count());
        let privilege_omission = match &result {
            Ok(QueueExecutionResult::Sync(stats)) => {
                assert_eq!(stats.files_copied, 2);
                assert_eq!(stats.bytes_copied, 52);
                println!(
                    "MTIME_QUEUE_RESULT case={case} stats={}",
                    serde_json::to_string(stats).unwrap()
                );
                stats.metadata_report.issues.iter().any(|issue| {
                    issue.path == "parent"
                        && issue.class == crate::sync::MetadataClass::Mtime
                        && issue.kind == crate::sync::MetadataIssueKind::PrivilegeFailure
                })
            }
            Ok(other) => {
                println!("MTIME_QUEUE_UNEXPECTED_RESULT case={case} result={other:?}");
                false
            }
            Err(error) => {
                println!(
                    "MTIME_QUEUE_ERROR case={case} code={:?} error={error:?}",
                    crate::error_code(error)
                );
                false
            }
        };
        let state = queue.snapshot().entries[0].state;
        let code = result.as_ref().err().map(crate::error_code);
        println!("MTIME_QUEUE_OBSERVATION case={case} queue_state={state:?} error_code={code:?} explicit_parent_open_access_denied={parent_denials} parent_set_count={parent_sets} time_before_children={before_children_time:?} time_after_children_before_parent_attempt={before_parent_attempt:?} privilege_omission={privilege_omission} queue={:?}",queue.snapshot());
        assert!(
            before_parent_attempt.is_some(),
            "Der echte normale Parent-Metadatenweg muss erreicht sein"
        );
        assert!(
            queue.retry(&entry.operation_id).is_err(),
            "Kein erneuter Lauf nach möglichem oder behauptetem unklarem SET"
        );
        drop(queue);
        let reopened = OperationQueue::open(&queue_path)?;
        let fresh = SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?;
        for (path, expected) in [
            (
                target_parent.join("child.bin"),
                b"child-content-confirmed".as_slice(),
            ),
            (
                target_root.join("z-independent.bin"),
                b"independent-content-confirmed".as_slice(),
            ),
        ] {
            let mut bytes = Vec::new();
            fresh.open_read(&path)?.read_to_end(&mut bytes)?;
            assert_eq!(bytes, expected);
        }
        assert_eq!(fresh.stat(&target_parent)?.inode, Some(original_id));
        assert_eq!(context(&fresh, &target_parent)?, original_context);
        assert_eq!(
            fresh.metadata_manifest(&target_parent)?.acl,
            Some(Acl::WindowsDacl(acl))
        );
        let after_time = fresh.stat(&target_parent)?.mtime;
        println!("MTIME_QUEUE_FRESH_READ_ONLY case={case} id={original_id} parent_time={after_time:?} queue_state={:?}",reopened.snapshot().entries[0].state);
        assert_eq!(
            normal_time::SET_PATHS.with(|paths| paths
                .borrow()
                .iter()
                .filter(|p| **p == remote)
                .count()),
            parent_sets,
            "Neu geöffnete Queue und neue Sitzung dürfen keinen Zeit-SET senden"
        );
        if lost_reply {
            assert!(armed);
            assert_eq!(parent_denials, 0);
            assert_eq!(parent_sets, 1);
            assert_eq!(code, Some(crate::ErrorCode::OutcomeUnclear));
            assert_eq!(after_time, Some(source_time));
            assert!(reopened.retry(&entry.operation_id).is_err());
            println!("MTIME_QUEUE_AFTER_ACTUAL_SET_SYNTHETIC_REPLY_LOSS_NO_RETRY_NEW_SESSION_READ_ONLY_OK");
        } else {
            assert_eq!(
                parent_denials, 1,
                "Echter OPEN_ACCESS_DENIED am Parent, kein direkter Adapter-Kontrolllauf"
            );
            assert_eq!(parent_sets, 0);
            // Nur mit dem Zustand nach Kinderarbeit vergleichen, nicht mit der
            // bewusst alten Zeit vor deren normalen CREATE-/Renamewirkungen.
            assert_eq!(after_time, before_parent_attempt);
            assert_eq!(state, QueueState::Completed);
            assert!(privilege_omission);
            assert_eq!(reopened.snapshot().entries[0].state, QueueState::Completed);
            let audit_report = completed_audit_metadata(&audit)?;
            assert_eq!(audit_report.privilege_failures, 1);
            assert!(audit_report
                .issues
                .iter()
                .any(|issue| issue.path == "parent"
                    && issue.class == crate::sync::MetadataClass::Mtime
                    && issue.kind == crate::sync::MetadataIssueKind::PrivilegeFailure));
            println!("MTIME_QUEUE_DENIED_COMPLETED_PRIVILEGE_OMISSION_ZERO_SET_OK");
        }
    }
    println!("MTIME_QUEUE_BOTH_CASES_OBSERVED; runner owns fresh artificial-share cleanup; no ACL/time restore or remote deletion by test");
    Ok(())
}

fn completed_audit_metadata(path: &Path) -> io::Result<crate::sync::MetadataReport> {
    let record = std::fs::read_to_string(path)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?
        .into_iter()
        .find(|record| record["status"] == "completed")
        .ok_or_else(|| invalid("Abgeschlossener Auditrecord fehlt"))?;
    assert_eq!(record["error_code"], serde_json::Value::Null);
    serde_json::from_value(record["metadata_report"].clone()).map_err(io::Error::other)
}

fn before_children_seed_file(
    provider: &Arc<SmbVfs>,
    root: &Path,
    name: &str,
    owner: crate::remote_real_mode::InteractiveFileOwner,
    content: &[u8],
    ads: Option<&[u8]>,
) -> io::Result<crate::metadata::MetadataManifest> {
    use std::io::Write;
    let context = provider.content_sync_mutations(root, &Cancel::new(), Some(owner))?;
    context.prepare_windows_dacl_write()?;
    let Support::Ready(expected) = context.windows_dacl_write_support()? else {
        return Err(invalid(
            "Native-Kinderprobe benötigt echte Datei-DACL-Unterstützung",
        ));
    };
    let target = root.join(name);
    let temp = root.join(format!(".native-seed-{name}"));
    let scope = context.staging_scope(&target, None)?.unwrap();
    let mut writer = scope.open_write_new(&temp)?;
    writer.write_all(content)?;
    writer.flush()?;
    drop(writer);
    if let Some(ads) = ads {
        let mut writer = scope.open_named_stream_write_new(&temp, b"named")?;
        writer.write_all(ads)?;
        writer.flush()?;
        drop(writer);
    }
    let mut acl = desired(&expected);
    let WindowsDaclState::Acl { aces, .. } = &mut acl.state else {
        panic!()
    };
    let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
        panic!()
    };
    *mask = 0x0017_01ff;
    scope.write_windows_dacl_exact(&temp, &acl, &expected)?;
    scope.set_mtime(&temp, 1_700_000_200)?;
    scope.sync_file(&temp)?;
    scope.rename_noreplace(&temp, &target)?;
    scope.finish_staging()?;
    let observed = provider.metadata_manifest(&target)?;
    assert_eq!(observed.acl, Some(Acl::WindowsDacl(acl)));
    assert_eq!(provider.stat(&target)?.mtime, Some(1_700_000_200));
    Ok(observed)
}

fn normalized_wire_path(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches('/').to_owned()
}

#[test]
#[ignore = "echte V8-NativeBestEffort-Produktqueue gegen frische künstliche Samba-Root"]
fn created_nonempty_directory_actual_native_content_queue_roundtrip() -> io::Result<()> {
    before_children_native_queue_case("success", true)
}

#[test]
#[ignore = "echte V8-Queue mit gezielten Fehlern an vorhandenen Phasengrenzen"]
fn created_nonempty_directory_native_queue_failure_boundaries_roundtrip() -> io::Result<()> {
    for (index, case) in [
        "source-raw-drift",
        "optional-before-set",
        "cancel-applied",
        "cancel-finished",
        "lost-dacl-reply",
        "checkpoint-failed",
        "child-hash-drift",
        "mtime-open-denied",
        "mtime-set-reply",
        "mtime-readback",
        "final-id-swap",
    ]
    .iter()
    .enumerate()
    {
        before_children_native_queue_case(case, index == 0)?;
    }
    println!("V8_NATIVE_FAILURE_BOUNDARIES_PASS cases=11 no_mutating_retry_restore_delete=1");
    Ok(())
}

#[test]
#[ignore = "echter einzelner V8-Queuecheckpointfehler mit terminalen Audits und rein lesender Recovery"]
fn created_nonempty_directory_native_queue_checkpoint_failure_roundtrip() -> io::Result<()> {
    before_children_native_queue_case("checkpoint-failed", true)
}

fn before_children_native_queue_case(case: &str, first: bool) -> io::Result<()> {
    use super::super::super::mtime as normal_time;
    let required = |name| std::env::var(name).expect("Isolierter SMB-Testrunner fehlt");
    let physical = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(physical
        .to_string_lossy()
        .starts_with("/tmp/doppeldecker-smb-acl-write-probe-"));
    assert_eq!(physical.file_name().unwrap(), "share");
    assert_eq!(physical.canonicalize()?, physical);
    if first {
        assert!(std::fs::read_dir(&physical)?.next().is_none());
    }
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
            "v8-native-children",
        )?,
    };
    let setup = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?);
    let base = PathBuf::from(format!("/v8-native-queue-{case}"));
    let source_root = base.join("source");
    let target_root = base.join("target");
    let source_parent = source_root.join("parent");
    let source_sub = source_parent.join("sub");
    let parent = target_root.join("parent");
    for path in [
        base.as_path(),
        &source_root,
        &target_root,
        &source_parent,
        &source_sub,
    ] {
        setup.create_dir_unchecked(path)?;
    }
    // Die frühe V8-DACL besitzt keine ACE-Vererbung. Derselbe künstliche
    // Eingangskontext verhindert einen allein durch die Fixture verursachten
    // Gruppenwechsel zwischen Rootprobe und späterem Kindtemp.
    let root_context = context(&setup, &target_root)?;
    let mut root_acl = desired(&root_context);
    let WindowsDaclState::Acl { aces, .. } = &mut root_acl.state else {
        panic!()
    };
    let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
        panic!()
    };
    *mask = 0x0017_01ff;
    set_bytes(
        &setup,
        &target_root,
        root_acl.encode_for_directory_write(root_context.namespace, &root_context.owner)?,
        false,
    )?;
    assert_eq!(
        setup.metadata_manifest(&target_root)?.acl,
        Some(Acl::WindowsDacl(root_acl))
    );
    println!("V8_NATIVE_ROOT_FIXTURE protected_explicit=1 owner_mask=0x001701ff ace_inheritance=0 sid_checks_unchanged=1");
    let area = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let queue_path = area.path().join("queue.json");
    let queue = OperationQueue::open(&queue_path)?;
    let audit = area.path().join("audit.jsonl");
    let (seed_owner, seed_lock, seed_root) = queue.ftps_owner(None)?;
    let seed_owner = crate::remote_real_mode::InteractiveFileOwner {
        staging: seed_owner,
        profiles: vec![],
    };
    let base_source = before_children_seed_file(
        &setup,
        &source_parent,
        "file.bin",
        seed_owner.clone(),
        b"native-main-content",
        Some(b"native-named-stream"),
    )?;
    let nested_source = before_children_seed_file(
        &setup,
        &source_sub,
        "inside.bin",
        seed_owner,
        b"native-nested-content",
        None,
    )?;
    drop(seed_root);
    drop(seed_lock);
    setup.set_mtime_unchecked(&source_parent, 1_600_000_000)?;
    let source_context = context(&setup, &source_parent)?;
    let mut after = desired(&source_context);
    let WindowsDaclState::Acl { aces, .. } = &mut after.state else {
        panic!()
    };
    let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
        panic!()
    };
    *mask = 0x0016_01e7;
    set_bytes(
        &setup,
        &source_parent,
        after.encode_for_directory_write(source_context.namespace, &source_context.owner)?,
        false,
    )?;
    let cancel = Cancel::new();
    let prepared = crate::prepare_content_sync_for_queue(
        &queue,
        ProviderEndpoint::smb(setup.clone(), &source_root)?,
        ProviderEndpoint::smb(setup.clone(), &target_root)?,
        SyncOptions {
            mode: SyncMode::AddOnly,
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeBestEffort,
            ..Default::default()
        },
        &cancel,
    )?;
    println!(
        "V8_NATIVE_ROOT_CONTEXT={}",
        serde_json::to_string(&context(&setup, &target_root)?).unwrap()
    );
    println!(
        "V8_NATIVE_PREVIEW={}",
        serde_json::to_string(prepared.preview()).unwrap()
    );
    assert_eq!(
        prepared
            .preview()
            .created_directory_before_children_effects
            .len(),
        1
    );
    assert_eq!(
        prepared.preview().created_directory_before_children_effects[0].directory,
        parent.to_string_lossy()
    );
    assert!(prepared.preview().created_directory_dacl_effects.is_empty());
    let planned_report = prepared.preview().plan.metadata_report.clone();
    let first_copy = prepared
        .preview()
        .plan
        .actions
        .iter()
        .find_map(|action| match action {
            crate::Action::CopyFile {
                path, source_mtime, ..
            } => Some((path.clone(), *source_mtime)),
            _ => None,
        })
        .unwrap();
    for action in &prepared.preview().plan.actions {
        if let crate::Action::CopyFile {
            path,
            metadata: Some(metadata),
            ..
        } = action
        {
            println!(
                "V8_NATIVE_PLANNED_FILE_CONTEXT path={path} transfer={:?}",
                metadata.windows_dacl
            );
        }
    }
    let enqueued = queue.enqueue(prepared.queue_operation())?;
    let entry = queue.claim_next()?.unwrap();
    assert_eq!(entry.operation_id, enqueued.operation_id);
    let (owner, owner_lock, owner_root) = queue.ftps_owner(Some(&entry.operation_id))?;
    TEST_BEFORE_CHILDREN_SETS.with(|paths| paths.borrow_mut().clear());
    TEST_BEFORE_CHILDREN_FINISHED.with(|paths| paths.borrow_mut().clear());
    wire::TEST_BEFORE_CHILDREN_FILE_DACL_PATHS.with(|paths| paths.borrow_mut().clear());
    normal_time::SET_PATHS.with(|paths| paths.borrow_mut().clear());
    wire::TEST_BEFORE_CHILDREN_CONTEXTS.with(|contexts| contexts.borrow_mut().clear());
    wire::TEST_BEFORE_CHILDREN_FILE_IDS.with(|ids| ids.borrow_mut().clear());
    normal_time::OPEN_ACCESS_DENIED_PATHS.with(|paths| paths.borrow_mut().clear());
    crate::sync::CREATED_SOURCE_CHECKS.with(|count| count.set(0));
    crate::sync::CREATED_SOURCE_FAULT.with(|fault| {
        fault.set(if case == "optional-before-set" {
            Some((2, crate::sync::CreatedSourceFault::ListingDenied))
        } else {
            None
        })
    });
    TEST_FAULT.with(|fault| {
        fault.set(if case == "lost-dacl-reply" {
            Some("after_set")
        } else {
            None
        })
    });
    if case == "source-raw-drift" {
        let physical_source = physical.join(source_parent.strip_prefix("/").unwrap());
        TEST_BEFORE_SECURITY.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(
                    physical_source.join("file.bin"),
                    physical_source.join("renamed.bin"),
                )
                .unwrap();
                crate::LocalVfs::new()
                    .set_mtime(&physical_source, 1_600_000_000)
                    .unwrap();
            }))
        });
    }
    let mut original_id = None;
    let mut checkpoint_seen = false;
    let mut children_seen = false;
    let mut final_time_seen = false;
    let result = crate::execute_prepared_content_sync_queue_entry(
        &queue,
        &entry,
        prepared,
        true,
        &audit,
        &cancel,
        &mut |progress| {
            if progress.phase == "directory_created" {
                original_id = progress
                    .created_directory_dacl_report
                    .created
                    .first()
                    .map(|value| value.effect_key.file_id);
                if case == "checkpoint-failed" {
                    queue.fail_nonterminal_progress_update_after_for_test(0);
                }
            }
            if progress.phase == "directory_rights_before_children" {
                let snapshot = queue.snapshot();
                let persisted = snapshot
                    .entries
                    .iter()
                    .find(|value| value.operation_id == entry.operation_id)
                    .unwrap();
                if case == "checkpoint-failed" {
                    assert!(persisted.created_directory_dacl_report.completed.is_empty());
                    assert!(cancel.is_cancelled());
                } else if case == "lost-dacl-reply" {
                    assert_eq!(persisted.created_directory_dacl_report.uncertain.len(), 1);
                } else {
                    assert_eq!(persisted.created_directory_dacl_report.completed.len(), 1);
                    assert!(persisted
                        .created_directory_dacl_report
                        .created
                        .iter()
                        .all(|value| !value.ordinary_metadata_applied));
                    assert_eq!(
                        Some(persisted.created_directory_dacl_report.completed[0].file_id),
                        original_id
                    );
                    checkpoint_seen = true;
                }
                println!(
                    "V8_NATIVE_NEW_PARENT_CONTEXT case={case} context={}",
                    serde_json::to_string(&context(&setup, &parent).unwrap()).unwrap()
                );
                if case == "cancel-applied" {
                    cancel.cancel();
                }
                if case == "cancel-finished"
                    && TEST_BEFORE_CHILDREN_FINISHED.with(|paths| !paths.borrow().is_empty())
                {
                    cancel.cancel();
                }
            }
            if progress.phase == "copy" && progress.current.starts_with("parent/") {
                if case != "optional-before-set" {
                    assert!(checkpoint_seen);
                    TEST_BEFORE_CHILDREN_FINISHED.with(|paths| {
                        assert_eq!(paths.borrow().as_slice(), std::slice::from_ref(&parent))
                    });
                }
                assert!(
                    !crate::created_directory_dacl_registry::has_open(&owner, &owner_root).unwrap()
                );
                normal_time::SET_PATHS.with(|paths| {
                    assert!(!paths.borrow().iter().any(|path| normalized_wire_path(path)
                        == normalized_wire_path(&parent.to_string_lossy())))
                });
                if case == "child-hash-drift" && !children_seen {
                    assert_eq!(progress.current, first_copy.0);
                    let child =
                        physical.join(source_root.join(&first_copy.0).strip_prefix("/").unwrap());
                    let mut bytes = std::fs::read(&child).unwrap();
                    bytes[0] ^= 1;
                    std::fs::write(&child, bytes).unwrap();
                    crate::LocalVfs::new()
                        .set_mtime(&child, first_copy.1.unwrap())
                        .unwrap();
                }
                children_seen = true;
            }
            if progress.phase == "metadata" && progress.current == "parent" && !final_time_seen {
                final_time_seen = true;
                normal_time::FAULT.with(|fault| {
                    fault.set(match case {
                        "mtime-open-denied" => Some(normal_time::Fault::OpenAccessDenied),
                        "mtime-set-reply" => Some(normal_time::Fault::AfterSet),
                        "mtime-readback" => Some(normal_time::Fault::Readback),
                        _ => None,
                    })
                });
                if case == "final-id-swap" {
                    let path = physical.join(parent.strip_prefix("/").unwrap());
                    normal_time::BEFORE_OPEN.with(|hook| {
                        *hook.borrow_mut() = Some(Box::new(move || {
                            std::fs::rename(&path, path.with_file_name("original-parent-held"))
                                .unwrap();
                            std::fs::create_dir(&path).unwrap();
                            std::fs::write(path.join("sentinel"), b"unrelated-target").unwrap();
                            crate::LocalVfs::new()
                                .set_mtime(&path, 1_555_000_000)
                                .unwrap();
                        }))
                    });
                }
            }
        },
    );
    TEST_FAULT.with(|fault| fault.set(None));
    TEST_BEFORE_SECURITY.with(|hook| hook.borrow_mut().take());
    crate::sync::CREATED_SOURCE_FAULT.with(|fault| fault.set(None));
    normal_time::FAULT.with(|fault| fault.set(None));
    normal_time::BEFORE_OPEN.with(|hook| hook.borrow_mut().take());
    if case != "success" {
        let snapshot = queue.snapshot();
        let current = &snapshot.entries[0];
        let report = current.created_directory_dacl_report.clone();
        println!(
            "V8_BOUNDARY_QUEUE case={case} entry={}",
            serde_json::to_string(current).unwrap()
        );
        println!(
            "V8_BOUNDARY_RESULT case={case} code={:?} state={:?} report={} result={result:?}",
            current.error_code,
            current.state,
            serde_json::to_string(&report).unwrap()
        );
        let completed_children = matches!(
            case,
            "optional-before-set"
                | "mtime-open-denied"
                | "mtime-set-reply"
                | "mtime-readback"
                | "final-id-swap"
        );
        let no_rights = matches!(case, "source-raw-drift" | "optional-before-set");
        let pending = matches!(
            case,
            "cancel-applied" | "lost-dacl-reply" | "checkpoint-failed"
        );
        let unclear_rights = case == "lost-dacl-reply";
        assert_eq!(report.created.len(), 1);
        assert_eq!(
            report.created[0].effect_key.origin,
            CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren
        );
        assert_eq!(Some(report.created[0].effect_key.file_id), original_id);
        assert_eq!(
            report.created[0].ordinary_metadata_applied,
            case == "optional-before-set"
        );
        assert_eq!(
            report.completed.len(),
            usize::from(!no_rights && !unclear_rights)
        );
        assert_eq!(report.uncertain.len(), usize::from(unclear_rights));
        assert_eq!(current.created_directory_dacl_recovery, pending);
        if matches!(case, "cancel-applied" | "cancel-finished") {
            let QueueExecutionResult::Sync(stats) = result.as_ref().unwrap() else {
                panic!()
            };
            assert!(stats.cancelled);
            assert_eq!(stats.files_copied, 0);
        } else if matches!(case, "optional-before-set" | "mtime-open-denied") {
            let QueueExecutionResult::Sync(stats) = result.as_ref().unwrap() else {
                panic!()
            };
            assert!(!stats.cancelled);
            assert_eq!((stats.files_copied, stats.bytes_copied), (2, 59));
            let expected_class = if case == "optional-before-set" {
                crate::sync::MetadataClass::Acl
            } else {
                crate::sync::MetadataClass::Mtime
            };
            assert_eq!(
                stats
                    .metadata_report
                    .issues
                    .iter()
                    .filter(|issue| issue.path == "parent"
                        && issue.class == expected_class
                        && issue.kind == crate::sync::MetadataIssueKind::PrivilegeFailure)
                    .count(),
                1
            );
        } else {
            let error = result.as_ref().unwrap_err();
            match case {
                "source-raw-drift" | "child-hash-drift" | "final-id-swap" => {
                    assert_eq!(crate::error_code(error), crate::ErrorCode::PlanDrift)
                }
                "lost-dacl-reply" | "mtime-set-reply" | "mtime-readback" => {
                    assert_eq!(crate::error_code(error), crate::ErrorCode::OutcomeUnclear)
                }
                "checkpoint-failed" => {}
                _ => unreachable!(),
            }
        }
        TEST_BEFORE_CHILDREN_SETS
            .with(|paths| assert_eq!(paths.borrow().len(), usize::from(!no_rights)));
        TEST_BEFORE_CHILDREN_FINISHED
            .with(|paths| assert_eq!(paths.borrow().len(), usize::from(!no_rights && !pending)));
        wire::TEST_BEFORE_CHILDREN_FILE_DACL_PATHS
            .with(|paths| assert_eq!(paths.borrow().len(), if completed_children { 2 } else { 0 }));
        let time_sets = normal_time::SET_PATHS.with(|paths| paths.borrow().clone());
        let parent_sets = time_sets
            .iter()
            .filter(|path| {
                normalized_wire_path(path) == normalized_wire_path(&parent.to_string_lossy())
            })
            .count();
        assert_eq!(
            parent_sets,
            usize::from(matches!(
                case,
                "optional-before-set" | "mtime-set-reply" | "mtime-readback"
            ))
        );
        assert_eq!(
            time_sets.len(),
            if completed_children {
                3 + parent_sets
            } else {
                0
            }
        );
        normal_time::OPEN_ACCESS_DENIED_PATHS.with(|paths| {
            assert_eq!(
                paths.borrow().len(),
                usize::from(case == "mtime-open-denied")
            )
        });
        if case == "mtime-open-denied" {
            println!("V8_BOUNDARY_SYNTHETIC_CREATE_ACCESS_DENIED_ZERO_PARENT_SET");
        }
        if !no_rights {
            assert!(queue.retry(&entry.operation_id).is_err());
        }
        let records = std::fs::read_to_string(&audit)?
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let terminal = records
            .iter()
            .rev()
            .find(|record| {
                record["operation_id"] == entry.operation_id && record["status"] != "started"
            })
            .unwrap();
        assert_eq!(
            terminal["created_directory_dacl_report"],
            serde_json::to_value(&report).unwrap()
        );
        if result.is_err() {
            assert!(terminal["metadata_report"].is_null());
        }
        drop(owner_root);
        drop(owner_lock);
        drop(setup);
        drop(queue);
        let reopened = OperationQueue::open(&queue_path)?;
        assert_eq!(
            reopened.snapshot().entries[0].created_directory_dacl_report,
            report
        );
        let fresh = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?);
        let read_parent = if case == "final-id-swap" {
            parent.with_file_name("original-parent-held")
        } else {
            parent.clone()
        };
        assert_eq!(fresh.stat(&read_parent)?.inode, original_id);
        if !no_rights {
            assert_eq!(
                fresh.metadata_manifest(&read_parent)?.acl,
                Some(Acl::WindowsDacl(after.clone()))
            );
            assert_eq!(context(&fresh, &read_parent)?, source_context);
        }
        if completed_children {
            let source = SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?;
            let ids = wire::TEST_BEFORE_CHILDREN_FILE_IDS.with(|ids| ids.borrow().clone());
            assert_before_children_files(
                &fresh,
                &source,
                &read_parent,
                &parent,
                &source_parent,
                [&base_source, &nested_source],
                &ids,
            )?;
            if matches!(
                case,
                "optional-before-set" | "mtime-set-reply" | "mtime-readback"
            ) {
                assert_eq!(fresh.stat(&read_parent)?.mtime, Some(1_600_000_000));
            }
        } else {
            assert!(!physical
                .join(read_parent.join("file.bin").strip_prefix("/").unwrap())
                .exists());
            assert!(!physical
                .join(
                    read_parent
                        .join("sub/inside.bin")
                        .strip_prefix("/")
                        .unwrap()
                )
                .exists());
        }
        if case == "final-id-swap" {
            assert_ne!(fresh.stat(&parent)?.inode, original_id);
            assert_eq!(fresh.stat(&parent)?.mtime, Some(1_555_000_000));
            assert_eq!(
                std::fs::read(physical.join(parent.join("sentinel").strip_prefix("/").unwrap()))?,
                b"unrelated-target"
            );
        }
        if pending {
            assert!(reopened.snapshot().entries[0].created_directory_dacl_recovery);
            crate::acknowledge_created_directory_dacl_outcome(
                &reopened,
                &entry.operation_id,
                &ProviderEndpoint::smb(fresh.clone(), &target_root)?,
                true,
            )?;
            let observed = reopened.snapshot().entries.remove(0);
            assert!(!observed.created_directory_dacl_recovery);
            assert_eq!(observed.created_directory_dacl_report.completed.len(), 1);
            assert!(observed.created_directory_dacl_report.uncertain.is_empty());
            assert!(!observed.created_directory_dacl_report.created[0].ordinary_metadata_applied);
            assert!(reopened.retry(&entry.operation_id).is_err());
            assert!(!physical
                .join(parent.join("file.bin").strip_prefix("/").unwrap())
                .exists());
            assert!(!physical
                .join(parent.join("sub/inside.bin").strip_prefix("/").unwrap())
                .exists());
        }
        TEST_BEFORE_CHILDREN_SETS.with(|paths| {
            assert_eq!(
                paths.borrow().len(),
                usize::from(!no_rights),
                "Reload/ACK darf keinen SET hinzufügen"
            )
        });
        assert_eq!(
            normal_time::SET_PATHS.with(|paths| paths.borrow().clone()),
            time_sets
        );
        println!("V8_NATIVE_BOUNDARY_PASS case={case} parent_dacl_sets={} parent_mtime_sets={parent_sets} ordinary_metadata={} fresh_original_id={} readonly_reload_ack=1 no_children_resume=1 no_retry_restore_delete=1", usize::from(!no_rights), report.created[0].ordinary_metadata_applied, original_id.unwrap());
        return Ok(());
    }
    let stats = match result {
        Ok(QueueExecutionResult::Sync(stats)) => stats,
        Ok(_) => return Err(invalid("V8-Queue lieferte keine Syncstatistik")),
        Err(error) => {
            println!(
                "V8_NATIVE_ERROR code={:?} error={error:?} queue={:?}",
                crate::error_code(&error),
                queue.snapshot()
            );
            return Err(error);
        }
    };
    println!("V8_NATIVE_STATS={}", serde_json::to_string(&stats).unwrap());
    wire::TEST_BEFORE_CHILDREN_CONTEXTS.with(|contexts| {
        for (path, expected, observed) in contexts.borrow().iter() {
            println!(
                "V8_NATIVE_ACTUAL_FILE_CONTEXT path={} expected={} observed={}",
                path.display(),
                serde_json::to_string(expected).unwrap(),
                serde_json::to_string(observed).unwrap()
            );
        }
    });
    assert!(checkpoint_seen && children_seen);
    assert_eq!(stats.files_copied, 2);
    assert_eq!(stats.bytes_copied, 59);
    assert_eq!(
        stats.metadata_report, planned_report,
        "Keine zusätzliche Laufzeitauslassung"
    );
    assert_eq!(stats.metadata_report.privilege_failures, 0);
    assert!(!stats.metadata_report.strict_verified);
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    println!(
        "V8_NATIVE_COMPLETED_QUEUE={}",
        serde_json::to_string(&queue.snapshot().entries[0]).unwrap()
    );
    assert_eq!(stats.created_directory_dacl_report.completed.len(), 1);
    assert!(stats.created_directory_dacl_report.uncertain.is_empty());
    assert_eq!(stats.created_directory_dacl_report.created.len(), 1);
    let observation = &stats.created_directory_dacl_report.created[0];
    assert!(observation.ordinary_metadata_applied);
    assert_eq!(
        observation.effect_key.origin,
        CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren
    );
    assert_eq!(Some(observation.effect_key.file_id), original_id);
    assert!(!crate::created_directory_dacl_registry::has_open(
        &owner,
        &owner_root
    )?);
    TEST_BEFORE_CHILDREN_SETS
        .with(|paths| assert_eq!(paths.borrow().as_slice(), std::slice::from_ref(&parent)));
    TEST_BEFORE_CHILDREN_FINISHED
        .with(|paths| assert_eq!(paths.borrow().as_slice(), std::slice::from_ref(&parent)));
    wire::TEST_BEFORE_CHILDREN_FILE_DACL_PATHS.with(|paths| assert_eq!(paths.borrow().len(), 2));
    normal_time::SET_PATHS.with(|paths| {
        assert_eq!(paths.borrow().len(), 4);
        assert_eq!(
            normalized_wire_path(paths.borrow().last().unwrap()),
            normalized_wire_path(&parent.to_string_lossy())
        );
    });
    drop(owner_root);
    drop(owner_lock);
    drop(setup);
    let verified = SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?;
    let source = SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?;
    let expected_ids = wire::TEST_BEFORE_CHILDREN_FILE_IDS.with(|ids| ids.borrow().clone());
    assert_before_children_files(
        &verified,
        &source,
        &parent,
        &parent,
        &source_parent,
        [&base_source, &nested_source],
        &expected_ids,
    )?;
    assert_eq!(verified.stat(&parent)?.inode, original_id);
    assert_eq!(verified.stat(&parent)?.mtime, Some(1_600_000_000));
    assert_eq!(
        verified.metadata_manifest(&parent)?.acl,
        Some(Acl::WindowsDacl(after))
    );
    assert_eq!(context(&verified, &parent)?, source_context);
    assert_eq!(verified.stat(&parent)?.inode, original_id);
    let audit_report = completed_audit_metadata(&audit)?;
    assert_eq!(audit_report, stats.metadata_report);
    drop(queue);
    let reopened = OperationQueue::open(&queue_path)?;
    assert_eq!(reopened.snapshot().entries[0].state, QueueState::Completed);
    assert_eq!(
        reopened.snapshot().entries[0].created_directory_dacl_report,
        *stats.created_directory_dacl_report
    );
    println!("V8_NATIVE_PRODUCT_QUEUE_PASS original_id={} parent_dacl_sets=1 durable_checkpoint_before_finish=1 all_rights_handles_closed_before_children=1 files=2 main_bytes=40 ads_bytes=19 file_dacl_sets=2 parent_final_mtime_sets=1 fresh_readbacks_exact=1 no_retry_restore_delete=1 native_completeness_claim=0", original_id.unwrap());
    println!(
        "V8_NATIVE_CLEANUP_SCOPE=runner-owned-fresh-artificial-share local_queue_area={}",
        area.path().display()
    );
    Ok(())
}

fn assert_before_children_files(
    verified: &SmbVfs,
    source: &SmbVfs,
    parent: &Path,
    expected_parent: &Path,
    source_parent: &Path,
    manifests: [&crate::metadata::MetadataManifest; 2],
    expected_ids: &[(PathBuf, u64)],
) -> io::Result<()> {
    use std::io::Read;
    for (relative, expected, manifest) in [
        ("file.bin", b"native-main-content".as_slice(), manifests[0]),
        (
            "sub/inside.bin",
            b"native-nested-content".as_slice(),
            manifests[1],
        ),
    ] {
        let path = parent.join(relative);
        let expected_path = expected_parent.join(relative);
        let ids = expected_ids
            .iter()
            .filter(|(path, _)| *path == expected_path)
            .map(|(_, id)| *id)
            .collect::<Vec<_>>();
        assert_eq!(
            ids.len(),
            1,
            "Genau ein eigener nativer Datei-DACL-Temp je Ziel"
        );
        let id = ids[0];
        assert_eq!(verified.stat(&path)?.inode, Some(id));
        assert_eq!(
            context(verified, &path)?,
            context(source, &source_parent.join(relative))?
        );
        let mut bytes = Vec::new();
        verified.open_read(&path)?.read_to_end(&mut bytes)?;
        assert_eq!(bytes, expected);
        let actual = verified.metadata_manifest(&path)?;
        assert_eq!(actual.acl, manifest.acl);
        assert_eq!(actual.named_streams, manifest.named_streams);
        assert_eq!(verified.stat(&path)?.mtime, Some(1_700_000_200));
        assert_eq!(verified.stat(&path)?.inode, Some(id));
        println!("V8_NATIVE_FRESH_FILE relative={relative} id={id} sid_context_exact=1 content_dacl_time_stream_manifest_exact=1");
    }
    let mut ads = Vec::new();
    verified
        .open_named_stream_read(&parent.join("file.bin"), b"named")?
        .read_to_end(&mut ads)?;
    assert_eq!(ads, b"native-named-stream");
    assert!(verified.stat(&parent.join("sub"))?.is_dir);
    assert_eq!(
        verified.stat(&parent.join("sub"))?.mtime,
        source.stat(&source_parent.join("sub"))?.mtime
    );
    Ok(())
}
