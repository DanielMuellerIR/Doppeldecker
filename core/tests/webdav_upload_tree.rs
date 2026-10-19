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
fn baum_schwache_listings_und_geaenderte_collectiontokens_stoppen_kinderwirkungen() {
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
    let case = Case::new();
    case.server.state.lock().unwrap().changing_collection_tokens = true;
    let op = case.prepared(&["Baum"]);
    let (_, result) = case.run(op, &Cancel::new(), &mut |_| {});
    assert_eq!(error_code(&result.unwrap_err()), ErrorCode::OutcomeUnclear);
    assert_eq!(case.count("MKCOL"), 1);
    assert_eq!(case.count("PUT"), 0);
    assert_eq!(case.count("DELETE"), 0);
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
fn baum_case_unicode_typkonflikte_werden_erhalten_oder_vor_wirkung_abgelehnt() {
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
    let case = Case::new();
    std::fs::write(case.source.join("Baum/é"), b"A").unwrap();
    std::fs::write(case.source.join("Baum/e\u{301}"), b"B").unwrap();
    let endpoint = ProviderEndpoint::local(&case.source).unwrap();
    let result = endpoint.interactive_file_contexts(
        &case.server.endpoint(),
        &[case.source.join("Baum")],
        false,
        false,
        &Cancel::new(),
    );
    if std::fs::read_dir(case.source.join("Baum")).unwrap().count() == 4 {
        assert!(result.is_err());
    }
    assert_eq!(case.count("MKCOL"), 0);
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
