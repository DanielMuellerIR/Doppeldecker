# Release- und Plattformplan

Stand: 2026-11-08. Die aktuelle öffentliche Preview ist
[0.1.126](https://github.com/DanielMuellerIR/Doppeldecker/releases/tag/v0.1.126).
Veröffentlichung und Abnahme sind für ihre konkreten Pakete nachgewiesen.
Künftige Änderungen bleiben unter `Unreleased`.

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

## Abnahmestand von 0.1.126

0.1.126 korrigiert gespeicherte Fenstergrößen, Linux-mDNS-Erkennung und die
Bereinigung entfernter wartender Archivuploads. Die vollständige
[GitHub-CI](https://github.com/DanielMuellerIR/Doppeldecker/actions/runs/42483101211)
besteht für den Releasequellstand `1eebba25` auf macOS 15 und Ubuntu 24.04.
Die lokalen Plattformgates sind ebenfalls abgeschlossen. Ein einzelner Linux-
Browser-Timeout bestand in der gezielten Wiederholung; die restlichen Teilsuiten
bestanden anschließend ohne Codeänderung.

Die endgültige macOS-App und das DMG sind Developer-ID-signiert, notarisiert und
gestapelt; das Updaterarchiv entstand danach aus dieser App. Paketversion,
Architektur, Signaturen, Ticket, Gatekeeper, Pfadprüfung und die bytegleiche App
im DMG sind geprüft. Die native Abnahme bindet genau die ausgelieferte Nutzlast.

| Native Prüfung | macOS-App | Linux-AppImage und DEB |
| --- | --- | --- |
| Fenstergröße und Upgrade von 0.1.125 | Größe ohne Sitzungswiederherstellung, Profil und Konfiguration erhalten | Echter Größenwechsel mit Neustart sowie erhaltene Profile und Konfiguration bestanden |
| Dateiaktionen und mehrere Fenster | Kopieren einschließlich großer Resource Fork, Umbenennen, Ordneranlage und getrennte Journale bestanden | Kopieren, Umbenennen, Ordneranlage und getrennte Journale bestanden |
| Entfernte ZIP-Dateiarbeit | SFTP, FTP, FTPS, WebDAV und verschlüsseltes SMB 3.1.1; Inhalt, leere Ordner und unveränderter Profilroot geprüft | Dieselben fünf Anbieter mit beiden Paketformen bestanden |
| Rekursive entfernte Rechte | SFTP, FTP und FTPS einschließlich Sonderbits, Abbruch und Teilfehler bestanden | Dieselben Anbieter und Fehlerpfade mit beiden Paketformen bestanden |
| Abbruch, Konflikte und Wiederherstellung | Vorschauabbruch, unveränderte Konfliktdateien, kein mutierender Blind-Retry sowie Papierkorb-, Quarantäne- und Queue-Recovery bestanden | Dieselben Zustände mit beiden Paketformen bestanden |
| Installation, Upgrade und Entfernung | Isolierter Austausch und Entfernung mit unveränderter Testkonfiguration bestanden | Benutzereigenes AppImage sowie DEB mit tatsächlichen Abhängigkeiten in Ubuntu 24.04 bestanden |
| Darstellung | Native Fenster- und Bedienungsbelege; keine Bildschirmaufnahme als Abnahmenachweis | Hell, Dunkel und Automatisch einschließlich dunkler Menüleiste und geöffneter Menüs visuell geprüft |

Alle Wirkungen verwendeten künstliche Daten in markierten Testbereichen. Beide
Linux-Pakete und die hashgebundene AppImage-Attestation sind signiert; das
Update-Manifest prüft die drei getrennten Nutzlastkanäle. Die flache Prüfsummenliste
erfasst alle veröffentlichten Nutzlasten und Metadaten. Die Paketabläufe benötigen
keine zusätzlichen Grafik-Workarounds. Die folgenden Belege zu 0.1.125 bleiben
getrennt und gelten ausschließlich dem früheren Release.

## Abnahmestand von 0.1.125

Die vollständige [GitHub-CI](https://github.com/DanielMuellerIR/Doppeldecker/actions/runs/42431365412)
ist für den Releasequellstand `a11cfc11` auf macOS 15 und Ubuntu 24.04 erfolgreich.
Die native Abnahme wurde nach den Korrekturen für WebDAV-Collections ohne ETag
mit der fertigen macOS-App sowie den endgültigen AppImage- und DEB-Nutzlasten
wiederholt. Die macOS-App und das DMG sind Developer-ID-signiert, notarisiert
und gestapelt. Das inhaltlich geprüfte Updaterarchiv entstand danach aus genau
dieser App.

| Native Prüfung | macOS-App | Linux-AppImage und DEB |
| --- | --- | --- |
| ZIP erstellen und in die Gegenpane entpacken | SFTP, FTP, FTPS, WebDAV und verschlüsseltes SMB3 bestanden; Inhalte, leere Ordner und unveränderter Profilroot unabhängig geprüft | Dieselben fünf Anbieter mit beiden Paketarten bestanden |
| Rekursive entfernte Unix-Rechte | SFTP, FTP und FTPS einschließlich Sonderbits bestanden | Dieselben Anbieter mit beiden Paketarten bestanden |
| Abbruch, Konflikt und Serverfehler | Abgebrochene Vorschau ohne Upload, vorhandenes Archiv erhalten, bewusstes Verwerfen, abgewiesener Upload mit erhaltenem Zwischenstand und ohne Blind-Retry nach Neustart; Rechte-Abbruch und Teilfehler bestanden | Dieselben Zustände mit beiden Paketarten bestanden |
| Papierkorb, Quarantäne und Queue-Recovery | Echte Prozessabbrüche, jede alte Version genau einmal erhalten, keine blinde Wiederholung, Restore-Konflikt und ausdrücklicher Restore sowie beschädigte Journale und Manifeste bestanden | Dieselben Recoveryfälle mit beiden Paketarten bestanden |
| Installation, Upgrade und Entfernung | Start der öffentlichen 0.1.124, Upgrade mit unverändertem Profil und Konfiguration, Entfernung der isolierten App mit erhaltenen Testdaten bestanden | Benutzereigenes AppImage-Upgrade und Entfernung sowie DEB-Installation, Upgrade und Entfernung in einer isolierten Ubuntu-24.04-Umgebung bestanden |

Kopieren, Umbenennen, Ordneranlage und getrennte Fensterjournale sind an den
fertigen Kandidaten nativ geprüft. Auf macOS blieb auch eine Resource Fork über
6 MiB erhalten. Die macOS-Funktionsabnahme verwendet native Fenster-/Bedienungs-
und Dateibelege; eine Bildschirmaufnahme wird dafür nicht als Nachweis gezählt.
Auf Linux sind Oberfläche, Informationsdialoge und Fehlerzustände visuell geprüft.
Hell, Dunkel und Automatisch sowie die dunkle native Menüleiste und aufklappende
Menüs sind mit beiden Paketarten geprüft. Der AppImage-Start benötigt keine
zusätzlichen Grafik-Workarounds.

Alle Wirkungen liefen ausschließlich mit künstlichen Daten in markierten
Testbereichen. Die genannten Anbieter- und Recoveryfälle sind abgeschlossen.
Die Updater-Signaturen für das endgültige macOS-Archiv, AppImage und DEB sowie
die hashgebundene Linux-Attestation sind geprüft. Die abschließenden macOS-/Linux-
Paketprüfungen bestehen; das Manifest enthält die drei getrennten Updatekanäle,
und die Prüfsummen erfassen alle veröffentlichten Dateien. Der echte Versionswechsel von 0.1.124 auf 0.1.125 über das veröffentlichte
GitHub-Angebot ist für das benutzereigene AppImage nativ bestanden: vor der
Bestätigung unveränderte Installation, geprüfte Signatur, exakt die veröffentlichte
Nutzlast, erfolgreicher Neustart und bytegleich erhaltene Profile und Konfiguration.

Der Release enthält elf Dateien einschließlich der getrennten Nutzlastsignaturen,
Linux-Attestation, Update-Manifest und flacher Prüfsummenliste. Alle hochgeladenen
Dateien wurden zurückgeladen und bytegleich bestätigt. Der öffentliche Latest-Kanal
liefert das geprüfte Manifest. Beide READMEs und die Projektseite beziehen sich auf
0.1.125; die Website ist nach dem Upload per HTTPS bytegleich geprüft.
Frühere Paketbelege werden getrennt aufbewahrt und ersetzen keine der hier
genannten Wiederholungen.
[Historische Paketbelege](release-history-2026-10.md) bleiben getrennt erhalten.

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
