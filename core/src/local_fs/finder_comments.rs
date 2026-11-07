//! Objektgebundene Finder-Kommentare ohne Pfadfreigabe an den nativen Adapter.
use super::*;
use crate::file_properties::{
    validate_finder_comment, FinderCommentAccess, FinderCommentState, FinderCommentWriteError,
};
use std::os::fd::AsFd;
use unicode_normalization::UnicodeNormalization;
const COMMENT: &str = "com.apple.metadata:kMDItemFinderComment";

struct BoundComment<'a> {
    root: &'a RootHandle,
    path: &'a Path,
    expected: &'a Entry,
    fd: OwnedFd,
    before: Stat,
}
impl<'a> BoundComment<'a> {
    fn open(root: &'a RootHandle, path: &'a Path, expected: &'a Entry) -> io::Result<Self> {
        let (fd, before) = open_interactive_metadata_entry(root, path, expected)?;
        if before.st_nlink > 1 && expected.is_file {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "Finder-Kommentare sind bei mehreren Hardlinknamen nicht eindeutig einem Namen zuzuordnen; unverändert"));
        }
        Ok(Self {
            root,
            path,
            expected,
            fd,
            before,
        })
    }
    fn check(&self) -> io::Result<()> {
        let current = fstat(&self.fd)?;
        let actual = entry_from_stat(self.expected.name.clone(), &current, None);
        let visible = stat_beneath(self.root, self.path)?;
        if !crate::batch_rename::same_source(self.expected, &actual)
            || visible.device != actual.device
            || visible.inode != actual.inode
            || current.st_uid != self.before.st_uid
            || current.st_gid != self.before.st_gid
            || current.st_flags != self.before.st_flags
            || current.st_nlink != self.before.st_nlink
            || !self.root.is_current()?
        {
            return Err(crate::real_mode::plan_drift(
                "Objekt oder Ordner während des Kommentarzugriffs verändert",
            ));
        }
        self.root.current_mount_identity()?;
        Ok(())
    }
    fn raw(&self) -> io::Result<Option<Vec<u8>>> {
        finder_tags::read_attribute(&self.fd, COMMENT, 64 * 1024)
    }
    fn read(
        &self,
        access: &mut dyn FinderCommentAccess,
    ) -> io::Result<(FinderCommentState, Option<Vec<u8>>)> {
        self.check()?;
        let text = access.read(self.fd.as_fd())?;
        validate_finder_comment(&text).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Finder-Kommentar überschreitet seine Textgrenzen",
            )
        })?;
        self.check()?;
        let raw = self.raw()?;
        self.check()?;
        if self.raw()? != raw {
            return Err(crate::real_mode::plan_drift(
                "Kommentar-Attribut wurde während des Lesens verändert",
            ));
        }
        let metadata_differs = match crate::file_properties::decode_comment(raw.as_deref()) {
            Ok(value) => !same_comment(&value, &text),
            Err(_) => true,
        };
        Ok((
            FinderCommentState {
                text,
                metadata_differs,
            },
            raw,
        ))
    }
}
pub(crate) fn read_comment_beneath(
    root: &RootHandle,
    path: &Path,
    expected: &Entry,
    access: &mut dyn FinderCommentAccess,
) -> io::Result<FinderCommentState> {
    BoundComment::open(root, path, expected)?
        .read(access)
        .map(|(state, _)| state)
}
pub(crate) fn edit_comment_beneath(
    root: &RootHandle,
    path: &Path,
    expected: &Entry,
    text: &str,
    cancel: &crate::Cancel,
    access: &mut dyn FinderCommentAccess,
) -> io::Result<(FinderCommentState, bool)> {
    let target = BoundComment::open(root, path, expected)?;
    let (before, raw) = target.read(access)?;
    if cancel.is_cancelled() {
        return Err(crate::coded_error(
            io::ErrorKind::Interrupted,
            crate::ErrorCode::Interrupted,
            "Kommentaränderung vor dem Schreiben abgebrochen",
        ));
    }
    if same_comment(&before.text, text) && !before.metadata_differs {
        return Ok((before, false));
    }
    target.check()?;
    if target.raw()? != raw {
        return Err(crate::real_mode::plan_drift(
            "Kommentar wurde vor dem Schreiben verändert",
        ));
    }
    match access.write(target.fd.as_fd(), text) {
        Ok(()) => {}
        Err(FinderCommentWriteError::NotStarted(error)) => return Err(error),
        Err(FinderCommentWriteError::OutcomeUnclear(error)) => return Err(unclear(path, error)),
    }
    let result = (|| {
        let (after, _) = target.read(access)?;
        if !same_comment(&after.text, text) || after.metadata_differs {
            return Err(io::Error::other(
                "Finder-Kommentar und Attribut nach dem Schreiben nicht übereinstimmend",
            ));
        }
        Ok((after, true))
    })();
    result.map_err(|error| unclear(path, error))
}
// Finder normalisiert etwa „ä“ zu „a“ plus kombinierendem Trema.
// Nur kanonische Unicode-Äquivalenz gilt als derselbe Kommentar.
fn same_comment(left: &str, right: &str) -> bool {
    left.nfc().eq(right.nfc())
}
fn unclear(path: &Path, error: io::Error) -> io::Error {
    crate::error::with_recovery_paths(
        crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            format!(
                "Kommentaränderung möglicherweise ausgeführt; vor erneutem Versuch prüfen: {error}"
            ),
        ),
        &[path.to_string_lossy().into_owned()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_properties::{change_local_finder_comments, read_local_finder_comments};
    use std::{fs, os::unix::fs::symlink};

    #[derive(Default)]
    struct Finder {
        text: String,
        reads: usize,
        writes: usize,
        lose_reply: bool,
        deny_write: bool,
        mismatch: bool,
        decompose: bool,
        cancel_on_read: Option<crate::Cancel>,
        replace_on_read: Option<PathBuf>,
        link_on_read: Option<PathBuf>,
    }
    impl FinderCommentAccess for Finder {
        fn read(&mut self, object: std::os::fd::BorrowedFd<'_>) -> io::Result<String> {
            self.reads += 1;
            assert!(fstat(object)?.st_ino > 0);
            if let Some(cancel) = self.cancel_on_read.take() {
                cancel.cancel();
            }
            if let Some(path) = self.replace_on_read.take() {
                fs::rename(&path, path.with_extension("saved"))?;
                fs::write(path, b"substitute")?;
            }
            if let Some(path) = self.link_on_read.take() {
                fs::hard_link(&path, path.with_extension("linked"))?;
            }
            Ok(self.text.clone())
        }
        fn write(
            &mut self,
            object: std::os::fd::BorrowedFd<'_>,
            text: &str,
        ) -> Result<(), FinderCommentWriteError> {
            if self.deny_write {
                return Err(FinderCommentWriteError::NotStarted(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "no permission",
                )));
            }
            self.writes += 1;
            let text = if self.decompose {
                text.nfd().collect::<String>()
            } else {
                text.into()
            };
            let mut raw = Vec::new();
            plist::to_writer_binary(&mut raw, &text).unwrap();
            fsetxattr(object, OsStr::new(COMMENT), &raw, XattrFlags::empty())
                .map_err(|error| FinderCommentWriteError::OutcomeUnclear(error.into()))?;
            if self.lose_reply {
                return Err(FinderCommentWriteError::OutcomeUnclear(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "reply lost",
                )));
            }
            if !self.mismatch {
                self.text = text;
            }
            Ok(())
        }
    }
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        fs::write(root.join("file"), b"data remains").unwrap();
        set_xattr_for_test(&root.join("file"), b"com.doppeldecker.keep", b"kept").unwrap();
        set_xattr_for_test(
            &root.join("file"),
            b"com.apple.ResourceFork",
            b"fork remains",
        )
        .unwrap();
        (area, root)
    }
    #[test]
    fn finder_comments_stop_if_a_second_name_appears_during_read() {
        let (_area, root) = fixture();
        let mut access = Finder {
            link_on_read: Some(root.join("file")),
            ..Default::default()
        };
        let rows = change_local_finder_comments(
            &root,
            &["file".into()],
            "new",
            Some(&root),
            &crate::Cancel::new(),
            &mut access,
        )
        .unwrap();
        assert_eq!(rows[0].code, Some(crate::ErrorCode::PlanDrift));
        assert_eq!(access.writes, 0);
    }
    #[test]
    fn finder_comments_accept_only_canonically_equivalent_native_text() {
        let (_area, root) = fixture();
        let mut access = Finder {
            decompose: true,
            ..Default::default()
        };
        let text = "Grüße – ä & < >\nZweite Zeile";
        let rows = change_local_finder_comments(
            &root,
            &["file".into()],
            text,
            Some(&root),
            &crate::Cancel::new(),
            &mut access,
        )
        .unwrap();
        assert!(rows[0].changed, "{rows:?}");
        assert_eq!(
            rows[0].state.as_ref().unwrap().text,
            text.nfd().collect::<String>()
        );
        let again = change_local_finder_comments(
            &root,
            &["file".into()],
            text,
            Some(&root),
            &crate::Cancel::new(),
            &mut access,
        )
        .unwrap();
        assert!(!again[0].changed);
        assert_eq!(access.writes, 1);
        assert!(!same_comment("①", "1"));
        assert!(!same_comment("A", "a"));
        assert!(!same_comment("line\n", "line"));
    }
    #[test]
    fn finder_comments_reconcile_stale_attribute_and_preserve_other_forks_and_data() {
        let (_area, root) = fixture();
        set_xattr_for_test(
            &root.join("file"),
            COMMENT.as_bytes(),
            b"broken old attribute",
        )
        .unwrap();
        let mut access = Finder {
            text: "Finder owns the current comment".into(),
            ..Default::default()
        };
        let read = read_local_finder_comments(
            &root,
            &["file".into()],
            Some(&root),
            &crate::Cancel::new(),
            &mut access,
        )
        .unwrap();
        assert!(read[0].state.as_ref().unwrap().metadata_differs);
        let text = access.text.clone();
        let rows = change_local_finder_comments(
            &root,
            &["file".into()],
            &text,
            Some(&root),
            &crate::Cancel::new(),
            &mut access,
        )
        .unwrap();
        assert!(rows[0].changed, "{rows:?}");
        assert!(!rows[0].state.as_ref().unwrap().metadata_differs);
        let rows = change_local_finder_comments(
            &root,
            &["file".into()],
            &text,
            Some(&root),
            &crate::Cancel::new(),
            &mut access,
        )
        .unwrap();
        assert!(!rows[0].changed);
        assert_eq!(access.writes, 1);
        let rows = change_local_finder_comments(
            &root,
            &["file".into()],
            "",
            Some(&root),
            &crate::Cancel::new(),
            &mut access,
        )
        .unwrap();
        assert!(rows[0].changed);
        assert_eq!(access.text, "");
        assert_eq!(fs::read(root.join("file")).unwrap(), b"data remains");
        assert_eq!(
            fs::read(root.join("file/..namedfork/rsrc")).unwrap(),
            b"fork remains"
        );
        let values = read_xattrs(&root.join("file")).unwrap();
        assert!(values
            .iter()
            .any(|value| value.name == b"com.doppeldecker.keep" && value.value == b"kept"));
    }
    #[test]
    fn finder_comments_stop_after_unclear_write_or_disagreeing_readback() {
        for lose_reply in [false, true] {
            let (_area, root) = fixture();
            fs::write(root.join("second"), b"second stays").unwrap();
            let mut access = Finder {
                lose_reply,
                mismatch: !lose_reply,
                ..Default::default()
            };
            let rows = change_local_finder_comments(
                &root,
                &["file".into(), "second".into()],
                "new",
                Some(&root),
                &crate::Cancel::new(),
                &mut access,
            )
            .unwrap();
            assert_eq!(
                rows[0].code,
                Some(crate::ErrorCode::OutcomeUnclear),
                "{rows:?}"
            );
            assert_eq!(rows[1].code, Some(crate::ErrorCode::Interrupted));
            assert_eq!(access.writes, 1);
            assert_eq!(fs::read(root.join("second")).unwrap(), b"second stays");
            let held = RootHandle::open_nofollow(&root).unwrap();
            let entry = stat_beneath(&held, &root.join("second")).unwrap();
            let (fd, _) =
                open_interactive_metadata_entry(&held, &root.join("second"), &entry).unwrap();
            assert!(finder_tags::read_attribute(&fd, COMMENT, 64 * 1024)
                .unwrap()
                .is_none());
        }
    }
    #[test]
    fn finder_comments_cancel_drift_links_and_denial_do_not_start_a_write() {
        let (_area, root) = fixture();
        let cancel = crate::Cancel::new();
        let mut access = Finder {
            cancel_on_read: Some(cancel.clone()),
            ..Default::default()
        };
        let rows = change_local_finder_comments(
            &root,
            &["file".into()],
            "new",
            Some(&root),
            &cancel,
            &mut access,
        )
        .unwrap();
        assert_eq!(rows[0].code, Some(crate::ErrorCode::Interrupted));
        assert_eq!(access.writes, 0);
        let mut access = Finder {
            replace_on_read: Some(root.join("file")),
            ..Default::default()
        };
        let rows = change_local_finder_comments(
            &root,
            &["file".into()],
            "new",
            Some(&root),
            &crate::Cancel::new(),
            &mut access,
        )
        .unwrap();
        assert_eq!(rows[0].code, Some(crate::ErrorCode::PlanDrift));
        assert_eq!(access.writes, 0);
        assert_eq!(fs::read(root.join("file")).unwrap(), b"substitute");
        symlink("file", root.join("link")).unwrap();
        fs::hard_link(root.join("file"), root.join("alias")).unwrap();
        let mut access = Finder::default();
        let rows = change_local_finder_comments(
            &root,
            &["link".into(), "alias".into()],
            "new",
            Some(&root),
            &crate::Cancel::new(),
            &mut access,
        )
        .unwrap();
        assert!(rows
            .iter()
            .all(|row| row.code == Some(crate::ErrorCode::Unsupported)));
        assert_eq!(access.reads, 0);
        let mut access = Finder {
            deny_write: true,
            ..Default::default()
        };
        let rows = change_local_finder_comments(
            &root,
            &["file.saved".into()],
            "new",
            Some(&root),
            &crate::Cancel::new(),
            &mut access,
        )
        .unwrap();
        assert_eq!(rows[0].code, Some(crate::ErrorCode::PermissionDenied));
        assert_eq!(access.writes, 0);
        let names = (0..41)
            .map(|index| format!("unique-{index}"))
            .collect::<Vec<_>>();
        assert_eq!(
            crate::error_code(
                &read_local_finder_comments(
                    &root,
                    &names,
                    Some(&root),
                    &crate::Cancel::new(),
                    &mut access
                )
                .unwrap_err()
            ),
            crate::ErrorCode::InvalidInput
        );
    }
    #[test]
    fn finder_comments_distinguish_native_consent_from_file_permission_errors() {
        struct Denied(bool);
        impl FinderCommentAccess for Denied {
            fn read(&mut self, _object: std::os::fd::BorrowedFd<'_>) -> io::Result<String> {
                Err(if self.0 {
                    crate::file_properties::finder_comment_permission_required()
                } else {
                    io::Error::new(io::ErrorKind::PermissionDenied, "file unreadable")
                })
            }
            fn write(
                &mut self,
                _object: std::os::fd::BorrowedFd<'_>,
                _text: &str,
            ) -> Result<(), FinderCommentWriteError> {
                panic!("Kein Schreibauftrag");
            }
        }
        let (_area, root) = fixture();
        for native_consent in [false, true] {
            let rows = read_local_finder_comments(
                &root,
                &["file".into()],
                Some(&root),
                &crate::Cancel::new(),
                &mut Denied(native_consent),
            )
            .unwrap();
            assert_eq!(rows[0].code, Some(crate::ErrorCode::PermissionDenied));
            assert_eq!(rows[0].permission_required, native_consent);
        }
    }
}
