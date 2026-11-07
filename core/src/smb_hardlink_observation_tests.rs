//! Reale Linkzahlbeobachtung, ohne aus ihr eine vollständige Aliasgruppe zu erfinden.
use super::*;

#[test]
#[ignore = "benötigt frischen künstlichen Samba mit acl_xattr und streams_xattr"]
fn reported_links_remain_visible_in_actual_queue_noops() -> io::Result<()> {
    let (provider, physical) = fixture()?;
    for case in ["inside", "outside", "single"] {
        std::fs::create_dir_all(physical.join(case).join("source"))?;
        std::fs::create_dir(physical.join(case).join("target"))?;
        std::fs::create_dir(physical.join(case).join("outside"))?;
        let source = Path::new("/").join(case).join("source");
        let target = Path::new("/").join(case).join("target");
        let area = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let (mut acl, context) = seed(&provider, &source, area.path(), b"linked-source", 6)?;
        let original = physical.join(case).join("source/file.bin");
        if case != "single" {
            std::fs::hard_link(
                &original,
                physical.join(case).join(if case == "inside" {
                    "source/alias.bin"
                } else {
                    "outside/alias.bin"
                }),
            )?;
        }
        let count = if case == "single" { 1 } else { 2 };
        let names = if case == "inside" {
            vec!["alias.bin", "file.bin"]
        } else {
            vec!["file.bin"]
        };
        let reader = provider.content_sync_reader(&source, &Cancel::new(), None)?;
        assert_eq!(
            reader.metadata_capabilities(&source)?.hardlinks,
            crate::metadata::Capability::Unsupported
        );
        assert_eq!(reader.metadata_manifest(&source)?.reported_hard_links, None);
        for name in &names {
            let entry = provider.stat(&source.join(name))?;
            assert_eq!(entry.hard_links, Some(count.into()));
            assert_eq!(entry.device, None);
            let manifest = reader.metadata_manifest(&source.join(name))?;
            assert_eq!(manifest.reported_hard_links, Some(count));
            assert_eq!(manifest.hardlink_group, None);
        }
        let queue = OperationQueue::open(area.path().join("queue.json"))?;
        for step in ["add", "noop", "update", "acl-only", "mirror", "final-noop"] {
            let bytes: &[u8] = if matches!(step, "add" | "noop") {
                b"linked-source"
            } else {
                b"linked-source-updated"
            };
            if step == "update" {
                std::fs::write(&original, bytes)?;
                std::fs::File::open(&original)?.set_times(
                    std::fs::FileTimes::new().set_modified(
                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_003),
                    ),
                )?;
            }
            if step == "mirror" {
                write_probe(&provider, &target.join("extra"), b"extra")?;
            }
            if step == "acl-only" {
                acl = desired(&context, 2);
                set_external_acl(&provider, &source.join("file.bin"), &acl, &context)?;
            }
            let mode = if step == "add" {
                SyncMode::AddOnly
            } else if step == "mirror" {
                SyncMode::Mirror
            } else {
                SyncMode::Update
            };
            let prepared = crate::prepare_content_sync_for_queue(
                &queue,
                ProviderEndpoint::smb(provider.clone(), &source)?,
                ProviderEndpoint::smb(provider.clone(), &target)?,
                SyncOptions {
                    mode,
                    compare: Compare::ContentHash,
                    metadata: MetadataProfile::NativeBestEffort,
                    ..Default::default()
                },
                &Cancel::new(),
            )?;
            let expected = if case == "inside" { names.len() } else { 0 };
            let report = &prepared.preview().plan.metadata_report;
            assert_eq!(
                report
                    .issues
                    .iter()
                    .filter(|i| i.class == crate::sync::MetadataClass::Hardlinks)
                    .count(),
                expected,
                "case={case} step={step}: {report:?}"
            );
            let copies = if matches!(step, "add" | "update") {
                names.len() as u64
            } else {
                0
            };
            assert_eq!(
                prepared.preview().plan.copy_count + prepared.preview().plan.update_count,
                copies
            );
            assert_eq!(prepared.preview().plan.metadata_report.hardlink_actions, 0);
            if matches!(step, "noop" | "final-noop") {
                assert!(prepared.preview().plan.actions.is_empty());
            }
            queue.enqueue(prepared.queue_operation())?;
            let entry = queue.claim_next()?.unwrap();
            let QueueExecutionResult::Sync(stats) =
                crate::execute_prepared_content_sync_queue_entry(
                    &queue,
                    &entry,
                    prepared,
                    true,
                    &area.path().join("audit.jsonl"),
                    &Cancel::new(),
                    &mut |_| {},
                )?
            else {
                panic!()
            };
            assert_eq!(stats.files_copied, copies);
            if step == "acl-only" {
                assert_eq!(stats.file_dacl_report.completed.len(), names.len());
            }
            assert_eq!(
                stats
                    .metadata_report
                    .issues
                    .iter()
                    .filter(|i| i.class == crate::sync::MetadataClass::Hardlinks)
                    .count(),
                expected
            );
            assert_eq!(
                queue.snapshot().entries.last().unwrap().state,
                QueueState::Completed
            );
            for name in &names {
                let target_file = target.join(name);
                verify_file(
                    &provider,
                    &target,
                    &target_file,
                    bytes,
                    &acl,
                    &context,
                    provider.stat(&target_file)?.inode.unwrap(),
                )?;
            }
            if case == "inside" {
                assert_eq!(
                    provider.stat(&source.join("file.bin"))?.inode,
                    provider.stat(&source.join("alias.bin"))?.inode
                );
                assert_ne!(
                    provider.stat(&target.join("file.bin"))?.inode,
                    provider.stat(&target.join("alias.bin"))?.inode
                );
            }
            if step == "mirror" {
                assert!(
                    matches!(provider.stat(&target.join("extra")), Err(e) if e.kind()==io::ErrorKind::NotFound)
                );
            }
            println!("REPORTED_LINKS_EXACT_QUEUE_CLASS_NO_INVENTED_GROUP_NOOP_OK case={case} step={step}");
        }
    }
    Ok(())
}
