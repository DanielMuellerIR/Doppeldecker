//! macOS-Verstecktmarkierung am gehaltenen Objekt; andere Flags bleiben erhalten.
use super::*;
use crate::file_properties::VisibilityState;

pub(super) fn check_identity(
    root: &RootHandle,
    path: &Path,
    expected: &Entry,
    before: &Stat,
    fd: &OwnedFd,
) -> io::Result<Stat> {
    let current = fstat(fd)?;
    let actual = entry_from_stat(expected.name.clone(), &current, None);
    let visible = stat_beneath(root, path)?;
    if !crate::batch_rename::same_source(expected, &actual)
        || visible.device != actual.device
        || visible.inode != actual.inode
        || current.st_uid != before.st_uid
        || current.st_gid != before.st_gid
        || !root.is_current()?
    {
        return Err(crate::real_mode::plan_drift(
            "Objekt oder Ordner während der Dateieigenschaftsänderung verändert",
        ));
    }
    root.current_mount_identity()?;
    Ok(current)
}
fn state(name: &str, stat: &Stat) -> VisibilityState {
    VisibilityState {
        marked: stat_is_marked_hidden(stat),
        by_name: name.starts_with('.'),
    }
}
pub(crate) fn read_visibility_beneath(
    root: &RootHandle,
    path: &Path,
    expected: &Entry,
) -> io::Result<VisibilityState> {
    let (fd, before) = open_interactive_metadata_entry(root, path, expected)?;
    let after = check_identity(root, path, expected, &before, &fd)?;
    if after.st_flags != before.st_flags {
        return Err(crate::real_mode::plan_drift(
            "Sichtbarkeitsmarkierung wurde gleichzeitig verändert",
        ));
    }
    Ok(state(&expected.name, &after))
}
pub(crate) fn edit_visibility_beneath(
    root: &RootHandle,
    path: &Path,
    expected: &Entry,
    hidden: bool,
    cancel: &crate::Cancel,
) -> io::Result<(VisibilityState, bool)> {
    let (fd, before) = open_interactive_metadata_entry(root, path, expected)?;
    let desired = (before.st_flags & !libc::UF_HIDDEN) | if hidden { libc::UF_HIDDEN } else { 0 };
    let current = check_identity(root, path, expected, &before, &fd)?;
    if current.st_flags != before.st_flags {
        return Err(crate::real_mode::plan_drift(
            "Dateiflags wurden vor der Änderung gleichzeitig verändert",
        ));
    }
    if cancel.is_cancelled() {
        return Err(crate::coded_error(
            io::ErrorKind::Interrupted,
            crate::ErrorCode::Interrupted,
            "Sichtbarkeitsänderung vor der ersten Wirkung abgebrochen",
        ));
    }
    if desired == before.st_flags {
        return Ok((state(&expected.name, &before), false));
    }
    // UF_HIDDEN bildet auch die alte FinderInfo-Unsichtbarkeit ab. Der Kernel
    // entfernt dort nötigenfalls das Unsichtbarkeitsbit; andere Attribute bleiben erhalten.
    if unsafe { libc::fchflags(fd.as_raw_fd(), desired) } != 0 {
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::EPERM | libc::EACCES | libc::EINVAL | libc::EROFS | libc::ENOTSUP)
        ) {
            return Err(error);
        }
        return Err(unclear(path, error));
    }
    let checked = (|| {
        let after = check_identity(root, path, expected, &before, &fd)?;
        if after.st_flags != desired {
            return Err(io::Error::other("Dateiflags nach der Änderung abweichend"));
        }
        Ok((state(&expected.name, &after), true))
    })();
    checked.map_err(|error| unclear(path, error))
}
fn unclear(path: &Path, error: io::Error) -> io::Error {
    crate::error::with_recovery_paths(
        crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            format!("Sichtbarkeit geändert, Abschluss unklar: {error}"),
        ),
        &[path.to_string_lossy().into_owned()],
    )
}
pub(crate) fn observed_entry_is_hidden(
    root: Option<&RootHandle>,
    path: &Path,
    expected: &Entry,
) -> io::Result<bool> {
    let (parent, name) = open_parent_from(root, path)?;
    let current = statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if !crate::batch_rename::same_source(
        expected,
        &entry_from_stat(expected.name.clone(), &current, None),
    ) {
        return Err(crate::real_mode::plan_drift(
            "Objekt während der Sichtbarkeitsabfrage verändert",
        ));
    }
    if let Some(root) = root {
        if !root.is_current()? {
            return Err(crate::real_mode::plan_drift(
                "Suchwurzel während der Sichtbarkeitsabfrage verändert",
            ));
        }
    }
    Ok(expected.name.starts_with('.') || stat_is_marked_hidden(&current))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_properties::{change_local_visibility, read_local_visibility};
    use std::os::macos::fs::MetadataExt as _;
    use std::{
        fs,
        os::unix::fs::{symlink, MetadataExt},
    };

    #[test]
    fn hidden_flag_roundtrip_preserves_finder_color_tags_resource_data_and_hardlinks() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let path = root.join("file");
        fs::write(&path, b"data remains").unwrap();
        fs::hard_link(&path, root.join("alias")).unwrap();
        let mut finder = vec![0u8; 32];
        finder[9] = 12;
        finder[16..20].copy_from_slice(b"KEEP");
        for (name, value) in [
            ("com.apple.FinderInfo", finder.as_slice()),
            ("com.apple.metadata:_kMDItemUserTags", b"opaque tags"),
            ("com.apple.ResourceFork", b"fork remains"),
            ("com.doppeldecker.keep", b"other remains"),
        ] {
            set_xattr_for_test(&path, name.as_bytes(), value).unwrap();
        }
        let before = fs::metadata(&path).unwrap();
        for hidden in [true, true, false] {
            let rows = change_local_visibility(
                &root,
                &["file".into()],
                hidden,
                Some(&root),
                &crate::Cancel::new(),
            )
            .unwrap();
            assert!(rows[0].code.is_none(), "{rows:?}");
            assert_eq!(rows[0].state.as_ref().unwrap().marked, hidden);
            let alias =
                read_local_visibility(&root, &["alias".into()], Some(&root), &crate::Cancel::new())
                    .unwrap();
            assert_eq!(alias[0].state.as_ref().unwrap().marked, hidden);
            let actual = fs::metadata(&path).unwrap();
            assert_eq!(
                actual.st_flags() & libc::UF_HIDDEN,
                if hidden { libc::UF_HIDDEN } else { 0 }
            );
            assert_eq!(
                (
                    actual.mode(),
                    actual.uid(),
                    actual.gid(),
                    actual.mtime(),
                    actual.mtime_nsec()
                ),
                (
                    before.mode(),
                    before.uid(),
                    before.gid(),
                    before.mtime(),
                    before.mtime_nsec()
                )
            );
            assert_eq!(fs::read(&path).unwrap(), b"data remains");
            let expected = finder.clone();
            assert_eq!(
                read_xattr_value(
                    &open_metadata_entry_from(None, &path).unwrap(),
                    b"com.apple.FinderInfo"
                )
                .unwrap(),
                expected
            );
            for (name, expected) in [
                (
                    "com.apple.metadata:_kMDItemUserTags",
                    b"opaque tags".as_slice(),
                ),
                ("com.apple.ResourceFork", b"fork remains"),
                ("com.doppeldecker.keep", b"other remains"),
            ] {
                assert_eq!(
                    read_xattr_value(
                        &open_metadata_entry_from(None, &path).unwrap(),
                        name.as_bytes()
                    )
                    .unwrap(),
                    expected
                );
            }
        }
        let mut legacy = finder.clone();
        legacy[8] |= 0x40;
        set_xattr_for_test(&path, b"com.apple.FinderInfo", &legacy).unwrap();
        assert_ne!(fs::metadata(&path).unwrap().st_flags() & libc::UF_HIDDEN, 0);
        let cleared = change_local_visibility(
            &root,
            &["file".into()],
            false,
            Some(&root),
            &crate::Cancel::new(),
        )
        .unwrap();
        assert!(cleared[0].changed);
        assert_eq!(
            read_xattr_value(
                &open_metadata_entry_from(None, &path).unwrap(),
                b"com.apple.FinderInfo"
            )
            .unwrap(),
            finder
        );
        let same = change_local_visibility(
            &root,
            &["file".into()],
            false,
            Some(&root),
            &crate::Cancel::new(),
        )
        .unwrap();
        assert!(!same[0].changed);
        fs::write(root.join(".dot"), b"dot").unwrap();
        let dot = change_local_visibility(
            &root,
            &[".dot".into()],
            false,
            Some(&root),
            &crate::Cancel::new(),
        )
        .unwrap();
        assert_eq!(
            dot[0].state,
            Some(VisibilityState {
                marked: false,
                by_name: true
            })
        );
    }

    #[test]
    fn hidden_flag_rejects_links_drift_and_cancel_without_touching_substitutes() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        fs::write(root.join("file"), b"original").unwrap();
        symlink("file", root.join("link")).unwrap();
        let rows = change_local_visibility(
            &root,
            &["link".into()],
            true,
            Some(&root),
            &crate::Cancel::new(),
        )
        .unwrap();
        assert!(rows[0].code.is_some());
        let held = RootHandle::open_nofollow(&root).unwrap();
        let expected = stat_beneath(&held, &root.join("file")).unwrap();
        fs::rename(root.join("file"), root.join("saved")).unwrap();
        fs::write(root.join("file"), b"substitute").unwrap();
        assert!(edit_visibility_beneath(
            &held,
            &root.join("file"),
            &expected,
            true,
            &crate::Cancel::new()
        )
        .is_err());
        let cancel = crate::Cancel::new();
        cancel.cancel();
        let rows =
            change_local_visibility(&root, &["file".into()], true, Some(&root), &cancel).unwrap();
        assert_eq!(rows[0].code, Some(crate::ErrorCode::Interrupted));
        assert_eq!(
            fs::metadata(root.join("file")).unwrap().st_flags() & libc::UF_HIDDEN,
            0
        );
        assert_eq!(
            fs::metadata(root.join("saved")).unwrap().st_flags() & libc::UF_HIDDEN,
            0
        );
        let old_root = root.with_file_name(format!(
            "{}-moved",
            root.file_name().unwrap().to_str().unwrap()
        ));
        fs::rename(&root, &old_root).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file"), b"new root").unwrap();
        let expected_saved = stat_beneath(
            &RootHandle::open_nofollow(&old_root).unwrap(),
            &old_root.join("saved"),
        )
        .unwrap();
        assert!(edit_visibility_beneath(
            &held,
            &root.join("saved"),
            &expected_saved,
            true,
            &crate::Cancel::new()
        )
        .is_err());
        assert_eq!(
            fs::metadata(old_root.join("saved")).unwrap().st_flags() & libc::UF_HIDDEN,
            0
        );
        fs::remove_file(root.join("file")).unwrap();
        fs::remove_dir(&root).unwrap();
        fs::rename(&old_root, &root).unwrap();
        let error = read_local_visibility(
            &root,
            &vec!["file".into(); 41],
            Some(&root),
            &crate::Cancel::new(),
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::InvalidInput);
    }

    #[test]
    fn display_and_recursive_search_respect_native_hidden_files_and_directories() {
        use crate::name_search::{
            search_names, NameSearchLimits, NameSearchOptions, VfsNameSearchReader,
        };
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        fs::create_dir(root.join("hidden-dir")).unwrap();
        fs::write(root.join("hidden-dir/needle.txt"), b"").unwrap();
        fs::write(root.join("needle-hidden.txt"), b"").unwrap();
        fs::write(root.join("needle-visible.txt"), b"").unwrap();
        let rows = change_local_visibility(
            &root,
            &["hidden-dir".into(), "needle-hidden.txt".into()],
            true,
            Some(&root),
            &crate::Cancel::new(),
        )
        .unwrap();
        assert!(rows.iter().all(|row| row.changed), "{rows:?}");
        let vfs = crate::LocalVfs::new();
        let entries = vfs.list_display_entries(&root).unwrap();
        assert_eq!(entries.iter().filter(|entry| entry.is_hidden).count(), 2);
        for include_hidden in [false, true] {
            let mut hits = Vec::new();
            search_names(
                &VfsNameSearchReader(&vfs),
                &root,
                &NameSearchOptions {
                    query: "needle".into(),
                    case_sensitive: false,
                    include_hidden,
                },
                NameSearchLimits::default(),
                &crate::Cancel::new(),
                &mut |update| hits.extend(update.hits.clone()),
            )
            .unwrap();
            assert_eq!(hits.len(), if include_hidden { 3 } else { 1 });
            if include_hidden {
                assert!(
                    hits.iter()
                        .find(|hit| hit.name == "needle-hidden.txt")
                        .unwrap()
                        .is_hidden
                );
            } else {
                assert_eq!(hits[0].name, "needle-visible.txt");
            }
        }
    }
}
