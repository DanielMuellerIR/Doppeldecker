use super::*;
use crate::file_dacl::{FileDaclAliasScope, FileDaclPlan};
use crate::metadata::{Acl, BlobSummary};
use crate::windows_security::{WindowsAce, WindowsDaclState, WindowsSid};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

struct TestSecrets;
impl SecretResolver for TestSecrets {
    fn resolve(&self, _: &crate::KeychainSecretReference) -> io::Result<crate::SecretString> {
        crate::SecretString::new("Doppeldecker-SMB-E2E-wertlos-2026".into())
    }
}
fn proof(provider: &SmbVfs, path: &Path) -> crate::sync::SyncTargetProof {
    let entry = provider.stat(path).unwrap();
    serde_json::from_value(serde_json::json!({"raw_name":entry.name.as_bytes(),"provider_token":null,"is_dir":entry.is_dir,"is_file":entry.is_file,"is_symlink":entry.is_symlink,"size":entry.size,"mtime":entry.mtime,"mode":entry.mode,"device":entry.device,"inode":entry.inode})).unwrap()
}
fn acl(context: &crate::windows_dacl::WindowsDaclWriteContext, after: bool) -> WindowsDacl {
    WindowsDacl {
        namespace: context.namespace,
        control: 0x1004,
        state: WindowsDaclState::Acl {
            revision: 2,
            aces: vec![
                WindowsAce::Allow {
                    flags: 0,
                    mask: FILE_INPLACE_ACCESS,
                    sid: context.owner.clone(),
                },
                WindowsAce::Allow {
                    flags: 0,
                    mask: if after { 0x81 } else { 0x80 },
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
fn seed_with_access(
    provider: &SmbVfs,
    path: &Path,
    acl: &WindowsDacl,
    context: &crate::windows_dacl::WindowsDaclWriteContext,
    access: u32,
) -> io::Result<()> {
    let remote = provider.remote_path(path)?;
    let (tree, mut connection) = {
        let state = lock(&provider.state);
        (state.session.tree.clone(), state.session.connection.clone())
    };
    provider.runtime.block_on(async {
        // Reine Fixtureinitialisierung: der Seed hält keine produktive Share-Schranke.
        let id = wire::open(&mut connection, &tree, &remote, access, false, false)
            .await
            .map_err(|error| {
                crate::error::with_context(
                    error,
                    "Testseed: vorhandene Datei mit ausdrücklich gewähltem Fixturezugriff öffnen",
                )
            })?;
        let result = wire::set(
            &mut connection,
            &tree,
            id,
            acl.encode_for_file_inplace_write(context.namespace, &context.owner)?,
        )
        .await
        .map_err(|error| {
            crate::error::with_context(error, "Testseed: explizite Vorher-DACL setzen")
        });
        tree.close_handle(&mut connection, id)
            .await
            .map_err(smb_error)?;
        result
    })
}
fn seed_initial(
    provider: &SmbVfs,
    path: &Path,
    acl: &WindowsDacl,
    context: &crate::windows_dacl::WindowsDaclWriteContext,
) -> io::Result<()> {
    // Nur frische künstliche Dateien: vor restriktiven Rechten gleiche feste Zeiten
    // setzen. Laufzeit des Setups darf keine Update-Versionsentscheidung erzeugen.
    provider.set_mtime_unchecked(path, 1_700_000_000)?;
    // Den Ausgangszustand unabhängig vom neuen minimalen Produktvertrag mit dem
    // bisher belegten breiten Zugriff herstellen.
    seed_with_access(provider, path, acl, context, 0x0017_01ff)
}
fn seed(
    provider: &SmbVfs,
    path: &Path,
    acl: &WindowsDacl,
    context: &crate::windows_dacl::WindowsDaclWriteContext,
) -> io::Result<()> {
    seed_with_access(provider, path, acl, context, FILE_INPLACE_ACCESS)
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
    writer.write_all(b"unchanged-file-ads")?;
    writer.flush()
}
/// Gegenbeweis zur Einzelnamensannahme: unabhängige SMB-Sitzung, nur Metadatenzugriff,
/// FileLinkInformation11 bei weiterhin gehaltenem Inplace- und Elternhandle.
fn add_link(provider: &SmbVfs, source: &Path, target: &Path) -> io::Result<()> {
    let remote = provider.remote_path(source)?;
    let target = provider.remote_path(target)?;
    let (tree, mut connection) = {
        let state = lock(&provider.state);
        (state.session.tree.clone(), state.session.connection.clone())
    };
    provider.runtime.block_on(async {
        let id = wire::open(&mut connection, &tree, &remote, 0x0012_0180, false, true)
            .await
            .map_err(|error| {
                crate::error::with_context(
                    error,
                    "Fremdsitzung: Linkquelle mit belegtem Metadatenzugriff 0x00120180 öffnen",
                )
            })?;
        let result = async {
            let request = SetInfoRequest {
                info_type: InfoType::File,
                file_info_class: 11,
                additional_information: 0,
                file_id: id,
                buffer: rename_buffer(&target, false),
            };
            let frame = connection
                .execute(Command::SetInfo, &request, Some(tree.tree_id))
                .await
                .map_err(smb_error)?;
            require_status(&frame, Command::SetInfo).map_err(|error| {
                crate::error::with_context(error, "Fremdsitzung: FileLinkInformation11 ausführen")
            })?;
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

#[test]
#[ignore = "benötigt frischen privaten Samba mit acl_xattr und streams_xattr"]
fn existing_file_dacl_object_wide_roundtrip() -> io::Result<()> {
    let required = |name| std::env::var(name).expect("Privater Datei-DACL-Runner fehlt");
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
        password_reference: crate::KeychainSecretReference::new("Doppeldecker-Test", "file-dacl")?,
    };
    let provider = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets))?);
    let foreign = Arc::new(SmbVfs::connect(config, Arc::new(TestSecrets))?);
    assert_ne!(
        lock(&provider.state).session.connection.session_id(),
        lock(&foreign.state).session.connection.session_id()
    );
    for name in ["pane", "external"] {
        std::fs::create_dir(physical.join(name))?;
        std::fs::set_permissions(physical.join(name), std::fs::Permissions::from_mode(0o777))?;
    }
    let root = Path::new("/pane");
    let cancel = crate::Cancel::new();
    for case in [
        "success",
        "after_set",
        "applied_crash",
        "before_open",
        "swapped",
        "third_state",
    ] {
        let config = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let staging = crate::local_staging::StagingOwner {
            queue: config.path().join("queue.json"),
            operation: format!("file-{case}"),
        };
        let scope = provider.content_sync_mutations(
            root,
            &cancel,
            Some(InteractiveFileOwner {
                staging: staging.clone(),
                profiles: vec![],
            }),
        )?;
        println!("FILE_DACL_PROVIDER_CASE_START case={case}");
        scope.prepare_file_dacl_write().map_err(|error| {
            crate::error::with_context(error, format!("{case}: Dateikontext vorbereiten"))
        })?;
        assert!(matches!(
            scope.file_dacl_write_support()?,
            Support::Ready(_)
        ));
        assert_eq!(
            scope.windows_dacl_write_support()?,
            Support::Unavailable(Issue::NotPrepared)
        );
        let path = root.join(case);
        write_probe(&provider, &path, b"unchanged-file-data").map_err(|error| {
            crate::error::with_context(error, format!("{case}: künstlichen Hauptinhalt anlegen"))
        })?;
        seed_ads(&provider, &path).map_err(|error| {
            crate::error::with_context(error, format!("{case}: künstlichen ADS anlegen"))
        })?;
        // Der breite Initialseed benötigt auch die im alten FILE_ACCESS enthaltene
        // Ausführungserlaubnis. Nur diese frische künstliche Datei wird vorbereitet.
        std::fs::set_permissions(
            physical.join("pane").join(case),
            std::fs::Permissions::from_mode(0o700),
        )?;
        let manifest = scope.metadata_manifest(&path).map_err(|error| {
            crate::error::with_context(error, format!("{case}: initiale Security-/ADS-Beobachtung"))
        })?;
        let security = manifest.windows_security.as_ref().unwrap();
        let context = crate::windows_dacl::WindowsDaclWriteContext {
            namespace: security.namespace,
            owner: security.owner.clone().unwrap(),
            group: security.group.clone(),
        };
        let mut before = acl(&context, false);
        let mut after = acl(&context, true);
        if case == "success" {
            for dacl in [&mut before, &mut after] {
                let WindowsDaclState::Acl { aces, .. } = &mut dacl.state else {
                    panic!()
                };
                let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
                    panic!()
                };
                *mask |= FileAccessMask::FILE_WRITE_ATTRIBUTES;
            }
        }
        seed_initial(&provider, &path, &before, &context).map_err(|error| {
            crate::error::with_context(
                error,
                format!("{case}: Vorherrechte des Testobjekts setzen"),
            )
        })?;
        assert!(before
            .encode_for_file_write(context.namespace, &context.owner)
            .is_err());
        let manifest = scope.metadata_manifest(&path).map_err(|error| {
            crate::error::with_context(error, format!("{case}: Vorherrechte und ADS frisch lesen"))
        })?;
        let file_id = provider.stat(&path)?.inode.unwrap();
        let identity = scope.root_identity(root)?;
        let mut plan = FileDaclPlan {
            before: before.clone(),
            after: after.clone(),
            context: context.clone(),
            target_proof: proof(&provider, &path),
            file_id,
            object_namespace: crate::file_dacl_registry::object_namespace(&identity)?,
            content: BlobSummary::from_bytes(b"unchanged-file-data"),
            streams: manifest.named_streams.clone().unwrap(),
            alias_scope: FileDaclAliasScope {
                selected_paths: vec![case.into()],
                observed_link_count: provider.stat(&path)?.hard_links,
                all_existing_and_future_names_including_outside_root: true,
            },
        };
        assert_eq!(plan.alias_scope.observed_link_count, Some(1));
        if case == "swapped" {
            std::fs::rename(
                physical.join("pane").join(case),
                physical.join("pane/swapped-original"),
            )?;
            write_probe(&provider, &path, b"replacement")?;
            assert!(scope.begin_file_dacl_transaction(&path, &plan).is_err());
            assert_eq!(
                std::fs::read(physical.join("pane/swapped"))?,
                b"replacement"
            );
            continue;
        }
        if case == "before_open" {
            TEST_FAULT.with(|fault| fault.set(Some("before_open")));
            assert!(scope.begin_file_dacl_transaction(&path, &plan).is_err());
            assert_eq!(
                scope.metadata_manifest(&path)?.acl,
                Some(Acl::WindowsDacl(before))
            );
            continue;
        }
        if case == "success" {
            let alias = root.join("success-alias");
            add_link(&foreign, &path, &alias).map_err(|error| {
                crate::error::with_context(
                    error,
                    "success: vorhandenen ausgewählten Hardlink über Fremdsitzung anlegen",
                )
            })?;
            add_link(&foreign, &path, Path::new("/external/existing")).map_err(|error| {
                crate::error::with_context(
                    error,
                    "success: vorhandenen externen Hardlink über Fremdsitzung anlegen",
                )
            })?;
            plan.alias_scope.selected_paths.push("success-alias".into());
            plan.alias_scope.selected_paths.sort();
            plan.alias_scope.observed_link_count = provider.stat(&path)?.hard_links;
            assert_eq!(plan.alias_scope.observed_link_count, Some(3));
        }
        let mut transaction = scope
            .begin_file_dacl_transaction(&path, &plan)
            .map_err(|error| {
                crate::error::with_context(
                    error,
                    format!("{case}: Produkttransaktion öffnen und Prepared-Intent sichern"),
                )
            })?;
        if case == "success" {
            add_link(&foreign, &path, Path::new("/external/new-while-held")).map_err(|error| {
                crate::error::with_context(
                    error,
                    "success: neuen externen Hardlink bei gehaltenem Writer anlegen",
                )
            })?;
            assert_eq!(std::fs::metadata(physical.join("pane/success"))?.nlink(), 4);
        }
        if case == "after_set" || case == "third_state" {
            TEST_FAULT.with(|fault| fault.set(Some("after_set")));
            let error = transaction.apply().unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            assert_eq!(crate::error::file_dacl_report(&error).uncertain.len(), 1);
        } else {
            let report = transaction.apply().map_err(|error| {
                crate::error::with_context(
                    error,
                    format!("{case}: Produkt-SET und gehaltener/frischer Readback"),
                )
            })?;
            assert_eq!(report.completed.len(), 1);
            assert!(report.uncertain.is_empty());
        }
        if case == "success" {
            transaction.finish().map_err(|error| {
                crate::error::with_context(error, "success: Produkttransaktion abschließen")
            })?;
        } else {
            drop(transaction);
            if case == "third_state" {
                let mut third = after.clone();
                let WindowsDaclState::Acl { aces, .. } = &mut third.state else {
                    panic!()
                };
                let WindowsAce::Allow { mask, .. } = &mut aces[1] else {
                    panic!()
                };
                *mask = 0x89;
                seed(&provider, &path, &third, &context)?;
                let held = crate::local_fs::RootHandle::open_nofollow(config.path())?;
                assert!(crate::file_dacl_registry::acknowledge(
                    &staging,
                    provider.as_ref(),
                    root,
                    match &identity {
                        crate::RootIdentity::Smb(root) => root.server.account_digest,
                        _ => panic!(),
                    },
                    &held
                )
                .is_err());
                continue;
            }
            let held = crate::local_fs::RootHandle::open_nofollow(config.path())?;
            let crate::RootIdentity::Smb(root_identity) = &identity else {
                panic!()
            };
            let ack = crate::file_dacl_registry::acknowledge(
                &staging,
                provider.as_ref(),
                root,
                root_identity.server.account_digest,
                &held,
            )?;
            assert_eq!(ack.report.completed.len(), 1);
        }
        let observed = scope.metadata_manifest(&path)?;
        assert_eq!(observed.acl, Some(Acl::WindowsDacl(after.clone())));
        assert_eq!(observed.windows_security, manifest.windows_security);
        assert_eq!(observed.named_streams, manifest.named_streams);
        assert_eq!(provider.stat(&path)?.inode, Some(file_id));
        assert_eq!(
            std::fs::read(physical.join("pane").join(case))?,
            b"unchanged-file-data"
        );
        if case == "success" {
            for alias in [
                "/pane/success-alias",
                "/external/existing",
                "/external/new-while-held",
            ] {
                let manifest = provider.metadata_manifest(Path::new(alias))?;
                assert_eq!(manifest.acl, Some(Acl::WindowsDacl(after.clone())));
                assert_eq!(provider.stat(Path::new(alias))?.inode, Some(file_id));
            }
            println!(
                "FILE_DACL_OBJECT_WIDE_EXISTING_AND_NEW_EXTERNAL_NAMES_ID_CONTENT_ADS_SIDS_OK"
            );
        }
    }
    println!("FILE_DACL_MINIMAL_RIGHTS_SINGLE_SET_LOST_REPLY_APPLIED_CANCEL_RECOVERY_OK");
    Ok(())
}

#[path = "smb_file_dacl_queue_tests.rs"]
mod queue_tests;
