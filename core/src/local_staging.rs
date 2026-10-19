//! Private, laufgebundene Zwischenstände. Sichtbare Pfade sind nur Recoveryhinweise;
//! jede Wirkung verwendet die während der Vorbereitung gehaltenen Deskriptoren.

use crate::local_fs::RootHandle;
use crate::metadata::{Acl, PosixAcl, PosixAclTag};
use rustix::fs::{fchmod, fchown, fstat, openat, renameat, Gid, Mode, OFlags};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StagingOwner {
    pub(crate) queue: PathBuf,
    pub(crate) operation: String,
}

/// Auf Volumes ohne privaten Namensraum bleibt die Tempdatei unangetastet.
/// Ihr vor der Anlage dauerhaft gespeicherter Index lebt im privaten Queueordner;
/// er ist ausschließlich ein Prüfhinweis und erlaubt niemals eine Datenlöschung.
pub(crate) struct NeighborStaging {
    config: RootHandle,
    pub(crate) parent: RootHandle,
    index: PathBuf,
    record: Mutex<NeighborRecord>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct NeighborRecord {
    version: u32,
    owner: StagingOwner,
    target: PathBuf,
    candidate: Option<PathBuf>,
    #[serde(default)]
    remote: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ftps: Option<FtpsIntentBinding>,
}

fn encode_neighbor_record(record: &NeighborRecord) -> io::Result<Vec<u8>> {
    let data = serde_json::to_vec(record).map_err(io::Error::other)?;
    if data.len() > 64 * 1024 {
        return Err(io::Error::other(
            "Temp-Recoveryindex überschreitet Größenlimit",
        ));
    }
    Ok(data)
}

/// Dauerhafte Beobachtungsbindung, kein Permit und kein Löschbeleg. Der STOU-Name
/// bleibt bis zu seiner bestätigten Antwort ausdrücklich unbekannt.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct FtpsIntentBinding {
    pub(crate) roots: Vec<crate::remote::FtpsRootIdentity>,
    pub(crate) account_digest: [u8; 32],
    pub(crate) phase: String,
    pub(crate) stou_name_unknown: bool,
    pub(crate) paths: Vec<PathBuf>,
}

fn validate_ftps_intent(binding: &FtpsIntentBinding, target: &Path) -> io::Result<()> {
    if !(1..=2).contains(&binding.roots.len())
        || binding.account_digest == [0; 32]
        || binding.account_digest == *blake3::hash(&[]).as_bytes()
        || !matches!(
            binding.phase.as_str(),
            "mkdir" | "probe" | "stou" | "rename" | "delete"
        )
        || (binding.stou_name_unknown && binding.phase != "stou")
        || binding.paths.is_empty()
        || binding.paths.len() > 16
    {
        return Err(io::Error::other("Ungültige FTPS-Intentbindung"));
    }
    for root in &binding.roots {
        root.validate()?;
        if root.server != binding.roots[0].server
            || root.session_generation != binding.roots[0].session_generation
        {
            return Err(io::Error::other("FTPS-Intent vermischt Sitzungen"));
        }
    }
    for path in std::iter::once(target).chain(binding.paths.iter().map(PathBuf::as_path)) {
        if !path.is_absolute()
            || path
                .to_str()
                .is_none_or(|value| value.len() > 4096 || value.chars().any(char::is_control))
            || path.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
            || !binding.roots.iter().any(|root| {
                let scope = Path::new(&root.server.canonical_root)
                    .join(root.virtual_root.trim_start_matches('/'));
                path.starts_with(scope)
            })
        {
            return Err(io::Error::other(
                "FTPS-Intentpfad verlässt die gebundenen Roots",
            ));
        }
    }
    Ok(())
}

/// Ein Remote-Intent ist nur ein dauerhafter Prüfhinweis. Weder Servertoken noch
/// Secrets werden gespeichert; nach einem Absturz entsteht daraus kein Löschrecht.
pub(crate) struct RemoteTempIntent {
    config: RootHandle,
    index: PathBuf,
    record: Mutex<NeighborRecord>,
}

impl RemoteTempIntent {
    pub(crate) fn create(
        owner: &StagingOwner,
        target: &Path,
        candidate: &Path,
    ) -> io::Result<Self> {
        Self::create_record(owner, target, Some(candidate), None, None)
    }

    fn create_record(
        owner: &StagingOwner,
        target: &Path,
        candidate: Option<&Path>,
        ftps: Option<FtpsIntentBinding>,
        held_config: Option<&RootHandle>,
    ) -> io::Result<Self> {
        let parent = owner
            .queue
            .parent()
            .ok_or_else(|| io::Error::other("Queue ohne Elternordner"))?;
        let config = match held_config {
            Some(config) if config.path() == parent => config.clone(),
            Some(_) => {
                return Err(io::Error::other(
                    "FTPS-Intent und Queueordner unterscheiden sich",
                ))
            }
            None => RootHandle::open_nofollow(parent)?,
        };
        if !config.is_current()?
            || !config.has_exclusive_namespace()?
            || candidate.is_some_and(|candidate| candidate.parent() != target.parent())
        {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Remote-Tempintent benötigt einen privaten gehaltenen Queueordner und genau einen Zielordner"));
        }
        let mut index =
            crate::atomic_file::sibling_candidate(&config.path().join("neighbor"), "recovery")?
                .into_os_string();
        index.push(".recovery.json");
        let index = PathBuf::from(index);
        let record = NeighborRecord {
            version: if ftps.is_some() { 2 } else { 1 },
            owner: owner.clone(),
            target: target.to_path_buf(),
            candidate: candidate.map(Path::to_path_buf),
            remote: true,
            ftps,
        };
        let vfs = crate::LocalVfs::rooted(config.path())?;
        if crate::Vfs::anchored_identity(&vfs) != Some(config.identity()) {
            return Err(io::Error::other("Remote-Intentordner wurde ausgetauscht"));
        }
        crate::atomic_write::atomic_write_vfs_with_mode(
            &vfs,
            &index,
            &encode_neighbor_record(&record)?,
            ".remote-intent-",
            crate::atomic_write::AtomicWriteMode::Create,
            Some(0o600),
        )?;
        Ok(Self {
            config,
            index,
            record: Mutex::new(record),
        })
    }

    pub(crate) fn create_ftps(
        owner: &StagingOwner,
        config: &RootHandle,
        target: &Path,
        binding: FtpsIntentBinding,
    ) -> io::Result<Self> {
        validate_ftps_intent(&binding, target)?;
        Self::create_record(owner, target, None, Some(binding), Some(config))
    }

    pub(crate) fn ftps_stou_pending(&self) -> io::Result<()> {
        self.update_ftps(|binding| {
            binding.phase = "stou".into();
            binding.stou_name_unknown = true;
        })
    }

    pub(crate) fn ftps_stou_name(&self, path: &Path) -> io::Result<()> {
        self.update_ftps(|binding| {
            binding.stou_name_unknown = false;
            if !binding.paths.contains(&path.to_path_buf()) {
                binding.paths.push(path.to_path_buf());
            }
        })
    }

    fn update_ftps(&self, update: impl FnOnce(&mut FtpsIntentBinding)) -> io::Result<()> {
        {
            let mut record = self
                .record
                .lock()
                .map_err(|_| io::Error::other("Remote-Intent gesperrt"))?;
            let target = record.target.clone();
            let mut binding = record
                .ftps
                .clone()
                .ok_or_else(|| io::Error::other("Kein FTPS-Intent"))?;
            update(&mut binding);
            validate_ftps_intent(&binding, &target)?;
            record.ftps = Some(binding);
        }
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        if !self.config.is_current()? || !self.config.has_exclusive_namespace()? {
            return Err(io::Error::other("Remote-Intentordner wurde ausgetauscht"));
        }
        let vfs = crate::LocalVfs::rooted(self.config.path())?;
        if crate::Vfs::anchored_identity(&vfs) != Some(self.config.identity()) {
            return Err(io::Error::other("Remote-Intentordner wurde ausgetauscht"));
        }
        crate::atomic_write::atomic_write_vfs_with_mode(
            &vfs,
            &self.index,
            &encode_neighbor_record(
                &*self
                    .record
                    .lock()
                    .map_err(|_| io::Error::other("Remote-Intent gesperrt"))?,
            )?,
            ".remote-intent-",
            crate::atomic_write::AtomicWriteMode::Replace,
            Some(0o600),
        )
    }

    pub(crate) fn finish(&self) -> io::Result<()> {
        if !self.config.is_current()? || !self.config.has_exclusive_namespace()? {
            return Err(io::Error::other("Remote-Intentordner wurde ausgetauscht"));
        }
        crate::local_fs::remove_file_beneath(&self.config, &self.index)?;
        crate::local_fs::sync_dir_beneath(&self.config, self.config.path())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.index
    }

    pub(crate) fn ftps_recovery(&self) -> (Vec<String>, bool) {
        let mut paths = vec![self.index.to_string_lossy().into_owned()];
        let mut unknown = false;
        if let Ok(record) = self.record.lock() {
            paths.push(record.target.to_string_lossy().into_owned());
            if let Some(binding) = &record.ftps {
                unknown = binding.stou_name_unknown;
                paths.extend(
                    binding
                        .paths
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned()),
                );
                if unknown {
                    if let Some(parent) = record.target.parent() {
                        paths.push(parent.to_string_lossy().into_owned());
                    }
                }
            }
        }
        paths.sort();
        paths.dedup();
        (paths, unknown)
    }
}

impl NeighborStaging {
    pub(crate) fn create(
        owner: &StagingOwner,
        target: &Path,
        root: Option<&RootHandle>,
    ) -> io::Result<Option<Self>> {
        let parent_path = target
            .parent()
            .ok_or_else(|| io::Error::other("Tempziel ohne Elternordner"))?;
        let parent = match root {
            Some(root) => RootHandle::open_beneath(root, parent_path)?,
            None => RootHandle::open_nofollow(parent_path)?,
        };
        let config = RootHandle::open_nofollow(
            owner
                .queue
                .parent()
                .ok_or_else(|| io::Error::other("Queue ohne Elternordner"))?,
        )?;
        if !config.has_exclusive_namespace()? {
            return Ok(None);
        }
        let candidate =
            crate::atomic_file::sibling_candidate(&config.path().join("neighbor"), "recovery")?;
        let mut name = candidate.into_os_string();
        name.push(".recovery.json");
        let index = PathBuf::from(name);
        let record = NeighborRecord {
            version: 1,
            owner: owner.clone(),
            target: target.to_path_buf(),
            candidate: None,
            remote: false,
            ftps: None,
        };
        let value = Self {
            config,
            parent,
            index,
            record: Mutex::new(record),
        };
        value.save(crate::atomic_write::AtomicWriteMode::Create)?;
        Ok(Some(value))
    }

    fn save(&self, mode: crate::atomic_write::AtomicWriteMode) -> io::Result<()> {
        if !self.config.is_current()? || !self.config.has_exclusive_namespace()? {
            return Err(io::Error::other("Temp-Recoveryordner wurde ausgetauscht"));
        }
        let data = serde_json::to_vec(
            &*self
                .record
                .lock()
                .map_err(|_| io::Error::other("Temp-Recovery gesperrt"))?,
        )
        .map_err(io::Error::other)?;
        let vfs = crate::LocalVfs::rooted(self.config.path())?;
        if crate::Vfs::anchored_identity(&vfs) != Some(self.config.identity()) {
            return Err(io::Error::other(
                "Temp-Recoveryordner wurde vor dem Journalschreiben ausgetauscht",
            ));
        }
        crate::atomic_write::atomic_write_vfs_with_mode(
            &vfs,
            &self.index,
            &data,
            ".neighbor-index-",
            mode,
            Some(0o600),
        )
    }

    pub(crate) fn candidate(&self, path: &Path) -> io::Result<()> {
        self.verify_parent()?;
        self.record
            .lock()
            .map_err(|_| io::Error::other("Temp-Recovery gesperrt"))?
            .candidate = Some(path.to_path_buf());
        self.save(crate::atomic_write::AtomicWriteMode::Replace)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.index
    }

    fn verify_parent(&self) -> io::Result<()> {
        if !self.parent.is_current()? {
            return Err(crate::real_mode::plan_drift(
                "Ziel-Elternordner des Nachbartemps wurde ausgetauscht",
            ));
        }
        Ok(())
    }

    pub(crate) fn finish(&self) -> io::Result<()> {
        self.verify_parent()?;
        let record = self
            .record
            .lock()
            .map_err(|_| io::Error::other("Temp-Recovery gesperrt"))?;
        if let Some(candidate) = &record.candidate {
            for path in [candidate.to_path_buf(), apple_double(candidate)] {
                match crate::local_fs::stat_beneath(&self.parent, &path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(crate::error::with_recovery_paths(
                            io::Error::other("Tempbestand blieb zur manuellen Prüfung erhalten"),
                            &[path.to_string_lossy().into_owned()],
                        ))
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        if !self.config.is_current()? || !self.config.has_exclusive_namespace()? {
            return Err(io::Error::other("Temp-Recoveryordner wurde ausgetauscht"));
        }
        crate::local_fs::remove_file_beneath(&self.config, &self.index)?;
        crate::local_fs::sync_dir_beneath(&self.config, self.config.path())
    }
}

fn apple_double(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from("._");
    name.push(path.file_name().unwrap_or_default());
    path.with_file_name(name)
}

pub(crate) struct LocalStaging {
    anchor: RootHandle,
    outer: Option<RootHandle>,
    lease: File,
    registry: Option<(RootHandle, PathBuf)>,
    finished: std::sync::atomic::AtomicBool,
    pub(crate) parent: RootHandle,
    pub(crate) directory: RootHandle,
    inheritance: Inheritance,
    journal: Mutex<Journal>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Inheritance {
    uid: u32,
    mode: u32,
    gid: u32,
    acl: Option<Acl>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Journal {
    version: u32,
    process: u32,
    parent: PathBuf,
    parent_identity: crate::vfs::LocalRootIdentity,
    staging_identity: crate::vfs::LocalRootIdentity,
    anchor_identity: crate::vfs::LocalRootIdentity,
    owner: Option<StagingOwner>,
    actions: Vec<Action>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Action {
    phase: String,
    from: PathBuf,
    to: Option<PathBuf>,
    from_identity: Option<(u64, u64)>,
    to_identity: Option<(u64, u64)>,
}

// Vor der Rückgabe eines Scope können Lease- oder Journalanlage scheitern. Der
// private Ordner existiert dann schon; sein Prüfpfad muss auch strukturiert erhalten
// bleiben, ohne einen neuen pfadbasierten Aufräumweg zu eröffnen.
fn prepare_with_recovery<T>(path: &Path, prepare: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    prepare().map_err(|error| {
        crate::error::with_recovery_paths(error, &[path.to_string_lossy().into_owned()])
    })
}

impl LocalStaging {
    pub(crate) fn create(
        root: Option<&RootHandle>,
        parent_root: Option<&RootHandle>,
        target: &Path,
        sandbox: Option<&Path>,
        owner: Option<&StagingOwner>,
        inherited_anchor: Option<&RootHandle>,
    ) -> io::Result<Option<Self>> {
        // Aktive SELinux-Policies brauchen einen eigenen belegten Kontextadapter.
        #[cfg(target_os = "linux")]
        if std::fs::read("/sys/fs/selinux/enforce").is_ok() {
            return Ok(None);
        }
        let parent_path = target
            .parent()
            .ok_or_else(|| io::Error::other("Ziel ohne Elternordner"))?;
        let outer = match root {
            Some(root) => Some(root.clone()),
            None => sandbox.map(RootHandle::open_nofollow).transpose()?,
        };
        let parent = match parent_root.or(outer.as_ref()) {
            Some(root) => RootHandle::open_beneath(root, parent_path)?,
            None => RootHandle::open_nofollow(parent_path)?,
        };
        // Ein Pane-Root kann enger als die Test-Sandbox sein. Sein gehaltener
        // Deskriptor darf dann nicht den darüberliegenden Sandboxroot öffnen;
        // das Staging bleibt stattdessen innerhalb beider Grenzen.
        let bounded_sandbox = sandbox.map(|sandbox| match root {
            Some(root) if root.path().starts_with(sandbox) => root.path(),
            _ => sandbox,
        });
        let anchor_path = bounded_sandbox
            .or_else(|| root.map(RootHandle::path))
            .or_else(|| inherited_anchor.map(RootHandle::path))
            .unwrap_or(parent_path);
        if !parent_path.starts_with(anchor_path) {
            return Ok(None);
        }
        let anchor =
            if let Some(anchor) = inherited_anchor.filter(|anchor| anchor.path() == anchor_path) {
                anchor.clone()
            } else {
                match outer.as_ref() {
                    Some(root) => RootHandle::open_beneath(root, anchor_path)?,
                    None => RootHandle::open_nofollow(anchor_path)?,
                }
            };
        if !anchor.is_private()? {
            return Ok(None);
        }
        let parent_identity = crate::local_fs::root_identity_beneath(&parent, parent.path())?;
        let anchor_identity = crate::local_fs::root_identity_beneath(&anchor, anchor.path())?;
        if parent_identity.mount != anchor_identity.mount {
            return Ok(None);
        }
        let config = owner
            .map(|owner| {
                RootHandle::open_nofollow(
                    owner
                        .queue
                        .parent()
                        .ok_or_else(|| io::Error::other("Queue ohne Konfigurationsordner"))?,
                )
            })
            .transpose()?;
        if let Some(config) = &config {
            if !config.has_exclusive_namespace()? {
                return Ok(None);
            }
        }
        let inheritance = Inheritance::read(parent.fd())?;
        let name = crate::atomic_file::sibling_candidate(&anchor.path().join("stage"), "stage")?;
        let directory = anchor.create_private_child(&name).map_err(|error| {
            let unclear = crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Private Staging-Anlage nicht bestätigt ({error}); möglicher Recoverypfad: {}",
                    name.display()
                ),
            );
            crate::error::with_recovery_paths(unclear, &[name.to_string_lossy().into_owned()])
        })?;
        let mut stage = prepare_with_recovery(&name, || {
            let lease = File::from(openat(
                directory.fd(),
                "lease",
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?);
            rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
            let journal = Journal {
                version: 1,
                process: std::process::id(),
                parent: parent.path().to_path_buf(),
                parent_identity,
                staging_identity: crate::local_fs::root_identity_beneath(
                    &directory,
                    directory.path(),
                )?,
                anchor_identity,
                owner: owner.cloned(),
                actions: Vec::new(),
            };
            let stage = Self {
                anchor,
                outer,
                lease,
                registry: None,
                finished: std::sync::atomic::AtomicBool::new(false),
                parent,
                directory,
                inheritance,
                journal: Mutex::new(journal),
            };
            stage.record("prepared", target, None)?;
            Ok(stage)
        })?;
        if owner.is_some() {
            let registered = (|| -> io::Result<()> {
                let config = config.unwrap();
                if !config.has_exclusive_namespace()? {
                    return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Staging-Recoveryindex verlangt einen gegen fremdes Schreiben geschützten Queueordner",
                ));
                }
                let name = stage
                    .directory
                    .path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy();
                let index = config.path().join(format!("{name}.recovery.json"));
                let next = index.with_extension("next");
                let mut file = File::from(openat(
                    config.fd(),
                    next.file_name().unwrap(),
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(0o600),
                )?);
                let journal = stage
                    .journal
                    .lock()
                    .map_err(|_| io::Error::other("Stagingjournal gesperrt"))?;
                serde_json::to_writer(&mut file, &*journal)?;
                file.sync_all()?;
                crate::local_fs::rename_noreplace_beneath(&config, &next, &index)?;
                stage.registry = Some((config.clone(), index));
                crate::local_fs::sync_dir_beneath(&config, config.path())?;
                drop(journal);
                Ok(())
            })();
            if let Err(error) = registered {
                let cleanup = stage.finish();
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(crate::error::with_recovery_path(
                        error,
                        stage.directory.path(),
                        &cleanup,
                    )),
                };
            }
        }
        Ok(Some(stage))
    }

    pub(crate) fn outer(&self) -> Option<&RootHandle> {
        self.outer.as_ref()
    }

    pub(crate) fn anchor(&self) -> &RootHandle {
        &self.anchor
    }

    pub(crate) fn candidate(&self, target: &Path, role: &str) -> io::Result<PathBuf> {
        let path = crate::atomic_file::sibling_candidate(
            &self.directory.path().join(
                target
                    .file_name()
                    .ok_or_else(|| io::Error::other("Ziel ohne Namen"))?,
            ),
            role,
        )?;
        self.record("create_intent", &path, Some(target))?;
        Ok(path)
    }

    fn entry_identity(&self, path: &Path) -> io::Result<Option<(u64, u64)>> {
        let root = if path.starts_with(self.directory.path()) {
            Some(&self.directory)
        } else if path.starts_with(self.parent.path()) {
            Some(&self.parent)
        } else {
            self.outer.as_ref()
        };
        let stat = match root {
            Some(root) => crate::local_fs::stat_beneath(root, path),
            None => crate::local_fs::stat(path),
        };
        match stat {
            Ok(entry) => Ok(entry.device.zip(entry.inode)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn finish(&self) -> io::Result<()> {
        if self.finished.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        self.verify_parent()?;
        let listing = crate::local_fs::list_beneath(&self.directory, self.directory.path())?;
        if listing.iter().any(|entry| {
            !matches!(
                entry.name.as_str(),
                "journal.json" | "journal.next" | "lease"
            )
        }) {
            return self.record("complete_with_remainders", self.directory.path(), None);
        }
        self.record("complete", self.directory.path(), None)?;
        // Der private, gehaltene Anker schließt fremde Namensänderungen aus. Ist
        // der sichtbare Name trotzdem ausgetauscht, bleibt der Recoverybeleg stehen.
        if !self.anchor.is_private()? || !self.directory.is_private()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Staging-Namensraum verlor seine privaten Rechte",
            ));
        }
        let visible = crate::local_fs::root_identity_beneath(&self.anchor, self.directory.path())?;
        let held = crate::local_fs::root_identity_beneath(&self.directory, self.directory.path())?;
        if visible != held {
            return Err(io::Error::other(
                "Stagingname wurde ausgetauscht; Journal blieb erhalten",
            ));
        }
        for name in ["journal.json", "lease"] {
            crate::local_fs::remove_file_beneath(
                &self.directory,
                &self.directory.path().join(name),
            )?;
        }
        self.lease.sync_all()?;
        crate::local_fs::remove_dir_beneath(&self.anchor, self.directory.path())?;
        crate::local_fs::sync_dir_beneath(&self.anchor, self.anchor.path())?;
        if let Some((config, index)) = &self.registry {
            crate::local_fs::remove_file_beneath(config, index)?;
            crate::local_fs::sync_dir_beneath(config, config.path())?;
        }
        self.finished
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    pub(crate) fn contains(&self, path: &Path) -> bool {
        path != self.directory.path() && path.starts_with(self.directory.path())
    }

    pub(crate) fn inherit_file(&self, file: &File) -> io::Result<()> {
        self.verify_parent()?;
        let fd = rustix::io::dup(file)?;
        let actual = fstat(&fd)?;
        #[cfg(target_os = "macos")]
        let gid = self.inheritance.gid;
        #[cfg(target_os = "linux")]
        let gid = if self.inheritance.mode & 0o2000 != 0 {
            self.inheritance.gid
        } else {
            actual.st_gid
        };
        fchown(file, None, Some(Gid::from_raw(gid)))?;
        #[cfg(target_os = "macos")]
        let ordinary_mode = u32::from(actual.st_mode & 0o777);
        #[cfg(target_os = "linux")]
        let ordinary_mode = actual.st_mode & 0o777;
        let (acl, mode) = inherited_file_acl(self.inheritance.acl.as_ref(), ordinary_mode);
        crate::acl::write_fd_exact(&fd, acl.as_ref())?;
        #[cfg(target_os = "macos")]
        let mode = mode as u16;
        fchmod(file, Mode::from_raw_mode(mode))?;
        // Ein exakter Readback verhindert still verlorene ACLs auf fremden Mounts.
        if crate::acl::read_fd(&fd)? != acl {
            return Err(io::Error::other(
                "ACL-Vererbung der Tempdatei stimmt im Readback nicht überein",
            ));
        }
        Ok(())
    }

    pub(crate) fn verify_parent(&self) -> io::Result<()> {
        if !self.parent.is_current().unwrap_or(false) {
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::PlanDrift,
                "Ziel-Elternordner wurde umbenannt oder ausgetauscht",
            ));
        }
        if Inheritance::read(self.parent.fd())? != self.inheritance {
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::PlanDrift,
                "Ziel-ACL, Gruppe oder Rechte änderten sich während der Übertragung",
            ));
        }
        Ok(())
    }

    pub(crate) fn record(&self, phase: &str, from: &Path, to: Option<&Path>) -> io::Result<()> {
        let mut journal = self
            .journal
            .lock()
            .map_err(|_| io::Error::other("Stagingjournal gesperrt"))?;
        journal.actions.push(Action {
            phase: phase.into(),
            from: from.into(),
            to: to.map(Path::to_path_buf),
            from_identity: self.entry_identity(from)?,
            to_identity: to
                .map(|path| self.entry_identity(path))
                .transpose()?
                .flatten(),
        });
        // Der feste Schreibname liegt im nachweislich privaten Namensraum. Kein
        // fremder Prozess kann ihn dort zwischen Schreiben und Rename austauschen.
        let fd = openat(
            self.directory.fd(),
            "journal.next",
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let mut file = File::from(fd);
        serde_json::to_writer(&mut file, &*journal)?;
        file.flush()?;
        file.sync_all()?;
        renameat(
            self.directory.fd(),
            "journal.next",
            self.directory.fd(),
            "journal.json",
        )?;
        File::from(rustix::io::dup(self.directory.fd())?).sync_all()
    }
}

impl Inheritance {
    fn read(fd: &OwnedFd) -> io::Result<Self> {
        for _ in 0..3 {
            let before = fstat(fd)?;
            let acl = crate::acl::read_fd(fd)?;
            let after = fstat(fd)?;
            if before.st_ctime == after.st_ctime
                && before.st_ctime_nsec == after.st_ctime_nsec
                && before.st_uid == after.st_uid
                && before.st_gid == after.st_gid
                && before.st_mode == after.st_mode
            {
                #[cfg(target_os = "macos")]
                let mode = u32::from(after.st_mode & 0o7777);
                #[cfg(target_os = "linux")]
                let mode = after.st_mode & 0o7777;
                return Ok(Self {
                    uid: after.st_uid,
                    mode,
                    gid: after.st_gid,
                    acl,
                });
            }
        }
        Err(crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::PlanDrift,
            "Ziel-Vererbung änderte sich während der Vorbereitung",
        ))
    }
}

fn inherited_file_acl(parent: Option<&Acl>, ordinary_mode: u32) -> (Option<Acl>, u32) {
    match parent {
        Some(Acl::Nfs4(parent)) => {
            let entries = parent
                .entries
                .iter()
                .filter(|entry| entry.flags & 32 != 0)
                .map(|entry| {
                    let mut entry = *entry;
                    entry.flags = (entry.flags & !(32 | 64 | 128 | 256)) | 16;
                    entry
                })
                .collect::<Vec<_>>();
            let acl = (!entries.is_empty())
                .then_some(Acl::Nfs4(crate::metadata::Nfs4Acl { entries, flags: 0 }));
            (acl, ordinary_mode)
        }
        Some(Acl::Posix(parent)) if !parent.default.is_empty() => {
            let mut access = parent.default.clone();
            for entry in &mut access {
                let limit = match entry.tag {
                    PosixAclTag::UserObject => 6,
                    PosixAclTag::Other => 6,
                    PosixAclTag::Mask => 6,
                    PosixAclTag::GroupObject
                        if !parent.default.iter().any(|e| e.tag == PosixAclTag::Mask) =>
                    {
                        6
                    }
                    _ => 7,
                };
                entry.permissions &= limit;
            }
            let permissions = |tag| {
                access
                    .iter()
                    .find(|e| e.tag == tag)
                    .map_or(0, |e| u32::from(e.permissions))
            };
            let mode = permissions(PosixAclTag::UserObject) << 6
                | permissions(if access.iter().any(|e| e.tag == PosixAclTag::Mask) {
                    PosixAclTag::Mask
                } else {
                    PosixAclTag::GroupObject
                }) << 3
                | permissions(PosixAclTag::Other);
            let acl = if access.len() == 3 {
                None
            } else {
                Some(Acl::Posix(PosixAcl {
                    access,
                    default: Vec::new(),
                }))
            };
            (acl, mode)
        }
        _ => (None, ordinary_mode),
    }
}

pub(crate) fn parent_inheritance(
    root: Option<&RootHandle>,
    path: &Path,
) -> io::Result<crate::metadata::LocalParentInheritance> {
    let parent = match root {
        Some(root) => RootHandle::open_beneath(root, path)?,
        None => RootHandle::open_nofollow(path)?,
    };
    let snapshot = Inheritance::read(parent.fd())?;
    Ok(crate::metadata::LocalParentInheritance {
        identity: crate::local_fs::root_identity_beneath(&parent, parent.path())?,
        uid: snapshot.uid,
        gid: snapshot.gid,
        mode: snapshot.mode,
        acl: snapshot.acl,
    })
}

pub(crate) struct StagingRecovery {
    pub(crate) operation: String,
    pub(crate) paths: Vec<String>,
    pub(crate) ftps_root: Option<(crate::remote::FtpsRootIdentity, [u8; 32])>,
}

/// Unter der gemeinsamen Prozesssperre lesen, aber niemals allein aufgrund eines
/// alten Journals löschen oder einen alten Permit wiederbeleben. Ein laufendes
/// anderes Fenster hält seine zusätzliche Staging-Sperre und bleibt ausgeschlossen.
#[cfg(test)]
pub(crate) fn recover_queue(queue: &Path) -> io::Result<Vec<StagingRecovery>> {
    let config = RootHandle::open_nofollow(
        queue
            .parent()
            .ok_or_else(|| io::Error::other("Queue ohne Elternordner"))?,
    )?;
    recover_queue_beneath(queue, &config)
}

pub(crate) fn recover_queue_beneath(
    queue: &Path,
    config: &RootHandle,
) -> io::Result<Vec<StagingRecovery>> {
    if queue.parent() != Some(config.path()) || !config.is_current()? {
        return Err(io::Error::other("Queue-Recoveryordner wurde ausgetauscht"));
    }
    if !config.has_exclusive_namespace()? {
        if crate::local_fs::list_beneath(config, config.path())?
            .iter()
            .any(|entry| {
                (entry.name.starts_with(".stage.doppeldecker-stage-")
                    || entry.name.starts_with(".neighbor.doppeldecker-recovery-"))
                    && entry.name.ends_with(".recovery.json")
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Recoveryindex verlor seinen gegen fremdes Schreiben geschützten Namensraum",
            ));
        }
        return Ok(Vec::new());
    }
    let mut recovered = Vec::new();
    for entry in crate::local_fs::list_beneath(config, config.path())? {
        if entry.name.starts_with(".neighbor.doppeldecker-recovery-")
            && entry.name.ends_with(".recovery.json")
        {
            if !entry.is_file || entry.is_symlink {
                return Err(io::Error::other(
                    "Temp-Recoveryindex ist keine reguläre Datei",
                ));
            }
            let mut data = Vec::new();
            crate::local_fs::open_read_beneath(config, &config.path().join(&entry.name))?
                .take(64 * 1024 + 1)
                .read_to_end(&mut data)?;
            if data.len() > 64 * 1024 {
                return Err(io::Error::other(
                    "Temp-Recoveryindex überschreitet Größenlimit",
                ));
            }
            let record: NeighborRecord = serde_json::from_slice(&data).map_err(io::Error::other)?;
            if record.owner.queue != queue {
                continue;
            }
            if !matches!(record.version, 1 | 2)
                || record.owner.operation.is_empty()
                || !record.target.is_absolute()
                || (record.version == 1 && record.ftps.is_some())
                || (record.version == 2
                    && (!record.remote || record.ftps.is_none() || record.candidate.is_some()))
                || (record.version == 1
                    && record
                        .candidate
                        .as_ref()
                        .is_some_and(|p| p.parent() != record.target.parent()))
            {
                return Err(io::Error::other("Ungültiger Temp-Recoveryvertrag"));
            }
            if let Some(binding) = record.ftps {
                validate_ftps_intent(&binding, &record.target)?;
                if record.owner.operation.len() != 32
                    || !record
                        .owner
                        .operation
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(io::Error::other("Ungültiger FTPS-Intentowner"));
                }
                let mut paths = binding.paths;
                paths.push(config.path().join(&entry.name));
                paths.push(record.target.clone());
                if binding.stou_name_unknown {
                    if let Some(parent) = record.target.parent() {
                        paths.push(parent.to_path_buf());
                    }
                }
                paths.sort();
                paths.dedup();
                recovered.push(StagingRecovery {
                    operation: record.owner.operation,
                    paths: paths
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect(),
                    ftps_root: Some((binding.roots[0].clone(), binding.account_digest)),
                });
                continue;
            }
            if let Some(candidate) = record.candidate {
                let mut paths = vec![candidate.to_string_lossy().into_owned()];
                if !record.remote {
                    paths.push(apple_double(&candidate).to_string_lossy().into_owned());
                }
                paths.push(record.target.to_string_lossy().into_owned());
                recovered.push(StagingRecovery {
                    operation: record.owner.operation,
                    paths,
                    ftps_root: None,
                });
            }
            continue;
        }
        if !entry.is_file
            || entry.is_symlink
            || !entry.name.starts_with(".stage.doppeldecker-stage-")
            || !entry.name.ends_with(".recovery.json")
        {
            continue;
        }
        let mut data = Vec::new();
        crate::local_fs::open_read_beneath(config, &config.path().join(&entry.name))?
            .take(64 * 1024 + 1)
            .read_to_end(&mut data)?;
        if data.len() > 64 * 1024 {
            return Err(io::Error::other(
                "Staging-Recoveryindex überschreitet Größenlimit",
            ));
        }
        let index: Journal = serde_json::from_slice(&data).map_err(io::Error::other)?;
        let Some(owner) = &index.owner else {
            continue;
        };
        if owner.queue != queue {
            continue;
        }
        if index.version != 1 || owner.operation.is_empty() {
            return Err(io::Error::other("Unbekannter Staging-Recoveryvertrag"));
        }
        let stage_path = &index.staging_identity.display_path;
        let mut recovery = StagingRecovery {
            operation: owner.operation.clone(),
            paths: Vec::new(),
            ftps_root: None,
        };
        let validated = (|| -> io::Result<RootHandle> {
            let anchor = RootHandle::open_nofollow(&index.anchor_identity.display_path)?;
            if !anchor.is_private()?
                || crate::local_fs::root_identity_beneath(&anchor, anchor.path())?
                    != index.anchor_identity
            {
                return Err(io::Error::other("Staging-Ankeridentität änderte sich"));
            }
            if stage_path.parent() != Some(anchor.path()) {
                return Err(io::Error::other(
                    "Staging liegt außerhalb des manifestierten Ankers",
                ));
            }
            let stage = RootHandle::open_beneath(&anchor, stage_path)?;
            if !stage.is_private()?
                || crate::local_fs::root_identity_beneath(&stage, stage_path)?
                    != index.staging_identity
            {
                return Err(io::Error::other("Staging-Identität änderte sich"));
            }
            Ok(stage)
        })();
        let stage = match validated {
            Ok(stage) => stage,
            Err(_) => {
                recovery
                    .paths
                    .push(stage_path.to_string_lossy().into_owned());
                recovered.push(recovery);
                continue;
            }
        };
        let lease = match openat(
            stage.fd(),
            "lease",
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(_) => {
                // Ein Abbruch beim abschließenden Entfernen darf weder blindes
                // Aufräumen noch den Verlust des privaten Recoveryhinweises bewirken.
                recovery
                    .paths
                    .push(stage_path.to_string_lossy().into_owned());
                recovered.push(recovery);
                continue;
            }
        };
        match rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => continue,
            Err(error) => return Err(error.into()),
        }
        let mut data = Vec::new();
        let journal_file =
            match crate::local_fs::open_read_beneath(&stage, &stage_path.join("journal.json")) {
                Ok(file) => file,
                Err(_) => {
                    recovery
                        .paths
                        .push(stage_path.to_string_lossy().into_owned());
                    recovered.push(recovery);
                    continue;
                }
            };
        journal_file.take(1024 * 1024 + 1).read_to_end(&mut data)?;
        if data.len() > 1024 * 1024 {
            return Err(io::Error::other("Stagingjournal überschreitet Größenlimit"));
        }
        let journal: Journal = serde_json::from_slice(&data).map_err(io::Error::other)?;
        if journal.version != index.version
            || journal.staging_identity != index.staging_identity
            || journal
                .owner
                .as_ref()
                .map(|owner| (&owner.queue, &owner.operation))
                != index
                    .owner
                    .as_ref()
                    .map(|owner| (&owner.queue, &owner.operation))
        {
            return Err(io::Error::other(
                "Stagingjournal passt nicht zum Recoveryindex",
            ));
        }
        for item in crate::local_fs::list_beneath(&stage, stage_path)? {
            if !matches!(
                item.name.as_str(),
                "journal.json" | "journal.next" | "lease"
            ) {
                recovery
                    .paths
                    .push(stage_path.join(item.name).to_string_lossy().into_owned());
            }
        }
        if recovery.paths.is_empty()
            && journal
                .actions
                .last()
                .is_none_or(|action| action.phase != "complete")
        {
            recovery
                .paths
                .push(stage_path.to_string_lossy().into_owned());
        }
        if !recovery.paths.is_empty() {
            recovered.push(recovery);
        }
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    fn ftps_binding() -> super::FtpsIntentBinding {
        super::FtpsIntentBinding {
            roots: vec![crate::remote::FtpsRootIdentity {
                server: crate::remote::FtpsServerIdentity {
                    host: "127.0.0.1".into(),
                    port: 2121,
                    transport_security: crate::remote::FtpTransportSecurity::Plaintext,
                    certificate: None,
                    canonical_root: "/server".into(),
                    features_digest: [1; 32],
                    welcome_digest: [2; 32],
                    legacy_welcome_digest: [0; 32],
                },
                virtual_root: "/pane".into(),
                session_generation: 1,
            }],
            account_digest: [3; 32],
            phase: "stou".into(),
            stou_name_unknown: true,
            paths: vec!["/server/pane/target".into()],
        }
    }

    #[test]
    fn remote_intent_recovery_erhaelt_v1_und_bindet_unbekannten_stou_namen() {
        use super::*;
        let (_fixture, root) = sandbox();
        let config = RootHandle::open_nofollow(&root).unwrap();
        let queue = root.join("queue-a.json");
        let owner = StagingOwner {
            queue: queue.clone(),
            operation: "a".repeat(32),
        };
        let legacy = RemoteTempIntent::create(
            &owner,
            Path::new("/old/target"),
            Path::new("/old/candidate"),
        )
        .unwrap();
        let current = RemoteTempIntent::create_ftps(
            &owner,
            &config,
            Path::new("/server/pane/target"),
            ftps_binding(),
        )
        .unwrap();
        let recovered = recover_queue(&queue).unwrap();
        assert_eq!(recovered.len(), 2);
        assert!(recovered
            .iter()
            .any(|record| record.ftps_root.is_none()
                && record.paths.contains(&"/old/candidate".into())));
        let unknown = recovered
            .iter()
            .find(|record| record.ftps_root.is_some())
            .unwrap();
        assert!(unknown.paths.contains(&"/server/pane".into()));
        assert!(unknown
            .paths
            .contains(&current.path().to_string_lossy().into_owned()));
        assert!(recover_queue(&root.join("queue-b.json"))
            .unwrap()
            .is_empty());
        current
            .ftps_stou_name(Path::new("/server/pane/acknowledged"))
            .unwrap();
        assert!(!current.ftps_recovery().1);
        assert!(current
            .ftps_recovery()
            .0
            .contains(&"/server/pane/acknowledged".into()));
        let before = std::fs::read(current.path()).unwrap();
        assert!(current
            .ftps_stou_name(Path::new("/server/foreign"))
            .is_err());
        assert_eq!(std::fs::read(current.path()).unwrap(), before);
        current.finish().unwrap();
        legacy.finish().unwrap();
        assert!(recover_queue(&queue).unwrap().is_empty());
    }

    #[test]
    fn ftps_intent_recovery_verweigert_vermischte_versionen_und_symlink_index() {
        use super::*;
        let (_fixture, root) = sandbox();
        let config = RootHandle::open_nofollow(&root).unwrap();
        let queue = root.join("queue.json");
        let owner = StagingOwner {
            queue: queue.clone(),
            operation: "b".repeat(32),
        };
        let intent = RemoteTempIntent::create_ftps(
            &owner,
            &config,
            Path::new("/server/pane/target"),
            ftps_binding(),
        )
        .unwrap();
        let valid: serde_json::Value =
            serde_json::from_slice(&std::fs::read(intent.path()).unwrap()).unwrap();
        for (key, value) in [
            ("version", serde_json::json!(3)),
            ("remote", serde_json::json!(false)),
            ("candidate", serde_json::json!("/server/pane/candidate")),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            std::fs::write(intent.path(), serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(recover_queue(&queue).is_err(), "Ungültiger Vertrag: {key}");
        }
        std::fs::remove_file(intent.path()).unwrap();
        let foreign = root.join("foreign.json");
        std::fs::write(&foreign, serde_json::to_vec(&valid).unwrap()).unwrap();
        std::os::unix::fs::symlink(&foreign, intent.path()).unwrap();
        assert!(recover_queue(&queue).is_err());
        assert!(foreign.is_file());
        assert!(std::fs::symlink_metadata(intent.path())
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn nachbartempindex_bindet_fensterqueue_und_bleibt_nur_nutzerlesbar() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let fixture = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let config = root.join("config");
        let target = root.join("target");
        std::fs::create_dir(&config).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o755)).unwrap();
        let queue = config.join("queue-window-a.json");
        let stage = super::NeighborStaging::create(
            &super::StagingOwner {
                queue: queue.clone(),
                operation: "op-a".into(),
            },
            &target.join("Datei.txt"),
            None,
        )
        .unwrap()
        .unwrap();
        let temp = target.join(".Datei.txt.doppeldecker-tmp-own");
        stage.candidate(&temp).unwrap();
        assert_eq!(
            std::fs::metadata(stage.path()).unwrap().mode() & 0o777,
            0o600
        );
        assert!(super::recover_queue(&config.join("queue-window-b.json"))
            .unwrap()
            .is_empty());
        let recovery = super::recover_queue(&queue).unwrap();
        assert_eq!(recovery.len(), 1);
        assert_eq!(recovery[0].operation, "op-a");
        assert!(recovery[0]
            .paths
            .contains(&temp.to_string_lossy().into_owned()));
        // Abwesenheit ist nur nach echtem NotFound belegt; eine erhaltene Datei
        // darf der Indexabschluss nicht löschen.
        std::fs::write(&temp, b"erhalten").unwrap();
        assert!(stage.finish().is_err());
        assert_eq!(std::fs::read(&temp).unwrap(), b"erhalten");
        std::fs::remove_file(&temp).unwrap();
        stage.finish().unwrap();
        assert!(super::recover_queue(&queue).unwrap().is_empty());
    }

    #[test]
    fn c2_vorbereitungsfehler_erhaelt_pruefpfad_und_fehlercode() {
        let temp = tempfile::tempdir().unwrap();
        let stage = temp.path().join("private-stage");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join("lease"), b"eigener Beleg").unwrap();
        std::fs::create_dir(stage.join("journal.next")).unwrap();
        std::fs::write(stage.join("journal.next/datenrest"), b"erhalten").unwrap();
        // Der echte Journal-Schreibfehler wird nach schon erfolgreicher Leaseanlage
        // durch denselben Vorbereitungswrapper wie im Produktionsweg geführt.
        let error = prepare_with_recovery(&stage, || {
            std::fs::write(stage.join("journal.next"), b"Journal")
        })
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::Io);
        let path = stage.to_string_lossy().into_owned();
        assert!(error.to_string().contains(&path));
        assert_eq!(crate::error::recovery_paths(&error), vec![path.clone()]);
        assert_eq!(
            std::fs::read(stage.join("journal.next/datenrest")).unwrap(),
            b"erhalten"
        );
        let error = prepare_with_recovery::<()>(&stage, || {
            Err(crate::coded_error(
                io::ErrorKind::StorageFull,
                crate::ErrorCode::StorageFull,
                "injizierter Journal-Syncfehler",
            ))
        })
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::StorageFull);
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert_eq!(crate::error::recovery_paths(&error), vec![path]);
    }

    use super::*;
    use crate::vfs::{LocalVfs, Vfs};
    use std::os::unix::fs::PermissionsExt;

    fn sandbox() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::local_fs::write_acl_exact(&root, None).unwrap();
        std::fs::write(root.join(".doppeldecker-test-sandbox"), b"C2").unwrap();
        (temp, root)
    }

    #[test]
    fn staging_mit_ungeschuetztem_queueordner_bleibt_ohne_nebenwirkung() {
        let (_temp, root) = sandbox();
        let parent = root.join("ziel");
        let config = root.join("config");
        std::fs::create_dir(&parent).unwrap();
        std::fs::create_dir(&config).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o777)).unwrap();
        let vfs = LocalVfs::rooted(&root)
            .unwrap()
            .with_staging_owner(&config.join("operations-queue.json"), "op-test");
        assert!(vfs
            .staging_scope(&parent.join("neu"), Some(&root))
            .unwrap()
            .is_none());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 3);
        assert_eq!(std::fs::read_dir(config).unwrap().count(), 0);
    }

    #[test]
    fn staging_haelt_namen_bei_eltern_swap_und_entfernt_nur_eigenen_temp() {
        let (_temp, root) = sandbox();
        let parent = root.join("ziel");
        std::fs::create_dir(&parent).unwrap();
        let target = parent.join("fertig");
        let vfs = LocalVfs::rooted(&root).unwrap();
        let scoped = vfs.staging_scope(&target, Some(&root)).unwrap().unwrap();
        assert!(!scoped.supports_remove_if_matches());
        let candidate = scoped.staging_candidate(&target, "tmp").unwrap();
        scoped
            .open_write_new(&candidate)
            .unwrap()
            .write_all(b"eigen")
            .unwrap();
        let stage = candidate.parent().unwrap();
        let moved = root.join("verschobenes-stage");
        std::fs::rename(stage, &moved).unwrap();
        std::fs::create_dir(stage).unwrap();
        let sentinel = stage.join(candidate.file_name().unwrap());
        std::fs::write(&sentinel, b"fremd").unwrap();
        scoped.cleanup_owned_temp(&candidate).unwrap();
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"fremd");
        assert!(!moved.join(candidate.file_name().unwrap()).exists());
        let new_temp = scoped.staging_candidate(&target, "tmp").unwrap();
        scoped
            .open_write_new(&new_temp)
            .unwrap()
            .write_all(b"neu")
            .unwrap();
        std::fs::rename(&parent, root.join("gehaltenes-ziel")).unwrap();
        std::fs::create_dir(&parent).unwrap();
        let error = scoped.rename_noreplace(&new_temp, &target).unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert!(!target.exists());
        assert!(!root.join("gehaltenes-ziel/fertig").exists());
        scoped.cleanup_owned_temp(&new_temp).unwrap();
    }

    #[test]
    fn staging_vererbt_wie_direkte_kernelanlage() {
        let (_temp, root) = sandbox();
        let parent = root.join("ziel");
        std::fs::create_dir(&parent).unwrap();
        let target = parent.join("neu");
        #[cfg(target_os = "macos")]
        let acl = Acl::Nfs4(crate::metadata::Nfs4Acl {
            entries: vec![crate::metadata::Nfs4AclEntry {
                qualifier: [0; 16],
                kind: crate::metadata::Nfs4AceKind::Allow,
                permissions: 2,
                flags: 32 | 64,
            }],
            flags: 0,
        });
        #[cfg(target_os = "linux")]
        let acl = {
            use crate::metadata::{PosixAclEntry, PosixAclTag::*};
            let entry = |tag, id, permissions| PosixAclEntry {
                tag,
                id,
                permissions,
            };
            Acl::Posix(PosixAcl {
                access: vec![],
                default: vec![
                    entry(UserObject, None, 7),
                    entry(User, Some(65534), 7),
                    entry(GroupObject, None, 7),
                    entry(Mask, None, 5),
                    entry(Other, None, 0),
                ],
            })
        };
        crate::local_fs::write_acl_exact(&parent, Some(&acl)).unwrap();
        std::fs::File::create(parent.join("direkt")).unwrap();
        let vfs = LocalVfs::rooted(&root).unwrap();
        let scoped = vfs.staging_scope(&target, Some(&root)).unwrap().unwrap();
        let candidate = scoped.staging_candidate(&target, "tmp").unwrap();
        scoped.open_write_new(&candidate).unwrap();
        let direct = vfs.metadata_manifest(&parent.join("direkt")).unwrap();
        let staged = scoped.metadata_manifest(&candidate).unwrap();
        assert_eq!(
            (staged.acl, staged.mode, staged.owner),
            (direct.acl, direct.mode, direct.owner)
        );
        scoped.cleanup_owned_temp(&candidate).unwrap();
    }

    #[test]
    fn c2_kopiert_beide_typkonflikte_mit_getrennter_erfolgsbilanz() {
        use crate::fileops::{FileOperationRequest, OverwritePolicy};
        for source_directory in [false, true] {
            let (_temp, root) = sandbox();
            let source = root.join("quelle/eintrag");
            let destination = root.join("ziel");
            std::fs::create_dir(root.join("quelle")).unwrap();
            std::fs::create_dir(&destination).unwrap();
            let target = destination.join("eintrag");
            if source_directory {
                std::fs::create_dir(&source).unwrap();
                std::fs::write(source.join("neu.txt"), b"neu").unwrap();
                std::fs::write(&target, b"alt").unwrap();
            } else {
                std::fs::write(&source, b"neu").unwrap();
                std::fs::create_dir(&target).unwrap();
                std::fs::write(target.join("alt.txt"), b"alt").unwrap();
            }
            let vfs = LocalVfs::rooted(&root).unwrap();
            let stats = crate::fileops::copy_items(
                FileOperationRequest {
                    src: &vfs,
                    dst: &vfs,
                    items: std::slice::from_ref(&source),
                    dst_dir: &destination,
                    policy: OverwritePolicy::Overwrite,
                    sandbox: &Some(root.clone()),
                },
                &crate::Cancel::new(),
                &mut |_| {},
            )
            .unwrap();
            assert_eq!((stats.files, stats.bytes, stats.cancelled), (1, 3, false));
            if source_directory {
                assert_eq!(std::fs::read(target.join("neu.txt")).unwrap(), b"neu");
                assert!(stats.retained_backup_paths.is_empty());
                assert!(stats.temp_recovery_paths.is_empty());
                assert!(!std::fs::read_dir(&root).unwrap().any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".stage.doppeldecker-stage-")));
            } else {
                assert_eq!(std::fs::read(&target).unwrap(), b"neu");
                assert_eq!(stats.retained_backup_paths.len(), 1);
                assert_eq!(stats.temp_recovery_paths, stats.retained_backup_paths);
                assert_eq!(
                    std::fs::read(Path::new(&stats.retained_backup_paths[0]).join("alt.txt"))
                        .unwrap(),
                    b"alt"
                );
            }
            assert!(!vfs.supports_remove_if_matches());
        }
    }

    #[test]
    fn c2_abbruch_entfernt_private_temps_und_behaelt_altziel() {
        use crate::fileops::{FileOperationRequest, OverwritePolicy};
        let (_temp, root) = sandbox();
        let source = root.join("quelle/eintrag");
        let destination = root.join("ziel");
        std::fs::create_dir(root.join("quelle")).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(&source, vec![7; 256 * 1024]).unwrap();
        std::fs::write(destination.join("eintrag"), b"alt").unwrap();
        let vfs = LocalVfs::rooted(&root).unwrap();
        let cancel = crate::Cancel::new();
        let stats = crate::fileops::copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &Some(root.clone()),
            },
            &cancel,
            &mut |progress| {
                if progress.bytes_done > 0 {
                    cancel.cancel();
                }
            },
        )
        .unwrap();
        assert!(stats.cancelled);
        assert_eq!(stats.files, 0);
        assert!(stats.temp_recovery_paths.is_empty());
        assert_eq!(std::fs::read(destination.join("eintrag")).unwrap(), b"alt");
        assert!(!std::fs::read_dir(&root).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".stage.doppeldecker-stage-")));
    }

    #[test]
    fn c2_vorbereitung_folgt_keinem_getauschten_eltern_symlink() {
        let (_temp, root) = sandbox();
        let (_outside_temp, outside) = sandbox();
        std::os::unix::fs::symlink(&outside, root.join("ziel")).unwrap();
        let before = std::fs::read_dir(&outside).unwrap().count();
        assert!(LocalVfs::new()
            .staging_scope(&root.join("ziel/datei"), None)
            .is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), before);
    }

    #[test]
    fn c2_acl_drift_stoppt_vor_dem_temp_commit() {
        let (_temp, root) = sandbox();
        std::fs::create_dir(root.join("ziel")).unwrap();
        let target = root.join("ziel/datei");
        let vfs = LocalVfs::rooted(&root).unwrap();
        let scoped = vfs.staging_scope(&target, Some(&root)).unwrap().unwrap();
        let candidate = scoped.staging_candidate(&target, "tmp").unwrap();
        scoped
            .open_write_new(&candidate)
            .unwrap()
            .write_all(b"neu")
            .unwrap();
        std::fs::set_permissions(root.join("ziel"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let error = scoped.rename_noreplace(&candidate, &target).unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert!(!target.exists());
        scoped.cleanup_owned_temp(&candidate).unwrap();
        let error = scoped.finish_staging().unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
    }

    #[test]
    fn c2_spaeter_eltern_swap_wird_nicht_als_erfolg_bilanziert() {
        let (_temp, root) = sandbox();
        let source = root.join("quelle/datei");
        let parent = root.join("ziel");
        std::fs::create_dir(root.join("quelle")).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(&source, b"neu").unwrap();
        let vfs = LocalVfs::rooted(&root).unwrap();
        let moved = root.join("verschobenes-ziel");
        let mut swapped = false;
        let error = crate::fileops::copy_items(
            crate::FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &[source],
                dst_dir: &parent,
                policy: crate::OverwritePolicy::Overwrite,
                sandbox: &Some(root.clone()),
            },
            &crate::Cancel::new(),
            &mut |progress| {
                if progress.files_done == 1 && !swapped {
                    std::fs::rename(&parent, &moved).unwrap();
                    std::fs::create_dir(&parent).unwrap();
                    std::fs::write(parent.join("sentinel"), b"fremd").unwrap();
                    swapped = true;
                }
            },
        )
        .unwrap_err();
        assert!(swapped);
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(parent.join("sentinel")).unwrap(), b"fremd");
        assert_eq!(std::fs::read(moved.join("datei")).unwrap(), b"neu");
        assert!(!parent.join("datei").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn c2_macos_acl_matrix_entspricht_kernelanlage() {
        for flags in 0..16 {
            for kind in [
                crate::metadata::Nfs4AceKind::Allow,
                crate::metadata::Nfs4AceKind::Deny,
            ] {
                let (_temp, root) = sandbox();
                let parent = root.join("ziel");
                std::fs::create_dir(&parent).unwrap();
                let acl = Acl::Nfs4(crate::metadata::Nfs4Acl {
                    entries: vec![crate::metadata::Nfs4AclEntry {
                        qualifier: [0; 16],
                        kind,
                        permissions: 2,
                        flags: flags << 5,
                    }],
                    flags: 0,
                });
                crate::local_fs::write_acl_exact(&parent, Some(&acl)).unwrap();
                let vfs = LocalVfs::rooted(&root).unwrap();
                let target = parent.join("datei");
                std::fs::File::create(parent.join("direkt")).unwrap();
                let scoped = vfs.staging_scope(&target, Some(&root)).unwrap().unwrap();
                let candidate = scoped.staging_candidate(&target, "tmp").unwrap();
                scoped.open_write_new(&candidate).unwrap();
                let direct = vfs.metadata_manifest(&parent.join("direkt")).unwrap();
                let staged = scoped.metadata_manifest(&candidate).unwrap();
                assert_eq!(
                    (staged.acl, staged.mode, staged.owner),
                    (direct.acl, direct.mode, direct.owner),
                    "flags={flags}, kind={kind:?}"
                );
            }
        }
    }

    #[test]
    fn c2_recovery_schliesst_aktive_fenster_aus_und_erhaelt_cleanup_luecken() {
        let (_temp, root) = sandbox();
        std::fs::create_dir(root.join("config")).unwrap();
        std::fs::set_permissions(root.join("config"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::create_dir(root.join("ziel")).unwrap();
        let queue = root.join("config/operations-queue-eigen.json");
        let owner = StagingOwner {
            queue: queue.clone(),
            operation: "eigen".into(),
        };
        let handle = RootHandle::open(&root).unwrap();
        let stage = LocalStaging::create(
            Some(&handle),
            Some(&handle),
            &root.join("ziel/datei"),
            Some(&root),
            Some(&owner),
            None,
        )
        .unwrap()
        .unwrap();
        let stage_path = stage.directory.path().to_path_buf();
        assert!(recover_queue(&queue).unwrap().is_empty());
        assert!(
            recover_queue(&root.join("config/operations-queue-anderes.json"))
                .unwrap()
                .is_empty()
        );
        stage.record("complete", &stage_path, None).unwrap();
        drop(stage);
        // Echter Zwischenzustand der abschließenden Bereinigung: Journal schon
        // entfernt, Lease noch vorhanden; anschließend auch die Lease entfernt.
        std::fs::remove_file(stage_path.join("journal.json")).unwrap();
        for remove_lease in [false, true] {
            if remove_lease {
                std::fs::remove_file(stage_path.join("lease")).unwrap();
            }
            let recovery = recover_queue(&queue).unwrap();
            assert_eq!(recovery.len(), 1);
            assert_eq!(
                recovery[0].paths,
                vec![stage_path.to_string_lossy().into_owned()]
            );
            assert!(stage_path.is_dir());
        }
    }

    #[test]
    fn c2_crash_child() {
        let Ok(root) = std::env::var("DOPPELDECKER_C2_CRASH_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        assert_eq!(
            std::fs::read(root.join(".doppeldecker-test-sandbox")).unwrap(),
            b"C2"
        );
        let operation = std::env::var("DOPPELDECKER_C2_CRASH_OPERATION").unwrap();
        let phase = std::env::var("DOPPELDECKER_C2_CRASH_PHASE").unwrap();
        let queue = root.join("config/operations-queue-crash.json");
        let target = root.join("ziel/eintrag");
        let vfs = LocalVfs::rooted(&root)
            .unwrap()
            .with_staging_owner(&queue, &operation);
        let scoped = vfs.staging_scope(&target, Some(&root)).unwrap().unwrap();
        let temp = scoped.staging_candidate(&target, "tmp").unwrap();
        scoped
            .open_write_new(&temp)
            .unwrap()
            .write_all(b"neu")
            .unwrap();
        scoped.sync_file(&temp).unwrap();
        if phase != "temp" {
            let backup = scoped.staging_candidate(&target, "replaced").unwrap();
            scoped.rename_noreplace(&target, &backup).unwrap();
            scoped.sync_dir(target.parent().unwrap()).unwrap();
            scoped.sync_dir(backup.parent().unwrap()).unwrap();
            if phase == "commit" {
                scoped.rename_noreplace(&temp, &target).unwrap();
                scoped.sync_file(&target).unwrap();
                scoped.sync_dir(target.parent().unwrap()).unwrap();
            }
        }
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGKILL) }, 0);
        loop {
            // SIGKILL wird asynchron zugestellt; keine reguläre Destruktorphase.
            unsafe {
                libc::pause();
            }
        }
    }

    #[test]
    fn c2_crash_recovery_bewahrt_daten_und_bindet_die_fensterqueue_frisch() {
        use std::os::unix::process::ExitStatusExt;
        for phase in ["temp", "park", "commit"] {
            let (_temp, root) = sandbox();
            std::fs::create_dir(root.join("config")).unwrap();
            std::fs::set_permissions(root.join("config"), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            std::fs::create_dir(root.join("ziel")).unwrap();
            std::fs::create_dir(root.join("ziel/eintrag")).unwrap();
            std::fs::write(root.join("ziel/eintrag/alt.txt"), b"alt").unwrap();
            let queue_path = root.join("config/operations-queue-crash.json");
            let queue = crate::OperationQueue::open(&queue_path).unwrap();
            let entry = queue
                .enqueue(crate::QueueOperation::File {
                    kind: crate::FileQueueKind::Copy,
                    items: vec![root.join("quelle").to_string_lossy().into_owned()],
                    destination: root.join("ziel").to_string_lossy().into_owned(),
                    overwrite: true,
                    expected_conflicts: vec![],
                    keep_both_targets: Vec::new(),
                    expected_conflict_proofs: vec![],
                    local_replace_best_effort_expected: Some(false),
                    local_copy_new_best_effort_expected: None,
                })
                .unwrap();
            queue.claim_next().unwrap().unwrap();
            queue.mark_running(&entry.operation_id).unwrap();
            drop(queue);
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "local_staging::tests::c2_crash_child",
                    "--nocapture",
                ])
                .env("DOPPELDECKER_C2_CRASH_ROOT", &root)
                .env("DOPPELDECKER_C2_CRASH_OPERATION", &entry.operation_id)
                .env("DOPPELDECKER_C2_CRASH_PHASE", phase)
                .status()
                .unwrap();
            assert_eq!(status.signal(), Some(libc::SIGKILL));
            let reopened = crate::OperationQueue::open(&queue_path).unwrap();
            let recovered = &reopened.snapshot().entries[0];
            assert_eq!(recovered.state, crate::QueueState::Interrupted);
            assert_eq!(recovered.error_code, Some(crate::ErrorCode::OutcomeUnclear));
            assert_eq!(
                recovered.temp_recovery_paths.len(),
                if phase == "park" { 2 } else { 1 }
            );
            if phase == "temp" {
                assert_eq!(
                    std::fs::read(root.join("ziel/eintrag/alt.txt")).unwrap(),
                    b"alt"
                );
            } else {
                let backup = recovered
                    .temp_recovery_paths
                    .iter()
                    .find(|path| path.contains("doppeldecker-replaced"))
                    .unwrap();
                assert_eq!(
                    std::fs::read(Path::new(backup).join("alt.txt")).unwrap(),
                    b"alt"
                );
                if phase == "park" {
                    assert!(!root.join("ziel/eintrag").exists());
                } else {
                    assert_eq!(std::fs::read(root.join("ziel/eintrag")).unwrap(), b"neu");
                }
            }
        }
    }
}
