# Release- und Plattformplan

Stand: 2026-11-21. Die aktuelle Version ist
[1.0.2](https://github.com/DanielMuellerIR/Doppeldecker/releases/tag/v1.0.2).
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

## Wiederanlaufprüfung für 1.0.1

Der erste stabile Stand enthält die Korrektur für fortsetzbare Kopien.
Der separate Byte-Beleg wird nach der Nutzdatei synchronisiert. In der
Regression mit 128 früheren Aufträgen blieb das Fensterjournal während des
Transfers bytegleich. Nach einem Prozessabbruch lud die native App den
Beleg bei 8 MiB und stellte die 192 MiB große Testdatei bytegleich fertig.
Eine pausierte Kopie aus Journalversion 45 bleibt fortsetzbar.

## Prüfstand des unveröffentlichten 1.0.0-Entwurfs

Die 1.0-Pakete verwenden dieselben Programmquellen wie der Release-Tag.
DMG, DEB und AppImage übernehmen beim nativen Wechsel von 0.1.127 die
gespeicherten Tabs oder Pane-Ziele. Die Apple-Silicon-App und das DMG tragen
Developer-ID-Signaturen und Apple-Tickets. Das AppImage startet über FUSE.
Die Datenschutzprüfung der Pakete sowie die Produktionssignaturen der
drei Updater-Nutzlasten und der hashgebundenen AppImage-Attestation bestehen.

- Unter macOS bleiben beide gespeicherten Tabs samt aktivem Tab erhalten.
  Spiegeln und anschließendes Wiederherstellen aus der Quarantäne erhalten
  die geprüften Dateiinhalte. Ein beschädigtes Queue-Journal bleibt beim
  bestätigten Beenden unverändert.
- Unter Linux prüft der gespeicherte Spiegelauftrag nach Programmneustart
  den veränderten SFTP-Bestand. Die tatsächlichen Zielinhalte entsprechen
  der bestätigten Vorschau, ausgeschlossene Dateien bleiben erhalten.
  Nach einem getrennten Prozessabbruch lädt die App den gesicherten
  ZIP-Zwischenstand bytegleich hoch und räumt den Prüf-Tab auf.
- Ein isolierter macOS-Updater von 0.1.127 installiert das korrigierte,
  mit dem Produktionsschlüssel signierte Archiv. Die installierte App
  startet als 1.0.0. GUI und CLI entsprechen bytegleich dem Paket.

## Privater Prüfstand vom 2026-11-18

Die privaten Pakete verwenden den Programmstand `63ebe1b4` mit der
bestehenden Versionsnummer 0.1.127. Der Paketwechsel von der veröffentlichten
0.1.126 übernimmt im DMG beide gespeicherten Tabs und deren aktive Auswahl.
DEB und AppImage übernehmen den aktiven Tab und seinen Ordner. Das AppImage
startet direkt über FUSE. Auf beiden Plattformen bestehen alle neun Gates
einschließlich der Browsertests. Paketstruktur, Architektur, Abhängigkeiten,
mitgelieferte CLI-Versionen und Datenschutzprüfung sind geprüft.

- Developer-ID-App und DMG sind notarisiert und gestapelt. Gatekeeper und
  der Vergleich von GUI und CLI aus dem DMG bestehen. Die App stellt eine
  beim Spiegeln verschobene Datei über die Wiederherstellungsansicht
  bytegleich wieder her. Der verschachtelte Teilkopieordner bleibt erhalten.
- Im DEB erreicht die Wiederherstellungsansicht nach erzwungenem Neustart
  den wartenden SFTP-Archivupload. Das am Ziel nach bestätigter Kollision
  installierte ZIP ist bytegleich mit dem gesicherten Zwischenstand, auch
  nachdem die Originaldatei geändert wurde. Der Prüf-Tab schließt nach
  der Bereinigung. Ein separat beschädigtes Queue-Journal bleibt
  unverändert. Kleine SFTP-, FTPS- und WebDAV-Downloads über die
  fortsetzbare Kopie sind bytegleich geprüft.
- Im AppImage plant der gespeicherte Spiegelauftrag nach einem Neustart
  erneut. Der zwischenzeitlich hinzugekommene Zieleintrag erscheint in
  der Vorschau und wird nach Bestätigung in die Quarantäne verschoben.
  Die Quelldatei wird kopiert und die ausgeschlossene Zieldatei bleibt
  bytegleich erhalten. Die Entfernung der isolierten Linux-Installationen
  erhält Konfiguration und Testdateien. In der TAR.GZ-Ansicht funktionieren
  F3 und das Entpacken des geöffneten Ordners. Die extrahierte Datei ist
  bytegleich, der Archivlink wird sichtbar ausgelassen.

Die vorherigen Paketläufe an `ad89d468` belegen zusätzlich gewöhnliche
FTP-/FTPS-Kopien mit UTF-8-Namen, WebDAV-Kopien in beide Richtungen,
SMB-Kollision mit „Beide behalten“, Testpapierkorb und getrennte
Fensterjournale. Die nativen Wiederanläufe großer lokaler, WebDAV-, FTPS-
und SFTP-Kopien wurden in den jeweiligen Entwicklungsständen geprüft.

Der öffentliche Linux-AppImage-Updater installiert die signierte 0.1.127
aus 0.1.126. Im öffentlichen macOS-Archiv dieser Version scheitert der
Updater an einem AppleDouble-Eintrag. Ein isolierter nativer Tauri-Updater
mit lokalem Prüfschlüssel installiert das korrigierte Archiv aus
`63ebe1b4`. Die darin enthaltene App startet als 0.1.127 und besteht
Bytevergleich, Codesign-, Ticket- und Gatekeeper-Prüfung.

Offen sind die separaten Produktionssignaturen der neuen Updater-Nutzlasten,
die signierte AppImage-Attestation und die abschließende Release-Prüfung mit
diesen Signaturen. Der damals öffentliche Stand blieb 0.1.127.

## Abnahmestand von 0.1.127

0.1.127 korrigiert die Wiederherstellung unterbrochener Uploadbereinigungen und
hält Schließen sowie Beenden während des Cleanupworkers auf. Die vollständigen
lokalen Plattformgates einschließlich Core, CLI, App, Frontend/CSP, Clippy,
Formatierung und Fuzz-Lockfile bestehen auf macOS und Linux. Die
[GitHub-CI](https://github.com/DanielMuellerIR/Doppeldecker/actions/runs/42806185251)
prüft den gleichen Programmstand `4be7368b` auf macOS 15 und Ubuntu 24.04.
Die Änderungen nach diesem Quellstand betreffen ausschließlich diese
Release-Dokumentation; die geprüften Programmquellen bleiben unverändert.

Die Prozessabbruch-, Queue-Neustart-, Cleanupfehler- und Exit-Barriere-Regressionen
bestehen. Die macOS-App und das DMG sind Developer-ID-signiert, notarisiert und
gestapelt; das anschließend erzeugte Updaterarchiv enthält dieselbe geprüfte App.
Signatur, Ticket, Gatekeeper, Architektur, Version, Sidecars, Pfadneutralität und
die bytegleiche App im fertigen DMG sind geprüft. Die Linux-Paketprüfer bestätigen
DEB und AppImage einschließlich der tatsächlich enthaltenen CLI-Versionen,
Laufzeitbibliotheken, detached Signaturen und hashgebundener AppImage-Attestation.
Das Update-Manifest bindet die drei getrennten Nutzlastkanäle; die flache
Prüfsummenliste erfasst alle zehn Nutzlast-, Signatur- und Metadatendateien.

Die native Baseline verwendet auf beiden Plattformen die neue Nutzlast und ein
isoliertes Upgrade von der öffentlichen 0.1.126. Profil und Konfiguration bleiben
erhalten; Kopieren, Umbenennen, Ordneranlage, getrennte Fensterjournale,
beschädigte Queue-Daten und die Entfernung der isolierten Installation mit
Konfigurationserhalt bestehen. Auf macOS bleibt die große Resource Fork erhalten;
auf Linux sind der Größenwechsel mit Neustart und der native DEB-Start geprüft.
Die Linux-Oberfläche ist in einer privaten Xvfb-Sitzung visuell geprüft. Alle
Dateiwirkungen verwenden künstliche Daten in markierten Testbereichen.

Die vollständige fünf-Provider-Matrix sowie alle drei Erscheinungsmodi aus
0.1.126 wurden für diese reine Cleanup-/Exit-Korrektur nicht erneut nativ
abgenommen; der Providercode und die Gestaltung sind unverändert. Die früheren
Belege bleiben ausdrücklich dem früheren Release zugeordnet.

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
keine zusätzlichen Grafik-Workarounds. Die elf GitHub-Dateien wurden vor der
Freigabe zurückgeladen und bytegleich geprüft. Der öffentliche Latest-Kanal
liefert das geprüfte Manifest. Der echte AppImage-Updateweg von 0.1.125 auf
0.1.126 bestand mit ausdrücklicher Installationsbestätigung, exakt der
veröffentlichten Nutzlast, nativem Neustart und erhaltenem Profil; das
Einstellungsformat wurde dabei erwartungsgemäß auf Version 3 migriert. Beide
READMEs und die Projektseite verlinken 0.1.126; die drei geänderten Website-Seiten
sind live per HTTPS bytegleich bestätigt. Die folgenden Belege zu 0.1.125 bleiben
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
[Historische Paketbelege](history/release-history-2026-10.md) bleiben getrennt erhalten.

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

[Historische Paketbelege](history/release-history-2026-10.md) bleiben zur Nachvollziehbarkeit
erhalten; die Produktziele stehen ausschließlich in der [Roadmap](roadmap.md).
