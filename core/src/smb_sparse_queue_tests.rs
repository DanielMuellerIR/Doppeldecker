//! Reale sparse SMB-Quellen über dieselbe Queue zum starken lokalen Ziel.
use super::*;
use crate::metadata::{Capability, DataExtent, SparseReadIssue};
use crate::smb::named_streams::sparse::{
    TEST_CLOSE_CALLS, TEST_FINISH_FAULT, TEST_ISSUE, TEST_READ_BYTES,
};
use smb2::msg::ioctl::{IoctlRequest, SMB2_0_IOCTL_IS_FSCTL};
use std::io::{Seek, SeekFrom};
const SIZE: u64 = 1024 * 1024;

fn mark(provider: &SmbVfs, path: &Path) -> io::Result<()> {
    let remote = provider.remote_path(path)?;
    let mut state = lock(&provider.state);
    let tree = state.session.tree.clone();
    provider.runtime.block_on(async {
        let id = open_handle(
            &mut state.session.connection,
            &tree,
            &remote,
            FileAccessMask::FILE_WRITE_DATA | FileAccessMask::SYNCHRONIZE,
            CreateDisposition::FileOpen,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        )
        .await?;
        let result = async {
            let request = IoctlRequest {
                ctl_code: 0x0009_00c4,
                file_id: id,
                max_input_response: 0,
                max_output_response: 0,
                flags: SMB2_0_IOCTL_IS_FSCTL,
                input_data: vec![1],
            };
            let frame = state
                .session
                .connection
                .execute(Command::Ioctl, &request, Some(tree.tree_id))
                .await
                .map_err(smb_error)?;
            require_status(&frame, Command::Ioctl)
        }
        .await;
        let close = tree
            .close_handle(&mut state.session.connection, id)
            .await
            .map_err(smb_error);
        result.and(close)
    })
}
fn seed_file(
    provider: &SmbVfs,
    physical: &Path,
    virtual_path: &Path,
    case: &str,
) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(physical)?;
    file.set_len(if case == "empty" { 0 } else { SIZE })?;
    if matches!(case, "islands" | "unmarked") {
        for offset in [4096, 128 * 1024, 512 * 1024] {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&vec![0x41; 4096])?;
        }
    } else if case == "dense" {
        file.write_all(&vec![0x42; SIZE as usize])?;
    }
    file.sync_all()?;
    if case != "unmarked" {
        mark(provider, virtual_path)?;
    }
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
    )?;
    Ok(())
}
fn options(mode: SyncMode) -> SyncOptions {
    SyncOptions {
        mode,
        compare: Compare::ContentHash,
        metadata: MetadataProfile::NativeBestEffort,
        ..Default::default()
    }
}
fn sparse_issues(report: &crate::MetadataReport) -> Vec<&crate::MetadataIssue> {
    report
        .issues
        .iter()
        .filter(|issue| issue.class == crate::MetadataClass::SparseExtents)
        .collect()
}
fn run(
    queue: &OperationQueue,
    provider: &Arc<SmbVfs>,
    source: &Path,
    target: &Path,
    area: &Path,
    mode: SyncMode,
    label: &str,
) -> io::Result<crate::SyncStats> {
    let prepared = crate::prepare_content_sync_for_queue(
        queue,
        ProviderEndpoint::smb(provider.clone(), source)?,
        ProviderEndpoint::local(target)?,
        options(mode),
        &Cancel::new(),
    )?;
    let preview = prepared.preview().clone();
    queue.enqueue(prepared.queue_operation())?;
    let entry = queue.claim_next()?.unwrap();
    TEST_READ_BYTES.with(|value| value.set(0));
    let QueueExecutionResult::Sync(stats) = crate::execute_prepared_content_sync_queue_entry(
        queue,
        &entry,
        prepared,
        true,
        &area.join("audit.jsonl"),
        &Cancel::new(),
        &mut |_| {},
    )?
    else {
        panic!()
    };
    assert_eq!(
        queue.snapshot().entries.last().unwrap().state,
        QueueState::Completed
    );
    assert_eq!(
        sparse_issues(&preview.plan.metadata_report),
        sparse_issues(&stats.metadata_report)
    );
    println!(
        "SPARSE_ACTUAL_QUEUE_OK case={label} files={} logical_bytes={} sparse_read_bytes={}",
        stats.files_copied,
        stats.bytes_copied,
        TEST_READ_BYTES.with(|value| value.get())
    );
    Ok(stats)
}
fn new_area() -> io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
}
fn writable_control(provider: &SmbVfs, path: &Path) -> io::Result<()> {
    let remote = provider.remote_path(path)?;
    let mut state = lock(&provider.state);
    let tree = state.session.tree.clone();
    provider.runtime.block_on(async {
        let id = open_handle(
            &mut state.session.connection,
            &tree,
            &remote,
            FileAccessMask::FILE_WRITE_DATA,
            CreateDisposition::FileOpen,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        )
        .await?;
        tree.close_handle(&mut state.session.connection, id)
            .await
            .map_err(smb_error)
    })
}
#[test]
#[ignore = "frischer künstlicher Samba mit Sparse-Fähigkeit und starkem lokalen Ziel"]
fn sparse_source_actual_queue_to_local() -> io::Result<()> {
    let (provider, physical) = fixture()?;
    std::fs::create_dir(physical.join("source"))?;
    let source = Path::new("/source");
    let cases = ["dense", "empty", "holes", "islands", "unmarked"];
    for case in cases {
        seed_file(
            &provider,
            &physical.join("source").join(case),
            &source.join(case),
            case,
        )?;
    }
    let scope = provider.content_sync_reader(source, &Cancel::new(), None)?;
    assert_eq!(
        scope.metadata_capabilities(source)?.sparse_extents,
        Capability::ReadOnly
    );
    assert_eq!(
        provider
            .content_sync_mutations(source, &Cancel::new(), None)?
            .metadata_capabilities(source)?
            .sparse_extents,
        Capability::Unsupported
    );
    assert_eq!(scope.metadata_manifest(source)?.sparse_extents, None);
    assert_eq!(
        scope
            .metadata_manifest(&source.join("islands"))?
            .sparse_extents,
        Some(vec![
            DataExtent {
                offset: 4096,
                length: 4096
            },
            DataExtent {
                offset: 128 * 1024,
                length: 4096
            },
            DataExtent {
                offset: 512 * 1024,
                length: 4096
            }
        ])
    );
    let mut reader = scope.open_sparse_read(&source.join("islands"))?;
    TEST_CLOSE_CALLS.with(|value| value.set(0));
    assert!(writable_control(&provider, &source.join("islands")).is_err());
    assert_eq!(reader.seek(SeekFrom::End(0))?, SIZE);
    assert_eq!(reader.read(&mut [0; 1])?, 0);
    reader.seek(SeekFrom::Start(4096))?;
    let mut data = [0; 8];
    reader.read_exact(&mut data)?;
    assert_eq!(data, [0x41; 8]);
    reader.seek(SeekFrom::Start(SIZE + 1))?;
    assert_eq!(reader.read(&mut [0; 1])?, 0);
    reader.seek(SeekFrom::Start(0))?;
    assert_eq!(reader.read(&mut [0; 1])?, 1);
    reader.finish()?;
    reader.finish()?;
    assert_eq!(
        reader.seek(SeekFrom::Start(0)).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(
        reader.read(&mut [0; 1]).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    drop(reader);
    assert_eq!(TEST_CLOSE_CALLS.with(|value| value.get()), 1);
    writable_control(&provider, &source.join("islands"))?;
    println!("SPARSE_HELD_SEEK_EOF_RETURN_EXACT_CLOSE_AND_SERVER_RELEASE_OK");
    let area = new_area()?;
    let target = area.path().join("target");
    std::fs::create_dir(&target)?;
    let queue = OperationQueue::open(area.path().join("queue.json"))?;
    for (mode, label) in [
        (SyncMode::AddOnly, "add"),
        (SyncMode::Update, "noop"),
        (SyncMode::Update, "update"),
        (SyncMode::Mirror, "mirror"),
        (SyncMode::Update, "final-noop"),
    ] {
        if label == "update" {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(physical.join("source/islands"))?;
            file.seek(SeekFrom::Start(4096))?;
            file.write_all(b"NEW")?;
            file.sync_all()?;
            file.set_times(std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_005),
            ))?;
        }
        if label == "mirror" {
            std::fs::write(target.join("extra"), b"extra")?;
        }
        let stats = run(&queue, &provider, source, &target, area.path(), mode, label)?;
        assert!(sparse_issues(&stats.metadata_report).is_empty());
        assert_eq!(
            stats.files_copied,
            if label == "add" {
                5
            } else {
                u64::from(label == "update")
            }
        );
        assert_eq!(
            TEST_READ_BYTES.with(|value| value.get()),
            if label == "add" {
                2 * SIZE + 12288
            } else if label == "update" {
                12288
            } else {
                0
            }
        );
        for case in cases {
            assert_eq!(
                std::fs::read(physical.join("source").join(case))?,
                std::fs::read(target.join(case))?
            );
            assert_eq!(
                scope.metadata_manifest(&source.join(case))?.sparse_extents,
                crate::LocalVfs::new()
                    .metadata_manifest(&target.join(case))?
                    .sparse_extents
            );
            assert_eq!(
                provider.stat(&source.join(case))?.mtime,
                crate::LocalVfs::new().stat(&target.join(case))?.mtime
            );
        }
        assert!(!target.join("extra").exists());
    }
    for (name, issue) in [
        ("unsupported", SparseReadIssue::ServerStatus(0xc000_00bb)),
        ("denied", SparseReadIssue::ServerStatus(0xc000_0022)),
        ("limit", SparseReadIssue::BufferLimit),
        ("malformed", SparseReadIssue::InvalidResponse),
    ] {
        let target = area.path().join(name);
        std::fs::create_dir(&target)?;
        TEST_ISSUE.with(|value| value.set(Some(issue)));
        for (mode, step) in [(SyncMode::AddOnly, "add"), (SyncMode::Update, "noop")] {
            let stats = run(
                &queue,
                &provider,
                source,
                &target,
                area.path(),
                mode,
                &format!("{name}-{step}"),
            )?;
            let issues = sparse_issues(&stats.metadata_report);
            assert_eq!(issues.len(), 4);
            assert!(issues.iter().all(|entry| entry.kind
                == if name == "denied" {
                    crate::MetadataIssueKind::PrivilegeFailure
                } else {
                    crate::MetadataIssueKind::Unsupported
                }));
            assert_eq!(stats.files_copied, if step == "add" { 5 } else { 0 });
            assert_eq!(TEST_READ_BYTES.with(|value| value.get()), 0);
        }
        TEST_ISSUE.with(|value| value.set(None));
    }
    std::fs::create_dir(physical.join("failure-source"))?;
    let failure_source = Path::new("/failure-source");
    seed_file(
        &provider,
        &physical.join("failure-source/file.bin"),
        &failure_source.join("file.bin"),
        "islands",
    )?;
    for finish_case in ["held-drift", "cancelled-finish"] {
        let cancel = Cancel::new();
        let scope = provider.content_sync_reader(failure_source, &cancel, None)?;
        let mut reader = scope.open_sparse_read(&failure_source.join("file.bin"))?;
        TEST_CLOSE_CALLS.with(|value| value.set(0));
        if finish_case == "held-drift" {
            std::fs::File::options()
                .write(true)
                .open(physical.join("failure-source/file.bin"))?
                .set_len(SIZE + 4096)?;
        } else {
            cancel.cancel();
        }
        let error = reader.finish().unwrap_err();
        assert_eq!(
            crate::error_code(&error),
            if finish_case == "held-drift" {
                crate::ErrorCode::PlanDrift
            } else {
                crate::ErrorCode::Interrupted
            }
        );
        assert_eq!(
            reader.finish().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        drop(reader);
        assert_eq!(TEST_CLOSE_CALLS.with(|value| value.get()), 1);
        writable_control(&provider, &failure_source.join("file.bin"))?;
        std::fs::File::options()
            .write(true)
            .open(physical.join("failure-source/file.bin"))?
            .set_len(SIZE)?;
        println!("SPARSE_HELD_FINISH_FAILURE_EXACT_CLOSE_SERVER_RELEASE_OK case={finish_case}");
    }
    for failure in ["read", "close", "cancel", "drift"] {
        let failure_area = new_area()?;
        let queue = OperationQueue::open(failure_area.path().join("queue.json"))?;
        let target = failure_area.path().join(format!("failure-{failure}"));
        std::fs::create_dir(&target)?;
        std::fs::write(target.join("file.bin"), b"OLD")?;
        std::fs::File::open(target.join("file.bin"))?.set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000),
            ),
        )?;
        let cancel = Cancel::new();
        let prepared = crate::prepare_content_sync_for_queue(
            &queue,
            ProviderEndpoint::smb(provider.clone(), failure_source)?,
            ProviderEndpoint::local(&target)?,
            options(SyncMode::Update),
            &cancel,
        )?;
        if failure == "drift" {
            std::fs::File::options()
                .write(true)
                .open(physical.join("failure-source/file.bin"))?
                .set_len(SIZE + 4096)?;
        }
        TEST_FINISH_FAULT.with(|value| {
            value.set(match failure {
                "read" => Some("read"),
                "close" => Some("close"),
                _ => None,
            })
        });
        queue.enqueue(prepared.queue_operation())?;
        let entry = queue.claim_next()?.unwrap();
        let result = crate::execute_prepared_content_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            true,
            &area.path().join("audit.jsonl"),
            &cancel,
            &mut |progress| {
                if failure == "cancel" && progress.bytes_done > 0 {
                    cancel.cancel();
                }
            },
        );
        TEST_FINISH_FAULT.with(|value| value.set(None));
        if failure == "cancel" {
            match result {
                Ok(QueueExecutionResult::Sync(stats)) => assert!(stats.cancelled),
                Err(error) => assert_eq!(crate::error_code(&error), crate::ErrorCode::Interrupted),
                _ => panic!(),
            }
        } else {
            let error = result.unwrap_err();
            if failure == "drift" {
                assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
            }
        }
        assert_eq!(std::fs::read(target.join("file.bin"))?, b"OLD");
        assert_eq!(std::fs::read_dir(&target)?.count(), 1);
        println!("SPARSE_ACTUAL_QUEUE_FAILURE_TARGET_UNCHANGED_OK case={failure}");
        if failure == "drift" {
            std::fs::File::options()
                .write(true)
                .open(physical.join("failure-source/file.bin"))?
                .set_len(SIZE)?;
        }
    }
    Ok(())
}

#[test]
#[ignore = "frischer künstlicher Samba; feinere Bereichsantwort ausdrücklich emuliert"]
fn narrow_ranges_content_sync_queue_roundtrip() -> io::Result<()> {
    let (provider, physical) = fixture()?;
    std::fs::create_dir(physical.join("source"))?;
    let source = Path::new("/source");
    let mut bytes = vec![0u8; 8192];
    bytes[512..1024].fill(0x41);
    std::fs::write(physical.join("source/file.bin"), &bytes)?;
    mark(&provider, &source.join("file.bin"))?;
    let area = new_area()?;
    let target = area.path().join("target");
    std::fs::create_dir(&target)?;
    let unit = crate::LocalVfs::new()
        .sparse_allocation_unit(&target)?
        .expect("Native Fixture braucht APFS/ext4/Btrfs");
    assert!(unit > 512);
    let queue = OperationQueue::open(area.path().join("operations-queue.json"))?;
    crate::smb::named_streams::sparse::TEST_EXTENTS.with(|value| {
        *value.borrow_mut() = Some(vec![DataExtent {
            offset: 512,
            length: 512,
        }])
    });
    for (mode, step) in [
        (SyncMode::AddOnly, "add"),
        (SyncMode::Update, "noop"),
        (SyncMode::Update, "update"),
        (SyncMode::Mirror, "mirror"),
        (SyncMode::Update, "final-noop"),
    ] {
        if step == "update" {
            bytes[512..1024].fill(0x42);
            std::fs::write(physical.join("source/file.bin"), &bytes)?;
        }
        std::fs::File::open(physical.join("source/file.bin"))?.set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH
                    + std::time::Duration::from_secs(if matches!(step, "add" | "noop") {
                        1_700_000_000
                    } else {
                        1_700_000_005
                    }),
            ),
        )?;
        if step == "mirror" {
            std::fs::write(target.join("extra"), b"extra")?;
        }
        let stats = run(
            &queue,
            &provider,
            source,
            &target,
            area.path(),
            mode,
            &format!("fine-{step}"),
        )?;
        assert_eq!(sparse_issues(&stats.metadata_report).len(), 1);
        assert_eq!(
            stats.files_copied,
            u64::from(matches!(step, "add" | "update"))
        );
        assert_eq!(std::fs::read(target.join("file.bin"))?, bytes);
        assert_eq!(TEST_READ_BYTES.with(|value| value.get()), 0);
        assert!(!target.join("extra").exists());
        println!("SMB_FINE_SPARSE_LAYOUT_REAL_QUEUE_CONVERGENCE_OK step={step} unit={unit}");
    }
    crate::smb::named_streams::sparse::TEST_EXTENTS.with(|value| *value.borrow_mut() = None);
    Ok(())
}
