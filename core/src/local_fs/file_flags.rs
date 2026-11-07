//! Sperrflag und ausgeblendete Endung am gehaltenen Objekt.
use super::*;
use crate::file_properties::FileFlagKind;
const FINDER_INFO: &[u8] = b"com.apple.FinderInfo";
const HIDDEN_EXTENSION: u8 = 0x10;

fn finder_info(fd: &OwnedFd) -> io::Result<Option<Vec<u8>>> {
    match read_xattr_value(fd, FINDER_INFO) {
        Ok(bytes) if bytes.len() == 32 => Ok(Some(bytes)),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Finder-Informationen haben keine gültige Länge; unverändert",
        )),
        Err(error) if error.raw_os_error() == Some(libc::ENOATTR) => Ok(None),
        Err(error) => Err(error),
    }
}
fn unclear(path: &Path, error: io::Error) -> io::Error {
    crate::error::with_recovery_paths(
        crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            format!("Dateieigenschaft geändert, Abschluss unklar: {error}"),
        ),
        &[path.to_string_lossy().into_owned()],
    )
}
pub(crate) fn access(
    root: &RootHandle,
    path: &Path,
    expected: &Entry,
    kind: FileFlagKind,
    enabled: Option<bool>,
    cancel: &crate::Cancel,
) -> io::Result<(bool, bool)> {
    let (fd, before) = open_interactive_metadata_entry(root, path, expected)?;
    if kind == FileFlagKind::HiddenExtension && !expected.is_file {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Ausgeblendete Erweiterungen benötigen eine reguläre Datei",
        ));
    }
    let finder = if kind == FileFlagKind::HiddenExtension {
        finder_info(&fd)?
    } else {
        None
    };
    let state = match kind {
        FileFlagKind::Locked => before.st_flags & libc::UF_IMMUTABLE != 0,
        FileFlagKind::HiddenExtension => finder
            .as_ref()
            .is_some_and(|bytes| bytes[9] & HIDDEN_EXTENSION != 0),
    };
    let current = hidden_flags::check_identity(root, path, expected, &before, &fd)?;
    if current.st_flags != before.st_flags
        || (kind == FileFlagKind::HiddenExtension && finder_info(&fd)? != finder)
    {
        return Err(crate::real_mode::plan_drift(
            "Dateieigenschaft während des Lesens verändert",
        ));
    }
    let Some(enabled) = enabled else {
        return Ok((state, false));
    };
    if cancel.is_cancelled() {
        return Err(crate::coded_error(
            io::ErrorKind::Interrupted,
            crate::ErrorCode::Interrupted,
            "Dateieigenschaft vor der ersten Wirkung abgebrochen",
        ));
    }
    if enabled == state {
        return Ok((state, false));
    }
    let mut desired_finder = finder.clone().unwrap_or_else(|| vec![0; 32]);
    let desired_flags =
        (before.st_flags & !libc::UF_IMMUTABLE) | if enabled { libc::UF_IMMUTABLE } else { 0 };
    let mutation: io::Result<()> = match kind {
        FileFlagKind::Locked => {
            if unsafe { libc::fchflags(fd.as_raw_fd(), desired_flags) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        FileFlagKind::HiddenExtension => {
            // Finder speichert die ausgeblendete Endung im Big-Endian-Flagwort,
            // Byte 9, Bit 0x10. Alle übrigen FinderInfo-Bytes bleiben erhalten.
            desired_finder[9] = (desired_finder[9] & !HIDDEN_EXTENSION)
                | if enabled { HIDDEN_EXTENSION } else { 0 };
            fsetxattr(
                &fd,
                OsStr::from_bytes(FINDER_INFO),
                &desired_finder,
                XattrFlags::empty(),
            )
            .map_err(Into::into)
        }
    };
    mutation.map_err(|error| {
        if matches!(
            error.raw_os_error(),
            Some(libc::EPERM | libc::EACCES | libc::EINVAL | libc::EROFS | libc::ENOTSUP)
        ) {
            error
        } else {
            unclear(path, error)
        }
    })?;
    let checked = (|| {
        let after = hidden_flags::check_identity(root, path, expected, &before, &fd)?;
        let actual = match kind {
            FileFlagKind::Locked => {
                if after.st_flags != desired_flags {
                    return Err(io::Error::other(
                        "Dateiflags nach der Sperränderung abweichend",
                    ));
                }
                after.st_flags & libc::UF_IMMUTABLE != 0
            }
            FileFlagKind::HiddenExtension => {
                if after.st_flags != before.st_flags
                    || finder_info(&fd)?.as_deref() != Some(desired_finder.as_slice())
                {
                    return Err(io::Error::other(
                        "Finder-Informationen nach der Änderung abweichend",
                    ));
                }
                enabled
            }
        };
        Ok((actual, true))
    })();
    checked.map_err(|error| unclear(path, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_properties::{change_local_file_flag, read_local_file_flags};
    use std::{fs, os::unix::fs::symlink};
    struct ResetFlags(OwnedFd);
    impl Drop for ResetFlags {
        fn drop(&mut self) {
            unsafe {
                libc::fchflags(self.0.as_raw_fd(), 0);
            }
        }
    }
    #[test]
    fn native_file_flags_roundtrip_preserves_data_metadata_and_hardlinks() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let path = root.join("file.txt");
        fs::write(&path, b"unchanged data").unwrap();
        fs::hard_link(&path, root.join("alias.txt")).unwrap();
        let held = RootHandle::open_nofollow(&root).unwrap();
        let reset = ResetFlags(open_metadata_entry_from(Some(&held), &path).unwrap());
        let mut finder = vec![0u8; 32];
        finder[..8].copy_from_slice(b"TEXTKEEP");
        finder[9] = 10;
        finder[20..24].copy_from_slice(b"KEEP");
        for (name, value) in [
            (FINDER_INFO, finder.as_slice()),
            (
                b"com.apple.ResourceFork",
                b"indispensable resource".as_slice(),
            ),
            (
                b"com.apple.metadata:_kMDItemUserTags",
                b"opaque tags".as_slice(),
            ),
            (b"com.doppeldecker.keep", b"other value".as_slice()),
        ] {
            set_xattr_for_test(&path, name, value).unwrap();
        }
        assert_eq!(
            unsafe { libc::fchflags(reset.0.as_raw_fd(), libc::UF_HIDDEN | libc::UF_NODUMP) },
            0
        );
        let before = fstat(&reset.0).unwrap();
        let names = ["file.txt".into()];
        let cancel = crate::Cancel::new();
        for kind in [FileFlagKind::HiddenExtension, FileFlagKind::Locked] {
            for (enabled, changed) in [(true, true), (true, false), (false, true)] {
                let rows =
                    change_local_file_flag(&root, &names, kind, enabled, Some(&root), &cancel)
                        .unwrap();
                assert!(rows[0].code.is_none(), "{rows:?}");
                assert_eq!(rows[0].changed, changed);
                assert_eq!(rows[0].state, Some(enabled));
                let read =
                    read_local_file_flags(&root, &["alias.txt".into()], kind, Some(&root), &cancel)
                        .unwrap();
                assert_eq!(read[0].state, Some(enabled));
                let after = fstat(&reset.0).unwrap();
                assert_eq!(
                    (
                        after.st_mode,
                        after.st_uid,
                        after.st_gid,
                        after.st_mtime,
                        after.st_mtime_nsec,
                        after.st_birthtime,
                        after.st_birthtime_nsec
                    ),
                    (
                        before.st_mode,
                        before.st_uid,
                        before.st_gid,
                        before.st_mtime,
                        before.st_mtime_nsec,
                        before.st_birthtime,
                        before.st_birthtime_nsec
                    )
                );
                assert_eq!(
                    after.st_flags,
                    (libc::UF_HIDDEN | libc::UF_NODUMP)
                        | if kind == FileFlagKind::Locked && enabled {
                            libc::UF_IMMUTABLE
                        } else {
                            0
                        }
                );
                assert_eq!(fs::read(&path).unwrap(), b"unchanged data");
                if kind == FileFlagKind::Locked && enabled {
                    assert!(fs::write(&path, b"forbidden mutation").is_err());
                }
                let mut expected_finder = finder.clone();
                if kind == FileFlagKind::HiddenExtension && enabled {
                    expected_finder[9] |= HIDDEN_EXTENSION;
                }
                assert_eq!(finder_info(&reset.0).unwrap(), Some(expected_finder));
                for (name, value) in [
                    (
                        b"com.apple.ResourceFork".as_slice(),
                        b"indispensable resource".as_slice(),
                    ),
                    (b"com.apple.metadata:_kMDItemUserTags", b"opaque tags"),
                    (b"com.doppeldecker.keep", b"other value"),
                ] {
                    assert_eq!(read_xattr_value(&reset.0, name).unwrap(), value);
                }
            }
        }
    }
    #[test]
    fn native_file_flags_reject_links_swaps_cancel_and_locked_attribute_write() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let path = root.join("file.txt");
        fs::write(&path, b"original").unwrap();
        fs::create_dir(root.join("folder")).unwrap();
        let held = RootHandle::open_nofollow(&root).unwrap();
        let reset = ResetFlags(open_metadata_entry_from(Some(&held), &path).unwrap());
        let expected = stat_beneath(&held, &path).unwrap();
        symlink("file.txt", root.join("link")).unwrap();
        for (name, kind) in [
            ("link", FileFlagKind::Locked),
            ("folder", FileFlagKind::HiddenExtension),
        ] {
            let rows = change_local_file_flag(
                &root,
                &[name.into()],
                kind,
                true,
                Some(&root),
                &crate::Cancel::new(),
            )
            .unwrap();
            assert!(rows[0].code.is_some());
        }
        let cancel = crate::Cancel::new();
        cancel.cancel();
        assert!(access(
            &held,
            &path,
            &expected,
            FileFlagKind::Locked,
            Some(true),
            &cancel
        )
        .is_err());
        assert_eq!(fstat(&reset.0).unwrap().st_flags, 0);
        assert_eq!(
            unsafe { libc::fchflags(reset.0.as_raw_fd(), libc::UF_IMMUTABLE) },
            0
        );
        let error = access(
            &held,
            &path,
            &expected,
            FileFlagKind::HiddenExtension,
            Some(true),
            &crate::Cancel::new(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(finder_info(&reset.0).unwrap().is_none());
        assert_eq!(unsafe { libc::fchflags(reset.0.as_raw_fd(), 0) }, 0);
        fs::rename(&path, root.join("old")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert_eq!(
            crate::error_code(
                &access(
                    &held,
                    &path,
                    &expected,
                    FileFlagKind::Locked,
                    Some(true),
                    &crate::Cancel::new()
                )
                .unwrap_err()
            ),
            crate::ErrorCode::PlanDrift
        );
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        assert_eq!(fstat(&reset.0).unwrap().st_flags, 0);
    }
}
