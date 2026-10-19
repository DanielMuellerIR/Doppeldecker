//! Descriptor-relative Quellen und begrenzte Baumbeobachtung für additive Uploads.

use crate::local_fs::{list_upload_beneath, open_read_beneath, stat_beneath, RootHandle};
use crate::vfs::{Entry, InteractiveSourceTree, ProofListEntry, SourceReadOmission};
use crate::{coded_error, Cancel, ErrorCode};
use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) struct VerifiedLocalFiles {
    files: BTreeMap<PathBuf, Arc<BoundFile>>,
    root: RootHandle,
    anchors: BTreeMap<PathBuf, RootHandle>,
    directories: BTreeMap<PathBuf, BoundDirectory>,
    summary: Option<InteractiveSourceTree>,
    omitted_entries: BTreeMap<PathBuf, Entry>,
}

const MAX_HANDLES: usize = 128;
const MAX_ENTRIES: usize = 1000;
const MAX_DEPTH: usize = 16;
const MAX_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[cfg(test)]
type BeforeFileOpen = Box<dyn FnOnce(&Path)>;

#[cfg(test)]
thread_local! {
    static BEFORE_FILE_OPEN: std::cell::RefCell<Option<BeforeFileOpen>> = const { std::cell::RefCell::new(None) };
}

struct BoundDirectory {
    handle: RootHandle,
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

fn limit(message: &str) -> io::Error {
    coded_error(
        io::ErrorKind::InvalidInput,
        ErrorCode::InvalidInput,
        message,
    )
}

fn add_bytes(total: u64, size: u64) -> io::Result<u64> {
    if size > 8 * 1024 * 1024 * 1024 {
        return Err(limit("WebDAV-Uploaddatei überschreitet 8 GiB"));
    }
    let total = total.checked_add(size).ok_or_else(drift)?;
    if total > MAX_BYTES {
        return Err(limit("WebDAV-Auswahl überschreitet 16 GiB"));
    }
    Ok(total)
}

fn check_preparation(deadline: Instant, cancel: &Cancel) -> io::Result<()> {
    if Instant::now() >= deadline || cancel.is_cancelled() {
        return Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::Interrupted,
            "Lokale Uploadvorbereitung abgebrochen oder Frist von 60 Sekunden überschritten",
        ));
    }
    Ok(())
}

struct BoundFile {
    file: File,
    root: RootHandle,
    path: PathBuf,
    version: FileVersion,
    hash: blake3::Hash,
    parents: Vec<RootHandle>,
}

#[derive(PartialEq, Eq, serde::Serialize)]
struct FileVersion {
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
    fn of(meta: &Metadata) -> Self {
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
        for parent in &self.parents {
            if !parent.is_current()? {
                return Err(drift());
            }
        }
        if !self.root.is_current()? || FileVersion::of(&self.file.metadata()?) != self.version {
            return Err(drift());
        }
        let entry = stat_beneath(&self.root, &self.path)?;
        if !entry.is_file
            || entry.is_symlink
            || entry.device != Some(self.version.device)
            || entry.inode != Some(self.version.inode)
            || entry.size != self.version.size
        {
            return Err(drift());
        }
        Ok(())
    }
}

impl VerifiedLocalFiles {
    pub(crate) fn bind(root: &RootHandle, items: &[PathBuf], cancel: &Cancel) -> io::Result<Self> {
        if items.is_empty() || items.len() > 100 {
            return Err(limit(
                "WebDAV-Upload benötigt 1 bis 100 direkte Auswahleinträge",
            ));
        }
        for (index, path) in items.iter().enumerate() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| limit("Uploadauswahl braucht bedienbare UTF-8-Namen"))?;
            if items[..index].iter().any(|other| {
                other
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|other| crate::remote_rename::names_may_collide(other, name))
            }) {
                return Err(limit(
                    "Uploadauswahl enthält am Ziel möglicherweise gleichwertige Namen",
                ));
            }
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut result = Self {
            files: BTreeMap::new(),
            root: root.clone(),
            anchors: BTreeMap::from([(root.path().to_path_buf(), root.clone())]),
            directories: BTreeMap::new(),
            summary: None,
            omitted_entries: BTreeMap::new(),
        };
        let mut total = 0u64;
        let mut count = 0usize;
        let mut omitted = Vec::new();
        let mut pending = items
            .iter()
            .rev()
            .map(|path| (path.clone(), 0usize))
            .collect::<Vec<_>>();
        while let Some((path, depth)) = pending.pop() {
            check_preparation(deadline, cancel)?;
            count += 1;
            if count > MAX_ENTRIES || depth > MAX_DEPTH {
                return Err(limit(
                    "WebDAV-Auswahl überschreitet 1000 Einträge oder 16 Unterordnerebenen",
                ));
            }
            result.bind_parents(&path)?;
            let meta = stat_beneath(root, &path)?;
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
                let handle = result.anchors[&path].clone();
                let version = directory_version(&handle)?;
                let observed = list_upload_beneath(&handle, &path)?;
                if count + pending.len() + observed.len() > MAX_ENTRIES {
                    return Err(limit("WebDAV-Auswahl überschreitet 1000 Einträge"));
                }
                let mut children = Vec::new();
                for child in &observed {
                    let name = &child.entry.name;
                    let usable = child.safely_decoded
                        && crate::directory_create::validate_directory_create_request(&path, name)
                            .is_ok_and(|validated| validated == *name);
                    if !usable {
                        count += 1;
                        let display = child
                            .raw_name
                            .iter()
                            .map(|byte| format!("%{byte:02X}"))
                            .collect::<String>();
                        omitted.push(SourceReadOmission { path: path.join(display).to_string_lossy().into_owned(),
                            reason: "Nicht bedienbarer Name (Rohbytes als Prozent-Hex); Inhalt nicht übertragen".into() });
                        continue;
                    }
                    if children.iter().any(|other: &ProofListEntry| {
                        crate::remote_rename::names_may_collide(&other.entry.name, name)
                    }) {
                        return Err(limit(
                            "Lokaler Uploadbaum enthält am Ziel möglicherweise gleichwertige Namen",
                        ));
                    }
                    if child.entry.is_symlink || (!child.entry.is_file && !child.entry.is_dir) {
                        count += 1;
                        omitted.push(SourceReadOmission {
                            path: path.join(name).to_string_lossy().into_owned(),
                            reason: "Symlink oder Spezialdatei; Inhalt nicht verfolgt".into(),
                        });
                    } else {
                        children.push(child.clone());
                    }
                }
                children.sort_by(|a, b| a.raw_name.cmp(&b.raw_name));
                for child in children.iter().rev() {
                    pending.push((path.join(&child.entry.name), depth + 1));
                }
                result.directories.insert(
                    path,
                    BoundDirectory {
                        handle,
                        version,
                        observed,
                        children,
                    },
                );
                continue;
            }
            result.check_handles(1)?;
            #[cfg(test)]
            BEFORE_FILE_OPEN.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook(&path);
                }
            });
            let file = open_read_beneath(root, &path)?;
            let version = FileVersion::of(&file.metadata()?);
            total = add_bytes(total, version.size)?;
            let mut bound = BoundFile {
                file,
                root: root.clone(),
                path: path.clone(),
                version,
                hash: blake3::hash(b""),
                parents: result
                    .anchors
                    .iter()
                    .filter(|(parent, _)| path.starts_with(parent))
                    .map(|(_, handle)| handle.clone())
                    .collect(),
            };
            bound.verify()?;
            let mut hash = blake3::Hasher::new();
            let mut offset = 0u64;
            let mut buffer = vec![0; crate::atomic_file::COPY_BUFFER_BYTES];
            loop {
                check_preparation(deadline, cancel)?;
                let bytes = bound.file.read_at(&mut buffer, offset)?;
                bound.verify()?;
                if bytes == 0 {
                    break;
                }
                hash.update(&buffer[..bytes]);
                offset = offset.checked_add(bytes as u64).ok_or_else(drift)?;
                if offset > bound.version.size {
                    return Err(drift());
                }
            }
            if offset != bound.version.size {
                return Err(drift());
            }
            bound.hash = hash.finalize();
            if result.files.insert(path, Arc::new(bound)).is_some() {
                return Err(drift());
            }
        }
        result.verify_all()?;
        check_preparation(deadline, cancel)?;
        if !result.directories.is_empty() || !omitted.is_empty() {
            let mut digest = blake3::Hasher::new();
            digest.update(b"doppeldecker-local-upload-tree-v1");
            digest.update(&serde_json::to_vec(items).map_err(io::Error::other)?);
            // Auch Eltern außerhalb der ausgewählten Teilbäume gehören zum
            // bestätigten Umfang. Gleicher Inhalt nach einem Mount-/Elterntausch
            // darf bei einer frischen Queuevorbereitung keinen identischen Beleg liefern.
            for (path, anchor) in &result.anchors {
                digest.update(
                    &serde_json::to_vec(&(
                        path,
                        anchor.identity(),
                        anchor.current_mount_identity()?,
                    ))
                    .map_err(io::Error::other)?,
                );
            }
            for (path, file) in &result.files {
                digest.update(
                    &serde_json::to_vec(&(path, &file.version, file.hash.to_hex().as_str()))
                        .map_err(io::Error::other)?,
                );
            }
            for (path, dir) in &result.directories {
                digest
                    .update(&serde_json::to_vec(&(path, &dir.version)).map_err(io::Error::other)?);
                digest.update(&listing_digest(&dir.observed)?);
            }
            digest.update(&serde_json::to_vec(&omitted).map_err(io::Error::other)?);
            digest.update(&serde_json::to_vec(&result.omitted_entries).map_err(io::Error::other)?);
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
        Ok(result)
    }

    fn check_handles(&self, extra: usize) -> io::Result<()> {
        // Arc-Klone teilen ihre Deskriptoren. Kurzlebige Prüf-/Listingdeskriptoren
        // kommen zu diesen höchstens 128 gehaltenen Quellhandles hinzu.
        if self.files.len() + self.anchors.len() + extra > MAX_HANDLES {
            return Err(limit("WebDAV-Auswahl überschreitet 128 gehaltene Quellhandles (Dateien und Elternordner)"));
        }
        Ok(())
    }
    fn bind_anchor(&mut self, path: &Path) -> io::Result<()> {
        if !self.anchors.contains_key(path) {
            self.check_handles(1)?;
            self.anchors.insert(
                path.to_path_buf(),
                RootHandle::open_beneath(&self.root, path)?,
            );
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
    fn verify_anchors(&self) -> io::Result<()> {
        for (path, handle) in &self.anchors {
            if !handle.is_current()?
                || RootHandle::open_beneath(&self.root, path)?.identity() != handle.identity()
            {
                return Err(drift());
            }
        }
        Ok(())
    }
    fn verify_directory(&self, path: &Path) -> io::Result<&BoundDirectory> {
        self.verify_anchors()?;
        let directory = self.directories.get(path).ok_or_else(drift)?;
        if directory_version(&directory.handle)? != directory.version
            || listing_digest(&list_upload_beneath(&directory.handle, path)?)?
                != listing_digest(&directory.observed)?
        {
            return Err(drift());
        }
        Ok(directory)
    }
    fn verify_all(&self) -> io::Result<()> {
        self.verify_anchors()?;
        for (path, expected) in &self.omitted_entries {
            let current = stat_beneath(&self.root, path)?;
            if serde_json::to_vec(&current).map_err(io::Error::other)?
                != serde_json::to_vec(expected).map_err(io::Error::other)?
            {
                return Err(drift());
            }
        }
        for file in self.files.values() {
            file.verify()?;
        }
        for path in self.directories.keys() {
            self.verify_directory(path)?;
        }
        Ok(())
    }
    pub(crate) fn tree(&self) -> io::Result<Option<InteractiveSourceTree>> {
        self.verify_all()?;
        Ok(self.summary.clone())
    }
    pub(crate) fn children(&self, path: &Path) -> io::Result<Option<Vec<ProofListEntry>>> {
        self.directories
            .contains_key(path)
            .then(|| {
                self.verify_directory(path)
                    .map(|directory| directory.children.clone())
            })
            .transpose()
    }
    pub(crate) fn stat(&self, path: &Path) -> io::Result<Entry> {
        self.verify_anchors()?;
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
        self.verify_anchors()?;
        let bound = self.files.get(path).ok_or_else(drift)?;
        bound.verify()?;
        if bound.hash != *hash {
            return Err(drift());
        }
        Ok(())
    }

    pub(crate) fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        self.verify_anchors()?;
        let bound = self.files.get(path).ok_or_else(drift)?.clone();
        bound.verify()?;
        Ok(Box::new(VerifiedReader {
            bound,
            offset: 0,
            hash: blake3::Hasher::new(),
        }))
    }
}

struct VerifiedReader {
    bound: Arc<BoundFile>,
    offset: u64,
    hash: blake3::Hasher,
}
impl Read for VerifiedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        // ctime und Nanosekunden erkennen auch eine absichtlich zurückgesetzte mtime.
        self.bound.verify()?;
        let count = self.bound.file.read_at(buffer, self.offset)?;
        self.offset = self.offset.checked_add(count as u64).ok_or_else(drift)?;
        if self.offset > self.bound.version.size {
            return Err(drift());
        }
        self.hash.update(&buffer[..count]);
        if count == 0
            && (self.offset != self.bound.version.size || self.hash.finalize() != self.bound.hash)
        {
            return Err(drift());
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
    fn rejected(root: &RootHandle, items: &[PathBuf]) {
        let error = VerifiedLocalFiles::bind(root, items, &Cancel::new())
            .err()
            .expect("Limit fehlt");
        assert_eq!(
            crate::error_code(&error),
            ErrorCode::InvalidInput,
            "{error}"
        );
    }
    #[test]
    fn lokale_uploadauswahl_begrenzt_anzahl_tiefe_handles_und_eintraege() {
        let (_tmp, root) = root();
        rejected(&root, &[]);
        rejected(
            &root,
            &(0..101)
                .map(|n| root.path().join(n.to_string()))
                .collect::<Vec<_>>(),
        );
        let tree = root.path().join("Baum");
        std::fs::create_dir(&tree).unwrap();
        let mut deepest = tree.clone();
        for _ in 0..17 {
            deepest.push("Tief");
            std::fs::create_dir(&deepest).unwrap();
        }
        rejected(&root, std::slice::from_ref(&tree));
        std::fs::remove_dir_all(&tree).unwrap();
        std::fs::create_dir(&tree).unwrap();
        for n in 0..127 {
            std::fs::write(tree.join(n.to_string()), b"").unwrap();
        }
        rejected(&root, std::slice::from_ref(&tree));
        std::fs::remove_dir_all(&tree).unwrap();
        std::fs::create_dir(&tree).unwrap();
        for n in 0..1001 {
            std::os::unix::fs::symlink("fehlend", tree.join(n.to_string())).unwrap();
        }
        rejected(&root, &[tree]);
    }
    #[test]
    fn lokale_uploadauswahl_begrenzt_bytes_und_vorbereitungsdauer() {
        let (_tmp, root) = root();
        let path = root.path().join("Gross");
        let file = File::create(&path).unwrap();
        file.set_len(8 * 1024 * 1024 * 1024 + 1).unwrap();
        rejected(&root, &[path]);
        assert_eq!(
            add_bytes(8 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024).unwrap(),
            MAX_BYTES
        );
        assert!(add_bytes(MAX_BYTES, 1).is_err());
        assert!(add_bytes(0, 8 * 1024 * 1024 * 1024 + 1).is_err());
        assert_eq!(
            crate::error_code(&check_preparation(Instant::now(), &Cancel::new()).unwrap_err()),
            ErrorCode::Interrupted
        );
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(VerifiedLocalFiles::bind(&root, &[root.path().join("Fehlt")], &cancel).is_err());
    }

    #[test]
    fn lokale_uploadgroesse_wird_am_gehaltenen_dateihandle_bilanziert() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                BEFORE_FILE_OPEN.with(|hook| {
                    hook.borrow_mut().take();
                });
            }
        }
        for oversized in [false, true] {
            let (_tmp, root) = root();
            let path = root.path().join("Datei");
            std::fs::write(&path, b"").unwrap();
            let omitted = root.path().join("Auslassen");
            std::os::unix::fs::symlink("Datei", &omitted).unwrap();
            let cancel = Cancel::new();
            let hook_cancel = cancel.clone();
            let changed = path.clone();
            let _reset = Reset;
            BEFORE_FILE_OPEN.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |opening| {
                    assert_eq!(opening, changed);
                    if oversized {
                        File::options()
                            .write(true)
                            .open(&changed)
                            .unwrap()
                            .set_len(8 * 1024 * 1024 * 1024 + 1)
                            .unwrap();
                        // Der Gegenlauf darf auch bei fehlender Größenprüfung keinen
                        // riesigen Sparse-Inhalt hashen; der alte Weg liefert Interrupted.
                        hook_cancel.cancel();
                    } else {
                        std::fs::write(&changed, b"abc").unwrap();
                    }
                }))
            });
            let bound = VerifiedLocalFiles::bind(&root, &[path.clone(), omitted], &cancel);
            if oversized {
                let error = bound
                    .err()
                    .expect("Größe des geöffneten Handles überschreitet das Limit");
                assert_eq!(crate::error_code(&error), ErrorCode::InvalidInput);
                assert_eq!(
                    std::fs::metadata(path).unwrap().len(),
                    8 * 1024 * 1024 * 1024 + 1
                );
            } else {
                let bound = bound.unwrap();
                let summary = bound.tree().unwrap().unwrap();
                let mut content = Vec::new();
                bound
                    .open(&path)
                    .unwrap()
                    .read_to_end(&mut content)
                    .unwrap();
                assert_eq!(content, b"abc");
                assert_eq!(
                    summary.bytes,
                    content.len() as u64,
                    "Baumbeleg zählt einen veralteten Stat statt der gehaltenen Datei"
                );
            }
        }
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
