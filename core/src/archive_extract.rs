//! Lokales ZIP-Entpacken über die vorhandenen Temp-, Commit- und Recoveryverträge.
//! Provisorische Sinkbytes werden erst nach CRC/EOF/Quellprüfung sichtbar gemacht.
use crate::archive_read::{ArchiveEntryKind, ArchiveRead, CatalogBudget, ReadBudget};
use crate::atomic_file::finish_staging_scope;
use crate::error::{coded_error, error_code, with_recovery_context, ErrorCode};
use crate::fileops::{self, FileConflictProof, TargetCommitEffect, TempCommitFailure};
use crate::remote_rename::folded_name;
use crate::vfs::{FileCommitClass, FileReplaceClass, LocalRootIdentity};
use crate::{Cancel, LocalVfs, Vfs};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
const INDEX_BUDGET: usize = 64 * 1024 * 1024;
const MAX_NODES: usize = 50_000;
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArchiveExtractOptions {
    pub overwrite: bool,
    pub allow_partial: bool,
}
/// Persistente Auswahl ohne Browser-Sitzungskennung. Eine Dateiauswahl bindet den
/// ursprünglichen zentralen Ordinal und Rohnamen; Unterbäume behalten ihren Präfix.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArchiveExtractSelection {
    #[default]
    All,
    Entry {
        ordinal: usize,
        raw_name: Vec<u8>,
    },
    Subtree {
        prefix: String,
    },
}
impl ArchiveExtractSelection {
    fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }
    fn validate(&self) -> io::Result<()> {
        match self {
            Self::All => Ok(()),
            Self::Entry { raw_name, .. }
                if !raw_name.is_empty() && raw_name.len() <= u16::MAX as usize =>
            {
                Ok(())
            }
            Self::Subtree { prefix }
                if crate::archive_read::path_is_safe(prefix)
                    && !prefix.ends_with('/')
                    && prefix.len() <= 4096
                    && prefix.split('/').count() <= 128 =>
            {
                Ok(())
            }
            _ => Err(invalid("Ungültige ZIP-Entpackauswahl")),
        }
    }
}
/// Herkunft ist eine eingefrorene Anzeige, kein Recht auf erneuten Serverzugriff.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveSnapshotSource {
    pub reference: crate::remote_snapshot::SnapshotReference,
    pub origin: String,
    pub profile: crate::RemoteProfileBinding,
}
impl ArchiveSnapshotSource {
    pub fn validate(&self) -> io::Result<()> {
        self.reference.validate()?;
        self.profile.validate()?;
        if self.origin.is_empty()
            || self.origin.len() > 8192
            || self.origin.chars().any(char::is_control)
        {
            return Err(invalid("Ungültige Herkunft der Archivmomentaufnahme"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveExtractRequest {
    // Lokale Altjournale behalten ihr Format. Eine Snapshotquelle enthält dagegen
    // ausschließlich die typisierte Referenz; beide lokalen Felder müssen leer
    // sein. validate() verbietet jede Mischform, prepare() erteilt daraus kein Recht.
    #[serde(default, skip_serializing_if = "path_is_empty")]
    pub source_root: PathBuf,
    #[serde(default, skip_serializing_if = "path_is_empty")]
    pub source_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<ArchiveSnapshotSource>,
    pub target_dir: PathBuf,
    pub options: ArchiveExtractOptions,
    #[serde(default, skip_serializing_if = "ArchiveExtractSelection::is_all")]
    pub selection: ArchiveExtractSelection,
    pub sandbox: Option<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveExtractIssue {
    pub path: String,
    pub reason: String,
    pub content_omitted: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveExtractConflict {
    pub path: String,
    pub relative_name: String,
    pub source_bytes: u64,
    pub target_bytes: u64,
    pub target_mtime: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveExtractPreview {
    pub source_root: PathBuf,
    pub source_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<ArchiveSnapshotSource>,
    pub target_dir: PathBuf,
    #[serde(default, skip_serializing_if = "ArchiveExtractSelection::is_all")]
    pub selection: ArchiveExtractSelection,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub relative_root: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub selection_path: String,
    pub catalog_complete: bool,
    pub known_entries: u64,
    pub files: u64,
    pub directories: u64,
    pub source_bytes: u64,
    pub omitted_entries: u64,
    pub issues: Vec<ArchiveExtractIssue>,
    pub conflicts: Vec<String>,
    pub type_conflicts: Vec<String>,
    pub conflict_details: Vec<ArchiveExtractConflict>,
    pub commit_class: FileCommitClass,
    pub replace_best_effort: bool,
    pub file_flush: bool,
    pub directory_flush: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveExtractBinding {
    pub digest: [u8; 32],
    pub preview: ArchiveExtractPreview,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveExtractStatus {
    Complete,
    Cancelled,
    Failed,
    OutcomeUnclear,
}
#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveExtractPhase {
    Extracting,
    Verifying,
    Committing,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct ArchiveExtractProgress {
    pub phase: ArchiveExtractPhase,
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub current: String,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveExtractReport {
    pub preview: ArchiveExtractPreview,
    pub status: ArchiveExtractStatus,
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub skipped: u64,
    pub failed: u64,
    pub unstarted: u64,
    pub has_effect: bool,
    pub recovery_paths: Vec<String>,
    pub target_copy_new_best_effort: Option<bool>,
    pub target_replace_best_effort: Option<bool>,
}
#[derive(Debug)]
pub struct ArchiveExtractFailure {
    pub error: io::Error,
    pub report: Box<ArchiveExtractReport>,
}
impl std::fmt::Display for ArchiveExtractFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for ArchiveExtractFailure {}
pub struct ArchiveExtractOwner<'a> {
    pub queue: &'a Path,
    pub operation_id: &'a str,
}
#[derive(Clone)]
struct ExtractFile {
    index: usize,
    relative: String,
    target: PathBuf,
    size: u64,
    compressed: u64,
    replacement: Option<FileConflictProof>,
    parent_identity: LocalRootIdentity,
    commit_class: FileCommitClass,
    flush: (bool, bool),
}
#[derive(Default)]
struct Node {
    directory: bool,
    leaves: usize,
    explicit_directories: usize,
}
pub struct PreparedArchiveExtract {
    request: ArchiveExtractRequest,
    reader: ArchiveRead,
    target: LocalVfs,
    identity: LocalRootIdentity,
    source_digest: [u8; 32],
    evidence: [u8; 32],
    preview: ArchiveExtractPreview,
    files: Vec<ExtractFile>,
    directories: Vec<PathBuf>,
    target_observations: Vec<(PathBuf, Option<FileConflictProof>)>,
    directory_identities: BTreeMap<PathBuf, (Option<u64>, Option<u64>)>,
    root_capabilities: (FileCommitClass, bool, bool),
    cancel: Cancel,
    snapshot: Option<crate::remote_snapshot::ValidatedSnapshot>,
}
fn path_is_empty(path: &Path) -> bool {
    path.as_os_str().is_empty()
}
fn invalid(message: &str) -> io::Error {
    coded_error(
        io::ErrorKind::InvalidInput,
        ErrorCode::InvalidInput,
        message,
    )
}
fn drift(message: &str) -> io::Error {
    coded_error(io::ErrorKind::Interrupted, ErrorCode::PlanDrift, message)
}
fn cancelled(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::Interrupted,
            "ZIP-Entpacken abgebrochen",
        ))
    } else {
        Ok(())
    }
}
fn budget() -> io::Error {
    invalid("ZIP-Entpackindex überschreitet sein Metadaten-/Knotenbudget; noch nichts entpackt")
}
struct TargetNameGroup {
    count: usize,
    exact_name: String,
}
fn charge_index(charged: &mut usize, bytes: usize) -> io::Result<()> {
    *charged = charged.checked_add(bytes).ok_or_else(budget)?;
    if *charged > INDEX_BUDGET {
        return Err(budget());
    }
    Ok(())
}
fn index_target_names(
    listing: crate::vfs::CompleteDirectoryListing,
    charged: &mut usize,
    cancel: &Cancel,
) -> io::Result<BTreeMap<String, TargetNameGroup>> {
    cancelled(cancel)?;
    if !listing.complete {
        return Err(invalid(
            "Unvollständiges oder nicht sicher dekodierbares Ziel-Elternlisting",
        ));
    }
    let mut groups = BTreeMap::<String, TargetNameGroup>::new();
    for item in listing.entries {
        cancelled(cancel)?;
        if !item.raw_name.is_empty() && std::str::from_utf8(&item.raw_name).is_err() {
            return Err(invalid(
                "Unvollständiges oder nicht sicher dekodierbares Ziel-Elternlisting",
            ));
        }
        let exact_name = item.entry.name;
        let folded = folded_name(&exact_name);
        if let Some(group) = groups.get_mut(&folded) {
            group.count += 1;
        } else {
            let bytes = 256usize
                .checked_add(folded.len())
                .and_then(|n| n.checked_add(exact_name.len()))
                .ok_or_else(budget)?;
            charge_index(charged, bytes)?;
            groups.insert(
                folded,
                TargetNameGroup {
                    count: 1,
                    exact_name,
                },
            );
        }
        #[cfg(test)]
        tests::target_name_indexed();
    }
    cancelled(cancel)?;
    Ok(groups)
}
fn optional_stat(vfs: &dyn Vfs, path: &Path) -> io::Result<Option<crate::vfs::Entry>> {
    match vfs.stat(path) {
        Ok(e) => Ok(Some(e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
fn display_name(entry: &crate::archive_read::ArchiveCatalogEntry) -> String {
    entry.name.clone().unwrap_or_else(|| {
        format!(
            "[Rohname hex: {}]",
            entry
                .raw_name
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<String>()
        )
    })
}
fn issue(preview: &mut ArchiveExtractPreview, path: String, reason: &str, omitted: bool) {
    if omitted {
        preview.omitted_entries += 1;
    }
    preview.issues.push(ArchiveExtractIssue {
        path,
        reason: reason.into(),
        content_omitted: omitted,
    });
}
fn prefixes(name: &str) -> Vec<String> {
    let parts: Vec<_> = name.split('/').collect();
    (1..=parts.len()).map(|n| parts[..n].join("/")).collect()
}
impl ArchiveExtractRequest {
    pub fn validate(&self) -> io::Result<()> {
        self.selection.validate()?;
        if fileops::validate_file_operation_root(&self.target_dir)? != self.target_dir {
            return Err(invalid(
                "ZIP-Entpackabsicht benötigt einen unveränderten absoluten Zielpfad",
            ));
        }
        if let Some(snapshot) = &self.snapshot {
            snapshot.validate()?;
            if !self.source_root.as_os_str().is_empty() || !self.source_path.as_os_str().is_empty()
            {
                return Err(invalid(
                    "Eine Archivmomentaufnahme erlaubt keine externen Quellpfade",
                ));
            }
        } else if fileops::validate_file_operation_root(&self.source_root)? != self.source_root
            || fileops::validate_file_operation_root(&self.source_path)? != self.source_path
            || self.source_path == self.source_root
            || !self.source_path.starts_with(&self.source_root)
        {
            return Err(invalid(
                "ZIP-Entpackabsicht benötigt unveränderte absolute Pane-/Quellpfade",
            ));
        }
        Ok(())
    }
    pub fn prepare(&self, cancel: &Cancel) -> io::Result<PreparedArchiveExtract> {
        if self.snapshot.is_some() {
            return Err(invalid(
                "Archivmomentaufnahme benötigt den bekannten Snapshotstore; Archiv erneut öffnen",
            ));
        }
        prepare(self, None, cancel)
    }
    pub fn prepare_with_snapshot(
        &self,
        snapshot: crate::remote_snapshot::ValidatedSnapshot,
        cancel: &Cancel,
    ) -> io::Result<PreparedArchiveExtract> {
        self.validate()?;
        if self.snapshot.as_ref().map(|source| &source.reference) != Some(snapshot.reference()) {
            return Err(drift("Archivmomentaufnahme und bestätigte Quelle passen nicht zusammen; Archiv erneut öffnen"));
        }
        prepare(self, Some(snapshot), cancel)
    }
}
fn selection_contains(
    selection: &ArchiveExtractSelection,
    index: usize,
    entry: &crate::archive_read::ArchiveCatalogEntry,
) -> bool {
    match selection {
        ArchiveExtractSelection::All => true,
        ArchiveExtractSelection::Entry { ordinal, .. } => index == *ordinal,
        ArchiveExtractSelection::Subtree { prefix } => entry.name.as_deref().is_some_and(|name| {
            let name = name.trim_end_matches('/');
            name == prefix
                || name
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }),
    }
}
pub(crate) fn validate_selection_catalog(
    selection: &ArchiveExtractSelection,
    reader: &ArchiveRead,
) -> io::Result<()> {
    selection.validate()?;
    selection_mapping(selection, reader).map(|_| ())
}
fn selection_mapping(
    selection: &ArchiveExtractSelection,
    reader: &ArchiveRead,
) -> io::Result<(String, String, String)> {
    let root = match selection {
        ArchiveExtractSelection::All => return Ok((String::new(), String::new(), String::new())),
        ArchiveExtractSelection::Entry { ordinal, raw_name } => {
            let entry = reader.entries().get(*ordinal).filter(|entry| entry.raw_name == *raw_name && entry.kind == ArchiveEntryKind::File)
                .ok_or_else(|| drift("ZIP-Auswahlordinal oder exakter Eintragsname änderte sich; Datei erneut im Archiv auswählen"))?;
            entry
                .name
                .as_deref()
                .ok_or_else(|| invalid("ZIP-Dateiauswahl benötigt einen UTF-8-Namen"))?
        }
        ArchiveExtractSelection::Subtree { prefix } => {
            if !reader.entries().iter().any(|entry| {
                entry.name.as_deref().is_some_and(|name| {
                    let name = name.trim_end_matches('/');
                    (name == prefix && entry.kind == ArchiveEntryKind::Directory)
                        || name
                            .strip_prefix(prefix)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                })
            }) {
                return Err(drift(
                    "Virtueller ZIP-Unterordner fehlt; Ordner erneut im Archiv auswählen",
                ));
            }
            prefix
        }
    };
    if !crate::archive_read::path_is_safe(root) {
        return Err(invalid("ZIP-Auswahl hat keinen sicheren Namen"));
    }
    let (base, leaf) = root.rsplit_once('/').unwrap_or(("", root));
    Ok((
        if base.is_empty() {
            String::new()
        } else {
            format!("{base}/")
        },
        leaf.to_owned(),
        root.to_owned(),
    ))
}
fn prepare(
    request: &ArchiveExtractRequest,
    snapshot: Option<crate::remote_snapshot::ValidatedSnapshot>,
    cancel: &Cancel,
) -> io::Result<PreparedArchiveExtract> {
    request.validate()?;
    cancelled(cancel)?;
    fileops::guard(&request.sandbox, &request.target_dir)?;
    let reader = if let Some(snapshot) = &snapshot {
        ArchiveRead::open_snapshot(snapshot.clone(), CatalogBudget::default(), cancel)?
    } else {
        for path in [&request.source_root, &request.source_path] {
            fileops::guard(&request.sandbox, path)?;
        }
        ArchiveRead::open(
            &request.source_root,
            request
                .source_path
                .strip_prefix(&request.source_root)
                .map_err(|_| invalid("Ungültige ZIP-Quelle"))?,
            CatalogBudget::default(),
            cancel,
        )?
    };
    let source_digest = reader.source_binding_digest(cancel)?;
    let (selection_base, relative_root, selection_path) =
        selection_mapping(&request.selection, &reader)?;
    let target = LocalVfs::rooted(&request.target_dir)?;
    let identity = target.local_root_identity(&request.target_dir)?;
    let (file_flush, directory_flush) = target.file_flush_guarantees_at(&request.target_dir)?;
    let root_commit_class = target.file_commit_class_at(&request.target_dir)?;
    let target = target.with_sync_execution(Some(crate::real_mode::LocalSyncExecution {
        new_file_commit: root_commit_class,
        file_flush,
        directory_flush,
        irreversible_replacements: 0,
    }));
    let mut preview = ArchiveExtractPreview {
        source_root: request.source_root.clone(),
        source_path: request.source_path.clone(),
        snapshot: request.snapshot.clone(),
        target_dir: request.target_dir.clone(),
        selection: request.selection.clone(),
        relative_root,
        selection_path,
        catalog_complete: reader.is_complete(),
        known_entries: 0,
        files: 0,
        directories: 0,
        source_bytes: 0,
        omitted_entries: 0,
        issues: Vec::new(),
        conflicts: Vec::new(),
        type_conflicts: Vec::new(),
        conflict_details: Vec::new(),
        commit_class: target.file_commit_class_at(&request.target_dir)?,
        replace_best_effort: false,
        file_flush,
        directory_flush,
    };
    if let Some(stop) = reader.stop_reason() {
        let scope = if request.selection.is_all() {
            format!(
                "Nur die {} bekannten zentralen Einträge sind Gegenstand dieses Laufs",
                reader.entries().len()
            )
        } else {
            format!("Der Katalog kennt {} zentrale Einträge; nur der bekannte ausgewählte Teil ist Gegenstand dieses Laufs", reader.entries().len())
        };
        issue(
            &mut preview,
            request.source_path.display().to_string(),
            &format!(
                "Teilkatalog: {}. {scope}; weitere Einträge sind unbekannt.",
                stop.message
            ),
            false,
        );
    }
    // Gruppen immer über den ganzen bekannten Katalog bilden. Der spätere
    // Auswahlfilter darf aus einem mehrdeutigen Namen keinen Gewinner machen.
    let mut nodes = BTreeMap::<String, Node>::new();
    let mut charged = 0usize;
    for entry in reader.entries() {
        cancelled(cancel)?;
        if entry.issues.iter().any(|issue| {
            matches!(
                issue,
                crate::archive_read::ArchiveEntryIssue::NameTooLong
                    | crate::archive_read::ArchiveEntryIssue::PathTooDeep
            )
        }) {
            continue;
        }
        let Some(name) = entry
            .name
            .as_deref()
            .filter(|n| crate::archive_read::path_is_safe(n))
        else {
            continue;
        };
        let name = name.trim_end_matches('/');
        let paths = prefixes(name);
        for (i, path) in paths.iter().enumerate() {
            if !nodes.contains_key(path) {
                charged = charged
                    .checked_add(512 + path.len() * 2)
                    .ok_or_else(budget)?;
                if charged > INDEX_BUDGET || nodes.len() >= MAX_NODES {
                    return Err(budget());
                }
            }
            let node = nodes.entry(path.clone()).or_default();
            if i + 1 < paths.len() || entry.kind == ArchiveEntryKind::Directory {
                node.directory = true;
            }
            if i + 1 == paths.len() {
                if entry.kind == ArchiveEntryKind::Directory {
                    node.explicit_directories += 1;
                } else {
                    node.leaves += 1;
                }
            }
        }
    }
    let mut aliases = BTreeMap::<(String, String), Vec<String>>::new();
    for path in nodes.keys() {
        let (parent, leaf) = path.rsplit_once('/').unwrap_or(("", path));
        aliases
            .entry((parent.into(), folded_name(leaf)))
            .or_default()
            .push(path.clone());
    }
    let mut blocked = BTreeSet::new();
    for group in aliases.values().filter(|g| g.len() > 1) {
        blocked.extend(group.iter().cloned());
    }
    for (path, node) in &nodes {
        if node.leaves > 1 || node.explicit_directories > 1 || (node.leaves > 0 && node.directory) {
            blocked.insert(path.clone());
        }
    }
    let source_identity = reader.source_identity()?;
    let source_path = reader.source_paths().1.to_owned();
    let mut files = Vec::new();
    let mut needed_dirs = BTreeSet::new();
    let mut observations = BTreeMap::<PathBuf, Option<FileConflictProof>>::new();
    // Nur Namensgruppen dieser Vorbereitung teilen. Stats und Identitätsbelege
    // bleiben frisch; fehlende Eltern sind kein bestätigtes leeres Listing.
    let mut parent_names = BTreeMap::<PathBuf, BTreeMap<String, TargetNameGroup>>::new();
    for (index, entry) in reader.entries().iter().enumerate() {
        cancelled(cancel)?;
        if !selection_contains(&request.selection, index, entry) {
            continue;
        }
        preview.known_entries += 1;
        let name = display_name(entry);
        charged = charged
            .checked_add(1024 + name.len() * 4)
            .ok_or_else(budget)?;
        if charged > INDEX_BUDGET {
            return Err(budget());
        }
        if !entry.issues.is_empty() {
            issue(
                &mut preview,
                name,
                &format!("Archiveintrag nicht ausführbar: {:?}", entry.issues),
                true,
            );
            continue;
        }
        let original_relative = entry
            .name
            .as_ref()
            .ok_or_else(|| invalid("ZIP-Eintrag ohne UTF-8-Namen"))?
            .trim_end_matches('/');
        if prefixes(original_relative)
            .iter()
            .any(|p| blocked.contains(p))
        {
            issue(&mut preview,name,"Mehrdeutige Duplikat-/Case-/Unicode- oder Datei-/Ordnergruppe vollständig ausgelassen",true);
            continue;
        }
        let relative = original_relative
            .strip_prefix(&selection_base)
            .ok_or_else(|| invalid("ZIP-Auswahl verlässt ihre relative Wurzel"))?;
        if !crate::archive_read::path_is_safe(relative) {
            return Err(invalid("Ungültige relative ZIP-Zielabbildung"));
        }
        let paths = prefixes(relative);
        let dst_path = request.target_dir.join(relative);
        if source_path.starts_with(&dst_path) || dst_path == source_path {
            issue(
                &mut preview,
                name,
                "Ziel würde die Archivquelle oder einen ihrer Eltern ersetzen",
                true,
            );
            continue;
        }
        let mut target_blocked = false;
        let mut replacement = None;
        for (i, prefix) in paths.iter().enumerate() {
            let path = request.target_dir.join(prefix);
            let parent = path
                .parent()
                .ok_or_else(|| invalid("Ziel ohne Elternordner"))?;
            let exists_parent = optional_stat(&target, parent)?.is_some();
            if !exists_parent {
                continue;
            }
            if !parent_names.contains_key(parent) {
                let bytes = 256usize
                    .checked_add(parent.as_os_str().len())
                    .ok_or_else(budget)?;
                charge_index(&mut charged, bytes)?;
                #[cfg(test)]
                tests::record_parent_listing(parent);
                let listing = target.list_complete(parent)?;
                let groups = index_target_names(listing, &mut charged, cancel)?;
                parent_names.insert(parent.to_owned(), groups);
            }
            let leaf = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| invalid("Zielname nicht UTF-8"))?;
            let matching = parent_names[parent].get(&folded_name(leaf));
            if matching.is_some_and(|group| group.count > 1 || group.exact_name != leaf) {
                issue(
                    &mut preview,
                    name.clone(),
                    "Mehrdeutiger Case-/Unicode-Zielname ausgelassen",
                    true,
                );
                target_blocked = true;
                break;
            }
            let actual = optional_stat(&target, &path)?;
            let needs_dir = i + 1 < paths.len() || entry.kind == ArchiveEntryKind::Directory;
            if let Some(actual) = &actual {
                if Some(source_identity.0) == actual.device
                    && Some(source_identity.1) == actual.inode
                {
                    issue(
                        &mut preview,
                        name.clone(),
                        "Ziel ist ein Identitätsalias der Archivquelle",
                        true,
                    );
                    target_blocked = true;
                    break;
                }
                if actual.is_symlink
                    || (!actual.is_file && !actual.is_dir)
                    || needs_dir != actual.is_dir
                {
                    preview.type_conflicts.push(fileops::path_to_utf8(&path)?);
                    issue(&mut preview,name.clone(),"Ziel-Typkonflikt, Symlink oder Spezialziel ausgelassen; bestehender Bestand bleibt erhalten",true);
                    target_blocked = true;
                    break;
                }
            }
            if !observations.contains_key(&path) {
                let proof = if actual.is_some() {
                    Some(
                        fileops::capture_file_metadata_proofs(
                            &target,
                            &[fileops::path_to_utf8(&path)?],
                        )?
                        .remove(0),
                    )
                } else {
                    None
                };
                observations.insert(path.clone(), proof);
            }
            if !needs_dir && actual.is_some() {
                let proof = fileops::capture_file_conflict_proofs(
                    &target,
                    &[fileops::path_to_utf8(&path)?],
                )?
                .remove(0);
                preview.conflicts.push(fileops::path_to_utf8(&path)?);
                preview.conflict_details.push(ArchiveExtractConflict {
                    path: fileops::path_to_utf8(&path)?,
                    relative_name: relative.into(),
                    source_bytes: entry.uncompressed_bytes,
                    target_bytes: proof.size,
                    target_mtime: proof.mtime,
                });
                replacement = Some(proof);
            }
        }
        if target_blocked {
            continue;
        }
        let dir_count = if entry.kind == ArchiveEntryKind::Directory {
            paths.len()
        } else {
            paths.len().saturating_sub(1)
        };
        for path in &paths[..dir_count] {
            needed_dirs.insert(request.target_dir.join(path));
        }
        if entry.kind == ArchiveEntryKind::File {
            preview.source_bytes = preview
                .source_bytes
                .checked_add(entry.uncompressed_bytes)
                .ok_or_else(|| invalid("Summe der ZIP-Inhaltsgrößen läuft über"))?;
            let mut existing_parent = dst_path
                .parent()
                .ok_or_else(|| invalid("Ziel ohne Eltern"))?;
            while optional_stat(&target, existing_parent)?.is_none() {
                existing_parent = existing_parent
                    .parent()
                    .ok_or_else(|| invalid("Ziel ohne existierenden Elternordner"))?;
            }
            let parent_identity = target.local_root_identity(existing_parent)?;
            let commit_class = target.file_commit_class_at(existing_parent)?;
            let flush = target.file_flush_guarantees_at(existing_parent)?;
            if commit_class == FileCommitClass::BestEffort {
                preview.commit_class = FileCommitClass::BestEffort;
            }
            preview.file_flush &= flush.0;
            preview.directory_flush &= flush.1;
            files.push(ExtractFile {
                index,
                relative: relative.into(),
                target: dst_path,
                size: entry.uncompressed_bytes,
                compressed: entry.compressed_bytes,
                replacement,
                parent_identity,
                commit_class,
                flush,
            });
        }
        issue(&mut preview,name,"Inhalte und Ordnerstruktur; archivierte Zeiten, Rechte, Owner, ACL, xattrs, Forks und Sparse-/Hardlinkbeziehungen werden nicht wiederhergestellt",false);
    }
    preview.files = files.len() as u64;
    preview.directories = needed_dirs.len() as u64;
    preview.replace_best_effort = !preview.conflicts.is_empty()
        && target.file_replace_class() == FileReplaceClass::BestEffort;
    let directories = needed_dirs.into_iter().collect();
    let directory_identities = observations
        .iter()
        .filter_map(|(p, proof)| {
            proof
                .as_ref()
                .filter(|p| p.is_dir)
                .map(|proof| (p.clone(), (proof.device, proof.inode)))
        })
        .collect();
    let target = target.with_sync_execution(Some(crate::real_mode::LocalSyncExecution {
        new_file_commit: preview.commit_class,
        file_flush: preview.file_flush,
        directory_flush: preview.directory_flush,
        irreversible_replacements: 0,
    }));
    let mut prepared = PreparedArchiveExtract {
        request: request.clone(),
        reader,
        target,
        identity,
        source_digest,
        evidence: [0; 32],
        preview,
        files,
        directories,
        target_observations: observations.into_iter().collect(),
        directory_identities,
        root_capabilities: (root_commit_class, file_flush, directory_flush),
        cancel: cancel.clone(),
        snapshot,
    };
    prepared.evidence = prepared.evidence_digest()?;
    Ok(prepared)
}
impl PreparedArchiveExtract {
    /// Der Queueabschluss behält die Quelle auch nach dem konsumierenden Executor.
    pub(crate) fn hold_snapshot(&self) -> Option<crate::remote_snapshot::ValidatedSnapshot> {
        self.snapshot.clone()
    }
    /// Bindet die erste Vorbereitung an die noch im Browser bestätigte Quelle.
    /// Queueclaims vergleichen anschließend die vollständige persistierte Binding.
    pub fn verify_selection_source(&self, expected: [u8; 32]) -> io::Result<()> {
        self.reader.verify_source(&self.cancel)?;
        if self.source_digest != expected {
            return Err(drift(
                "Archivquelle änderte sich seit der Browserauswahl; Archiv erneut öffnen",
            ));
        }
        Ok(())
    }
    pub fn preview(&self) -> &ArchiveExtractPreview {
        &self.preview
    }
    pub fn with_options(self, options: ArchiveExtractOptions, cancel: &Cancel) -> io::Result<Self> {
        let mut request = self.request.clone();
        request.options = options;
        let next = if let Some(snapshot) = &self.snapshot {
            request.prepare_with_snapshot(snapshot.reopen(cancel)?, cancel)?
        } else {
            request.prepare(cancel)?
        };
        if next.evidence != self.evidence {
            return Err(drift(
                "Archivquelle oder Zielbestand haben sich seit der Entscheidung geändert",
            ));
        }
        Ok(next)
    }
    fn evidence_digest(&self) -> io::Result<[u8; 32]> {
        self.reader.verify_source(&self.cancel)?;
        self.verify_root(&self.target)?;
        let mut digest = blake3::Hasher::new();
        digest.update(b"doppeldecker-archive-extract-evidence-v1");
        digest.update(
            &serde_json::to_vec(&(
                &self.source_digest,
                &self.identity,
                &self.preview,
                &self.target_observations,
                self.files
                    .iter()
                    .map(|f| {
                        (
                            &f.relative,
                            &f.replacement,
                            &f.parent_identity,
                            f.commit_class,
                            f.flush,
                        )
                    })
                    .collect::<Vec<_>>(),
            ))
            .map_err(io::Error::other)?,
        );
        Ok(*digest.finalize().as_bytes())
    }
    pub fn binding(&self) -> io::Result<ArchiveExtractBinding> {
        let evidence = self.evidence_digest()?;
        let mut hash = blake3::Hasher::new();
        hash.update(&evidence);
        hash.update(&serde_json::to_vec(&self.request.options).map_err(io::Error::other)?);
        Ok(ArchiveExtractBinding {
            digest: *hash.finalize().as_bytes(),
            preview: self.preview.clone(),
        })
    }
    fn verify_root(&self, dst: &dyn Vfs) -> io::Result<()> {
        cancelled(&self.cancel)?;
        if !dst.anchor_is_current()?
            || dst.local_root_identity(&self.request.target_dir)? != self.identity
            || dst.file_commit_class_at(&self.request.target_dir)? != self.root_capabilities.0
            || dst.file_flush_guarantees_at(&self.request.target_dir)?
                != (self.root_capabilities.1, self.root_capabilities.2)
        {
            return Err(drift("ZIP-Zielroot oder Fähigkeiten änderten sich"));
        }
        Ok(())
    }
    fn verify_parents(&self, dst: &dyn Vfs, path: &Path) -> io::Result<()> {
        for parent in path
            .ancestors()
            .skip(1)
            .take_while(|p| p.starts_with(&self.request.target_dir))
        {
            if let Some(expected) = self.directory_identities.get(parent) {
                let actual = dst.stat(parent)?;
                if actual.is_symlink || !actual.is_dir || &(actual.device, actual.inode) != expected
                {
                    return Err(drift("ZIP-Ziel-Elternordner wurde ausgetauscht"));
                }
            }
        }
        Ok(())
    }
    pub fn execute(
        self,
        owner: Option<ArchiveExtractOwner<'_>>,
        on_progress: &mut dyn FnMut(&ArchiveExtractProgress),
    ) -> Result<ArchiveExtractReport, ArchiveExtractFailure> {
        self.execute_recorded(owner, on_progress, None)
    }
    pub fn execute_generated(
        self,
        owner: Option<ArchiveExtractOwner<'_>>,
        on_progress: &mut dyn FnMut(&ArchiveExtractProgress),
    ) -> Result<
        crate::generated_source::GeneratedArchive<ArchiveExtractReport>,
        ArchiveExtractFailure,
    > {
        let mut receipt = crate::generated_source::GeneratedReceipt::new();
        let report = self.execute_recorded(owner, on_progress, Some(&mut receipt))?;
        Ok(crate::generated_source::GeneratedArchive { report, receipt })
    }
    fn execute_recorded(
        mut self,
        owner: Option<ArchiveExtractOwner<'_>>,
        on_progress: &mut dyn FnMut(&ArchiveExtractProgress),
        mut receipt: Option<&mut crate::generated_source::GeneratedReceipt>,
    ) -> Result<ArchiveExtractReport, ArchiveExtractFailure> {
        let dst = if let Some(owner) = owner {
            self.target
                .clone()
                .with_required_temp_recovery(owner.queue, owner.operation_id)
        } else {
            self.target.clone()
        };
        let mut report = ArchiveExtractReport {
            preview: self.preview.clone(),
            status: ArchiveExtractStatus::Failed,
            files: 0,
            directories: 0,
            bytes: 0,
            skipped: self.preview.omitted_entries,
            failed: 0,
            unstarted: self.preview.files,
            has_effect: false,
            recovery_paths: Vec::new(),
            target_copy_new_best_effort: Some(false),
            target_replace_best_effort: Some(false),
        };
        let mut attempted_file = false;
        let result = (|| -> io::Result<()> {
            cancelled(&self.cancel)?;
            if !self.preview.catalog_complete && !self.request.options.allow_partial {
                return Err(invalid(
                    "Teilkatalog benötigt ausdrückliche Entscheidung für den bekannten Umfang",
                ));
            }
            self.reader.verify_source(&self.cancel)?;
            self.verify_root(&dst)?;
            for (path, expected) in &self.target_observations {
                let actual = optional_stat(&dst, path)?;
                match (expected, actual) {
                    (Some(proof), Some(actual))
                        if proof.is_dir
                            && actual.is_dir
                            && !actual.is_symlink
                            && proof.device == actual.device
                            && proof.inode == actual.inode => {}
                    (Some(proof), Some(actual))
                        if !proof.is_dir
                            && proof.device == actual.device
                            && proof.inode == actual.inode
                            && proof.size == actual.size
                            && proof.mtime == actual.mtime => {}
                    (None, None) => {}
                    _ => return Err(drift("ZIP-Zielbestand änderte sich seit der Vorbereitung")),
                }
            }
            for path in &self.directories {
                self.reader.verify_source(&self.cancel)?;
                self.verify_root(&dst)?;
                self.verify_parents(&dst, path)?;
                match optional_stat(&dst, path)? {
                    Some(e)
                        if e.is_dir
                            && !e.is_symlink
                            && self.directory_identities.get(path)
                                == Some(&(e.device, e.inode)) => {}
                    Some(_) => return Err(drift("Zielordner inzwischen durch anderen Typ belegt")),
                    None => {
                        let parent = path.parent().ok_or_else(|| invalid("Ordner ohne Eltern"))?;
                        fileops::create_dir(
                            &dst,
                            parent,
                            path.file_name()
                                .and_then(|s| s.to_str())
                                .ok_or_else(|| invalid("Ungültiger Ordnername"))?,
                            &self.request.sandbox,
                        )?;
                        report.directories += 1;
                        report.has_effect = true;
                        let actual = dst.stat(path)?;
                        self.directory_identities
                            .insert(path.clone(), (actual.device, actual.inode));
                    }
                }
                if let Some(receipt) = receipt.as_deref_mut() {
                    receipt.insert(
                        path.clone(),
                        crate::generated_source::GeneratedContentProof::Directory,
                    );
                }
            }
            for i in 0..self.files.len() {
                cancelled(&self.cancel)?;
                if self.files[i].replacement.is_some() && !self.request.options.overwrite {
                    report.skipped += 1;
                    report.unstarted -= 1;
                    continue;
                }
                let scope =
                    dst.staging_scope(&self.files[i].target, self.request.sandbox.as_deref())?;
                let scoped = scope.as_deref().unwrap_or(&dst);
                attempted_file = true;
                let files_before = report.files;
                let result = self.write_file(i, scoped, &mut report, on_progress);
                if report.files > files_before {
                    attempted_file = false;
                }
                let (bytes, blake3) = finish_staging_scope(scoped, result, |_, path| {
                    if !report.recovery_paths.contains(&path) {
                        report.recovery_paths.push(path);
                    }
                })?;
                if let Some(receipt) = receipt.as_deref_mut() {
                    receipt.insert(
                        self.files[i].target.clone(),
                        crate::generated_source::GeneratedContentProof::File { bytes, blake3 },
                    );
                }
                attempted_file = false;
            }
            report.status = ArchiveExtractStatus::Complete;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(report),
            Err(error) => {
                for path in crate::error::recovery_paths(&error) {
                    if !report.recovery_paths.contains(&path) {
                        report.recovery_paths.push(path);
                    }
                }
                if error_code(&error) == ErrorCode::OutcomeUnclear {
                    report.status = ArchiveExtractStatus::OutcomeUnclear;
                    report.has_effect = true;
                } else if self.cancel.is_cancelled() && error_code(&error) == ErrorCode::Interrupted
                {
                    report.status = ArchiveExtractStatus::Cancelled;
                    return Ok(report);
                } else {
                    report.status = ArchiveExtractStatus::Failed;
                }
                report.failed = u64::from(attempted_file);
                report.unstarted = report.unstarted.saturating_sub(report.failed);
                Err(ArchiveExtractFailure {
                    error,
                    report: Box::new(report),
                })
            }
        }
    }
    fn write_file(
        &mut self,
        index: usize,
        dst: &dyn Vfs,
        report: &mut ArchiveExtractReport,
        on_progress: &mut dyn FnMut(&ArchiveExtractProgress),
    ) -> io::Result<(u64, [u8; 32])> {
        let item = self.files[index].clone();
        self.reader.verify_source(&self.cancel)?;
        self.verify_root(dst)?;
        self.verify_parents(dst, &item.target)?;
        let parent = item
            .target
            .parent()
            .ok_or_else(|| invalid("Ziel ohne Eltern"))?;
        if dst.local_root_identity(&item.parent_identity.display_path)? != item.parent_identity
            || dst.local_root_identity(parent)?.mount != item.parent_identity.mount
            || dst.file_commit_class_at(parent)? != item.commit_class
            || dst.file_flush_guarantees_at(parent)? != item.flush
        {
            return Err(drift(
                "ZIP-Ziel-Elternvolume oder Commit-/Flushklasse änderte sich",
            ));
        }
        if let Ok(available) = dst.available_bytes(parent) {
            if item.size > available {
                return Err(coded_error(
                    io::ErrorKind::StorageFull,
                    ErrorCode::StorageFull,
                    "Nicht genug verfügbarer Platz für den angekündigten ZIP-Eintrag",
                ));
            }
        }
        let created = fileops::create_fileop_temp(dst, &item.target, &self.request.sandbox)
            .map_err(|e| e.error)?;
        let path = created.path;
        let proof = created.removal_proof;
        let result = (|| -> io::Result<(u64, [u8; 32])> {
            let mut sink = ExtractSink {
                inner: created.writer,
                hash: blake3::Hasher::new(),
                bytes: 0,
                report,
                on_progress,
                current: &item.relative,
            };
            // Die Größen des gebundenen Eintrags bestimmen die Wirkung. Die
            // Vorschaugrenzen würden große reguläre Entpackungen pauschal sperren.
            let io = item
                .compressed
                .checked_add(256 * 1024)
                .ok_or_else(|| invalid("ZIP-Lesebudget läuft über"))?;
            let calls = item
                .compressed
                .checked_add(8191)
                .and_then(|n| n.checked_div(8192))
                .and_then(|n| n.checked_add(1024))
                .ok_or_else(|| invalid("ZIP-Aufrufbudget läuft über"))?;
            let receipt = self.reader.read_entry_to(
                self.reader.entries()[item.index].id,
                ReadBudget {
                    max_output_bytes: item.size,
                    max_io_bytes: io,
                    max_read_calls: calls,
                },
                &self.cancel,
                &mut sink,
            )?;
            sink.inner.flush()?;
            let hash = *sink.hash.finalize().as_bytes();
            drop(sink);
            self.reader.verify_source(&self.cancel)?;
            self.verify_root(dst)?;
            self.verify_parents(dst, &item.target)?;
            dst.sync_file(&path)?;
            on_progress(&ArchiveExtractProgress {
                phase: ArchiveExtractPhase::Verifying,
                files: report.files,
                directories: report.directories,
                bytes: report.bytes,
                current: item.relative.clone(),
            });
            let mut check = blake3::Hasher::new();
            let mut read = dst.open_read(&path)?;
            let mut buf = [0; 65536];
            let mut count = 0u64;
            loop {
                cancelled(&self.cancel)?;
                let n = read.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                count = count
                    .checked_add(n as u64)
                    .ok_or_else(|| invalid("Readbackgröße läuft über"))?;
                if count > receipt.output_bytes {
                    return Err(invalid("Tempdatei nach dem Entpacken gewachsen"));
                }
                check.update(&buf[..n]);
            }
            if count != receipt.output_bytes || check.finalize().as_bytes() != &hash {
                return Err(invalid(
                    "ZIP-Tempdatei besteht BLAKE3-/Größenreadback nicht",
                ));
            }
            self.reader.verify_source(&self.cancel)?;
            self.verify_root(dst)?;
            self.verify_parents(dst, &item.target)?;
            on_progress(&ArchiveExtractProgress {
                phase: ArchiveExtractPhase::Committing,
                files: report.files,
                directories: report.directories,
                bytes: report.bytes,
                current: item.relative.clone(),
            });
            self.reader.verify_source(&self.cancel)?;
            self.verify_root(dst)?;
            self.verify_parents(dst, &item.target)?;
            Ok((receipt.output_bytes, hash))
        })();
        let (bytes, hash) = match result {
            Ok(v) => v,
            Err(error) => {
                return Err(fileops::fail_file_copy_before_commit(
                    dst,
                    &path,
                    proof.as_ref(),
                    error,
                    "ZIP-Inhaltsprüfung vor Commit scheiterte",
                )
                .error)
            }
        };
        let result = fileops::commit_temp_file_with_phase(
            fileops::TempFileCommitRequest {
                dst,
                temp_path: &path,
                dst_path: &item.target,
                sandbox: &self.request.sandbox,
                replace_existing: item.replacement.is_some(),
                expected_replacement: item.replacement.as_ref(),
                expected_size: Some(bytes),
                expected_hash: Some(hash),
            },
            Some(&self.cancel),
        );
        match result {
            Ok(effect) => {
                report.files += 1;
                report.bytes = report
                    .bytes
                    .checked_add(bytes)
                    .ok_or_else(|| invalid("Ergebnisgröße läuft über"))?;
                report.unstarted -= 1;
                report.has_effect = true;
                if effect == TargetCommitEffect::CopyNewBestEffort {
                    report.target_copy_new_best_effort = Some(true);
                }
                if effect == TargetCommitEffect::ReplaceBestEffort {
                    report.target_replace_best_effort = Some(true);
                }
                if let TargetCommitEffect::BackupRetained(path) = effect {
                    report.recovery_paths.push(path);
                }
                Ok((bytes, hash))
            }
            Err(TempCommitFailure::TempStillOwned(error)) => {
                Err(fileops::fail_file_copy_before_commit(
                    dst,
                    &path,
                    proof.as_ref(),
                    error,
                    "ZIP-Zielcommit scheiterte",
                )
                .error)
            }
            Err(failure) => {
                report.has_effect = true;
                if item.replacement.is_some() {
                    report.target_replace_best_effort = None;
                } else {
                    report.target_copy_new_best_effort = None;
                }
                let mut released = false;
                let error = failure.into_sync_error(&mut released);
                Err(with_recovery_context(error, &[], &[&path, &item.target]))
            }
        }
    }
}
struct ExtractSink<'a> {
    inner: Box<dyn Write + Send>,
    hash: blake3::Hasher,
    bytes: u64,
    report: &'a mut ArchiveExtractReport,
    on_progress: &'a mut dyn FnMut(&ArchiveExtractProgress),
    current: &'a str,
}
// Cancel prüft der Reader vor/nach jedem begrenzten Sinkchunk. Ein dauerndes
// Interrupted aus Write selbst würde write_all endlos erneut versuchen.
impl Write for ExtractSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(bytes)?;
        self.hash.update(&bytes[..n]);
        self.bytes += n as u64;
        (self.on_progress)(&ArchiveExtractProgress {
            phase: ArchiveExtractPhase::Extracting,
            files: self.report.files,
            directories: self.report.directories,
            bytes: self.report.bytes.saturating_add(self.bytes),
            current: self.current.into(),
        });
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
#[cfg(test)]
#[path = "archive_extract_tests.rs"]
mod tests;
