//! Tauri-Commands für die Schnellzugriff-Favoriten.

use std::path::PathBuf;

use doppeldecker_core::{Favorite, FavoriteProvider};

use crate::{app_config_dir, AppError};

/// Pfad der Favoriten-Ablage im App-Konfigurationsordner.
fn favorites_store(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    let dir = app_config_dir(app)?;
    Ok(dir.join("favorites.json"))
}

/// Beim ersten Start den Benutzerordner und vorhandene Standardordner anbieten.
fn seed_favorites() -> Vec<Favorite> {
    let mut favorites = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        favorites.push(Favorite::local(
            "Benutzerordner",
            home.to_string_lossy().into_owned(),
        ));
        for subdirectory in ["Desktop", "Documents", "Downloads"] {
            let path = home.join(subdirectory);
            if path.is_dir() {
                favorites.push(Favorite::local(
                    subdirectory,
                    path.to_string_lossy().into_owned(),
                ));
            }
        }
    }
    favorites
}

#[tauri::command]
pub(crate) fn list_favorites(app: tauri::AppHandle) -> Result<Vec<Favorite>, AppError> {
    let store = favorites_store(&app)?;
    doppeldecker_core::favorites::load_or_initialize(&store, &seed_favorites())
        .map_err(AppError::from)
}

#[tauri::command]
pub(crate) fn add_favorite(
    app: tauri::AppHandle,
    name: String,
    path: String,
) -> Result<Vec<Favorite>, AppError> {
    let store = favorites_store(&app)?;
    doppeldecker_core::favorites::update(&store, |favorites| {
        doppeldecker_core::favorites::add(favorites, &name, &path)
    })
    .map_err(AppError::from)
}

#[tauri::command]
pub(crate) fn add_remote_favorite(
    app: tauri::AppHandle,
    name: String,
    provider: FavoriteProvider,
    profile_id: String,
    path: String,
) -> Result<Vec<Favorite>, AppError> {
    if provider == FavoriteProvider::Local {
        return Err(AppError::new(
            "invalid_input",
            "Remote-Favorit benötigt einen Remote-Provider",
        ));
    }
    let store = favorites_store(&app)?;
    doppeldecker_core::favorites::update(&store, |favorites| {
        doppeldecker_core::favorites::add_endpoint(
            favorites,
            &name,
            provider,
            Some(&profile_id),
            &path,
        )
    })
    .map_err(AppError::from)
}

/// Reihenfolge per Drag-Sortierung der Seitenleiste ändern.
#[tauri::command]
pub(crate) fn move_favorite(
    app: tauri::AppHandle,
    provider: FavoriteProvider,
    profile_id: Option<String>,
    path: String,
    index: usize,
) -> Result<Vec<Favorite>, AppError> {
    let store = favorites_store(&app)?;
    doppeldecker_core::favorites::update(&store, |favorites| {
        doppeldecker_core::favorites::move_endpoint(
            favorites,
            provider,
            profile_id.as_deref(),
            &path,
            index,
        )
    })
    .map_err(AppError::from)
}

/// Ergebnisbilanz eines Finder-Imports Richtung Frontend. Übersprungenes wird
/// gezählt statt verschwiegen, damit die Oberfläche es sichtbar bilanzieren kann.
#[derive(serde::Serialize)]
pub(crate) struct FinderImportSummary {
    /// Neuer Gesamtbestand der Favoriten (zum direkten Neu-Rendern).
    favorites: Vec<Favorite>,
    added: usize,
    skipped_existing: usize,
    skipped_missing: usize,
    skipped_hidden: usize,
    skipped_unreadable: usize,
}

/// Obergrenze für die Finder-Favoritendatei; echte Dateien sind wenige KiB groß.
#[cfg(any(target_os = "macos", test))]
const MAX_FINDER_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Sucht die Finder-Favoritendatei, neueste Formatvariante zuerst (macOS schreibt
/// je nach Version .sfl4, .sfl3 oder .sfl2 — alle mit derselben Grundstruktur).
#[cfg(target_os = "macos")]
fn finder_favorites_file(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    use tauri::Manager;
    finder_favorites_file_at_home(&app.path().home_dir()?)
}

#[cfg(any(target_os = "macos", test))]
fn finder_favorites_file_at_home(home: &std::path::Path) -> Result<PathBuf, AppError> {
    let base = home.join("Library/Application Support/com.apple.sharedfilelist");
    for extension in ["sfl4", "sfl3", "sfl2"] {
        let candidate = base.join(format!(
            "com.apple.LSSharedFileList.FavoriteItems.{extension}"
        ));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(AppError::new(
        "app",
        "Die Finder-Favoritendatei wurde nicht gefunden.",
    ))
}

#[cfg(any(target_os = "macos", test))]
fn read_finder_favorites_file(file: &std::path::Path) -> Result<Vec<u8>, AppError> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    // Genau EINMAL öffnen (ohne Symlinkfolgen) und Metadaten wie Inhalt vom
    // gehaltenen Handle lesen. Eine getrennte Pfadprüfung vor `fs::read`
    // ließe sich zwischen beiden Zugriffen durch Austausch oder Wachstum
    // unterlaufen (Symlink-Umlenkung, unbegrenztes Lesen).
    let mut options = std::fs::OpenOptions::new();
    // O_NONBLOCK verhindert schon beim Öffnen, dass ein nach der Auswahl
    // eingewechselter FIFO vor der Handle-Typprüfung auf einen Schreiber wartet.
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let handle = match options.open(file) {
        Ok(handle) => handle,
        // macOS schützt diesen Ordner per TCC: ohne Festplattenvollzugriff
        // liefert das Öffnen „permission denied“. Dann klar sagen, was fehlt,
        // statt einen nackten I/O-Fehler zu zeigen.
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(AppError::new(
                "app",
                "macOS verweigert den Zugriff auf die Finder-Konfiguration. \
                 Bitte Doppeldecker unter Systemeinstellungen → Datenschutz & \
                 Sicherheit → Festplattenvollzugriff erlauben und erneut versuchen.",
            ));
        }
        Err(error) => return Err(AppError::from(error)),
    };
    let metadata = handle.metadata().map_err(AppError::from)?;
    if !metadata.is_file() || metadata.len() > MAX_FINDER_FILE_BYTES {
        return Err(AppError::new(
            "app",
            "Die Finder-Favoritendatei ist keine reguläre Datei oder unplausibel groß.",
        ));
    }
    // Höchstens Grenze + 1 Byte lesen: Wächst die Datei nach der Prüfung
    // weiter, wird das sichtbar abgelehnt statt unbegrenzt gepuffert.
    let mut bytes = Vec::new();
    handle
        .take(MAX_FINDER_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(AppError::from)?;
    if bytes.len() as u64 > MAX_FINDER_FILE_BYTES {
        return Err(AppError::new(
            "app",
            "Die Finder-Favoritendatei ist keine reguläre Datei oder unplausibel groß.",
        ));
    }
    Ok(bytes)
}

/// Übernimmt die Ordner aus der Finder-Seitenleiste als Favoriten — rein additiv:
/// vorhandene Favoriten bleiben unverändert, Duplikate (nach Pfad) werden
/// übersprungen, entfernt wird nie etwas. Nur existierende Ordner werden
/// übernommen; im Finder ausgeblendete oder unlesbare Einträge werden gezählt.
#[tauri::command]
pub(crate) fn import_finder_favorites(
    app: tauri::AppHandle,
) -> Result<FinderImportSummary, AppError> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err(AppError::new(
            "app",
            "Finder-Favoriten gibt es nur auf macOS.",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        let bytes = read_finder_favorites_file(&finder_favorites_file(&app)?)?;
        let parsed = doppeldecker_core::finder_favorites::parse_favorite_items(&bytes)
            .map_err(AppError::from)?;

        let mut skipped_hidden = 0usize;
        let mut skipped_missing = 0usize;
        let mut candidates: Vec<Favorite> = Vec::new();
        for item in parsed.items {
            if item.hidden {
                skipped_hidden += 1;
                continue;
            }
            let path = std::path::Path::new(&item.path);
            // Nur real existierende Ordner werden Favoriten; Dateien und
            // verwaiste Einträge fallen sichtbar in die Bilanz.
            if !path.is_dir() {
                skipped_missing += 1;
                continue;
            }
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| item.path.clone());
            candidates.push(Favorite::local(name, item.path));
        }

        let mut added = 0usize;
        let mut skipped_existing = 0usize;
        let store = favorites_store(&app)?;
        let favorites = doppeldecker_core::favorites::update(&store, |favorites| {
            let mut changed = false;
            for candidate in &candidates {
                if doppeldecker_core::favorites::add(favorites, &candidate.name, &candidate.path) {
                    added += 1;
                    changed = true;
                } else {
                    skipped_existing += 1;
                }
            }
            changed
        })
        .map_err(AppError::from)?;

        Ok(FinderImportSummary {
            favorites,
            added,
            skipped_existing,
            skipped_missing,
            skipped_hidden,
            skipped_unreadable: parsed.skipped_unreadable,
        })
    }
}

#[tauri::command]
pub(crate) fn remove_favorite(
    app: tauri::AppHandle,
    provider: FavoriteProvider,
    profile_id: Option<String>,
    path: String,
) -> Result<Vec<Favorite>, AppError> {
    let store = favorites_store(&app)?;
    doppeldecker_core::favorites::update(&store, |favorites| {
        doppeldecker_core::favorites::remove_endpoint(
            favorites,
            provider,
            profile_id.as_deref(),
            &path,
        )
    })
    .map_err(AppError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "nur im isolierten Gegenlauf nach Austausch der ausgewählten Testdatei"]
    fn finder_fifo_child() {
        let root = PathBuf::from(std::env::var_os("DOPPELDECKER_FINDER_FIFO_ROOT").unwrap());
        assert!(root
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("doppeldecker-finder-fifo-"));
        assert_eq!(
            std::fs::read(root.join("marker")).unwrap(),
            b"finder-fifo-v1"
        );
        let selected = std::fs::read_to_string(root.join("selected")).unwrap();
        let selected = PathBuf::from(selected);
        assert!(selected.starts_with(&root));
        std::fs::write(root.join("ready"), b"ready").unwrap();
        assert_eq!(
            read_finder_favorites_file(&selected).unwrap_err().code,
            "app"
        );
    }

    #[test]
    fn qa_local_finder_austausch_durch_fifo_blockiert_nicht() {
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
            .prefix("doppeldecker-finder-fifo-")
            .tempdir()
            .unwrap();
        std::fs::write(area.path().join("marker"), b"finder-fifo-v1").unwrap();
        let base = area
            .path()
            .join("Library/Application Support/com.apple.sharedfilelist");
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("com.apple.LSSharedFileList.FavoriteItems.sfl4");
        let original = b"kuenstliche Finder-Testdatei";
        std::fs::write(&path, original).unwrap();
        let selected = finder_favorites_file_at_home(area.path()).unwrap();
        assert_eq!(selected, path);
        assert_eq!(read_finder_favorites_file(&selected).unwrap(), original);
        let preserved = base.join("preserved-original");
        std::fs::rename(&selected, &preserved).unwrap();
        let fifo = std::ffi::CString::new(selected.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        std::fs::write(area.path().join("selected"), selected.to_str().unwrap()).unwrap();
        let name = "favorites_commands::tests::finder_fifo_child";
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
                .env("DOPPELDECKER_FINDER_FIFO_ROOT", area.path())
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
        assert_eq!(std::fs::read(preserved).unwrap(), original);
        assert!(
            status.is_some_and(|status| status.success()),
            "Finder-Leser blockiert nach dem Austausch an einem FIFO ohne Schreiber"
        );
    }
}
