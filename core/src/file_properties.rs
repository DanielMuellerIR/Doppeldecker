//! Gewöhnliche lokale Metadatenänderungen mit einer Bilanz je ausgewähltem Objekt.
use std::{collections::HashSet, io, path::Path};

use crate::{local_fs, Cancel, ErrorCode};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionEditRow {
    pub name: String,
    pub mode: Option<u32>,
    pub changed: bool,
    pub code: Option<ErrorCode>,
    pub message: String,
}

/// Ändert die gewählten direkten Kinder. Harte Links teilen die Wirkung.
pub fn change_local_permissions(
    root: &Path,
    names: &[String],
    mode: u32,
    sandbox: Option<&Path>,
    cancel: &Cancel,
) -> io::Result<Vec<PermissionEditRow>> {
    change_local_permissions_recursive(root, names, mode, false, sandbox, cancel)
}

/// Erfasst rekursive Inhalte vor dem ersten chmod. Kinder werden vor ihren
/// Eltern geändert, damit entzogene Suchrechte keine noch offene Wirkung verdecken.
pub fn change_local_permissions_recursive(
    root: &Path,
    names: &[String],
    mode: u32,
    recursive: bool,
    sandbox: Option<&Path>,
    cancel: &Cancel,
) -> io::Result<Vec<PermissionEditRow>> {
    if mode & !0o777 != 0 || names.is_empty() {
        return Err(invalid(
            "Berechtigungen benötigen eine Auswahl und drei Oktalziffern",
        ));
    }
    validate_local_selection(root, names, sandbox)?;
    if cancel.is_cancelled() {
        return Ok(names
            .iter()
            .map(|name| rejected(name.clone(), check_cancel(cancel).unwrap_err()))
            .collect());
    }
    let held = local_fs::RootHandle::open_nofollow(root)?;
    change_permissions_bound(&held, names, mode, recursive, None, cancel)
}

fn change_permissions_bound(
    held: &local_fs::RootHandle,
    names: &[String],
    mode: u32,
    recursive: bool,
    prepared: Option<Vec<PermissionTarget>>,
    cancel: &Cancel,
) -> io::Result<Vec<PermissionEditRow>> {
    let root = held.path();
    let targets = match prepared {
        Some(targets) => targets,
        None => collect_targets(held, root, names, recursive, cancel)?,
    };
    let mut rows = Vec::with_capacity(targets.len());
    let mut changed_objects = HashSet::new();
    for target in targets {
        let (name, path, mut expected) = match target {
            PermissionTarget::Object {
                name,
                path,
                expected,
            } => (name, path, expected),
            PermissionTarget::Rejected(row) => {
                rows.push(row);
                continue;
            }
        };
        if changed_objects.contains(&(expected.device, expected.inode)) {
            // Ein vorheriger ausgewählter Hardlink wurde von diesem Lauf geändert.
            // Alle übrigen Identitäts-/Inhaltsbelege bleiben unverändert erforderlich.
            expected.mode = Some(mode);
        }
        let result = (|| {
            check_cancel(cancel)?;
            if !held.is_current()? {
                return Err(crate::real_mode::plan_drift(
                    "Der ausgewählte Ordner wurde ausgetauscht",
                ));
            }
            local_fs::set_interactive_mode_beneath(held, &path, &expected, mode)
        })();
        rows.push(match result {
            Ok(entry) => {
                changed_objects.insert((entry.device, entry.inode));
                PermissionEditRow {
                    name,
                    mode: entry.mode,
                    changed: true,
                    code: None,
                    message: "Berechtigungen am geöffneten Objekt gesetzt und zurückgelesen".into(),
                }
            }
            Err(error) => rejected(name, error),
        });
    }
    Ok(rows)
}

fn validate_local_selection(
    root: &Path,
    names: &[String],
    sandbox: Option<&Path>,
) -> io::Result<()> {
    if names.is_empty() || names.len() > MAX_PERMISSION_ENTRIES {
        return Err(invalid(
            "Metadatenänderung benötigt eine Auswahl mit höchstens 100.000 Objekten",
        ));
    }
    if crate::validate_file_operation_root(root)? != root
        || sandbox.is_some_and(|sandbox| !root.starts_with(sandbox))
    {
        return Err(invalid(
            "Ungültiger lokaler Ordner oder Ziel außerhalb der Testsandbox",
        ));
    }
    if names.iter().map(String::len).sum::<usize>() > MAX_PERMISSION_PATH_BYTES {
        return Err(invalid("Metadatenauswahl überschreitet 32 MiB Pfadnamen"));
    }
    let mut unique = HashSet::new();
    for name in names {
        // Namen verlustlos prüfen: äußere Leerzeichen sind gültige Dateinamen.
        if name.is_empty()
            || name.contains(['/', '\0'])
            || matches!(name.as_str(), "." | "..")
            || !unique.insert(name)
        {
            return Err(invalid("Die Auswahl enthält ungültige oder doppelte Namen"));
        }
    }
    Ok(())
}

#[path = "file_properties_admin.rs"]
pub mod admin;

#[path = "file_properties_owner.rs"]
mod owner;
pub use owner::{change_local_ownership, resolve_local_owner_edit, LocalOwnerEdit, OwnerEditRow};

#[path = "file_properties_remote.rs"]
mod remote;
pub(crate) use remote::same_observation;
pub use remote::{
    change_provider_permissions, property_capabilities, validate_permission_selection,
    FilePropertyCapabilities,
};

#[path = "file_properties_tags.rs"]
pub(crate) mod tags;
pub use tags::{
    change_local_finder_tags, read_local_finder_tags, FinderTag, FinderTagEdit, FinderTagEditRow,
    FinderTagReadRow, FinderTagState,
};

#[path = "file_properties_visibility.rs"]
mod visibility;
pub use visibility::{
    change_local_visibility, read_local_visibility, VisibilityEditRow, VisibilityReadRow,
    VisibilityState,
};

#[path = "file_properties_flags.rs"]
mod flags;
pub use flags::{change_local_file_flag, read_local_file_flags, FileFlagKind, FileFlagRow};

#[path = "file_properties_comments.rs"]
mod comments;
pub use comments::{
    change_local_finder_comments, finder_comment_permission_required, read_local_finder_comments,
    validate_finder_comment, FinderCommentAccess, FinderCommentEditRow, FinderCommentReadRow,
    FinderCommentState, FinderCommentWriteError, MAX_FINDER_COMMENT_BYTES,
};

#[cfg(target_os = "macos")]
pub(crate) use comments::decode_comment;

const MAX_PERMISSION_ENTRIES: usize = 100_000;
const MAX_PERMISSION_PATH_BYTES: usize = 32 * 1024 * 1024;
enum PermissionTarget {
    Object {
        name: String,
        path: std::path::PathBuf,
        expected: crate::Entry,
    },
    Rejected(PermissionEditRow),
}
fn check_cancel(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        return Err(crate::coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::Interrupted,
            "Vor dieser Metadatenänderung abgebrochen; unverändert",
        ));
    }
    Ok(())
}
fn rejected(name: String, error: io::Error) -> PermissionEditRow {
    PermissionEditRow {
        name,
        mode: None,
        changed: false,
        code: Some(crate::error_code(&error)),
        message: error.to_string(),
    }
}
fn collect_targets(
    held: &local_fs::RootHandle,
    root: &Path,
    names: &[String],
    recursive: bool,
    cancel: &Cancel,
) -> io::Result<Vec<PermissionTarget>> {
    let mut pending = names
        .iter()
        .rev()
        .map(|name| (root.join(name), None))
        .collect::<Vec<_>>();
    let mut targets = Vec::new();
    let mut count = names.len();
    let mut bytes = names.iter().map(String::len).sum::<usize>();
    while let Some((path, visited)) = pending.pop() {
        check_cancel(cancel)?;
        if count > MAX_PERMISSION_ENTRIES || bytes > MAX_PERMISSION_PATH_BYTES {
            return Err(invalid("Auswahl überschreitet 100.000 Einträge oder 32 MiB Pfadnamen; kleinere Auswahl verwenden. Noch keine Rechte geändert"));
        }
        if !held.is_current()? {
            return Err(crate::real_mode::plan_drift(
                "Auswahlordner während der Erfassung ausgetauscht",
            ));
        }
        let name = path
            .strip_prefix(root)
            .map_err(|_| invalid("Pfad außerhalb des Auswahlordners"))?
            .to_str()
            .ok_or_else(|| invalid("Nicht eindeutig dekodierbarer Pfad"))?
            .to_owned();
        if let Some(expected) = visited {
            targets.push(PermissionTarget::Object {
                name,
                path,
                expected,
            });
            continue;
        }
        let expected = match local_fs::stat_beneath(held, &path) {
            Ok(entry) => entry,
            Err(error) => {
                targets.push(PermissionTarget::Rejected(rejected(name, error)));
                continue;
            }
        };
        if recursive && expected.is_dir && !expected.is_symlink {
            let children = match local_fs::list_complete_beneath(held, &path) {
                Ok(children) => children,
                Err(error) => {
                    targets.push(PermissionTarget::Rejected(rejected(name, error)));
                    continue;
                }
            };
            count = count.saturating_add(children.len());
            pending.push((path.clone(), Some(expected)));
            for child in children.into_iter().rev() {
                let label = format!("{name}/{}", child.entry.name);
                bytes = bytes.saturating_add(label.len());
                if child.safely_decoded {
                    pending.push((path.join(&child.entry.name), None));
                } else {
                    targets.push(PermissionTarget::Rejected(rejected(
                        label,
                        invalid("Nicht eindeutig dekodierbarer Name; unverändert"),
                    )));
                }
            }
        } else {
            targets.push(PermissionTarget::Object {
                name,
                path,
                expected,
            });
        }
    }
    Ok(targets)
}

fn invalid(message: &str) -> io::Error {
    crate::coded_error(
        io::ErrorKind::InvalidInput,
        ErrorCode::InvalidInput,
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::{
            ffi::OsStrExt,
            fs::{symlink, PermissionsExt},
        },
    };

    #[test]
    fn oversized_selection_is_rejected_before_local_metadata_access() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let names = vec!["x".repeat(MAX_PERMISSION_PATH_BYTES + 1)];
        let error = validate_local_selection(&root, &names, None).unwrap_err();
        assert_eq!(crate::error_code(&error), ErrorCode::InvalidInput);
        let error = change_local_ownership(
            &root,
            &names,
            LocalOwnerEdit {
                uid: Some(0),
                gid: None,
            },
            None,
            &Cancel::new(),
        );
        assert!(error.is_err());
    }

    #[test]
    fn explicit_selection_preserves_content_special_bits_and_other_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        fs::write(root.join(" a "), b"Inhalt").unwrap();
        fs::write(root.join("other"), b"Unveraendert").unwrap();
        fs::set_permissions(root.join(" a "), fs::Permissions::from_mode(0o1644)).unwrap();
        fs::set_permissions(root.join("other"), fs::Permissions::from_mode(0o640)).unwrap();
        let report =
            change_local_permissions(&root, &[" a ".into()], 0o600, Some(&root), &Cancel::new())
                .unwrap();
        assert!(report[0].changed, "{report:?}");
        assert_eq!(
            fs::metadata(root.join(" a ")).unwrap().permissions().mode() & 0o7777,
            0o1600
        );
        assert_eq!(
            fs::metadata(root.join("other"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(fs::read(root.join(" a ")).unwrap(), b"Inhalt");
    }

    #[test]
    fn symlink_and_fifo_do_not_touch_external_target_or_block_independent_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        fs::create_dir(root.join("pane")).unwrap();
        fs::write(root.join("outside"), b"Aussen").unwrap();
        fs::set_permissions(root.join("outside"), fs::Permissions::from_mode(0o640)).unwrap();
        symlink(root.join("outside"), root.join("pane/link")).unwrap();
        let fifo = std::ffi::CString::new(root.join("pane/fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        fs::write(root.join("pane/file"), b"Datei").unwrap();
        let report = change_local_permissions(
            &root.join("pane"),
            &["link".into(), "fifo".into(), "file".into()],
            0o600,
            Some(&root),
            &Cancel::new(),
        )
        .unwrap();
        assert!(
            !report[0].changed && !report[1].changed && report[2].changed,
            "{report:?}"
        );
        assert_eq!(
            fs::metadata(root.join("outside"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn owner_can_restore_permissions_without_content_read_access() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        fs::write(root.join("file"), b"Inhalt").unwrap();
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o0)).unwrap();
        let report =
            change_local_permissions(&root, &["file".into()], 0o600, None, &Cancel::new()).unwrap();
        assert!(report[0].changed, "{report:?}");
        assert_eq!(fs::read(root.join("file")).unwrap(), b"Inhalt");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn unreadable_macos_object_reports_denial_without_path_based_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        fs::write(root.join("file"), b"Inhalt").unwrap();
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o0)).unwrap();
        let report =
            change_local_permissions(&root, &["file".into()], 0o600, None, &Cancel::new()).unwrap();
        assert!(!report[0].changed);
        assert_eq!(report[0].code, Some(ErrorCode::PermissionDenied));
        assert_eq!(
            fs::metadata(root.join("file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0
        );
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read(root.join("file")).unwrap(), b"Inhalt");
    }

    #[test]
    fn exchanged_source_is_rejected_before_chmod_and_hardlinks_share_the_effect() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        fs::write(root.join("file"), b"Original").unwrap();
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o644)).unwrap();
        let held = local_fs::RootHandle::open_nofollow(&root).unwrap();
        let expected = local_fs::stat_beneath(&held, &root.join("file")).unwrap();
        fs::rename(root.join("file"), root.join("saved")).unwrap();
        fs::write(root.join("file"), b"Fremd").unwrap();
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o640)).unwrap();
        let error =
            local_fs::set_interactive_mode_beneath(&held, &root.join("file"), &expected, 0o600)
                .unwrap_err();
        assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(
            fs::metadata(root.join("file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        fs::hard_link(root.join("saved"), root.join("alias")).unwrap();
        let report =
            change_local_permissions(&root, &["saved".into()], 0o600, None, &Cancel::new())
                .unwrap();
        assert!(report[0].changed);
        assert_eq!(
            fs::metadata(root.join("alias"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn invalid_selection_is_rejected_before_any_effect() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        fs::write(root.join("file"), b"Inhalt").unwrap();
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o640)).unwrap();
        for names in [
            vec!["file".into(), "../outside".into()],
            vec!["file".into(), "file".into()],
        ] {
            assert!(change_local_permissions(&root, &names, 0o600, None, &Cancel::new()).is_err());
        }
        assert!(
            change_local_permissions(&root, &["file".into()], 0o7777, None, &Cancel::new())
                .is_err()
        );
        assert!(change_local_permissions(
            &root,
            &["file".into()],
            0o600,
            Some(&root.join("different")),
            &Cancel::new()
        )
        .is_err());
        let cancel = Cancel::new();
        cancel.cancel();
        let report =
            change_local_permissions(&root, &["file".into()], 0o600, None, &cancel).unwrap();
        assert_eq!(report[0].code, Some(ErrorCode::Interrupted));
        assert_eq!(
            fs::metadata(root.join("file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }
    #[test]
    fn recursive_permissions_change_children_before_restrictive_parents_and_preserve_hardlinks() {
        let area = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(area.path()).unwrap();
        fs::create_dir_all(root.join("tree/nested/empty")).unwrap();
        fs::write(root.join("tree/nested/file"), b"bytes").unwrap();
        fs::hard_link(root.join("tree/nested/file"), root.join("tree/alias")).unwrap();
        fs::write(root.join("other"), b"unselected").unwrap();
        fs::set_permissions(root.join("other"), fs::Permissions::from_mode(0o644)).unwrap();
        let report = change_local_permissions_recursive(
            &root,
            &["tree".into()],
            0o600,
            true,
            Some(&root),
            &Cancel::new(),
        )
        .unwrap();
        // Wiederherstellung ausschließlich für die eigene künstliche Fixture.
        for name in ["tree", "tree/nested", "tree/nested/empty"] {
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(report.iter().all(|row| row.changed), "{report:?}");
        assert_eq!(report.len(), 5);
        assert_eq!(report.last().unwrap().name, "tree");
        let file = report
            .iter()
            .position(|row| row.name == "tree/nested/file")
            .unwrap();
        let parent = report
            .iter()
            .position(|row| row.name == "tree/nested")
            .unwrap();
        assert!(file < parent);
        assert_eq!(
            fs::metadata(root.join("tree/alias"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(root.join("other"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(fs::read(root.join("tree/nested/file")).unwrap(), b"bytes");
    }

    #[test]
    fn recursive_permissions_skip_symlink_subtrees_and_keep_default_nonrecursive() {
        let area = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(area.path()).unwrap();
        fs::create_dir_all(root.join("tree/inside")).unwrap();
        fs::create_dir(root.join("outside")).unwrap();
        fs::write(root.join("outside/file"), b"outside").unwrap();
        fs::write(root.join("tree/inside/file"), b"inside").unwrap();
        for name in ["outside/file", "tree/inside/file"] {
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o644)).unwrap();
        }
        symlink(root.join("outside"), root.join("tree/link")).unwrap();
        change_local_permissions(&root, &["tree".into()], 0o750, None, &Cancel::new()).unwrap();
        assert_eq!(
            fs::metadata(root.join("tree/inside/file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        let report = change_local_permissions_recursive(
            &root,
            &["tree".into()],
            0o700,
            true,
            None,
            &Cancel::new(),
        )
        .unwrap();
        assert!(report.iter().any(|row| row.name == "tree/link"
            && !row.changed
            && row.code == Some(ErrorCode::Unsupported)));
        assert!(
            report
                .iter()
                .filter(|row| row.name != "tree/link")
                .all(|row| row.changed),
            "{report:?}"
        );
        assert_eq!(
            fs::metadata(root.join("outside/file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(root.join("tree/inside/file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn recursive_capture_binds_identity_and_budget_before_mutation() {
        let area = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(area.path()).unwrap();
        fs::create_dir(root.join("tree")).unwrap();
        fs::write(root.join("tree/file"), b"original").unwrap();
        let held = local_fs::RootHandle::open_nofollow(&root).unwrap();
        let targets =
            collect_targets(&held, &root, &["tree".into()], true, &Cancel::new()).unwrap();
        fs::rename(root.join("tree/file"), root.join("saved")).unwrap();
        fs::write(root.join("tree/file"), b"substitute").unwrap();
        let PermissionTarget::Object { path, expected, .. } = &targets[0] else {
            panic!("Dateibeleg fehlt")
        };
        assert!(local_fs::set_interactive_mode_beneath(&held, path, expected, 0o600).is_err());
        let too_large = vec!["tree".into(); MAX_PERMISSION_ENTRIES + 1];
        assert!(collect_targets(&held, &root, &too_large, true, &Cancel::new()).is_err());
        assert_eq!(fs::read(root.join("tree/file")).unwrap(), b"substitute");
    }
}
