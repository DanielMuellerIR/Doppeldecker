//! Versiegelte lokale Ergebnisse für den anschließenden gewöhnlichen Upload.
//! Die Herkunft bleibt lokal erzeugt; dies erteilt keine Remote-Snapshotrechte.
use crate::local_fs::{self, RootHandle};
use crate::local_read::FileVersion;
use crate::{Cancel, ErrorCode, LocalVfs, OperationQueue, Vfs};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

const STORE: &str = "generated-sources";
const MANIFEST: &str = "manifest.json";
const CONTENT: &str = "content";
const OWNER: &str = "queue-owner.json";
const PREFIX: &str = "generated-";
const MAX_ENTRIES: usize = 50_001;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) fn is_store_path(config: &Path, path: &Path) -> bool {
    path.starts_with(config.join(STORE))
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeneratedSourceKind {
    Zip,
    ZipExtraction,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedSourceIssue {
    pub path: String,
    pub reason: String,
    pub content_omitted: bool,
}

/// Pfadfreie Queuequelle. Nur der bekannte Store darf diese Referenz auflösen.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedSourceReference {
    id: String,
    manifest_digest: [u8; 32],
    pub kind: GeneratedSourceKind,
    pub label: String,
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub issues: Vec<GeneratedSourceIssue>,
}
impl GeneratedSourceReference {
    pub(crate) fn workspace_reference(&self) -> GeneratedWorkspaceReference {
        GeneratedWorkspaceReference {
            id: self.id.clone(),
        }
    }

    pub(crate) fn path_in(&self, config: &Path) -> io::Result<PathBuf> {
        self.validate()?;
        Ok(config.join(STORE).join(&self.id).join(CONTENT))
    }
    pub fn validate(&self) -> io::Result<()> {
        if !self.id.strip_prefix(PREFIX).is_some_and(|nonce| {
            nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) || self.manifest_digest == [0; 32]
            || self.label.is_empty()
            || self.label.len() > 8192
            || self.label.contains('\0')
            || self
                .files
                .checked_add(self.directories)
                .is_none_or(|count| count == 0 || count > MAX_ENTRIES as u64)
            || self.issues.len() > MAX_ENTRIES
            || self.issues.iter().any(|issue| {
                issue.path.len() > 8192
                    || issue.reason.len() > 8192
                    || issue.path.contains('\0')
                    || issue.reason.contains('\0')
            })
        {
            return Err(drift());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GeneratedContentProof {
    File { bytes: u64, blake3: [u8; 32] },
    Directory,
}
/// Die Produzenten geben diese Belege unmittelbar nach ihrem geprüften Commit aus.
pub(crate) type GeneratedReceipt = BTreeMap<PathBuf, GeneratedContentProof>;

/// Nicht serialisierbare Commitbelege des Produzenten. Queueberichte bleiben klein.
#[derive(Debug)]
pub struct GeneratedArchive<T> {
    pub report: T,
    pub(crate) receipt: GeneratedReceipt,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredEntry {
    version: FileVersion,
    proof: StoredProof,
}
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredProof {
    File { bytes: u64, blake3: [u8; 32] },
    Directory,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    kind: GeneratedSourceKind,
    label: String,
    issues: Vec<GeneratedSourceIssue>,
    content_version: FileVersion,
    source_binding_digest: [u8; 32],
    entries: BTreeMap<PathBuf, StoredEntry>,
}

#[derive(Clone)]
pub struct GeneratedSourceStore {
    root: RootHandle,
    queue_name: String,
    _process_lock: Arc<crate::OperationQueueProcessLock>,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedWorkspaceReference {
    id: String,
}
impl GeneratedWorkspaceReference {
    pub(crate) fn path_in(&self, config: &Path) -> io::Result<PathBuf> {
        self.validate()?;
        Ok(config.join(STORE).join(&self.id).join(CONTENT))
    }
    pub fn validate(&self) -> io::Result<()> {
        if !self.id.strip_prefix(PREFIX).is_some_and(|nonce| {
            nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(drift());
        }
        Ok(())
    }
}
pub struct GeneratedWorkspace {
    store: GeneratedSourceStore,
    id: String,
    directory: RootHandle,
    content: RootHandle,
}
pub struct ResolvedGeneratedSource {
    store: GeneratedSourceStore,
    directory: RootHandle,
    content: RootHandle,
    manifest_file: File,
    manifest_version: FileVersion,
    manifest: Manifest,
}

fn drift() -> io::Error {
    crate::coded_error(io::ErrorKind::InvalidData, ErrorCode::PlanDrift,
        "Die erzeugte Quelle fehlt oder hat sich geändert. Kein Upload; ZIP beziehungsweise Entpackauswahl bewusst erneut vorbereiten.")
}
fn stopped() -> io::Error {
    crate::coded_error(
        io::ErrorKind::Interrupted,
        ErrorCode::Interrupted,
        "Erzeugte Quelle vor dem Upload abgebrochen",
    )
}
fn cancelled(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(stopped())
    } else {
        Ok(())
    }
}
fn directory_version(root: &RootHandle) -> io::Result<FileVersion> {
    Ok(FileVersion::of(
        &File::from(root.fd().try_clone()?).metadata()?,
    ))
}
fn private_child(parent: &RootHandle, name: &str, exclusive: bool) -> io::Result<RootHandle> {
    if !parent.is_current()? || !parent.has_exclusive_namespace()? {
        return Err(drift());
    }
    match rustix::fs::mkdirat(parent.fd(), name, rustix::fs::Mode::from_raw_mode(0o700)) {
        Ok(()) => local_fs::sync_dir_beneath(parent, parent.path())?,
        Err(rustix::io::Errno::EXIST) if !exclusive => {}
        Err(error) => return Err(error.into()),
    }
    let child = RootHandle::open_beneath(parent, &parent.path().join(name))?;
    if !child.is_private()? {
        return Err(drift());
    }
    Ok(child)
}
fn relative_name(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|part| {
        matches!(part, Component::Normal(name) if name.to_str().is_some_and(|name| !name.contains('\0')))
    })
}

/// Prüft den vollständigen Ergebnisbaum. Keine nachträglichen stillen Auslassungen.
fn collect(
    root: &RootHandle,
    cancel: &Cancel,
) -> io::Result<(FileVersion, BTreeMap<PathBuf, StoredEntry>)> {
    if !root.is_current()? {
        return Err(drift());
    }
    let content_version = directory_version(root)?;
    let mut entries = BTreeMap::new();
    let mut stack = vec![root.path().to_path_buf()];
    while let Some(parent) = stack.pop() {
        cancelled(cancel)?;
        let held = RootHandle::open_beneath(root, &parent)?;
        let before = directory_version(&held)?;
        let listing = local_fs::list_complete_beneath(root, &parent)?;
        if listing.iter().any(|entry| !entry.safely_decoded) {
            return Err(drift());
        }
        let mut names = BTreeSet::new();
        for item in listing {
            cancelled(cancel)?;
            let entry = item.entry;
            if entry.is_symlink
                || (!entry.is_file && !entry.is_dir)
                || !names.insert(crate::remote_rename::folded_name(&entry.name))
            {
                return Err(drift());
            }
            let path = parent.join(&entry.name);
            let relative = path
                .strip_prefix(root.path())
                .map_err(|_| drift())?
                .to_path_buf();
            if !relative_name(&relative) || entries.len() >= MAX_ENTRIES {
                return Err(drift());
            }
            let stored = if entry.is_dir {
                let child = RootHandle::open_beneath(root, &path)?;
                stack.push(path);
                StoredEntry {
                    version: directory_version(&child)?,
                    proof: StoredProof::Directory,
                }
            } else {
                let mut file = local_fs::open_read_beneath(root, &path)?;
                let version = FileVersion::of(&file.metadata()?);
                let mut bytes = 0u64;
                let mut hash = blake3::Hasher::new();
                let mut buffer = [0; 65536];
                loop {
                    cancelled(cancel)?;
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    bytes = bytes.checked_add(count as u64).ok_or_else(drift)?;
                    hash.update(&buffer[..count]);
                }
                if version != FileVersion::of(&file.metadata()?) {
                    return Err(drift());
                }
                StoredEntry {
                    version,
                    proof: StoredProof::File {
                        bytes,
                        blake3: *hash.finalize().as_bytes(),
                    },
                }
            };
            if entries.insert(relative, stored).is_some() {
                return Err(drift());
            }
        }
        if before != directory_version(&held)? {
            return Err(drift());
        }
    }
    // Auch früh gescannte Mitglieder müssen nach dem letzten Dateihash noch passen.
    for (relative, entry) in &entries {
        cancelled(cancel)?;
        let path = root.path().join(relative);
        let actual = match entry.proof {
            StoredProof::Directory => directory_version(&RootHandle::open_beneath(root, &path)?)?,
            StoredProof::File { .. } => {
                FileVersion::of(&local_fs::open_read_beneath(root, &path)?.metadata()?)
            }
        };
        if actual != entry.version {
            return Err(drift());
        }
    }
    if !root.is_current()? || content_version != directory_version(root)? {
        return Err(drift());
    }
    Ok((content_version, entries))
}
fn reference(
    id: String,
    digest: [u8; 32],
    manifest: &Manifest,
) -> io::Result<GeneratedSourceReference> {
    let mut files = 0u64;
    let mut directories = 0u64;
    let mut bytes = 0u64;
    for entry in manifest.entries.values() {
        match entry.proof {
            StoredProof::Directory => directories += 1,
            StoredProof::File { bytes: size, .. } => {
                files += 1;
                bytes = bytes.checked_add(size).ok_or_else(drift)?;
            }
        }
    }
    let result = GeneratedSourceReference {
        id,
        manifest_digest: digest,
        kind: manifest.kind.clone(),
        label: manifest.label.clone(),
        files,
        directories,
        bytes,
        issues: manifest.issues.clone(),
    };
    result.validate()?;
    Ok(result)
}
impl GeneratedSourceStore {
    pub fn for_queue(queue: &OperationQueue) -> io::Result<Self> {
        let (owner, process_lock, config) = queue.ftps_owner(None)?;
        Ok(Self {
            root: private_child(&config, STORE, false)?,
            queue_name: owner
                .queue
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(drift)?
                .to_owned(),
            _process_lock: process_lock,
        })
    }
    pub fn begin(&self) -> io::Result<GeneratedWorkspace> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
        let id = format!(
            "{PREFIX}{}",
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let directory = private_child(&self.root, &id, true)?;
        let content = private_child(&directory, CONTENT, true)?;
        let owner = serde_json::to_vec(&(self.queue_name.clone(), directory.identity()))
            .map_err(io::Error::other)?;
        let vfs = LocalVfs::rooted(directory.path())?;
        if vfs.anchored_identity() != Some(directory.identity()) {
            return Err(drift());
        }
        crate::atomic_write::atomic_write_vfs_with_mode(
            &vfs,
            &directory.path().join(OWNER),
            &owner,
            ".generated-owner-",
            crate::atomic_write::AtomicWriteMode::Create,
            Some(0o600),
        )?;
        Ok(GeneratedWorkspace {
            store: self.clone(),
            id,
            directory,
            content,
        })
    }
    fn owns_workspace(&self, directory: &RootHandle) -> io::Result<bool> {
        let mut file = match local_fs::open_read_beneath(directory, &directory.path().join(OWNER)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.by_ref().take(4097).read_to_end(&mut bytes)?;
        if bytes.len() > 4096 {
            return Err(drift());
        }
        let (queue, identity): (String, (u64, u64)) =
            serde_json::from_slice(&bytes).map_err(|_| drift())?;
        if queue != self.queue_name || identity != directory.identity() {
            return Err(drift());
        }
        Ok(true)
    }

    pub fn open_workspace(
        &self,
        reference: &GeneratedWorkspaceReference,
    ) -> io::Result<GeneratedWorkspace> {
        reference.validate()?;
        if !self.root.is_current()? {
            return Err(drift());
        }
        let directory =
            RootHandle::open_beneath(&self.root, &self.root.path().join(&reference.id))?;
        self.owns_workspace(&directory)?;
        let content = RootHandle::open_beneath(&directory, &directory.path().join(CONTENT))?;
        if !directory.is_private()? || !content.is_private()? {
            return Err(drift());
        }
        Ok(GeneratedWorkspace {
            store: self.clone(),
            id: reference.id.clone(),
            directory,
            content,
        })
    }
    /// Entfernt ausschließlich einen vollständig belegten Zwischenstand. Ein
    /// weiterer Leser (auch aus einem anderen Fenster) hält die Bereinigung auf.
    pub(crate) fn remove_source(&self, reference: &GeneratedSourceReference) -> io::Result<()> {
        self.remove_source_with_hook(reference, |_| {})
    }

    fn remove_source_with_hook(
        &self,
        reference: &GeneratedSourceReference,
        before_cleanup: impl FnOnce(&Path),
    ) -> io::Result<()> {
        reference.validate()?;
        let path = self.root.path().join(&reference.id);
        match local_fs::stat_beneath(&self.root, &path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        let mut source = self.resolve(reference, &Cancel::new())?;
        rustix::fs::flock(
            &source.manifest_file,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )?;
        source.ensure_current()?;
        let entries = local_fs::list_complete_beneath(&source.directory, source.directory.path())?;
        if !self.owns_workspace(&source.directory)? {
            return Err(drift());
        }
        if entries.len() != 3
            || entries.iter().any(|entry| {
                !entry.safely_decoded
                    || !matches!(entry.entry.name.as_str(), CONTENT | MANIFEST | OWNER)
            })
        {
            return Err(drift());
        }
        if !self.root.is_private()? || !source.directory.is_private()? {
            return Err(drift());
        }
        // Der atomare Claim entfernt den sichtbaren Uploadpfad vor der letzten
        // Inhaltsprüfung. Bereits geöffnete Elterndeskriptoren können noch wirken;
        // deshalb löschen wir danach nur einzeln beanspruchte Manifestdateien und
        // ausschließlich leere Verzeichnisse, niemals einen neu erfassten Baum.
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
        let claim = self.root.path().join(format!(
            ".cleanup-{}",
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ));
        local_fs::rename_noreplace_beneath(&self.root, &path, &claim)?;
        let cleanup = (|| {
            let directory = RootHandle::open_beneath(&self.root, &claim)?;
            if directory.identity() != source.directory.identity() {
                return Err(drift());
            }
            let content = RootHandle::open_beneath(&directory, &claim.join(CONTENT))?;
            if content.identity() != source.content.identity() {
                return Err(drift());
            }
            source.directory = directory;
            source.content = content;
            source.ensure_current()?;
            let (version, entries) = collect(&source.content, &Cancel::new())?;
            if version != source.manifest.content_version || entries != source.manifest.entries {
                return Err(drift());
            }
            before_cleanup(source.content.path());
            let holding = private_child(&source.directory, ".cleanup-files", true)?;
            for (index, (relative, expected)) in source.manifest.entries.iter().enumerate() {
                let StoredProof::File { bytes, blake3 } = expected.proof else {
                    continue;
                };
                let from = source.content.path().join(relative);
                let held = local_fs::open_read_beneath(&source.content, &from)?;
                let identity = held.metadata()?;
                if FileVersion::of(&identity) != expected.version {
                    return Err(drift());
                }
                let claimed = holding.path().join(index.to_string());
                local_fs::rename_noreplace_beneath(&self.root, &from, &claimed)?;
                let checked = (|| {
                    let mut file = local_fs::open_read_beneath(&holding, &claimed)?;
                    let metadata = file.metadata()?;
                    if metadata.dev() != identity.dev() || metadata.ino() != identity.ino() {
                        return Err(drift());
                    }
                    let mut hash = blake3::Hasher::new();
                    let mut count = 0u64;
                    let mut buffer = [0; 65536];
                    loop {
                        let n = file.read(&mut buffer)?;
                        if n == 0 {
                            break;
                        }
                        hash.update(&buffer[..n]);
                        count += n as u64;
                    }
                    if count != bytes
                        || hash.finalize().as_bytes() != &blake3
                        || FileVersion::of(&metadata) != FileVersion::of(&file.metadata()?)
                    {
                        return Err(drift());
                    }
                    local_fs::remove_file_beneath(&holding, &claimed)
                })();
                if let Err(error) = checked {
                    // Ein ausgetauschter oder inzwischen veränderter Inhalt bleibt
                    // erhalten; ein neuer Inhalt am alten Namen wird nicht ersetzt.
                    let _ = local_fs::rename_noreplace_beneath(&self.root, &claimed, &from);
                    return Err(error);
                }
            }
            let mut directories: Vec<_> = source
                .manifest
                .entries
                .iter()
                .filter(|(_, entry)| matches!(entry.proof, StoredProof::Directory))
                .map(|(path, _)| path)
                .collect();
            directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
            for relative in directories {
                local_fs::remove_dir_beneath(
                    &source.content,
                    &source.content.path().join(relative),
                )?;
            }
            local_fs::remove_dir_beneath(&source.directory, source.content.path())?;
            local_fs::remove_dir_beneath(&source.directory, holding.path())?;
            local_fs::remove_file_beneath(&source.directory, &claim.join(MANIFEST))?;
            local_fs::remove_file_beneath(&source.directory, &claim.join(OWNER))?;
            local_fs::remove_dir_beneath(&self.root, &claim)?;
            local_fs::sync_dir_beneath(&self.root, self.root.path())
        })();
        if let Err(error) = cleanup {
            let restored = local_fs::rename_noreplace_beneath(&self.root, &claim, &path);
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "Zwischenstand mit Prüfrest erhalten unter {}: {error}",
                    if restored.is_ok() { &path } else { &claim }.display()
                ),
            ));
        }
        Ok(())
    }

    pub(crate) fn remove_empty_workspace(
        &self,
        reference: &GeneratedWorkspaceReference,
    ) -> io::Result<()> {
        let work = self.open_workspace(reference)?;
        if !self.owns_workspace(&work.directory)?
            || !local_fs::list_complete_beneath(&work.content, work.path())?.is_empty()
            || local_fs::list_complete_beneath(&work.directory, work.directory.path())?.len() != 2
        {
            return Err(drift());
        }
        local_fs::remove_dir_beneath(&work.directory, work.path())?;
        local_fs::remove_file_beneath(&work.directory, &work.directory.path().join(OWNER))?;
        local_fs::remove_dir_beneath(&self.root, work.directory.path())?;
        local_fs::sync_dir_beneath(&self.root, self.root.path())
    }

    pub fn resolve(
        &self,
        expected: &GeneratedSourceReference,
        cancel: &Cancel,
    ) -> io::Result<ResolvedGeneratedSource> {
        expected.validate()?;
        cancelled(cancel)?;
        if !self.root.is_current()? {
            return Err(drift());
        }
        let directory = RootHandle::open_beneath(&self.root, &self.root.path().join(&expected.id))?;
        if !directory.is_private()? {
            return Err(drift());
        }
        self.owns_workspace(&directory)?;
        let content = RootHandle::open_beneath(&directory, &directory.path().join(CONTENT))?;
        let mut file = local_fs::open_read_beneath(&directory, &directory.path().join(MANIFEST))?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockShared)?;
        let manifest_version = FileVersion::of(&file.metadata()?);
        let mut data = Vec::new();
        file.by_ref()
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut data)?;
        if data.len() as u64 > MAX_MANIFEST_BYTES
            || *blake3::hash(&data).as_bytes() != expected.manifest_digest
        {
            return Err(drift());
        }
        let manifest: Manifest = serde_json::from_slice(&data).map_err(|_| drift())?;
        if manifest.version != 1
            || manifest.entries.keys().any(|path| !relative_name(path))
            || reference(expected.id.clone(), expected.manifest_digest, &manifest)? != *expected
        {
            return Err(drift());
        }
        let (version, entries) = collect(&content, cancel)?;
        if version != manifest.content_version || entries != manifest.entries {
            return Err(drift());
        }
        let result = ResolvedGeneratedSource {
            store: self.clone(),
            directory,
            content,
            manifest_file: file,
            manifest_version,
            manifest,
        };
        result.ensure_current()?;
        Ok(result)
    }
}
impl GeneratedWorkspace {
    pub fn reference(&self) -> GeneratedWorkspaceReference {
        GeneratedWorkspaceReference {
            id: self.id.clone(),
        }
    }
    pub fn path(&self) -> &Path {
        self.content.path()
    }
    pub fn seal_archive(
        self,
        generated: GeneratedArchive<crate::archive_pack::ArchivePackReport>,
        cancel: &Cancel,
    ) -> io::Result<GeneratedSourceReference> {
        let report = generated.report;
        if report.status == crate::archive_pack::ArchivePackStatus::Cancelled {
            return Err(stopped());
        }
        if report.status != crate::archive_pack::ArchivePackStatus::Complete
            || report.preview.target.parent() != Some(self.path())
        {
            return Err(drift());
        }
        let label = report
            .preview
            .target
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(drift)?
            .to_string();
        let issues = report
            .preview
            .issues
            .into_iter()
            .map(|issue| GeneratedSourceIssue {
                path: issue.path,
                reason: issue.reason,
                content_omitted: issue.content_omitted,
            })
            .collect();
        self.seal(
            GeneratedSourceKind::Zip,
            label,
            issues,
            &generated.receipt,
            cancel,
        )
    }
    pub fn seal_extraction(
        self,
        generated: GeneratedArchive<crate::archive_extract::ArchiveExtractReport>,
        label: String,
        cancel: &Cancel,
    ) -> io::Result<GeneratedSourceReference> {
        let report = generated.report;
        if report.status == crate::archive_extract::ArchiveExtractStatus::Cancelled {
            return Err(stopped());
        }
        if report.status != crate::archive_extract::ArchiveExtractStatus::Complete
            || report.preview.target_dir != self.path()
            || report.failed != 0
            || report.unstarted != 0
        {
            return Err(drift());
        }
        let issues = report
            .preview
            .issues
            .into_iter()
            .map(|issue| GeneratedSourceIssue {
                path: issue.path,
                reason: issue.reason,
                content_omitted: issue.content_omitted,
            })
            .collect();
        self.seal(
            GeneratedSourceKind::ZipExtraction,
            label,
            issues,
            &generated.receipt,
            cancel,
        )
    }
    fn seal(
        self,
        kind: GeneratedSourceKind,
        label: String,
        issues: Vec<GeneratedSourceIssue>,
        receipt: &GeneratedReceipt,
        cancel: &Cancel,
    ) -> io::Result<GeneratedSourceReference> {
        let result = (|| {
            let (content_version, entries) = collect(&self.content, cancel)?;
            if receipt.len() != entries.len() {
                return Err(drift());
            }
            for (relative, entry) in &entries {
                let expected = receipt
                    .get(&self.content.path().join(relative))
                    .ok_or_else(drift)?;
                if !match (&entry.proof, expected) {
                    (StoredProof::Directory, GeneratedContentProof::Directory) => true,
                    (
                        StoredProof::File { bytes, blake3 },
                        GeneratedContentProof::File {
                            bytes: expected_bytes,
                            blake3: expected_hash,
                        },
                    ) => bytes == expected_bytes && blake3 == expected_hash,
                    _ => false,
                } {
                    return Err(drift());
                }
            }
            let items: Vec<_> = entries
                .keys()
                .filter(|path| path.components().count() == 1)
                .map(|path| self.path().join(path))
                .collect();
            let source_binding_digest = LocalVfs::rooted(self.path())?
                .with_verified_reads(&items, cancel)?
                .verified_source_binding_digest()?
                .ok_or_else(drift)?;
            let manifest = Manifest {
                source_binding_digest,
                version: 1,
                kind,
                label,
                issues,
                content_version,
                entries,
            };
            let data = serde_json::to_vec(&manifest).map_err(io::Error::other)?;
            if data.len() as u64 > MAX_MANIFEST_BYTES {
                return Err(drift());
            }
            let result = reference(self.id.clone(), *blake3::hash(&data).as_bytes(), &manifest)?;
            let vfs = LocalVfs::rooted(self.directory.path())?;
            if vfs.anchored_identity() != Some(self.directory.identity()) {
                return Err(drift());
            }
            crate::atomic_write::atomic_write_vfs_with_mode(
                &vfs,
                &self.directory.path().join(MANIFEST),
                &data,
                ".generated-manifest-",
                crate::atomic_write::AtomicWriteMode::Create,
                Some(0o600),
            )?;
            self.store.resolve(&result, cancel)?;
            Ok(result)
        })();
        result.map_err(|error| {
            crate::error::with_recovery_paths(
                error,
                &[self.directory.path().to_string_lossy().into_owned()],
            )
        })
    }
}
impl ResolvedGeneratedSource {
    pub fn path(&self) -> &Path {
        self.content.path()
    }
    pub fn items(&self) -> Vec<PathBuf> {
        self.manifest
            .entries
            .keys()
            .filter(|path| path.components().count() == 1)
            .map(|path| self.content.path().join(path))
            .collect()
    }
    pub fn read_vfs(&self, cancel: &Cancel) -> io::Result<LocalVfs> {
        self.ensure_current()?;
        let vfs = LocalVfs::rooted(self.path())?.with_verified_reads(&self.items(), cancel)?;
        if vfs.verified_source_binding_digest()? != Some(self.manifest.source_binding_digest) {
            return Err(drift());
        }
        let hashes = self
            .manifest
            .entries
            .iter()
            .filter_map(|(path, entry)| {
                if let StoredProof::File { blake3, .. } = entry.proof {
                    Some((self.path().join(path), blake3))
                } else {
                    None
                }
            })
            .collect();
        vfs.with_expected_content_hashes(&hashes)
    }
    pub fn ensure_current(&self) -> io::Result<()> {
        let visible =
            local_fs::open_read_beneath(&self.directory, &self.directory.path().join(MANIFEST))?;
        if !self.store.root.is_current()?
            || !self.directory.is_current()?
            || !self.content.is_current()?
            || FileVersion::of(&self.manifest_file.metadata()?) != self.manifest_version
            || FileVersion::of(&visible.metadata()?) != self.manifest_version
            || directory_version(&self.content)? != self.manifest.content_version
        {
            return Err(drift());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_pack::{prepare_archive_pack, ArchivePackOptions};
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn fixture() -> (
        tempfile::TempDir,
        OperationQueue,
        GeneratedSourceStore,
        PathBuf,
    ) {
        let area = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let root = fs::canonicalize(area.path()).unwrap();
        let source = root.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("original.txt"), b"Originale Archivquelle\n").unwrap();
        let queue = OperationQueue::open(root.join("config/queue.json")).unwrap();
        let store = GeneratedSourceStore::for_queue(&queue).unwrap();
        (area, queue, store, source)
    }
    fn generate(
        workspace: &GeneratedWorkspace,
        source: &Path,
    ) -> GeneratedArchive<crate::archive_pack::ArchivePackReport> {
        prepare_archive_pack(
            source,
            &[source.join("original.txt")],
            workspace.path(),
            "result.zip",
            ArchivePackOptions::default(),
            None,
            &Cancel::new(),
        )
        .unwrap()
        .execute_generated(None, &mut |_| {})
        .unwrap()
    }

    #[test]
    fn cleanup_preserves_active_readers_and_foreign_contents() {
        for alteration in ["reader", "extra_content", "extra_workspace_file", "symlink"] {
            let (_area, _queue, store, source) = fixture();
            let workspace = store.begin().unwrap();
            let path = workspace.path().to_path_buf();
            let generated = generate(&workspace, &source);
            let reference = workspace.seal_archive(generated, &Cancel::new()).unwrap();
            let reader = store.resolve(&reference, &Cancel::new()).unwrap();
            assert!(store.remove_source(&reference).is_err());
            assert!(path.join("result.zip").is_file());
            drop(reader);
            match alteration {
                "extra_content" => fs::write(path.join("foreign"), b"Fremd").unwrap(),
                "extra_workspace_file" => {
                    fs::write(path.parent().unwrap().join("foreign"), b"Fremd").unwrap()
                }
                "symlink" => {
                    fs::remove_file(path.join("result.zip")).unwrap();
                    symlink(source.join("original.txt"), path.join("result.zip")).unwrap();
                }
                _ => {}
            }
            if alteration == "reader" {
                store.remove_source(&reference).unwrap();
                store.remove_source(&reference).unwrap();
                assert!(!path.parent().unwrap().exists());
            } else {
                assert!(store.remove_source(&reference).is_err(), "{alteration}");
                assert!(path.exists());
            }
            assert_eq!(
                fs::read(source.join("original.txt")).unwrap(),
                b"Originale Archivquelle\n"
            );
        }
    }

    #[test]
    fn cleanup_preserves_contents_inserted_after_final_verification() {
        for replace in [false, true] {
            let (_area, _queue, store, source) = fixture();
            let workspace = store.begin().unwrap();
            let path = workspace.path().to_path_buf();
            let generated = generate(&workspace, &source);
            let reference = workspace.seal_archive(generated, &Cancel::new()).unwrap();
            let result = store.remove_source_with_hook(&reference, |claimed| {
                fs::write(
                    claimed.join(if replace {
                        "result.zip"
                    } else {
                        "late-foreign"
                    }),
                    b"Fremde Daten",
                )
                .unwrap();
            });
            assert!(result.is_err());
            assert_eq!(
                fs::read(path.join(if replace {
                    "result.zip"
                } else {
                    "late-foreign"
                }))
                .unwrap(),
                b"Fremde Daten"
            );
        }
    }

    #[test]
    fn cleanup_cannot_take_a_workspace_from_a_different_window_queue() {
        let (area, queue, store, source) = fixture();
        let workspace = store.begin().unwrap();
        let path = workspace.path().to_path_buf();
        let workspace_reference = workspace.reference();
        let generated = generate(&workspace, &source);
        let reference = workspace.seal_archive(generated, &Cancel::new()).unwrap();
        let (_, process_lock, _) = queue.ftps_owner(None).unwrap();
        let other_queue = OperationQueue::open_with_process_lock(
            area.path().join("config/other-window.json"),
            process_lock,
        )
        .unwrap();
        let other_store = GeneratedSourceStore::for_queue(&other_queue).unwrap();
        assert!(other_store.open_workspace(&workspace_reference).is_err());
        assert!(other_store.resolve(&reference, &Cancel::new()).is_err());
        assert!(other_store.remove_source(&reference).is_err());
        assert!(path.join("result.zip").is_file());
        store.remove_source(&reference).unwrap();
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn cleanup_of_unstarted_workspace_requires_empty_known_contents() {
        let (_area, _queue, store, source) = fixture();
        let workspace = store.begin().unwrap();
        let reference = workspace.reference();
        let path = workspace.path().to_path_buf();
        fs::write(path.join("foreign"), b"Erhalten").unwrap();
        assert!(store.remove_empty_workspace(&reference).is_err());
        assert_eq!(fs::read(path.join("foreign")).unwrap(), b"Erhalten");
        fs::remove_file(path.join("foreign")).unwrap();
        store.remove_empty_workspace(&reference).unwrap();
        assert!(!path.parent().unwrap().exists());
        assert!(source.join("original.txt").is_file());
    }

    #[test]
    fn generated_zip_survives_restart_without_rebuilding_from_changed_originals() {
        let (area, queue, store, source) = fixture();
        let workspace = store.begin().unwrap();
        let generated = generate(&workspace, &source);
        let bytes = fs::read(workspace.path().join("result.zip")).unwrap();
        let reference = workspace.seal_archive(generated, &Cancel::new()).unwrap();
        assert_eq!(reference.kind, GeneratedSourceKind::Zip);
        assert_eq!(reference.files, 1);
        assert_eq!(reference.directories, 0);
        assert_eq!(reference.bytes, bytes.len() as u64);
        let json = serde_json::to_vec(&reference).unwrap();
        assert!(!String::from_utf8_lossy(&json).contains("generated-sources/"));
        fs::write(source.join("original.txt"), b"Spaetere Originalfassung\n").unwrap();
        drop(store);
        drop(queue);
        let queue = OperationQueue::open(
            area.path()
                .canonicalize()
                .unwrap()
                .join("config/queue.json"),
        )
        .unwrap();
        let store = GeneratedSourceStore::for_queue(&queue).unwrap();
        let reference = serde_json::from_slice(&json).unwrap();
        let resolved = store.resolve(&reference, &Cancel::new()).unwrap();
        let vfs = resolved.read_vfs(&Cancel::new()).unwrap();
        let path = resolved.items().remove(0);
        let mut read = Vec::new();
        vfs.open_read(&path)
            .unwrap()
            .read_to_end(&mut read)
            .unwrap();
        assert_eq!(read, bytes);
        vfs.verify_transferred_source(&path, &blake3::hash(&read))
            .unwrap();
        assert_eq!(
            fs::read(source.join("original.txt")).unwrap(),
            b"Spaetere Originalfassung\n"
        );
    }

    #[test]
    fn producer_receipt_rejects_changed_bytes_before_the_manifest_is_sealed() {
        let (_area, _queue, store, source) = fixture();
        let workspace = store.begin().unwrap();
        let generated = generate(&workspace, &source);
        let path = workspace.path().join("result.zip");
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] ^= 1;
        fs::write(&path, &bytes).unwrap();
        let error = workspace
            .seal_archive(generated, &Cancel::new())
            .unwrap_err();
        assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(!crate::error::recovery_paths(&error).is_empty());
    }

    #[test]
    fn persisted_reference_rejects_missing_changed_extra_and_symlinked_results() {
        for change in [
            "same_size_and_mtime",
            "missing",
            "extra_file",
            "extra_directory",
            "symlink",
        ] {
            let (_area, _queue, store, source) = fixture();
            let workspace = store.begin().unwrap();
            let generated = generate(&workspace, &source);
            let root = workspace.path().to_path_buf();
            let path = root.join("result.zip");
            let reference = workspace.seal_archive(generated, &Cancel::new()).unwrap();
            match change {
                "same_size_and_mtime" => {
                    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
                    let mut bytes = fs::read(&path).unwrap();
                    bytes[0] ^= 1;
                    fs::write(&path, &bytes).unwrap();
                    File::options()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_times(fs::FileTimes::new().set_modified(mtime))
                        .unwrap();
                }
                "missing" => fs::remove_file(&path).unwrap(),
                "extra_file" => fs::write(root.join("extra"), b"Zusatz").unwrap(),
                "extra_directory" => fs::create_dir(root.join("extra")).unwrap(),
                "symlink" => {
                    fs::remove_file(&path).unwrap();
                    symlink(source.join("original.txt"), &path).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                store.resolve(&reference, &Cancel::new()).is_err(),
                "{change}"
            );
            assert_eq!(
                fs::read(source.join("original.txt")).unwrap(),
                b"Originale Archivquelle\n"
            );
        }
    }

    #[test]
    fn foreign_root_and_manifest_replacement_do_not_rebind_a_generated_reference() {
        for replace in ["content_root", "manifest"] {
            let (_area, _queue, store, source) = fixture();
            let workspace = store.begin().unwrap();
            let generated = generate(&workspace, &source);
            let root = workspace.path().to_path_buf();
            let reference = workspace.seal_archive(generated, &Cancel::new()).unwrap();
            let held = store.resolve(&reference, &Cancel::new()).unwrap();
            if replace == "content_root" {
                fs::rename(&root, root.with_file_name("original-content")).unwrap();
                fs::create_dir(&root).unwrap();
                fs::write(root.join("result.zip"), b"Fremd").unwrap();
            } else {
                let manifest = root.parent().unwrap().join(MANIFEST);
                let bytes = fs::read(&manifest).unwrap();
                fs::rename(&manifest, manifest.with_file_name("original-manifest")).unwrap();
                fs::write(&manifest, bytes).unwrap();
            }
            assert!(held.ensure_current().is_err(), "{replace}");
            assert!(held.read_vfs(&Cancel::new()).is_err(), "{replace}");
        }
    }

    #[test]
    fn frozen_hash_prevents_commit_and_preserves_recovery_evidence() {
        let (_area, _queue, store, source) = fixture();
        let workspace = store.begin().unwrap();
        let generated = generate(&workspace, &source);
        let reference = workspace.seal_archive(generated, &Cancel::new()).unwrap();
        let resolved = store.resolve(&reference, &Cancel::new()).unwrap();
        let items = resolved.items();
        let source_vfs = LocalVfs::rooted(resolved.path())
            .unwrap()
            .with_verified_reads(&items, &Cancel::new())
            .unwrap()
            .with_expected_content_hashes(&BTreeMap::from([(
                items[0].clone(),
                *blake3::hash(b"andere Bytes").as_bytes(),
            )]))
            .unwrap();
        let target = source.parent().unwrap().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("sentinel"), b"Erhalten").unwrap();
        let target_vfs = LocalVfs::rooted(&target).unwrap();
        let error = crate::fileops::copy_items(
            crate::fileops::FileOperationRequest {
                src: &source_vfs,
                dst: &target_vfs,
                items: &items,
                dst_dir: &target,
                policy: crate::OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
        assert!(!target.join("result.zip").exists());
        assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"Erhalten");
        let leftovers: Vec<_> = fs::read_dir(&target)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.file_name().unwrap() != "sentinel")
            .collect();
        assert!(leftovers.len() <= 1);
        for path in leftovers {
            assert!(
                crate::error::recovery_paths(&error).contains(&path.to_string_lossy().into_owned())
            );
            assert_eq!(fs::read(&path).unwrap(), fs::read(&items[0]).unwrap());
        }
    }

    #[test]
    fn generated_extraction_keeps_files_and_empty_directories_bound_to_the_output() {
        let (_area, _queue, store, source) = fixture();
        let tree = source.join("tree");
        fs::create_dir(&tree).unwrap();
        fs::create_dir(tree.join("empty")).unwrap();
        fs::write(tree.join("leaf"), b"Blatt").unwrap();
        let archive_root = source.parent().unwrap().join("archives");
        fs::create_dir(&archive_root).unwrap();
        prepare_archive_pack(
            &source,
            &[tree],
            &archive_root,
            "input.zip",
            ArchivePackOptions::default(),
            None,
            &Cancel::new(),
        )
        .unwrap()
        .execute(None, &mut |_| {})
        .unwrap();
        let workspace = store.begin().unwrap();
        let generated = crate::archive_extract::ArchiveExtractRequest {
            source_root: archive_root.clone(),
            source_path: archive_root.join("input.zip"),
            snapshot: None,
            target_dir: workspace.path().to_path_buf(),
            options: Default::default(),
            selection: Default::default(),
            sandbox: None,
        }
        .prepare(&Cancel::new())
        .unwrap()
        .execute_generated(None, &mut |_| {})
        .unwrap();
        let reference = workspace
            .seal_extraction(generated, "input.zip".into(), &Cancel::new())
            .unwrap();
        assert_eq!(reference.kind, GeneratedSourceKind::ZipExtraction);
        assert_eq!(reference.files, 1);
        assert_eq!(reference.directories, 2);
        assert_eq!(reference.bytes, 5);
        fs::remove_file(archive_root.join("input.zip")).unwrap();
        let resolved = store.resolve(&reference, &Cancel::new()).unwrap();
        assert!(resolved.path().join("tree/empty").is_dir());
        assert_eq!(
            fs::read(resolved.path().join("tree/leaf")).unwrap(),
            b"Blatt"
        );
        let vfs = resolved.read_vfs(&Cancel::new()).unwrap();
        let scope = vfs.interactive_source_tree().unwrap().unwrap();
        assert_eq!(scope.files, 1);
        assert_eq!(scope.directories, 2);
    }
}
