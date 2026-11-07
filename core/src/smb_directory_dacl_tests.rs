use super::*;
use crate::windows_security::{WindowsDaclWriteIssue, WindowsSid};
use std::os::unix::fs::PermissionsExt;

fn model_context() -> crate::windows_dacl::WindowsDaclWriteContext {
    crate::windows_dacl::WindowsDaclWriteContext {
        namespace: [7; 16],
        owner: WindowsSid {
            revision: 1,
            authority: 5,
            sub_authorities: vec![21, 100, 1000],
        },
        group: None,
    }
}

#[test]
fn directory_encoder_requires_traversal_but_no_delete_or_data_write() {
    let context = model_context();
    let mut acl = probe_acl(&context);
    let WindowsDaclState::Acl { aces, .. } = &mut acl.state else {
        panic!()
    };
    let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
        panic!()
    };
    *mask = DIRECTORY_ACCESS;
    assert_eq!(
        acl.directory_write_issue(context.namespace, &context.owner),
        None
    );
    assert_eq!(
        acl.file_write_issue(context.namespace, &context.owner),
        Some(WindowsDaclWriteIssue::MissingOwnerAccess)
    );
    let bytes = acl
        .encode_for_directory_write(context.namespace, &context.owner)
        .unwrap();
    for offset in [4, 8, 12] {
        assert_eq!(read_u32(&bytes, offset).unwrap(), 0);
    }
    assert_eq!(
        crate::windows_security::parse_security_descriptor(&bytes, context.namespace)
            .unwrap()
            .dacl,
        acl
    );
    let WindowsDaclState::Acl { aces, .. } = &mut acl.state else {
        panic!()
    };
    let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
        panic!()
    };
    *mask &= !0x20;
    assert_eq!(
        acl.directory_write_issue(context.namespace, &context.owner),
        Some(WindowsDaclWriteIssue::MissingOwnerAccess)
    );
    assert_eq!(
        DIRECTORY_ACCESS & (FileAccessMask::DELETE | FileAccessMask::WRITE_OWNER),
        0
    );
}

struct TestSecrets;
impl SecretResolver for TestSecrets {
    fn resolve(&self, _: &crate::KeychainSecretReference) -> io::Result<crate::SecretString> {
        crate::SecretString::new("Doppeldecker-SMB-E2E-wertlos-2026".into())
    }
}
fn proof(provider: &SmbVfs, path: &Path) -> crate::sync::SyncTargetProof {
    let entry = provider.stat(path).unwrap();
    serde_json::from_value(serde_json::json!({
        "raw_name": entry.name.as_bytes(), "provider_token": null,
        "is_dir": entry.is_dir, "is_file": entry.is_file, "is_symlink": entry.is_symlink,
        "size": entry.size, "mtime": entry.mtime, "mode": entry.mode, "device": entry.device, "inode": entry.inode,
    })).unwrap()
}
fn observed(scope: &dyn Vfs, path: &Path) -> io::Result<WindowsDacl> {
    let Some(crate::metadata::Acl::WindowsDacl(acl)) = scope.metadata_manifest(path)?.acl else {
        return Err(invalid("DACL fehlt"));
    };
    Ok(acl)
}
fn seed_directory(
    provider: &SmbVfs,
    path: &Path,
    acl: &WindowsDacl,
    context: &crate::windows_dacl::WindowsDaclWriteContext,
) -> io::Result<()> {
    let remote = provider.remote_path(path)?;
    let (tree, mut connection) = {
        let state = lock(&provider.state);
        (state.session.tree.clone(), state.session.connection.clone())
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
        let result = wire::set(
            &mut connection,
            &tree,
            id,
            acl.encode_for_directory_write(context.namespace, &context.owner)?,
        )
        .await;
        tree.close_handle(&mut connection, id)
            .await
            .map_err(smb_error)?;
        result
    })
}
fn seed_ads(provider: &SmbVfs, path: &Path) -> io::Result<()> {
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
        CreateDisposition::FileCreate,
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
    writer.write_all(b"unchanged-directory-ads")?;
    writer.flush()
}

#[test]
#[ignore = "benötigt frischen privaten Samba mit optionalem acl_xattr"]
fn existing_directory_dacl_roundtrip() -> io::Result<()> {
    let required = |name| std::env::var(name).expect("Privater Directory-DACL-Runner fehlt");
    let physical = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(physical.starts_with("/tmp/doppeldecker-smb-acl-write-probe-20261029"));
    assert_eq!(physical.file_name().unwrap(), "share");
    assert!(std::fs::read_dir(&physical)?.next().is_none());
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    assert_eq!(host, "127.0.0.1");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
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
                "directory-dacl",
            )?,
        },
        Arc::new(TestSecrets),
    )?);
    {
        let state = lock(&provider.state);
        assert!(state.session.identity.signing_active && state.session.identity.encryption_active);
    }
    std::fs::create_dir_all(physical.join("outside/selected"))?;
    let root = Path::new("/outside/selected");
    let cancel = crate::Cancel::new();
    let config = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let staging = crate::local_staging::StagingOwner {
        queue: config.path().join("queue.json"),
        operation: "directory-provider-proof".into(),
    };
    let owner = || {
        Some(InteractiveFileOwner {
            staging: staging.clone(),
            profiles: vec![],
        })
    };
    let scope = provider.content_sync_mutations(root, &cancel, owner())?;
    scope.prepare_directory_dacl_write()?;
    let support = scope.directory_dacl_write_support()?;
    assert!(std::fs::read_dir(physical.join("outside/selected"))?
        .next()
        .is_none());
    assert_eq!(std::fs::read_dir(&physical)?.count(), 1);
    assert_eq!(
        scope.windows_dacl_write_support()?,
        Support::Unavailable(Issue::NotPrepared)
    );
    let exact_server = std::env::var("DOPPELDECKER_SMB_STREAM_MODULE")
        .unwrap_or_default()
        .contains("acl_xattr");
    if !exact_server {
        assert_eq!(support, Support::Unavailable(Issue::Normalized));
        println!("DIRECTORY_DACL_DEFAULT_NORMALIZED_SEPARATE_PROBE_CLEAN_OK");
        return Ok(());
    }
    let Support::Ready(context) = support else {
        panic!("Directory support: {support:?}")
    };
    let mut before = probe_acl(&context);
    let WindowsDaclState::Acl { aces, .. } = &mut before.state else {
        panic!()
    };
    let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
        panic!()
    };
    *mask = DIRECTORY_ACCESS;
    let mut after = before.clone();
    let WindowsDaclState::Acl { aces, .. } = &mut after.state else {
        panic!()
    };
    aces.swap(1, 2);
    for case in [
        "success",
        "before_open",
        "after_set",
        "applied_crash",
        "swapped",
        "access_denied",
        "group_mismatch",
        "held_rename",
    ] {
        let before = if case == "held_rename" {
            let mut value = before.clone();
            let WindowsDaclState::Acl { aces, .. } = &mut value.state else {
                panic!()
            };
            let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
                panic!()
            };
            *mask |= FileAccessMask::DELETE;
            value
        } else {
            before.clone()
        };
        let path = root.join(case);
        let physical_path = physical.join("outside/selected").join(case);
        if case == "group_mismatch" {
            std::fs::create_dir(&physical_path)?;
        } else {
            provider.create_dir_unchecked(&path)?;
        }
        std::fs::write(physical_path.join("child"), b"unchanged-child")?;
        seed_ads(&provider, &path)?;
        seed_directory(&provider, &path, &before, &context)?;
        let inode = provider.stat(&path)?.inode;
        let manifest = scope.metadata_manifest(&path)?;
        assert_eq!(
            manifest.acl,
            Some(crate::metadata::Acl::WindowsDacl(before.clone())),
            "seed before {case}"
        );
        assert_eq!(
            manifest
                .windows_security
                .as_ref()
                .and_then(|value| value.owner.as_ref()),
            Some(&context.owner),
            "seed owner {case}"
        );
        if case != "group_mismatch" {
            assert_eq!(
                manifest
                    .windows_security
                    .as_ref()
                    .and_then(|value| value.group.as_ref()),
                context.group.as_ref(),
                "seed group {case}"
            );
        }
        let plan = DirectoryDaclPlan {
            before: before.clone(),
            after: after.clone(),
            context: context.clone(),
            target_proof: Some(proof(&provider, &path)),
        };
        if case == "group_mismatch" {
            let error = scope
                .begin_directory_dacl_transaction(&path, &plan)
                .err()
                .expect("Groupdrift erwartet");
            assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
            assert_eq!(observed(scope.as_ref(), &path)?, before);
            println!("DIRECTORY_DACL_DIFFERENT_GROUP_ZERO_SET_OK");
            continue;
        }
        if case == "swapped" {
            std::fs::rename(&physical_path, physical_path.with_extension("original"))?;
            std::fs::create_dir(&physical_path)?;
            let replacement_before = scope.metadata_manifest(&path)?.acl;
            assert!(scope
                .begin_directory_dacl_transaction(&path, &plan)
                .is_err());
            assert_eq!(scope.metadata_manifest(&path)?.acl, replacement_before);
            assert_eq!(
                observed(scope.as_ref(), &root.join("swapped.original"))?,
                before
            );
            println!("DIRECTORY_DACL_FOREIGN_SWAP_ZERO_SET_OK");
            continue;
        }
        if case == "access_denied" {
            std::fs::set_permissions(&physical_path, std::fs::Permissions::from_mode(0o000))?;
            let result = scope.begin_directory_dacl_transaction(&path, &plan);
            std::fs::set_permissions(&physical_path, std::fs::Permissions::from_mode(0o700))?;
            let error = result.err().expect("Realer Zugriff muss vor SET scheitern");
            assert_eq!(
                crate::windows_dacl::known_write_omission(&error),
                Some(Issue::RequiredAccessDenied)
            );
            println!("DIRECTORY_DACL_ACTUAL_ACCESS_DENIED_BEFORE_SET_OK");
            continue;
        }
        let foreign = Arc::new(SmbVfs::connect(
            provider.config.clone(),
            Arc::new(TestSecrets),
        )?);
        if case == "held_rename" {
            let moved = root.join("baseline-rename");
            foreign.rename_noreplace_unchecked(&path, &moved)?;
            foreign.rename_noreplace_unchecked(&moved, &path)?;
            let original = path.clone();
            let foreign = foreign.clone();
            TEST_RECOVERY_HELD.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    assert!(foreign
                        .rename_noreplace_unchecked(&original, &moved)
                        .is_err());
                }));
            });
            let observed =
                provider.directory_dacl_recovery_observation(&path, &provider.root_identity(root)?);
            TEST_RECOVERY_HELD.with(|hook| {
                hook.borrow_mut().take();
            });
            let observed = observed?;
            assert_eq!(Some(observed.base_id), inode);
            assert_eq!(observed.context, context);
            assert_eq!(observed.dacl, before);
            println!("DIRECTORY_DACL_RECOVERY_SAME_HANDLE_ID_SIDS_DACL_FOREIGN_RENAME_BLOCKED_OK");
        }
        TEST_FAULT.with(|fault| {
            fault.set(if case == "before_open" {
                Some("before_open")
            } else {
                None
            })
        });
        let transaction = scope.begin_directory_dacl_transaction(&path, &plan);
        TEST_FAULT.with(|fault| fault.set(None));
        if case == "before_open" {
            let error = transaction.err().expect("Known denial expected");
            assert_eq!(
                crate::windows_dacl::known_write_omission(&error),
                Some(Issue::RequiredAccessDenied)
            );
            assert_eq!(observed(scope.as_ref(), &path)?, before);
            println!("DIRECTORY_DACL_KNOWN_DENIAL_ZERO_SET_OK");
            continue;
        }
        let mut transaction = transaction?;
        assert!(foreign
            .rename_noreplace_unchecked(&path, &root.join("foreign-moved"))
            .is_err());
        if case == "held_rename" {
            println!("DIRECTORY_DACL_FOREIGN_RENAME_BASELINE_ALLOWED_HELD_DENIED_OK");
        }
        TEST_FAULT.with(|fault| {
            fault.set(if case == "after_set" {
                Some("after_set")
            } else {
                None
            })
        });
        let result = transaction.apply();
        TEST_FAULT.with(|fault| fault.set(None));
        if case == "after_set" {
            assert!(result.is_err());
            assert_eq!(transaction.report().uncertain.len(), 1);
            let recovery = transaction.recovery_paths();
            let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&recovery[1])?)?;
            assert_eq!(value["directory_dacl"]["phase"], "Applying");
            drop(transaction);
            let account = lock(&provider.state).session.identity.account_digest;
            let report = crate::local_staging::finish_directory_dacl_recovery(
                &staging,
                scope.as_ref(),
                root,
                account,
            )?;
            assert!(report
                .completed
                .iter()
                .any(|outcome| outcome.directory == path.to_string_lossy()));
            println!("DIRECTORY_DACL_AFTER_SET_LOSS_DURABLE_UNCERTAIN_EXPLICIT_RECOVERY_OK");
        } else if case == "applied_crash" {
            assert_eq!(result?.completed.len(), 1);
            let recovery = transaction.recovery_paths();
            let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&recovery[1])?)?;
            assert_eq!(value["directory_dacl"]["phase"], "Applied");
            drop(transaction);
            let account = lock(&provider.state).session.identity.account_digest;
            let report = crate::local_staging::finish_directory_dacl_recovery(
                &staging,
                scope.as_ref(),
                root,
                account,
            )?;
            assert!(report.uncertain.is_empty());
            assert!(report
                .completed
                .iter()
                .any(|outcome| outcome.directory == path.to_string_lossy()));
            println!("DIRECTORY_DACL_APPLIED_CRASH_DURABLE_COMPLETED_RECOVERY_OK");
        } else {
            assert_eq!(result?.completed.len(), 1);
            assert_eq!(transaction.finish()?.completed.len(), 1);
            let remote = provider.remote_path(&path)?;
            let (tree, mut connection) = {
                let state = lock(&provider.state);
                (state.session.tree.clone(), state.session.connection.clone())
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
                let denied = wire::set(
                    &mut connection,
                    &tree,
                    id,
                    after.encode_for_directory_write(context.namespace, &context.owner)?,
                )
                .await
                .unwrap_err();
                assert_eq!(
                    crate::windows_dacl::known_write_omission(&denied),
                    Some(Issue::RequiredAccessDenied)
                );
                wire::exact(
                    &wire::query(&mut connection, &tree, id, context.namespace).await?,
                    &after,
                    &context,
                )?;
                tree.close_handle(&mut connection, id)
                    .await
                    .map_err(smb_error)?;
                Ok::<_, io::Error>(())
            })?;
            println!("DIRECTORY_DACL_RAW_HANDLE_WITHOUT_WRITE_DAC_SET_DENIED_UNCHANGED_DACL_OWNER_GROUP_OK");
            println!("DIRECTORY_DACL_EXACT_FRESH_REOPEN_NO_DELETE_OWNER_GROUP_ID_CHILD_ADS_OK");
        }
        assert_eq!(provider.stat(&path)?.inode, inode);
        let fresh = scope.metadata_manifest(&path)?;
        assert_eq!(
            fresh.acl,
            Some(crate::metadata::Acl::WindowsDacl(after.clone()))
        );
        assert_eq!(fresh.windows_security, manifest.windows_security);
        assert_eq!(
            std::fs::read(physical_path.join("child"))?,
            b"unchanged-child"
        );
        let mut ads = Vec::new();
        scope
            .open_named_stream_read(&path, b"named")?
            .read_to_end(&mut ads)?;
        assert_eq!(ads, b"unchanged-directory-ads");
    }
    Ok(())
}

#[path = "smb_directory_dacl_queue_tests.rs"]
mod queue;
