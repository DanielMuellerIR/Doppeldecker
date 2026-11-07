//! Verifizierte lokale Momentaufnahmen regulärer Remote-Dateien.
//!
//! Jede Datei liegt in einem eigenen app-verwalteten Verzeichnis mit Manifest.
//! Cleanup fasst ausschließlich exakt manifestierte Dateien an und löscht keine
//! fremden Tempinhalte oder noch jungen Momentaufnahmen.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::local_fs::{self, RootHandle};
use crate::local_read::FileVersion;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::with_context;
use crate::{Cancel, LocalVfs, Vfs};

const MANIFEST: &str = ".doppeldecker-snapshot.json";
const MANIFEST_VERSION: u32 = 1;
const SNAPSHOT_PREFIX: &str = "snapshot-";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
pub const SNAPSHOT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Passive Darstellung und bewusstes Öffnen haben unterschiedliche Formatwirkungen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotOpenMode {
    #[default]
    Passive,
    OriginalFormat,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotVerification {
    #[default]
    Streaming,
    FullReadback,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotOptions {
    pub open_mode: SnapshotOpenMode,
    pub verification: SnapshotVerification,
}

/// Originalbeleg für einen späteren ausdrücklich ausgelösten Rückupload.
/// Endpunkt und Quellpfad hält der Aufrufer zusätzlich ausschließlich im Backend.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RemoteOriginalProof {
    pub bytes: u64,
    pub blake3: String,
    pub mtime: Option<i64>,
    pub mode: Option<u32>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RemoteSnapshot {
    pub path: String,
    pub bytes: u64,
    pub blake3: String,
    pub created_unix_ms: u64,
    pub original: RemoteOriginalProof,
}

#[derive(serde::Deserialize, serde::Serialize, Clone)]
#[serde(deny_unknown_fields)]
struct SnapshotManifest {
    version: u32,
    created_unix_ms: u64,
    file_name: String,
    bytes: u64,
    blake3: String,
}

/// Persistenter, pfadfreier Beleg. Erst ein bekannter Store löst ihn als Lesequelle auf.
/// Die Referenz ist untrusted Queueinhalt: Ihre ID ist genau ein begrenzter Name,
/// ihr Manifestdigest beschreibt alle Manifestfelder. Sie erlaubt kein Öffnen
/// außerhalb des getrennt gehaltenen Stores und ersetzt keinen frischen Dateihash.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotReference {
    id: String,
    manifest_digest: [u8; 32],
    bytes: u64,
    blake3: String,
    created_unix_ms: u64,
}
impl SnapshotReference {
    pub fn validate(&self) -> io::Result<()> {
        if !self.id.strip_prefix(SNAPSHOT_PREFIX).is_some_and(|nonce| {
            nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) || self.manifest_digest == [0; 32]
            || self.blake3.len() != 64
            || !self.blake3.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(snapshot_drift());
        }
        Ok(())
    }
}
/// Ausschließlich vom Runtimekontext geöffnet; niemals Teil einer Queueabsicht.
#[derive(Clone)]
pub struct SnapshotStore {
    root: RootHandle,
}
/// Der gehaltene Datei- und Manifestbeleg ist die einzige interne Archivquelle.
#[derive(Clone)]
pub struct ValidatedSnapshot {
    pub(crate) lease: std::sync::Arc<SnapshotLease>,
    reference: SnapshotReference,
    store: SnapshotStore,
}
impl ValidatedSnapshot {
    pub fn reference(&self) -> &SnapshotReference {
        &self.reference
    }
    pub(crate) fn reopen(&self, cancel: &Cancel) -> io::Result<Self> {
        self.store.resolve(&self.reference, cancel)
    }
    pub(crate) fn source_parts(&self) -> io::Result<(RootHandle, File, String)> {
        self.lease.ensure_current()?;
        // try_clone teilt den Unix-Dateicursor. Der Archivreader benutzt deshalb
        // ausschließlich read_at; er übernimmt exakt diesen geprüften FD.
        Ok((
            self.lease.directory.clone(),
            self.lease.snapshot.try_clone()?,
            self.lease.file_name.clone(),
        ))
    }
}
impl SnapshotStore {
    pub fn open(root: &Path) -> io::Result<Self> {
        // Der bekannte Runtimecache darf wie beim Download verlagert sein.
        // Nur diesen Root kanonisieren; untrusted Snapshotkinder werden danach
        // weiterhin descriptor-relativ ohne Symlinkfolgen geöffnet.
        Ok(Self {
            root: RootHandle::open(root)?,
        })
    }
    pub fn capture(
        &self,
        snapshot: &RemoteSnapshot,
        cancel: &Cancel,
    ) -> io::Result<ValidatedSnapshot> {
        let path = Path::new(&snapshot.path);
        let directory = path.parent().ok_or_else(snapshot_drift)?;
        if directory.parent() != Some(self.root.path()) {
            return Err(snapshot_drift());
        }
        let id = directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(snapshot_drift)?;
        let (manifest, lease) = self.open_lease(id, cancel)?;
        if path.file_name().and_then(|name| name.to_str()) != Some(&manifest.file_name)
            || manifest.bytes != snapshot.bytes
            || manifest.blake3 != snapshot.blake3
            || manifest.created_unix_ms != snapshot.created_unix_ms
        {
            return Err(snapshot_drift());
        }
        let reference = SnapshotReference {
            id: id.into(),
            manifest_digest: manifest_digest(&manifest)?,
            bytes: manifest.bytes,
            blake3: manifest.blake3,
            created_unix_ms: manifest.created_unix_ms,
        };
        reference.validate()?;
        Ok(ValidatedSnapshot {
            lease: std::sync::Arc::new(lease),
            reference,
            store: self.clone(),
        })
    }
    pub fn resolve(
        &self,
        reference: &SnapshotReference,
        cancel: &Cancel,
    ) -> io::Result<ValidatedSnapshot> {
        reference.validate()?;
        let (manifest, lease) = self.open_lease(&reference.id, cancel).map_err(|error| {
            crate::error::with_context(
                error,
                "Momentaufnahme ist nicht mehr verfügbar oder verändert; Archiv erneut öffnen",
            )
        })?;
        if manifest_digest(&manifest)? != reference.manifest_digest
            || manifest.bytes != reference.bytes
            || manifest.blake3 != reference.blake3
            || manifest.created_unix_ms != reference.created_unix_ms
        {
            return Err(snapshot_drift());
        }
        Ok(ValidatedSnapshot {
            lease: std::sync::Arc::new(lease),
            reference: reference.clone(),
            store: self.clone(),
        })
    }
    fn open_lease(
        &self,
        id: &str,
        cancel: &Cancel,
    ) -> io::Result<(SnapshotManifest, SnapshotLease)> {
        if !id.strip_prefix(SNAPSHOT_PREFIX).is_some_and(|nonce| {
            nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) || !self.root.is_current()?
        {
            return Err(snapshot_drift());
        }
        let directory = RootHandle::open_beneath(&self.root, &self.root.path().join(id))?;
        let manifest_file =
            local_fs::open_read_beneath(&directory, &directory.path().join(MANIFEST))?;
        rustix::fs::flock(
            &manifest_file,
            rustix::fs::FlockOperation::NonBlockingLockShared,
        )?;
        let (manifest, manifest_version) = read_bound_manifest(&directory, &manifest_file)?;
        let (snapshot, snapshot_version) = validate_snapshot(&directory, &manifest, cancel)?;
        let lease = SnapshotLease {
            manifest: manifest_file,
            manifest_version,
            snapshot,
            snapshot_version,
            file_name: manifest.file_name.clone(),
            directory,
            cache: Some(self.root.clone()),
        };
        lease.ensure_current()?;
        Ok((manifest, lease))
    }
}
fn manifest_digest(manifest: &SnapshotManifest) -> io::Result<[u8; 32]> {
    Ok(*blake3::hash(&serde_json::to_vec(manifest).map_err(io::Error::other)?).as_bytes())
}

/// Hält Manifestlock und gebundenen Laufordner bis zum letzten Leserbesitzer.
/// Der Lease gehört zusammen mit dem Reader in jeden laufenden Inhaltsworker.
pub struct SnapshotLease {
    manifest: File,
    manifest_version: std::fs::Metadata,
    snapshot: File,
    snapshot_version: std::fs::Metadata,
    file_name: String,
    directory: RootHandle,
    cache: Option<RootHandle>,
}

impl RemoteSnapshot {
    pub fn lease(&self, cancel: &Cancel) -> io::Result<SnapshotLease> {
        let path = Path::new(&self.path);
        let directory = RootHandle::open_nofollow(path.parent().ok_or_else(snapshot_drift)?)?;
        let manifest_file =
            local_fs::open_read_beneath(&directory, &directory.path().join(MANIFEST))?;
        rustix::fs::flock(
            &manifest_file,
            rustix::fs::FlockOperation::NonBlockingLockShared,
        )?;
        let (manifest, manifest_version) = read_bound_manifest(&directory, &manifest_file)?;
        if path.file_name().and_then(|name| name.to_str()) != Some(&manifest.file_name)
            || manifest.bytes != self.bytes
            || manifest.blake3 != self.blake3
            || manifest.created_unix_ms != self.created_unix_ms
        {
            return Err(snapshot_drift());
        }
        let (snapshot, snapshot_version) = validate_snapshot(&directory, &manifest, cancel)?;
        let lease = SnapshotLease {
            manifest: manifest_file,
            manifest_version,
            snapshot,
            snapshot_version,
            file_name: manifest.file_name,
            directory,
            cache: None,
        };
        lease.ensure_current()?;
        Ok(lease)
    }
}

impl SnapshotLease {
    /// Nach dem Öffnen eines Readers die Übergabe an exakt die geprüfte Datei binden.
    pub fn ensure_current(&self) -> io::Result<()> {
        let visible_manifest =
            local_fs::open_read_beneath(&self.directory, &self.directory.path().join(MANIFEST))?;
        let visible_snapshot = local_fs::open_read_beneath(
            &self.directory,
            &self.directory.path().join(&self.file_name),
        )?;
        if let Some(cache) = &self.cache {
            if !cache.is_current()? {
                return Err(snapshot_drift());
            }
        }
        if !self.directory.is_current()?
            || !same_file(&self.manifest_version, &self.manifest.metadata()?)
            || !same_file(&self.manifest_version, &visible_manifest.metadata()?)
            || !same_file(&self.snapshot_version, &self.snapshot.metadata()?)
            || !same_file(&self.snapshot_version, &visible_snapshot.metadata()?)
        {
            return Err(snapshot_drift());
        }
        Ok(())
    }
}

fn snapshot_drift() -> io::Error {
    crate::coded_error(
        io::ErrorKind::InvalidData,
        crate::ErrorCode::PlanDrift,
        "Momentaufnahme oder Manifest änderte sich; Archiv erneut öffnen",
    )
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    FileVersion::of(left) == FileVersion::of(right)
}

fn read_bound_manifest(
    directory: &RootHandle,
    file: &File,
) -> io::Result<(SnapshotManifest, std::fs::Metadata)> {
    let before = file.metadata()?;
    if !before.is_file() || before.len() > MAX_MANIFEST_BYTES {
        return Err(snapshot_drift());
    }
    let mut payload = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut payload)?;
    let visible = local_fs::open_read_beneath(directory, &directory.path().join(MANIFEST))?;
    if payload.len() as u64 > MAX_MANIFEST_BYTES
        || !same_file(&before, &file.metadata()?)
        || !same_file(&before, &visible.metadata()?)
        || !directory.is_current()?
    {
        return Err(snapshot_drift());
    }
    let manifest: SnapshotManifest = serde_json::from_slice(&payload).map_err(io::Error::other)?;
    if manifest.version != MANIFEST_VERSION
        || !safe_manifest_name(&manifest.file_name)
        || manifest.file_name == MANIFEST
    {
        return Err(snapshot_drift());
    }
    Ok((manifest, before))
}

fn validate_snapshot(
    directory: &RootHandle,
    manifest: &SnapshotManifest,
    cancel: &Cancel,
) -> io::Result<(File, std::fs::Metadata)> {
    let path = directory.path().join(&manifest.file_name);
    let mut file = local_fs::open_read_beneath(directory, &path)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() != manifest.bytes {
        return Err(snapshot_drift());
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; 1024 * 1024];
    let mut stream = (&mut file).take(manifest.bytes.saturating_add(1));
    loop {
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Momentaufnahmeprüfung wurde abgebrochen",
            ));
        }
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let visible = local_fs::open_read_beneath(directory, &path)?;
    if hasher.finalize().to_hex().as_str() != manifest.blake3
        || !same_file(&before, &file.metadata()?)
        || !same_file(&before, &visible.metadata()?)
        || !directory.is_current()?
    {
        return Err(snapshot_drift());
    }
    Ok((file, before))
}

pub fn download_remote_snapshot(
    vfs: &dyn Vfs,
    source: &Path,
    cache_root: &Path,
    cancel: &Cancel,
) -> io::Result<RemoteSnapshot> {
    download_remote_snapshot_with_options(
        vfs,
        source,
        cache_root,
        cancel,
        SnapshotOptions::default(),
    )
}

pub fn download_remote_snapshot_with_options(
    vfs: &dyn Vfs,
    source: &Path,
    cache_root: &Path,
    cancel: &Cancel,
    options: SnapshotOptions,
) -> io::Result<RemoteSnapshot> {
    download_remote_snapshot_bounded(vfs, source, cache_root, cancel, options, u64::MAX)
}

/// Die Vorschau begrenzt den vollständigen Transfer bereits vor Cacheanlage und OPEN.
/// Derselbe Größenbeleg begrenzt anschließend auch einen wachsenden Serverstrom.
pub(crate) fn download_remote_snapshot_bounded(
    vfs: &dyn Vfs,
    source: &Path,
    cache_root: &Path,
    cancel: &Cancel,
    options: SnapshotOptions,
    maximum_bytes: u64,
) -> io::Result<RemoteSnapshot> {
    let entry = vfs
        .stat(source)
        .map_err(|error| with_context(error, "Quellprüfung"))?;
    if !entry.is_file || entry.is_symlink {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Momentaufnahme benötigt eine reguläre Datei ohne Symlink",
        ));
    }
    if entry.size > maximum_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Die integrierte Vorschau liest höchstens 8 MiB. Die Datei kann mit der System-App geöffnet werden.",
        ));
    }
    prepare_cache_root(cache_root)?;
    let cache_root = std::fs::canonicalize(cache_root)?;
    let available = LocalVfs::new()
        .available_bytes(&cache_root)
        .map_err(|error| with_context(error, "Lokale Speicherprüfung"))?;
    if available < entry.size.saturating_add(1024 * 1024) {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "Nicht genügend lokaler Speicher für die Remote-Momentaufnahme",
        ));
    }

    let (run_dir, nonce) = reserve_run_dir(&cache_root)?;
    let file_name = if options.open_mode == SnapshotOpenMode::Passive {
        format!("remote-{nonce}{}", passive_extension(source))
    } else {
        // Der private Laufordner ist bereits eindeutig. Der Originalname erhält
        // auch lange Endungen, ohne durch einen Zufallspräfix NAME_MAX zu sprengen.
        source
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| {
                safe_manifest_name(name) && *name != MANIFEST && !name.chars().any(char::is_control)
            })
            .map(str::to_owned)
            .unwrap_or_else(|| format!("remote-{nonce}{}", safe_extension(source)))
    };
    let file_path = run_dir.join(&file_name);
    let result = (|| -> io::Result<RemoteSnapshot> {
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&file_path)?;
        // Ein zusätzliches Byte erkennt Überlänge, ohne einen unbeschränkten
        // Serverstrom erst vollständig auf das lokale Volume zu schreiben.
        let mut source_stream = vfs
            .open_read(source)
            .map_err(|error| with_context(error, "Remote-Stream"))?
            .take(entry.size.saturating_add(1));
        let mut hasher = blake3::Hasher::new();
        let mut copied = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            if cancel.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Remote-Momentaufnahme wurde abgebrochen",
                ));
            }
            let read = source_stream.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let next = copied.saturating_add(read as u64);
            if next > entry.size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Remote-Dateistrom überschreitet die angekündigte Größe",
                ));
            }
            target.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            copied = next;
        }
        target.sync_all()?;
        drop(target);
        if copied != entry.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Remote-Dateigröße änderte sich während des Downloads",
            ));
        }
        let final_entry = vfs.stat(source)?;
        if !final_entry.is_file
            || final_entry.is_symlink
            || final_entry.size != entry.size
            || final_entry.mtime != entry.mtime
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Remote-Datei oder Provider-Token änderte sich während des Downloads",
            ));
        }
        let source_hash = hasher.finalize();
        if options.verification == SnapshotVerification::FullReadback {
            let verified_hash = hash_remote(vfs, source, entry.size, cancel)?;
            let verified_entry = vfs.stat(source)?;
            if verified_hash != source_hash
                || !verified_entry.is_file
                || verified_entry.is_symlink
                || verified_entry.size != entry.size
                || verified_entry.mtime != entry.mtime
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Remote-BLAKE3-Readback oder Providerbindung änderte sich",
                ));
            }
        }
        let local_hash = hash_local(&file_path)?;
        if source_hash != local_hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "BLAKE3-Readback der lokalen Momentaufnahme stimmt nicht",
            ));
        }
        let created_unix_ms = now_unix_ms()?;
        let manifest = SnapshotManifest {
            version: MANIFEST_VERSION,
            created_unix_ms,
            file_name,
            bytes: copied,
            blake3: source_hash.to_hex().to_string(),
        };
        write_manifest(&run_dir.join(MANIFEST), &manifest)?;
        File::open(&run_dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| with_context(error, "Momentaufnahme-Verzeichnis-Sync"))?;
        Ok(RemoteSnapshot {
            path: file_path.to_string_lossy().into_owned(),
            bytes: copied,
            blake3: manifest.blake3,
            created_unix_ms,
            original: RemoteOriginalProof {
                bytes: copied,
                blake3: source_hash.to_hex().to_string(),
                mtime: entry.mtime,
                mode: entry.mode,
            },
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&file_path);
        let _ = std::fs::remove_file(run_dir.join(MANIFEST));
        let _ = std::fs::remove_dir(&run_dir);
    }
    result
}

/// Prüft vor einem gesonderten Rückupload den ursprünglichen Serverinhalt frisch.
/// Das Ergebnis ist kein atomarer Replace-Permit: Der anschließende Schreibweg
/// muss Zielversion und Eltern nochmals binden und die Ersetzung bestätigen.
pub fn verify_remote_original(
    vfs: &dyn Vfs,
    source: &Path,
    original: &RemoteOriginalProof,
    cancel: &Cancel,
) -> io::Result<()> {
    let conflict = || {
        crate::coded_error(io::ErrorKind::InvalidData, crate::ErrorCode::PlanDrift,
        "Remote-Datei wurde seit dem Öffnen verändert; Rückupload benötigt eine neue Konfliktentscheidung")
    };
    let before = vfs.stat(source)?;
    if !before.is_file
        || before.is_symlink
        || before.size != original.bytes
        || original
            .mtime
            .is_some_and(|mtime| before.mtime != Some(mtime))
        || original.mode.is_some_and(|mode| before.mode != Some(mode))
    {
        return Err(conflict());
    }
    let hash = hash_remote(vfs, source, original.bytes, cancel)?;
    let after = vfs.stat(source)?;
    if hash.to_hex().as_str() != original.blake3
        || !after.is_file
        || after.is_symlink
        || after.size != original.bytes
        || original
            .mtime
            .is_some_and(|mtime| after.mtime != Some(mtime))
        || original.mode.is_some_and(|mode| after.mode != Some(mode))
    {
        return Err(conflict());
    }
    Ok(())
}

/// Entfernt nur alte, vollständig validierte eigene Momentaufnahmen. Ein fremder
/// Eintrag, ein unbekanntes Manifest oder ein zusätzliches Verzeichnisobjekt bleibt.
pub fn cleanup_remote_snapshots(
    cache_root: &Path,
    now: SystemTime,
    retention: Duration,
) -> io::Result<u64> {
    match std::fs::symlink_metadata(cache_root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    prepare_cache_root(cache_root)?;
    let cache_root = std::fs::canonicalize(cache_root)?;
    let cache = RootHandle::open_nofollow(&cache_root)?;
    let cutoff = now
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis()
        .saturating_sub(retention.as_millis());
    let mut removed = 0_u64;
    for child in std::fs::read_dir(&cache_root)? {
        let child = child?;
        let name = child.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(SNAPSHOT_PREFIX) || !child.file_type()?.is_dir() {
            continue;
        }
        let run_dir = child.path();
        let directory = match RootHandle::open_beneath(&cache, &run_dir) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let manifest_file = match local_fs::open_read_beneath(&directory, &run_dir.join(MANIFEST)) {
            Ok(file) => file,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
                ) =>
            {
                continue
            }
            Err(error)
                if error.raw_os_error() == Some(libc::ELOOP)
                    || error.kind() == io::ErrorKind::Unsupported =>
            {
                // Nur den belegten unzulässigen Kandidatentyp überspringen.
                // Ein echtes ENOTSUP einer regulären Datei bleibt ein I/O-Fehler.
                match rustix::fs::statat(
                    directory.fd(),
                    MANIFEST,
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                ) {
                    Ok(stat) if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() => {
                        continue
                    }
                    Err(rustix::io::Errno::NOENT) => continue,
                    Err(stat_error) => return Err(stat_error.into()),
                    _ => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        match rustix::fs::flock(
            &manifest_file,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        ) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => continue,
            Err(error) => return Err(error.into()),
        }
        let Ok((manifest, manifest_version)) = read_bound_manifest(&directory, &manifest_file)
        else {
            continue;
        };
        if u128::from(manifest.created_unix_ms) > cutoff {
            continue;
        }
        let entries = local_fs::list_beneath(&directory, &run_dir)?;
        if entries.len() != 2 {
            continue;
        }
        let Ok((snapshot_file, snapshot_version)) =
            validate_snapshot(&directory, &manifest, &Cancel::new())
        else {
            continue;
        };
        let snapshot_path = run_dir.join(&manifest.file_name);
        let visible_snapshot = local_fs::open_read_beneath(&directory, &snapshot_path)?;
        let visible_manifest = local_fs::open_read_beneath(&directory, &run_dir.join(MANIFEST))?;
        if !same_file(&manifest_version, &manifest_file.metadata()?)
            || !same_file(&manifest_version, &visible_manifest.metadata()?)
            || !same_file(&snapshot_version, &snapshot_file.metadata()?)
            || !same_file(&snapshot_version, &visible_snapshot.metadata()?)
            || !directory.is_current()?
        {
            continue;
        }
        // Der exklusive Manifestlock bleibt bis nach allen Löschentscheidungen
        // gehalten. Ein aktiver Reader besitzt dagegen einen geteilten Lock.
        local_fs::remove_file_beneath(&directory, &snapshot_path)?;
        local_fs::remove_file_beneath(&directory, &run_dir.join(MANIFEST))?;
        local_fs::remove_dir_beneath(&cache, &run_dir)?;
        removed += 1;
    }
    Ok(removed)
}

pub(crate) fn prepare_cache_root(cache_root: &Path) -> io::Result<()> {
    std::fs::create_dir_all(cache_root)?;
    let metadata = std::fs::symlink_metadata(cache_root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Momentaufnahme-Cache ist kein echtes Verzeichnis",
        ));
    }
    std::fs::set_permissions(cache_root, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub(crate) fn reserve_run_dir(cache_root: &Path) -> io::Result<(PathBuf, String)> {
    for _ in 0..100 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| io::Error::other(format!("Zufallsquelle: {error}")))?;
        let nonce = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = cache_root.join(format!("{SNAPSHOT_PREFIX}{nonce}"));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
                return Ok((path, nonce));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Kein kollisionsfreier Momentaufnahme-Ordner verfügbar",
    ))
}

fn safe_extension(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 16
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
        .map(|value| format!(".{value}"))
        .unwrap_or_default()
}

fn passive_extension(path: &Path) -> String {
    // LaunchServices kann etwa .command an ein Terminal übergeben, obwohl der
    // Snapshot keine Ausführungsbits besitzt. Die neue SFTP-Klasse öffnet deshalb
    // nur ausgewählte Dokument-/Medienendungen. HTML/SVG können Skripte, XML
    // externe Stylesheets starten; sie und unbekannter Inhalt gehen als Text an
    // die System-App. Das ersetzt keine Sandbox des jeweiligen Dokumentbetrachters.
    // Remote-Namen werden weiterhin vollständig durch Nonces ersetzt.
    let extension = safe_extension(path);
    match extension.to_ascii_lowercase().as_str() {
        ".txt" | ".md" | ".csv" | ".tsv" | ".log" | ".json" | ".yaml" | ".yml" | ".toml"
        | ".ini" | ".conf" | ".css" | ".rs" | ".c" | ".h" | ".pdf" | ".rtf" | ".docx" | ".xlsx"
        | ".pptx" | ".odt" | ".ods" | ".odp" | ".png" | ".jpg" | ".jpeg" | ".gif" | ".webp"
        | ".avif" | ".bmp" | ".tif" | ".tiff" | ".heic" | ".mp3" | ".wav" | ".flac" | ".ogg"
        | ".m4a" | ".aac" | ".aiff" | ".mp4" | ".mov" | ".mkv" | ".webm" | ".avi" => extension,
        _ => ".txt".into(),
    }
}

fn safe_manifest_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.contains('/') && !matches!(name, "." | "..")
}

fn hash_local(path: &Path) -> io::Result<blake3::Hash> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize())
}

fn hash_remote(
    vfs: &dyn Vfs,
    path: &Path,
    expected_size: u64,
    cancel: &Cancel,
) -> io::Result<blake3::Hash> {
    let mut source = vfs.open_read(path)?.take(expected_size.saturating_add(1));
    let mut copied = 0_u64;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Remote-Momentaufnahme wurde beim Readback abgebrochen",
            ));
        }
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied = copied.saturating_add(read as u64);
        if copied > expected_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Remote-Readback überschreitet die angekündigte Größe",
            ));
        }
        hasher.update(&buffer[..read]);
    }
    if copied != expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Remote-Readback endet vor der angekündigten Größe",
        ));
    }
    Ok(hasher.finalize())
}

fn write_manifest(path: &Path, manifest: &SnapshotManifest) -> io::Result<()> {
    let payload = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(&payload)?;
    file.sync_all()
}

fn now_unix_ms() -> io::Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    u64::try_from(millis).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, FileTypeExt};

    struct OversizedSnapshotVfs {
        inner: LocalVfs,
        oversized_on_open: usize,
        opens: std::sync::atomic::AtomicUsize,
        transferred: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    struct CountedSnapshotStream {
        content: std::io::Cursor<Vec<u8>>,
        transferred: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Read for CountedSnapshotStream {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read = self.content.read(buffer)?;
            self.transferred
                .fetch_add(read, std::sync::atomic::Ordering::SeqCst);
            Ok(read)
        }
    }

    impl Vfs for OversizedSnapshotVfs {
        fn list(&self, path: &Path) -> io::Result<Vec<crate::Entry>> {
            self.inner.list(path)
        }
        fn stat(&self, path: &Path) -> io::Result<crate::Entry> {
            self.inner.stat(path)
        }
        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }
        fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
            if self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
                == self.oversized_on_open
            {
                Ok(Box::new(CountedSnapshotStream {
                    content: std::io::Cursor::new(vec![b'x'; 64 * 1024]),
                    transferred: self.transferred.clone(),
                }))
            } else {
                self.inner.open_read(path)
            }
        }
        fn create_dir_all(&self, _: &Path) -> io::Result<()> {
            unreachable!("rein lesende Snapshotquelle")
        }
        fn create_dir(&self, _: &Path) -> io::Result<()> {
            unreachable!("rein lesende Snapshotquelle")
        }
        fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
            unreachable!("rein lesende Snapshotquelle")
        }
        fn open_write_new(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
            unreachable!("rein lesende Snapshotquelle")
        }
        fn sync_file(&self, _: &Path) -> io::Result<()> {
            unreachable!("rein lesende Snapshotquelle")
        }
        fn sync_dir(&self, _: &Path) -> io::Result<()> {
            unreachable!("rein lesende Snapshotquelle")
        }
        fn set_mtime(&self, _: &Path, _: i64) -> io::Result<()> {
            unreachable!("rein lesende Snapshotquelle")
        }
        fn set_mode(&self, _: &Path, _: u32) -> io::Result<()> {
            unreachable!("rein lesende Snapshotquelle")
        }
        fn remove_file(&self, _: &Path) -> io::Result<()> {
            unreachable!("rein lesende Snapshotquelle")
        }
        fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
            unreachable!("rein lesende Snapshotquelle")
        }
        fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
            unreachable!("rein lesende Snapshotquelle")
        }
        fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
            unreachable!("rein lesende Snapshotquelle")
        }
    }

    #[test]
    fn geteilte_leases_schuetzen_bis_zum_letzten_besitzer_vor_cleanup() {
        let source = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let path = source.path().join("a.zip");
        std::fs::write(&path, b"snapshot").unwrap();
        let snapshot = download_remote_snapshot(
            &LocalVfs::new(),
            &path.canonicalize().unwrap(),
            cache.path(),
            &Cancel::new(),
        )
        .unwrap();
        let first = snapshot.lease(&Cancel::new()).unwrap();
        let second = std::sync::Arc::new(snapshot.lease(&Cancel::new()).unwrap());
        let worker = second.clone();
        let future = SystemTime::now() + SNAPSHOT_RETENTION + Duration::from_secs(1);
        assert_eq!(
            cleanup_remote_snapshots(cache.path(), future, SNAPSHOT_RETENTION).unwrap(),
            0
        );
        drop(first);
        drop(second);
        assert_eq!(
            cleanup_remote_snapshots(cache.path(), future, SNAPSHOT_RETENTION).unwrap(),
            0
        );
        assert!(Path::new(&snapshot.path).is_file());
        worker.ensure_current().unwrap();
        drop(worker);
        assert_eq!(
            cleanup_remote_snapshots(cache.path(), future, SNAPSHOT_RETENTION).unwrap(),
            1
        );
    }

    #[test]
    fn lease_lehnt_manifest_und_snapshotdrift_sowie_abbruch_ab() {
        let source = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let path = source.path().join("a.zip");
        std::fs::write(&path, b"snapshot").unwrap();
        let snapshot = download_remote_snapshot(
            &LocalVfs::new(),
            &path.canonicalize().unwrap(),
            cache.path(),
            &Cancel::new(),
        )
        .unwrap();
        let cancelled = Cancel::new();
        cancelled.cancel();
        assert_eq!(
            snapshot.lease(&cancelled).err().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
        let lease = snapshot.lease(&Cancel::new()).unwrap();
        std::fs::write(&snapshot.path, b"modified").unwrap();
        assert_eq!(
            crate::error_code(&lease.ensure_current().err().unwrap()),
            crate::ErrorCode::PlanDrift
        );
        assert_eq!(
            crate::error_code(&snapshot.lease(&Cancel::new()).err().unwrap()),
            crate::ErrorCode::PlanDrift
        );
        let future = SystemTime::now() + SNAPSHOT_RETENTION + Duration::from_secs(1);
        drop(lease);
        assert_eq!(
            cleanup_remote_snapshots(cache.path(), future, SNAPSHOT_RETENTION).unwrap(),
            0
        );
        assert!(Path::new(&snapshot.path).is_file());
        let manifest = Path::new(&snapshot.path).parent().unwrap().join(MANIFEST);
        std::fs::write(&manifest, b"{}").unwrap();
        assert!(snapshot.lease(&Cancel::new()).is_err());
        assert_eq!(
            cleanup_remote_snapshots(cache.path(), future, SNAPSHOT_RETENTION).unwrap(),
            0
        );
    }

    #[test]
    fn snapshot_download_und_readback_begrenzen_den_stream_auf_die_quellgroesse() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        for oversized_on_open in [1, 2] {
            let source = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            let path = source.path().join("bericht.txt");
            std::fs::write(&path, b"abc").unwrap();
            let path = path.canonicalize().unwrap();
            let transferred = Arc::new(AtomicUsize::new(0));
            let vfs = OversizedSnapshotVfs {
                inner: LocalVfs::new(),
                oversized_on_open,
                opens: AtomicUsize::new(0),
                transferred: transferred.clone(),
            };

            let error = download_remote_snapshot_with_options(
                &vfs,
                &path,
                cache.path(),
                &Cancel::new(),
                SnapshotOptions {
                    verification: SnapshotVerification::FullReadback,
                    ..SnapshotOptions::default()
                },
            )
            .unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(vfs.opens.load(Ordering::SeqCst), oversized_on_open);
            assert!(std::fs::read_dir(cache.path()).unwrap().next().is_none());
            assert!(transferred.load(Ordering::SeqCst) <= 4,
                "Drei angekündigte Bytes erlauben höchstens ein zusätzliches Prüfbyte, auch im Readback; Stream {oversized_on_open} lieferte {}", transferred.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn originalformat_erhaelt_lange_endungen_und_reserviert_den_manifestnamen() {
        let source = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        for name in [format!("x.{}", "a".repeat(253)), MANIFEST.to_owned()] {
            let path = source.path().join(&name);
            std::fs::write(&path, b"Originalinhalt").unwrap();
            let snapshot = download_remote_snapshot_with_options(
                &LocalVfs::new(),
                &path.canonicalize().unwrap(),
                cache.path(),
                &Cancel::new(),
                SnapshotOptions {
                    open_mode: SnapshotOpenMode::OriginalFormat,
                    ..SnapshotOptions::default()
                },
            )
            .unwrap();
            assert_eq!(std::fs::read(&snapshot.path).unwrap(), b"Originalinhalt");
            assert!(Path::new(&snapshot.path).file_name().unwrap().len() <= 255);
            if name != MANIFEST {
                assert_eq!(
                    Path::new(&snapshot.path)
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    name
                );
            }
        }
        assert_eq!(
            cleanup_remote_snapshots(
                cache.path(),
                SystemTime::now() + SNAPSHOT_RETENTION + Duration::from_secs(1),
                SNAPSHOT_RETENTION
            )
            .unwrap(),
            2
        );
    }

    #[test]
    fn snapshot_kontext_erhaelt_driftcode_und_strukturierte_recoverypfade() {
        let error = crate::error::with_recovery_paths(
            crate::coded_error(
                io::ErrorKind::InvalidData,
                crate::ErrorCode::PlanDrift,
                "geänderter Quellbeleg",
            ),
            &["/synthetic/recovery".into()],
        );
        let error = with_context(error, "Quellprüfung");
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(
            crate::error::recovery_paths(&error),
            ["/synthetic/recovery"]
        );
    }

    #[test]
    fn momentaufnahme_verifiziert_rechte_und_raeumt_nur_manifestierte_altdaten() {
        let source = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let source_file = source.path().join("bericht.txt");
        std::fs::write(&source_file, b"Momentaufnahme\n").unwrap();
        let source_file = std::fs::canonicalize(source_file).unwrap();
        let snapshot =
            download_remote_snapshot(&LocalVfs::new(), &source_file, cache.path(), &Cancel::new())
                .unwrap();
        assert_eq!(std::fs::read(&snapshot.path).unwrap(), b"Momentaufnahme\n");
        assert_eq!(
            std::fs::metadata(&snapshot.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let foreign = cache.path().join("snapshot-fremd");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("nicht-von-uns"), b"bleibt").unwrap();
        let outside = cache.path().join("ausserhalb");
        std::fs::write(&outside, b"fremd").unwrap();
        let linked = cache.path().join("snapshot-verlinkt");
        std::fs::create_dir(&linked).unwrap();
        symlink(&outside, linked.join("remote-link")).unwrap();
        write_manifest(
            &linked.join(MANIFEST),
            &SnapshotManifest {
                version: MANIFEST_VERSION,
                created_unix_ms: 1,
                file_name: "remote-link".into(),
                bytes: 5,
                blake3: blake3::hash(b"fremd").to_hex().to_string(),
            },
        )
        .unwrap();
        let future = SystemTime::now() + Duration::from_secs(20 * 24 * 60 * 60);
        assert_eq!(
            cleanup_remote_snapshots(cache.path(), future, SNAPSHOT_RETENTION).unwrap(),
            1
        );
        assert!(!Path::new(&snapshot.path).exists());
        assert!(foreign.join("nicht-von-uns").exists());
        assert!(linked.join("remote-link").is_symlink());
        assert_eq!(std::fs::read(outside).unwrap(), b"fremd");
    }

    #[test]
    fn cleanup_ueberspringt_symlink_und_spezialmanifest_neben_gueltigem_snapshot() {
        let source = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let source_file = source.path().join("a.zip");
        std::fs::write(&source_file, b"snapshot").unwrap();
        let snapshot = download_remote_snapshot(
            &LocalVfs::new(),
            &source_file.canonicalize().unwrap(),
            cache.path(),
            &Cancel::new(),
        )
        .unwrap();
        let foreign = source.path().join("foreign.json");
        std::fs::write(&foreign, b"fremder Inhalt bleibt erhalten").unwrap();
        let linked = cache.path().join("snapshot-symlink-manifest");
        std::fs::create_dir(&linked).unwrap();
        symlink(&foreign, linked.join(MANIFEST)).unwrap();
        let special = cache.path().join("snapshot-fifo-manifest");
        std::fs::create_dir(&special).unwrap();
        use std::os::unix::ffi::OsStrExt;
        let fifo = std::ffi::CString::new(special.join(MANIFEST).as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let future = SystemTime::now() + SNAPSHOT_RETENTION + Duration::from_secs(1);
        assert_eq!(
            cleanup_remote_snapshots(cache.path(), future, SNAPSHOT_RETENTION).unwrap(),
            1
        );
        assert!(!Path::new(&snapshot.path).exists());
        assert_eq!(
            std::fs::read(&foreign).unwrap(),
            b"fremder Inhalt bleibt erhalten"
        );
        assert!(linked.join(MANIFEST).is_symlink());
        assert!(std::fs::symlink_metadata(special.join(MANIFEST))
            .unwrap()
            .file_type()
            .is_fifo());
    }

    #[test]
    fn abbruch_hinterlaesst_weder_datei_noch_manifest() {
        let source = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let source_file = source.path().join("gross.bin");
        std::fs::write(&source_file, vec![7_u8; 2 * 1024 * 1024]).unwrap();
        let source_file = std::fs::canonicalize(source_file).unwrap();
        let cancel = Cancel::new();
        cancel.cancel();
        let error = download_remote_snapshot(&LocalVfs::new(), &source_file, cache.path(), &cancel)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(std::fs::read_dir(cache.path()).unwrap().next().is_none());
    }

    #[test]
    fn cache_symlink_wird_vor_rechteaenderung_abgelehnt() {
        let parent = tempfile::tempdir().unwrap();
        let outside = parent.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o751)).unwrap();
        let cache_link = parent.path().join("cache-link");
        symlink(&outside, &cache_link).unwrap();

        let error = cleanup_remote_snapshots(&cache_link, SystemTime::now(), SNAPSHOT_RETENTION)
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }
    #[test]
    fn store_referenz_ist_pfadfrei_gebunden_und_lease_schuetzt_cleanup() {
        let source = tempfile::tempdir().unwrap();
        let source_path = source.path().canonicalize().unwrap().join("archive.zip");
        std::fs::write(&source_path, b"artificial content").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let snapshot =
            download_remote_snapshot(&LocalVfs::new(), &source_path, cache.path(), &Cancel::new())
                .unwrap();
        let store = SnapshotStore::open(&cache.path().canonicalize().unwrap()).unwrap();
        let validated = store.capture(&snapshot, &Cancel::new()).unwrap();
        let reference = validated.reference().clone();
        let json = serde_json::to_string(&reference).unwrap();
        assert!(!json.contains(&snapshot.path));
        let future = SystemTime::now() + SNAPSHOT_RETENTION + Duration::from_secs(1);
        assert_eq!(
            cleanup_remote_snapshots(cache.path(), future, SNAPSHOT_RETENTION).unwrap(),
            0
        );
        let reopened = store.resolve(&reference, &Cancel::new()).unwrap();
        drop(validated);
        assert_eq!(
            cleanup_remote_snapshots(cache.path(), future, SNAPSHOT_RETENTION).unwrap(),
            0
        );
        drop(reopened);
        assert_eq!(
            cleanup_remote_snapshots(cache.path(), future, SNAPSHOT_RETENTION).unwrap(),
            1
        );
        assert!(store.resolve(&reference, &Cancel::new()).is_err());
    }
    #[test]
    fn store_referenz_sperrt_traversal_symlinks_manifest_und_inhaltsdrift() {
        use std::os::unix::fs::symlink;
        let source = tempfile::tempdir().unwrap();
        let source_path = source.path().canonicalize().unwrap().join("archive.zip");
        std::fs::write(&source_path, b"artificial content").unwrap();
        for attack in ["id", "run_symlink", "file_symlink", "manifest", "content"] {
            let cache = tempfile::tempdir().unwrap();
            let snapshot = download_remote_snapshot(
                &LocalVfs::new(),
                &source_path,
                cache.path(),
                &Cancel::new(),
            )
            .unwrap();
            let store = SnapshotStore::open(&cache.path().canonicalize().unwrap()).unwrap();
            let validated = store.capture(&snapshot, &Cancel::new()).unwrap();
            let mut reference = validated.reference().clone();
            let run = Path::new(&snapshot.path).parent().unwrap();
            match attack {
                "id" => reference.id = "snapshot-../../outside".into(),
                "run_symlink" => {
                    let renamed = cache.path().join("saved");
                    std::fs::rename(run, &renamed).unwrap();
                    symlink(renamed, run).unwrap();
                }
                "file_symlink" => {
                    std::fs::remove_file(&snapshot.path).unwrap();
                    symlink(&source_path, &snapshot.path).unwrap();
                }
                "manifest" => {
                    let mut manifest: SnapshotManifest =
                        serde_json::from_slice(&std::fs::read(run.join(MANIFEST)).unwrap())
                            .unwrap();
                    manifest.created_unix_ms += 1;
                    std::fs::write(run.join(MANIFEST), serde_json::to_vec(&manifest).unwrap())
                        .unwrap();
                }
                "content" => std::fs::write(&snapshot.path, b"substituted bytes").unwrap(),
                _ => unreachable!(),
            }
            assert!(
                store.resolve(&reference, &Cancel::new()).is_err(),
                "{attack}"
            );
            if attack != "id" {
                assert!(validated.lease.ensure_current().is_err(), "{attack}");
            }
        }
    }
    #[test]
    fn runtimecache_mit_symlink_eltern_bindet_capture_und_reference_reopen() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("real/cache")).unwrap();
        symlink(root.join("real"), root.join("alias")).unwrap();
        let runtime_cache = root.join("alias/cache");
        let source = root.join("archive.zip");
        std::fs::write(&source, b"artificial snapshot content").unwrap();
        let downloaded =
            download_remote_snapshot(&LocalVfs::new(), &source, &runtime_cache, &Cancel::new())
                .unwrap();
        assert!(Path::new(&downloaded.path).starts_with(root.join("real/cache")));
        let store = SnapshotStore::open(&runtime_cache).unwrap();
        let validated = store.capture(&downloaded, &Cancel::new()).unwrap();
        let persisted = serde_json::to_vec(validated.reference()).unwrap();
        drop(validated);
        drop(store);
        let reference: SnapshotReference = serde_json::from_slice(&persisted).unwrap();
        let reopened = SnapshotStore::open(&runtime_cache)
            .unwrap()
            .resolve(&reference, &Cancel::new())
            .unwrap();
        assert_eq!(reopened.reference(), &reference);
        reopened.lease.ensure_current().unwrap();
    }
}
