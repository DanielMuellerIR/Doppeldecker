//! Einzelne native Dateieigenschaften; keine freie Flagmaske aus dem Frontend.
use super::{invalid, validate_local_selection};
use std::{io, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileFlagKind {
    Locked,
    HiddenExtension,
}
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFlagRow {
    pub name: String,
    pub state: Option<bool>,
    pub changed: bool,
    pub code: Option<crate::ErrorCode>,
    pub message: String,
}
fn validate(root: &Path, names: &[String], sandbox: Option<&Path>) -> io::Result<()> {
    validate_local_selection(root, names, sandbox)
}
pub fn read_local_file_flags(
    root: &Path,
    names: &[String],
    kind: FileFlagKind,
    sandbox: Option<&Path>,
    cancel: &crate::Cancel,
) -> io::Result<Vec<FileFlagRow>> {
    validate(root, names, sandbox)?;
    if names.len() > 40 {
        return Err(invalid(
            "Dateieigenschaften benötigen höchstens 40 Objekte je Seite",
        ));
    }
    visit(root, names, kind, None, cancel)
}
pub fn change_local_file_flag(
    root: &Path,
    names: &[String],
    kind: FileFlagKind,
    enabled: bool,
    sandbox: Option<&Path>,
    cancel: &crate::Cancel,
) -> io::Result<Vec<FileFlagRow>> {
    validate(root, names, sandbox)?;
    visit(root, names, kind, Some(enabled), cancel)
}
fn visit(
    root: &Path,
    names: &[String],
    kind: FileFlagKind,
    enabled: Option<bool>,
    cancel: &crate::Cancel,
) -> io::Result<Vec<FileFlagRow>> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (root, names, kind, enabled, cancel);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Diese Dateieigenschaften benötigen macOS",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        let held = crate::local_fs::RootHandle::open_nofollow(root)?;
        let mut stopped = false;
        Ok(names.iter().map(|name| {
            let result = (|| {
                super::check_cancel(cancel)?;
                if stopped {
                    return Err(crate::coded_error(io::ErrorKind::Interrupted, crate::ErrorCode::Interrupted,
                        "Nach unklarer Teilwirkung nicht mehr geändert; vorheriges Ergebnis prüfen"));
                }
                let path = root.join(name);
                let entry = crate::local_fs::stat_beneath(&held, &path)?;
                crate::local_fs::file_flags::access(&held, &path, &entry, kind, enabled, cancel)
            })();
            match result {
                Ok((state, changed)) => FileFlagRow {name:name.clone(),state:Some(state),changed,code:None,
                    message:if changed { "Dateieigenschaft am geöffneten Objekt bestätigt" } else { "Dateieigenschaft bereits passend; unverändert" }.into()},
                Err(error) => {
                    let code=crate::error_code(&error);
                    stopped |= code == crate::ErrorCode::OutcomeUnclear;
                    FileFlagRow {name:name.clone(),state:None,changed:false,code:Some(code),message:error.to_string()}
                }
            }
        }).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_flags_validate_authority_and_platform_before_access() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        for names in [vec!["../escape".into()], vec!["file".into(), "file".into()]] {
            assert!(change_local_file_flag(
                &root,
                &names,
                FileFlagKind::Locked,
                true,
                Some(&root),
                &crate::Cancel::new()
            )
            .is_err());
        }
        assert!(read_local_file_flags(
            &root,
            &vec!["file".into(); 41],
            FileFlagKind::Locked,
            Some(&root),
            &crate::Cancel::new()
        )
        .is_err());
        assert!(serde_json::from_str::<FileFlagKind>("\"systemImmutable\"").is_err());
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            change_local_file_flag(
                &root,
                &["file".into()],
                FileFlagKind::Locked,
                true,
                Some(&root),
                &crate::Cancel::new()
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Unsupported
        );
    }
}
