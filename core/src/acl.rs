//! Descriptor-relative ACL-Adapter für macOS-NFSv4- und Linux-POSIX-ACLs.
//!
//! Die Plattformformate werden sofort in typisierte DTOs übersetzt. Weder Pfad-APIs
//! noch Textdarstellungen mit Nutzer- oder Gruppennamen gehören zum Vertrag.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

use crate::metadata::Acl;

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use crate::metadata::{PosixAcl, PosixAclEntry, PosixAclTag};

    const VERSION: u32 = 2;
    const ACCESS: &[u8] = b"system.posix_acl_access\0";
    const DEFAULT: &[u8] = b"system.posix_acl_default\0";
    const USER_OBJ: u16 = 0x01;
    const USER: u16 = 0x02;
    const GROUP_OBJ: u16 = 0x04;
    const GROUP: u16 = 0x08;
    const MASK: u16 = 0x10;
    const OTHER: u16 = 0x20;

    pub(super) fn read(fd: &OwnedFd) -> io::Result<Option<Acl>> {
        let mut access = match read_xattr(fd, ACCESS)? {
            Some(bytes) => decode(&bytes)?,
            None => Vec::new(),
        };
        let mut default = match read_xattr(fd, DEFAULT)? {
            Some(bytes) => decode(&bytes)?,
            None => Vec::new(),
        };
        canonicalize(&mut access);
        canonicalize(&mut default);
        if access.is_empty() && default.is_empty() {
            Ok(None)
        } else {
            let acl = Acl::Posix(PosixAcl { access, default });
            acl.validate()?;
            Ok(Some(acl))
        }
    }

    pub(super) fn write_exact(fd: &OwnedFd, acl: Option<&Acl>) -> io::Result<()> {
        let (access, default) = match acl {
            Some(Acl::Posix(acl)) => (&acl.access[..], &acl.default[..]),
            Some(Acl::Nfs4(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "NFSv4-ACL ist mit Linux-POSIX-ACL nicht kompatibel",
                ))
            }
            None => (&[][..], &[][..]),
        };
        if let Some(acl) = acl {
            acl.validate()?;
        }
        write_or_remove(fd, ACCESS, access)?;
        write_or_remove(fd, DEFAULT, default)?;
        if read(fd)?.as_ref() != acl {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "POSIX-ACL stimmt nach dem Schreiben nicht exakt überein",
            ));
        }
        Ok(())
    }

    fn read_xattr(fd: &OwnedFd, name: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let size = unsafe {
            libc::fgetxattr(
                fd.as_raw_fd(),
                name.as_ptr().cast(),
                std::ptr::null_mut(),
                0,
            )
        };
        if size < 0 {
            let error = io::Error::last_os_error();
            return if matches!(error.raw_os_error(), Some(libc::ENODATA)) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        let mut bytes = vec![0u8; size as usize];
        let read = unsafe {
            libc::fgetxattr(
                fd.as_raw_fd(),
                name.as_ptr().cast(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        if read as usize != bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "POSIX-ACL änderte sich während des Lesens",
            ));
        }
        Ok(Some(bytes))
    }

    fn write_or_remove(fd: &OwnedFd, name: &[u8], entries: &[PosixAclEntry]) -> io::Result<()> {
        if entries.is_empty() {
            let result = unsafe { libc::fremovexattr(fd.as_raw_fd(), name.as_ptr().cast()) };
            if result != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ENODATA) {
                    return Err(error);
                }
            }
            return Ok(());
        }
        let bytes = encode(entries);
        let result = unsafe {
            libc::fsetxattr(
                fd.as_raw_fd(),
                name.as_ptr().cast(),
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn decode(bytes: &[u8]) -> io::Result<Vec<PosixAclEntry>> {
        if bytes.len() < 4
            || !(bytes.len() - 4).is_multiple_of(8)
            || u32::from_le_bytes(bytes[0..4].try_into().unwrap()) != VERSION
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Ungültiges Linux-POSIX-ACL-Binärformat",
            ));
        }
        bytes[4..]
            .chunks_exact(8)
            .map(|item| {
                let raw_tag = u16::from_le_bytes(item[0..2].try_into().unwrap());
                let permissions = u16::from_le_bytes(item[2..4].try_into().unwrap());
                if permissions > 7 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "POSIX-ACL enthält unbekannte Rechtebits",
                    ));
                }
                let raw_id = u32::from_le_bytes(item[4..8].try_into().unwrap());
                let (tag, id) = match raw_tag {
                    USER_OBJ => (PosixAclTag::UserObject, None),
                    USER => (PosixAclTag::User, Some(raw_id)),
                    GROUP_OBJ => (PosixAclTag::GroupObject, None),
                    GROUP => (PosixAclTag::Group, Some(raw_id)),
                    MASK => (PosixAclTag::Mask, None),
                    OTHER => (PosixAclTag::Other, None),
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "POSIX-ACL enthält unbekannten Tag",
                        ))
                    }
                };
                Ok(PosixAclEntry {
                    tag,
                    id,
                    permissions: permissions as u8,
                })
            })
            .collect()
    }

    fn encode(entries: &[PosixAclEntry]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(4 + entries.len() * 8);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        for entry in entries {
            let tag = match entry.tag {
                PosixAclTag::UserObject => USER_OBJ,
                PosixAclTag::User => USER,
                PosixAclTag::GroupObject => GROUP_OBJ,
                PosixAclTag::Group => GROUP,
                PosixAclTag::Mask => MASK,
                PosixAclTag::Other => OTHER,
            };
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&(entry.permissions as u16).to_le_bytes());
            bytes.extend_from_slice(&entry.id.unwrap_or(u32::MAX).to_le_bytes());
        }
        bytes
    }

    fn canonicalize(entries: &mut [PosixAclEntry]) {
        // Dieselbe Tagnummer wie im Manifest-Digest: Sortierung und Kodierung
        // dürfen nicht auseinanderlaufen.
        entries.sort_by_key(|entry| {
            (
                crate::metadata::posix_tag_byte(entry.tag),
                entry.id.unwrap_or(0),
            )
        });
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use crate::metadata::{Nfs4AceKind, Nfs4Acl, Nfs4AclEntry};
    use std::ffi::c_void;

    const ACL_TYPE_EXTENDED: libc::c_int = 0x100;
    const ACL_FIRST_ENTRY: libc::c_int = 0;
    const ACL_NEXT_ENTRY: libc::c_int = -1;
    const ACL_EXTENDED_ALLOW: libc::c_int = 1;
    const ACL_EXTENDED_DENY: libc::c_int = 2;
    const ACL_FLAG_NO_INHERIT: libc::c_int = 1 << 17;

    unsafe extern "C" {
        fn acl_init(count: libc::c_int) -> *mut c_void;
        fn acl_free(object: *mut c_void) -> libc::c_int;
        fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> *mut c_void;
        fn acl_set_fd_np(fd: libc::c_int, acl: *mut c_void, acl_type: libc::c_int) -> libc::c_int;
        fn acl_get_entry(
            acl: *mut c_void,
            entry_id: libc::c_int,
            entry: *mut *mut c_void,
        ) -> libc::c_int;
        fn acl_create_entry(acl: *mut *mut c_void, entry: *mut *mut c_void) -> libc::c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut libc::c_int) -> libc::c_int;
        fn acl_set_tag_type(entry: *mut c_void, tag: libc::c_int) -> libc::c_int;
        fn acl_get_qualifier(entry: *mut c_void) -> *mut c_void;
        fn acl_set_qualifier(entry: *mut c_void, qualifier: *const c_void) -> libc::c_int;
        fn acl_get_permset_mask_np(entry: *mut c_void, mask: *mut u64) -> libc::c_int;
        fn acl_set_permset_mask_np(entry: *mut c_void, mask: u64) -> libc::c_int;
        fn acl_get_flagset_np(object: *mut c_void, flags: *mut *mut c_void) -> libc::c_int;
        fn acl_get_flag_np(flags: *mut c_void, flag: libc::c_int) -> libc::c_int;
        fn acl_add_flag_np(flags: *mut c_void, flag: libc::c_int) -> libc::c_int;
        fn acl_clear_flags_np(flags: *mut c_void) -> libc::c_int;
    }

    const FLAGS: [i32; 5] = [1 << 4, 1 << 5, 1 << 6, 1 << 7, 1 << 8];

    struct OwnedAcl(*mut c_void);
    impl Drop for OwnedAcl {
        fn drop(&mut self) {
            unsafe {
                acl_free(self.0);
            }
        }
    }

    pub(super) fn read(fd: &OwnedFd) -> io::Result<Option<Acl>> {
        let raw = unsafe { acl_get_fd_np(fd.as_raw_fd(), ACL_TYPE_EXTENDED) };
        if raw.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ENOENT) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        let acl = OwnedAcl(raw);
        let mut entries = Vec::new();
        let mut entry = std::ptr::null_mut();
        let mut selector = ACL_FIRST_ENTRY;
        loop {
            let result = unsafe { acl_get_entry(acl.0, selector, &mut entry) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if selector == ACL_NEXT_ENTRY && error.raw_os_error() == Some(libc::EINVAL) {
                    break;
                }
                return Err(error);
            }
            selector = ACL_NEXT_ENTRY;
            let mut tag = 0;
            check(unsafe { acl_get_tag_type(entry, &mut tag) })?;
            let kind = match tag {
                ACL_EXTENDED_ALLOW => Nfs4AceKind::Allow,
                ACL_EXTENDED_DENY => Nfs4AceKind::Deny,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "macOS-ACL enthält unbekannten Allow/Deny-Typ",
                    ))
                }
            };
            let qualifier = unsafe { acl_get_qualifier(entry) };
            if qualifier.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut uuid = [0u8; 16];
            unsafe {
                std::ptr::copy_nonoverlapping(qualifier.cast::<u8>(), uuid.as_mut_ptr(), 16);
                acl_free(qualifier);
            }
            let mut permissions = 0;
            check(unsafe { acl_get_permset_mask_np(entry, &mut permissions) })?;
            let mut flagset = std::ptr::null_mut();
            check(unsafe { acl_get_flagset_np(entry, &mut flagset) })?;
            let mut flags = 0u32;
            for flag in FLAGS {
                if unsafe { acl_get_flag_np(flagset, flag) } == 1 {
                    flags |= flag as u32;
                }
            }
            entries.push(Nfs4AclEntry {
                qualifier: uuid,
                kind,
                permissions,
                flags,
            });
        }
        let mut acl_flags = std::ptr::null_mut();
        check(unsafe { acl_get_flagset_np(acl.0, &mut acl_flags) })?;
        let flags = if unsafe { acl_get_flag_np(acl_flags, ACL_FLAG_NO_INHERIT) } == 1 {
            ACL_FLAG_NO_INHERIT as u32
        } else {
            0
        };
        if entries.is_empty() && flags == 0 {
            Ok(None)
        } else {
            Ok(Some(Acl::Nfs4(Nfs4Acl { entries, flags })))
        }
    }

    pub(super) fn write_exact(fd: &OwnedFd, expected: Option<&Acl>) -> io::Result<()> {
        let entries = match expected {
            Some(Acl::Nfs4(acl)) => acl.entries.as_slice(),
            Some(Acl::Posix(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "POSIX-ACL ist mit macOS-NFSv4-ACL nicht kompatibel",
                ))
            }
            None => &[],
        };
        if let Some(acl) = expected {
            acl.validate()?;
        }
        let raw = unsafe { acl_init(entries.len() as libc::c_int) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut acl = OwnedAcl(raw);
        let global_flags = match expected {
            Some(Acl::Nfs4(acl)) => acl.flags,
            _ => 0,
        };
        let mut acl_flagset = std::ptr::null_mut();
        check(unsafe { acl_get_flagset_np(acl.0, &mut acl_flagset) })?;
        check(unsafe { acl_clear_flags_np(acl_flagset) })?;
        if global_flags & ACL_FLAG_NO_INHERIT as u32 != 0 {
            check(unsafe { acl_add_flag_np(acl_flagset, ACL_FLAG_NO_INHERIT) })?;
        }
        for item in entries {
            let mut entry = std::ptr::null_mut();
            check(unsafe { acl_create_entry(&mut acl.0, &mut entry) })?;
            let tag = match item.kind {
                Nfs4AceKind::Allow => ACL_EXTENDED_ALLOW,
                Nfs4AceKind::Deny => ACL_EXTENDED_DENY,
            };
            check(unsafe { acl_set_tag_type(entry, tag) })?;
            check(unsafe { acl_set_qualifier(entry, item.qualifier.as_ptr().cast()) })?;
            check(unsafe { acl_set_permset_mask_np(entry, item.permissions) })?;
            let mut flagset = std::ptr::null_mut();
            check(unsafe { acl_get_flagset_np(entry, &mut flagset) })?;
            check(unsafe { acl_clear_flags_np(flagset) })?;
            for flag in FLAGS {
                if item.flags & flag as u32 != 0 {
                    check(unsafe { acl_add_flag_np(flagset, flag) })?;
                }
            }
        }
        check(unsafe { acl_set_fd_np(fd.as_raw_fd(), acl.0, ACL_TYPE_EXTENDED) })?;
        let actual = read(fd)?;
        if actual.as_ref() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("NFSv4-ACL stimmt nach dem Schreiben nicht exakt überein: erwartet {expected:?}, gelesen {actual:?}"),
            ));
        }
        Ok(())
    }

    fn check(result: libc::c_int) -> io::Result<()> {
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

pub(crate) fn read_fd(fd: &OwnedFd) -> io::Result<Option<Acl>> {
    platform::read(fd)
}
pub(crate) fn write_fd_exact(fd: &OwnedFd, acl: Option<&Acl>) -> io::Result<()> {
    platform::write_exact(fd, acl)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    #[cfg(target_os = "macos")]
    use crate::metadata::{Nfs4AceKind, Nfs4Acl, Nfs4AclEntry};
    #[cfg(target_os = "linux")]
    use crate::metadata::{PosixAcl, PosixAclEntry, PosixAclTag};
    use crate::vfs::{LocalVfs, Vfs};

    use super::*;

    fn canonical_tempfile() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let path = dir.path().join("acl-roundtrip");
        std::fs::write(&path, b"ACL-Test").unwrap();
        (dir, path)
    }

    #[cfg(target_os = "linux")]
    fn test_acl() -> Acl {
        Acl::Posix(PosixAcl {
            access: vec![
                PosixAclEntry {
                    tag: PosixAclTag::UserObject,
                    id: None,
                    permissions: 6,
                },
                PosixAclEntry {
                    tag: PosixAclTag::User,
                    id: Some(12345),
                    permissions: 4,
                },
                PosixAclEntry {
                    tag: PosixAclTag::GroupObject,
                    id: None,
                    permissions: 4,
                },
                PosixAclEntry {
                    tag: PosixAclTag::Mask,
                    id: None,
                    permissions: 4,
                },
                PosixAclEntry {
                    tag: PosixAclTag::Other,
                    id: None,
                    permissions: 0,
                },
            ],
            default: vec![],
        })
    }

    #[cfg(target_os = "macos")]
    fn test_acl() -> Acl {
        unsafe extern "C" {
            fn mbr_uid_to_uuid(id: libc::uid_t, uu: *mut u8) -> libc::c_int;
        }
        let mut qualifier = [0u8; 16];
        let result = unsafe { mbr_uid_to_uuid(libc::geteuid(), qualifier.as_mut_ptr()) };
        assert_eq!(result, 0);
        Acl::Nfs4(Nfs4Acl {
            entries: vec![Nfs4AclEntry {
                qualifier,
                kind: Nfs4AceKind::Allow,
                permissions: 1 << 1,
                flags: 0,
            }],
            flags: 0,
        })
    }

    #[test]
    fn typisierte_acl_wird_descriptor_relativ_geschrieben_gelesen_und_entfernt() {
        let (_dir, path) = canonical_tempfile();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let vfs = LocalVfs::new();
        let acl = test_acl();

        vfs.write_acl_exact(&path, Some(&acl)).unwrap();
        assert_eq!(vfs.metadata_manifest(&path).unwrap().acl, Some(acl));
        vfs.write_acl_exact(&path, None).unwrap();
        assert_eq!(vfs.metadata_manifest(&path).unwrap().acl, None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_default_acl_eines_ordners_bleibt_typisiert() {
        let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let path = dir.path().join("ordner");
        std::fs::create_dir(&path).unwrap();
        let acl = Acl::Posix(PosixAcl {
            access: vec![],
            default: vec![
                PosixAclEntry {
                    tag: PosixAclTag::UserObject,
                    id: None,
                    permissions: 7,
                },
                PosixAclEntry {
                    tag: PosixAclTag::User,
                    id: Some(12345),
                    permissions: 5,
                },
                PosixAclEntry {
                    tag: PosixAclTag::GroupObject,
                    id: None,
                    permissions: 5,
                },
                PosixAclEntry {
                    tag: PosixAclTag::Mask,
                    id: None,
                    permissions: 5,
                },
                PosixAclEntry {
                    tag: PosixAclTag::Other,
                    id: None,
                    permissions: 0,
                },
            ],
        });
        let vfs = LocalVfs::new();

        vfs.write_acl_exact(&path, Some(&acl)).unwrap();
        assert_eq!(vfs.metadata_manifest(&path).unwrap().acl, Some(acl));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_vererbungsflag_eines_ordners_bleibt_typisiert() {
        let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let path = dir.path().join("ordner");
        std::fs::create_dir(&path).unwrap();
        let Acl::Nfs4(mut acl) = test_acl() else {
            unreachable!()
        };
        acl.entries[0].flags = 1 << 5;
        acl.flags = 1 << 17;
        let acl = Acl::Nfs4(acl);
        let vfs = LocalVfs::new();

        vfs.write_acl_exact(&path, Some(&acl)).unwrap();
        assert_eq!(vfs.metadata_manifest(&path).unwrap().acl, Some(acl));
    }
}
