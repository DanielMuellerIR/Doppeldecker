//! Begrenzte additive Bäume durch denselben Core-/Queuekopierer wie F5 und CLI.
#[path = "support/webdav_namespace.rs"]
mod server;
use doppeldecker_core::*;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
struct Case {
    server: server::Server,
    _source: tempfile::TempDir,
    source: PathBuf,
    config: tempfile::TempDir,
}
impl Case {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let source = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(source.join("Baum/Unter/Leer")).unwrap();
        std::fs::write(source.join("Baum/A.txt"), b"Inhalt").unwrap();
        std::fs::write(source.join("Baum/Unter/literal%2F.txt"), b"").unwrap();
        std::fs::write(source.join("Direkt.txt"), b"Direkt").unwrap();
        let server = server::Server::new();
        server.state.lock().unwrap().deny_collection_probes = true;
        std::fs::write(server.root.path().join("Sentinel"), b"Erhalten").unwrap();
        Self {
            server,
            _source: tmp,
            source,
            config: tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .unwrap(),
        }
    }
    fn prepared(&self, names: &[&str]) -> QueueOperation {
        let source = ProviderEndpoint::local(&self.source).unwrap();
        let target = self.server.endpoint();
        let items = names
            .iter()
            .map(|name| self.source.join(name))
            .collect::<Vec<_>>();
        let contexts = source
            .interactive_file_contexts(&target, &items, false, false, &Cancel::new())
            .unwrap();
        let expected_conflicts = fileops::find_recursive_conflicts(
            contexts.source_vfs.as_ref(),
            contexts.target_vfs.as_ref(),
            Path::new("/pane"),
            &items,
        )
        .unwrap();
        QueueOperation::ProviderFile {
            kind: FileQueueKind::Copy,
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: self.source.to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(self.server.config.clone()),
                root: "/pane".into(),
            }),
            items: items
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            destination: "/pane".into(),
            overwrite: false,
            expected_conflicts,
            source_tree_expected: contexts
                .source_vfs
                .interactive_source_tree()
                .unwrap()
                .map(Box::new),
            keep_both_targets: vec![],
            expected_conflict_proofs: vec![],
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: Some(true),
            replace_best_effort_expected: Some(false),
            remove_source_best_effort_expected: None,
        }
    }
    fn run(
        &self,
        op: QueueOperation,
        cancel: &Cancel,
        progress: &mut dyn FnMut(&Progress),
    ) -> (OperationQueue, std::io::Result<QueueExecutionResult>) {
        let queue = OperationQueue::open(self.config.path().join("queue.json")).unwrap();
        queue.enqueue(op).unwrap();
        let result = execute_provider_file_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &execution(),
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
    fn intact(&self) {
        assert_eq!(
            std::fs::read(self.server.root.path().join("Sentinel")).unwrap(),
            b"Erhalten"
        );
        assert_eq!(
            std::fs::read(self.source.join("Baum/A.txt")).unwrap(),
            b"Inhalt"
        );
        assert!(self
            .server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|(m, p)| !p.contains("probe")
                && (!matches!(m.as_str(), "MKCOL" | "PUT" | "MOVE" | "DELETE")
                    || p.starts_with("/pane/"))));
        assert!(self
            .server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(m, _)| m == "DELETE")
            .all(|(_, p)| p.contains("fileop-tmp")));
    }
}
fn execution() -> LocalQueueExecution {
    LocalQueueExecution {
        sandbox: None,
        sync_quarantine: None,
        max_delete_entries: 100,
        max_delete_fraction: 0.25,
    }
}
#[test]
fn baum_mehrfachauswahl_fuehrt_exakte_ordner_zusammen_und_erhaelt_kollisionen() {
    let case = Case::new();
    let pane = case.server.root.path().join("pane");
    std::fs::create_dir_all(pane.join("Baum/Unter")).unwrap();
    std::fs::write(pane.join("Baum/A.txt"), b"Altbestand").unwrap();
    std::fs::write(pane.join("Baum/Fremd"), b"Erhalten").unwrap();
    let op = case.prepared(&["Baum", "Direkt.txt"]);
    let (queue, result) = case.run(op, &Cancel::new(), &mut |_| {});
    let Ok(QueueExecutionResult::File(stats)) = result else {
        panic!("{result:?}")
    };
    assert_eq!((stats.files, stats.skipped), (2, 1));
    assert!(stats.target_copy_new_best_effort);
    for name in [
        "/pane/Baum",
        "/pane/Baum/Unter",
        "/pane/Baum/Unter/Leer",
        "/pane/Direkt.txt",
    ] {
        assert!(
            stats
                .target_write_limitations
                .iter()
                .any(|v| v.contains(name)),
            "{stats:?}"
        );
    }
    assert_eq!(
        std::fs::read(pane.join("Baum/A.txt")).unwrap(),
        b"Altbestand"
    );
    assert_eq!(std::fs::read(pane.join("Baum/Fremd")).unwrap(), b"Erhalten");
    assert!(pane.join("Baum/Unter/Leer").is_dir());
    assert_eq!(case.count("MKCOL"), 1);
    assert_eq!(case.count("DELETE"), 0);
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    case.intact();
}
#[test]
fn baum_mkcol_antwortverlust_und_readbackfehler_erhalten_ohne_delete_ihren_intent() {
    for fault in [
        server::Fault::LostAfterCreate,
        server::Fault::ErrorAfterCreate,
        server::Fault::MissingReadback,
        server::Fault::WrongKind,
        server::Fault::WrongCreatedEtag,
        server::Fault::NonemptyCreated,
        server::Fault::MissingParentChild,
        server::Fault::MissingCreatedToken,
    ] {
        let case = Case::new();
        let op = case.prepared(&["Baum"]);
        case.server.state.lock().unwrap().fault = fault;
        let (queue, result) = case.run(op, &Cancel::new(), &mut |_| {});
        let error = result.unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear, "{error}");
        assert_eq!(
            (case.count("MKCOL"), case.count("PUT"), case.count("DELETE")),
            (1, 0, 0)
        );
        assert!(case.server.root.path().join("pane/Baum").exists());
        let snapshot = queue.snapshot();
        assert!(snapshot.entries[0]
            .temp_recovery_paths
            .iter()
            .any(|p| p == "/pane/Baum"));
        let id = snapshot.entries[0].operation_id.clone();
        drop(queue);
        let reopened = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
        assert!(reopened.retry(&id).is_err());
        assert!(reopened.snapshot().entries[0]
            .temp_recovery_paths
            .iter()
            .any(|p| p == "/pane/Baum"));
        case.intact();
    }
}
#[test]
fn baum_schwache_listings_stoppen_kinderwirkungen() {
    for fault in [
        server::Fault::PartialListing,
        server::Fault::WeakParentEtag,
        server::Fault::MissingParentEtag,
    ] {
        let case = Case::new();
        let op = case.prepared(&["Baum"]);
        case.server.state.lock().unwrap().fault = fault;
        let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
        assert!(result.is_err());
        assert_eq!(case.count("MKCOL"), 0);
        assert_eq!(case.count("PUT"), 0);
        case.intact();
    }
}
#[test]
fn baum_eigene_wirkungen_duerfen_collectionversionen_aendern() {
    let case = Case::new();
    case.server.state.lock().unwrap().changing_collection_tokens = true;
    let op = case.prepared(&["Baum"]);
    let (queue, result) = case.run(op, &Cancel::new(), &mut |_| {});
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    assert!(queue.snapshot().entries[0].temp_recovery_paths.is_empty());
    assert_eq!(case.count("MKCOL"), 3);
    assert_eq!(case.count("PUT"), 2);
    assert_eq!(case.count("MOVE"), 2);
    assert_eq!(case.count("DELETE"), 0);
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/Baum/A.txt")).unwrap(),
        b"Inhalt"
    );
    assert_eq!(
        std::fs::read(
            case.server
                .root
                .path()
                .join("pane/Baum/Unter/literal%2F.txt")
        )
        .unwrap(),
        b""
    );
    assert!(case
        .server
        .root
        .path()
        .join("pane/Baum/Unter/Leer")
        .is_dir());
    case.intact();
}
#[test]
fn baum_inhaltswechsel_mit_gleichem_umfang_verlangt_neue_bestaetigung_vor_mkcol() {
    let case = Case::new();
    let op = case.prepared(&["Baum"]);
    std::fs::write(case.source.join("Baum/A.txt"), b"Anders").unwrap();
    let (queue, result) = case.run(op, &Cancel::new(), &mut |_| {});
    assert!(
        matches!(
            result,
            Ok(QueueExecutionResult::AwaitingConfirmation { .. })
        ),
        "{result:?}"
    );
    assert!(matches!(
        queue.snapshot().entries[0].attention,
        Some(QueueAttention::SourceTreeChanged { .. })
    ));
    assert_eq!(case.count("MKCOL"), 0);
    assert_eq!(case.count("PUT"), 0);
}
#[test]
fn baum_nach_teiladdition_abbrechen_erhaelt_ordner_und_fertige_datei() {
    let case = Case::new();
    let op = case.prepared(&["Baum"]);
    let cancel = Cancel::new();
    let stop = cancel.clone();
    let (queue, result) = case.run(op, &cancel, &mut |progress| {
        if progress.files_done == 1 {
            stop.cancel()
        }
    });
    assert_eq!(error_code(&result.unwrap_err()), ErrorCode::OutcomeUnclear);
    assert_eq!(case.count("DELETE"), 0);
    assert_eq!(case.count("PUT"), 1);
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/Baum/A.txt")).unwrap(),
        b"Inhalt"
    );
    assert!(queue.snapshot().entries[0]
        .temp_recovery_paths
        .iter()
        .any(|p| p == "/pane/Baum"));
    case.intact();
}
#[test]
fn baum_put_move_und_deletefehler_erhalten_alle_teilpfade_ohne_ordnercleanup() {
    for fault in [
        server::Fault::LostAfterPut,
        server::Fault::LostAfterMove,
        server::Fault::WrongGetAfterMove,
        server::Fault::MissingPutEtag,
        server::Fault::WeakPutEtag,
    ] {
        let case = Case::new();
        let op = case.prepared(&["Baum"]);
        case.server.state.lock().unwrap().fault = fault;
        let (queue, result) = case.run(op, &Cancel::new(), &mut |_| {});
        assert_eq!(error_code(&result.unwrap_err()), ErrorCode::OutcomeUnclear);
        assert_eq!(case.count("MKCOL"), 1);
        assert_eq!(case.count("PUT"), 1);
        assert_eq!(case.count("DELETE"), 0);
        assert!(queue.snapshot().entries[0]
            .temp_recovery_paths
            .iter()
            .any(|p| p.contains("/pane/Baum/.A.txt")));
        assert!(queue.snapshot().entries[0]
            .temp_recovery_paths
            .iter()
            .any(|p| p == "/pane/Baum"));
        case.intact();
    }
}
#[test]
fn baum_case_typkonflikte_werden_erhalten_oder_vor_wirkung_abgelehnt() {
    for name in ["baum", "Baum"] {
        let case = Case::new();
        std::fs::write(case.server.root.path().join("pane").join(name), b"Fremd").unwrap();
        let op = case.prepared(&["Baum"]);
        let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
        if name == "Baum" {
            assert!(result.is_ok(), "{result:?}")
        } else if let Ok(QueueExecutionResult::File(stats)) = &result {
            assert_eq!(stats.skipped, 1);
        } else {
            assert!(result.is_err());
        }
        assert_eq!(case.count("MKCOL"), 0);
        assert_eq!(case.count("PUT"), 0);
        case.intact();
    }
}

#[test]
fn baum_unicode_namensgruppen_lassen_unabhaengige_inhalte_nutzbar() {
    let case = Case::new();
    std::fs::write(case.source.join("Baum/é"), b"A").unwrap();
    std::fs::write(case.source.join("Baum/e\u{301}"), b"B").unwrap();
    let distinct = std::fs::read_dir(case.source.join("Baum")).unwrap().count() == 4;
    #[cfg(target_os = "linux")]
    assert!(distinct, "Linux-Gegenlauf braucht eine namensgenaue Quelle");
    let op = case.prepared(&["Baum"]);
    let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
    let Ok(QueueExecutionResult::File(stats)) = result else {
        panic!("{result:?}")
    };
    let tree = stats.source_tree.unwrap();
    if distinct {
        assert_eq!(
            (
                stats.files,
                stats.skipped,
                tree.entries,
                tree.files,
                tree.directories
            ),
            (2, 2, 7, 2, 3)
        );
        assert_eq!(tree.omitted.len(), 2);
        for name in ["é", "e\u{301}"] {
            assert!(tree
                .omitted
                .iter()
                .any(|item| item.path == case.source.join("Baum").join(name).to_str().unwrap()));
            assert!(!case
                .server
                .root
                .path()
                .join("pane/Baum")
                .join(name)
                .exists());
        }
    } else {
        // Eine normalisierende Quelle hat nur einen Eintrag, also keine Gruppe.
        assert_eq!((stats.files, stats.skipped, tree.omitted.len()), (3, 0, 0));
    }
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/Baum/A.txt")).unwrap(),
        b"Inhalt"
    );
    case.intact();
}

#[test]
fn baum_case_namensgruppe_laesst_unabhaengige_inhalte_nutzbar() {
    let case = Case::new();
    std::fs::write(case.source.join("Baum/Bericht"), b"A").unwrap();
    std::fs::write(case.source.join("Baum/bericht"), b"B").unwrap();
    let distinct = std::fs::read_dir(case.source.join("Baum")).unwrap().count() == 4;
    let op = case.prepared(&["Baum"]);
    let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
    let Ok(QueueExecutionResult::File(stats)) = result else {
        panic!("{result:?}")
    };
    let tree = stats.source_tree.unwrap();
    if distinct {
        assert_eq!((stats.files, stats.skipped, tree.omitted.len()), (2, 2, 2));
        for name in ["Bericht", "bericht"] {
            assert!(tree
                .omitted
                .iter()
                .any(|item| item.path == case.source.join("Baum").join(name).to_str().unwrap()));
            assert!(!case
                .server
                .root
                .path()
                .join("pane/Baum")
                .join(name)
                .exists());
        }
    } else {
        assert_eq!((stats.files, stats.skipped, tree.omitted.len()), (3, 0, 0));
    }
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/Baum/A.txt")).unwrap(),
        b"Inhalt"
    );
    case.intact();
}

#[cfg(target_os = "linux")]
#[test]
fn baum_case_namensgruppe_erhaelt_ausgelassenen_teilbaum_und_erkennt_drift() {
    let case = Case::new();
    std::fs::create_dir(case.source.join("Baum/Bericht")).unwrap();
    std::fs::write(case.source.join("Baum/Bericht/Kind"), b"Erhalten").unwrap();
    std::fs::write(case.source.join("Baum/bericht"), b"Erhalten").unwrap();
    let op = case.prepared(&["Baum"]);
    let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
    let Ok(QueueExecutionResult::File(stats)) = result else {
        panic!("{result:?}")
    };
    let tree = stats.source_tree.unwrap();
    assert_eq!(
        (
            stats.files,
            stats.skipped,
            tree.entries,
            tree.files,
            tree.directories,
            tree.omitted.len()
        ),
        (2, 2, 7, 2, 3, 2)
    );
    assert!(!case.server.root.path().join("pane/Baum/Bericht").exists());
    assert!(!case.server.root.path().join("pane/Baum/bericht").exists());
    assert_eq!(
        std::fs::read(case.source.join("Baum/Bericht/Kind")).unwrap(),
        b"Erhalten"
    );
    assert_eq!(
        std::fs::read(case.source.join("Baum/bericht")).unwrap(),
        b"Erhalten"
    );
    case.intact();

    let case = Case::new();
    std::fs::write(case.source.join("Baum/Bericht"), b"A").unwrap();
    std::fs::write(case.source.join("Baum/bericht"), b"B").unwrap();
    let op = case.prepared(&["Baum"]);
    let QueueOperation::ProviderFile {
        source_tree_expected: Some(before),
        ..
    } = &op
    else {
        panic!("Keine bestätigte Quellbaumbilanz");
    };
    assert_eq!(
        (before.files, before.directories, before.omitted.len()),
        (2, 3, 2)
    );
    std::fs::rename(
        case.source.join("Baum/bericht"),
        case.source.join("Baum/Eindeutig"),
    )
    .unwrap();
    let (queue, result) = case.run(op, &Cancel::new(), &mut |_| {});
    assert!(
        matches!(
            result,
            Ok(QueueExecutionResult::AwaitingConfirmation { .. })
        ),
        "Eine geänderte Auslassungsgruppe braucht frische Zustimmung: {result:?}"
    );
    let snapshot = queue.snapshot();
    let Some(QueueAttention::SourceTreeChanged {
        current_tree: Some(tree),
        ..
    }) = &snapshot.entries[0].attention
    else {
        panic!(
            "Keine gebundene Quellbaumänderung: {:?}",
            snapshot.entries[0]
        );
    };
    assert_eq!(
        (tree.files, tree.directories, tree.omitted.len()),
        (4, 3, 0)
    );
    assert_eq!(
        (case.count("MKCOL"), case.count("PUT"), case.count("MOVE")),
        (0, 0, 0)
    );
}

#[test]
fn baum_spaete_kollision_und_elterntausch_beim_mkcol_bleiben_ohne_kinderwirkung() {
    for fault in [
        server::Fault::LateCollision,
        server::Fault::ParentSwapAtMkdir,
    ] {
        let case = Case::new();
        let op = case.prepared(&["Baum"]);
        case.server.state.lock().unwrap().fault = fault;
        let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
        if fault == server::Fault::ParentSwapAtMkdir {
            assert!(result.is_ok());
            assert_eq!(
                std::fs::read(case.server.root.path().join("pane/Fremd.txt")).unwrap(),
                b"Fremder Elternordner"
            );
            assert!(case.server.root.path().join("pane-before-swap").is_dir());
            continue;
        }
        assert!(result.is_err());
        assert_eq!(case.count("MKCOL"), 1);
        assert_eq!(case.count("PUT"), 0);
        assert_eq!(case.count("DELETE"), 0);
        case.intact();
        if fault == server::Fault::LateCollision {
            assert_eq!(
                std::fs::read(case.server.root.path().join("pane/Baum")).unwrap(),
                b"Fremd"
            );
        }
    }
}
#[test]
fn baum_profilwechsel_nach_erster_datei_stoppt_vor_naechstem_mkcol_und_erhaelt_addition() {
    let case = Case::new();
    let op = case.prepared(&["Baum"]);
    let path = case.config.path().join("webdav-profiles.json");
    let saved = save_webdav_profile_versioned(
        &path,
        WebDavProfile {
            id: "baum".into(),
            name: "Baum".into(),
            connection: case.server.config.clone(),
        },
    )
    .unwrap();
    let queue = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
    let entry = queue
        .enqueue_claimed_with_profile_bindings(
            op,
            vec![
                RemoteProfileBinding::new(RemoteProfileProtocol::Webdav, "baum", saved.revision)
                    .unwrap(),
            ],
        )
        .unwrap();
    let mut changed = false;
    let result = execute_provider_file_queue_entry(
        &queue,
        entry,
        &execution(),
        Arc::new(server::TestSecrets),
        &Cancel::new(),
        &mut |p| {
            if !changed && p.files_done == 1 {
                changed = true;
                save_webdav_profile_versioned(
                    &path,
                    WebDavProfile {
                        id: "baum".into(),
                        name: "Neu".into(),
                        connection: case.server.config.clone(),
                    },
                )
                .unwrap();
            }
        },
    );
    assert!(changed);
    assert_eq!(error_code(&result.unwrap_err()), ErrorCode::OutcomeUnclear);
    assert_eq!(
        (
            case.count("MKCOL"),
            case.count("PUT"),
            case.count("MOVE"),
            case.count("DELETE")
        ),
        (1, 1, 1, 0)
    );
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/Baum/A.txt")).unwrap(),
        b"Inhalt"
    );
    case.intact();
}
#[test]
fn baum_auslassungen_bleiben_im_umfang_und_erzeugen_keine_remote_eintraege() {
    let case = Case::new();
    std::os::unix::fs::symlink("A.txt", case.source.join("Baum/Link")).unwrap();
    std::fs::write(case.source.join("Baum/Zeile\nUmbruch"), b"Auslassen").unwrap();
    let op = case.prepared(&["Baum"]);
    let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
    let Ok(QueueExecutionResult::File(stats)) = result else {
        panic!("{result:?}")
    };
    assert_eq!(stats.skipped, 2);
    assert_eq!(stats.source_tree.as_ref().unwrap().omitted.len(), 2);
    assert!(stats
        .source_read_limitations
        .iter()
        .any(|v| v.contains("Nicht übertragen")));
    assert!(!case.server.root.path().join("pane/Baum/Link").exists());
    assert!(!case
        .server
        .root
        .path()
        .join("pane/Baum/Zeile\nUmbruch")
        .exists());
    case.intact();
}
#[test]
fn baum_crash_child() {
    let Some(config) = std::env::var_os("DOPPELDECKER_WEBDAV_TREE_CRASH") else {
        return;
    };
    let config = PathBuf::from(config);
    let op: QueueOperation =
        serde_json::from_slice(&std::fs::read(config.join("operation.json")).unwrap()).unwrap();
    let queue = OperationQueue::open(config.join("queue.json")).unwrap();
    queue.enqueue(op).unwrap();
    execute_provider_file_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &execution(),
        Arc::new(server::TestSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
}
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
#[test]
fn baum_echter_prozessabbruch_nach_mkcol_erhaelt_ordnerintent_ohne_wiederholung() {
    let case = Case::new();
    let op = case.prepared(&["Baum"]);
    case.server.state.lock().unwrap().hold_after_mkdir = true;
    std::fs::write(
        case.config.path().join("operation.json"),
        serde_json::to_vec(&op).unwrap(),
    )
    .unwrap();
    let exe = std::env::current_exe().unwrap();
    let list = std::process::Command::new(&exe)
        .arg("--list")
        .output()
        .unwrap();
    assert!(list.status.success());
    assert_eq!(
        String::from_utf8(list.stdout)
            .unwrap()
            .lines()
            .filter(|line| *line == "baum_crash_child: test")
            .count(),
        1
    );
    let mut child = Child(
        std::process::Command::new(exe)
            .args(["--exact", "baum_crash_child"])
            .env("DOPPELDECKER_WEBDAV_TREE_CRASH", case.config.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !case.server.state.lock().unwrap().mkdir_paused {
        assert!(std::time::Instant::now() < deadline);
        assert!(child.0.try_wait().unwrap().is_none());
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    case.server.state.lock().unwrap().resume_mkdir = true;
    let queue = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
    let entry = queue.snapshot().entries[0].clone();
    assert_eq!(entry.state, QueueState::Interrupted);
    assert_eq!(entry.error_code, Some(ErrorCode::OutcomeUnclear));
    assert!(entry.temp_recovery_paths.iter().any(|p| p == "/pane/Baum"));
    assert!(queue.retry(&entry.operation_id).is_err());
    assert!(case.server.root.path().join("pane/Baum").is_dir());
    assert_eq!(
        (case.count("MKCOL"), case.count("PUT"), case.count("DELETE")),
        (1, 0, 0)
    );
    case.intact();
}
#[test]
fn baum_crash_zwischen_abgeschlossenen_additionen_bleibt_ohne_intent_unklar() {
    let case = Case::new();
    let op = case.prepared(&["Baum"]);
    let queue = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
    let entry = queue.enqueue(op).unwrap();
    let claimed = queue.claim_next().unwrap().unwrap();
    queue.mark_running(&claimed.operation_id).unwrap();
    drop(queue);
    let queue = OperationQueue::open(case.config.path().join("queue.json")).unwrap();
    let resumed = queue.snapshot().entries[0].clone();
    assert_eq!(resumed.error_code, Some(ErrorCode::OutcomeUnclear));
    assert!(resumed.temp_recovery_paths.contains(&"/pane/Baum".into()));
    assert!(queue.retry(&entry.operation_id).is_err());
    assert_eq!(case.count("MKCOL"), 0);
}

#[test]
fn baum_temp_deleteverlust_erhaelt_datei_und_ordnerpruefpfade_ohne_zweiten_versuch() {
    for fault in [
        server::Fault::LostAfterDelete,
        server::Fault::DeleteLeavesFile,
    ] {
        let case = Case::new();
        let op = case.prepared(&["Baum"]);
        let cancel = Cancel::new();
        {
            let mut state = case.server.state.lock().unwrap();
            state.fault = fault;
            state.cancel_on_put = Some(cancel.clone());
        }
        let (queue, result) = case.run(op, &cancel, &mut |_| {});
        assert_eq!(error_code(&result.unwrap_err()), ErrorCode::OutcomeUnclear);
        assert_eq!(
            (
                case.count("MKCOL"),
                case.count("PUT"),
                case.count("MOVE"),
                case.count("DELETE")
            ),
            (1, 1, 0, 1)
        );
        let entry = queue.snapshot().entries[0].clone();
        assert!(entry.temp_recovery_paths.iter().any(|p| p == "/pane/Baum"));
        assert!(entry
            .temp_recovery_paths
            .iter()
            .any(|p| p.contains("fileop-tmp")));
        case.intact();
    }
}

#[test]
fn baum_ein_verschwundener_gebundener_zielordner_wird_nicht_neu_angelegt() {
    let case = Case::new();
    let pane = case.server.root.path().join("pane");
    std::fs::create_dir(pane.join("Baum")).unwrap();
    let source = ProviderEndpoint::local(&case.source).unwrap();
    let target = case.server.endpoint();
    let items = vec![case.source.join("Baum")];
    let contexts = source
        .interactive_file_contexts(&target, &items, false, false, &Cancel::new())
        .unwrap();
    contexts
        .target_vfs
        .list_complete(Path::new("/pane/Baum"))
        .unwrap();
    std::fs::rename(pane.join("Baum"), pane.join("Alter-Baum")).unwrap();
    let result = fileops::copy_items(
        fileops::FileOperationRequest {
            src: contexts.source_vfs.as_ref(),
            dst: contexts.target_vfs.as_ref(),
            items: &items,
            dst_dir: Path::new("/pane"),
            policy: OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut |_| {},
    );
    assert!(
        result.is_err(),
        "Ein alter Ordnerbeleg wurde still ersetzt: {result:?}"
    );
    assert_eq!(case.count("MKCOL"), 0);
    assert_eq!(case.count("PUT"), 0);
    assert!(!pane.join("Baum").exists());
    case.intact();
}

#[test]
fn direkte_symlinks_und_spezialdateien_sperren_unabhaengige_dateiadditionen_nicht() {
    use std::os::unix::ffi::OsStrExt;
    let case = Case::new();
    std::os::unix::fs::symlink("Direkt.txt", case.source.join("Link")).unwrap();
    let fifo = std::ffi::CString::new(case.source.join("FIFO").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let op = case.prepared(&["Link", "Direkt.txt", "FIFO"]);
    let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
    let Ok(QueueExecutionResult::File(stats)) = result else {
        panic!("{result:?}")
    };
    assert_eq!(
        (stats.files, stats.skipped, stats.symlinks_skipped),
        (1, 2, 0)
    );
    let tree = stats.source_tree.unwrap();
    assert_eq!(
        (tree.files, tree.directories, tree.omitted.len()),
        (1, 0, 2)
    );
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/Direkt.txt")).unwrap(),
        b"Direkt"
    );
    assert!(!case.server.root.path().join("pane/Link").exists());
    assert!(!case.server.root.path().join("pane/FIFO").exists());
    assert_eq!(
        (
            case.count("MKCOL"),
            case.count("PUT"),
            case.count("MOVE"),
            case.count("DELETE")
        ),
        (0, 1, 1, 0)
    );
    case.intact();
}
#[test]
fn eine_vollstaendig_ausgelassene_auswahl_bleibt_ohne_remote_wirkung() {
    let case = Case::new();
    std::os::unix::fs::symlink("Direkt.txt", case.source.join("Link")).unwrap();
    let op = case.prepared(&["Link"]);
    let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
    let Ok(QueueExecutionResult::File(stats)) = result else {
        panic!("{result:?}")
    };
    assert_eq!((stats.files, stats.skipped), (0, 1));
    assert_eq!(stats.source_tree.unwrap().omitted.len(), 1);
    assert_eq!(
        (
            case.count("MKCOL"),
            case.count("PUT"),
            case.count("MOVE"),
            case.count("DELETE")
        ),
        (0, 0, 0, 0)
    );
    case.intact();
}

#[test]
fn baum_nicht_unveraendert_bedienbare_namen_werden_ausgelassen_ohne_umbenennung() {
    let case = Case::new();
    std::fs::create_dir(case.source.join("Baum/ Ordner ")).unwrap();
    std::fs::write(case.source.join("Baum/ Ordner /Inhalt"), b"Erhalten").unwrap();
    std::fs::write(case.source.join("Baum/ Datei .txt "), b"Erhalten").unwrap();
    let op = case.prepared(&["Baum"]);
    let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
    let Ok(QueueExecutionResult::File(stats)) = result else {
        panic!("{result:?}")
    };
    assert_eq!(stats.skipped, 2);
    assert_eq!(stats.source_tree.unwrap().omitted.len(), 2);
    for name in ["Ordner", " Ordner ", "Datei .txt", " Datei .txt "] {
        assert!(!case
            .server
            .root
            .path()
            .join("pane/Baum")
            .join(name)
            .exists());
    }
    assert_eq!(
        std::fs::read(case.source.join("Baum/ Ordner /Inhalt")).unwrap(),
        b"Erhalten"
    );
    case.intact();
}

#[test]
fn direkte_case_gruppe_laesst_good_hochladen_ohne_zielkonflikt() {
    use std::os::unix::fs::MetadataExt;
    let case = Case::new();
    for (name, content) in [
        ("a", b"lower".as_slice()),
        ("A", b"upper".as_slice()),
        ("good.txt", b"good".as_slice()),
    ] {
        std::fs::write(case.source.join(name), content).unwrap();
    }
    let distinct = std::fs::metadata(case.source.join("a")).unwrap().ino()
        != std::fs::metadata(case.source.join("A")).unwrap().ino();
    if std::env::var_os("DOPPELDECKER_TEST_CASE_SENSITIVE").is_some() {
        assert!(distinct, "Gegenlauf benötigt echte getrennte Case-Namen");
    }
    std::fs::write(case.server.root.path().join("pane/a"), b"existing").unwrap();
    let op = case.prepared(&["a", "A", "good.txt"]);
    let QueueOperation::ProviderFile {
        expected_conflicts,
        source_tree_expected,
        ..
    } = &op
    else {
        panic!()
    };
    assert!(expected_conflicts.is_empty());
    let tree = source_tree_expected.as_ref().unwrap();
    assert_eq!((tree.files, tree.bytes, tree.omitted.len()), (1, 4, 2));
    let mut totals = Vec::new();
    let (_, result) = case.run(op, &Cancel::new(), &mut |p| {
        totals.push((p.files_total, p.bytes_total))
    });
    let Ok(QueueExecutionResult::File(stats)) = result else {
        panic!("{result:?}")
    };
    assert_eq!((stats.files, stats.skipped, stats.bytes), (1, 2, 4));
    assert!(totals.iter().all(|total| *total == (1, 4)));
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/good.txt")).unwrap(),
        b"good"
    );
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/a")).unwrap(),
        b"existing"
    );
    if distinct {
        assert!(!case.server.root.path().join("pane/A").exists());
    }
    assert_eq!((case.count("PUT"), case.count("MOVE")), (1, 1));
    case.intact();
}

#[test]
fn direkte_unicode_gruppe_bleibt_unveraendert_und_good_laesst_sich_hochladen() {
    use std::os::unix::fs::MetadataExt;
    let case = Case::new();
    std::fs::write(case.source.join("é"), b"nfc").unwrap();
    std::fs::write(case.source.join("e\u{301}"), b"nfd").unwrap();
    std::fs::write(case.source.join("good.txt"), b"good").unwrap();
    #[cfg(target_os = "linux")]
    assert_ne!(
        std::fs::metadata(case.source.join("é")).unwrap().ino(),
        std::fs::metadata(case.source.join("e\u{301}"))
            .unwrap()
            .ino(),
        "Linux muss beide Unicode-Rohnamen getrennt halten"
    );
    #[cfg(target_os = "macos")]
    let _ = std::fs::metadata(case.source.join("é")).unwrap().ino();
    let op = case.prepared(&["é", "e\u{301}", "good.txt"]);
    let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
    let Ok(QueueExecutionResult::File(stats)) = result else {
        panic!("{result:?}")
    };
    assert_eq!((stats.files, stats.skipped), (1, 2));
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/good.txt")).unwrap(),
        b"good"
    );
    assert_eq!((case.count("PUT"), case.count("MOVE")), (1, 1));
    case.intact();
}

#[test]
fn direkte_ordnergruppe_wird_nicht_traversiert_unabhaengiger_ordner_bleibt_nutzbar() {
    use std::os::unix::fs::PermissionsExt;
    let case = Case::new();
    for name in ["a", "A"] {
        std::fs::create_dir_all(case.source.join(name)).unwrap();
        std::fs::write(case.source.join(name).join("kind"), b"nicht lesen").unwrap();
    }
    std::fs::create_dir(case.source.join("good")).unwrap();
    std::fs::write(case.source.join("good/a"), b"yes").unwrap();
    for name in ["a", "A"] {
        std::fs::set_permissions(
            case.source.join(name),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
    }
    let op = case.prepared(&["a", "A", "good"]);
    let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
    // Auch der Testbaum muss nachher ohne privilegierten rekursiven Fallback abbaubar sein.
    for name in ["a", "A"] {
        std::fs::set_permissions(
            case.source.join(name),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    let Ok(QueueExecutionResult::File(stats)) = result else {
        panic!("{result:?}")
    };
    let tree = stats.source_tree.as_ref().unwrap();
    assert_eq!(
        (
            stats.files,
            stats.skipped,
            tree.files,
            tree.directories,
            tree.entries
        ),
        (1, 2, 1, 1, 4)
    );
    assert_eq!(tree.omitted.len(), 2);
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/good/a")).unwrap(),
        b"yes"
    );
    assert!(!case.server.root.path().join("pane/a").exists());
    assert!(!case.server.root.path().join("pane/A").exists());
    assert_eq!(
        (case.count("MKCOL"), case.count("PUT"), case.count("MOVE")),
        (1, 1, 1)
    );
    case.intact();
}

#[test]
fn ausgelassenes_direktes_mitglied_bleibt_frisch_gebunden_vor_erster_wirkung() {
    let case = Case::new();
    for name in ["a", "A"] {
        std::fs::write(case.source.join(name), b"equal").unwrap();
    }
    std::fs::write(case.source.join("good.txt"), b"good").unwrap();
    let source = ProviderEndpoint::local(&case.source).unwrap();
    let target = case.server.endpoint();
    let items = ["a", "A", "good.txt"].map(|name| case.source.join(name));
    let contexts = source
        .interactive_file_contexts(&target, &items, false, false, &Cancel::new())
        .unwrap();
    let original = std::fs::metadata(case.source.join("a")).unwrap();
    std::fs::write(case.source.join("a"), b"drift").unwrap();
    filetime::set_file_mtime(
        case.source.join("a"),
        filetime::FileTime::from_last_modification_time(&original),
    )
    .unwrap();
    assert_eq!(
        error_code(&contexts.source_vfs.interactive_source_tree().unwrap_err()),
        ErrorCode::PlanDrift
    );
    assert!(fileops::find_recursive_conflicts(
        contexts.source_vfs.as_ref(),
        contexts.target_vfs.as_ref(),
        Path::new("/pane"),
        &items
    )
    .is_err());
    assert!(fileops::copy_items(
        fileops::FileOperationRequest {
            src: contexts.source_vfs.as_ref(),
            dst: contexts.target_vfs.as_ref(),
            items: &items,
            dst_dir: Path::new("/pane"),
            policy: OverwritePolicy::Skip,
            sandbox: &None
        },
        &Cancel::new(),
        &mut |_| {}
    )
    .is_err());
    assert_eq!(
        (case.count("PUT"), case.count("MOVE"), case.count("MKCOL")),
        (0, 0, 0)
    );
}

#[test]
fn exakt_doppelter_direkter_quellpfad_bleibt_ungueltig() {
    let case = Case::new();
    let source = ProviderEndpoint::local(&case.source).unwrap();
    let target = case.server.endpoint();
    let file = case.source.join("Direkt.txt");
    let error = source
        .interactive_file_contexts(&target, &[file.clone(), file], false, false, &Cancel::new())
        .err()
        .unwrap();
    assert_eq!(error_code(&error), ErrorCode::InvalidInput);
    assert_eq!(
        (case.count("PUT"), case.count("MOVE"), case.count("MKCOL")),
        (0, 0, 0)
    );
}

#[test]
fn ausgelassene_gruppe_driftet_nach_erstem_commit_und_stoppt_naechste_datei() {
    let case = Case::new();
    for name in ["a", "A"] {
        std::fs::write(case.source.join(name), b"equal").unwrap();
    }
    std::fs::write(case.source.join("good1.txt"), b"first").unwrap();
    std::fs::write(case.source.join("good2.txt"), b"second").unwrap();
    let original = std::fs::metadata(case.source.join("a")).unwrap();
    let op = case.prepared(&["a", "A", "good1.txt", "good2.txt"]);
    let mut changed = false;
    let (_, result) = case.run(op, &Cancel::new(), &mut |progress| {
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
    assert!(result.is_err());
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/good1.txt")).unwrap(),
        b"first"
    );
    assert!(!case.server.root.path().join("pane/good2.txt").exists());
    assert_eq!((case.count("PUT"), case.count("MOVE")), (1, 1));
    case.intact();
}

#[test]
fn content_sync_webdav_baum_und_update_ohne_strikte_quarantaene() {
    let case = Case::new();
    case.server.state.lock().unwrap().allow_file_overwrite = true;
    let queue = OperationQueue::open(case.config.path().join("operations-queue.json")).unwrap();
    let opts = SyncOptions {
        compare: Compare::ContentHash,
        ..SyncOptions::default()
    };
    for mode in [SyncMode::AddOnly, SyncMode::Update] {
        if mode == SyncMode::Update {
            std::fs::write(case.source.join("Direkt.txt"), b"Neuer Direktinhalt").unwrap();
        }
        let options = SyncOptions {
            mode,
            ..opts.clone()
        };
        let prepared = doppeldecker_core::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::local(&case.source).unwrap(),
            case.server.endpoint(),
            options.clone(),
            &Cancel::new(),
        )
        .unwrap();
        let operation = QueueOperation::ContentSync {
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: case.source.to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(case.server.config.clone()),
                root: "/pane".into(),
            }),
            options,
            expected_plan: Box::new(prepared.preview().plan.clone()),
            binding_digest: prepared.preview().digest,
        };
        queue.enqueue(operation).unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        let result = execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &case.config.path().join("audit.jsonl"),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        let QueueExecutionResult::Sync(stats) = result else {
            panic!("Syncbilanz fehlt")
        };
        assert!(!stats.dry_run);
    }
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/Direkt.txt")).unwrap(),
        b"Neuer Direktinhalt"
    );
    assert_eq!(
        std::fs::read(case.server.root.path().join("pane/Baum/A.txt")).unwrap(),
        b"Inhalt"
    );
    assert!(case
        .server
        .root
        .path()
        .join("pane/Baum/Unter/Leer")
        .is_dir());
    assert_eq!(
        std::fs::read(case.server.root.path().join("Sentinel")).unwrap(),
        b"Erhalten"
    );
}

#[test]
fn content_sync_webdav_mirror_uebertraegt_und_entfernt_dauerhaft_nur_geplantes() {
    let case = Case::new();
    case.server.state.lock().unwrap().deny_collection_probes = false;
    case.server.state.lock().unwrap().allow_file_overwrite = true;
    case.server.state.lock().unwrap().changing_collection_tokens = true;
    case.server
        .state
        .lock()
        .unwrap()
        .reject_collection_conditions = true;
    let target_root = case.server.root.path().join("pane");
    std::fs::create_dir_all(target_root.join("Alt/Unter")).unwrap();
    std::fs::write(target_root.join("Alt/Unter/Datei"), b"alt").unwrap();
    let queue = OperationQueue::open(case.config.path().join("operations-queue.json")).unwrap();
    let options = SyncOptions {
        mode: SyncMode::Mirror,
        compare: Compare::ContentHash,
        ..SyncOptions::default()
    };
    let prepared = doppeldecker_core::prepare_content_sync_for_queue(
        &queue,
        ProviderEndpoint::local(&case.source).unwrap(),
        case.server.endpoint(),
        options.clone(),
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(prepared.preview().irreversible_removals, 1);
    assert_eq!(prepared.preview().plan.skip_count, 2);
    queue
        .enqueue(QueueOperation::ContentSync {
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: case.source.to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(case.server.config.clone()),
                root: "/pane".into(),
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
        &case.config.path().join("audit.jsonl"),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    let QueueExecutionResult::Sync(stats) = result else {
        panic!("Syncbilanz fehlt")
    };
    assert_eq!(stats.files_deleted, 1);
    assert!(target_root.join("Alt/Unter").is_dir());
    assert!(!target_root.join("Alt/Unter/Datei").exists());
    assert_eq!(case.count("DELETE"), 1);
    assert_eq!(
        std::fs::read(target_root.join("Baum/A.txt")).unwrap(),
        b"Inhalt"
    );
}

#[path = "support/content_sync_queue.rs"]
mod guarded_content_queue;
use guarded_content_queue::prepare as prepare_content_sync;
#[test]
fn content_sync_webdav_zeitproperty_exakt_geschuetzt_und_ignoriert() {
    use std::time::{Duration, UNIX_EPOCH};
    for (behavior, profile) in [1, 0, 2].into_iter().flat_map(|behavior| {
        [MetadataProfile::Portable, MetadataProfile::NativeBestEffort]
            .map(|profile| (behavior, profile))
    }) {
        let case = Case::new();
        case.server.state.lock().unwrap().mtime_behavior = behavior;
        let timestamp = 1_700_000_000;
        for relative in [
            "Baum/A.txt",
            "Baum/Unter/literal%2F.txt",
            "Direkt.txt",
            "Baum/Unter/Leer",
            "Baum/Unter",
            "Baum",
        ] {
            std::fs::File::open(case.source.join(relative))
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new()
                        .set_modified(UNIX_EPOCH + Duration::from_secs(timestamp)),
                )
                .unwrap();
        }
        let queue = OperationQueue::open(case.config.path().join("operations-queue.json")).unwrap();
        let options = SyncOptions {
            compare: Compare::ContentHash,
            metadata: profile,
            ..SyncOptions::default()
        };
        let prepared = doppeldecker_core::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::local(&case.source).unwrap(),
            case.server.endpoint(),
            options.clone(),
            &Cancel::new(),
        )
        .unwrap();
        assert!(prepared
            .preview()
            .limitations
            .iter()
            .any(|s| s.contains("PROPPATCH")));
        queue
            .enqueue(QueueOperation::ContentSync {
                source: Box::new(BidirectionalQueueEndpoint::Local {
                    root: case.source.to_string_lossy().into_owned(),
                }),
                target: Box::new(BidirectionalQueueEndpoint::WebDav {
                    connection: Box::new(case.server.config.clone()),
                    root: "/pane".into(),
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
            &case.config.path().join("audit.jsonl"),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        let QueueExecutionResult::Sync(stats) = result else {
            panic!("Syncbilanz fehlt")
        };
        assert_eq!(stats.files_copied, 3);
        assert_eq!(
            std::fs::read(case.server.root.path().join("pane/Baum/A.txt")).unwrap(),
            b"Inhalt"
        );
        let mtime = stats
            .metadata_report
            .classes
            .iter()
            .find(|c| c.class == sync::MetadataClass::Mtime)
            .unwrap();
        assert_eq!(
            mtime.transferred,
            if behavior == 1 { 6 } else { 0 },
            "behavior={behavior}: {:?}",
            stats.metadata_report
        );
        assert_eq!(
            mtime.omitted,
            if behavior == 1 { 0 } else { 6 },
            "behavior={behavior}: {:?}",
            stats.metadata_report
        );
        let requests = case.server.state.lock().unwrap().requests.clone();
        assert_eq!(requests.iter().filter(|(m, _)| m == "PROPPATCH").count(), 6);
        assert!(requests
            .iter()
            .filter(|(m, _)| m == "PROPPATCH")
            .all(|(_, p)| p.starts_with("/pane/")));
    }
}

#[test]
fn content_sync_webdav_zeitantwortverlust_und_movezeitdrift_werden_nicht_wiederholt() {
    for behavior in [3, 4] {
        let case = Case::new();
        // Eine Datei genügt, um die erste Zeitmutation eindeutig zu zählen.
        let source = tempfile::tempdir().unwrap();
        let source_root = std::fs::canonicalize(source.path()).unwrap();
        std::fs::write(source_root.join("Datei"), b"Inhalt").unwrap();
        std::fs::File::open(source_root.join("Datei"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            ))
            .unwrap();
        case.server.state.lock().unwrap().mtime_behavior = behavior;
        let queue = OperationQueue::open(case.config.path().join("operations-queue.json")).unwrap();
        let options = SyncOptions {
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        };
        let prepared = doppeldecker_core::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::local(&source_root).unwrap(),
            case.server.endpoint(),
            options.clone(),
            &Cancel::new(),
        )
        .unwrap();
        queue
            .enqueue(QueueOperation::ContentSync {
                source: Box::new(BidirectionalQueueEndpoint::Local {
                    root: source_root.to_string_lossy().into_owned(),
                }),
                target: Box::new(BidirectionalQueueEndpoint::WebDav {
                    connection: Box::new(case.server.config.clone()),
                    root: "/pane".into(),
                }),
                options,
                expected_plan: Box::new(prepared.preview().plan.clone()),
                binding_digest: prepared.preview().digest,
            })
            .unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        let error = execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &case.config.path().join("audit.jsonl"),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear, "{error}");
        let state = case.server.state.lock().unwrap();
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|(m, _)| m == "PROPPATCH")
                .count(),
            1
        );
        assert_eq!(
            state.requests.iter().filter(|(m, _)| m == "MOVE").count(),
            if behavior == 3 { 0 } else { 1 }
        );
        assert_eq!(
            state.requests.iter().filter(|(m, _)| m == "DELETE").count(),
            0
        );
        drop(state);
        let remaining = std::fs::read_dir(case.server.root.path().join("pane"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(remaining.len(), 1);
        assert_eq!(std::fs::read(&remaining[0]).unwrap(), b"Inhalt");
        assert!(queue.claim_next().unwrap().is_none());
    }
}

#[test]
fn gewoehnlicher_webdav_upload_erteilt_keinen_content_sync_zeitvertrag() {
    let case = Case::new();
    case.server.state.lock().unwrap().mtime_behavior = 1;
    let (_, result) = case.run(case.prepared(&["Direkt.txt"]), &Cancel::new(), &mut |_| {});
    result.unwrap();
    assert_eq!(case.count("PROPPATCH"), 0);
    assert!(case
        .server
        .state
        .lock()
        .unwrap()
        .put_mtime_headers
        .iter()
        .all(|(_, time)| time.is_none()));
}

#[test]
fn content_sync_webdav_strict_ist_keine_laufzeit_metadatenausnahme() {
    let case = Case::new();
    case.server.state.lock().unwrap().mtime_behavior = 1;
    let error = prepare_content_sync(
        ProviderEndpoint::local(&case.source).unwrap(),
        case.server.endpoint(),
        SyncOptions {
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeStrict,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    )
    .err()
    .expect("Strict darf fehlende native WebDAV-Klassen nicht auslassen");
    assert_eq!(error_code(&error), ErrorCode::Unsupported);
    assert!(case
        .server
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .all(|(method, _)| !matches!(
            method.as_str(),
            "PUT" | "PROPPATCH" | "MKCOL" | "MOVE" | "DELETE"
        )));
}

fn xoc_sync_job(
    server: &server::Server,
    endpoint: ProviderEndpoint,
    target_root: &str,
    source: &Path,
    config: &Path,
) -> (OperationQueue, QueueEntry, PreparedContentSync, PathBuf) {
    let queue = OperationQueue::open(config.join("operations-queue.json")).unwrap();
    let options = SyncOptions {
        compare: Compare::ContentHash,
        ..SyncOptions::default()
    };
    let prepared = doppeldecker_core::prepare_content_sync_for_queue(
        &queue,
        ProviderEndpoint::local(source).unwrap(),
        endpoint,
        options.clone(),
        &Cancel::new(),
    )
    .unwrap();
    queue
        .enqueue(QueueOperation::ContentSync {
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: source.to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(server.config.clone()),
                root: target_root.into(),
            }),
            options,
            expected_plan: Box::new(prepared.preview().plan.clone()),
            binding_digest: prepared.preview().digest,
        })
        .unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    (queue, entry, prepared, config.join("audit.jsonl"))
}

#[test]
fn content_sync_webdav_xoc_mtime_verlangt_exakten_readback_und_nur_einen_put() {
    use std::os::unix::fs::MetadataExt;
    for (header, property, preserved, patches) in [
        (1, 0, true, 0),
        (2, 1, true, 1),
        (2, 0, false, 1),
        (3, 0, false, 1),
        (3, 1, true, 1),
    ] {
        let case = Case::new();
        {
            let mut state = case.server.state.lock().unwrap();
            state.xoc_mtime_behavior = header;
            state.mtime_behavior = property;
        }
        let source = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(source.path()).unwrap();
        let expected = 1_700_000_001;
        std::fs::write(root.join("Datei"), b"Inhalt").unwrap();
        filetime::set_file_mtime(
            root.join("Datei"),
            filetime::FileTime::from_unix_time(expected, 0),
        )
        .unwrap();
        let (queue, entry, prepared, audit) = xoc_sync_job(
            &case.server,
            case.server.endpoint(),
            "/pane",
            &root,
            case.config.path(),
        );
        let QueueExecutionResult::Sync(stats) = execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &audit,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap() else {
            panic!("Syncbilanz fehlt")
        };
        assert_eq!(stats.files_copied, 1);
        let report = stats
            .metadata_report
            .classes
            .iter()
            .find(|row| row.class == sync::MetadataClass::Mtime)
            .unwrap();
        assert_eq!(
            (report.transferred, report.omitted),
            if preserved { (1, 0) } else { (0, 1) },
            "header={header},property={property}"
        );
        let target = case.server.root.path().join("pane/Datei");
        assert_eq!(std::fs::read(&target).unwrap(), b"Inhalt");
        if preserved {
            assert_eq!(std::fs::metadata(target).unwrap().mtime(), expected);
        } else {
            assert!(stats
                .metadata_report
                .issues
                .iter()
                .any(|issue| issue.class == sync::MetadataClass::Mtime
                    && issue.path == "Datei"
                    && issue.reason.contains("X-OC-MTime")));
        }
        let state = case.server.state.lock().unwrap();
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|(method, _)| method == "PUT")
                .count(),
            1
        );
        assert_eq!(state.put_mtime_headers.len(), 1);
        assert_eq!(state.put_mtime_headers[0].1, Some(expected));
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|(m, _)| m == "PROPPATCH")
                .count(),
            patches
        );
        assert_eq!(
            state.requests.iter().filter(|(m, _)| m == "MOVE").count(),
            1
        );
    }
}

#[test]
fn content_sync_webdav_xoc_mtime_ist_bei_parallelen_dateien_nicht_geteilt() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let server = server::Server::new();
    {
        let mut state = server.state.lock().unwrap();
        state.xoc_mtime_behavior = 1;
        state.parallel_puts = 2;
    }
    let provider =
        Arc::new(WebDavVfs::connect(server.config.clone(), Arc::new(server::TestSecrets)).unwrap());
    let mut guards = Vec::new();
    let mut jobs = Vec::new();
    for (name, expected) in [("A", 1_700_000_001), ("B", 1_700_000_123)] {
        std::fs::create_dir(server.root.path().join("pane").join(name)).unwrap();
        let source = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(source.path()).unwrap();
        std::fs::write(root.join("Datei"), name.as_bytes()).unwrap();
        filetime::set_file_mtime(
            root.join("Datei"),
            filetime::FileTime::from_unix_time(expected, 0),
        )
        .unwrap();
        let config = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let remote = format!("/pane/{name}");
        jobs.push(xoc_sync_job(
            &server,
            ProviderEndpoint::webdav(provider.clone(), Path::new(&remote)).unwrap(),
            &remote,
            &root,
            config.path(),
        ));
        guards.push((source, config));
    }
    std::thread::scope(|scope| {
        let handles = jobs
            .into_iter()
            .map(|(queue, entry, prepared, audit)| {
                scope.spawn(move || {
                    execute_prepared_content_sync_queue_entry(
                        &queue,
                        &entry,
                        prepared,
                        true,
                        &audit,
                        &Cancel::new(),
                        &mut |_| {},
                    )
                    .unwrap()
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            let QueueExecutionResult::Sync(stats) = handle.join().unwrap() else {
                panic!("Syncbilanz fehlt")
            };
            assert_eq!(stats.files_copied, 1);
        }
    });
    for (name, expected) in [("A", 1_700_000_001), ("B", 1_700_000_123)] {
        let target = server.root.path().join("pane").join(name).join("Datei");
        assert_eq!(std::fs::read(&target).unwrap(), name.as_bytes());
        assert_eq!(std::fs::metadata(target).unwrap().mtime(), expected);
    }
    let state = server.state.lock().unwrap();
    assert_eq!(state.arrived_puts, 2);
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(method, _)| method == "PUT")
            .count(),
        2
    );
    assert_eq!(state.put_mtime_headers.len(), 2);
    let mut values = state
        .put_mtime_headers
        .iter()
        .map(|(_, time)| time.unwrap())
        .collect::<Vec<_>>();
    values.sort_unstable();
    assert_eq!(values, [1_700_000_001, 1_700_000_123]);
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(m, _)| m == "PROPPATCH")
            .count(),
        0
    );
    drop(guards);
}

#[test]
fn content_sync_webdav_xoc_mtime_antwortverlust_und_commitdrift_bleiben_fatal() {
    for lost_put in [true, false] {
        let case = Case::new();
        {
            let mut state = case.server.state.lock().unwrap();
            state.xoc_mtime_behavior = 1;
            if lost_put {
                state.fault = server::Fault::LostAfterPut;
            } else {
                state.mtime_behavior = 4;
            }
        }
        let source = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(source.path()).unwrap();
        std::fs::write(root.join("Datei"), b"Inhalt").unwrap();
        filetime::set_file_mtime(
            root.join("Datei"),
            filetime::FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();
        let (queue, entry, prepared, audit) = xoc_sync_job(
            &case.server,
            case.server.endpoint(),
            "/pane",
            &root,
            case.config.path(),
        );
        let error = execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &audit,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear, "{error}");
        assert!(!error::recovery_paths(&error).is_empty());
        let state = case.server.state.lock().unwrap();
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|(method, _)| method == "PUT")
                .count(),
            1
        );
        assert_eq!(state.put_mtime_headers.len(), 1);
        assert_eq!(state.put_mtime_headers[0].1, Some(1_700_000_000));
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|(m, _)| m == "PROPPATCH")
                .count(),
            0
        );
        assert_eq!(
            state.requests.iter().filter(|(m, _)| m == "MOVE").count(),
            usize::from(!lost_put)
        );
        assert_eq!(
            state.requests.iter().filter(|(m, _)| m == "DELETE").count(),
            0
        );
        drop(state);
        let survivors = std::fs::read_dir(case.server.root.path().join("pane"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(survivors.len(), 1);
        assert_eq!(std::fs::read(&survivors[0]).unwrap(), b"Inhalt");
        assert!(queue.claim_next().unwrap().is_none());
    }
}

#[test]
fn content_sync_webdav_technische_ordnerskips_mit_symlink_sperren_alle_entfernungen() {
    let case = Case::new();
    std::os::unix::fs::symlink("Direkt.txt", case.source.join("Link")).unwrap();
    let target = case.server.root.path().join("pane");
    std::fs::create_dir_all(target.join("Extra/Unter")).unwrap();
    std::fs::write(target.join("Extra/Unter/Datei"), b"erhalten").unwrap();
    let prepared = prepare_content_sync(
        ProviderEndpoint::local(&case.source).unwrap(),
        case.server.endpoint(),
        SyncOptions {
            mode: SyncMode::Mirror,
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(prepared.preview().plan.delete_count, 0);
    assert_eq!(prepared.preview().irreversible_removals, 0);
    assert!(prepared.preview().plan.skip_count >= 3);
    let digest = prepared.preview().digest;
    let stats = prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(stats.files_deleted, 0);
    assert!(stats.files_copied > 0);
    assert_eq!(case.count("DELETE"), 0);
    assert_eq!(
        std::fs::read(target.join("Extra/Unter/Datei")).unwrap(),
        b"erhalten"
    );
}
