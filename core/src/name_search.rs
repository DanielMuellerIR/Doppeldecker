//! Lesende rekursive Namenssuche mit sichtbarer Teilbilanz und begrenztem Aufwand.
//!
//! Ein Reader darf keine beobachteten Symlinks betreten. Treffer sind beobachtete
//! Namen, keine dauerhaften Identitätsbelege für spätere Dateiaktionen. Die Suche
//! öffnet keine Dateiinhalte und besitzt weder Queue noch Mutationsfreigabe.

use std::io;
use std::path::{Component, Path, PathBuf};

use crate::vfs::CompleteDirectoryListing;
use crate::{coded_error, error_code, Cancel, Entry, ErrorCode, SftpVfs, Vfs};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NameSearchOptions {
    pub query: String,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub include_hidden: bool,
}

impl NameSearchOptions {
    pub fn validate(&self) -> io::Result<()> {
        if self.query.trim().is_empty() || self.query.len() > 1024 || self.query.contains('\0') {
            return Err(coded_error(
                io::ErrorKind::InvalidInput,
                ErrorCode::InvalidInput,
                "Die Namenssuche braucht 1 bis 1024 Bytes Suchtext ohne NUL-Zeichen",
            ));
        }
        Ok(())
    }
}

/// Feste technische Grenzen des ersten Schnitts. Jedes Erreichen beendet den Lauf
/// mit `limited`, niemals mit einer behaupteten vollständigen Suche.
#[derive(Clone, Copy, Debug)]
pub struct NameSearchLimits {
    pub entries: u64,
    pub directories: u64,
    pub hits: u64,
    pub pending_directories: usize,
    pub depth: usize,
    pub issues: usize,
    pub path_bytes: usize,
}

impl Default for NameSearchLimits {
    fn default() -> Self {
        Self {
            entries: 250_000,
            directories: 25_000,
            hits: 5_000,
            pending_directories: 25_000,
            depth: 512,
            issues: 100,
            path_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NameSearchStatus {
    Running,
    Complete,
    Partial,
    Cancelled,
    Limited,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct NameSearchStats {
    pub entries: u64,
    pub directories: u64,
    pub hits: u64,
    pub skipped: u64,
    pub issues: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct NameSearchHit {
    pub is_hidden: bool,
    pub name: String,
    pub parent: PathBuf,
    pub relative_path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct NameSearchIssue {
    pub path: PathBuf,
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct NameSearchUpdate {
    pub status: NameSearchStatus,
    pub stats: NameSearchStats,
    /// Nur die seit dem vorigen Callback neu gefundenen Treffer und Fehler.
    pub hits: Vec<NameSearchHit>,
    pub issues: Vec<NameSearchIssue>,
    pub limit: Option<String>,
}

#[derive(Clone)]
pub struct NameSearchListing {
    pub entries: Vec<(Entry, bool)>,
    pub complete: bool,
    pub omissions: Vec<String>,
}

impl From<CompleteDirectoryListing> for NameSearchListing {
    fn from(listing: CompleteDirectoryListing) -> Self {
        Self {
            entries: listing
                .entries
                .into_iter()
                .map(|item| (item.entry, item.safely_decoded))
                .collect(),
            complete: listing.complete,
            omissions: Vec::new(),
        }
    }
}

pub trait NameSearchReader {
    fn entry_is_hidden(&self, _path: &Path, entry: &Entry) -> io::Result<bool> {
        Ok(entry.name.starts_with('.'))
    }
    /// Ein Listing ist eine einzelne blockierende Provideranfrage. Abbruch wird
    /// mindestens davor/danach geprüft; er beendet keinen fremden Netzwerkaufruf.
    fn read_directory(&self, path: &Path, cancel: &Cancel) -> io::Result<NameSearchListing>;
}

pub struct VfsNameSearchReader<'a>(pub &'a dyn Vfs);
impl NameSearchReader for VfsNameSearchReader<'_> {
    fn entry_is_hidden(&self, path: &Path, entry: &Entry) -> io::Result<bool> {
        self.0.entry_is_hidden(path, entry)
    }
    fn read_directory(&self, path: &Path, _cancel: &Cancel) -> io::Result<NameSearchListing> {
        self.0.list_complete(path).map(Into::into)
    }
}

/// Die typisierte SFTP-Anzeigeklasse bleibt ausdrücklich vom strikten VFS getrennt.
/// Jeder Aufruf prüft den Elternpfad ohne beobachtete Symlinks und besitzt die
/// bestehende 60-Sekunden-Lesefrist. Abbruch wartet höchstens auf diesen Aufruf.
pub struct SftpNameSearchReader<'a> {
    provider: &'a SftpVfs,
    generation: u64,
}
impl<'a> SftpNameSearchReader<'a> {
    pub fn new(provider: &'a SftpVfs) -> Self {
        Self {
            provider,
            generation: provider.session_generation(),
        }
    }
}
impl NameSearchReader for SftpNameSearchReader<'_> {
    fn read_directory(&self, path: &Path, _cancel: &Cancel) -> io::Result<NameSearchListing> {
        let check = || {
            if self.provider.session_generation() == self.generation {
                Ok(())
            } else {
                Err(coded_error(
                    io::ErrorKind::Interrupted,
                    ErrorCode::PlanDrift,
                    "SFTP-Sitzung wechselte während der Namenssuche",
                ))
            }
        };
        check()?;
        let listing = self.provider.list_interactive_best_effort(path)?;
        check()?;
        Ok(NameSearchListing {
            entries: listing
                .entries
                .into_iter()
                .map(|item| (item.entry, item.safely_decoded))
                .collect(),
            complete: true,
            omissions: listing
                .omitted
                .into_iter()
                .map(|item| format!("{}: {}", item.display_name, item.reason))
                .collect(),
        })
    }
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\0')
        && !name.contains('/')
        && !matches!(name, "." | "..")
        && Path::new(name).components().count() == 1
}

fn comparable_name(name: &str, case_sensitive: bool) -> String {
    // macOS liefert häufig zerlegte Zeichen, die Eingabe dagegen zusammengesetzte.
    // Nur den Vergleich vereinheitlichen; Treffer und spätere Aktionen behalten den Rohnamen.
    let normalized: String = name.nfc().collect();
    if case_sensitive {
        normalized
    } else {
        normalized.to_lowercase()
    }
}

/// Iterativer Tiefenscan; der Stack und die übertragenen Resultate sind begrenzt.
/// Der Reader liefert weiterhin jeweils ein ganzes Providerlisting. Dessen eigene
/// Parser-/Antwortgrenze bleibt zusätzlich wirksam. Es wird kein Gesamtbaum gehalten.
pub fn search_names(
    reader: &dyn NameSearchReader,
    root: &Path,
    options: &NameSearchOptions,
    limits: NameSearchLimits,
    cancel: &Cancel,
    on_update: &mut dyn FnMut(&NameSearchUpdate),
) -> io::Result<NameSearchStats> {
    options.validate()?;
    if !root.is_absolute()
        || root
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || limits.entries == 0
        || limits.directories == 0
        || limits.hits == 0
        || limits.pending_directories == 0
        || limits.depth == 0
        || limits.path_bytes == 0
    {
        return Err(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Ungültiger Suchroot oder ungültige Suchgrenze",
        ));
    }
    let needle = comparable_name(&options.query, options.case_sensitive);
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut pending_bytes = root.as_os_str().len();
    let mut hit_bytes = 0usize;
    let mut update = NameSearchUpdate {
        status: NameSearchStatus::Running,
        stats: NameSearchStats::default(),
        hits: Vec::new(),
        issues: Vec::new(),
        limit: None,
    };
    let mut reported_issues = 0usize;
    let mut limited = None;
    let mut fatal = false;
    while let Some((directory, depth)) = pending.pop() {
        pending_bytes = pending_bytes.saturating_sub(directory.as_os_str().len());
        if cancel.is_cancelled() {
            break;
        }
        if update.stats.directories >= limits.directories {
            limited = Some(format!(
                "Anfragegrenze von {} Verzeichnissen erreicht",
                limits.directories
            ));
            break;
        }
        update.stats.directories += 1;
        match reader.read_directory(&directory, cancel) {
            Ok(listing) => {
                if cancel.is_cancelled() {
                    break;
                }
                if !listing.complete {
                    record_issue(
                        &mut update,
                        &mut reported_issues,
                        limits.issues,
                        &directory,
                        ErrorCode::InvalidData,
                        "Das Verzeichnislisting ist unvollständig".into(),
                    );
                }
                for message in listing.omissions {
                    update.stats.skipped += 1;
                    record_issue(
                        &mut update,
                        &mut reported_issues,
                        limits.issues,
                        &directory,
                        ErrorCode::Unsupported,
                        message,
                    );
                }
                for (entry, safely_decoded) in listing.entries {
                    if cancel.is_cancelled() {
                        break;
                    }
                    if update.stats.entries >= limits.entries {
                        limited = Some(format!(
                            "Scanbegrenzung von {} Einträgen erreicht",
                            limits.entries
                        ));
                        break;
                    }
                    update.stats.entries += 1;
                    if !safely_decoded || !safe_name(&entry.name) {
                        update.stats.skipped += 1;
                        record_issue(
                            &mut update,
                            &mut reported_issues,
                            limits.issues,
                            &directory,
                            ErrorCode::InvalidData,
                            format!("Nicht sicher adressierbarer Name: {}", entry.name),
                        );
                        continue;
                    }
                    let candidate = comparable_name(&entry.name, options.case_sensitive);
                    let matches = candidate.contains(&needle);
                    let is_hidden = if !options.include_hidden || matches {
                        match reader.entry_is_hidden(&directory.join(&entry.name), &entry) {
                            Ok(value) => value,
                            Err(error) => {
                                update.stats.skipped += 1;
                                record_issue(
                                    &mut update,
                                    &mut reported_issues,
                                    limits.issues,
                                    &directory.join(&entry.name),
                                    error_code(&error),
                                    error.to_string(),
                                );
                                continue;
                            }
                        }
                    } else {
                        false
                    };
                    if !options.include_hidden && is_hidden {
                        continue;
                    }
                    if matches {
                        let bytes = directory
                            .as_os_str()
                            .len()
                            .saturating_mul(2)
                            .saturating_add(entry.name.len().saturating_mul(2));
                        if hit_bytes.saturating_add(bytes) > limits.path_bytes {
                            limited = Some("Pfadspeichergrenze für Suchtreffer erreicht".into());
                            break;
                        }
                        hit_bytes += bytes;
                        update.stats.hits += 1;
                        update.hits.push(NameSearchHit {
                            is_hidden,
                            parent: directory.clone(),
                            relative_path: directory
                                .strip_prefix(root)
                                .expect("Suchpfad liegt unter Root")
                                .join(&entry.name),
                            name: entry.name.clone(),
                            is_dir: entry.is_dir,
                            is_symlink: entry.is_symlink,
                        });
                        if update.stats.hits >= limits.hits {
                            limited = Some(format!(
                                "Treffergrenze von {} Treffern erreicht",
                                limits.hits
                            ));
                            break;
                        }
                    }
                    if entry.is_dir && !entry.is_symlink {
                        let child = directory.join(&entry.name);
                        let bytes = child.as_os_str().len();
                        if depth >= limits.depth
                            || pending.len() >= limits.pending_directories
                            || pending_bytes.saturating_add(bytes) > limits.path_bytes
                        {
                            limited = Some("Speicher- oder Tiefengrenze der Suche erreicht".into());
                            break;
                        }
                        pending_bytes += bytes;
                        pending.push((child, depth + 1));
                    } else if !entry.is_file && !entry.is_symlink {
                        update.stats.skipped += 1;
                    }
                }
            }
            Err(error) => {
                let code = error_code(&error);
                record_issue(
                    &mut update,
                    &mut reported_issues,
                    limits.issues,
                    &directory,
                    code,
                    error.to_string(),
                );
                fatal = matches!(
                    code,
                    ErrorCode::PlanDrift | ErrorCode::ConnectionLost | ErrorCode::Interrupted
                );
            }
        }
        on_update(&update);
        update.hits.clear();
        update.issues.clear();
        if limited.is_some() || fatal {
            break;
        }
    }
    update.status = if cancel.is_cancelled() {
        NameSearchStatus::Cancelled
    } else if limited.is_some() {
        NameSearchStatus::Limited
    } else if update.stats.issues > 0 {
        NameSearchStatus::Partial
    } else {
        NameSearchStatus::Complete
    };
    update.limit = limited;
    on_update(&update);
    Ok(update.stats)
}

fn record_issue(
    update: &mut NameSearchUpdate,
    reported: &mut usize,
    maximum: usize,
    path: &Path,
    code: ErrorCode,
    message: String,
) {
    update.stats.issues += 1;
    if *reported < maximum {
        *reported += 1;
        update.issues.push(NameSearchIssue {
            path: path.to_path_buf(),
            code,
            message,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalVfs;
    use std::sync::Mutex;

    #[test]
    fn unicode_normalisierung_findet_den_unveraenderten_originalnamen() {
        let tree = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tree.path()).unwrap();
        std::fs::write(root.join("Cafe\u{301}.txt"), b"Inhalt").unwrap();
        let original = std::fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name();
        for (query, case_sensitive) in [("Café", true), ("CAFÉ", false)] {
            let mut hits = Vec::new();
            search_names(
                &VfsNameSearchReader(&LocalVfs::new()),
                &root,
                &NameSearchOptions {
                    query: query.into(),
                    case_sensitive,
                    include_hidden: false,
                },
                NameSearchLimits::default(),
                &Cancel::new(),
                &mut |update| hits.extend(update.hits.clone()),
            )
            .unwrap();
            assert_eq!(hits.len(), 1, "Suchtext: {query}");
            assert_eq!(hits[0].name, original.to_str().unwrap());
            assert_eq!(hits[0].relative_path.as_os_str(), original);
        }
    }

    fn run(
        reader: &dyn NameSearchReader,
        root: &Path,
        query: &str,
        limits: NameSearchLimits,
        cancel: &Cancel,
    ) -> Vec<NameSearchUpdate> {
        let mut updates = Vec::new();
        search_names(
            reader,
            root,
            &NameSearchOptions {
                query: query.into(),
                case_sensitive: false,
                include_hidden: false,
            },
            limits,
            cancel,
            &mut |update| updates.push(update.clone()),
        )
        .unwrap();
        updates
    }

    #[test]
    fn rekursiv_ohne_inhalte_oder_symlink_verfolgung() {
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir(tree.path().join("Ordner")).unwrap();
        std::fs::write(tree.path().join("Ordner/TREFFER.txt"), "nicht durchsuchen").unwrap();
        std::fs::create_dir(tree.path().join(".versteckt")).unwrap();
        std::fs::write(tree.path().join(".versteckt/treffer.txt"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(tree.path(), tree.path().join("treffer-link")).unwrap();
        let updates = run(
            &VfsNameSearchReader(&LocalVfs::new()),
            &tree.path().canonicalize().unwrap(),
            "treffer",
            NameSearchLimits::default(),
            &Cancel::new(),
        );
        let hits: Vec<_> = updates.iter().flat_map(|update| &update.hits).collect();
        assert!(hits
            .iter()
            .any(|hit| hit.relative_path == Path::new("Ordner/TREFFER.txt")));
        assert!(!hits
            .iter()
            .any(|hit| hit.relative_path.starts_with(".versteckt")));
        assert_eq!(updates.last().unwrap().stats.directories, 2);
        assert_eq!(updates.last().unwrap().status, NameSearchStatus::Complete);
    }

    struct FakeReader {
        calls: Mutex<Vec<PathBuf>>,
        cancel_after_read: bool,
        cancel: Cancel,
    }
    impl NameSearchReader for FakeReader {
        fn read_directory(&self, path: &Path, _: &Cancel) -> io::Result<NameSearchListing> {
            self.calls.lock().unwrap().push(path.to_path_buf());
            if self.cancel_after_read {
                self.cancel.cancel();
            }
            Err(coded_error(
                io::ErrorKind::PermissionDenied,
                ErrorCode::PermissionDenied,
                "Kein Zugriff",
            ))
        }
    }

    #[test]
    fn listingfehler_und_abbruch_sind_keine_vollstaendige_suche() {
        for should_cancel in [false, true] {
            let cancel = Cancel::new();
            let reader = FakeReader {
                calls: Mutex::new(Vec::new()),
                cancel_after_read: should_cancel,
                cancel: cancel.clone(),
            };
            let updates = run(
                &reader,
                Path::new("/root"),
                "x",
                NameSearchLimits::default(),
                &cancel,
            );
            assert_eq!(
                updates.last().unwrap().status,
                if should_cancel {
                    NameSearchStatus::Cancelled
                } else {
                    NameSearchStatus::Partial
                }
            );
            assert_eq!(updates.last().unwrap().stats.issues, 1);
        }
    }

    #[test]
    fn frueher_abbruch_verhindert_providerzugriff() {
        let cancel = Cancel::new();
        cancel.cancel();
        let reader = FakeReader {
            calls: Mutex::new(Vec::new()),
            cancel_after_read: false,
            cancel: cancel.clone(),
        };
        let updates = run(
            &reader,
            Path::new("/root"),
            "x",
            NameSearchLimits::default(),
            &cancel,
        );
        assert!(reader.calls.lock().unwrap().is_empty());
        assert_eq!(updates.last().unwrap().status, NameSearchStatus::Cancelled);
    }

    #[test]
    fn treffergrenze_bleibt_sichtbare_teilbilanz() {
        let tree = tempfile::tempdir().unwrap();
        for name in ["x1", "x2", "x3"] {
            std::fs::write(tree.path().join(name), "").unwrap();
        }
        let updates = run(
            &VfsNameSearchReader(&LocalVfs::new()),
            &tree.path().canonicalize().unwrap(),
            "x",
            NameSearchLimits {
                hits: 2,
                ..Default::default()
            },
            &Cancel::new(),
        );
        assert_eq!(updates.last().unwrap().status, NameSearchStatus::Limited);
        assert_eq!(updates.last().unwrap().stats.hits, 2);
        assert!(updates.last().unwrap().limit.is_some());
        assert_eq!(
            updates
                .iter()
                .map(|update| update.hits.len())
                .sum::<usize>(),
            2
        );
    }

    #[test]
    fn ungueltige_optionen_und_roots_starten_keine_anfrage() {
        let cancel = Cancel::new();
        let reader = FakeReader {
            calls: Mutex::new(Vec::new()),
            cancel_after_read: false,
            cancel: cancel.clone(),
        };
        for (root, query) in [("/root", " "), ("/root/../outside", "x"), ("relative", "x")] {
            assert!(search_names(
                &reader,
                Path::new(root),
                &NameSearchOptions {
                    query: query.into(),
                    case_sensitive: false,
                    include_hidden: false
                },
                NameSearchLimits::default(),
                &cancel,
                &mut |_| {}
            )
            .is_err());
        }
        assert!(reader.calls.lock().unwrap().is_empty());
    }
    struct ScriptedReader {
        entries: Vec<(Entry, bool)>,
        complete: bool,
        calls: Mutex<Vec<PathBuf>>,
    }
    impl NameSearchReader for ScriptedReader {
        fn read_directory(&self, path: &Path, _: &Cancel) -> io::Result<NameSearchListing> {
            self.calls.lock().unwrap().push(path.into());
            if path == Path::new("/root") {
                Ok(NameSearchListing {
                    entries: self.entries.clone(),
                    complete: self.complete,
                    omissions: vec![],
                })
            } else {
                Err(coded_error(
                    io::ErrorKind::PermissionDenied,
                    ErrorCode::PermissionDenied,
                    "Kein Zugriff",
                ))
            }
        }
    }
    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.into(),
            is_dir,
            is_file: !is_dir,
            is_symlink: false,
            size: 0,
            mtime: None,
            created: None,
            owner: None,
            group: None,
            extension: None,
            mode: None,
            device: None,
            inode: None,
            hard_links: None,
        }
    }
    #[test]
    fn unsichere_namen_und_listingluecken_bleiben_begrenzt_sichtbar() {
        let reader = ScriptedReader {
            entries: vec![
                (entry("../escape", true), true),
                (entry("nicht-dekodiert", true), false),
                (entry("sichtbar", true), true),
            ],
            complete: false,
            calls: Mutex::new(Vec::new()),
        };
        let updates = run(
            &reader,
            Path::new("/root"),
            "x",
            NameSearchLimits {
                issues: 1,
                ..Default::default()
            },
            &Cancel::new(),
        );
        assert_eq!(
            *reader.calls.lock().unwrap(),
            vec![PathBuf::from("/root"), PathBuf::from("/root/sichtbar")]
        );
        let terminal = updates.last().unwrap();
        assert_eq!(terminal.status, NameSearchStatus::Partial);
        assert_eq!(terminal.stats.issues, 4);
        assert_eq!(
            updates
                .iter()
                .map(|update| update.issues.len())
                .sum::<usize>(),
            1
        );
    }
    #[test]
    fn anfrage_stack_scan_und_pfadspeichergrenzen_sind_teilbilanzen() {
        for limits in [
            NameSearchLimits {
                directories: 1,
                ..Default::default()
            },
            NameSearchLimits {
                pending_directories: 1,
                ..Default::default()
            },
            NameSearchLimits {
                entries: 1,
                ..Default::default()
            },
            NameSearchLimits {
                path_bytes: 1,
                ..Default::default()
            },
        ] {
            let reader = ScriptedReader {
                entries: vec![
                    (entry("ordner1", true), true),
                    (entry("ordner2", true), true),
                ],
                complete: true,
                calls: Mutex::new(Vec::new()),
            };
            let updates = run(
                &reader,
                Path::new("/root"),
                "ordner",
                limits,
                &Cancel::new(),
            );
            assert_eq!(updates.last().unwrap().status, NameSearchStatus::Limited);
            assert!(updates.last().unwrap().limit.is_some());
        }
    }
    #[test]
    fn symlink_tausch_eines_geplanten_unterordners_wird_nicht_betreten() {
        #[cfg(unix)]
        {
            let tree = tempfile::tempdir().unwrap();
            let root = tree.path().canonicalize().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("treffer.txt"), "").unwrap();
            std::fs::create_dir(root.join("kind")).unwrap();
            struct SwapReader<'a> {
                root: &'a Path,
                outside: &'a Path,
            }
            impl NameSearchReader for SwapReader<'_> {
                fn read_directory(&self, path: &Path, _: &Cancel) -> io::Result<NameSearchListing> {
                    let listing = LocalVfs::new().list_complete(path)?;
                    if path == self.root {
                        std::fs::remove_dir(self.root.join("kind"))?;
                        std::os::unix::fs::symlink(self.outside, self.root.join("kind"))?;
                    }
                    Ok(listing.into())
                }
            }
            let updates = run(
                &SwapReader {
                    root: &root,
                    outside: outside.path(),
                },
                &root,
                "treffer",
                NameSearchLimits::default(),
                &Cancel::new(),
            );
            assert_eq!(updates.last().unwrap().status, NameSearchStatus::Partial);
            assert_eq!(updates.last().unwrap().stats.hits, 0);
        }
    }
    #[test]
    fn tiefe_begrenzt_auch_einen_zyklischen_providerbaum() {
        struct EndlessReader;
        impl NameSearchReader for EndlessReader {
            fn read_directory(&self, _: &Path, _: &Cancel) -> io::Result<NameSearchListing> {
                Ok(NameSearchListing {
                    entries: vec![(entry("kind", true), true)],
                    complete: true,
                    omissions: vec![],
                })
            }
        }
        let updates = run(
            &EndlessReader,
            Path::new("/root"),
            "x",
            NameSearchLimits {
                depth: 1,
                ..Default::default()
            },
            &Cancel::new(),
        );
        assert_eq!(updates.last().unwrap().status, NameSearchStatus::Limited);
        assert_eq!(updates.last().unwrap().stats.directories, 2);
    }
}
