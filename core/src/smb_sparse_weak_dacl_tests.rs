use super::*;

fn weak_provider() -> io::Result<(Arc<SmbVfs>, PathBuf)> {
    let physical = PathBuf::from(
        std::env::var("DOPPELDECKER_SMB_WEAK_ROOT").expect("Zweiter künstlicher Share fehlt"),
    );
    assert!(physical.starts_with("/tmp/doppeldecker-smb-acl-write-probe-20261029/wd31"));
    assert_eq!(physical.file_name().unwrap(), "weak-share");
    assert!(std::fs::read_dir(&physical)?.next().is_none());
    let host = std::env::var("DOPPELDECKER_SMB_TEST_HOST").unwrap();
    assert_eq!(host, "127.0.0.1");
    let port = std::env::var("DOPPELDECKER_SMB_TEST_PORT")
        .unwrap()
        .parse()
        .unwrap();
    assert!(port > 1024);
    let provider = Arc::new(SmbVfs::connect(
        SmbConnectionConfig {
            server_guid: discover_smb_server(&host, port)?.server_guid,
            host,
            port,
            share: "DDWEAK".into(),
            username: std::env::var("DOPPELDECKER_SMB_TEST_USER").unwrap(),
            domain: String::new(),
            requested_root: "/".into(),
            password_reference: crate::KeychainSecretReference::new(
                "Doppeldecker-Test",
                "weak-dacl",
            )?,
        },
        Arc::new(fixtures::TestSecrets),
    )?);
    Ok((provider, physical))
}

#[test]
#[ignore = "frischer künstlicher Zwei-Share-Samba: konkrete DACL-Abweichung und reale Sparse-/ADS-Wirkung"]
fn sparse_weak_dacl_capability_gate() -> io::Result<()> {
    use std::io::{Read, Write};
    let (source, physical_source) = fixtures::fixture()?;
    let (target, physical_target) = weak_provider()?;
    assert_eq!(physical_source.parent(), physical_target.parent());
    std::fs::create_dir(physical_source.join("selected"))?;
    std::fs::create_dir(physical_target.join("selected"))?;
    std::fs::write(physical_source.join("sentinel"), b"SOURCE")?;
    std::fs::write(physical_target.join("sentinel"), b"TARGET")?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    let root = Path::new("/selected");
    let (strong, _) = fixtures::prepared(&source, root, &config, &Cancel::new())?;
    assert!(matches!(
        strong.windows_dacl_write_support()?,
        crate::windows_dacl::WindowsDaclWriteSupport::Ready(_)
    ));
    let weak = target_scope(&target, root, &config, &Cancel::new())?;
    weak.prepare_windows_dacl_write()?;
    let support = weak.windows_dacl_write_support()?;
    println!("SMB_WEAK_DACL_CAPABILITY_OBSERVED source=Ready target={support:?}");
    assert!(
        matches!(
            support,
            crate::windows_dacl::WindowsDaclWriteSupport::Unavailable(_)
        ),
        "Tatsächliche Zielabweichung fehlt: {support:?}"
    );
    assert!(std::fs::read_dir(physical_source.join("selected"))?
        .next()
        .is_none());
    assert!(std::fs::read_dir(physical_target.join("selected"))?
        .next()
        .is_none());
    // Das bestehende minimale Sparse-Ziel ist unabhängig vom neuen SMB-Quellvertrag.
    weak.prepare_sparse_write(crate::metadata::AclModel::Posix)?;
    assert_eq!(
        weak.metadata_capabilities(root)?.sparse_extents,
        Capability::Exact
    );
    let final_path = root.join("capability.bin");
    let scope = weak.staging_scope(&final_path, None)?.unwrap();
    let temp = scope.staging_candidate(&final_path, "tmp")?;
    let mut data = scope.open_sparse_write_new(&temp)?;
    data.set_len(65536)?;
    data.punch_hole(0, 65536)?;
    data.flush()?;
    drop(data);
    let mut ads = scope.open_named_stream_write_new(&temp, b"named")?;
    ads.write_all(b"weak-target-ads")?;
    ads.flush()?;
    drop(ads);
    scope.set_mtime(&temp, 1_700_000_003)?;
    scope.sync_file(&temp)?;
    scope.rename_noreplace(&temp, &final_path)?;
    scope.finish_staging()?;
    let reader = target.content_sync_reader(root, &Cancel::new(), None)?;
    let mut bytes = Vec::new();
    reader.open_read(&final_path)?.read_to_end(&mut bytes)?;
    assert_eq!(bytes, vec![0; 65536]);
    let mut stream = Vec::new();
    reader
        .open_named_stream_read(&final_path, b"named")?
        .read_to_end(&mut stream)?;
    assert_eq!(stream, b"weak-target-ads");
    assert_eq!(
        reader.metadata_manifest(&final_path)?.sparse_extents,
        Some(vec![])
    );
    assert_eq!(
        crate::LocalVfs::new()
            .metadata_manifest(&physical_target.join("selected/capability.bin"))?
            .sparse_extents,
        Some(vec![])
    );
    assert_eq!(
        std::fs::read_dir(physical_target.join("selected"))?.count(),
        1
    );
    assert_eq!(std::fs::read(physical_source.join("sentinel"))?, b"SOURCE");
    assert_eq!(std::fs::read(physical_target.join("sentinel"))?, b"TARGET");
    println!(
        "SMB_WEAK_DACL_TWO_SHARE_REAL_SPARSE_ADS_HASH_MTIME_AND_BOTH_ROOTS_OK target={support:?}"
    );
    Ok(())
}

fn seed_weak_source(
    provider: &Arc<SmbVfs>,
    root: &Path,
    config: &Path,
    bytes: &[u8],
    islands: &[(u64, usize)],
) -> io::Result<()> {
    let parent = target_scope(provider, root, config, &Cancel::new())?;
    parent.prepare_sparse_write(crate::metadata::AclModel::Posix)?;
    let target = root.join("file.bin");
    let temp = root.join(".seed");
    let scope = parent.staging_scope(&target, None)?.unwrap();
    let mut writer = scope.open_sparse_write_new(&temp)?;
    writer.set_len(bytes.len() as u64)?;
    let mut cursor = 0;
    for &(offset, length) in islands {
        if offset > cursor {
            writer.punch_hole(cursor, offset - cursor)?;
        }
        writer.seek(SeekFrom::Start(offset))?;
        writer.write_all(&bytes[offset as usize..offset as usize + length])?;
        cursor = offset + length as u64;
    }
    if cursor < bytes.len() as u64 {
        writer.punch_hole(cursor, bytes.len() as u64 - cursor)?;
    }
    writer.flush()?;
    drop(writer);
    let mut ads = scope.open_named_stream_write_new(&temp, b"named")?;
    ads.write_all(b"temp-dacl-ads")?;
    ads.flush()?;
    drop(ads);
    scope.set_mtime(&temp, 1_700_000_003)?;
    scope.sync_file(&temp)?;
    match provider.stat(&target) {
        Ok(_) => scope.rename_replace_plain_once(&temp, &target)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            scope.rename_noreplace(&temp, &target)?
        }
        Err(error) => return Err(error),
    }
    scope.finish_staging()?;
    let manifest = provider
        .content_sync_reader(root, &Cancel::new(), None)?
        .metadata_manifest(&target)?;
    let Some(crate::metadata::Acl::WindowsDacl(acl)) = manifest.acl else {
        panic!("Windows-Quell-ACL fehlt")
    };
    assert!(
        acl.file_write_issue(
            manifest.windows_security.as_ref().unwrap().namespace,
            manifest
                .windows_security
                .as_ref()
                .unwrap()
                .owner
                .as_ref()
                .unwrap()
        )
        .is_some(),
        "Die echte ungeschützte Quell-ACL muss konkret ausgelassen bleiben: {acl:?}"
    );
    Ok(())
}

fn weak_prepare(
    source: &Arc<SmbVfs>,
    target: &Arc<SmbVfs>,
    queue: &OperationQueue,
    root: &Path,
    mode: crate::SyncMode,
) -> io::Result<crate::PreparedContentSync> {
    crate::prepare_content_sync_for_queue(
        queue,
        ProviderEndpoint::smb(source.clone(), root)?,
        ProviderEndpoint::smb(target.clone(), root)?,
        joint_options(mode),
        &Cancel::new(),
    )
}

fn weak_queue_matrix(
    source: &Arc<SmbVfs>,
    target: &Arc<SmbVfs>,
    marker: &[u8],
    physical: Option<&Path>,
) -> io::Result<()> {
    use sha2::Digest;
    use std::io::Read;
    let root = Path::new("/selected");
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    let queue = OperationQueue::open(config.join("queue.json"))?;
    let mut bytes = vec![0; 1024 * 1024];
    let mut islands = vec![];
    seed_weak_source(source, root, &config, &bytes, &islands)?;
    for (mode, phase) in [
        (crate::SyncMode::AddOnly, "add"),
        (crate::SyncMode::Update, "noop"),
        (crate::SyncMode::Update, "replace"),
        (crate::SyncMode::Update, "replace-noop"),
        (crate::SyncMode::Mirror, "mirror"),
        (crate::SyncMode::Update, "final-noop"),
    ] {
        if phase == "replace" {
            bytes[4096..8192].fill(0x41);
            islands = vec![(4096, 4096)];
            seed_weak_source(source, root, &config, &bytes, &islands)?;
        }
        if phase == "mirror" {
            let scope = target.ordinary_mutations(root, &Cancel::new(), None)?;
            let mut extra = scope.open_write_new(&root.join("extra"))?;
            extra.write_all(b"EXTRA")?;
            extra.flush()?;
            drop(extra);
        }
        let prepared = weak_prepare(source, target, &queue, root, mode)?;
        let preview = prepared.preview();
        assert!(
            !preview
                .plan
                .metadata_report
                .issues
                .iter()
                .any(|issue| matches!(
                    issue.class,
                    crate::MetadataClass::SparseExtents | crate::MetadataClass::NamedStreams
                )),
            "{:?}",
            preview.plan.metadata_report
        );
        assert!(
            preview
                .plan
                .metadata_report
                .issues
                .iter()
                .any(|issue| issue.class == crate::MetadataClass::Acl),
            "{:?}",
            preview.plan.metadata_report
        );
        if matches!(phase, "add" | "replace") {
            assert!(preview.plan.actions.iter().any(|action|matches!(action,crate::Action::CopyFile{metadata:Some(value),..}|crate::Action::UpdateFile{metadata:Some(value),..} if value.sparse_extents && value.named_streams && !value.acl)));
        }
        if phase.contains("noop") {
            assert!(
                preview.plan.actions.is_empty(),
                "{:?}",
                preview.plan.actions
            );
        }
        let before = target
            .stat(&root.join("file.bin"))
            .ok()
            .and_then(|entry| entry.inode);
        let stats = execute_joint(&queue, prepared, &config)?;
        assert_eq!(
            stats.files_copied,
            u64::from(matches!(phase, "add" | "replace"))
        );
        assert!(stats
            .metadata_report
            .issues
            .iter()
            .any(|issue| issue.class == crate::MetadataClass::Acl));
        assert!(!stats.metadata_report.issues.iter().any(|issue| matches!(
            issue.class,
            crate::MetadataClass::SparseExtents | crate::MetadataClass::NamedStreams
        )));
        if phase.contains("noop") {
            assert_eq!(target.stat(&root.join("file.bin"))?.inode, before);
        }
        let reader = target.content_sync_reader(root, &Cancel::new(), None)?;
        let path = root.join("file.bin");
        let manifest = reader.metadata_manifest(&path)?;
        let expected: Vec<_> = islands
            .iter()
            .map(|&(offset, length)| crate::metadata::DataExtent {
                offset,
                length: length as u64,
            })
            .collect();
        assert_eq!(manifest.sparse_extents, Some(expected.clone()));
        let mut data = Vec::new();
        reader.open_read(&path)?.read_to_end(&mut data)?;
        assert_eq!(data, bytes);
        let mut ads = Vec::new();
        reader
            .open_named_stream_read(&path, b"named")?
            .read_to_end(&mut ads)?;
        assert_eq!(ads, b"temp-dacl-ads");
        assert_eq!(target.stat(&path)?.mtime, Some(1_700_000_003));
        assert_eq!(target.list(root)?.len(), 1);
        for provider in [source, target] {
            let mut actual = Vec::new();
            provider
                .open_read(Path::new("/sentinel"))?
                .read_to_end(&mut actual)?;
            assert_eq!(actual, marker);
        }
        if let Some(physical) = physical {
            assert_eq!(
                crate::LocalVfs::new()
                    .metadata_manifest(&physical.join("selected/file.bin"))?
                    .sparse_extents,
                manifest.sparse_extents
            );
            assert_eq!(std::fs::read(physical.join("selected/file.bin"))?, bytes);
        }
        println!("SMB_WEAK_DACL_ACTUAL_QUEUE_SPARSE_ADS_OMISSION_OK phase={phase}");
        if phase == "final-noop" {
            for side in ["source", "target"] {
                println!(
                    "SMB_WEAK_MAC_EXPECTED {}",
                    serde_json::json!({"side":side,"path":"file.bin","size":bytes.len(),"sha256":format!("{:x}",sha2::Sha256::digest(&bytes)),"extents":expected})
                );
            }
        }
    }
    Ok(())
}

#[test]
#[ignore = "frischer künstlicher Zwei-Share-Samba: Sparse-/ADS-Queue und leere Folgeläufe bei konkreter ACL-Auslassung"]
fn sparse_weak_dacl_actual_queue_gate() -> io::Result<()> {
    let (source, physical_source) = fixtures::fixture()?;
    let (target, physical_target) = weak_provider()?;
    assert_eq!(physical_source.parent(), physical_target.parent());
    for physical in [&physical_source, &physical_target] {
        std::fs::create_dir(physical.join("selected"))?;
        std::fs::write(physical.join("sentinel"), b"UNCHANGED")?;
    }
    weak_queue_matrix(&source, &target, b"UNCHANGED", Some(&physical_target))
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "privater Loopbacktunnel: Zwei-Share-Sparse-/ADS-Queue bei konkreter ACL-Auslassung"]
fn sparse_weak_dacl_macos_actual_queue() -> io::Result<()> {
    use std::io::Read;
    let (source, marker) = mac_bridge_fixture()?;
    let mut config = source.config.clone();
    config.share = "DDWEAK".into();
    let target = Arc::new(SmbVfs::connect(config, Arc::new(fixtures::TestSecrets))?);
    let mut names: Vec<_> = target
        .list(Path::new("/"))?
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["selected", "sentinel"]);
    let mut actual = Vec::new();
    target
        .open_read(Path::new("/sentinel"))?
        .read_to_end(&mut actual)?;
    assert_eq!(actual, marker.as_bytes());
    assert!(target.list(Path::new("/selected"))?.is_empty());
    weak_queue_matrix(&source, &target, marker.as_bytes(), None)
}

#[test]
#[ignore = "frischer künstlicher Zwei-Share-Samba: Sparse-Abschlussfehler ohne DACL-Schreibzugriff"]
fn sparse_weak_dacl_queue_failures() -> io::Result<()> {
    let (source, physical_source) = fixtures::fixture()?;
    let (target, physical_target) = weak_provider()?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    let bytes = vec![0; 65536];
    for (index, point) in [
        "metadata-restore-before",
        "metadata-restore-after",
        "metadata-flush-before",
        "metadata-flush-after",
        "restore-before",
        "restore-after",
        "attribute-flush-before",
        "attribute-flush-after",
        "attribute-close-before",
        "attribute-close-after",
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("case-{index}");
        let root = Path::new("/").join(&name);
        let private = config.join(&name);
        std::fs::create_dir(&private)?;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))?;
        for physical in [&physical_source, &physical_target] {
            std::fs::create_dir(physical.join(&name))?;
        }
        seed_weak_source(&source, &root, &private, &bytes, &[])?;
        std::fs::write(physical_target.join(&name).join("file.bin"), b"OLD")?;
        std::fs::write(physical_target.join(&name).join("extra"), b"EXTRA")?;
        let original = target.stat(&root.join("file.bin"))?.inode;
        let queue = OperationQueue::open(private.join("queue.json"))?;
        let prepared = weak_prepare(&source, &target, &queue, &root, crate::SyncMode::Mirror)?;
        reset_counters();
        FAULT.with(|v| v.set(Some(point)));
        let error = execute_joint(&queue, prepared, &private).unwrap_err();
        FAULT.with(|v| assert!(v.get().is_none(), "unused {point}"));
        assert_eq!(
            crate::error_code(&error),
            crate::ErrorCode::OutcomeUnclear,
            "{point}: {error}"
        );
        assert!(!crate::error::recovery_paths(&error).is_empty());
        assert_eq!(
            queue.snapshot().entries.last().unwrap().state,
            crate::QueueState::Failed
        );
        let committed = point.starts_with("restore-") || point.starts_with("attribute-");
        assert_eq!(
            std::fs::read(physical_target.join(&name).join("file.bin"))?,
            if committed {
                bytes.clone()
            } else {
                b"OLD".to_vec()
            },
            "{point}"
        );
        if !committed {
            assert_eq!(target.stat(&root.join("file.bin"))?.inode, original);
        }
        assert_eq!(
            std::fs::read(physical_target.join(&name).join("extra"))?,
            b"EXTRA"
        );
        assert_eq!(counters().0, 2, "probe and data CLOSE: {point}");
        assert_eq!(counters().2, 1, "single completion CLOSE: {point}");
        let before = (counters(), METADATA_RESTORES.with(|v| v.get()));
        let blocked = weak_prepare(&source, &target, &queue, &root, crate::SyncMode::Mirror)
            .err()
            .expect("Offener Sparse-Abschluss muss erneutem Schreiben vorausgehen");
        assert!(!crate::error::recovery_paths(&blocked).is_empty());
        assert_eq!((counters(), METADATA_RESTORES.with(|v| v.get())), before);
        println!("SMB_WEAK_DACL_FAILURE_NO_RETRY_OLD_OR_KNOWN_NEW_EXTRA_OK point={point} committed={committed}");
    }
    Ok(())
}

#[test]
#[ignore = "frischer künstlicher Samba: unbekannter DACL-Probeausgang gewährt keine Sparse-Probe"]
fn sparse_dacl_unknown_probe_not_downgraded() -> io::Result<()> {
    let (provider, physical) = fixtures::fixture()?;
    std::fs::create_dir(physical.join("selected"))?;
    std::fs::write(physical.join("sentinel"), b"UNCHANGED")?;
    let area = secure_area()?;
    let config = area.path().canonicalize()?;
    let root = Path::new("/selected");
    let scope = target_scope(&provider, root, &config, &Cancel::new())?;
    reset_counters();
    wire::TEST_FAULT.with(|v| v.set(Some("invalid_frame")));
    let error = scope.prepare_windows_dacl_write().unwrap_err();
    wire::TEST_FAULT.with(|v| assert!(v.get().is_none()));
    assert!(crate::windows_dacl::known_write_omission(&error).is_none());
    assert!(matches!(
        scope.windows_dacl_write_support()?,
        crate::windows_dacl::WindowsDaclWriteSupport::Unavailable(
            crate::windows_dacl::WindowsDaclTargetIssue::NotPrepared
        )
    ));
    scope.prepare_sparse_write(crate::metadata::AclModel::WindowsSid)?;
    assert_eq!(
        scope.metadata_capabilities(root)?.sparse_extents,
        Capability::Unsupported
    );
    assert_eq!(counters(), (0, 0, 0, 0));
    assert!(provider.list(root)?.is_empty());
    assert_eq!(std::fs::read(physical.join("sentinel"))?, b"UNCHANGED");
    println!("SMB_UNKNOWN_DACL_PROBE_NO_SPARSE_FREIGABE_NO_RETRY_NO_ROOT_REMAINS_OK");
    Ok(())
}
