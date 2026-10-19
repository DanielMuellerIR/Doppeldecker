//! Lokale FTP-/FTPS-Gegenläufe mit künstlichem Bestand und kurzlebigen Zertifikaten.
//!
//! Die Server binden nur Loopback; ihr Bestand liegt in `tempfile` oder im Speicher.
//! Das Passwort ist ein wertloser anonymer Testwert und wird nie serialisiert.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use doppeldecker_core::bidirectional::finish_bidirectional_run_completion;
use doppeldecker_core::{
    discover_ftps_certificate, error_code, execute_prepared_sftp_queue_entry,
    execute_provider_file_queue_entry, prepare_ftps_download, prepare_ftps_queue_entry,
    prepare_provider_bidirectional_with_decisions, BidirectionalQueueEndpoint, BinaryRelativePath,
    Cancel, Compare, ConflictDecision, ConflictResolution, ErrorCode, FileQueueKind,
    FtpSecurityStatus, FtpTransportSecurity, FtpsCertificatePolicy, FtpsConnectionConfig, FtpsVfs,
    KeychainSecretReference, LocalQueueExecution, OperationQueue, PairId, ProviderEndpoint,
    QueueExecutionResult, QueueOperation, QueueState, RenameExecutionClass, RenameOutcome,
    SecretResolver, SecretString, SftpQueueDirection, SftpQueuePreparation, SyncMode, SyncOptions,
    Vfs,
};
use libunftp::{options::FtpsRequired, ServerBuilder};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::PrivatePkcs8KeyDer;
use unftp_sbe_fs::Filesystem;

fn private_area() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let area = tempfile::tempdir().unwrap();
    std::fs::set_permissions(area.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    area
}

struct TestSecrets;

fn owned_vfs(config: FtpsConnectionConfig) -> (FtpsVfs, tempfile::TempDir, OperationQueue) {
    let intent_config = private_area();
    let queue_path = std::fs::canonicalize(intent_config.path())
        .unwrap()
        .join("operations-queue.json");
    let queue = OperationQueue::open(queue_path).unwrap();
    let vfs = FtpsVfs::connect(config, Arc::new(TestSecrets))
        .unwrap()
        .for_queue_preparation(&queue, Path::new("/"))
        .unwrap();
    (vfs, intent_config, queue)
}

fn fault_config(port: u16) -> FtpsConnectionConfig {
    FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: vec![],
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    }
}

fn fault_move_operation(config: FtpsConnectionConfig, items: &[&str]) -> QueueOperation {
    QueueOperation::ProviderFile {
        kind: FileQueueKind::Move,
        source: Box::new(BidirectionalQueueEndpoint::Ftps {
            connection: Box::new(config.clone()),
            root: "/from".into(),
        }),
        target: Box::new(BidirectionalQueueEndpoint::Ftps {
            connection: Box::new(config),
            root: "/to".into(),
        }),
        items: items.iter().map(|item| format!("/from/{item}")).collect(),
        destination: "/to".into(),
        overwrite: false,
        expected_conflicts: vec![],
        source_tree_expected: None,
        expected_conflict_proofs: vec![],
        keep_both_targets: vec![],
        best_effort_commit_expected: None,
        local_copy_new_best_effort_expected: None,
        copy_new_best_effort_expected: Some(false),
        replace_best_effort_expected: Some(false),
        remove_source_best_effort_expected: Some(false),
    }
}

fn execute_fault_move(queue: &OperationQueue) -> std::io::Result<QueueExecutionResult> {
    execute_provider_file_queue_entry(
        queue,
        queue.claim_next()?.unwrap(),
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(TestSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
}

fn fault_cancel_source(control: &FaultControl, mixed: bool) {
    let mut fs = control.fs.lock().unwrap();
    for name in ["from", "to"] {
        fs.get_mut("/").unwrap().insert(name.into(), true);
        fs.insert(format!("/{name}"), Default::default());
    }
    fs.get_mut("/from")
        .unwrap()
        .insert("file.txt".into(), false);
    if mixed {
        fs.get_mut("/from").unwrap().insert("folder".into(), true);
        fs.insert("/from/folder".into(), Default::default());
    }
    control
        .contents
        .lock()
        .unwrap()
        .insert("/from/file.txt".into(), b"source".to_vec());
}

fn fault_merge_tree(control: &FaultControl, new_file: bool) {
    let mut fs = control.fs.lock().unwrap();
    for root in ["from", "to"] {
        fs.get_mut("/").unwrap().insert(root.into(), true);
        fs.insert(format!("/{root}"), [("tree".into(), true)].into());
        fs.insert(
            format!("/{root}/tree"),
            [("conflict.txt".into(), false)].into(),
        );
    }
    fs.get_mut("/from/tree")
        .unwrap()
        .insert("new".into(), !new_file);
    if !new_file {
        fs.insert("/from/tree/new".into(), [("child".into(), true)].into());
        fs.insert("/from/tree/new/child".into(), Default::default());
    }
    control.contents.lock().unwrap().extend([
        (
            "/from/tree/conflict.txt".into(),
            b"source conflict retained".to_vec(),
        ),
        (
            "/to/tree/conflict.txt".into(),
            b"target conflict retained".to_vec(),
        ),
    ]);
    if new_file {
        control
            .contents
            .lock()
            .unwrap()
            .insert("/from/tree/new".into(), b"new content copied".to_vec());
    }
}

#[test]
fn ftp_best_effort_ersetzt_exakten_namen_neben_erhaltenem_aequivalenten_nachbarn() {
    use doppeldecker_core::fileops::{copy_items, FileOperationRequest, OverwritePolicy};
    use doppeldecker_core::LocalVfs;
    use std::sync::atomic::Ordering;

    let mut failed = Vec::new();
    for (name, neighbor) in [("Bericht.txt", "bericht.txt"), ("Ä.txt", "A\u{308}.txt")] {
        let (port, control) = fault_ftp_server(&[name, neighbor]);
        control.enable_stou.store(true, Ordering::SeqCst);
        control.contents.lock().unwrap().extend([
            (format!("/{name}"), b"alter Zielinhalt".to_vec()),
            (format!("/{neighbor}"), b"unveraenderter Nachbar".to_vec()),
        ]);
        let (vfs, _intent_area, _intent_queue) = owned_vfs(fault_config(port));
        let capabilities = vfs.probe_add_only_capabilities_at(Path::new("/")).unwrap();
        assert!(capabilities.best_effort_file_replace);
        let area = private_area();
        let source = area.path().canonicalize().unwrap().join(name);
        std::fs::write(&source, b"neuer Zielinhalt").unwrap();

        let result = copy_items(
            FileOperationRequest {
                src: &LocalVfs::new(),
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: Path::new("/"),
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        );

        let contents = control.contents.lock().unwrap();
        assert_eq!(contents[&format!("/{name}")], b"neuer Zielinhalt");
        assert_eq!(contents[&format!("/{neighbor}")], b"unveraenderter Nachbar");
        assert_eq!(std::fs::read(&source).unwrap(), b"neuer Zielinhalt");
        assert_eq!(control.fs.lock().unwrap()["/"].len(), 2);
        let renames = control
            .mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|command| {
                command
                    .strip_prefix("RNTO ")
                    .is_some_and(|value| fault_path("/", value) == format!("/{name}"))
            })
            .count();
        assert_eq!(renames, 1, "Ersetzen muss genau einmal gesendet werden");
        match result {
            Ok(stats) => assert_eq!(stats.files, 1),
            Err(error) => failed.push((name.to_owned(), error_code(&error), error.to_string())),
        }
    }
    assert!(
        failed.is_empty(),
        "Nachweislich korrekte Ersetzung wurde nicht als Erfolg erkannt: {failed:?}"
    );
}

#[test]
fn ftp_normalisierter_tempname_bleibt_nach_bestaetigtem_ersetzen_unklar() {
    use doppeldecker_core::fileops::{copy_items, FileOperationRequest, OverwritePolicy};
    use doppeldecker_core::LocalVfs;
    use std::sync::atomic::Ordering;
    let (port, control) = fault_ftp_server(&["Bericht.txt"]);
    control.enable_stou.store(true, Ordering::SeqCst);
    control
        .contents
        .lock()
        .unwrap()
        .insert("/Bericht.txt".into(), b"identisch".to_vec());
    let (vfs, _intent_area, _intent_queue) = owned_vfs(fault_config(port));
    assert!(
        vfs.probe_add_only_capabilities_at(Path::new("/"))
            .unwrap()
            .best_effort_file_replace
    );
    *control.fault_destination.lock().unwrap() = Some("/Bericht.txt".into());
    *control.fault.lock().unwrap() = Some(RntoFault::NormalizeTempAndAcknowledge);
    let start = control.mutations.lock().unwrap().len();
    let area = private_area();
    let source = area.path().canonicalize().unwrap().join("Bericht.txt");
    std::fs::write(&source, b"identisch").unwrap();

    let result = copy_items(
        FileOperationRequest {
            src: &LocalVfs::new(),
            dst: &vfs,
            items: std::slice::from_ref(&source),
            dst_dir: Path::new("/"),
            policy: OverwritePolicy::Overwrite,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut |_| {},
    );

    let names: Vec<_> = control.fs.lock().unwrap()["/"].keys().cloned().collect();
    let temp = names
        .iter()
        .find(|name| name.as_str() != "Bericht.txt")
        .unwrap();
    assert_eq!(temp, &temp.to_uppercase());
    assert_eq!(names.len(), 2);
    assert_eq!(
        control.contents.lock().unwrap()[&format!("/{temp}")],
        b"identisch"
    );
    assert_eq!(
        control.contents.lock().unwrap()["/Bericht.txt"],
        b"identisch"
    );
    let mutations = control.mutations.lock().unwrap()[start..].to_vec();
    assert_eq!(
        mutations
            .iter()
            .filter(|command| command
                .strip_prefix("RNTO ")
                .is_some_and(|value| fault_path("/", value) == "/Bericht.txt"))
            .count(),
        1
    );
    assert!(!mutations.iter().any(|command| command.starts_with("DELE ")));
    assert!(
        matches!(&result, Err(error) if error_code(error)==ErrorCode::OutcomeUnclear),
        "Normalisierter Tempbestand wurde fälschlich als erfolgreicher Commit gewertet: {result:?}"
    );
    let paths = doppeldecker_core::error::recovery_paths(&result.unwrap_err());
    assert!(paths.contains(&format!("/{temp}")));
    assert!(paths.contains(&"/Bericht.txt".into()));
}

#[test]
fn ftp_veraenderter_aequivalenter_nachbar_bleibt_trotz_gleicher_metadaten_unklar() {
    use doppeldecker_core::fileops::{copy_items, FileOperationRequest, OverwritePolicy};
    use doppeldecker_core::LocalVfs;
    use std::sync::atomic::Ordering;
    for existing in [true, false] {
        let initial: &[&str] = if existing {
            &["Bericht.txt", "bericht.txt"]
        } else {
            &["Bericht.txt"]
        };
        let (port, control) = fault_ftp_server(initial);
        control.enable_stou.store(true, Ordering::SeqCst);
        control.contents.lock().unwrap().extend([
            ("/Bericht.txt".into(), b"alter Zielinhalt".to_vec()),
            ("/bericht.txt".into(), vec![b'A'; 16]),
        ]);
        if !existing {
            control.contents.lock().unwrap().remove("/bericht.txt");
        }
        let (vfs, _intent_area, _intent_queue) = owned_vfs(fault_config(port));
        assert!(
            vfs.probe_add_only_capabilities_at(Path::new("/"))
                .unwrap()
                .best_effort_file_replace
        );
        let weak = Arc::downgrade(&control);
        *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
            if command
                .strip_prefix("RNTO ")
                .is_some_and(|value| fault_path("/", value) == "/Bericht.txt")
            {
                let control = weak.upgrade().unwrap();
                control
                    .fs
                    .lock()
                    .unwrap()
                    .get_mut("/")
                    .unwrap()
                    .insert("bericht.txt".into(), false);
                control
                    .contents
                    .lock()
                    .unwrap()
                    .insert("/bericht.txt".into(), vec![b'B'; 16]);
            }
        }));
        let area = private_area();
        let source = area.path().canonicalize().unwrap().join("Bericht.txt");
        std::fs::write(&source, b"neuer Zielinhalt").unwrap();
        let result = copy_items(
            FileOperationRequest {
                src: &LocalVfs::new(),
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: Path::new("/"),
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        );
        assert_eq!(
            control.contents.lock().unwrap()["/Bericht.txt"],
            b"neuer Zielinhalt"
        );
        assert_eq!(
            control.contents.lock().unwrap()["/bericht.txt"],
            vec![b'B'; 16]
        );
        assert!(matches!(result, Err(error) if error_code(&error)==ErrorCode::OutcomeUnclear));
    }
}

#[test]
fn ftp_namenspruefung_beachtet_abbruch_vor_der_zielmutation() {
    use doppeldecker_core::fileops::{copy_items, FileOperationRequest, OverwritePolicy};
    use doppeldecker_core::LocalVfs;
    use std::sync::atomic::Ordering;
    let (port, control) = fault_ftp_server(&["Bericht.txt", "bericht.txt"]);
    control.enable_stou.store(true, Ordering::SeqCst);
    control.contents.lock().unwrap().extend([
        ("/Bericht.txt".into(), b"alter Zielinhalt".to_vec()),
        ("/bericht.txt".into(), b"erhaltener Nachbar".to_vec()),
    ]);
    let (vfs, _intent_area, _intent_queue) = owned_vfs(fault_config(port));
    assert!(
        vfs.probe_add_only_capabilities_at(Path::new("/"))
            .unwrap()
            .best_effort_file_replace
    );
    let cancel = Cancel::new();
    let flag = cancel.clone();
    *control.retr_observer.lock().unwrap() = Some(Arc::new(move |path| {
        if path == "/bericht.txt" {
            flag.cancel();
        }
    }));
    let start = control.mutations.lock().unwrap().len();
    let area = private_area();
    let source = area.path().canonicalize().unwrap().join("Bericht.txt");
    std::fs::write(&source, b"neuer Zielinhalt").unwrap();

    let result = copy_items(
        FileOperationRequest {
            src: &LocalVfs::new(),
            dst: &vfs,
            items: std::slice::from_ref(&source),
            dst_dir: Path::new("/"),
            policy: OverwritePolicy::Overwrite,
            sandbox: &None,
        },
        &cancel,
        &mut |_| {},
    );

    assert!(cancel.is_cancelled());
    let paths = match &result {
        Ok(stats) => {
            assert!(stats.cancelled && stats.files == 0);
            stats.temp_recovery_paths.clone()
        }
        Err(error) => {
            assert!(
                matches!(
                    error_code(error),
                    ErrorCode::Interrupted | ErrorCode::OutcomeUnclear
                ),
                "{error}"
            );
            doppeldecker_core::error::recovery_paths(error)
        }
    };
    assert_eq!(
        control.contents.lock().unwrap()["/Bericht.txt"],
        b"alter Zielinhalt"
    );
    assert_eq!(
        control.contents.lock().unwrap()["/bericht.txt"],
        b"erhaltener Nachbar"
    );
    assert_eq!(std::fs::read(source).unwrap(), b"neuer Zielinhalt");
    assert!(!control.mutations.lock().unwrap()[start..]
        .iter()
        .any(|command| {
            command
                .strip_prefix("RNTO ")
                .is_some_and(|value| fault_path("/", value) == "/Bericht.txt")
        }));
    for name in control.fs.lock().unwrap()["/"]
        .keys()
        .filter(|name| !["Bericht.txt", "bericht.txt"].contains(&name.as_str()))
    {
        assert!(
            paths.contains(&format!("/{name}")),
            "unbilanzierter Tempbestand: {name}: {result:?}"
        );
    }
}

#[test]
fn ftp_ersetzungsreadback_neben_nachbarn_klaert_antwortverlust_ohne_retry() {
    use doppeldecker_core::fileops::{copy_items, FileOperationRequest, OverwritePolicy};
    use doppeldecker_core::LocalVfs;
    use std::sync::atomic::Ordering;
    for fault in [RntoFault::DropBeforeMutation, RntoFault::DropAfterMutation] {
        let (port, control) = fault_ftp_server(&["Bericht.txt", "bericht.txt"]);
        control.enable_stou.store(true, Ordering::SeqCst);
        control.contents.lock().unwrap().extend([
            ("/Bericht.txt".into(), b"alter Zielinhalt".to_vec()),
            ("/bericht.txt".into(), b"erhaltener Nachbar".to_vec()),
        ]);
        let (vfs, _intent_area, _intent_queue) = owned_vfs(fault_config(port));
        assert!(
            vfs.probe_add_only_capabilities_at(Path::new("/"))
                .unwrap()
                .best_effort_file_replace
        );
        *control.fault_destination.lock().unwrap() = Some("/Bericht.txt".into());
        *control.fault.lock().unwrap() = Some(fault);
        let start = control.mutations.lock().unwrap().len();
        let area = private_area();
        let source = area.path().canonicalize().unwrap().join("Bericht.txt");
        std::fs::write(&source, b"neuer Zielinhalt").unwrap();
        let result = copy_items(
            FileOperationRequest {
                src: &LocalVfs::new(),
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: Path::new("/"),
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        );
        assert_eq!(
            control.contents.lock().unwrap()["/bericht.txt"],
            b"erhaltener Nachbar"
        );
        let error = result.unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        let paths = doppeldecker_core::error::recovery_paths(&error);
        assert!(paths.contains(&"/Bericht.txt".into()));
        assert!(paths
            .iter()
            .any(|path| path.ends_with(".recovery.json") && Path::new(path).is_file()));
        if fault == RntoFault::DropAfterMutation {
            assert_eq!(
                control.contents.lock().unwrap()["/Bericht.txt"],
                b"neuer Zielinhalt"
            );
        } else {
            assert_eq!(
                control.contents.lock().unwrap()["/Bericht.txt"],
                b"alter Zielinhalt"
            );
        }
        for name in control.fs.lock().unwrap()["/"]
            .keys()
            .filter(|name| !["Bericht.txt", "bericht.txt"].contains(&name.as_str()))
        {
            assert!(
                paths.contains(&format!("/{name}")),
                "unbilanzierter Tempbestand: {name}"
            );
        }
        assert_eq!(
            control.mutations.lock().unwrap()[start..]
                .iter()
                .filter(|command| {
                    command
                        .strip_prefix("RNTO ")
                        .is_some_and(|value| fault_path("/", value) == "/Bericht.txt")
                })
                .count(),
            1,
            "Kein erneutes RNTO nach Antwortverlust"
        );
        assert_eq!(std::fs::read(source).unwrap(), b"neuer Zielinhalt");
    }
}

#[test]
fn ftps_passive_konfliktmetadaten_benoetigen_keinen_retr_inhaltsbeleg() {
    use std::sync::atomic::Ordering;
    let (port, control) = fault_ftp_server(&["keep.txt"]);
    fault_merge_tree(&control, false);
    control.deny_retr.store(true, Ordering::SeqCst);
    let (vfs, _area, _queue) = owned_vfs(fault_config(port));
    let items = [PathBuf::from("/from/tree")];
    let conflicts = vec!["/to/tree/conflict.txt".to_owned()];
    let views = doppeldecker_core::fileops::file_conflict_metadata_views(
        &vfs,
        &vfs,
        Path::new("/to"),
        &items,
        &conflicts,
    )
    .unwrap();
    assert_eq!(views.len(), 1);
    assert!(
        views[0].source_proof.content_hash.is_none()
            && views[0].target_proof.content_hash.is_none()
    );
    for proof in [&views[0].source_proof, &views[0].target_proof] {
        doppeldecker_core::fileops::verify_file_metadata_proofs(&vfs, std::slice::from_ref(proof))
            .unwrap();
    }
    assert!(control.retr_paths.lock().unwrap().is_empty());
    assert!(doppeldecker_core::fileops::file_conflict_views(
        &vfs,
        &vfs,
        Path::new("/to"),
        &items,
        &conflicts
    )
    .is_err());
    assert_eq!(
        *control.retr_paths.lock().unwrap(),
        ["/to/tree/conflict.txt"]
    );
    assert!(control.mutations.lock().unwrap().is_empty());
    control.deny_retr.store(false, Ordering::SeqCst);
    let full = doppeldecker_core::fileops::file_conflict_views(
        &vfs,
        &vfs,
        Path::new("/to"),
        &items,
        &conflicts,
    )
    .unwrap();
    assert!(
        full[0].source_proof.content_hash.is_some() && full[0].target_proof.content_hash.is_some()
    );
    assert!(doppeldecker_core::fileops::verify_file_metadata_proofs(
        &vfs,
        std::slice::from_ref(&full[0].target_proof)
    )
    .is_err());
    control
        .fs
        .lock()
        .unwrap()
        .get_mut("/to/tree")
        .unwrap()
        .remove("conflict.txt");
    let error = doppeldecker_core::fileops::verify_file_metadata_proofs(
        &vfs,
        std::slice::from_ref(&views[0].target_proof),
    )
    .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::PlanDrift);
}

#[test]
fn ftps_merge_skip_bereitet_nur_neue_nachfahren_ohne_quellentfernung_vor() {
    use std::sync::atomic::Ordering;
    for kind in [FileQueueKind::Copy, FileQueueKind::Move] {
        for new_file in [false, true] {
            for content_supported in [false, true] {
                if !new_file && content_supported {
                    continue;
                }
                let (port, control) = fault_ftp_server(&["keep.txt"]);
                fault_merge_tree(&control, new_file);
                control
                    .enable_stou
                    .store(content_supported, Ordering::SeqCst);
                let before_contents = control.contents.lock().unwrap().clone();
                let before_fs = control.fs.lock().unwrap().clone();
                let area = private_area();
                let queue = OperationQueue::open(area.path().join("queue.json")).unwrap();
                let mut operation = fault_move_operation(fault_config(port), &["tree"]);
                if let QueueOperation::ProviderFile {
                    kind: selected,
                    expected_conflicts,
                    copy_new_best_effort_expected,
                    remove_source_best_effort_expected,
                    ..
                } = &mut operation
                {
                    *selected = kind;
                    *expected_conflicts = vec!["/to/tree/conflict.txt".into()];
                    *copy_new_best_effort_expected = Some(true);
                    *remove_source_best_effort_expected = None;
                }
                queue.enqueue(operation).unwrap();
                let result = execute_fault_move(&queue);
                let commands = control.mutations.lock().unwrap().clone();
                assert!(
                    !commands.iter().any(|cmd| cmd.contains("/from/")),
                    "{kind:?}/{new_file}/{content_supported}: {commands:?}"
                );
                let after_fs = control.fs.lock().unwrap().clone();
                let after_contents = control.contents.lock().unwrap().clone();
                assert_eq!(after_fs["/from"], before_fs["/from"]);
                assert_eq!(after_fs["/from/tree"], before_fs["/from/tree"]);
                for (path, content) in &before_contents {
                    assert_eq!(after_contents.get(path), Some(content), "{path}");
                }
                if new_file && !content_supported {
                    assert!(result.is_err(), "{result:?}");
                    assert_eq!(after_fs, before_fs);
                    assert_eq!(after_contents, before_contents);
                    assert!(commands.iter().any(|cmd| cmd == "STOU"));
                } else {
                    assert!(
                        matches!(&result, Ok(QueueExecutionResult::File(stats)) if stats.skipped == 1 && stats.files == u64::from(new_file) && !stats.source_removal_best_effort),
                        "{kind:?}/{new_file}/{content_supported}: {result:?}"
                    );
                    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
                    if new_file {
                        assert_eq!(after_contents["/to/tree/new"], b"new content copied");
                        assert!(commands.iter().any(|cmd| cmd == "STOU"));
                    } else {
                        assert!(after_fs.contains_key("/to/tree/new/child"));
                        assert!(!commands
                            .iter()
                            .any(|cmd| cmd == "STOU" || cmd.starts_with("DELE ")));
                    }
                }
                assert!(!std::fs::read_dir(area.path()).unwrap().any(|entry| entry
                    .unwrap()
                    .path()
                    .to_string_lossy()
                    .ends_with(".recovery.json")));
            }
        }
    }
}

#[test]
fn ftps_merge_skip_entfernt_nur_unabhaengige_vollstaendig_kopierte_quellen() {
    use std::sync::atomic::Ordering;
    let (port, control) = fault_ftp_server(&["keep.txt"]);
    fault_merge_tree(&control, true);
    control.enable_stou.store(true, Ordering::SeqCst);
    control
        .fs
        .lock()
        .unwrap()
        .get_mut("/from")
        .unwrap()
        .insert("complete.txt".into(), false);
    control.contents.lock().unwrap().insert(
        "/from/complete.txt".into(),
        b"independent complete source".to_vec(),
    );
    let area = private_area();
    let queue = OperationQueue::open(area.path().join("queue.json")).unwrap();
    let mut operation = fault_move_operation(fault_config(port), &["tree", "complete.txt"]);
    if let QueueOperation::ProviderFile {
        expected_conflicts,
        copy_new_best_effort_expected,
        remove_source_best_effort_expected,
        ..
    } = &mut operation
    {
        *expected_conflicts = vec!["/to/tree/conflict.txt".into()];
        *copy_new_best_effort_expected = Some(true);
        *remove_source_best_effort_expected = Some(true);
    }
    queue.enqueue(operation).unwrap();
    let result = execute_fault_move(&queue).unwrap();
    assert!(
        matches!(result, QueueExecutionResult::File(stats) if stats.files == 2 && stats.skipped == 1 && stats.source_removal_best_effort)
    );
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    let fs = control.fs.lock().unwrap();
    assert!(!fs["/from"].contains_key("complete.txt"));
    assert!(fs["/from/tree"].contains_key("new"));
    let contents = control.contents.lock().unwrap();
    assert_eq!(contents["/to/complete.txt"], b"independent complete source");
    assert_eq!(
        contents["/from/tree/conflict.txt"],
        b"source conflict retained"
    );
    assert_eq!(
        contents["/to/tree/conflict.txt"],
        b"target conflict retained"
    );
    assert_eq!(contents["/from/tree/new"], contents["/to/tree/new"]);
    let commands = control.mutations.lock().unwrap();
    assert!(commands.iter().any(|cmd| cmd == "DELE /from/complete.txt"));
    assert!(!commands
        .iter()
        .any(|cmd| cmd.starts_with("DELE /from/tree/") || cmd.starts_with("RMD /from/tree")));
}

#[test]
fn ftps_merge_skip_haelt_verschwundene_konflikte_nach_der_probe_gebunden() {
    use std::sync::atomic::Ordering;
    for kind in [FileQueueKind::Copy, FileQueueKind::Move] {
        let (port, control) = fault_ftp_server(&["keep.txt"]);
        fault_merge_tree(&control, true);
        control.enable_stou.store(true, Ordering::SeqCst);
        let weak = Arc::downgrade(&control);
        *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
            if command.starts_with("MKD /to/.doppeldecker-probe-") {
                let state = weak.upgrade().unwrap();
                state
                    .fs
                    .lock()
                    .unwrap()
                    .get_mut("/to/tree")
                    .unwrap()
                    .remove("conflict.txt");
                state
                    .contents
                    .lock()
                    .unwrap()
                    .remove("/to/tree/conflict.txt");
            }
        }));
        let area = private_area();
        let queue = OperationQueue::open(area.path().join("queue.json")).unwrap();
        let mut operation = fault_move_operation(fault_config(port), &["tree"]);
        if let QueueOperation::ProviderFile {
            kind: selected,
            expected_conflicts,
            copy_new_best_effort_expected,
            remove_source_best_effort_expected,
            ..
        } = &mut operation
        {
            *selected = kind;
            *expected_conflicts = vec!["/to/tree/conflict.txt".into()];
            *copy_new_best_effort_expected = Some(true);
            *remove_source_best_effort_expected = None;
        }
        queue.enqueue(operation).unwrap();
        let error = execute_fault_move(&queue).unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::PlanDrift, "{error}");
        let contents = control.contents.lock().unwrap();
        assert!(!contents.contains_key("/to/tree/conflict.txt"));
        assert_eq!(
            contents["/from/tree/conflict.txt"],
            b"source conflict retained"
        );
        assert_eq!(contents["/from/tree/new"], b"new content copied");
        assert!(!control
            .mutations
            .lock()
            .unwrap()
            .iter()
            .any(|cmd| cmd.contains("/from/") || cmd.ends_with("/to/tree/conflict.txt")));
    }
}

#[test]
fn ftps_vollstaendig_uebersprungener_merge_benoetigt_keine_inhaltsprobe() {
    let mut failures = Vec::new();
    for kind in [FileQueueKind::Copy, FileQueueKind::Move] {
        for nested in [false, true] {
            let (port, control) = fault_ftp_server(&["keep.txt"]);
            {
                let mut fs = control.fs.lock().unwrap();
                for root in ["from", "to"] {
                    fs.get_mut("/").unwrap().insert(root.into(), true);
                    fs.insert(format!("/{root}"), [("tree".into(), true)].into());
                    fs.insert(format!("/{root}/tree"), [("a.txt".into(), false)].into());
                    if nested {
                        fs.get_mut(&format!("/{root}/tree"))
                            .unwrap()
                            .insert("sub".into(), true);
                        fs.insert(
                            format!("/{root}/tree/sub"),
                            [("b.txt".into(), false)].into(),
                        );
                    } else {
                        fs.get_mut(&format!("/{root}/tree"))
                            .unwrap()
                            .insert("b.txt".into(), false);
                    }
                }
            }
            let relative = if nested {
                "tree/sub/b.txt"
            } else {
                "tree/b.txt"
            };
            control.contents.lock().unwrap().extend([
                ("/from/tree/a.txt".into(), b"source a retained".to_vec()),
                ("/to/tree/a.txt".into(), b"target a retained".to_vec()),
                (format!("/from/{relative}"), b"source b retained".to_vec()),
                (format!("/to/{relative}"), b"target b retained".to_vec()),
            ]);
            let before_fs = control.fs.lock().unwrap().clone();
            let before_contents = control.contents.lock().unwrap().clone();
            let area = private_area();
            let root = std::fs::canonicalize(area.path()).unwrap();
            let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
            let mut operation = fault_move_operation(fault_config(port), &["tree"]);
            if let QueueOperation::ProviderFile {
                kind: operation_kind,
                expected_conflicts,
                copy_new_best_effort_expected,
                remove_source_best_effort_expected,
                ..
            } = &mut operation
            {
                *operation_kind = kind;
                *expected_conflicts = vec!["/to/tree/a.txt".into(), format!("/to/{relative}")];
                *copy_new_best_effort_expected = None;
                *remove_source_best_effort_expected = None;
            }
            queue.enqueue(operation).unwrap();
            let result = execute_fault_move(&queue);
            let commands = control.mutations.lock().unwrap().clone();
            assert_eq!(*control.fs.lock().unwrap(), before_fs);
            assert_eq!(*control.contents.lock().unwrap(), before_contents);
            if !matches!(&result, Ok(QueueExecutionResult::File(stats)) if stats.skipped==2 && stats.files==0 && stats.bytes==0 && !stats.cancelled)
                || !commands.is_empty()
                || queue.snapshot().entries[0].state != QueueState::Completed
            {
                failures.push(format!(
                    "{kind:?}, nested={nested}: {result:?}; commands={commands:?}; state={:?}",
                    queue.snapshot().entries[0].state
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "Vollständig übersprungener Merge benötigt keinen Dateiinhalt:\n{}",
        failures.join("\n")
    );
}

#[test]
fn ftps_vollstaendig_uebersprungener_move_bleibt_ohne_schreibproben() {
    use std::sync::atomic::Ordering;
    let mut failures = Vec::new();
    for kind in [FileQueueKind::Copy, FileQueueKind::Move] {
        for directory in [false, true] {
            for reject_probe in [false, true] {
                let (port, control) = fault_ftp_server(&["keep.txt"]);
                {
                    let mut fs = control.fs.lock().unwrap();
                    for name in ["from", "to"] {
                        fs.get_mut("/").unwrap().insert(name.into(), true);
                        fs.insert(format!("/{name}"), Default::default());
                    }
                    fs.get_mut("/from")
                        .unwrap()
                        .insert("empty".into(), directory);
                    if directory {
                        fs.insert("/from/empty".into(), [("inside.txt".into(), false)].into());
                    }
                    fs.get_mut("/to").unwrap().insert("empty".into(), false);
                }
                control
                    .contents
                    .lock()
                    .unwrap()
                    .insert("/to/empty".into(), b"foreign target".to_vec());
                control.contents.lock().unwrap().insert(
                    if directory {
                        "/from/empty/inside.txt"
                    } else {
                        "/from/empty"
                    }
                    .into(),
                    b"source retained".to_vec(),
                );
                let before_fs = control.fs.lock().unwrap().clone();
                let before_contents = control.contents.lock().unwrap().clone();
                control
                    .reject_probe_mkdir
                    .store(reject_probe, Ordering::SeqCst);
                let area = private_area();
                let root = std::fs::canonicalize(area.path()).unwrap();
                let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
                let mut operation = fault_move_operation(fault_config(port), &["empty"]);
                if let QueueOperation::ProviderFile {
                    kind: operation_kind,
                    expected_conflicts,
                    copy_new_best_effort_expected,
                    remove_source_best_effort_expected,
                    ..
                } = &mut operation
                {
                    *operation_kind = kind;
                    *expected_conflicts = vec!["/to/empty".into()];
                    // Ohne Nutzerwirkung erteilen auch alte/fehlende Klassen keine Schreibfreigabe.
                    *copy_new_best_effort_expected = None;
                    *remove_source_best_effort_expected = None;
                }
                queue.enqueue(operation).unwrap();
                let result = execute_fault_move(&queue);
                let commands = control.mutations.lock().unwrap().clone();
                assert_eq!(*control.fs.lock().unwrap(), before_fs);
                assert_eq!(*control.contents.lock().unwrap(), before_contents);
                if !matches!(&result, Ok(QueueExecutionResult::File(stats)) if stats.skipped == 1 && stats.files == 0 && !stats.cancelled)
                    || !commands.is_empty()
                    || queue.snapshot().entries[0].state != QueueState::Completed
                {
                    failures.push(format!("{kind:?}, directory={directory}, reject_probe={reject_probe}: {result:?}; commands={commands:?}; state={:?}", queue.snapshot().entries[0].state));
                }
                let before = serde_json::to_value(queue.snapshot()).unwrap();
                drop(queue);
                let reopened = OperationQueue::open(root.join("config/queue.json")).unwrap();
                assert_eq!(serde_json::to_value(reopened.snapshot()).unwrap(), before);
                assert_eq!(*control.mutations.lock().unwrap(), commands);
            }
        }
    }
    assert!(
        failures.is_empty(),
        "Bestätigtes Skip benötigt keine Probe oder Quellenentfernung:\n{}",
        failures.join("\n")
    );
}

#[test]
fn ftps_skip_blendet_dateikonflikt_aus_der_ordnerwirkung_aus() {
    for kind in [FileQueueKind::Copy, FileQueueKind::Move] {
        let (port, control) = fault_ftp_server(&["keep.txt"]);
        {
            let mut fs = control.fs.lock().unwrap();
            for name in ["from", "to"] {
                fs.get_mut("/").unwrap().insert(name.into(), true);
                fs.insert(format!("/{name}"), Default::default());
            }
            fs.get_mut("/from")
                .unwrap()
                .extend([("conflict.txt".into(), false), ("empty".into(), true)]);
            fs.insert("/from/empty".into(), [("child".into(), true)].into());
            fs.insert("/from/empty/child".into(), Default::default());
            fs.get_mut("/to")
                .unwrap()
                .insert("conflict.txt".into(), false);
        }
        control.contents.lock().unwrap().extend([
            ("/from/conflict.txt".into(), b"source retained".to_vec()),
            ("/to/conflict.txt".into(), b"target retained".to_vec()),
        ]);
        let area = private_area();
        let root = std::fs::canonicalize(area.path()).unwrap();
        let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
        let mut operation = fault_move_operation(fault_config(port), &["conflict.txt", "empty"]);
        if let QueueOperation::ProviderFile {
            kind: operation_kind,
            expected_conflicts,
            copy_new_best_effort_expected,
            remove_source_best_effort_expected,
            ..
        } = &mut operation
        {
            *operation_kind = kind;
            *expected_conflicts = vec!["/to/conflict.txt".into()];
            *copy_new_best_effort_expected = Some(true);
            *remove_source_best_effort_expected = (kind == FileQueueKind::Move).then_some(true);
        }
        queue.enqueue(operation).unwrap();
        let result = execute_fault_move(&queue).unwrap();
        assert!(
            matches!(result, QueueExecutionResult::File(stats) if stats.skipped == 1 && stats.files == 0 && !stats.cancelled)
        );
        let fs = control.fs.lock().unwrap();
        assert!(fs.contains_key("/to/empty/child"));
        assert_eq!(fs.contains_key("/from/empty"), kind == FileQueueKind::Copy);
        assert!(
            fs["/from"].contains_key("conflict.txt")
                && fs["/to"].contains_key("conflict.txt")
                && fs["/"].contains_key("keep.txt")
        );
        assert!(!fs.keys().any(|name| name.contains(".doppeldecker-")));
        let contents = control.contents.lock().unwrap();
        assert_eq!(contents["/from/conflict.txt"], b"source retained");
        assert_eq!(contents["/to/conflict.txt"], b"target retained");
        let commands = control.mutations.lock().unwrap();
        assert!(
            !commands
                .iter()
                .any(|cmd| cmd == "STOU" || cmd.starts_with("DELE ")),
            "{commands:?}"
        );
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    }
}

#[test]
fn ftps_skip_konfliktdrift_bleibt_vor_jeder_probe_passiv() {
    for condition in ["new-conflict", "missing-conflict", "stat-error"] {
        let (port, control) = fault_ftp_server(&["keep.txt"]);
        {
            let mut fs = control.fs.lock().unwrap();
            for name in ["from", "to"] {
                fs.get_mut("/").unwrap().insert(name.into(), true);
                fs.insert(format!("/{name}"), Default::default());
            }
            fs.get_mut("/from")
                .unwrap()
                .insert("file.txt".into(), false);
            if condition != "missing-conflict" {
                fs.get_mut("/to").unwrap().insert("file.txt".into(), false);
            }
            if condition == "stat-error" {
                *control.reject_listing_path.lock().unwrap() = Some("/to".into());
            }
        }
        let before_fs = control.fs.lock().unwrap().clone();
        let area = private_area();
        let root = std::fs::canonicalize(area.path()).unwrap();
        let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
        let mut operation = fault_move_operation(fault_config(port), &["file.txt"]);
        if let QueueOperation::ProviderFile {
            expected_conflicts, ..
        } = &mut operation
        {
            if condition != "new-conflict" {
                *expected_conflicts = vec!["/to/file.txt".into()];
            }
        }
        queue.enqueue(operation).unwrap();
        let result = execute_fault_move(&queue);
        if condition == "stat-error" {
            assert!(result.is_err());
        } else {
            assert!(matches!(
                result,
                Ok(QueueExecutionResult::AwaitingConfirmation { .. })
            ));
        }
        assert!(control.mutations.lock().unwrap().is_empty());
        assert_eq!(*control.fs.lock().unwrap(), before_fs);
    }
}

#[test]
fn ftps_konfliktvorschau_erteilt_vor_der_wahl_keine_mutationsfaehigkeit() {
    use std::sync::atomic::Ordering;
    let (port, control) = fault_ftp_server(&["keep.txt"]);
    {
        let mut fs = control.fs.lock().unwrap();
        fs.get_mut("/").unwrap().insert("to".into(), true);
        fs.insert("/to".into(), [("file.txt".into(), false)].into());
    }
    control.reject_probe_mkdir.store(true, Ordering::SeqCst);
    let (vfs, area, queue) = owned_vfs(fault_config(port));
    let local = std::fs::canonicalize(area.path()).unwrap().join("source");
    std::fs::create_dir(&local).unwrap();
    std::fs::write(local.join("file.txt"), b"source retained").unwrap();
    let source = ProviderEndpoint::local(&local).unwrap();
    let target = ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/to")).unwrap();
    let items = [local.join("file.txt")];
    for (inspect, skip) in [(true, false), (false, true)] {
        let result = source
            .probe_interactive_file_target(doppeldecker_core::ProviderFileTargetProbe {
                target: &target,
                items: &items,
                kind: FileQueueKind::Copy,
                overwrite: false,
                skip_conflicts: skip,
                inspect_conflicts: inspect,
                expected_conflicts: None,
                shared_session: false,
                targets_missing: false,
                capabilities_override: None,
            })
            .unwrap();
        assert_eq!(result.choice_required, inspect);
        assert_eq!(result.skip_only, skip);
        assert!(!result.rename_only);
        let caps = result.capabilities.unwrap();
        assert!(
            !caps.copy_new
                && !caps.copy_new_best_effort
                && !caps.replace
                && !caps.replace_best_effort
                && !caps.remove_source
                && !caps.remove_source_best_effort
                && !caps.rename_noreplace
        );
        assert!(control.mutations.lock().unwrap().is_empty());
    }
    let fresh = queue.bind_provider_preparation(&target).unwrap();
    let replacement = source
        .probe_interactive_file_target(doppeldecker_core::ProviderFileTargetProbe {
            target: &fresh,
            items: &items,
            kind: FileQueueKind::Copy,
            overwrite: true,
            skip_conflicts: false,
            inspect_conflicts: false,
            expected_conflicts: None,
            shared_session: false,
            targets_missing: false,
            capabilities_override: None,
        })
        .unwrap();
    assert!(!replacement.choice_required && !replacement.skip_only);
    let capabilities = replacement.capabilities.unwrap();
    assert!(!capabilities.copy_new && !capabilities.copy_new_best_effort);
    let commands = control.mutations.lock().unwrap();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].starts_with("MKD /to/"));
    assert!(control.fs.lock().unwrap()["/to"].contains_key("file.txt"));
    assert_eq!(
        std::fs::read(local.join("file.txt")).unwrap(),
        b"source retained"
    );
}

#[test]
fn ftps_spaet_verschwundener_skip_konflikt_startet_keinen_kopierversuch() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (port, control) = fault_ftp_server(&["keep.txt"]);
    {
        let mut fs = control.fs.lock().unwrap();
        for name in ["from", "to"] {
            fs.get_mut("/").unwrap().insert(name.into(), true);
            fs.insert(format!("/{name}"), [("file.txt".into(), false)].into());
        }
    }
    control.contents.lock().unwrap().extend([
        ("/from/file.txt".into(), b"source retained".to_vec()),
        (
            "/to/file.txt".into(),
            b"target before external drift".to_vec(),
        ),
    ]);
    let weak = Arc::downgrade(&control);
    let once = Arc::new(AtomicBool::new(true));
    let observed = once.clone();
    *control.listing_observer.lock().unwrap() = Some(Arc::new(move |path| {
        if path == "/to" && observed.swap(false, Ordering::SeqCst) {
            let control = weak.upgrade().unwrap();
            control
                .fs
                .lock()
                .unwrap()
                .get_mut("/to")
                .unwrap()
                .remove("file.txt");
            control.contents.lock().unwrap().remove("/to/file.txt");
        }
    }));
    let area = private_area();
    let root = std::fs::canonicalize(area.path()).unwrap();
    let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
    let mut operation = fault_move_operation(fault_config(port), &["file.txt"]);
    if let QueueOperation::ProviderFile {
        expected_conflicts,
        copy_new_best_effort_expected,
        remove_source_best_effort_expected,
        ..
    } = &mut operation
    {
        *expected_conflicts = vec!["/to/file.txt".into()];
        *copy_new_best_effort_expected = None;
        *remove_source_best_effort_expected = None;
    }
    queue.enqueue(operation).unwrap();
    let result = execute_fault_move(&queue);
    assert!(
        !once.load(Ordering::SeqCst),
        "Zielwechsel wurde nicht tatsächlich ausgelöst"
    );
    let commands = control.mutations.lock().unwrap();
    assert!(
        commands.is_empty(),
        "Verschwundener bestätigter Skip startete neue Wirkungsproben: {commands:?}; {result:?}"
    );
    assert_eq!(
        control.contents.lock().unwrap()["/from/file.txt"],
        b"source retained"
    );
    assert!(!control.fs.lock().unwrap()["/to"].contains_key("file.txt"));
    assert!(
        result.is_err()
            || matches!(result, Ok(QueueExecutionResult::File(stats)) if stats.skipped==1 && stats.files==0)
    );
}

#[test]
fn ftps_leere_ordner_benoetigen_keine_stou_probe() {
    let mut failures = Vec::new();
    for (nested, merge) in [(false, false), (true, false), (false, true), (true, true)] {
        for route in ["local-copy", "local-move-target", "remote-move"] {
            let (port, control) = fault_ftp_server(&["keep.txt"]);
            {
                let mut fs = control.fs.lock().unwrap();
                for name in ["from", "to"] {
                    fs.get_mut("/").unwrap().insert(name.into(), true);
                    fs.insert(format!("/{name}"), Default::default());
                }
                fs.get_mut("/from").unwrap().insert("empty".into(), true);
                fs.insert("/from/empty".into(), Default::default());
                if nested {
                    fs.get_mut("/from/empty")
                        .unwrap()
                        .insert("child".into(), true);
                    fs.insert("/from/empty/child".into(), Default::default());
                }
                if merge {
                    fs.get_mut("/to").unwrap().insert("empty".into(), true);
                    fs.insert("/to/empty".into(), [("keep.txt".into(), false)].into());
                }
            }
            let area = private_area();
            let root = std::fs::canonicalize(area.path()).unwrap();
            let local_source = root.join("source");
            let local_target = root.join("target");
            std::fs::create_dir_all(local_source.join("empty")).unwrap();
            std::fs::create_dir(&local_target).unwrap();
            if nested {
                std::fs::create_dir(local_source.join("empty/child")).unwrap();
            }
            if merge {
                std::fs::create_dir(local_target.join("empty")).unwrap();
                std::fs::write(local_target.join("empty/keep.txt"), b"foreign").unwrap();
                control
                    .contents
                    .lock()
                    .unwrap()
                    .insert("/to/empty/keep.txt".into(), b"foreign".to_vec());
            }
            let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
            let config = fault_config(port);
            let local_copy = route == "local-copy";
            let local_destination = route == "local-move-target";
            let source = if local_copy {
                BidirectionalQueueEndpoint::Local {
                    root: local_source.to_string_lossy().into_owned(),
                }
            } else {
                BidirectionalQueueEndpoint::Ftps {
                    connection: Box::new(config.clone()),
                    root: "/from".into(),
                }
            };
            let target = if local_destination {
                BidirectionalQueueEndpoint::Local {
                    root: local_target.to_string_lossy().into_owned(),
                }
            } else {
                BidirectionalQueueEndpoint::Ftps {
                    connection: Box::new(config),
                    root: "/to".into(),
                }
            };
            let item = if local_copy {
                local_source.join("empty").to_string_lossy().into_owned()
            } else {
                "/from/empty".into()
            };
            let destination = if local_destination {
                local_target.to_string_lossy().into_owned()
            } else {
                "/to".into()
            };
            queue
                .enqueue(QueueOperation::ProviderFile {
                    kind: if local_copy {
                        FileQueueKind::Copy
                    } else {
                        FileQueueKind::Move
                    },
                    source: Box::new(source),
                    target: Box::new(target),
                    items: vec![item],
                    destination,
                    overwrite: false,
                    expected_conflicts: vec![],
                    source_tree_expected: None,
                    expected_conflict_proofs: vec![],
                    keep_both_targets: vec![],
                    best_effort_commit_expected: None,
                    local_copy_new_best_effort_expected: None,
                    copy_new_best_effort_expected: Some(!local_destination),
                    replace_best_effort_expected: Some(false),
                    remove_source_best_effort_expected: (!local_copy).then_some(true),
                })
                .unwrap();
            let result = execute_fault_move(&queue);
            let commands = control.mutations.lock().unwrap().clone();
            let fs = control.fs.lock().unwrap();
            let target_exists = if local_destination {
                local_target.join("empty").is_dir()
            } else {
                fs.contains_key("/to/empty")
            };
            let source_exists = if local_copy {
                local_source.join("empty").is_dir()
            } else {
                fs.contains_key("/from/empty")
            };
            let success = matches!(&result, Ok(QueueExecutionResult::File(stats)) if !stats.cancelled)
                && target_exists
                && source_exists == local_copy
                && !commands.iter().any(|cmd| cmd == "STOU")
                && queue.snapshot().entries[0].state == QueueState::Completed
                && queue.snapshot().entries[0].attempts == 1;
            if merge {
                assert!(fs["/to/empty"].contains_key("keep.txt"));
                assert_eq!(
                    control.contents.lock().unwrap()["/to/empty/keep.txt"],
                    b"foreign"
                );
                assert_eq!(
                    std::fs::read(local_target.join("empty/keep.txt")).unwrap(),
                    b"foreign"
                );
            }
            if success && nested {
                assert!(if local_destination {
                    local_target.join("empty/child").is_dir()
                } else {
                    fs.contains_key("/to/empty/child")
                });
            }
            if merge && !nested && !local_destination {
                assert!(
                    !commands.iter().any(|cmd| cmd.starts_with("MKD /to/")),
                    "Unnötige Zielprobe trotz vorhandener Ordner: {commands:?}"
                );
            }
            assert!(fs["/"].contains_key("keep.txt"));
            assert!(
                !fs.keys().any(|path| path.contains(".doppeldecker-")),
                "Probenrest bei {route}: {commands:?}"
            );
            if !success {
                failures.push(format!("{route}, nested={nested}, merge={merge}: {result:?}; target={target_exists}, source={source_exists}; commands={commands:?}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "Leere Ordner verlangen unnötigen Dateiinhalt:\n{}",
        failures.join("\n")
    );
}

fn execute_fault_move_with_cancel(
    queue: &OperationQueue,
    cancel: &Cancel,
) -> std::io::Result<QueueExecutionResult> {
    execute_provider_file_queue_entry(
        queue,
        queue.claim_next()?.unwrap(),
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(TestSecrets),
        cancel,
        &mut |_| {},
    )
}

#[test]
fn ftps_ordnerbeleg_gibt_weder_dateien_noch_andere_roots_frei() {
    use std::sync::atomic::Ordering;
    let (port, control) = fault_ftp_server(&[]);
    control.enable_stou.store(true, Ordering::SeqCst);
    control.rename_noreplace.store(true, Ordering::SeqCst);
    {
        let mut fs = control.fs.lock().unwrap();
        for root in ["from", "to"] {
            fs.get_mut("/").unwrap().insert(root.into(), true);
            fs.insert(format!("/{root}"), Default::default());
        }
        fs.get_mut("/from").unwrap().insert("foreign".into(), true);
        fs.insert("/from/foreign".into(), Default::default());
        fs.get_mut("/to")
            .unwrap()
            .insert("foreign.txt".into(), false);
    }
    control
        .contents
        .lock()
        .unwrap()
        .insert("/to/foreign.txt".into(), b"foreign".to_vec());
    let (vfs, area, queue) = owned_vfs(fault_config(port));
    // Der Owner umfasst absichtlich /; erst der tatsächlich unter /to erhobene
    // Typ-/Rootbeleg muss den anderen Root trotz dieser äußeren Hülle sperren.
    vfs.probe_add_only_capabilities_at(Path::new("/to"))
        .unwrap();
    vfs.probe_permanent_delete_capability_at(Path::new("/to"))
        .unwrap();
    assert!(vfs.capabilities().atomic_noreplace_rename && vfs.capabilities().permanent_delete);
    let local = std::fs::canonicalize(area.path()).unwrap().join("source");
    std::fs::create_dir_all(local.join("empty")).unwrap();
    let source = ProviderEndpoint::local(&local).unwrap();
    let target = ProviderEndpoint::ftps(Arc::new(vfs.clone()), Path::new("/to")).unwrap();
    let items = [local.join("empty")];
    let result = source
        .probe_interactive_file_target(doppeldecker_core::ProviderFileTargetProbe {
            target: &target,
            items: &items,
            kind: FileQueueKind::Copy,
            overwrite: false,
            skip_conflicts: false,
            inspect_conflicts: false,
            expected_conflicts: None,
            shared_session: false,
            targets_missing: true,
            capabilities_override: None,
        })
        .unwrap();
    assert!(result.capabilities.unwrap().copy_new_best_effort);
    assert!(!vfs.capabilities().atomic_noreplace_rename && !vfs.capabilities().permanent_delete);
    let mutations = control.mutations.lock().unwrap().clone();
    let context = target.file_context().unwrap().0;
    assert!(context.open_write_new(Path::new("/to/new.txt")).is_err());
    assert!(context.open_read(Path::new("/to/foreign.txt")).is_err());
    assert!(context.sync_file(Path::new("/to/foreign.txt")).is_err());
    assert!(context.remove_file(Path::new("/to/foreign.txt")).is_err());
    assert!(context.remove_dir_all(Path::new("/from/foreign")).is_err());
    assert!(context
        .rename_noreplace(Path::new("/to/foreign.txt"), Path::new("/to/new.txt"))
        .is_err());
    assert!(context
        .rename_plain_once(Path::new("/to/foreign.txt"), Path::new("/to/new.txt"))
        .is_err());
    assert!(context.create_dir(Path::new("/from/new")).is_err());
    assert!(context
        .remove_move_empty_dir_best_effort(Path::new("/from/foreign"))
        .is_err());
    assert!(target.probe_file_target_capabilities().is_err());
    assert_eq!(*control.mutations.lock().unwrap(), mutations);
    assert_eq!(
        control.contents.lock().unwrap()["/to/foreign.txt"],
        b"foreign"
    );
    let fresh = queue.bind_provider_preparation(&target).unwrap();
    assert!(fresh.probe_file_target_capabilities().unwrap().copy_new);
    assert_eq!(
        control
            .mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|cmd| cmd.as_str() == "STOU")
            .count(),
        mutations
            .iter()
            .filter(|cmd| cmd.as_str() == "STOU")
            .count()
            + 3
    );
}

#[test]
fn ftps_spaeter_dateiinhalt_bleibt_nach_ordnerkopie_in_der_quelle() {
    let (port, control) = fault_ftp_server(&[]);
    {
        let mut fs = control.fs.lock().unwrap();
        for root in ["from", "to"] {
            fs.get_mut("/").unwrap().insert(root.into(), true);
            fs.insert(format!("/{root}"), Default::default());
        }
        fs.get_mut("/from").unwrap().insert("empty".into(), true);
        fs.insert("/from/empty".into(), Default::default());
    }
    let weak = Arc::downgrade(&control);
    *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
        if command == "MKD /to/empty" {
            let control = weak.upgrade().unwrap();
            control
                .fs
                .lock()
                .unwrap()
                .get_mut("/from/empty")
                .unwrap()
                .insert("late.txt".into(), false);
            control
                .contents
                .lock()
                .unwrap()
                .insert("/from/empty/late.txt".into(), b"late foreign".to_vec());
        }
    }));
    let area = private_area();
    let queue = OperationQueue::open(area.path().join("queue.json")).unwrap();
    let mut operation = fault_move_operation(fault_config(port), &["empty"]);
    if let QueueOperation::ProviderFile {
        copy_new_best_effort_expected,
        remove_source_best_effort_expected,
        ..
    } = &mut operation
    {
        *copy_new_best_effort_expected = Some(true);
        *remove_source_best_effort_expected = Some(true);
    }
    queue.enqueue(operation).unwrap();
    assert!(execute_fault_move(&queue).is_err());
    assert_eq!(
        control.contents.lock().unwrap()["/from/empty/late.txt"],
        b"late foreign"
    );
    let fs = control.fs.lock().unwrap();
    assert!(fs.contains_key("/from/empty"));
    assert!(fs["/to/empty"].is_empty());
    let commands = control.mutations.lock().unwrap();
    assert!(!commands
        .iter()
        .any(|cmd| cmd == "STOU" || cmd == "RMD /from/empty" || cmd.starts_with("DELE /from/")));
}

#[test]
fn ftps_ordner_rmd_antwortverlust_bleibt_dauerhaft_unklar_ohne_retry() {
    for after in [false, true] {
        let (port, control) = fault_ftp_server(&[]);
        {
            let mut fs = control.fs.lock().unwrap();
            for root in ["from", "to"] {
                fs.get_mut("/").unwrap().insert(root.into(), true);
                fs.insert(format!("/{root}"), Default::default());
            }
            fs.get_mut("/from").unwrap().insert("empty".into(), true);
            fs.insert("/from/empty".into(), Default::default());
        }
        *control.rmd_drop_after.lock().unwrap() = Some(after);
        let area = private_area();
        let path = area.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let mut operation = fault_move_operation(fault_config(port), &["empty"]);
        if let QueueOperation::ProviderFile {
            copy_new_best_effort_expected,
            remove_source_best_effort_expected,
            ..
        } = &mut operation
        {
            *copy_new_best_effort_expected = Some(true);
            *remove_source_best_effort_expected = Some(true);
        }
        queue.enqueue(operation).unwrap();
        let error = execute_fault_move(&queue).unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Failed);
        assert!(!queue.snapshot().entries[0].temp_recovery_paths.is_empty());
        {
            let fs = control.fs.lock().unwrap();
            assert_eq!(fs.contains_key("/from/empty"), !after);
            assert!(fs["/to/empty"].is_empty());
        }
        let commands = control.mutations.lock().unwrap().clone();
        assert_eq!(
            commands
                .iter()
                .filter(|cmd| cmd.as_str() == "RMD /from/empty")
                .count(),
            1
        );
        assert!(!commands.iter().any(|cmd| cmd == "STOU"));
        let intents = std::fs::read_dir(area.path())
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                path.to_string_lossy().ends_with(".recovery.json").then(|| {
                    serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap())
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        assert!(intents
            .iter()
            .any(|intent| intent["ftps"]["phase"] == "delete"
                && intent["ftps"]["paths"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p.as_str() == Some("/from/empty"))));
        drop(queue);
        let reopened = OperationQueue::open(&path).unwrap();
        assert!(reopened.claim_next().unwrap().is_none());
        assert_eq!(*control.mutations.lock().unwrap(), commands);
    }
}

#[test]
fn ftps_ordnerkopie_laesst_symlink_und_spezialdatei_ohne_inhaltsprobe_aus() {
    use std::os::unix::ffi::OsStrExt;
    let (port, control) = fault_ftp_server(&[]);
    {
        let mut fs = control.fs.lock().unwrap();
        fs.get_mut("/").unwrap().insert("to".into(), true);
        fs.insert("/to".into(), Default::default());
    }
    let area = private_area();
    let root = std::fs::canonicalize(area.path()).unwrap();
    let source_root = root.join("source");
    std::fs::create_dir_all(source_root.join("empty/child")).unwrap();
    std::fs::write(root.join("outside.txt"), b"unchanged").unwrap();
    std::os::unix::fs::symlink(root.join("outside.txt"), source_root.join("empty/link")).unwrap();
    let fifo =
        std::ffi::CString::new(source_root.join("empty/fifo").as_os_str().as_bytes()).unwrap();
    // SAFETY: CString lebt bis nach dem Aufruf; der Pfad liegt im eigenen Testbaum.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
    queue
        .enqueue(QueueOperation::ProviderFile {
            kind: FileQueueKind::Copy,
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: source_root.to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::Ftps {
                connection: Box::new(fault_config(port)),
                root: "/to".into(),
            }),
            items: vec![source_root.join("empty").to_string_lossy().into_owned()],
            destination: "/to".into(),
            overwrite: false,
            expected_conflicts: vec![],
            source_tree_expected: None,
            expected_conflict_proofs: vec![],
            keep_both_targets: vec![],
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: Some(true),
            replace_best_effort_expected: Some(false),
            remove_source_best_effort_expected: None,
        })
        .unwrap();
    let result = execute_fault_move(&queue).unwrap();
    assert!(
        matches!(result,QueueExecutionResult::File(stats) if stats.files==0 && stats.symlinks_skipped==2)
    );
    assert_eq!(
        std::fs::read(root.join("outside.txt")).unwrap(),
        b"unchanged"
    );
    let fs = control.fs.lock().unwrap();
    assert_eq!(fs["/to/empty"], [("child".into(), true)].into());
    assert!(fs["/to/empty/child"].is_empty());
    assert!(!control
        .mutations
        .lock()
        .unwrap()
        .iter()
        .any(|cmd| cmd == "STOU"));
}

#[test]
fn ftps_abbruch_vor_der_ersten_probe_veraendert_nichts() {
    let (port, control) = fault_ftp_server(&[]);
    fault_cancel_source(&control, false);
    let area = private_area();
    let queue = OperationQueue::open(area.path().join("queue.json")).unwrap();
    let endpoint = ProviderEndpoint::ftps(
        Arc::new(FtpsVfs::connect(fault_config(port), Arc::new(TestSecrets)).unwrap()),
        Path::new("/to"),
    )
    .unwrap();
    let cancel = Cancel::new();
    let endpoint = queue
        .bind_provider_preparation_with_cancel(&endpoint, &cancel)
        .unwrap();
    cancel.cancel();
    let error = endpoint.probe_file_target_capabilities().unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::Interrupted);
    assert!(control.mutations.lock().unwrap().is_empty());
    assert!(queue.snapshot().entries.is_empty());
    assert!(std::fs::read_dir(area.path()).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".recovery.json")));
}

#[test]
fn ftps_abbruch_zwischen_typ_proben_beendet_den_queueeintrag_ohne_nutzerwirkung() {
    use std::sync::atomic::Ordering;
    let (port, control) = fault_ftp_server(&[]);
    control.enable_stou.store(true, Ordering::SeqCst);
    control.rename_noreplace.store(true, Ordering::SeqCst);
    fault_cancel_source(&control, true);
    let area = private_area();
    let path = area.path().join("queue.json");
    let queue = OperationQueue::open(&path).unwrap();
    queue
        .enqueue(fault_move_operation(
            fault_config(port),
            &["file.txt", "folder"],
        ))
        .unwrap();
    let cancel = Cancel::new();
    let flag = cancel.clone();
    *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
        if command.starts_with("RMD /to/.doppeldecker-probe-") {
            flag.cancel();
        }
    }));
    let result = execute_fault_move_with_cancel(&queue, &cancel).unwrap();
    assert!(
        matches!(result, QueueExecutionResult::File(stats) if stats.cancelled && stats.files == 0)
    );
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Cancelled);
    assert_eq!(queue.snapshot().entries[0].attempts, 1);
    assert_eq!(
        control.contents.lock().unwrap()["/from/file.txt"],
        b"source"
    );
    assert!(control.fs.lock().unwrap()["/to"].is_empty());
    let mutations = control.mutations.lock().unwrap().clone();
    assert_eq!(
        mutations
            .iter()
            .filter(|command| command.starts_with("MKD "))
            .count(),
        1,
        "Weitere Teilprobe nach Abbruch: {mutations:?}"
    );
    assert!(!mutations.iter().any(|command| command.contains("/from/")));
    assert!(std::fs::read_dir(area.path()).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".recovery.json")));
    drop(queue);
    let reopened = OperationQueue::open(&path).unwrap();
    assert_eq!(reopened.snapshot().entries[0].state, QueueState::Cancelled);
    assert!(reopened.claim_next().unwrap().is_none());
    assert_eq!(*control.mutations.lock().unwrap(), mutations);
}

#[test]
fn ftps_abbruch_waehrend_stou_erhaelt_jeden_unklaren_ausgang() {
    use std::sync::atomic::Ordering;
    for kind in [FileQueueKind::Copy, FileQueueKind::Move] {
        for unknown_name in [true, false] {
            let (port, control) = fault_ftp_server(&[]);
            fault_cancel_source(&control, false);
            control.lose_stou_name.store(unknown_name, Ordering::SeqCst);
            control.enable_stou.store(true, Ordering::SeqCst);
            control
                .lose_stou_completion
                .store(!unknown_name, Ordering::SeqCst);
            let area = private_area();
            let path = area.path().join("queue.json");
            let queue = OperationQueue::open(&path).unwrap();
            queue
                .enqueue({
                    let mut operation = fault_move_operation(fault_config(port), &["file.txt"]);
                    if let QueueOperation::ProviderFile {
                        kind: operation_kind,
                        ..
                    } = &mut operation
                    {
                        *operation_kind = kind;
                    }
                    operation
                })
                .unwrap();
            let cancel = Cancel::new();
            let flag = cancel.clone();
            *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
                if command == "STOU" {
                    flag.cancel();
                }
            }));
            let error = execute_fault_move_with_cancel(&queue, &cancel).unwrap_err();
            assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
            assert_eq!(queue.snapshot().entries[0].state, QueueState::Failed);
            assert_eq!(
                queue.snapshot().entries[0].error_code,
                Some(ErrorCode::OutcomeUnclear)
            );
            assert!(!queue.snapshot().entries[0].temp_recovery_paths.is_empty());
            let intents = std::fs::read_dir(area.path())
                .unwrap()
                .filter_map(|entry| {
                    let path = entry.unwrap().path();
                    path.to_string_lossy().ends_with(".recovery.json").then(|| {
                        serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap())
                            .unwrap()
                    })
                })
                .collect::<Vec<_>>();
            assert_eq!(intents.len(), 1);
            assert_eq!(intents[0]["ftps"]["stou_name_unknown"], unknown_name);
            assert_eq!(
                control.contents.lock().unwrap()["/from/file.txt"],
                b"source"
            );
            let mutations = control.mutations.lock().unwrap().clone();
            assert_eq!(
                mutations
                    .iter()
                    .filter(|cmd| cmd.as_str() == "STOU")
                    .count(),
                1
            );
            assert!(!mutations.iter().any(|cmd| cmd.starts_with("DELE ")
                || cmd.starts_with("RMD ")
                || cmd.contains("/from/")));
            drop(queue);
            let reopened = OperationQueue::open(&path).unwrap();
            assert!(reopened.claim_next().unwrap().is_none());
            assert_eq!(*control.mutations.lock().unwrap(), mutations);
        }
    }
}

#[test]
fn ftps_abbruch_nach_bekanntem_stou_laesst_eigene_probe_sauber_beenden() {
    use std::sync::atomic::Ordering;
    for kind in [FileQueueKind::Copy, FileQueueKind::Move] {
        let (port, control) = fault_ftp_server(&[]);
        control.enable_stou.store(true, Ordering::SeqCst);
        control.rename_noreplace.store(true, Ordering::SeqCst);
        fault_cancel_source(&control, false);
        let area = private_area();
        let queue = OperationQueue::open(area.path().join("queue.json")).unwrap();
        queue
            .enqueue({
                let mut operation = fault_move_operation(fault_config(port), &["file.txt"]);
                if let QueueOperation::ProviderFile {
                    kind: operation_kind,
                    ..
                } = &mut operation
                {
                    *operation_kind = kind;
                }
                operation
            })
            .unwrap();
        let directory = area.path().to_path_buf();
        let cancel = Cancel::new();
        let flag = cancel.clone();
        let known = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = known.clone();
        *control.stou_payload_observer.lock().unwrap() = Some(Arc::new(move |path| {
            let intent = std::fs::read_dir(&directory)
                .unwrap()
                .find_map(|entry| {
                    let path = entry.unwrap().path();
                    path.to_string_lossy().ends_with(".recovery.json").then(|| {
                        serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap())
                            .unwrap()
                    })
                })
                .unwrap();
            assert_eq!(intent["ftps"]["stou_name_unknown"], false);
            assert!(intent["ftps"]["paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str() == Some(path)));
            observed.store(true, Ordering::SeqCst);
            flag.cancel();
        }));
        let result = execute_fault_move_with_cancel(&queue, &cancel).unwrap();
        assert!(known.load(Ordering::SeqCst));
        assert!(
            matches!(result, QueueExecutionResult::File(stats) if stats.cancelled && stats.files == 0)
        );
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Cancelled);
        assert_eq!(
            control.contents.lock().unwrap()["/from/file.txt"],
            b"source"
        );
        assert!(control.fs.lock().unwrap()["/to"].is_empty());
        assert!(std::fs::read_dir(area.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".recovery.json")));
    }
}

impl SecretResolver for TestSecrets {
    fn resolve(&self, _reference: &KeychainSecretReference) -> std::io::Result<SecretString> {
        SecretString::new("anonymous-test".into())
    }
}

/// Dieser minimale Klartext-FTP-Server bildet zwei reale Kompatibilitätsgrenzen
/// unabhängig vom Doppeldecker-Parser nach: MLST-Fakten sind zwar verfügbar, aber
/// zunächst nicht aktiviert, und MLSD akzeptiert ausschließlich die in RFC 3659
/// definierte argumentlose Form für das zuvor per CWD gewählte Verzeichnis.
fn strict_mlsd_server() -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let thread = std::thread::spawn(move || {
        let (mut control, _) = listener.accept().unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        control
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(control.try_clone().unwrap());
        control.write_all(b"220 Strict MLSD test\r\n").unwrap();
        control.flush().unwrap();

        let mut facts_selected = false;
        let mut data_listener = None;
        loop {
            let mut command = String::new();
            assert_ne!(reader.read_line(&mut command).unwrap(), 0);
            let command = command.trim_end_matches(['\r', '\n']);
            let response = match command {
                value if value.starts_with("USER ") => "331 Password required\r\n",
                value if value.starts_with("PASS ") => "230 Logged in\r\n",
                "TYPE I" => "200 Binary mode\r\n",
                "FEAT" => {
                    "211-Features\r\n MLST Type;Size;Modify;UNIX.mode;\r\n UTF8\r\n211 End\r\n"
                }
                "OPTS UTF8 ON" => "200 UTF8 enabled\r\n",
                "OPTS MLST Type;Size;Modify;" => {
                    facts_selected = true;
                    "200 MLST OPTS Type;Size;Modify;\r\n"
                }
                "CWD /" => "250 Directory changed\r\n",
                "PWD" => "257 \"/\" is current directory\r\n",
                "EPSV" => {
                    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                    let data_port = listener.local_addr().unwrap().port();
                    data_listener = Some(listener);
                    let response =
                        format!("229 Entering Extended Passive Mode (|||{data_port}|)\r\n");
                    control.write_all(response.as_bytes()).unwrap();
                    control.flush().unwrap();
                    continue;
                }
                "MLSD" if facts_selected => {
                    control
                        .write_all(b"150 Opening data connection\r\n")
                        .unwrap();
                    control.flush().unwrap();
                    let (mut data, _) = data_listener.take().unwrap().accept().unwrap();
                    data.write_all(
                        b"Type=file;Size=12;Modify=20260728060000;UNIX.mode=0644; test.txt\r\n",
                    )
                    .unwrap();
                    data.flush().unwrap();
                    drop(data);
                    control.write_all(b"226 Transfer complete\r\n").unwrap();
                    control.flush().unwrap();
                    break;
                }
                value if value.starts_with("MLSD ") => {
                    "501 MLSD path arguments deliberately unsupported\r\n"
                }
                other => panic!("unerwarteter FTP-Testbefehl: {other:?}"),
            };
            control.write_all(response.as_bytes()).unwrap();
            control.flush().unwrap();
        }
    });
    (port, thread)
}

/// Steuert verlorene RNTO-Antworten vor/nach der Wirkung sowie einen bestätigten
/// Rename mit nur normalisiert weiterbestehendem Tempnamen.
#[derive(Clone, Copy, PartialEq)]
enum RntoFault {
    /// Verbindung fällt, BEVOR der Server die Umbenennung ausführt.
    DropBeforeMutation,
    /// Der Server benennt um und die Verbindung fällt VOR der Erfolgsantwort.
    DropAfterMutation,
    /// RNTO wird bestätigt, die Tempdatei bleibt aber unter normalisierter Schreibweise.
    NormalizeTempAndAcknowledge,
}

#[test]
fn ftp_upload_erhaelt_unklaren_abschluss_bei_weiteren_streamaufrufen() {
    use std::io::Write;
    use std::sync::atomic::Ordering;
    for fault in [RntoFault::DropBeforeMutation, RntoFault::DropAfterMutation] {
        let (port, control) = fault_ftp_server(&["keep.txt"]);
        control.enable_stou.store(true, Ordering::SeqCst);
        control
            .contents
            .lock()
            .unwrap()
            .insert("/keep.txt".into(), b"unveraendert".to_vec());
        let (vfs, _area, _queue) = owned_vfs(fault_config(port));
        vfs.probe_add_only_capabilities().unwrap();
        let mut writer = vfs.open_write_new(Path::new("/upload.tmp")).unwrap();
        writer.write_all(b"vollstaendig").unwrap();
        *control.fault.lock().unwrap() = Some(fault);
        let first = writer.flush().unwrap_err();
        assert_eq!(error_code(&first), ErrorCode::OutcomeUnclear);
        let paths = doppeldecker_core::error::recovery_paths(&first);
        assert!(paths.iter().any(|path| path == "/upload.tmp"));
        let before = control.mutations.lock().unwrap().clone();
        for error in [
            writer.flush().unwrap_err(),
            writer.write(b"weiter").unwrap_err(),
        ] {
            assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear, "{error}");
            let retained = doppeldecker_core::error::recovery_paths(&error);
            assert!(paths.iter().all(|path| retained.contains(path)));
        }
        drop(writer);
        assert_eq!(*control.mutations.lock().unwrap(), before);
        let contents = control.contents.lock().unwrap();
        assert_eq!(contents["/keep.txt"], b"unveraendert");
        assert_eq!(
            contents.contains_key("/upload.tmp"),
            fault == RntoFault::DropAfterMutation
        );
        assert_eq!(
            contents
                .values()
                .filter(|bytes| bytes.as_slice() == b"vollstaendig")
                .count(),
            1
        );
        assert_eq!(
            before
                .iter()
                .filter(|command| command.as_str() == "RNTO upload.tmp")
                .count(),
            1
        );
    }
}

/// In-Memory-Abbild des Serverbestands: Verzeichnispfad → (Name → ist Ordner).
type FaultFs = std::collections::HashMap<String, std::collections::BTreeMap<String, bool>>;
type ProbeGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
type MutationObserver = Arc<dyn Fn(&str) + Send + Sync>;

struct FaultControl {
    fs: std::sync::Mutex<FaultFs>,
    contents: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
    listings: std::sync::Mutex<Vec<String>>,
    mutations: std::sync::Mutex<Vec<String>>,
    retr_paths: std::sync::Mutex<Vec<String>>,
    retr_observer: std::sync::Mutex<Option<MutationObserver>>,
    deny_retr: std::sync::atomic::AtomicBool,
    reject_probe_mkdir: std::sync::atomic::AtomicBool,
    fail_after_probe_mkdir: std::sync::atomic::AtomicBool,
    fail_after_delete_probe_mkdir: std::sync::atomic::AtomicBool,
    reject_profile_listing: std::sync::atomic::AtomicBool,
    reject_listing_path: std::sync::Mutex<Option<String>>,
    reject_pane_readback: std::sync::atomic::AtomicBool,
    misplace_probe: std::sync::atomic::AtomicBool,
    /// Nächster RNTO-Fehler; wirkt nur auf Nicht-Probe-Pfade und genau einmal.
    fault: std::sync::Mutex<Option<RntoFault>>,
    fault_destination: std::sync::Mutex<Option<String>>,
    /// Nach dem nächsten Fehler meldet sich der Server mit anderem Willkommenstext:
    /// Der identitätsgeprüfte Reconnect muss dann ablehnen.
    swap_identity_after_fault: std::sync::atomic::AtomicBool,
    /// True, sobald der Willkommenstext gewechselt hat.
    identity_swapped: std::sync::atomic::AtomicBool,
    hold_first_probe: std::sync::Mutex<Option<ProbeGate>>,
    mutation_observer: std::sync::Mutex<Option<MutationObserver>>,
    listing_observer: std::sync::Mutex<Option<MutationObserver>>,
    stou_payload_observer: std::sync::Mutex<Option<MutationObserver>>,
    lose_stou_name: std::sync::atomic::AtomicBool,
    lose_stou_completion: std::sync::atomic::AtomicBool,
    rmd_drop_after: std::sync::Mutex<Option<bool>>,
    rename_noreplace: std::sync::atomic::AtomicBool,
    directory_only_noreplace: std::sync::atomic::AtomicBool,
    enable_stou: std::sync::atomic::AtomicBool,
    next_stou: std::sync::atomic::AtomicU64,
}

fn fault_path(cwd: &str, value: &str) -> String {
    let value = if value.starts_with('/') {
        value.to_owned()
    } else {
        format!("{cwd}/{value}")
    };
    let mut parts = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

fn fault_split(path: &str) -> (String, String) {
    let trimmed = path.trim_end_matches('/');
    let position = trimmed.rfind('/').unwrap();
    let parent = if position == 0 {
        "/"
    } else {
        &trimmed[..position]
    };
    (parent.to_string(), trimmed[position + 1..].to_string())
}

fn fault_remove_entry(fs: &mut FaultFs, path: &str) -> Option<bool> {
    let (parent, name) = fault_split(path);
    let is_dir = fs.get_mut(&parent)?.remove(&name)?;
    if is_dir {
        // Verzeichnisschlüssel samt Kindern mitnehmen; der Aufrufer hängt sie neu ein.
        fs.remove(path);
    }
    Some(is_dir)
}

/// Minimaler Klartext-FTP-Server mit steuerbarem RNTO-Antwortverlust. Er bedient
/// mehrere gleichzeitige Sitzungen (gehaltene Sitzung + frische Probe-Sitzungen)
/// und hält den Bestand ausschließlich im Speicher.
fn fault_ftp_server(initial_files: &[&str]) -> (u16, Arc<FaultControl>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut root = std::collections::BTreeMap::new();
    for name in initial_files {
        root.insert((*name).to_string(), false);
    }
    let mut fs = FaultFs::new();
    fs.insert("/".into(), root);
    let control = Arc::new(FaultControl {
        fs: std::sync::Mutex::new(fs),
        contents: std::sync::Mutex::new(Default::default()),
        listings: std::sync::Mutex::new(Vec::new()),
        mutations: std::sync::Mutex::new(Vec::new()),
        retr_paths: std::sync::Mutex::new(Vec::new()),
        retr_observer: std::sync::Mutex::new(None),
        deny_retr: std::sync::atomic::AtomicBool::new(false),
        reject_probe_mkdir: std::sync::atomic::AtomicBool::new(false),
        fail_after_probe_mkdir: std::sync::atomic::AtomicBool::new(false),
        fail_after_delete_probe_mkdir: std::sync::atomic::AtomicBool::new(false),
        reject_profile_listing: std::sync::atomic::AtomicBool::new(false),
        reject_listing_path: std::sync::Mutex::new(None),
        reject_pane_readback: std::sync::atomic::AtomicBool::new(false),
        misplace_probe: std::sync::atomic::AtomicBool::new(false),
        fault: std::sync::Mutex::new(None),
        fault_destination: std::sync::Mutex::new(None),
        swap_identity_after_fault: std::sync::atomic::AtomicBool::new(false),
        identity_swapped: std::sync::atomic::AtomicBool::new(false),
        hold_first_probe: std::sync::Mutex::new(None),
        mutation_observer: std::sync::Mutex::new(None),
        listing_observer: std::sync::Mutex::new(None),
        stou_payload_observer: std::sync::Mutex::new(None),
        lose_stou_name: std::sync::atomic::AtomicBool::new(false),
        lose_stou_completion: std::sync::atomic::AtomicBool::new(false),
        rmd_drop_after: std::sync::Mutex::new(None),
        rename_noreplace: std::sync::atomic::AtomicBool::new(false),
        directory_only_noreplace: std::sync::atomic::AtomicBool::new(false),
        enable_stou: std::sync::atomic::AtomicBool::new(false),
        next_stou: std::sync::atomic::AtomicU64::new(1),
    });
    let shared = control.clone();
    std::thread::spawn(move || loop {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let state = shared.clone();
        std::thread::spawn(move || fault_session(stream, state));
    });
    (port, control)
}

fn fault_session(mut control_stream: TcpStream, state: Arc<FaultControl>) {
    use std::sync::atomic::Ordering;
    control_stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    control_stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(control_stream.try_clone().unwrap());
    let greeting: &[u8] = if state.identity_swapped.load(Ordering::SeqCst) {
        b"220 Anderer Server\r\n"
    } else {
        b"220 Fault-Injection FTP\r\n"
    };
    if control_stream.write_all(greeting).is_err() {
        return;
    }
    let mut cwd = String::from("/");
    let mut pending_rename: Option<String> = None;
    let mut data_listener: Option<TcpListener> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let command = line.trim_end_matches(['\r', '\n']);
        if ["MKD", "RMD", "STOU", "STOR", "RNFR", "RNTO", "DELE"]
            .contains(&command.split_whitespace().next().unwrap_or_default())
        {
            state.mutations.lock().unwrap().push(command.to_string());
            if let Some(observer) = state.mutation_observer.lock().unwrap().clone() {
                observer(command);
            }
        }
        let response: String = match command {
            value if value.starts_with("USER ") => "331 Password required\r\n".into(),
            value if value.starts_with("PASS ") => "230 Logged in\r\n".into(),
            "TYPE I" => "200 Binary mode\r\n".into(),
            "STOU" => {
                if state
                    .lose_stou_name
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    state
                        .fs
                        .lock()
                        .unwrap()
                        .get_mut(&cwd)
                        .unwrap()
                        .insert("unknown-stou".into(), false);
                    return;
                }
                if !state.enable_stou.load(Ordering::SeqCst) {
                    "502 STOU unavailable\r\n".into()
                } else {
                    let name = format!("owned-{}", state.next_stou.fetch_add(1, Ordering::SeqCst));
                    let path = fault_path(&cwd, &name);
                    state
                        .fs
                        .lock()
                        .unwrap()
                        .get_mut(&cwd)
                        .unwrap()
                        .insert(name.clone(), false);
                    state
                        .contents
                        .lock()
                        .unwrap()
                        .insert(path.clone(), Vec::new());
                    control_stream
                        .write_all(format!("150 FILE: {name}\r\n").as_bytes())
                        .unwrap();
                    control_stream.flush().unwrap();
                    let (mut data, _) = data_listener.take().unwrap().accept().unwrap();
                    data.set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let mut body = Vec::new();
                    if let Some(observer) = state.stou_payload_observer.lock().unwrap().clone() {
                        let mut first = [0];
                        assert_eq!(data.read(&mut first).unwrap(), 1);
                        body.push(first[0]);
                        observer(&path);
                    }
                    Read::by_ref(&mut data)
                        .take(16 * 1024 * 1024 + 1)
                        .read_to_end(&mut body)
                        .unwrap();
                    assert!(
                        body.len() <= 16 * 1024 * 1024,
                        "STOU-Testinhalt überschreitet das Fixturelimit"
                    );
                    state.contents.lock().unwrap().insert(path, body);
                    if state.lose_stou_completion.swap(false, Ordering::SeqCst) {
                        return;
                    }
                    "226 Transfer complete\r\n".into()
                }
            }
            "FEAT" => "211-Features\r\n MLST Type;Size;Modify;\r\n UTF8\r\n211 End\r\n".into(),
            "OPTS UTF8 ON" => "200 UTF8 enabled\r\n".into(),
            "OPTS MLST Type;Size;Modify;" => "200 MLST OPTS Type;Size;Modify;\r\n".into(),
            "QUIT" => {
                let _ = control_stream.write_all(b"221 Bye\r\n");
                return;
            }
            "PWD" => format!("257 \"{cwd}\" is current directory\r\n"),
            value if value.starts_with("CWD ") => {
                let target = fault_path(&cwd, &value[4..]);
                if state.fs.lock().unwrap().contains_key(&target) {
                    cwd = target;
                    "250 Directory changed\r\n".into()
                } else {
                    "550 No such directory\r\n".into()
                }
            }
            "EPSV" => {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let data_port = listener.local_addr().unwrap().port();
                data_listener = Some(listener);
                format!("229 Entering Extended Passive Mode (|||{data_port}|)\r\n")
            }
            "MLSD" => {
                let pane_reads = {
                    let mut listings = state.listings.lock().unwrap();
                    listings.push(cwd.clone());
                    listings.iter().filter(|path| *path == "/pane").count()
                };
                if state.reject_listing_path.lock().unwrap().as_deref() == Some(&cwd)
                    || (cwd == "/" && state.reject_profile_listing.load(Ordering::SeqCst))
                    || (cwd == "/pane"
                        && pane_reads > 1
                        && state.reject_pane_readback.load(Ordering::SeqCst))
                {
                    data_listener.take();
                    if control_stream
                        .write_all(b"450 Profile listing unavailable\r\n")
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                let lines: String = {
                    let fs = state.fs.lock().unwrap();
                    fs.get(&cwd)
                        .map(|entries| {
                            entries
                                .iter()
                                .map(|(name, is_dir)| {
                                    if *is_dir {
                                        format!("Type=dir;Size=0;Modify=20260728060000; {name}\r\n")
                                    } else {
                                        let path = format!("{}/{name}", cwd.trim_end_matches('/'));
                                        let size = state.contents.lock().unwrap().get(&path).map_or(4, Vec::len);
                                        format!(
                                            "Type=file;Size={size};Modify=20260728060000; {name}\r\n"
                                        )
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let observer = state.listing_observer.lock().unwrap().clone();
                if let Some(observer) = observer {
                    observer(&cwd);
                }
                control_stream
                    .write_all(b"150 Opening data connection\r\n")
                    .unwrap();
                let (mut data, _) = data_listener.take().unwrap().accept().unwrap();
                data.write_all(lines.as_bytes()).unwrap();
                drop(data);
                "226 Transfer complete\r\n".into()
            }
            value if value.starts_with("RETR ") => {
                state
                    .retr_paths
                    .lock()
                    .unwrap()
                    .push(fault_path(&cwd, &value[5..]));
                if state.deny_retr.load(Ordering::SeqCst) {
                    control_stream
                        .write_all(b"550 RETR denied by artificial fixture\r\n")
                        .unwrap();
                    continue;
                }
                control_stream
                    .write_all(b"150 Opening data connection\r\n")
                    .unwrap();
                let (mut data, _) = data_listener.take().unwrap().accept().unwrap();
                let content = state
                    .contents
                    .lock()
                    .unwrap()
                    .get(&fault_path(&cwd, &value[5..]))
                    .cloned()
                    .unwrap_or_else(|| b"data".to_vec());
                if let Some(observer) = state.retr_observer.lock().unwrap().clone() {
                    observer(&fault_path(&cwd, &value[5..]));
                }
                if data.write_all(&content).is_err() {
                    return;
                }
                drop(data);
                "226 Transfer complete\r\n".into()
            }
            value if value.starts_with("MKD ") => {
                let path = fault_path(&cwd, &value[4..]);
                let (parent, name) = fault_split(&path);
                if state.reject_probe_mkdir.load(Ordering::SeqCst) {
                    if control_stream
                        .write_all(b"450 MKD unavailable\r\n")
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                let mut fs = state.fs.lock().unwrap();
                if state.misplace_probe.load(Ordering::SeqCst) {
                    fs.get_mut("/").unwrap().insert(name.clone(), true);
                }
                fs.get_mut(&parent).unwrap().insert(name, true);
                fs.entry(path.clone()).or_default();
                let response = if state.fail_after_probe_mkdir.load(Ordering::SeqCst)
                    || (path.contains(".doppeldecker-delete-probe-")
                        && state.fail_after_delete_probe_mkdir.load(Ordering::SeqCst))
                {
                    "450 MKD result unavailable\r\n".into()
                } else {
                    format!("257 \"{path}\" created\r\n")
                };
                drop(fs);
                let gate = state.hold_first_probe.lock().unwrap().take();
                if let Some((started, release)) = gate {
                    started.send(()).unwrap();
                    release.recv_timeout(Duration::from_secs(10)).unwrap();
                }
                response
            }
            value if value.starts_with("RMD ") => {
                let path = fault_path(&cwd, &value[4..]);
                let fault = if path == "/from/empty" {
                    state.rmd_drop_after.lock().unwrap().take()
                } else {
                    None
                };
                if fault == Some(false) {
                    return;
                }
                let mut fs = state.fs.lock().unwrap();
                if fs.get(&path).is_some_and(|children| !children.is_empty()) {
                    if control_stream
                        .write_all(b"550 Directory not empty\r\n")
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                fault_remove_entry(&mut fs, &path);
                if fault == Some(true) {
                    return;
                }
                "250 Directory removed\r\n".into()
            }
            value if value.starts_with("DELE ") => {
                let path = fault_path(&cwd, &value[5..]);
                fault_remove_entry(&mut state.fs.lock().unwrap(), &path);
                state.contents.lock().unwrap().remove(&path);
                "250 File removed\r\n".into()
            }
            value if value.starts_with("RNFR ") => {
                pending_rename = Some(fault_path(&cwd, &value[5..]));
                "350 Ready for RNTO\r\n".into()
            }
            value if value.starts_with("RNTO ") => {
                let to = fault_path(&cwd, &value[5..]);
                let from = pending_rename.take().unwrap();
                let (parent, name) = fault_split(&to);
                let (from_parent, from_name) = fault_split(&from);
                let source_is_directory = state.fs.lock().unwrap()[&from_parent][&from_name];
                if state.rename_noreplace.load(Ordering::SeqCst)
                    && (!state.directory_only_noreplace.load(Ordering::SeqCst)
                        || source_is_directory)
                    && state
                        .fs
                        .lock()
                        .unwrap()
                        .get(&parent)
                        .is_some_and(|entries| entries.contains_key(&name))
                {
                    if control_stream.write_all(b"553 Target exists\r\n").is_err() {
                        return;
                    }
                    continue;
                }
                // Nur die echte Nutzermutation (kein Probe-Pfad) löst den Fehler aus.
                let chosen_destination = state.fault_destination.lock().unwrap().clone();
                let fault = if chosen_destination.as_deref() == Some(to.as_str())
                    || (chosen_destination.is_none() && !from.contains(".doppeldecker-"))
                {
                    state.fault.lock().unwrap().take()
                } else {
                    None
                };
                if fault == Some(RntoFault::DropBeforeMutation) {
                    if state.swap_identity_after_fault.load(Ordering::SeqCst) {
                        state.identity_swapped.store(true, Ordering::SeqCst);
                    }
                    return; // Verbindung fällt, Server hat nichts verändert.
                }
                if fault == Some(RntoFault::NormalizeTempAndAcknowledge) {
                    let normalized_name = from_name.to_uppercase();
                    let normalized =
                        format!("{}/{}", from_parent.trim_end_matches('/'), normalized_name);
                    {
                        let mut fs = state.fs.lock().unwrap();
                        let is_dir = fault_remove_entry(&mut fs, &from).unwrap();
                        fs.get_mut(&from_parent)
                            .unwrap()
                            .insert(normalized_name, is_dir);
                    }
                    let body = state.contents.lock().unwrap().remove(&from).unwrap();
                    state.contents.lock().unwrap().insert(normalized, body);
                    if control_stream
                        .write_all(b"250 Rename successful\r\n")
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                {
                    let mut fs = state.fs.lock().unwrap();
                    // Überschreibendes RNTO wie bei vielen realen Servern: ein
                    // vorhandenes Ziel wird ersetzt, nicht abgelehnt.
                    fault_remove_entry(&mut fs, &to);
                    let is_dir = fault_remove_entry(&mut fs, &from).unwrap();
                    let (to_parent, to_name) = fault_split(&to);
                    fs.get_mut(&to_parent).unwrap().insert(to_name, is_dir);
                    if is_dir {
                        fs.entry(to.clone()).or_default();
                    }
                }
                {
                    let mut contents = state.contents.lock().unwrap();
                    let body = contents.remove(&from);
                    contents.remove(&to);
                    if let Some(body) = body {
                        contents.insert(to.clone(), body);
                    }
                }
                if fault == Some(RntoFault::DropAfterMutation) {
                    if state.swap_identity_after_fault.load(Ordering::SeqCst) {
                        state.identity_swapped.store(true, Ordering::SeqCst);
                    }
                    return; // Mutation ist ausgeführt, die Antwort geht verloren.
                }
                "250 Rename successful\r\n".into()
            }
            other => panic!("unerwarteter Fault-FTP-Befehl: {other:?}"),
        };
        if control_stream.write_all(response.as_bytes()).is_err() {
            return;
        }
    }
}

#[test]
fn ftps_queueziel_journalisiert_vor_mkd_und_stou_ohne_loeschteilprobe() {
    let (port, control) = fault_ftp_server(&[]);
    {
        let mut fs = control.fs.lock().unwrap();
        fs.get_mut("/").unwrap().insert("pane".into(), true);
        fs.insert("/pane".into(), Default::default());
    }
    control
        .lose_stou_name
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let area = private_area();
    let area_path = std::fs::canonicalize(area.path()).unwrap();
    let source = area_path.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file.txt"), b"eins").unwrap();
    let queue_path = area_path.join("config/operations-queue.json");
    let queue = OperationQueue::open(&queue_path).unwrap();
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: vec![],
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let queued = queue
        .enqueue(QueueOperation::ProviderFile {
            kind: FileQueueKind::Copy,
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: source.to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::Ftps {
                connection: Box::new(config),
                root: "/pane".into(),
            }),
            items: vec![source.join("file.txt").to_string_lossy().into_owned()],
            destination: "/pane".into(),
            overwrite: false,
            expected_conflicts: vec![],
            source_tree_expected: None,
            keep_both_targets: vec![],
            expected_conflict_proofs: vec![],
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: Some(true),
            replace_best_effort_expected: Some(false),
            remove_source_best_effort_expected: None,
        })
        .unwrap();
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = observed.clone();
    let intent_directory = queue_path.parent().unwrap().to_path_buf();
    *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
        if !(command.starts_with("MKD ") || command == "STOU") {
            return;
        }
        let records = std::fs::read_dir(&intent_directory)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                if !path.to_string_lossy().ends_with(".recovery.json") {
                    return None;
                }
                Some(
                    serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap())
                        .unwrap(),
                )
            })
            .collect::<Vec<_>>();
        captured.lock().unwrap().push((command.to_owned(), records));
    }));
    let entry = queue.claim_next().unwrap().unwrap();
    let error = execute_provider_file_queue_entry(
        &queue,
        entry,
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(TestSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    let observed = observed.lock().unwrap();
    assert_eq!(
        observed
            .iter()
            .filter(|(command, _)| command == "STOU")
            .count(),
        1
    );
    for (command, records) in observed.iter() {
        let own = records
            .iter()
            .find(|record| record["owner"]["operation"] == queued.operation_id)
            .expect("Intent fehlt vor Serverwirkung");
        assert_eq!(own["version"], 2);
        assert_eq!(own["ftps"]["roots"][0]["virtual_root"], "/pane");
        if command == "STOU" {
            assert_eq!(own["ftps"]["phase"], "stou");
            assert_eq!(own["ftps"]["stou_name_unknown"], true);
        }
    }
    let mutations = control.mutations.lock().unwrap().clone();
    assert!(!mutations
        .iter()
        .any(|command| command.contains("delete-probe") || command.contains("mkdir-probe")));
    let base = control
        .fs
        .lock()
        .unwrap()
        .keys()
        .find(|path| path.starts_with("/pane/.doppeldecker-probe-"))
        .unwrap()
        .clone();
    control
        .fs
        .lock()
        .unwrap()
        .get_mut(&base)
        .unwrap()
        .insert("later-foreign".into(), false);
    let original = control.fs.lock().unwrap().clone();
    assert_eq!(std::fs::read(source.join("file.txt")).unwrap(), b"eins");
    drop(queue);
    let reopened = OperationQueue::open(&queue_path).unwrap();
    let entry = reopened
        .snapshot()
        .entries
        .into_iter()
        .find(|entry| entry.operation_id == queued.operation_id)
        .unwrap();
    assert_eq!(entry.state, QueueState::Interrupted);
    assert_eq!(entry.error_code, Some(ErrorCode::OutcomeUnclear));
    assert_eq!(entry.attempts, 1);
    assert!(entry.temp_recovery_paths.iter().any(|path| path == &base));
    assert!(reopened.retry(&entry.operation_id).is_err());
    assert!(reopened.claim_next().unwrap().is_none());
    assert!(reopened
        .acknowledge_unclear_outcome(&entry.operation_id, false)
        .is_err());
    reopened
        .acknowledge_unclear_outcome(&entry.operation_id, true)
        .unwrap();
    assert_eq!(*control.fs.lock().unwrap(), original);
    assert_eq!(*control.mutations.lock().unwrap(), mutations);
}

#[test]
fn ftps_vorbereitungsreste_gehoeren_nur_zum_eigenen_fensterjournal() {
    let (port, control) = fault_ftp_server(&[]);
    {
        let mut fs = control.fs.lock().unwrap();
        fs.get_mut("/").unwrap().insert("pane".into(), true);
        fs.insert("/pane".into(), Default::default());
    }
    control
        .fail_after_probe_mkdir
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let area = private_area();
    let config_directory = std::fs::canonicalize(area.path()).unwrap();
    let first_path = config_directory.join("operations-queue-window-first.json");
    let second_path = config_directory.join("operations-queue-window-second.json");
    let process_lock =
        Arc::new(doppeldecker_core::OperationQueueProcessLock::acquire(&first_path).unwrap());
    let first = OperationQueue::open_with_process_lock(&first_path, process_lock.clone()).unwrap();
    let second =
        OperationQueue::open_with_process_lock(&second_path, process_lock.clone()).unwrap();
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: vec![],
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let vfs = Arc::new(FtpsVfs::connect(config, Arc::new(TestSecrets)).unwrap());
    let endpoint = ProviderEndpoint::ftps(vfs.clone(), Path::new("/pane")).unwrap();
    let owned = first.bind_provider_preparation(&endpoint).unwrap();
    assert_eq!(
        error_code(&owned.probe_directory_create_capability().unwrap_err()),
        ErrorCode::OutcomeUnclear
    );
    assert!(first.snapshot().entries.is_empty());
    assert!(second.snapshot().entries.is_empty());
    let original = control.fs.lock().unwrap().clone();
    let mutations = control.mutations.lock().unwrap().clone();
    drop(owned);
    drop(endpoint);
    drop(vfs);
    drop(first);
    drop(second);
    drop(process_lock);
    let process_lock =
        Arc::new(doppeldecker_core::OperationQueueProcessLock::acquire(&first_path).unwrap());
    let second =
        OperationQueue::open_with_process_lock(&second_path, process_lock.clone()).unwrap();
    assert!(second.snapshot().entries.is_empty());
    let first = OperationQueue::open_with_process_lock(&first_path, process_lock).unwrap();
    let snapshot = first.snapshot();
    assert_eq!(snapshot.entries.len(), 1);
    let entry = &snapshot.entries[0];
    assert_eq!(
        serde_json::to_value(&entry.operation).unwrap()["type"],
        "FtpsRecovery"
    );
    assert_eq!(entry.state, QueueState::Interrupted);
    assert_eq!(entry.error_code, Some(ErrorCode::OutcomeUnclear));
    assert!(first.retry(&entry.operation_id).is_err());
    assert!(first.claim_next().unwrap().is_none());
    assert!(!entry.temp_recovery_paths.is_empty());
    assert_eq!(*control.fs.lock().unwrap(), original);
    assert_eq!(*control.mutations.lock().unwrap(), mutations);
}

#[test]
fn ftps_journalordnertausch_stoppt_vor_der_ersten_serverwirkung() {
    let (port, control) = fault_ftp_server(&[]);
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: vec![],
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let area = private_area();
    let root = std::fs::canonicalize(area.path()).unwrap();
    let directory = root.join("config");
    let queue = OperationQueue::open(directory.join("operations-queue.json")).unwrap();
    let shared = Arc::new(FtpsVfs::connect(config, Arc::new(TestSecrets)).unwrap());
    let endpoint = ProviderEndpoint::ftps(shared, Path::new("/")).unwrap();
    let owned = queue.bind_provider_preparation(&endpoint).unwrap();
    std::fs::rename(&directory, root.join("old-config")).unwrap();
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("foreign.txt"), b"behalten").unwrap();
    assert_eq!(
        error_code(&owned.probe_directory_create_capability().unwrap_err()),
        ErrorCode::PlanDrift
    );
    assert!(control.mutations.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
    assert_eq!(
        std::fs::read(directory.join("foreign.txt")).unwrap(),
        b"behalten"
    );
}

#[test]
fn frischer_widerspruch_entzieht_dem_alten_noreplace_beleg_die_wirkung() {
    let (port, control) = fault_ftp_server(&[]);
    control
        .rename_noreplace
        .store(true, std::sync::atomic::Ordering::SeqCst);
    {
        let mut fs = control.fs.lock().unwrap();
        for name in ["source", "existing"] {
            fs.get_mut("/").unwrap().insert(name.into(), true);
            fs.insert(format!("/{name}"), Default::default());
        }
    }
    let area = private_area();
    let directory = std::fs::canonicalize(area.path()).unwrap();
    let first_path = directory.join("operations-queue-first.json");
    let lock =
        Arc::new(doppeldecker_core::OperationQueueProcessLock::acquire(&first_path).unwrap());
    let first_queue = OperationQueue::open_with_process_lock(&first_path, lock.clone()).unwrap();
    let second_queue = OperationQueue::open_with_process_lock(
        directory.join("operations-queue-second.json"),
        lock,
    )
    .unwrap();
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: vec![],
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let shared = FtpsVfs::connect(config, Arc::new(TestSecrets)).unwrap();
    let first = shared
        .for_queue_preparation(&first_queue, Path::new("/"))
        .unwrap();
    let second = shared
        .for_queue_preparation(&second_queue, Path::new("/"))
        .unwrap();
    let first_endpoint = ProviderEndpoint::ftps(Arc::new(first.clone()), Path::new("/")).unwrap();
    let second_endpoint = ProviderEndpoint::ftps(Arc::new(second), Path::new("/")).unwrap();
    assert!(
        first_endpoint
            .probe_rename_capabilities_for_item(Path::new("/source"))
            .unwrap()
            .assured
    );
    control
        .rename_noreplace
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(
        !second_endpoint
            .probe_rename_capabilities_for_item(Path::new("/source"))
            .unwrap()
            .assured
    );
    let original = control.fs.lock().unwrap().clone();
    let mutations = control.mutations.lock().unwrap().clone();
    let result = first.rename_noreplace(Path::new("/source"), Path::new("/existing"));
    assert!(
        result.is_err(),
        "Neue Gegenprobe darf den alten abgesicherten Beleg nicht stehen lassen"
    );
    assert_eq!(error_code(&result.unwrap_err()), ErrorCode::PlanDrift);
    assert_eq!(*control.fs.lock().unwrap(), original);
    assert_eq!(*control.mutations.lock().unwrap(), mutations);
}

#[test]
fn ftps_ordnerbeleg_erlaubt_keinen_datei_noreplace_rename() {
    use std::sync::atomic::Ordering;
    let (port, control) = fault_ftp_server(&["source.txt"]);
    control.rename_noreplace.store(true, Ordering::SeqCst);
    control
        .directory_only_noreplace
        .store(true, Ordering::SeqCst);
    control
        .contents
        .lock()
        .unwrap()
        .insert("/source.txt".into(), b"source".to_vec());
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: vec![],
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let (vfs, _area, _queue) = owned_vfs(config);
    assert!(vfs.probe_rename_capability().unwrap().best_effort_rename);
    // Die Datei entstand erst nach der vollständig abgeschlossenen Ordnerprobe.
    control
        .fs
        .lock()
        .unwrap()
        .get_mut("/")
        .unwrap()
        .insert("late.txt".into(), false);
    control
        .contents
        .lock()
        .unwrap()
        .insert("/late.txt".into(), b"foreign".to_vec());
    let result = vfs.rename_noreplace(Path::new("/source.txt"), Path::new("/late.txt"));
    assert_eq!(
        control.contents.lock().unwrap()["/late.txt"],
        b"foreign",
        "Ordnerbeleg ließ fremden Dateiinhalt ersetzen: {result:?}"
    );
    assert_eq!(control.contents.lock().unwrap()["/source.txt"], b"source");
    assert!(
        result.is_err(),
        "Datei-Rename erhielt ohne Dateibeleg eine Freigabe"
    );
}

#[test]
fn ftps_move_verlangt_dateibeleg_und_bestaetigt_den_rueckfall() {
    use std::sync::atomic::Ordering;
    let (port, control) = fault_ftp_server(&[]);
    control.enable_stou.store(true, Ordering::SeqCst);
    control.rename_noreplace.store(true, Ordering::SeqCst);
    control
        .directory_only_noreplace
        .store(true, Ordering::SeqCst);
    {
        let mut fs = control.fs.lock().unwrap();
        for name in ["from", "to"] {
            fs.get_mut("/").unwrap().insert(name.into(), true);
            fs.insert(format!("/{name}"), Default::default());
        }
        fs.get_mut("/from")
            .unwrap()
            .insert("file.txt".into(), false);
    }
    control
        .contents
        .lock()
        .unwrap()
        .insert("/from/file.txt".into(), b"source".to_vec());
    let weak = Arc::downgrade(&control);
    *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
        if command == "RNTO /to/file.txt" {
            let control = weak.upgrade().unwrap();
            control
                .fs
                .lock()
                .unwrap()
                .get_mut("/to")
                .unwrap()
                .insert("file.txt".into(), false);
            control
                .contents
                .lock()
                .unwrap()
                .insert("/to/file.txt".into(), b"foreign".to_vec());
        }
    }));
    let area = private_area();
    let path = std::fs::canonicalize(area.path())
        .unwrap()
        .join("operations-queue.json");
    let queue = OperationQueue::open(path).unwrap();
    queue
        .enqueue(fault_move_operation(fault_config(port), &["file.txt"]))
        .unwrap();
    let result = execute_fault_move(&queue);
    let contents = control.contents.lock().unwrap();
    if let Some(target) = contents.get("/to/file.txt") {
        assert_eq!(
            target, b"foreign",
            "Ordnerprobe erlaubte überschreibendes Datei-RNTO: {result:?}"
        );
    }
    assert_eq!(contents.get("/from/file.txt").unwrap(), b"source");
    assert!(
        matches!(
            result,
            Ok(QueueExecutionResult::AwaitingConfirmation { .. })
        ),
        "F6 muss vor dem unbestätigten Copy-/Remove-Rückfall halten: {result:?}"
    );
    assert!(!contents.contains_key("/to/file.txt"));
    assert!(!control
        .mutations
        .lock()
        .unwrap()
        .iter()
        .any(|command| command == "RNFR /from/file.txt" || command == "DELE /from/file.txt"));
    assert!(!std::fs::read_dir(area.path()).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".recovery.json")));
}

#[test]
fn ftps_move_bindet_typbelege_an_den_tatsaechlichen_zielroot() {
    use std::sync::atomic::Ordering;
    for (files, directories, collision) in [
        (true, false, false),
        (false, true, false),
        (true, true, false),
        (true, false, true),
    ] {
        let (port, control) = fault_ftp_server(&[]);
        control.enable_stou.store(files, Ordering::SeqCst);
        control.rename_noreplace.store(true, Ordering::SeqCst);
        let mut items = Vec::new();
        {
            let mut fs = control.fs.lock().unwrap();
            for name in ["from", "to"] {
                fs.get_mut("/").unwrap().insert(name.into(), true);
                fs.insert(format!("/{name}"), Default::default());
            }
            if files {
                fs.get_mut("/from")
                    .unwrap()
                    .insert("file.txt".into(), false);
                items.push("file.txt");
            }
            if directories {
                fs.get_mut("/from").unwrap().insert("folder".into(), true);
                fs.insert("/from/folder".into(), Default::default());
                items.push("folder");
            }
        }
        if files {
            control
                .contents
                .lock()
                .unwrap()
                .insert("/from/file.txt".into(), b"source".to_vec());
        }
        if collision {
            let weak = Arc::downgrade(&control);
            *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
                if command == "RNTO /to/file.txt" {
                    let control = weak.upgrade().unwrap();
                    control
                        .fs
                        .lock()
                        .unwrap()
                        .get_mut("/to")
                        .unwrap()
                        .insert("file.txt".into(), false);
                    control
                        .contents
                        .lock()
                        .unwrap()
                        .insert("/to/file.txt".into(), b"foreign".to_vec());
                }
            }));
        }
        let area = private_area();
        let path = std::fs::canonicalize(area.path())
            .unwrap()
            .join("operations-queue.json");
        let queue = OperationQueue::open(path).unwrap();
        queue
            .enqueue(fault_move_operation(fault_config(port), &items))
            .unwrap();
        let result = execute_fault_move(&queue);
        let commands = control.mutations.lock().unwrap().clone();
        assert!(
            commands
                .iter()
                .filter(|cmd| cmd.starts_with("MKD "))
                .all(|cmd| cmd.starts_with("MKD /to/.doppeldecker-")),
            "Probe verließ den RNTO-Zielroot: {commands:?}"
        );
        assert_eq!(
            commands.iter().filter(|cmd| cmd.as_str() == "STOU").count(),
            usize::from(files) * 3,
            "files={files}, directories={directories}, collision={collision}, result={result:?}, commands={commands:?}"
        );
        assert!(!commands.iter().any(|cmd| cmd.starts_with("DELE /from/")));
        if collision {
            assert!(
                result.is_err(),
                "Späte Kollision wurde verschoben: {result:?}"
            );
            assert_eq!(control.contents.lock().unwrap()["/to/file.txt"], b"foreign");
            assert_eq!(
                control.contents.lock().unwrap()["/from/file.txt"],
                b"source"
            );
            assert_eq!(
                commands
                    .iter()
                    .filter(|cmd| cmd.as_str() == "RNTO /to/file.txt")
                    .count(),
                1
            );
        } else {
            assert!(
                matches!(result, Ok(QueueExecutionResult::File(_))),
                "Typgebundener Move fehlt: {result:?}"
            );
            if files {
                assert_eq!(control.contents.lock().unwrap()["/to/file.txt"], b"source");
            }
            assert!(control.fs.lock().unwrap()["/from"].is_empty());
            assert_eq!(control.fs.lock().unwrap()["/to"].len(), items.len());
            assert!(!std::fs::read_dir(area.path()).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".recovery.json")));
        }
    }
}

#[test]
fn ftps_dateibeleg_bleibt_typ_root_und_widerspruchsgebunden() {
    use std::sync::atomic::Ordering;
    let (port, control) = fault_ftp_server(&["source.txt"]);
    control.enable_stou.store(true, Ordering::SeqCst);
    control.rename_noreplace.store(true, Ordering::SeqCst);
    {
        let mut fs = control.fs.lock().unwrap();
        for name in ["a", "b", "folder"] {
            fs.get_mut("/").unwrap().insert(name.into(), true);
            fs.insert(format!("/{name}"), Default::default());
        }
    }
    let (vfs, _area, queue) = owned_vfs(fault_config(port));
    assert!(
        vfs.probe_add_only_capabilities_at(Path::new("/a"))
            .unwrap()
            .atomic_noreplace_rename
    );
    let before = control.mutations.lock().unwrap().clone();
    for (source, target) in [("/source.txt", "/b/file.txt"), ("/folder", "/a/folder")] {
        assert_eq!(
            error_code(
                &vfs.rename_noreplace(Path::new(source), Path::new(target))
                    .unwrap_err()
            ),
            ErrorCode::CapabilityProbeRequired
        );
    }
    assert_eq!(*control.mutations.lock().unwrap(), before);
    let other = vfs.for_queue_preparation(&queue, Path::new("/")).unwrap();
    control.rename_noreplace.store(false, Ordering::SeqCst);
    assert!(
        !other
            .probe_add_only_capabilities_at(Path::new("/a"))
            .unwrap()
            .atomic_noreplace_rename
    );
    // Ein späterer positiver Beleg darf den älteren Gegenbeleg nicht rückwirkend
    // aus einer schon bestätigten Operationssicht entfernen.
    control.rename_noreplace.store(true, Ordering::SeqCst);
    assert!(
        other
            .probe_add_only_capabilities_at(Path::new("/a"))
            .unwrap()
            .atomic_noreplace_rename
    );
    let before = control.mutations.lock().unwrap().clone();
    assert_eq!(
        error_code(
            &vfs.rename_noreplace(Path::new("/source.txt"), Path::new("/a/file.txt"))
                .unwrap_err()
        ),
        ErrorCode::PlanDrift
    );
    assert_eq!(*control.mutations.lock().unwrap(), before);
    // Derselbe Datei-Gegenbeleg sperrt keinen unabhängigen Ordnerbeleg.
    vfs.probe_rename_capability_at(Path::new("/b")).unwrap();
    vfs.rename_noreplace(Path::new("/folder"), Path::new("/b/folder"))
        .unwrap();
    assert!(control.fs.lock().unwrap()["/b"]["folder"]);
    assert!(control.fs.lock().unwrap()["/"].contains_key("source.txt"));
}

#[test]
fn ftps_reiner_move_erhaelt_intent_antwortverlust_und_neustartgrenze() {
    use std::sync::atomic::Ordering;
    for fault in [RntoFault::DropBeforeMutation, RntoFault::DropAfterMutation] {
        let (port, control) = fault_ftp_server(&[]);
        control.enable_stou.store(true, Ordering::SeqCst);
        control.rename_noreplace.store(true, Ordering::SeqCst);
        {
            let mut fs = control.fs.lock().unwrap();
            for name in ["from", "to"] {
                fs.get_mut("/").unwrap().insert(name.into(), true);
                fs.insert(format!("/{name}"), Default::default());
            }
            fs.get_mut("/from")
                .unwrap()
                .insert("file.txt".into(), false);
        }
        control
            .contents
            .lock()
            .unwrap()
            .insert("/from/file.txt".into(), b"source".to_vec());
        let area = private_area();
        let directory = std::fs::canonicalize(area.path()).unwrap();
        let path = directory.join("operations-queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let operation = queue
            .enqueue(fault_move_operation(fault_config(port), &["file.txt"]))
            .unwrap()
            .operation_id;
        let weak = Arc::downgrade(&control);
        let observed_intent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = observed_intent.clone();
        *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
            if command == "RNFR /from/file.txt" {
                let control = weak.upgrade().unwrap();
                *control.fault.lock().unwrap() = Some(fault);
                let intents = std::fs::read_dir(&directory)
                    .unwrap()
                    .filter_map(|entry| {
                        let path = entry.unwrap().path();
                        path.to_string_lossy().ends_with(".recovery.json").then(|| {
                            serde_json::from_slice::<serde_json::Value>(
                                &std::fs::read(path).unwrap(),
                            )
                            .unwrap()
                        })
                    })
                    .collect::<Vec<_>>();
                assert_eq!(intents.len(), 1);
                assert_eq!(intents[0]["owner"]["operation"], operation);
                assert_eq!(intents[0]["ftps"]["phase"], "rename");
                assert_eq!(
                    intents[0]["ftps"]["paths"],
                    serde_json::json!(["/from/file.txt", "/to/file.txt"])
                );
                observed.store(true, Ordering::SeqCst);
            }
        }));
        let result = execute_fault_move(&queue);
        assert!(observed_intent.load(Ordering::SeqCst));
        let error = result.unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        let moved = fault == RntoFault::DropAfterMutation;
        let contents = control.contents.lock().unwrap();
        assert_eq!(contents.contains_key("/from/file.txt"), !moved);
        assert_eq!(contents.contains_key("/to/file.txt"), moved);
        assert_eq!(
            contents
                .values()
                .filter(|body| body.as_slice() == b"source")
                .count(),
            1
        );
        drop(contents);
        let commands = control.mutations.lock().unwrap().clone();
        assert_eq!(
            commands
                .iter()
                .filter(|cmd| cmd.as_str() == "RNTO /to/file.txt")
                .count(),
            1
        );
        assert!(!commands.iter().any(|cmd| cmd.starts_with("DELE /from/")));
        drop(queue);
        let reopened = OperationQueue::open(&path).unwrap();
        assert!(reopened
            .snapshot()
            .entries
            .iter()
            .all(|entry| entry.state != QueueState::Pending));
        assert!(reopened.claim_next().unwrap().is_none());
        assert_eq!(
            *control.mutations.lock().unwrap(),
            commands,
            "Recovery verband oder mutierte den Provider"
        );
    }
}

#[test]
fn ftps_move_quelle_probt_keinen_dateicommit_oder_rename() {
    let (port, control) = fault_ftp_server(&[]);
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: vec![],
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let (vfs, _area, _queue) = owned_vfs(config);
    let endpoint = ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap();
    // Dieser Gegenserver lehnt STOU begrenzt ab. Auch der Fehlerpfad darf keine
    // zusätzliche Ziel- oder Rename-Probe als vermeintliche Hilfe beginnen.
    assert!(endpoint.probe_file_source_removal_capabilities().is_err());
    let mutations = control.mutations.lock().unwrap();
    assert!(mutations
        .iter()
        .any(|command| command.contains(".doppeldecker-delete-probe-")));
    assert!(!mutations
        .iter()
        .any(|command| command.contains(".doppeldecker-probe-")
            || command.contains(".doppeldecker-rename-probe-")
            || command.contains(".doppeldecker-mkdir-probe-")));
    assert_eq!(control.fs.lock().unwrap().len(), 1);
}

#[test]
fn ftps_reine_queuequelle_benoetigt_keinen_privaten_probenordner() {
    use std::os::unix::fs::PermissionsExt;
    let (port, control) = fault_ftp_server(&["source.txt"]);
    let area = private_area();
    let root = std::fs::canonicalize(area.path()).unwrap();
    let config_dir = root.join("config");
    let destination = root.join("target");
    std::fs::create_dir(&destination).unwrap();
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700)).unwrap();
    let queue = OperationQueue::open(config_dir.join("queue.json")).unwrap();
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: vec![],
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let queued = queue
        .enqueue(QueueOperation::ProviderFile {
            kind: FileQueueKind::Copy,
            source: Box::new(BidirectionalQueueEndpoint::Ftps {
                connection: Box::new(config.clone()),
                root: "/".into(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::Local {
                root: destination.to_string_lossy().into_owned(),
            }),
            items: vec!["/source.txt".into()],
            destination: destination.to_string_lossy().into_owned(),
            overwrite: false,
            expected_conflicts: vec![],
            source_tree_expected: None,
            keep_both_targets: vec![],
            expected_conflict_proofs: vec![],
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: Some(false),
            replace_best_effort_expected: Some(false),
            remove_source_best_effort_expected: None,
        })
        .unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    execute_provider_file_queue_entry(
        &queue,
        entry,
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(TestSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        std::fs::read(destination.join("source.txt")).unwrap(),
        b"data"
    );
    assert_eq!(
        queue
            .snapshot()
            .entries
            .iter()
            .find(|entry| entry.operation_id == queued.operation_id)
            .unwrap()
            .state,
        QueueState::Completed
    );
    let vfs = FtpsVfs::connect(config, Arc::new(TestSecrets))
        .unwrap()
        .for_queue_preparation(&queue, Path::new("/"))
        .unwrap();
    assert!(vfs
        .probe_directory_create_capability_at(Path::new("/"))
        .is_err());
    let source = ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap();
    let target = ProviderEndpoint::local(&destination).unwrap();
    let moving = source.interactive_file_contexts(
        &target,
        &[PathBuf::from("/source.txt")],
        true,
        false,
        &Cancel::new(),
    );
    assert_eq!(
        error_code(&moving.err().expect("Move braucht privaten Probenbesitz")),
        ErrorCode::PermissionDenied
    );
    assert!(control.mutations.lock().unwrap().is_empty());
    assert_eq!(
        std::fs::metadata(config_dir).unwrap().permissions().mode() & 0o777,
        0o777
    );
}

#[test]
fn ftps_passiv_gesperrtes_bidirektionales_ziel_startet_keine_probe() {
    let (port, control) = fault_ftp_server(&[]);
    {
        let mut fs = control.fs.lock().unwrap();
        fs.get_mut("/").unwrap().insert("pane".into(), true);
        fs.insert("/pane".into(), Default::default());
    }
    control
        .fail_after_probe_mkdir
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: vec![],
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let vfs = Arc::new(FtpsVfs::connect(config, Arc::new(TestSecrets)).unwrap());
    let area = private_area();
    let local = area.path().join("local");
    std::fs::create_dir(&local).unwrap();
    let baseline = area.path().join("state/pair.json");
    let initial = prepare_provider_bidirectional_with_decisions(
        PairId([116; 16]),
        "Passiv gesperrtes FTPS-Ziel",
        ProviderEndpoint::local(&local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/pane")).unwrap(),
        &baseline,
        &[],
    )
    .unwrap();
    let phrase = initial.preview().confirmation_phrase.clone();
    initial
        .confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    finish_bidirectional_run_completion(&baseline).unwrap();
    std::fs::write(local.join("addition.txt"), b"neuer Inhalt").unwrap();
    let prepared = prepare_provider_bidirectional_with_decisions(
        PairId([116; 16]),
        "Passiv gesperrtes FTPS-Ziel",
        ProviderEndpoint::local(&local).unwrap(),
        ProviderEndpoint::ftps(vfs, Path::new("/pane")).unwrap(),
        &baseline,
        &[],
    )
    .unwrap();
    assert!(prepared
        .preview()
        .plan
        .actions
        .iter()
        .any(|action| matches!(action, doppeldecker_core::BidirectionalAction::Copy { .. })));
    assert!(prepared.preview().stage_blocker.is_some());
    let mutations = control.mutations.lock().unwrap().clone();
    assert!(
        mutations.is_empty(),
        "Die fehlende Quarantäne ist passiv bekannt: {mutations:?}"
    );
    assert!(control.fs.lock().unwrap()["/pane"].is_empty());
}

#[test]
fn ftps_probe_ohne_dauerhaften_owner_bleibt_mutationsfrei() {
    let (port, control) = fault_ftp_server(&[]);
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: vec![],
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let vfs = FtpsVfs::connect(config, Arc::new(TestSecrets)).unwrap();
    let result = vfs.probe_directory_create_capability_at(Path::new("/"));
    assert!(
        result.is_err(),
        "Eine öffentliche ungebundene VFS-Sicht besitzt keinen Probenintent"
    );
    assert!(control.mutations.lock().unwrap().is_empty());
}

#[test]
fn ftps_baseline_und_quelle_bleiben_ohne_mutierende_proben() {
    let (port, control) = fault_ftp_server(&[]);
    {
        let mut fs = control.fs.lock().unwrap();
        fs.get_mut("/").unwrap().insert("pane".into(), true);
        fs.insert("/pane".into(), Default::default());
    }
    // Eine unnötige Probe würde einen Ordner anlegen und die Antwort verlieren.
    // Der leere Plan benötigt nur eine lokale Baseline, keine Remote-Schreibwirkung.
    control
        .fail_after_probe_mkdir
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: Vec::new(),
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let vfs = Arc::new(FtpsVfs::connect(config, Arc::new(TestSecrets)).unwrap());
    let area = private_area();
    let local = area.path().join("local");
    std::fs::create_dir(&local).unwrap();
    let baseline = area.path().join("state/pair.json");
    let prepared = prepare_provider_bidirectional_with_decisions(
        PairId([114; 16]),
        "Leere FTPS-Erstbaseline",
        ProviderEndpoint::local(&local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/pane")).unwrap(),
        &baseline,
        &[],
    )
    .unwrap();
    let mutations = control.mutations.lock().unwrap().clone();
    assert!(
        mutations.is_empty(),
        "Eine reine Erstbaseline darf keine Probe starten: {mutations:?}"
    );
    let phrase = prepared.preview().confirmation_phrase.clone();
    prepared
        .confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    finish_bidirectional_run_completion(&baseline).unwrap();
    assert!(control.fs.lock().unwrap()["/pane"].is_empty());
    assert!(std::fs::read_dir(&local).unwrap().next().is_none());

    control
        .fs
        .lock()
        .unwrap()
        .get_mut("/pane")
        .unwrap()
        .insert("source.txt".into(), false);
    let prepared = prepare_provider_bidirectional_with_decisions(
        PairId([114; 16]),
        "FTPS als reine Quelle",
        ProviderEndpoint::local(&local).unwrap(),
        ProviderEndpoint::ftps(vfs, Path::new("/pane")).unwrap(),
        &baseline,
        &[],
    )
    .unwrap();
    let phrase = prepared.preview().confirmation_phrase.clone();
    prepared
        .confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    finish_bidirectional_run_completion(&baseline).unwrap();
    assert_eq!(std::fs::read(local.join("source.txt")).unwrap(), b"data");
    assert!(control.mutations.lock().unwrap().is_empty());
    assert_eq!(
        control.fs.lock().unwrap()["/pane"]
            .keys()
            .collect::<Vec<_>>(),
        ["source.txt"]
    );
}

#[test]
fn ftps_proben_lesen_beide_roots_auch_nach_fehlern() {
    use std::sync::atomic::Ordering;
    for scenario in [
        "success",
        "mkdir-error",
        "mkdir-partial",
        "profile-error",
        "pane-error",
        "misplaced",
    ] {
        let (port, control) = fault_ftp_server(&["behalten.txt"]);
        {
            let mut fs = control.fs.lock().unwrap();
            fs.get_mut("/").unwrap().insert("pane".into(), true);
            fs.insert("/pane".into(), Default::default());
        }
        let config = FtpsConnectionConfig {
            host: "127.0.0.1".into(),
            alternate_hosts: Vec::new(),
            port,
            username: "anonymous".into(),
            requested_root: "/".into(),
            transport_security: FtpTransportSecurity::Plaintext,
            certificate: None,
            certificate_policy: None,
            password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
        };
        let (vfs, _intent_config, _intent_queue) = owned_vfs(config);
        assert_eq!(vfs.identity().canonical_root, "/");
        control
            .reject_probe_mkdir
            .store(scenario == "mkdir-error", Ordering::SeqCst);
        control
            .fail_after_probe_mkdir
            .store(scenario == "mkdir-partial", Ordering::SeqCst);
        control
            .reject_profile_listing
            .store(scenario == "profile-error", Ordering::SeqCst);
        control
            .reject_pane_readback
            .store(scenario == "pane-error", Ordering::SeqCst);
        control
            .misplace_probe
            .store(scenario == "misplaced", Ordering::SeqCst);
        let result = vfs.probe_directory_create_capability_at(Path::new("/pane"));
        assert_eq!(
            result.is_ok(),
            scenario == "success",
            "{scenario}: {result:?}"
        );
        assert_eq!(
            vfs.capabilities().exclusive_directory_create,
            scenario == "success"
        );
        let listings = control.listings.lock().unwrap();
        assert!(
            listings.iter().filter(|path| *path == "/pane").count() >= 2,
            "{scenario}: {listings:?}"
        );
        assert!(
            listings.iter().any(|path| path == "/"),
            "{scenario}: Profilroot nicht gelesen: {listings:?}; {result:?}"
        );
        let fs = control.fs.lock().unwrap();
        if scenario != "pane-error" {
            assert_eq!(fs["/pane"].is_empty(), scenario != "mkdir-partial");
        }
        if scenario == "mkdir-partial" {
            assert_eq!(
                error_code(result.as_ref().unwrap_err()),
                ErrorCode::OutcomeUnclear
            );
        }
        assert!(fs["/"].contains_key("behalten.txt"));
        if scenario == "misplaced" {
            assert!(fs["/"]
                .keys()
                .any(|name| name.starts_with(".doppeldecker-")));
            assert!(result.unwrap_err().to_string().contains("Profilroot"));
        }
    }
}

#[test]
fn ftps_proben_bleiben_parallel_und_alte_antworten_geben_nichts_frei() {
    let (port, control) = fault_ftp_server(&["behalten.txt"]);
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: Vec::new(),
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let (vfs, _intent_config, _intent_queue) = owned_vfs(config);
    let vfs = Arc::new(vfs);
    let (started, started_rx) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    *control.hold_first_probe.lock().unwrap() = Some((started, release_rx));
    let first_vfs = vfs.clone();
    let first =
        std::thread::spawn(move || first_vfs.probe_directory_create_capability_at(Path::new("/")));
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let second_vfs = vfs.clone();
    let (completed, completed_rx) = std::sync::mpsc::channel();
    let second = std::thread::spawn(move || {
        completed
            .send(second_vfs.probe_directory_create_capability_at(Path::new("/")))
            .unwrap()
    });
    let before_release = completed_rx.recv_timeout(Duration::from_secs(5));
    release.send(()).unwrap();
    let old_result = first.join().unwrap();
    second.join().unwrap();
    assert!(
        before_release.is_ok(),
        "Zweite Probe wartete auf die ältere Kontrollantwort"
    );
    assert!(before_release.unwrap().unwrap().exclusive_directory_create);
    assert_eq!(error_code(&old_result.unwrap_err()), ErrorCode::Interrupted);
    assert!(vfs.capabilities().exclusive_directory_create);
    let fs = control.fs.lock().unwrap();
    assert_eq!(fs["/"].len(), 1);
    assert!(fs["/"].contains_key("behalten.txt"));
}

#[test]
fn ftps_getrennte_pane_roots_behalten_ihre_parallelen_proben() {
    use std::sync::atomic::Ordering;
    for (first_root, second_root, rename, independent, reconnect, shared_view) in [
        ("/first", "/second", false, true, false, false),
        ("/pane", "/pane-other", true, true, false, false),
        ("/pane", "/pane/nested", false, false, false, false),
        ("/pane/nested", "/pane", false, false, false, false),
        ("/pane", "/pane", true, false, false, false),
        ("/first", "/second", false, false, true, false),
        ("/first", "/second", false, false, false, true),
    ] {
        let (port, control) = fault_ftp_server(&["behalten.txt"]);
        {
            let mut fs = control.fs.lock().unwrap();
            for root in [first_root, second_root] {
                let mut parent = String::from("/");
                for name in root.trim_start_matches('/').split('/') {
                    fs.get_mut(&parent).unwrap().insert(name.into(), true);
                    parent = format!("{}/{name}", parent.trim_end_matches('/'));
                    fs.entry(parent.clone()).or_default();
                }
            }
        }
        if rename {
            let mut fs = control.fs.lock().unwrap();
            for root in [first_root, second_root] {
                fs.get_mut(root).unwrap().insert("user-dir".into(), true);
                fs.insert(format!("{root}/user-dir"), Default::default());
            }
        }
        let original = control.fs.lock().unwrap().clone();
        let config = FtpsConnectionConfig {
            host: "127.0.0.1".into(),
            alternate_hosts: Vec::new(),
            port,
            username: "anonymous".into(),
            requested_root: "/".into(),
            transport_security: FtpTransportSecurity::Plaintext,
            certificate: None,
            certificate_policy: None,
            password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
        };
        let area = private_area();
        let directory = std::fs::canonicalize(area.path()).unwrap();
        let first_path = directory.join("operations-queue-window-first.json");
        let process_lock =
            Arc::new(doppeldecker_core::OperationQueueProcessLock::acquire(&first_path).unwrap());
        let first_queue =
            OperationQueue::open_with_process_lock(&first_path, process_lock.clone()).unwrap();
        let second_queue = OperationQueue::open_with_process_lock(
            directory.join("operations-queue-window-second.json"),
            process_lock,
        )
        .unwrap();
        let shared = FtpsVfs::connect(config, Arc::new(TestSecrets)).unwrap();
        let first_vfs = shared
            .for_queue_preparation(
                &first_queue,
                Path::new(if shared_view { "/" } else { first_root }),
            )
            .unwrap();
        let second_vfs = if shared_view {
            first_vfs.clone()
        } else {
            shared
                .for_queue_preparation(&second_queue, Path::new(second_root))
                .unwrap()
        };
        let first_view = first_vfs.clone();
        let second_view = second_vfs.clone();
        let probe = move |vfs: &FtpsVfs, root: &Path| -> std::io::Result<(bool, bool, bool)> {
            if rename {
                let endpoint = ProviderEndpoint::ftps(Arc::new(vfs.clone()), root)?;
                let capabilities =
                    endpoint.probe_rename_capabilities_for_item(&root.join("user-dir"))?;
                Ok((false, capabilities.assured, capabilities.best_effort))
            } else {
                Ok((
                    vfs.probe_directory_create_capability_at(root)?
                        .exclusive_directory_create,
                    false,
                    false,
                ))
            }
        };
        let (started, started_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        *control.hold_first_probe.lock().unwrap() = Some((started, release_rx));
        let first = std::thread::spawn(move || probe(&first_vfs, Path::new(first_root)));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (completed, completed_rx) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            completed
                .send(probe(&second_vfs, Path::new(second_root)))
                .unwrap()
        });
        let second_result = completed_rx.recv_timeout(Duration::from_secs(5));
        if reconnect {
            shared.reconnect().unwrap();
        }
        // Die spätere Antwort von A belegt eine andere Renameklasse als B.
        // Bei getrennten Roots darf sie nur A freigeben und Bs Cache nicht ersetzen.
        control.rename_noreplace.store(true, Ordering::SeqCst);
        release.send(()).unwrap();
        let first_result = first.join().unwrap();
        second.join().unwrap();
        let second_result = second_result
            .expect("Zweite Rootprobe wurde blockiert")
            .unwrap();
        if rename {
            assert!(second_result.2);
            assert!(!second_result.1);
            assert!(!second_view.capabilities().atomic_noreplace_rename);
            assert!(!shared.capabilities().atomic_noreplace_rename);
        } else {
            assert!(second_result.0);
            assert_eq!(
                second_view.capabilities().exclusive_directory_create,
                !reconnect
            );
        }
        if independent {
            let first_result = first_result.unwrap_or_else(|error| {
                panic!("Unabhängige Probe {first_root} nach {second_root}: {error}")
            });
            if rename {
                assert!(first_result.1);
                assert!(
                    !first_view.capabilities().atomic_noreplace_rename,
                    "Ordnerprobe darf keinen Datei-Commit freigeben"
                );
            } else {
                assert!(first_result.0);
            }
        } else {
            assert_eq!(
                error_code(&first_result.unwrap_err()),
                ErrorCode::Interrupted
            );
            assert!(!first_view.capabilities().atomic_noreplace_rename);
            assert_eq!(
                first_view.capabilities().exclusive_directory_create,
                shared_view
            );
        }
        assert_eq!(*control.fs.lock().unwrap(), original);
        assert!(!std::fs::read_dir(directory).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".recovery.json")));
    }
}

#[test]
fn ftps_dateifahigkeiten_erhalten_den_fehler_der_loeschteilprobe() {
    let (port, control) = fault_ftp_server(&["behalten.txt"]);
    control
        .fail_after_delete_probe_mkdir
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: Vec::new(),
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let (vfs, _intent_config, _intent_queue) = owned_vfs(config);
    let vfs = Arc::new(vfs);
    let endpoint = ProviderEndpoint::ftps(vfs.clone(), Path::new("/")).unwrap();
    let result = endpoint.probe_file_capabilities();
    assert!(
        result.is_err(),
        "Unklarer Teilprobenausgang ging verloren: {result:?}"
    );
    assert_eq!(error_code(&result.unwrap_err()), ErrorCode::OutcomeUnclear);
    assert!(!vfs.capabilities().best_effort_file_replace);
    let mutations = control.mutations.lock().unwrap();
    assert_eq!(
        mutations
            .iter()
            .filter(|cmd| cmd.contains(".doppeldecker-delete-probe-"))
            .count(),
        1
    );
    assert!(!mutations
        .iter()
        .any(|cmd| cmd.starts_with("MKD /.doppeldecker-probe-")));
    let fs = control.fs.lock().unwrap();
    assert!(fs["/"].contains_key("behalten.txt"));
    assert!(fs["/"]
        .keys()
        .any(|name| name.starts_with(".doppeldecker-delete-probe-")));
}

#[test]
fn verlorene_rnto_antworten_werden_geklaert_und_nie_wiederholt() {
    let (port, control) = fault_ftp_server(&["alt.txt"]);
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: Vec::new(),
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let (vfs, _intent_config, _intent_queue) = owned_vfs(config);
    let vfs = Arc::new(vfs);
    let endpoint = ProviderEndpoint::ftps(vfs.clone(), Path::new("/")).unwrap();

    // Der Server ersetzt bei RNTO vorhandene Ziele: Die Probe stuft Rename als
    // Best Effort ein, nicht als atomar abgesichert.
    let capabilities = endpoint.probe_rename_capabilities().unwrap();
    assert!(!capabilities.assured);
    assert!(capabilities.best_effort);

    // Fall 1: Antwortverlust NACH der serverseitigen Mutation. Die Mutation wird
    // nie wiederholt; ein identitätsgeprüfter Reconnect plus genau ein Listing
    // belegen den Erfolg.
    *control.fault.lock().unwrap() = Some(RntoFault::DropAfterMutation);
    let report = endpoint
        .execute_interactive_rename(Path::new("/alt.txt"), "neu.txt", &None)
        .unwrap();
    assert_eq!(report.outcome, RenameOutcome::Renamed);
    assert!(report.verified_after_reconnect);
    {
        let fs = control.fs.lock().unwrap();
        let root_entries = fs.get("/").unwrap();
        assert!(root_entries.contains_key("neu.txt"));
        assert!(!root_entries.contains_key("alt.txt"));
    }

    // Fall 2: Antwortverlust VOR der serverseitigen Mutation. Das klärende Listing
    // belegt den unveränderten Bestand; der stabile Code bleibt connection_lost.
    *control.fault.lock().unwrap() = Some(RntoFault::DropBeforeMutation);
    let endpoint = _intent_queue.bind_provider_preparation(&endpoint).unwrap();
    let unchanged = endpoint
        .execute_interactive_rename(Path::new("/neu.txt"), "wieder.txt", &None)
        .unwrap_err();
    assert_eq!(error_code(&unchanged), ErrorCode::ConnectionLost);
    {
        let fs = control.fs.lock().unwrap();
        let root_entries = fs.get("/").unwrap();
        assert!(root_entries.contains_key("neu.txt"));
        assert!(!root_entries.contains_key("wieder.txt"));
    }

    // Fall 3: Nach dem Antwortverlust meldet sich ein Server mit anderer Identität.
    // Die Zustandsklärung darf dort nicht weiterlesen: Ausgang bleibt ehrlich unklar.
    control
        .swap_identity_after_fault
        .store(true, std::sync::atomic::Ordering::SeqCst);
    *control.fault.lock().unwrap() = Some(RntoFault::DropAfterMutation);
    let endpoint = _intent_queue.bind_provider_preparation(&endpoint).unwrap();
    let unclear = endpoint
        .execute_interactive_rename(Path::new("/neu.txt"), "ende.txt", &None)
        .unwrap_err();
    assert_eq!(error_code(&unclear), ErrorCode::OutcomeUnclear);
    // Trotz unklarem Ausgang lief die Mutation serverseitig genau einmal — der
    // Bestand belegt, dass keine blinde Wiederholung stattfand.
    {
        let fs = control.fs.lock().unwrap();
        let root_entries = fs.get("/").unwrap();
        assert!(root_entries.contains_key("ende.txt"));
        assert!(!root_entries.contains_key("neu.txt"));
    }
}

#[test]
fn ftp_waehlt_mlst_fakten_und_listet_das_aktuelle_verzeichnis() {
    let (port, server) = strict_mlsd_server();
    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: Vec::new(),
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };

    let (vfs, _intent_config, _intent_queue) = owned_vfs(config);
    let listing = vfs.list_complete(Path::new("/")).unwrap();
    assert!(listing.complete);
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].entry.name, "test.txt");
    assert_eq!(listing.entries[0].entry.size, 12);
    server.join().unwrap();
}

#[test]
fn ftps_pin_mlsd_stream_und_reconnect_bleiben_gebunden() {
    let root = private_area();
    std::fs::write(root.path().join("inhalt.txt"), b"FTPS Testinhalt\n").unwrap();
    std::fs::create_dir(root.path().join("ordner")).unwrap();

    let certified = generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der = certified.cert.der().clone();
    let key_der = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .unwrap();

    let port = free_port();
    let address = format!("127.0.0.1:{port}");
    let server_root = root.path().to_path_buf();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let server = ServerBuilder::new(Box::new(move || {
                Filesystem::new(server_root.clone()).unwrap()
            }))
            .ftps_manual::<PathBuf>(Arc::new(server_tls))
            .ftps_required(FtpsRequired::All, FtpsRequired::All)
            .build()
            .unwrap();
            server.listen(address).await.unwrap();
        });
    });
    wait_for_server(port);

    let pin = discover_ftps_certificate("localhost", port).unwrap();
    let config = FtpsConnectionConfig {
        host: "localhost".into(),
        alternate_hosts: Vec::new(),
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::ExplicitTls,
        certificate: Some(pin),
        certificate_policy: Some(FtpsCertificatePolicy::PinnedCertificate),
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftps").unwrap(),
    };
    let mut wrong_pin = config.clone();
    wrong_pin.certificate.as_mut().unwrap().sha256[0] ^= 0xff;
    let pin_error = FtpsVfs::connect(wrong_pin, Arc::new(TestSecrets))
        .err()
        .unwrap();
    assert_ne!(error_code(&pin_error), ErrorCode::TlsIdentityUnverified);
    let mut untrusted_public = config.clone();
    untrusted_public.certificate_policy = Some(FtpsCertificatePolicy::WebPkiAndPin);
    let public_identity_error = FtpsVfs::connect(untrusted_public, Arc::new(TestSecrets))
        .err()
        .unwrap();
    assert_eq!(
        error_code(&public_identity_error),
        ErrorCode::TlsIdentityUnverified
    );
    let mut ip_unverified = config.clone();
    ip_unverified.host = "127.0.0.1".into();
    ip_unverified.certificate_policy = Some(FtpsCertificatePolicy::UnverifiedCertificate);
    let ip_vfs = FtpsVfs::connect(ip_unverified, Arc::new(TestSecrets)).unwrap();
    assert_eq!(
        ip_vfs.security_status(),
        FtpSecurityStatus::TlsIdentityUnverified
    );
    drop(ip_vfs);
    let (vfs, _intent_config, _intent_queue) = owned_vfs(config.clone());
    assert_eq!(
        vfs.security_status(),
        FtpSecurityStatus::TlsIdentityExternallyVerified
    );
    assert!(!vfs.supports_mode_write());
    assert!(!vfs.supports_mtime_write());
    let listing = vfs.list_complete(Path::new("/")).unwrap();
    assert!(listing.complete);
    assert!(listing.provider_token.is_some());
    assert_eq!(listing.entries.len(), 2);
    assert!(listing
        .entries
        .iter()
        .any(|entry| entry.entry.name == "inhalt.txt" && entry.entry.is_file));

    let mut bytes = Vec::new();
    vfs.open_read(Path::new("/inhalt.txt"))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"FTPS Testinhalt\n");

    // Capability-Proben gehören unter den konkret gebundenen Pane-Root. Weder bei
    // Erfolg noch bei einer absichtlich erkannten Rename-Kollision darf kurzlebiger
    // Probeabfall neben diesem Root entstehen.
    std::fs::create_dir(root.path().join("scoped-probes")).unwrap();
    let generation = vfs.session_generation();
    vfs.reconnect().unwrap();
    assert_eq!(vfs.session_generation(), generation + 1);
    assert_eq!(vfs.identity().certificate, config.certificate);
    assert!(!vfs.capabilities().permits_directory_create());
    let missing_probe = vfs.open_write_new(Path::new("/gesperrt")).err().unwrap();
    assert_eq!(
        error_code(&missing_probe),
        ErrorCode::CapabilityProbeRequired
    );
    assert_eq!(
        error_code(&vfs.open_write(Path::new("/inhalt.txt")).err().unwrap()),
        ErrorCode::Unsupported
    );
    assert!(!root.path().join("gesperrt").exists());

    assert!(vfs
        .probe_directory_create_capability_at(Path::new("/scoped-probes"))
        .is_err());
    let vfs = vfs
        .for_queue_preparation(&_intent_queue, Path::new("/"))
        .unwrap();
    let mkdir_capabilities = vfs
        .probe_directory_create_capability_at(Path::new("/scoped-probes"))
        .unwrap();
    assert!(mkdir_capabilities.permits_directory_create());
    assert!(!mkdir_capabilities.permits_add_only());
    assert_eq!(
        std::fs::read_dir(root.path().join("scoped-probes"))
            .unwrap()
            .count(),
        0
    );
    assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".doppeldecker-")));
    vfs.create_dir(Path::new("/mkdir-e2e")).unwrap();
    assert!(vfs.create_dir(Path::new("/mkdir-e2e")).is_err());
    assert!(vfs.stat(Path::new("/mkdir-e2e")).unwrap().is_dir);
    std::fs::remove_dir(root.path().join("mkdir-e2e")).unwrap();

    vfs.reconnect().unwrap();
    assert!(!vfs.capabilities().permits_directory_create());
    assert!(vfs.create_dir(Path::new("/mkdir-ohne-reprobe")).is_err());
    let vfs = vfs
        .for_queue_preparation(&_intent_queue, Path::new("/"))
        .unwrap();
    vfs.probe_directory_create_capability_at(Path::new("/scoped-probes"))
        .unwrap();
    // Der lokale Testserver verwendet POSIX-Rename und ersetzt vorhandene Ziele.
    // Die zweistufige Probe scheitert daran nicht mehr: Sie belegt das gewöhnliche
    // Rename samt Readback (Best Effort) und verweigert nur den No-Replace-Beweis.
    let rename_capabilities = vfs
        .probe_rename_capability_at(Path::new("/scoped-probes"))
        .unwrap();
    assert!(rename_capabilities.best_effort_rename);
    assert!(!rename_capabilities.atomic_noreplace_rename);
    assert!(!vfs.capabilities().atomic_noreplace_rename);
    // Der Sync-taugliche No-Replace-Weg bleibt ohne Beweis weiterhin gesperrt.
    assert!(vfs
        .rename_noreplace(Path::new("/inhalt.txt"), Path::new("/anders.txt"))
        .is_err());
    assert_eq!(
        std::fs::read(root.path().join("inhalt.txt")).unwrap(),
        b"FTPS Testinhalt\n"
    );
    assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".doppeldecker-rename-probe-")));

    // Der lokale Testserver verwendet POSIX-Rename und ersetzt vorhandene Ziele.
    // Die zweistufige Probe erkennt das, entfernt ihren künstlichen Ordner
    // vollständig und liefert die ehrliche Abstufung: kein AddOnly-Beweis, aber
    // der sichtbare Best-Effort-Commit für neue Dateien ist belegt.
    let add_only = vfs
        .probe_add_only_capabilities_at(Path::new("/scoped-probes"))
        .unwrap();
    assert!(!add_only.permits_add_only());
    assert!(!add_only.exclusive_create);
    assert!(!add_only.atomic_noreplace_rename);
    assert!(add_only.best_effort_file_commit);
    assert!(add_only.best_effort_file_replace);
    let (file_vfs, _file_intent_config, _file_intent_queue) = owned_vfs(config.clone());
    let file_vfs = Arc::new(file_vfs);
    let file_capabilities = ProviderEndpoint::ftps(file_vfs, Path::new("/scoped-probes"))
        .unwrap()
        .probe_file_capabilities()
        .unwrap();
    assert!(!file_capabilities.replace);
    assert!(file_capabilities.replace_best_effort);
    assert_eq!(
        std::fs::read(root.path().join("inhalt.txt")).unwrap(),
        b"FTPS Testinhalt\n"
    );
    assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".doppeldecker-probe-")));

    // Upload lokal→FTPS läuft jetzt als Best-Effort-Commit: Temp per STOU, Hash-
    // Readback, frisches Elternlisting, genau ein Rename, Listing-Readback.
    // Die vorherige Inhaltsprobe galt nur für /scoped-probes. Der folgende
    // Upload nach / benötigt seinen eigenen Beleg am tatsächlichen Zielroot.
    vfs.probe_add_only_capabilities_at(Path::new("/")).unwrap();
    let upload_source = private_area();
    // macOS-Tempdirs liegen hinter dem /var-Symlink; der descriptor-relative
    // lokale Walk verlangt den kanonischen Pfad.
    let upload_root = std::fs::canonicalize(upload_source.path()).unwrap();
    std::fs::write(upload_root.join("upload-e2e.txt"), b"best effort\n").unwrap();
    let local_src = doppeldecker_core::LocalVfs::new();
    let mut upload_progress = |_: &doppeldecker_core::Progress| {};
    let stats = doppeldecker_core::fileops::copy_items(
        doppeldecker_core::fileops::FileOperationRequest {
            src: &local_src,
            dst: &vfs,
            items: &[upload_root.join("upload-e2e.txt")],
            dst_dir: Path::new("/"),
            policy: doppeldecker_core::fileops::OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut upload_progress,
    )
    .unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(
        std::fs::read(root.path().join("upload-e2e.txt")).unwrap(),
        b"best effort\n"
    );
    // Ein bereits vorhandener Zielname wird übersprungen, nie still ersetzt.
    std::fs::write(upload_root.join("inhalt.txt"), b"anders\n").unwrap();
    let skipped = doppeldecker_core::fileops::copy_items(
        doppeldecker_core::fileops::FileOperationRequest {
            src: &local_src,
            dst: &vfs,
            items: &[upload_root.join("inhalt.txt")],
            dst_dir: Path::new("/"),
            policy: doppeldecker_core::fileops::OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut upload_progress,
    )
    .unwrap();
    assert_eq!(skipped.files, 0);
    assert_eq!(skipped.skipped, 1);
    assert_eq!(
        std::fs::read(root.path().join("inhalt.txt")).unwrap(),
        b"FTPS Testinhalt\n"
    );

    // Dieselbe Probe trägt ausschließlich die ausdrücklich bestätigte Ersetzung
    // einer regulären Datei. Die Altversion wird nicht geparkt; der Core prüft den
    // Bestand frisch, führt RNFR/RNTO genau einmal aus und liest den Inhalts-Hash zurück.
    std::fs::write(root.path().join("replace-e2e.txt"), b"alte FTPS-Version\n").unwrap();
    std::fs::write(
        upload_root.join("replace-e2e.txt"),
        b"neue FTPS-Version mit anderer Laenge\n",
    )
    .unwrap();
    let replaced = doppeldecker_core::fileops::copy_items(
        doppeldecker_core::fileops::FileOperationRequest {
            src: &local_src,
            dst: &vfs,
            items: &[upload_root.join("replace-e2e.txt")],
            dst_dir: Path::new("/"),
            policy: doppeldecker_core::fileops::OverwritePolicy::Overwrite,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut upload_progress,
    )
    .unwrap();
    assert_eq!(replaced.files, 1);
    assert_eq!(
        std::fs::read(root.path().join("replace-e2e.txt")).unwrap(),
        b"neue FTPS-Version mit anderer Laenge\n"
    );
    std::fs::remove_file(root.path().join("replace-e2e.txt")).unwrap();
    std::fs::remove_file(root.path().join("upload-e2e.txt")).unwrap();

    // Der Server kann trotz unsicherer RNTO-Replace-Semantik gezieltes DELE/RMD
    // beweisen. Diese Fähigkeit trägt ausschließlich interne Probe-Cleanups und
    // den sichtbar begrenzten Quellentfernungsweg interaktiver Moves: FTP besitzt
    // keine Bedingung, die den zuvor gelesenen Objekt-Token atomar bis in DELE/RMD
    // trägt. Der Provider-Endpunkt darf sie deshalb nicht als dauerhaftes
    // Benutzer-Löschen anbieten und dabei auch keine weitere Probe mutieren.
    let delete_capabilities = vfs
        .probe_permanent_delete_capability_at(Path::new("/scoped-probes"))
        .unwrap();
    assert!(delete_capabilities.permanent_delete);
    assert_eq!(
        std::fs::read_dir(root.path().join("scoped-probes"))
            .unwrap()
            .count(),
        0
    );
    let names_before_endpoint_delete_probe = std::fs::read_dir(root.path().join("scoped-probes"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<std::collections::BTreeSet<_>>();
    let vfs = Arc::new(vfs);
    let endpoint_delete_capabilities =
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/scoped-probes"))
            .unwrap()
            .probe_delete_capabilities()
            .unwrap();
    assert!(!endpoint_delete_capabilities.quarantine);
    assert!(!endpoint_delete_capabilities.permanent);
    assert!(endpoint_delete_capabilities
        .blockers
        .iter()
        .any(|blocker| blocker.contains("DELE/RMD")));
    let names_after_endpoint_delete_probe = std::fs::read_dir(root.path().join("scoped-probes"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        names_before_endpoint_delete_probe, names_after_endpoint_delete_probe,
        "der gesperrte Nutzer-Löschpfad darf keine Capability-Probe mutieren"
    );
    std::fs::create_dir(root.path().join("delete-e2e")).unwrap();
    std::fs::write(root.path().join("delete-e2e/eigen.txt"), b"nur test").unwrap();
    vfs.remove_dir_all(Path::new("/delete-e2e")).unwrap();
    assert!(!root.path().join("delete-e2e").exists());

    let download_area = private_area();
    let download_target = download_area.path().join("ziel");
    let selection_target = download_area.path().join("auswahl");
    std::fs::create_dir(&download_target).unwrap();
    std::fs::create_dir(&selection_target).unwrap();

    // Interaktives F2-Umbenennen läuft auf diesem Server als sichtbarer Best-Effort-
    // Weg: frisches Elternlisting, sichtbare Kollision, genau ein RNFR/RNTO und
    // anschließender Listing-Readback — ohne Abhängigkeit von der No-Replace-Probe.
    std::fs::write(root.path().join("umbenennen.txt"), b"rename me\n").unwrap();
    let rename_endpoint = ProviderEndpoint::ftps(vfs.clone(), Path::new("/")).unwrap();
    let rename_caps = rename_endpoint.probe_rename_capabilities().unwrap();
    assert!(!rename_caps.assured);
    assert!(rename_caps.best_effort);
    // Reihenfolge ist Vertrag: Die klassenspezifische Einschränkung (Kollisionsfenster
    // beim Umbenennen) steht vorn, weil GUI und Vorschau nur den ersten Eintrag zeigen; die
    // klassenunabhängige Pfadbindung der Quelle folgt dahinter.
    assert!(rename_caps.limitations[0].contains("Kollisionsfenster"));
    assert_eq!(
        rename_caps.limitations.last().map(String::as_str),
        Some(doppeldecker_core::SOURCE_PATH_BINDING_LIMITATION)
    );
    let report = rename_endpoint
        .execute_interactive_rename(Path::new("/umbenennen.txt"), "umbenannt.txt", &None)
        .unwrap();
    assert_eq!(report.outcome, RenameOutcome::Renamed);
    assert_eq!(report.class, RenameExecutionClass::BestEffort);
    assert!(!report.verified_after_reconnect);
    assert_eq!(
        report.new_path.as_deref(),
        Some(Path::new("/umbenannt.txt"))
    );
    assert_eq!(
        std::fs::read(root.path().join("umbenannt.txt")).unwrap(),
        b"rename me\n"
    );
    assert!(!root.path().join("umbenennen.txt").exists());
    // Eine im frischen Listing sichtbare Kollision verhindert die Mutation,
    // statt das vorhandene Ziel über das überschreibende RNTO zu ersetzen.
    std::fs::write(root.path().join("kollision.txt"), b"anders\n").unwrap();
    let collision = rename_endpoint
        .execute_interactive_rename(Path::new("/umbenannt.txt"), "kollision.txt", &None)
        .unwrap();
    assert_eq!(collision.outcome, RenameOutcome::Collision);
    assert_eq!(collision.existing_is_dir, Some(false));
    assert_eq!(
        std::fs::read(root.path().join("kollision.txt")).unwrap(),
        b"anders\n"
    );
    assert_eq!(
        std::fs::read(root.path().join("umbenannt.txt")).unwrap(),
        b"rename me\n"
    );
    std::fs::remove_file(root.path().join("umbenannt.txt")).unwrap();
    std::fs::remove_file(root.path().join("kollision.txt")).unwrap();

    // Auswahlbezogenes F5/F6 verwendet nicht den gesamten Remote-Root. Selbst wenn
    // dieser Server keinen sicheren Upload-Commit beweist, bleiben Download und die
    // separat geprüfte Quellentfernung für genau diese Richtung verfügbar.
    std::fs::write(root.path().join("selected-copy.txt"), b"auswahl copy\n").unwrap();
    std::fs::write(root.path().join("selected-move.txt"), b"auswahl move\n").unwrap();
    let selection_queue_dir = private_area();
    let selection_queue =
        OperationQueue::open(selection_queue_dir.path().join("queue.json")).unwrap();
    for (name, kind) in [
        ("selected-copy.txt", FileQueueKind::Copy),
        ("selected-move.txt", FileQueueKind::Move),
    ] {
        let queued = selection_queue
            .enqueue(QueueOperation::ProviderFile {
                kind,
                source: Box::new(BidirectionalQueueEndpoint::Ftps {
                    connection: Box::new(config.clone()),
                    root: "/".into(),
                }),
                target: Box::new(BidirectionalQueueEndpoint::Local {
                    root: selection_target.to_string_lossy().into_owned(),
                }),
                items: vec![format!("/{name}")],
                destination: selection_target.to_string_lossy().into_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                source_tree_expected: None,
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                best_effort_commit_expected: None,
                local_copy_new_best_effort_expected: None,
                copy_new_best_effort_expected: Some(false),
                replace_best_effort_expected: Some(false),
                remove_source_best_effort_expected: (kind == FileQueueKind::Move).then_some(true),
            })
            .unwrap();
        let claimed = selection_queue.claim_next().unwrap().unwrap();
        let execution = execute_provider_file_queue_entry(
            &selection_queue,
            claimed,
            &LocalQueueExecution {
                sandbox: None,
                sync_quarantine: None,
                max_delete_entries: 100,
                max_delete_fraction: 0.25,
            },
            Arc::new(TestSecrets),
            &Cancel::new(),
            &mut |_| {},
        );
        let result = execution.unwrap();
        assert!(matches!(result, QueueExecutionResult::File(_)));
        let snapshot = selection_queue.snapshot();
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.operation_id == queued.operation_id)
            .unwrap();
        assert_eq!(entry.state, QueueState::Completed);
        assert_eq!(
            std::fs::read(selection_target.join(name)).unwrap(),
            if kind == FileQueueKind::Copy {
                b"auswahl copy\n".as_slice()
            } else {
                b"auswahl move\n".as_slice()
            }
        );
    }
    assert!(root.path().join("selected-copy.txt").is_file());
    assert!(!root.path().join("selected-move.txt").exists());

    let prepared = prepare_ftps_download(
        vfs.clone(),
        Path::new("/"),
        &download_target,
        SyncOptions::default(),
    )
    .unwrap();
    let phrase = prepared.preview().confirmation_phrase.clone();
    prepared
        .confirm(&phrase, false)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(
        std::fs::read(download_target.join("inhalt.txt")).unwrap(),
        b"FTPS Testinhalt\n"
    );

    std::fs::write(root.path().join("inhalt.txt"), b"FTPS aktualisiert\n").unwrap();
    let update = prepare_ftps_download(
        vfs.clone(),
        Path::new("/"),
        &download_target,
        SyncOptions {
            mode: SyncMode::Update,
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        },
    )
    .unwrap();
    let phrase = update.preview().confirmation_phrase.clone();
    update
        .confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(
        std::fs::read(download_target.join("inhalt.txt")).unwrap(),
        b"FTPS aktualisiert\n"
    );

    for index in 0..20 {
        std::fs::write(
            root.path().join(format!("datei-{index:02}.txt")),
            b"mirror\n",
        )
        .unwrap();
    }
    let fill = prepare_ftps_download(
        vfs.clone(),
        Path::new("/"),
        &download_target,
        SyncOptions::default(),
    )
    .unwrap();
    let phrase = fill.preview().confirmation_phrase.clone();
    fill.confirm(&phrase, false)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    std::fs::write(download_target.join("ueberzaehlig.txt"), b"weg\n").unwrap();
    let mirror = prepare_ftps_download(
        vfs.clone(),
        Path::new("/"),
        &download_target,
        SyncOptions {
            mode: SyncMode::Mirror,
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        },
    )
    .unwrap();
    let phrase = mirror.preview().confirmation_phrase.clone();
    mirror
        .confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert!(!download_target.join("ueberzaehlig.txt").exists());

    // FTPS bleibt im bidirektionalen Modus reine Quelle: Der lokale Zielvertrag
    // übernimmt Addition, Quarantäne und Recovery, ohne eine nicht vorhandene
    // FTPS-Zielgarantie vorzutäuschen.
    std::fs::create_dir(root.path().join("bidirektional")).unwrap();
    let bidi_local = download_area.path().join("bidirektional-lokal");
    std::fs::create_dir(&bidi_local).unwrap();
    let bidi_baseline = download_area.path().join("state/ftps-pair.json");
    let pair = PairId([110; 16]);
    let capabilities_before = vfs.capabilities();
    let initial = prepare_provider_bidirectional_with_decisions(
        pair,
        "FTPS-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[],
    )
    .unwrap();
    let prepared_plan = format!("{:?}", initial.preview().plan);
    let capabilities_after = vfs.capabilities();
    let phrase = initial.preview().confirmation_phrase.clone();
    initial
        .confirm(&phrase, true)
        .unwrap_or_else(|error| {
            // Keine zusätzliche Providerabfrage im erfolgreichen Prüfweg: Erst
            // nach der Driftabsage die frischen Belege sichern, damit der nächste
            // Fehler Root-/Sitzungswechsel und Inhaltsdrift unterscheiden lässt.
            use doppeldecker_core::{build_tree_snapshot, LocalVfs, MetadataProfile, SnapshotLimits};
            let local_snapshot = LocalVfs::rooted(bidi_local.parent().unwrap()).and_then(|local| {
                build_tree_snapshot(&local, &bidi_local, MetadataProfile::Portable, SnapshotLimits::default())
            });
            panic!(
                "Leere FTPS-Erstbaseline: {error}\nVorher: {prepared_plan}\nCapabilities vor/nach Vorbereitung: {capabilities_before:?} / {capabilities_after:?}\nFrisch lokal: {:?}\nFrisch remote: {:?}\nCapabilities frisch: {:?}\nMetadaten frisch: {:?}",
                local_snapshot,
                build_tree_snapshot(vfs.as_ref(), Path::new("/bidirektional"), MetadataProfile::Portable, SnapshotLimits::default()),
                vfs.capabilities(),
                vfs.metadata_capabilities(Path::new("/bidirektional")),
            )
        })
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    finish_bidirectional_run_completion(&bidi_baseline).unwrap();
    std::fs::write(
        root.path().join("bidirektional/remote.txt"),
        b"FTPS bidirektional\n",
    )
    .unwrap();
    let remote_to_local = prepare_provider_bidirectional_with_decisions(
        pair,
        "FTPS-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[],
    )
    .unwrap();
    assert!(remote_to_local.preview().stage_blocker.is_none());
    let phrase = remote_to_local.preview().confirmation_phrase.clone();
    remote_to_local
        .confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    finish_bidirectional_run_completion(&bidi_baseline).unwrap();
    assert_eq!(
        std::fs::read(bidi_local.join("remote.txt")).unwrap(),
        b"FTPS bidirektional\n"
    );
    std::fs::write(
        root.path().join("bidirektional/remote.txt"),
        b"FTPS bidirektional aktualisiert\n",
    )
    .unwrap();
    let replacement = prepare_provider_bidirectional_with_decisions(
        pair,
        "FTPS-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[],
    )
    .unwrap();
    let phrase = replacement.preview().confirmation_phrase.clone();
    replacement
        .confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    finish_bidirectional_run_completion(&bidi_baseline).unwrap();
    for index in 0..24 {
        std::fs::write(
            root.path()
                .join(format!("bidirektional/bestand-{index:02}.txt")),
            b"gleich\n",
        )
        .unwrap();
    }
    let fill = prepare_provider_bidirectional_with_decisions(
        pair,
        "FTPS-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[],
    )
    .unwrap();
    let phrase = fill.preview().confirmation_phrase.clone();
    fill.confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    finish_bidirectional_run_completion(&bidi_baseline).unwrap();
    std::fs::remove_file(root.path().join("bidirektional/bestand-00.txt")).unwrap();
    let deletion = prepare_provider_bidirectional_with_decisions(
        pair,
        "FTPS-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[],
    )
    .unwrap();
    let phrase = deletion.preview().confirmation_phrase.clone();
    deletion
        .confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    finish_bidirectional_run_completion(&bidi_baseline).unwrap();
    assert!(!bidi_local.join("bestand-00.txt").exists());
    assert_eq!(
        std::fs::read(bidi_local.join("remote.txt")).unwrap(),
        b"FTPS bidirektional aktualisiert\n"
    );
    std::fs::remove_file(root.path().join("bidirektional/remote.txt")).unwrap();
    std::fs::create_dir(root.path().join("bidirektional/remote.txt")).unwrap();
    std::fs::write(
        root.path().join("bidirektional/remote.txt/inhalt.txt"),
        b"FTPS Typwechsel\n",
    )
    .unwrap();
    let type_swap = prepare_provider_bidirectional_with_decisions(
        pair,
        "FTPS-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[ConflictResolution {
            path: BinaryRelativePath(vec![b"remote.txt".to_vec()]),
            decision: ConflictDecision::KeepRight,
        }],
    )
    .unwrap();
    assert!(type_swap.preview().stage_blocker.is_none());
    let phrase = type_swap.preview().confirmation_phrase.clone();
    type_swap
        .confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    finish_bidirectional_run_completion(&bidi_baseline).unwrap();
    assert_eq!(
        std::fs::read(bidi_local.join("remote.txt/inhalt.txt")).unwrap(),
        b"FTPS Typwechsel\n"
    );

    let queue_target = download_area.path().join("queue-ziel");
    std::fs::create_dir(&queue_target).unwrap();
    let queue_preview = prepare_ftps_download(
        vfs.clone(),
        Path::new("/"),
        &queue_target,
        SyncOptions::default(),
    )
    .unwrap();
    let expected_plan = queue_preview.preview().plan.clone();
    drop(queue_preview);
    let queue_dir = private_area();
    let queue = OperationQueue::open(queue_dir.path().join("queue.json")).unwrap();
    let queued = queue
        .enqueue(QueueOperation::FtpsSync {
            direction: SftpQueueDirection::Download,
            connection: Box::new(config),
            local_root: queue_target.to_string_lossy().into_owned(),
            remote_root: "/".into(),
            options: SyncOptions::default(),
            expected_plan: Box::new(expected_plan),
        })
        .unwrap();
    let claimed = queue.claim_next().unwrap().unwrap();
    let prepared = match prepare_ftps_queue_entry(&queue, claimed, Arc::new(TestSecrets)).unwrap() {
        SftpQueuePreparation::Ready(prepared) => *prepared,
        SftpQueuePreparation::AwaitingConfirmation { reason } => {
            panic!("FTPS-Queue war unerwartet nicht bereit: {reason}")
        }
    };
    let phrase = prepared.preview().confirmation_phrase.clone();
    execute_prepared_sftp_queue_entry(
        &queue,
        prepared,
        &phrase,
        false,
        &queue_dir.path().join("audit.jsonl"),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        queue
            .snapshot()
            .entries
            .iter()
            .find(|entry| entry.operation_id == queued.operation_id)
            .unwrap()
            .state,
        QueueState::Completed
    );
    assert_eq!(
        std::fs::read(queue_target.join("inhalt.txt")).unwrap(),
        b"FTPS aktualisiert\n"
    );
}

#[test]
fn klartext_ftp_listet_und_reconnectet_ohne_tls_vortaeuschung() {
    let root = private_area();
    std::fs::write(root.path().join("klartext.txt"), b"FTP Testinhalt\n").unwrap();
    let port = free_port();
    let address = format!("127.0.0.1:{port}");
    let server_root = root.path().to_path_buf();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let server = ServerBuilder::new(Box::new(move || {
                Filesystem::new(server_root.clone()).unwrap()
            }))
            .build()
            .unwrap();
            server.listen(address).await.unwrap();
        });
    });
    wait_for_server(port);

    let config = FtpsConnectionConfig {
        host: "127.0.0.1".into(),
        alternate_hosts: Vec::new(),
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::Plaintext,
        certificate: None,
        certificate_policy: None,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "ftp").unwrap(),
    };
    let (vfs, _intent_config, _intent_queue) = owned_vfs(config);
    assert_eq!(vfs.security_status(), FtpSecurityStatus::Plaintext);
    assert_eq!(
        vfs.identity().transport_security,
        FtpTransportSecurity::Plaintext
    );
    assert!(vfs.identity().certificate.is_none());
    assert!(vfs
        .list(Path::new("/"))
        .unwrap()
        .iter()
        .any(|entry| entry.name == "klartext.txt"));
    let mut bytes = Vec::new();
    vfs.open_read(Path::new("/klartext.txt"))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"FTP Testinhalt\n");
    vfs.reconnect().unwrap();
    assert_eq!(vfs.security_status(), FtpSecurityStatus::Plaintext);
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_server(port: u16) {
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("lokaler FTP-Testserver startete nicht");
}

#[test]
fn ftps_move_behaelt_den_bestaetigten_copy_remove_weg() {
    use std::sync::atomic::Ordering;
    let (port, control) = fault_ftp_server(&[]);
    control.enable_stou.store(true, Ordering::SeqCst);
    control.rename_noreplace.store(true, Ordering::SeqCst);
    {
        let mut fs = control.fs.lock().unwrap();
        for name in ["from", "to"] {
            fs.get_mut("/").unwrap().insert(name.into(), true);
            fs.insert(format!("/{name}"), Default::default());
        }
        fs.get_mut("/from")
            .unwrap()
            .insert("file.txt".into(), false);
    }
    control
        .contents
        .lock()
        .unwrap()
        .insert("/from/file.txt".into(), b"source".to_vec());
    let area = private_area();
    let path = std::fs::canonicalize(area.path())
        .unwrap()
        .join("operations-queue.json");
    let queue = OperationQueue::open(path).unwrap();
    let mut operation = fault_move_operation(fault_config(port), &["file.txt"]);
    if let QueueOperation::ProviderFile {
        copy_new_best_effort_expected,
        remove_source_best_effort_expected,
        ..
    } = &mut operation
    {
        *copy_new_best_effort_expected = Some(true);
        *remove_source_best_effort_expected = Some(true);
    }
    queue.enqueue(operation).unwrap();
    let result = execute_fault_move(&queue);
    let commands = control.mutations.lock().unwrap().clone();
    assert!(
        !commands.iter().any(|cmd| cmd == "RNFR /from/file.txt"),
        "Bestaetigtes Copy/Remove wechselte ohne neue Vorschau zu Rename: {result:?}"
    );
    assert!(
        matches!(result, Ok(QueueExecutionResult::File(_))),
        "Copy/Remove fehlt: {result:?}"
    );
    assert_eq!(control.contents.lock().unwrap()["/to/file.txt"], b"source");
    assert!(!control
        .contents
        .lock()
        .unwrap()
        .contains_key("/from/file.txt"));
    assert_eq!(
        commands
            .iter()
            .filter(|cmd| cmd.as_str() == "DELE /from/file.txt")
            .count(),
        1
    );
}
