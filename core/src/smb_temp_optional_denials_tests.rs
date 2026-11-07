use super::*;
use crate::smb::interactive::sparse_write::tests::joint::seed_joint_remote;

fn verify_ea_access(provider: &SmbVfs, path: &Path, mask: u32) -> io::Result<()> {
    for access in [
        FileAccessMask::FILE_READ_EA,
        FileAccessMask::FILE_WRITE_EA,
        FileAccessMask::FILE_EXECUTE,
    ] {
        assert_eq!(
            access_status(provider, path, access, 7)?,
            if mask & access != 0 {
                NtStatus::ACCESS_DENIED
            } else {
                NtStatus::SUCCESS
            },
            "mask={mask:x} access={access:x}"
        );
    }
    Ok(())
}

fn ea_options(mode: SyncMode) -> SyncOptions {
    SyncOptions {
        mode,
        compare: Compare::ContentHash,
        metadata: MetadataProfile::NativeBestEffort,
        ..Default::default()
    }
}
fn ea_prepare(
    provider: &Arc<SmbVfs>,
    queue: &OperationQueue,
    source: &Path,
    target: &Path,
    mode: SyncMode,
) -> io::Result<crate::PreparedContentSync> {
    crate::prepare_content_sync_for_queue(
        queue,
        ProviderEndpoint::smb(provider.clone(), source)?,
        ProviderEndpoint::smb(provider.clone(), target)?,
        ea_options(mode),
        &Cancel::new(),
    )
}
fn ea_execute(
    queue: &OperationQueue,
    prepared: crate::PreparedContentSync,
    config: &Path,
) -> io::Result<crate::SyncStats> {
    queue.enqueue(prepared.queue_operation())?;
    let entry = queue.claim_next()?.unwrap();
    let QueueExecutionResult::Sync(stats) = crate::execute_prepared_content_sync_queue_entry(
        queue,
        &entry,
        prepared,
        true,
        &config.join("audit.jsonl"),
        &Cancel::new(),
        &mut |_| {},
    )?
    else {
        panic!()
    };
    Ok(stats)
}
fn assert_ea_report(report: &crate::MetadataReport) {
    assert!(
        !report.issues.iter().any(|issue| matches!(
            issue.class,
            crate::MetadataClass::Acl
                | crate::MetadataClass::SparseExtents
                | crate::MetadataClass::NamedStreams
        )),
        "{report:?}"
    );
}

#[test]
#[ignore = "frischer künstlicher Samba: normale neue/ersetzte Temps ohne unnötigen Daten-FLUSH-Handle"]
fn temp_ea_execute_legacy_completion() -> io::Result<()> {
    legacy_completion(&[8, 16, 32, 0x38, 0x3e, 0x8003e])
}

fn legacy_completion(masks: &[u32]) -> io::Result<()> {
    let (provider, physical) = fixture()?;
    std::fs::write(physical.join("sentinel"), b"UNCHANGED")?;
    for &mask in masks {
        let name = format!("mask-{mask:x}");
        std::fs::create_dir(physical.join(&name))?;
        let root = Path::new("/").join(&name);
        let area = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let mut previous = None;
        for bytes in [
            b"legacy-first".as_slice(),
            b"legacy-second-content".as_slice(),
        ] {
            FLUSHES.with(|v| v.set(0));
            CLOSES.with(|v| v.set(0));
            let (acl, context) = seed(&provider, &root, area.path(), bytes, mask)?;
            assert_eq!(FLUSHES.with(|v| v.get()), u32::from(mask & 6 != 0));
            assert_eq!(CLOSES.with(|v| v.get()), u32::from(mask & 6 != 0));
            let id = provider.stat(&root.join("file.bin"))?.inode.unwrap();
            assert_ne!(Some(id), previous);
            previous = Some(id);
            verify_file(
                &provider,
                &root,
                &root.join("file.bin"),
                bytes,
                &acl,
                &context,
                id,
            )?;
            verify_ea_access(&provider, &root.join("file.bin"), mask)?;
            assert_eq!(std::fs::read_dir(physical.join(&name))?.count(), 1);
            assert_eq!(std::fs::read(physical.join("sentinel"))?, b"UNCHANGED");
            println!(
                "SMB_FILE_DACL_LEGACY_COMPLETION_NO_EXTRA_WRITER_OK mask={mask:x} bytes={}",
                bytes.len()
            );
        }
    }
    Ok(())
}

fn ea_queue_matrix(
    provider: &Arc<SmbVfs>,
    selected: &Path,
    physical: Option<&Path>,
    marker: &[u8],
    masks: &[u32],
) -> io::Result<()> {
    use sha2::Digest;
    let setup = provider.ordinary_mutations(selected, &Cancel::new(), None)?;
    let area = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let config = area.path().canonicalize()?;
    for &mask in masks {
        let name = format!("mask-{mask:x}");
        let root = selected.join(&name);
        setup.create_dir(&root)?;
        setup.create_dir(&root.join("source"))?;
        setup.create_dir(&root.join("target"))?;
        let source = root.join("source");
        let target = root.join("target");
        let private = config.join(&name);
        std::fs::create_dir(&private)?;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))?;
        let mut bytes = vec![0; 1024 * 1024];
        let mut islands = vec![];
        let mut current_mask = mask;
        let (mut acl, mut context) =
            seed_joint_remote(provider, &source, &private, &bytes, &islands, current_mask)?;
        let queue = OperationQueue::open(private.join("queue.json"))?;
        for (mode, phase) in [
            (SyncMode::AddOnly, "add"),
            (SyncMode::Update, "noop"),
            (SyncMode::Update, "replace"),
            (SyncMode::Update, "acl-only"),
            (SyncMode::Mirror, "mirror"),
            (SyncMode::Update, "final-noop"),
        ] {
            if phase == "replace" {
                bytes[4096..8192].fill(0x41);
                islands = vec![(4096, 4096)];
                (acl, context) =
                    seed_joint_remote(provider, &source, &private, &bytes, &islands, current_mask)?;
            }
            if phase == "acl-only" {
                current_mask ^= 0x20;
                (acl, context) =
                    seed_joint_remote(provider, &source, &private, &bytes, &islands, current_mask)?;
            }
            if phase == "mirror" {
                write_probe(provider, &target.join("extra"), b"EXTRA")?;
            }
            let previous = match provider.stat(&target.join("file.bin")) {
                Ok(entry) => entry.inode,
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let prepared = ea_prepare(provider, &queue, &source, &target, mode)?;
            assert_ea_report(&prepared.preview().plan.metadata_report);
            if phase.contains("noop") {
                assert!(
                    prepared.preview().plan.actions.is_empty(),
                    "{:?}",
                    prepared.preview().plan.actions
                );
            }
            if matches!(phase, "add" | "replace") {
                assert!(prepared.preview().plan.actions.iter().any(|action|matches!(action,crate::Action::CopyFile{metadata:Some(metadata),..}|crate::Action::UpdateFile{metadata:Some(metadata),..} if metadata.acl && metadata.named_streams && metadata.sparse_extents)));
            }
            let stats = ea_execute(&queue, prepared, &private)?;
            assert_ea_report(&stats.metadata_report);
            assert_eq!(
                stats.files_copied,
                u64::from(matches!(phase, "add" | "replace"))
            );
            let id = provider.stat(&target.join("file.bin"))?.inode.unwrap();
            if phase.contains("noop") || phase == "acl-only" {
                assert_eq!(Some(id), previous);
            }
            if phase == "replace" {
                assert_ne!(Some(id), previous);
            }
            if phase == "acl-only" {
                assert_eq!(stats.file_dacl_report.completed.len(), 1);
            }
            let expected: Vec<_> = islands
                .iter()
                .map(|&(offset, length)| crate::metadata::DataExtent {
                    offset,
                    length: length as u64,
                })
                .collect();
            for (side, path) in [("source", &source), ("target", &target)] {
                let file = path.join("file.bin");
                verify_file(
                    provider,
                    path,
                    &file,
                    &bytes,
                    &acl,
                    &context,
                    provider.stat(&file)?.inode.unwrap(),
                )?;
                verify_ea_access(provider, &file, current_mask)?;
                assert_eq!(
                    provider
                        .content_sync_reader(path, &Cancel::new(), None)?
                        .metadata_manifest(&file)?
                        .sparse_extents,
                    Some(expected.clone())
                );
                assert_eq!(provider.list(path)?.len(), 1);
                if let Some(physical) = physical {
                    let file = physical.join(&name).join(side).join("file.bin");
                    assert_eq!(
                        crate::LocalVfs::new()
                            .metadata_manifest(&file)?
                            .sparse_extents,
                        Some(expected.clone())
                    );
                    assert_eq!(std::fs::read(file)?, bytes);
                }
                if phase == "final-noop" {
                    println!(
                        "SMB_SPARSE_MAC_EXPECTED {}",
                        serde_json::json!({"path":format!("{name}/{side}/file.bin"),"size":bytes.len(),"sha256":format!("{:x}",sha2::Sha256::digest(&bytes)),"extents":expected})
                    );
                }
            }
            let mut actual = Vec::new();
            provider
                .open_read(Path::new("/sentinel"))?
                .read_to_end(&mut actual)?;
            assert_eq!(actual, marker);
            println!("SMB_FILE_DACL_SPARSE_QUEUE_MATRIX_OK mask={mask:x} phase={phase} copied={} id={id}",stats.files_copied);
        }
    }
    Ok(())
}

#[test]
#[ignore = "frischer künstlicher Samba: EA-/Ausführungs-Denys und Sparse/ADS über die echte Queue"]
fn temp_ea_execute_sparse_queue_matrix() -> io::Result<()> {
    let (provider, physical) = fixture()?;
    std::fs::create_dir(physical.join("selected"))?;
    std::fs::write(physical.join("sentinel"), b"UNCHANGED")?;
    ea_queue_matrix(
        &provider,
        Path::new("/selected"),
        Some(&physical.join("selected")),
        b"UNCHANGED",
        &[8, 16, 32, 0x38, 0x3e, 0x8003e],
    )
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "privater Loopbacktunnel: macOS-Sparse-/ADS-Queue mit EA-/Ausführungs-Denys"]
fn temp_ea_execute_macos_actual_queue() -> io::Result<()> {
    let (provider, marker) = crate::smb::interactive::sparse_write::tests::mac_bridge_fixture()?;
    ea_queue_matrix(
        &provider,
        Path::new("/selected"),
        None,
        marker.as_bytes(),
        &[0x38, 0x3e],
    )
}

#[test]
#[ignore = "frischer künstlicher Samba: Fehler nach EA-DACL-SET behalten alte Ziele und verhindern Wiederholung"]
fn temp_ea_execute_after_set_failures() -> io::Result<()> {
    after_set_failures(&[0x38, 0x3e])
}

fn after_set_failures(masks: &[u32]) -> io::Result<()> {
    let (provider, physical) = fixture()?;
    std::fs::write(physical.join("sentinel"), b"UNCHANGED")?;
    for &mask in masks {
        for point in ["after_set", "normalized"] {
            let name = format!("mask-{mask:x}-{point}");
            let local = physical.join(&name);
            std::fs::create_dir_all(local.join("source"))?;
            std::fs::create_dir(local.join("target"))?;
            let root = Path::new("/").join(&name);
            let source = root.join("source");
            let target = root.join("target");
            let area = tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()?;
            let private = area.path().canonicalize()?;
            seed_joint_remote(&provider, &source, &private, &vec![0; 65536], &[], mask)?;
            std::fs::write(local.join("target/file.bin"), b"OLD")?;
            std::fs::write(local.join("target/extra"), b"EXTRA")?;
            let original = provider.stat(&target.join("file.bin"))?.inode;
            let queue = OperationQueue::open(private.join("queue.json"))?;
            let prepared = ea_prepare(&provider, &queue, &source, &target, SyncMode::Mirror)?;
            wire::TEST_BEFORE_SET.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    wire::TEST_FAULT.with(|value| value.set(Some(point)))
                }))
            });
            let error = ea_execute(&queue, prepared, &private).unwrap_err();
            wire::TEST_FAULT.with(|v| assert!(v.get().is_none()));
            wire::TEST_BEFORE_SET.with(|v| assert!(v.borrow().is_none()));
            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            assert!(!crate::error::recovery_paths(&error).is_empty());
            assert_eq!(
                queue.snapshot().entries.last().unwrap().state,
                QueueState::Failed
            );
            assert_eq!(provider.stat(&target.join("file.bin"))?.inode, original);
            assert_eq!(std::fs::read(local.join("target/file.bin"))?, b"OLD");
            assert_eq!(std::fs::read(local.join("target/extra"))?, b"EXTRA");
            let before = std::fs::metadata(local.join("target"))?.modified()?;
            let mut names: Vec<_> = std::fs::read_dir(local.join("target"))?
                .map(|entry| entry.unwrap().file_name())
                .collect();
            names.sort();
            let blocked = ea_prepare(&provider, &queue, &source, &target, SyncMode::Mirror)
                .err()
                .expect("Offener Rechteabschluss muss weitere Vorbereitung vor Effekten stoppen");
            assert!(!crate::error::recovery_paths(&blocked).is_empty());
            assert_eq!(std::fs::metadata(local.join("target"))?.modified()?, before);
            let mut observed: Vec<_> = std::fs::read_dir(local.join("target"))?
                .map(|entry| entry.unwrap().file_name())
                .collect();
            observed.sort();
            assert_eq!(observed, names);
            assert_eq!(std::fs::read(physical.join("sentinel"))?, b"UNCHANGED");
            println!(
                "SMB_FILE_DACL_AFTER_SET_UNCLEAR_OLD_EXTRA_NO_RETRY_OK mask={mask:x} point={point}"
            );
        }
    }
    Ok(())
}

#[test]
#[ignore = "frischer künstlicher Samba: WRITE_DAC-Deny über tatsächliche Sparse-/DACL-Queue"]
fn write_dac_sparse_queue_matrix() -> io::Result<()> {
    let (provider, physical) = fixture()?;
    std::fs::create_dir(physical.join("selected"))?;
    std::fs::write(physical.join("sentinel"), b"UNCHANGED")?;
    ea_queue_matrix(
        &provider,
        Path::new("/selected"),
        Some(&physical.join("selected")),
        b"UNCHANGED",
        &[0x40000, 0x40006, 0x4003e, 0xc003e],
    )
}

#[test]
#[ignore = "frischer künstlicher Samba: normale neue/ersetzte WRITE_DAC-Temps"]
fn write_dac_legacy_completion() -> io::Result<()> {
    legacy_completion(&[0x40000, 0x40006, 0x4003e, 0xc003e])
}

#[test]
#[ignore = "frischer künstlicher Samba: WRITE_DAC-Antwortverlust nach SET"]
fn write_dac_after_set_failures() -> io::Result<()> {
    after_set_failures(&[0x40000, 0x40006])
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "privater Loopbacktunnel: macOS-WRITE_DAC-Sparse-/ADS-Queue"]
fn write_dac_macos_actual_queue() -> io::Result<()> {
    let (provider, marker) = crate::smb::interactive::sparse_write::tests::mac_bridge_fixture()?;
    ea_queue_matrix(
        &provider,
        Path::new("/selected"),
        None,
        marker.as_bytes(),
        &[0x40000, 0x40006],
    )
}

#[test]
#[ignore = "frischer künstlicher Samba: erste WRITE_DAC-Versiegelung an bestehender Datei"]
fn write_dac_inplace_seal_and_noop() -> io::Result<()> {
    let (provider, physical) = fixture()?;
    std::fs::write(physical.join("sentinel"), b"UNCHANGED")?;
    for mask in [0x40000, 0x40006] {
        let name = format!("mask-{mask:x}");
        let local = physical.join(&name);
        std::fs::create_dir_all(local.join("source"))?;
        std::fs::create_dir(local.join("target"))?;
        let root = Path::new("/").join(&name);
        let source = root.join("source");
        let target = root.join("target");
        let area = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let private = area.path().canonicalize()?;
        let bytes = vec![0; 65536];
        seed_joint_remote(&provider, &source, &private, &bytes, &[], mask & 6)?;
        let queue = OperationQueue::open(private.join("queue.json"))?;
        let initial = ea_execute(
            &queue,
            ea_prepare(&provider, &queue, &source, &target, SyncMode::AddOnly)?,
            &private,
        )?;
        assert_eq!(initial.files_copied, 1);
        let id = provider.stat(&target.join("file.bin"))?.inode.unwrap();
        let (acl, context) = seed_joint_remote(&provider, &source, &private, &bytes, &[], mask)?;
        let prepared = ea_prepare(&provider, &queue, &source, &target, SyncMode::Update)?;
        assert!(prepared
            .preview()
            .plan
            .actions
            .iter()
            .any(|action| matches!(action, crate::Action::SetFileDacl { .. })));
        let stats = ea_execute(&queue, prepared, &private)?;
        assert_eq!(stats.files_copied, 0);
        assert_eq!(stats.file_dacl_report.completed.len(), 1);
        assert_ea_report(&stats.metadata_report);
        verify_file(
            &provider,
            &target,
            &target.join("file.bin"),
            &bytes,
            &acl,
            &context,
            id,
        )?;
        let noop = ea_prepare(&provider, &queue, &source, &target, SyncMode::Update)?;
        assert!(noop.preview().plan.actions.is_empty());
        let stats = ea_execute(&queue, noop, &private)?;
        assert_eq!(stats.files_copied, 0);
        assert_ea_report(&stats.metadata_report);
        assert_eq!(provider.stat(&target.join("file.bin"))?.inode, Some(id));
        assert_eq!(std::fs::read_dir(local.join("target"))?.count(), 1);
        assert_eq!(std::fs::read(physical.join("sentinel"))?, b"UNCHANGED");
        println!("WRITE_DAC_EXISTING_FILE_FIRST_SEAL_AND_TRUE_NOOP_OK mask={mask:x} id={id}");
    }
    Ok(())
}
