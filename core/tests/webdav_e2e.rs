//! Lokaler HTTPS-WebDAV-Gegenlauf mit kurzlebigem Zertifikat und Wegwerf-Root.
//!
//! Der Server bindet nur Loopback. Das feste Testpasswort ist wertlos und gelangt
//! weder in eine Profil- noch in eine Queue-Datei.

use std::convert::Infallible;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use doppeldecker_core::bidirectional::finish_bidirectional_run_completion;
use doppeldecker_core::{
    discover_webdav_certificate, download_remote_snapshot, error_code,
    execute_prepared_sftp_queue_entry, execute_provider_file_queue_entry,
    prepare_provider_bidirectional_with_decisions, prepare_webdav_download,
    prepare_webdav_queue_entry, prepare_webdav_upload, BidirectionalQueueEndpoint,
    BinaryRelativePath, Cancel, Compare, ConflictDecision, ConflictResolution, ErrorCode,
    FileQueueKind, FtpsCertificatePolicy, KeychainSecretReference, LocalQueueExecution,
    OperationQueue, PairId, ProviderEndpoint, QueueOperation, QueueState, RootIdentity,
    SecretResolver, SecretString, SftpQueueDirection, SftpQueuePreparation, SyncMode, SyncOptions,
    Vfs, WebDavConnectionConfig, WebDavVfs,
};
use http_body_util::{BodyExt, Full};
use hyper::{server::conn::http1, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use percent_encoding::percent_decode_str;
use rcgen::generate_simple_self_signed;
use rustls::pki_types::PrivatePkcs8KeyDer;
use tokio_rustls::TlsAcceptor;

const AUTHORIZATION: &str = "Basic dGVzdGVyOndlcnRsb3M=";
type TestBody = Full<hyper::body::Bytes>;

struct TestSecrets;

impl SecretResolver for TestSecrets {
    fn resolve(&self, _reference: &KeychainSecretReference) -> std::io::Result<SecretString> {
        SecretString::new("wertlos".into())
    }
}

#[test]
fn webdav_pin_auth_listing_stream_und_reconnect_bleiben_gebunden() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("inhalt.txt"), b"WebDAV Testinhalt\n").unwrap();
    std::fs::write(
        root.path().join("literal%2F.txt"),
        b"Prozent bleibt Bestandteil des Namens\n",
    )
    .unwrap();
    std::fs::create_dir(root.path().join("ordner")).unwrap();
    std::fs::create_dir(root.path().join("auswahl")).unwrap();

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
    let server_root = root.path().to_path_buf();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let acceptor = TlsAcceptor::from(Arc::new(server_tls));
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .unwrap();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                let server_root = server_root.clone();
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let service = service_fn(move |request| {
                        let server_root = server_root.clone();
                        async move {
                            let authorized = request
                                .headers()
                                .get(hyper::header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                == Some(AUTHORIZATION);
                            if !authorized {
                                let mut response = Response::new(empty_body());
                                *response.status_mut() = StatusCode::UNAUTHORIZED;
                                return Ok::<_, Infallible>(response);
                            }
                            Ok::<_, Infallible>(mock_response(request, &server_root).await)
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
    });
    wait_for_server(port);

    let origin = format!("https://localhost:{port}");
    let pin = discover_webdav_certificate(&origin).unwrap();
    let config = WebDavConnectionConfig {
        origin,
        username: "tester".into(),
        requested_root: "/".into(),
        certificate: pin,
        certificate_policy: FtpsCertificatePolicy::PinnedCertificate,
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "webdav").unwrap(),
    };
    let mut wrong_pin = config.clone();
    wrong_pin.certificate.sha256[0] ^= 0xff;
    assert!(WebDavVfs::connect(wrong_pin, Arc::new(TestSecrets)).is_err());
    let mut untrusted_public = config.clone();
    untrusted_public.certificate_policy = FtpsCertificatePolicy::WebPkiAndPin;
    assert!(WebDavVfs::connect(untrusted_public, Arc::new(TestSecrets)).is_err());

    let slash_root = root.path().join("slash-required");
    std::fs::create_dir(&slash_root).unwrap();
    std::fs::write(slash_root.join("datei.txt"), b"Slashform").unwrap();
    let mut slash_config = config.clone();
    slash_config.requested_root = "/slash-required/".into();
    let slash_vfs = WebDavVfs::connect(slash_config, Arc::new(TestSecrets)).unwrap();
    let slash_listing = slash_vfs.list_complete(Path::new("/")).unwrap();
    assert!(slash_listing
        .entries
        .iter()
        .any(|entry| entry.entry.name == "datei.txt"));
    std::fs::remove_file(slash_root.join("datei.txt")).unwrap();
    std::fs::remove_dir(slash_root).unwrap();

    // Ein Depth:0-PROPFIND darf den ausdrücklich gewählten Root nicht anhand eines
    // fremden href erweitern. Der Testserver antwortet für `/auswahl` absichtlich
    // mit der Serverwurzel `/`; der Verbindungsaufbau muss vor jeder Nutzung enden.
    let mut wrong_root_href = config.clone();
    wrong_root_href.requested_root = "/auswahl".into();
    assert!(WebDavVfs::connect(wrong_root_href, Arc::new(TestSecrets)).is_err());
    std::fs::remove_dir(root.path().join("auswahl")).unwrap();

    let vfs = Arc::new(WebDavVfs::connect(config.clone(), Arc::new(TestSecrets)).unwrap());
    let RootIdentity::WebDav(initial_root_identity) = vfs.root_identity(Path::new("/")).unwrap()
    else {
        panic!("WebDAV-VFS lieferte keine WebDAV-Rootidentität");
    };
    assert!(initial_root_identity.root_etag.starts_with('"'));
    assert!(!initial_root_identity.root_etag.starts_with("W/"));
    let listing = vfs.list_complete(Path::new("/")).unwrap();
    assert!(listing.complete);
    assert!(listing.provider_token.is_some());
    assert_eq!(listing.entries.len(), 3);
    let file = listing
        .entries
        .iter()
        .find(|entry| entry.entry.name == "inhalt.txt")
        .unwrap();
    assert!(file.entry.is_file);
    assert!(file.provider_token.is_some());

    let mut bytes = Vec::new();
    vfs.open_read(Path::new("/inhalt.txt"))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"WebDAV Testinhalt\n");
    let mut percent_bytes = Vec::new();
    vfs.open_read(Path::new("/literal%2F.txt"))
        .unwrap()
        .read_to_end(&mut percent_bytes)
        .unwrap();
    assert_eq!(percent_bytes, b"Prozent bleibt Bestandteil des Namens\n");
    let snapshot_cache = tempfile::tempdir().unwrap();
    let snapshot = download_remote_snapshot(
        vfs.as_ref(),
        Path::new("/inhalt.txt"),
        snapshot_cache.path(),
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(snapshot.path).unwrap(),
        b"WebDAV Testinhalt\n"
    );

    let generation = vfs.session_generation();
    vfs.reconnect().unwrap();
    assert_eq!(vfs.session_generation(), generation + 1);
    assert_eq!(vfs.identity().certificate, config.certificate);
    let missing_probe = vfs.open_write_new(Path::new("/gesperrt")).err().unwrap();
    assert_eq!(error_code(&missing_probe), ErrorCode::Unsupported);
    assert_eq!(
        error_code(&vfs.open_write(Path::new("/inhalt.txt")).err().unwrap()),
        ErrorCode::Unsupported
    );
    assert!(!root.path().join("gesperrt").exists());
    assert!(!vfs.capabilities().permits_directory_create());

    // Eine Probe kann die fehlende gemeinsame Elternbindung nicht nachrüsten.
    // Alle strikten Einstiegspunkte enden deshalb ohne künstliche Mutation.
    for result in [
        vfs.probe_directory_create_capability(),
        vfs.probe_rename_capability(),
        vfs.probe_permanent_delete_capability(),
        vfs.probe_add_only_capabilities(),
    ] {
        assert_eq!(error_code(&result.unwrap_err()), ErrorCode::Unsupported);
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 3);
    std::fs::create_dir(root.path().join("f5-target")).unwrap();
    let f5_source = tempfile::tempdir().unwrap();
    std::fs::write(f5_source.path().join("auswahl.txt"), b"WebDAV F5 Auswahl\n").unwrap();
    let f5_queue_dir = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let f5_queue =
        OperationQueue::open(f5_queue_dir.path().join("provider-file-queue.json")).unwrap();
    f5_queue
        .enqueue(QueueOperation::ProviderFile {
            source_tree_expected: None,
            kind: FileQueueKind::Copy,
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: f5_source.path().to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(config.clone()),
                root: "/f5-target".into(),
            }),
            items: vec![f5_source
                .path()
                .join("auswahl.txt")
                .to_string_lossy()
                .into_owned()],
            destination: "/f5-target".into(),
            overwrite: false,
            expected_conflicts: Vec::new(),
            keep_both_targets: Vec::new(),
            expected_conflict_proofs: Vec::new(),
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: Some(true),
            replace_best_effort_expected: Some(false),
            remove_source_best_effort_expected: None,
        })
        .unwrap();
    let claimed = f5_queue.claim_next().unwrap().unwrap();
    execute_provider_file_queue_entry(
        &f5_queue,
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
    )
    .unwrap();
    assert_eq!(
        std::fs::read(root.path().join("f5-target/auswahl.txt")).unwrap(),
        b"WebDAV F5 Auswahl\n"
    );
    assert_eq!(f5_queue.snapshot().entries[0].state, QueueState::Completed);
    drop(f5_queue);

    // Eine frühere Ersetzungsprobe bindet die bestätigte Altzielversion nicht
    // gemeinsam mit allen Eltern. Auch persistierte Best-Effort-Flags dürfen
    // diese fehlende Freigabe für lokales Copy nicht ersetzen.
    std::fs::write(
        f5_source.path().join("ersetzen.txt"),
        b"neue WebDAV-Version mit anderer Laenge\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("f5-target/ersetzen.txt"),
        b"alte WebDAV-Version\n",
    )
    .unwrap();
    let replace_queue =
        OperationQueue::open(f5_queue_dir.path().join("provider-replace-queue.json")).unwrap();
    replace_queue
        .enqueue(QueueOperation::ProviderFile {
            source_tree_expected: None,
            kind: FileQueueKind::Copy,
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: f5_source.path().to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(config.clone()),
                root: "/f5-target".into(),
            }),
            items: vec![f5_source
                .path()
                .join("ersetzen.txt")
                .to_string_lossy()
                .into_owned()],
            destination: "/f5-target".into(),
            overwrite: true,
            expected_conflicts: vec!["/f5-target/ersetzen.txt".into()],
            keep_both_targets: Vec::new(),
            expected_conflict_proofs: doppeldecker_core::capture_file_conflict_proofs(
                vfs.as_ref(),
                &["/f5-target/ersetzen.txt".into()],
            )
            .unwrap(),
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: Some(false),
            replace_best_effort_expected: Some(true),
            remove_source_best_effort_expected: None,
        })
        .unwrap();
    let replace_result = execute_provider_file_queue_entry(
        &replace_queue,
        replace_queue.claim_next().unwrap().unwrap(),
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
    assert_eq!(error_code(&replace_result), ErrorCode::Unsupported);
    assert_eq!(
        std::fs::read(root.path().join("f5-target/ersetzen.txt")).unwrap(),
        b"alte WebDAV-Version\n"
    );
    assert_eq!(
        replace_queue.snapshot().entries[0].state,
        QueueState::Failed
    );
    drop(replace_queue);

    // Strikter Upload und Remote-Zielmatrix erhalten keine Schreibfreigabe.
    let upload = tempfile::tempdir().unwrap();
    std::fs::write(upload.path().join("upload.txt"), b"lokaler Upload\n").unwrap();
    std::fs::create_dir(root.path().join("upload-target")).unwrap();
    assert_eq!(
        error_code(
            &prepare_webdav_upload(
                upload.path(),
                vfs.clone(),
                Path::new("/upload-target"),
                SyncOptions {
                    compare: Compare::ContentHash,
                    ..SyncOptions::default()
                },
            )
            .err()
            .unwrap()
        ),
        ErrorCode::Unsupported
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("upload-target"))
            .unwrap()
            .count(),
        0
    );
    let endpoint = ProviderEndpoint::webdav(vfs.clone(), Path::new("/f5-target")).unwrap();
    let files = endpoint.probe_file_capabilities().unwrap();
    assert!(
        !files.copy_new
            && !files.copy_new_best_effort
            && !files.replace
            && !files.replace_best_effort
            && !files.remove_source
    );
    let deletion = endpoint.probe_delete_capabilities().unwrap();
    assert!(!deletion.quarantine && !deletion.permanent);
    assert!(endpoint.probe_quarantine_restore_capability().is_err());
    assert!(endpoint.probe_quarantine_purge_capability().is_err());
    assert_eq!(
        std::fs::read(root.path().join("f5-target/auswahl.txt")).unwrap(),
        b"WebDAV F5 Auswahl\n"
    );

    // WebDAV bleibt bidirektional reine Quelle; alle Mutationen und Recovery-Proben
    // liegen am lokalen Ziel mit starkem Namespace-Vertrag.
    std::fs::create_dir(root.path().join("bidirektional")).unwrap();
    let bidi_area = tempfile::tempdir().unwrap();
    let bidi_local = bidi_area.path().join("lokal");
    std::fs::create_dir(&bidi_local).unwrap();
    let bidi_baseline = bidi_area.path().join("state/webdav-pair.json");
    let pair = PairId([111; 16]);
    let initial = prepare_provider_bidirectional_with_decisions(
        pair,
        "WebDAV-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::webdav(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[],
    )
    .unwrap();
    let phrase = initial.preview().confirmation_phrase.clone();
    initial
        .confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    finish_bidirectional_run_completion(&bidi_baseline).unwrap();
    std::fs::write(
        root.path().join("bidirektional/remote.txt"),
        b"WebDAV bidirektional\n",
    )
    .unwrap();
    let remote_to_local = prepare_provider_bidirectional_with_decisions(
        pair,
        "WebDAV-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::webdav(vfs.clone(), Path::new("/bidirektional")).unwrap(),
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
        b"WebDAV bidirektional\n"
    );
    std::fs::write(
        root.path().join("bidirektional/remote.txt"),
        b"WebDAV bidirektional aktualisiert\n",
    )
    .unwrap();
    let replacement = prepare_provider_bidirectional_with_decisions(
        pair,
        "WebDAV-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::webdav(vfs.clone(), Path::new("/bidirektional")).unwrap(),
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
        "WebDAV-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::webdav(vfs.clone(), Path::new("/bidirektional")).unwrap(),
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
        "WebDAV-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::webdav(vfs.clone(), Path::new("/bidirektional")).unwrap(),
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
        b"WebDAV bidirektional aktualisiert\n"
    );
    std::fs::remove_file(root.path().join("bidirektional/remote.txt")).unwrap();
    std::fs::create_dir(root.path().join("bidirektional/remote.txt")).unwrap();
    std::fs::write(
        root.path().join("bidirektional/remote.txt/inhalt.txt"),
        b"WebDAV Typwechsel\n",
    )
    .unwrap();
    let type_swap = prepare_provider_bidirectional_with_decisions(
        pair,
        "WebDAV-E2E-Paar",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::webdav(vfs.clone(), Path::new("/bidirektional")).unwrap(),
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
        b"WebDAV Typwechsel\n"
    );

    let download_area = tempfile::tempdir().unwrap();
    let download_target = download_area.path().join("ziel");
    std::fs::create_dir(&download_target).unwrap();
    let prepared = prepare_webdav_download(
        vfs.clone(),
        Path::new("/"),
        &download_target,
        SyncOptions {
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        },
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
        b"WebDAV Testinhalt\n"
    );

    std::fs::write(root.path().join("inhalt.txt"), b"WebDAV aktualisiert\n").unwrap();
    let update = prepare_webdav_download(
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
        b"WebDAV aktualisiert\n"
    );

    for index in 0..25 {
        std::fs::write(
            root.path().join(format!("datei-{index:02}.txt")),
            b"mirror\n",
        )
        .unwrap();
    }
    let fill = prepare_webdav_download(
        vfs.clone(),
        Path::new("/"),
        &download_target,
        SyncOptions {
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        },
    )
    .unwrap();
    let phrase = fill.preview().confirmation_phrase.clone();
    fill.confirm(&phrase, false)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    std::fs::write(download_target.join("ueberzaehlig.txt"), b"weg\n").unwrap();
    let mirror = prepare_webdav_download(
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

    let queue_target = download_area.path().join("queue-ziel");
    std::fs::create_dir(&queue_target).unwrap();
    let queue_options = SyncOptions {
        compare: Compare::ContentHash,
        ..SyncOptions::default()
    };
    let preview =
        prepare_webdav_download(vfs, Path::new("/"), &queue_target, queue_options.clone()).unwrap();
    let expected_plan = preview.preview().plan.clone();
    drop(preview);
    let queue_dir = tempfile::tempdir().unwrap();
    let queue = OperationQueue::open(queue_dir.path().join("queue.json")).unwrap();
    let queued = queue
        .enqueue(QueueOperation::WebDavSync {
            direction: SftpQueueDirection::Download,
            connection: Box::new(config),
            local_root: queue_target.to_string_lossy().into_owned(),
            remote_root: "/".into(),
            options: queue_options,
            expected_plan: Box::new(expected_plan),
        })
        .unwrap();
    let claimed = queue.claim_next().unwrap().unwrap();
    let prepared = match prepare_webdav_queue_entry(&queue, claimed, Arc::new(TestSecrets)).unwrap()
    {
        SftpQueuePreparation::Ready(prepared) => *prepared,
        SftpQueuePreparation::AwaitingConfirmation { reason } => {
            panic!("WebDAV-Queue war unerwartet nicht bereit: {reason}")
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
        b"WebDAV aktualisiert\n"
    );
}

async fn mock_response(
    mut request: Request<hyper::body::Incoming>,
    root: &Path,
) -> Response<TestBody> {
    let path = request.uri().path();
    if path.starts_with("/slash-required")
        && !path.ends_with('/')
        && matches!(request.method().as_str(), "OPTIONS" | "PROPFIND")
        && local_path(root, path).is_some_and(|local| local.is_dir())
    {
        return Response::builder()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header("Location", format!("{path}/"))
            .body(empty_body())
            .unwrap();
    }
    if request.method() == hyper::Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header("DAV", "1")
            .header("Server", "Doppeldecker-WebDAV-Test")
            .body(empty_body())
            .unwrap();
    }
    if request.method().as_str() == "PROPFIND" {
        let depth = request
            .headers()
            .get("depth")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let decoded_request_path = match percent_decode_str(path).decode_utf8() {
            Ok(path) => path,
            Err(_) => return empty_response(StatusCode::BAD_REQUEST),
        };
        let local = match local_path(root, path) {
            Some(path) if path.exists() => path,
            _ => return empty_response(StatusCode::NOT_FOUND),
        };
        let mut responses = if depth == "0" && path == "/auswahl" {
            // Regression: Der Client darf die angefragte Collection nicht durch
            // das href des Servers ersetzen und dadurch den Root auf `/` erweitern.
            resource_response("/", &local)
        } else {
            resource_response(&decoded_request_path, &local)
        };
        if depth == "1" && local.is_dir() {
            let mut children = std::fs::read_dir(&local)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect::<Vec<_>>();
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                let name = child.file_name().to_string_lossy().into_owned();
                let href = if decoded_request_path == "/" {
                    format!("/{name}")
                } else {
                    format!("{}/{name}", decoded_request_path.trim_end_matches('/'))
                };
                responses.push_str(&resource_response(&href, &child.path()));
            }
        } else if depth != "0" {
            return empty_response(StatusCode::BAD_REQUEST);
        }
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">{responses}</D:multistatus>"
        );
        if depth == "1"
            && local.is_dir()
            && path.contains("/.doppeldecker-mkdir-probe-")
            && root.join(".simulate-probe-swap-before-mkcol").exists()
            && std::fs::read_dir(&local).unwrap().next().is_none()
        {
            swap_probe_collection(root, &local, "mkcol");
        }
        if depth == "1"
            && path.ends_with("/.depth-zero-sentinel")
            && root.join(".simulate-probe-swap-before-put").exists()
            && path.contains("/.doppeldecker-rename-probe-")
        {
            swap_probe_collection(root, local.parent().unwrap(), "put");
        }
        if depth == "1"
            && path.ends_with("/.depth-zero-sentinel")
            && root.join(".probe-swap-cleanup-armed").exists()
        {
            swap_probe_collection(root, local.parent().unwrap(), "cleanup");
        }
        if depth == "0" && path.ends_with("/etag-race.txt") && local.is_file() {
            // Die Antwort trägt noch die ETag von „vorher!“. Erst danach entsteht
            // der gleich große Austausch, sodass DELETE nur mit If-Match sicher
            // stoppen kann. Ohne den Header würde der Testserver die neue Datei
            // unwiederbringlich entfernen.
            std::fs::write(&local, b"nachher").unwrap();
        }
        if depth == "0" && path.ends_with("/dir-etag-race") && local.is_dir() {
            let marker = root.join(".dir-etag-race-swapped");
            if std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(marker)
                .is_ok()
            {
                // Die XML-Antwort enthält noch die starke ETag der ersten
                // Collection. Erst danach ersetzt der Testserver sie durch eine
                // andere leere Collection am selben Namen.
                std::fs::remove_dir(&local).unwrap();
                std::fs::create_dir(&local).unwrap();
            }
        }
        return Response::builder()
            .status(StatusCode::MULTI_STATUS)
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(full_body(xml))
            .unwrap();
    }
    if request.method() == hyper::Method::GET {
        let Some(local) = local_path(root, path).filter(|path| path.is_file()) else {
            return empty_response(StatusCode::NOT_FOUND);
        };
        let bytes = std::fs::read(local).unwrap();
        let etag = etag_for(&bytes);
        if request
            .headers()
            .get("if-match")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value != etag)
        {
            return empty_response(StatusCode::PRECONDITION_FAILED);
        }
        return Response::builder()
            .header("ETag", etag)
            .body(full_body(bytes))
            .unwrap();
    }
    if request.method().as_str() == "MKCOL" {
        record_probe_swap_followup(root, path, "MKCOL");
        if !tagged_collection_conditions_match(&request, root) {
            return empty_response(StatusCode::PRECONDITION_FAILED);
        }
        let Some(local) = local_path(root, path) else {
            return empty_response(StatusCode::BAD_REQUEST);
        };
        if path.ends_with("/missing-201-etag") && root.join(".omit-create-dir-etag").exists() {
            return match std::fs::create_dir(&local) {
                Ok(()) => empty_response(StatusCode::CREATED),
                Err(_) => empty_response(StatusCode::CONFLICT),
            };
        }
        if path.ends_with("/swapped-after-201") && root.join(".swap-create-dir-after-201").exists()
        {
            if std::fs::create_dir(&local).is_err() {
                return empty_response(StatusCode::CONFLICT);
            }
            let created_etag = directory_etag(&local);
            std::fs::remove_dir(&local).unwrap();
            std::fs::create_dir(&local).unwrap();
            std::fs::write(local.join("fremder-sentinel"), b"fremd\n").unwrap();
            return Response::builder()
                .status(StatusCode::CREATED)
                .header("ETag", created_etag)
                .body(empty_body())
                .unwrap();
        }
        if path.contains("/.doppeldecker-rename-probe-") && path.ends_with("/collection") {
            if root
                .join(".simulate-idempotent-rename-child-mkcol")
                .exists()
            {
                let _ = std::fs::create_dir(&local);
                let _ = std::fs::write(local.join("fremder-sentinel"), b"fremd\n");
                let _ = std::fs::write(root.join(".rename-child-idempotent-done"), b"an\n");
                return Response::builder()
                    .status(StatusCode::CREATED)
                    .header("ETag", directory_etag(&local))
                    .body(empty_body())
                    .unwrap();
            }
            if root.join(".simulate-swap-rename-child-after-201").exists()
                && !root.join(".rename-child-swap-done").exists()
            {
                std::fs::create_dir(&local).unwrap();
                let created_etag = directory_etag(&local);
                std::fs::remove_dir(&local).unwrap();
                std::fs::create_dir(&local).unwrap();
                std::fs::write(local.join("fremder-sentinel"), b"fremd\n").unwrap();
                std::fs::write(root.join(".rename-child-swap-done"), b"an\n").unwrap();
                return Response::builder()
                    .status(StatusCode::CREATED)
                    .header("ETag", created_etag)
                    .body(empty_body())
                    .unwrap();
            }
        }
        if root.join(".simulate-probe-swap-before-cleanup").exists()
            && path
                .rsplit('/')
                .next()
                .is_some_and(|name| name.starts_with(".doppeldecker-mkdir-probe-"))
            && local.join(".depth-zero-sentinel").exists()
        {
            std::fs::write(root.join(".probe-swap-cleanup-armed"), b"an\n").unwrap();
            return empty_response(StatusCode::METHOD_NOT_ALLOWED);
        }
        if root.join(".simulate-mkcol-swap-probe").exists()
            && path.contains("/.doppeldecker-mkdir-probe-")
        {
            let swap_done = root.join(".mkcol-swap-done");
            if path
                .rsplit('/')
                .next()
                .is_some_and(|name| name.starts_with(".doppeldecker-mkdir-probe-"))
                && !swap_done.exists()
            {
                std::fs::create_dir(&local).unwrap();
                let created_etag = directory_etag(&local);
                std::fs::remove_dir(&local).unwrap();
                std::fs::create_dir(&local).unwrap();
                std::fs::write(local.join("fremder-sentinel"), b"fremd\n").unwrap();
                std::fs::write(swap_done, b"getauscht\n").unwrap();
                return Response::builder()
                    .status(StatusCode::CREATED)
                    .header("ETag", created_etag)
                    .body(empty_body())
                    .unwrap();
            }
            std::fs::write(root.join(".mkcol-swap-followup-observed"), b"MKCOL\n").unwrap();
        }
        if root.join(".simulate-idempotent-mkdir-probe").exists()
            && path
                .rsplit('/')
                .next()
                .is_some_and(|name| name.starts_with(".doppeldecker-mkdir-probe-"))
        {
            let _ = std::fs::create_dir(&local);
            let _ = std::fs::write(local.join("fremder-sentinel"), b"fremd\n");
            return Response::builder()
                .status(StatusCode::CREATED)
                .header("ETag", directory_etag(&local))
                .body(empty_body())
                .unwrap();
        }
        return match std::fs::create_dir(&local) {
            Ok(()) => Response::builder()
                .status(StatusCode::CREATED)
                .header("ETag", directory_etag(&local))
                .body(empty_body())
                .unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                empty_response(StatusCode::PRECONDITION_FAILED)
            }
            Err(_) => empty_response(StatusCode::CONFLICT),
        };
    }
    if request.method() == hyper::Method::PUT {
        record_probe_swap_followup(root, path, "PUT");
        record_rename_child_followup(root, path, "PUT");
        if !tagged_collection_conditions_match(&request, root) {
            return empty_response(StatusCode::PRECONDITION_FAILED);
        }
        if root.join(".reject-put-during-mkdir-probe").exists()
            && path.contains("/.doppeldecker-mkdir-probe-")
        {
            return empty_response(StatusCode::METHOD_NOT_ALLOWED);
        }
        let Some(local) = local_path(root, path) else {
            return empty_response(StatusCode::BAD_REQUEST);
        };
        let swap_before_move = root.join(".simulate-probe-swap-before-move").exists()
            && path.contains("/.doppeldecker-rename-probe-")
            && path.ends_with("/collision");
        let swap_before_delete = root.join(".simulate-probe-swap-before-delete").exists()
            && path.contains("/.doppeldecker-delete-probe-")
            && path.ends_with("/file");
        let body = request.body_mut().collect().await.unwrap().to_bytes();
        let result = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&local)
            .and_then(|mut file| std::io::Write::write_all(&mut file, &body));
        let response = match result {
            Ok(()) => Response::builder()
                .status(StatusCode::CREATED)
                .header("ETag", etag_for(&body))
                .body(empty_body())
                .unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                empty_response(StatusCode::PRECONDITION_FAILED)
            }
            Err(_) => empty_response(StatusCode::CONFLICT),
        };
        if response.status() == StatusCode::CREATED && swap_before_move {
            swap_probe_collection(root, local.parent().unwrap(), "move");
        }
        if response.status() == StatusCode::CREATED && swap_before_delete {
            swap_probe_collection(root, local.parent().unwrap(), "delete");
        }
        return response;
    }
    if request.method().as_str() == "MOVE" {
        record_probe_swap_followup(root, path, "MOVE");
        record_rename_child_followup(root, path, "MOVE");
        if !tagged_collection_conditions_match(&request, root) {
            return empty_response(StatusCode::PRECONDITION_FAILED);
        }
        let destination = request
            .headers()
            .get("destination")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| url::Url::parse(value).ok());
        let (Some(from), Some(to)) = (
            local_path(root, path),
            destination.and_then(|url| local_path(root, url.path())),
        ) else {
            return empty_response(StatusCode::BAD_REQUEST);
        };
        if !from.exists() {
            return empty_response(StatusCode::NOT_FOUND);
        }
        if path.ends_with("/weak-collection") {
            std::fs::write(root.join(".weak-collection-move-observed"), b"MOVE\n").unwrap();
        }
        let overwrite = request
            .headers()
            .get("overwrite")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !matches!(overwrite, "F" | "T") {
            return empty_response(StatusCode::BAD_REQUEST);
        }
        {
            let etag = if from.is_file() {
                etag_for(&std::fs::read(&from).unwrap())
            } else {
                directory_etag(&from)
            };
            if request
                .headers()
                .get("if-match")
                .and_then(|value| value.to_str().ok())
                != Some(&etag)
            {
                return empty_response(StatusCode::PRECONDITION_FAILED);
            }
        }
        return if overwrite == "T" {
            // Das Queue-Ersetzen mit diesem Testnamen bildet das reale Rennen nach:
            // Das beim vorausgehenden PROPFIND noch vorhandene Ziel verschwindet vor
            // MOVE. Der Server legt den Namen dadurch neu an und antwortet korrekt
            // mit 201 statt 204; der Client muss trotzdem seinen normalen Readback
            // bis zum geprüften Inhalt ausführen.
            let target_disappeared = to.file_name().and_then(|name| name.to_str())
                == Some("ersetzen.txt")
                && std::fs::remove_file(&to).is_ok();
            match std::fs::rename(&from, &to) {
                Ok(()) if target_disappeared => empty_response(StatusCode::CREATED),
                Ok(()) => empty_response(StatusCode::NO_CONTENT),
                Err(_) => empty_response(StatusCode::CONFLICT),
            }
        } else {
            match rustix::fs::renameat_with(
                rustix::fs::CWD,
                &from,
                rustix::fs::CWD,
                &to,
                rustix::fs::RenameFlags::NOREPLACE,
            ) {
                Ok(()) => empty_response(StatusCode::CREATED),
                Err(error) if error == rustix::io::Errno::EXIST => {
                    empty_response(StatusCode::PRECONDITION_FAILED)
                }
                Err(_) => empty_response(StatusCode::CONFLICT),
            }
        };
    }
    if request.method() == hyper::Method::DELETE {
        record_probe_swap_followup(root, path, "DELETE");
        record_rename_child_followup(root, path, "DELETE");
        if !tagged_collection_conditions_match(&request, root) {
            return empty_response(StatusCode::PRECONDITION_FAILED);
        }
        let Some(local) = local_path(root, path) else {
            return empty_response(StatusCode::BAD_REQUEST);
        };
        if root.join(".simulate-idempotent-mkdir-probe").exists()
            && path.contains("/.doppeldecker-mkdir-probe-")
        {
            std::fs::write(root.join(".idempotent-probe-delete-observed"), b"DELETE\n").unwrap();
        }
        if root.join(".simulate-mkcol-swap-probe").exists()
            && path.contains("/.doppeldecker-mkdir-probe-")
        {
            std::fs::write(root.join(".mkcol-swap-followup-observed"), b"DELETE\n").unwrap();
        }
        if local.is_file() {
            let etag = etag_for(&std::fs::read(&local).unwrap());
            if request
                .headers()
                .get("if-match")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value != etag)
            {
                return empty_response(StatusCode::PRECONDITION_FAILED);
            }
        } else if local.is_dir()
            && request
                .headers()
                .get("if-match")
                .and_then(|value| value.to_str().ok())
                != Some(directory_etag(&local).as_str())
        {
            // Der echte Provider darf eine Collection nur mit der unmittelbar
            // rückgelesenen starken ETag entfernen. Der Testserver lehnt deshalb
            // sowohl eine falsche als auch eine fehlende Bindung sichtbar ab.
            return empty_response(StatusCode::PRECONDITION_FAILED);
        }
        let result = if local.is_dir()
            && request
                .headers()
                .get("depth")
                .and_then(|value| value.to_str().ok())
                == Some("0")
        {
            std::fs::remove_dir(local)
        } else if local.is_dir() {
            std::fs::remove_dir_all(local)
        } else {
            std::fs::remove_file(local)
        };
        return match result {
            Ok(()) => empty_response(StatusCode::NO_CONTENT),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                empty_response(StatusCode::NOT_FOUND)
            }
            Err(_) => empty_response(StatusCode::CONFLICT),
        };
    }
    empty_response(StatusCode::METHOD_NOT_ALLOWED)
}

fn tagged_collection_conditions_match(
    request: &Request<hyper::body::Incoming>,
    root: &Path,
) -> bool {
    let Some(mut value) = request
        .headers()
        .get("if")
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let mut count = 0;
    let mut matched = false;
    while !value.is_empty() {
        let Some(after_open) = value.strip_prefix('<') else {
            return false;
        };
        let Some((url, after_url)) = after_open.split_once('>') else {
            return false;
        };
        let Some(after_prefix) = after_url.strip_prefix(" ([") else {
            return false;
        };
        let Some((etag, after_etag)) = after_prefix.split_once("])") else {
            return false;
        };
        let Ok(url) = url::Url::parse(url) else {
            return false;
        };
        let Some(local) = local_path(root, url.path()).filter(|path| path.is_dir()) else {
            return false;
        };
        matched |= etag == directory_etag(&local);
        count += 1;
        value = after_etag.strip_prefix(' ').unwrap_or(after_etag);
    }
    count > 0 && matched
}

fn swap_probe_collection(root: &Path, probe: &Path, mutation: &str) {
    let done = root.join(format!(".probe-swap-{mutation}-done"));
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(done)
        .is_err()
    {
        return;
    }
    std::fs::remove_dir_all(probe).unwrap();
    std::fs::create_dir(probe).unwrap();
    std::fs::write(probe.join("fremder-sentinel"), b"fremd\n").unwrap();
}

fn record_probe_swap_followup(root: &Path, path: &str, method: &str) {
    for mutation in ["mkcol", "put", "move", "delete", "cleanup"] {
        if root.join(format!(".probe-swap-{mutation}-done")).exists() {
            std::fs::write(
                root.join(format!(".probe-swap-{mutation}-followup-observed")),
                format!("{method} {path}\n"),
            )
            .unwrap();
        }
    }
}

fn record_rename_child_followup(root: &Path, path: &str, method: &str) {
    for mutation in ["idempotent", "swap"] {
        if root.join(format!(".rename-child-{mutation}-done")).exists() {
            std::fs::write(
                root.join(format!(".rename-child-{mutation}-followup-observed")),
                format!("{method} {path}\n"),
            )
            .unwrap();
        }
    }
}

fn resource_response(href: &str, path: &Path) -> String {
    // PROPFIND liefert URL-Pfade, keine bereits dekodierten Dateinamen. Das
    // segmentweise Kodieren bildet insbesondere ein wörtliches `%2F` korrekt als
    // `%252F` ab, ohne die echten Schrägstriche zwischen Komponenten zu verändern.
    let href = encoded_href(href);
    if path.is_dir() {
        let name = path.file_name().and_then(|name| name.to_str());
        let etag = if name == Some("weak-collection") {
            "W/\"weak-collection\"".to_owned()
        } else {
            directory_etag(path)
        };
        let etag_property = if name == Some("missing-etag-collection") {
            String::new()
        } else {
            format!("<D:getetag>{etag}</D:getetag>")
        };
        format!(
            "<D:response><D:href>{href}/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype>{etag_property}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
            href = href.trim_end_matches('/')
        )
    } else {
        let bytes = std::fs::read(path).unwrap();
        format!(
            "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>{length}</D:getcontentlength><D:getlastmodified>Sun, 12 Jul 2026 20:00:00 GMT</D:getlastmodified><D:getetag>{etag}</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
            length = bytes.len(),
            etag = etag_for(&bytes)
        )
    }
}

fn encoded_href(path: &str) -> String {
    let mut url = url::Url::parse("https://webdav-test.invalid/").unwrap();
    {
        let mut segments = url.path_segments_mut().unwrap();
        segments.clear();
        for segment in path.split('/').filter(|segment| !segment.is_empty()) {
            segments.push(segment);
        }
    }
    url.path().to_owned()
}

fn directory_etag(path: &Path) -> String {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    format!("\"directory-{}-{}\"", metadata.dev(), metadata.ino())
}

fn etag_for(bytes: &[u8]) -> String {
    format!("\"{}\"", blake3::hash(bytes).to_hex())
}

fn local_path(root: &Path, request_path: &str) -> Option<std::path::PathBuf> {
    let decoded = percent_decode_str(request_path).decode_utf8().ok()?;
    let mut result = root.to_path_buf();
    for part in decoded.split('/').filter(|part| !part.is_empty()) {
        if matches!(part, "." | "..") || part.contains(['/', '\0']) {
            return None;
        }
        result.push(part);
    }
    Some(result)
}

fn empty_response(status: StatusCode) -> Response<TestBody> {
    Response::builder()
        .status(status)
        .body(empty_body())
        .unwrap()
}

fn empty_body() -> TestBody {
    Full::new(hyper::body::Bytes::new())
}

fn full_body(value: impl Into<hyper::body::Bytes>) -> TestBody {
    Full::new(value.into())
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
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
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("WebDAV-Testserver startete nicht");
}
