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

## Abnahmestand des Kandidaten 0.1.125

Die folgenden Paketbelege betreffen den ersten vorbereiteten Kandidaten.
Die native WebDAV-Abnahme hat zusätzlich eine fehlerhafte Ablehnung von
Collections ohne ETag gefunden. Nach dieser Parserkorrektur werden die Pakete
neu gebaut und nativ erneut geprüft. Die Veröffentlichung folgt erst nach
der vollständigen Kandidatenabnahme.

Die vollständige [GitHub-CI](https://github.com/DanielMuellerIR/Doppeldecker/actions/runs/42386074224)
ist für den Quellstand `4ec77e3f` auf macOS 15 und Ubuntu 24.04 erfolgreich.
Die signierten Pakete enthalten den Laufzeitstand `b911ad6a`; die danach
geänderten Dateien betreffen ausschließlich Tests. Version, Architektur,
Abhängigkeiten, Signaturen, Prüfsummen und die Abwesenheit privater Buildpfade
sind an den fertigen Paketen geprüft.

Die macOS-App und das DMG sind Developer-ID-signiert, notarisiert und gestapelt.
Die App im DMG ist bytegleich zur geprüften App; das Updaterarchiv wurde erst nach
dem Stapeln erzeugt. Der native Lauf mit dem fertigen Bundle belegt Kopieren
(einschließlich einer Resource Fork über 6 MiB), Umbenennen, Ordneranlage und zwei
getrennte Fensterjournale. Die Bildschirmaufnahme war auf dem Testkonto nicht
freigegeben; Dateiwirkungen und native Fenster wurden unabhängig geprüft.

Auf Linux starten das fertige AppImage und die entpackte DEB-App mit vollständiger
Oberfläche. Kopieren, Umbenennen, Ordneranlage und getrennte Fensterjournale sind
nativ geprüft. Der AppImage-Start benötigt keine testseitigen Grafik-Workarounds.
Die zugehörigen Ansichten einschließlich des Fehlers bei beschädigtem Journal
sind visuell geprüft.

Das Upgrade von der öffentlichen 0.1.124 auf die Kandidaten-App beziehungsweise
das benutzereigene AppImage erhält ein gespeichertes SFTP-Profil und die
Konfiguration. Nach Entfernen der isolierten Installation bleiben Konfiguration
und Testdateien erhalten. Ein beschädigtes Queue-Journal erlaubt ein bestätigtes
Beenden und bleibt mit den Testdateien unverändert. Die DEB-Prüfung belegt echte
Installation, Entfernung und Upgrade von 0.1.124 mit den deklarierten
Laufzeitabhängigkeiten in einer isolierten Ubuntu-24.04-Umgebung.

Zusätzlich bestehen echte SFTP-Gegenläufe auf macOS und Linux sowie die
verschlüsselten Samba-Gegenläufe für Queue/Quarantäne und ZIP-Übertragung auf
Linux. Diese automatisierten Providerprüfungen ersetzen die noch offenen
nativen Kombinationen nicht.

| Noch offene Prüfung | macOS | Linux |
| --- | --- | --- |
| Weitere native Anbieterabläufe | FTP-/FTPS-/SMB-Archive und rekursive entfernte Rechteänderungen; Fehler, Konflikt und Abbruch | Anbieterabläufe mit der fertigen Kandidaten-App wiederholen |
| Recovery mit Kandidatenartefakten | Unterbrochene Papierkorb-/Quarantäneabläufe und weitere beschädigte Persistenz | Dieselben Recoveryfälle mit beiden Paketarten |
| Update über den veröffentlichten Kanal | Installation über das signierte GitHub-Updateangebot | Signierter Versionswechsel für AppImage und DEB |

Der Kandidat ist vorbereitet, aber noch nicht veröffentlicht. Die offene native
Matrix bleibt Teil der weiteren Abnahme und darf nicht als vollständig geprüft
bezeichnet werden.
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
