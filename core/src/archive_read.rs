//! Rein lesende lokale ZIP-Sicht. Einträge folgen dem Zentralverzeichnis;
//! eine vollständige strukturelle Validierung einschließlich aller Data Descriptors
//! wird nicht behauptet. Dieses Modul erzeugt niemals Dateisystemziele.
//!
//! Der feste Parser-Scratchbedarf beträgt etwa 320 KiB. Die variablen Katalogdaten
//! sind durch Eintragszahl und Summe der zentralen Metadaten begrenzt. Dateigröße
//! allein ist keine Eintrittsgrenze. Namen werden weder dekodiert ersetzt noch
//! normalisiert; unbedienbare Einträge bleiben einzeln im Katalog sichtbar.
//! Ressourcenbudgets gelten für die konkrete Lesesitzung beziehungsweise den
//! einzelnen Inhaltsaufruf und sind keine allgemeinen Copy-Limits.
//!
//! Data Descriptors werden nicht zur Grenzbestimmung benutzt. Header-/Payload-
//! Überlappungen werden abgelehnt; eine vollständige Prüfung der nachlaufenden
//! Descriptorbereiche wird nicht zugesagt. Inhaltsbelege gelten ausschließlich
//! für den ausgewählten zentralen Eintrag. CRC ist die ZIP-Inhaltsprüfung, kein
//! kryptografischer Echtheitsbeleg. Die Quellenbindung prüft vorhandene lokale
//! Datei-/Eltern-/Rootversionen ohne vollständiges Vorhashen.

use crate::local_fs::{open_read_beneath, RootHandle};
use crate::local_read::FileVersion;
use crate::{coded_error, Cancel, ErrorCode};
use rawzip::ReaderAt;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

fn invalid(message: &str) -> io::Error {
    coded_error(io::ErrorKind::InvalidData, ErrorCode::InvalidData, message)
}
fn drift() -> io::Error {
    coded_error(
        io::ErrorKind::Interrupted,
        ErrorCode::PlanDrift,
        "ZIP-Quelle wurde geändert oder ausgetauscht",
    )
}
fn check_cancel(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::Interrupted,
            "ZIP-Lesen abgebrochen",
        ))
    } else {
        Ok(())
    }
}

struct Source {
    root: RootHandle,
    parent: RootHandle,
    path: PathBuf,
    file: File,
    version: FileVersion,
    length: u64,
    verified: Option<crate::vfs::LocalVfs>,
    snapshot: Option<crate::remote_snapshot::ValidatedSnapshot>,
    digest: [u8; 32],
}
impl Source {
    fn bind(root: &Path, relative: &Path, cancel: &Cancel) -> io::Result<Self> {
        check_cancel(cancel)?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(invalid(
                "ZIP-Quelle benötigt einen relativen Pfad ohne Sonderkomponenten",
            ));
        }
        let root = RootHandle::open(root)?;
        let path = root.path().join(relative);
        let parent = RootHandle::open_beneath(&root, path.parent().ok_or_else(drift)?)?;
        // Der gemeinsame Binder scannt Ordner rekursiv. Vor seinem Aufruf muss
        // deshalb der finale FD genau eine reguläre Datei belegen.
        let file = open_read_beneath(&parent, &path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(invalid("ZIP-Quelle ist keine reguläre Datei"));
        }
        check_cancel(cancel)?;
        let verified = crate::vfs::LocalVfs::rooted(root.path())?
            .with_verified_reads(std::slice::from_ref(&path), cancel)?;
        let digest = verified
            .verified_source_binding_digest()?
            .ok_or_else(drift)?;
        let source = Self {
            root,
            parent,
            path,
            file,
            version: FileVersion::of(&meta),
            length: meta.len(),
            verified: Some(verified),
            snapshot: None,
            digest,
        };
        source.verify()?;
        check_cancel(cancel)?;
        Ok(source)
    }
    fn bind_snapshot(
        snapshot: crate::remote_snapshot::ValidatedSnapshot,
        cancel: &Cancel,
    ) -> io::Result<Self> {
        check_cancel(cancel)?;
        let (root, file, name) = snapshot.source_parts()?;
        let meta = file.metadata()?;
        let version = FileVersion::of(&meta);
        let digest = *blake3::hash(
            &serde_json::to_vec(&(snapshot.reference(), &version)).map_err(io::Error::other)?,
        )
        .as_bytes();
        let source = Self {
            parent: root.clone(),
            path: root.path().join(name),
            root,
            file,
            version,
            length: meta.len(),
            verified: None,
            snapshot: Some(snapshot),
            digest,
        };
        source.verify()?;
        Ok(source)
    }
    fn verify(&self) -> io::Result<()> {
        let current_digest = if let Some(verified) = &self.verified {
            verified.verified_source_binding_digest().map_err(|error| {
                if crate::error_code(&error) == ErrorCode::PlanDrift {
                    drift()
                } else {
                    error
                }
            })?
        } else {
            self.snapshot
                .as_ref()
                .ok_or_else(drift)?
                .lease
                .ensure_current()?;
            Some(self.digest)
        };
        if current_digest != Some(self.digest)
            || !self.root.is_current()?
            || !self.parent.is_current()?
            || FileVersion::of(&self.file.metadata()?) != self.version
            || FileVersion::of(&open_read_beneath(&self.parent, &self.path)?.metadata()?)
                != self.version
        {
            return Err(drift());
        }
        Ok(())
    }
}
fn validate_catalog_budget(budget: CatalogBudget) -> io::Result<()> {
    if budget.max_entries == 0
        || budget.max_metadata_bytes == 0
        || budget.max_name_bytes == 0
        || budget.max_path_depth == 0
        || budget.max_io_bytes == 0
        || budget.max_read_calls == 0
    {
        return Err(budget_error());
    }
    Ok(())
}
struct BoundedReader {
    source: Source,
    cancel: Cancel,
    bytes: Cell<u64>,
    calls: Cell<u64>,
    max_bytes: u64,
    max_calls: u64,
}
impl ReaderAt for BoundedReader {
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
        check_cancel(&self.cancel)?;
        if buffer.is_empty() {
            return Ok(0);
        }
        if offset > self.source.length {
            return Err(invalid(
                "ZIP-Lesebereich liegt außerhalb der gebundenen Quelle",
            ));
        }
        let available = self.source.length - offset;
        let requested = available.min(buffer.len() as u64);
        if self.calls.get() >= self.max_calls
            || requested > self.max_bytes.saturating_sub(self.bytes.get())
        {
            return Err(budget_error());
        }
        self.calls.set(self.calls.get() + 1);
        self.source.verify()?;
        let read = self
            .source
            .file
            .read_at(&mut buffer[..requested as usize], offset)?;
        self.bytes.set(self.bytes.get() + read as u64);
        self.source.verify()?;
        check_cancel(&self.cancel)?;
        Ok(read)
    }
}

/// Budgets für das Lesen des Katalogs, nicht für normale Dateioperationen.
#[derive(Clone, Copy, Debug)]
pub struct CatalogBudget {
    pub max_entries: usize,
    pub max_metadata_bytes: u64,
    pub max_name_bytes: usize,
    pub max_path_depth: usize,
    pub max_io_bytes: u64,
    pub max_read_calls: u64,
}
impl Default for CatalogBudget {
    fn default() -> Self {
        Self {
            max_entries: 20_000,
            max_metadata_bytes: 16 * 1024 * 1024,
            max_name_bytes: 4096,
            max_path_depth: 128,
            max_io_bytes: 64 * 1024 * 1024,
            max_read_calls: 100_000,
        }
    }
}
/// Ein Leseaufruf besitzt sein eigenes Eingabe-/Ausgabe- und Arbeitsbudget.
#[derive(Clone, Copy, Debug)]
pub struct ReadBudget {
    pub max_output_bytes: u64,
    pub max_io_bytes: u64,
    pub max_read_calls: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ArchiveEntryId {
    session: [u8; 16],
    index: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveEntryKind {
    File,
    Directory,
    Unsupported,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveEntryIssue {
    InvalidUtf8,
    UnsafePath,
    NameTooLong,
    PathTooDeep,
    Encrypted,
    UnsupportedType,
    UnsupportedCompression,
    InvalidHeader,
    HeaderMismatch,
    InvalidRange,
    OverlappingRange,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct ArchiveCatalogEntry {
    pub id: ArchiveEntryId,
    pub raw_name: Vec<u8>,
    /// Nur strikt dekodiertes UTF8, niemals normalisiert oder repariert.
    pub name: Option<String>,
    pub kind: ArchiveEntryKind,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    pub duplicate_name: bool,
    pub issues: Vec<ArchiveEntryIssue>,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct ArchiveCatalogStop {
    pub code: ErrorCode,
    pub message: String,
}
#[derive(Debug)]
pub struct ArchiveDirectory<'a> {
    pub directories: Vec<String>,
    pub entries: Vec<&'a ArchiveCatalogEntry>,
}
/// Nur dieses Ergebnis bestätigt einen vollständig gelesenen, geprüften Eintrag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifiedArchiveRead {
    pub output_bytes: u64,
    pub io_bytes: u64,
}

struct BoundEntry {
    wayfinder: rawzip::ZipArchiveEntryWayfinder,
    method: rawzip::CompressionMethod,
}
/// Nicht serialisierbare Lesesitzung mit gebundener Quelle und Backend-IDs.
/// Der Katalog ist keine Zusage vollständiger ZIP-Strukturvalidität.
pub struct ArchiveRead {
    archive: rawzip::ZipArchive<BoundedReader>,
    entries: Vec<ArchiveCatalogEntry>,
    bound: Vec<BoundEntry>,
    session: [u8; 16],
    stop: Option<ArchiveCatalogStop>,
}
fn zip_error(error: rawzip::Error) -> io::Error {
    match error.into_kind() {
        rawzip::ErrorKind::IO(error) => error,
        kind => invalid(&format!("Ungültige ZIP-Struktur: {kind}")),
    }
}
fn budget_error() -> io::Error {
    coded_error(
        io::ErrorKind::InvalidInput,
        ErrorCode::InvalidInput,
        "ZIP-Ressourcenbudget erreicht",
    )
}
pub(crate) fn path_is_safe(name: &str) -> bool {
    let name = name.strip_suffix('/').unwrap_or(name);
    !name.is_empty()
        && !name.contains(['\\', '\0', ':'])
        && !name.chars().any(char::is_control)
        && name
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
}
fn source_failure(error: &io::Error) -> bool {
    crate::error_code(error) != ErrorCode::InvalidData
}
impl ArchiveRead {
    pub fn open(
        root: &Path,
        relative_source: &Path,
        budget: CatalogBudget,
        cancel: &Cancel,
    ) -> io::Result<Self> {
        validate_catalog_budget(budget)?;
        let source = Source::bind(root, relative_source, cancel)?;
        Self::open_source(source, budget, cancel)
    }
    /// Übernimmt die bereits manifest- und inhaltsgeprüfte Datei am gehaltenen FD.
    pub fn open_snapshot(
        snapshot: crate::remote_snapshot::ValidatedSnapshot,
        budget: CatalogBudget,
        cancel: &Cancel,
    ) -> io::Result<Self> {
        validate_catalog_budget(budget)?;
        let source = Source::bind_snapshot(snapshot, cancel)?;
        Self::open_source(source, budget, cancel)
    }
    fn open_source(source: Source, budget: CatalogBudget, cancel: &Cancel) -> io::Result<Self> {
        check_cancel(cancel)?;
        let length = source.length;
        let reader = BoundedReader {
            source,
            cancel: cancel.clone(),
            bytes: Cell::new(0),
            calls: Cell::new(0),
            max_bytes: budget.max_io_bytes,
            max_calls: budget.max_read_calls,
        };
        // Feste Formatobergrenzen: keine aus Archivzahlen abgeleitete Allokation.
        let mut buffer = vec![0; rawzip::MAX_CENTRAL_DIRECTORY_RECORD_SIZE];
        let mut local_buffer = vec![0; 2 * usize::from(u16::MAX)];
        let archive = rawzip::ZipLocator::new()
            .locate_in_reader(reader, &mut buffer, length)
            .map_err(|(_, e)| zip_error(e))?;
        let mut session = [0; 16];
        getrandom::fill(&mut session).map_err(|error| io::Error::other(error.to_string()))?;
        let mut result = Self {
            archive,
            entries: Vec::new(),
            bound: Vec::new(),
            session,
            stop: None,
        };
        let mut iterator = result.archive.entries(&mut buffer);
        let mut metadata_bytes = 0u64;
        let mut names = BTreeMap::<Vec<u8>, usize>::new();
        let mut ranges = Vec::new();
        loop {
            check_cancel(cancel)?;
            let record = match iterator.next_entry().map_err(zip_error) {
                Ok(Some(record)) => record,
                Ok(None) => break,
                Err(error) if source_failure(&error) => return Err(error),
                Err(error) => {
                    result.stop = Some(ArchiveCatalogStop {
                        code: crate::error_code(&error),
                        message: error.to_string(),
                    });
                    break;
                }
            };
            let raw = record.file_path();
            let extra_len = record.extra_fields().remaining_bytes().len();
            let charge = 46
                + raw.as_ref().len() as u64
                + extra_len as u64
                + record.comment().as_bytes().len() as u64;
            if result.entries.len() >= budget.max_entries
                || charge > budget.max_metadata_bytes.saturating_sub(metadata_bytes)
            {
                let error = budget_error();
                result.stop = Some(ArchiveCatalogStop {
                    code: crate::error_code(&error),
                    message: error.to_string(),
                });
                break;
            }
            metadata_bytes += charge;
            let index = result.entries.len();
            let mut issues = Vec::new();
            let name = std::str::from_utf8(raw.as_ref()).ok().map(str::to_owned);
            if name.is_none() {
                issues.push(ArchiveEntryIssue::InvalidUtf8);
            }
            if raw.as_ref().len() > budget.max_name_bytes {
                issues.push(ArchiveEntryIssue::NameTooLong);
            }
            if let Some(name) = &name {
                if !path_is_safe(name) {
                    issues.push(ArchiveEntryIssue::UnsafePath);
                }
                if name.split('/').filter(|s| !s.is_empty()).count() > budget.max_path_depth {
                    issues.push(ArchiveEntryIssue::PathTooDeep);
                }
            }
            let mode = record.mode().value() & 0o170000;
            let kind = match mode {
                0 | 0o100000 if !record.is_dir() => ArchiveEntryKind::File,
                0 | 0o040000 if record.is_dir() => ArchiveEntryKind::Directory,
                _ => ArchiveEntryKind::Unsupported,
            };
            if kind == ArchiveEntryKind::Unsupported {
                issues.push(ArchiveEntryIssue::UnsupportedType);
            }
            if record.flags().is_encrypted()
                || record.flags().has_strong_encryption()
                || record.flags().is_masked()
            {
                issues.push(ArchiveEntryIssue::Encrypted);
            }
            let method = record.compression_method();
            if method != rawzip::CompressionMethod::STORE
                && method != rawzip::CompressionMethod::DEFLATE
            {
                issues.push(ArchiveEntryIssue::UnsupportedCompression);
            }
            let mut extra = record.extra_fields();
            for _ in extra.by_ref() {}
            if !extra.remaining_bytes().is_empty() {
                issues.push(ArchiveEntryIssue::InvalidHeader);
            }
            match result
                .archive
                .get_entry(record.wayfinder())
                .map_err(zip_error)
            {
                Ok(entry) => {
                    let (start, end) = entry.compressed_data_range();
                    let header = record.local_header_offset();
                    if header >= result.archive.directory_offset()
                        || start > end
                        || end > result.archive.directory_offset()
                        || end > length
                    {
                        issues.push(ArchiveEntryIssue::InvalidRange);
                    } else {
                        ranges.push((header, end, index));
                    }
                    match entry.local_header(&mut local_buffer).map_err(zip_error) {
                        Ok(local) => {
                            if local.file_path().as_ref() != raw.as_ref()
                                || local.flags() != record.flags()
                                || local.compression_method() != method
                                || (!record.flags().has_data_descriptor()
                                    && (local.crc32() != record.crc32()
                                        || local.compressed_size_hint()
                                            != record.compressed_size_hint()
                                        || local.uncompressed_size_hint()
                                            != record.uncompressed_size_hint()))
                            {
                                issues.push(ArchiveEntryIssue::HeaderMismatch);
                            }
                        }
                        Err(error) if source_failure(&error) => return Err(error),
                        Err(_) => issues.push(ArchiveEntryIssue::InvalidHeader),
                    }
                }
                Err(error) if source_failure(&error) => return Err(error),
                Err(_) => issues.push(ArchiveEntryIssue::InvalidHeader),
            }
            let previous = names.get(raw.as_ref()).copied();
            if let Some(previous) = previous {
                result.entries[previous].duplicate_name = true;
            } else {
                names.insert(raw.as_ref().to_vec(), index);
            }
            result.entries.push(ArchiveCatalogEntry {
                id: ArchiveEntryId { session, index },
                raw_name: raw.as_ref().to_vec(),
                name,
                kind,
                uncompressed_bytes: record.uncompressed_size_hint(),
                compressed_bytes: record.compressed_size_hint(),
                duplicate_name: previous.is_some(),
                issues,
            });
            result.bound.push(BoundEntry {
                wayfinder: record.wayfinder(),
                method,
            });
        }
        if result.stop.is_none() && result.entries.len() as u64 != result.archive.entries_hint() {
            result.stop = Some(ArchiveCatalogStop {
                code: ErrorCode::InvalidData,
                message: "ZIP-Eintragszahl stimmt nicht mit dem gelesenen Katalog überein".into(),
            });
        }
        ranges.sort_unstable();
        let mut covering: Option<(u64, usize)> = None;
        for (start, end, index) in ranges {
            if let Some((max_end, previous)) = covering {
                if start < max_end {
                    for i in [previous, index] {
                        if !result.entries[i]
                            .issues
                            .contains(&ArchiveEntryIssue::OverlappingRange)
                        {
                            result.entries[i]
                                .issues
                                .push(ArchiveEntryIssue::OverlappingRange);
                        }
                    }
                }
                if end > max_end {
                    covering = Some((end, index));
                }
            } else {
                covering = Some((end, index));
            }
        }
        result.archive.get_ref().source.verify()?;
        check_cancel(cancel)?;
        Ok(result)
    }
    /// Frische gemeinsame Quellenbindung ohne vollständiges Vorhashen.
    pub fn source_binding_digest(&self, cancel: &Cancel) -> io::Result<[u8; 32]> {
        self.verify_source(cancel)?;
        Ok(self.archive.get_ref().source.digest)
    }
    /// Auch nach einem Ziel-Readback und vor einer Ordneranlage/Commit prüfen.
    pub fn verify_source(&self, cancel: &Cancel) -> io::Result<()> {
        check_cancel(cancel)?;
        self.archive.get_ref().source.verify()?;
        check_cancel(cancel)
    }
    /// Gehaltene Quellbezeichner; ein frischer Versionsvergleich bleibt erforderlich.
    pub fn source_paths(&self) -> (&Path, &Path) {
        let source = &self.archive.get_ref().source;
        (source.root.path(), &source.path)
    }
    pub(crate) fn source_identity(&self) -> io::Result<(u64, u64)> {
        let metadata = self.archive.get_ref().source.file.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
    /// Wandelt ausschließlich eine Kennung dieses Katalogs in eine persistente Dateiauswahl um.
    pub fn extract_entry_selection(
        &self,
        id: ArchiveEntryId,
        cancel: &Cancel,
    ) -> io::Result<crate::archive_extract::ArchiveExtractSelection> {
        self.verify_source(cancel)?;
        if id.session != self.session {
            return Err(invalid(
                "ZIP-Eintragskennung gehört nicht zu diesem Katalog",
            ));
        }
        let entry = self
            .entries
            .get(id.index)
            .filter(|entry| entry.id == id && entry.kind == ArchiveEntryKind::File)
            .ok_or_else(|| invalid("ZIP-Auswahl benötigt einen vorhandenen Dateieintrag"))?;
        Ok(crate::archive_extract::ArchiveExtractSelection::Entry {
            ordinal: id.index,
            raw_name: entry.raw_name.clone(),
        })
    }
    /// Bindet einen exakten virtuellen Nichtroot-Unterordner ohne Namensnormalisierung.
    pub fn extract_subtree_selection(
        &self,
        prefix: &str,
        cancel: &Cancel,
    ) -> io::Result<crate::archive_extract::ArchiveExtractSelection> {
        self.verify_source(cancel)?;
        let selection = crate::archive_extract::ArchiveExtractSelection::Subtree {
            prefix: prefix.to_owned(),
        };
        crate::archive_extract::validate_selection_catalog(&selection, self)?;
        Ok(selection)
    }
    pub fn entries(&self) -> &[ArchiveCatalogEntry] {
        &self.entries
    }
    pub fn stop_reason(&self) -> Option<&ArchiveCatalogStop> {
        self.stop.as_ref()
    }
    /// Vollständigkeit des zentralen Katalogs, keine vollständige Strukturprüfung.
    pub fn is_complete(&self) -> bool {
        self.stop.is_none()
    }
    /// Virtuelle Ordner entstehen aus sicheren, unveränderten ZIP-Segmenten.
    /// Nicht navigierbare Namen bleiben als problematische Einträge im Wurzelkatalog.
    pub fn directory(&self, directory: &str) -> io::Result<ArchiveDirectory<'_>> {
        if !directory.is_empty() && (!path_is_safe(directory) || directory.ends_with('/')) {
            return Err(invalid("Ungültiger virtueller ZIP-Ordner"));
        }
        let prefix = if directory.is_empty() {
            String::new()
        } else {
            format!("{directory}/")
        };
        let mut directories = BTreeSet::new();
        let mut entries = Vec::new();
        for entry in &self.entries {
            let usable = entry.name.as_deref().filter(|n| path_is_safe(n));
            let Some(name) = usable else {
                if directory.is_empty() {
                    entries.push(entry);
                }
                continue;
            };
            let Some(rest) = name.strip_prefix(&prefix) else {
                continue;
            };
            if let Some((child, _)) = rest.split_once('/') {
                if !child.is_empty() {
                    directories.insert(format!("{prefix}{child}"));
                }
            } else if !rest.is_empty() {
                entries.push(entry);
            }
        }
        Ok(ArchiveDirectory {
            directories: directories.into_iter().collect(),
            entries,
        })
    }
    /// Bytes im Sink sind bis zum erfolgreichen Rückgabewert vorläufig. Bei Fehler
    /// oder Abbruch muss der Aufrufer sie verwerfen; es gibt keinen Teilerfolgbeleg.
    pub fn read_entry_to(
        &mut self,
        id: ArchiveEntryId,
        budget: ReadBudget,
        cancel: &Cancel,
        sink: &mut dyn Write,
    ) -> io::Result<VerifiedArchiveRead> {
        check_cancel(cancel)?;
        if id.session != self.session {
            return Err(invalid(
                "ZIP-Eintrags-ID gehört zu einer anderen Lesesitzung",
            ));
        }
        let entry = self
            .entries
            .get(id.index)
            .ok_or_else(|| invalid("Unbekannte ZIP-Eintrags-ID"))?;
        if entry.kind != ArchiveEntryKind::File || !entry.issues.is_empty() {
            return Err(coded_error(
                io::ErrorKind::Unsupported,
                ErrorCode::Unsupported,
                "ZIP-Eintrag ist in dieser Lesesicht nicht ausführbar",
            ));
        }
        if entry.uncompressed_bytes > budget.max_output_bytes
            || budget.max_io_bytes == 0
            || budget.max_read_calls == 0
        {
            return Err(budget_error());
        }
        let bound = &self.bound[id.index];
        let reader = self.archive.get_mut();
        reader.cancel = cancel.clone();
        reader.bytes.set(0);
        reader.calls.set(0);
        reader.max_bytes = budget.max_io_bytes;
        reader.max_calls = budget.max_read_calls;
        reader.source.verify()?;
        let local = self.archive.get_entry(bound.wayfinder).map_err(zip_error)?;
        let expected = local.reader().claim_verifier();
        let mut crc = rawzip::Crc32::new();
        let output = if bound.method == rawzip::CompressionMethod::STORE {
            copy_stored(
                local.reader(),
                expected.uncompressed_size,
                budget.max_output_bytes,
                cancel,
                sink,
                &mut crc,
            )?
        } else {
            copy_deflate(
                local.reader(),
                entry.compressed_bytes,
                expected.uncompressed_size,
                budget.max_output_bytes,
                cancel,
                sink,
                &mut crc,
            )?
        };
        expected
            .valid(rawzip::ZipVerification {
                crc: crc.checksum(),
                uncompressed_size: output,
            })
            .map_err(zip_error)?;
        self.archive.get_ref().source.verify()?;
        check_cancel(cancel)?;
        Ok(VerifiedArchiveRead {
            output_bytes: output,
            io_bytes: self.archive.get_ref().bytes.get(),
        })
    }
}
fn emit(
    bytes: &[u8],
    total: &mut u64,
    expected: u64,
    max: u64,
    cancel: &Cancel,
    sink: &mut dyn Write,
    crc: &mut rawzip::Crc32,
) -> io::Result<()> {
    check_cancel(cancel)?;
    let count = bytes.len() as u64;
    if count > expected.saturating_sub(*total) {
        return Err(invalid("ZIP-Inhalt ist länger als angekündigt"));
    }
    if count > max.saturating_sub(*total) {
        return Err(budget_error());
    }
    sink.write_all(bytes)?;
    crc.update(bytes);
    *total += count;
    check_cancel(cancel)
}
fn copy_stored(
    mut reader: impl Read,
    expected: u64,
    max: u64,
    cancel: &Cancel,
    sink: &mut dyn Write,
    crc: &mut rawzip::Crc32,
) -> io::Result<u64> {
    let mut buffer = [0; 8192];
    let mut total = 0;
    loop {
        check_cancel(cancel)?;
        let cap =
            (buffer.len() as u64).min(expected.saturating_sub(total).saturating_add(1)) as usize;
        let read = reader.read(&mut buffer[..cap])?;
        if read == 0 {
            break;
        }
        emit(
            &buffer[..read],
            &mut total,
            expected,
            max,
            cancel,
            sink,
            crc,
        )?;
    }
    Ok(total)
}
fn copy_deflate(
    mut reader: impl Read,
    compressed: u64,
    expected: u64,
    max: u64,
    cancel: &Cancel,
    sink: &mut dyn Write,
    crc: &mut rawzip::Crc32,
) -> io::Result<u64> {
    let mut decoder = flate2::Decompress::new(false);
    let mut input = [0; 8192];
    let mut output = [0; 8192];
    let mut pos = 0;
    let mut end = 0;
    let mut eof = false;
    let mut total = 0;
    loop {
        check_cancel(cancel)?;
        if pos == end && !eof {
            end = reader.read(&mut input)?;
            pos = 0;
            eof = end == 0;
        }
        let cap =
            (output.len() as u64).min(expected.saturating_sub(total).saturating_add(1)) as usize;
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let status = decoder
            .decompress(
                &input[pos..end],
                &mut output[..cap],
                flate2::FlushDecompress::None,
            )
            .map_err(|_| invalid("Deflate-Daten sind ungültig"))?;
        let used = (decoder.total_in() - before_in) as usize;
        let produced = (decoder.total_out() - before_out) as usize;
        pos += used;
        emit(
            &output[..produced],
            &mut total,
            expected,
            max,
            cancel,
            sink,
            crc,
        )?;
        if status == flate2::Status::StreamEnd {
            if decoder.total_in() != compressed {
                return Err(invalid(
                    "Deflate verbraucht nicht den vollständigen gebundenen Datenbereich",
                ));
            }
            break;
        }
        if used == 0 && produced == 0 && (eof || pos < end) {
            return Err(invalid(
                "Deflate-Strom endet unvollständig oder ohne Fortschritt",
            ));
        }
    }
    Ok(total)
}

#[cfg(test)]
#[path = "archive_read_tests.rs"]
mod tests;
