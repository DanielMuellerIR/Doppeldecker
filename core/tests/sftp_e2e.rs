//! Opt-in-Gegenlauf gegen einen echten SFTP-Server.
//!
//! Der Test ist absichtlich ignoriert: Der Aufrufer muss einen frischen künstlichen
//! Server-Root sowie öffentliche Host-/Agent-Fingerprints setzen. Es gibt keine
//! ungesicherte „Hostkey akzeptieren"-Abkürzung und keine Passwort-Umgebungsvariable.

use std::env;
#[cfg(target_os = "macos")]
use std::ffi::CStr;
use std::fs;
use std::fs::File;
use std::net::IpAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Barrier};

use doppeldecker_core::{
    error_code, prepare_sftp_bidirectional_rollback, prepare_sftp_bidirectional_with_decisions,
    prepare_sftp_download, prepare_sftp_queue_entry, prepare_sftp_upload, ErrorCode,
    OperationQueue, PairId, Progress, ProviderEndpoint, QueueOperation, QueueState, SecretResolver,
    SecretString, SftpAuthenticationReference, SftpConnectionConfig, SftpHostKeyPin,
    SftpQueueDirection, SftpVfs, SyncOptions, SyncPlan, Vfs,
};

struct NoSecrets;

impl SecretResolver for NoSecrets {
    fn resolve(
        &self,
        _reference: &doppeldecker_core::KeychainSecretReference,
    ) -> std::io::Result<SecretString> {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Der E2E-Test verwendet ausschließlich einen bereits laufenden SSH-Agenten",
        ))
    }
}

#[test]
#[ignore = "benötigt künstlichen SFTP-Testroot ohne fsync-Ansage"]
fn sftp_ohne_fsync_erlaubt_sichtbaren_best_effort_upload() {
    assert_eq!(required("DOPPELDECKER_SFTP_EXPECT_NO_FSYNC"), "1");
    let guarded = guarded_runner_root().unwrap();
    let host = guarded.host.clone();
    let port = required("DOPPELDECKER_SFTP_TEST_PORT").parse().unwrap();
    let username = required("DOPPELDECKER_SFTP_TEST_USER");
    let root = guarded.root.clone();
    let config = SftpConnectionConfig {
        host,
        alternate_hosts: Vec::new(),
        port,
        username,
        requested_root: root.clone(),
        host_key: SftpHostKeyPin {
            algorithm: required("DOPPELDECKER_SFTP_TEST_HOST_KEY_ALGORITHM"),
            sha256: digest("DOPPELDECKER_SFTP_TEST_HOST_KEY_SHA256_HEX"),
        },
        authentication: SftpAuthenticationReference::SshAgent {
            public_key_sha256: digest("DOPPELDECKER_SFTP_TEST_AGENT_KEY_SHA256_HEX"),
        },
    };
    let vfs = Arc::new(SftpVfs::connect(config.clone(), Arc::new(NoSecrets)).unwrap());
    assert!(!vfs.capabilities().durable_file_flush);
    assert!(!vfs.capabilities().complete_listing);

    let endpoint = ProviderEndpoint::sftp(vfs.clone(), Path::new("/")).unwrap();
    let capabilities = endpoint.probe_file_capabilities().unwrap();
    assert!(!capabilities.copy_new);
    assert!(!capabilities.copy_new_best_effort);
    assert!(!capabilities.remove_source);
    assert!(!capabilities.remove_source_best_effort);
    assert!(capabilities
        .limitations
        .iter()
        .any(|note| note.contains("SFTP-Ordneranlage ist gesperrt")));
    assert!(!vfs.capabilities().best_effort_file_commit);
    assert!(!vfs.capabilities().permits_add_only());
    assert_remote_root_empty(vfs.as_ref());

    // Der historische Testname bleibt wegen des isolierten Runners stabil. Der
    // Vertrag ist jetzt absichtlich stärker: Auch eine neue reguläre Datei bleibt
    // ohne atomare Elternbindung vor OPEN gesperrt; fehlendes fsync ist nicht mehr
    // die erste Sicherheitsgrenze.
    guarded.assert_server_root_unchanged();
    let write_error = match vfs.open_write_new(Path::new("/best-effort-upload.txt")) {
        Ok(_) => panic!("SFTP-OPEN lief ohne atomare Elternbindung"),
        Err(error) => error,
    };
    assert_eq!(
        error_code(&write_error),
        ErrorCode::Unsupported,
        "OPEN muss an der Elternbindungsgrenze scheitern"
    );
    assert!(!Path::new(&root).join("best-effort-upload.txt").exists());
    assert_remote_root_empty(vfs.as_ref());

    // Passive Fähigkeitsabfragen starten überhaupt keine pfadbasierte Probe. Auch
    // ein ungeeigneter Pfad ändert deshalb nur die bereits bekannte Negativmatrix.
    let error_parent = Path::new(&root).join("probe-error-parent");
    guarded.assert_server_root_unchanged();
    std::fs::write(&error_parent, b"kein Verzeichnis\n").unwrap();
    assert!(!vfs
        .probe_add_only_capabilities_at(Path::new("/probe-error-parent"))
        .unwrap()
        .permits_add_only());
    assert_eq!(remote_root_names(vfs.as_ref()), ["probe-error-parent"]);
    guarded.assert_server_root_unchanged();
    std::fs::remove_file(error_parent).unwrap();
    assert_remote_root_empty(vfs.as_ref());
}

#[test]
#[ignore = "benötigt künstlichen SFTP-Testroot und öffentliche Fingerprints"]
fn echter_sftp_root_bindet_identitaet_probe_reconnect_und_streams() {
    let guarded = guarded_runner_root().unwrap();
    let host = guarded.host.clone();
    let port = required("DOPPELDECKER_SFTP_TEST_PORT").parse().unwrap();
    let username = required("DOPPELDECKER_SFTP_TEST_USER");
    let root = guarded.root.clone();
    // macOS bildet `/tmp` als Symlink auf `/private/tmp` ab. Die SFTP-Identität
    // meldet absichtlich den echten kanonischen Serverpfad; der Test muss daher
    // denselben sicheren Testroot kanonisieren, statt den Symlinktext zu erwarten.
    let canonical_root = guarded.canonical_root.clone();

    let config = SftpConnectionConfig {
        host,
        alternate_hosts: Vec::new(),
        port,
        username,
        requested_root: root.clone(),
        host_key: SftpHostKeyPin {
            algorithm: required("DOPPELDECKER_SFTP_TEST_HOST_KEY_ALGORITHM"),
            sha256: digest("DOPPELDECKER_SFTP_TEST_HOST_KEY_SHA256_HEX"),
        },
        authentication: SftpAuthenticationReference::SshAgent {
            public_key_sha256: digest("DOPPELDECKER_SFTP_TEST_AGENT_KEY_SHA256_HEX"),
        },
    };
    let vfs = Arc::new(SftpVfs::connect(config.clone(), Arc::new(NoSecrets)).unwrap());
    assert_eq!(vfs.identity().canonical_root, canonical_root);
    assert_eq!(
        error_code(&vfs.list_complete(Path::new("/")).unwrap_err()),
        ErrorCode::Unsupported
    );
    let initial_listing = vfs.list_interactive_best_effort(Path::new("/")).unwrap();
    assert!(initial_listing.entries.is_empty());
    assert_eq!(
        initial_listing.security,
        doppeldecker_core::sftp::SftpReadSecurity::SftpPathBestEffort
    );
    assert_eq!(initial_listing.session_generation, vfs.session_generation());
    let read_source = Path::new(&root).join("ap04-inhalt.txt");
    fs::write(&read_source, b"AP04: gepruefter SFTP-Inhalt\n").unwrap();
    let cache = tempfile::tempdir().unwrap();
    let content_endpoint = ProviderEndpoint::sftp(vfs.clone(), Path::new("/")).unwrap();
    let (content_vfs, _) = content_endpoint
        .interactive_file_read_context(
            &[PathBuf::from("/ap04-inhalt.txt")],
            &doppeldecker_core::Cancel::new(),
        )
        .unwrap();
    let snapshot = doppeldecker_core::download_remote_snapshot(
        content_vfs.as_ref(),
        Path::new("/ap04-inhalt.txt"),
        cache.path(),
        &doppeldecker_core::Cancel::new(),
    )
    .unwrap();
    assert_eq!(
        fs::read(&snapshot.path).unwrap(),
        b"AP04: gepruefter SFTP-Inhalt\n"
    );
    fs::remove_file(read_source).unwrap();
    let tree_root = Path::new(&root).join("ap04r-baum");
    fs::create_dir(&tree_root).unwrap();
    fs::create_dir(tree_root.join("Leer")).unwrap();
    fs::write(tree_root.join("Grüße %2F.txt"), b"AP04R: Baum\n").unwrap();
    std::os::unix::fs::symlink(tree_root.join("Grüße %2F.txt"), tree_root.join("Link")).unwrap();
    let tree_items = [PathBuf::from("/ap04r-baum")];
    let (tree_source, _) = content_endpoint
        .interactive_file_read_context(&tree_items, &doppeldecker_core::Cancel::new())
        .unwrap();
    let summary = tree_source.interactive_source_tree().unwrap().unwrap();
    assert_eq!(
        (summary.files, summary.directories, summary.omitted.len()),
        (1, 2, 1)
    );
    assert!(tree_source.list_complete(Path::new("/ap04r-baum")).is_err());
    let target = tempfile::tempdir().unwrap();
    let target_root = target.path().canonicalize().unwrap();
    let target_vfs = doppeldecker_core::LocalVfs::rooted(&target_root).unwrap();
    let copied = doppeldecker_core::copy_items(
        doppeldecker_core::FileOperationRequest {
            src: tree_source.as_ref(),
            dst: &target_vfs,
            items: &tree_items,
            dst_dir: &target_root,
            policy: doppeldecker_core::OverwritePolicy::Skip,
            sandbox: &None,
        },
        &doppeldecker_core::Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((copied.files, copied.skipped), (1, 1));
    assert_eq!(
        fs::read(target_root.join("ap04r-baum/Grüße %2F.txt")).unwrap(),
        b"AP04R: Baum\n"
    );
    assert!(target_root.join("ap04r-baum/Leer").is_dir());
    assert!(!target_root.join("ap04r-baum/Link").exists());
    fs::remove_dir_all(tree_root).unwrap();
    assert!(!vfs.capabilities().permits_add_only());
    assert!(!vfs.capabilities().permits_directory_create());

    // SFTP v3 kann MKDIR nicht an einen gehaltenen Elternordner binden. Deshalb
    // bleibt die Fähigkeit auch nach der rein lesenden Probe aus, und beide
    // öffentlichen Ordneranlagewege stoppen vor jeder Außenwirkung.
    let mkdir_capabilities = vfs.probe_directory_create_capability().unwrap();
    assert!(!mkdir_capabilities.permits_directory_create());
    let mkdir_error = vfs.create_dir(Path::new("/mkdir-gesperrt")).unwrap_err();
    assert_eq!(error_code(&mkdir_error), ErrorCode::Unsupported);
    let mkdir_all_error = vfs
        .create_dir_all(Path::new("/mkdir-all-gesperrt"))
        .unwrap_err();
    assert_eq!(error_code(&mkdir_all_error), ErrorCode::Unsupported);
    assert!(!Path::new(&root).join("mkdir-gesperrt").exists());
    assert!(!Path::new(&root).join("mkdir-all-gesperrt").exists());

    // Sowohl ein finaler als auch ein übergeordneter Symlink zeigen hier auf
    // künstliche Dateien außerhalb des kanonischen Serverroots. `open_read` darf
    // diese Namen niemals durch den Server auflösen und öffnen.
    let owner_root = Path::new(&root).parent().unwrap();
    let outside_file = owner_root.join("outside-read.txt");
    let outside_dir = owner_root.join("outside-read-dir");
    let outside_nested = outside_dir.join("nested");
    let outside_mutation_sentinel = outside_dir.join("mutation-sentinel.txt");
    guarded.assert_server_root_unchanged();
    fs::write(&outside_file, b"ausserhalb\n").unwrap();
    fs::create_dir(&outside_dir).unwrap();
    fs::create_dir(&outside_nested).unwrap();
    fs::write(outside_dir.join("secret.txt"), b"ausserhalb-dir\n").unwrap();
    fs::write(&outside_mutation_sentinel, b"nicht mutieren\n").unwrap();

    // Der Barrierentausch bildet das kritische Interleaving deterministisch nach:
    // Ein zunächst echter Elternordner wird unmittelbar vor den öffentlichen
    // Sicherheits-Lesewegen gegen einen Link auf einen gleichartig aufgebauten
    // Fremdbaum ersetzt. Diese Wege müssen schon vor jeder Serveroperation stoppen.
    let race_parent = Path::new(&root).join("race-parent");
    let race_parked = Path::new(&root).join("race-parent-inside");
    let race_outside = owner_root.join("race-outside");
    fs::create_dir(&race_parent).unwrap();
    fs::write(race_parent.join("gleich.txt"), b"innen\n").unwrap();
    fs::create_dir(&race_outside).unwrap();
    fs::write(race_outside.join("gleich.txt"), b"aussen\n").unwrap();
    let race_ready = Arc::new(Barrier::new(2));
    let race_done = Arc::new(Barrier::new(2));
    let swap_ready = race_ready.clone();
    let swap_done = race_done.clone();
    let swap_parent = race_parent.clone();
    let swap_parked = race_parked.clone();
    let swap_outside = race_outside.clone();
    let swap = std::thread::spawn(move || {
        swap_ready.wait();
        fs::rename(&swap_parent, &swap_parked).unwrap();
        std::os::unix::fs::symlink(&swap_outside, &swap_parent).unwrap();
        swap_done.wait();
    });
    race_ready.wait();
    race_done.wait();
    swap.join().unwrap();
    assert_eq!(
        error_code(&vfs.list_complete(Path::new("/race-parent")).unwrap_err()),
        ErrorCode::Unsupported
    );
    assert_eq!(
        error_code(&vfs.stat(Path::new("/race-parent/gleich.txt")).unwrap_err()),
        ErrorCode::Unsupported
    );
    assert_eq!(
        error_code(&vfs.root_identity(Path::new("/race-parent")).unwrap_err()),
        ErrorCode::Unsupported
    );
    assert_eq!(
        error_code(&match vfs.open_read(Path::new("/race-parent/gleich.txt")) {
            Ok(_) => panic!("SFTP-OPEN akzeptierte den ausgetauschten Fremdpfad"),
            Err(error) => error,
        }),
        ErrorCode::Unsupported
    );
    assert_eq!(
        fs::read(race_outside.join("gleich.txt")).unwrap(),
        b"aussen\n"
    );
    std::os::unix::fs::symlink("../outside-read.txt", Path::new(&root).join("escape-read"))
        .unwrap();
    std::os::unix::fs::symlink(
        "../outside-read-dir",
        Path::new(&root).join("escape-parent"),
    )
    .unwrap();
    assert!(vfs.open_read(Path::new("/escape-read")).is_err());
    assert!(vfs
        .open_read(Path::new("/escape-parent/secret.txt"))
        .is_err());
    assert!(vfs.list_complete(Path::new("/escape-parent")).is_err());
    assert!(vfs.stat(Path::new("/escape-parent/secret.txt")).is_err());
    assert!(vfs
        .root_identity(Path::new("/escape-parent/nested"))
        .is_err());
    assert!(vfs
        .create_dir_all(Path::new("/escape-parent/create-dir-all-outside"))
        .is_err());
    assert!(
        !outside_dir.join("create-dir-all-outside").exists(),
        "create_dir_all folgte einem Zwischen-Symlink aus dem gebundenen Root"
    );
    assert_eq!(fs::read(&outside_file).unwrap(), b"ausserhalb\n");
    assert_eq!(
        fs::read(outside_dir.join("secret.txt")).unwrap(),
        b"ausserhalb-dir\n"
    );

    // Kein pfadbasiertes OPEN, SETSTAT oder RENAME darf die getrennte Elternprüfung
    // überleben. Das Sentinel liegt im künstlichen Besitzerbereich, aber bewusst
    // außerhalb des gebundenen Pane-Roots.
    assert!(vfs
        .open_write_new(Path::new("/escape-parent/upload-outside.txt"))
        .is_err());
    assert!(!outside_dir.join("upload-outside.txt").exists());
    let sentinel_mode = fs::metadata(&outside_mutation_sentinel)
        .unwrap()
        .permissions()
        .mode();
    assert!(vfs
        .set_mode(Path::new("/escape-parent/mutation-sentinel.txt"), 0o600)
        .is_err());
    assert_eq!(
        fs::metadata(&outside_mutation_sentinel)
            .unwrap()
            .permissions()
            .mode(),
        sentinel_mode
    );
    assert!(vfs
        .rename_plain_once(
            Path::new("/escape-parent/mutation-sentinel.txt"),
            Path::new("/escape-parent/mutation-renamed.txt"),
        )
        .is_err());
    assert_eq!(
        fs::read(&outside_mutation_sentinel).unwrap(),
        b"nicht mutieren\n"
    );
    assert!(!outside_dir.join("mutation-renamed.txt").exists());

    // Auch ein zu Beginn vorhandener Ordner ändert den Vertrag nicht: `create_dir`
    // sendet grundsätzlich kein pfadbasiertes MKDIR und lässt fremden Inhalt stehen.
    fs::create_dir(Path::new(&root).join("mkdir-kollision")).unwrap();
    fs::write(
        Path::new(&root).join("mkdir-kollision/fremder-sentinel"),
        b"fremd\n",
    )
    .unwrap();
    let mkdir_collision = vfs.create_dir(Path::new("/mkdir-kollision")).unwrap_err();
    assert_eq!(error_code(&mkdir_collision), ErrorCode::Unsupported);
    assert_eq!(
        fs::read(Path::new(&root).join("mkdir-kollision/fremder-sentinel")).unwrap(),
        b"fremd\n"
    );
    fs::remove_file(Path::new(&root).join("mkdir-kollision/fremder-sentinel")).unwrap();
    fs::remove_dir(Path::new(&root).join("mkdir-kollision")).unwrap();

    // Der Best-Effort-Abbau darf Zwischen-Symlinks ebenso wenig verfolgen wie
    // open_read. Beide Ziele liegen im test-eigenen Besitzerverzeichnis, aber
    // bewusst außerhalb des gebundenen SFTP-Serverroots.
    fs::write(outside_dir.join("move-victim.txt"), b"nicht entfernen\n").unwrap();
    fs::create_dir(outside_dir.join("move-empty-dir")).unwrap();
    assert!(vfs
        .remove_move_leaf_best_effort(Path::new("/escape-parent/move-victim.txt"))
        .is_err());
    assert!(vfs
        .remove_move_empty_dir_best_effort(Path::new("/escape-parent/move-empty-dir"))
        .is_err());
    assert_eq!(
        fs::read(outside_dir.join("move-victim.txt")).unwrap(),
        b"nicht entfernen\n"
    );
    assert!(outside_dir.join("move-empty-dir").is_dir());
    guarded.assert_server_root_unchanged();
    fs::remove_file(Path::new(&root).join("escape-read")).unwrap();
    fs::remove_file(Path::new(&root).join("escape-parent")).unwrap();
    fs::remove_file(&race_parent).unwrap();
    fs::remove_dir_all(&race_parked).unwrap();
    fs::remove_dir_all(&race_outside).unwrap();
    fs::remove_file(outside_dir.join("move-victim.txt")).unwrap();
    fs::remove_dir(outside_dir.join("move-empty-dir")).unwrap();
    fs::remove_file(&outside_mutation_sentinel).unwrap();
    fs::remove_file(outside_dir.join("secret.txt")).unwrap();
    fs::remove_dir(&outside_nested).unwrap();
    fs::remove_dir(outside_dir).unwrap();
    fs::remove_file(outside_file).unwrap();

    // Alle Capability-Abfragen bleiben lesend. SFTP v3 kann keine Pfadmutation bis
    // zur Wirkung an den Elternordner binden; starke und Best-Effort-Aussagen
    // bleiben deshalb gleichermaßen false.
    let delete_capabilities = vfs.probe_delete_capability().unwrap();
    assert!(!delete_capabilities.permits_entry_delete());
    let add_only_capabilities = vfs.probe_add_only_capabilities().unwrap();
    assert!(!add_only_capabilities.permits_add_only());
    let endpoint = ProviderEndpoint::sftp(vfs.clone(), Path::new("/")).unwrap();
    assert_eq!(
        error_code(&endpoint.probe_quarantine_restore_capability().unwrap_err()),
        ErrorCode::Unsupported
    );
    assert_eq!(
        error_code(&endpoint.probe_quarantine_purge_capability().unwrap_err()),
        ErrorCode::Unsupported
    );
    let file_capabilities = endpoint.probe_file_capabilities().unwrap();
    assert!(!file_capabilities.remove_source);
    assert!(!file_capabilities.remove_source_best_effort);
    assert!(!file_capabilities.copy_new);
    assert!(!file_capabilities.copy_new_best_effort);
    assert!(file_capabilities
        .limitations
        .iter()
        .any(|note| note.contains("SFTP-Ordneranlage ist gesperrt")));
    assert!(file_capabilities
        .blockers
        .iter()
        .any(|note| note.contains("SFTP-Dateidownload und Sync sind gesperrt")));
    assert_remote_root_empty(vfs.as_ref());

    let generation = vfs.session_generation();
    vfs.reconnect().unwrap();
    assert_eq!(vfs.session_generation(), generation + 1);
    assert!(!vfs.capabilities().permits_add_only());
    assert!(!vfs.capabilities().permits_entry_delete());
    assert!(!vfs.capabilities().permits_directory_create());
    let reconnected_listing = vfs.list_interactive_best_effort(Path::new("/")).unwrap();
    assert!(reconnected_listing.entries.is_empty());
    assert_eq!(reconnected_listing.session_generation, generation + 1);

    // Die Richtungsmatrix sperrt SFTP in beiden Wirkungsrichtungen. Nur das
    // ausdrücklich klassifizierte interaktive Pane-Listing bleibt verfügbar.
    let sftp_transfer = endpoint.current_transfer_capabilities();
    assert!(!sftp_transfer.add_only);
    assert!(!sftp_transfer.update);
    assert!(!sftp_transfer.mirror);

    // Auch Remote→lokal bleibt gesperrt: Ohne serverseitiges No-Follow kann OPEN
    // nach REALPATH auf eine gleichartig aussehende Fremddatei umgelenkt werden.
    // Das lokale Ziel muss beim Vorbereitungsfehler vollständig leer bleiben.
    guarded.assert_server_root_unchanged();
    fs::create_dir(Path::new(&root).join("remote-quelle")).unwrap();
    fs::write(
        Path::new(&root).join("remote-quelle/remote.txt"),
        b"remote eins\n",
    )
    .unwrap();
    let download_area = tempfile::tempdir().unwrap();
    let download_target = download_area.path().join("ziel");
    std::fs::create_dir(&download_target).unwrap();
    let download_error = match prepare_sftp_download(
        vfs.clone(),
        Path::new("/remote-quelle"),
        &download_target,
        SyncOptions::default(),
    ) {
        Ok(_) => panic!("SFTP-Download erhielt ohne gebundenes READDIR/OPEN einen Planner-Beleg"),
        Err(error) => error,
    };
    assert_eq!(error_code(&download_error), ErrorCode::Unsupported);
    assert!(std::fs::read_dir(&download_target)
        .unwrap()
        .next()
        .is_none());
    assert_eq!(
        fs::read(Path::new(&root).join("remote-quelle/remote.txt")).unwrap(),
        b"remote eins\n"
    );

    // Lokal→SFTP wird dagegen vor jeder Mutation mit einem maschinenlesbaren
    // Unsupported beendet. Weder Zielinhalt noch Capability-Probereste entstehen.
    guarded.assert_server_root_unchanged();
    fs::create_dir(Path::new(&root).join("remote-ziel")).unwrap();
    let upload_source = tempfile::tempdir().unwrap();
    std::fs::write(upload_source.path().join("upload.txt"), b"kein Upload\n").unwrap();
    let root_before_upload = remote_root_names(vfs.as_ref());
    let upload_error = match prepare_sftp_upload(
        upload_source.path(),
        vfs.clone(),
        Path::new("/remote-ziel"),
        SyncOptions::default(),
    ) {
        Ok(_) => panic!("SFTP-Ziel ohne starken AddOnly-Vertrag wurde freigegeben"),
        Err(error) => error,
    };
    assert_eq!(error_code(&upload_error), ErrorCode::Unsupported);
    assert_eq!(root_before_upload, remote_root_names(vfs.as_ref()));
    assert!(vfs
        .list_interactive_best_effort(Path::new("/remote-ziel"))
        .unwrap()
        .entries
        .is_empty());

    // Auch die Remote-Quarantäne muss vor ihrem ersten Probeobjekt abbrechen.
    let root_before_quarantine = remote_root_names(vfs.as_ref());
    let delete_capabilities = endpoint.probe_delete_capabilities().unwrap();
    assert!(!delete_capabilities.quarantine);
    assert!(!delete_capabilities.permanent);
    assert_eq!(root_before_quarantine, remote_root_names(vfs.as_ref()));
    assert!(!vfs.exists(Path::new("/.doppeldecker-quarantine")));

    // Derselbe negative Zielvertrag gilt im persistierten Upload-Queuepfad. Ein
    // Fehler bleibt terminal `Failed`; Temp-Journale und entfernte Reste fehlen.
    let queue_area = tempfile::tempdir().unwrap();
    let queue = OperationQueue::open(queue_area.path().join("queue.json")).unwrap();
    let queued = queue
        .enqueue(QueueOperation::SftpSync {
            direction: SftpQueueDirection::Upload,
            connection: Box::new(config.clone()),
            local_root: upload_source.path().to_string_lossy().into_owned(),
            remote_root: "/remote-ziel".into(),
            options: SyncOptions::default(),
            expected_plan: Box::new(SyncPlan::default()),
        })
        .unwrap();
    let queue_error = match prepare_sftp_queue_entry(
        &queue,
        queue.claim_next().unwrap().unwrap(),
        Arc::new(NoSecrets),
    ) {
        Ok(_) => panic!("SFTP-Upload-Queue bereitete ein unzulässiges Remote-Ziel vor"),
        Err(error) => error,
    };
    assert_eq!(error_code(&queue_error), ErrorCode::Unsupported);
    queue
        .fail(
            &queued.operation_id,
            Progress::default(),
            queue_error.to_string(),
        )
        .unwrap();
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Failed);
    assert!(std::fs::read_dir(queue_area.path()).unwrap().all(|entry| {
        let name = entry.unwrap().file_name();
        let name = name.to_string_lossy();
        !name.starts_with(".operations-queue-") || !name.ends_with(".tmp")
    }));
    assert!(vfs
        .list_interactive_best_effort(Path::new("/remote-ziel"))
        .unwrap()
        .entries
        .is_empty());

    // Eine bidirektionale Vorbereitung stoppt bereits an der fehlenden sicheren
    // Root-Identität und schreibt weder Baseline noch Recovery-Journal.
    guarded.assert_server_root_unchanged();
    fs::create_dir(Path::new(&root).join("bidirektional")).unwrap();
    let bidi_area = tempfile::tempdir().unwrap();
    let bidi_local = bidi_area.path().join("lokal");
    std::fs::create_dir(&bidi_local).unwrap();
    std::fs::write(bidi_local.join("lokal.txt"), b"lokal\n").unwrap();
    let bidi_baseline = bidi_area.path().join("state/pair.json");
    let bidirectional_error = match prepare_sftp_bidirectional_with_decisions(
        PairId([90; 16]),
        "SFTP-E2E-negativ",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::sftp(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
        &[],
    ) {
        Ok(_) => panic!("Bidirectional-Lauf erhielt eine ungebundene SFTP-Root-Identität"),
        Err(error) => error,
    };
    assert_eq!(error_code(&bidirectional_error), ErrorCode::Unsupported);
    assert_eq!(
        std::fs::read(bidi_local.join("lokal.txt")).unwrap(),
        b"lokal\n"
    );
    assert!(vfs
        .list_interactive_best_effort(Path::new("/bidirektional"))
        .unwrap()
        .entries
        .is_empty());
    assert!(!bidi_baseline.exists());
    assert!(!bidi_baseline.with_extension("run.json").exists());
    let rollback_error = match prepare_sftp_bidirectional_rollback(
        "SFTP-E2E-negativ",
        ProviderEndpoint::local(&bidi_local).unwrap(),
        ProviderEndpoint::sftp(vfs.clone(), Path::new("/bidirektional")).unwrap(),
        &bidi_baseline,
    ) {
        Ok(_) => panic!("Mutationslos blockierter Lauf hinterließ ein Rollback-Journal"),
        Err(error) => error,
    };
    assert_eq!(
        error_code(&rollback_error),
        ErrorCode::NotFound,
        "Ohne jemals erzeugtes Laufjournal gibt es keinen Rollbackzustand; Unsupported gilt erst für ein vorhandenes, aber ungebundenes SFTP-Journal"
    );

    // Der Provider selbst ist absichtlich read-only. Der Runner baut deshalb seine
    // ausschließlich unter dem frischen künstlichen Serverroot erzeugten Fixtures
    // direkt auf der lokalen Testseite wieder ab.
    guarded.assert_server_root_unchanged();
    fs::remove_dir_all(Path::new(&root).join("remote-quelle")).unwrap();
    fs::remove_dir_all(Path::new(&root).join("remote-ziel")).unwrap();
    fs::remove_dir_all(Path::new(&root).join("bidirektional")).unwrap();
    assert_remote_root_empty(vfs.as_ref());
}

fn remote_root_names(vfs: &SftpVfs) -> Vec<String> {
    let listing = vfs.list_interactive_best_effort(Path::new("/")).unwrap();
    let mut names = listing
        .entries
        .into_iter()
        .map(|entry| entry.entry.name)
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn assert_remote_root_empty(vfs: &SftpVfs) {
    assert_eq!(
        remote_root_names(vfs),
        Vec::<String>::new(),
        "SFTP-Capability-Probe hinterließ Einträge im künstlichen Serverroot"
    );
}

struct GuardedRunnerRoot {
    host: String,
    root: String,
    canonical_root: String,
    server_device: u64,
    server_inode: u64,
    server_uid: u64,
    server_mode: u64,
    server_mount_id: String,
}

impl GuardedRunnerRoot {
    fn assert_server_root_unchanged(&self) {
        let metadata = fs::symlink_metadata(&self.root).unwrap();
        assert!(!metadata.file_type().is_symlink() && metadata.is_dir());
        assert_eq!(metadata.dev(), self.server_device);
        assert_eq!(metadata.ino(), self.server_inode);
        assert_eq!(u64::from(metadata.uid()), self.server_uid);
        assert_eq!(
            u64::from(metadata.permissions().mode() & 0o7777),
            self.server_mode
        );
        assert_eq!(
            mount_identity(Path::new(&self.root)).unwrap(),
            self.server_mount_id
        );
    }
}

struct RunnerManifestExpectation<'a> {
    host: &'a str,
    port: u16,
    root: &'a str,
    canonical_root: &'a str,
    owner_name: Option<&'a str>,
    host_key_algorithm: &'a str,
    host_key_sha256_hex: &'a str,
    owner_device: u64,
    owner_inode: u64,
    owner_uid: u64,
    owner_mode: u64,
    owner_mount_id: &'a str,
    server_device: u64,
    server_inode: u64,
    server_uid: u64,
    server_mode: u64,
    server_mount_id: &'a str,
}

fn validate_runner_manifest(
    manifest: &serde_json::Value,
    expected: &RunnerManifestExpectation<'_>,
) -> std::io::Result<()> {
    if manifest["provider"]["kind"].as_str() != Some("sftp")
        || manifest["provider"]["host"].as_str() != Some(expected.host)
        || manifest["provider"]["port"].as_u64() != Some(u64::from(expected.port))
        || manifest["provider"]["host_key_algorithm"].as_str() != Some(expected.host_key_algorithm)
        || manifest["provider"]["host_key_sha256"].as_str() != Some(expected.host_key_sha256_hex)
        || manifest["virtual_root"].as_str() != Some(expected.root)
        || manifest["canonical_virtual_root"].as_str() != Some(expected.canonical_root)
        || manifest["ownership_marker"].as_str() != expected.owner_name
        || manifest["owner_root"]["device"].as_u64() != Some(expected.owner_device)
        || manifest["owner_root"]["inode"].as_u64() != Some(expected.owner_inode)
        || manifest["owner_root"]["uid"].as_u64() != Some(expected.owner_uid)
        || manifest["owner_root"]["mode"].as_u64() != Some(expected.owner_mode)
        || manifest["owner_root"]["mount_id"].as_str() != Some(expected.owner_mount_id)
        || manifest["server_root"]["device"].as_u64() != Some(expected.server_device)
        || manifest["server_root"]["inode"].as_u64() != Some(expected.server_inode)
        || manifest["server_root"]["uid"].as_u64() != Some(expected.server_uid)
        || manifest["server_root"]["mode"].as_u64() != Some(expected.server_mode)
        || manifest["server_root"]["mount_id"].as_str() != Some(expected.server_mount_id)
    {
        return Err(permission_denied(
            "SFTP-E2E-Manifest stimmt nicht mit der tatsächlich verwendeten Verbindung überein",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_identity(path: &Path) -> std::io::Result<String> {
    let directory = File::open(path)?;
    let fdinfo = fs::read_to_string(format!("/proc/self/fdinfo/{}", directory.as_raw_fd()))?;
    let id = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:").map(str::trim))
        .ok_or_else(|| permission_denied("Linux lieferte keine Mount-ID"))?;
    Ok(format!("linux:{id}"))
}

#[cfg(target_os = "macos")]
fn mount_identity(path: &Path) -> std::io::Result<String> {
    let directory = File::open(path)?;
    let mut information = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `directory` bleibt während `fstatfs` offen; der Kernel initialisiert
    // bei Erfolg die vollständige `statfs`-Struktur.
    let status = unsafe { libc::fstatfs(directory.as_raw_fd(), information.as_mut_ptr()) };
    if status != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: Der erfolgreiche Aufruf oben hat die Struktur initialisiert und
    // `f_mntonname` ist laut Darwin-ABI nullterminiert.
    let information = unsafe { information.assume_init() };
    // SAFETY: Darwin definiert `fsid_t` ABI-stabil als zwei direkt aufeinander
    // folgende `i32`; `read_unaligned` stellt keine zusätzliche Ausrichtung voraus.
    let fsid = unsafe {
        std::ptr::read_unaligned(std::ptr::addr_of!(information.f_fsid).cast::<[libc::c_int; 2]>())
    };
    let mount_point = unsafe { CStr::from_ptr(information.f_mntonname.as_ptr()) }
        .to_str()
        .map_err(|_| permission_denied("Mountpunkt ist nicht als UTF-8 darstellbar"))?;
    Ok(format!("darwin:{}:{}:{mount_point}", fsid[0], fsid[1]))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn mount_identity(_path: &Path) -> std::io::Result<String> {
    Err(permission_denied(
        "Diese Plattform bietet keine sichere Mount-ID-Prüfung",
    ))
}

fn permission_denied(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, message.into())
}

fn validate_runner_root_lexical(host: &str, root: &str) -> std::io::Result<PathBuf> {
    let address: IpAddr = host
        .parse()
        .map_err(|_| permission_denied("SFTP-E2E-Host ist keine IP-Adresse"))?;
    if !address.is_loopback() {
        return Err(permission_denied("SFTP-E2E-Host ist nicht Loopback"));
    }
    if root.is_empty() || root.contains(['\n', '\r', '\0']) {
        return Err(permission_denied("SFTP-E2E-Root ist leer oder mehrzeilig"));
    }
    let server_root = Path::new(root);
    if !server_root.is_absolute()
        || server_root
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
        || server_root.file_name() != Some(std::ffi::OsStr::new("root"))
    {
        return Err(permission_denied(
            "SFTP-E2E-Serverroot ist nicht absolut und normalisiert",
        ));
    }
    let owner_root = server_root
        .parent()
        .ok_or_else(|| permission_denied("SFTP-E2E-Root besitzt keinen Eigentümerroot"))?;
    if owner_root.parent() != Some(Path::new("/tmp"))
        && owner_root.parent() != Some(Path::new("/private/tmp"))
    {
        return Err(permission_denied(
            "SFTP-E2E-Eigentümerroot liegt nicht direkt unter /tmp",
        ));
    }
    let owner_name = owner_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| permission_denied("SFTP-E2E-Eigentümername ist nicht sicher dekodierbar"))?;
    let suffix = owner_name
        .strip_prefix("doppeldecker-sftp-e2e.")
        .ok_or_else(|| permission_denied("SFTP-E2E-Eigentümerroot hat den falschen Namen"))?;
    if suffix.is_empty() {
        return Err(permission_denied(
            "SFTP-E2E-Eigentümerroot besitzt kein zufälliges Suffix",
        ));
    }
    let parent_text = if owner_root.parent() == Some(Path::new("/tmp")) {
        "/tmp"
    } else {
        "/private/tmp"
    };
    if root != format!("{parent_text}/{owner_name}/root") {
        return Err(permission_denied(
            "SFTP-E2E-Serverroot enthält redundante oder nicht normalisierte Pfadteile",
        ));
    }
    Ok(owner_root.to_path_buf())
}

fn required_u64(name: &str) -> std::io::Result<u64> {
    required(name)
        .parse()
        .map_err(|_| permission_denied(format!("{name} ist keine gültige Besitzangabe")))
}

fn validate_server_root_binding(
    path: &Path,
    expected_device: u64,
    expected_inode: u64,
    expected_uid: u64,
    expected_mode: u64,
    expected_mount_id: &str,
) -> std::io::Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.dev() != expected_device
        || metadata.ino() != expected_inode
        || u64::from(metadata.uid()) != expected_uid
        || u64::from(metadata.permissions().mode() & 0o7777) != expected_mode
        || mount_identity(path)? != expected_mount_id
    {
        return Err(permission_denied(
            "SFTP-E2E-Serverroot stimmt nicht mit der unmittelbaren Runner-Bindung überein",
        ));
    }
    let canonical = fs::canonicalize(path)?;
    let canonical_metadata = fs::symlink_metadata(&canonical)?;
    if canonical_metadata.dev() != expected_device || canonical_metadata.ino() != expected_inode {
        return Err(permission_denied(
            "SFTP-E2E-Serverroot wechselte bei der Kanonisierung seine Identität",
        ));
    }
    Ok(canonical)
}

fn guarded_runner_root() -> std::io::Result<GuardedRunnerRoot> {
    let host = required("DOPPELDECKER_SFTP_TEST_HOST");
    let port = required("DOPPELDECKER_SFTP_TEST_PORT")
        .parse::<u16>()
        .map_err(|_| permission_denied("SFTP-E2E-Port ist ungültig"))?;
    let root = required("DOPPELDECKER_SFTP_TEST_ROOT");
    let host_key_algorithm = required("DOPPELDECKER_SFTP_TEST_HOST_KEY_ALGORITHM");
    let host_key_sha256_hex = required("DOPPELDECKER_SFTP_TEST_HOST_KEY_SHA256_HEX");
    let owner_root = validate_runner_root_lexical(&host, &root)?;
    let expected_device = required_u64("DOPPELDECKER_SFTP_TEST_OWNER_DEVICE")?;
    let expected_inode = required_u64("DOPPELDECKER_SFTP_TEST_OWNER_INODE")?;
    let expected_uid = required_u64("DOPPELDECKER_SFTP_TEST_OWNER_UID")?;
    let expected_mode = required_u64("DOPPELDECKER_SFTP_TEST_OWNER_MODE")?;
    let expected_mount_id = required("DOPPELDECKER_SFTP_TEST_OWNER_MOUNT_ID");
    let server_device = required_u64("DOPPELDECKER_SFTP_TEST_SERVER_DEVICE")?;
    let server_inode = required_u64("DOPPELDECKER_SFTP_TEST_SERVER_INODE")?;
    let server_uid = required_u64("DOPPELDECKER_SFTP_TEST_SERVER_UID")?;
    let server_mode = required_u64("DOPPELDECKER_SFTP_TEST_SERVER_MODE")?;
    let server_mount_id = required("DOPPELDECKER_SFTP_TEST_SERVER_MOUNT_ID");

    // Symlink-Metadaten werden vor jeder Kanonisierung geprüft. Andernfalls könnte
    // ein passend benannter Link still zu einem fremden Baum normalisiert werden.
    let owner_metadata = fs::symlink_metadata(&owner_root)?;
    if owner_metadata.file_type().is_symlink() || !owner_metadata.is_dir() {
        return Err(permission_denied(
            "SFTP-E2E-Eigentümerroot ist kein echtes Verzeichnis",
        ));
    }
    if owner_metadata.dev() != expected_device
        || owner_metadata.ino() != expected_inode
        || u64::from(owner_metadata.uid()) != expected_uid
        || u64::from(owner_metadata.permissions().mode() & 0o7777) != expected_mode
    {
        return Err(permission_denied(
            "SFTP-E2E-Eigentümerroot stimmt nicht mit der Runner-Bindung überein",
        ));
    }
    let canonical_owner = fs::canonicalize(&owner_root)?;
    // Auf macOS löst dies zugleich `/private/tmp` auf; Linux benötigt diesen
    // dort normalerweise fehlenden macOS-Pfad nicht als zusätzliche Voraussetzung.
    let allowed_parent = fs::canonicalize("/tmp")?;
    if canonical_owner.parent() != Some(allowed_parent.as_path()) {
        return Err(permission_denied(
            "SFTP-E2E-Eigentümerroot verließ bei der Kanonisierung /tmp",
        ));
    }
    let canonical_owner_metadata = fs::symlink_metadata(&canonical_owner)?;
    if canonical_owner_metadata.dev() != expected_device
        || canonical_owner_metadata.ino() != expected_inode
    {
        return Err(permission_denied(
            "SFTP-E2E-Eigentümerroot wechselte bei der Kanonisierung seine Identität",
        ));
    }
    if mount_identity(&owner_root)? != expected_mount_id {
        return Err(permission_denied(
            "SFTP-E2E-Eigentümerroot wechselte seine Mount-ID",
        ));
    }

    let canonical_root = validate_server_root_binding(
        Path::new(&root),
        server_device,
        server_inode,
        server_uid,
        server_mode,
        &server_mount_id,
    )?;
    if server_mount_id != expected_mount_id {
        return Err(permission_denied(
            "SFTP-E2E-Serverroot liegt nicht auf der Mount-ID seines Eigentümerroots",
        ));
    }
    if canonical_root.parent() != Some(canonical_owner.as_path())
        || canonical_root.file_name() != Some(std::ffi::OsStr::new("root"))
    {
        return Err(permission_denied(
            "SFTP-E2E-Serverroot gehört nicht direkt zum gebundenen Eigentümerroot",
        ));
    }

    let manifest_path = owner_root.join("manifest.json");
    let manifest_metadata = fs::symlink_metadata(&manifest_path)?;
    if manifest_metadata.file_type().is_symlink() || !manifest_metadata.is_file() {
        return Err(permission_denied(
            "SFTP-E2E-Manifest ist keine echte reguläre Datei",
        ));
    }
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(manifest_path)?)
        .map_err(|error| permission_denied(format!("SFTP-E2E-Manifest ist ungültig: {error}")))?;
    let owner_name = owner_root.file_name().and_then(|name| name.to_str());
    let canonical_root_text = canonical_root.to_string_lossy().into_owned();
    validate_runner_manifest(
        &manifest,
        &RunnerManifestExpectation {
            host: &host,
            port,
            root: &root,
            canonical_root: &canonical_root_text,
            owner_name,
            host_key_algorithm: &host_key_algorithm,
            host_key_sha256_hex: &host_key_sha256_hex,
            owner_device: expected_device,
            owner_inode: expected_inode,
            owner_uid: expected_uid,
            owner_mode: expected_mode,
            owner_mount_id: &expected_mount_id,
            server_device,
            server_inode,
            server_uid,
            server_mode,
            server_mount_id: &server_mount_id,
        },
    )?;

    Ok(GuardedRunnerRoot {
        host,
        root,
        canonical_root: canonical_root_text,
        server_device,
        server_inode,
        server_uid,
        server_mode,
        server_mount_id,
    })
}

#[test]
fn runner_root_guard_verwirft_prefix_traversal_und_nicht_loopback() {
    assert!(
        validate_runner_root_lexical("127.0.0.1", "/tmp/doppeldecker-sftp-e2e.sicher/root").is_ok()
    );
    assert!(validate_runner_root_lexical(
        "127.0.0.1",
        "/tmp/doppeldecker-sftp-e2e.sicher/../fremd/root"
    )
    .is_err());
    assert!(
        validate_runner_root_lexical("203.0.113.7", "/tmp/doppeldecker-sftp-e2e.sicher/root")
            .is_err()
    );
}

#[test]
fn serverroot_guard_verwirft_austausch_und_mountwechsel() {
    let owner = tempfile::tempdir().unwrap();
    let server = owner.path().join("root");
    fs::create_dir(&server).unwrap();
    fs::set_permissions(&server, fs::Permissions::from_mode(0o700)).unwrap();
    let metadata = fs::symlink_metadata(&server).unwrap();
    let mount_id = mount_identity(&server).unwrap();
    assert!(validate_server_root_binding(
        &server,
        metadata.dev(),
        metadata.ino(),
        u64::from(metadata.uid()),
        u64::from(metadata.permissions().mode() & 0o7777),
        &mount_id,
    )
    .is_ok());
    assert!(validate_server_root_binding(
        &server,
        metadata.dev(),
        metadata.ino(),
        u64::from(metadata.uid()),
        u64::from(metadata.permissions().mode() & 0o7777),
        "simulierter-fremder-mount",
    )
    .is_err());

    fs::rename(&server, owner.path().join("urspruenglich")).unwrap();
    fs::create_dir(&server).unwrap();
    fs::set_permissions(&server, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(validate_server_root_binding(
        &server,
        metadata.dev(),
        metadata.ino(),
        u64::from(metadata.uid()),
        u64::from(metadata.permissions().mode() & 0o7777),
        &mount_id,
    )
    .is_err());
}

#[test]
fn runner_manifest_bindet_die_tatsaechliche_sftp_verbindung() {
    let fingerprint = "ab".repeat(32);
    let expected = RunnerManifestExpectation {
        host: "127.0.0.1",
        port: 22022,
        root: "/tmp/doppeldecker-sftp-e2e.test/root",
        canonical_root: "/private/tmp/doppeldecker-sftp-e2e.test/root",
        owner_name: Some("doppeldecker-sftp-e2e.test"),
        host_key_algorithm: "ssh-ed25519",
        host_key_sha256_hex: &fingerprint,
        owner_device: 1,
        owner_inode: 2,
        owner_uid: 501,
        owner_mode: 0o700,
        owner_mount_id: "darwin:1:2:/private/tmp",
        server_device: 1,
        server_inode: 3,
        server_uid: 501,
        server_mode: 0o700,
        server_mount_id: "darwin:1:2:/private/tmp",
    };
    let valid = serde_json::json!({
        "provider": {
            "kind": "sftp",
            "host": expected.host,
            "port": expected.port,
            "host_key_algorithm": expected.host_key_algorithm,
            "host_key_sha256": expected.host_key_sha256_hex,
        },
        "virtual_root": expected.root,
        "canonical_virtual_root": expected.canonical_root,
        "ownership_marker": expected.owner_name,
        "owner_root": {
            "device": expected.owner_device,
            "inode": expected.owner_inode,
            "uid": expected.owner_uid,
            "mode": expected.owner_mode,
            "mount_id": expected.owner_mount_id,
        }
    });
    assert!(
        validate_runner_manifest(&valid, &expected).is_err(),
        "Ein Manifest ohne unmittelbar gebundene Serverroot-Identität darf keine Mutation erlauben"
    );
    let mut valid = valid;
    valid["server_root"] = serde_json::json!({
        "device": expected.server_device,
        "inode": expected.server_inode,
        "uid": expected.server_uid,
        "mode": expected.server_mode,
        "mount_id": expected.server_mount_id,
    });
    validate_runner_manifest(&valid, &expected).unwrap();
    for (field, replacement) in [
        ("kind", serde_json::json!("webdav")),
        ("port", serde_json::json!(22023)),
        ("host_key_algorithm", serde_json::json!("ssh-rsa")),
        ("host_key_sha256", serde_json::json!("00")),
    ] {
        let mut changed = valid.clone();
        changed["provider"][field] = replacement;
        assert!(
            validate_runner_manifest(&changed, &expected).is_err(),
            "{field}"
        );
    }
    let mut changed_root = valid;
    changed_root["canonical_virtual_root"] = serde_json::json!("/private/tmp/fremd/root");
    assert!(validate_runner_manifest(&changed_root, &expected).is_err());
}

fn required(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} fehlt"))
}

fn digest(name: &str) -> [u8; 32] {
    let value = required(name);
    assert_eq!(value.len(), 64, "{name} muss 32 Byte als Hex enthalten");
    let mut result = [0_u8; 32];
    for (index, byte) in result.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).unwrap();
    }
    result
}
