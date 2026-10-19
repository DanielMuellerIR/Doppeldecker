//! Native Datei-Zwischenablage. Textfelder behalten ihre WebKit-/GTK-Bearbeitung.
//! Datei-URIs sind Übergabedaten, keine Ausführungsfreigaben; der Datei-Executor
//! prüft Quellen, Sandbox und Ziel unmittelbar vor jeder Wirkung erneut.

use crate::{AppError, MutexExt};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

const MAX_FILES: usize = 100_000;
const MAX_BYTES: usize = 16 * 1024 * 1024;

// Die Systemzwischenablage ist fensterübergreifend. Eine große ältere Auswahl
// darf eine inzwischen neu angeforderte Auswahl nicht nachträglich ersetzen.
#[derive(Default)]
struct ClipboardWrites {
    generation: AtomicU64,
    pending: Mutex<Option<(u64, Option<bool>)>>,
    initial_version: Mutex<Option<(u64, String)>>,
}

impl ClipboardWrites {
    fn begin(&self) -> u64 {
        let mut pending = self.pending.lock_recover();
        let request = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *pending = Some((request, None));
        request
    }

    fn complete(&self, request: u64, success: bool) {
        let mut pending = self.pending.lock_recover();
        if let Some((current, result)) = pending.as_mut() {
            if *current == request && result.is_none() {
                *result = Some(success);
            }
        }
    }

    fn bind_version(&self, request: u64, version: String) {
        let pending = self.pending.lock_recover();
        if pending.is_some_and(|(current, _)| current == request) {
            *self.initial_version.lock_recover() = Some((request, version));
        }
    }

    fn validate_read(&self, request: u64, version: &str) -> Result<(), AppError> {
        let pending = self.pending.lock_recover();
        if *pending == Some((request, Some(false))) {
            let initial = self.initial_version.lock_recover();
            // Ein neuer externer Clipboardstand ist wieder verwendbar. Ohne
            // gebundene Version darf eine abgebrochene Vorbereitung dagegen
            // keine möglicherweise alte Copy-/Cut-Auswahl freigeben.
            if !initial
                .as_ref()
                .is_some_and(|(current, initial)| *current == request && initial != version)
            {
                return Err(AppError::new("clipboard_copy_failed", "Die Kopierauswahl wurde nicht in die Zwischenablage übernommen. Bitte erneut kopieren; die alte Auswahl wurde nicht eingefügt."));
            }
        }
        Ok(())
    }

    async fn ready_to_read(&self) -> Result<u64, AppError> {
        let started = std::time::Instant::now();
        loop {
            let state = *self.pending.lock_recover();
            match state {
                None => return Ok(0),
                Some((request, Some(_))) => return Ok(request),
                Some((_, None)) => {}
            }
            if started.elapsed() >= std::time::Duration::from_secs(10) {
                return Err(AppError::new(
                    "busy",
                    "Die Kopierauswahl wird noch vorbereitet. Es wurde nichts eingefügt.",
                ));
            }
            // Kein Fenstergate und keine blockierte UI: Nur dieses Paste wartet
            // auf die jüngste eigene, prozessweite Dateiübergabe.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    fn publish(
        &self,
        request: u64,
        expected_version: &str,
        current_version: impl FnOnce() -> String,
        write: impl FnOnce() -> Result<(), AppError>,
    ) -> Result<bool, AppError> {
        if self.generation.load(Ordering::SeqCst) != request
            || current_version() != expected_version
        {
            return Ok(false);
        }
        write()?;
        Ok(true)
    }
}

static CLIPBOARD_WRITES: LazyLock<ClipboardWrites> = LazyLock::new(ClipboardWrites::default);

struct ClipboardWriteCompletion<'a> {
    writes: &'a ClipboardWrites,
    request: u64,
}
impl Drop for ClipboardWriteCompletion<'_> {
    fn drop(&mut self) {
        self.writes.complete(self.request, false);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileClipboard {
    paths: Vec<String>,
    cut: bool,
    version: String,
}

fn file_uris(paths: &[String]) -> Result<Vec<String>, AppError> {
    if paths.is_empty()
        || paths.len() > MAX_FILES
        || paths.iter().map(String::len).sum::<usize>() > MAX_BYTES
    {
        return Err(AppError::new(
            "invalid_input",
            "Keine Dateien oder zu große Dateiauswahl",
        ));
    }
    let uris = paths
        .iter()
        .map(|path| {
            let parsed = Path::new(path);
            if !parsed.is_absolute()
                || parsed.parent().is_none()
                || parsed.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
                || path.as_bytes().contains(&0)
            {
                return Err(AppError::new(
                    "invalid_input",
                    "Ungültiger lokaler Dateipfad",
                ));
            }
            url::Url::from_file_path(parsed)
                .map(|url| url.to_string())
                .map_err(|_| {
                    AppError::new(
                        "invalid_input",
                        "Dateipfad ist nicht als Datei-URI darstellbar",
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if uris.iter().map(String::len).sum::<usize>() > MAX_BYTES {
        return Err(AppError::new(
            "invalid_input",
            "Datei-URIs sind zu groß für die Zwischenablage",
        ));
    }
    Ok(uris)
}

fn decode_uris(uris: &[String], cut: bool, version: String) -> Result<FileClipboard, AppError> {
    if uris.len() > MAX_FILES || uris.iter().map(String::len).sum::<usize>() > MAX_BYTES {
        return Err(AppError::new(
            "invalid_input",
            "Datei-Zwischenablage ist zu groß",
        ));
    }
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for uri in uris {
        let url = url::Url::parse(uri).map_err(|_| {
            AppError::new("invalid_input", "Ungültige Datei-URI in der Zwischenablage")
        })?;
        if url.scheme() != "file"
            || url.query().is_some()
            || url.fragment().is_some()
            || url.host_str().is_some_and(|host| host != "localhost")
        {
            return Err(AppError::new(
                "invalid_input",
                "Die Zwischenablage enthält keine lokalen Dateien",
            ));
        }
        let path = url
            .to_file_path()
            .ok()
            .and_then(|path| path.into_os_string().into_string().ok())
            .ok_or_else(|| {
                AppError::new("invalid_input", "Dateiname ist nicht eindeutig darstellbar")
            })?;
        if seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    if !paths.is_empty() {
        file_uris(&paths)?;
    }
    Ok(FileClipboard {
        paths,
        cut,
        version,
    })
}

#[tauri::command]
pub(crate) async fn write_file_clipboard(
    app: tauri::AppHandle,
    paths: Vec<String>,
    cut: bool,
) -> Result<bool, AppError> {
    let request = CLIPBOARD_WRITES.begin();
    let _completion = ClipboardWriteCompletion {
        writes: &CLIPBOARD_WRITES,
        request,
    };
    let result = async {
        let initial_version = on_main(&app, |sender| {
            let _ = sender.send(Ok(native::current_version()));
        })
        .await?;
        CLIPBOARD_WRITES.bind_version(request, initial_version.clone());
        // Die Zwischenablage übergibt Referenzen. Erst Paste/Executor prüft die
        // Quellen frisch; Copy darf nicht auf tausende langsame Mount-Stats warten.
        let uris = tauri::async_runtime::spawn_blocking(move || file_uris(&paths))
            .await
            .map_err(|error| AppError::new("app", error.to_string()))??;
        on_main(&app, move |sender| {
            let _ = sender.send(CLIPBOARD_WRITES.publish(
                request,
                &initial_version,
                native::current_version,
                || native::write(&uris, cut),
            ));
        })
        .await
    }
    .await;
    CLIPBOARD_WRITES.complete(request, matches!(result, Ok(true)));
    result
}

#[tauri::command]
pub(crate) async fn read_file_clipboard(app: tauri::AppHandle) -> Result<FileClipboard, AppError> {
    loop {
        let generation = CLIPBOARD_WRITES.ready_to_read().await?;
        let result = on_main(&app, move |sender| {
            if let Err(error) =
                CLIPBOARD_WRITES.validate_read(generation, &native::current_version())
            {
                let _ = sender.send(Err(error));
            } else {
                native::read(sender);
            }
        })
        .await;
        if CLIPBOARD_WRITES.generation.load(Ordering::SeqCst) == generation {
            return result;
        }
    }
}

#[tauri::command]
pub(crate) async fn clear_file_clipboard_if_unchanged(
    app: tauri::AppHandle,
    version: String,
) -> Result<bool, AppError> {
    on_main(&app, move |sender| {
        let _ = sender.send(native::clear(&version));
    })
    .await
}

type Sender<T> = std::sync::mpsc::SyncSender<Result<T, AppError>>;
async fn on_main<T: Send + 'static>(
    app: &tauri::AppHandle,
    action: impl FnOnce(Sender<T>) + Send + 'static,
) -> Result<T, AppError> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || action(sender))
        .map_err(|error| AppError::new("app", error.to_string()))?;
    tauri::async_runtime::spawn_blocking(move || {
        receiver.recv_timeout(std::time::Duration::from_secs(5))
    })
    .await
    .map_err(|_| AppError::new("app", "Zwischenablage-Auftrag wurde beendet"))?
    .map_err(|_| AppError::new("busy", "Systemzwischenablage antwortet nicht"))?
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSString, NSURL};
    const FILE: &str = "public.file-url";
    const ACTION: &str = "org.doppeldecker.file-operation";
    pub(super) fn write(uris: &[String], cut: bool) -> Result<(), AppError> {
        write_to(&NSPasteboard::generalPasteboard(), uris, cut)
    }
    fn write_to(board: &NSPasteboard, uris: &[String], cut: bool) -> Result<(), AppError> {
        let items = uris
            .iter()
            .map(|uri| {
                let item = NSPasteboardItem::new();
                if !item.setString_forType(&NSString::from_str(uri), &NSString::from_str(FILE))
                    || !item.setString_forType(
                        &NSString::from_str(if cut { "cut" } else { "copy" }),
                        &NSString::from_str(ACTION),
                    )
                {
                    return Err(AppError::new(
                        "app",
                        "Dateien konnten nicht in die Zwischenablage geschrieben werden",
                    ));
                }
                Ok(item)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let refs: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = items
            .iter()
            .map(|item| ProtocolObject::from_ref(&**item))
            .collect();
        board.clearContents();
        if !board.writeObjects(&NSArray::from_slice(&refs)) {
            return Err(AppError::new(
                "app",
                "Systemzwischenablage hat die Dateien abgelehnt",
            ));
        }
        Ok(())
    }
    pub(super) fn read(sender: Sender<FileClipboard>) {
        let _ = sender.send(read_from(&NSPasteboard::generalPasteboard()));
    }
    pub(super) fn current_version() -> String {
        NSPasteboard::generalPasteboard().changeCount().to_string()
    }
    fn read_from(board: &NSPasteboard) -> Result<FileClipboard, AppError> {
        let version = board.changeCount();
        let mut uris = Vec::new();
        let mut cut = false;
        if let Some(items) = board.pasteboardItems() {
            for item in items.iter() {
                if let Some(uri) = item.stringForType(&NSString::from_str(FILE)) {
                    let uri = uri.to_string();
                    // Finder darf eine volumegebundene File-Reference-URL statt
                    // eines Pfads liefern. Foundation löst sie zum echten Namen
                    // auf; sonst würde der Executor „id=…“ als Dateiname verwenden.
                    let uri = if uri.starts_with("file:///.file/") {
                        NSURL::URLWithString(&NSString::from_str(&uri))
                            .and_then(|url| url.filePathURL())
                            .and_then(|url| url.absoluteString())
                            .map(|value| value.to_string())
                            .ok_or_else(|| {
                                AppError::new(
                                    "not_found",
                                    "Finder-Dateireferenz ist nicht mehr verfügbar",
                                )
                            })?
                    } else {
                        uri
                    };
                    uris.push(uri);
                }
                cut |= item
                    .stringForType(&NSString::from_str(ACTION))
                    .is_some_and(|action| action.to_string() == "cut");
            }
        }
        if version != board.changeCount() {
            Err(AppError::new(
                "busy",
                "Zwischenablage wurde während des Lesens geändert",
            ))
        } else {
            decode_uris(&uris, cut, version.to_string())
        }
    }
    pub(super) fn clear(version: &str) -> Result<bool, AppError> {
        clear_from(&NSPasteboard::generalPasteboard(), version)
    }
    fn clear_from(board: &NSPasteboard, version: &str) -> Result<bool, AppError> {
        if board.changeCount().to_string() != version {
            return Ok(false);
        }
        board.clearContents();
        Ok(true)
    }
    #[test]
    fn zehntausend_native_dateireferenzen_bleiben_vollstaendig() {
        let board = NSPasteboard::pasteboardWithUniqueName();
        let paths: Vec<_> = (0..10_000)
            .map(|index| format!("/tmp/Grosse Auswahl/Datei-{index:05}.txt"))
            .collect();
        write_to(&board, &file_uris(&paths).unwrap(), false).unwrap();
        assert_eq!(read_from(&board).unwrap().paths, paths);
        board.clearContents();
    }

    #[test]
    fn gescheiterte_neue_copyauswahl_sperrt_alten_nativen_cut_auch_in_fenster_b() {
        let board = NSPasteboard::pasteboardWithUniqueName();
        let writes = ClipboardWrites::default();
        write_to(
            &board,
            &file_uris(&["/tmp/Alter Cut.txt".into()]).unwrap(),
            true,
        )
        .unwrap();
        assert!(read_from(&board).unwrap().cut);
        let request = writes.begin();
        writes.bind_version(request, board.changeCount().to_string());
        assert!(file_uris(&[format!("/tmp/{}", "ü".repeat(MAX_BYTES / 2))]).is_err());
        writes.complete(request, false);
        let paste = tauri::async_runtime::block_on(writes.ready_to_read()).unwrap();
        assert_eq!(
            writes
                .validate_read(paste, &board.changeCount().to_string())
                .unwrap_err()
                .code,
            "clipboard_copy_failed"
        );
        write_to(
            &board,
            &file_uris(&["/tmp/Externe Auswahl.txt".into()]).unwrap(),
            false,
        )
        .unwrap();
        writes
            .validate_read(paste, &board.changeCount().to_string())
            .unwrap();
        assert_eq!(
            read_from(&board).unwrap().paths,
            ["/tmp/Externe Auswahl.txt"]
        );
        board.clearContents();
    }

    #[test]
    fn fenster_b_paste_wartet_auf_grosse_ausstehende_copyauswahl_in_fenster_a() {
        let board = NSPasteboard::pasteboardWithUniqueName();
        let old = vec!["/tmp/Alte Auswahl.txt".into()];
        write_to(&board, &file_uris(&old).unwrap(), false).unwrap();
        let writes = std::sync::Arc::new(ClipboardWrites::default());
        let request = writes.begin();
        let name = board.name().to_string();
        let new_paths = (0..10_000)
            .map(|index| format!("/tmp/Neue Auswahl/Datei-{index:05}.txt"))
            .collect::<Vec<_>>();
        let expected = new_paths.clone();
        let writer = writes.clone();
        let thread = std::thread::spawn(move || {
            let uris = file_uris(&new_paths).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(80));
            let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&name));
            let initial = board.changeCount().to_string();
            let published = writer
                .publish(
                    request,
                    &initial,
                    || board.changeCount().to_string(),
                    || write_to(&board, &uris, false),
                )
                .unwrap();
            writer.complete(request, published);
        });
        tauri::async_runtime::block_on(writes.ready_to_read()).unwrap();
        let actual = read_from(&board).unwrap().paths;
        thread.join().unwrap();
        board.clearContents();
        assert_eq!(
            actual.len(),
            expected.len(),
            "Paste in Fenster B darf die alte native Auswahl nicht übernehmen"
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn spaete_appkit_auswahl_ueberschreibt_keine_neuere() {
        let board = NSPasteboard::pasteboardWithUniqueName();
        let writes = ClipboardWrites::default();
        let old = writes.begin();
        let newer = writes.begin();
        let old_uris = file_uris(&["/tmp/Alte Auswahl.txt".into()]).unwrap();
        let new_paths = vec!["/tmp/Neue Auswahl.txt".into()];
        let new_uris = file_uris(&new_paths).unwrap();
        let version = board.changeCount().to_string();
        writes
            .publish(
                newer,
                &version,
                || board.changeCount().to_string(),
                || write_to(&board, &new_uris, false),
            )
            .unwrap();
        let published_old = writes
            .publish(
                old,
                &version,
                || board.changeCount().to_string(),
                || write_to(&board, &old_uris, true),
            )
            .unwrap();
        assert_eq!(read_from(&board).unwrap().paths, new_paths);
        assert!(!published_old);
        board.clearContents();
    }

    #[test]
    fn spaete_dateiauswahl_erhaelt_inzwischen_kopierten_text() {
        let board = NSPasteboard::pasteboardWithUniqueName();
        let writes = ClipboardWrites::default();
        let request = writes.begin();
        let initial = board.changeCount().to_string();
        let text_type = NSString::from_str("public.utf8-plain-text");
        board.clearContents();
        assert!(board.setString_forType(&NSString::from_str("Neu kopierter Text"), &text_type));
        let uris = file_uris(&["/tmp/Alte Dateiauswahl.txt".into()]).unwrap();
        let published = writes
            .publish(
                request,
                &initial,
                || board.changeCount().to_string(),
                || write_to(&board, &uris, false),
            )
            .unwrap();
        assert_eq!(
            board.stringForType(&text_type).map(|text| text.to_string()),
            Some("Neu kopierter Text".into())
        );
        assert!(!published);
        board.clearContents();
    }

    #[test]
    fn echte_appkit_datei_zwischenablage_ohne_nutzerclipboard() {
        let board = NSPasteboard::pasteboardWithUniqueName();
        let paths = vec!["/tmp/A B.txt".into(), "/tmp/literal%2F.txt".into()];
        let uris = file_uris(&paths).unwrap();
        write_to(&board, &uris, true).unwrap();
        let captured = read_from(&board).unwrap();
        assert_eq!(captured.paths, paths);
        assert!(captured.cut);
        assert_eq!(board.pasteboardItems().unwrap().len(), 2);
        write_to(&board, &uris, false).unwrap();
        assert!(!clear_from(&board, &captured.version).unwrap());
        let newer = read_from(&board).unwrap();
        assert!(!newer.cut);
        assert!(clear_from(&board, &newer.version).unwrap());
        assert!(read_from(&board).unwrap().paths.is_empty());
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Finder Grüße.txt");
        std::fs::write(&path, b"test").unwrap();
        let reference = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()))
            .fileReferenceURL()
            .unwrap()
            .absoluteString()
            .unwrap()
            .to_string();
        write_to(&board, &[reference], false).unwrap();
        assert_eq!(
            read_from(&board).unwrap().paths,
            vec![std::fs::canonicalize(&path).unwrap().to_str().unwrap()]
        );
        board.clearContents();
    }
}

#[cfg(target_os = "linux")]
mod native {
    use super::*;
    use gtk::prelude::*;
    use gtk::{gdk, Clipboard, TargetEntry, TargetFlags};
    use std::cell::Cell;
    const GNOME: &str = "x-special/gnome-copied-files";
    const URI: &str = "text/uri-list";
    const KDE: &str = "application/x-kde-cutselection";
    thread_local! {
        static VERSION: Cell<u64> = const { Cell::new(0) };
        static BOARD: Clipboard = {
            let board = Clipboard::get(&gdk::SELECTION_CLIPBOARD);
            board.connect_local("owner-change", false, |_| {
                VERSION.with(|value| value.set(value.get().wrapping_add(1)));
                None
            });
            board
        };
    }
    fn version() -> String {
        VERSION.with(|value| value.get().to_string())
    }
    pub(super) fn current_version() -> String {
        BOARD.with(|_| version())
    }
    pub(super) fn write(uris: &[String], cut: bool) -> Result<(), AppError> {
        let uris = uris.join("\r\n");
        let gnome = format!(
            "{}\n{}",
            if cut { "cut" } else { "copy" },
            uris.replace("\r\n", "\n")
        );
        let targets = [
            TargetEntry::new(URI, TargetFlags::empty(), 0),
            TargetEntry::new(GNOME, TargetFlags::empty(), 1),
            TargetEntry::new(KDE, TargetFlags::empty(), 2),
        ];
        BOARD.with(|board| {
            if !board.set_with_data(&targets, move |_, selection, info| {
                let value = match info {
                    1 => gnome.as_bytes(),
                    2 => {
                        if cut {
                            b"1"
                        } else {
                            b"0"
                        }
                    }
                    _ => uris.as_bytes(),
                };
                selection.set(&selection.target(), 8, value);
            }) {
                return Err(AppError::new(
                    "app",
                    "Systemzwischenablage hat die Dateien abgelehnt",
                ));
            }
            // Ein vorhandener Clipboard-Manager darf die Übergabe beim Beenden
            // erhalten. Ohne Manager bleibt der normale GTK-Ownervertrag gültig.
            unsafe {
                gtk::ffi::gtk_clipboard_set_can_store(board.to_glib_none().0, std::ptr::null(), 0);
            }
            Ok(())
        })
    }
    fn lines(data: &[u8]) -> Result<Vec<String>, AppError> {
        if data.len() > MAX_BYTES {
            return Err(AppError::new(
                "invalid_input",
                "Datei-Zwischenablage ist zu groß",
            ));
        }
        let text = std::str::from_utf8(data)
            .map_err(|_| AppError::new("invalid_input", "Ungültige Datei-Zwischenablage"))?;
        Ok(text
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect())
    }
    fn data(selection: &gtk::SelectionData) -> Result<Vec<u8>, AppError> {
        let length = selection.length();
        if length > MAX_BYTES as i32 {
            return Err(AppError::new(
                "invalid_input",
                "Datei-Zwischenablage ist zu groß",
            ));
        }
        if length <= 0 {
            return Ok(Vec::new());
        }
        Ok(selection.data())
    }
    pub(super) fn read(sender: Sender<FileClipboard>) {
        BOARD.with(|board| {
            let initial = version();
            board.request_contents(&gdk::Atom::intern(GNOME), move |board, selection| {
                let bytes = match data(selection) {
                    Ok(data) => data,
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        return;
                    }
                };
                if !bytes.is_empty() {
                    let result = lines(&bytes).and_then(|mut values| {
                        if values
                            .first()
                            .is_none_or(|action| action != "copy" && action != "cut")
                        {
                            return Err(AppError::new(
                                "invalid_input",
                                "Ungültige Dateiaktion in der Zwischenablage",
                            ));
                        }
                        let cut = values.remove(0) == "cut";
                        finish(&values, cut, initial)
                    });
                    let _ = sender.send(result);
                } else {
                    board.request_contents(&gdk::Atom::intern(URI), move |board, selection| {
                        let values = data(selection).and_then(|data| lines(&data));
                        board.request_contents(&gdk::Atom::intern(KDE), move |_, selection| {
                            let result = data(selection).and_then(|data| {
                                values.and_then(|uris| finish(&uris, data == b"1", initial))
                            });
                            let _ = sender.send(result);
                        });
                    });
                }
            });
        });
    }
    fn finish(uris: &[String], cut: bool, initial: String) -> Result<FileClipboard, AppError> {
        if initial != version() {
            return Err(AppError::new(
                "busy",
                "Zwischenablage wurde während des Lesens geändert",
            ));
        }
        decode_uris(uris, cut, initial)
    }
    pub(super) fn clear(expected: &str) -> Result<bool, AppError> {
        BOARD.with(|board| {
            if version() != expected {
                return Ok(false);
            }
            board.clear();
            Ok(true)
        })
    }
    #[test]
    #[ignore = "benötigt isolierten Xvfb-Display und DOPPELDECKER_NATIVE_CLIPBOARD_TEST=1"]
    fn echte_gtk_datei_zwischenablage_gnome_kde_und_uri() {
        assert_eq!(
            std::env::var("DOPPELDECKER_NATIVE_CLIPBOARD_TEST").as_deref(),
            Ok("1")
        );
        gtk::init().unwrap();
        fn drain() {
            let context = gtk::glib::MainContext::default();
            while context.pending() {
                context.iteration(false);
            }
        }
        fn snapshot() -> FileClipboard {
            drain();
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            read(sender);
            let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                drain();
                if let Ok(value) = receiver.try_recv() {
                    return value.unwrap();
                }
                assert!(std::time::Instant::now() < until);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        let paths = vec!["/tmp/A B.txt".into(), "/tmp/literal%2F.txt".into()];
        let uris = file_uris(&paths).unwrap();
        let writes = ClipboardWrites::default();
        let old_request = writes.begin();
        let newer_request = writes.begin();
        let initial = current_version();
        writes
            .publish(newer_request, &initial, current_version, || {
                write(&uris[..1], false)
            })
            .unwrap();
        let published_old = writes
            .publish(old_request, &initial, current_version, || {
                write(&uris[1..], true)
            })
            .unwrap();
        assert_eq!(snapshot().paths, paths[..1]);
        assert!(!published_old);
        let request = writes.begin();
        let initial = current_version();
        BOARD.with(|board| board.set_text("Neu kopierter Text"));
        drain();
        assert!(!writes
            .publish(request, &initial, current_version, || write(&uris, false))
            .unwrap());
        assert_eq!(
            BOARD.with(|board| board.wait_for_text().map(|text| text.to_string())),
            Some("Neu kopierter Text".into())
        );
        write(&uris, true).unwrap();
        let own = snapshot();
        assert_eq!(own.paths, paths);
        assert!(own.cut);
        BOARD.with(|board| {
            let value = uris.join("\r\n");
            assert!(board.set_with_data(
                &[
                    TargetEntry::new(URI, TargetFlags::empty(), 0),
                    TargetEntry::new(KDE, TargetFlags::empty(), 1)
                ],
                move |_, selection, info| {
                    selection.set(
                        &selection.target(),
                        8,
                        if info == 1 { b"1" } else { value.as_bytes() },
                    );
                }
            ));
        });
        let kde = snapshot();
        assert_eq!(kde.paths, paths);
        assert!(kde.cut);
        assert!(!clear(&own.version).unwrap());
        BOARD.with(|board| {
            let value = uris.join("\r\n");
            assert!(board.set_with_data(
                &[TargetEntry::new(URI, TargetFlags::empty(), 0)],
                move |_, selection, _| selection.set(&selection.target(), 8, value.as_bytes())
            ));
        });
        let plain = snapshot();
        assert_eq!(plain.paths, paths);
        assert!(!plain.cut);
        assert!(!clear(&kde.version).unwrap());
        assert!(clear(&plain.version).unwrap());
        assert!(snapshot().paths.is_empty());
        let temp = tempfile::tempdir().unwrap();
        let foreign = temp.path().join("foreign %2F.txt");
        std::fs::write(&foreign, b"fixture").unwrap();
        struct Peer(std::process::Child);
        impl Drop for Peer {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut peer = Peer(
            std::process::Command::new(std::env::current_exe().unwrap())
                .arg("file_clipboard::native::gtk_clipboard_peer")
                .args(["--exact", "--ignored"])
                .env("DOPPELDECKER_NATIVE_CLIPBOARD_PEER", temp.path())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        fn wait_file(path: &Path) {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !path.is_file() {
                drain();
                assert!(
                    std::time::Instant::now() < until,
                    "Gegenprozess liefert keinen Beleg"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        std::fs::write(temp.path().join("command"), b"publish").unwrap();
        wait_file(&temp.path().join("ready"));
        let received = snapshot();
        assert_eq!(received.paths, vec![foreign.to_str().unwrap()]);
        assert!(received.cut);
        write(&uris, false).unwrap();
        drain();
        assert!(
            !clear(&received.version).unwrap(),
            "Ein neuer Besitzer darf nicht mit dem alten Stand geleert werden"
        );
        std::fs::write(temp.path().join("command"), b"read").unwrap();
        wait_file(&temp.path().join("received.json"));
        let external: Vec<String> =
            serde_json::from_slice(&std::fs::read(temp.path().join("received.json")).unwrap())
                .unwrap();
        assert_eq!(external, uris);
        std::fs::remove_file(temp.path().join("ready")).unwrap();
        std::fs::write(temp.path().join("command"), b"publish").unwrap();
        wait_file(&temp.path().join("ready"));
        let foreign_cut = snapshot();
        assert!(foreign_cut.cut);
        assert!(clear(&foreign_cut.version).unwrap());
        assert!(
            snapshot().paths.is_empty(),
            "Auch ein fremder Cut-Clipboard-Stand muss nach erfolgreichem Move verbraucht sein"
        );
        std::fs::write(temp.path().join("command"), b"stop").unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if let Some(status) = peer.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(std::time::Instant::now() < until);
            drain();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    #[test]
    #[ignore = "nur als isolierter GTK-Clipboard-Gegenprozess gestartet"]
    fn gtk_clipboard_peer() {
        assert_eq!(
            std::env::var("DOPPELDECKER_NATIVE_CLIPBOARD_TEST").as_deref(),
            Ok("1")
        );
        let root =
            std::path::PathBuf::from(std::env::var("DOPPELDECKER_NATIVE_CLIPBOARD_PEER").unwrap());
        gtk::init().unwrap();
        let board = Clipboard::get(&gdk::SELECTION_CLIPBOARD);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut previous = String::new();
        while std::time::Instant::now() < until {
            let command = std::fs::read_to_string(root.join("command")).unwrap_or_default();
            if command != previous {
                previous = command.clone();
                if command == "stop" {
                    return;
                }
                if command == "publish" {
                    let uri = url::Url::from_file_path(root.join("foreign %2F.txt"))
                        .unwrap()
                        .to_string();
                    // Der Gegenprozess publiziert das fremde GNOME-Format direkt,
                    // ohne den zu prüfenden Encoder oder Writer zu verwenden.
                    let payload = format!("cut\n{uri}\n");
                    assert!(board.set_with_data(
                        &[TargetEntry::new(GNOME, TargetFlags::empty(), 0)],
                        move |_, selection, _| selection.set(
                            &selection.target(),
                            8,
                            payload.as_bytes()
                        )
                    ));
                    std::fs::write(root.join("ready"), b"published").unwrap();
                } else if command == "read" {
                    let output = root.join("received.json");
                    board.request_uris(move |_, uris| {
                        let values: Vec<String> = uris.iter().map(|uri| uri.to_string()).collect();
                        std::fs::write(output, serde_json::to_vec(&values).unwrap()).unwrap();
                    });
                }
            }
            let context = gtk::glib::MainContext::default();
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("Clipboard-Gegenprozess wurde nicht rechtzeitig beendet");
    }
    use gtk::glib::translate::ToGlibPtr;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grosse_ordnerauswahl_bleibt_vollstaendig_und_geordnet() {
        let paths: Vec<_> = (0..10_000)
            .map(|index| format!("/tmp/grosser-ordner/Datei-{index:05}.txt"))
            .collect();
        let mut uris = file_uris(&paths).unwrap();
        uris.push(uris[0].clone());
        assert_eq!(
            decode_uris(&uris, false, "bulk".into()).unwrap().paths,
            paths
        );
    }

    #[test]
    fn native_datei_uris_erhalten_sonderzeichen_und_mehrfachauswahl() {
        let paths = vec![
            "/tmp/A B/literal%2F.txt".into(),
            "/tmp/Grüße\nneu.txt".into(),
        ];
        let uris = file_uris(&paths).unwrap();
        assert!(uris[0].contains("%252F"));
        assert_eq!(decode_uris(&uris, true, "7".into()).unwrap().paths, paths);
        assert!(decode_uris(&uris, true, "7".into()).unwrap().cut);
    }
    #[test]
    fn fremde_und_ungueltige_datei_uris_scheitern_sichtbar() {
        for uri in [
            "https://example.test/file",
            "file://server/share/file",
            "file:///tmp/file?secret=1",
            "file:///tmp/%FF",
        ] {
            assert!(
                decode_uris(&[uri.into()], false, "1".into()).is_err(),
                "{uri}"
            );
        }
        assert!(file_uris(&["relative/file".into()]).is_err());
        assert!(file_uris(&["/".into()]).is_err());
        assert!(file_uris(&[format!("/tmp/{}", "ü".repeat(MAX_BYTES / 2 - 10))]).is_err());
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::*;

    #[test]
    fn paste_wartet_auf_die_juengste_auswahl_und_verbraucht_keine_fehlgeschlagene() {
        for success in [false, true] {
            let writes = std::sync::Arc::new(ClipboardWrites::default());
            let old = writes.begin();
            let latest = writes.begin();
            writes.bind_version(latest, "old-cut".into());
            writes.complete(old, true);
            let worker = writes.clone();
            let thread = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(30));
                worker.complete(latest, success);
            });
            let result = tauri::async_runtime::block_on(writes.ready_to_read());
            thread.join().unwrap();
            assert_eq!(result.unwrap(), latest);
            assert_eq!(writes.validate_read(latest, "old-cut").is_ok(), success);
        }
    }

    #[test]
    fn copyfehler_vor_fensteruebergreifendem_paste_sperrt_alten_cut_bis_neuer_version() {
        let writes = ClipboardWrites::default();
        let request = writes.begin();
        writes.bind_version(request, "old-cut".into());
        writes.complete(request, false);
        let paste = tauri::async_runtime::block_on(writes.ready_to_read()).unwrap();
        assert_eq!(
            writes.validate_read(paste, "old-cut").unwrap_err().code,
            "clipboard_copy_failed"
        );
        assert!(writes.validate_read(paste, "external-copy").is_ok());
        let next = writes.begin();
        writes.bind_version(next, "external-copy".into());
        writes.complete(next, true);
        assert!(writes.validate_read(next, "own-copy").is_ok());
    }

    #[test]
    fn abgebrochene_copyvorbereitung_laesst_keine_ewige_pastesperre_zurueck() {
        let writes = ClipboardWrites::default();
        let request = writes.begin();
        {
            let _completion = ClipboardWriteCompletion {
                writes: &writes,
                request,
            };
        }
        assert_eq!(*writes.pending.lock_recover(), Some((request, Some(false))));
    }
}
