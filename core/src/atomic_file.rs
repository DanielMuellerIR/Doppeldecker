//! Gemeinsame Primitive für atomare reguläre Dateiübertragungen.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::progress::Cancel;
use crate::vfs::{MutationFailure, Vfs};

// Jeder Besitzer eines Staging-Kontexts muss ihn auch auf frühen Fehler- und
// Abbruchpfaden abschließen. Ein fehlgeschlagener Abschluss darf Datenreste nur
// melden, nie über einen ungebundenen Pfad entfernen.
pub(crate) fn finish_staging_scope<T>(
    dst: &dyn Vfs,
    result: io::Result<T>,
    mut record_recovery: impl FnMut(&mut T, String),
) -> io::Result<T> {
    // Ohne bestätigten Ausgang bleibt auch ein schon datenleeres Journal ein
    // notwendiger Intentbeleg. Ein normaler Abschluss würde diesen Beleg löschen.
    if result
        .as_ref()
        .is_err_and(|error| crate::error_code(error) == crate::ErrorCode::OutcomeUnclear)
    {
        return result.map_err(|error| {
            let paths = dst
                .staging_recovery_path()
                .map(|path| vec![path.to_string_lossy().into_owned()])
                .unwrap_or_default();
            crate::error::with_recovery_paths(error, &paths)
        });
    }
    let Err(cleanup) = dst.finish_staging() else {
        return result;
    };
    let Some(path) = dst.staging_recovery_path() else {
        return result.and(Err(cleanup));
    };
    match result {
        Err(error) => Err(crate::error::with_recovery_path(error, path, &cleanup)),
        Ok(mut value) => {
            if crate::error_code(&cleanup) == crate::ErrorCode::PlanDrift {
                let error = crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    "Ziel-Elternbindung beim Staging-Abschluss nicht mehr bewiesen",
                );
                return Err(crate::error::with_recovery_path(error, path, &cleanup));
            }
            record_recovery(&mut value, path.to_string_lossy().into_owned());
            Ok(value)
        }
    }
}

pub(crate) const COPY_BUFFER_BYTES: usize = 128 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct StreamCopy {
    pub complete: bool,
    pub hash: blake3::Hash,
    pub bytes: u64,
}

/// Erzeugt einen laufweit eindeutigen versteckten Nachbarn im selben Verzeichnis.
/// Exklusives Anlegen beziehungsweise Rename bleibt trotzdem die harte Kollisionsgrenze.
pub(crate) fn sibling_candidate(target: &Path, role: &str) -> io::Result<PathBuf> {
    if role.is_empty() || role.contains('/') || role.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Temporäre Dateirolle ist ungültig",
        ));
    }
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Zieldatei hat keinen Elternordner",
        )
    })?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "datei".into());
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    let random = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let suffix = format!(
        ".doppeldecker-{role}-{}-{sequence}-{random}",
        std::process::id()
    );
    // Der Nachbarname ist durch Rolle, PID, Zähler und Zufall länger als der Zielname. Ohne Kürzung
    // sprengt schon eine völlig legale Datei mit 225-Byte-Namen das `NAME_MAX` von
    // 255: `openat` liefert dann `ENAMETOOLONG`, was kein `AlreadyExists` ist — die
    // Suchschleife bricht ab und die Datei wird nie kopiert. Die Eindeutigkeit hängt
    // ohnehin am Suffix aus PID, laufender Nummer und Zufall, nicht am sichtbaren Teil.
    let verfuegbar = NAME_MAX_BYTES.saturating_sub(suffix.len() + 1);
    Ok(parent.join(format!(
        ".{}{suffix}",
        truncate_on_char_boundary(&name, verfuegbar)
    )))
}

/// Namensbudget der üblichen macOS-/Linux-Dateisysteme in Bytes. Ein Provider mit
/// kleinerer Grenze muss einen zu langen Namen weiterhin sichtbar ablehnen.
const NAME_MAX_BYTES: usize = 255;

/// Kürzt auf höchstens `limit` Bytes, ohne ein UTF-8-Zeichen zu zerschneiden.
fn truncate_on_char_boundary(value: &str, limit: usize) -> &str {
    if value.len() <= limit {
        return value;
    }
    let mut ende = limit;
    while ende > 0 && !value.is_char_boundary(ende) {
        ende -= 1;
    }
    &value[..ende]
}

/// Legt eine neue reguläre Temp-Datei neben dem Ziel an. Der Aufrufer liefert seine
/// Sandbox-/Providerprüfung, damit der gemeinsame Helfer keine Sicherheitspolitik kennt.
pub(crate) fn create_neighbor_temp(
    dst: &dyn Vfs,
    target: &Path,
    role: &str,
    validate: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> io::Result<(PathBuf, Box<dyn Write + Send>)> {
    for _ in 0..100 {
        let candidate = dst.staging_candidate(target, role)?;
        validate(&candidate)?;
        match dst.open_write_new(&candidate) {
            Ok(writer) => return Ok((candidate, writer)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Kein freier temporärer Dateiname gefunden",
    ))
}

/// Pumpt einen regulären Strom, prüft zwischen Blöcken das Abbruchsignal und bildet
/// den Digest exakt über die tatsächlich geschriebenen Bytes.
pub(crate) fn copy_stream(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    cancel: &Cancel,
    on_bytes: &mut dyn FnMut(u64),
) -> io::Result<StreamCopy> {
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0_u64;
    loop {
        if cancel.is_cancelled() {
            return Ok(StreamCopy {
                complete: false,
                hash: hasher.finalize(),
                bytes,
            });
        }
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        bytes = bytes.saturating_add(read as u64);
        on_bytes(read as u64);
    }
    writer.flush()?;
    Ok(StreamCopy {
        complete: true,
        hash: hasher.finalize(),
        bytes,
    })
}

pub(crate) fn hash_vfs_file(vfs: &dyn Vfs, path: &Path) -> io::Result<blake3::Hash> {
    let mut reader = vfs.open_read(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(hasher.finalize());
        }
        hasher.update(&buffer[..read]);
    }
}

/// Fehlerphase einer No-Replace-Umbenennung mit anschließendem Elternordner-Sync.
pub(crate) enum SyncedRenameFailure {
    /// Die Umbenennung selbst wurde vom Provider abgelehnt; beide Namen haben aus
    /// Sicht dieses Aufrufs ihre vorherige Bedeutung.
    RenameUnchanged(io::Error),
    /// Der Provider meldete den Fehler möglicherweise erst nach dem Rename. Beide
    /// Namen sind deshalb Recoverypfade; ein Rückweg wäre eine zweite blinde Mutation.
    RenameOutcomeUnclear(io::Error),
    /// Die Umbenennung war erfolgreich, erst die Dauerhaftigkeitsbestätigung schlug
    /// fehl. Der Eintrag ist damit bereits unter dem neuen Namen sichtbar.
    Sync(io::Error),
}

/// Trennt die sichtbare Umbenennung vom nachfolgenden Verzeichnis-Sync.
///
/// Ein gewöhnliches `io::Result` kann beide Fehlerphasen nicht unterscheiden. Für
/// einen sicheren Rückweg müssen wir aber wissen, unter welchem Namen die Daten nach
/// dem Fehler tatsächlich liegen und dürfen deshalb den Rename nicht verstecken.
pub(crate) fn rename_noreplace_with_phase(
    vfs: &dyn Vfs,
    from: &Path,
    to: &Path,
) -> Result<(), SyncedRenameFailure> {
    match vfs.rename_noreplace_with_removal_proof(from, to) {
        Ok(_) => {}
        Err(MutationFailure::Unchanged(error)) => {
            return Err(SyncedRenameFailure::RenameUnchanged(error));
        }
        Err(MutationFailure::OutcomeUnclear(error)) => {
            return Err(SyncedRenameFailure::RenameOutcomeUnclear(error));
        }
    }
    sync_rename_parents(vfs, from, to).map_err(SyncedRenameFailure::Sync)
}

pub(crate) fn sync_rename_parents(vfs: &dyn Vfs, from: &Path, to: &Path) -> io::Result<()> {
    let from_parent = from.parent().ok_or_else(|| invalid_path(from))?;
    let to_parent = to.parent().ok_or_else(|| invalid_path(to))?;
    vfs.sync_dir(from_parent)?;
    if to_parent != from_parent {
        vfs.sync_dir(to_parent)?;
    }
    Ok(())
}

pub(crate) fn sync_parent(vfs: &dyn Vfs, path: &Path) -> io::Result<()> {
    vfs.sync_dir(path.parent().ok_or_else(|| invalid_path(path))?)
}

fn invalid_path(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("Dateipfad besitzt keinen Elternordner: {}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::LocalVfs;

    #[test]
    fn zufaellige_nachbarn_bleiben_bei_langem_utf8_namen_anlegbar() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("ä".repeat(127));
        let vfs = LocalVfs::rooted(&root).unwrap();
        let mut paths = std::collections::HashSet::new();
        for _ in 0..16 {
            let (pending, writer) =
                create_neighbor_temp(&vfs, &target, "test-tmp", &mut |_| Ok(())).unwrap();
            drop(writer);
            assert!(pending.file_name().unwrap().len() <= NAME_MAX_BYTES);
            assert!(paths.insert(pending));
        }
    }

    #[test]
    fn strom_digest_und_noreplace_commit_bleiben_gebunden() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("ziel");
        let vfs = LocalVfs::rooted(&root).unwrap();
        let (pending, mut writer) =
            create_neighbor_temp(&vfs, &target, "test-tmp", &mut |_| Ok(())).unwrap();
        let mut source = &b"vollstaendig"[..];
        let copied =
            copy_stream(&mut source, writer.as_mut(), &Cancel::new(), &mut |_| {}).unwrap();
        drop(writer);
        vfs.sync_file(&pending).unwrap();
        assert!(copied.complete);
        assert_eq!(copied.bytes, b"vollstaendig".len() as u64);
        assert_eq!(hash_vfs_file(&vfs, &pending).unwrap(), copied.hash);
        assert!(matches!(
            rename_noreplace_with_phase(&vfs, &pending, &target),
            Ok(())
        ));
        assert_eq!(std::fs::read(target).unwrap(), b"vollstaendig");
    }
}

#[cfg(test)]
pub(crate) mod staging_fault_tests {
    use super::*;
    use crate::vfs::{CompleteDirectoryListing, Entry};
    use crate::LocalVfs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn c2_unklare_wirkung_erhaelt_auch_datenleeres_journal() {
        let (_temp, root, _source, target, local) = fixture();
        let scope = local
            .staging_scope(&target.join("datei"), Some(&root))
            .unwrap()
            .unwrap();
        let stage = scope.staging_recovery_path().unwrap().to_path_buf();
        let before = std::fs::read(stage.join("journal.json")).unwrap();
        let error = finish_staging_scope::<()>(
            scope.as_ref(),
            Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                "Commitwirkung unklar",
            )),
            |_, _| panic!("Fehler darf keine Erfolgsbilanz erhalten"),
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(stage.join("journal.json")).unwrap(), before);
        let path = stage.to_string_lossy().into_owned();
        assert_eq!(crate::error::recovery_paths(&error), vec![path.clone()]);
        assert!(error.to_string().contains(&path));
        assert!(stage.join("lease").is_file());
    }

    pub(crate) struct FaultVfs {
        pub(crate) inner: Box<dyn Vfs>,
        pub(crate) read_error: bool,
        pub(crate) cancel_on_read: Option<Cancel>,
        pub(crate) finish_error: bool,
    }
    impl Vfs for FaultVfs {
        fn list(&self, p: &Path) -> io::Result<Vec<Entry>> {
            self.inner.list(p)
        }
        fn list_complete(&self, p: &Path) -> io::Result<CompleteDirectoryListing> {
            self.inner.list_complete(p)
        }
        fn stat(&self, p: &Path) -> io::Result<Entry> {
            self.inner.stat(p)
        }
        fn exists(&self, p: &Path) -> bool {
            self.inner.exists(p)
        }
        fn create_dir_all(&self, p: &Path) -> io::Result<()> {
            self.inner.create_dir_all(p)
        }
        fn create_dir(&self, p: &Path) -> io::Result<()> {
            self.inner.create_dir(p)
        }
        fn open_read(&self, p: &Path) -> io::Result<Box<dyn Read + Send>> {
            if self.read_error {
                return Err(crate::coded_error(
                    io::ErrorKind::PermissionDenied,
                    crate::ErrorCode::PermissionDenied,
                    "injizierter Quell-Lesefehler",
                ));
            }
            if let Some(cancel) = &self.cancel_on_read {
                cancel.cancel();
            }
            self.inner.open_read(p)
        }
        fn open_write(&self, p: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write(p)
        }
        fn open_write_new(&self, p: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write_new(p)
        }
        fn sync_file(&self, p: &Path) -> io::Result<()> {
            self.inner.sync_file(p)
        }
        fn sync_dir(&self, p: &Path) -> io::Result<()> {
            self.inner.sync_dir(p)
        }
        fn remove_file(&self, p: &Path) -> io::Result<()> {
            self.inner.remove_file(p)
        }
        fn remove_dir_all(&self, p: &Path) -> io::Result<()> {
            self.inner.remove_dir_all(p)
        }
        fn set_mtime(&self, p: &Path, value: i64) -> io::Result<()> {
            self.inner.set_mtime(p, value)
        }
        fn set_mode(&self, p: &Path, value: u32) -> io::Result<()> {
            self.inner.set_mode(p, value)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }
        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename_noreplace(from, to)
        }
        fn is_local(&self) -> bool {
            self.inner.is_local()
        }
        fn anchored_root(&self) -> Option<&Path> {
            self.inner.anchored_root()
        }
        fn anchored_identity(&self) -> Option<(u64, u64)> {
            self.inner.anchored_identity()
        }
        fn anchor_is_current(&self) -> io::Result<bool> {
            self.inner.anchor_is_current()
        }
        fn staging_scope(
            &self,
            p: &Path,
            sandbox: Option<&Path>,
        ) -> io::Result<Option<Box<dyn Vfs>>> {
            let scope = self.inner.staging_scope(p, sandbox)?;
            if self.read_error && self.finish_error {
                let path = scope.as_ref().unwrap().staging_recovery_path().unwrap();
                return Err(crate::error::with_recovery_paths(
                    crate::coded_error(
                        io::ErrorKind::StorageFull,
                        crate::ErrorCode::StorageFull,
                        "injizierter Vorbereitungsfehler nach privater Staginganlage",
                    ),
                    &[path.to_string_lossy().into_owned()],
                ));
            }
            Ok(scope.map(|inner| {
                Box::new(Self {
                    inner,
                    read_error: self.read_error,
                    cancel_on_read: self.cancel_on_read.clone(),
                    finish_error: self.finish_error,
                }) as Box<dyn Vfs>
            }))
        }
        fn staging_recovery_path(&self) -> Option<&Path> {
            self.inner.staging_recovery_path()
        }
        fn staging_candidate(&self, p: &Path, role: &str) -> io::Result<PathBuf> {
            self.inner.staging_candidate(p, role)
        }
        fn cleanup_owned_temp(&self, p: &Path) -> io::Result<bool> {
            self.inner.cleanup_owned_temp(p)
        }
        fn finish_staging(&self) -> io::Result<()> {
            if self.finish_error {
                let stage = self.inner.staging_recovery_path().unwrap();
                // Ein Datenrest macht blindes rekursives Aufräumen beobachtbar.
                std::fs::write(stage.join("erhaltener-datenrest"), b"erhalten").unwrap();
                return Err(io::Error::other("injizierter Staging-Abschlussfehler"));
            }
            self.inner.finish_staging()
        }
    }
    pub(crate) fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, LocalVfs) {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::local_fs::write_acl_exact(&root, None).unwrap();
        std::fs::write(
            root.join(crate::TEST_SANDBOX_MARKER),
            crate::TEST_SANDBOX_MARKER_CONTENT,
        )
        .unwrap();
        let source = root.join("source");
        let target = root.join("target");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(source.join("datei"), b"Quelle bleibt erhalten").unwrap();
        let local = LocalVfs::rooted(&root).unwrap();
        (temp, root, source.join("datei"), target, local)
    }
    pub(crate) fn stages(root: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(".doppeldecker-stage-")
            })
            .collect()
    }
}
