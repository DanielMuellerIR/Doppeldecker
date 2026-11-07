//! Enger Metadatenauftrag für einen separat authentifizierten lokalen Helfer.
//! Die serialisierbare Anfrage enthält keine Freigabe. Erst der native Transport
//! authentifiziert den Aufrufer; die Ausführung hält zusätzlich ihren eigenen Root.
use super::{check_cancel, invalid, owner, validate_local_selection, LocalOwnerEdit};
use crate::{local_fs, Cancel, ErrorCode, LocalRootIdentity};
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum AdminMetadataAction {
    Permissions { mode: u32, recursive: bool },
    Ownership { uid: Option<u32>, gid: Option<u32> },
}
impl AdminMetadataAction {
    fn validate(&self) -> io::Result<()> {
        match *self {
            Self::Permissions { mode, .. } if mode & !0o777 == 0 => Ok(()),
            Self::Ownership { uid, gid } => owner::validate_edit(LocalOwnerEdit { uid, gid }),
            _ => Err(invalid("Administratorauftrag enthält ungültige Rechte")),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdminMetadataRequest {
    version: u32,
    pub request_id: String,
    root: PathBuf,
    names: Vec<String>,
    action: AdminMetadataAction,
    expected_root: LocalRootIdentity,
    selected: Vec<SelectedObject>,
    sandbox: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SelectedObject {
    name: String,
    device: Option<u64>,
    inode: Option<u64>,
    file: bool,
    directory: bool,
    symlink: bool,
    size: u64,
    mtime: Option<i64>,
    mode: Option<u32>,
}
impl From<&crate::Entry> for SelectedObject {
    fn from(entry: &crate::Entry) -> Self {
        Self {
            name: entry.name.clone(),
            device: entry.device,
            inode: entry.inode,
            file: entry.is_file,
            directory: entry.is_dir,
            symlink: entry.is_symlink,
            size: entry.size,
            mtime: entry.mtime,
            mode: entry.mode,
        }
    }
}

pub struct BoundAdminMetadataRequest {
    held: local_fs::RootHandle,
    request: AdminMetadataRequest,
    direct: Vec<crate::Entry>,
    permissions: Option<Vec<super::PermissionTarget>>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "rows", rename_all = "camelCase")]
pub enum AdminMetadataResult {
    Permissions(Vec<super::PermissionEditRow>),
    Ownership(Vec<super::OwnerEditRow>),
}

impl AdminMetadataRequest {
    pub fn capture(
        request_id: String,
        root: &Path,
        names: Vec<String>,
        action: AdminMetadataAction,
        sandbox: Option<&Path>,
        cancel: &Cancel,
    ) -> io::Result<Self> {
        validate_id(&request_id)?;
        action.validate()?;
        validate_local_selection(root, &names, sandbox)?;
        let held = local_fs::RootHandle::open_nofollow(root)?;
        let expected_root = local_fs::root_identity_beneath(&held, root)?;
        let selected = names
            .iter()
            .map(|name| {
                check_cancel(cancel)?;
                local_fs::stat_beneath(&held, &root.join(name))
                    .map(|entry| SelectedObject::from(&entry))
            })
            .collect::<io::Result<Vec<_>>>()?;
        if !held.is_current()? {
            return Err(crate::real_mode::plan_drift(
                "Auswahlordner vor Administratorfreigabe verändert",
            ));
        }
        Ok(Self {
            version: 1,
            request_id,
            root: root.into(),
            names,
            action,
            expected_root,
            selected,
            sandbox: sandbox.map(Path::to_path_buf),
        })
    }

    /// Nur nach nativer Authentifizierung aufrufen. Kein vom Frontend erzeugter
    /// Identitätswert ersetzt die hier erneut geöffnete, gehaltene Wurzel.
    pub fn bind(self, cancel: &Cancel) -> io::Result<BoundAdminMetadataRequest> {
        check_cancel(cancel)?;
        if self.version != 1 {
            return Err(invalid("Unbekannte Version des Administratorauftrags"));
        }
        validate_id(&self.request_id)?;
        self.action.validate()?;
        validate_local_selection(&self.root, &self.names, self.sandbox.as_deref())?;
        let held = local_fs::RootHandle::open_nofollow(&self.root)?;
        if local_fs::root_identity_beneath(&held, &self.root)? != self.expected_root
            || !held.is_current()?
        {
            return Err(crate::real_mode::plan_drift(
                "Auswahlordner seit Administratorfreigabe verändert",
            ));
        }
        if self.selected.len() != self.names.len() {
            return Err(invalid("Unvollständige gebundene Administratorauswahl"));
        }
        let mut direct = Vec::with_capacity(self.names.len());
        for (name, selected) in self.names.iter().zip(&self.selected) {
            check_cancel(cancel)?;
            let actual = local_fs::stat_beneath(&held, &self.root.join(name))?;
            if selected.name != *name
                || selected.device.is_none()
                || selected.inode.is_none()
                || SelectedObject::from(&actual) != *selected
            {
                return Err(crate::real_mode::plan_drift(
                    "Ausgewähltes Objekt während Administratorfreigabe verändert",
                ));
            }
            direct.push(actual);
        }
        let permissions = if let AdminMetadataAction::Permissions { recursive, .. } = self.action {
            let targets =
                super::collect_targets(&held, &self.root, &self.names, recursive, cancel)?;
            let selected_by_name = self
                .selected
                .iter()
                .map(|selected| (selected.name.as_str(), selected))
                .collect::<std::collections::HashMap<_, _>>();
            for target in &targets {
                if let super::PermissionTarget::Object { name, expected, .. } = target {
                    if let Some(&selected) = selected_by_name.get(name.as_str()) {
                        if SelectedObject::from(expected) != *selected {
                            return Err(crate::real_mode::plan_drift(
                                "Ausgewähltes Objekt während Administratorerfassung verändert",
                            ));
                        }
                    }
                }
            }
            Some(targets)
        } else {
            None
        };
        Ok(BoundAdminMetadataRequest {
            held,
            request: self,
            direct,
            permissions,
        })
    }
}
impl BoundAdminMetadataRequest {
    pub fn execute(self, cancel: &Cancel) -> io::Result<AdminMetadataResult> {
        check_cancel(cancel)?;
        if !self.held.is_current()? {
            return Err(crate::real_mode::plan_drift(
                "Gebundener Administratorordner ausgetauscht",
            ));
        }
        match self.request.action {
            AdminMetadataAction::Permissions { mode, recursive } => {
                super::change_permissions_bound(
                    &self.held,
                    &self.request.names,
                    mode,
                    recursive,
                    self.permissions,
                    cancel,
                )
                .map(AdminMetadataResult::Permissions)
            }
            AdminMetadataAction::Ownership { uid, gid } => owner::change_ownership_bound(
                &self.held,
                &self.request.names,
                LocalOwnerEdit { uid, gid },
                Some(&self.direct),
                cancel,
            )
            .map(AdminMetadataResult::Ownership),
        }
    }
}
fn validate_id(id: &str) -> io::Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(crate::coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Ungültige Administratorauftrags-ID",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{symlink, PermissionsExt},
    };

    fn request(root: &Path, names: &[&str]) -> AdminMetadataRequest {
        AdminMetadataRequest::capture(
            "admin-test".into(),
            root,
            names.iter().map(|name| name.to_string()).collect(),
            AdminMetadataAction::Permissions {
                mode: 0o640,
                recursive: true,
            },
            Some(root),
            &Cancel::new(),
        )
        .unwrap()
    }
    #[test]
    fn administrator_request_roundtrip_uses_normal_recursive_core_and_skips_symlinks() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        fs::create_dir(root.join("folder")).unwrap();
        fs::write(root.join("folder/file"), b"content").unwrap();
        fs::write(root.join("outside"), b"untouched").unwrap();
        symlink(root.join("outside"), root.join("folder/link")).unwrap();
        let before = fs::metadata(root.join("outside"))
            .unwrap()
            .permissions()
            .mode();
        let bytes = serde_json::to_vec(&request(&root, &["folder"])).unwrap();
        let parsed: AdminMetadataRequest = serde_json::from_slice(&bytes).unwrap();
        let AdminMetadataResult::Permissions(rows) = parsed
            .bind(&Cancel::new())
            .unwrap()
            .execute(&Cancel::new())
            .unwrap()
        else {
            panic!("Falsche Ergebnisart")
        };
        assert!(rows.iter().any(|r| r.name == "folder/file" && r.changed));
        assert!(rows.iter().any(|r| r.name == "folder/link"
            && !r.changed
            && r.code == Some(ErrorCode::Unsupported)));
        fs::set_permissions(root.join("folder"), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fs::read(root.join("folder/file")).unwrap(), b"content");
        assert_eq!(
            fs::metadata(root.join("outside"))
                .unwrap()
                .permissions()
                .mode(),
            before
        );
    }
    #[test]
    fn administrator_request_rejects_root_exchange_before_binding_and_before_execution() {
        let area = tempfile::tempdir().unwrap();
        let outer = area.path().canonicalize().unwrap();
        let root = outer.join("pane");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file"), b"selected").unwrap();
        let first = request(&root, &["file"]);
        let bound = first.clone().bind(&Cancel::new()).unwrap();
        fs::rename(&root, outer.join("old")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file"), b"replacement").unwrap();
        assert_eq!(
            crate::error_code(&first.bind(&Cancel::new()).err().unwrap()),
            ErrorCode::PlanDrift
        );
        assert_eq!(
            crate::error_code(&bound.execute(&Cancel::new()).unwrap_err()),
            ErrorCode::PlanDrift
        );
        assert_eq!(fs::read(root.join("file")).unwrap(), b"replacement");
        assert_eq!(fs::read(outer.join("old/file")).unwrap(), b"selected");
    }
    #[test]
    fn administrator_selection_rejects_replacement_during_authentication_and_after_binding() {
        for ownership in [false, true] {
            let area = tempfile::tempdir().unwrap();
            let root = area.path().canonicalize().unwrap();
            let file = root.join("file");
            fs::write(&file, b"selected object").unwrap();
            let action = if ownership {
                AdminMetadataAction::Ownership {
                    uid: Some(unsafe { libc::geteuid() }),
                    gid: None,
                }
            } else {
                AdminMetadataAction::Permissions {
                    mode: 0o640,
                    recursive: false,
                }
            };
            let captured = AdminMetadataRequest::capture(
                "selection-swap".into(),
                &root,
                vec!["file".into()],
                action,
                Some(&root),
                &Cancel::new(),
            )
            .unwrap();
            let bound = captured.clone().bind(&Cancel::new()).unwrap();
            fs::rename(&file, root.join("original")).unwrap();
            fs::write(&file, b"foreign replacement").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(
                crate::error_code(&captured.bind(&Cancel::new()).err().unwrap()),
                ErrorCode::PlanDrift
            );
            match bound.execute(&Cancel::new()).unwrap() {
                AdminMetadataResult::Permissions(rows) => {
                    assert_eq!(rows[0].code, Some(ErrorCode::PlanDrift));
                    assert!(!rows[0].changed);
                }
                AdminMetadataResult::Ownership(rows) => {
                    assert_eq!(rows[0].code, Some(ErrorCode::PlanDrift));
                    assert!(!rows[0].changed);
                }
            }
            assert_eq!(
                fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(fs::read(file).unwrap(), b"foreign replacement");
            assert_eq!(fs::read(root.join("original")).unwrap(), b"selected object");
        }
    }

    #[test]
    fn administrator_request_validates_action_selection_protocol_and_cancellation() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        fs::write(root.join("file"), b"content").unwrap();
        for action in [
            AdminMetadataAction::Permissions {
                mode: 0o4640,
                recursive: false,
            },
            AdminMetadataAction::Ownership {
                uid: Some(u32::MAX),
                gid: None,
            },
            AdminMetadataAction::Ownership {
                uid: None,
                gid: None,
            },
        ] {
            assert!(AdminMetadataRequest::capture(
                "valid".into(),
                &root,
                vec!["file".into()],
                action,
                Some(&root),
                &Cancel::new(),
            )
            .is_err());
        }
        assert!(AdminMetadataRequest::capture(
            "bad/id".into(),
            &root,
            vec!["file".into()],
            AdminMetadataAction::Permissions {
                mode: 0o640,
                recursive: false
            },
            Some(&root),
            &Cancel::new(),
        )
        .is_err());
        let before = fs::metadata(root.join("file"))
            .unwrap()
            .permissions()
            .mode();
        let bound = request(&root, &["file"]).bind(&Cancel::new()).unwrap();
        let cancel = Cancel::new();
        cancel.cancel();
        assert_eq!(
            crate::error_code(&bound.execute(&cancel).unwrap_err()),
            ErrorCode::Interrupted
        );
        assert_eq!(
            fs::metadata(root.join("file"))
                .unwrap()
                .permissions()
                .mode(),
            before
        );
        let mut value = serde_json::to_value(request(&root, &["file"])).unwrap();
        value["version"] = 2.into();
        assert!(
            serde_json::from_value::<AdminMetadataRequest>(value.clone())
                .unwrap()
                .bind(&Cancel::new())
                .is_err()
        );
        value["version"] = 1.into();
        value["command"] = "arbitrary shell".into();
        assert!(serde_json::from_value::<AdminMetadataRequest>(value).is_err());
    }
}
