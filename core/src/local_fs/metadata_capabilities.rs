//! Nur lesende Fähigkeitsabfrage am gehaltenen Root; plan() legt keine Probe an.
use super::*;
use crate::metadata::{AclModel, Capability, MetadataCapabilities};

#[cfg(target_os = "macos")]
pub(super) fn read(
    directory: &OwnedFd,
    mount: &LocalMountIdentity,
) -> io::Result<MetadataCapabilities> {
    let capabilities = volume_capabilities(directory)?;
    Ok(from_volume_bits(
        &capabilities.capabilities,
        &capabilities.valid,
        mount.mount_flags,
    ))
}

#[cfg(target_os = "macos")]
pub(super) fn volume_capabilities(
    directory: &OwnedFd,
) -> io::Result<libc::vol_capabilities_attr_t> {
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_CAPABILITIES,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    #[repr(C)]
    struct VolumeCapabilitiesBuffer {
        length: u32,
        capabilities: libc::vol_capabilities_attr_t,
    }
    let mut buffer: VolumeCapabilitiesBuffer = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fgetattrlist(
            directory.as_raw_fd(),
            (&mut attributes as *mut libc::attrlist).cast(),
            (&mut buffer as *mut VolumeCapabilitiesBuffer).cast(),
            std::mem::size_of::<VolumeCapabilitiesBuffer>(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if buffer.length as usize != std::mem::size_of::<VolumeCapabilitiesBuffer>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unvollständige macOS-Volume-Fähigkeitsantwort",
        ));
    }
    Ok(buffer.capabilities)
}

#[cfg(target_os = "macos")]
fn from_volume_bits(bits: &[u32; 4], valid: &[u32; 4], flags: u64) -> MetadataCapabilities {
    let feature = |group: usize, mask| {
        if valid[group] & mask == 0 {
            Capability::Unknown
        } else if bits[group] & mask != 0 {
            Capability::Exact
        } else {
            Capability::Unsupported
        }
    };
    let permissions = if flags & libc::MNT_IGNORE_OWNERSHIP as u64 != 0 {
        Capability::Unsupported
    } else {
        match feature(0, libc::VOL_CAP_FMT_NO_PERMISSIONS) {
            Capability::Exact => Capability::Unsupported,
            Capability::Unsupported => Capability::Exact,
            _ => Capability::Unknown,
        }
    };
    MetadataCapabilities {
        owner: permissions,
        special_mode_bits: permissions,
        acl_model: match feature(1, libc::VOL_CAP_INT_EXTENDED_SECURITY) {
            Capability::Exact => AclModel::Nfs4,
            Capability::Unsupported => AclModel::Unsupported,
            _ => AclModel::Unknown,
        },
        // Der lokale Adapter besitzt auf macOS nur den Lesevertrag für xattrs.
        xattrs: match feature(1, libc::VOL_CAP_INT_EXTENDED_ATTR) {
            Capability::Exact => Capability::ReadOnly,
            other => other,
        },
        resource_fork: feature(1, libc::VOL_CAP_INT_NAMEDSTREAMS),
        sparse_extents: feature(0, libc::VOL_CAP_FMT_SPARSE_FILES),
        hardlinks: feature(0, libc::VOL_CAP_FMT_HARDLINKS),
    }
}

#[cfg(target_os = "linux")]
pub(super) fn read(
    directory: &OwnedFd,
    _mount: &LocalMountIdentity,
) -> io::Result<MetadataCapabilities> {
    let filesystem = linux_filesystem_name(directory)?;
    let mut capabilities = for_linux_filesystem(&filesystem);
    if capabilities.owner != Capability::Exact {
        return Ok(capabilities);
    }
    // Auch native Dateisysteme können ohne ACL-/xattr-Unterstützung eingehängt
    // sein. Fehlende Werte und ein nicht unterstütztes Modell sind verschieden.
    capabilities.acl_model = match crate::acl::read_fd(directory) {
        Ok(_) => AclModel::Posix,
        Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => AclModel::Unsupported,
        Err(error) => return Err(error),
    };
    // flistxattr kann auch bei gesperrtem user-Namensraum eine leere Liste
    // liefern. ENODATA der gezielten Leseabfrage beweist dagegen dessen Zugang.
    let result = unsafe {
        libc::fgetxattr(
            directory.as_raw_fd(),
            c"user.doppeldecker.capability".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    capabilities.xattrs = if result >= 0 {
        Capability::Exact
    } else {
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENODATA) => Capability::Exact,
            Some(libc::EOPNOTSUPP) => Capability::Unsupported,
            _ => return Err(error),
        }
    };
    Ok(capabilities)
}

#[cfg(target_os = "linux")]
fn for_linux_filesystem(filesystem: &str) -> MetadataCapabilities {
    match filesystem {
        // ext2/3/4 teilen dieselbe statfs-Magic. Der tatsächliche Treibername
        // verhindert deshalb, dass ext2 unbewiesen den ext4-Sparse-Vertrag erhält.
        "ext4" | "xfs" | "btrfs" | "tmpfs" => MetadataCapabilities {
            owner: Capability::Exact,
            special_mode_bits: Capability::Exact,
            acl_model: AclModel::Posix,
            xattrs: Capability::Exact,
            resource_fork: Capability::Unsupported,
            sparse_extents: Capability::Exact,
            hardlinks: Capability::Exact,
        },
        "exfat" | "vfat" | "msdos" => MetadataCapabilities {
            owner: Capability::Unsupported,
            special_mode_bits: Capability::Unsupported,
            acl_model: AclModel::Unsupported,
            xattrs: Capability::Unsupported,
            resource_fork: Capability::Unsupported,
            sparse_extents: Capability::Unsupported,
            hardlinks: Capability::Unsupported,
        },
        // Insbesondere FUSE und eingehängte Remote-Freigaben beweisen allein
        // durch einen lokalen Pfad keine nativen Schreibgarantien.
        _ => MetadataCapabilities::unknown(),
    }
}

#[cfg(target_os = "linux")]
pub(super) fn linux_filesystem_name(directory: &OwnedFd) -> io::Result<String> {
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    // mountinfo enthält die normale Mount-ID, nicht STATX_MNT_ID_UNIQUE.
    if unsafe {
        libc::statx(
            directory.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_MNT_ID,
            &mut stat,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if stat.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Mount-ID für die Fähigkeitsabfrage fehlt",
        ));
    }
    let mut bytes = Vec::new();
    File::open("/proc/self/mountinfo")?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Mounttabelle überschreitet die Lesegrenze",
        ));
    }
    filesystem_for_mount(&bytes, stat.stx_mnt_id)
}

#[cfg(target_os = "linux")]
fn filesystem_for_mount(bytes: &[u8], mount_id: u64) -> io::Result<String> {
    let expected_id = mount_id.to_string();
    for line in bytes.split(|byte| *byte == b'\n') {
        let mut fields = line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty());
        if fields.next() != Some(expected_id.as_bytes()) {
            continue;
        }
        let filesystem = fields.skip_while(|field| *field != b"-").nth(1);
        return filesystem
            .and_then(|name| std::str::from_utf8(name).ok())
            .map(str::to_owned)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Mounttabelle enthält keinen Dateisystemnamen",
                )
            });
    }
    Err(io::Error::other(
        "Gehaltene Mount-ID fehlt in der Mounttabelle",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn fehlende_volume_bits_bleiben_unbekannt_und_owner_ignorieren_sperrt_rechte() {
        let unknown = from_volume_bits(&[0; 4], &[0; 4], 0);
        assert_eq!(unknown, MetadataCapabilities::unknown());
        let ignored = from_volume_bits(&[0; 4], &[u32::MAX; 4], libc::MNT_IGNORE_OWNERSHIP as u64);
        assert_eq!(ignored.owner, Capability::Unsupported);
        assert_eq!(ignored.special_mode_bits, Capability::Unsupported);
        assert_eq!(ignored.hardlinks, Capability::Unsupported);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn fremde_mounts_und_fat_erhalten_keine_native_zusage() {
        assert_eq!(
            for_linux_filesystem("fuseblk"),
            MetadataCapabilities::unknown()
        );
        assert_eq!(for_linux_filesystem("nfs"), MetadataCapabilities::unknown());
        for fs in ["exfat", "vfat", "msdos"] {
            let caps = for_linux_filesystem(fs);
            assert_eq!(caps.owner, Capability::Unsupported);
            assert_eq!(caps.acl_model, AclModel::Unsupported);
            assert_eq!(caps.hardlinks, Capability::Unsupported);
        }
        assert_eq!(
            for_linux_filesystem("ext2"),
            MetadataCapabilities::unknown()
        );
        let listing = b"123 1 0:1 / /tmp/roh\xff rw - ext4 /dev/loop0 rw\n124 1 0:1 / /anderer rw - exfat /dev/loop1 rw\n";
        assert_eq!(filesystem_for_mount(listing, 123).unwrap(), "ext4");
        assert_eq!(filesystem_for_mount(listing, 124).unwrap(), "exfat");
        assert!(filesystem_for_mount(listing, 125).is_err());
    }
}
