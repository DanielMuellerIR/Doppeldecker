//! Additive WebDAV-Downloads gegen einen künstlichen, getrennten HTTPS-Root.
#[path = "support/webdav_namespace.rs"]
mod server;
use doppeldecker_core::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Case {
    server: server::Server,
    target: tempfile::TempDir,
    vfs: Arc<WebDavVfs>,
}
impl Case {
    fn new() -> Self {
        let server = server::Server::new();
        let vfs = Arc::new(
            WebDavVfs::connect(server.config.clone(), Arc::new(server::TestSecrets)).unwrap(),
        );
        Self {
            server,
            target: tempfile::tempdir().unwrap(),
            vfs,
        }
    }
    fn target(&self) -> PathBuf {
        self.target.path().canonicalize().unwrap()
    }
    fn write(&self, name: &str) {
        std::fs::write(
            self.server.root.path().join("pane").join(name),
            name.as_bytes(),
        )
        .unwrap();
    }
    fn aliases(&self, parent: &str, names: &[&str]) {
        self.server
            .state
            .lock()
            .unwrap()
            .download_aliases
            .extend(names.iter().map(|name| {
                (
                    self.server.root.path().join(parent),
                    (*name).into(),
                    format!("\"{}\"", blake3::hash(name.as_bytes()).to_hex()),
                )
            }));
    }
    fn contexts(&self, names: &[&str], cancel: &Cancel) -> (InteractiveFileContexts, Vec<PathBuf>) {
        let source = ProviderEndpoint::webdav(self.vfs.clone(), Path::new("/pane")).unwrap();
        let target = ProviderEndpoint::local(&self.target()).unwrap();
        let items = names
            .iter()
            .map(|name| Path::new("/pane").join(name))
            .collect::<Vec<_>>();
        (
            source
                .interactive_file_contexts(&target, &items, false, false, cancel)
                .unwrap(),
            items,
        )
    }
    fn copy(
        &self,
        contexts: &InteractiveFileContexts,
        items: &[PathBuf],
        cancel: &Cancel,
        progress: &mut dyn FnMut(&Progress),
    ) -> std::io::Result<fileops::OpStats> {
        fileops::copy_items(
            fileops::FileOperationRequest {
                src: contexts.source_vfs.as_ref(),
                dst: contexts.target_vfs.as_ref(),
                items,
                dst_dir: &self.target(),
                policy: fileops::OverwritePolicy::Skip,
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
            source: Box::new(BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(self.server.config.clone()),
                root: "/pane".into(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::Local {
                root: self.target().to_string_lossy().into(),
            }),
            items: names.iter().map(|name| format!("/pane/{name}")).collect(),
            destination: self.target().to_string_lossy().into(),
            overwrite: false,
            source_tree_expected: contexts
                .source_vfs
                .interactive_source_tree()
                .unwrap()
                .map(Box::new),
            expected_conflicts: vec![],
            expected_conflict_proofs: vec![],
            keep_both_targets: vec![],
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: None,
            replace_best_effort_expected: None,
            remove_source_best_effort_expected: None,
        }
    }
    fn assert_read_only(&self) {
        assert!(!self
            .server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|(method, _)| matches!(method.as_str(), "PUT" | "MOVE" | "DELETE" | "MKCOL")));
    }
}

#[test]
fn ausgelassener_etagwechsel_nach_erstem_commit_stoppt_zweite_datei() {
    let case = Case::new();
    case.aliases("pane", &["a", "A"]);
    case.write("good1");
    case.write("good2");
    let (contexts, items) = case.contexts(&["a", "A", "good1", "good2"], &Cancel::new());
    let mut changed = false;
    let error = case
        .copy(&contexts, &items, &Cancel::new(), &mut |progress| {
            if progress.files_done == 1 && !changed {
                assert!(case.target().join("good1").exists());
                case.server.state.lock().unwrap().download_aliases[0].2 = "\"changed\"".into();
                changed = true;
            }
        })
        .unwrap_err();
    assert!(changed);
    assert_eq!(error_code(&error), ErrorCode::PlanDrift);
    assert!(case.target().join("good1").exists());
    assert!(!case.target().join("good2").exists());
    assert!(!case
        .server
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .any(|(method, path)| method == "GET"
            && (path == "/pane/a" || path == "/pane/A" || path == "/pane/good2")));
    case.assert_read_only();
}

#[test]
fn direkte_und_rekursive_case_nfc_gruppen_lassen_unabhaengige_inhalte_nutzbar() {
    let case = Case::new();
    case.aliases("pane", &["a", "A", "é", "e\u{301}"]);
    std::fs::create_dir(case.server.root.path().join("pane/tree")).unwrap();
    case.aliases("pane/tree", &["b", "B", "ö", "o\u{308}"]);
    case.write("tree/good");
    case.write("literal%2F.txt");
    let (contexts, items) = case.contexts(
        &["a", "A", "é", "e\u{301}", "tree", "literal%2F.txt"],
        &Cancel::new(),
    );
    let tree = contexts
        .source_vfs
        .interactive_source_tree()
        .unwrap()
        .unwrap();
    assert_eq!(
        (tree.files, tree.directories, tree.omitted.len()),
        (2, 1, 8)
    );
    let stats = case
        .copy(&contexts, &items, &Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!((stats.files, stats.skipped), (2, 8));
    assert_eq!(
        std::fs::read(case.target().join("tree/good")).unwrap(),
        b"tree/good"
    );
    assert_eq!(
        std::fs::read(case.target().join("literal%2F.txt")).unwrap(),
        b"literal%2F.txt"
    );
    assert_eq!(
        case.server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _)| method == "GET")
            .count(),
        2
    );
    case.assert_read_only();
}

#[test]
fn ausgewaehlter_baum_bindet_alle_namen_aber_unselektierte_geschwister_nicht() {
    let case = Case::new();
    case.write("good");
    case.write("neighbor");
    let (contexts, items) = case.contexts(&["good"], &Cancel::new());
    std::fs::write(
        case.server.root.path().join("pane/neighbor"),
        b"new neighbor",
    )
    .unwrap();
    std::fs::write(case.server.root.path().join("outside"), b"new outside").unwrap();
    case.copy(&contexts, &items, &Cancel::new(), &mut |_| {})
        .unwrap();
    std::fs::create_dir(case.server.root.path().join("pane/tree")).unwrap();
    case.write("tree/file");
    let (contexts, _) = case.contexts(&["tree"], &Cancel::new());
    case.write("tree/new");
    assert_eq!(
        error_code(&contexts.source_vfs.interactive_source_tree().unwrap_err()),
        ErrorCode::PlanDrift
    );
}

#[test]
fn get_verwendet_vorbereiteten_etag_und_412_stoppt_vor_commit() {
    let case = Case::new();
    case.write("good");
    let (contexts, items) = case.contexts(&["good"], &Cancel::new());
    case.server.state.lock().unwrap().change_before_get =
        Some(case.server.root.path().join("pane/good"));
    let error = case
        .copy(&contexts, &items, &Cancel::new(), &mut |_| {})
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::PlanDrift);
    assert!(!case.target().join("good").exists());
    let state = case.server.state.lock().unwrap();
    assert_eq!(state.get_if_match_headers.len(), 1);
    assert_eq!(
        state.get_if_match_headers[0],
        format!("\"file-{}\"", blake3::hash(b"good").to_hex())
    );
}

#[test]
fn kurze_und_lange_streams_committen_nicht() {
    for bytes in [vec![b'x'], vec![b'x'; 20]] {
        let case = Case::new();
        case.write("good");
        let (contexts, items) = case.contexts(&["good"], &Cancel::new());
        case.server.state.lock().unwrap().download_body = Some(bytes);
        let error = case
            .copy(&contexts, &items, &Cancel::new(), &mut |_| {})
            .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::PlanDrift);
        assert!(!case.target().join("good").exists());
        case.assert_read_only();
    }
}

#[test]
fn abbruch_und_generationwechsel_stoppen_vor_commit() {
    let case = Case::new();
    case.write("good");
    let cancel = Cancel::new();
    let (contexts, items) = case.contexts(&["good"], &cancel);
    case.server.state.lock().unwrap().cancel_on_get = Some(cancel.clone());
    let result = case.copy(&contexts, &items, &cancel, &mut |_| {});
    assert!(result.is_err() || result.unwrap().cancelled);
    assert!(!case.target().join("good").exists());
    let (contexts, _) = case.contexts(&["good"], &Cancel::new());
    case.vfs.reconnect().unwrap();
    assert_eq!(
        error_code(&contexts.source_vfs.interactive_source_tree().unwrap_err()),
        ErrorCode::PlanDrift
    );
}

#[test]
fn queue_reopen_erhaelt_auslassungsbeleg_und_verlangt_bestaetigung_bei_drift() {
    let case = Case::new();
    case.aliases("pane", &["a", "A"]);
    case.write("good");
    let area = tempfile::tempdir().unwrap();
    let path = area.path().join("queue.json");
    let operation = case.operation(&["a", "A", "good"]);
    {
        let queue = OperationQueue::open(&path).unwrap();
        queue.enqueue(operation).unwrap();
    }
    case.server.state.lock().unwrap().download_aliases[0].2 = "\"new\"".into();
    let queue = OperationQueue::open(&path).unwrap();
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
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(matches!(
        result,
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    assert!(matches!(
        queue.snapshot().entries[0].attention,
        Some(QueueAttention::SourceTreeChanged { .. })
    ));
    assert!(!case.target().join("good").exists());
    case.assert_read_only();
}

#[test]
fn baumbeleg_erteilt_keine_overwrite_move_remoteziel_oder_fremde_auslassungsfreigabe() {
    let case = Case::new();
    case.aliases("pane", &["a", "A"]);
    case.write("good");
    for variant in 0..4 {
        let mut operation = case.operation(&["a", "A", "good"]);
        if let QueueOperation::ProviderFile {
            kind,
            overwrite,
            target,
            source_tree_expected,
            ..
        } = &mut operation
        {
            match variant {
                0 => *kind = FileQueueKind::Move,
                1 => *overwrite = true,
                2 => {
                    **target = BidirectionalQueueEndpoint::WebDav {
                        connection: Box::new(case.server.config.clone()),
                        root: "/pane".into(),
                    }
                }
                _ => source_tree_expected.as_mut().unwrap().omitted[0].path = "/outside".into(),
            }
        }
        let area = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(area.path().join("queue.json")).unwrap();
        assert!(queue.enqueue(operation).is_err());
    }
    let source = ProviderEndpoint::webdav(case.vfs.clone(), Path::new("/pane")).unwrap();
    let target = ProviderEndpoint::local(&case.target()).unwrap();
    for (moving, overwrite) in [(true, false), (false, true)] {
        let context = source
            .interactive_file_contexts(
                &target,
                &[PathBuf::from("/pane/good")],
                moving,
                overwrite,
                &Cancel::new(),
            )
            .unwrap();
        assert!(context
            .source_vfs
            .interactive_source_tree()
            .unwrap()
            .is_none());
    }
}

#[test]
fn queue_reopen_mit_unveraendertem_beleg_kopiert_ohne_neue_baumfrage() {
    let case = Case::new();
    case.aliases("pane", &["a", "A"]);
    case.write("good");
    let area = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(area.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = area.path().join("queue.json");
    let operation = case.operation(&["a", "A", "good"]);
    {
        let queue = OperationQueue::open(&path).unwrap();
        queue.enqueue(operation).unwrap();
    }
    case.vfs.reconnect().unwrap();
    let queue = OperationQueue::open(&path).unwrap();
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
        &Cancel::new(),
        &mut |_| {},
    );
    let QueueExecutionResult::File(stats) = result.unwrap() else {
        panic!("kein Dateiergebnis");
    };
    assert!(!stats
        .target_write_limitations
        .iter()
        .any(|note| note.contains("Temp-Recovery")));
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    assert_eq!(std::fs::read(case.target().join("good")).unwrap(), b"good");
    case.assert_read_only();
}

#[test]
fn leere_datei_und_ausgetauschter_vorfahr_werden_geprueft() {
    let case = Case::new();
    std::fs::write(case.server.root.path().join("pane/empty"), b"").unwrap();
    let (contexts, items) = case.contexts(&["empty"], &Cancel::new());
    case.copy(&contexts, &items, &Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(case.target().join("empty").metadata().unwrap().len(), 0);
    case.write("good");
    let (contexts, items) = case.contexts(&["good"], &Cancel::new());
    std::fs::rename(
        case.server.root.path().join("pane"),
        case.server.root.path().join("old-pane"),
    )
    .unwrap();
    std::fs::create_dir(case.server.root.path().join("pane")).unwrap();
    case.write("good");
    let error = case
        .copy(&contexts, &items, &Cancel::new(), &mut |_| {})
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::PlanDrift);
    assert!(!case.target().join("good").exists());
}

#[test]
fn tiefer_baum_navigation_bleibt_linear_und_ohne_neue_tiefengrenze() {
    const DEPTH: usize = 72;
    let case = Case::new();
    let mut relative = PathBuf::from("tree");
    for _ in 0..DEPTH {
        relative.push("d");
    }
    std::fs::create_dir_all(case.server.root.path().join("pane").join(&relative)).unwrap();
    std::fs::write(
        case.server
            .root
            .path()
            .join("pane")
            .join(&relative)
            .join("file"),
        b"deep",
    )
    .unwrap();
    let (contexts, items) = case.contexts(&["tree"], &Cancel::new());
    let before = case.server.state.lock().unwrap().requests.len();
    let stats = case
        .copy(&contexts, &items, &Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(
        std::fs::read(case.target().join(&relative).join("file")).unwrap(),
        b"deep"
    );
    let state = case.server.state.lock().unwrap();
    let listings = state.requests[before..]
        .iter()
        .filter(|(method, _)| method == "PROPFIND")
        .count();
    // Eigene Ordnergrenzen und vollständige Inhaltsgrenzen dürfen mehrfach
    // prüfen; eine Vorfahrkette je reinem Navigationsschritt wäre quadratisch.
    assert!(
        listings <= 10 * DEPTH + 50,
        "{listings} Listings für {DEPTH} Ebenen"
    );
}

#[test]
fn webdav_download_crash_reopen_erhaelt_privaten_tempintent_ohne_neustart() {
    use std::os::unix::fs::PermissionsExt;
    let case = Case::new();
    std::fs::write(
        case.server.root.path().join("pane/good"),
        vec![0x63; 4 * 1024 * 1024],
    )
    .unwrap();
    let target = case.target();
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
            .filter(|line| *line == "webdav_download_temp_crash_child: test")
            .count(),
        1
    );
    let status = std::process::Command::new(binary)
        .args(["--exact", "webdav_download_temp_crash_child", "--ignored"])
        .env("DOPPELDECKER_WEBDAV_DOWNLOAD_CRASH_QUEUE", &journal)
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
    case.assert_read_only();
}

#[test]
#[ignore = "wird ausschließlich vom begrenzten Prozessabbruch-Gegenlauf gestartet"]
fn webdav_download_temp_crash_child() {
    let journal = PathBuf::from(
        std::env::var_os("DOPPELDECKER_WEBDAV_DOWNLOAD_CRASH_QUEUE")
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
        Arc::new(server::TestSecrets),
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
fn webdav_download_ohne_privaten_queueordner_bilanziert_fehlende_temp_recovery() {
    use std::os::unix::fs::PermissionsExt;
    let case = Case::new();
    case.write("good");
    let area = tempfile::tempdir().unwrap();
    let queue = OperationQueue::open(area.path().join("queue.json")).unwrap();
    queue.enqueue(case.operation(&["good"])).unwrap();
    std::fs::set_permissions(area.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
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
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    assert_eq!(std::fs::read(case.target().join("good")).unwrap(), b"good");
    case.assert_read_only();
}
