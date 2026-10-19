//! Bisheriger v1-Schreibweg, ausschließlich als Regressionsevidenz für alte
//! Journale und die frühere Claim-Lücke in Tests erhalten.
use super::*;
use crate::atomic_write::{atomic_write_local, AtomicWriteMode};
use crate::local_fs::{dir_flags, remove_file};
use rustix::fs::{renameat_with, unlinkat, RenameFlags};
use rustix::io::dup;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
const INFO_FILE_MODE: Mode = Mode::from_raw_mode(0o600);
pub(super) const MAX_TRASH_NAME_BYTES: usize = 255;
static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub(super) struct TrashDestination {
    pub(super) root: PathBuf,
    pub(super) files: PathBuf,
    pub(super) info: Option<PathBuf>,
    pub(super) needs_trashinfo: bool,
    pub(super) create_base: bool,
}

pub(super) trait TrashLayout {
    fn destination(&self, source: &Path, source_device: u64) -> io::Result<TrashDestination>;
}

pub(super) struct PreparedItem {
    pub(super) original: PathBuf,
    pub(super) source_parent: OwnedFd,
    pub(super) source_name: OsString,
    pub(super) expected: Identity,
    pub(super) claim_name: OsString,
    pub(super) files_dir: OwnedFd,
    pub(super) info_dir: Option<OwnedFd>,
    pub(super) target_name: OsString,
    pub(super) trash_path: PathBuf,
    pub(super) info_path: Option<PathBuf>,
    pub(super) needs_trashinfo: bool,
}

pub(super) fn trash_items_with_layout(
    items: &[PathBuf],
    sandbox: &Option<PathBuf>,
    journal_dir: &Path,
    layout: &dyn TrashLayout,
    before_claim: &mut dyn FnMut(usize),
) -> io::Result<TrashStats> {
    trash_items_with_layout_and_transfer_hook(
        items,
        sandbox,
        journal_dir,
        layout,
        before_claim,
        &mut |_| {},
    )
}

pub(super) fn trash_items_with_layout_and_transfer_hook(
    items: &[PathBuf],
    sandbox: &Option<PathBuf>,
    journal_dir: &Path,
    layout: &dyn TrashLayout,
    before_claim: &mut dyn FnMut(usize),
    before_transfer: &mut dyn FnMut(&PreparedItem),
) -> io::Result<TrashStats> {
    trash_items_with_layout_and_journal_hook(
        items,
        sandbox,
        journal_dir,
        layout,
        before_claim,
        before_transfer,
        &mut persist_journal,
    )
}

pub(super) fn trash_items_with_layout_and_journal_hook(
    items: &[PathBuf],
    sandbox: &Option<PathBuf>,
    journal_dir: &Path,
    layout: &dyn TrashLayout,
    before_claim: &mut dyn FnMut(usize),
    before_transfer: &mut dyn FnMut(&PreparedItem),
    write_journal: &mut dyn FnMut(&Path, &TrashJournal, bool) -> io::Result<()>,
) -> io::Result<TrashStats> {
    validate_batch(items)?;
    let sandbox_fd = sandbox
        .as_ref()
        .map(|root| open_dir_nofollow(root, false))
        .transpose()?;

    let mut reserved = HashSet::new();
    let mut prepared = Vec::with_capacity(items.len());
    for item in items {
        let (source_parent, source_name) = open_source_parent(item, sandbox, sandbox_fd.as_ref())?;
        let source_stat = statat(&source_parent, &source_name, AtFlags::SYMLINK_NOFOLLOW)?;
        let expected = Identity::from_stat(&source_stat);
        let destination = layout.destination(item, expected.device)?;
        let (files_dir, info_dir) = prepare_destination(&destination, expected.device)?;
        let target_name = reserve_target_name(
            &files_dir,
            info_dir.as_ref(),
            &source_name,
            &destination.files,
            &mut reserved,
        )?;
        let claim_name = reserve_claim_name(&source_parent)?;
        let trash_path = destination.files.join(&target_name);
        let info_path = destination
            .info
            .as_ref()
            .map(|dir| dir.join(info_name(&target_name)));
        prepared.push(PreparedItem {
            original: item.clone(),
            source_parent,
            source_name,
            expected,
            claim_name,
            files_dir,
            info_dir,
            target_name,
            trash_path,
            info_path,
            needs_trashinfo: destination.needs_trashinfo,
        });
    }

    let batch_id = unique_id();
    let journal_path = journal_dir.join(format!("trash-{batch_id}.json"));
    let mut journal = TrashJournal {
        version: JOURNAL_VERSION,
        batch_id,
        entries: prepared
            .iter()
            .map(|item| JournalEntry {
                original: item.original.clone(),
                expected: item.expected,
                claim_name: item.claim_name.to_string_lossy().into_owned(),
                trash_path: item.trash_path.clone(),
                info_path: item.info_path.clone(),
                status: JournalStatus::Planned,
                failure: None,
            })
            .collect(),
    };
    write_journal(&journal_path, &journal, true)?;

    let mut stats = TrashStats::default();
    for (index, item) in prepared.iter().enumerate() {
        before_claim(index);
        match trash_one(
            item,
            &journal_path,
            &mut journal,
            index,
            before_transfer,
            write_journal,
        ) {
            Ok(()) => stats.trashed += 1,
            Err(error) => {
                if crate::error_code(&error) == crate::ErrorCode::OutcomeUnclear
                    && journal.entries[index].failure.is_some()
                {
                    stats.unclear += 1;
                    stats
                        .errors
                        .push(format!("{}: {error}", item.original.display()));
                    stats
                        .recovery_paths
                        .push(item.trash_path.display().to_string());
                    stats.recovery_paths.push(
                        item.original
                            .with_file_name(&item.claim_name)
                            .display()
                            .to_string(),
                    );
                    continue;
                }
                // Ob der Eintrag den Papierkorb erreicht hat, entscheidet allein die
                // Identitaet am Zielnamen. Ein Stat-Fehler ist dabei kein
                // "nicht angekommen": Er sagt gar nichts. Ihn als Misserfolg zu buchen
                // haette eine bereits vollzogene Verschiebung dem Nutzer als
                // "Loeschen fehlgeschlagen" gemeldet.
                let reached_trash = identity_at_checked(&item.files_dir, &item.target_name)
                    .map(|found| found == Some(item.expected));
                match &reached_trash {
                    Ok(true) => {
                        // Der Daten-Rename ist der eigentliche Papierkorb-Erfolg.
                        // Scheitert nur das anschliessende Journal-Update, bleibt die
                        // Bilanz ehrlich. Ein noch persistierter Transferhinweis
                        // bleibt bis zur manuellen Prüfung sichtbar; ein bereits
                        // committetes `trashed` belegt dagegen den positiven Readback.
                        stats.trashed += 1;
                        if journal.entries[index].failure.is_some() {
                            stats
                                .recovery_paths
                                .push(item.trash_path.display().to_string());
                            stats.recovery_paths.push(
                                item.original
                                    .with_file_name(&item.claim_name)
                                    .display()
                                    .to_string(),
                            );
                        }
                        stats.errors.push(format!(
                            "{} wurde verschoben, aber das Crash-Journal meldete: {error}",
                            item.original.display()
                        ));
                    }
                    Ok(false) => {
                        stats.failed += 1;
                        stats
                            .errors
                            .push(format!("{}: {error}", item.original.display()));
                    }
                    Err(stat_error) => {
                        stats.unclear += 1;
                        stats.errors.push(format!(
                            "{}: {error}; ob der Eintrag den Papierkorb erreicht hat, liess sich nicht feststellen ({stat_error})",
                            item.original.display()
                        ));
                    }
                }
                // Der Claimname ist ein Wiederherstellungshinweis, kein Beweis. Bleibt
                // die Nachfrage unbeantwortet, wird er lieber genannt als verschwiegen.
                let claim_may_remain =
                    match identity_at_checked(&item.source_parent, &item.claim_name) {
                        Ok(found) => found == Some(item.expected),
                        Err(_) => true,
                    };
                if !matches!(reached_trash, Ok(true)) && claim_may_remain {
                    stats.recovery_paths.push(
                        item.original
                            .with_file_name(&item.claim_name)
                            .display()
                            .to_string(),
                    );
                }
            }
        }
    }

    cleanup_journal_if_settled(&journal_path, &journal)?;
    Ok(stats)
}

pub(super) fn trash_one(
    item: &PreparedItem,
    journal_path: &Path,
    journal: &mut TrashJournal,
    index: usize,
    before_transfer: &mut dyn FnMut(&PreparedItem),
    write_journal: &mut dyn FnMut(&Path, &TrashJournal, bool) -> io::Result<()>,
) -> io::Result<()> {
    renameat_with(
        &item.source_parent,
        &item.source_name,
        &item.source_parent,
        &item.claim_name,
        RenameFlags::NOREPLACE,
    )?;

    if identity_at(&item.source_parent, &item.claim_name) != Some(item.expected) {
        restore_claim(item)?;
        return Err(io::Error::other(
            "Eintrag wurde zwischen Vorpruefung und Claim ausgetauscht; Papierkorb-Transfer gestoppt",
        ));
    }

    journal.entries[index].status = JournalStatus::Claimed;
    if let Err(error) = write_journal(journal_path, journal, false) {
        journal.entries[index].status = JournalStatus::Planned;
        let restore = restore_claim(item);
        return match restore {
            Ok(()) => Err(error),
            Err(restore_error) => Err(io::Error::other(format!(
                "Journal konnte nach Claim nicht aktualisiert werden ({error}); Claim liegt unter {} und konnte nicht zurueckgesetzt werden ({restore_error})",
                item.original.with_file_name(&item.claim_name).display()
            ))),
        };
    }

    let info_created = if item.needs_trashinfo {
        match create_trashinfo(item) {
            Ok(()) => true,
            Err(error) => {
                restore_claim(item)?;
                return Err(error);
            }
        }
    } else {
        false
    };

    // Die Wirkung wird schon vor dem Rename als ungeprüft festgehalten. Scheitert
    // später das Speichern des Readbackfehlers, bleibt damit trotzdem ein Blocker.
    journal.entries[index].failure =
        Some("Papierkorb-Transfer begonnen; abschließender Identitätsbeleg steht aus".into());
    if let Err(error) = write_journal(journal_path, journal, false) {
        journal.entries[index].failure = None;
        if info_created {
            remove_reserved_info(item);
        }
        restore_claim(item)?;
        return Err(error);
    }
    before_transfer(item);
    let moved = renameat_with(
        &item.source_parent,
        &item.claim_name,
        &item.files_dir,
        &item.target_name,
        RenameFlags::NOREPLACE,
    );
    if let Err(error) = moved {
        if !matches!(identity_at_checked(&item.source_parent, &item.claim_name), Ok(Some(identity)) if identity == item.expected)
        {
            return Err(crate::coded_error(io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!("Papierkorb-Transfer scheiterte ({error}); der Claim besitzt nicht mehr die bestätigte Identität. Keine automatische Rückbenennung oder Entfernung wurde versucht")));
        }
        journal.entries[index].failure = None;
        if info_created {
            remove_reserved_info(item);
        }
        restore_claim(item)?;
        return Err(error.into());
    }
    // Vor dem Transfer lag der Claim im gemeinsam beschreibbaren Quellordner.
    // Seine frühere Prüfung bindet nicht den späteren Namen: Ein fremder Nachfolger darf
    // deshalb nach dem Transfer weder als Erfolg gelten noch automatisch zurück.
    match identity_at_checked(&item.files_dir, &item.target_name) {
        Ok(Some(identity)) if identity == item.expected => {}
        result => {
            let detail = match result {
                Ok(_) => {
                    "Der Papierkorb-Zieleintrag besitzt nicht die bestätigte Identität".to_string()
                }
                Err(error) => {
                    format!("Der Papierkorb-Zieleintrag konnte nicht geprüft werden: {error}")
                }
            };
            journal.entries[index].failure = Some(detail.clone());
            let persist_error = write_journal(journal_path, journal, false).err();
            return Err(crate::coded_error(io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!("{detail}. Keine automatische Rückbenennung oder Entfernung wurde versucht. Recoverypfade: Ziel: {}; Claim: {}{}",
                    item.trash_path.display(),
                    item.original.with_file_name(&item.claim_name).display(),
                    persist_error.map_or(String::new(), |error| format!("; Journalbestätigung scheiterte: {error}")))));
        }
    }
    fsync(&item.source_parent)?;
    fsync(&item.files_dir)?;

    journal.entries[index].status = JournalStatus::Trashed;
    journal.entries[index].failure = None;
    if let Err(error) = write_journal(journal_path, journal, false) {
        // Auf Platte kann noch `claimed` oder bereits `trashed` stehen: Auch der
        // Eltern-fsync nach erfolgreichem Journal-Rename kann scheitern. RAM erhält
        // das Journal; ein lesbares `trashed` belegt den positiven Identitätsreadback,
        // ein verbliebener Transferhinweis verlangt manuelle Prüfung.
        journal.entries[index].status = JournalStatus::Claimed;
        journal.entries[index].failure =
            Some("Identitätsbeleg erfolgreich; dauerhafter Journalabschluss steht aus".into());
        return Err(error);
    }
    Ok(())
}

pub(super) fn restore_claim(item: &PreparedItem) -> io::Result<()> {
    renameat_with(
        &item.source_parent,
        &item.claim_name,
        &item.source_parent,
        &item.source_name,
        RenameFlags::NOREPLACE,
    )?;
    fsync(&item.source_parent).map_err(Into::into)
}

pub(super) fn remove_reserved_info(item: &PreparedItem) {
    if let (Some(info_dir), Some(info_path)) = (&item.info_dir, &item.info_path) {
        if let Some(name) = info_path.file_name() {
            let _ = unlinkat(info_dir, name, AtFlags::empty());
            let _ = fsync(info_dir);
        }
    }
}

pub(super) fn create_trashinfo(item: &PreparedItem) -> io::Result<()> {
    let info_dir = item
        .info_dir
        .as_ref()
        .ok_or_else(|| io::Error::other("XDG-Papierkorb ohne info-Verzeichnis"))?;
    let name = info_name(&item.target_name);
    let fd = openat(
        info_dir,
        &name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        INFO_FILE_MODE,
    )?;
    let mut file = File::from(fd);
    let content = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode_path(&item.original),
        local_deletion_time()
    );
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fsync(info_dir).map_err(Into::into)
}

pub(super) fn cleanup_journal_if_settled(path: &Path, journal: &TrashJournal) -> io::Result<()> {
    let settled = journal.entries.iter().all(|entry| {
        entry.status == JournalStatus::Trashed
            || (entry.failure.is_none() && path_identity(&entry.original) == Some(entry.expected))
    });
    if settled {
        remove_file(path)?;
        sync_parent(path)?;
    }
    Ok(())
}

pub(super) fn persist_journal(path: &Path, journal: &TrashJournal, create: bool) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| invalid_path(path))?;
    if !parent.exists() {
        let ancestor = parent.parent().ok_or_else(|| invalid_path(parent))?;
        crate::local_fs::create_dir_all(ancestor)?;
    }
    ensure_private_directory(parent)?;

    let payload = serde_json::to_vec_pretty(journal)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if payload.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Papierkorb-Journal überschreitet die Größenbegrenzung",
        ));
    }
    atomic_write_local(
        path,
        &payload,
        ".journal-",
        if create {
            AtomicWriteMode::Create
        } else {
            AtomicWriteMode::Replace
        },
    )
}

pub(super) fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| invalid_path(path))?;
    sync_directory(parent)
}

pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
    let directory = open_dir_nofollow(path, false)?;
    fsync(directory).map_err(Into::into)
}

pub(super) fn open_source_parent(
    item: &Path,
    sandbox: &Option<PathBuf>,
    sandbox_fd: Option<&OwnedFd>,
) -> io::Result<(OwnedFd, OsString)> {
    let Some(root) = sandbox else {
        return open_parent(item);
    };
    let relative = item.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Sicherheits-Sandbox aktiv: {} liegt nicht unter {}",
                item.display(),
                root.display()
            ),
        )
    })?;
    let mut components: Vec<_> = relative.components().collect();
    let final_component = components.pop().ok_or_else(|| invalid_path(item))?;
    let Component::Normal(name) = final_component else {
        return Err(invalid_path(item));
    };
    let mut current = dup(sandbox_fd.ok_or_else(|| invalid_path(root))?)?;
    for component in components {
        let Component::Normal(name) = component else {
            return Err(invalid_path(item));
        };
        current = openat(&current, name, dir_flags(), Mode::empty())?;
    }
    Ok((current, name.to_os_string()))
}

pub(super) fn reserve_claim_name(parent: &OwnedFd) -> io::Result<OsString> {
    for _ in 0..1000 {
        let name = OsString::from(format!(".doppeldecker-trash-claim-{}", unique_id()));
        if identity_at(parent, &name).is_none() {
            return Ok(name);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Kein freier Claim-Name gefunden",
    ))
}

pub(super) fn reserve_target_name(
    files: &OwnedFd,
    info: Option<&OwnedFd>,
    original: &OsStr,
    files_path: &Path,
    reserved: &mut HashSet<(PathBuf, OsString)>,
) -> io::Result<OsString> {
    // Linux legt zum Dateinamen eine gleichnamige Sidecar-Datei an. Ihr Suffix
    // muss schon vor dem Claim in dieselbe Dateisystem-Namensgrenze passen.
    let max_bytes = MAX_TRASH_NAME_BYTES
        - if info.is_some() {
            TRASHINFO_SUFFIX.len()
        } else {
            0
        };
    for suffix in 0..100_000_u32 {
        let candidate = collision_name(original, suffix, max_bytes);
        let key = (files_path.to_path_buf(), candidate.clone());
        let data_free = identity_at(files, &candidate).is_none();
        let info_free = info
            .map(|directory| identity_at(directory, &info_name(&candidate)).is_none())
            .unwrap_or(true);
        if data_free && info_free && reserved.insert(key) {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "Kein freier Papierkorb-Name fuer {}",
            original.to_string_lossy()
        ),
    ))
}

pub(super) fn identity_at(parent: &OwnedFd, name: &OsStr) -> Option<Identity> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Some(Identity::from_stat(&stat)),
        Err(rustix::io::Errno::NOENT) => None,
        Err(_) => None,
    }
}

pub(super) fn path_identity(path: &Path) -> Option<Identity> {
    let (parent, name) = open_parent(path).ok()?;
    identity_at(&parent, &name)
}

pub(super) fn ensure_private_directory(path: &Path) -> io::Result<OwnedFd> {
    let (parent, name) = open_parent(path)?;
    match mkdirat(&parent, &name, PRIVATE_DIR_MODE) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(error.into()),
    }
    let directory = openat(&parent, &name, dir_flags(), Mode::empty())?;
    let stat = fstat(&directory)?;
    let uid = rustix::process::getuid().as_raw();
    if !FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != uid
        || (stat.st_mode as u32 & 0o077) != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Unsichere Eigentums- oder Rechteverhaeltnisse am Papierkorb-Ordner {}",
                path.display()
            ),
        ));
    }
    Ok(directory)
}

pub(super) fn prepare_destination(
    destination: &TrashDestination,
    source_device: u64,
) -> io::Result<(OwnedFd, Option<OwnedFd>)> {
    if destination.create_base {
        let parent = destination
            .root
            .parent()
            .ok_or_else(|| invalid_path(&destination.root))?;
        crate::local_fs::create_dir_all(parent)?;
    }
    let root = ensure_private_directory(&destination.root)?;
    if fstat(&root)?.st_dev as u64 != source_device {
        return Err(io::Error::new(
            io::ErrorKind::CrossesDevices,
            "Papierkorb-Ziel liegt auf einem anderen Dateisystem; Copy-and-delete ist aus Sicherheitsgruenden gesperrt",
        ));
    }
    let files = ensure_private_directory(&destination.files)?;
    let info = destination
        .info
        .as_ref()
        .map(|path| ensure_private_directory(path))
        .transpose()?;
    Ok((files, info))
}

pub(super) fn unique_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{counter}", std::process::id())
}
