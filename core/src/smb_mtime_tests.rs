use super::*;
use std::cell::Cell;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::rc::Rc;

struct FakeWire {
    original_held: Rc<Cell<bool>>,
    pressure: bool,
    identity_changed: bool,
    identity_read_denied: bool,
    lose_set_reply: bool,
    lose_close_reply: bool,
    open_error: Option<io::Error>,
    open_access_denied: bool,
    deny_set: bool,
    opens: usize,
    sets: usize,
    closes: usize,
}
impl FakeWire {
    fn new() -> Self {
        Self {
            original_held: Rc::new(Cell::new(true)),
            pressure: false,
            identity_changed: false,
            identity_read_denied: false,
            lose_set_reply: false,
            lose_close_reply: false,
            open_error: None,
            open_access_denied: false,
            deny_set: false,
            opens: 0,
            sets: 0,
            closes: 0,
        }
    }
}
impl MtimeWire for FakeWire {
    type Handle = u64;
    async fn open(&mut self) -> Result<u64, OpenFailure> {
        self.opens += 1;
        if self.open_access_denied {
            return Err(open_failure(smb2::Error::Protocol {
                status: NtStatus::ACCESS_DENIED,
                command: Command::Create,
            }));
        }
        if let Some(error) = self.open_error.take() {
            return Err(OpenFailure {
                error,
                resource: false,
                access_denied: false,
            });
        }
        if self.pressure && self.original_held.get() {
            return Err(OpenFailure {
                error: io::Error::other("Gehaltenes Original belegt letzte Ressource"),
                resource: true,
                access_denied: false,
            });
        }
        Ok(42)
    }
    async fn verify_identity(&mut self, _: u64) -> io::Result<()> {
        if self.identity_read_denied && self.original_held.get() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Zusätzliche ID-Abfrage verweigert",
            ));
        }
        if self.identity_changed {
            Err(crate::real_mode::plan_drift("fremde ID"))
        } else {
            Ok(())
        }
    }
    async fn set(&mut self, _: u64) -> io::Result<()> {
        self.sets += 1;
        if self.deny_set {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SET verweigert",
            ));
        }
        if self.lose_set_reply {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Antwort fehlt nach Wirkung",
            ))
        } else {
            Ok(())
        }
    }
    async fn close(&mut self, _: u64) -> io::Result<()> {
        self.closes += 1;
        if self.lose_close_reply {
            Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "Closeantwort fehlt",
            ))
        } else {
            Ok(())
        }
    }
}
struct Receipt(Rc<Cell<bool>>);
impl Drop for Receipt {
    fn drop(&mut self) {
        self.0.set(false);
    }
}
fn runtime() -> Runtime {
    RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn mtime_resource_open_requires_irrevocable_receipt_release_before_single_normal_set() {
    let mut wire = FakeWire::new();
    wire.pressure = true;
    let mut receipt = Some(Receipt(wire.original_held.clone()));
    let failure = runtime().block_on(set_once(&mut wire)).unwrap_err();
    assert_eq!(failure.phase, MtimePhase::BeforeSet);
    assert!(failure.resource_open && failure.allows_receipt_release());
    assert_eq!((wire.sets, wire.closes), (0, 0));
    // Ohne Freigabe bleibt die reale Ursache des zweiten Open bestehen.
    assert!(runtime().block_on(set_once(&mut wire)).is_err());
    assert_eq!(wire.sets, 0);
    drop(receipt.take());
    assert!(receipt.is_none());
    runtime().block_on(set_once(&mut wire)).unwrap();
    assert_eq!((wire.sets, wire.closes), (1, 1));
    assert!(!wire.original_held.get());
}
#[test]
fn mtime_set_or_close_reply_loss_never_allows_mutation_retry() {
    for after_set in [true, false] {
        let mut wire = FakeWire::new();
        wire.lose_set_reply = after_set;
        wire.lose_close_reply = !after_set;
        let failure = runtime().block_on(set_once(&mut wire)).unwrap_err();
        assert_eq!(
            failure.phase,
            if after_set {
                MtimePhase::SetUnclear
            } else {
                MtimePhase::SetConfirmedReadbackUnclear
            }
        );
        assert!(!failure.allows_receipt_release());
        assert_eq!(failure.close_failed, !after_set);
        assert_eq!((wire.opens, wire.sets, wire.closes), (1, 1, 1));
        assert_eq!(
            failure.into_io().kind(),
            if after_set {
                io::ErrorKind::ConnectionAborted
            } else {
                io::ErrorKind::ConnectionReset
            }
        );
    }
}
#[test]
fn mtime_original_identity_mismatch_is_plan_drift_and_not_an_optional_omission() {
    let mut wire = FakeWire::new();
    wire.identity_changed = true;
    let failure = runtime().block_on(set_once(&mut wire)).unwrap_err();
    assert_eq!(failure.phase, MtimePhase::BeforeSet);
    assert!(!failure.allows_receipt_release());
    assert_eq!(
        crate::error_code(&failure.into_io()),
        crate::ErrorCode::PlanDrift
    );
    assert_eq!((wire.sets, wire.closes), (0, 1));
}
#[test]
fn mtime_resource_classification_keeps_permissions_distinct() {
    assert!(resource_status(NtStatus::INSUFFICIENT_RESOURCES));
    assert!(resource_status(NtStatus::INSUFF_SERVER_RESOURCES));
    assert!(!resource_status(NtStatus::ACCESS_DENIED));
}

#[test]
fn mtime_optional_identity_query_failure_releases_receipt_before_one_normal_set() {
    let mut wire = FakeWire::new();
    wire.identity_read_denied = true;
    let mut receipt = Some(Receipt(wire.original_held.clone()));
    let failure = runtime().block_on(set_once(&mut wire)).unwrap_err();
    assert_eq!(
        failure.before_cause,
        BeforeSetCause::OptionalIdentityObservation
    );
    assert!(failure.allows_receipt_release());
    assert_eq!((wire.sets, wire.closes), (0, 1));
    drop(receipt.take());
    runtime().block_on(set_once(&mut wire)).unwrap();
    assert_eq!(wire.sets, 1);
    assert!(receipt.is_none());
    let mut wire = FakeWire::new();
    wire.identity_read_denied = true;
    wire.lose_close_reply = true;
    let failure = runtime().block_on(set_once(&mut wire)).unwrap_err();
    assert_eq!(failure.before_cause, BeforeSetCause::Other);
    assert!(!failure.allows_receipt_release());
    assert_eq!(wire.sets, 0);
}

struct TestSecrets;
impl SecretResolver for TestSecrets {
    fn resolve(&self, _: &crate::KeychainSecretReference) -> io::Result<crate::SecretString> {
        crate::SecretString::new("Doppeldecker-SMB-E2E-wertlos-2026".into())
    }
}
#[test]
#[ignore = "benötigt frischen privaten Samba; echter SET mit gezielten mtime-Phasenfehlern"]
fn mtime_phases_held_minimal_create_roundtrip() -> io::Result<()> {
    let required = |name| std::env::var(name).expect("Privater mtime-Phasenrunner fehlt");
    let physical = std::path::PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(physical
        .to_string_lossy()
        .starts_with("/tmp/doppeldecker-smb-acl-write-probe-"));
    assert_eq!(physical.file_name().unwrap(), "share");
    assert_eq!(physical.canonicalize()?, physical);
    assert!(std::fs::read_dir(&physical)?.next().is_none());
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    assert_eq!(host, "127.0.0.1");
    let port: u16 = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    assert!(port > 1024);
    let share = required("DOPPELDECKER_SMB_TEST_SHARE");
    let username = required("DOPPELDECKER_SMB_TEST_USER");
    let provider = SmbVfs::connect(
        SmbConnectionConfig {
            server_guid: discover_smb_server(&host, port)?.server_guid,
            host,
            port,
            share,
            username,
            domain: String::new(),
            requested_root: "/".into(),
            password_reference: crate::KeychainSecretReference::new(
                "Doppeldecker-Test",
                "mtime-phases",
            )?,
        },
        Arc::new(TestSecrets),
    )?;
    let (tree, mut connection) = {
        let state = lock(&provider.state);
        assert!(state.session.identity.signing_active && state.session.identity.encryption_active);
        (state.session.tree.clone(), state.session.connection.clone())
    };
    let selected = physical.join("mtime-phase-selected");
    std::fs::create_dir(&selected)?;
    std::fs::set_permissions(&selected, std::fs::Permissions::from_mode(0o777))?;
    let mut original = None;
    let mut owned = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> io::Result<()> {
        for (index, case) in [
            "resource-open",
            "set-reply-lost",
            "readback",
            "identity-mismatch",
        ]
        .iter()
        .enumerate()
        {
            let path = Path::new("/mtime-phase-selected").join(case);
            let physical_path = selected.join(case);
            let remote = provider.remote_path(&path)?;
            let id = provider.runtime.block_on(async {
                let request = CreateRequest {
                    requested_oplock_level: OplockLevel::None,
                    impersonation_level: ImpersonationLevel::Impersonation,
                    desired_access: FileAccessMask::new(0x0010_0080),
                    file_attributes: FILE_ATTRIBUTE_DIRECTORY,
                    share_access: ShareAccess(
                        ShareAccess::FILE_SHARE_READ
                            | ShareAccess::FILE_SHARE_WRITE
                            | ShareAccess::FILE_SHARE_DELETE,
                    ),
                    create_disposition: CreateDisposition::FileCreate,
                    create_options: FILE_DIRECTORY_FILE,
                    name: remote,
                    create_contexts: Vec::new(),
                };
                let frame = connection
                    .execute(Command::Create, &request, Some(tree.tree_id))
                    .await
                    .map_err(smb_error)?;
                require_status(&frame, Command::Create)?;
                Ok::<_, io::Error>(
                    CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
                        .map_err(smb_error)?
                        .file_id,
                )
            })?;
            original = Some(id);
            let metadata = std::fs::symlink_metadata(&physical_path)?;
            owned = Some((physical_path.clone(), metadata.dev(), metadata.ino()));
            let entry = provider.runtime.block_on(query_handle_entry(
                &mut connection,
                &tree,
                id,
                String::new(),
            ))?;
            assert!(entry.file_id != 0 && entry.is_dir && !entry.is_symlink);
            let inode = entry.file_id;
            let wanted = 1_700_000_000 + index as i64;
            let expected = if *case == "identity-mismatch" {
                inode.checked_add(1).unwrap_or(inode - 1)
            } else {
                inode
            };
            SET_COUNT.with(|count| count.set(0));
            FAULT.with(|fault| {
                fault.set(match *case {
                    "resource-open" => Some(Fault::ResourceOpen),
                    "set-reply-lost" => Some(Fault::AfterSet),
                    "readback" => Some(Fault::Readback),
                    _ => None,
                })
            });
            let failure = provider
                .set_mtime_phased(&path, wanted, Some(expected))
                .unwrap_err();
            let phase = failure.phase;
            let fallback = failure.allows_receipt_release();
            let resource = failure.resource_open;
            let error = failure.into_io();
            match *case {
                "resource-open" => {
                    assert_eq!(phase, MtimePhase::BeforeSet);
                    assert!(fallback && resource);
                    SET_COUNT.with(|count| assert_eq!(count.get(), 0));
                    provider
                        .runtime
                        .block_on(tree.close_handle(&mut connection, original.take().unwrap()))
                        .map_err(smb_error)?;
                    assert!(provider
                        .runtime
                        .block_on(query_info(
                            &mut connection,
                            &tree,
                            id,
                            FILE_INTERNAL_INFORMATION,
                            8
                        ))
                        .is_err());
                    assert!(original.is_none());
                    // Der normale alte Pfad läuft erst nach endgültiger Aufgabe.
                    provider.set_mtime_unchecked(&path, wanted)?;
                    SET_COUNT.with(|count| assert_eq!(count.get(), 1));
                    println!(
                        "MTIME_SYNTHETIC_RESOURCE_OPEN_ORIGINAL_RELEASED_ONE_ACTUAL_NORMAL_SET_OK"
                    );
                }
                "set-reply-lost" => {
                    assert_eq!(phase, MtimePhase::SetUnclear);
                    assert!(!fallback);
                    SET_COUNT.with(|count| assert_eq!(count.get(), 1));
                    provider.verify_mtime_readback(&path, wanted, Some(inode))?;
                    SET_COUNT.with(|count| assert_eq!(count.get(), 1));
                    println!("MTIME_ACTUAL_SET_REPLY_LOSS_ZERO_MUTATION_RETRY_OK");
                }
                "readback" => {
                    assert_eq!(phase, MtimePhase::SetConfirmedReadbackUnclear);
                    assert!(!fallback);
                    provider
                        .runtime
                        .block_on(tree.close_handle(&mut connection, original.take().unwrap()))
                        .map_err(smb_error)?;
                    provider.verify_mtime_readback(&path, wanted, Some(inode))?;
                    SET_COUNT.with(|count| assert_eq!(count.get(), 1));
                    assert!(original.is_none());
                    println!("MTIME_ORIGINAL_RELEASED_READ_ONLY_COMPLETION_ONE_SET_OK");
                }
                _ => {
                    assert_eq!(phase, MtimePhase::BeforeSet);
                    assert!(!fallback);
                    assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
                    SET_COUNT.with(|count| assert_eq!(count.get(), 0));
                    assert_eq!(provider.stat_unchecked(&path)?.file_id, inode);
                    println!("MTIME_IDENTITY_MISMATCH_PLAN_DRIFT_ZERO_SET_OK");
                }
            }
            if let Some(handle) = original.take() {
                provider
                    .runtime
                    .block_on(tree.close_handle(&mut connection, handle))
                    .map_err(smb_error)?;
            }
            let current = std::fs::symlink_metadata(&physical_path)?;
            assert!(current.is_dir() && !current.file_type().is_symlink());
            assert_eq!(
                (current.dev(), current.ino()),
                (metadata.dev(), metadata.ino())
            );
            std::fs::remove_dir(&physical_path)?;
            owned = None;
        }
        Ok(())
    }));
    FAULT.with(|fault| fault.set(None));
    let closed = if let Some(handle) = original.take() {
        provider
            .runtime
            .block_on(tree.close_handle(&mut connection, handle))
            .map_err(smb_error)
    } else {
        Ok(())
    };
    let cleanup = closed.and_then(|()| {
        if let Some((path, dev, ino)) = owned {
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || (metadata.dev(), metadata.ino()) != (dev, ino)
            {
                return Err(invalid("mtime-Fixtureidentität änderte sich"));
            }
            std::fs::remove_dir(path)?;
        }
        std::fs::remove_dir(&selected)?;
        assert!(std::fs::read_dir(&physical)?.next().is_none());
        Ok(())
    });
    let result = match result {
        Ok(result) => result,
        Err(payload) => Err(io::Error::other(
            payload
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "mtime-Assertion fehlgeschlagen".into()),
        )),
    };
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(crate::error::with_context(
            error,
            format!("mtime-Fixturecleanup: {cleanup}"),
        )),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup,
    }
}

#[test]
fn mtime_open_interrupted_or_drift_never_releases_receipt_for_another_mutation() {
    for error in [
        io::Error::new(io::ErrorKind::Interrupted, "Open abgebrochen"),
        crate::real_mode::plan_drift("Open beobachtet fremde Identität"),
    ] {
        let mut wire = FakeWire::new();
        wire.open_error = Some(error);
        let failure = runtime().block_on(set_once(&mut wire)).unwrap_err();
        assert_eq!(failure.phase, MtimePhase::BeforeSet);
        assert_eq!(failure.before_cause, BeforeSetCause::Open);
        let receipt = Receipt(wire.original_held.clone());
        if failure.allows_receipt_release() {
            drop(receipt);
            runtime().block_on(set_once(&mut wire)).unwrap();
        }
        assert_eq!((wire.opens, wire.sets, wire.closes), (1, 0, 0));
        assert!(wire.original_held.get());
    }
}

#[test]
fn mtime_only_protocol_create_access_denied_before_set_is_privilege_omission() {
    let mut wire = FakeWire::new();
    wire.open_access_denied = true;
    let failure = runtime().block_on(set_once(&mut wire)).unwrap_err();
    assert!(failure.is_open_access_denied());
    assert_eq!((wire.opens, wire.sets, wire.closes), (1, 0, 0));
    let error = failure.into_ordinary_io(Path::new("/selected/parent"));
    let omission = crate::content_sync::mtime_omission_reason(&error).unwrap();
    assert_eq!(
        omission.kind,
        crate::sync::MetadataIssueKind::PrivilegeFailure
    );
    assert!(omission.allowed_in_profile(crate::MetadataProfile::NativeBestEffort));
    assert!(
        !omission.allowed_in_profile(crate::MetadataProfile::Portable),
        "Portable erhält keine Privilegienausnahme"
    );
    assert!(
        !omission.allowed_in_profile(crate::MetadataProfile::NativeStrict),
        "NativeStrict bleibt streng"
    );
    for error in [
        smb2::Error::Auth {
            message: "Auth".into(),
        },
        smb2::Error::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "keine Statusantwort",
        )),
        smb2::Error::Protocol {
            status: NtStatus::ACCESS_DENIED,
            command: Command::SetInfo,
        },
        smb2::Error::Protocol {
            status: NtStatus::LOGON_FAILURE,
            command: Command::Create,
        },
    ] {
        assert!(!open_failure(error).access_denied);
    }
}

#[test]
fn mtime_ordinary_set_close_readback_and_unproven_open_errors_never_become_omissions() {
    for case in [
        "set-denied",
        "set-reply",
        "close",
        "readback",
        "plain-permission",
        "interrupted",
        "drift",
        "invalid-input",
    ] {
        let mut wire = FakeWire::new();
        match case {
            "set-denied" => wire.deny_set = true,
            "set-reply" => wire.lose_set_reply = true,
            "close" => wire.lose_close_reply = true,
            "plain-permission" => {
                wire.open_error = Some(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "keine Statusantwort",
                ))
            }
            "interrupted" => {
                wire.open_error = Some(io::Error::new(io::ErrorKind::Interrupted, "Abbruch"))
            }
            "drift" => wire.open_error = Some(crate::real_mode::plan_drift("fremdes Objekt")),
            "invalid-input" => {
                wire.open_error = Some(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ungültige Zeit",
                ))
            }
            _ => {}
        }
        let failure = if case == "readback" {
            runtime().block_on(set_once(&mut wire)).unwrap();
            MtimeFailure::confirmed(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Readback verweigert",
            ))
        } else {
            runtime().block_on(set_once(&mut wire)).unwrap_err()
        };
        assert!(!failure.is_open_access_denied(), "{case}");
        let error = failure.into_ordinary_io(Path::new("/selected/parent"));
        assert!(
            crate::content_sync::mtime_omission_reason(&error).is_none(),
            "{case}"
        );
        match case {
            "interrupted" => assert_eq!(crate::error_code(&error), crate::ErrorCode::Interrupted),
            "drift" => assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift),
            "invalid-input" => assert_eq!(error.kind(), io::ErrorKind::InvalidInput),
            _ => assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear),
        }
        assert_eq!(wire.opens, 1, "{case}");
        assert_eq!(
            wire.sets,
            usize::from(["set-denied", "set-reply", "close", "readback"].contains(&case)),
            "{case}"
        );
    }
}

#[test]
fn mtime_open_response_requires_decoded_create_command_before_classifying_status() {
    use smb2::msg::header::Header;
    use smb2::pack::{Pack, WriteCursor};
    for command in [Command::Create, Command::SetInfo, Command::QueryInfo] {
        for status in [
            NtStatus::SUCCESS,
            NtStatus::ACCESS_DENIED,
            NtStatus::INSUFFICIENT_RESOURCES,
        ] {
            let mut header = Header::new_request(command);
            header.status = status;
            let mut cursor = WriteCursor::new();
            header.pack(&mut cursor);
            let bytes = cursor.into_inner();
            let header = Header::unpack(&mut ReadCursor::new(&bytes)).unwrap();
            let frame = smb2::client::connection::Frame {
                header,
                body: Vec::new(),
                raw: bytes.to_vec(),
            };
            let result = check_open_response(&frame);
            if command == Command::Create && status == NtStatus::SUCCESS {
                assert!(result.is_ok());
                continue;
            }
            let failure = result.err().unwrap();
            assert_eq!(
                failure.access_denied,
                command == Command::Create && status == NtStatus::ACCESS_DENIED
            );
            assert_eq!(
                failure.resource,
                command == Command::Create && status == NtStatus::INSUFFICIENT_RESOURCES
            );
            if command != Command::Create {
                assert_eq!(failure.error.kind(), io::ErrorKind::InvalidData);
                let failure = MtimeFailure {
                    phase: MtimePhase::BeforeSet,
                    before_cause: BeforeSetCause::Open,
                    resource_open: failure.resource,
                    open_access_denied: failure.access_denied,
                    close_failed: false,
                    error: failure.error,
                };
                let error = failure.into_ordinary_io(Path::new("/selected/parent"));
                assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
                assert!(crate::content_sync::mtime_omission_reason(&error).is_none());
            }
        }
    }
}

#[test]
fn mtime_readback_retry_never_erases_known_drift_or_mismatch() {
    let drift = MtimeFailure::confirmed(crate::real_mode::plan_drift("fremde ID"));
    assert!(!drift.allows_readback_retry());
    assert_eq!(
        crate::error_code(&drift.into_io()),
        crate::ErrorCode::PlanDrift
    );
    for kind in [
        io::ErrorKind::InvalidInput,
        io::ErrorKind::InvalidData,
        io::ErrorKind::Interrupted,
    ] {
        assert!(
            !MtimeFailure::confirmed(io::Error::new(kind, "bekannte Abweichung"))
                .allows_readback_retry()
        );
    }
    let mut unknown = MtimeFailure::confirmed(io::Error::new(
        io::ErrorKind::ConnectionReset,
        "Antwort fehlt",
    ));
    assert!(unknown.allows_readback_retry());
    unknown.close_failed = true;
    assert!(!unknown.allows_readback_retry());
}
