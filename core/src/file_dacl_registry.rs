//! Additiver V6-Intent; alte Neighbor-V1–V5-Formate bleiben unverändert.
//! Jede Phase wird atomar und fsync-bestätigt gespeichert. Geschlossene Vollbelege
//! liegen getrennt vom heißen Ledger und bleiben bis zum dauerhaften Queuecheckpoint.
use crate::file_dacl::{FileDaclPlan, FileDaclReport};
use crate::local_fs::RootHandle;
use crate::local_staging::StagingOwner;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const PREFIX: &str = ".file-dacl-v6-";
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Phase {
    Prepared,
    Applying,
    Applied,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    owner: StagingOwner,
    endpoint: crate::EndpointBinding,
    root: crate::RootIdentity,
    file: PathBuf,
    plan: FileDaclPlan,
    phase: Phase,
    closed: bool,
    generation: u64,
    closed_observation: Option<bool>,
}
impl Record {
    fn validate(&self, config: &RootHandle) -> io::Result<()> {
        self.plan.validate()?;
        let crate::RootIdentity::Smb(root) = &self.root else {
            return Err(io::Error::other("Datei-DACL ohne SMB-Root"));
        };
        root.validate()?;
        if self.version != 6
            || self.owner.queue.parent() != Some(config.path())
            || !self.file.is_absolute()
            || !self.file.starts_with(&root.virtual_root)
            || self.file == Path::new(&root.virtual_root)
            || self.plan.context.namespace != root.server.server_guid
            || object_namespace(&self.root)? != self.plan.object_namespace
            || crate::EndpointBinding::remote(&self.root, root.server.account_digest)?
                != self.endpoint
            || self.closed != self.closed_observation.is_some()
            || self.generation
                != match self.phase {
                    Phase::Prepared => 0,
                    Phase::Applying => 1,
                    Phase::Applied => 2,
                } + u64::from(self.closed)
            || self.closed_observation == Some(true) && self.phase == Phase::Prepared
            || self.closed_observation == Some(false) && self.phase == Phase::Applied
        {
            return Err(io::Error::other("Ungültige V6-Datei-DACL-Bindung"));
        }
        Ok(())
    }
    fn report(&self) -> FileDaclReport {
        let mut report = FileDaclReport::default();
        if self.closed_observation == Some(false) {
            return report;
        }
        if let Ok(outcome) = self.plan.outcome(&self.file.to_string_lossy()) {
            match self.phase {
                Phase::Prepared => {}
                Phase::Applying if self.closed_observation == Some(true) => {
                    report.completed.push(outcome)
                }
                Phase::Applying => report.uncertain.push(outcome),
                Phase::Applied => report.completed.push(outcome),
            }
        }
        report
    }
}
/// ID gilt nur innerhalb desselben authentifizierten Server-/Share-Namensraums.
pub(crate) fn object_namespace(identity: &crate::RootIdentity) -> io::Result<[u8; 32]> {
    let crate::RootIdentity::Smb(root) = identity else {
        return Err(io::Error::other("Datei-DACL benötigt SMB-Namensraum"));
    };
    let mut hash = blake3::Hasher::new();
    hash.update(b"doppeldecker-file-object-namespace-v6\0");
    hash.update(
        &serde_json::to_vec(&(root.server.server_guid, root.server.share.to_lowercase()))
            .map_err(io::Error::other)?,
    );
    Ok(*hash.finalize().as_bytes())
}

pub(crate) struct Intent {
    config: RootHandle,
    path: PathBuf,
    record: Mutex<Record>,
    _lease: File,
}
impl Intent {
    pub(crate) fn create(
        owner: &StagingOwner,
        file: &Path,
        plan: &FileDaclPlan,
        root: &crate::RootIdentity,
        account: [u8; 32],
    ) -> io::Result<Self> {
        let config = RootHandle::open_nofollow(
            owner
                .queue
                .parent()
                .ok_or_else(|| io::Error::other("Queue ohne Elternordner"))?,
        )
        .map_err(|error| crate::error::with_context(error, "Datei-DACL-Queueordner öffnen"))?;
        ensure(&config)
            .map_err(|error| crate::error::with_context(error, "Datei-DACL-Queueordner prüfen"))?;
        let record = Record {
            version: 6,
            owner: owner.clone(),
            endpoint: crate::EndpointBinding::remote(root, account)?,
            root: root.clone(),
            file: file.into(),
            plan: plan.clone(),
            phase: Phase::Prepared,
            closed: false,
            generation: 0,
            closed_observation: None,
        };
        record.validate(&config)?;
        let candidate =
            crate::atomic_file::sibling_candidate(&config.path().join("file-dacl-v6"), "intent")?;
        let name = format!(
            "{PREFIX}{}.json",
            candidate.file_name().unwrap().to_string_lossy()
        );
        let path = config.path().join(name);
        // Lease und erste Ledgerregistrierung teilen denselben Registrylock.
        // Zwei erstmalige Fenster können dadurch keine fremden Orphan-Leases sehen.
        let _registry_lock = registry_lock(&config)
            .map_err(|error| crate::error::with_context(error, "Datei-DACL-Registry sperren"))?;
        let lease = File::from(
            rustix::fs::openat(
                config.fd(),
                lease_path(&path).file_name().unwrap(),
                rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::RDWR
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::from_raw_mode(0o600),
            )
            .map_err(|error| {
                crate::error::with_context(error.into(), "Datei-DACL-Lease anlegen")
            })?,
        );
        rustix::fs::flock(&lease, rustix::fs::FlockOperation::LockExclusive)?;
        lease.sync_all()?;
        persist_locked(&config, &path, &record, true).map_err(|error| {
            crate::error::with_context(error, "Datei-DACL-Intent dauerhaft registrieren")
        })?;
        Ok(Self {
            config,
            path,
            record: Mutex::new(record),
            _lease: lease,
        })
    }
    pub(crate) fn phase(&self, phase: Phase) -> io::Result<()> {
        let mut record = self
            .record
            .lock()
            .map_err(|_| io::Error::other("Datei-DACL-Intent gesperrt"))?;
        if record.closed
            || !matches!(
                (record.phase, phase),
                (Phase::Prepared, Phase::Applying) | (Phase::Applying, Phase::Applied)
            )
        {
            return Err(io::Error::other("Ungültige Datei-DACL-Phase"));
        }
        let mut next = record.clone();
        next.phase = phase;
        next.generation += 1;
        // Beim Fehlschlag bleibt mindestens Applying erhalten, falls SET schon lief.
        persist(&self.config, &self.path, &next, false)?;
        *record = next;
        Ok(())
    }
    pub(crate) fn report(&self) -> FileDaclReport {
        self.record
            .lock()
            .map(|record| record.report())
            .unwrap_or_default()
    }
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn finish(&self, applied: bool) -> io::Result<()> {
        let mut record = self
            .record
            .lock()
            .map_err(|_| io::Error::other("Datei-DACL-Intent gesperrt"))?;
        if record.phase
            != if applied {
                Phase::Applied
            } else {
                Phase::Prepared
            }
        {
            return Err(io::Error::other(
                "Datei-DACL-Phase erlaubt keinen Abschluss",
            ));
        }
        let mut next = record.clone();
        next.closed = true;
        next.closed_observation = Some(applied);
        next.generation += 1;
        persist(&self.config, &self.path, &next, false)?;
        *record = next;
        Ok(())
    }
}
fn lease_path(path: &Path) -> PathBuf {
    path.with_extension("lease")
}
fn registry_lock(config: &RootHandle) -> io::Result<File> {
    ensure(config)?;
    use rustix::fs::{openat, Mode, OFlags};
    let name = ".file-dacl-v6-registry.lock";
    let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    // macOS kann zwei gleichzeitige O_CREAT-Öffnungen derselben neuen Datei
    // mit ENOENT beantworten. Exklusive Erstanlage trennt Kollision und Fehlen.
    let opened = match openat(config.fd(), name, flags, Mode::empty()) {
        Ok(fd) => Ok(fd),
        Err(rustix::io::Errno::NOENT) => match openat(
            config.fd(),
            name,
            flags | OFlags::CREATE | OFlags::EXCL,
            Mode::from_raw_mode(0o600),
        ) {
            Err(rustix::io::Errno::EXIST) => openat(config.fd(), name, flags, Mode::empty()),
            result => result,
        },
        result => result,
    };
    let file =
        File::from(opened.map_err(|error| {
            crate::error::with_context(error.into(), "Registrylockdatei öffnen")
        })?);
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
    ensure(config)?;
    Ok(file)
}

fn ensure(config: &RootHandle) -> io::Result<()> {
    if !config.is_current()? || !config.has_exclusive_namespace()? {
        return Err(io::Error::other(
            "Datei-DACL-Registry verlor ihren privaten Namensraum",
        ));
    }
    Ok(())
}
// Das heiße Ledger enthält keine abgeschlossene Vollplanhistorie. Die Zähler
// bleiben monoton; eigene Beiträge stehen getrennt in privaten Ownerdateien.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    generation: u64,
    records: std::collections::BTreeMap<String, Record>,
    format: u32,
    namespaces: std::collections::BTreeMap<String, u64>,
    retained_count: u64,
    checkpointed: std::collections::BTreeSet<String>,
    pending: Option<Pending>,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerState {
    owner: StagingOwner,
    contributions: std::collections::BTreeMap<String, u64>,
    retained_count: u64,
    retained_digest: [u8; 32],
}
impl OwnerState {
    fn new(owner: &StagingOwner) -> Self {
        Self {
            owner: owner.clone(),
            contributions: Default::default(),
            retained_count: 0,
            retained_digest: [0; 32],
        }
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    state: OwnerState,
    remove: Vec<PathBuf>,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    digest: [u8; 32],
    ledger: Ledger,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerEnvelope {
    version: u32,
    digest: [u8; 32],
    state: OwnerState,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedEnvelope {
    version: u32,
    digest: [u8; 32],
    name: String,
    record: Record,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyLedger {
    generation: u64,
    records: std::collections::BTreeMap<String, Record>,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyEnvelope {
    version: u32,
    digest: [u8; 32],
    ledger: LegacyLedger,
}
const LEDGER: &str = ".file-dacl-v6-ledger.json";
const OWNERS: &str = ".file-dacl-v6-owners";
const CLOSED: &str = ".file-dacl-v6-closed";
fn digest(domain: &[u8], value: &impl serde::Serialize) -> io::Result<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    hash.update(domain);
    hash.update(&serde_json::to_vec(value).map_err(io::Error::other)?);
    Ok(*hash.finalize().as_bytes())
}
fn ledger_digest(ledger: &Ledger) -> io::Result<[u8; 32]> {
    digest(b"doppeldecker-file-dacl-hot-ledger-v6\0", ledger)
}
fn key(value: &impl serde::Serialize) -> io::Result<String> {
    Ok(
        blake3::Hash::from_bytes(digest(b"doppeldecker-file-dacl-private-key-v6\0", value)?)
            .to_hex()
            .to_string(),
    )
}
fn owner_relative(owner: &StagingOwner) -> io::Result<PathBuf> {
    Ok(PathBuf::from(key(&owner.queue)?).join(key(&owner.operation)?))
}
fn owner_path(config: &RootHandle, owner: &StagingOwner) -> io::Result<PathBuf> {
    Ok(config
        .path()
        .join(OWNERS)
        .join(owner_relative(owner)?)
        .with_extension("json"))
}
fn closed_path(config: &RootHandle, owner: &StagingOwner, name: &str) -> io::Result<PathBuf> {
    Ok(config
        .path()
        .join(CLOSED)
        .join(owner_relative(owner)?)
        .join(name))
}
fn valid_name(name: &str) -> bool {
    name.starts_with(PREFIX) && name.ends_with(".json") && !name.contains('/') && name != LEDGER
}
fn private_parent(config: &RootHandle, path: &Path) -> io::Result<()> {
    let relative = path
        .parent()
        .unwrap()
        .strip_prefix(config.path())
        .map_err(io::Error::other)?;
    let mut held = RootHandle::open_nofollow(config.path())?;
    if held.identity() != config.identity() {
        return Err(io::Error::other("Registryordner wurde ausgetauscht"));
    }
    for part in relative.components() {
        let std::path::Component::Normal(name) = part else {
            return Err(io::Error::other("Ungültiger privater Registryordner"));
        };
        match rustix::fs::mkdirat(held.fd(), name, rustix::fs::Mode::from_raw_mode(0o700)) {
            Ok(()) => rustix::fs::fsync(held.fd())?,
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        held = RootHandle::open_nofollow(&held.path().join(name))?;
        if !held.has_exclusive_namespace()? {
            return Err(io::Error::other(
                "Registryunterordner verlor privaten Namensraum",
            ));
        }
    }
    ensure(config)
}
fn write_json(
    config: &RootHandle,
    path: &Path,
    value: &impl serde::Serialize,
    create: bool,
) -> io::Result<()> {
    ensure(config)?;
    private_parent(config, path)?;
    let vfs = crate::LocalVfs::rooted(config.path())?;
    if crate::Vfs::anchored_identity(&vfs) != Some(config.identity()) {
        return Err(io::Error::other("Datei-DACL-Registry wurde ausgetauscht"));
    }
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(io::Error::other(
            "Einzelner V6-Datensatz überschreitet Größenlimit",
        ));
    }
    crate::atomic_write::atomic_write_vfs_with_mode(
        &vfs,
        path,
        &bytes,
        ".file-dacl-v6-write-",
        if create {
            crate::atomic_write::AtomicWriteMode::Create
        } else {
            crate::atomic_write::AtomicWriteMode::Replace
        },
        Some(0o600),
    )
}
fn read_json<T: serde::de::DeserializeOwned>(config: &RootHandle, path: &Path) -> io::Result<T> {
    let mut bytes = Vec::new();
    crate::local_fs::open_read_beneath(config, path)?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(io::Error::other("V6-Datensatz überschreitet Größenlimit"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
fn owner_digest(state: &OwnerState) -> io::Result<[u8; 32]> {
    digest(b"doppeldecker-file-dacl-owner-v6\0", state)
}
fn load_owner(config: &RootHandle, owner: &StagingOwner) -> io::Result<OwnerState> {
    let path = owner_path(config, owner)?;
    let envelope: OwnerEnvelope = match read_json(config, &path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(OwnerState::new(owner)),
        Err(error) => return Err(error),
    };
    if envelope.version != 6
        || envelope.state.owner != *owner
        || envelope.state.owner.queue.parent() != Some(config.path())
        || envelope.digest != owner_digest(&envelope.state)?
        || envelope
            .state
            .contributions
            .values()
            .any(|count| *count == 0)
    {
        return Err(io::Error::other("Beschädigter V6-Ownerzähler"));
    }
    Ok(envelope.state)
}
fn write_owner(config: &RootHandle, state: &OwnerState) -> io::Result<()> {
    write_json(
        config,
        &owner_path(config, &state.owner)?,
        &OwnerEnvelope {
            version: 6,
            digest: owner_digest(state)?,
            state: state.clone(),
        },
        false,
    )
}
fn closed_digest(name: &str, record: &Record) -> io::Result<[u8; 32]> {
    digest(b"doppeldecker-file-dacl-closed-v6\0", &(name, record))
}
fn xor(into: &mut [u8; 32], value: [u8; 32]) {
    for (left, right) in into.iter_mut().zip(value) {
        *left ^= right;
    }
}
fn save_ledger(config: &RootHandle, ledger: &mut Ledger) -> io::Result<()> {
    ledger.generation = ledger
        .generation
        .checked_add(1)
        .ok_or_else(|| io::Error::other("V6-Generation erschöpft"))?;
    write_json(
        config,
        &config.path().join(LEDGER),
        &Envelope {
            version: 6,
            digest: ledger_digest(ledger)?,
            ledger: Ledger {
                generation: ledger.generation,
                records: ledger.records.clone(),
                format: ledger.format,
                namespaces: ledger.namespaces.clone(),
                retained_count: ledger.retained_count,
                checkpointed: ledger.checkpointed.clone(),
                pending: ledger.pending.as_ref().map(|pending| Pending {
                    state: pending.state.clone(),
                    remove: pending.remove.clone(),
                }),
            },
        },
        ledger.generation == 1,
    )
}
fn remove_exact(config: &RootHandle, path: &Path) -> io::Result<()> {
    // Nur zuvor in unserem dauerhaft geschriebenen Commit benannte Metadaten.
    if !path.starts_with(config.path()) || path == config.path() {
        return Err(io::Error::other("V6-Bereinigung verlässt Registry"));
    }
    match crate::local_fs::remove_file_beneath(config, path) {
        Ok(()) => crate::local_fs::sync_dir_beneath(config, path.parent().unwrap()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
fn finish_pending(config: &RootHandle, ledger: &mut Ledger) -> io::Result<()> {
    if let Some(pending) = &ledger.pending {
        if pending.state.owner.queue.parent() != Some(config.path()) {
            return Err(io::Error::other("V6-Ownercommit verlässt Queueordner"));
        }
        for path in &pending.remove {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| io::Error::other("Ungültiger V6-Metadatenname"))?;
            let intent = if name.ends_with(".lease") {
                Path::new(name).with_extension("json")
            } else {
                PathBuf::from(name)
            };
            let intent = intent.to_str().unwrap();
            if !valid_name(intent)
                || (path != &config.path().join(name)
                    && path != &closed_path(config, &pending.state.owner, intent)?)
            {
                return Err(io::Error::other("V6-Commit benennt fremde Metadaten"));
            }
        }
        // Der heiße Commit enthält den vollständigen nächsten Zählerstand. Ein
        // Crash darf weder doppelt zählen noch vor einem Queuecheckpoint löschen.
        write_owner(config, &pending.state)?;
        fault("after_owner")?;
        for path in &pending.remove {
            remove_exact(config, path)?;
        }
        fault("after_cleanup")?;
        ledger.pending = None;
        save_ledger(config, ledger)?;
    }
    Ok(())
}
#[cfg(test)]
thread_local! { static TEST_FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }
fn fault(_stage: &'static str) -> io::Result<()> {
    #[cfg(test)]
    if TEST_FAULT.with(|value| value.get() == Some(_stage)) {
        TEST_FAULT.with(|value| value.set(None));
        return Err(io::Error::other(format!("V6-Testabbruch {_stage}")));
    }
    Ok(())
}
fn load_ledger(config: &RootHandle, creating: Option<&Path>) -> io::Result<Ledger> {
    let value: serde_json::Value = match read_json(config, &config.path().join(LEDGER)) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if crate::local_fs::list_beneath(config, config.path())?
                .iter()
                .any(|entry| {
                    entry.name.starts_with(PREFIX)
                        && (entry.is_dir
                            || entry.name.ends_with(".json")
                            || entry.name.ends_with(".lease"))
                        && creating.is_none_or(|path| {
                            lease_path(path).file_name().and_then(|name| name.to_str())
                                != Some(entry.name.as_str())
                        })
                })
            {
                return Err(io::Error::other("V6-Ledger fehlt trotz Datei-DACL-Belegen"));
            }
            return Ok(Ledger {
                format: 1,
                ..Default::default()
            });
        }
        Err(error) => return Err(error),
    };
    // Auch ein zuvor nur sichtbar gewordener Rename wird erst nach Datei- und
    // Eltern-fsync als dauerhafter lokaler Abschluss verwendet.
    crate::local_fs::open_read_beneath(config, &config.path().join(LEDGER))?.sync_all()?;
    crate::local_fs::sync_dir_beneath(config, config.path())?;
    if value["ledger"].get("format").is_none() {
        let old: LegacyEnvelope = serde_json::from_value(value).map_err(io::Error::other)?;
        if old.version != 6
            || old.digest != digest(b"doppeldecker-file-dacl-ledger-v6\0", &old.ledger)?
        {
            return Err(io::Error::other("Beschädigtes altes V6-Ledger"));
        }
        return migrate_legacy(config, old.ledger);
    }
    let envelope: Envelope = serde_json::from_value(value).map_err(io::Error::other)?;
    if envelope.version != 6
        || envelope.ledger.format != 1
        || envelope.digest != ledger_digest(&envelope.ledger)?
        || envelope.ledger.generation == 0
    {
        return Err(io::Error::other("Beschädigtes heißes V6-Ledger"));
    }
    let mut ledger = envelope.ledger;
    for (name, record) in &ledger.records {
        if !valid_name(name) || record.closed {
            return Err(io::Error::other("Ungültiger heißer V6-Intent"));
        }
        record.validate(config)?;
    }
    if ledger
        .checkpointed
        .iter()
        .any(|name| !ledger.records.contains_key(name))
    {
        return Err(io::Error::other("V6-Checkpoint ohne offenen Intent"));
    }
    finish_pending(config, &mut ledger)?;
    for record in ledger.records.values() {
        let owner = load_owner(config, &record.owner)?;
        let scope = key(&record.plan.object_namespace)?;
        if owner.contributions.get(&scope).copied().unwrap_or(0) == 0 {
            return Err(io::Error::other(
                "V6-Ownerzähler eines offenen Intents fehlt",
            ));
        }
    }
    Ok(ledger)
}
fn migrate_legacy(config: &RootHandle, old: LegacyLedger) -> io::Result<Ledger> {
    let mut ledger = Ledger {
        generation: old.generation,
        format: 1,
        ..Default::default()
    };
    let mut owners = std::collections::BTreeMap::<String, OwnerState>::new();
    for (name, record) in old.records {
        if !valid_name(&name) {
            return Err(io::Error::other("Ungültiger alter V6-Intent"));
        }
        record.validate(config)?;
        let ns = key(&record.plan.object_namespace)?;
        *ledger.namespaces.entry(ns.clone()).or_default() += 1;
        let state = owners
            .entry(key(&record.owner)?)
            .or_insert_with(|| OwnerState::new(&record.owner));
        *state.contributions.entry(ns).or_default() += 1;
        if record.closed {
            let checksum = closed_digest(&name, &record)?;
            write_json(
                config,
                &closed_path(config, &record.owner, &name)?,
                &ClosedEnvelope {
                    version: 6,
                    digest: checksum,
                    name,
                    record,
                },
                false,
            )?;
            state.retained_count += 1;
            ledger.retained_count += 1;
            xor(&mut state.retained_digest, checksum);
        } else {
            ledger.records.insert(name, record);
        }
    }
    for state in owners.values() {
        write_owner(config, state)?;
    }
    // Erst dieser atomare Commit löst das alte vollständige Ledger ab.
    fault("before_migration_commit")?;
    save_ledger(config, &mut ledger)?;
    Ok(ledger)
}
fn persist(config: &RootHandle, path: &Path, record: &Record, create: bool) -> io::Result<()> {
    let _lock = registry_lock(config)?;
    persist_locked(config, path, record, create)
}
fn persist_locked(
    config: &RootHandle,
    path: &Path,
    record: &Record,
    create: bool,
) -> io::Result<()> {
    ensure(config)?;
    record.validate(config)?;
    if path.parent() != Some(config.path()) {
        return Err(io::Error::other("Datei-DACL-Intent verlässt Registry"));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| valid_name(name))
        .ok_or_else(|| io::Error::other("Ungültiger V6-Intentname"))?;
    let mut ledger = load_ledger(config, create.then_some(path))?;
    if create {
        if ledger.records.values().any(|previous| {
            previous.plan.object_namespace == record.plan.object_namespace
                && previous.plan.file_id == record.plan.file_id
        }) {
            return Err(crate::coded_error(
                io::ErrorKind::WouldBlock,
                crate::ErrorCode::OutcomeUnclear,
                "Für dasselbe SMB-Dateiobjekt existiert bereits ein offener DACL-Intent",
            ));
        }
        if ledger.records.contains_key(name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Datei-DACL-Intent existiert bereits",
            ));
        }
        let mut state = load_owner(config, &record.owner)?;
        let ns = key(&record.plan.object_namespace)?;
        let total = ledger.namespaces.entry(ns.clone()).or_default();
        *total = total
            .checked_add(1)
            .ok_or_else(|| io::Error::other("V6-Namensraumzähler erschöpft"))?;
        let own = state.contributions.entry(ns).or_default();
        *own = own
            .checked_add(1)
            .ok_or_else(|| io::Error::other("V6-Ownerzähler erschöpft"))?;
        ledger.pending = Some(Pending {
            state,
            remove: Vec::new(),
        });
    } else {
        let previous = ledger
            .records
            .get(name)
            .ok_or_else(|| io::Error::other("Datei-DACL-Intent verschwand aus Ledger"))?;
        if previous.generation + 1 != record.generation
            || previous.closed
            || previous.owner != record.owner
            || previous.endpoint != record.endpoint
            || previous.root != record.root
            || previous.file != record.file
            || previous.plan != record.plan
        {
            return Err(io::Error::other(
                "Datei-DACL-Intentgeneration oder Bindung änderte sich",
            ));
        }
    }
    if record.closed {
        let checksum = closed_digest(name, record)?;
        let cold = closed_path(config, &record.owner, name)?;
        // Vollständiger dauerhafter Beleg VOR Entfernen aus dem heißen Ledger.
        write_json(
            config,
            &cold,
            &ClosedEnvelope {
                version: 6,
                digest: checksum,
                name: name.into(),
                record: record.clone(),
            },
            false,
        )?;
        fault("after_archive")?;
        let mut state = load_owner(config, &record.owner)?;
        state.retained_count = state
            .retained_count
            .checked_add(1)
            .ok_or_else(|| io::Error::other("V6-Belegzähler erschöpft"))?;
        xor(&mut state.retained_digest, checksum);
        let mut remove = vec![path.into(), lease_path(path)];
        ledger.retained_count = ledger
            .retained_count
            .checked_add(1)
            .ok_or_else(|| io::Error::other("V6-Gesamtbelegzähler erschöpft"))?;
        if ledger.checkpointed.remove(name) {
            remove.push(cold);
            state.retained_count -= 1;
            ledger.retained_count -= 1;
            xor(&mut state.retained_digest, checksum);
        }
        ledger.records.remove(name);
        ledger.pending = Some(Pending { state, remove });
    } else {
        ledger.records.insert(name.into(), record.clone());
    }
    save_ledger(config, &mut ledger)?;
    fault("after_hot_commit")?;
    finish_pending(config, &mut ledger)?;
    if !record.closed {
        write_json(config, path, record, create)?;
    }
    Ok(())
}
fn archived_for_owner(
    config: &RootHandle,
    ledger: &Ledger,
    owner: &StagingOwner,
) -> io::Result<Vec<(PathBuf, Record)>> {
    let state = load_owner(config, owner)?;
    let directory = config.path().join(CLOSED).join(owner_relative(owner)?);
    let listing = match crate::local_fs::list_beneath(config, &directory) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let mut count = 0u64;
    let mut sum = [0; 32];
    let mut result = Vec::new();
    for entry in listing {
        if !valid_name(&entry.name) {
            continue;
        }
        if !entry.is_file || entry.is_symlink {
            return Err(io::Error::other("V6-Closedbeleg ist keine reguläre Datei"));
        }
        let envelope: ClosedEnvelope = read_json(config, &directory.join(&entry.name))?;
        if envelope.version != 6
            || envelope.name != entry.name
            || envelope.record.owner != *owner
            || !envelope.record.closed
            || envelope.digest != closed_digest(&envelope.name, &envelope.record)?
        {
            return Err(io::Error::other("Beschädigter V6-Closedbeleg"));
        }
        envelope.record.validate(config)?;
        // Ein vor dem heißen Commit geschriebener Closedbeleg ist noch kein
        // Abschluss. Der offene Ledgerrecord bleibt maßgeblich und sperrt.
        if ledger.records.contains_key(&envelope.name) {
            continue;
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| io::Error::other("V6-Belegzähler erschöpft"))?;
        xor(&mut sum, envelope.digest);
        result.push((config.path().join(envelope.name), envelope.record));
    }
    if count != state.retained_count || sum != state.retained_digest {
        return Err(io::Error::other("V6-Closedbeleg fehlt oder änderte sich"));
    }
    Ok(result)
}
fn owner_records_locked(
    config: &RootHandle,
    ledger: &Ledger,
    owner: &StagingOwner,
) -> io::Result<Vec<(PathBuf, Record)>> {
    let mut result = archived_for_owner(config, ledger, owner)?;
    result.extend(
        ledger
            .records
            .iter()
            .filter(|(_, record)| record.owner == *owner)
            .map(|(name, record)| (config.path().join(name), record.clone())),
    );
    Ok(result)
}
fn records_for_owner(
    config: &RootHandle,
    owner: &StagingOwner,
) -> io::Result<Vec<(PathBuf, Record)>> {
    let _lock = registry_lock(config)?;
    let ledger = load_ledger(config, None)?;
    owner_records_locked(config, &ledger, owner)
}
pub(crate) fn has_open(owner: &StagingOwner, config: &RootHandle) -> io::Result<bool> {
    let _lock = registry_lock(config)?;
    let ledger = load_ledger(config, None)?;
    Ok(ledger.records.values().any(|record| record.owner == *owner))
}
/// Offene objektweite Intents sperren den gesamten Server-/Share-Namensraum.
/// Geschlossene Zyklen bleiben als monotone Epoch sichtbar, auch nach Retirement.
pub(crate) fn check_registry(
    config: &RootHandle,
    identities: &[crate::RootIdentity],
    ignore_owner: Option<&StagingOwner>,
) -> io::Result<Option<[u8; 32]>> {
    let _lock = registry_lock(config)?;
    let ledger = load_ledger(config, None)?;
    let scopes = identities
        .iter()
        .filter(|id| matches!(id, crate::RootIdentity::Smb(_)))
        .map(|id| object_namespace(id).and_then(|ns| key(&ns)))
        .collect::<io::Result<std::collections::BTreeSet<_>>>()?;
    for (name, record) in &ledger.records {
        if scopes.contains(&key(&record.plan.object_namespace)?) {
            return Err(crate::error::with_file_dacl_report(crate::error::with_recovery_paths(crate::coded_error(io::ErrorKind::WouldBlock,crate::ErrorCode::OutcomeUnclear,"Eine offene objektweite Datei-DACL benötigt den bewussten Recoveryabschluss. Kein automatischer SET, Restore oder Löschen."),&[record.file.to_string_lossy().into_owned(),config.path().join(name).to_string_lossy().into_owned()]),&record.report()));
        }
    }
    let own = ignore_owner
        .map(|owner| load_owner(config, owner))
        .transpose()?;
    let mut visible = std::collections::BTreeMap::new();
    for scope in scopes {
        let total = ledger.namespaces.get(&scope).copied().unwrap_or(0);
        let own = own
            .as_ref()
            .and_then(|state| state.contributions.get(&scope))
            .copied()
            .unwrap_or(0);
        let count = total
            .checked_sub(own)
            .ok_or_else(|| io::Error::other("V6-Ownerzähler überschreitet Namensraumgeneration"))?;
        if count != 0 {
            visible.insert(scope, count);
        }
    }
    if visible.is_empty() {
        Ok(None)
    } else {
        Ok(Some(digest(
            b"doppeldecker-file-dacl-registry-epoch-v6\0",
            &visible,
        )?))
    }
}
/// Nur nach erfolgreich dauerhaft geschriebenem Queuecheckpoint aufrufen.
/// Ein Applied-Intent bleibt bis finish offen; die Markierung erlaubt dann nur
/// das Entfernen genau seines bereits im Journal bestätigten privaten Belegs.
pub(crate) fn checkpoint(
    owner: &StagingOwner,
    config: &RootHandle,
    report: &FileDaclReport,
    resolved: &[crate::file_dacl::FileDaclOutcome],
    terminal: bool,
) -> io::Result<()> {
    let _lock = registry_lock(config)?;
    let mut ledger = load_ledger(config, None)?;
    let mut changed = false;
    for (name, record) in &ledger.records {
        if record.owner == *owner
            && record.phase == Phase::Applied
            && report
                .completed
                .contains(&record.plan.outcome(&record.file.to_string_lossy())?)
        {
            changed |= ledger.checkpointed.insert(name.clone());
        }
    }
    if changed {
        save_ledger(config, &mut ledger)?;
    }
    for (path, record) in archived_for_owner(config, &ledger, owner)? {
        let outcome = record.plan.outcome(&record.file.to_string_lossy())?;
        let confirmed =
            record.closed_observation == Some(true) && report.completed.contains(&outcome);
        let before = record.closed_observation == Some(false)
            && terminal
            && (record.phase == Phase::Prepared || resolved.contains(&outcome));
        if !confirmed && !before {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap();
        let checksum = closed_digest(name, &record)?;
        let mut state = load_owner(config, owner)?;
        state.retained_count = state
            .retained_count
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("V6-Belegzähler unterläuft"))?;
        xor(&mut state.retained_digest, checksum);
        ledger.retained_count = ledger
            .retained_count
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("V6-Gesamtbelegzähler unterläuft"))?;
        ledger.pending = Some(Pending {
            state,
            remove: vec![
                closed_path(config, owner, name)?,
                path.clone(),
                lease_path(&path),
            ],
        });
        save_ledger(config, &mut ledger)?;
        fault("after_retire_commit")?;
        finish_pending(config, &mut ledger)?;
    }
    Ok(())
}

fn verify_owner_inventory(config: &RootHandle, ledger: &Ledger) -> io::Result<()> {
    let directory = config.path().join(OWNERS);
    let queues = match crate::local_fs::list_beneath(config, &directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let mut retained = 0u64;
    for queue in queues {
        if !queue.is_dir || queue.is_symlink {
            return Err(io::Error::other("V6-Ownerinventar besitzt fremden Eintrag"));
        }
        let folder = directory.join(&queue.name);
        for entry in crate::local_fs::list_beneath(config, &folder)? {
            if entry.name.starts_with('.') || !entry.name.ends_with(".json") {
                continue;
            }
            let envelope: OwnerEnvelope = read_json(config, &folder.join(&entry.name))?;
            if owner_path(config, &envelope.state.owner)? != folder.join(&entry.name) {
                return Err(io::Error::other("V6-Ownerinventar besitzt fremde Bindung"));
            }
            let state = load_owner(config, &envelope.state.owner)?;
            retained = retained
                .checked_add(state.retained_count)
                .ok_or_else(|| io::Error::other("V6-Gesamtbelegzähler erschöpft"))?;
        }
    }
    if retained != ledger.retained_count {
        return Err(io::Error::other(
            "V6-Ownerbeleg fehlt; Recovery darf nicht als leer gelten",
        ));
    }
    Ok(())
}

pub(crate) fn recover_queue(
    queue: &Path,
    config: &RootHandle,
) -> io::Result<Vec<crate::local_staging::StagingRecovery>> {
    let mut map =
        std::collections::BTreeMap::<String, crate::local_staging::StagingRecovery>::new();
    let records = {
        let _lock = registry_lock(config)?;
        let ledger = load_ledger(config, None)?;
        verify_owner_inventory(config, &ledger)?;
        let directory = config.path().join(OWNERS).join(key(&queue)?);
        let entries = match crate::local_fs::list_beneath(config, &directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        let mut owners = std::collections::BTreeMap::new();
        for entry in entries {
            if !entry.name.ends_with(".json") || entry.name.starts_with('.') {
                continue;
            }
            let envelope: OwnerEnvelope = read_json(config, &directory.join(&entry.name))?;
            if envelope.state.owner.queue != queue
                || owner_path(config, &envelope.state.owner)? != directory.join(&entry.name)
            {
                return Err(io::Error::other(
                    "V6-Ownerdatei besitzt fremde Queuebindung",
                ));
            }
            let owner = envelope.state.owner;
            owners.insert(owner.operation.clone(), owner);
        }
        for record in ledger
            .records
            .values()
            .filter(|record| record.owner.queue == queue)
        {
            owners.insert(record.owner.operation.clone(), record.owner.clone());
        }
        let mut records = Vec::new();
        for owner in owners.values() {
            records.extend(owner_records_locked(config, &ledger, owner)?);
        }
        records
    };
    for (path, record) in records {
        let recovery = map
            .entry(record.owner.operation.clone())
            .or_insert_with(|| crate::local_staging::StagingRecovery {
                operation: record.owner.operation.clone(),
                paths: Vec::new(),
                ftps_root: None,
                directory_stream_report: Default::default(),
                directory_dacl_report: Default::default(),
                directory_dacl_acknowledged: false,
                directory_dacl_recovery: false,
                directory_dacl_acknowledged_paths: Vec::new(),
                file_dacl_report: Default::default(),
                file_dacl_recovery: false,
                file_dacl_acknowledged: true,
                file_dacl_acknowledged_paths: Vec::new(),
                file_dacl_resolved: Vec::new(),
                created_directory_dacl_report: Default::default(),
                created_directory_dacl_recovery: false,
                before_children_recovery: Default::default(),
                created_directory_dacl_acknowledged: false,
                created_directory_dacl_acknowledged_paths: Vec::new(),
                created_directory_dacl_resolved: Vec::new(),
            });
        recovery.file_dacl_report.merge(&record.report());
        recovery.file_dacl_acknowledged &= record.closed;
        recovery.file_dacl_recovery |= !record.closed;
        let paths = [
            record.file.to_string_lossy().into_owned(),
            path.to_string_lossy().into_owned(),
        ];
        if record.closed {
            recovery
                .file_dacl_resolved
                .push(record.plan.outcome(&record.file.to_string_lossy())?);
            recovery.file_dacl_acknowledged_paths.extend(paths);
        } else {
            recovery.paths.extend(paths);
        }
    }
    Ok(map.into_values().collect())
}

pub(crate) struct Acknowledgement {
    pub(crate) report: FileDaclReport,
    pub(crate) paths: Vec<String>,
    pub(crate) resolved: Vec<crate::file_dacl::FileDaclOutcome>,
}
pub(crate) fn acknowledge(
    owner: &StagingOwner,
    vfs: &dyn crate::Vfs,
    root: &Path,
    account: [u8; 32],
    config: &RootHandle,
) -> io::Result<Acknowledgement> {
    let identity = vfs.root_identity(root)?;
    let endpoint = crate::EndpointBinding::remote(&identity, account)?;
    let mut checked = Vec::new();
    for (path, record) in records_for_owner(config, owner)?
        .into_iter()
        .filter(|(_, r)| r.owner == *owner && !r.closed)
    {
        let lease = crate::local_fs::open_read_beneath(config, &lease_path(&path))?;
        rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Datei-DACL-Writer ist noch aktiv",
                )
            },
        )?;
        if record.endpoint != endpoint
            || !crate::local_staging::same_recovery_root(&record.root, &identity)
        {
            return Err(crate::real_mode::plan_drift(
                "Datei-DACL-Recoveryendpoint änderte sich",
            ));
        }
        let observed = vfs.file_dacl_recovery_observation(&record.file, &identity)?;
        if observed.file_id != record.plan.file_id
            || observed.object_namespace != record.plan.object_namespace
            || observed.context != record.plan.context
        {
            return Err(crate::real_mode::plan_drift(
                "Datei-DACL-Recoveryobjekt oder SID-Kontext änderte sich",
            ));
        }
        let before = observed.dacl == record.plan.before;
        let after = observed.dacl == record.plan.after;
        let recognized = match record.phase {
            Phase::Prepared => before,
            Phase::Applying => before || after,
            Phase::Applied => after,
        };
        if !recognized {
            return Err(crate::coded_error(
                io::ErrorKind::WouldBlock,
                crate::ErrorCode::OutcomeUnclear,
                "Datei-DACL-Recovery sieht keinen zum Intent passenden Vorher-/Nachherzustand",
            ));
        }
        checked.push((path, record, after, lease));
    }
    if checked.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Kein offener Datei-DACL-Intent",
        ));
    }
    let mut report = FileDaclReport::default();
    let mut paths = Vec::new();
    for (path, mut record, after, _lease) in checked {
        record.closed_observation = Some(after);
        record.generation += 1;
        record.closed = true;
        persist(config, &path, &record, false)?;
        report.merge(&record.report());
        paths.extend([
            record.file.to_string_lossy().into_owned(),
            path.to_string_lossy().into_owned(),
        ]);
    }
    // Der bewusste Abschluss ersetzt den Gesamtbericht. Frühere geschlossene
    // Objektwirkungen bleiben erhalten; ein erkannter Vorherzustand entfernt nur
    // genau den damit aufgelösten unklaren Beleg.
    report = FileDaclReport::default();
    let mut resolved = Vec::new();
    for (path, record) in records_for_owner(config, owner)?
        .into_iter()
        .filter(|(_, record)| record.owner == *owner)
    {
        report.merge(&record.report());
        resolved.push(record.plan.outcome(&record.file.to_string_lossy())?);
        if !record.closed {
            return Err(crate::error::with_file_dacl_report(
                crate::coded_error(
                    io::ErrorKind::WouldBlock,
                    crate::ErrorCode::OutcomeUnclear,
                    "Während des Datei-DACL-Abschlusses entstand ein weiterer offener Intent",
                ),
                &report,
            ));
        }
        paths.extend([
            record.file.to_string_lossy().into_owned(),
            path.to_string_lossy().into_owned(),
        ]);
    }
    paths.sort();
    paths.dedup();
    Ok(Acknowledgement {
        report,
        paths,
        resolved,
    })
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    use crate::file_dacl::tests::{fixture, identity, plan, Readback};

    fn applied(owner: &StagingOwner, effect: &FileDaclPlan) -> Intent {
        let intent = Intent::create(
            owner,
            Path::new("/pane/a"),
            effect,
            &identity("/pane"),
            [2; 32],
        )
        .unwrap();
        intent.phase(Phase::Applying).unwrap();
        intent.phase(Phase::Applied).unwrap();
        intent
    }
    #[test]
    fn file_dacl_large_single_owner_retires_over_old_lifetime_limit() {
        let (_area, owner, config) = fixture();
        let mut effect = plan();
        effect.alias_scope.selected_paths = (0..64)
            .map(|i| format!("{i:04}{}", "x".repeat(16380)))
            .collect();
        let size = serde_json::to_vec(&effect).unwrap().len();
        let mut logical = 0;
        for _ in 0..65 {
            let intent = applied(&owner, &effect);
            logical += size;
            // Diese interne Freigabe simuliert ausschließlich den schon durable
            // geschriebenen Queuecallback; direkte finish-Aufrufe tun das nicht.
            checkpoint(&owner, &config, &intent.report(), &[], false).unwrap();
            intent.finish(true).unwrap();
            assert!(std::fs::metadata(config.path().join(LEDGER)).unwrap().len() < 4096);
        }
        assert!(logical > 64 * 1024 * 1024);
        assert!(recover_queue(&owner.queue, &config).unwrap().is_empty());
        assert_eq!(
            check_registry(&config, &[identity("/pane")], Some(&owner)).unwrap(),
            None
        );
        assert!(check_registry(&config, &[identity("/pane")], None)
            .unwrap()
            .is_some());
        let next = applied(&owner, &plan());
        next.finish(true).unwrap();
        assert_eq!(
            recover_queue(&owner.queue, &config).unwrap()[0]
                .file_dacl_report
                .completed
                .len(),
            1
        );
    }
    #[test]
    fn file_dacl_many_closed_owners_do_not_grow_hot_ledger_or_guard_reads() {
        let (_area, mut owner, config) = fixture();
        let mut first = None;
        for i in 0..100 {
            owner.operation = format!("{i:032x}");
            let intent = applied(&owner, &plan());
            let name = intent
                .path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            intent.finish(true).unwrap();
            first.get_or_insert(closed_path(&config, &owner, &name).unwrap());
            assert!(std::fs::metadata(config.path().join(LEDGER)).unwrap().len() < 4096);
        }
        std::fs::write(first.unwrap(), b"corrupt").unwrap();
        assert!(check_registry(&config, &[identity("/pane")], None)
            .unwrap()
            .is_some());
        assert!(recover_queue(&owner.queue, &config).is_err());
        let intent = applied(&owner, &plan());
        intent.finish(true).unwrap();
    }
    #[test]
    fn file_dacl_direct_finish_retains_full_proof_and_missing_cold_blocks_recovery() {
        let (_area, owner, config) = fixture();
        let intent = applied(&owner, &plan());
        let report = intent.report();
        let name = intent
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        intent.finish(true).unwrap();
        drop(intent);
        let path = closed_path(&config, &owner, &name).unwrap();
        assert_eq!(
            recover_queue(&owner.queue, &config).unwrap()[0].file_dacl_report,
            report
        );
        std::fs::remove_file(path).unwrap();
        assert!(recover_queue(&owner.queue, &config).is_err());
    }
    #[test]
    fn file_dacl_missing_owner_header_or_whole_inventory_never_looks_empty() {
        for all in [false, true] {
            let (_area, owner, config) = fixture();
            let intent = applied(&owner, &plan());
            intent.finish(true).unwrap();
            drop(intent);
            if all {
                std::fs::remove_dir_all(config.path().join(OWNERS)).unwrap();
                std::fs::remove_dir_all(config.path().join(CLOSED)).unwrap();
            } else {
                std::fs::remove_file(owner_path(&config, &owner).unwrap()).unwrap();
            }
            assert!(recover_queue(&owner.queue, &config).is_err());
        }
    }

    #[test]
    fn file_dacl_closed_crash_boundaries_keep_complete_evidence_and_aba() {
        for stage in [
            "after_archive",
            "after_hot_commit",
            "after_owner",
            "after_cleanup",
        ] {
            let (_area, owner, config) = fixture();
            let intent = applied(&owner, &plan());
            let report = intent.report();
            TEST_FAULT.with(|value| value.set(Some(stage)));
            assert!(intent.finish(true).is_err(), "{stage}");
            drop(intent);
            let recovery = recover_queue(&owner.queue, &config).unwrap();
            assert_eq!(recovery[0].file_dacl_report, report, "{stage}");
            if stage == "after_archive" {
                assert!(check_registry(&config, &[identity("/pane")], None).is_err());
                acknowledge(
                    &owner,
                    &Readback::new(true),
                    Path::new("/pane"),
                    [2; 32],
                    &config,
                )
                .unwrap();
            }
            assert_eq!(
                check_registry(&config, &[identity("/pane")], Some(&owner)).unwrap(),
                None
            );
            assert!(check_registry(&config, &[identity("/pane")], None)
                .unwrap()
                .is_some());
            assert_eq!(
                recover_queue(&owner.queue, &config).unwrap()[0].file_dacl_report,
                report
            );
        }
    }
    #[test]
    fn file_dacl_retirement_crash_keeps_namespace_epoch_and_exact_cleanup() {
        for stage in ["after_retire_commit", "after_owner", "after_cleanup"] {
            let (_area, owner, config) = fixture();
            let intent = applied(&owner, &plan());
            let report = intent.report();
            intent.finish(true).unwrap();
            drop(intent);
            let epoch = check_registry(&config, &[identity("/pane")], None).unwrap();
            std::fs::write(config.path().join("unrelated"), b"keep").unwrap();
            TEST_FAULT.with(|value| value.set(Some(stage)));
            assert!(
                checkpoint(&owner, &config, &report, &[], false).is_err(),
                "{stage}"
            );
            assert!(recover_queue(&owner.queue, &config).unwrap().is_empty());
            assert_eq!(
                check_registry(&config, &[identity("/pane")], None).unwrap(),
                epoch
            );
            assert_eq!(
                std::fs::read(config.path().join("unrelated")).unwrap(),
                b"keep"
            );
        }
    }
    #[test]
    fn file_dacl_legacy_v6_migration_preserves_open_closed_and_owner_epoch() {
        let (_area, owner, config) = fixture();
        let intent = applied(&owner, &plan());
        let mut record = intent.record.lock().unwrap().clone();
        drop(intent);
        record.closed = true;
        record.closed_observation = Some(true);
        record.generation += 1;
        let name = ".file-dacl-v6-legacy.json".to_owned();
        let mut open = record.clone();
        open.closed = false;
        open.closed_observation = None;
        open.phase = Phase::Applying;
        open.generation = 1;
        open.file = "/pane/b".into();
        open.plan.file_id = 43;
        open.plan.target_proof = crate::file_dacl::tests::proof("b", 43);
        let open_name = ".file-dacl-v6-open.json".to_owned();
        std::fs::write(lease_path(&config.path().join(&open_name)), []).unwrap();
        let old = LegacyLedger {
            generation: 6,
            records: [(name.clone(), record.clone()), (open_name, open)].into(),
        };
        write_json(
            &config,
            &config.path().join(LEDGER),
            &LegacyEnvelope {
                version: 6,
                digest: digest(b"doppeldecker-file-dacl-ledger-v6\0", &old).unwrap(),
                ledger: old,
            },
            false,
        )
        .unwrap();
        TEST_FAULT.with(|value| value.set(Some("before_migration_commit")));
        assert!(recover_queue(&owner.queue, &config).is_err());
        let recovered = recover_queue(&owner.queue, &config).unwrap();
        assert_eq!(recovered[0].file_dacl_report.completed.len(), 1);
        assert_eq!(recovered[0].file_dacl_report.uncertain.len(), 1);
        assert!(recovered[0].file_dacl_recovery);
        assert!(check_registry(&config, &[identity("/pane")], None).is_err());
        let mut readback = Readback::new(true);
        readback.observed.as_mut().unwrap().file_id = 43;
        acknowledge(&owner, &readback, Path::new("/pane"), [2; 32], &config).unwrap();
        assert_eq!(
            check_registry(&config, &[identity("/pane")], Some(&owner)).unwrap(),
            None
        );
        assert!(check_registry(&config, &[identity("/pane")], None)
            .unwrap()
            .is_some());
        std::fs::remove_file(closed_path(&config, &owner, &name).unwrap()).unwrap();
        assert!(recover_queue(&owner.queue, &config).is_err());
    }
}
