//! Einzelne Finder-Attribute am gehaltenen Objekt, ohne vollständigen xattr-Ersatz.
use super::*;
use crate::file_properties::tags::{
    clear_legacy_color, decode_state, encode_tags, FinderTagEdit, FinderTagState, MAX_TAG_BYTES,
};

const TAGS: &str = "com.apple.metadata:_kMDItemUserTags";
const FINDER: &str = "com.apple.FinderInfo";

#[cfg(test)]
thread_local! { static FAIL_FINDER_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

#[derive(Clone, PartialEq, Eq)]
struct RawTags {
    tags: Option<Vec<u8>>,
    finder: Option<Vec<u8>>,
}
pub(super) fn read_attribute(
    fd: &OwnedFd,
    name: &str,
    limit: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut empty = [0_u8; 0];
    let size = match fgetxattr(fd, OsStr::new(name), &mut empty) {
        Ok(size) => size,
        Err(error) if error.raw_os_error() == libc::ENOATTR => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if size > limit {
        return Err(crate::coded_error(
            io::ErrorKind::InvalidData,
            crate::ErrorCode::InvalidData,
            "Finder-Attribut überschreitet seine Größe; unverändert",
        ));
    }
    let mut value = vec![0; size];
    let read = fgetxattr(fd, OsStr::new(name), &mut value)?;
    if read != size {
        return Err(crate::real_mode::plan_drift(
            "Finder-Attribut änderte sich beim Lesen",
        ));
    }
    Ok(Some(value))
}
fn read_raw(fd: &OwnedFd) -> io::Result<RawTags> {
    Ok(RawTags {
        tags: read_attribute(fd, TAGS, MAX_TAG_BYTES)?,
        finder: read_attribute(fd, FINDER, 32)?,
    })
}
fn check_identity(
    root: &RootHandle,
    path: &Path,
    expected: &Entry,
    before: &Stat,
    fd: &OwnedFd,
) -> io::Result<()> {
    let after = fstat(fd)?;
    let actual = entry_from_stat(expected.name.clone(), &after, None);
    let visible = stat_beneath(root, path)?;
    if !crate::batch_rename::same_source(expected, &actual)
        || visible.device != actual.device
        || visible.inode != actual.inode
        || after.st_uid != before.st_uid
        || after.st_gid != before.st_gid
        || !root.is_current()?
    {
        return Err(crate::real_mode::plan_drift(
            "Objekt oder Ordner während der Finder-Tagaktion verändert",
        ));
    }
    root.current_mount_identity()?;
    Ok(())
}

pub(crate) fn read_interactive_finder_tags_beneath(
    root: &RootHandle,
    path: &Path,
    expected: &Entry,
) -> io::Result<FinderTagState> {
    let (fd, before) = open_interactive_metadata_entry(root, path, expected)?;
    let raw = read_raw(&fd)?;
    let state = decode_state(raw.tags.as_deref(), raw.finder.as_deref())?;
    if read_raw(&fd)? != raw {
        return Err(crate::real_mode::plan_drift(
            "Finder-Tags wurden gleichzeitig verändert",
        ));
    }
    check_identity(root, path, expected, &before, &fd)?;
    Ok(state)
}

fn write_attribute(fd: &OwnedFd, name: &str, value: Option<&[u8]>) -> io::Result<()> {
    #[cfg(test)]
    if name == FINDER && FAIL_FINDER_WRITE.with(|fail| fail.get()) {
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
    match value {
        Some(value) => {
            fsetxattr(fd, OsStr::new(name), value, XattrFlags::empty()).map_err(Into::into)
        }
        None => fremovexattr(fd, OsStr::new(name)).map_err(Into::into),
    }
}
fn unclear(path: &Path, error: io::Error) -> io::Error {
    crate::error::with_recovery_paths(
        crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            format!("Finder-Tagänderung begonnen, Abschluss unklar: {error}"),
        ),
        &[path.to_string_lossy().into_owned()],
    )
}
pub(crate) fn edit_interactive_finder_tags_beneath(
    root: &RootHandle,
    path: &Path,
    expected: &Entry,
    edit: &FinderTagEdit,
    cancel: &crate::Cancel,
) -> io::Result<(FinderTagState, bool)> {
    edit.validate()?;
    let (fd, before) = open_interactive_metadata_entry(root, path, expected)?;
    let raw = read_raw(&fd)?;
    let state = decode_state(raw.tags.as_deref(), raw.finder.as_deref())?;
    let desired = edit.apply(&state)?;
    let mut wanted = raw.clone();
    if desired.tags != state.tags {
        wanted.tags = if desired.tags.is_empty() {
            None
        } else {
            Some(encode_tags(&desired.tags)?)
        };
    }
    if desired.legacy_color != state.legacy_color {
        wanted.finder =
            Some(clear_legacy_color(raw.finder.as_deref().ok_or_else(
                || io::Error::other("Farbmarkierung ohne FinderInfo"),
            )?)?);
    }
    check_identity(root, path, expected, &before, &fd)?;
    if read_raw(&fd)? != raw {
        return Err(crate::real_mode::plan_drift(
            "Finder-Tags wurden vor der Änderung gleichzeitig verändert",
        ));
    }
    if cancel.is_cancelled() {
        return Err(crate::coded_error(
            io::ErrorKind::Interrupted,
            crate::ErrorCode::Interrupted,
            "Tagänderung vor der ersten Wirkung abgebrochen; unverändert",
        ));
    }
    let mut touched = false;
    for (name, previous, value) in [
        (TAGS, &raw.tags, &wanted.tags),
        (FINDER, &raw.finder, &wanted.finder),
    ] {
        if previous == value {
            continue;
        }
        if let Err(error) = write_attribute(&fd, name, value.as_deref()) {
            if !touched && error.raw_os_error() == Some(libc::ENOATTR) {
                return Err(crate::real_mode::plan_drift(
                    "Finder-Attribut vor der Entfernung verändert; keine eigene Wirkung",
                ));
            }
            let unchanged = matches!(
                error.raw_os_error(),
                Some(
                    libc::EPERM
                        | libc::EACCES
                        | libc::EINVAL
                        | libc::EROFS
                        | libc::ENOTSUP
                        | libc::ENOATTR
                )
            );
            return Err(if !touched && unchanged {
                error
            } else {
                unclear(path, error)
            });
        }
        touched = true;
        // Ein angefangener Zwei-Attribut-Abschluss wird auch nach Abbruch fertig
        // geprüft. Sonst könnte „Tags leeren“ die alte Farbmarkierung verbergen.
    }
    let checked = (|| {
        if read_raw(&fd)? != wanted {
            return Err(io::Error::other(
                "Finder-Attribute nach der Änderung abweichend",
            ));
        }
        check_identity(root, path, expected, &before, &fd)?;
        Ok((desired, touched))
    })();
    checked.map_err(|error| if touched { unclear(path, error) } else { error })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_properties::{change_local_finder_tags, read_local_finder_tags, FinderTag};
    use std::{fs, os::unix::fs::symlink};
    fn fixture() -> (tempfile::TempDir, PathBuf, Vec<u8>, Vec<u8>) {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        fs::write(root.join("file"), b"unchanged content").unwrap();
        fs::hard_link(root.join("file"), root.join("alias")).unwrap();
        let tags = encode_tags(&[
            FinderTag {
                name: "Projekt".into(),
                color: 4,
            },
            FinderTag {
                name: "Archiv".into(),
                color: 0,
            },
        ])
        .unwrap();
        let mut finder = vec![0; 32];
        finder[8] = 0x40;
        finder[9] = 12;
        finder[16..20].copy_from_slice(b"TEST");
        for (name, value) in [
            (TAGS.as_bytes(), tags.as_slice()),
            (FINDER.as_bytes(), finder.as_slice()),
            (b"com.doppeldecker.keep", b"other xattrs".as_slice()),
            (b"com.apple.ResourceFork", b"resource fork".as_slice()),
        ] {
            set_xattr_for_test(&root.join("file"), name, value).unwrap();
        }
        (area, root, tags, finder)
    }
    fn raw(root: &Path, name: &str) -> RawTags {
        let held = RootHandle::open_nofollow(root).unwrap();
        let path = root.join(name);
        let entry = stat_beneath(&held, &path).unwrap();
        let (fd, _) = open_interactive_metadata_entry(&held, &path, &entry).unwrap();
        read_raw(&fd).unwrap()
    }
    #[test]
    fn edits_and_clear_preserve_content_other_attributes_finder_bytes_and_hardlinks() {
        let (_area, root, _tags, finder) = fixture();
        let edit = FinderTagEdit::Upsert {
            tag: FinderTag {
                name: "Neu".into(),
                color: 2,
            },
            previous_name: None,
        };
        let rows = change_local_finder_tags(
            &root,
            &["file".into()],
            &edit,
            Some(&root),
            &crate::Cancel::new(),
        )
        .unwrap();
        assert!(rows[0].changed, "{rows:?}");
        let read = read_local_finder_tags(
            &root,
            &["file".into(), "alias".into()],
            Some(&root),
            &crate::Cancel::new(),
        )
        .unwrap();
        assert_eq!(read[0].state, read[1].state);
        assert_eq!(read[0].state.as_ref().unwrap().tags.len(), 3);
        assert_eq!(raw(&root, "file").finder, Some(finder.clone()));
        let rows = change_local_finder_tags(
            &root,
            &["file".into()],
            &FinderTagEdit::Clear,
            Some(&root),
            &crate::Cancel::new(),
        )
        .unwrap();
        assert!(rows[0].changed, "{rows:?}");
        let cleared = raw(&root, "alias");
        assert_eq!(cleared.tags, None);
        assert_eq!(cleared.finder, Some(clear_legacy_color(&finder).unwrap()));
        assert_eq!(fs::read(root.join("file")).unwrap(), b"unchanged content");
        let held = RootHandle::open_nofollow(&root).unwrap();
        let values = read_xattrs_beneath(&held, &root.join("file")).unwrap();
        assert!(values
            .iter()
            .any(|value| value.name == b"com.doppeldecker.keep" && value.value == b"other xattrs"));
        assert_eq!(
            fs::read(root.join("file/..namedfork/rsrc")).unwrap(),
            b"resource fork"
        );
    }
    #[test]
    fn corrupt_tags_and_links_leave_valid_independent_files_editable() {
        let (_area, root, tags, _) = fixture();
        fs::write(root.join("bad"), b"bad metadata only").unwrap();
        set_xattr_for_test(&root.join("bad"), TAGS.as_bytes(), b"broken plist").unwrap();
        symlink("file", root.join("link")).unwrap();
        let rows = change_local_finder_tags(
            &root,
            &["bad".into(), "link".into(), "file".into()],
            &FinderTagEdit::ClearLegacy,
            Some(&root),
            &crate::Cancel::new(),
        )
        .unwrap();
        assert_eq!(rows[0].code, Some(crate::ErrorCode::InvalidData));
        assert_eq!(rows[1].code, Some(crate::ErrorCode::Unsupported));
        assert!(rows[2].changed);
        assert_eq!(raw(&root, "file").tags, Some(tags));
        assert_eq!(raw(&root, "bad").tags, Some(b"broken plist".to_vec()));
        let cancelled = crate::Cancel::new();
        cancelled.cancel();
        let before = raw(&root, "file");
        let rows = change_local_finder_tags(
            &root,
            &["file".into()],
            &FinderTagEdit::Clear,
            Some(&root),
            &cancelled,
        )
        .unwrap();
        assert_eq!(rows[0].code, Some(crate::ErrorCode::Interrupted));
        assert_eq!(raw(&root, "file").tags, before.tags);
    }
    #[test]
    fn second_attribute_failure_keeps_partial_effect_and_reports_unclear_without_rollback() {
        let (_area, root, _tags, finder) = fixture();
        FAIL_FINDER_WRITE.with(|fail| fail.set(true));
        let result = change_local_finder_tags(
            &root,
            &["file".into()],
            &FinderTagEdit::Clear,
            Some(&root),
            &crate::Cancel::new(),
        );
        FAIL_FINDER_WRITE.with(|fail| fail.set(false));
        let rows = result.unwrap();
        assert_eq!(rows[0].code, Some(crate::ErrorCode::OutcomeUnclear));
        assert!(!rows[0].changed);
        let actual = raw(&root, "file");
        assert_eq!(actual.tags, None);
        assert_eq!(actual.finder, Some(finder));
        assert_eq!(fs::read(root.join("file")).unwrap(), b"unchanged content");
    }
    #[test]
    fn swapped_object_invalid_action_and_oversized_read_page_do_not_mutate_targets() {
        let (_area, root, _tags, _) = fixture();
        let held = RootHandle::open_nofollow(&root).unwrap();
        let path = root.join("file");
        let expected = stat_beneath(&held, &path).unwrap();
        fs::rename(&path, root.join("old")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        let error = edit_interactive_finder_tags_beneath(
            &held,
            &path,
            &expected,
            &FinderTagEdit::Clear,
            &crate::Cancel::new(),
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(raw(&root, "file").tags, None);
        let error = change_local_finder_tags(
            &root,
            &["alias".into()],
            &FinderTagEdit::Upsert {
                tag: FinderTag {
                    name: "invalid".into(),
                    color: 8,
                },
                previous_name: None,
            },
            Some(&root),
            &crate::Cancel::new(),
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::InvalidInput);
        let names = (0..41).map(|i| format!("file{i}")).collect::<Vec<_>>();
        assert_eq!(
            crate::error_code(
                &read_local_finder_tags(&root, &names, Some(&root), &crate::Cancel::new())
                    .unwrap_err()
            ),
            crate::ErrorCode::InvalidInput
        );
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }
}
