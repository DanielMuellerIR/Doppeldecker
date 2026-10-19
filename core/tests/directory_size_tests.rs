use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use doppeldecker_core::vfs::CompleteDirectoryListing;
use doppeldecker_core::{
    Cancel, DirectorySizeIssueKind, DirectorySizeMode, DirectorySizeSession, DirectorySizeStatus,
    Entry, ErrorCode, LocalVfs, Vfs,
};

#[derive(Default)]
struct ConcurrencyProbe {
    active: AtomicUsize,
    maximum: AtomicUsize,
    changed: Condvar,
    lock: Mutex<()>,
}

#[derive(Default)]
struct FirstListingGate {
    state: Mutex<FirstListingState>,
    changed: Condvar,
}

#[derive(Default)]
struct FirstListingState {
    claimed: bool,
    entered: bool,
    released: bool,
}

impl FirstListingGate {
    fn block_first_after_listing(&self) {
        let mut state = self.state.lock().unwrap();
        if state.claimed {
            return;
        }
        state.claimed = true;
        state.entered = true;
        self.changed.notify_all();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(5), |state| !state.released)
            .unwrap();
        assert!(
            state.released,
            "Test hat den ersten Größenlauf nicht wieder freigegeben"
        );
    }

    fn wait_until_entered(&self) {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| !state.entered)
            .unwrap();
        assert!(
            state.entered,
            "erster Größenlauf erreichte das Testgate nicht"
        );
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.released = true;
        self.changed.notify_all();
    }
}

impl ConcurrencyProbe {
    fn enter(&self) {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        self.changed.notify_all();
        let guard = self.lock.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(2), |_| {
                self.active.load(Ordering::SeqCst) < 2
            })
            .unwrap();
    }

    fn leave(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.changed.notify_all();
    }
}

/// Lokaler Testprovider mit gezielten Leseinstrumenten. Alle Mutationen delegieren
/// unverändert; der Größen-Walker selbst verwendet ausschließlich `list_complete`.
struct InstrumentedVfs {
    inner: LocalVfs,
    fail_path: Option<PathBuf>,
    cancel_on: Option<PathBuf>,
    cancel: Cancel,
    concurrent_paths: HashSet<PathBuf>,
    concurrency: Arc<ConcurrencyProbe>,
    list_calls: AtomicU64,
    block_first_path: Option<PathBuf>,
    first_listing_gate: Arc<FirstListingGate>,
    /// Stellt einen Provider nach, der die Vollständigkeit seines Listings nicht
    /// bestätigt — der Default von `Vfs::list_complete`.
    incomplete_path: Option<PathBuf>,
}

impl InstrumentedVfs {
    fn plain() -> Self {
        Self {
            inner: LocalVfs::new(),
            fail_path: None,
            cancel_on: None,
            cancel: Cancel::new(),
            concurrent_paths: HashSet::new(),
            concurrency: Arc::new(ConcurrencyProbe::default()),
            list_calls: AtomicU64::new(0),
            block_first_path: None,
            first_listing_gate: Arc::new(FirstListingGate::default()),
            incomplete_path: None,
        }
    }
}

impl Vfs for InstrumentedVfs {
    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        self.inner.list(path)
    }

    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_path.as_deref() == Some(path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "künstlich nicht lesbar",
            ));
        }
        let concurrent = self.concurrent_paths.contains(path);
        if concurrent {
            self.concurrency.enter();
        }
        let result = self.inner.list_complete(path);
        if self.block_first_path.as_deref() == Some(path) {
            self.first_listing_gate.block_first_after_listing();
        }
        if self.cancel_on.as_deref() == Some(path) {
            self.cancel.cancel();
        }
        if concurrent {
            self.concurrency.leave();
        }
        if self.incomplete_path.as_deref() == Some(path) {
            return result.map(|mut listing| {
                listing.complete = false;
                listing
            });
        }
        result
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
        self.inner.open_write_new(path)
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_file(path)
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
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
        self.inner.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename_noreplace(from, to)
    }
}

#[test]
fn summiert_logische_dateibytes_und_cache_bleibt_bis_zur_neuberechnung() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("root");
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::write(root.join("oben"), b"12").unwrap();
    std::fs::write(root.join("a/datei"), b"123").unwrap();
    std::fs::write(root.join("b/datei"), b"1234").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("a"), root.join("link")).unwrap();

    let vfs = Arc::new(InstrumentedVfs::plain());
    let session = DirectorySizeSession::with_parallelism(vfs.clone(), 2);
    let mut first = Vec::new();
    let stats = session.measure(
        std::slice::from_ref(&root),
        DirectorySizeMode::UseCache,
        &Cancel::new(),
        &mut |result| first.push(result.clone()),
    );
    assert_eq!(stats.requested, 1);
    assert_eq!(stats.complete, 1, "{first:?}");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].status, DirectorySizeStatus::Complete);
    assert_eq!(first[0].bytes, 9);
    assert_eq!(first[0].files, 3);
    assert_eq!(first[0].directories, 3);
    #[cfg(unix)]
    assert_eq!(first[0].skipped, 1);
    let calls = vfs.list_calls.load(Ordering::SeqCst);

    // Eine tiefere Änderung verändert die mtime des Roots nicht zuverlässig. Der
    // Sitzungscache bleibt deshalb ehrlich stabil, bis der Nutzer neu berechnen lässt.
    std::fs::write(root.join("a/datei"), vec![0_u8; 100]).unwrap();
    let mut cached = Vec::new();
    let stats = session.measure(
        &[root.clone(), root.clone()],
        DirectorySizeMode::UseCache,
        &Cancel::new(),
        &mut |result| cached.push(result.clone()),
    );
    assert_eq!(
        stats.requested, 1,
        "doppelte sichtbare Ordner werden zusammengeführt"
    );
    assert_eq!(stats.cached, 1);
    assert_eq!(cached[0].bytes, 9);
    assert!(cached[0].cached);
    assert_eq!(vfs.list_calls.load(Ordering::SeqCst), calls);

    let mut recalculated = Vec::new();
    session.measure(
        std::slice::from_ref(&root),
        DirectorySizeMode::Recalculate,
        &Cancel::new(),
        &mut |result| recalculated.push(result.clone()),
    );
    assert_eq!(recalculated[0].bytes, 106);
    assert!(!recalculated[0].cached);
    assert_eq!(session.cached(&root).unwrap().bytes, 106);
}

#[test]
fn aelterer_parallel_lauf_ueberschreibt_keine_neuberechnung_im_cache() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("root");
    std::fs::create_dir(&root).unwrap();
    let file = root.join("datei");
    std::fs::write(&file, b"alt").unwrap();

    let mut vfs = InstrumentedVfs::plain();
    vfs.block_first_path = Some(root.clone());
    let gate = Arc::clone(&vfs.first_listing_gate);
    let session = Arc::new(DirectorySizeSession::with_parallelism(Arc::new(vfs), 1));
    let expected = b"deutlich neuer".len() as u64;

    std::thread::scope(|scope| {
        let first_session = Arc::clone(&session);
        let first_root = root.clone();
        let first = scope.spawn(move || {
            let mut results = Vec::new();
            first_session.measure(
                std::slice::from_ref(&first_root),
                DirectorySizeMode::Recalculate,
                &Cancel::new(),
                &mut |result| results.push(result.clone()),
            );
            results.remove(0)
        });

        gate.wait_until_entered();
        let new_content = b"deutlich neuer";
        std::fs::write(&file, new_content).unwrap();
        let mut refreshed = Vec::new();
        session.measure(
            std::slice::from_ref(&root),
            DirectorySizeMode::Recalculate,
            &Cancel::new(),
            &mut |result| refreshed.push(result.clone()),
        );
        gate.release();
        let older = first.join().unwrap();
        assert_eq!(refreshed[0].bytes, expected);
        assert_eq!(session.cached(&root).unwrap().bytes, expected);
        assert_eq!(
            older.bytes, 3,
            "der ältere Lauf behält sein eigenes Ergebnis"
        );
    });

    assert_eq!(
        session.cached(&root).unwrap().bytes,
        expected,
        "der später beendete alte Lauf darf den Cache nicht zurückdrehen"
    );
}

#[test]
fn neuberechnung_mit_fehler_reaktiviert_keinen_alten_cache() {
    let area = tempfile::tempdir().unwrap();
    let base = area.path().canonicalize().unwrap();
    let root = base.join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Datei"), b"alt").unwrap();
    let session = DirectorySizeSession::new(Arc::new(LocalVfs::new()));
    let measure = |mode| {
        let mut results = Vec::new();
        session.measure(
            std::slice::from_ref(&root),
            mode,
            &Cancel::new(),
            &mut |r| results.push(r.clone()),
        );
        results.remove(0)
    };
    assert_eq!(measure(DirectorySizeMode::UseCache).bytes, 3);
    std::fs::rename(&root, base.join("alter-root")).unwrap();
    assert_eq!(
        measure(DirectorySizeMode::Recalculate).status,
        DirectorySizeStatus::Failed
    );
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Datei"), b"neuer Inhalt").unwrap();
    let fresh = measure(DirectorySizeMode::UseCache);
    assert_eq!(fresh.status, DirectorySizeStatus::Complete);
    assert_eq!(fresh.bytes, b"neuer Inhalt".len() as u64);
    assert!(!fresh.cached);
    assert_eq!(
        std::fs::read(base.join("alter-root/Datei")).unwrap(),
        b"alt"
    );
}

#[test]
fn verteilt_unterordner_eines_einzelnen_roots_parallel() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("root");
    let left = root.join("a");
    let right = root.join("b");
    std::fs::create_dir_all(&left).unwrap();
    std::fs::create_dir_all(&right).unwrap();

    let mut vfs = InstrumentedVfs::plain();
    vfs.concurrent_paths = HashSet::from([left, right]);
    let probe = Arc::clone(&vfs.concurrency);
    let session = DirectorySizeSession::with_parallelism(Arc::new(vfs), 2);
    let mut results = Vec::new();
    session.measure(
        &[root],
        DirectorySizeMode::Recalculate,
        &Cancel::new(),
        &mut |result| results.push(result.clone()),
    );

    assert_eq!(
        results[0].status,
        DirectorySizeStatus::Complete,
        "{results:?}"
    );
    assert!(
        probe.maximum.load(Ordering::SeqCst) >= 2,
        "die beiden Unterordner wurden nicht gleichzeitig gelistet"
    );
}

#[test]
fn teilbaumfehler_bleibt_am_genauen_ordner_sichtbar_und_wird_nicht_gecacht() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("root");
    let broken = root.join("kaputt");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(root.join("lesbar"), b"123").unwrap();
    std::fs::write(broken.join("unsichtbar"), b"12345").unwrap();

    let mut vfs = InstrumentedVfs::plain();
    vfs.fail_path = Some(broken.clone());
    let session = DirectorySizeSession::with_parallelism(Arc::new(vfs), 2);
    let mut results = Vec::new();
    let stats = session.measure(
        std::slice::from_ref(&root),
        DirectorySizeMode::UseCache,
        &Cancel::new(),
        &mut |result| results.push(result.clone()),
    );

    assert_eq!(stats.partial, 1, "{results:?}");
    assert_eq!(results[0].status, DirectorySizeStatus::Partial);
    assert_eq!(results[0].bytes, 3);
    assert_eq!(results[0].issues.len(), 1);
    assert_eq!(results[0].issues[0].path, broken);
    assert_eq!(results[0].issues[0].kind, DirectorySizeIssueKind::Listing);
    assert_eq!(results[0].issues[0].code, ErrorCode::PermissionDenied);
    assert!(session.cached(&root).is_none());
}

/// Ein Provider, der die Vollständigkeit seines Listings nicht bestätigt, macht die
/// Bilanz unvollständig — auch ohne einen einzigen Fehler.
///
/// Genau dieser Zweig war ungetestet: `LocalVfs` und alle vier Provider überschreiben
/// `list_complete` mit `complete: true`, nur die Default-Implementierung des Traits
/// liefert `false`. Ein neuer Provider, der sie erbt, wäre also erstmals in
/// Produktion durch diesen Zweig gelaufen — und eine zu kleine Zahl hätte als
/// vollständig gegolten und wäre gecacht worden.
#[test]
fn unbestaetigte_vollstaendigkeit_macht_die_bilanz_teilweise_und_wird_nicht_gecacht() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("root");
    let unterordner = root.join("unterordner");
    std::fs::create_dir_all(&unterordner).unwrap();
    std::fs::write(root.join("lesbar"), b"123").unwrap();
    std::fs::write(unterordner.join("kind"), b"12345").unwrap();

    let mut vfs = InstrumentedVfs::plain();
    vfs.incomplete_path = Some(unterordner.clone());
    let session = DirectorySizeSession::with_parallelism(Arc::new(vfs), 2);
    let mut results = Vec::new();
    let stats = session.measure(
        std::slice::from_ref(&root),
        DirectorySizeMode::UseCache,
        &Cancel::new(),
        &mut |result| results.push(result.clone()),
    );

    assert_eq!(stats.partial, 1, "{results:?}");
    assert_eq!(results[0].status, DirectorySizeStatus::Partial);
    // Die gelesenen Bytes bleiben erhalten; nur die Vollständigkeit fehlt.
    assert_eq!(results[0].bytes, 8);
    assert_eq!(results[0].issues.len(), 1);
    assert_eq!(results[0].issues[0].path, unterordner);
    assert_eq!(
        results[0].issues[0].kind,
        DirectorySizeIssueKind::IncompleteListing
    );
    assert!(
        session.cached(&root).is_none(),
        "Eine unvollständige Bilanz darf nie als vollständige Zahl gecacht werden"
    );
}

#[test]
fn abbruch_liefert_fuer_jeden_root_ein_terminales_uncached_ergebnis() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("root");
    std::fs::create_dir_all(root.join("unter")).unwrap();
    std::fs::write(root.join("unter/datei"), b"123").unwrap();

    let cancel = Cancel::new();
    let mut vfs = InstrumentedVfs::plain();
    vfs.cancel_on = Some(root.clone());
    vfs.cancel = cancel.clone();
    let session = DirectorySizeSession::with_parallelism(Arc::new(vfs), 2);
    let mut results = Vec::new();
    let stats = session.measure(
        std::slice::from_ref(&root),
        DirectorySizeMode::Recalculate,
        &cancel,
        &mut |result| results.push(result.clone()),
    );

    assert_eq!(stats.finished, 1);
    assert_eq!(stats.cancelled, 1);
    assert_eq!(results[0].status, DirectorySizeStatus::Cancelled);
    assert!(session.cached(&root).is_none());
}

#[test]
fn bereits_gesetzter_abbruch_beendet_alle_roots_ohne_listing() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let left = base.join("links");
    let right = base.join("rechts");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();

    let vfs = Arc::new(InstrumentedVfs::plain());
    let session = DirectorySizeSession::with_parallelism(vfs.clone(), 2);
    let cancel = Cancel::new();
    cancel.cancel();
    let mut results = Vec::new();
    let stats = session.measure(
        &[left, right],
        DirectorySizeMode::Recalculate,
        &cancel,
        &mut |result| results.push(result.clone()),
    );

    assert_eq!(stats.requested, 2);
    assert_eq!(stats.finished, 2);
    assert_eq!(stats.cancelled, 2);
    assert_eq!(results.len(), 2);
    assert!(results
        .iter()
        .all(|result| result.status == DirectorySizeStatus::Cancelled));
    assert_eq!(vfs.list_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn nicht_lesbarer_root_ist_fehler_statt_nulldatei() {
    let temp = tempfile::tempdir().unwrap();
    let missing = std::fs::canonicalize(temp.path()).unwrap().join("fehlt");
    let session = DirectorySizeSession::new(Arc::new(LocalVfs::new()));
    let mut results = Vec::new();
    let stats = session.measure(
        std::slice::from_ref(&missing),
        DirectorySizeMode::Recalculate,
        &Cancel::new(),
        &mut |result| results.push(result.clone()),
    );

    assert_eq!(stats.failed, 1);
    assert_eq!(results[0].status, DirectorySizeStatus::Failed);
    assert_eq!(results[0].issues[0].path, missing);
    assert_eq!(results[0].issues[0].code, ErrorCode::NotFound);
    assert!(session.cached(&results[0].path).is_none());
}

#[test]
fn panik_im_ergebnis_callback_laesst_keinen_worker_haengen() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("root");
    std::fs::create_dir(&root).unwrap();

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let session = DirectorySizeSession::with_parallelism(Arc::new(LocalVfs::new()), 2);
        session.measure(
            &[root],
            DirectorySizeMode::Recalculate,
            &Cancel::new(),
            &mut |_| panic!("künstlicher UI-Callbackfehler"),
        );
    }));

    assert!(panicked.is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn nicht_utf8_ordner_wird_bilanziert_aber_nie_ueber_anzeigenamen_betreten() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("root");
    std::fs::create_dir(&root).unwrap();
    let unsafe_name = OsString::from_vec(vec![0xff]);
    std::fs::create_dir(root.join(&unsafe_name)).unwrap();
    std::fs::write(
        root.join(&unsafe_name).join("datei"),
        b"darf nicht gelesen werden",
    )
    .unwrap();

    let session = DirectorySizeSession::new(Arc::new(LocalVfs::new()));
    let mut results = Vec::new();
    session.measure(
        std::slice::from_ref(&root),
        DirectorySizeMode::Recalculate,
        &Cancel::new(),
        &mut |result| results.push(result.clone()),
    );

    assert_eq!(results[0].status, DirectorySizeStatus::Partial);
    assert_eq!(results[0].bytes, 0);
    assert_eq!(results[0].skipped, 1);
    assert_eq!(results[0].issues[0].path, root);
    assert_eq!(
        results[0].issues[0].kind,
        DirectorySizeIssueKind::UnsafeDirectoryName
    );
}
