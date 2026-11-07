//! Additiver V7-Intent; alte Neighbor-V1–V5-Formate bleiben unverändert.
//! Jede Phase wird atomar und fsync-bestätigt gespeichert. Geschlossene Vollbelege
//! liegen getrennt vom heißen Ledger und bleiben bis zum dauerhaften Queuecheckpoint.
use crate::created_directory_dacl::{CreatedDirectoryDaclPlan, CreatedDirectoryDaclReport};
use crate::local_fs::RootHandle;
use crate::local_staging::StagingOwner;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const PREFIX: &str = ".created-directory-dacl-v7-";
// Geschlossener RAM-Kontext, niemals ein Feld der alten V7-Envelopes. Beide
// Stores teilen Mechanik und Sperre; Version, Pfade und Digestdomains bleiben getrennt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum StoreKind {
    #[default]
    V7,
    V8,
}
impl StoreKind {
    const ALL: [Self; 2] = [Self::V7, Self::V8];
    fn version(self) -> u32 {
        match self {
            Self::V7 => 7,
            Self::V8 => 8,
        }
    }
    fn origin(self) -> crate::created_directory_dacl::CreatedDirectoryOrigin {
        use crate::created_directory_dacl::CreatedDirectoryOrigin as Origin;
        match self {
            Self::V7 => Origin::ExclusiveMinimalCreate,
            Self::V8 => Origin::ExclusiveMinimalCreateBeforeChildren,
        }
    }
    fn from_origin(origin: crate::created_directory_dacl::CreatedDirectoryOrigin) -> Self {
        use crate::created_directory_dacl::CreatedDirectoryOrigin as Origin;
        match origin {
            Origin::ExclusiveMinimalCreate => Self::V7,
            Origin::ExclusiveMinimalCreateBeforeChildren => Self::V8,
        }
    }
    fn from_version(version: u32) -> io::Result<Self> {
        match version {
            7 => Ok(Self::V7),
            8 => Ok(Self::V8),
            _ => Err(io::Error::other("Unbekannte Neuordner-DACL-Storeversion")),
        }
    }
    fn prefix(self) -> &'static str {
        match self {
            Self::V7 => PREFIX,
            Self::V8 => ".created-directory-dacl-v8-",
        }
    }
    fn ledger(self) -> &'static str {
        match self {
            Self::V7 => LEDGER,
            Self::V8 => ".created-directory-dacl-v8-ledger.json",
        }
    }
    fn owners(self) -> &'static str {
        match self {
            Self::V7 => OWNERS,
            Self::V8 => ".created-directory-dacl-v8-owners",
        }
    }
    fn closed(self) -> &'static str {
        match self {
            Self::V7 => CLOSED,
            Self::V8 => ".created-directory-dacl-v8-closed",
        }
    }
    fn write_prefix(self) -> &'static str {
        match self {
            Self::V7 => ".created-directory-dacl-v7-write-",
            Self::V8 => ".created-directory-dacl-v8-write-",
        }
    }
    fn ledger_domain(self) -> &'static [u8] {
        match self {
            Self::V7 => b"doppeldecker-created-directory-dacl-hot-ledger-v7\0",
            Self::V8 => b"doppeldecker-created-directory-dacl-hot-ledger-v8\0",
        }
    }
    fn key_domain(self) -> &'static [u8] {
        match self {
            Self::V7 => b"doppeldecker-created-directory-dacl-private-key-v7\0",
            Self::V8 => b"doppeldecker-created-directory-dacl-private-key-v8\0",
        }
    }
    fn owner_domain(self) -> &'static [u8] {
        match self {
            Self::V7 => b"doppeldecker-created-directory-dacl-owner-v7\0",
            Self::V8 => b"doppeldecker-created-directory-dacl-owner-v8\0",
        }
    }
    fn closed_domain(self) -> &'static [u8] {
        match self {
            Self::V7 => b"doppeldecker-created-directory-dacl-closed-v7\0",
            Self::V8 => b"doppeldecker-created-directory-dacl-closed-v8\0",
        }
    }
}

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
    scope: crate::created_directory_dacl::CreatedDirectoryRootScope,
    file: PathBuf,
    plan: CreatedDirectoryDaclPlan,
    phase: Phase,
    closed: bool,
    generation: u64,
    closed_observation: Option<bool>,
}
impl Record {
    fn validate(&self, config: &RootHandle) -> io::Result<()> {
        self.plan.validate()?;
        self.scope.validate(&self.root)?;
        let crate::RootIdentity::Smb(root) = &self.root else {
            return Err(io::Error::other("Neuordner-DACL ohne SMB-Root"));
        };
        root.validate()?;
        let kind = StoreKind::from_version(self.version)?;
        if self.plan.origin != kind.origin()
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
            return Err(io::Error::other("Ungültige V7-Neuordner-DACL-Bindung"));
        }
        Ok(())
    }
    fn report(&self) -> CreatedDirectoryDaclReport {
        let mut report = CreatedDirectoryDaclReport::default();
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
        return Err(io::Error::other("Neuordner-DACL benötigt SMB-Namensraum"));
    };
    let mut hash = blake3::Hasher::new();
    hash.update(b"doppeldecker-file-object-namespace-v7\0");
    hash.update(
        &serde_json::to_vec(&(root.server.server_guid, root.server.share.to_lowercase()))
            .map_err(io::Error::other)?,
    );
    Ok(*hash.finalize().as_bytes())
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryScope {
    bound: crate::created_directory_dacl::CreatedDirectoryRootScope,
    display_root: PathBuf,
}
fn display_root(identity: &crate::RootIdentity) -> io::Result<PathBuf> {
    let crate::RootIdentity::Smb(root) = identity else {
        return Err(io::Error::other("V7 benötigt SMB-Root"));
    };
    root.validate()?;
    Ok(Path::new("/")
        .join(root.server.canonical_root.trim_start_matches('/'))
        .join(root.virtual_root.trim_start_matches('/')))
}
fn scope_key(
    root: &crate::RootIdentity,
    bound: &crate::created_directory_dacl::CreatedDirectoryRootScope,
) -> io::Result<String> {
    bound.validate(root)?;
    serde_json::to_string(&DirectoryScope {
        bound: bound.clone(),
        display_root: display_root(root)?,
    })
    .map_err(io::Error::other)
}
fn scopes_overlap(
    left: &crate::created_directory_dacl::CreatedDirectoryRootScope,
    right: &crate::created_directory_dacl::CreatedDirectoryRootScope,
) -> bool {
    left.object_namespace == right.object_namespace
        && (left.root_file_id == right.root_file_id
            || left.ancestor_file_ids.contains(&right.root_file_id)
            || right.ancestor_file_ids.contains(&left.root_file_id))
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
        plan: &CreatedDirectoryDaclPlan,
        root: &crate::RootIdentity,
        account: [u8; 32],
        scope: &crate::created_directory_dacl::CreatedDirectoryRootScope,
    ) -> io::Result<Self> {
        let config = RootHandle::open_nofollow(
            owner
                .queue
                .parent()
                .ok_or_else(|| io::Error::other("Queue ohne Elternordner"))?,
        )
        .map_err(|error| crate::error::with_context(error, "Neuordner-DACL-Queueordner öffnen"))?;
        ensure(&config).map_err(|error| {
            crate::error::with_context(error, "Neuordner-DACL-Queueordner prüfen")
        })?;
        let kind = StoreKind::from_origin(plan.origin);
        let record = Record {
            version: kind.version(),
            owner: owner.clone(),
            endpoint: crate::EndpointBinding::remote(root, account)?,
            root: root.clone(),
            scope: scope.clone(),
            file: file.into(),
            plan: plan.clone(),
            phase: Phase::Prepared,
            closed: false,
            generation: 0,
            closed_observation: None,
        };
        record.validate(&config)?;
        let candidate = crate::atomic_file::sibling_candidate(
            &config
                .path()
                .join(format!("created-directory-dacl-v{}", kind.version())),
            "intent",
        )?;
        let name = format!(
            "{}{}.json",
            kind.prefix(),
            candidate.file_name().unwrap().to_string_lossy()
        );
        let path = config.path().join(name);
        // Lease und erste Ledgerregistrierung teilen denselben Registrylock.
        // Zwei erstmalige Fenster können dadurch keine fremden Orphan-Leases sehen.
        let _registry_lock = registry_lock(&config).map_err(|error| {
            crate::error::with_context(error, "Neuordner-DACL-Registry sperren")
        })?;
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
                crate::error::with_context(error.into(), "Neuordner-DACL-Lease anlegen")
            })?,
        );
        rustix::fs::flock(&lease, rustix::fs::FlockOperation::LockExclusive)?;
        lease.sync_all()?;
        persist_locked(&config, &path, &record, true).map_err(|error| {
            crate::error::with_context(error, "Neuordner-DACL-Intent dauerhaft registrieren")
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
            .map_err(|_| io::Error::other("Neuordner-DACL-Intent gesperrt"))?;
        if record.closed
            || !matches!(
                (record.phase, phase),
                (Phase::Prepared, Phase::Applying) | (Phase::Applying, Phase::Applied)
            )
        {
            return Err(io::Error::other("Ungültige Neuordner-DACL-Phase"));
        }
        let mut next = record.clone();
        next.phase = phase;
        next.generation += 1;
        // Beim Fehlschlag bleibt mindestens Applying erhalten, falls SET schon lief.
        persist(&self.config, &self.path, &next, false)?;
        *record = next;
        Ok(())
    }
    pub(crate) fn report(&self) -> CreatedDirectoryDaclReport {
        self.record
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .report()
    }
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn finish(&self, applied: bool) -> io::Result<()> {
        let mut record = self
            .record
            .lock()
            .map_err(|_| io::Error::other("Neuordner-DACL-Intent gesperrt"))?;
        if record.phase
            != if applied {
                Phase::Applied
            } else {
                Phase::Prepared
            }
        {
            return Err(io::Error::other(
                "Neuordner-DACL-Phase erlaubt keinen Abschluss",
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
    let name = ".created-directory-dacl-v7-registry.lock";
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
            "Neuordner-DACL-Registry verlor ihren privaten Namensraum",
        ));
    }
    Ok(())
}
// Das heiße Ledger enthält keine abgeschlossene Vollplanhistorie. Die Zähler
// bleiben monoton; eigene Beiträge stehen getrennt in privaten Ownerdateien.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    #[serde(skip)]
    kind: StoreKind,
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
    #[serde(skip)]
    kind: StoreKind,
    owner: StagingOwner,
    contributions: std::collections::BTreeMap<String, u64>,
    retained_count: u64,
    retained_digest: [u8; 32],
}
impl OwnerState {
    fn new(owner: &StagingOwner, kind: StoreKind) -> Self {
        Self {
            kind,
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
const LEDGER: &str = ".created-directory-dacl-v7-ledger.json";
const OWNERS: &str = ".created-directory-dacl-v7-owners";
const CLOSED: &str = ".created-directory-dacl-v7-closed";
fn digest(domain: &[u8], value: &impl serde::Serialize) -> io::Result<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    hash.update(domain);
    hash.update(&serde_json::to_vec(value).map_err(io::Error::other)?);
    Ok(*hash.finalize().as_bytes())
}
fn ledger_digest(ledger: &Ledger) -> io::Result<[u8; 32]> {
    digest(ledger.kind.ledger_domain(), ledger)
}
fn key(value: &impl serde::Serialize) -> io::Result<String> {
    store_key(value, StoreKind::V7)
}
fn store_key(value: &impl serde::Serialize, kind: StoreKind) -> io::Result<String> {
    Ok(blake3::Hash::from_bytes(digest(kind.key_domain(), value)?)
        .to_hex()
        .to_string())
}
fn owner_relative(owner: &StagingOwner, kind: StoreKind) -> io::Result<PathBuf> {
    Ok(PathBuf::from(store_key(&owner.queue, kind)?).join(store_key(&owner.operation, kind)?))
}
fn owner_path(config: &RootHandle, owner: &StagingOwner, kind: StoreKind) -> io::Result<PathBuf> {
    Ok(config
        .path()
        .join(kind.owners())
        .join(owner_relative(owner, kind)?)
        .with_extension("json"))
}
fn closed_path(
    config: &RootHandle,
    owner: &StagingOwner,
    name: &str,
    kind: StoreKind,
) -> io::Result<PathBuf> {
    Ok(config
        .path()
        .join(kind.closed())
        .join(owner_relative(owner, kind)?)
        .join(name))
}
fn valid_name(name: &str, kind: StoreKind) -> bool {
    name.starts_with(kind.prefix())
        && name.ends_with(".json")
        && !name.contains('/')
        && name != kind.ledger()
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
    kind: StoreKind,
) -> io::Result<()> {
    ensure(config)?;
    private_parent(config, path)?;
    let vfs = crate::LocalVfs::rooted(config.path())?;
    if crate::Vfs::anchored_identity(&vfs) != Some(config.identity()) {
        return Err(io::Error::other(
            "Neuordner-DACL-Registry wurde ausgetauscht",
        ));
    }
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(io::Error::other(
            "Einzelner V7-Datensatz überschreitet Größenlimit",
        ));
    }
    crate::atomic_write::atomic_write_vfs_with_mode(
        &vfs,
        path,
        &bytes,
        kind.write_prefix(),
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
        return Err(io::Error::other("V7-Datensatz überschreitet Größenlimit"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
fn owner_digest(state: &OwnerState) -> io::Result<[u8; 32]> {
    digest(state.kind.owner_domain(), state)
}
fn load_owner(
    config: &RootHandle,
    owner: &StagingOwner,
    kind: StoreKind,
) -> io::Result<OwnerState> {
    let path = owner_path(config, owner, kind)?;
    let mut envelope: OwnerEnvelope = match read_json(config, &path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(OwnerState::new(owner, kind))
        }
        Err(error) => return Err(error),
    };
    envelope.state.kind = kind;
    if envelope.version != kind.version()
        || envelope.state.owner != *owner
        || envelope.state.owner.queue.parent() != Some(config.path())
        || envelope.digest != owner_digest(&envelope.state)?
        || envelope
            .state
            .contributions
            .values()
            .any(|count| *count == 0)
    {
        return Err(io::Error::other("Beschädigter V7-Ownerzähler"));
    }
    Ok(envelope.state)
}
fn write_owner(config: &RootHandle, state: &OwnerState) -> io::Result<()> {
    write_json(
        config,
        &owner_path(config, &state.owner, state.kind)?,
        &OwnerEnvelope {
            version: state.kind.version(),
            digest: owner_digest(state)?,
            state: state.clone(),
        },
        false,
        state.kind,
    )
}
fn closed_digest(name: &str, record: &Record) -> io::Result<[u8; 32]> {
    digest(
        StoreKind::from_version(record.version)?.closed_domain(),
        &(name, record),
    )
}
fn xor(into: &mut [u8; 32], value: [u8; 32]) {
    for (left, right) in into.iter_mut().zip(value) {
        *left ^= right;
    }
}
fn save_ledger(config: &RootHandle, ledger: &mut Ledger) -> io::Result<()> {
    let kind = ledger.kind;
    ledger.generation = ledger
        .generation
        .checked_add(1)
        .ok_or_else(|| io::Error::other("V7-Generation erschöpft"))?;
    write_json(
        config,
        &config.path().join(kind.ledger()),
        &Envelope {
            version: kind.version(),
            digest: ledger_digest(ledger)?,
            ledger: Ledger {
                kind,
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
        kind,
    )
}
fn remove_exact(config: &RootHandle, path: &Path) -> io::Result<()> {
    // Nur zuvor in unserem dauerhaft geschriebenen Commit benannte Metadaten.
    if !path.starts_with(config.path()) || path == config.path() {
        return Err(io::Error::other("V7-Bereinigung verlässt Registry"));
    }
    match crate::local_fs::remove_file_beneath(config, path) {
        Ok(()) => crate::local_fs::sync_dir_beneath(config, path.parent().unwrap()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
fn finish_pending(config: &RootHandle, ledger: &mut Ledger) -> io::Result<()> {
    let kind = ledger.kind;
    if let Some(pending) = &ledger.pending {
        if pending.state.owner.queue.parent() != Some(config.path()) {
            return Err(io::Error::other("V7-Ownercommit verlässt Queueordner"));
        }
        for path in &pending.remove {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| io::Error::other("Ungültiger V7-Metadatenname"))?;
            let intent = if name.ends_with(".lease") {
                Path::new(name).with_extension("json")
            } else {
                PathBuf::from(name)
            };
            let intent = intent.to_str().unwrap();
            if !valid_name(intent, kind)
                || (path != &config.path().join(name)
                    && path != &closed_path(config, &pending.state.owner, intent, kind)?)
            {
                return Err(io::Error::other("V7-Commit benennt fremde Metadaten"));
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
        return Err(io::Error::other(format!("V7-Testabbruch {_stage}")));
    }
    Ok(())
}
fn load_ledger(
    config: &RootHandle,
    creating: Option<&Path>,
    kind: StoreKind,
) -> io::Result<Ledger> {
    let value: serde_json::Value = match read_json(config, &config.path().join(kind.ledger())) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if crate::local_fs::list_beneath(config, config.path())?
                .iter()
                .any(|entry| {
                    entry.name.starts_with(kind.prefix())
                        && (entry.is_dir
                            || entry.name.ends_with(".json")
                            || entry.name.ends_with(".lease"))
                        && creating.is_none_or(|path| {
                            lease_path(path).file_name().and_then(|name| name.to_str())
                                != Some(entry.name.as_str())
                        })
                })
            {
                return Err(io::Error::other(
                    "V7-Ledger fehlt trotz Neuordner-DACL-Belegen",
                ));
            }
            return Ok(Ledger {
                kind,
                format: 1,
                ..Default::default()
            });
        }
        Err(error) => return Err(error),
    };
    // Auch ein zuvor nur sichtbar gewordener Rename wird erst nach Datei- und
    // Eltern-fsync als dauerhafter lokaler Abschluss verwendet.
    crate::local_fs::open_read_beneath(config, &config.path().join(kind.ledger()))?.sync_all()?;
    crate::local_fs::sync_dir_beneath(config, config.path())?;
    let mut envelope: Envelope = serde_json::from_value(value).map_err(io::Error::other)?;
    // Der Aufrufer bindet den Store vor dem Lesen; JSON darf diesen Kontext
    // weder vorgeben noch über Pending-Ownerdaten auf einen anderen Store wechseln.
    envelope.ledger.kind = kind;
    if let Some(pending) = &mut envelope.ledger.pending {
        pending.state.kind = kind;
    }
    if envelope.version != kind.version()
        || envelope.ledger.format != 1
        || envelope.digest != ledger_digest(&envelope.ledger)?
        || envelope.ledger.generation == 0
    {
        return Err(io::Error::other("Beschädigtes heißes V7-Ledger"));
    }
    let mut ledger = envelope.ledger;
    for (name, record) in &ledger.records {
        if !valid_name(name, kind) || record.closed {
            return Err(io::Error::other("Ungültiger heißer V7-Intent"));
        }
        record.validate(config)?;
        if record.version != kind.version() {
            return Err(io::Error::other(
                "Neuordner-DACL-Record gehört zum anderen Store",
            ));
        }
    }
    if ledger
        .checkpointed
        .iter()
        .any(|name| !ledger.records.contains_key(name))
    {
        return Err(io::Error::other("V7-Checkpoint ohne offenen Intent"));
    }
    finish_pending(config, &mut ledger)?;
    for record in ledger.records.values() {
        let owner = load_owner(config, &record.owner, kind)?;
        let scope = scope_key(&record.root, &record.scope)?;
        if owner.contributions.get(&scope).copied().unwrap_or(0) == 0 {
            return Err(io::Error::other(
                "V7-Ownerzähler eines offenen Intents fehlt",
            ));
        }
    }
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
    let kind = StoreKind::from_version(record.version)?;
    if path.parent() != Some(config.path()) {
        return Err(io::Error::other("Neuordner-DACL-Intent verlässt Registry"));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| valid_name(name, kind))
        .ok_or_else(|| io::Error::other("Ungültiger V7-Intentname"))?;
    let mut ledger = load_ledger(config, create.then_some(path), kind)?;
    if create {
        for other in StoreKind::ALL.into_iter().filter(|other| *other != kind) {
            if load_ledger(config, None, other)?
                .records
                .values()
                .any(|previous| {
                    previous.plan.object_namespace == record.plan.object_namespace
                        && previous.plan.file_id == record.plan.file_id
                })
            {
                return Err(crate::coded_error(
                    io::ErrorKind::WouldBlock,
                    crate::ErrorCode::OutcomeUnclear,
                    "Für dasselbe SMB-Objekt existiert bereits ein offener V7-/V8-Rechteintent",
                ));
            }
        }
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
                "Neuordner-DACL-Intent existiert bereits",
            ));
        }
        let mut state = load_owner(config, &record.owner, kind)?;
        let ns = scope_key(&record.root, &record.scope)?;
        let total = ledger.namespaces.entry(ns.clone()).or_default();
        *total = total
            .checked_add(1)
            .ok_or_else(|| io::Error::other("V7-Namensraumzähler erschöpft"))?;
        let own = state.contributions.entry(ns).or_default();
        *own = own
            .checked_add(1)
            .ok_or_else(|| io::Error::other("V7-Ownerzähler erschöpft"))?;
        ledger.pending = Some(Pending {
            state,
            remove: Vec::new(),
        });
    } else {
        let previous = ledger
            .records
            .get(name)
            .ok_or_else(|| io::Error::other("Neuordner-DACL-Intent verschwand aus Ledger"))?;
        if previous.generation + 1 != record.generation
            || previous.closed
            || previous.owner != record.owner
            || previous.endpoint != record.endpoint
            || previous.root != record.root
            || previous.scope != record.scope
            || previous.file != record.file
            || previous.plan != record.plan
        {
            return Err(io::Error::other(
                "Neuordner-DACL-Intentgeneration oder Bindung änderte sich",
            ));
        }
    }
    if record.closed {
        let checksum = closed_digest(name, record)?;
        let cold = closed_path(config, &record.owner, name, kind)?;
        // Vollständiger dauerhafter Beleg VOR Entfernen aus dem heißen Ledger.
        write_json(
            config,
            &cold,
            &ClosedEnvelope {
                version: kind.version(),
                digest: checksum,
                name: name.into(),
                record: record.clone(),
            },
            false,
            kind,
        )?;
        fault("after_archive")?;
        let mut state = load_owner(config, &record.owner, kind)?;
        state.retained_count = state
            .retained_count
            .checked_add(1)
            .ok_or_else(|| io::Error::other("V7-Belegzähler erschöpft"))?;
        xor(&mut state.retained_digest, checksum);
        let mut remove = vec![path.into(), lease_path(path)];
        ledger.retained_count = ledger
            .retained_count
            .checked_add(1)
            .ok_or_else(|| io::Error::other("V7-Gesamtbelegzähler erschöpft"))?;
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
        write_json(config, path, record, create, kind)?;
    }
    Ok(())
}
fn archived_for_owner(
    config: &RootHandle,
    ledger: &Ledger,
    owner: &StagingOwner,
) -> io::Result<Vec<(PathBuf, Record)>> {
    let kind = ledger.kind;
    let state = load_owner(config, owner, kind)?;
    let directory = config
        .path()
        .join(kind.closed())
        .join(owner_relative(owner, kind)?);
    let listing = match crate::local_fs::list_beneath(config, &directory) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let mut count = 0u64;
    let mut sum = [0; 32];
    let mut result = Vec::new();
    for entry in listing {
        if !valid_name(&entry.name, kind) {
            continue;
        }
        if !entry.is_file || entry.is_symlink {
            return Err(io::Error::other("V7-Closedbeleg ist keine reguläre Datei"));
        }
        let envelope: ClosedEnvelope = read_json(config, &directory.join(&entry.name))?;
        if envelope.version != kind.version()
            || envelope.record.version != kind.version()
            || envelope.name != entry.name
            || envelope.record.owner != *owner
            || !envelope.record.closed
            || envelope.digest != closed_digest(&envelope.name, &envelope.record)?
        {
            return Err(io::Error::other("Beschädigter V7-Closedbeleg"));
        }
        envelope.record.validate(config)?;
        // Ein vor dem heißen Commit geschriebener Closedbeleg ist noch kein
        // Abschluss. Der offene Ledgerrecord bleibt maßgeblich und sperrt.
        if ledger.records.contains_key(&envelope.name) {
            continue;
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| io::Error::other("V7-Belegzähler erschöpft"))?;
        xor(&mut sum, envelope.digest);
        result.push((config.path().join(envelope.name), envelope.record));
    }
    if count != state.retained_count || sum != state.retained_digest {
        return Err(io::Error::other("V7-Closedbeleg fehlt oder änderte sich"));
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
// Ausschließlich unter registry_lock: V7 und V8 müssen dieselbe Beobachtungsgrenze
// besitzen, damit ein Wechsel zwischen beiden Klassen keinen offenen Claim verdeckt.
fn load_all_ledgers(config: &RootHandle) -> io::Result<Vec<Ledger>> {
    StoreKind::ALL
        .into_iter()
        .map(|kind| load_ledger(config, None, kind))
        .collect()
}
fn records_for_owner(
    config: &RootHandle,
    owner: &StagingOwner,
) -> io::Result<Vec<(PathBuf, Record)>> {
    let _lock = registry_lock(config)?;
    let mut records = Vec::new();
    for ledger in load_all_ledgers(config)? {
        records.extend(owner_records_locked(config, &ledger, owner)?);
    }
    Ok(records)
}
pub(crate) fn has_open(owner: &StagingOwner, config: &RootHandle) -> io::Result<bool> {
    let _lock = registry_lock(config)?;
    Ok(load_all_ledgers(config)?
        .iter()
        .any(|ledger| ledger.records.values().any(|record| record.owner == *owner)))
}
/// Die Netzbeobachtungen laufen außerhalb des gemeinsamen Registrylocks.
/// Nur offene Intents benötigen frische gespeicherte Roots; abgeschlossene
/// Epochhistorie begründet keine dauerhafte Recovery-Sperre für gelöschte Roots.
#[cfg(test)]
pub(crate) fn check_registry(
    config: &RootHandle,
    identities: &[crate::RootIdentity],
    owner: Option<&StagingOwner>,
) -> io::Result<Option<[u8; 32]>> {
    check_registry_observed(config, identities, owner, |_| Ok(None), |_| Ok(None))
}
pub(crate) fn check_registry_observed(
    config: &RootHandle,
    identities: &[crate::RootIdentity],
    ignore_owner: Option<&StagingOwner>,
    mut observe: impl FnMut(
        usize,
    ) -> io::Result<
        Option<crate::created_directory_dacl::CreatedDirectoryRootScope>,
    >,
    mut observe_foreign: impl FnMut(
        &crate::RootIdentity,
    ) -> io::Result<
        Option<crate::created_directory_dacl::CreatedDirectoryRootScope>,
    >,
) -> io::Result<Option<[u8; 32]>> {
    let (needed, open_roots) = {
        let _lock = registry_lock(config)?;
        let mut needed = std::collections::BTreeSet::new();
        let mut roots = Vec::new();
        for ledger in load_all_ledgers(config)? {
            for key in ledger.namespaces.keys() {
                let scope: DirectoryScope = serde_json::from_str(key).map_err(io::Error::other)?;
                needed.insert(scope.bound.object_namespace);
            }
            roots.extend(ledger.records.values().map(|record| record.root.clone()));
        }
        (needed, roots)
    };
    let mut selected = Vec::new();
    for (index, identity) in identities.iter().enumerate() {
        if let crate::RootIdentity::Smb(root) = identity {
            let namespace = object_namespace(identity)?;
            let bound = if needed.contains(&namespace) {
                observe(index).ok().flatten()
            } else {
                None
            };
            if let Some(scope) = &bound {
                scope.validate(identity)?;
            }
            selected.push((namespace, root.root_file_id, bound));
        }
    }
    let mut foreign = std::collections::BTreeMap::new();
    for root in open_roots {
        let key = key(&root)?;
        if foreign.contains_key(&key) {
            continue;
        }
        let observed = observe_foreign(&root).ok().flatten();
        if let Some(scope) = &observed {
            scope.validate(&root)?;
        }
        foreign.insert(key, observed);
    }
    let _lock = registry_lock(config)?;
    let ledgers = load_all_ledgers(config)?;
    for ledger in &ledgers {
        for (name, record) in &ledger.records {
            let current = foreign.get(&key(&record.root)?).and_then(Option::as_ref);
            for (namespace, id, selected_scope) in &selected {
                if *namespace != record.scope.object_namespace {
                    continue;
                }
                let known_overlap = *id == record.scope.root_file_id
                    || selected_scope.as_ref().is_some_and(|scope| {
                        scope.ancestor_file_ids.contains(&record.scope.root_file_id)
                    });
                let overlap = match (selected_scope, current) {
                    (Some(selected), Some(current)) => {
                        known_overlap || scopes_overlap(selected, current)
                    }
                    _ => {
                        return Err(crate::error::with_recovery_paths(crate::coded_error(io::ErrorKind::WouldBlock,crate::ErrorCode::OutcomeUnclear,"Dieses SMB-Rootpaar besitzt keinen vollständigen frischen Objekt-/Vorfahrenbeleg für die offene Ordnerrechtewirkung"),&[record.file.to_string_lossy().into_owned(),config.path().join(name).to_string_lossy().into_owned()]));
                    }
                };
                if overlap {
                    // Der blockierte fremde Lauf hat diese Wirkung nicht ausgeführt.
                    // Der Eigentümerbericht bleibt in seiner eigenen Fensterqueue.
                    return Err(crate::error::with_recovery_paths(crate::coded_error(io::ErrorKind::WouldBlock,crate::ErrorCode::OutcomeUnclear,"Offene Rechte eines neu erzeugten Ordners benötigen den bewussten beobachtenden Abschluss in der zugehörigen Queue."),&[record.file.to_string_lossy().into_owned(),config.path().join(name).to_string_lossy().into_owned()]));
                }
            }
        }
    }
    let mut visible_v7 = std::collections::BTreeMap::new();
    let mut visible_v8 = std::collections::BTreeMap::new();
    for ledger in &ledgers {
        let own = ignore_owner
            .map(|owner| load_owner(config, owner, ledger.kind))
            .transpose()?;
        let visible = match ledger.kind {
            StoreKind::V7 => &mut visible_v7,
            StoreKind::V8 => &mut visible_v8,
        };
        for (scope, total) in &ledger.namespaces {
            let bound: DirectoryScope = serde_json::from_str(scope).map_err(io::Error::other)?;
            let overlaps = selected.iter().any(|(namespace, id, observed)| {
                *namespace == bound.bound.object_namespace
                    && (*id == bound.bound.root_file_id
                        || observed
                            .as_ref()
                            .is_none_or(|observed| scopes_overlap(observed, &bound.bound)))
            });
            if !overlaps {
                continue;
            }
            let own = own
                .as_ref()
                .and_then(|state| state.contributions.get(scope))
                .copied()
                .unwrap_or(0);
            let count = total.checked_sub(own).ok_or_else(|| {
                io::Error::other("Neuordner-Ownerzähler überschreitet Namensraumgeneration")
            })?;
            if count != 0 {
                visible.insert(scope.clone(), count);
            }
        }
    }
    // Ohne fremde V8-Beiträge bleibt selbst der alte V7-Epochbeleg bytegleich.
    if visible_v8.is_empty() {
        if visible_v7.is_empty() {
            Ok(None)
        } else {
            Ok(Some(digest(
                b"doppeldecker-created-directory-dacl-registry-epoch-v7\0",
                &visible_v7,
            )?))
        }
    } else {
        Ok(Some(digest(
            b"doppeldecker-created-directory-dacl-registry-epoch-v8\0",
            &(visible_v7, visible_v8),
        )?))
    }
}
/// Nur nach erfolgreich dauerhaft geschriebenem Queuecheckpoint aufrufen.
/// Ein Applied-Intent bleibt bis finish offen; die Markierung erlaubt dann nur
/// das Entfernen genau seines bereits im Journal bestätigten privaten Belegs.
pub(crate) fn checkpoint(
    owner: &StagingOwner,
    config: &RootHandle,
    report: &CreatedDirectoryDaclReport,
    resolved: &[crate::created_directory_dacl::CreatedDirectoryDaclOutcome],
    terminal: bool,
) -> io::Result<()> {
    let _lock = registry_lock(config)?;
    for kind in StoreKind::ALL {
        checkpoint_kind(owner, config, report, resolved, terminal, kind)?;
    }
    Ok(())
}
fn checkpoint_kind(
    owner: &StagingOwner,
    config: &RootHandle,
    report: &CreatedDirectoryDaclReport,
    resolved: &[crate::created_directory_dacl::CreatedDirectoryDaclOutcome],
    terminal: bool,
    kind: StoreKind,
) -> io::Result<()> {
    let mut ledger = load_ledger(config, None, kind)?;
    let kind = ledger.kind;
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
        let mut state = load_owner(config, owner, kind)?;
        state.retained_count = state
            .retained_count
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("V7-Belegzähler unterläuft"))?;
        xor(&mut state.retained_digest, checksum);
        ledger.retained_count = ledger
            .retained_count
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("V7-Gesamtbelegzähler unterläuft"))?;
        ledger.pending = Some(Pending {
            state,
            remove: vec![
                closed_path(config, owner, name, kind)?,
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
    let kind = ledger.kind;
    let directory = config.path().join(kind.owners());
    let queues = match crate::local_fs::list_beneath(config, &directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let mut retained = 0u64;
    for queue in queues {
        if !queue.is_dir || queue.is_symlink {
            return Err(io::Error::other("V7-Ownerinventar besitzt fremden Eintrag"));
        }
        let folder = directory.join(&queue.name);
        for entry in crate::local_fs::list_beneath(config, &folder)? {
            if entry.name.starts_with('.') || !entry.name.ends_with(".json") {
                continue;
            }
            let envelope: OwnerEnvelope = read_json(config, &folder.join(&entry.name))?;
            if owner_path(config, &envelope.state.owner, kind)? != folder.join(&entry.name) {
                return Err(io::Error::other("V7-Ownerinventar besitzt fremde Bindung"));
            }
            let state = load_owner(config, &envelope.state.owner, kind)?;
            retained = retained
                .checked_add(state.retained_count)
                .ok_or_else(|| io::Error::other("V7-Gesamtbelegzähler erschöpft"))?;
        }
    }
    if retained != ledger.retained_count {
        return Err(io::Error::other(
            "V7-Ownerbeleg fehlt; Recovery darf nicht als leer gelten",
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
        let mut all_records = Vec::new();
        for ledger in load_all_ledgers(config)? {
            let kind = ledger.kind;
            verify_owner_inventory(config, &ledger)?;
            let directory = config
                .path()
                .join(kind.owners())
                .join(store_key(&queue, kind)?);
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
                    || owner_path(config, &envelope.state.owner, kind)?
                        != directory.join(&entry.name)
                {
                    return Err(io::Error::other(
                        "V7-Ownerdatei besitzt fremde Queuebindung",
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
            all_records.extend(records);
        }
        all_records
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
                created_directory_dacl_report: Default::default(),
                created_directory_dacl_recovery: false,
                before_children_recovery: Default::default(),
                created_directory_dacl_acknowledged: true,
                created_directory_dacl_acknowledged_paths: Vec::new(),
                created_directory_dacl_resolved: Vec::new(),
                file_dacl_report: Default::default(),
                file_dacl_recovery: false,
                file_dacl_acknowledged: false,
                file_dacl_acknowledged_paths: Vec::new(),
                file_dacl_resolved: Vec::new(),
            });
        recovery
            .created_directory_dacl_report
            .merge(&record.report());
        recovery.created_directory_dacl_acknowledged &= record.closed;
        recovery.created_directory_dacl_recovery |= !record.closed;
        if record.plan.origin == crate::created_directory_dacl::CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren {
            use crate::created_directory_dacl::BeforeChildrenRecovery;
            let presence = if !record.closed || record.phase != Phase::Prepared {
                BeforeChildrenRecovery::Pending
            } else {
                BeforeChildrenRecovery::Historical
            };
            recovery.before_children_recovery = recovery.before_children_recovery.max(presence);
        }
        let paths = [
            record.file.to_string_lossy().into_owned(),
            path.to_string_lossy().into_owned(),
        ];
        if record.closed {
            recovery
                .created_directory_dacl_resolved
                .push(record.plan.outcome(&record.file.to_string_lossy())?);
            recovery
                .created_directory_dacl_acknowledged_paths
                .extend(paths);
        } else {
            recovery.paths.extend(paths);
        }
    }
    Ok(map.into_values().collect())
}

pub(crate) struct Acknowledgement {
    pub(crate) report: CreatedDirectoryDaclReport,
    pub(crate) paths: Vec<String>,
    pub(crate) resolved: Vec<crate::created_directory_dacl::CreatedDirectoryDaclOutcome>,
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
                    "Neuordner-DACL-Writer ist noch aktiv",
                )
            },
        )?;
        if record.endpoint != endpoint
            || !crate::local_staging::same_recovery_root(&record.root, &identity)
        {
            return Err(crate::real_mode::plan_drift(
                "Neuordner-DACL-Recoveryendpoint änderte sich",
            ));
        }
        let observed = vfs.created_directory_dacl_recovery_observation(&record.file, &identity)?;
        if observed.file_id != record.plan.file_id
            || observed.object_namespace != record.plan.object_namespace
            || observed.context != record.plan.context
        {
            return Err(crate::real_mode::plan_drift(
                "Neuordner-DACL-Recoveryobjekt oder SID-Kontext änderte sich",
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
                "Neuordner-DACL-Recovery sieht keinen zum Intent passenden Vorher-/Nachherzustand",
            ));
        }
        checked.push((path, record, after, lease));
    }
    if checked.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Kein offener Neuordner-DACL-Intent",
        ));
    }
    let mut report = CreatedDirectoryDaclReport::default();
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
    report = CreatedDirectoryDaclReport::default();
    let mut resolved = Vec::new();
    for (path, record) in records_for_owner(config, owner)?
        .into_iter()
        .filter(|(_, record)| record.owner == *owner)
    {
        report.merge(&record.report());
        resolved.push(record.plan.outcome(&record.file.to_string_lossy())?);
        if !record.closed {
            return Err(crate::error::with_created_directory_dacl_report(
                crate::coded_error(
                    io::ErrorKind::WouldBlock,
                    crate::ErrorCode::OutcomeUnclear,
                    "Während des Neuordner-DACL-Abschlusses entstand ein weiterer offener Intent",
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
#[path = "created_directory_dacl_registry_tests.rs"]
mod tests;
