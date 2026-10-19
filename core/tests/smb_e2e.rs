//! Opt-in-Gegenlauf gegen einen isolierten lokalen Samba-Testshare.
//!
//! Der Aufrufer muss einen frischen Root unter `/tmp` bereitstellen. Nutzer und
//! Passwort sind ausschließlich wertlose Testdaten; das Queue-Journal erhält wie im
//! Produkt nur die Schlüsselbundreferenz, niemals den Wert.

use std::env;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use doppeldecker_core::bidirectional::finish_bidirectional_run_completion;
use doppeldecker_core::real_mode::build_listing_proof;
use doppeldecker_core::{
    discover_smb_server, error_code, execute_prepared_sftp_queue_entry,
    execute_provider_file_queue_entry, prepare_provider_bidirectional_with_decisions,
    prepare_smb_download, prepare_smb_queue_entry, prepare_smb_upload, BidirectionalQueueEndpoint,
    Cancel, Compare, ErrorCode, FileQueueKind, KeychainSecretReference, LocalQueueExecution,
    OperationQueue, PairId, ProviderEndpoint, QueueExecutionResult, QueueOperation, QueueState,
    SecretResolver, SecretString, SftpQueueDirection, SftpQueuePreparation, SmbConnectionConfig,
    SmbVfs, SyncMode, SyncOptions, Vfs,
};

const TEST_PASSWORD: &str = "Doppeldecker-SMB-E2E-wertlos-2026";

struct TestSecrets;

struct CountingTestSecrets(Arc<AtomicUsize>);

impl SecretResolver for CountingTestSecrets {
    fn resolve(&self, reference: &KeychainSecretReference) -> std::io::Result<SecretString> {
        self.0.fetch_add(1, Ordering::SeqCst);
        TestSecrets.resolve(reference)
    }
}

impl SecretResolver for TestSecrets {
    fn resolve(&self, _reference: &KeychainSecretReference) -> std::io::Result<SecretString> {
        SecretString::new(TEST_PASSWORD.into())
    }
}

#[test]
#[ignore = "benötigt einen isolierten lokalen Samba-Testshare"]
fn smb3_guid_paging_stream_quarantaene_reconnect_und_queue_bleiben_gebunden() {
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    let share = required("DOPPELDECKER_SMB_TEST_SHARE");
    let username = required("DOPPELDECKER_SMB_TEST_USER");
    let server_root = required("DOPPELDECKER_SMB_TEST_ROOT");
    assert!(server_root.starts_with("/tmp/doppeldecker-smb-e2e."));
    let server_root = Path::new(&server_root);

    let sandbox = server_root.join("sandbox");
    std::fs::create_dir_all(sandbox.join("paging")).unwrap();
    std::fs::create_dir_all(sandbox.join("upload-target")).unwrap();
    std::fs::create_dir_all(sandbox.join("download-source")).unwrap();
    std::fs::write(
        sandbox.join("download-source/remote.txt"),
        b"SMB remote eins\n",
    )
    .unwrap();
    // Genug Einträge erzwingen mehrere QUERY_DIRECTORY-Seiten. Der Provider darf
    // erst STATUS_NO_MORE_FILES als Vollständigkeitsbeweis akzeptieren.
    for index in 0..1_500 {
        std::fs::write(sandbox.join(format!("paging/datei-{index:04}.txt")), b"").unwrap();
    }

    let discovery = discover_smb_server(&host, port).unwrap();
    assert_eq!(discovery.dialect, 0x0311);
    assert!(discovery.encryption_capable);
    let config = SmbConnectionConfig {
        host,
        port,
        server_guid: discovery.server_guid,
        share,
        username,
        domain: String::new(),
        requested_root: "/sandbox".into(),
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "smb").unwrap(),
    };

    let mut wrong_guid = config.clone();
    wrong_guid.server_guid[0] ^= 0xff;
    let secret_requests = Arc::new(AtomicUsize::new(0));
    let error = SmbVfs::connect(
        wrong_guid,
        Arc::new(CountingTestSecrets(secret_requests.clone())),
    )
    .err()
    .expect("Abweichender SMB-Pin wurde akzeptiert");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(secret_requests.load(Ordering::SeqCst), 0,
        "Eine abweichende öffentliche Server-GUID darf keine Credentialauflösung oder NTLM-Anmeldung auslösen");

    eprintln!("SMB-E2E: GUID-Pin vor Credentialauflösung geprüft");

    // Der gewählte Unterordner bleibt les-/schreibbar, während der Profilroot
    // nur durchsuchbar ist. Die Probe darf dessen fehlendes Listing nicht übergehen.
    {
        use std::os::unix::fs::PermissionsExt;
        let selected = sandbox.join("scoped-probes");
        std::fs::create_dir(&selected).unwrap();
        let probe_vfs = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets)).unwrap());
        assert_eq!(probe_vfs.identity().canonical_root, "/sandbox");
        let selected_permissions = std::fs::metadata(&selected).unwrap().permissions();
        std::fs::set_permissions(&selected, std::fs::Permissions::from_mode(0o000)).unwrap();
        let create_denied =
            probe_vfs.probe_directory_create_capability_at(Path::new("/scoped-probes"));
        std::fs::set_permissions(&selected, selected_permissions).unwrap();
        assert!(create_denied.is_err());
        assert!(!probe_vfs.capabilities().exclusive_directory_create);
        let original = std::fs::metadata(&sandbox).unwrap().permissions();
        std::fs::set_permissions(&sandbox, std::fs::Permissions::from_mode(0o111)).unwrap();
        let denied = probe_vfs.probe_directory_create_capability_at(Path::new("/scoped-probes"));
        std::fs::set_permissions(&sandbox, original).unwrap();
        assert!(
            denied.is_err(),
            "Profilroot-Listing darf nicht übergangen werden"
        );
        assert!(!probe_vfs.capabilities().exclusive_directory_create);
        assert!(std::fs::read_dir(&selected).unwrap().next().is_none());
        for root in [&selected, &sandbox] {
            assert!(!std::fs::read_dir(root).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".doppeldecker-")));
        }
        probe_vfs
            .probe_directory_create_capability_at(Path::new("/scoped-probes"))
            .unwrap();
        probe_vfs
            .probe_rename_capability_at(Path::new("/scoped-probes"))
            .unwrap();
        probe_vfs
            .probe_delete_capability_at(Path::new("/scoped-probes"))
            .unwrap();
        assert!(probe_vfs
            .probe_add_only_capabilities_at(Path::new("/scoped-probes"))
            .unwrap()
            .permits_add_only());
        let endpoint =
            ProviderEndpoint::smb(probe_vfs.clone(), Path::new("/scoped-probes")).unwrap();
        assert!(endpoint.probe_file_capabilities().unwrap().copy_new);
        let permissions = std::fs::metadata(&selected).unwrap().permissions();
        std::fs::set_permissions(&selected, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = endpoint.probe_file_capabilities();
        std::fs::set_permissions(&selected, permissions).unwrap();
        let denied = denied.unwrap();
        assert!(!denied.copy_new && !denied.copy_new_best_effort);
        assert!(!denied.remove_source && !denied.remove_source_best_effort);
        assert!(!denied.rename_noreplace && !denied.blockers.is_empty());
        assert!(probe_vfs.capabilities().permits_add_only());
        assert!(std::fs::read_dir(&selected).unwrap().next().is_none());
        eprintln!("SMB-E2E: neue Wirkung bleibt trotz alter Sitzungscapabilities gesperrt");
        for root in [&selected, &sandbox] {
            assert!(!std::fs::read_dir(root).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".doppeldecker-")));
        }
        std::fs::remove_dir(&selected).unwrap();
    }

    let vfs = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets)).unwrap());
    eprintln!("SMB-E2E: verschlüsselte Sitzung verbunden");
    {
        let mut reader = vfs
            .open_read(Path::new("/download-source/remote.txt"))
            .unwrap();
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        let mut first = [0; 1];
        assert_eq!(reader.read(&mut first).unwrap(), 1);
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        let mut bytes = first.to_vec();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"SMB remote eins\n");
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        assert_eq!(reader.read(&mut first).unwrap(), 0);
    }
    eprintln!("SMB-E2E: leere Reads erhalten den offenen Stream");
    assert_eq!(vfs.identity().server_guid, discovery.server_guid);
    assert!(vfs.identity().signing_active);
    assert!(vfs.identity().encryption_active);
    let paging = vfs.list_complete(Path::new("/paging")).unwrap();
    assert!(paging.complete);
    assert_eq!(paging.entries.len(), 1_500);
    eprintln!("SMB-E2E: gepagtes Listing vollständig");
    assert!(paging.provider_token.is_some());
    assert!(!vfs.capabilities().permits_add_only());

    let delete_capabilities = vfs.probe_delete_capability().unwrap();
    assert!(delete_capabilities.permits_entry_delete());
    let mkdir_capabilities = vfs.capabilities();
    assert!(mkdir_capabilities.permits_directory_create());
    assert!(!mkdir_capabilities.permits_add_only());
    vfs.create_dir(Path::new("/mkdir-e2e")).unwrap();
    assert!(vfs.create_dir(Path::new("/mkdir-e2e")).is_err());

    assert!(
        !vfs.capabilities().permits_add_only(),
        "Quarantäne darf den Uploadvertrag nicht nebenbei freigeben"
    );
    assert!(vfs
        .open_write_new(Path::new("/upload-ohne-add-only"))
        .is_err());
    vfs.remove_dir(Path::new("/mkdir-e2e")).unwrap();
    assert!(matches!(
        vfs.stat(Path::new("/mkdir-e2e")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    ));
    vfs.create_dir(Path::new("/mkdir-e2e")).unwrap();

    let capabilities = vfs.probe_add_only_capabilities().unwrap();
    eprintln!("SMB-E2E: Schreibfähigkeiten geprüft");
    assert!(capabilities.permits_add_only());
    let file_capabilities = ProviderEndpoint::smb(vfs.clone(), Path::new("/"))
        .unwrap()
        .probe_file_capabilities()
        .unwrap();
    assert!(!file_capabilities.replace);
    assert!(!file_capabilities.replace_best_effort);
    assert!(file_capabilities
        .blockers
        .iter()
        .any(|blocker| blocker.contains("identitätsgebundenes bedingtes Rename")));
    assert!(!std::fs::read_dir(&sandbox).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".doppeldecker-probe-")));

    // F5/F6 übertragen ausschließlich die ausgewählten Einträge und entfernen die
    // Move-Quelle erst nach vollständig verifiziertem lokalen Commit.
    {
        let mut copy = vfs.open_write_new(Path::new("/selected-copy.txt")).unwrap();
        copy.write_all(b"copy\n").unwrap();
    }
    {
        let mut mov = vfs.open_write_new(Path::new("/selected-move.txt")).unwrap();
        mov.write_all(b"move\n").unwrap();
    }
    let selection_area = tempfile::tempdir().unwrap();
    let selection_target = selection_area.path().join("ziel");
    std::fs::create_dir(&selection_target).unwrap();
    let selection_queue = OperationQueue::open(selection_area.path().join("queue.json")).unwrap();
    for (name, kind) in [
        ("selected-copy.txt", FileQueueKind::Copy),
        ("selected-move.txt", FileQueueKind::Move),
    ] {
        let queued = selection_queue
            .enqueue(QueueOperation::ProviderFile {
                kind,
                source: Box::new(BidirectionalQueueEndpoint::Smb {
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
        let result = execute_provider_file_queue_entry(
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
        )
        .unwrap();
        assert!(matches!(result, QueueExecutionResult::File(_)));
        assert_eq!(
            selection_queue
                .snapshot()
                .entries
                .iter()
                .find(|entry| entry.operation_id == queued.operation_id)
                .unwrap()
                .state,
            QueueState::Completed
        );
        assert!(selection_target.join(name).is_file());
    }
    assert!(vfs.stat(Path::new("/selected-copy.txt")).is_ok());
    assert!(matches!(
        vfs.stat(Path::new("/selected-move.txt")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    ));
    vfs.remove_file(Path::new("/selected-copy.txt")).unwrap();
    vfs.remove_dir_all(Path::new("/mkdir-e2e")).unwrap();

    vfs.create_dir(Path::new("/stream")).unwrap();
    let mut writer = vfs.open_write_new(Path::new("/stream/temp.txt")).unwrap();
    writer.write_all(b"SMB verschluesselter Stream\n").unwrap();
    writer.flush().unwrap();
    vfs.rename_noreplace(
        Path::new("/stream/temp.txt"),
        Path::new("/stream/fertig.txt"),
    )
    .unwrap();
    assert_eq!(
        read_all(vfs.as_ref(), Path::new("/stream/fertig.txt")),
        b"SMB verschluesselter Stream\n"
    );

    let mut collision = vfs
        .open_write_new(Path::new("/stream/kollision.txt"))
        .unwrap();
    collision.write_all(b"darf nicht ersetzen\n").unwrap();
    collision.flush().unwrap();
    assert!(vfs
        .rename_noreplace(
            Path::new("/stream/kollision.txt"),
            Path::new("/stream/fertig.txt")
        )
        .is_err());
    assert_eq!(
        read_all(vfs.as_ref(), Path::new("/stream/fertig.txt")),
        b"SMB verschluesselter Stream\n"
    );
    vfs.remove_file(Path::new("/stream/kollision.txt")).unwrap();
    vfs.set_mtime(Path::new("/stream/fertig.txt"), -1).unwrap();
    assert_eq!(
        vfs.stat(Path::new("/stream/fertig.txt")).unwrap().mtime,
        Some(-1)
    );
    eprintln!("SMB-E2E: Stream und späte Kollision geprüft");

    let generation = vfs.session_generation();
    vfs.reconnect().unwrap();
    assert_eq!(vfs.session_generation(), generation + 1);
    assert!(!vfs.capabilities().permits_add_only());
    assert!(!vfs.capabilities().permits_entry_delete());
    assert!(!vfs.capabilities().permits_directory_create());
    assert!(vfs.create_dir(Path::new("/mkdir-ohne-reprobe")).is_err());
    assert!(vfs
        .open_write_new(Path::new("/nach-reconnect-gesperrt"))
        .is_err());
    vfs.probe_directory_create_capability().unwrap();
    vfs.probe_delete_capability().unwrap();
    vfs.probe_add_only_capabilities().unwrap();
    eprintln!("SMB-E2E: Reconnect setzt Fähigkeiten zurück");

    let upload_identity = vfs.root_identity(Path::new("/upload-target")).unwrap();
    let upload_proof =
        build_listing_proof(vfs.as_ref(), Path::new("/upload-target"), upload_identity);
    assert!(upload_proof.complete, "{:?}", upload_proof.errors);
    eprintln!("SMB-E2E: leerer Ziel-Proof vollständig");

    let upload = tempfile::tempdir().unwrap();
    std::fs::write(upload.path().join("upload.txt"), b"SMB Upload eins\n").unwrap();
    let add_only = prepare_smb_upload(
        upload.path(),
        vfs.clone(),
        Path::new("/upload-target"),
        SyncOptions::default(),
    )
    .unwrap();
    eprintln!("SMB-E2E: AddOnly-Upload vorbereitet");
    execute_prepared(add_only, false);
    eprintln!("SMB-E2E: AddOnly-Upload abgeschlossen");
    assert_eq!(
        read_all(vfs.as_ref(), Path::new("/upload-target/upload.txt")),
        b"SMB Upload eins\n"
    );

    std::fs::write(upload.path().join("upload.txt"), b"SMB Upload zwei\n").unwrap();
    let listing_fingerprint = |path: &Path| {
        vfs.list_complete(path)
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| {
                (
                    entry.raw_name,
                    entry.entry.is_dir,
                    entry.entry.is_file,
                    entry.entry.size,
                    entry.provider_token,
                )
            })
            .collect::<Vec<_>>()
    };
    let target_before = listing_fingerprint(Path::new("/upload-target"));
    let root_before = listing_fingerprint(Path::new("/"));
    let update_error = prepare_smb_upload(
        upload.path(),
        vfs.clone(),
        Path::new("/upload-target"),
        SyncOptions {
            mode: SyncMode::Update,
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        },
    )
    .err()
    .expect("SMB ohne vollständige Proof-Primitiven darf Update nicht vorbereiten");
    assert_eq!(error_code(&update_error), ErrorCode::Unsupported);
    assert_eq!(
        read_all(vfs.as_ref(), Path::new("/upload-target/upload.txt")),
        b"SMB Upload eins\n"
    );
    assert_eq!(
        listing_fingerprint(Path::new("/upload-target")),
        target_before
    );
    assert_eq!(
        listing_fingerprint(Path::new("/")),
        root_before,
        "die abgelehnte Restore-Probe darf weder Probe- noch Quarantänereste hinterlassen"
    );
    eprintln!("SMB-E2E: Update ohne Proof-Primitiven mutationsfrei gesperrt");

    // Mirror braucht neben Dauerhaftigkeit den vollständigen Restore-Proofvertrag.
    // Der zusätzliche Zielpfad erzwingt eine Entfernung im Vorschauplan und prüft
    // damit auch diese stärkere Fähigkeitsschranke.
    std::fs::write(
        sandbox.join("upload-target/nur-im-ziel.txt"),
        b"muss fuer Mirror entfernt werden\n",
    )
    .unwrap();
    let mirror_options = SyncOptions {
        mode: SyncMode::Mirror,
        compare: Compare::ContentHash,
        ..SyncOptions::default()
    };
    let mirror_preview = prepare_smb_upload(
        upload.path(),
        vfs.clone(),
        Path::new("/upload-target"),
        mirror_options,
    );
    let mirror_error = mirror_preview
        .err()
        .expect("SMB-Mirror braucht ebenfalls den vollständigen Restore-Proofvertrag");
    assert_eq!(error_code(&mirror_error), ErrorCode::Unsupported);
    eprintln!("SMB-E2E: Mirror-Fähigkeitsgate geprüft");

    let local_area = tempfile::tempdir().unwrap();
    let local_target = local_area.path().join("download-target");
    std::fs::create_dir(&local_target).unwrap();
    execute_prepared(
        prepare_smb_download(
            vfs.clone(),
            Path::new("/download-source"),
            &local_target,
            SyncOptions::default(),
        )
        .unwrap(),
        false,
    );
    eprintln!("SMB-E2E: Download abgeschlossen");
    assert_eq!(
        std::fs::read(local_target.join("remote.txt")).unwrap(),
        b"SMB remote eins\n"
    );

    // SMB kann gewöhnliche AddOnly-Kopien ausführen, besitzt aber noch keinen
    // identitätsgebundenen Quarantänevertrag für den Rollback eines bidirektionalen
    // Laufs. Zielwirkungen auf SMB bleiben deshalb gesperrt; die lokale Richtung
    // muss weiterhin Additionen, Ersetzungen und Entfernungen ausführen können.
    std::fs::create_dir_all(sandbox.join("bidirektional")).unwrap();
    let bidi_local = local_area.path().join("bidirektional-lokal");
    std::fs::create_dir(&bidi_local).unwrap();
    let bidi_baseline = local_area.path().join("state/smb-pair.json");
    let pair = PairId([112; 16]);
    let prepare = || {
        prepare_provider_bidirectional_with_decisions(
            pair,
            "SMB-E2E-Paar",
            ProviderEndpoint::local(&bidi_local).unwrap(),
            ProviderEndpoint::smb(vfs.clone(), Path::new("/bidirektional")).unwrap(),
            &bidi_baseline,
            &[],
        )
        .unwrap()
    };
    let reject_remote_effect = |run: doppeldecker_core::PreparedProviderBidirectionalRun| {
        assert!(run.preview().stage_blocker.is_some());
        let phrase = run.preview().confirmation_phrase.clone();
        let error = run.confirm(&phrase, true).err().unwrap();
        assert_eq!(error_code(&error), ErrorCode::Unsupported);
    };
    execute_bidirectional(prepare(), &bidi_baseline);

    std::fs::write(bidi_local.join("links.txt"), b"links eins\n").unwrap();
    std::fs::write(sandbox.join("bidirektional/rechts.txt"), b"rechts eins\n").unwrap();
    reject_remote_effect(prepare());
    assert!(!sandbox.join("bidirektional/links.txt").exists());
    assert!(!bidi_local.join("rechts.txt").exists());
    assert_eq!(
        std::fs::read(bidi_local.join("links.txt")).unwrap(),
        b"links eins\n"
    );
    std::fs::remove_file(bidi_local.join("links.txt")).unwrap();
    execute_bidirectional(prepare(), &bidi_baseline);
    assert_eq!(
        std::fs::read(bidi_local.join("rechts.txt")).unwrap(),
        b"rechts eins\n"
    );

    std::fs::write(bidi_local.join("rechts.txt"), b"lokaler Ersatz\n").unwrap();
    reject_remote_effect(prepare());
    assert_eq!(
        read_all(vfs.as_ref(), Path::new("/bidirektional/rechts.txt")),
        b"rechts eins\n"
    );
    std::fs::write(bidi_local.join("rechts.txt"), b"rechts eins\n").unwrap();
    std::fs::write(sandbox.join("bidirektional/rechts.txt"), b"rechts zwei\n").unwrap();
    execute_bidirectional(prepare(), &bidi_baseline);
    assert_eq!(
        std::fs::read(bidi_local.join("rechts.txt")).unwrap(),
        b"rechts zwei\n"
    );

    for index in 0..24 {
        std::fs::write(
            sandbox.join(format!("bidirektional/bestand-{index:02}.txt")),
            b"gleich\n",
        )
        .unwrap();
    }
    execute_bidirectional(prepare(), &bidi_baseline);
    std::fs::remove_file(bidi_local.join("bestand-00.txt")).unwrap();
    reject_remote_effect(prepare());
    assert_eq!(
        std::fs::read(sandbox.join("bidirektional/bestand-00.txt")).unwrap(),
        b"gleich\n"
    );
    std::fs::write(bidi_local.join("bestand-00.txt"), b"gleich\n").unwrap();
    std::fs::remove_file(sandbox.join("bidirektional/bestand-01.txt")).unwrap();
    execute_bidirectional(prepare(), &bidi_baseline);
    assert!(!bidi_local.join("bestand-01.txt").exists());
    assert!(sandbox.join("bidirektional/bestand-00.txt").exists());
    assert_eq!(
        read_all(vfs.as_ref(), Path::new("/bidirektional/rechts.txt")),
        b"rechts zwei\n"
    );
    eprintln!("SMB-E2E: bidirektionale Capability-Matrix abgeschlossen");

    let queue_target = local_area.path().join("queue-target");
    std::fs::create_dir(&queue_target).unwrap();
    let preview = prepare_smb_download(
        vfs.clone(),
        Path::new("/download-source"),
        &queue_target,
        SyncOptions::default(),
    )
    .unwrap();
    let expected_plan = preview.preview().plan.clone();
    drop(preview);
    let queue_area = tempfile::tempdir().unwrap();
    let queue = OperationQueue::open(queue_area.path().join("queue.json")).unwrap();
    let queued = queue
        .enqueue(QueueOperation::SmbSync {
            direction: SftpQueueDirection::Download,
            connection: Box::new(config),
            local_root: queue_target.to_string_lossy().into_owned(),
            remote_root: "/download-source".into(),
            options: SyncOptions::default(),
            expected_plan: Box::new(expected_plan),
        })
        .unwrap();
    let claimed = queue.claim_next().unwrap().unwrap();
    let prepared = match prepare_smb_queue_entry(&queue, claimed, Arc::new(TestSecrets)).unwrap() {
        SftpQueuePreparation::Ready(prepared) => *prepared,
        SftpQueuePreparation::AwaitingConfirmation { reason } => {
            panic!("SMB-Queue war unerwartet nicht bereit: {reason}")
        }
    };
    let phrase = prepared.preview().confirmation_phrase.clone();
    execute_prepared_sftp_queue_entry(
        &queue,
        prepared,
        &phrase,
        false,
        &queue_area.path().join("audit.jsonl"),
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    eprintln!("SMB-E2E: persistente Queue abgeschlossen");
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
        std::fs::read(queue_target.join("remote.txt")).unwrap(),
        b"SMB remote eins\n"
    );
}

fn execute_bidirectional(
    run: doppeldecker_core::PreparedProviderBidirectionalRun,
    baseline_path: &Path,
) {
    assert!(
        run.preview().stage_blocker.is_none(),
        "Bidirektionale Vorbereitung gesperrt: {:?}",
        run.preview().stage_blocker
    );
    let phrase = run.preview().confirmation_phrase.clone();
    run.confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    finish_bidirectional_run_completion(baseline_path).unwrap();
}

fn execute_prepared(run: doppeldecker_core::PreparedSftpRealRun, backup: bool) {
    let phrase = run.preview().confirmation_phrase.clone();
    let mut last_phase = String::new();
    run.confirm(&phrase, backup)
        .unwrap()
        .execute(&Cancel::new(), &mut |progress| {
            if progress.phase != last_phase {
                eprintln!("SMB-E2E: Phase {}", progress.phase);
                last_phase = progress.phase.clone();
            }
        })
        .unwrap();
}

fn read_all(vfs: &dyn Vfs, path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    vfs.open_read(path)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

fn required(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} fehlt"))
}
