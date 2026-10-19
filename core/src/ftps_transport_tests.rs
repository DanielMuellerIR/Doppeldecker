use super::*;
use libunftp::{options::FtpsRequired, ServerBuilder};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::PrivatePkcs8KeyDer;
use std::net::TcpListener;
use unftp_sbe_fs::Filesystem;

trait PeerStream: Read + Write + Send {}
impl<T: Read + Write + Send> PeerStream for T {}

fn server_tls() -> (FtpsCertificatePin, Arc<rustls::ServerConfig>) {
    let certified = generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let pin = FtpsCertificatePin {
        sha256: Sha256::digest(certified.cert.der().as_ref()).into(),
    };
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![certified.cert.der().clone()],
        PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into(),
    )
    .unwrap();
    (pin, Arc::new(config))
}

fn private_config() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let config = tempfile::tempdir().unwrap();
    std::fs::set_permissions(config.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = std::fs::canonicalize(config.path()).unwrap();
    crate::local_fs::write_acl_exact(&root, None).unwrap();
    config
}

struct TestSecrets;

impl SecretResolver for TestSecrets {
    fn resolve(&self, _: &crate::KeychainSecretReference) -> io::Result<SecretString> {
        SecretString::new("anonymous-test".into())
    }
}

struct Fixture {
    root: tempfile::TempDir,
    config: FtpsConnectionConfig,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn new(tls: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let (pin, server_tls) = server_tls();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let server_root = root.path().to_path_buf();
        let (stop, receiver) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let builder = ServerBuilder::new(Box::new(move || {
                    Filesystem::new(server_root.clone()).unwrap()
                }))
                .shutdown_indicator(async move {
                    let _ = receiver.await;
                    libunftp::options::Shutdown::new()
                });
                let builder = if tls {
                    builder
                        .ftps_manual::<std::path::PathBuf>(server_tls)
                        .ftps_required(FtpsRequired::All, FtpsRequired::All)
                } else {
                    builder
                };
                builder
                    .build()
                    .unwrap()
                    .listen(format!("127.0.0.1:{port}"))
                    .await
                    .unwrap();
            });
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "FTPS-Fixture startete nicht");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self {
            root,
            config: FtpsConnectionConfig {
                host: "localhost".into(),
                alternate_hosts: Vec::new(),
                port,
                username: "anonymous".into(),
                requested_root: "/".into(),
                transport_security: if tls {
                    FtpTransportSecurity::ExplicitTls
                } else {
                    FtpTransportSecurity::Plaintext
                },
                certificate: tls.then_some(pin),
                certificate_policy: tls.then_some(FtpsCertificatePolicy::PinnedCertificate),
                password_reference: crate::KeychainSecretReference::new(
                    "Doppeldecker-Test",
                    "ftps-transport",
                )
                .unwrap(),
            },
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn stou_uebertraegt_leere_und_kleine_dateien_mit_unabhaengigem_readback() {
    for tls in [false, true] {
        let fixture = Fixture::new(tls);
        let mut session = connect_once(&fixture.config, &TestSecrets).unwrap();
        for bytes in [b"".as_slice(), b"eins", b"delete-probe"] {
            let name =
                store_unique_bytes(&mut session.stream, bytes, &mut BTreeMap::new(), None).unwrap();
            assert_eq!(
                std::fs::read(fixture.root.path().join(&name)).unwrap(),
                bytes,
                "TLS={tls}, STOU={name}"
            );
            assert_eq!(
                read_remote_hash(&mut session.stream, &name).unwrap(),
                blake3::hash(bytes),
                "TLS={tls}, STOU={name}"
            );
        }
        session.stream.quit().unwrap();
    }
}

#[test]
fn fehlgeschlagene_neuprobe_entzieht_alte_dateifreigaben() {
    let fixture = Fixture::new(false);
    std::fs::create_dir(fixture.root.path().join("pane")).unwrap();
    let intent_config = private_config();
    let queue_path = std::fs::canonicalize(intent_config.path())
        .unwrap()
        .join("operations-queue.json");
    let queue = crate::OperationQueue::open(queue_path).unwrap();
    let vfs = FtpsVfs::connect(fixture.config.clone(), Arc::new(TestSecrets))
        .unwrap()
        .for_queue_preparation(&queue, Path::new("/pane"))
        .unwrap();
    assert!(
        vfs.probe_add_only_capabilities_at(Path::new("/pane"))
            .unwrap()
            .best_effort_file_replace
    );
    std::fs::remove_dir(fixture.root.path().join("pane")).unwrap();
    vfs.probe_add_only_capabilities_at(Path::new("/pane"))
        .unwrap_err();
    let capabilities = vfs.capabilities();
    assert!(!capabilities.best_effort_file_commit);
    assert!(!capabilities.best_effort_file_replace);
    assert!(!capabilities.exclusive_create);
    assert!(!capabilities.verified_file_commit);
    assert!(!capabilities.atomic_noreplace_rename);
    assert!(!capabilities.quarantine);
}

#[test]
fn probe_cleanup_erhaelt_teilinhalt_fremde_namen_und_belegte_ordner() {
    for tls in [false, true] {
        for scenario in [
            "partial",
            "changed",
            "foreign-file",
            "foreign-dir",
            "foreign-child",
        ] {
            let fixture = Fixture::new(tls);
            let base = fixture.root.path().join("probe");
            std::fs::create_dir(&base).unwrap();
            let mut session = connect_once(&fixture.config, &TestSecrets).unwrap();
            session.stream.cwd("/probe").unwrap();
            let mut owned = BTreeMap::new();
            let name = store_unique_bytes(&mut session.stream, b"eins", &mut owned, None).unwrap();
            let mut directories = BTreeSet::new();
            match scenario {
                "partial" => std::fs::write(base.join(&name), b"ei").unwrap(),
                "changed" => std::fs::write(base.join(&name), b"fremd").unwrap(),
                "foreign-file" => std::fs::write(base.join("spaeter-fremd"), b"behalten").unwrap(),
                "foreign-dir" => std::fs::create_dir(base.join("spaeter-fremd")).unwrap(),
                "foreign-child" => {
                    std::fs::create_dir(base.join("empty")).unwrap();
                    directories.insert("empty".to_owned());
                    std::fs::write(base.join("empty/spaeter-fremd"), b"behalten").unwrap();
                }
                _ => unreachable!(),
            }
            let before = std::fs::read(base.join(&name)).unwrap();
            let error =
                cleanup_probe_entries(&mut session.stream, "/", "/probe", &owned, &directories)
                    .unwrap_err();
            assert_eq!(
                crate::error_code(&error),
                ErrorCode::OutcomeUnclear,
                "{scenario}: {error}"
            );
            assert_eq!(std::fs::read(base.join(&name)).unwrap(), before);
            assert!(base.exists());
            if scenario.starts_with("foreign") {
                assert!(base
                    .join(if scenario == "foreign-child" {
                        "empty/spaeter-fremd"
                    } else {
                        "spaeter-fremd"
                    })
                    .exists());
            }
            session.stream.quit().unwrap();
        }
    }
}

struct FaultData {
    accepted: usize,
    partial: bool,
    flush_error: bool,
    close_error: bool,
    closes: Arc<Mutex<usize>>,
}

impl Read for FaultData {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
}

impl Write for FaultData {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.partial && self.accepted > 0 {
            return Err(io::Error::other("Teiltransfer abgebrochen"));
        }
        let count = if self.partial {
            bytes.len().min(2)
        } else {
            bytes.len()
        };
        self.accepted += count;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.flush_error {
            Err(io::Error::other("Flush verweigert"))
        } else {
            Ok(())
        }
    }
}

impl FtpsDataStream for FaultData {
    fn shutdown_write(&mut self) -> io::Result<()> {
        *lock(&self.closes) += 1;
        if self.close_error {
            Err(coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                "TLS-Abschluss unklar",
            ))
        } else {
            Ok(())
        }
    }
    fn socket(&self) -> &TcpStream {
        panic!("Fehlerinjektion besitzt keinen Datensocket")
    }
}

#[test]
fn stou_erhaelt_teil_flush_abschluss_und_antwortfehler_ohne_retry() {
    for scenario in ["partial", "flush", "close", "lost-response", "success"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.write_all(b"220 Payload test\r\n").unwrap();
            if scenario != "lost-response" {
                socket.write_all(b"226 Complete\r\n").unwrap();
            }
        });
        let socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut stream = RustlsFtpStream::connect_with_stream(socket).unwrap();
        let closes = Arc::new(Mutex::new(0));
        let data = FaultData {
            accepted: 0,
            partial: scenario == "partial",
            flush_error: scenario == "flush",
            close_error: matches!(scenario, "partial" | "flush" | "close"),
            closes: closes.clone(),
        };
        let result = write_unique_contents(&mut stream, "own", Box::new(data), b"eins");
        peer.join().unwrap();
        assert_eq!(*lock(&closes), 1, "{scenario}: Abschluss wiederholt");
        if scenario == "success" {
            result.unwrap();
            continue;
        }
        let error = result.unwrap_err();
        let text = error.to_string();
        assert!(text.contains("own"), "{scenario}: {error}");
        if matches!(scenario, "partial" | "flush") {
            assert!(text.contains(if scenario == "partial" {
                "Teiltransfer"
            } else {
                "Flush verweigert"
            }));
            assert!(text.contains("TLS-Abschluss unklar"));
            assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
        }
        if scenario == "lost-response" {
            assert_eq!(crate::error_code(&error), ErrorCode::ConnectionLost);
        }
    }
}

#[test]
fn upload_renameteilfehler_loescht_keinen_ungebundenen_stou_namen() {
    use std::io::{BufRead, BufReader};
    for lost in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .write_all(b"220 Upload test\r\n226 Complete\r\n")
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut commands = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let command = line.trim().to_owned();
                commands.push(command.clone());
                if command.starts_with("RNFR ") {
                    socket.write_all(b"350 Ready\r\n").unwrap();
                } else if command.starts_with("RNTO ") {
                    if lost {
                        break;
                    }
                    socket.write_all(b"550 Collision\r\n").unwrap();
                } else {
                    socket.write_all(b"250 Removed\r\n").unwrap();
                }
            }
            commands
        });
        let socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let stream = RustlsFtpStream::connect_with_stream(socket).unwrap();
        let closes = Arc::new(Mutex::new(0));
        let data = FaultData {
            accepted: 0,
            partial: false,
            flush_error: false,
            close_error: false,
            closes,
        };
        let owned = Arc::new(Mutex::new(BTreeSet::new()));
        let mut writer = FtpsWrite {
            session: Some(stream),
            data: Some(Box::new(data)),
            unique: "own".into(),
            target: "temp".into(),
            owned_path: "/pane/temp".into(),
            owned_files: owned.clone(),
            finished: false,
            failed: false,
            intent: None,
            binding: None,
            file_proof: None,
        };
        writer.write_all(b"eins").unwrap();
        let error = writer.flush().unwrap_err();
        assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
        let mut paths = crate::error::recovery_paths(&error);
        paths.sort();
        assert_eq!(paths, ["/pane/own", "/pane/temp"]);
        drop(writer);
        assert_eq!(peer.join().unwrap(), ["RNFR own", "RNTO temp"]);
        assert!(lock(&owned).is_empty());
    }
}

#[test]
fn retr_erhaelt_datenfehler_und_verlorene_kontrollantwort() {
    use std::io::{BufRead, BufReader};
    use std::os::fd::AsRawFd;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (connected, connected_rx) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket.write_all(b"220 RETR test\r\n").unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "EPSV");
        let passive = TcpListener::bind("127.0.0.1:0").unwrap();
        socket
            .write_all(
                format!(
                    "229 Passive (|||{}|)\r\n",
                    passive.local_addr().unwrap().port()
                )
                .as_bytes(),
            )
            .unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "RETR own");
        socket.write_all(b"150 Data\r\n").unwrap();
        let (data, _) = passive.accept().unwrap();
        // accept allein beweist nicht, dass clientseitiges connect bereits
        // erfolgreich zurückkam. Der RST soll den Inhaltsread treffen, nicht
        // schon die Anlage des noch gar nicht zurückgegebenen Datenstroms.
        connected_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // Der eigene Datensocket wird gezielt mit RST beendet; die Kontrollseite
        // fällt unabhängig ohne Abschlussantwort. Keine echten Server betroffen.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    data.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_LINGER,
                    std::ptr::from_ref(&linger).cast(),
                    std::mem::size_of_val(&linger) as libc::socklen_t,
                )
            },
            0
        );
        drop(data);
    });
    let socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut stream = RustlsFtpStream::connect_with_stream(socket)
        .unwrap()
        .passive_stream_builder(move |address| {
            let data = TcpStream::connect(address).map_err(FtpError::ConnectionError)?;
            connected.send(()).unwrap();
            Ok(data)
        });
    stream.set_mode(Mode::ExtendedPassive);
    let error = read_remote_hash(&mut stream, "own").unwrap_err();
    peer.join().unwrap();
    assert!(error.to_string().contains("FTPS-RETR Inhalt"), "{error}");
    assert!(error.to_string().contains("FTPS-RETR Abschluss"), "{error}");
    assert_eq!(crate::error_code(&error), ErrorCode::ConnectionLost);
}

// Der Peer wartet vor der Datenauswertung auf das Transportende. Damit bleibt
// ein gültiges TLS-Sessionticket nachweislich ungelesen, wenn der Upload endet;
// das zufällige Timing des libunftp-Workers ist für diesen Gegenbeleg irrelevant.
#[test]
fn stou_abschluss_erhaelt_datenkanal_bis_zur_kontrollantwort() {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (pin, tls) = server_tls();
    let (ticket_sent, ticket_ready) = mpsc::channel();
    let (observed, observation) = mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut raw_control = Some(socket.try_clone().unwrap());
        let mut control: BufReader<Box<dyn PeerStream>> = BufReader::new(Box::new(socket));
        control
            .get_mut()
            .write_all(b"220 STOU transport peer\r\n")
            .unwrap();
        let mut passive = None;
        loop {
            let mut line = String::new();
            if control.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let command = line.trim_end_matches(['\r', '\n']);
            let response = match command {
                "AUTH TLS" => {
                    control.get_mut().write_all(b"234 TLS\r\n").unwrap();
                    control.get_mut().flush().unwrap();
                    control = BufReader::new(Box::new(rustls::StreamOwned::new(
                        rustls::ServerConnection::new(tls.clone()).unwrap(),
                        raw_control.take().unwrap(),
                    )));
                    continue;
                }
                "PBSZ 0" | "PROT P" | "TYPE I" | "OPTS UTF8 ON" => "200 OK\r\n".to_owned(),
                value if value.starts_with("USER ") => "331 Password\r\n".to_owned(),
                value if value.starts_with("PASS ") => "230 OK\r\n".to_owned(),
                "FEAT" => "211-Features\r\n MLST\r\n UTF8\r\n211 End\r\n".to_owned(),
                "CWD /" => "250 OK\r\n".to_owned(),
                "PWD" => "257 \"/\"\r\n".to_owned(),
                "EPSV" => {
                    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                    let port = listener.local_addr().unwrap().port();
                    passive = Some(listener);
                    format!("229 Entering Extended Passive Mode (|||{port}|)\r\n")
                }
                "STOU" => {
                    control
                        .get_mut()
                        .write_all(b"150 FILE: owned-stou\r\n")
                        .unwrap();
                    control.get_mut().flush().unwrap();
                    let (socket, _) = passive.take().unwrap().accept().unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    socket
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut data = rustls::StreamOwned::new(
                        rustls::ServerConnection::new(tls.clone()).unwrap(),
                        socket,
                    );
                    while data.conn.is_handshaking() || data.conn.wants_write() {
                        data.conn.complete_io(&mut data.sock).unwrap();
                    }
                    ticket_sent.send(()).unwrap();
                    let mut bytes = Vec::new();
                    let mut close_notify = false;
                    let outcome = loop {
                        match data.conn.process_new_packets() {
                            Ok(state) => close_notify |= state.peer_has_closed(),
                            Err(error) => break Err(error.to_string()),
                        }
                        let mut buffer = [0; 64];
                        loop {
                            match data.conn.reader().read(&mut buffer) {
                                Ok(0) | Err(_) => break,
                                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                            }
                        }
                        match data.conn.read_tls(&mut data.sock) {
                            Ok(0) => break Ok(()),
                            Ok(_) => {}
                            Err(error) => break Err(format!("{:?}", error.kind())),
                        }
                    };
                    let success = outcome.is_ok() && close_notify && bytes == b"eins";
                    observed.send((bytes, close_notify, outcome)).unwrap();
                    drop(data);
                    if success {
                        "226 Transfer complete\r\n".to_owned()
                    } else {
                        "451 Incomplete transport\r\n".to_owned()
                    }
                }
                "QUIT" => {
                    control.get_mut().write_all(b"221 Bye\r\n").unwrap();
                    control.get_mut().flush().unwrap();
                    break;
                }
                _ => panic!("Unerwarteter Befehlstyp im STOU-Gegenserver"),
            };
            control.get_mut().write_all(response.as_bytes()).unwrap();
            control.get_mut().flush().unwrap();
        }
    });
    let config = FtpsConnectionConfig {
        host: "localhost".into(),
        alternate_hosts: Vec::new(),
        port,
        username: "anonymous".into(),
        requested_root: "/".into(),
        transport_security: FtpTransportSecurity::ExplicitTls,
        certificate: Some(pin),
        certificate_policy: Some(FtpsCertificatePolicy::PinnedCertificate),
        password_reference: crate::KeychainSecretReference::new("Doppeldecker-Test", "stou")
            .unwrap(),
    };
    let mut session = connect_once(&config, &TestSecrets).unwrap();
    let started = Instant::now();
    let (name, mut data) = start_stou(&mut session.stream).unwrap();
    data.write_all(b"eins").unwrap();
    data.flush().unwrap();
    ticket_ready.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut encrypted = [0];
    assert_eq!(data.socket().peek(&mut encrypted).unwrap(), 1);
    let completion = finish_put(&mut session.stream, data);
    let observed = observation.recv_timeout(Duration::from_secs(5)).unwrap();
    let _ = session.stream.quit();
    peer.join().unwrap();
    assert!(completion.is_ok(), "STOU={name}, akzeptiert=4, Flush=Ok, Dauer={:?}, Kontrollabschluss={completion:?}, Peer={observed:?}", started.elapsed());
    assert_eq!(observed, (b"eins".to_vec(), true, Ok(())));
}

#[test]
fn parallele_neuprobe_entzieht_keine_freigabe_des_laufenden_uploads() {
    let fixture = Fixture::new(true);
    let intent_config = private_config();
    let directory = std::fs::canonicalize(intent_config.path()).unwrap();
    let first_path = directory.join("operations-queue-window-first.json");
    let process_lock = Arc::new(crate::OperationQueueProcessLock::acquire(&first_path).unwrap());
    let first_queue =
        crate::OperationQueue::open_with_process_lock(&first_path, process_lock.clone()).unwrap();
    let second_queue = crate::OperationQueue::open_with_process_lock(
        directory.join("operations-queue-window-second.json"),
        process_lock,
    )
    .unwrap();
    let shared = FtpsVfs::connect(fixture.config.clone(), Arc::new(TestSecrets)).unwrap();
    let first = shared
        .for_queue_preparation(&first_queue, Path::new("/"))
        .unwrap();
    let second = shared
        .for_queue_preparation(&second_queue, Path::new("/"))
        .unwrap();
    first
        .probe_add_only_capabilities_at(Path::new("/"))
        .unwrap();
    let mut writer = first.open_write_new(Path::new("/own-temp")).unwrap();
    writer.write_all(b"vollstaendiger Inhalt").unwrap();
    writer.flush().unwrap();
    // Beobachtete Grenze der zweiten Vorbereitung: alte globale Flags sind
    // widerrufen, die neue Probe hat noch keine Freigabe veröffentlicht.
    let _second_proof = second.begin_probe(ProbeKind::File, Path::new("/")).unwrap();
    first
        .rename_plain_once(Path::new("/own-temp"), Path::new("/finished"))
        .unwrap();
    assert_eq!(
        std::fs::read(fixture.root.path().join("finished")).unwrap(),
        b"vollstaendiger Inhalt"
    );
    assert!(!fixture.root.path().join("own-temp").exists());
    first.finish_operation_intents().unwrap();
}

#[test]
fn ftps_move_ohne_noreplace_kopiert_und_entfernt_nur_nach_bestaetigung() {
    for tls in [false, true] {
        for (copy_confirmed, remove_confirmed) in [(true, true), (false, true), (true, false)] {
            for selection in ["file", "directory", "mixed"] {
                let fixture = Fixture::new(tls);
                let source = fixture.root.path().join("from");
                let target = fixture.root.path().join("to");
                std::fs::create_dir(&source).unwrap();
                std::fs::create_dir(&target).unwrap();
                let mut items = Vec::new();
                let mut expected = Vec::new();
                if selection != "directory" {
                    std::fs::write(source.join("file.txt"), b"Datei bleibt vollstaendig").unwrap();
                    items.push("/from/file.txt".to_owned());
                    expected.push("file.txt");
                }
                if selection != "file" {
                    std::fs::create_dir(source.join("folder")).unwrap();
                    std::fs::create_dir(source.join("folder/empty")).unwrap();
                    std::fs::write(
                        source.join("folder/nested.txt"),
                        b"Datei bleibt vollstaendig",
                    )
                    .unwrap();
                    items.push("/from/folder".to_owned());
                    expected.push("folder/nested.txt");
                }
                let config = private_config();
                let directory = std::fs::canonicalize(config.path()).unwrap();
                let queue =
                    crate::OperationQueue::open(directory.join("operations-queue.json")).unwrap();
                queue
                    .enqueue(crate::QueueOperation::ProviderFile {
                        kind: crate::FileQueueKind::Move,
                        source: Box::new(crate::BidirectionalQueueEndpoint::Ftps {
                            connection: Box::new(fixture.config.clone()),
                            root: "/from".into(),
                        }),
                        target: Box::new(crate::BidirectionalQueueEndpoint::Ftps {
                            connection: Box::new(fixture.config.clone()),
                            root: "/to".into(),
                        }),
                        items: items.clone(),
                        destination: "/to".into(),
                        overwrite: false,
                        expected_conflicts: vec![],
                        source_tree_expected: None,
                        expected_conflict_proofs: vec![],
                        keep_both_targets: vec![],
                        best_effort_commit_expected: None,
                        local_copy_new_best_effort_expected: None,
                        copy_new_best_effort_expected: Some(copy_confirmed),
                        replace_best_effort_expected: Some(false),
                        remove_source_best_effort_expected: Some(remove_confirmed),
                    })
                    .unwrap();
                let entry = queue.claim_next().unwrap().unwrap();
                let result = crate::execute_provider_file_queue_entry(
                    &queue,
                    entry,
                    &crate::LocalQueueExecution {
                        sandbox: None,
                        sync_quarantine: None,
                        max_delete_entries: 100,
                        max_delete_fraction: 0.25,
                    },
                    Arc::new(TestSecrets),
                    &crate::Cancel::new(),
                    &mut |_| {},
                );
                if copy_confirmed && remove_confirmed {
                    assert!(
                        matches!(result, Ok(crate::QueueExecutionResult::File(_))),
                        "F6-Rückfall fehlt: TLS={tls}, {result:?}"
                    );
                    for path in &expected {
                        assert_eq!(
                            std::fs::read(target.join(path)).unwrap(),
                            b"Datei bleibt vollstaendig"
                        );
                    }
                    if selection != "file" {
                        assert!(target.join("folder/empty").is_dir());
                    }
                } else {
                    assert!(
                        matches!(
                            result,
                            Ok(crate::QueueExecutionResult::AwaitingConfirmation { .. })
                        ),
                        "Best Effort blieb unbestätigt: TLS={tls}, {result:?}"
                    );
                    for path in &expected {
                        assert_eq!(
                            std::fs::read(source.join(path)).unwrap(),
                            b"Datei bleibt vollstaendig"
                        );
                    }
                    if selection != "file" {
                        assert!(source.join("folder/empty").is_dir());
                    }
                }
                assert!(!std::fs::read_dir(&directory).unwrap().any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".recovery.json")));
                assert_eq!(
                    std::fs::read_dir(&source).unwrap().count(),
                    usize::from(!(copy_confirmed && remove_confirmed)) * items.len()
                );
                assert_eq!(
                    std::fs::read_dir(&target).unwrap().count(),
                    usize::from(copy_confirmed && remove_confirmed) * items.len()
                );
            }
        }
    }
}
#[test]
fn normaler_stou_name_ist_vor_dem_ersten_inhaltsbyte_dauerhaft_bekannt() {
    for tls in [false, true] {
        let fixture = Fixture::new(tls);
        std::fs::create_dir(fixture.root.path().join("pane")).unwrap();
        let intent_config = private_config();
        let directory = std::fs::canonicalize(intent_config.path()).unwrap();
        let queue = crate::OperationQueue::open(directory.join("operations-queue.json")).unwrap();
        let vfs = FtpsVfs::connect(fixture.config.clone(), Arc::new(TestSecrets))
            .unwrap()
            .for_queue_preparation(&queue, Path::new("/pane"))
            .unwrap();
        vfs.probe_add_only_capabilities_at(Path::new("/pane"))
            .unwrap();
        for (name, body) in [
            ("empty.tmp", b"".as_slice()),
            ("small.tmp", b"eins".as_slice()),
        ] {
            let path = Path::new("/pane").join(name);
            let mut writer = vfs.open_write_new(&path).unwrap();
            let records = std::fs::read_dir(&directory)
                .unwrap()
                .filter_map(|entry| {
                    let path = entry.unwrap().path();
                    path.to_string_lossy().ends_with(".recovery.json").then(|| {
                        serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap())
                            .unwrap()
                    })
                })
                .collect::<Vec<_>>();
            assert_eq!(records.len(), 1);
            let record = &records[0];
            assert_eq!(record["version"], 2);
            assert_eq!(record["target"], path.to_str().unwrap());
            assert_eq!(record["ftps"]["stou_name_unknown"], false);
            assert_eq!(record["ftps"]["roots"][0]["virtual_root"], "/pane");
            assert_eq!(record["ftps"]["paths"].as_array().unwrap().len(), 2);
            let serialized = serde_json::to_string(record).unwrap();
            assert!(!serialized.contains("password_reference"));
            assert!(!serialized.contains("anonymous-test"));
            writer.write_all(body).unwrap();
            writer.flush().unwrap();
            assert_eq!(
                std::fs::read(fixture.root.path().join("pane").join(name)).unwrap(),
                body
            );
            assert_eq!(
                read_remote_hash(
                    &mut connect_once(&fixture.config, &TestSecrets).unwrap().stream,
                    path.to_str().unwrap()
                )
                .unwrap(),
                blake3::hash(body)
            );
            vfs.finish_operation_intents().unwrap();
            assert!(!std::fs::read_dir(&directory).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".recovery.json")));
        }
    }
}
