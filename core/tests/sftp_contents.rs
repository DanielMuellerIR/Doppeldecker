//! Reale SSH-/SFTP-Pakete gegen einen ausschließlich künstlichen Loopback-Server.
//! Der Server kann Antworten verlieren und Inhalte zwischen Reads austauschen;
//! keine Nutzerdatei, kein Schlüsselbund und kein externer sshd wird verwendet.

use doppeldecker_core::*;
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
};
use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    Symlink,
    Special,
    MissingSize,
    TooLarge,
    RootSwap,
    ParentSwap,
    ContentDrift,
    Truncate,
    Grow,
    EmptyData,
    OversizedData,
    FstatDrift,
    ReadbackDenied,
    LostResponse,
    CloseLost,
    ListingDenied,
    MetadataLost,
    TreeDuplicate,
    TreeInvalidName,
    TreeEmptyPage,
    TreeListingLost,
    TreePartialAttributes,
    SlowDotPages,
}
struct State {
    data: Vec<u8>,
    fault: Fault,
    read_started: bool,
    opens: usize,
    mutations: usize,
    closes: usize,
    lose_readback_close: bool,
    metadata_requests: usize,
    cancel_on_metadata: Option<Cancel>,
    cancel_on_readdir: Option<Cancel>,
    tree: Option<HashMap<String, (FileAttributes, Vec<u8>)>>,
    page_size: usize,
    listing_fail_after: Option<usize>,
    mtime: u32,
}
struct FakeSecrets;
impl SecretResolver for FakeSecrets {
    fn resolve(&self, _: &KeychainSecretReference) -> io::Result<SecretString> {
        SecretString::new("synthetic-ap04-only".into())
    }
}
struct Server {
    runtime: Option<tokio::runtime::Runtime>,
    state: Arc<Mutex<State>>,
    config: SftpConnectionConfig,
}
impl Server {
    fn tree() -> Self {
        let server = Self::new(b"Bauminhalt\n".to_vec(), Fault::None);
        {
            let mut state = server.state.lock().unwrap();
            let directory = attrs(&state, false);
            let file = attrs(&state, true);
            state.tree = Some(HashMap::from([
                ("/vault".into(), (directory.clone(), Vec::new())),
                ("/vault/pane".into(), (directory.clone(), Vec::new())),
                ("/vault/pane/Baum".into(), (directory.clone(), Vec::new())),
                ("/vault/pane/Baum/Leer".into(), (directory, Vec::new())),
                (
                    "/vault/pane/Baum/Kind.txt".into(),
                    (file, state.data.clone()),
                ),
            ]));
        }
        server
    }
    fn new(data: Vec<u8>, fault: Fault) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let key = russh::keys::PrivateKey::new(
            russh::keys::ssh_key::private::KeypairData::Ed25519(
                russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[0x41; 32]),
            ),
            "AP04 artificial server",
        )
        .unwrap();
        let fingerprint = key
            .public_key()
            .fingerprint(russh::keys::ssh_key::HashAlg::Sha256);
        let pin = SftpHostKeyPin {
            algorithm: key.public_key().algorithm().as_str().to_owned(),
            sha256: fingerprint.as_bytes().try_into().unwrap(),
        };
        let server_config = Arc::new(russh::server::Config {
            keys: vec![key],
            auth_rejection_time: std::time::Duration::ZERO,
            ..Default::default()
        });
        let state = Arc::new(Mutex::new(State {
            data,
            fault,
            read_started: false,
            opens: 0,
            mutations: 0,
            closes: 0,
            lose_readback_close: false,
            metadata_requests: 0,
            cancel_on_metadata: None,
            cancel_on_readdir: None,
            tree: None,
            page_size: usize::MAX,
            listing_fail_after: None,
            mtime: 123456,
        }));
        let shared = state.clone();
        runtime.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let config = server_config.clone();
                let handler = Ssh {
                    channels: HashMap::new(),
                    state: shared.clone(),
                };
                tokio::spawn(async move {
                    if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        Self {
            runtime: Some(runtime),
            state,
            config: SftpConnectionConfig {
                host: "127.0.0.1".into(),
                alternate_hosts: Vec::new(),
                port,
                username: "artificial".into(),
                requested_root: "/vault".into(),
                host_key: pin,
                authentication: SftpAuthenticationReference::Password {
                    secret: KeychainSecretReference::new("ap04-test", "fake").unwrap(),
                },
            },
        }
    }
    fn connect(&self) -> Arc<SftpVfs> {
        Arc::new(SftpVfs::connect(self.config.clone(), Arc::new(FakeSecrets)).unwrap())
    }
    fn context(&self, cancel: &Cancel) -> (Arc<SftpVfs>, Arc<dyn Vfs>) {
        let provider = self.connect();
        let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
        let (vfs, _) = endpoint
            .interactive_file_read_context(&[PathBuf::from("/pane/inhalt.txt")], cancel)
            .unwrap();
        (provider, vfs)
    }
}

#[test]
fn sftp_anzeige_laesst_unbrauchbare_eintraege_aus_und_erhaelt_unabhaengige_dateien() {
    let server = Server::tree();
    tree_file(&server, "/vault/pane/Bekannt.txt", b"nutzbar".to_vec());
    {
        let mut state = server.state.lock().unwrap();
        state.page_size = 1;
        let regular = attrs(&state, true);
        let mut missing = regular.clone();
        missing.permissions = None;
        let tree = state.tree.as_mut().unwrap();
        tree.insert("/vault/pane/Zeile\nneu.txt".into(), (regular, Vec::new()));
        tree.insert(
            "/vault/pane/.Unklar.txt".into(),
            (missing.clone(), Vec::new()),
        );
        tree.insert("/vault/pane/Unklar.txt".into(), (missing, Vec::new()));
    }
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let listing = endpoint
        .list_interactive()
        .expect("Ein unbrauchbarer Eintrag darf unabhängige Dateien nicht verstecken");
    assert!(listing
        .entries
        .iter()
        .any(|item| item.entry.name == "Bekannt.txt"));
    assert!(!listing.complete);
    assert_eq!(listing.omitted.len(), 3);
    assert_eq!(
        listing.omitted.iter().filter(|item| item.is_hidden).count(),
        1
    );
    assert!(listing
        .omitted
        .iter()
        .any(|item| item.display_name == "Zeile\\nneu.txt"));
    assert!(listing
        .omitted
        .iter()
        .all(|item| !item.display_name.contains('\n') && !item.reason.is_empty()));
    {
        let state = server.state.lock().unwrap();
        assert_eq!((state.opens, state.mutations), (0, 0));
    }
    let (source, _) = endpoint
        .interactive_file_read_context(&[PathBuf::from("/pane/Bekannt.txt")], &Cancel::new())
        .unwrap();
    let mut bytes = Vec::new();
    source
        .open_read(Path::new("/pane/Bekannt.txt"))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"nutzbar");
    assert_eq!(server.state.lock().unwrap().mutations, 0);
}

#[test]
fn sftp_anzeigelisting_liefert_bei_doppelten_namen_oder_antwortverlust_keine_teilfreigabe() {
    for fault in [
        Fault::TreeDuplicate,
        Fault::TreeListingLost,
        Fault::TreeEmptyPage,
        Fault::CloseLost,
    ] {
        let server = Server::tree();
        server.state.lock().unwrap().fault = fault;
        let provider = server.connect();
        let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
        assert!(endpoint.list_interactive().is_err());
        let state = server.state.lock().unwrap();
        assert_eq!((state.opens, state.mutations), (0, 0));
        assert_eq!(
            state.closes, 1,
            "Auch bei kaputtem READDIR muss der Verzeichnishandle schließen"
        );
        drop(state);
        if matches!(fault, Fault::TreeListingLost | Fault::CloseLost) {
            server.state.lock().unwrap().fault = Fault::None;
            assert!(endpoint.list_interactive().is_ok());
            assert_eq!(provider.session_generation(), 2);
        }
    }
}

#[test]
fn sftp_anzeigelisting_verwirft_antwortverlust_nach_einer_gueltigen_seite() {
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        state.page_size = 1;
        state.listing_fail_after = Some(1);
    }
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap();
    assert_eq!(
        error_code(&endpoint.list_interactive().unwrap_err()),
        ErrorCode::ConnectionLost
    );
    let state = server.state.lock().unwrap();
    assert_eq!((state.opens, state.mutations, state.closes), (0, 0, 1));
}

#[test]
fn sftp_anzeige_und_inhaltsleser_teilen_sitzung_ohne_handlevermischung() {
    let server = Server::tree();
    tree_file(&server, "/vault/pane/Parallel.txt", b"parallel\n".to_vec());
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let start = std::sync::Barrier::new(3);
    std::thread::scope(|scope| {
        let listing_reader = scope.spawn(|| {
            start.wait();
            for _ in 0..20 {
                let listing = endpoint.list_interactive().unwrap();
                assert!(listing
                    .entries
                    .iter()
                    .any(|item| item.entry.name == "Parallel.txt" && item.entry.size == 9));
                assert!(listing.omitted.is_empty());
            }
        });
        let content_reader = scope.spawn(|| {
            start.wait();
            for _ in 0..20 {
                let (source, _) = endpoint
                    .interactive_file_read_context(
                        &[PathBuf::from("/pane/Parallel.txt")],
                        &Cancel::new(),
                    )
                    .unwrap();
                let mut bytes = Vec::new();
                source
                    .open_read(Path::new("/pane/Parallel.txt"))
                    .unwrap()
                    .read_to_end(&mut bytes)
                    .unwrap();
                assert_eq!(bytes, b"parallel\n");
            }
        });
        start.wait();
        listing_reader.join().unwrap();
        content_reader.join().unwrap();
    });
    assert_eq!(provider.session_generation(), 1);
    assert_eq!(server.state.lock().unwrap().mutations, 0);
}

#[test]
fn sftp_anzeigelisting_begrenzt_endlose_punktseiten_und_schliesst_den_handle() {
    let server = Server::tree();
    server.state.lock().unwrap().fault = Fault::SlowDotPages;
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap();
    let start = std::time::Instant::now();
    assert_eq!(
        error_code(&endpoint.list_interactive().unwrap_err()),
        ErrorCode::Unsupported
    );
    assert!(start.elapsed() >= std::time::Duration::from_secs(60));
    assert!(start.elapsed() < std::time::Duration::from_secs(80));
    let state = server.state.lock().unwrap();
    assert_eq!((state.opens, state.mutations, state.closes), (0, 0, 1));
}

#[test]
fn sftp_anzeigelisting_erhaelt_fehlende_groesse_ueber_echte_protokollpakete() {
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        state.page_size = 1;
        let mut file = attrs(&state, true);
        file.size = None;
        let mut empty = file.clone();
        empty.size = Some(0);
        let tree = state.tree.as_mut().unwrap();
        tree.insert(
            "/vault/pane/Unbekannt.txt".into(),
            (file, b"nicht leer".to_vec()),
        );
        tree.insert("/vault/pane/Leer.txt".into(), (empty, Vec::new()));
    }
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let listing = endpoint.list_interactive().unwrap();
    assert!(!listing.complete);
    let unknown = listing
        .entries
        .iter()
        .find(|item| item.entry.name == "Unbekannt.txt")
        .unwrap();
    let empty = listing
        .entries
        .iter()
        .find(|item| item.entry.name == "Leer.txt")
        .unwrap();
    assert!(unknown.display_entry().unwrap()["size"].is_null());
    assert_eq!(empty.display_entry().unwrap()["size"], 0);
    assert!(!provider.capabilities().complete_listing);
    let state = server.state.lock().unwrap();
    assert_eq!((state.opens, state.mutations), (0, 0));
}

#[test]
fn begrenzter_sftp_baum_kopiert_datei_und_leeren_ordner_ohne_strikte_listingfreigabe() {
    let server = Server::tree();
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let selection = [PathBuf::from("/pane/Baum")];
    let cancel = Cancel::new();
    let (source, _) = endpoint
        .interactive_file_read_context(&selection, &cancel)
        .unwrap();
    assert!(provider.root_identity(Path::new("/pane")).is_err());
    assert!(source.list_complete(Path::new("/pane/Baum")).is_err());
    let target = tempfile::tempdir().unwrap();
    let target_root = target.path().canonicalize().unwrap();
    let destination = LocalVfs::rooted(&target_root).unwrap();
    let stats = copy_items(
        FileOperationRequest {
            src: source.as_ref(),
            dst: &destination,
            items: &selection,
            dst_dir: &target_root,
            policy: OverwritePolicy::Skip,
            sandbox: &None,
        },
        &cancel,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(stats.bytes, 11);
    assert_eq!(
        std::fs::read(target.path().join("Baum/Kind.txt")).unwrap(),
        b"Bauminhalt\n"
    );
    assert!(target.path().join("Baum/Leer").is_dir());
    assert_eq!(
        find_recursive_conflicts(source.as_ref(), &destination, &target_root, &selection)
            .unwrap()
            .len(),
        1
    );
}
impl Drop for Server {
    fn drop(&mut self) {
        assert_eq!(
            self.state.lock().unwrap().mutations,
            0,
            "Inhaltsklasse sendete eine Mutation"
        );
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(std::time::Duration::from_secs(1));
        }
    }
}

fn tree_file(server: &Server, path: &str, data: Vec<u8>) {
    let mut state = server.state.lock().unwrap();
    let mut attributes = attrs(&state, true);
    attributes.size = Some(data.len() as u64);
    state
        .tree
        .as_mut()
        .unwrap()
        .insert(path.into(), (attributes, data));
}

#[test]
fn sftp_baum_erhaelt_abbruchcode_und_erfasst_sekundaeren_closeverlust_fuer_den_naechsten_leser() {
    let server = Server::tree();
    let cancel = Cancel::new();
    let (provider, source) = tree_context(&server, &cancel);
    {
        let mut state = server.state.lock().unwrap();
        state.fault = Fault::CloseLost;
        state.cancel_on_readdir = Some(cancel);
    }
    assert_eq!(
        error_code(
            &source
                .interactive_copy_children(Path::new("/pane/Baum"))
                .unwrap_err()
        ),
        ErrorCode::Interrupted
    );
    {
        let mut state = server.state.lock().unwrap();
        state.fault = Fault::None;
        state.cancel_on_readdir = None;
    }
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let (fresh, _) = endpoint
        .interactive_file_read_context(&[PathBuf::from("/pane/Baum")], &Cancel::new())
        .unwrap();
    assert_eq!(provider.session_generation(), 2);
    assert!(fresh.stat(Path::new("/pane/Baum")).unwrap().is_dir);
    assert_eq!(server.state.lock().unwrap().opens, 0);
}

#[test]
fn sftp_baum_braucht_fuer_ordner_keine_erfundene_groesse_oder_zeit() {
    let server = Server::tree();
    for (attributes, _) in server
        .state
        .lock()
        .unwrap()
        .tree
        .as_mut()
        .unwrap()
        .values_mut()
    {
        if attributes.is_dir() {
            attributes.size = None;
            attributes.mtime = None;
        }
    }
    let (_, source) = tree_context(&server, &Cancel::new());
    assert_eq!(
        source
            .interactive_source_tree()
            .unwrap()
            .unwrap()
            .directories,
        2
    );
    assert!(source
        .interactive_copy_children(Path::new("/pane/Baum/Leer"))
        .unwrap()
        .unwrap()
        .is_empty());
}

#[test]
fn sftp_baum_schlusscheck_bilanziert_spaete_drift_und_erhaltene_lokale_teilwirkung() {
    let server = Server::tree();
    let (_, source) = tree_context(&server, &Cancel::new());
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let dst = LocalVfs::rooted(&root).unwrap();
    std::fs::create_dir(root.join("Baum")).unwrap();
    std::fs::write(root.join("Baum/Extra"), b"Alt").unwrap();
    let mut changed = false;
    let error = copy_items(
        FileOperationRequest {
            src: source.as_ref(),
            dst: &dst,
            items: &[PathBuf::from("/pane/Baum")],
            dst_dir: &root,
            policy: OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut |progress| {
            if !changed && progress.files_done > 0 {
                changed = true;
                tree_file(&server, "/vault/pane/Baum/Neu", b"Neu".to_vec());
            }
        },
    )
    .unwrap_err();
    assert!(changed);
    assert_eq!(error_code(&error), ErrorCode::PlanDrift);
    assert_eq!(
        std::fs::read(root.join("Baum/Kind.txt")).unwrap(),
        b"Bauminhalt\n"
    );
    assert_eq!(std::fs::read(root.join("Baum/Extra")).unwrap(), b"Alt");
    assert!(error
        .to_string()
        .contains(root.join("Baum").to_str().unwrap()));
}

#[test]
fn sftp_baum_abbruch_und_spaete_kollision_erhalten_bestehenden_zielbaum() {
    for collision in [false, true] {
        let server = Server::tree();
        tree_file(&server, "/vault/pane/Baum/Kind.txt", vec![0x63; 256 * 1024]);
        let cancel = Cancel::new();
        let (_, source) = tree_context(&server, &cancel);
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let dst = LocalVfs::rooted(&root).unwrap();
        std::fs::create_dir(root.join("Baum")).unwrap();
        std::fs::write(root.join("Baum/Extra"), b"Alt").unwrap();
        let mut triggered = false;
        let result = copy_items(
            FileOperationRequest {
                src: source.as_ref(),
                dst: &dst,
                items: &[PathBuf::from("/pane/Baum")],
                dst_dir: &root,
                policy: OverwritePolicy::Skip,
                sandbox: &None,
            },
            &cancel,
            &mut |progress| {
                if !triggered && progress.bytes_done > 0 {
                    triggered = true;
                    if collision {
                        std::fs::write(root.join("Baum/Kind.txt"), b"Fremd").unwrap();
                    } else {
                        cancel.cancel();
                    }
                }
            },
        );
        assert!(triggered);
        assert_eq!(std::fs::read(root.join("Baum/Extra")).unwrap(), b"Alt");
        if collision {
            assert!(result.is_err());
            assert_eq!(std::fs::read(root.join("Baum/Kind.txt")).unwrap(), b"Fremd");
        } else {
            assert!(result.unwrap().cancelled);
            assert!(!root.join("Baum/Kind.txt").exists());
        }
    }
}

#[test]
fn sftp_baum_crash_reopen_erhaelt_privaten_tempintent_und_startet_nicht_automatisch_neu() {
    let server = Server::tree();
    tree_file(
        &server,
        "/vault/pane/Baum/Kind.txt",
        vec![0x63; 4 * 1024 * 1024],
    );
    let (_, source) = tree_context(&server, &Cancel::new());
    let expected = source.interactive_source_tree().unwrap();
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = root.join("ziel");
    std::fs::create_dir(&target).unwrap();
    let config = private_config(&root);
    let journal = config.join("queue.json");
    let queue = OperationQueue::open(&journal).unwrap();
    queue
        .enqueue(tree_operation(&server, &target, expected))
        .unwrap();
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
            .filter(|line| *line == "sftp_download_crash_child: test")
            .count(),
        1
    );
    let status = std::process::Command::new(binary)
        .args(["--exact", "sftp_download_crash_child", "--ignored"])
        .env("DOPPELDECKER_AP04_CRASH_ROOT", &root)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(91));
    let queue = OperationQueue::open(&journal).unwrap();
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.entries[0].state, QueueState::Interrupted);
    assert!(!snapshot.entries[0].temp_recovery_paths.is_empty());
    assert!(queue.claim_next().unwrap().is_none());
    assert!(!target.join("Baum/Kind.txt").exists());
    assert!(snapshot.entries[0]
        .temp_recovery_paths
        .iter()
        .any(|path| Path::new(path).exists()));
}

#[test]
fn sftp_baum_abbruch_nach_neuem_zielordner_erhaelt_pruefpfade_ohne_blinden_neustart() {
    let server = Server::tree();
    tree_file(&server, "/vault/pane/Baum/Kind.txt", vec![0x63; 256 * 1024]);
    let (_, source) = tree_context(&server, &Cancel::new());
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = root.join("ziel");
    std::fs::create_dir(&target).unwrap();
    let config = private_config(&root);
    let journal = config.join("queue.json");
    let queue = OperationQueue::open(&journal).unwrap();
    queue
        .enqueue(tree_operation(
            &server,
            &target,
            source.interactive_source_tree().unwrap(),
        ))
        .unwrap();
    let cancel = Cancel::new();
    let mut cancelled = false;
    let error = queue::execute_provider_file_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &execution(&root),
        Arc::new(FakeSecrets),
        &cancel,
        &mut |progress| {
            if progress.bytes_done > 0 {
                cancelled = true;
                cancel.cancel();
            }
        },
    )
    .unwrap_err();
    assert!(cancelled);
    assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    assert!(target.join("Baum").is_dir());
    assert!(!target.join("Baum/Kind.txt").exists());
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.entries[0].state, QueueState::Failed);
    assert_eq!(
        snapshot.entries[0].error_code,
        Some(ErrorCode::OutcomeUnclear)
    );
    let retained = snapshot.entries[0].temp_recovery_paths.clone();
    assert!(retained
        .iter()
        .any(|path| Path::new(path) == target.join("Baum")));
    assert!(retained
        .iter()
        .all(|path| Path::new(path).starts_with(&root)));
    drop(queue);
    let reopened = OperationQueue::open(journal).unwrap();
    let recovered = reopened.snapshot().entries[0].temp_recovery_paths.clone();
    assert!(retained.iter().all(|path| recovered.contains(path)));
    assert!(recovered
        .iter()
        .all(|path| Path::new(path).starts_with(&root)));
    assert!(reopened.claim_next().unwrap().is_none());
}

fn tree_context(server: &Server, cancel: &Cancel) -> (Arc<SftpVfs>, Arc<dyn Vfs>) {
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let (source, _) = endpoint
        .interactive_file_read_context(&[PathBuf::from("/pane/Baum")], cancel)
        .unwrap();
    (provider, source)
}

fn tree_operation(
    server: &Server,
    target: &Path,
    summary: Option<doppeldecker_core::vfs::InteractiveSourceTree>,
) -> QueueOperation {
    let mut value = operation(&server.config, target);
    if let QueueOperation::ProviderFile {
        items,
        source_tree_expected,
        ..
    } = &mut value
    {
        *items = vec!["/pane/Baum".into()];
        *source_tree_expected = summary.map(Box::new);
    }
    value
}

#[test]
#[ignore = "benötigt eigene markierte native/exFAT-Testimages"]
fn sftp_baum_downloads_binden_tatsaechliche_lokale_volumeklasse_und_neustart() {
    use doppeldecker_core::vfs::FileCommitClass;
    use std::os::unix::fs::MetadataExt;
    let root = PathBuf::from(
        std::env::var_os("DOPPELDECKER_METADATA_TEST_ROOT").expect("eigener Image-Root fehlt"),
    );
    assert!(root
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("doppeldecker-metadata-e2e-"));
    assert_eq!(
        std::fs::read(root.join(".doppeldecker-metadata-test")).unwrap(),
        b"doppeldecker-metadata-e2e-v1\n"
    );
    assert_eq!(std::fs::symlink_metadata(&root).unwrap().uid(), unsafe {
        libc::geteuid()
    });
    let root = root.canonicalize().unwrap();
    let server = Server::tree();
    tree_file(
        &server,
        "/vault/pane/Baum/Grüße %2F.txt",
        "Grüße\n".as_bytes().to_vec(),
    );
    {
        let mut state = server.state.lock().unwrap();
        state.mtime = 1704067200;
        for (attributes, _) in state.tree.as_mut().unwrap().values_mut() {
            attributes.mtime = Some(1704067200);
        }
    }
    let (_, source) = tree_context(&server, &Cancel::new());
    let expected = source.interactive_source_tree().unwrap();
    for volume in ["native", "exfat"] {
        let target = tempfile::tempdir_in(root.join(volume)).unwrap();
        let target_root = target.path().canonicalize().unwrap();
        // Wie die Anwendung: Recoverykonfiguration auf einem privaten Host-Namensraum, unabhängig vom Datenvolume.
        let state = tempfile::tempdir_in(&root).unwrap();
        let config = private_config(state.path());
        let journal = config.join("queue.json");
        let target_vfs = LocalVfs::rooted(&target_root).unwrap();
        let weak =
            target_vfs.file_commit_class_at(&target_root).unwrap() == FileCommitClass::BestEffort;
        assert_eq!(weak, cfg!(target_os = "macos") && volume == "exfat");
        let mut queue = OperationQueue::open(&journal).unwrap();
        let entry = queue
            .enqueue(tree_operation(&server, &target_root, expected.clone()))
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        let first = queue::execute_provider_file_queue_entry(
            &queue,
            claimed,
            &execution(&root),
            Arc::new(FakeSecrets),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        let result = if weak {
            assert!(matches!(
                first,
                QueueExecutionResult::AwaitingConfirmation { .. }
            ));
            assert_eq!(std::fs::read_dir(&target_root).unwrap().count(), 0);
            let snapshot = queue.snapshot();
            let QueueAttention::LocalFileCommit {
                confirmation,
                target_identity,
                ..
            } = snapshot.entries[0].attention.as_ref().unwrap()
            else {
                panic!("Volumebindung fehlt")
            };
            assert_eq!(target_identity.display_path, target_root);
            assert!(queue
                .approve_attention(&entry.operation_id, "falsch")
                .is_err());
            queue
                .approve_attention(&entry.operation_id, confirmation)
                .unwrap();
            drop(queue);
            queue = OperationQueue::open(&journal).unwrap();
            let claimed = queue.claim_next().unwrap().unwrap();
            queue::execute_provider_file_queue_entry(
                &queue,
                claimed,
                &execution(&root),
                Arc::new(FakeSecrets),
                &Cancel::new(),
                &mut |_| {},
            )
            .unwrap()
        } else {
            first
        };
        let QueueExecutionResult::File(stats) = result else {
            panic!("kein abgeschlossener Download")
        };
        assert_eq!(stats.files, 2);
        assert_eq!(stats.target_copy_new_best_effort, weak);
        assert_eq!(stats.source_tree, expected);
        assert_eq!(
            std::fs::read(target_root.join("Baum/Kind.txt")).unwrap(),
            b"Bauminhalt\n"
        );
        assert_eq!(
            std::fs::read(target_root.join("Baum/Grüße %2F.txt")).unwrap(),
            "Grüße\n".as_bytes()
        );
        assert!(target_root.join("Baum/Leer").is_dir());
        drop(queue);
        let queue = OperationQueue::open(&journal).unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    }
}

#[test]
fn begrenzte_sftp_quellklasse_erlaubt_auch_im_direkten_core_keine_ersetzung_oder_move() {
    for recursive in [false, true] {
        let server = if recursive {
            Server::tree()
        } else {
            Server::new(b"Neu".to_vec(), Fault::None)
        };
        let (_, source) = if recursive {
            tree_context(&server, &Cancel::new())
        } else {
            server.context(&Cancel::new())
        };
        let item = PathBuf::from(if recursive {
            "/pane/Baum"
        } else {
            "/pane/inhalt.txt"
        });
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let dst = LocalVfs::rooted(&root).unwrap();
        let relative = if recursive {
            "Baum/Kind.txt"
        } else {
            "inhalt.txt"
        };
        if recursive {
            std::fs::create_dir(root.join("Baum")).unwrap();
        }
        std::fs::write(root.join(relative), b"Alt").unwrap();
        for moving in [false, true] {
            let items = [item.clone()];
            let request = FileOperationRequest {
                src: source.as_ref(),
                dst: &dst,
                items: &items,
                dst_dir: &root,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            };
            let result = if moving {
                move_items(request, &Cancel::new(), &mut |_| {})
            } else {
                copy_items(request, &Cancel::new(), &mut |_| {})
            };
            assert!(
                result.is_err(),
                "Schwache Quellklasse erlaubte Ersetzung/Move"
            );
            assert_eq!(std::fs::read(root.join(relative)).unwrap(), b"Alt");
        }
    }
}

#[test]
fn sftp_baum_bilanziert_symlinks_spezialdateien_und_fehlende_belege_bei_paginiertem_listing() {
    let server = Server::tree();
    tree_file(
        &server,
        "/vault/pane/Baum/Grüße %2F.txt",
        "Grüße\n".as_bytes().to_vec(),
    );
    {
        let mut state = server.state.lock().unwrap();
        state.page_size = 1;
        let regular = attrs(&state, true);
        for (name, mode, size) in [
            ("Link", Some(0o120777), Some(11)),
            ("Pipe", Some(0o010600), Some(0)),
            ("Unklar", Some(0o100600), None),
        ] {
            let mut attributes = regular.clone();
            attributes.permissions = mode;
            attributes.size = size;
            state
                .tree
                .as_mut()
                .unwrap()
                .insert(format!("/vault/pane/Baum/{name}"), (attributes, Vec::new()));
        }
    }
    let (_provider, source) = tree_context(&server, &Cancel::new());
    let summary = source.interactive_source_tree().unwrap().unwrap();
    assert_eq!(
        (
            summary.entries,
            summary.files,
            summary.directories,
            summary.omitted.len()
        ),
        (7, 2, 2, 3)
    );
    assert!(summary
        .omitted
        .iter()
        .all(|item| !item.reason.is_empty() && item.path.starts_with("/pane/Baum/")));
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let dst = LocalVfs::rooted(&root).unwrap();
    let stats = copy_items(
        FileOperationRequest {
            src: source.as_ref(),
            dst: &dst,
            items: &[PathBuf::from("/pane/Baum")],
            dst_dir: &root,
            policy: OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((stats.files, stats.skipped), (2, 3));
    assert_eq!(stats.source_tree, Some(summary));
    assert!(root.join("Baum/Leer").is_dir());
    assert_eq!(
        std::fs::read(root.join("Baum/Grüße %2F.txt")).unwrap(),
        "Grüße\n".as_bytes()
    );
    for name in ["Link", "Pipe", "Unklar"] {
        assert!(!root.join("Baum").join(name).exists());
    }
}

#[test]
fn sftp_baum_verlangt_eof_eindeutige_namen_und_erhaelt_fehler_vor_jeder_zielwirkung() {
    for fault in [
        Fault::TreeDuplicate,
        Fault::TreeInvalidName,
        Fault::TreeEmptyPage,
        Fault::TreeListingLost,
        Fault::ListingDenied,
    ] {
        let server = Server::tree();
        server.state.lock().unwrap().fault = fault;
        let provider = server.connect();
        let endpoint = ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap();
        let result =
            endpoint.interactive_file_read_context(&[PathBuf::from("/pane/Baum")], &Cancel::new());
        let error = match result {
            Ok(_) => panic!("Ungültiges Listing akzeptiert: {fault:?}"),
            Err(error) => error,
        };
        if fault == Fault::TreeListingLost {
            assert_eq!(error_code(&error), ErrorCode::ConnectionLost);
        }
        assert_eq!(server.state.lock().unwrap().opens, 0);
    }
    let server = Server::tree();
    server.state.lock().unwrap().fault = Fault::TreePartialAttributes;
    let (_, source) = tree_context(&server, &Cancel::new());
    assert_eq!(source.interactive_source_tree().unwrap().unwrap().files, 1);
}

#[test]
fn sftp_baum_begrenzt_menge_tiefe_gesamtbytes_und_lokale_namensmehrdeutigkeit() {
    for variant in 0..4 {
        let server = Server::tree();
        match variant {
            0 => {
                for i in 0..1000 {
                    tree_file(&server, &format!("/vault/pane/Baum/f{i}"), Vec::new());
                }
            }
            1 => {
                let mut state = server.state.lock().unwrap();
                let directory = attrs(&state, false);
                let mut path = "/vault/pane/Baum".to_owned();
                for _ in 0..17 {
                    path.push_str("/Tief");
                    state
                        .tree
                        .as_mut()
                        .unwrap()
                        .insert(path.clone(), (directory.clone(), Vec::new()));
                }
            }
            2 => {
                let mut state = server.state.lock().unwrap();
                let mut attributes = attrs(&state, true);
                attributes.size = Some(sftp::SFTP_MAX_INTERACTIVE_FILE_BYTES);
                for name in ["A", "B", "C"] {
                    state.tree.as_mut().unwrap().insert(
                        format!("/vault/pane/Baum/{name}"),
                        (attributes.clone(), Vec::new()),
                    );
                }
            }
            _ => {
                tree_file(&server, "/vault/pane/Baum/Name", Vec::new());
                tree_file(&server, "/vault/pane/Baum/name", Vec::new());
            }
        }
        let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
        assert!(
            endpoint
                .interactive_file_read_context(&[PathBuf::from("/pane/Baum")], &Cancel::new())
                .is_err(),
            "Grenze {variant} ignoriert"
        );
        assert_eq!(server.state.lock().unwrap().opens, 0);
    }
}

#[test]
fn sftp_baum_erkennt_bestands_und_elterndrift_vor_lokaler_anlage() {
    for variant in 0..2 {
        let server = Server::tree();
        let (_, source) = tree_context(&server, &Cancel::new());
        if variant == 0 {
            tree_file(&server, "/vault/pane/Baum/Neu.txt", b"Neu".to_vec());
        } else {
            server
                .state
                .lock()
                .unwrap()
                .tree
                .as_mut()
                .unwrap()
                .get_mut("/vault/pane/Baum")
                .unwrap()
                .0
                .uid = Some(2000);
        }
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let dst = LocalVfs::rooted(&root).unwrap();
        let error = copy_items(
            FileOperationRequest {
                src: source.as_ref(),
                dst: &dst,
                items: &[PathBuf::from("/pane/Baum")],
                dst_dir: &root,
                policy: OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }
}

#[test]
fn sftp_baum_reconnect_bindet_neu_ohne_den_neustartdigest_an_eine_generation_zu_koppeln() {
    let server = Server::tree();
    let (provider, old) = tree_context(&server, &Cancel::new());
    let expected = old.interactive_source_tree().unwrap();
    server.state.lock().unwrap().fault = Fault::TreeListingLost;
    assert_eq!(
        error_code(
            &old.interactive_copy_children(Path::new("/pane/Baum"))
                .unwrap_err()
        ),
        ErrorCode::ConnectionLost
    );
    server.state.lock().unwrap().fault = Fault::None;
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let (fresh, _) = endpoint
        .interactive_file_read_context(&[PathBuf::from("/pane/Baum")], &Cancel::new())
        .unwrap();
    assert_eq!(provider.session_generation(), 2);
    assert_eq!(fresh.interactive_source_tree().unwrap(), expected);
    assert_eq!(
        error_code(&old.stat(Path::new("/pane/Baum")).unwrap_err()),
        ErrorCode::PlanDrift
    );
}

#[test]
fn sftp_baum_queue_bestaetigt_neuen_umfang_erhaelt_ihn_beim_neustart_und_stoppt_spaetere_drift() {
    let server = Server::tree();
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = root.join("ziel");
    std::fs::create_dir(&target).unwrap();
    let config = private_config(&root);
    let journal = config.join("queue.json");
    let queue = OperationQueue::open(&journal).unwrap();
    let entry = queue
        .enqueue(tree_operation(&server, &target, None))
        .unwrap();
    let claimed = queue.claim_next().unwrap().unwrap();
    let result = queue::execute_provider_file_queue_entry(
        &queue,
        claimed,
        &execution(&root),
        Arc::new(FakeSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(matches!(
        result,
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    assert!(!target.join("Baum").exists());
    let snapshot = queue.snapshot();
    let phrase = match snapshot.entries[0].attention.as_ref().unwrap() {
        QueueAttention::SourceTreeChanged { confirmation, .. } => confirmation.clone(),
        other => panic!("{other:?}"),
    };
    assert!(queue
        .approve_attention(&entry.operation_id, "falsch")
        .is_err());
    drop(queue);
    let queue = OperationQueue::open(&journal).unwrap();
    queue
        .approve_attention(&entry.operation_id, &phrase)
        .unwrap();
    tree_file(&server, "/vault/pane/Baum/Neu.txt", b"Neu".to_vec());
    let claimed = queue.claim_next().unwrap().unwrap();
    let result = queue::execute_provider_file_queue_entry(
        &queue,
        claimed,
        &execution(&root),
        Arc::new(FakeSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(matches!(
        result,
        QueueExecutionResult::AwaitingConfirmation { .. }
    ));
    assert!(!target.join("Baum").exists());
    let snapshot = queue.snapshot();
    let phrase = match snapshot.entries[0].attention.as_ref().unwrap() {
        QueueAttention::SourceTreeChanged {
            confirmation,
            current_tree,
            ..
        } => {
            assert_eq!(current_tree.as_ref().unwrap().files, 2);
            confirmation.clone()
        }
        other => panic!("{other:?}"),
    };
    queue
        .approve_attention(&entry.operation_id, &phrase)
        .unwrap();
    let claimed = queue.claim_next().unwrap().unwrap();
    let result = queue::execute_provider_file_queue_entry(
        &queue,
        claimed,
        &execution(&root),
        Arc::new(FakeSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    let QueueExecutionResult::File(stats) = result else {
        panic!("kein abgeschlossener Download")
    };
    assert_eq!(stats.files, 2);
    assert_eq!(std::fs::read(target.join("Baum/Neu.txt")).unwrap(), b"Neu");
    assert!(target.join("Baum/Leer").is_dir());
}

#[test]
fn sftp_baum_konflikte_erhalten_altinhalte_und_beide_behalten_bindet_den_ganzen_quellinhalt() {
    let server = Server::tree();
    let (_, source) = tree_context(&server, &Cancel::new());
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let dst = LocalVfs::rooted(&root).unwrap();
    std::fs::create_dir(root.join("Baum")).unwrap();
    std::fs::write(root.join("Baum/Kind.txt"), b"Alt").unwrap();
    std::fs::write(root.join("Baum/Extra"), b"Zusatz").unwrap();
    let items = [PathBuf::from("/pane/Baum")];
    let conflicts = find_recursive_conflicts(source.as_ref(), &dst, &root, &items).unwrap();
    assert_eq!(conflicts.len(), 1);
    let views =
        fileops::file_conflict_views(source.as_ref(), &dst, &root, &items, &conflicts).unwrap();
    assert_eq!(views.len(), 1);
    let targets =
        fileops::prepare_keep_both_targets(source.as_ref(), &dst, &root, &items, &conflicts)
            .unwrap();
    assert_eq!(targets.len(), 1);
    let first = targets[0].source_proof.content_hash;
    assert!(first.is_some());
    tree_file(
        &server,
        "/vault/pane/Baum/Kind.txt",
        b"anderertext".to_vec(),
    );
    assert!(
        fileops::verify_keep_both_targets(source.as_ref(), &dst, &items, &root, &targets).is_err()
    );
    assert_eq!(std::fs::read(root.join("Baum/Kind.txt")).unwrap(), b"Alt");
    assert_eq!(std::fs::read(root.join("Baum/Extra")).unwrap(), b"Zusatz");
}
struct Ssh {
    channels: HashMap<ChannelId, Channel<Msg>>,
    state: Arc<Mutex<State>>,
}
impl russh::server::Handler for Ssh {
    type Error = russh::Error;
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(
            if user == "artificial" && password == "synthetic-ap04-only" {
                Auth::Accept
            } else {
                Auth::reject()
            },
        )
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "sftp" {
            session.channel_success(id)?;
            let channel = self.channels.remove(&id).unwrap();
            let sftp = Sftp {
                state: self.state.clone(),
                handles: HashMap::new(),
                directories: HashMap::new(),
            };
            tokio::spawn(async move {
                russh_sftp::server::run(channel.into_stream(), sftp).await;
            });
        } else {
            session.channel_failure(id)?;
        }
        Ok(())
    }
}
struct Sftp {
    state: Arc<Mutex<State>>,
    handles: HashMap<String, (Vec<u8>, bool)>,
    directories: HashMap<String, (String, usize)>,
}
fn attrs(state: &State, file: bool) -> FileAttributes {
    let mode = if !file {
        0o040700
    } else {
        match state.fault {
            Fault::Symlink => 0o120777,
            Fault::Special => 0o010600,
            _ => 0o100777,
        }
    };
    FileAttributes {
        size: if file && state.fault == Fault::MissingSize {
            None
        } else if file && state.fault == Fault::TooLarge {
            Some(sftp::SFTP_MAX_INTERACTIVE_FILE_BYTES + 1)
        } else {
            Some(if file { state.data.len() as u64 } else { 0 })
        },
        uid: Some(1000),
        gid: Some(1000),
        permissions: Some(mode),
        mtime: Some(state.mtime),
        atime: Some(state.mtime),
        ..FileAttributes::empty()
    }
}
fn status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: String::new(),
    }
}
impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;
    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }
    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let state = self.state.lock().unwrap();
        let swapped = state.read_started
            && ((state.fault == Fault::RootSwap && path == "/vault")
                || (state.fault == Fault::ParentSwap && path == "/vault/pane"));
        Ok(Name {
            id,
            files: vec![File::dummy(if swapped { "/outside".into() } else { path })],
        })
    }
    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let mut state = self.state.lock().unwrap();
        if state.fault == Fault::MetadataLost {
            return Err(StatusCode::ConnectionLost);
        }
        state.metadata_requests += 1;
        if let Some(cancel) = &state.cancel_on_metadata {
            cancel.cancel();
        }
        Ok(Attrs {
            id,
            attrs: match &state.tree {
                Some(tree) => tree.get(&path).ok_or(StatusCode::NoSuchFile)?.0.clone(),
                None => attrs(&state, path != "/vault" && path != "/vault/pane"),
            },
        })
    }
    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let state = self.state.lock().unwrap();
        if state.fault == Fault::ListingDenied {
            return Err(StatusCode::PermissionDenied);
        }
        let tree = state.tree.as_ref().ok_or(StatusCode::OpUnsupported)?;
        if !tree.get(&path).ok_or(StatusCode::NoSuchFile)?.0.is_dir() {
            return Err(StatusCode::Failure);
        }
        let handle = format!("directory-{id}");
        self.directories.insert(handle.clone(), (path, 0));
        Ok(Handle { id, handle })
    }
    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let slow_dots = self.state.lock().unwrap().fault == Fault::SlowDotPages;
        if slow_dots {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            return Ok(Name {
                id,
                files: vec![File::dummy("."), File::dummy("..")],
            });
        }
        let (path, offset) = self
            .directories
            .get_mut(&handle)
            .ok_or(StatusCode::Failure)?;
        let state = self.state.lock().unwrap();
        if let Some(cancel) = &state.cancel_on_readdir {
            cancel.cancel();
        }
        if state.fault == Fault::TreeListingLost
            || state
                .listing_fail_after
                .is_some_and(|limit| *offset >= limit)
        {
            return Err(StatusCode::ConnectionLost);
        }
        if state.fault == Fault::TreeEmptyPage {
            return Ok(Name {
                id,
                files: Vec::new(),
            });
        }
        let tree = state.tree.as_ref().ok_or(StatusCode::OpUnsupported)?;
        let mut files = tree
            .iter()
            .filter(|(name, _)| Path::new(name).parent() == Some(Path::new(path)))
            .map(|(name, (attributes, _))| File {
                filename: Path::new(name)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .into(),
                longname: String::new(),
                attrs: attributes.clone(),
            })
            .collect::<Vec<_>>();
        files.sort_by(|a, b| a.filename.cmp(&b.filename));
        if state.fault == Fault::TreeDuplicate && !files.is_empty() {
            files.push(files[0].clone());
        }
        if state.fault == Fault::TreeInvalidName && !files.is_empty() {
            files[0].filename = "../ausbruch".into();
        }
        if state.fault == Fault::TreePartialAttributes {
            for file in &mut files {
                file.attrs.uid = None;
                file.attrs.gid = None;
            }
        }
        if *offset >= files.len() {
            return Err(StatusCode::Eof);
        }
        let end = offset.saturating_add(state.page_size).min(files.len());
        let files = files[*offset..end].to_vec();
        *offset = end;
        Ok(Name { id, files })
    }
    async fn open(
        &mut self,
        id: u32,
        path: String,
        flags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let mut state = self.state.lock().unwrap();
        if flags.bits() != OpenFlags::READ.bits() {
            state.mutations += 1;
            return Err(StatusCode::PermissionDenied);
        }
        state.opens += 1;
        let second = state.opens.is_multiple_of(2);
        if second && state.fault == Fault::ReadbackDenied {
            return Err(StatusCode::PermissionDenied);
        }
        let mut data = match &state.tree {
            Some(tree) => tree.get(&path).ok_or(StatusCode::NoSuchFile)?.1.clone(),
            None => state.data.clone(),
        };
        if second && state.fault == Fault::ContentDrift {
            data.fill(0x71);
        }
        if !second && state.fault == Fault::Truncate {
            data.pop();
        }
        if !second && state.fault == Fault::Grow {
            data.push(0x70);
        }
        let handle = format!("file-{}", state.opens);
        self.handles.insert(handle.clone(), (data, second));
        Ok(Handle { id, handle })
    }
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        if !self.handles.contains_key(&handle) {
            return Err(StatusCode::Failure);
        }
        let state = self.state.lock().unwrap();
        let mut attributes = attrs(&state, true);
        if state.tree.is_some() {
            attributes.size = Some(self.handles[&handle].0.len() as u64);
        }
        if state.read_started && state.fault == Fault::FstatDrift {
            attributes.mtime = Some(123457);
        }
        Ok(Attrs {
            id,
            attrs: attributes,
        })
    }
    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        length: u32,
    ) -> Result<Data, Self::Error> {
        let mut state = self.state.lock().unwrap();
        state.read_started = true;
        if state.fault == Fault::LostResponse {
            return Err(StatusCode::ConnectionLost);
        }
        let (data, _) = self.handles.get(&handle).ok_or(StatusCode::Failure)?;
        if state.fault == Fault::EmptyData {
            return Ok(Data {
                id,
                data: Vec::new(),
            });
        }
        if state.fault == Fault::OversizedData {
            return Ok(Data {
                id,
                data: vec![0x70; length as usize + 1],
            });
        }
        let offset = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
        if offset >= data.len() {
            return Err(StatusCode::Eof);
        }
        Ok(Data {
            id,
            data: data[offset..data.len().min(offset + length as usize)].to_vec(),
        })
    }
    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        let readback = self
            .handles
            .remove(&handle)
            .is_some_and(|(_, second)| second);
        self.directories.remove(&handle);
        let mut state = self.state.lock().unwrap();
        state.closes += 1;
        if state.fault == Fault::CloseLost || (readback && state.lose_readback_close) {
            return Err(StatusCode::ConnectionLost);
        }
        Ok(status(id))
    }
}

#[test]
fn snapshot_und_additive_kopie_lesen_leere_und_grosse_inhalte_ohne_ausfuehrungsrechte() {
    use std::os::unix::fs::PermissionsExt;
    for data in [
        Vec::new(),
        b"SFTP-Inhalt\n".to_vec(),
        vec![0x63; 2 * 1024 * 1024 + 13],
    ] {
        let server = Server::new(data.clone(), Fault::None);
        let cancel = Cancel::new();
        let (_provider, vfs) = server.context(&cancel);
        let cache = tempfile::tempdir().unwrap();
        let snapshot = download_remote_snapshot(
            vfs.as_ref(),
            Path::new("/pane/inhalt.txt"),
            cache.path(),
            &cancel,
        )
        .unwrap();
        assert_eq!(std::fs::read(&snapshot.path).unwrap(), data);
        assert_eq!(
            std::fs::metadata(&snapshot.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let target = tempfile::tempdir().unwrap();
        let target_root = target.path().canonicalize().unwrap();
        let destination = LocalVfs::rooted(&target_root).unwrap();
        let stats = fileops::copy_items(
            fileops::FileOperationRequest {
                src: vfs.as_ref(),
                dst: &destination,
                items: &[PathBuf::from("/pane/inhalt.txt")],
                dst_dir: &target_root,
                policy: OverwritePolicy::Skip,
                sandbox: &Some(target_root.clone()),
            },
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(stats.files, 1);
        assert_eq!(
            std::fs::read(target.path().join("inhalt.txt")).unwrap(),
            data
        );
        assert_eq!(
            std::fs::metadata(target.path().join("inhalt.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert!(vfs.root_identity(Path::new("/pane")).is_err());
        assert!(vfs.list_complete(Path::new("/pane")).is_err());
        assert!(vfs.open_write_new(Path::new("/pane/inhalt.txt")).is_err());
        assert!(!vfs.supports_mode_write() && !vfs.supports_mtime_write());
        assert_eq!(
            vfs.file_flush_guarantees_at(Path::new("/pane")).unwrap(),
            (false, false)
        );
    }
}

#[test]
fn symlink_spezialdatei_fehlende_belege_und_groessenlimit_stoppen_vor_open() {
    for fault in [
        Fault::Symlink,
        Fault::Special,
        Fault::MissingSize,
        Fault::TooLarge,
    ] {
        let server = Server::new(b"Daten".to_vec(), fault);
        let provider = server.connect();
        let endpoint = ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap();
        assert!(
            endpoint
                .interactive_file_read_context(&[PathBuf::from("/pane/inhalt.txt")], &Cancel::new())
                .is_err(),
            "{fault:?}"
        );
        assert_eq!(server.state.lock().unwrap().opens, 0);
    }
}

#[test]
fn pfadtausch_inhaltsdrift_abgeschnittener_stream_und_verlorene_antwort_ergeben_keinen_snapshot() {
    for fault in [
        Fault::RootSwap,
        Fault::ParentSwap,
        Fault::ContentDrift,
        Fault::Truncate,
        Fault::Grow,
        Fault::EmptyData,
        Fault::OversizedData,
        Fault::FstatDrift,
        Fault::ReadbackDenied,
        Fault::LostResponse,
        Fault::CloseLost,
    ] {
        let server = Server::new(vec![0x63; 128 * 1024 + 1], fault);
        let cancel = Cancel::new();
        let (_provider, vfs) = server.context(&cancel);
        let cache = tempfile::tempdir().unwrap();
        let error = download_remote_snapshot(
            vfs.as_ref(),
            Path::new("/pane/inhalt.txt"),
            cache.path(),
            &cancel,
        )
        .unwrap_err();
        assert_ne!(
            error_code(&error),
            ErrorCode::OutcomeUnclear,
            "{fault:?}: keine lokale Mutation committed"
        );
        assert_eq!(
            std::fs::read_dir(cache.path()).unwrap().count(),
            0,
            "{fault:?}"
        );
    }
}

#[test]
fn abbruch_und_spaete_kollision_erhalten_das_ziel_und_bilanzieren_tempreste() {
    for abort in [true, false] {
        let server = Server::new(vec![0x63; 2 * 1024 * 1024], Fault::None);
        let cancel = Cancel::new();
        let (_provider, vfs) = server.context(&cancel);
        let target = tempfile::tempdir().unwrap();
        let target_root = target.path().canonicalize().unwrap();
        let destination = LocalVfs::rooted(&target_root).unwrap();
        let mut injected = false;
        let result = fileops::copy_items(
            fileops::FileOperationRequest {
                src: vfs.as_ref(),
                dst: &destination,
                items: &[PathBuf::from("/pane/inhalt.txt")],
                dst_dir: &target_root,
                policy: OverwritePolicy::Skip,
                sandbox: &Some(target_root.clone()),
            },
            &cancel,
            &mut |progress| {
                if progress.bytes_done > 0 && !injected {
                    injected = true;
                    if abort {
                        cancel.cancel();
                    } else {
                        std::fs::write(target.path().join("inhalt.txt"), b"fremd").unwrap();
                    }
                }
            },
        );
        assert!(injected);
        let recovery = if abort {
            let stats = result.unwrap();
            assert!(stats.cancelled);
            assert!(!target.path().join("inhalt.txt").exists());
            stats.temp_recovery_paths
        } else {
            let error = result.unwrap_err();
            assert_eq!(
                std::fs::read(target.path().join("inhalt.txt")).unwrap(),
                b"fremd"
            );
            let paths = std::fs::read_dir(target.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.file_name().unwrap() != "inhalt.txt")
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            for path in &paths {
                assert!(error.to_string().contains(path));
            }
            paths
        };
        assert!(!recovery.is_empty());
        for entry in std::fs::read_dir(target.path()).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() != "inhalt.txt" {
                assert!(recovery
                    .iter()
                    .any(|path| Path::new(path).canonicalize().unwrap()
                        == entry.path().canonicalize().unwrap()));
            }
        }
    }
}

#[test]
fn sitzungswechsel_und_abbruch_verbrauchen_keinen_alten_lesehandle() {
    let server = Server::new(vec![0x63; 128 * 1024], Fault::None);
    let cancel = Cancel::new();
    let (provider, vfs) = server.context(&cancel);
    let mut reader = vfs.open_read(Path::new("/pane/inhalt.txt")).unwrap();
    assert_eq!(reader.read(&mut [0; 10]).unwrap(), 10);
    provider.reconnect().unwrap();
    assert_eq!(
        error_code(&reader.read(&mut [0; 10]).unwrap_err()),
        ErrorCode::PlanDrift
    );
    assert!(vfs.stat(Path::new("/pane/inhalt.txt")).is_err());
    let already_cancelled = Cancel::new();
    already_cancelled.cancel();
    let endpoint = ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap();
    assert_eq!(
        error_code(&match endpoint
            .interactive_file_read_context(&[PathBuf::from("/pane/inhalt.txt")], &already_cancelled)
        {
            Ok(_) => panic!("Abbruch ignoriert"),
            Err(error) => error,
        }),
        ErrorCode::Interrupted
    );
}

#[test]
fn einzelinhalt_braucht_keine_scheinbar_vollstaendige_verzeichnisfreigabe() {
    let server = Server::new(b"Daten".to_vec(), Fault::ListingDenied);
    let (_provider, vfs) = server.context(&Cancel::new());
    let mut data = Vec::new();
    vfs.open_read(Path::new("/pane/inhalt.txt"))
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();
    assert_eq!(data, b"Daten");
    assert!(vfs.list_complete(Path::new("/pane")).is_err());
}

fn operation(config: &SftpConnectionConfig, target: &Path) -> QueueOperation {
    QueueOperation::ProviderFile {
        kind: FileQueueKind::Copy,
        source: Box::new(BidirectionalQueueEndpoint::Sftp {
            connection: Box::new(config.clone()),
            root: "/pane".into(),
        }),
        target: Box::new(BidirectionalQueueEndpoint::Local {
            root: target.to_string_lossy().into_owned(),
        }),
        items: vec!["/pane/inhalt.txt".into()],
        destination: target.to_string_lossy().into_owned(),
        overwrite: false,
        expected_conflicts: Vec::new(),
        expected_conflict_proofs: Vec::new(),
        source_tree_expected: None,
        keep_both_targets: Vec::new(),
        best_effort_commit_expected: None,
        local_copy_new_best_effort_expected: None,
        copy_new_best_effort_expected: Some(false),
        replace_best_effort_expected: Some(false),
        remove_source_best_effort_expected: None,
    }
}
fn execution(root: &Path) -> LocalQueueExecution {
    LocalQueueExecution {
        sandbox: Some(root.to_path_buf()),
        sync_quarantine: None,
        max_delete_entries: 100,
        max_delete_fraction: 0.25,
    }
}
fn private_config(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let config = root.join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
    config
}

#[test]
fn queue_reopen_verbindet_frisch_bilanziert_klasse_und_erhaelt_vorhandene_ziele() {
    let server = Server::new(b"Queue-Inhalt".to_vec(), Fault::None);
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = root.join("ziel");
    std::fs::create_dir(&target).unwrap();
    let config = private_config(&root);
    let journal = config.join("queue.json");
    let queue = OperationQueue::open(&journal).unwrap();
    queue.enqueue(operation(&server.config, &target)).unwrap();
    drop(queue);
    let persisted = std::fs::read_to_string(&journal).unwrap();
    assert!(!persisted.contains("synthetic-ap04-only"));
    assert!(!persisted.contains("file-1"));
    let queue = OperationQueue::open(&journal).unwrap();
    let result = queue::execute_provider_file_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &execution(&root),
        Arc::new(FakeSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    let QueueExecutionResult::File(stats) = result else {
        panic!("kein Dateiergebnis");
    };
    assert_eq!(stats.files, 1);
    assert!(!stats.source_read_limitations.is_empty());
    assert_eq!(
        std::fs::read(target.join("inhalt.txt")).unwrap(),
        b"Queue-Inhalt"
    );
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    let mut next = operation(&server.config, &target);
    if let QueueOperation::ProviderFile {
        expected_conflicts, ..
    } = &mut next
    {
        expected_conflicts.push(target.join("inhalt.txt").to_string_lossy().into_owned());
    }
    queue.enqueue(next).unwrap();
    let result = queue::execute_provider_file_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &execution(&root),
        Arc::new(FakeSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    let QueueExecutionResult::File(stats) = result else {
        panic!("kein Dateiergebnis");
    };
    assert_eq!(stats.files, 0);
    assert_eq!(stats.skipped, 1);
    assert_eq!(
        std::fs::read(target.join("inhalt.txt")).unwrap(),
        b"Queue-Inhalt"
    );
}

#[test]
fn queue_verweigert_move_replace_und_remote_ziel_vor_jedem_providerkontakt() {
    let server = Server::new(b"Inhalt".to_vec(), Fault::None);
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = root.join("ziel");
    std::fs::create_dir(&target).unwrap();
    let queue = OperationQueue::open(root.join("queue.json")).unwrap();
    for variant in 0..3 {
        let mut operation = operation(&server.config, &target);
        if let QueueOperation::ProviderFile {
            kind,
            target,
            overwrite,
            destination,
            ..
        } = &mut operation
        {
            match variant {
                0 => *kind = FileQueueKind::Move,
                1 => *overwrite = true,
                _ => {
                    **target = BidirectionalQueueEndpoint::Sftp {
                        connection: Box::new(server.config.clone()),
                        root: "/ziel".into(),
                    };
                    *destination = "/ziel".into();
                }
            }
        }
        assert_eq!(
            error_code(&queue.enqueue(operation).unwrap_err()),
            ErrorCode::Unsupported
        );
    }
    assert_eq!(server.state.lock().unwrap().opens, 0);
}

#[test]
fn ungueltige_auswahl_und_hostkeywechsel_erteilen_keine_inhaltsfreigabe() {
    use sftp::validate_interactive_content_selection as validate;
    for items in [
        Vec::new(),
        vec![PathBuf::from("/pane/../x")],
        vec![PathBuf::from("/pane/sub/x")],
        vec![PathBuf::from("/pane/x"), PathBuf::from("/pane/x")],
        (0..101)
            .map(|i| PathBuf::from(format!("/pane/{i}")))
            .collect(),
        vec![PathBuf::from(format!("/pane/{}", "x".repeat(4096)))],
    ] {
        assert!(validate(Path::new("/pane"), &items).is_err());
    }
    let mut server = Server::new(b"Daten".to_vec(), Fault::None);
    server.config.host_key.sha256[0] ^= 1;
    let error = match SftpVfs::connect(server.config.clone(), Arc::new(FakeSecrets)) {
        Ok(_) => panic!("falscher Pin akzeptiert"),
        Err(error) => error,
    };
    assert_eq!(error_code(&error), ErrorCode::PermissionDenied);
    assert_eq!(server.state.lock().unwrap().opens, 0);
}

#[test]
fn unicode_und_prozentnamen_bleiben_einzelne_namen_und_snapshot_erhaelt_nur_sichere_endung() {
    for name in [
        "Grüße %2F.txt",
        "literal%2F.sh",
        "Befehl.$(x)",
        "ausfuehrbar.command",
    ] {
        let server = Server::new(b"Text".to_vec(), Fault::None);
        let provider = server.connect();
        let endpoint = ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap();
        let source = PathBuf::from(format!("/pane/{name}"));
        let (vfs, _) = endpoint
            .interactive_file_read_context(std::slice::from_ref(&source), &Cancel::new())
            .unwrap();
        assert_eq!(vfs.stat(&source).unwrap().name, name);
        let cache = tempfile::tempdir().unwrap();
        let snapshot = endpoint
            .download_interactive_snapshot(&source, cache.path(), &Cancel::new())
            .unwrap();
        if name.ends_with(".command") || name.ends_with(".sh") {
            assert!(snapshot.path.ends_with(".txt"));
        }
        assert_eq!(std::fs::read(&snapshot.path).unwrap(), b"Text");
        assert!(!snapshot.path.contains("$(x)"));
        assert!(!snapshot.path.contains("%2F"));
    }
}

#[test]
fn aktive_webformate_werden_unveraendert_als_text_snapshot_uebergeben() {
    use std::os::unix::fs::PermissionsExt;
    for name in ["aktiv.html", "aktiv.HTM", "aktiv.SvG", "aktiv.xml"] {
        let data = b"<script>document.title='AP06 aktiv';</script>\n";
        let server = Server::new(data.to_vec(), Fault::None);
        let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let snapshot = endpoint
            .download_interactive_snapshot(
                &PathBuf::from(format!("/pane/{name}")),
                cache.path(),
                &Cancel::new(),
            )
            .unwrap();
        assert!(snapshot.path.ends_with(".txt"), "{name}: {}", snapshot.path);
        assert_eq!(std::fs::read(&snapshot.path).unwrap(), data);
        assert_eq!(snapshot.blake3, blake3::hash(data).to_hex().to_string());
        assert_eq!(
            std::fs::metadata(&snapshot.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(server.state.lock().unwrap().mutations, 0);
    }
}

#[test]
fn prozessabbruch_hinterlaesst_einen_queuegebundenen_tempintent_ohne_blinden_retry() {
    use std::os::unix::fs::PermissionsExt;
    let server = Server::new(vec![0x63; 4 * 1024 * 1024], Fault::None);
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = root.join("ziel");
    std::fs::create_dir(&target).unwrap();
    // Ein öffentlich lesbarer Zielordner eignet sich absichtlich nicht als privater
    // C2-Namespace; der Nachbar-Temp muss vor Anlage im privaten Queueindex stehen.
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = private_config(&root);
    let journal = config.join("queue.json");
    let queue = OperationQueue::open(&journal).unwrap();
    queue.enqueue(operation(&server.config, &target)).unwrap();
    drop(queue);
    let binary = std::env::current_exe().unwrap();
    let listing = std::process::Command::new(&binary)
        .arg("--list")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(listing.stdout)
            .unwrap()
            .lines()
            .filter(|line| *line == "sftp_download_crash_child: test")
            .count(),
        1
    );
    let status = std::process::Command::new(binary)
        .args(["--exact", "sftp_download_crash_child", "--ignored"])
        .env("DOPPELDECKER_AP04_CRASH_ROOT", &root)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(91));
    assert!(!target.join("inhalt.txt").exists());
    let queue = OperationQueue::open(&journal).unwrap();
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.entries[0].state, QueueState::Interrupted);
    assert!(!snapshot.entries[0].temp_recovery_paths.is_empty());
    assert!(snapshot.entries[0]
        .temp_recovery_paths
        .iter()
        .any(|path| Path::new(path).is_file()));
    assert!(queue.claim_next().unwrap().is_none());
    assert!(!target.join("inhalt.txt").exists());
}

#[test]
#[ignore = "wird ausschließlich vom begrenzten Prozessabbruch-Gegenlauf gestartet"]
fn sftp_download_crash_child() {
    let root = PathBuf::from(
        std::env::var_os("DOPPELDECKER_AP04_CRASH_ROOT").expect("eigener Testroot fehlt"),
    );
    let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
    let claimed = queue.claim_next().unwrap().unwrap();
    let _ = queue::execute_provider_file_queue_entry(
        &queue,
        claimed,
        &execution(&root),
        Arc::new(FakeSecrets),
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
fn verlorener_inhaltsversuch_scheitert_einmal_und_nur_ein_neuer_leseversuch_verbindet_frisch() {
    let server = Server::new(b"Daten".to_vec(), Fault::LostResponse);
    let (provider, old) = server.context(&Cancel::new());
    let mut reader = old.open_read(Path::new("/pane/inhalt.txt")).unwrap();
    assert_eq!(
        error_code(&reader.read(&mut [0; 10]).unwrap_err()),
        ErrorCode::ConnectionLost
    );
    assert!(reader.read(&mut [0; 10]).is_err());
    assert_eq!(
        server.state.lock().unwrap().opens,
        1,
        "kein OPEN-Retry innerhalb des fehlgeschlagenen Versuchs"
    );
    drop(reader);
    server.state.lock().unwrap().fault = Fault::None;
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let (fresh, _) = endpoint
        .interactive_file_read_context(&[PathBuf::from("/pane/inhalt.txt")], &Cancel::new())
        .unwrap();
    assert_eq!(provider.session_generation(), 2);
    assert!(old.open_read(Path::new("/pane/inhalt.txt")).is_err());
    let mut data = Vec::new();
    fresh
        .open_read(Path::new("/pane/inhalt.txt"))
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();
    assert_eq!(data, b"Daten");
    assert_eq!(provider.session_generation(), 2);
    assert!(!provider.capabilities().complete_listing);
}

#[test]
fn inhaltsdrift_erhaelt_sekundaeren_closeverlust_auch_bei_gehaltenem_leser() {
    let server = Server::new(b"Daten".to_vec(), Fault::ContentDrift);
    server.state.lock().unwrap().lose_readback_close = true;
    let (provider, old) = server.context(&Cancel::new());
    let mut reader = old.open_read(Path::new("/pane/inhalt.txt")).unwrap();
    let mut bytes = Vec::new();
    assert_eq!(
        error_code(&reader.read_to_end(&mut bytes).unwrap_err()),
        ErrorCode::PlanDrift
    );
    assert_eq!(bytes, b"Daten");
    {
        let mut state = server.state.lock().unwrap();
        assert_eq!((state.opens, state.closes), (2, 1));
        state.fault = Fault::None;
        state.lose_readback_close = false;
    }
    // Der fehlgeschlagene Leser bleibt gehalten. Sein späterer Drop darf nicht
    // nötig sein, damit der bereits gemeldete Closeverlust die Sitzung entwertet.
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let (fresh, _) = endpoint
        .interactive_file_read_context(&[PathBuf::from("/pane/inhalt.txt")], &Cancel::new())
        .unwrap();
    assert_eq!(provider.session_generation(), 2);
    let mut fresh_bytes = Vec::new();
    fresh
        .open_read(Path::new("/pane/inhalt.txt"))
        .unwrap()
        .read_to_end(&mut fresh_bytes)
        .unwrap();
    assert_eq!(fresh_bytes, b"Daten");
    assert!(reader.read(&mut [0; 1]).is_err());
    assert_eq!(server.state.lock().unwrap().mutations, 0);
}

#[test]
fn konfliktvergleich_und_beide_behalten_binden_sftp_quellinhalt_ohne_striktes_elternlisting() {
    let server = Server::new(b"neuer Inhalt".to_vec(), Fault::None);
    let (_provider, source) = server.context(&Cancel::new());
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = root.join("ziel");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("inhalt.txt"), b"alte Zielversion").unwrap();
    let destination = LocalVfs::rooted(&target).unwrap();
    let items = [PathBuf::from("/pane/inhalt.txt")];
    let conflicts =
        fileops::find_recursive_conflicts(source.as_ref(), &destination, &target, &items).unwrap();
    let views =
        fileops::file_conflict_views(source.as_ref(), &destination, &target, &items, &conflicts)
            .unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(
        views[0].source_proof.content_hash,
        Some(*blake3::hash(b"neuer Inhalt").as_bytes())
    );
    assert!(
        fileops::capture_file_conflict_proofs(source.as_ref(), &["/pane/inhalt.txt".into()])
            .is_err(),
        "strikte Zielbindung bleibt gesperrt"
    );
    let aliases = fileops::prepare_keep_both_targets(
        source.as_ref(),
        &destination,
        &target,
        &items,
        &conflicts,
    )
    .unwrap();
    assert_eq!(aliases.len(), 1);
    let alias = PathBuf::from(&aliases[0].target);
    let config = private_config(&root);
    let queue = OperationQueue::open(config.join("queue.json")).unwrap();
    let mut operation = operation(&server.config, &target);
    if let QueueOperation::ProviderFile {
        keep_both_targets,
        expected_conflicts,
        ..
    } = &mut operation
    {
        *keep_both_targets = aliases;
        *expected_conflicts = conflicts;
    }
    queue.enqueue(operation).unwrap();
    let result = queue::execute_provider_file_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        &execution(&root),
        Arc::new(FakeSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(
        matches!(result, QueueExecutionResult::File(_)),
        "{result:?}"
    );
    assert_eq!(
        std::fs::read(target.join("inhalt.txt")).unwrap(),
        b"alte Zielversion"
    );
    assert_eq!(std::fs::read(alias).unwrap(), b"neuer Inhalt");
}

#[test]
fn abbruch_waehrend_elternpruefung_stoppt_vor_dem_naechsten_metadatenrequest() {
    let server = Server::new(b"Inhalt".to_vec(), Fault::None);
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap();
    let cancel = Cancel::new();
    let before = {
        let mut state = server.state.lock().unwrap();
        state.cancel_on_metadata = Some(cancel.clone());
        state.metadata_requests
    };
    let error = match endpoint
        .interactive_file_read_context(&[PathBuf::from("/pane/inhalt.txt")], &cancel)
    {
        Ok(_) => panic!("Abgebrochene Elternprüfung lieferte eine Inhaltsfreigabe"),
        Err(error) => error,
    };
    assert_eq!(error_code(&error), ErrorCode::Interrupted);
    let (requests, opens) = {
        let state = server.state.lock().unwrap();
        (state.metadata_requests - before, state.opens)
    };
    assert_eq!(requests, 1);
    assert_eq!(opens, 0);
}

#[test]
fn verlorene_metadatenvorbereitung_verbindet_erst_beim_neuen_leseversuch_frisch() {
    let server = Server::new(b"Inhalt".to_vec(), Fault::None);
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    server.state.lock().unwrap().fault = Fault::MetadataLost;
    let error = match endpoint
        .interactive_file_read_context(&[PathBuf::from("/pane/inhalt.txt")], &Cancel::new())
    {
        Ok(_) => panic!("Verlorene Metadatenantwort lieferte eine Lesesicht"),
        Err(error) => error,
    };
    assert_eq!(error_code(&error), ErrorCode::ConnectionLost);
    assert_eq!(provider.session_generation(), 1);
    assert_eq!(server.state.lock().unwrap().opens, 0);
    server.state.lock().unwrap().fault = Fault::None;
    let (fresh, _) = endpoint
        .interactive_file_read_context(&[PathBuf::from("/pane/inhalt.txt")], &Cancel::new())
        .unwrap();
    assert_eq!(provider.session_generation(), 2);
    let mut data = Vec::new();
    fresh
        .open_read(Path::new("/pane/inhalt.txt"))
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();
    assert_eq!(data, b"Inhalt");
}

#[test]
fn verschwundene_lokale_quellversion_bleibt_bei_der_neuen_quellbindung_drift() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let path = root.join("quelle.txt");
    std::fs::write(&path, b"gebundene Version").unwrap();
    let vfs = LocalVfs::rooted(&root).unwrap();
    let proofs =
        fileops::capture_file_source_proofs(&vfs, &[path.to_str().unwrap().into()]).unwrap();
    std::fs::remove_file(path).unwrap();
    let error = fileops::verify_file_source_proofs(&vfs, &proofs).unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::PlanDrift);
}

#[test]
fn interaktive_sftp_ordnergroesse_nutzt_den_begrenzten_lesebaum_statt_striktem_sync_vfs() {
    let server = Server::tree();
    let provider = server.connect();
    let mut results = Vec::new();
    let stats = provider
        .measure_interactive_directory_sizes(
            Path::new("/pane"),
            &["Baum".into()],
            &Cancel::new(),
            &mut |r| results.push(r.clone()),
        )
        .unwrap();
    let result = &results[0];
    assert_eq!(
        result.status,
        doppeldecker_core::DirectorySizeStatus::Observed
    );
    assert_eq!(
        (
            result.bytes,
            result.files,
            result.directories,
            result.skipped
        ),
        (11, 1, 1, 0)
    );
    assert_eq!(stats.observed, 1);
    assert!(!result.cached && !result.limitations.is_empty());
    assert!(provider.list_complete(Path::new("/pane/Baum")).is_err());
    let state = server.state.lock().unwrap();
    assert_eq!(state.opens, 0);
    assert_eq!(state.mutations, 0);
}

fn measured_sizes(
    provider: &Arc<SftpVfs>,
    names: &[&str],
    cancel: &Cancel,
) -> Vec<doppeldecker_core::DirectorySizeResult> {
    let mut results = Vec::new();
    provider
        .measure_interactive_directory_sizes(
            Path::new("/pane"),
            &names.iter().map(|v| (*v).into()).collect::<Vec<_>>(),
            cancel,
            &mut |r| results.push(r.clone()),
        )
        .unwrap();
    results
}

#[test]
fn sftp_ordnergroesse_bilanziert_auslassungen_und_liest_jedes_mal_frisch_ohne_open() {
    use doppeldecker_core::DirectorySizeStatus;
    let server = Server::tree();
    tree_file(
        &server,
        "/vault/pane/Baum/Grüße %2F.txt",
        b"Test\n".to_vec(),
    );
    {
        let mut state = server.state.lock().unwrap();
        state.page_size = 1;
        let regular = attrs(&state, true);
        for (name, mode, size) in [
            ("Link", 0o120777, Some(11)),
            ("Pipe", 0o010600, Some(0)),
            ("Unklar", 0o100600, None),
        ] {
            let mut a = regular.clone();
            a.permissions = Some(mode);
            a.size = size;
            state
                .tree
                .as_mut()
                .unwrap()
                .insert(format!("/vault/pane/Baum/{name}"), (a, Vec::new()));
        }
    }
    let provider = server.connect();
    let first = measured_sizes(&provider, &["Baum"], &Cancel::new());
    assert_eq!(first[0].status, DirectorySizeStatus::Partial);
    assert_eq!(
        (
            first[0].bytes,
            first[0].files,
            first[0].directories,
            first[0].skipped
        ),
        (16, 2, 1, 3)
    );
    assert_eq!(first[0].issues.len(), 3);
    assert!(first[0]
        .issues
        .iter()
        .all(|i| i.path.starts_with("/pane/Baum") && !i.message.is_empty()));
    tree_file(&server, "/vault/pane/Baum/Neu.txt", b"Frisch\n".to_vec());
    let before = server.state.lock().unwrap().metadata_requests;
    let second = measured_sizes(&provider, &["Baum"], &Cancel::new());
    assert_eq!(second[0].bytes, 23);
    assert!(!second[0].cached);
    let state = server.state.lock().unwrap();
    assert!(state.metadata_requests > before);
    assert_eq!((state.opens, state.mutations), (0, 0));
}

#[test]
fn sftp_ordnergroesse_isoliert_ordnerfehler_und_prueft_eingaben_vor_metadaten() {
    use doppeldecker_core::DirectorySizeStatus;
    let server = Server::tree();
    let provider = server.connect();
    let result = measured_sizes(&provider, &["Fehlt", "Baum"], &Cancel::new());
    assert_eq!(result[0].status, DirectorySizeStatus::Failed);
    assert_eq!(result[1].status, DirectorySizeStatus::Observed);
    let before = server.state.lock().unwrap().metadata_requests;
    for names in [
        vec!["../Baum".into()],
        vec!["a/b".into()],
        vec!["".into()],
        vec!["Baum".into(), "Baum".into()],
        (0..101).map(|i| format!("Ordner{i}")).collect(),
    ] {
        assert!(provider
            .measure_interactive_directory_sizes(
                Path::new("/pane"),
                &names,
                &Cancel::new(),
                &mut |_| panic!("Ungültige Auswahl darf kein Ergebnis liefern")
            )
            .is_err());
        assert_eq!(server.state.lock().unwrap().metadata_requests, before);
    }
    assert!(!provider.capabilities().complete_listing);
    assert!(provider.root_identity(Path::new("/pane")).is_err());
    let state = server.state.lock().unwrap();
    assert_eq!((state.opens, state.mutations), (0, 0));
}

#[test]
fn sftp_ordnergroesse_beachtet_abbruch_und_sitzungswechsel_ohne_inhaltszugriff() {
    use doppeldecker_core::DirectorySizeStatus;
    let server = Server::tree();
    let provider = server.connect();
    let cancel = Cancel::new();
    server.state.lock().unwrap().cancel_on_readdir = Some(cancel.clone());
    let result = measured_sizes(&provider, &["Baum", "Fehlt"], &cancel);
    assert!(result
        .iter()
        .all(|r| r.status == DirectorySizeStatus::Cancelled));
    server.state.lock().unwrap().cancel_on_readdir = None;
    let mut result = Vec::new();
    provider
        .measure_interactive_directory_sizes(
            Path::new("/pane"),
            &["Baum".into(), "Fehlt".into()],
            &Cancel::new(),
            &mut |r| {
                result.push(r.clone());
                if result.len() == 1 {
                    provider.reconnect().unwrap();
                }
            },
        )
        .unwrap();
    assert_eq!(result[0].status, DirectorySizeStatus::Observed);
    assert_eq!(result[1].status, DirectorySizeStatus::Failed);
    assert_eq!(result[1].issues[0].code, ErrorCode::PlanDrift);
    let state = server.state.lock().unwrap();
    assert_eq!((state.opens, state.mutations), (0, 0));
}

#[test]
fn sftp_ordnergroesse_zaehlt_grosse_dateien_ohne_inhaltsfreigabe_oder_zeitpflicht() {
    use doppeldecker_core::DirectorySizeStatus;
    let server = Server::tree();
    let eight = 8 * 1024 * 1024 * 1024u64;
    {
        let mut state = server.state.lock().unwrap();
        let mut large = attrs(&state, true);
        large.size = Some(eight);
        state.tree.as_mut().unwrap().insert(
            "/vault/pane/Baum/Gross1".into(),
            (large.clone(), Vec::new()),
        );
        state
            .tree
            .as_mut()
            .unwrap()
            .insert("/vault/pane/Baum/Gross2".into(), (large, Vec::new()));
    }
    let provider = server.connect();
    let result = measured_sizes(&provider, &["Baum"], &Cancel::new());
    assert_eq!(result[0].status, DirectorySizeStatus::Observed);
    assert_eq!(result[0].bytes, 2 * eight + 11);
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    assert!(
        endpoint
            .interactive_file_read_context(&[PathBuf::from("/pane/Baum")], &Cancel::new())
            .is_err(),
        "Die Grenze des Inhaltsdownloads bleibt bei 16 GiB"
    );
    {
        let mut state = server.state.lock().unwrap();
        let mut large = attrs(&state, true);
        large.size = Some(eight + 1);
        let tree = state.tree.as_mut().unwrap();
        tree.remove("/vault/pane/Baum/Gross2");
        tree.insert("/vault/pane/Baum/Gross1".into(), (large, Vec::new()));
    }
    let result = measured_sizes(&provider, &["Baum"], &Cancel::new());
    assert_eq!(result[0].status, DirectorySizeStatus::Observed);
    assert_eq!(result[0].bytes, eight + 12);
    assert!(
        endpoint
            .interactive_file_read_context(&[PathBuf::from("/pane/Baum")], &Cancel::new())
            .is_err(),
        "Die Einzeldateigrenze des Inhaltsdownloads bleibt bei 8 GiB"
    );
    {
        let mut state = server.state.lock().unwrap();
        let attributes = &mut state
            .tree
            .as_mut()
            .unwrap()
            .get_mut("/vault/pane/Baum/Gross1")
            .unwrap()
            .0;
        attributes.size = Some(7);
        // SFTP v3 überträgt beide Zeiten unter demselben Attributflag.
        attributes.mtime = None;
        attributes.atime = None;
    }
    let result = measured_sizes(&provider, &["Baum"], &Cancel::new());
    assert_eq!(result[0].status, DirectorySizeStatus::Observed);
    assert_eq!(result[0].bytes, 18);
    let (contents, _) = endpoint
        .interactive_file_read_context(&[PathBuf::from("/pane/Baum")], &Cancel::new())
        .unwrap();
    let tree = contents.interactive_source_tree().unwrap().unwrap();
    assert!(tree
        .omitted
        .iter()
        .any(|item| item.path == "/pane/Baum/Gross1"));
    assert!(contents.open_read(Path::new("/pane/Baum/Gross1")).is_err());
    let state = server.state.lock().unwrap();
    assert_eq!((state.opens, state.mutations), (0, 0));
}

#[test]
fn sftp_ordnergroesse_begrenzt_tiefe_und_rechenueberlauf_pro_ordner() {
    use doppeldecker_core::DirectorySizeStatus;
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        let directory = attrs(&state, false);
        let mut path = "/vault/pane/Tief".to_string();
        for _ in 0..19 {
            state
                .tree
                .as_mut()
                .unwrap()
                .insert(path.clone(), (directory.clone(), Vec::new()));
            path.push_str("/Unter");
        }
        let mut large = attrs(&state, true);
        large.size = Some(u64::MAX);
        state
            .tree
            .as_mut()
            .unwrap()
            .insert("/vault/pane/Baum/ZuGross".into(), (large, Vec::new()));
    }
    let provider = server.connect();
    let result = measured_sizes(&provider, &["Tief", "Baum"], &Cancel::new());
    assert!(result
        .iter()
        .all(|r| r.status == DirectorySizeStatus::Failed && !r.issues.is_empty()));
    assert!(result[0].issues[0].message.contains("16 Unterordnerebenen"));
    assert!(result[1].issues[0].message.contains("nicht darstellbar"));
    let state = server.state.lock().unwrap();
    assert_eq!((state.opens, state.mutations), (0, 0));
}

#[test]
fn sftp_ordnergroesse_zaehlt_case_und_unicodevarianten_ohne_lokale_zielannahme() {
    use doppeldecker_core::DirectorySizeStatus;
    let server = Server::tree();
    for (name, bytes) in [
        ("Foo", b"AA".to_vec()),
        ("foo", b"BBB".to_vec()),
        ("é.txt", b"C".to_vec()),
        ("e\u{301}.txt", b"D".to_vec()),
    ] {
        tree_file(&server, &format!("/vault/pane/Baum/{name}"), bytes);
    }
    let provider = server.connect();
    let result = measured_sizes(&provider, &["Baum"], &Cancel::new());
    assert_eq!(result[0].status, DirectorySizeStatus::Observed);
    assert_eq!((result[0].bytes, result[0].files), (18, 5));
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    assert!(
        endpoint
            .interactive_file_read_context(&[PathBuf::from("/pane/Baum")], &Cancel::new())
            .is_err(),
        "Inhaltsdownloads behalten die konservative Zielnamensprüfung"
    );
    let empty = Vec::<String>::new();
    assert!(provider
        .measure_interactive_directory_sizes(
            Path::new("/pane"),
            &empty,
            &Cancel::new(),
            &mut |_| {}
        )
        .is_err());
    let mut result = Vec::new();
    provider
        .measure_interactive_directory_sizes(
            Path::new("/pane/Baum"),
            &["Leer".into()],
            &Cancel::new(),
            &mut |r| result.push(r.clone()),
        )
        .unwrap();
    assert_eq!(
        (result[0].bytes, result[0].files, result[0].directories),
        (0, 0, 0)
    );
    assert_eq!(result[0].status, DirectorySizeStatus::Observed);
    let state = server.state.lock().unwrap();
    assert_eq!((state.opens, state.mutations), (0, 0));
}
