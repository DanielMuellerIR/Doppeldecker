//! Verifizierte lokale Momentaufnahmen regulärer Remote-Dateien.
//!
//! Jede Datei liegt in einem eigenen app-verwalteten Verzeichnis mit Manifest.
//! Cleanup fasst ausschließlich exakt manifestierte Dateien an und löscht keine
//! fremden Tempinhalte oder noch jungen Momentaufnahmen.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::with_context;
use crate::{Cancel, LocalVfs, Vfs};

const MANIFEST: &str = ".doppeldecker-snapshot.json";
const MANIFEST_VERSION: u32 = 1;
const SNAPSHOT_PREFIX: &str = "snapshot-";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
pub const SNAPSHOT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone, Debug, serde::Serialize)]
pub struct RemoteSnapshot {
    pub path: String,
    pub bytes: u64,
    pub blake3: String,
    pub created_unix_ms: u64,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct SnapshotManifest {
    version: u32,
    created_unix_ms: u64,
    file_name: String,
    bytes: u64,
    blake3: String,
}

pub fn download_remote_snapshot(
    vfs: &dyn Vfs,
    source: &Path,
    cache_root: &Path,
    cancel: &Cancel,
) -> io::Result<RemoteSnapshot> {
    download_snapshot(vfs, source, cache_root, cancel, false)
}

pub(crate) fn download_passive_remote_snapshot(
    vfs: &dyn Vfs,
    source: &Path,
    cache_root: &Path,
    cancel: &Cancel,
) -> io::Result<RemoteSnapshot> {
    download_snapshot(vfs, source, cache_root, cancel, true)
}

fn download_snapshot(
    vfs: &dyn Vfs,
    source: &Path,
    cache_root: &Path,
    cancel: &Cancel,
    passive_name: bool,
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
    let extension = if passive_name {
        passive_extension(source)
    } else {
        safe_extension(source)
    };
    let file_name = format!("remote-{nonce}{extension}");
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
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&file_path);
        let _ = std::fs::remove_file(run_dir.join(MANIFEST));
        let _ = std::fs::remove_dir(&run_dir);
    }
    result
}

/// Entfernt nur alte, vollständig validierte eigene Momentaufnahmen. Ein fremder
/// Eintrag, ein unbekanntes Manifest oder ein zusätzliches Verzeichnisobjekt bleibt.
pub fn cleanup_remote_snapshots(
    cache_root: &Path,
    now: SystemTime,
    retention: Duration,
) -> io::Result<u64> {
    if !cache_root.exists() {
        return Ok(0);
    }
    prepare_cache_root(cache_root)?;
    let cutoff = now
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis()
        .saturating_sub(retention.as_millis());
    let mut removed = 0_u64;
    for child in std::fs::read_dir(cache_root)? {
        let child = child?;
        let name = child.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(SNAPSHOT_PREFIX) || !child.file_type()?.is_dir() {
            continue;
        }
        let run_dir = child.path();
        let Ok(manifest) = read_manifest(&run_dir.join(MANIFEST)) else {
            continue;
        };
        if u128::from(manifest.created_unix_ms) > cutoff || !safe_manifest_name(&manifest.file_name)
        {
            continue;
        }
        let declared = run_dir.join(&manifest.file_name);
        let entries: Vec<_> = std::fs::read_dir(&run_dir)?.collect::<Result<_, _>>()?;
        let declared_metadata = std::fs::symlink_metadata(&declared);
        if entries.len() != 2
            || !matches!(
                declared_metadata,
                Ok(ref metadata)
                    if metadata.is_file() && !metadata.file_type().is_symlink()
            )
            || !run_dir.join(MANIFEST).is_file()
            || hash_local(&declared)?.to_hex().to_string() != manifest.blake3
        {
            continue;
        }
        std::fs::remove_file(&declared)?;
        std::fs::remove_file(run_dir.join(MANIFEST))?;
        std::fs::remove_dir(&run_dir)?;
        removed += 1;
    }
    Ok(removed)
}

fn prepare_cache_root(cache_root: &Path) -> io::Result<()> {
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

fn reserve_run_dir(cache_root: &Path) -> io::Result<(PathBuf, String)> {
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
    !name.is_empty() && name.len() <= 128 && !name.contains('/') && !matches!(name, "." | "..")
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

fn read_manifest(path: &Path) -> io::Result<SnapshotManifest> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_MANIFEST_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Momentaufnahme-Manifest ist ungültig",
        ));
    }
    let payload = std::fs::read(path)?;
    let manifest: SnapshotManifest = serde_json::from_slice(&payload).map_err(io::Error::other)?;
    if manifest.version != MANIFEST_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unbekannte Momentaufnahme-Manifestversion",
        ));
    }
    Ok(manifest)
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
    use std::os::unix::fs::symlink;

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

            let error =
                download_remote_snapshot(&vfs, &path, cache.path(), &Cancel::new()).unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(vfs.opens.load(Ordering::SeqCst), oversized_on_open);
            assert!(std::fs::read_dir(cache.path()).unwrap().next().is_none());
            assert!(transferred.load(Ordering::SeqCst) <= 4,
                "Drei angekündigte Bytes erlauben höchstens ein zusätzliches Prüfbyte, auch im Readback; Stream {oversized_on_open} lieferte {}", transferred.load(Ordering::SeqCst));
        }
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
}
