//! Gebundene Dateizugriffe für die privaten DACL-Register.

use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::Path;

use crate::atomic_write::{atomic_write_vfs_with_mode, AtomicWriteMode};
use crate::local_fs::RootHandle;
use crate::{LocalVfs, Vfs};

const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct WritePolicy {
    pub(crate) temp_prefix: &'static str,
    pub(crate) namespace_lost: &'static str,
    pub(crate) registry_replaced: &'static str,
    pub(crate) record_too_large: &'static str,
}

pub(crate) fn file_name(path: &Path) -> io::Result<&OsStr> {
    path.file_name()
        .ok_or_else(|| io::Error::other("Registrydatei hat keinen Dateinamen"))
}

pub(crate) fn utf8_file_name(path: &Path) -> io::Result<&str> {
    file_name(path)?
        .to_str()
        .ok_or_else(|| io::Error::other("Registryname ist kein UTF-8"))
}

pub(crate) fn remove_exact(
    config: &RootHandle,
    path: &Path,
    outside_registry: &'static str,
) -> io::Result<()> {
    // Nur zuvor in unserem dauerhaft geschriebenen Commit benannte Metadaten.
    let parent = path
        .parent()
        .filter(|parent| parent.starts_with(config.path()))
        .ok_or_else(|| io::Error::other(outside_registry))?;
    match crate::local_fs::remove_file_beneath(config, path) {
        Ok(()) => crate::local_fs::sync_dir_beneath(config, parent),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn ensure_private_namespace(
    config: &RootHandle,
    namespace_lost: &'static str,
) -> io::Result<()> {
    if !config.is_current()? || !config.has_exclusive_namespace()? {
        return Err(io::Error::other(namespace_lost));
    }
    Ok(())
}

fn private_parent(
    config: &RootHandle,
    path: &Path,
    namespace_lost: &'static str,
) -> io::Result<()> {
    let relative = path
        .parent()
        .ok_or_else(|| io::Error::other("Registrydatei hat keinen Elternordner"))?
        .strip_prefix(config.path())
        .map_err(io::Error::other)?;
    let mut held = RootHandle::open_nofollow(config.path())?;
    if held.identity() != config.identity() {
        return Err(io::Error::other("Registryordner wurde ausgetauscht"));
    }
    for part in relative.components() {
        let std::path::Component::Normal(name) = part else {
            return Err(io::Error::other("Ungültiger privater Registryordner"));
        };
        match rustix::fs::mkdirat(held.fd(), name, rustix::fs::Mode::from_raw_mode(0o700)) {
            Ok(()) => rustix::fs::fsync(held.fd())?,
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        held = RootHandle::open_nofollow(&held.path().join(name))?;
        if !held.has_exclusive_namespace()? {
            return Err(io::Error::other(
                "Registryunterordner verlor privaten Namensraum",
            ));
        }
    }
    ensure_private_namespace(config, namespace_lost)
}

pub(crate) fn write_json(
    config: &RootHandle,
    path: &Path,
    value: &impl serde::Serialize,
    create: bool,
    policy: &WritePolicy,
) -> io::Result<()> {
    ensure_private_namespace(config, policy.namespace_lost)?;
    private_parent(config, path, policy.namespace_lost)?;
    let vfs = LocalVfs::rooted(config.path())?;
    if Vfs::anchored_identity(&vfs) != Some(config.identity()) {
        return Err(io::Error::other(policy.registry_replaced));
    }
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(io::Error::other(policy.record_too_large));
    }
    atomic_write_vfs_with_mode(
        &vfs,
        path,
        &bytes,
        policy.temp_prefix,
        if create {
            AtomicWriteMode::Create
        } else {
            AtomicWriteMode::Replace
        },
        Some(0o600),
    )
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(
    config: &RootHandle,
    path: &Path,
    record_too_large: &'static str,
) -> io::Result<T> {
    let mut bytes = Vec::new();
    crate::local_fs::open_read_beneath(config, path)?
        .take(MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(io::Error::other(record_too_large));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
