//! Favoriten (Schnellzugriff-Ordner) für die Seitenleiste.
//!
//! Ein Favorit ist ein benannter Ordner-Endpoint: lokal genügt ein absoluter Pfad,
//! remote kommen Providerart und stabile Profil-ID hinzu. Zugangsdaten werden nie
//! gespeichert. Die Liste liegt als JSON in der App-Konfiguration (den konkreten
//! Ablage-Pfad bestimmt die App-Schicht, damit dieser Kern GUI-/plattformfrei und
//! headless testbar bleibt — wie der Rest von `core`).
//!
//! Favoriten löschen keine Nutzerdaten. Ihre Ablage darf trotzdem weder bei einem
//! Absturz verstümmelt noch bei zwei gleichzeitigen Änderungen unbemerkt überschrieben
//! werden. Deshalb laufen Änderungen unter einem Prozess-Lock und werden atomar
//! geschrieben; beschädigtes JSON bleibt samt Recovery-Kopie erhalten.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::atomic_write::{atomic_write_local, AtomicWriteMode};

const MAX_FAVORITES_BYTES: u64 = 1024 * 1024;
const FAVORITES_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FavoriteProvider {
    #[default]
    Local,
    Sftp,
    Ftps,
    Webdav,
    Smb,
}

/// Ein einzelner Favorit: Anzeigename in der Seitenleiste + absoluter Ordner-Pfad.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Favorite {
    /// Frei wählbarer Anzeigename (Default in der UI: der Ordnername).
    pub name: String,
    /// Absoluter Pfad des Ordners.
    pub path: String,
    /// Providerart und stabile Profil-ID reichen zum frischen Wiederverbinden.
    #[serde(default)]
    pub provider: FavoriteProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

impl Favorite {
    pub fn local(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            provider: FavoriteProvider::Local,
            profile_id: None,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FavoriteStore {
    version: u32,
    favorites: Vec<Favorite>,
}

/// Favoriten aus der JSON-Ablage `store` lesen.
///
/// Eine fehlende Datei bedeutet eine leere Liste. Lese- und Parsefehler bleiben dagegen
/// sichtbar, damit eine spätere Änderung keinen beschädigten Bestand überschreibt.
pub fn load(store: &Path) -> io::Result<Vec<Favorite>> {
    load_state(store).map(|(favorites, _)| favorites)
}

fn load_state(store: &Path) -> io::Result<(Vec<Favorite>, bool)> {
    let mut file = match open_store_read(store) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), false)),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_FAVORITES_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Favoriten-Ablage ist keine reguläre Datei oder zu groß",
        ));
    }
    let mut payload = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_FAVORITES_BYTES + 1)
        .read_to_end(&mut payload)?;
    let parsed = serde_json::from_slice::<FavoriteStore>(&payload)
        .map(|document| {
            if document.version != FAVORITES_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Favoriten-Version {} wird nicht unterstützt",
                        document.version
                    ),
                ));
            }
            validate_favorites(&document.favorites)?;
            Ok((document.favorites, false))
        })
        .unwrap_or_else(|document_error| {
            serde_json::from_slice::<Vec<Favorite>>(&payload)
                .map_err(io::Error::other)
                .and_then(|favorites| {
                    validate_favorites(&favorites)?;
                    Ok((favorites, true))
                })
                .map_err(|legacy_error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{document_error}; Altformat: {legacy_error}"),
                    )
                })
        });
    match parsed {
        Ok(value) => Ok(value),
        Err(error) => {
            let recovery = preserve_corrupt_store(store, &payload);
            let suffix = match recovery {
                Ok(path) => format!("; Recovery-Kopie: {}", path.display()),
                Err(copy_error) => format!("; Recovery-Kopie fehlgeschlagen: {copy_error}"),
            };
            Err(io::Error::new(
                error.kind(),
                format!("Favoriten-Ablage ist beschädigt: {error}{suffix}"),
            ))
        }
    }
}

/// Initialisiert eine fehlende Ablage unter demselben Lock wie normale Änderungen.
/// Ein parallel bereits angelegter Bestand gewinnt und wird unverändert geladen.
pub fn load_or_initialize(store: &Path, initial: &[Favorite]) -> io::Result<Vec<Favorite>> {
    let _lock = acquire_process_lock(store)?;
    match open_store_read(store) {
        Ok(_) => {
            let (favorites, legacy) = load_state(store)?;
            if legacy {
                save_locked(store, &favorites)?;
            }
            Ok(favorites)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            save_locked(store, initial)?;
            Ok(initial.to_vec())
        }
        Err(error) => Err(error),
    }
}

fn open_store_read(store: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Ein FIFO darf nicht schon vor der anschließenden Dateitypprüfung warten.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options.open(store)
}

/// Führt Read-modify-write als eine flock-geschützte Transaktion aus. Die Mutation
/// meldet, ob sie den Bestand geändert hat; reine No-ops schreiben nicht neu.
pub fn update(
    store: &Path,
    mutate: impl FnOnce(&mut Vec<Favorite>) -> bool,
) -> io::Result<Vec<Favorite>> {
    let _lock = acquire_process_lock(store)?;
    let (mut favorites, legacy) = load_state(store)?;
    if mutate(&mut favorites) || legacy {
        save_locked(store, &favorites)?;
    }
    Ok(favorites)
}

fn save_locked(store: &Path, favorites: &[Favorite]) -> io::Result<()> {
    validate_favorites(favorites)?;
    let document = FavoriteStore {
        version: FAVORITES_VERSION,
        favorites: favorites.to_vec(),
    };
    let mut payload = serde_json::to_vec_pretty(&document).map_err(io::Error::other)?;
    payload.push(b'\n');
    // Dieselbe Grenze wie beim Laden: Ein Bestand, der sich noch erfolgreich
    // speichern ließe, aber ab dem nächsten Zugriff als „zu groß" abgelehnt
    // würde, hätte die eigene Ablage dauerhaft unlesbar gemacht. Deshalb wird
    // die Mutation abgelehnt, bevor irgendetwas geschrieben ist.
    if payload.len() as u64 > MAX_FAVORITES_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Favoritenbestand überschreitet die Ablagegrenze; Änderung wurde nicht gespeichert",
        ));
    }
    atomic_write_local(store, &payload, ".favorites-", AtomicWriteMode::Replace)
}

/// Zerlegt den Ablagepfad in Elternordner und Dateinamen.
///
/// Beide Teile braucht jede Nebendatei der Favoriten — Sperrdatei und
/// Recovery-Kopie liegen bewusst neben der Ablage, nicht in ihr.
fn store_parent_and_name(store: &Path) -> io::Result<(&Path, std::borrow::Cow<'_, str>)> {
    let parent = store.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Favoriten-Ablage braucht einen Elternordner",
        )
    })?;
    let name = store
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Favoriten-Dateiname fehlt"))?
        .to_string_lossy();
    Ok((parent, name))
}

fn acquire_process_lock(store: &Path) -> io::Result<File> {
    let (parent, name) = store_parent_and_name(store)?;
    std::fs::create_dir_all(parent)?;
    let lock_path = parent.join(format!(".{name}.lock"));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(lock_path)?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).map_err(io::Error::from)?;
    Ok(lock)
}

fn corrupt_recovery_path(store: &Path) -> io::Result<PathBuf> {
    let (parent, name) = store_parent_and_name(store)?;
    Ok(parent.join(format!("{name}.corrupt-recovery")))
}

fn preserve_corrupt_store(store: &Path, payload: &[u8]) -> io::Result<PathBuf> {
    let recovery = corrupt_recovery_path(store)?;
    match atomic_write_local(
        &recovery,
        payload,
        ".favorites-corrupt-",
        AtomicWriteMode::Create,
    ) {
        Ok(()) => Ok(recovery),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(recovery),
        Err(error) => Err(error),
    }
}

/// Einen Ordner als Favorit ergänzen — aber nur, wenn der Pfad noch nicht enthalten ist
/// (Deduplizierung nach Pfad). Gibt `true` zurück, wenn wirklich ergänzt wurde.
pub fn add(favs: &mut Vec<Favorite>, name: &str, path: &str) -> bool {
    add_endpoint(favs, name, FavoriteProvider::Local, None, path)
}

pub fn add_endpoint(
    favs: &mut Vec<Favorite>,
    name: &str,
    provider: FavoriteProvider,
    profile_id: Option<&str>,
    path: &str,
) -> bool {
    if favs.iter().any(|favorite| {
        favorite.provider == provider
            && favorite.profile_id.as_deref() == profile_id
            && favorite.path == path
    }) {
        return false;
    }
    favs.push(Favorite {
        name: name.to_string(),
        path: path.to_string(),
        provider,
        profile_id: profile_id.map(str::to_owned),
    });
    true
}

/// Den Favoriten mit genau diesem Pfad entfernen. Gibt `true` zurück, wenn einer wegfiel.
pub fn remove(favs: &mut Vec<Favorite>, path: &str) -> bool {
    remove_endpoint(favs, FavoriteProvider::Local, None, path)
}

pub fn remove_endpoint(
    favs: &mut Vec<Favorite>,
    provider: FavoriteProvider,
    profile_id: Option<&str>,
    path: &str,
) -> bool {
    let before = favs.len();
    favs.retain(|favorite| {
        favorite.provider != provider
            || favorite.profile_id.as_deref() != profile_id
            || favorite.path != path
    });
    favs.len() != before
}

/// Den Favoriten mit diesem Pfad an die Zielposition `index` verschieben (Drag-Sortierung
/// in der Seitenleiste). Ein zu großer Index landet am Ende; ein unbekannter Pfad ändert
/// nichts. Gibt `true` zurück, wenn sich die Reihenfolge wirklich geändert hat.
pub fn move_to(favs: &mut Vec<Favorite>, path: &str, index: usize) -> bool {
    move_endpoint(favs, FavoriteProvider::Local, None, path, index)
}

pub fn move_endpoint(
    favs: &mut Vec<Favorite>,
    provider: FavoriteProvider,
    profile_id: Option<&str>,
    path: &str,
    index: usize,
) -> bool {
    let Some(from) = favs.iter().position(|favorite| {
        favorite.provider == provider
            && favorite.profile_id.as_deref() == profile_id
            && favorite.path == path
    }) else {
        return false;
    };
    let to = index.min(favs.len().saturating_sub(1));
    if from == to {
        return false;
    }
    let favorite = favs.remove(from);
    favs.insert(to, favorite);
    true
}

fn validate_favorites(favorites: &[Favorite]) -> io::Result<()> {
    for favorite in favorites {
        if favorite.name.trim().is_empty()
            || !Path::new(&favorite.path).is_absolute()
            || favorite.path.contains('\0')
            || (favorite.provider == FavoriteProvider::Local && favorite.profile_id.is_some())
            || (favorite.provider != FavoriteProvider::Local
                && favorite.profile_id.as_deref().is_none_or(str::is_empty))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Favorit besitzt ungültige Provider-, Profil- oder Pfaddaten",
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (headless auf macOS UND Linux)
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "wird nur vom isolierten FIFO-Gegenlauf mit eigener Markierung gestartet"]
    fn favorites_fifo_child() {
        let root = PathBuf::from(std::env::var_os("DOPPELDECKER_FAVORITES_FIFO_ROOT").unwrap());
        assert!(root
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("doppeldecker-favorites-fifo-"));
        assert_eq!(
            std::fs::read(root.join("marker")).unwrap(),
            b"favorites-fifo-v1"
        );
        std::fs::write(root.join("ready"), b"ready").unwrap();
        let error = load(&root.join("favorites.json")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn favorites_fifo_wird_ohne_wartenden_schreiber_abgelehnt() {
        use std::os::unix::ffi::OsStrExt;
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};
        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }
        let area = tempfile::Builder::new()
            .prefix("doppeldecker-favorites-fifo-")
            .tempdir()
            .unwrap();
        std::fs::write(area.path().join("marker"), b"favorites-fifo-v1").unwrap();
        let fifo =
            std::ffi::CString::new(area.path().join("favorites.json").as_os_str().as_bytes())
                .unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let name = "favorites::tests::favorites_fifo_child";
        let binary = std::env::current_exe().unwrap();
        let list = Command::new(&binary)
            .args(["--list", "--exact", name])
            .output()
            .unwrap();
        assert!(list.status.success());
        assert_eq!(
            String::from_utf8(list.stdout)
                .unwrap()
                .lines()
                .filter(|line| *line == format!("{name}: test"))
                .count(),
            1
        );
        let mut child = ChildGuard(
            Command::new(binary)
                .args(["--exact", name, "--ignored"])
                .env("DOPPELDECKER_FAVORITES_FIFO_ROOT", area.path())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let startup = Instant::now() + Duration::from_secs(15);
        while !area.path().join("ready").exists() {
            if child.0.try_wait().unwrap().is_some() {
                assert!(
                    area.path().join("ready").exists(),
                    "FIFO-Kind endete vor seiner Lesebereitschaft"
                );
                break;
            }
            assert!(Instant::now() < startup, "FIFO-Kind wurde nicht bereit");
            std::thread::sleep(Duration::from_millis(5));
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(
            status.is_some_and(|status| status.success()),
            "Favoritenleser blockiert an einem FIFO ohne Schreiber"
        );
    }

    /// Regression: Ein Bestand oberhalb der Ladegrenze darf gar nicht erst
    /// gespeichert werden — sonst wäre die Ablage ab dem nächsten Zugriff
    /// dauerhaft unlesbar („zu groß"). Der vorherige Bestand bleibt intakt.
    #[test]
    fn speichern_oberhalb_der_ladegrenze_wird_abgelehnt() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("favorites.json");
        update(&store, |favorites| {
            favorites.push(Favorite::local("klein", "/ok"));
            true
        })
        .unwrap();

        // Ein einzelner Favorit mit >1 MiB Namen sprengt die Grenze sicher.
        let error = update(&store, |favorites| {
            favorites.push(Favorite::local("x".repeat(2 * 1024 * 1024), "/gross"));
            true
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        // Die Ablage ist weiterhin lesbar und zeigt den alten Bestand.
        let favorites = load(&store).unwrap();
        assert_eq!(favorites.len(), 1);
        assert_eq!(favorites[0].name, "klein");
    }

    /// Liest eine fehlende Ablage → leere Liste (kein Fehler, kein Panic).
    #[test]
    fn load_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("nicht-da.json");
        assert!(load(&store).unwrap().is_empty());
    }

    /// Beschädigter JSON-Inhalt bleibt sichtbar und wird vor Änderungen gesichert.
    #[test]
    fn load_corrupt_returns_error_and_preserves_recovery_copy() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("favs.json");
        let corrupt = b"das ist kein JSON {{{";
        std::fs::write(&store, corrupt).unwrap();

        let error = load(&store).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&store).unwrap(), corrupt);
        assert_eq!(
            std::fs::read(corrupt_recovery_path(&store).unwrap()).unwrap(),
            corrupt
        );
    }

    /// Der atomare Speicherpfad legt Elternordner an und erhält die Reihenfolge.
    #[test]
    fn save_creates_parent_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        // Elternordner existiert bewusst noch nicht.
        let store = dir.path().join("sub/dir/favs.json");
        let favs = vec![
            Favorite::local("Benutzerordner", "/home/testuser"),
            Favorite::local("Downloads", "/home/testuser/Downloads"),
        ];
        save_locked(&store, &favs).unwrap();
        assert_eq!(load(&store).unwrap(), favs);
    }

    /// add() ergänzt neue Pfade, verweigert Duplikate (Dedup nach Pfad).
    #[test]
    fn add_dedups_by_path() {
        let mut favs = Vec::new();
        assert!(add(&mut favs, "A", "/x"));
        assert!(add(&mut favs, "B", "/y"));
        // Gleicher Pfad, anderer Name → nicht ergänzt.
        assert!(!add(&mut favs, "A-anders", "/x"));
        assert_eq!(favs.len(), 2);
        assert_eq!(favs[0].path, "/x");
        assert_eq!(favs[1].path, "/y");
    }

    /// Gleichlautende Remote-Pfade verschiedener Profile sind getrennte Endpoints.
    #[test]
    fn remote_favorites_deduplizieren_nach_endpoint() {
        let mut favs = Vec::new();
        assert!(add_endpoint(
            &mut favs,
            "Server A",
            FavoriteProvider::Sftp,
            Some("profil-a"),
            "/daten",
        ));
        assert!(add_endpoint(
            &mut favs,
            "Server B",
            FavoriteProvider::Sftp,
            Some("profil-b"),
            "/daten",
        ));
        assert!(!add_endpoint(
            &mut favs,
            "Server A anders",
            FavoriteProvider::Sftp,
            Some("profil-a"),
            "/daten",
        ));
        assert_eq!(favs.len(), 2);
        assert!(remove_endpoint(
            &mut favs,
            FavoriteProvider::Sftp,
            Some("profil-a"),
            "/daten",
        ));
        assert_eq!(favs[0].profile_id.as_deref(), Some("profil-b"));
    }

    /// Die unversionierte lokale Liste älterer Releases wird beim nächsten Zugriff
    /// ohne Verlust in das versionierte Endpoint-Format migriert.
    #[test]
    fn legacy_liste_wird_atomar_migriert() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("favs.json");
        let legacy = vec![Favorite::local("A", "/a")];
        std::fs::write(&store, serde_json::to_vec(&legacy).unwrap()).unwrap();

        assert_eq!(load_or_initialize(&store, &[]).unwrap(), legacy);

        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&store).unwrap()).unwrap();
        assert_eq!(document["version"], FAVORITES_VERSION);
        assert_eq!(document["favorites"][0]["provider"], "local");
        assert!(document["favorites"][0].get("profile_id").is_none());
    }

    /// remove() löscht genau den passenden Pfad und meldet, ob etwas wegfiel.
    #[test]
    fn remove_by_path() {
        let mut favs = vec![Favorite::local("A", "/x"), Favorite::local("B", "/y")];
        assert!(remove(&mut favs, "/x"));
        assert_eq!(favs.len(), 1);
        assert_eq!(favs[0].path, "/y");
        // Nicht vorhandener Pfad → kein Effekt, meldet false.
        assert!(!remove(&mut favs, "/z"));
        assert_eq!(favs.len(), 1);
    }

    /// move_to() sortiert per Pfad an die Zielposition; Ränder bleiben robust.
    #[test]
    fn move_to_reorders_clamped() {
        let favorite = |name: &str, path: &str| Favorite::local(name, path);
        let mut favs = vec![
            favorite("A", "/a"),
            favorite("B", "/b"),
            favorite("C", "/c"),
        ];
        // Nach vorn verschieben.
        assert!(move_to(&mut favs, "/c", 0));
        assert_eq!(
            favs.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["/c", "/a", "/b"],
        );
        // Zu großer Index wird ans Ende geklemmt.
        assert!(move_to(&mut favs, "/c", 99));
        assert_eq!(
            favs.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["/a", "/b", "/c"],
        );
        // Gleiche Position oder unbekannter Pfad → kein Effekt, meldet false.
        assert!(!move_to(&mut favs, "/a", 0));
        assert!(!move_to(&mut favs, "/unbekannt", 1));
        // Leere Liste bleibt ein No-op statt zu panicen.
        let mut empty: Vec<Favorite> = Vec::new();
        assert!(!move_to(&mut empty, "/a", 0));
    }

    #[test]
    fn parallele_updates_verlieren_keinen_favoriten() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(dir.path().join("favs.json"));
        let workers = 8;
        let barrier = Arc::new(Barrier::new(workers));
        let mut threads = Vec::new();
        for index in 0..workers {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                let name = format!("Favorit {index}");
                let path = format!("/parallel/{index}");
                barrier.wait();
                update(&store, |favorites| add(favorites, &name, &path)).unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let favorites = load(&store).unwrap();
        assert_eq!(favorites.len(), workers);
        for index in 0..workers {
            assert!(favorites
                .iter()
                .any(|favorite| favorite.path == format!("/parallel/{index}")));
        }
    }
}
