# Release- und Plattformplan

Stand: 2026-11-07. Der im Repo dokumentierte erste öffentliche Previewstand ist
[0.1.124](https://github.com/DanielMuellerIR/Doppeldecker/releases/tag/v0.1.124).
Der nächste benannte Kandidat ist **0.1.125**; Veröffentlichung und Abnahme
werden für seine konkreten Pakete getrennt nachgewiesen. Künftige Änderungen
bleiben unter `Unreleased`.

## Unterstützte Ziele

| Plattform | Mindestziel | Pakete |
| --- | --- | --- |
| macOS | macOS 14+, Apple Silicon | Developer-ID-signierte, notarisierte und gestapelte App im ebenso geprüften DMG |
| Linux | Mint 22.2 / Ubuntu 24.04, x86_64 | DEB und AppImage mit WebKitGTK-4.1-/GTK-3-Abhängigkeiten |

Intel-Macs und weitere Distributionen werden erst nach echtem Plattformgegenlauf
als unterstützt bezeichnet. Cross-Builds allein reichen nicht.

## Release-Kandidat

Ein Kandidat benötigt für genau seinen Quell- und Artefaktstand:

1. konsistente Versionen, Changelog und reproduzierbaren Build aus sauberem Stand;
2. passende vollständige macOS-/Linux-Gates einschließlich Core, CLI, App,
   Frontend/CSP, Clippy, Formatierung und Fuzz-Lockfile;
3. echte GUI-/Providerabläufe ausschließlich mit künstlichen Daten in der
   markierten Sandbox, einschließlich mehrerer Fenster, Konflikten und Abbruch;
4. Installation, Start, Upgrade von der vorherigen Version und Deinstallation
   mit erhaltener Nutzerkonfiguration;
5. Quarantäne-/Papierkorb-/Queue-Recovery und beschädigte Persistenz ohne
   Datenverlust oder mutierende Blindwiederholung;
6. geprüfte Paketversion, Architektur, Abhängigkeiten, Signaturen und Prüfsummen;
7. keine Secrets, realen Build-/Nutzerpfade oder Testserverdaten in Artefakten;
8. übereinstimmende Funktionsmatrix, beide READMEs, Release Notes und Website.

Historische erfolgreiche Gegenläufe ersetzen keinen aktuellen Kandidatenlauf.
Versionswechsel nur für ausdrücklich benannte Meilensteine/Kandidaten; keine
Versionsbumps für gewöhnliche Entwicklungs- oder Dokumentationscommits.
Veröffentlichung, Tags und öffentliche Pakete brauchen einen konkreten Auftrag.

## Abnahmestand

Geprüfte Basis vor den Linux-Ergänzungen: `360e3f23`, weiterhin Version `0.1.124` unter
`Unreleased`. Core, CLI, App, Formatierung, Workspace-Clippy und Fuzz-Build sind
auf macOS und Linux geprüft; Frontend und Werkzeuge auf macOS.
[Reviewkorrekturen](code-review-2026-11-02.md).

Die nachfolgenden Linux-Ergänzungen bringen Polkit für lokale Metadaten und eine
benutzereigene AppImage-Installation mit stabilem Updatepfad. Ihr gemeinsamer
Mac-/Linux-Gegenlauf und die native AppImage-Prüfung stehen im
[Linux-Prüfstand](linux.md#prüfstand). Diese Entwicklungsprüfung ersetzt die
nachstehende Abnahme eines benannten öffentlichen Kandidaten nicht.

Zusätzlich bestehen SFTP-Archiv- und Rechteabläufe auf beiden Plattformen sowie
ZIP-Erzeugung und Entpacken über verschlüsseltes Samba auf Linux gegen frische
künstliche Serverroots. Die native Kandidatenabnahme steht in der Tabelle unten.

Zuletzt nativ und als Paket abgenommener Laufzeitstand: `f5425300`. Die folgenden
Paketbelege gelten für diesen Stand.

macOS: lokale Metadaten einschließlich Administratorfreigabe sowie direkte
SFTP-Rechteänderung, Datei-/Ordnerzyklen sowie SFTP-/WebDAV-Archivuploads sind
nativ geprüft. Die aktuelle notarisierte App ist atomar installiert; das vorhandene
Queue-Journal bleibt unverändert. Cmd+Q und Menü erlauben auch bei unlesbaren
Journalen ein bestätigtes Beenden. Das aktuelle private DMG erhält Signatur und
Notarisierungsticket; alle drei Programme bleiben nach Extraktion bytegleich zur
geprüften App.

Linux: Beide Pakete sind auf Version, Architektur, Bibliotheken und mitgelieferte
CLI geprüft. Die entpackte DEB-App und das regulär gestartete AppImage zeigen die
vollständige Oberfläche. Das AppImage enthält die benötigte GStreamer-Runtime
und portable Starthooks; der Pfadscan erfasst die gesamten Paketbäume.

Für den nächsten benannten Kandidaten bleiben diese Abnahmen offen:

| Prüfung | macOS | Linux |
| --- | --- | --- |
| Weitere native Anbieterabläufe | FTP-/FTPS-/SMB-Archive und rekursive entfernte Rechteänderungen; Fehler, Konflikt und Abbruch | Anbieterabläufe mit der fertigen Kandidaten-App wiederholen |
| Installation und Deinstallation | Frische Installation aus dem DMG, Start, erhaltene Konfiguration nach Entfernung | DEB installieren/entfernen, AppImage samt Integration starten/entfernen; Konfiguration erhalten |
| Upgrade aus der öffentlichen Preview | Kandidatenpaket über 0.1.124, Profile und Journale erhalten | DEB und AppImage getrennt von 0.1.124 aktualisieren; Profile und Journale erhalten |
| Recovery mit Kandidatenartefakten | Queue-, Papierkorb- und Quarantäneabbrüche sowie beschädigte Persistenz | Dieselben Recoveryfälle mit beiden Paketarten |

Die [Entwicklungshinweise](release-notes-next.md), Funktionsmatrix und READMEs
beschreiben den Quellstand. Kandidatenversion, Downloadlinks und Website werden
nach der Artefaktabnahme gemeinsam darauf bezogen.

## macOS-Paketvertrag

`./build-app` erzeugt die notarisierte App, `./install` installiert sie zusätzlich
atomar nach `/Applications`; `./release` erzeugt das DMG und installiert nicht.
Signatur, Notary-Ticket und Gatekeeper müssen vor dem Schreiben nach `/Applications`
bestanden sein. Test-Builds bleiben außerhalb dieses Ordners.

Die App im fertigen DMG muss ihr Ticket behalten und bytegleich zum geprüften
Sandbox-Bundle sein. Das Updaterarchiv entsteht erst aus der fertig notarisierten
und gestapelten App. Releasezugänge kommen aus Schlüsselbund/Umgebung, nie aus
Argumenten oder eingecheckten Dateien. Technische Prüfungen und Buildverträge
stehen in den vorhandenen `tools/`-Skripten und [Projektregeln](../AGENTS.md).

## Installation, Updates und Persistenz

Der [manuelle Updatekanal](update-design.md) fragt nur auf Menüklick und installiert
nach getrennter Bestätigung. App-, AppImage- und DEB-Nutzlasten bleiben getrennt;
jede Signatur gilt genau ihrem ausgelieferten Artefakt. Keine stillen Hintergrundupdates.

Konfiguration und Journale verwenden `com.doppeldecker.desktop`. Parser migrieren
nur bekannte Formate; unbekannte/beschädigte Daten bleiben erhalten und erzeugen
einen sichtbaren Fehler. Sandboxtests isolieren alle App-Daten in ihrem Testroot.

## Diagnose und Datenschutz

Keine Telemetrie oder automatischen Crash-Uploads. Das begrenzte `errors.jsonl`
enthält nur Zeit, kurzen UI-Kontext und Fehlercode, keine Pfade, Nutzer,
Profilkennungen, Fehlertexte oder Providerantworten. Rotation bei 1 MiB in eine
Vorgängerdatei. Ein künftig teilbares Diagnosepaket benötigt Vorschau und Maskierung.

## Definition „Release fertig“

Der aktuelle Kandidat ist auf beiden unterstützten Plattformen installierbar,
Upgrade und Recovery sind belegt, Paketprüfungen bestehen und die öffentliche
Beschreibung entspricht seinen tatsächlichen Funktionen und Grenzen.

[Historische Paketbelege](release-history-2026-10.md) bleiben zur Nachvollziehbarkeit
erhalten; die Produktziele stehen ausschließlich in der [Roadmap](roadmap.md).
