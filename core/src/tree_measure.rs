//! Gemeinsame, konservative Bilanz eines bereits gelisteten VFS-Unterbaums.

use std::io;
use std::path::Path;

use crate::vfs::{Entry, Vfs};

/// Zählt einen bereits gelisteten Eintrag einschließlich aller Kinder und summiert
/// ausschließlich reguläre Dateibytes. Symlinks und Spezialdateien zählen als Name,
/// werden aber nie betreten und tragen keine Nutzdatenbytes bei.
///
/// Verschwindet ein gelisteter Ordner vor seiner Rekursion, wird der Listenfehler
/// absichtlich weitergereicht. Ein Drift darf weder Mirror-Grenzen noch eine
/// Quarantänebilanz künstlich verkleinern.
pub(crate) fn measure_entry(vfs: &dyn Vfs, path: &Path, entry: &Entry) -> io::Result<(u64, u64)> {
    if entry.is_symlink || !entry.is_dir {
        return Ok((1, if entry.is_file { entry.size } else { 0 }));
    }
    let mut count = 1_u64;
    let mut bytes = 0_u64;
    let listing = vfs.list_complete(path)?;
    crate::real_mode::require_complete_safe_listing(&listing, "Rekursive Mengenbilanz")?;
    for child in listing.entries {
        let child = child.entry;
        let (child_count, child_bytes) = measure_entry(vfs, &path.join(&child.name), &child)?;
        count = count.saturating_add(child_count);
        bytes = bytes.saturating_add(child_bytes);
    }
    Ok((count, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{CompleteDirectoryListing, LocalVfs};
    use std::io::{Read, Write};

    /// Simuliert einen Provider, dessen bequemes `list()` Einträge liefert, der den
    /// vollständigen Listingvertrag aber ausdrücklich nicht bestätigen kann.
    struct IncompleteListingVfs {
        inner: LocalVfs,
    }

    impl Vfs for IncompleteListingVfs {
        fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
            self.inner.list(path)
        }

        fn list_complete(&self, _path: &Path) -> io::Result<CompleteDirectoryListing> {
            Ok(CompleteDirectoryListing {
                entries: Vec::new(),
                provider_token: None,
                complete: false,
            })
        }

        fn stat(&self, path: &Path) -> io::Result<Entry> {
            self.inner.stat(path)
        }

        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }

        fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
            unreachable!("rein lesender Messtest")
        }

        fn create_dir(&self, _path: &Path) -> io::Result<()> {
            unreachable!("rein lesender Messtest")
        }

        fn open_read(&self, _path: &Path) -> io::Result<Box<dyn Read + Send>> {
            unreachable!("rein lesender Messtest")
        }

        fn open_write(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
            unreachable!("rein lesender Messtest")
        }

        fn open_write_new(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
            unreachable!("rein lesender Messtest")
        }

        fn sync_file(&self, _path: &Path) -> io::Result<()> {
            unreachable!("rein lesender Messtest")
        }

        fn sync_dir(&self, _path: &Path) -> io::Result<()> {
            unreachable!("rein lesender Messtest")
        }

        fn set_mtime(&self, _path: &Path, _mtime: i64) -> io::Result<()> {
            unreachable!("rein lesender Messtest")
        }

        fn set_mode(&self, _path: &Path, _mode: u32) -> io::Result<()> {
            unreachable!("rein lesender Messtest")
        }

        fn remove_file(&self, _path: &Path) -> io::Result<()> {
            unreachable!("rein lesender Messtest")
        }

        fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
            unreachable!("rein lesender Messtest")
        }

        fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            unreachable!("rein lesender Messtest")
        }

        fn rename_noreplace(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            unreachable!("rein lesender Messtest")
        }
    }

    #[cfg(unix)]
    #[test]
    fn zaehlt_unterbaum_ohne_symlink_zu_betreten() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let tree = std::fs::canonicalize(temp.path()).unwrap().join("baum");
        std::fs::create_dir_all(tree.join("unter")).unwrap();
        std::fs::write(tree.join("a"), b"abc").unwrap();
        std::fs::write(tree.join("unter/b"), b"defg").unwrap();
        symlink(tree.join("unter"), tree.join("link")).unwrap();

        let vfs = LocalVfs::new();
        let entry = vfs.stat(&tree).unwrap();
        assert_eq!(measure_entry(&vfs, &tree, &entry).unwrap(), (5, 7));
    }

    #[test]
    fn verschwundener_gelisteter_ordner_bleibt_fehler() {
        let temp = tempfile::tempdir().unwrap();
        let tree = std::fs::canonicalize(temp.path()).unwrap().join("baum");
        std::fs::create_dir(&tree).unwrap();
        let vfs = LocalVfs::new();
        let entry = vfs.stat(&tree).unwrap();
        std::fs::remove_dir(&tree).unwrap();

        assert_eq!(
            measure_entry(&vfs, &tree, &entry).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn unvollstaendiges_providerlisting_darf_mirrorbilanz_nicht_verkleinern() {
        let temp = tempfile::tempdir().unwrap();
        let tree = std::fs::canonicalize(temp.path()).unwrap().join("baum");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("nicht-bilanzierte-datei"), b"inhalt").unwrap();
        let vfs = IncompleteListingVfs {
            inner: LocalVfs::new(),
        };
        let entry = vfs.stat(&tree).unwrap();

        assert_eq!(
            measure_entry(&vfs, &tree, &entry).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
