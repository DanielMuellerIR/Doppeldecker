//! Dünner IPC-Einstieg für die begrenzte DNS-SD-Suche im Core.

use std::time::Duration;

use doppeldecker_core::DiscoveredNetworkService;

use crate::AppError;

const DISCOVERY_WINDOW: Duration = Duration::from_millis(900);

#[tauri::command]
pub(crate) async fn discover_network_services() -> Result<Vec<DiscoveredNetworkService>, AppError> {
    // Socketpolling und DNS-Auswertung sind absichtlich synchron im GUI-freien Core.
    // Der IPC-Einstieg schiebt sie deshalb auf Tauris blockierenden Worker, damit die
    // Runtime während des 900-ms-Suchfensters und auch unter feindlichen LAN-Paketen
    // weiter Fensterereignisse und andere Commands bedienen kann.
    tauri::async_runtime::spawn_blocking(|| {
        doppeldecker_core::discover_network_services(DISCOVERY_WINDOW)
    })
    .await
    .map_err(|_| AppError::new("app", "Der begrenzte LAN-Suchworker wurde unterbrochen"))?
    .map_err(AppError::from)
}

#[cfg(test)]
#[test]
fn command_read_results_discovered_network_services() {
    use doppeldecker_core::NetworkServiceProtocol;

    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../tools/tests/command-read-results.json")).unwrap();
    let services = [
        (
            NetworkServiceProtocol::Smb,
            "Studio-NAS",
            "studio-nas.local",
            445,
        ),
        (
            NetworkServiceProtocol::Sftp,
            "Buildserver",
            "build.local",
            2222,
        ),
        (NetworkServiceProtocol::WebDav, "Cloud", "cloud.local", 8443),
    ]
    .map(|(protocol, name, host, port)| DiscoveredNetworkService {
        protocol,
        name: name.into(),
        host: host.into(),
        port,
    });
    assert_eq!(
        serde_json::to_value(services).unwrap(),
        fixture["networkServices"]
    );
}
