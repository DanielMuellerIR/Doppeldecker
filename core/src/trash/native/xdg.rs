use super::*;

#[cfg(test)]
mod tests;

pub(super) struct DataHome {
    path: PathBuf,
    ancestor: RootHandle,
}

impl DataHome {
    pub(super) fn from_environment() -> io::Result<Self> {
        let path = match std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
        {
            Some(path) => path,
            None => home_dir()?.join(".local/share"),
        };
        Self::new(path)
    }

    fn new(path: PathBuf) -> io::Result<Self> {
        validate_absolute_path(&path)?;
        let mut existing = path.as_path();
        let ancestor = loop {
            match RootHandle::open_nofollow(existing) {
                Ok(root) => break root,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    existing = existing.parent().ok_or_else(|| invalid_path(&path))?;
                }
                Err(error) => return Err(error),
            }
        };
        Ok(Self { path, ancestor })
    }

    fn prepare(&self) -> io::Result<RootHandle> {
        let fresh = RootHandle::open_nofollow(self.ancestor.path())?;
        if fresh.identity() != self.ancestor.identity()
            || fresh.current_mount_identity()? != self.ancestor.current_mount_identity()?
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Datenhome-Vorfahre wurde ausgetauscht",
            ));
        }
        check_directory(&fresh, false)?;
        let mut root = fresh;
        let names = self
            .path
            .strip_prefix(root.path())
            .map_err(invalid_path_error)?
            .components()
            .map(|component| component.as_os_str().to_os_string())
            .collect::<Vec<_>>();
        for name in names {
            root = directory(&root.path().join(name), None, false, false)?;
            if root.current_mount_identity()? != self.ancestor.current_mount_identity()? {
                return Err(io::Error::new(
                    io::ErrorKind::CrossesDevices,
                    "Datenhome-Mount änderte sich während der Anlage",
                ));
            }
        }
        Ok(root)
    }
}

fn invalid_path_error(_: std::path::StripPrefixError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "Datenhome liegt außerhalb seines Vorfahren",
    )
}

pub(super) fn destination(source: &RootHandle, data_home: &DataHome) -> io::Result<Destination> {
    // Ein fehlendes Datenhome wird erst nach der vollständigen Stapelauswahl
    // angelegt. Sein nächster vorhandener Vorfahre liefert den echten Mount.
    if source.current_mount_identity()? == data_home.ancestor.current_mount_identity()? {
        return Ok(Destination {
            root: data_home.path.join("Trash"),
            origin_mount: None,
            shared_parent: false,
        });
    }
    let mount = mount_root(source)?;
    let uid = unsafe { libc::geteuid() };
    let shared = mount.path().join(".Trash");
    let shared_valid = match statat(mount.fd(), ".Trash", AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat)
            if FileType::from_raw_mode(stat.st_mode).is_dir()
                && stat.st_uid == 0
                && stat.st_mode & 0o1000 != 0 =>
        {
            // Auch ein vorhandenes, aber für diese UID nicht lesbares .Trash
            // verlangt den unabhängigen zweiten Layoutversuch. Das ist kein
            // Abwesenheitsbeweis; dessen eigener Eltern-/Rechtebeleg bleibt nötig.
            (|| -> io::Result<()> {
                let root = RootHandle::open_beneath(&mount, &shared)?;
                if Identity::from_stat(&fstat(root.fd())?) != Identity::from_stat(&stat) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Gemeinsamer Papierkorb wurde ausgetauscht",
                    ));
                }
                parent_proof(&root, true)?;
                Ok(())
            })()
            .is_ok()
        }
        Ok(_) | Err(rustix::io::Errno::NOENT) => false,
        Err(error) => return Err(error.into()),
    };
    Ok(Destination {
        root: if shared_valid {
            shared.join(uid.to_string())
        } else {
            mount.path().join(format!(".Trash-{uid}"))
        },
        origin_mount: Some(mount),
        shared_parent: true,
    })
}

pub(super) fn prepare(
    destination: &Destination,
    sandbox: Option<&RootHandle>,
    data_home: Option<&DataHome>,
) -> io::Result<(RootHandle, RootHandle, Option<RootHandle>)> {
    if let Some(mount) = &destination.origin_mount {
        let primary = prepare_volume(&destination.root, mount);
        if destination.root.parent() == Some(mount.path()) {
            return primary;
        }
        // XDG verlangt auch bei verweigerter UID-Anlage unter .Trash den
        // Versuch des zweiten Layouts. Noch gibt es keinen Claim oder Daten-Rename.
        return primary.or_else(|_| {
            prepare_volume(
                &mount
                    .path()
                    .join(format!(".Trash-{}", unsafe { libc::geteuid() })),
                mount,
            )
        });
    }
    if sandbox.is_none() && destination.origin_mount.is_none() {
        data_home
            .ok_or_else(|| invalid_path(&destination.root))?
            .prepare()?;
    }
    let anchor = directory(
        &destination.root,
        sandbox,
        destination.shared_parent,
        // XDG schreibt für den Home-Trash-Root keine private Zugriffsmaske vor.
        // Der private Claim sowie files/ und info/ werden gesondert geprüft.
        sandbox.is_some(),
    )?;
    let files = private_directory(&anchor.path().join("files"), sandbox, false)?;
    let info = private_directory(&anchor.path().join("info"), sandbox, false)?;
    Ok((anchor, files, Some(info)))
}

fn prepare_volume(
    path: &Path,
    mount: &RootHandle,
) -> io::Result<(RootHandle, RootHandle, Option<RootHandle>)> {
    let parent_path = path.parent().ok_or_else(|| invalid_path(path))?;
    let parent = RootHandle::open_beneath(mount, parent_path)?;
    if parent_path != mount.path() {
        let stat = fstat(parent.fd())?;
        if stat.st_uid != 0 || stat.st_mode & 0o1000 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Gemeinsamer XDG-Papierkorb benötigt Root-Eigentum und Sticky-Bit",
            ));
        }
    }
    let anchor = directory_at(&parent, path, true, true)?;
    let files = private_directory(&anchor.path().join("files"), None, false)?;
    let info = private_directory(&anchor.path().join("info"), None, false)?;
    Ok((anchor, files, Some(info)))
}
