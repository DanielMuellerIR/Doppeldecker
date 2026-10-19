//! Additive F5-Uploads ohne künstliche Collection-/Löschprobe.
#[path = "support/webdav_namespace.rs"]
mod server;
use doppeldecker_core::*;
use std::sync::Arc;

fn private_queue_directory() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}

fn operation(server: &server::Server, source: &std::path::Path, names: &[&str]) -> QueueOperation {
    QueueOperation::ProviderFile {
        kind: FileQueueKind::Copy,
        source: Box::new(BidirectionalQueueEndpoint::Local {
            root: source.to_string_lossy().into_owned(),
        }),
        target: Box::new(BidirectionalQueueEndpoint::WebDav {
            connection: Box::new(server.config.clone()),
            root: "/pane".into(),
        }),
        items: names
            .iter()
            .map(|name| source.join(name).to_string_lossy().into_owned())
            .collect(),
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
    }
}
#[test]
fn additive_webdav_dateien_brauchen_keine_collection_oder_loeschprobe() {
    let server = server::Server::new();
    let source = tempfile::tempdir().unwrap();
    let source_root = std::fs::canonicalize(source.path()).unwrap();
    let names = ["Datei.txt", "Leer", "Grüße %2F.txt"];
    for (index, name) in names.iter().enumerate() {
        std::fs::write(
            source_root.join(name),
            if index == 1 { &b""[..] } else { &b"Inhalt"[..] },
        )
        .unwrap();
    }
    let config = private_queue_directory();
    let queue = OperationQueue::open(config.path().join("queue.json")).unwrap();
    queue
        .enqueue(operation(&server, &source_root, &names))
        .unwrap();
    let claimed = queue.claim_next().unwrap().unwrap();
    let result = execute_provider_file_queue_entry(
        &queue,
        claimed,
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(server::TestSecrets),
        &Cancel::new(),
        &mut |_| {},
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    for name in names {
        assert_eq!(
            std::fs::read(source_root.join(name)).unwrap(),
            std::fs::read(server.root.path().join("pane").join(name)).unwrap()
        );
    }
    let state = server.state.lock().unwrap();
    assert_eq!(state.requests.iter().filter(|(m, _)| m == "PUT").count(), 3);
    assert_eq!(
        state.requests.iter().filter(|(m, _)| m == "MOVE").count(),
        3
    );
    assert!(!state
        .requests
        .iter()
        .any(|(m, _)| matches!(m.as_str(), "MKCOL" | "DELETE")));
    assert_eq!(std::fs::read_dir(server.root.path()).unwrap().count(), 1);
    assert_eq!(
        std::fs::read_dir(server.root.path().join("pane"))
            .unwrap()
            .count(),
        3
    );
}

struct Case {
    server: server::Server,
    _source: tempfile::TempDir,
    source_root: std::path::PathBuf,
    config: tempfile::TempDir,
}
impl Case {
    fn new(bytes: &[u8]) -> Self {
        let source = tempfile::tempdir().unwrap();
        let source_root = std::fs::canonicalize(source.path()).unwrap();
        std::fs::write(source_root.join("Datei.txt"), bytes).unwrap();
        Self {
            server: server::Server::new(),
            _source: source,
            source_root,
            config: private_queue_directory(),
        }
    }
    fn run(
        &self,
        cancel: &Cancel,
        progress: &mut dyn FnMut(&Progress),
    ) -> (OperationQueue, std::io::Result<QueueExecutionResult>) {
        let queue = OperationQueue::open(self.config.path().join("queue.json")).unwrap();
        queue
            .enqueue(operation(&self.server, &self.source_root, &["Datei.txt"]))
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
            Arc::new(server::TestSecrets),
            cancel,
            progress,
        );
        (queue, result)
    }
    fn count(&self, method: &str) -> usize {
        self.server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(m, _)| m == method)
            .count()
    }
    fn assert_no_probes(&self) {
        assert_eq!(self.count("MKCOL"), 0);
    }
    fn leftovers(&self) -> Vec<String> {
        std::fs::read_dir(self.server.root.path().join("pane"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_str().unwrap().into())
            .collect()
    }
}

#[test]
fn upload_antwortverlust_und_schwache_put_belege_bleiben_ohne_retry_erhalten() {
    for fault in [
        server::Fault::LostAfterPut,
        server::Fault::ErrorAfterPut,
        server::Fault::MissingPutEtag,
        server::Fault::WeakPutEtag,
        server::Fault::WrongPutEtag,
    ] {
        let case = Case::new(b"Inhalt");
        case.server.state.lock().unwrap().fault = fault;
        let (queue, result) = case.run(&Cancel::new(), &mut |_| {});
        let error = result.unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear, "{error}");
        assert_eq!(case.count("PUT"), 1);
        assert_eq!(case.count("MOVE"), 0);
        assert_eq!(case.count("DELETE"), 0);
        case.assert_no_probes();
        let paths = case.leftovers();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].contains("fileop-tmp"));
        let snapshot = queue.snapshot();
        let entry = &snapshot.entries[0];
        assert_eq!(entry.error_code, Some(ErrorCode::OutcomeUnclear));
        assert!(entry
            .temp_recovery_paths
            .iter()
            .any(|p| p.ends_with(&paths[0])));
        let id = entry.operation_id.clone();
        drop(queue);
        let queue = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Interrupted);
        assert_eq!(
            error_code(&queue.retry(&id).unwrap_err()),
            ErrorCode::OutcomeUnclear
        );
        assert_eq!(case.count("PUT"), 1);
    }
}

#[test]
fn einzelne_elternbedingung_verhindert_put_in_ausgetauschtem_elternordner() {
    for swap in [false, true] {
        let case = Case::new(b"Inhalt");
        {
            let mut state = case.server.state.lock().unwrap();
            state.deny_collection_probes = true;
            if swap {
                state.fault = server::Fault::ParentSwapAtPut;
            }
        }
        let (queue, result) = case.run(&Cancel::new(), &mut |_| {});
        if swap {
            assert_eq!(error_code(&result.unwrap_err()), ErrorCode::OutcomeUnclear);
            assert_eq!(case.count("MOVE"), 0);
            assert_eq!(case.count("DELETE"), 0);
            assert_eq!(
                std::fs::read(case.server.root.path().join("pane/Fremd.txt")).unwrap(),
                b"Fremder Elternordner"
            );
            assert!(case
                .leftovers()
                .iter()
                .all(|name| !name.contains("fileop-tmp")));
            assert_eq!(
                std::fs::read_dir(case.server.root.path().join("pane-before-swap"))
                    .unwrap()
                    .count(),
                0
            );
            let id = queue.snapshot().entries[0].operation_id.clone();
            drop(queue);
            let queue = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
            assert_eq!(
                error_code(&queue.retry(&id).unwrap_err()),
                ErrorCode::OutcomeUnclear
            );
        } else {
            assert!(result.is_ok(), "{result:?}");
            assert_eq!(case.count("MOVE"), 1);
            assert_eq!(
                std::fs::read(case.server.root.path().join("pane/Datei.txt")).unwrap(),
                b"Inhalt"
            );
        }
        assert_eq!(case.count("PUT"), 1);
        case.assert_no_probes();
        assert_eq!(
            std::fs::read(case.source_root.join("Datei.txt")).unwrap(),
            b"Inhalt"
        );
    }
}
#[test]
fn upload_moveverlust_und_falscher_readback_erhalten_beide_pruefpfade() {
    for fault in [
        server::Fault::LostAfterMove,
        server::Fault::ErrorAfterMove,
        server::Fault::WrongReadbackEtag,
        server::Fault::MissingParentChild,
    ] {
        let case = Case::new(b"Inhalt");
        case.server.state.lock().unwrap().fault = fault;
        let (queue, result) = case.run(&Cancel::new(), &mut |_| {});
        let error = result.unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear, "{error}");
        assert_eq!(case.count("PUT"), 1);
        assert_eq!(case.count("MOVE"), 1);
        assert_eq!(case.count("DELETE"), 0);
        case.assert_no_probes();
        assert_eq!(
            std::fs::read(case.server.root.path().join("pane/Datei.txt")).unwrap(),
            b"Inhalt"
        );
        let entry = &queue.snapshot().entries[0];
        assert!(entry
            .temp_recovery_paths
            .contains(&"/pane/Datei.txt".into()));
        assert!(entry
            .temp_recovery_paths
            .iter()
            .any(|p| p.contains("fileop-tmp")));
    }
}
#[test]
fn upload_spaete_kollision_erhaelt_fremdes_ziel_und_bereinigt_nur_den_eigenen_temp() {
    let case = Case::new(b"Inhalt");
    case.server.state.lock().unwrap().fault = server::Fault::LateMoveCollision;
    let (_, result) = case.run(&Cancel::new(), &mut |_| {});
    let error = result.unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::AlreadyExists, "{error}");
    assert_eq!(case.count("PUT"), 1);
    assert_eq!(case.count("MOVE"), 1);
    assert_eq!(case.count("DELETE"), 1);
    case.assert_no_probes();
    assert_eq!(case.leftovers(), vec!["Datei.txt"]);
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/Datei.txt")).unwrap(),
        b"Fremdes Ziel"
    );
    let state = case.server.state.lock().unwrap();
    for conditions in state
        .put_if_headers
        .iter()
        .chain(&state.move_if_headers)
        .chain(&state.delete_if_headers)
    {
        assert!(!conditions.contains(&format!("<{}/> ", case.server.config.origin)));
        assert_eq!(conditions.matches("> ([").count(), 1);
        assert!(conditions.contains(&format!("<{}/pane> ", case.server.config.origin)));
    }
}
#[test]
fn upload_geaenderte_quelle_gleicher_groesse_und_zeit_wird_vor_move_erkannt() {
    let case = Case::new(b"Original");
    let path = case.source_root.join("Datei.txt");
    let time = path.metadata().unwrap().modified().unwrap();
    case.server.state.lock().unwrap().source_change_on_put = Some(path.clone());
    let (_, result) = case.run(&Cancel::new(), &mut |_| {});
    let error = result.unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::PlanDrift, "{error}");
    assert_eq!(path.metadata().unwrap().modified().unwrap(), time);
    assert_eq!(case.count("PUT"), 1);
    assert_eq!(case.count("MOVE"), 0);
    assert_eq!(case.count("DELETE"), 1);
    assert!(case.leftovers().is_empty());
}
#[test]
fn upload_abbruch_vor_und_waehrend_des_stroms_erhaelt_die_quelle() {
    for after_bytes in [false, true] {
        let case = Case::new(&vec![0x5a; 512 * 1024]);
        let cancel = Cancel::new();
        if !after_bytes {
            cancel.cancel();
        }
        let (_, result) = case.run(&cancel, &mut |progress| {
            if after_bytes && progress.bytes_done >= 128 * 1024 {
                cancel.cancel();
            }
        });
        if after_bytes {
            let QueueExecutionResult::File(stats) = result.unwrap() else {
                panic!()
            };
            assert!(stats.cancelled);
            assert_eq!(stats.files, 0);
            assert_eq!(case.count("PUT"), 1);
            assert_eq!(case.count("DELETE"), 1);
        } else {
            assert!(result.is_err());
            assert_eq!(case.count("PUT"), 0);
        }
        assert_eq!(case.count("MOVE"), 0);
        assert!(case.leftovers().is_empty());
        assert_eq!(
            std::fs::read(case.source_root.join("Datei.txt")).unwrap(),
            vec![0x5a; 512 * 1024]
        );
    }
}
#[test]
fn upload_deleteverlust_und_erfolgsantwort_ohne_abwesenheit_bleiben_unklar() {
    for fault in [
        server::Fault::LostAfterDelete,
        server::Fault::DeleteLeavesFile,
    ] {
        let case = Case::new(b"Inhalt");
        let cancel = Cancel::new();
        {
            let mut state = case.server.state.lock().unwrap();
            state.fault = fault;
            state.cancel_on_put = Some(cancel.clone());
        }
        let (_, result) = case.run(&cancel, &mut |_| {});
        let error = result.unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear, "{error}");
        assert_eq!(case.count("PUT"), 1);
        assert_eq!(case.count("MOVE"), 0);
        assert_eq!(case.count("DELETE"), 1);
        assert_eq!(
            case.leftovers().len(),
            usize::from(fault == server::Fault::DeleteLeavesFile)
        );
    }
}
#[test]
fn upload_unvollstaendiges_listing_und_schwache_eltern_sperren_vor_put() {
    for fault in [
        server::Fault::PartialListing,
        server::Fault::MissingParentEtag,
        server::Fault::WeakParentEtag,
        server::Fault::ParentSwapAfterList,
    ] {
        let case = Case::new(b"Inhalt");
        case.server.state.lock().unwrap().fault = fault;
        let (_, result) = case.run(&Cancel::new(), &mut |_| {});
        assert!(result.is_err(), "falscher Erfolg");
        assert_eq!(case.count("PUT"), 0);
        assert_eq!(case.count("MOVE"), 0);
        assert_eq!(case.count("DELETE"), 0);
        case.assert_no_probes();
    }
}
#[test]
fn upload_parent_und_profilrootwechsel_bleiben_ohne_retry_mit_ehrlicher_wirkungsbilanz() {
    for fault in [
        server::Fault::ParentSwapAtPut,
        server::Fault::ProfileRootSwapAtPut,
        server::Fault::ParentSwapAtMove,
        server::Fault::ProfileRootSwapAtMove,
        server::Fault::SourceSwapAtMove,
    ] {
        let mut case = Case::new(b"Original");
        if matches!(
            fault,
            server::Fault::ProfileRootSwapAtPut | server::Fault::ProfileRootSwapAtMove
        ) {
            std::fs::create_dir(case.server.root.path().join("profil")).unwrap();
            std::fs::rename(
                case.server.root.path().join("pane"),
                case.server.root.path().join("profil/pane"),
            )
            .unwrap();
            case.server.config.requested_root = "/profil".into();
        }
        case.server.state.lock().unwrap().fault = fault;
        let (_, result) = case.run(&Cancel::new(), &mut |_| {});
        assert!(result.is_err());
        assert_eq!(case.count("PUT"), 1);
        assert!(case.count("MOVE") <= 1);
        assert_eq!(case.count("DELETE"), 0);
        case.assert_no_probes();
        assert_eq!(
            std::fs::read(case.source_root.join("Datei.txt")).unwrap(),
            b"Original"
        );
        let newroot = case.server.root.path().join(
            if matches!(
                fault,
                server::Fault::ProfileRootSwapAtPut | server::Fault::ProfileRootSwapAtMove
            ) {
                "profil/pane"
            } else {
                "pane"
            },
        );
        if fault == server::Fault::ProfileRootSwapAtMove {
            assert_eq!(
                std::fs::read(newroot.join("Datei.txt")).unwrap(),
                b"Original"
            );
        } else {
            assert!(!newroot.join("Datei.txt").exists());
        }
    }
}
#[test]
fn upload_falsche_get_bytes_verhindern_commit() {
    let case = Case::new(b"Original");
    case.server.state.lock().unwrap().fault = server::Fault::WrongGetBytes;
    let (_, result) = case.run(&Cancel::new(), &mut |_| {});
    assert!(result.is_err());
    assert_eq!(case.count("PUT"), 1);
    assert_eq!(case.count("MOVE"), 0);
    assert_eq!(case.count("DELETE"), 1);
    assert!(case.leftovers().is_empty());
}
#[test]
fn upload_queue_reopen_verlangt_frische_wirkungsbestaetigung() {
    let case = Case::new(b"Original");
    let journal = case.config.path().join("queue.json");
    let queue = OperationQueue::open(&journal).unwrap();
    queue
        .enqueue(operation(&case.server, &case.source_root, &["Datei.txt"]))
        .unwrap();
    drop(queue);
    let queue = OperationQueue::open(&journal).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let id = entry.operation_id.clone();
    let result = execute_provider_file_queue_entry(
        &queue,
        entry,
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(server::TestSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(matches!(
        result,
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    assert_eq!(case.count("PUT"), 0);
    let entry = &queue.snapshot().entries[0];
    assert_eq!(entry.operation_id, id);
    assert!(matches!(
        entry.attention,
        Some(QueueAttention::BestEffortCommitDowngrade { .. })
    ));
}

#[test]
fn upload_abbruch_nach_put_loescht_nur_eigene_datei_und_meldet_abgebrochen() {
    let case = Case::new(b"Original");
    let cancel = Cancel::new();
    case.server.state.lock().unwrap().cancel_on_put = Some(cancel.clone());
    let (queue, result) = case.run(&cancel, &mut |_| {});
    let QueueExecutionResult::File(stats) = result.unwrap() else {
        panic!()
    };
    assert!(stats.cancelled);
    assert_eq!(stats.files, 0);
    assert_eq!(case.count("PUT"), 1);
    assert_eq!(case.count("DELETE"), 1);
    assert_eq!(case.count("MOVE"), 0);
    assert!(case.leftovers().is_empty());
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Cancelled);
}
#[test]
fn upload_vorhandene_case_und_unicode_ziele_bleiben_erhalten() {
    for existing in ["Datei.txt", "datei.TXT"] {
        let case = Case::new(b"Original");
        std::fs::write(
            case.server.root.path().join("pane").join(existing),
            b"Fremd",
        )
        .unwrap();
        let (_, result) = case.run(&Cancel::new(), &mut |_| {});
        assert!(!matches!(result, Ok(QueueExecutionResult::File(ref stats)) if stats.files > 0));
        assert_eq!(case.count("PUT"), 0);
        assert_eq!(case.count("MOVE"), 0);
        assert_eq!(
            std::fs::read(case.server.root.path().join("pane").join(existing)).unwrap(),
            b"Fremd"
        );
    }
    let case = Case::new(b"Original");
    let source = case.source_root.join("Grüße.txt");
    std::fs::write(&source, b"Original").unwrap();
    let existing = "Gru\u{308}ße.txt";
    std::fs::write(
        case.server.root.path().join("pane").join(existing),
        b"Fremd",
    )
    .unwrap();
    let queue = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
    queue
        .enqueue(operation(&case.server, &case.source_root, &["Grüße.txt"]))
        .unwrap();
    let result = execute_provider_file_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(server::TestSecrets),
        &Cancel::new(),
        &mut |_| {},
    );
    assert!(!matches!(result, Ok(QueueExecutionResult::File(ref stats)) if stats.files > 0));
    assert_eq!(case.count("PUT"), 0);
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane").join(existing)).unwrap(),
        b"Fremd"
    );
}
#[test]
fn upload_private_sicht_lockert_keine_strikten_faehigkeiten_und_bilanziert_metadaten() {
    let case = Case::new(b"Original");
    let target = case.server.endpoint();
    let source = ProviderEndpoint::local(&case.source_root).unwrap();
    let InteractiveFileContexts {
        source_vfs,
        target_vfs,
        target_capabilities: capabilities,
    } = source
        .interactive_file_contexts(
            &target,
            &[case.source_root.join("Datei.txt")],
            false,
            false,
            &Cancel::new(),
        )
        .unwrap();
    let capabilities = capabilities.unwrap();
    assert!(capabilities.copy_new_best_effort);
    assert!(!capabilities.copy_new);
    assert!(!capabilities.replace);
    assert!(!capabilities.rename_noreplace);
    assert!(!target.current_transfer_capabilities().add_only);
    assert_eq!(case.count("PUT"), 0);
    case.assert_no_probes();
    let stats = fileops::copy_items(
        fileops::FileOperationRequest {
            src: source_vfs.as_ref(),
            dst: target_vfs.as_ref(),
            items: &[case.source_root.join("Datei.txt")],
            dst_dir: std::path::Path::new("/pane"),
            policy: OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(stats.files, 1);
    assert!(stats.target_copy_new_best_effort);
    assert_eq!(stats.target_write_limitations.len(), 1);
    assert!(stats.target_write_limitations[0].contains("/pane/Datei.txt"));
    assert!(!target.current_transfer_capabilities().add_only);
}
#[test]
fn upload_ungueltige_auswahl_bleibt_vor_providerzugriff_gesperrt() {
    let case = Case::new(b"Original");
    let target = case.server.endpoint();
    let source = ProviderEndpoint::local(&case.source_root).unwrap();
    let before = case.server.state.lock().unwrap().requests.len();
    for items in [
        vec![],
        vec![case.source_root.join("../Datei.txt")],
        vec![case.source_root.join("Datei.txt"); 101],
    ] {
        assert!(source
            .interactive_file_contexts(&target, &items, false, false, &Cancel::new())
            .is_err());
        assert_eq!(case.server.state.lock().unwrap().requests.len(), before);
    }
}
#[test]
fn upload_begrenzt_grosse_datei_streamt_und_prueft_den_vollstaendigen_inhalt() {
    let bytes = (0..16 * 1024 * 1024)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let case = Case::new(&bytes);
    let (_, result) = case.run(&Cancel::new(), &mut |_| {});
    let QueueExecutionResult::File(stats) = result.unwrap() else {
        panic!()
    };
    assert_eq!(stats.bytes, bytes.len() as u64);
    assert_eq!(
        blake3::hash(&std::fs::read(case.server.root.path().join("pane/Datei.txt")).unwrap()),
        blake3::hash(&bytes)
    );
    assert_eq!(case.count("PUT"), 1);
    assert_eq!(case.count("MOVE"), 1);
}

#[test]
fn upload_fehlende_und_schwache_temp_etags_erlauben_keinen_commit_oder_cleanup() {
    for fault in [server::Fault::MissingFileEtag, server::Fault::WeakFileEtag] {
        let case = Case::new(b"Original");
        case.server.state.lock().unwrap().fault = fault;
        let (_, result) = case.run(&Cancel::new(), &mut |_| {});
        let error = result.unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(case.count("PUT"), 1);
        assert_eq!(case.count("MOVE"), 0);
        assert_eq!(case.count("DELETE"), 0);
        assert_eq!(case.leftovers().len(), 1);
    }
}
#[test]
fn upload_falsche_bytes_nach_move_bleiben_unklar_und_werden_nicht_geloescht() {
    let case = Case::new(b"Original");
    case.server.state.lock().unwrap().fault = server::Fault::WrongGetAfterMove;
    let (_, result) = case.run(&Cancel::new(), &mut |_| {});
    let error = result.unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    assert_eq!(case.count("PUT"), 1);
    assert_eq!(case.count("MOVE"), 1);
    assert_eq!(case.count("DELETE"), 0);
    assert_eq!(case.leftovers(), vec!["Datei.txt"]);
}
#[test]
fn upload_mehrdateiabbruch_erhaelt_bereits_fertige_addition_und_restquellen() {
    let case = Case::new(b"Erste");
    std::fs::write(case.source_root.join("Zweite.txt"), vec![0x5a; 512 * 1024]).unwrap();
    let queue = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
    queue
        .enqueue(operation(
            &case.server,
            &case.source_root,
            &["Datei.txt", "Zweite.txt"],
        ))
        .unwrap();
    let cancel = Cancel::new();
    let result = execute_provider_file_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(server::TestSecrets),
        &cancel,
        &mut |p| {
            if p.files_done == 1 && p.bytes_done >= 128 * 1024 {
                cancel.cancel();
            }
        },
    )
    .unwrap();
    let QueueExecutionResult::File(stats) = result else {
        panic!()
    };
    assert!(stats.cancelled);
    assert_eq!(stats.files, 1);
    assert_eq!(stats.target_write_limitations.len(), 1);
    assert_eq!(case.count("PUT"), 2);
    assert_eq!(case.count("MOVE"), 1);
    assert_eq!(case.count("DELETE"), 1);
    assert_eq!(case.leftovers(), vec!["Datei.txt"]);
    assert_eq!(
        std::fs::read(case.source_root.join("Datei.txt")).unwrap(),
        b"Erste"
    );
    assert_eq!(
        std::fs::read(case.source_root.join("Zweite.txt")).unwrap(),
        vec![0x5a; 512 * 1024]
    );
}

#[test]
fn upload_queue_bindet_auch_nichtkanonische_lokale_tempwurzel() {
    let case = Case::new(b"Original");
    let lexical = case._source.path();
    let queue = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
    queue
        .enqueue(operation(&case.server, lexical, &["Datei.txt"]))
        .unwrap();
    let result = execute_provider_file_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(server::TestSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(matches!(result,QueueExecutionResult::File(ref stats) if stats.files==1));
}
#[test]
fn upload_quelltausch_waehrend_des_lesens_verhindert_commit() {
    let case = Case::new(&vec![0x5a; 1024 * 1024]);
    let mut fired = false;
    let path = case.source_root.join("Datei.txt");
    let time = filetime::FileTime::from_last_modification_time(&path.metadata().unwrap());
    let (_, result) = case.run(&Cancel::new(), &mut |p| {
        if p.bytes_done >= 128 * 1024 && !fired {
            fired = true;
            std::fs::write(&path, vec![b'X'; 1024 * 1024]).unwrap();
            filetime::set_file_mtime(&path, time).unwrap();
        }
    });
    let error = result.unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::PlanDrift, "{error}");
    assert_eq!(case.count("MOVE"), 0);
    assert_eq!(case.count("PUT"), 1);
    assert_eq!(case.count("DELETE"), 1);
    assert!(case.leftovers().is_empty());
}

#[test]
fn upload_sitzungswechsel_waehrend_des_listings_stoppt_vor_put() {
    let case = Case::new(b"Original");
    let vfs = Arc::new(
        WebDavVfs::connect(case.server.config.clone(), Arc::new(server::TestSecrets)).unwrap(),
    );
    let target = ProviderEndpoint::webdav(vfs.clone(), std::path::Path::new("/pane")).unwrap();
    let source = ProviderEndpoint::local(&case.source_root).unwrap();
    let contexts = source
        .interactive_file_contexts(
            &target,
            &[case.source_root.join("Datei.txt")],
            false,
            false,
            &Cancel::new(),
        )
        .unwrap();
    case.server.state.lock().unwrap().hold_parent_list = true;
    let source_path = case.source_root.join("Datei.txt");
    let worker = std::thread::spawn(move || {
        fileops::copy_items(
            fileops::FileOperationRequest {
                src: contexts.source_vfs.as_ref(),
                dst: contexts.target_vfs.as_ref(),
                items: &[source_path],
                dst_dir: std::path::Path::new("/pane"),
                policy: OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
    });
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !case.server.state.lock().unwrap().parent_list_paused && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(case.server.state.lock().unwrap().parent_list_paused);
    vfs.reconnect().unwrap();
    case.server.state.lock().unwrap().resume_parent_list = true;
    let error = worker.join().unwrap().unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::PlanDrift, "{error}");
    assert_eq!(case.count("PUT"), 0);
    assert_eq!(case.count("MOVE"), 0);
}

#[test]
fn upload_fremd_beschreibbarer_intentordner_sperrt_vor_put() {
    use std::os::unix::fs::PermissionsExt;
    let case = Case::new(b"Original");
    std::fs::set_permissions(case.config.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
    let (_, result) = case.run(&Cancel::new(), &mut |_| {});
    assert_eq!(
        error_code(&result.unwrap_err()),
        ErrorCode::PermissionDenied
    );
    assert_eq!(case.count("PUT"), 0);
    assert_eq!(case.count("MOVE"), 0);
    assert_eq!(case.count("DELETE"), 0);
}

#[test]
fn webdav_upload_crash_child() {
    let Some(config) = std::env::var_os("DOPPELDECKER_WEBDAV_CRASH_FIXTURE") else {
        return;
    };
    let config = std::path::PathBuf::from(config);
    let operation: QueueOperation =
        serde_json::from_slice(&std::fs::read(config.join("operation.json")).unwrap()).unwrap();
    let queue = OperationQueue::open(config.join("queue.json")).unwrap();
    queue.enqueue(operation).unwrap();
    execute_provider_file_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(server::TestSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
}
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
#[test]
fn upload_echter_prozessabbruch_erhaelt_temp_und_commitintents_ohne_wiederholung() {
    for after_move in [false, true] {
        let case = Case::new(b"Original");
        case.server.state.lock().unwrap().hold_upload_get = Some(after_move);
        std::fs::write(
            case.config.path().join("operation.json"),
            serde_json::to_vec(&operation(&case.server, &case.source_root, &["Datei.txt"]))
                .unwrap(),
        )
        .unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "webdav_upload_crash_child", "--nocapture"])
            .env("DOPPELDECKER_WEBDAV_CRASH_FIXTURE", case.config.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut child = ChildGuard(child);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !case.server.state.lock().unwrap().upload_get_paused
            && std::time::Instant::now() < end
        {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "Crashkind endete vor der Haltestelle"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(case.server.state.lock().unwrap().upload_get_paused);
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        case.server.state.lock().unwrap().resume_upload_get = true;
        let queue = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
        let snapshot = queue.snapshot();
        let entry = &snapshot.entries[0];
        assert_eq!(entry.state, QueueState::Interrupted);
        assert_eq!(entry.error_code, Some(ErrorCode::OutcomeUnclear));
        assert!(entry
            .temp_recovery_paths
            .contains(&"/pane/Datei.txt".into()));
        assert!(entry
            .temp_recovery_paths
            .iter()
            .any(|p| p.contains("fileop-tmp")));
        assert_eq!(
            error_code(&queue.retry(&entry.operation_id).unwrap_err()),
            ErrorCode::OutcomeUnclear
        );
        assert_eq!(case.count("PUT"), 1);
        assert_eq!(case.count("MOVE"), usize::from(after_move));
        assert_eq!(case.count("DELETE"), 0);
        case.assert_no_probes();
        let leftovers = case.leftovers();
        assert_eq!(leftovers.len(), 1);
        assert_eq!(leftovers[0] == "Datei.txt", after_move);
        assert_eq!(
            std::fs::read(case.server.root.path().join("pane").join(&leftovers[0])).unwrap(),
            b"Original"
        );
        assert_eq!(
            std::fs::read(case.source_root.join("Datei.txt")).unwrap(),
            b"Original"
        );
    }
}

fn bound_profile(case: &Case) -> (std::path::PathBuf, RemoteProfileBinding) {
    let path = case.config.path().join("webdav-profiles.json");
    let saved = save_webdav_profile_versioned(
        &path,
        WebDavProfile {
            id: "upload".into(),
            name: "Upload".into(),
            connection: case.server.config.clone(),
        },
    )
    .unwrap();
    (
        path,
        RemoteProfileBinding::new(RemoteProfileProtocol::Webdav, "upload", saved.revision).unwrap(),
    )
}
#[test]
fn upload_profilwechsel_vor_providerzugriff_und_nach_put_sperrt_weitere_mutationen() {
    for during_put in [false, true] {
        let case = Case::new(b"Original");
        let (path, binding) = bound_profile(&case);
        let queue = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
        let entry = queue
            .enqueue_claimed_with_profile_bindings(
                operation(&case.server, &case.source_root, &["Datei.txt"]),
                vec![binding],
            )
            .unwrap();
        let update = || {
            save_webdav_profile_versioned(
                &path,
                WebDavProfile {
                    id: "upload".into(),
                    name: "Geändert".into(),
                    connection: case.server.config.clone(),
                },
            )
            .unwrap()
        };
        if !during_put {
            assert!(update().changed);
        }
        let mut fired = false;
        let error = execute_provider_file_queue_entry(
            &queue,
            entry,
            &LocalQueueExecution {
                sandbox: None,
                sync_quarantine: None,
                max_delete_entries: 100,
                max_delete_fraction: 0.25,
            },
            Arc::new(server::TestSecrets),
            &Cancel::new(),
            &mut |p| {
                if during_put && !fired && p.bytes_done > 0 {
                    fired = true;
                    assert!(update().changed);
                }
            },
        )
        .unwrap_err();
        assert_eq!(
            error_code(&error),
            if during_put {
                ErrorCode::OutcomeUnclear
            } else {
                ErrorCode::PlanDrift
            },
            "{error}"
        );
        assert_eq!(case.count("PUT"), usize::from(during_put));
        assert_eq!(case.count("MOVE"), 0);
        assert_eq!(case.count("DELETE"), 0);
        case.assert_no_probes();
        if !during_put {
            assert!(case.server.state.lock().unwrap().requests.is_empty());
        } else {
            assert_eq!(case.leftovers().len(), 1);
        }
        assert_eq!(
            std::fs::read(case.source_root.join("Datei.txt")).unwrap(),
            b"Original"
        );
    }
}
#[test]
fn upload_profilbindung_laesst_unabhaengige_profilrevisionen_zu() {
    let case = Case::new(b"Original");
    let (path, binding) = bound_profile(&case);
    let queue = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
    let entry = queue
        .enqueue_claimed_with_profile_bindings(
            operation(&case.server, &case.source_root, &["Datei.txt"]),
            vec![binding],
        )
        .unwrap();
    save_webdav_profile_versioned(
        &path,
        WebDavProfile {
            id: "anderes".into(),
            name: "Unabhängig".into(),
            connection: case.server.config.clone(),
        },
    )
    .unwrap();
    let result = execute_provider_file_queue_entry(
        &queue,
        entry,
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(server::TestSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(matches!(result,QueueExecutionResult::File(ref stats) if stats.files==1));
    assert_eq!(case.count("PUT"), 1);
    assert_eq!(case.count("MOVE"), 1);
}

#[test]
fn webdav_baum_mit_zwei_ebenen_und_leerem_ordner_ohne_loeschprobe() {
    let server = server::Server::new();
    server.state.lock().unwrap().deny_collection_probes = true;
    let source = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(source.path()).unwrap();
    std::fs::create_dir_all(root.join("Baum/Unterordner/Leer")).unwrap();
    std::fs::write(root.join("Baum/Datei.txt"), b"Inhalt").unwrap();
    std::fs::write(root.join("Baum/Unterordner/literal%2F.txt"), b"").unwrap();
    let config = private_queue_directory();
    let queue = OperationQueue::open(config.path().join("queue.json")).unwrap();
    queue.enqueue(operation(&server, &root, &["Baum"])).unwrap();
    let claimed = queue.claim_next().unwrap().unwrap();
    let first = execute_provider_file_queue_entry(
        &queue,
        claimed,
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(server::TestSecrets),
        &Cancel::new(),
        &mut |_| {},
    );
    assert!(
        matches!(first, Ok(QueueExecutionResult::AwaitingConfirmation { .. })),
        "{first:?}"
    );
    assert!(!server
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .any(|(m, _)| matches!(m.as_str(), "MKCOL" | "PUT" | "MOVE" | "DELETE")));
    let entry = queue.snapshot().entries[0].clone();
    let QueueAttention::SourceTreeChanged { confirmation, .. } = entry.attention.unwrap() else {
        panic!("Keine Baumbestätigung")
    };
    queue
        .approve_attention(&entry.operation_id, &confirmation)
        .unwrap();
    let result = execute_provider_file_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        },
        Arc::new(server::TestSecrets),
        &Cancel::new(),
        &mut |_| {},
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        std::fs::read(server.root.path().join("pane/Baum/Datei.txt")).unwrap(),
        b"Inhalt"
    );
    assert!(server
        .root
        .path()
        .join("pane/Baum/Unterordner/Leer")
        .is_dir());
    assert_eq!(
        std::fs::read(
            server
                .root
                .path()
                .join("pane/Baum/Unterordner/literal%2F.txt")
        )
        .unwrap(),
        b""
    );
    let state = server.state.lock().unwrap();
    assert_eq!(
        state.requests.iter().filter(|(m, _)| m == "MKCOL").count(),
        3
    );
    assert_eq!(state.requests.iter().filter(|(m, _)| m == "PUT").count(), 2);
    assert_eq!(
        state.requests.iter().filter(|(m, _)| m == "MOVE").count(),
        2
    );
    assert_eq!(
        state.requests.iter().filter(|(m, _)| m == "DELETE").count(),
        0
    );
    assert!(state.requests.iter().all(|(_, p)| !p.contains("probe")));
    assert_eq!(
        std::fs::read_dir(config.path())
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|v| v == "json")
                && e.as_ref().unwrap().file_name() != "queue.json")
            .count(),
        0
    );
}
