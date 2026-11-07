//! Native Kopierattribute über gehaltene Objekte; keine Handles in Plänen oder IPC.
use super::*;
use crate::{vfs::FileCopyMetadataReport, Cancel};
use std::{cell::Cell, collections::HashSet};

pub struct Source {
    fd: OwnedFd,
    root: Option<RootHandle>,
    path: PathBuf,
    before: Stat,
    created: FileCreationTime,
    attributes: Vec<XattrValue>,
    resource: Option<Option<BlobSummary>>,
    limitations: Vec<String>,
}
pub struct Target {
    fd: OwnedFd,
    root: Option<RootHandle>,
    path: PathBuf,
}
fn cancelled(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        return Err(crate::coded_error(
            io::ErrorKind::Interrupted,
            crate::ErrorCode::Interrupted,
            "Kopiermetadaten abgebrochen",
        ));
    }
    Ok(())
}
fn same_version(before: &Stat, after: &Stat) -> bool {
    before.st_dev == after.st_dev
        && before.st_ino == after.st_ino
        && before.st_mode == after.st_mode
        && before.st_uid == after.st_uid
        && before.st_gid == after.st_gid
        && before.st_size == after.st_size
        && before.st_mtime == after.st_mtime
        && before.st_mtime_nsec == after.st_mtime_nsec
        && before.st_ctime == after.st_ctime
        && before.st_ctime_nsec == after.st_ctime_nsec
        && before.st_flags == after.st_flags
}
fn label(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}
fn attributes(fd: &OwnedFd, cancel: &Cancel) -> io::Result<(Vec<XattrValue>, Vec<String>)> {
    let mut values = Vec::new();
    let mut notes = Vec::new();
    let mut bytes = 0usize;
    for name in list_xattr_names(fd)? {
        cancelled(cancel)?;
        if is_resource_fork(&name) || is_acl_xattr(&name) {
            continue;
        }
        match read_xattr_value(fd, &name) {
            Ok(value) if bytes.saturating_add(value.len()) <= 32 * 1024 * 1024 => {
                bytes += value.len();
                values.push(XattrValue { name, value });
            }
            Ok(_) => {
                notes.push("Weitere xattrs überschreiten das Kopierbudget; ausgelassen".into())
            }
            Err(error) => notes.push(format!(
                "xattr {} nicht lesbar: {error}",
                String::from_utf8_lossy(&name)
            )),
        }
    }
    Ok((values, notes))
}
fn resource_summary(fd: &OwnedFd, cancel: &Cancel) -> io::Result<Option<BlobSummary>> {
    cancelled(cancel)?;
    let mut empty = [0_u8; 0];
    let size = match fgetxattr(fd, OsStr::new("com.apple.ResourceFork"), &mut empty) {
        Ok(size) => size as u64,
        Err(error) if error.raw_os_error() == libc::ENOATTR => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut reader: Box<dyn Read> = if FileType::from_raw_mode(fstat(fd)?.st_mode).is_file() {
        // Der Kernel löst den benannten Fork direkt relativ zum regulären Datei-FD
        // auf. Es wird kein Elternpfad und kein /dev/fd-Symlink erneut geöffnet.
        Box::new(File::from(openat(
            fd,
            "..namedfork/rsrc",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?))
    } else {
        Box::new(mac_resource_fork::Reader::open(dup(fd)?)?)
    };
    let mut hash = blake3::Hasher::new();
    let mut bytes = 0u64;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        cancelled(cancel)?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    if bytes != size {
        return Err(crate::real_mode::plan_drift(
            "Ressourcen-Fork änderte während des Lesens seine Länge",
        ));
    }
    Ok(Some(BlobSummary {
        size,
        digest: *hash.finalize().as_bytes(),
    }))
}
impl Source {
    pub(crate) fn open(
        root: Option<&RootHandle>,
        path: &Path,
        expected: &Entry,
        cancel: &Cancel,
    ) -> io::Result<Self> {
        cancelled(cancel)?;
        let fd = open_entry(root, path)?;
        let before = check_visible(root, path, &fd)?;
        if !crate::batch_rename::same_source(
            expected,
            &entry_from_stat(expected.name.clone(), &before, None),
        ) {
            return Err(crate::real_mode::plan_drift(
                "Quelle vor den Kopiermetadaten verändert",
            ));
        }
        let created = value(&before)?;
        let (attributes, mut limitations) = match attributes(&fd, cancel) {
            Ok(value) => value,
            Err(error) if crate::error_code(&error) == crate::ErrorCode::Interrupted => {
                return Err(error)
            }
            Err(error) => (
                Vec::new(),
                vec![format!("Quell-xattrs nicht lesbar: {error}")],
            ),
        };
        let resource = match resource_summary(&fd, cancel) {
            Ok(value) => Some(value),
            Err(error) if crate::error_code(&error) == crate::ErrorCode::Interrupted => {
                return Err(error)
            }
            Err(error) => {
                limitations.push(format!("Quell-Ressourcen-Fork nicht prüfbar: {error}"));
                None
            }
        };
        if matches!(&resource, Some(Some(summary)) if summary.size > u32::MAX as u64) {
            limitations.push("Ressourcen-Fork über 4 GiB vom nativen Kopierweg ausgelassen".into());
        }
        let after = check_visible(root, path, &fd)?;
        if !same_version(&before, &after) {
            return Err(crate::real_mode::plan_drift(
                "Quellmetadaten änderten sich beim Erfassen",
            ));
        }
        Ok(Self {
            fd,
            root: root.cloned(),
            path: path.to_path_buf(),
            before,
            created,
            attributes,
            resource,
            limitations,
        })
    }
    pub(crate) fn resource_digest(&self) -> Option<[u8; 32]> {
        let mut hash = blake3::Hasher::new();
        match &self.resource {
            None => return None,
            Some(Some(summary)) if summary.size > 0 => {
                hash.update(&summary.size.to_le_bytes());
                hash.update(&summary.digest);
            }
            _ => {
                hash.update(&0u64.to_le_bytes());
            }
        }
        Some(*hash.finalize().as_bytes())
    }
    pub(crate) fn version_digest(&self) -> [u8; 32] {
        // Nur Werte, keine FD-/Pfadpersistenz. Auch ein erst nach dem Kopieren
        // geänderter Fork oder Tag muss den Quellabbau eines Moves stoppen.
        let mut hash = blake3::Hasher::new();
        for bytes in [
            self.before.st_dev.to_le_bytes().as_slice(),
            self.before.st_ino.to_le_bytes().as_slice(),
            self.before.st_mode.to_le_bytes().as_slice(),
            self.before.st_uid.to_le_bytes().as_slice(),
            self.before.st_gid.to_le_bytes().as_slice(),
            self.before.st_size.to_le_bytes().as_slice(),
            self.before.st_mtime.to_le_bytes().as_slice(),
            self.before.st_mtime_nsec.to_le_bytes().as_slice(),
            self.before.st_ctime.to_le_bytes().as_slice(),
            self.before.st_ctime_nsec.to_le_bytes().as_slice(),
            self.before.st_flags.to_le_bytes().as_slice(),
        ] {
            hash.update(bytes);
        }
        for attribute in &self.attributes {
            hash.update(&(attribute.name.len() as u64).to_le_bytes());
            hash.update(&attribute.name);
            hash.update(&(attribute.value.len() as u64).to_le_bytes());
            hash.update(&attribute.value);
        }
        if let Some(resource) = self.resource_digest() {
            hash.update(&resource);
        }
        *hash.finalize().as_bytes()
    }
    pub(crate) fn creation_time(&self) -> Option<FileCreationTime> {
        Some(self.created)
    }
    pub(crate) fn has_locked_flag(&self) -> bool {
        self.before.st_flags & libc::UF_IMMUTABLE != 0
    }
    pub(crate) fn needs_copy(&self) -> bool {
        !self.attributes.is_empty()
            || !matches!(
                self.resource,
                Some(None) | Some(Some(BlobSummary { size: 0, .. }))
            )
            || !self.limitations.is_empty()
            || self.before.st_flags & libc::UF_HIDDEN != 0
            || self.has_locked_flag()
    }
    fn resource_copy_supported(&self) -> bool {
        matches!(&self.resource, Some(None))
            || matches!(&self.resource, Some(Some(summary)) if summary.size <= u32::MAX as u64)
    }
    pub(crate) fn resource_needed(&self) -> bool {
        !matches!(
            self.resource,
            Some(None) | Some(Some(BlobSummary { size: 0, .. }))
        )
    }
    pub(crate) fn verify(&self, cancel: &Cancel) -> io::Result<()> {
        cancelled(cancel)?;
        if !same_version(
            &self.before,
            &check_visible(self.root.as_ref(), &self.path, &self.fd)?,
        ) {
            return Err(crate::real_mode::plan_drift(
                "Quelle während der Metadatenkopie verändert",
            ));
        }
        Ok(())
    }
    pub(crate) fn omitted_flags_report(&self) -> FileCopyMetadataReport {
        let limitations = if self.has_locked_flag() {
            vec![format!(
                "Dateisperre wird am Ziel nicht übernommen: {}",
                label(&self.path)
            )]
        } else {
            Vec::new()
        };
        FileCopyMetadataReport {
            limitations,
            resource_fork_incomplete: false,
        }
    }
    pub(crate) fn omitted_report(&self) -> FileCopyMetadataReport {
        let mut limitations = self.limitations.clone();
        if !self.attributes.is_empty() {
            limitations
                .push("xattrs einschließlich Finder-Tags werden am Ziel nicht übernommen".into());
        }
        if self.before.st_flags & libc::UF_HIDDEN != 0 {
            limitations.push(format!(
                "Verstecktmarkierung wird am Ziel nicht übernommen: {}",
                label(&self.path)
            ));
        }
        limitations.extend(self.omitted_flags_report().limitations);
        if self.resource_needed() {
            limitations.push(format!(
                "Ressourcen-Fork am Ziel nicht vollständig bestätigt: {}",
                label(&self.path)
            ));
        }
        FileCopyMetadataReport {
            limitations,
            resource_fork_incomplete: self.resource_needed(),
        }
    }
}
impl Target {
    pub(crate) fn from_file(
        root: Option<&RootHandle>,
        path: &Path,
        file: &File,
    ) -> io::Result<Self> {
        let fd = dup(file)?;
        check_visible(root, path, &fd)?;
        Ok(Self {
            fd,
            root: root.cloned(),
            path: path.to_path_buf(),
        })
    }
    pub(crate) fn from_directory(root: Option<&RootHandle>, path: &Path) -> io::Result<Self> {
        let fd = open_entry(root, path)?;
        check_visible(root, path, &fd)?;
        Ok(Self {
            fd,
            root: root.cloned(),
            path: path.to_path_buf(),
        })
    }
    pub(crate) fn finish_flags(
        &self,
        final_root: Option<&RootHandle>,
        source: &Source,
        final_path: &Path,
    ) -> io::Result<FileCopyMetadataReport> {
        if !source.has_locked_flag() {
            return Ok(FileCopyMetadataReport::default());
        }
        // Eine gesperrte Tempdatei ist nicht umbenennbar. Dieser kurze Abschluss
        // folgt dem sichtbaren Commit und benutzt weiterhin dessen eigenen FD.
        if let Some(root) = &self.root {
            if !root.is_current()? {
                return Err(crate::real_mode::plan_drift(
                    "Ursprünglicher Kopierroot vor dem Flagabschluss verändert",
                ));
            }
            root.current_mount_identity()?;
        }
        let before = check_visible(final_root, final_path, &self.fd)?;
        let desired = before.st_flags | libc::UF_IMMUTABLE;
        if desired != before.st_flags
            && unsafe { libc::fchflags(self.fd.as_raw_fd(), desired) } != 0
        {
            let error = io::Error::last_os_error();
            if matches!(
                error.raw_os_error(),
                Some(libc::EPERM | libc::EACCES | libc::ENOTSUP | libc::EINVAL | libc::EROFS)
            ) {
                return Ok(FileCopyMetadataReport {
                    limitations: vec![format!("Dateisperre am Ziel nicht übernommen: {error}")],
                    resource_fork_incomplete: false,
                });
            }
            return Err(error);
        }
        File::from(dup(&self.fd)?).sync_all()?;
        let after = check_visible(final_root, final_path, &self.fd)?;
        if after.st_dev != before.st_dev
            || after.st_ino != before.st_ino
            || after.st_flags != desired
        {
            return Err(crate::real_mode::plan_drift(
                "Dateisperre oder Zielidentität nach dem Kopieren abweichend",
            ));
        }
        Ok(FileCopyMetadataReport::default())
    }
    pub(crate) fn apply(
        &self,
        source: &Source,
        cancel: &Cancel,
    ) -> io::Result<FileCopyMetadataReport> {
        source.verify(cancel)?;
        check_visible(self.root.as_ref(), &self.path, &self.fd)?;
        let context = CopyContext {
            source,
            cancel,
            resource_unavailable: Cell::new(false),
        };
        let state = CopyState::new(&context)?;
        let result = unsafe {
            libc::fcopyfile(
                source.fd.as_raw_fd(),
                self.fd.as_raw_fd(),
                state.0,
                libc::COPYFILE_XATTR,
            )
        };
        let error = (result != 0).then(io::Error::last_os_error);
        // copyfile hält interne Fork-Deskriptoren bis zum Freigeben des Zustands.
        // Erst deren Abschluss macht den neuen Resource Fork für den Readback sichtbar.
        drop(state);
        let unavailable = context.resource_unavailable.get()
            || error.as_ref().is_some_and(|error| {
                matches!(
                    error.kind(),
                    io::ErrorKind::Unsupported | io::ErrorKind::PermissionDenied
                )
            });
        cancelled(cancel)?;
        source.verify(cancel)?;
        let mut limitations = source.limitations.clone();
        if let Some(error) = error {
            limitations.push(format!(
                "Native Attributkopie meldete einen Fehler: {error}"
            ));
        }
        let current = check_visible(self.root.as_ref(), &self.path, &self.fd)?;
        let desired_flags =
            (current.st_flags & !libc::UF_HIDDEN) | (source.before.st_flags & libc::UF_HIDDEN);
        if desired_flags != current.st_flags
            && unsafe { libc::fchflags(self.fd.as_raw_fd(), desired_flags) } != 0
        {
            limitations.push(format!(
                "Verstecktmarkierung nicht übernommen: {}",
                io::Error::last_os_error()
            ));
        }
        let (actual, notes) = match attributes(&self.fd, cancel) {
            Ok(value) => value,
            Err(error) => (
                Vec::new(),
                vec![format!("Ziel-xattrs nicht prüfbar: {error}")],
            ),
        };
        limitations.extend(notes);
        let expected = source
            .attributes
            .iter()
            .map(|attr| (attr.name.as_slice(), attr.value.as_slice()))
            .collect::<HashSet<_>>();
        let actual = actual
            .iter()
            .map(|attr| (attr.name.as_slice(), attr.value.as_slice()))
            .collect::<HashSet<_>>();
        if !expected.is_subset(&actual) {
            limitations.push(format!(
                "Einzelne xattrs am Ziel ausgelassen oder angepasst: {}",
                label(&source.path)
            ));
        }
        let resource = match resource_summary(&self.fd, cancel) {
            Ok(value) => Some(value),
            Err(error) => {
                limitations.push(format!("Ziel-Ressourcen-Fork nicht prüfbar: {error}"));
                None
            }
        };
        let resource_matches = match (&source.resource, &resource) {
            (Some(Some(expected)), Some(Some(actual))) => expected == actual,
            (Some(Some(expected)), Some(None)) => expected.size == 0,
            (Some(None), Some(None)) => true,
            _ => false,
        };
        if !resource_matches
            && !unavailable
            && source.resource_copy_supported()
            && matches!((&source.resource, &resource), (Some(Some(expected)), Some(_)) if expected.size > 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Ressourcen-Fork stimmt am Ziel nicht mit der gebundenen Quelle überein",
            ));
        }
        if !resource_matches && source.resource_needed() {
            limitations.push(format!(
                "Ressourcen-Fork am Ziel nicht vollständig übernommen: {}",
                label(&source.path)
            ));
        }
        let final_stat = check_visible(self.root.as_ref(), &self.path, &self.fd)?;
        if final_stat.st_flags & libc::UF_HIDDEN != source.before.st_flags & libc::UF_HIDDEN {
            limitations.push(format!(
                "Verstecktmarkierung am Ziel abweichend: {}",
                label(&source.path)
            ));
        }
        source.verify(cancel)?;
        Ok(FileCopyMetadataReport {
            limitations,
            resource_fork_incomplete: !resource_matches && source.resource_needed(),
        })
    }
}
struct CopyContext<'a> {
    source: &'a Source,
    cancel: &'a Cancel,
    resource_unavailable: Cell<bool>,
}
extern "C" fn callback(
    what: libc::c_int,
    stage: libc::c_int,
    state: libc::copyfile_state_t,
    _source: *const libc::c_char,
    _target: *const libc::c_char,
    context: *mut libc::c_void,
) -> libc::c_int {
    // copyfile ruft synchron auf; Kontext und Quellen bleiben bis zur Rückkehr
    // erhalten. Der Callback allokiert nicht und lässt keine Rust-Panik über FFI.
    let Some(context) = (unsafe { (context as *const CopyContext<'_>).as_ref() }) else {
        return libc::COPYFILE_QUIT;
    };
    if context.cancel.is_cancelled() {
        return libc::COPYFILE_QUIT;
    }
    if what == libc::COPYFILE_COPY_XATTR
        && matches!(stage, libc::COPYFILE_START | libc::COPYFILE_ERR)
    {
        let error = unsafe { *libc::__error() };
        let mut name: *const libc::c_char = std::ptr::null();
        if unsafe {
            libc::copyfile_state_get(
                state,
                libc::COPYFILE_STATE_XATTRNAME as u32,
                (&mut name as *mut *const libc::c_char).cast(),
            )
        } != 0
            || name.is_null()
        {
            return libc::COPYFILE_SKIP;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes();
        if stage == libc::COPYFILE_ERR {
            if is_resource_fork(name) && matches!(error, libc::ENOTSUP | libc::EPERM | libc::EACCES)
            {
                context.resource_unavailable.set(true);
            }
            return libc::COPYFILE_CONTINUE;
        }
        if is_resource_fork(name) {
            // Der native fcopyfile-Fallback adressiert Fork-Offsets als u32.
            // Größere Forks bleiben sichtbar ausgelassen statt bei 4 GiB umzubrechen.
            return if context.source.resource_copy_supported() {
                libc::COPYFILE_CONTINUE
            } else {
                libc::COPYFILE_SKIP
            };
        }
        if !context
            .source
            .attributes
            .iter()
            .any(|attr| attr.name == name)
        {
            return libc::COPYFILE_SKIP;
        }
    }
    libc::COPYFILE_CONTINUE
}
struct CopyState<'a>(
    libc::copyfile_state_t,
    std::marker::PhantomData<&'a CopyContext<'a>>,
);
impl<'a> CopyState<'a> {
    fn new(context: &'a CopyContext<'a>) -> io::Result<Self> {
        let state = Self(
            unsafe { libc::copyfile_state_alloc() },
            std::marker::PhantomData,
        );
        if state.0.is_null() {
            return Err(io::Error::from(io::ErrorKind::OutOfMemory));
        }
        for (key, pointer) in [
            (
                libc::COPYFILE_STATE_STATUS_CB,
                callback as *const libc::c_void,
            ),
            (
                libc::COPYFILE_STATE_STATUS_CTX,
                (context as *const CopyContext<'_>).cast(),
            ),
        ] {
            if unsafe { libc::copyfile_state_set(state.0, key as u32, pointer) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(state)
    }
}
impl Drop for CopyState<'_> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                libc::copyfile_state_free(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    struct Unlock(OwnedFd);
    impl Drop for Unlock {
        fn drop(&mut self) {
            unsafe {
                libc::fchflags(self.0.as_raw_fd(), 0);
            }
        }
    }
    #[test]
    fn locked_copy_completion_uses_final_root_and_held_inode_after_private_stage_rename() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let stage = root.join("stage");
        std::fs::create_dir(&stage).unwrap();
        let source = root.join("source");
        std::fs::write(&source, b"source").unwrap();
        let held = RootHandle::open_nofollow(&root).unwrap();
        let staged = RootHandle::open_nofollow(&stage).unwrap();
        let source_fd = open_entry(Some(&held), &source).unwrap();
        let _source_cleanup = Unlock(dup(&source_fd).unwrap());
        assert_eq!(
            unsafe { libc::fchflags(source_fd.as_raw_fd(), libc::UF_IMMUTABLE) },
            0
        );
        let snapshot = Source::open(
            Some(&held),
            &source,
            &stat_beneath(&held, &source).unwrap(),
            &Cancel::new(),
        )
        .unwrap();
        for mode in ["success", "swap", "stage-root-swap"] {
            let temp = stage.join(mode);
            let final_path = root.join(mode);
            let file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp)
                .unwrap();
            let target = Target::from_file(Some(&staged), &temp, &file).unwrap();
            let _target_cleanup = Unlock(dup(&target.fd).unwrap());
            std::fs::rename(&temp, &final_path).unwrap();
            if mode == "swap" {
                std::fs::rename(&final_path, root.join("displaced")).unwrap();
                std::fs::write(&final_path, b"foreign replacement").unwrap();
                assert!(target
                    .finish_flags(Some(&held), &snapshot, &final_path)
                    .is_err());
                assert_eq!(fstat(&target.fd).unwrap().st_flags & libc::UF_IMMUTABLE, 0);
                assert_eq!(
                    fstat(open_entry(Some(&held), &final_path).unwrap())
                        .unwrap()
                        .st_flags
                        & libc::UF_IMMUTABLE,
                    0
                );
                assert_eq!(std::fs::read(final_path).unwrap(), b"foreign replacement");
            } else if mode == "stage-root-swap" {
                std::fs::rename(&stage, root.join("stage-old")).unwrap();
                std::fs::create_dir(&stage).unwrap();
                assert!(target
                    .finish_flags(Some(&held), &snapshot, &final_path)
                    .is_err());
                assert_eq!(fstat(&target.fd).unwrap().st_flags & libc::UF_IMMUTABLE, 0);
                std::fs::remove_dir(&stage).unwrap();
                std::fs::rename(root.join("stage-old"), &stage).unwrap();
            } else {
                let report = target
                    .finish_flags(Some(&held), &snapshot, &final_path)
                    .unwrap();
                assert!(report.limitations.is_empty());
                assert_eq!(
                    fstat(&target.fd).unwrap().st_flags & libc::UF_IMMUTABLE,
                    libc::UF_IMMUTABLE
                );
                assert_eq!(
                    fstat(open_entry(Some(&held), &final_path).unwrap())
                        .unwrap()
                        .st_flags
                        & libc::UF_IMMUTABLE,
                    libc::UF_IMMUTABLE
                );
            }
        }
    }
    use super::*;
    use crate::{
        fileops::{copy_items, FileOperationRequest, OverwritePolicy},
        LocalVfs, Vfs,
    };
    use std::fs::{self, OpenOptions};
    use std::os::darwin::fs::MetadataExt;

    fn fixture() -> (tempfile::TempDir, PathBuf, LocalVfs, LocalVfs, Vec<u8>) {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("source/folder")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        let file = root.join("source/folder/file");
        fs::write(&file, b"ordinary data fork").unwrap();
        let mut finder = vec![0_u8; 32];
        finder[9] = 12;
        finder[16..20].copy_from_slice(b"TEST");
        for path in [&file, &root.join("source/folder")] {
            set_xattr_for_test(path, b"com.doppeldecker.keep", b"user attribute").unwrap();
            set_xattr_for_test(path, b"com.apple.FinderInfo", &finder).unwrap();
            let mut tags = Vec::new();
            plist::to_writer_binary(&mut tags, &vec!["Projekt\n4"]).unwrap();
            set_xattr_for_test(path, b"com.apple.metadata:_kMDItemUserTags", &tags).unwrap();
        }
        let resource = b"resource-data".repeat(410_000);
        set_xattr_for_test(&file, b"com.apple.ResourceFork", &resource).unwrap();
        let src = LocalVfs::rooted(&root.join("source")).unwrap();
        let dst = LocalVfs::rooted(&root.join("target")).unwrap();
        (area, root, src, dst, resource)
    }
    #[test]
    fn native_copy_preserves_file_directory_tags_finderinfo_custom_attributes_and_streamed_resource(
    ) {
        let (_area, root, src, dst, resource) = fixture();
        let source = root.join("source/folder");
        let target = root.join("target");
        let source_file = source.join("file");
        let fd = open_entry(None, &source_file).unwrap();
        assert_eq!(
            unsafe {
                libc::fchflags(
                    fd.as_raw_fd(),
                    fstat(&fd).unwrap().st_flags | libc::UF_HIDDEN,
                )
            },
            0
        );
        let stats = copy_items(
            FileOperationRequest {
                src: &src,
                dst: &dst,
                items: std::slice::from_ref(&source),
                dst_dir: &target,
                policy: OverwritePolicy::Skip,
                sandbox: &Some(root.clone()),
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(stats.files, 1);
        assert_eq!(stats.incomplete_resource_forks, 0);
        assert!(
            stats.target_write_limitations.is_empty(),
            "{:?}",
            stats.target_write_limitations
        );
        assert_eq!(
            fs::read(target.join("folder/file")).unwrap(),
            b"ordinary data fork"
        );
        assert_eq!(
            fs::read(target.join("folder/file/..namedfork/rsrc")).unwrap(),
            resource
        );
        assert_eq!(fs::read(&source_file).unwrap(), b"ordinary data fork");
        assert_eq!(
            fs::read(source_file.join("..namedfork/rsrc")).unwrap(),
            resource
        );
        for relative in ["folder", "folder/file"] {
            let from = root.join("source").join(relative);
            let to = target.join(relative);
            assert_eq!(
                read_xattrs_from(None, &from).unwrap(),
                read_xattrs_from(None, &to).unwrap()
            );
        }
        assert_ne!(
            fs::metadata(target.join("folder/file")).unwrap().st_flags() & libc::UF_HIDDEN,
            0
        );
    }
    #[test]
    fn native_copy_detects_source_drift_target_swap_and_preexisting_cancel_before_attribute_effect()
    {
        let (_area, root, src, _dst, _resource) = fixture();
        let source_path = root.join("source/folder/file");
        let source = Source::open(
            None,
            &source_path,
            &src.stat(&source_path).unwrap(),
            &Cancel::new(),
        )
        .unwrap();
        let path = root.join("target/new");
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let target = Target::from_file(None, &path, &file).unwrap();
        let initial = read_xattrs_from(None, &path).unwrap();
        let cancelled = Cancel::new();
        cancelled.cancel();
        assert_eq!(
            crate::error_code(&target.apply(&source, &cancelled).unwrap_err()),
            crate::ErrorCode::Interrupted
        );
        assert_eq!(read_xattrs_from(None, &path).unwrap(), initial);
        fs::rename(&path, root.join("target/old")).unwrap();
        fs::write(&path, b"foreign file").unwrap();
        let initial = read_xattrs_from(None, &path).unwrap();
        assert_eq!(
            crate::error_code(&target.apply(&source, &Cancel::new()).unwrap_err()),
            crate::ErrorCode::PlanDrift
        );
        assert_eq!(read_xattrs_from(None, &path).unwrap(), initial);
        assert_eq!(fs::read(&path).unwrap(), b"foreign file");
        let path = root.join("target/fresh");
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let target = Target::from_file(None, &path, &file).unwrap();
        let initial = read_xattrs_from(None, &path).unwrap();
        set_xattr_for_test(&source_path, b"com.doppeldecker.keep", b"changed metadata").unwrap();
        assert_eq!(
            crate::error_code(&target.apply(&source, &Cancel::new()).unwrap_err()),
            crate::ErrorCode::PlanDrift
        );
        assert_eq!(read_xattrs_from(None, &path).unwrap(), initial);
    }
}
