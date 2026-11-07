use super::*;
use std::io::{Seek, SeekFrom};

struct Item {
    name: Vec<u8>,
    local_name: Option<Vec<u8>>,
    plain: Vec<u8>,
    encoded: Vec<u8>,
    method: u16,
    flags: u16,
    mode: u32,
    size: Option<u32>,
    crc: Option<u32>,
    alias: bool,
}
impl Item {
    fn stored(name: &[u8], bytes: &[u8]) -> Self {
        Self {
            name: name.into(),
            local_name: None,
            plain: bytes.into(),
            encoded: bytes.into(),
            method: 0,
            flags: 0,
            mode: 0o100644,
            size: None,
            crc: None,
            alias: false,
        }
    }
    fn deflated(name: &[u8], bytes: &[u8]) -> Self {
        let mut item = Self::stored(name, bytes);
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        item.encoded = encoder.finish().unwrap();
        item.method = 8;
        item
    }
}
fn u16le(out: &mut Vec<u8>, value: u16) {
    out.extend(value.to_le_bytes());
}
fn u32le(out: &mut Vec<u8>, value: u32) {
    out.extend(value.to_le_bytes());
}
fn checksum(bytes: &[u8]) -> u32 {
    let mut crc = rawzip::Crc32::new();
    crc.update(bytes);
    crc.checksum()
}
// Testproduzent mit expliziten Gegenfällen, kein produktiver ZIP-Parser.
fn zip(items: &[Item]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for item in items {
        let offset = if item.alias { 0 } else { out.len() as u32 };
        let crc = item.crc.unwrap_or_else(|| checksum(&item.plain));
        let size = item.size.unwrap_or(item.plain.len() as u32);
        let name = item.local_name.as_ref().unwrap_or(&item.name);
        if !item.alias {
            u32le(&mut out, 0x04034b50);
            for v in [20, item.flags, item.method, 0, 0] {
                u16le(&mut out, v);
            }
            for v in [crc, item.encoded.len() as u32, size] {
                u32le(&mut out, v);
            }
            u16le(&mut out, name.len() as u16);
            u16le(&mut out, 0);
            out.extend(name);
            out.extend(&item.encoded);
            if item.flags & 8 != 0 {
                for v in [0x08074b50, crc, item.encoded.len() as u32, size] {
                    u32le(&mut out, v);
                }
            }
        }
        u32le(&mut central, 0x02014b50);
        for v in [(3 << 8) | 20, 20, item.flags, item.method, 0, 0] {
            u16le(&mut central, v);
        }
        for v in [crc, item.encoded.len() as u32, size] {
            u32le(&mut central, v);
        }
        for v in [item.name.len() as u16, 0, 0, 0, 0] {
            u16le(&mut central, v);
        }
        u32le(&mut central, item.mode << 16);
        u32le(&mut central, offset);
        central.extend(&item.name);
    }
    let offset = out.len() as u32;
    let len = central.len() as u32;
    out.extend(central);
    u32le(&mut out, 0x06054b50);
    for v in [0, 0, items.len() as u16, items.len() as u16] {
        u16le(&mut out, v);
    }
    u32le(&mut out, len);
    u32le(&mut out, offset);
    u16le(&mut out, 0);
    out
}
fn session(items: &[Item]) -> (tempfile::TempDir, ArchiveRead) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.zip"), zip(items)).unwrap();
    let archive = ArchiveRead::open(
        dir.path(),
        Path::new("a.zip"),
        CatalogBudget::default(),
        &Cancel::new(),
    )
    .unwrap();
    (dir, archive)
}
fn read_budget() -> ReadBudget {
    ReadBudget {
        max_output_bytes: 1024 * 1024,
        max_io_bytes: 1024 * 1024,
        max_read_calls: 1000,
    }
}
fn read(archive: &mut ArchiveRead, index: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    archive.read_entry_to(
        archive.entries()[index].id,
        read_budget(),
        &Cancel::new(),
        &mut bytes,
    )?;
    Ok(bytes)
}
#[test]
fn source_reader_binds_descriptor_and_fresh_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.zip");
    std::fs::write(&path, b"original").unwrap();
    let source = Source::bind(dir.path(), Path::new("a.zip"), &Cancel::new()).unwrap();
    let cancel = Cancel::new();
    let reader = BoundedReader {
        source,
        cancel,
        bytes: Cell::new(0),
        calls: Cell::new(0),
        max_bytes: 64,
        max_calls: 4,
    };
    let mut bytes = [0; 8];
    assert_eq!(reader.read_at(&mut bytes, 0).unwrap(), 8);
    assert_eq!(&bytes, b"original");
    std::fs::rename(&path, dir.path().join("old.zip")).unwrap();
    std::fs::write(&path, b"original").unwrap();
    assert_eq!(
        crate::error_code(&reader.read_at(&mut bytes, 0).unwrap_err()),
        ErrorCode::PlanDrift
    );
}
#[test]
fn stored_deflate_duplicates_and_navigation_keep_exact_names_and_ids() {
    let (_dir, mut archive) = session(&[
        Item::stored(b"dir/e\xcc\x81.txt", b"one"),
        Item::deflated(b"dir/e\xcc\x81.txt", b"two"),
        Item::stored(b"other", b"three"),
    ]);
    assert!(archive.is_complete());
    assert_eq!(archive.entries().len(), 3);
    assert!(archive.entries()[0].duplicate_name && archive.entries()[1].duplicate_name);
    assert_eq!(
        archive.entries()[0].name.as_deref(),
        Some("dir/e\u{301}.txt")
    );
    assert_eq!(archive.directory("").unwrap().directories, vec!["dir"]);
    assert_eq!(archive.directory("dir").unwrap().entries.len(), 2);
    assert!(archive.directory("../").is_err());
    assert_eq!(read(&mut archive, 0).unwrap(), b"one");
    assert_eq!(read(&mut archive, 1).unwrap(), b"two");
    let (_other, mut other) = session(&[Item::stored(b"x", b"x")]);
    let id = archive.entries()[0].id;
    assert!(other
        .read_entry_to(id, read_budget(), &Cancel::new(), &mut Vec::new())
        .is_err());
}
#[test]
fn invalid_utf8_traversal_and_type_are_individual_rows() {
    let mut link = Item::stored(b"link", b"target");
    link.mode = 0o120777;
    let mut encrypted = Item::stored(b"secret", b"x");
    encrypted.flags = 1;
    let mut unsupported = Item::stored(b"method", b"x");
    unsupported.method = 99;
    let (_dir, mut archive) = session(&[
        Item::stored(b"\xff", b"a"),
        Item::stored(b"../out", b"b"),
        Item::stored(b"/out", b"b"),
        Item::stored(b"a\\b", b"b"),
        link,
        encrypted,
        unsupported,
        Item::stored(b"valid", b"ok"),
    ]);
    assert!(archive.is_complete());
    assert_eq!(archive.entries()[0].raw_name, b"\xff");
    for i in 0..7 {
        assert!(!archive.entries()[i].issues.is_empty());
        assert!(read(&mut archive, i).is_err());
    }
    assert_eq!(read(&mut archive, 7).unwrap(), b"ok");
}
#[test]
fn long_name_depth_header_mismatch_and_overlap_are_explained() {
    let mut mismatch = Item::stored(b"central", b"a");
    mismatch.local_name = Some(b"local".to_vec());
    let mut alias = Item::stored(b"x", b"a");
    alias.alias = true;
    let (_dir, archive) = session(&[Item::stored(b"x", b"a"), alias, mismatch]);
    assert!(archive.entries()[0]
        .issues
        .contains(&ArchiveEntryIssue::OverlappingRange));
    assert!(archive.entries()[1]
        .issues
        .contains(&ArchiveEntryIssue::OverlappingRange));
    assert!(archive.entries()[2]
        .issues
        .contains(&ArchiveEntryIssue::HeaderMismatch));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.zip"),
        zip(&[Item::stored(b"abcd", b"x"), Item::stored(b"a/b/c", b"x")]),
    )
    .unwrap();
    let archive = ArchiveRead::open(
        dir.path(),
        Path::new("a.zip"),
        CatalogBudget {
            max_name_bytes: 3,
            max_path_depth: 2,
            ..CatalogBudget::default()
        },
        &Cancel::new(),
    )
    .unwrap();
    assert!(archive.entries()[0]
        .issues
        .contains(&ArchiveEntryIssue::NameTooLong));
    assert!(archive.entries()[1]
        .issues
        .contains(&ArchiveEntryIssue::PathTooDeep));
}
#[test]
fn crc_short_and_overlong_content_never_return_verified_success() {
    let mut crc = Item::stored(b"crc", b"abc");
    crc.crc = Some(0);
    let mut short = Item::stored(b"short", b"abc");
    short.size = Some(4);
    let mut long = Item::deflated(b"long", &[b'x'; 20000]);
    long.size = Some(3);
    let (_dir, mut archive) = session(&[crc, short, long]);
    for i in 0..3 {
        assert_eq!(
            crate::error_code(&read(&mut archive, i).unwrap_err()),
            ErrorCode::InvalidData
        );
    }
}
#[test]
fn deflate_requires_stream_end_and_entire_compressed_range() {
    let mut trailing = Item::deflated(b"trailing", b"valid payload");
    trailing.encoded.extend(b"junk");
    let mut truncated = Item::deflated(b"truncated", b"valid payload");
    truncated.encoded.pop();
    let (_dir, mut archive) = session(&[trailing, truncated]);
    assert!(read(&mut archive, 0).is_err());
    assert!(read(&mut archive, 1).is_err());
}
#[test]
fn metadata_io_output_and_request_budgets_are_actual_limits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.zip"),
        zip(&[Item::stored(b"a", b"data"), Item::stored(b"b", b"data")]),
    )
    .unwrap();
    for budget in [
        CatalogBudget {
            max_entries: 1,
            ..CatalogBudget::default()
        },
        CatalogBudget {
            max_metadata_bytes: 48,
            ..CatalogBudget::default()
        },
    ] {
        let archive =
            ArchiveRead::open(dir.path(), Path::new("a.zip"), budget, &Cancel::new()).unwrap();
        assert!(!archive.is_complete());
        assert_eq!(archive.entries().len(), 1);
    }
    assert!(ArchiveRead::open(
        dir.path(),
        Path::new("a.zip"),
        CatalogBudget {
            max_io_bytes: 1,
            ..CatalogBudget::default()
        },
        &Cancel::new()
    )
    .is_err());
    let (_dir, mut archive) = session(&[Item::deflated(b"a", &[0; 20000])]);
    let id = archive.entries()[0].id;
    for budget in [
        ReadBudget {
            max_output_bytes: 100,
            ..read_budget()
        },
        ReadBudget {
            max_io_bytes: 1,
            ..read_budget()
        },
        ReadBudget {
            max_read_calls: 1,
            ..read_budget()
        },
    ] {
        assert!(archive
            .read_entry_to(id, budget, &Cancel::new(), &mut Vec::new())
            .is_err());
    }
}
#[test]
fn source_content_parent_symlink_and_source_swaps_are_rejected() {
    let (dir, mut archive) = session(&[Item::stored(b"x", b"old")]);
    std::fs::write(dir.path().join("a.zip"), zip(&[Item::stored(b"x", b"new")])).unwrap();
    assert_eq!(
        crate::error_code(&read(&mut archive, 0).unwrap_err()),
        ErrorCode::PlanDrift
    );
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(
        dir.path().join("sub/a.zip"),
        zip(&[Item::stored(b"x", b"x")]),
    )
    .unwrap();
    let mut archive = ArchiveRead::open(
        dir.path(),
        Path::new("sub/a.zip"),
        CatalogBudget::default(),
        &Cancel::new(),
    )
    .unwrap();
    std::fs::rename(dir.path().join("sub"), dir.path().join("old")).unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(
        dir.path().join("sub/a.zip"),
        zip(&[Item::stored(b"x", b"x")]),
    )
    .unwrap();
    assert!(read(&mut archive, 0).is_err());
    std::os::unix::fs::symlink(dir.path().join("old/a.zip"), dir.path().join("link.zip")).unwrap();
    assert!(ArchiveRead::open(
        dir.path(),
        Path::new("link.zip"),
        CatalogBudget::default(),
        &Cancel::new()
    )
    .is_err());
}
#[test]
fn cancel_and_sink_error_never_return_a_verified_read() {
    let cancel = Cancel::new();
    cancel.cancel();
    assert!(ArchiveRead::open(
        Path::new("/nonexistent"),
        Path::new("x"),
        CatalogBudget::default(),
        &cancel
    )
    .is_err());
    let (_dir, mut archive) = session(&[Item::deflated(b"x", &[1; 20000])]);
    let id = archive.entries()[0].id;
    struct Cancelling(Cancel);
    impl Write for Cancelling {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.cancel();
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let cancel = Cancel::new();
    let error = archive
        .read_entry_to(id, read_budget(), &cancel, &mut Cancelling(cancel.clone()))
        .unwrap_err();
    assert_eq!(crate::error_code(&error), ErrorCode::Interrupted);
    struct Failing;
    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::StorageFull.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert_eq!(
        archive
            .read_entry_to(id, read_budget(), &Cancel::new(), &mut Failing)
            .unwrap_err()
            .kind(),
        io::ErrorKind::StorageFull
    );
}
#[test]
fn data_descriptor_does_not_create_a_false_overlap_barrier() {
    let mut a = Item::deflated(b"a", b"hello");
    a.flags = 8;
    let (_dir, mut archive) = session(&[a, Item::stored(b"b", b"second")]);
    assert!(archive.entries().iter().all(|e| e.issues.is_empty()));
    assert_eq!(read(&mut archive, 0).unwrap(), b"hello");
    assert_eq!(read(&mut archive, 1).unwrap(), b"second");
}
#[test]
fn zip64_sparse_extension_does_not_consume_archive_sized_resources() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = zip(&[Item::stored(b"x", b"payload")]);
    let end = bytes.split_off(bytes.len() - 22);
    let directory_size = u32::from_le_bytes(end[12..16].try_into().unwrap()) as u64;
    let directory_offset = u32::from_le_bytes(end[16..20].try_into().unwrap()) as u64;
    let mut file = File::create(dir.path().join("a.zip")).unwrap();
    file.write_all(&bytes).unwrap();
    let record_offset = bytes.len() as u64;
    let sector = 64 * 1024 * 1024u64;
    file.write_all(&0x06064b50u32.to_le_bytes()).unwrap();
    file.write_all(&(44 + sector).to_le_bytes()).unwrap();
    for n in [45u16, 45] {
        file.write_all(&n.to_le_bytes()).unwrap();
    }
    for n in [0u32, 0] {
        file.write_all(&n.to_le_bytes()).unwrap();
    }
    for n in [1u64, 1, directory_size, directory_offset] {
        file.write_all(&n.to_le_bytes()).unwrap();
    }
    file.seek(SeekFrom::Current(sector as i64)).unwrap();
    file.write_all(&0x07064b50u32.to_le_bytes()).unwrap();
    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.write_all(&record_offset.to_le_bytes()).unwrap();
    file.write_all(&1u32.to_le_bytes()).unwrap();
    let mut end = end;
    end[8..12].fill(255);
    end[12..20].fill(255);
    file.write_all(&end).unwrap();
    drop(file);
    let mut archive = ArchiveRead::open(
        dir.path(),
        Path::new("a.zip"),
        CatalogBudget {
            max_io_bytes: 1024 * 1024,
            ..CatalogBudget::default()
        },
        &Cancel::new(),
    )
    .unwrap();
    assert!(archive.is_complete());
    assert!(archive.archive.get_ref().bytes.get() < 1024 * 1024);
    assert_eq!(read(&mut archive, 0).unwrap(), b"payload");
}

#[test]
fn invalid_entry_offset_does_not_hide_independent_content() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = zip(&[Item::stored(b"bad", b"x"), Item::stored(b"good", b"ok")]);
    let end = bytes.len() - 22;
    let central = u32::from_le_bytes(bytes[end + 16..end + 20].try_into().unwrap()) as usize;
    bytes[central + 42..central + 46].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(dir.path().join("a.zip"), bytes).unwrap();
    let mut archive = ArchiveRead::open(
        dir.path(),
        Path::new("a.zip"),
        CatalogBudget::default(),
        &Cancel::new(),
    )
    .unwrap();
    assert!(archive.is_complete());
    assert!(!archive.entries()[0].issues.is_empty());
    assert_eq!(read(&mut archive, 1).unwrap(), b"ok");
}

#[test]
fn source_change_from_sink_is_caught_before_verified_success() {
    let (dir, mut archive) = session(&[Item::stored(b"x", b"old")]);
    struct Changing(PathBuf);
    impl Write for Changing {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            std::fs::write(&self.0, zip(&[Item::stored(b"x", b"new")]))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let id = archive.entries()[0].id;
    let error = archive
        .read_entry_to(
            id,
            read_budget(),
            &Cancel::new(),
            &mut Changing(dir.path().join("a.zip")),
        )
        .unwrap_err();
    assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
    assert_eq!(
        error.to_string(),
        "ZIP-Quelle wurde geändert oder ausgetauscht"
    );
}

#[test]
fn directory_source_is_rejected_before_visiting_unreadable_children() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let child = dir.path().join("folder/unreadable");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(child.join("not-an-archive"), b"private fixture").unwrap();
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o0)).unwrap();
    let result = Source::bind(dir.path(), Path::new("folder"), &Cancel::new());
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.err().expect("Ordner ist keine ZIP-Dateiquelle");
    assert_eq!(crate::error_code(&error), ErrorCode::InvalidData);
    assert_eq!(error.to_string(), "ZIP-Quelle ist keine reguläre Datei");
}

#[test]
fn source_binding_honors_caller_cancellation_before_filesystem_work() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.zip"), b"fixture").unwrap();
    let cancel = Cancel::new();
    cancel.cancel();
    for relative in ["a.zip", "missing.zip"] {
        let error = Source::bind(dir.path(), Path::new(relative), &cancel)
            .err()
            .expect("Quellenbindung muss Aufruferabbruch beachten");
        assert_eq!(crate::error_code(&error), ErrorCode::Interrupted);
    }
}
