//! Descriptor-relative lokale Dateisystemzugriffe für macOS und Linux.
//!
//! Eine reine Reihenfolge aus `canonicalize(path)` und anschließendem `std::fs`-Zugriff
//! hat ein unvermeidbares Zeitfenster: Ein anderer Prozess kann einen geprüften
//! Elternordner dazwischen durch einen Symlink ersetzen. Alle Funktionen dieses Moduls
//! öffnen deshalb jede Pfadkomponente mit `openat(..., O_NOFOLLOW | O_DIRECTORY)` und
//! führen die eigentliche Operation relativ zum geöffneten Eltern-Deskriptor aus.
//!
//! Ein einmal geöffneter Deskriptor bleibt an genau dieses Verzeichnis gebunden, selbst
//! wenn dessen sichtbarer Pfad danach umbenannt oder durch einen Symlink ersetzt wird.
//! Damit kann ein Symlink-Swap die Operation höchstens sauber scheitern lassen, aber
//! nicht aus der geprüften Verzeichniskette herauslenken.

#[cfg(target_os = "macos")]
use std::ffi::CStr;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use rustix::fs::{
    fchmod, fchown, fgetxattr, flistxattr, fremovexattr, fsetxattr, fstat, mkdirat, open, openat,
    renameat, renameat_with, seek, statat, unlinkat, AtFlags, Dir, FileType, Gid, Mode, OFlags,
    RenameFlags, SeekFrom, Stat, Uid, XattrFlags,
};
use rustix::io::dup;

#[cfg(target_os = "linux")]
use rustix::fs::{statx, StatxFlags};

use crate::metadata::{
    BlobSummary, DataExtent, MetadataManifest, NamedBlobSummary, Owner, XattrValue,
};
use crate::vfs::{entry_extension, Entry, LocalMountIdentity, LocalRootIdentity, ProofListEntry};

mod metadata_capabilities;

pub(crate) struct FileCapabilities {
    pub commit_class: crate::vfs::FileCommitClass,
    pub mode_write: bool,
    pub file_flush: bool,
    pub directory_flush: bool,
}

pub(crate) fn file_capabilities(
    root: Option<&RootHandle>,
    parent: &Path,
) -> io::Result<FileCapabilities> {
    let directory = open_dir_nofollow_from(root, parent, false)?;
    let before = mount_identity_from_fd(&directory)?;
    #[cfg(target_os = "macos")]
    let (assured, mode_write, flush) = {
        let caps = metadata_capabilities::volume_capabilities(&directory)?;
        let rename = libc::VOL_CAP_INT_RENAME_EXCL;
        let no_permissions = libc::VOL_CAP_FMT_NO_PERMISSIONS;
        (
            caps.valid[1] & rename != 0 && caps.capabilities[1] & rename != 0,
            caps.valid[0] & no_permissions != 0 && caps.capabilities[0] & no_permissions == 0,
            matches!(before.fs_type.as_str(), "apfs" | "hfs" | "exfat" | "msdos"),
        )
    };
    #[cfg(target_os = "linux")]
    let (assured, mode_write, flush) = {
        let filesystem = metadata_capabilities::linux_filesystem_name(&directory)?;
        // Diese Kernel-Treiber unterstützen RENAME_NOREPLACE; FUSE- und
        // unbekannte Remote-Mounts erhalten keine pauschale lokale Zusage.
        (
            matches!(
                filesystem.as_str(),
                "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "tmpfs" | "overlay" | "exfat" | "vfat"
            ),
            !matches!(filesystem.as_str(), "exfat" | "vfat" | "msdos"),
            matches!(
                filesystem.as_str(),
                "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "tmpfs" | "overlay" | "exfat" | "vfat"
            ),
        )
    };
    if mount_identity_from_fd(&directory)? != before {
        return Err(io::Error::other(
            "Mount änderte sich während der Fähigkeitsabfrage",
        ));
    }
    let capabilities = FileCapabilities {
        commit_class: if assured {
            crate::vfs::FileCommitClass::Assured
        } else {
            crate::vfs::FileCommitClass::BestEffort
        },
        mode_write,
        file_flush: flush,
        directory_flush: flush,
    };
    #[cfg(test)]
    let capabilities = WEAK_FILE_CAPABILITIES_ROOT.with(|root| {
        if root
            .borrow()
            .as_ref()
            .is_some_and(|root| parent.starts_with(root))
        {
            FileCapabilities {
                commit_class: crate::vfs::FileCommitClass::BestEffort,
                file_flush: false,
                directory_flush: false,
                ..capabilities
            }
        } else {
            capabilities
        }
    });
    Ok(capabilities)
}

#[cfg(test)]
thread_local! {
    static WEAK_FILE_CAPABILITIES_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

// Der Dateigegenlauf nutzt echte native Inodes mit künstlich schwächeren
// Zielgarantien. Die Abweichung gilt nur im Testthread und unter seinem Zielroot.
#[cfg(test)]
pub(crate) fn with_weak_file_capabilities_for_test<T>(root: &Path, test: impl FnOnce() -> T) -> T {
    struct Reset(Option<PathBuf>);
    impl Drop for Reset {
        fn drop(&mut self) {
            WEAK_FILE_CAPABILITIES_ROOT.with(|root| *root.borrow_mut() = self.0.take());
        }
    }
    let _reset =
        Reset(WEAK_FILE_CAPABILITIES_ROOT.with(|current| current.replace(Some(root.into()))));
    test()
}

pub(crate) fn metadata_capabilities(
    root: Option<&RootHandle>,
    path: &Path,
) -> io::Result<crate::metadata::MetadataCapabilities> {
    let directory = open_dir_nofollow_from(root, path, false)?;
    let before = mount_identity_from_fd(&directory)?;
    let mut capabilities = metadata_capabilities::read(&directory, &before)?;
    #[cfg(target_os = "macos")]
    let readonly = before.mount_flags & libc::MNT_RDONLY as u64 != 0;
    #[cfg(target_os = "linux")]
    let readonly = before.mount_flags & libc::ST_RDONLY != 0;
    if readonly {
        for capability in [
            &mut capabilities.owner,
            &mut capabilities.special_mode_bits,
            &mut capabilities.xattrs,
            &mut capabilities.resource_fork,
            &mut capabilities.sparse_extents,
            &mut capabilities.hardlinks,
        ] {
            if *capability == crate::metadata::Capability::Exact {
                *capability = crate::metadata::Capability::ReadOnly;
            }
        }
    }
    if mount_identity_from_fd(&directory)? != before {
        return Err(io::Error::other(
            "Mount änderte sich während der Metadatenfähigkeitsabfrage",
        ));
    }
    Ok(capabilities)
}

#[cfg(test)]
thread_local! {
    /// Deterministischer Testpunkt zwischen rekursivem Leeren und Leerheits-Readback.
    /// Thread-lokal verhindert er, dass parallel laufende Tests einander beeinflussen.
    static PURGE_AFTER_REMOVE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

/// Rechte für neu angelegte Dateien/Ordner. Die Prozess-`umask` reduziert sie wie bei
/// `std::fs`; wir umgehen also keine normale Plattform- oder Nutzervorgabe.
const FILE_MODE: Mode = Mode::from_raw_mode(0o666);
const DIR_MODE: Mode = Mode::from_raw_mode(0o777);
const XATTR_RETRIES: usize = 3;
const MAX_XATTR_LIST_BYTES: usize = 1024 * 1024;
const MAX_XATTR_VALUE_BYTES: usize = 16 * 1024 * 1024;

/// Gehaltene lokale Wurzel für einen Sync-Lauf. Der Deskriptor bleibt auch dann an
/// demselben Ordner, wenn sein sichtbarer Name zwischenzeitlich ersetzt wird.
#[derive(Clone)]
pub(crate) struct RootHandle {
    path: PathBuf,
    directory: Arc<OwnedFd>,
    device: u64,
    inode: u64,
    mount: LocalMountIdentity,
}

impl RootHandle {
    pub(crate) fn fd(&self) -> &OwnedFd {
        &self.directory
    }

    pub(crate) fn has_exclusive_namespace(&self) -> io::Result<bool> {
        if !self.has_native_permission_model()? {
            return Ok(false);
        }
        let stat = fstat(self.fd())?;
        if stat.st_uid != unsafe { libc::geteuid() } || stat.st_mode & 0o022 != 0 {
            return Ok(false);
        }
        Ok(match crate::acl::read_fd(self.fd())? {
            None => true,
            Some(crate::metadata::Acl::Nfs4(acl)) => acl.entries.is_empty(),
            Some(crate::metadata::Acl::Posix(acl)) => acl.access.iter().all(|entry| {
                entry.tag == crate::metadata::PosixAclTag::UserObject || entry.permissions & 2 == 0
            }),
        })
    }

    /// Ein privater Elternordner verhindert schon während mkdir/open fremde
    /// Kindzugriffe. 0700 allein genügt auf macOS wegen zusätzlicher ACLs nicht.
    pub(crate) fn is_private(&self) -> io::Result<bool> {
        if !self.has_native_permission_model()? {
            return Ok(false);
        }
        let stat = fstat(self.fd())?;
        if stat.st_uid != unsafe { libc::geteuid() } || stat.st_mode & 0o077 != 0 {
            return Ok(false);
        }
        Ok(match crate::acl::read_fd(self.fd())? {
            None => true,
            Some(crate::metadata::Acl::Nfs4(acl)) => acl.entries.is_empty(),
            Some(crate::metadata::Acl::Posix(acl)) => acl.access.iter().all(|entry| {
                entry.tag == crate::metadata::PosixAclTag::UserObject || entry.permissions == 0
            }),
        })
    }

    fn has_native_permission_model(&self) -> io::Result<bool> {
        let current = mount_identity_from_fd(self.fd())?;
        if current != self.mount {
            return Ok(false);
        }
        // Eine lokal eingehängte Freigabe ist kein Beweis für lokale Unix-Rechte.
        // Unbekannte ACL-/Owner-Modelle erhalten deshalb keine C2-Namensraumzusage.
        #[cfg(target_os = "macos")]
        return Ok(matches!(current.fs_type.as_str(), "apfs" | "hfs")
            && current.mount_flags & libc::MNT_IGNORE_OWNERSHIP as u64 == 0);
        #[cfg(target_os = "linux")]
        Ok(matches!(
            current.fs_type.as_str(),
            "0xef53" | "0x58465342" | "0x9123683e" | "0x1021994" | "0x794c7630"
        ))
    }

    pub(crate) fn create_private_child(&self, path: &Path) -> io::Result<Self> {
        if !self.is_private()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Staging-Elternordner ist nicht privat",
            ));
        }
        let (parent, name) = open_parent_from(Some(self), path)?;
        mkdirat(&parent, &name, Mode::from_raw_mode(0o700))?;
        let child = Self::open_beneath(self, path)?;
        crate::acl::write_fd_exact(child.fd(), None)?;
        fchmod(child.fd(), Mode::from_raw_mode(0o700))?;
        File::from(dup(child.fd())?).sync_all()?;
        File::from(parent).sync_all()?;
        if !child.is_private()? || child.mount != self.mount {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Staging konnte nicht privat auf demselben Mount gebunden werden",
            ));
        }
        Ok(child)
    }
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let path = std::fs::canonicalize(path)?;
        Self::open_nofollow(&path)
    }

    /// Eine bereits validierte Kindkomponente niemals erneut kanonisieren:
    /// sonst könnte ein Symlink-Swap den neuen Laufkontext außerhalb des Roots binden.
    pub(crate) fn open_nofollow(path: &Path) -> io::Result<Self> {
        let directory = open_dir_nofollow(path, false)?;
        let stat = fstat(&directory)?;
        let mount = mount_identity_from_fd(&directory)?;
        Ok(Self {
            path: path.to_path_buf(),
            directory: Arc::new(directory),
            device: stat.st_dev as u64,
            inode: stat.st_ino,
            mount,
        })
    }

    pub(crate) fn open_beneath(root: &RootHandle, path: &Path) -> io::Result<Self> {
        let directory = open_dir_nofollow_from(Some(root), path, false)?;
        let stat = fstat(&directory)?;
        let mount = mount_identity_from_fd(&directory)?;
        Ok(Self {
            path: path.to_path_buf(),
            directory: Arc::new(directory),
            device: stat.st_dev as u64,
            inode: stat.st_ino,
            mount,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn identity(&self) -> (u64, u64) {
        (self.device, self.inode)
    }

    /// Auch ein gehaltener Ordner verliert bei einem Mountwechsel seine alte
    /// Berechtigung. Der Papierkorb bindet diesen frischen Beleg zusätzlich im Journal.
    pub(crate) fn current_mount_identity(&self) -> io::Result<LocalMountIdentity> {
        let current = mount_identity_from_fd(self.fd())?;
        if current != self.mount {
            return Err(io::Error::other(
                "Mount des gehaltenen Ordners änderte sich",
            ));
        }
        Ok(current)
    }

    fn matches_root_identity(&self, expected: &LocalRootIdentity) -> bool {
        expected.is_directory
            && self.device == expected.device
            && self.inode == expected.inode
            && self.mount == expected.mount
    }

    /// Prüft, ob der sichtbare Pfad noch auf genau den gehaltenen Ordner zeigt.
    /// Die eigentlichen Zugriffe bleiben unabhängig davon descriptor-verankert.
    pub(crate) fn is_current(&self) -> io::Result<bool> {
        let visible = open_dir_nofollow(&self.path, false)?;
        let stat = fstat(&visible)?;
        Ok(stat.st_dev as u64 == self.device
            && stat.st_ino == self.inode
            && mount_identity_from_fd(&visible)? == self.mount)
    }
}

/// Mountmerkmale ausschließlich über den bereits gehaltenen Deskriptor lesen.
#[cfg(target_os = "linux")]
pub(crate) fn mount_identity_from_fd(fd: &OwnedFd) -> io::Result<LocalMountIdentity> {
    const STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;
    let mut statfs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(fd.as_raw_fd(), &mut statfs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut statx: libc::statx = unsafe { std::mem::zeroed() };
    let mask = libc::STATX_MNT_ID | STATX_MNT_ID_UNIQUE;
    if unsafe {
        libc::statx(
            fd.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            mask,
            &mut statx,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let unique_mount_id = statx.stx_mask & STATX_MNT_ID_UNIQUE != 0;
    if statx.stx_mask & libc::STATX_MNT_ID == 0 && !unique_mount_id {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Linux-Kernel liefert keine Mount-ID für den Realmodus",
        ));
    }
    let fsid = unsafe {
        std::slice::from_raw_parts(
            (&statfs.f_fsid as *const libc::fsid_t).cast::<u8>(),
            std::mem::size_of::<libc::fsid_t>(),
        )
        .to_vec()
    };
    let flags = statvfs_from_fd(fd)?.f_flag as u64;
    Ok(LocalMountIdentity {
        mount_id: statx.stx_mnt_id,
        unique_mount_id,
        fsid,
        volume_id: None,
        fs_type: format!("0x{:x}", statfs.f_type),
        mount_flags: flags,
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn mount_identity_from_fd(fd: &OwnedFd) -> io::Result<LocalMountIdentity> {
    let mut statfs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(fd.as_raw_fd(), &mut statfs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let fsid = unsafe {
        std::slice::from_raw_parts(
            (&statfs.f_fsid as *const libc::fsid_t).cast::<u8>(),
            std::mem::size_of::<libc::fsid_t>(),
        )
        .to_vec()
    };
    let mount_id = u64::from_le_bytes(
        fsid.iter()
            .copied()
            .chain(std::iter::repeat(0))
            .take(8)
            .collect::<Vec<_>>()
            .try_into()
            .expect("acht Bytes"),
    );
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    #[repr(C)]
    struct VolumeUuidBuffer {
        length: u32,
        uuid: [u8; 16],
    }
    let mut buffer = VolumeUuidBuffer {
        length: 0,
        uuid: [0; 16],
    };
    if unsafe {
        libc::fgetattrlist(
            fd.as_raw_fd(),
            (&mut attributes as *mut libc::attrlist).cast(),
            (&mut buffer as *mut VolumeUuidBuffer).cast(),
            std::mem::size_of::<VolumeUuidBuffer>(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if buffer.length as usize > std::mem::size_of::<VolumeUuidBuffer>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "macOS-Volume-ID-Antwort ist größer als der bereitgestellte Puffer",
        ));
    }
    let fs_type = unsafe { CStr::from_ptr(statfs.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Ok(LocalMountIdentity {
        mount_id,
        unique_mount_id: true,
        fsid,
        volume_id: Some(buffer.uuid),
        fs_type,
        mount_flags: statfs.f_flags as u64,
    })
}

fn statvfs_from_fd(fd: &OwnedFd) -> io::Result<libc::statvfs> {
    let mut statvfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatvfs(fd.as_raw_fd(), &mut statvfs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(statvfs)
}

fn available_bytes_from_fd(fd: &OwnedFd) -> io::Result<u64> {
    let stat = statvfs_from_fd(fd)?;
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

pub(crate) fn available_bytes(path: &Path) -> io::Result<u64> {
    let directory = open_dir_nofollow(path, false)?;
    available_bytes_from_fd(&directory)
}

pub(crate) fn available_bytes_beneath(root: &RootHandle, path: &Path) -> io::Result<u64> {
    let directory = open_dir_nofollow_from(Some(root), path, false)?;
    available_bytes_from_fd(&directory)
}

/// Verzeichnisflags für jede einzelne Pfadkomponente. `NOFOLLOW` ist hier die zentrale
/// Schranke: Eine Komponente darf niemals als Symlink durchlaufen werden.
pub(crate) fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// Liefert einen verständlichen Fehler für Pfade, die sich nicht sicher in Elternordner
/// und reinen Dateinamen zerlegen lassen (insbesondere `/`, `..` und Plattformpräfixe).
pub(crate) fn invalid_path(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "Pfad ist nicht descriptor-relativ nutzbar: {}",
            path.display()
        ),
    )
}

/// Öffnet einen Ordner ohne einem Symlink in irgendeiner Pfadkomponente zu folgen.
/// `create_missing` implementiert bei Bedarf ein ebenfalls descriptor-relatives
/// `create_dir_all`.
pub(crate) fn open_dir_nofollow(path: &Path, create_missing: bool) -> io::Result<OwnedFd> {
    open_dir_nofollow_from(None, path, create_missing)
}

/// Wie `open_dir_nofollow`, startet aber bei einer bereits gehaltenen Wurzel. Der
/// übergebene Pfad muss unter deren ursprünglichem absoluten Anzeigenpfad liegen.
fn open_dir_nofollow_from(
    root: Option<&RootHandle>,
    path: &Path,
    create_missing: bool,
) -> io::Result<OwnedFd> {
    let (mut current, relative) = if let Some(root) = root {
        let relative = path
            .strip_prefix(root.path())
            .map_err(|_| invalid_path(path))?;
        (dup(&*root.directory)?, relative)
    } else {
        let start = if path.is_absolute() { "/" } else { "." };
        (open(start, dir_flags(), Mode::empty())?, path)
    };

    for component in relative.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => return Err(invalid_path(path)),
        };

        current = match openat(&current, name, dir_flags(), Mode::empty()) {
            Ok(next) => next,
            Err(rustix::io::Errno::NOENT) if create_missing => {
                // Ein paralleler Erzeuger darf gewinnen. Danach öffnen wir den Namen
                // trotzdem erneut mit NOFOLLOW und prüfen dadurch, dass es ein echter
                // Ordner und kein eingeschleuster Symlink ist.
                match mkdirat(&current, name, DIR_MODE) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                openat(&current, name, dir_flags(), Mode::empty())?
            }
            Err(error) => return Err(error.into()),
        };
    }
    Ok(current)
}

/// Variante für Restore-Zielketten, nachdem der Aufrufer mindestens eine fehlende
/// Pfadkomponente beobachtet hat. Die Rückgabe hält jeden Ordner vom Zielroot bis
/// zum tiefsten Ziel-Elternordner. Das gilt auch, wenn ein anderer Prozess die
/// fehlende Kette im Fenster bis zu diesem erneuten Öffnen anlegt: Der Restore kann
/// sie nach dem Rename von innen nach außen synchronisieren und damit sowohl den
/// neuen Zieleintrag als auch jeden neu erschienenen Ordnernamen dauerhaft verankern.
fn open_restore_parent_after_missing_with_durability_chain(
    root: &RootHandle,
    path: &Path,
) -> io::Result<(OwnedFd, Vec<OwnedFd>)> {
    let relative = path
        .strip_prefix(root.path())
        .map_err(|_| invalid_path(path))?;
    let mut current = dup(&*root.directory)?;
    let mut chain = vec![dup(&current)?];

    for component in relative.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => return Err(invalid_path(path)),
        };
        let next = match openat(&current, name, dir_flags(), Mode::empty()) {
            Ok(next) => next,
            Err(rustix::io::Errno::NOENT) => {
                match mkdirat(&current, name, DIR_MODE) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                openat(&current, name, dir_flags(), Mode::empty())?
            }
            Err(error) => return Err(error.into()),
        };
        current = next;
        chain.push(dup(&current)?);
    }

    Ok((current, chain))
}

/// Öffnet den Elternordner und gibt den letzten Namen separat zurück. Alle mutierenden
/// `*at`-Operationen verwenden genau dieses Paar.
pub(crate) fn open_parent(path: &Path) -> io::Result<(OwnedFd, OsString)> {
    open_parent_from(None, path)
}

fn open_parent_from(root: Option<&RootHandle>, path: &Path) -> io::Result<(OwnedFd, OsString)> {
    let parent = path.parent().ok_or_else(|| invalid_path(path))?;
    let name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid_path(path))?
        .to_os_string();
    Ok((open_dir_nofollow_from(root, parent, false)?, name))
}

/// Einen finalen Pfad relativ zum bereits sicher geöffneten Elternordner öffnen.
fn open_final_from(
    root: Option<&RootHandle>,
    path: &Path,
    flags: OFlags,
    mode: Mode,
) -> io::Result<OwnedFd> {
    let (parent, name) = open_parent_from(root, path)?;
    // Ein zwischen Scan und open eingewechselter FIFO darf den Worker nicht
    // blockieren. Erst der gehaltene FD beweist den Typ; NOFOLLOW allein schützt
    // nur gegen Symlinks. NOCTTY verhindert die Bindung eines fremden Terminals.
    let entry = openat(
        parent,
        &name,
        flags | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY,
        mode,
    )?;
    let stat = fstat(&entry).map_err(|error| {
        if flags.intersects(OFlags::CREATE | OFlags::TRUNC) {
            crate::error::with_recovery_paths(
                crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!("Datei wurde geöffnet, ihr Typ ist nicht bestätigt ({error})"),
                ),
                &[path.to_string_lossy().into_owned()],
            )
        } else {
            error.into()
        }
    })?;
    let kind = FileType::from_raw_mode(stat.st_mode);
    if !kind.is_file() && !kind.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Lokaler Dateizugriff lehnt Spezialdateien ab",
        ));
    }
    Ok(entry)
}

/// Rustix-`stat` in das protokollneutrale VFS-DTO übersetzen.
// Rustix verwendet auf macOS und Linux unterschiedliche zugrunde liegende Typen;
// die expliziten Casts sind deshalb nur auf jeweils einer Plattform redundant.
#[allow(clippy::unnecessary_cast)]
fn entry_from_stat(name: String, stat: &Stat, created: Option<i64>) -> Entry {
    let kind = FileType::from_raw_mode(stat.st_mode);
    let is_file = kind.is_file();
    Entry {
        extension: entry_extension(&name, is_file),
        name,
        is_dir: kind.is_dir(),
        is_file,
        is_symlink: kind.is_symlink(),
        size: if is_file {
            stat.st_size.max(0) as u64
        } else {
            0
        },
        mtime: (stat.st_mtime >= 0).then_some(stat.st_mtime),
        created,
        owner: Some(user_display(stat.st_uid as u32)),
        group: Some(group_display(stat.st_gid as u32)),
        mode: Some((stat.st_mode & 0o777) as u32),
        device: Some(stat.st_dev as u64),
        inode: Some(stat.st_ino),
        hard_links: Some(stat.st_nlink as u64),
    }
}

static USER_NAMES: OnceLock<Mutex<std::collections::HashMap<u32, String>>> = OnceLock::new();
static GROUP_NAMES: OnceLock<Mutex<std::collections::HashMap<u32, String>>> = OnceLock::new();

fn user_display(uid: u32) -> String {
    cached_principal(&USER_NAMES, uid, lookup_user_name)
}

fn group_display(gid: u32) -> String {
    cached_principal(&GROUP_NAMES, gid, lookup_group_name)
}

fn cached_principal(
    cache: &OnceLock<Mutex<std::collections::HashMap<u32, String>>>,
    id: u32,
    lookup: fn(u32) -> Option<String>,
) -> String {
    let cache = cache.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some(value) = cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&id)
    {
        return value.clone();
    }
    let value = lookup(id).unwrap_or_else(|| id.to_string());
    cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(id, value.clone());
    value
}

fn principal_buffer_size(key: libc::c_int) -> usize {
    // POSIX erlaubt -1 als „unbestimmt". 16 KiB decken übliche lokale und
    // Verzeichnisdienst-Einträge ab; ERANGE wird unten bis 1 MiB vergrößert.
    let value = unsafe { libc::sysconf(key) };
    usize::try_from(value)
        .unwrap_or(16 * 1024)
        .clamp(1024, 1024 * 1024)
}

fn lookup_user_name(uid: u32) -> Option<String> {
    let mut size = principal_buffer_size(libc::_SC_GETPW_R_SIZE_MAX);
    loop {
        let mut record = std::mem::MaybeUninit::<libc::passwd>::zeroed();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_i8; size];
        let status = unsafe {
            libc::getpwuid_r(
                uid as libc::uid_t,
                record.as_mut_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && size < 1024 * 1024 {
            size = (size * 2).min(1024 * 1024);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let record = unsafe { record.assume_init() };
        if record.pw_name.is_null() {
            return None;
        }
        return Some(
            unsafe { std::ffi::CStr::from_ptr(record.pw_name) }
                .to_string_lossy()
                .into_owned(),
        );
    }
}

fn lookup_group_name(gid: u32) -> Option<String> {
    let mut size = principal_buffer_size(libc::_SC_GETGR_R_SIZE_MAX);
    loop {
        let mut record = std::mem::MaybeUninit::<libc::group>::zeroed();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_i8; size];
        let status = unsafe {
            libc::getgrgid_r(
                gid as libc::gid_t,
                record.as_mut_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && size < 1024 * 1024 {
            size = (size * 2).min(1024 * 1024);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let record = unsafe { record.assume_init() };
        if record.gr_name.is_null() {
            return None;
        }
        return Some(
            unsafe { std::ffi::CStr::from_ptr(record.gr_name) }
                .to_string_lossy()
                .into_owned(),
        );
    }
}

#[cfg(target_os = "macos")]
fn creation_time_at<Fd: std::os::fd::AsFd>(
    _directory: Fd,
    _name: &OsStr,
    stat: &Stat,
) -> Option<i64> {
    (stat.st_birthtime >= 0).then_some(stat.st_birthtime)
}

#[cfg(target_os = "linux")]
fn creation_time_at<Fd: std::os::fd::AsFd>(
    directory: Fd,
    name: &OsStr,
    _stat: &Stat,
) -> Option<i64> {
    let value = statx(
        directory,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::BTIME,
    )
    .ok()?;
    (value.stx_mask & StatxFlags::BTIME.bits() != 0 && value.stx_btime.tv_sec >= 0)
        .then_some(value.stx_btime.tv_sec)
}

pub(crate) fn list(path: &Path) -> io::Result<Vec<Entry>> {
    list_from(None, path)
}

pub(crate) fn list_beneath(root: &RootHandle, path: &Path) -> io::Result<Vec<Entry>> {
    list_from(Some(root), path)
}

pub(crate) fn list_complete(path: &Path) -> io::Result<Vec<ProofListEntry>> {
    list_complete_from(None, path)
}

pub(crate) fn list_complete_beneath(
    root: &RootHandle,
    path: &Path,
) -> io::Result<Vec<ProofListEntry>> {
    list_complete_from(Some(root), path)
}

/// Läuft einmal über einen descriptor-relativ geöffneten Ordner.
///
/// Der Ordner wird ohne Symlinkfolgen geöffnet und bleibt während des gesamten
/// Durchlaufs offen; jeder Eintrag wird gegen genau diesen Deskriptor gestatet.
/// `.` und `..` bleiben ausgespart. `build` bekommt neben dem fertigen `Entry`
/// auch den rohen Namen, damit ein Aufrufer die Dekodierbarkeit bilanzieren kann,
/// ohne den Durchlauf ein zweites Mal zu schreiben.
fn read_dir_entries<T>(
    root: Option<&RootHandle>,
    path: &Path,
    mut build: impl FnMut(&[u8], &OsStr, Entry) -> T,
) -> io::Result<Vec<T>> {
    read_dir_entries_bounded(root, path, usize::MAX, &mut build)
}

fn read_dir_entries_bounded<T>(
    root: Option<&RootHandle>,
    path: &Path,
    maximum: usize,
    mut build: impl FnMut(&[u8], &OsStr, Entry) -> T,
) -> io::Result<Vec<T>> {
    let directory = open_dir_nofollow_from(root, path, false)?;
    let mut out = Vec::new();
    for item in Dir::read_from(&directory)? {
        let item = item?;
        let bytes = item.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if out.len() >= maximum {
            return Err(crate::coded_error(
                io::ErrorKind::InvalidInput,
                crate::ErrorCode::InvalidInput,
                "Upload-Ordnerlisting überschreitet 1000 Einträge",
            ));
        }
        let name = OsStr::from_bytes(bytes);
        let stat = statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
        let created = creation_time_at(&directory, name, &stat);
        let entry = entry_from_stat(name.to_string_lossy().into_owned(), &stat, created);
        out.push(build(bytes, name, entry));
    }
    Ok(out)
}

fn list_from(root: Option<&RootHandle>, path: &Path) -> io::Result<Vec<Entry>> {
    read_dir_entries(root, path, |_raw_name, _name, entry| entry)
}

fn list_complete_from(root: Option<&RootHandle>, path: &Path) -> io::Result<Vec<ProofListEntry>> {
    read_dir_entries(root, path, |raw_name, name, entry| ProofListEntry {
        entry,
        raw_name: raw_name.to_vec(),
        safely_decoded: name.to_str().is_some(),
        provider_token: None,
    })
}

pub(crate) fn list_upload_beneath(
    root: &RootHandle,
    path: &Path,
) -> io::Result<Vec<ProofListEntry>> {
    read_dir_entries_bounded(Some(root), path, 1000, |raw, name, entry| ProofListEntry {
        entry,
        raw_name: raw.to_vec(),
        safely_decoded: name.to_str().is_some(),
        provider_token: None,
    })
}

pub(crate) fn stat(path: &Path) -> io::Result<Entry> {
    stat_from(None, path)
}

pub(crate) fn stat_beneath(root: &RootHandle, path: &Path) -> io::Result<Entry> {
    stat_from(Some(root), path)
}

fn stat_from(root: Option<&RootHandle>, path: &Path) -> io::Result<Entry> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stat = raw_stat_from(root, path)?;
    #[cfg(target_os = "macos")]
    let created = (stat.st_birthtime >= 0).then_some(stat.st_birthtime);
    #[cfg(target_os = "linux")]
    let created = None;
    Ok(entry_from_stat(name, &stat, created))
}

fn raw_stat_from(root: Option<&RootHandle>, path: &Path) -> io::Result<Stat> {
    let stat = if let Some(root) = root.filter(|root| path == root.path()) {
        fstat(dup(&*root.directory)?)?
    } else if path.file_name().is_some() {
        let (parent, final_name) = open_parent_from(root, path)?;
        statat(parent, final_name, AtFlags::SYMLINK_NOFOLLOW)?
    } else {
        let directory = open_dir_nofollow_from(root, path, false)?;
        fstat(directory)?
    };
    Ok(stat)
}

pub(crate) fn root_identity(path: &Path) -> io::Result<LocalRootIdentity> {
    let path = std::fs::canonicalize(path)?;
    let root = RootHandle::open(&path)?;
    root_identity_beneath(&root, &path)
}

pub(crate) fn root_identity_beneath(
    root: &RootHandle,
    path: &Path,
) -> io::Result<LocalRootIdentity> {
    let directory = open_dir_nofollow_from(Some(root), path, false)?;
    let stat = fstat(&directory)?;
    let kind = FileType::from_raw_mode(stat.st_mode);
    if !kind.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Realmodus-Wurzel ist kein Verzeichnis: {}", path.display()),
        ));
    }
    // Mountflags und IDs erneut am gehaltenen Handle lesen. Ein Wechsel seit dem
    // Öffnen wird dadurch nicht mit der gespeicherten Identität verwechselt.
    let mount = mount_identity_from_fd(&directory)?;
    Ok(LocalRootIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino,
        is_directory: true,
        mount,
        display_path: path.to_path_buf(),
    })
}

pub(crate) fn metadata_manifest(path: &Path) -> io::Result<MetadataManifest> {
    metadata_manifest_from(None, path)
}

pub(crate) fn metadata_manifest_beneath(
    root: &RootHandle,
    path: &Path,
) -> io::Result<MetadataManifest> {
    metadata_manifest_from(Some(root), path)
}

fn metadata_manifest_from(root: Option<&RootHandle>, path: &Path) -> io::Result<MetadataManifest> {
    let fd = open_metadata_entry_from(root, path)?;
    let mount = mount_identity_from_fd(&fd)?;
    let capabilities = metadata_capabilities::read(&fd, &mount)?;
    for _ in 0..XATTR_RETRIES {
        let before = fstat(&fd)?;
        // Nur eine ausdrücklich fehlende Dateisystemfähigkeit erlaubt die
        // Auslassung. Unbekannte Fähigkeiten und echte Lesefehler bleiben Fehler.
        let acl = if capabilities.acl_model == crate::metadata::AclModel::Unsupported {
            None
        } else {
            crate::acl::read_fd(&fd)?
        };
        let xattr_values = if capabilities.xattrs == crate::metadata::Capability::Unsupported {
            Vec::new()
        } else {
            read_xattrs_from_fd(&fd)?
        };
        let xattrs = xattr_values
            .into_iter()
            .map(|item| NamedBlobSummary {
                name: item.name,
                value: BlobSummary::from_bytes(&item.value),
            })
            .collect();
        let resource_fork =
            if capabilities.resource_fork == crate::metadata::Capability::Unsupported {
                None
            } else {
                resource_fork_summary_from_fd(&fd)?
            };
        let sparse_extents = if FileType::from_raw_mode(before.st_mode).is_file()
            && capabilities.sparse_extents != crate::metadata::Capability::Unsupported
        {
            Some(sparse_extents_from_fd(&fd, before.st_size.max(0) as u64)?)
        } else {
            None
        };
        let after = fstat(&fd)?;
        if before.st_ctime == after.st_ctime && before.st_ctime_nsec == after.st_ctime_nsec {
            if mount_identity_from_fd(&fd)? != mount {
                return Err(io::Error::other(
                    "Mount änderte sich während des Manifest-Scans",
                ));
            }
            return MetadataManifest::new(
                (capabilities.owner != crate::metadata::Capability::Unsupported).then_some(Owner {
                    uid: after.st_uid,
                    gid: after.st_gid,
                }),
                (after.st_mode & 0o7777) as u32,
                acl,
                xattrs,
                resource_fork,
                sparse_extents,
                None,
            );
        }
    }
    Err(io::Error::other(
        "Metadaten änderten sich fortlaufend während des Manifest-Scans",
    ))
}

/// Ermittelt die belegten Datenbereiche über `SEEK_DATA`/`SEEK_HOLE`. `ENXIO` ab
/// einem Loch bis Dateiende ist das reguläre Ende; fehlende Dateisystemunterstützung
/// wird dagegen ausdrücklich gemeldet und niemals als „vollständig sparse“ ausgegeben.
fn sparse_extents_from_fd(fd: &OwnedFd, size: u64) -> io::Result<Vec<DataExtent>> {
    let mut extents = Vec::new();
    let mut offset = 0u64;
    while offset < size {
        let data = match seek(fd, SeekFrom::Data(offset)) {
            Ok(data) => data,
            Err(error) if error.raw_os_error() == libc::ENXIO => break,
            Err(error) if matches!(error.raw_os_error(), libc::EINVAL | libc::ENOTSUP) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Dateisystem unterstützt SEEK_DATA/SEEK_HOLE nicht exakt",
                ));
            }
            Err(error) => return Err(error.into()),
        };
        if data >= size {
            break;
        }
        let hole = seek(fd, SeekFrom::Hole(data))?.min(size);
        if hole <= data {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SEEK_HOLE lieferte keinen vorwärts laufenden Datenbereich",
            ));
        }
        extents.push(DataExtent {
            offset: data,
            length: hole - data,
        });
        offset = hole;
    }
    Ok(extents)
}

pub(crate) fn read_xattrs(path: &Path) -> io::Result<Vec<XattrValue>> {
    read_xattrs_from(None, path)
}

pub(crate) fn read_xattrs_beneath(root: &RootHandle, path: &Path) -> io::Result<Vec<XattrValue>> {
    read_xattrs_from(Some(root), path)
}

fn read_xattrs_from(root: Option<&RootHandle>, path: &Path) -> io::Result<Vec<XattrValue>> {
    let fd = open_metadata_entry_from(root, path)?;
    read_xattrs_from_fd(&fd)
}

pub(crate) fn write_xattrs_exact(path: &Path, xattrs: &[XattrValue]) -> io::Result<()> {
    write_xattrs_exact_from(None, path, xattrs)
}

pub(crate) fn write_xattrs_exact_beneath(
    root: &RootHandle,
    path: &Path,
    xattrs: &[XattrValue],
) -> io::Result<()> {
    write_xattrs_exact_from(Some(root), path, xattrs)
}

fn write_xattrs_exact_from(
    root: Option<&RootHandle>,
    path: &Path,
    xattrs: &[XattrValue],
) -> io::Result<()> {
    let mut desired = xattrs.to_vec();
    desired.sort_by(|left, right| left.name.cmp(&right.name));
    validate_xattr_values(&desired)?;
    let fd = open_metadata_entry_from(root, path)?;
    let current = list_xattr_names(&fd)?;

    for name in current {
        if is_resource_fork(&name)
            || is_acl_xattr(&name)
            || desired.iter().any(|item| item.name == name)
        {
            continue;
        }
        fremovexattr(&fd, OsStr::from_bytes(&name))?;
    }
    for item in &desired {
        fsetxattr(
            &fd,
            OsStr::from_bytes(&item.name),
            &item.value,
            XattrFlags::empty(),
        )?;
    }

    let actual = read_xattrs_from_fd(&fd)?;
    if actual != desired {
        let describe = |items: &[XattrValue]| {
            items
                .iter()
                .map(|item| {
                    (
                        String::from_utf8_lossy(&item.name).into_owned(),
                        item.value.len(),
                    )
                })
                .collect::<Vec<_>>()
        };
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Extended Attributes stimmen nach dem Schreiben nicht exakt überein: erwartet {:?}, gelesen {:?}",
                describe(&desired),
                describe(&actual)
            ),
        ));
    }
    Ok(())
}

/// Datei oder Ordner einmal ohne Symlink-Folgen öffnen; alle xattr-Aufrufe eines
/// Snapshots arbeiten anschließend ausschließlich auf diesem Deskriptor.
fn open_metadata_entry_from(root: Option<&RootHandle>, path: &Path) -> io::Result<OwnedFd> {
    if let Some(root) = root.filter(|root| path == root.path()) {
        return Ok(dup(&*root.directory)?);
    }
    let stat = raw_stat_from(root, path)?;
    let kind = FileType::from_raw_mode(stat.st_mode);
    if kind.is_dir() {
        open_dir_nofollow_from(root, path, false)
    } else if kind.is_file() {
        open_final_from(root, path, OFlags::RDONLY, Mode::empty())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Extended Attributes werden nur für reguläre Dateien und Ordner erfasst",
        ))
    }
}

fn read_xattrs_from_fd(fd: &OwnedFd) -> io::Result<Vec<XattrValue>> {
    for _ in 0..XATTR_RETRIES {
        let before = fstat(fd)?;
        let names = list_xattr_names(fd)?;
        let Some(out) = read_xattrs_for_names(fd, &names)? else {
            continue;
        };
        let names_after = list_xattr_names(fd)?;
        if names != names_after {
            continue;
        }
        let Some(confirm) = read_xattrs_for_names(fd, &names_after)? else {
            continue;
        };
        let after = fstat(fd)?;
        if before.st_ctime == after.st_ctime
            && before.st_ctime_nsec == after.st_ctime_nsec
            && out == confirm
        {
            return Ok(out);
        }
    }
    Err(io::Error::other(
        "Extended Attributes änderten sich fortlaufend während des Lesens",
    ))
}

fn read_xattrs_for_names(fd: &OwnedFd, names: &[Vec<u8>]) -> io::Result<Option<Vec<XattrValue>>> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if is_resource_fork(name) || is_acl_xattr(name) {
            continue;
        }
        let value = match read_xattr_value(fd, name) {
            Ok(value) => value,
            Err(error) if is_missing_xattr(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        out.push(XattrValue {
            name: name.clone(),
            value,
        });
    }
    out.sort_by(|left, right| left.name.cmp(&right.name));
    validate_xattr_values(&out)?;
    Ok(Some(out))
}

fn list_xattr_names(fd: &OwnedFd) -> io::Result<Vec<Vec<u8>>> {
    for _ in 0..XATTR_RETRIES {
        let mut empty = [0u8; 0];
        let required = flistxattr(fd, &mut empty)?;
        if required == 0 {
            return Ok(Vec::new());
        }
        if required > MAX_XATTR_LIST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "xattr-Namensliste überschreitet die Sicherheitsgrenze",
            ));
        }
        let mut buffer = vec![0u8; required];
        match flistxattr(fd, &mut buffer) {
            Ok(written) => {
                if written == 0 {
                    continue;
                }
                buffer.truncate(written);
                if buffer.last() != Some(&0) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "xattr-Namensliste ist nicht nullterminiert",
                    ));
                }
                let mut names: Vec<Vec<u8>> = buffer[..buffer.len() - 1]
                    .split(|byte| *byte == 0)
                    .map(<[u8]>::to_vec)
                    .collect();
                names.sort();
                if names.iter().any(Vec::is_empty)
                    || names.windows(2).any(|pair| pair[0] == pair[1])
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "xattr-Namensliste enthält leere oder doppelte Namen",
                    ));
                }
                return Ok(names);
            }
            Err(rustix::io::Errno::RANGE) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::other(
        "xattr-Namensliste änderte sich fortlaufend",
    ))
}

fn read_xattr_value(fd: &OwnedFd, name: &[u8]) -> io::Result<Vec<u8>> {
    let name = OsStr::from_bytes(name);
    for _ in 0..XATTR_RETRIES {
        let mut empty = [0u8; 0];
        let required = fgetxattr(fd, name, &mut empty)?;
        if required > MAX_XATTR_VALUE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "xattr-Wert überschreitet die Sicherheitsgrenze",
            ));
        }
        if required == 0 {
            return Ok(Vec::new());
        }
        let mut value = vec![0u8; required];
        match fgetxattr(fd, name, &mut value) {
            Ok(written) => {
                value.truncate(written);
                return Ok(value);
            }
            Err(rustix::io::Errno::RANGE) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::other("xattr-Wert änderte sich fortlaufend"))
}

fn validate_xattr_values(xattrs: &[XattrValue]) -> io::Result<()> {
    for item in xattrs {
        if item.name.is_empty() || item.name.contains(&0) || is_resource_fork(&item.name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Ungültiger oder separat behandelter xattr-Name",
            ));
        }
        if item.value.len() > MAX_XATTR_VALUE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "xattr-Wert überschreitet die Sicherheitsgrenze",
            ));
        }
    }
    if xattrs.windows(2).any(|pair| pair[0].name >= pair[1].name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "xattrs müssen eindeutige Namen besitzen",
        ));
    }
    Ok(())
}

fn is_resource_fork(name: &[u8]) -> bool {
    cfg!(target_os = "macos") && name == b"com.apple.ResourceFork"
}

fn is_acl_xattr(name: &[u8]) -> bool {
    cfg!(target_os = "linux")
        && matches!(
            name,
            b"system.posix_acl_access" | b"system.posix_acl_default"
        )
}

pub(crate) fn write_acl_exact(path: &Path, acl: Option<&crate::metadata::Acl>) -> io::Result<()> {
    write_acl_exact_from(None, path, acl)
}

pub(crate) fn write_acl_exact_beneath(
    root: &RootHandle,
    path: &Path,
    acl: Option<&crate::metadata::Acl>,
) -> io::Result<()> {
    write_acl_exact_from(Some(root), path, acl)
}

fn write_acl_exact_from(
    root: Option<&RootHandle>,
    path: &Path,
    acl: Option<&crate::metadata::Acl>,
) -> io::Result<()> {
    let fd = open_metadata_entry_from(root, path)?;
    crate::acl::write_fd_exact(&fd, acl)
}

fn is_missing_xattr(error: &io::Error) -> bool {
    #[cfg(target_os = "macos")]
    let missing = libc::ENOATTR;
    #[cfg(target_os = "linux")]
    let missing = libc::ENODATA;
    error.raw_os_error() == Some(missing)
}

pub(crate) fn open_resource_fork_read(path: &Path) -> io::Result<Box<dyn Read + Send>> {
    open_resource_fork_read_from(None, path)
}

pub(crate) fn open_resource_fork_read_beneath(
    root: &RootHandle,
    path: &Path,
) -> io::Result<Box<dyn Read + Send>> {
    open_resource_fork_read_from(Some(root), path)
}

fn open_resource_fork_read_from(
    root: Option<&RootHandle>,
    path: &Path,
) -> io::Result<Box<dyn Read + Send>> {
    #[cfg(target_os = "macos")]
    {
        let fd = open_metadata_entry_from(root, path)?;
        Ok(Box::new(mac_resource_fork::Reader::open(fd)?))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (root, path);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Ressourcen-Forks werden auf dieser Plattform nicht unterstützt",
        ))
    }
}

pub(crate) fn open_resource_fork_write_new(path: &Path) -> io::Result<Box<dyn Write + Send>> {
    open_resource_fork_write_new_from(None, path)
}

pub(crate) fn open_resource_fork_write_new_beneath(
    root: &RootHandle,
    path: &Path,
) -> io::Result<Box<dyn Write + Send>> {
    open_resource_fork_write_new_from(Some(root), path)
}

fn open_resource_fork_write_new_from(
    root: Option<&RootHandle>,
    path: &Path,
) -> io::Result<Box<dyn Write + Send>> {
    #[cfg(target_os = "macos")]
    {
        let fd = open_metadata_entry_from(root, path)?;
        Ok(Box::new(mac_resource_fork::Writer::create(fd)?))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (root, path);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Ressourcen-Forks werden auf dieser Plattform nicht unterstützt",
        ))
    }
}

fn resource_fork_summary_from_fd(fd: &OwnedFd) -> io::Result<Option<BlobSummary>> {
    #[cfg(target_os = "macos")]
    {
        mac_resource_fork::stable_summary(fd)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = fd;
        Ok(None)
    }
}

#[cfg(target_os = "macos")]
mod mac_resource_fork {
    use super::*;

    const NAME: &[u8] = b"com.apple.ResourceFork\0";
    const STREAM_BUFFER: usize = 128 * 1024;

    pub(super) struct Reader {
        fd: OwnedFd,
        position: u32,
        remaining: u64,
    }

    impl Reader {
        pub(super) fn open(fd: OwnedFd) -> io::Result<Self> {
            let size = resource_size(&fd)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Datei besitzt keinen Ressourcen-Fork",
                )
            })?;
            if size > u32::MAX as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Ressourcen-Fork überschreitet den positionierbaren 32-Bit-Bereich",
                ));
            }
            Ok(Self {
                fd,
                position: 0,
                remaining: size,
            })
        }
    }

    impl Read for Reader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if buffer.is_empty() || self.remaining == 0 {
                return Ok(0);
            }
            let remaining = usize::try_from(self.remaining).unwrap_or(usize::MAX);
            let position_room = (u32::MAX - self.position) as usize;
            let wanted = buffer.len().min(remaining).min(position_room);
            if wanted == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Ressourcen-Fork überschreitet den positionierbaren 32-Bit-Bereich",
                ));
            }
            let read = unsafe {
                libc::fgetxattr(
                    self.fd.as_raw_fd(),
                    NAME.as_ptr().cast(),
                    buffer.as_mut_ptr().cast(),
                    wanted,
                    self.position,
                    0,
                )
            };
            if read < 0 {
                return Err(io::Error::last_os_error());
            }
            let read = read as usize;
            if read == 0 || read > wanted {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Ressourcen-Fork änderte sich während des Lesens",
                ));
            }
            self.position = self.position.checked_add(read as u32).ok_or_else(|| {
                io::Error::new(io::ErrorKind::Unsupported, "Ressourcen-Fork ist zu groß")
            })?;
            self.remaining -= read as u64;
            Ok(read)
        }
    }

    pub(super) struct Writer {
        fd: OwnedFd,
        position: u32,
        started: bool,
    }

    impl Writer {
        pub(super) fn create(fd: OwnedFd) -> io::Result<Self> {
            if resource_size(&fd)?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Ressourcen-Fork existiert bereits",
                ));
            }
            Ok(Self {
                fd,
                position: 0,
                started: false,
            })
        }
    }

    impl Write for Writer {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            if buffer.len() > (u32::MAX - self.position) as usize {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Ressourcen-Fork überschreitet den positionierbaren 32-Bit-Bereich",
                ));
            }
            let options = if self.started { 0 } else { libc::XATTR_CREATE };
            let result = unsafe {
                libc::fsetxattr(
                    self.fd.as_raw_fd(),
                    NAME.as_ptr().cast(),
                    buffer.as_ptr().cast(),
                    buffer.len(),
                    self.position,
                    options,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            self.position = self
                .position
                .checked_add(buffer.len() as u32)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::Unsupported, "Ressourcen-Fork ist zu groß")
                })?;
            self.started = true;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn resource_size(fd: &OwnedFd) -> io::Result<Option<u64>> {
        let size = unsafe {
            libc::fgetxattr(
                fd.as_raw_fd(),
                NAME.as_ptr().cast(),
                std::ptr::null_mut(),
                0,
                0,
                0,
            )
        };
        if size >= 0 {
            return Ok(Some(size as u64));
        }
        let error = io::Error::last_os_error();
        if is_missing_xattr(&error) {
            Ok(None)
        } else {
            Err(error)
        }
    }

    fn summary_once(fd: &OwnedFd) -> io::Result<Option<BlobSummary>> {
        let Some(size) = resource_size(fd)? else {
            return Ok(None);
        };
        let mut reader = Reader::open(dup(fd)?)?;
        let mut hash = blake3::Hasher::new();
        let mut total = 0u64;
        let mut buffer = vec![0u8; STREAM_BUFFER];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
            total += read as u64;
        }
        if total != size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Ressourcen-Fork änderte seine Größe während des Lesens",
            ));
        }
        Ok(Some(BlobSummary {
            size,
            digest: *hash.finalize().as_bytes(),
        }))
    }

    pub(super) fn stable_summary(fd: &OwnedFd) -> io::Result<Option<BlobSummary>> {
        for _ in 0..XATTR_RETRIES {
            let before = fstat(fd)?;
            let first = match summary_once(fd) {
                Ok(summary) => summary,
                Err(error) if is_missing_xattr(&error) => continue,
                Err(error) => return Err(error),
            };
            let second = match summary_once(fd) {
                Ok(summary) => summary,
                Err(error) if is_missing_xattr(&error) => continue,
                Err(error) => return Err(error),
            };
            let after = fstat(fd)?;
            if first == second
                && before.st_ctime == after.st_ctime
                && before.st_ctime_nsec == after.st_ctime_nsec
            {
                return Ok(first);
            }
        }
        Err(io::Error::other(
            "Ressourcen-Fork änderte sich fortlaufend während des Lesens",
        ))
    }
}

/// Testhelfer für einen einzelnen descriptor-relativen Drift-Eingriff. Produktcode
/// verwendet ausschließlich den exakten, rückgelesenen VFS-Schreibpfad.
#[cfg(test)]
pub(crate) fn set_xattr_for_test(path: &Path, name: &[u8], value: &[u8]) -> io::Result<()> {
    let fd = open_metadata_entry_from(None, path)?;
    fsetxattr(fd, OsStr::from_bytes(name), value, XattrFlags::empty())?;
    Ok(())
}

pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
    create_dir_all_from(None, path)
}

pub(crate) fn create_dir_all_beneath(root: &RootHandle, path: &Path) -> io::Result<()> {
    create_dir_all_from(Some(root), path)
}

fn create_dir_all_from(root: Option<&RootHandle>, path: &Path) -> io::Result<()> {
    open_dir_nofollow_from(root, path, true).map(drop)
}

/// Genau einen Ordner anlegen. Anders als `create_dir_all` scheitert diese Funktion
/// atomar mit `AlreadyExists`, wenn ein Konkurrent denselben Namen zuerst belegt.
pub(crate) fn create_dir(path: &Path) -> io::Result<()> {
    create_dir_from(None, path)
}

pub(crate) fn create_dir_beneath(root: &RootHandle, path: &Path) -> io::Result<()> {
    create_dir_from(Some(root), path)
}

fn create_dir_from(root: Option<&RootHandle>, path: &Path) -> io::Result<()> {
    let (parent, name) = open_parent_from(root, path)?;
    Ok(mkdirat(parent, name, DIR_MODE)?)
}

pub(crate) fn open_read(path: &Path) -> io::Result<File> {
    open_read_from(None, path)
}

pub(crate) fn open_read_beneath(root: &RootHandle, path: &Path) -> io::Result<File> {
    open_read_from(Some(root), path)
}

fn open_read_from(root: Option<&RootHandle>, path: &Path) -> io::Result<File> {
    Ok(File::from(open_final_from(
        root,
        path,
        OFlags::RDONLY,
        Mode::empty(),
    )?))
}

pub(crate) fn open_write(path: &Path) -> io::Result<File> {
    open_write_from(None, path)
}

pub(crate) fn open_write_beneath(root: &RootHandle, path: &Path) -> io::Result<File> {
    open_write_from(Some(root), path)
}

fn open_write_from(root: Option<&RootHandle>, path: &Path) -> io::Result<File> {
    Ok(File::from(open_final_from(
        root,
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
        FILE_MODE,
    )?))
}

pub(crate) fn open_write_new(path: &Path) -> io::Result<File> {
    open_write_new_from(None, path)
}

pub(crate) fn open_write_new_beneath(root: &RootHandle, path: &Path) -> io::Result<File> {
    open_write_new_from(Some(root), path)
}

fn open_write_new_from(root: Option<&RootHandle>, path: &Path) -> io::Result<File> {
    Ok(File::from(open_final_from(
        root,
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
        FILE_MODE,
    )?))
}

pub(crate) fn sync_file(path: &Path) -> io::Result<()> {
    sync_file_from(None, path)
}

pub(crate) fn sync_file_beneath(root: &RootHandle, path: &Path) -> io::Result<()> {
    sync_file_from(Some(root), path)
}

fn sync_file_from(root: Option<&RootHandle>, path: &Path) -> io::Result<()> {
    // Nach der Metadatenphase kann die Datei absichtlich schreibgeschützt sein. `fsync`
    // benötigt auf macOS/Linux keinen erneut schreibend geöffneten Deskriptor; RDONLY
    // hält den Abschluss deshalb auch für Quellmodi wie 0444 funktionsfähig.
    File::from(open_final_from(root, path, OFlags::RDONLY, Mode::empty())?).sync_all()
}

/// Verzeichniseinträge dauerhaft sichern. Das ist nach einem atomaren Rename nötig,
/// damit nicht nur die Datei, sondern auch ihr neuer Name einen Stromausfall überlebt.
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    sync_dir_from(None, path)
}

pub(crate) fn sync_dir_beneath(root: &RootHandle, path: &Path) -> io::Result<()> {
    sync_dir_from(Some(root), path)
}

fn sync_dir_from(root: Option<&RootHandle>, path: &Path) -> io::Result<()> {
    File::from(open_dir_nofollow_from(root, path, false)?).sync_all()
}

pub(crate) fn set_mtime(path: &Path, mtime: i64) -> io::Result<()> {
    set_mtime_from(None, path, mtime)
}

pub(crate) fn set_mtime_beneath(root: &RootHandle, path: &Path, mtime: i64) -> io::Result<()> {
    set_mtime_from(Some(root), path, mtime)
}

fn set_mtime_from(root: Option<&RootHandle>, path: &Path, mtime: i64) -> io::Result<()> {
    let file = File::from(open_final_from(root, path, OFlags::RDONLY, Mode::empty())?);
    let timestamp = filetime::FileTime::from_unix_time(mtime, 0);
    filetime::set_file_handle_times(&file, None, Some(timestamp))
}

/// POSIX-Rechte am sicher geöffneten Eintrag setzen. Verzeichnisse brauchen eigene
/// Öffnungsflags; reguläre Dateien werden ebenfalls mit `NOFOLLOW` geöffnet. Dadurch
/// kann ein eingeschleuster Symlink chmod nie auf sein externes Ziel lenken.
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    set_mode_from(None, path, mode)
}

pub(crate) fn set_mode_beneath(root: &RootHandle, path: &Path, mode: u32) -> io::Result<()> {
    set_mode_from(Some(root), path, mode)
}

fn set_mode_from(root: Option<&RootHandle>, path: &Path, mode: u32) -> io::Result<()> {
    let entry = stat_from(root, path)?;
    let fd = if entry.is_dir && !entry.is_symlink {
        open_dir_nofollow_from(root, path, false)?
    } else {
        open_final_from(root, path, OFlags::RDONLY, Mode::empty())?
    };
    // Sonderbits des vorhandenen Ziels bleiben erhalten. Bei neuen Temp-Dateien sind
    // sie null; setuid/setgid/sticky werden also nie ohne passenden Eigentümer kopiert.
    let existing_special = Mode::from_raw_mode(fstat(&fd)?.st_mode & 0o7000);
    let requested = Mode::from_raw_mode((mode & 0o777) as _);
    Ok(fchmod(fd, existing_special | requested)?)
}

pub(crate) fn set_metadata_owner(path: &Path, owner: Owner) -> io::Result<()> {
    set_metadata_owner_from(None, path, owner)
}

pub(crate) fn set_metadata_owner_beneath(
    root: &RootHandle,
    path: &Path,
    owner: Owner,
) -> io::Result<()> {
    set_metadata_owner_from(Some(root), path, owner)
}

fn set_metadata_owner_from(root: Option<&RootHandle>, path: &Path, owner: Owner) -> io::Result<()> {
    if owner.uid == u32::MAX || owner.gid == u32::MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "-1 ist keine übertragbare Nutzer- oder Gruppen-ID",
        ));
    }
    let fd = open_metadata_entry_from(root, path)?;
    fchown(
        &fd,
        Some(Uid::from_raw(owner.uid)),
        Some(Gid::from_raw(owner.gid)),
    )?;
    let actual = fstat(fd)?;
    if actual.st_uid != owner.uid || actual.st_gid != owner.gid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Eigentümer stimmt nach fchown nicht exakt überein",
        ));
    }
    Ok(())
}

pub(crate) fn set_metadata_normal_mode(path: &Path, mode: u32) -> io::Result<()> {
    set_metadata_normal_mode_from(None, path, mode)
}

pub(crate) fn set_metadata_normal_mode_beneath(
    root: &RootHandle,
    path: &Path,
    mode: u32,
) -> io::Result<()> {
    set_metadata_normal_mode_from(Some(root), path, mode)
}

fn set_metadata_normal_mode_from(
    root: Option<&RootHandle>,
    path: &Path,
    mode: u32,
) -> io::Result<()> {
    if mode & !0o777 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Normale Metadatenrechte dürfen keine Sonderbits enthalten",
        ));
    }
    let fd = open_metadata_entry_from(root, path)?;
    fchmod(&fd, Mode::from_raw_mode(mode as _))?;
    let actual = (fstat(fd)?.st_mode & 0o7777) as u32;
    if actual != mode {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Normale Rechte stimmen nach fchmod nicht exakt überein",
        ));
    }
    Ok(())
}

pub(crate) fn set_metadata_special_bits(path: &Path, bits: u32) -> io::Result<()> {
    set_metadata_special_bits_from(None, path, bits)
}

pub(crate) fn set_metadata_special_bits_beneath(
    root: &RootHandle,
    path: &Path,
    bits: u32,
) -> io::Result<()> {
    set_metadata_special_bits_from(Some(root), path, bits)
}

fn set_metadata_special_bits_from(
    root: Option<&RootHandle>,
    path: &Path,
    bits: u32,
) -> io::Result<()> {
    if bits & !0o7000 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Sonderbit-Phase akzeptiert nur setuid/setgid/sticky",
        ));
    }
    let fd = open_metadata_entry_from(root, path)?;
    let normal = (fstat(&fd)?.st_mode & 0o777) as u32;
    let requested = normal | bits;
    fchmod(&fd, Mode::from_raw_mode(requested as _))?;
    let actual = (fstat(fd)?.st_mode & 0o7777) as u32;
    if actual != requested {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Sonderbits stimmen nach fchmod nicht exakt überein",
        ));
    }
    Ok(())
}

pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    remove_file_from(None, path)
}

pub(crate) fn remove_file_beneath(root: &RootHandle, path: &Path) -> io::Result<()> {
    remove_file_from(Some(root), path)
}

fn remove_file_from(root: Option<&RootHandle>, path: &Path) -> io::Result<()> {
    let (parent, name) = open_parent_from(root, path)?;
    Ok(unlinkat(parent, name, AtFlags::empty())?)
}

/// Löscht den Inhalt eines bereits geöffneten Ordners. Namen werden stets relativ zu
/// `directory` verarbeitet; selbst ein während der Rekursion eingesetzter Symlink wird
/// deshalb nur als Link entfernt und niemals betreten.
fn remove_dir_contents(directory: &OwnedFd) -> io::Result<()> {
    let mut names = Vec::new();
    let entries = Dir::read_from(directory)?;
    for item in entries {
        let item = item?;
        let bytes = item.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(OsString::from_vec(bytes.to_vec()));
        }
    }

    for name in names {
        let current = match statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(current) => current,
            // Ein paralleler Entferner darf gewinnen; wichtig ist, niemals einem neu
            // eingesetzten Link zu folgen.
            Err(rustix::io::Errno::NOENT) => continue,
            Err(error) => return Err(error.into()),
        };
        if FileType::from_raw_mode(current.st_mode).is_dir() {
            let child = openat(directory, &name, dir_flags(), Mode::empty())?;
            remove_dir_contents(&child)?;
            drop(child);
            unlinkat(directory, &name, AtFlags::REMOVEDIR)?;
        } else {
            unlinkat(directory, &name, AtFlags::empty())?;
        }
    }
    Ok(())
}

pub(crate) fn remove_dir_all(path: &Path) -> io::Result<()> {
    remove_dir_all_from(None, path)
}

pub(crate) fn remove_dir_all_beneath(root: &RootHandle, path: &Path) -> io::Result<()> {
    remove_dir_all_from(Some(root), path)
}

pub(crate) fn remove_dir(path: &Path) -> io::Result<()> {
    remove_dir_from(None, path)
}

pub(crate) fn remove_dir_beneath(root: &RootHandle, path: &Path) -> io::Result<()> {
    remove_dir_from(Some(root), path)
}

fn remove_dir_from(root: Option<&RootHandle>, path: &Path) -> io::Result<()> {
    let (parent, name) = open_parent_from(root, path)?;
    Ok(unlinkat(parent, name, AtFlags::REMOVEDIR)?)
}

fn remove_dir_all_from(root: Option<&RootHandle>, path: &Path) -> io::Result<()> {
    let (parent, name) = open_parent_from(root, path)?;
    let directory = openat(&parent, &name, dir_flags(), Mode::empty())?;
    remove_dir_contents(&directory)?;
    drop(directory);
    Ok(unlinkat(parent, name, AtFlags::REMOVEDIR)?)
}

pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
    rename_from(None, from, to)
}

pub(crate) fn rename_beneath(root: &RootHandle, from: &Path, to: &Path) -> io::Result<()> {
    rename_from(Some(root), from, to)
}

fn rename_from(root: Option<&RootHandle>, from: &Path, to: &Path) -> io::Result<()> {
    rename_between(root, from, root, to, None)
}

pub(crate) fn rename_between(
    from_root: Option<&RootHandle>,
    from: &Path,
    to_root: Option<&RootHandle>,
    to: &Path,
    flags: Option<RenameFlags>,
) -> io::Result<()> {
    let (from_parent, from_name) = open_parent_from(from_root, from)?;
    let (to_parent, to_name) = open_parent_from(to_root, to)?;
    match flags {
        Some(flags) => Ok(renameat_with(
            from_parent,
            from_name,
            to_parent,
            to_name,
            flags,
        )?),
        None => Ok(renameat(from_parent, from_name, to_parent, to_name)?),
    }
}

/// Umbenennen ohne stilles Ersetzen. macOS (`RENAME_EXCL`) und Linux
/// (`RENAME_NOREPLACE`) stellen diese Prüfung atomar im Kernel bereit.
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    rename_noreplace_from(None, from, to)
}

pub(crate) fn rename_noreplace_beneath(
    root: &RootHandle,
    from: &Path,
    to: &Path,
) -> io::Result<()> {
    rename_noreplace_from(Some(root), from, to)
}

fn rename_noreplace_from(root: Option<&RootHandle>, from: &Path, to: &Path) -> io::Result<()> {
    rename_between(root, from, root, to, Some(RenameFlags::NOREPLACE))
}

/// Restore aus einem geöffneten Quarantänelauf. Der sichtbare Claimname darf nach
/// `open_beneath` ausgetauscht werden: Quellauflösung und Rename verwenden weiterhin
/// den gehaltenen Lauf-Deskriptor sowie den separat gehaltenen äußeren Root.
#[allow(clippy::too_many_arguments)]
pub(crate) fn restore_from_bound_quarantine_run(
    outer_root: Option<&RootHandle>,
    run_root: &Path,
    expected_run_root: &LocalRootIdentity,
    source: &Path,
    target_root: &Path,
    expected_target_root: &crate::real_mode::RootIdentity,
    target: &Path,
    expected_source: &ProofListEntry,
    intent_manifest: &Path,
    expected_intent_manifest: &ProofListEntry,
    expected_intent_digest: [u8; 32],
) -> Result<(), crate::vfs::MutationFailure> {
    use crate::vfs::MutationFailure;

    let run = match outer_root {
        Some(root) => RootHandle::open_beneath(root, run_root),
        None => RootHandle::open(run_root),
    }
    .map_err(MutationFailure::Unchanged)?;
    if !run.matches_root_identity(expected_run_root) {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Geclaimter Quarantänelauf wurde vor dem Restore ausgetauscht",
        )));
    }
    let crate::real_mode::RootIdentity::Local(expected_target_root) = expected_target_root else {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Lokaler Quarantäne-Restore erhielt keine lokale Zielrootidentität",
        )));
    };
    let target_handle = match outer_root {
        Some(root) => RootHandle::open_beneath(root, target_root),
        None => RootHandle::open(target_root),
    }
    .map_err(MutationFailure::Unchanged)?;
    if !target_handle.matches_root_identity(expected_target_root) {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Restore-Zielroot wurde vor der descriptor-gebundenen Wirkung ausgetauscht",
        )));
    }
    let (intent_parent, intent_name) =
        open_parent_from(outer_root, intent_manifest).map_err(MutationFailure::Unchanged)?;
    if expected_intent_manifest.raw_name != intent_name.as_bytes() {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::InvalidData,
            "Restore-Manifestbeleg gehört nicht zum gewählten Namen",
        )));
    }
    let mut intent_file = File::from(
        openat(
            &intent_parent,
            &intent_name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY,
            Mode::empty(),
        )
        .map_err(io::Error::from)
        .map_err(MutationFailure::Unchanged)?,
    );
    let intent_stat = fstat(&intent_file)
        .map_err(io::Error::from)
        .map_err(MutationFailure::Unchanged)?;
    if !stat_matches_proof(&intent_stat, expected_intent_manifest)
        || read_file_digest_bounded(
            &mut intent_file,
            4 * 1024 * 1024,
            "Quarantäne-Restore-Manifest überschreitet die Größenbegrenzung",
        )
        .map_err(MutationFailure::Unchanged)?
            != expected_intent_digest
    {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Quarantäne-Restore-Manifest änderte sich vor der Wirkung",
        )));
    }
    let (source_parent, source_name) =
        open_parent_from(Some(&run), source).map_err(MutationFailure::Unchanged)?;
    if expected_source.raw_name != source_name.as_bytes() {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::InvalidData,
            "Quarantäne-Quellbeleg gehört nicht zum gewählten Namen",
        )));
    }
    let current = statat(&source_parent, &source_name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io::Error::from)
        .map_err(MutationFailure::Unchanged)?;
    let current_kind = FileType::from_raw_mode(current.st_mode);
    if expected_source.entry.device != Some(current.st_dev as u64)
        || expected_source.entry.inode != Some(current.st_ino)
        || (expected_source.entry.is_dir && !current_kind.is_dir())
        || (expected_source.entry.is_file && !current_kind.is_file())
    {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Quarantäne-Eintrag wurde vor dem descriptor-gebundenen Restore ausgetauscht",
        )));
    }
    let target_parent_path = target
        .parent()
        .ok_or_else(|| MutationFailure::Unchanged(invalid_path(target)))?;
    let target_name = target
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| MutationFailure::Unchanged(invalid_path(target)))?
        .to_os_string();
    let (target_parent, durability_chain) =
        match open_dir_nofollow_from(Some(&target_handle), target_parent_path, false) {
            Ok(parent) => (parent, Vec::new()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                open_restore_parent_after_missing_with_durability_chain(
                    &target_handle,
                    target_parent_path,
                )
                .map_err(MutationFailure::OutcomeUnclear)?
            }
            Err(error) => return Err(MutationFailure::Unchanged(error)),
        };
    renameat_with(
        &source_parent,
        &source_name,
        &target_parent,
        &target_name,
        RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)
    .map_err(|error| {
        if !durability_chain.is_empty() {
            MutationFailure::OutcomeUnclear(error)
        } else {
            MutationFailure::Unchanged(error)
        }
    })?;
    let restored = statat(&target_parent, &target_name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io::Error::from)
        .map_err(MutationFailure::OutcomeUnclear)?;
    if expected_source.entry.device != Some(restored.st_dev as u64)
        || expected_source.entry.inode != Some(restored.st_ino)
    {
        return Err(MutationFailure::OutcomeUnclear(io::Error::other(
            "Restore-Ziel bezeichnet nach dem Rename nicht den bestätigten Quarantäneeintrag",
        )));
    }
    // Zuerst den wiederhergestellten Zieleintrag und danach jede möglicherweise
    // neu angelegte Elternkomponente von innen nach außen verankern. Erst wenn der
    // vollständige Zielpfad dauerhaft ist, darf der Quell-Elternordner die
    // Entfernung des Quarantänenamens dauerhaft bestätigen.
    rustix::fs::fsync(&target_parent)
        .map_err(io::Error::from)
        .map_err(MutationFailure::OutcomeUnclear)?;
    for directory in durability_chain.iter().rev() {
        rustix::fs::fsync(directory)
            .map_err(io::Error::from)
            .map_err(MutationFailure::OutcomeUnclear)?;
    }
    rustix::fs::fsync(&source_parent)
        .map_err(io::Error::from)
        .map_err(MutationFailure::OutcomeUnclear)?;
    let visible_intent = statat(&intent_parent, &intent_name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io::Error::from)
        .map_err(MutationFailure::OutcomeUnclear)?;
    seek(&intent_file, SeekFrom::Start(0))
        .map_err(io::Error::from)
        .map_err(MutationFailure::OutcomeUnclear)?;
    if !stat_matches_proof(&visible_intent, expected_intent_manifest)
        || read_file_digest_bounded(
            &mut intent_file,
            4 * 1024 * 1024,
            "Quarantäne-Restore-Manifest überschreitet die Größenbegrenzung",
        )
        .map_err(MutationFailure::OutcomeUnclear)?
            != expected_intent_digest
    {
        return Err(MutationFailure::OutcomeUnclear(io::Error::other(
            "Quarantäne-Restore-Manifest wurde während der Wirkung ausgetauscht oder verändert",
        )));
    }
    if !run.is_current().map_err(MutationFailure::OutcomeUnclear)?
        || !target_handle
            .is_current()
            .map_err(MutationFailure::OutcomeUnclear)?
    {
        return Err(MutationFailure::OutcomeUnclear(io::Error::other(
            "Quarantäne- oder Zielroot wurde während des Restore ausgetauscht",
        )));
    }
    Ok(())
}

/// Leert nur den über Gerät/Inode und Mount gebundenen Lauf. Der finale leere
/// Claimname bleibt stehen; POSIX bietet keinen `rmdir`, der denselben Identitätswert
/// in einer atomaren Bedingung prüft.
pub(crate) fn purge_bound_quarantine_run_contents(
    outer_root: Option<&RootHandle>,
    run_root: &Path,
    expected_run_root: &LocalRootIdentity,
    intent: &Path,
    expected_intent: &ProofListEntry,
    expected_intent_digest: [u8; 32],
) -> Result<(), crate::vfs::MutationFailure> {
    use crate::vfs::MutationFailure;

    let run = match outer_root {
        Some(root) => RootHandle::open_beneath(root, run_root),
        None => RootHandle::open(run_root),
    }
    .map_err(MutationFailure::Unchanged)?;
    if !run.matches_root_identity(expected_run_root) {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Geclaimter Quarantänelauf wurde vor dem Leeren ausgetauscht",
        )));
    }
    let (intent_parent, intent_name) =
        open_parent_from(outer_root, intent).map_err(MutationFailure::Unchanged)?;
    if expected_intent.raw_name != intent_name.as_bytes() {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::InvalidData,
            "Quarantäne-Purge-Intent-Beleg gehört nicht zum gewählten Namen",
        )));
    }
    let mut intent_file = File::from(
        openat(
            &intent_parent,
            &intent_name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY,
            Mode::empty(),
        )
        .map_err(io::Error::from)
        .map_err(MutationFailure::Unchanged)?,
    );
    let intent_stat = fstat(&intent_file)
        .map_err(io::Error::from)
        .map_err(MutationFailure::Unchanged)?;
    if !stat_matches_proof(&intent_stat, expected_intent)
        || read_small_file_digest(&mut intent_file).map_err(MutationFailure::Unchanged)?
            != expected_intent_digest
    {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Quarantäne-Purge-Intent wurde vor dem Leeren ausgetauscht oder verändert",
        )));
    }
    remove_dir_contents(&run.directory).map_err(MutationFailure::OutcomeUnclear)?;
    #[cfg(test)]
    PURGE_AFTER_REMOVE_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    let remaining = Dir::read_from(&*run.directory)
        .map_err(io::Error::from)
        .map_err(MutationFailure::OutcomeUnclear)?;
    for item in remaining {
        let item = item
            .map_err(io::Error::from)
            .map_err(MutationFailure::OutcomeUnclear)?;
        let name = item.file_name().to_bytes();
        if name != b"." && name != b".." {
            return Err(MutationFailure::OutcomeUnclear(io::Error::other(
                "Geclaimter Quarantänelauf ist nach dem Leeren nicht leer geblieben",
            )));
        }
    }
    rustix::fs::fsync(&*run.directory)
        .map_err(io::Error::from)
        .map_err(MutationFailure::OutcomeUnclear)?;
    let visible_intent = statat(&intent_parent, &intent_name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io::Error::from)
        .map_err(MutationFailure::OutcomeUnclear)?;
    seek(&intent_file, SeekFrom::Start(0))
        .map_err(io::Error::from)
        .map_err(MutationFailure::OutcomeUnclear)?;
    if !stat_matches_proof(&visible_intent, expected_intent)
        || read_small_file_digest(&mut intent_file).map_err(MutationFailure::OutcomeUnclear)?
            != expected_intent_digest
    {
        return Err(MutationFailure::OutcomeUnclear(io::Error::other(
            "Quarantäne-Purge-Intent wurde während des Leerens ausgetauscht oder verändert",
        )));
    }
    if !run.is_current().map_err(MutationFailure::OutcomeUnclear)? {
        return Err(MutationFailure::OutcomeUnclear(io::Error::other(
            "Quarantänelauf wurde während des Leerens ausgetauscht",
        )));
    }
    Ok(())
}

/// Abschließender Beleg über genau einen geöffneten Laufdeskriptor. Identitätsprüfung
/// und vollständiges Listing dürfen nicht in zwei Pfadauflösungen zerfallen: Sonst
/// könnte ein Writer dazwischen den bestätigten Lauf wegbenennen und einen leeren
/// Nachfolger unter denselben Namen setzen.
pub(crate) fn verify_bound_quarantine_run_empty(
    outer_root: Option<&RootHandle>,
    run_root: &Path,
    expected_run_root: &LocalRootIdentity,
    intent: &Path,
    expected_intent: &ProofListEntry,
    expected_intent_digest: [u8; 32],
) -> Result<(), crate::vfs::MutationFailure> {
    use crate::vfs::MutationFailure;

    let run = match outer_root {
        Some(root) => RootHandle::open_beneath(root, run_root),
        None => RootHandle::open(run_root),
    }
    .map_err(MutationFailure::Unchanged)?;
    if !run.matches_root_identity(expected_run_root) {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Geclaimter Quarantänelauf wurde vor dem Leerheitsbeleg ausgetauscht",
        )));
    }
    let remaining = Dir::read_from(&*run.directory)
        .map_err(io::Error::from)
        .map_err(MutationFailure::Unchanged)?;
    for item in remaining {
        let item = item
            .map_err(io::Error::from)
            .map_err(MutationFailure::Unchanged)?;
        let name = item.file_name().to_bytes();
        if name != b"." && name != b".." {
            return Err(MutationFailure::Unchanged(io::Error::other(
                "Geclaimter Quarantänelauf enthält nach dem Leeren wieder Bestand",
            )));
        }
    }
    let (intent_parent, intent_name) =
        open_parent_from(outer_root, intent).map_err(MutationFailure::Unchanged)?;
    if expected_intent.raw_name != intent_name.as_bytes() {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::InvalidData,
            "Quarantäne-Purge-Intent-Beleg gehört nicht zum gewählten Namen",
        )));
    }
    let mut intent_file = File::from(
        openat(
            &intent_parent,
            &intent_name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY,
            Mode::empty(),
        )
        .map_err(io::Error::from)
        .map_err(MutationFailure::Unchanged)?,
    );
    let intent_stat = fstat(&intent_file)
        .map_err(io::Error::from)
        .map_err(MutationFailure::Unchanged)?;
    if !stat_matches_proof(&intent_stat, expected_intent)
        || read_small_file_digest(&mut intent_file).map_err(MutationFailure::Unchanged)?
            != expected_intent_digest
        || !run.is_current().map_err(MutationFailure::Unchanged)?
    {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Quarantänelauf oder Purge-Intent änderte sich vor dem Leerheitsbeleg",
        )));
    }
    Ok(())
}

fn stat_matches_proof(stat: &Stat, proof: &ProofListEntry) -> bool {
    let kind = FileType::from_raw_mode(stat.st_mode);
    // `rustix::fs::Stat::st_dev` ist unter Linux bereits `u64`, unter macOS aber
    // ein schmalerer Typ. Getrennte Adapter vermeiden sowohl einen verlustbehafteten
    // Vergleich als auch Clippys plattformspezifischen `unnecessary_cast`-Fehler.
    #[cfg(target_os = "macos")]
    let device = stat.st_dev as u64;
    #[cfg(target_os = "linux")]
    let device = stat.st_dev;
    proof.entry.device == Some(device)
        && proof.entry.inode == Some(stat.st_ino)
        && ((proof.entry.is_file && kind.is_file())
            || (proof.entry.is_dir && kind.is_dir())
            || (proof.entry.is_symlink && kind.is_symlink()))
}

fn read_small_file_digest(file: &mut File) -> io::Result<[u8; 32]> {
    read_file_digest_bounded(
        file,
        1024,
        "Quarantäne-Purge-Intent überschreitet die Größenbegrenzung",
    )
}

fn read_file_digest_bounded(
    file: &mut File,
    max_bytes: usize,
    oversized_message: &'static str,
) -> io::Result<[u8; 32]> {
    let mut data = Vec::new();
    file.take(max_bytes as u64 + 1).read_to_end(&mut data)?;
    if data.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            oversized_message,
        ));
    }
    Ok(*blake3::hash(&data).as_bytes())
}

/// Zwei vorhandene Namen in genau einem Kernel-Schritt austauschen. `rustix`
/// übersetzt `EXCHANGE` auf Linux zu `renameat2(RENAME_EXCHANGE)` und auf macOS zu
/// `renameatx_np(RENAME_SWAP)`; beide Eltern bleiben descriptor-relativ geöffnet.
pub(crate) fn rename_exchange(left: &Path, right: &Path) -> io::Result<()> {
    rename_exchange_from(None, left, right)
}

pub(crate) fn rename_exchange_beneath(
    root: &RootHandle,
    left: &Path,
    right: &Path,
) -> io::Result<()> {
    rename_exchange_from(Some(root), left, right)
}

fn rename_exchange_from(root: Option<&RootHandle>, left: &Path, right: &Path) -> io::Result<()> {
    if left == right {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Namensaustausch benötigt zwei verschiedene Pfade",
        ));
    }
    let (left_parent, left_name) = open_parent_from(root, left)?;
    let (right_parent, right_name) = open_parent_from(root, right)?;
    Ok(renameat_with(
        left_parent,
        left_name,
        right_parent,
        right_name,
        RenameFlags::EXCHANGE,
    )?)
}

/// Descriptor-relativer Hardlink ohne Symlink-Folgen und ohne Überschreiben. `linkat`
/// ist für den neuen Namen atomar exklusiv; ohne `SYMLINK_FOLLOW` wird ein Symlink als
/// Quelle nicht dereferenziert (die Sync-Schicht erlaubt ohnehin nur reguläre Dateien).
pub(crate) fn create_hard_link_noreplace(existing: &Path, new: &Path) -> io::Result<()> {
    create_hard_link_noreplace_from(None, existing, new)
}

pub(crate) fn create_hard_link_noreplace_beneath(
    root: &RootHandle,
    existing: &Path,
    new: &Path,
) -> io::Result<()> {
    create_hard_link_noreplace_from(Some(root), existing, new)
}

fn create_hard_link_noreplace_from(
    root: Option<&RootHandle>,
    existing: &Path,
    new: &Path,
) -> io::Result<()> {
    hard_link_between(root, existing, root, new)
}

pub(crate) fn hard_link_between(
    existing_root: Option<&RootHandle>,
    existing: &Path,
    new_root: Option<&RootHandle>,
    new: &Path,
) -> io::Result<()> {
    let (existing_parent, existing_name) = open_parent_from(existing_root, existing)?;
    let (new_parent, new_name) = open_parent_from(new_root, new)?;
    Ok(rustix::fs::linkat(
        existing_parent,
        existing_name,
        new_parent,
        new_name,
        AtFlags::empty(),
    )?)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::{symlink, PermissionsExt};

    use super::*;

    /// macOS stellt `/var` als Symlink auf `/private/var` bereit. Produktionspfade
    /// werden bereits an der App-Grenze kanonisiert; der Test erzeugt seine Temp-Pfade
    /// deshalb genauso, damit wirklich nur der gezielt eingesetzte Symlink geprüft wird.
    fn canonical_tempdir() -> tempfile::TempDir {
        let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    fn fifo_call_finishes_with_error(
        path: &Path,
        call: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> (bool, bool) {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx.send(call()).unwrap();
        });
        started_rx.recv().unwrap();
        let early = result_rx.recv_timeout(std::time::Duration::from_secs(1));
        let finished = early.is_ok();
        // Auch der alte blockierende Gegenfall muss ohne zurückgelassenen Thread
        // enden: Ein eigener nicht blockierender Peer löst dessen FIFO-open aus.
        let peer = open(
            path,
            OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let result = early.unwrap_or_else(|_| {
            result_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
        });
        worker.join().unwrap();
        drop(peer);
        (finished, result.is_err())
    }

    #[test]
    fn eingewechselte_fifo_blockiert_keinen_finalen_dateizugriff() {
        let mut results = Vec::new();
        for rooted in [false, true] {
            for operation in ["read", "write", "sync", "mtime", "mode"] {
                let temp = canonical_tempdir();
                let path = temp.path().join("entry");
                std::fs::write(&path, b"urspruenglich regulaer").unwrap();
                let root = RootHandle::open(temp.path()).unwrap();
                assert!(stat_beneath(&root, &path).unwrap().is_file);
                std::fs::remove_file(&path).unwrap();
                let fifo_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
                let entry = path.clone();
                let result = fifo_call_finishes_with_error(&path, move || {
                    let root = rooted.then_some(&root);
                    match operation {
                        "read" => open_read_from(root, &entry).map(drop),
                        "write" => open_write_from(root, &entry).map(drop),
                        "sync" => sync_file_from(root, &entry),
                        "mtime" => set_mtime_from(root, &entry, 1_700_000_000),
                        "mode" => set_mode_from(root, &entry, 0o600),
                        _ => unreachable!(),
                    }
                });
                results.push((rooted, operation, result));
            }
        }
        assert!(
            results.iter().all(|(_, _, result)| *result == (true, true)),
            "FIFO-Zugriffe: {results:?}"
        );
    }

    #[test]
    fn eingewechselter_fifo_intent_blockiert_keine_quarantaene_wirkung() {
        let mut results = Vec::new();
        for operation in ["restore", "purge", "verify"] {
            let temp = canonical_tempdir();
            let outer = RootHandle::open(temp.path()).unwrap();
            let run = temp.path().join("run");
            let target = temp.path().join("target");
            std::fs::create_dir(&run).unwrap();
            std::fs::create_dir(&target).unwrap();
            let expected_run = root_identity_beneath(&outer, &run).unwrap();
            let expected_target = crate::real_mode::RootIdentity::Local(
                root_identity_beneath(&outer, &target).unwrap(),
            );
            let source = run.join("entry");
            let expected_source = if operation == "restore" {
                std::fs::write(&source, b"bleibt").unwrap();
                Some(list_complete(&run).unwrap().remove(0))
            } else {
                None
            };
            let intent = temp.path().join("intent");
            std::fs::write(&intent, b"intent").unwrap();
            let expected_intent = list_complete(temp.path())
                .unwrap()
                .into_iter()
                .find(|entry| entry.raw_name == b"intent")
                .unwrap();
            std::fs::remove_file(&intent).unwrap();
            let fifo_path = std::ffi::CString::new(intent.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
            let entry = intent.clone();
            let retained_source = source.clone();
            let retained_target = target.clone();
            let result = fifo_call_finishes_with_error(&intent, move || {
                let digest = *blake3::hash(b"intent").as_bytes();
                let result = match operation {
                    "restore" => restore_from_bound_quarantine_run(
                        Some(&outer),
                        &run,
                        &expected_run,
                        &source,
                        &target,
                        &expected_target,
                        &target.join("entry"),
                        expected_source.as_ref().unwrap(),
                        &entry,
                        &expected_intent,
                        digest,
                    ),
                    "purge" => purge_bound_quarantine_run_contents(
                        Some(&outer),
                        &run,
                        &expected_run,
                        &entry,
                        &expected_intent,
                        digest,
                    ),
                    "verify" => verify_bound_quarantine_run_empty(
                        Some(&outer),
                        &run,
                        &expected_run,
                        &entry,
                        &expected_intent,
                        digest,
                    ),
                    _ => unreachable!(),
                };
                result.map_err(crate::vfs::MutationFailure::into_error)
            });
            results.push((operation, result));
            assert!(std::fs::read_dir(&retained_target)
                .unwrap()
                .next()
                .is_none());
            if operation == "restore" {
                assert_eq!(std::fs::read(&retained_source).unwrap(), b"bleibt");
            }
        }
        assert!(
            results.iter().all(|(_, result)| *result == (true, true)),
            "FIFO-Intents: {results:?}"
        );
    }

    /// Die Anzeige-Metadaten stammen aus demselben descriptor-relativen Stat wie
    /// Dateityp und Größe. Namen werden nur dann als Endung gedeutet, wenn das Ziel
    /// tatsächlich eine normale Datei ist.
    #[test]
    fn listing_liefert_lokale_darstellungsmetadaten() {
        let temp = canonical_tempdir();
        let file = temp.path().join("archiv.tar.gz");
        std::fs::write(&file, b"inhalt").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::fs::create_dir(temp.path().join("ordner.txt")).unwrap();
        std::fs::write(temp.path().join(".env"), b"verborgen").unwrap();

        let entries = list_complete(temp.path()).unwrap();
        let file = entries
            .iter()
            .find(|entry| entry.entry.name == "archiv.tar.gz")
            .unwrap();
        assert!(file.safely_decoded);
        assert!(file.entry.is_file);
        assert_eq!(file.entry.extension.as_deref(), Some("gz"));
        assert_eq!(file.entry.mode, Some(0o640));
        assert!(file.entry.created.is_some());
        assert!(file
            .entry
            .owner
            .as_ref()
            .is_some_and(|value| !value.is_empty()));
        assert!(file
            .entry
            .group
            .as_ref()
            .is_some_and(|value| !value.is_empty()));

        let directory = entries
            .iter()
            .find(|entry| entry.entry.name == "ordner.txt")
            .unwrap();
        assert!(directory.entry.is_dir);
        assert_eq!(directory.entry.extension, None);

        let hidden = entries
            .iter()
            .find(|entry| entry.entry.name == ".env")
            .unwrap();
        assert_eq!(hidden.entry.extension, None);
    }

    #[test]
    fn purge_meldet_einen_nach_dem_loeschdurchlauf_erschienenen_eintrag_als_unklar() {
        let temp = canonical_tempdir();
        let outer = RootHandle::open(temp.path()).unwrap();
        let run = temp.path().join("lauf");
        std::fs::create_dir(&run).unwrap();
        std::fs::write(run.join("alt"), b"weg").unwrap();
        let expected = root_identity_beneath(&outer, &run).unwrap();
        let intent = temp.path().join("lauf.purge-intent");
        std::fs::write(&intent, b"intent").unwrap();
        let intent_proof = list_complete(temp.path())
            .unwrap()
            .into_iter()
            .find(|entry| entry.raw_name == b"lauf.purge-intent")
            .unwrap();
        let intent_digest = *blake3::hash(b"intent").as_bytes();
        let late = run.join("spaet");
        PURGE_AFTER_REMOVE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                std::fs::write(&late, b"bleibt").unwrap();
            }));
        });

        let error = purge_bound_quarantine_run_contents(
            Some(&outer),
            &run,
            &expected,
            &intent,
            &intent_proof,
            intent_digest,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            crate::vfs::MutationFailure::OutcomeUnclear(_)
        ));
        assert_eq!(std::fs::read(run.join("spaet")).unwrap(), b"bleibt");
    }

    /// Gezielter Swap-Test: Nach dem Öffnen wird der sichtbare Ordner wegbenannt und
    /// durch einen Link nach außen ersetzt. Der Deskriptor muss trotzdem im ursprünglich
    /// geöffneten (jetzt geparkten) Ordner schreiben.
    #[test]
    fn geoeffneter_ordner_bleibt_bei_symlink_swap_verankert() {
        let temp = canonical_tempdir();
        let slot = temp.path().join("slot");
        let parked = temp.path().join("geparkt");
        let outside = temp.path().join("ausserhalb");
        std::fs::create_dir(&slot).unwrap();
        std::fs::create_dir(&outside).unwrap();

        let anchored = open_dir_nofollow(&slot, false).unwrap();
        std::fs::rename(&slot, &parked).unwrap();
        symlink(&outside, &slot).unwrap();

        let fd = openat(
            &anchored,
            "beweis.txt",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
            FILE_MODE,
        )
        .unwrap();
        File::from(fd).write_all(b"verankert").unwrap();

        assert_eq!(
            std::fs::read(parked.join("beweis.txt")).unwrap(),
            b"verankert"
        );
        assert!(!outside.join("beweis.txt").exists());
    }

    /// Auch ein bereits vor dem Aufruf vorhandener Symlink in einer Elternkomponente
    /// wird nicht durchlaufen.
    #[test]
    fn schreiben_durch_symlink_elternordner_wird_abgelehnt() {
        let temp = canonical_tempdir();
        let outside = temp.path().join("ausserhalb");
        let link = temp.path().join("link");
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, &link).unwrap();

        let result = open_write_new(&link.join("darf-nicht-entstehen.txt"));
        assert!(result.is_err());
        assert!(!outside.join("darf-nicht-entstehen.txt").exists());
    }

    /// Der neue Root-VFS-Weg darf nach einem sichtbaren Austausch ausschließlich den
    /// ursprünglich geöffneten Ordner erreichen. Die Sync-Schicht erkennt den Wechsel
    /// zusätzlich und bricht ab; dieser Test belegt die darunterliegende Verankerung.
    #[test]
    fn root_handle_schreibt_nach_austausch_nur_in_alten_ordner() {
        let temp = canonical_tempdir();
        let root = temp.path().join("wurzel");
        let parked = temp.path().join("geparkt");
        std::fs::create_dir(&root).unwrap();
        let handle = RootHandle::open(&root).unwrap();

        std::fs::rename(&root, &parked).unwrap();
        std::fs::create_dir(&root).unwrap();
        let mut file = open_write_new_beneath(&handle, &root.join("beweis.txt")).unwrap();
        file.write_all(b"alter handle").unwrap();
        drop(file);

        assert_eq!(
            std::fs::read(parked.join("beweis.txt")).unwrap(),
            b"alter handle"
        );
        assert!(!root.join("beweis.txt").exists());
        assert!(!handle.is_current().unwrap());
    }

    #[test]
    fn root_handle_lehnt_pfad_ausserhalb_ab() {
        let temp = canonical_tempdir();
        let root = temp.path().join("wurzel");
        let outside = temp.path().join("ausserhalb");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let handle = RootHandle::open(&root).unwrap();

        let result = open_write_new_beneath(&handle, &outside.join("darf-nicht.txt"));
        assert!(result.is_err());
        assert!(!outside.join("darf-nicht.txt").exists());
    }

    #[test]
    fn restore_elternkette_haelt_jeden_neu_angelegten_ordner_bis_zum_root() {
        let temp = canonical_tempdir();
        let root_path = temp.path().join("wurzel");
        std::fs::create_dir(&root_path).unwrap();
        let root = RootHandle::open(&root_path).unwrap();
        let target_parent = root_path.join("eins/zwei/drei");

        let (_parent, chain) =
            open_restore_parent_after_missing_with_durability_chain(&root, &target_parent).unwrap();

        assert_eq!(
            chain.len(),
            4,
            "Root und drei neue Ordner müssen gehalten werden"
        );
        assert!(target_parent.is_dir());
    }

    #[test]
    fn restore_elternkette_haelt_auch_zwischen_pruefung_und_oeffnen_erschienene_ordner() {
        let temp = canonical_tempdir();
        let root_path = temp.path().join("wurzel");
        std::fs::create_dir(&root_path).unwrap();
        let root = RootHandle::open(&root_path).unwrap();
        let target_parent = root_path.join("eins/zwei");

        let missing = open_dir_nofollow_from(Some(&root), &target_parent, false).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        // Ein anderer Prozess gewinnt genau das Fenster zwischen der ersten
        // Abwesenheitsprüfung und dem erneuten descriptor-relativen Öffnen.
        std::fs::create_dir_all(&target_parent).unwrap();

        let (_parent, chain) =
            open_restore_parent_after_missing_with_durability_chain(&root, &target_parent).unwrap();

        assert_eq!(
            chain.len(),
            3,
            "Root und beide Elternnamen müssen gehalten werden"
        );
    }

    #[test]
    fn atomarer_namensaustausch_tauscht_datei_und_nichtleeren_ordner() {
        let temp = canonical_tempdir();
        let root = temp.path().join("wurzel");
        std::fs::create_dir(&root).unwrap();
        let file = root.join("links");
        let directory = root.join("rechts");
        std::fs::write(&file, b"datei").unwrap();
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("kind"), b"ordner").unwrap();
        let handle = RootHandle::open(&root).unwrap();

        rename_exchange_beneath(&handle, &file, &directory).unwrap();

        assert_eq!(std::fs::read(file.join("kind")).unwrap(), b"ordner");
        assert_eq!(std::fs::read(&directory).unwrap(), b"datei");
        assert!(rename_exchange_beneath(&handle, &file, &file).is_err());
    }
}
