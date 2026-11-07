#![no_main]

use doppeldecker_core::archive_read::{ArchiveEntryKind, ArchiveRead, CatalogBudget, ReadBudget};
use doppeldecker_core::Cancel;
use libfuzzer_sys::fuzz_target;
use std::path::Path;

const INPUT_BYTES: usize = 64 * 1024;
const IO_BYTES: u64 = 256 * 1024;
const OUTPUT_BYTES: u64 = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > INPUT_BYTES {
        return;
    }
    let directory = tempfile::tempdir().expect("Künstlichen ZIP-Testroot anlegen");
    std::fs::write(directory.path().join("input.zip"), data)
        .expect("Begrenzte künstliche ZIP-Quelle schreiben");
    let cancel = Cancel::new();
    let Ok(mut archive) = ArchiveRead::open(
        directory.path(),
        Path::new("input.zip"),
        CatalogBudget {
            max_entries: 128,
            max_metadata_bytes: 64 * 1024,
            max_name_bytes: 512,
            max_path_depth: 16,
            max_io_bytes: IO_BYTES,
            max_read_calls: 512,
        },
        &cancel,
    ) else {
        return;
    };
    assert!(archive.entries().len() <= 128);
    if let Ok(root) = archive.directory("") {
        for child in root.directories.iter().take(8) {
            let _ = archive.directory(child);
        }
    }
    // Teilkataloge sind erlaubt. Nur der Erfolgsbeleg eines einzelnen Streams
    // bestätigt dessen Ausgabe; Fehler beliebiger ZIP-Bytes sind erwartbar.
    let reads = archive
        .entries()
        .iter()
        .take(8)
        .map(|entry| {
            (
                entry.id,
                entry.uncompressed_bytes,
                entry.kind == ArchiveEntryKind::File && entry.issues.is_empty(),
            )
        })
        .collect::<Vec<_>>();
    for (id, expected, readable) in reads {
        if let Ok(verified) = archive.read_entry_to(
            id,
            ReadBudget {
                max_output_bytes: OUTPUT_BYTES,
                max_io_bytes: IO_BYTES,
                max_read_calls: 512,
            },
            &cancel,
            &mut std::io::sink(),
        ) {
            assert!(readable);
            assert_eq!(verified.output_bytes, expected);
            assert!(verified.output_bytes <= OUTPUT_BYTES);
            assert!(verified.io_bytes <= IO_BYTES);
        }
    }
});
