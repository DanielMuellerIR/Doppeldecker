//! Lokale ZIP-Erzeugung über die bestehenden Quell-, Temp- und Commitverträge.
//!
//! Dieser Produktweg liest niemals ein fremdes ZIP. Der Writer schreibt direkt in
//! genau eine laufgebundene Tempdatei. Erst nach explizitem Writerabschluss,
//! Quellschlussprüfung, Flush und BLAKE3-Readback wird der Zielname committed.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::atomic_file::{copy_stream, finish_staging_scope, COPY_BUFFER_BYTES};
use crate::error::{coded_error, error_code, with_recovery_context, ErrorCode};
use crate::fileops::{self, CreatedFileopTemp, TargetCommitEffect, TempCommitFailure};
use crate::remote_rename::{folded_name, names_may_collide};
use crate::vfs::{Entry, FileCommitClass, LocalRootIdentity, SourceReadOmission};
use crate::{Cancel, LocalVfs, Vfs};

// Zusätzlicher ZIP-Index und pfadgenaue Hinweise; die vorhandene Quellbindung
// besitzt ihren eigenen Metadatenbestand. Kein Versprechen für den Gesamt-Heap.
const METADATA_BUDGET_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchivePackCompression {
    Stored,
    #[default]
    Deflate,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArchivePackOptions {
    pub compression: ArchivePackCompression,
}
impl Default for ArchivePackOptions {
    fn default() -> Self {
        Self {
            compression: ArchivePackCompression::Deflate,
        }
    }
}
impl ArchivePackOptions {
    pub fn validate(&self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchivePackIssue {
    /// Ein unsicher dekodierter Name wird nur als Anzeige mit Rohbytes bilanziert.
    /// Dieser Wert wird niemals wieder als ausführbarer Pfad verwendet.
    pub path: String,
    pub reason: String,
    pub content_omitted: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchivePackPreview {
    pub source_root: PathBuf,
    pub target: PathBuf,
    pub files: u64,
    pub directories: u64,
    pub source_bytes: u64,
    pub commit_class: FileCommitClass,
    pub file_flush: bool,
    pub directory_flush: bool,
    pub issues: Vec<ArchivePackIssue>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchivePackStatus {
    Cancelled,
    Complete,
    Failed,
    OutcomeUnclear,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct ArchivePackReport {
    pub preview: ArchivePackPreview,
    pub status: ArchivePackStatus,
    pub files: u64,
    pub directories: u64,
    pub source_bytes: u64,
    pub archive_bytes: u64,
    pub target_copy_new_best_effort: Option<bool>,
    pub recovery_paths: Vec<String>,
}
#[derive(Debug)]
pub struct ArchivePackFailure {
    pub error: io::Error,
    pub report: Box<ArchivePackReport>,
}
impl std::fmt::Display for ArchivePackFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for ArchivePackFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}
#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchivePackPhase {
    Packing,
    Finalizing,
    Verifying,
    Committing,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct ArchivePackProgress {
    pub phase: ArchivePackPhase,
    pub files: u64,
    pub directories: u64,
    pub source_bytes: u64,
    pub archive_bytes: u64,
}
/// Der spätere Queueworker gibt nur seinen bereits vorhandenen privaten Besitzer
/// weiter. Es entsteht weder ein zweites Journal noch ein neuer Recoverymechanismus.
pub struct ArchivePackOwner<'a> {
    pub queue: &'a Path,
    pub operation_id: &'a str,
}

/// Persistierbare Absicht. Pfade und Optionen werden ausschließlich im Backend gebunden.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchivePackRequest {
    pub source_root: PathBuf,
    pub items: Vec<PathBuf>,
    pub target_dir: PathBuf,
    pub target_name: String,
    pub options: ArchivePackOptions,
    pub sandbox: Option<PathBuf>,
}
impl ArchivePackRequest {
    pub fn validate(&self) -> io::Result<()> {
        self.options.validate()?;
        if self.items.is_empty()
            || fileops::validate_file_operation_root(&self.source_root)? != self.source_root
            || fileops::validate_file_operation_root(&self.target_dir)? != self.target_dir
            || !usable_name(&self.target_name)
            || fileops::validate_file_name(&self.target_name)? != self.target_name
            || self.items.iter().any(|item| {
                !item.is_absolute()
                    || item.parent() != Some(self.source_root.as_path())
                    || item.to_str().is_none()
            })
        {
            return Err(invalid(
                "ZIP-Queueabsicht besitzt keine unveränderten direkten Quell-/Zielpfade",
            ));
        }
        Ok(())
    }
    pub fn prepare(&self, cancel: &Cancel) -> io::Result<PreparedArchivePack> {
        self.validate()?;
        prepare_archive_pack(
            &self.source_root,
            &self.items,
            &self.target_dir,
            &self.target_name,
            self.options.clone(),
            self.sandbox.clone(),
            cancel,
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchivePackBinding {
    pub digest: [u8; 32],
    pub preview: ArchivePackPreview,
}

struct PackEntry {
    path: PathBuf,
    name: String,
    entry: Entry,
}
pub struct PreparedArchivePack {
    source: Option<LocalVfs>,
    target: LocalVfs,
    target_identity: LocalRootIdentity,
    target_dir: PathBuf,
    entries: Vec<PackEntry>,
    options: ArchivePackOptions,
    sandbox: Option<PathBuf>,
    cancel: Cancel,
    preview: ArchivePackPreview,
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
fn check_cancel(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(coded_error(
            io::ErrorKind::Other,
            ErrorCode::Interrupted,
            "ZIP-Erzeugung abgebrochen",
        ))
    } else {
        Ok(())
    }
}
fn charge(used: &mut usize, amount: usize, budget: usize) -> io::Result<()> {
    *used = used
        .checked_add(amount)
        .ok_or_else(|| invalid("ZIP-Metadatenbudget übergelaufen"))?;
    if *used > budget {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "ZIP-Metadatenbudget erreicht; Auswahl verkleinern",
        ));
    }
    Ok(())
}
fn add_issue(
    issues: &mut Vec<ArchivePackIssue>,
    used: &mut usize,
    budget: usize,
    path: String,
    reason: String,
) -> io::Result<()> {
    charge(
        used,
        path.len().saturating_add(reason.len()).saturating_add(128),
        budget,
    )?;
    issues.push(ArchivePackIssue {
        path,
        reason,
        content_omitted: true,
    });
    Ok(())
}
fn add_metadata_issue(
    issues: &mut Vec<ArchivePackIssue>,
    used: &mut usize,
    budget: usize,
    path: String,
    reason: String,
) -> io::Result<()> {
    add_issue(issues, used, budget, path, reason)?;
    if let Some(issue) = issues.last_mut() {
        issue.content_omitted = false;
    }
    Ok(())
}
fn usable_name(name: &str) -> bool {
    !name.is_empty()
        && !matches!(name, "." | "..")
        && !name.contains(['/', '\\', '\0'])
        && !name.chars().any(char::is_control)
}
fn usable_source_name(parent: &Path, name: &str) -> bool {
    usable_entry_name(name)
        && crate::directory_create::validate_directory_create_request(parent, name)
            .is_ok_and(|validated| validated == name)
}
fn usable_entry_name(name: &str) -> bool {
    usable_name(name) && crate::archive_read::path_is_safe(name)
}
fn target_free(vfs: &dyn Vfs, parent: &Path, target: &Path) -> io::Result<()> {
    let name = target
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("ZIP-Zielname ist ungültig"))?;
    let listing = vfs.list_complete(parent)?;
    if !listing.complete {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "ZIP-Zielbestand ist unvollständig",
        ));
    }
    if listing
        .entries
        .iter()
        .any(|item| item.safely_decoded && names_may_collide(&item.entry.name, name))
    {
        return Err(coded_error(
            io::ErrorKind::AlreadyExists,
            ErrorCode::AlreadyExists,
            "ZIP-Zielname ist bereits belegt; bestehende Archive werden nicht ersetzt",
        ));
    }
    match vfs.stat(target) {
        Ok(_) => Err(coded_error(
            io::ErrorKind::AlreadyExists,
            ErrorCode::AlreadyExists,
            "ZIP-Ziel existiert bereits",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Alle Optionen und die lexikalische Auswahl werden vor Quell-/Zielzugriffen
/// validiert. Nur direkte Pane-Kinder sind auswählbar; Rekursion und Pfade entstehen
/// anschließend ausschließlich aus der gehaltenen lokalen Quellbindung.
pub fn prepare_archive_pack(
    source_root: &Path,
    items: &[PathBuf],
    target_dir: &Path,
    target_name: &str,
    options: ArchivePackOptions,
    sandbox: Option<PathBuf>,
    cancel: &Cancel,
) -> io::Result<PreparedArchivePack> {
    options.validate()?;
    let lexical_root = fileops::validate_file_operation_root(source_root)?;
    if lexical_root != source_root
        || items.is_empty()
        || !usable_name(target_name)
        || fileops::validate_file_name(target_name)? != target_name
    {
        return Err(invalid("ZIP-Erzeugung braucht eine direkte Auswahl und einen unveränderten einzelnen Zielnamen"));
    }
    if fileops::validate_file_operation_root(target_dir)? != target_dir {
        return Err(invalid("ZIP-Zielroot muss unverändert absolut sein"));
    }
    for item in items {
        if !item.is_absolute() || item.parent() != Some(source_root) {
            return Err(invalid(
                "ZIP-Auswahl akzeptiert ausschließlich direkte Kinder des Quellroots",
            ));
        }
    }
    check_cancel(cancel)?;
    let source_root = std::fs::canonicalize(source_root)?;
    let target_dir = std::fs::canonicalize(target_dir)?;
    fileops::guard(&sandbox, &target_dir)?;
    let target_path = target_dir.join(target_name);
    fileops::guard(&sandbox, &target_path)?;
    let target = LocalVfs::rooted(&target_dir)?;
    let target_identity = target.local_root_identity(&target_dir)?;
    target_free(&target, &target_dir, &target_path)?;
    let commit_class = target.file_commit_class_at(&target_dir)?;
    let (file_flush, directory_flush) = target.file_flush_guarantees_at(&target_dir)?;
    let target = target.with_sync_execution(Some(crate::real_mode::LocalSyncExecution {
        new_file_commit: commit_class,
        file_flush,
        directory_flush,
        irreversible_replacements: 0,
    }));
    let mut issues = Vec::new();
    let mut used = 0;
    let mut counts = BTreeMap::<String, usize>::new();
    for item in items {
        if let Some(name) = item
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|name| usable_source_name(&source_root, name))
        {
            *counts.entry(folded_name(name)).or_default() += 1;
        }
    }
    let mut selected = Vec::new();
    for item in items {
        let name = item
            .file_name()
            .ok_or_else(|| invalid("ZIP-Auswahl ohne Namen"))?;
        let path = source_root.join(name);
        let decoded = name.to_str();
        let reason = match decoded {
            Some(name) if usable_source_name(&source_root, name) && counts.get(&folded_name(name)) == Some(&1) => None,
            Some(name) if usable_source_name(&source_root, name) => Some("Case-/Unicode-Namensgruppe ist mehrdeutig; gesamte Gruppe ausgelassen"),
            _ => Some("Name ist für den portablen ZIP-Namensraum nicht eindeutig darstellbar; ausgelassen"),
        };
        if let Some(reason) = reason {
            #[cfg(unix)]
            let raw = {
                use std::os::unix::ffi::OsStrExt;
                name.as_bytes()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            };
            add_issue(
                &mut issues,
                &mut used,
                METADATA_BUDGET_BYTES,
                format!("{} (Rohname hex: {raw})", path.display()),
                reason.into(),
            )?;
        } else {
            selected.push(path);
        }
    }
    let source = if selected.is_empty() {
        None
    } else {
        Some(LocalVfs::rooted(&source_root)?.with_verified_reads(&selected, cancel)?)
    };
    let mut entries = Vec::new();
    let mut files = 0u64;
    let mut directories = 0u64;
    let mut source_bytes = 0u64;
    if let Some(source) = &source {
        if let Some(tree) = source.interactive_source_tree()? {
            for SourceReadOmission { path, reason } in tree.omitted {
                add_issue(&mut issues, &mut used, METADATA_BUDGET_BYTES, path, reason)?;
            }
        }
        let mut pending = selected.into_iter().rev().collect::<Vec<_>>();
        while let Some(path) = pending.pop() {
            check_cancel(cancel)?;
            let entry = source.stat(&path)?;
            if entry.is_symlink || (!entry.is_file && !entry.is_dir) {
                continue;
            }
            if entry.is_dir && target_dir.starts_with(&path) {
                return Err(invalid(
                    "ZIP-Ziel darf nicht innerhalb eines ausgewählten Quellordners liegen",
                ));
            }
            let relative = path
                .strip_prefix(&source_root)
                .map_err(|_| drift("ZIP-Quelle verließ den gehaltenen Root"))?;
            let name = relative
                .to_str()
                .ok_or_else(|| invalid("ZIP-Quellname nicht UTF-8"))?;
            if name.len().saturating_add(usize::from(entry.is_dir)) > u16::MAX as usize
                || relative
                    .components()
                    .any(|part| !part.as_os_str().to_str().is_some_and(usable_entry_name))
            {
                add_issue(&mut issues, &mut used, METADATA_BUDGET_BYTES, path.to_string_lossy().into_owned(), "ZIP-Pfad enthält einen nicht portabel darstellbaren Namen; Teilbaum ausgelassen".into())?;
                continue;
            }
            charge(
                &mut used,
                name.len().saturating_mul(4).saturating_add(2048),
                METADATA_BUDGET_BYTES,
            )?;
            add_metadata_issue(&mut issues, &mut used, METADATA_BUDGET_BYTES, path.to_string_lossy().into_owned(),
                "ZIP Portable: enthält Inhalt, einfache Rechte und DOS-Dateizeit in UTC (Zweisekundenauflösung, 1980–2107; sonst Standardzeit); Erstellungszeit, Sonderbits, Owner, ACL, xattrs/Resource Forks sowie Sparse-/Hardlinkbeziehungen werden nicht gespeichert".into())?;
            if entry.is_dir {
                directories = directories
                    .checked_add(1)
                    .ok_or_else(|| invalid("ZIP-Ordnerzahl übergelaufen"))?;
                let children = source
                    .interactive_copy_children(&path)?
                    .ok_or_else(|| drift("ZIP-Quellordner fehlt in der Bindung"))?;
                for child in children.into_iter().rev() {
                    pending.push(path.join(child.entry.name));
                }
            } else {
                files = files
                    .checked_add(1)
                    .ok_or_else(|| invalid("ZIP-Dateizahl übergelaufen"))?;
                source_bytes = source_bytes
                    .checked_add(entry.size)
                    .ok_or_else(|| invalid("ZIP-Quellgrößen übergelaufen"))?;
            }
            entries.push(PackEntry {
                path: path.clone(),
                name: name.to_string(),
                entry,
            });
        }
    }
    check_cancel(cancel)?;
    Ok(PreparedArchivePack {
        source,
        target,
        target_identity,
        target_dir,
        entries,
        options,
        sandbox,
        cancel: cancel.clone(),
        preview: ArchivePackPreview {
            source_root,
            target: target_path,
            files,
            directories,
            source_bytes,
            commit_class,
            file_flush,
            directory_flush,
            issues,
        },
    })
}

struct Written {
    hash: blake3::Hasher,
    bytes: u64,
}
struct ArchiveWriter {
    inner: Box<dyn Write + Send>,
    written: Arc<Mutex<Written>>,
    cancel: Cancel,
}
impl Write for ArchiveWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        check_cancel(&self.cancel)?;
        let count = self.inner.write(bytes)?;
        let mut state = self
            .written
            .lock()
            .map_err(|_| io::Error::other("ZIP-Ausgabebilanz gesperrt"))?;
        state.bytes = state
            .bytes
            .checked_add(count as u64)
            .ok_or_else(|| invalid("ZIP-Ausgabegröße übergelaufen"))?;
        state.hash.update(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        check_cancel(&self.cancel)?;
        self.inner.flush()
    }
}
fn zip_error(error: zip::result::ZipError) -> io::Error {
    match error {
        zip::result::ZipError::Io(error) => error,
        other => coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            format!("ZIP-Erzeugung: {other}"),
        ),
    }
}
fn zip_options(
    entry: &Entry,
    compression: ArchivePackCompression,
) -> zip::write::SimpleFileOptions {
    // ZIP64 ist vor dem ersten Inhaltsbyte gewählt, damit eine unbekannte spätere
    // Kompressionsgröße keine Größenpflicht oder einen Seek auf den Tempwriter erzeugt.
    let mut options = zip::write::SimpleFileOptions::default()
        .large_file(true)
        .compression_method(if compression == ArchivePackCompression::Stored {
            zip::CompressionMethod::Stored
        } else {
            zip::CompressionMethod::Deflated
        });
    if let Some(mode) = entry.mode {
        options = options.unix_permissions(mode & 0o777);
    }
    if let Some(date) = entry
        .mtime
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok())
    {
        if let Ok(date) = zip::DateTime::from_date_and_time(
            date.year() as u16,
            date.month() as u8,
            date.day(),
            date.hour(),
            date.minute(),
            date.second() & !1,
        ) {
            options = options.last_modified_time(date);
        }
    }
    options
}
impl PreparedArchivePack {
    pub fn binding(&self) -> io::Result<ArchivePackBinding> {
        self.verify_target(&self.target)?;
        let source_digest = self
            .source
            .as_ref()
            .map(|source| source.verified_source_binding_digest())
            .transpose()?
            .flatten();
        let mut digest = blake3::Hasher::new();
        digest.update(b"doppeldecker-archive-pack-binding-v1");
        digest.update(
            &serde_json::to_vec(&(
                source_digest,
                &self.target_identity,
                &self.options,
                &self.preview,
            ))
            .map_err(io::Error::other)?,
        );
        Ok(ArchivePackBinding {
            digest: *digest.finalize().as_bytes(),
            preview: self.preview.clone(),
        })
    }
    pub fn preview(&self) -> &ArchivePackPreview {
        &self.preview
    }
    fn verify_target(&self, dst: &dyn Vfs) -> io::Result<()> {
        if !dst.anchor_is_current()?
            || dst.local_root_identity(&self.target_dir)? != self.target_identity
        {
            return Err(drift(
                "ZIP-Zielroot oder Volume änderte sich seit der Vorbereitung",
            ));
        }
        if dst.file_commit_class_at(&self.target_dir)? != self.preview.commit_class {
            return Err(drift("ZIP-Commitklasse änderte sich seit der Vorbereitung"));
        }
        if dst.file_flush_guarantees_at(&self.target_dir)?
            != (self.preview.file_flush, self.preview.directory_flush)
        {
            return Err(drift(
                "ZIP-Flushfähigkeiten änderten sich seit der Vorbereitung",
            ));
        }
        target_free(dst, &self.target_dir, &self.preview.target)
    }
    fn verify_source(&self) -> io::Result<()> {
        if let Some(source) = &self.source {
            source.verified_source_binding_digest()?;
        }
        Ok(())
    }
    fn execution_target(&self, owner: Option<ArchivePackOwner<'_>>) -> LocalVfs {
        if let Some(owner) = owner {
            self.target
                .clone()
                .with_required_temp_recovery(owner.queue, owner.operation_id)
        } else {
            self.target.clone()
        }
    }
    pub fn execute(
        self,
        owner: Option<ArchivePackOwner<'_>>,
        on_progress: &mut dyn FnMut(&ArchivePackProgress),
    ) -> Result<ArchivePackReport, ArchivePackFailure> {
        let dst = self.execution_target(owner);
        self.execute_with_target(&dst, on_progress)
    }
    pub fn execute_generated(
        self,
        owner: Option<ArchivePackOwner<'_>>,
        on_progress: &mut dyn FnMut(&ArchivePackProgress),
    ) -> Result<crate::generated_source::GeneratedArchive<ArchivePackReport>, ArchivePackFailure>
    {
        let dst = self.execution_target(owner);
        let mut receipt = crate::generated_source::GeneratedReceipt::new();
        let report = self.execute_recorded(&dst, on_progress, Some(&mut receipt))?;
        Ok(crate::generated_source::GeneratedArchive { report, receipt })
    }
    fn execute_with_target(
        &self,
        dst: &dyn Vfs,
        on_progress: &mut dyn FnMut(&ArchivePackProgress),
    ) -> Result<ArchivePackReport, ArchivePackFailure> {
        self.execute_recorded(dst, on_progress, None)
    }
    fn execute_recorded(
        &self,
        dst: &dyn Vfs,
        on_progress: &mut dyn FnMut(&ArchivePackProgress),
        mut receipt: Option<&mut crate::generated_source::GeneratedReceipt>,
    ) -> Result<ArchivePackReport, ArchivePackFailure> {
        let mut report = ArchivePackReport {
            preview: self.preview.clone(),
            status: ArchivePackStatus::Failed,
            files: 0,
            directories: 0,
            source_bytes: 0,
            archive_bytes: 0,
            target_copy_new_best_effort: Some(false),
            recovery_paths: Vec::new(),
        };
        let result = (|| -> io::Result<()> {
            if self.cancel.is_cancelled() {
                report.status = ArchivePackStatus::Cancelled;
                return Ok(());
            }
            self.verify_source()?;
            self.verify_target(dst)?;
            let scope = dst.staging_scope(&self.preview.target, self.sandbox.as_deref())?;
            let scoped = scope.as_deref().unwrap_or(dst);
            let result =
                self.write_archive(scoped, &mut report, on_progress, receipt.as_deref_mut());
            finish_staging_scope(scoped, result, |_, path| {
                if !report.recovery_paths.contains(&path) {
                    report.recovery_paths.push(path);
                }
            })
        })();
        match result {
            Ok(()) => Ok(report),
            Err(error) => {
                report.status = if error_code(&error) == ErrorCode::OutcomeUnclear {
                    ArchivePackStatus::OutcomeUnclear
                } else {
                    ArchivePackStatus::Failed
                };
                if report.status == ArchivePackStatus::OutcomeUnclear
                    && self.preview.commit_class == FileCommitClass::BestEffort
                {
                    report.target_copy_new_best_effort = None;
                }
                for path in crate::error::recovery_paths(&error) {
                    if !report.recovery_paths.contains(&path) {
                        report.recovery_paths.push(path);
                    }
                }
                if self.cancel.is_cancelled() && error_code(&error) == ErrorCode::Interrupted {
                    report.status = ArchivePackStatus::Cancelled;
                    return Ok(report);
                }
                Err(ArchivePackFailure {
                    error,
                    report: Box::new(report),
                })
            }
        }
    }
    fn write_archive(
        &self,
        dst: &dyn Vfs,
        report: &mut ArchivePackReport,
        on_progress: &mut dyn FnMut(&ArchivePackProgress),
        receipt: Option<&mut crate::generated_source::GeneratedReceipt>,
    ) -> io::Result<()> {
        let CreatedFileopTemp {
            path,
            writer,
            removal_proof,
            metadata: _,
        } = fileops::create_fileop_temp(dst, &self.preview.target, &self.sandbox)
            .map_err(|failure| failure.error)?;
        let written = Arc::new(Mutex::new(Written {
            hash: blake3::Hasher::new(),
            bytes: 0,
        }));
        let write_result = (|| -> io::Result<(u64, blake3::Hash)> {
            let mut zip = zip::ZipWriter::new_stream(ArchiveWriter {
                inner: writer,
                written: written.clone(),
                cancel: self.cancel.clone(),
            });
            for item in &self.entries {
                check_cancel(&self.cancel)?;
                if item.entry.is_dir {
                    zip.add_directory(
                        format!("{}/", item.name),
                        zip_options(&item.entry, ArchivePackCompression::Stored),
                    )
                    .map_err(zip_error)?;
                    report.directories += 1;
                } else {
                    zip.start_file(
                        &item.name,
                        zip_options(&item.entry, self.options.compression),
                    )
                    .map_err(zip_error)?;
                    let source = self
                        .source
                        .as_ref()
                        .ok_or_else(|| drift("ZIP-Quellbindung fehlt"))?;
                    let mut reader = source.open_read(&item.path)?;
                    let copied =
                        copy_stream(reader.as_mut(), &mut zip, &self.cancel, &mut |bytes| {
                            report.source_bytes += bytes;
                            on_progress(&progress(report, ArchivePackPhase::Packing));
                        })?;
                    if !copied.complete {
                        check_cancel(&self.cancel)?;
                    }
                    if copied.bytes != item.entry.size {
                        return Err(drift(
                            "ZIP-Quelldatei lieferte einen unerwartet kurzen oder langen Inhalt",
                        ));
                    }
                    source.verify_transferred_source(&item.path, &copied.hash)?;
                    report.files += 1;
                }
                on_progress(&progress(report, ArchivePackPhase::Packing));
            }
            on_progress(&progress(report, ArchivePackPhase::Finalizing));
            check_cancel(&self.cancel)?;
            drop(zip.finish().map_err(zip_error)?);
            let state = written
                .lock()
                .map_err(|_| io::Error::other("ZIP-Ausgabebilanz gesperrt"))?;
            Ok((state.bytes, state.hash.clone().finalize()))
        })();
        let prepared = write_result.and_then(|(bytes, hash)| {
            report.archive_bytes = bytes;
            self.verify_source()?;
            self.verify_target(dst)?;
            check_cancel(&self.cancel)?;
            dst.sync_file(&path)?;
            on_progress(&progress(report, ArchivePackPhase::Verifying));
            let mut reader = dst.open_read(&path)?;
            let mut check = blake3::Hasher::new();
            let mut buffer = vec![0; COPY_BUFFER_BYTES];
            loop {
                check_cancel(&self.cancel)?;
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                check.update(&buffer[..count]);
            }
            if check.finalize() != hash {
                return Err(coded_error(
                    io::ErrorKind::InvalidData,
                    ErrorCode::InvalidData,
                    "ZIP-Tempdatei besteht den BLAKE3-Readback nicht",
                ));
            }
            drop(reader);
            self.verify_source()?;
            self.verify_target(dst)?;
            on_progress(&progress(report, ArchivePackPhase::Committing));
            check_cancel(&self.cancel)?;
            self.verify_source()?;
            self.verify_target(dst)?;
            Ok((bytes, hash))
        });
        let (bytes, hash) = match prepared {
            Ok(value) => value,
            Err(error) => {
                if self.cancel.is_cancelled() && error_code(&error) != ErrorCode::OutcomeUnclear {
                    if let Some(path) = fileops::cleanup_fileop_temp(
                        dst,
                        &path,
                        removal_proof.as_ref(),
                        "ZIP-Erzeugung abgebrochen",
                    )
                    .map_err(|failure| failure.error)?
                    {
                        report.recovery_paths.push(path);
                    }
                    report.status = ArchivePackStatus::Cancelled;
                    return Ok(());
                }
                return Err(fileops::fail_file_copy_before_commit(
                    dst,
                    &path,
                    removal_proof.as_ref(),
                    error,
                    "ZIP-Erzeugung scheiterte vor dem Commit",
                )
                .error);
            }
        };
        match fileops::commit_new_name(
            dst,
            &path,
            &self.preview.target,
            Some(bytes),
            Some(*hash.as_bytes()),
        ) {
            Ok(effect) => {
                report.status = ArchivePackStatus::Complete;
                report.target_copy_new_best_effort =
                    Some(effect == TargetCommitEffect::CopyNewBestEffort);
                if let Some(receipt) = receipt {
                    receipt.insert(
                        self.preview.target.clone(),
                        crate::generated_source::GeneratedContentProof::File {
                            bytes,
                            blake3: *hash.as_bytes(),
                        },
                    );
                }
                Ok(())
            }
            Err(TempCommitFailure::TempStillOwned(error)) => {
                Err(fileops::fail_file_copy_before_commit(
                    dst,
                    &path,
                    removal_proof.as_ref(),
                    error,
                    "ZIP-Zielcommit scheiterte",
                )
                .error)
            }
            Err(failure) => {
                let mut released = false;
                let error = failure.into_sync_error(&mut released);
                Err(with_recovery_context(
                    error,
                    &[],
                    &[&path, &self.preview.target],
                ))
            }
        }
    }
}
fn progress(report: &ArchivePackReport, phase: ArchivePackPhase) -> ArchivePackProgress {
    ArchivePackProgress {
        phase,
        files: report.files,
        directories: report.directories,
        source_bytes: report.source_bytes,
        archive_bytes: report.archive_bytes,
    }
}

#[cfg(test)]
#[path = "archive_pack_tests.rs"]
mod tests;
