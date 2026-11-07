//! Gewöhnliche SMB-Aktionen verwenden die echte exklusive Anlage und serverseitige
//! Renamewirkung. Diese private Sicht erteilt keine Sync-/Quarantänefähigkeiten.
use super::*;
#[path = "smb_created_directory_dacl.rs"]
mod created_directory_dacl;
#[path = "smb_directory_dacl.rs"]
mod directory_dacl;
#[path = "smb_file_dacl.rs"]
mod file_dacl;
#[path = "smb_sparse_write.rs"]
mod sparse_write;
#[path = "smb_temp_dacl_flush.rs"]
mod temp_dacl_flush;
#[path = "smb_windows_dacl.rs"]
mod windows_dacl;
use crate::remote_real_mode::InteractiveFileOwner;
use crate::vfs::{FileCommitClass, FileReplaceClass, MutationFailure};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    OnceLock,
};

#[derive(Clone)]
struct OrdinarySmb {
    provider: Arc<SmbVfs>,
    root: PathBuf,
    identity: crate::real_mode::RootIdentity,
    cancel: crate::Cancel,
    owner: Option<Arc<InteractiveFileOwner>>,
    file: Option<Arc<OrdinarySmbFile>>,
    sync_metadata: bool,
    stream_target: bool,
    dacl_support: Arc<Mutex<Option<crate::windows_dacl::WindowsDaclWriteSupport>>>,
    directory_dacl_support: Arc<Mutex<Option<crate::windows_dacl::WindowsDaclWriteSupport>>>,
    file_dacl_support: Arc<Mutex<Option<crate::windows_dacl::WindowsDaclWriteSupport>>>,
    sparse_support: Arc<Mutex<Option<Capability>>>,
    sparse_dacl: Arc<AtomicBool>,
    sparse_metadata_restore: Arc<AtomicBool>,
}
struct OrdinarySmbFile {
    target: PathBuf,
    temp: Mutex<Option<(PathBuf, u64)>>,
    intent: OnceLock<crate::local_staging::RemoteTempIntent>,
    attempted: AtomicBool,
    settled: AtomicBool,
    dacl_attempted: AtomicBool,
    dacl_flush: Mutex<Option<temp_dacl_flush::TempDaclFlush>>,
    sparse: Mutex<Option<Arc<Mutex<sparse_write::State>>>>,
    sparse_commit: Mutex<Option<sparse_write::CommitGuard>>,
}
impl SmbVfs {
    pub(crate) fn ordinary_mutations(
        self: &Arc<Self>,
        root: &Path,
        cancel: &crate::Cancel,
        owner: Option<InteractiveFileOwner>,
    ) -> io::Result<Arc<dyn Vfs>> {
        self.ordinary_context(root, cancel, owner, false, true)
    }
    pub(crate) fn content_sync_mutations(
        self: &Arc<Self>,
        root: &Path,
        cancel: &crate::Cancel,
        owner: Option<InteractiveFileOwner>,
    ) -> io::Result<Arc<dyn Vfs>> {
        self.ordinary_context(root, cancel, owner, true, true)
    }
    pub(crate) fn content_sync_reader(
        self: &Arc<Self>,
        root: &Path,
        cancel: &crate::Cancel,
        owner: Option<InteractiveFileOwner>,
    ) -> io::Result<Arc<dyn Vfs>> {
        self.ordinary_context(root, cancel, owner, true, false)
    }

    fn ordinary_context(
        self: &Arc<Self>,
        root: &Path,
        cancel: &crate::Cancel,
        owner: Option<InteractiveFileOwner>,
        sync_metadata: bool,
        stream_target: bool,
    ) -> io::Result<Arc<dyn Vfs>> {
        let value = OrdinarySmb {
            provider: self.clone(),
            root: PathBuf::from(normalize_virtual_path(root)?),
            identity: self.root_identity(root)?,
            cancel: cancel.clone(),
            owner: owner.map(Arc::new),
            file: None,
            sync_metadata,
            stream_target,
            dacl_support: Arc::new(Mutex::new(None)),
            directory_dacl_support: Arc::new(Mutex::new(None)),
            file_dacl_support: Arc::new(Mutex::new(None)),
            sparse_support: Arc::new(Mutex::new(None)),
            sparse_dacl: Arc::new(AtomicBool::new(false)),
            sparse_metadata_restore: Arc::new(AtomicBool::new(false)),
        };
        value.check(root, false)?;
        Ok(Arc::new(value))
    }
}
impl OrdinarySmb {
    fn check(&self, path: &Path, mutation: bool) -> io::Result<()> {
        if self.cancel.is_cancelled() {
            return Err(crate::coded_error(
                io::ErrorKind::Interrupted,
                crate::ErrorCode::Interrupted,
                "SMB-Dateiaktion abgebrochen",
            ));
        }
        self.check_namespace(path, mutation)
    }
    fn check_namespace(&self, path: &Path, mutation: bool) -> io::Result<()> {
        let normalized = normalize_virtual_path(path)?;
        let path = Path::new(&normalized);
        if !path.starts_with(&self.root) || (mutation && path == self.root) {
            return Err(invalid("SMB-Wirkung liegt außerhalb des gewählten Roots"));
        }
        if let Some(owner) = &self.owner {
            let config = owner
                .staging
                .queue
                .parent()
                .ok_or_else(|| invalid("SMB-Queueordner fehlt"))?;
            let profiles = crate::load_smb_profiles_versioned(&config.join("smb-profiles.json"))?;
            for binding in owner
                .profiles
                .iter()
                .filter(|b| b.protocol == crate::RemoteProfileProtocol::Smb)
            {
                if !profiles.iter().any(|p| {
                    p.profile.id == binding.profile_id
                        && p.revision == binding.revision
                        && p.profile.connection == self.provider.config
                }) {
                    return Err(crate::real_mode::plan_drift(
                        "SMB-Profil änderte sich vor der Wirkung",
                    ));
                }
            }
        }
        if self.provider.root_identity(&self.root)? != self.identity {
            return Err(crate::real_mode::plan_drift(
                "SMB-Root oder Sitzung änderte sich",
            ));
        }
        let parent = if mutation {
            path.parent().unwrap_or(&self.root)
        } else {
            path
        };
        let mut current = self.root.clone();
        for component in parent.strip_prefix(&self.root).unwrap().components() {
            current.push(component.as_os_str());
            let entry = self.provider.stat_unchecked(&current)?;
            if !entry.is_dir || entry.is_symlink {
                return Err(crate::real_mode::plan_drift(
                    "SMB-Elternpfad ist kein echter Ordner",
                ));
            }
        }
        Ok(())
    }
    fn start_intent(&self, path: &Path) -> io::Result<()> {
        if let (Some(file), Some(owner)) = (&self.file, &self.owner) {
            file.intent
                .set(
                    crate::local_staging::RemoteTempIntent::create_bound_with_root(
                        &owner.staging,
                        &file.target,
                        path,
                        &self.identity,
                        self.provider.identity().account_digest,
                    )?,
                )
                .map_err(|_| invalid("SMB-Tempintent ist bereits gebunden"))?;
        }
        Ok(())
    }
    fn bind_created_temp(&self, path: &Path, writer: &mut SmbWrite) -> io::Result<()> {
        if self.file.is_none() {
            return Ok(());
        }
        self.bind_created_temp_identity(path, writer.created_file_identity())
    }
    fn bind_created_temp_identity(&self, path: &Path, inode: io::Result<u64>) -> io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let result = (|| {
            let inode = inode?;
            let current = self.provider.stat(path)?;
            if !current.is_file || current.is_symlink || current.inode != Some(inode) {
                return Err(crate::real_mode::plan_drift(
                    "SMB-Tempdatei wurde nach CREATE ausgetauscht",
                ));
            }
            // Niemals eine nachträglich am Pfad gefundene fremde ID legitimieren.
            *lock(&file.temp) = Some((path.to_path_buf(), inode));
            Ok(())
        })();
        result.map_err(|error| {
            let mut paths = vec![path];
            if let Some(intent) = file.intent.get() {
                paths.push(intent.path());
            }
            crate::error::with_recovery_context(
                crate::coded_error(
                    error.kind(),
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "SMB-Datei wurde angelegt, ihre Tempbesitzbindung blieb unklar: {error}"
                    ),
                ),
                &[&error],
                &paths,
            )
        })
    }
    fn remove(&self, path: &Path, directory: bool) -> io::Result<()> {
        self.check(path, true)?;
        self.remove_checked(path, directory)
    }
    fn remove_checked(&self, path: &Path, directory: bool) -> io::Result<()> {
        let entry = self.provider.stat_unchecked(path)?;
        if entry.is_symlink || entry.is_dir != directory || (!directory && !entry.entry().is_file) {
            return Err(crate::real_mode::plan_drift(
                "SMB-Löschobjekt änderte seinen Typ",
            ));
        }
        if directory && !self.provider.list_complete(path)?.entries.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::DirectoryNotEmpty,
                "SMB-Ordner ist nicht leer",
            ));
        }
        // FILE_DELETE_ON_CLOSE adressiert ein geöffnetes Serverhandle. Das erste
        // Öffnen bleibt pfadbasiert; eine atomare Elternbindung wird nicht behauptet.
        let result = if directory {
            self.provider.remove_dir_unchecked(path)
        } else {
            self.provider.remove_file_unchecked(path)
        };
        result?;
        match self.provider.stat(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                "SMB-Löschantwort ließ den Pfad sichtbar",
            )),
            Err(e) => Err(e),
        }
    }
}
impl Vfs for OrdinarySmb {
    fn staging_scope(&self, target: &Path, _: Option<&Path>) -> io::Result<Option<Box<dyn Vfs>>> {
        self.check(target, true)?;
        let mut scoped = self.clone();
        scoped.file = Some(Arc::new(OrdinarySmbFile {
            target: target.to_path_buf(),
            temp: Mutex::new(None),
            intent: OnceLock::new(),
            attempted: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            dacl_attempted: AtomicBool::new(false),
            dacl_flush: Mutex::new(None),
            sparse: Mutex::new(None),
            sparse_commit: Mutex::new(None),
        }));
        Ok(Some(Box::new(scoped)))
    }
    fn staging_recovery_path(&self) -> Option<&Path> {
        self.file
            .as_ref()
            .and_then(|f| f.intent.get())
            .map(|i| i.path())
    }
    fn finish_staging(&self) -> io::Result<()> {
        if let Some(file) = &self.file {
            if let Some(intent) = file.intent.get() {
                if !file.settled.load(Ordering::SeqCst) {
                    return Err(crate::coded_error(
                        io::ErrorKind::Other,
                        crate::ErrorCode::OutcomeUnclear,
                        "SMB-Tempwirkung ist nicht abgeschlossen; Prüfpfade bleiben erhalten",
                    ));
                }
                intent.finish()?;
            }
        }
        Ok(())
    }
    fn cleanup_owned_temp(&self, path: &Path) -> io::Result<bool> {
        self.remove_owned_temp(path)
    }
    fn prepare_windows_dacl_write(&self) -> io::Result<()> {
        self.prepare_dacl()
    }
    fn windows_dacl_write_support(
        &self,
    ) -> io::Result<crate::windows_dacl::WindowsDaclWriteSupport> {
        self.dacl_support()
    }
    fn prepare_file_dacl_write(&self) -> io::Result<()> {
        self.prepare_file_dacl()
    }
    fn file_dacl_write_support(&self) -> io::Result<crate::windows_dacl::WindowsDaclWriteSupport> {
        self.file_dacl_support()
    }
    fn begin_file_dacl_transaction(
        &self,
        path: &Path,
        plan: &crate::file_dacl::FileDaclPlan,
    ) -> io::Result<Box<dyn crate::file_dacl::FileDaclTransaction>> {
        self.begin_file_dacl(path, plan)
    }
    fn file_dacl_recovery_observation(
        &self,
        path: &Path,
        expected_root: &crate::RootIdentity,
    ) -> io::Result<crate::file_dacl::FileDaclRecoveryObservation> {
        self.check_namespace(path, true)?;
        if expected_root != &self.identity {
            return Err(crate::real_mode::plan_drift(
                "Datei-DACL-Recoveryroot änderte sich",
            ));
        }
        self.provider.read_file_dacl_recovery(path, expected_root)
    }
    fn created_directory_foreign_root_scope(
        &self,
        root: &crate::RootIdentity,
    ) -> io::Result<Option<crate::created_directory_dacl::CreatedDirectoryRootScope>> {
        self.provider.observe_created_foreign_root_scope(root)
    }
    fn created_directory_root_scope(
        &self,
        path: &Path,
        root: &crate::RootIdentity,
    ) -> io::Result<Option<crate::created_directory_dacl::CreatedDirectoryRootScope>> {
        self.check(path, false)?;
        self.provider.observe_created_root_scope(path, root)
    }
    fn created_directory_dacl_write_support(
        &self,
    ) -> io::Result<crate::windows_dacl::WindowsDaclWriteSupport> {
        self.directory_dacl_support()
    }
    fn create_directory_dacl_receipt(
        &self,
        path: &Path,
    ) -> io::Result<Option<Box<dyn crate::created_directory_dacl::CreatedDirectoryDaclReceipt>>>
    {
        self.create_dacl_receipt(path).map(Some)
    }
    fn create_directory_before_children_receipt(
        &self,
        path: &Path,
    ) -> io::Result<Option<Box<dyn crate::created_directory_dacl::CreatedDirectoryDaclReceipt>>>
    {
        self.create_dacl_receipt_for_origin(path,
            crate::created_directory_dacl::CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren).map(Some)
    }
    fn set_created_directory_mtime(
        &self,
        path: &Path,
        mtime: i64,
        expected: &crate::created_directory_dacl::CreatedDirectoryEffectKey,
    ) -> io::Result<()> {
        if !self.sync_metadata
            || expected.origin != crate::created_directory_dacl::CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren
            || expected.directory != path.to_string_lossy()
            || expected.file_id == 0
            || expected.object_namespace != crate::created_directory_dacl_registry::object_namespace(&self.identity)?
        {
            return Err(crate::real_mode::plan_drift("V8-Zeitabschluss besitzt keinen passenden Originalbeleg"));
        }
        self.check(path, true)?;
        self.provider
            .set_mtime_phased(path, mtime, Some(expected.file_id))
            .map_err(|failure| failure.into_ordinary_io(path))?;
        self.check(path, true).map_err(|error| {
            crate::error::with_recovery_context(
                crate::coded_error(
                    error.kind(),
                    crate::ErrorCode::OutcomeUnclear,
                    format!("Originalgebundener SMB-Zeitabschluss ist unklar: {error}"),
                ),
                &[&error],
                &[path],
            )
        })
    }
    fn prepare_directory_dacl_write(&self) -> io::Result<()> {
        self.prepare_directory_dacl()
    }
    fn directory_dacl_write_support(
        &self,
    ) -> io::Result<crate::windows_dacl::WindowsDaclWriteSupport> {
        self.directory_dacl_support()
    }
    fn directory_dacl_recovery_observation(
        &self,
        path: &Path,
        expected_root: &crate::RootIdentity,
    ) -> io::Result<crate::windows_dacl::DirectoryDaclRecoveryObservation> {
        self.check_namespace(path, true)?;
        if expected_root != &self.identity {
            return Err(crate::real_mode::plan_drift(
                "SMB-DACL-Recoveryroot änderte sich",
            ));
        }
        self.provider
            .read_directory_dacl_recovery(path, expected_root)
    }
    fn begin_directory_dacl_transaction(
        &self,
        path: &Path,
        plan: &crate::directory_dacl::DirectoryDaclPlan,
    ) -> io::Result<Box<dyn crate::directory_dacl::DirectoryDaclTransaction>> {
        self.begin_directory_dacl(path, plan)
    }
    fn write_windows_dacl_exact(
        &self,
        path: &Path,
        acl: &crate::windows_security::WindowsDacl,
        context: &crate::windows_dacl::WindowsDaclWriteContext,
    ) -> io::Result<()> {
        self.write_dacl(path, acl, context)
    }
    fn supports_mode_write(&self) -> bool {
        false
    }
    fn file_commit_class(&self) -> FileCommitClass {
        FileCommitClass::Assured
    }
    fn file_flush_guarantees_at(&self, _: &Path) -> io::Result<(bool, bool)> {
        Ok((true, false))
    }
    fn file_replace_class(&self) -> FileReplaceClass {
        FileReplaceClass::BestEffort
    }
    fn supports_best_effort_permanent_delete(&self) -> bool {
        true
    }
    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        self.check(path, false)?;
        self.provider.list(path)
    }
    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        self.check(path, false)?;
        self.provider.list_complete(path)
    }
    fn stat(&self, path: &Path) -> io::Result<Entry> {
        self.provider.stat(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }
    fn root_identity(&self, path: &Path) -> io::Result<crate::real_mode::RootIdentity> {
        self.check(path, false)?;
        self.provider.root_identity(path)
    }
    fn open_sparse_read(&self, path: &Path) -> io::Result<Box<dyn crate::vfs::SparseRead>> {
        self.check(path, true)?;
        let reader = self
            .provider
            .sparse_read(path, Some(&self.root), self.cancel.clone())?;
        self.check(path, true)?;
        Ok(reader)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        self.check(path, true)?;
        self.provider.open_read(path)
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(invalid("SMB schreibt ausschließlich neue Tempdateien"))
    }
    fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.check(path, true)?;
        self.start_intent(path)?;
        let mut writer = self.provider.open_write_new_unchecked(path)?;
        self.bind_created_temp(path, &mut writer)?;
        Ok(Box::new(writer))
    }
    fn prepare_sparse_write(&self, source_acl: crate::metadata::AclModel) -> io::Result<()> {
        self.prepare_sparse(source_acl)
    }
    fn open_sparse_write_new(&self, path: &Path) -> io::Result<Box<dyn crate::vfs::SparseWrite>> {
        self.open_sparse(path)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.check(path, false)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.check(path, true)?;
        self.start_intent(path)?;
        self.provider.create_dir_unchecked(path)?;
        let entry = self.provider.stat(path)?;
        if !entry.is_dir || entry.is_symlink {
            return Err(invalid("SMB-Ordnerreadback weicht ab"));
        }
        if let Some(file) = &self.file {
            file.settled.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn sync_file(&self, path: &Path) -> io::Result<()> {
        if self.sync_dacl_temp(path)? {
            return Ok(());
        }
        if self.sync_sparse_metadata(path)? {
            return Ok(());
        }
        self.check(path, true)?;
        let remote = self.provider.remote_path(path)?;
        let mut state = lock(&self.provider.state);
        let tree = state.session.tree.clone();
        self.provider.runtime.block_on(flush_path(
            &mut state.session.connection,
            tree.as_ref(),
            &remote,
            false,
        ))
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.check(path, false)
    }
    fn set_mode(&self, _: &Path, _: u32) -> io::Result<()> {
        Err(invalid("SMB überträgt keine POSIX-Rechte"))
    }
    fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
        if !self.sync_metadata {
            return Err(invalid(
                "Gewöhnliche SMB-Inhaltsaktionen übertragen keine Dateizeiten",
            ));
        }
        self.check(path, true)?;
        let expected_temp = match self.dacl_temp_mtime_identity(path)? {
            Some(identity) => Some(identity),
            None => self.sparse_temp_mtime_identity(path)?,
        };
        self.provider
            .set_mtime_phased(path, mtime, expected_temp)
            .map_err(|failure| failure.into_ordinary_io(path))?;
        self.check(path, true).map_err(|error| {
            crate::error::with_recovery_context(
                crate::coded_error(
                    error.kind(),
                    crate::ErrorCode::OutcomeUnclear,
                    format!("SMB-Zeitübernahme besitzt einen unklaren Abschluss: {error}"),
                ),
                &[&error],
                &[path],
            )
        })
    }
    fn supports_mtime_write(&self) -> bool {
        self.sync_metadata
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        if self.file.is_some() {
            return if self.remove_owned_temp(path)? {
                Ok(())
            } else {
                Err(self.dacl_unclear(
                    path,
                    self.staging_recovery_path(),
                    invalid("SMB-Tempbesitz ist nicht für eine Löschung freigegeben"),
                ))
            };
        }
        self.remove(path, false)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.remove(path, true)
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(invalid(
            "SMB entfernt Bäume ausschließlich über bestätigte Einzelrecords",
        ))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.rename_replace_plain_once(from, to)
    }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self
            .file
            .as_ref()
            .is_some_and(|file| file.dacl_attempted.load(Ordering::SeqCst))
        {
            return self.rename_dacl_temp(from, to, false);
        }
        self.check_temp_completion(from)?;
        self.check(from, true)?;
        self.check(to, true)?;
        if let Some(file) = &self.file {
            file.attempted.store(true, Ordering::SeqCst);
        }
        self.prepare_sparse_commit()?;
        let result = self
            .provider
            .rename_noreplace_unchecked(from, to)
            .and_then(|()| self.finish_sparse_commit(to));
        if let Some(file) = &self.file {
            if result.is_ok() {
                file.settled.store(true, Ordering::SeqCst);
            } else if result
                .as_ref()
                .is_err_and(|e| e.kind() == io::ErrorKind::AlreadyExists)
            {
                self.reject_sparse_commit(from).map_err(|error| {
                    self.dacl_unclear(from, self.staging_recovery_path(), error)
                })?;
                file.attempted.store(false, Ordering::SeqCst);
            }
        }
        result
    }
    fn rename_noreplace_with_removal_proof(
        &self,
        from: &Path,
        to: &Path,
    ) -> Result<Option<ProofListEntry>, MutationFailure> {
        self.rename_noreplace(from, to)
            .map(|()| None)
            .map_err(|error| {
                classify_commit_failure(
                    error,
                    self.file
                        .as_ref()
                        .map(|file| file.attempted.load(Ordering::SeqCst)),
                )
            })
    }
    fn rename_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.rename_noreplace(from, to)
    }
    fn rename_replace_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self
            .file
            .as_ref()
            .is_some_and(|file| file.dacl_attempted.load(Ordering::SeqCst))
        {
            return self.rename_dacl_temp(from, to, true);
        }
        self.check_temp_completion(from)?;
        self.check(from, true)?;
        self.check(to, true)?;
        if let Some(file) = &self.file {
            file.attempted.store(true, Ordering::SeqCst);
        }
        self.prepare_sparse_commit()?;
        self.provider.rename_replace_unchecked(from, to)?;
        self.finish_sparse_commit(to)?;
        if let Some(file) = &self.file {
            file.settled.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn metadata_manifest(&self, path: &Path) -> io::Result<crate::metadata::MetadataManifest> {
        self.provider
            .validate_stream_scope(path, Some(&self.root))?;
        self.check(path, path != self.root)?;
        let manifest = self.provider.stream_manifest_with_write_identity(
            path,
            Some(&self.cancel),
            Some(&self.root),
            self.sparse_manifest_identity(path),
        )?;
        self.check(path, path != self.root)?;
        Ok(manifest)
    }
    fn open_named_stream_read(&self, path: &Path, name: &[u8]) -> io::Result<Box<dyn Read + Send>> {
        crate::validate_named_stream_name(name)?;
        self.provider
            .validate_stream_scope(path, Some(&self.root))?;
        self.check(path, true)?;
        let reader = self.provider.stream_read_with_write_identity(
            path,
            name,
            Some(&self.root),
            self.sparse_manifest_identity(path),
        )?;
        self.check(path, true)?;
        Ok(reader)
    }
    fn open_named_stream_write_new(
        &self,
        path: &Path,
        name: &[u8],
    ) -> io::Result<Box<dyn Write + Send>> {
        crate::validate_named_stream_name(name)?;
        self.provider
            .validate_stream_scope(path, Some(&self.root))?;
        self.check(path, true)?;
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| invalid("SMB-Streamwrite benötigt eine eigene Tempbindung"))?;
        if lock(&file.dacl_flush).is_some() {
            return Err(invalid(
                "SMB-Datenströme müssen vor dem DACL-/FLUSH-Abschluss geschrieben sein",
            ));
        }
        if file.attempted.load(Ordering::SeqCst) || file.settled.load(Ordering::SeqCst) {
            return Err(invalid("SMB-Tempdatei wurde bereits abgeschlossen"));
        }
        let owned = lock(&file.temp).clone();
        let (temp, inode) =
            owned.ok_or_else(|| invalid("SMB-Tempdatei wurde noch nicht exklusiv angelegt"))?;
        if temp != path {
            return Err(invalid(
                "SMB-Streamwrite liegt nicht auf der eigenen Tempdatei",
            ));
        }
        let writer = self
            .provider
            .stream_write_new(path, name, inode, Some(&self.root))?;
        self.check(path, true)?;
        Ok(writer)
    }
    fn supports_directory_stream_updates(&self, root: &Path) -> io::Result<bool> {
        if !self.sync_metadata || !self.stream_target {
            return Ok(false);
        }
        self.check(root, false)?;
        let supported = self.provider.named_stream_capability(root, true)? == Capability::Exact;
        self.check(root, false)?;
        Ok(supported)
    }
    fn begin_directory_stream_update(
        &self,
        path: &Path,
        expected_base: &crate::sync::SyncTargetProof,
        expected_streams: &[crate::metadata::NamedBlobSummary],
    ) -> io::Result<Box<dyn crate::directory_streams::DirectoryStreamTransaction>> {
        if !self.sync_metadata || !self.stream_target {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SMB-Verzeichnisstreams benötigen den gewöhnlichen Sync-Zielkontext",
            ));
        }
        self.provider
            .validate_stream_scope(path, Some(&self.root))?;
        self.check(path, false)?;
        let owner = self.owner.as_ref().ok_or_else(|| {
            invalid("SMB-Verzeichnisstreams benötigen einen laufgebundenen Stagingowner")
        })?;
        let context = self.clone();
        let checked_path = path.to_path_buf();
        self.provider.begin_directory_stream_transaction(
            path,
            &self.root,
            expected_base,
            expected_streams,
            self.cancel.clone(),
            owner.staging.clone(),
            self.identity.clone(),
            Box::new(move || context.check_namespace(&checked_path, false)),
        )
    }
    fn metadata_capabilities(&self, root: &Path) -> io::Result<MetadataCapabilities> {
        self.check(root, false)?;
        let mut capabilities = self
            .provider
            .stream_metadata_capabilities(root, self.stream_target)?;
        if self.sync_metadata && self.stream_target {
            capabilities.sparse_extents =
                lock(&self.sparse_support).unwrap_or(Capability::Unsupported);
        }
        self.check(root, false)?;
        Ok(capabilities)
    }
    fn available_bytes(&self, path: &Path) -> io::Result<u64> {
        self.check(path, false)?;
        self.provider.available_bytes_unchecked(path)
    }
    fn reclaim_verification_session(&self) -> io::Result<()> {
        self.provider.reconnect()
    }
}

#[cfg(test)]
pub(super) fn verify_created_temp_binding(provider: &Arc<SmbVfs>) -> io::Result<()> {
    fn context(provider: &Arc<SmbVfs>, target: &Path) -> io::Result<OrdinarySmb> {
        Ok(OrdinarySmb {
            provider: provider.clone(),
            root: PathBuf::from("/"),
            identity: provider.root_identity(Path::new("/"))?,
            cancel: crate::Cancel::new(),
            owner: None,
            file: Some(Arc::new(OrdinarySmbFile {
                target: target.to_path_buf(),
                temp: Mutex::new(None),
                intent: OnceLock::new(),
                attempted: AtomicBool::new(false),
                settled: AtomicBool::new(false),
                dacl_attempted: AtomicBool::new(false),
                dacl_flush: Mutex::new(None),
                sparse: Mutex::new(None),
                sparse_commit: Mutex::new(None),
            })),
            sync_metadata: true,
            stream_target: true,
            dacl_support: Arc::new(Mutex::new(None)),
            directory_dacl_support: Arc::new(Mutex::new(None)),
            file_dacl_support: Arc::new(Mutex::new(None)),
            sparse_support: Arc::new(Mutex::new(None)),
            sparse_dacl: Arc::new(AtomicBool::new(false)),
            sparse_metadata_restore: Arc::new(AtomicBool::new(false)),
        })
    }
    let temp = Path::new("/swap-temp");
    let original = Path::new("/swap-created");
    let foreign = Path::new("/swap-foreign");
    let owned = context(provider, Path::new("/swap-target"))?;
    let other = context(provider, foreign)?;
    let mut foreign_writer = other.open_write_new(foreign)?;
    foreign_writer.write_all(b"foreign-main")?;
    foreign_writer.flush()?;
    drop(foreign_writer);
    let mut foreign_stream = other.open_named_stream_write_new(foreign, b"foreign-stream")?;
    foreign_stream.write_all(b"must-remain")?;
    foreign_stream.flush()?;
    drop(foreign_stream);
    let foreign_manifest = provider.stream_manifest(foreign)?;
    let mut writer = provider.open_write_new_unchecked(temp)?;
    writer.write_all(b"created-main")?;
    let created_inode = writer.created_file_identity()?;
    // Exakt das Fenster zwischen erfolgreichem CREATE und der Besitzbindung:
    // Der erzeugte Handle bleibt offen, der Pfad zeigt nun auf die fremde Datei.
    provider.rename_noreplace_unchecked(temp, original)?;
    provider.rename_noreplace_unchecked(foreign, temp)?;
    let error = owned.bind_created_temp(temp, &mut writer).unwrap_err();
    assert_eq!(
        crate::error::error_code(&error),
        crate::ErrorCode::OutcomeUnclear
    );
    assert!(crate::error::recovery_paths(&error).contains(&"/swap-temp".to_owned()));
    assert!(lock(&owned.file.as_ref().unwrap().temp).is_none());
    assert!(!owned.cleanup_owned_temp(temp)?);
    assert!(owned
        .open_named_stream_write_new(temp, b"must-not-appear")
        .is_err());
    assert_eq!(
        provider.stream_manifest(temp)?.digest,
        foreign_manifest.digest
    );
    let mut bytes = Vec::new();
    provider.open_read(temp)?.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"foreign-main");
    assert_eq!(provider.stat(original)?.inode, Some(created_inode));
    writer.flush()?;
    drop(writer);
    provider.remove_file_unchecked(temp)?;
    provider.remove_file_unchecked(original)?;

    // Auch ein QUERY_INFO-Fehler nach erfolgreicher Anlage ist bereits eine
    // mögliche Dateiwirkung. Er darf weder Besitz noch automatische Bereinigung
    // legitimieren und muss den bekannten Temp als Prüfpfad erhalten.
    let failed = context(provider, Path::new("/failed-target"))?;
    let mut writer = provider.open_write_new_unchecked(temp)?;
    let inode = writer.created_file_identity()?;
    writer
        .runtime
        .block_on(
            writer
                .tree
                .close_handle(&mut writer.connection, writer.file_id),
        )
        .map_err(smb_error)?;
    writer.finished = true;
    let error = failed.bind_created_temp(temp, &mut writer).unwrap_err();
    assert_eq!(
        crate::error::error_code(&error),
        crate::ErrorCode::OutcomeUnclear
    );
    assert!(crate::error::recovery_paths(&error).contains(&"/swap-temp".to_owned()));
    assert!(lock(&failed.file.as_ref().unwrap().temp).is_none());
    assert!(!failed.cleanup_owned_temp(temp)?);
    assert_eq!(provider.stat(temp)?.inode, Some(inode));
    provider.remove_file_unchecked(temp)
}

fn classify_commit_failure(error: io::Error, attempted: Option<bool>) -> MutationFailure {
    if crate::error_code(&error) != crate::ErrorCode::OutcomeUnclear
        && (attempted == Some(false) || error.kind() == io::ErrorKind::AlreadyExists)
    {
        MutationFailure::Unchanged(error)
    } else {
        MutationFailure::OutcomeUnclear(error)
    }
}
#[cfg(test)]
mod commit_failure_tests {
    use super::*;
    #[test]
    fn collision_and_precheck_preserve_unchanged_but_lost_reply_keeps_recovery() {
        assert!(matches!(
            classify_commit_failure(io::ErrorKind::AlreadyExists.into(), Some(false)),
            MutationFailure::Unchanged(_)
        ));
        assert!(matches!(
            classify_commit_failure(io::ErrorKind::PermissionDenied.into(), Some(false)),
            MutationFailure::Unchanged(_)
        ));
        assert!(matches!(
            classify_commit_failure(io::ErrorKind::ConnectionAborted.into(), Some(true)),
            MutationFailure::OutcomeUnclear(_)
        ));
        let unclear = crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            "Ungeklärte Metadatenwirkung",
        );
        assert!(matches!(
            classify_commit_failure(unclear, Some(false)),
            MutationFailure::OutcomeUnclear(_)
        ));
    }
}
