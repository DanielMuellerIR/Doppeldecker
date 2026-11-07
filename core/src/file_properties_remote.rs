//! Direkte und rekursive Rechteänderungen über den gewöhnlichen Providervertrag.
use super::{
    check_cancel, invalid, rejected, PermissionEditRow, MAX_PERMISSION_ENTRIES,
    MAX_PERMISSION_PATH_BYTES,
};
use crate::{Cancel, Entry, ProviderEndpoint, ProviderKind, Vfs};
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
};

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilePropertyCapabilities {
    pub permissions: bool,
    pub recursive_permissions: bool,
    pub ownership: bool,
    pub administration: bool,
    pub tags: bool,
    pub visibility: bool,
    pub native_flags: bool,
    pub comments: bool,
    pub limitations: Vec<String>,
}

pub fn property_capabilities(kind: ProviderKind) -> FilePropertyCapabilities {
    let permissions = matches!(
        kind,
        ProviderKind::Local | ProviderKind::Sftp | ProviderKind::Ftps
    );
    FilePropertyCapabilities {
        permissions,
        recursive_permissions: permissions,
        ownership: kind == ProviderKind::Local,
        administration: kind == ProviderKind::Local
            && cfg!(any(target_os = "macos", target_os = "linux")),
        tags: kind == ProviderKind::Local && cfg!(target_os = "macos"),
        visibility: kind == ProviderKind::Local && cfg!(target_os = "macos"),
        native_flags: kind == ProviderKind::Local && cfg!(target_os = "macos"),
        comments: kind == ProviderKind::Local && cfg!(target_os = "macos"),
        limitations: match kind {
            ProviderKind::Sftp => vec![crate::sftp::SFTP_WRITE_LIMITATION.into()],
            ProviderKind::Ftps => vec!["FTP/FTPS · Best Effort: SITE CHMOD benötigt Serverunterstützung und lesbare Unix-Rechte. Gleichzeitige serverseitige Pfadänderungen lassen sich nicht atomar ausschließen.".into()],
            ProviderKind::WebDav | ProviderKind::Smb => vec!["Dieser Provider bietet derzeit keinen interaktiven Unix-Rechteeditor. Seine Zugriffsregeln entsprechen nicht allgemein drei Unix-Rechteziffern.".into()],
            ProviderKind::Local => vec![],
        },
    }
}

/// Vollständige Eingabeprüfung vor Verbindungs-/Secretzugriffen.
pub fn validate_permission_selection(
    root: &Path,
    names: &[String],
    mode: u32,
    kind: ProviderKind,
) -> io::Result<()> {
    if mode & !0o777 != 0 || names.is_empty() || names.len() > MAX_PERMISSION_ENTRIES {
        return Err(invalid(
            "Rechteänderung benötigt drei Oktalziffern und 1 bis 100.000 Objekte",
        ));
    }
    if crate::validate_file_operation_root(root)? != root
        || kind != ProviderKind::Local
            && root
                .to_str()
                .is_none_or(|value| value.chars().any(char::is_control))
    {
        return Err(invalid(
            "Rechteänderung benötigt einen normalisierten absoluten Pane-Root",
        ));
    }
    let mut unique = HashSet::new();
    if names.iter().any(|name| {
        name.is_empty()
            || name.contains(['/', '\0'])
            || kind != ProviderKind::Local && name.chars().any(char::is_control)
            || matches!(name.as_str(), "." | "..")
            || !unique.insert(name)
    }) {
        return Err(invalid("Die Auswahl enthält ungültige oder doppelte Namen"));
    }
    if names.iter().map(String::len).sum::<usize>() > MAX_PERMISSION_PATH_BYTES {
        return Err(invalid("Auswahl überschreitet 32 MiB Pfadnamen"));
    }
    Ok(())
}

pub fn change_provider_permissions(
    endpoint: &ProviderEndpoint,
    names: &[String],
    mode: u32,
    recursive: bool,
    cancel: &Cancel,
    binding_current: &dyn Fn() -> io::Result<()>,
) -> io::Result<Vec<PermissionEditRow>> {
    validate_permission_selection(endpoint.root(), names, mode, endpoint.kind())?;
    if !property_capabilities(endpoint.kind()).permissions || endpoint.kind() == ProviderKind::Local
    {
        return Err(crate::coded_error(
            io::ErrorKind::Unsupported,
            crate::ErrorCode::Unsupported,
            "Dieser Endpunkt bietet keine entfernte Unix-Rechteänderung",
        ));
    }
    check_cancel(cancel)?;
    binding_current()?;
    let vfs = endpoint.interactive_permission_context(cancel)?;
    edit_permissions(
        vfs.as_ref(),
        endpoint.root(),
        names,
        mode,
        recursive,
        cancel,
        binding_current,
    )
}

enum Target {
    Object {
        label: String,
        path: PathBuf,
        expected: Entry,
    },
    Rejected(PermissionEditRow),
}

fn edit_permissions(
    vfs: &dyn Vfs,
    root: &Path,
    names: &[String],
    mode: u32,
    recursive: bool,
    cancel: &Cancel,
    binding_current: &dyn Fn() -> io::Result<()>,
) -> io::Result<Vec<PermissionEditRow>> {
    let targets = collect(vfs, root, names, recursive, cancel, binding_current)?;
    let mut rows = Vec::with_capacity(targets.len());
    let mut stopped = false;
    for target in targets {
        let Target::Object {
            label,
            path,
            expected,
        } = target
        else {
            if let Target::Rejected(row) = target {
                rows.push(row);
            }
            continue;
        };
        let result = (|| {
            check_cancel(cancel)?;
            if stopped {
                return Err(crate::coded_error(
                    io::ErrorKind::Interrupted,
                    crate::ErrorCode::Interrupted,
                    "Nach unklarem Ausgang nicht begonnen; unverändert",
                ));
            }
            binding_current()?;
            let actual = vfs.stat(&path)?;
            if !same_observation(&expected, &actual) {
                return Err(crate::real_mode::plan_drift(
                    "Objekt änderte sich vor der Rechteänderung",
                ));
            }
            binding_current()?;
            let changed = vfs.set_interactive_mode(&path, &actual, mode)?;
            binding_current().map_err(|error| crate::error::with_recovery_paths(crate::coded_error(io::ErrorKind::Other, crate::ErrorCode::OutcomeUnclear, format!("Rechteänderung abgeschlossen, Profilbindung danach nicht bestätigt: {error}")), &[path.to_string_lossy().into_owned()]))?;
            Ok(changed)
        })();
        rows.push(match result {
            Ok(entry) => PermissionEditRow {
                name: label,
                mode: entry.mode,
                changed: true,
                code: None,
                message: "Berechtigungen als Best Effort gesetzt und zurückgelesen".into(),
            },
            Err(error) => {
                stopped |= crate::error_code(&error) == crate::ErrorCode::OutcomeUnclear;
                rejected(label, error)
            }
        });
    }
    Ok(rows)
}

pub(crate) fn same_observation(expected: &Entry, actual: &Entry) -> bool {
    crate::batch_rename::same_source(expected, actual)
        && expected.owner == actual.owner
        && expected.group == actual.group
}

fn collect(
    vfs: &dyn Vfs,
    root: &Path,
    names: &[String],
    recursive: bool,
    cancel: &Cancel,
    binding_current: &dyn Fn() -> io::Result<()>,
) -> io::Result<Vec<Target>> {
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
        binding_current()?;
        let label = path
            .strip_prefix(root)
            .ok()
            .and_then(Path::to_str)
            .ok_or_else(|| {
                invalid("Pfad außerhalb des Pane-Roots oder nicht eindeutig dekodierbar")
            })?
            .to_owned();
        if let Some(expected) = visited {
            targets.push(Target::Object {
                label,
                path,
                expected,
            });
            continue;
        }
        let expected = match vfs.stat(&path) {
            Ok(entry) if !entry.is_symlink && (entry.is_file || entry.is_dir) => entry,
            Ok(_) => {
                targets.push(Target::Rejected(rejected(
                    label,
                    invalid("Link oder Spezialdatei; unverändert"),
                )));
                continue;
            }
            Err(error) => {
                targets.push(Target::Rejected(rejected(label, error)));
                continue;
            }
        };
        if recursive && expected.is_dir {
            let listing = match vfs.list_complete(&path).and_then(|listing| {
                crate::fileops::require_complete_safe_listing(&listing, "Rekursive Rechteauswahl")?;
                Ok(listing)
            }) {
                Ok(listing) => listing,
                Err(error) => {
                    targets.push(Target::Rejected(rejected(label, error)));
                    continue;
                }
            };
            count = count.saturating_add(listing.entries.len());
            for child in &listing.entries {
                bytes = bytes.saturating_add(label.len() + 1 + child.entry.name.len());
            }
            if count > MAX_PERMISSION_ENTRIES || bytes > MAX_PERMISSION_PATH_BYTES {
                return Err(invalid("Auswahl überschreitet 100.000 Einträge oder 32 MiB Pfadnamen; kleinere Auswahl verwenden. Noch keine Rechte geändert"));
            }
            pending.push((path.clone(), Some(expected)));
            pending.extend(
                listing
                    .entries
                    .into_iter()
                    .rev()
                    .map(|child| (path.join(child.entry.name), None)),
            );
        } else {
            targets.push(Target::Object {
                label,
                path,
                expected,
            });
        }
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capabilities_and_selection_are_decided_without_provider_access() {
        for kind in [
            ProviderKind::Local,
            ProviderKind::Sftp,
            ProviderKind::Ftps,
            ProviderKind::Smb,
            ProviderKind::WebDav,
        ] {
            let caps = property_capabilities(kind);
            assert_eq!(
                caps.permissions,
                matches!(
                    kind,
                    ProviderKind::Local | ProviderKind::Sftp | ProviderKind::Ftps
                )
            );
            assert_eq!(caps.ownership, kind == ProviderKind::Local);
        }
        for (root, names, mode) in [
            ("relative", vec!["file".into()], 0o600),
            ("/pane/../outside", vec!["file".into()], 0o600),
            ("/pane", vec![], 0o600),
            ("/pane", vec!["../file".into()], 0o600),
            ("/pane", vec!["line\nfile".into()], 0o600),
            ("/pane", vec!["file".into(), "file".into()], 0o600),
            ("/pane", vec!["file".into()], 0o1600),
        ] {
            assert!(validate_permission_selection(
                Path::new(root),
                &names,
                mode,
                ProviderKind::Ftps
            )
            .is_err());
        }
        validate_permission_selection(Path::new("/"), &[" a ".into()], 0o600, ProviderKind::Ftps)
            .unwrap();
        validate_permission_selection(
            Path::new("/local\nfolder"),
            &["line\nfile".into()],
            0o600,
            ProviderKind::Local,
        )
        .unwrap();
    }
}
