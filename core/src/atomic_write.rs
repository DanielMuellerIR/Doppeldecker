//! Gemeinsamer absturzfester Write-then-Rename-Pfad für kleine Zustandsdateien.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{coded_error, error_code, with_recovery_context, ErrorCode};
use crate::vfs::{LocalVfs, Vfs};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AtomicWriteMode {
    /// Der endgültige Name muss frei bleiben; bestehender Zustand gewinnt.
    Create,
    /// Eine vorhandene Zustandsdatei darf atomar ersetzt werden.
    Replace,
}

/// Schreibt eine kleine Zustandsdatei vollständig unter einen exklusiven Tempnamen,
/// synchronisiert sie und committed sie erst danach per Rename. Nach dem Commit wird
/// auch der Elternordner synchronisiert. Fehler vor dem Rename entfernen den Tempnamen
/// ausschließlich mit einem bereits bei seiner Anlage atomar gebundenen Beleg; ohne
/// diesen Beleg bleibt der Pfad sichtbar als Recoverybestand stehen.
pub(crate) fn atomic_write_vfs(
    vfs: &dyn Vfs,
    path: &Path,
    payload: &[u8],
    temp_prefix: &str,
    mode: AtomicWriteMode,
) -> io::Result<()> {
    atomic_write_vfs_with_mode(vfs, path, payload, temp_prefix, mode, None)
}

pub(crate) fn atomic_write_vfs_with_mode(
    vfs: &dyn Vfs,
    path: &Path,
    payload: &[u8],
    temp_prefix: &str,
    mode: AtomicWriteMode,
    file_mode: Option<u32>,
) -> io::Result<()> {
    if !temp_prefix.starts_with('.') || temp_prefix.contains('/') || temp_prefix.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Atomarer Temppräfix ist ungültig",
        ));
    }
    let parent = path.parent().ok_or_else(|| invalid_path(path))?;
    vfs.create_dir_all(parent)?;
    let temp = unique_temp_path(parent, temp_prefix)?;
    let (mut writer, removal_proof) = match vfs.open_write_new_with_removal_proof(&temp) {
        Ok(created) => created,
        Err(crate::vfs::MutationFailure::Unchanged(error)) => return Err(error),
        Err(crate::vfs::MutationFailure::OutcomeUnclear(error)) => {
            let failure = coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Das exklusive Anlegen der atomaren Temp-Datei meldete einen Fehler nach möglicher Wirkung ({error}). Der Name blieb unangetastet. Möglicher Temp-Recoverypfad: {}",
                    temp.display(),
                ),
            );
            return Err(with_recovery_context(failure, &[&error], &[&temp]));
        }
    };
    let prepare = (|| {
        writer.write_all(payload)?;
        writer.flush()?;
        drop(writer);
        if let Some(file_mode) = file_mode {
            vfs.set_mode(&temp, file_mode)?;
        }
        vfs.sync_file(&temp)
    })();
    if let Err(error) = prepare {
        return fail_before_atomic_commit(vfs, &temp, removal_proof.as_ref(), error);
    }

    match mode {
        AtomicWriteMode::Create => match vfs.rename_noreplace_with_removal_proof(&temp, path) {
            Ok(_) => {}
            Err(crate::vfs::MutationFailure::Unchanged(error)) => {
                return fail_before_atomic_commit(vfs, &temp, removal_proof.as_ref(), error);
            }
            Err(crate::vfs::MutationFailure::OutcomeUnclear(error)) => {
                return Err(unclear_atomic_commit(&temp, path, error));
            }
        },
        AtomicWriteMode::Replace => {
            if let Err(error) = vfs.rename(&temp, path) {
                // Nur ein unverändert abgelehntes lokales Kernel-Rename erlaubt
                // Cleanup. Auch ein lokaler Stagingadapter kann nach dem Rename
                // noch einen unklaren Journal- oder Bindungsfehler melden.
                if vfs.is_local() && error_code(&error) != ErrorCode::OutcomeUnclear {
                    return fail_before_atomic_commit(vfs, &temp, removal_proof.as_ref(), error);
                }
                return Err(unclear_atomic_commit(&temp, path, error));
            }
        }
    }
    vfs.sync_dir(parent).map_err(|error| {
        let failure = coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "Die Zustandsdatei wurde sichtbar nach {} committet, aber der Elternordner konnte nicht dauerhaft synchronisiert werden ({error}). Der frühere Tempname wurde nicht bereinigt: {}",
                path.display(),
                temp.display(),
            ),
        );
        with_recovery_context(failure, &[&error], &[&temp, path])
    })
}

fn fail_before_atomic_commit(
    vfs: &dyn Vfs,
    temp: &Path,
    removal_proof: Option<&crate::vfs::ProofListEntry>,
    cause: io::Error,
) -> io::Result<()> {
    let Some(removal_proof) = removal_proof else {
        let failure = coded_error(
            cause.kind(),
            error_code(&cause),
            format!(
                "{cause}. Die Temp-Datei besitzt keinen atomar an ihre Anlage gebundenen Entfernungsbeleg und blieb unangetastet. Temp-Recoverypfad: {}",
                temp.display(),
            ),
        );
        return Err(with_recovery_context(failure, &[&cause], &[temp]));
    };
    if let Err(remove_error) = vfs.remove_if_matches(temp, removal_proof) {
        let failure = coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "{cause}. Die Temp-Datei konnte nicht identitätsgebunden entfernt werden ({remove_error}) und blieb unangetastet. Temp-Recoverypfad: {}",
                temp.display(),
            ),
        );
        return Err(with_recovery_context(
            failure,
            &[&cause, &remove_error],
            &[temp],
        ));
    }
    if let Err(sync_error) = vfs.sync_dir(temp.parent().ok_or_else(|| invalid_path(temp))?) {
        let failure = coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "{cause}. Die gebundene Temp-Datei wurde entfernt, aber ihr Elternordner konnte nicht dauerhaft synchronisiert werden ({sync_error}). Früherer Temp-Pfad: {}",
                temp.display(),
            ),
        );
        return Err(with_recovery_context(
            failure,
            &[&cause, &sync_error],
            &[temp],
        ));
    }
    Err(cause)
}

fn unclear_atomic_commit(temp: &Path, path: &Path, cause: io::Error) -> io::Error {
    let failure = coded_error(
        io::ErrorKind::Other,
        ErrorCode::OutcomeUnclear,
        format!(
            "Das Commit-Rename der Zustandsdatei meldete einen Fehler nach möglicher Wirkung ({cause}). Es wurde kein pfadbasierter Cleanup versucht. Recoverypfade: möglicher Tempbestand: {}; mögliches Ziel: {}",
            temp.display(),
            path.display(),
        ),
    );
    with_recovery_context(failure, &[&cause], &[temp, path])
}

/// Lokale Variante: Der vorhandene Elternordner wird einmal kanonisch aufgelöst und
/// danach descriptor-relativ gehalten. Das erlaubt normale Systempfade mit einem
/// kanonischen Symlink-Präfix, ohne während Write/Rename erneut Pfadstrings zu öffnen.
pub(crate) fn atomic_write_local(
    path: &Path,
    payload: &[u8],
    temp_prefix: &str,
    mode: AtomicWriteMode,
) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| invalid_path(path))?;
    std::fs::create_dir_all(parent)?;
    let parent = std::fs::canonicalize(parent)?;
    let name = path.file_name().ok_or_else(|| invalid_path(path))?;
    let anchored_path = parent.join(name);
    let vfs = LocalVfs::rooted(&parent)?;
    // Journale enthalten zwar keine Secrets, sollen aber wie die frühere Queue-Datei
    // unabhängig von der Prozess-umask ausschließlich für den Nutzer lesbar sein.
    atomic_write_vfs_with_mode(
        &vfs,
        &anchored_path,
        payload,
        temp_prefix,
        mode,
        Some(0o600),
    )
}

fn unique_temp_path(parent: &Path, prefix: &str) -> io::Result<PathBuf> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    let random = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(parent.join(format!(
        "{prefix}{}-{sequence}-{random}.tmp",
        std::process::id()
    )))
}

fn invalid_path(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "Zustandsdatei braucht Elternordner und Namen: {}",
            path.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{Entry, MutationFailure, ProofListEntry};
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    #[derive(Clone, Copy, PartialEq)]
    enum FailurePoint {
        Create,
        Prepare,
        Commit,
        DirectorySync,
    }

    struct AtomicWriteFaultVfs {
        inner: LocalVfs,
        temp: Mutex<Option<PathBuf>>,
        rename_completed: AtomicBool,
        failure: FailurePoint,
        prior_recovery: PathBuf,
    }

    impl AtomicWriteFaultVfs {
        fn new(failure: FailurePoint, prior_recovery: PathBuf) -> Self {
            Self {
                inner: LocalVfs::new(),
                temp: Mutex::new(None),
                rename_completed: AtomicBool::new(false),
                failure,
                prior_recovery,
            }
        }

        fn injected_error(&self, code: ErrorCode) -> io::Error {
            crate::error::with_recovery_paths(
                coded_error(
                    io::ErrorKind::PermissionDenied,
                    code,
                    "injizierter Zustandsdateifehler",
                ),
                &[self.prior_recovery.to_string_lossy().into_owned()],
            )
        }
    }

    impl Vfs for AtomicWriteFaultVfs {
        fn is_local(&self) -> bool {
            true
        }

        fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
            self.inner.list(path)
        }

        fn stat(&self, path: &Path) -> io::Result<Entry> {
            self.inner.stat(path)
        }

        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }

        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir_all(path)
        }

        fn create_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir(path)
        }

        fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
            self.inner.open_read(path)
        }

        fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write(path)
        }

        fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            *self.temp.lock().unwrap() = Some(path.to_path_buf());
            self.inner.open_write_new(path)
        }

        fn open_write_new_with_removal_proof(
            &self,
            path: &Path,
        ) -> Result<(Box<dyn Write + Send>, Option<ProofListEntry>), MutationFailure> {
            let writer = self
                .open_write_new(path)
                .map_err(MutationFailure::Unchanged)?;
            if self.failure == FailurePoint::Create {
                return Err(MutationFailure::OutcomeUnclear(
                    self.injected_error(ErrorCode::OutcomeUnclear),
                ));
            }
            Ok((writer, None))
        }

        fn sync_file(&self, path: &Path) -> io::Result<()> {
            if self.failure == FailurePoint::Prepare {
                return Err(self.injected_error(ErrorCode::PermissionDenied));
            }
            self.inner.sync_file(path)
        }

        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            if self.failure == FailurePoint::DirectorySync
                && self.rename_completed.swap(false, Ordering::SeqCst)
            {
                let temp = self.temp.lock().unwrap().clone().unwrap();
                std::fs::write(temp, b"fremd").unwrap();
                return Err(self.injected_error(ErrorCode::OutcomeUnclear));
            }
            self.inner.sync_dir(path)
        }

        fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
            self.inner.set_mtime(path, mtime)
        }

        fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
            self.inner.set_mode(path, mode)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_file(path)
        }

        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_dir_all(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)?;
            self.rename_completed.store(true, Ordering::SeqCst);
            if self.failure == FailurePoint::Commit {
                return Err(self.injected_error(ErrorCode::OutcomeUnclear));
            }
            Ok(())
        }

        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename_noreplace(from, to)?;
            self.rename_completed.store(true, Ordering::SeqCst);
            if self.failure == FailurePoint::Commit {
                return Err(self.injected_error(ErrorCode::OutcomeUnclear));
            }
            Ok(())
        }
    }

    #[test]
    fn create_bewahrt_bestand_und_replace_committet_vollstaendig() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("zustand.json");
        atomic_write_local(
            &path,
            b"erste-version",
            ".atomic-test-",
            AtomicWriteMode::Create,
        )
        .unwrap();
        assert!(atomic_write_local(
            &path,
            b"darf-nicht-gewinnen",
            ".atomic-test-",
            AtomicWriteMode::Create,
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"erste-version");

        atomic_write_local(
            &path,
            b"zweite-version",
            ".atomic-test-",
            AtomicWriteMode::Replace,
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"zweite-version");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let recovery_temps = std::fs::read_dir(temp.path())
            .unwrap()
            .filter(|entry| {
                entry.as_ref().is_ok_and(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".atomic-test-")
                })
            })
            .count();
        assert_eq!(
            recovery_temps, 1,
            "Der Tempname des abgelehnten Create besitzt keinen atomaren Entfernungsbeleg und bleibt sichtbar"
        );
    }

    #[test]
    fn syncfehler_nach_rename_entfernt_keinen_fremden_eintrag_am_tempnamen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let path = root.join("zustand.json");
        let vfs = AtomicWriteFaultVfs::new(
            FailurePoint::DirectorySync,
            root.join("vorheriger-pruefpfad"),
        );

        let error = atomic_write_vfs(
            &vfs,
            &path,
            b"neuer-zustand",
            ".atomic-race-",
            AtomicWriteMode::Replace,
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(&path).unwrap(), b"neuer-zustand");
        let foreign_temp = vfs.temp.lock().unwrap().clone().unwrap();
        assert_eq!(std::fs::read(foreign_temp).unwrap(), b"fremd");
    }

    #[test]
    fn atomare_zustandsfehler_erhalten_alle_strukturierten_pruefpfade() {
        let mut failures = Vec::new();
        for (label, failure) in [
            ("create", FailurePoint::Create),
            ("prepare", FailurePoint::Prepare),
            ("commit", FailurePoint::Commit),
            ("directory-sync", FailurePoint::DirectorySync),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let path = root.join("zustand.json");
            let prior = root.join("vorheriger-datenrest");
            std::fs::write(&prior, b"erhalten").unwrap();
            let vfs = AtomicWriteFaultVfs::new(failure, prior.clone());
            let error = atomic_write_vfs(
                &vfs,
                &path,
                b"neu",
                ".atomic-recovery-",
                AtomicWriteMode::Replace,
            )
            .unwrap_err();
            let paths = crate::error::recovery_paths(&error);
            let retained_temp = vfs.temp.lock().unwrap().clone().unwrap();
            let committed = matches!(failure, FailurePoint::Commit | FailurePoint::DirectorySync);
            let expected_code = if failure == FailurePoint::Prepare {
                ErrorCode::PermissionDenied
            } else {
                ErrorCode::OutcomeUnclear
            };
            assert_eq!(error_code(&error), expected_code, "{label}");
            assert_eq!(std::fs::read(&prior).unwrap(), b"erhalten");
            if committed {
                assert_eq!(std::fs::read(&path).unwrap(), b"neu");
            } else {
                assert!(!path.exists());
                assert!(retained_temp.exists());
            }
            let complete = paths.contains(&prior.to_string_lossy().into_owned())
                && paths.contains(&retained_temp.to_string_lossy().into_owned())
                && (!committed || paths.contains(&path.to_string_lossy().into_owned()));
            if !complete {
                failures.push((label, paths));
            }
        }
        assert!(
            failures.is_empty(),
            "Verlorene strukturierte Prüfpfade: {failures:?}"
        );
    }
}
