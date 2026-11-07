use doppeldecker_core::{BidirectionalBaseline, BidirectionalRunJournal, MetadataManifest};

#[test]
fn zip_fuzz_seeds_oeffnen_navigation_und_gepruefte_inhaltsstreams() {
    use doppeldecker_core::archive_read::{
        ArchiveEntryKind, ArchiveRead, CatalogBudget, ReadBudget,
    };
    use doppeldecker_core::Cancel;
    use std::path::Path;

    let deflated = b"Deflate ZIP\n".repeat(64);
    let seeds: &[(&[u8], &[&[u8]], bool)] = &[
        (
            include_bytes!("../seeds/archive_read/stored.zip"),
            &[b"Stored ZIP\n", b"UTF8\n"],
            false,
        ),
        (
            include_bytes!("../seeds/archive_read/deflate.zip"),
            &[&deflated],
            false,
        ),
        (
            include_bytes!("../seeds/archive_read/duplicates.zip"),
            &[b"first\n", b"second\n"],
            true,
        ),
    ];
    for (bytes, expected, duplicates) in seeds {
        assert!(bytes.len() <= 64 * 1024);
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("seed.zip"), bytes).unwrap();
        let cancel = Cancel::new();
        let mut archive = ArchiveRead::open(
            directory.path(),
            Path::new("seed.zip"),
            CatalogBudget {
                max_entries: 128,
                max_metadata_bytes: 64 * 1024,
                max_name_bytes: 512,
                max_path_depth: 16,
                max_io_bytes: 256 * 1024,
                max_read_calls: 512,
            },
            &cancel,
        )
        .unwrap();
        assert!(archive.is_complete());
        assert!(archive
            .entries()
            .iter()
            .all(|entry| entry.issues.is_empty()));
        assert_eq!(
            archive.entries().iter().any(|entry| entry.duplicate_name),
            *duplicates
        );
        for child in archive.directory("").unwrap().directories {
            archive.directory(&child).unwrap();
        }
        let ids = archive
            .entries()
            .iter()
            .filter(|entry| entry.kind == ArchiveEntryKind::File)
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), expected.len());
        assert!(ids.len() <= 8);
        for (id, expected) in ids.into_iter().zip(*expected) {
            let mut output = Vec::new();
            let verified = archive
                .read_entry_to(
                    id,
                    ReadBudget {
                        max_output_bytes: 64 * 1024,
                        max_io_bytes: 256 * 1024,
                        max_read_calls: 512,
                    },
                    &cancel,
                    &mut output,
                )
                .unwrap();
            assert_eq!(output, *expected);
            assert_eq!(verified.output_bytes, expected.len() as u64);
            assert!(verified.io_bytes <= 256 * 1024);
        }
    }
}

#[test]
fn versionierte_fuzz_seeds_bleiben_schema_und_digestgueltig() {
    let baseline: BidirectionalBaseline =
        serde_json::from_slice(include_bytes!("../seeds/bidirectional_baseline/valid.json"))
            .unwrap();
    baseline.validate().unwrap();

    let journal: BidirectionalRunJournal = serde_json::from_slice(include_bytes!(
        "../seeds/bidirectional_run_journal/valid.json"
    ))
    .unwrap();
    journal.validate().unwrap();

    let manifest: MetadataManifest =
        serde_json::from_slice(include_bytes!("../seeds/metadata_manifest/valid.json")).unwrap();
    manifest.validate().unwrap();
}
