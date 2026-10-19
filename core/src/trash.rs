//! Descriptor-relativer, absturzsicherer Papierkorb-Uebergang.
//!
//! macOS und Linux beanspruchen den sichtbaren Quellnamen direkt in einem von Anfang an
//! privaten Bereich. Der Transfer verwendet ausschließlich diesen gehaltenen
//! Namensraum und prüft Identität sowie dauerhaften Abschluss getrennt.
//! Die gemeinsame Recovery bindet private Belege frisch und erhält ungeklärte
//! Transfers, öffentliche Altclaims und fremde Reste zur manuellen Prüfung.

mod journal_v2;
#[cfg(test)]
mod legacy;
#[cfg(test)]
use legacy::*;
mod native;

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{fstat, fsync, mkdirat, openat, statat, AtFlags, FileType, Mode, OFlags, Stat};
use serde::{Deserialize, Serialize};

use crate::local_fs::{invalid_path, open_dir_nofollow, open_parent};

const PRIVATE_DIR_MODE: Mode = Mode::from_raw_mode(0o700);
const JOURNAL_VERSION: u32 = 1;
const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;
const TRASHINFO_SUFFIX: &str = ".trashinfo";

/// Ergebnis eines Papierkorb-Stapels. Fehler nach Teilfortschritt werden nicht als
/// scheinbarer Komplettfehler versteckt, sondern einzeln fuer die UI bilanziert.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TrashStats {
    pub trashed: usize,
    pub failed: usize,
    /// Einträge ohne Beleg für die bestätigte Wirkung: Der finale Identitätscheck
    /// wich ab oder eine nötige Nachfrage schlug fehl. Sie gehören weder zu
    /// `trashed` noch zu `failed`; die drei Zahlen ergeben die Gesamtzahl.
    pub unclear: usize,
    pub recovery_paths: Vec<String>,
    pub errors: Vec<String>,
}

/// Ergebnis der Crash-Recovery vor einer neuen Papierkorb-Aktion.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RecoveryStats {
    pub restored: usize,
    pub completed: usize,
    /// Vor dem ersten Claim extern verschwundene oder ausgetauschte Einträge. Diese
    /// Drift braucht keinen Wiederherstellungspfad und blockiert neue Aktionen nicht.
    pub drifted: usize,
    pub recovery_paths: Vec<String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl RecoveryStats {
    /// Bei einem ungeklaerten Claim darf kein neuer Stapel beginnen. So wird ein
    /// sichtbarer Recovery-Pfad nie von spaeteren Aktionen verdeckt.
    pub fn is_clean(&self) -> bool {
        self.recovery_paths.is_empty() && self.errors.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    device: u64,
    inode: u64,
    file_type: u32,
}

impl Identity {
    // Die libc-/rustix-Typaliase unterscheiden sich zwischen macOS und Linux.
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &Stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino,
            file_type: (stat.st_mode as u32) & (libc::S_IFMT as u32),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum JournalStatus {
    Planned,
    Claimed,
    Trashed,
    Drifted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct JournalEntry {
    original: PathBuf,
    expected: Identity,
    claim_name: String,
    trash_path: PathBuf,
    info_path: Option<PathBuf>,
    status: JournalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrashJournal {
    version: u32,
    batch_id: String,
    entries: Vec<JournalEntry>,
}

/// Verschiebt alle ausgewaehlten Eintraege in den nativen Papierkorb. Vor dem ersten
/// Claim werden der gesamte Stapel, alle Quellidentitaeten und alle Ziele geprueft.
pub fn trash_items(
    items: &[PathBuf],
    sandbox: &Option<PathBuf>,
    journal_dir: &Path,
) -> io::Result<TrashStats> {
    native::trash(items, sandbox, journal_dir)
}

/// Verarbeitet alle verbliebenen Journale. Ein Claim wird nur dann zurueckbenannt,
/// wenn seine Identitaet exakt passt und der urspruengliche Name frei ist.
pub fn recover_trash_journals(journal_dir: &Path) -> io::Result<RecoveryStats> {
    match std::fs::symlink_metadata(journal_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Papierkorb-Journalpfad ist kein symlinkfreier Ordner",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RecoveryStats::default());
        }
        Err(error) => return Err(error),
    }
    let directory = open_dir_nofollow(journal_dir, false)?;
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(&directory)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name.starts_with(b"trash-") && name.ends_with(b".json") {
            names.push(OsString::from(OsStr::from_bytes(name)));
        }
    }

    let mut report = RecoveryStats::default();
    for name in names {
        let path = journal_dir.join(&name);
        if let Err(error) = recover_journal(&path, &mut report) {
            for recovery in crate::error::recovery_paths(&error) {
                if !report.recovery_paths.contains(&recovery) {
                    report.recovery_paths.push(recovery);
                }
            }
            report.errors.push(format!("{}: {error}", path.display()));
        }
    }
    Ok(report)
}

fn recover_journal(path: &Path, report: &mut RecoveryStats) -> io::Result<()> {
    let mut data = Vec::new();
    let (parent, name) = open_parent(path)?;
    let file = openat(
        &parent,
        &name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    if FileType::from_raw_mode(fstat(&file)?.st_mode) != FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Papierkorb-Journal ist keine reguläre Datei",
        ));
    }
    File::from(file)
        .take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Papierkorb-Journal überschreitet die Größenbegrenzung",
        ));
    }
    #[derive(Deserialize)]
    struct Version {
        version: u32,
    }
    let version: Version = serde_json::from_slice(&data)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if version.version == 2 {
        return journal_v2::claim::recover(path, &data, report);
    }
    if version.version != JOURNAL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unbekannte Journal-Version {}", version.version),
        ));
    }
    let mut journal: TrashJournal = serde_json::from_slice(&data)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut store = journal_v2::claim::LegacyJournal::bind(path, &data).map_err(|error| {
        crate::error::with_recovery_paths(error, &[path.to_string_lossy().into_owned()])
    })?;

    let mut settled = true;
    let mut preserve_terminal_journal = false;
    let mut changed = false;
    for entry in &mut journal.entries {
        if entry.status == JournalStatus::Claimed && entry.failure.is_some() {
            // Der finale Transfer wurde begonnen, aber nicht dauerhaft bestätigt.
            // Auch ein inzwischen passender Name autorisiert keinen blinden Rückweg.
            settled = false;
            report.errors.push(entry.failure.clone().unwrap());
            report
                .recovery_paths
                .push(entry.trash_path.display().to_string());
            report.recovery_paths.push(
                entry
                    .original
                    .with_file_name(&entry.claim_name)
                    .display()
                    .to_string(),
            );
            continue;
        }
        if entry.status == JournalStatus::Drifted {
            preserve_terminal_journal = true;
            report.drifted += 1;
            if let Some(warning) = &entry.failure {
                report.warnings.push(warning.clone());
            }
            continue;
        }
        if entry.status == JournalStatus::Trashed {
            report.completed += 1;
            continue;
        }
        let claim_path = entry.original.with_file_name(&entry.claim_name);
        let claim = path_identity_checked(&claim_path)?;
        if path_identity_checked(&entry.original)? == Some(entry.expected) {
            if claim.is_none() {
                report.restored += 1;
            } else {
                settled = false;
                report
                    .errors
                    .push("Original und Legacy-Claim sind gleichzeitig vorhanden".into());
                report.recovery_paths.push(claim_path.display().to_string());
            }
            continue;
        }

        let expected_claim_exists = claim == Some(entry.expected);
        if expected_claim_exists {
            match journal_v2::claim::restore_legacy_claim(
                &entry.original,
                &entry.claim_name,
                entry.expected,
            ) {
                Ok(()) => {
                    report.restored += 1;
                    continue;
                }
                Err(error) => report.errors.push(format!(
                    "Claim {} konnte nicht zurueckgesetzt werden: {error}",
                    claim_path.display()
                )),
            }
            settled = false;
            report.recovery_paths.push(claim_path.display().to_string());
            continue;
        }

        if path_identity_checked(&entry.trash_path)? == Some(entry.expected) {
            changed = true;
            entry.status = JournalStatus::Trashed;
            report.completed += 1;
            continue;
        }

        // `planned` wurde vor jedem Claim dauerhaft geschrieben. Fehlen jetzt
        // Original, erwarteter Claim und Papierkorbziel, hat Doppeldecker selbst noch
        // nichts umbenannt. Das ist externe Drift, kein liegen gebliebener Claim.
        if entry.status == JournalStatus::Planned && !expected_claim_exists {
            let warning = format!(
                "{} verschwand oder änderte sich vor dem Papierkorb-Claim; kein Recovery-Pfad nötig",
                entry.original.display()
            );
            entry.status = JournalStatus::Drifted;
            entry.failure = Some(warning.clone());
            report.drifted += 1;
            report.warnings.push(warning);
            preserve_terminal_journal = true;
            changed = true;
            continue;
        }

        settled = false;
        report.recovery_paths.push(claim_path.display().to_string());
    }

    if settled && !preserve_terminal_journal {
        store.remove().map_err(|error| {
            crate::error::with_recovery_paths(error, &[path.to_string_lossy().into_owned()])
        })?;
    } else if changed {
        store.save(&journal).map_err(|error| {
            crate::error::with_recovery_paths(error, &[path.to_string_lossy().into_owned()])
        })?;
    }
    Ok(())
}

fn validate_batch(items: &[PathBuf]) -> io::Result<()> {
    if items.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Leerer Papierkorb-Stapel",
        ));
    }
    let mut seen = HashSet::new();
    for item in items {
        validate_absolute_path(item)?;
        if item == Path::new("/") {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Die Dateisystemwurzel darf nicht in den Papierkorb",
            ));
        }
        if !seen.insert(item.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Doppelter Papierkorb-Pfad: {}", item.display()),
            ));
        }
    }
    for (index, left) in items.iter().enumerate() {
        for right in items.iter().skip(index + 1) {
            if left.starts_with(right) || right.starts_with(left) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "Eltern-/Kind-Ueberlappung im Papierkorb-Stapel: {} und {}",
                        left.display(),
                        right.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn validate_absolute_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return Err(invalid_path(path));
    }
    path.file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid_path(path))?;
    Ok(())
}

fn collision_name(original: &OsStr, suffix: u32, max_bytes: usize) -> OsString {
    use std::os::unix::ffi::OsStringExt;

    let suffix = if suffix == 0 {
        String::new()
    } else {
        format!(" ({suffix})")
    };
    let content_budget = max_bytes.saturating_sub(suffix.len());
    let path = Path::new(original);
    let stem = path.file_stem().unwrap_or(original).as_bytes();
    let extension = path.extension().map(OsStr::as_bytes);
    let mut candidate = Vec::with_capacity(max_bytes);
    if let Some(extension) = extension {
        let stem_budget = content_budget.saturating_sub(extension.len() + 1);
        let shortened_stem = bounded_name_bytes(stem, stem_budget);
        if !shortened_stem.is_empty() {
            candidate.extend_from_slice(shortened_stem);
            candidate.extend_from_slice(suffix.as_bytes());
            candidate.push(b'.');
            candidate.extend_from_slice(extension);
            return OsString::from_vec(candidate);
        }
    }
    // Eine selbst überlange Erweiterung kann nicht erhalten bleiben. Raw Unix-
    // Bytes werden nie verlustbehaftet dekodiert; gültiges UTF-8 bleibt gültig.
    candidate.extend_from_slice(bounded_name_bytes(original.as_bytes(), content_budget));
    candidate.extend_from_slice(suffix.as_bytes());
    OsString::from_vec(candidate)
}

fn bounded_name_bytes(name: &[u8], max_bytes: usize) -> &[u8] {
    let mut end = name.len().min(max_bytes);
    if let Ok(text) = std::str::from_utf8(name) {
        while !text.is_char_boundary(end) {
            end -= 1;
        }
    }
    &name[..end]
}

fn info_name(target: &OsStr) -> OsString {
    let mut name = target.to_os_string();
    name.push(TRASHINFO_SUFFIX);
    name
}

/// Fehlererhaltende Nachfrage am gehaltenen Elterndeskriptor: Nur ein echtes
/// `ENOENT` heißt „nicht vorhanden“, jeder andere Fehler bleibt ein Fehler. Die
/// bequeme Variante `legacy::identity_at` macht aus beidem `None` und taugt nur
/// dort, wo eine unbeantwortete Nachfrage ohnehin fail-closed weiterläuft.
fn identity_at_checked(parent: &OwnedFd, name: &OsStr) -> io::Result<Option<Identity>> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(Identity::from_stat(&stat))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Recovery darf nur einen fehlenden **Namen** als externe Drift werten. Ist schon
/// sein Elternordner nicht erreichbar (etwa weil das Volume nicht eingebunden ist),
/// bleibt das Journal als Blocker erhalten. Die bequeme Laufzeitabfrage
/// `legacy::path_identity` unterscheidet diese Fälle absichtlich nicht und ist daher hier
/// ungeeignet.
fn path_identity_checked(path: &Path) -> io::Result<Option<Identity>> {
    let (parent, name) = open_parent(path)?;
    identity_at_checked(&parent, &name)
}

fn home_dir() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME ist nicht gesetzt"))
}

fn percent_encode_path(path: &Path) -> String {
    let mut out = String::new();
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn local_deletion_time() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    // `localtime_r` schreibt ausschliesslich in unseren Stack-Puffer und ist im
    // Gegensatz zu `localtime` threadsicher. Bei einem Plattformfehler bleibt eine
    // gueltige, deutlich erkennbare Fallback-Zeit stehen.
    if unsafe { libc::localtime_r(&now, &mut local) }.is_null() {
        return "1970-01-01T00:00:00".into();
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    struct TestLayout {
        root: PathBuf,
        needs_trashinfo: bool,
    }

    impl TrashLayout for TestLayout {
        fn destination(&self, _source: &Path, _source_device: u64) -> io::Result<TrashDestination> {
            Ok(if !self.needs_trashinfo {
                TrashDestination {
                    root: self.root.clone(),
                    files: self.root.clone(),
                    info: None,
                    needs_trashinfo: false,
                    create_base: false,
                }
            } else {
                TrashDestination {
                    root: self.root.clone(),
                    files: self.root.join("files"),
                    info: Some(self.root.join("info")),
                    needs_trashinfo: true,
                    create_base: false,
                }
            })
        }
    }

    fn fixture(needs_trashinfo: bool) -> (tempfile::TempDir, PathBuf, PathBuf, TestLayout) {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("source");
        let journal = root.join("journal");
        std::fs::create_dir(&source).unwrap();
        let trash = root.join("trash");
        let layout = TestLayout {
            root: trash,
            needs_trashinfo,
        };
        (temp, source, journal, layout)
    }

    #[test]
    fn synthetisches_journal_v2_ohne_gehaltene_layoutbelege_bleibt_unveraendert() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let journal_dir = base.join("journale");
        std::fs::create_dir(&journal_dir).unwrap();
        let journal = journal_dir.join("trash-v2.json");
        let data = serde_json::to_vec(&journal_v2::fixture()).unwrap();
        std::fs::write(&journal, &data).unwrap();
        let source = base.join("quelle");
        std::fs::write(&source, b"fremde Daten bleiben erhalten").unwrap();
        let before = std::fs::read(&source).unwrap();
        for _ in 0..2 {
            let report = recover_trash_journals(&journal_dir).unwrap();
            assert!(!report.is_clean());
            assert_eq!(report.completed, 0);
            assert_eq!(report.restored, 0);
            assert_eq!(report.recovery_paths, vec![journal.display().to_string()]);
            assert_eq!(report.errors.len(), 1);
            assert_eq!(std::fs::read(&journal).unwrap(), data);
            assert_eq!(std::fs::read(&source).unwrap(), before);
        }
        let mut invalid = journal_v2::fixture();
        invalid["version"] = 97.into();
        let unknown = serde_json::to_vec(&invalid).unwrap();
        std::fs::write(&journal, &unknown).unwrap();
        let report = recover_trash_journals(&journal_dir).unwrap();
        assert!(!report.is_clean());
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("Unbekannte Journal-Version 97")));
        assert_eq!(std::fs::read(&journal).unwrap(), unknown);
        assert_eq!(std::fs::read(source).unwrap(), before);
    }

    #[test]
    fn journalfehler_nach_vortransfer_commit_wird_nach_ruecksetzung_failed() {
        for transfer_hint in [false, true] {
            let (_temp, source, journal_dir, layout) = fixture(false);
            let original = source.join("auswahl");
            std::fs::write(&original, b"ausgewaehlt").unwrap();
            let mut injected = false;
            let stats = trash_items_with_layout_and_journal_hook(
                std::slice::from_ref(&original),
                &None,
                &journal_dir,
                &layout,
                &mut |_| {},
                &mut |_| {},
                &mut |path, journal, create| {
                    persist_journal(path, journal, create)?;
                    let entry = &journal.entries[0];
                    if !injected
                        && entry.status == JournalStatus::Claimed
                        && entry.failure.is_some() == transfer_hint
                    {
                        injected = true;
                        return Err(crate::coded_error(
                            io::ErrorKind::Other,
                            crate::ErrorCode::OutcomeUnclear,
                            "Simulierter Eltern-fsync-Fehler nach Journalcommit",
                        ));
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert!(injected);
            assert_eq!((stats.trashed, stats.failed, stats.unclear), (0, 1, 0));
            assert!(stats.recovery_paths.is_empty());
            assert_eq!(std::fs::read(&original).unwrap(), b"ausgewaehlt");
            assert!(!layout.root.join("auswahl").exists());
            assert_eq!(std::fs::read_dir(&journal_dir).unwrap().count(), 0);
            assert!(recover_trash_journals(&journal_dir).unwrap().is_clean());
        }
    }

    #[test]
    fn journalfehler_nach_trashed_commit_erhaelt_positiven_readback_fuer_recovery() {
        let (_temp, source, journal_dir, layout) = fixture(false);
        let original = source.join("auswahl");
        std::fs::write(&original, b"ausgewaehlt").unwrap();
        let mut injected = false;
        let stats = trash_items_with_layout_and_journal_hook(
            std::slice::from_ref(&original),
            &None,
            &journal_dir,
            &layout,
            &mut |_| {},
            &mut |_| {},
            &mut |path, journal, create| {
                persist_journal(path, journal, create)?;
                if journal.entries[0].status == JournalStatus::Trashed {
                    injected = true;
                    return Err(crate::coded_error(
                        io::ErrorKind::Other,
                        crate::ErrorCode::OutcomeUnclear,
                        "Simulierter Eltern-fsync-Fehler nach terminalem Journalcommit",
                    ));
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(injected);
        assert_eq!((stats.trashed, stats.failed, stats.unclear), (0, 0, 1));
        assert!(!original.exists());
        assert_eq!(
            std::fs::read(layout.root.join("auswahl")).unwrap(),
            b"ausgewaehlt"
        );
        let recovery = recover_trash_journals(&journal_dir).unwrap();
        assert!(recovery.is_clean());
        assert_eq!((recovery.restored, recovery.completed), (0, 1));
        assert_eq!(std::fs::read_dir(&journal_dir).unwrap().count(), 0);
        assert!(recover_trash_journals(&journal_dir).unwrap().is_clean());
    }

    #[test]
    fn claim_austausch_nach_pruefung_bleibt_unclear_auch_nach_neustart() {
        let (_temp, source, journal_dir, layout) = fixture(false);
        let original = source.join("auswahl");
        let saved = source.join("erwarteter-inhalt");
        std::fs::write(&original, b"ausgewaehlt").unwrap();
        let mut observed_claim = None;
        let stats = trash_items_with_layout_and_transfer_hook(
            std::slice::from_ref(&original),
            &None,
            &journal_dir,
            &layout,
            &mut |_| {},
            &mut |item| {
                let claim = item.original.with_file_name(&item.claim_name);
                std::fs::rename(&claim, &saved).unwrap();
                std::fs::write(&claim, b"fremder-nachfolger").unwrap();
                observed_claim = Some(claim);
            },
        )
        .unwrap();
        let claim = observed_claim.unwrap();
        let target = layout.root.join("auswahl");
        assert_eq!(stats.trashed, 0);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.unclear, 1);
        assert!(stats.recovery_paths.contains(&target.display().to_string()));
        assert!(stats.recovery_paths.contains(&claim.display().to_string()));
        assert_eq!(std::fs::read(&target).unwrap(), b"fremder-nachfolger");
        assert_eq!(std::fs::read(&saved).unwrap(), b"ausgewaehlt");
        // Selbst wenn der ausgewählte Inhalt inzwischen wieder am Ursprung steht,
        // darf das ungeklärte Fremdobjekt nicht durch Journal-Cleanup verschwinden.
        std::fs::rename(&saved, &original).unwrap();
        let journal_files = std::fs::read_dir(&journal_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert_eq!(journal_files.len(), 1);
        let persisted: TrashJournal =
            serde_json::from_slice(&std::fs::read(&journal_files[0]).unwrap()).unwrap();
        assert_eq!(persisted.entries[0].status, JournalStatus::Claimed);
        assert!(persisted.entries[0].failure.is_some());
        for _ in 0..2 {
            let recovery = recover_trash_journals(&journal_dir).unwrap();
            assert!(!recovery.is_clean());
            assert_eq!(recovery.restored, 0);
            assert_eq!(recovery.completed, 0);
            assert!(recovery
                .recovery_paths
                .contains(&target.display().to_string()));
            assert!(recovery
                .recovery_paths
                .contains(&claim.display().to_string()));
            assert_eq!(std::fs::read(&target).unwrap(), b"fremder-nachfolger");
            assert_eq!(std::fs::read(&original).unwrap(), b"ausgewaehlt");
            assert!(journal_files[0].exists());
        }
    }

    #[test]
    fn fremder_claim_nach_transferfehler_wird_nicht_zurueckbenannt() {
        let (_temp, source, journal_dir, layout) = fixture(false);
        let original = source.join("auswahl");
        let saved = source.join("erwarteter-inhalt");
        std::fs::write(&original, b"ausgewaehlt").unwrap();
        let mut observed_claim = None;
        let stats = trash_items_with_layout_and_transfer_hook(
            std::slice::from_ref(&original),
            &None,
            &journal_dir,
            &layout,
            &mut |_| {},
            &mut |item| {
                let claim = item.original.with_file_name(&item.claim_name);
                std::fs::rename(&claim, &saved).unwrap();
                std::fs::write(&claim, b"fremder-claim").unwrap();
                std::fs::write(&item.trash_path, b"spaete-kollision").unwrap();
                observed_claim = Some(claim);
            },
        )
        .unwrap();
        let claim = observed_claim.unwrap();
        assert_eq!((stats.trashed, stats.failed, stats.unclear), (0, 0, 1));
        assert_eq!(std::fs::read(&saved).unwrap(), b"ausgewaehlt");
        assert_eq!(std::fs::read(&claim).unwrap(), b"fremder-claim");
        assert_eq!(
            std::fs::read(layout.root.join("auswahl")).unwrap(),
            b"spaete-kollision"
        );
        assert!(!original.exists());
        let recovery = recover_trash_journals(&journal_dir).unwrap();
        assert!(!recovery.is_clean());
        assert_eq!((recovery.restored, recovery.completed), (0, 0));
        assert_eq!(std::fs::read(&claim).unwrap(), b"fremder-claim");
        assert!(!original.exists());
    }

    #[test]
    fn datei_ordner_und_finaler_symlink_werden_als_eintrag_verschoben() {
        let (_temp, source, journal, layout) = fixture(false);
        std::fs::write(source.join("datei.txt"), b"inhalt").unwrap();
        std::fs::create_dir(source.join("ordner")).unwrap();
        std::fs::write(source.join("ordner/kind"), b"kind").unwrap();
        symlink("nicht-betreten", source.join("link")).unwrap();
        let items = [
            source.join("datei.txt"),
            source.join("ordner"),
            source.join("link"),
        ];

        let mut noop = |_| {};
        let stats = trash_items_with_layout(&items, &None, &journal, &layout, &mut noop).unwrap();

        assert_eq!(stats.trashed, 3);
        assert!(stats.recovery_paths.is_empty());
        assert!(stats.errors.is_empty());
        assert_eq!(std::fs::read_dir(&journal).unwrap().count(), 0);
        assert!(recover_trash_journals(&journal).unwrap().is_clean());
        assert_eq!(
            std::fs::read(layout.root.join("datei.txt")).unwrap(),
            b"inhalt"
        );
        assert!(layout.root.join("ordner/kind").is_file());
        assert!(std::fs::symlink_metadata(layout.root.join("link"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn vorhandener_name_wird_nicht_ersetzt() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, source, journal, layout) = fixture(false);
        std::fs::create_dir(&layout.root).unwrap();
        std::fs::set_permissions(&layout.root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(layout.root.join("gleich.txt"), b"alt").unwrap();
        std::fs::write(source.join("gleich.txt"), b"neu").unwrap();
        let mut noop = |_| {};
        let stats = trash_items_with_layout(
            &[source.join("gleich.txt")],
            &None,
            &journal,
            &layout,
            &mut noop,
        )
        .unwrap();

        assert_eq!(stats.trashed, 1);
        assert_eq!(
            std::fs::read(layout.root.join("gleich.txt")).unwrap(),
            b"alt"
        );
        assert_eq!(
            std::fs::read(layout.root.join("gleich (1).txt")).unwrap(),
            b"neu"
        );
    }

    #[test]
    fn lange_ascii_und_utf8_namen_passen_mit_kollision_und_trashinfo() {
        for needs_trashinfo in [false, true] {
            for original_name in [
                format!("{}.txt", "a".repeat(251)),
                format!("{}.txt", "ä".repeat(125)),
            ] {
                let (_temp, source, journal, layout) = fixture(needs_trashinfo);
                let original = source.join(&original_name);
                std::fs::write(&original, b"erster Inhalt").unwrap();
                let mut noop = |_| {};
                let first = trash_items_with_layout(
                    std::slice::from_ref(&original),
                    &None,
                    &journal,
                    &layout,
                    &mut noop,
                )
                .unwrap();
                assert_eq!(first.trashed, 1, "{first:?}");
                std::fs::write(&original, b"zweiter Inhalt").unwrap();
                let second = trash_items_with_layout(
                    std::slice::from_ref(&original),
                    &None,
                    &journal,
                    &layout,
                    &mut noop,
                )
                .unwrap();
                assert_eq!(second.trashed, 1, "{second:?}");
                assert!(!original.exists());
                let files = if needs_trashinfo {
                    layout.root.join("files")
                } else {
                    layout.root.clone()
                };
                let mut payloads = Vec::new();
                for entry in std::fs::read_dir(&files).unwrap() {
                    let entry = entry.unwrap();
                    let name = entry.file_name();
                    let name_bytes = name.as_bytes();
                    assert!(name_bytes.len() <= MAX_TRASH_NAME_BYTES);
                    assert!(std::str::from_utf8(name_bytes).is_ok());
                    payloads.push(std::fs::read(entry.path()).unwrap());
                    if needs_trashinfo {
                        let info = layout.root.join("info").join(info_name(&name));
                        assert!(info.file_name().unwrap().as_bytes().len() <= MAX_TRASH_NAME_BYTES);
                        let content = std::fs::read_to_string(info).unwrap();
                        assert!(
                            content.contains(&format!("Path={}\n", percent_encode_path(&original)))
                        );
                    }
                }
                payloads.sort();
                assert_eq!(
                    payloads,
                    [b"erster Inhalt".to_vec(), b"zweiter Inhalt".to_vec()]
                );
            }
        }
    }

    #[test]
    fn kollisionsname_bewahrt_raw_unixbytes_ohne_ersatzzeichen() {
        use std::os::unix::ffi::OsStringExt;
        let short = OsString::from_vec(b"a\xff.txt".to_vec());
        assert_eq!(
            collision_name(&short, 1, MAX_TRASH_NAME_BYTES).as_bytes(),
            b"a\xff (1).txt"
        );
        let mut bytes = vec![b'a'; 250];
        bytes.extend_from_slice(&[0xff, b'.', b't', b'x', b't']);
        let original = OsString::from_vec(bytes.clone());
        let unchanged = collision_name(&original, 0, MAX_TRASH_NAME_BYTES);
        assert_eq!(unchanged.as_bytes(), bytes);
        let shortened = collision_name(&original, 1, MAX_TRASH_NAME_BYTES);
        assert_eq!(shortened.as_bytes().len(), MAX_TRASH_NAME_BYTES);
        assert!(shortened.as_bytes().ends_with(b" (1).txt"));
        assert!(!shortened
            .as_bytes()
            .windows(3)
            .any(|bytes| bytes == b"\xef\xbf\xbd"));
        let utf8 = format!("{}.txt", "ä".repeat(125));
        let candidate = collision_name(
            OsStr::new(&utf8),
            1,
            MAX_TRASH_NAME_BYTES - TRASHINFO_SUFFIX.len(),
        );
        assert!(candidate.to_str().is_some());
        assert!(candidate.as_bytes().len() <= MAX_TRASH_NAME_BYTES - TRASHINFO_SUFFIX.len());
    }

    #[test]
    fn geoeffneter_quellordner_bleibt_bei_symlink_swap_verankert() {
        let (temp, source, journal, layout) = fixture(false);
        let parked = temp.path().join("parked");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(source.join("beweis"), b"innen").unwrap();
        std::fs::write(outside.join("beweis"), b"aussen").unwrap();

        let mut hook = |_| {
            std::fs::rename(&source, &parked).unwrap();
            symlink(&outside, &source).unwrap();
        };
        let stats = trash_items_with_layout(
            &[source.join("beweis")],
            &None,
            &journal,
            &layout,
            &mut hook,
        )
        .unwrap();

        assert_eq!(stats.trashed, 1);
        assert_eq!(std::fs::read(outside.join("beweis")).unwrap(), b"aussen");
        assert_eq!(std::fs::read(layout.root.join("beweis")).unwrap(), b"innen");
    }

    #[test]
    fn identitaetswechsel_stoppt_transfer_und_stellt_fremden_namen_zurueck() {
        let (temp, source, journal, layout) = fixture(false);
        let original = source.join("wechsel.txt");
        let parked = temp.path().join("urspruenglich.txt");
        std::fs::write(&original, b"original").unwrap();
        let mut hook = |_| {
            std::fs::rename(&original, &parked).unwrap();
            std::fs::write(&original, b"fremd").unwrap();
        };
        let stats = trash_items_with_layout(
            std::slice::from_ref(&original),
            &None,
            &journal,
            &layout,
            &mut hook,
        )
        .unwrap();

        assert_eq!((stats.trashed, stats.failed), (0, 1));
        assert_eq!(std::fs::read(&original).unwrap(), b"fremd");
        assert_eq!(std::fs::read(&parked).unwrap(), b"original");
        assert!(!layout.root.join("wechsel.txt").exists());
    }

    #[test]
    fn sandbox_swap_betritt_den_eingeschleusten_link_nicht() {
        let (temp, source, journal, layout) = fixture(false);
        let child = source.join("kind");
        let parked = temp.path().join("kind-geparkt");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&child).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(child.join("datei"), b"innen").unwrap();
        std::fs::write(outside.join("datei"), b"aussen").unwrap();
        let mut hook = |_| {
            std::fs::rename(&child, &parked).unwrap();
            symlink(&outside, &child).unwrap();
        };
        let stats = trash_items_with_layout(
            &[child.join("datei")],
            &Some(source.clone()),
            &journal,
            &layout,
            &mut hook,
        )
        .unwrap();

        assert_eq!(stats.trashed, 1);
        assert_eq!(std::fs::read(outside.join("datei")).unwrap(), b"aussen");
        assert_eq!(std::fs::read(layout.root.join("datei")).unwrap(), b"innen");
    }

    #[test]
    fn vorpruefungsfehler_veraendert_keinen_quellnamen() {
        let (_temp, source, journal, layout) = fixture(false);
        let parent = source.join("ordner");
        let child = parent.join("kind");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(&child, b"kind").unwrap();
        let mut noop = |_| {};
        assert!(trash_items_with_layout(
            &[parent.clone(), child.clone()],
            &None,
            &journal,
            &layout,
            &mut noop,
        )
        .is_err());
        assert!(child.exists());
        assert!(!layout.root.exists());
    }

    #[test]
    fn fehler_nach_erstem_eintrag_liefert_teilbilanz() {
        let (_temp, source, journal, layout) = fixture(false);
        let first = source.join("eins");
        let second = source.join("zwei");
        std::fs::write(&first, b"1").unwrap();
        std::fs::write(&second, b"2").unwrap();
        let mut hook = |index| {
            if index == 1 {
                std::fs::remove_file(&second).unwrap();
            }
        };
        let stats = trash_items_with_layout(
            &[first, second.clone()],
            &None,
            &journal,
            &layout,
            &mut hook,
        )
        .unwrap();
        assert_eq!((stats.trashed, stats.failed), (1, 1));
    }

    /// Ein Stat-Fehler am Papierkorb-Zielnamen beweist gar nichts. Frueher wurde er
    /// als "nicht angekommen" gebucht — der Nutzer bekam "Loeschen fehlgeschlagen"
    /// fuer eine moeglicherweise bereits vollzogene Verschiebung.
    ///
    /// Der Fall wird ueber entzogene Rechte am Papierkorbordner hergestellt. Das
    /// setzt einen gewoehnlichen Nutzer voraus: root umgeht die Rechtepruefung
    /// (`CAP_DAC_OVERRIDE`), der Rename gelingt dann, und `trashed == 1` waere das
    /// korrekte Ergebnis — der Test pruefte nichts mehr. Belegt am 2026-09-03 im
    /// `rust:1.96`-Container auf dem Linux-Testhost, der als root laeuft. Die
    /// GitHub-CI faehrt als gewoehnlicher Nutzer; dort laeuft der Fall normal mit.
    #[test]
    fn unbeantwortbare_nachfrage_am_papierkorb_wird_nicht_als_fehlschlag_gebucht() {
        use std::os::unix::fs::PermissionsExt;

        if rustix::process::geteuid().is_root() {
            eprintln!(
                "uebersprungen: als root schraenkt `chmod 000` den Papierkorbordner nicht ein"
            );
            return;
        }

        let (_temp, source, journal, layout) = fixture(false);
        let item = source.join("eins");
        std::fs::write(&item, b"1").unwrap();
        let trash_root = layout.root.clone();
        let mut hook = |_index| {
            // Ab hier beantwortet der Papierkorbordner weder den Rename noch die
            // anschliessende Nachfrage.
            std::fs::set_permissions(&trash_root, std::fs::Permissions::from_mode(0o000)).unwrap();
        };
        let stats = trash_items_with_layout(
            std::slice::from_ref(&item),
            &None,
            &journal,
            &layout,
            &mut hook,
        )
        .unwrap();
        std::fs::set_permissions(&layout.root, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(
            (stats.trashed, stats.failed, stats.unclear),
            (0, 0, 1),
            "ein unbeantworteter Readback ist weder Erfolg noch Fehlschlag"
        );
        assert_eq!(
            stats.trashed + stats.failed + stats.unclear,
            1,
            "die Teilsummen muessen die Gesamtzahl ergeben"
        );
        assert_eq!(stats.errors.len(), 1);
        assert!(
            stats.errors[0].contains("liess sich nicht feststellen"),
            "der unklare Ausgang muss im Text stehen: {}",
            stats.errors[0]
        );
        // Der Claim ist zurueckgenommen, die Quelle steht unveraendert da.
        assert!(item.exists());
    }

    #[test]
    fn recovery_setzt_liegen_gebliebenen_claim_zurueck() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, source, journal_dir, layout) = fixture(false);
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
        let original = source.join("absturz");
        let claim = source.join(".claim-test");
        std::fs::write(&original, b"daten").unwrap();
        let expected = path_identity(&original).unwrap();
        std::fs::rename(&original, &claim).unwrap();
        let journal = TrashJournal {
            version: JOURNAL_VERSION,
            batch_id: "test".into(),
            entries: vec![JournalEntry {
                original: original.clone(),
                expected,
                claim_name: ".claim-test".into(),
                trash_path: layout.root.join("absturz"),
                info_path: None,
                status: JournalStatus::Claimed,
                failure: None,
            }],
        };
        let path = journal_dir.join("trash-test.json");
        persist_journal(&path, &journal, true).unwrap();

        let report = recover_trash_journals(&journal_dir).unwrap();
        assert!(report.is_clean());
        assert_eq!(report.restored, 1);
        assert_eq!(std::fs::read(original).unwrap(), b"daten");
        assert!(!path.exists());
    }

    #[test]
    fn legacy_recovery_erhaelt_oeffentliche_claims_und_gleichzeitige_originale() {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o755, 0o777, 0o700] {
            let (_temp, source, journal_dir, layout) = fixture(false);
            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(mode)).unwrap();
            let original = source.join("original");
            let claim = source.join(".claim");
            std::fs::write(&claim, b"original").unwrap();
            let expected = path_identity(&claim).unwrap();
            if mode == 0o700 {
                std::fs::hard_link(&claim, &original).unwrap();
            }
            let journal = TrashJournal {
                version: JOURNAL_VERSION,
                batch_id: "legacy".into(),
                entries: vec![JournalEntry {
                    original: original.clone(),
                    expected,
                    claim_name: ".claim".into(),
                    trash_path: layout.root.join("original"),
                    info_path: None,
                    status: JournalStatus::Claimed,
                    failure: None,
                }],
            };
            let path = journal_dir.join("trash-legacy.json");
            persist_journal(&path, &journal, true).unwrap();
            let before = std::fs::read(&path).unwrap();
            let report = recover_trash_journals(&journal_dir).unwrap();
            assert!(!report.is_clean());
            assert_eq!((report.restored, report.completed), (0, 0));
            assert_eq!(std::fs::read(path).unwrap(), before);
            assert_eq!(std::fs::read(claim).unwrap(), b"original");
            assert_eq!(original.exists(), mode == 0o700);
        }
    }

    #[test]
    fn fifo_journal_wird_ohne_blockierendes_lesen_erhalten() {
        let (_temp, _source, journal_dir, _layout) = fixture(false);
        std::fs::create_dir(&journal_dir).unwrap();
        let path = journal_dir.join("trash-fifo.json");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let report = recover_trash_journals(&journal_dir).unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(std::fs::symlink_metadata(path).is_ok());
    }

    #[test]
    fn recovery_erkennt_bereits_verschobenen_eintrag() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, source, journal_dir, layout) = fixture(false);
        std::fs::create_dir(&layout.root).unwrap();
        std::fs::set_permissions(&layout.root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let original = source.join("absturz");
        let target = layout.root.join("absturz");
        std::fs::write(&target, b"daten").unwrap();
        let expected = path_identity(&target).unwrap();
        let journal = TrashJournal {
            version: JOURNAL_VERSION,
            batch_id: "test".into(),
            entries: vec![JournalEntry {
                original,
                expected,
                claim_name: ".claim-test".into(),
                trash_path: target,
                info_path: None,
                status: JournalStatus::Claimed,
                failure: None,
            }],
        };
        let path = journal_dir.join("trash-test.json");
        persist_journal(&path, &journal, true).unwrap();

        let report = recover_trash_journals(&journal_dir).unwrap();
        assert_eq!(report.completed, 1);
        assert!(!path.exists());
    }

    #[test]
    fn recovery_markiert_vor_claim_verschwundenen_eintrag_als_terminale_drift() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, source, journal_dir, layout) = fixture(false);
        // Das Produktionsverfahren legt das Ziel vor dem Planned-Journal an. Nur
        // dann beweist ein fehlender Zielname echte Drift statt ein offline
        // gegangenes Papierkorb-Volume.
        std::fs::create_dir(&layout.root).unwrap();
        std::fs::set_permissions(&layout.root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let original = source.join("extern-verschoben");
        std::fs::write(&original, b"daten").unwrap();
        let expected = path_identity(&original).unwrap();
        std::fs::remove_file(&original).unwrap();
        let path = journal_dir.join("trash-planned-race.json");
        persist_journal(
            &path,
            &TrashJournal {
                version: JOURNAL_VERSION,
                batch_id: "planned-race".into(),
                entries: vec![JournalEntry {
                    original,
                    expected,
                    claim_name: ".claim-nie-angelegt".into(),
                    trash_path: layout.root.join("extern-verschoben"),
                    info_path: None,
                    status: JournalStatus::Planned,
                    failure: None,
                }],
            },
            true,
        )
        .unwrap();

        let report = recover_trash_journals(&journal_dir).unwrap();
        assert!(report.is_clean());
        assert_eq!(report.drifted, 1);
        assert!(report.recovery_paths.is_empty());
        assert!(report.errors.is_empty());
        assert_eq!(report.warnings.len(), 1);
        let persisted: TrashJournal =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.entries[0].status, JournalStatus::Drifted);
        assert!(persisted.entries[0].failure.is_some());

        let second = recover_trash_journals(&journal_dir).unwrap();
        assert!(second.is_clean());
        assert_eq!(second.drifted, 1);
        assert!(second.recovery_paths.is_empty());
    }

    #[test]
    fn recovery_vertagt_planned_eintrag_bei_nicht_erreichbarem_quellordner() {
        use std::os::unix::fs::PermissionsExt;

        let (temp, source, journal_dir, layout) = fixture(false);
        std::fs::create_dir(&layout.root).unwrap();
        std::fs::set_permissions(&layout.root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let original = source.join("nicht-eingebunden");
        std::fs::write(&original, b"daten").unwrap();
        let expected = path_identity(&original).unwrap();
        let path = journal_dir.join("trash-offline.json");
        persist_journal(
            &path,
            &TrashJournal {
                version: JOURNAL_VERSION,
                batch_id: "offline".into(),
                entries: vec![JournalEntry {
                    original,
                    expected,
                    claim_name: ".claim-offline".into(),
                    trash_path: layout.root.join("nicht-eingebunden"),
                    info_path: None,
                    status: JournalStatus::Planned,
                    failure: None,
                }],
            },
            true,
        )
        .unwrap();
        std::fs::rename(&source, temp.path().join("quelle-offline")).unwrap();

        let report = recover_trash_journals(&journal_dir).unwrap();

        assert!(!report.is_clean());
        assert_eq!(report.drifted, 0);
        assert_eq!(report.errors.len(), 1);
        let persisted: TrashJournal =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.entries[0].status, JournalStatus::Planned);
    }

    #[test]
    fn recovery_laesst_uebergrosses_journal_unangetastet() {
        let (_temp, source, journal_dir, layout) = fixture(false);
        std::fs::create_dir(&journal_dir).unwrap();
        let path = journal_dir.join("trash-zu-gross.json");
        let journal = TrashJournal {
            version: JOURNAL_VERSION,
            batch_id: "zu-gross".into(),
            entries: vec![JournalEntry {
                original: source.join("datei"),
                expected: Identity {
                    device: 1,
                    inode: 1,
                    file_type: 0o100_000,
                },
                claim_name: ".doppeldecker-trash-claim-zu-gross".into(),
                trash_path: layout.root.join("datei"),
                info_path: None,
                status: JournalStatus::Drifted,
                failure: Some("x".repeat(4 * 1024 * 1024)),
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();

        let report = recover_trash_journals(&journal_dir).unwrap();

        assert!(!report.is_clean());
        assert_eq!(report.errors.len(), 1);
        assert!(report.warnings.is_empty());
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn recovery_wertet_baumelnden_journal_symlink_nicht_als_leeren_bestand() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let journal_dir = root.join("journal");
        symlink(root.join("nicht-vorhanden"), &journal_dir).unwrap();

        let error = recover_trash_journals(&journal_dir)
            .expect_err("ein unsicherer Journalpfad darf Recovery nicht als leer freigeben");

        assert!(matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::NotADirectory
        ));
    }

    #[test]
    fn planned_journal_mit_echtem_claim_bleibt_recovery_blocker() {
        let (_temp, source, journal_dir, layout) = fixture(false);
        let original = source.join("absturz-vor-statusupdate");
        let claim = source.join(".claim-vor-statusupdate");
        std::fs::write(&original, b"erwartet").unwrap();
        let expected = path_identity(&original).unwrap();
        std::fs::rename(&original, &claim).unwrap();
        std::fs::write(&original, b"fremd").unwrap();
        let path = journal_dir.join("trash-planned-claim.json");
        persist_journal(
            &path,
            &TrashJournal {
                version: JOURNAL_VERSION,
                batch_id: "planned-claim".into(),
                entries: vec![JournalEntry {
                    original,
                    expected,
                    claim_name: ".claim-vor-statusupdate".into(),
                    trash_path: layout.root.join("absturz-vor-statusupdate"),
                    info_path: None,
                    status: JournalStatus::Planned,
                    failure: None,
                }],
            },
            true,
        )
        .unwrap();

        let report = recover_trash_journals(&journal_dir).unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.drifted, 0);
        assert_eq!(report.recovery_paths, vec![claim.display().to_string()]);
        assert_eq!(std::fs::read(&claim).unwrap(), b"erwartet");
    }

    #[test]
    fn xdg_info_kodiert_sonderzeichen_und_ursprung() {
        let (_temp, source, journal, layout) = fixture(true);
        let original = source.join("leer zeichen#.txt");
        std::fs::write(&original, b"daten").unwrap();
        let mut noop = |_| {};
        let stats = trash_items_with_layout(
            std::slice::from_ref(&original),
            &None,
            &journal,
            &layout,
            &mut noop,
        )
        .unwrap();
        assert_eq!(stats.trashed, 1);
        let info =
            std::fs::read_to_string(layout.root.join("info/leer zeichen#.txt.trashinfo")).unwrap();
        assert!(info.contains("leer%20zeichen%23.txt"));
        assert!(info.contains("DeletionDate="));
    }

    #[test]
    fn unsichere_papierkorb_rechte_brechen_vor_claim_ab() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, source, journal, layout) = fixture(false);
        std::fs::create_dir(&layout.root).unwrap();
        std::fs::set_permissions(&layout.root, std::fs::Permissions::from_mode(0o777)).unwrap();
        let original = source.join("bleibt");
        std::fs::write(&original, b"daten").unwrap();
        let mut noop = |_| {};
        assert!(trash_items_with_layout(
            std::slice::from_ref(&original),
            &None,
            &journal,
            &layout,
            &mut noop,
        )
        .is_err());
        assert!(original.exists());
    }
}
