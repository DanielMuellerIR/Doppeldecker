fn shortcut_label(label: &str, macos: &str, other: &str) -> String {
    let shortcut = if cfg!(target_os = "macos") {
        macos
    } else {
        other
    };
    format!("{label}  ({shortcut})")
}

/// Datei- und Pane-Einträge senden ihre `id` als `menu`-Event ans Frontend.
/// Fensteraktionen werden nativ behandelt und behalten ihre Close-/Exitprüfung.
/// Vordefinierte Einträge bringen ihr natives Verhalten selbst mit.
pub(crate) fn build_menu<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> tauri::Result<tauri::menu::Menu<R>> {
    use tauri::menu::{AboutMetadata, MenuBuilder, SubmenuBuilder};

    let about = AboutMetadata {
        name: Some("Doppeldecker".into()),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        ..Default::default()
    };

    let app_menu = SubmenuBuilder::new(app, "Doppeldecker")
        .about(Some(about))
        .separator()
        .text(
            "preferences",
            shortcut_label("Einstellungen …", "⌘,", "Strg+,"),
        )
        .separator()
        .text("check_updates", "Nach Updates suchen …")
        .separator();
    #[cfg(not(target_os = "linux"))]
    let app_menu = app_menu.hide().quit();
    #[cfg(target_os = "linux")]
    let app_menu = app_menu.item(
        &tauri::menu::MenuItemBuilder::with_id("quit", "Beenden")
            .accelerator("Ctrl+Q")
            .build(app)?,
    );
    let app_menu = app_menu.build()?;

    // Datei- und Pane-Kürzel liegen im Frontend, damit Menü und Tastatur sich
    // nicht doppelt auslösen. Reine Fensteraktionen behandelt das native Menü.
    let file = SubmenuBuilder::new(app, "Datei")
        .text("open", shortcut_label("Öffnen", "⌘↓", "Strg+↓"))
        .text(
            "information",
            shortcut_label("Informationen", "⌘I", "Strg+I"),
        )
        .text("compare_files", "Dateien vergleichen")
        .separator()
        .text(
            "new_folder",
            shortcut_label("Neuer Ordner", "⌘⇧N", "Strg+Umschalt+N"),
        )
        .separator()
        .text("files_copy", "Dateien kopieren")
        .text("files_paste", "Dateien einfügen");
    #[cfg(target_os = "linux")]
    let file = file.text("files_cut", "Dateien ausschneiden");
    #[cfg(target_os = "macos")]
    let file = file.text("files_move", "Dateien hierher bewegen  (⌥⌘V)");
    let file = file
        .separator()
        .text("trash", shortcut_label("Löschen …", "⌘⌫", "Strg+Rücktaste"));
    // Nur macOS hat einen Finder; auf Linux gäbe der Eintrag keinen Sinn.
    #[cfg(target_os = "macos")]
    let file = file
        .separator()
        .text("import_finder_favorites", "Finder-Favoriten übernehmen");
    let file = file.build()?;

    let edit = SubmenuBuilder::new(app, "Bearbeiten")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all();
    #[cfg(target_os = "macos")]
    let edit = edit
        .separator()
        .text("paste_move", "Dateien hierher bewegen  (⌥⌘V)");
    let edit = edit.build()?;

    let view = SubmenuBuilder::new(app, "Darstellung")
        .text(
            "view_options",
            shortcut_label("Darstellungsoptionen …", "⌘J", "Strg+J"),
        )
        .separator()
        .text("theme_auto", "Darstellung: Automatisch")
        .text("theme_light", "Darstellung: Hell")
        .text("theme_dark", "Darstellung: Dunkel")
        .separator()
        // Reine Anzeigewahl: dieselben rohen Bytes, andere Schreibweise. Die
        // Voreinstellung setzt das Frontend nach Plattform (macOS dezimal, Linux
        // binär); der Einstellungsdienst merkt jede Änderung prozessweit.
        .text("units_decimal", "Größen: Dezimal (KB, MB, GB)")
        .text("units_binary", "Größen: Binär (KiB, MiB, GiB)")
        .separator()
        .text(
            "toggle_hidden",
            shortcut_label("Versteckte Dateien ein/aus", "⌘⇧.", "Strg+Umschalt+."),
        )
        .text("refresh", shortcut_label("Aktualisieren", "⌘R", "Strg+R"))
        .separator()
        .text("sort_name", "Sortieren nach Name")
        .text("sort_size", "Sortieren nach Größe")
        .text("sort_date", "Sortieren nach Datum")
        .separator()
        .text("swap_panes", "Panes tauschen")
        .build()?;

    // „Befehle" bündelt die Aktionen zwischen den Panes an einer festen Stelle.
    // Der Sync ist bewusst nur hier (und per Kürzel) erreichbar: Er öffnet einen
    // eigenen Modus mit Vorschau statt dauerhafter Kopfleisten-Steuerung.
    let commands = SubmenuBuilder::new(app, "Befehle")
        .text("copy_over", "Ins andere Pane kopieren  (F5)")
        .text("move_over", "Ins andere Pane verschieben  (F6)")
        .text("rename", "Umbenennen  (F2)")
        .separator()
        .text(
            "sync_mode",
            shortcut_label("Synchronisieren …", "⇧⌘S", "Strg+Umschalt+S"),
        )
        .build()?;

    let go = SubmenuBuilder::new(app, "Gehe zu")
        .text("back", shortcut_label("Zurück", "⌘[", "Strg+["))
        .text("forward", shortcut_label("Vor", "⌘]", "Strg+]"))
        .text(
            "up",
            shortcut_label("Übergeordneter Ordner", "⌘↑", "Strg+↑"),
        )
        .separator()
        .text("home", "Benutzerordner")
        .text(
            "goto",
            shortcut_label("Gehe zu Ordner…", "⌘⇧G", "Strg+Umschalt+G"),
        )
        .separator()
        .text("quarantine", "Quarantäne…")
        .build()?;

    let window = SubmenuBuilder::new(app, "Fenster")
        .text(
            "new_window",
            shortcut_label("Neues Fenster", "⌘N", "Strg+N"),
        )
        .separator();
    #[cfg(not(target_os = "linux"))]
    let window = window.minimize().separator().close_window();
    // Tauri/Muda unterstützt diese vordefinierten Fensteraktionen unter Linux
    // nicht. Eigene native Einträge verwenden weiterhin die normalen Exitgates.
    #[cfg(target_os = "linux")]
    let window = window
        .item(
            &tauri::menu::MenuItemBuilder::with_id("minimize_window", "Minimieren")
                .accelerator("Ctrl+M")
                .build(app)?,
        )
        .separator()
        .item(
            &tauri::menu::MenuItemBuilder::with_id("close_window", "Fenster schließen")
                .accelerator("Ctrl+W")
                .build(app)?,
        );
    let window = window.build()?;

    MenuBuilder::new(app)
        .item(&app_menu)
        .item(&file)
        .item(&edit)
        .item(&commands)
        .item(&view)
        .item(&go)
        .item(&window)
        .build()
}

pub(crate) fn handle_menu_event(app: &tauri::AppHandle, event: tauri::menu::MenuEvent) {
    use tauri::Manager;

    let id = event.id().0.as_str();
    #[cfg(target_os = "linux")]
    if id == "quit" {
        app.exit(0);
        return;
    }
    // Menüaktionen gehören ausschließlich zum aktiven Fenster. Ein Rundruf
    // würde beispielsweise F5 in mehreren Panes zugleich auslösen.
    // Unser Helfer bindet das Webview-Ziel: Tauris gleichnamiger Fensterversand
    // erreicht dagegen alle Webviews des Prozesses.
    let target = app
        .webview_windows()
        .into_values()
        .find(|window| window.is_focused().unwrap_or(false))
        .or_else(|| app.get_webview_window("main"));
    if let Some(window) = target {
        #[cfg(target_os = "linux")]
        match id {
            "minimize_window" => {
                let _ = window.minimize();
                return;
            }
            "close_window" => {
                let _ = window.close();
                return;
            }
            _ => {}
        }
        let _ = crate::emit_to_window(&window, "menu", id);
    } else if id == "new_window" {
        let _ = crate::window_commands::new_default_app_window(app);
    }
}

/// AppKit fragt auch bei Cmd-Q und regulären macOS-Beendenereignissen hier nach.
/// Der verwendete Tao-Delegate meldet bislang erst das unabwendbare WillTerminate;
/// die verhinderbare Tauri-Exitprüfung muss vorher erreicht werden.
#[cfg(target_os = "macos")]
pub(crate) fn install_native_exit_gate(app: &tauri::AppHandle) -> Result<(), crate::AppError> {
    use objc2::runtime::{AnyObject, ClassBuilder, Sel};
    use objc2::{sel, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};
    use std::sync::OnceLock;

    static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
    extern "C-unwind" fn should_terminate(
        _delegate: &AnyObject,
        _selector: Sel,
        _sender: &AnyObject,
    ) -> NSApplicationTerminateReply {
        if let Some(app) = APP.get() {
            app.exit(0);
        }
        NSApplicationTerminateReply::TerminateCancel
    }

    let mtm = MainThreadMarker::new().ok_or_else(|| {
        crate::AppError::new("app", "Native Exitprüfung benötigt den Hauptthread")
    })?;
    let delegate = NSApplication::sharedApplication(mtm)
        .delegate()
        .ok_or_else(|| crate::AppError::new("app", "Nativer App-Delegate fehlt"))?;
    let delegate_object: &AnyObject = (*delegate).as_ref();
    let parent = delegate_object.class();
    let mut builder = ClassBuilder::new(c"DoppeldeckerExitDelegate", parent)
        .ok_or_else(|| crate::AppError::new("app", "Native Exitklasse ist bereits registriert"))?;
    // Die Unterklasse fügt keine Ivars hinzu und erhält sämtliche übrigen
    // Delegate-Methoden und deren Zustand. Nur die passende AppKit-Signatur wird
    // ergänzt. Installation und Delegatezugriff erfolgen auf dem Hauptthread.
    unsafe {
        builder.add_method(
            sel!(applicationShouldTerminate:),
            should_terminate as extern "C-unwind" fn(_, _, _) -> _,
        );
    }
    let class = builder.register();
    assert_eq!(class.instance_size(), parent.instance_size());
    APP.set(app.clone())
        .map_err(|_| crate::AppError::new("app", "Native Exitprüfung ist bereits gebunden"))?;
    // Identische Instanzgröße, unmittelbare Unterklasse und kompatible Methode;
    // es gibt vor der Fenstererzeugung keinen parallelen Klassentausch.
    let previous = unsafe { AnyObject::set_class(delegate_object, class) };
    assert!(std::ptr::eq(previous, parent));
    Ok(())
}
