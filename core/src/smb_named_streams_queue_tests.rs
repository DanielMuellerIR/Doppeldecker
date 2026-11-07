//! Echter ContentSync über die Fensterqueue gegen einen privaten Samba-Share.
use super::*;
use crate::{
    execute_prepared_content_sync_queue_entry, prepare_content_sync_for_queue, Action, Cancel,
    Compare, ContentSyncPreview, KeychainSecretReference, MetadataClass, MetadataProfile,
    NamedBlobSummary, OperationQueue, ProviderEndpoint, QueueExecutionResult, QueueState,
    SecretString, SyncMode, SyncOptions, SyncStats,
};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const TEST_PASSWORD: &str = "Doppeldecker-SMB-E2E-wertlos-2026";
static SEED_SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct TestSecrets;
impl SecretResolver for TestSecrets {
    fn resolve(&self, _: &KeychainSecretReference) -> io::Result<SecretString> {
        SecretString::new(TEST_PASSWORD.into())
    }
}

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} fehlt für isolierten Gegenlauf"))
}

fn fixture() -> (Arc<SmbVfs>, PathBuf) {
    let root = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(root.starts_with("/tmp/doppeldecker-smb-stream-probe-20261029"));
    assert_eq!(root.file_name().unwrap(), "share");
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    let config = SmbConnectionConfig {
        server_guid: discover_smb_server(&host, port).unwrap().server_guid,
        host,
        port,
        share: required("DOPPELDECKER_SMB_TEST_SHARE"),
        username: required("DOPPELDECKER_SMB_TEST_USER"),
        domain: String::new(),
        requested_root: "/".into(),
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "smb").unwrap(),
    };
    (
        Arc::new(SmbVfs::connect(config, Arc::new(TestSecrets)).unwrap()),
        root,
    )
}

fn seed(provider: &Arc<SmbVfs>, path: &Path, streams: &[(&str, &[u8])], replace: bool) {
    let context = provider
        .content_sync_mutations(path.parent().unwrap(), &Cancel::new(), None)
        .unwrap();
    let scoped = context.staging_scope(path, None).unwrap().unwrap();
    let temp = path.parent().unwrap().join(format!(
        ".seed-{}",
        SEED_SEQUENCE.fetch_add(1, Ordering::SeqCst)
    ));
    let mut main = scoped.open_write_new(&temp).unwrap();
    main.write_all(b"main").unwrap();
    main.flush().unwrap();
    drop(main);
    for (name, data) in streams {
        let mut writer = scoped
            .open_named_stream_write_new(&temp, name.as_bytes())
            .unwrap();
        writer.write_all(data).unwrap();
        writer.flush().unwrap();
        drop(writer);
    }
    scoped.set_mtime(&temp, 1_700_000_000).unwrap();
    scoped.sync_file(&temp).unwrap();
    if replace {
        scoped.rename_replace_plain_once(&temp, path).unwrap();
    } else {
        scoped.rename_noreplace(&temp, path).unwrap();
    }
    scoped.finish_staging().unwrap();
}

fn observed(provider: &Arc<SmbVfs>, path: &Path) -> Vec<NamedBlobSummary> {
    provider
        .content_sync_mutations(path.parent().unwrap(), &Cancel::new(), None)
        .unwrap()
        .metadata_manifest(path)
        .unwrap()
        .named_streams
        .expect("Unterstützter regulärer SMB-Eintrag braucht beobachtete Strommenge")
}

fn options(mode: SyncMode) -> SyncOptions {
    SyncOptions {
        mode,
        compare: Compare::ContentHash,
        metadata: MetadataProfile::NativeBestEffort,
        ..SyncOptions::default()
    }
}

fn run(
    queue: &OperationQueue,
    source: ProviderEndpoint,
    target: ProviderEndpoint,
    mode: SyncMode,
    audit: &Path,
) -> (SyncStats, ContentSyncPreview) {
    let prepared =
        prepare_content_sync_for_queue(queue, source, target, options(mode), &Cancel::new())
            .unwrap();
    let preview = prepared.preview().clone();
    queue.enqueue(prepared.queue_operation()).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let result = execute_prepared_content_sync_queue_entry(
        queue,
        &entry,
        prepared,
        true,
        audit,
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        queue.snapshot().entries.last().unwrap().state,
        QueueState::Completed
    );
    let QueueExecutionResult::Sync(stats) = result else {
        panic!("Syncbilanz fehlt")
    };
    (stats, preview)
}

fn assert_windows_omitted(report: &crate::MetadataReport, classes: &[MetadataClass]) {
    for expected in classes {
        assert!(
            report
                .issues
                .iter()
                .any(|issue| { issue.path == "Datei.bin" && issue.class == *expected }),
            "Windows-Auslassung {expected:?} fehlt: {report:?}"
        );
        assert!(
            report.classes.iter().any(|class| {
                class.class == *expected && class.transferred == 0 && class.omitted >= 1
            }),
            "Windows-Klassenbilanz {expected:?} fehlt: {report:?}"
        );
    }
}

#[test]
#[ignore = "benötigt isolierten Samba-Share mit streams_xattr"]
fn named_streams_actual_content_queue_roundtrip() {
    let (provider, root) = fixture();
    assert!(!root.join("named-queue").exists());
    std::fs::create_dir_all(root.join("named-queue/source")).unwrap();
    std::fs::create_dir(root.join("named-queue/target")).unwrap();
    let source_path = Path::new("/named-queue/source/Datei.bin");
    let target_path = Path::new("/named-queue/target/Datei.bin");
    seed(
        &provider,
        source_path,
        &[("Änderung", b"eins"), ("leer", b"")],
        false,
    );
    let source = || ProviderEndpoint::smb(provider.clone(), source_path.parent().unwrap()).unwrap();
    let target = || ProviderEndpoint::smb(provider.clone(), target_path.parent().unwrap()).unwrap();
    let config = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let config_root = config.path().canonicalize().unwrap();
    let queue = OperationQueue::open(config_root.join("queue.json")).unwrap();
    let audit = config_root.join("audit.jsonl");
    let (first, first_preview) = run(&queue, source(), target(), SyncMode::AddOnly, &audit);
    assert_eq!(first.bytes_copied, 8);
    assert_eq!(first_preview.plan.copy_count, 1);
    for report in [&first_preview.plan.metadata_report, &first.metadata_report] {
        assert_windows_omitted(report, &[MetadataClass::Acl, MetadataClass::Owner]);
    }
    assert_eq!(
        observed(&provider, source_path),
        observed(&provider, target_path)
    );
    assert!(first.metadata_report.classes.iter().any(|class| {
        class.class == MetadataClass::NamedStreams && class.transferred == 1 && class.omitted == 0
    }));

    seed(
        &provider,
        source_path,
        &[("neu", b"zwei"), ("leer", b"")],
        true,
    );
    let (updated, preview) = run(&queue, source(), target(), SyncMode::Update, &audit);
    assert!(preview.plan.actions.iter().any(|action| matches!(
        action,
        Action::UpdateFile {
            metadata_only: true,
            ..
        }
    )));
    assert_eq!(updated.bytes_copied, 8);
    assert_eq!(
        observed(&provider, source_path),
        observed(&provider, target_path)
    );
    // Nur die künstliche Zielbasis bekommt andere beobachtete Metadaten.
    // Eine bereits vorhandene Supplementärgruppe benötigt keine Kontoänderung.
    let target_physical = root.join("named-queue/target/Datei.bin");
    std::fs::set_permissions(&target_physical, std::fs::Permissions::from_mode(0o600)).unwrap();
    let other_gid = std::env::var("DOPPELDECKER_SMB_TEST_OTHER_GID")
        .ok()
        .map(|value| value.parse::<u32>().unwrap());
    if let Some(gid) = other_gid {
        assert_ne!(std::fs::metadata(&target_physical).unwrap().gid(), gid);
        std::os::unix::fs::chown(&target_physical, None, Some(gid)).unwrap();
    }
    let (noop, noop_preview) = run(&queue, source(), target(), SyncMode::Update, &audit);
    for report in [&noop_preview.plan.metadata_report, &noop.metadata_report] {
        assert_windows_omitted(report, &[MetadataClass::Acl]);
        if other_gid.is_some() {
            assert_windows_omitted(report, &[MetadataClass::Owner]);
        }
    }
    assert_eq!(
        (noop.files_copied, noop.bytes_copied, noop.metadata_updated),
        (0, 0, 0)
    );

    seed(&provider, source_path, &[], true);
    let (cleared, preview) = run(&queue, source(), target(), SyncMode::Update, &audit);
    assert_eq!(preview.plan.update_count, 1);
    assert_eq!(cleared.bytes_copied, 4);
    assert!(observed(&provider, target_path).is_empty());
    let (noop, _) = run(&queue, source(), target(), SyncMode::Update, &audit);
    assert_eq!(noop.bytes_copied, 0);

    seed(
        &provider,
        source_path,
        &[("lokal-nicht-darstellbar", b"metadata")],
        true,
    );
    let local = tempfile::tempdir().unwrap();
    let local_root = local.path().canonicalize().unwrap();
    let (downloaded, _) = run(
        &queue,
        source(),
        ProviderEndpoint::local(&local_root).unwrap(),
        SyncMode::AddOnly,
        &audit,
    );
    assert_eq!(downloaded.bytes_copied, 4);
    assert_eq!(
        std::fs::read(local_root.join("Datei.bin")).unwrap(),
        b"main"
    );
    assert!(downloaded
        .metadata_report
        .issues
        .iter()
        .any(|issue| { issue.path == "Datei.bin" && issue.class == MetadataClass::NamedStreams }));
    let (local_noop, local_noop_preview) = run(
        &queue,
        source(),
        ProviderEndpoint::local(&local_root).unwrap(),
        SyncMode::Update,
        &audit,
    );
    assert_eq!(
        (
            local_noop_preview.plan.copy_count,
            local_noop_preview.plan.update_count,
            local_noop_preview.plan.metadata_count,
        ),
        (0, 0, 0)
    );
    assert_eq!(
        (
            local_noop.files_copied,
            local_noop.bytes_copied,
            local_noop.metadata_updated,
        ),
        (0, 0, 0)
    );
    // Gleicher Hauptinhalt beseitigt die lokale Darstellungsgrenze nicht.
    for report in [
        &local_noop_preview.plan.metadata_report,
        &local_noop.metadata_report,
    ] {
        assert!(report.issues.iter().any(|issue| {
            issue.path == "Datei.bin" && issue.class == MetadataClass::NamedStreams
        }));
        assert!(report.classes.iter().any(|class| {
            class.class == MetadataClass::NamedStreams
                && class.transferred == 0
                && class.omitted == 1
        }));
    }
    assert_eq!(
        std::fs::read(local_root.join("Datei.bin")).unwrap(),
        b"main"
    );
    assert_eq!(queue.snapshot().entries.len(), 7);
    assert!(queue
        .snapshot()
        .entries
        .iter()
        .all(|entry| { entry.state == QueueState::Completed && entry.attempts == 1 }));

    let old_target = observed(&provider, target_path);
    let prepared = prepare_content_sync_for_queue(
        &queue,
        source(),
        target(),
        options(SyncMode::Update),
        &Cancel::new(),
    )
    .unwrap();
    queue.enqueue(prepared.queue_operation()).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    // Nur die benannten Ströme ändern sich; Hauptinhalt und mtime bleiben gleich.
    // Auch diese Drift muss den echten Queueauftrag vor dem Basistausch stoppen.
    seed(
        &provider,
        source_path,
        &[("nach-bestaetigung", b"drift")],
        true,
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
    assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
    assert_eq!(observed(&provider, target_path), old_target);
    // Die frühe Vorbereitungsprüfung liegt vor mark_running; wie der aufrufende
    // Queueworker persistiert der Test den zurückgegebenen Validierungsfehler.
    assert_eq!(
        queue.snapshot().entries.last().unwrap().state,
        QueueState::Validating
    );
    queue
        .fail_coded(&entry.operation_id, crate::Progress::default(), &error)
        .unwrap();
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.entries.len(), 8);
    assert_eq!(snapshot.entries.last().unwrap().state, QueueState::Failed);
    assert_eq!(snapshot.entries.last().unwrap().attempts, 1);
    let target_before_acl_drift = observed(&provider, target_path);
    let target_before_acl_manifest = provider.metadata_manifest(target_path).unwrap();
    let target_before_acl_id = provider.stat(target_path).unwrap().inode;
    let prepared = prepare_content_sync_for_queue(
        &queue,
        source(),
        target(),
        options(SyncMode::Update),
        &Cancel::new(),
    )
    .unwrap();
    queue.enqueue(prepared.queue_operation()).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let source_physical = root.join("named-queue/source/Datei.bin");
    let before = provider.metadata_manifest(source_path).unwrap();
    let mtime = std::fs::metadata(&source_physical)
        .unwrap()
        .modified()
        .unwrap();
    let mode = std::fs::metadata(&source_physical)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    std::fs::set_permissions(
        &source_physical,
        std::fs::Permissions::from_mode(mode ^ 0o004),
    )
    .unwrap();
    let after = provider.metadata_manifest(source_path).unwrap();
    assert_ne!(before.acl, after.acl);
    assert_ne!(before.digest, after.digest);
    assert_eq!(before.named_streams, after.named_streams);
    assert_eq!(before.windows_security, after.windows_security);
    assert_eq!(
        std::fs::metadata(&source_physical)
            .unwrap()
            .modified()
            .unwrap(),
        mtime
    );
    assert_eq!(std::fs::read(&source_physical).unwrap(), b"main");
    let mut transferred = (0, 0);
    let error = execute_prepared_content_sync_queue_entry(
        &queue,
        &entry,
        prepared,
        true,
        &audit,
        &Cancel::new(),
        &mut |progress| {
            transferred.0 = transferred.0.max(progress.bytes_done);
            transferred.1 = transferred.1.max(progress.files_done);
        },
    )
    .unwrap_err();
    assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
    assert_eq!(transferred, (0, 0));
    assert_eq!(
        provider.stat(target_path).unwrap().inode,
        target_before_acl_id
    );
    assert_eq!(
        provider.metadata_manifest(target_path).unwrap().digest,
        target_before_acl_manifest.digest
    );
    // Die manifestgebundene Vorprüfung erkennt diese Drift jetzt bereits vor
    // mark_running. Den Fehler wie der aufrufende Fensterworker persistieren.
    assert_eq!(
        queue.snapshot().entries.last().unwrap().state,
        QueueState::Validating
    );
    queue
        .fail_coded(&entry.operation_id, crate::Progress::default(), &error)
        .unwrap();
    assert_eq!(
        queue.snapshot().entries.last().unwrap().state,
        QueueState::Failed
    );
    assert_eq!(observed(&provider, target_path), target_before_acl_drift);

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.entries.len(), 9);
    assert_eq!(snapshot.entries.last().unwrap().state, QueueState::Failed);
    assert_eq!(snapshot.entries.last().unwrap().attempts, 1);
    assert_eq!(snapshot.entries.last().unwrap().progress.bytes_done, 0);
    assert_eq!(snapshot.entries.last().unwrap().progress.files_done, 0);
    let serialized = std::fs::read_to_string(config_root.join("queue.json")).unwrap();
    assert!(!serialized.contains(TEST_PASSWORD));
    for side in ["source", "target"] {
        let names = std::fs::read_dir(root.join("named-queue").join(side))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, vec![std::ffi::OsString::from("Datei.bin")]);
    }
    assert_eq!(
        std::fs::read(root.join("named-queue/target/Datei.bin")).unwrap(),
        b"main"
    );
    println!("NAMED_STREAM_QUEUE_OK completed=7 drift_stopped=2 acl_only_source_drift=1 acl_drift_transferred_files_bytes=0 target_id_manifest_preserved=1 windows_acl_owner_base_omitted=1 windows_acl_noop_omitted=1 old_target_preserved=1 add_bytes=8 update_bytes=8 clear_bytes=4 local_bytes=4 noop_bytes=0 local_noop_bytes=0 local_noop_named_streams_omitted=1");
}
