//! Tests für die Sync-Engine — der korrektheitskritische Kern (kann Dateien löschen!).
//!
//! Läuft komplett headless über echte temporäre Verzeichnisse → identisch auf
//! macOS und Linux ausführbar (`cargo test -p doppeldecker-core`).

use std::fs;
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::io::{Seek, SeekFrom};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use doppeldecker_core::{
    execute, list_quarantine_entries_for_target, list_quarantine_runs, plan,
    restore_quarantine_entry_for_target, validate_local_sync_roots, Acl, AclModel, Action, Cancel,
    Compare, ComparisonAction, ComparisonEntryKind, ComparisonSide, ExecuteRequest, ForkKind,
    LocalVfs, MetadataCapabilities, MetadataManifest, MetadataProfile, Owner, Progress, Safety,
    SyncMode, SyncOptions, Vfs, XattrValue, TEST_SANDBOX_MARKER, TEST_SANDBOX_MARKER_CONTENT,
};
#[cfg(target_os = "macos")]
use doppeldecker_core::{Nfs4AceKind, Nfs4Acl, Nfs4AclEntry};
#[cfg(target_os = "linux")]
use doppeldecker_core::{PosixAcl, PosixAclEntry, PosixAclTag};

// macOS stellt `/var` als Symlink auf `/private/var` bereit. Produktionspfade werden
// an der App-Grenze kanonisiert; der Wrapper hält die Testpfade unter derselben
// descriptor-relativ begehbaren Wurzel.
mod canonical_tempfile {
    pub fn tempdir() -> std::io::Result<::tempfile::TempDir> {
        let root = std::fs::canonicalize(std::env::temp_dir())?;
        ::tempfile::tempdir_in(root)
    }
}
use canonical_tempfile as tempfile;

/// Echte Ausführung ohne Sandbox/Dry-Run (Standard für die meisten Tests).
fn real() -> Safety {
    Safety::headless_test()
}

/// Nur Tests, die das explizite Mirror-Löschverhalten selbst prüfen, dürfen die neuen
/// Mengen-Gates gezielt öffnen. Produktionscode setzt diese Felder niemals so.
fn real_with_explicit_delete_permission() -> Safety {
    let mut safety = Safety::headless_test();
    safety.max_delete_entries = None;
    safety.max_delete_fraction = None;
    safety.allow_empty_source_mirror = true;
    safety
}

// --- kleine Helfer ---------------------------------------------------------

/// Datei mit Inhalt anlegen (inkl. Elternverzeichnisse).
fn write_file(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, content).unwrap();
}

/// Test-Sandbox exakt wie der Launcher markieren, Root-Handle öffnen und das daran
/// gebundene Kern-Permit holen.
fn marked_sandbox(root: &Path) -> (LocalVfs, Safety) {
    fs::write(root.join(TEST_SANDBOX_MARKER), TEST_SANDBOX_MARKER_CONTENT).unwrap();
    let vfs = LocalVfs::rooted(root).unwrap();
    let safety = Safety::test_sandbox(root.to_path_buf(), &vfs).unwrap();
    (vfs, safety)
}

/// mtime einer Datei/eines Ordners auf eine feste Unix-Sekunde setzen.
fn set_mtime(root: &Path, rel: &str, secs: i64) {
    let ft = filetime::FileTime::from_unix_time(secs, 0);
    filetime::set_file_mtime(root.join(rel), ft).unwrap();
}

/// Inhalt einer Datei lesen.
fn read_file(root: &Path, rel: &str) -> Vec<u8> {
    fs::read(root.join(rel)).unwrap()
}

/// Nur die portablen POSIX-Rechtebits lesen/setzen; Dateityp-Bits gehören nicht zum
/// Metadatenvertrag und unterscheiden sich zwischen Datei und Ordner.
fn set_mode(root: &Path, rel: &str, mode: u32) {
    fs::set_permissions(root.join(rel), fs::Permissions::from_mode(mode)).unwrap();
}

fn mode(root: &Path, rel: &str) -> u32 {
    fs::metadata(root.join(rel)).unwrap().permissions().mode() & 0o777
}

fn full_mode(root: &Path, rel: &str) -> u32 {
    fs::metadata(root.join(rel)).unwrap().permissions().mode() & 0o7777
}

/// Einen echten Lochbereich erzeugen; bloßes Seek beziehungsweise `set_len` darf auf
/// beiden Plattformen konservativ allokieren und wäre deshalb kein belastbarer Test.
fn punch_hole(file: &fs::File, offset: u64, length: u64) {
    #[cfg(target_os = "linux")]
    rustix::fs::fallocate(
        file,
        rustix::fs::FallocateFlags::PUNCH_HOLE | rustix::fs::FallocateFlags::KEEP_SIZE,
        offset,
        length,
    )
    .unwrap();
    #[cfg(target_os = "macos")]
    {
        let mut hole = libc::fpunchhole_t {
            fp_flags: 0,
            reserved: 0,
            fp_offset: offset as i64,
            fp_length: length as i64,
        };
        let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &mut hole) };
        assert_eq!(
            result,
            0,
            "F_PUNCHHOLE: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Fortschritts-Callback, der nichts tut (für Tests ohne Fortschritts-Prüfung).
fn ignore_progress(_p: &Progress) {}

/// Lokaler Zieladapter für gezielte Ownerfehler und Capabilityabweichungen.
/// Reale Datenwirkungen prüfen sowohl den atomaren Fehlerpfad als auch sichtbar
/// ausgelassene Metadaten ohne fremde UID oder Nutzerdaten.
struct MetadataTestVfs {
    inner: LocalVfs,
    unknown_capabilities: bool,
    incompatible_acl_model: bool,
    missing_hardlinks: bool,
    missing_native_metadata: bool,
    fail_owner: bool,
    fail_sync: bool,
    ignore_mode_write: bool,
}

impl MetadataTestVfs {
    fn new() -> Self {
        Self {
            inner: LocalVfs::new(),
            unknown_capabilities: false,
            incompatible_acl_model: false,
            missing_hardlinks: false,
            missing_native_metadata: false,
            fail_owner: true,
            fail_sync: false,
            ignore_mode_write: false,
        }
    }

    fn without_hardlinks() -> Self {
        Self {
            missing_hardlinks: true,
            fail_owner: false,
            ..Self::new()
        }
    }

    fn without_native_metadata() -> Self {
        Self {
            missing_native_metadata: true,
            fail_owner: false,
            ..Self::new()
        }
    }

    fn with_unknown_capabilities() -> Self {
        Self {
            unknown_capabilities: true,
            ..Self::new()
        }
    }

    fn with_incompatible_acl_model() -> Self {
        Self {
            incompatible_acl_model: true,
            ..Self::new()
        }
    }
}

impl Vfs for MetadataTestVfs {
    fn list(&self, path: &Path) -> std::io::Result<Vec<doppeldecker_core::Entry>> {
        self.inner.list(path)
    }

    fn list_complete(
        &self,
        path: &Path,
    ) -> std::io::Result<doppeldecker_core::vfs::CompleteDirectoryListing> {
        self.inner.list_complete(path)
    }

    fn stat(&self, path: &Path) -> std::io::Result<doppeldecker_core::Entry> {
        self.inner.stat(path)
    }

    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.inner.create_dir_all(path)
    }

    fn create_dir(&self, path: &Path) -> std::io::Result<()> {
        self.inner.create_dir(path)
    }

    fn open_read(&self, path: &Path) -> std::io::Result<Box<dyn Read + Send>> {
        self.inner.open_read(path)
    }

    fn open_write(&self, path: &Path) -> std::io::Result<Box<dyn Write + Send>> {
        self.inner.open_write(path)
    }

    fn open_write_new(&self, path: &Path) -> std::io::Result<Box<dyn Write + Send>> {
        self.inner.open_write_new(path)
    }

    fn open_sparse_read(
        &self,
        path: &Path,
    ) -> std::io::Result<Box<dyn doppeldecker_core::vfs::SparseRead>> {
        self.inner.open_sparse_read(path)
    }

    fn open_sparse_write_new(
        &self,
        path: &Path,
    ) -> std::io::Result<Box<dyn doppeldecker_core::vfs::SparseWrite>> {
        self.inner.open_sparse_write_new(path)
    }

    fn sync_file(&self, path: &Path) -> std::io::Result<()> {
        if self.fail_sync {
            return Err(std::io::Error::other("erzwungener Metadaten-fsync-Fehler"));
        }
        self.inner.sync_file(path)
    }

    fn sync_dir(&self, path: &Path) -> std::io::Result<()> {
        if self.fail_sync {
            return Err(std::io::Error::other("erzwungener Metadaten-fsync-Fehler"));
        }
        self.inner.sync_dir(path)
    }

    fn set_mtime(&self, path: &Path, mtime: i64) -> std::io::Result<()> {
        self.inner.set_mtime(path, mtime)
    }

    fn set_mode(&self, path: &Path, mode: u32) -> std::io::Result<()> {
        if self.missing_native_metadata {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Ziel besitzt keine nativen Modusbits",
            ));
        }
        if self.ignore_mode_write {
            return Ok(());
        }
        self.inner.set_mode(path, mode)
    }

    fn set_metadata_owner(&self, path: &Path, owner: Owner) -> std::io::Result<()> {
        if !self.fail_owner {
            return self.inner.set_metadata_owner(path, owner);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "injizierter fchown-Fehler",
        ))
    }

    fn set_metadata_normal_mode(&self, path: &Path, mode: u32) -> std::io::Result<()> {
        self.inner.set_metadata_normal_mode(path, mode)
    }

    fn set_metadata_special_bits(&self, path: &Path, bits: u32) -> std::io::Result<()> {
        self.inner.set_metadata_special_bits(path, bits)
    }

    fn metadata_capabilities(&self, root: &Path) -> std::io::Result<MetadataCapabilities> {
        if self.missing_native_metadata {
            Ok(MetadataCapabilities {
                owner: doppeldecker_core::Capability::Unsupported,
                special_mode_bits: doppeldecker_core::Capability::Unsupported,
                acl_model: AclModel::Unsupported,
                xattrs: doppeldecker_core::Capability::Unsupported,
                resource_fork: doppeldecker_core::Capability::Unsupported,
                sparse_extents: doppeldecker_core::Capability::Unsupported,
                hardlinks: doppeldecker_core::Capability::Unsupported,
            })
        } else if self.unknown_capabilities {
            Ok(MetadataCapabilities::unknown())
        } else {
            let mut capabilities = self.inner.metadata_capabilities(root)?;
            if self.missing_hardlinks {
                capabilities.hardlinks = doppeldecker_core::Capability::Unsupported;
            }
            if self.incompatible_acl_model {
                capabilities.acl_model = if cfg!(target_os = "linux") {
                    AclModel::Nfs4
                } else {
                    AclModel::Posix
                };
            }
            Ok(capabilities)
        }
    }

    fn metadata_manifest(&self, path: &Path) -> std::io::Result<MetadataManifest> {
        self.inner.metadata_manifest(path)
    }

    fn write_acl_exact(&self, path: &Path, acl: Option<&Acl>) -> std::io::Result<()> {
        self.inner.write_acl_exact(path, acl)
    }

    fn read_xattrs(&self, path: &Path) -> std::io::Result<Vec<XattrValue>> {
        self.inner.read_xattrs(path)
    }

    fn write_xattrs_exact(&self, path: &Path, xattrs: &[XattrValue]) -> std::io::Result<()> {
        self.inner.write_xattrs_exact(path, xattrs)
    }

    fn open_fork_read(&self, path: &Path, kind: ForkKind) -> std::io::Result<Box<dyn Read + Send>> {
        self.inner.open_fork_read(path, kind)
    }

    fn open_fork_write_new(
        &self,
        path: &Path,
        kind: ForkKind,
    ) -> std::io::Result<Box<dyn Write + Send>> {
        self.inner.open_fork_write_new(path, kind)
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.inner.remove_file(path)
    }

    fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.inner.remove_dir_all(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename_noreplace(from, to)
    }
}

/// Anzahl Aktionen eines bestimmten Typs zählen.
fn count(actions: &[Action], pred: impl Fn(&Action) -> bool) -> usize {
    actions.iter().filter(|a| pred(a)).count()
}

/// Standardoptionen mit gegebenem Modus.
/// Zählt Ziel-`stat`-Aufrufe des Planners. Belegt die Round-Trip-Reduktion:
/// Unterhalb der Wurzel kennt der Planner jeden Ziel-Ordner bereits aus dem
/// Eltern-Listing — über ein Netzwerk wäre jeder vermiedene `stat` ein voller
/// Round-Trip pro Verzeichnis.
struct CountingVfs {
    inner: LocalVfs,
    stat_calls: std::sync::atomic::AtomicU64,
    incomplete_listings: std::sync::atomic::AtomicBool,
    incomplete_listing_path: std::sync::Mutex<Option<std::path::PathBuf>>,
    stat_failure_path: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// Stellt einen Remote-Provider nach: keine schreibbaren Rechte, und wie SMB,
    /// WebDAV und FTPS liefert das Listing gar keinen Modus.
    no_mode_write: std::sync::atomic::AtomicBool,
}

impl CountingVfs {
    fn new() -> Self {
        Self {
            inner: LocalVfs::new(),
            stat_calls: std::sync::atomic::AtomicU64::new(0),
            incomplete_listings: std::sync::atomic::AtomicBool::new(false),
            incomplete_listing_path: std::sync::Mutex::new(None),
            stat_failure_path: std::sync::Mutex::new(None),
            no_mode_write: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn without_mode_write() -> Self {
        let value = Self::new();
        value
            .no_mode_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
        value
    }

    fn hides_mode(&self) -> bool {
        self.no_mode_write.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn with_incomplete_listings() -> Self {
        let value = Self::new();
        value
            .incomplete_listings
            .store(true, std::sync::atomic::Ordering::SeqCst);
        value
    }

    fn fail_stat_for(&self, path: &Path) {
        *self.stat_failure_path.lock().unwrap() = Some(path.to_path_buf());
    }

    fn mark_listing_incomplete_at(&self, path: &Path) {
        *self.incomplete_listing_path.lock().unwrap() = Some(path.to_path_buf());
    }

    fn stats(&self) -> u64 {
        self.stat_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Vfs for CountingVfs {
    fn supports_mode_write(&self) -> bool {
        !self.hides_mode()
    }
    fn list(&self, path: &Path) -> std::io::Result<Vec<doppeldecker_core::Entry>> {
        let mut entries = self.inner.list(path)?;
        if self.hides_mode() {
            for entry in &mut entries {
                entry.mode = None;
            }
        }
        Ok(entries)
    }
    fn list_complete(
        &self,
        path: &Path,
    ) -> std::io::Result<doppeldecker_core::vfs::CompleteDirectoryListing> {
        let mut listing = self.inner.list_complete(path)?;
        if self.hides_mode() {
            for item in &mut listing.entries {
                item.entry.mode = None;
            }
        }
        if self
            .incomplete_listings
            .load(std::sync::atomic::Ordering::SeqCst)
            || self.incomplete_listing_path.lock().unwrap().as_deref() == Some(path)
        {
            listing.complete = false;
        }
        Ok(listing)
    }
    fn stat(&self, path: &Path) -> std::io::Result<doppeldecker_core::Entry> {
        self.stat_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.stat_failure_path.lock().unwrap().as_deref() == Some(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injizierter stat-Fehler",
            ));
        }
        let mut entry = self.inner.stat(path)?;
        if self.hides_mode() {
            entry.mode = None;
        }
        Ok(entry)
    }
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn create_dir(&self, path: &Path) -> std::io::Result<()> {
        self.inner.create_dir(path)
    }
    fn open_read(&self, path: &Path) -> std::io::Result<Box<dyn Read + Send>> {
        self.inner.open_read(path)
    }
    fn open_write(&self, path: &Path) -> std::io::Result<Box<dyn Write + Send>> {
        self.inner.open_write(path)
    }
    fn open_write_new(&self, path: &Path) -> std::io::Result<Box<dyn Write + Send>> {
        self.inner.open_write_new(path)
    }
    fn sync_file(&self, path: &Path) -> std::io::Result<()> {
        self.inner.sync_file(path)
    }
    fn sync_dir(&self, path: &Path) -> std::io::Result<()> {
        self.inner.sync_dir(path)
    }
    fn set_mtime(&self, path: &Path, mtime: i64) -> std::io::Result<()> {
        self.inner.set_mtime(path, mtime)
    }
    fn set_mode(&self, path: &Path, mode: u32) -> std::io::Result<()> {
        self.inner.set_mode(path, mode)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.inner.remove_dir_all(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename_noreplace(from, to)
    }
}

fn opts(mode: SyncMode) -> SyncOptions {
    SyncOptions {
        mode,
        ..Default::default()
    }
}

fn hash_opts(mode: SyncMode) -> SyncOptions {
    SyncOptions {
        mode,
        compare: Compare::ContentHash,
        ..Default::default()
    }
}

fn native_best_effort_opts(mode: SyncMode) -> SyncOptions {
    SyncOptions {
        mode,
        compare: Compare::ContentHash,
        metadata: MetadataProfile::NativeBestEffort,
        ..Default::default()
    }
}

#[cfg(target_os = "linux")]
fn native_test_acl() -> Acl {
    Acl::Posix(PosixAcl {
        access: vec![
            PosixAclEntry {
                tag: PosixAclTag::UserObject,
                id: None,
                permissions: 6,
            },
            PosixAclEntry {
                tag: PosixAclTag::User,
                id: Some(12345),
                permissions: 4,
            },
            PosixAclEntry {
                tag: PosixAclTag::GroupObject,
                id: None,
                permissions: 4,
            },
            PosixAclEntry {
                tag: PosixAclTag::Mask,
                id: None,
                permissions: 4,
            },
            PosixAclEntry {
                tag: PosixAclTag::Other,
                id: None,
                permissions: 0,
            },
        ],
        default: vec![],
    })
}

#[cfg(target_os = "macos")]
fn native_test_acl() -> Acl {
    unsafe extern "C" {
        fn mbr_uid_to_uuid(id: libc::uid_t, uu: *mut u8) -> libc::c_int;
    }
    let mut qualifier = [0u8; 16];
    assert_eq!(
        unsafe { mbr_uid_to_uuid(libc::geteuid(), qualifier.as_mut_ptr()) },
        0
    );
    Acl::Nfs4(Nfs4Acl {
        entries: vec![Nfs4AclEntry {
            qualifier,
            kind: Nfs4AceKind::Allow,
            permissions: 1 << 1,
            flags: 0,
        }],
        flags: 0,
    })
}

// --- Tests -----------------------------------------------------------------

#[test]
fn neue_datei_wird_kopiert() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"hallo");

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();

    assert_eq!(p.copy_count, 1);
    assert_eq!(p.update_count, 0);
    assert_eq!(p.delete_count, 0);
    assert_eq!(p.bytes_to_transfer, 5);

    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(read_file(dst.path(), "a.txt"), b"hallo");
}

#[test]
fn vergleichsdto_kommt_aus_demselben_baumvergleich_und_traegt_beide_seiten() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "Ordner/neu.txt", b"neu");
    write_file(src.path(), "aktuell.txt", b"quelle");
    write_file(src.path(), "konflikt/innen.txt", b"quelle");
    write_file(dst.path(), "aktuell.txt", b"ziel");
    write_file(dst.path(), "alt.txt", b"alt");
    write_file(dst.path(), "konflikt", b"datei");
    set_mtime(src.path(), "aktuell.txt", 200);
    set_mtime(dst.path(), "aktuell.txt", 100);

    let vfs = LocalVfs::new();
    let result = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();

    assert_eq!(result.actions.len(), result.comparison.rows.len());
    assert_eq!(result.comparison.categories.additions, 2);
    assert_eq!(result.comparison.categories.updates, 1);
    assert_eq!(result.comparison.categories.quarantines, 1);
    assert_eq!(result.comparison.categories.skipped, 1);
    assert_eq!(
        result
            .comparison
            .rows
            .iter()
            .map(|row| row.path.as_str())
            .collect::<Vec<_>>(),
        [
            "Ordner",
            "Ordner/neu.txt",
            "aktuell.txt",
            "alt.txt",
            "konflikt"
        ]
    );

    let nested = &result.comparison.rows[1];
    assert_eq!(nested.depth, 1);
    assert_eq!(nested.ancestors, ["Ordner"]);
    assert_eq!(nested.action, ComparisonAction::Add);
    assert_eq!(nested.affected_side, ComparisonSide::Target);
    assert_eq!(
        nested.source.as_ref().map(|entry| entry.kind),
        Some(ComparisonEntryKind::File)
    );
    assert!(nested.target.is_none());

    let update = &result.comparison.rows[2];
    assert_eq!(update.action, ComparisonAction::Update);
    assert_eq!(update.source.as_ref().map(|entry| entry.size), Some(6));
    assert_eq!(update.target.as_ref().map(|entry| entry.size), Some(4));

    let quarantine = &result.comparison.rows[3];
    assert_eq!(quarantine.action, ComparisonAction::Quarantine);
    assert!(quarantine.source.is_none());
    assert_eq!(
        quarantine.target.as_ref().map(|entry| entry.kind),
        Some(ComparisonEntryKind::File)
    );

    let conflict = &result.comparison.rows[4];
    assert_eq!(conflict.action, ComparisonAction::Skip);
    assert_eq!(conflict.affected_side, ComparisonSide::Neither);
    assert_eq!(
        conflict.source.as_ref().map(|entry| entry.kind),
        Some(ComparisonEntryKind::Directory)
    );
    assert_eq!(
        conflict.target.as_ref().map(|entry| entry.kind),
        Some(ComparisonEntryKind::File)
    );

    // Alte Queue-Journale besitzen diese später ergänzten Felder nicht. Die
    // Deserialisierung muss das Journal weiterhin laden können. Ein alter Plan ohne
    // Zielbeleg bleibt anschließend aber absichtlich unausführbar, statt eine
    // pfadbasierte Quarantänemutation zu erlauben.
    let mut serialized = serde_json::to_value(&result).unwrap();
    serialized.as_object_mut().unwrap().remove("comparison");
    fn remove_target_proofs(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Array(values) => {
                values.iter_mut().for_each(remove_target_proofs);
            }
            serde_json::Value::Object(fields) => {
                fields.remove("target_proof");
                fields.values_mut().for_each(remove_target_proofs);
            }
            _ => {}
        }
    }
    remove_target_proofs(&mut serialized);
    let restored: doppeldecker_core::SyncPlan = serde_json::from_value(serialized).unwrap();
    assert!(restored.comparison.rows.is_empty());
    let error = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &restored,
            safety: &Safety::default(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .expect_err("ein alter ungebundener Plan darf nur für die Recovery lesbar bleiben");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

/// Zwei VERSCHIEDENE Nicht-UTF-8-Quellnamen fallen beim verlustbehafteten Dekodieren
/// auf denselben String zusammen ("kollision-\u{FFFD}.txt"). Der Planner darf sie
/// weder verschmelzen noch kopieren, und ein verlustbehafteter Ziel-Name darf im
/// Mirror-Modus nie zum Löschen eingeplant werden — alle werden konservativ
/// ausgelassen. Nur Linux: APFS auf macOS lehnt ungültiges UTF-8 in Namen ab, dort
/// kann der Fall lokal gar nicht entstehen.
#[cfg(target_os = "linux")]
#[test]
fn nicht_utf8_namen_werden_konservativ_ausgelassen() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    // \xff und \xfe sind ungültiges UTF-8 → beide Namen dekodieren zu demselben
    // String mit Ersatzzeichen. Vor dem Fix verschmolzen sie in der Plan-Map.
    fs::write(
        src.path().join(OsStr::from_bytes(b"kollision-\xff.txt")),
        b"a",
    )
    .unwrap();
    fs::write(
        src.path().join(OsStr::from_bytes(b"kollision-\xfe.txt")),
        b"b",
    )
    .unwrap();
    fs::write(
        dst.path().join(OsStr::from_bytes(b"ueberzaehlig-\xfd.txt")),
        b"c",
    )
    .unwrap();

    let vfs = LocalVfs::new();
    let options = SyncOptions {
        // Selbst ein bewusst alles umfassendes Muster darf einen nicht eindeutig
        // adressierbaren Namen nicht in einen vermeintlich sicheren Ausschluss drehen.
        exclude: vec!["*".into()],
        ..opts(SyncMode::Mirror)
    };
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &options).unwrap();

    assert_eq!(p.copy_count, 0);
    assert_eq!(p.update_count, 0);
    assert_eq!(p.delete_count, 0);
    assert_eq!(p.skip_count, 3);
    assert_eq!(
        p.actions
            .iter()
            .filter(|a| matches!(a, Action::SkipUnsupported { .. }))
            .count(),
        3
    );
}

#[test]
fn ausschluesse_sind_beidseitig_unsichtbar_und_sperren_mirror_nicht() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    write_file(src.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "bleibt.txt", b"gleich");
    set_mtime(src.path(), "bleibt.txt", 1_700_000_000);
    set_mtime(dst.path(), "bleibt.txt", 1_700_000_000);
    write_file(src.path(), "neu.txt", b"sichtbar");

    // Gleichnamige ausgeschlossene Einträge dürfen weder aktualisiert noch als
    // Konflikt oder Skip sichtbar werden.
    write_file(src.path(), ".env", b"quelle-geheim");
    write_file(dst.path(), ".env", b"ziel-bleibt");
    write_file(src.path(), "cache/datei.bin", b"quelle-cache");
    write_file(dst.path(), "cache/datei.bin", b"ziel-cache");
    write_file(src.path(), "a/datei.tmp", b"quelle-temp");
    write_file(dst.path(), "a/datei.tmp", b"ziel-temp");
    write_file(src.path(), "build/tief/artefakt", b"kein build-output");

    // Der Ordner selbst ist nur im Ziel, enthält aber einen ausgeschlossenen
    // Nachfahren. Deshalb muss er stehen bleiben, während sein sichtbares Kind
    // weiterhin eine normale Mirror-Entfernung ist.
    write_file(dst.path(), "container/.env", b"behalten");
    write_file(dst.path(), "container/extra.txt", b"entfernen");
    write_file(dst.path(), "extra.txt", b"auch entfernen");

    let options = SyncOptions {
        mode: SyncMode::Mirror,
        exclude: vec![
            "cache".into(),
            "**/*.tmp".into(),
            "build/**".into(),
            ".env".into(),
        ],
        ..Default::default()
    };
    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &options).unwrap();

    assert_eq!(p.copy_count, 1);
    assert_eq!(p.update_count, 0);
    assert_eq!(p.delete_count, 2);
    assert_eq!(p.skip_count, 0);
    assert_eq!(
        p.source_count, 3,
        "bleibt.txt, neu.txt und der sichtbare Ordner a"
    );
    assert_eq!(p.target_count, 5, "ausgeschlossene Einträge zählen nicht");
    for hidden in [".env", "cache", ".tmp", "build"] {
        assert!(p
            .comparison
            .rows
            .iter()
            .all(|row| !row.path.contains(hidden)));
    }
    assert!(p.actions.iter().any(
        |action| matches!(action, Action::DeleteFile { path, .. } if path == "container/extra.txt")
    ));
    assert!(!p
        .actions
        .iter()
        .any(|action| matches!(action, Action::DeleteDir { path, .. } if path == "container")));

    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real_with_explicit_delete_permission(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(read_file(dst.path(), "neu.txt"), b"sichtbar");
    assert_eq!(read_file(dst.path(), ".env"), b"ziel-bleibt");
    assert_eq!(read_file(dst.path(), "cache/datei.bin"), b"ziel-cache");
    assert_eq!(read_file(dst.path(), "a/datei.tmp"), b"ziel-temp");
    assert_eq!(read_file(dst.path(), "container/.env"), b"behalten");
    assert!(!dst.path().join("container/extra.txt").exists());
    assert!(!dst.path().join("extra.txt").exists());
    assert!(!dst.path().join("build").exists());
}

#[test]
fn ungueltiges_ausschlussmuster_scheitert_vor_jedem_planerzugriff() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let vfs = CountingVfs::new();
    let options = SyncOptions {
        exclude: vec!["ordner/**rest".into()],
        ..opts(SyncMode::Mirror)
    };

    let error = plan(&vfs, src.path(), &vfs, dst.path(), &options).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(vfs.stats(), 0);
}

#[test]
fn negative_zeittoleranz_scheitert_vor_jedem_planerzugriff() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let vfs = CountingVfs::new();
    let options = SyncOptions {
        mtime_tolerance: -1,
        ..opts(SyncMode::Update)
    };

    let error = plan(&vfs, src.path(), &vfs, dst.path(), &options).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(vfs.stats(), 0);
}

#[test]
fn unvollstaendiges_listing_wird_nicht_als_vollstaendiger_plan_ausgegeben() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"inhalt");
    let source_vfs = LocalVfs::new();
    let target_vfs = CountingVfs::with_incomplete_listings();

    let error = plan(
        &source_vfs,
        src.path(),
        &target_vfs,
        dst.path(),
        &opts(SyncMode::AddOnly),
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("unvollständig"));
}

#[test]
fn unvollstaendiges_tiefenlisting_erzeugt_keinen_delete_dir_hash() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "extra/tief.txt", b"wichtig");
    set_mtime(src.path(), "bleibt.txt", 1_700_000_000);
    set_mtime(dst.path(), "bleibt.txt", 1_700_000_000);
    let source_vfs = LocalVfs::new();
    let target_vfs = CountingVfs::new();
    target_vfs.mark_listing_incomplete_at(&dst.path().join("extra"));

    let error = plan(
        &source_vfs,
        src.path(),
        &target_vfs,
        dst.path(),
        &hash_opts(SyncMode::Mirror),
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("unvollständig"));
}

#[test]
fn zielwurzel_stat_fehler_gilt_nicht_als_leeres_ziel() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"inhalt");
    let source_vfs = LocalVfs::new();
    let target_vfs = CountingVfs::new();
    target_vfs.fail_stat_for(dst.path());

    let error = plan(
        &source_vfs,
        src.path(),
        &target_vfs,
        dst.path(),
        &opts(SyncMode::AddOnly),
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn stat_fehler_eines_neuen_ziels_stoppt_vor_der_temp_anlage() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"inhalt");
    let source_vfs = LocalVfs::new();
    let target_vfs = CountingVfs::new();
    let sync_plan = plan(
        &source_vfs,
        src.path(),
        &target_vfs,
        dst.path(),
        &opts(SyncMode::AddOnly),
    )
    .unwrap();
    target_vfs.fail_stat_for(&dst.path().join("a.txt"));

    let error = execute(
        ExecuteRequest {
            src: &source_vfs,
            src_root: src.path(),
            dst: &target_vfs,
            dst_root: dst.path(),
            plan: &sync_plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(fs::read_dir(dst.path()).unwrap().next().is_none());
}

#[test]
fn alte_syncoptionen_laden_mit_leerer_ausschlussliste() {
    let options: SyncOptions = serde_json::from_value(serde_json::json!({
        "mode": "AddOnly",
        "compare": "SizeAndTime",
        "metadata": "Portable",
        "mtime_tolerance": 2
    }))
    .unwrap();

    assert!(options.exclude.is_empty());
}

/// Ein Name mit LITERALEM Ersatzzeichen ist gültiges UTF-8 und über das
/// `safely_decoded`-Flag der Vollauflistung klar vom verlustbehaftet dekodierten
/// Fall unterscheidbar — er wird deshalb ganz normal synchronisiert.
#[test]
fn literaler_ersatzzeichen_name_wird_normal_synchronisiert() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "echt-\u{FFFD}.txt", b"inhalt");

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert_eq!(p.copy_count, 1);
    assert_eq!(p.skip_count, 0);

    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(read_file(dst.path(), "echt-\u{FFFD}.txt"), b"inhalt");
}

/// Round-Trip-Beleg: Der Planner `stat`et das Ziel nur an der Wurzel. Jede tiefere
/// Ebene ist bereits durch das Eltern-Listing bekannt — vor der Optimierung fiel
/// pro Verzeichnisebene ein zusätzlicher `stat` an (remote: ein Round-Trip).
#[test]
fn planner_statet_das_ziel_nur_an_der_wurzel() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a/b/c/tief.txt", b"x");
    write_file(src.path(), "a/flach.txt", b"y");
    fs::create_dir_all(dst.path().join("a/b/c")).unwrap();

    let vfs = CountingVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::AddOnly)).unwrap();

    assert_eq!(p.copy_count, 2);
    // Genau EIN stat (Ziel-Wurzel) — unabhängig von der Baumtiefe. AddOnly/Update
    // messen zudem keine Gesamtbestände mehr (das brauchen nur die Mirror-Gates).
    assert_eq!(vfs.stats(), 1);
}

#[test]
fn native_strict_lehnt_unbekannte_ziel_faehigkeiten_vor_mutation_ab() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"inhalt");
    let source_vfs = LocalVfs::new();
    let target_vfs = MetadataTestVfs::with_unknown_capabilities();
    let options = SyncOptions {
        compare: Compare::ContentHash,
        metadata: MetadataProfile::NativeStrict,
        ..opts(SyncMode::Update)
    };

    let error = plan(&source_vfs, src.path(), &target_vfs, dst.path(), &options).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert!(!dst.path().join("a.txt").exists());
}

#[test]
fn native_strict_lehnt_inkompatibles_acl_modell_vor_mutation_ab() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "acl.txt", b"inhalt");
    let source_vfs = LocalVfs::new();
    source_vfs
        .write_acl_exact(&src.path().join("acl.txt"), Some(&native_test_acl()))
        .unwrap();
    let target_vfs = MetadataTestVfs::with_incompatible_acl_model();
    let options = SyncOptions {
        compare: Compare::ContentHash,
        metadata: MetadataProfile::NativeStrict,
        ..opts(SyncMode::AddOnly)
    };

    let error = plan(&source_vfs, src.path(), &target_vfs, dst.path(), &options).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert!(error.to_string().contains("inkompatible Modelle"));
    assert!(fs::read_dir(dst.path()).unwrap().next().is_none());
}

#[test]
#[cfg(target_os = "linux")]
fn linux_native_strict_wird_nur_ohne_auslassung_freigegeben() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"inhalt");
    let vfs = LocalVfs::new();
    vfs.write_xattrs_exact(
        &src.path().join("a.txt"),
        &[XattrValue {
            name: b"user.doppeldecker.strict".to_vec(),
            value: b"exakt".to_vec(),
        }],
    )
    .unwrap();
    let options = SyncOptions {
        compare: Compare::ContentHash,
        metadata: MetadataProfile::NativeStrict,
        ..opts(SyncMode::AddOnly)
    };

    let sync_plan = plan(&vfs, src.path(), &vfs, dst.path(), &options).unwrap();

    assert!(sync_plan.metadata_report.strict_verified);
    assert!(sync_plan.metadata_report.issues.is_empty());
    assert_eq!(sync_plan.metadata_report.unsupported_metadata, 0);
    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &sync_plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(
        vfs.read_xattrs(&dst.path().join("a.txt")).unwrap(),
        vfs.read_xattrs(&src.path().join("a.txt")).unwrap()
    );
}

#[test]
fn native_best_effort_ist_fuer_mirror_gesperrt() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let vfs = LocalVfs::new();

    let error = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::Mirror),
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn acl_wird_plan_gebunden_und_gemeinsam_mit_modusbits_uebertragen() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "acl.txt", b"gleicher inhalt");
    write_file(dst.path(), "acl.txt", b"gleicher inhalt");
    let vfs = LocalVfs::new();
    fs::set_permissions(dst.path(), fs::Permissions::from_mode(0o700)).unwrap();
    vfs.write_acl_exact(dst.path(), None).unwrap();
    let stage = vfs
        .staging_scope(&dst.path().join("probe"), None)
        .unwrap()
        .unwrap();
    stage.finish_staging().unwrap();

    let acl = native_test_acl();
    vfs.write_acl_exact(&src.path().join("acl.txt"), Some(&acl))
        .unwrap();
    set_mtime(src.path(), "acl.txt", 1_700_000_000);
    set_mtime(dst.path(), "acl.txt", 1_700_000_000);

    let sync_plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::Update),
    )
    .unwrap();
    assert!(sync_plan.actions.iter().any(|action| matches!(
        action,
        Action::UpdateFile { metadata_only: true, metadata: Some(metadata), .. }
            if metadata.acl && metadata.normal_mode && metadata.source.acl.as_ref() == Some(&acl)
    )));

    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &sync_plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    let target = vfs.metadata_manifest(&dst.path().join("acl.txt")).unwrap();
    assert_eq!(target.acl, Some(acl));
    assert_eq!(
        target.mode & 0o777,
        vfs.metadata_manifest(&src.path().join("acl.txt"))
            .unwrap()
            .mode
            & 0o777
    );
}

#[test]
fn acl_drift_stoppt_vor_der_ersten_zielmutation() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "acl.txt", b"quelle");
    write_file(dst.path(), "acl.txt", b"ziel-alt");
    let vfs = LocalVfs::new();
    let source_path = src.path().join("acl.txt");
    let target_path = dst.path().join("acl.txt");
    vfs.write_acl_exact(&source_path, Some(&native_test_acl()))
        .unwrap();
    set_mtime(src.path(), "acl.txt", 1_700_000_100);
    set_mtime(dst.path(), "acl.txt", 1_700_000_000);
    let sync_plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::Update),
    )
    .unwrap();

    vfs.write_acl_exact(&source_path, None).unwrap();
    let error = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &sync_plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();

    assert!(error.to_string().contains("Planbindung"));
    assert_eq!(fs::read(&target_path).unwrap(), b"ziel-alt");
    assert_eq!(vfs.metadata_manifest(&target_path).unwrap().acl, None);
}

#[test]
#[cfg(target_os = "linux")]
fn linux_xattrs_werden_atomar_ueber_die_temp_datei_ersetzt() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"gleicher inhalt");
    write_file(dst.path(), "a.txt", b"gleicher inhalt");
    set_mtime(src.path(), "a.txt", 1_700_000_000);
    set_mtime(dst.path(), "a.txt", 1_700_000_000);
    let vfs = LocalVfs::new();
    fs::set_permissions(dst.path(), fs::Permissions::from_mode(0o700)).unwrap();
    vfs.write_acl_exact(dst.path(), None).unwrap();
    let stage = vfs
        .staging_scope(&dst.path().join("probe"), None)
        .unwrap()
        .unwrap();
    stage.finish_staging().unwrap();

    let source_xattrs = vec![XattrValue {
        name: b"user.doppeldecker.quelle".to_vec(),
        value: vec![0, 1, 2, 255],
    }];
    let target_xattrs = vec![XattrValue {
        name: b"user.doppeldecker.alt".to_vec(),
        value: b"weg".to_vec(),
    }];
    vfs.write_xattrs_exact(&src.path().join("a.txt"), &source_xattrs)
        .unwrap();
    vfs.write_xattrs_exact(&dst.path().join("a.txt"), &target_xattrs)
        .unwrap();

    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::Update),
    )
    .unwrap();
    assert_eq!(plan.update_count, 1);
    assert_eq!(plan.metadata_count, 1);
    assert!(matches!(
        &plan.actions[0],
        Action::UpdateFile {
            metadata_only: true,
            metadata: Some(metadata),
            ..
        } if metadata.xattrs && !metadata.resource_fork
    ));

    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(read_file(dst.path(), "a.txt"), b"gleicher inhalt");
    assert_eq!(
        vfs.read_xattrs(&dst.path().join("a.txt")).unwrap(),
        source_xattrs
    );
}

#[test]
#[cfg(target_os = "linux")]
fn xattr_plan_drift_stoppt_vor_der_ersten_zielmutation() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"inhalt");
    let vfs = LocalVfs::new();
    let source_path = src.path().join("a.txt");
    vfs.write_xattrs_exact(
        &source_path,
        &[XattrValue {
            name: b"user.doppeldecker.plan".to_vec(),
            value: b"vorher".to_vec(),
        }],
    )
    .unwrap();
    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::AddOnly),
    )
    .unwrap();

    vfs.write_xattrs_exact(
        &source_path,
        &[XattrValue {
            name: b"user.doppeldecker.plan".to_vec(),
            value: b"nachher".to_vec(),
        }],
    )
    .unwrap();
    let error = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();

    assert!(error.to_string().contains("Metadaten-Planbindung"));
    assert!(fs::read_dir(dst.path()).unwrap().next().is_none());
}

#[test]
#[cfg(target_os = "macos")]
fn macos_ressourcen_fork_wird_als_zweiter_datenstrom_kopiert() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"daten-fork");
    let vfs = LocalVfs::new();
    fs::set_permissions(dst.path(), fs::Permissions::from_mode(0o700)).unwrap();
    vfs.write_acl_exact(dst.path(), None).unwrap();
    let stage = vfs
        .staging_scope(&dst.path().join("probe"), None)
        .unwrap()
        .unwrap();
    stage.finish_staging().unwrap();

    let source_path = src.path().join("a.txt");
    let target_path = dst.path().join("a.txt");
    let mut expected = vec![0u8; 400_000];
    for (index, byte) in expected.iter_mut().enumerate() {
        *byte = (index % 239) as u8;
    }
    let mut writer = vfs
        .open_fork_write_new(&source_path, ForkKind::Resource)
        .unwrap();
    writer.write_all(&expected).unwrap();
    writer.flush().unwrap();
    drop(writer);

    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::AddOnly),
    )
    .unwrap();
    assert_eq!(plan.bytes_to_transfer, 400_010);
    assert!(matches!(
        &plan.actions[0],
        Action::CopyFile {
            metadata: Some(metadata),
            ..
        } if metadata.resource_fork && !metadata.xattrs
    ));

    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    let mut actual = Vec::new();
    vfs.open_fork_read(&target_path, ForkKind::Resource)
        .unwrap()
        .read_to_end(&mut actual)
        .unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn portable_modusaenderung_bestaetigt_erst_nach_fsync() {
    check_unconfirmed_portable_mode(true);
}

#[test]
fn portable_modusaenderung_bestaetigt_erst_nach_readback() {
    check_unconfirmed_portable_mode(false);
}

fn check_unconfirmed_portable_mode(fail_sync: bool) {
    for directory in [false, true] {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        if directory {
            fs::create_dir(src.path().join("a")).unwrap();
            fs::create_dir(dst.path().join("a")).unwrap();
        } else {
            write_file(src.path(), "a", b"gleich");
            write_file(dst.path(), "a", b"gleich");
        }
        set_mode(src.path(), "a", 0o755);
        set_mode(dst.path(), "a", if directory { 0o700 } else { 0o644 });
        let source = LocalVfs::new();
        let target = MetadataTestVfs {
            fail_sync,
            ignore_mode_write: !fail_sync,
            ..MetadataTestVfs::new()
        };
        let confirmed = plan(
            &source,
            src.path(),
            &target,
            dst.path(),
            &hash_opts(SyncMode::Update),
        )
        .unwrap();
        assert!(confirmed
            .actions
            .iter()
            .any(|a| matches!(a, Action::SetMode { .. })));
        let error = execute(
            ExecuteRequest {
                src: &source,
                src_root: src.path(),
                dst: &target,
                dst_root: dst.path(),
                plan: &confirmed,
                safety: &real(),
            },
            &Cancel::new(),
            &mut ignore_progress,
        )
        .unwrap_err();
        assert_eq!(
            doppeldecker_core::error_code(&error),
            doppeldecker_core::ErrorCode::OutcomeUnclear
        );
        assert!(error
            .to_string()
            .contains(&dst.path().join("a").to_string_lossy().into_owned()));
        assert_eq!(
            fs::metadata(dst.path().join("a"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            if fail_sync {
                0o755
            } else if directory {
                0o700
            } else {
                0o644
            }
        );
    }
}

#[test]
fn native_datei_uebertraegt_owner_normale_rechte_und_sonderbits() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "tool", b"#!/bin/sh\n");
    set_mode(src.path(), "tool", 0o4751);

    let vfs = LocalVfs::new();
    fs::set_permissions(dst.path(), fs::Permissions::from_mode(0o700)).unwrap();
    vfs.write_acl_exact(dst.path(), None).unwrap();
    let stage = vfs
        .staging_scope(&dst.path().join("probe"), None)
        .unwrap()
        .unwrap();
    stage.finish_staging().unwrap();

    let source_manifest = vfs.metadata_manifest(&src.path().join("tool")).unwrap();
    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::AddOnly),
    )
    .unwrap();
    assert!(matches!(
        &plan.actions[0],
        Action::CopyFile {
            metadata: Some(metadata),
            ..
        } if metadata.owner && metadata.normal_mode && metadata.special_mode_bits
    ));

    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    let target_manifest = vfs.metadata_manifest(&dst.path().join("tool")).unwrap();
    assert_eq!(target_manifest.owner, source_manifest.owner);
    assert_eq!(full_mode(dst.path(), "tool"), 0o4751);
}

#[test]
fn native_sparse_datei_bleibt_sparse_und_manifestgebunden() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let source_path = src.path().join("sparse.bin");
    let target_path = dst.path().join("sparse.bin");
    let logical_size = 8 * 1024 * 1024;
    let mut source = fs::File::create(&source_path).unwrap();
    #[cfg(target_os = "linux")]
    {
        source.set_len(logical_size).unwrap();
        source.seek(SeekFrom::Start(2 * 1024 * 1024)).unwrap();
        source.write_all(&vec![0x5a; 8192]).unwrap();
        source.seek(SeekFrom::Start(6 * 1024 * 1024)).unwrap();
        source.write_all(&vec![0xa5; 4096]).unwrap();
    }
    #[cfg(target_os = "macos")]
    {
        // APFS allokiert `set_len` konservativ. Ein echter Punch erzeugt dagegen ein
        // vom Dateisystem gemeldetes Loch zwischen Daten vor und nach dem Bereich.
        source
            .write_all(&vec![0x5a; logical_size as usize])
            .unwrap();
        punch_hole(&source, 2 * 1024 * 1024, 2 * 1024 * 1024);
    }
    source.sync_all().unwrap();
    drop(source);

    let vfs = LocalVfs::new();
    fs::set_permissions(dst.path(), fs::Permissions::from_mode(0o700)).unwrap();
    vfs.write_acl_exact(dst.path(), None).unwrap();
    let stage = vfs
        .staging_scope(&dst.path().join("probe"), None)
        .unwrap()
        .unwrap();
    stage.finish_staging().unwrap();

    let source_manifest = vfs.metadata_manifest(&source_path).unwrap();
    let source_extents = source_manifest.sparse_extents.as_ref().unwrap();
    assert!(!source_extents.is_empty());
    assert!(
        source_extents
            .iter()
            .map(|extent| extent.length)
            .sum::<u64>()
            < logical_size
    );

    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::AddOnly),
    )
    .unwrap();
    assert!(matches!(
        &plan.actions[0],
        Action::CopyFile {
            metadata: Some(metadata),
            ..
        } if metadata.sparse_extents
            && metadata.source.sparse_extents == source_manifest.sparse_extents
    ));

    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(
        fs::read(&target_path).unwrap(),
        fs::read(&source_path).unwrap()
    );
    assert_eq!(
        vfs.metadata_manifest(&target_path).unwrap().sparse_extents,
        source_manifest.sparse_extents
    );
}

#[test]
fn best_effort_ohne_native_zielklassen_kopiert_und_aktualisiert_inhalte() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.bin", b"erste Version");
    fs::hard_link(src.path().join("a.bin"), src.path().join("b.bin")).unwrap();
    write_file(src.path(), "folder/c.bin", b"Kindinhalt");
    let source = LocalVfs::new();
    let target = MetadataTestVfs::without_native_metadata();
    for mode in [SyncMode::AddOnly, SyncMode::Update] {
        if mode == SyncMode::Update {
            write_file(src.path(), "a.bin", b"zweite Version");
        }
        let options = native_best_effort_opts(mode);
        let preview = plan(&source, src.path(), &target, dst.path(), &options).unwrap();
        assert!(!preview.metadata_report.strict_verified);
        assert_eq!(preview.metadata_report.metadata_changes, 0);
        for class in [
            doppeldecker_core::MetadataClass::Owner,
            doppeldecker_core::MetadataClass::NormalMode,
            doppeldecker_core::MetadataClass::Hardlinks,
        ] {
            assert!(preview
                .metadata_report
                .issues
                .iter()
                .any(|issue| issue.class == class));
        }
        let stats = execute(
            ExecuteRequest {
                src: &source,
                src_root: src.path(),
                dst: &target,
                dst_root: dst.path(),
                plan: &preview,
                safety: &real(),
            },
            &Cancel::new(),
            &mut ignore_progress,
        )
        .unwrap();
        assert_eq!(stats.metadata_report, preview.metadata_report);
        for name in ["a.bin", "b.bin", "folder/c.bin"] {
            assert_eq!(
                fs::read(src.path().join(name)).unwrap(),
                fs::read(dst.path().join(name)).unwrap()
            );
        }
        assert_ne!(
            fs::metadata(dst.path().join("a.bin")).unwrap().ino(),
            fs::metadata(dst.path().join("b.bin")).unwrap().ino()
        );
    }
    fs::set_permissions(src.path().join("a.bin"), fs::Permissions::from_mode(0o600)).unwrap();
    let preview = plan(
        &source,
        src.path(),
        &target,
        dst.path(),
        &native_best_effort_opts(SyncMode::Update),
    )
    .unwrap();
    assert!(!preview
        .actions
        .iter()
        .any(|action| matches!(action, Action::SetMode { .. })));
    assert_eq!(preview.update_count, 0);
}

#[test]
fn fehlende_zielhardlinks_werden_bilanziert_und_sperren_strict() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.bin", b"gemeinsamer Inhalt");
    fs::hard_link(src.path().join("a.bin"), src.path().join("b.bin")).unwrap();
    let source = LocalVfs::new();
    let target = MetadataTestVfs::without_hardlinks();
    let options = native_best_effort_opts(SyncMode::AddOnly);
    #[cfg(target_os = "linux")]
    {
        let mut strict = options.clone();
        strict.metadata = MetadataProfile::NativeStrict;
        let error = plan(&source, src.path(), &target, dst.path(), &strict).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("Hardlinks"));
    }
    let preview = plan(&source, src.path(), &target, dst.path(), &options).unwrap();
    let omitted = preview
        .metadata_report
        .issues
        .iter()
        .filter(|issue| issue.class == doppeldecker_core::MetadataClass::Hardlinks)
        .map(|issue| issue.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(omitted, vec!["a.bin", "b.bin"]);
    assert!(!preview
        .actions
        .iter()
        .any(|action| matches!(action, Action::CreateHardlink { .. })));
    assert_eq!(
        preview.bytes_to_transfer,
        2 * b"gemeinsamer Inhalt".len() as u64
    );
    assert!(fs::read_dir(dst.path()).unwrap().next().is_none());
    execute(
        ExecuteRequest {
            src: &source,
            src_root: src.path(),
            dst: &target,
            dst_root: dst.path(),
            plan: &preview,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    let a = fs::metadata(dst.path().join("a.bin")).unwrap();
    let b = fs::metadata(dst.path().join("b.bin")).unwrap();
    assert_ne!(a.ino(), b.ino());
    assert_eq!(
        fs::read(dst.path().join("a.bin")).unwrap(),
        b"gemeinsamer Inhalt"
    );
    assert_eq!(
        fs::read(dst.path().join("b.bin")).unwrap(),
        b"gemeinsamer Inhalt"
    );
}

#[test]
fn hardlink_ausserhalb_der_auswahl_erzeugt_keine_falsche_auslassung() {
    for excluded_partner in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let source_root = root.path().join("source");
        let target_root = root.path().join("target");
        fs::create_dir(&source_root).unwrap();
        fs::create_dir(&target_root).unwrap();
        write_file(&source_root, "a.bin", b"ein gewaehlter Name");
        let partner = if excluded_partner {
            source_root.join("b.bin")
        } else {
            root.path().join("outside.bin")
        };
        fs::hard_link(source_root.join("a.bin"), partner).unwrap();
        let source = LocalVfs::new();
        let target = MetadataTestVfs::without_hardlinks();
        let mut options = native_best_effort_opts(SyncMode::AddOnly);
        if excluded_partner {
            options.exclude = vec!["b.bin".into()];
        }
        let preview = plan(&source, &source_root, &target, &target_root, &options).unwrap();
        assert_eq!(preview.copy_count, 1);
        assert!(!preview
            .metadata_report
            .issues
            .iter()
            .any(|issue| issue.class == doppeldecker_core::MetadataClass::Hardlinks));
        #[cfg(target_os = "linux")]
        {
            let mut strict = options.clone();
            strict.metadata = MetadataProfile::NativeStrict;
            let preview = plan(&source, &source_root, &target, &target_root, &strict).unwrap();
            assert!(preview.metadata_report.strict_verified);
        }
    }
}

#[test]
fn native_hardlink_gruppe_kopiert_daten_einmal_und_bindet_weitere_namen() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.bin", b"gemeinsamer inhalt");
    fs::hard_link(src.path().join("a.bin"), src.path().join("b.bin")).unwrap();

    let vfs = LocalVfs::new();
    fs::set_permissions(dst.path(), fs::Permissions::from_mode(0o700)).unwrap();
    vfs.write_acl_exact(dst.path(), None).unwrap();
    let stage = vfs
        .staging_scope(&dst.path().join("probe"), None)
        .unwrap()
        .unwrap();
    stage.finish_staging().unwrap();

    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::AddOnly),
    )
    .unwrap();
    assert_eq!(plan.copy_count, 2);
    assert_eq!(plan.bytes_to_transfer, b"gemeinsamer inhalt".len() as u64);
    let copy_group = plan.actions.iter().find_map(|action| match action {
        Action::CopyFile {
            metadata: Some(metadata),
            ..
        } => metadata.source.hardlink_group,
        _ => None,
    });
    assert!(matches!(
        plan.actions.iter().find(|action| matches!(action, Action::CreateHardlink { .. })),
        Some(Action::CreateHardlink { link_to, metadata, .. })
            if link_to == "a.bin"
                && metadata.hardlinks
                && metadata.source.hardlink_group == copy_group
    ));

    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    let a = fs::metadata(dst.path().join("a.bin")).unwrap();
    let b = fs::metadata(dst.path().join("b.bin")).unwrap();
    assert_eq!(a.ino(), b.ino());
    assert_eq!(a.dev(), b.dev());
    assert_eq!(a.nlink(), 2);

    let second = doppeldecker_core::plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::Update),
    )
    .unwrap();
    assert!(!second
        .actions
        .iter()
        .any(|action| matches!(action, Action::CreateHardlink { .. })));
}

#[test]
fn native_hardlink_update_ersetzt_einen_unabhaengigen_zielnamen() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.bin", b"neu");
    fs::hard_link(src.path().join("a.bin"), src.path().join("b.bin")).unwrap();
    write_file(dst.path(), "a.bin", b"neu");
    write_file(dst.path(), "b.bin", b"alt");

    let vfs = LocalVfs::new();
    fs::set_permissions(dst.path(), fs::Permissions::from_mode(0o700)).unwrap();
    vfs.write_acl_exact(dst.path(), None).unwrap();
    let stage = vfs
        .staging_scope(&dst.path().join("probe"), None)
        .unwrap()
        .unwrap();
    stage.finish_staging().unwrap();

    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::Update),
    )
    .unwrap();
    assert!(plan
        .actions
        .iter()
        .any(|action| matches!(action, Action::CreateHardlink { path, .. } if path == "b.bin")));
    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    let a = fs::metadata(dst.path().join("a.bin")).unwrap();
    let b = fs::metadata(dst.path().join("b.bin")).unwrap();
    assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
    assert_eq!(read_file(dst.path(), "b.bin"), b"neu");
}

#[test]
fn hardlink_update_bewahrt_neueren_abweichenden_zielnamen() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.bin", b"quelle");
    fs::hard_link(src.path().join("a.bin"), src.path().join("b.bin")).unwrap();
    set_mtime(src.path(), "a.bin", 1_700_000_000);
    write_file(dst.path(), "a.bin", b"quelle");
    write_file(dst.path(), "b.bin", b"neueres ziel");
    set_mtime(dst.path(), "a.bin", 1_800_000_000);
    set_mtime(dst.path(), "b.bin", 1_800_000_000);

    let vfs = LocalVfs::new();
    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::Update),
    )
    .unwrap();
    assert!(!plan
        .actions
        .iter()
        .any(|action| matches!(action, Action::CreateHardlink { path, .. } if path == "b.bin")));
    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(read_file(dst.path(), "b.bin"), b"neueres ziel");
}

#[test]
fn hardlink_gruppendrift_stoppt_vor_der_ersten_zielmutation() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.bin", b"gebunden");
    fs::hard_link(src.path().join("a.bin"), src.path().join("b.bin")).unwrap();

    let vfs = LocalVfs::new();
    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::AddOnly),
    )
    .unwrap();
    fs::remove_file(src.path().join("b.bin")).unwrap();
    write_file(src.path(), "b.bin", b"gebunden");

    let error = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();
    assert!(error.to_string().contains("Hardlink-Gruppe"));
    assert!(fs::read_dir(dst.path()).unwrap().next().is_none());
}

#[test]
fn sparse_extent_drift_stoppt_vor_der_ersten_zielmutation() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let source_path = src.path().join("drift.bin");
    let source = fs::File::create(&source_path).unwrap();
    source.set_len(4 * 1024 * 1024).unwrap();
    drop(source);
    // Ein dicht geschriebener Nullstrom hat denselben logischen Inhalt wie das später
    // ausgestanzte Loch, aber eine andere native Extent-Struktur.
    fs::write(&source_path, vec![0u8; 4 * 1024 * 1024]).unwrap();
    set_mtime(src.path(), "drift.bin", 1_700_000_000);

    let vfs = LocalVfs::new();
    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::AddOnly),
    )
    .unwrap();
    let source = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&source_path)
        .unwrap();
    punch_hole(&source, 1024 * 1024, 1024 * 1024);
    source.sync_all().unwrap();
    drop(source);
    set_mtime(src.path(), "drift.bin", 1_700_000_000);

    let error = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();

    assert!(error.to_string().contains("Metadaten-Planbindung"));
    assert!(fs::read_dir(dst.path()).unwrap().next().is_none());
}

#[test]
fn owner_fehler_erhaelt_vorhandenes_ziel_und_entfernt_tempdatei() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"neuer inhalt");
    write_file(dst.path(), "a.txt", b"altes ziel");
    set_mtime(src.path(), "a.txt", 1_700_000_100);
    set_mtime(dst.path(), "a.txt", 1_700_000_000);

    let source_vfs = LocalVfs::new();
    let target_vfs = MetadataTestVfs::new();
    let plan = plan(
        &source_vfs,
        src.path(),
        &target_vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::Update),
    )
    .unwrap();
    let error = execute(
        ExecuteRequest {
            src: &source_vfs,
            src_root: src.path(),
            dst: &target_vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(read_file(dst.path(), "a.txt"), b"altes ziel");
    assert_eq!(fs::read_dir(dst.path()).unwrap().count(), 1);
}

#[test]
fn native_ordnermetadaten_laufen_post_order_mit_mtime_readback() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    fs::create_dir_all(src.path().join("parent/child")).unwrap();
    write_file(src.path(), "parent/child/datei", b"inhalt");
    set_mode(src.path(), "parent", 0o1750);
    set_mode(src.path(), "parent/child", 0o1701);
    // Erst nach dem Anlegen aller Kinder setzen, damit dies die gewünschte Endzeit ist.
    set_mtime(src.path(), "parent/child", 1_700_000_010);
    set_mtime(src.path(), "parent", 1_700_000_020);

    let vfs = LocalVfs::new();
    #[cfg(target_os = "linux")]
    vfs.write_xattrs_exact(
        &src.path().join("parent"),
        &[XattrValue {
            name: b"user.doppeldecker.ordner".to_vec(),
            value: b"post-order".to_vec(),
        }],
    )
    .unwrap();
    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::AddOnly),
    )
    .unwrap();
    let mut metadata_order = Vec::new();
    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut |progress| {
            if progress.phase == "metadata" {
                metadata_order.push(progress.current.clone());
            }
        },
    )
    .unwrap();

    assert_eq!(metadata_order, ["parent/child", "parent"]);
    assert_eq!(full_mode(dst.path(), "parent/child"), 0o1701);
    assert_eq!(full_mode(dst.path(), "parent"), 0o1750);
    assert_eq!(
        vfs.stat(&dst.path().join("parent/child")).unwrap().mtime,
        Some(1_700_000_010)
    );
    assert_eq!(
        vfs.stat(&dst.path().join("parent")).unwrap().mtime,
        Some(1_700_000_020)
    );
    assert_eq!(
        vfs.metadata_manifest(&dst.path().join("parent"))
            .unwrap()
            .owner,
        vfs.metadata_manifest(&src.path().join("parent"))
            .unwrap()
            .owner
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        vfs.read_xattrs(&dst.path().join("parent")).unwrap(),
        vfs.read_xattrs(&src.path().join("parent")).unwrap()
    );
}

#[test]
fn bestehender_ordner_erhaelt_native_metadaten_ohne_inhaltskopie() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    fs::create_dir(src.path().join("ordner")).unwrap();
    fs::create_dir(dst.path().join("ordner")).unwrap();
    set_mode(src.path(), "ordner", 0o1750);
    set_mode(dst.path(), "ordner", 0o700);
    set_mtime(src.path(), "ordner", 1_700_000_000);
    set_mtime(dst.path(), "ordner", 1_600_000_000);

    let vfs = LocalVfs::new();
    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::Update),
    )
    .unwrap();
    assert_eq!(plan.metadata_count, 1);
    assert!(matches!(
        &plan.actions[0],
        Action::SetDirectoryMetadata { path, metadata, .. }
            if path == "ordner" && metadata.owner && metadata.special_mode_bits
    ));

    let stats = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(stats.metadata_updated, 1);
    assert_eq!(stats.bytes_copied, 0);
    assert_eq!(full_mode(dst.path(), "ordner"), 0o1750);
    assert_eq!(
        vfs.stat(&dst.path().join("ordner")).unwrap().mtime,
        Some(1_700_000_000)
    );
}

#[test]
fn ordner_mtime_drift_stoppt_vor_der_ersten_zielmutation() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    fs::create_dir(src.path().join("ordner")).unwrap();
    fs::create_dir(dst.path().join("ordner")).unwrap();
    write_file(src.path(), "neu.txt", b"darf noch nicht kopiert werden");
    set_mode(src.path(), "ordner", 0o1750);
    set_mode(dst.path(), "ordner", 0o700);
    set_mtime(src.path(), "ordner", 1_700_000_000);
    set_mtime(dst.path(), "ordner", 1_600_000_000);

    let vfs = LocalVfs::new();
    let plan = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &native_best_effort_opts(SyncMode::Update),
    )
    .unwrap();
    set_mtime(dst.path(), "ordner", 1_650_000_000);

    let error = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &plan,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    assert!(!dst.path().join("neu.txt").exists());
    assert_eq!(full_mode(dst.path(), "ordner"), 0o700);
}

#[test]
fn neue_datei_und_neuer_ordner_erhalten_posix_rechte() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "bin/start.sh", b"#!/bin/sh\nexit 0\n");
    // Sticky ist ohne Eigentümervertrag nicht portabel und darf NICHT mitkopiert werden.
    set_mode(src.path(), "bin", 0o1750);
    set_mode(src.path(), "bin/start.sh", 0o551);

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(mode(dst.path(), "bin"), 0o750);
    assert_eq!(full_mode(dst.path(), "bin"), 0o750);
    assert_eq!(mode(dst.path(), "bin/start.sh"), 0o551);
}

#[test]
fn reine_rechteanpassung_bewahrt_vorhandene_sonderbits_am_ziel() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    fs::create_dir(src.path().join("ordner")).unwrap();
    fs::create_dir(dst.path().join("ordner")).unwrap();
    set_mode(src.path(), "ordner", 0o750);
    set_mode(dst.path(), "ordner", 0o1700);

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert_eq!(p.metadata_count, 1);
    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(full_mode(dst.path(), "ordner"), 0o1750);
}

#[test]
fn identischer_inhalt_passt_rechte_ohne_neue_uebertragung_an() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "tool", b"gleich");
    write_file(dst.path(), "tool", b"gleich");
    set_mtime(src.path(), "tool", 1_700_000_000);
    set_mtime(dst.path(), "tool", 1_700_000_000);
    set_mode(src.path(), "tool", 0o751);
    set_mode(dst.path(), "tool", 0o640);

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert_eq!(p.copy_count, 0);
    assert_eq!(p.update_count, 0);
    assert_eq!(p.metadata_count, 1);
    assert_eq!(p.bytes_to_transfer, 0);
    assert!(p.actions.iter().any(|action| matches!(
        action,
        Action::SetMode {
            path,
            source_mode: 0o751,
            is_dir: false,
            ..
        } if path == "tool"
    )));

    let stats = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(stats.metadata_updated, 1);
    assert_eq!(stats.bytes_copied, 0);
    assert_eq!(mode(dst.path(), "tool"), 0o751);
}

#[test]
fn rechte_plan_drift_bricht_ab_ohne_neue_rechte_zu_setzen() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "tool", b"gleich");
    write_file(dst.path(), "tool", b"gleich");
    set_mtime(src.path(), "tool", 1_700_000_000);
    set_mtime(dst.path(), "tool", 1_700_000_000);
    set_mode(src.path(), "tool", 0o751);
    set_mode(dst.path(), "tool", 0o640);

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert_eq!(p.metadata_count, 1);

    // Änderung NACH der Vorschau: execute darf diese neue Nutzerentscheidung nicht
    // mit dem alten Plan überschreiben.
    set_mode(dst.path(), "tool", 0o600);
    let error = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(mode(dst.path(), "tool"), 0o600);
}

#[test]
fn add_only_laesst_abweichende_rechte_vorhandener_datei_unberuehrt() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "tool", b"gleich");
    write_file(dst.path(), "tool", b"gleich");
    set_mtime(src.path(), "tool", 1_700_000_000);
    set_mtime(dst.path(), "tool", 1_700_000_000);
    set_mode(src.path(), "tool", 0o751);
    set_mode(dst.path(), "tool", 0o640);

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::AddOnly)).unwrap();
    assert_eq!(p.metadata_count, 0);
    assert!(p.actions.is_empty());
    assert_eq!(mode(dst.path(), "tool"), 0o640);
}

#[test]
fn geaenderte_groesse_wird_aktualisiert() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"laenger geworden");
    write_file(dst.path(), "a.txt", b"kurz");
    // Gleiche Zeit, damit NUR die Größe den Unterschied macht.
    set_mtime(src.path(), "a.txt", 1_700_000_000);
    set_mtime(dst.path(), "a.txt", 1_700_000_000);

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert_eq!(p.update_count, 1);
    assert_eq!(p.copy_count, 0);

    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(read_file(dst.path(), "a.txt"), b"laenger geworden");
}

#[test]
fn neuere_quelle_wird_aktualisiert() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    // Gleiche Größe, aber Quelle neuer → Update (Zeitunterschied > Toleranz).
    write_file(src.path(), "a.txt", b"AAAA");
    write_file(dst.path(), "a.txt", b"BBBB");
    set_mtime(dst.path(), "a.txt", 1_700_000_000);
    set_mtime(src.path(), "a.txt", 1_700_000_100);

    let vfs = LocalVfs::new();
    for mode in [SyncMode::Mirror, SyncMode::Update] {
        let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(mode)).unwrap();
        assert_eq!(p.update_count, 1, "mode {:?} sollte aktualisieren", mode);
    }
}

#[test]
fn content_hash_erkennt_gleiche_groesse_und_mtime_mit_anderem_inhalt() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"AAAA");
    write_file(dst.path(), "a.txt", b"BBBB");
    set_mtime(src.path(), "a.txt", 1_700_000_000);
    set_mtime(dst.path(), "a.txt", 1_700_000_000);
    let vfs = LocalVfs::new();

    let fast = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert_eq!(
        fast.update_count, 0,
        "schneller Metadatenvergleich sieht keinen Unterschied"
    );

    let exact = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &hash_opts(SyncMode::Mirror),
    )
    .unwrap();
    assert_eq!(exact.update_count, 1);
    assert_eq!(exact.hashed_files, 2);
    assert_eq!(exact.bytes_hashed, 8);
    assert!(matches!(
        &exact.actions[0],
        Action::UpdateFile {
            source_hash: Some(_),
            target_hash: Some(_),
            ..
        }
    ));

    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &exact,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(read_file(dst.path(), "a.txt"), b"AAAA");
}

#[test]
fn content_hash_plan_drift_der_quelle_erhaelt_ziel() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"AAAA");
    write_file(dst.path(), "a.txt", b"BBBB");
    set_mtime(src.path(), "a.txt", 1_700_000_000);
    set_mtime(dst.path(), "a.txt", 1_700_000_000);
    let vfs = LocalVfs::new();
    let options = hash_opts(SyncMode::Mirror);
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &options).unwrap();

    write_file(src.path(), "a.txt", b"CCCC");
    set_mtime(src.path(), "a.txt", 1_700_000_000);
    assert!(execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .is_err());
    assert_eq!(read_file(dst.path(), "a.txt"), b"BBBB");
}

#[test]
fn content_hash_plan_drift_vor_datei_entfernung_stoppt_mirror() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "extra.txt", b"AAAA");
    set_mtime(src.path(), "bleibt.txt", 1_700_000_000);
    set_mtime(dst.path(), "bleibt.txt", 1_700_000_000);
    set_mtime(dst.path(), "extra.txt", 1_700_000_000);
    let vfs = LocalVfs::new();
    let p = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &hash_opts(SyncMode::Mirror),
    )
    .unwrap();
    assert_eq!(p.delete_count, 1);

    write_file(dst.path(), "extra.txt", b"BBBB");
    set_mtime(dst.path(), "extra.txt", 1_700_000_000);
    assert!(execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real_with_explicit_delete_permission(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .is_err());
    assert_eq!(read_file(dst.path(), "extra.txt"), b"BBBB");
}

#[test]
fn content_hash_bindet_tiefen_inhalt_eines_delete_dir() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "extra/tief.txt", b"AAAA");
    set_mtime(src.path(), "bleibt.txt", 1_700_000_000);
    set_mtime(dst.path(), "bleibt.txt", 1_700_000_000);
    set_mtime(dst.path(), "extra/tief.txt", 1_700_000_000);
    let vfs = LocalVfs::new();
    let p = plan(
        &vfs,
        src.path(),
        &vfs,
        dst.path(),
        &hash_opts(SyncMode::Mirror),
    )
    .unwrap();
    assert!(p.actions.iter().any(|action| matches!(
        action,
        Action::DeleteDir {
            target_tree_hash: Some(_),
            ..
        }
    )));

    write_file(dst.path(), "extra/tief.txt", b"BBBB");
    set_mtime(dst.path(), "extra/tief.txt", 1_700_000_000);
    assert!(execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real_with_explicit_delete_permission(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .is_err());
    assert_eq!(read_file(dst.path(), "extra/tief.txt"), b"BBBB");
}

#[test]
fn identische_dateien_keine_aktion() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"gleich");
    write_file(dst.path(), "a.txt", b"gleich");
    set_mtime(src.path(), "a.txt", 1_700_000_000);
    set_mtime(dst.path(), "a.txt", 1_700_000_000);

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert_eq!(p.actions.len(), 0, "identische Dateien → leerer Plan");
}

#[test]
fn ueberzaehlige_datei_mirror_loescht_update_behaelt() {
    let vfs = LocalVfs::new();

    // Mirror: Datei nur im Ziel → wird gelöscht.
    {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(dst.path(), "nur_im_ziel.txt", b"weg damit");
        let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
        assert_eq!(p.delete_count, 1);
        assert_eq!(
            count(&p.actions, |a| matches!(a, Action::DeleteFile { .. })),
            1
        );

        execute(
            ExecuteRequest {
                src: &vfs,
                src_root: src.path(),
                dst: &vfs,
                dst_root: dst.path(),
                plan: &p,
                safety: &real_with_explicit_delete_permission(),
            },
            &Cancel::new(),
            &mut ignore_progress,
        )
        .unwrap();
        assert!(!dst.path().join("nur_im_ziel.txt").exists());
    }

    // Update: dieselbe Datei bleibt erhalten.
    {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(dst.path(), "nur_im_ziel.txt", b"bleib da");
        let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Update)).unwrap();
        assert_eq!(p.delete_count, 0);
        assert_eq!(p.actions.len(), 0);
    }
}

#[test]
fn update_ueberschreibt_neueres_ziel_nicht() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    // Ziel ist NEUER als die Quelle → Update darf es nicht überschreiben.
    write_file(src.path(), "a.txt", b"alt");
    write_file(dst.path(), "a.txt", b"neu und wichtig");
    set_mtime(src.path(), "a.txt", 1_700_000_000);
    set_mtime(dst.path(), "a.txt", 1_700_000_500);

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Update)).unwrap();
    assert_eq!(p.update_count, 0, "Update darf neueres Ziel nicht anfassen");

    // Im Mirror-Modus hingegen wird auf den Quellstand „zurückgesetzt".
    let pm = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert_eq!(pm.update_count, 1);
}

#[test]
fn verschachtelte_ordner_und_leerer_ordner() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "tief/drin/datei.txt", b"x");
    fs::create_dir_all(src.path().join("leer")).unwrap();

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(read_file(dst.path(), "tief/drin/datei.txt"), b"x");
    assert!(
        dst.path().join("leer").is_dir(),
        "leerer Ordner muss angelegt werden"
    );
}

#[test]
fn round_trip_ist_idempotent() {
    // Nach einem Sync muss ein erneuter Plan LEER sein (nichts mehr zu tun).
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"eins");
    write_file(src.path(), "sub/b.txt", b"zwei");
    write_file(src.path(), "sub/c.bin", &vec![7u8; 300_000]);

    let vfs = LocalVfs::new();
    let p1 = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p1,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    let p2 = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert_eq!(
        p2.actions.len(),
        0,
        "zweiter Plan sollte leer sein, war: {:?}",
        p2.actions
    );
}

#[test]
fn symlink_wird_uebersprungen() {
    // Symlinks nur auf Unix testen (v0.1-Zielplattformen macOS/Linux).
    #[cfg(unix)]
    {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "echt.txt", b"inhalt");
        std::os::unix::fs::symlink(src.path().join("echt.txt"), src.path().join("link.txt"))
            .unwrap();

        let vfs = LocalVfs::new();
        let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
        assert_eq!(p.skip_count, 1, "Symlink sollte übersprungen werden");
        assert!(p
            .actions
            .iter()
            .any(|a| matches!(a, Action::SkipSymlink { .. })));

        execute(
            ExecuteRequest {
                src: &vfs,
                src_root: src.path(),
                dst: &vfs,
                dst_root: dst.path(),
                plan: &p,
                safety: &real(),
            },
            &Cancel::new(),
            &mut ignore_progress,
        )
        .unwrap();
        assert!(dst.path().join("echt.txt").exists());
        assert!(
            !dst.path().join("link.txt").exists(),
            "Symlink nicht kopiert"
        );
    }
}

#[test]
fn abbruch_stoppt_ausfuehrung() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "gross.bin", &vec![1u8; 2_000_000]);

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();

    // Bereits VOR dem Start abgebrochen → es darf nichts geschrieben werden.
    let cancel = Cancel::new();
    cancel.cancel();
    let stats = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &cancel,
        &mut ignore_progress,
    )
    .unwrap();
    assert!(stats.cancelled);
    assert_eq!(stats.files_copied, 0);
    assert!(!dst.path().join("gross.bin").exists());
}

#[test]
fn typkonflikt_wird_sicher_ausgelassen() {
    // Im Ziel liegt eine DATEI dort, wo die Quelle einen ORDNER hat. Auch Mirror darf
    // diesen destruktiven Konflikt nicht automatisch auflösen.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "x/inhalt.txt", b"ordner-inhalt");
    write_file(dst.path(), "x", b"ich bin faelschlich eine datei");

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert!(p
        .actions
        .iter()
        .any(|a| matches!(a, Action::SkipConflict { .. })));
    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert!(dst.path().join("x").is_file());
    assert_eq!(
        read_file(dst.path(), "x"),
        b"ich bin faelschlich eine datei"
    );
}

// --- Sicherheitsschicht: Sandbox + Dry-Run ---------------------------------

#[test]
fn relative_test_sandbox_wird_abgelehnt() {
    let result = Safety::test_sandbox(
        std::path::PathBuf::from("relative-sandbox"),
        &LocalVfs::new(),
    );
    assert!(result.is_err());
}

#[test]
fn sandbox_blockt_ziel_ausserhalb() {
    // Ziel liegt NICHT unter der Sandbox → execute muss abbrechen, ohne etwas zu tun.
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("quelle");
    fs::create_dir(&src).unwrap();
    let dst = tempfile::tempdir().unwrap(); // bewusst außerhalb der Sandbox
    write_file(&src, "a.txt", b"neu");
    write_file(dst.path(), "loeschen.txt", b"darf NICHT geloescht werden");

    let plan_vfs = LocalVfs::new();
    let p = plan(
        &plan_vfs,
        &src,
        &plan_vfs,
        dst.path(),
        &opts(SyncMode::Mirror),
    )
    .unwrap();
    let (vfs, safety) = marked_sandbox(sandbox.path());
    let res = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: &src,
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &safety,
        },
        &Cancel::new(),
        &mut ignore_progress,
    );

    assert!(
        res.is_err(),
        "Ziel außerhalb der Sandbox muss abgelehnt werden"
    );
    // Nichts wurde verändert:
    assert!(
        dst.path().join("loeschen.txt").exists(),
        "Löschziel unangetastet"
    );
    assert!(!dst.path().join("a.txt").exists(), "nichts kopiert");
}

#[test]
fn sandbox_erlaubt_ziel_innerhalb() {
    // Ziel liegt unter der Sandbox → Sync läuft normal.
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("quelle");
    let dst_path = sandbox.path().join("ziel");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst_path).unwrap();
    write_file(&src, "a.txt", b"hallo");

    let (vfs, safety) = marked_sandbox(sandbox.path());
    let p = plan(&vfs, &src, &vfs, &dst_path, &opts(SyncMode::Mirror)).unwrap();
    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: &src,
            dst: &vfs,
            dst_root: &dst_path,
            plan: &p,
            safety: &safety,
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(read_file(&dst_path, "a.txt"), b"hallo");
}

#[test]
fn sandbox_sync_verlangt_gehaltene_vfs_wurzel() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("quelle");
    let dst = sandbox.path().join("ziel");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    write_file(&src, "a.txt", b"hallo");

    let (_permit_vfs, safety) = marked_sandbox(sandbox.path());
    let unanchored = LocalVfs::new();
    let p = plan(
        &unanchored,
        &src,
        &unanchored,
        &dst,
        &opts(SyncMode::AddOnly),
    )
    .unwrap();
    let result = execute(
        ExecuteRequest {
            src: &unanchored,
            src_root: &src,
            dst: &unanchored,
            dst_root: &dst,
            plan: &p,
            safety: &safety,
        },
        &Cancel::new(),
        &mut ignore_progress,
    );

    assert!(result.is_err());
    assert!(!dst.join("a.txt").exists());
}

#[test]
fn ausgetauschte_sandbox_wurzel_stoppt_vor_mutation() {
    let parent = tempfile::tempdir().unwrap();
    let sandbox = parent.path().join("sandbox");
    let parked = parent.path().join("geparkt");
    let src = sandbox.join("quelle");
    let dst = sandbox.join("ziel");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    write_file(&src, "a.txt", b"urspruenglich");

    let (anchored, safety) = marked_sandbox(&sandbox);
    let p = plan(&anchored, &src, &anchored, &dst, &opts(SyncMode::AddOnly)).unwrap();

    fs::rename(&sandbox, &parked).unwrap();
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    fs::write(
        sandbox.join(TEST_SANDBOX_MARKER),
        TEST_SANDBOX_MARKER_CONTENT,
    )
    .unwrap();

    let result = execute(
        ExecuteRequest {
            src: &anchored,
            src_root: &src,
            dst: &anchored,
            dst_root: &dst,
            plan: &p,
            safety: &safety,
        },
        &Cancel::new(),
        &mut ignore_progress,
    );

    assert!(result.is_err());
    assert!(
        !dst.join("a.txt").exists(),
        "Ersatzwurzel bleibt unangetastet"
    );
    assert!(
        !parked.join("ziel/a.txt").exists(),
        "gehaltene alte Wurzel wird nach Identitätswechsel nicht mehr verändert"
    );
}

#[test]
fn permit_lehnt_neu_geoeffnete_ersatzwurzel_ab() {
    let parent = tempfile::tempdir().unwrap();
    let sandbox = parent.path().join("sandbox");
    let parked = parent.path().join("geparkt");
    let src = sandbox.join("quelle");
    let dst = sandbox.join("ziel");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    write_file(&src, "a.txt", b"alt");

    let (original, safety) = marked_sandbox(&sandbox);
    let p = plan(&original, &src, &original, &dst, &opts(SyncMode::AddOnly)).unwrap();
    fs::rename(&sandbox, &parked).unwrap();
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    fs::write(
        sandbox.join(TEST_SANDBOX_MARKER),
        TEST_SANDBOX_MARKER_CONTENT,
    )
    .unwrap();
    let replacement = LocalVfs::rooted(&sandbox).unwrap();

    let result = execute(
        ExecuteRequest {
            src: &replacement,
            src_root: &src,
            dst: &replacement,
            dst_root: &dst,
            plan: &p,
            safety: &safety,
        },
        &Cancel::new(),
        &mut ignore_progress,
    );

    assert!(result.is_err());
    assert!(!dst.join("a.txt").exists());
    assert!(!parked.join("ziel/a.txt").exists());
}

#[test]
fn dry_run_veraendert_nichts() {
    // Testlauf: der Plan hat Aktionen, aber im Ziel ändert sich real NICHTS.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "neu.txt", b"soll kopiert werden");
    write_file(dst.path(), "weg.txt", b"soll (nicht) geloescht werden");

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert!(p.copy_count >= 1 && p.delete_count >= 1);

    let safety = Safety::default();
    let stats = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &safety,
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert!(stats.dry_run);
    assert_eq!(stats.files_copied, 1, "Bilanz zählt simuliert mit");
    // Aber real ist nichts passiert:
    assert!(
        !dst.path().join("neu.txt").exists(),
        "im Testlauf nichts kopiert"
    );
    assert!(
        dst.path().join("weg.txt").exists(),
        "im Testlauf nichts gelöscht"
    );
}

// --- Safety-Hardening: konkrete Datenverlust-Szenarien ---------------------

#[test]
fn add_only_laesst_vorhandenes_unangetastet() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "vorhanden.txt", b"quelle");
    write_file(src.path(), "neu.txt", b"neu");
    write_file(dst.path(), "vorhanden.txt", b"ziel bleibt");

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::AddOnly)).unwrap();
    assert_eq!(p.copy_count, 1);
    assert_eq!(p.update_count, 0);
    assert_eq!(p.delete_count, 0);

    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(read_file(dst.path(), "vorhanden.txt"), b"ziel bleibt");
    assert_eq!(read_file(dst.path(), "neu.txt"), b"neu");
}

#[test]
fn quell_symlink_schuetzt_gleichnamiges_ziel_vor_mirror_loeschung() {
    #[cfg(unix)]
    {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "echt.txt", b"quelle");
        std::os::unix::fs::symlink(src.path().join("echt.txt"), src.path().join("link.txt"))
            .unwrap();
        write_file(dst.path(), "link.txt", b"wichtiges ziel");

        let vfs = LocalVfs::new();
        let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
        assert_eq!(p.delete_count, 0);
        assert!(p
            .actions
            .iter()
            .any(|a| matches!(a, Action::SkipSymlink { path } if path == "link.txt")));

        execute(
            ExecuteRequest {
                src: &vfs,
                src_root: src.path(),
                dst: &vfs,
                dst_root: dst.path(),
                plan: &p,
                safety: &real(),
            },
            &Cancel::new(),
            &mut ignore_progress,
        )
        .unwrap();
        assert_eq!(read_file(dst.path(), "link.txt"), b"wichtiges ziel");
    }
}

#[test]
fn mirror_mit_ausgelassenem_eintrag_entfernt_gar_nichts() {
    #[cfg(unix)]
    {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "echt.txt", b"quelle");
        std::os::unix::fs::symlink(src.path().join("echt.txt"), src.path().join("link.txt"))
            .unwrap();
        write_file(
            dst.path(),
            "extra.txt",
            b"bleibt wegen unvollstaendigem Plan",
        );

        let vfs = LocalVfs::new();
        let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
        assert_eq!(p.skip_count, 1);
        assert_eq!(p.delete_count, 1);

        let result = execute(
            ExecuteRequest {
                src: &vfs,
                src_root: src.path(),
                dst: &vfs,
                dst_root: dst.path(),
                plan: &p,
                safety: &real_with_explicit_delete_permission(),
            },
            &Cancel::new(),
            &mut ignore_progress,
        );
        assert!(result.is_err());
        assert!(dst.path().join("extra.txt").exists());
    }
}

#[test]
fn ziel_symlink_kann_sandbox_beim_schreiben_nicht_verlassen() {
    #[cfg(unix)]
    {
        let sandbox = tempfile::tempdir().unwrap();
        let src = sandbox.path().join("quelle");
        let dst = sandbox.path().join("ziel");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        write_file(&src, "canary.txt", b"neuer inhalt");

        let outside = tempfile::tempdir().unwrap();
        write_file(outside.path(), "canary.txt", b"AUSSEN UNVERAENDERT");
        std::os::unix::fs::symlink(outside.path().join("canary.txt"), dst.join("canary.txt"))
            .unwrap();

        let (vfs, safety) = marked_sandbox(sandbox.path());
        let p = plan(&vfs, &src, &vfs, &dst, &opts(SyncMode::Mirror)).unwrap();
        assert_eq!(p.update_count, 0);
        assert!(p
            .actions
            .iter()
            .any(|a| matches!(a, Action::SkipSymlink { .. })));
        execute(
            ExecuteRequest {
                src: &vfs,
                src_root: &src,
                dst: &vfs,
                dst_root: &dst,
                plan: &p,
                safety: &safety,
            },
            &Cancel::new(),
            &mut ignore_progress,
        )
        .unwrap();

        assert_eq!(
            read_file(outside.path(), "canary.txt"),
            b"AUSSEN UNVERAENDERT"
        );
        assert!(dst
            .join("canary.txt")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
    }
}

#[test]
fn abbruch_mitten_im_update_erhaelt_alte_zieldatei() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "gross.bin", &vec![7u8; 2_000_000]);
    write_file(dst.path(), "gross.bin", b"UNERSETZBARES ORIGINAL");
    set_mtime(src.path(), "gross.bin", 1_700_000_100);
    set_mtime(dst.path(), "gross.bin", 1_700_000_000);

    fs::set_permissions(dst.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let vfs = LocalVfs::new();
    vfs.write_acl_exact(dst.path(), None).unwrap();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Update)).unwrap();
    let cancel = Cancel::new();
    let cancel_from_callback = cancel.clone();
    let mut callback = move |progress: &Progress| {
        if progress.bytes_done >= 128 * 1024 {
            cancel_from_callback.cancel();
        }
    };
    let stats = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &cancel,
        &mut callback,
    )
    .unwrap();

    assert!(stats.cancelled);
    assert_eq!(stats.files_copied, 0);
    assert_eq!(
        read_file(dst.path(), "gross.bin"),
        b"UNERSETZBARES ORIGINAL"
    );
    assert!(
        fs::read_dir(dst.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("doppeldecker-tmp")),
        "abgebrochene Temp-Datei muss entfernt werden"
    );
}

#[test]
fn abbruch_ohne_privaten_namensraum_bilanziert_erhaltenen_temp() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "gross.bin", &vec![7u8; 2_000_000]);
    write_file(dst.path(), "gross.bin", b"UNERSETZBARES ORIGINAL");
    set_mtime(src.path(), "gross.bin", 1_700_000_100);
    set_mtime(dst.path(), "gross.bin", 1_700_000_000);

    fs::set_permissions(dst.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let vfs = LocalVfs::new();
    vfs.write_acl_exact(dst.path(), None).unwrap();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Update)).unwrap();
    let cancel = Cancel::new();
    let cancel_from_callback = cancel.clone();
    let mut callback = move |progress: &Progress| {
        if progress.bytes_done >= 128 * 1024 {
            cancel_from_callback.cancel();
        }
    };
    let stats = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &cancel,
        &mut callback,
    )
    .unwrap();

    assert!(stats.cancelled);
    assert_eq!(stats.files_copied, 0);
    assert_eq!(
        read_file(dst.path(), "gross.bin"),
        b"UNERSETZBARES ORIGINAL"
    );
    assert_eq!(stats.temp_recovery_paths.len(), 1);
    assert!(Path::new(&stats.temp_recovery_paths[0]).is_file());
}

#[test]
fn plan_drift_vor_update_erhaelt_zieldatei() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"geplant");
    write_file(dst.path(), "a.txt", b"ziel bleibt");
    set_mtime(src.path(), "a.txt", 1_700_000_100);
    set_mtime(dst.path(), "a.txt", 1_700_000_000);

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Update)).unwrap();
    write_file(src.path(), "a.txt", b"nach Vorschau veraendert und laenger");

    let result = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    );
    assert!(result.is_err());
    assert_eq!(read_file(dst.path(), "a.txt"), b"ziel bleibt");
}

#[test]
fn abweichende_planzaehler_koennen_loeschgates_nicht_umgehen() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "extra.txt", b"darf nicht verschwinden");
    set_mtime(src.path(), "bleibt.txt", 1_700_000_000);
    set_mtime(dst.path(), "bleibt.txt", 1_700_000_000);

    let vfs = LocalVfs::new();
    let mut sync_plan = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert_eq!(sync_plan.delete_count, 1);
    sync_plan.delete_count = 0;

    let error = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &sync_plan,
            safety: &real_with_explicit_delete_permission(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(dst.path().join("extra.txt").exists());
}

#[test]
fn nicht_endlicher_loeschanteil_wird_vor_der_wirkung_abgelehnt() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "extra.txt", b"darf nicht verschwinden");
    set_mtime(src.path(), "bleibt.txt", 1_700_000_000);
    set_mtime(dst.path(), "bleibt.txt", 1_700_000_000);

    let vfs = LocalVfs::new();
    let sync_plan = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    let mut safety = real_with_explicit_delete_permission();
    safety.max_delete_fraction = Some(f64::NAN);

    let error = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &sync_plan,
            safety: &safety,
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(dst.path().join("extra.txt").exists());
}

#[test]
fn leere_quelle_darf_ziel_nicht_leeren() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(dst.path(), "wichtig.txt", b"behalten");

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    let result = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    );
    assert!(result.is_err());
    assert!(dst.path().join("wichtig.txt").exists());
}

#[test]
fn hoher_loeschanteil_wird_hart_blockiert() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "bleibt.txt", b"gleich");
    write_file(dst.path(), "extra.txt", b"nicht automatisch entfernen");
    set_mtime(src.path(), "bleibt.txt", 1_700_000_000);
    set_mtime(dst.path(), "bleibt.txt", 1_700_000_000);

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert_eq!(p.delete_count, 1);
    assert_eq!(p.target_count, 2);
    let result = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    );
    assert!(
        result.is_err(),
        "50-Prozent-Löschanteil muss die 25-Prozent-Grenze reißen"
    );
    assert!(dst.path().join("extra.txt").exists());
}

#[test]
fn delete_dir_zaehlt_rekursiv_statt_nur_eins() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "bleibt.txt", b"x");
    write_file(dst.path(), "bleibt.txt", b"x");
    write_file(dst.path(), "extra/a.txt", b"a");
    write_file(dst.path(), "extra/tief/b.txt", b"bb");
    set_mtime(src.path(), "bleibt.txt", 1_700_000_000);
    set_mtime(dst.path(), "bleibt.txt", 1_700_000_000);

    let vfs = LocalVfs::new();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::Mirror)).unwrap();
    assert_eq!(p.delete_count, 4, "extra + tief + zwei Dateien");
    assert_eq!(p.delete_bytes, 3);
    assert!(p.actions.iter().any(|a| matches!(
        a,
        Action::DeleteDir {
            entries: 4,
            bytes: 3,
            ..
        }
    )));
}

#[test]
fn quarantine_bewahrt_ersetzte_und_entfernte_ziele() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("quelle");
    let dst = sandbox.path().join("ziel");
    let quarantine = sandbox.path().join("quarantaene/lauf-1");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    write_file(&src, "a.txt", b"neu");
    write_file(&dst, "a.txt", b"alt und wichtig");
    write_file(&dst, "extra.txt", b"auch wichtig");
    set_mtime(&src, "a.txt", 1_700_000_100);
    set_mtime(&dst, "a.txt", 1_700_000_000);

    let (vfs, mut safety) = marked_sandbox(sandbox.path());
    let p = plan(&vfs, &src, &vfs, &dst, &opts(SyncMode::Mirror)).unwrap();
    safety.quarantine = Some(quarantine.clone());
    safety.max_delete_entries = None;
    safety.max_delete_fraction = None;
    let stats = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: &src,
            dst: &vfs,
            dst_root: &dst,
            plan: &p,
            safety: &safety,
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(read_file(&dst, "a.txt"), b"neu");
    assert!(!dst.join("extra.txt").exists());
    assert_eq!(read_file(&quarantine, "a.txt"), b"alt und wichtig");
    assert_eq!(read_file(&quarantine, "extra.txt"), b"auch wichtig");
    assert_eq!(
        stats.quarantine.as_deref(),
        Some(quarantine.to_string_lossy().as_ref())
    );

    let base = quarantine.parent().unwrap();
    let boundary = Some(sandbox.path().to_path_buf());
    let runs = list_quarantine_runs(&vfs, base, &boundary).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].items_available, 2);
    assert_eq!(runs[0].target_root, dst.to_string_lossy());
    let entries =
        list_quarantine_entries_for_target(&vfs, base, "lauf-1", &boundary, Some(&dst)).unwrap();
    assert_eq!(entries.len(), 2);

    // Ein entfernter Name kann zurückkehren; die ersetzte alte Version darf die neue
    // Zieldatei dagegen niemals überschreiben.
    restore_quarantine_entry_for_target(&vfs, base, "lauf-1", "extra.txt", &boundary, Some(&dst))
        .unwrap();
    assert_eq!(read_file(&dst, "extra.txt"), b"auch wichtig");
    assert!(restore_quarantine_entry_for_target(
        &vfs,
        base,
        "lauf-1",
        "a.txt",
        &boundary,
        Some(&dst),
    )
    .is_err());
    assert_eq!(read_file(&dst, "a.txt"), b"neu");
}

#[test]
fn identische_oder_verschachtelte_wurzeln_werden_abgelehnt() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("quelle");
    let nested = source.join("darunter");
    let separate = root.path().join("ziel");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(&separate).unwrap();

    assert!(validate_local_sync_roots(&source, &source).is_err());
    assert!(validate_local_sync_roots(&source, &nested).is_err());
    assert!(validate_local_sync_roots(&nested, &source).is_err());
    assert!(validate_local_sync_roots(&source, &separate).is_ok());
}

/// Ein Ziel, das POSIX-Rechte gar nicht schreiben kann, darf keine Rechteaktion
/// bekommen.
///
/// Alle vier Remote-Provider melden `supports_mode_write() == false`, und SMB,
/// WebDAV sowie FTPS liefern `mode: None` — gegen einen lokalen Quellmodus ist das
/// immer ein Unterschied. Früher plante der Sync daraufhin `SetMode`, der Executor
/// übersprang die Aktion still, und der Lauf konvergierte nie: Der Folgeplan enthielt
/// dieselbe Aktion erneut. Bei Mirror brach der Zweitscan nach den bereits
/// geschriebenen Kopien ab, weil er die unerledigte Aktion als fremde Änderung las —
/// die Entfernungen liefen dann gar nicht mehr.
#[test]
fn ziel_ohne_schreibbare_rechte_bekommt_keine_rechteaktion() {
    let temp = canonical_tempfile::tempdir().unwrap();
    let source = temp.path().join("quelle");
    let target = temp.path().join("ziel");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&target).unwrap();
    fs::write(source.join("datei.txt"), b"inhalt").unwrap();
    fs::write(target.join("datei.txt"), b"inhalt").unwrap();
    // Gleiche Größe und Zeit, nur andere Rechte: genau der Fall, der SetMode auslöst.
    fs::set_permissions(source.join("datei.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(target.join("datei.txt"), fs::Permissions::from_mode(0o644)).unwrap();
    let source_time = fs::metadata(source.join("datei.txt")).unwrap();
    filetime::set_file_mtime(
        target.join("datei.txt"),
        filetime::FileTime::from_last_modification_time(&source_time),
    )
    .ok();

    let options = SyncOptions {
        mode: SyncMode::Update,
        ..SyncOptions::default()
    };

    // Gegenprobe: Ein lokales Ziel kann Rechte schreiben und bekommt die Aktion.
    let lokal = plan(
        &LocalVfs::new(),
        &source,
        &LocalVfs::new(),
        &target,
        &options,
    )
    .unwrap();
    assert!(
        lokal
            .actions
            .iter()
            .any(|action| matches!(action, Action::SetMode { .. })),
        "Ein lokales Ziel muss die Rechteanpassung weiterhin planen: {:?}",
        lokal.actions
    );

    let remote_artig = CountingVfs::without_mode_write();
    let ergebnis = plan(&LocalVfs::new(), &source, &remote_artig, &target, &options).unwrap();
    assert!(
        !ergebnis
            .actions
            .iter()
            .any(|action| matches!(action, Action::SetMode { .. })),
        "Ein Ziel ohne schreibbare Rechte darf keine SetMode-Aktion bekommen: {:?}",
        ergebnis.actions
    );
    assert_eq!(ergebnis.metadata_count, 0);
}

#[test]
fn c2_parent_acl_drift_entwertet_sync_plan_vor_erster_wirkung() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "datei", b"neu");
    fs::set_permissions(dst.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let vfs = LocalVfs::new();
    vfs.write_acl_exact(dst.path(), None).unwrap();
    let p = plan(&vfs, src.path(), &vfs, dst.path(), &opts(SyncMode::AddOnly)).unwrap();
    assert!(!p.local_parent_inheritance.is_empty());
    fs::set_permissions(dst.path(), fs::Permissions::from_mode(0o750)).unwrap();
    let error = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: src.path(),
            dst: &vfs,
            dst_root: dst.path(),
            plan: &p,
            safety: &real(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();
    assert_eq!(
        doppeldecker_core::error_code(&error),
        doppeldecker_core::ErrorCode::PlanDrift
    );
    assert_eq!(fs::read_dir(dst.path()).unwrap().count(), 0);
}

#[test]
fn mirror_entfernt_kinder_vor_restriktiven_ordnerrechten() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("quelle");
    let target = temp.path().join("ziel");
    write_file(&source, "ordner/behalten", b"gleich");
    write_file(&target, "ordner/behalten", b"gleich");
    write_file(&target, "ordner/entfernen", b"alt");
    set_mode(&source, "ordner", 0o555);
    set_mode(&target, "ordner", 0o755);
    let (vfs, mut safety) = marked_sandbox(temp.path());
    safety.quarantine = Some(temp.path().join("quarantaene/lauf"));
    safety.max_delete_fraction = Some(1.0);
    let options = hash_opts(SyncMode::Mirror);
    let sync_plan = plan(&vfs, &source, &vfs, &target, &options).unwrap();
    let result = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: &source,
            dst: &vfs,
            dst_root: &target,
            plan: &sync_plan,
            safety: &safety,
        },
        &Cancel::new(),
        &mut ignore_progress,
    );
    let final_mode = mode(&target, "ordner");
    let extra_exists = target.join("ordner/entfernen").exists();
    set_mode(&source, "ordner", 0o755);
    set_mode(&target, "ordner", 0o755);
    let stats = result.unwrap();
    assert_eq!(stats.files_deleted, 1);
    assert_eq!(final_mode, 0o555);
    assert!(!extra_exists);
    assert_eq!(read_file(&target, "ordner/behalten"), b"gleich");
    assert_eq!(
        read_file(temp.path(), "quarantaene/lauf/ordner/entfernen"),
        b"alt"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn native_ordner_mtime_wird_nach_eigener_kindkopie_im_realmodus_finalisiert() {
    for sync_mode in [SyncMode::Update, SyncMode::Mirror] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        // Eine Mirror-Entfernung bleibt unter der realen Fünf-Prozent-Grenze.
        for index in 0..20 {
            let path = format!("ordner/behalten-{index}");
            write_file(&source, &path, b"gleich");
            write_file(&target, &path, b"gleich");
            set_mtime(&source, &path, 1_700_000_000);
            set_mtime(&target, &path, 1_700_000_000);
        }
        write_file(&source, "ordner/neu", b"neu");
        set_mtime(&source, "ordner/neu", 1_700_000_000);
        write_file(&target, "ordner/entfernen", b"alt");
        set_mtime(&source, "ordner", 1_600_000_000);
        set_mtime(&target, "ordner", 1_600_000_000);
        let options = SyncOptions {
            mode: sync_mode,
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeStrict,
            ..Default::default()
        };
        let prepared =
            doppeldecker_core::prepare_local_real_run(&source, &target, options.clone()).unwrap();
        assert_eq!(prepared.preview().plan.copy_count, 1);
        assert_eq!(
            prepared.preview().plan.delete_count,
            u64::from(sync_mode == SyncMode::Mirror)
        );
        let directory_actions = prepared
            .preview()
            .plan
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::SetDirectoryMetadata {
                    path,
                    source_mtime,
                    target_mtime,
                    metadata,
                } => Some((path, source_mtime, target_mtime, metadata)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(directory_actions.len(), 1);
        let (path, source_mtime, target_mtime, metadata) = directory_actions[0];
        assert_eq!(path, "ordner");
        assert_eq!(*source_mtime, Some(1_600_000_000));
        assert_eq!(*target_mtime, *source_mtime);
        assert_eq!(metadata.source, *metadata.target.as_ref().unwrap());
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&Cancel::new(), &mut ignore_progress)
            .unwrap();
        assert_eq!(stats.files_copied, 1);
        assert_eq!(
            stats.files_deleted,
            u64::from(sync_mode == SyncMode::Mirror)
        );
        assert_eq!(read_file(&target, "ordner/neu"), b"neu");
        assert_eq!(
            target.join("ordner/entfernen").exists(),
            sync_mode == SyncMode::Update
        );
        let vfs = LocalVfs::new();
        assert_eq!(
            vfs.stat(&target.join("ordner")).unwrap().mtime,
            Some(1_600_000_000)
        );
        let followup = plan(&vfs, &source, &vfs, &target, &options).unwrap();
        assert!(
            followup.actions.is_empty(),
            "Folgeplan: {:?}",
            followup.actions
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_ordnerabschluss_zaehlt_nur_direkte_nicht_ausgeschlossene_kindaktionen() {
    for sync_mode in [SyncMode::Update, SyncMode::Mirror] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        fs::create_dir_all(source.join("ordner/tief")).unwrap();
        fs::create_dir_all(target.join("ordner/tief")).unwrap();
        write_file(&source, "ordner/ausgeschlossen.txt", b"ausgelassen");
        for root in [&source, &target] {
            set_mtime(root, "ordner", 1_600_000_000);
            set_mtime(root, "ordner/tief", 1_600_000_000);
        }
        let options = SyncOptions {
            mode: sync_mode,
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeStrict,
            exclude: vec!["ordner/ausgeschlossen.txt".into()],
            ..Default::default()
        };
        let vfs = LocalVfs::new();
        let unchanged = plan(&vfs, &source, &vfs, &target, &options).unwrap();
        assert!(unchanged.actions.is_empty());

        write_file(&source, "ordner/tief/neu", b"neu");
        set_mtime(&source, "ordner/tief", 1_600_000_000);
        let changed = plan(&vfs, &source, &vfs, &target, &options).unwrap();
        assert_eq!(changed.copy_count, 1);
        let directory_paths = changed
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::SetDirectoryMetadata { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(directory_paths, vec!["ordner/tief"]);
    }
}
