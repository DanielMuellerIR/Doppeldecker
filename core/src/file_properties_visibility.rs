//! Native Verstecktmarkierung; Punktnamen bleiben ein eigener Sichtbarkeitsgrund.
#[cfg(target_os = "macos")]
use super::check_cancel;
use super::{invalid, validate_local_selection};
use std::{io, path::Path};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VisibilityState {
    pub marked: bool,
    pub by_name: bool,
}
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VisibilityReadRow {
    pub name: String,
    pub state: Option<VisibilityState>,
    pub code: Option<crate::ErrorCode>,
    pub message: String,
}
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VisibilityEditRow {
    pub name: String,
    pub state: Option<VisibilityState>,
    pub changed: bool,
    pub code: Option<crate::ErrorCode>,
    pub message: String,
}
fn validate(root: &Path, names: &[String], sandbox: Option<&Path>) -> io::Result<()> {
    validate_local_selection(root, names, sandbox)
}
pub fn read_local_visibility(
    root: &Path,
    names: &[String],
    sandbox: Option<&Path>,
    cancel: &crate::Cancel,
) -> io::Result<Vec<VisibilityReadRow>> {
    validate(root, names, sandbox)?;
    if names.len() > 40 {
        return Err(invalid(
            "Sichtbarkeitsansicht benötigt höchstens 40 Objekte je Seite",
        ));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cancel;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Native Verstecktmarkierungen werden nur auf macOS bearbeitet",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        let held = crate::local_fs::RootHandle::open_nofollow(root)?;
        Ok(names
            .iter()
            .map(|name| {
                let result = (|| {
                    check_cancel(cancel)?;
                    let path = root.join(name);
                    let entry = crate::local_fs::stat_beneath(&held, &path)?;
                    crate::local_fs::read_visibility_beneath(&held, &path, &entry)
                })();
                match result {
                    Ok(state) => VisibilityReadRow {
                        name: name.clone(),
                        state: Some(state),
                        code: None,
                        message: String::new(),
                    },
                    Err(error) => VisibilityReadRow {
                        name: name.clone(),
                        state: None,
                        code: Some(crate::error_code(&error)),
                        message: error.to_string(),
                    },
                }
            })
            .collect())
    }
}
pub fn change_local_visibility(
    root: &Path,
    names: &[String],
    hidden: bool,
    sandbox: Option<&Path>,
    cancel: &crate::Cancel,
) -> io::Result<Vec<VisibilityEditRow>> {
    validate(root, names, sandbox)?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (hidden, cancel);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Native Verstecktmarkierungen werden nur auf macOS bearbeitet",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        let held = crate::local_fs::RootHandle::open_nofollow(root)?;
        Ok(names
            .iter()
            .map(|name| {
                let result = (|| {
                    check_cancel(cancel)?;
                    let path = root.join(name);
                    let entry = crate::local_fs::stat_beneath(&held, &path)?;
                    crate::local_fs::edit_visibility_beneath(&held, &path, &entry, hidden, cancel)
                })();
                match result {
                    Ok((state, changed)) => VisibilityEditRow {
                        name: name.clone(),
                        state: Some(state.clone()),
                        changed,
                        code: None,
                        message: if state.by_name && !state.marked {
                            "Markierung entfernt; Punktname bleibt ausgeblendet"
                        } else if changed {
                            "Verstecktmarkierung am geöffneten Objekt bestätigt"
                        } else {
                            "Markierung bereits passend; unverändert"
                        }
                        .into(),
                    },
                    Err(error) => VisibilityEditRow {
                        name: name.clone(),
                        state: None,
                        changed: false,
                        code: Some(crate::error_code(&error)),
                        message: error.to_string(),
                    },
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn visibility_rejects_escaping_names_and_keeps_display_facts_out_of_entry() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        std::fs::write(root.join(".dot"), b"data").unwrap();
        for names in [vec!["../escape".into()], vec![".dot".into(), ".dot".into()]] {
            assert!(change_local_visibility(
                &root,
                &names,
                true,
                Some(&root),
                &crate::Cancel::new()
            )
            .is_err());
        }
        let display = crate::LocalVfs::new().list_display_entries(&root).unwrap();
        assert!(display[0].is_hidden);
        let json = serde_json::to_value(&display[0].entry).unwrap();
        assert!(json.get("is_hidden").is_none());
        let json = serde_json::to_value(&display[0]).unwrap();
        assert_eq!(json["is_hidden"], true);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            change_local_visibility(
                &root,
                &[".dot".into()],
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
