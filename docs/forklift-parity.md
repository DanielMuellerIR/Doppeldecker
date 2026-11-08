# Metadaten: ForkLift als Releasemaßstab

Stand: 2026-11-01. Releaseziel ist die für gewöhnliche Dateiarbeit belegte
ForkLift-Parität, keine vollständige Migration aller Dateisystem-/Servermodelle.
Vorhandene stärkere Garantien und Funktionen bleiben erhalten.

## Belegte Alltagslücken

| Funktion | Referenz | Doppeldecker / nächste Wirkung |
| --- | --- | --- |
| Berechtigungen ändern, auch auf enthaltene Objekte anwenden | ForkLift-Versionshistorie, 4.1.8 | Direkte und rekursive Rechteänderung lokal, über SFTP und FTP/FTPS implementiert und auf beiden Plattformen geprüft; native Abnahme mit den endgültigen macOS-/Linux-Paketen einschließlich rekursiver SFTP-/FTP-/FTPS-Änderungen, Sonderbits, Abbruch und Teilfehlern abgeschlossen. [Vertrag und Grenzen](file-properties.md). |
| Verfügbare Eigentümer-/Gruppenfelder bearbeiten | ForkLift-Funktionsbeschreibung | Lokal implementiert, auf macOS auch mit ausdrücklich gewählter Administratorfreigabe. Im kontrollierten SFTP-Infofenster von ForkLift 4.4.4 sind Owner/Gruppe reine Textanzeigen; ein entfernter Editor ist dadurch keine belegte Releasepflicht. IDs niemals still zwischen Identitätsdomänen übertragen. |
| Entfernte Eigentümer-/Gruppennamen anzeigen | ForkLift 4.3, Versionshistorie | Für SFTP mit passender OpenSSH-Erweiterung implementiert und auf beiden Plattformen geprüft; numerischer Rückfall bleibt erhalten. [Anzeigevertrag](sftp-listing.md). |
| Lokale macOS-Tags anzeigen und bearbeiten | ForkLift-Funktionsbeschreibung und Versionshistorie | Informationsdialog und macOS-Backend implementiert; Tags, Farben und alte Farbmarkierungen getrennt bearbeitbar. Andere xattrs bleiben erhalten. Native macOS-Läufe einschließlich Neustart geprüft. |
| Erstellungsdatum beim lokalen Kopieren erhalten | [ForkLift 4.1](https://blog.binarynights.com/2024/03/05/forklift-4-1-is-available/) | Auf macOS für Dateien und neue Ordner implementiert, einschließlich Nanosekunden und sichtbarer Auslassung bei fehlender Zielfähigkeit. [Kopiervertrag](copy-metadata.md). |
| Verstecktmarkierung im Informationsdialog ändern | ForkLift 4.2.1, Versionshistorie | Informationsdialog, lokaler Pane-Filter und Unterordnersuche implementiert; natives Setzen und Entfernen nach Neustart geprüft. Punktnamen bleiben unabhängig verborgen. |
| Dateisperre und ausgeblendete Erweiterung bearbeiten | Kontrolliertes Infofenster und Attributabgleich mit ForkLift 4.4.4 | Beide Editoren einschließlich Auswahlen implementiert. Native Abnahme bestätigt Setzen, Neustart und Entfernen ohne Änderung anderer Metadaten. Gewöhnliche lokale Kopien erhalten auch das Sperrflag. [Grenzen](file-properties.md). |
| Finder-kompatible Dateikommentare bearbeiten | [ForkLift 4.6.2](https://blog.binarynights.com/2026/05/12/forklift-4-6-2-is-available-stability-improvements-and-important-fixes-for-icon-previews-on-macos-sequoia/) | Editor mit nativer Finder-Anbindung implementiert. Speichern, Neustart und Entfernen geprüft; Finder-Infofenster bestätigt die Anzeige. [Grenzen](file-properties.md). |
| Weitere darstellbare Transfermetadaten | Kontrollierte lokale Kopie mit ForkLift 4.4.4 | Tags, Finder-Informationen, lesbare xattrs, Resource Forks, Verstecktmarkierung und genaue Erstellungszeit sind implementiert. Die im Vergleich gefundenen gekürzten Änderungszeit-Nanosekunden sind korrigiert; die erneute native F5-Kopie stimmt in allen erfassten Werten mit ForkLift überein. [Kopiervertrag](copy-metadata.md). |

Quellen, geprüft am 2026-11-01:
[ForkLift-Funktionsbeschreibung](https://blog.binarynights.com/2023/04/04/forklift-4-beta-is-finally-here/),
[Versionshistorie](https://binarynights.com/versionhistory),
[Bedienübersicht](https://binarynights.com/manual).
Die Quellen belegen Bedienfunktionen; sie versprechen keine universelle
ACL-/Ownerübertragung für alle Protokolle. Fehlende Dokumentation beweist weder
Vorhandensein noch Unmöglichkeit. Ein neuer Paritätsbefund braucht einen konkreten
Herstellerbeleg oder einen kontrollierten Vergleich mit gleichen künstlichen Daten.

Beim lokalen Vergleich mit gleichen künstlichen Dateien erhielt ForkLift 4.4.4
Daten, xattrs, Resource Fork, Rechte und genaue Zeiten. Die zusätzliche explizite
Quell-ACL übernahm keines der beiden Programme. Daraus folgt keine universelle
Aussage zu anderen ACLs oder Transferwegen.

ForkLift 4.2 nennt einen passwortpflichtigen Helfer für erhöhte Rechte.
Doppeldecker bietet einen auf lokale Rechte und Eigentümer/Gruppe begrenzten
Freigabeweg. Der native Test bestätigt drei durch macOS freigegebene Aktionen:
Eigentümerwechsel zu `root`, Rechteänderung und Rückwechsel.
[Herstellerbeleg und Freigabeverhalten](https://binarynights.com/support).

## Zukunftswünsche

Weitere SMB-ACE-/Vererbungsmodelle, SID-/Domänenmigration, vollständiger
SMB-Hardlink-Schreibvertrag und DAV-ACL-Schreiben bleiben außerhalb des
Releaseumfangs, solange kein konkreter ForkLift-Gegenbeleg eine Alltagslücke zeigt.
Weitere native Spezialmodelle werden ebenso bewertet. Bereits beobachtete
Auslassungen bleiben ehrlich sichtbar; eine Änderung des Releaseumfangs erklärt
keinen unvollständigen Transfer nachträglich für vollständig.

Technischer Bestand: [Metadatenvertrag](metadata-stage2-design.md).
Offene Umsetzung: [Backlog](../backlog.md).
