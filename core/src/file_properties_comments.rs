//! Finder-Kommentare: Core bindet das Objekt; die App vermittelt zum nativen Finder.
use super::{invalid, validate_local_selection};
use std::{io, os::fd::BorrowedFd, path::Path};
use unicode_normalization::UnicodeNormalization;

pub const MAX_FINDER_COMMENT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinderCommentState {
    pub text: String,
    pub metadata_differs: bool,
}
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinderCommentReadRow {
    pub name: String,
    pub state: Option<FinderCommentState>,
    pub code: Option<crate::ErrorCode>,
    pub permission_required: bool,
    pub message: String,
}
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinderCommentEditRow {
    pub name: String,
    pub state: Option<FinderCommentState>,
    pub changed: bool,
    pub code: Option<crate::ErrorCode>,
    pub message: String,
}

#[derive(Debug)]
struct FinderPermissionRequired;
impl std::fmt::Display for FinderPermissionRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Zum Lesen und Speichern von Kommentaren benötigt Doppeldecker die macOS-Freigabe für Finder.")
    }
}
impl std::error::Error for FinderPermissionRequired {}

/// Nur ein entsprechender nativer Vorabstatus darf die Freigabeschaltfläche
/// anbieten. Fehlende Leserechte einer Datei sind kein Automationsauftrag.
pub fn finder_comment_permission_required() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, FinderPermissionRequired)
}

/// Ein nativer Fehler vor dem Senden ist von einem möglicherweise ausgeführten
/// Auftrag getrennt. Nur der erste Fall darf als unverändert gelten.
#[derive(Debug)]
pub enum FinderCommentWriteError {
    NotStarted(io::Error),
    OutcomeUnclear(io::Error),
}

/// Erhält ausschließlich einen geliehenen, vom Core geöffneten Objektdeskriptor.
/// Die macOS-App bildet daraus eine native Dateireferenz; keine Pfadauflösung als
/// Ersatz und kein serialisierbares Handle. Tests benötigen keinen Finder.
pub trait FinderCommentAccess {
    fn read(&mut self, object: BorrowedFd<'_>) -> io::Result<String>;
    fn write(&mut self, object: BorrowedFd<'_>, text: &str) -> Result<(), FinderCommentWriteError>;
}

pub fn validate_finder_comment(text: &str) -> io::Result<()> {
    if text.len() > MAX_FINDER_COMMENT_BYTES
        || text.nfd().map(char::len_utf8).sum::<usize>() > MAX_FINDER_COMMENT_BYTES
        || text
            .chars()
            .any(|value| value.is_control() && !matches!(value, '\n' | '\r' | '\t'))
    {
        return Err(invalid("Kommentare benötigen höchstens 16 KiB UTF-8-Text einschließlich Finder-Normalisierung, ohne Steuerzeichen außer Tab und Zeilenumbruch"));
    }
    Ok(())
}
fn validate(root: &Path, names: &[String], sandbox: Option<&Path>) -> io::Result<()> {
    validate_local_selection(root, names, sandbox)
}

pub fn read_local_finder_comments(
    root: &Path,
    names: &[String],
    sandbox: Option<&Path>,
    cancel: &crate::Cancel,
    access: &mut dyn FinderCommentAccess,
) -> io::Result<Vec<FinderCommentReadRow>> {
    validate(root, names, sandbox)?;
    if names.len() > 40 {
        return Err(invalid(
            "Kommentaransicht benötigt höchstens 40 Objekte je Seite",
        ));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (cancel, access);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Finder-Kommentare benötigen macOS",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        let held = crate::local_fs::RootHandle::open_nofollow(root)?;
        Ok(names
            .iter()
            .map(|name| {
                let result = (|| {
                    super::check_cancel(cancel)?;
                    let path = root.join(name);
                    let expected = crate::local_fs::stat_beneath(&held, &path)?;
                    crate::local_fs::read_comment_beneath(&held, &path, &expected, access)
                })();
                match result {
                    Ok(state) => FinderCommentReadRow {
                        name: name.clone(),
                        state: Some(state),
                        code: None,
                        permission_required: false,
                        message: String::new(),
                    },
                    Err(error) => FinderCommentReadRow {
                        name: name.clone(),
                        state: None,
                        code: Some(crate::error_code(&error)),
                        permission_required: error
                            .get_ref()
                            .is_some_and(|error| error.is::<FinderPermissionRequired>()),
                        message: error.to_string(),
                    },
                }
            })
            .collect())
    }
}

pub fn change_local_finder_comments(
    root: &Path,
    names: &[String],
    text: &str,
    sandbox: Option<&Path>,
    cancel: &crate::Cancel,
    access: &mut dyn FinderCommentAccess,
) -> io::Result<Vec<FinderCommentEditRow>> {
    validate(root, names, sandbox)?;
    validate_finder_comment(text)?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (cancel, access);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Finder-Kommentare benötigen macOS",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        let held = crate::local_fs::RootHandle::open_nofollow(root)?;
        let mut stop = false;
        Ok(names
            .iter()
            .map(|name| {
                let result = (|| {
                    super::check_cancel(cancel)?;
                    if stop {
                        return Err(crate::coded_error(
                            io::ErrorKind::Interrupted,
                            crate::ErrorCode::Interrupted,
                            "Nicht begonnen; vorheriger Kommentarabschluss unklar",
                        ));
                    }
                    let path = root.join(name);
                    let expected = crate::local_fs::stat_beneath(&held, &path)?;
                    crate::local_fs::edit_comment_beneath(
                        &held, &path, &expected, text, cancel, access,
                    )
                })();
                match result {
                    Ok((state, changed)) => FinderCommentEditRow {
                        name: name.clone(),
                        state: Some(state),
                        changed,
                        code: None,
                        message: if changed {
                            "Finder-Kommentar und Metadaten bestätigt"
                        } else {
                            "Kommentar bereits passend; unverändert"
                        }
                        .into(),
                    },
                    Err(error) => {
                        let code = crate::error_code(&error);
                        stop |= code == crate::ErrorCode::OutcomeUnclear;
                        FinderCommentEditRow {
                            name: name.clone(),
                            state: None,
                            changed: false,
                            code: Some(code),
                            message: error.to_string(),
                        }
                    }
                }
            })
            .collect())
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn decode_comment(raw: Option<&[u8]>) -> io::Result<String> {
    if raw.is_some_and(|raw| raw.len() > 64 * 1024) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Kommentar-Attribut überschreitet 64 KiB",
        ));
    }
    let text: String = match raw {
        None => String::new(),
        Some(raw) => plist::from_reader(std::io::Cursor::new(raw)).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "Ungültiges Kommentar-Attribut")
        })?,
    };
    validate_finder_comment(&text).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Kommentar-Attribut überschreitet seine Textgrenzen",
        )
    })?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct NoNativeAccess;
    impl FinderCommentAccess for NoNativeAccess {
        fn read(&mut self, _object: BorrowedFd<'_>) -> io::Result<String> {
            panic!("Keine native Anfrage bei ungültigen Optionen");
        }
        fn write(
            &mut self,
            _object: BorrowedFd<'_>,
            _text: &str,
        ) -> Result<(), FinderCommentWriteError> {
            panic!("Keine native Änderung bei ungültigen Optionen");
        }
    }
    #[test]
    fn invalid_comments_and_selections_never_reach_the_native_adapter() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        std::fs::write(root.join("file"), b"data").unwrap();
        let mut access = NoNativeAccess;
        for text in [
            "nul\0text".into(),
            "x".repeat(MAX_FINDER_COMMENT_BYTES + 1),
            "ä".repeat(MAX_FINDER_COMMENT_BYTES / 2),
        ] {
            assert!(change_local_finder_comments(
                &root,
                &["file".into()],
                &text,
                Some(&root),
                &crate::Cancel::new(),
                &mut access
            )
            .is_err());
        }
        assert!(change_local_finder_comments(
            &root,
            &["../escape".into()],
            "text",
            Some(&root),
            &crate::Cancel::new(),
            &mut access
        )
        .is_err());
        let names = (0..41)
            .map(|index| format!("object-{index}"))
            .collect::<Vec<_>>();
        assert!(read_local_finder_comments(
            &root,
            &names,
            Some(&root),
            &crate::Cancel::new(),
            &mut access
        )
        .is_err());
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            change_local_finder_comments(
                &root,
                &["file".into()],
                "text",
                Some(&root),
                &crate::Cancel::new(),
                &mut access
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Unsupported
        );
    }
}
