// Doppeldecker — Desktop-Einstiegspunkt.
// `windows_subsystem`-Attribut: auf Windows kein Konsolenfenster (schadet mac/linux nicht).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_commands;
mod app_support;
mod bidirectional_commands;
mod browser_commands;
mod cli_setup;
mod connection_state_commands;
mod desktop_settings;
mod directory_size_commands;
mod favorites_commands;
mod file_clipboard;
mod file_commands;
mod file_intents;
mod ftps_commands;
mod local_sync_commands;
mod menu;
mod mutex;
mod network_discovery_commands;
mod normal_sync_commands;
mod operation_events;
mod pane_tabs;
mod provider_sync_commands;
mod quarantine_commands;
mod queue_commands;
mod queue_runtime;
mod sftp_commands;
mod smb_commands;
mod system_mount_commands;
mod update_commands;
mod webdav_commands;
mod window_commands;

use std::sync::atomic::Ordering;

#[cfg(test)]
use app_support::validate_sandbox_root;
pub(crate) use app_support::{
    app_config_dir, sandbox_root, AppError, MAX_DELETE_ENTRIES, MAX_DELETE_FRACTION,
};
pub(crate) use mutex::MutexExt;
pub(crate) use queue_runtime::require_absolute_remote_parent;
pub(crate) use queue_runtime::{
    begin_profile_transaction, create_remote_folder, emit_queue_snapshot, emit_to_window,
    emit_to_window_label, enqueue_operation_with_profile_bindings, enqueue_operation_with_readback,
    finish_operation, operation_queue, profile_transaction_owned_by, reserve_operation,
    start_queue_worker, take_pending_for_operation, window_data_file_name, with_app_extension,
    with_queue_extension, with_reserved_operation, AppState, WindowState,
};
use tauri::{Manager, WebviewWindowBuilder};

/// Nur abgeschlossene oder ausdrücklich abgebrochene Einträge sind beim Beenden
/// vollständig erledigt. Auch ein fehlgeschlagener Eintrag bleibt in der Queue und
/// kann neu geprüft werden; deshalb gehört er in den sichtbaren Beenden-Hinweis.
pub(crate) fn queue_state_needs_exit_warning(state: doppeldecker_core::QueueState) -> bool {
    !matches!(
        state,
        doppeldecker_core::QueueState::Completed | doppeldecker_core::QueueState::Cancelled
    )
}

fn exit_message_target(app: &tauri::AppHandle) -> Option<tauri::WebviewWindow> {
    app.webview_windows()
        .into_values()
        .find(|window| window.is_focused().unwrap_or(false))
        .or_else(|| app.get_webview_window("main"))
        .or_else(|| app.webview_windows().into_values().next())
}

fn labeled_queue_error(failure: &app_commands::ExitFailure) -> AppError {
    AppError::new(
        &failure.code,
        format!(
            "Queue-Journal des Fensters „{}“: {}",
            failure.label, failure.message
        ),
    )
}

/// Meldet einen appweiten Exit-Blocker an ein tatsächlich sichtbares Fenster. Nur
/// ein reiner Befund offener Queues bekommt einen bestätigbaren Einmaltoken;
/// aktive Operationen und Journalfehler bleiben immer fail-closed.
fn report_exit_blockers(
    app: &tauri::AppHandle,
    state: &AppState,
    assessment: &app_commands::ExitSnapshot,
) {
    let Some(target) = exit_message_target(app) else {
        return;
    };
    for failure in assessment.failures() {
        let _ = crate::emit_to_window(&target, "queue-error", labeled_queue_error(failure));
    }
    if assessment.has_active_operations() {
        let labels = assessment
            .windows
            .iter()
            .filter(|window| window.active_operation)
            .map(|window| window.label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if !labels.is_empty() {
            let _ = crate::emit_to_window(
                &target,
                "window-close-blocked",
                format!("In diesen Fenstern laufen noch Dateioperationen: {labels}."),
            );
        }
        if assessment.profile_transaction_active {
            let _ = crate::emit_to_window(
                &target,
                "window-close-blocked",
                "Ein Verbindungsprofil wird gerade sicher gespeichert.",
            );
        }
    }
    if assessment.has_pending_preparations() {
        let pending = assessment
            .windows
            .iter()
            .filter(|window| !window.pending_preparations.is_empty())
            .map(|window| {
                format!(
                    "{} ({})",
                    window.label,
                    window.pending_preparations.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let _ = crate::emit_to_window(&target,
            "window-close-blocked",
            format!(
                "Noch nicht gestartete Vorbereitungen müssen zuerst ausgeführt oder verworfen werden: {pending}."
            ),
        );
    }
    if assessment.can_confirm_open_queues() {
        let request = app_commands::issue_exit_confirmation(state, assessment.clone());
        if crate::emit_to_window(&target, "queue-exit-requested", &request).is_err() {
            // Ohne zugestellten Dialog existiert auch keine gültige Bestätigung.
            app_commands::clear_exit_confirmation(state, &request.token);
        }
    } else {
        state.pending_exit_confirmation.lock_recover().take();
    }
}

fn last_window_must_stay_open(
    remaining_open_windows: usize,
    assessment: &app_commands::ExitSnapshot,
) -> bool {
    remaining_open_windows == 0 && assessment.must_prevent_exit()
}

fn remaining_open_windows(app: &tauri::AppHandle, state: &AppState) -> usize {
    count_remaining_window_labels(app.webview_windows().into_keys(), state)
}

fn count_remaining_window_labels(
    labels: impl IntoIterator<Item = String>,
    state: &AppState,
) -> usize {
    labels
        .into_iter()
        .filter(|label| !state.window(label).closing.load(Ordering::Acquire))
        .count()
}

/// Führt den sicherheitsrelevanten Start strikt vor der ersten Webview aus. Die
/// injizierten Schritte halten diese Reihenfolge auch ohne echte GUI testbar.
fn run_startup_steps<T>(
    acquire_process_lock: impl FnOnce() -> Result<(), AppError>,
    prepare_queues: impl FnOnce() -> Result<T, AppError>,
    build_main_window: impl FnOnce() -> Result<(), AppError>,
    show_recovery_windows: impl FnOnce(T),
) -> Result<(), AppError> {
    acquire_process_lock()?;
    let recovery = prepare_queues()?;
    build_main_window()?;
    show_recovery_windows(recovery);
    Ok(())
}

fn build_main_window(app: &tauri::AppHandle) -> Result<(), AppError> {
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .ok_or_else(|| AppError::new("window", "Die Hauptfensterkonfiguration fehlt"))?;
    if config.create {
        return Err(AppError::new(
            "window",
            "Das Hauptfenster muss bis nach ProcessLock und Recovery deaktiviert bleiben",
        ));
    }
    // `from_config` übernimmt URL, Größen und plattformspezifische Titelleiste aus
    // genau demselben Objekt, das früher das automatische Fenster erzeugte.
    let window = WebviewWindowBuilder::from_config(app, config)
        .map_err(|error| AppError::new("window", error.to_string()))?
        .build()
        .map_err(|error| AppError::new("window", error.to_string()))?;
    if let Err(error) = desktop_settings::apply_window_icon(&window) {
        let _ = app_support::record_app_error(app, "Fenstericon", Some(&error.code));
    }
    window_commands::apply_restored_geometry(&window);
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .menu(menu::build_menu)
        .on_menu_event(menu::handle_menu_event)
        .on_window_event(|window, event| {
            let tauri::WindowEvent::CloseRequested { api, .. } = event else {
                return;
            };
            let app = window.app_handle();
            let app_state = app.state::<AppState>();
            let state = app_state.window(window.label());
            let _barrier = app_state.exit_barrier.lock_recover();
            state.closing.store(true, Ordering::Release);
            let active = state.active_cancel.lock_recover().is_some();
            let pending = state.pending_preparation_kinds();
            let unfinished = match operation_queue(app, &state, window.label()) {
                Ok(queue) => queue
                    .snapshot()
                    .entries
                    .iter()
                    .filter(|entry| queue_state_needs_exit_warning(entry.state))
                    .count(),
                Err(error) => {
                    // Der echte Öffnungsfehler bleibt erhalten. Ein erfundener
                    // „offener Eintrag“ würde die nötige Recoverydiagnose verdecken.
                    api.prevent_close();
                    queue_runtime::reopen_window_operation_gate(app, window.label(), &state);
                    let _ = crate::emit_to_window_label(app, window.label(), "queue-error", error);
                    return;
                }
            };
            let profile_transaction = profile_transaction_owned_by(&app_state, window.label());
            if active || profile_transaction || !pending.is_empty() || unfinished > 0 {
                api.prevent_close();
                queue_runtime::reopen_window_operation_gate(app, window.label(), &state);
                let reason = if profile_transaction {
                    "Dieses Fenster speichert gerade ein Verbindungsprofil."
                } else if active {
                    "In diesem Fenster läuft noch eine Dateioperation."
                } else if !pending.is_empty() {
                    "Dieses Fenster besitzt noch eine nicht gestartete Vorbereitung."
                } else {
                    "Die Queue dieses Fensters enthält noch offene Einträge."
                };
                let _ = crate::emit_to_window_label(
                    app,
                    window.label(),
                    "window-close-blocked",
                    reason,
                );
                return;
            }

            // Ein fremdes beschädigtes Journal blockiert nicht das Schließen eines
            // einzelnen sauberen Fensters. Beim letzten lebenden Fenster muss der
            // appweite Befund aber noch dort sichtbar werden, bevor es verschwände.
            let assessment = app_commands::assess_app_exit(app, &app_state);
            // Einmal zählen genügt: Zwischen hier und der zweiten Verwendung unten
            // läuft nur das Verwerfen der Einmal-IDs dieses Fensters, und das rührt
            // kein `closing`-Flag an — der Wert kann sich also nicht ändern.
            let remaining = remaining_open_windows(app, &app_state);
            if last_window_must_stay_open(remaining, &assessment) {
                api.prevent_close();
                queue_runtime::reopen_window_operation_gate(app, window.label(), &state);
                report_exit_blockers(app, &app_state, &assessment);
                return;
            }
            // Ein geschlossenes Fenster darf keine von ihm erfasste Einmal-ID im
            // RAM zurücklassen. Noch laufende native Dialoge prüfen den Besitzer
            // zusätzlich unmittelbar vor dem Eintragen.
            sftp_commands::discard_connection_secrets_for_owner(&app_state, window.label());
            let last_window = remaining == 0;
            // Auf macOS lässt das Schließen des letzten Fensters den Prozess
            // weiterlaufen; dieses Fenster soll aus der nächsten Sitzung
            // verschwinden. Auf Linux beendet derselbe Schritt die App. Ein bereits
            // laufender appweiter Exit erhält auf beiden Systemen den zuletzt
            // geschriebenen vollständigen Fensterstand.
            let preserve_for_exit = app_state.exit_in_progress.load(Ordering::Acquire)
                || (last_window && !cfg!(target_os = "macos"));
            desktop_settings::schedule_forget_closed_window(app, window.label(), preserve_for_exit);
        })
        .manage(AppState::default())
        .manage(desktop_settings::DesktopState::default())
        .manage(update_commands::UpdateState::default())
        .setup(|app| {
            run_startup_steps(
                || {
                    #[cfg(target_os = "macos")]
                    menu::install_native_exit_gate(app.handle())?;
                    queue_runtime::initialize_queue_process_lock(app.handle())?;
                    desktop_settings::initialize(app.handle())
                },
                || {
                    // Erst die Recovery-Planung: Sie weiß, welche Nebenfenster noch
                    // eine offene Queue haben, und nur ein sicher beendetes darf
                    // zum neuen Hauptfenster werden.
                    let recovery = window_commands::prepare_incomplete_queue_windows(app.handle())?;
                    let unavailable = recovery.unavailable_labels();
                    let session =
                        window_commands::prepare_session_windows(app.handle(), &unavailable)?;
                    Ok((recovery, session))
                },
                || build_main_window(app.handle()),
                |(recovery, session)| {
                    window_commands::show_incomplete_queue_windows(app.handle(), recovery);
                    window_commands::show_session_windows(app.handle(), &session);
                },
            )
            .map_err(|error| {
                std::io::Error::other(format!(
                    "Doppeldecker wurde vor der Fenstererzeugung sicher gestoppt: {}",
                    error.message
                ))
            })?;
            // Linux setzt das Icon bereits beim Bau jedes Fensters. Nur macOS hat
            // stattdessen ein prozessweites Dockicon, das einmal nach dem Start
            // aktualisiert werden muss.
            #[cfg(target_os = "macos")]
            {
                if let Err(error) = desktop_settings::apply_icon(app.handle()) {
                    window_commands::append_startup_app_error(
                        &app.state::<AppState>(),
                        format!("App-Icon konnte nicht gesetzt werden: {}", error.message),
                    );
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            window_commands::new_app_window,
            window_commands::take_window_start_state,
            window_commands::take_startup_queue_error,
            window_commands::take_startup_app_error,
            desktop_settings::desktop_preferences,
            desktop_settings::update_desktop_preferences,
            desktop_settings::remember_window,
            desktop_settings::restored_pane_tabs,
            desktop_settings::restored_active_side,
            browser_commands::default_dirs,
            browser_commands::scan_dir,
            browser_commands::open_path,
            browser_commands::open_project_page,
            browser_commands::open_remote_snapshot,
            browser_commands::diff_tool_status,
            browser_commands::compare_files,
            system_mount_commands::list_system_mounts,
            system_mount_commands::eject_system_mount,
            network_discovery_commands::discover_network_services,
            directory_size_commands::start_directory_sizes,
            directory_size_commands::cancel_directory_sizes,
            local_sync_commands::plan_sync,
            normal_sync_commands::prepare_local_sync,
            normal_sync_commands::discard_local_sync,
            normal_sync_commands::enqueue_local_sync,
            normal_sync_commands::local_sync_queue_preview,
            normal_sync_commands::approve_local_sync_queue_entry,
            local_sync_commands::prepare_real_sync,
            local_sync_commands::discard_real_sync,
            local_sync_commands::run_real_sync,
            local_sync_commands::run_sync,
            queue_commands::cancel_sync,
            queue_commands::list_operation_queue,
            queue_commands::set_queue_paused,
            queue_commands::set_queue_stop_on_error,
            queue_commands::move_queue_entry,
            queue_commands::remove_queue_entry,
            queue_commands::retry_queue_entry,
            queue_commands::discard_replan_required_queue_entry,
            queue_commands::discard_queue_confirmation,
            queue_commands::acknowledge_legacy_webdav_delete,
            queue_commands::acknowledge_unclear_queue_outcome,
            queue_commands::approve_queue_entry,
            bidirectional_commands::prepare_bidirectional_sync,
            bidirectional_commands::register_bidirectional_pair,
            bidirectional_commands::prepare_bidirectional_resume,
            bidirectional_commands::discard_bidirectional_recovery_preparation,
            bidirectional_commands::preview_bidirectional_conflicts,
            bidirectional_commands::discard_bidirectional_sync,
            bidirectional_commands::enqueue_bidirectional_sync,
            bidirectional_commands::run_bidirectional_sync,
            bidirectional_commands::approve_bidirectional_queue_entry,
            bidirectional_commands::prepare_bidirectional_rollback,
            bidirectional_commands::run_bidirectional_rollback,
            bidirectional_commands::finish_bidirectional_completion_recovery,
            provider_sync_commands::prepare_provider_sync,
            provider_sync_commands::discard_provider_sync,
            provider_sync_commands::run_provider_sync,
            provider_sync_commands::enqueue_provider_sync,
            provider_sync_commands::approve_provider_queue_entry,
            sftp_commands::discover_sftp_host_key,
            sftp_commands::list_sftp_agent_keys,
            sftp_commands::capture_connection_secret,
            sftp_commands::discard_connection_secret,
            sftp_commands::list_sftp_profiles,
            sftp_commands::save_sftp_profile,
            sftp_commands::remove_sftp_profile,
            sftp_commands::connect_sftp_profile,
            sftp_commands::scan_sftp_dir,
            sftp_commands::prepare_sftp_sync,
            sftp_commands::discard_sftp_sync,
            sftp_commands::run_sftp_sync,
            sftp_commands::enqueue_sftp_sync,
            sftp_commands::approve_sftp_queue_entry,
            sftp_commands::list_sftp_quarantine_runs,
            sftp_commands::list_sftp_quarantine_entries,
            sftp_commands::restore_sftp_quarantine_entry,
            sftp_commands::purge_sftp_quarantine_run,
            ftps_commands::discover_ftps_certificate,
            ftps_commands::list_ftps_profiles,
            ftps_commands::save_ftps_profile,
            ftps_commands::remove_ftps_profile,
            ftps_commands::connect_ftps_profile,
            ftps_commands::scan_ftps_dir,
            ftps_commands::prepare_ftps_sync,
            ftps_commands::discard_ftps_sync,
            ftps_commands::run_ftps_sync,
            ftps_commands::enqueue_ftps_sync,
            ftps_commands::approve_ftps_queue_entry,
            webdav_commands::discover_webdav_certificate,
            webdav_commands::list_webdav_profiles,
            webdav_commands::save_webdav_profile,
            webdav_commands::remove_webdav_profile,
            webdav_commands::connect_webdav_profile,
            webdav_commands::scan_webdav_dir,
            webdav_commands::prepare_webdav_sync,
            webdav_commands::discard_webdav_sync,
            webdav_commands::run_webdav_sync,
            webdav_commands::enqueue_webdav_sync,
            webdav_commands::approve_webdav_queue_entry,
            smb_commands::list_smb_profiles,
            smb_commands::discover_smb_server,
            smb_commands::save_smb_profile,
            smb_commands::remove_smb_profile,
            smb_commands::connect_smb_profile,
            smb_commands::scan_smb_dir,
            smb_commands::prepare_smb_sync,
            smb_commands::discard_smb_sync,
            smb_commands::run_smb_sync,
            smb_commands::enqueue_smb_sync,
            smb_commands::approve_smb_queue_entry,
            smb_commands::list_smb_quarantine_runs,
            smb_commands::list_smb_quarantine_entries,
            smb_commands::restore_smb_quarantine_entry,
            smb_commands::purge_smb_quarantine_run,
            local_sync_commands::safety_info,
            quarantine_commands::list_quarantine_runs,
            quarantine_commands::list_quarantine_entries,
            quarantine_commands::restore_quarantine_entry,
            quarantine_commands::purge_quarantine_run,
            quarantine_commands::list_provider_quarantine_runs,
            quarantine_commands::list_provider_quarantine_entries,
            quarantine_commands::restore_provider_quarantine_entry,
            quarantine_commands::purge_provider_quarantine_run,
            file_commands::check_conflicts,
            file_commands::check_local_file_operation,
            file_commands::check_provider_file_operation,
            file_commands::check_provider_delete,
            file_commands::copy_paths,
            file_commands::move_paths,
            file_commands::enqueue_provider_file_operation,
            file_clipboard::write_file_clipboard,
            file_clipboard::read_file_clipboard,
            file_clipboard::clear_file_clipboard_if_unchanged,
            file_intents::enqueue_provider_file_intent,
            file_commands::enqueue_provider_delete,
            file_commands::trash_paths,
            file_commands::create_folder,
            file_commands::create_provider_folder,
            file_commands::provider_folder_contract,
            file_commands::rename_path,
            file_commands::rename_provider_path,
            file_commands::provider_rename_contract,
            favorites_commands::list_favorites,
            favorites_commands::add_favorite,
            favorites_commands::add_remote_favorite,
            favorites_commands::move_favorite,
            favorites_commands::remove_favorite,
            favorites_commands::import_finder_favorites,
            connection_state_commands::list_active_connections,
            connection_state_commands::disconnect_remote_profile,
            app_commands::app_version,
            app_commands::quit_after_queue_warning,
            app_commands::record_app_error,
            update_commands::check_for_update,
            update_commands::prepared_update_status,
            update_commands::install_update,
            cli_setup::cli_install_status,
            cli_setup::decline_cli_install,
            cli_setup::install_cli,
        ])
        .build(tauri::generate_context!())
        .expect("Fehler beim Starten von Doppeldecker")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows: false,
                ..
            } = &event
            {
                let _ = window_commands::new_default_app_window(app);
                return;
            }
            let tauri::RunEvent::ExitRequested { code, api, .. } = event else {
                return;
            };
            // Tauri fordert auch nach dem letzten geschlossenen Fenster einen
            // Exit ohne Code an. Auf macOS bleibt die App dabei weiter nutzbar;
            // explizites Beenden läuft mit Code durch die vollständige Prüfung.
            #[cfg(target_os = "macos")]
            if code.is_none() {
                api.prevent_exit();
                return;
            }
            let state = app.state::<AppState>();
            // Das appweite Gate wird vor dem ersten Snapshot geschlossen. Dadurch
            // kann nach der Prüfung kein paralleler Command mehr eine neue
            // Operation reservieren und unbemerkt in den Prozess-Exit laufen.
            let _barrier = state.exit_barrier.lock_recover();
            state.exit_in_progress.store(true, Ordering::Release);
            if desktop_settings::defer_exit_for_session_writes(app, code) {
                api.prevent_exit();
                return;
            }
            let assessment = app_commands::assess_app_exit(app, &state);
            if !assessment.must_prevent_exit() {
                state.pending_exit_confirmation.lock_recover().take();
                state.authorized_exit.lock_recover().take();
                return;
            }
            // Auch `app.exit(0)` wird vollständig neu geprüft. Nur der unmittelbar
            // zuvor autorisierte, unveränderte Snapshot darf den Blocker passieren.
            if code.is_some() && app_commands::take_matching_exit_authorization(&state, &assessment)
            {
                return;
            }
            api.prevent_exit();
            report_exit_blockers(app, &state, &assessment);
            queue_runtime::reopen_app_operation_gate(app, &state);
        });
}

#[cfg(test)]
mod event_scope_tests {
    /// Tauri v2 stellt `Emitter` auch auf einem `WebviewWindow` über den prozessweiten
    /// `AppManager` zu — der Versand erreicht damit jeden Webview. Fortschritt,
    /// Abschluss, Fehler und Menüaktionen gehören aber ausschließlich zum
    /// verursachenden Fenster: Sonst entsperrt der Abschluss eines fremden Laufs die
    /// Oberfläche eines noch arbeitenden Fensters, und ein Menübefehl löst dieselbe
    /// Dateiaktion in jedem Fenster auf dessen eigener Auswahl aus. Der Test hält
    /// mechanisch fest, dass die App-Schicht nur noch fensterbezogen sendet.
    #[test]
    fn fensterereignisse_gehen_nie_als_rundruf_hinaus() {
        // Prozessweite Zustände dürfen und sollen alle Fenster erreichen; die
        // Begründung steht an der Funktion selbst.
        const RUNDRUF_ERLAUBT: &[&str] = &["connection_state_commands.rs"];
        // Zusammengesetzt, damit die Suchzeichenkette nicht im eigenen Quelltext steht
        // und der Test sich nicht selbst findet.
        let rundruf = format!(".{}(", "emit");

        let quellen = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut treffer = Vec::new();
        for eintrag in std::fs::read_dir(&quellen).unwrap() {
            let pfad = eintrag.unwrap().path();
            if pfad.extension().and_then(|endung| endung.to_str()) != Some("rs") {
                continue;
            }
            let name = pfad.file_name().unwrap().to_string_lossy().into_owned();
            if RUNDRUF_ERLAUBT.contains(&name.as_str()) {
                continue;
            }
            let inhalt = std::fs::read_to_string(&pfad).unwrap();
            for (nummer, zeile) in inhalt.lines().enumerate() {
                if zeile.contains(&rundruf) {
                    treffer.push(format!("{name}:{}: {}", nummer + 1, zeile.trim()));
                }
            }
        }
        assert!(
            treffer.is_empty(),
            "Rundruf statt fensterbezogenem Versand — crate::emit_to_window benutzen:\n{}",
            treffer.join("\n")
        );
    }
}

#[cfg(test)]
mod listing_binding_tests {
    /// Listing und Sitzungsgeneration müssen aus demselben gehaltenen Providerzustand
    /// stammen.
    ///
    /// Die Provider-Verbindungen sind prozessweit geteilt. Liest ein Command erst das
    /// Listing und danach erneut die Generation, kann dazwischen ein Reconnect aus
    /// einem anderen Fenster liegen: Die Einträge der alten Sitzung tragen dann das
    /// Etikett der neuen. Genau daran hängt die Driftbindung von Pane, Ordnergrösse
    /// und Folgeoperation. Für SFTP hielt ein eigener Test das schon fest; SMB, FTPS
    /// und WebDAV hatten dieselbe Stelle offen.
    #[test]
    fn scan_commands_binden_listing_und_generation_atomar() {
        let quellen = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // Zusammengesetzt, damit die Suchzeichenkette nicht im eigenen Quelltext steht.
        let getrennter_zugriff = format!("connection.session_{}()", "generation");

        for (datei, funktion) in [
            ("sftp_commands.rs", "pub(crate) async fn scan_sftp_dir("),
            ("smb_commands.rs", "pub(crate) async fn scan_smb_dir("),
            ("ftps_commands.rs", "pub(crate) async fn scan_ftps_dir("),
            ("webdav_commands.rs", "pub(crate) async fn scan_webdav_dir("),
        ] {
            let inhalt = std::fs::read_to_string(quellen.join(datei)).unwrap();
            let start = inhalt
                .find(funktion)
                .unwrap_or_else(|| panic!("{datei}: {funktion} nicht gefunden"));
            // Bis zum nächsten Command — das ist die Grenze der Funktion.
            let rest = &inhalt[start + funktion.len()..];
            let ende = rest.find("#[tauri::command]").unwrap_or(rest.len());
            let rumpf = &rest[..ende];

            assert!(
                rumpf.contains("list_interactive"),
                "{datei}: das Pane-Listing muss die gebundene Variante benutzen"
            );
            assert!(
                rumpf.contains("session_generation: listing.session_generation"),
                "{datei}: die Antwort muss die Generation des Listings tragen"
            );
            assert!(
                !rumpf.contains(&getrennter_zugriff),
                "{datei}: Listing und Generation dürfen nicht aus zwei getrennten \
                 State-Snapshots stammen"
            );
        }
    }
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;
    use doppeldecker_core::{
        Cancel, FileQueueKind, LocalVfs, OperationQueue, QueueOperation, QueueState, SyncOptions,
        TEST_SANDBOX_MARKER, TEST_SANDBOX_MARKER_CONTENT,
    };
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// Exakten Marker schreiben; schon ein ähnlicher Text darf die Sandbox nicht
    /// freischalten, deshalb nutzen alle positiven Tests dieselbe Konstante wie die App.
    fn mark(root: &Path) {
        std::fs::write(root.join(TEST_SANDBOX_MARKER), TEST_SANDBOX_MARKER_CONTENT).unwrap();
    }

    fn local_queue_execution() -> doppeldecker_core::LocalQueueExecution {
        doppeldecker_core::LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: MAX_DELETE_ENTRIES,
            max_delete_fraction: MAX_DELETE_FRACTION,
        }
    }

    #[test]
    fn beenden_warnung_zaehlt_jeden_nicht_erledigten_queue_zustand() {
        for state in [
            QueueState::Pending,
            QueueState::Validating,
            QueueState::Running,
            QueueState::AwaitingConfirmation,
            QueueState::Failed,
            QueueState::Interrupted,
        ] {
            assert!(queue_state_needs_exit_warning(state), "Zustand {state:?}");
        }
        for state in [QueueState::Completed, QueueState::Cancelled] {
            assert!(!queue_state_needs_exit_warning(state), "Zustand {state:?}");
        }
    }

    #[test]
    fn setup_erwirbt_prozesslock_vor_journal_recovery() {
        let source = include_str!("main.rs");
        let setup = &source[source.find(".setup(|app|").unwrap()..];
        let lock = setup.find("initialize_queue_process_lock").unwrap();
        let recovery = setup.find("prepare_incomplete_queue_windows").unwrap();
        let main_window = setup.find("build_main_window").unwrap();
        let recovery_windows = setup.find("show_incomplete_queue_windows").unwrap();
        assert!(lock < recovery && recovery < main_window && main_window < recovery_windows);
    }

    #[test]
    fn hauptfenster_bleibt_bis_nach_lock_und_recovery_deaktiviert() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let window = &config["app"]["windows"][0];
        assert_eq!(window["label"], "main");
        assert_eq!(window["url"], "index.html");
        assert_eq!(window["create"], false);
        assert_eq!(window["width"], 1180);
        assert_eq!(window["height"], 760);
        assert_eq!(window["minWidth"], 760);
        assert_eq!(window["minHeight"], 480);
        assert_eq!(window["titleBarStyle"], "Overlay");
        assert_eq!(window["hiddenTitle"], true);
    }

    #[test]
    fn headless_startsequenz_baut_bei_lock_oder_recoveryfehler_kein_fenster() {
        use std::cell::RefCell;

        let calls = RefCell::new(Vec::new());
        let error = run_startup_steps(
            || {
                calls.borrow_mut().push("lock");
                Err(AppError::new("queue", "fremder Prozess"))
            },
            || {
                calls.borrow_mut().push("recovery");
                Ok(())
            },
            || {
                calls.borrow_mut().push("window");
                Ok(())
            },
            |_: ()| calls.borrow_mut().push("recovery-windows"),
        )
        .unwrap_err();
        assert_eq!(error.code, "queue");
        assert_eq!(*calls.borrow(), ["lock"]);

        calls.borrow_mut().clear();
        let error = run_startup_steps(
            || {
                calls.borrow_mut().push("lock");
                Ok(())
            },
            || {
                calls.borrow_mut().push("recovery");
                Err(AppError::new("queue", "Recoveryfehler"))
            },
            || {
                calls.borrow_mut().push("window");
                Ok(())
            },
            |_: ()| calls.borrow_mut().push("recovery-windows"),
        )
        .unwrap_err();
        assert_eq!(error.code, "queue");
        assert_eq!(*calls.borrow(), ["lock", "recovery"]);

        calls.borrow_mut().clear();
        run_startup_steps(
            || {
                calls.borrow_mut().push("lock");
                Ok(())
            },
            || {
                calls.borrow_mut().push("recovery");
                Ok(())
            },
            || {
                calls.borrow_mut().push("window");
                Ok(())
            },
            |_: ()| calls.borrow_mut().push("recovery-windows"),
        )
        .unwrap();
        assert_eq!(
            *calls.borrow(),
            ["lock", "recovery", "window", "recovery-windows"]
        );
    }

    #[test]
    fn nativer_exit_bleibt_bei_jedem_queue_oeffnungsfehler_gesperrt() {
        let labels = vec!["main".to_string(), "window-1".to_string()];
        let assessment = app_commands::assess_exit(&labels, false, |label| match label {
            "main" => (false, Ok(Vec::new())),
            _ => (false, Err(AppError::new("io", "Journal beschädigt"))),
        });

        assert!(assessment.must_prevent_exit());
        assert_eq!(assessment.open_entries(), 0);
        let failure = assessment.failures().next().unwrap();
        assert_eq!(failure.label, "window-1");
        assert_eq!(failure.code, "io");
        assert!(labeled_queue_error(failure).message.contains("window-1"));
    }

    #[test]
    fn nur_das_letzte_fenster_bleibt_wegen_unsichtbarem_journal_offen() {
        let assessment = app_commands::assess_exit(&["window-defekt".into()], false, |_| {
            (false, Err(AppError::new("io", "Journal beschädigt")))
        });

        assert!(!last_window_must_stay_open(1, &assessment));
        assert!(last_window_must_stay_open(0, &assessment));
    }

    #[test]
    fn pending_in_einem_fenster_blockiert_app_exit_aber_nicht_fremdes_einzelfenster() {
        let assessment = app_commands::assess_exit_with_pending(
            &["main".into(), "window-1".into()],
            false,
            |label| {
                let pending = if label == "window-1" {
                    vec!["bidirectional-real".to_owned()]
                } else {
                    Vec::new()
                };
                (false, pending, Ok(Vec::new()))
            },
        );

        assert!(assessment.must_prevent_exit());
        assert!(!last_window_must_stay_open(1, &assessment));
        assert!(last_window_must_stay_open(0, &assessment));
    }

    #[test]
    fn neue_operation_bleibt_nach_begonnener_exitpruefung_gesperrt() {
        let state = AppState::default();
        let window = state.window("main");
        state.exit_in_progress.store(true, Ordering::Release);

        let error = match reserve_operation(&window) {
            Ok(_) => panic!("Exit-Gate ließ eine neue Operation zu"),
            Err(error) => error,
        };

        assert_eq!(error.code, "busy");
        state.exit_in_progress.store(false, Ordering::Release);
        assert!(reserve_operation(&window).is_ok());
    }

    #[test]
    fn zweite_profiltransaktion_wartet_nicht_mit_gehaltener_exit_barriere() {
        let state = Arc::new(AppState::default());
        let first = state.window("main");
        let transaction = begin_profile_transaction(&state, "main", &first).unwrap();
        let second_state = state.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let window = second_state.window("window-1");
            let result = begin_profile_transaction(&second_state, "window-1", &window)
                .map(|_| "ok".to_owned())
                .unwrap_or_else(|error| error.code);
            sender.send(result).unwrap();
        });
        let result = receiver.recv_timeout(std::time::Duration::from_millis(200));
        // Auch im roten Gegenlauf den blockierten Worker freigeben und ernten.
        drop(transaction);
        worker.join().unwrap();
        assert_eq!(result.unwrap(), "busy");
        assert!(state.exit_barrier.try_lock().is_ok());
        assert!(!state.profile_transaction_active.load(Ordering::Acquire));
    }

    #[test]
    fn profiltransaktion_blockiert_exit_und_startet_nicht_im_exit() {
        let state = AppState::default();
        let window = state.window("main");
        let transaction = begin_profile_transaction(&state, "main", &window).unwrap();
        assert!(profile_transaction_owned_by(&state, "main"));
        assert!(!profile_transaction_owned_by(&state, "window-1"));
        let during = app_commands::assess_exit(
            &["main".into()],
            state.profile_transaction_active.load(Ordering::Acquire),
            |_| (false, Ok(Vec::new())),
        );
        assert!(during.must_prevent_exit());

        drop(transaction);
        let after = app_commands::assess_exit(
            &["main".into()],
            state.profile_transaction_active.load(Ordering::Acquire),
            |_| (false, Ok(Vec::new())),
        );
        assert!(!after.must_prevent_exit());

        state.exit_in_progress.store(true, Ordering::Release);
        let error = match begin_profile_transaction(&state, "main", &window) {
            Ok(_) => panic!("Exit-Gate ließ eine neue Profiltransaktion zu"),
            Err(error) => error,
        };
        assert_eq!(error.code, "busy");
        assert!(!state.profile_transaction_active.load(Ordering::Acquire));
    }

    #[test]
    fn profiltransaktion_beginnt_nicht_hinter_dem_closing_gate() {
        let state = Arc::new(AppState::default());
        let window = state.window("main");
        let barrier = state.exit_barrier.lock_recover();
        let begin_state = state.clone();
        let begin_window = window.clone();
        let begin = std::thread::spawn(move || {
            begin_profile_transaction(&begin_state, "main", &begin_window)
                .map(|_| "ok".to_owned())
                .unwrap_or_else(|error| error.code)
        });

        // CloseRequested setzt dieses Gate unter derselben Barriere. Der bereits
        // wartende Profilstart darf nach ihrer Freigabe keine Schlüsselbundwirkung
        // mehr beginnen.
        window.closing.store(true, Ordering::Release);
        drop(barrier);
        assert_eq!(begin.join().unwrap(), "busy");
        assert!(!state.profile_transaction_active.load(Ordering::Acquire));
    }

    #[test]
    fn close_prueft_profilbesitz_ohne_auf_transaktionsmutex_zu_warten() {
        let state = Arc::new(AppState::default());
        let owner = state.window("window-1");
        let transaction = begin_profile_transaction(&state, "window-1", &owner).unwrap();
        let close_state = state.clone();
        let close_owner = owner.clone();
        let (checked_tx, checked_rx) = std::sync::mpsc::channel();

        let close = std::thread::spawn(move || {
            let _barrier = close_state.exit_barrier.lock_recover();
            close_owner.closing.store(true, Ordering::Release);
            checked_tx
                .send(profile_transaction_owned_by(&close_state, "window-1"))
                .unwrap();
        });

        assert!(checked_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("Close-Prüfung wartete fälschlich auf die laufende Profiltransaktion"));
        drop(transaction);
        close.join().unwrap();
    }

    #[test]
    fn clean_exit_gewinnt_atomisch_gegen_reales_enqueue() {
        let temp = tempfile::tempdir().unwrap();
        let queue = Arc::new(OperationQueue::open(temp.path().join("queue.json")).unwrap());
        let state = Arc::new(AppState::default());
        let window = state.window("main");
        let (exit_ready_tx, exit_ready_rx) = std::sync::mpsc::channel();
        let (finish_exit_tx, finish_exit_rx) = std::sync::mpsc::channel();

        let exit_state = state.clone();
        let exit_queue = queue.clone();
        let exit = std::thread::spawn(move || {
            let _barrier = exit_state.exit_barrier.lock_recover();
            exit_state.exit_in_progress.store(true, Ordering::Release);
            exit_ready_tx.send(()).unwrap();
            finish_exit_rx.recv().unwrap();
            assert!(exit_queue.snapshot().entries.is_empty());
        });
        exit_ready_rx.recv().unwrap();

        let enqueue_state = state.clone();
        let enqueue_window = window.clone();
        let enqueue_queue = queue.clone();
        let enqueue = std::thread::spawn(move || {
            with_queue_extension(&enqueue_state, &enqueue_window, || {
                enqueue_queue
                    .enqueue(QueueOperation::File {
                        kind: FileQueueKind::Copy,
                        items: vec!["/tmp/quelle".into()],
                        destination: "/tmp/ziel".into(),
                        overwrite: false,
                        expected_conflicts: Vec::new(),
                        keep_both_targets: Vec::new(),
                        expected_conflict_proofs: Vec::new(),
                        local_replace_best_effort_expected: Some(false),
                        local_copy_new_best_effort_expected: None,
                    })
                    .map_err(AppError::from)
            })
        });
        finish_exit_tx.send(()).unwrap();
        exit.join().unwrap();
        assert_eq!(enqueue.join().unwrap().unwrap_err().code, "busy");
        assert!(queue.snapshot().entries.is_empty());
    }

    #[test]
    fn closing_gate_stoppt_start_und_erkennt_zwei_gleichzeitige_fenster() {
        let state = AppState::default();
        let first = state.window("main");
        let second = state.window("window-1");
        let assessment = app_commands::assess_exit(&["hidden".into()], false, |_| {
            (false, Err(AppError::new("io", "Journal beschädigt")))
        });

        {
            let _barrier = state.exit_barrier.lock_recover();
            first.closing.store(true, Ordering::Release);
        }
        let error = match reserve_operation(&first) {
            Ok(_) => panic!("Closing-Gate ließ eine neue Operation zu"),
            Err(error) => error,
        };
        assert_eq!(error.code, "busy");
        assert_eq!(
            count_remaining_window_labels(vec!["main".into(), "window-1".into()], &state),
            1
        );
        assert!(!last_window_must_stay_open(1, &assessment));

        {
            let _barrier = state.exit_barrier.lock_recover();
            second.closing.store(true, Ordering::Release);
        }
        assert_eq!(
            count_remaining_window_labels(vec!["main".into(), "window-1".into()], &state),
            0
        );
        assert!(last_window_must_stay_open(0, &assessment));
    }

    #[test]
    fn markierter_temp_ordner_wird_akzeptiert() {
        let root = tempfile::tempdir().unwrap();
        mark(root.path());
        assert_eq!(
            validate_sandbox_root(root.path().to_str().unwrap()).unwrap(),
            std::fs::canonicalize(root.path()).unwrap()
        );
    }

    #[test]
    fn fehlender_oder_falscher_marker_wird_abgelehnt() {
        let root = tempfile::tempdir().unwrap();
        assert!(validate_sandbox_root(root.path().to_str().unwrap()).is_err());
        std::fs::write(root.path().join(TEST_SANDBOX_MARKER), "fast richtig\n").unwrap();
        assert!(validate_sandbox_root(root.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn git_und_cloud_ordner_werden_trotz_marker_abgelehnt() {
        let git_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(git_root.path().join(".git")).unwrap();
        mark(git_root.path());
        assert!(validate_sandbox_root(git_root.path().to_str().unwrap()).is_err());

        let parent = tempfile::tempdir().unwrap();
        let cloud_root = parent.path().join("Nextcloud-Test");
        std::fs::create_dir(&cloud_root).unwrap();
        mark(&cloud_root);
        assert!(validate_sandbox_root(cloud_root.to_str().unwrap()).is_err());
    }

    #[test]
    fn sync_vorschau_und_lauf_verlangen_sandbox_unterordner() {
        let sandbox = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(sandbox.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();

        let vfs = local_sync_commands::local_sync_vfs(Some(&root), &source, &target).unwrap();
        std::fs::rename(&target, root.join("altes-ziel")).unwrap();
        std::fs::create_dir(&target).unwrap();
        assert!(!doppeldecker_core::Vfs::anchor_is_current(&vfs).unwrap());
        assert!(local_sync_commands::local_sync_vfs(Some(&root), &root, &target).is_err());

        let outside = tempfile::tempdir().unwrap();
        let outside = std::fs::canonicalize(outside.path()).unwrap();
        let error = match local_sync_commands::local_sync_vfs(Some(&root), &outside, &target) {
            Ok(_) => panic!("Ordner außerhalb der Sandbox wurde akzeptiert"),
            Err(error) => error,
        };
        assert_eq!(error.code, "sandbox_violation");
    }

    #[test]
    fn system_app_oeffnet_nur_regulaere_dateien_ohne_shell() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("Datei mit Leerzeichen.txt");
        let folder = root.path().join("ordner");
        let link = root.path().join("link");
        std::fs::write(&file, b"inhalt").unwrap();
        std::fs::create_dir(&folder).unwrap();
        symlink(&file, &link).unwrap();

        let validated = browser_commands::validate_openable_file(&file).unwrap();
        let command = browser_commands::system_open_command(&validated).unwrap();
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![validated.as_os_str()]
        );
        assert!(browser_commands::validate_openable_file(&folder).is_err());
        assert!(browser_commands::validate_openable_file(&link).is_err());
        assert!(browser_commands::validate_openable_file(&root.path().join("fehlt")).is_err());
    }

    #[test]
    fn realmodus_ohne_backend_vorbereitung_kann_nicht_erfunden_werden() {
        let pending = Mutex::new(None);
        assert!(local_sync_commands::take_pending_real(&pending).is_err());
    }

    #[test]
    fn queue_dateikopie_laeuft_bis_completed() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let target = root.join("ziel");
        std::fs::write(&source, b"inhalt").unwrap();
        std::fs::create_dir(&target).unwrap();
        let queue = OperationQueue::open(temp.path().join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec![source.to_string_lossy().into_owned()],
                destination: target.to_string_lossy().into_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        doppeldecker_core::execute_local_queue_entry(
            &queue,
            claimed,
            &local_queue_execution(),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
        assert_eq!(std::fs::read(target.join("quelle.txt")).unwrap(), b"inhalt");
    }

    #[test]
    fn queue_spaete_ersetzungskollision_stoppt_vor_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let target = root.join("ziel");
        std::fs::write(&source, b"neu").unwrap();
        std::fs::create_dir(&target).unwrap();
        let queue = OperationQueue::open(temp.path().join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec![source.to_string_lossy().into_owned()],
                destination: target.to_string_lossy().into_owned(),
                overwrite: true,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        // Die Kollision entsteht absichtlich erst nach dem persistenten Claim.
        std::fs::write(target.join("quelle.txt"), b"fremd").unwrap();
        doppeldecker_core::execute_local_queue_entry(
            &queue,
            claimed,
            &local_queue_execution(),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            queue.snapshot().entries[0].state,
            QueueState::AwaitingConfirmation
        );
        assert_eq!(std::fs::read(target.join("quelle.txt")).unwrap(), b"fremd");
    }

    #[test]
    fn queue_abbruch_entfernt_keine_fremden_ziele() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let target = root.join("ziel");
        std::fs::write(&source, b"inhalt").unwrap();
        std::fs::create_dir(&target).unwrap();
        let queue = OperationQueue::open(temp.path().join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec![source.to_string_lossy().into_owned()],
                destination: target.to_string_lossy().into_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        let cancel = Cancel::new();
        cancel.cancel();
        doppeldecker_core::execute_local_queue_entry(
            &queue,
            claimed,
            &local_queue_execution(),
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Cancelled);
        assert!(!target.join("quelle.txt").exists());
        assert_eq!(std::fs::read(source).unwrap(), b"inhalt");
    }

    #[test]
    fn queue_sync_drift_stoppt_vor_ausfuehrung() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let options = SyncOptions::default();
        let vfs = LocalVfs::new();
        let expected = doppeldecker_core::plan(&vfs, &source, &vfs, &target, &options).unwrap();
        let queue = OperationQueue::open(temp.path().join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::Sync {
                source: source.to_string_lossy().into_owned(),
                target: target.to_string_lossy().into_owned(),
                sandbox: None,
                options: options.clone(),
                expected_plan: Box::new(expected.clone()),
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        std::fs::write(source.join("spaet.txt"), b"drift").unwrap();
        doppeldecker_core::execute_local_queue_entry(
            &queue,
            claimed,
            &local_queue_execution(),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            queue.snapshot().entries[0].state,
            QueueState::AwaitingConfirmation
        );
        assert!(std::fs::read_dir(target).unwrap().next().is_none());
    }
}
