//! Private, versionsgebundene Arbeitskopien für bewusstes Remote-Bearbeiten.
//! Die Kopie für einen Queueauftrag bleibt von späteren Editoränderungen getrennt.

use crate::local_fs::{open_read_beneath, RootHandle};
use crate::{coded_error, Cancel, ErrorCode, LocalVfs, Vfs};
use std::fs::Metadata;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub struct WorkingCopy {
    pub path: PathBuf,
    pub bytes: u64,
    pub blake3: String,
}

#[derive(PartialEq, Eq)]
struct Version {
    device: u64,
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    mode: u32,
}
impl Version {
    fn of(meta: &Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
            mode: meta.mode(),
        }
    }
}

fn drift() -> io::Error {
    coded_error(
        io::ErrorKind::InvalidData,
        ErrorCode::PlanDrift,
        "Arbeitsdatei änderte sich während der Savevorbereitung",
    )
}
fn not_cancelled(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::Interrupted,
            "Remote-Bearbeitung abgebrochen",
        ))
    } else {
        Ok(())
    }
}

fn read_version(
    source: &Path,
    cancel: &Cancel,
    mut consume: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<(u64, blake3::Hash)> {
    not_cancelled(cancel)?;
    let parent = source.parent().ok_or_else(drift)?;
    let root = RootHandle::open_nofollow(parent)?;
    let mut input = open_read_beneath(&root, source)?;
    let before = Version::of(&input.metadata()?);
    let mut hash = blake3::Hasher::new();
    let mut count = 0u64;
    let mut buffer = vec![0; crate::atomic_file::COPY_BUFFER_BYTES];
    loop {
        not_cancelled(cancel)?;
        let bytes = input.read(&mut buffer)?;
        if !root.is_current()? || Version::of(&input.metadata()?) != before {
            return Err(drift());
        }
        if bytes == 0 {
            break;
        }
        count = count.checked_add(bytes as u64).ok_or_else(drift)?;
        if count > before.size {
            return Err(drift());
        }
        consume(&buffer[..bytes])?;
        hash.update(&buffer[..bytes]);
    }
    let visible = open_read_beneath(&root, source)?;
    if count != before.size || Version::of(&visible.metadata()?) != before || !root.is_current()? {
        return Err(drift());
    }
    Ok((count, hash.finalize()))
}

pub fn working_copy_hash(source: &Path, cancel: &Cancel) -> io::Result<String> {
    read_version(source, cancel, |_| Ok(())).map(|(_, hash)| hash.to_hex().to_string())
}

/// Erstellt ausschließlich eine neue private Datei unter einem eigenen Cacheordner.
/// Der ursprüngliche Basisname bleibt für Systemanwendung und Queue erhalten.
pub fn create_working_copy(
    source: &Path,
    cache: &Path,
    name: &str,
    cancel: &Cancel,
) -> io::Result<WorkingCopy> {
    not_cancelled(cancel)?;
    if name.is_empty() || matches!(name, "." | "..") || name.contains('/') {
        return Err(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Originalname ist kein einzelner Dateiname",
        ));
    }
    crate::remote_snapshot::prepare_cache_root(cache)?;
    let cache = std::fs::canonicalize(cache)?;
    let (directory, _) = crate::remote_snapshot::reserve_run_dir(&cache)?;
    let target = directory.join(name);
    let vfs = LocalVfs::rooted(&directory)?;
    let result = (|| {
        let mut output = vfs.open_write_new(&target)?;
        let (bytes, hash) = read_version(source, cancel, |data| output.write_all(data))?;
        output.flush()?;
        drop(output);
        vfs.set_mode(&target, 0o600)?;
        vfs.sync_file(&target)?;
        vfs.sync_dir(&directory)?;
        Ok(WorkingCopy {
            path: target.clone(),
            bytes,
            blake3: hash.to_hex().to_string(),
        })
    })();
    if result.is_err() {
        let _ = vfs.remove_file(&target);
        let _ = std::fs::remove_dir(&directory);
    }
    result
}

/// Bindet dieselbe ursprüngliche Inhaltsversion als normalen Queue-Konfliktbeleg.
/// Der Executor prüft diesen Beleg vor der Ersetzung erneut; eine Änderung zwischen
/// Save-Klick und Queueausführung erteilt damit kein neues Überschreibrecht.
pub fn bind_original_replacement(
    target: &dyn Vfs,
    source: &Path,
    original: &crate::RemoteOriginalProof,
) -> io::Result<Vec<crate::FileConflictProof>> {
    let proofs = crate::fileops::capture_file_conflict_proofs(
        target,
        &[source.to_string_lossy().into_owned()],
    )?;
    if proofs.len() != 1
        || !proofs[0].is_file
        || proofs[0].is_symlink
        || proofs[0].size != original.bytes
        || proofs[0]
            .content_hash
            .map(|hash| blake3::Hash::from_bytes(hash).to_hex().to_string())
            .as_deref()
            != Some(original.blake3.as_str())
        || original
            .mtime
            .is_some_and(|mtime| proofs[0].mtime != Some(mtime))
        || original
            .mode
            .is_some_and(|mode| proofs[0].mode != Some(mode))
    {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::PlanDrift,
            "Originaldatei änderte sich vor dem Rückupload; bitte neu öffnen",
        ));
    }
    Ok(proofs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn arbeitskopie_bleibt_privat_und_wird_nicht_mit_dem_editor_geaendert() {
        let source = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let path = source.path().canonicalize().unwrap().join("Original.html");
        std::fs::write(&path, b"Original").unwrap();
        let copy =
            create_working_copy(&path, cache.path(), "Original.html", &Cancel::new()).unwrap();
        assert_eq!(copy.path.file_name().unwrap(), "Original.html");
        assert_eq!(copy.bytes, 8);
        assert_eq!(
            copy.blake3,
            working_copy_hash(&path, &Cancel::new()).unwrap()
        );
        assert_eq!(
            copy.path.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&path, b"Spaeter").unwrap();
        assert_eq!(std::fs::read(&copy.path).unwrap(), b"Original");
        let cancelled = Cancel::new();
        cancelled.cancel();
        assert_eq!(
            crate::error_code(
                &create_working_copy(&path, cache.path(), "Abbruch", &cancelled)
                    .err()
                    .unwrap()
            ),
            ErrorCode::Interrupted
        );
    }
    #[test]
    fn arbeitskopie_folgt_keinen_quell_oder_cache_symlinks() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let path = root.join("Datei");
        std::fs::write(&path, b"Original").unwrap();
        std::os::unix::fs::symlink(&path, root.join("Link")).unwrap();
        assert!(create_working_copy(
            &root.join("Link"),
            &root.join("Cache"),
            "Ziel",
            &Cancel::new()
        )
        .is_err());
        std::os::unix::fs::symlink(root.join("Cache"), root.join("CacheLink")).unwrap();
        assert!(
            create_working_copy(&path, &root.join("CacheLink"), "Ziel", &Cancel::new()).is_err()
        );
    }
}
