//! Sichtbarkeit und Trennen lebender Remote-Sitzungen (Eject in der Seitenleiste).
//!
//! Die Session-Caches in `AppState` sind nach Profil-ID gekeyt und leben sonst bis
//! zum Prozessende — auch wenn keine Pane das Profil mehr anzeigt. Diese Commands
//! machen den Zustand für die Seitenleiste sichtbar und erlauben ein explizites
//! Trennen. Queue- und Realmodus-Läufe bauen eigene, frische Sitzungen auf und
//! bleiben von einem Trennen des Anzeige-Caches unberührt.

use tauri::{Emitter, State};

use crate::queue_runtime::AppState;
use crate::{AppError, MutexExt};

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConnectionChangeKind {
    Connected,
    Saved,
    Removed,
    Disconnected,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectionUpdate {
    protocol: doppeldecker_core::RemoteProfileProtocol,
    profile_id: String,
    revision: u64,
    change: ConnectionChangeKind,
}

/// Der einzige bewusste Rundruf der App-Schicht.
///
/// Profile und Verbindungen sind laut Produktvertrag prozessweit sichtbar: Speichert
/// ein Fenster ein Profil oder trennt eine Verbindung, müssen **alle** Fenster ihre
/// Profilzeilen, Favoriten und Panes an der neuen Revision neu bewerten. Jeder
/// andere Versand der App-Schicht ist fensterbezogen und läuft deshalb über
/// `crate::emit_to_window`; der Guard-Test `fensterereignisse_gehen_nie_als_rundruf_hinaus`
/// hält das fest und nimmt genau diese Datei aus.
pub(crate) fn emit_connection_update(
    app: &tauri::AppHandle,
    binding: &doppeldecker_core::RemoteProfileBinding,
    change: ConnectionChangeKind,
) {
    let _ = app.emit(
        "connections-updated",
        ConnectionUpdate {
            protocol: binding.protocol,
            profile_id: binding.profile_id.clone(),
            revision: binding.revision,
            change,
        },
    );
}

/// Eine lebende (gecachte) Verbindung: Protokoll plus Profil-ID. Mehr braucht das
/// Frontend nicht — Name/Host kennt es aus den geheimnisfreien Profillisten.
#[derive(Clone, serde::Serialize)]
pub(crate) struct ActiveConnection {
    protocol: doppeldecker_core::RemoteProfileProtocol,
    profile_id: String,
    revision: u64,
}

fn collect_active(state: &AppState) -> Vec<ActiveConnection> {
    let mut result = Vec::new();
    let mut push = |connections: Vec<doppeldecker_core::RemoteProfileBinding>| {
        for binding in connections {
            result.push(ActiveConnection {
                protocol: binding.protocol,
                profile_id: binding.profile_id,
                revision: binding.revision,
            });
        }
    };
    push(
        state
            .sftp_connections
            .lock_recover()
            .values()
            .map(|cached| cached.binding.clone())
            .collect(),
    );
    push(
        state
            .ftps_connections
            .lock_recover()
            .values()
            .map(|cached| cached.binding.clone())
            .collect(),
    );
    push(
        state
            .webdav_connections
            .lock_recover()
            .values()
            .map(|cached| cached.binding.clone())
            .collect(),
    );
    push(
        state
            .smb_connections
            .lock_recover()
            .values()
            .map(|cached| cached.binding.clone())
            .collect(),
    );
    result
}

#[tauri::command]
pub(crate) fn list_active_connections(state: State<AppState>) -> Vec<ActiveConnection> {
    collect_active(&state)
}

/// Entfernt die gecachte Sitzung eines Profils. Mit dem letzten `Arc`-Halter wird
/// die Verbindung wirklich geschlossen; ein gerade laufendes Listing beendet sich
/// noch auf der alten Sitzung und bleibt konsistent. Unbekannte IDs sind ein No-op.
#[tauri::command]
pub(crate) fn disconnect_remote_profile(
    app: tauri::AppHandle,
    state: State<AppState>,
    protocol: String,
    profile_id: String,
    profile_revision: Option<u64>,
) -> Result<Vec<ActiveConnection>, AppError> {
    fn remove_current<T>(
        pool: &std::sync::Mutex<
            std::collections::HashMap<String, crate::queue_runtime::CachedRemoteConnection<T>>,
        >,
        profile_id: &str,
        profile_revision: Option<u64>,
    ) -> Result<Option<doppeldecker_core::RemoteProfileBinding>, AppError> {
        let mut pool = pool.lock_recover();
        if let Some(cached) = pool.get(profile_id) {
            crate::queue_runtime::ensure_requested_profile_revision(
                profile_revision,
                &cached.binding,
            )?;
        }
        Ok(pool.remove(profile_id).map(|cached| cached.binding))
    }

    let removed = match protocol.as_str() {
        "sftp" => remove_current(&state.sftp_connections, &profile_id, profile_revision)?,
        "ftps" => remove_current(&state.ftps_connections, &profile_id, profile_revision)?,
        "webdav" => remove_current(&state.webdav_connections, &profile_id, profile_revision)?,
        "smb" => remove_current(&state.smb_connections, &profile_id, profile_revision)?,
        other => {
            return Err(AppError::new(
                "app",
                format!("Unbekanntes Protokoll zum Trennen: {other}"),
            ))
        }
    };
    if let Some(removed) = removed {
        emit_connection_update(&app, &removed, ConnectionChangeKind::Disconnected);
    }
    Ok(collect_active(&state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbindungsereignis_traegt_protokoll_profil_revision_und_aenderung() {
        let update = ConnectionUpdate {
            protocol: doppeldecker_core::RemoteProfileProtocol::Webdav,
            profile_id: "profil-eins".into(),
            revision: 23,
            change: ConnectionChangeKind::Saved,
        };
        let json = serde_json::to_value(update).unwrap();
        assert_eq!(json["protocol"], "webdav");
        assert_eq!(json["profileId"], "profil-eins");
        assert_eq!(json["revision"], 23);
        assert_eq!(json["change"], "saved");
    }
}
