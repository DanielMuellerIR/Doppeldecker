use super::*;

const BLOCK: u64 = 512;

const MAX_EXPANDED_BYTES: u64 = 64 * 1024 * 1024;

struct Sequential<'a> {
    reader: &'a BoundedReader,
    offset: u64,
}

impl Read for Sequential<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.reader.read_at(buffer, self.offset)?;
        self.offset += count as u64;
        Ok(count)
    }
}

pub(super) fn expand_gzip(
    reader: &BoundedReader,
    budget: u64,
    cancel: &Cancel,
) -> io::Result<Vec<u8>> {
    let mut gzip = flate2::read::MultiGzDecoder::new(Sequential { reader, offset: 0 });
    let mut expanded = Vec::new();
    let mut block = [0; 32 * 1024];
    let upper_bound = budget.min(MAX_EXPANDED_BYTES) as usize;
    loop {
        check_cancel(cancel)?;
        let count = gzip.read(&mut block)?;
        if count == 0 {
            break;
        }
        if count > upper_bound.saturating_sub(expanded.len()) {
            return Err(coded_error(
                io::ErrorKind::InvalidInput,
                ErrorCode::InvalidInput,
                "Entpacktes TAR überschreitet das Lesebudget von höchstens 64 MiB",
            ));
        }
        let needed = expanded.len() + count;
        if needed > expanded.capacity() {
            let capacity = (2 * expanded.capacity()).min(upper_bound).max(needed);
            expanded
                .try_reserve_exact(capacity - expanded.len())
                .map_err(io::Error::other)?;
        }
        expanded.extend_from_slice(&block[..count]);
    }
    reader.source.verify()?;
    check_cancel(cancel)?;
    Ok(expanded)
}

fn fill_block(reader: &BoundedReader, offset: u64) -> io::Result<[u8; 512]> {
    let mut block = [0; 512];
    let mut read = 0;
    while read < block.len() {
        let count = reader.read_at(&mut block[read..], offset + read as u64)?;
        if count == 0 {
            return Err(invalid("TAR-Kopf oder Abschlussblock ist unvollständig"));
        }
        read += count;
    }
    Ok(block)
}

fn parse_header(block: &[u8; 512]) -> io::Result<&tar::Header> {
    let header = tar::Header::from_byte_slice(block);
    let checksum: u32 = block[..148]
        .iter()
        .chain(&block[156..])
        .map(|byte| u32::from(*byte))
        .sum::<u32>()
        + 8 * u32::from(b' ');
    if header.cksum().ok() != Some(checksum) {
        return Err(invalid("TAR-Kopf besitzt eine ungültige Prüfsumme"));
    }
    Ok(header)
}

#[derive(Default)]
struct NextHeader {
    extension_seen: bool,
    name: Option<Vec<u8>>,
    size: Option<u64>,
}

impl NextHeader {
    fn set_name(&mut self, value: &[u8]) -> io::Result<()> {
        if value.is_empty() || value.contains(&0) || self.name.is_some() {
            return Err(invalid(
                "TAR-Erweiterung enthält mehrere oder ungültige Pfade",
            ));
        }
        self.name = Some(value.to_vec());
        Ok(())
    }

    fn apply(&mut self, kind: tar::EntryType, bytes: &[u8]) -> io::Result<()> {
        if kind.is_pax_global_extensions() {
            if self.extension_seen {
                return Err(invalid("Globaler PAX-Kopf nach lokalem Erweiterungskopf"));
            }
            return read_pax(bytes, |key, value| self.pax(key, value, true));
        }
        self.extension_seen = true;
        if kind.is_pax_local_extensions() {
            return read_pax(bytes, |key, value| self.pax(key, value, false));
        }
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        if end == 0 || bytes[end..].iter().any(|byte| *byte != 0) {
            return Err(invalid("Ungültiger GNU-Pfad im TAR-Erweiterungskopf"));
        }
        if kind.is_gnu_longname() {
            self.set_name(&bytes[..end])?;
        }
        // GNU LongLink belegt nur das Linkziel. Der Link selbst bleibt im Katalog
        // ein nicht ausführbarer Spezialtyp.
        Ok(())
    }

    fn pax(&mut self, key: &str, value: &[u8], global: bool) -> io::Result<()> {
        match key {
            "path" if !global => self.set_name(value)?,
            "size" if !global && self.size.is_none() => {
                self.size =
                    Some(parse_decimal(value).ok_or_else(|| invalid("Ungültige PAX-Dateigröße"))?);
            }
            "linkpath" if !global => {}
            "mtime"
            | "atime"
            | "ctime"
            | "uid"
            | "gid"
            | "uname"
            | "gname"
            | "charset"
            | "hdrcharset"
            | "comment"
            | "LIBARCHIVE.creationtime"
            | "SCHILY.fflags" => {}
            key if key.starts_with("LIBARCHIVE.xattr.")
                || key.starts_with("SCHILY.xattr.")
                || key.starts_with("SCHILY.acl.") => {}
            _ => return Err(unsupported_extension()),
        }
        Ok(())
    }
}

fn parse_decimal(value: &[u8]) -> Option<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn read_pax(
    mut bytes: &[u8],
    mut apply: impl FnMut(&str, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    while !bytes.is_empty() {
        let space = bytes
            .iter()
            .take(21)
            .position(|byte| *byte == b' ')
            .ok_or_else(|| invalid("Ungültige PAX-Satzlänge"))?;
        let len = parse_decimal(&bytes[..space])
            .and_then(|len| usize::try_from(len).ok())
            .filter(|len| *len > space + 3 && *len <= bytes.len())
            .ok_or_else(|| invalid("PAX-Satz liegt außerhalb seines Kopfs"))?;
        let record = &bytes[space + 1..len - 1];
        if bytes[len - 1] != b'\n' {
            return Err(invalid("PAX-Satzende fehlt"));
        }
        let equals = record
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or_else(|| invalid("PAX-Satz enthält keinen Wert"))?;
        let key = std::str::from_utf8(&record[..equals])
            .map_err(|_| invalid("PAX-Feldname ist kein UTF-8"))?;
        apply(key, &record[equals + 1..])?;
        bytes = &bytes[len..];
    }
    Ok(())
}

fn unsupported_extension() -> io::Error {
    coded_error(
        io::ErrorKind::Unsupported,
        ErrorCode::Unsupported,
        "TAR-Katalog endet an einer unbekannten oder nicht unterstützten Erweiterung",
    )
}

fn padded_end(reader: &BoundedReader, data_offset: u64, size: u64) -> io::Result<u64> {
    let padded = size
        .checked_add(BLOCK - 1)
        .ok_or_else(|| invalid("TAR-Länge liegt außerhalb des Archivs"))?
        / BLOCK
        * BLOCK;
    data_offset
        .checked_add(padded)
        .filter(|next| *next <= reader.len())
        .ok_or_else(|| invalid("TAR-Nutzdaten liegen außerhalb des Archivs"))
}

fn extension_bytes(reader: &BoundedReader, offset: u64, size: u64) -> io::Result<Vec<u8>> {
    // Ein einzelner Erweiterungskopf darf auch bei einem größeren
    // Katalogbudget den Speicher nicht mit unzähligen Metadaten belegen.
    const MAX_EXTENSION_BYTES: u64 = 1024 * 1024;
    if size > MAX_EXTENSION_BYTES {
        return Err(budget_error());
    }
    let mut bytes = vec![0; size as usize];
    let mut read = 0;
    while read < bytes.len() {
        let count = reader.read_at(&mut bytes[read..], offset + read as u64)?;
        if count == 0 {
            return Err(invalid("TAR-Erweiterung endet vor der angekündigten Länge"));
        }
        read += count;
    }
    Ok(bytes)
}

pub(super) fn open_tar(
    reader: BoundedReader,
    session: [u8; 16],
    budget: CatalogBudget,
    cancel: &Cancel,
) -> io::Result<ArchiveRead> {
    let mut entries = Vec::<ArchiveCatalogEntry>::new();
    let mut bound = Vec::new();
    let mut names = BTreeMap::<Vec<u8>, usize>::new();
    let mut offset = 0_u64;
    let mut metadata = 0_u64;
    let mut next_header = NextHeader::default();
    let scan = (|| -> io::Result<()> {
        loop {
            let block = fill_block(&reader, offset)?;
            if block.iter().all(|byte| *byte == 0) {
                if next_header.extension_seen {
                    return Err(invalid("TAR-Erweiterung ohne nachfolgenden Eintrag"));
                }
                let last = fill_block(&reader, offset + BLOCK)?;
                if last.iter().any(|byte| *byte != 0) {
                    return Err(invalid("TAR besitzt keinen gültigen Abschluss"));
                }
                verify_padding(&reader, offset + 2 * BLOCK)?;
                break;
            }
            let header = parse_header(&block)?;
            let kind = header.entry_type();
            if kind.is_gnu_sparse() {
                return Err(unsupported_extension());
            }
            let header_size = header
                .entry_size()
                .map_err(|_| invalid("TAR-Größenfeld ist ungültig"))?;
            let extension = kind.is_pax_local_extensions()
                || kind.is_pax_global_extensions()
                || kind.is_gnu_longname()
                || kind.is_gnu_longlink();
            let data_offset = offset + BLOCK;
            let size = if extension {
                header_size
            } else {
                next_header.size.unwrap_or(header_size)
            };
            offset = padded_end(&reader, data_offset, size)?;
            if extension {
                let charge = BLOCK.checked_add(size).ok_or_else(budget_error)?;
                if charge > budget.max_metadata_bytes.saturating_sub(metadata) {
                    return Err(budget_error());
                }
                let bytes = extension_bytes(&reader, data_offset, size)?;
                next_header.apply(kind, &bytes)?;
                metadata += charge;
                continue;
            }
            let raw = match std::mem::take(&mut next_header).name {
                Some(name) => std::borrow::Cow::Owned(name),
                None => header.path_bytes(),
            };
            let charge = BLOCK + raw.len() as u64;
            if entries.len() >= budget.max_entries
                || charge > budget.max_metadata_bytes.saturating_sub(metadata)
            {
                return Err(budget_error());
            }
            metadata += charge;
            // `tar -C Ordner .` setzt einen relativen Archivwurzelpunkt vor gewöhnliche Pfade.
            if kind.is_dir() && matches!(raw.as_ref(), b"." | b"./") && size == 0 {
                continue;
            }
            let visible = raw.strip_prefix(b"./").unwrap_or(&raw);
            let name = std::str::from_utf8(visible).ok().map(str::to_owned);
            let mut issues = Vec::new();
            if raw.len() > budget.max_name_bytes {
                issues.push(ArchiveEntryIssue::NameTooLong);
            }
            match &name {
                None => issues.push(ArchiveEntryIssue::InvalidUtf8),
                Some(name) => {
                    if !path_is_safe(name) || (kind.is_file() && name.ends_with('/')) {
                        issues.push(ArchiveEntryIssue::UnsafePath);
                    }
                    if name.split('/').filter(|part| !part.is_empty()).count()
                        > budget.max_path_depth
                    {
                        issues.push(ArchiveEntryIssue::PathTooDeep);
                    }
                }
            }
            let entry_kind = if kind.is_dir() && size == 0 {
                ArchiveEntryKind::Directory
            } else if kind.is_file() {
                ArchiveEntryKind::File
            } else {
                issues.push(ArchiveEntryIssue::UnsupportedType);
                ArchiveEntryKind::Unsupported
            };
            let index = entries.len();
            let previous = names.get(visible).copied();
            if let Some(previous) = previous {
                entries[previous].duplicate_name = true;
            } else {
                names.insert(visible.to_vec(), index);
            }
            entries.push(ArchiveCatalogEntry {
                id: ArchiveEntryId { session, index },
                raw_name: raw.into_owned(),
                name,
                kind: entry_kind,
                uncompressed_bytes: size,
                compressed_bytes: size,
                duplicate_name: previous.is_some(),
                issues,
            });
            bound.push(BoundEntry::Tar {
                offset: data_offset,
            });
        }
        Ok(())
    })();
    let stop = match scan {
        Ok(()) => None,
        Err(error) => {
            let code = crate::error_code(&error);
            if matches!(
                code,
                ErrorCode::InvalidData | ErrorCode::InvalidInput | ErrorCode::Unsupported
            ) {
                Some(ArchiveCatalogStop {
                    code,
                    message: error.to_string(),
                })
            } else {
                return Err(error);
            }
        }
    };
    reader.source.verify()?;
    check_cancel(cancel)?;
    Ok(ArchiveRead {
        storage: ArchiveStorage::Tar(reader),
        entries,
        bound,
        session,
        stop,
    })
}

fn verify_padding(reader: &BoundedReader, mut offset: u64) -> io::Result<()> {
    let mut bytes = [0; 32 * 1024];
    while offset < reader.len() {
        let read = reader.read_at(&mut bytes, offset)?;
        if read == 0 || bytes[..read].iter().any(|byte| *byte != 0) {
            return Err(invalid(
                "TAR besitzt unerwartete Daten hinter dem Abschluss",
            ));
        }
        offset += read as u64;
    }
    Ok(())
}

pub(super) fn read_payload(
    reader: &BoundedReader,
    offset: u64,
    size: u64,
    sink: &mut dyn Write,
) -> io::Result<VerifiedArchiveRead> {
    let mut buffer = [0; 32 * 1024];
    let mut copied = 0;
    while copied < size {
        let request = (size - copied).min(buffer.len() as u64) as usize;
        let read = reader.read_at(&mut buffer[..request], offset + copied)?;
        if read == 0 {
            return Err(invalid("TAR-Nutzdaten enden vor der angekündigten Länge"));
        }
        sink.write_all(&buffer[..read])?;
        copied += read as u64;
    }
    reader.source.verify()?;
    check_cancel(&reader.cancel)?;
    Ok(VerifiedArchiveRead {
        output_bytes: copied,
        io_bytes: reader.bytes.get(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append(archive: &mut Vec<u8>, name: &[u8], kind: tar::EntryType, data: &[u8]) {
        let mut header = tar::Header::new_ustar();
        header.as_mut_bytes()[..name.len()].copy_from_slice(name);
        header.set_mode(0o644);
        header.set_size(data.len() as u64);
        header.set_entry_type(kind);
        header.set_cksum();
        archive.extend_from_slice(header.as_bytes());
        archive.extend_from_slice(data);
        archive.resize(archive.len().next_multiple_of(BLOCK as usize), 0);
    }

    fn source(bytes: Vec<u8>) -> (tempfile::TempDir, ArchiveRead) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(root.join("test.tar"), bytes).unwrap();
        let archive = ArchiveRead::open_as(
            &root,
            Path::new("test.tar"),
            ArchiveReadFormat::Tar,
            CatalogBudget::default(),
            &Cancel::new(),
        )
        .unwrap();
        (directory, archive)
    }

    #[test]
    fn tar_liest_gebundene_datei_und_bilanziert_unsichere_eintraege() {
        let mut bytes = Vec::new();
        append(&mut bytes, b"./", tar::EntryType::Directory, b"");
        append(
            &mut bytes,
            b"./ordner/A.txt",
            tar::EntryType::Regular,
            b"TAR-Prueftext",
        );
        append(&mut bytes, b"./ordner/link", tar::EntryType::Symlink, b"");
        append(
            &mut bytes,
            b"../versuch.txt",
            tar::EntryType::Regular,
            b"auslassen",
        );
        bytes.extend([0; 2048]);
        let (_root, mut archive) = source(bytes);
        assert!(archive.is_complete());
        assert_eq!(archive.directory("").unwrap().directories, ["ordner"]);
        let entries = archive.entries();
        assert_eq!(entries[1].issues, [ArchiveEntryIssue::UnsupportedType]);
        assert_eq!(entries[2].issues, [ArchiveEntryIssue::UnsafePath]);
        let entry = entries[0].id;
        let mut payload = Vec::new();
        let budget = ReadBudget {
            max_output_bytes: 30,
            max_io_bytes: 30,
            max_read_calls: 4,
        };
        let result = archive
            .read_entry_to(entry, budget, &Cancel::new(), &mut payload)
            .unwrap();
        assert_eq!(payload, b"TAR-Prueftext");
        assert_eq!(result.output_bytes, payload.len() as u64);
        assert!(archive
            .extract_entry_selection(entry, &Cancel::new())
            .is_ok());
    }

    fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
        let body_len = key.len() + 3 + value.len();
        let mut total = body_len;
        while body_len + total.to_string().len() != total {
            total = body_len + total.to_string().len();
        }
        let mut entry = format!("{total} {key}=").into_bytes();
        entry.extend(value);
        entry.push(b'\n');
        entry
    }

    #[test]
    fn tar_bindet_pax_pfad_und_groesse_sowie_gnu_langnamen() {
        let mut bytes = Vec::new();
        let path = "Ordner/Datei mit ö und langem Namen.txt";
        let body = vec![b'x'; 540];
        let mut pax = pax_record("path", path.as_bytes());
        pax.extend(pax_record("size", b"540"));
        pax.extend(pax_record("LIBARCHIVE.xattr.user.test", b"metadata\nonly"));
        append(&mut bytes, b"pax", tar::EntryType::XHeader, &pax);
        let header_offset = bytes.len();
        append(&mut bytes, b"legacy.txt", tar::EntryType::Regular, &body);
        let encoded = &mut bytes[header_offset..header_offset + 512];
        let mut header = tar::Header::from_byte_slice(encoded).clone();
        header.set_size(0);
        header.set_cksum();
        encoded.copy_from_slice(header.as_bytes());
        let gnu = format!("Ordner/{}.txt", "a".repeat(112));
        let long = [gnu.as_bytes(), b"\0"].concat();
        append(&mut bytes, b"LongLink", tar::EntryType::GNULongName, &long);
        append(
            &mut bytes,
            b"shortened.txt",
            tar::EntryType::Regular,
            b"gnu",
        );
        bytes.extend([0; 1024]);
        let (_root, mut archive) = source(bytes);
        assert!(archive.is_complete(), "{:?}", archive.stop_reason());
        let entries = archive.entries();
        assert_eq!(entries[0].raw_name, path.as_bytes());
        assert_eq!(entries[0].uncompressed_bytes, body.len() as u64);
        assert_eq!(entries[1].name.as_deref(), Some(gnu.as_str()));
        let id = entries[0].id;
        let mut result = Vec::new();
        archive
            .read_entry_to(
                id,
                ReadBudget {
                    max_output_bytes: 1024,
                    max_io_bytes: 1024,
                    max_read_calls: 8,
                },
                &Cancel::new(),
                &mut result,
            )
            .unwrap();
        assert_eq!(result, body);
    }

    #[test]
    fn tar_hinter_unvollstaendigem_pax_kopf_bleibt_unbekannt() {
        let mut bytes = Vec::new();
        let mut pax = pax_record("path", b"safe.txt");
        pax.extend(b"9 X=a");
        append(&mut bytes, b"pax", tar::EntryType::XHeader, &pax);
        append(
            &mut bytes,
            b"legacy.txt",
            tar::EntryType::Regular,
            b"not mapped",
        );
        bytes.extend([0; 1024]);
        let (_root, archive) = source(bytes);
        assert!(archive.entries().is_empty());
        assert_eq!(archive.stop_reason().unwrap().code, ErrorCode::InvalidData);
    }

    #[test]
    fn tar_zeigt_nur_belegte_eintraege_bis_zur_formatgrenze() {
        let mut prefix = Vec::new();
        append(&mut prefix, b"first.txt", tar::EntryType::Regular, b"first");
        let mut extended = prefix.clone();
        append(
            &mut extended,
            b"pax",
            tar::EntryType::XHeader,
            b"21 GNU.sparse.size=5\n",
        );
        append(
            &mut extended,
            b"stale.txt",
            tar::EntryType::Regular,
            b"unbound",
        );
        extended.extend([0; 1024]);
        let (_root, archive) = source(extended);
        assert_eq!(archive.entries().len(), 1);
        assert_eq!(archive.stop_reason().unwrap().code, ErrorCode::Unsupported);

        let mut trailing = prefix;
        trailing.extend([0; 1024]);
        trailing.extend(b"hidden trailing bytes");
        let (_root, archive) = source(trailing);
        assert_eq!(archive.entries().len(), 1);
        assert_eq!(archive.stop_reason().unwrap().code, ErrorCode::InvalidData);
    }

    #[test]
    fn kompakter_tar_stream_prueft_gzip_abschluss_und_expansionslimit() {
        let mut bytes = Vec::new();
        append(&mut bytes, b"test.txt", tar::EntryType::Regular, b"kompakt");
        bytes.extend([0; 1024]);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&bytes).unwrap();
        let compressed = encoder.finish().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = Path::new("test.tgz");
        std::fs::write(root.join(path), &compressed).unwrap();
        let open = |budget| {
            ArchiveRead::open_as(
                &root,
                path,
                ArchiveReadFormat::TarGzip,
                budget,
                &Cancel::new(),
            )
        };
        let mut archive = open(CatalogBudget::default()).unwrap();
        assert!(archive.is_complete());
        assert_eq!(archive.format(), ArchiveReadFormat::TarGzip);
        let id = archive.entries()[0].id;
        let mut contents = Vec::new();
        archive
            .read_entry_to(
                id,
                ReadBudget {
                    max_output_bytes: 32,
                    max_io_bytes: 32,
                    max_read_calls: 4,
                },
                &Cancel::new(),
                &mut contents,
            )
            .unwrap();
        assert_eq!(contents, b"kompakt");
        assert!(open(CatalogBudget {
            max_io_bytes: 1024,
            ..CatalogBudget::default()
        })
        .is_err());
        std::fs::write(root.join(path), &compressed[..compressed.len() - 1]).unwrap();
        assert!(open(CatalogBudget::default()).is_err());
    }
}
