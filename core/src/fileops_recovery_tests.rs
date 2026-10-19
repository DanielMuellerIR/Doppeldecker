use super::*;
use crate::vfs::{CompleteDirectoryListing, Entry};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug)]
enum Fault {
    Reclaim,
    Listing,
    Incomplete,
    Ambiguous,
    Size,
    Hash,
    Read,
    DirectorySync,
}

struct CommitFaultVfs {
    inner: crate::LocalVfs,
    fault: Fault,
    renamed: Mutex<Option<(PathBuf, PathBuf)>>,
    mutations: AtomicUsize,
    syncs: AtomicUsize,
}

impl CommitFaultVfs {
    fn error(code: ErrorCode, path: &str) -> io::Error {
        with_recovery_context(
            coded_error(io::ErrorKind::Other, code, "injizierter Providerfehler"),
            &[],
            &[Path::new(path)],
        )
    }

    fn commit(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.mutations.fetch_add(1, Ordering::SeqCst);
        self.inner.rename(from, to)?;
        *self.renamed.lock().unwrap() = Some((from.into(), to.into()));
        if matches!(
            self.fault,
            Fault::Reclaim | Fault::Listing | Fault::Incomplete
        ) {
            return Err(Self::error(ErrorCode::ConnectionLost, "rename-recovery"));
        }
        Ok(())
    }
}

impl Vfs for CommitFaultVfs {
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
    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.inner.open_write(path)
    }
    fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.inner.open_write_new(path)
    }
    fn sync_file(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_file(path)
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
        self.inner.rename(from, to)
    }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename_noreplace(from, to)
    }
    fn rename_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.commit(from, to)
    }
    fn rename_replace_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.commit(from, to)
    }
    fn file_commit_class(&self) -> FileCommitClass {
        if matches!(self.fault, Fault::DirectorySync) {
            FileCommitClass::Assured
        } else {
            FileCommitClass::BestEffort
        }
    }
    fn file_replace_class(&self) -> FileReplaceClass {
        FileReplaceClass::BestEffort
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        // Parken des Altordners ist dauerhaft; erst das zweite Rename scheitert am Sync.
        if self.syncs.fetch_add(1, Ordering::SeqCst) == 1
            && matches!(self.fault, Fault::DirectorySync)
        {
            return Err(Self::error(ErrorCode::Io, "sync-recovery"));
        }
        self.inner.sync_dir(path)
    }
    fn reclaim_verification_session(&self) -> io::Result<()> {
        if matches!(self.fault, Fault::Reclaim) {
            Err(Self::error(ErrorCode::ConnectionLost, "reclaim-recovery"))
        } else {
            Ok(())
        }
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        if self
            .renamed
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(_, target)| target == path)
        {
            match self.fault {
                Fault::Hash => return Ok(Box::new(std::io::Cursor::new(b"fremd!"))),
                Fault::Read => return Err(Self::error(ErrorCode::Io, "read-recovery")),
                _ => {}
            }
        }
        self.inner.open_read(path)
    }
    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        let mut listing = self.inner.list_complete(path)?;
        if let Some((_, target)) = self.renamed.lock().unwrap().as_ref() {
            assert_eq!(target.parent(), Some(path));
            match self.fault {
                Fault::Listing => return Err(Self::error(ErrorCode::Io, "listing-recovery")),
                Fault::Incomplete => listing.complete = false,
                Fault::Ambiguous => listing.entries.push(listing.entries[0].clone()),
                Fault::Size => listing.entries[0].entry.size += 1,
                _ => {}
            }
        }
        Ok(listing)
    }
}

fn fixture(fault: Fault) -> CommitFaultVfs {
    CommitFaultVfs {
        inner: crate::LocalVfs::new(),
        fault,
        renamed: Mutex::new(None),
        mutations: AtomicUsize::new(0),
        syncs: AtomicUsize::new(0),
    }
}

#[test]
fn best_effort_fehler_erhalten_phasengerechte_pfade_und_providerbelege() {
    for replace in [false, true] {
        for fault in [
            Fault::Reclaim,
            Fault::Listing,
            Fault::Incomplete,
            Fault::Ambiguous,
            Fault::Size,
            Fault::Hash,
            Fault::Read,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let source = root.join("datei");
            let destination = root.join("ziel");
            let target = destination.join("datei");
            std::fs::write(&source, b"Inhalt").unwrap();
            std::fs::create_dir(&destination).unwrap();
            if replace {
                std::fs::write(&target, b"Altbestand").unwrap();
            }
            let vfs = fixture(fault);
            let mut actuals = FileOperationActuals::default();
            let error = copy_items_inner(
                FileOperationRequest {
                    src: &vfs.inner,
                    dst: &vfs,
                    items: std::slice::from_ref(&source),
                    dst_dir: &destination,
                    policy: OverwritePolicy::Overwrite,
                    sandbox: &None,
                },
                FileTransferBindings::default(),
                &Cancel::new(),
                &mut |_| {},
                &mut actuals,
            )
            .unwrap_err();
            assert_eq!(
                error_code(&error),
                ErrorCode::OutcomeUnclear,
                "{replace}/{fault:?}: {error}"
            );
            assert_eq!(vfs.mutations.load(Ordering::SeqCst), 1);
            assert_eq!(std::fs::read(&target).unwrap(), b"Inhalt");
            assert_eq!(std::fs::read(&source).unwrap(), b"Inhalt");
            let paths = crate::error::recovery_paths(&error);
            assert!(
                paths.contains(&target.to_string_lossy().into_owned()),
                "{replace}/{fault:?}: {paths:?}"
            );
            let renamed = vfs.renamed.lock().unwrap();
            let temp_path = &renamed.as_ref().unwrap().0;
            assert!(!temp_path.exists());
            let temp_unclear = matches!(
                fault,
                Fault::Reclaim | Fault::Listing | Fault::Incomplete | Fault::Ambiguous
            );
            assert_eq!(
                paths.contains(&temp_path.to_string_lossy().into_owned()),
                temp_unclear,
                "{replace}/{fault:?}: {paths:?}"
            );
            let provider_paths: &[&str] = match fault {
                Fault::Reclaim => &["rename-recovery", "reclaim-recovery"],
                Fault::Listing => &["rename-recovery", "listing-recovery"],
                Fault::Incomplete => &["rename-recovery"],
                Fault::Read => &["read-recovery"],
                _ => &[],
            };
            for path in provider_paths {
                assert!(
                    paths.contains(&(*path).to_owned()),
                    "{replace}/{fault:?}: {paths:?}"
                );
            }
            assert_eq!(actuals.temp_recovery_paths, paths);
        }
    }
}

#[test]
fn datei_ersetzt_ordner_syncfehler_bilanziert_ziel_und_altbestand() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let source = root.join("datei");
    let destination = root.join("ziel");
    let target = destination.join("datei");
    std::fs::write(&source, b"Inhalt").unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("alt"), b"Altbestand").unwrap();
    let vfs = fixture(Fault::DirectorySync);
    let mut actuals = FileOperationActuals::default();
    let error = copy_items_inner(
        FileOperationRequest {
            src: &vfs.inner,
            dst: &vfs,
            items: std::slice::from_ref(&source),
            dst_dir: &destination,
            policy: OverwritePolicy::Overwrite,
            sandbox: &None,
        },
        FileTransferBindings::default(),
        &Cancel::new(),
        &mut |_| {},
        &mut actuals,
    )
    .unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    assert_eq!(std::fs::read(&target).unwrap(), b"Inhalt");
    let backup = std::fs::read_dir(&destination)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    assert_eq!(std::fs::read(backup.join("alt")).unwrap(), b"Altbestand");
    let mut expected = vec![
        target.to_string_lossy().into_owned(),
        backup.to_string_lossy().into_owned(),
        "sync-recovery".into(),
    ];
    expected.sort();
    let mut paths = crate::error::recovery_paths(&error);
    paths.sort();
    assert_eq!(paths, expected);
    actuals.temp_recovery_paths.sort();
    assert_eq!(actuals.temp_recovery_paths, expected);
    assert_eq!(std::fs::read_dir(destination).unwrap().count(), 2);
}

#[test]
fn beide_behalten_move_zwischen_vfs_erhaelt_quelle_bei_abbruch_und_spaeter_kindkollision() {
    for scenario in ["success", "cancel", "collision"] {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle/Projekt");
        let destination = root.join("ziel");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(destination.join("Projekt")).unwrap();
        std::fs::write(source.join("a.txt"), b"Quelle A").unwrap();
        std::fs::write(source.join("b.txt"), b"Quelle B").unwrap();
        std::fs::write(destination.join("Projekt/a.txt"), b"Ziel alt").unwrap();
        let src = crate::LocalVfs::new();
        let dst = crate::LocalVfs::new();
        let items = vec![source.clone()];
        let conflicts = find_recursive_conflicts(&src, &dst, &destination, &items).unwrap();
        let targets =
            prepare_keep_both_targets(&src, &dst, &destination, &items, &conflicts).unwrap();
        let assigned = PathBuf::from(&targets[0].target);
        let cancel = Cancel::new();
        let mut foreign = None;
        let result = move_items_with_targets_tracked(
            FileOperationRequest {
                src: &src,
                dst: &dst,
                items: &items,
                dst_dir: &destination,
                policy: OverwritePolicy::Skip,
                sandbox: &Some(root),
            },
            FileTransferBindings {
                targets: &targets,
                replacements: Some(&[]),
                skipped_targets: None,
            },
            false,
            &cancel,
            &mut |progress| {
                if scenario == "cancel" && progress.files_done >= 1 {
                    cancel.cancel();
                }
                if scenario == "collision"
                    && foreign.is_none()
                    && assigned.is_dir()
                    && ["a.txt", "b.txt"].contains(&progress.current.as_str())
                {
                    let name = if progress.current == "a.txt" {
                        "b.txt"
                    } else {
                        "a.txt"
                    };
                    let path = assigned.join(name);
                    std::fs::write(&path, b"Fremd").unwrap();
                    foreign = Some(path);
                }
            },
            &mut FileOperationActuals::default(),
        );
        match scenario {
            "success" => {
                let stats = result.unwrap();
                assert_eq!(stats.files, 2);
                assert!(!stats.cancelled);
                assert!(!source.exists());
                assert_eq!(std::fs::read(assigned.join("a.txt")).unwrap(), b"Quelle A");
                assert_eq!(std::fs::read(assigned.join("b.txt")).unwrap(), b"Quelle B");
            }
            "cancel" => {
                assert!(cancel.is_cancelled());
                let error = result.unwrap_err();
                assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
                assert!(crate::error::recovery_paths(&error)
                    .contains(&assigned.to_string_lossy().into_owned()));
                assert_eq!(std::fs::read_dir(&assigned).unwrap().count(), 1);
            }
            "collision" => {
                assert_eq!(error_code(&result.unwrap_err()), ErrorCode::OutcomeUnclear);
                assert_eq!(std::fs::read(foreign.unwrap()).unwrap(), b"Fremd");
            }
            _ => unreachable!(),
        }
        if scenario != "success" {
            assert_eq!(std::fs::read(source.join("a.txt")).unwrap(), b"Quelle A");
            assert_eq!(std::fs::read(source.join("b.txt")).unwrap(), b"Quelle B");
        }
        assert_eq!(
            std::fs::read(destination.join("Projekt/a.txt")).unwrap(),
            b"Ziel alt"
        );
    }
}
