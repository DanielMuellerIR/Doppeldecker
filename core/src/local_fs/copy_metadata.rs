//! Lokale Kopiermetadaten am gehaltenen Objekt.
#[path = "copy_metadata_attributes.rs"]
pub(crate) mod attributes;
use super::*;
use crate::vfs::FileCreationTime;

fn open_entry(root: Option<&RootHandle>, path: &Path) -> io::Result<OwnedFd> {
    let fd = open_final_from(root, path, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty())?;
    let stat = fstat(&fd)?;
    if !matches!(
        FileType::from_raw_mode(stat.st_mode),
        FileType::RegularFile | FileType::Directory
    ) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Erstellungszeit benötigt eine reguläre Datei oder einen Ordner",
        ));
    }
    Ok(fd)
}
fn value(stat: &Stat) -> io::Result<FileCreationTime> {
    let time = FileCreationTime {
        seconds: stat.st_birthtime,
        nanoseconds: u32::try_from(stat.st_birthtime_nsec)
            .map_err(|_| io::Error::other("Ungültige Erstellungszeit"))?,
    };
    time.validate()?;
    Ok(time)
}
fn check_visible(root: Option<&RootHandle>, path: &Path, fd: &OwnedFd) -> io::Result<Stat> {
    let current = fstat(fd)?;
    let visible = stat_from(root, path)?;
    if let Some(held) = root {
        if !held.is_current()? {
            return Err(crate::real_mode::plan_drift(
                "Wurzel während der Erstellungszeitaktion verändert",
            ));
        }
    }
    if visible.device != Some(current.st_dev as u64) || visible.inode != Some(current.st_ino) {
        return Err(crate::real_mode::plan_drift(
            "Objekt oder Wurzel während der Erstellungszeitaktion verändert",
        ));
    }
    if let Some(root) = root {
        root.current_mount_identity()?;
    }
    Ok(current)
}

pub(crate) fn read_copy_creation_time(
    root: Option<&RootHandle>,
    path: &Path,
    expected: &Entry,
) -> io::Result<Option<FileCreationTime>> {
    let fd = open_entry(root, path)?;
    let before = check_visible(root, path, &fd)?;
    let actual = entry_from_stat(expected.name.clone(), &before, None);
    if !crate::batch_rename::same_source(expected, &actual) {
        return Err(crate::real_mode::plan_drift(
            "Quelle vor dem Lesen der Erstellungszeit verändert",
        ));
    }
    Ok(Some(value(&before)?))
}

pub(crate) fn set_copy_creation_time(
    root: Option<&RootHandle>,
    path: &Path,
    time: FileCreationTime,
) -> io::Result<()> {
    time.validate()?;
    let fd = open_entry(root, path)?;
    let before = check_visible(root, path, &fd)?;
    if value(&before)? == time {
        return Ok(());
    }
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_CRTIME,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut timestamp = libc::timespec {
        tv_sec: time.seconds,
        tv_nsec: i64::from(time.nanoseconds),
    };
    // Der Deskriptor bindet die Änderung an das geöffnete Objekt. fsetattrlist
    // erwartet für genau ATTR_CMN_CRTIME einen einzelnen nativen timespec.
    if unsafe {
        libc::fsetattrlist(
            fd.as_raw_fd(),
            (&mut attributes as *mut libc::attrlist).cast(),
            (&mut timestamp as *mut libc::timespec).cast(),
            std::mem::size_of::<libc::timespec>(),
            0,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::EINVAL | libc::ENOTSUP | libc::EACCES | libc::EPERM)
        ) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("Das Ziel kann diese Erstellungszeit nicht übernehmen: {error}"),
            ));
        }
        return Err(error);
    }
    let after = check_visible(root, path, &fd)?;
    if after.st_dev != before.st_dev || after.st_ino != before.st_ino || value(&after)? != time {
        return Err(crate::real_mode::plan_drift(
            "Erstellungszeit nach der Änderung abweichend",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn creation_time_rejects_swapped_objects_links_and_invalid_values_without_external_effect() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let path = root.join("file");
        fs::write(&path, b"original").unwrap();
        let held = RootHandle::open_nofollow(&root).unwrap();
        let expected = stat_beneath(&held, &path).unwrap();
        fs::rename(&path, root.join("old")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        let error = read_copy_creation_time(Some(&held), &path, &expected).unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(fs::read(&path).unwrap(), b"replacement");

        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().canonicalize().unwrap().join("sentinel");
        fs::write(&sentinel, b"external content").unwrap();
        let before = fs::metadata(&sentinel).unwrap();
        symlink(&sentinel, root.join("link")).unwrap();
        let time = FileCreationTime {
            seconds: 1_000_000_000,
            nanoseconds: 942_123_456,
        };
        assert!(set_copy_creation_time(Some(&held), &root.join("link"), time).is_err());
        let after = fs::metadata(&sentinel).unwrap();
        assert_eq!(before.created().unwrap(), after.created().unwrap());
        assert_eq!(fs::read(&sentinel).unwrap(), b"external content");
        let error = set_copy_creation_time(
            Some(&held),
            &root.join("absent"),
            FileCreationTime {
                nanoseconds: 1_000_000_000,
                ..time
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
