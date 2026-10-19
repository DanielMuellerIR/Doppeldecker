//! Kurzlebige, abbrechbare Ordnergrößen für die Dateilisten.
//!
//! Der Walker arbeitet ausschließlich lesend über [`Vfs`]. Mehrere Verzeichnisse
//! desselben Laufs werden in einem kleinen Worker-Pool parallel gelistet; ein
//! Ergebnis wird gemeldet, sobald genau der angeforderte Ordner vollständig
//! bilanziert ist. Fehler eines Teilbaums bleiben im Ergebnis erhalten, damit die
//! Oberfläche keine unvollständige Summe als sichere Größe ausgibt.
//!
//! Der Cache gehört absichtlich zu einer [`DirectorySizeSession`] und damit zu einem
//! konkreten, vom Aufrufer gebundenen Endpunkt. Er wird weder persistiert noch aus
//! Verzeichnis-mtime-Werten vermeintlich invalidiert: Die mtime eines Ordners deckt
//! Änderungen tieferer Nachfahren nicht ab. Eine neue Berechnung verlangt deshalb
//! ausdrücklich [`DirectorySizeMode::Recalculate`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};

use crate::error::{error_code, ErrorCode};
use crate::progress::Cancel;
use crate::vfs::Vfs;

const MAX_WORKERS: usize = 8;

/// Soll ein vorhandenes Sitzungsergebnis verwendet oder bewusst neu gelesen werden?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectorySizeMode {
    UseCache,
    Recalculate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectorySizeStatus {
    Complete,
    Observed,
    Partial,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectorySizeIssueKind {
    Listing,
    IncompleteListing,
    UnsafeDirectoryName,
    Overflow,
}

/// Sichtbarer Fehler genau des Teilbaums, an dem die Bilanz unvollständig wurde.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DirectorySizeIssue {
    pub path: PathBuf,
    /// Nur bei einem nicht verlustfrei dekodierbaren Kindnamen gesetzt. `path`
    /// bleibt dann der sichere Elternpfad; der Anzeigename wird niemals wieder zu
    /// einem Dateisystempfad zusammengesetzt.
    pub entry_name: Option<String>,
    pub kind: DirectorySizeIssueKind,
    pub code: ErrorCode,
    pub message: String,
}

/// Terminales Ergebnis eines vom Aufrufer angeforderten Ordners.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DirectorySizeResult {
    pub path: PathBuf,
    /// Logische Länge regulärer Dateien. Symlinks und Spezialdateien werden nicht
    /// betreten und tragen keine erfundene Nutzdatenlänge bei.
    pub bytes: u64,
    pub files: u64,
    pub directories: u64,
    pub skipped: u64,
    pub status: DirectorySizeStatus,
    pub issues: Vec<DirectorySizeIssue>,
    pub cached: bool,
    pub limitations: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct DirectorySizeStats {
    pub requested: u64,
    pub finished: u64,
    pub complete: u64,
    pub observed: u64,
    pub partial: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub cached: u64,
}

/// Speicher- und Ausführungsgrenze eines konkreten VFS-Endpunkts.
///
/// Eine Session darf nicht für verschiedene Providerprofile wiederverwendet werden:
/// Gleichlautende virtuelle Pfade wären sonst kein eindeutiger Cache-Schlüssel.
pub struct DirectorySizeSession {
    vfs: Arc<dyn Vfs>,
    cache: Mutex<DirectorySizeCache>,
    workers: usize,
}

#[derive(Default)]
struct DirectorySizeCache {
    results: HashMap<PathBuf, DirectorySizeResult>,
    latest_generation: HashMap<PathBuf, u128>,
    next_generation: u128,
    invalid_through: u128,
}

impl DirectorySizeCache {
    fn begin(&mut self, path: &Path) -> u128 {
        // u128 macht einen Überlauf auch bei dauerhaft laufender App praktisch
        // unerreichbar. `checked_add` verhindert trotzdem eine stille Wiederverwendung.
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("Ordnergrößen-Generation ist nach 2^128 Läufen erschöpft");
        self.latest_generation
            .insert(path.to_path_buf(), self.next_generation);
        // Eine gescheiterte Neuberechnung darf kein früheres Vollergebnis reaktivieren.
        self.results.remove(path);
        self.next_generation
    }

    fn invalidate(&mut self, path: &Path) {
        self.begin(path);
    }
}

impl DirectorySizeSession {
    pub fn new(vfs: Arc<dyn Vfs>) -> Self {
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, MAX_WORKERS);
        Self::with_parallelism(vfs, workers)
    }

    /// Explizite Workerzahl vor allem für reproduzierbare Tests. Auch eine zu große
    /// Eingabe bleibt begrenzt, damit ein UI-Klick keinen unbeschränkten Thread-Fächer
    /// erzeugen kann.
    pub fn with_parallelism(vfs: Arc<dyn Vfs>, workers: usize) -> Self {
        Self {
            vfs,
            cache: Mutex::new(DirectorySizeCache::default()),
            workers: workers.clamp(1, MAX_WORKERS),
        }
    }

    pub fn cached(&self, path: &Path) -> Option<DirectorySizeResult> {
        lock_recover(&self.cache)
            .results
            .get(path)
            .cloned()
            .map(|mut result| {
                result.cached = true;
                result
            })
    }

    pub fn remove_cached(&self, path: &Path) -> bool {
        let mut cache = lock_recover(&self.cache);
        let existed = cache.results.contains_key(path);
        cache.invalidate(path);
        existed
    }

    pub fn clear_cache(&self) {
        let mut cache = lock_recover(&self.cache);
        cache.next_generation = cache
            .next_generation
            .checked_add(1)
            .expect("Ordnergrößen-Generation ist nach 2^128 Läufen erschöpft");
        cache.invalid_through = cache.next_generation;
        cache.results.clear();
        cache.latest_generation.clear();
    }

    /// Misst jeden eindeutigen Pfad höchstens einmal und ruft `on_result` für jeden
    /// angeforderten Ordner genau einmal auf. Die Reihenfolge ist bewusst nicht
    /// festgelegt: kleine beziehungsweise bereits gecachte Ordner dürfen zuerst in
    /// der Oberfläche erscheinen.
    pub fn measure(
        &self,
        paths: &[PathBuf],
        mode: DirectorySizeMode,
        cancel: &Cancel,
        on_result: &mut dyn FnMut(&DirectorySizeResult),
    ) -> DirectorySizeStats {
        let mut seen = HashSet::new();
        let unique: Vec<_> = paths
            .iter()
            .filter(|path| seen.insert((*path).clone()))
            .cloned()
            .collect();
        let mut stats = DirectorySizeStats {
            requested: unique.len() as u64,
            ..DirectorySizeStats::default()
        };
        let mut roots = Vec::new();

        for path in unique {
            let (cached, generation) = {
                let mut cache = lock_recover(&self.cache);
                let cached = if mode == DirectorySizeMode::UseCache {
                    cache.results.get(&path).cloned().map(|mut result| {
                        result.cached = true;
                        result
                    })
                } else {
                    None
                };
                let generation = cached.is_none().then(|| cache.begin(&path));
                (cached, generation)
            };
            if let Some(result) = cached {
                record_result(&mut stats, &result);
                stats.cached = stats.cached.saturating_add(1);
                on_result(&result);
            } else {
                roots.push(RootRun::new(
                    path,
                    generation.expect("ungecachter Lauf besitzt eine Generation"),
                ));
            }
        }
        if roots.is_empty() {
            return stats;
        }

        let queue = Arc::new((
            Mutex::new(TaskQueue {
                tasks: roots
                    .iter()
                    .enumerate()
                    .map(|(root, state)| WalkTask {
                        root,
                        path: state.path.clone(),
                        is_root: true,
                    })
                    .collect(),
                shutdown: false,
            }),
            Condvar::new(),
        ));
        let (sender, receiver) = mpsc::channel::<WalkOutcome>();

        std::thread::scope(|scope| {
            // Auch wenn ein vom Aufrufer gelieferter Callback panikt, weckt dieser
            // Guard die wartenden Worker vor dem impliziten Join des Scopes. Ohne
            // ihn könnte der panikende Aufrufer dauerhaft im Thread-Abbau hängen.
            let _shutdown = QueueShutdown(Arc::clone(&queue));
            for _ in 0..self.workers {
                let queue = Arc::clone(&queue);
                let sender = sender.clone();
                let vfs = Arc::clone(&self.vfs);
                let cancel = cancel.clone();
                scope.spawn(move || worker(vfs.as_ref(), &cancel, &queue, &sender));
            }
            drop(sender);

            let mut unfinished = roots.len();
            while unfinished > 0 {
                let outcome = receiver
                    .recv()
                    .expect("Ordnergrößen-Worker endeten ohne Ergebnis");
                let state = &mut roots[outcome.root];
                state.pending = state.pending.saturating_sub(1);
                state.bytes = checked_sum(
                    state.bytes,
                    outcome.bytes,
                    &state.path,
                    "Dateibytes",
                    &mut state.issues,
                );
                state.files = checked_sum(
                    state.files,
                    outcome.files,
                    &state.path,
                    "Dateianzahl",
                    &mut state.issues,
                );
                state.directories = checked_sum(
                    state.directories,
                    outcome.directories,
                    &state.path,
                    "Ordneranzahl",
                    &mut state.issues,
                );
                state.skipped = checked_sum(
                    state.skipped,
                    outcome.skipped,
                    &state.path,
                    "ausgelassene Einträge",
                    &mut state.issues,
                );
                if outcome.is_root && outcome.listed {
                    state.root_listed = true;
                }
                state.cancelled |= outcome.cancelled;
                state.issues.extend(outcome.issues);

                if !state.cancelled && !cancel.is_cancelled() {
                    state.pending = state.pending.saturating_add(outcome.children.len());
                    if !outcome.children.is_empty() {
                        let (lock, ready) = &*queue;
                        let mut queue_state = lock_recover(lock);
                        queue_state
                            .tasks
                            .extend(outcome.children.into_iter().map(|path| WalkTask {
                                root: outcome.root,
                                path,
                                is_root: false,
                            }));
                        ready.notify_all();
                    }
                } else {
                    state.cancelled = true;
                }

                if state.pending == 0 {
                    let result = state.result();
                    if result.status == DirectorySizeStatus::Complete {
                        let mut stored = result.clone();
                        stored.cached = false;
                        let mut cache = lock_recover(&self.cache);
                        let generation_is_current = state.generation > cache.invalid_through
                            && cache.latest_generation.get(&stored.path) == Some(&state.generation);
                        // Abbruch und Generation werden unter demselben Cache-Lock
                        // erneut geprüft. So kann ein älterer Lauf nach einer
                        // erzwungenen Neuberechnung kein neueres Ergebnis verdrängen.
                        if generation_is_current && !cancel.is_cancelled() {
                            cache.results.insert(stored.path.clone(), stored);
                        }
                    }
                    record_result(&mut stats, &result);
                    on_result(&result);
                    unfinished -= 1;
                }
            }
        });

        stats
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn record_result(stats: &mut DirectorySizeStats, result: &DirectorySizeResult) {
    stats.finished = stats.finished.saturating_add(1);
    match result.status {
        DirectorySizeStatus::Complete => stats.complete = stats.complete.saturating_add(1),
        DirectorySizeStatus::Observed => stats.observed = stats.observed.saturating_add(1),
        DirectorySizeStatus::Partial => stats.partial = stats.partial.saturating_add(1),
        DirectorySizeStatus::Failed => stats.failed = stats.failed.saturating_add(1),
        DirectorySizeStatus::Cancelled => stats.cancelled = stats.cancelled.saturating_add(1),
    }
}

fn checked_sum(
    left: u64,
    right: u64,
    path: &Path,
    label: &str,
    issues: &mut Vec<DirectorySizeIssue>,
) -> u64 {
    match left.checked_add(right) {
        Some(value) => value,
        None => {
            if !issues
                .iter()
                .any(|issue| issue.kind == DirectorySizeIssueKind::Overflow)
            {
                issues.push(DirectorySizeIssue {
                    path: path.to_path_buf(),
                    entry_name: None,
                    kind: DirectorySizeIssueKind::Overflow,
                    code: ErrorCode::InvalidData,
                    message: format!("{label} überschreiten den darstellbaren Bereich"),
                });
            }
            u64::MAX
        }
    }
}

struct RootRun {
    path: PathBuf,
    generation: u128,
    pending: usize,
    bytes: u64,
    files: u64,
    /// Der angeforderte Root selbst zählt als sichtbarer Ordnername, auch wenn sein
    /// Listing später fehlschlägt. Kinder werden gezählt, sobald ihr Elternlisting
    /// sie verlässlich gemeldet hat.
    directories: u64,
    skipped: u64,
    root_listed: bool,
    cancelled: bool,
    issues: Vec<DirectorySizeIssue>,
}

impl RootRun {
    fn new(path: PathBuf, generation: u128) -> Self {
        Self {
            path,
            generation,
            pending: 1,
            bytes: 0,
            files: 0,
            directories: 1,
            skipped: 0,
            root_listed: false,
            cancelled: false,
            issues: Vec::new(),
        }
    }

    fn result(&self) -> DirectorySizeResult {
        let status = if self.cancelled {
            DirectorySizeStatus::Cancelled
        } else if !self.root_listed {
            DirectorySizeStatus::Failed
        } else if self.issues.is_empty() {
            DirectorySizeStatus::Complete
        } else {
            DirectorySizeStatus::Partial
        };
        let mut issues = self.issues.clone();
        issues.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.entry_name.cmp(&right.entry_name))
                .then_with(|| left.kind.cmp(&right.kind))
        });
        DirectorySizeResult {
            path: self.path.clone(),
            bytes: self.bytes,
            files: self.files,
            directories: self.directories,
            skipped: self.skipped,
            status,
            issues,
            cached: false,
            limitations: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct WalkTask {
    root: usize,
    path: PathBuf,
    is_root: bool,
}

struct TaskQueue {
    tasks: VecDeque<WalkTask>,
    shutdown: bool,
}

struct QueueShutdown(Arc<(Mutex<TaskQueue>, Condvar)>);

impl Drop for QueueShutdown {
    fn drop(&mut self) {
        let (lock, ready) = &*self.0;
        let mut state = lock_recover(lock);
        state.shutdown = true;
        ready.notify_all();
    }
}

struct WalkOutcome {
    root: usize,
    is_root: bool,
    listed: bool,
    cancelled: bool,
    bytes: u64,
    files: u64,
    directories: u64,
    skipped: u64,
    children: Vec<PathBuf>,
    issues: Vec<DirectorySizeIssue>,
}

impl WalkOutcome {
    /// Bilanziert genau einen nicht mitgezählten Eintrag.
    ///
    /// Symlinks, Spezialdateien und nicht verlustfrei dekodierbare Ordnernamen
    /// werden bewusst nicht mitgemessen. Sie verschwinden aber nicht still,
    /// sondern erscheinen sichtbar als „ausgelassen" — mit derselben
    /// Überlaufsicherung wie jede andere Zählung.
    fn count_skipped(&mut self, path: &Path) {
        self.skipped = checked_sum(
            self.skipped,
            1,
            path,
            "ausgelassene Einträge",
            &mut self.issues,
        );
    }
}

fn worker(
    vfs: &dyn Vfs,
    cancel: &Cancel,
    queue: &(Mutex<TaskQueue>, Condvar),
    sender: &mpsc::Sender<WalkOutcome>,
) {
    loop {
        let task = {
            let (lock, ready) = queue;
            let mut state = lock_recover(lock);
            while state.tasks.is_empty() && !state.shutdown {
                state = ready
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.shutdown {
                return;
            }
            state
                .tasks
                .pop_front()
                .expect("geweckte Queue enthält Task")
        };
        let outcome = walk_directory(vfs, task, cancel);
        if sender.send(outcome).is_err() {
            return;
        }
    }
}

fn walk_directory(vfs: &dyn Vfs, task: WalkTask, cancel: &Cancel) -> WalkOutcome {
    let mut outcome = WalkOutcome {
        root: task.root,
        is_root: task.is_root,
        listed: false,
        cancelled: cancel.is_cancelled(),
        bytes: 0,
        files: 0,
        directories: 0,
        skipped: 0,
        children: Vec::new(),
        issues: Vec::new(),
    };
    if outcome.cancelled {
        return outcome;
    }

    let listing = match vfs.list_complete(&task.path) {
        Ok(listing) => listing,
        Err(error) => {
            outcome.issues.push(DirectorySizeIssue {
                path: task.path,
                entry_name: None,
                kind: DirectorySizeIssueKind::Listing,
                code: error_code(&error),
                message: error.to_string(),
            });
            return outcome;
        }
    };
    outcome.listed = true;
    if !listing.complete {
        outcome.issues.push(DirectorySizeIssue {
            path: task.path.clone(),
            entry_name: None,
            kind: DirectorySizeIssueKind::IncompleteListing,
            code: ErrorCode::InvalidData,
            message: "Provider bestätigt die Vollständigkeit des Listings nicht".into(),
        });
    }

    for listed in listing.entries {
        if cancel.is_cancelled() {
            outcome.cancelled = true;
            outcome.children.clear();
            return outcome;
        }
        let entry = listed.entry;
        if entry.is_symlink {
            outcome.count_skipped(&task.path);
            continue;
        }
        if entry.is_dir {
            outcome.directories = checked_sum(
                outcome.directories,
                1,
                &task.path,
                "Ordneranzahl",
                &mut outcome.issues,
            );
            if !listed.safely_decoded {
                outcome.count_skipped(&task.path);
                outcome.issues.push(DirectorySizeIssue {
                    path: task.path.clone(),
                    entry_name: Some(entry.name),
                    kind: DirectorySizeIssueKind::UnsafeDirectoryName,
                    code: ErrorCode::InvalidData,
                    message: "Ordnername ist nicht verlustfrei als UTF-8-Pfad darstellbar".into(),
                });
            } else {
                outcome.children.push(task.path.join(entry.name));
            }
        } else if entry.is_file {
            outcome.files = checked_sum(
                outcome.files,
                1,
                &task.path,
                "Dateianzahl",
                &mut outcome.issues,
            );
            outcome.bytes = checked_sum(
                outcome.bytes,
                entry.size,
                &task.path,
                "Dateibytes",
                &mut outcome.issues,
            );
        } else {
            outcome.count_skipped(&task.path);
        }
    }
    if cancel.is_cancelled() {
        outcome.cancelled = true;
        outcome.children.clear();
    }
    outcome
}
