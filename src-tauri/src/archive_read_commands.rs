//! Fenstergebundener, rein lesender ZIP-Browser. Keine Queue oder Mutationsfreigabe.
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bidirectional_commands::{resolve_endpoint, BidirectionalEndpointRequest};
use crate::{AppError, MutexExt};
use doppeldecker_core::archive_read::{
    ArchiveEntryId, ArchiveEntryIssue, ArchiveEntryKind, ArchiveRead, CatalogBudget, ReadBudget,
};
use doppeldecker_core::preview::{classify_preview_bytes, FilePreview, PREVIEW_MAX_BYTES};
use doppeldecker_core::Cancel;
use tauri::{Manager, State, WebviewWindow};

const IDLE: Duration = Duration::from_secs(600);
const PAGE_SIZE: usize = 100;
const TOMBSTONES: usize = 128;
fn interrupted() -> AppError {
    AppError::new(
        "interrupted",
        "Archivansicht wurde geschlossen oder abgebrochen",
    )
}
fn unknown() -> AppError {
    AppError::new("expired", "Die Archivansicht ist abgelaufen oder gehört zu einem anderen Fenster. Bitte erneut öffnen.")
}
fn busy() -> AppError {
    AppError::new("busy", "Der vorherige Archiv-Lesevorgang endet noch.")
}
fn valid_request(id: &str) -> Result<(), AppError> {
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        Err(AppError::new(
            "invalid_input",
            "Ungültige Archiv-Anfrage-ID",
        ))
    } else {
        Ok(())
    }
}

#[derive(Clone, serde::Serialize)]
struct Issue {
    code: String,
    message: String,
}
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    row_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    directory_id: Option<String>,
    display_name: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    uncompressed_bytes: Option<u64>,
    duplicate_name: bool,
    can_preview: bool,
    issues: Vec<Issue>,
}
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    complete: bool,
    entry_count: usize,
    displayed_entry_count: usize,
    problem_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<Issue>,
}
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Breadcrumb {
    directory_id: String,
    label: String,
}
pub(crate) struct ExtractSource {
    pub root: std::path::PathBuf,
    pub path: std::path::PathBuf,
    pub selection: doppeldecker_core::archive_extract::ArchiveExtractSelection,
    pub digest: [u8; 32],
    pub snapshot: Option<doppeldecker_core::archive_extract::ArchiveSnapshotSource>,
    pub validated: Option<doppeldecker_core::remote_snapshot::ValidatedSnapshot>,
}
struct Directory {
    parent: Option<String>,
    label: String,
    rows: Vec<Row>,
}
struct Entry {
    core_id: ArchiveEntryId,
    bytes: u64,
    can_preview: bool,
}
struct Metadata {
    directories: HashMap<String, Directory>,
    entries: HashMap<String, Entry>,
    catalog: Catalog,
    root_id: String,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Listing {
    session_id: String,
    directory_id: String,
    breadcrumbs: Vec<Breadcrumb>,
    page: u32,
    page_size: usize,
    total_rows: usize,
    rows: Vec<Row>,
    catalog: Catalog,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Opened {
    request_id: String,
    session_id: String,
    listing: Listing,
    can_extract: bool,
    can_extract_file: bool,
    origin: Option<String>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Previewed {
    request_id: String,
    session_id: String,
    entry_id: String,
    preview: FilePreview,
}

fn issue(value: ArchiveEntryIssue) -> Issue {
    let code = serde_json::to_value(value)
        .expect("issue serializable")
        .as_str()
        .unwrap()
        .to_string();
    let message = match value {
        ArchiveEntryIssue::InvalidUtf8 => {
            "Name enthält ungültige UTF-8-Bytes; keine Aktion verfügbar"
        }
        ArchiveEntryIssue::UnsafePath => "Unsicherer Archivpfad; keine Aktion verfügbar",
        ArchiveEntryIssue::NameTooLong => "Name überschreitet das Katalog-Namensbudget",
        ArchiveEntryIssue::PathTooDeep => "Pfad überschreitet das Katalog-Tiefenbudget",
        ArchiveEntryIssue::Encrypted => "Verschlüsselter Eintrag wird nicht gelesen",
        ArchiveEntryIssue::UnsupportedType => "Symlink oder anderer nicht unterstützter Dateityp",
        ArchiveEntryIssue::UnsupportedCompression => "Kompressionsverfahren wird nicht unterstützt",
        ArchiveEntryIssue::InvalidHeader => "Ungültiger ZIP-Header",
        ArchiveEntryIssue::HeaderMismatch => "Lokaler und zentraler ZIP-Header widersprechen sich",
        ArchiveEntryIssue::InvalidRange => "Ungültiger Inhaltsbereich",
        ArchiveEntryIssue::OverlappingRange => "Überlappender Inhaltsbereich",
    }
    .into();
    Issue { code, message }
}
fn usable_name(issues: &[ArchiveEntryIssue]) -> bool {
    !issues.iter().any(|i| {
        matches!(
            i,
            ArchiveEntryIssue::InvalidUtf8
                | ArchiveEntryIssue::UnsafePath
                | ArchiveEntryIssue::NameTooLong
                | ArchiveEntryIssue::PathTooDeep
        )
    })
}
const VIEW_MAX_DIRECTORIES: usize = 10_000;
const VIEW_MAX_METADATA_BYTES: usize = 32 * 1024 * 1024;
fn mapping_cancel(cancel: &Cancel) -> Result<(), AppError> {
    if cancel.is_cancelled() {
        Err(interrupted())
    } else {
        Ok(())
    }
}
fn metadata(archive: &ArchiveRead, session: &str, cancel: &Cancel) -> Result<Metadata, AppError> {
    metadata_bounded(
        archive,
        session,
        cancel,
        VIEW_MAX_DIRECTORIES,
        VIEW_MAX_METADATA_BYTES,
    )
}
fn metadata_bounded(
    archive: &ArchiveRead,
    session: &str,
    cancel: &Cancel,
    max_directories: usize,
    max_bytes: usize,
) -> Result<Metadata, AppError> {
    mapping_cancel(cancel)?;
    let root_id = format!("{session}:d0");
    let mut paths = BTreeMap::from([(String::new(), root_id.clone())]);
    let mut directories = HashMap::from([(
        root_id.clone(),
        Directory {
            parent: None,
            label: "ZIP-Inhalt".into(),
            rows: Vec::new(),
        },
    )]);
    let mut explicit = std::collections::HashSet::new();
    let mut entries = HashMap::new();
    let mut charged = 512usize;
    let mut mapped = 0usize;
    for (index, entry) in archive.entries().iter().enumerate() {
        mapping_cancel(cancel)?;
        let safe = usable_name(&entry.issues);
        let name = entry.name.as_deref();
        let trimmed = name.unwrap_or("").trim_end_matches('/');
        let directory =
            safe && entry.kind == ArchiveEntryKind::Directory && entry.issues.is_empty();
        let (parent, leaf) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
        // Erst den vollständigen Zusatzbedarf des Eintrags prüfen, dann aufnehmen.
        // Dies begrenzt auch die vorübergehend gehaltenen Präfixschlüssel. Es ist
        // eine konservative Metadatenbilanz, keine Zusage einer exakten Heapgrenze.
        let mut missing = Vec::new();
        if safe {
            let parts: Vec<_> = trimmed.split('/').collect();
            let count = if directory {
                parts.len()
            } else {
                parts.len().saturating_sub(1)
            };
            for end in 1..=count {
                mapping_cancel(cancel)?;
                let path = parts[..end].join("/");
                if !paths.contains_key(&path) {
                    missing.push(path);
                }
            }
        }
        let display_name = if safe {
            leaf.to_string()
        } else if let Some(name) = name {
            name.to_string()
        } else {
            format!(
                "[ungültiges UTF-8: {}]",
                entry
                    .raw_name
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        };
        let charge = 1024usize
            .saturating_add(display_name.len().saturating_mul(2))
            .saturating_add(entry.issues.len() * 256)
            .saturating_add(
                missing
                    .iter()
                    .map(|p| 1024usize.saturating_add(p.len().saturating_mul(2)))
                    .sum::<usize>(),
            );
        if paths.len().saturating_add(missing.len()) > max_directories
            || charge > max_bytes.saturating_sub(charged)
        {
            break;
        }
        charged += charge;
        for path in missing {
            let id = format!("{session}:d{}", paths.len());
            let (parent, label) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            directories.insert(
                id.clone(),
                Directory {
                    parent: Some(paths[parent].clone()),
                    label: label.into(),
                    rows: Vec::new(),
                },
            );
            paths.insert(path, id);
        }
        let id = format!("{session}:e{index}");
        let directory_id = directory.then(|| paths[trimmed].clone());
        if directory {
            explicit.insert(trimmed.to_string());
        }
        let parent_id = if safe {
            paths.get(parent).unwrap_or(&root_id)
        } else {
            &root_id
        };
        let can_preview = entry.kind == ArchiveEntryKind::File && entry.issues.is_empty();
        entries.insert(
            id.clone(),
            Entry {
                core_id: entry.id,
                bytes: entry.uncompressed_bytes,
                can_preview,
            },
        );
        directories.get_mut(parent_id).unwrap().rows.push(Row {
            row_id: id.clone(),
            entry_id: Some(id),
            directory_id,
            display_name,
            kind: if !entry.issues.is_empty() {
                "problem"
            } else if directory {
                "directory"
            } else {
                "file"
            }
            .into(),
            uncompressed_bytes: Some(entry.uncompressed_bytes),
            duplicate_name: entry.duplicate_name,
            can_preview,
            issues: entry.issues.iter().copied().map(issue).collect(),
        });
        mapped += 1;
    }
    for (path, id) in &paths {
        mapping_cancel(cancel)?;
        if path.is_empty() || explicit.contains(path) {
            continue;
        }
        let (parent, leaf) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        directories.get_mut(&paths[parent]).unwrap().rows.push(Row {
            row_id: id.clone(),
            entry_id: None,
            directory_id: Some(id.clone()),
            display_name: leaf.into(),
            kind: "directory".into(),
            uncompressed_bytes: None,
            duplicate_name: false,
            can_preview: false,
            issues: Vec::new(),
        });
    }
    for dir in directories.values_mut() {
        mapping_cancel(cancel)?;
        dir.rows.sort_by(|a, b| {
            (a.directory_id.is_none(), &a.display_name, &a.row_id).cmp(&(
                b.directory_id.is_none(),
                &b.display_name,
                &b.row_id,
            ))
        });
    }
    let view_partial = mapped != archive.entries().len();
    let stop_reason = if view_partial {
        Some(Issue { code: "resource_limit".into(), message: format!("Teilansicht: {mapped} von {} katalogisierten Einträgen abgebildet, {} nicht dargestellt. Das zusätzliche Budget für virtuelle Ordner oder Ansichtsmetadaten wurde erreicht.{}", archive.entries().len(), archive.entries().len() - mapped, archive.stop_reason().map(|s| format!(" Der ZIP-Katalog ist ebenfalls unvollständig: {}", s.message)).unwrap_or_default()) })
    } else {
        archive.stop_reason().map(|s| Issue {
            code: s.code.as_str().into(),
            message: s.message.clone(),
        })
    };
    Ok(Metadata {
        directories,
        entries,
        root_id,
        catalog: Catalog {
            complete: archive.is_complete() && !view_partial,
            entry_count: archive.entries().len(),
            displayed_entry_count: mapped,
            problem_count: archive
                .entries()
                .iter()
                .filter(|e| !e.issues.is_empty())
                .count(),
            stop_reason,
        },
    })
}

fn listing(
    session: &str,
    meta: &Metadata,
    directory: &str,
    page: u32,
) -> Result<Listing, AppError> {
    let dir = meta.directories.get(directory).ok_or_else(unknown)?;
    let start = (page as usize).checked_mul(PAGE_SIZE).ok_or_else(unknown)?;
    if start > dir.rows.len() || (start == dir.rows.len() && page != 0) {
        return Err(AppError::new("invalid_input", "Unbekannte Archivseite"));
    }
    let mut breadcrumbs = Vec::new();
    let mut current = Some(directory);
    while let Some(id) = current {
        let node = meta.directories.get(id).ok_or_else(unknown)?;
        breadcrumbs.push(Breadcrumb {
            directory_id: id.into(),
            label: node.label.clone(),
        });
        current = node.parent.as_deref();
    }
    breadcrumbs.reverse();
    Ok(Listing {
        session_id: session.into(),
        directory_id: directory.into(),
        breadcrumbs,
        page,
        page_size: PAGE_SIZE,
        total_rows: dir.rows.len(),
        rows: dir
            .rows
            .iter()
            .skip(start)
            .take(PAGE_SIZE)
            .cloned()
            .collect(),
        catalog: meta.catalog.clone(),
    })
}

enum ArchiveOrigin {
    Local,
    Snapshot(Box<SnapshotArchiveOrigin>),
}
struct SnapshotArchiveOrigin {
    label: String,
    source: doppeldecker_core::archive_extract::ArchiveSnapshotSource,
    // Reader und Herkunft tragen denselben Lease auch nach Browserschließen.
    validated: doppeldecker_core::remote_snapshot::ValidatedSnapshot,
}
struct ArchiveSource {
    reader: ArchiveRead,
    origin: ArchiveOrigin,
}
impl ArchiveSource {
    fn origin_label(&self) -> Option<&str> {
        match &self.origin {
            ArchiveOrigin::Local => None,
            ArchiveOrigin::Snapshot(origin) => Some(&origin.label),
        }
    }
}
impl From<ArchiveRead> for ArchiveSource {
    fn from(reader: ArchiveRead) -> Self {
        Self {
            reader,
            origin: ArchiveOrigin::Local,
        }
    }
}
impl std::ops::Deref for ArchiveSource {
    type Target = ArchiveRead;
    fn deref(&self) -> &ArchiveRead {
        &self.reader
    }
}
impl std::ops::DerefMut for ArchiveSource {
    fn deref_mut(&mut self) -> &mut ArchiveRead {
        &mut self.reader
    }
}

struct Work {
    request: String,
    generation: u64,
    cancel: Cancel,
}
struct Session {
    id: String,
    open_request: String,
    cancel: Cancel,
    reader: Option<ArchiveSource>,
    meta: Arc<Metadata>,
}
struct Slot {
    generation: u64,
    open: Option<Work>,
    read: Option<Work>,
    session: Option<Session>,
    tombstones: VecDeque<String>,
    touched: Instant,
    destroyed: bool,
    timer_running: bool,
}
impl Default for Slot {
    fn default() -> Self {
        Self {
            generation: 0,
            open: None,
            read: None,
            session: None,
            tombstones: VecDeque::new(),
            touched: Instant::now(),
            destroyed: false,
            timer_running: false,
        }
    }
}
impl Slot {
    fn remember(&mut self, request: &str) {
        if !self.tombstones.iter().any(|r| r == request) {
            self.tombstones.push_back(request.into());
            if self.tombstones.len() > TOMBSTONES {
                self.tombstones.pop_front();
            }
        }
    }
    fn invalidate(&mut self) {
        self.generation += 1;
        if let Some(w) = &self.open {
            w.cancel.cancel();
        }
        if let Some(w) = &self.read {
            w.cancel.cancel();
        }
        if let Some(s) = self.session.take() {
            s.cancel.cancel();
        }
    }
}
#[derive(Clone, Default)]
pub(crate) struct ArchiveReadState {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
    sequence: Arc<std::sync::atomic::AtomicU64>,
}
impl ArchiveReadState {
    pub(crate) fn destroy_window(&self, window: &str) {
        let mut slots = self.slots.lock_recover();
        let slot = slots.entry(window.into()).or_default();
        slot.invalidate();
        slot.destroyed = true;
    }
    fn reserve_open(&self, window: &str, request: &str) -> Result<(u64, Cancel, String), AppError> {
        valid_request(request)?;
        let mut slots = self.slots.lock_recover();
        let slot = slots.entry(window.into()).or_default();
        if slot.destroyed || slot.tombstones.iter().any(|r| r == request) {
            return Err(interrupted());
        }
        if slot.open.is_some() || slot.read.is_some() {
            return Err(busy());
        }
        slot.invalidate();
        slot.remember(request);
        slot.touched = Instant::now();
        let cancel = Cancel::new();
        let generation = slot.generation;
        slot.open = Some(Work {
            request: request.into(),
            generation,
            cancel: cancel.clone(),
        });
        let session = format!(
            "archive-{}",
            self.sequence
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        Ok((generation, cancel, session))
    }
    fn finish_open(
        &self,
        window: &str,
        request: &str,
        generation: u64,
        id: String,
        result: Result<ArchiveSource, AppError>,
    ) -> Result<Opened, AppError> {
        // Katalogabbildung ist begrenzt, aber gehört ebenfalls außerhalb des Registrylocks.
        let cancel = self
            .slots
            .lock_recover()
            .get(window)
            .and_then(|s| s.open.as_ref())
            .filter(|w| w.generation == generation && w.request == request)
            .map(|w| w.cancel.clone())
            .ok_or_else(interrupted)?;
        let result = result.and_then(|reader| {
            let meta = Arc::new(metadata(&reader, &id, &cancel)?);
            Ok((reader, meta))
        });
        let mut slots = self.slots.lock_recover();
        let slot = slots.get_mut(window).ok_or_else(interrupted)?;
        let work = slot
            .open
            .take()
            .filter(|w| w.generation == generation && w.request == request)
            .ok_or_else(interrupted)?;
        if slot.generation != generation || slot.destroyed || work.cancel.is_cancelled() {
            return Err(interrupted());
        }
        let (reader, meta) = result?;
        let listing = listing(&id, &meta, &meta.root_id, 0)?;
        let can_extract = true;
        let origin = reader.origin_label().map(str::to_owned);
        slot.session = Some(Session {
            id: id.clone(),
            open_request: request.into(),
            cancel: work.cancel,
            reader: Some(reader),
            meta,
        });
        slot.touched = Instant::now();
        Ok(Opened {
            request_id: request.into(),
            session_id: id,
            listing,
            can_extract,
            can_extract_file: true,
            origin,
        })
    }
    fn cancel(&self, window: &str, request: &str) {
        if valid_request(request).is_err() {
            return;
        }
        let mut slots = self.slots.lock_recover();
        let slot = slots.entry(window.into()).or_default();
        slot.remember(request);
        if let Some(work) = &slot.read {
            if work.request == request {
                work.cancel.cancel();
            }
        }
        if slot.open.as_ref().is_some_and(|w| w.request == request)
            || slot
                .session
                .as_ref()
                .is_some_and(|s| s.open_request == request)
        {
            slot.invalidate();
        }
    }
    fn close(&self, window: &str, request: &str, session: Option<&str>) {
        if valid_request(request).is_err() {
            return;
        }
        let mut slots = self.slots.lock_recover();
        let slot = slots.entry(window.into()).or_default();
        slot.remember(request);
        if slot.open.as_ref().is_some_and(|w| w.request == request)
            || slot
                .session
                .as_ref()
                .is_some_and(|s| s.open_request == request && session.is_none_or(|id| id == s.id))
        {
            slot.invalidate();
        }
    }
    fn start_timer(&self, window: &str) -> bool {
        let mut slots = self.slots.lock_recover();
        let Some(slot) = slots.get_mut(window) else {
            return false;
        };
        if slot.timer_running {
            return false;
        }
        slot.timer_running = true;
        true
    }
    fn timer_tick(&self, window: &str, now: Instant) -> Option<Duration> {
        let mut slots = self.slots.lock_recover();
        let slot = slots.get_mut(window)?;
        if slot.destroyed || (slot.session.is_none() && slot.open.is_none() && slot.read.is_none())
        {
            slot.timer_running = false;
            return None;
        }
        let elapsed = now.saturating_duration_since(slot.touched);
        if elapsed >= IDLE {
            slot.invalidate();
            slot.timer_running = false;
            None
        } else {
            Some(IDLE - elapsed)
        }
    }
    fn page(
        &self,
        window: &str,
        id: &str,
        directory: &str,
        page: u32,
    ) -> Result<Listing, AppError> {
        let meta = {
            let mut slots = self.slots.lock_recover();
            let slot = slots.get_mut(window).ok_or_else(unknown)?;
            let session = slot
                .session
                .as_ref()
                .filter(|s| s.id == id)
                .ok_or_else(unknown)?;
            slot.touched = Instant::now();
            session.meta.clone()
        };
        listing(id, &meta, directory, page)
    }
    fn begin_read(
        &self,
        window: &str,
        id: &str,
        request: &str,
        entry: &str,
    ) -> Result<(ArchiveSource, ArchiveEntryId, u64, Cancel), AppError> {
        valid_request(request)?;
        let mut slots = self.slots.lock_recover();
        let slot = slots.get_mut(window).ok_or_else(unknown)?;
        if slot.tombstones.iter().any(|r| r == request) {
            return Err(interrupted());
        }
        if slot.read.is_some() {
            return Err(busy());
        }
        if slot
            .session
            .as_ref()
            .is_some_and(|s| s.open_request == request)
        {
            return Err(AppError::new(
                "invalid_input",
                "Vorschau benötigt eine eigene Anfrage-ID",
            ));
        }
        let session = slot
            .session
            .as_mut()
            .filter(|s| s.id == id)
            .ok_or_else(unknown)?;
        let entry = session
            .meta
            .entries
            .get(entry)
            .filter(|e| e.can_preview)
            .ok_or_else(|| {
                AppError::new(
                    "unsupported",
                    "Dieser Archiveintrag hat keine Inhaltsvorschau",
                )
            })?;
        if entry.bytes > PREVIEW_MAX_BYTES {
            return Err(AppError::new("unsupported", "Der Eintrag überschreitet die Vorschaugrenze von 8 MiB; sein Inhalt wurde nicht gelesen."));
        }
        let core_id = entry.core_id;
        let reader = session.reader.take().ok_or_else(busy)?;
        let cancel = Cancel::new();
        slot.remember(request);
        slot.touched = Instant::now();
        slot.read = Some(Work {
            request: request.into(),
            generation: slot.generation,
            cancel: cancel.clone(),
        });
        Ok((reader, core_id, slot.generation, cancel))
    }
    pub(crate) fn resolve_extract_selection(
        &self,
        window: &str,
        session_id: &str,
        request: &str,
        entry_id: Option<&str>,
        directory_id: Option<&str>,
        cancel: &Cancel,
    ) -> Result<ExtractSource, AppError> {
        valid_request(request)?;
        enum Choice {
            All,
            Entry(ArchiveEntryId),
            Subtree(String),
        }
        let (reader, choice, generation) = {
            let mut slots = self.slots.lock_recover();
            let slot = slots.get_mut(window).ok_or_else(unknown)?;
            if slot.read.is_some() {
                return Err(busy());
            }
            if slot.destroyed
                || slot.tombstones.iter().any(|id| id == request)
                || cancel.is_cancelled()
            {
                return Err(interrupted());
            }
            let session = slot
                .session
                .as_mut()
                .filter(|session| session.id == session_id)
                .ok_or_else(unknown)?;
            let choice = match (entry_id, directory_id) {
                (Some(id), None) => {
                    Choice::Entry(session.meta.entries.get(id).ok_or_else(unknown)?.core_id)
                }
                (None, Some(id)) => {
                    let mut directory = session.meta.directories.get(id).ok_or_else(unknown)?;
                    if directory.parent.is_none() {
                        Choice::All
                    } else {
                        let mut parts = Vec::new();
                        while let Some(parent) = &directory.parent {
                            parts.push(directory.label.as_str());
                            directory = session.meta.directories.get(parent).ok_or_else(unknown)?;
                        }
                        parts.reverse();
                        Choice::Subtree(parts.join("/"))
                    }
                }
                _ => {
                    return Err(AppError::new(
                        "invalid_input",
                        "Genau eine Datei oder ein geöffneter Ordner muss gewählt sein",
                    ))
                }
            };
            let reader = session.reader.take().ok_or_else(busy)?;
            slot.remember(request);
            slot.touched = Instant::now();
            slot.read = Some(Work {
                request: request.into(),
                generation: slot.generation,
                cancel: cancel.clone(),
            });
            (reader, choice, slot.generation)
        };
        // Die Sitzung gibt ihren Reader nur für diese gebundene Arbeit ab. Kein
        // Dateizugriff hält den fensterübergreifenden Registrylock.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let selection = match choice {
                Choice::All => doppeldecker_core::archive_extract::ArchiveExtractSelection::All,
                Choice::Entry(id) => reader.extract_entry_selection(id, cancel)?,
                Choice::Subtree(prefix) => reader.extract_subtree_selection(&prefix, cancel)?,
            };
            let digest = reader.source_binding_digest(cancel)?;
            let (root, path) = reader.source_paths();
            let (snapshot, validated) = match &reader.origin {
                ArchiveOrigin::Local => (None, None),
                ArchiveOrigin::Snapshot(origin) => {
                    (Some(origin.source.clone()), Some(origin.validated.clone()))
                }
            };
            Ok(ExtractSource {
                root: if snapshot.is_some() {
                    Default::default()
                } else {
                    root.to_owned()
                },
                path: if snapshot.is_some() {
                    Default::default()
                } else {
                    path.to_owned()
                },
                selection,
                digest,
                snapshot,
                validated,
            })
        }))
        .unwrap_or_else(|_| {
            Err(AppError::new(
                "io",
                "Archivauswahlworker wurde unerwartet beendet",
            ))
        });
        self.finish_access(window, session_id, request, generation, reader, result)
    }
    fn finish_read(
        &self,
        window: &str,
        id: &str,
        request: &str,
        generation: u64,
        reader: ArchiveSource,
        result: Result<FilePreview, AppError>,
    ) -> Result<FilePreview, AppError> {
        self.finish_access(window, id, request, generation, reader, result)
    }
    fn finish_access<T>(
        &self,
        window: &str,
        id: &str,
        request: &str,
        generation: u64,
        reader: ArchiveSource,
        result: Result<T, AppError>,
    ) -> Result<T, AppError> {
        let mut slots = self.slots.lock_recover();
        let slot = slots.get_mut(window).ok_or_else(interrupted)?;
        let work = slot
            .read
            .take()
            .filter(|work| work.generation == generation && work.request == request)
            .ok_or_else(interrupted)?;
        if slot.generation != generation || slot.destroyed {
            return Err(interrupted());
        }
        let session = slot
            .session
            .as_mut()
            .filter(|s| s.id == id)
            .ok_or_else(interrupted)?;
        session.reader = Some(reader);
        slot.touched = Instant::now();
        if work.cancel.is_cancelled() {
            Err(interrupted())
        } else {
            result
        }
    }
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchiveOpenRequest {
    endpoint: BidirectionalEndpointRequest,
    path: String,
}
fn validate_open_request(request: &ArchiveOpenRequest) -> Result<(), AppError> {
    crate::bidirectional_commands::validate_endpoint_request_root(&request.endpoint)?;
    crate::browser_commands::validate_diff_path(request.endpoint.root(), &request.path)?;
    Ok(())
}
fn open_source(
    app: &tauri::AppHandle,
    request: &ArchiveOpenRequest,
    cancel: &Cancel,
) -> Result<ArchiveSource, AppError> {
    validate_open_request(request)?;
    mapping_cancel(cancel)?;
    if matches!(request.endpoint, BidirectionalEndpointRequest::Local { .. }) {
        return open_local_source(
            request.endpoint.root(),
            &request.path,
            &crate::sandbox_root()?,
            cancel,
        )
        .map(ArchiveSource::from);
    }
    let state = app.state::<crate::AppState>();
    let (endpoint, _, bindings) = resolve_endpoint(app, &state, &request.endpoint)?;
    mapping_cancel(cancel)?;
    let cache = crate::browser_commands::snapshot_cache(app, "archive-snapshots")?;
    doppeldecker_core::cleanup_remote_snapshots(
        &cache,
        std::time::SystemTime::now(),
        doppeldecker_core::SNAPSHOT_RETENTION,
    )
    .map_err(AppError::from)?;
    mapping_cancel(cancel)?;
    let snapshot = endpoint
        .download_interactive_snapshot(Path::new(&request.path), &cache, cancel)
        .map_err(AppError::from)?;
    mapping_cancel(cancel)?;
    let protocol = match request.endpoint.provider_kind() {
        doppeldecker_core::ProviderKind::Sftp => "SFTP",
        doppeldecker_core::ProviderKind::Ftps => "FTP/FTPS",
        doppeldecker_core::ProviderKind::WebDav => "WebDAV",
        doppeldecker_core::ProviderKind::Smb => "SMB",
        doppeldecker_core::ProviderKind::Local => unreachable!(),
    };
    let store = doppeldecker_core::remote_snapshot::SnapshotStore::open(&cache)?;
    let profile = bindings
        .first()
        .cloned()
        .ok_or_else(|| AppError::new("invalid_input", "Archivmomentaufnahme ohne Profilbindung"))?;
    let source = open_snapshot_source(
        &snapshot,
        &store,
        format!("{protocol} · {}", request.path),
        profile,
        cancel,
    )?;
    crate::queue_runtime::ensure_profile_bindings_current(app, &bindings)?;
    mapping_cancel(cancel)?;
    Ok(source)
}
fn open_snapshot_source(
    snapshot: &doppeldecker_core::RemoteSnapshot,
    store: &doppeldecker_core::remote_snapshot::SnapshotStore,
    origin: String,
    profile: doppeldecker_core::RemoteProfileBinding,
    cancel: &Cancel,
) -> Result<ArchiveSource, AppError> {
    let validated = store.capture(snapshot, cancel)?;
    let reader = ArchiveRead::open_snapshot(validated.clone(), CatalogBudget::default(), cancel)?;
    Ok(ArchiveSource {
        reader,
        origin: ArchiveOrigin::Snapshot(Box::new(SnapshotArchiveOrigin {
            label: origin.clone(),
            source: doppeldecker_core::archive_extract::ArchiveSnapshotSource {
                reference: validated.reference().clone(),
                origin,
                profile,
            },
            validated,
        })),
    })
}
fn open_local_source(
    root: &str,
    path: &str,
    sandbox: &Option<std::path::PathBuf>,
    cancel: &Cancel,
) -> Result<ArchiveRead, AppError> {
    crate::browser_commands::validate_diff_path(root, path)?;
    let normalized =
        doppeldecker_core::validate_file_operation_root(Path::new(root)).map_err(AppError::from)?;
    if normalized != Path::new(root) {
        return Err(AppError::new("invalid_input", "Ungültiger Pane-Root"));
    }
    doppeldecker_core::fileops::guard(sandbox, &normalized).map_err(AppError::from)?;
    doppeldecker_core::fileops::guard(sandbox, Path::new(path)).map_err(AppError::from)?;
    let relative = Path::new(path).strip_prefix(&normalized).map_err(|_| {
        AppError::new(
            "invalid_input",
            "Archivquelle liegt außerhalb des Pane-Roots",
        )
    })?;
    ArchiveRead::open(&normalized, relative, CatalogBudget::default(), cancel)
        .map_err(AppError::from)
}
fn read_preview(
    reader: &mut ArchiveRead,
    entry: ArchiveEntryId,
    cancel: &Cancel,
) -> Result<FilePreview, AppError> {
    let mut bytes = Vec::new();
    let verified = reader
        .read_entry_to(
            entry,
            ReadBudget {
                max_output_bytes: PREVIEW_MAX_BYTES,
                max_io_bytes: 16 * 1024 * 1024,
                max_read_calls: 100_000,
            },
            cancel,
            &mut bytes,
        )
        .map_err(AppError::from)?;
    Ok(FilePreview {
        bytes: verified.output_bytes,
        content: classify_preview_bytes(&bytes),
        limitations: Vec::new(),
    })
}
#[tauri::command]
pub(crate) async fn open_archive_read(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<'_, ArchiveReadState>,
    request_id: String,
    request: ArchiveOpenRequest,
) -> Result<Opened, AppError> {
    validate_open_request(&request)?;
    let runtime = state.inner().clone();
    let label = window.label().to_string();
    let (generation, cancel, id) = runtime.reserve_open(&label, &request_id)?;
    let timer = runtime.clone();
    let timer_label = label.clone();
    if runtime.start_timer(&label) {
        tauri::async_runtime::spawn(async move {
            let mut wait = IDLE;
            loop {
                tokio::time::sleep(wait).await;
                match timer.timer_tick(&timer_label, Instant::now()) {
                    Some(next) => wait = next,
                    None => break,
                }
            }
        });
    }
    tauri::async_runtime::spawn_blocking(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            open_source(&app, &request, &cancel)
        }))
        .unwrap_or_else(|_| Err(AppError::new("io", "Archivworker wurde unerwartet beendet")));
        runtime.finish_open(&label, &request_id, generation, id, result)
    })
    .await
    .map_err(|_| AppError::new("io", "Archivworker wurde unterbrochen"))?
}
#[tauri::command]
pub(crate) fn list_archive_directory(
    window: WebviewWindow,
    state: State<'_, ArchiveReadState>,
    session_id: String,
    directory_id: String,
    page: u32,
) -> Result<Listing, AppError> {
    state.page(window.label(), &session_id, &directory_id, page)
}
#[tauri::command]
pub(crate) async fn preview_archive_entry(
    window: WebviewWindow,
    state: State<'_, ArchiveReadState>,
    request_id: String,
    session_id: String,
    entry_id: String,
) -> Result<Previewed, AppError> {
    let runtime = state.inner().clone();
    let label = window.label().to_string();
    let (mut reader, entry, generation, cancel) =
        runtime.begin_read(&label, &session_id, &request_id, &entry_id)?;
    tauri::async_runtime::spawn_blocking(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            read_preview(&mut reader, entry, &cancel)
        }))
        .unwrap_or_else(|_| {
            Err(AppError::new(
                "io",
                "Vorschauworker wurde unerwartet beendet",
            ))
        });
        runtime
            .finish_read(&label, &session_id, &request_id, generation, reader, result)
            .map(|preview| Previewed {
                request_id,
                session_id,
                entry_id,
                preview,
            })
    })
    .await
    .map_err(|_| AppError::new("io", "Vorschauworker wurde unterbrochen"))?
}
#[tauri::command]
pub(crate) fn cancel_archive_read(
    window: WebviewWindow,
    state: State<'_, ArchiveReadState>,
    request_id: String,
) {
    state.cancel(window.label(), &request_id);
}
#[tauri::command]
pub(crate) fn close_archive_read(
    window: WebviewWindow,
    state: State<'_, ArchiveReadState>,
    open_request_id: String,
    session_id: Option<String>,
) {
    state.close(window.label(), &open_request_id, session_id.as_deref());
}

#[cfg(test)]
mod tests {
    use super::*;
    use doppeldecker_core::preview::PreviewContent;
    use std::io::Write;
    fn fixture(items: &[(&[u8], &[u8], u32)]) -> Vec<u8> {
        fn n16(out: &mut Vec<u8>, n: u16) {
            out.extend(n.to_le_bytes());
        }
        fn n32(out: &mut Vec<u8>, n: u32) {
            out.extend(n.to_le_bytes());
        }
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data, mode) in items {
            let offset = out.len() as u32;
            let mut crc = !0u32;
            for byte in *data {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb88320u32 & 0u32.wrapping_sub(crc & 1));
                }
            }
            crc = !crc;
            n32(&mut out, 0x04034b50);
            for v in [20, 0, 0, 0, 0] {
                n16(&mut out, v);
            }
            for v in [crc, data.len() as u32, data.len() as u32] {
                n32(&mut out, v);
            }
            n16(&mut out, name.len() as u16);
            n16(&mut out, 0);
            out.extend(*name);
            out.extend(*data);
            n32(&mut central, 0x02014b50);
            for v in [(3 << 8) | 20, 20, 0, 0, 0, 0] {
                n16(&mut central, v);
            }
            for v in [crc, data.len() as u32, data.len() as u32] {
                n32(&mut central, v);
            }
            for v in [name.len() as u16, 0, 0, 0, 0] {
                n16(&mut central, v);
            }
            n32(&mut central, mode << 16);
            n32(&mut central, offset);
            central.extend(*name);
        }
        let offset = out.len() as u32;
        let length = central.len() as u32;
        out.extend(central);
        n32(&mut out, 0x06054b50);
        for v in [0, 0, items.len() as u16, items.len() as u16] {
            n16(&mut out, v);
        }
        n32(&mut out, length);
        n32(&mut out, offset);
        n16(&mut out, 0);
        out
    }
    fn source(bytes: &[u8]) -> (tempfile::TempDir, ArchiveRead) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.zip"), bytes).unwrap();
        let reader = ArchiveRead::open(
            temp.path(),
            Path::new("a.zip"),
            CatalogBudget::default(),
            &Cancel::new(),
        )
        .unwrap();
        (temp, reader)
    }
    fn opened(
        state: &ArchiveReadState,
        window: &str,
        request: &str,
        reader: ArchiveRead,
    ) -> Opened {
        let (generation, _, id) = state.reserve_open(window, request).unwrap();
        state
            .finish_open(window, request, generation, id, Ok(reader.into()))
            .unwrap()
    }
    fn test_snapshot_source(
        snapshot: &doppeldecker_core::RemoteSnapshot,
        cache: &Path,
    ) -> Result<ArchiveSource, AppError> {
        let store =
            doppeldecker_core::remote_snapshot::SnapshotStore::open(&cache.canonicalize()?)?;
        open_snapshot_source(
            snapshot,
            &store,
            "SFTP · /daten/Archiv.zip".into(),
            doppeldecker_core::RemoteProfileBinding::new(
                doppeldecker_core::RemoteProfileProtocol::Sftp,
                "artificial",
                1,
            )?,
            &Cancel::new(),
        )
    }
    #[test]
    fn remote_momentaufnahme_erlaubt_gehaltene_datei_und_ordnerauswahl() {
        let (temp, _) = source(&fixture(&[(b"A/text.txt", b"abc", 0o100644)]));
        let cache = tempfile::tempdir().unwrap();
        let snapshot = doppeldecker_core::download_remote_snapshot(
            &doppeldecker_core::LocalVfs::new(),
            &temp.path().join("a.zip").canonicalize().unwrap(),
            cache.path(),
            &Cancel::new(),
        )
        .unwrap();
        let reader = test_snapshot_source(&snapshot, cache.path()).unwrap();
        let state = ArchiveReadState::default();
        let (generation, _, id) = state.reserve_open("one", "remote").unwrap();
        let open = state
            .finish_open("one", "remote", generation, id, Ok(reader))
            .unwrap();
        assert!(open.can_extract);
        assert_eq!(open.origin.as_deref(), Some("SFTP · /daten/Archiv.zip"));
        let folder = open.listing.rows[0].directory_id.as_deref().unwrap();
        let bound_folder = state
            .resolve_extract_selection(
                "one",
                &open.session_id,
                "folder-extract",
                None,
                Some(folder),
                &Cancel::new(),
            )
            .unwrap();
        assert!(matches!(bound_folder.selection,
            doppeldecker_core::archive_extract::ArchiveExtractSelection::Subtree { ref prefix } if prefix == "A"));
        let listing = state.page("one", &open.session_id, folder, 0).unwrap();
        let entry = listing.rows[0].entry_id.as_deref().unwrap();
        let selected = state
            .resolve_extract_selection(
                "one",
                &open.session_id,
                "extract",
                Some(entry),
                None,
                &Cancel::new(),
            )
            .unwrap();
        assert!(selected.root.as_os_str().is_empty());
        assert!(selected.path.as_os_str().is_empty());
        assert!(selected.snapshot.is_some());
        assert!(selected.validated.is_some());
        assert!(state
            .resolve_extract_selection(
                "other",
                &open.session_id,
                "foreign",
                Some(entry),
                None,
                &Cancel::new()
            )
            .is_err());
        let (mut reader, core_id, generation, cancel) = state
            .begin_read("one", &open.session_id, "preview-after-rejection", entry)
            .unwrap();
        let preview = read_preview(&mut reader, core_id, &cancel);
        let preview = state
            .finish_read(
                "one",
                &open.session_id,
                "preview-after-rejection",
                generation,
                reader,
                preview,
            )
            .unwrap();
        assert!(matches!(preview.content, PreviewContent::Text { text } if text == "abc"));
    }
    #[test]
    fn uebergebene_snapshotdatei_ueberlebt_browser_close_ohne_cachepfad_im_request() {
        let (temp, _) = source(&fixture(&[(b"A/text.txt", b"snapshot content", 0o100644)]));
        let cache = tempfile::tempdir().unwrap();
        let snapshot = doppeldecker_core::download_remote_snapshot(
            &doppeldecker_core::LocalVfs::new(),
            &temp.path().join("a.zip").canonicalize().unwrap(),
            cache.path(),
            &Cancel::new(),
        )
        .unwrap();
        let state = ArchiveReadState::default();
        let (generation, _, id) = state.reserve_open("one", "remote").unwrap();
        let open = state
            .finish_open(
                "one",
                "remote",
                generation,
                id,
                test_snapshot_source(&snapshot, cache.path()),
            )
            .unwrap();
        assert!(open.can_extract_file);
        assert!(open.can_extract);
        let folder = open.listing.rows[0].directory_id.as_deref().unwrap();
        let listing = state.page("one", &open.session_id, folder, 0).unwrap();
        let selected = state
            .resolve_extract_selection(
                "one",
                &open.session_id,
                "extract",
                listing.rows[0].entry_id.as_deref(),
                None,
                &Cancel::new(),
            )
            .unwrap();
        state.close("one", "remote", Some(&open.session_id));
        let target = temp.path().canonicalize().unwrap().join("target");
        std::fs::create_dir(&target).unwrap();
        let request = doppeldecker_core::archive_extract::ArchiveExtractRequest {
            source_root: selected.root,
            source_path: selected.path,
            snapshot: selected.snapshot,
            target_dir: target.clone(),
            selection: selected.selection,
            options: Default::default(),
            sandbox: Some(temp.path().canonicalize().unwrap()),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains(&snapshot.path));
        let prepared = request
            .prepare_with_snapshot(selected.validated.unwrap(), &Cancel::new())
            .unwrap();
        prepared.verify_selection_source(selected.digest).unwrap();
        let queue =
            doppeldecker_core::OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let entry = queue
            .enqueue(doppeldecker_core::QueueOperation::ArchiveExtract {
                request: Box::new(request.clone()),
                expected: Box::new(prepared.binding().unwrap()),
            })
            .unwrap();
        let context = serde_json::to_value(
            crate::operation_events::OperationViewContext::for_entry(temp.path(), &entry),
        )
        .unwrap();
        assert_eq!(context["affectedEndpoints"].as_array().unwrap().len(), 1);
        assert_eq!(
            context["affectedEndpoints"][0]["path"],
            target.to_string_lossy().as_ref()
        );
        let prepared = prepared
            .with_options(Default::default(), &Cancel::new())
            .unwrap();
        let report = prepared.execute(None, &mut |_| {}).unwrap();
        assert_eq!(report.files, 1);
        assert_eq!(
            std::fs::read(target.join("text.txt")).unwrap(),
            b"snapshot content"
        );
    }
    #[test]
    fn momentaufnahme_bleibt_beim_gehaltenen_worker_nach_close_und_timeout_erhalten() {
        const CHILD: &str = "DOPPELDECKER_SNAPSHOT_WORKER_TEST_CHILD";
        if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("1")) {
            // Parallele Prozessstarts können Manifest-FDs bis zum exec erben.
            // Ein dann noch belegter Lock ist eine gültige Cleanup-Auslassung.
            // Diese Lebensdauerprobe braucht deshalb einen eigenen Testprozess.
            let binary = std::env::current_exe().unwrap();
            let test = "archive_read_commands::tests::momentaufnahme_bleibt_beim_gehaltenen_worker_nach_close_und_timeout_erhalten";
            let list = std::process::Command::new(&binary)
                .args([test, "--list", "--exact"])
                .output()
                .unwrap();
            assert!(list.status.success());
            assert_eq!(
                String::from_utf8(list.stdout)
                    .unwrap()
                    .lines()
                    .filter(|line| *line == format!("{test}: test"))
                    .count(),
                1
            );
            let output = std::process::Command::new(binary)
                .args([test, "--exact", "--nocapture"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "Isolierte Snapshot-Lebensdauerprobe fehlgeschlagen:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        for timeout in [false, true] {
            let (temp, _) = source(&fixture(&[(b"A/text.txt", b"snapshot content", 0o100644)]));
            let cache = tempfile::tempdir().unwrap();
            let snapshot = doppeldecker_core::download_remote_snapshot(
                &doppeldecker_core::LocalVfs::new(),
                &temp.path().join("a.zip").canonicalize().unwrap(),
                cache.path(),
                &Cancel::new(),
            )
            .unwrap();
            let source = test_snapshot_source(&snapshot, cache.path()).unwrap();
            // Die schon geöffnete Momentaufnahme bleibt vom Remoteoriginal unabhängig.
            std::fs::write(temp.path().join("a.zip"), b"remote changed").unwrap();
            let state = ArchiveReadState::default();
            let (generation, _, id) = state.reserve_open("one", "remote").unwrap();
            let open = state
                .finish_open("one", "remote", generation, id, Ok(source))
                .unwrap();
            let folder = open.listing.rows[0].directory_id.as_deref().unwrap();
            assert!(state.page("other", &open.session_id, folder, 0).is_err());
            let listing = state.page("one", &open.session_id, folder, 0).unwrap();
            let entry = listing.rows[0].entry_id.as_deref().unwrap();
            let (mut reader, core_id, _, cancel) = state
                .begin_read("one", &open.session_id, "preview", entry)
                .unwrap();
            assert!(
                matches!(read_preview(&mut reader, core_id, &cancel).unwrap().content, PreviewContent::Text { text } if text == "snapshot content")
            );
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                assert!(cancel.is_cancelled());
                drop(reader);
            });
            ready_rx.recv().unwrap();
            if timeout {
                state.timer_tick("one", Instant::now() + IDLE);
            } else {
                state.close("one", "remote", Some(&open.session_id));
            }
            assert!(state.page("one", &open.session_id, folder, 0).is_err());
            let future = std::time::SystemTime::now()
                + doppeldecker_core::SNAPSHOT_RETENTION
                + Duration::from_secs(1);
            assert_eq!(
                doppeldecker_core::cleanup_remote_snapshots(
                    cache.path(),
                    future,
                    doppeldecker_core::SNAPSHOT_RETENTION
                )
                .unwrap(),
                0
            );
            assert!(Path::new(&snapshot.path).is_file());
            release_tx.send(()).unwrap();
            worker.join().unwrap();
            assert_eq!(
                doppeldecker_core::cleanup_remote_snapshots(
                    cache.path(),
                    future,
                    doppeldecker_core::SNAPSHOT_RETENTION
                )
                .unwrap(),
                1
            );
        }
    }
    #[test]
    fn ganze_browserwurzel_aus_tiefe_und_unterbaum_bleiben_fenstergebunden() {
        for remote in [false, true] {
            let (temp, reader) = source(&fixture(&[
                (b"outer/sub/file", b"snapshot bytes", 0o100644),
                (b"outer/sub/empty/", b"", 0o040755),
                (b"outer/sibling", b"sibling", 0o100644),
            ]));
            let cache = tempfile::tempdir().unwrap();
            let source = if remote {
                let downloaded = doppeldecker_core::download_remote_snapshot(
                    &doppeldecker_core::LocalVfs::new(),
                    &temp.path().join("a.zip").canonicalize().unwrap(),
                    cache.path(),
                    &Cancel::new(),
                )
                .unwrap();
                test_snapshot_source(&downloaded, cache.path()).unwrap()
            } else {
                reader.into()
            };
            let state = ArchiveReadState::default();
            let (generation, _, id) = state.reserve_open("one", "open").unwrap();
            let open = state
                .finish_open("one", "open", generation, id, Ok(source))
                .unwrap();
            let outer = open
                .listing
                .rows
                .iter()
                .find_map(|row| row.directory_id.as_deref())
                .unwrap();
            let outer_page = state.page("one", &open.session_id, outer, 0).unwrap();
            let sub = outer_page
                .rows
                .iter()
                .find(|row| row.display_name == "sub")
                .unwrap()
                .directory_id
                .as_deref()
                .unwrap();
            let deep_page = state.page("one", &open.session_id, sub, 0).unwrap();
            let root = &deep_page.breadcrumbs[0].directory_id;
            let child = deep_page
                .rows
                .iter()
                .find_map(|row| row.entry_id.as_deref())
                .unwrap();
            let all = state
                .resolve_extract_selection(
                    "one",
                    &open.session_id,
                    "all",
                    None,
                    Some(root),
                    &Cancel::new(),
                )
                .unwrap();
            assert!(matches!(
                all.selection,
                doppeldecker_core::archive_extract::ArchiveExtractSelection::All
            ));
            let selected = state
                .resolve_extract_selection(
                    "one",
                    &open.session_id,
                    "subtree",
                    None,
                    Some(sub),
                    &Cancel::new(),
                )
                .unwrap();
            assert!(
                matches!(selected.selection, doppeldecker_core::archive_extract::ArchiveExtractSelection::Subtree { ref prefix } if prefix == "outer/sub")
            );
            assert_eq!(all.snapshot.is_some(), remote);
            assert_eq!(selected.snapshot.is_some(), remote);
            assert!(state
                .resolve_extract_selection(
                    "one",
                    &open.session_id,
                    "mixed",
                    Some(child),
                    Some(sub),
                    &Cancel::new()
                )
                .is_err());
            assert!(state
                .resolve_extract_selection(
                    "other",
                    &open.session_id,
                    "foreign",
                    None,
                    Some(root),
                    &Cancel::new()
                )
                .is_err());
            state.close("one", "open", Some(&open.session_id));
            assert!(state
                .resolve_extract_selection(
                    "one",
                    &open.session_id,
                    "closed",
                    None,
                    Some(sub),
                    &Cancel::new()
                )
                .is_err());
        }
    }
    #[test]
    fn extraktionsauswahl_bleibt_nach_browserende_quellgebunden() {
        let (temp, reader) = source(&fixture(&[(b"A/B/file.txt", b"abc", 0o100644)]));
        let state = ArchiveReadState::default();
        let open = opened(&state, "one", "open", reader);
        assert!(open.can_extract);
        let a = open.listing.rows[0].directory_id.as_ref().unwrap();
        let page = state.page("one", &open.session_id, a, 0).unwrap();
        let b = page.rows[0].directory_id.as_ref().unwrap();
        let page = state.page("one", &open.session_id, b, 0).unwrap();
        let id = page.rows[0].entry_id.as_ref().unwrap();
        assert!(state
            .resolve_extract_selection(
                "other",
                &open.session_id,
                "foreign",
                Some(id),
                None,
                &Cancel::new()
            )
            .is_err());
        let cancelled = Cancel::new();
        cancelled.cancel();
        assert!(state
            .resolve_extract_selection(
                "one",
                &open.session_id,
                "cancelled",
                Some(id),
                None,
                &cancelled
            )
            .is_err());
        let source = state
            .resolve_extract_selection(
                "one",
                &open.session_id,
                "select",
                Some(id),
                None,
                &Cancel::new(),
            )
            .unwrap();
        state.close("one", "open", Some(&open.session_id));
        let target = temp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let request = doppeldecker_core::archive_extract::ArchiveExtractRequest {
            snapshot: None,
            source_root: std::fs::canonicalize(source.root).unwrap(),
            source_path: std::fs::canonicalize(source.path).unwrap(),
            target_dir: std::fs::canonicalize(target).unwrap(),
            options: Default::default(),
            sandbox: None,
            selection: source.selection,
        };
        let prepared = request.prepare(&Cancel::new()).unwrap();
        prepared.verify_selection_source(source.digest).unwrap();
        assert_eq!(prepared.preview().relative_root, "file.txt");
        assert_eq!(prepared.preview().selection_path, "A/B/file.txt");
        std::fs::write(
            temp.path().join("a.zip"),
            fixture(&[(b"A/B/file.txt", b"changed", 0o100644)]),
        )
        .unwrap();
        assert!(request
            .prepare(&Cancel::new())
            .unwrap()
            .verify_selection_source(source.digest)
            .is_err());
    }
    #[test]
    fn extraktionsauswahl_ordner_und_fremde_oder_geschlossene_ids() {
        let (temp, reader) = source(&fixture(&[
            (b"A/B/Leer/", b"", 0o040755),
            (b"A/B/file", b"abc", 0o100644),
        ]));
        let state = ArchiveReadState::default();
        let open = opened(&state, "one", "open", reader);
        let root = state
            .resolve_extract_selection(
                "one",
                &open.session_id,
                "root",
                None,
                Some(&open.listing.directory_id),
                &Cancel::new(),
            )
            .unwrap();
        assert!(matches!(
            root.selection,
            doppeldecker_core::archive_extract::ArchiveExtractSelection::All
        ));
        assert!(state
            .resolve_extract_selection(
                "one",
                &open.session_id,
                "foreign",
                Some("foreign-entry"),
                None,
                &Cancel::new()
            )
            .is_err());
        let a = open.listing.rows[0].directory_id.as_ref().unwrap();
        let page = state.page("one", &open.session_id, a, 0).unwrap();
        let b = page.rows[0].directory_id.as_ref().unwrap();
        let bound = state
            .resolve_extract_selection(
                "one",
                &open.session_id,
                "subtree",
                None,
                Some(b),
                &Cancel::new(),
            )
            .unwrap();
        assert!(
            matches!(bound.selection, doppeldecker_core::archive_extract::ArchiveExtractSelection::Subtree { ref prefix } if prefix == "A/B")
        );
        // Ein gleichnamig ersetztes ZIP darf bereits vor dem Auswahlbinding nicht
        // auf eine andere zentrale Reihenfolge oder neue Inhalte zeigen.
        std::fs::write(
            temp.path().join("replacement"),
            fixture(&[(b"A/B/file", b"new", 0o100644)]),
        )
        .unwrap();
        std::fs::rename(temp.path().join("replacement"), temp.path().join("a.zip")).unwrap();
        assert!(state
            .resolve_extract_selection(
                "one",
                &open.session_id,
                "swapped",
                None,
                Some(b),
                &Cancel::new()
            )
            .is_err());
        state.close("one", "open", Some(&open.session_id));
        assert!(state
            .resolve_extract_selection(
                "one",
                &open.session_id,
                "closed",
                None,
                Some(b),
                &Cancel::new()
            )
            .is_err());
    }
    #[test]
    fn katalog_erhaelt_leere_und_doppelte_ordner_sowie_datei_ordner_kollisionen() {
        let (temp, reader) = source(&fixture(&[
            (b"empty/", b"", 0o040755),
            (b"dup/", b"", 0o040755),
            (b"dup/", b"", 0o040755),
            (b"same", b"text", 0o100644),
            (b"same/", b"", 0o040755),
            (b"same/child", b"child", 0o100644),
            (b"a/b/file", b"nested", 0o100644),
            (b"link/", b"target", 0o120777),
            (b"\xff", b"invalid", 0o100644),
        ]));
        let meta = metadata(&reader, "session", &Cancel::new()).unwrap();
        let root = listing("session", &meta, &meta.root_id, 0).unwrap();
        assert_eq!(root.catalog.entry_count, 9);
        assert_eq!(root.catalog.problem_count, 2);
        assert_eq!(
            root.rows.iter().filter(|r| r.display_name == "dup").count(),
            2
        );
        let dup: Vec<_> = root
            .rows
            .iter()
            .filter(|r| r.display_name == "dup")
            .collect();
        assert_ne!(dup[0].entry_id, dup[1].entry_id);
        assert_eq!(dup[0].directory_id, dup[1].directory_id);
        assert_eq!(
            root.rows
                .iter()
                .filter(|r| r.display_name == "same")
                .count(),
            2
        );
        let empty = root
            .rows
            .iter()
            .find(|r| r.display_name == "empty")
            .unwrap();
        assert!(
            listing("session", &meta, empty.directory_id.as_ref().unwrap(), 0)
                .unwrap()
                .rows
                .is_empty()
        );
        let a = root.rows.iter().find(|r| r.display_name == "a").unwrap();
        assert!(a.entry_id.is_none());
        let link = root.rows.iter().find(|r| r.display_name == "link").unwrap();
        assert!(link.directory_id.is_none());
        assert!(!link.can_preview);
        let raw = root
            .rows
            .iter()
            .find(|r| r.display_name.contains("FF"))
            .unwrap();
        assert_eq!(raw.kind, "problem");
        assert!(!raw.can_preview);
        drop(temp);
    }
    #[test]
    fn ids_sind_fenstergebunden_und_fremde_eintraege_werden_abgelehnt() {
        let state = ArchiveReadState::default();
        let (_a, reader) = source(&fixture(&[(b"file", b"hello", 0o100644)]));
        let a = opened(&state, "main", "open-a", reader);
        let (_b, reader) = source(&fixture(&[(b"other", b"world", 0o100644)]));
        let b = opened(&state, "window-2", "open-b", reader);
        assert!(state
            .page("window-2", &a.session_id, &a.listing.directory_id, 0)
            .is_err());
        assert!(state
            .begin_read(
                "main",
                &a.session_id,
                "r",
                b.listing.rows[0].entry_id.as_ref().unwrap()
            )
            .is_err());
        assert!(state
            .page("main", &a.session_id, &b.listing.directory_id, 0)
            .is_err());
    }
    #[test]
    fn fruehes_cancel_close_und_verspaetete_generationen_publizieren_nichts() {
        let state = ArchiveReadState::default();
        state.cancel("main", "early");
        assert!(state.reserve_open("main", "early").is_err());
        state.close("main", "closed", None);
        assert!(state.reserve_open("main", "closed").is_err());
        let (generation, cancel, id) = state.reserve_open("main", "a").unwrap();
        state.close("main", "a", None);
        assert!(cancel.is_cancelled());
        assert!(state.reserve_open("main", "b").is_err());
        let (_temp, reader) = source(&fixture(&[]));
        assert!(state
            .finish_open("main", "a", generation, id, Ok(reader.into()))
            .is_err());
        let (generation, _, id) = state.reserve_open("main", "b").unwrap();
        let (_temp2, reader) = source(&fixture(&[]));
        let b = state
            .finish_open("main", "b", generation, id, Ok(reader.into()))
            .unwrap();
        state.close("main", "a", None);
        assert!(state
            .page("main", &b.session_id, &b.listing.directory_id, 0)
            .is_ok());
        for i in 0..1000 {
            state.cancel("main", &format!("cancel-{i}"));
        }
        assert_eq!(
            state.slots.lock_recover()["main"].tombstones.len(),
            TOMBSTONES
        );
    }
    #[test]
    fn ablauf_und_destroy_verwerfen_sitzungen_und_aktive_worker() {
        let state = ArchiveReadState::default();
        let (_temp, reader) = source(&fixture(&[]));
        let a = opened(&state, "main", "a", reader);
        assert!(state.timer_tick("main", Instant::now() + IDLE).is_none());
        assert!(state
            .page("main", &a.session_id, &a.listing.directory_id, 0)
            .is_err());
        let (generation, cancel, id) = state.reserve_open("main", "b").unwrap();
        state.destroy_window("main");
        assert!(cancel.is_cancelled());
        let (_temp, reader) = source(&fixture(&[]));
        assert!(state
            .finish_open("main", "b", generation, id, Ok(reader.into()))
            .is_err());
        assert!(state.reserve_open("main", "c").is_err());
    }
    #[test]
    fn geschlossene_lesearbeit_endet_vor_neuer_sitzung() {
        let state = ArchiveReadState::default();
        let (_temp_a, reader_a) = source(&fixture(&[(b"a", b"first", 0o100644)]));
        let a = opened(&state, "main", "open-a", reader_a);
        let (reader_a, _, gen_a, _) = state
            .begin_read(
                "main",
                &a.session_id,
                "read-a",
                a.listing.rows[0].entry_id.as_deref().unwrap(),
            )
            .unwrap();
        state.close("main", "open-a", Some(&a.session_id));
        assert!(state.reserve_open("main", "open-b").is_err());
        assert!(state
            .finish_read(
                "main",
                &a.session_id,
                "read-a",
                gen_a,
                reader_a,
                Err(interrupted())
            )
            .is_err());
        let (_temp_b, reader_b) = source(&fixture(&[(b"b", b"second", 0o100644)]));
        let b = opened(&state, "main", "open-b", reader_b);
        let (mut reader_b, core_b, gen_b, cancel_b) = state
            .begin_read(
                "main",
                &b.session_id,
                "read-b",
                b.listing.rows[0].entry_id.as_deref().unwrap(),
            )
            .unwrap();
        let preview = read_preview(&mut reader_b, core_b, &cancel_b);
        assert!(state
            .finish_read("main", &b.session_id, "read-b", gen_b, reader_b, preview)
            .is_ok());
    }
    #[test]
    fn einzelabbruch_laesst_navigation_und_spaetere_vorschau_nutzbar() {
        let state = ArchiveReadState::default();
        let (_temp, reader) = source(&fixture(&[
            (b"a", b"first", 0o100644),
            (b"b", b"second", 0o100644),
        ]));
        let a = opened(&state, "main", "open", reader);
        let entry = a.listing.rows[0].entry_id.as_ref().unwrap();
        let (mut reader, core, generation, cancel) = state
            .begin_read("main", &a.session_id, "r1", entry)
            .unwrap();
        assert!(state
            .begin_read("main", &a.session_id, "r2", entry)
            .is_err());
        state.cancel("window-2", "r1");
        assert!(!cancel.is_cancelled());
        state.cancel("main", "r1");
        assert!(state
            .page("main", &a.session_id, &a.listing.directory_id, 0)
            .is_ok());
        let result = read_preview(&mut reader, core, &cancel);
        assert!(result.is_err());
        assert!(state
            .finish_read("main", &a.session_id, "r1", generation, reader, result)
            .is_err());
        let (mut reader, core, generation, cancel) = state
            .begin_read(
                "main",
                &a.session_id,
                "r2",
                a.listing.rows[1].entry_id.as_ref().unwrap(),
            )
            .unwrap();
        let result = read_preview(&mut reader, core, &cancel);
        let result = state
            .finish_read("main", &a.session_id, "r2", generation, reader, result)
            .unwrap();
        assert!(matches!(result.content, PreviewContent::Text {text} if text == "second"));
    }
    #[test]
    fn quellendrift_verhindert_vorschauerfolg() {
        let state = ArchiveReadState::default();
        let (temp, reader) = source(&fixture(&[(b"file", b"original", 0o100644)]));
        let a = opened(&state, "main", "open", reader);
        let (mut reader, core, generation, cancel) = state
            .begin_read(
                "main",
                &a.session_id,
                "r",
                a.listing.rows[0].entry_id.as_ref().unwrap(),
            )
            .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(temp.path().join("a.zip"))
            .unwrap()
            .write_all(b"changed")
            .unwrap();
        let result = read_preview(&mut reader, core, &cancel);
        assert!(result.is_err());
        assert!(state
            .finish_read("main", &a.session_id, "r", generation, reader, result)
            .is_err());
    }
    #[test]
    fn grosse_eintraege_vor_inflation_und_text_typgrenze_nach_verifikation() {
        let state = ArchiveReadState::default();
        let bytes = vec![b'a'; PREVIEW_MAX_BYTES as usize + 1];
        let (_temp, reader) = source(&fixture(&[(b"large", &bytes, 0o100644)]));
        let a = opened(&state, "main", "a", reader);
        assert_eq!(
            state
                .begin_read(
                    "main",
                    &a.session_id,
                    "r",
                    a.listing.rows[0].entry_id.as_ref().unwrap()
                )
                .err()
                .unwrap()
                .code,
            "unsupported"
        );
        let bytes = vec![b'a'; 1024 * 1024 + 1];
        let (_temp2, mut reader) = source(&fixture(&[(b"text", &bytes, 0o100644)]));
        let id = reader.entries()[0].id;
        assert!(matches!(
            read_preview(&mut reader, id, &Cancel::new())
                .unwrap()
                .content,
            PreviewContent::Unsupported { .. }
        ));
    }
    #[test]
    fn seiten_sind_begrenzt_und_katalogteilbilanz_bleibt_sichtbar() {
        let items: Vec<_> = (0..205)
            .map(|_| (b"duplicate".as_slice(), b"".as_slice(), 0o100644))
            .collect();
        let bytes = fixture(&items);
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.zip"), bytes).unwrap();
        let reader = ArchiveRead::open(
            temp.path(),
            Path::new("a.zip"),
            CatalogBudget {
                max_entries: 201,
                ..CatalogBudget::default()
            },
            &Cancel::new(),
        )
        .unwrap();
        let meta = metadata(&reader, "s", &Cancel::new()).unwrap();
        let first = listing("s", &meta, &meta.root_id, 0).unwrap();
        assert_eq!(first.rows.len(), 100);
        assert_eq!(first.total_rows, 201);
        assert!(!first.catalog.complete);
        assert!(first.catalog.stop_reason.is_some());
        assert_eq!(listing("s", &meta, &meta.root_id, 2).unwrap().rows.len(), 1);
        assert!(listing("s", &meta, &meta.root_id, 3).is_err());
    }
    #[test]
    fn oeffnungsrequest_bindet_camelcase_profil_und_prueft_pfade_vor_providerzugriff() {
        let request: ArchiveOpenRequest = serde_json::from_value(serde_json::json!({
            "endpoint": { "provider": "sftp", "profileId": "test", "profileRevision": 11, "root": "/daten" }, "path": "/daten/a.zip"
        })).unwrap();
        assert!(
            matches!(&request.endpoint, BidirectionalEndpointRequest::Sftp { profile_id, profile_revision: Some(11), .. } if profile_id == "test")
        );
        validate_open_request(&request).unwrap();
        for path in [
            "/fremd/a.zip",
            "/daten/../a.zip",
            "/daten/./a.zip",
            "/daten",
        ] {
            assert!(validate_open_request(&ArchiveOpenRequest {
                endpoint: request.endpoint.clone(),
                path: path.into()
            })
            .is_err());
        }
    }
    #[test]
    fn abbruch_im_inhaltsstream_vergiftet_nicht_die_sitzung() {
        struct StopSink(Cancel);
        impl std::io::Write for StopSink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.cancel();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (_temp, mut reader) = source(&fixture(&[
            (b"first", &vec![b'x'; 32768], 0o100644),
            (b"second", b"still readable", 0o100644),
        ]));
        let cancel = Cancel::new();
        assert!(reader
            .read_entry_to(
                reader.entries()[0].id,
                ReadBudget {
                    max_output_bytes: PREVIEW_MAX_BYTES,
                    max_io_bytes: 16 * 1024 * 1024,
                    max_read_calls: 100_000
                },
                &cancel,
                &mut StopSink(cancel.clone())
            )
            .is_err());
        let entry = reader.entries()[1].id;
        assert!(
            matches!(read_preview(&mut reader, entry, &Cancel::new()).unwrap().content, PreviewContent::Text { text } if text == "still readable")
        );
    }
    #[test]
    fn echte_deflate_fixture_und_verifizierte_raster_bytes() {
        let (_temp, mut reader) =
            source(include_bytes!("../../fuzz/seeds/archive_read/deflate.zip"));
        let entry = reader
            .entries()
            .iter()
            .find(|e| e.kind == ArchiveEntryKind::File)
            .unwrap()
            .id;
        assert!(matches!(
            read_preview(&mut reader, entry, &Cancel::new())
                .unwrap()
                .content,
            PreviewContent::Text { .. }
        ));
        let jpeg = [0xff, 0xd8, 0xff, 0xc0, 0, 8, 8, 0, 50, 0, 100, 0];
        let (_temp2, mut reader) = source(&fixture(&[(b"picture", &jpeg, 0o100644)]));
        let entry = reader.entries()[0].id;
        assert!(matches!(
            read_preview(&mut reader, entry, &Cancel::new())
                .unwrap()
                .content,
            PreviewContent::Image {
                width: 100,
                height: 50,
                ..
            }
        ));
    }
    #[test]
    fn ein_timer_je_fenster_und_fristverlaengerung_durch_navigation() {
        let state = ArchiveReadState::default();
        let (_temp, reader) = source(&fixture(&[]));
        let a = opened(&state, "main", "a", reader);
        assert!(state.start_timer("main"));
        assert!(!state.start_timer("main"));
        assert!(state
            .page("main", &a.session_id, &a.listing.directory_id, 0)
            .is_ok());
        assert!(state
            .timer_tick("main", Instant::now() + IDLE / 2)
            .is_some());
        assert!(state.timer_tick("main", Instant::now() + IDLE).is_none());
        assert!(state.slots.lock_recover()["main"].session.is_none());
    }
    #[test]
    fn tiefe_viele_pfade_begrenzen_ansichtsindex_ohne_quadratische_breadcrumbs() {
        let names: Vec<_> = (0..100)
            .map(|i| format!("root-{i}/{}file", "d/".repeat(126)).into_bytes())
            .collect();
        let items: Vec<_> = names
            .iter()
            .map(|name| (name.as_slice(), b"".as_slice(), 0o100644))
            .collect();
        let (_temp, reader) = source(&fixture(&items));
        assert!(reader.is_complete());
        let meta = metadata(&reader, "s", &Cancel::new()).unwrap();
        assert!(meta.directories.len() <= VIEW_MAX_DIRECTORIES);
        assert!(meta.catalog.displayed_entry_count < 100);
        assert_eq!(meta.catalog.entry_count, 100);
        assert!(!meta.catalog.complete);
        assert_eq!(
            meta.catalog.stop_reason.as_ref().unwrap().code,
            "resource_limit"
        );
        let (_temp2, reader) = source(&fixture(&items[..1]));
        let meta = metadata(&reader, "s", &Cancel::new()).unwrap();
        assert!(meta.catalog.complete);
        let deepest = meta
            .directories
            .iter()
            .find(|(_, d)| d.rows.iter().any(|r| r.entry_id.is_some()))
            .unwrap()
            .0;
        assert_eq!(
            listing("s", &meta, deepest, 0).unwrap().breadcrumbs.len(),
            128
        );
        let tiny =
            metadata_bounded(&reader, "s", &Cancel::new(), VIEW_MAX_DIRECTORIES, 1024).unwrap();
        assert_eq!(tiny.catalog.displayed_entry_count, 0);
        assert_eq!(tiny.directories.len(), 1);
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(metadata(&reader, "s", &cancel).is_err());
    }
    #[test]
    fn bewusste_einzelvorschauen_haben_keine_kumulative_sitzungsquote() {
        let state = ArchiveReadState::default();
        let (_temp, reader) = source(&fixture(&[(b"file", b"hello", 0o100644)]));
        let a = opened(&state, "main", "open", reader);
        for i in 0..205 {
            let request = format!("read-{i}");
            let (mut reader, core, generation, cancel) = state
                .begin_read(
                    "main",
                    &a.session_id,
                    &request,
                    a.listing.rows[0].entry_id.as_ref().unwrap(),
                )
                .unwrap();
            let result = read_preview(&mut reader, core, &cancel);
            assert!(state
                .finish_read("main", &a.session_id, &request, generation, reader, result)
                .is_ok());
        }
    }
    #[test]
    fn sandbox_sperrt_pane_root_und_dateisymlinks_in_kunstlichen_aussenordner() {
        let sandbox = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sandbox = std::fs::canonicalize(sandbox.path()).unwrap();
        let outside = std::fs::canonicalize(outside.path()).unwrap();
        std::fs::write(outside.join("a.zip"), fixture(&[])).unwrap();
        std::os::unix::fs::symlink(&outside, sandbox.join("link")).unwrap();
        let root = sandbox.join("link");
        let path = root.join("a.zip");
        assert_eq!(
            open_local_source(
                root.to_str().unwrap(),
                path.to_str().unwrap(),
                &Some(sandbox.clone()),
                &Cancel::new()
            )
            .err()
            .unwrap()
            .code,
            "sandbox_violation"
        );
        std::os::unix::fs::symlink(outside.join("a.zip"), sandbox.join("file.zip")).unwrap();
        assert_eq!(
            open_local_source(
                sandbox.to_str().unwrap(),
                sandbox.join("file.zip").to_str().unwrap(),
                &Some(sandbox.clone()),
                &Cancel::new()
            )
            .err()
            .unwrap()
            .code,
            "sandbox_violation"
        );
        std::fs::write(sandbox.join("inside.zip"), fixture(&[])).unwrap();
        assert!(open_local_source(
            sandbox.to_str().unwrap(),
            sandbox.join("inside.zip").to_str().unwrap(),
            &Some(sandbox.clone()),
            &Cancel::new()
        )
        .is_ok());
    }
}
