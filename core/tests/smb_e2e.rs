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
use doppeldecker_core::bidirectional::prepare_provider_bidirectional_with_decisions_bound_for_queue;
use doppeldecker_core::real_mode::build_listing_proof;
use doppeldecker_core::{
    discover_smb_server, error_code, execute_prepared_sftp_queue_entry,
    execute_provider_file_queue_entry, prepare_provider_transfer_for_queue,
    prepare_smb_queue_entry, BidirectionalQueueEndpoint, Cancel, Compare, ErrorCode, FileQueueKind,
    KeychainSecretReference, LocalQueueExecution, OperationQueue, PairId, ProviderEndpoint,
    QueueExecutionResult, QueueOperation, QueueState, SecretResolver, SecretString,
    SftpQueueDirection, SftpQueuePreparation, SmbConnectionConfig, SmbVfs, SyncMode, SyncOptions,
    Vfs,
};

const TEST_PASSWORD: &str = "Doppeldecker-SMB-E2E-wertlos-2026";

struct TestSecrets;
#[path = "support/smb_native_copy_fixture.rs"]
mod native_copy_fixture;

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

    smb_batch_minimal_probe(&config, server_root);

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

    // Gewöhnliche Erstwirkungen auf frischer Sitzung ohne Proben: exklusive
    // Anlage, dauerhafte Ersetzung und vollständig bilanzierter Baumabbau.
    {
        let remote = sandbox.join("ordinary-actions");
        std::fs::create_dir(&remote).unwrap();
        std::fs::create_dir_all(remote.join("Baum/Tief")).unwrap();
        std::fs::write(remote.join("Baum/Tief/Alt"), b"Alt").unwrap();
        std::fs::write(remote.join("Datei"), b"Altversion").unwrap();
        std::fs::write(remote.join("Sentinel"), b"Erhalten").unwrap();
        let provider = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets)).unwrap());
        let endpoint =
            ProviderEndpoint::smb(provider.clone(), Path::new("/ordinary-actions")).unwrap();
        let caps = endpoint.probe_file_capabilities().unwrap();
        assert!(caps.copy_new && caps.replace_best_effort);
        assert!(!provider.capabilities().permits_add_only());
        let (view, root) = endpoint.ordinary_file_context().unwrap();
        doppeldecker_core::create_dir(view.as_ref(), &root, "Neu", &None).unwrap();
        assert_eq!(
            doppeldecker_core::create_dir(view.as_ref(), &root, "Neu", &None)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
        let renamed = endpoint
            .execute_interactive_rename(&root.join("Neu"), "Neu2", &None)
            .unwrap();
        assert_eq!(renamed.outcome, doppeldecker_core::RenameOutcome::Renamed);
        let collision = endpoint
            .execute_interactive_rename(&root.join("Neu2"), "Datei", &None)
            .unwrap();
        assert_eq!(
            collision.outcome,
            doppeldecker_core::RenameOutcome::Collision
        );
        assert!(remote.join("Neu2").is_dir());
        let source = tempfile::tempdir().unwrap();
        let source_root = source.path().canonicalize().unwrap();
        std::fs::write(source_root.join("Datei"), b"Neuer Inhalt").unwrap();
        let local = doppeldecker_core::LocalVfs::rooted(&source_root).unwrap();
        let copy = doppeldecker_core::fileops::copy_items(
            doppeldecker_core::FileOperationRequest {
                src: &local,
                dst: view.as_ref(),
                items: &[source_root.join("Datei")],
                dst_dir: &root,
                policy: doppeldecker_core::OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(copy.files, 1);
        assert_eq!(
            std::fs::read(remote.join("Datei")).unwrap(),
            b"Neuer Inhalt"
        );
        let plan =
            doppeldecker_core::plan_remote_delete(view.as_ref(), &root, &[root.join("Baum")])
                .unwrap();
        let deleted = doppeldecker_core::execute_remote_delete(
            view.as_ref(),
            &plan,
            doppeldecker_core::RemoteDeleteMode::Permanent,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(deleted.entries, 3);
        assert!(!remote.join("Baum").exists());
        assert_eq!(std::fs::read(remote.join("Sentinel")).unwrap(), b"Erhalten");
        assert_eq!(std::fs::read_dir(&remote).unwrap().count(), 3);
        ordinary_cancel_cleanup(&config, &remote);
        eprintln!("SMB-E2E: normale Erstwirkungen ohne Schreibproben bestätigt");
    }

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
        let advertised = endpoint.probe_file_capabilities().unwrap();
        assert!(advertised.copy_new && advertised.replace_best_effort);
        // Die gewöhnliche Anzeige ist keine künstliche Schreibberechtigungsprobe.
        // Eine echte, verweigerte Wirkung muss ohne weitere Seiteneffekte scheitern.
        let denied = endpoint.create_interactive_directory(
            "NichtErlaubt",
            doppeldecker_core::DirectoryCreateExecutionClass::Assured,
        );
        std::fs::set_permissions(&selected, permissions).unwrap();
        assert!(denied.is_err());
        assert!(probe_vfs.capabilities().permits_add_only());
        assert!(std::fs::read_dir(&selected).unwrap().next().is_none());
        eprintln!("SMB-E2E: normale Wirkung prüft Serverrechte ohne künstliche Vorprobe");
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
    assert!(file_capabilities.replace_best_effort);
    assert!(file_capabilities.blockers.is_empty());
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

    let transfer_area = tempfile::tempdir().unwrap();
    let transfer_queue =
        OperationQueue::open(transfer_area.path().join("config/queue.json")).unwrap();
    let upload = tempfile::tempdir().unwrap();
    std::fs::write(upload.path().join("upload.txt"), b"SMB Upload eins\n").unwrap();
    let add_only = prepare_provider_transfer_for_queue(
        &transfer_queue,
        ProviderEndpoint::local(upload.path()).unwrap(),
        ProviderEndpoint::smb(vfs.clone(), Path::new("/upload-target")).unwrap(),
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
    let update_error = prepare_provider_transfer_for_queue(
        &transfer_queue,
        ProviderEndpoint::local(upload.path()).unwrap(),
        ProviderEndpoint::smb(vfs.clone(), Path::new("/upload-target")).unwrap(),
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
    let mirror_preview = prepare_provider_transfer_for_queue(
        &transfer_queue,
        ProviderEndpoint::local(upload.path()).unwrap(),
        ProviderEndpoint::smb(vfs.clone(), Path::new("/upload-target")).unwrap(),
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
        prepare_provider_transfer_for_queue(
            &transfer_queue,
            ProviderEndpoint::smb(vfs.clone(), Path::new("/download-source")).unwrap(),
            ProviderEndpoint::local(&local_target).unwrap(),
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
    let bidi_queue = OperationQueue::open(local_area.path().join("config/queue.json")).unwrap();
    let bidi_baseline = local_area.path().join("state/smb-pair.json");
    let pair = PairId([112; 16]);
    let prepare = || {
        prepare_provider_bidirectional_with_decisions_bound_for_queue(
            &bidi_queue,
            pair,
            "SMB-E2E-Paar",
            ProviderEndpoint::local(&bidi_local).unwrap(),
            ProviderEndpoint::smb(vfs.clone(), Path::new("/bidirektional")).unwrap(),
            &bidi_baseline,
            &[],
            None,
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
    let queue_area = tempfile::tempdir().unwrap();
    let queue = OperationQueue::open(queue_area.path().join("config/queue.json")).unwrap();
    let preview = prepare_provider_transfer_for_queue(
        &queue,
        ProviderEndpoint::smb(vfs.clone(), Path::new("/download-source")).unwrap(),
        ProviderEndpoint::local(&queue_target).unwrap(),
        SyncOptions::default(),
    )
    .unwrap();
    let expected_plan = preview.preview().plan.clone();
    drop(preview);
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

fn ordinary_cancel_cleanup(config: &SmbConnectionConfig, remote: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for replace_temp in [false, true] {
        let source = tempfile::tempdir().unwrap();
        let source_root = source.path().canonicalize().unwrap();
        let source_file = source_root.join("Cancelled.txt");
        std::fs::write(&source_file, vec![b'C'; 1024 * 1024]).unwrap();
        let journal = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let queue = OperationQueue::open(journal.path().join("queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::ProviderFile {
                kind: FileQueueKind::Copy,
                source: Box::new(BidirectionalQueueEndpoint::Local {
                    root: source_root.to_string_lossy().into_owned(),
                }),
                target: Box::new(BidirectionalQueueEndpoint::Smb {
                    connection: Box::new(config.clone()),
                    root: "/ordinary-actions".into(),
                }),
                items: vec![source_file.to_string_lossy().into_owned()],
                destination: "/ordinary-actions".into(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                source_tree_expected: None,
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                best_effort_commit_expected: None,
                local_copy_new_best_effort_expected: None,
                copy_new_best_effort_expected: Some(false),
                replace_best_effort_expected: Some(false),
                remove_source_best_effort_expected: None,
            })
            .unwrap();
        let cancel = Cancel::new();
        let mut observed_temp = None;
        let result = execute_provider_file_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &LocalQueueExecution {
                sandbox: None,
                sync_quarantine: None,
                max_delete_entries: 100,
                max_delete_fraction: 0.25,
            },
            Arc::new(TestSecrets),
            &cancel,
            &mut |progress| {
                if progress.bytes_done == 0 || observed_temp.is_some() {
                    return;
                }
                let temp = std::fs::read_dir(remote)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .contains("fileop-tmp")
                    })
                    .expect("echter Temp muss vor dem Abbruch vorhanden sein");
                if replace_temp {
                    std::fs::rename(&temp, remote.join("displaced-owned-temp")).unwrap();
                    std::fs::write(&temp, b"Fremder Bestand").unwrap();
                }
                observed_temp = Some(temp);
                cancel.cancel();
            },
        );
        let temp = observed_temp.expect("Abbruch nach echter Tempanlage wurde nicht erreicht");
        let recovery_paths = if replace_temp {
            let error = result.unwrap_err();
            assert_eq!(
                doppeldecker_core::error_code(&error),
                ErrorCode::OutcomeUnclear
            );
            let entry = queue.snapshot().entries.remove(0);
            assert_eq!(entry.state, QueueState::Failed);
            assert_eq!(entry.error_code, Some(ErrorCode::OutcomeUnclear));
            assert!(queue.retry(&entry.operation_id).is_err());
            entry.temp_recovery_paths
        } else {
            let stats = match result.unwrap() {
                QueueExecutionResult::File(stats) => stats,
                other => panic!("unerwarteter Abschluss: {other:?}"),
            };
            assert!(stats.cancelled);
            assert_eq!(queue.snapshot().entries[0].state, QueueState::Cancelled);
            stats.temp_recovery_paths
        };
        assert!(!remote.join("Cancelled.txt").exists());
        assert_eq!(
            std::fs::read(&source_file).unwrap(),
            vec![b'C'; 1024 * 1024]
        );
        let intents = std::fs::read_dir(journal.path())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".recovery.json")
            })
            .count();
        if replace_temp {
            assert_eq!(std::fs::read(&temp).unwrap(), b"Fremder Bestand");
            assert!(remote.join("displaced-owned-temp").exists());
            assert!(!recovery_paths.is_empty());
            assert_eq!(intents, 1);
            std::fs::remove_file(&temp).unwrap();
            std::fs::remove_file(remote.join("displaced-owned-temp")).unwrap();
        } else {
            assert!(
                !temp.exists(),
                "bewiesener eigener Temp muss trotz Nutzerabbruch entfernt werden"
            );
            assert!(recovery_paths.is_empty());
            assert_eq!(
                intents, 0,
                "abgeschlossener Cleanup muss den eigenen Intent schließen"
            );
        }
        assert_eq!(std::fs::read(remote.join("Sentinel")).unwrap(), b"Erhalten");
    }
}

fn required(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} fehlt"))
}

fn smb_batch_minimal_probe(config: &SmbConnectionConfig, server_root: &Path) {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::smb_batch_rename::PreparedSmbBatchRename;
    let options = BatchRenameOptions {
        find: String::new(),
        replacement: String::new(),
        prefix: "new-".into(),
        suffix: String::new(),
        preserve_extension: true,
        trim_outer_spaces: false,
        numbering: None,
    };
    for change in 0..7 {
        let name = format!("batch-selection-{change}");
        let host = server_root.join("sandbox").join(&name);
        std::fs::create_dir(&host).unwrap();
        for file in ["a", "b", "c"] {
            std::fs::write(host.join(file), file.as_bytes()).unwrap();
        }
        let provider = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets)).unwrap());
        let pane = format!("/{name}");
        let endpoint = ProviderEndpoint::smb(provider.clone(), Path::new(&pane)).unwrap();
        let before = provider.stat(&Path::new(&pane).join("a")).unwrap().inode;
        assert!(before.is_some());
        let plan = PreparedSmbBatchRename::prepare(
            &endpoint,
            &["a".into(), "b".into(), "c".into()],
            &options,
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(
            plan.execution_class(),
            doppeldecker_core::RenameExecutionClass::Assured
        );
        let cancel = Cancel::new();
        if change == 4 {
            cancel.cancel();
        }
        let old = server_root.join("sandbox").join(format!("{name}-old"));
        let original_b = std::fs::metadata(host.join("b")).unwrap();
        let report = plan.execute(&cancel, &mut || Ok(()), &mut |done, _| {
            if done == 1 {
                match change {
                    0 => std::fs::write(host.join("new-b"), b"bestehend").unwrap(),
                    1 => provider.reconnect().unwrap(),
                    2 => {
                        std::fs::rename(&host, &old).unwrap();
                        std::fs::create_dir(&host).unwrap();
                        std::fs::write(host.join("b"), b"fremd").unwrap();
                    }
                    3 => cancel.cancel(),
                    5 => {
                        std::fs::rename(host.join("b"), host.join("kept-b")).unwrap();
                        std::fs::write(host.join("b"), b"b").unwrap();
                        filetime::set_file_mtime(
                            host.join("b"),
                            filetime::FileTime::from_last_modification_time(&original_b),
                        )
                        .unwrap();
                    }
                    6 => {
                        let before_token = provider.list_complete(Path::new(&pane)).unwrap().entries.into_iter().find(|entry| entry.entry.name == "b").unwrap().provider_token;
                        std::thread::sleep(std::time::Duration::from_millis(1100));
                        std::fs::write(host.join("b"), b"z").unwrap();
                        filetime::set_file_mtime(
                            host.join("b"),
                            filetime::FileTime::from_last_modification_time(&original_b),
                        )
                        .unwrap();
                        let after_token = provider.list_complete(Path::new(&pane)).unwrap().entries.into_iter().find(|entry| entry.entry.name == "b").unwrap().provider_token;
                        assert_eq!(before_token, after_token, "Samba hat in dieser Hostwrite-Probe einen neuen ChangeTime-Beleg geliefert; Erwartung neu bewerten");
                    }
                    _ => {}
                }
            }
        });
        match change {
            0 => {
                assert_eq!(
                    (
                        report.renamed,
                        report.failed_unchanged,
                        report.outcome_unclear
                    ),
                    (2, 1, 0),
                    "{:?}",
                    report.rows
                );
                assert_eq!(report.rows[1].code, Some(ErrorCode::AlreadyExists));
                assert_eq!(std::fs::read(host.join("new-b")).unwrap(), b"bestehend");
                assert_eq!(std::fs::read(host.join("b")).unwrap(), b"b");
                assert_eq!(
                    provider
                        .stat(&Path::new(&pane).join("new-a"))
                        .unwrap()
                        .inode,
                    before
                );
                assert_eq!(std::fs::read(host.join("new-c")).unwrap(), b"c");
            }
            1 | 2 => {
                assert_eq!(
                    (
                        report.renamed,
                        report.failed_unchanged,
                        report.outcome_unclear
                    ),
                    (1, 2, 0),
                    "change={change}: {:?}",
                    report.rows
                );
                assert_eq!(report.rows[1].code, Some(ErrorCode::PlanDrift));
                assert!(!host.join("new-b").exists());
                assert!(!host.join("new-c").exists());
                assert!(if change == 2 {
                    old.join("new-a").exists()
                } else {
                    host.join("new-a").exists()
                });
            }
            5 => {
                assert_eq!(
                    (report.renamed, report.failed_unchanged),
                    (2, 1),
                    "{:?}",
                    report.rows
                );
                assert_eq!(report.rows[1].code, Some(ErrorCode::PlanDrift));
                assert_eq!(std::fs::read(host.join("b")).unwrap(), b"b");
                assert_eq!(std::fs::read(host.join("new-c")).unwrap(), b"c");
            }
            6 => {
                // Assured schützt das Ziel atomar; gleiche beobachtbare Metadaten
                // desselben Objekts versprechen keine Inhaltsunveränderlichkeit.
                assert_eq!(
                    (
                        report.renamed,
                        report.failed_unchanged,
                        report.outcome_unclear
                    ),
                    (3, 0, 0)
                );
                assert_eq!(std::fs::read(host.join("new-b")).unwrap(), b"z");
                assert_eq!(std::fs::read(host.join("new-c")).unwrap(), b"c");
            }
            3 => {
                assert_eq!((report.renamed, report.cancelled), (1, 2));
                assert_eq!(report.rows[1].status, BatchRenameStatus::Cancelled);
            }
            _ => {
                assert_eq!((report.renamed, report.cancelled), (0, 3));
                assert!(host.join("a").exists());
            }
        }
        eprintln!("SMB-Stapelprobe {change}: eindeutige Bilanz und keine Restmutation");
    }
    smb_batch_chain_probe(config, server_root);
    smb_batch_spelling_probe(config, server_root);
    smb_cycle_probe(config, server_root);
}

fn smb_batch_chain_probe(config: &SmbConnectionConfig, server_root: &Path) {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::smb_batch_rename::PreparedSmbBatchRename;
    let options = BatchRenameOptions {
        find: String::new(),
        replacement: String::new(),
        prefix: "a".into(),
        suffix: String::new(),
        preserve_extension: true,
        trim_outer_spaces: false,
        numbering: None,
    };
    for collision in [false, true] {
        let name = format!("batch-chain-{collision}");
        let host = server_root.join("sandbox").join(&name);
        std::fs::create_dir(&host).unwrap();
        for file in ["a", "aa", "aaa", "b"] {
            std::fs::write(host.join(file), file).unwrap();
        }
        let provider = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets)).unwrap());
        let pane = format!("/{name}");
        let endpoint = ProviderEndpoint::smb(provider.clone(), Path::new(&pane)).unwrap();
        let selected = ["a", "aa", "aaa", "b"].map(str::to_string);
        let identities = selected
            .iter()
            .map(|file| provider.stat(&Path::new(&pane).join(file)).unwrap().inode)
            .collect::<Vec<_>>();
        let plan = PreparedSmbBatchRename::prepare(&endpoint, &selected, &options, &Cancel::new())
            .unwrap();
        assert!(plan
            .rows()
            .iter()
            .all(|row| row.status == BatchRenameStatus::Ready));
        assert_eq!(
            plan.rows()
                .iter()
                .map(|row| row.old_name.as_str())
                .collect::<Vec<_>>(),
            ["a", "aa", "aaa", "b"]
        );
        if collision {
            std::fs::write(host.join("aaaa"), b"bestehend").unwrap();
        }
        let mut observed = Vec::new();
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |done, total| {
            observed.push((done, total));
            if !collision && done == 1 {
                assert!(!host.join("aaa").exists());
                assert_eq!(std::fs::read(host.join("aaaa")).unwrap(), b"aaa");
                assert!(host.join("a").exists());
                assert!(host.join("aa").exists());
            }
        });
        assert_eq!(observed, [(1, 4), (2, 4), (3, 4), (4, 4)]);
        assert_eq!(report.outcome_unclear, 0, "{:?}", report.rows);
        assert_eq!(std::fs::read(host.join("ab")).unwrap(), b"b");
        if collision {
            assert_eq!(
                (report.renamed, report.failed_unchanged),
                (1, 3),
                "{:?}",
                report.rows
            );
            assert_eq!(report.rows[2].code, Some(ErrorCode::AlreadyExists));
            for file in ["a", "aa", "aaa"] {
                assert_eq!(std::fs::read(host.join(file)).unwrap(), file.as_bytes());
            }
            assert_eq!(std::fs::read(host.join("aaaa")).unwrap(), b"bestehend");
        } else {
            assert_eq!(
                (report.renamed, report.failed_unchanged),
                (4, 0),
                "{:?}",
                report.rows
            );
            for (index, file) in selected.iter().enumerate() {
                let renamed = format!("a{file}");
                assert_eq!(std::fs::read(host.join(&renamed)).unwrap(), file.as_bytes());
                assert_eq!(
                    provider
                        .stat(&Path::new(&pane).join(&renamed))
                        .unwrap()
                        .inode,
                    identities[index]
                );
            }
        }
        assert_eq!(
            std::fs::read_dir(&host).unwrap().count(),
            if collision { 5 } else { 4 }
        );
    }
}

#[test]
#[ignore = "benötigt einen isolierten lokalen Samba-Testshare"]
fn smb_additive_namensgruppen_lassen_unabhaengige_inhalte_zu() {
    use std::os::unix::fs::PermissionsExt;

    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    let server_root = required("DOPPELDECKER_SMB_TEST_ROOT");
    assert!(server_root.starts_with("/tmp/doppeldecker-smb-e2e."));
    let target_host = Path::new(&server_root).join("sandbox/name-groups");
    std::fs::create_dir_all(&target_host).unwrap();
    let config = SmbConnectionConfig {
        server_guid: discover_smb_server(&host, port).unwrap().server_guid,
        host,
        port,
        share: required("DOPPELDECKER_SMB_TEST_SHARE"),
        username: required("DOPPELDECKER_SMB_TEST_USER"),
        domain: String::new(),
        requested_root: "/sandbox".into(),
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "smb").unwrap(),
    };
    let provider = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets)).unwrap());
    let source_area = tempfile::tempdir().unwrap();
    let source_root = std::fs::canonicalize(source_area.path()).unwrap();
    std::fs::create_dir(source_root.join("tree")).unwrap();
    for name in [
        "a",
        "A",
        "é",
        "e\u{301}",
        "good",
        "tree/x",
        "tree/X",
        "tree/child",
    ] {
        std::fs::write(source_root.join(name), name).unwrap();
    }
    assert_eq!(
        std::fs::read_dir(source_root.join("tree")).unwrap().count(),
        3,
        "Baumprobe benötigt eine case-sensitive lokale Quelle"
    );
    assert_eq!(
        std::fs::read_dir(&source_root).unwrap().count(),
        6,
        "Probe benötigt getrennte Case- und NFC/NFD-Quellnamen"
    );
    let source = ProviderEndpoint::local(&source_root).unwrap();
    let target = ProviderEndpoint::smb(provider.clone(), Path::new("/name-groups")).unwrap();
    let items = ["a", "A", "é", "e\u{301}", "good", "tree"].map(|name| source_root.join(name));
    let contexts = source
        .interactive_file_contexts(&target, &items, false, false, &Cancel::new())
        .unwrap();
    let tree = contexts
        .source_vfs
        .interactive_source_tree()
        .unwrap()
        .unwrap();
    assert_eq!(
        (tree.files, tree.directories, tree.omitted.len()),
        (2, 1, 6)
    );
    let stats = doppeldecker_core::fileops::copy_items(
        doppeldecker_core::fileops::FileOperationRequest {
            src: contexts.source_vfs.as_ref(),
            dst: contexts.target_vfs.as_ref(),
            items: &items,
            dst_dir: Path::new("/name-groups"),
            policy: doppeldecker_core::fileops::OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((stats.files, stats.skipped), (2, 6));
    assert_eq!(std::fs::read(target_host.join("good")).unwrap(), b"good");
    assert_eq!(
        std::fs::read(target_host.join("tree/child")).unwrap(),
        b"tree/child"
    );
    assert_eq!(std::fs::read_dir(&target_host).unwrap().count(), 2);
    assert_eq!(
        std::fs::read_dir(target_host.join("tree")).unwrap().count(),
        1
    );
    std::fs::write(source_root.join("a"), b"changed omitted member").unwrap();
    assert_eq!(
        error_code(&contexts.source_vfs.interactive_source_tree().unwrap_err()),
        ErrorCode::PlanDrift
    );

    let drift_host = target_host.join("drift");
    std::fs::create_dir(&drift_host).unwrap();
    let target = ProviderEndpoint::smb(provider.clone(), Path::new("/name-groups/drift")).unwrap();
    for name in ["good1", "good2"] {
        std::fs::write(source_root.join(name), b"independent").unwrap();
    }
    let items = ["a", "A", "good1", "good2"].map(|name| source_root.join(name));
    let contexts = source
        .interactive_file_contexts(&target, &items, false, false, &Cancel::new())
        .unwrap();
    let original = std::fs::metadata(source_root.join("a")).unwrap();
    let mut changed = false;
    let result = doppeldecker_core::fileops::copy_items(
        doppeldecker_core::fileops::FileOperationRequest {
            src: contexts.source_vfs.as_ref(),
            dst: contexts.target_vfs.as_ref(),
            items: &items,
            dst_dir: Path::new("/name-groups/drift"),
            policy: doppeldecker_core::fileops::OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut |progress| {
            if !changed && progress.files_done == 1 {
                changed = true;
                std::fs::write(source_root.join("a"), b"drift after first commit").unwrap();
                filetime::set_file_mtime(
                    source_root.join("a"),
                    filetime::FileTime::from_last_modification_time(&original),
                )
                .unwrap();
            }
        },
    );
    assert!(changed);
    assert_eq!(error_code(&result.unwrap_err()), ErrorCode::PlanDrift);
    assert_eq!(
        std::fs::read(drift_host.join("good1")).unwrap(),
        b"independent"
    );
    assert!(!drift_host.join("good2").exists());

    for drift in [false, true] {
        let name = if drift { "queued-drift" } else { "queued-ok" };
        let queue_target_host = target_host.join(name);
        std::fs::create_dir(&queue_target_host).unwrap();
        let destination = format!("/name-groups/{name}");
        let target = ProviderEndpoint::smb(provider.clone(), Path::new(&destination)).unwrap();
        let items = ["a", "A", "é", "e\u{301}", "good", "tree"].map(|name| source_root.join(name));
        let tree = source
            .interactive_file_contexts(&target, &items, false, false, &Cancel::new())
            .unwrap()
            .source_vfs
            .interactive_source_tree()
            .unwrap()
            .unwrap();
        let queue_area = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let journal = std::fs::canonicalize(queue_area.path())
            .unwrap()
            .join("queue.json");
        let queue = OperationQueue::open(&journal).unwrap();
        queue
            .enqueue(QueueOperation::ProviderFile {
                kind: FileQueueKind::Copy,
                source: Box::new(BidirectionalQueueEndpoint::Local {
                    root: source_root.to_str().unwrap().into(),
                }),
                target: Box::new(BidirectionalQueueEndpoint::Smb {
                    connection: Box::new(config.clone()),
                    root: destination.clone(),
                }),
                items: items
                    .iter()
                    .map(|path| path.to_str().unwrap().into())
                    .collect(),
                destination,
                overwrite: false,
                expected_conflicts: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                keep_both_targets: Vec::new(),
                source_tree_expected: Some(Box::new(tree)),
                best_effort_commit_expected: None,
                local_copy_new_best_effort_expected: None,
                copy_new_best_effort_expected: Some(false),
                replace_best_effort_expected: Some(false),
                remove_source_best_effort_expected: Some(false),
            })
            .unwrap();
        drop(queue);
        if drift {
            std::fs::write(source_root.join("a"), b"Drift nach Queue-Neustart").unwrap();
        }
        let queue = OperationQueue::open(&journal).unwrap();
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
        if drift {
            assert!(matches!(
                result,
                QueueExecutionResult::AwaitingConfirmation { .. }
            ));
            assert!(matches!(
                queue.snapshot().entries[0].attention,
                Some(doppeldecker_core::QueueAttention::SourceTreeChanged { .. })
            ));
            assert_eq!(std::fs::read_dir(&queue_target_host).unwrap().count(), 0);
        } else {
            let QueueExecutionResult::File(stats) = result else {
                panic!("{result:?}");
            };
            assert_eq!((stats.files, stats.skipped), (2, 6));
            assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
            assert_eq!(
                std::fs::read(queue_target_host.join("good")).unwrap(),
                b"good"
            );
            assert_eq!(
                std::fs::read(queue_target_host.join("tree/child")).unwrap(),
                b"tree/child"
            );
            assert_eq!(std::fs::read_dir(&queue_target_host).unwrap().count(), 2);
        }
    }
}

fn additive_download_fixture(name: &str) -> (SmbConnectionConfig, Arc<SmbVfs>, std::path::PathBuf) {
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    let server_root = required("DOPPELDECKER_SMB_TEST_ROOT");
    assert!(server_root.starts_with("/tmp/doppeldecker-smb-e2e."));
    let root = Path::new(&server_root).join("sandbox").join(name);
    std::fs::create_dir_all(&root).unwrap();
    let config = SmbConnectionConfig {
        server_guid: discover_smb_server(&host, port).unwrap().server_guid,
        host,
        port,
        share: required("DOPPELDECKER_SMB_TEST_SHARE"),
        username: required("DOPPELDECKER_SMB_TEST_USER"),
        domain: String::new(),
        requested_root: "/sandbox".into(),
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "smb").unwrap(),
    };
    let provider = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets)).unwrap());
    (config, provider, root)
}

fn enqueue_additive_download(
    queue: &OperationQueue,
    config: &SmbConnectionConfig,
    root: &str,
    target: &Path,
    items: &[std::path::PathBuf],
    tree: Option<doppeldecker_core::vfs::InteractiveSourceTree>,
) {
    queue
        .enqueue(QueueOperation::ProviderFile {
            kind: FileQueueKind::Copy,
            source: Box::new(BidirectionalQueueEndpoint::Smb {
                connection: Box::new(config.clone()),
                root: root.into(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::Local {
                root: target.to_str().unwrap().into(),
            }),
            items: items
                .iter()
                .map(|path| path.to_str().unwrap().into())
                .collect(),
            destination: target.to_str().unwrap().into(),
            overwrite: false,
            expected_conflicts: Vec::new(),
            expected_conflict_proofs: Vec::new(),
            keep_both_targets: Vec::new(),
            source_tree_expected: tree.map(Box::new),
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: Some(false),
            replace_best_effort_expected: Some(false),
            remove_source_best_effort_expected: Some(false),
        })
        .unwrap();
}

#[test]
#[ignore = "benötigt einen isolierten lokalen Samba-Testshare"]
fn smb_additiver_download_bindet_handle_bis_zum_echten_eof() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let (config, _, host_root) = additive_download_fixture("download-handle");
    for attack in ["grow", "shrink", "swap", "cancel"] {
        let host = host_root.join(attack);
        std::fs::create_dir(&host).unwrap();
        let source_file = host.join("large");
        std::fs::write(&source_file, vec![42_u8; 512 * 1024]).unwrap();
        let inode = std::fs::metadata(&source_file).unwrap().ino();
        let area = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let target = area.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let target = std::fs::canonicalize(target).unwrap();
        let queue = OperationQueue::open(area.path().join("queue.json")).unwrap();
        let root = format!("/download-handle/{attack}");
        enqueue_additive_download(
            &queue,
            &config,
            &root,
            &target,
            &[Path::new(&root).join("large")],
            None,
        );
        let cancel = Cancel::new();
        let mut changed = false;
        let result = execute_provider_file_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &LocalQueueExecution {
                sandbox: None,
                sync_quarantine: None,
                max_delete_entries: 100,
                max_delete_fraction: 0.25,
            },
            Arc::new(TestSecrets),
            &cancel,
            &mut |progress| {
                if changed || progress.bytes_done == 0 {
                    return;
                }
                changed = true;
                assert_eq!(
                    progress.files_done, 0,
                    "Angriff muss vor lokalem Commit erfolgen"
                );
                match attack {
                    "grow" => {
                        std::fs::OpenOptions::new()
                            .append(true)
                            .open(&source_file)
                            .unwrap()
                            .write_all(b"extra")
                            .unwrap();
                        assert_eq!(std::fs::metadata(&source_file).unwrap().ino(), inode);
                    }
                    "shrink" => std::fs::OpenOptions::new()
                        .write(true)
                        .open(&source_file)
                        .unwrap()
                        .set_len(1)
                        .unwrap(),
                    "swap" => {
                        std::fs::write(host.join("replacement"), vec![43_u8; 512 * 1024]).unwrap();
                        std::fs::rename(host.join("replacement"), &source_file).unwrap();
                        assert_ne!(std::fs::metadata(&source_file).unwrap().ino(), inode);
                    }
                    "cancel" => cancel.cancel(),
                    _ => unreachable!(),
                }
            },
        );
        assert!(changed, "{attack}: kein Teiltransfer; Ergebnis: {result:?}");
        let result_paths = if attack == "cancel" {
            let QueueExecutionResult::File(stats) = result.unwrap() else {
                panic!("Abbruch lieferte keine Dateibilanz")
            };
            assert!(stats.cancelled);
            assert_eq!(stats.files, 0);
            stats.temp_recovery_paths
        } else {
            let error = result.unwrap_err();
            assert_eq!(
                error_code(&error),
                ErrorCode::PlanDrift,
                "{attack}: {error}"
            );
            doppeldecker_core::error::recovery_paths(&error)
        };
        assert!(!target.join("large").exists());
        let snapshot = queue.snapshot();
        let queued = &snapshot.entries[0];
        if attack == "cancel" {
            assert_eq!(queued.state, QueueState::Cancelled);
        }
        eprintln!(
            "SMB-{attack}: Queue-Recovery {:?}; Ergebnis-Recovery {result_paths:?}",
            queued.temp_recovery_paths
        );
        let neighbor_intents = queued
            .temp_recovery_paths
            .iter()
            .filter(|path| {
                Path::new(path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(".neighbor.doppeldecker-recovery-")
                            && name.ends_with(".recovery.json")
                    })
            })
            .map(|path| {
                let path = Path::new(path);
                let metadata = std::fs::symlink_metadata(path).unwrap();
                assert!(metadata.is_file() && !metadata.file_type().is_symlink());
                assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
                assert_eq!(path.parent(), Some(area.path()));
                assert!(result_paths.iter().any(|hint| Path::new(hint) == path));
                let intent: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                eprintln!("SMB-{attack}: Nachbarintent {}: {intent}", path.display());
                assert_eq!(intent["version"], 1);
                assert_eq!(intent["remote"], false);
                assert_eq!(
                    intent["owner"]["queue"],
                    area.path().join("queue.json").to_str().unwrap()
                );
                assert_eq!(intent["owner"]["operation"], queued.operation_id);
                assert_eq!(intent["target"], target.join("large").to_str().unwrap());
                intent
            })
            .collect::<Vec<_>>();
        // Dateireste brauchen den konkreten Intent und Recoveryhinweise in Queue
        // sowie Fehler beziehungsweise regulärer Abbruchbilanz.
        // Der lokale Vertrag kann private Container oder Nachbartemps verwenden.
        for remaining in std::fs::read_dir(&target).unwrap() {
            let path = remaining.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            eprintln!(
                "SMB-{attack}: Rest {} ({} Bytes, Modus {:o})",
                path.display(),
                metadata.len(),
                metadata.permissions().mode()
            );
            assert!(!metadata.file_type().is_symlink());
            let (files, manifest) = if metadata.is_dir() {
                assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
                let manifest: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(path.join("journal.json")).unwrap())
                        .unwrap();
                eprintln!("SMB-{attack}: Stagingmanifest {manifest}");
                assert_eq!(manifest["version"], 1);
                assert_eq!(manifest["parent"], target.to_str().unwrap());
                assert_eq!(
                    manifest["staging_identity"]["display_path"],
                    path.to_str().unwrap()
                );
                assert_eq!(
                    manifest["owner"]["queue"],
                    area.path().join("queue.json").to_str().unwrap()
                );
                assert_eq!(manifest["owner"]["operation"], queued.operation_id);
                assert!(path.join("lease").is_file());
                (
                    std::fs::read_dir(&path)
                        .unwrap()
                        .map(|child| child.unwrap().path())
                        .filter(|child| {
                            !matches!(
                                child.file_name().unwrap().to_str().unwrap(),
                                "journal.json" | "journal.next" | "lease"
                            )
                        })
                        .collect::<Vec<_>>(),
                    Some(manifest),
                )
            } else {
                assert!(metadata.is_file());
                (vec![path.clone()], None)
            };
            for file in files {
                let metadata = std::fs::symlink_metadata(&file).unwrap();
                assert!(metadata.is_file() && !metadata.file_type().is_symlink());
                let content = std::fs::read(&file).unwrap();
                eprintln!(
                    "SMB-{attack}: erhaltene Daten {}: {} Bytes, BLAKE3 {}",
                    file.display(),
                    content.len(),
                    blake3::hash(&content)
                );
                assert!(
                    content.len() <= 512 * 1024 && content.iter().all(|byte| *byte == 42),
                    "Datenrest weicht vom vorbereiteten Quellpayload ab"
                );
                let recorded = if let Some(manifest) = &manifest {
                    manifest["actions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|action| {
                            action["phase"] == "create_intent"
                                && action["from"] == file.to_str().unwrap()
                                && action["to"] == target.join("large").to_str().unwrap()
                        })
                } else {
                    neighbor_intents
                        .iter()
                        .any(|intent| intent["candidate"] == file.to_str().unwrap())
                };
                assert!(
                    recorded,
                    "Datenrest ohne passenden Intent: {}",
                    file.display()
                );
                for hints in [&queued.temp_recovery_paths, &result_paths] {
                    assert!(
                        hints.iter().any(|hint| Path::new(hint) == file
                            || (manifest.is_some() && Path::new(hint) == path)),
                        "Datenrest fehlt in Recovery: {}",
                        file.display()
                    );
                }
            }
        }
        assert!(
            queue.claim_next().unwrap().is_none(),
            "kein automatischer zweiter Kopierversuch"
        );
    }
}

#[test]
#[ignore = "benötigt einen isolierten lokalen Samba-Testshare"]
fn smb_additiver_download_bindet_namensgruppen_und_queue_neustart() {
    use std::os::unix::fs::PermissionsExt;
    let (config, provider, host) = additive_download_fixture("download-groups");
    std::fs::create_dir(host.join("tree")).unwrap();
    for name in [
        "a",
        "A",
        "é",
        "e\u{301}",
        "good",
        "tree/x",
        "tree/X",
        "tree/child",
    ] {
        std::fs::write(host.join(name), name).unwrap();
    }
    for name in ["Clash", "clash", "tree/Pair", "tree/pair"] {
        std::fs::create_dir(host.join(name)).unwrap();
        std::fs::write(
            host.join(name).join("unzulässig\n"),
            b"darf nie betreten werden",
        )
        .unwrap();
    }
    assert_eq!(
        std::fs::read_dir(&host).unwrap().count(),
        8,
        "Probe benötigt getrennte Case-/Unicode-Namen"
    );
    let source = ProviderEndpoint::smb(provider.clone(), Path::new("/download-groups")).unwrap();
    let items = ["a", "A", "é", "e\u{301}", "good", "tree", "Clash", "clash"]
        .map(|name| Path::new("/download-groups").join(name));
    for attack in [
        "none",
        "omitted",
        "recursive-omitted",
        "between-commits",
        "sibling",
        "public-queue",
    ] {
        let area = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let target = area.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let target = std::fs::canonicalize(target).unwrap();
        let target_endpoint = ProviderEndpoint::local(&target).unwrap();
        let contexts = source
            .interactive_file_contexts(&target_endpoint, &items, false, false, &Cancel::new())
            .unwrap();
        let tree = contexts
            .source_vfs
            .interactive_source_tree()
            .unwrap()
            .unwrap();
        assert_eq!(
            (tree.files, tree.directories, tree.omitted.len()),
            (2, 1, 10)
        );
        if attack == "public-queue" {
            std::fs::set_permissions(area.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        }
        let journal = area.path().join("queue.json");
        let queue = OperationQueue::open(&journal).unwrap();
        enqueue_additive_download(
            &queue,
            &config,
            "/download-groups",
            &target,
            &items,
            Some(tree.clone()),
        );
        drop(queue);
        match attack {
            "omitted" => std::fs::write(host.join("a"), b"changed omitted direct").unwrap(),
            "recursive-omitted" => {
                std::fs::write(host.join("tree/x"), b"changed omitted recursive").unwrap()
            }
            "sibling" => {
                std::fs::write(host.join("unselected"), b"independent").unwrap();
                assert_eq!(
                    contexts
                        .source_vfs
                        .interactive_source_tree()
                        .unwrap()
                        .unwrap()
                        .digest,
                    tree.digest
                );
                assert_eq!(
                    source
                        .interactive_file_contexts(
                            &target_endpoint,
                            &items,
                            false,
                            false,
                            &Cancel::new()
                        )
                        .unwrap()
                        .source_vfs
                        .interactive_source_tree()
                        .unwrap()
                        .unwrap()
                        .digest,
                    tree.digest
                );
            }
            _ => {}
        }
        let queue = OperationQueue::open(&journal).unwrap();
        let mut changed = false;
        let result = execute_provider_file_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &LocalQueueExecution {
                sandbox: None,
                sync_quarantine: None,
                max_delete_entries: 100,
                max_delete_fraction: 0.25,
            },
            Arc::new(TestSecrets),
            &Cancel::new(),
            &mut |progress| {
                if attack == "between-commits" && !changed && progress.files_done == 1 {
                    changed = true;
                    std::fs::write(host.join("a"), b"drift between commits").unwrap();
                }
            },
        );
        match attack {
            "omitted" | "recursive-omitted" => {
                assert!(matches!(
                    result.unwrap(),
                    QueueExecutionResult::AwaitingConfirmation { .. }
                ));
                assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
            }
            "between-commits" => {
                assert!(changed);
                assert_eq!(error_code(&result.unwrap_err()), ErrorCode::PlanDrift);
                assert!(target.join("good").exists());
                assert!(!target.join("tree/child").exists());
            }
            _ => {
                let QueueExecutionResult::File(stats) = result.unwrap() else {
                    panic!("Dateibilanz fehlt")
                };
                assert_eq!((stats.files, stats.skipped), (2, 10));
                assert_eq!(
                    stats
                        .target_write_limitations
                        .iter()
                        .any(|note| note.contains("Keine dauerhafte Temp-Recovery")),
                    attack == "public-queue",
                    "Recoverybilanz bei {attack}: {:?}",
                    stats.target_write_limitations,
                );
                assert_eq!(
                    std::fs::read(target.join("tree/child")).unwrap(),
                    b"tree/child"
                );
            }
        }
    }
    // Der geöffnete Inhaltsstrom behält seine alte Verbindung. Ein Reconnect
    // desselben Providers muss trotzdem vor dem lokalen Commit abbrechen.
    std::fs::write(host.join("live"), vec![42_u8; 512 * 1024]).unwrap();
    let live_area = tempfile::tempdir().unwrap();
    let live_target = std::fs::canonicalize(live_area.path()).unwrap();
    let live_endpoint = ProviderEndpoint::local(&live_target).unwrap();
    let live_items = [Path::new("/download-groups/live").to_path_buf()];
    let live_contexts = source
        .interactive_file_contexts(&live_endpoint, &live_items, false, false, &Cancel::new())
        .unwrap();
    let generation = provider.session_generation();
    let mut reconnects = 0;
    let mut transferred = 0;
    let result = doppeldecker_core::fileops::copy_items(
        doppeldecker_core::fileops::FileOperationRequest {
            src: live_contexts.source_vfs.as_ref(),
            dst: live_contexts.target_vfs.as_ref(),
            items: &live_items,
            dst_dir: &live_target,
            policy: doppeldecker_core::fileops::OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut |progress| {
            transferred = transferred.max(progress.bytes_done);
            if reconnects == 0 && progress.bytes_done > 0 {
                assert_eq!(progress.files_done, 0);
                assert!(progress.bytes_done < 512 * 1024);
                provider.reconnect().unwrap();
                reconnects += 1;
            }
        },
    );
    assert_eq!(reconnects, 1, "kein Reconnect im Teiltransfer: {result:?}");
    assert_eq!(error_code(&result.unwrap_err()), ErrorCode::PlanDrift);
    assert_eq!(provider.session_generation(), generation + 1);
    assert!(
        transferred > 0 && transferred < 512 * 1024,
        "kein vollständiger oder neu begonnener Transfer"
    );
    assert!(!live_target.join("live").exists());
    let area = tempfile::tempdir().unwrap();
    let target = ProviderEndpoint::local(area.path()).unwrap();
    let contexts = source
        .interactive_file_contexts(&target, &items, false, false, &Cancel::new())
        .unwrap();
    provider.reconnect().unwrap();
    assert_eq!(
        error_code(&contexts.source_vfs.interactive_source_tree().unwrap_err()),
        ErrorCode::PlanDrift
    );
}

fn smb_batch_spelling_probe(config: &SmbConnectionConfig, server_root: &Path) {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::smb_batch_rename::PreparedSmbBatchRename;
    use std::os::unix::fs::MetadataExt;
    for (index, (old, new, kind)) in [
        ("Foo", "foo", "file"),
        ("Foo", "foo", "dir"),
        ("café", "cafe\u{301}", "file"),
        ("cafe\u{301}", "café", "file"),
        ("Foo", "foo", "outside_link"),
        ("Foo", "foo", "alias"),
        ("Foo", "foo", "alias_link"),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("batch-spelling-{index}");
        let host = server_root.join("sandbox").join(&name);
        std::fs::create_dir(&host).unwrap();
        if kind == "dir" {
            std::fs::create_dir(host.join(old)).unwrap();
            std::fs::write(host.join(old).join("child"), b"child-original").unwrap();
        } else {
            std::fs::write(host.join(old), b"original").unwrap();
        }
        if kind == "outside_link" {
            std::fs::hard_link(host.join(old), host.join("unrelated")).unwrap();
        }
        if kind == "alias" {
            std::fs::write(host.join(new), b"foreign").unwrap();
        }
        if kind == "alias_link" {
            std::fs::hard_link(host.join(old), host.join(new)).unwrap();
        }
        let independent_target = format!("{new}independent");
        std::fs::write(host.join("independent"), b"independent").unwrap();
        let inode = std::fs::symlink_metadata(host.join(old)).unwrap().ino();
        let child_inode = (kind == "dir").then(|| {
            std::fs::metadata(host.join(old).join("child"))
                .unwrap()
                .ino()
        });
        let mut before_names = std::fs::read_dir(&host)
            .unwrap()
            .map(|item| item.unwrap().file_name())
            .collect::<Vec<_>>();
        before_names.sort();
        let provider = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets)).unwrap());
        let pane = format!("/{name}");
        let endpoint = ProviderEndpoint::smb(provider.clone(), Path::new(&pane)).unwrap();
        let options = BatchRenameOptions {
            find: old.into(),
            replacement: String::new(),
            prefix: new.into(),
            suffix: String::new(),
            preserve_extension: false,
            trim_outer_spaces: false,
            numbering: None,
        };
        let plan = PreparedSmbBatchRename::prepare(
            &endpoint,
            &[old.into(), "independent".into()],
            &options,
            &Cancel::new(),
        )
        .unwrap();
        let blocked = matches!(kind, "alias" | "alias_link");
        assert_eq!(
            plan.rows()[0].status,
            if blocked {
                BatchRenameStatus::Blocked
            } else {
                BatchRenameStatus::Ready
            },
            "{kind}: {:?}",
            plan.rows()
        );
        assert_eq!(
            plan.rows()[1].status,
            BatchRenameStatus::Ready,
            "{kind}: {:?}",
            plan.rows()
        );
        assert_eq!(plan.rows()[1].new_name, independent_target);
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
        assert_eq!(report.outcome_unclear, 0, "{kind}: {:?}", report.rows);
        assert_eq!(
            report.rows[1].status,
            BatchRenameStatus::Renamed,
            "{kind}: {:?}",
            report.rows
        );
        assert_eq!(
            std::fs::read(host.join(&independent_target)).unwrap(),
            b"independent"
        );
        let stored = std::fs::read_dir(&host)
            .unwrap()
            .map(|item| item.unwrap().file_name())
            .collect::<Vec<_>>();
        let listing = provider.list_complete(Path::new(&pane)).unwrap();
        assert!(listing.complete);
        assert_eq!(listing.entries.len(), stored.len());
        for item in &listing.entries {
            use std::os::unix::ffi::OsStrExt;
            assert!(stored.iter().any(|name| name.as_bytes() == item.raw_name));
            let metadata = std::fs::symlink_metadata(host.join(&item.entry.name)).unwrap();
            assert_eq!(item.entry.inode, Some(metadata.ino()));
            assert_eq!(item.entry.is_dir, metadata.is_dir());
        }
        let mut expected = before_names
            .into_iter()
            .map(|name| {
                if name == "independent" {
                    independent_target.clone().into()
                } else if !blocked && name == old {
                    new.into()
                } else {
                    name
                }
            })
            .collect::<Vec<_>>();
        expected.sort();
        let mut actual = stored.clone();
        actual.sort();
        assert_eq!(actual, expected, "{kind}: vollständiger Rohnamenssatz");
        if blocked {
            assert_eq!(report.rows[0].status, BatchRenameStatus::Blocked);
            assert_eq!(std::fs::metadata(host.join(old)).unwrap().ino(), inode);
            assert_eq!(std::fs::read(host.join(old)).unwrap(), b"original");
            assert_eq!(
                std::fs::read(host.join(new)).unwrap(),
                if kind == "alias" {
                    b"foreign".as_slice()
                } else {
                    b"original".as_slice()
                }
            );
        } else {
            assert_eq!(
                report.rows[0].status,
                BatchRenameStatus::Renamed,
                "{kind}: {:?}",
                report.rows
            );
            use std::os::unix::ffi::OsStrExt;
            assert!(stored.iter().any(|name| name.as_bytes() == new.as_bytes()));
            assert!(!stored.iter().any(|name| name.as_bytes() == old.as_bytes()));
            assert_eq!(std::fs::metadata(host.join(new)).unwrap().ino(), inode);
            if kind == "dir" {
                assert_eq!(
                    std::fs::metadata(host.join(new).join("child"))
                        .unwrap()
                        .ino(),
                    child_inode.unwrap()
                );
                assert_eq!(
                    std::fs::read(host.join(new).join("child")).unwrap(),
                    b"child-original"
                );
            } else {
                assert_eq!(std::fs::read(host.join(new)).unwrap(), b"original");
            }
            if kind == "outside_link" {
                assert_eq!(
                    std::fs::metadata(host.join("unrelated")).unwrap().ino(),
                    inode
                );
                assert_eq!(std::fs::read(host.join("unrelated")).unwrap(), b"original");
            }
        }
        eprintln!("SMB-Produkt-Schreibweisenprobe {index}/{kind}: vollständiger Rohbestand, File-ID, Typ und Inhalt bestätigt");
    }
}

#[test]
#[ignore = "benötigt einen isolierten lokalen Samba-Testshare"]
fn content_sync_smb_addonly_update_mirror_ohne_quarantaeneversprechen() {
    use doppeldecker_core::execute_prepared_content_sync_queue_entry;
    let (config, provider, remote_root) = additive_download_fixture("content-sync");
    let source_guard = tempfile::tempdir().unwrap();
    let source = source_guard.path().canonicalize().unwrap();
    std::fs::create_dir_all(source.join("Baum/Leer")).unwrap();
    std::fs::write(source.join("Baum/Kind"), b"kind").unwrap();
    std::fs::write(source.join("Datei"), b"alpha").unwrap();
    let expected_time = 1_700_000_000;
    for path in [
        source.join("Datei"),
        source.join("Baum/Kind"),
        source.join("Baum/Leer"),
        source.join("Baum"),
    ] {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(expected_time, 0))
            .unwrap();
    }
    use std::os::unix::fs::PermissionsExt;
    let area = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let queue = OperationQueue::open(area.path().join("operations-queue.json")).unwrap();
    for mode in [SyncMode::AddOnly, SyncMode::Update, SyncMode::Mirror] {
        if mode == SyncMode::Update {
            std::fs::write(source.join("Datei"), b"bravo").unwrap();
        }
        if mode == SyncMode::Mirror {
            std::fs::create_dir_all(remote_root.join("Alt/Unter")).unwrap();
            std::fs::write(remote_root.join("Alt/Unter/Datei"), b"alt").unwrap();
        }
        let options = SyncOptions {
            mode,
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        };
        let target = ProviderEndpoint::smb(provider.clone(), Path::new("/content-sync")).unwrap();
        let prepared = doppeldecker_core::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::local(&source).unwrap(),
            target,
            options.clone(),
            &Cancel::new(),
        )
        .unwrap();
        queue
            .enqueue(QueueOperation::ContentSync {
                source: Box::new(BidirectionalQueueEndpoint::Local {
                    root: source.to_string_lossy().into_owned(),
                }),
                target: Box::new(BidirectionalQueueEndpoint::Smb {
                    connection: Box::new(config.clone()),
                    root: "/content-sync".into(),
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
        if mode == SyncMode::Mirror {
            assert_eq!(stats.files_deleted, 3);
        }
    }
    assert_eq!(std::fs::read(remote_root.join("Datei")).unwrap(), b"bravo");
    use std::os::unix::fs::MetadataExt;
    for path in [
        remote_root.join("Baum"),
        remote_root.join("Baum/Leer"),
        remote_root.join("Baum/Kind"),
    ] {
        assert_eq!(std::fs::metadata(path).unwrap().mtime(), expected_time);
    }
    assert!(remote_root.join("Baum/Leer").is_dir());
    assert!(!remote_root.join("Alt").exists());
    assert!(!provider.capabilities().permits_update());
    assert!(!provider.capabilities().permits_mirror());
}

#[test]
#[ignore = "benötigt einen isolierten lokalen Samba-Testshare"]
fn smb_dateinamenzyklen_mit_dauerhafter_queuebilanz() {
    let server_root = required("DOPPELDECKER_SMB_TEST_ROOT");
    assert!(server_root.starts_with("/tmp/doppeldecker-smb-e2e."));
    let server_root = Path::new(&server_root);
    std::fs::create_dir(server_root.join("sandbox")).unwrap();
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    let discovery = discover_smb_server(&host, port).unwrap();
    let config = SmbConnectionConfig {
        host,
        port,
        server_guid: discovery.server_guid,
        share: required("DOPPELDECKER_SMB_TEST_SHARE"),
        username: required("DOPPELDECKER_SMB_TEST_USER"),
        domain: String::new(),
        requested_root: "/sandbox".into(),
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "smb").unwrap(),
    };
    smb_cycle_probe(&config, server_root);
    assert_eq!(
        std::fs::read_dir(server_root.join("sandbox"))
            .unwrap()
            .count(),
        0
    );
}

fn smb_cycle_probe(config: &SmbConnectionConfig, server_root: &Path) {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::smb_batch_rename::PreparedSmbBatchRename;
    use doppeldecker_core::{OperationQueue, QueueState};
    for directories in [false, true] {
        for interrupted in [false, true] {
            let leaf = if interrupted {
                "batch-cycle-interrupted"
            } else {
                "batch-cycle-complete"
            };
            let host = server_root.join("sandbox").join(leaf);
            std::fs::create_dir(&host).unwrap();
            let names = ["ab", "aab", "aaab"];
            let content = |name: &str| {
                let path = host.join(name);
                if directories {
                    path.join("inside/leaf")
                } else {
                    path
                }
            };
            for (index, name) in names.iter().enumerate() {
                if directories {
                    std::fs::create_dir_all(host.join(name).join("inside")).unwrap();
                }
                std::fs::write(content(name), format!("Original{index}")).unwrap();
            }
            let provider =
                Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets)).unwrap());
            let endpoint = ProviderEndpoint::smb(provider, Path::new(&format!("/{leaf}"))).unwrap();
            let options = BatchRenameOptions {
                find: "aaa".into(),
                replacement: String::new(),
                prefix: "a".into(),
                suffix: String::new(),
                preserve_extension: false,
                trim_outer_spaces: false,
                numbering: None,
            };
            let plan = PreparedSmbBatchRename::prepare(
                &endpoint,
                &names.map(str::to_string),
                &options,
                &Cancel::new(),
            )
            .unwrap();
            assert!(plan
                .rows()
                .iter()
                .all(|row| row.status == BatchRenameStatus::Ready));
            let config_dir = tempfile::Builder::new()
                .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .tempdir()
                .unwrap();
            let queue = OperationQueue::open(config_dir.path().join("queue.json")).unwrap();
            let cancel = Cancel::new();
            let report = plan.execute_with_queue(
                &cancel,
                &mut || {
                    if interrupted
                        && std::fs::read_dir(&host).unwrap().any(|entry| {
                            entry
                                .unwrap()
                                .file_name()
                                .to_string_lossy()
                                .starts_with(".doppeldecker-rename-")
                        })
                    {
                        cancel.cancel();
                    }
                    Ok(())
                },
                &mut |_, _| {},
                &queue,
            );
            if interrupted {
                assert_eq!(
                    report.partially_renamed, 1,
                    "{:?} {:?}",
                    report.rows, report.limitations
                );
                let name = report.rows[0].current_name.as_ref().unwrap();
                assert_eq!(std::fs::read(content(name)).unwrap(), b"Original0");
                assert_eq!(std::fs::read(content("aab")).unwrap(), b"Original1");
                assert_eq!(std::fs::read(content("aaab")).unwrap(), b"Original2");
                assert_eq!(queue.snapshot().entries[0].state, QueueState::Interrupted);
            } else {
                assert_eq!(
                    report.renamed, 3,
                    "{:?} {:?}",
                    report.rows, report.limitations
                );
                for (index, name) in ["aab", "aaab", "ab"].iter().enumerate() {
                    assert_eq!(
                        std::fs::read(content(name)).unwrap(),
                        format!("Original{index}").as_bytes()
                    );
                }
                assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
            }
            assert_eq!(std::fs::read_dir(&host).unwrap().count(), 3);
            std::fs::remove_dir_all(&host).unwrap();
        }
    }
}

#[path = "support/archive_upload_probe.rs"]
mod archive_upload_probe;
#[test]
#[ignore = "benötigt einen isolierten lokalen Samba-Testshare"]
fn smb_uebertraegt_erzeugte_zip_und_entpackquellen() {
    let root = required("DOPPELDECKER_SMB_TEST_ROOT");
    assert!(root.starts_with("/tmp/doppeldecker-smb-e2e."));
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    let discovery = discover_smb_server(&host, port).unwrap();
    let config = SmbConnectionConfig {
        host,
        port,
        server_guid: discovery.server_guid,
        share: required("DOPPELDECKER_SMB_TEST_SHARE"),
        username: required("DOPPELDECKER_SMB_TEST_USER"),
        domain: String::new(),
        requested_root: "/".into(),
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "smb").unwrap(),
    };
    archive_upload_probe::run(
        BidirectionalQueueEndpoint::Smb {
            connection: Box::new(config),
            root: "/".into(),
        },
        Path::new(&root),
        Arc::new(TestSecrets),
    );
}
