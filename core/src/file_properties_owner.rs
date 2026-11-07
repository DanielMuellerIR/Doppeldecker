//! Lokale Eigentümer-/Gruppenänderungen ohne Übertragung fremder Identitätsdomänen.
use super::{check_cancel, invalid, validate_local_selection};
use crate::{local_fs, Cancel, ErrorCode};
use std::{ffi::CString, io, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalOwnerEdit {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerEditRow {
    pub name: String,
    pub owner: Option<String>,
    pub group: Option<String>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub special_bits_changed: Option<bool>,
    pub changed: bool,
    pub code: Option<ErrorCode>,
    pub message: String,
}

pub fn resolve_local_owner_edit(owner: &str, group: &str) -> io::Result<LocalOwnerEdit> {
    let edit = LocalOwnerEdit {
        uid: resolve_id(owner, false)?,
        gid: resolve_id(group, true)?,
    };
    validate_edit(edit)?;
    Ok(edit)
}
pub(super) fn validate_edit(edit: LocalOwnerEdit) -> io::Result<()> {
    if edit.uid == Some(u32::MAX)
        || edit.gid == Some(u32::MAX)
        || edit.uid.is_none() && edit.gid.is_none()
    {
        return Err(invalid(
            "Mindestens einen lokalen Namen oder eine ID angeben; -1 ist kein Zielwert",
        ));
    }
    Ok(())
}
fn resolve_id(input: &str, group: bool) -> io::Result<Option<u32>> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    if input.len() > 1024 || input.contains('\0') || input == "-1" {
        return Err(invalid("Ungültiger lokaler Kontoname oder ID"));
    }
    if input.bytes().all(|byte| byte.is_ascii_digit()) {
        let id = input
            .parse::<u32>()
            .map_err(|_| invalid("Lokale ID ist zu groß"))?;
        if id == u32::MAX {
            return Err(invalid("-1 ist kein Zielwert"));
        }
        return Ok(Some(id));
    }
    let name = CString::new(input).map_err(|_| invalid("Ungültiger Kontoname"))?;
    let mut size = local_fs::principal_buffer_size(if group {
        libc::_SC_GETGR_R_SIZE_MAX
    } else {
        libc::_SC_GETPW_R_SIZE_MAX
    });
    loop {
        let mut buffer = vec![0_i8; size];
        let (status, id) = if group {
            let mut record = std::mem::MaybeUninit::<libc::group>::zeroed();
            let mut result = std::ptr::null_mut();
            let status = unsafe {
                libc::getgrnam_r(
                    name.as_ptr(),
                    record.as_mut_ptr(),
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut result,
                )
            };
            let id = if status == 0 && !result.is_null() {
                Some(unsafe { record.assume_init() }.gr_gid)
            } else {
                None
            };
            (status, id)
        } else {
            let mut record = std::mem::MaybeUninit::<libc::passwd>::zeroed();
            let mut result = std::ptr::null_mut();
            let status = unsafe {
                libc::getpwnam_r(
                    name.as_ptr(),
                    record.as_mut_ptr(),
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut result,
                )
            };
            let id = if status == 0 && !result.is_null() {
                Some(unsafe { record.assume_init() }.pw_uid)
            } else {
                None
            };
            (status, id)
        };
        if status == libc::ERANGE && size < 1024 * 1024 {
            size = (size * 2).min(1024 * 1024);
            continue;
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        return id.map(Some).ok_or_else(|| {
            invalid(if group {
                "Lokale Gruppe nicht gefunden"
            } else {
                "Lokaler Benutzer nicht gefunden"
            })
        });
    }
}

pub fn change_local_ownership(
    root: &Path,
    names: &[String],
    edit: LocalOwnerEdit,
    sandbox: Option<&Path>,
    cancel: &Cancel,
) -> io::Result<Vec<OwnerEditRow>> {
    validate_edit(edit)?;
    validate_local_selection(root, names, sandbox)?;
    let held = local_fs::RootHandle::open_nofollow(root)?;
    change_ownership_bound(&held, names, edit, None, cancel)
}

pub(super) fn change_ownership_bound(
    held: &local_fs::RootHandle,
    names: &[String],
    edit: LocalOwnerEdit,
    prepared: Option<&[crate::Entry]>,
    cancel: &Cancel,
) -> io::Result<Vec<OwnerEditRow>> {
    let root = held.path();
    let mut rows = Vec::with_capacity(names.len());
    for (index, name) in names.iter().enumerate() {
        let result = (|| {
            check_cancel(cancel)?;
            let path = root.join(name);
            let expected = match prepared {
                Some(entries) => entries
                    .get(index)
                    .cloned()
                    .ok_or_else(|| invalid("Unvollständige gebundene Eigentümerauswahl"))?,
                None => local_fs::stat_beneath(held, &path)?,
            };
            local_fs::set_interactive_owner_beneath(held, &path, &expected, edit.uid, edit.gid)
        })();
        rows.push(match result {
            Ok((entry, owner, special_bits_changed)) => OwnerEditRow {
                name: name.clone(),
                owner: entry.owner,
                group: entry.group,
                uid: Some(owner.uid),
                gid: Some(owner.gid),
                special_bits_changed: Some(special_bits_changed),
                changed: true,
                code: None,
                message: if special_bits_changed {
                    "Eigentümer/Gruppe bestätigt; das System hat Sonderbits geändert"
                } else {
                    "Eigentümer/Gruppe am geöffneten Objekt bestätigt"
                }
                .into(),
            },
            Err(error) => OwnerEditRow {
                name: name.clone(),
                owner: None,
                group: None,
                uid: None,
                gid: None,
                special_bits_changed: None,
                changed: false,
                code: Some(crate::error_code(&error)),
                message: error.to_string(),
            },
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{symlink, MetadataExt, PermissionsExt},
    };
    fn own() -> LocalOwnerEdit {
        LocalOwnerEdit {
            uid: Some(unsafe { libc::geteuid() }),
            gid: Some(unsafe { libc::getegid() }),
        }
    }
    #[test]
    fn local_names_and_numeric_ids_resolve_without_cross_domain_translation() {
        assert_eq!(resolve_local_owner_edit("root", "").unwrap().uid, Some(0));
        assert_eq!(
            resolve_local_owner_edit(" 123 ", "456").unwrap(),
            LocalOwnerEdit {
                uid: Some(123),
                gid: Some(456)
            }
        );
        for (owner, group) in [
            ("", ""),
            ("-1", ""),
            ("4294967295", ""),
            ("4294967296", ""),
            ("name\0tail", ""),
            ("__doppeldecker_no_such_local_account__", ""),
        ] {
            assert!(
                resolve_local_owner_edit(owner, group).is_err(),
                "{owner:?} {group:?}"
            );
        }
    }
    #[test]
    fn unchanged_owner_preserves_special_bits_and_hardlink_contents() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        fs::write(root.join("file"), b"content").unwrap();
        fs::hard_link(root.join("file"), root.join("alias")).unwrap();
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o4640)).unwrap();
        let before = fs::metadata(root.join("file")).unwrap();
        let rows = change_local_ownership(
            &root,
            &["file".into(), "alias".into()],
            own(),
            Some(&root),
            &Cancel::new(),
        )
        .unwrap();
        assert!(
            rows.iter()
                .all(|row| row.changed && row.special_bits_changed == Some(false)),
            "{rows:?}"
        );
        let after = fs::metadata(root.join("file")).unwrap();
        assert_eq!(
            (after.uid(), after.gid(), after.mode()),
            (before.uid(), before.gid(), before.mode())
        );
        assert_eq!(fs::read(root.join("alias")).unwrap(), b"content");
    }
    #[test]
    fn denied_ownership_symlink_and_cancel_leave_independent_content_intact() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        fs::write(root.join("file"), b"content").unwrap();
        fs::write(root.join("outside"), b"outside").unwrap();
        symlink(root.join("outside"), root.join("link")).unwrap();
        let report = change_local_ownership(
            &root,
            &["link".into(), "file".into()],
            own(),
            None,
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(report[0].code, Some(ErrorCode::Unsupported));
        assert!(report[1].changed);
        if unsafe { libc::geteuid() } != 0 {
            let report = change_local_ownership(
                &root,
                &["file".into()],
                LocalOwnerEdit {
                    uid: Some(0),
                    gid: None,
                },
                None,
                &Cancel::new(),
            )
            .unwrap();
            assert_eq!(
                report[0].code,
                Some(ErrorCode::PermissionDenied),
                "{report:?}"
            );
            assert_eq!(fs::metadata(root.join("file")).unwrap().uid(), unsafe {
                libc::geteuid()
            });
        }
        let cancel = Cancel::new();
        cancel.cancel();
        let report = change_local_ownership(&root, &["file".into()], own(), None, &cancel).unwrap();
        assert_eq!(report[0].code, Some(ErrorCode::Interrupted));
        assert_eq!(fs::read(root.join("file")).unwrap(), b"content");
        assert_eq!(fs::read(root.join("outside")).unwrap(), b"outside");
    }
    #[test]
    fn invalid_selection_and_exchanged_object_never_reach_chown() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        fs::write(root.join("file"), b"original").unwrap();
        for names in [
            vec!["file".into(), "../outside".into()],
            vec!["file".into(), "file".into()],
        ] {
            assert!(change_local_ownership(&root, &names, own(), None, &Cancel::new()).is_err());
        }
        assert!(change_local_ownership(
            &root,
            &["file".into()],
            LocalOwnerEdit {
                uid: Some(u32::MAX),
                gid: None
            },
            None,
            &Cancel::new()
        )
        .is_err());
        assert!(change_local_ownership(
            &root,
            &["file".into()],
            own(),
            Some(&root.join("other")),
            &Cancel::new()
        )
        .is_err());
        let held = local_fs::RootHandle::open_nofollow(&root).unwrap();
        let expected = local_fs::stat_beneath(&held, &root.join("file")).unwrap();
        fs::rename(root.join("file"), root.join("saved")).unwrap();
        fs::write(root.join("file"), b"substitute").unwrap();
        let error = local_fs::set_interactive_owner_beneath(
            &held,
            &root.join("file"),
            &expected,
            own().uid,
            own().gid,
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(fs::read(root.join("file")).unwrap(), b"substitute");
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn unreadable_linux_object_uses_held_path_descriptor_for_ownership() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        fs::write(root.join("file"), b"content").unwrap();
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o0)).unwrap();
        let report =
            change_local_ownership(&root, &["file".into()], own(), None, &Cancel::new()).unwrap();
        assert!(report[0].changed, "{report:?}");
        assert_eq!(fs::metadata(root.join("file")).unwrap().mode() & 0o777, 0);
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[test]
    fn permitted_group_change_reports_actual_special_bit_effects() {
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        assert!(count >= 0);
        let mut groups = vec![0; count as usize];
        let read = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
        assert_eq!(read, count);
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        fs::write(root.join("file"), b"content").unwrap();
        let before = fs::metadata(root.join("file")).unwrap();
        let Some(group) = groups.into_iter().find(|gid| *gid != before.gid()) else {
            return;
        };
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o6750)).unwrap();
        let special_before = fs::metadata(root.join("file")).unwrap().mode() & 0o7000;
        let report = change_local_ownership(
            &root,
            &["file".into()],
            LocalOwnerEdit {
                uid: None,
                gid: Some(group),
            },
            None,
            &Cancel::new(),
        )
        .unwrap();
        assert!(report[0].changed, "{report:?}");
        let after = fs::metadata(root.join("file")).unwrap();
        assert_eq!((after.uid(), after.gid()), (before.uid(), group));
        assert_eq!(
            report[0].special_bits_changed,
            Some(special_before != after.mode() & 0o7000)
        );
        assert_eq!(fs::read(root.join("file")).unwrap(), b"content");
    }
}
