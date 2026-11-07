//! Descriptor-relative Quellen und iterative Baumbeobachtung für Uploads.

use crate::local_fs::{list_upload_beneath, open_read_beneath, stat_beneath, RootHandle};
use crate::vfs::{Entry, InteractiveSourceTree, ProofListEntry, SourceReadOmission};
use crate::{coded_error, Cancel, ErrorCode};
use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) struct VerifiedLocalFiles {
    expected_hashes: Option<BTreeMap<PathBuf, [u8; 32]>>,
    files: BTreeMap<PathBuf, Arc<BoundFile>>,
    cancel: Cancel,
    root: RootHandle,
    anchors: BTreeMap<PathBuf, crate::vfs::Entry>,
    completed: Arc<Mutex<BTreeMap<PathBuf, blake3::Hash>>>,
    directories: BTreeMap<PathBuf, BoundDirectory>,
    summary: Option<InteractiveSourceTree>,
    binding_digest: [u8; 32],
    omitted_entries: BTreeMap<PathBuf, Entry>,
    omitted_selection_versions: BTreeMap<PathBuf, Vec<u8>>,
    has_name_group_omissions: bool,
}

fn omitted_selection_version(root: &RootHandle, path: &Path) -> io::Result<Vec<u8>> {
    let parent = RootHandle::open_beneath(root, path.parent().ok_or_else(drift)?)?;
    let stat = rustix::fs::statat(
        parent.fd(),
        path.file_name().ok_or_else(drift)?,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )?;
    serde_json::to_vec(&(
        stat.st_dev,
        stat.st_ino,
        stat.st_size,
        stat.st_mode,
        stat.st_mtime,
        stat.st_mtime_nsec,
        stat.st_ctime,
        stat.st_ctime_nsec,
    ))
    .map_err(io::Error::other)
}

struct BoundDirectory {
    version: FileVersion,
    // Auch ausgelassene Einträge bleiben im ursprünglichen Bestand gebunden;
    // ein späterer Austausch darf sie nicht unbemerkt in kopierbare Kinder ändern.
    observed: Vec<ProofListEntry>,
    children: Vec<ProofListEntry>,
}

fn directory_version(handle: &RootHandle) -> io::Result<FileVersion> {
    Ok(FileVersion::of(
        &File::from(handle.fd().try_clone()?).metadata()?,
    ))
}

fn listing_digest(entries: &[ProofListEntry]) -> io::Result<Vec<u8>> {
    let mut entries = entries
        .iter()
        .map(|proof| (&proof.raw_name, &proof.entry))
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    serde_json::to_vec(&entries).map_err(io::Error::other)
}

fn usable_upload_name(parent: &Path, child: &ProofListEntry) -> bool {
    child.safely_decoded
        && crate::directory_create::validate_directory_create_request(parent, &child.entry.name)
            .is_ok_and(|validated| validated == child.entry.name)
}

fn upload_children(
    parent: &Path,
    observed: &[ProofListEntry],
) -> (Vec<ProofListEntry>, Vec<SourceReadOmission>, bool) {
    let mut name_counts = BTreeMap::<String, usize>::new();
    for child in observed {
        if usable_upload_name(parent, child)
            && !child.entry.is_symlink
            && (child.entry.is_file || child.entry.is_dir)
        {
            *name_counts
                .entry(crate::remote_rename::folded_name(&child.entry.name))
                .or_default() += 1;
        }
    }
    let mut children = Vec::new();
    let mut omitted = Vec::new();
    let mut has_name_groups = false;
    for child in observed {
        let name = &child.entry.name;
        let (path, reason) = if !usable_upload_name(parent, child) {
            let display = child
                .raw_name
                .iter()
                .map(|byte| format!("%{byte:02X}"))
                .collect::<String>();
            (
                parent.join(display),
                "Nicht bedienbarer Name (Rohbytes als Prozent-Hex); Inhalt nicht übertragen",
            )
        } else if child.entry.is_symlink || (!child.entry.is_file && !child.entry.is_dir) {
            (
                parent.join(name),
                "Symlink oder Spezialdatei; Inhalt nicht verfolgt",
            )
        } else if name_counts[&crate::remote_rename::folded_name(name)] > 1 {
            has_name_groups = true;
            // Keinen zufälligen Gewinner übertragen: Auch auf einer exakten Quelle
            // kann das entfernte Ziel alle Namen dieser Gruppe gleichsetzen.
            (parent.join(name), "Am Ziel möglicherweise gleichwertige Case-/Unicode-Namen; gesamte Namensgruppe nicht übertragen")
        } else {
            children.push(child.clone());
            continue;
        };
        omitted.push(SourceReadOmission {
            path: path.to_string_lossy().into_owned(),
            reason: reason.into(),
        });
    }
    children.sort_by(|a, b| a.raw_name.cmp(&b.raw_name));
    (children, omitted, has_name_groups)
}

fn limit(message: &str) -> io::Error {
    coded_error(
        io::ErrorKind::InvalidInput,
        ErrorCode::InvalidInput,
        message,
    )
}

fn add_bytes(total: u64, size: u64) -> io::Result<u64> {
    total.checked_add(size).ok_or_else(drift)
}

fn check_preparation(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        return Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::Interrupted,
            "Lokale Uploadvorbereitung abgebrochen",
        ));
    }
    Ok(())
}

struct BoundFile {
    root: RootHandle,
    path: PathBuf,
    version: FileVersion,
    parents: Vec<(PathBuf, (u64, u64))>,
    cancel: Cancel,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileVersion {
    device: u64,
    inode: u64,
    size: u64,
    mtime: i64,
    mtime_nanos: i64,
    ctime: i64,
    ctime_nanos: i64,
    mode: u32,
}
impl FileVersion {
    pub(crate) fn of(meta: &Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.len(),
            mtime: meta.mtime(),
            mtime_nanos: meta.mtime_nsec(),
            ctime: meta.ctime(),
            ctime_nanos: meta.ctime_nsec(),
            mode: meta.mode(),
        }
    }
}

fn drift() -> io::Error {
    coded_error(
        io::ErrorKind::Interrupted,
        ErrorCode::PlanDrift,
        "Lokale Uploadquelle änderte ihre Identität oder ihren Inhalt",
    )
}

impl BoundFile {
    fn verify(&self) -> io::Result<()> {
        if !self.root.is_current()? {
            return Err(drift());
        }
        for (path, expected) in &self.parents {
            let current = stat_beneath(&self.root, path)?;
            if !current.is_dir
                || current.is_symlink
                || (current.device, current.inode) != (Some(expected.0), Some(expected.1))
            {
                return Err(drift());
            }
        }
        let entry = stat_beneath(&self.root, &self.path)?;
        if !entry.is_file
            || entry.is_symlink
            || entry.device != Some(self.version.device)
            || entry.inode != Some(self.version.inode)
            || entry.size != self.version.size
            || FileVersion::of(&open_read_beneath(&self.root, &self.path)?.metadata()?)
                != self.version
        {
            return Err(drift());
        }
        Ok(())
    }
}

impl VerifiedLocalFiles {
    pub(crate) fn has_name_group_omissions(&self) -> bool {
        self.has_name_group_omissions
    }

    pub(crate) fn bind(root: &RootHandle, items: &[PathBuf], cancel: &Cancel) -> io::Result<Self> {
        if items.is_empty() {
            return Err(limit(
                "WebDAV-Upload benötigt mindestens einen direkten Auswahleintrag",
            ));
        }
        let mut exact_paths = std::collections::HashSet::new();
        let mut folded_names = BTreeMap::<String, usize>::new();
        for path in items {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| limit("Uploadauswahl braucht bedienbare UTF-8-Namen"))?;
            if !exact_paths.insert(path) {
                return Err(limit("Uploadauswahl enthält denselben Quellpfad mehrfach"));
            }
            *folded_names
                .entry(crate::remote_rename::folded_name(name))
                .or_default() += 1;
        }
        let grouped_paths = items
            .iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| folded_names[&crate::remote_rename::folded_name(name)] > 1)
            })
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut result = Self {
            expected_hashes: None,
            files: BTreeMap::new(),
            cancel: cancel.clone(),
            root: root.clone(),
            anchors: BTreeMap::new(),
            completed: Arc::new(Mutex::new(BTreeMap::new())),
            directories: BTreeMap::new(),
            summary: None,
            binding_digest: [0; 32],
            omitted_entries: BTreeMap::new(),
            omitted_selection_versions: BTreeMap::new(),
            has_name_group_omissions: !grouped_paths.is_empty(),
        };
        let mut total = 0u64;
        let mut count = 0usize;
        let mut omitted = Vec::new();
        let mut pending = items.iter().rev().cloned().collect::<Vec<_>>();
        while let Some(path) = pending.pop() {
            check_preparation(cancel)?;
            count += 1;
            result.bind_parents(&path)?;
            let meta = stat_beneath(root, &path)?;
            if grouped_paths.contains(&path) {
                // Die ganze direkte Namensgruppe bleibt unberührt. Nur ihr eigener
                // lstat-Beleg wird gehalten; insbesondere keinen Ordner öffnen/listen.
                let version = omitted_selection_version(root, &path)?;
                result
                    .omitted_selection_versions
                    .insert(path.clone(), version);
                omitted.push(SourceReadOmission {
                    path: path.to_string_lossy().into_owned(),
                    reason: "Am Ziel möglicherweise gleichwertige Case-/Unicode-Namen; gesamte direkte Namensgruppe nicht übertragen".into(),
                });
                result.omitted_entries.insert(path, meta);
                continue;
            }
            if meta.is_symlink || (!meta.is_file && !meta.is_dir) {
                omitted.push(SourceReadOmission {
                    path: path.to_string_lossy().into_owned(),
                    reason: "Symlink oder Spezialdatei; Inhalt nicht verfolgt".into(),
                });
                result.omitted_entries.insert(path, meta);
                continue;
            }
            if meta.is_dir {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(drift)?;
                if crate::directory_create::validate_directory_create_request(
                    path.parent().ok_or_else(drift)?,
                    name,
                )? != name
                {
                    return Err(limit(
                        "Direkter Uploadordner kann nicht ohne Namensänderung bedient werden",
                    ));
                }
                result.bind_anchor(&path)?;
                let handle = RootHandle::open_beneath(root, &path)?;
                let version = directory_version(&handle)?;
                let observed = list_upload_beneath(&handle, &path, cancel)?;
                let (children, child_omissions, has_name_groups) =
                    upload_children(&path, &observed);
                result.has_name_group_omissions |= has_name_groups;
                count += child_omissions.len();
                omitted.extend(child_omissions);
                for child in children.iter().rev() {
                    pending.push(path.join(&child.entry.name));
                }
                result.directories.insert(
                    path,
                    BoundDirectory {
                        version,
                        observed,
                        children,
                    },
                );
                continue;
            }
            let file = open_read_beneath(root, &path)?;
            let version = FileVersion::of(&file.metadata()?);
            total = add_bytes(total, version.size)?;
            let bound = BoundFile {
                cancel: cancel.clone(),
                root: root.clone(),
                path: path.clone(),
                version,
                parents: result
                    .anchors
                    .iter()
                    .filter(|(parent, _)| path.starts_with(parent))
                    .map(|(parent, entry)| {
                        (
                            parent.clone(),
                            (entry.device.unwrap_or(0), entry.inode.unwrap_or(0)),
                        )
                    })
                    .collect(),
            };
            bound.verify()?;
            if result.files.insert(path, Arc::new(bound)).is_some() {
                return Err(drift());
            }
        }
        result.verify_all()?;
        check_preparation(cancel)?;
        {
            let mut digest = blake3::Hasher::new();
            digest.update(b"doppeldecker-local-upload-tree-v1");
            digest.update(&serde_json::to_vec(items).map_err(io::Error::other)?);
            // Auch Eltern außerhalb der ausgewählten Teilbäume gehören zum
            // bestätigten Umfang. Gleicher Inhalt nach einem Mount-/Elterntausch
            // darf bei einer frischen Queuevorbereitung keinen identischen Beleg liefern.
            for (path, anchor) in &result.anchors {
                digest.update(&serde_json::to_vec(&(path, anchor)).map_err(io::Error::other)?);
            }
            for (path, file) in &result.files {
                digest
                    .update(&serde_json::to_vec(&(path, &file.version)).map_err(io::Error::other)?);
            }
            for (path, dir) in &result.directories {
                digest
                    .update(&serde_json::to_vec(&(path, &dir.version)).map_err(io::Error::other)?);
                digest.update(&listing_digest(&dir.observed)?);
            }
            digest.update(&serde_json::to_vec(&omitted).map_err(io::Error::other)?);
            digest.update(&serde_json::to_vec(&result.omitted_entries).map_err(io::Error::other)?);
            if !result.omitted_selection_versions.is_empty() {
                digest.update(b"direct-name-group-versions-v1");
                digest.update(
                    &serde_json::to_vec(&result.omitted_selection_versions)
                        .map_err(io::Error::other)?,
                );
            }
            let mut binding = blake3::Hasher::new();
            binding.update(b"doppeldecker-local-source-versions-v1");
            binding.update(digest.finalize().as_bytes());
            binding.update(
                &serde_json::to_vec(&(
                    result.root.identity(),
                    result.root.current_mount_identity()?,
                ))
                .map_err(io::Error::other)?,
            );
            result.binding_digest = *binding.finalize().as_bytes();
            if !result.directories.is_empty() || !omitted.is_empty() {
                let summary = InteractiveSourceTree {
                    digest: digest.finalize().to_hex().to_string(),
                    entries: count as u64,
                    files: result.files.len() as u64,
                    directories: result.directories.len() as u64,
                    bytes: total,
                    omitted,
                };
                summary.validate()?;
                result.summary = Some(summary);
            }
        }
        Ok(result)
    }

    fn bind_anchor(&mut self, path: &Path) -> io::Result<()> {
        if !self.anchors.contains_key(path) {
            let handle = RootHandle::open_beneath(&self.root, path)?;
            let entry = stat_beneath(&self.root, path)?;
            if !entry.is_dir
                || entry.is_symlink
                || Some(handle.identity().0) != entry.device
                || Some(handle.identity().1) != entry.inode
            {
                return Err(drift());
            }
            self.anchors.insert(path.to_path_buf(), entry);
        }
        Ok(())
    }
    fn bind_parents(&mut self, path: &Path) -> io::Result<()> {
        let relative = path.strip_prefix(self.root.path()).map_err(|_| drift())?;
        let mut parent = self.root.path().to_path_buf();
        for component in relative.parent().unwrap_or(Path::new("")).components() {
            parent.push(component);
            self.bind_anchor(&parent)?;
        }
        Ok(())
    }
    fn verify_anchors(&self, relevant: Option<&Path>) -> io::Result<()> {
        if !self.root.is_current()? {
            return Err(drift());
        }
        for (path, expected) in &self.anchors {
            if relevant.is_some_and(|relevant| !relevant.starts_with(path)) {
                continue;
            }
            let current = stat_beneath(&self.root, path)?;
            if !current.is_dir
                || current.is_symlink
                || current.device != expected.device
                || current.inode != expected.inode
            {
                return Err(drift());
            }
        }
        Ok(())
    }
    fn verify_directory(&self, path: &Path) -> io::Result<&BoundDirectory> {
        self.verify_anchors(Some(path))?;
        let directory = self.directories.get(path).ok_or_else(drift)?;
        let handle = RootHandle::open_beneath(&self.root, path)?;
        if directory_version(&handle)? != directory.version
            || listing_digest(&list_upload_beneath(&handle, path, &self.cancel)?)?
                != listing_digest(&directory.observed)?
        {
            return Err(drift());
        }
        Ok(directory)
    }
    fn verify_omitted_selection(&self) -> io::Result<()> {
        for (path, expected) in &self.omitted_selection_versions {
            check_preparation(&self.cancel)?;
            if omitted_selection_version(&self.root, path)? != *expected {
                return Err(drift());
            }
        }
        Ok(())
    }
    fn verify_all(&self) -> io::Result<()> {
        self.verify_anchors(None)?;
        for (path, expected) in &self.omitted_entries {
            check_preparation(&self.cancel)?;
            let current = stat_beneath(&self.root, path)?;
            if serde_json::to_vec(&current).map_err(io::Error::other)?
                != serde_json::to_vec(expected).map_err(io::Error::other)?
            {
                return Err(drift());
            }
        }
        self.verify_omitted_selection()?;
        for file in self.files.values() {
            check_preparation(&self.cancel)?;
            file.verify()?;
        }
        for path in self.directories.keys() {
            check_preparation(&self.cancel)?;
            self.verify_directory(path)?;
        }
        Ok(())
    }
    pub(crate) fn binding_digest(&self) -> io::Result<[u8; 32]> {
        self.verify_all()?;
        Ok(self.binding_digest)
    }

    pub(crate) fn tree(&self) -> io::Result<Option<InteractiveSourceTree>> {
        self.verify_all()?;
        Ok(self.summary.clone())
    }
    pub(crate) fn children(&self, path: &Path) -> io::Result<Option<Vec<ProofListEntry>>> {
        self.verify_omitted_selection()?;
        self.directories
            .contains_key(path)
            .then(|| {
                self.verify_directory(path)
                    .map(|directory| directory.children.clone())
            })
            .transpose()
    }
    pub(crate) fn stat(&self, path: &Path) -> io::Result<Entry> {
        self.verify_omitted_selection()?;
        self.verify_anchors(Some(path))?;
        if let Some(file) = self.files.get(path) {
            file.verify()?;
        } else if self.directories.contains_key(path) {
            self.verify_directory(path)?;
        } else if let Some(expected) = self.omitted_entries.get(path) {
            let current = stat_beneath(&self.root, path)?;
            if serde_json::to_vec(&current).map_err(io::Error::other)?
                != serde_json::to_vec(expected).map_err(io::Error::other)?
            {
                return Err(drift());
            }
        } else if !self.anchors.contains_key(path) {
            return Err(drift());
        }
        stat_beneath(&self.root, path)
    }

    pub(crate) fn verify_transfer(&self, path: &Path, hash: &blake3::Hash) -> io::Result<()> {
        self.verify_omitted_selection()?;
        self.verify_anchors(Some(path))?;
        let bound = self.files.get(path).ok_or_else(drift)?;
        bound.verify()?;
        if self
            .completed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            != Some(hash)
        {
            return Err(drift());
        }
        if self
            .expected_hashes
            .as_ref()
            .is_some_and(|expected| expected.get(path) != Some(hash.as_bytes()))
        {
            return Err(drift());
        }
        Ok(())
    }

    pub(crate) fn expect_content_hashes(
        &mut self,
        expected: &BTreeMap<PathBuf, [u8; 32]>,
    ) -> io::Result<()> {
        if self.files.keys().ne(expected.keys()) {
            return Err(drift());
        }
        self.verify_all()?;
        self.expected_hashes = Some(expected.clone());
        Ok(())
    }

    pub(crate) fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        self.verify_omitted_selection()?;
        check_preparation(&self.cancel)?;
        self.verify_anchors(Some(path))?;
        let bound = self.files.get(path).ok_or_else(drift)?.clone();
        bound.verify()?;
        let file = open_read_beneath(&self.root, path)?;
        if FileVersion::of(&file.metadata()?) != bound.version {
            return Err(drift());
        }
        Ok(Box::new(VerifiedReader {
            file,
            completed: self.completed.clone(),
            bound,
            offset: 0,
            hash: blake3::Hasher::new(),
        }))
    }
}

struct VerifiedReader {
    file: File,
    completed: Arc<Mutex<BTreeMap<PathBuf, blake3::Hash>>>,
    bound: Arc<BoundFile>,
    offset: u64,
    hash: blake3::Hasher,
}
impl Read for VerifiedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        check_preparation(&self.bound.cancel)?;
        // ctime und Nanosekunden erkennen auch eine absichtlich zurückgesetzte mtime.
        self.bound.verify()?;
        if FileVersion::of(&self.file.metadata()?) != self.bound.version {
            return Err(drift());
        }
        let count = self.file.read_at(buffer, self.offset)?;
        self.bound.verify()?;
        if FileVersion::of(&self.file.metadata()?) != self.bound.version {
            return Err(drift());
        }
        self.offset = self.offset.checked_add(count as u64).ok_or_else(drift)?;
        if self.offset > self.bound.version.size {
            return Err(drift());
        }
        self.hash.update(&buffer[..count]);
        if count == 0 {
            if self.offset != self.bound.version.size {
                return Err(drift());
            }
            self.completed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(self.bound.path.clone(), self.hash.finalize());
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root() -> (tempfile::TempDir, RootHandle) {
        let tmp = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(tmp.path()).unwrap();
        let root = RootHandle::open_nofollow(&path).unwrap();
        (tmp, root)
    }
    #[test]
    fn upload_namensgruppen_lassen_alle_case_und_unicode_varianten_aus() {
        let (_tmp, root) = root();
        std::fs::write(root.path().join("Vorlage"), b"abc").unwrap();
        let template = list_upload_beneath(&root, root.path(), &Cancel::new())
            .unwrap()
            .remove(0);
        let mut observed = ["Bericht", "bericht", "é", "e\u{301}", "Unabhaengig"]
            .into_iter()
            .map(|name| {
                let mut child = template.clone();
                child.entry.name = name.into();
                child.raw_name = name.as_bytes().to_vec();
                // Ein ausgelassener Ordner steht für seinen gesamten Teilbaum.
                if name == "Bericht" {
                    child.entry.is_file = false;
                    child.entry.is_dir = true;
                }
                child
            })
            .collect::<Vec<_>>();
        for _ in 0..observed.len() {
            let (children, omitted, has_name_groups) = upload_children(root.path(), &observed);
            assert!(has_name_groups);
            assert_eq!(children.len(), 1);
            assert_eq!(children[0].entry.name, "Unabhaengig");
            assert_eq!(omitted.len(), 4);
            for name in ["Bericht", "bericht", "é", "e\u{301}"] {
                assert!(omitted
                    .iter()
                    .any(|item| item.path == root.path().join(name).to_str().unwrap()
                        && item.reason.contains("gesamte Namensgruppe")));
            }
            observed.rotate_left(1);
        }
    }

    #[test]
    fn lokale_uploadauswahl_bindet_grosse_bestaende_ohne_vorhash_oder_handlemasse() {
        let (_tmp, root) = root();
        let tree = root.path().join("Baum");
        std::fs::create_dir(&tree).unwrap();
        for n in 0..1100 {
            std::fs::write(tree.join(n.to_string()), b"x").unwrap();
        }
        let large = tree.join("Gross");
        File::create(&large)
            .unwrap()
            .set_len(20 * 1024 * 1024 * 1024)
            .unwrap();
        let mut deepest = tree.clone();
        for _ in 0..20 {
            deepest.push("Tief");
            std::fs::create_dir(&deepest).unwrap();
        }
        let bound = VerifiedLocalFiles::bind(&root, &[tree], &Cancel::new()).unwrap();
        let summary = bound.tree().unwrap().unwrap();
        assert_eq!(summary.files, 1101);
        assert_eq!(summary.bytes, 20 * 1024 * 1024 * 1024 + 1100);
        assert!(bound.completed.lock().unwrap().is_empty());
        let mut reader = bound.open(&large).unwrap();
        assert_eq!(reader.read(&mut [0; 32]).unwrap(), 32);
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(VerifiedLocalFiles::bind(&root, &[large], &cancel).is_err());
    }

    #[test]
    fn lokaler_stream_bindet_hash_und_erkennt_aenderung_waehrend_transfer() {
        let (_tmp, root) = root();
        let path = root.path().join("Datei");
        std::fs::write(&path, b"abc").unwrap();
        let bound =
            VerifiedLocalFiles::bind(&root, std::slice::from_ref(&path), &Cancel::new()).unwrap();
        assert!(bound.verify_transfer(&path, &blake3::hash(b"abc")).is_err());
        let mut content = Vec::new();
        bound
            .open(&path)
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        bound
            .verify_transfer(&path, &blake3::hash(&content))
            .unwrap();
        assert!(bound.verify_transfer(&path, &blake3::hash(b"xyz")).is_err());
        let mut reader = bound.open(&path).unwrap();
        assert_eq!(reader.read(&mut [0; 1]).unwrap(), 1);
        std::fs::write(&path, b"xyz").unwrap();
        assert!(reader.read(&mut [0; 1]).is_err());
    }

    #[test]
    fn lokale_uploadquelle_bindet_eltern_dateiversion_inhalt_und_vollen_ordnerbestand() {
        let (_tmp, root) = root();
        let tree = root.path().join("Baum");
        std::fs::create_dir(&tree).unwrap();
        let path = tree.join("Datei");
        std::fs::write(&path, b"vorher").unwrap();
        let bound =
            VerifiedLocalFiles::bind(&root, std::slice::from_ref(&tree), &Cancel::new()).unwrap();
        let before = bound.tree().unwrap().unwrap();
        std::fs::write(&path, b"nachher").unwrap();
        assert!(bound.open(&path).is_err());
        let fresh =
            VerifiedLocalFiles::bind(&root, std::slice::from_ref(&tree), &Cancel::new()).unwrap();
        assert_ne!(before.digest, fresh.tree().unwrap().unwrap().digest);
        std::fs::write(tree.join("Neu"), b"").unwrap();
        assert!(fresh.children(&tree).is_err());
        let fresh =
            VerifiedLocalFiles::bind(&root, std::slice::from_ref(&tree), &Cancel::new()).unwrap();
        std::fs::rename(&tree, root.path().join("Alt")).unwrap();
        std::fs::create_dir(&tree).unwrap();
        std::fs::rename(root.path().join("Alt/Datei"), &path).unwrap();
        assert!(fresh.open(&path).is_err());
    }
    #[test]
    fn lokale_uploadquelle_bilanziert_symlinks_spezialdateien_und_rohnamen() {
        #[cfg(target_os = "linux")]
        use std::os::unix::ffi::OsStringExt;
        let (_tmp, root) = root();
        let tree = root.path().join("Baum");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("Inhalt"), b"abc").unwrap();
        std::os::unix::fs::symlink("Inhalt", tree.join("Link")).unwrap();
        use std::os::unix::ffi::OsStrExt;
        let fifo = std::ffi::CString::new(tree.join("FIFO").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        std::fs::write(tree.join("Zeile\nUmbruch"), b"auslassen").unwrap();
        #[cfg(target_os = "linux")]
        std::fs::write(
            tree.join(std::ffi::OsString::from_vec(vec![0xff])),
            b"auslassen",
        )
        .unwrap();
        let bound =
            VerifiedLocalFiles::bind(&root, std::slice::from_ref(&tree), &Cancel::new()).unwrap();
        let summary = bound.tree().unwrap().unwrap();
        assert_eq!(
            (
                summary.entries,
                summary.files,
                summary.directories,
                summary.bytes,
                summary.omitted.len()
            ),
            (
                5 + u64::from(cfg!(target_os = "linux")),
                1,
                1,
                3,
                3 + usize::from(cfg!(target_os = "linux"))
            )
        );
        assert_eq!(bound.children(&tree).unwrap().unwrap().len(), 1);
        #[cfg(target_os = "linux")]
        assert!(summary
            .omitted
            .iter()
            .any(|item| item.path.ends_with("%FF")));
        assert!(summary
            .omitted
            .iter()
            .any(|item| item.reason.contains("Rohbytes")));
    }
}
