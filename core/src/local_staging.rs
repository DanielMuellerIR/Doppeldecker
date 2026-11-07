//! Private, laufgebundene Zwischenstände. Sichtbare Pfade sind nur Recoveryhinweise;
//! jede Wirkung verwendet die während der Vorbereitung gehaltenen Deskriptoren.

use crate::local_fs::RootHandle;
use crate::metadata::{Acl, PosixAcl, PosixAclTag};
use rustix::fs::{fchmod, fchown, fstat, openat, renameat, Gid, Mode, OFlags};
use std::fs::File;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct StagingOwner {
    pub(crate) queue: PathBuf,
    pub(crate) operation: String,
}

/// Auf Volumes ohne privaten Namensraum bleibt die Tempdatei unangetastet.
/// Ihr vor der Anlage dauerhaft gespeicherter Index lebt im privaten Queueordner;
/// er ist ausschließlich ein Prüfhinweis und erlaubt niemals eine Datenlöschung.
pub(crate) struct NeighborStaging {
    config: RootHandle,
    pub(crate) parent: RootHandle,
    index: PathBuf,
    record: Mutex<NeighborRecord>,
    _lease: File,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct NeighborRecord {
    version: u32,
    owner: StagingOwner,
    target: PathBuf,
    candidate: Option<PathBuf>,
    #[serde(default)]
    remote: bool,
    #[serde(default)]
    stage_guard: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ftps: Option<FtpsIntentBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<crate::EndpointBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint_root: Option<crate::RootIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    named_stream: Option<NamedStreamIntentBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directory_dacl: Option<DirectoryDaclIntentBinding>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum DirectoryDaclIntentPhase {
    Prepared,
    Applying,
    Applied,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectoryDaclIntentBinding {
    pub(crate) directory: PathBuf,
    pub(crate) base_id: u64,
    pub(crate) namespace: [u8; 16],
    pub(crate) before_digest: [u8; 32],
    pub(crate) after_digest: [u8; 32],
    pub(crate) owner: crate::windows_security::WindowsSid,
    pub(crate) group: Option<crate::windows_security::WindowsSid>,
    pub(crate) phase: DirectoryDaclIntentPhase,
}
impl DirectoryDaclIntentBinding {
    fn validate(&self) -> io::Result<()> {
        self.owner.validate()?;
        if let Some(group) = &self.group {
            group.validate()?;
        }
        if !self.directory.is_absolute()
            || self
                .directory
                .to_str()
                .is_none_or(|value| value.len() > 4096 || value.chars().any(char::is_control))
            || self.directory.components().any(|part| {
                !matches!(
                    part,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
            || self.base_id == 0
            || self.namespace == [0; 16]
            || self.before_digest == self.after_digest
        {
            return Err(io::Error::other("Ungültige Verzeichnis-DACL-Intentbindung"));
        }
        Ok(())
    }
    fn report(&self) -> crate::directory_dacl::DirectoryDaclReport {
        let outcome = crate::directory_dacl::DirectoryDaclOutcome {
            directory: self.directory.to_string_lossy().into_owned(),
            before_digest: self.before_digest,
            after_digest: self.after_digest,
        };
        let mut report = crate::directory_dacl::DirectoryDaclReport::default();
        match self.phase {
            DirectoryDaclIntentPhase::Prepared => {}
            DirectoryDaclIntentPhase::Applying => report.uncertain.push(outcome),
            DirectoryDaclIntentPhase::Applied => report.completed.push(outcome),
        }
        report
    }
}

/// Bestehende Metadatenobjekte besitzen eine eigene Klasse: weder Tempbesitz noch
/// eine verschwundene Datei können einen möglicherweise gesendeten SET auflösen.
pub(crate) struct RemoteDirectoryDaclIntent {
    config: RootHandle,
    index: PathBuf,
    record: Mutex<NeighborRecord>,
    _lease: File,
}
impl RemoteDirectoryDaclIntent {
    pub(crate) fn create_with_root(
        owner: &StagingOwner,
        binding: DirectoryDaclIntentBinding,
        identity: &crate::RootIdentity,
        account_digest: [u8; 32],
    ) -> io::Result<Self> {
        if binding.phase != DirectoryDaclIntentPhase::Prepared {
            return Err(io::Error::other("Neuer DACL-Intent muss Prepared sein"));
        }
        let config = RootHandle::open_nofollow(
            owner
                .queue
                .parent()
                .ok_or_else(|| io::Error::other("Queue ohne Elternordner"))?,
        )?;
        if !config.is_current()? || !config.has_exclusive_namespace()? {
            return Err(io::Error::other(
                "DACL-Intent benötigt privaten Queueordner",
            ));
        }
        let record = NeighborRecord {
            version: 5,
            owner: owner.clone(),
            target: binding.directory.clone(),
            candidate: None,
            remote: true,
            stage_guard: false,
            ftps: None,
            endpoint: Some(crate::EndpointBinding::remote(identity, account_digest)?),
            endpoint_root: Some(identity.clone()),
            named_stream: None,
            directory_dacl: Some(binding),
        };
        validate_neighbor(&record, config.path())?;
        let _lock = lock_intent_events(&config)?;
        repair_intent_tail(&config)?;
        let mut name =
            crate::atomic_file::sibling_candidate(&config.path().join("neighbor"), "recovery")?
                .into_os_string();
        name.push(".recovery.json");
        let index = PathBuf::from(name);
        let lease = File::from(openat(
            config.fd(),
            intent_lease_path(&index).file_name().unwrap(),
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        rustix::fs::flock(&lease, rustix::fs::FlockOperation::LockExclusive)?;
        lease.sync_all()?;
        let vfs = crate::LocalVfs::rooted(config.path())?;
        if crate::Vfs::anchored_identity(&vfs) != Some(config.identity()) {
            return Err(io::Error::other("DACL-Intentordner wurde ausgetauscht"));
        }
        crate::atomic_write::atomic_write_vfs_with_mode(
            &vfs,
            &index,
            &encode_neighbor_record(&record)?,
            ".directory-dacl-intent-",
            crate::atomic_write::AtomicWriteMode::Create,
            Some(0o600),
        )?;
        append_intent_event_locked(&config, &record, "start")?;
        Ok(Self {
            config,
            index,
            record: Mutex::new(record),
            _lease: lease,
        })
    }
    pub(crate) fn phase(&self, phase: DirectoryDaclIntentPhase) -> io::Result<()> {
        let mut record = self
            .record
            .lock()
            .map_err(|_| io::Error::other("DACL-Intent gesperrt"))?;
        let mut next = record.clone();
        let binding = next
            .directory_dacl
            .as_mut()
            .ok_or_else(|| io::Error::other("DACL-Intent fehlt"))?;
        use DirectoryDaclIntentPhase::*;
        if !matches!(
            (binding.phase, phase),
            (Prepared, Applying) | (Applying, Applied)
        ) && binding.phase != phase
        {
            return Err(io::Error::other("Ungültiger DACL-Phasenwechsel"));
        }
        binding.phase = phase;
        validate_neighbor(&next, self.config.path())?;
        self.ensure_current()?;
        let _lock = lock_intent_events(&self.config)?;
        // WAL vor Index: ein bestätigter Phasenwechsel überlebt auch einen
        // anschließenden fehlgeschlagenen Snapshot. SET beginnt erst nach Ok.
        *record = next;
        append_intent_event_locked(&self.config, &record, "update")?;
        let vfs = crate::LocalVfs::rooted(self.config.path())?;
        if crate::Vfs::anchored_identity(&vfs) != Some(self.config.identity()) {
            return Err(io::Error::other("DACL-Intentordner wurde ausgetauscht"));
        }
        crate::atomic_write::atomic_write_vfs_with_mode(
            &vfs,
            &self.index,
            &encode_neighbor_record(&record)?,
            ".directory-dacl-intent-",
            crate::atomic_write::AtomicWriteMode::Replace,
            Some(0o600),
        )
    }
    pub(crate) fn report(&self) -> crate::directory_dacl::DirectoryDaclReport {
        self.record
            .lock()
            .ok()
            .and_then(|record| {
                record
                    .directory_dacl
                    .as_ref()
                    .map(DirectoryDaclIntentBinding::report)
            })
            .unwrap_or_default()
    }
    pub(crate) fn path(&self) -> &Path {
        &self.index
    }
    fn ensure_current(&self) -> io::Result<()> {
        if !self.config.is_current()? || !self.config.has_exclusive_namespace()? {
            return Err(io::Error::other("DACL-Intentordner wurde ausgetauscht"));
        }
        Ok(())
    }
    pub(crate) fn finish_prepared(&self) -> io::Result<()> {
        self.close(DirectoryDaclIntentPhase::Prepared)
    }
    pub(crate) fn finish(&self) -> io::Result<()> {
        self.close(DirectoryDaclIntentPhase::Applied)
    }
    fn close(&self, expected: DirectoryDaclIntentPhase) -> io::Result<()> {
        self.ensure_current()?;
        let record = self
            .record
            .lock()
            .map_err(|_| io::Error::other("DACL-Intent gesperrt"))?;
        if record
            .directory_dacl
            .as_ref()
            .is_none_or(|binding| binding.phase != expected)
        {
            return Err(io::Error::other(
                "DACL-Intentphase erlaubt keinen Abschluss",
            ));
        }
        let _lock = lock_intent_events(&self.config)?;
        // Der vollständige Applied-Datensatz bleibt im fsync-bestätigten WAL,
        // auch wenn Queue-Sicherung oder Indexentfernung danach scheitert.
        append_intent_event_locked(&self.config, &record, "finish")?;
        remove_directory_dacl_index(&self.config, &self.index)
    }
}
fn remove_directory_dacl_index(config: &RootHandle, index: &Path) -> io::Result<()> {
    crate::local_fs::remove_file_beneath(config, index)?;
    match crate::local_fs::remove_file_beneath(config, &intent_lease_path(index)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    crate::local_fs::sync_dir_beneath(config, config.path())
}
struct DirectoryDaclJournalState {
    record: NeighborRecord,
    report: crate::directory_dacl::DirectoryDaclReport,
    closed: bool,
}
fn directory_dacl_key(record: &NeighborRecord) -> io::Result<Vec<u8>> {
    let binding = record
        .directory_dacl
        .as_ref()
        .ok_or_else(|| io::Error::other("Kein DACL-Intent"))?;
    serde_json::to_vec(&(
        &record.owner,
        &record.endpoint,
        &record.endpoint_root,
        &binding.directory,
        binding.base_id,
        binding.namespace,
        binding.before_digest,
        binding.after_digest,
        &binding.owner,
        &binding.group,
    ))
    .map_err(io::Error::other)
}
fn directory_dacl_journal(
    config: &RootHandle,
) -> io::Result<std::collections::BTreeMap<Vec<u8>, DirectoryDaclJournalState>> {
    let mut states = std::collections::BTreeMap::<Vec<u8>, DirectoryDaclJournalState>::new();
    let file = match crate::local_fs::open_read_beneath(config, &config.path().join(INTENT_EVENTS))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(states),
        Err(error) => return Err(error),
    };
    let mut reader = std::io::BufReader::new(file);
    loop {
        let line = read_intent_line(&mut reader)?;
        if line.is_empty() {
            break;
        }
        if !line.ends_with(b"\n") {
            return Err(io::Error::other("DACL-Ereignisjournal ist unvollständig"));
        }
        let IntentEvent::Record((phase, record)) = decode_intent_event(&line)? else {
            continue;
        };
        if record.directory_dacl.is_none() {
            continue;
        }
        validate_neighbor(&record, config.path())?;
        let key = directory_dacl_key(&record)?;
        let mut record = *record;
        if phase == "interrupted"
            && record
                .directory_dacl
                .as_ref()
                .is_some_and(|binding| binding.phase == DirectoryDaclIntentPhase::Prepared)
        {
            record.directory_dacl.as_mut().unwrap().phase = DirectoryDaclIntentPhase::Applying;
        }
        let report = record.directory_dacl.as_ref().unwrap().report();
        let state = states
            .entry(key)
            .or_insert_with(|| DirectoryDaclJournalState {
                record: record.clone(),
                report: Default::default(),
                closed: false,
            });
        state.report.merge(&report);
        if state.record.directory_dacl.as_ref().unwrap().phase == DirectoryDaclIntentPhase::Applied
        {
            record.directory_dacl.as_mut().unwrap().phase = DirectoryDaclIntentPhase::Applied;
        }
        if phase == "recovery_before"
            && record.directory_dacl.as_ref().unwrap().phase != DirectoryDaclIntentPhase::Applied
        {
            state.report.uncertain.clear();
        } else if phase == "recovery_after" {
            let mut applied = record.directory_dacl.as_ref().unwrap().clone();
            applied.phase = DirectoryDaclIntentPhase::Applied;
            state.report.merge(&applied.report());
        }
        state.closed = matches!(
            phase.as_str(),
            "finish" | "recovery_before" | "recovery_after"
        );
        state.record = record;
    }
    Ok(states)
}
fn apply_directory_dacl_journal(
    record: &mut NeighborRecord,
    states: &std::collections::BTreeMap<Vec<u8>, DirectoryDaclJournalState>,
) -> io::Result<()> {
    if record.directory_dacl.is_some() {
        if let Some(state) = states.get(&directory_dacl_key(record)?) {
            record.directory_dacl = state.record.directory_dacl.clone();
        }
    }
    Ok(())
}

/// Prüft nur lokale typisierte Belege; insbesondere ist Prepared auch ohne
/// Wirkungsbericht offen. Beschädigte oder unvollständige Ledger bleiben Fehler.
pub(crate) fn has_open_directory_dacl_intent_beneath(
    owner: &StagingOwner,
    config: &RootHandle,
) -> io::Result<bool> {
    if owner.queue.parent() != Some(config.path())
        || !config.is_current()?
        || !config.has_exclusive_namespace()?
    {
        return Err(io::Error::other(
            "DACL-Owner und gehaltener Queueordner sind nicht sicher gebunden",
        ));
    }
    let lock =
        match crate::local_fs::open_read_beneath(config, &config.path().join(INTENT_EVENTS_LOCK)) {
            Ok(file) => {
                rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
                Some(file)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
    let states = directory_dacl_journal(config)?;
    let mut open = states
        .values()
        .any(|state| state.record.owner == *owner && !state.closed);
    for entry in crate::local_fs::list_beneath(config, config.path())? {
        if !entry.name.starts_with(".neighbor.doppeldecker-recovery-")
            || !entry.name.ends_with(".recovery.json")
        {
            continue;
        }
        let (record, _) = read_neighbor(config, &config.path().join(&entry.name))?;
        if record.owner != *owner || record.directory_dacl.is_none() {
            continue;
        }
        if lock.is_none() {
            return Err(io::Error::other("DACL-Index ohne Registrylock"));
        }
        open |= !states
            .get(&directory_dacl_key(&record)?)
            .is_some_and(|state| state.closed);
    }
    if !config.is_current()? {
        return Err(io::Error::other(
            "DACL-Queueordner änderte sich während der Prüfung",
        ));
    }
    Ok(open)
}

/// Ausschließlich bewusst aufgerufener lesender Abschluss. Kein SET, Restore,
/// Retry oder Entfernen des entfernten Ordners wird hieraus abgeleitet.
#[cfg(test)]
pub(crate) fn finish_directory_dacl_recovery(
    owner: &StagingOwner,
    vfs: &dyn crate::Vfs,
    root: &Path,
    account: [u8; 32],
) -> io::Result<crate::directory_dacl::DirectoryDaclReport> {
    let config = RootHandle::open_nofollow(
        owner
            .queue
            .parent()
            .ok_or_else(|| io::Error::other("Queue ohne Elternordner"))?,
    )?;
    Ok(finish_directory_dacl_recovery_beneath(owner, vfs, root, account, &config)?.report)
}

pub(crate) struct DirectoryDaclAcknowledgement {
    pub(crate) report: crate::directory_dacl::DirectoryDaclReport,
    pub(crate) paths: Vec<String>,
}

pub(crate) fn finish_directory_dacl_recovery_beneath(
    owner: &StagingOwner,
    vfs: &dyn crate::Vfs,
    root: &Path,
    account: [u8; 32],
    config: &RootHandle,
) -> io::Result<DirectoryDaclAcknowledgement> {
    if owner.queue.parent() != Some(config.path()) {
        return Err(io::Error::other(
            "DACL-Owner und gehaltener Queueordner unterscheiden sich",
        ));
    }
    if !config.is_current()? || !config.has_exclusive_namespace()? {
        return Err(io::Error::other("DACL-Recoveryordner wurde ausgetauscht"));
    }
    let _lock = lock_intent_events(config)?;
    repair_intent_tail(config)?;
    let states = directory_dacl_journal(config)?;
    let identity = vfs.root_identity(root)?;
    let endpoint = crate::EndpointBinding::remote(&identity, account)?;
    let mut report = crate::directory_dacl::DirectoryDaclReport::default();
    // Erst sämtliche offenen Objekte prüfen, dann die lokalen Belege schließen.
    let mut verified = Vec::new();
    for entry in crate::local_fs::list_beneath(config, config.path())? {
        if !entry.name.starts_with(".neighbor.doppeldecker-recovery-")
            || !entry.name.ends_with(".recovery.json")
        {
            continue;
        }
        let index = config.path().join(&entry.name);
        let (mut record, _) = read_neighbor(config, &index)?;
        if record.owner != *owner || record.directory_dacl.is_none() {
            continue;
        }
        apply_directory_dacl_journal(&mut record, &states)?;
        let binding = record.directory_dacl.as_ref().unwrap();
        let check = || -> io::Result<bool> {
            if writer_active(config, &index, &record)? {
                return Err(io::Error::other(
                    "DACL-Intent besitzt noch einen aktiven Writer",
                ));
            }
            if record.endpoint.as_ref() != Some(&endpoint)
                || record
                    .endpoint_root
                    .as_ref()
                    .is_none_or(|old| !same_recovery_root(old, &identity))
            {
                return Err(io::Error::other(
                    "DACL-Recovery-Endpunkt oder Root änderte sich",
                ));
            }
            // ID, Owner/Group und DACL müssen aus demselben gehaltenen
            // Directoryhandle stammen. Getrennte Stat-/Manifest-Reads erlauben
            // einen fremden Austausch und Rücktausch zwischen den Beobachtungen.
            let observed =
                vfs.directory_dacl_recovery_observation(&binding.directory, &identity)?;
            observed.context.validate()?;
            observed.dacl.validate()?;
            if observed.base_id != binding.base_id {
                return Err(io::Error::other(
                    "DACL-Recovery-Basisidentität änderte sich",
                ));
            }
            if observed.context.namespace != binding.namespace
                || observed.context.owner != binding.owner
                || observed.context.group != binding.group
            {
                return Err(io::Error::other(
                    "DACL-Recovery-Owner oder Gruppe änderte sich",
                ));
            }
            let digest = crate::directory_dacl::dacl_digest(&observed.dacl)?;
            let after = digest == binding.after_digest;
            if digest != binding.before_digest && !after {
                return Err(io::Error::other(
                    "DACL-Recovery beobachtet einen dritten DACL-Wert",
                ));
            }
            if binding.phase == DirectoryDaclIntentPhase::Prepared && after {
                return Err(io::Error::other(
                    "Prepared-DACL entspricht nicht mehr dem Vorzustand",
                ));
            }
            if vfs.root_identity(root)? != identity {
                return Err(io::Error::other(
                    "DACL-Recovery-Bindung änderte sich beim Readback",
                ));
            }
            Ok(after)
        };
        let after = check().map_err(|error| {
            crate::error::with_recovery_paths(
                crate::coded_error(
                    io::ErrorKind::WouldBlock,
                    crate::ErrorCode::OutcomeUnclear,
                    error.to_string(),
                ),
                &[
                    binding.directory.to_string_lossy().into_owned(),
                    index.to_string_lossy().into_owned(),
                ],
            )
        })?;
        verified.push((index, record, after));
    }
    for (key, state) in &states {
        if state.record.owner == *owner
            && !state.closed
            && !verified.iter().any(|(_, record, _)| {
                directory_dacl_key(record)
                    .as_ref()
                    .is_ok_and(|candidate| candidate == key)
            })
        {
            return Err(crate::error::with_recovery_paths(crate::coded_error(io::ErrorKind::WouldBlock, crate::ErrorCode::OutcomeUnclear,
                "Offener DACL-WAL-Beleg besitzt keinen prüfbaren Recoveryindex und keine prüfbare Writerlease"), &[state.record.target.to_string_lossy().into_owned()]));
        }
    }
    let mut paths = Vec::new();
    for (index, record, after) in verified {
        append_intent_event_locked(
            config,
            &record,
            if after {
                "recovery_after"
            } else {
                "recovery_before"
            },
        )?;
        let binding = record.directory_dacl.as_ref().unwrap();
        if after || binding.phase == DirectoryDaclIntentPhase::Applied {
            let mut applied = binding.clone();
            applied.phase = DirectoryDaclIntentPhase::Applied;
            report.merge(&applied.report());
        }
        paths.push(record.target.to_string_lossy().into_owned());
        paths.push(index.to_string_lossy().into_owned());
        remove_directory_dacl_index(config, &index)?;
    }
    // Ein fehlgeschlagener Queuewrite darf beim wiederholten bewussten
    // Abschluss keinen schon dauerhaft gespeicherten Ergebnisbeleg verlieren.
    for state in directory_dacl_journal(config)?
        .values()
        .filter(|state| state.record.owner == *owner)
    {
        report.merge(&state.report);
        if state.closed {
            paths.push(state.record.target.to_string_lossy().into_owned());
        }
    }
    paths.sort();
    paths.dedup();
    Ok(DirectoryDaclAcknowledgement { report, paths })
}
pub(crate) fn same_recovery_root(left: &crate::RootIdentity, right: &crate::RootIdentity) -> bool {
    match (left, right) {
        (crate::RootIdentity::Smb(left), crate::RootIdentity::Smb(right)) => {
            left.server == right.server
                && left.virtual_root == right.virtual_root
                && left.root_file_id == right.root_file_id
        }
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum NamedStreamIntentPhase {
    Prepared,
    Committing,
    Committed,
    Removing,
    Removed,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct NamedStreamIntentBinding {
    pub(crate) base_directory: PathBuf,
    pub(crate) temporary_name: Option<Vec<u8>>,
    pub(crate) final_name: Vec<u8>,
    pub(crate) effect: crate::directory_streams::DirectoryStreamEffect,
    pub(crate) phase: NamedStreamIntentPhase,
}
impl NamedStreamIntentBinding {
    fn validate(&self) -> io::Result<()> {
        use NamedStreamIntentPhase::*;
        self.effect.validate()?;
        crate::metadata::validate_named_stream_name(&self.final_name)?;
        if let Some(name) = &self.temporary_name {
            crate::metadata::validate_named_stream_name(name)?;
        }
        let remove = matches!(
            self.effect,
            crate::directory_streams::DirectoryStreamEffect::Remove { .. }
        );
        if !self.base_directory.is_absolute()
            || self.base_directory.components().any(|part| {
                !matches!(
                    part,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
            || self.effect.name() != self.final_name
            || self.temporary_name.as_ref() == Some(&self.final_name)
            || (remove && self.temporary_name.is_some())
            || (!remove && self.temporary_name.is_none())
            || (remove && matches!(self.phase, Committing | Committed))
            || (!remove && matches!(self.phase, Removing | Removed))
        {
            return Err(io::Error::other(
                "Ungültige Verzeichnisstream-Intentbindung",
            ));
        }
        Ok(())
    }
    fn objects(&self) -> Vec<StagingObject> {
        use NamedStreamIntentPhase::*;
        let mut names = Vec::new();
        if matches!(self.phase, Prepared | Committing | Committed) {
            names.extend(self.temporary_name.iter().cloned());
        }
        if matches!(self.phase, Committing | Committed | Removing | Removed)
            || self.temporary_name.is_none()
        {
            names.push(self.final_name.clone());
        }
        names
            .into_iter()
            .map(|name| StagingObject::NamedStream {
                base_directory: self.base_directory.clone(),
                name,
            })
            .collect()
    }
    fn report(&self) -> crate::directory_streams::DirectoryStreamReport {
        use NamedStreamIntentPhase::*;
        let outcome = crate::directory_streams::DirectoryStreamOutcome {
            directory: self.base_directory.to_string_lossy().into_owned(),
            effect: self.effect.clone(),
        };
        let mut report = crate::directory_streams::DirectoryStreamReport::default();
        match self.phase {
            Committed | Removed => report.completed.push(outcome),
            Committing | Removing => report.uncertain.push(outcome),
            Prepared => {}
        }
        report
    }
}

/// Ein Streamlocator darf niemals als gewöhnlicher Pfad gestattet oder gelöscht
/// werden. Seine Abwesenheit verlangt die vollständig beobachtete Streammenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StagingObject {
    Path(PathBuf),
    DirectoryDacl(PathBuf),
    NamedStream {
        base_directory: PathBuf,
        name: Vec<u8>,
    },
}
impl StagingObject {
    pub(crate) fn base_path(&self) -> &Path {
        match self {
            Self::Path(path) | Self::DirectoryDacl(path) => path,
            Self::NamedStream { base_directory, .. } => base_directory,
        }
    }
    pub(crate) fn recovery_hint(&self) -> String {
        match self {
            Self::Path(path) => path.to_string_lossy().into_owned(),
            Self::DirectoryDacl(path) => format!("{} [Verzeichnis-DACL]", path.display()),
            Self::NamedStream {
                base_directory,
                name,
            } => format!(
                "{} [Datenstrom {:?}]",
                base_directory.display(),
                String::from_utf8_lossy(name)
            ),
        }
    }
    pub(crate) fn with_base_path(&self, path: PathBuf) -> Self {
        match self {
            Self::Path(_) => Self::Path(path),
            Self::DirectoryDacl(_) => Self::DirectoryDacl(path),
            Self::NamedStream { name, .. } => Self::NamedStream {
                base_directory: path,
                name: name.clone(),
            },
        }
    }
    pub(crate) fn is_absent(&self, vfs: &dyn crate::Vfs) -> io::Result<bool> {
        match self {
            Self::DirectoryDacl(_) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Verzeichnis-DACL benötigt bewussten frischen Recoveryabschluss",
            )),
            Self::Path(path) => match vfs.stat(path) {
                Ok(_) => Ok(false),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
                Err(error) => Err(error),
            },
            Self::NamedStream {
                base_directory,
                name,
            } => {
                match vfs.stat(base_directory) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
                    Err(error) => return Err(error),
                    Ok(entry) if entry.is_dir && !entry.is_symlink && !entry.is_file => {}
                    Ok(_) => {
                        return Err(io::Error::other(
                            "Verzeichnisstream-Basis ist kein echter Ordner",
                        ))
                    }
                }
                let manifest = vfs.metadata_manifest(base_directory)?;
                manifest.validate()?;
                let streams = manifest.named_streams.ok_or_else(|| {
                    io::Error::other("Verzeichnisstreammenge wurde nicht vollständig beobachtet")
                })?;
                let name = std::str::from_utf8(name).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Streamlocator besitzt kein UTF-8",
                    )
                })?;
                let key = crate::smb::named_stream_lookup_key(name);
                Ok(!streams.iter().any(|stream| {
                    std::str::from_utf8(&stream.name)
                        .is_ok_and(|name| crate::smb::named_stream_lookup_key(name) == key)
                }))
            }
        }
    }
}

fn encode_neighbor_record(record: &NeighborRecord) -> io::Result<Vec<u8>> {
    let data = serde_json::to_vec(record).map_err(io::Error::other)?;
    if data.len() > 64 * 1024 {
        return Err(io::Error::other(
            "Temp-Recoveryindex überschreitet Größenlimit",
        ));
    }
    Ok(data)
}

/// Dauerhafte Beobachtungsbindung, kein Permit und kein Löschbeleg. Der STOU-Name
/// bleibt bis zu seiner bestätigten Antwort ausdrücklich unbekannt.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct FtpsIntentBinding {
    pub(crate) roots: Vec<crate::remote::FtpsRootIdentity>,
    pub(crate) account_digest: [u8; 32],
    pub(crate) phase: String,
    pub(crate) stou_name_unknown: bool,
    pub(crate) paths: Vec<PathBuf>,
}

fn validate_ftps_intent(binding: &FtpsIntentBinding, target: &Path) -> io::Result<()> {
    if !(1..=2).contains(&binding.roots.len())
        || binding.account_digest == [0; 32]
        || binding.account_digest == *blake3::hash(&[]).as_bytes()
        || !matches!(
            binding.phase.as_str(),
            "mkdir" | "probe" | "stou" | "rename" | "delete"
        )
        || (binding.stou_name_unknown && binding.phase != "stou")
        || binding.paths.is_empty()
        || binding.paths.len() > 16
    {
        return Err(io::Error::other("Ungültige FTPS-Intentbindung"));
    }
    for root in &binding.roots {
        root.validate()?;
        if root.server != binding.roots[0].server
            || root.session_generation != binding.roots[0].session_generation
        {
            return Err(io::Error::other("FTPS-Intent vermischt Sitzungen"));
        }
    }
    for path in std::iter::once(target).chain(binding.paths.iter().map(PathBuf::as_path)) {
        if !path.is_absolute()
            || path
                .to_str()
                .is_none_or(|value| value.len() > 4096 || value.chars().any(char::is_control))
            || path.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
            || !binding.roots.iter().any(|root| {
                let scope = Path::new(&root.server.canonical_root)
                    .join(root.virtual_root.trim_start_matches('/'));
                path.starts_with(scope)
            })
        {
            return Err(io::Error::other(
                "FTPS-Intentpfad verlässt die gebundenen Roots",
            ));
        }
    }
    Ok(())
}

/// Ein Remote-Intent ist nur ein dauerhafter Prüfhinweis. Weder Servertoken noch
/// Secrets werden gespeichert; nach einem Absturz entsteht daraus kein Löschrecht.
pub(crate) struct RemoteTempIntent {
    config: RootHandle,
    index: PathBuf,
    record: Mutex<NeighborRecord>,
    _lease: File,
}

impl RemoteTempIntent {
    /// Offene Zyklen binden ursprüngliche Namen und Zwischenname. Erfolg schließt
    /// die Intents; nach manueller Prüfung bleibt der konkrete Zwischenname gesperrt.
    pub(crate) fn rename_cycle_pending(&self) -> io::Result<()> {
        {
            let mut record = self
                .record
                .lock()
                .map_err(|_| io::Error::other("Rename-Intent gesperrt"))?;
            if record.version != 3
                || record.endpoint.is_none()
                || record.ftps.is_some()
                || record.named_stream.is_some()
                || record.directory_dacl.is_some()
            {
                return Err(io::Error::other(
                    "Namenstausch benötigt einen gebundenen Remote-Intent",
                ));
            }
            record.candidate = None;
        }
        self.save()
    }
    #[cfg(test)]
    pub(crate) fn create(
        owner: &StagingOwner,
        target: &Path,
        candidate: &Path,
    ) -> io::Result<Self> {
        Self::create_record(owner, target, Some(candidate), None, None, None, None, None)
    }

    pub(crate) fn create_bound_with_root(
        owner: &StagingOwner,
        target: &Path,
        candidate: &Path,
        identity: &crate::RootIdentity,
        account: [u8; 32],
    ) -> io::Result<Self> {
        let endpoint = crate::EndpointBinding::remote(identity, account)?;
        Self::create_record(
            owner,
            target,
            Some(candidate),
            None,
            None,
            Some(endpoint),
            Some(identity.clone()),
            None,
        )
    }

    /// Nach dem nächsten Rename kann der Tempname fehlen, obwohl sein endgültiger
    /// Metadatenabschluss unklar blieb. Der bestehende V3-Intent ohne Kandidaten
    /// bindet dann das Ziel als unbekannte Wirkung; reine Temp-Abwesenheit reicht
    /// für einen neuen Lauf nicht mehr. Erst nach dauerhaftem Save darf Rename starten.
    pub(crate) fn smb_commit_pending(&self) -> io::Result<()> {
        {
            let mut record = self
                .record
                .lock()
                .map_err(|_| io::Error::other("Remote-Intent gesperrt"))?;
            if record.version != 3
                || !matches!(record.endpoint, Some(crate::EndpointBinding::Smb(_)))
            {
                return Err(io::Error::other(
                    "Sparse-Commit benötigt einen gebundenen SMB-Tempintent",
                ));
            }
            record.candidate = None;
        }
        self.save()
    }

    /// Nur nach serverseitig garantiert abgelehntem No-Replace. Der Aufrufer
    /// hält weiterhin genau den eigenen Temp; eine verlorene Antwort reicht nicht.
    pub(crate) fn smb_commit_rejected(&self, candidate: &Path) -> io::Result<()> {
        {
            let mut record = self
                .record
                .lock()
                .map_err(|_| io::Error::other("Remote-Intent gesperrt"))?;
            if record.version != 3
                || !matches!(record.endpoint, Some(crate::EndpointBinding::Smb(_)))
                || record.candidate.is_some()
                || candidate.parent() != record.target.parent()
                || candidate == record.target
            {
                return Err(io::Error::other(
                    "Abgelehnter SMB-Commit besitzt keinen passenden Tempintent",
                ));
            }
            record.candidate = Some(candidate.to_path_buf());
        }
        self.save()
    }

    pub(crate) fn create_named_stream_with_root(
        owner: &StagingOwner,
        binding: NamedStreamIntentBinding,
        identity: &crate::RootIdentity,
        account: [u8; 32],
    ) -> io::Result<Self> {
        binding.validate()?;
        if !matches!(identity, crate::RootIdentity::Smb(_)) {
            return Err(io::Error::other("Verzeichnisstream-Intent benötigt SMB"));
        }
        if let crate::RootIdentity::Smb(root) = identity {
            root.validate()?;
            if !binding.base_directory.starts_with(&root.virtual_root) {
                return Err(io::Error::other(
                    "Verzeichnisstream-Basis verlässt den gewählten Root",
                ));
            }
        }
        let endpoint = crate::EndpointBinding::remote(identity, account)?;
        let target = binding.base_directory.clone();
        Self::create_record(
            owner,
            &target,
            None,
            None,
            None,
            Some(endpoint),
            Some(identity.clone()),
            Some(binding),
        )
    }

    pub(crate) fn named_stream_phase(&self, phase: NamedStreamIntentPhase) -> io::Result<()> {
        {
            let mut record = self
                .record
                .lock()
                .map_err(|_| io::Error::other("Remote-Intent gesperrt"))?;
            let mut binding = record
                .named_stream
                .clone()
                .ok_or_else(|| io::Error::other("Kein Verzeichnisstream-Intent"))?;
            use NamedStreamIntentPhase::*;
            if !matches!(
                (binding.phase, phase),
                (Prepared, Committing | Removing) | (Committing, Committed) | (Removing, Removed)
            ) && binding.phase != phase
            {
                return Err(io::Error::other(
                    "Ungültiger Verzeichnisstream-Phasenwechsel",
                ));
            }
            binding.phase = phase;
            binding.validate()?;
            record.named_stream = Some(binding);
        }
        self.save()
    }

    #[allow(clippy::too_many_arguments)]
    fn create_record(
        owner: &StagingOwner,
        target: &Path,
        candidate: Option<&Path>,
        ftps: Option<FtpsIntentBinding>,
        held_config: Option<&RootHandle>,
        endpoint: Option<crate::EndpointBinding>,
        endpoint_root: Option<crate::RootIdentity>,
        named_stream: Option<NamedStreamIntentBinding>,
    ) -> io::Result<Self> {
        let parent = owner
            .queue
            .parent()
            .ok_or_else(|| io::Error::other("Queue ohne Elternordner"))?;
        let config = match held_config {
            Some(config) if config.path() == parent => config.clone(),
            Some(_) => {
                return Err(io::Error::other(
                    "FTPS-Intent und Queueordner unterscheiden sich",
                ))
            }
            None => RootHandle::open_nofollow(parent)?,
        };
        if !config.is_current()?
            || !config.has_exclusive_namespace()?
            || candidate.is_some_and(|candidate| candidate.parent() != target.parent())
        {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Remote-Tempintent benötigt einen privaten gehaltenen Queueordner und genau einen Zielordner"));
        }
        // Kein neuer Owner darf zwischen Tailprüfung und Startevent in einen
        // Reparatursnapshot eines älteren Teilwrites geraten.
        let _lock = lock_intent_events(&config)?;
        repair_intent_tail(&config)?;
        #[cfg(test)]
        after_initial_intent_registry_check();
        let mut index =
            crate::atomic_file::sibling_candidate(&config.path().join("neighbor"), "recovery")?
                .into_os_string();
        index.push(".recovery.json");
        let index = PathBuf::from(index);
        let record = NeighborRecord {
            version: if named_stream.is_some() {
                4
            } else if endpoint.is_some() {
                3
            } else if ftps.is_some() {
                2
            } else {
                1
            },
            owner: owner.clone(),
            target: target.to_path_buf(),
            candidate: candidate.map(Path::to_path_buf),
            remote: true,
            stage_guard: false,
            ftps,
            endpoint,
            endpoint_root,
            named_stream,
            directory_dacl: None,
        };
        let vfs = crate::LocalVfs::rooted(config.path())?;
        if crate::Vfs::anchored_identity(&vfs) != Some(config.identity()) {
            return Err(io::Error::other("Remote-Intentordner wurde ausgetauscht"));
        }
        let lease_path = intent_lease_path(&index);
        let lease = File::from(openat(
            config.fd(),
            lease_path.file_name().unwrap(),
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        rustix::fs::flock(&lease, rustix::fs::FlockOperation::LockExclusive)?;
        lease.sync_all()?;
        crate::atomic_write::atomic_write_vfs_with_mode(
            &vfs,
            &index,
            &encode_neighbor_record(&record)?,
            ".remote-intent-",
            crate::atomic_write::AtomicWriteMode::Create,
            Some(0o600),
        )?;
        append_intent_event_locked(&config, &record, "start")?;
        Ok(Self {
            config,
            index,
            record: Mutex::new(record),
            _lease: lease,
        })
    }

    pub(crate) fn create_ftps(
        owner: &StagingOwner,
        config: &RootHandle,
        target: &Path,
        binding: FtpsIntentBinding,
    ) -> io::Result<Self> {
        validate_ftps_intent(&binding, target)?;
        Self::create_record(
            owner,
            target,
            None,
            Some(binding),
            Some(config),
            None,
            None,
            None,
        )
    }

    pub(crate) fn ftps_stou_pending(&self) -> io::Result<()> {
        self.update_ftps(|binding| {
            binding.phase = "stou".into();
            binding.stou_name_unknown = true;
        })
    }

    pub(crate) fn ftps_stou_name(&self, path: &Path) -> io::Result<()> {
        self.update_ftps(|binding| {
            binding.stou_name_unknown = false;
            if !binding.paths.contains(&path.to_path_buf()) {
                binding.paths.push(path.to_path_buf());
            }
        })
    }

    fn update_ftps(&self, update: impl FnOnce(&mut FtpsIntentBinding)) -> io::Result<()> {
        {
            let mut record = self
                .record
                .lock()
                .map_err(|_| io::Error::other("Remote-Intent gesperrt"))?;
            let target = record.target.clone();
            let mut binding = record
                .ftps
                .clone()
                .ok_or_else(|| io::Error::other("Kein FTPS-Intent"))?;
            update(&mut binding);
            validate_ftps_intent(&binding, &target)?;
            record.ftps = Some(binding);
        }
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        if !self.config.is_current()? || !self.config.has_exclusive_namespace()? {
            return Err(io::Error::other("Remote-Intentordner wurde ausgetauscht"));
        }
        let vfs = crate::LocalVfs::rooted(self.config.path())?;
        if crate::Vfs::anchored_identity(&vfs) != Some(self.config.identity()) {
            return Err(io::Error::other("Remote-Intentordner wurde ausgetauscht"));
        }
        let record = self
            .record
            .lock()
            .map_err(|_| io::Error::other("Remote-Intent gesperrt"))?;
        let _lock = lock_intent_events(&self.config)?;
        append_intent_event_locked(&self.config, &record, "update")?;
        crate::atomic_write::atomic_write_vfs_with_mode(
            &vfs,
            &self.index,
            &encode_neighbor_record(&record)?,
            ".remote-intent-",
            crate::atomic_write::AtomicWriteMode::Replace,
            Some(0o600),
        )
    }

    pub(crate) fn finish(&self) -> io::Result<()> {
        if !self.config.is_current()? || !self.config.has_exclusive_namespace()? {
            return Err(io::Error::other("Remote-Intentordner wurde ausgetauscht"));
        }
        let record = self
            .record
            .lock()
            .map_err(|_| io::Error::other("Remote-Intent gesperrt"))?;
        let _lock = lock_intent_events(&self.config)?;
        append_intent_event_locked(&self.config, &record, "finish")?;
        crate::local_fs::remove_file_beneath(&self.config, &self.index)?;
        crate::local_fs::remove_file_beneath(&self.config, &intent_lease_path(&self.index))?;
        crate::local_fs::sync_dir_beneath(&self.config, self.config.path())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.index
    }

    pub(crate) fn ftps_recovery(&self) -> (Vec<String>, bool) {
        let mut paths = vec![self.index.to_string_lossy().into_owned()];
        let mut unknown = false;
        if let Ok(record) = self.record.lock() {
            paths.push(record.target.to_string_lossy().into_owned());
            if let Some(binding) = &record.ftps {
                unknown = binding.stou_name_unknown;
                paths.extend(
                    binding
                        .paths
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned()),
                );
                if unknown {
                    if let Some(parent) = record.target.parent() {
                        paths.push(parent.to_string_lossy().into_owned());
                    }
                }
            }
        }
        paths.sort();
        paths.dedup();
        (paths, unknown)
    }
}

/// Eine manuelle Prüfung beendet die Queue-Recovery, beweist aber keine
/// Abwesenheit des Zwischennamens. Der Registrybeleg bleibt deshalb auf genau
/// diesen Pfad gebunden, bis ein späterer rein lesender Check ihn nicht mehr findet.
pub(crate) fn acknowledge_rename_cycle_intents_after_review(
    owner: &StagingOwner,
    config: &RootHandle,
    candidate: &Path,
) -> io::Result<()> {
    if owner.queue.parent() != Some(config.path())
        || !config.is_current()?
        || !config.has_exclusive_namespace()?
    {
        return Err(io::Error::other(
            "Namenstausch-Recoveryordner ist nicht privat gebunden",
        ));
    }
    let _lock = lock_intent_events(config)?;
    repair_intent_tail(config)?;
    let mut verified = Vec::new();
    for entry in crate::local_fs::list_beneath(config, config.path())? {
        if !entry.name.starts_with(".neighbor.doppeldecker-recovery-")
            || !entry.name.ends_with(".recovery.json")
        {
            continue;
        }
        let index = config.path().join(&entry.name);
        let (mut record, _) = read_neighbor(config, &index)?;
        if record.owner != *owner {
            continue;
        }
        if record.version != 3
            || !record.remote
            || record.endpoint.is_none()
            || record.ftps.is_some()
            || record.named_stream.is_some()
            || record.directory_dacl.is_some()
            || candidate.parent() != record.target.parent()
        {
            return Err(io::Error::other(
                "Namenstausch enthält einen anderen Intentvertrag",
            ));
        }
        if writer_active(config, &index, &record)? {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Der Namenstausch läuft noch",
            ));
        }
        record.candidate = Some(candidate.to_path_buf());
        validate_neighbor(&record, config.path())?;
        verified.push((index, record));
    }
    let vfs = crate::LocalVfs::rooted(config.path())?;
    for (index, record) in verified {
        append_intent_event_locked(config, &record, "update")?;
        crate::atomic_write::atomic_write_vfs_with_mode(
            &vfs,
            &index,
            &encode_neighbor_record(&record)?,
            ".remote-intent-",
            crate::atomic_write::AtomicWriteMode::Replace,
            Some(0o600),
        )?;
    }
    Ok(())
}

impl NeighborStaging {
    pub(crate) fn create(
        owner: &StagingOwner,
        target: &Path,
        root: Option<&RootHandle>,
    ) -> io::Result<Option<Self>> {
        let parent_path = target
            .parent()
            .ok_or_else(|| io::Error::other("Tempziel ohne Elternordner"))?;
        let parent = match root {
            Some(root) => RootHandle::open_beneath(root, parent_path)?,
            None => RootHandle::open_nofollow(parent_path)?,
        };
        let config = RootHandle::open_nofollow(
            owner
                .queue
                .parent()
                .ok_or_else(|| io::Error::other("Queue ohne Elternordner"))?,
        )?;
        if !config.has_exclusive_namespace()? {
            return Ok(None);
        }
        // Kein neuer Owner darf zwischen Tailprüfung und Startevent in einen
        // Reparatursnapshot eines älteren Teilwrites geraten.
        let _lock = lock_intent_events(&config)?;
        repair_intent_tail(&config)?;
        #[cfg(test)]
        after_initial_intent_registry_check();
        let candidate =
            crate::atomic_file::sibling_candidate(&config.path().join("neighbor"), "recovery")?;
        let mut name = candidate.into_os_string();
        name.push(".recovery.json");
        let index = PathBuf::from(name);
        let record = NeighborRecord {
            version: 1,
            owner: owner.clone(),
            target: target.to_path_buf(),
            candidate: None,
            remote: false,
            stage_guard: false,
            ftps: None,
            endpoint: None,
            endpoint_root: None,
            named_stream: None,
            directory_dacl: None,
        };
        let lease = File::from(openat(
            config.fd(),
            intent_lease_path(&index).file_name().unwrap(),
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        rustix::fs::flock(&lease, rustix::fs::FlockOperation::LockExclusive)?;
        let value = Self {
            config,
            parent,
            index,
            record: Mutex::new(record),
            _lease: lease,
        };
        {
            let record = value
                .record
                .lock()
                .map_err(|_| io::Error::other("Temp-Recovery gesperrt"))?;
            value.save_locked(crate::atomic_write::AtomicWriteMode::Create, &record)?;
        }
        Ok(Some(value))
    }

    fn save(&self, mode: crate::atomic_write::AtomicWriteMode) -> io::Result<()> {
        let record = self
            .record
            .lock()
            .map_err(|_| io::Error::other("Temp-Recovery gesperrt"))?;
        let _lock = lock_intent_events(&self.config)?;
        repair_intent_tail(&self.config)?;
        self.save_locked(mode, &record)
    }

    fn save_locked(
        &self,
        mode: crate::atomic_write::AtomicWriteMode,
        record: &NeighborRecord,
    ) -> io::Result<()> {
        if !self.config.is_current()? || !self.config.has_exclusive_namespace()? {
            return Err(io::Error::other("Temp-Recoveryordner wurde ausgetauscht"));
        }
        let data = encode_neighbor_record(record)?;
        let vfs = crate::LocalVfs::rooted(self.config.path())?;
        if crate::Vfs::anchored_identity(&vfs) != Some(self.config.identity()) {
            return Err(io::Error::other(
                "Temp-Recoveryordner wurde vor dem Journalschreiben ausgetauscht",
            ));
        }
        crate::atomic_write::atomic_write_vfs_with_mode(
            &vfs,
            &self.index,
            &data,
            ".neighbor-index-",
            mode,
            Some(0o600),
        )?;
        append_intent_event_locked(&self.config, record, "neighbor")
    }

    pub(crate) fn candidate(&self, path: &Path) -> io::Result<()> {
        self.verify_parent()?;
        self.record
            .lock()
            .map_err(|_| io::Error::other("Temp-Recovery gesperrt"))?
            .candidate = Some(path.to_path_buf());
        self.save(crate::atomic_write::AtomicWriteMode::Replace)
    }

    fn stage_candidate(&self, path: &Path) -> io::Result<()> {
        self.verify_parent()?;
        {
            let mut record = self
                .record
                .lock()
                .map_err(|_| io::Error::other("Temp-Recovery gesperrt"))?;
            record.stage_guard = true;
            record.candidate = Some(path.to_path_buf());
        }
        self.save(crate::atomic_write::AtomicWriteMode::Replace)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.index
    }

    fn verify_parent(&self) -> io::Result<()> {
        if !self.parent.is_current()? {
            return Err(crate::real_mode::plan_drift(
                "Ziel-Elternordner des Nachbartemps wurde ausgetauscht",
            ));
        }
        Ok(())
    }

    pub(crate) fn finish(&self) -> io::Result<()> {
        self.verify_parent()?;
        let record = self
            .record
            .lock()
            .map_err(|_| io::Error::other("Temp-Recovery gesperrt"))?;
        if let Some(candidate) = &record.candidate {
            for path in [candidate.to_path_buf(), apple_double(candidate)] {
                match crate::local_fs::stat_beneath(&self.parent, &path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(crate::error::with_recovery_paths(
                            io::Error::other("Tempbestand blieb zur manuellen Prüfung erhalten"),
                            &[path.to_string_lossy().into_owned()],
                        ))
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        if !self.config.is_current()? || !self.config.has_exclusive_namespace()? {
            return Err(io::Error::other("Temp-Recoveryordner wurde ausgetauscht"));
        }
        let _lock = lock_intent_events(&self.config)?;
        append_intent_event_locked(&self.config, &record, "finish")?;
        crate::local_fs::remove_file_beneath(&self.config, &self.index)?;
        crate::local_fs::remove_file_beneath(&self.config, &intent_lease_path(&self.index))?;
        crate::local_fs::sync_dir_beneath(&self.config, self.config.path())
    }
}

fn apple_double(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from("._");
    name.push(path.file_name().unwrap_or_default());
    path.with_file_name(name)
}

pub(crate) struct LocalStaging {
    anchor: RootHandle,
    outer: Option<RootHandle>,
    lease: File,
    registry: Option<(RootHandle, PathBuf)>,
    creation_guard: Option<NeighborStaging>,
    finished: std::sync::atomic::AtomicBool,
    pub(crate) parent: RootHandle,
    pub(crate) directory: RootHandle,
    inheritance: Inheritance,
    journal: Mutex<Journal>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Inheritance {
    uid: u32,
    mode: u32,
    gid: u32,
    acl: Option<Acl>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Journal {
    version: u32,
    process: u32,
    parent: PathBuf,
    parent_identity: crate::vfs::LocalRootIdentity,
    staging_identity: crate::vfs::LocalRootIdentity,
    anchor_identity: crate::vfs::LocalRootIdentity,
    owner: Option<StagingOwner>,
    actions: Vec<Action>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Action {
    phase: String,
    from: PathBuf,
    to: Option<PathBuf>,
    from_identity: Option<(u64, u64)>,
    to_identity: Option<(u64, u64)>,
}

// Vor der Rückgabe eines Scope können Lease- oder Journalanlage scheitern. Der
// private Ordner existiert dann schon; sein Prüfpfad muss auch strukturiert erhalten
// bleiben, ohne einen neuen pfadbasierten Aufräumweg zu eröffnen.
fn prepare_with_recovery<T>(path: &Path, prepare: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    prepare().map_err(|error| {
        crate::error::with_recovery_paths(error, &[path.to_string_lossy().into_owned()])
    })
}

impl LocalStaging {
    pub(crate) fn create(
        root: Option<&RootHandle>,
        parent_root: Option<&RootHandle>,
        target: &Path,
        sandbox: Option<&Path>,
        owner: Option<&StagingOwner>,
        inherited_anchor: Option<&RootHandle>,
    ) -> io::Result<Option<Self>> {
        // Aktive SELinux-Policies brauchen einen eigenen belegten Kontextadapter.
        #[cfg(target_os = "linux")]
        if std::fs::read("/sys/fs/selinux/enforce").is_ok() {
            return Ok(None);
        }
        let parent_path = target
            .parent()
            .ok_or_else(|| io::Error::other("Ziel ohne Elternordner"))?;
        let outer = match root {
            Some(root) => Some(root.clone()),
            None => sandbox.map(RootHandle::open_nofollow).transpose()?,
        };
        let parent = match parent_root.or(outer.as_ref()) {
            Some(root) => RootHandle::open_beneath(root, parent_path)?,
            None => RootHandle::open_nofollow(parent_path)?,
        };
        // Ein Pane-Root kann enger als die Test-Sandbox sein. Sein gehaltener
        // Deskriptor darf dann nicht den darüberliegenden Sandboxroot öffnen;
        // das Staging bleibt stattdessen innerhalb beider Grenzen.
        let bounded_sandbox = sandbox.map(|sandbox| match root {
            Some(root) if root.path().starts_with(sandbox) => root.path(),
            _ => sandbox,
        });
        let anchor_path = bounded_sandbox
            .or_else(|| root.map(RootHandle::path))
            .or_else(|| inherited_anchor.map(RootHandle::path))
            .unwrap_or(parent_path);
        if !parent_path.starts_with(anchor_path) {
            return Ok(None);
        }
        let anchor =
            if let Some(anchor) = inherited_anchor.filter(|anchor| anchor.path() == anchor_path) {
                anchor.clone()
            } else {
                match outer.as_ref() {
                    Some(root) => RootHandle::open_beneath(root, anchor_path)?,
                    None => RootHandle::open_nofollow(anchor_path)?,
                }
            };
        if !anchor.is_private()? {
            return Ok(None);
        }
        let parent_identity = crate::local_fs::root_identity_beneath(&parent, parent.path())?;
        let anchor_identity = crate::local_fs::root_identity_beneath(&anchor, anchor.path())?;
        if parent_identity.mount != anchor_identity.mount {
            return Ok(None);
        }
        let config = owner
            .map(|owner| {
                RootHandle::open_nofollow(
                    owner
                        .queue
                        .parent()
                        .ok_or_else(|| io::Error::other("Queue ohne Konfigurationsordner"))?,
                )
            })
            .transpose()?;
        if let Some(config) = &config {
            if !config.has_exclusive_namespace()? {
                return Ok(None);
            }
        }
        let inheritance = Inheritance::read(parent.fd())?;
        let name = crate::atomic_file::sibling_candidate(&anchor.path().join("stage"), "stage")?;
        // Der sichtbare Stagebaum entsteht erst nach einem dauerhaft bekannten
        // lokalen Intent. Dessen Lease bleibt bis zur wirklichen Entfernung gehalten.
        let creation_guard = owner
            .map(|owner| -> io::Result<NeighborStaging> {
                let guard =
                    NeighborStaging::create(owner, &name, Some(&anchor))?.ok_or_else(|| {
                        io::Error::other("Privates Staging benötigt registrierten Creationguard")
                    })?;
                guard.stage_candidate(&name)?;
                Ok(guard)
            })
            .transpose()?;
        let directory = anchor.create_private_child(&name).map_err(|error| {
            let unclear = crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Private Staging-Anlage nicht bestätigt ({error}); möglicher Recoverypfad: {}",
                    name.display()
                ),
            );
            crate::error::with_recovery_paths(unclear, &[name.to_string_lossy().into_owned()])
        })?;
        let mut stage = prepare_with_recovery(&name, || {
            let lease = File::from(openat(
                directory.fd(),
                "lease",
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?);
            rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
            let journal = Journal {
                version: 1,
                process: std::process::id(),
                parent: parent.path().to_path_buf(),
                parent_identity,
                staging_identity: crate::local_fs::root_identity_beneath(
                    &directory,
                    directory.path(),
                )?,
                anchor_identity,
                owner: owner.cloned(),
                actions: Vec::new(),
            };
            let stage = Self {
                anchor,
                outer,
                lease,
                registry: None,
                creation_guard,
                finished: std::sync::atomic::AtomicBool::new(false),
                parent,
                directory,
                inheritance,
                journal: Mutex::new(journal),
            };
            stage.record("prepared", target, None)?;
            Ok(stage)
        })?;
        if owner.is_some() {
            let registered = (|| -> io::Result<()> {
                let config = config.unwrap();
                if !config.has_exclusive_namespace()? {
                    return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Staging-Recoveryindex verlangt einen gegen fremdes Schreiben geschützten Queueordner",
                ));
                }
                let name = stage
                    .directory
                    .path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy();
                let index = config.path().join(format!("{name}.recovery.json"));
                let next = index.with_extension("next");
                let mut file = File::from(openat(
                    config.fd(),
                    next.file_name().unwrap(),
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(0o600),
                )?);
                let journal = stage
                    .journal
                    .lock()
                    .map_err(|_| io::Error::other("Stagingjournal gesperrt"))?;
                serde_json::to_writer(&mut file, &*journal)?;
                file.sync_all()?;
                crate::local_fs::rename_noreplace_beneath(&config, &next, &index)?;
                stage.registry = Some((config.clone(), index));
                crate::local_fs::sync_dir_beneath(&config, config.path())?;
                drop(journal);
                Ok(())
            })();
            if let Err(error) = registered {
                let cleanup = stage.finish();
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(crate::error::with_recovery_path(
                        error,
                        stage.directory.path(),
                        &cleanup,
                    )),
                };
            }
        }
        Ok(Some(stage))
    }

    pub(crate) fn outer(&self) -> Option<&RootHandle> {
        self.outer.as_ref()
    }

    pub(crate) fn anchor(&self) -> &RootHandle {
        &self.anchor
    }

    pub(crate) fn candidate(&self, target: &Path, role: &str) -> io::Result<PathBuf> {
        let path = crate::atomic_file::sibling_candidate(
            &self.directory.path().join(
                target
                    .file_name()
                    .ok_or_else(|| io::Error::other("Ziel ohne Namen"))?,
            ),
            role,
        )?;
        self.record("create_intent", &path, Some(target))?;
        Ok(path)
    }

    fn entry_identity(&self, path: &Path) -> io::Result<Option<(u64, u64)>> {
        let root = if path.starts_with(self.directory.path()) {
            Some(&self.directory)
        } else if path.starts_with(self.parent.path()) {
            Some(&self.parent)
        } else {
            self.outer.as_ref()
        };
        let stat = match root {
            Some(root) => crate::local_fs::stat_beneath(root, path),
            None => crate::local_fs::stat(path),
        };
        match stat {
            Ok(entry) => Ok(entry.device.zip(entry.inode)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn finish(&self) -> io::Result<()> {
        if self.finished.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        self.verify_parent()?;
        let listing = crate::local_fs::list_beneath(&self.directory, self.directory.path())?;
        if listing.iter().any(|entry| {
            !matches!(
                entry.name.as_str(),
                "journal.json" | "journal.next" | "lease"
            )
        }) {
            return self.record("complete_with_remainders", self.directory.path(), None);
        }
        self.record("complete", self.directory.path(), None)?;
        // Der private, gehaltene Anker schließt fremde Namensänderungen aus. Ist
        // der sichtbare Name trotzdem ausgetauscht, bleibt der Recoverybeleg stehen.
        if !self.anchor.is_private()? || !self.directory.is_private()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Staging-Namensraum verlor seine privaten Rechte",
            ));
        }
        let visible = crate::local_fs::root_identity_beneath(&self.anchor, self.directory.path())?;
        let held = crate::local_fs::root_identity_beneath(&self.directory, self.directory.path())?;
        if visible != held {
            return Err(io::Error::other(
                "Stagingname wurde ausgetauscht; Journal blieb erhalten",
            ));
        }
        for name in ["journal.json", "lease"] {
            crate::local_fs::remove_file_beneath(
                &self.directory,
                &self.directory.path().join(name),
            )?;
        }
        self.lease.sync_all()?;
        crate::local_fs::remove_dir_beneath(&self.anchor, self.directory.path())?;
        crate::local_fs::sync_dir_beneath(&self.anchor, self.anchor.path())?;
        if let Some((config, index)) = &self.registry {
            let _lock = lock_intent_events(config)?;
            crate::local_fs::remove_file_beneath(config, index)?;
            crate::local_fs::sync_dir_beneath(config, config.path())?;
        }
        if let Some(guard) = &self.creation_guard {
            guard.finish()?;
        }
        self.finished
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    pub(crate) fn contains(&self, path: &Path) -> bool {
        path != self.directory.path() && path.starts_with(self.directory.path())
    }

    pub(crate) fn inherit_file(&self, file: &File) -> io::Result<()> {
        self.verify_parent()?;
        let fd = rustix::io::dup(file)?;
        let actual = fstat(&fd)?;
        #[cfg(target_os = "macos")]
        let gid = self.inheritance.gid;
        #[cfg(target_os = "linux")]
        let gid = if self.inheritance.mode & 0o2000 != 0 {
            self.inheritance.gid
        } else {
            actual.st_gid
        };
        fchown(file, None, Some(Gid::from_raw(gid)))?;
        #[cfg(target_os = "macos")]
        let ordinary_mode = u32::from(actual.st_mode & 0o777);
        #[cfg(target_os = "linux")]
        let ordinary_mode = actual.st_mode & 0o777;
        let (acl, mode) = inherited_file_acl(self.inheritance.acl.as_ref(), ordinary_mode);
        crate::acl::write_fd_exact(&fd, acl.as_ref())?;
        #[cfg(target_os = "macos")]
        let mode = mode as u16;
        fchmod(file, Mode::from_raw_mode(mode))?;
        // Ein exakter Readback verhindert still verlorene ACLs auf fremden Mounts.
        if crate::acl::read_fd(&fd)? != acl {
            return Err(io::Error::other(
                "ACL-Vererbung der Tempdatei stimmt im Readback nicht überein",
            ));
        }
        Ok(())
    }

    pub(crate) fn verify_parent(&self) -> io::Result<()> {
        if !self.parent.is_current().unwrap_or(false) {
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::PlanDrift,
                "Ziel-Elternordner wurde umbenannt oder ausgetauscht",
            ));
        }
        if Inheritance::read(self.parent.fd())? != self.inheritance {
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::PlanDrift,
                "Ziel-ACL, Gruppe oder Rechte änderten sich während der Übertragung",
            ));
        }
        Ok(())
    }

    pub(crate) fn record(&self, phase: &str, from: &Path, to: Option<&Path>) -> io::Result<()> {
        let mut journal = self
            .journal
            .lock()
            .map_err(|_| io::Error::other("Stagingjournal gesperrt"))?;
        journal.actions.push(Action {
            phase: phase.into(),
            from: from.into(),
            to: to.map(Path::to_path_buf),
            from_identity: self.entry_identity(from)?,
            to_identity: to
                .map(|path| self.entry_identity(path))
                .transpose()?
                .flatten(),
        });
        // Der feste Schreibname liegt im nachweislich privaten Namensraum. Kein
        // fremder Prozess kann ihn dort zwischen Schreiben und Rename austauschen.
        let fd = openat(
            self.directory.fd(),
            "journal.next",
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let mut file = File::from(fd);
        serde_json::to_writer(&mut file, &*journal)?;
        file.flush()?;
        file.sync_all()?;
        renameat(
            self.directory.fd(),
            "journal.next",
            self.directory.fd(),
            "journal.json",
        )?;
        File::from(rustix::io::dup(self.directory.fd())?).sync_all()
    }
}

impl Inheritance {
    fn read(fd: &OwnedFd) -> io::Result<Self> {
        for _ in 0..3 {
            let before = fstat(fd)?;
            let acl = crate::acl::read_fd(fd)?;
            let after = fstat(fd)?;
            if before.st_ctime == after.st_ctime
                && before.st_ctime_nsec == after.st_ctime_nsec
                && before.st_uid == after.st_uid
                && before.st_gid == after.st_gid
                && before.st_mode == after.st_mode
            {
                #[cfg(target_os = "macos")]
                let mode = u32::from(after.st_mode & 0o7777);
                #[cfg(target_os = "linux")]
                let mode = after.st_mode & 0o7777;
                return Ok(Self {
                    uid: after.st_uid,
                    mode,
                    gid: after.st_gid,
                    acl,
                });
            }
        }
        Err(crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::PlanDrift,
            "Ziel-Vererbung änderte sich während der Vorbereitung",
        ))
    }
}

fn inherited_file_acl(parent: Option<&Acl>, ordinary_mode: u32) -> (Option<Acl>, u32) {
    match parent {
        Some(Acl::Nfs4(parent)) => {
            let entries = parent
                .entries
                .iter()
                .filter(|entry| entry.flags & 32 != 0)
                .map(|entry| {
                    let mut entry = *entry;
                    entry.flags = (entry.flags & !(32 | 64 | 128 | 256)) | 16;
                    entry
                })
                .collect::<Vec<_>>();
            let acl = (!entries.is_empty())
                .then_some(Acl::Nfs4(crate::metadata::Nfs4Acl { entries, flags: 0 }));
            (acl, ordinary_mode)
        }
        Some(Acl::Posix(parent)) if !parent.default.is_empty() => {
            let mut access = parent.default.clone();
            for entry in &mut access {
                let limit = match entry.tag {
                    PosixAclTag::UserObject => 6,
                    PosixAclTag::Other => 6,
                    PosixAclTag::Mask => 6,
                    PosixAclTag::GroupObject
                        if !parent.default.iter().any(|e| e.tag == PosixAclTag::Mask) =>
                    {
                        6
                    }
                    _ => 7,
                };
                entry.permissions &= limit;
            }
            let permissions = |tag| {
                access
                    .iter()
                    .find(|e| e.tag == tag)
                    .map_or(0, |e| u32::from(e.permissions))
            };
            let mode = permissions(PosixAclTag::UserObject) << 6
                | permissions(if access.iter().any(|e| e.tag == PosixAclTag::Mask) {
                    PosixAclTag::Mask
                } else {
                    PosixAclTag::GroupObject
                }) << 3
                | permissions(PosixAclTag::Other);
            let acl = if access.len() == 3 {
                None
            } else {
                Some(Acl::Posix(PosixAcl {
                    access,
                    default: Vec::new(),
                }))
            };
            (acl, mode)
        }
        _ => (None, ordinary_mode),
    }
}

pub(crate) fn parent_inheritance(
    root: Option<&RootHandle>,
    path: &Path,
) -> io::Result<crate::metadata::LocalParentInheritance> {
    let parent = match root {
        Some(root) => RootHandle::open_beneath(root, path)?,
        None => RootHandle::open_nofollow(path)?,
    };
    let snapshot = Inheritance::read(parent.fd())?;
    Ok(crate::metadata::LocalParentInheritance {
        identity: crate::local_fs::root_identity_beneath(&parent, parent.path())?,
        uid: snapshot.uid,
        gid: snapshot.gid,
        mode: snapshot.mode,
        acl: snapshot.acl,
    })
}

#[cfg(test)]
thread_local! {
    static AFTER_INITIAL_INTENT_REGISTRY_CHECK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
}

#[cfg(test)]
fn after_initial_intent_registry_check() {
    AFTER_INITIAL_INTENT_REGISTRY_CHECK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

const INTENT_EVENTS: &str = ".doppeldecker-temp-events.jsonl";
const INTENT_EVENTS_LOCK: &str = ".doppeldecker-temp-events.lock";
const INTENT_EVENT_LIMIT: usize = 64 * 1024 + 128;
// Die JSON-Zahlendarstellung der erhaltenen Rohbytes benötigt höchstens vier
// Bytes je Eingabebyte. Normale Ereignisse behalten ihr bisheriges Limit.
const RECOVERY_EVENT_LIMIT: usize = 4 * INTENT_EVENT_LIMIT + 128;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum IntentEvent {
    Record((String, Box<NeighborRecord>)),
    Recovery { interrupted_tail: Vec<u8> },
}

fn intent_lease_path(index: &Path) -> PathBuf {
    let mut name = index.as_os_str().to_os_string();
    name.push(".lease");
    PathBuf::from(name)
}

fn lock_intent_events(config: &RootHandle) -> io::Result<File> {
    if !config.is_current()? || !config.has_exclusive_namespace()? {
        return Err(io::Error::other(
            "Temp-Registry verlor ihren privaten Namensraum",
        ));
    }
    // Das Lock lebt auf einem stabilen Inode: Eine Tail-Reparatur ersetzt das
    // Journal atomar und darf wartende Fenster nicht auf den alten Inode lassen.
    let lock = File::from(openat(
        config.fd(),
        INTENT_EVENTS_LOCK,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::from_raw_mode(0o600),
    )?);
    if !lock.metadata()?.is_file() {
        return Err(io::Error::other(
            "Temp-Registrylock ist keine reguläre Datei",
        ));
    }
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
    Ok(lock)
}

fn read_intent_line(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    reader
        .take(RECOVERY_EVENT_LIMIT as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if line.len() > RECOVERY_EVENT_LIMIT {
        return Err(io::Error::other("Temp-Ereignis überschreitet Größenlimit"));
    }
    Ok(line)
}

fn decode_intent_event(line: &[u8]) -> io::Result<IntentEvent> {
    let event = serde_json::from_slice::<IntentEvent>(line).map_err(io::Error::other)?;
    match &event {
        IntentEvent::Record((phase, record))
            if line.len() <= INTENT_EVENT_LIMIT
                && matches!(
                    phase.as_str(),
                    "start" | "update" | "neighbor" | "finish" | "interrupted"
                )
                || line.len() <= INTENT_EVENT_LIMIT
                    && record.version == 5
                    && record.directory_dacl.is_some()
                    && matches!(phase.as_str(), "recovery_before" | "recovery_after") => {}
        IntentEvent::Recovery { interrupted_tail }
            if !interrupted_tail.is_empty() && interrupted_tail.len() <= INTENT_EVENT_LIMIT => {}
        _ => return Err(io::Error::other("Ungültiges Temp-Ereignis")),
    }
    Ok(event)
}

fn repair_intent_tail(config: &RootHandle) -> io::Result<()> {
    let mut file =
        match crate::local_fs::open_read_beneath(config, &config.path().join(INTENT_EVENTS)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(
            "Temp-Ereignisjournal ist keine reguläre Datei",
        ));
    }
    if file.metadata()?.len() == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    if last == [b'\n'] {
        // Im Normalfall nur die letzte begrenzte Zeile prüfen. Die vollständige
        // Historie bleibt Aufgabe des Namespace-Readers; ein erneuter Gesamtscan
        // je Dateiereignis würde große Transfers quadratisch verteuern.
        let start = file
            .metadata()?
            .len()
            .saturating_sub(RECOVERY_EVENT_LIMIT as u64 + 1);
        file.seek(SeekFrom::Start(start))?;
        let mut end = Vec::new();
        file.take(RECOVERY_EVENT_LIMIT as u64 + 1)
            .read_to_end(&mut end)?;
        let preceding = end[..end.len() - 1].iter().rposition(|byte| *byte == b'\n');
        let line = match preceding {
            Some(position) => &end[position + 1..],
            None if start == 0 => &end,
            None => return Err(io::Error::other("Temp-Ereignis überschreitet Größenlimit")),
        };
        decode_intent_event(line)?;
        return Ok(());
    }
    file.rewind()?;
    let mut reader = std::io::BufReader::new(file);
    let mut prefix_len = 0u64;
    let tail = loop {
        let line = read_intent_line(&mut reader)?;
        if !line.ends_with(b"\n") {
            break line;
        }
        // Eine vollständige kaputte Zeile ist keine abgebrochene letzte Wirkung.
        decode_intent_event(&line)?;
        prefix_len += line.len() as u64;
    };
    if tail.is_empty() || tail.len() > INTENT_EVENT_LIMIT || tail.first() != Some(&b'[') {
        return Err(io::Error::other(
            "Temp-Ereignisjournal besitzt keinen reparierbaren Tail",
        ));
    }
    if ![
        b"[\"start\",".as_slice(),
        b"[\"update\",",
        b"[\"neighbor\",",
        b"[\"finish\",",
        b"[\"recovery_before\",",
        b"[\"recovery_after\",",
    ]
    .iter()
    .any(|prefix| tail.starts_with(prefix) || prefix.starts_with(&tail))
    {
        return Err(io::Error::other(
            "Temp-Ereignisjournal besitzt einen fremden Tail",
        ));
    }
    let complete_owner = match serde_json::from_slice::<(String, NeighborRecord)>(&tail) {
        Ok((phase, record))
            if matches!(phase.as_str(), "start" | "update" | "neighbor" | "finish")
                || (record.version == 5
                    && record.directory_dacl.is_some()
                    && matches!(phase.as_str(), "recovery_before" | "recovery_after")) =>
        {
            Some(record.owner)
        }
        Err(error) if error.is_eof() => None,
        _ => {
            return Err(io::Error::other(
                "Temp-Ereignisjournal besitzt einen korrupten Tail",
            ))
        }
    };
    let mut source = reader.into_inner();
    source.rewind()?;
    let temp = crate::atomic_file::sibling_candidate(&config.path().join("temp-events"), "repair")?;
    let mut repaired = File::from(openat(
        config.fd(),
        temp.file_name().unwrap(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    let repair = (|| {
        // Die Historie bleibt unbegrenzt, der Arbeitspuffer dagegen nicht:
        // validierten Präfix streamen und jeden Index einzeln übernehmen.
        if io::copy(&mut source.take(prefix_len), &mut repaired)? != prefix_len {
            return Err(io::Error::other(
                "Temp-Ereignisjournal änderte seinen gültigen Präfix",
            ));
        }
        let mut found = false;
        let mut owner_found = complete_owner.is_none();
        for entry in crate::local_fs::list_beneath(config, config.path())? {
            if entry.name.starts_with(".neighbor.doppeldecker-recovery-")
                && entry.name.ends_with(".recovery.json")
            {
                let (record, _) = read_neighbor(config, &config.path().join(entry.name))?;
                found = true;
                owner_found |= complete_owner
                    .as_ref()
                    .is_some_and(|owner| owner == &record.owner);
                // Remote-Updates können im Index noch die vorige Phase besitzen.
                // Alle möglichen Owner bleiben daher unbekannt; ihre Rohbytes
                // werden weder erraten noch als sauberer Abschluss ausgegeben.
                serde_json::to_writer(&mut repaired, &("interrupted", record))
                    .map_err(io::Error::other)?;
                repaired.write_all(b"\n")?;
            }
        }
        if !found || !owner_found {
            return Err(io::Error::other(
                "Abgebrochener Temp-Intent besitzt keinen erhaltenen Index",
            ));
        }
        serde_json::to_writer(
            &mut repaired,
            &IntentEvent::Recovery {
                interrupted_tail: tail,
            },
        )
        .map_err(io::Error::other)?;
        repaired.write_all(b"\n")?;
        repaired.sync_all()?;
        if !config.is_current()? || !config.has_exclusive_namespace()? {
            return Err(io::Error::other(
                "Temp-Registry wurde vor der Tail-Reparatur ausgetauscht",
            ));
        }
        renameat(
            config.fd(),
            temp.file_name().unwrap(),
            config.fd(),
            INTENT_EVENTS,
        )?;
        crate::local_fs::sync_dir_beneath(config, config.path()).map_err(|error| {
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!("Das reparierte Temp-Ereignisjournal wurde bereits sichtbar ersetzt, aber sein Elternordner konnte nicht dauerhaft synchronisiert werden ({error})"),
            )
        })
    })();
    repair.map_err(|error| {
        // Vor dem Commit bleibt der ursprüngliche Tail erhalten. Ein Fehler
        // löscht den privaten Reparaturtemp nicht ohne gebundenen Entfernbarkeitsbeleg.
        crate::error::with_recovery_paths(
            error,
            &[
                temp.to_string_lossy().into_owned(),
                config
                    .path()
                    .join(INTENT_EVENTS)
                    .to_string_lossy()
                    .into_owned(),
            ],
        )
    })
}

fn append_intent_event_locked(
    config: &RootHandle,
    record: &NeighborRecord,
    phase: &str,
) -> io::Result<()> {
    repair_intent_tail(config)?;
    let mut file = File::from(openat(
        config.fd(),
        INTENT_EVENTS,
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::APPEND
            | OFlags::NOFOLLOW
            | OFlags::CLOEXEC
            | OFlags::NONBLOCK,
        Mode::from_raw_mode(0o600),
    )?);
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(
            "Temp-Ereignisjournal ist keine reguläre Datei",
        ));
    }
    let mut data = serde_json::to_vec(&(phase, record)).map_err(io::Error::other)?;
    if data.len() > 64 * 1024 {
        return Err(io::Error::other("Temp-Ereignis überschreitet Größenlimit"));
    }
    data.push(b'\n');
    file.write_all(&data)?;
    file.sync_all()?;
    crate::local_fs::sync_dir_beneath(config, config.path())
}
fn same_namespace(left: &crate::EndpointBinding, right: &crate::EndpointBinding) -> bool {
    use crate::EndpointBinding::*;
    match (left, right) {
        (Sftp(a), Sftp(b)) | (Ftps(a), Ftps(b)) | (WebDav(a), WebDav(b)) | (Smb(a), Smb(b)) => {
            a.schema_version == b.schema_version
                && a.account_digest == b.account_digest
                && a.public_identity_digest == b.public_identity_digest
        }
        (Local(_), Local(_)) => true,
        _ => false,
    }
}
pub(crate) fn remote_account(binding: &crate::EndpointBinding) -> Option<[u8; 32]> {
    match binding {
        crate::EndpointBinding::Sftp(value)
        | crate::EndpointBinding::Ftps(value)
        | crate::EndpointBinding::WebDav(value)
        | crate::EndpointBinding::Smb(value) => Some(value.account_digest),
        _ => None,
    }
}
pub(crate) fn translate_namespace_path(
    identity: &crate::RootIdentity,
    path: &Path,
    target: &crate::RootIdentity,
    root: &Path,
) -> Option<PathBuf> {
    // Zwischenstandsrelevanz ist keine Mutationsfreigabe: neue Hostkeys,
    // Zertifikate oder ausgehandelte Fähigkeiten beweisen keinen anderen
    // Dateinamensraum am selben Endpunkt/Konto. Die strenge Laufidentität bleibt
    // in root_namespace_relation und den Providerpermits unverändert.
    let (same, canonical, target_canonical) = match (identity, target) {
        (crate::RootIdentity::Sftp(a), crate::RootIdentity::Sftp(b)) => (
            a.server.host == b.server.host && a.server.port == b.server.port,
            &a.server.canonical_root,
            &b.server.canonical_root,
        ),
        (crate::RootIdentity::Ftps(a), crate::RootIdentity::Ftps(b)) => (
            a.server.host == b.server.host && a.server.port == b.server.port,
            &a.server.canonical_root,
            &b.server.canonical_root,
        ),
        (crate::RootIdentity::WebDav(a), crate::RootIdentity::WebDav(b)) => (
            a.server.origin == b.server.origin
                && a.server.account_digest == b.server.account_digest,
            &a.server.canonical_root,
            &b.server.canonical_root,
        ),
        (crate::RootIdentity::Smb(a), crate::RootIdentity::Smb(b)) => (
            a.server.host == b.server.host
                && a.server.port == b.server.port
                && a.server.share == b.server.share
                && a.server.account_digest == b.server.account_digest,
            &a.server.canonical_root,
            &b.server.canonical_root,
        ),
        _ => return None,
    };
    if !same {
        return None;
    }
    let virtual_root = match target {
        crate::RootIdentity::Sftp(value) => &value.virtual_root,
        crate::RootIdentity::Ftps(value) => &value.virtual_root,
        crate::RootIdentity::WebDav(value) => &value.virtual_root,
        crate::RootIdentity::Smb(value) => &value.virtual_root,
        _ => return None,
    };
    let physical = Path::new(canonical).join(path.strip_prefix("/").ok()?);
    let target_root =
        Path::new(target_canonical).join(Path::new(virtual_root).strip_prefix("/").ok()?);
    physical
        .strip_prefix(target_root)
        .ok()
        .map(|relative| root.join(relative))
}
fn record_paths(
    record: &NeighborRecord,
    binding: &crate::EndpointBinding,
    target_identity: Option<&crate::RootIdentity>,
    root: &Path,
) -> io::Result<Vec<(StagingObject, bool)>> {
    let mut paths = Vec::new();
    if let Some(identity) = &record.endpoint_root {
        let endpoint = record
            .endpoint
            .as_ref()
            .ok_or_else(|| io::Error::other("Temp-Namespace besitzt keine Kontobindung"))?;
        let account = remote_account(endpoint)
            .ok_or_else(|| io::Error::other("Remote-Temp besitzt keine Remote-Kontobindung"))?;
        if crate::EndpointBinding::remote(identity, account)? != *endpoint {
            return Err(io::Error::other(
                "Temp-Root und Kontobindung unterscheiden sich",
            ));
        }
    }
    if let Some(dacl) = &record.directory_dacl {
        dacl.validate()?;
        let endpoint = record
            .endpoint
            .as_ref()
            .ok_or_else(|| io::Error::other("DACL-Intent ohne Endpunkt"))?;
        if remote_account(endpoint) != remote_account(binding) {
            return Ok(paths);
        }
        let base = match (&record.endpoint_root, target_identity) {
            (Some(identity), Some(target)) => {
                translate_namespace_path(identity, &dacl.directory, target, root)
            }
            (_, None) if same_namespace(endpoint, binding) && dacl.directory.starts_with(root) => {
                Some(dacl.directory.clone())
            }
            _ => None,
        };
        if let Some(base) = base {
            paths.push((StagingObject::DirectoryDacl(base), false));
        }
        return Ok(paths);
    }
    if let Some(stream) = &record.named_stream {
        stream.validate()?;
        let endpoint = record
            .endpoint
            .as_ref()
            .ok_or_else(|| io::Error::other("Streamintent ohne Endpunkt"))?;
        if remote_account(endpoint) != remote_account(binding) {
            return Ok(paths);
        }
        let base = match (&record.endpoint_root, target_identity) {
            (Some(identity), Some(target)) => {
                translate_namespace_path(identity, &stream.base_directory, target, root)
            }
            (_, None)
                if same_namespace(endpoint, binding) && stream.base_directory.starts_with(root) =>
            {
                Some(stream.base_directory.clone())
            }
            _ => None,
        };
        if let Some(base) = base {
            paths.extend(
                stream
                    .objects()
                    .into_iter()
                    .map(|object| (object.with_base_path(base.clone()), false)),
            );
        }
        return Ok(paths);
    }
    if let Some(ftps) = &record.ftps {
        validate_ftps_intent(ftps, &record.target)?;
        for namespace in &ftps.roots {
            let identity = crate::EndpointBinding::remote(
                &crate::RootIdentity::Ftps(namespace.clone()),
                ftps.account_digest,
            )?;
            if remote_account(&identity) != remote_account(binding) {
                continue;
            }
            if target_identity.is_none() && !same_namespace(&identity, binding) {
                continue;
            }
            for path in &ftps.paths {
                let virtual_path = Path::new("/").join(
                    path.strip_prefix(&namespace.server.canonical_root)
                        .map_err(|_| io::Error::other("FTPS-Intentpfad verlässt Namespace"))?,
                );
                let translated = target_identity.and_then(|target| {
                    translate_namespace_path(
                        &crate::RootIdentity::Ftps(namespace.clone()),
                        &virtual_path,
                        target,
                        root,
                    )
                });
                if let Some(path) = translated {
                    paths.push((StagingObject::Path(path), ftps.stou_name_unknown));
                } else if target_identity.is_none()
                    && (virtual_path.starts_with(root)
                        || (ftps.stou_name_unknown
                            && root.starts_with(virtual_path.parent().unwrap_or(&virtual_path))))
                {
                    paths.push((StagingObject::Path(virtual_path), ftps.stou_name_unknown));
                }
            }
        }
    } else {
        let local = matches!(binding, crate::EndpointBinding::Local(_));
        if record.remote == local {
            return Ok(paths);
        }
        let candidate = record.candidate.as_ref().unwrap_or(&record.target);
        if let (Some(endpoint), Some(identity), Some(target)) =
            (&record.endpoint, &record.endpoint_root, target_identity)
        {
            if remote_account(endpoint) != remote_account(binding) {
                return Ok(paths);
            }
            if let Some(path) = translate_namespace_path(identity, candidate, target, root) {
                paths.push((StagingObject::Path(path), record.candidate.is_none()));
            } else {
                // Ein Sync kann direkt unter einem erhaltenen Zwischenordner
                // beginnen. Dessen Elternpfad liegt dann außerhalb des Scans und
                // darf deshalb nicht als unbetroffener Nachbar verschwinden.
                let original_root = match identity {
                    crate::RootIdentity::Sftp(value) => &value.virtual_root,
                    crate::RootIdentity::Ftps(value) => &value.virtual_root,
                    crate::RootIdentity::WebDav(value) => &value.virtual_root,
                    crate::RootIdentity::Smb(value) => &value.virtual_root,
                    _ => return Err(io::Error::other("Remote-Intent ohne Remote-Namensraum")),
                };
                if translate_namespace_path(target, root, identity, Path::new(original_root))
                    .is_some_and(|translated_root| translated_root.starts_with(candidate))
                {
                    paths.push((StagingObject::Path(root.to_path_buf()), true));
                }
            }
            return Ok(paths);
        }
        if let Some(endpoint) = &record.endpoint {
            if !same_namespace(endpoint, binding)
                && remote_account(endpoint) != remote_account(binding)
            {
                return Ok(paths);
            }
        }
        if candidate.starts_with(root) {
            paths.push((
                StagingObject::Path(candidate.clone()),
                record.candidate.is_none(),
            ));
            if !record.remote && record.candidate.is_some() {
                paths.push((StagingObject::Path(apple_double(candidate)), false));
            }
        }
    }
    Ok(paths)
}
fn interrupted_record_affects(
    record: &NeighborRecord,
    binding: &crate::EndpointBinding,
    target: Option<&crate::RootIdentity>,
    root: &Path,
) -> bool {
    if record.named_stream.is_some() || record.directory_dacl.is_some() {
        return record_paths(record, binding, target, root)
            .map_or(true, |objects| !objects.is_empty());
    }
    let scope = |identity: &crate::RootIdentity| -> Option<PathBuf> {
        match identity {
            crate::RootIdentity::Sftp(value) => Some(value.virtual_root.clone().into()),
            crate::RootIdentity::Ftps(value) => Some(value.virtual_root.clone().into()),
            crate::RootIdentity::WebDav(value) => Some(value.virtual_root.clone().into()),
            crate::RootIdentity::Smb(value) => Some(value.virtual_root.clone().into()),
            _ => None,
        }
    };
    let overlaps = |identity: &crate::RootIdentity| {
        let (Some(target), Some(source_scope)) = (target, scope(identity)) else {
            return false;
        };
        let Some(target_scope) = scope(target) else {
            return false;
        };
        translate_namespace_path(identity, &source_scope, target, root).is_some()
            || translate_namespace_path(target, &target_scope, identity, &source_scope).is_some()
    };
    if let Some(ftps) = &record.ftps {
        return remote_account(binding) == Some(ftps.account_digest)
            && (target.is_none()
                || ftps
                    .roots
                    .iter()
                    .any(|identity| overlaps(&crate::RootIdentity::Ftps(identity.clone()))));
    }
    if let (Some(endpoint), Some(identity)) = (&record.endpoint, &record.endpoint_root) {
        return remote_account(endpoint) == remote_account(binding)
            && (target.is_none() || overlaps(identity));
    }
    if record.remote == matches!(binding, crate::EndpointBinding::Local(_)) {
        return false;
    }
    let parent = record.target.parent().unwrap_or(&record.target);
    parent.starts_with(root) || root.starts_with(parent)
}

fn read_neighbor(config: &RootHandle, path: &Path) -> io::Result<(NeighborRecord, Vec<u8>)> {
    let mut data = Vec::new();
    crate::local_fs::open_read_beneath(config, path)?
        .take(64 * 1024 + 1)
        .read_to_end(&mut data)?;
    if data.len() > 64 * 1024 {
        return Err(io::Error::other(
            "Temp-Recoveryindex überschreitet Größenlimit",
        ));
    }
    let record: NeighborRecord = serde_json::from_slice(&data).map_err(io::Error::other)?;
    validate_neighbor(&record, config.path())?;
    Ok((record, data))
}
fn validate_neighbor(record: &NeighborRecord, config_path: &Path) -> io::Result<()> {
    if !matches!(record.version, 1..=5)
        || record.owner.queue.parent() != Some(config_path)
        || record.owner.operation.is_empty()
        || !record.target.is_absolute()
        || record
            .candidate
            .as_ref()
            .is_some_and(|path| !path.is_absolute() || path.parent() != record.target.parent())
        || (record.version != 5 && record.directory_dacl.is_some())
        || (record.version == 5
            && (!record.remote
                || record.stage_guard
                || record.ftps.is_some()
                || record.candidate.is_some()
                || record.named_stream.is_some()
                || record.directory_dacl.is_none()
                || !matches!(record.endpoint, Some(crate::EndpointBinding::Smb(_)))
                || !matches!(record.endpoint_root, Some(crate::RootIdentity::Smb(_)))))
        || (record.version != 4 && record.named_stream.is_some())
        || (record.version == 4
            && (!record.remote
                || record.stage_guard
                || record.ftps.is_some()
                || record.candidate.is_some()
                || record.named_stream.is_none()
                || !matches!(record.endpoint, Some(crate::EndpointBinding::Smb(_)))
                || !matches!(record.endpoint_root, Some(crate::RootIdentity::Smb(_)))))
        || (record.version == 1 && (record.ftps.is_some() || record.endpoint.is_some()))
        || (record.version == 2
            && (!record.remote
                || record.ftps.is_none()
                || record.endpoint.is_some()
                || record.candidate.is_some()))
        || (record.version == 3
            && (!record.remote || record.endpoint.is_none() || record.ftps.is_some()))
    {
        return Err(io::Error::other(
            "Ungültiger gebundener Temp-Recoveryvertrag",
        ));
    }
    if let Some(ftps) = &record.ftps {
        validate_ftps_intent(ftps, &record.target)?;
    }
    if let Some(binding) = &record.directory_dacl {
        binding.validate()?;
        let (Some(endpoint), Some(crate::RootIdentity::Smb(root))) =
            (&record.endpoint, &record.endpoint_root)
        else {
            return Err(io::Error::other("Verzeichnis-DACL ohne SMB-Namespace"));
        };
        root.validate()?;
        if crate::EndpointBinding::remote(
            &crate::RootIdentity::Smb(root.clone()),
            root.server.account_digest,
        )? != *endpoint
            || binding.namespace != root.server.server_guid
            || !binding.directory.starts_with(&root.virtual_root)
            || binding.directory == Path::new(&root.virtual_root)
            || binding.directory != record.target
        {
            return Err(io::Error::other(
                "Verzeichnis-DACL verlässt ihre gebundene Basis",
            ));
        }
    }
    if let Some(binding) = &record.named_stream {
        binding.validate()?;
        let (Some(endpoint), Some(crate::RootIdentity::Smb(root))) =
            (&record.endpoint, &record.endpoint_root)
        else {
            return Err(io::Error::other(
                "Verzeichnisstream-Intent ohne SMB-Namespace",
            ));
        };
        root.validate()?;
        if crate::EndpointBinding::remote(
            &crate::RootIdentity::Smb(root.clone()),
            root.server.account_digest,
        )? != *endpoint
            || !binding.base_directory.starts_with(&root.virtual_root)
            || binding.base_directory != record.target
        {
            return Err(io::Error::other(
                "Verzeichnisstream-Basis und Intentziel unterscheiden sich",
            ));
        }
    }
    Ok(())
}
struct NamedStreamJournalState {
    binding: NamedStreamIntentBinding,
    report: crate::directory_streams::DirectoryStreamReport,
}
fn named_stream_key(record: &NeighborRecord) -> io::Result<Vec<u8>> {
    let binding = record
        .named_stream
        .as_ref()
        .ok_or_else(|| io::Error::other("Kein Streamintent"))?;
    serde_json::to_vec(&(
        &record.owner,
        &record.endpoint,
        &record.endpoint_root,
        &binding.base_directory,
        &binding.temporary_name,
        &binding.final_name,
        &binding.effect,
    ))
    .map_err(io::Error::other)
}
fn named_stream_journal(
    config: &RootHandle,
) -> io::Result<std::collections::BTreeMap<Vec<u8>, NamedStreamJournalState>> {
    let mut states = std::collections::BTreeMap::<Vec<u8>, NamedStreamJournalState>::new();
    let file = match crate::local_fs::open_read_beneath(config, &config.path().join(INTENT_EVENTS))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(states),
        Err(error) => return Err(error),
    };
    let mut reader = std::io::BufReader::new(file);
    loop {
        let line = read_intent_line(&mut reader)?;
        if line.is_empty() {
            break;
        }
        if !line.ends_with(b"\n") {
            return Err(io::Error::other("Temp-Ereignisjournal ist unvollständig"));
        }
        let IntentEvent::Record((phase, record)) = decode_intent_event(&line)? else {
            continue;
        };
        let Some(mut binding) = record.named_stream.clone() else {
            continue;
        };
        validate_neighbor(&record, config.path())?;
        let key = named_stream_key(&record)?;
        if phase == "interrupted" {
            // Der erhaltene Index kann vor dem abgebrochenen Phasenwrite liegen.
            // Beide bekannten Namen bleiben prüfbar; kein Besitz wird unterstellt.
            binding.phase = match binding.phase {
                NamedStreamIntentPhase::Prepared if binding.temporary_name.is_some() => {
                    NamedStreamIntentPhase::Committing
                }
                NamedStreamIntentPhase::Prepared => NamedStreamIntentPhase::Removing,
                phase => phase,
            };
        }
        let state = states
            .entry(key)
            .or_insert_with(|| NamedStreamJournalState {
                binding: binding.clone(),
                report: Default::default(),
            });
        state.report.merge(&binding.report());
        state.binding = binding;
    }
    Ok(states)
}
fn apply_named_stream_journal(
    record: &mut NeighborRecord,
    states: &std::collections::BTreeMap<Vec<u8>, NamedStreamJournalState>,
) -> io::Result<()> {
    if record.named_stream.is_some() {
        if let Some(state) = states.get(&named_stream_key(record)?) {
            record.named_stream = Some(state.binding.clone());
        }
    }
    Ok(())
}
fn record_stamp(record: &NeighborRecord, data: &[u8]) -> io::Result<[u8; 32]> {
    if let Some(binding) = &record.directory_dacl {
        let mut hash = blake3::Hasher::new();
        hash.update(data);
        hash.update(&serde_json::to_vec(binding).map_err(io::Error::other)?);
        Ok(*hash.finalize().as_bytes())
    } else if let Some(binding) = &record.named_stream {
        let mut hash = blake3::Hasher::new();
        hash.update(data);
        hash.update(&serde_json::to_vec(binding).map_err(io::Error::other)?);
        Ok(*hash.finalize().as_bytes())
    } else {
        Ok(*blake3::hash(data).as_bytes())
    }
}
fn writer_active(config: &RootHandle, index: &Path, record: &NeighborRecord) -> io::Result<bool> {
    match crate::local_fs::open_read_beneath(config, &intent_lease_path(index)) {
        Ok(lease) => {
            match rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => Ok(false),
                Err(error) if error == rustix::io::Errno::WOULDBLOCK => Ok(true),
                Err(error) => Err(error.into()),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Nur Legacy-Indizes dürfen ohne Lease existieren. Keine fremde
            // Fensterqueue öffnen: deren gespeicherten Zustand ausschließlich lesen.
            if record.version >= 3 {
                return Ok(true);
            }
            let mut data = Vec::new();
            crate::local_fs::open_read_beneath(config, &record.owner.queue)?
                .take(32 * 1024 * 1024 + 1)
                .read_to_end(&mut data)?;
            if data.len() > 32 * 1024 * 1024 {
                return Err(io::Error::other(
                    "Queue-Recoveryprüfung überschreitet Größenlimit",
                ));
            }
            let queue: serde_json::Value =
                serde_json::from_slice(&data).map_err(io::Error::other)?;
            let state = queue
                .get("entries")
                .and_then(|entries| entries.as_array())
                .and_then(|entries| {
                    entries.iter().find(|entry| {
                        entry.get("operation_id").and_then(|id| id.as_str())
                            == Some(record.owner.operation.as_str())
                    })
                })
                .and_then(|entry| entry.get("state"))
                .and_then(|state| state.as_str());
            Ok(!matches!(
                state,
                Some(
                    "Interrupted"
                        | "Failed"
                        | "Completed"
                        | "Cancelled"
                        | "Pending"
                        | "AwaitingConfirmation"
                )
            ))
        }
        Err(error) => Err(error),
    }
}
fn stage_registry_state(
    config: &RootHandle,
    index_path: &Path,
    index: &Journal,
    twin_active: Option<bool>,
) -> io::Result<(bool, bool)> {
    let path = &index.staging_identity.display_path;
    let inspect = || -> io::Result<bool> {
        let anchor = RootHandle::open_nofollow(&index.anchor_identity.display_path)?;
        if !anchor.is_private()?
            || crate::local_fs::root_identity_beneath(&anchor, anchor.path())?
                != index.anchor_identity
            || path.parent() != Some(anchor.path())
        {
            return Err(io::Error::other("Stage-Ankeridentität änderte sich"));
        }
        let stage = RootHandle::open_beneath(&anchor, path)?;
        if !stage.is_private()?
            || crate::local_fs::root_identity_beneath(&stage, path)? != index.staging_identity
        {
            return Err(io::Error::other("Stage-Objektidentität änderte sich"));
        }
        let lease = crate::local_fs::open_read_beneath(&stage, &path.join("lease"))?;
        match rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(false),
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => Ok(true),
            Err(error) => Err(error.into()),
        }
    };
    match inspect() {
        Ok(active) => Ok((active || twin_active.unwrap_or(false), false)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let exists = crate::local_fs::stat(path);
            if !matches!(exists, Err(ref error) if error.kind() == io::ErrorKind::NotFound) {
                return Ok((twin_active.unwrap_or(false), true));
            }
            let active = match twin_active {
                Some(active) => active,
                None => {
                    let owner = index
                        .owner
                        .as_ref()
                        .ok_or_else(|| io::Error::other("Stage-Owner fehlt"))?;
                    let hint = NeighborRecord {
                        version: 1,
                        owner: owner.clone(),
                        target: path.clone(),
                        candidate: Some(path.clone()),
                        remote: false,
                        stage_guard: false,
                        ftps: None,
                        endpoint: None,
                        endpoint_root: None,
                        named_stream: None,
                        directory_dacl: None,
                    };
                    writer_active(config, index_path, &hint)?
                }
            };
            Ok((active, false))
        }
        Err(_) => Ok((twin_active.unwrap_or(false), true)),
    }
}

pub(crate) struct NamespaceStagingCandidate {
    pub(crate) side: usize,
    pub(crate) object: StagingObject,
    pub(crate) active: bool,
    pub(crate) requires_absence: bool,
    pub(crate) unknown: bool,
}
pub(crate) struct NamespaceStagingView {
    pub(crate) candidates: Vec<NamespaceStagingCandidate>,
    pub(crate) epoch: [u8; 32],
}
/// Alle Fenster desselben privaten Stagingbereichs lesen. Die Endpunktbindung
/// bestimmt die Relevanz; gleiche virtuelle Pfade sind kein Kontobeweis.
#[cfg(test)]
pub(crate) fn namespace_staging_view(
    config_path: &Path,
    endpoints: &[(&crate::EndpointBinding, Option<&crate::RootIdentity>, &Path)],
) -> io::Result<NamespaceStagingView> {
    namespace_staging_view_excluding(config_path, endpoints, None)
}
pub(crate) fn namespace_staging_view_excluding(
    config_path: &Path,
    endpoints: &[(&crate::EndpointBinding, Option<&crate::RootIdentity>, &Path)],
    ignore_target_owner: Option<&StagingOwner>,
) -> io::Result<NamespaceStagingView> {
    namespace_staging_view_excluding_scope(config_path, endpoints, ignore_target_owner, false)
}

pub(crate) fn namespace_staging_view_excluding_both(
    config_path: &Path,
    endpoints: &[(&crate::EndpointBinding, Option<&crate::RootIdentity>, &Path)],
    owner: Option<&StagingOwner>,
) -> io::Result<NamespaceStagingView> {
    namespace_staging_view_excluding_scope(config_path, endpoints, owner, true)
}
pub(crate) fn namespace_staging_view_excluding_scope(
    config_path: &Path,
    endpoints: &[(&crate::EndpointBinding, Option<&crate::RootIdentity>, &Path)],
    ignore_target_owner: Option<&StagingOwner>,
    exclude_owner_on_both_sides: bool,
) -> io::Result<NamespaceStagingView> {
    let config = RootHandle::open_nofollow(config_path)?;
    if !config.is_current()? || !config.has_exclusive_namespace()? {
        return Err(io::Error::other("Temp-Registry ist nicht privat gebunden"));
    }
    let _lock = lock_intent_events(&config)?;
    repair_intent_tail(&config)?;
    let named_stream_states = named_stream_journal(&config)?;
    let directory_dacl_states = directory_dacl_journal(&config)?;
    let exclude_record = |record: &NeighborRecord, side: usize| -> io::Result<bool> {
        if !(side == 1 || exclude_owner_on_both_sides)
            || !ignore_target_owner.is_some_and(|owner| *owner == record.owner)
        {
            return Ok(false);
        }
        // Eine eigene bekannte Einzelwirkung darf den Fremdepoch nicht ändern.
        // Die Ausnahme beginnt erst beim dauerhaften Abschluss; jeder offene
        // Prepared/Applying/Applied-Intent bleibt auch für denselben Owner sichtbar.
        Ok(record.directory_dacl.is_none()
            || directory_dacl_states
                .get(&directory_dacl_key(record)?)
                .is_some_and(|state| state.closed))
    };
    let mut candidates = Vec::new();
    let mut records = Vec::new();
    let mut neighbor_activity = std::collections::BTreeMap::new();
    let mut seen_directory_dacl = std::collections::BTreeSet::new();
    let entries = crate::local_fs::list_beneath(&config, config.path())?;
    for entry in &entries {
        if !entry.name.starts_with(".neighbor.doppeldecker-recovery-")
            || !entry.name.ends_with(".recovery.json")
        {
            continue;
        }
        if !entry.is_file || entry.is_symlink {
            return Err(io::Error::other(
                "Temp-Registryindex ist keine reguläre Datei",
            ));
        }
        let index = config.path().join(&entry.name);
        let (mut record, data) = read_neighbor(&config, &index)?;
        apply_named_stream_journal(&mut record, &named_stream_states)?;
        apply_directory_dacl_journal(&mut record, &directory_dacl_states)?;
        if record.directory_dacl.is_some() {
            let key = directory_dacl_key(&record)?;
            seen_directory_dacl.insert(key.clone());
            // Nach dauerhaftem Abschluss ist ein nicht entfernter lokaler Index
            // nur noch Forensik. Er darf den geprüften Ordner nicht erneut sperren.
            if directory_dacl_states
                .get(&key)
                .is_some_and(|state| state.closed)
            {
                continue;
            }
        }
        for (side, (binding, identity, root)) in endpoints.iter().enumerate() {
            if exclude_record(&record, side)? {
                continue;
            }
            let paths = record_paths(&record, binding, *identity, root)?;
            if paths.is_empty() {
                continue;
            }
            let active = writer_active(&config, &index, &record)?;
            if !record.remote {
                if let Some(candidate) = &record.candidate {
                    neighbor_activity.insert(
                        (
                            record.owner.queue.clone(),
                            record.owner.operation.clone(),
                            candidate.clone(),
                        ),
                        active,
                    );
                }
            }
            for (path, unknown) in paths {
                candidates.push(NamespaceStagingCandidate {
                    side,
                    object: path,
                    active,
                    requires_absence: !record.named_stream.as_ref().is_some_and(|binding| {
                        binding.phase == NamedStreamIntentPhase::Prepared
                            && binding.temporary_name.is_none()
                    }),
                    unknown,
                });
            }
            records.push((
                index.clone(),
                if record.directory_dacl.is_some() {
                    record_stamp(&record, &data)?
                } else {
                    *blake3::hash(&data).as_bytes()
                },
                active,
            ));
        }
    }
    for (key, state) in &directory_dacl_states {
        if state.closed || seen_directory_dacl.contains(key) {
            continue;
        }
        for (side, (binding, identity, root)) in endpoints.iter().enumerate() {
            for (object, _) in record_paths(&state.record, binding, *identity, root)? {
                candidates.push(NamespaceStagingCandidate {
                    side,
                    object,
                    active: false,
                    requires_absence: true,
                    unknown: true,
                });
            }
        }
    }
    for entry in &entries {
        if !entry.name.starts_with(".stage.doppeldecker-stage-")
            || !entry.name.ends_with(".recovery.json")
        {
            continue;
        }
        if !entry.is_file || entry.is_symlink {
            return Err(io::Error::other(
                "Stage-Registryindex ist keine reguläre Datei",
            ));
        }
        let index_path = config.path().join(&entry.name);
        let mut data = Vec::new();
        crate::local_fs::open_read_beneath(&config, &index_path)?
            .take(64 * 1024 + 1)
            .read_to_end(&mut data)?;
        if data.len() > 64 * 1024 {
            return Err(io::Error::other(
                "Stage-Registryindex überschreitet Größenlimit",
            ));
        }
        let index: Journal = serde_json::from_slice(&data).map_err(io::Error::other)?;
        let owner = index
            .owner
            .as_ref()
            .ok_or_else(|| io::Error::other("Stage-Registryindex besitzt keinen Owner"))?;
        if index.version != 1
            || owner.queue.parent() != Some(config.path())
            || owner.operation.is_empty()
        {
            return Err(io::Error::other("Ungültiger Stage-Registryvertrag"));
        }
        let path = &index.staging_identity.display_path;
        for (side, (binding, _, root)) in endpoints.iter().enumerate() {
            if (side == 1 || exclude_owner_on_both_sides)
                && ignore_target_owner.is_some_and(|ignored| ignored == owner)
            {
                continue;
            }
            if !matches!(binding, crate::EndpointBinding::Local(_)) || !path.starts_with(root) {
                continue;
            }
            let twin_active = neighbor_activity
                .get(&(owner.queue.clone(), owner.operation.clone(), path.clone()))
                .copied();
            let (active, unknown) =
                stage_registry_state(&config, &index_path, &index, twin_active)?;
            candidates.push(NamespaceStagingCandidate {
                side,
                object: StagingObject::Path(path.clone()),
                active,
                requires_absence: true,
                unknown,
            });
            records.push((index_path.clone(), *blake3::hash(&data).as_bytes(), active));
        }
    }
    records.sort();
    let mut hash = blake3::Hasher::new();
    hash.update(&serde_json::to_vec(&records).map_err(io::Error::other)?);
    match crate::local_fs::open_read_beneath(&config, &config.path().join(INTENT_EVENTS)) {
        Ok(file) => {
            let mut reader = std::io::BufReader::new(file);
            loop {
                let line = read_intent_line(&mut reader)?;
                if line.is_empty() {
                    break;
                }
                if !line.ends_with(b"\n") {
                    return Err(io::Error::other("Temp-Ereignisjournal ist unvollständig"));
                }
                let (phase, record) = match decode_intent_event(&line)? {
                    IntentEvent::Record(event) => event,
                    IntentEvent::Recovery { .. } => {
                        // Auch alte unabhängige Permits verfallen einmalig nach
                        // einer Reparatur; frische Scans binden diese Generation.
                        hash.update(&line);
                        continue;
                    }
                };
                if phase == "interrupted" {
                    if record.directory_dacl.is_some()
                        && directory_dacl_states
                            .get(&directory_dacl_key(&record)?)
                            .is_some_and(|state| state.closed)
                    {
                        let mut relevant = false;
                        for (side, (binding, identity, root)) in endpoints.iter().enumerate() {
                            if !exclude_record(&record, side)? {
                                relevant |=
                                    !record_paths(&record, binding, *identity, root)?.is_empty();
                            }
                        }
                        if relevant {
                            hash.update(&line);
                        }
                        continue;
                    }
                    if record.named_stream.is_some() || record.directory_dacl.is_some() {
                        let mut record = *record;
                        apply_named_stream_journal(&mut record, &named_stream_states)?;
                        apply_directory_dacl_journal(&mut record, &directory_dacl_states)?;
                        for (side, (binding, identity, root)) in endpoints.iter().enumerate() {
                            if exclude_record(&record, side)? {
                                continue;
                            }
                            for (object, unknown) in
                                record_paths(&record, binding, *identity, root)?
                            {
                                candidates.push(NamespaceStagingCandidate {
                                    side,
                                    object,
                                    active: false,
                                    requires_absence: true,
                                    unknown,
                                });
                            }
                        }
                        hash.update(&line);
                        continue;
                    }
                    for (side, (binding, identity, root)) in endpoints.iter().enumerate() {
                        if interrupted_record_affects(&record, binding, *identity, root) {
                            candidates.push(NamespaceStagingCandidate {
                                side,
                                object: StagingObject::Path(root.to_path_buf()),
                                active: false,
                                requires_absence: true,
                                unknown: true,
                            });
                        }
                    }
                    hash.update(&line);
                    continue;
                }
                let mut relevant = false;
                for (side, (binding, identity, root)) in endpoints.iter().enumerate() {
                    if exclude_record(&record, side)? {
                        continue;
                    }
                    relevant |= !record_paths(&record, binding, *identity, root)?.is_empty();
                }
                if relevant {
                    hash.update(&line);
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if !config.is_current()? {
        return Err(io::Error::other(
            "Temp-Registry wurde während des Lesens ausgetauscht",
        ));
    }
    Ok(NamespaceStagingView {
        candidates,
        epoch: *hash.finalize().as_bytes(),
    })
}

/// Konkrete Nachbar-Kandidaten, getrennt von lokalen Index-/Quarantänepfaden.
/// Der Aufrufer bindet sie an den dauerhaft gespeicherten Projektionsendpunkt.
pub(crate) struct ForwardStagingCandidate {
    pub(crate) index: PathBuf,
    pub(crate) stamp: [u8; 32],
    pub(crate) remote: bool,
    pub(crate) object: StagingObject,
    pub(crate) active: bool,
    pub(crate) requires_absence: bool,
    pub(crate) endpoint: Option<crate::EndpointBinding>,
    pub(crate) endpoint_root: Option<crate::RootIdentity>,
    pub(crate) ftps: Option<FtpsIntentBinding>,
}

pub(crate) fn forward_staging_candidates(
    owner: &StagingOwner,
) -> io::Result<Vec<ForwardStagingCandidate>> {
    let config = RootHandle::open_nofollow(
        owner
            .queue
            .parent()
            .ok_or_else(|| io::Error::other("Queue ohne Elternordner"))?,
    )?;
    // Bestehenden Parser samt Privatheits-, Versions- und Pfadprüfung verwenden.
    recover_queue_beneath(&owner.queue, &config)?;
    let _lock = lock_intent_events(&config)?;
    repair_intent_tail(&config)?;
    let named_stream_states = named_stream_journal(&config)?;
    let directory_dacl_states = directory_dacl_journal(&config)?;
    if let Some(state) = directory_dacl_states
        .values()
        .find(|state| state.record.owner == *owner && !state.closed)
    {
        return Err(crate::error::with_recovery_paths(
            crate::coded_error(
                io::ErrorKind::WouldBlock,
                crate::ErrorCode::OutcomeUnclear,
                "Verzeichnis-DACL benötigt ihren eigenen Recoveryabschluss",
            ),
            &[state.record.target.to_string_lossy().into_owned()],
        ));
    }
    let mut candidates = Vec::new();
    for entry in crate::local_fs::list_beneath(&config, config.path())? {
        if !entry.name.starts_with(".neighbor.doppeldecker-recovery-")
            || !entry.name.ends_with(".recovery.json")
        {
            continue;
        }
        let mut data = Vec::new();
        crate::local_fs::open_read_beneath(&config, &config.path().join(&entry.name))?
            .take(64 * 1024 + 1)
            .read_to_end(&mut data)?;
        if data.len() > 64 * 1024 {
            return Err(io::Error::other(
                "Temp-Recoveryindex überschreitet Größenlimit",
            ));
        }
        let mut record: NeighborRecord = serde_json::from_slice(&data).map_err(io::Error::other)?;
        validate_neighbor(&record, config.path())?;
        apply_named_stream_journal(&mut record, &named_stream_states)?;
        apply_directory_dacl_journal(&mut record, &directory_dacl_states)?;
        let stamp = record_stamp(&record, &data)?;
        if record.owner != *owner {
            continue;
        }
        let index = config.path().join(&entry.name);
        if let Some(binding) = &record.directory_dacl {
            if directory_dacl_states
                .get(&directory_dacl_key(&record)?)
                .is_some_and(|state| state.closed)
            {
                continue;
            }
            return Err(crate::error::with_recovery_paths(
                crate::coded_error(
                    io::ErrorKind::WouldBlock,
                    crate::ErrorCode::OutcomeUnclear,
                    "Verzeichnis-DACL benötigt ihren eigenen Recoveryabschluss",
                ),
                &[
                    binding.directory.to_string_lossy().into_owned(),
                    index.to_string_lossy().into_owned(),
                ],
            ));
        }
        let active = writer_active(&config, &index, &record)?;
        if let Some(stream) = &record.named_stream {
            for object in stream.objects() {
                candidates.push(ForwardStagingCandidate {
                    index: index.clone(),
                    stamp,
                    remote: true,
                    object,
                    active,
                    requires_absence: !(stream.phase == NamedStreamIntentPhase::Prepared
                        && stream.temporary_name.is_none()),
                    endpoint: record.endpoint.clone(),
                    endpoint_root: record.endpoint_root.clone(),
                    ftps: None,
                });
            }
        } else if let Some(binding) = record.ftps {
            if binding.stou_name_unknown {
                candidates.push(ForwardStagingCandidate {
                    index: config.path().join(&entry.name),
                    stamp,
                    remote: true,
                    object: StagingObject::Path(record.target),
                    active,
                    requires_absence: true,
                    endpoint: record.endpoint.clone(),
                    endpoint_root: record.endpoint_root.clone(),
                    ftps: Some(binding),
                });
            } else {
                for path in &binding.paths {
                    candidates.push(ForwardStagingCandidate {
                        index: config.path().join(&entry.name),
                        stamp,
                        remote: true,
                        object: StagingObject::Path(path.clone()),
                        active,
                        requires_absence: true,
                        endpoint: record.endpoint.clone(),
                        endpoint_root: record.endpoint_root.clone(),
                        ftps: Some(binding.clone()),
                    });
                }
            }
        } else if let Some(path) = record.candidate {
            candidates.push(ForwardStagingCandidate {
                index: config.path().join(&entry.name),
                stamp,
                remote: record.remote,
                object: StagingObject::Path(path),
                active,
                requires_absence: true,
                endpoint: record.endpoint.clone(),
                endpoint_root: record.endpoint_root.clone(),
                ftps: None,
            });
        }
    }
    Ok(candidates)
}

pub(crate) struct StagingRecovery {
    pub(crate) operation: String,
    pub(crate) paths: Vec<String>,
    pub(crate) ftps_root: Option<(crate::remote::FtpsRootIdentity, [u8; 32])>,
    pub(crate) directory_stream_report: crate::directory_streams::DirectoryStreamReport,
    pub(crate) directory_dacl_report: crate::directory_dacl::DirectoryDaclReport,
    pub(crate) file_dacl_report: crate::file_dacl::FileDaclReport,
    pub(crate) file_dacl_recovery: bool,
    pub(crate) file_dacl_acknowledged: bool,
    pub(crate) file_dacl_acknowledged_paths: Vec<String>,
    pub(crate) file_dacl_resolved: Vec<crate::file_dacl::FileDaclOutcome>,
    pub(crate) created_directory_dacl_report:
        crate::created_directory_dacl::CreatedDirectoryDaclReport,
    pub(crate) created_directory_dacl_recovery: bool,
    pub(crate) before_children_recovery: crate::created_directory_dacl::BeforeChildrenRecovery,
    pub(crate) created_directory_dacl_acknowledged: bool,
    pub(crate) created_directory_dacl_acknowledged_paths: Vec<String>,
    pub(crate) created_directory_dacl_resolved:
        Vec<crate::created_directory_dacl::CreatedDirectoryDaclOutcome>,
    pub(crate) directory_dacl_acknowledged: bool,
    pub(crate) directory_dacl_recovery: bool,
    pub(crate) directory_dacl_acknowledged_paths: Vec<String>,
}

/// Unter der gemeinsamen Prozesssperre lesen, aber niemals allein aufgrund eines
/// alten Journals löschen oder einen alten Permit wiederbeleben. Ein laufendes
/// anderes Fenster hält seine zusätzliche Staging-Sperre und bleibt ausgeschlossen.
#[cfg(test)]
pub(crate) fn recover_queue(queue: &Path) -> io::Result<Vec<StagingRecovery>> {
    let config = RootHandle::open_nofollow(
        queue
            .parent()
            .ok_or_else(|| io::Error::other("Queue ohne Elternordner"))?,
    )?;
    recover_queue_beneath(queue, &config)
}

pub(crate) fn recover_queue_beneath(
    queue: &Path,
    config: &RootHandle,
) -> io::Result<Vec<StagingRecovery>> {
    if queue.parent() != Some(config.path()) || !config.is_current()? {
        return Err(io::Error::other("Queue-Recoveryordner wurde ausgetauscht"));
    }
    if !config.has_exclusive_namespace()? {
        if crate::local_fs::list_beneath(config, config.path())?
            .iter()
            .any(|entry| {
                ((entry.name.starts_with(".stage.doppeldecker-stage-")
                    || entry.name.starts_with(".neighbor.doppeldecker-recovery-"))
                    && entry.name.ends_with(".recovery.json"))
                    || ((entry.name.starts_with(".file-dacl-v6-")
                        || entry.name.starts_with(".created-directory-dacl-v7-"))
                        && (entry.is_dir
                            || entry.name.ends_with(".json")
                            || entry.name.ends_with(".lease")))
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Recoveryindex verlor seinen gegen fremdes Schreiben geschützten Namensraum",
            ));
        }
        return Ok(Vec::new());
    }
    let _lock = lock_intent_events(config)?;
    repair_intent_tail(config)?;
    let named_stream_states = named_stream_journal(config)?;
    let directory_dacl_states = directory_dacl_journal(config)?;
    let mut recovered = Vec::new();
    let mut seen_directory_dacl = std::collections::BTreeSet::new();
    let mut directory_dacl_index_paths = std::collections::BTreeMap::<String, Vec<String>>::new();
    for entry in crate::local_fs::list_beneath(config, config.path())? {
        if entry.name.starts_with(".neighbor.doppeldecker-recovery-")
            && entry.name.ends_with(".recovery.json")
        {
            if !entry.is_file || entry.is_symlink {
                return Err(io::Error::other(
                    "Temp-Recoveryindex ist keine reguläre Datei",
                ));
            }
            let mut data = Vec::new();
            crate::local_fs::open_read_beneath(config, &config.path().join(&entry.name))?
                .take(64 * 1024 + 1)
                .read_to_end(&mut data)?;
            if data.len() > 64 * 1024 {
                return Err(io::Error::other(
                    "Temp-Recoveryindex überschreitet Größenlimit",
                ));
            }
            let mut record: NeighborRecord =
                serde_json::from_slice(&data).map_err(io::Error::other)?;
            if record.owner.queue != queue {
                continue;
            }
            validate_neighbor(&record, config.path())?;
            apply_named_stream_journal(&mut record, &named_stream_states)?;
            apply_directory_dacl_journal(&mut record, &directory_dacl_states)?;
            if record.stage_guard {
                let candidate = record
                    .candidate
                    .as_ref()
                    .ok_or_else(|| io::Error::other("Stage-Creationguard ohne Kandidat"))?;
                let name = candidate
                    .file_name()
                    .ok_or_else(|| io::Error::other("Stage-Creationguard ohne Namen"))?;
                let mut index_name = name.to_os_string();
                index_name.push(".recovery.json");
                match crate::local_fs::stat_beneath(config, &config.path().join(index_name)) {
                    Ok(entry) if entry.is_file && !entry.is_symlink => continue,
                    Ok(_) => return Err(io::Error::other("Stageindex wurde ausgetauscht")),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        if writer_active(config, &config.path().join(&entry.name), &record)? {
                            continue;
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
            if let Some(binding) = &record.directory_dacl {
                seen_directory_dacl.insert(directory_dacl_key(&record)?);
                directory_dacl_index_paths
                    .entry(record.owner.operation.clone())
                    .or_default()
                    .push(
                        config
                            .path()
                            .join(&entry.name)
                            .to_string_lossy()
                            .into_owned(),
                    );
                recovered.push(StagingRecovery {
                    operation: record.owner.operation.clone(),
                    paths: vec![
                        binding.directory.to_string_lossy().into_owned(),
                        config
                            .path()
                            .join(&entry.name)
                            .to_string_lossy()
                            .into_owned(),
                    ],
                    ftps_root: None,
                    directory_stream_report: Default::default(),
                    directory_dacl_report: Default::default(),
                    file_dacl_report: Default::default(),
                    file_dacl_recovery: false,
                    file_dacl_acknowledged: false,
                    file_dacl_acknowledged_paths: Vec::new(),
                    file_dacl_resolved: Vec::new(),
                    created_directory_dacl_report: Default::default(),
                    created_directory_dacl_recovery: false,
                    before_children_recovery: Default::default(),
                    created_directory_dacl_acknowledged: false,
                    created_directory_dacl_acknowledged_paths: Vec::new(),
                    created_directory_dacl_resolved: Vec::new(),
                    directory_dacl_acknowledged: false,
                    directory_dacl_acknowledged_paths: Vec::new(),
                    directory_dacl_recovery: directory_dacl_states
                        .get(&directory_dacl_key(&record)?)
                        .is_none_or(|state| !state.closed),
                });
                continue;
            }
            if let Some(binding) = &record.named_stream {
                let mut paths = binding
                    .objects()
                    .iter()
                    .map(StagingObject::recovery_hint)
                    .collect::<Vec<_>>();
                paths.push(
                    config
                        .path()
                        .join(&entry.name)
                        .to_string_lossy()
                        .into_owned(),
                );
                recovered.push(StagingRecovery {
                    operation: record.owner.operation.clone(),
                    paths,
                    ftps_root: None,
                    directory_dacl_report: Default::default(),
                    file_dacl_report: Default::default(),
                    file_dacl_recovery: false,
                    file_dacl_acknowledged: false,
                    file_dacl_acknowledged_paths: Vec::new(),
                    file_dacl_resolved: Vec::new(),
                    created_directory_dacl_report: Default::default(),
                    created_directory_dacl_recovery: false,
                    before_children_recovery: Default::default(),
                    created_directory_dacl_acknowledged: false,
                    created_directory_dacl_acknowledged_paths: Vec::new(),
                    created_directory_dacl_resolved: Vec::new(),
                    directory_dacl_acknowledged: false,
                    directory_dacl_recovery: false,
                    directory_dacl_acknowledged_paths: Vec::new(),
                    directory_stream_report: named_stream_states
                        .get(&named_stream_key(&record)?)
                        .map_or_else(|| binding.report(), |state| state.report.clone()),
                });
                continue;
            }
            if let Some(binding) = record.ftps {
                validate_ftps_intent(&binding, &record.target)?;
                if record.owner.operation.len() != 32
                    || !record
                        .owner
                        .operation
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(io::Error::other("Ungültiger FTPS-Intentowner"));
                }
                let mut paths = binding.paths;
                paths.push(config.path().join(&entry.name));
                paths.push(record.target.clone());
                if binding.stou_name_unknown {
                    if let Some(parent) = record.target.parent() {
                        paths.push(parent.to_path_buf());
                    }
                }
                paths.sort();
                paths.dedup();
                recovered.push(StagingRecovery {
                    operation: record.owner.operation,
                    paths: paths
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect(),
                    ftps_root: Some((binding.roots[0].clone(), binding.account_digest)),
                    directory_stream_report: Default::default(),
                    directory_dacl_report: Default::default(),
                    file_dacl_report: Default::default(),
                    file_dacl_recovery: false,
                    file_dacl_acknowledged: false,
                    file_dacl_acknowledged_paths: Vec::new(),
                    file_dacl_resolved: Vec::new(),
                    created_directory_dacl_report: Default::default(),
                    created_directory_dacl_recovery: false,
                    before_children_recovery: Default::default(),
                    created_directory_dacl_acknowledged: false,
                    created_directory_dacl_acknowledged_paths: Vec::new(),
                    created_directory_dacl_resolved: Vec::new(),
                    directory_dacl_acknowledged: false,
                    directory_dacl_recovery: false,
                    directory_dacl_acknowledged_paths: Vec::new(),
                });
                continue;
            }
            if let Some(candidate) = record.candidate {
                let mut paths = vec![candidate.to_string_lossy().into_owned()];
                if !record.remote {
                    paths.push(apple_double(&candidate).to_string_lossy().into_owned());
                }
                paths.push(record.target.to_string_lossy().into_owned());
                recovered.push(StagingRecovery {
                    operation: record.owner.operation,
                    paths,
                    ftps_root: None,
                    directory_stream_report: Default::default(),
                    directory_dacl_report: Default::default(),
                    file_dacl_report: Default::default(),
                    file_dacl_recovery: false,
                    file_dacl_acknowledged: false,
                    file_dacl_acknowledged_paths: Vec::new(),
                    file_dacl_resolved: Vec::new(),
                    created_directory_dacl_report: Default::default(),
                    created_directory_dacl_recovery: false,
                    before_children_recovery: Default::default(),
                    created_directory_dacl_acknowledged: false,
                    created_directory_dacl_acknowledged_paths: Vec::new(),
                    created_directory_dacl_resolved: Vec::new(),
                    directory_dacl_acknowledged: false,
                    directory_dacl_recovery: false,
                    directory_dacl_acknowledged_paths: Vec::new(),
                });
            }
            continue;
        }
        if !entry.is_file
            || entry.is_symlink
            || !entry.name.starts_with(".stage.doppeldecker-stage-")
            || !entry.name.ends_with(".recovery.json")
        {
            continue;
        }
        let mut data = Vec::new();
        crate::local_fs::open_read_beneath(config, &config.path().join(&entry.name))?
            .take(64 * 1024 + 1)
            .read_to_end(&mut data)?;
        if data.len() > 64 * 1024 {
            return Err(io::Error::other(
                "Staging-Recoveryindex überschreitet Größenlimit",
            ));
        }
        let index: Journal = serde_json::from_slice(&data).map_err(io::Error::other)?;
        let Some(owner) = &index.owner else {
            continue;
        };
        if owner.queue != queue {
            continue;
        }
        if index.version != 1 || owner.operation.is_empty() {
            return Err(io::Error::other("Unbekannter Staging-Recoveryvertrag"));
        }
        let stage_path = &index.staging_identity.display_path;
        let mut recovery = StagingRecovery {
            operation: owner.operation.clone(),
            paths: Vec::new(),
            ftps_root: None,
            directory_stream_report: Default::default(),
            directory_dacl_report: Default::default(),
            file_dacl_report: Default::default(),
            file_dacl_recovery: false,
            file_dacl_acknowledged: false,
            file_dacl_acknowledged_paths: Vec::new(),
            file_dacl_resolved: Vec::new(),
            created_directory_dacl_report: Default::default(),
            created_directory_dacl_recovery: false,
            before_children_recovery: Default::default(),
            created_directory_dacl_acknowledged: false,
            created_directory_dacl_acknowledged_paths: Vec::new(),
            created_directory_dacl_resolved: Vec::new(),
            directory_dacl_acknowledged: false,
            directory_dacl_recovery: false,
            directory_dacl_acknowledged_paths: Vec::new(),
        };
        let validated = (|| -> io::Result<RootHandle> {
            let anchor = RootHandle::open_nofollow(&index.anchor_identity.display_path)?;
            if !anchor.is_private()?
                || crate::local_fs::root_identity_beneath(&anchor, anchor.path())?
                    != index.anchor_identity
            {
                return Err(io::Error::other("Staging-Ankeridentität änderte sich"));
            }
            if stage_path.parent() != Some(anchor.path()) {
                return Err(io::Error::other(
                    "Staging liegt außerhalb des manifestierten Ankers",
                ));
            }
            let stage = RootHandle::open_beneath(&anchor, stage_path)?;
            if !stage.is_private()?
                || crate::local_fs::root_identity_beneath(&stage, stage_path)?
                    != index.staging_identity
            {
                return Err(io::Error::other("Staging-Identität änderte sich"));
            }
            Ok(stage)
        })();
        let stage = match validated {
            Ok(stage) => stage,
            Err(_) => {
                recovery
                    .paths
                    .push(stage_path.to_string_lossy().into_owned());
                recovered.push(recovery);
                continue;
            }
        };
        let lease = match openat(
            stage.fd(),
            "lease",
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(_) => {
                // Ein Abbruch beim abschließenden Entfernen darf weder blindes
                // Aufräumen noch den Verlust des privaten Recoveryhinweises bewirken.
                recovery
                    .paths
                    .push(stage_path.to_string_lossy().into_owned());
                recovered.push(recovery);
                continue;
            }
        };
        match rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => continue,
            Err(error) => return Err(error.into()),
        }
        let mut data = Vec::new();
        let journal_file =
            match crate::local_fs::open_read_beneath(&stage, &stage_path.join("journal.json")) {
                Ok(file) => file,
                Err(_) => {
                    recovery
                        .paths
                        .push(stage_path.to_string_lossy().into_owned());
                    recovered.push(recovery);
                    continue;
                }
            };
        journal_file.take(1024 * 1024 + 1).read_to_end(&mut data)?;
        if data.len() > 1024 * 1024 {
            return Err(io::Error::other("Stagingjournal überschreitet Größenlimit"));
        }
        let journal: Journal = serde_json::from_slice(&data).map_err(io::Error::other)?;
        if journal.version != index.version
            || journal.staging_identity != index.staging_identity
            || journal
                .owner
                .as_ref()
                .map(|owner| (&owner.queue, &owner.operation))
                != index
                    .owner
                    .as_ref()
                    .map(|owner| (&owner.queue, &owner.operation))
        {
            return Err(io::Error::other(
                "Stagingjournal passt nicht zum Recoveryindex",
            ));
        }
        for item in crate::local_fs::list_beneath(&stage, stage_path)? {
            if !matches!(
                item.name.as_str(),
                "journal.json" | "journal.next" | "lease"
            ) {
                recovery
                    .paths
                    .push(stage_path.join(item.name).to_string_lossy().into_owned());
            }
        }
        if recovery.paths.is_empty()
            && journal
                .actions
                .last()
                .is_none_or(|action| action.phase != "complete")
        {
            recovery
                .paths
                .push(stage_path.to_string_lossy().into_owned());
        }
        if !recovery.paths.is_empty() {
            recovered.push(recovery);
        }
    }
    // Ein einziger Ownerbeleg ersetzt bei bestätigtem Abschluss auch einen
    // älteren uncertain-Queuecheckpoint. Einzelne geschlossene Intents dürfen
    // dagegen keine andere noch offene DACL-Wirkung desselben Owners verdecken.
    let mut owner_reports = std::collections::BTreeMap::<
        String,
        (
            crate::directory_dacl::DirectoryDaclReport,
            bool,
            Vec<String>,
        ),
    >::new();
    for (key, state) in directory_dacl_states
        .iter()
        .filter(|(_, state)| state.record.owner.queue == queue)
    {
        let aggregate = owner_reports
            .entry(state.record.owner.operation.clone())
            .or_insert_with(|| (Default::default(), true, Vec::new()));
        aggregate.0.merge(&state.report);
        aggregate.1 &= state.closed;
        if !state.closed && !seen_directory_dacl.contains(key) {
            aggregate
                .2
                .push(state.record.target.to_string_lossy().into_owned());
        }
    }
    for (operation, (directory_dacl_report, directory_dacl_acknowledged, paths)) in owner_reports {
        let mut directory_dacl_acknowledged_paths = if directory_dacl_acknowledged {
            directory_dacl_states
                .values()
                .filter(|state| {
                    state.record.owner.queue == queue
                        && state.record.owner.operation == operation
                        && state.closed
                })
                .map(|state| state.record.target.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        if directory_dacl_acknowledged {
            directory_dacl_acknowledged_paths.extend(
                directory_dacl_index_paths
                    .remove(&operation)
                    .unwrap_or_default(),
            );
        }
        directory_dacl_acknowledged_paths.sort();
        directory_dacl_acknowledged_paths.dedup();
        recovered.push(StagingRecovery {
            operation,
            paths,
            ftps_root: None,
            directory_stream_report: Default::default(),
            directory_dacl_report,
            file_dacl_report: Default::default(),
            file_dacl_recovery: false,
            file_dacl_acknowledged: false,
            file_dacl_acknowledged_paths: Vec::new(),
            file_dacl_resolved: Vec::new(),
            created_directory_dacl_report: Default::default(),
            created_directory_dacl_recovery: false,
            before_children_recovery: Default::default(),
            created_directory_dacl_acknowledged: false,
            created_directory_dacl_acknowledged_paths: Vec::new(),
            created_directory_dacl_resolved: Vec::new(),
            directory_dacl_acknowledged,
            directory_dacl_recovery: !directory_dacl_acknowledged,
            directory_dacl_acknowledged_paths,
        });
    }
    recovered.extend(crate::file_dacl_registry::recover_queue(queue, config)?);
    recovered.extend(crate::created_directory_dacl_registry::recover_queue(
        queue, config,
    )?);
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    fn stream_identity(root: &str) -> crate::RootIdentity {
        crate::RootIdentity::Smb(crate::remote::SmbRootIdentity {
            server: crate::remote::SmbServerIdentity {
                host: "server".into(),
                port: 445,
                server_guid: [1; 16],
                share: "share".into(),
                account_digest: [2; 32],
                canonical_root: "/vault".into(),
                dialect: 0x0311,
                signing_active: true,
                encryption_active: true,
                signing_algorithm: "AES-GMAC".into(),
                encryption_cipher: "AES-128-GCM".into(),
            },
            virtual_root: root.into(),
            root_file_id: 1,
            tree_id: 1,
            session_generation: 1,
        })
    }
    fn stream_binding() -> super::NamedStreamIntentBinding {
        super::NamedStreamIntentBinding {
            base_directory: "/pane/folder".into(),
            temporary_name: Some(b"temporary".to_vec()),
            final_name: b"final".to_vec(),
            effect: crate::directory_streams::DirectoryStreamEffect::Add {
                after: crate::metadata::NamedBlobSummary {
                    name: b"final".to_vec(),
                    value: crate::metadata::BlobSummary::from_bytes(b"data"),
                },
            },
            phase: super::NamedStreamIntentPhase::Prepared,
        }
    }

    #[test]
    fn sparse_commit_intent_survives_handle_drop_and_blocks_missing_temp() {
        use super::*;
        let (_fixture, config) = sandbox();
        let owner = StagingOwner {
            queue: config.join("queue.json"),
            operation: "a".repeat(32),
        };
        let identity = stream_identity("/pane");
        let binding = crate::EndpointBinding::remote(&identity, [2; 32]).unwrap();
        let scopes = [(&binding, Some(&identity), Path::new("/pane"))];
        let intent = RemoteTempIntent::create_bound_with_root(
            &owner,
            Path::new("/pane/final"),
            Path::new("/pane/.partial"),
            &identity,
            [2; 32],
        )
        .unwrap();
        intent.smb_commit_pending().unwrap();
        let index = intent.path().to_path_buf();
        let bytes = std::fs::read(&index).unwrap();
        let record: NeighborRecord = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record.version, 3);
        assert!(record.candidate.is_none());
        drop(intent);
        let view = namespace_staging_view(&config, &scopes).unwrap();
        assert_eq!(view.candidates.len(), 1);
        assert_eq!(
            view.candidates[0].object,
            StagingObject::Path("/pane/final".into())
        );
        assert!(!view.candidates[0].active);
        assert!(view.candidates[0].unknown);
        assert!(view.candidates[0].requires_absence);
        assert_eq!(std::fs::read(index).unwrap(), bytes);
    }

    #[test]
    fn rejected_sparse_commit_restores_known_candidate_instead_of_unknown_namespace() {
        use super::*;
        let (_fixture, config) = sandbox();
        let owner = StagingOwner {
            queue: config.join("queue.json"),
            operation: "a".repeat(32),
        };
        let identity = stream_identity("/pane");
        let binding = crate::EndpointBinding::remote(&identity, [2; 32]).unwrap();
        let scopes = [(&binding, Some(&identity), Path::new("/pane"))];
        let intent = RemoteTempIntent::create_bound_with_root(
            &owner,
            Path::new("/pane/final"),
            Path::new("/pane/.partial"),
            &identity,
            [2; 32],
        )
        .unwrap();
        assert!(intent
            .smb_commit_rejected(Path::new("/pane/.partial"))
            .is_err());
        intent.smb_commit_pending().unwrap();
        assert!(intent
            .smb_commit_rejected(Path::new("/other/.partial"))
            .is_err());
        intent
            .smb_commit_rejected(Path::new("/pane/.partial"))
            .unwrap();
        drop(intent);
        let view = namespace_staging_view(&config, &scopes).unwrap();
        assert_eq!(view.candidates.len(), 1);
        assert!(!view.candidates[0].unknown);
        assert_eq!(
            view.candidates[0].object,
            StagingObject::Path("/pane/.partial".into())
        );
    }

    #[test]
    fn directory_stream_registry_phases_recovery_and_typed_root_translation() {
        use super::*;
        let (_fixture, config) = sandbox();
        let owner = StagingOwner {
            queue: config.join("queue.json"),
            operation: "a".repeat(32),
        };
        let identity = stream_identity("/pane");
        let binding = crate::EndpointBinding::remote(&identity, [2; 32]).unwrap();
        let scope = [(&binding, Some(&identity), Path::new("/pane"))];
        let intent = RemoteTempIntent::create_named_stream_with_root(
            &owner,
            stream_binding(),
            &identity,
            [2; 32],
        )
        .unwrap();
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(intent.path()).unwrap()).unwrap();
        assert_eq!(data["version"], 4);
        assert!(data["candidate"].is_null());
        let view = namespace_staging_view(&config, &scope).unwrap();
        assert_eq!(view.candidates.len(), 1);
        assert!(
            view.candidates[0].active,
            "Lease muss vor sichtbarer Streamanlage sperren"
        );
        assert_eq!(
            view.candidates[0].object,
            StagingObject::NamedStream {
                base_directory: "/pane/folder".into(),
                name: b"temporary".to_vec()
            }
        );
        let nested = stream_identity("/pane/folder");
        let nested_binding = crate::EndpointBinding::remote(&nested, [2; 32]).unwrap();
        let view =
            namespace_staging_view(&config, &[(&nested_binding, Some(&nested), Path::new("/"))])
                .unwrap();
        assert_eq!(view.candidates[0].object.base_path(), Path::new("/"));
        let unrelated = stream_identity("/independent");
        let unrelated_binding = crate::EndpointBinding::remote(&unrelated, [2; 32]).unwrap();
        assert!(namespace_staging_view(
            &config,
            &[(&unrelated_binding, Some(&unrelated), Path::new("/"))]
        )
        .unwrap()
        .candidates
        .is_empty());
        let mut other_identity = identity.clone();
        if let crate::RootIdentity::Smb(value) = &mut other_identity {
            value.server.account_digest = [3; 32];
        }
        let other = crate::EndpointBinding::remote(&other_identity, [3; 32]).unwrap();
        assert!(
            namespace_staging_view(&config, &[(&other, Some(&identity), Path::new("/pane"))])
                .unwrap()
                .candidates
                .is_empty()
        );
        assert!(intent
            .named_stream_phase(NamedStreamIntentPhase::Committed)
            .is_err());
        intent
            .named_stream_phase(NamedStreamIntentPhase::Committing)
            .unwrap();
        assert_eq!(
            namespace_staging_view(&config, &scope)
                .unwrap()
                .candidates
                .len(),
            2
        );
        assert_eq!(
            recover_queue(&owner.queue).unwrap()[0]
                .directory_stream_report
                .uncertain
                .len(),
            1
        );
        intent
            .named_stream_phase(NamedStreamIntentPhase::Committed)
            .unwrap();
        let recovery = recover_queue(&owner.queue).unwrap();
        assert_eq!(recovery[0].directory_stream_report.completed.len(), 1);
        assert_eq!(forward_staging_candidates(&owner).unwrap().len(), 2);
        drop(intent);
        assert!(!namespace_staging_view(&config, &scope).unwrap().candidates[0].active);
        assert_eq!(
            recover_queue(&owner.queue).unwrap()[0]
                .directory_stream_report
                .completed
                .len(),
            1
        );
    }

    #[test]
    fn directory_stream_registry_removal_and_legacy_json_remain_separate() {
        use super::*;
        let (_fixture, config) = sandbox();
        let owner = StagingOwner {
            queue: config.join("queue.json"),
            operation: "b".repeat(32),
        };
        let legacy =
            RemoteTempIntent::create(&owner, Path::new("/pane/file"), Path::new("/pane/.temp"))
                .unwrap();
        let bytes = std::fs::read(legacy.path()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("named_stream"));
        legacy.finish().unwrap();
        let identity = stream_identity("/pane");
        let mut binding = stream_binding();
        binding.effect = crate::directory_streams::DirectoryStreamEffect::Remove {
            before: binding.effect.after().unwrap().clone(),
        };
        binding.temporary_name = None;
        let intent =
            RemoteTempIntent::create_named_stream_with_root(&owner, binding, &identity, [2; 32])
                .unwrap();
        intent
            .named_stream_phase(NamedStreamIntentPhase::Removing)
            .unwrap();
        assert_eq!(
            recover_queue(&owner.queue).unwrap()[0]
                .directory_stream_report
                .uncertain
                .len(),
            1
        );
        intent
            .named_stream_phase(NamedStreamIntentPhase::Removed)
            .unwrap();
        let candidates = forward_staging_candidates(&owner).unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(
            matches!(&candidates[0].object, StagingObject::NamedStream { name, .. } if name == b"final")
        );
        assert_eq!(
            recover_queue(&owner.queue).unwrap()[0]
                .directory_stream_report
                .completed
                .len(),
            1
        );
        intent.finish().unwrap();
        assert!(forward_staging_candidates(&owner).unwrap().is_empty());
    }

    struct StreamObservation {
        local: crate::LocalVfs,
        manifest: Option<crate::metadata::MetadataManifest>,
        identity: crate::RootIdentity,
        stat_error: Option<std::io::ErrorKind>,
    }
    impl crate::Vfs for StreamObservation {
        fn root_identity(&self, _: &std::path::Path) -> std::io::Result<crate::RootIdentity> {
            Ok(self.identity.clone())
        }
        fn list(&self, _: &std::path::Path) -> std::io::Result<Vec<crate::Entry>> {
            unreachable!()
        }
        fn stat(&self, path: &std::path::Path) -> std::io::Result<crate::Entry> {
            if let Some(kind) = self.stat_error {
                return Err(std::io::Error::new(kind, "Stat nicht verfügbar"));
            }
            crate::Vfs::stat(&self.local, path)
        }
        fn exists(&self, _: &std::path::Path) -> bool {
            unreachable!()
        }
        fn metadata_manifest(
            &self,
            _: &std::path::Path,
        ) -> std::io::Result<crate::metadata::MetadataManifest> {
            self.manifest.clone().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nicht lesbar")
            })
        }
        fn create_dir_all(&self, _path: &std::path::Path) -> std::io::Result<()> {
            unreachable!()
        }
        fn create_dir(&self, _path: &std::path::Path) -> std::io::Result<()> {
            unreachable!()
        }
        fn sync_file(&self, _path: &std::path::Path) -> std::io::Result<()> {
            unreachable!()
        }
        fn sync_dir(&self, _path: &std::path::Path) -> std::io::Result<()> {
            unreachable!()
        }
        fn set_mtime(&self, _path: &std::path::Path, _mtime: i64) -> std::io::Result<()> {
            unreachable!()
        }
        fn set_mode(&self, _path: &std::path::Path, _mode: u32) -> std::io::Result<()> {
            unreachable!()
        }
        fn remove_file(&self, _path: &std::path::Path) -> std::io::Result<()> {
            unreachable!()
        }
        fn remove_dir_all(&self, _path: &std::path::Path) -> std::io::Result<()> {
            unreachable!()
        }
        fn rename(&self, _from: &std::path::Path, _to: &std::path::Path) -> std::io::Result<()> {
            unreachable!()
        }
        fn rename_noreplace(
            &self,
            _from: &std::path::Path,
            _to: &std::path::Path,
        ) -> std::io::Result<()> {
            unreachable!()
        }
        fn open_read(
            &self,
            _path: &std::path::Path,
        ) -> std::io::Result<Box<dyn std::io::Read + Send>> {
            unreachable!()
        }
        fn open_write(
            &self,
            _path: &std::path::Path,
        ) -> std::io::Result<Box<dyn std::io::Write + Send>> {
            unreachable!()
        }
        fn open_write_new(
            &self,
            _path: &std::path::Path,
        ) -> std::io::Result<Box<dyn std::io::Write + Send>> {
            unreachable!()
        }
    }

    #[test]
    fn directory_stream_registry_absence_requires_observed_set_and_never_stats_stream_path() {
        use super::*;
        let (_fixture, root) = sandbox();
        let base = root.join("folder");
        std::fs::create_dir(&base).unwrap();
        let object = StagingObject::NamedStream {
            base_directory: base.clone(),
            name: b"final".to_vec(),
        };
        let mut vfs = StreamObservation {
            local: crate::LocalVfs::rooted(&root).unwrap(),
            manifest: None,
            identity: stream_identity("/pane"),
            stat_error: None,
        };
        assert!(object.is_absent(&vfs).is_err());
        let manifest =
            crate::metadata::MetadataManifest::new(None, 0o700, None, vec![], None, None, None)
                .unwrap();
        vfs.manifest = Some(manifest.clone());
        assert!(
            object.is_absent(&vfs).is_err(),
            "None ist keine leere Menge"
        );
        vfs.manifest = Some(manifest.clone().with_named_streams(Some(vec![])).unwrap());
        assert!(
            object.is_absent(&vfs).unwrap(),
            "Der vorhandene Basisordner ist kein Temp"
        );
        vfs.manifest = Some(
            manifest
                .clone()
                .with_named_streams(Some(vec![stream_binding().effect.after().unwrap().clone()]))
                .unwrap(),
        );
        assert!(
            !object.is_absent(&vfs).unwrap(),
            "Finalname muss nach Rename weiter sperren"
        );
        for (requested, observed) in [("final", "FINAL"), ("straße", "STRASSE")] {
            let alias = StagingObject::NamedStream {
                base_directory: base.clone(),
                name: requested.as_bytes().to_vec(),
            };
            vfs.manifest = Some(
                manifest
                    .clone()
                    .with_named_streams(Some(vec![crate::metadata::NamedBlobSummary {
                        name: observed.as_bytes().to_vec(),
                        value: crate::metadata::BlobSummary::from_bytes(b"erhalten"),
                    }]))
                    .unwrap(),
            );
            assert!(
                !alias.is_absent(&vfs).unwrap(),
                "SMB-Lookupalias ist keine bewiesene Abwesenheit"
            );
        }
        vfs.manifest.as_mut().unwrap().digest = [0; 32];
        assert!(object.is_absent(&vfs).is_err());
        let file = root.join("file");
        std::fs::write(&file, b"safe").unwrap();
        assert!(object.with_base_path(file).is_absent(&vfs).is_err());
        let link = root.join("link");
        std::os::unix::fs::symlink(&base, &link).unwrap();
        assert!(object.with_base_path(link).is_absent(&vfs).is_err());
        assert!(object
            .with_base_path(root.join("missing"))
            .is_absent(&vfs)
            .unwrap());
        vfs.stat_error = Some(io::ErrorKind::PermissionDenied);
        assert!(object.is_absent(&vfs).is_err());
        assert!(base.is_dir());
    }

    #[test]
    fn directory_stream_registry_guard_blocks_lease_and_final_but_allows_observed_absence() {
        use super::*;
        let (_fixture, root) = sandbox();
        let config = root.join("config");
        let data = root.join("data");
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&config)
            .unwrap();
        std::fs::create_dir(&data).unwrap();
        std::fs::create_dir(data.join("folder")).unwrap();
        let owner = StagingOwner {
            queue: config.join("queue.json"),
            operation: "c".repeat(32),
        };
        let identity = stream_identity("/pane");
        let binding = crate::EndpointBinding::remote(&identity, [2; 32]).unwrap();
        let empty =
            crate::metadata::MetadataManifest::new(None, 0o700, None, vec![], None, None, None)
                .unwrap()
                .with_named_streams(Some(vec![]))
                .unwrap();
        let endpoint = |manifest| crate::staging_guard::StagingEndpoint {
            vfs: std::sync::Arc::new(StreamObservation {
                local: crate::LocalVfs::rooted(&data).unwrap(),
                identity: identity.clone(),
                manifest,
                stat_error: None,
            }),
            root: data.clone(),
            binding: binding.clone(),
        };
        let registry =
            crate::staging_guard::KnownStagingRegistry::at_private_root(&config).unwrap();
        let intent = RemoteTempIntent::create_named_stream_with_root(
            &owner,
            stream_binding(),
            &identity,
            [2; 32],
        )
        .unwrap();
        assert!(
            registry
                .check(&[endpoint(Some(empty.clone()))], None)
                .is_err(),
            "Aktive Lease sperrt vor CREATE"
        );
        intent
            .named_stream_phase(NamedStreamIntentPhase::Committing)
            .unwrap();
        drop(intent);
        let final_set = empty
            .clone()
            .with_named_streams(Some(vec![stream_binding().effect.after().unwrap().clone()]))
            .unwrap();
        assert!(
            registry.check(&[endpoint(Some(final_set))], None).is_err(),
            "Fehlender Temp allein reicht nach Renameversuch nicht"
        );
        assert!(registry.check(&[endpoint(None)], None).is_err());
        assert!(
            registry.check(&[endpoint(Some(empty))], None).is_ok(),
            "Bewusst außerhalb gesicherte Streams erlauben frischen Abgleich"
        );
        assert!(data.join("folder").is_dir());
    }

    #[test]
    fn directory_stream_registry_recovers_durable_event_after_index_write_failure() {
        use super::*;
        let (_fixture, config) = sandbox();
        let owner = StagingOwner {
            queue: config.join("queue.json"),
            operation: "d".repeat(32),
        };
        let identity = stream_identity("/pane");
        let intent = RemoteTempIntent::create_named_stream_with_root(
            &owner,
            stream_binding(),
            &identity,
            [2; 32],
        )
        .unwrap();
        intent
            .named_stream_phase(NamedStreamIntentPhase::Committing)
            .unwrap();
        let mut event = intent.record.lock().unwrap().clone();
        event.named_stream.as_mut().unwrap().phase = NamedStreamIntentPhase::Committed;
        let held = RootHandle::open_nofollow(&config).unwrap();
        {
            let _lock = lock_intent_events(&held).unwrap();
            append_intent_event_locked(&held, &event, "update").unwrap();
        }
        drop(intent);
        let recovery = recover_queue(&owner.queue).unwrap();
        assert_eq!(recovery[0].directory_stream_report.completed.len(), 1);
        assert!(recovery[0].directory_stream_report.uncertain.is_empty());
        assert_eq!(forward_staging_candidates(&owner).unwrap().len(), 2);
        assert_eq!(
            std::fs::read_dir(&config)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".recovery.json"))
                .count(),
            1
        );
    }

    #[test]
    fn directory_stream_registry_repaired_tail_keeps_exact_names_and_independent_roots() {
        use super::*;
        let (_fixture, config) = sandbox();
        let owner = StagingOwner {
            queue: config.join("queue.json"),
            operation: "e".repeat(32),
        };
        let identity = stream_identity("/pane");
        let binding = crate::EndpointBinding::remote(&identity, [2; 32]).unwrap();
        let intent = RemoteTempIntent::create_named_stream_with_root(
            &owner,
            stream_binding(),
            &identity,
            [2; 32],
        )
        .unwrap();
        let mut update = intent.record.lock().unwrap().clone();
        update.named_stream.as_mut().unwrap().phase = NamedStreamIntentPhase::Committing;
        append_partial_event(&config, &update, "update", 180);
        drop(intent);
        let scope = [(&binding, Some(&identity), Path::new("/pane"))];
        let view = namespace_staging_view(&config, &scope).unwrap();
        assert!(view.candidates.iter().all(|candidate| matches!(
            candidate.object,
            StagingObject::NamedStream { .. }
        ) && !candidate.unknown));
        assert!(view.candidates.iter().any(|candidate| matches!(&candidate.object, StagingObject::NamedStream { name, .. } if name == b"final")));
        assert_eq!(forward_staging_candidates(&owner).unwrap().len(), 2);
        assert_eq!(
            recover_queue(&owner.queue).unwrap()[0]
                .directory_stream_report
                .uncertain
                .len(),
            1
        );
        let unrelated = stream_identity("/other");
        let unrelated_binding = crate::EndpointBinding::remote(&unrelated, [2; 32]).unwrap();
        assert!(namespace_staging_view(
            &config,
            &[(&unrelated_binding, Some(&unrelated), Path::new("/"))]
        )
        .unwrap()
        .candidates
        .is_empty());
        let empty =
            crate::metadata::MetadataManifest::new(None, 0o700, None, vec![], None, None, None)
                .unwrap()
                .with_named_streams(Some(vec![]))
                .unwrap();
        let base = config.join("folder");
        std::fs::create_dir(&base).unwrap();
        let observation = StreamObservation {
            local: crate::LocalVfs::rooted(&config).unwrap(),
            manifest: Some(empty),
            identity,
            stat_error: None,
        };
        assert!(view.candidates.iter().all(|candidate| candidate
            .object
            .with_base_path(base.clone())
            .is_absent(&observation)
            .unwrap()));
    }

    fn ftps_binding() -> super::FtpsIntentBinding {
        super::FtpsIntentBinding {
            roots: vec![crate::remote::FtpsRootIdentity {
                server: crate::remote::FtpsServerIdentity {
                    host: "127.0.0.1".into(),
                    port: 2121,
                    transport_security: crate::remote::FtpTransportSecurity::Plaintext,
                    certificate: None,
                    canonical_root: "/server".into(),
                    features_digest: [1; 32],
                    welcome_digest: [2; 32],
                    legacy_welcome_digest: [0; 32],
                },
                virtual_root: "/pane".into(),
                session_generation: 1,
            }],
            account_digest: [3; 32],
            phase: "stou".into(),
            stou_name_unknown: true,
            paths: vec!["/server/pane/target".into()],
        }
    }

    #[test]
    fn remote_intent_recovery_erhaelt_v1_und_bindet_unbekannten_stou_namen() {
        use super::*;
        let (_fixture, root) = sandbox();
        let config = RootHandle::open_nofollow(&root).unwrap();
        let queue = root.join("queue-a.json");
        let owner = StagingOwner {
            queue: queue.clone(),
            operation: "a".repeat(32),
        };
        let legacy = RemoteTempIntent::create(
            &owner,
            Path::new("/old/target"),
            Path::new("/old/candidate"),
        )
        .unwrap();
        let current = RemoteTempIntent::create_ftps(
            &owner,
            &config,
            Path::new("/server/pane/target"),
            ftps_binding(),
        )
        .unwrap();
        let recovered = recover_queue(&queue).unwrap();
        assert_eq!(recovered.len(), 2);
        assert!(recovered
            .iter()
            .any(|record| record.ftps_root.is_none()
                && record.paths.contains(&"/old/candidate".into())));
        let unknown = recovered
            .iter()
            .find(|record| record.ftps_root.is_some())
            .unwrap();
        assert!(unknown.paths.contains(&"/server/pane".into()));
        assert!(unknown
            .paths
            .contains(&current.path().to_string_lossy().into_owned()));
        assert!(recover_queue(&root.join("queue-b.json"))
            .unwrap()
            .is_empty());
        current
            .ftps_stou_name(Path::new("/server/pane/acknowledged"))
            .unwrap();
        assert!(!current.ftps_recovery().1);
        assert!(current
            .ftps_recovery()
            .0
            .contains(&"/server/pane/acknowledged".into()));
        let before = std::fs::read(current.path()).unwrap();
        assert!(current
            .ftps_stou_name(Path::new("/server/foreign"))
            .is_err());
        assert_eq!(std::fs::read(current.path()).unwrap(), before);
        current.finish().unwrap();
        legacy.finish().unwrap();
        assert!(recover_queue(&queue).unwrap().is_empty());
    }

    #[test]
    fn ftps_intent_recovery_verweigert_vermischte_versionen_und_symlink_index() {
        use super::*;
        let (_fixture, root) = sandbox();
        let config = RootHandle::open_nofollow(&root).unwrap();
        let queue = root.join("queue.json");
        let owner = StagingOwner {
            queue: queue.clone(),
            operation: "b".repeat(32),
        };
        let intent = RemoteTempIntent::create_ftps(
            &owner,
            &config,
            Path::new("/server/pane/target"),
            ftps_binding(),
        )
        .unwrap();
        let valid: serde_json::Value =
            serde_json::from_slice(&std::fs::read(intent.path()).unwrap()).unwrap();
        for (key, value) in [
            ("version", serde_json::json!(3)),
            ("remote", serde_json::json!(false)),
            ("candidate", serde_json::json!("/server/pane/candidate")),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            std::fs::write(intent.path(), serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(recover_queue(&queue).is_err(), "Ungültiger Vertrag: {key}");
        }
        std::fs::remove_file(intent.path()).unwrap();
        let foreign = root.join("foreign.json");
        std::fs::write(&foreign, serde_json::to_vec(&valid).unwrap()).unwrap();
        std::os::unix::fs::symlink(&foreign, intent.path()).unwrap();
        assert!(recover_queue(&queue).is_err());
        assert!(foreign.is_file());
        assert!(std::fs::symlink_metadata(intent.path())
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn nachbartempindex_bindet_fensterqueue_und_bleibt_nur_nutzerlesbar() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let fixture = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let config = root.join("config");
        let target = root.join("target");
        std::fs::create_dir(&config).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o755)).unwrap();
        let queue = config.join("queue-window-a.json");
        let stage = super::NeighborStaging::create(
            &super::StagingOwner {
                queue: queue.clone(),
                operation: "op-a".into(),
            },
            &target.join("Datei.txt"),
            None,
        )
        .unwrap()
        .unwrap();
        let temp = target.join(".Datei.txt.doppeldecker-tmp-own");
        stage.candidate(&temp).unwrap();
        assert_eq!(
            std::fs::metadata(stage.path()).unwrap().mode() & 0o777,
            0o600
        );
        assert!(super::recover_queue(&config.join("queue-window-b.json"))
            .unwrap()
            .is_empty());
        let recovery = super::recover_queue(&queue).unwrap();
        assert_eq!(recovery.len(), 1);
        assert_eq!(recovery[0].operation, "op-a");
        assert!(recovery[0]
            .paths
            .contains(&temp.to_string_lossy().into_owned()));
        // Abwesenheit ist nur nach echtem NotFound belegt; eine erhaltene Datei
        // darf der Indexabschluss nicht löschen.
        std::fs::write(&temp, b"erhalten").unwrap();
        assert!(stage.finish().is_err());
        assert_eq!(std::fs::read(&temp).unwrap(), b"erhalten");
        std::fs::remove_file(&temp).unwrap();
        stage.finish().unwrap();
        assert!(super::recover_queue(&queue).unwrap().is_empty());
    }

    #[test]
    fn c2_vorbereitungsfehler_erhaelt_pruefpfad_und_fehlercode() {
        let temp = tempfile::tempdir().unwrap();
        let stage = temp.path().join("private-stage");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join("lease"), b"eigener Beleg").unwrap();
        std::fs::create_dir(stage.join("journal.next")).unwrap();
        std::fs::write(stage.join("journal.next/datenrest"), b"erhalten").unwrap();
        // Der echte Journal-Schreibfehler wird nach schon erfolgreicher Leaseanlage
        // durch denselben Vorbereitungswrapper wie im Produktionsweg geführt.
        let error = prepare_with_recovery(&stage, || {
            std::fs::write(stage.join("journal.next"), b"Journal")
        })
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::Io);
        let path = stage.to_string_lossy().into_owned();
        assert!(error.to_string().contains(&path));
        assert_eq!(crate::error::recovery_paths(&error), vec![path.clone()]);
        assert_eq!(
            std::fs::read(stage.join("journal.next/datenrest")).unwrap(),
            b"erhalten"
        );
        let error = prepare_with_recovery::<()>(&stage, || {
            Err(crate::coded_error(
                io::ErrorKind::StorageFull,
                crate::ErrorCode::StorageFull,
                "injizierter Journal-Syncfehler",
            ))
        })
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::StorageFull);
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert_eq!(crate::error::recovery_paths(&error), vec![path]);
    }

    use super::*;
    use crate::vfs::{LocalVfs, Vfs};
    use std::os::unix::fs::PermissionsExt;

    fn sandbox() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::local_fs::write_acl_exact(&root, None).unwrap();
        std::fs::write(root.join(".doppeldecker-test-sandbox"), b"C2").unwrap();
        (temp, root)
    }

    #[test]
    fn staging_mit_ungeschuetztem_queueordner_bleibt_ohne_nebenwirkung() {
        let (_temp, root) = sandbox();
        let parent = root.join("ziel");
        let config = root.join("config");
        std::fs::create_dir(&parent).unwrap();
        std::fs::create_dir(&config).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o777)).unwrap();
        let vfs = LocalVfs::rooted(&root)
            .unwrap()
            .with_staging_owner(&config.join("operations-queue.json"), "op-test");
        assert!(vfs
            .staging_scope(&parent.join("neu"), Some(&root))
            .unwrap()
            .is_none());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 3);
        assert_eq!(std::fs::read_dir(config).unwrap().count(), 0);
    }

    #[test]
    fn staging_haelt_namen_bei_eltern_swap_und_entfernt_nur_eigenen_temp() {
        let (_temp, root) = sandbox();
        let parent = root.join("ziel");
        std::fs::create_dir(&parent).unwrap();
        let target = parent.join("fertig");
        let vfs = LocalVfs::rooted(&root).unwrap();
        let scoped = vfs.staging_scope(&target, Some(&root)).unwrap().unwrap();
        assert!(!scoped.supports_remove_if_matches());
        let candidate = scoped.staging_candidate(&target, "tmp").unwrap();
        scoped
            .open_write_new(&candidate)
            .unwrap()
            .write_all(b"eigen")
            .unwrap();
        let stage = candidate.parent().unwrap();
        let moved = root.join("verschobenes-stage");
        std::fs::rename(stage, &moved).unwrap();
        std::fs::create_dir(stage).unwrap();
        let sentinel = stage.join(candidate.file_name().unwrap());
        std::fs::write(&sentinel, b"fremd").unwrap();
        scoped.cleanup_owned_temp(&candidate).unwrap();
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"fremd");
        assert!(!moved.join(candidate.file_name().unwrap()).exists());
        let new_temp = scoped.staging_candidate(&target, "tmp").unwrap();
        scoped
            .open_write_new(&new_temp)
            .unwrap()
            .write_all(b"neu")
            .unwrap();
        std::fs::rename(&parent, root.join("gehaltenes-ziel")).unwrap();
        std::fs::create_dir(&parent).unwrap();
        let error = scoped.rename_noreplace(&new_temp, &target).unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert!(!target.exists());
        assert!(!root.join("gehaltenes-ziel/fertig").exists());
        scoped.cleanup_owned_temp(&new_temp).unwrap();
    }

    #[test]
    fn staging_vererbt_wie_direkte_kernelanlage() {
        let (_temp, root) = sandbox();
        let parent = root.join("ziel");
        std::fs::create_dir(&parent).unwrap();
        let target = parent.join("neu");
        #[cfg(target_os = "macos")]
        let acl = Acl::Nfs4(crate::metadata::Nfs4Acl {
            entries: vec![crate::metadata::Nfs4AclEntry {
                qualifier: [0; 16],
                kind: crate::metadata::Nfs4AceKind::Allow,
                permissions: 2,
                flags: 32 | 64,
            }],
            flags: 0,
        });
        #[cfg(target_os = "linux")]
        let acl = {
            use crate::metadata::{PosixAclEntry, PosixAclTag::*};
            let entry = |tag, id, permissions| PosixAclEntry {
                tag,
                id,
                permissions,
            };
            Acl::Posix(PosixAcl {
                access: vec![],
                default: vec![
                    entry(UserObject, None, 7),
                    entry(User, Some(65534), 7),
                    entry(GroupObject, None, 7),
                    entry(Mask, None, 5),
                    entry(Other, None, 0),
                ],
            })
        };
        crate::local_fs::write_acl_exact(&parent, Some(&acl)).unwrap();
        std::fs::File::create(parent.join("direkt")).unwrap();
        let vfs = LocalVfs::rooted(&root).unwrap();
        let scoped = vfs.staging_scope(&target, Some(&root)).unwrap().unwrap();
        let candidate = scoped.staging_candidate(&target, "tmp").unwrap();
        scoped.open_write_new(&candidate).unwrap();
        let direct = vfs.metadata_manifest(&parent.join("direkt")).unwrap();
        let staged = scoped.metadata_manifest(&candidate).unwrap();
        assert_eq!(
            (staged.acl, staged.mode, staged.owner),
            (direct.acl, direct.mode, direct.owner)
        );
        scoped.cleanup_owned_temp(&candidate).unwrap();
    }

    #[test]
    fn c2_kopiert_beide_typkonflikte_mit_getrennter_erfolgsbilanz() {
        use crate::fileops::{FileOperationRequest, OverwritePolicy};
        for source_directory in [false, true] {
            let (_temp, root) = sandbox();
            let source = root.join("quelle/eintrag");
            let destination = root.join("ziel");
            std::fs::create_dir(root.join("quelle")).unwrap();
            std::fs::create_dir(&destination).unwrap();
            let target = destination.join("eintrag");
            if source_directory {
                std::fs::create_dir(&source).unwrap();
                std::fs::write(source.join("neu.txt"), b"neu").unwrap();
                std::fs::write(&target, b"alt").unwrap();
            } else {
                std::fs::write(&source, b"neu").unwrap();
                std::fs::create_dir(&target).unwrap();
                std::fs::write(target.join("alt.txt"), b"alt").unwrap();
            }
            let vfs = LocalVfs::rooted(&root).unwrap();
            let stats = crate::fileops::copy_items(
                FileOperationRequest {
                    src: &vfs,
                    dst: &vfs,
                    items: std::slice::from_ref(&source),
                    dst_dir: &destination,
                    policy: OverwritePolicy::Overwrite,
                    sandbox: &Some(root.clone()),
                },
                &crate::Cancel::new(),
                &mut |_| {},
            )
            .unwrap();
            assert_eq!((stats.files, stats.bytes, stats.cancelled), (1, 3, false));
            if source_directory {
                assert_eq!(std::fs::read(target.join("neu.txt")).unwrap(), b"neu");
                assert!(stats.retained_backup_paths.is_empty());
                assert!(stats.temp_recovery_paths.is_empty());
                assert!(!std::fs::read_dir(&root).unwrap().any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".stage.doppeldecker-stage-")));
            } else {
                assert_eq!(std::fs::read(&target).unwrap(), b"neu");
                assert_eq!(stats.retained_backup_paths.len(), 1);
                assert_eq!(stats.temp_recovery_paths, stats.retained_backup_paths);
                assert_eq!(
                    std::fs::read(Path::new(&stats.retained_backup_paths[0]).join("alt.txt"))
                        .unwrap(),
                    b"alt"
                );
            }
            assert!(!vfs.supports_remove_if_matches());
        }
    }

    #[test]
    fn c2_abbruch_entfernt_private_temps_und_behaelt_altziel() {
        use crate::fileops::{FileOperationRequest, OverwritePolicy};
        let (_temp, root) = sandbox();
        let source = root.join("quelle/eintrag");
        let destination = root.join("ziel");
        std::fs::create_dir(root.join("quelle")).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(&source, vec![7; 256 * 1024]).unwrap();
        std::fs::write(destination.join("eintrag"), b"alt").unwrap();
        let vfs = LocalVfs::rooted(&root).unwrap();
        let cancel = crate::Cancel::new();
        let stats = crate::fileops::copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &Some(root.clone()),
            },
            &cancel,
            &mut |progress| {
                if progress.bytes_done > 0 {
                    cancel.cancel();
                }
            },
        )
        .unwrap();
        assert!(stats.cancelled);
        assert_eq!(stats.files, 0);
        assert!(stats.temp_recovery_paths.is_empty());
        assert_eq!(std::fs::read(destination.join("eintrag")).unwrap(), b"alt");
        assert!(!std::fs::read_dir(&root).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".stage.doppeldecker-stage-")));
    }

    #[test]
    fn c2_vorbereitung_folgt_keinem_getauschten_eltern_symlink() {
        let (_temp, root) = sandbox();
        let (_outside_temp, outside) = sandbox();
        std::os::unix::fs::symlink(&outside, root.join("ziel")).unwrap();
        let before = std::fs::read_dir(&outside).unwrap().count();
        assert!(LocalVfs::new()
            .staging_scope(&root.join("ziel/datei"), None)
            .is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), before);
    }

    #[test]
    fn c2_acl_drift_stoppt_vor_dem_temp_commit() {
        let (_temp, root) = sandbox();
        std::fs::create_dir(root.join("ziel")).unwrap();
        let target = root.join("ziel/datei");
        let vfs = LocalVfs::rooted(&root).unwrap();
        let scoped = vfs.staging_scope(&target, Some(&root)).unwrap().unwrap();
        let candidate = scoped.staging_candidate(&target, "tmp").unwrap();
        scoped
            .open_write_new(&candidate)
            .unwrap()
            .write_all(b"neu")
            .unwrap();
        std::fs::set_permissions(root.join("ziel"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let error = scoped.rename_noreplace(&candidate, &target).unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert!(!target.exists());
        scoped.cleanup_owned_temp(&candidate).unwrap();
        let error = scoped.finish_staging().unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
    }

    #[test]
    fn c2_spaeter_eltern_swap_wird_nicht_als_erfolg_bilanziert() {
        let (_temp, root) = sandbox();
        let source = root.join("quelle/datei");
        let parent = root.join("ziel");
        std::fs::create_dir(root.join("quelle")).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(&source, b"neu").unwrap();
        let vfs = LocalVfs::rooted(&root).unwrap();
        let moved = root.join("verschobenes-ziel");
        let mut swapped = false;
        let error = crate::fileops::copy_items(
            crate::FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &[source],
                dst_dir: &parent,
                policy: crate::OverwritePolicy::Overwrite,
                sandbox: &Some(root.clone()),
            },
            &crate::Cancel::new(),
            &mut |progress| {
                if progress.files_done == 1 && !swapped {
                    std::fs::rename(&parent, &moved).unwrap();
                    std::fs::create_dir(&parent).unwrap();
                    std::fs::write(parent.join("sentinel"), b"fremd").unwrap();
                    swapped = true;
                }
            },
        )
        .unwrap_err();
        assert!(swapped);
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(parent.join("sentinel")).unwrap(), b"fremd");
        assert_eq!(std::fs::read(moved.join("datei")).unwrap(), b"neu");
        assert!(!parent.join("datei").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn c2_macos_acl_matrix_entspricht_kernelanlage() {
        for flags in 0..16 {
            for kind in [
                crate::metadata::Nfs4AceKind::Allow,
                crate::metadata::Nfs4AceKind::Deny,
            ] {
                let (_temp, root) = sandbox();
                let parent = root.join("ziel");
                std::fs::create_dir(&parent).unwrap();
                let acl = Acl::Nfs4(crate::metadata::Nfs4Acl {
                    entries: vec![crate::metadata::Nfs4AclEntry {
                        qualifier: [0; 16],
                        kind,
                        permissions: 2,
                        flags: flags << 5,
                    }],
                    flags: 0,
                });
                crate::local_fs::write_acl_exact(&parent, Some(&acl)).unwrap();
                let vfs = LocalVfs::rooted(&root).unwrap();
                let target = parent.join("datei");
                std::fs::File::create(parent.join("direkt")).unwrap();
                let scoped = vfs.staging_scope(&target, Some(&root)).unwrap().unwrap();
                let candidate = scoped.staging_candidate(&target, "tmp").unwrap();
                scoped.open_write_new(&candidate).unwrap();
                let direct = vfs.metadata_manifest(&parent.join("direkt")).unwrap();
                let staged = scoped.metadata_manifest(&candidate).unwrap();
                assert_eq!(
                    (staged.acl, staged.mode, staged.owner),
                    (direct.acl, direct.mode, direct.owner),
                    "flags={flags}, kind={kind:?}"
                );
            }
        }
    }

    #[test]
    fn c2_recovery_schliesst_aktive_fenster_aus_und_erhaelt_cleanup_luecken() {
        let (_temp, root) = sandbox();
        std::fs::create_dir(root.join("config")).unwrap();
        std::fs::set_permissions(root.join("config"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::create_dir(root.join("ziel")).unwrap();
        let queue = root.join("config/operations-queue-eigen.json");
        let owner = StagingOwner {
            queue: queue.clone(),
            operation: "eigen".into(),
        };
        let handle = RootHandle::open(&root).unwrap();
        let stage = LocalStaging::create(
            Some(&handle),
            Some(&handle),
            &root.join("ziel/datei"),
            Some(&root),
            Some(&owner),
            None,
        )
        .unwrap()
        .unwrap();
        let stage_path = stage.directory.path().to_path_buf();
        assert!(recover_queue(&queue).unwrap().is_empty());
        assert!(
            recover_queue(&root.join("config/operations-queue-anderes.json"))
                .unwrap()
                .is_empty()
        );
        stage.record("complete", &stage_path, None).unwrap();
        drop(stage);
        // Echter Zwischenzustand der abschließenden Bereinigung: Journal schon
        // entfernt, Lease noch vorhanden; anschließend auch die Lease entfernt.
        std::fs::remove_file(stage_path.join("journal.json")).unwrap();
        for remove_lease in [false, true] {
            if remove_lease {
                std::fs::remove_file(stage_path.join("lease")).unwrap();
            }
            let recovery = recover_queue(&queue).unwrap();
            assert_eq!(recovery.len(), 1);
            assert_eq!(
                recovery[0].paths,
                vec![stage_path.to_string_lossy().into_owned()]
            );
            assert!(stage_path.is_dir());
        }
    }

    #[test]
    fn c2_crash_child() {
        let Ok(root) = std::env::var("DOPPELDECKER_C2_CRASH_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        assert_eq!(
            std::fs::read(root.join(".doppeldecker-test-sandbox")).unwrap(),
            b"C2"
        );
        let operation = std::env::var("DOPPELDECKER_C2_CRASH_OPERATION").unwrap();
        let phase = std::env::var("DOPPELDECKER_C2_CRASH_PHASE").unwrap();
        let queue = root.join("config/operations-queue-crash.json");
        let target = root.join("ziel/eintrag");
        let vfs = LocalVfs::rooted(&root)
            .unwrap()
            .with_staging_owner(&queue, &operation);
        let scoped = vfs.staging_scope(&target, Some(&root)).unwrap().unwrap();
        let temp = scoped.staging_candidate(&target, "tmp").unwrap();
        scoped
            .open_write_new(&temp)
            .unwrap()
            .write_all(b"neu")
            .unwrap();
        scoped.sync_file(&temp).unwrap();
        if phase != "temp" {
            let backup = scoped.staging_candidate(&target, "replaced").unwrap();
            scoped.rename_noreplace(&target, &backup).unwrap();
            scoped.sync_dir(target.parent().unwrap()).unwrap();
            scoped.sync_dir(backup.parent().unwrap()).unwrap();
            if phase == "commit" {
                scoped.rename_noreplace(&temp, &target).unwrap();
                scoped.sync_file(&target).unwrap();
                scoped.sync_dir(target.parent().unwrap()).unwrap();
            }
        }
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGKILL) }, 0);
        loop {
            // SIGKILL wird asynchron zugestellt; keine reguläre Destruktorphase.
            unsafe {
                libc::pause();
            }
        }
    }

    #[test]
    fn c2_crash_recovery_bewahrt_daten_und_bindet_die_fensterqueue_frisch() {
        use std::os::unix::process::ExitStatusExt;
        for phase in ["temp", "park", "commit"] {
            let (_temp, root) = sandbox();
            std::fs::create_dir(root.join("config")).unwrap();
            std::fs::set_permissions(root.join("config"), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            std::fs::create_dir(root.join("ziel")).unwrap();
            std::fs::create_dir(root.join("ziel/eintrag")).unwrap();
            std::fs::write(root.join("ziel/eintrag/alt.txt"), b"alt").unwrap();
            let queue_path = root.join("config/operations-queue-crash.json");
            let queue = crate::OperationQueue::open(&queue_path).unwrap();
            let entry = queue
                .enqueue(crate::QueueOperation::File {
                    kind: crate::FileQueueKind::Copy,
                    items: vec![root.join("quelle").to_string_lossy().into_owned()],
                    destination: root.join("ziel").to_string_lossy().into_owned(),
                    overwrite: true,
                    expected_conflicts: vec![],
                    keep_both_targets: Vec::new(),
                    expected_conflict_proofs: vec![],
                    local_replace_best_effort_expected: Some(false),
                    local_copy_new_best_effort_expected: None,
                })
                .unwrap();
            queue.claim_next().unwrap().unwrap();
            queue.mark_running(&entry.operation_id).unwrap();
            drop(queue);
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "local_staging::tests::c2_crash_child",
                    "--nocapture",
                ])
                .env("DOPPELDECKER_C2_CRASH_ROOT", &root)
                .env("DOPPELDECKER_C2_CRASH_OPERATION", &entry.operation_id)
                .env("DOPPELDECKER_C2_CRASH_PHASE", phase)
                .status()
                .unwrap();
            assert_eq!(status.signal(), Some(libc::SIGKILL));
            let reopened = crate::OperationQueue::open(&queue_path).unwrap();
            let recovered = &reopened.snapshot().entries[0];
            assert_eq!(recovered.state, crate::QueueState::Interrupted);
            assert_eq!(recovered.error_code, Some(crate::ErrorCode::OutcomeUnclear));
            assert_eq!(
                recovered.temp_recovery_paths.len(),
                if phase == "park" { 2 } else { 1 }
            );
            if phase == "temp" {
                assert_eq!(
                    std::fs::read(root.join("ziel/eintrag/alt.txt")).unwrap(),
                    b"alt"
                );
            } else {
                let backup = recovered
                    .temp_recovery_paths
                    .iter()
                    .find(|path| path.contains("doppeldecker-replaced"))
                    .unwrap();
                assert_eq!(
                    std::fs::read(Path::new(backup).join("alt.txt")).unwrap(),
                    b"alt"
                );
                if phase == "park" {
                    assert!(!root.join("ziel/eintrag").exists());
                } else {
                    assert_eq!(std::fs::read(root.join("ziel/eintrag")).unwrap(), b"neu");
                }
            }
        }
    }
    fn append_partial_event(
        config: &Path,
        record: &NeighborRecord,
        phase: &str,
        length: usize,
    ) -> Vec<u8> {
        let data = serde_json::to_vec(&(phase, record)).unwrap();
        let tail = data[..length.min(data.len())].to_vec();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(config.join(INTENT_EVENTS))
            .unwrap();
        file.write_all(&tail).unwrap();
        file.sync_all().unwrap();
        tail
    }

    #[test]
    fn namespace_registry_partial_append_invalidates_permit_but_allows_fresh_independent_sync() {
        for tail_length in [1, 180, usize::MAX] {
            let area = tempfile::tempdir().unwrap();
            let root = area.path().canonicalize().unwrap();
            let source = root.join("source");
            let target = root.join("target");
            let affected = root.join("affected");
            for path in [&source, &target, &affected] {
                std::fs::create_dir(path).unwrap();
            }
            std::fs::write(source.join("file"), b"source").unwrap();
            let queue_path = root.join("config/operations-queue.json");
            let queue = crate::OperationQueue::open(&queue_path).unwrap();
            let (owner, _, _) = queue.ftps_owner(None).unwrap();
            let neighbor = NeighborStaging::create(&owner, &affected.join("file"), None)
                .unwrap()
                .unwrap();
            let prepare = |queue: &crate::OperationQueue| {
                crate::prepare_local_sync_run_for_queue(
                    queue,
                    &source,
                    &target,
                    crate::SyncOptions::default(),
                    None,
                )
            };
            let prepared = prepare(&queue).unwrap();
            let permitted = prepare(&queue).unwrap().confirm(true).unwrap();
            let record = neighbor.record.lock().unwrap().clone();
            let tail =
                append_partial_event(queue_path.parent().unwrap(), &record, "finish", tail_length);
            let fresh = prepare(&queue)
                .expect("Frischer unabhängiger Sync bleibt nach Tail-Recovery möglich");
            assert_eq!(
                crate::error_code(&prepared.confirm(true).err().unwrap()),
                crate::ErrorCode::PlanDrift
            );
            let error = permitted
                .execute(&crate::Cancel::new(), &mut |_| {})
                .unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
            assert!(!target.join("file").exists());
            fresh
                .confirm(true)
                .unwrap()
                .execute(&crate::Cancel::new(), &mut |_| {})
                .unwrap();
            assert_eq!(std::fs::read(target.join("file")).unwrap(), b"source");
            // Auch nach bewusstem Abschluss und Entfernung des normalen Index
            // bleibt die beim Teilwrite unklare Wirkung erhalten.
            neighbor.finish().unwrap();
            let binding = crate::EndpointBinding::local(
                &crate::Vfs::root_identity(&crate::LocalVfs::rooted(&affected).unwrap(), &affected)
                    .unwrap(),
            )
            .unwrap();
            let view = namespace_staging_view(
                queue_path.parent().unwrap(),
                &[(&binding, None, &affected)],
            )
            .unwrap();
            assert!(view.candidates.iter().any(|candidate| candidate.unknown));
            let bytes = std::fs::read(queue_path.parent().unwrap().join(INTENT_EVENTS)).unwrap();
            assert!(bytes.ends_with(b"\n"));
            let events = bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert!(events
                .iter()
                .any(|event| event.get("interrupted_tail") == Some(&serde_json::json!(tail))));
            drop(neighbor);
            drop(queue);
            let reopened = crate::OperationQueue::open(&queue_path).unwrap();
            prepare(&reopened).unwrap();
        }
    }

    #[test]
    fn namespace_registry_recovery_keeps_corruption_and_orphaned_tails_fail_closed() {
        for (tail, keep_index) in [
            (b"[not-json".as_slice(), true),
            (b"[\"foreign\",".as_slice(), true),
            (b"invalid\n".as_slice(), true),
            (b"[".as_slice(), false),
        ] {
            let area = tempfile::tempdir().unwrap();
            let root = area.path().canonicalize().unwrap();
            let queue =
                crate::OperationQueue::open(root.join("config/operations-queue.json")).unwrap();
            let (owner, _, _) = queue.ftps_owner(None).unwrap();
            let neighbor = NeighborStaging::create(&owner, &root.join("file"), None)
                .unwrap()
                .unwrap();
            let config = owner.queue.parent().unwrap();
            if !keep_index {
                std::fs::remove_file(neighbor.path()).unwrap();
            }
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(config.join(INTENT_EVENTS))
                .unwrap();
            file.write_all(tail).unwrap();
            file.sync_all().unwrap();
            let original = std::fs::read(config.join(INTENT_EVENTS)).unwrap();
            let error = namespace_staging_view(config, &[]).err().unwrap();
            if !keep_index {
                assert!(crate::error::recovery_paths(&error).iter().any(|path| {
                    Path::new(path).file_name().unwrap().to_string_lossy().contains("doppeldecker-repair-")
                        && Path::new(path).is_file()
                }), "Erhaltener privater Reparaturtemp muss als konkreter Recoverypfad sichtbar sein");
            }
            assert_eq!(std::fs::read(config.join(INTENT_EVENTS)).unwrap(), original);
            let held = RootHandle::open_nofollow(config).unwrap();
            let _lock = lock_intent_events(&held).unwrap();
            assert!(
                append_intent_event_locked(&held, &neighbor.record.lock().unwrap(), "finish")
                    .is_err(),
                "Auch der Writer darf Korruption nicht überkleben"
            );
            assert_eq!(std::fs::read(config.join(INTENT_EVENTS)).unwrap(), original);
        }
    }

    #[test]
    fn namespace_registry_tail_repair_preserves_a_streamed_history_byte_for_byte() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let queue = crate::OperationQueue::open(root.join("config/operations-queue.json")).unwrap();
        let (owner, _, _) = queue.ftps_owner(None).unwrap();
        let config = owner.queue.parent().unwrap();
        let neighbor = NeighborStaging::create(&owner, &root.join("file"), None)
            .unwrap()
            .unwrap();
        let record = neighbor.record.lock().unwrap().clone();
        let journal = config.join(INTENT_EVENTS);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap();
        let mut historical = record.clone();
        for index in 0..4096 {
            historical.owner.operation = format!("historical-{index}");
            serde_json::to_writer(&mut file, &("neighbor", &historical)).unwrap();
            file.write_all(b"\n").unwrap();
        }
        file.sync_all().unwrap();
        let prefix = std::fs::read(&journal).unwrap();
        assert!(prefix.len() > RECOVERY_EVENT_LIMIT);
        let tail = append_partial_event(config, &record, "finish", 1);
        namespace_staging_view(config, &[]).unwrap();
        let repaired = std::fs::read(&journal).unwrap();
        assert_eq!(&repaired[..prefix.len()], prefix);
        assert!(repaired.ends_with(b"\n"));
        assert!(repaired[prefix.len()..].split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).any(|line| {
            matches!(decode_intent_event(line).unwrap(), IntentEvent::Recovery { interrupted_tail } if interrupted_tail == tail)
        }));
    }

    #[test]
    fn namespace_registry_initial_index_and_event_keep_the_recovery_lock_together() {
        for remote in [false, true] {
            let area = tempfile::tempdir().unwrap();
            let root = area.path().canonicalize().unwrap();
            let affected = root.join("affected");
            let independent = root.join("independent");
            std::fs::create_dir(&affected).unwrap();
            std::fs::create_dir(&independent).unwrap();
            let queue =
                crate::OperationQueue::open(root.join("config/operations-queue.json")).unwrap();
            let (old_owner, _, _) = queue.ftps_owner(None).unwrap();
            let config = old_owner.queue.parent().unwrap().to_path_buf();
            let old = NeighborStaging::create(&old_owner, &affected.join("file"), None)
                .unwrap()
                .unwrap();
            let old_record = old.record.lock().unwrap().clone();
            let new_owner = StagingOwner {
                queue: config.join("operations-queue-window-b.json"),
                operation: "new-independent".into(),
            };
            let expected_owner = new_owner.clone();
            let (checked_send, checked_receive) = std::sync::mpsc::channel();
            let (continue_send, continue_receive) = std::sync::mpsc::channel();
            let target = independent.join("file");
            let constructor = std::thread::spawn(move || -> Box<dyn FnOnce() + Send> {
                AFTER_INITIAL_INTENT_REGISTRY_CHECK.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        checked_send.send(()).unwrap();
                        continue_receive.recv().unwrap();
                    }));
                });
                if remote {
                    let intent = RemoteTempIntent::create(
                        &new_owner,
                        &target,
                        &target.with_file_name(".partial"),
                    )
                    .unwrap();
                    Box::new(move || intent.finish().unwrap())
                } else {
                    let intent = NeighborStaging::create(&new_owner, &target, None)
                        .unwrap()
                        .unwrap();
                    Box::new(move || intent.finish().unwrap())
                }
            });
            checked_receive
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let (attempt_send, attempt_receive) = std::sync::mpsc::channel();
            let (finished_send, finished_receive) = std::sync::mpsc::channel();
            let writer_config = config.clone();
            let competitor = std::thread::spawn(move || {
                let held = RootHandle::open_nofollow(&writer_config).unwrap();
                let lock = crate::local_fs::open_read_beneath(
                    &held,
                    &writer_config.join(INTENT_EVENTS_LOCK),
                )
                .unwrap();
                let blocked = match rustix::fs::flock(
                    &lock,
                    rustix::fs::FlockOperation::NonBlockingLockExclusive,
                ) {
                    Ok(()) => false,
                    Err(error) if error == rustix::io::Errno::WOULDBLOCK => true,
                    Err(error) => panic!("Unerwarteter Lockfehler: {error}"),
                };
                if blocked {
                    attempt_send.send(true).unwrap();
                    // Der Konstruktor hat die Sperre gehalten. Sein unabhängiger
                    // Intent wird zuerst sauber abgeschlossen; danach darf der
                    // konkurrierende alte Owner den echten Teilwrite hinterlassen.
                    finished_receive.recv().unwrap();
                    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
                    append_partial_event(&writer_config, &old_record, "finish", 1);
                } else {
                    // Regression: Der Teilwrite liegt zwischen sauberer erster
                    // Prüfung und neuer Indexanlage. Der neue Owner darf dadurch
                    // niemals rückwirkend zum unklaren alten Owner werden.
                    append_partial_event(&writer_config, &old_record, "finish", 1);
                    attempt_send.send(false).unwrap();
                }
            });
            let blocked = attempt_receive
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            continue_send.send(()).unwrap();
            constructor.join().unwrap()();
            if blocked {
                finished_send.send(()).unwrap();
            }
            competitor.join().unwrap();
            namespace_staging_view(&config, &[]).unwrap();
            let events = std::fs::read(config.join(INTENT_EVENTS)).unwrap();
            for line in events
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
            {
                if let IntentEvent::Record((phase, record)) = decode_intent_event(line).unwrap() {
                    assert!(
                        !(phase == "interrupted" && record.owner == expected_owner),
                        "Neuer unabhängiger Owner darf nicht als alter Teilwrite markiert werden"
                    );
                }
            }
            assert!(blocked, "Initiale Tailprüfung, Indexanlage und Startevent benötigen dieselbe gehaltene Sperre");
        }
    }

    #[test]
    fn namespace_registry_writer_recovery_and_parallel_windows_share_stable_lock() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let affected = root.join("affected");
        let independent = root.join("independent");
        std::fs::create_dir(&affected).unwrap();
        std::fs::create_dir(&independent).unwrap();
        let queue = crate::OperationQueue::open(root.join("config/operations-queue.json")).unwrap();
        let (owner, _, _) = queue.ftps_owner(None).unwrap();
        let config = owner.queue.parent().unwrap().to_path_buf();
        let crashed = NeighborStaging::create(&owner, &affected.join("file"), None)
            .unwrap()
            .unwrap();
        append_partial_event(&config, &crashed.record.lock().unwrap(), "finish", 1);
        drop(crashed);
        // Der Writer muss vor seinem ersten neuen Index reparieren: Der neue
        // unabhängige Owner kann nicht Urheber eines älteren Teilwrites sein.
        let other = StagingOwner {
            queue: config.join("operations-queue-window-b.json"),
            operation: "b".repeat(32),
        };
        let new = NeighborStaging::create(&other, &independent.join("file"), None)
            .unwrap()
            .unwrap();
        new.finish().unwrap();
        let binding = crate::EndpointBinding::local(
            &crate::Vfs::root_identity(
                &crate::LocalVfs::rooted(&independent).unwrap(),
                &independent,
            )
            .unwrap(),
        )
        .unwrap();
        let before = namespace_staging_view(&config, &[(&binding, None, &independent)])
            .unwrap()
            .epoch;
        std::thread::scope(|scope| {
            for window in 0..2 {
                let config = &config;
                let independent = &independent;
                scope.spawn(move || {
                    let owner = StagingOwner {
                        queue: config.join(format!("operations-queue-window-{window}.json")),
                        operation: format!("window-{window}"),
                    };
                    for _ in 0..12 {
                        let intent =
                            NeighborStaging::create(&owner, &independent.join("file"), None)
                                .unwrap()
                                .unwrap();
                        intent.finish().unwrap();
                    }
                });
            }
            for _ in 0..24 {
                namespace_staging_view(&config, &[(&binding, None, &independent)]).unwrap();
            }
        });
        let after = namespace_staging_view(&config, &[(&binding, None, &independent)]).unwrap();
        assert!(after.candidates.is_empty());
        assert_ne!(
            before, after.epoch,
            "Start/End-ABA bleibt auch nach Tail-Recovery sichtbar"
        );
        for line in std::fs::read(config.join(INTENT_EVENTS))
            .unwrap()
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            serde_json::from_slice::<IntentEvent>(line).unwrap();
        }
    }

    #[test]
    fn namespace_registry_interrupted_remote_update_blocks_whole_bound_root_and_own_owner() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let queue = crate::OperationQueue::open(root.join("config/operations-queue.json")).unwrap();
        let (owner, _, held) = queue.ftps_owner(None).unwrap();
        let mut original = ftps_binding();
        original.stou_name_unknown = false;
        original.phase = "probe".into();
        let intent = RemoteTempIntent::create_ftps(
            &owner,
            &held,
            Path::new("/server/pane/target"),
            original.clone(),
        )
        .unwrap();
        let mut update = intent.record.lock().unwrap().clone();
        update.ftps.as_mut().unwrap().stou_name_unknown = true;
        update.ftps.as_mut().unwrap().phase = "stou".into();
        update
            .ftps
            .as_mut()
            .unwrap()
            .paths
            .push("/server/pane/subdir/new-unknown".into());
        // save() schreibt das Updateereignis vor dem neuen Index. Bei Abbruch
        // enthält der erhaltene Index weder STOU-Phase noch neuen Servernamen.
        append_partial_event(held.path(), &update, "update", 1);
        let mut nested = original.roots[0].clone();
        nested.server.canonical_root = "/server/pane/subdir".into();
        nested.server.features_digest = [9; 32];
        nested.virtual_root = "/".into();
        let nested = crate::RootIdentity::Ftps(nested);
        let binding = crate::EndpointBinding::remote(&nested, original.account_digest).unwrap();
        let view = namespace_staging_view_excluding_both(
            held.path(),
            &[(&binding, Some(&nested), Path::new("/"))],
            Some(&owner),
        )
        .unwrap();
        assert!(
            view.candidates.iter().any(|candidate| candidate.unknown),
            "Auch bisher nicht im Index genannter Unterordner und eigener Owner bleiben unklar"
        );
        let mut other = original.roots[0].clone();
        other.virtual_root = "/independent".into();
        let other = crate::RootIdentity::Ftps(other);
        let other_binding =
            crate::EndpointBinding::remote(&other, original.account_digest).unwrap();
        assert!(namespace_staging_view(
            held.path(),
            &[(&other_binding, Some(&other), Path::new("/independent"))]
        )
        .unwrap()
        .candidates
        .is_empty());
        let other_account = crate::EndpointBinding::remote(&nested, [8; 32]).unwrap();
        assert!(namespace_staging_view(
            held.path(),
            &[(&other_account, Some(&nested), Path::new("/"))]
        )
        .unwrap()
        .candidates
        .is_empty());
    }

    #[test]
    fn namespace_registry_binds_other_owners_active_leases_and_aba_without_global_block() {
        let area = ::tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap();
        let config = &area.path().canonicalize().unwrap();
        let owner = StagingOwner {
            queue: config.join("operations-queue-window-b.json"),
            operation: "b".repeat(32),
        };
        let identity = |port| {
            crate::RootIdentity::Sftp(crate::remote::SftpRootIdentity {
                server: crate::remote::SftpServerIdentity {
                    host: "server".into(),
                    port,
                    host_key: crate::SftpHostKeyPin {
                        algorithm: "ssh-ed25519".into(),
                        sha256: [8; 32],
                    },
                    canonical_root: "/vault".into(),
                    sftp_version: 3,
                    extensions_digest: [5; 32],
                },
                virtual_root: "/pane".into(),
                session_generation: 1,
            })
        };
        let identity_a = identity(22);
        let identity_b = identity(23);
        let binding = crate::EndpointBinding::remote(&identity_a, [2; 32]).unwrap();
        let other = crate::EndpointBinding::remote(&identity_b, [2; 32]).unwrap();
        let scopes = [(&binding, Some(&identity_a), Path::new("/pane"))];
        let before = namespace_staging_view(config, &scopes).unwrap();
        let unrelated_before =
            namespace_staging_view(config, &[(&other, Some(&identity_b), Path::new("/pane"))])
                .unwrap()
                .epoch;
        let intent = RemoteTempIntent::create_bound_with_root(
            &owner,
            Path::new("/pane/file"),
            Path::new("/pane/.partial"),
            &identity_a,
            [2; 32],
        )
        .unwrap();
        let active = namespace_staging_view(config, &scopes).unwrap();
        assert_eq!(active.candidates.len(), 1);
        assert!(
            active.candidates[0].active,
            "Aktive Lease sperrt auch vor sichtbarer Remoteanlage"
        );
        assert!(
            namespace_staging_view(config, &[(&other, Some(&identity_b), Path::new("/pane"))])
                .unwrap()
                .candidates
                .is_empty()
        );
        assert_eq!(
            namespace_staging_view(config, &[(&other, Some(&identity_b), Path::new("/pane"))])
                .unwrap()
                .epoch,
            unrelated_before
        );
        let mut independent = identity_a.clone();
        if let crate::RootIdentity::Sftp(value) = &mut independent {
            value.virtual_root = "/independent".into();
        }
        let independent_binding = crate::EndpointBinding::remote(&independent, [2; 32]).unwrap();
        assert!(namespace_staging_view(
            config,
            &[(
                &independent_binding,
                Some(&independent),
                Path::new("/independent")
            )]
        )
        .unwrap()
        .candidates
        .is_empty());
        intent.finish().unwrap();
        drop(intent);
        let after = namespace_staging_view(config, &scopes).unwrap();
        assert!(before.candidates.is_empty() && after.candidates.is_empty());
        assert_ne!(
            before.epoch, after.epoch,
            "Vollständiges Start/End-ABA muss einen laufenden Scan entwerten"
        );
    }

    #[test]
    fn namespace_registry_keeps_crashed_intent_and_events_across_capability_changes() {
        let area = ::tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap();
        let config = area.path().canonicalize().unwrap();
        let owner = StagingOwner {
            queue: config.join("operations-queue-window-b.json"),
            operation: "b".repeat(32),
        };
        let old = crate::RootIdentity::Sftp(crate::remote::SftpRootIdentity {
            server: crate::remote::SftpServerIdentity {
                host: "server".into(),
                port: 22,
                host_key: crate::SftpHostKeyPin {
                    algorithm: "ssh-ed25519".into(),
                    sha256: [8; 32],
                },
                canonical_root: "/vault".into(),
                sftp_version: 3,
                extensions_digest: [5; 32],
            },
            virtual_root: "/pane".into(),
            session_generation: 1,
        });
        let mut fresh = old.clone();
        if let crate::RootIdentity::Sftp(value) = &mut fresh {
            value.server.extensions_digest = [6; 32];
        }
        let binding = crate::EndpointBinding::remote(&fresh, [2; 32]).unwrap();
        let scopes = [(&binding, Some(&fresh), Path::new("/pane"))];
        let before = namespace_staging_view(&config, &scopes).unwrap().epoch;
        let intent = RemoteTempIntent::create_bound_with_root(
            &owner,
            Path::new("/pane/file"),
            Path::new("/pane/.partial"),
            &old,
            [2; 32],
        )
        .unwrap();
        drop(intent);
        let crashed = namespace_staging_view(&config, &scopes).unwrap();
        assert_eq!(crashed.candidates.len(), 1);
        assert_eq!(
            crashed.candidates[0].object,
            StagingObject::Path("/pane/.partial".into())
        );
        assert!(!crashed.candidates[0].active);
        assert_ne!(before, crashed.epoch);
        // Selbst nach bewusstem Entfernen nur des internen Index bleibt das
        // bereits persistierte Start-Ereignis gegenüber dem ersten Scan relevant.
        let index = std::fs::read_dir(&config)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with(".recovery.json")
            })
            .unwrap();
        std::fs::remove_file(index).unwrap();
        let events_only = namespace_staging_view(&config, &scopes).unwrap();
        assert!(events_only.candidates.is_empty());
        assert_ne!(before, events_only.epoch);
    }

    #[test]
    fn namespace_registry_keeps_other_local_neighbor_out_of_forward_roots() {
        let area = ::tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap();
        let root = area.path().canonicalize().unwrap();
        let config = root.join("config");
        let data = root.join("data");
        std::fs::create_dir(&config).unwrap();
        std::fs::create_dir(&data).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = StagingOwner {
            queue: config.join("operations-queue-other.json"),
            operation: "c".repeat(32),
        };
        let neighbor = NeighborStaging::create(&owner, &data.join("file"), None)
            .unwrap()
            .unwrap();
        neighbor.candidate(&data.join(".partial")).unwrap();
        let vfs = crate::LocalVfs::rooted(&data).unwrap();
        let binding =
            crate::EndpointBinding::local(&crate::Vfs::root_identity(&vfs, &data).unwrap())
                .unwrap();
        let view = namespace_staging_view(&config, &[(&binding, None, data.as_path())]).unwrap();
        assert!(view.candidates.iter().any(|candidate| candidate.object
            == StagingObject::Path(data.join(".partial"))
            && candidate.active));
        assert!(!view
            .candidates
            .iter()
            .any(|candidate| candidate.object.base_path().starts_with(&config)));
        drop(neighbor);
        let view = namespace_staging_view(&config, &[(&binding, None, data.as_path())]).unwrap();
        assert!(view.candidates.iter().all(|candidate| !candidate.active));
    }
    #[test]
    fn namespace_registry_maps_nested_profile_roots_of_the_same_account() {
        let area = ::tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap();
        let config = area.path().canonicalize().unwrap();
        let owner = StagingOwner {
            queue: config.join("operations-queue-alias.json"),
            operation: "d".repeat(32),
        };
        let identity = |canonical: &str, virtual_root: &str| {
            crate::RootIdentity::Sftp(crate::remote::SftpRootIdentity {
                server: crate::remote::SftpServerIdentity {
                    host: "same-server".into(),
                    port: 22,
                    host_key: crate::SftpHostKeyPin {
                        algorithm: "ssh-ed25519".into(),
                        sha256: [8; 32],
                    },
                    canonical_root: canonical.into(),
                    sftp_version: 3,
                    extensions_digest: [5; 32],
                },
                virtual_root: virtual_root.into(),
                session_generation: 1,
            })
        };
        let source = identity("/vault", "/pane");
        let target = identity("/vault/pane", "/");
        let independent = identity("/vault/other", "/");
        let target_binding = crate::EndpointBinding::remote(&target, [2; 32]).unwrap();
        let independent_binding = crate::EndpointBinding::remote(&independent, [2; 32]).unwrap();
        let intent = RemoteTempIntent::create_bound_with_root(
            &owner,
            Path::new("/pane/file"),
            Path::new("/pane/.partial"),
            &source,
            [2; 32],
        )
        .unwrap();
        let view =
            namespace_staging_view(&config, &[(&target_binding, Some(&target), Path::new("/"))])
                .unwrap();
        assert_eq!(view.candidates.len(), 1);
        assert_eq!(
            view.candidates[0].object,
            StagingObject::Path("/.partial".into())
        );
        assert!(view.candidates[0].active);
        assert!(namespace_staging_view(
            &config,
            &[(&independent_binding, Some(&independent), Path::new("/"))]
        )
        .unwrap()
        .candidates
        .is_empty());
        intent.finish().unwrap();
    }
    #[test]
    fn namespace_private_stage_inside_pair_blocks_active_and_crashed_forward() {
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let left = root.join("left");
        let right = root.join("right");
        let independent = root.join("independent");
        for path in [&left, &right, &independent] {
            std::fs::create_dir(path).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        for name in ["a", "b", "c"] {
            std::fs::write(left.join(name), name).unwrap();
            std::fs::write(right.join(name), name).unwrap();
        }
        let queue =
            crate::OperationQueue::open(root.join("config/operations-queue-other.json")).unwrap();
        let (owner, _, _) = queue.ftps_owner(None).unwrap();
        let held = RootHandle::open_nofollow(&left).unwrap();
        let stage = LocalStaging::create(
            Some(&held),
            None,
            &left.join("download"),
            None,
            Some(&owner),
            None,
        )
        .unwrap()
        .unwrap();
        let candidate = stage.candidate(&left.join("download"), "copy").unwrap();
        std::fs::write(&candidate, b"real partial data").unwrap();
        let stage_path = stage.directory.path().to_path_buf();
        assert!(stage_path.starts_with(&left));
        let (forward_owner, _, _) = queue.ftps_owner(None).unwrap();
        let baseline = root.join("baseline.json");
        let pair = crate::PairId::generate().unwrap();
        let prepare = |local: &Path, baseline: &Path| {
            crate::bidirectional::forward::prepare_forward_bidirectional_owned(
                pair,
                crate::ProviderEndpoint::local(local).unwrap(),
                crate::ProviderEndpoint::test_sftp_local(&right).unwrap(),
                baseline,
                &[],
                false,
                None,
                &crate::Cancel::new(),
                Some(crate::remote_real_mode::InteractiveFileOwner {
                    staging: forward_owner.clone(),
                    profiles: Vec::new(),
                }),
            )
        };
        let error = prepare(&left, &baseline)
            .err()
            .expect("Aktiver privater Stagebaum muss frischen Forward vor Planung sperren");
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(!baseline.exists());
        for (source, target, mode) in [
            (&left, &right, crate::SyncMode::AddOnly),
            (&right, &left, crate::SyncMode::Mirror),
        ] {
            let error = crate::prepare_content_sync_for_queue(
                &queue,
                crate::ProviderEndpoint::local(source).unwrap(),
                crate::ProviderEndpoint::local(target).unwrap(),
                crate::SyncOptions {
                    mode,
                    compare: crate::Compare::ContentHash,
                    ..crate::SyncOptions::default()
                },
                &crate::Cancel::new(),
            )
            .err()
            .expect("Einseitiger Sync darf Stage weder übernehmen noch Mirror-entfernen");
            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            assert!(stage_path.exists());
        }

        let unrelated = prepare(&independent, &root.join("independent-baseline.json"))
            .expect("Nicht betroffener lokaler Root bleibt nutzbar");
        unrelated.discard().unwrap();
        drop(stage);
        let error = prepare(&left, &baseline)
            .err()
            .expect("Abgestürzter nichtleerer Stagebaum bleibt gesperrt");
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        std::fs::rename(&stage_path, root.join("user-reviewed-stage")).unwrap();
        let prepared = prepare(&left, &baseline).unwrap();
        let digest = prepared.preview().digest;
        prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut |_| {})
            .unwrap();
        let store = crate::BaselineStore::open(&baseline).unwrap();
        let mut paths = store
            .load()
            .unwrap()
            .unwrap()
            .entries
            .into_iter()
            .map(|record| record.path.display_lossy())
            .collect::<Vec<_>>();
        paths.sort();
        assert_eq!(paths, ["a", "b", "c"]);
    }
    #[test]
    fn remote_directory_intents_guard_nested_roots_and_rebound_profile_roots() {
        let area = ::tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap();
        let config = RootHandle::open_nofollow(&area.path().canonicalize().unwrap()).unwrap();
        let owner = StagingOwner {
            queue: config.path().join("queue.json"),
            operation: "c".repeat(32),
        };
        let identity = crate::RootIdentity::Sftp(crate::remote::SftpRootIdentity {
            server: crate::remote::SftpServerIdentity {
                host: "server".into(),
                port: 22,
                host_key: crate::SftpHostKeyPin {
                    algorithm: "ssh-ed25519".into(),
                    sha256: [8; 32],
                },
                canonical_root: "/vault".into(),
                sftp_version: 3,
                extensions_digest: [5; 32],
            },
            virtual_root: "/pane".into(),
            session_generation: 1,
        });
        let intent = RemoteTempIntent::create_bound_with_root(
            &owner,
            Path::new("/pane/dir"),
            Path::new("/pane/.temp"),
            &identity,
            [2; 32],
        )
        .unwrap();
        let check = |path: &str, rebound: bool, expected: bool| {
            let mut nested = identity.clone();
            let crate::RootIdentity::Sftp(value) = &mut nested else {
                unreachable!()
            };
            if rebound {
                value.server.canonical_root = format!("/vault{path}");
                value.virtual_root = "/".into();
            } else {
                value.virtual_root = path.into();
            }
            let root = if rebound {
                Path::new("/")
            } else {
                Path::new(path)
            };
            let binding = crate::EndpointBinding::remote(&nested, [2; 32]).unwrap();
            let view = namespace_staging_view(
                config.path(),
                &[
                    (&binding, Some(&nested), root),
                    (&binding, Some(&nested), root),
                ],
            )
            .unwrap();
            assert_eq!(
                !view.candidates.is_empty(),
                expected,
                "{path}, rebound={rebound}"
            );
            if expected {
                assert_eq!(view.candidates.len(), 2);
                assert!(view.candidates.iter().all(|candidate| candidate.unknown));
            }
            let other_account = crate::EndpointBinding::remote(&nested, [3; 32]).unwrap();
            assert!(namespace_staging_view(
                config.path(),
                &[(&other_account, Some(&nested), root)]
            )
            .unwrap()
            .candidates
            .is_empty());
        };
        for rebound in [false, true] {
            check("/pane/.temp/inside", rebound, true);
            check("/pane/independent", rebound, false);
        }
        intent.rename_cycle_pending().unwrap();
        for rebound in [false, true] {
            check("/pane/dir/inside", rebound, true);
        }
        drop(intent);
        acknowledge_rename_cycle_intents_after_review(&owner, &config, Path::new("/pane/.temp"))
            .unwrap();
        for rebound in [false, true] {
            check("/pane/.temp/inside", rebound, true);
            check("/pane/dir/inside", rebound, false);
        }
    }
}

#[cfg(test)]
#[path = "local_staging_directory_dacl_tests.rs"]
mod directory_dacl_registry_tests;
