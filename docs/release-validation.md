# Prüfumfang der ersten Preview

Stand: 2026-10-19 · Version 0.1.124.

## Automatisierte Gates

`python3 tools/check-gates.py --json` führt Formatprüfung, gesperrte
Node-Installation, Werkzeugtests, Rust-Core-, CLI- und App-Tests, Clippy,
Fuzz-Übersetzung mit gesperrtem Lockfile sowie die Browser-Suite aus.
Die GitHub-CI verwendet macOS 15 und Ubuntu 24.04. Einzelne native Sondertests
bleiben ausdrücklich ignoriert; ein grünes Headless-Gate ersetzt keinen GUI-Test.
Für reproduzierbare Gates sind die beim Paketbau geprüften Rust-Versionen
festgelegt: 1.95.0 auf macOS und 1.96.1 auf Linux.

Die Browser-Suite arbeitet mit einer kontrollierten Tauri-Brücke und künstlichen
Datei-/Providerantworten. Sie prüft Oberfläche, IPC-Verträge, Fehler und
Antwortreihenfolgen. Sie belegt keine echte Serverfähigkeit.

## Native und paketbezogene Prüfungen

Künstliche Datei-, Queue-, Provider- und Recoveryläufe verwenden isolierte
Konfigurationsordner und markierte Quell-/Zielbäume. Native Gegenläufe behandeln
F2/F5/F6, mehrere Fenster, getrennte Journale, Start und Beenden. Die menschliche
Linux-Abnahme vom 2026-10-19 bestätigt Bedienung und das sichtbare Dockicon des
unmittelbar vorausgehenden Kandidaten 0.1.123. Die finalen Artefakte 0.1.124 wurden zusätzlich nativ gestartet: macOS aus dem
fertigen DMG sowie Linux aus DEB und AppImage. Kopieren mit unveränderter Quelle
und bytegleichem Ziel, zwei Fenster, das Linux-Fenstericon und die rechtlichen
App-Links waren dabei erfolgreich; alle Test-Apps beendeten sich regulär.

Der macOS-Prüfer kontrolliert Developer ID, Notarisierung, Staple, Gatekeeper,
CLI-Sidecar und den bytegleichen App-Baum im fertigen DMG. Der Linux-Prüfer
extrahiert beide Paketformate und prüft Version, Architektur, Abhängigkeiten,
Desktop-Datei, Icons und eingebettete Binaries. Updater-Signaturen werden gegen
genau die ausgelieferte Nutzlast und den eingebauten öffentlichen Schlüssel geprüft.

Ein frischer Ubuntu-24.04-Container prüfte die tatsächliche Installation des
aufbewahrten privaten Kandidaten 0.1.123, das Upgrade mit dem finalen DEB 0.1.124
und die anschließende Paketentfernung. Beide installierten Binärdateien waren
bytegleich zum DEB-Inhalt; künstliche Benutzereinstellungen blieben unverändert.
Der Testcontainer wurde danach entfernt. Die erste öffentliche Preview besitzt noch keinen vorherigen
öffentlichen Release, von dem ein echtes öffentliches Upgrade belegt werden könnte.

## Bekannte Grenzen

- macOS 14 oder neuer auf Apple Silicon und Linux Mint 22.2/Ubuntu 24.04 auf
  x86_64 sind die vorgesehenen Plattformen. Intel-Macs und andere Distributionen
  besitzen noch keine Freigabe.
- Die [Funktionsmatrix](function-audit.md) begrenzt die Provideroperationen.
  SFTP-Mutationen und gewöhnliches WebDAV-Ersetzen bleiben gesperrt.
- Ein grüner Lauf beweist keine Fehlerfreiheit. Die Preview sollte zunächst mit
  Zweitkopien erprobt werden; sensible Dateiarbeit benötigt weiterhin ein Backup.
- Unklare Teilwirkungen, Recoverypfade und alte Journale bleiben sichtbar erhalten.
  Zeitablauf oder ein terminaler Queuezustand erteilen keine Löschfreigabe.
