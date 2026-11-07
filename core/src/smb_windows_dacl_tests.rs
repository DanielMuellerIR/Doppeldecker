use super::*;

#[test]
fn probe_preserves_noncanonical_explicit_deny_without_write_owner_access() {
    let context = Context {
        namespace: [7; 16],
        owner: WindowsSid {
            revision: 1,
            authority: 5,
            sub_authorities: vec![21, 100, 1000],
        },
        group: Some(WindowsSid {
            revision: 1,
            authority: 5,
            sub_authorities: vec![21, 100, 1001],
        }),
    };
    let acl = probe_acl(&context);
    assert!(acl
        .file_write_issue(context.namespace, &context.owner)
        .is_none());
    let bytes = acl
        .encode_for_file_write(context.namespace, &context.owner)
        .unwrap();
    assert_eq!(FILE_ACCESS & FileAccessMask::WRITE_OWNER, 0);
    for offset in [4, 8, 12] {
        assert_eq!(read_u32(&bytes, offset).unwrap(), 0);
    }
    let parsed =
        crate::windows_security::parse_security_descriptor(&bytes, context.namespace).unwrap();
    assert_eq!(parsed.dacl, acl);
    assert!(parsed.owner.is_none() && parsed.group.is_none());
    let WindowsDaclState::Acl { aces, .. } = parsed.dacl.state else {
        panic!()
    };
    assert!(matches!(aces[0], WindowsAce::Allow { .. }));
    assert!(matches!(
        aces[1],
        WindowsAce::Deny {
            mask: FileAccessMask::WRITE_OWNER,
            ..
        }
    ));
}

struct TestSecrets;
impl SecretResolver for TestSecrets {
    fn resolve(&self, _: &crate::KeychainSecretReference) -> io::Result<crate::SecretString> {
        crate::SecretString::new("Doppeldecker-SMB-E2E-wertlos-2026".into())
    }
}

#[test]
#[ignore = "benötigt privaten frischen Samba mit optionalem acl_xattr"]
fn ordinary_file_dacl_roundtrip() -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let required = |name| std::env::var(name).expect("Privater DACL-Proberunner erforderlich");
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
                "smb-dacl",
            )?,
        },
        Arc::new(TestSecrets),
    )?);
    std::fs::create_dir_all(physical.join("outside/selected"))?;
    let selected = Path::new("/outside/selected");
    let cancel = crate::Cancel::new();
    let no_owner = provider.content_sync_mutations(selected, &cancel, None)?;
    no_owner.prepare_windows_dacl_write()?;
    assert_eq!(
        no_owner.windows_dacl_write_support()?,
        Support::Unavailable(Issue::NoStagingOwner)
    );
    let config = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let owner = || {
        Some(InteractiveFileOwner {
            staging: crate::local_staging::StagingOwner {
                queue: config.path().join("queue.json"),
                operation: "dacl-provider-test".into(),
            },
            profiles: vec![],
        })
    };
    let target = provider.content_sync_mutations(selected, &cancel, owner())?;
    assert_eq!(
        target.windows_dacl_write_support()?,
        Support::Unavailable(Issue::NotPrepared)
    );
    assert_eq!(
        target.metadata_capabilities(selected)?.acl,
        Capability::ReadOnly
    );
    assert!(std::fs::read_dir(physical.join("outside/selected"))?
        .next()
        .is_none());
    target.prepare_windows_dacl_write()?;
    // Alle Proben bleiben unter dem ausgewählten Root und schließen ihre Intents.
    assert!(std::fs::read_dir(physical.join("outside/selected"))?
        .next()
        .is_none());
    assert_eq!(std::fs::read_dir(&physical)?.count(), 1);
    let ready = target.windows_dacl_write_support()?;
    target.prepare_windows_dacl_write()?;
    assert_eq!(target.windows_dacl_write_support()?, ready);
    let source = provider.content_sync_reader(selected, &cancel, owner())?;
    source.prepare_windows_dacl_write()?;
    assert_eq!(
        source.windows_dacl_write_support()?,
        Support::Unavailable(Issue::UnsupportedBackend)
    );
    assert!(matches!(
        provider.windows_dacl_write_support()?,
        Support::Unavailable(Issue::UnsupportedBackend)
    ));
    let exact_server = std::env::var("DOPPELDECKER_SMB_STREAM_MODULE")
        .unwrap_or_default()
        .contains("acl_xattr");
    if !exact_server {
        assert_eq!(ready, Support::Unavailable(Issue::Normalized));
        let final_path = selected.join("fallback");
        let temp = selected.join(".fallback-temp");
        let scope = target.staging_scope(&final_path, None)?.unwrap();
        let mut writer = scope.open_write_new(&temp)?;
        writer.write_all(b"fallback-content")?;
        writer.flush()?;
        drop(writer);
        let mut stream = scope.open_named_stream_write_new(&temp, b"named")?;
        stream.write_all(b"fallback-ads")?;
        stream.flush()?;
        drop(stream);
        scope.rename_noreplace(&temp, &final_path)?;
        scope.finish_staging()?;
        assert_eq!(
            std::fs::read(physical.join("outside/selected/fallback"))?,
            b"fallback-content"
        );
        provider.remove_file_unchecked(&final_path)?;
        println!("DACL_DEFAULT_NORMALIZED_CLEAN_FALLBACK_CONTENT_ADS_OK");
    } else {
        let Support::Ready(expected) = ready else {
            panic!("Exact-Ziel blieb unvorbereitet: {ready:?}")
        };
        let acl = probe_acl(&expected);
        let foreign = Arc::new(SmbVfs::connect(
            provider.config.clone(),
            Arc::new(TestSecrets),
        )?);
        assert_ne!(
            lock(&provider.state).session.connection.session_id(),
            lock(&foreign.state).session.connection.session_id()
        );
        for fault in [
            None,
            Some("before_open"),
            Some("different_context"),
            Some("after_set"),
            Some("normalized"),
            Some("reopen_denied"),
            Some("invalid_frame"),
        ] {
            let name = fault.unwrap_or("success");
            let final_path = selected.join(name);
            let temp = selected.join(format!(".{name}-temp"));
            let scope = target.staging_scope(&final_path, None)?.unwrap();
            let mut writer = scope.open_write_new(&temp)?;
            writer.write_all(b"content")?;
            writer.flush()?;
            drop(writer);
            let before = scope.metadata_manifest(&temp)?;
            let mut stream = scope.open_named_stream_write_new(&temp, b"named")?;
            stream.write_all(b"ads")?;
            stream.flush()?;
            drop(stream);
            assert!(target
                .write_windows_dacl_exact(&temp, &acl, &expected)
                .is_err());
            assert!(scope
                .write_windows_dacl_exact(&final_path, &acl, &expected)
                .is_err());
            if fault.is_none() {
                let foreign = foreign.clone();
                let original = temp.clone();
                let moved = selected.join("foreign-moved");
                TEST_BEFORE_SET.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        assert!(
                            foreign
                                .rename_noreplace_unchecked(&original, &moved)
                                .is_err(),
                            "Fremde Sitzung konnte den eigenen Temp vor SET verschieben"
                        );
                        assert!(foreign.stat(&original).is_ok());
                        assert_eq!(
                            foreign.stat(&moved).unwrap_err().kind(),
                            io::ErrorKind::NotFound
                        );
                    }))
                });
            }
            TEST_FAULT.with(|value| value.set(fault));
            let written = scope.write_windows_dacl_exact(&temp, &acl, &expected);
            TEST_FAULT.with(|value| value.set(None));
            match fault {
                None => {
                    written?;
                    let remote = provider.remote_path(&temp)?;
                    let (tree, mut connection) = {
                        let state = lock(&provider.state);
                        (state.session.tree.clone(), state.session.connection.clone())
                    };
                    provider.runtime.block_on(async {
                        let id = open(
                            &mut connection,
                            &tree,
                            &remote,
                            FileAccessMask::READ_CONTROL
                                | FileAccessMask::FILE_READ_DATA
                                | FileAccessMask::FILE_READ_ATTRIBUTES
                                | FileAccessMask::SYNCHRONIZE,
                            false,
                            false,
                        )
                        .await?;
                        let before = query(&mut connection, &tree, id, expected.namespace).await?;
                        let denied = set(
                            &mut connection,
                            &tree,
                            id,
                            acl.encode_for_file_write(expected.namespace, &expected.owner)?,
                        )
                        .await
                        .unwrap_err();
                        assert_eq!(
                            crate::windows_dacl::known_write_omission(&denied),
                            Some(Issue::RequiredAccessDenied)
                        );
                        assert_eq!(
                            query(&mut connection, &tree, id, expected.namespace).await?,
                            before
                        );
                        tree.close_handle(&mut connection, id)
                            .await
                            .map_err(smb_error)
                    })?;
                    let observed = scope.metadata_manifest(&temp)?;
                    assert_eq!(observed.acl, Some(Acl::WindowsDacl(acl.clone())));
                    assert_eq!(observed.windows_security, before.windows_security);
                    assert!(scope
                        .write_windows_dacl_exact(&temp, &acl, &expected)
                        .is_err());
                    scope.rename_noreplace(&temp, &final_path)?;
                    scope.finish_staging()?;
                    assert_eq!(
                        std::fs::read(physical.join("outside/selected/success"))?,
                        b"content"
                    );
                    let mut ads = Vec::new();
                    target
                        .open_named_stream_read(&final_path, b"named")?
                        .read_to_end(&mut ads)?;
                    assert_eq!(ads, b"ads");
                    provider.remove_file_unchecked(&final_path)?;
                }
                Some("before_open") | Some("different_context") => {
                    assert_eq!(
                        crate::windows_dacl::known_write_omission(&written.unwrap_err()),
                        Some(if fault == Some("different_context") {
                            Issue::DifferentTempContext
                        } else {
                            Issue::RequiredAccessDenied
                        })
                    );
                    assert_eq!(scope.metadata_manifest(&temp)?.acl, before.acl);
                    assert!(scope.cleanup_owned_temp(&temp)?);
                    scope.finish_staging()?;
                }
                _ => {
                    let error = written.unwrap_err();
                    assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
                    assert!(scope.staging_recovery_path().unwrap().exists());
                    assert!(
                        std::fs::read_to_string(scope.staging_recovery_path().unwrap())?
                            .contains(&temp.to_string_lossy().to_string())
                    );
                    assert!(!scope.cleanup_owned_temp(&temp)?);
                    assert!(scope.rename_noreplace(&temp, &final_path).is_err());
                    assert!(scope.finish_staging().is_err());
                    assert!(scope
                        .write_windows_dacl_exact(&temp, &acl, &expected)
                        .is_err());
                    provider.remove_file_unchecked(&temp)?;
                }
            }
        }
        for via_writer in [false, true] {
            let label = if via_writer {
                "swap-before-dacl"
            } else {
                "swap-before-remove"
            };
            let final_path = selected.join(label);
            let temp = selected.join(format!(".{label}-temp"));
            let moved = selected.join(format!(".{label}-moved"));
            let scope = target.staging_scope(&final_path, None)?.unwrap();
            let mut own = scope.open_write_new(&temp)?;
            own.write_all(b"own-original")?;
            own.flush()?;
            drop(own);
            let own_inode = scope.stat(&temp)?.inode;
            foreign.rename_noreplace_unchecked(&temp, &moved)?;
            let mut sentinel = foreign.open_write_new_unchecked(&temp)?;
            sentinel.write_all(b"foreign-sentinel")?;
            sentinel.flush()?;
            drop(sentinel);
            let sentinel_inode = foreign.stat(&temp)?.inode;
            assert_ne!(sentinel_inode, own_inode);
            let sentinel_manifest = target.metadata_manifest(&temp)?;
            if via_writer {
                let error = scope
                    .write_windows_dacl_exact(&temp, &acl, &expected)
                    .unwrap_err();
                assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            }
            let error = scope.remove_file(&temp).unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            assert!(!scope.cleanup_owned_temp(&temp)?);
            assert!(scope.rename_noreplace(&temp, &final_path).is_err());
            assert!(scope.finish_staging().is_err());
            assert!(scope.staging_recovery_path().unwrap().exists());
            assert_eq!(foreign.stat(&temp)?.inode, sentinel_inode);
            assert_eq!(target.metadata_manifest(&temp)?, sentinel_manifest);
            let mut content = Vec::new();
            foreign.open_read(&temp)?.read_to_end(&mut content)?;
            assert_eq!(content, b"foreign-sentinel");
            assert_eq!(foreign.stat(&moved)?.inode, own_inode);
            let mut original = Vec::new();
            foreign.open_read(&moved)?.read_to_end(&mut original)?;
            assert_eq!(original, b"own-original");
            foreign.remove_file_unchecked(&temp)?;
            foreign.remove_file_unchecked(&moved)?;
        }
        for replace in [false, true] {
            let label = if replace {
                "post-dacl-replace"
            } else {
                "post-dacl-noreplace"
            };
            let final_path = selected.join(label);
            let temp = selected.join(format!(".{label}-temp"));
            let moved = selected.join(format!(".{label}-moved"));
            let existing = if replace {
                let mut writer = foreign.open_write_new_unchecked(&final_path)?;
                writer.write_all(b"existing-target")?;
                writer.flush()?;
                drop(writer);
                Some((
                    foreign.stat(&final_path)?.inode,
                    target.metadata_manifest(&final_path)?,
                ))
            } else {
                None
            };
            let scope = target.staging_scope(&final_path, None)?.unwrap();
            let mut writer = scope.open_write_new(&temp)?;
            writer.write_all(b"post-dacl-original")?;
            writer.flush()?;
            drop(writer);
            scope.write_windows_dacl_exact(&temp, &acl, &expected)?;
            let own_inode = foreign.stat(&temp)?.inode;
            foreign.rename_noreplace_unchecked(&temp, &moved)?;
            let mut sentinel = foreign.open_write_new_unchecked(&temp)?;
            sentinel.write_all(b"post-dacl-sentinel")?;
            sentinel.flush()?;
            drop(sentinel);
            let sentinel_inode = foreign.stat(&temp)?.inode;
            let sentinel_manifest = target.metadata_manifest(&temp)?;
            let result = if replace {
                scope.rename_replace_plain_once(&temp, &final_path)
            } else {
                scope.rename_noreplace(&temp, &final_path)
            };
            assert_eq!(
                crate::error_code(&result.unwrap_err()),
                crate::ErrorCode::OutcomeUnclear
            );
            assert_eq!(foreign.stat(&temp)?.inode, sentinel_inode);
            assert_eq!(target.metadata_manifest(&temp)?, sentinel_manifest);
            let mut bytes = Vec::new();
            foreign.open_read(&temp)?.read_to_end(&mut bytes)?;
            assert_eq!(bytes, b"post-dacl-sentinel");
            assert_eq!(foreign.stat(&moved)?.inode, own_inode);
            let mut original = Vec::new();
            foreign.open_read(&moved)?.read_to_end(&mut original)?;
            assert_eq!(original, b"post-dacl-original");
            if let Some((inode, manifest)) = existing {
                assert_eq!(foreign.stat(&final_path)?.inode, inode);
                assert_eq!(target.metadata_manifest(&final_path)?, manifest);
                let mut bytes = Vec::new();
                foreign.open_read(&final_path)?.read_to_end(&mut bytes)?;
                assert_eq!(bytes, b"existing-target");
                foreign.remove_file_unchecked(&final_path)?;
            } else {
                assert_eq!(
                    foreign.stat(&final_path).unwrap_err().kind(),
                    io::ErrorKind::NotFound
                );
            }
            assert!(scope.remove_file(&temp).is_err());
            assert!(!scope.cleanup_owned_temp(&temp)?);
            assert!(scope.finish_staging().is_err());
            assert!(scope.staging_recovery_path().unwrap().exists());
            foreign.remove_file_unchecked(&temp)?;
            foreign.remove_file_unchecked(&moved)?;
        }
        println!("SMB_DACL_POSTSET_SWAP_COMMIT_NOREPLACE_REPLACE_SENTINEL_AND_EXISTING_TARGET_PRESERVED_OK");
        let cleanup_cancel = crate::Cancel::new();
        let cleanup_context =
            provider.content_sync_mutations(selected, &cleanup_cancel, owner())?;
        let cleanup_temp = selected.join(".cancel-owned-temp");
        let cleanup_scope = cleanup_context
            .staging_scope(&selected.join("cancel-final"), None)?
            .unwrap();
        let mut writer = cleanup_scope.open_write_new(&cleanup_temp)?;
        writer.write_all(b"cancel-own")?;
        writer.flush()?;
        drop(writer);
        cleanup_cancel.cancel();
        cleanup_scope.remove_file(&cleanup_temp)?;
        cleanup_scope.finish_staging()?;
        assert_eq!(
            provider.stat(&cleanup_temp).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        println!("SMB_GENERIC_TEMP_CLEANUP_SWAP_SENTINEL_ID_ACL_DATA_UNCHANGED_DACL_ID_DRIFT_UNCLEAR_CANCEL_OWN_DELETE_OK");
        let alternate_group = rustix::process::getgroups()?
            .into_iter()
            .find(|group| *group != rustix::process::getegid())
            .expect("SGID-Gegenlauf braucht eine bestehende zusätzliche Testprozessgruppe");
        let child = selected.join("sgid");
        let physical_child = physical.join("outside/selected/sgid");
        std::fs::create_dir(&physical_child)?;
        rustix::fs::chown(&physical_child, None, Some(alternate_group))?;
        std::fs::set_permissions(&physical_child, std::fs::Permissions::from_mode(0o2770))?;
        let final_path = child.join("file");
        let temp = child.join(".temp");
        let scope = target.staging_scope(&final_path, None)?.unwrap();
        let mut writer = scope.open_write_new(&temp)?;
        writer.write_all(b"sgid-content")?;
        writer.flush()?;
        drop(writer);
        let before = scope.metadata_manifest(&temp)?;
        let parent_metadata = std::fs::metadata(&physical_child)?;
        assert_eq!(parent_metadata.gid(), alternate_group.as_raw());
        assert_ne!(parent_metadata.permissions().mode() & 0o2000, 0);
        let file_gid_before = std::fs::metadata(physical_child.join(".temp"))?.gid();
        let windows_context_differs =
            before.windows_security.as_ref().unwrap().group != expected.group;
        let written = scope.write_windows_dacl_exact(&temp, &acl, &expected);
        if windows_context_differs {
            assert_eq!(
                crate::windows_dacl::known_write_omission(&written.unwrap_err()),
                Some(Issue::DifferentTempContext)
            );
            assert_eq!(scope.metadata_manifest(&temp)?.acl, before.acl);
        } else {
            written?;
            assert_eq!(
                scope.metadata_manifest(&temp)?.acl,
                Some(Acl::WindowsDacl(acl.clone()))
            );
        }
        assert_eq!(
            scope.metadata_manifest(&temp)?.windows_security,
            before.windows_security
        );
        assert_eq!(
            std::fs::metadata(physical_child.join(".temp"))?.gid(),
            file_gid_before
        );
        println!("DACL_SYNTHETIC_WINDOWS_GROUP_MISMATCH_OMITTED_BEFORE_SET_OK");
        println!(
            "DACL_SGID_PARENT_OBSERVED_GID_PRESERVED_OK file_gid={file_gid_before} parent_gid={} actual_windows_context_differs={windows_context_differs}", alternate_group.as_raw()
        );
        scope.rename_noreplace(&temp, &final_path)?;
        scope.finish_staging()?;
        assert_eq!(std::fs::read(physical_child.join("file"))?, b"sgid-content");
        provider.remove_file_unchecked(&final_path)?;
        provider.remove_dir_unchecked(&child)?;
        println!("DACL_EXACT_OWN_TEMP_OWNER_GROUP_DATA_ADS_FRESH_REOPEN_COMMIT_OK DENIED_PRESET_OMITTED POSTSET_TRANSPORT_NORMALIZATION_UNCLEAR_INTENT_OPEN_OK");
    }
    assert!(std::fs::read_dir(physical.join("outside/selected"))?
        .next()
        .is_none());
    provider.reconnect()?;
    assert_eq!(
        crate::error_code(&target.windows_dacl_write_support().unwrap_err()),
        crate::ErrorCode::PlanDrift
    );
    provider.remove_dir_unchecked(selected)?;
    provider.remove_dir_unchecked(Path::new("/outside"))?;
    assert!(std::fs::read_dir(&physical)?.next().is_none());
    Ok(())
}

#[path = "smb_windows_dacl_queue_tests.rs"]
mod queue;
