//! Benannte Ströme an echten Datei- und Ordner-Inodes: Basistausch und getrennte
//! Ordnertransaktionen übertragen vollständige Mengen und bilanzieren Teilwirkungen.
use doppeldecker_core::directory_streams::{
    DirectoryStreamEffect, DirectoryStreamOutcome, DirectoryStreamReport, DirectoryStreamTemp,
    DirectoryStreamTransaction,
};
use doppeldecker_core::{
    execute, plan, Action, BlobSummary, Cancel, Capability, Compare, ExecuteRequest, ForkKind,
    LocalVfs, MetadataCapabilities, MetadataClass, MetadataManifest, MetadataProfile,
    NamedBlobSummary, Progress, Safety, SyncMode, SyncOptions, Vfs,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Cursor, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

type Streams = BTreeMap<Vec<u8>, Vec<u8>>;
type Inode = (u64, u64);
type Store = Arc<Mutex<BTreeMap<Inode, Streams>>>;
struct StreamVfs {
    inner: LocalVfs,
    streams: Store,
    forks: Store,
    capability: Capability,
    observed: bool,
    observe_directories: bool,
    directory_updates: bool,
    directory_failure_at: Option<String>,
    directory_finish_failure: bool,
    main_writes: Arc<AtomicUsize>,
    directory_commits: Arc<AtomicUsize>,
    directory_aborts: Arc<AtomicUsize>,
    corrupt_read: bool,
    fail_write: bool,
    fail_flush: bool,
    extra_temp: bool,
    extra_after_rename: bool,
    collide_temp: bool,
    fail_temp_readback: bool,
    grow_read: bool,
}
impl StreamVfs {
    fn new() -> Self {
        Self {
            inner: LocalVfs::new(),
            streams: Arc::default(),
            forks: Arc::default(),
            capability: Capability::Exact,
            observed: true,
            observe_directories: false,
            directory_updates: false,
            directory_failure_at: None,
            directory_finish_failure: false,
            main_writes: Arc::default(),
            directory_commits: Arc::default(),
            directory_aborts: Arc::default(),
            corrupt_read: false,
            fail_write: false,
            fail_flush: false,
            extra_temp: false,
            extra_after_rename: false,
            collide_temp: false,
            fail_temp_readback: false,
            grow_read: false,
        }
    }
    fn key(path: &Path) -> io::Result<Inode> {
        let m = fs::metadata(path)?;
        Ok((m.dev(), m.ino()))
    }
    fn set(&self, path: &Path, values: &[(&str, &[u8])]) {
        self.streams.lock().unwrap().insert(
            Self::key(path).unwrap(),
            values
                .iter()
                .map(|(n, v)| (n.as_bytes().to_vec(), v.to_vec()))
                .collect(),
        );
    }
    fn get(&self, path: &Path) -> Streams {
        self.streams
            .lock()
            .unwrap()
            .get(&Self::key(path).unwrap())
            .cloned()
            .unwrap_or_default()
    }
    fn temp(path: &Path) -> bool {
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .contains(".doppeldecker-")
    }
}
struct StreamWriter {
    store: Store,
    key: Inode,
    name: Vec<u8>,
    fail_write: bool,
    fail_flush: bool,
}
impl Write for StreamWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fail_write {
            return Err(io::Error::other("Streamwrite injiziert"));
        }
        self.store
            .lock()
            .unwrap()
            .get_mut(&self.key)
            .unwrap()
            .get_mut(&self.name)
            .unwrap()
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.fail_flush {
            Err(io::Error::other("Streamflush injiziert"))
        } else {
            Ok(())
        }
    }
}
fn summaries(streams: &Streams) -> Vec<NamedBlobSummary> {
    streams
        .iter()
        .map(|(name, bytes)| NamedBlobSummary {
            name: name.clone(),
            value: BlobSummary::from_bytes(bytes),
        })
        .collect()
}
// Die Ordnerfixture bindet Inode und vollständige Menge. Stagingbytes bleiben
// bis zum Commit im Speicher; permanente Einzelwirkungen ändern den Inodestore.
struct DirectoryTransaction {
    path: PathBuf,
    key: Inode,
    store: Store,
    expected: Vec<NamedBlobSummary>,
    report: DirectoryStreamReport,
    fail_write: bool,
    fail_flush: bool,
    fail_finish: bool,
    commits: Arc<AtomicUsize>,
    aborts: Arc<AtomicUsize>,
}
impl DirectoryTransaction {
    fn verify(&self) -> io::Result<()> {
        if StreamVfs::key(&self.path)? != self.key {
            return Err(io::Error::other("Gehaltene Ordnerbasis änderte ihre ID"));
        }
        let values = self
            .store
            .lock()
            .unwrap()
            .get(&self.key)
            .cloned()
            .unwrap_or_default();
        if summaries(&values) != self.expected {
            return Err(io::Error::other(
                "Vollständige Ordnerstreammenge änderte sich",
            ));
        }
        Ok(())
    }
    fn completed(&mut self, effect: DirectoryStreamEffect) {
        self.commits.fetch_add(1, Ordering::SeqCst);
        self.report.completed.push(DirectoryStreamOutcome {
            directory: self.path.to_string_lossy().into_owned(),
            effect,
        });
    }
}
struct DirectoryTemp<'a> {
    transaction: &'a mut DirectoryTransaction,
    desired: NamedBlobSummary,
    bytes: Vec<u8>,
}
impl Write for DirectoryTemp<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.transaction.fail_write {
            return Err(io::Error::other("Ordnerstreamwrite injiziert"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.transaction.fail_flush {
            Err(io::Error::other("Ordnerstreamflush injiziert"))
        } else {
            Ok(())
        }
    }
}
impl DirectoryStreamTemp for DirectoryTemp<'_> {
    fn commit(
        self: Box<Self>,
        before: Option<&NamedBlobSummary>,
    ) -> io::Result<DirectoryStreamEffect> {
        self.transaction.verify()?;
        if BlobSummary::from_bytes(&self.bytes) != self.desired.value {
            return Err(io::Error::other("Ordnerstreambytes stimmen nicht"));
        }
        let actual_before = self
            .transaction
            .expected
            .iter()
            .find(|value| value.name == self.desired.name);
        if actual_before != before {
            return Err(io::Error::other("Ordnerstream-Einzelwirkung änderte sich"));
        }
        let effect = match before {
            Some(before) => DirectoryStreamEffect::Replace {
                before: before.clone(),
                after: self.desired.clone(),
            },
            None => DirectoryStreamEffect::Add {
                after: self.desired.clone(),
            },
        };
        self.transaction
            .store
            .lock()
            .unwrap()
            .entry(self.transaction.key)
            .or_default()
            .insert(self.desired.name.clone(), self.bytes.clone());
        self.transaction
            .expected
            .retain(|value| value.name != self.desired.name);
        self.transaction.expected.push(self.desired.clone());
        self.transaction
            .expected
            .sort_by(|a, b| a.name.cmp(&b.name));
        self.transaction.completed(effect.clone());
        Ok(effect)
    }
    fn abort(self: Box<Self>) -> io::Result<()> {
        self.transaction.aborts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
impl DirectoryStreamTransaction for DirectoryTransaction {
    fn stage(
        &mut self,
        desired: &NamedBlobSummary,
    ) -> io::Result<Box<dyn DirectoryStreamTemp + '_>> {
        self.verify()?;
        Ok(Box::new(DirectoryTemp {
            transaction: self,
            desired: desired.clone(),
            bytes: vec![],
        }))
    }
    fn remove(&mut self, before: &NamedBlobSummary) -> io::Result<DirectoryStreamEffect> {
        self.verify()?;
        if !self.expected.contains(before) {
            return Err(io::Error::other("Ordnerstream-Entfernung änderte sich"));
        }
        self.store
            .lock()
            .unwrap()
            .entry(self.key)
            .or_default()
            .remove(&before.name);
        self.expected.retain(|value| value.name != before.name);
        let effect = DirectoryStreamEffect::Remove {
            before: before.clone(),
        };
        self.completed(effect.clone());
        Ok(effect)
    }
    fn report(&self) -> DirectoryStreamReport {
        self.report.clone()
    }
    fn recovery_paths(&self) -> Vec<String> {
        if self.fail_finish {
            vec![self
                .path
                .join("synthetic-stream-intent.recovery.json")
                .to_string_lossy()
                .into_owned()]
        } else {
            Vec::new()
        }
    }
    fn finish(
        self: Box<Self>,
        expected_final: &[NamedBlobSummary],
    ) -> io::Result<DirectoryStreamReport> {
        self.verify()?;
        if self.fail_finish {
            return Err(io::Error::other("Ordnerstreamabschluss injiziert"));
        }
        if self.expected != expected_final {
            return Err(io::Error::other("Ordnerstream-Abschlussmenge weicht ab"));
        }
        Ok(self.report.clone())
    }
}

impl Vfs for StreamVfs {
    fn list(&self, p: &Path) -> io::Result<Vec<doppeldecker_core::Entry>> {
        self.inner.list(p)
    }
    fn list_complete(
        &self,
        p: &Path,
    ) -> io::Result<doppeldecker_core::vfs::CompleteDirectoryListing> {
        self.inner.list_complete(p)
    }
    fn stat(&self, p: &Path) -> io::Result<doppeldecker_core::Entry> {
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
        self.inner.open_read(p)
    }
    fn open_write(&self, p: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.main_writes.fetch_add(1, Ordering::SeqCst);
        self.inner.open_write(p)
    }
    fn open_write_new(&self, p: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.main_writes.fetch_add(1, Ordering::SeqCst);
        let w = self.inner.open_write_new(p)?;
        self.streams
            .lock()
            .unwrap()
            .insert(Self::key(p)?, Streams::new());
        if self.collide_temp {
            self.streams
                .lock()
                .unwrap()
                .get_mut(&Self::key(p)?)
                .unwrap()
                .insert(b"neu".to_vec(), b"fremd".to_vec());
        }
        self.forks
            .lock()
            .unwrap()
            .insert(Self::key(p)?, Streams::new());
        Ok(w)
    }
    fn sync_file(&self, p: &Path) -> io::Result<()> {
        self.inner.sync_file(p)
    }
    fn sync_dir(&self, p: &Path) -> io::Result<()> {
        self.inner.sync_dir(p)
    }
    fn set_mtime(&self, p: &Path, m: i64) -> io::Result<()> {
        self.inner.set_mtime(p, m)
    }
    fn set_mode(&self, p: &Path, m: u32) -> io::Result<()> {
        self.inner.set_mode(p, m)
    }
    fn metadata_capabilities(&self, _: &Path) -> io::Result<MetadataCapabilities> {
        let mut c = MetadataCapabilities::unknown();
        c.owner = Capability::Unsupported;
        c.special_mode_bits = Capability::Unsupported;
        c.acl_model = doppeldecker_core::AclModel::Unsupported;
        c.xattrs = Capability::Unsupported;
        c.resource_fork = Capability::Exact;
        c.named_streams = self.capability;
        c.sparse_extents = Capability::Unsupported;
        c.hardlinks = Capability::Exact;
        Ok(c)
    }
    fn metadata_manifest(&self, p: &Path) -> io::Result<MetadataManifest> {
        let local = self.inner.metadata_manifest(p)?;
        let fork = self
            .forks
            .lock()
            .unwrap()
            .get(&Self::key(p)?)
            .and_then(|streams| streams.get(b"".as_slice()))
            .map(|bytes| BlobSummary::from_bytes(bytes));
        let manifest = MetadataManifest::new(
            local.owner,
            local.mode,
            local.acl,
            local.xattrs,
            fork,
            local.sparse_extents,
            local.hardlink_group,
        )?;
        if self.fail_temp_readback && Self::temp(p) {
            return Err(io::Error::other("Tempreadback injiziert"));
        }
        let mut streams = self.get(p);
        if self.extra_temp && Self::temp(p) {
            streams.insert(b"unerwartet".to_vec(), b"extra".to_vec());
        }
        manifest.with_named_streams(
            if self.observed && (self.observe_directories || !fs::metadata(p)?.is_dir()) {
                Some(
                    streams
                        .into_iter()
                        .map(|(name, value)| NamedBlobSummary {
                            name,
                            value: BlobSummary::from_bytes(&value),
                        })
                        .collect(),
                )
            } else {
                None
            },
        )
    }
    fn open_named_stream_read(&self, p: &Path, n: &[u8]) -> io::Result<Box<dyn Read + Send>> {
        let mut bytes = self
            .get(p)
            .get(n)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Stream fehlt"))?;
        if self.corrupt_read && !bytes.is_empty() {
            bytes[0] ^= 1;
        }
        if self.grow_read {
            bytes.push(1);
        }
        Ok(Box::new(Cursor::new(bytes)))
    }
    fn open_named_stream_write_new(&self, p: &Path, n: &[u8]) -> io::Result<Box<dyn Write + Send>> {
        let key = Self::key(p)?;
        let mut all = self.streams.lock().unwrap();
        let streams = all
            .get_mut(&key)
            .ok_or_else(|| io::Error::other("Basisdatei nicht exklusiv angelegt"))?;
        if streams.contains_key(n) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Stream kollidiert",
            ));
        }
        streams.insert(n.to_vec(), Vec::new());
        Ok(Box::new(StreamWriter {
            store: self.streams.clone(),
            key,
            name: n.to_vec(),
            fail_write: self.fail_write,
            fail_flush: self.fail_flush,
        }))
    }
    fn supports_directory_stream_updates(&self, _: &Path) -> io::Result<bool> {
        Ok(self.directory_updates)
    }
    fn begin_directory_stream_update(
        &self,
        path: &Path,
        expected_base: &doppeldecker_core::sync::SyncTargetProof,
        expected_streams: &[NamedBlobSummary],
    ) -> io::Result<Box<dyn DirectoryStreamTransaction>> {
        if !self.directory_updates || !expected_base.matches_directory_identity(&self.stat(path)?) {
            return Err(io::Error::other("Ordnerbasis-ID änderte sich"));
        }
        let inject = self
            .directory_failure_at
            .as_ref()
            .is_none_or(|name| path.file_name().unwrap() == name.as_str());
        let transaction = DirectoryTransaction {
            path: path.into(),
            key: Self::key(path)?,
            store: self.streams.clone(),
            expected: expected_streams.to_vec(),
            report: DirectoryStreamReport::default(),
            fail_write: inject && self.fail_write,
            fail_flush: inject && self.fail_flush,
            fail_finish: inject && self.directory_finish_failure,
            commits: self.directory_commits.clone(),
            aborts: self.directory_aborts.clone(),
        };
        transaction.verify()?;
        Ok(Box::new(transaction))
    }
    fn open_fork_read(&self, p: &Path, _: ForkKind) -> io::Result<Box<dyn Read + Send>> {
        let bytes = self
            .forks
            .lock()
            .unwrap()
            .get(&Self::key(p)?)
            .and_then(|streams| streams.get(b"".as_slice()))
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Fork fehlt"))?;
        Ok(Box::new(Cursor::new(bytes)))
    }
    fn open_fork_write_new(&self, p: &Path, _: ForkKind) -> io::Result<Box<dyn Write + Send>> {
        let key = Self::key(p)?;
        let mut all = self.forks.lock().unwrap();
        let streams = all
            .get_mut(&key)
            .ok_or_else(|| io::Error::other("Basisdatei nicht exklusiv angelegt"))?;
        if streams.insert(Vec::new(), Vec::new()).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Fork kollidiert",
            ));
        }
        Ok(Box::new(StreamWriter {
            store: self.forks.clone(),
            key,
            name: Vec::new(),
            fail_write: false,
            fail_flush: false,
        }))
    }
    fn create_hard_link_noreplace(&self, existing: &Path, new_path: &Path) -> io::Result<()> {
        self.inner.create_hard_link_noreplace(existing, new_path)
    }
    fn remove_file(&self, p: &Path) -> io::Result<()> {
        let key = Self::key(p)?;
        self.inner.remove_file(p)?;
        self.streams.lock().unwrap().remove(&key);
        self.forks.lock().unwrap().remove(&key);
        Ok(())
    }
    fn remove_dir_all(&self, p: &Path) -> io::Result<()> {
        self.inner.remove_dir_all(p)
    }
    fn rename(&self, a: &Path, b: &Path) -> io::Result<()> {
        self.inner.rename(a, b)?;
        if self.extra_after_rename {
            self.streams
                .lock()
                .unwrap()
                .get_mut(&Self::key(b)?)
                .unwrap()
                .insert(b"nach-commit".to_vec(), b"extra".to_vec());
        }
        Ok(())
    }
    fn rename_noreplace(&self, a: &Path, b: &Path) -> io::Result<()> {
        self.inner.rename_noreplace(a, b)?;
        if self.extra_after_rename {
            self.streams
                .lock()
                .unwrap()
                .get_mut(&Self::key(b)?)
                .unwrap()
                .insert(b"nach-commit".to_vec(), b"extra".to_vec());
        }
        Ok(())
    }
}
fn options() -> SyncOptions {
    SyncOptions {
        mode: SyncMode::Update,
        compare: Compare::ContentHash,
        metadata: MetadataProfile::NativeBestEffort,
        ..Default::default()
    }
}
struct Fixture {
    src: tempfile::TempDir,
    dst: tempfile::TempDir,
    source: StreamVfs,
    target: StreamVfs,
}
impl Fixture {
    fn new() -> Self {
        let root = fs::canonicalize(std::env::temp_dir()).unwrap();
        let src = tempfile::tempdir_in(&root).unwrap();
        let dst = tempfile::tempdir_in(root).unwrap();
        fs::write(src.path().join("a"), b"main").unwrap();
        fs::write(dst.path().join("a"), b"main").unwrap();
        Self {
            src,
            dst,
            source: StreamVfs::new(),
            target: StreamVfs::new(),
        }
    }
    fn directories() -> Self {
        let mut fixture = Self::new();
        fixture.source.observe_directories = true;
        fixture.target.observe_directories = true;
        fixture.target.directory_updates = true;
        fixture
            .source
            .set_mtime(&fixture.source_path(), 1_700_000_000)
            .unwrap();
        fixture
            .target
            .set_mtime(&fixture.target_path(), 1_700_000_000)
            .unwrap();
        fixture
    }
    fn directory(
        &self,
        name: &str,
        existing: bool,
        source: &[(&str, &[u8])],
        target: &[(&str, &[u8])],
    ) -> (PathBuf, PathBuf) {
        let source_path = self.src.path().join(name);
        let target_path = self.dst.path().join(name);
        fs::create_dir(&source_path).unwrap();
        self.source.set(&source_path, source);
        self.source.set_mtime(&source_path, 1_700_000_000).unwrap();
        if existing {
            fs::create_dir(&target_path).unwrap();
            self.target.set(&target_path, target);
            self.target.set_mtime(&target_path, 1_700_000_000).unwrap();
        }
        (source_path, target_path)
    }
    fn preview(&self) -> doppeldecker_core::SyncPlan {
        plan(
            &self.source,
            self.src.path(),
            &self.target,
            self.dst.path(),
            &options(),
        )
        .unwrap()
    }
    fn run(
        &self,
        p: &doppeldecker_core::SyncPlan,
        c: &Cancel,
        cb: &mut dyn FnMut(&Progress),
    ) -> io::Result<doppeldecker_core::SyncStats> {
        execute(
            ExecuteRequest {
                src: &self.source,
                src_root: self.src.path(),
                dst: &self.target,
                dst_root: self.dst.path(),
                plan: p,
                safety: &Safety::headless_test(),
            },
            c,
            cb,
        )
    }
    fn source_path(&self) -> std::path::PathBuf {
        self.src.path().join("a")
    }
    fn target_path(&self) -> std::path::PathBuf {
        self.dst.path().join("a")
    }
    fn assert_old(&self) {
        assert_eq!(fs::read(self.target_path()).unwrap(), b"main");
        assert_eq!(
            self.target
                .get(&self.target_path())
                .get(b"alt".as_slice())
                .unwrap(),
            b"alt"
        );
        assert_eq!(fs::read_dir(self.dst.path()).unwrap().count(), 1);
    }
}
#[test]
fn stream_only_update_unicode_leer_noop_und_bytebilanz() {
    let f = Fixture::new();
    f.source
        .set(&f.source_path(), &[("日本語", b"stream"), ("leer", b"")]);
    f.target.set(&f.target_path(), &[("alt", b"alt")]);
    f.source.forks.lock().unwrap().insert(
        StreamVfs::key(&f.source_path()).unwrap(),
        [(Vec::new(), b"fork".to_vec())].into(),
    );
    let p = f.preview();
    assert!(p.actions.iter().any(|a| matches!(
        a,
        Action::UpdateFile {
            metadata_only: true,
            ..
        }
    )));
    assert_eq!(p.bytes_to_transfer, 14);
    let mut done = 0;
    let stats = f
        .run(&p, &Cancel::new(), &mut |p| done = p.bytes_done)
        .unwrap();
    assert_eq!(done, 14);
    assert_eq!(stats.bytes_copied, 14);
    assert_eq!(
        f.source.get(&f.source_path()),
        f.target.get(&f.target_path())
    );
    assert_eq!(f.preview().update_count, 0);
}
#[test]
fn beobachtete_leere_quelle_entfernt_alte_zielstroeme_durch_basistausch() {
    let f = Fixture::new();
    f.target.set(&f.target_path(), &[("alt", b"alt")]);
    let p = f.preview();
    assert_eq!(p.update_count, 1);
    assert!(p
        .metadata_report
        .classes
        .iter()
        .any(|c| c.class == MetadataClass::NamedStreams && c.transferred == 1));
    f.run(&p, &Cancel::new(), &mut |_| {}).unwrap();
    assert!(f.target.get(&f.target_path()).is_empty());
}
#[test]
fn quell_und_zieldrift_erhalten_altziel() {
    for source_drift in [true, false] {
        let f = Fixture::new();
        f.source.set(&f.source_path(), &[("neu", b"neu")]);
        f.target.set(&f.target_path(), &[("alt", b"alt")]);
        let p = f.preview();
        if source_drift {
            f.source.set(&f.source_path(), &[("neu", b"drift")]);
        } else {
            f.target
                .set(&f.target_path(), &[("alt", b"alt"), ("drift", b"x")]);
        }
        assert!(f.run(&p, &Cancel::new(), &mut |_| {}).is_err());
        f.assert_old();
    }
}
#[test]
fn streamfehler_flush_zusatzstream_readback_und_wachsende_quelle_erhalten_altziel() {
    for failure in 0..6 {
        let mut f = Fixture::new();
        f.source.set(&f.source_path(), &[("neu", b"neu")]);
        f.target.set(&f.target_path(), &[("alt", b"alt")]);
        let p = f.preview();
        match failure {
            0 => f.target.fail_write = true,
            1 => f.target.fail_flush = true,
            2 => f.target.extra_temp = true,
            3 => f.target.fail_temp_readback = true,
            4 => f.source.grow_read = true,
            _ => f.target.collide_temp = true,
        };
        assert!(
            f.run(&p, &Cancel::new(), &mut |_| {}).is_err(),
            "Fehlerfall {failure}"
        );
        f.assert_old();
    }
}
#[test]
fn abbruch_waehrend_streamcopy_erhaelt_altziel() {
    let f = Fixture::new();
    let bytes = vec![8; 1024 * 1024];
    f.source.set(&f.source_path(), &[("gross", &bytes)]);
    f.target.set(&f.target_path(), &[("alt", b"alt")]);
    let p = f.preview();
    let cancel = Cancel::new();
    let result = f
        .run(&p, &cancel, &mut |p| {
            if p.bytes_done > 4 {
                cancel.cancel();
            }
        })
        .unwrap();
    assert!(result.cancelled);
    f.assert_old();
}
#[test]
fn unsupported_ziel_kopiert_inhalt_mit_issue_und_strict_sperre() {
    let mut f = Fixture::new();
    fs::write(f.source_path(), b"neu").unwrap();
    f.source.set(&f.source_path(), &[("neu", b"stream")]);
    f.target.capability = Capability::Unsupported;
    f.target.observed = false;
    let p = f.preview();
    assert!(p
        .metadata_report
        .issues
        .iter()
        .any(|i| i.class == MetadataClass::NamedStreams && i.path == "a"));
    f.run(&p, &Cancel::new(), &mut |_| {}).unwrap();
    assert_eq!(fs::read(f.target_path()).unwrap(), b"neu");
    let mut strict = options();
    strict.metadata = MetadataProfile::NativeStrict;
    assert!(plan(&f.source, f.src.path(), &f.target, f.dst.path(), &strict).is_err());
}
#[test]
fn nicht_schreibbare_dateistroeme_bleiben_beim_noop_bilanziert_und_erhalten() {
    for capability in [
        Capability::ReadOnly,
        Capability::Unsupported,
        Capability::Unknown,
    ] {
        for (source_streams, target_streams, target_observed, omitted) in [
            (
                vec![("quelle", b"neu".as_slice())],
                vec![("ziel", b"alt".as_slice())],
                true,
                true,
            ),
            (vec![("quelle", b"neu".as_slice())], vec![], false, true),
            (vec![], vec![("ziel", b"alt".as_slice())], true, true),
            (
                vec![("gleich", b"gleich".as_slice())],
                vec![("gleich", b"gleich".as_slice())],
                true,
                false,
            ),
            (vec![], vec![], true, false),
            (vec![], vec![], false, false),
        ] {
            let mut f = Fixture::new();
            f.target.capability = capability;
            f.target.observed = target_observed;
            f.source.set(&f.source_path(), &source_streams);
            f.target.set(&f.target_path(), &target_streams);
            f.source.set_mtime(&f.source_path(), 1_700_000_000).unwrap();
            f.target.set_mtime(&f.target_path(), 1_700_000_000).unwrap();
            let before = f.target.get(&f.target_path());
            let preview = f.preview();
            assert!(preview.actions.is_empty());
            assert_eq!(
                (
                    preview.copy_count,
                    preview.update_count,
                    preview.metadata_count,
                    preview.bytes_to_transfer
                ),
                (0, 0, 0, 0)
            );
            let issues: Vec<_> = preview
                .metadata_report
                .issues
                .iter()
                .filter(|issue| issue.path == "a" && issue.class == MetadataClass::NamedStreams)
                .collect();
            assert_eq!(
                issues.len(),
                usize::from(omitted),
                "{capability:?}, observed={target_observed}"
            );
            assert!(issues
                .iter()
                .all(|issue| issue.reason.contains("unverändert")
                    && !issue.reason.contains("verloren")));
            let class = preview
                .metadata_report
                .classes
                .iter()
                .find(|class| class.class == MetadataClass::NamedStreams);
            assert_eq!(class.map_or(0, |class| class.transferred), 0);
            assert_eq!(class.map_or(0, |class| class.omitted), u64::from(omitted));
            assert_eq!(
                preview.metadata_report.unsupported_metadata,
                u64::from(omitted)
            );
            let stats = f.run(&preview, &Cancel::new(), &mut |_| {}).unwrap();
            assert_eq!(
                (
                    stats.files_copied,
                    stats.bytes_copied,
                    stats.metadata_updated
                ),
                (0, 0, 0)
            );
            assert_eq!(stats.metadata_report, preview.metadata_report);
            assert_eq!(f.target.get(&f.target_path()), before);
            assert_eq!(fs::read(f.target_path()).unwrap(), b"main");
        }
    }
}

#[test]
fn unbeobachtete_quelle_bilanziert_bekannten_zielverlust() {
    let mut f = Fixture::new();
    fs::write(f.source_path(), b"neu").unwrap();
    f.source.observed = false;
    f.source.capability = Capability::Unsupported;
    f.target.set(&f.target_path(), &[("alt", b"alt")]);
    let p = f.preview();
    assert!(p.metadata_report.issues.iter().any(
        |i| i.class == MetadataClass::NamedStreams && i.reason.contains("Zielströme verloren")
    ));
    f.run(&p, &Cancel::new(), &mut |_| {}).unwrap();
    assert!(f.target.get(&f.target_path()).is_empty());
}

#[test]
fn drift_waehrend_streamcopy_erhaelt_altziel() {
    for source_drift in [true, false] {
        let f = Fixture::new();
        f.source.set(&f.source_path(), &[("neu", b"neu")]);
        f.target.set(&f.target_path(), &[("alt", b"alt")]);
        let p = f.preview();
        let mut changed = false;
        let result = f.run(&p, &Cancel::new(), &mut |progress| {
            if progress.bytes_done > 4 && !changed {
                changed = true;
                if source_drift {
                    f.source.set(&f.source_path(), &[("neu", b"drift")]);
                } else {
                    f.target
                        .set(&f.target_path(), &[("alt", b"alt"), ("drift", b"extra")]);
                }
            }
        });
        assert!(changed);
        assert!(result.is_err());
        f.assert_old();
    }
}
#[test]
fn add_only_erhaelt_vorhandene_streams_und_kopiert_neue_datei() {
    let f = Fixture::new();
    f.source.set(&f.source_path(), &[("neu", b"neu")]);
    f.target.set(&f.target_path(), &[("alt", b"alt")]);
    let mut opts = options();
    opts.mode = SyncMode::AddOnly;
    let p = plan(&f.source, f.src.path(), &f.target, f.dst.path(), &opts).unwrap();
    assert_eq!(p.update_count, 0);
    f.run(&p, &Cancel::new(), &mut |_| {}).unwrap();
    f.assert_old();
    fs::write(f.src.path().join("b"), b"neu").unwrap();
    f.source
        .set(&f.src.path().join("b"), &[("strom", b"inhalt")]);
    let p = plan(&f.source, f.src.path(), &f.target, f.dst.path(), &opts).unwrap();
    assert_eq!(p.copy_count, 1);
    f.run(&p, &Cancel::new(), &mut |_| {}).unwrap();
    assert_eq!(
        f.target
            .get(&f.dst.path().join("b"))
            .get(b"strom".as_slice())
            .unwrap(),
        b"inhalt"
    );
}
#[test]
fn readback_nach_commit_prueft_weiter_die_vollstaendige_streammenge() {
    let mut f = Fixture::new();
    f.source.set(&f.source_path(), &[("neu", b"neu")]);
    f.target.set(&f.target_path(), &[("alt", b"alt")]);
    let p = f.preview();
    f.target.extra_after_rename = true;
    let error = f.run(&p, &Cancel::new(), &mut |_| {}).unwrap_err();
    assert_eq!(
        doppeldecker_core::error_code(&error),
        doppeldecker_core::ErrorCode::OutcomeUnclear
    );
    assert!(doppeldecker_core::error::recovery_paths(&error)
        .contains(&f.target_path().to_string_lossy().into_owned()));
    assert!(f
        .target
        .get(&f.target_path())
        .contains_key(b"nach-commit".as_slice()));
}

#[test]
fn hardlink_nutzt_gepruefte_streammenge_des_ankers() {
    for drift in [false, true] {
        let f = Fixture::new();
        fs::hard_link(f.source_path(), f.src.path().join("b")).unwrap();
        f.source.set(&f.source_path(), &[("neu", b"neu")]);
        let p = f.preview();
        assert!(p
            .actions
            .iter()
            .any(|a| matches!(a, Action::CreateHardlink { .. })));
        let mut changed = false;
        let result = f.run(&p, &Cancel::new(), &mut |progress| {
            if drift && progress.files_done == 1 && !changed {
                changed = true;
                f.target.set(&f.target_path(), &[("neu", b"drift")]);
            }
        });
        if drift {
            assert!(changed);
            assert!(result.is_err());
            assert!(!f.dst.path().join("b").exists());
        } else {
            result.unwrap();
            assert_eq!(
                StreamVfs::key(&f.target_path()).unwrap(),
                StreamVfs::key(&f.dst.path().join("b")).unwrap()
            );
            assert_eq!(
                f.source.get(&f.source_path()),
                f.target.get(&f.dst.path().join("b"))
            );
        }
    }
}

#[test]
fn verzeichnisstroeme_werden_auch_beim_inhaltlichen_noop_bilanziert() {
    for (source_streams, target_streams, omitted) in [
        (vec![("quelle", b"eins".as_slice())], vec![], true),
        (vec![], vec![("ziel", b"erhalten".as_slice())], true),
        (
            vec![("gleich", b"gleich".as_slice())],
            vec![("gleich", b"gleich".as_slice())],
            false,
        ),
        (vec![], vec![], false),
    ] {
        let mut f = Fixture::new();
        f.source.observe_directories = true;
        f.target.observe_directories = true;
        let source = f.src.path().join("Ordner");
        let target = f.dst.path().join("Ordner");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        f.source.set(&source, &source_streams);
        f.target.set(&target, &target_streams);
        f.source.set_mtime(&source, 1_700_000_000).unwrap();
        f.target.set_mtime(&target, 1_700_000_000).unwrap();
        let before = f.target.get(&target);
        let preview = f.preview();
        assert!(preview.actions.is_empty());
        let issue = preview
            .metadata_report
            .issues
            .iter()
            .find(|issue| issue.path == "Ordner" && issue.class == MetadataClass::NamedStreams);
        assert_eq!(issue.is_some(), omitted);
        let count = preview
            .metadata_report
            .classes
            .iter()
            .find(|class| class.class == MetadataClass::NamedStreams);
        assert_eq!(count.map_or(0, |class| class.transferred), 0);
        assert_eq!(count.map_or(0, |class| class.omitted), u64::from(omitted));
        assert_eq!(
            preview.metadata_report.unsupported_metadata,
            u64::from(omitted)
        );
        if let Some(issue) = issue {
            assert!(issue.reason.contains("Verzeichnis"));
            assert!(!issue.reason.contains("verloren"));
        }
        let stats = f.run(&preview, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(
            (
                stats.files_copied,
                stats.bytes_copied,
                stats.metadata_updated
            ),
            (0, 0, 0)
        );
        assert_eq!(stats.metadata_report, preview.metadata_report);
        assert_eq!(f.target.get(&target), before);
    }
}

#[test]
fn neue_und_geaenderte_ordner_bilanzieren_streams_ohne_zielverlustbehauptung() {
    for existing in [false, true] {
        let mut f = Fixture::new();
        f.source.observe_directories = true;
        f.target.observe_directories = true;
        let source = f.src.path().join("Ordner");
        let target = f.dst.path().join("Ordner");
        fs::create_dir(&source).unwrap();
        f.source.set(&source, &[("quelle", b"eins")]);
        f.source.set_mtime(&source, 1_700_000_000).unwrap();
        if existing {
            fs::create_dir(&target).unwrap();
            f.target.set(&target, &[("ziel", b"behalten")]);
            f.target.set_mtime(&target, 1_600_000_000).unwrap();
        }
        let before = if existing {
            Some(f.target.get(&target))
        } else {
            None
        };
        let preview = f.preview();
        let issue = preview
            .metadata_report
            .issues
            .iter()
            .find(|issue| issue.path == "Ordner" && issue.class == MetadataClass::NamedStreams)
            .unwrap();
        assert!(issue.reason.contains("Verzeichnis"));
        assert!(!issue.reason.contains("verloren"));
        f.run(&preview, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(f.target.get(&target), before.unwrap_or_default());
        assert_eq!(
            f.source.get(&source).get(b"quelle".as_slice()).unwrap(),
            b"eins"
        );
        let mut strict = options();
        strict.metadata = MetadataProfile::NativeStrict;
        assert!(plan(&f.source, f.src.path(), &f.target, f.dst.path(), &strict).is_err());
    }
}

#[test]
fn unbeobachtete_verzeichnisquelle_behauptet_keinen_verlust_erhaltener_zielstroeme() {
    let mut f = Fixture::new();
    f.target.observe_directories = true;
    let source = f.src.path().join("Ordner");
    let target = f.dst.path().join("Ordner");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    f.target.set(&target, &[("ziel", b"behalten")]);
    f.source.set_mtime(&source, 1_700_000_000).unwrap();
    f.target.set_mtime(&target, 1_600_000_000).unwrap();
    let before = f.target.get(&target);
    let preview = f.preview();
    let issue = preview
        .metadata_report
        .issues
        .iter()
        .find(|issue| issue.path == "Ordner" && issue.class == MetadataClass::NamedStreams)
        .unwrap();
    assert!(issue.reason.contains("nicht vollständig beobachtet"));
    assert!(!issue.reason.contains("verloren"));
    f.run(&preview, &Cancel::new(), &mut |_| {}).unwrap();
    assert_eq!(f.target.get(&target), before);
}

#[test]
fn strict_verlangt_beobachtung_lesbar_angekuendigter_stroeme_auch_beim_noop() {
    for missing_source in [false, true] {
        let mut f = Fixture::new();
        f.source.observed = !missing_source;
        f.target.observed = missing_source;
        if missing_source {
            assert!(f.preview().actions.is_empty());
        }
        let mut strict = options();
        strict.metadata = MetadataProfile::NativeStrict;
        let error = plan(&f.source, f.src.path(), &f.target, f.dst.path(), &strict).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }
}

#[test]
fn unbeobachtete_verzeichnisstroeme_bleiben_auch_beim_noop_sichtbar() {
    let mut f = Fixture::new();
    f.target.observe_directories = true;
    let source = f.src.path().join("Ordner");
    let target = f.dst.path().join("Ordner");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    f.source.set_mtime(&source, 1_700_000_000).unwrap();
    f.target.set_mtime(&target, 1_700_000_000).unwrap();
    let preview = f.preview();
    assert!(preview.actions.is_empty());
    let issue = preview
        .metadata_report
        .issues
        .iter()
        .find(|issue| issue.path == "Ordner" && issue.class == MetadataClass::NamedStreams)
        .unwrap();
    assert!(issue.reason.contains("nicht vollständig beobachtet"));
    assert_eq!(preview.metadata_report.unsupported_metadata, 1);
    let stats = f.run(&preview, &Cancel::new(), &mut |_| {}).unwrap();
    assert_eq!(
        (
            stats.files_copied,
            stats.bytes_copied,
            stats.metadata_updated
        ),
        (0, 0, 0)
    );
    assert_eq!(stats.metadata_report, preview.metadata_report);
}

#[test]
fn verzeichniswriter_add_only_neuer_ordner_kopiert_unicode_und_leere_stroeme() {
    let f = Fixture::directories();
    let (source, target) = f.directory(
        "Ordner",
        false,
        &[("日本語", b"stream"), ("leer", b"")],
        &[],
    );
    let mut opts = options();
    opts.mode = SyncMode::AddOnly;
    let preview = plan(&f.source, f.src.path(), &f.target, f.dst.path(), &opts).unwrap();
    assert_eq!(preview.bytes_to_transfer, 6);
    assert!(preview.actions.iter().any(|action| matches!(action, Action::CreateDir { metadata: Some(metadata), .. } if metadata.directory_streams.as_ref().is_some_and(|streams| streams.effects.len() == 2))));
    let mut done = 0;
    let stats = f
        .run(&preview, &Cancel::new(), &mut |progress| {
            done = progress.bytes_done
        })
        .unwrap();
    assert_eq!(f.target.get(&target), f.source.get(&source));
    assert_eq!((done, stats.bytes_copied, stats.files_copied), (6, 6, 0));
    assert_eq!(stats.directory_stream_report.completed.len(), 2);
    assert!(stats.directory_stream_report.uncertain.is_empty());
    assert_eq!(f.target.main_writes.load(Ordering::SeqCst), 0);
    let next = plan(&f.source, f.src.path(), &f.target, f.dst.path(), &opts).unwrap();
    assert!(next.actions.is_empty());
    assert_eq!(next.bytes_to_transfer, 0);
}

#[test]
fn verzeichniswriter_update_add_replace_remove_und_folgelauf_noop() {
    let f = Fixture::directories();
    let (source, target) = f.directory(
        "Ordner",
        true,
        &[("add", b"new"), ("gleich", b"same"), ("replace", b"fresh")],
        &[
            ("remove", b"old"),
            ("gleich", b"same"),
            ("replace", b"stale"),
        ],
    );
    let inode = StreamVfs::key(&target).unwrap();
    let preview = f.preview();
    assert_eq!(preview.bytes_to_transfer, 8);
    assert!(preview.actions.iter().any(|action| matches!(action, Action::SetDirectoryMetadata { metadata, .. } if !metadata.named_streams && metadata.directory_streams.as_ref().is_some_and(|streams| streams.effects.len() == 3 && streams.target_proof.is_some()))));
    let mut progress = Progress::default();
    let stats = f
        .run(&preview, &Cancel::new(), &mut |current| {
            progress = current.clone()
        })
        .unwrap();
    assert_eq!(f.target.get(&target), f.source.get(&source));
    assert_eq!(StreamVfs::key(&target).unwrap(), inode);
    assert_eq!(
        (stats.bytes_copied, progress.bytes_done, stats.files_copied),
        (8, 8, 0)
    );
    assert_eq!(stats.directory_stream_report.completed.len(), 3);
    assert_eq!(
        progress.directory_stream_report,
        stats.directory_stream_report
    );
    for kind in [0, 1, 2] {
        assert!(stats
            .directory_stream_report
            .completed
            .iter()
            .any(|outcome| match kind {
                0 => matches!(outcome.effect, DirectoryStreamEffect::Add { .. }),
                1 => matches!(outcome.effect, DirectoryStreamEffect::Replace { .. }),
                _ => matches!(outcome.effect, DirectoryStreamEffect::Remove { .. }),
            }));
    }
    assert_eq!(f.target.main_writes.load(Ordering::SeqCst), 0);
    let next = f.preview();
    assert!(
        next.actions.is_empty(),
        "Folgelauf muss ohne Metadatenrest auskommen: {:?}",
        next.actions
    );
    assert_eq!(next.bytes_to_transfer, 0);
    let next_stats = f.run(&next, &Cancel::new(), &mut |_| {}).unwrap();
    assert!(next_stats.directory_stream_report.is_empty());
    assert_eq!(f.target.directory_commits.load(Ordering::SeqCst), 3);
}

#[test]
fn verzeichniswriter_add_only_laesst_vorhandenen_ordner_unveraendert() {
    let f = Fixture::directories();
    let (_, target) = f.directory("Ordner", true, &[("quelle", b"new")], &[("ziel", b"old")]);
    let before = f.target.get(&target);
    let mut opts = options();
    opts.mode = SyncMode::AddOnly;
    let preview = plan(&f.source, f.src.path(), &f.target, f.dst.path(), &opts).unwrap();
    assert!(preview.actions.is_empty());
    assert_eq!(preview.bytes_to_transfer, 0);
    let stats = f.run(&preview, &Cancel::new(), &mut |_| {}).unwrap();
    assert_eq!(f.target.get(&target), before);
    assert!(stats.directory_stream_report.is_empty());
    assert_eq!(f.target.directory_commits.load(Ordering::SeqCst), 0);
}

#[test]
fn verzeichniswriter_kopiert_alle_ordner_vor_entfernungen_und_erhaelt_teilbericht() {
    for flush_failure in [false, true] {
        let mut f = Fixture::directories();
        let (_, target_a) =
            f.directory("a-Ordner", true, &[("neu", b"new")], &[("extra", b"keep")]);
        let (_, target_b) = f.directory(
            "b-Ordner",
            true,
            &[("ersetzen", b"fresh")],
            &[("ersetzen", b"old")],
        );
        let preview = f.preview();
        f.target.directory_failure_at = Some("b-Ordner".into());
        f.target.fail_write = !flush_failure;
        f.target.fail_flush = flush_failure;
        let error = f.run(&preview, &Cancel::new(), &mut |_| {}).unwrap_err();
        let report = doppeldecker_core::error::directory_stream_report(&error);
        assert_eq!(
            report.completed.len(),
            1,
            "Erste permanente Wirkung fehlt: {error}"
        );
        assert!(
            matches!(&report.completed[0].effect, DirectoryStreamEffect::Add { after } if after.name == b"neu")
        );
        assert_eq!(report.completed[0].directory, target_a.to_string_lossy());
        assert!(report.uncertain.is_empty());
        assert_eq!(
            f.target.get(&target_a),
            [
                (b"extra".to_vec(), b"keep".to_vec()),
                (b"neu".to_vec(), b"new".to_vec())
            ]
            .into()
        );
        assert_eq!(
            f.target.get(&target_b),
            [(b"ersetzen".to_vec(), b"old".to_vec())].into()
        );
        assert_eq!(f.target.directory_commits.load(Ordering::SeqCst), 1);
        assert_eq!(f.target.directory_aborts.load(Ordering::SeqCst), 1);
        assert_eq!(f.target.main_writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn verzeichniswriter_cancel_vor_commit_erhaelt_eigene_zielstroeme() {
    let f = Fixture::directories();
    let bytes = vec![8; 1024 * 1024];
    let (_, target) = f.directory("Ordner", true, &[("gross", &bytes)], &[("alt", b"keep")]);
    let before = f.target.get(&target);
    let preview = f.preview();
    let cancel = Cancel::new();
    let stats = f
        .run(&preview, &cancel, &mut |progress| {
            if progress.bytes_done > 0 {
                cancel.cancel();
            }
        })
        .unwrap();
    assert!(stats.cancelled);
    assert_eq!(f.target.get(&target), before);
    assert!(stats.directory_stream_report.is_empty());
    assert_eq!(f.target.directory_commits.load(Ordering::SeqCst), 0);
    assert_eq!(f.target.directory_aborts.load(Ordering::SeqCst), 1);
}

#[test]
fn verzeichniswriter_quell_ziel_und_basisid_drift_stoppt_vor_permanenter_wirkung() {
    for drift in 0..3 {
        let f = Fixture::directories();
        let (source, target) = f.directory("Ordner", true, &[("neu", b"new")], &[("alt", b"keep")]);
        let preview = f.preview();
        let parked = tempfile::tempdir().unwrap();
        match drift {
            0 => f.source.set(&source, &[("neu", b"changed")]),
            1 => f
                .target
                .set(&target, &[("alt", b"keep"), ("fremd", b"external")]),
            _ => {
                fs::rename(&target, parked.path().join("urspruenglich")).unwrap();
                fs::create_dir(&target).unwrap();
                f.target.set(&target, &[("alt", b"keep")]);
                f.target.set_mtime(&target, 1_700_000_000).unwrap();
            }
        }
        let before = f.target.get(&target);
        assert!(
            f.run(&preview, &Cancel::new(), &mut |_| {}).is_err(),
            "Driftfall {drift}"
        );
        assert_eq!(f.target.get(&target), before);
        assert_eq!(f.target.directory_commits.load(Ordering::SeqCst), 0);
        assert_eq!(f.target.main_writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn verzeichniswriter_planmanipulation_und_falsche_bytes_bleiben_vor_mutation() {
    for tamper in 0..5 {
        let mut f = Fixture::directories();
        let (_, target) = f.directory("Ordner", true, &[("neu", b"new")], &[("alt", b"keep")]);
        let mut preview = f.preview();
        if tamper == 1 {
            preview.bytes_to_transfer += 1;
        } else if tamper == 4 {
            f.source.corrupt_read = true;
        } else {
            let metadata = preview
                .actions
                .iter_mut()
                .find_map(|action| match action {
                    Action::SetDirectoryMetadata { metadata, .. } => Some(metadata),
                    _ => None,
                })
                .unwrap();
            match tamper {
                0 => metadata.directory_streams.as_mut().unwrap().effects.clear(),
                2 => metadata.named_streams = true,
                _ => metadata.directory_streams.as_mut().unwrap().target_proof = None,
            }
        }
        let before = f.target.get(&target);
        assert!(
            f.run(&preview, &Cancel::new(), &mut |_| {}).is_err(),
            "Manipulationsfall {tamper}"
        );
        assert_eq!(f.target.get(&target), before);
        assert_eq!(f.target.directory_commits.load(Ordering::SeqCst), 0);
        assert_eq!(f.target.main_writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn verzeichniswriter_beobachtete_leere_quelle_entfernt_exakt_in_update() {
    let f = Fixture::directories();
    let (_, target) = f.directory("Ordner", true, &[], &[("eins", b"old"), ("leer", b"")]);
    let inode = StreamVfs::key(&target).unwrap();
    let opts = options();
    let preview = plan(&f.source, f.src.path(), &f.target, f.dst.path(), &opts).unwrap();
    assert_eq!(preview.bytes_to_transfer, 0);
    let stats = f.run(&preview, &Cancel::new(), &mut |_| {}).unwrap();
    assert!(f.target.get(&target).is_empty());
    assert_eq!(StreamVfs::key(&target).unwrap(), inode);
    assert_eq!((stats.bytes_copied, stats.files_copied), (0, 0));
    assert_eq!(stats.directory_stream_report.completed.len(), 2);
    assert!(stats
        .directory_stream_report
        .completed
        .iter()
        .all(|outcome| matches!(outcome.effect, DirectoryStreamEffect::Remove { .. })));
    assert_eq!(f.target.main_writes.load(Ordering::SeqCst), 0);
    let next = plan(&f.source, f.src.path(), &f.target, f.dst.path(), &opts).unwrap();
    assert!(next.actions.is_empty());
}

#[test]
fn verzeichniswriter_faehigkeit_erteilt_portable_und_strict_keine_freigabe() {
    let f = Fixture::directories();
    let (_, target) = f.directory("Ordner", true, &[("quelle", b"new")], &[("ziel", b"keep")]);
    let before = f.target.get(&target);
    let mut opts = options();
    opts.metadata = MetadataProfile::Portable;
    let preview = plan(&f.source, f.src.path(), &f.target, f.dst.path(), &opts).unwrap();
    assert!(preview.actions.is_empty());
    let stats = f.run(&preview, &Cancel::new(), &mut |_| {}).unwrap();
    assert!(stats.directory_stream_report.is_empty());
    assert_eq!(f.target.get(&target), before);
    opts.metadata = MetadataProfile::NativeBestEffort;
    opts.mode = SyncMode::Mirror;
    let error = plan(&f.source, f.src.path(), &f.target, f.dst.path(), &opts).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    opts.mode = SyncMode::Update;
    opts.metadata = MetadataProfile::NativeStrict;
    assert!(plan(&f.source, f.src.path(), &f.target, f.dst.path(), &opts).is_err());
    assert_eq!(f.target.directory_commits.load(Ordering::SeqCst), 0);
    assert_eq!(f.target.main_writes.load(Ordering::SeqCst), 0);
}

#[test]
fn verzeichniswriter_quelldrift_nach_streamcopy_bricht_temp_vor_commit_ab() {
    let f = Fixture::directories();
    let (source, target) = f.directory("Ordner", true, &[("neu", b"new")], &[("alt", b"keep")]);
    let before = f.target.get(&target);
    let preview = f.preview();
    let mut changed = false;
    let error = f
        .run(&preview, &Cancel::new(), &mut |progress| {
            if progress.bytes_done > 0 && !changed {
                changed = true;
                f.source.set(&source, &[("neu", b"changed")]);
            }
        })
        .unwrap_err();
    assert!(changed);
    assert_eq!(f.target.get(&target), before);
    assert!(doppeldecker_core::error::directory_stream_report(&error).is_empty());
    assert_eq!(f.target.directory_commits.load(Ordering::SeqCst), 0);
    assert_eq!(f.target.directory_aborts.load(Ordering::SeqCst), 1);
}

#[test]
fn verzeichniswriter_roher_abschlussfehler_erhaelt_addwirkung_und_recoveryhinweis() {
    let mut f = Fixture::directories();
    let (source, target) = f.directory("Ordner", true, &[("neu", b"new")], &[]);
    let preview = f.preview();
    f.target.directory_finish_failure = true;
    let error = f.run(&preview, &Cancel::new(), &mut |_| {}).unwrap_err();
    let report = doppeldecker_core::error::directory_stream_report(&error);
    assert_eq!(report.completed.len(), 1);
    assert!(
        matches!(&report.completed[0].effect, DirectoryStreamEffect::Add { after } if after.name == b"neu")
    );
    assert_eq!(report.completed[0].directory, target.to_string_lossy());
    assert!(report.uncertain.is_empty());
    let hint = target
        .join("synthetic-stream-intent.recovery.json")
        .to_string_lossy()
        .into_owned();
    assert!(doppeldecker_core::error::recovery_paths(&error).contains(&hint));
    assert_eq!(f.target.get(&target), f.source.get(&source));
    assert_eq!(f.target.directory_commits.load(Ordering::SeqCst), 1);
}
