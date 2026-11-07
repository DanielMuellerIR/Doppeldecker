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
    ForwardMidWrite,
    ForwardMidWriteHold,
    ForwardTempStatDenied,
}
struct State {
    data: Vec<u8>,
    fault: Fault,
    read_started: bool,
    opens: usize,
    bytes_read: u64,
    mutations: usize,
    allow_writes: bool,
    lose_mutation_response: bool,
    rename_parent_mtime: bool,
    rename_parent_uid: bool,
    batch_spelling_fault: &'static str,
    batch_spelling_listings: usize,
    closes: usize,
    lose_readback_close: bool,
    metadata_requests: usize,
    last_lstat: HashMap<String, usize>,
    setstat_observations: Vec<(String, bool, usize)>,
    swap_on_lstat: Option<(usize, u32)>,
    opendir_counts: HashMap<String, usize>,
    omission_drift_after_read_close: Option<String>,
    cancel_on_metadata: Option<Cancel>,
    cancel_on_readdir: Option<Cancel>,
    cancel_on_write: Option<Cancel>,
    tree: Option<HashMap<String, (FileAttributes, Vec<u8>)>>,
    page_size: usize,
    listing_fail_after: Option<usize>,
    mtime: u32,
    principal_version: Option<&'static str>,
    principal_fault: &'static str,
    principal_requests: Vec<Vec<u8>>,
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
            bytes_read: 0,
            mutations: 0,
            allow_writes: false,
            lose_mutation_response: false,
            rename_parent_mtime: false,
            rename_parent_uid: false,
            batch_spelling_fault: "",
            batch_spelling_listings: 0,
            closes: 0,
            lose_readback_close: false,
            metadata_requests: 0,
            last_lstat: HashMap::new(),
            setstat_observations: Vec::new(),
            swap_on_lstat: None,
            opendir_counts: HashMap::new(),
            omission_drift_after_read_close: None,
            cancel_on_metadata: None,
            cancel_on_readdir: None,
            cancel_on_write: None,
            tree: None,
            page_size: usize::MAX,
            listing_fail_after: None,
            mtime: 123456,
            principal_version: None,
            principal_fault: "",
            principal_requests: Vec::new(),
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
fn sftp_anzeige_loest_servernamen_nur_bei_genau_verhandelter_erweiterung_auf() {
    for version in [None, Some("1"), Some("2")] {
        let server = Server::tree();
        server.state.lock().unwrap().principal_version = version;
        let provider = server.connect();
        let before = provider.capabilities();
        let listing = provider
            .list_interactive_best_effort(Path::new("/pane"))
            .unwrap();
        let entry = listing
            .entries
            .iter()
            .find(|row| row.entry.name == "Baum")
            .unwrap();
        assert_eq!(
            entry.entry.owner.as_deref(),
            Some(if version == Some("1") {
                "server-user-1000"
            } else {
                "1000"
            })
        );
        assert_eq!(
            entry.entry.group.as_deref(),
            Some(if version == Some("1") {
                "server-group-1000"
            } else {
                "1000"
            })
        );
        assert_eq!(before, provider.capabilities());
        let state = server.state.lock().unwrap();
        assert_eq!(
            state.principal_requests.len(),
            usize::from(version == Some("1"))
        );
        assert_eq!(state.mutations, 0);
    }
}
#[test]
fn sftp_optionaler_namensfehler_erhaelt_dateiliste_und_wird_nicht_blind_wiederholt() {
    for fault in ["denied", "malformed"] {
        let server = Server::tree();
        {
            let mut state = server.state.lock().unwrap();
            state.principal_version = Some("1");
            state.principal_fault = fault;
        }
        let provider = server.connect();
        for _ in 0..2 {
            let listing = provider
                .list_interactive_best_effort(Path::new("/pane"))
                .unwrap();
            let entry = listing
                .entries
                .iter()
                .find(|row| row.entry.name == "Baum")
                .unwrap();
            assert_eq!(entry.entry.owner.as_deref(), Some("1000"));
        }
        assert_eq!(server.state.lock().unwrap().principal_requests.len(), 1);
    }
}
#[test]
fn sftp_verlorene_namensantwort_erhaelt_listing_und_erneuert_den_naechsten_leser() {
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        state.principal_version = Some("1");
        state.principal_fault = "lost";
    }
    let provider = server.connect();
    let first = provider
        .list_interactive_best_effort(Path::new("/pane"))
        .unwrap();
    server.state.lock().unwrap().principal_fault = "";
    let second = provider
        .list_interactive_best_effort(Path::new("/pane"))
        .unwrap();
    assert!(second.session_generation > first.session_generation);
    assert_eq!(
        second
            .entries
            .iter()
            .find(|row| row.entry.name == "Baum")
            .unwrap()
            .entry
            .owner
            .as_deref(),
        Some("server-user-1000")
    );
    assert_eq!(server.state.lock().unwrap().principal_requests.len(), 2);
}
#[test]
fn sftp_namensabfrage_bleibt_begrenzt_und_unbekannte_ids_bleiben_zahlen() {
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        state.principal_version = Some("1");
        let mut unknown = attrs(&state, true);
        unknown.uid = Some(9999);
        state
            .tree
            .as_mut()
            .unwrap()
            .insert("/vault/pane/Unbekannt".into(), (unknown, Vec::new()));
    }
    let provider = server.connect();
    let listing = provider
        .list_interactive_best_effort(Path::new("/pane"))
        .unwrap();
    assert_eq!(
        listing
            .entries
            .iter()
            .find(|row| row.entry.name == "Unbekannt")
            .unwrap()
            .entry
            .owner
            .as_deref(),
        Some("9999")
    );
    {
        let mut state = server.state.lock().unwrap();
        for id in 0..40 {
            let mut attributes = attrs(&state, true);
            attributes.uid = Some(id);
            state.tree.as_mut().unwrap().insert(
                format!("/vault/pane/Benutzer-{id}"),
                (attributes, Vec::new()),
            );
        }
    }
    let listing = provider
        .list_interactive_best_effort(Path::new("/pane"))
        .unwrap();
    assert_eq!(
        listing
            .entries
            .iter()
            .find(|row| row.entry.name == "Benutzer-31")
            .unwrap()
            .entry
            .owner
            .as_deref(),
        Some("server-user-31")
    );
    assert_eq!(
        listing
            .entries
            .iter()
            .find(|row| row.entry.name == "Benutzer-32")
            .unwrap()
            .entry
            .owner
            .as_deref(),
        Some("32")
    );
    let state = server.state.lock().unwrap();
    assert_eq!(state.principal_requests.len(), 2);
    assert!(state
        .principal_requests
        .iter()
        .all(|request| request.len() <= 264));
    assert_eq!(state.mutations, 0);
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
        let state = self.state.lock().unwrap();
        if !state.allow_writes {
            assert_eq!(state.mutations, 0, "Inhaltsklasse sendete eine Mutation");
        }
        drop(state);
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

fn deep_content_pane(server: &Server) -> PathBuf {
    let mut state = server.state.lock().unwrap();
    let directory = attrs(&state, false);
    let mut pane = PathBuf::from("/pane");
    for _ in 1..64 {
        pane.push("d");
        state.tree.as_mut().unwrap().insert(
            format!("/vault{}", pane.display()),
            (directory.clone(), Vec::new()),
        );
    }
    pane
}

#[test]
fn sftp_inhalte_lesen_direktes_kind_eines_panes_mit_64_komponenten() {
    let server = Server::tree();
    let pane = deep_content_pane(&server);
    let path = pane.join("Datei.txt");
    let bytes = b"Tiefer direkter Inhalt\n";
    tree_file(
        &server,
        &format!("/vault{}", path.display()),
        bytes.to_vec(),
    );
    let endpoint = ProviderEndpoint::sftp(server.connect(), &pane).unwrap();
    let listing = endpoint.list_interactive().unwrap();
    assert!(listing.omitted.is_empty());
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].entry.name, "Datei.txt");
    assert_eq!(server.state.lock().unwrap().opens, 0);
    let cancel = Cancel::new();
    let prepared = endpoint.interactive_file_read_context(std::slice::from_ref(&path), &cancel);
    if prepared.is_err() {
        assert_eq!(server.state.lock().unwrap().opens, 0);
    }
    let (source, _) = prepared.expect("gelistetes direktes Kind muss als Inhalt lesbar sein");
    let cache = tempfile::tempdir().unwrap();
    let snapshot = download_remote_snapshot(source.as_ref(), &path, cache.path(), &cancel).unwrap();
    assert_eq!(std::fs::read(snapshot.path).unwrap(), bytes);
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = LocalVfs::rooted(&root).unwrap();
    let stats = copy_items(
        FileOperationRequest {
            src: source.as_ref(),
            dst: &target,
            items: &[path],
            dst_dir: &root,
            policy: OverwritePolicy::Skip,
            sandbox: &None,
        },
        &cancel,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((stats.files, stats.bytes), (1, bytes.len() as u64));
    assert_eq!(std::fs::read(root.join("Datei.txt")).unwrap(), bytes);
    assert_eq!(server.state.lock().unwrap().mutations, 0);
}

#[test]
fn sftp_inhalte_kopieren_ausgewaehlten_baum_jenseits_64_komponenten() {
    let server = Server::tree();
    let pane = deep_content_pane(&server);
    let selected = pane.join("Baum");
    {
        let mut state = server.state.lock().unwrap();
        let directory = attrs(&state, false);
        for path in [&selected, &selected.join("Unter")] {
            state.tree.as_mut().unwrap().insert(
                format!("/vault{}", path.display()),
                (directory.clone(), Vec::new()),
            );
        }
    }
    let path = selected.join("Unter/Kind.txt");
    tree_file(
        &server,
        &format!("/vault{}", path.display()),
        b"rekursiv".to_vec(),
    );
    let endpoint = ProviderEndpoint::sftp(server.connect(), &pane).unwrap();
    let cancel = Cancel::new();
    let (source, _) = endpoint
        .interactive_file_read_context(std::slice::from_ref(&selected), &cancel)
        .unwrap();
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = LocalVfs::rooted(&root).unwrap();
    let stats = copy_items(
        FileOperationRequest {
            src: source.as_ref(),
            dst: &target,
            items: &[selected],
            dst_dir: &root,
            policy: OverwritePolicy::Skip,
            sandbox: &None,
        },
        &cancel,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((stats.files, stats.bytes), (1, 8));
    assert_eq!(
        std::fs::read(root.join("Baum/Unter/Kind.txt")).unwrap(),
        b"rekursiv"
    );
    assert_eq!(server.state.lock().unwrap().mutations, 0);
}

#[test]
fn sftp_inhalte_tiefer_pane_erhaelt_elternbeleg_und_symlinksperre_vor_open() {
    for symlink in [false, true] {
        let server = Server::tree();
        let pane = deep_content_pane(&server);
        let path = pane.join("Datei.txt");
        tree_file(
            &server,
            &format!("/vault{}", path.display()),
            b"privat".to_vec(),
        );
        let endpoint = ProviderEndpoint::sftp(server.connect(), &pane).unwrap();
        let cancel = Cancel::new();
        let (source, _) = endpoint
            .interactive_file_read_context(std::slice::from_ref(&path), &cancel)
            .unwrap();
        {
            let mut state = server.state.lock().unwrap();
            let parent = &mut state
                .tree
                .as_mut()
                .unwrap()
                .get_mut(&format!("/vault{}", pane.display()))
                .unwrap()
                .0;
            if symlink {
                parent.permissions = Some(0o120777);
            } else {
                parent.uid = Some(2000);
            }
        }
        assert!(source.open_read(&path).is_err(), "symlink={symlink}");
        let state = server.state.lock().unwrap();
        assert_eq!((state.opens, state.bytes_read, state.mutations), (0, 0, 0));
    }
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
fn sftp_baum_aliasgruppen_lassen_unabhaengige_dateien_uebertragbar() {
    for (left, right) in [("Name", "name"), ("é.txt", "e\u{301}.txt")] {
        let server = Server::tree();
        tree_file(
            &server,
            &format!("/vault/pane/Baum/{left}"),
            b"Links".to_vec(),
        );
        tree_file(
            &server,
            &format!("/vault/pane/Baum/{right}"),
            b"Rechts".to_vec(),
        );
        server.state.lock().unwrap().page_size = 1;
        let (_provider, source) = tree_context(&server, &Cancel::new());
        let summary = source.interactive_source_tree().unwrap().unwrap();
        assert!(source.list_complete(Path::new("/pane/Baum")).is_err());
        assert_eq!(
            (
                summary.entries,
                summary.files,
                summary.directories,
                summary.omitted.len()
            ),
            (5, 1, 2, 2)
        );
        for name in [left, right] {
            assert!(summary
                .omitted
                .iter()
                .any(|item| item.path == format!("/pane/Baum/{name}")
                    && item.reason.contains("Namensgruppe")));
        }
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let target = LocalVfs::rooted(&root).unwrap();
        let stats = copy_items(
            FileOperationRequest {
                src: source.as_ref(),
                dst: &target,
                items: &[PathBuf::from("/pane/Baum")],
                dst_dir: &root,
                policy: OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!((stats.files, stats.skipped), (1, 2));
        assert_eq!(stats.source_tree, Some(summary));
        assert_eq!(
            std::fs::read(root.join("Baum/Kind.txt")).unwrap(),
            b"Bauminhalt\n"
        );
        assert!(root.join("Baum/Leer").is_dir());
        for name in [left, right] {
            assert!(!root.join("Baum").join(name).exists());
        }
        let state = server.state.lock().unwrap();
        assert_eq!(state.opens, 1);
        assert_eq!(state.mutations, 0);
        assert_eq!(
            state.tree.as_ref().unwrap()[&format!("/vault/pane/Baum/{left}")].1,
            b"Links"
        );
        assert_eq!(
            state.tree.as_ref().unwrap()[&format!("/vault/pane/Baum/{right}")].1,
            b"Rechts"
        );
    }
}

fn recursive_omission_fixture(left: &str, right: &str) -> (Server, Arc<dyn Vfs>) {
    let server = Server::tree();
    for (name, bytes) in [
        (left, b"omit-left".to_vec()),
        (right, b"omit-right".to_vec()),
        ("A-good1", b"first".to_vec()),
        ("B-good2", b"second".to_vec()),
    ] {
        tree_file(&server, &format!("/vault/pane/Baum/{name}"), bytes);
    }
    let (_, source) = tree_context(&server, &Cancel::new());
    (server, source)
}

fn change_recursive_omission(server: &Server, omitted: &str) {
    let mut state = server.state.lock().unwrap();
    let tree = state.tree.as_mut().unwrap();
    let before = [
        "/vault",
        "/vault/pane",
        "/vault/pane/Baum",
        "/vault/pane/Baum/A-good1",
        "/vault/pane/Baum/B-good2",
    ]
    .map(|path| {
        let a = &tree[path].0;
        (path, (a.size, a.mtime, a.permissions, a.uid, a.gid))
    });
    tree.get_mut(omitted).unwrap().0.uid = Some(2000);
    for (path, expected) in before {
        let a = &tree[path].0;
        assert_eq!((a.size, a.mtime, a.permissions, a.uid, a.gid), expected);
    }
}

#[test]
fn sftp_rekursive_omission_stoppt_nach_letzter_navigation_vor_naechster_datei() {
    for (left, right) in [("Name", "name"), ("é.txt", "e\u{301}.txt")] {
        let (server, source) = recursive_omission_fixture(left, right);
        let omitted = format!("/vault/pane/Baum/{left}");
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let target = LocalVfs::rooted(&root).unwrap();
        std::fs::create_dir(root.join("Baum")).unwrap();
        std::fs::write(root.join("Baum/Sentinel"), b"existing").unwrap();
        let mut changed = false;
        let error = copy_items(
            FileOperationRequest {
                src: source.as_ref(),
                dst: &target,
                items: &[PathBuf::from("/pane/Baum")],
                dst_dir: &root,
                policy: OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |progress| {
                if !changed && progress.files_done == 1 {
                    change_recursive_omission(&server, &omitted);
                    changed = true;
                }
            },
        )
        .unwrap_err();
        assert!(changed);
        assert_eq!(error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(std::fs::read(root.join("Baum/A-good1")).unwrap(), b"first");
        assert!(
            !root.join("Baum/B-good2").exists(),
            "Kein weiterer Commit nach geändertem rekursivem Auslassungsbeleg"
        );
        assert_eq!(
            std::fs::read(root.join("Baum/Sentinel")).unwrap(),
            b"existing"
        );
        let state = server.state.lock().unwrap();
        assert_eq!((state.opens, state.mutations), (1, 0));
    }
}

#[test]
fn sftp_rekursive_omission_stoppt_am_eof_und_nach_eof_vor_commit() {
    for after_eof in [false, true] {
        let (server, source) = recursive_omission_fixture("Name", "name");
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let target = LocalVfs::rooted(&root).unwrap();
        std::fs::create_dir(root.join("Baum")).unwrap();
        std::fs::write(root.join("Baum/Sentinel"), b"existing").unwrap();
        if after_eof {
            // Der CLOSE folgt dem geprüften EOF. Die Antwort ändert nur den
            // ausgelassenen Nachbarn, bevor copy_file_stream den Commit vorbereitet.
            server.state.lock().unwrap().omission_drift_after_read_close =
                Some("/vault/pane/Baum/Name".into());
        }
        let mut changed_during_read = false;
        let error = copy_items(
            FileOperationRequest {
                src: source.as_ref(),
                dst: &target,
                items: &[PathBuf::from("/pane/Baum")],
                dst_dir: &root,
                policy: OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |progress| {
                if !after_eof && !changed_during_read && progress.bytes_done > 0 {
                    change_recursive_omission(&server, "/vault/pane/Baum/Name");
                    changed_during_read = true;
                }
            },
        )
        .unwrap_err();
        assert_eq!(
            error_code(&error),
            ErrorCode::PlanDrift,
            "after_eof={after_eof}: {error}"
        );
        assert_eq!(changed_during_read, !after_eof);
        assert!(
            !root.join("Baum/A-good1").exists(),
            "Kein Commit mit verändertem rekursivem Auslassungsbeleg, after_eof={after_eof}"
        );
        assert!(!root.join("Baum/B-good2").exists());
        assert_eq!(
            std::fs::read(root.join("Baum/Sentinel")).unwrap(),
            b"existing"
        );
        let state = server.state.lock().unwrap();
        assert!(state.omission_drift_after_read_close.is_none());
        assert_eq!(
            state.tree.as_ref().unwrap()["/vault/pane/Baum/Name"].0.uid,
            Some(2000)
        );
        assert_eq!((state.opens, state.mutations), (1, 0));
    }
}

#[test]
fn sftp_rekursive_omission_eltern_sind_global_dedupliziert_und_ohne_auslassung_zugriffsfrei() {
    for omissions in [false, true] {
        let server = Server::tree();
        {
            let mut state = server.state.lock().unwrap();
            let directory = attrs(&state, false);
            for path in [
                "/vault/pane/Baum/Tief",
                "/vault/pane/Baum/Tief/Noch",
                "/vault/pane/Zweiter",
            ] {
                state
                    .tree
                    .as_mut()
                    .unwrap()
                    .insert(path.into(), (directory.clone(), Vec::new()));
            }
        }
        for parent in ["Baum", "Baum/Tief/Noch", "Zweiter"] {
            tree_file(
                &server,
                &format!("/vault/pane/{parent}/good"),
                b"content".to_vec(),
            );
            if omissions {
                for name in ["Name", "name"] {
                    tree_file(
                        &server,
                        &format!("/vault/pane/{parent}/{name}"),
                        b"omit".to_vec(),
                    );
                }
            }
        }
        let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
        let (source, _) = endpoint
            .interactive_file_read_context(
                &[PathBuf::from("/pane/Baum"), PathBuf::from("/pane/Zweiter")],
                &Cancel::new(),
            )
            .unwrap();
        server.state.lock().unwrap().opendir_counts.clear();
        let mut bytes = Vec::new();
        source
            .open_read(Path::new("/pane/Baum/good"))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        source
            .verify_transferred_source(Path::new("/pane/Baum/good"), &blake3::hash(&bytes))
            .unwrap();
        let state = server.state.lock().unwrap();
        let expected = if omissions { 4 } else { 0 };
        for parent in [
            "/vault/pane/Baum",
            "/vault/pane/Baum/Tief/Noch",
            "/vault/pane/Zweiter",
        ] {
            assert_eq!(
                state.opendir_counts.get(parent).copied().unwrap_or(0),
                expected,
                "{parent}, omissions={omissions}"
            );
        }
        assert_eq!(state.opendir_counts.len(), if omissions { 3 } else { 0 });
        assert_eq!((state.opens, state.mutations), (1, 0));
        drop(state);
        server.state.lock().unwrap().opendir_counts.clear();
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let target = LocalVfs::rooted(&root).unwrap();
        let started = std::time::Instant::now();
        let stats = copy_items(
            FileOperationRequest {
                src: source.as_ref(),
                dst: &target,
                items: &[PathBuf::from("/pane/Baum"), PathBuf::from("/pane/Zweiter")],
                dst_dir: &root,
                policy: OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(stats.files, 4);
        assert_eq!(std::fs::read(root.join("Baum/good")).unwrap(), b"content");
        assert_eq!(
            std::fs::read(root.join("Zweiter/good")).unwrap(),
            b"content"
        );
        let state = server.state.lock().unwrap();
        eprintln!("SFTP-omission-copy-cost omissions={omissions} files={} elapsed_ms={} OPENDIR-completed-listings={:?}", stats.files, started.elapsed().as_millis(), state.opendir_counts);
        assert_eq!(state.mutations, 0);
        let opens_before = state.opens;
        drop(state);
        if omissions {
            // Auch eine ausgelassene Gruppe einer anderen ausgewählten Wurzel
            // gehört zum bestätigten Lauf, nicht nur Vorfahren der nächsten Datei.
            server
                .state
                .lock()
                .unwrap()
                .tree
                .as_mut()
                .unwrap()
                .get_mut("/vault/pane/Zweiter/Name")
                .unwrap()
                .0
                .mtime = Some(123457);
            assert_eq!(
                error_code(
                    &source
                        .open_read(Path::new("/pane/Baum/good"))
                        .err()
                        .unwrap()
                ),
                ErrorCode::PlanDrift
            );
            assert_eq!(server.state.lock().unwrap().opens, opens_before);
        }
    }
}

#[test]
fn sftp_rekursive_omission_erhaelt_cancel_listingverlust_und_sitzungsdrift() {
    for fault in ["cancel", "listing_lost", "session"] {
        let server = Server::tree();
        for name in ["Name", "name"] {
            tree_file(
                &server,
                &format!("/vault/pane/Baum/{name}"),
                b"omit".to_vec(),
            );
        }
        let provider = server.connect();
        let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
        let cancel = Cancel::new();
        let (source, _) = endpoint
            .interactive_file_read_context(&[PathBuf::from("/pane/Baum")], &cancel)
            .unwrap();
        {
            let mut state = server.state.lock().unwrap();
            state.opendir_counts.clear();
            if fault == "cancel" {
                state.cancel_on_readdir = Some(cancel.clone());
            }
            if fault == "listing_lost" {
                state.fault = Fault::TreeListingLost;
            }
        }
        if fault == "session" {
            provider.reconnect().unwrap();
        }
        let error = source
            .open_read(Path::new("/pane/Baum/Kind.txt"))
            .err()
            .unwrap();
        assert_eq!(
            error_code(&error),
            match fault {
                "cancel" => ErrorCode::Interrupted,
                "listing_lost" => ErrorCode::ConnectionLost,
                _ => ErrorCode::PlanDrift,
            },
            "{fault}: {error}"
        );
        let state = server.state.lock().unwrap();
        assert_eq!((state.opens, state.mutations), (0, 0));
        assert_eq!(
            state
                .opendir_counts
                .get("/vault/pane/Baum")
                .copied()
                .unwrap_or(0),
            usize::from(fault != "session")
        );
    }
}

#[test]
fn sftp_baum_aliasordner_werden_einmal_ausgelassen_und_belege_erkennen_drift() {
    for rename_omission in [false, true] {
        let server = Server::tree();
        tree_file(&server, "/vault/pane/Baum/Name", b"Links".to_vec());
        {
            let mut state = server.state.lock().unwrap();
            state.page_size = 1;
            let directory = attrs(&state, false);
            state
                .tree
                .as_mut()
                .unwrap()
                .insert("/vault/pane/Baum/name".into(), (directory, Vec::new()));
        }
        tree_file(
            &server,
            "/vault/pane/Baum/name/Kind",
            b"Teilbaum bleibt".to_vec(),
        );
        let (_provider, source) = tree_context(&server, &Cancel::new());
        let before = source.interactive_source_tree().unwrap().unwrap();
        assert_eq!(
            (
                before.entries,
                before.files,
                before.directories,
                before.omitted.len()
            ),
            (5, 1, 2, 2)
        );
        assert!(before
            .omitted
            .iter()
            .all(|item| !item.path.ends_with("/Kind")));
        assert!(source.stat(Path::new("/pane/Baum/name/Kind")).is_err());
        let children = source
            .interactive_copy_children(Path::new("/pane/Baum"))
            .unwrap()
            .unwrap();
        assert_eq!(children.len(), 2);
        assert!(children
            .iter()
            .all(|child| ["Kind.txt", "Leer"].contains(&child.entry.name.as_str())));
        {
            let mut state = server.state.lock().unwrap();
            let tree = state.tree.as_mut().unwrap();
            if rename_omission {
                let entry = tree.remove("/vault/pane/Baum/Name").unwrap();
                tree.insert("/vault/pane/Baum/Eindeutig".into(), entry);
            } else {
                tree.get_mut("/vault/pane/Baum/Name").unwrap().0.uid = Some(2000);
            }
        }
        let (_, fresh) = tree_context(&server, &Cancel::new());
        assert_ne!(
            before.digest,
            fresh.interactive_source_tree().unwrap().unwrap().digest
        );
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let target = LocalVfs::rooted(&root).unwrap();
        let error = copy_items(
            FileOperationRequest {
                src: source.as_ref(),
                dst: &target,
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
        let state = server.state.lock().unwrap();
        assert_eq!((state.opens, state.mutations), (0, 0));
        assert_eq!(
            state.tree.as_ref().unwrap()["/vault/pane/Baum/name/Kind"].1,
            b"Teilbaum bleibt"
        );
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
fn sftp_baum_akzeptiert_grosse_bestaende_und_behaelt_namenskollisionen() {
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
                attributes.size = Some(8 * 1024 * 1024 * 1024u64);
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
        let result =
            endpoint.interactive_file_read_context(&[PathBuf::from("/pane/Baum")], &Cancel::new());
        let (source, _) = result.unwrap_or_else(|error| panic!("Baumvariante {variant}: {error}"));
        if variant == 3 {
            assert_eq!(
                source
                    .interactive_source_tree()
                    .unwrap()
                    .unwrap()
                    .omitted
                    .len(),
                2
            );
        }
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
                write_handles: HashMap::new(),
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
    handles: HashMap<String, (Vec<u8>, bool, FileAttributes)>,
    directories: HashMap<String, (String, usize)>,
    write_handles: HashMap<String, String>,
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
            Some((8 * 1024 * 1024 * 1024u64) + 1)
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
    async fn init(
        &mut self,
        _: u32,
        _: HashMap<String, String>,
    ) -> Result<russh_sftp::protocol::Version, Self::Error> {
        let mut version = russh_sftp::protocol::Version::new();
        if let Some(value) = self.state.lock().unwrap().principal_version {
            version
                .extensions
                .insert("users-groups-by-id@openssh.com".into(), value.into());
        }
        Ok(version)
    }
    async fn extended(
        &mut self,
        id: u32,
        request: String,
        data: Vec<u8>,
    ) -> Result<russh_sftp::protocol::Packet, Self::Error> {
        if request != "users-groups-by-id@openssh.com" {
            return Err(StatusCode::OpUnsupported);
        }
        let mut state = self.state.lock().unwrap();
        state.principal_requests.push(data.clone());
        match state.principal_fault {
            "denied" => return Err(StatusCode::PermissionDenied),
            "lost" => return Err(StatusCode::ConnectionLost),
            "malformed" => {
                return Ok(russh_sftp::protocol::Packet::ExtendedReply(
                    russh_sftp::protocol::ExtendedReply {
                        id,
                        data: vec![0, 0, 0, 1, 255],
                    },
                ))
            }
            _ => {}
        }
        let mut input = data.as_slice();
        let mut output = Vec::new();
        for prefix in ["server-user-", "server-group-"] {
            let size = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
            assert!(size <= 128 && size.is_multiple_of(4));
            let mut names = Vec::new();
            for bytes in input[4..4 + size].chunks_exact(4) {
                let value = u32::from_be_bytes(bytes.try_into().unwrap());
                let name = if value == 9999 {
                    String::new()
                } else {
                    format!("{prefix}{value}")
                };
                names.extend_from_slice(&(name.len() as u32).to_be_bytes());
                names.extend_from_slice(name.as_bytes());
            }
            output.extend_from_slice(&(names.len() as u32).to_be_bytes());
            output.extend_from_slice(&names);
            input = &input[4 + size..];
        }
        assert!(input.is_empty());
        Ok(russh_sftp::protocol::Packet::ExtendedReply(
            russh_sftp::protocol::ExtendedReply { id, data: output },
        ))
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
        if state.fault == Fault::ForwardTempStatDenied && path.contains(".b.doppeldecker-") {
            return Err(StatusCode::PermissionDenied);
        }
        state.metadata_requests += 1;
        let request = state.metadata_requests;
        state.last_lstat.insert(path.clone(), request);
        if let Some((at, kind)) = state.swap_on_lstat {
            if at == request {
                let value = state.tree.as_mut().unwrap().get_mut(&path).unwrap();
                value.0.permissions = Some(kind | 0o777);
                value.1 = b"/vault/pane/Sentinel".to_vec();
            }
        }
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
        let mut state = self.state.lock().unwrap();
        if state.fault == Fault::ListingDenied {
            return Err(StatusCode::PermissionDenied);
        }
        let tree = state.tree.as_ref().ok_or(StatusCode::OpUnsupported)?;
        if !tree.get(&path).ok_or(StatusCode::NoSuchFile)?.0.is_dir() {
            return Err(StatusCode::Failure);
        }
        *state.opendir_counts.entry(path.clone()).or_default() += 1;
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
        let mut state = self.state.lock().unwrap();
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
        if *offset == 0 && state.mutations > 0 && !state.batch_spelling_fault.is_empty() {
            state.batch_spelling_listings += 1;
            if state.batch_spelling_listings >= 2 {
                match state.batch_spelling_fault {
                    "final_scan" => return Err(StatusCode::PermissionDenied),
                    "final_uid" => {
                        state
                            .tree
                            .as_mut()
                            .unwrap()
                            .get_mut("/vault/pane/a-one")
                            .unwrap()
                            .0
                            .uid = Some(2000)
                    }
                    "final_parent" => {
                        state
                            .tree
                            .as_mut()
                            .unwrap()
                            .get_mut("/vault/pane")
                            .unwrap()
                            .0
                            .uid = Some(2000)
                    }
                    _ => {}
                }
            }
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
        if state.batch_spelling_listings >= 1 && state.batch_spelling_fault == "wrong_raw" {
            for file in &mut files {
                if file.filename == "a-one" {
                    file.filename = "A-ONE".into();
                }
            }
        }
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
        attributes: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let mut state = self.state.lock().unwrap();
        if flags.bits() != OpenFlags::READ.bits() {
            state.mutations += 1;
            if !state.allow_writes {
                return Err(StatusCode::PermissionDenied);
            }
            if !flags.contains(OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE) {
                return Err(StatusCode::PermissionDenied);
            }
            let tree = state.tree.as_mut().ok_or(StatusCode::Failure)?;
            if tree.contains_key(&path) {
                return Err(StatusCode::Failure);
            }
            let mut attributes = attributes;
            attributes.permissions = Some(0o100000 | attributes.permissions.unwrap_or(0o600));
            attributes.size = Some(0);
            tree.insert(path.clone(), (attributes, Vec::new()));
            let handle = format!("write-{id}");
            self.write_handles.insert(handle.clone(), path);
            if state.lose_mutation_response {
                return Err(StatusCode::ConnectionLost);
            }
            return Ok(Handle { id, handle });
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
        let attributes = match &state.tree {
            Some(tree) => tree[&path].0.clone(),
            None => attrs(&state, true),
        };
        self.handles
            .insert(handle.clone(), (data, second, attributes));
        Ok(Handle { id, handle })
    }
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        if !self.handles.contains_key(&handle) {
            return Err(StatusCode::Failure);
        }
        let state = self.state.lock().unwrap();
        let mut attributes = self.handles[&handle].2.clone();
        attributes.size = Some(self.handles[&handle].0.len() as u64);
        if attributes.mtime.is_some() && state.mtime != 123456 {
            attributes.mtime = Some(state.mtime);
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
        let (data, _, _) = self.handles.get(&handle).ok_or(StatusCode::Failure)?;
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
        let data = data[offset..data.len().min(offset + length as usize)].to_vec();
        state.bytes_read += data.len() as u64;
        Ok(Data { id, data })
    }
    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attributes: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let mut state = self.state.lock().unwrap();
        state.mutations += 1;
        if !state.allow_writes {
            return Err(StatusCode::PermissionDenied);
        }
        let last_lstat = state.last_lstat[&path];
        state.setstat_observations.push((
            path.clone(),
            attributes.permissions.is_some(),
            last_lstat,
        ));
        let tree = state.tree.as_mut().ok_or(StatusCode::Failure)?;
        let entry = tree.get(&path).ok_or(StatusCode::NoSuchFile)?;
        // SETSTAT folgt einem Symlink wie ein gewöhnlicher SFTP-v3-Server.
        let target = if entry.0.is_symlink() {
            String::from_utf8(entry.1.clone()).map_err(|_| StatusCode::Failure)?
        } else {
            path
        };
        let value = &mut tree.get_mut(&target).ok_or(StatusCode::NoSuchFile)?.0;
        if let Some(mode) = attributes.permissions {
            value.permissions = value.permissions.map(|old| old & !0o7777 | mode & 0o7777);
        }
        if let Some(mtime) = attributes.mtime {
            value.mtime = Some(mtime);
            value.atime = attributes.atime;
        }
        if state.lose_mutation_response {
            return Err(StatusCode::ConnectionLost);
        }
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: String::new(),
            language_tag: String::new(),
        })
    }
    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let mut state = self.state.lock().unwrap();
        state.mutations += 1;
        if !state.allow_writes {
            return Err(StatusCode::PermissionDenied);
        }
        let directory = attrs(&state, false);
        let tree = state.tree.as_mut().unwrap();
        if tree.contains_key(&path) {
            return Err(StatusCode::Failure);
        }
        tree.insert(path, (directory, Vec::new()));
        if state.lose_mutation_response {
            return Err(StatusCode::ConnectionLost);
        }
        Ok(status(id))
    }
    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let delay = {
            let mut state = self.state.lock().unwrap();
            state.mutations += 1;
            let path = self
                .write_handles
                .get(&handle)
                .ok_or(StatusCode::PermissionDenied)?;
            let (attributes, bytes) = state
                .tree
                .as_mut()
                .unwrap()
                .get_mut(path)
                .ok_or(StatusCode::NoSuchFile)?;
            let offset = offset as usize;
            if offset != bytes.len() {
                return Err(StatusCode::Failure);
            }
            bytes.extend_from_slice(&data);
            attributes.size = Some(bytes.len() as u64);
            if matches!(
                state.fault,
                Fault::ForwardMidWrite | Fault::ForwardMidWriteHold
            ) && path.contains(".b.doppeldecker-")
            {
                let delay = if state.fault == Fault::ForwardMidWriteHold {
                    5000
                } else {
                    500
                };
                Some(delay)
            } else {
                if let Some(cancel) = &state.cancel_on_write {
                    cancel.cancel();
                }
                if state.lose_mutation_response {
                    return Err(StatusCode::ConnectionLost);
                }
                None
            }
        };
        if let Some(delay) = delay {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        Ok(status(id))
    }
    async fn rename(&mut self, id: u32, from: String, to: String) -> Result<Status, Self::Error> {
        let mut state = self.state.lock().unwrap();
        state.mutations += 1;
        if !state.allow_writes {
            return Err(StatusCode::PermissionDenied);
        }
        if state.batch_spelling_fault == "noop" {
            return Ok(status(id));
        }
        let update_mtime = state.rename_parent_mtime;
        let update_uid = state.rename_parent_uid;
        let parent = Path::new(&from)
            .parent()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let tree = state.tree.as_mut().unwrap();
        if tree.contains_key(&to) {
            return Err(StatusCode::Failure);
        }
        let entry = tree.remove(&from).ok_or(StatusCode::NoSuchFile)?;
        let children = tree
            .keys()
            .filter(|path| path.starts_with(&format!("{from}/")))
            .cloned()
            .collect::<Vec<_>>();
        for child in children {
            let entry = tree.remove(&child).unwrap();
            tree.insert(format!("{to}{}", &child[from.len()..]), entry);
        }
        tree.insert(to, entry);
        if update_mtime {
            let attrs = &mut tree.get_mut(&parent).unwrap().0;
            attrs.mtime = Some(attrs.mtime.unwrap_or_default() + 1);
        }
        if update_uid {
            tree.get_mut(&parent).unwrap().0.uid = Some(2000);
        }
        if state.lose_mutation_response {
            return Err(StatusCode::ConnectionLost);
        }
        Ok(status(id))
    }
    async fn remove(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let mut state = self.state.lock().unwrap();
        state.mutations += 1;
        if !state.allow_writes {
            return Err(StatusCode::PermissionDenied);
        }
        let tree = state.tree.as_mut().unwrap();
        if !tree
            .get(&path)
            .ok_or(StatusCode::NoSuchFile)?
            .0
            .is_regular()
        {
            return Err(StatusCode::Failure);
        }
        tree.remove(&path);
        if state.lose_mutation_response {
            return Err(StatusCode::ConnectionLost);
        }
        Ok(status(id))
    }
    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let mut state = self.state.lock().unwrap();
        state.mutations += 1;
        if !state.allow_writes {
            return Err(StatusCode::PermissionDenied);
        }
        let tree = state.tree.as_mut().unwrap();
        if !tree.get(&path).ok_or(StatusCode::NoSuchFile)?.0.is_dir()
            || tree.keys().any(|key| key.starts_with(&format!("{path}/")))
        {
            return Err(StatusCode::Failure);
        }
        tree.remove(&path);
        if state.lose_mutation_response {
            return Err(StatusCode::ConnectionLost);
        }
        Ok(status(id))
    }
    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        let file = self.handles.remove(&handle);
        let readback = file.as_ref().is_some_and(|(_, second, _)| *second);
        self.directories.remove(&handle);
        self.write_handles.remove(&handle);
        let mut state = self.state.lock().unwrap();
        state.closes += 1;
        if file.is_some() {
            if let Some(path) = state.omission_drift_after_read_close.take() {
                state.tree.as_mut().unwrap().get_mut(&path).unwrap().0.uid = Some(2000);
            }
        }
        if state.fault == Fault::CloseLost || (readback && state.lose_readback_close) {
            return Err(StatusCode::ConnectionLost);
        }
        Ok(status(id))
    }
}

#[test]
fn snapshot_bleibt_privat_und_download_erhaelt_darstellbare_ausfuehrungsrechte() {
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
        {
            let state = server.state.lock().unwrap();
            assert_eq!(
                state.opens, 1,
                "ein Remote-Inhaltsdurchlauf für den Standardsnapshot"
            );
            assert_eq!(state.bytes_read, data.len() as u64);
        }
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
            0o111
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
fn bewusstes_originalformat_und_rueckuploadbeleg_trennen_format_von_remoteaenderungen() {
    use std::os::unix::fs::PermissionsExt;
    let server = Server::new(b"<b>abc</b>".to_vec(), Fault::None);
    let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let source = Path::new("/pane/inhalt.html");
    let passive = endpoint
        .download_interactive_snapshot(source, cache.path(), &Cancel::new())
        .unwrap();
    assert!(passive.path.ends_with(".txt"));
    let original = endpoint
        .download_interactive_snapshot_with_options(
            source,
            cache.path(),
            &Cancel::new(),
            SnapshotOptions {
                open_mode: SnapshotOpenMode::OriginalFormat,
                ..SnapshotOptions::default()
            },
        )
        .unwrap();
    assert!(original.path.ends_with(".html"));
    assert_eq!(
        std::fs::metadata(&original.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(original.original.mode, Some(0o777));
    {
        let state = server.state.lock().unwrap();
        assert_eq!(state.opens, 2);
        assert_eq!(state.bytes_read, 20);
    }
    endpoint
        .verify_interactive_remote_original(source, &original.original, &Cancel::new())
        .unwrap();
    std::fs::write(&original.path, b"lokal editiert").unwrap();
    endpoint
        .verify_interactive_remote_original(source, &original.original, &Cancel::new())
        .unwrap();
    server.state.lock().unwrap().data = b"<b>xyz</b>".to_vec();
    let error = endpoint
        .verify_interactive_remote_original(source, &original.original, &Cancel::new())
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::PlanDrift);
    assert_eq!(server.state.lock().unwrap().mutations, 0);
}

#[test]
fn grosse_direktauswahl_wird_ohne_inhaltsopen_vorbereitet() {
    let large = Server::new(Vec::new(), Fault::TooLarge);
    let endpoint = ProviderEndpoint::sftp(large.connect(), Path::new("/pane")).unwrap();
    let (source, _) = endpoint
        .interactive_file_read_context(&[PathBuf::from("/pane/Gross")], &Cancel::new())
        .unwrap();
    assert!(source.stat(Path::new("/pane/Gross")).unwrap().size > 8 * 1024 * 1024 * 1024);
    assert_eq!(large.state.lock().unwrap().opens, 0);
    let server = Server::tree();
    let mut items = Vec::new();
    for n in 0..125 {
        let name = format!("f{n}");
        tree_file(&server, &format!("/vault/pane/{name}"), Vec::new());
        items.push(PathBuf::from(format!("/pane/{name}")));
    }
    let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
    let (source, _) = endpoint
        .interactive_file_read_context(&items, &Cancel::new())
        .unwrap();
    assert!(source.stat(&items[124]).unwrap().is_file);
    assert_eq!(server.state.lock().unwrap().opens, 0);
}

#[test]
fn symlink_spezialdatei_und_fehlende_belege_stoppen_vor_open() {
    for fault in [Fault::Symlink, Fault::Special, Fault::MissingSize] {
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
        let error = download_remote_snapshot_with_options(
            vfs.as_ref(),
            Path::new("/pane/inhalt.txt"),
            cache.path(),
            &cancel,
            SnapshotOptions {
                verification: if matches!(fault, Fault::ContentDrift | Fault::ReadbackDenied) {
                    SnapshotVerification::FullReadback
                } else {
                    SnapshotVerification::Streaming
                },
                ..SnapshotOptions::default()
            },
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
fn queue_akzeptiert_sftp_move_replace_und_remote_ziel_ohne_providerkontakt() {
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
        queue.enqueue(operation).unwrap();
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
    run_sftp_download_crash_case(false);
}

#[test]
fn sftp_forward_blocks_private_stage_after_local_download_crash() {
    run_sftp_download_crash_case(true);
}

fn run_sftp_download_crash_case(private_stage: bool) {
    use std::os::unix::fs::PermissionsExt;
    let server = if private_stage {
        let server = Server::tree();
        server
            .state
            .lock()
            .unwrap()
            .tree
            .as_mut()
            .unwrap()
            .retain(|path, _| path == "/vault" || path == "/vault/pane");
        tree_file(
            &server,
            "/vault/pane/inhalt.txt",
            vec![0x63; 4 * 1024 * 1024],
        );
        server
    } else {
        Server::new(vec![0x63; 4 * 1024 * 1024], Fault::None)
    };
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = root.join("ziel");
    std::fs::create_dir(&target).unwrap();
    // Ein öffentlich lesbarer Zielordner eignet sich absichtlich nicht als privater
    // C2-Namespace; der Nachbar-Temp muss vor Anlage im privaten Queueindex stehen.
    std::fs::set_permissions(
        &target,
        std::fs::Permissions::from_mode(if private_stage { 0o700 } else { 0o755 }),
    )
    .unwrap();
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
    let process_lock = Arc::new(OperationQueueProcessLock::acquire(&journal).unwrap());
    let queue = OperationQueue::open_with_process_lock(&journal, process_lock.clone()).unwrap();
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.entries[0].state, QueueState::Interrupted);
    assert!(!snapshot.entries[0].temp_recovery_paths.is_empty());
    assert!(snapshot.entries[0]
        .temp_recovery_paths
        .iter()
        .any(|path| Path::new(path).is_file()));
    assert!(queue.claim_next().unwrap().is_none());
    assert!(!target.join("inhalt.txt").exists());
    if private_stage {
        let stage = std::fs::read_dir(&target)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .expect("Nichtleerer privater Stagebaum liegt im Paarroot");
        assert!(!std::fs::read_dir(&stage)
            .unwrap()
            .collect::<Vec<_>>()
            .is_empty());
        let baseline = root.join("forward-baseline.json");
        let pair = PairId::generate().unwrap();
        // Dieser noch unbestätigte Auftrag erteilt kein Schreibrecht. Die offene
        // Stage muss bereits vor Planung und jedem Inhaltsopen sperren.
        let forward = OperationQueue::open_with_process_lock(
            config.join("operations-queue-forward-stage.json"),
            process_lock,
        )
        .unwrap();
        forward
            .enqueue(QueueOperation::BidirectionalForward {
                pair_id: pair,
                pair_name: "Privater Downloadrest".into(),
                left: Box::new(BidirectionalQueueEndpoint::Local {
                    root: target.to_string_lossy().into_owned(),
                }),
                right: Box::new(BidirectionalQueueEndpoint::Sftp {
                    connection: Box::new(server.config.clone()),
                    root: "/pane".into(),
                }),
                baseline_path: baseline.to_string_lossy().into_owned(),
                resolutions: vec![],
                expected_baseline_digest: None,
                expected_plan_digest: [1; 32],
                expected_unresolved_digest: [2; 32],
                expected_preview_digest: [3; 32],
            })
            .unwrap();
        let entry = forward.claim_next().unwrap().unwrap();
        let opens = server.state.lock().unwrap().opens;
        let error = queue::prepare_bidirectional_forward_queue_entry(
            &forward,
            entry,
            Arc::new(FakeSecrets),
            &Cancel::new(),
        )
        .err()
        .expect("Registrierter privater Stagebaum darf nicht gescannt oder hochgeladen werden");
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(server.state.lock().unwrap().opens, opens);
        assert_eq!(server.state.lock().unwrap().mutations, 0);
        assert!(!baseline.exists());
    }
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
fn optionaler_readback_erkennt_inhaltsdrift_und_closeverlust_entwertet_die_sitzung() {
    let server = Server::new(b"Daten".to_vec(), Fault::ContentDrift);
    server.state.lock().unwrap().lose_readback_close = true;
    let (provider, old) = server.context(&Cancel::new());
    let cache = tempfile::tempdir().unwrap();
    assert!(download_remote_snapshot_with_options(
        old.as_ref(),
        Path::new("/pane/inhalt.txt"),
        cache.path(),
        &Cancel::new(),
        SnapshotOptions {
            verification: SnapshotVerification::FullReadback,
            ..SnapshotOptions::default()
        }
    )
    .is_err());
    {
        let mut state = server.state.lock().unwrap();
        assert_eq!((state.opens, state.closes), (2, 2));
        state.fault = Fault::None;
        state.lose_readback_close = false;
    }
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    endpoint
        .download_interactive_snapshot(Path::new("/pane/inhalt.txt"), cache.path(), &Cancel::new())
        .unwrap();
    assert_eq!(provider.session_generation(), 2);
    assert!(old.open_read(Path::new("/pane/inhalt.txt")).is_err());
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
            .is_ok(),
        "Inhaltsvorbereitung darf mehr als 16 GiB binden"
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
            .is_ok(),
        "Inhaltsvorbereitung darf Dateien über 8 GiB binden"
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
fn sftp_ordnergroesse_erlaubt_tiefe_baeume_und_meldet_rechenueberlauf() {
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
    assert_eq!(result[0].status, DirectorySizeStatus::Observed);
    assert_eq!(result[1].status, DirectorySizeStatus::Failed);
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
    let (contents, _) = endpoint
        .interactive_file_read_context(&[PathBuf::from("/pane/Baum")], &Cancel::new())
        .unwrap();
    let summary = contents.interactive_source_tree().unwrap().unwrap();
    assert_eq!(
        (
            summary.bytes,
            summary.files,
            summary.directories,
            summary.omitted.len()
        ),
        (11, 1, 2, 4),
        "Nur der Inhaltsdownload lässt Aliasgruppen aus; die Größenbilanz zählt alle Serverdateien"
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

#[test]
fn interaktives_sftp_mkdir_prueft_kollision_und_haelt_strikten_provider_gesperrt() {
    let server = Server::tree();
    server.state.lock().unwrap().allow_writes = true;
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let contract = directory_create_contract(ProviderKind::Sftp);
    assert_eq!(
        contract.execution_class,
        Some(DirectoryCreateExecutionClass::BestEffort)
    );
    let report = endpoint
        .create_interactive_directory("Neu", DirectoryCreateExecutionClass::BestEffort)
        .unwrap();
    assert_eq!(report.path, Path::new("/pane/Neu"));
    assert!(server
        .state
        .lock()
        .unwrap()
        .tree
        .as_ref()
        .unwrap()
        .contains_key("/vault/pane/Neu"));
    assert_eq!(server.state.lock().unwrap().mutations, 1);
    let error = endpoint
        .create_interactive_directory("Neu", DirectoryCreateExecutionClass::BestEffort)
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::AlreadyExists);
    assert_eq!(server.state.lock().unwrap().mutations, 1);
    assert!(!provider.capabilities().permits_directory_create());
    assert!(provider.create_dir(Path::new("/pane/Strikt")).is_err());
    let error = endpoint
        .create_interactive_directory("../Ausbruch", DirectoryCreateExecutionClass::BestEffort)
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::InvalidInput);
    assert_eq!(server.state.lock().unwrap().mutations, 1);
}

#[test]
fn interaktive_sftp_datei_und_baumuploads_committen_geprueft_und_ersetzen_nur_bestaetigt() {
    let server = Server::tree();
    server.state.lock().unwrap().allow_writes = true;
    let provider = server.connect();
    let remote = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let source = tempfile::tempdir().unwrap();
    let source_root = source.path().canonicalize().unwrap();
    std::fs::write(source_root.join("Datei.txt"), b"erster Inhalt").unwrap();
    std::fs::create_dir_all(source_root.join("UploadBaum/Leer")).unwrap();
    std::fs::write(source_root.join("UploadBaum/Kind.txt"), b"Kindinhalt").unwrap();
    let local = ProviderEndpoint::local(&source_root).unwrap();
    let items = [
        source_root.join("Datei.txt"),
        source_root.join("UploadBaum"),
    ];
    let contexts = local
        .interactive_file_contexts(&remote, &items, false, false, &Cancel::new())
        .unwrap();
    let request = || fileops::FileOperationRequest {
        src: contexts.source_vfs.as_ref(),
        dst: contexts.target_vfs.as_ref(),
        items: &items,
        dst_dir: Path::new("/pane"),
        policy: OverwritePolicy::Skip,
        sandbox: &None,
    };
    let stats = fileops::copy_items(request(), &Cancel::new(), &mut |_| {}).unwrap();
    assert_eq!(stats.files, 2);
    assert!(stats.target_copy_new_best_effort);
    {
        let state = server.state.lock().unwrap();
        let tree = state.tree.as_ref().unwrap();
        assert_eq!(tree["/vault/pane/Datei.txt"].1, b"erster Inhalt");
        assert_eq!(tree["/vault/pane/UploadBaum/Kind.txt"].1, b"Kindinhalt");
        assert!(tree["/vault/pane/UploadBaum/Leer"].0.is_dir());
        assert!(!tree.keys().any(|path| path.contains("doppeldecker")));
    }
    std::fs::write(source_root.join("Datei.txt"), b"bestaetigt ersetzt").unwrap();
    let replacement = [source_root.join("Datei.txt")];
    let contexts = local
        .interactive_file_contexts(&remote, &replacement, false, true, &Cancel::new())
        .unwrap();
    let _proofs =
        capture_file_conflict_proofs(contexts.target_vfs.as_ref(), &["/pane/Datei.txt".into()])
            .unwrap();
    let stats = fileops::copy_items(
        fileops::FileOperationRequest {
            src: contexts.source_vfs.as_ref(),
            dst: contexts.target_vfs.as_ref(),
            items: &replacement,
            dst_dir: Path::new("/pane"),
            policy: OverwritePolicy::Overwrite,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(stats.target_replace_best_effort);
    assert_eq!(
        server.state.lock().unwrap().tree.as_ref().unwrap()["/vault/pane/Datei.txt"].1,
        b"bestaetigt ersetzt"
    );
    assert!(!provider.capabilities().permits_add_only());
    assert!(!provider.capabilities().atomic_replace_rename);
}

#[test]
fn interaktives_sftp_rename_und_bestaetigtes_loeschen_bleiben_getrennt_von_atomaren_zusagen() {
    let server = Server::tree();
    server.state.lock().unwrap().allow_writes = true;
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let report = endpoint
        .execute_ordinary_rename(
            Path::new("/pane/Baum"),
            "Umbenannt",
            &None,
            Some(RenameExecutionClass::BestEffort),
        )
        .unwrap();
    assert_eq!(report.outcome, RenameOutcome::Renamed);
    assert!(server
        .state
        .lock()
        .unwrap()
        .tree
        .as_ref()
        .unwrap()
        .contains_key("/vault/pane/Umbenannt/Kind.txt"));
    let (vfs, root) = endpoint.ordinary_file_context().unwrap();
    assert!(!vfs.supports_remove_if_matches());
    assert!(vfs.supports_best_effort_permanent_delete());
    let selected = [root.join("Umbenannt")];
    let plan = plan_remote_delete(vfs.as_ref(), &root, &selected).unwrap();
    let stats = execute_remote_delete(
        vfs.as_ref(),
        &plan,
        RemoteDeleteMode::Permanent,
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(stats.entries, 3);
    assert_eq!(stats.items, 1);
    assert!(!server
        .state
        .lock()
        .unwrap()
        .tree
        .as_ref()
        .unwrap()
        .keys()
        .any(|path| path.starts_with("/vault/pane/Umbenannt")));
    assert!(!provider.capabilities().entry_delete);
}

#[test]
fn verlorene_sftp_mutationsantwort_wird_nicht_wiederholt_und_meldet_recoverypfad() {
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        state.allow_writes = true;
        state.lose_mutation_response = true;
    }
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap();
    let error = endpoint
        .create_interactive_directory("AntwortVerloren", DirectoryCreateExecutionClass::BestEffort)
        .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    assert!(error.to_string().contains("AntwortVerloren"));
    let state = server.state.lock().unwrap();
    assert_eq!(state.mutations, 1);
    assert!(state
        .tree
        .as_ref()
        .unwrap()
        .contains_key("/vault/pane/AntwortVerloren"));
}

#[test]
fn sftp_move_zwischen_panes_laueft_ueber_die_queue_und_entfernt_quelle_nach_readback() {
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        state.allow_writes = true;
        let directory = attrs(&state, false);
        state
            .tree
            .as_mut()
            .unwrap()
            .insert("/vault/ziel".into(), (directory, Vec::new()));
    }
    let provider = server.connect();
    let source = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let target = ProviderEndpoint::sftp(provider.clone(), Path::new("/ziel")).unwrap();
    let items = [PathBuf::from("/pane/Baum")];
    let contexts = source
        .interactive_file_contexts(&target, &items, true, false, &Cancel::new())
        .unwrap();
    let tree = contexts.source_vfs.interactive_source_tree().unwrap();
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let config = private_config(&root);
    let queue = OperationQueue::open(config.join("queue.json")).unwrap();
    let operation = QueueOperation::ProviderFile {
        kind: FileQueueKind::Move,
        source: Box::new(BidirectionalQueueEndpoint::Sftp {
            connection: Box::new(server.config.clone()),
            root: "/pane".into(),
        }),
        target: Box::new(BidirectionalQueueEndpoint::Sftp {
            connection: Box::new(server.config.clone()),
            root: "/ziel".into(),
        }),
        items: vec!["/pane/Baum".into()],
        destination: "/ziel".into(),
        overwrite: false,
        expected_conflicts: Vec::new(),
        expected_conflict_proofs: Vec::new(),
        source_tree_expected: tree.map(Box::new),
        keep_both_targets: Vec::new(),
        best_effort_commit_expected: None,
        local_copy_new_best_effort_expected: None,
        copy_new_best_effort_expected: Some(true),
        replace_best_effort_expected: Some(false),
        remove_source_best_effort_expected: Some(true),
    };
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
    let QueueExecutionResult::File(stats) = result else {
        panic!("kein Dateiergebnis")
    };
    assert_eq!(stats.files, 1);
    assert!(stats.source_removal_best_effort);
    assert!(stats.target_copy_new_best_effort);
    let state = server.state.lock().unwrap();
    assert_eq!(
        state.mutations, 1,
        "Gleichserver-F6 verwendet genau ein Rename"
    );
    assert_eq!(
        state.opens, 0,
        "Gleichserver-F6 überträgt keine Dateiinhalte"
    );
    let tree = state.tree.as_ref().unwrap();
    assert_eq!(tree["/vault/ziel/Baum/Kind.txt"].1, b"Bauminhalt\n");
    assert!(!tree.keys().any(|path| path.starts_with("/vault/pane/Baum")));
    assert!(!tree.keys().any(|path| path.contains("doppeldecker")));
    drop(state);
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    assert!(std::fs::read_dir(&config).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with("recovery.json")));
    assert!(!provider.capabilities().entry_delete);
}

#[test]
fn sftp_delete_antwortverlust_bleibt_im_queueaudit_mit_recoverypfad_erhalten() {
    let server = Server::tree();
    server.state.lock().unwrap().allow_writes = true;
    let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane/Baum")).unwrap();
    let (vfs, root) = endpoint.ordinary_file_context().unwrap();
    let plan = plan_remote_delete(vfs.as_ref(), &root, &[root.join("Kind.txt")]).unwrap();
    let area = tempfile::tempdir().unwrap();
    let config = private_config(&area.path().canonicalize().unwrap());
    let queue = OperationQueue::open(config.join("queue.json")).unwrap();
    queue
        .enqueue(QueueOperation::ProviderDelete {
            endpoint: Box::new(BidirectionalQueueEndpoint::Sftp {
                connection: Box::new(server.config.clone()),
                root: root.to_string_lossy().into_owned(),
            }),
            mode: RemoteDeleteMode::Permanent,
            expected_plan: Box::new(plan),
        })
        .unwrap();
    server.state.lock().unwrap().lose_mutation_response = true;
    let error = queue::execute_provider_delete_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        Arc::new(FakeSecrets),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    assert_eq!(server.state.lock().unwrap().mutations, 1);
    assert!(error::recovery_paths(&error)
        .iter()
        .any(|path| path == "/pane/Baum/Kind.txt"));
    let audit = std::fs::read_to_string(config.join("queue-audit.jsonl")).unwrap();
    let failed = audit
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|record| record["status"] == "failed")
        .unwrap();
    assert!(failed.to_string().contains("/pane/Baum/Kind.txt"));
}

#[test]
fn sftp_quellsicht_erlaubt_lokales_ersetzen_und_f6_ohne_additivsperre() {
    let server = Server::tree();
    server.state.lock().unwrap().allow_writes = true;
    let provider = server.connect();
    let source = ProviderEndpoint::sftp(provider, Path::new("/pane/Baum")).unwrap();
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = ProviderEndpoint::local(&root).unwrap();
    let items = [PathBuf::from("/pane/Baum/Kind.txt")];
    std::fs::write(root.join("Kind.txt"), b"bestaetigte Altdatei").unwrap();
    let contexts = source
        .interactive_file_contexts(&target, &items, false, true, &Cancel::new())
        .unwrap();
    assert!(!contexts.source_vfs.interactive_additive_only());
    let stats = fileops::copy_items(
        fileops::FileOperationRequest {
            src: contexts.source_vfs.as_ref(),
            dst: contexts.target_vfs.as_ref(),
            items: &items,
            dst_dir: &root,
            policy: OverwritePolicy::Overwrite,
            sandbox: &Some(root.clone()),
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(
        std::fs::read(root.join("Kind.txt")).unwrap(),
        b"Bauminhalt\n"
    );
    std::fs::remove_file(root.join("Kind.txt")).unwrap();
    let contexts = source
        .interactive_file_contexts(&target, &items, true, false, &Cancel::new())
        .unwrap();
    let stats = fileops::move_items(
        fileops::FileOperationRequest {
            src: contexts.source_vfs.as_ref(),
            dst: contexts.target_vfs.as_ref(),
            items: &items,
            dst_dir: &root,
            policy: OverwritePolicy::Skip,
            sandbox: &Some(root.clone()),
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(stats.files, 1);
    assert!(stats.source_removal_best_effort);
    assert_eq!(
        std::fs::read(root.join("Kind.txt")).unwrap(),
        b"Bauminhalt\n"
    );
    assert!(!server
        .state
        .lock()
        .unwrap()
        .tree
        .as_ref()
        .unwrap()
        .contains_key("/vault/pane/Baum/Kind.txt"));
}

#[test]
fn neue_sftp_schreibklasse_akzeptiert_grosse_sparse_quelle_und_streamt_bis_zum_abbruch() {
    let server = Server::tree();
    server.state.lock().unwrap().allow_writes = true;
    let target = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let path = root.join("Gross.bin");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(9 * 1024 * 1024 * 1024)
        .unwrap();
    let source = ProviderEndpoint::local(&root).unwrap();
    let cancel = Cancel::new();
    let contexts = source
        .interactive_file_contexts(&target, std::slice::from_ref(&path), false, false, &cancel)
        .unwrap();
    let stats = fileops::copy_items(
        fileops::FileOperationRequest {
            src: contexts.source_vfs.as_ref(),
            dst: contexts.target_vfs.as_ref(),
            items: &[path],
            dst_dir: Path::new("/pane"),
            policy: OverwritePolicy::Skip,
            sandbox: &Some(root.clone()),
        },
        &cancel,
        &mut |progress| {
            if progress.bytes_done > 0 {
                cancel.cancel();
            }
        },
    )
    .unwrap();
    assert!(stats.cancelled);
    let state = server.state.lock().unwrap();
    let tree = state.tree.as_ref().unwrap();
    assert!(!tree.contains_key("/vault/pane/Gross.bin"));
    assert!(tree
        .iter()
        .any(|(path, (_, data))| path.contains("doppeldecker")
            && !data.is_empty()
            && data.len() <= 128 * 1024));
}

#[test]
fn sftp_delete_grosser_datei_verwendet_metadaten_statt_inhaltsdownload() {
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        state.allow_writes = true;
        state
            .tree
            .as_mut()
            .unwrap()
            .get_mut("/vault/pane/Baum/Kind.txt")
            .unwrap()
            .0
            .size = Some(100 * 1024 * 1024 * 1024);
    }
    let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane/Baum")).unwrap();
    let (vfs, root) = endpoint.ordinary_file_context().unwrap();
    let plan = plan_remote_delete(vfs.as_ref(), &root, &[root.join("Kind.txt")]).unwrap();
    assert_eq!(plan.bytes, 100 * 1024 * 1024 * 1024);
    let stats = execute_remote_delete(
        vfs.as_ref(),
        &plan,
        RemoteDeleteMode::Permanent,
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(stats.bytes, plan.bytes);
    let state = server.state.lock().unwrap();
    assert_eq!(state.mutations, 1);
    assert_eq!(state.opens, 0);
}

fn interactive_transfer_operation(
    source: BidirectionalQueueEndpoint,
    target: BidirectionalQueueEndpoint,
    item: &str,
    kind: FileQueueKind,
) -> QueueOperation {
    let destination = target.root().to_owned();
    QueueOperation::ProviderFile {
        kind,
        source: Box::new(source),
        target: Box::new(target),
        items: vec![item.into()],
        destination,
        overwrite: false,
        expected_conflicts: Vec::new(),
        expected_conflict_proofs: Vec::new(),
        source_tree_expected: None,
        keep_both_targets: Vec::new(),
        best_effort_commit_expected: None,
        local_copy_new_best_effort_expected: None,
        copy_new_best_effort_expected: Some(true),
        replace_best_effort_expected: Some(false),
        remove_source_best_effort_expected: Some(kind == FileQueueKind::Move),
    }
}

fn save_bound_sftp_profile(
    config: &Path,
    id: &str,
    connection: &SftpConnectionConfig,
) -> RemoteProfileBinding {
    let saved = save_sftp_profile_versioned(
        &config.join("sftp-profiles.json"),
        SftpProfile {
            id: id.into(),
            name: id.into(),
            connection: connection.clone(),
        },
    )
    .unwrap();
    RemoteProfileBinding::new(RemoteProfileProtocol::Sftp, id, saved.revision).unwrap()
}

#[test]
fn sftp_queue_crossprofil_kopie_und_move_pruefen_eigene_konfiguration_und_beide_revisionen() {
    for kind in [FileQueueKind::Copy, FileQueueKind::Move] {
        let source = Server::tree();
        let target = Server::tree();
        source.state.lock().unwrap().allow_writes = true;
        target.state.lock().unwrap().allow_writes = true;
        assert_ne!(source.config, target.config);
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let config = private_config(&root);
        let bindings = vec![
            save_bound_sftp_profile(&config, "quelle", &source.config),
            save_bound_sftp_profile(&config, "ziel", &target.config),
        ];
        let queue = OperationQueue::open(config.join("queue.json")).unwrap();
        let entry = queue
            .enqueue_claimed_with_profile_bindings(
                interactive_transfer_operation(
                    BidirectionalQueueEndpoint::Sftp {
                        connection: Box::new(source.config.clone()),
                        root: "/pane/Baum".into(),
                    },
                    BidirectionalQueueEndpoint::Sftp {
                        connection: Box::new(target.config.clone()),
                        root: "/pane".into(),
                    },
                    "/pane/Baum/Kind.txt",
                    kind,
                ),
                bindings,
            )
            .unwrap();
        let result = queue::execute_provider_file_queue_entry(
            &queue,
            entry,
            &execution(&root),
            Arc::new(FakeSecrets),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        let QueueExecutionResult::File(stats) = result else {
            panic!("kein Dateiergebnis")
        };
        assert_eq!(stats.files, 1, "{kind:?}");
        assert_eq!(
            target.state.lock().unwrap().tree.as_ref().unwrap()["/vault/pane/Kind.txt"].1,
            b"Bauminhalt\n"
        );
        assert_eq!(
            source
                .state
                .lock()
                .unwrap()
                .tree
                .as_ref()
                .unwrap()
                .contains_key("/vault/pane/Baum/Kind.txt"),
            kind == FileQueueKind::Copy
        );
    }
}

#[test]
fn sftp_stream_pakete_laden_keine_profile_oder_elternpfade_nach_und_commit_prueft_profil() {
    let mut metadata_counts = Vec::new();
    for bytes in [128 * 1024, 4 * 1024 * 1024] {
        let server = Server::tree();
        server.state.lock().unwrap().allow_writes = true;
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let source = root.join("quelle");
        std::fs::create_dir(&source).unwrap();
        let path = source.join("Stream.bin");
        std::fs::write(&path, vec![0x67; bytes]).unwrap();
        let config = private_config(&root);
        let binding = save_bound_sftp_profile(&config, "ziel", &server.config);
        let profiles = config.join("sftp-profiles.json");
        let saved = std::fs::read(&profiles).unwrap();
        let queue = OperationQueue::open(config.join("queue.json")).unwrap();
        let entry = queue
            .enqueue_claimed_with_profile_bindings(
                interactive_transfer_operation(
                    BidirectionalQueueEndpoint::Local {
                        root: source.to_string_lossy().into_owned(),
                    },
                    BidirectionalQueueEndpoint::Sftp {
                        connection: Box::new(server.config.clone()),
                        root: "/pane".into(),
                    },
                    path.to_str().unwrap(),
                    FileQueueKind::Copy,
                ),
                vec![binding],
            )
            .unwrap();
        let mut transient = false;
        let result = queue::execute_provider_file_queue_entry(
            &queue,
            entry,
            &execution(&root),
            Arc::new(FakeSecrets),
            &Cancel::new(),
            &mut |progress| {
                if progress.bytes_done > 0 && progress.bytes_done < bytes as u64 && !transient {
                    std::fs::write(&profiles, b"voruebergehend unlesbares Profil").unwrap();
                    transient = true;
                }
                if progress.bytes_done >= bytes as u64 {
                    std::fs::write(&profiles, &saved).unwrap();
                }
            },
        )
        .unwrap();
        assert!(matches!(result, QueueExecutionResult::File(_)));
        assert_eq!(transient, bytes > 128 * 1024);
        let state = server.state.lock().unwrap();
        assert_eq!(
            state.tree.as_ref().unwrap()["/vault/pane/Stream.bin"]
                .1
                .len(),
            bytes
        );
        metadata_counts.push(state.metadata_requests);
    }
    assert_eq!(
        metadata_counts[0], metadata_counts[1],
        "Mehr Pakete dürfen keine zusätzlichen Eltern-LSTAT auslösen"
    );

    let server = Server::tree();
    server.state.lock().unwrap().allow_writes = true;
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let source = root.join("quelle");
    std::fs::create_dir(&source).unwrap();
    let path = source.join("Drift.bin");
    std::fs::write(&path, vec![0x68; 256 * 1024]).unwrap();
    let config = private_config(&root);
    let binding = save_bound_sftp_profile(&config, "ziel", &server.config);
    let queue = OperationQueue::open(config.join("queue.json")).unwrap();
    let entry = queue
        .enqueue_claimed_with_profile_bindings(
            interactive_transfer_operation(
                BidirectionalQueueEndpoint::Local {
                    root: source.to_string_lossy().into_owned(),
                },
                BidirectionalQueueEndpoint::Sftp {
                    connection: Box::new(server.config.clone()),
                    root: "/pane".into(),
                },
                path.to_str().unwrap(),
                FileQueueKind::Copy,
            ),
            vec![binding],
        )
        .unwrap();
    let mut changed = false;
    let error = queue::execute_provider_file_queue_entry(
        &queue,
        entry,
        &execution(&root),
        Arc::new(FakeSecrets),
        &Cancel::new(),
        &mut |progress| {
            if progress.bytes_done > 0 && !changed {
                save_bound_sftp_profile(&config, "ziel", &server.config);
                changed = true;
            }
        },
    )
    .unwrap_err();
    assert!(changed);
    assert!(matches!(
        error_code(&error),
        ErrorCode::PlanDrift | ErrorCode::OutcomeUnclear
    ));
    assert!(!server
        .state
        .lock()
        .unwrap()
        .tree
        .as_ref()
        .unwrap()
        .contains_key("/vault/pane/Drift.bin"));
}

#[test]
fn sftp_abbruch_waehrend_write_all_wiederholt_interrupted_nicht() {
    let server = Server::tree();
    let cancel = Cancel::new();
    {
        let mut state = server.state.lock().unwrap();
        state.allow_writes = true;
        state.cancel_on_write = Some(cancel.clone());
    }
    let target = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let path = root.join("Abbruch.bin");
    std::fs::write(&path, vec![0x69; 128 * 1024]).unwrap();
    let source = ProviderEndpoint::local(&root).unwrap();
    let contexts = source
        .interactive_file_contexts(&target, std::slice::from_ref(&path), false, false, &cancel)
        .unwrap();
    let error = fileops::copy_items(
        fileops::FileOperationRequest {
            src: contexts.source_vfs.as_ref(),
            dst: contexts.target_vfs.as_ref(),
            items: &[path],
            dst_dir: Path::new("/pane"),
            policy: OverwritePolicy::Skip,
            sandbox: &Some(root.clone()),
        },
        &cancel,
        &mut |_| {},
    )
    .unwrap_err();
    assert!(matches!(
        error_code(&error),
        ErrorCode::Interrupted | ErrorCode::OutcomeUnclear
    ));
    let state = server.state.lock().unwrap();
    assert_eq!(
        state.mutations, 2,
        "genau OPEN und ein WRITE; kein Interrupted-Retry"
    );
    assert!(!state
        .tree
        .as_ref()
        .unwrap()
        .contains_key("/vault/pane/Abbruch.bin"));
}

#[test]
fn sftp_verlorene_create_antwort_markiert_sitzung_ohne_mutationsretry() {
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        state.allow_writes = true;
        state.lose_mutation_response = true;
    }
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let (context, _) = endpoint.ordinary_file_context().unwrap();
    let error = match context.open_write_new(Path::new("/pane/Temp.bin")) {
        Ok(_) => panic!("Antwortverlust akzeptiert"),
        Err(error) => error,
    };
    assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    assert_eq!(server.state.lock().unwrap().mutations, 1);
    server.state.lock().unwrap().lose_mutation_response = false;
    endpoint.ordinary_file_context().unwrap();
    assert_eq!(provider.session_generation(), 2);
    assert_eq!(server.state.lock().unwrap().mutations, 1);
    assert!(server
        .state
        .lock()
        .unwrap()
        .tree
        .as_ref()
        .unwrap()
        .contains_key("/vault/pane/Temp.bin"));
}

#[test]
fn interaktiver_sftp_leser_erkennt_attributdrift_vor_read_und_am_ende() {
    for (after_read, path_only) in [(false, false), (true, false), (false, true), (true, true)] {
        let server = Server::tree();
        let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane/Baum")).unwrap();
        let (source, _) = endpoint.ordinary_file_context().unwrap();
        let path = Path::new("/pane/Baum/Kind.txt");
        let mut reader = source.open_read(path).unwrap();
        if after_read {
            let mut bytes = vec![0; b"Bauminhalt\n".len()];
            reader.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, b"Bauminhalt\n");
        }
        {
            let mut state = server.state.lock().unwrap();
            if !path_only {
                state.mtime += 1;
            }
            let (attributes, data) = state
                .tree
                .as_mut()
                .unwrap()
                .get_mut("/vault/pane/Baum/Kind.txt")
                .unwrap();
            attributes.mtime = Some(123457);
            data.fill(b'X');
        }
        let error = reader.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(
            error_code(&error),
            ErrorCode::PlanDrift,
            "after_read={after_read}, path_only={path_only}"
        );
        if !after_read {
            assert_eq!(server.state.lock().unwrap().bytes_read, 0);
        }
    }
}

#[test]
fn sftp_ersetzungsprobe_zeigt_remove_vor_rename_auch_ohne_ftps() {
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        let tree = state.tree.as_mut().unwrap();
        tree.insert("/vault/ziel".into(), tree["/vault/pane"].clone());
        tree.insert(
            "/vault/ziel/Kind.txt".into(),
            tree["/vault/pane/Baum/Kind.txt"].clone(),
        );
    }
    let remote = ProviderEndpoint::sftp(server.connect(), Path::new("/pane/Baum")).unwrap();
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("Kind.txt"), b"Neue Fassung").unwrap();
    let local = ProviderEndpoint::local(&source.path().canonicalize().unwrap()).unwrap();
    for (endpoint, items) in [
        (
            &local,
            vec![source.path().canonicalize().unwrap().join("Kind.txt")],
        ),
        (&remote, vec![PathBuf::from("/pane/Baum/Kind.txt")]),
    ] {
        let target = ProviderEndpoint::sftp(server.connect(), Path::new("/ziel")).unwrap();
        let preparation = endpoint
            .probe_interactive_file_target(ProviderFileTargetProbe {
                target: &target,
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
        assert!(preparation
            .capabilities
            .unwrap()
            .limitations
            .iter()
            .any(|note| note.contains("Altziel wird vor dem Commit entfernt")));
    }
}

#[test]
fn sftp_direkte_aliasgruppen_lassen_good_weiter_uebertragbar() {
    for (left, right) in [("Name", "name"), ("é.txt", "e\u{301}.txt")] {
        let server = Server::tree();
        for (name, bytes) in [
            (left, b"left".to_vec()),
            (right, b"right".to_vec()),
            ("good.txt", b"good".to_vec()),
        ] {
            tree_file(&server, &format!("/vault/pane/{name}"), bytes);
        }
        let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
        let items = [left, right, "good.txt"].map(|name| PathBuf::from(format!("/pane/{name}")));
        let (source, _) = endpoint
            .interactive_file_read_context(&items, &Cancel::new())
            .unwrap();
        let summary = source
            .interactive_source_tree()
            .unwrap()
            .expect("Auslassungen brauchen eine Bilanz");
        assert_eq!(
            (
                summary.entries,
                summary.files,
                summary.directories,
                summary.omitted.len()
            ),
            (3, 1, 0, 2)
        );
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let target = LocalVfs::rooted(&root).unwrap();
        let stats = copy_items(
            FileOperationRequest {
                src: source.as_ref(),
                dst: &target,
                items: &items,
                dst_dir: &root,
                policy: OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!((stats.files, stats.skipped), (1, 2));
        assert_eq!(std::fs::read(root.join("good.txt")).unwrap(), b"good");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        let state = server.state.lock().unwrap();
        assert_eq!((state.opens, state.mutations), (1, 0));
    }
}

#[test]
fn sftp_direkte_aliasordner_werden_nicht_gelistet_auch_wenn_alle_ausgelassen_sind() {
    for include_good in [false, true] {
        let server = Server::tree();
        tree_file(&server, "/vault/pane/Name", b"file".to_vec());
        {
            let mut state = server.state.lock().unwrap();
            let directory = attrs(&state, false);
            state
                .tree
                .as_mut()
                .unwrap()
                .insert("/vault/pane/name".into(), (directory, Vec::new()));
            state.fault = Fault::ListingDenied;
        }
        tree_file(&server, "/vault/pane/name/Kind", b"nicht lesen".to_vec());
        tree_file(&server, "/vault/pane/good.txt", b"good".to_vec());
        let mut items = vec![PathBuf::from("/pane/Name"), PathBuf::from("/pane/name")];
        if include_good {
            items.push(PathBuf::from("/pane/good.txt"));
        }
        let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
        let (source, _) = endpoint
            .interactive_file_read_context(&items, &Cancel::new())
            .unwrap();
        let summary = source.interactive_source_tree().unwrap().unwrap();
        assert_eq!(
            (
                summary.entries,
                summary.files,
                summary.directories,
                summary.omitted.len()
            ),
            (
                if include_good { 3 } else { 2 },
                u64::from(include_good),
                0,
                2
            )
        );
        assert!(source.stat(Path::new("/pane/name/Kind")).is_err());
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let target = LocalVfs::rooted(&root).unwrap();
        let stats = copy_items(
            FileOperationRequest {
                src: source.as_ref(),
                dst: &target,
                items: &items,
                dst_dir: &root,
                policy: OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!((stats.files, stats.skipped), (u64::from(include_good), 2));
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            usize::from(include_good)
        );
        let state = server.state.lock().unwrap();
        assert_eq!(
            (state.opens, state.mutations),
            (usize::from(include_good), 0)
        );
    }
}

#[test]
fn sftp_direkte_exakte_doppelauswahl_bleibt_ungueltig_vor_providerzugriff() {
    let server = Server::tree();
    let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
    let before = server.state.lock().unwrap().metadata_requests;
    let error = endpoint
        .interactive_file_read_context(
            &[PathBuf::from("/pane/Baum"), PathBuf::from("/pane/Baum")],
            &Cancel::new(),
        )
        .err()
        .unwrap();
    assert_eq!(error_code(&error), ErrorCode::InvalidInput);
    let state = server.state.lock().unwrap();
    assert_eq!(
        (state.metadata_requests, state.opens, state.mutations),
        (before, 0, 0)
    );
}

#[test]
fn sftp_direkte_omission_drift_nach_erstem_commit_stoppt_good2() {
    let server = Server::tree();
    for (name, bytes) in [
        ("Name", b"left".to_vec()),
        ("name", b"right".to_vec()),
        ("good1", b"first".to_vec()),
        ("good2", b"second".to_vec()),
    ] {
        tree_file(&server, &format!("/vault/pane/{name}"), bytes);
    }
    let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
    let items =
        ["Name", "name", "good1", "good2"].map(|name| PathBuf::from(format!("/pane/{name}")));
    let (source, _) = endpoint
        .interactive_file_read_context(&items, &Cancel::new())
        .unwrap();
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = LocalVfs::rooted(&root).unwrap();
    let mut changed = false;
    let result = copy_items(
        FileOperationRequest {
            src: source.as_ref(),
            dst: &target,
            items: &items,
            dst_dir: &root,
            policy: OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut |progress| {
            if !changed && progress.files_done == 1 {
                changed = true;
                server
                    .state
                    .lock()
                    .unwrap()
                    .tree
                    .as_mut()
                    .unwrap()
                    .get_mut("/vault/pane/Name")
                    .unwrap()
                    .0
                    .uid = Some(2000);
            }
        },
    );
    assert!(changed);
    assert!(result.is_err());
    assert_eq!(std::fs::read(root.join("good1")).unwrap(), b"first");
    assert!(!root.join("good2").exists());
    assert!(source.interactive_source_tree().is_err());
    let state = server.state.lock().unwrap();
    assert_eq!((state.opens, state.mutations), (1, 0));
}

#[test]
fn sftp_direkte_omission_uid_und_mtime_drift_nach_queue_reopen_brauchen_neue_bestaetigung() {
    for uid in [false, true] {
        let server = Server::tree();
        for name in ["Name", "name", "good"] {
            tree_file(&server, &format!("/vault/pane/{name}"), b"data".to_vec());
        }
        let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
        let items = ["Name", "name", "good"].map(|name| PathBuf::from(format!("/pane/{name}")));
        let (source, _) = endpoint
            .interactive_file_read_context(&items, &Cancel::new())
            .unwrap();
        let expected = source.interactive_source_tree().unwrap();
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let target = root.join("target");
        std::fs::create_dir(&target).unwrap();
        let config = private_config(&root);
        let journal = config.join("queue.json");
        let queue = OperationQueue::open(&journal).unwrap();
        let mut op = tree_operation(&server, &target, expected);
        if let QueueOperation::ProviderFile {
            items: selected, ..
        } = &mut op
        {
            *selected = items
                .iter()
                .map(|path| path.to_str().unwrap().to_owned())
                .collect();
        }
        queue.enqueue(op).unwrap();
        drop(queue);
        {
            let mut state = server.state.lock().unwrap();
            let attributes = &mut state
                .tree
                .as_mut()
                .unwrap()
                .get_mut("/vault/pane/Name")
                .unwrap()
                .0;
            if uid {
                attributes.uid = Some(2000);
            } else {
                attributes.mtime = Some(attributes.mtime.unwrap() + 1);
            }
        }
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
        assert!(matches!(
            result,
            QueueExecutionResult::AwaitingConfirmation { .. }
        ));
        assert!(matches!(
            queue.snapshot().entries[0].attention,
            Some(QueueAttention::SourceTreeChanged { .. })
        ));
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
        let state = server.state.lock().unwrap();
        assert_eq!((state.opens, state.mutations), (0, 0));
    }
}

#[test]
fn sftp_direkte_aliasgruppe_laesst_unabhaengigen_ordner_mit_gleichnamigem_kind_zu() {
    let server = Server::tree();
    for name in ["Name", "name"] {
        tree_file(&server, &format!("/vault/pane/{name}"), b"omit".to_vec());
    }
    tree_file(&server, "/vault/pane/Baum/Name", b"unabhaengig".to_vec());
    let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
    let items = ["Name", "name", "Baum"].map(|name| PathBuf::from(format!("/pane/{name}")));
    let (source, _) = endpoint
        .interactive_file_read_context(&items, &Cancel::new())
        .unwrap();
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    let target = LocalVfs::rooted(&root).unwrap();
    let stats = copy_items(
        FileOperationRequest {
            src: source.as_ref(),
            dst: &target,
            items: &items,
            dst_dir: &root,
            policy: OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((stats.files, stats.skipped), (2, 2));
    assert_eq!(
        std::fs::read(root.join("Baum/Name")).unwrap(),
        b"unabhaengig"
    );
    assert!(root.join("Baum/Leer").is_dir());
    assert!(!root.join("Name").exists());
    assert!(!root.join("name").exists());
    let state = server.state.lock().unwrap();
    assert_eq!((state.opens, state.mutations), (2, 0));
}

#[test]
fn sftp_direkte_omission_bleibt_nach_datei_eof_vor_commit_gebunden() {
    let server = Server::tree();
    for name in ["Name", "name", "good"] {
        tree_file(&server, &format!("/vault/pane/{name}"), b"data".to_vec());
    }
    let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
    let items = ["Name", "name", "good"].map(|name| PathBuf::from(format!("/pane/{name}")));
    let (source, _) = endpoint
        .interactive_file_read_context(&items, &Cancel::new())
        .unwrap();
    let mut bytes = Vec::new();
    source
        .open_read(Path::new("/pane/good"))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let hash = blake3::hash(&bytes);
    source
        .verify_transferred_source(Path::new("/pane/good"), &hash)
        .unwrap();
    server
        .state
        .lock()
        .unwrap()
        .tree
        .as_mut()
        .unwrap()
        .get_mut("/vault/pane/Name")
        .unwrap()
        .0
        .uid = Some(2000);
    assert_eq!(
        error_code(
            &source
                .verify_transferred_source(Path::new("/pane/good"), &hash)
                .unwrap_err()
        ),
        ErrorCode::PlanDrift
    );
    let state = server.state.lock().unwrap();
    assert_eq!((state.opens, state.mutations), (1, 0));
}

#[test]
fn sftp_stapel_antwortverlust_mutiert_nur_einmal_und_stoppt_den_rest() {
    use doppeldecker_core::sftp_batch_rename::PreparedSftpBatchRename;
    let server = Server::tree();
    for name in ["a", "b"] {
        tree_file(&server, &format!("/vault/pane/{name}"), b"content".to_vec());
    }
    server.state.lock().unwrap().allow_writes = true;
    let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
    let options = doppeldecker_core::batch_rename::BatchRenameOptions {
        find: "".into(),
        replacement: "".into(),
        prefix: "new-".into(),
        suffix: "".into(),
        preserve_extension: true,
        trim_outer_spaces: false,
        numbering: None,
    };
    let plan = PreparedSftpBatchRename::prepare(
        &endpoint,
        &["a".into(), "b".into()],
        &options,
        &Cancel::new(),
    )
    .unwrap();
    server.state.lock().unwrap().lose_mutation_response = true;
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    assert_eq!(
        (
            report.renamed,
            report.outcome_unclear,
            report.failed_unchanged
        ),
        (0, 1, 1)
    );
    let state = server.state.lock().unwrap();
    assert_eq!((state.mutations, state.opens), (1, 0));
    assert!(state
        .tree
        .as_ref()
        .unwrap()
        .contains_key("/vault/pane/new-a"));
    assert!(state.tree.as_ref().unwrap().contains_key("/vault/pane/b"));
}

fn sftp_stapel_plan(
    server: &Server,
    names: &[&str],
) -> (
    Arc<SftpVfs>,
    doppeldecker_core::sftp_batch_rename::PreparedSftpBatchRename,
) {
    server.state.lock().unwrap().allow_writes = true;
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let options = doppeldecker_core::batch_rename::BatchRenameOptions {
        find: "".into(),
        replacement: "".into(),
        prefix: "new-".into(),
        suffix: "".into(),
        preserve_extension: true,
        trim_outer_spaces: false,
        numbering: None,
    };
    let plan = doppeldecker_core::sftp_batch_rename::PreparedSftpBatchRename::prepare(
        &endpoint,
        &names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
        &options,
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(plan.execution_class(), RenameExecutionClass::BestEffort);
    assert!(!plan.limitations().is_empty());
    (provider, plan)
}
fn sftp_stapel_dateien(server: &Server) {
    for name in ["a", "b", "c"] {
        tree_file(
            server,
            &format!("/vault/pane/{name}"),
            name.as_bytes().to_vec(),
        );
    }
}
#[test]
fn sftp_stapel_geklaerte_spaete_kollision_laesst_unabhaengige_zeile_weiterlaufen() {
    let server = Server::tree();
    sftp_stapel_dateien(&server);
    let (_, plan) = sftp_stapel_plan(&server, &["a", "b", "c"]);
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |done, _| {
        if done == 1 {
            tree_file(&server, "/vault/pane/new-b", b"bestehend".to_vec());
        }
    });
    assert_eq!(
        (
            report.renamed,
            report.failed_unchanged,
            report.outcome_unclear
        ),
        (2, 1, 0)
    );
    assert_eq!(report.rows[1].code, Some(ErrorCode::AlreadyExists));
    let state = server.state.lock().unwrap();
    assert_eq!((state.mutations, state.opens), (2, 0));
    let tree = state.tree.as_ref().unwrap();
    assert!(tree.contains_key("/vault/pane/new-a"));
    assert!(tree.contains_key("/vault/pane/b"));
    assert!(tree.contains_key("/vault/pane/new-c"));
    assert_eq!(tree["/vault/pane/new-b"].1, b"bestehend");
}
#[test]
fn sftp_stapel_antwortverlust_nach_erstem_erfolg_startet_dritte_zeile_nicht() {
    let server = Server::tree();
    sftp_stapel_dateien(&server);
    let (_, plan) = sftp_stapel_plan(&server, &["a", "b", "c"]);
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |done, _| {
        if done == 1 {
            server.state.lock().unwrap().lose_mutation_response = true;
        }
    });
    assert_eq!(
        (
            report.renamed,
            report.outcome_unclear,
            report.failed_unchanged
        ),
        (1, 1, 1)
    );
    assert!(report.rows[1].message.contains("b"));
    assert!(report.rows[1].message.contains("new-b"));
    let state = server.state.lock().unwrap();
    assert_eq!((state.mutations, state.opens), (2, 0));
    let tree = state.tree.as_ref().unwrap();
    assert!(tree.contains_key("/vault/pane/new-a"));
    assert!(tree.contains_key("/vault/pane/new-b"));
    assert!(tree.contains_key("/vault/pane/c"));
}
#[test]
fn sftp_stapel_cancel_vor_start_und_nach_erster_wirkung_ist_exakt() {
    for before in [true, false] {
        let server = Server::tree();
        sftp_stapel_dateien(&server);
        let (_, plan) = sftp_stapel_plan(&server, &["a", "b", "c"]);
        let cancel = Cancel::new();
        if before {
            cancel.cancel();
        }
        let report = plan.execute(&cancel, &mut || Ok(()), &mut |done, _| {
            if done == 1 {
                cancel.cancel();
            }
        });
        assert_eq!(
            (report.renamed, report.cancelled),
            if before { (0, 3) } else { (1, 2) }
        );
        let state = server.state.lock().unwrap();
        assert_eq!((state.mutations, state.opens), (usize::from(!before), 0));
        assert!(state.tree.as_ref().unwrap().contains_key("/vault/pane/b"));
    }
}
#[test]
fn sftp_stapel_quell_eltern_root_und_sitzungsdrift_stoppen_vor_weiterer_wirkung() {
    for change in 0..6 {
        let server = Server::tree();
        sftp_stapel_dateien(&server);
        let (provider, plan) = sftp_stapel_plan(&server, &["a", "b", "c"]);
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |done, _| {
            if done == 1 {
                if change == 5 {
                    provider.reconnect().unwrap();
                } else {
                    let mut state = server.state.lock().unwrap();
                    let tree = state.tree.as_mut().unwrap();
                    match change {
                        0 => tree.get_mut("/vault/pane/b").unwrap().0.uid = Some(2000),
                        1 => tree.get_mut("/vault/pane/b").unwrap().0.mtime = Some(999),
                        2 => tree.get_mut("/vault/pane").unwrap().0.uid = Some(2000),
                        3 => tree.get_mut("/vault").unwrap().0.mtime = Some(999),
                        _ => tree.get_mut("/vault/pane").unwrap().0.mtime = Some(999),
                    }
                }
            }
        });
        assert_eq!(
            (
                report.renamed,
                report.failed_unchanged,
                report.outcome_unclear
            ),
            (1, 2, 0),
            "change={change}"
        );
        assert_eq!(report.rows[1].code, Some(ErrorCode::PlanDrift));
        let state = server.state.lock().unwrap();
        assert_eq!((state.mutations, state.opens), (1, 0));
        assert!(state.tree.as_ref().unwrap().contains_key("/vault/pane/b"));
        assert!(state.tree.as_ref().unwrap().contains_key("/vault/pane/c"));
    }
}
#[test]
fn sftp_stapel_profil_guard_stoppt_rest_ohne_provider_mutation() {
    let server = Server::tree();
    sftp_stapel_dateien(&server);
    let (_, plan) = sftp_stapel_plan(&server, &["a", "b", "c"]);
    let mut calls = 0;
    let report = plan.execute(
        &Cancel::new(),
        &mut || {
            calls += 1;
            if calls == 2 {
                Err(doppeldecker_core::coded_error(
                    io::ErrorKind::Interrupted,
                    ErrorCode::PlanDrift,
                    "Profilrevision geändert",
                ))
            } else {
                Ok(())
            }
        },
        &mut |_, _| {},
    );
    assert_eq!((report.renamed, report.failed_unchanged, calls), (1, 2, 2));
    assert_eq!(server.state.lock().unwrap().mutations, 1);
}
#[test]
fn sftp_stapel_namensgruppen_blockieren_ohne_gewinner_und_unabhaengige_bleiben() {
    for (left, right) in [("a", "A"), ("é.txt", "e\u{301}.txt")] {
        let server = Server::tree();
        for name in [left, right, "good"] {
            tree_file(&server, &format!("/vault/pane/{name}"), b"same".to_vec());
        }
        let (_, plan) = sftp_stapel_plan(&server, &[left, right, "good"]);
        use doppeldecker_core::batch_rename::BatchRenameStatus;
        assert!(plan.rows()[..2]
            .iter()
            .all(|row| row.status == BatchRenameStatus::Blocked));
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
        assert_eq!(report.renamed, 1);
        let state = server.state.lock().unwrap();
        assert_eq!((state.mutations, state.opens), (1, 0));
        let tree = state.tree.as_ref().unwrap();
        assert!(tree.contains_key(&format!("/vault/pane/{left}")));
        assert!(tree.contains_key(&format!("/vault/pane/{right}")));
        assert!(tree.contains_key("/vault/pane/new-good"));
    }
}
#[test]
fn sftp_stapel_dateiendung_ordner_und_grosse_datei_brauchen_keine_inhaltsreads() {
    let server = Server::tree();
    tree_file(&server, "/vault/pane/Grüße %2F.txt", b"unchanged".to_vec());
    {
        server
            .state
            .lock()
            .unwrap()
            .tree
            .as_mut()
            .unwrap()
            .get_mut("/vault/pane/Grüße %2F.txt")
            .unwrap()
            .0
            .size = Some(9 * 1024 * 1024 * 1024);
    }
    let (_, plan) = sftp_stapel_plan(&server, &["Grüße %2F.txt", "Baum"]);
    assert_eq!(plan.rows()[0].new_name, "new-Grüße %2F.txt");
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    assert_eq!(report.renamed, 2);
    let state = server.state.lock().unwrap();
    assert_eq!((state.mutations, state.opens, state.bytes_read), (2, 0, 0));
    let tree = state.tree.as_ref().unwrap();
    assert!(tree.contains_key("/vault/pane/new-Baum/Kind.txt"));
    assert!(tree.contains_key("/vault/pane/new-Baum/Leer"));
}
#[test]
fn sftp_stapel_unvollstaendige_listings_und_ungueltige_optionen_haben_keine_wirkung() {
    use doppeldecker_core::sftp_batch_rename::PreparedSftpBatchRename;
    for fault in [
        Fault::ListingDenied,
        Fault::TreeEmptyPage,
        Fault::TreeDuplicate,
    ] {
        let server = Server::tree();
        sftp_stapel_dateien(&server);
        let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
        let options = doppeldecker_core::batch_rename::BatchRenameOptions {
            find: "".into(),
            replacement: "".into(),
            prefix: "new-".into(),
            suffix: "".into(),
            preserve_extension: true,
            trim_outer_spaces: false,
            numbering: None,
        };
        server.state.lock().unwrap().fault = fault;
        assert!(
            PreparedSftpBatchRename::prepare(&endpoint, &["a".into()], &options, &Cancel::new())
                .is_err(),
            "{fault:?}"
        );
        let state = server.state.lock().unwrap();
        assert_eq!((state.mutations, state.opens), (0, 0));
    }
    let server = Server::tree();
    let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
    let before = server.state.lock().unwrap().metadata_requests;
    let options = doppeldecker_core::batch_rename::BatchRenameOptions {
        find: "".into(),
        replacement: "".into(),
        prefix: "x".repeat(4097),
        suffix: "".into(),
        preserve_extension: true,
        trim_outer_spaces: false,
        numbering: None,
    };
    assert!(
        PreparedSftpBatchRename::prepare(&endpoint, &["a".into()], &options, &Cancel::new())
            .is_err()
    );
    assert_eq!(server.state.lock().unwrap().metadata_requests, before);
}

#[test]
fn sftp_stapel_eigene_rootzeit_wird_uebernommen_fremde_rechte_nicht() {
    for foreign_uid in [false, true] {
        let server = Server::tree();
        sftp_stapel_dateien(&server);
        let (_, plan) = sftp_stapel_plan(&server, &["a", "b", "c"]);
        {
            let mut state = server.state.lock().unwrap();
            state.rename_parent_mtime = true;
            state.rename_parent_uid = foreign_uid;
        }
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
        assert_eq!(
            (report.renamed, report.failed_unchanged),
            if foreign_uid { (1, 2) } else { (3, 0) }
        );
        let state = server.state.lock().unwrap();
        assert_eq!(state.mutations, if foreign_uid { 1 } else { 3 });
        assert_eq!(state.opens, 0);
    }
}

fn sftp_ketten_plan(
    server: &Server,
) -> (
    Arc<SftpVfs>,
    doppeldecker_core::sftp_batch_rename::PreparedSftpBatchRename,
) {
    for name in ["a", "aa", "aaa", "b"] {
        tree_file(
            server,
            &format!("/vault/pane/{name}"),
            name.as_bytes().to_vec(),
        );
    }
    server.state.lock().unwrap().allow_writes = true;
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let options = doppeldecker_core::batch_rename::BatchRenameOptions {
        find: "".into(),
        replacement: "".into(),
        prefix: "a".into(),
        suffix: "".into(),
        preserve_extension: true,
        trim_outer_spaces: false,
        numbering: None,
    };
    let plan = doppeldecker_core::sftp_batch_rename::PreparedSftpBatchRename::prepare(
        &endpoint,
        &["a".into(), "aa".into(), "aaa".into(), "b".into()],
        &options,
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(plan.execution_class(), RenameExecutionClass::BestEffort);
    assert!(!plan.limitations().is_empty());
    (provider, plan)
}

#[test]
fn sftp_kette_spaete_kollision_gibt_abhaengige_namen_nicht_frei() {
    use doppeldecker_core::batch_rename::BatchRenameStatus;
    let server = Server::tree();
    let (_, plan) = sftp_ketten_plan(&server);
    assert!(plan
        .rows()
        .iter()
        .all(|row| row.status == BatchRenameStatus::Ready));
    tree_file(&server, "/vault/pane/aaaa", b"fremd".to_vec());
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    assert_eq!(
        (
            report.renamed,
            report.failed_unchanged,
            report.outcome_unclear
        ),
        (1, 3, 0)
    );
    assert_eq!(report.rows[2].code, Some(ErrorCode::AlreadyExists));
    assert!(report.rows[..2]
        .iter()
        .all(|row| row.message.contains("nicht nachweislich freigemacht")));
    let state = server.state.lock().unwrap();
    assert_eq!((state.mutations, state.opens), (1, 0));
    let tree = state.tree.as_ref().unwrap();
    for name in ["a", "aa", "aaa"] {
        assert_eq!(tree[&format!("/vault/pane/{name}")].1, name.as_bytes());
    }
    assert_eq!(tree["/vault/pane/aaaa"].1, b"fremd");
    assert_eq!(tree["/vault/pane/ab"].1, b"b");
}

#[test]
fn sftp_kette_erfolg_ordnet_nur_ausfuehrung_und_uebernimmt_eigene_elternzeit() {
    let server = Server::tree();
    let (_, plan) = sftp_ketten_plan(&server);
    assert_eq!(
        plan.rows()
            .iter()
            .map(|row| row.old_name.as_str())
            .collect::<Vec<_>>(),
        ["a", "aa", "aaa", "b"]
    );
    assert_eq!(
        plan.rows()
            .iter()
            .map(|row| row.new_name.as_str())
            .collect::<Vec<_>>(),
        ["aa", "aaa", "aaaa", "ab"]
    );
    server.state.lock().unwrap().rename_parent_mtime = true;
    let mut snapshots = Vec::new();
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {
        let state = server.state.lock().unwrap();
        let tree = state.tree.as_ref().unwrap();
        snapshots.push(
            ["a", "aa", "aaa", "aaaa", "ab"]
                .map(|name| tree.contains_key(&format!("/vault/pane/{name}"))),
        );
    });
    assert_eq!(
        (
            report.renamed,
            report.failed_unchanged,
            report.outcome_unclear
        ),
        (4, 0, 0)
    );
    assert_eq!(
        snapshots,
        [
            [true, true, false, true, false],
            [true, true, false, true, true],
            [true, false, true, true, true],
            [false, true, true, true, true]
        ]
    );
    let state = server.state.lock().unwrap();
    assert_eq!((state.mutations, state.opens, state.bytes_read), (4, 0, 0));
    let tree = state.tree.as_ref().unwrap();
    for (old, new) in [("a", "aa"), ("aa", "aaa"), ("aaa", "aaaa"), ("b", "ab")] {
        assert_eq!(tree[&format!("/vault/pane/{new}")].1, old.as_bytes());
    }
}

#[test]
fn sftp_kette_antwortverlust_stoppt_abhaengige_und_unabhaengige_ohne_retry() {
    let server = Server::tree();
    let (_, plan) = sftp_ketten_plan(&server);
    server.state.lock().unwrap().lose_mutation_response = true;
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    assert_eq!(
        (
            report.renamed,
            report.outcome_unclear,
            report.failed_unchanged
        ),
        (0, 1, 3)
    );
    assert_eq!(report.rows[2].code, Some(ErrorCode::OutcomeUnclear));
    let state = server.state.lock().unwrap();
    assert_eq!((state.mutations, state.opens), (1, 0));
    let tree = state.tree.as_ref().unwrap();
    assert!(!tree.contains_key("/vault/pane/aaa"));
    assert_eq!(tree["/vault/pane/aaaa"].1, b"aaa");
    for name in ["a", "aa", "b"] {
        assert_eq!(tree[&format!("/vault/pane/{name}")].1, name.as_bytes());
    }
}

#[test]
fn sftp_kette_fremde_bindungsdrift_stoppt_vor_naechster_wirkung() {
    for change in 0..7 {
        let server = Server::tree();
        let (provider, plan) = sftp_ketten_plan(&server);
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |done, _| {
            if done != 1 {
                return;
            }
            if change == 6 {
                provider.reconnect().unwrap();
            } else {
                let mut state = server.state.lock().unwrap();
                let tree = state.tree.as_mut().unwrap();
                match change {
                    0 => tree.get_mut("/vault/pane/b").unwrap().0.uid = Some(2000),
                    1 => tree.get_mut("/vault/pane/b").unwrap().0.mtime = Some(999),
                    2 => tree.get_mut("/vault/pane").unwrap().0.permissions = Some(0o040755),
                    3 => tree.get_mut("/vault/pane").unwrap().0.uid = Some(2000),
                    4 => tree.get_mut("/vault").unwrap().0.mtime = Some(999),
                    _ => tree.get_mut("/vault/pane").unwrap().0.mtime = Some(999),
                }
            }
        });
        assert_eq!(
            (
                report.renamed,
                report.failed_unchanged,
                report.outcome_unclear
            ),
            (1, 3, 0),
            "change={change}"
        );
        assert_eq!(
            report.rows[3].code,
            Some(ErrorCode::PlanDrift),
            "change={change}"
        );
        let state = server.state.lock().unwrap();
        assert_eq!((state.mutations, state.opens), (1, 0));
        let tree = state.tree.as_ref().unwrap();
        for name in ["a", "aa", "b"] {
            assert!(tree.contains_key(&format!("/vault/pane/{name}")));
        }
    }
}

#[test]
fn sftp_kette_fremder_owner_bei_eigenem_rename_wird_nicht_uebernommen() {
    let server = Server::tree();
    let (_, plan) = sftp_ketten_plan(&server);
    {
        let mut state = server.state.lock().unwrap();
        state.rename_parent_mtime = true;
        state.rename_parent_uid = true;
    }
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    assert_eq!((report.renamed, report.failed_unchanged), (1, 3));
    assert_eq!(server.state.lock().unwrap().mutations, 1);
}

#[test]
fn sftp_kette_abbruch_belaesst_bereits_bestaetigten_freigeber() {
    for before in [true, false] {
        let server = Server::tree();
        let (_, plan) = sftp_ketten_plan(&server);
        let cancel = Cancel::new();
        if before {
            cancel.cancel();
        }
        let report = plan.execute(&cancel, &mut || Ok(()), &mut |_, _| cancel.cancel());
        assert_eq!(
            (report.renamed, report.cancelled),
            if before { (0, 4) } else { (1, 3) }
        );
        let state = server.state.lock().unwrap();
        assert_eq!((state.mutations, state.opens), (usize::from(!before), 0));
        let tree = state.tree.as_ref().unwrap();
        assert_eq!(tree.contains_key("/vault/pane/aaaa"), !before);
        for name in ["a", "aa", "b"] {
            assert!(tree.contains_key(&format!("/vault/pane/{name}")));
        }
    }
}

#[test]
fn sftp_kette_nummerierung_bleibt_an_urspruengliche_auswahl_gebunden() {
    use doppeldecker_core::batch_rename::{
        BatchRenameNumberPosition, BatchRenameNumbering, BatchRenameOptions,
    };
    let server = Server::tree();
    for name in ["a", "1_a"] {
        tree_file(
            &server,
            &format!("/vault/pane/{name}"),
            name.as_bytes().to_vec(),
        );
    }
    server.state.lock().unwrap().allow_writes = true;
    let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
    let options = BatchRenameOptions {
        find: "".into(),
        replacement: "".into(),
        prefix: "".into(),
        suffix: "".into(),
        preserve_extension: true,
        trim_outer_spaces: false,
        numbering: Some(BatchRenameNumbering {
            start: 1,
            step: 1,
            min_digits: 1,
            position: BatchRenameNumberPosition::Before,
        }),
    };
    let plan = doppeldecker_core::sftp_batch_rename::PreparedSftpBatchRename::prepare(
        &endpoint,
        &["a".into(), "1_a".into()],
        &options,
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(plan.rows()[0].new_name, "1_a");
    assert_eq!(plan.rows()[1].new_name, "2_1_a");
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    assert_eq!(report.renamed, 2);
    let state = server.state.lock().unwrap();
    assert_eq!((state.mutations, state.opens), (2, 0));
    let tree = state.tree.as_ref().unwrap();
    assert_eq!(tree["/vault/pane/1_a"].1, b"a");
    assert_eq!(tree["/vault/pane/2_1_a"].1, b"1_a");
}

fn sftp_schreibweisen_plan(
    server: &Server,
) -> (
    Arc<SftpVfs>,
    doppeldecker_core::sftp_batch_rename::PreparedSftpBatchRename,
) {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::sftp_batch_rename::PreparedSftpBatchRename;
    for name in ["A-one", "A-two", "A-three"] {
        tree_file(
            server,
            &format!("/vault/pane/{name}"),
            name.as_bytes().to_vec(),
        );
    }
    server.state.lock().unwrap().allow_writes = true;
    let provider = server.connect();
    let endpoint = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let options = BatchRenameOptions {
        find: "A".into(),
        replacement: "a".into(),
        prefix: String::new(),
        suffix: String::new(),
        preserve_extension: false,
        trim_outer_spaces: false,
        numbering: None,
    };
    let plan = PreparedSftpBatchRename::prepare(
        &endpoint,
        &["A-one".into(), "A-two".into(), "A-three".into()],
        &options,
        &Cancel::new(),
    )
    .unwrap();
    assert!(plan
        .rows()
        .iter()
        .all(|row| row.status == BatchRenameStatus::Ready));
    (provider, plan)
}

#[test]
fn sftp_schreibweisen_unklarer_oder_falscher_abschluss_stoppt_den_rest() {
    for fault in [
        "success",
        "noop",
        "lost_response",
        "wrong_raw",
        "final_scan",
        "final_uid",
        "final_parent",
    ] {
        let server = Server::tree();
        let (_, plan) = sftp_schreibweisen_plan(&server);
        {
            let mut state = server.state.lock().unwrap();
            state.batch_spelling_fault = fault;
            state.lose_mutation_response = fault == "lost_response";
            state.rename_parent_mtime = true;
        }
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
        let state = server.state.lock().unwrap();
        if fault == "success" {
            assert_eq!(
                (
                    report.renamed,
                    report.failed_unchanged,
                    report.outcome_unclear
                ),
                (3, 0, 0)
            );
            assert_eq!((state.mutations, state.opens), (3, 0));
        } else {
            assert_eq!(
                (
                    report.renamed,
                    report.failed_unchanged,
                    report.outcome_unclear
                ),
                (0, 2, 1),
                "{fault}: {report:?}"
            );
            assert_eq!(
                report.rows[0].code,
                Some(ErrorCode::OutcomeUnclear),
                "{fault}"
            );
            assert_eq!((state.mutations, state.opens), (1, 0), "{fault}");
            assert!(state
                .tree
                .as_ref()
                .unwrap()
                .contains_key("/vault/pane/A-two"));
            assert!(state
                .tree
                .as_ref()
                .unwrap()
                .contains_key("/vault/pane/A-three"));
            if matches!(
                fault,
                "final_scan" | "final_uid" | "final_parent" | "wrong_raw"
            ) {
                assert_eq!(state.batch_spelling_listings, 2, "{fault}");
            }
            if fault == "lost_response" {
                assert_eq!(
                    state.batch_spelling_listings, 0,
                    "Verlorene Antwort bleibt ohne heilenden Abschlussscan unklar"
                );
            }
        }
        assert_eq!(
            state
                .tree
                .as_ref()
                .unwrap()
                .contains_key("/vault/pane/A-one"),
            fault == "noop"
        );
    }
}

#[test]
fn sftp_schreibweisen_abbruch_und_spaete_kollision_bleiben_begrenzt() {
    for scenario in ["cancel_before", "cancel_after", "collision"] {
        let server = Server::tree();
        let (_, plan) = sftp_schreibweisen_plan(&server);
        let cancel = Cancel::new();
        if scenario == "cancel_before" {
            cancel.cancel();
        }
        let mut calls = 0;
        let report = plan.execute(
            &cancel,
            &mut || {
                calls += 1;
                if scenario == "collision" && calls == 1 {
                    tree_file(&server, "/vault/pane/a-one", b"foreign".to_vec());
                }
                Ok(())
            },
            &mut |done, _| {
                if scenario == "cancel_after" && done == 1 {
                    cancel.cancel();
                }
            },
        );
        let state = server.state.lock().unwrap();
        match scenario {
            "cancel_before" => {
                assert_eq!((report.renamed, report.cancelled), (0, 3));
                assert_eq!(state.mutations, 0);
            }
            "cancel_after" => {
                assert_eq!((report.renamed, report.cancelled), (1, 2));
                assert_eq!(state.mutations, 1);
            }
            _ => {
                assert_eq!(
                    (
                        report.renamed,
                        report.failed_unchanged,
                        report.outcome_unclear
                    ),
                    (2, 1, 0)
                );
                assert_eq!(state.mutations, 2);
                assert_eq!(report.rows[0].code, Some(ErrorCode::AlreadyExists));
                assert_eq!(
                    state.tree.as_ref().unwrap()["/vault/pane/a-one"].1,
                    b"foreign"
                );
            }
        }
        assert_eq!(state.opens, 0);
    }
}

#[test]
fn sftp_schreibweisen_case_ordner_unicode_und_zweiter_alias() {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::sftp_batch_rename::PreparedSftpBatchRename;
    for (old, new, folder) in [
        ("Foo", "foo", false),
        ("Foo", "foo", true),
        ("café", "cafe\u{301}", false),
        ("cafe\u{301}", "café", false),
    ] {
        for alias in [false, true] {
            let server = Server::tree();
            if folder {
                let mut state = server.state.lock().unwrap();
                let directory = state.tree.as_ref().unwrap()["/vault/pane/Baum"].0.clone();
                state
                    .tree
                    .as_mut()
                    .unwrap()
                    .insert(format!("/vault/pane/{old}"), (directory, Vec::new()));
                drop(state);
                tree_file(
                    &server,
                    &format!("/vault/pane/{old}/child"),
                    b"child".to_vec(),
                );
            } else {
                tree_file(&server, &format!("/vault/pane/{old}"), b"original".to_vec());
            }
            if alias {
                let mut state = server.state.lock().unwrap();
                let original = state.tree.as_ref().unwrap()[&format!("/vault/pane/{old}")].clone();
                state
                    .tree
                    .as_mut()
                    .unwrap()
                    .insert(format!("/vault/pane/{new}"), original);
            }
            server.state.lock().unwrap().allow_writes = true;
            let endpoint = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
            let options = BatchRenameOptions {
                find: old.into(),
                replacement: new.into(),
                prefix: String::new(),
                suffix: String::new(),
                preserve_extension: false,
                trim_outer_spaces: false,
                numbering: None,
            };
            let plan = PreparedSftpBatchRename::prepare(
                &endpoint,
                &[old.into()],
                &options,
                &Cancel::new(),
            )
            .unwrap();
            assert_eq!(
                plan.rows()[0].status,
                if alias {
                    BatchRenameStatus::Blocked
                } else {
                    BatchRenameStatus::Ready
                }
            );
            let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
            let state = server.state.lock().unwrap();
            assert_eq!(
                (report.renamed, state.mutations, state.opens),
                (usize::from(!alias), usize::from(!alias), 0)
            );
            let tree = state.tree.as_ref().unwrap();
            assert_eq!(tree.contains_key(&format!("/vault/pane/{old}")), alias);
            assert!(tree.contains_key(&format!("/vault/pane/{new}")));
            if folder && !alias {
                assert_eq!(tree[&format!("/vault/pane/{new}/child")].1, b"child");
            }
        }
    }
}

#[test]
fn sftp_schreibweisen_source_eltern_sitzung_und_profil_bleiben_gebunden() {
    for change in ["source", "parent", "session", "profile"] {
        let server = Server::tree();
        let (provider, plan) = sftp_schreibweisen_plan(&server);
        let mut calls = 0;
        let report = plan.execute(
            &Cancel::new(),
            &mut || {
                calls += 1;
                if calls == 2 && change == "profile" {
                    return Err(coded_error(
                        io::ErrorKind::Interrupted,
                        ErrorCode::PlanDrift,
                        "Profilbindung verändert",
                    ));
                }
                Ok(())
            },
            &mut |done, _| {
                if done == 1 {
                    if change == "session" {
                        provider.reconnect().unwrap();
                    } else if change != "profile" {
                        let mut state = server.state.lock().unwrap();
                        let path = if change == "source" {
                            "/vault/pane/A-two"
                        } else {
                            "/vault/pane"
                        };
                        state.tree.as_mut().unwrap().get_mut(path).unwrap().0.uid = Some(2000);
                    }
                }
            },
        );
        assert_eq!(
            (
                report.renamed,
                report.failed_unchanged,
                report.outcome_unclear
            ),
            (1, 2, 0),
            "{change}"
        );
        assert_eq!(report.rows[1].code, Some(ErrorCode::PlanDrift), "{change}");
        let state = server.state.lock().unwrap();
        assert_eq!((state.mutations, state.opens), (1, 0));
        assert!(state
            .tree
            .as_ref()
            .unwrap()
            .contains_key("/vault/pane/A-two"));
    }
}

#[test]
fn content_sync_sftp_baum_addonly_update_und_strikte_klasse_bleibt_gesperrt() {
    let server = Server::tree();
    server.state.lock().unwrap().allow_writes = true;
    let provider = server.connect();
    let remote = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let source = tempfile::tempdir().unwrap();
    let root = source.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("Sync/Leer")).unwrap();
    std::fs::write(root.join("Sync/Datei"), b"alpha").unwrap();
    let local = ProviderEndpoint::local(&root).unwrap();
    // Rote fachliche Gegenprobe: Der strikte Einstieg bleibt trotz gewöhnlich
    // schreibbarer SFTP-Sitzung vor jeder eigenen Mutation gesperrt.
    assert!(
        prepare_provider_transfer(local.clone(), remote.clone(), SyncOptions::default()).is_err()
    );
    assert_eq!(server.state.lock().unwrap().mutations, 0);
    let options = SyncOptions {
        compare: Compare::ContentHash,
        ..SyncOptions::default()
    };
    let prepared = prepare_content_sync(
        local.clone(),
        remote.clone(),
        options.clone(),
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
        server.state.lock().unwrap().tree.as_ref().unwrap()["/vault/pane/Sync/Datei"].1,
        b"alpha"
    );
    std::fs::write(root.join("Sync/Datei"), b"bravo").unwrap();
    let options = SyncOptions {
        mode: SyncMode::Update,
        ..options
    };
    let prepared = prepare_content_sync(
        local.clone(),
        remote.clone(),
        options.clone(),
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(prepared.preview().irreversible_replacements, 1);
    let digest = prepared.preview().digest;
    prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(
        server.state.lock().unwrap().tree.as_ref().unwrap()["/vault/pane/Sync/Datei"].1,
        b"bravo"
    );
    let mutations = server.state.lock().unwrap().mutations;
    let prepared = prepare_content_sync(local, remote, options, &Cancel::new()).unwrap();
    assert_eq!(prepared.preview().plan.update_count, 0);
    let digest = prepared.preview().digest;
    prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(server.state.lock().unwrap().mutations, mutations);
    assert!(!provider.capabilities().permits_add_only());
    assert!(!provider.capabilities().permits_update());
}

#[test]
fn content_sync_sftp_mirror_entfernt_nur_bestaetigte_blaetter_nach_kopien() {
    let server = Server::tree();
    server.state.lock().unwrap().allow_writes = true;
    let provider = server.connect();
    let remote = ProviderEndpoint::sftp(provider.clone(), Path::new("/pane")).unwrap();
    let source = tempfile::tempdir().unwrap();
    let root = source.path().canonicalize().unwrap();
    std::fs::write(root.join("Neu"), b"neu").unwrap();
    let prepared = prepare_content_sync(
        ProviderEndpoint::local(&root).unwrap(),
        remote,
        SyncOptions {
            mode: SyncMode::Mirror,
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(prepared.preview().irreversible_removals, 3);
    let digest = prepared.preview().digest;
    let stats = prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |progress| {
            if progress.phase == "delete" {
                assert_eq!(
                    server.state.lock().unwrap().tree.as_ref().unwrap()["/vault/pane/Neu"].1,
                    b"neu"
                );
            }
        })
        .unwrap();
    assert_eq!(stats.files_deleted, 3);
    assert!(!server
        .state
        .lock()
        .unwrap()
        .tree
        .as_ref()
        .unwrap()
        .contains_key("/vault/pane/Baum"));
    assert!(!provider.capabilities().permits_mirror());
}

#[path = "support/webdav_namespace.rs"]
mod content_webdav_server;

#[test]
fn content_sync_sftp_zu_webdav_streamt_ohne_lokalen_klartextspool() {
    let source = Server::tree();
    let provider = source.connect();
    let source_endpoint = ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap();
    let target = content_webdav_server::Server::new();
    let config = private_content_sync_config();
    let queue = OperationQueue::open(config.path().join("operations-queue.json")).unwrap();
    let options = SyncOptions {
        metadata: MetadataProfile::NativeBestEffort,
        compare: Compare::ContentHash,
        ..SyncOptions::default()
    };
    let prepared = doppeldecker_core::prepare_content_sync_for_queue(
        &queue,
        source_endpoint,
        target.endpoint(),
        options.clone(),
        &Cancel::new(),
    )
    .unwrap();
    assert!(prepared
        .preview()
        .plan
        .metadata_report
        .issues
        .iter()
        .any(|issue| issue.path == "Baum/Kind.txt"
            && issue.class == sync::MetadataClass::Acl
            && issue.reason.contains("kein Beleg für Abwesenheit")));
    let operation = QueueOperation::ContentSync {
        source: Box::new(BidirectionalQueueEndpoint::Sftp {
            connection: Box::new(source.config.clone()),
            root: "/pane".into(),
        }),
        target: Box::new(BidirectionalQueueEndpoint::WebDav {
            connection: Box::new(target.config.clone()),
            root: "/pane".into(),
        }),
        options,
        expected_plan: Box::new(prepared.preview().plan.clone()),
        binding_digest: prepared.preview().digest,
    };
    queue.enqueue(operation).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    execute_prepared_content_sync_queue_entry(
        &queue,
        &entry,
        prepared,
        true,
        &config.path().join("audit.jsonl"),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        std::fs::read(target.root.path().join("pane/Baum/Kind.txt")).unwrap(),
        b"Bauminhalt\n"
    );
    assert!(target.root.path().join("pane/Baum/Leer").is_dir());
    assert!(
        queue.snapshot().entries[0].temp_recovery_paths.is_empty(),
        "Erfolgreiche MKCOL-Readbacks müssen ihre Directoryintents abschließen"
    );
    assert!(!target.state.lock().unwrap().requests.is_empty());
    assert_eq!(source.state.lock().unwrap().mutations, 0);
    assert!(!std::fs::read_dir(config.path()).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("spool")));
}

#[test]
fn content_sync_unbekannte_quellzeit_wird_bilanziert_und_versionskonflikt_bleibt_offen() {
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        let attributes = &mut state
            .tree
            .as_mut()
            .unwrap()
            .get_mut("/vault/pane/Baum/Kind.txt")
            .unwrap()
            .0;
        attributes.mtime = None;
        attributes.atime = None;
    }
    let provider = server.connect();
    let source = ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap();
    let target = tempfile::tempdir().unwrap();
    let target_root = target.path().canonicalize().unwrap();
    let local = ProviderEndpoint::local(&target_root).unwrap();
    let opts = SyncOptions {
        compare: Compare::ContentHash,
        ..SyncOptions::default()
    };
    let prepared =
        prepare_content_sync(source.clone(), local.clone(), opts.clone(), &Cancel::new()).unwrap();
    assert!(prepared
        .preview()
        .plan
        .metadata_report
        .issues
        .iter()
        .any(|issue| issue.path == "Baum/Kind.txt" && issue.class == sync::MetadataClass::Mtime));
    let digest = prepared.preview().digest;
    let stats = prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert!(stats
        .metadata_report
        .issues
        .iter()
        .any(|issue| issue.class == sync::MetadataClass::Mtime));
    std::fs::write(target_root.join("Baum/Kind.txt"), b"anderer Inhalt").unwrap();
    let prepared = prepare_content_sync(
        source,
        local,
        SyncOptions {
            mode: SyncMode::Update,
            ..opts
        },
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(prepared.preview().plan.update_count, 0);
    assert!(prepared
        .preview()
        .plan
        .actions
        .iter()
        .any(|action| matches!(action,sync::Action::SkipConflict{path} if path=="Baum/Kind.txt")));
    let digest = prepared.preview().digest;
    prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(
        std::fs::read(target_root.join("Baum/Kind.txt")).unwrap(),
        b"anderer Inhalt"
    );
}

#[test]
fn content_sync_sftp_quelle_erhaelt_lokale_update_und_mirror_quarantaene() {
    let server = Server::tree();
    let provider = server.connect();
    let source = ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap();
    for mode in [SyncMode::Update, SyncMode::Mirror] {
        let area = tempfile::tempdir().unwrap();
        let target = area.path().join("ziel");
        std::fs::create_dir_all(target.join("Baum")).unwrap();
        std::fs::write(target.join("Baum/Kind.txt"), b"alte lokale Fassung").unwrap();
        filetime::set_file_mtime(
            target.join("Baum/Kind.txt"),
            filetime::FileTime::from_unix_time(123455, 0),
        )
        .unwrap();
        std::fs::write(target.join("Extra"), b"extra").unwrap();
        let prepared = prepare_content_sync(
            source.clone(),
            ProviderEndpoint::local(&target).unwrap(),
            SyncOptions {
                mode,
                compare: Compare::ContentHash,
                ..SyncOptions::default()
            },
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(prepared.preview().quarantined_replacements, 1);
        assert_eq!(prepared.preview().irreversible_replacements, 0);
        assert!(prepared.preview().restore_probe_ok);
        if mode == SyncMode::Mirror {
            assert_eq!(prepared.preview().quarantined_removals, 1);
            assert_eq!(prepared.preview().irreversible_removals, 0);
        }
        let quarantine = PathBuf::from(prepared.preview().quarantine_run.clone().unwrap());
        let digest = prepared.preview().digest;
        let stats = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.quarantine.as_deref(), quarantine.to_str());
        assert_eq!(
            std::fs::read(quarantine.join("Baum/Kind.txt")).unwrap(),
            b"alte lokale Fassung"
        );
        assert_eq!(
            std::fs::read(target.join("Baum/Kind.txt")).unwrap(),
            b"Bauminhalt\n"
        );
        if mode == SyncMode::Mirror {
            assert_eq!(std::fs::read(quarantine.join("Extra")).unwrap(), b"extra");
        }
    }
}

#[test]
fn content_sync_mirror_mehrgruppenfehler_erhaelt_teilwirkung_in_queue_audit_und_reopen() {
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        state.allow_writes = true;
        let directory = attrs(&state, false);
        let mut file = attrs(&state, true);
        file.size = Some(3);
        state.tree = Some(HashMap::from([
            ("/vault".into(), (directory.clone(), vec![])),
            ("/vault/pane".into(), (directory.clone(), vec![])),
            ("/vault/pane/a".into(), (directory.clone(), vec![])),
            ("/vault/pane/b".into(), (directory, vec![])),
            ("/vault/pane/a/alt".into(), (file.clone(), b"alt".to_vec())),
            ("/vault/pane/b/alt".into(), (file, b"alt".to_vec())),
        ]));
    }
    let provider = server.connect();
    let source_guard = tempfile::tempdir().unwrap();
    let source = source_guard.path().canonicalize().unwrap();
    std::fs::create_dir(source.join("a")).unwrap();
    std::fs::create_dir(source.join("b")).unwrap();
    let state = private_content_sync_config();
    let journal = state.path().join("operations-queue.json");
    let audit = state.path().join("audit.jsonl");
    let queue = OperationQueue::open(&journal).unwrap();
    let options = SyncOptions {
        mode: SyncMode::Mirror,
        compare: Compare::ContentHash,
        ..SyncOptions::default()
    };
    let prepared = doppeldecker_core::prepare_content_sync_for_queue(
        &queue,
        ProviderEndpoint::local(&source).unwrap(),
        ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap(),
        options.clone(),
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(prepared.preview().irreversible_removals, 2);
    queue
        .enqueue(QueueOperation::ContentSync {
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: source.to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::Sftp {
                connection: Box::new(server.config.clone()),
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
        &audit,
        &Cancel::new(),
        &mut |progress| {
            if progress.phase == "delete" && progress.files_done == 1 {
                server.state.lock().unwrap().fault = Fault::MetadataLost;
            }
        },
    )
    .unwrap_err();
    assert!(matches!(
        error_code(&error),
        ErrorCode::ConnectionLost | ErrorCode::PlanDrift
    ));
    assert_eq!(queue.snapshot().entries[0].progress.files_done, 1);
    let tree = server.state.lock().unwrap().tree.clone().unwrap();
    assert!(!tree.contains_key("/vault/pane/a/alt"));
    assert!(tree.contains_key("/vault/pane/b/alt"));
    let records = std::fs::read_to_string(&audit)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records.last().unwrap()["files_deleted"], 1);
    drop(queue);
    let reopened = OperationQueue::open(&journal).unwrap();
    assert_eq!(reopened.snapshot().entries[0].progress.files_done, 1);
    assert_eq!(reopened.snapshot().entries[0].state, QueueState::Failed);
}

fn private_content_sync_config() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}

#[test]
fn content_sync_sftp_unklare_mutation_bleibt_in_queue_und_wird_nicht_wiederholt() {
    let server = Server::tree();
    server.state.lock().unwrap().allow_writes = true;
    let provider = server.connect();
    let source_guard = tempfile::tempdir().unwrap();
    let source = source_guard.path().canonicalize().unwrap();
    std::fs::write(source.join("Neu"), b"neu").unwrap();
    let config = private_content_sync_config();
    let journal = config.path().join("operations-queue.json");
    let queue = OperationQueue::open(&journal).unwrap();
    let options = SyncOptions {
        compare: Compare::ContentHash,
        ..SyncOptions::default()
    };
    let prepared = doppeldecker_core::prepare_content_sync_for_queue(
        &queue,
        ProviderEndpoint::local(&source).unwrap(),
        ProviderEndpoint::sftp(provider, Path::new("/pane")).unwrap(),
        options.clone(),
        &Cancel::new(),
    )
    .unwrap();
    queue
        .enqueue(QueueOperation::ContentSync {
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: source.to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::Sftp {
                connection: Box::new(server.config.clone()),
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
        &config.path().join("audit.jsonl"),
        &Cancel::new(),
        &mut |progress| {
            if progress.phase == "copy" {
                server.state.lock().unwrap().lose_mutation_response = true;
            }
        },
    )
    .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    assert_eq!(server.state.lock().unwrap().mutations, 1);
    assert!(!queue.snapshot().entries[0].temp_recovery_paths.is_empty());
    assert!(queue.retry(&entry.operation_id).is_err());
    drop(queue);
    let reopened = OperationQueue::open(&journal).unwrap();
    assert!(reopened.retry(&entry.operation_id).is_err());
    assert_eq!(server.state.lock().unwrap().mutations, 1);
    assert_eq!(std::fs::read(source.join("Neu")).unwrap(), b"neu");
}

#[test]
fn content_sync_portable_sftp_ordnerzeit_folgt_dem_letzten_kindcommit() {
    let server = Server::tree();
    server.state.lock().unwrap().allow_writes = true;
    let source_guard = tempfile::tempdir().unwrap();
    let source = source_guard.path().canonicalize().unwrap();
    std::fs::create_dir_all(source.join("Ordner/Unter")).unwrap();
    std::fs::write(source.join("Ordner/Unter/Datei"), b"inhalt").unwrap();
    let expected = 1_700_000_000;
    for path in [
        source.join("Ordner/Unter/Datei"),
        source.join("Ordner/Unter"),
        source.join("Ordner"),
    ] {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(expected, 0)).unwrap();
    }
    let prepared = prepare_content_sync(
        ProviderEndpoint::local(&source).unwrap(),
        ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap(),
        SyncOptions {
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    )
    .unwrap();
    let digest = prepared.preview().digest;
    prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    let state = server.state.lock().unwrap();
    let tree = state.tree.as_ref().unwrap();
    for path in [
        "/vault/pane/Ordner",
        "/vault/pane/Ordner/Unter",
        "/vault/pane/Ordner/Unter/Datei",
    ] {
        assert_eq!(tree[path].0.mtime, Some(expected as u32), "{path}");
    }
}

#[test]
fn sftp_forward_queue_child_exits_after_first_committed_file() {
    use doppeldecker_core::queue::{
        execute_prepared_bidirectional_forward_queue_entry,
        prepare_bidirectional_forward_queue_entry,
    };
    let Some(root) = std::env::var_os("DOPPELDECKER_SFTP_FORWARD_QUEUE_CRASH") else {
        return;
    };
    let queue = OperationQueue::open(PathBuf::from(root).join("operations-queue.json")).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let prepared = prepare_bidirectional_forward_queue_entry(
        &queue,
        entry,
        Arc::new(FakeSecrets),
        &Cancel::new(),
    )
    .unwrap();
    let digest = prepared.preview().digest;
    execute_prepared_bidirectional_forward_queue_entry(
        &queue,
        prepared,
        digest,
        true,
        &[],
        &Cancel::new(),
        &mut |progress| {
            if progress.files_done == 1
                && std::env::var_os("DOPPELDECKER_SFTP_FORWARD_MIDWRITE").is_none()
            {
                std::process::exit(17);
            }
        },
    )
    .unwrap();
    panic!("Erster bestätigter SFTP-Dateicommit wurde nicht erreicht");
}

#[test]
fn sftp_forward_queue_owner_survives_process_crash_and_replans_against_b0() {
    run_sftp_forward_crash_case(false, false);
}

#[test]
fn sftp_forward_midwrite_crash_blocks_temp_until_user_cleanup() {
    run_sftp_forward_crash_case(true, false);
}

#[test]
fn sftp_forward_midwrite_reverse_direction_keeps_temp_out_of_baseline() {
    run_sftp_forward_crash_case(true, true);
}

fn run_sftp_forward_crash_case(midwrite: bool, reverse: bool) {
    use doppeldecker_core::bidirectional::forward::{
        forward_bidirectional_journal_status, prepare_forward_bidirectional,
    };
    use doppeldecker_core::queue::{
        execute_prepared_bidirectional_forward_queue_entry,
        prepare_bidirectional_forward_queue_recovery,
    };
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        state.allow_writes = true;
        state
            .tree
            .as_mut()
            .unwrap()
            .retain(|path, _| path == "/vault" || path == "/vault/pane");
    }
    let area = private_content_sync_config();
    let root = area.path().canonicalize().unwrap();
    let left = root.join("left");
    std::fs::create_dir(&left).unwrap();
    let baseline = root.join("baseline.json");
    let pair = PairId::generate().unwrap();
    for name in ["a", "b", "c"] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(left.join(name), name).unwrap();
        std::fs::set_permissions(left.join(name), std::fs::Permissions::from_mode(0o777)).unwrap();
        tree_file(
            &server,
            &format!("/vault/pane/{name}"),
            name.as_bytes().to_vec(),
        );
    }
    let endpoints = || {
        let local = ProviderEndpoint::local(&left).unwrap();
        let remote = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
        if reverse {
            (remote, local)
        } else {
            (local, remote)
        }
    };
    let prepare = |queue: &OperationQueue, recovery| {
        let (a, b) = endpoints();
        doppeldecker_core::bidirectional::forward::prepare_forward_bidirectional_for_queue(
            queue,
            pair,
            a,
            b,
            &baseline,
            &[],
            recovery,
            None,
            &Cancel::new(),
        )
        .unwrap()
    };
    let queue = OperationQueue::open(root.join("operations-queue.json")).unwrap();
    let initial = prepare(&queue, false);
    let preview = initial.preview().clone();
    let mut a = BidirectionalQueueEndpoint::Local {
        root: left.to_string_lossy().into_owned(),
    };
    let mut b = BidirectionalQueueEndpoint::Sftp {
        connection: Box::new(server.config.clone()),
        root: "/pane".into(),
    };
    if reverse {
        std::mem::swap(&mut a, &mut b);
    }
    queue
        .enqueue(QueueOperation::BidirectionalForward {
            pair_id: pair,
            pair_name: "SFTP-B0".into(),
            left: Box::new(a),
            right: Box::new(b),
            baseline_path: baseline.to_string_lossy().into_owned(),
            resolutions: vec![],
            expected_baseline_digest: preview.plan.baseline_digest,
            expected_plan_digest: preview.plan.plan_digest,
            expected_unresolved_digest: preview.unresolved_digest,
            expected_preview_digest: preview.digest,
        })
        .unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let prepared = doppeldecker_core::queue::bind_prepared_bidirectional_forward_queue_entry(
        &queue,
        &entry,
        initial,
        &Cancel::new(),
    )
    .unwrap();
    let digest = prepared.preview().digest;
    let QueueExecutionResult::Bidirectional(stats) =
        execute_prepared_bidirectional_forward_queue_entry(
            &queue,
            prepared,
            digest,
            true,
            &[],
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap()
    else {
        panic!("B0 fehlt")
    };
    assert_eq!(stats.baseline_generation, 1);
    drop(queue);
    let b0 = std::fs::read(&baseline).unwrap();
    std::fs::write(left.join("a"), b"a-new").unwrap();
    let new_b = if midwrite {
        vec![b'B'; 1024 * 1024]
    } else {
        b"b-new".to_vec()
    };
    std::fs::write(left.join("b"), &new_b).unwrap();
    tree_file(&server, "/vault/pane/c", b"c-new".to_vec());
    let queue = OperationQueue::open(root.join("operations-queue.json")).unwrap();
    let prepared = prepare(&queue, false);
    assert_eq!(prepared.preview().effects.len(), 2);
    let preview = prepared.preview().clone();
    prepared.discard().unwrap();
    let fresh_baseline = root.join("fresh-baseline.json");
    std::fs::write(&fresh_baseline, &b0).unwrap();
    let journal = root.join("operations-queue.json");
    let entry = queue
        .enqueue(QueueOperation::BidirectionalForward {
            pair_id: pair,
            pair_name: "SFTP-Forwardqueue".into(),
            left: Box::new(if reverse {
                BidirectionalQueueEndpoint::Sftp {
                    connection: Box::new(server.config.clone()),
                    root: "/pane".into(),
                }
            } else {
                BidirectionalQueueEndpoint::Local {
                    root: left.to_string_lossy().into_owned(),
                }
            }),
            right: Box::new(if reverse {
                BidirectionalQueueEndpoint::Local {
                    root: left.to_string_lossy().into_owned(),
                }
            } else {
                BidirectionalQueueEndpoint::Sftp {
                    connection: Box::new(server.config.clone()),
                    root: "/pane".into(),
                }
            }),
            baseline_path: baseline.to_string_lossy().into_owned(),
            resolutions: vec![],
            expected_baseline_digest: preview.plan.baseline_digest,
            expected_plan_digest: preview.plan.plan_digest,
            expected_unresolved_digest: preview.unresolved_digest,
            expected_preview_digest: preview.digest,
        })
        .unwrap();
    let id = entry.operation_id;
    drop(queue);
    // Der Elternprozess hält den realen SSH-Server; der Kindprozess verliert alle
    // RAM-Freigaben unmittelbar nach dem ersten per Readback bestätigten Commit.
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "sftp_forward_queue_child_exits_after_first_committed_file",
            "--nocapture",
        ])
        .env("DOPPELDECKER_SFTP_FORWARD_QUEUE_CRASH", &root);
    let partial_candidate = if midwrite {
        server.state.lock().unwrap().fault = Fault::ForwardMidWrite;
        command.env("DOPPELDECKER_SFTP_FORWARD_MIDWRITE", "1");
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let candidate = loop {
            let candidate = server
                .state
                .lock()
                .unwrap()
                .tree
                .as_ref()
                .unwrap()
                .iter()
                .find(|(path, (_, data))| {
                    path.contains(".b.doppeldecker-")
                        && !data.is_empty()
                        && data.len() < new_b.len()
                })
                .map(|(path, (_, data))| (path.clone(), data.len()));
            if let Some(candidate) = candidate {
                break candidate;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Keine echte Teil-WRITE-Wirkung erreicht"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        server.state.lock().unwrap().fault = Fault::None;
        assert!(candidate.1 > 0 && candidate.1 < new_b.len());
        Some(candidate.0)
    } else {
        let child = command.output().unwrap();
        assert_eq!(
            child.status.code(),
            Some(17),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        None
    };
    assert_eq!(std::fs::read(&baseline).unwrap(), b0);
    {
        let state = server.state.lock().unwrap();
        let tree = state.tree.as_ref().unwrap();
        assert_eq!(tree["/vault/pane/a"].1, b"a-new");
        assert_eq!(tree["/vault/pane/b"].1, b"b");
    }
    assert_eq!(
        std::fs::read(left.join("c")).unwrap(),
        if reverse {
            b"c-new".as_slice()
        } else {
            b"c".as_slice()
        }
    );
    let process_lock = Arc::new(OperationQueueProcessLock::acquire(&journal).unwrap());
    let queue = OperationQueue::open_with_process_lock(&journal, process_lock.clone()).unwrap();
    let entry = queue
        .snapshot()
        .entries
        .into_iter()
        .find(|entry| entry.operation_id == id)
        .unwrap();
    assert_eq!(entry.state, QueueState::Interrupted);
    assert!(entry.recovery_available);
    assert!(queue.retry(&id).is_err());
    assert!(queue.claim_next().unwrap().is_none());
    let foreign = "/vault/pane/.foreign.doppeldecker-tmp-user";
    if let Some(candidate) = &partial_candidate {
        // Zweite Fensterqueue, anderer Operationowner und kein Forwardjournal
        // an dieser Baseline: der bekannte fremde Temp muss schon den Scan sperren.
        let other_queue = OperationQueue::open_with_process_lock(
            root.join("operations-queue-other-window.json"),
            process_lock.clone(),
        )
        .unwrap();
        let mut operation = entry.operation.clone();
        if let QueueOperation::BidirectionalForward { baseline_path, .. } = &mut operation {
            *baseline_path = fresh_baseline.to_string_lossy().into_owned();
        }
        other_queue.enqueue(operation).unwrap();
        let other_entry = other_queue.claim_next().unwrap().unwrap();
        let scans_before = server.state.lock().unwrap().opens;
        let error = doppeldecker_core::queue::prepare_bidirectional_forward_queue_entry(
            &other_queue,
            other_entry,
            Arc::new(FakeSecrets),
            &Cancel::new(),
        )
        .err()
        .expect("Fremder Queueowner darf den Teilupload nicht scannen");
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(server.state.lock().unwrap().opens, scans_before);
        assert_eq!(std::fs::read(&fresh_baseline).unwrap(), b0);
        drop(other_queue);
        let error = prepare_bidirectional_forward_queue_recovery(
            &queue,
            &id,
            &[],
            None,
            Arc::new(FakeSecrets),
            &Cancel::new(),
        )
        .err()
        .expect("Teilupload darf keine neue Vorschau erhalten");
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(doppeldecker_core::error::recovery_paths(&error)
            .iter()
            .any(|path| candidate.ends_with(path)));
        assert_eq!(std::fs::read(&baseline).unwrap(), b0);
        assert!(!left
            .join(candidate.strip_prefix("/vault/pane/").unwrap())
            .exists());
        let (a, b) = endpoints();
        let direct_error =
            prepare_forward_bidirectional(pair, a, b, &baseline, &[], true, &Cancel::new())
                .err()
                .expect("Direkter Einstieg darf bekannten Teilupload nicht scannen");
        assert_eq!(error_code(&direct_error), ErrorCode::OutcomeUnclear);
        server.state.lock().unwrap().fault = Fault::ForwardTempStatDenied;
        let error = prepare_bidirectional_forward_queue_recovery(
            &queue,
            &id,
            &[],
            None,
            Arc::new(FakeSecrets),
            &Cancel::new(),
        )
        .err()
        .expect("Temp-Statfehler ist keine Abwesenheit");
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(&baseline).unwrap(), b0);
        server.state.lock().unwrap().fault = Fault::None;
        // Auch ein inzwischen ersetztes reguläres Objekt am belegten Pfad bleibt
        // sichtbar gesperrt und wird vom Core weder versteckt noch entfernt.
        tree_file(&server, candidate, b"bewusst ersetzter Kandidat".to_vec());
        assert!(prepare_bidirectional_forward_queue_recovery(
            &queue,
            &id,
            &[],
            None,
            Arc::new(FakeSecrets),
            &Cancel::new()
        )
        .is_err());
        assert_eq!(
            server.state.lock().unwrap().tree.as_ref().unwrap()[candidate].1,
            b"bewusst ersetzter Kandidat"
        );
        // Bewusst simulierte normale Benutzeraktion: Kandidat aus dem Paarbaum
        // verschieben; kein Core-Recoverypfad entfernt eigene oder fremde Dateien.
        let mut state = server.state.lock().unwrap();
        let value = state.tree.as_mut().unwrap().remove(candidate).unwrap();
        state
            .tree
            .as_mut()
            .unwrap()
            .insert("/vault/user-reviewed-partial".into(), value);
        drop(state);
        tree_file(&server, foreign, b"normale fremde Datei".to_vec());
    }
    let mutations = server.state.lock().unwrap().mutations;
    let prepared = prepare_bidirectional_forward_queue_recovery(
        &queue,
        &id,
        &[],
        None,
        Arc::new(FakeSecrets),
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(std::fs::read(&baseline).unwrap(), b0);
    assert_eq!(server.state.lock().unwrap().mutations, mutations);
    assert_eq!(prepared.preview().plan.baseline_generation, Some(1));
    let recovery = prepared.preview().recovery.as_ref().unwrap();
    assert!(if reverse {
        &recovery.changed_left
    } else {
        &recovery.changed_right
    }
    .iter()
    .any(|path| path.display_lossy() == "a"));
    let digest = prepared.preview().digest;
    let QueueExecutionResult::Bidirectional(stats) =
        execute_prepared_bidirectional_forward_queue_entry(
            &queue,
            prepared,
            digest,
            true,
            &[],
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap()
    else {
        panic!("Forwardbilanz fehlt")
    };
    assert_eq!(stats.baseline_generation, 2);
    assert!(stats.baseline_committed);
    assert_eq!(
        stats.execution_class,
        Some(sync::SyncExecutionClass::BestEffort)
    );
    let entry = queue
        .snapshot()
        .entries
        .into_iter()
        .find(|entry| entry.operation_id == id)
        .unwrap();
    assert_eq!(entry.state, QueueState::Completed);
    assert!(entry.bidirectional_completion_recovery.is_none());
    assert!(forward_bidirectional_journal_status(&baseline)
        .unwrap()
        .is_none());
    for name in ["a", "b", "c"] {
        let state = server.state.lock().unwrap();
        assert_eq!(
            std::fs::read(left.join(name)).unwrap(),
            state.tree.as_ref().unwrap()[&format!("/vault/pane/{name}")].1
        );
    }
    if midwrite {
        assert_eq!(
            std::fs::read(left.join(".foreign.doppeldecker-tmp-user")).unwrap(),
            b"normale fremde Datei"
        );
        let store = BaselineStore::open(&baseline).unwrap();
        let baseline = store.load().unwrap().unwrap();
        let mut paths = baseline
            .entries
            .iter()
            .map(|entry| entry.path.display_lossy())
            .collect::<Vec<_>>();
        paths.sort();
        assert_eq!(paths, vec![".foreign.doppeldecker-tmp-user", "a", "b", "c"]);
    }
    let audit = std::fs::read_to_string(root.join("operations-audit.jsonl")).unwrap();
    for status in ["started", "completed"] {
        assert!(audit
            .lines()
            .any(|line| line.contains("bidirectional_forward") && line.contains(status)));
    }
    let mutations = server.state.lock().unwrap().mutations;
    let noop = prepare(&queue, false);
    assert!(noop.preview().effects.is_empty());
    let digest = noop.preview().digest;
    let stats = noop
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert!(stats.runs.is_empty());
    assert!(!stats.baseline_committed);
    assert_eq!(stats.baseline_generation, 2);
    assert_eq!(server.state.lock().unwrap().mutations, mutations);
}

#[test]
fn content_sync_sftp_native_owner_ohne_gemeinsame_uid_gid_domaene_bleibt_lokal() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let server = Server::new(b"Ownerinhalt".to_vec(), Fault::None);
    let area = tempfile::tempdir().unwrap();
    let target = area.path().join("ziel");
    std::fs::create_dir(&target).unwrap();
    let local = std::fs::metadata(&target).unwrap();
    {
        let mut state = server.state.lock().unwrap();
        let directory = attrs(&state, false);
        let mut file = attrs(&state, true);
        file.uid = Some(local.uid().wrapping_add(1));
        file.gid = Some(local.gid().wrapping_add(1));
        file.permissions = Some(0o100640);
        state.tree = Some(HashMap::from([
            ("/vault".into(), (directory.clone(), vec![])),
            ("/vault/pane".into(), (directory, vec![])),
            ("/vault/pane/Datei".into(), (file, state.data.clone())),
        ]));
    }
    let source = ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap();
    let options = SyncOptions {
        compare: Compare::ContentHash,
        metadata: MetadataProfile::NativeBestEffort,
        ..SyncOptions::default()
    };
    let prepared = prepare_content_sync(
        source.clone(),
        ProviderEndpoint::local(&target).unwrap(),
        options.clone(),
        &Cancel::new(),
    )
    .unwrap();
    let report = &prepared.preview().plan.metadata_report;
    let owner = report
        .classes
        .iter()
        .find(|row| row.class == sync::MetadataClass::Owner)
        .expect("Ownerbilanz fehlt");
    assert_eq!((owner.transferred, owner.omitted), (0, 1), "{report:?}");
    assert!(report.issues.iter().any(|issue| issue.path == "Datei"
        && issue.class == sync::MetadataClass::Owner
        && issue
            .reason
            .contains("bestätigte gemeinsame UID/GID-Domäne")));
    let digest = prepared.preview().digest;
    let stats = prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    let owner = stats
        .metadata_report
        .classes
        .iter()
        .find(|row| row.class == sync::MetadataClass::Owner)
        .unwrap();
    assert_eq!((owner.transferred, owner.omitted), (0, 1));
    let actual = std::fs::metadata(target.join("Datei")).unwrap();
    assert_eq!((actual.uid(), actual.gid()), (local.uid(), local.gid()));
    assert_eq!(actual.mtime(), 123456);
    assert_eq!(actual.permissions().mode() & 0o777, 0o640);
    assert_eq!(std::fs::read(target.join("Datei")).unwrap(), b"Ownerinhalt");
    let strict_target = area.path().join("strict");
    std::fs::create_dir(&strict_target).unwrap();
    let error = prepare_content_sync(
        source.clone(),
        ProviderEndpoint::local(&strict_target).unwrap(),
        SyncOptions {
            metadata: MetadataProfile::NativeStrict,
            ..options
        },
        &Cancel::new(),
    )
    .err()
    .expect("Strict darf die Ownerauslassung nicht verschweigen");
    assert_eq!(error_code(&error), ErrorCode::Unsupported);
    assert!(!strict_target.join("Datei").exists());
    let error = prepare_content_sync(
        source.clone(),
        ProviderEndpoint::local(&target).unwrap(),
        SyncOptions {
            mode: SyncMode::Update,
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeStrict,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    )
    .err()
    .expect("Auch gleicher Inhalt darf eine unbekannte Ownerdomäne nicht als Strict verbergen");
    assert_eq!(error_code(&error), ErrorCode::Unsupported);
    assert!(error
        .to_string()
        .contains("bestätigte gemeinsame UID/GID-Domäne"));
    let mirror = area.path().join("mirror");
    std::fs::create_dir(&mirror).unwrap();
    std::fs::write(mirror.join("Extra"), b"extra").unwrap();
    let prepared = prepare_content_sync(
        source,
        ProviderEndpoint::local(&mirror).unwrap(),
        SyncOptions {
            mode: SyncMode::Mirror,
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeBestEffort,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(prepared.preview().blocked_removals, 0);
    assert_eq!(prepared.preview().quarantined_removals, 1);
    let digest = prepared.preview().digest;
    let stats = prepared
        .confirm_effects(digest, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(stats.files_deleted, 1);
    assert!(stats.quarantine.is_some());
    assert!(!mirror.join("Extra").exists());
    assert_eq!(std::fs::read(mirror.join("Datei")).unwrap(), b"Ownerinhalt");
}

#[test]
fn content_sync_unbekannter_sftp_owner_bleibt_unbekannt_und_lokale_owner_bleiben_uebertragbar() {
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        for (_, (attributes, _)) in state
            .tree
            .as_mut()
            .unwrap()
            .iter_mut()
            .filter(|(path, _)| path.ends_with("Kind.txt"))
        {
            attributes.uid = None;
            attributes.gid = None;
        }
    }
    let remote_target = tempfile::tempdir().unwrap();
    let prepared = prepare_content_sync(
        ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap(),
        ProviderEndpoint::local(remote_target.path()).unwrap(),
        SyncOptions {
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeBestEffort,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    )
    .unwrap();
    let issue = prepared
        .preview()
        .plan
        .metadata_report
        .issues
        .iter()
        .find(|issue| issue.path == "Baum/Kind.txt" && issue.class == sync::MetadataClass::Owner)
        .expect("Unbekannter Dateiowner muss sichtbar bleiben");
    assert!(issue.reason.contains("fehlende Beobachtung"));
    assert!(!issue
        .reason
        .contains("numerischen UID/GID der Quelle sind bekannt"));
    let local_source = tempfile::tempdir().unwrap();
    let local_target = tempfile::tempdir().unwrap();
    std::fs::write(local_source.path().join("Datei"), b"lokal").unwrap();
    let prepared = prepare_content_sync(
        ProviderEndpoint::local(local_source.path()).unwrap(),
        ProviderEndpoint::local(local_target.path()).unwrap(),
        SyncOptions {
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeBestEffort,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    )
    .unwrap();
    let owner = prepared
        .preview()
        .plan
        .metadata_report
        .classes
        .iter()
        .find(|row| row.class == sync::MetadataClass::Owner)
        .unwrap();
    assert_eq!((owner.transferred, owner.omitted), (1, 0));
}

#[test]
fn sftp_forward_fresh_pair_blocks_active_other_window_and_keeps_other_server_parallel() {
    use doppeldecker_core::bidirectional::forward::prepare_forward_bidirectional;
    use doppeldecker_core::queue::{
        execute_prepared_bidirectional_forward_queue_entry,
        prepare_bidirectional_forward_queue_entry,
    };
    let server = Server::tree();
    {
        let mut state = server.state.lock().unwrap();
        state.allow_writes = true;
        state
            .tree
            .as_mut()
            .unwrap()
            .retain(|path, _| path == "/vault" || path == "/vault/pane");
    }
    let area = private_content_sync_config();
    let root = area.path().canonicalize().unwrap();
    let left = root.join("left");
    let upload = root.join("upload");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&upload).unwrap();
    use std::os::unix::fs::PermissionsExt;
    for name in ["a", "c", "d"] {
        std::fs::write(left.join(name), name).unwrap();
        std::fs::set_permissions(left.join(name), std::fs::Permissions::from_mode(0o777)).unwrap();
        tree_file(
            &server,
            &format!("/vault/pane/{name}"),
            name.as_bytes().to_vec(),
        );
    }
    std::fs::write(upload.join("b"), vec![b'B'; 1024 * 1024]).unwrap();
    let pair = PairId::generate().unwrap();
    let baseline = root.join("baseline.json");
    let prepared = prepare_forward_bidirectional(
        pair,
        ProviderEndpoint::local(&left).unwrap(),
        ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap(),
        &baseline,
        &[],
        false,
        &Cancel::new(),
    )
    .unwrap();
    let preview = prepared.preview().clone();
    prepared.discard().unwrap();
    let operation = QueueOperation::BidirectionalForward {
        pair_id: pair,
        pair_name: "Frisches anderes Fenster".into(),
        left: Box::new(BidirectionalQueueEndpoint::Local {
            root: left.to_string_lossy().into_owned(),
        }),
        right: Box::new(BidirectionalQueueEndpoint::Sftp {
            connection: Box::new(server.config.clone()),
            root: "/pane".into(),
        }),
        baseline_path: baseline.to_string_lossy().into_owned(),
        resolutions: vec![],
        expected_baseline_digest: preview.plan.baseline_digest,
        expected_plan_digest: preview.plan.plan_digest,
        expected_unresolved_digest: preview.unresolved_digest,
        expected_preview_digest: preview.digest,
    };
    let process_lock = Arc::new(
        OperationQueueProcessLock::acquire(root.join("operations-queue-upload.json")).unwrap(),
    );
    let uploader = Arc::new(
        OperationQueue::open_with_process_lock(
            root.join("operations-queue-upload.json"),
            process_lock.clone(),
        )
        .unwrap(),
    );
    uploader
        .enqueue(interactive_transfer_operation(
            BidirectionalQueueEndpoint::Local {
                root: upload.to_string_lossy().into_owned(),
            },
            BidirectionalQueueEndpoint::Sftp {
                connection: Box::new(server.config.clone()),
                root: "/pane".into(),
            },
            upload.join("b").to_str().unwrap(),
            FileQueueKind::Copy,
        ))
        .unwrap();
    let upload_entry = uploader.claim_next().unwrap().unwrap();
    let cancel = Cancel::new();
    let worker_cancel = cancel.clone();
    let worker_queue = uploader.clone();
    let worker_root = root.clone();
    server.state.lock().unwrap().fault = Fault::ForwardMidWriteHold;
    let worker = std::thread::spawn(move || {
        queue::execute_provider_file_queue_entry(
            &worker_queue,
            upload_entry,
            &execution(&worker_root),
            Arc::new(FakeSecrets),
            &worker_cancel,
            &mut |_| {},
        )
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let candidate = loop {
        let candidate = server
            .state
            .lock()
            .unwrap()
            .tree
            .as_ref()
            .unwrap()
            .iter()
            .find(|(path, (_, bytes))| {
                path.contains(".b.doppeldecker-") && !bytes.is_empty() && bytes.len() < 1024 * 1024
            })
            .map(|(path, _)| path.clone());
        if let Some(candidate) = candidate {
            break candidate;
        }
        if std::time::Instant::now() >= deadline {
            cancel.cancel();
            panic!(
                "Kein realer fremder Teilupload: {:?}",
                worker.join().unwrap()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let held = server
        .state
        .lock()
        .unwrap()
        .tree
        .as_mut()
        .unwrap()
        .remove(&candidate)
        .unwrap();
    let attempt = |name: &str| {
        let queue =
            OperationQueue::open_with_process_lock(root.join(name), process_lock.clone()).unwrap();
        queue.enqueue(operation.clone()).unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        let result = prepare_bidirectional_forward_queue_entry(
            &queue,
            entry,
            Arc::new(FakeSecrets),
            &Cancel::new(),
        );
        (queue, result)
    };
    let opens = server.state.lock().unwrap().opens;
    let (_, blocked) = attempt("operations-queue-active-check.json");
    assert_eq!(
        error_code(
            &blocked
                .err()
                .expect("Aktiver fremder Writer sperrt auch bei NotFound")
        ),
        ErrorCode::OutcomeUnclear
    );
    assert_eq!(server.state.lock().unwrap().opens, opens);
    assert!(!baseline.exists());
    let content_queue = OperationQueue::open_with_process_lock(
        root.join("operations-queue-content-check.json"),
        process_lock.clone(),
    )
    .unwrap();
    let source_error = doppeldecker_core::prepare_content_sync_for_queue(
        &content_queue,
        ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap(),
        ProviderEndpoint::local(&left).unwrap(),
        SyncOptions {
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    )
    .err()
    .expect("Einseitiger Sync darf fremden aktiven Temp nicht als Quelle übernehmen");
    assert_eq!(error_code(&source_error), ErrorCode::OutcomeUnclear);
    let mirror_error = doppeldecker_core::prepare_content_sync_for_queue(
        &content_queue,
        ProviderEndpoint::local(&left).unwrap(),
        ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap(),
        SyncOptions {
            mode: SyncMode::Mirror,
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        },
        &Cancel::new(),
    )
    .err()
    .expect("Mirror darf fremden aktiven Temp nicht entfernen");
    assert_eq!(error_code(&mirror_error), ErrorCode::OutcomeUnclear);
    assert_eq!(server.state.lock().unwrap().opens, opens);
    server
        .state
        .lock()
        .unwrap()
        .tree
        .as_mut()
        .unwrap()
        .insert(candidate.clone(), held);
    // Gleichlautender virtueller Pfad auf einem zweiten echten Server bleibt
    // ein normaler Benutzerpfad; dessen Fenster darf während des Uploads arbeiten.
    let other = Server::tree();
    {
        let mut state = other.state.lock().unwrap();
        state.allow_writes = true;
        state
            .tree
            .as_mut()
            .unwrap()
            .retain(|path, _| path == "/vault" || path == "/vault/pane");
    }
    let other_left = root.join("other-left");
    std::fs::create_dir(&other_left).unwrap();
    let name = candidate.strip_prefix("/vault/pane/").unwrap();
    for file in ["a", "c", "d", name] {
        std::fs::write(other_left.join(file), b"regular").unwrap();
        std::fs::set_permissions(
            other_left.join(file),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        tree_file(&other, &format!("/vault/pane/{file}"), b"regular".to_vec());
    }
    let other_pair = PairId::generate().unwrap();
    let other_baseline = root.join("other-baseline.json");
    let prepared = prepare_forward_bidirectional(
        other_pair,
        ProviderEndpoint::local(&other_left).unwrap(),
        ProviderEndpoint::sftp(other.connect(), Path::new("/pane")).unwrap(),
        &other_baseline,
        &[],
        false,
        &Cancel::new(),
    )
    .unwrap();
    let other_preview = prepared.preview().clone();
    let other_queue = OperationQueue::open_with_process_lock(
        root.join("operations-queue-other-server.json"),
        process_lock.clone(),
    )
    .unwrap();
    other_queue
        .enqueue(QueueOperation::BidirectionalForward {
            pair_id: other_pair,
            pair_name: "Unabhängiger Server".into(),
            left: Box::new(BidirectionalQueueEndpoint::Local {
                root: other_left.to_string_lossy().into_owned(),
            }),
            right: Box::new(BidirectionalQueueEndpoint::Sftp {
                connection: Box::new(other.config.clone()),
                root: "/pane".into(),
            }),
            baseline_path: other_baseline.to_string_lossy().into_owned(),
            resolutions: vec![],
            expected_baseline_digest: other_preview.plan.baseline_digest,
            expected_plan_digest: other_preview.plan.plan_digest,
            expected_unresolved_digest: other_preview.unresolved_digest,
            expected_preview_digest: other_preview.digest,
        })
        .unwrap();
    let entry = other_queue.claim_next().unwrap().unwrap();
    let prepared = queue::bind_prepared_bidirectional_forward_queue_entry(
        &other_queue,
        &entry,
        prepared,
        &Cancel::new(),
    )
    .unwrap();
    let digest = prepared.preview().digest;
    execute_prepared_bidirectional_forward_queue_entry(
        &other_queue,
        prepared,
        digest,
        true,
        &[],
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(other_baseline.exists());
    cancel.cancel();
    let _ = worker.join().unwrap();
    server.state.lock().unwrap().fault = Fault::None;
    assert!(server
        .state
        .lock()
        .unwrap()
        .tree
        .as_ref()
        .unwrap()
        .contains_key(&candidate));
    let (_, blocked) = attempt("operations-queue-stopped-check.json");
    assert_eq!(
        error_code(
            &blocked
                .err()
                .expect("Beendeter fremder Teilupload bleibt gesperrt")
        ),
        ErrorCode::OutcomeUnclear
    );
    assert!(!baseline.exists());
    let partial = server
        .state
        .lock()
        .unwrap()
        .tree
        .as_mut()
        .unwrap()
        .remove(&candidate)
        .unwrap();
    server
        .state
        .lock()
        .unwrap()
        .tree
        .as_mut()
        .unwrap()
        .insert("/vault/user-reviewed-other-window".into(), partial);
    let (queue, prepared) = attempt("operations-queue-cleaned-check.json");
    let prepared = prepared.unwrap();
    let digest = prepared.preview().digest;
    execute_prepared_bidirectional_forward_queue_entry(
        &queue,
        prepared,
        digest,
        true,
        &[],
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    let store = BaselineStore::open(&baseline).unwrap();
    let mut paths = store
        .load()
        .unwrap()
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| entry.path.display_lossy())
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(paths, ["a", "c", "d"]);
}

#[path = "support/content_sync_queue.rs"]
mod guarded_content_queue;
use guarded_content_queue::prepare as prepare_content_sync;

#[test]
fn content_sync_sftp_metadaten_stoppen_frisch_beobachtete_typwechsel() {
    // Der erfolgreiche Lauf bestimmt die unmittelbar vor SETSTAT gelesenen
    // LSTATs. Derselbe Paketablauf tauscht genau dort das Objekt aus.
    let run = |swap: Option<(usize, u32)>| {
        let server = Server::tree();
        {
            let mut state = server.state.lock().unwrap();
            state.allow_writes = true;
            state.swap_on_lstat = swap;
            let sentinel = attrs(&state, true);
            state.tree.as_mut().unwrap().insert(
                "/vault/pane/Sentinel".into(),
                (sentinel, b"unveraendert".to_vec()),
            );
        }
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir(source.path().join("Ordner")).unwrap();
        std::fs::write(source.path().join("Ordner/Datei"), b"inhalt").unwrap();
        let prepared = prepare_content_sync(
            ProviderEndpoint::local(source.path()).unwrap(),
            ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap(),
            SyncOptions {
                compare: Compare::ContentHash,
                ..SyncOptions::default()
            },
            &Cancel::new(),
        )
        .unwrap();
        let sentinel =
            server.state.lock().unwrap().tree.as_ref().unwrap()["/vault/pane/Sentinel"].clone();
        let digest = prepared.preview().digest;
        let result = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {});
        let state = server.state.lock().unwrap();
        let actual_sentinel = state.tree.as_ref().unwrap()["/vault/pane/Sentinel"].clone();
        let observations = state.setstat_observations.clone();
        drop(state);
        assert_eq!(actual_sentinel.1, sentinel.1);
        assert_eq!(actual_sentinel.0.permissions, sentinel.0.permissions);
        assert_eq!(actual_sentinel.0.mtime, sentinel.0.mtime);
        (result, observations)
    };
    let (result, observations) = run(None);
    assert_eq!(result.unwrap().files_copied, 1);
    assert!(observations
        .iter()
        .any(|(path, mode, _)| path.ends_with("/Ordner") && !mode));
    assert!(observations.iter().any(|(_, mode, _)| *mode));
    for (index, (path, mode, request)) in observations.iter().enumerate() {
        let opposite = if path.ends_with("/Ordner") {
            0o100000
        } else {
            0o040000
        };
        for kind in [0o120000, 0o010000, opposite] {
            let (result, actual) = run(Some((*request, kind)));
            assert!(result.is_err(), "SETSTAT mode={mode}, kind={kind:o}");
            assert_eq!(
                actual.len(),
                index,
                "Kein SETSTAT nach beobachtetem Typwechsel: {mode}, {kind:o}"
            );
        }
    }
}

#[test]
fn content_sync_sftp_zeit_2107_erhaelt_inhalt_und_pfadgenaue_auslassung() {
    for metadata in [
        MetadataProfile::Portable,
        MetadataProfile::NativeBestEffort,
        MetadataProfile::NativeStrict,
    ] {
        let server = Server::tree();
        server.state.lock().unwrap().allow_writes = true;
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir(source.path().join("Ordner")).unwrap();
        std::fs::write(source.path().join("Ordner/Datei"), b"zukunft").unwrap();
        for path in [
            source.path().join("Ordner/Datei"),
            source.path().join("Ordner"),
        ] {
            filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(4_323_283_200, 0))
                .unwrap();
        }
        let prepared = prepare_content_sync(
            ProviderEndpoint::local(source.path()).unwrap(),
            ProviderEndpoint::sftp(server.connect(), Path::new("/pane")).unwrap(),
            SyncOptions {
                compare: Compare::ContentHash,
                metadata,
                ..SyncOptions::default()
            },
            &Cancel::new(),
        );
        if metadata == MetadataProfile::NativeStrict {
            assert!(
                prepared.is_err(),
                "NativeStrict darf den unvollständigen Vertrag nicht herabstufen"
            );
            assert_eq!(server.state.lock().unwrap().mutations, 0);
            continue;
        }
        let prepared = prepared.unwrap();
        let digest = prepared.preview().digest;
        let stats = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.files_copied, 1);
        assert_eq!(
            server.state.lock().unwrap().tree.as_ref().unwrap()["/vault/pane/Ordner/Datei"].1,
            b"zukunft"
        );
        for path in ["Ordner/Datei", "Ordner"] {
            assert!(
                stats
                    .metadata_report
                    .issues
                    .iter()
                    .any(|issue| issue.path == path
                        && issue.class == sync::MetadataClass::Mtime
                        && issue.reason.contains("SFTP v3")),
                "{:?}",
                stats.metadata_report
            );
        }
    }
}
