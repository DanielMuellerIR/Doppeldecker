# Preview 0.1.125

Stand: 2026-11-07. Diese Preview erweitert die entfernte Dateiarbeit, Archive,
Dateieigenschaften und die Linux-Integration gegenüber 0.1.124.

## Entfernte Dateiarbeit und Archive

SFTP, FTP/FTPS, WebDAV und SMB unterstützen gewöhnliche Dateiaktionen mit ihren
konkreten Protokollgrenzen. Kopieren und Verschieben, Ordneranlage, Umbenennung
und Konfliktentscheidungen laufen über die Queue des jeweiligen Fensters.
Entfernte Datei- und Ordnernamen lassen sich auch zyklisch vertauschen.

WebDAV liest auch Ordner ohne ETag oder Inhaltslänge und verwendet sie als
Ziel gewöhnlicher additiver Uploads und Ordneranlagen. Dateien benötigen
weiterhin die geprüften Metadaten und einen starken, standardkonformen ETag.

Eine lokale Auswahl lässt sich als ZIP zur entfernten Gegenpane übertragen.
ZIP-Archive, einzelne enthaltene Dateien und Unterbäume lassen sich in lokale
oder entfernte Ziele entpacken. Konfliktpausen und Neustarts verwenden denselben
geprüften Zwischenstand. Erfolgreiche Uploads geben belegte lokale Ergebnisdaten
frei; offene oder unklare Vorgänge behalten ihre Daten zur Prüfung.

## Dateieigenschaften und lokale Kopien

Der Informationsdialog ändert normale Unix-Rechte lokal, über SFTP und FTP/FTPS,
auch rekursiv. Lokale Eigentümer und Gruppen lassen sich über Namen oder IDs
bearbeiten; macOS und Linux bieten dafür und für Rechteänderungen eine optionale
Administratorfreigabe. SFTP zeigt vom Server gelieferte Eigentümer- und
Gruppennamen, soweit dessen Erweiterungen das ermöglichen.

Auf macOS bearbeitet der Dialog Finder-Tags und Farben, Kommentare,
Verstecktmarkierung, Dateisperre und ausgeblendete Erweiterungen. Gewöhnliche
lokale Kopien erhalten dort auch die darstellbaren Erstellungszeiten, Tags,
Finder-Informationen, lesbaren erweiterten Attribute und Resource Forks.
Änderungszeiten behalten auf macOS und Linux ihre Nanosekunden. Auslassungen
bleiben sichtbar; bestehende zusammengeführte Ordner behalten ihre eigenen Zeiten.

## Synchronisation und Wiederherstellung

Einseitiger und bidirektionaler Inhaltssync verwenden konkrete Vorschauen und
fenstereigene Aufträge. Starke lokale Ziele sichern Ersetzungen und
Spiegel-Entfernungen in Quarantäne. Gewöhnlicher Remote-Sync kann nach
Wirkungsbestätigung endgültig ersetzen und entfernen. Unklare Serverantworten
führen zu einer sichtbaren Teilbilanz und keinem blinden mutierenden Retry.

## Plattformen und Abnahme

Das Linux-AppImage enthält die für WebKit benötigte GStreamer-Runtime und
portable Starthooks. Es verwendet den Wayland-Client des Systems, damit
neuere Mesa-/EGL-Treiber die Oberfläche zuverlässig starten können. Native
Menüleisten und Popups folgen der gewählten hellen oder dunklen Darstellung.
Der benutzereigene Linux-Installer erhält einen stabilen,
beschreibbaren AppImage-Pfad für die signierten GitHub-Updates. Die optionale
Administratorfreigabe verwendet Polkit ohne dauerhaften Rootdienst.
Bei einem unlesbaren Queue-Journal lässt sich die App
nach einem klaren Hinweis regulär beenden, ohne das Journal zu verändern oder
laufende Arbeit zu übergehen.

Die Abnahme und die jeweils belegten Plattformgrenzen stehen im Releaseplan.
ZIP bleibt das unterstützte Archivformat; Finder-Funktionen benötigen macOS. Zusätzliche
Protokoll- und Metadatengrenzen stehen in der [Funktionsmatrix](function-audit.md).
[Alle Änderungen](../CHANGELOG.md#01125---2026-11-07) · [Kandidatenabnahme](release-plan.md).
