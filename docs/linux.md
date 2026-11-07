# Linux: Funktionen, Installation und Updates

Stand: 2026-11-07. Linux und macOS verwenden dieselbe Oberfläche und denselben
Datei-, Provider-, Sync- und Queuekern. Die [Funktionsmatrix](function-audit.md)
gilt für beide Plattformen, sofern eine Funktion nicht ausdrücklich macOS nennt.

## Plattformumfang

| Bereich | Linux |
| --- | --- |
| Dateiaktionen, Archive, Suche, Vorschau und Vergleich | Gemeinsame Implementierung; verfügbare Vorschauformate hängen auch von der System-WebView ab. |
| Tabs, mehrere Fenster, getrennte Queues und Recovery | Derselbe Vertrag wie auf macOS. |
| SFTP, FTP/FTPS, WebDAV und SMB | Gemeinsame Provider samt ihren ausgewiesenen Grenzen. |
| Lokale Rechte und Eigentümer/Gruppe | Normale Benutzerrechte; optional Systemfreigabe über Polkit. |
| Zwischenablage, Dateidrops, Öffnen und Laufwerke | Native GTK-/Desktopintegration statt Finder/AppKit. |
| Papierkorb und Metadaten | XDG-Papierkorb und Linux-Dateisystemfähigkeiten; Auslassungen bleiben sichtbar. |
| Finder-Tags, Finder-Kommentare, Finder-Erweiterungsanzeige und macOS-Dateiflags | macOS-spezifisch; kein entsprechender Linux-Editor. |

Polkit startet für jede ausdrücklich erhöhte Metadatenänderung den mitgelieferten
Helfer. Es gibt keinen dauerhaft eingerichteten Rootdienst und keine erhöhten
gewöhnlichen Dateiaktionen. Die Anmeldung entscheidet das System; Passwörter
gelangen weder in die Oberfläche noch in Journale. Ohne `pkexec` und einen
Authentifizierungsagenten bleiben normale Rechteänderungen verfügbar.
[Metadatenvertrag](file-properties.md).

## Pakete und benutzereigene Installation

DEB ist für Mint/Ubuntu gedacht. Auf anderen x86_64-Distributionen verwendet man
das AppImage; die Eignung muss auf dem konkreten System geprüft werden. Ein
AppImage ersetzt nicht sämtliche Systembibliotheken oder den Grafiktreiber.
Für den regulären Start werden FUSE 2 und eine funktionsfähige grafische Sitzung
benötigt. Der Wayland-Client (`libwayland-client.so.0`) kommt vom System, damit
er zum dortigen Mesa-/EGL-Grafikstack passt. Die Paketprüfung lehnt eine
mitgelieferte Kopie ab. Polkit ist für die optionale Administratorfreigabe erforderlich.

Eine ältere gebündelte Wayland-Bibliothek kann bei neueren Mesa-Treibern das
Symbol `wl_fixes_interface` nicht liefern. WebKit bricht dann mit
`EGL_BAD_PARAMETER` ab, während Menü und leeres Fenster erhalten bleiben.
Der Paketbau entfernt diese Bibliothek vor dem abschließenden Packen und Signieren.
[Upstream-Bericht](https://github.com/tauri-apps/tauri/issues/15665).

Nach der Prüfung von Herkunft, Signatur beziehungsweise Release-Prüfsumme:

```bash
./install.sh --appimage /pfad/Doppeldecker_0.1.125_amd64.AppImage --sha256 <SHA256>
```

Der Installer verwendet `~/.local/lib/doppeldecker/Doppeldecker.AppImage`, legt
einen Terminalstarter und einen Menüeintrag samt Icon an und benötigt kein sudo.
Der eigentliche AppImage-Pfad bleibt eine reguläre, benutzereigene Datei: Der
Updater kann sie ersetzen, während Starter und Menüeintrag weiterhin passen.
Konfiguration und Journale bleiben im bisherigen App-Datenverzeichnis.
Ein fremder gleichnamiger Starter wird nicht ersetzt. Fehlgeschlagene
Installationen nehmen ihre bereits eingesetzten Dateien zurück.

Die CLI liegt in beiden Paketformaten unter `usr/bin/doppeldecker-cli`; das DEB
installiert sie unmittelbar als Terminalbefehl. Beim AppImage kann sie aus einer
mit `--appimage-extract` entpackten Kopie aufgerufen werden.

## Darstellung

Die Wahl Hell, Dunkel oder Automatisch gilt auch für die native Menüleiste und
ihre aufklappenden Menüs. Automatisch folgt dem von WebKit erkannten Systemmodus;
ein Wechsel zurück von Hell oder Dunkel übernimmt wieder diesen Systemmodus.

## GitHub-Updates

**Nach Updates suchen …** ruft den gemeinsamen GitHub-Releasekanal ab. Nach der
getrennten Bestätigung lädt und installiert die App das passende signierte
AppImage beziehungsweise DEB. Linux-Paketarten bleiben getrennt; eine ungültige
Signatur wird abgelehnt. Der nächste reguläre Start verwendet die neue Version.
Vor 1.0 bleiben Prüfung und Installation ausdrücklich ausgelöste Menüaktionen,
ohne Hintergrundabruf oder erzwungenen Neustart. [Updatevertrag](update-design.md).

Ein aus dem Quellstand lokal gebautes AppImage enthält denselben öffentlichen
Updateschlüssel und Endpunkt. Eine spätere höhere veröffentlichte Version kann
es regulär aktualisieren. Änderungen unter `Unreleased` mit unveränderter
Versionsnummer erzeugen keinen eigenen Updatefund.

## Aus dem Quellstand bauen

Rust, Node.js 20+, pnpm und die Linux-Tauri-Buildabhängigkeiten werden benötigt;
die [Projektregeln](../AGENTS.md#bauen-und-testen) nennen die Prüfungen.
`./build.sh` baut lokale DEB-/AppImage-Pakete ohne Zugriff auf den privaten
Updateschlüssel. Diese Pakete sind keine signierten öffentlichen Releases.
`./release.sh` verwendet den bestehenden signierten Linux-Releaseweg. Das
Erstellen von Paketen veröffentlicht sie nicht.

## Prüfstand

Der Entwicklungsstand vom 2026-11-07 besteht die Core-, CLI-, App-, Werkzeug-,
Clippy-, Format- und Fuzz-Prüfungen auf macOS und Mint/Ubuntu. Alle Browserteilsuiten
einschließlich der Linux-Größeneinheiten bestehen ebenfalls. Das fertige AppImage
ist auf Linux mit Kopieren, Umbenennen, Verschieben, einer lokalen Rechteänderung,
zwei getrennten Fensterqueues, regulärem Beenden und der echten GitHub-Updateprüfung
geprüft. Paketlayout, ELF-Architektur, Bibliotheken und beide CLI-Versionen sind belegt.
Die native Menüleiste samt aufgeklappten Menüs ist in Hell und Dunkel sowie nach
der Rückkehr zu Automatisch geprüft; der Fensterstart mit dem Wayland-Client
des Systems ist auch auf einem aktuellen Mesa-/WebKitGTK-System belegt.

Der erhöhte Helfer besteht einen isolierten Root-Gegenlauf mit Rechte- und
Eigentümeränderung, Abbruch und unveränderten Inhalten/Symlinkzielen. Ein betreut
bestätigter Polkit-Desktopdialog und ein Versionswechsel über einen künftigen
GitHub-Release sind damit noch nicht belegt. Die Freigaben für einen benannten
öffentlichen Kandidaten bleiben im [Releaseplan](release-plan.md) getrennt.
