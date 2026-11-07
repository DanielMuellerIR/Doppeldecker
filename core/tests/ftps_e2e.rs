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
use doppeldecker_core::bidirectional::prepare_provider_bidirectional_with_decisions_bound_for_queue;
use doppeldecker_core::{
    discover_ftps_certificate, error_code, execute_prepared_sftp_queue_entry,
    execute_provider_file_queue_entry, prepare_ftps_download, prepare_ftps_queue_entry,
    BidirectionalQueueEndpoint, BinaryRelativePath, Cancel, Compare, ConflictDecision,
    ConflictResolution, ErrorCode, FileQueueKind, FtpSecurityStatus, FtpTransportSecurity,
    FtpsCertificatePolicy, FtpsConnectionConfig, FtpsVfs, KeychainSecretReference,
    LocalQueueExecution, OperationQueue, PairId, ProviderEndpoint, QueueExecutionResult,
    QueueOperation, QueueState, RenameExecutionClass, RenameOutcome, SecretResolver, SecretString,
    SftpQueueDirection, SftpQueuePreparation, SyncMode, SyncOptions, Vfs,
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
        copy_new_best_effort_expected: Some(true),
        replace_best_effort_expected: Some(false),
        remove_source_best_effort_expected: Some(true),
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
            if command == "STOU" {
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
        let result = execute_fault_move(&queue).unwrap();
        assert!(
            matches!(result, QueueExecutionResult::File(stats) if stats.files == 1 && stats.skipped == 1)
        );
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
    assert!(
        !capabilities.copy_new
            && capabilities.copy_new_best_effort
            && capabilities.replace_best_effort
    );
    let commands = control.mutations.lock().unwrap();
    assert!(commands.is_empty());
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
    assert!(
        target
            .probe_file_target_capabilities()
            .unwrap()
            .copy_new_best_effort
    );
    assert_eq!(*control.mutations.lock().unwrap(), mutations);
    assert_eq!(
        control.contents.lock().unwrap()["/to/foreign.txt"],
        b"foreign"
    );
    let fresh = queue.bind_provider_preparation(&target).unwrap();
    assert!(
        fresh
            .probe_file_target_capabilities()
            .unwrap()
            .copy_new_best_effort
    );
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
    cancel.cancel();
    let result = execute_fault_move_with_cancel(&queue, &cancel).unwrap();
    assert!(
        matches!(result, QueueExecutionResult::File(stats) if stats.cancelled && stats.files == 0)
    );
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Cancelled);
    assert!(control.mutations.lock().unwrap().is_empty());
    drop(queue);
    let reopened = OperationQueue::open(&path).unwrap();
    assert!(reopened.claim_next().unwrap().is_none());
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
                    if let QueueOperation::ProviderFile { target, .. } = &mut operation {
                        if let BidirectionalQueueEndpoint::Ftps { connection, .. } = target.as_mut()
                        {
                            connection.username = "anonymous-copy".into();
                        }
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
                if let QueueOperation::ProviderFile { target, .. } = &mut operation {
                    if let BidirectionalQueueEndpoint::Ftps { connection, .. } = target.as_mut() {
                        connection.username = "anonymous-copy".into();
                    }
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
        let result = execute_fault_move_with_cancel(&queue, &cancel);
        assert!(known.load(Ordering::SeqCst));
        assert_eq!(error_code(&result.unwrap_err()), ErrorCode::OutcomeUnclear);
        assert_eq!(
            control.contents.lock().unwrap()["/from/file.txt"],
            b"source"
        );
        assert!(!control
            .mutations
            .lock()
            .unwrap()
            .iter()
            .any(|cmd| cmd.starts_with("DELE /from/")));
        assert!(std::fs::read_dir(area.path()).unwrap().any(|entry| entry
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
    /// RNTO bestätigt ohne jede Namenswirkung; gewöhnlicher Readback muss ablehnen.
    NoopAndAcknowledge,
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
    require_mode_selection: std::sync::atomic::AtomicBool,
    accept_chmod: std::sync::atomic::AtomicBool,
    omit_mode_readback: std::sync::atomic::AtomicBool,
    ignore_chmod: std::sync::atomic::AtomicBool,
    lose_chmod_response: std::sync::atomic::AtomicBool,
    reset_mode_on_final_rename: std::sync::atomic::AtomicBool,
    mode_overrides: std::sync::Mutex<std::collections::HashMap<String, u32>>,
    accept_mfmt: std::sync::atomic::AtomicBool,
    advertise_mfmt: std::sync::atomic::AtomicBool,
    round_mfmt: std::sync::atomic::AtomicBool,
    lose_mfmt_response: std::sync::atomic::AtomicBool,
    reject_directory_mfmt: std::sync::atomic::AtomicBool,
    reset_mtime_on_final_rename: std::sync::atomic::AtomicBool,
    mtime_overrides: std::sync::Mutex<std::collections::HashMap<String, String>>,
    fs: std::sync::Mutex<FaultFs>,
    contents: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
    listings: std::sync::Mutex<Vec<String>>,
    mutations: std::sync::Mutex<Vec<String>>,
    retr_paths: std::sync::Mutex<Vec<String>>,
    retr_observer: std::sync::Mutex<Option<MutationObserver>>,
    retr_payload_override: std::sync::Mutex<Option<Vec<u8>>>,
    drop_retr_completion: std::sync::atomic::AtomicBool,
    unique_overrides: std::sync::Mutex<std::collections::HashMap<String, String>>,
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
    fault_ftp_server_transport(initial_files, None)
}

fn fault_ftp_server_transport(
    initial_files: &[&str],
    tls: Option<Arc<rustls::ServerConfig>>,
) -> (u16, Arc<FaultControl>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut root = std::collections::BTreeMap::new();
    for name in initial_files {
        root.insert((*name).to_string(), false);
    }
    let mut fs = FaultFs::new();
    fs.insert("/".into(), root);
    let control = Arc::new(FaultControl {
        require_mode_selection: std::sync::atomic::AtomicBool::new(false),
        accept_chmod: std::sync::atomic::AtomicBool::new(false),
        omit_mode_readback: std::sync::atomic::AtomicBool::new(true),
        ignore_chmod: std::sync::atomic::AtomicBool::new(false),
        lose_chmod_response: std::sync::atomic::AtomicBool::new(false),
        reset_mode_on_final_rename: std::sync::atomic::AtomicBool::new(false),
        mode_overrides: std::sync::Mutex::new(Default::default()),
        accept_mfmt: std::sync::atomic::AtomicBool::new(false),
        advertise_mfmt: std::sync::atomic::AtomicBool::new(false),
        round_mfmt: std::sync::atomic::AtomicBool::new(false),
        lose_mfmt_response: std::sync::atomic::AtomicBool::new(false),
        reject_directory_mfmt: std::sync::atomic::AtomicBool::new(false),
        reset_mtime_on_final_rename: std::sync::atomic::AtomicBool::new(false),
        mtime_overrides: std::sync::Mutex::new(Default::default()),
        fs: std::sync::Mutex::new(fs),
        contents: std::sync::Mutex::new(Default::default()),
        listings: std::sync::Mutex::new(Vec::new()),
        mutations: std::sync::Mutex::new(Vec::new()),
        retr_paths: std::sync::Mutex::new(Vec::new()),
        retr_observer: std::sync::Mutex::new(None),
        retr_payload_override: std::sync::Mutex::new(None),
        drop_retr_completion: std::sync::atomic::AtomicBool::new(false),
        unique_overrides: std::sync::Mutex::new(Default::default()),
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
        let tls = tls.clone();
        std::thread::spawn(move || fault_session_transport(stream, state, tls));
    });
    (port, control)
}

trait FaultStream: Read + Write {}
impl<T: Read + Write> FaultStream for T {}

struct FaultTlsData(rustls::StreamOwned<rustls::ServerConnection, TcpStream>);
impl Read for FaultTlsData {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(bytes)
    }
}
impl Write for FaultTlsData {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
impl Drop for FaultTlsData {
    fn drop(&mut self) {
        self.0.conn.send_close_notify();
        let _ = self.0.flush();
    }
}

fn fault_data_stream(
    stream: TcpStream,
    tls: &Option<Arc<rustls::ServerConfig>>,
) -> Box<dyn FaultStream> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    match tls {
        Some(config) => {
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(config.clone()).unwrap(),
                stream,
            );
            while stream.conn.is_handshaking() {
                stream.conn.complete_io(&mut stream.sock).unwrap();
            }
            Box::new(FaultTlsData(stream))
        }
        None => Box::new(stream),
    }
}

fn fault_session_transport(
    control_stream: TcpStream,
    state: Arc<FaultControl>,
    tls: Option<Arc<rustls::ServerConfig>>,
) {
    use std::sync::atomic::Ordering;
    control_stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    control_stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader: BufReader<Box<dyn FaultStream>> = BufReader::new(Box::new(control_stream));
    let greeting: &[u8] = if state.identity_swapped.load(Ordering::SeqCst) {
        b"220 Anderer Server\r\n"
    } else {
        b"220 Fault-Injection FTP\r\n"
    };
    if reader.get_mut().write_all(greeting).is_err() {
        return;
    }
    let mut cwd = String::from("/");
    let mut pending_rename: Option<String> = None;
    let mut data_listener: Option<TcpListener> = None;
    let mut protected_data = false;
    let mut mode_facts_selected = false;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let command = line.trim_end_matches(['\r', '\n']);
        if [
            "MKD", "RMD", "STOU", "STOR", "RNFR", "RNTO", "DELE", "MFMT", "SITE",
        ]
        .contains(&command.split_whitespace().next().unwrap_or_default())
        {
            state.mutations.lock().unwrap().push(command.to_string());
            if let Some(observer) = state.mutation_observer.lock().unwrap().clone() {
                observer(command);
            }
        }
        let response: String = match command {
            "AUTH TLS" if tls.is_some() => {
                reader.get_mut().write_all(b"234 Start TLS\r\n").unwrap();
                reader.get_mut().flush().unwrap();
                let stream = reader.into_inner();
                reader = BufReader::new(Box::new(rustls::StreamOwned::new(
                    rustls::ServerConnection::new(tls.as_ref().unwrap().clone()).unwrap(),
                    stream,
                )));
                continue;
            }
            "PBSZ 0" => "200 PBSZ accepted\r\n".into(),
            "PROT P" => {
                protected_data = true;
                "200 PROT accepted\r\n".into()
            }
            "NOOP" => "200 NOOP\r\n".into(),
            value if value.starts_with("USER ") => "331 Password required\r\n".into(),
            value if value.starts_with("PASS ") => "230 Logged in\r\n".into(),
            "TYPE I" => "200 Binary mode\r\n".into(),
            "OPTS MLST Type;Size;Modify;Unique;" => "200 MLST facts selected\r\n".into(),
            "OPTS MLST Type;Size;Modify;Unique;unix.mode;" => {
                mode_facts_selected = true;
                "200 MLST facts selected\r\n".into()
            }
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
                    reader
                        .get_mut()
                        .write_all(format!("150 FILE: {name}\r\n").as_bytes())
                        .unwrap();
                    reader.get_mut().flush().unwrap();
                    let (data, _) = data_listener.take().unwrap().accept().unwrap();
                    let mut data =
                        fault_data_stream(data, &if protected_data { tls.clone() } else { None });
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
            "FEAT" => {
                format!(
                    "211-Features\r\n MLST Type;Size;Modify;Unique;{}\r\n UTF8\r\n{}211 End\r\n",
                    if state.require_mode_selection.load(Ordering::SeqCst) {
                        "unix.mode;"
                    } else {
                        ""
                    },
                    if state.advertise_mfmt.load(Ordering::SeqCst) {
                        " MFMT\r\n"
                    } else {
                        ""
                    }
                )
            }
            value if value.starts_with("SITE CHMOD ") => {
                if !state.accept_chmod.load(Ordering::SeqCst) {
                    "502 CHMOD unavailable\r\n".into()
                } else {
                    let (mode, path) = value[11..].split_once(' ').unwrap();
                    let mode = u32::from_str_radix(mode, 8).unwrap();
                    let path = fault_path(&cwd, path);
                    if !state.ignore_chmod.load(Ordering::SeqCst) {
                        state.mode_overrides.lock().unwrap().insert(path, mode);
                    }
                    if state.lose_chmod_response.swap(false, Ordering::SeqCst) {
                        return;
                    }
                    "200 CHMOD accepted\r\n".into()
                }
            }
            value if value.starts_with("MFMT ") => {
                if !state.accept_mfmt.load(Ordering::SeqCst) {
                    "502 MFMT unavailable\r\n".into()
                } else {
                    let (timestamp, path) = value[5..].split_once(' ').unwrap();
                    assert_eq!(timestamp.len(), 14);
                    assert!(timestamp.bytes().all(|byte| byte.is_ascii_digit()));
                    let path = fault_path(&cwd, path);
                    let (parent, name) = fault_split(&path);
                    assert!(state.fs.lock().unwrap()[&parent].contains_key(&name));
                    if state.fs.lock().unwrap()[&parent][&name]
                        && state.reject_directory_mfmt.load(Ordering::SeqCst)
                    {
                        reader
                            .get_mut()
                            .write_all(b"550 Directory timestamps denied\r\n")
                            .unwrap();
                        reader.get_mut().flush().unwrap();
                        continue;
                    }
                    let timestamp = if state.round_mfmt.load(Ordering::SeqCst) {
                        format!("{}00", &timestamp[..12])
                    } else {
                        timestamp.to_owned()
                    };
                    state
                        .mtime_overrides
                        .lock()
                        .unwrap()
                        .insert(path, timestamp.clone());
                    if state.lose_mfmt_response.swap(false, Ordering::SeqCst) {
                        return;
                    }
                    format!("213 Modify={timestamp};\r\n")
                }
            }
            "OPTS UTF8 ON" => "200 UTF8 enabled\r\n".into(),
            "OPTS MLST Type;Size;Modify;" => "200 MLST OPTS Type;Size;Modify;\r\n".into(),
            "QUIT" => {
                let _ = reader.get_mut().write_all(b"221 Bye\r\n");
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
                    if reader
                        .get_mut()
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
                                    let full = format!("{}/{name}", cwd.trim_end_matches('/'));
                                    let modify = state
                                        .mtime_overrides
                                        .lock()
                                        .unwrap()
                                        .get(&full)
                                        .cloned()
                                        .unwrap_or_else(|| "20260728060000".into());
                                    let mode = if state.omit_mode_readback.load(Ordering::SeqCst) || (state.require_mode_selection.load(Ordering::SeqCst) && !mode_facts_selected) { String::new() } else { format!("unix.mode={:04o};", state.mode_overrides.lock().unwrap().get(&full).copied().unwrap_or(if *is_dir {0o755} else {0o644})) };
                                    let unique = state.unique_overrides.lock().unwrap().get(&full).map(|value| format!("Unique={value};")).unwrap_or_default();
                                    if *is_dir {
                                        format!("Type=dir;Size=0;Modify={modify};{unique}{mode} {name}\r\n")
                                    } else {
                                        let path = format!("{}/{name}", cwd.trim_end_matches('/'));
                                        let size = state
                                            .contents
                                            .lock()
                                            .unwrap()
                                            .get(&path)
                                            .map_or(4, Vec::len);
                                        format!("Type=file;Size={size};Modify={modify};{unique}{mode} {name}\r\n")
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
                reader
                    .get_mut()
                    .write_all(b"150 Opening data connection\r\n")
                    .unwrap();
                let (data, _) = data_listener.take().unwrap().accept().unwrap();
                let mut data =
                    fault_data_stream(data, &if protected_data { tls.clone() } else { None });
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
                    reader
                        .get_mut()
                        .write_all(b"550 RETR denied by artificial fixture\r\n")
                        .unwrap();
                    continue;
                }
                reader
                    .get_mut()
                    .write_all(b"150 Opening data connection\r\n")
                    .unwrap();
                let (data, _) = data_listener.take().unwrap().accept().unwrap();
                let mut data =
                    fault_data_stream(data, &if protected_data { tls.clone() } else { None });
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
                let content = state
                    .retr_payload_override
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or(content);
                if data.write_all(&content).is_err() {
                    return;
                }
                drop(data);
                if state.drop_retr_completion.load(Ordering::SeqCst) {
                    return;
                }
                "226 Transfer complete\r\n".into()
            }
            value if value.starts_with("MKD ") => {
                let path = fault_path(&cwd, &value[4..]);
                let (parent, name) = fault_split(&path);
                if state.reject_probe_mkdir.load(Ordering::SeqCst) {
                    if reader
                        .get_mut()
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
                    if reader
                        .get_mut()
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
                    if reader
                        .get_mut()
                        .write_all(b"553 Target exists\r\n")
                        .is_err()
                    {
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
                if fault == Some(RntoFault::NoopAndAcknowledge) {
                    if reader
                        .get_mut()
                        .write_all(b"250 Rename successful\r\n")
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
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
                    if reader
                        .get_mut()
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
                {
                    let mut times = state.mtime_overrides.lock().unwrap();
                    if let Some(mtime) = times.remove(&from) {
                        times.insert(to.clone(), mtime);
                    }
                    if state.reset_mtime_on_final_rename.load(Ordering::SeqCst)
                        && !to.contains(".doppeldecker-")
                    {
                        times.remove(&to);
                    }
                }
                {
                    let mut modes = state.mode_overrides.lock().unwrap();
                    if let Some(mode) = modes.remove(&from) {
                        modes.insert(to.clone(), mode);
                    }
                    if state.reset_mode_on_final_rename.load(Ordering::SeqCst)
                        && !to.contains(".doppeldecker-")
                    {
                        modes.remove(&to);
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
        if reader
            .get_mut()
            .write_all(response.as_bytes())
            .and_then(|_| reader.get_mut().flush())
            .is_err()
        {
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
    let base = String::from("/pane");
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

    assert!(
        first
            .probe_rename_capability_at(Path::new("/"))
            .unwrap()
            .best_effort_rename
    );
    control
        .rename_noreplace
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(
        second
            .probe_rename_capability_at(Path::new("/"))
            .unwrap()
            .best_effort_rename
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
    let mut operation = fault_move_operation(fault_config(port), &["file.txt"]);
    if let QueueOperation::ProviderFile {
        copy_new_best_effort_expected,
        remove_source_best_effort_expected,
        ..
    } = &mut operation
    {
        *copy_new_best_effort_expected = Some(false);
        *remove_source_best_effort_expected = Some(false);
    }
    queue.enqueue(operation).unwrap();
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
                .all(|cmd| cmd.starts_with("MKD /to/") && !cmd.contains("probe")),
            "Echte Anlage verließ den Zielroot: {commands:?}"
        );
        assert_eq!(
            commands.iter().filter(|cmd| cmd.as_str() == "STOU").count(),
            usize::from(files),
            "files={files}, directories={directories}, collision={collision}, result={result:?}, commands={commands:?}"
        );
        if collision {
            assert!(!commands.iter().any(|cmd| cmd.starts_with("DELE /from/")));
        }
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
            if command.starts_with("RNFR /to/") {
                let control = weak.upgrade().unwrap();
                *control.fault.lock().unwrap() = Some(fault);
                *control.fault_destination.lock().unwrap() = Some("/to/file.txt".into());
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
                assert!(intents.len() >= 2);
                assert_eq!(intents[0]["owner"]["operation"], operation);
                assert!(intents
                    .iter()
                    .any(|intent| intent["ftps"]["phase"] == "rename"
                        && intent["ftps"]["paths"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|p| p == "/to/file.txt")));
                observed.store(true, Ordering::SeqCst);
            }
        }));
        let result = execute_fault_move(&queue);
        assert!(observed_intent.load(Ordering::SeqCst));
        let error = result.unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        let moved = fault == RntoFault::DropAfterMutation;
        let contents = control.contents.lock().unwrap();
        assert!(contents.contains_key("/from/file.txt"));
        assert_eq!(contents.contains_key("/to/file.txt"), moved);
        assert_eq!(
            contents
                .values()
                .filter(|body| body.as_slice() == b"source")
                .count(),
            2
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
    assert!(
        endpoint
            .probe_file_source_removal_capabilities()
            .unwrap()
            .remove_source_best_effort
    );
    assert!(control.mutations.lock().unwrap().is_empty());
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
    let result = execute_provider_file_queue_entry(
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
    let QueueExecutionResult::File(stats) = result else {
        panic!("kein Dateiergebnis");
    };
    assert!(stats
        .target_write_limitations
        .iter()
        .any(|note| note.contains("Keine dauerhafte Temp-Recovery")));
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
    let bidi_queue = OperationQueue::open(area.path().join("queue.json")).unwrap();
    let baseline = area.path().join("state/pair.json");
    let initial = prepare_provider_bidirectional_with_decisions_bound_for_queue(
        &bidi_queue,
        PairId([116; 16]),
        "Passiv gesperrtes FTPS-Ziel",
        ProviderEndpoint::local(&local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/pane")).unwrap(),
        &baseline,
        &[],
        None,
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
    let prepared = prepare_provider_bidirectional_with_decisions_bound_for_queue(
        &bidi_queue,
        PairId([116; 16]),
        "Passiv gesperrtes FTPS-Ziel",
        ProviderEndpoint::local(&local).unwrap(),
        ProviderEndpoint::ftps(vfs, Path::new("/pane")).unwrap(),
        &baseline,
        &[],
        None,
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
    let bidi_queue = OperationQueue::open(area.path().join("queue.json")).unwrap();
    let baseline = area.path().join("state/pair.json");
    let prepared = prepare_provider_bidirectional_with_decisions_bound_for_queue(
        &bidi_queue,
        PairId([114; 16]),
        "Leere FTPS-Erstbaseline",
        ProviderEndpoint::local(&local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/pane")).unwrap(),
        &baseline,
        &[],
        None,
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
    let prepared = prepare_provider_bidirectional_with_decisions_bound_for_queue(
        &bidi_queue,
        PairId([114; 16]),
        "FTPS als reine Quelle",
        ProviderEndpoint::local(&local).unwrap(),
        ProviderEndpoint::ftps(vfs, Path::new("/pane")).unwrap(),
        &baseline,
        &[],
        None,
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
                let capabilities = vfs.probe_rename_capability_at(root)?;
                Ok((
                    false,
                    capabilities.atomic_noreplace_rename,
                    capabilities.best_effort_rename,
                ))
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
                assert!(first_result.2);
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
    assert!(
        endpoint
            .probe_file_capabilities()
            .unwrap()
            .replace_best_effort
    );
    assert!(control.mutations.lock().unwrap().is_empty());
    let result = vfs.probe_permanent_delete_capability_at(Path::new("/"));
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
    {
        use doppeldecker_core::batch_rename::BatchRenameOptions;
        use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
        let pane = root.path().join("batch-tls");
        std::fs::create_dir(&pane).unwrap();
        std::fs::write(pane.join("a"), b"erster Inhalt").unwrap();
        std::fs::write(pane.join("b"), b"zweiter Inhalt").unwrap();
        let endpoint =
            ProviderEndpoint::ftps(Arc::new(vfs.clone()), Path::new("/batch-tls")).unwrap();
        let plan = PreparedFtpBatchRename::prepare(
            &endpoint,
            &["a".into(), "b".into()],
            &BatchRenameOptions {
                find: String::new(),
                replacement: String::new(),
                prefix: "neu-".into(),
                suffix: String::new(),
                preserve_extension: true,
                trim_outer_spaces: false,
                numbering: None,
            },
            &Cancel::new(),
        )
        .unwrap();
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
        assert_eq!(report.renamed, 2, "{:?}", report.rows);
        assert_eq!(std::fs::read(pane.join("neu-a")).unwrap(), b"erster Inhalt");
        assert_eq!(
            std::fs::read(pane.join("neu-b")).unwrap(),
            b"zweiter Inhalt"
        );
        std::fs::remove_dir_all(pane).unwrap();
    }

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

    // Strikte Proben bleiben separat. Gewöhnliches Benutzerlöschen verwendet
    // unabhängig davon den explizit bestätigten Best-Effort-Vertrag.
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
    assert!(endpoint_delete_capabilities.permanent);
    assert!(endpoint_delete_capabilities.blockers.is_empty());
    let names_after_endpoint_delete_probe = std::fs::read_dir(root.path().join("scoped-probes"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        names_before_endpoint_delete_probe, names_after_endpoint_delete_probe,
        "gewöhnliche Löschfreigabe darf keine Capability-Probe mutieren"
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
    let initial = prepare_provider_bidirectional_with_decisions_bound_for_queue(
        &_intent_queue,
        pair,
        "FTPS-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[],
        None,
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
    let remote_to_local = prepare_provider_bidirectional_with_decisions_bound_for_queue(
        &_intent_queue,
        pair,
        "FTPS-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[],
        None,
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
    let replacement = prepare_provider_bidirectional_with_decisions_bound_for_queue(
        &_intent_queue,
        pair,
        "FTPS-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[],
        None,
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
    let fill = prepare_provider_bidirectional_with_decisions_bound_for_queue(
        &_intent_queue,
        pair,
        "FTPS-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[],
        None,
    )
    .unwrap();
    let phrase = fill.preview().confirmation_phrase.clone();
    fill.confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    finish_bidirectional_run_completion(&bidi_baseline).unwrap();
    std::fs::remove_file(root.path().join("bidirektional/bestand-00.txt")).unwrap();
    let deletion = prepare_provider_bidirectional_with_decisions_bound_for_queue(
        &_intent_queue,
        pair,
        "FTPS-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[],
        None,
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
    let type_swap = prepare_provider_bidirectional_with_decisions_bound_for_queue(
        &_intent_queue,
        pair,
        "FTPS-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::ftps(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[ConflictResolution {
            path: BinaryRelativePath(vec![b"remote.txt".to_vec()]),
            decision: ConflictDecision::KeepRight,
        }],
        None,
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

#[test]
fn ftp_stapel_ausschlussprobe_gehaltene_sitzung_ohne_probe_oder_zweiten_rnto() {
    for lost in [false, true] {
        let (port, control) = fault_ftp_server(&["a", "b"]);
        let (vfs, _area, _queue) = owned_vfs(fault_config(port));
        let endpoint = ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap();
        let caps = endpoint.probe_rename_capabilities().unwrap();
        assert_eq!(
            caps.execution_class(),
            Some(RenameExecutionClass::BestEffort)
        );
        assert!(control.mutations.lock().unwrap().is_empty());
        if lost {
            *control.fault.lock().unwrap() = Some(RntoFault::DropAfterMutation);
        }
        let first = endpoint.execute_ordinary_rename(Path::new("/a"), "new-a", &None, None);
        if lost {
            assert!(first.unwrap().verified_after_reconnect);
            assert!(control.fs.lock().unwrap()["/"].contains_key("new-a"));
        } else {
            assert_eq!(first.unwrap().outcome, RenameOutcome::Renamed);
            assert_eq!(
                endpoint
                    .execute_ordinary_rename(Path::new("/b"), "new-b", &None, None)
                    .unwrap()
                    .outcome,
                RenameOutcome::Renamed
            );
        }
        assert_eq!(
            control
                .mutations
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.starts_with("RNTO "))
                .count(),
            if lost { 1 } else { 2 }
        );
        assert!(control.retr_paths.lock().unwrap().is_empty());
    }
}

#[test]
fn ftp_stapel_bindet_quellen_eltern_sitzung_abbruch_und_unklare_wirkung() {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    for change in 0..8 {
        let (port, control) = fault_ftp_server(&[]);
        {
            let mut fs = control.fs.lock().unwrap();
            fs.get_mut("/").unwrap().insert("pane".into(), true);
            fs.insert(
                "/pane".into(),
                [
                    ("a".into(), false),
                    ("b".into(), false),
                    ("c".into(), false),
                ]
                .into(),
            );
        }
        let (vfs, _area, _queue) = owned_vfs(fault_config(port));
        let provider = Arc::new(vfs);
        let endpoint = ProviderEndpoint::ftps(provider.clone(), Path::new("/pane")).unwrap();
        let options = BatchRenameOptions {
            find: String::new(),
            replacement: String::new(),
            prefix: "new-".into(),
            suffix: String::new(),
            preserve_extension: true,
            trim_outer_spaces: false,
            numbering: None,
        };
        let plan = PreparedFtpBatchRename::prepare(
            &endpoint,
            &["a".into(), "b".into(), "c".into()],
            &options,
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(plan.execution_class(), RenameExecutionClass::BestEffort);
        assert!(control.mutations.lock().unwrap().is_empty());
        assert!(control.retr_paths.lock().unwrap().is_empty());
        let weak = Arc::downgrade(&control);
        *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
            if command.starts_with("RNTO ") {
                if let Some(control) = weak.upgrade() {
                    control
                        .mtime_overrides
                        .lock()
                        .unwrap()
                        .insert("/pane".into(), "20260728060001".into());
                }
            }
        }));
        let cancel = Cancel::new();
        if change == 7 {
            cancel.cancel();
        }
        let report = plan.execute(&cancel, &mut || Ok(()), &mut |done, _| {
            if done == 1 {
                match change {
                    1 => {
                        control
                            .fs
                            .lock()
                            .unwrap()
                            .get_mut("/pane")
                            .unwrap()
                            .insert("new-b".into(), false);
                    }
                    2 => {
                        control
                            .contents
                            .lock()
                            .unwrap()
                            .insert("/pane/b".into(), b"changed".to_vec());
                    }
                    3 => {
                        control
                            .mtime_overrides
                            .lock()
                            .unwrap()
                            .insert("/pane".into(), "20260728060002".into());
                    }
                    4 => provider.reconnect().unwrap(),
                    5 => {
                        *control.fault.lock().unwrap() = Some(RntoFault::DropAfterMutation);
                    }
                    6 => cancel.cancel(),
                    _ => {}
                }
            }
        });
        let rnto = control
            .mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("RNTO "))
            .count();
        match change {
            0 => {
                assert_eq!(report.renamed, 3, "{:?}", report.rows);
                assert_eq!(rnto, 3);
            }
            1 => {
                assert_eq!(
                    (report.renamed, report.failed_unchanged),
                    (2, 1),
                    "{:?}",
                    report.rows
                );
                assert_eq!(report.rows[1].code, Some(ErrorCode::AlreadyExists));
                assert_eq!(rnto, 2);
            }
            2..=4 => {
                assert_eq!(
                    (report.renamed, report.failed_unchanged),
                    (1, 2),
                    "{:?}",
                    report.rows
                );
                assert_eq!(report.rows[1].code, Some(ErrorCode::PlanDrift));
                assert_eq!(rnto, 1);
            }
            5 => {
                assert_eq!(
                    (
                        report.renamed,
                        report.outcome_unclear,
                        report.failed_unchanged
                    ),
                    (1, 1, 1),
                    "{:?}",
                    report.rows
                );
                assert_eq!(report.rows[1].code, Some(ErrorCode::OutcomeUnclear));
                assert_eq!(rnto, 2);
                assert!(report.rows[1].message.contains("/pane/new-b"));
            }
            6 => {
                assert_eq!((report.renamed, report.cancelled), (1, 2));
                assert_eq!(rnto, 1);
            }
            7 => {
                assert_eq!(report.cancelled, 3);
                assert_eq!(rnto, 0);
            }
            _ => unreachable!(),
        }
        if (2..=7).contains(&change) {
            assert!(control.fs.lock().unwrap()["/pane"].contains_key("c"));
            assert_ne!(report.rows[2].status, BatchRenameStatus::Renamed);
        }
        assert!(control.retr_paths.lock().unwrap().is_empty());
    }
}

#[test]
fn ftp_stapel_nummeriert_die_gebundene_reihenfolge_ohne_inhaltslauf() {
    use doppeldecker_core::batch_rename::{
        BatchRenameNumberPosition, BatchRenameNumbering, BatchRenameOptions,
    };
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    let (port, control) = fault_ftp_server(&["a.txt", "b.txt"]);
    control.contents.lock().unwrap().extend([
        ("/a.txt".into(), b"A".to_vec()),
        ("/b.txt".into(), b"B".to_vec()),
    ]);
    let (vfs, _area, _queue) = owned_vfs(fault_config(port));
    let endpoint = ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap();
    let options = BatchRenameOptions {
        find: String::new(),
        replacement: String::new(),
        prefix: String::new(),
        suffix: String::new(),
        preserve_extension: true,
        trim_outer_spaces: false,
        numbering: Some(BatchRenameNumbering {
            start: 0,
            step: 2,
            min_digits: 2,
            position: BatchRenameNumberPosition::After,
        }),
    };
    let plan = PreparedFtpBatchRename::prepare(
        &endpoint,
        &["b.txt".into(), "a.txt".into()],
        &options,
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(
        plan.rows()
            .iter()
            .map(|row| row.new_name.as_str())
            .collect::<Vec<_>>(),
        ["b_00.txt", "a_02.txt"]
    );
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    assert_eq!(report.renamed, 2, "{:?}", report.rows);
    assert_eq!(
        control
            .mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("RNTO "))
            .cloned()
            .collect::<Vec<_>>(),
        ["RNTO /b_00.txt", "RNTO /a_02.txt"]
    );
    assert_eq!(control.contents.lock().unwrap()["/b_00.txt"], b"B");
    assert_eq!(control.contents.lock().unwrap()["/a_02.txt"], b"A");
    assert!(control.retr_paths.lock().unwrap().is_empty());
}

struct FtpNameGroupCase {
    remote: tempfile::TempDir,
    _source: tempfile::TempDir,
    source: PathBuf,
    _config: tempfile::TempDir,
    _owner: OperationQueue,
    vfs: Arc<FtpsVfs>,
    connection: FtpsConnectionConfig,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}
impl FtpNameGroupCase {
    fn new() -> Self {
        Self::new_transport(false)
    }
    fn new_transport(tls: bool) -> Self {
        let remote = private_area();
        std::fs::create_dir(remote.path().join("target")).unwrap();
        let source = match std::env::var_os("DOPPELDECKER_TEST_CASE_SENSITIVE_ROOT") {
            Some(root) => tempfile::tempdir_in(root).unwrap(),
            None => private_area(),
        };
        let source_path = std::fs::canonicalize(source.path()).unwrap();
        let port = free_port();
        let address = format!("127.0.0.1:{port}");
        let root = remote.path().to_path_buf();
        let server_tls = tls.then(|| {
            let certified = generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key.into())
            .unwrap()
        });
        let (shutdown, receiver) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let builder = ServerBuilder::new(Box::new(move || Filesystem::new(root.clone()).unwrap()));
                let server = match server_tls {
                    Some(config) => builder.ftps_manual::<PathBuf>(Arc::new(config)).ftps_required(FtpsRequired::All, FtpsRequired::All).build().unwrap(),
                    None => builder.build().unwrap(),
                };
                tokio::select! { result = server.listen(address) => result.unwrap(), _ = receiver => {} }
            });
        });
        wait_for_server(port);
        let mut connection = fault_config(port);
        if tls {
            connection.host = "localhost".into();
            connection.transport_security = FtpTransportSecurity::ExplicitTls;
            connection.certificate = Some(discover_ftps_certificate("localhost", port).unwrap());
            connection.certificate_policy = Some(FtpsCertificatePolicy::PinnedCertificate);
        }
        let (vfs, config, owner) = owned_vfs(connection.clone());
        Self {
            remote,
            _source: source,
            source: source_path,
            _config: config,
            _owner: owner,
            vfs: Arc::new(vfs),
            connection,
            shutdown: Some(shutdown),
            thread: Some(thread),
        }
    }
    fn operation(&self, names: &[&str]) -> QueueOperation {
        let (contexts, items) = self.contexts(names);
        QueueOperation::ProviderFile {
            kind: FileQueueKind::Copy,
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: self.source.to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::Ftps {
                connection: Box::new(self.connection.clone()),
                root: "/target".into(),
            }),
            items: items
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            destination: "/target".into(),
            overwrite: false,
            expected_conflicts: vec![],
            source_tree_expected: contexts
                .source_vfs
                .interactive_source_tree()
                .unwrap()
                .map(Box::new),
            expected_conflict_proofs: vec![],
            keep_both_targets: vec![],
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: Some(true),
            replace_best_effort_expected: Some(false),
            remove_source_best_effort_expected: None,
        }
    }
    fn copy(
        &self,
        contexts: &doppeldecker_core::InteractiveFileContexts,
        items: &[PathBuf],
        progress: &mut dyn FnMut(&doppeldecker_core::Progress),
    ) -> std::io::Result<doppeldecker_core::fileops::OpStats> {
        doppeldecker_core::fileops::copy_items(
            doppeldecker_core::fileops::FileOperationRequest {
                src: contexts.source_vfs.as_ref(),
                dst: contexts.target_vfs.as_ref(),
                items,
                dst_dir: Path::new("/target"),
                policy: doppeldecker_core::fileops::OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            progress,
        )
    }
    fn contexts(
        &self,
        names: &[&str],
    ) -> (doppeldecker_core::InteractiveFileContexts, Vec<PathBuf>) {
        let source = ProviderEndpoint::local(&self.source).unwrap();
        let target = ProviderEndpoint::ftps(self.vfs.clone(), Path::new("/target")).unwrap();
        let items = names
            .iter()
            .map(|name| self.source.join(name))
            .collect::<Vec<_>>();
        let contexts = source
            .interactive_file_contexts(&target, &items, false, false, &Cancel::new())
            .unwrap();
        (contexts, items)
    }
}
impl Drop for FtpNameGroupCase {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[test]
fn ftp_namensgruppen_direkte_auswahl_laesst_ganze_gruppe_aus_und_kopiert_good() {
    let case = FtpNameGroupCase::new();
    for name in ["a", "A", "good.txt"] {
        std::fs::write(case.source.join(name), name).unwrap();
    }
    #[cfg(target_os = "linux")]
    assert_eq!(
        std::fs::read_dir(&case.source).unwrap().count(),
        3,
        "Probe benötigt ein case-sensitives Dateisystem"
    );
    let (contexts, items) = case.contexts(&["a", "A", "good.txt"]);
    let tree = contexts
        .source_vfs
        .interactive_source_tree()
        .unwrap()
        .expect("Gruppenbilanz fehlt");
    assert_eq!(
        (
            tree.entries,
            tree.files,
            tree.directories,
            tree.omitted.len()
        ),
        (3, 1, 0, 2)
    );
    let stats = doppeldecker_core::fileops::copy_items(
        doppeldecker_core::fileops::FileOperationRequest {
            src: contexts.source_vfs.as_ref(),
            dst: contexts.target_vfs.as_ref(),
            items: &items,
            dst_dir: Path::new("/target"),
            policy: doppeldecker_core::fileops::OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((stats.files, stats.skipped), (1, 2));
    assert_eq!(
        std::fs::read(case.remote.path().join("target/good.txt")).unwrap(),
        b"good.txt"
    );
    assert_eq!(
        std::fs::read_dir(case.remote.path().join("target"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn ftp_namensgruppen_unicode_und_belegtes_auslassungsziel_sind_keine_konflikte() {
    let case = FtpNameGroupCase::new();
    for name in ["é", "e\u{301}", "good.txt"] {
        std::fs::write(case.source.join(name), name).unwrap();
    }
    #[cfg(target_os = "linux")]
    assert_eq!(
        std::fs::read_dir(&case.source).unwrap().count(),
        3,
        "NFC/NFD müssen hier zwei echte Dateien sein"
    );
    std::fs::write(case.remote.path().join("target/é"), b"Ziel erhalten").unwrap();
    let (contexts, items) = case.contexts(&["é", "e\u{301}", "good.txt"]);
    let tree = contexts
        .source_vfs
        .interactive_source_tree()
        .unwrap()
        .unwrap();
    assert_eq!((tree.entries, tree.files, tree.omitted.len()), (3, 1, 2));
    assert!(doppeldecker_core::fileops::find_recursive_conflicts(
        contexts.source_vfs.as_ref(),
        contexts.target_vfs.as_ref(),
        Path::new("/target"),
        &items
    )
    .unwrap()
    .is_empty());
    let stats = case.copy(&contexts, &items, &mut |_| {}).unwrap();
    assert_eq!((stats.files, stats.skipped), (1, 2));
    assert_eq!(
        std::fs::read(case.remote.path().join("target/é")).unwrap(),
        b"Ziel erhalten"
    );
    assert_eq!(
        std::fs::read(case.remote.path().join("target/good.txt")).unwrap(),
        b"good.txt"
    );
}

#[test]
fn ftp_namensgruppen_ordner_werden_nicht_traversiert_und_baumgruppen_ausgelassen() {
    use std::os::unix::fs::PermissionsExt;
    let case = FtpNameGroupCase::new();
    for name in ["a", "A"] {
        std::fs::create_dir_all(case.source.join(name)).unwrap();
        std::fs::write(case.source.join(name).join("Kind"), b"nie lesen").unwrap();
    }
    for name in ["a", "A"] {
        std::fs::set_permissions(
            case.source.join(name),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
    }
    std::fs::create_dir(case.source.join("good")).unwrap();
    for name in ["x", "X", "yes.txt"] {
        std::fs::write(case.source.join("good").join(name), name).unwrap();
    }
    let distinct_children = std::fs::read_dir(case.source.join("good")).unwrap().count() == 3;
    #[cfg(target_os = "linux")]
    assert!(
        distinct_children,
        "Linuxprobe braucht zwei echte Case-Einträge"
    );
    let (contexts, items) = case.contexts(&["a", "A", "good"]);
    let tree = contexts
        .source_vfs
        .interactive_source_tree()
        .unwrap()
        .unwrap();
    let result = case.copy(&contexts, &items, &mut |_| {});
    for name in ["a", "A"] {
        std::fs::set_permissions(
            case.source.join(name),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    let stats = result.unwrap();
    assert_eq!(
        (
            tree.entries,
            tree.files,
            tree.directories,
            tree.omitted.len()
        ),
        if distinct_children {
            (6, 1, 1, 4)
        } else {
            (5, 2, 1, 2)
        }
    );
    assert_eq!(
        (stats.files, stats.skipped),
        if distinct_children { (1, 4) } else { (2, 2) }
    );
    assert_eq!(
        std::fs::read(case.remote.path().join("target/good/yes.txt")).unwrap(),
        b"yes.txt"
    );
    assert!(!case.remote.path().join("target/a").exists());
    assert!(!case.remote.path().join("target/A").exists());
    assert_eq!(
        std::fs::read_dir(case.remote.path().join("target/good"))
            .unwrap()
            .count(),
        if distinct_children { 1 } else { 2 }
    );
}

#[test]
fn ftp_namensgruppen_live_drift_nach_erstem_commit_stoppt_zweite_datei() {
    let case = FtpNameGroupCase::new();
    for name in ["a", "A", "good1.txt", "good2.txt"] {
        std::fs::write(case.source.join(name), b"equal").unwrap();
    }
    let original = std::fs::metadata(case.source.join("a")).unwrap();
    let (contexts, items) = case.contexts(&["a", "A", "good1.txt", "good2.txt"]);
    let mut changed = false;
    let result = case.copy(&contexts, &items, &mut |progress| {
        if !changed && progress.files_done == 1 {
            changed = true;
            std::fs::write(case.source.join("a"), b"drift").unwrap();
            filetime::set_file_mtime(
                case.source.join("a"),
                filetime::FileTime::from_last_modification_time(&original),
            )
            .unwrap();
        }
    });
    assert!(changed);
    assert_eq!(error_code(&result.unwrap_err()), ErrorCode::PlanDrift);
    assert_eq!(
        std::fs::read(case.remote.path().join("target/good1.txt")).unwrap(),
        b"equal"
    );
    assert!(!case.remote.path().join("target/good2.txt").exists());
}

#[test]
fn ftp_namensgruppen_queue_reopen_behaelt_bilanz_und_drift_verlangt_bestaetigung() {
    for drift in [false, true] {
        let case = FtpNameGroupCase::new();
        for name in ["a", "A", "good.txt"] {
            std::fs::write(case.source.join(name), b"equal").unwrap();
        }
        let queue_dir = private_area();
        let path = std::fs::canonicalize(queue_dir.path())
            .unwrap()
            .join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        queue
            .enqueue(case.operation(&["a", "A", "good.txt"]))
            .unwrap();
        drop(queue);
        if drift {
            let original = std::fs::metadata(case.source.join("a")).unwrap();
            std::fs::write(case.source.join("a"), b"drift").unwrap();
            filetime::set_file_mtime(
                case.source.join("a"),
                filetime::FileTime::from_last_modification_time(&original),
            )
            .unwrap();
        }
        let queue = OperationQueue::open(&path).unwrap();
        let mut result = execute_fault_move(&queue).unwrap();
        if !drift {
            assert!(matches!(
                result,
                QueueExecutionResult::AwaitingConfirmation { .. }
            ));
            let snapshot = queue.snapshot();
            let entry = &snapshot.entries[0];
            let attention = entry.attention.as_ref().unwrap();
            assert!(matches!(
                attention,
                doppeldecker_core::QueueAttention::BestEffortCommitDowngrade { .. }
            ));
            assert_eq!(
                std::fs::read_dir(case.remote.path().join("target"))
                    .unwrap()
                    .count(),
                0
            );
            queue
                .approve_attention_effects(&entry.operation_id, attention, true)
                .unwrap();
            result = execute_fault_move(&queue).unwrap();
        }
        if drift {
            assert!(matches!(
                result,
                QueueExecutionResult::AwaitingConfirmation { .. }
            ));
            assert!(matches!(
                queue.snapshot().entries[0].attention,
                Some(doppeldecker_core::QueueAttention::SourceTreeChanged { .. })
            ));
            assert_eq!(
                std::fs::read_dir(case.remote.path().join("target"))
                    .unwrap()
                    .count(),
                0
            );
        } else {
            let QueueExecutionResult::File(stats) = result else {
                panic!("{result:?}");
            };
            assert_eq!((stats.files, stats.skipped), (1, 2));
            assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
            assert_eq!(
                std::fs::read(case.remote.path().join("target/good.txt")).unwrap(),
                b"equal"
            );
        }
    }
}

#[test]
fn ftp_namensgruppen_queue_neue_baumgruppe_verlangt_bestaetigung_vor_zielwirkung() {
    let case = FtpNameGroupCase::new();
    let tree = case.source.join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("x"), b"urspruenglich").unwrap();
    let operation = case.operation(&["tree"]);
    assert!(matches!(
        &operation,
        QueueOperation::ProviderFile {
            source_tree_expected: None,
            ..
        }
    ));
    let queue_dir = private_area();
    let queue_path = std::fs::canonicalize(queue_dir.path())
        .unwrap()
        .join("queue.json");
    let queue = OperationQueue::open(&queue_path).unwrap();
    queue.enqueue(operation).unwrap();
    drop(queue);
    std::fs::write(tree.join("X"), b"neu").unwrap();
    let distinct_children = std::fs::read_dir(&tree).unwrap().count() == 2;
    #[cfg(target_os = "linux")]
    assert!(
        distinct_children,
        "Linuxprobe braucht zwei echte Case-Einträge"
    );
    if !distinct_children {
        assert!(case
            .contexts(&["tree"])
            .0
            .source_vfs
            .interactive_source_tree()
            .unwrap()
            .is_none());
        eprintln!("Baumgruppen-Drift braucht ein case-sensitives Dateisystem; bisherige None-Sicht bleibt erhalten");
        return;
    }
    let queue = OperationQueue::open(&queue_path).unwrap();
    assert!(matches!(
        execute_fault_move(&queue).unwrap(),
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    let snapshot = queue.snapshot();
    let Some(doppeldecker_core::QueueAttention::SourceTreeChanged {
        current_tree: Some(current_tree),
        ..
    }) = &snapshot.entries[0].attention
    else {
        panic!("{:?}", snapshot.entries[0].attention);
    };
    assert_eq!(
        (
            current_tree.entries,
            current_tree.directories,
            current_tree.files,
            current_tree.omitted.len()
        ),
        (3, 1, 0, 2)
    );
    assert_eq!(
        std::fs::read_dir(case.remote.path().join("target"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(std::fs::read(tree.join("x")).unwrap(), b"urspruenglich");
    assert_eq!(std::fs::read(tree.join("X")).unwrap(), b"neu");
}

#[test]
fn ftp_namensgruppen_exaktes_duplikat_bleibt_ungueltig_und_andere_klassen_unveraendert() {
    let case = FtpNameGroupCase::new();
    for name in ["a", "A"] {
        std::fs::write(case.source.join(name), b"equal").unwrap();
    }
    let source = ProviderEndpoint::local(&case.source).unwrap();
    let target = ProviderEndpoint::ftps(case.vfs.clone(), Path::new("/target")).unwrap();
    let file = case.source.join("a");
    let error = source
        .interactive_file_contexts(&target, &[file.clone(), file], false, false, &Cancel::new())
        .err()
        .unwrap();
    assert_eq!(error_code(&error), ErrorCode::InvalidInput);
    for (move_source, overwrite) in [(true, false), (false, true)] {
        let contexts = source
            .interactive_file_contexts(
                &target,
                &[case.source.join("a"), case.source.join("A")],
                move_source,
                overwrite,
                &Cancel::new(),
            )
            .unwrap();
        assert!(contexts
            .source_vfs
            .interactive_source_tree()
            .unwrap()
            .is_none());
    }
    assert_eq!(
        std::fs::read_dir(case.remote.path().join("target"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn ftp_namensgruppen_explizites_tls_behaelt_gruppenbilanz_ohne_klartextfallback() {
    let case = FtpNameGroupCase::new_transport(true);
    for name in ["a", "A", "good.txt"] {
        std::fs::write(case.source.join(name), name).unwrap();
    }
    let (contexts, items) = case.contexts(&["a", "A", "good.txt"]);
    let tree = contexts
        .source_vfs
        .interactive_source_tree()
        .unwrap()
        .unwrap();
    assert_eq!((tree.files, tree.omitted.len()), (1, 2));
    let stats = case.copy(&contexts, &items, &mut |_| {}).unwrap();
    assert_eq!((stats.files, stats.skipped), (1, 2));
    assert_eq!(
        case.vfs.identity().transport_security,
        FtpTransportSecurity::ExplicitTls
    );
    assert_eq!(
        std::fs::read(case.remote.path().join("target/good.txt")).unwrap(),
        b"good.txt"
    );
    assert_eq!(
        std::fs::read_dir(case.remote.path().join("target"))
            .unwrap()
            .count(),
        1
    );
}

struct FtpDownloadCase {
    port: u16,
    control: Arc<FaultControl>,
    target: tempfile::TempDir,
    vfs: Arc<FtpsVfs>,
}
impl FtpDownloadCase {
    fn new(names: &[&str]) -> Self {
        let (port, control) = fault_ftp_server(names);
        let vfs = Arc::new(FtpsVfs::connect(fault_config(port), Arc::new(TestSecrets)).unwrap());
        Self {
            port,
            control,
            target: private_area(),
            vfs,
        }
    }
    fn target_path(&self) -> PathBuf {
        self.target.path().canonicalize().unwrap()
    }
    fn contexts(
        &self,
        names: &[&str],
        cancel: &Cancel,
    ) -> (doppeldecker_core::InteractiveFileContexts, Vec<PathBuf>) {
        let source = ProviderEndpoint::ftps(self.vfs.clone(), Path::new("/")).unwrap();
        let target = ProviderEndpoint::local(&self.target_path()).unwrap();
        let items = names
            .iter()
            .map(|name| Path::new("/").join(name))
            .collect::<Vec<_>>();
        let contexts = source
            .interactive_file_contexts(&target, &items, false, false, cancel)
            .unwrap();
        (contexts, items)
    }
    fn copy(
        &self,
        contexts: &doppeldecker_core::InteractiveFileContexts,
        items: &[PathBuf],
        cancel: &Cancel,
        progress: &mut dyn FnMut(&doppeldecker_core::Progress),
    ) -> std::io::Result<doppeldecker_core::fileops::OpStats> {
        doppeldecker_core::fileops::copy_items(
            doppeldecker_core::fileops::FileOperationRequest {
                src: contexts.source_vfs.as_ref(),
                dst: contexts.target_vfs.as_ref(),
                items,
                dst_dir: &self.target_path(),
                policy: doppeldecker_core::fileops::OverwritePolicy::Skip,
                sandbox: &None,
            },
            cancel,
            progress,
        )
    }
    fn operation(&self, names: &[&str]) -> QueueOperation {
        let (contexts, _) = self.contexts(names, &Cancel::new());
        QueueOperation::ProviderFile {
            kind: FileQueueKind::Copy,
            source: Box::new(BidirectionalQueueEndpoint::Ftps {
                connection: Box::new(fault_config(self.port)),
                root: "/".into(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::Local {
                root: self.target_path().to_string_lossy().into(),
            }),
            items: names.iter().map(|name| format!("/{name}")).collect(),
            destination: self.target_path().to_string_lossy().into(),
            overwrite: false,
            expected_conflicts: vec![],
            source_tree_expected: contexts
                .source_vfs
                .interactive_source_tree()
                .unwrap()
                .map(Box::new),
            expected_conflict_proofs: vec![],
            keep_both_targets: vec![],
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: None,
            replace_best_effort_expected: None,
            remove_source_best_effort_expected: None,
        }
    }
}

#[test]
fn ftp_download_gruppen_direkt_case_nfc_und_unabhaengige_datei() {
    let names = ["a", "A", "é", "e\u{301}", "good"];
    let case = FtpDownloadCase::new(&names);
    {
        let mut fs = case.control.fs.lock().unwrap();
        fs.get_mut("/").unwrap().insert("a".into(), true);
        fs.insert("/a".into(), [("hidden".into(), false)].into());
    }
    let before = case.control.fs.lock().unwrap().clone();
    let (contexts, items) = case.contexts(&names, &Cancel::new());
    let tree = contexts
        .source_vfs
        .interactive_source_tree()
        .unwrap()
        .unwrap();
    assert_eq!((tree.entries, tree.files, tree.omitted.len()), (5, 1, 4));
    assert!(contexts.source_vfs.interactive_additive_only());
    assert!(contexts.source_vfs.open_write(Path::new("/good")).is_err());
    let stats = case
        .copy(&contexts, &items, &Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!((stats.files, stats.skipped), (1, 4));
    assert_eq!(
        std::fs::read(case.target_path().join("good")).unwrap(),
        b"data"
    );
    assert_eq!(std::fs::read_dir(case.target_path()).unwrap().count(), 1);
    assert_eq!(*case.control.retr_paths.lock().unwrap(), ["/good"]);
    assert!(!case
        .control
        .listings
        .lock()
        .unwrap()
        .iter()
        .any(|path| path == "/a"));
    assert_eq!(*case.control.fs.lock().unwrap(), before);
    assert!(case.control.mutations.lock().unwrap().is_empty());
}

#[test]
fn ftp_download_gruppen_baum_laesst_ordner_ohne_traversierung_aus() {
    let case = FtpDownloadCase::new(&[]);
    {
        let mut fs = case.control.fs.lock().unwrap();
        fs.get_mut("/").unwrap().insert("tree".into(), true);
        fs.insert(
            "/tree".into(),
            [
                ("a".into(), true),
                ("A".into(), false),
                ("é".into(), true),
                ("e\u{301}".into(), false),
                ("sub".into(), true),
            ]
            .into(),
        );
        fs.insert("/tree/a".into(), [("secret".into(), false)].into());
        fs.insert("/tree/é".into(), [("secret".into(), false)].into());
        fs.insert("/tree/sub".into(), [("good".into(), false)].into());
    }
    let before = case.control.fs.lock().unwrap().clone();
    let (contexts, items) = case.contexts(&["tree"], &Cancel::new());
    let stats = case
        .copy(&contexts, &items, &Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!((stats.files, stats.skipped), (1, 4));
    assert_eq!(stats.source_tree.as_ref().unwrap().directories, 2);
    assert_eq!(
        std::fs::read(case.target_path().join("tree/sub/good")).unwrap(),
        b"data"
    );
    assert_eq!(
        std::fs::read_dir(case.target_path().join("tree"))
            .unwrap()
            .count(),
        1
    );
    assert!(!case
        .control
        .listings
        .lock()
        .unwrap()
        .iter()
        .any(|path| path == "/tree/a" || path == "/tree/é"));
    assert_eq!(*case.control.retr_paths.lock().unwrap(), ["/tree/sub/good"]);
    assert_eq!(*case.control.fs.lock().unwrap(), before);
    assert!(case.control.mutations.lock().unwrap().is_empty());
}

#[test]
fn ftp_download_gruppen_ausgelassene_fakten_driften_vor_und_nach_erstem_commit() {
    for after_first in [false, true] {
        let case = FtpDownloadCase::new(&["a", "A", "first", "second"]);
        let (contexts, items) = case.contexts(&["a", "A", "first", "second"], &Cancel::new());
        let change = || {
            case.control
                .mtime_overrides
                .lock()
                .unwrap()
                .insert("/a".into(), "20260728060000.123456".into());
        };
        if !after_first {
            change();
        }
        let result = case.copy(&contexts, &items, &Cancel::new(), &mut |progress| {
            if after_first && progress.files_done == 1 {
                change();
            }
        });
        assert!(matches!(result, Err(error) if error_code(&error) == ErrorCode::PlanDrift));
        assert_eq!(case.target_path().join("first").exists(), after_first);
        assert!(!case.target_path().join("second").exists());
        assert!(case.control.mutations.lock().unwrap().is_empty());
    }
}

#[test]
fn ftp_download_gruppen_streamlaenge_abbruch_und_quellreadback_vor_commit() {
    for mode in [
        "short",
        "long",
        "cancel",
        "drift",
        "completion",
        "reconnect",
    ] {
        let case = FtpDownloadCase::new(&["a", "A", "good"]);
        let cancel = Cancel::new();
        if mode == "completion" {
            case.control
                .drop_retr_completion
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let (contexts, items) = case.contexts(&["a", "A", "good"], &cancel);
        let control = Arc::downgrade(&case.control);
        let flag = cancel.clone();
        *case.control.retr_observer.lock().unwrap() = Some(Arc::new(move |_| {
            let control = control.upgrade().unwrap();
            match mode {
                "cancel" => flag.cancel(),
                "drift" => {
                    control
                        .mtime_overrides
                        .lock()
                        .unwrap()
                        .insert("/a".into(), "20260728060000.1".into());
                }
                _ => {}
            }
        }));
        // Die Größe war bei der Vorbereitung 4; der Datenweg liefert jetzt eine
        // andere Länge. MLSD bleibt absichtlich unverändert (RETR-Override unten).
        if mode == "short" || mode == "long" {
            *case.control.retr_payload_override.lock().unwrap() = Some(if mode == "short" {
                b"abc".to_vec()
            } else {
                b"abcde".to_vec()
            });
        }
        let mut reconnected = false;
        let result = case.copy(&contexts, &items, &cancel, &mut |progress| {
            if mode == "reconnect" && progress.bytes_done > 0 && !reconnected {
                case.vfs.reconnect().unwrap();
                reconnected = true;
            }
        });
        if mode == "reconnect" {
            assert!(
                reconnected,
                "Reconnect erst nach tatsächlich gelesenen Bytes"
            );
        }
        assert!(
            result.is_err() || result.as_ref().is_ok_and(|stats| stats.cancelled),
            "{mode}: {result:?}"
        );
        assert!(!case.target_path().join("good").exists(), "{mode}");
        assert_eq!(
            std::fs::read_dir(case.target_path()).unwrap().count(),
            0,
            "{mode}"
        );
        assert_eq!(
            case.control.retr_paths.lock().unwrap().len(),
            1,
            "{mode}: kein Blindretry"
        );
        assert!(case.control.mutations.lock().unwrap().is_empty());
    }
}

#[test]
fn ftp_download_gruppen_queue_reopen_stabil_und_neue_gruppe_stoppt() {
    for drift in [false, true] {
        let case = FtpDownloadCase::new(&[]);
        {
            let mut fs = case.control.fs.lock().unwrap();
            fs.get_mut("/").unwrap().insert("tree".into(), true);
            fs.insert("/tree".into(), [("x".into(), false)].into());
        }
        let queue_dir = private_area();
        let queue_path = queue_dir.path().canonicalize().unwrap().join("queue.json");
        let queue = OperationQueue::open(&queue_path).unwrap();
        queue.enqueue(case.operation(&["tree"])).unwrap();
        drop(queue);
        if drift {
            case.control
                .fs
                .lock()
                .unwrap()
                .get_mut("/tree")
                .unwrap()
                .insert("X".into(), false);
        }
        let queue = OperationQueue::open(&queue_path).unwrap();
        let result = execute_fault_move(&queue).unwrap();
        if drift {
            assert!(matches!(
                result,
                QueueExecutionResult::AwaitingConfirmation { .. }
            ));
            assert!(matches!(
                queue.snapshot().entries[0].attention,
                Some(doppeldecker_core::QueueAttention::SourceTreeChanged { .. })
            ));
            assert_eq!(std::fs::read_dir(case.target_path()).unwrap().count(), 0);
            assert!(case.control.retr_paths.lock().unwrap().is_empty());
        } else {
            let QueueExecutionResult::File(stats) = result else {
                panic!("{result:?}");
            };
            assert_eq!(stats.files, 1);
            assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
            assert_eq!(
                std::fs::read(case.target_path().join("tree/x")).unwrap(),
                b"data"
            );
        }
        assert!(case.control.mutations.lock().unwrap().is_empty());
    }
}

#[test]
fn ftp_download_gruppen_unique_und_aktive_generation_sind_gebunden() {
    let case = FtpDownloadCase::new(&["a", "A", "good"]);
    case.control
        .unique_overrides
        .lock()
        .unwrap()
        .insert("/a".into(), "erstes-objekt".into());
    let (contexts, _) = case.contexts(&["a", "A", "good"], &Cancel::new());
    let first = contexts
        .source_vfs
        .interactive_source_tree()
        .unwrap()
        .unwrap();
    case.vfs.reconnect().unwrap();
    assert_eq!(
        error_code(&contexts.source_vfs.interactive_source_tree().unwrap_err()),
        ErrorCode::PlanDrift
    );
    let (fresh, _) = case.contexts(&["a", "A", "good"], &Cancel::new());
    assert_eq!(
        fresh.source_vfs.interactive_source_tree().unwrap().unwrap(),
        first
    );
    case.control
        .unique_overrides
        .lock()
        .unwrap()
        .insert("/a".into(), "zweites-objekt".into());
    assert_eq!(
        error_code(&fresh.source_vfs.stat(Path::new("/good")).unwrap_err()),
        ErrorCode::PlanDrift
    );
    assert_eq!(std::fs::read_dir(case.target_path()).unwrap().count(), 0);
    assert!(case.control.retr_paths.lock().unwrap().is_empty());
}

#[test]
fn ftp_download_gruppen_queue_begrenzt_baumbelege_auf_additive_downloads() {
    let case = FtpDownloadCase::new(&["a", "A", "good"]);
    let base = case.operation(&["a", "A", "good"]);
    let queue_dir = private_area();
    let queue = OperationQueue::open(queue_dir.path().join("queue.json")).unwrap();
    queue.enqueue(base.clone()).unwrap();
    for mode in ["move", "overwrite", "remote_target", "foreign_omission"] {
        let mut invalid = base.clone();
        let QueueOperation::ProviderFile {
            kind,
            target,
            destination,
            overwrite,
            source_tree_expected,
            ..
        } = &mut invalid
        else {
            unreachable!()
        };
        match mode {
            "move" => *kind = FileQueueKind::Move,
            "overwrite" => *overwrite = true,
            "remote_target" => {
                **target = BidirectionalQueueEndpoint::Ftps {
                    connection: Box::new(fault_config(case.port)),
                    root: "/target".into(),
                };
                *destination = "/target".into();
            }
            "foreign_omission" => {
                source_tree_expected.as_mut().unwrap().omitted[0].path = "/fremd".into()
            }
            _ => unreachable!(),
        }
        assert!(queue.enqueue(invalid).is_err(), "{mode}");
    }
    assert_eq!(queue.snapshot().entries.len(), 1);
    assert!(case.control.mutations.lock().unwrap().is_empty());
}

#[test]
fn ftp_download_gruppen_auslassungen_binden_alle_beobachteten_vorfahren() {
    let case = FtpDownloadCase::new(&["good"]);
    {
        let mut fs = case.control.fs.lock().unwrap();
        fs.get_mut("/").unwrap().insert("tree".into(), true);
        fs.insert("/tree".into(), [("branch".into(), true)].into());
        fs.insert("/tree/branch".into(), [("sub".into(), true)].into());
        fs.insert(
            "/tree/branch/sub".into(),
            [("a".into(), false), ("A".into(), false)].into(),
        );
    }
    let (contexts, items) = case.contexts(&["good", "tree"], &Cancel::new());
    // Das unmittelbare Elternlisting der Gruppe bleibt identisch. Der geänderte
    // Vorfahr muss trotzdem vor der unabhängigen /good-Wirkung auffallen.
    case.control
        .mtime_overrides
        .lock()
        .unwrap()
        .insert("/tree/branch".into(), "20260728060000.9".into());
    let error = case
        .copy(&contexts, &items, &Cancel::new(), &mut |_| {})
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::PlanDrift);
    assert_eq!(std::fs::read_dir(case.target_path()).unwrap().count(), 0);
    assert!(case.control.retr_paths.lock().unwrap().is_empty());
}

#[test]
fn ftp_download_gruppen_andere_quellklassen_und_ungueltige_auswahl_bleiben_getrennt() {
    let case = FtpDownloadCase::new(&["a", "A", "good"]);
    let (owned, _config, _owner) = owned_vfs(fault_config(case.port));
    let source = ProviderEndpoint::ftps(Arc::new(owned), Path::new("/")).unwrap();
    let target = ProviderEndpoint::local(&case.target_path()).unwrap();
    let items = [PathBuf::from("/a"), PathBuf::from("/A")];
    for (move_source, overwrite) in [(true, false), (false, true)] {
        let context = source
            .interactive_file_contexts(&target, &items, move_source, overwrite, &Cancel::new())
            .unwrap();
        assert!(!context.source_vfs.interactive_additive_only());
        assert!(context
            .source_vfs
            .interactive_source_tree()
            .unwrap()
            .is_none());
    }
    for original in [
        source.file_context().unwrap().0,
        source
            .interactive_file_read_context(&items, &Cancel::new())
            .unwrap()
            .0,
    ] {
        assert!(!original.interactive_additive_only());
        assert!(original.interactive_source_tree().unwrap().is_none());
    }
    assert!(source
        .interactive_file_read_limitations()
        .iter()
        .any(|message| message.contains("kein FSTAT")
            && message.contains("Additive FTP-/FTPS-Kopien")));
    let (_, unique) = case.contexts(&["good"], &Cancel::new());
    let context = source
        .interactive_file_contexts(&target, &unique, false, false, &Cancel::new())
        .unwrap();
    assert!(context
        .source_vfs
        .interactive_source_tree()
        .unwrap()
        .is_none());
    let before = case.control.listings.lock().unwrap().len();
    assert!(source
        .interactive_file_contexts(
            &target,
            &[items[0].clone(), items[0].clone()],
            false,
            false,
            &Cancel::new()
        )
        .is_err());
    assert!(source
        .interactive_file_contexts(
            &target,
            &[PathBuf::from("/nested/file")],
            false,
            false,
            &Cancel::new()
        )
        .is_err());
    assert_eq!(case.control.listings.lock().unwrap().len(), before);
    assert!(case.control.mutations.lock().unwrap().is_empty());
    assert!(case.control.retr_paths.lock().unwrap().is_empty());
}

#[test]
fn ftp_download_gruppen_tiefer_baum_behaelt_vorhandenen_pfadvertrag() {
    let case = FtpDownloadCase::new(&[]);
    let mut leaf = PathBuf::from("/");
    {
        let mut fs = case.control.fs.lock().unwrap();
        for _ in 0..70 {
            fs.get_mut(leaf.to_str().unwrap())
                .unwrap()
                .insert("d".into(), true);
            leaf.push("d");
            fs.insert(leaf.to_str().unwrap().into(), Default::default());
        }
        fs.get_mut(leaf.to_str().unwrap())
            .unwrap()
            .insert("leaf".into(), false);
        leaf.push("leaf");
    }
    assert!(leaf.components().count() > 64);
    let target_file = case.target_path().join(leaf.strip_prefix("/").unwrap());
    assert!(
        target_file.as_os_str().len() < 512,
        "Lokaler Pfad bleibt unter OS-PATH_MAX"
    );
    let before = case.control.fs.lock().unwrap().clone();
    let (contexts, items) = case.contexts(&["d"], &Cancel::new());
    let tree = contexts
        .source_vfs
        .interactive_source_tree()
        .unwrap()
        .unwrap();
    assert_eq!(
        (tree.entries, tree.directories, tree.files, tree.bytes),
        (71, 70, 1, 4)
    );
    assert!(tree.omitted.is_empty());
    let stats = case
        .copy(&contexts, &items, &Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!((stats.files, stats.bytes, stats.skipped), (1, 4, 0));
    assert_eq!(std::fs::read(target_file).unwrap(), b"data");
    let requests = case.control.listings.lock().unwrap().len();
    assert!(
        requests <= 12 * 71,
        "70 ausgewählte Ordner mit einem Blatt benötigen linear viele MLSD-Requests: {requests}"
    );
    eprintln!("70 ausgewählte Ordner, ein Blatt: {requests} vollständige MLSD-Requests");
    assert_eq!(
        case.control.retr_paths.lock().unwrap().as_slice(),
        [leaf.to_str().unwrap()]
    );
    assert_eq!(*case.control.fs.lock().unwrap(), before);
    assert!(case.control.mutations.lock().unwrap().is_empty());
}

#[test]
fn ftp_download_gruppen_ungewaehlte_geschwister_sperren_keine_unabhaengige_kopie() {
    let case = FtpDownloadCase::new(&["good", "neighbor"]);
    let (contexts, items) = case.contexts(&["good"], &Cancel::new());
    case.control
        .mtime_overrides
        .lock()
        .unwrap()
        .insert("/neighbor".into(), "20260728060000.77".into());
    let stats = case
        .copy(&contexts, &items, &Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!((stats.files, stats.bytes, stats.skipped), (1, 4, 0));
    assert_eq!(
        std::fs::read(case.target_path().join("good")).unwrap(),
        b"data"
    );
    assert_eq!(*case.control.retr_paths.lock().unwrap(), ["/good"]);
    assert!(case.control.mutations.lock().unwrap().is_empty());
}

#[test]
fn ftp_download_gruppen_digest_bindet_pane_vorfahren_ohne_ungewaehlte_geschwister() {
    let case = FtpDownloadCase::new(&["outside"]);
    {
        let mut fs = case.control.fs.lock().unwrap();
        fs.get_mut("/").unwrap().insert("pane".into(), true);
        fs.insert(
            "/pane".into(),
            [
                ("good".into(), false),
                ("a".into(), false),
                ("A".into(), false),
                ("neighbor".into(), false),
            ]
            .into(),
        );
    }
    let source = ProviderEndpoint::ftps(case.vfs.clone(), Path::new("/pane")).unwrap();
    let target = ProviderEndpoint::local(&case.target_path()).unwrap();
    let items = ["good", "a", "A"].map(|name| Path::new("/pane").join(name));
    let contexts = source
        .interactive_file_contexts(&target, &items, false, false, &Cancel::new())
        .unwrap();
    let expected = contexts
        .source_vfs
        .interactive_source_tree()
        .unwrap()
        .unwrap();
    for path in ["/outside", "/pane/neighbor"] {
        case.control
            .mtime_overrides
            .lock()
            .unwrap()
            .insert(path.into(), "20260728060000.77".into());
    }
    let fresh = source
        .interactive_file_contexts(&target, &items, false, false, &Cancel::new())
        .unwrap();
    assert_eq!(
        fresh.source_vfs.interactive_source_tree().unwrap().unwrap(),
        expected
    );
    let stats = case
        .copy(&contexts, &items, &Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!((stats.files, stats.bytes, stats.skipped), (1, 4, 2));
    assert_eq!(
        std::fs::read(case.target_path().join("good")).unwrap(),
        b"data"
    );
    assert_eq!(*case.control.retr_paths.lock().unwrap(), ["/pane/good"]);
    // Der benötigte Pane-Vorfahr bleibt trotz der engeren Geschwisterbindung Teil
    // des Belegs und stoppt auch eine unabhängige verbleibende Wirkung.
    case.control
        .mtime_overrides
        .lock()
        .unwrap()
        .insert("/pane".into(), "20260728060000.88".into());
    assert_eq!(
        error_code(&fresh.source_vfs.stat(Path::new("/pane/good")).unwrap_err()),
        ErrorCode::PlanDrift
    );
    assert!(case.control.mutations.lock().unwrap().is_empty());
}

#[test]
fn ftp_download_crash_reopen_erhaelt_privaten_tempintent_ohne_neustart() {
    use std::os::unix::fs::PermissionsExt;
    let case = FtpDownloadCase::new(&["good"]);
    case.control
        .contents
        .lock()
        .unwrap()
        .insert("/good".into(), vec![0x63; 4 * 1024 * 1024]);
    let target = case.target_path();
    let area = tempfile::tempdir().unwrap();
    std::fs::set_permissions(area.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let journal = area.path().join("queue.json");
    let queue = OperationQueue::open(&journal).unwrap();
    queue.enqueue(case.operation(&["good"])).unwrap();
    drop(queue);
    let binary = std::env::current_exe().unwrap();
    let list = std::process::Command::new(&binary)
        .arg("--list")
        .output()
        .unwrap();
    assert!(list.status.success());
    assert_eq!(
        String::from_utf8(list.stdout)
            .unwrap()
            .lines()
            .filter(|line| *line == "ftp_download_temp_crash_child: test")
            .count(),
        1
    );
    let status = std::process::Command::new(binary)
        .args(["--exact", "ftp_download_temp_crash_child", "--ignored"])
        .env("DOPPELDECKER_FTP_DOWNLOAD_CRASH_QUEUE", &journal)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(91));
    let queue = OperationQueue::open(&journal).unwrap();
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.entries[0].state, QueueState::Interrupted);
    assert!(!snapshot.entries[0].temp_recovery_paths.is_empty());
    assert!(snapshot.entries[0]
        .temp_recovery_paths
        .iter()
        .any(|path| Path::new(path).starts_with(&target) && Path::new(path).is_file()));
    assert!(!target.join("good").exists());
    assert!(queue.claim_next().unwrap().is_none());
    assert!(case.control.mutations.lock().unwrap().is_empty());
}

#[test]
#[ignore = "wird ausschließlich vom begrenzten Prozessabbruch-Gegenlauf gestartet"]
fn ftp_download_temp_crash_child() {
    let journal = PathBuf::from(
        std::env::var_os("DOPPELDECKER_FTP_DOWNLOAD_CRASH_QUEUE")
            .expect("eigener Testqueuepfad fehlt"),
    );
    let queue = OperationQueue::open(journal).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let _ = execute_provider_file_queue_entry(
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
        &mut |progress| {
            if progress.bytes_done > 0 {
                std::process::exit(91);
            }
        },
    );
    panic!("Kindprozess erreichte keinen Teiltransfer");
}

#[test]
fn ftp_stapelkette_antwortverlust_gibt_keinen_abhaengigen_namen_frei() {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    let (port, control) = fault_ftp_server(&["a", "aa", "aaa", "b"]);
    let (vfs, _area, _queue) = owned_vfs(fault_config(port));
    let endpoint = ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap();
    let plan = PreparedFtpBatchRename::prepare(
        &endpoint,
        &["a".into(), "aa".into(), "aaa".into(), "b".into()],
        &BatchRenameOptions {
            find: String::new(),
            replacement: String::new(),
            prefix: "a".into(),
            suffix: String::new(),
            preserve_extension: true,
            trim_outer_spaces: false,
            numbering: None,
        },
        &Cancel::new(),
    )
    .unwrap();
    assert!(plan
        .rows()
        .iter()
        .all(|row| row.status == BatchRenameStatus::Ready));
    *control.fault.lock().unwrap() = Some(RntoFault::DropAfterMutation);
    *control.fault_destination.lock().unwrap() = Some("/aaaa".into());
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    assert_eq!(
        (
            report.renamed,
            report.outcome_unclear,
            report.failed_unchanged
        ),
        (0, 1, 3),
        "{:?}",
        report.rows
    );
    assert_eq!(report.rows[2].status, BatchRenameStatus::OutcomeUnclear);
    assert_eq!(
        control
            .mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("RNTO "))
            .cloned()
            .collect::<Vec<_>>(),
        ["RNTO /aaaa"]
    );
    let fs = control.fs.lock().unwrap();
    assert_eq!(
        fs["/"].keys().map(String::as_str).collect::<Vec<_>>(),
        ["a", "aa", "aaaa", "b"]
    );
    assert!(control.retr_paths.lock().unwrap().is_empty());
}

#[test]
fn ftp_stapelkette_kollision_drift_profil_und_abbruch_stoppen_passende_zeilen() {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    for change in [
        "success",
        "collision",
        "source",
        "parent",
        "ancestor",
        "session",
        "profile",
        "cancel",
    ] {
        let (port, control) = fault_ftp_server(&[]);
        {
            let mut fs = control.fs.lock().unwrap();
            fs.get_mut("/").unwrap().insert("outer".into(), true);
            fs.insert("/outer".into(), [("pane".into(), true)].into());
            fs.insert(
                "/outer/pane".into(),
                [
                    ("a".into(), false),
                    ("aa".into(), false),
                    ("aaa".into(), false),
                    ("b".into(), false),
                ]
                .into(),
            );
        }
        let (vfs, _area, _queue) = owned_vfs(fault_config(port));
        let provider = Arc::new(vfs);
        let endpoint = ProviderEndpoint::ftps(provider.clone(), Path::new("/outer/pane")).unwrap();
        let plan = PreparedFtpBatchRename::prepare(
            &endpoint,
            &["a".into(), "aa".into(), "aaa".into(), "b".into()],
            &BatchRenameOptions {
                find: String::new(),
                replacement: String::new(),
                prefix: "a".into(),
                suffix: String::new(),
                preserve_extension: true,
                trim_outer_spaces: false,
                numbering: None,
            },
            &Cancel::new(),
        )
        .unwrap();
        assert!(plan
            .rows()
            .iter()
            .all(|row| row.status == BatchRenameStatus::Ready));
        assert!(control.mutations.lock().unwrap().is_empty());
        // Eigene Pane-Zeitänderung ist erwartbar und wird nach Erfolg übernommen.
        let weak = Arc::downgrade(&control);
        *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
            if command.starts_with("RNTO ") {
                weak.upgrade()
                    .unwrap()
                    .mtime_overrides
                    .lock()
                    .unwrap()
                    .insert("/outer/pane".into(), "20260728060001".into());
            }
        }));
        let cancel = Cancel::new();
        let mut checks = 0;
        let report = plan.execute(
            &cancel,
            &mut || {
                checks += 1;
                if change == "profile" && checks == 2 {
                    return Err(doppeldecker_core::coded_error(
                        std::io::ErrorKind::Interrupted,
                        ErrorCode::PlanDrift,
                        "Testprofil geändert",
                    ));
                }
                Ok(())
            },
            &mut |done, _| {
                if done == 1 {
                    match change {
                        "collision" => {
                            control
                                .fs
                                .lock()
                                .unwrap()
                                .get_mut("/outer/pane")
                                .unwrap()
                                .insert("aaa".into(), false);
                        }
                        "source" => {
                            control
                                .contents
                                .lock()
                                .unwrap()
                                .insert("/outer/pane/aa".into(), b"changed size".to_vec());
                        }
                        "parent" => {
                            control
                                .mtime_overrides
                                .lock()
                                .unwrap()
                                .insert("/outer/pane".into(), "20260728060002".into());
                        }
                        "ancestor" => {
                            control
                                .mtime_overrides
                                .lock()
                                .unwrap()
                                .insert("/outer".into(), "20260728060002".into());
                        }
                        "session" => provider.reconnect().unwrap(),
                        "cancel" => cancel.cancel(),
                        _ => {}
                    }
                }
            },
        );
        let commands = control
            .mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("RNTO "))
            .cloned()
            .collect::<Vec<_>>();
        match change {
            "success" => {
                assert_eq!(report.renamed, 4, "{:?}", report.rows);
                assert_eq!(
                    commands,
                    [
                        "RNTO /outer/pane/aaaa",
                        "RNTO /outer/pane/ab",
                        "RNTO /outer/pane/aaa",
                        "RNTO /outer/pane/aa"
                    ]
                );
            }
            "collision" => {
                assert_eq!(
                    (report.renamed, report.failed_unchanged),
                    (2, 2),
                    "{:?}",
                    report.rows
                );
                assert_eq!(report.rows[1].code, Some(ErrorCode::AlreadyExists));
                assert_eq!(commands.len(), 2);
            }
            "source" => {
                assert_eq!(
                    (report.renamed, report.failed_unchanged),
                    (2, 2),
                    "{:?}",
                    report.rows
                );
                assert_eq!(report.rows[1].code, Some(ErrorCode::PlanDrift));
                assert_eq!(commands.len(), 2);
            }
            "cancel" => {
                assert_eq!((report.renamed, report.cancelled), (1, 3));
                assert_eq!(commands.len(), 1);
            }
            _ => {
                assert_eq!(
                    (report.renamed, report.failed_unchanged),
                    (1, 3),
                    "{change}: {:?}",
                    report.rows
                );
                assert_eq!(report.rows[3].code, Some(ErrorCode::PlanDrift));
                assert_eq!(commands.len(), 1);
            }
        }
        assert!(control.retr_paths.lock().unwrap().is_empty());
    }
}

#[test]
fn ftp_stapelkette_klartext_und_explizites_tls_erhalten_auswahl_und_nummern() {
    use doppeldecker_core::batch_rename::{
        BatchRenameNumberPosition, BatchRenameNumbering, BatchRenameOptions, BatchRenameStatus,
    };
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    for tls in [false, true] {
        for numbered in [false, true] {
            let case = FtpNameGroupCase::new_transport(tls);
            let names = if numbered {
                ["a", "a_1", "a_1_2", "b"]
            } else {
                ["a", "aa", "aaa", "b"]
            };
            let target = case.remote.path().join("target");
            for name in names {
                std::fs::write(target.join(name), name).unwrap();
            }
            let endpoint = ProviderEndpoint::ftps(case.vfs.clone(), Path::new("/target")).unwrap();
            let plan = PreparedFtpBatchRename::prepare(
                &endpoint,
                &names.map(str::to_string),
                &BatchRenameOptions {
                    find: String::new(),
                    replacement: String::new(),
                    prefix: if numbered { String::new() } else { "a".into() },
                    suffix: String::new(),
                    preserve_extension: true,
                    trim_outer_spaces: false,
                    numbering: numbered.then_some(BatchRenameNumbering {
                        start: 1,
                        step: 1,
                        min_digits: 1,
                        position: BatchRenameNumberPosition::After,
                    }),
                },
                &Cancel::new(),
            )
            .unwrap();
            let expected = if numbered {
                ["a_1", "a_1_2", "a_1_2_3", "b_4"]
            } else {
                ["aa", "aaa", "aaaa", "ab"]
            };
            assert_eq!(
                plan.rows()
                    .iter()
                    .map(|row| row.old_name.as_str())
                    .collect::<Vec<_>>(),
                names
            );
            assert_eq!(
                plan.rows()
                    .iter()
                    .map(|row| row.new_name.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(plan
                .rows()
                .iter()
                .all(|row| row.status == BatchRenameStatus::Ready));
            assert_eq!(plan.execution_class(), RenameExecutionClass::BestEffort);
            let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
            assert_eq!(
                report.renamed, 4,
                "tls={tls}, numbered={numbered}: {:?}",
                report.rows
            );
            for (old, new) in names.iter().zip(expected) {
                assert_eq!(std::fs::read(target.join(new)).unwrap(), old.as_bytes());
            }
            assert_eq!(std::fs::read_dir(target).unwrap().count(), 4);
            assert_eq!(
                case.vfs.identity().transport_security,
                if tls {
                    FtpTransportSecurity::ExplicitTls
                } else {
                    FtpTransportSecurity::Plaintext
                }
            );
        }
    }
}

#[test]
fn ftp_stapelkette_beide_transporte_stoppen_bei_drift_und_lassen_kollisionen_aus() {
    use doppeldecker_core::batch_rename::BatchRenameOptions;
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    for tls in [false, true] {
        for change in [
            "collision",
            "source",
            "parent",
            "session",
            "profile",
            "cancel",
        ] {
            let case = FtpNameGroupCase::new_transport(tls);
            let target = case.remote.path().join("target");
            let names = ["a", "aa", "aaa", "b"];
            for name in names {
                std::fs::write(target.join(name), name).unwrap();
            }
            if change == "collision" {
                // Gleiche Namenslänge und Eintragszahl erhalten die beobachtbare
                // Directorygröße; die eigene Zeitänderung wird unten zurückgesetzt.
                std::fs::write(target.join("zzz"), b"late collision").unwrap();
            }
            let endpoint = ProviderEndpoint::ftps(case.vfs.clone(), Path::new("/target")).unwrap();
            let plan = PreparedFtpBatchRename::prepare(
                &endpoint,
                &names.map(str::to_string),
                &BatchRenameOptions {
                    find: String::new(),
                    replacement: String::new(),
                    prefix: "a".into(),
                    suffix: String::new(),
                    preserve_extension: true,
                    trim_outer_spaces: false,
                    numbering: None,
                },
                &Cancel::new(),
            )
            .unwrap();
            let cancel = Cancel::new();
            let mut checks = 0;
            let report = plan.execute(
                &cancel,
                &mut || {
                    checks += 1;
                    if change == "profile" && checks == 2 {
                        return Err(doppeldecker_core::coded_error(
                            std::io::ErrorKind::Interrupted,
                            ErrorCode::PlanDrift,
                            "Testprofil geändert",
                        ));
                    }
                    Ok(())
                },
                &mut |done, _| {
                    if done == 1 {
                        match change {
                            "collision" => {
                                let parent = std::fs::metadata(&target).unwrap();
                                let before = case.vfs.stat(Path::new("/target")).unwrap();
                                std::fs::rename(target.join("zzz"), target.join("aaa")).unwrap();
                                filetime::set_file_mtime(
                                    &target,
                                    filetime::FileTime::from_last_modification_time(&parent),
                                )
                                .unwrap();
                                let after = case.vfs.stat(Path::new("/target")).unwrap();
                                assert_eq!(
                                    serde_json::to_value(before).unwrap(),
                                    serde_json::to_value(after).unwrap(),
                                    "Die Kollisionsfixture muss sämtliche beobachtbaren Elternfakten erhalten"
                                );
                            }
                            "source" => std::fs::write(target.join("aa"), b"changed size").unwrap(),
                            "parent" => {
                                let before = case.vfs.stat(Path::new("/target")).unwrap();
                                filetime::set_file_mtime(
                                    &target,
                                    filetime::FileTime::from_unix_time(1_700_000_000, 0),
                                )
                                .unwrap();
                                let after = case.vfs.stat(Path::new("/target")).unwrap();
                                assert_ne!(before.mtime, after.mtime);
                            }
                            "session" => case.vfs.reconnect().unwrap(),
                            "cancel" => cancel.cancel(),
                            _ => {}
                        }
                    }
                },
            );
            match change {
                "collision" => {
                    assert_eq!(
                        (report.renamed, report.failed_unchanged),
                        (2, 2),
                        "tls={tls}, {change}: {:?}",
                        report.rows
                    );
                    assert_eq!(report.rows[1].code, Some(ErrorCode::AlreadyExists));
                    assert_eq!(
                        std::fs::read(target.join("aaa")).unwrap(),
                        b"late collision"
                    );
                    assert_eq!(std::fs::read(target.join("aa")).unwrap(), b"aa");
                }
                "source" => {
                    assert_eq!(
                        (report.renamed, report.failed_unchanged),
                        (2, 2),
                        "tls={tls}, {change}: {:?}",
                        report.rows
                    );
                    assert_eq!(report.rows[1].code, Some(ErrorCode::PlanDrift));
                    assert_eq!(std::fs::read(target.join("aa")).unwrap(), b"changed size");
                }
                "cancel" => assert_eq!((report.renamed, report.cancelled), (1, 3)),
                _ => {
                    assert_eq!(
                        (report.renamed, report.failed_unchanged),
                        (1, 3),
                        "tls={tls}, {change}: {:?}",
                        report.rows
                    );
                    assert_eq!(report.rows[3].code, Some(ErrorCode::PlanDrift));
                }
            }
            assert_eq!(std::fs::read(target.join("a")).unwrap(), b"a");
            assert_eq!(std::fs::read(target.join("aaaa")).unwrap(), b"aaa");
        }
    }
}

fn ftp_spelling_options(
    old: &str,
    new: &str,
) -> doppeldecker_core::batch_rename::BatchRenameOptions {
    doppeldecker_core::batch_rename::BatchRenameOptions {
        find: old.into(),
        replacement: String::new(),
        prefix: new.into(),
        suffix: String::new(),
        preserve_extension: false,
        trim_outer_spaces: false,
        numbering: None,
    }
}

#[test]
fn ftp_stapelschreibweise_public_prepare_und_echte_transporte() {
    use doppeldecker_core::batch_rename::BatchRenameStatus;
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    use std::os::unix::fs::MetadataExt;
    let mut scenarios = 0;
    for tls in [false, true] {
        for kind in [
            "file",
            "directory",
            "nfc_nfd",
            "nfd_nfc",
            "outside_hardlink",
        ] {
            let case = FtpNameGroupCase::new_transport(tls);
            let target = case.remote.path().join("target");
            let (old, new) = match kind {
                "nfc_nfd" => ("é.txt", "e\u{301}.txt"),
                "nfd_nfc" => ("e\u{301}.txt", "é.txt"),
                _ => ("Foo", "foo"),
            };
            if kind == "directory" {
                std::fs::create_dir(target.join(old)).unwrap();
                std::fs::write(target.join(old).join("child"), b"child-content").unwrap();
            } else {
                std::fs::write(target.join(old), b"source-content").unwrap();
            }
            if kind == "outside_hardlink" {
                std::fs::hard_link(target.join(old), target.join("outside")).unwrap();
            }
            std::fs::write(target.join("other"), b"independent").unwrap();
            let inode = std::fs::symlink_metadata(target.join(old)).unwrap().ino();
            let endpoint = ProviderEndpoint::ftps(case.vfs.clone(), Path::new("/target")).unwrap();
            let plan = PreparedFtpBatchRename::prepare(
                &endpoint,
                &[old.into(), "other".into()],
                &ftp_spelling_options(old, new),
                &Cancel::new(),
            )
            .unwrap();
            assert!(
                plan.rows()
                    .iter()
                    .all(|row| row.status == BatchRenameStatus::Ready),
                "tls={tls}/{kind}: {:?}",
                plan.rows()
            );
            assert!(plan.rows()[0].message.contains("FTP-/FTPS-Schreibweise"));
            assert_eq!(plan.execution_class(), RenameExecutionClass::BestEffort);
            let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
            assert_eq!(report.renamed, 2, "tls={tls}/{kind}: {:?}", report.rows);
            let names = std::fs::read_dir(&target)
                .unwrap()
                .map(|x| x.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            assert!(names.iter().any(|x| x == new));
            assert!(!names.iter().any(|x| x == old));
            assert_eq!(
                std::fs::symlink_metadata(target.join(new)).unwrap().ino(),
                inode
            );
            if kind == "directory" {
                assert_eq!(
                    std::fs::read(target.join(new).join("child")).unwrap(),
                    b"child-content"
                );
            } else {
                assert_eq!(std::fs::read(target.join(new)).unwrap(), b"source-content");
            }
            assert_eq!(
                std::fs::read(target.join(format!("{new}other"))).unwrap(),
                b"independent"
            );
            if kind == "outside_hardlink" {
                assert_eq!(
                    std::fs::symlink_metadata(target.join("outside"))
                        .unwrap()
                        .ino(),
                    inode
                );
                assert_eq!(
                    std::fs::read(target.join("outside")).unwrap(),
                    b"source-content"
                );
            }
            scenarios += 1;
        }
    }
    eprintln!("FTP-Schreibweisen: {scenarios} echte FTP-/TLS-Dateisystemfälle");
}

#[test]
fn ftp_stapelschreibweise_beide_transporte_gruppen_und_abbruch_drift() {
    use doppeldecker_core::batch_rename::BatchRenameStatus;
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    let mut scenarios = 0;
    for tls in [false, true] {
        for change in [
            "success",
            "foreign_alias",
            "hardlink_alias",
            "duplicate_target",
            "cancel_before",
            "cancel_after",
            "source",
            "parent",
            "session",
            "profile",
        ] {
            let case = FtpNameGroupCase::new_transport(tls);
            let target = case.remote.path().join("target");
            for name in ["Foo", "Fuzz", "other"] {
                std::fs::write(target.join(name), name).unwrap();
            }
            let alias = match change {
                "foreign_alias" => Some(
                    std::fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(target.join("foo"))
                        .map(|_| ()),
                ),
                "hardlink_alias" => {
                    Some(std::fs::hard_link(target.join("Foo"), target.join("foo")))
                }
                _ => None,
            };
            if let Some(Err(error)) = alias {
                assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
                continue;
            }
            let options = ftp_spelling_options("F", "f");
            let selected = if change == "duplicate_target" {
                std::fs::write(target.join("FooF"), b"duplicate").unwrap();
                vec!["Foo".into(), "FooF".into()]
            } else {
                vec!["Foo".into(), "Fuzz".into(), "other".into()]
            };
            let endpoint = ProviderEndpoint::ftps(case.vfs.clone(), Path::new("/target")).unwrap();
            let plan =
                PreparedFtpBatchRename::prepare(&endpoint, &selected, &options, &Cancel::new())
                    .unwrap();
            if change == "duplicate_target" {
                assert!(plan
                    .rows()
                    .iter()
                    .all(|r| r.status == BatchRenameStatus::Blocked));
                assert_eq!(
                    plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {})
                        .renamed,
                    0
                );
                scenarios += 1;
                continue;
            }
            if change.contains("alias") {
                assert_eq!(plan.rows()[0].status, BatchRenameStatus::Blocked);
                let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
                assert_eq!(report.renamed, 2, "{tls}/{change}: {:?}", report.rows);
                assert_eq!(std::fs::read(target.join("Foo")).unwrap(), b"Foo");
                assert!(std::fs::read_dir(&target)
                    .unwrap()
                    .any(|x| x.unwrap().file_name() == "foo"));
                scenarios += 1;
                continue;
            }
            assert!(plan
                .rows()
                .iter()
                .all(|r| r.status == BatchRenameStatus::Ready));
            let cancel = Cancel::new();
            if change == "cancel_before" {
                cancel.cancel();
            }
            let mut checks = 0;
            let report = plan.execute(
                &cancel,
                &mut || {
                    checks += 1;
                    if change == "profile" && checks == 3 {
                        return Err(doppeldecker_core::coded_error(
                            std::io::ErrorKind::Interrupted,
                            ErrorCode::PlanDrift,
                            "Testprofil geändert",
                        ));
                    }
                    Ok(())
                },
                &mut |done, _| {
                    // Die gewöhnliche unabhängige Zeile läuft zuerst; Foo ist Schritt2.
                    if done == 2 {
                        match change {
                            "cancel_after" => cancel.cancel(),
                            "source" => {
                                std::fs::write(target.join("Fuzz"), b"changed-size").unwrap()
                            }
                            "parent" => filetime::set_file_mtime(
                                &target,
                                filetime::FileTime::from_unix_time(1_700_000_000, 0),
                            )
                            .unwrap(),
                            "session" => case.vfs.reconnect().unwrap(),
                            _ => {}
                        }
                    }
                },
            );
            match change {
                "success" => assert_eq!(report.renamed, 3, "{:?}", report.rows),
                "cancel_before" => assert_eq!((report.renamed, report.cancelled), (0, 3)),
                "cancel_after" => assert_eq!((report.renamed, report.cancelled), (2, 1)),
                _ => {
                    assert_eq!(
                        (report.renamed, report.failed_unchanged),
                        (2, 1),
                        "{tls}/{change}: {:?}",
                        report.rows
                    );
                    assert_eq!(report.rows[1].code, Some(ErrorCode::PlanDrift));
                }
            }
            if change != "cancel_before" {
                assert_eq!(std::fs::read(target.join("foo")).unwrap(), b"Foo");
                assert_eq!(std::fs::read(target.join("fother")).unwrap(), b"other");
            }
            scenarios += 1;
        }
    }
    eprintln!("FTP-Schreibweisen: {scenarios} reale Gruppen-/Cancel-/Driftfälle, nicht darstellbare Aliasaufbauten separat");
}

#[test]
fn ftp_stapelschreibweise_ein_rnfr_rnto_und_sticky_unbekannt() {
    use doppeldecker_core::batch_rename::BatchRenameStatus;
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    for fault in [
        RntoFault::NoopAndAcknowledge,
        RntoFault::NormalizeTempAndAcknowledge,
        RntoFault::DropBeforeMutation,
        RntoFault::DropAfterMutation,
    ] {
        let (port, control) = fault_ftp_server(&["Foo", "Fuzz", "other"]);
        for name in ["Foo", "Fuzz", "other"] {
            control
                .contents
                .lock()
                .unwrap()
                .insert(format!("/{name}"), name.as_bytes().to_vec());
        }
        let (vfs, _area, _queue) = owned_vfs(fault_config(port));
        let endpoint = ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap();
        let plan = PreparedFtpBatchRename::prepare(
            &endpoint,
            &["Foo".into(), "Fuzz".into(), "other".into()],
            &ftp_spelling_options("F", "f"),
            &Cancel::new(),
        )
        .unwrap();
        assert!(plan
            .rows()
            .iter()
            .all(|r| r.status == BatchRenameStatus::Ready));
        *control.fault.lock().unwrap() = Some(fault);
        *control.fault_destination.lock().unwrap() = Some("/foo".into());
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
        assert_eq!(
            (
                report.renamed,
                report.outcome_unclear,
                report.failed_unchanged
            ),
            (1, 1, 1),
            "{:?}",
            report.rows
        );
        assert_eq!(report.rows[0].status, BatchRenameStatus::OutcomeUnclear);
        assert_eq!(report.rows[0].current_name, None);
        assert_eq!(report.rows[1].status, BatchRenameStatus::FailedUnchanged);
        let commands = control.mutations.lock().unwrap().clone();
        assert_eq!(
            commands
                .iter()
                .filter(|x| x.starts_with("RNFR "))
                .cloned()
                .collect::<Vec<_>>(),
            ["RNFR /other", "RNFR /Foo"]
        );
        assert_eq!(
            commands
                .iter()
                .filter(|x| x.starts_with("RNTO "))
                .cloned()
                .collect::<Vec<_>>(),
            ["RNTO /fother", "RNTO /foo"]
        );
        let contents = control.contents.lock().unwrap();
        assert_eq!(contents["/Fuzz"], b"Fuzz");
        assert_eq!(contents["/fother"], b"other");
        let expected = match fault {
            RntoFault::DropAfterMutation => "/foo",
            RntoFault::NormalizeTempAndAcknowledge => "/FOO",
            _ => "/Foo",
        };
        assert_eq!(contents[expected], b"Foo");
        assert!(!contents.contains_key("/fuzz"));
        assert!(control.retr_paths.lock().unwrap().is_empty());
    }
}

#[test]
fn ftp_stapelschreibweise_eigene_elternbelege_erneuern_aber_vorfahren_drift_stoppt() {
    use doppeldecker_core::batch_rename::BatchRenameStatus;
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    for ancestor_drift in [false, true] {
        let (port, control) = fault_ftp_server(&[]);
        {
            let mut fs = control.fs.lock().unwrap();
            fs.get_mut("/").unwrap().insert("outer".into(), true);
            fs.insert("/outer".into(), [("pane".into(), true)].into());
            fs.insert(
                "/outer/pane".into(),
                [
                    ("Foo".into(), false),
                    ("Fuzz".into(), false),
                    ("other".into(), false),
                ]
                .into(),
            );
        }
        let (vfs, _area, _queue) = owned_vfs(fault_config(port));
        let endpoint = ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/outer/pane")).unwrap();
        let plan = PreparedFtpBatchRename::prepare(
            &endpoint,
            &["Foo".into(), "Fuzz".into(), "other".into()],
            &ftp_spelling_options("F", "f"),
            &Cancel::new(),
        )
        .unwrap();
        assert!(plan
            .rows()
            .iter()
            .all(|r| r.status == BatchRenameStatus::Ready));
        let weak = Arc::downgrade(&control);
        *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
            if let Some(to) = command.strip_prefix("RNTO ") {
                let state = weak.upgrade().unwrap();
                let seconds = if to.ends_with("fother") {
                    1
                } else if to.ends_with("foo") {
                    2
                } else {
                    3
                };
                state
                    .mtime_overrides
                    .lock()
                    .unwrap()
                    .insert("/outer/pane".into(), format!("2026072806000{seconds}"));
                if ancestor_drift && to.ends_with("foo") {
                    state
                        .mtime_overrides
                        .lock()
                        .unwrap()
                        .insert("/outer".into(), "20260728060009".into());
                }
            }
        }));
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
        if ancestor_drift {
            assert_eq!(
                (
                    report.renamed,
                    report.outcome_unclear,
                    report.failed_unchanged
                ),
                (1, 1, 1),
                "{:?}",
                report.rows
            );
            assert_eq!(report.rows[0].status, BatchRenameStatus::OutcomeUnclear);
        } else {
            assert_eq!(report.renamed, 3, "{:?}", report.rows);
        }
        let commands = control
            .mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|x| x.starts_with("RNTO "))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(commands.len(), if ancestor_drift { 2 } else { 3 });
        assert!(control.retr_paths.lock().unwrap().is_empty());
    }
}

#[test]
fn ftp_stapelschreibweise_frische_aliasgruppe_und_abbruch_im_versuch() {
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    for late_alias in [false, true] {
        let (port, control) = fault_ftp_server(&["Foo", "Fuzz", "other"]);
        for name in ["Foo", "Fuzz", "other"] {
            control
                .contents
                .lock()
                .unwrap()
                .insert(format!("/{name}"), name.as_bytes().to_vec());
        }
        let (vfs, _area, _queue) = owned_vfs(fault_config(port));
        let endpoint = ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap();
        let plan = PreparedFtpBatchRename::prepare(
            &endpoint,
            &["Foo".into(), "Fuzz".into(), "other".into()],
            &ftp_spelling_options("F", "f"),
            &Cancel::new(),
        )
        .unwrap();
        let cancel = Cancel::new();
        if !late_alias {
            let pending = cancel.clone();
            *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
                if command == "RNFR /Foo" {
                    pending.cancel();
                }
            }));
        }
        let mut checks = 0;
        let report = plan.execute(
            &cancel,
            &mut || {
                checks += 1;
                if late_alias && checks == 2 {
                    control
                        .fs
                        .lock()
                        .unwrap()
                        .get_mut("/")
                        .unwrap()
                        .insert("foo".into(), false);
                    control
                        .contents
                        .lock()
                        .unwrap()
                        .insert("/foo".into(), b"foreign".to_vec());
                }
                Ok(())
            },
            &mut |_, _| {},
        );
        let commands = control.mutations.lock().unwrap().clone();
        let source_attempts = commands
            .iter()
            .filter(|x| x.as_str() == "RNFR /Foo")
            .count();
        assert_eq!(source_attempts, usize::from(!late_alias));
        assert!(!commands.iter().any(|x| x == "RNFR /Fuzz"));
        if late_alias {
            assert_eq!(
                (report.renamed, report.failed_unchanged),
                (1, 2),
                "{:?}",
                report.rows
            );
            assert_eq!(control.contents.lock().unwrap()["/Foo"], b"Foo");
            assert_eq!(control.contents.lock().unwrap()["/foo"], b"foreign");
        } else {
            assert_eq!(
                (report.renamed, report.cancelled),
                (2, 1),
                "{:?}",
                report.rows
            );
            assert_eq!(control.contents.lock().unwrap()["/foo"], b"Foo");
        }
        assert_eq!(control.contents.lock().unwrap()["/Fuzz"], b"Fuzz");
    }
}

#[test]
fn content_sync_ftp_und_ftps_addonly_update_mirror_ohne_starke_flags() {
    use doppeldecker_core::execute_prepared_content_sync_queue_entry;
    for tls in [false, true] {
        let case = FtpNameGroupCase::new_transport(tls);
        std::fs::create_dir_all(case.source.join("Baum/Leer")).unwrap();
        std::fs::write(case.source.join("Baum/Kind"), b"kind").unwrap();
        std::fs::write(case.source.join("Datei"), b"alpha").unwrap();
        let area = private_area();
        let queue = OperationQueue::open(area.path().join("operations-queue.json")).unwrap();
        for mode in [SyncMode::AddOnly, SyncMode::Update, SyncMode::Mirror] {
            if mode == SyncMode::Update {
                std::fs::write(case.source.join("Datei"), b"bravo").unwrap();
            }
            if mode == SyncMode::Mirror {
                std::fs::create_dir_all(case.remote.path().join("target/Alt/Unter")).unwrap();
                std::fs::write(case.remote.path().join("target/Alt/Unter/Datei"), b"alt").unwrap();
            }
            let options = SyncOptions {
                mode,
                compare: Compare::ContentHash,
                ..SyncOptions::default()
            };
            let target = ProviderEndpoint::ftps(case.vfs.clone(), Path::new("/target")).unwrap();
            let prepared = doppeldecker_core::prepare_content_sync_for_queue(
                &queue,
                ProviderEndpoint::local(&case.source).unwrap(),
                target,
                options.clone(),
                &Cancel::new(),
            )
            .unwrap();
            assert!(prepared
                .preview()
                .limitations
                .iter()
                .any(|reason| reason.contains("MFMT")));
            queue
                .enqueue(QueueOperation::ContentSync {
                    source: Box::new(BidirectionalQueueEndpoint::Local {
                        root: case.source.to_string_lossy().into_owned(),
                    }),
                    target: Box::new(BidirectionalQueueEndpoint::Ftps {
                        connection: Box::new(case.connection.clone()),
                        root: "/target".into(),
                    }),
                    options,
                    expected_plan: Box::new(prepared.preview().plan.clone()),
                    binding_digest: prepared.preview().digest,
                })
                .unwrap();
            let entry = queue.claim_next().unwrap().unwrap();
            let result = execute_prepared_content_sync_queue_entry(
                &queue,
                &entry,
                prepared,
                true,
                &area.path().join("audit.jsonl"),
                &Cancel::new(),
                &mut |_| {},
            )
            .unwrap();
            let QueueExecutionResult::Sync(stats) = result else {
                panic!("Syncbilanz fehlt")
            };
            assert!(
                stats
                    .metadata_report
                    .issues
                    .iter()
                    .any(|issue| issue.class == doppeldecker_core::sync::MetadataClass::Mtime)
                    || mode == SyncMode::Mirror
            );
            if mode == SyncMode::Mirror {
                assert_eq!(stats.files_deleted, 3);
            }
        }
        assert_eq!(
            std::fs::read(case.remote.path().join("target/Datei")).unwrap(),
            b"bravo"
        );
        assert!(case.remote.path().join("target/Baum/Leer").is_dir());
        assert!(!case.remote.path().join("target/Alt").exists());
        assert!(!case.vfs.capabilities().permits_update());
        assert!(!case.vfs.capabilities().permits_mirror());
        let audit = std::fs::read_to_string(area.path().join("audit.jsonl")).unwrap();
        assert_eq!(audit.lines().count(), 6);
        assert!(!audit.contains(&case.source.to_string_lossy().to_string()));
    }
}

#[test]
fn content_sync_ftp_setzt_dateizeit_auch_ohne_feat_ankuendigung() {
    use std::sync::atomic::Ordering;
    for (tls, advertised) in [(false, false), (false, true), (true, false), (true, true)] {
        let (vfs, _config, _queue, control) = mfmt_case(tls, advertised);
        control.enable_stou.store(true, Ordering::SeqCst);
        control.accept_mfmt.store(true, Ordering::SeqCst);
        let source = private_area();
        let file = source.path().join("inhalt mit leerzeichen");
        std::fs::write(&file, b"unveraenderter Inhalt").unwrap();
        let mtime = 1_700_000_013;
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(mtime, 0)).unwrap();
        let target = ProviderEndpoint::ftps(Arc::new(vfs.clone()), Path::new("/")).unwrap();
        let prepared = prepare_content_sync(
            ProviderEndpoint::local(source.path()).unwrap(),
            target,
            SyncOptions::default(),
            &Cancel::new(),
        )
        .unwrap();
        assert!(
            !prepared
                .preview()
                .plan
                .metadata_report
                .issues
                .iter()
                .any(|issue| issue.class == doppeldecker_core::sync::MetadataClass::Mtime),
            "{:?}",
            prepared.preview().plan.metadata_report
        );
        let digest = prepared.preview().digest;
        let stats = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.files_copied, 1);
        assert_eq!(
            vfs.stat(Path::new("/inhalt mit leerzeichen"))
                .unwrap()
                .mtime,
            Some(mtime)
        );
        assert_eq!(
            control.contents.lock().unwrap()["/inhalt mit leerzeichen"],
            b"unveraenderter Inhalt"
        );
        assert!(!vfs.supports_mtime_write());
    }
}

fn mfmt_case(
    tls: bool,
    advertised: bool,
) -> (
    FtpsVfs,
    tempfile::TempDir,
    OperationQueue,
    Arc<FaultControl>,
) {
    metadata_case(tls, advertised, false)
}
fn metadata_case(
    tls: bool,
    advertised: bool,
    require_mode_selection: bool,
) -> (
    FtpsVfs,
    tempfile::TempDir,
    OperationQueue,
    Arc<FaultControl>,
) {
    use std::sync::atomic::Ordering;
    let server_tls = tls.then(|| {
        let certified = generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
        Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key.into())
            .unwrap(),
        )
    });
    let (port, control) = fault_ftp_server_transport(&[], server_tls);
    control.advertise_mfmt.store(advertised, Ordering::SeqCst);
    control
        .require_mode_selection
        .store(require_mode_selection, Ordering::SeqCst);
    let mut config = fault_config(port);
    if tls {
        config.host = "localhost".into();
        config.transport_security = FtpTransportSecurity::ExplicitTls;
        config.certificate = Some(discover_ftps_certificate("localhost", port).unwrap());
        config.certificate_policy = Some(FtpsCertificatePolicy::PinnedCertificate);
    }
    let (vfs, area, queue) = owned_vfs(config);
    (vfs, area, queue, control)
}

#[test]
fn content_sync_mfmt_ablehnung_und_rundung_erhalten_inhalt_und_bilanzieren_pfadgenau() {
    use doppeldecker_core::sync::MetadataClass;
    use std::sync::atomic::Ordering;
    for tls in [false, true] {
        for rounded in [false, true] {
            for metadata in [
                doppeldecker_core::MetadataProfile::Portable,
                doppeldecker_core::MetadataProfile::NativeBestEffort,
            ] {
                let (vfs, _config, _queue, control) = mfmt_case(tls, true);
                control.enable_stou.store(true, Ordering::SeqCst);
                control.accept_mfmt.store(rounded, Ordering::SeqCst);
                control.round_mfmt.store(rounded, Ordering::SeqCst);
                let source = private_area();
                let path = source.path().join("inhalt");
                std::fs::write(&path, b"vollstaendiger Inhalt").unwrap();
                filetime::set_file_mtime(
                    &path,
                    filetime::FileTime::from_unix_time(1_700_000_013, 0),
                )
                .unwrap();
                let prepared = prepare_content_sync(
                    ProviderEndpoint::local(source.path()).unwrap(),
                    ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap(),
                    SyncOptions {
                        compare: Compare::ContentHash,
                        metadata,
                        ..SyncOptions::default()
                    },
                    &Cancel::new(),
                )
                .unwrap();
                let digest = prepared.preview().digest;
                let stats = prepared
                    .confirm_effects(digest, true)
                    .unwrap()
                    .execute(&Cancel::new(), &mut |_| {})
                    .unwrap();
                assert_eq!(stats.files_copied, 1);
                assert_eq!(
                    control.contents.lock().unwrap()["/inhalt"],
                    b"vollstaendiger Inhalt"
                );
                let issues: Vec<_> = stats
                    .metadata_report
                    .issues
                    .iter()
                    .filter(|issue| issue.class == MetadataClass::Mtime)
                    .collect();
                assert_eq!(issues.len(), 1, "{:?}", stats.metadata_report);
                assert_eq!(issues[0].path, "inhalt");
                assert!(
                    issues[0]
                        .reason
                        .contains(if rounded { "nicht exakt" } else { "502" }),
                    "{}",
                    issues[0].reason
                );
                let times = stats
                    .metadata_report
                    .classes
                    .iter()
                    .find(|class| class.class == MetadataClass::Mtime)
                    .unwrap();
                assert_eq!((times.transferred, times.omitted), (0, 1));
                assert_eq!(
                    control
                        .mutations
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|command| command.starts_with("MFMT "))
                        .count(),
                    1
                );
            }
        }
    }
}

#[test]
fn content_sync_mfmt_antwortverlust_wiederholt_nicht_und_erhaelt_recoverytemp() {
    use std::sync::atomic::Ordering;
    for tls in [false, true] {
        let (vfs, config, queue, control) = mfmt_case(tls, false);
        control.enable_stou.store(true, Ordering::SeqCst);
        control.accept_mfmt.store(true, Ordering::SeqCst);
        control.lose_mfmt_response.store(true, Ordering::SeqCst);
        let source = private_area();
        let path = source.path().join("inhalt");
        std::fs::write(&path, b"vollstaendiger Inhalt").unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1_700_000_013, 0))
            .unwrap();
        let prepared = doppeldecker_core::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::local(source.path()).unwrap(),
            ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap(),
            SyncOptions::default(),
            &Cancel::new(),
        )
        .unwrap();
        let entry = queue.enqueue_claimed(prepared.queue_operation()).unwrap();
        let error = doppeldecker_core::queue::execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &config.path().join("audit.jsonl"),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Failed);
        assert_eq!(queue.snapshot().entries[0].attempts, 1);
        assert_eq!(
            error_code(&queue.retry(&entry.operation_id).unwrap_err()),
            ErrorCode::OutcomeUnclear
        );
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear, "{error}");
        let commands = control.mutations.lock().unwrap().clone();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.starts_with("MFMT "))
                .count(),
            1
        );
        assert!(!commands
            .iter()
            .any(|command| command == "RNTO /inhalt" || command.starts_with("DELE ")));
        let contents = control.contents.lock().unwrap();
        assert!(!contents.contains_key("/inhalt"));
        assert_eq!(contents.len(), 1);
        assert_eq!(contents.values().next().unwrap(), b"vollstaendiger Inhalt");
        let journal = std::fs::read_dir(config.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("recovery"));
        assert!(journal, "FTP-Intent für eigenen Temp fehlt");
    }
}

#[test]
fn content_sync_mfmt_setzt_neue_ordner_nach_ihren_kindern() {
    use std::sync::atomic::Ordering;
    for (tls, denied) in [(false, false), (true, false), (false, true), (true, true)] {
        let (vfs, _config, _queue, control) = mfmt_case(tls, false);
        control.enable_stou.store(true, Ordering::SeqCst);
        control.accept_mfmt.store(true, Ordering::SeqCst);
        control
            .reject_directory_mfmt
            .store(denied, Ordering::SeqCst);
        let source = private_area();
        std::fs::create_dir_all(source.path().join("Baum/Leer")).unwrap();
        std::fs::write(source.path().join("Baum/Kind"), b"kind").unwrap();
        let mtime = 1_700_000_013;
        for path in ["Baum/Kind", "Baum/Leer", "Baum"] {
            filetime::set_file_mtime(
                source.path().join(path),
                filetime::FileTime::from_unix_time(mtime, 0),
            )
            .unwrap();
        }
        let target = Arc::new(vfs.clone());
        let prepared = prepare_content_sync(
            ProviderEndpoint::local(source.path()).unwrap(),
            ProviderEndpoint::ftps(target, Path::new("/")).unwrap(),
            SyncOptions::default(),
            &Cancel::new(),
        )
        .unwrap();
        let digest = prepared.preview().digest;
        let stats = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(
            vfs.stat(Path::new("/Baum/Kind")).unwrap().mtime,
            Some(mtime)
        );
        let issues: Vec<_> = stats
            .metadata_report
            .issues
            .iter()
            .filter(|issue| issue.class == doppeldecker_core::sync::MetadataClass::Mtime)
            .collect();
        assert_eq!(issues.len(), if denied { 2 } else { 0 });
        for path in ["/Baum/Leer", "/Baum"] {
            let actual = vfs.stat(Path::new(path)).unwrap().mtime;
            if denied {
                assert_ne!(actual, Some(mtime));
                assert!(issues
                    .iter()
                    .any(|issue| issue.path == path.trim_start_matches('/')
                        && issue.reason.contains("550")));
            } else {
                assert_eq!(actual, Some(mtime), "{path}");
            }
        }
        let commands = control.mutations.lock().unwrap();
        let parent_time = commands
            .iter()
            .position(|command| command.ends_with(" /Baum") && command.starts_with("MFMT "))
            .unwrap();
        let child_commit = commands
            .iter()
            .position(|command| command == "RNTO /Baum/Kind")
            .unwrap();
        assert!(parent_time > child_commit);
    }
}

#[test]
fn content_sync_mfmt_lockert_native_strict_nicht_still() {
    use std::sync::atomic::Ordering;
    let (vfs, _config, _queue, control) = mfmt_case(false, true);
    control.enable_stou.store(true, Ordering::SeqCst);
    control.accept_mfmt.store(true, Ordering::SeqCst);
    let source = private_area();
    std::fs::write(source.path().join("inhalt"), b"inhalt").unwrap();
    let result = prepare_content_sync(
        ProviderEndpoint::local(source.path()).unwrap(),
        ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap(),
        SyncOptions {
            compare: Compare::ContentHash,
            metadata: doppeldecker_core::MetadataProfile::NativeStrict,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    );
    assert!(result.is_err());
    assert!(control.mutations.lock().unwrap().is_empty());
    assert!(control.contents.lock().unwrap().is_empty());
}

#[test]
fn content_sync_mfmt_prueft_dateizeit_nach_finalem_rename_frisch() {
    use std::sync::atomic::Ordering;
    for tls in [false, true] {
        let (vfs, _config, _queue, control) = mfmt_case(tls, false);
        control.enable_stou.store(true, Ordering::SeqCst);
        control.accept_mfmt.store(true, Ordering::SeqCst);
        control
            .reset_mtime_on_final_rename
            .store(true, Ordering::SeqCst);
        let source = private_area();
        std::fs::write(source.path().join("inhalt"), b"vollstaendiger Inhalt").unwrap();
        filetime::set_file_mtime(
            source.path().join("inhalt"),
            filetime::FileTime::from_unix_time(1_700_000_013, 0),
        )
        .unwrap();
        let prepared = prepare_content_sync(
            ProviderEndpoint::local(source.path()).unwrap(),
            ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap(),
            SyncOptions::default(),
            &Cancel::new(),
        )
        .unwrap();
        let digest = prepared.preview().digest;
        let error = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear, "{error}");
        assert_eq!(
            control.contents.lock().unwrap()["/inhalt"],
            b"vollstaendiger Inhalt"
        );
        let commands = control.mutations.lock().unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.starts_with("MFMT "))
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.as_str() == "RNTO /inhalt")
                .count(),
            1
        );
        assert!(!commands.iter().any(|command| command.starts_with("DELE ")));
    }
}

#[test]
fn content_sync_chmod_ftp_und_tls_setzen_normale_rechte_am_temp() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;
    for (tls, select_mode) in [(false, false), (true, false), (false, true)] {
        let (vfs, _config, _queue, control) = metadata_case(tls, false, select_mode);
        control.enable_stou.store(true, Ordering::SeqCst);
        control.accept_mfmt.store(true, Ordering::SeqCst);
        control.accept_chmod.store(true, Ordering::SeqCst);
        control.omit_mode_readback.store(false, Ordering::SeqCst);
        let source = private_area();
        let file = source.path().join("inhalt mit leerzeichen");
        std::fs::write(&file, b"inhalt").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        let prepared = prepare_content_sync(
            ProviderEndpoint::local(source.path()).unwrap(),
            ProviderEndpoint::ftps(Arc::new(vfs.clone()), Path::new("/")).unwrap(),
            SyncOptions::default(),
            &Cancel::new(),
        )
        .unwrap();
        let digest = prepared.preview().digest;
        let stats = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.files_copied, 1);
        assert_eq!(
            vfs.stat(Path::new("/inhalt mit leerzeichen")).unwrap().mode,
            Some(0o640)
        );
        assert_eq!(
            control
                .mutations
                .lock()
                .unwrap()
                .iter()
                .filter(|command| command.starts_with("SITE CHMOD "))
                .count(),
            1
        );
        assert!(!vfs.supports_mode_write());
    }
}

#[test]
fn content_sync_chmod_bilanziert_ablehnung_fehlenden_und_abweichenden_readback() {
    use doppeldecker_core::{sync::MetadataClass, MetadataProfile};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;
    for metadata in [MetadataProfile::Portable, MetadataProfile::NativeBestEffort] {
        for case in ["rejected", "missing", "ignored"] {
            let (vfs, _config, _queue, control) = mfmt_case(false, false);
            control.enable_stou.store(true, Ordering::SeqCst);
            control.accept_mfmt.store(true, Ordering::SeqCst);
            control
                .accept_chmod
                .store(case != "rejected", Ordering::SeqCst);
            control
                .omit_mode_readback
                .store(case == "missing", Ordering::SeqCst);
            control
                .ignore_chmod
                .store(case == "ignored", Ordering::SeqCst);
            let source = private_area();
            std::fs::create_dir(source.path().join("Ordner")).unwrap();
            std::fs::set_permissions(
                source.path().join("Ordner"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            std::fs::write(source.path().join("inhalt"), b"vollstaendig").unwrap();
            std::fs::set_permissions(
                source.path().join("inhalt"),
                std::fs::Permissions::from_mode(0o640),
            )
            .unwrap();
            let prepared = prepare_content_sync(
                ProviderEndpoint::local(source.path()).unwrap(),
                ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap(),
                SyncOptions {
                    compare: Compare::ContentHash,
                    metadata,
                    ..SyncOptions::default()
                },
                &Cancel::new(),
            )
            .unwrap();
            let digest = prepared.preview().digest;
            let stats = prepared
                .confirm_effects(digest, true)
                .unwrap()
                .execute(&Cancel::new(), &mut |_| {})
                .unwrap();
            assert_eq!(stats.files_copied, 1);
            assert_eq!(stats.metadata_updated, 0);
            assert_eq!(control.contents.lock().unwrap()["/inhalt"], b"vollstaendig");
            let issues: Vec<_> = stats
                .metadata_report
                .issues
                .iter()
                .filter(|issue| issue.class == MetadataClass::NormalMode)
                .collect();
            assert_eq!(issues.len(), 2, "{case}: {:?}", stats.metadata_report);
            for path in ["inhalt", "Ordner"] {
                assert!(
                    issues.iter().any(|issue| issue.path == path
                        && issue.reason.contains(match case {
                            "rejected" => "502",
                            "missing" => "kein unix.mode",
                            _ => "nicht exakt",
                        })),
                    "{case}: {issues:?}"
                );
            }
            let count = stats
                .metadata_report
                .classes
                .iter()
                .find(|count| count.class == MetadataClass::NormalMode)
                .unwrap();
            assert_eq!((count.transferred, count.omitted), (0, 2), "{case}");
            assert_eq!(
                control
                    .mutations
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|command| command.starts_with("SITE CHMOD "))
                    .count(),
                2
            );
        }
    }
}

#[test]
fn content_sync_chmod_native_uebertraegt_normale_rechte_und_bilanziert_sonderbits_getrennt() {
    use doppeldecker_core::{sync::MetadataClass, MetadataProfile};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;
    let (vfs, _config, _queue, control) = mfmt_case(false, false);
    control.enable_stou.store(true, Ordering::SeqCst);
    control.accept_mfmt.store(true, Ordering::SeqCst);
    control.accept_chmod.store(true, Ordering::SeqCst);
    control.omit_mode_readback.store(false, Ordering::SeqCst);
    let source = private_area();
    std::fs::create_dir(source.path().join("Ordner")).unwrap();
    std::fs::set_permissions(
        source.path().join("Ordner"),
        std::fs::Permissions::from_mode(0o2700),
    )
    .unwrap();
    std::fs::write(source.path().join("inhalt"), b"inhalt").unwrap();
    std::fs::set_permissions(
        source.path().join("inhalt"),
        std::fs::Permissions::from_mode(0o2640),
    )
    .unwrap();
    let prepared = prepare_content_sync(
        ProviderEndpoint::local(source.path()).unwrap(),
        ProviderEndpoint::ftps(Arc::new(vfs.clone()), Path::new("/")).unwrap(),
        SyncOptions {
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeBestEffort,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    )
    .unwrap();
    let digest = prepared.preview().digest;
    let stats = prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(vfs.stat(Path::new("/inhalt")).unwrap().mode, Some(0o640));
    assert_eq!(vfs.stat(Path::new("/Ordner")).unwrap().mode, Some(0o700));
    assert!(!stats
        .metadata_report
        .issues
        .iter()
        .any(|issue| issue.class == MetadataClass::NormalMode));
    assert_eq!(
        stats
            .metadata_report
            .issues
            .iter()
            .filter(|issue| issue.class == MetadataClass::SpecialModeBits)
            .count(),
        2
    );
    assert!(!vfs.supports_mode_write());
}

#[test]
fn content_sync_chmod_antwortverlust_bleibt_unklar_ohne_retry_oder_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;
    let (vfs, _config, _queue, control) = mfmt_case(false, false);
    control.enable_stou.store(true, Ordering::SeqCst);
    control.accept_chmod.store(true, Ordering::SeqCst);
    control.omit_mode_readback.store(false, Ordering::SeqCst);
    control.lose_chmod_response.store(true, Ordering::SeqCst);
    let source = private_area();
    std::fs::write(source.path().join("inhalt"), b"inhalt").unwrap();
    std::fs::set_permissions(
        source.path().join("inhalt"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let prepared = prepare_content_sync(
        ProviderEndpoint::local(source.path()).unwrap(),
        ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap(),
        SyncOptions::default(),
        &Cancel::new(),
    )
    .unwrap();
    let digest = prepared.preview().digest;
    let error = prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear, "{error}");
    let commands = control.mutations.lock().unwrap();
    assert_eq!(
        commands
            .iter()
            .filter(|command| command.starts_with("SITE CHMOD "))
            .count(),
        1
    );
    assert!(!commands
        .iter()
        .any(|command| command == "RNTO /inhalt" || command.starts_with("DELE ")));
    let contents = control.contents.lock().unwrap();
    assert_eq!(contents.len(), 1);
    assert_eq!(contents.values().next().unwrap(), b"inhalt");
}

#[test]
fn content_sync_chmod_modusdrift_nach_finalem_rename_ist_keine_auslassung() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;
    for existing in [false, true] {
        let (vfs, _config, _queue, control) = mfmt_case(false, false);
        control.enable_stou.store(true, Ordering::SeqCst);
        control.accept_mfmt.store(true, Ordering::SeqCst);
        control.accept_chmod.store(true, Ordering::SeqCst);
        control.omit_mode_readback.store(false, Ordering::SeqCst);
        control
            .reset_mode_on_final_rename
            .store(true, Ordering::SeqCst);
        if existing {
            control
                .fs
                .lock()
                .unwrap()
                .get_mut("/")
                .unwrap()
                .insert("inhalt".into(), false);
            control
                .contents
                .lock()
                .unwrap()
                .insert("/inhalt".into(), b"alt".to_vec());
        }
        let source = private_area();
        std::fs::write(source.path().join("inhalt"), b"neuer Inhalt").unwrap();
        std::fs::set_permissions(
            source.path().join("inhalt"),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        let prepared = prepare_content_sync(
            ProviderEndpoint::local(source.path()).unwrap(),
            ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap(),
            SyncOptions {
                mode: SyncMode::Update,
                compare: Compare::ContentHash,
                ..SyncOptions::default()
            },
            &Cancel::new(),
        )
        .unwrap();
        let digest = prepared.preview().digest;
        let error = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap_err();
        assert_eq!(
            error_code(&error),
            ErrorCode::OutcomeUnclear,
            "{existing}: {error}"
        );
        assert_eq!(control.contents.lock().unwrap()["/inhalt"], b"neuer Inhalt");
        let commands = control.mutations.lock().unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.starts_with("SITE CHMOD "))
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.as_str() == "RNTO /inhalt")
                .count(),
            1
        );
    }
}

#[test]
fn content_sync_chmod_reine_modeaktionen_zaehlen_nur_bestaetigte_wirkung() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;
    for accepted in [false, true] {
        let (vfs, _config, _queue, control) = mfmt_case(false, false);
        control.accept_chmod.store(accepted, Ordering::SeqCst);
        control.omit_mode_readback.store(false, Ordering::SeqCst);
        control
            .fs
            .lock()
            .unwrap()
            .get_mut("/")
            .unwrap()
            .insert("inhalt".into(), false);
        control
            .contents
            .lock()
            .unwrap()
            .insert("/inhalt".into(), b"inhalt".to_vec());
        control
            .fs
            .lock()
            .unwrap()
            .get_mut("/")
            .unwrap()
            .insert("Ordner".into(), true);
        control
            .fs
            .lock()
            .unwrap()
            .insert("/Ordner".into(), Default::default());
        let source = private_area();
        std::fs::write(source.path().join("inhalt"), b"inhalt").unwrap();
        std::fs::set_permissions(
            source.path().join("inhalt"),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        std::fs::create_dir(source.path().join("Ordner")).unwrap();
        std::fs::set_permissions(
            source.path().join("Ordner"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let prepared = prepare_content_sync(
            ProviderEndpoint::local(source.path()).unwrap(),
            ProviderEndpoint::ftps(Arc::new(vfs.clone()), Path::new("/")).unwrap(),
            SyncOptions {
                mode: SyncMode::Update,
                compare: Compare::ContentHash,
                ..SyncOptions::default()
            },
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(prepared.preview().plan.copy_count, 0);
        let digest = prepared.preview().digest;
        let stats = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.files_copied, 0);
        assert_eq!(stats.metadata_updated, if accepted { 2 } else { 0 });
        assert_eq!(
            control
                .mutations
                .lock()
                .unwrap()
                .iter()
                .filter(|command| command.starts_with("SITE CHMOD "))
                .count(),
            2
        );
        assert_eq!(
            vfs.stat(Path::new("/inhalt")).unwrap().mode,
            Some(if accepted { 0o640 } else { 0o644 })
        );
    }
}

#[test]
fn content_sync_ftp_quelle_native_normale_rechte_brauchen_keinen_sonderbitvertrag() {
    use doppeldecker_core::{sync::MetadataClass, MetadataProfile};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;
    for has_mode in [false, true] {
        let (vfs, _config, _queue, control) = mfmt_case(false, false);
        control
            .omit_mode_readback
            .store(!has_mode, Ordering::SeqCst);
        control
            .fs
            .lock()
            .unwrap()
            .get_mut("/")
            .unwrap()
            .insert("inhalt".into(), false);
        control
            .contents
            .lock()
            .unwrap()
            .insert("/inhalt".into(), b"vollstaendig".to_vec());
        control
            .mode_overrides
            .lock()
            .unwrap()
            .insert("/inhalt".into(), 0o640);
        let target = private_area();
        let prepared = prepare_content_sync(
            ProviderEndpoint::ftps(Arc::new(vfs.clone()), Path::new("/")).unwrap(),
            ProviderEndpoint::local(target.path()).unwrap(),
            SyncOptions {
                compare: Compare::ContentHash,
                metadata: MetadataProfile::NativeBestEffort,
                ..SyncOptions::default()
            },
            &Cancel::new(),
        )
        .unwrap();
        let transfer = prepared
            .preview()
            .plan
            .actions
            .iter()
            .find_map(|action| match action {
                doppeldecker_core::sync::Action::CopyFile { metadata, .. } => metadata.as_ref(),
                _ => None,
            })
            .unwrap();
        assert_eq!(transfer.normal_mode, has_mode);
        assert!(!transfer.special_mode_bits);
        let normal: Vec<_> = prepared
            .preview()
            .plan
            .metadata_report
            .issues
            .iter()
            .filter(|issue| issue.class == MetadataClass::NormalMode)
            .collect();
        assert_eq!(
            normal.len(),
            usize::from(!has_mode),
            "{:?}",
            prepared.preview().plan.metadata_report
        );
        assert!(prepared
            .preview()
            .plan
            .metadata_report
            .issues
            .iter()
            .any(|issue| issue.class == MetadataClass::SpecialModeBits
                && issue.reason.contains("Quelle")));
        let digest = prepared.preview().digest;
        let stats = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(
            std::fs::read(target.path().join("inhalt")).unwrap(),
            b"vollstaendig"
        );
        let actual = std::fs::metadata(target.path().join("inhalt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        if has_mode {
            assert_eq!(actual, 0o640);
        } else {
            assert_ne!(actual, 0o000);
        }
        assert_eq!(stats.files_copied, 1);
        let strict_target = private_area();
        assert!(prepare_content_sync(
            ProviderEndpoint::ftps(Arc::new(vfs.clone()), Path::new("/")).unwrap(),
            ProviderEndpoint::local(strict_target.path()).unwrap(),
            SyncOptions {
                compare: Compare::ContentHash,
                metadata: MetadataProfile::NativeStrict,
                ..SyncOptions::default()
            },
            &Cancel::new()
        )
        .is_err());
        assert_eq!(std::fs::read_dir(strict_target.path()).unwrap().count(), 0);
        let ordinary_target = private_area();
        let strict_root = std::fs::canonicalize(ordinary_target.path()).unwrap();
        let strict_view = doppeldecker_core::LocalVfs::rooted(&strict_root).unwrap();
        let strict_plan = doppeldecker_core::sync::plan(
            &vfs,
            Path::new("/"),
            &strict_view,
            &strict_root,
            &SyncOptions {
                compare: Compare::ContentHash,
                metadata: MetadataProfile::NativeBestEffort,
                ..SyncOptions::default()
            },
        )
        .unwrap();
        assert!(strict_plan.actions.iter().any(|action|matches!(action,doppeldecker_core::sync::Action::CopyFile{metadata:Some(metadata),..} if !metadata.normal_mode)));

        assert!(control.mutations.lock().unwrap().is_empty());
    }
}

#[test]
fn content_sync_native_reine_ausgelassene_ordnerzeit_zaehlt_keine_erfolgswirkung() {
    use doppeldecker_core::{
        sync::{Action, MetadataClass},
        MetadataProfile,
    };
    use std::sync::atomic::Ordering;
    for accepted in [false, true] {
        let (source, _source_config, _source_queue, source_control) = mfmt_case(false, false);
        let (target, _target_config, _target_queue, target_control) = mfmt_case(false, false);
        target_control.accept_mfmt.store(accepted, Ordering::SeqCst);
        for control in [&source_control, &target_control] {
            control
                .fs
                .lock()
                .unwrap()
                .get_mut("/")
                .unwrap()
                .insert("Ordner".into(), true);
            control
                .fs
                .lock()
                .unwrap()
                .insert("/Ordner".into(), Default::default());
        }
        source_control
            .mtime_overrides
            .lock()
            .unwrap()
            .insert("/Ordner".into(), "20231114221333".into());
        let prepared = prepare_content_sync(
            ProviderEndpoint::ftps(Arc::new(source), Path::new("/")).unwrap(),
            ProviderEndpoint::ftps(Arc::new(target), Path::new("/")).unwrap(),
            SyncOptions {
                mode: SyncMode::Update,
                compare: Compare::ContentHash,
                metadata: MetadataProfile::NativeBestEffort,
                ..SyncOptions::default()
            },
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(prepared.preview().plan.actions.len(), 1);
        let Action::SetDirectoryMetadata { metadata, .. } = &prepared.preview().plan.actions[0]
        else {
            panic!("Ordnerzeitaktion fehlt")
        };
        assert!(
            !metadata.normal_mode
                && !metadata.owner
                && !metadata.acl
                && !metadata.xattrs
                && !metadata.special_mode_bits
        );
        let digest = prepared.preview().digest;
        let stats = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.metadata_updated, u64::from(accepted));
        assert_eq!(stats.files_copied, 0);
        assert_eq!(stats.files_deleted, 0);
        assert_eq!(
            stats
                .metadata_report
                .issues
                .iter()
                .any(|issue| issue.path == "Ordner"
                    && issue.class == MetadataClass::Mtime
                    && issue.reason.contains("502")),
            !accepted
        );
        let commands = target_control.mutations.lock().unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.starts_with("MFMT "))
                .count(),
            1
        );
        assert!(!commands
            .iter()
            .any(|command| command.starts_with("SITE CHMOD ")));
        assert!(source_control.mutations.lock().unwrap().is_empty());
    }
}

#[path = "support/content_sync_queue.rs"]
mod guarded_content_queue;
use guarded_content_queue::prepare as prepare_content_sync;

#[test]
fn ftp_dateinamenzyklen_mit_dauerhafter_queuebilanz() {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    use doppeldecker_core::{OperationQueue, QueueOperation, QueueState};
    for scenario in 0..3 {
        let directories = scenario == 1;
        let is_directory = |index: usize| directories || scenario == 2 && index % 2 == 1;
        for tls in [false, true] {
            for interrupted in [false, true] {
                let case = FtpNameGroupCase::new_transport(tls);
                let target = case.remote.path().join("target");
                let names = ["ab", "aab", "aaab"];
                let content = |name: &str, index: usize| {
                    let path = target.join(name);
                    if is_directory(index) {
                        path.join("inside/leaf")
                    } else {
                        path
                    }
                };
                for (index, name) in names.iter().enumerate() {
                    if is_directory(index) {
                        std::fs::create_dir_all(target.join(name).join("inside")).unwrap();
                    }
                    std::fs::write(content(name, index), format!("Original{index}")).unwrap();
                }
                let config = private_area();
                let journal = config.path().join("operations-queue.json");
                let process_lock = std::sync::Arc::new(
                    doppeldecker_core::OperationQueueProcessLock::acquire(&journal).unwrap(),
                );
                let queue =
                    OperationQueue::open_with_process_lock(&journal, process_lock.clone()).unwrap();
                let endpoint =
                    ProviderEndpoint::ftps(case.vfs.clone(), Path::new("/target")).unwrap();
                let options = BatchRenameOptions {
                    find: "aaa".into(),
                    replacement: String::new(),
                    prefix: "a".into(),
                    suffix: String::new(),
                    preserve_extension: false,
                    trim_outer_spaces: false,
                    numbering: None,
                };
                let plan = PreparedFtpBatchRename::prepare(
                    &endpoint,
                    &names.map(str::to_string),
                    &options,
                    &Cancel::new(),
                )
                .unwrap();
                if !tls && !interrupted {
                    let refused = PreparedFtpBatchRename::prepare(
                        &endpoint,
                        &names.map(str::to_string),
                        &options,
                        &Cancel::new(),
                    )
                    .unwrap()
                    .execute_with_queue(
                        &Cancel::new(),
                        &mut || Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                        &mut |_, _| {},
                        &queue,
                    );
                    assert!(refused
                        .rows
                        .iter()
                        .all(|row| row.status == BatchRenameStatus::FailedUnchanged
                            && row.code == Some(ErrorCode::PermissionDenied)));
                    assert!(queue.snapshot().entries.is_empty());
                }
                assert!(
                    plan.rows()
                        .iter()
                        .all(|row| row.status == BatchRenameStatus::Ready),
                    "{:?}",
                    plan.rows()
                );
                let cancel = Cancel::new();
                let report = plan.execute_with_queue(
                    &cancel,
                    &mut || {
                        if interrupted
                            && std::fs::read_dir(&target).unwrap().any(|entry| {
                                entry
                                    .unwrap()
                                    .file_name()
                                    .to_string_lossy()
                                    .starts_with(".doppeldecker-rename-")
                            })
                        {
                            let entry = queue
                                .snapshot()
                                .entries
                                .into_iter()
                                .find(|entry| {
                                    matches!(
                                        entry.operation,
                                        QueueOperation::RenameCycleRecovery { .. }
                                    )
                                })
                                .unwrap();
                            assert!(
                                queue
                                    .acknowledge_unclear_outcome(&entry.operation_id, true)
                                    .is_err(),
                                "Aktive Intents dürfen nicht geschlossen werden"
                            );
                            cancel.cancel();
                        }
                        Ok(())
                    },
                    &mut |_, _| {},
                    &queue,
                );
                let entry = queue
                    .snapshot()
                    .entries
                    .into_iter()
                    .find(|entry| {
                        matches!(entry.operation, QueueOperation::RenameCycleRecovery { .. })
                    })
                    .unwrap();
                let QueueOperation::RenameCycleRecovery { record } = &entry.operation else {
                    unreachable!()
                };
                record.validate().unwrap();
                if interrupted && !tls && scenario == 0 {
                    if let Some(path) = std::env::var_os("DOPPELDECKER_TEST_CYCLE_JOURNAL") {
                        std::fs::copy(&journal, path).unwrap();
                    }
                }
                if interrupted {
                    assert_eq!(report.partially_renamed, 1, "{tls}: {:?}", report.rows);
                    assert_eq!(entry.state, QueueState::Interrupted);
                    let mut impossible = (**record).clone();
                    impossible.current_names.swap(1, 2);
                    impossible.validate().unwrap();
                    assert!(
                        queue.checkpoint_rename_cycle(&impossible).is_err(),
                        "Ohne vorher gespeicherten Schritt darf keine Namenslage bestätigt werden"
                    );
                    assert_eq!(
                        std::fs::read(content(&record.temp_name, 0)).unwrap(),
                        b"Original0"
                    );
                    assert_eq!(std::fs::read(content("aab", 1)).unwrap(), b"Original1");
                    assert_eq!(std::fs::read(content("aaab", 2)).unwrap(), b"Original2");
                    assert!(!target.join("ab").exists());
                    let other_queue = OperationQueue::open_with_process_lock(
                        config.path().join("operations-queue-window-other.json"),
                        process_lock.clone(),
                    )
                    .unwrap();
                    let sync_target = tempfile::tempdir().unwrap();
                    let error = match doppeldecker_core::prepare_content_sync_for_queue(
                        &other_queue,
                        endpoint.clone(),
                        ProviderEndpoint::local(sync_target.path()).unwrap(),
                        doppeldecker_core::SyncOptions::default(),
                        &Cancel::new(),
                    ) {
                        Ok(_) => {
                            panic!("Offener Namenstausch wurde aus anderem Fenster nicht erkannt")
                        }
                        Err(error) => error,
                    };
                    assert_eq!(
                        doppeldecker_core::error_code(&error),
                        ErrorCode::OutcomeUnclear
                    );
                    if directories {
                        for name in [record.temp_name.as_str(), "aab", "aaab"] {
                            let root = format!("/target/{name}/inside");
                            let nested =
                                ProviderEndpoint::ftps(case.vfs.clone(), Path::new(&root)).unwrap();
                            let result = doppeldecker_core::prepare_content_sync_for_queue(
                                &other_queue,
                                nested,
                                ProviderEndpoint::local(sync_target.path()).unwrap(),
                                doppeldecker_core::SyncOptions::default(),
                                &Cancel::new(),
                            );
                            let error = match result {
                                Ok(_) => panic!("Offener Zyklus wurde unter {root} nicht erkannt"),
                                Err(error) => error,
                            };
                            assert_eq!(
                                doppeldecker_core::error_code(&error),
                                ErrorCode::OutcomeUnclear,
                                "{root}: {error}"
                            );
                        }
                    }
                    drop(other_queue);
                    assert!(queue.retry(&entry.operation_id).is_err());
                    assert!(queue
                        .acknowledge_unclear_outcome(&entry.operation_id, false)
                        .is_err());
                } else {
                    assert_eq!(
                        report.renamed, 3,
                        "{tls}: {:?} {:?}",
                        report.rows, report.limitations
                    );
                    assert_eq!(entry.state, QueueState::Completed);
                    assert!(record.finished);
                    for (index, name) in ["aab", "aaab", "ab"].iter().enumerate() {
                        assert_eq!(
                            std::fs::read(content(name, index)).unwrap(),
                            format!("Original{index}").as_bytes()
                        );
                    }
                    assert!(!target.join(&record.temp_name).exists());
                }
                let id = entry.operation_id.clone();
                drop(queue);
                drop(process_lock);
                if interrupted {
                    let original: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
                    for forge in ["old_version", "pending"] {
                        let mut data = original.clone();
                        if forge == "old_version" {
                            data["version"] = serde_json::json!(37);
                        } else {
                            data["entries"][0]["state"] = serde_json::json!("Pending");
                        }
                        let path = config.path().join(format!("forged-{forge}.json"));
                        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
                        let error = match OperationQueue::open(&path) {
                            Ok(_) => panic!("Recovery darf nicht ausführbar migriert werden"),
                            Err(error) => error,
                        };
                        assert_eq!(
                            error.kind(),
                            std::io::ErrorKind::InvalidData,
                            "{forge}: {error}"
                        );
                    }
                }
                let queue = OperationQueue::open(&journal).unwrap();
                if interrupted {
                    assert!(queue.retry(&id).is_err());
                    queue.acknowledge_unclear_outcome(&id, true).unwrap();
                    assert_eq!(
                        queue
                            .snapshot()
                            .entries
                            .iter()
                            .find(|entry| entry.operation_id == id)
                            .unwrap()
                            .state,
                        QueueState::Cancelled
                    );
                    assert_eq!(
                        std::fs::read(content(&record.temp_name, 0)).unwrap(),
                        b"Original0"
                    );
                    let sync_target = tempfile::tempdir().unwrap();
                    let blocked = doppeldecker_core::prepare_content_sync_for_queue(
                        &queue,
                        endpoint.clone(),
                        ProviderEndpoint::local(sync_target.path()).unwrap(),
                        doppeldecker_core::SyncOptions::default(),
                        &Cancel::new(),
                    );
                    assert!(blocked.is_err(),"Manuelle Queuebestätigung darf einen vorhandenen Zwischenstand nicht zu Nutzdaten machen");
                    let secured = tempfile::tempdir().unwrap();
                    std::fs::rename(
                        target.join(&record.temp_name),
                        secured.path().join("original0"),
                    )
                    .unwrap();
                    assert!(
                    doppeldecker_core::prepare_content_sync_for_queue(
                        &queue,
                        endpoint.clone(),
                        ProviderEndpoint::local(sync_target.path()).unwrap(),
                        doppeldecker_core::SyncOptions::default(),
                        &Cancel::new()
                    )
                    .is_ok(),
                    "Nach bewusster Prüfung und tatsächlichem Sichern außerhalb beider Wurzeln muss der Sync wieder planbar sein"
                );
                    drop(queue);
                    OperationQueue::open(&journal).unwrap();
                } else {
                    assert_eq!(
                        queue
                            .snapshot()
                            .entries
                            .iter()
                            .find(|entry| entry.operation_id == id)
                            .unwrap()
                            .state,
                        QueueState::Completed
                    );
                }
            }
        }
    }
}

#[path = "support/archive_upload_probe.rs"]
mod archive_upload_probe;
#[path = "support/property_native_fixture.rs"]
mod property_native_fixture;
#[test]
fn ftp_und_ftps_uebertragen_erzeugte_zip_und_entpackquellen() {
    for tls in [false, true] {
        let case = FtpNameGroupCase::new_transport(tls);
        archive_upload_probe::run(
            BidirectionalQueueEndpoint::Ftps {
                connection: Box::new(case.connection.clone()),
                root: "/target".into(),
            },
            &case.remote.path().join("target"),
            Arc::new(TestSecrets),
        );
    }
}

#[test]
fn interaktive_ftp_rechte_rekursiv_mit_sonderbits_und_ohne_inhaltszugriff() {
    use doppeldecker_core::file_properties::change_provider_permissions;
    use std::sync::atomic::Ordering;
    for tls in [false, true] {
        let (vfs, _area, _queue, control) = metadata_case(tls, false, true);
        control.accept_chmod.store(true, Ordering::SeqCst);
        control.omit_mode_readback.store(false, Ordering::SeqCst);
        {
            let mut fs = control.fs.lock().unwrap();
            fs.get_mut("/").unwrap().insert("Tree".into(), true);
            fs.get_mut("/").unwrap().insert("sentinel".into(), false);
            fs.insert(
                "/Tree".into(),
                [("file".into(), false), ("empty".into(), true)].into(),
            );
            fs.insert("/Tree/empty".into(), Default::default());
        }
        control
            .contents
            .lock()
            .unwrap()
            .insert("/Tree/file".into(), b"unchanged".to_vec());
        control
            .mode_overrides
            .lock()
            .unwrap()
            .extend([("/Tree".into(), 0o2755), ("/Tree/file".into(), 0o4644)]);
        let endpoint = ProviderEndpoint::ftps(Arc::new(vfs.clone()), Path::new("/")).unwrap();
        let rows = change_provider_permissions(
            &endpoint,
            &["Tree".into()],
            0o750,
            true,
            &Cancel::new(),
            &|| Ok(()),
        )
        .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.changed), "{rows:?}");
        let chmod = control
            .mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|command| command.starts_with("SITE CHMOD"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(chmod.len(), 3);
        assert_eq!(chmod.last().unwrap(), "SITE CHMOD 2750 /Tree");
        assert_eq!(control.mode_overrides.lock().unwrap()["/Tree/file"], 0o4750);
        let unchanged = change_provider_permissions(
            &endpoint,
            &["Tree".into()],
            0o750,
            true,
            &Cancel::new(),
            &|| Ok(()),
        )
        .unwrap();
        assert!(unchanged.iter().all(|row| row.changed));
        assert_eq!(
            control
                .mutations
                .lock()
                .unwrap()
                .iter()
                .filter(|command| command.starts_with("SITE CHMOD"))
                .count(),
            3,
            "Passende Rechte benötigen keinen neuen CHMOD"
        );
        assert!(control.retr_paths.lock().unwrap().is_empty());
        assert_eq!(control.contents.lock().unwrap()["/Tree/file"], b"unchanged");
        assert!(!vfs.supports_mode_write());
    }
}

#[test]
fn interaktive_ftp_rechte_ablehnung_antwortverlust_und_profilwechsel_bilanziert() {
    use doppeldecker_core::file_properties::change_provider_permissions;
    use std::sync::atomic::Ordering;
    for case in [
        "rejected",
        "missing",
        "ignored",
        "lost",
        "profile",
        "cancel",
        "profile_after",
        "cancel_after",
    ] {
        let (vfs, _area, _queue, control) = metadata_case(false, false, true);
        control
            .accept_chmod
            .store(case != "rejected", Ordering::SeqCst);
        control
            .omit_mode_readback
            .store(case == "missing", Ordering::SeqCst);
        control
            .ignore_chmod
            .store(case == "ignored", Ordering::SeqCst);
        control
            .lose_chmod_response
            .store(case == "lost", Ordering::SeqCst);
        control
            .fs
            .lock()
            .unwrap()
            .get_mut("/")
            .unwrap()
            .extend([("a".into(), false), ("b".into(), false)]);
        let endpoint = ProviderEndpoint::ftps(Arc::new(vfs), Path::new("/")).unwrap();
        let cancel = Cancel::new();
        let profile_changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if ["profile_after", "cancel_after"].contains(&case) {
            let changed = profile_changed.clone();
            let stop = cancel.clone();
            *control.mutation_observer.lock().unwrap() = Some(Arc::new(move |command| {
                if command.starts_with("SITE CHMOD") {
                    if case == "profile_after" {
                        changed.store(true, Ordering::SeqCst);
                    } else {
                        stop.cancel();
                    }
                }
            }));
        }
        if case == "cancel" {
            cancel.cancel();
        }
        let result = change_provider_permissions(
            &endpoint,
            &["a".into(), "b".into()],
            0o600,
            false,
            &cancel,
            &|| {
                if case == "profile" || profile_changed.load(Ordering::SeqCst) {
                    Err(doppeldecker_core::coded_error(
                        std::io::ErrorKind::Interrupted,
                        ErrorCode::PlanDrift,
                        "Profil geändert",
                    ))
                } else {
                    Ok(())
                }
            },
        );
        if ["profile", "cancel"].contains(&case) {
            assert_eq!(
                error_code(&result.unwrap_err()),
                if case == "cancel" {
                    ErrorCode::Interrupted
                } else {
                    ErrorCode::PlanDrift
                }
            );
        } else {
            let rows = result.unwrap();
            if case == "cancel_after" {
                assert!(rows[0].changed);
                assert_eq!(rows[1].code, Some(ErrorCode::Interrupted));
            } else {
                assert!(rows.iter().all(|row| !row.changed), "{case}: {rows:?}");
            }
            if ["ignored", "lost", "profile_after"].contains(&case) {
                assert_eq!(rows[0].code, Some(ErrorCode::OutcomeUnclear));
                assert_eq!(rows[1].code, Some(ErrorCode::Interrupted));
            }
        }
        let count = control
            .mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|command| command.starts_with("SITE CHMOD"))
            .count();
        assert_eq!(
            count,
            match case {
                "profile" | "cancel" | "missing" => 0,
                "ignored" | "lost" | "profile_after" | "cancel_after" => 1,
                _ => 2,
            },
            "{case}"
        );
        assert!(control.retr_paths.lock().unwrap().is_empty());
    }
}

#[test]
fn ftp_unbegonnener_zyklus_hinterlaesst_nach_drift_oder_abbruch_keine_recovery() {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::ftp_batch_rename::PreparedFtpBatchRename;
    for stop in [false, true] {
        let case = FtpNameGroupCase::new();
        let target = case.remote.path().join("target");
        for name in ["ab", "aab", "aaab"] {
            std::fs::write(target.join(name), name).unwrap();
        }
        let endpoint = ProviderEndpoint::ftps(case.vfs.clone(), Path::new("/target")).unwrap();
        let options = BatchRenameOptions {
            find: "aaa".into(),
            replacement: String::new(),
            prefix: "a".into(),
            suffix: String::new(),
            preserve_extension: false,
            trim_outer_spaces: false,
            numbering: None,
        };
        let plan = PreparedFtpBatchRename::prepare(
            &endpoint,
            &["ab".into(), "aab".into(), "aaab".into()],
            &options,
            &Cancel::new(),
        )
        .unwrap();
        let config = private_area();
        let journal = config.path().join("queue.json");
        let queue = OperationQueue::open(&journal).unwrap();
        let cancel = Cancel::new();
        let mut injected = false;
        let report = plan.execute_with_queue(
            &cancel,
            &mut || {
                if !injected && !queue.snapshot().entries.is_empty() {
                    injected = true;
                    if stop {
                        cancel.cancel();
                    } else {
                        std::fs::write(target.join("ab"), b"changed source").unwrap();
                    }
                }
                Ok(())
            },
            &mut |_, _| {},
            &queue,
        );
        assert!(injected);
        assert!(
            report.rows.iter().all(|row| row.status
                == if stop {
                    BatchRenameStatus::Cancelled
                } else {
                    BatchRenameStatus::FailedUnchanged
                }),
            "{report:?}"
        );
        assert!(queue.snapshot().entries.is_empty());
        drop(queue);
        assert!(OperationQueue::open(&journal)
            .unwrap()
            .snapshot()
            .entries
            .is_empty());
        let mut names = std::fs::read_dir(&target)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["aaab", "aab", "ab"]);
        assert!(!report
            .limitations
            .iter()
            .any(|text| text.contains("Queuebilanz und Zwischenname")));
    }
}
