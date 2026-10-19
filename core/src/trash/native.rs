use super::journal_v2::claim::{
    check_directory, check_permission_model, ClaimOutcome, ClaimRun, HeldLayout, TransferOutcome,
};
use super::*;
use crate::local_fs::RootHandle;

#[cfg(all(test, target_os = "macos"))]
mod tests;
#[cfg(target_os = "linux")]
mod xdg;

#[cfg(target_os = "macos")]
type Context = RootHandle;
#[cfg(target_os = "linux")]
type Context = xdg::DataHome;

struct Destination {
    root: PathBuf,
    origin_mount: Option<RootHandle>,
    shared_parent: bool,
}

pub(super) fn trash(
    items: &[PathBuf],
    sandbox: &Option<PathBuf>,
    journal_dir: &Path,
) -> io::Result<TrashStats> {
    validate_batch(items)?;
    // Die Sandbox erhält ihr eigenes Layout. HOME und der Systempapierkorb werden
    // in diesem Pfad weder geöffnet noch als Ausweichziel verwendet.
    let sandbox = sandbox
        .as_ref()
        .map(|path| RootHandle::open_nofollow(path))
        .transpose()?;
    let context = if sandbox.is_none() {
        #[cfg(target_os = "macos")]
        let context = RootHandle::open(&home_dir()?)?;
        #[cfg(target_os = "linux")]
        let context = xdg::DataHome::from_environment()?;
        Some(context)
    } else {
        None
    };
    trash_with_context(items, sandbox, journal_dir, context.as_ref())
}

struct Selection {
    path: PathBuf,
    parent: RootHandle,
    expected: Identity,
}

struct Group {
    destination_key: PathBuf,
    layout: HeldLayout,
    items: Vec<PathBuf>,
    selected: Vec<(RootHandle, Identity)>,
    origin_mount: Option<RootHandle>,
}

#[cfg(all(test, target_os = "macos"))]
fn trash_with_home(
    items: &[PathBuf],
    sandbox: Option<RootHandle>,
    journal_dir: &Path,
    home: Option<&RootHandle>,
) -> io::Result<TrashStats> {
    trash_with_context(items, sandbox, journal_dir, home)
}

fn trash_with_context(
    items: &[PathBuf],
    sandbox: Option<RootHandle>,
    journal_dir: &Path,
    context: Option<&Context>,
) -> io::Result<TrashStats> {
    validate_batch(items)?;
    let selected = items
        .iter()
        .map(|path| {
            let parent_path = path.parent().ok_or_else(|| invalid_path(path))?;
            let parent = open_root(parent_path, sandbox.as_ref())?;
            let name = path.file_name().ok_or_else(|| invalid_path(path))?;
            let expected = identity_at_checked(parent.fd(), name)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            Ok(Selection {
                path: path.clone(),
                parent,
                expected,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut destinations = Vec::new();
    for item in &selected {
        let destination = match &sandbox {
            Some(root) => Destination {
                root: root.path().join(".doppeldecker-test-trash"),
                origin_mount: None,
                shared_parent: false,
            },
            None => destination(
                &item.parent,
                context.ok_or_else(|| invalid_path(&item.path))?,
            )?,
        };
        let auxiliaries = vec![destination.root.clone(), journal_dir.to_path_buf()];
        #[cfg(target_os = "linux")]
        let auxiliaries = {
            let mut roots = auxiliaries;
            if let Some(mount) = &destination.origin_mount {
                roots.push(
                    mount
                        .path()
                        .join(format!(".Trash-{}", unsafe { libc::geteuid() })),
                );
            }
            roots
        };
        for auxiliary in auxiliaries {
            if items.iter().any(|path| auxiliary.starts_with(path)) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Papierkorb oder Journal liegt im ausgewählten Teilbaum",
                ));
            }
        }
        destinations.push(destination);
    }
    let journal = private_directory(journal_dir, sandbox.as_ref(), false)?;
    let mut groups: Vec<Group> = Vec::new();
    for (item, destination) in selected.into_iter().zip(destinations) {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.destination_key == destination.root)
        {
            group.items.push(item.path);
            group.selected.push((item.parent, item.expected));
            continue;
        }
        #[cfg(target_os = "macos")]
        let (anchor, files, info) = {
            let files = private_directory(
                &destination.root,
                sandbox.as_ref(),
                destination.shared_parent,
            )?;
            (files.clone(), files, None)
        };
        #[cfg(target_os = "linux")]
        let (anchor, files, info) = xdg::prepare(&destination, sandbox.as_ref(), context)?;
        if files.current_mount_identity()? != item.parent.current_mount_identity()? {
            return Err(io::Error::new(
                io::ErrorKind::CrossesDevices,
                "Papierkorb und Quelle liegen auf unterschiedlichen Mounts",
            ));
        }
        groups.push(Group {
            destination_key: destination.root,
            layout: HeldLayout {
                anchor,
                files,
                info,
                journal: journal.clone(),
                sandbox: sandbox.clone(),
            },
            items: vec![item.path],
            selected: vec![(item.parent, item.expected)],
            origin_mount: destination.origin_mount,
        });
    }
    // Alle Gruppen sind vorbereitet, bevor die erste Quelle verschoben wird.
    // Ein späterer Vorbereitungsfehler erhält auch die bereits erzeugten Journale.
    let mut runs: Vec<(ClaimRun, Vec<PathBuf>, Option<RootHandle>)> = Vec::new();
    for group in groups {
        super::journal_v2::claim::boundary("native_layout_ready", group.layout.files.path())?;
        match ClaimRun::prepare_selected(group.layout, &group.items, Some(&group.selected)) {
            Ok(run) => runs.push((run, group.items, group.origin_mount)),
            Err(mut error) => {
                for (run, _, _) in &mut runs {
                    let pending = run.unclear(io::Error::other("Stapelvorbereitung abgebrochen"));
                    error = crate::error::with_recovery_paths(
                        error,
                        &crate::error::recovery_paths(&pending),
                    );
                }
                return Err(error);
            }
        }
    }
    let mut stats = TrashStats::default();
    let mut stopped = false;
    for (run, paths, origin_mount) in &mut runs {
        if stopped {
            stats.failed += paths.len();
            record_error(
                &mut stats,
                run.unclear(io::Error::other(
                    "Nicht begonnene Gruppe bleibt zur Recovery erhalten",
                )),
            );
            continue;
        }
        for (index, path) in paths.iter().enumerate() {
            let result = run.claim(index).and_then(|claim| match claim {
                ClaimOutcome::ChangedAndRestored => Ok(None),
                ClaimOutcome::Claimed => run.transfer(index, origin_mount.as_ref()).map(Some),
            });
            match result {
                Ok(Some(TransferOutcome::Durable)) => stats.trashed += 1,
                Ok(None) => {
                    stats.failed += 1;
                    stats.errors.push(format!(
                        "{} wurde vor dem Claim ausgetauscht und exklusiv zurückgesetzt",
                        path.display()
                    ));
                }
                Ok(Some(TransferOutcome::Observed(error))) => {
                    stats.trashed += 1;
                    record_error(&mut stats, error);
                    stopped = true;
                }
                Err(error) => {
                    if crate::error_code(&error) == crate::ErrorCode::OutcomeUnclear {
                        stats.unclear += 1;
                    } else {
                        stats.failed += 1;
                    }
                    record_error(&mut stats, run.unclear(error));
                    stopped = true;
                }
            }
            if stopped {
                let remaining = paths.len() - index - 1;
                stats.failed += remaining;
                if remaining > 0 {
                    stats.errors.push(format!(
                        "{remaining} weitere Einträge wegen ungeklärtem Lauf nicht begonnen"
                    ));
                }
                break;
            }
        }
        if !stopped {
            if let Err(error) = run.finish() {
                record_error(&mut stats, error);
                stopped = true;
            }
        }
    }
    Ok(stats)
}

fn record_error(stats: &mut TrashStats, error: io::Error) {
    for path in crate::error::recovery_paths(&error) {
        if !stats.recovery_paths.contains(&path) {
            stats.recovery_paths.push(path);
        }
    }
    stats.errors.push(error.to_string());
}

fn open_root(path: &Path, sandbox: Option<&RootHandle>) -> io::Result<RootHandle> {
    match sandbox {
        Some(root) => RootHandle::open_beneath(root, path),
        None => RootHandle::open_nofollow(path),
    }
}

#[cfg(target_os = "macos")]
fn destination(source: &RootHandle, home: &Context) -> io::Result<Destination> {
    let mount = source.current_mount_identity()?;
    if mount == home.current_mount_identity()? {
        return Ok(Destination {
            root: home.path().join(".Trash"),
            origin_mount: None,
            shared_parent: false,
        });
    }
    let root = mount_root(source)?;
    Ok(Destination {
        root: root
            .path()
            .join(".Trashes")
            .join(unsafe { libc::geteuid() }.to_string()),
        origin_mount: None,
        shared_parent: true,
    })
}

#[cfg(target_os = "linux")]
fn destination(source: &RootHandle, context: &Context) -> io::Result<Destination> {
    xdg::destination(source, context)
}

fn mount_root(source: &RootHandle) -> io::Result<RootHandle> {
    let mount = source.current_mount_identity()?;
    let mut root = source.clone();
    while let Some(parent) = root.path().parent() {
        let candidate = RootHandle::open_nofollow(parent)?;
        if candidate.current_mount_identity()? != mount {
            break;
        }
        root = candidate;
    }
    Ok(root)
}

fn private_directory(
    path: &Path,
    sandbox: Option<&RootHandle>,
    shared_volume_parent: bool,
) -> io::Result<RootHandle> {
    directory(path, sandbox, shared_volume_parent, true)
}

fn directory(
    path: &Path,
    sandbox: Option<&RootHandle>,
    shared_volume_parent: bool,
    private: bool,
) -> io::Result<RootHandle> {
    match open_root(path, sandbox) {
        Ok(root) => {
            check_directory(&root, private)?;
            return Ok(root);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = open_root(path.parent().ok_or_else(|| invalid_path(path))?, sandbox)?;
    directory_at(&parent, path, shared_volume_parent, private)
}

fn directory_at(
    parent: &RootHandle,
    path: &Path,
    shared_volume_parent: bool,
    private: bool,
) -> io::Result<RootHandle> {
    if path.parent() != Some(parent.path()) {
        return Err(invalid_path(path));
    }
    let name = path.file_name().ok_or_else(|| invalid_path(path))?;
    let before = parent_proof(parent, shared_volume_parent)?;
    match mkdirat(parent.fd(), name, PRIVATE_DIR_MODE) {
        Ok(()) => {
            fsync(parent.fd())?;
        }
        Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(error.into()),
    }
    if parent_proof(parent, shared_volume_parent)? != before {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Papierkorb-Eltern änderten sich während der Anlage",
        ));
    }
    let root = RootHandle::open_beneath(parent, path)?;
    // Vorhandene ACLs bleiben unverändert, insbesondere Finders Deny-ACLs.
    check_directory(&root, private)?;
    Ok(root)
}

fn parent_proof(
    root: &RootHandle,
    shared: bool,
) -> io::Result<(Identity, u32, u32, Option<crate::metadata::Acl>)> {
    check_permission_model(root)?;
    let stat = fstat(root.fd())?;
    #[cfg(target_os = "macos")]
    let mode = u32::from(stat.st_mode);
    #[cfg(target_os = "linux")]
    let mode = stat.st_mode;
    let acl = crate::acl::read_fd(root.fd())?;
    if shared {
        let safe_acl = match &acl {
            None => true,
            Some(crate::metadata::Acl::Nfs4(value)) => value
                .entries
                .iter()
                .all(|entry| entry.kind == crate::metadata::Nfs4AceKind::Deny),
            #[cfg(target_os = "linux")]
            Some(crate::metadata::Acl::Posix(_)) => true,
            #[cfg(target_os = "macos")]
            _ => false,
        };
        if !safe_acl
            || ![0, unsafe { libc::geteuid() }].contains(&stat.st_uid)
            || (mode & 0o022 != 0 && mode & 0o1000 == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Gemeinsamer Volume-Papierkorb ist nicht sicher gebunden",
            ));
        }
    } else {
        check_directory(root, false)?;
    }
    // Auch der gemeinsame Elternordner muss dasselbe echte native Rechte-Modell
    // wie das anschließende private Kind besitzen; dessen Gate prüft dies erneut.
    root.current_mount_identity()?;
    Ok((Identity::from_stat(&stat), stat.st_uid, mode, acl))
}
