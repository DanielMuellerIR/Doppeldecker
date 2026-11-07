//! Opt-in-Gegenlauf des echten Writers gegen eine frische verschlüsselte Freigabe.
use super::*;
use std::os::unix::fs::PermissionsExt;

struct TestSecrets;
impl SecretResolver for TestSecrets {
    fn resolve(&self, _: &crate::KeychainSecretReference) -> io::Result<crate::SecretString> {
        crate::SecretString::new("Doppeldecker-SMB-E2E-wertlos-2026".into())
    }
}

fn summary(name: &str, bytes: &[u8]) -> NamedBlobSummary {
    NamedBlobSummary {
        name: name.as_bytes().to_vec(),
        value: BlobSummary::from_bytes(bytes),
    }
}

fn proof(provider: &SmbVfs, path: &Path) -> crate::sync::SyncTargetProof {
    let entry = provider.stat(path).unwrap();
    serde_json::from_value(serde_json::json!({
        "raw_name": entry.name.as_bytes(), "provider_token": null,
        "is_dir": entry.is_dir, "is_file": entry.is_file, "is_symlink": entry.is_symlink,
        "size": entry.size, "mtime": entry.mtime, "mode": entry.mode,
        "device": entry.device, "inode": entry.inode,
    }))
    .unwrap()
}

fn observed(provider: &Arc<SmbVfs>, path: &Path) -> Vec<NamedBlobSummary> {
    provider
        .content_sync_reader(path.parent().unwrap(), &crate::Cancel::new(), None)
        .unwrap()
        .metadata_manifest(path)
        .unwrap()
        .named_streams
        .unwrap()
}

fn names(provider: &SmbVfs, path: &Path) -> BTreeMap<Vec<u8>, u64> {
    let remote = provider.remote_path(path).unwrap();
    let mut state = lock(&provider.state);
    let tree = state.session.tree.clone();
    provider
        .runtime
        .block_on(async {
            let (base, _, mut parents) = open_bound_base(
                &mut state.session.connection,
                &tree,
                &remote,
                StreamBaseKind::Directory,
                true,
                &StreamBindingScope::EntireShare,
            )
            .await?;
            let result = stream_list(
                &mut state.session.connection,
                &tree,
                base,
                StreamBaseKind::Directory,
            )
            .await;
            tree.close_handle(&mut state.session.connection, base)
                .await
                .map_err(smb_error)?;
            close_parents(&mut state.session.connection, &tree, &mut parents).await?;
            result
        })
        .unwrap()
}

fn seed(provider: &SmbVfs, path: &Path, name: &[u8], bytes: &[u8]) {
    let path = stream_path(&provider.remote_path(path).unwrap(), name).unwrap();
    let state = lock(&provider.state);
    let mut connection = state.session.connection.clone();
    let tree = state.session.tree.clone();
    drop(state);
    let id = provider
        .runtime
        .block_on(exclusive_stream(
            &mut connection,
            &tree,
            &path,
            CreateDisposition::FileCreate,
        ))
        .unwrap();
    let mut writer = SmbWrite {
        runtime: provider.runtime.clone(),
        tree,
        connection,
        file_id: id,
        offset: 0,
        max_write: 64 * 1024,
        finished: false,
    };
    writer.write_all(bytes).unwrap();
    writer.flush().unwrap();
}

#[test]
fn confirmed_stream_set_rejects_lookup_aliases() {
    assert!(stream_set(&[summary("A", b"a"), summary("a", b"b")]).is_err());
    assert!(stream_set(&[summary("Straße", b"a"), summary("STRASSE", b"b")]).is_err());
    assert!(stream_set(&[summary("a:b", b"a")]).is_err());
    assert_eq!(stream_set(&[summary("leer", b"")]).unwrap().len(), 1);
}

#[test]
#[ignore = "benötigt privaten Samba-Loopback mit streams_xattr"]
fn directory_stream_transaction_roundtrip() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("{name} fehlt"));
    let physical_root = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(physical_root.starts_with("/tmp/doppeldecker-directory-stream-write-probe-20261029"));
    assert_eq!(physical_root.file_name().unwrap(), "share");
    assert!(std::fs::read_dir(&physical_root).unwrap().next().is_none());
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
        password_reference: crate::KeychainSecretReference::new("Doppeldecker-Test", "smb")
            .unwrap(),
    };
    let provider = Arc::new(SmbVfs::connect(config.clone(), Arc::new(TestSecrets)).unwrap());
    let foreign = Arc::new(SmbVfs::connect(config, Arc::new(TestSecrets)).unwrap());
    {
        let ours = lock(&provider.state);
        let theirs = lock(&foreign.state);
        assert_ne!(
            ours.session.connection.session_id(),
            theirs.session.connection.session_id()
        );
    }
    std::fs::create_dir_all(physical_root.join("provider/selected/directory")).unwrap();
    std::fs::write(
        physical_root.join("provider/selected/directory/child.txt"),
        b"child",
    )
    .unwrap();
    let selected = Path::new("/provider/selected");
    let directory = Path::new("/provider/selected/directory");
    let config_root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let staging_owner = StagingOwner {
        queue: config_root.path().join("queue.json"),
        operation: "directory-provider-test".into(),
    };
    let owner = || {
        Some(crate::remote_real_mode::InteractiveFileOwner {
            staging: staging_owner.clone(),
            profiles: Vec::new(),
        })
    };
    let cancel = crate::Cancel::new();
    let context = provider
        .content_sync_mutations(selected, &cancel, owner())
        .unwrap();
    assert!(context.supports_directory_stream_updates(selected).unwrap());
    assert!(!provider
        .supports_directory_stream_updates(selected)
        .unwrap());
    assert!(!provider
        .content_sync_reader(selected, &cancel, None)
        .unwrap()
        .supports_directory_stream_updates(selected)
        .unwrap());
    assert!(!provider
        .ordinary_mutations(selected, &cancel, None)
        .unwrap()
        .supports_directory_stream_updates(selected)
        .unwrap());

    let base_proof = proof(&provider, directory);
    let mut transaction = context
        .begin_directory_stream_update(directory, &base_proof, &[])
        .unwrap();
    let unicode = summary("Änderung", b"eins");
    let empty = summary("leer", b"");
    {
        let mut writer = transaction.stage(&unicode).unwrap();
        writer.write_all(b"eins").unwrap();
        writer.flush().unwrap();
        let current_names = names(&foreign, directory);
        let temp = current_names.keys().next().unwrap();
        assert!(temp.starts_with(b".doppeldecker-stream-"));
        assert!(
            foreign
                .stream_read(directory, temp, Some(selected))
                .is_err(),
            "flush darf exklusiven Creator nicht schließen"
        );
        assert!(foreign
            .rename_replace_unchecked(directory, Path::new("/provider/selected/moved"))
            .is_err());
        assert!(foreign
            .rename_replace_unchecked(selected, Path::new("/provider/moved"))
            .is_err());
        assert_eq!(
            writer.commit(None).unwrap(),
            DirectoryStreamEffect::Add {
                after: unicode.clone()
            }
        );
    }
    transaction.stage(&empty).unwrap().commit(None).unwrap();
    let report = transaction
        .finish(&[unicode.clone(), empty.clone()])
        .unwrap();
    assert_eq!(report.completed.len(), 2);
    assert!(report.uncertain.is_empty());
    assert!(report
        .completed
        .iter()
        .all(|outcome| outcome.directory == directory.to_str().unwrap()));
    let initial = observed(&provider, directory);
    assert_eq!(
        stream_set(&initial).unwrap(),
        stream_set(&[unicode.clone(), empty.clone()]).unwrap()
    );
    assert!(!std::fs::read_dir(config_root.path())
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .path()
            .to_string_lossy()
            .ends_with(".recovery.json")));
    eprintln!("DIRECTORY_WRITER add Unicode+leer, Livecreator/Parent/Basis gebunden, 2 vollständige Wirkungen");

    // Kindänderungen verändern Ordnerzeiten, niemals aber die bestätigte Basis-ID.
    std::fs::write(
        physical_root.join("provider/selected/directory/other.txt"),
        b"other",
    )
    .unwrap();
    let mut transaction = context
        .begin_directory_stream_update(directory, &base_proof, &initial)
        .unwrap();
    let replacement = summary("Änderung", b"zwei!");
    let mut writer = transaction.stage(&replacement).unwrap();
    writer.write_all(b"zwei!").unwrap();
    assert_eq!(
        writer.commit(Some(&unicode)).unwrap(),
        DirectoryStreamEffect::Replace {
            before: unicode.clone(),
            after: replacement.clone()
        }
    );
    transaction.remove(&empty).unwrap();
    let report = transaction
        .finish(std::slice::from_ref(&replacement))
        .unwrap();
    assert_eq!(report.completed.len(), 2);
    assert!(report.uncertain.is_empty());
    assert_eq!(observed(&provider, directory), vec![replacement.clone()]);
    eprintln!("DIRECTORY_WRITER Replace class10 + frische Remove-Handles exakt bestätigt");

    let mut transaction = context
        .begin_directory_stream_update(directory, &base_proof, std::slice::from_ref(&replacement))
        .unwrap();
    transaction.remove(&replacement).unwrap();
    assert_eq!(transaction.finish(&[]).unwrap().completed.len(), 1);
    assert!(observed(&provider, directory).is_empty());
    assert!(context
        .begin_directory_stream_update(directory, &base_proof, &[])
        .unwrap()
        .finish(&[])
        .unwrap()
        .is_empty());

    // Ein Datenfehler vor dem Commit erlaubt nur den nie umbenannten eigenen
    // Creator zu entfernen. Der Finalname darf dabei nicht entstehen.
    let mut transaction = context
        .begin_directory_stream_update(directory, &base_proof, &[])
        .unwrap();
    let mut writer = transaction.stage(&unicode).unwrap();
    writer.write_all(b"xxxx").unwrap();
    assert!(writer.flush().is_err());
    writer.abort().unwrap();
    assert!(transaction.finish(&[]).unwrap().is_empty());
    assert!(observed(&provider, directory).is_empty());

    let abort_cancel = crate::Cancel::new();
    let abort_context = provider
        .content_sync_mutations(selected, &abort_cancel, owner())
        .unwrap();
    let mut transaction = abort_context
        .begin_directory_stream_update(directory, &base_proof, &[])
        .unwrap();
    let mut writer = transaction.stage(&unicode).unwrap();
    writer.write_all(b"eins").unwrap();
    abort_cancel.cancel();
    writer.abort().unwrap();
    assert!(transaction.report().is_empty());
    drop(transaction);
    assert!(observed(&provider, directory).is_empty());
    eprintln!("DIRECTORY_WRITER leerer Satz/Noop, Hashfehler und Cancel vor Rename: keine Finalwirkung, Tempabwesenheit bestätigt");

    for point in ["write", "flush"] {
        let mut transaction = context
            .begin_directory_stream_update(directory, &base_proof, &[])
            .unwrap();
        let mut writer = transaction.stage(&unicode).unwrap();
        let error = if point == "write" {
            TEST_FAULT.with(|fault| fault.set(Some(point)));
            writer.write_all(b"eins").unwrap_err()
        } else {
            writer.write_all(b"eins").unwrap();
            TEST_FAULT.with(|fault| fault.set(Some(point)));
            writer.flush().unwrap_err()
        };
        assert_eq!(
            crate::error::error_code(&error),
            crate::ErrorCode::OutcomeUnclear
        );
        writer.abort().unwrap();
        assert!(transaction.finish(&[]).unwrap().is_empty());
        assert!(observed(&provider, directory).is_empty());
    }
    for point in ["rename", "close"] {
        let virtual_path = selected.join(format!("fault-{point}"));
        std::fs::create_dir(
            physical_root
                .join("provider/selected")
                .join(format!("fault-{point}")),
        )
        .unwrap();
        let mut transaction = context
            .begin_directory_stream_update(&virtual_path, &proof(&provider, &virtual_path), &[])
            .unwrap();
        let mut writer = transaction.stage(&unicode).unwrap();
        writer.write_all(b"eins").unwrap();
        TEST_FAULT.with(|fault| fault.set(Some(point)));
        let error = writer.commit(None).unwrap_err();
        assert_eq!(
            crate::error::error_code(&error),
            crate::ErrorCode::OutcomeUnclear
        );
        assert!(transaction.report().completed.is_empty());
        assert_eq!(transaction.report().uncertain.len(), 1);
        let paths = transaction.recovery_paths();
        assert!(paths
            .iter()
            .any(|path| path.contains("Datenstrom") && path.contains("Änderung")));
        let index = paths
            .iter()
            .find(|path| path.ends_with(".recovery.json"))
            .unwrap();
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(index).unwrap()).unwrap();
        assert_eq!(index["named_stream"]["phase"], "Committing");
        drop(transaction);
        assert_eq!(observed(&foreign, &virtual_path), vec![unicode.clone()]);
    }
    eprintln!("DIRECTORY_WRITER injizierter Fehler nach echtem WRITE/FLUSH: eigener Abort; nach echtem Rename/Close: OutcomeUnclear + einzelne uncertain-Wirkung + Committing-Intent, Finalbytes erhalten");

    let mut transaction = context
        .begin_directory_stream_update(directory, &base_proof, &[])
        .unwrap();
    let mut writer = transaction.stage(&unicode).unwrap();
    writer.write_all(b"eins").unwrap();
    writer.commit(None).unwrap();
    let later = summary("später", b"later");
    let mut writer = transaction.stage(&later).unwrap();
    writer.write_all(b"later").unwrap();
    seed(&foreign, directory, b"foreign-extra", b"foreign");
    assert!(writer.commit(None).is_err());
    assert_eq!(transaction.report().completed.len(), 1);
    assert!(transaction.report().uncertain.is_empty());
    drop(transaction);
    let current_names = names(&foreign, directory);
    assert!(current_names.contains_key(b"foreign-extra".as_slice()));
    assert!(!current_names.contains_key(later.name.as_slice()));
    assert!(current_names
        .keys()
        .any(|name| name.starts_with(b".doppeldecker-stream-")));
    assert!(std::fs::read_dir(config_root.path())
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .path()
            .to_string_lossy()
            .ends_with(".recovery.json")));
    assert_eq!(
        std::fs::read(physical_root.join("provider/selected/directory/child.txt")).unwrap(),
        b"child"
    );
    assert_eq!(
        std::fs::read(physical_root.join("provider/selected/directory/other.txt")).unwrap(),
        b"other"
    );
    eprintln!("DIRECTORY_WRITER Fremdstreamdrift verhindert späteren Rename, erste Wirkung/konkrete Registryintents bleiben erhalten, Kinddaten unverändert");

    drop(context);
    drop(abort_context);
    drop(foreign);
    drop(provider);
    std::fs::remove_dir_all(physical_root.join("provider")).unwrap();
    assert!(std::fs::read_dir(&physical_root).unwrap().next().is_none());
    eprintln!("DIRECTORY_WRITER künstlicher Root vor/nach vollständig leer");
}

#[path = "smb_directory_streams_queue_tests.rs"]
mod queue;
