# Funktionsaudit: Datei- und Sync-Operationen

Stand: 2026-11-07. Diese Matrix beschreibt das implementierte Verhalten;
[Roadmap](roadmap.md) und [Backlog](../backlog.md) nennen die offene Arbeit.
Frühere Prüfberichte gelten für ihren datierten Stand. Grundlage bleiben die
[Produktprinzipien](product-principles.md) und das
[Produktaudit](usability-audit-2026-10-20.md#umsetzungsstand).

## Zweck und Legende

- **A:** mit den für die konkrete Wirkung erforderlichen Belegen abgesichert.
- **BE:** Best Effort mit sichtbarer Grenze, frischer Prüfung, einem Mutationsversuch
  und Readback. Kein blinder Retry.
- **N:** benötigte Fähigkeit oder Bindung fehlt; derzeit gesperrt.
- **—:** kein eigener Weg.

Die Tabellen setzen eine erfolgreiche Verbindung und frische Wirkungsprüfungen
voraus. Gewöhnliche Aktionen benötigen keine künstlichen Fähigkeitsproben.
Ablehnung, unvollständiges Listing oder Identitätsdrift können einzelne Wirkungen
sperren. Simulierte Browserantworten geben keine Serverfähigkeit frei.

## 1. Navigation und Darstellung

| Funktion | Lokal | SFTP | FTP/FTPS | WebDAV | SMB |
| --- | --- | --- | --- | --- | --- |
| Listing, Refresh, Verlauf, Sortierung | A | BE | A | A | A |
| Mehrere Tabs je Pane | A | A | A | A | A |
| Reguläre Datei öffnen / geprüfter Nur-Lese-Snapshot | A | BE | A | A | A |
| ZIP-Inhalt anzeigen / passive Inhaltsvorschau | A | BE | A | A | A |
| Ordnerreferenz als Favorit speichern | A | A | A | A | A |

[Tabs](pane-tabs.md) halten Verlauf, Auswahl, Sortierung und Scrollposition.
Favoriten speichern Navigationsreferenzen, keine Serverberechtigung.
[ZIP-Ansicht](archive-read.md) und [Entpacken](archive-extract.md) verwenden bei
entfernten Quellen eine geprüfte lokale Momentaufnahme. ZIP-Erzeugung und Entpacken
zu entfernten Zielen übertragen denselben versiegelten Zwischenstand auch nach
Konfliktpause oder Neustart. Nach dauerhaftem Erfolg werden belegte lokale
Ergebnisdaten freigegeben; Abbruch und unklare Ausgänge behalten sie.
[Archivupload](remote-pack.md).

SFTP liest über die private Inhaltsklasse mit gehaltenem READ-Handle und Hash;
das erste OPEN bleibt pfadbasiert. [Lesevertrag](sftp-contents.md).
[Ordnergrößen](sftp-directory-sizes.md) sind gesonderte Metadatenbeobachtungen.
Nicht bedienbare Namen oder fehlende Dateitypen werden sichtbar ausgelassen;
doppelte Namen und Protokollfehler bleiben Fehler. [Listing](sftp-listing.md).

## 2. Interaktive Dateioperationen

| Funktion | Lokal | SFTP | FTP/FTPS | WebDAV | SMB |
| --- | --- | --- | --- | --- | --- |
| Ordner anlegen | A | BE | BE | BE | BE |
| Normale Unix-Rechte bearbeiten, auch rekursiv | A | BE | BE | — | — |
| Eigentümer/Gruppe ändern; macOS/Linux auch mit Administratorfreigabe | A | — | — | — | — |
| Finder-Tags/Farben, Kommentare und Dateiflags bearbeiten | A (macOS) | — | — | — | — |
| Umbenennen im selben Ordner | A | BE | BE | BE | BE |
| Neue Datei / rekursive Kopie / Upload | A | BE | A / BE | BE | A |
| Reguläre Zieldatei bestätigt ersetzen | A / BE | BE | BE | BE | BE |
| Download in einen lokalen Ordner | A | BE | BE | BE | BE |
| Vorhandene Ordner zusammenführen | A | BE | A / BE | BE | A |
| Extern bearbeiten und explizit zurückspeichern | — | BE | BE | BE | BE |

Bei Schreibaktionen bezeichnet die Providerspalte das Ziel, bei Downloads die
Quelle. Vorhandene Ordner werden ohne Entfernen unabhängiger Inhalte zusammengeführt.

- **Lokal:** Ersetzung verwendet den privaten C2-Arbeitsbereich, soweit dessen
  Bindung belegt ist; ein gekennzeichneter Best-Effort-Weg kann ohne wiederherstellbare
  Altversion auskommen. [Temp- und Sicherungsreste](decision-lokale-restbereinigung.md).
- **SFTP:** Standard-Subsystem ohne Serverhelfer. Exklusive Temps, Streaming und
  Readback; dauerhafte Ersetzung über POSIX-Rename oder bestätigtes Remove/Rename.
  Ein gleichzeitiger Elternpfadtausch kann innerhalb desselben Serverkontos umleiten.
  Nicht übertragbare Metadaten bleiben sichtbar. [Schreibvertrag](sftp-writes.md).
- **FTP/FTPS:** Absicherung nur bei bewiesener No-Replace-Semantik; sonst kann RNTO
  trotz frischem Listing ein zwischenzeitlich entstandenes Ziel ersetzen.
  Downloads prüfen MLSD-Fakten, Länge und Transportabschluss; FSTAT und atomare
  Pfadbindung fehlen. [Inhaltsvertrag](ftp-contents.md).
- **WebDAV:** Bedingtes [MKCOL](webdav-folder.md) sowie
  [PUT/MOVE](webdav-upload.md) mit vollständigen frischen Listings und Readback.
  Die MKCOL-Antwort-ETag ist optional. Bestätigte reguläre Ersetzung hat keine
  Quarantäne. Downloads nutzen GET/If-Match; Elternbindung und Symlinkerkennung
  bleiben begrenzt. [Lesevertrag](webdav-contents.md).
- **SMB-Download:** Datei-ID, ChangeTime und Dateifakten werden am geöffneten Handle
  vor Inhaltsbeginn und nach EOF geprüft. Fehlende IDs lassen nur die ausgewiesene
  schwächere Attributbindung zu; Rennen zwischen Requests bleiben möglich.
  [Inhaltsvertrag](smb-contents.md).

Additive lokale Uploads nach FTP/FTPS und SMB sowie lokale WebDAV-Uploads und
Downloads aller angebotenen Remote-Provider lassen mehrdeutige Case-/Unicode-
Namensgruppen pfadgenau aus. Das erweitert weder Move noch Ersetzen pauschal.

Der [Informationsdialog](file-properties.md) ändert normale Rechte unter Erhalt
von Sonderbits, lokal auch Eigentümer/Gruppe, macOS-Tags und
Verstecktmarkierung, Dateisperre und Erweiterungsanzeige.
Ein Eigentümerwechsel kann Sonderbits entfernen; die Bilanz nennt das Ergebnis.
Links und Spezialdateien werden ausgelassen. Lokale macOS-Editoren einschließlich
App-Neustart sind nativ geprüft; direkte SFTP-Rechteänderungen und Anzeigenamen
ebenfalls. Weitere entfernte macOS-Abläufe folgen mit dem Kandidaten.

Lokale macOS-Kopien erhalten Erstellungszeit, Tags, lesbare xattrs, Finder-
Informationen, Resource Forks, Verstecktmarkierung und Dateisperre. Fehlende Klassen werden
bilanziert; ein unvollständiger Resource Fork verhindert den Quellabbau beim
kopierenden Move. [Kopiervertrag und Grenzen](copy-metadata.md).

### Kopieren und Verschieben getrennt bewerten

Verschieben benötigt zusätzlich eine eigene Quellentfernung. Lokale, SFTP-,
FTPS- und SMB-Fallbacks entfernen nach vollständigem Readback einzelne Blätter und
zuletzt leere Ordner als Best Effort. Das Austauschfenster vor der Einzelwirkung
bleibt sichtbar. WebDAV bietet keinen solchen Quellabbau. Innerhalb derselben
gehaltenen SFTP-Sitzung kann F6 bei freiem Zielnamen einen einzelnen Server-Rename
verwenden. Eine solche Rename-Fähigkeit erlaubt keine beliebigen Quelle→Ziel-Moves.

„Beide behalten“ vergibt und prüft einen neuen Namen im Core und setzt einen
atomaren No-Replace-Commit des konkreten Ziels voraus. Kollidierende Ordner bleiben
vollständig daneben erhalten. [Konfliktentscheidungen](conflict-decisions.md).

### Umbenennen und Quellenbindung

Gewöhnlicher Remote-Rename bindet die Quelle über ihren Pfad. Auch bei No-Replace
kann ein zwischen Listing und Mutation ausgetauschter gleichnamiger Eintrag getroffen
werden. Best Effort ergänzt das Zielkollisionsfenster; unklare Ausgänge erlauben
keinen erneuten Schreibversuch. [WebDAV-Rename](webdav-rename.md) prüft zusätzlich
Quellabwesenheit und die exakte Zielversion über bedingtes MOVE und Readback.

[Stapelumbenennung](batch-rename.md) unterstützt reine Case-/Unicode-Änderungen und
entfernte Datei-/Ordnerzyklen mit dauerhaft bilanziertem Zwischennamen. Rohname und
Providerbeleg werden frisch geprüft. Offene Zwischenordner bleiben auch für darin
liegende Sync-Wurzeln gesperrt.

### Finder-Drop in eine FTPS-Pane

Der native Ablauf belegt Pane-Root als Ziel, Kopie, Überspringen, dauerhaftes
Ersetzen, Fortschritt, Abbruch und Providerfehler bei unveränderter Quelle.
Bereits eingereihte Gruppen bleiben in der Teilbilanz. [Prüfbericht](finder-ftps-gui.md).

## 3. Löschen und Wiederherstellen

| Funktion | Lokal | SFTP | FTP/FTPS | WebDAV | SMB |
| --- | --- | --- | --- | --- | --- |
| Systempapierkorb | A | — | — | — | — |
| Neue Einträge in Provider-Quarantäne verschieben | — | N | N | N | N |
| Dauerhaftes Remote-Löschen mit Wirkungsbestätigung | — | BE | BE | BE | BE |
| Vorhandene Remote-Quarantäneläufe anzeigen | — | N | N | N | A |
| Quarantäne-Restore / endgültiges Leeren | A | N | N | N | N |

Der lokale [Papierkorb](trash-private-claim.md) verwendet private Claims und ein
Crash-Journal. Lokale Sync-Quarantäne bleibt davon getrennt: Restore ersetzt nie,
Leeren verlangt den exakten Lauftext, die Aufbewahrungsdauer ist kein Löschauftrag.
Kein Remote-Provider liefert den vollständigen Restore-/Purge-Vertrag; SMB kann
alte Läufe nur lesen.

Dauerhaftes Remote-Löschen bindet Auswahl und Rekursionsumfang, prüft jeden Eintrag
frisch und liest seine Abwesenheit zurück. Es gibt weder Papierkorb noch stillen
Retry. Mehr als 1000 Einträge oder 10 GiB benötigen zusätzlich eine exakte
Löschphrase. Stat-/Providerfehler beweisen keine Abwesenheit.
[Mutationsvertrag](webdav-mutation-boundary.md).

## 4. Synchronisation und Richtungen

Die Tabelle bezeichnet den **Zielprovider**; alle angebotenen Provider können
Quellen sein. Gewöhnlicher Remote-Inhaltssync verwendet vollständige Listings,
frische Quellen-/Zielprüfungen, Streaminghash und einen Mutationsversuch als
Best Effort. Remote↔Remote benötigt keinen lokalen Klartext-Spool.

| Zielwirkung | Lokal | SFTP | FTP/FTPS | WebDAV | SMB |
| --- | --- | --- | --- | --- | --- |
| Einweg AddOnly | A / BE | BE | BE | BE | BE |
| Einweg Update | A / BE | BE | BE | BE | BE |
| Einweg Mirror | A / BE | BE | BE | BE | BE |
| Bidirektionale Zielprojektion (GUI) | A / BE | BE | BE | BE | BE |

[Lokaler AddOnly/Update-Sync](local-sync.md) läuft über die Fensterqueue. Additionen
brauchen keine Laufphrase; Update braucht ContentHash und Wirkungsbestätigung.
Starke lokale Ziele behalten Quarantäne. Fehlt sie, nennt die Vorschau dauerhafte
Ersetzungen und Entfernungen. Mirror lässt bei Konflikten, Symlinks oder
Auslassungen sämtliche Entfernungen aus. Neustart verlangt frische Vorbereitung.
Verträge: [Inhaltssync](content-sync.md), [bidirektionaler Alltagssync](bidirectional-forward.md).

Einseitiger nativer SMB-Inhaltssync erhält bei geeigneten Endpunkten benannte
Dateiströme; Verzeichnisströme besitzen einen getrennten NativeBestEffort-Vertrag
mit Einzel- und Teilbilanz. Der portable bidirektionale Vertrag bleibt unverändert.
[Metadatenumfang](metadata-stage2-design.md#benannte-smb-datenströme-regulärer-dateien).

Der getrennte strikte ProviderSync bleibt an Listing-, Create-/Rename-, Flush-,
Platz- und Quarantänefähigkeiten gebunden. Gewöhnliche Upload-/Renamefähigkeiten
geben striktes Update/Mirror oder bidirektionales Rollback nicht frei.
Verträge: [Remote-Provider](remote-providers-design.md),
[strikter bidirektionaler Sync](bidirectional-sync-design.md).

## 5. Queue, Fehlercodes, Metadaten und Oberfläche

Jedes Fenster besitzt eigene Queue, Schranke, Vorbereitungen und Permits;
Verbindungen und externer Prozesslock sind gemeinsam. Handles, Secrets und Permits
werden nicht persistiert. `outcome_unclear` sperrt den normalen Retry auch nach
Neustart. Ausgelassene Resource Forks bleiben als Teilstatus erhalten.
Verträge: [Queue](operations-queue-design.md), [Fenster](goal-mehrere-fenster.md),
[Recoverybilanz](recovery-aggregation.md).

Core, Tauri und CLI reichen strukturierte Fehlercodes durch. Fehlende Fähigkeit,
fehlender Sitzungsbeleg und verweigerte Rechte bleiben unterscheidbar. Ein
Verbindungsfehler wird bei möglicher unklärbarer Schreibwirkung `outcome_unclear`.

`Portable` ist Default. Native Profile brauchen ContentHash; NativeBestEffort
bleibt im strikten Vertrag für Mirror und bidirektionale Paare gesperrt.
Technische Metadatenauslassungen werden getrennt bilanziert; ausgelassene
Dateieinträge sperren weiterhin Mirror-Entfernungen. Tatsächliche Mount-/Provider-
Fähigkeiten entscheiden, nicht der Plattformname. Listen sind zugänglich und
virtualisiert; Templates halten die strikte CSP.

## 6. CLI und Prüfbelege

Die [CLI](headless-cli.md) nutzt dieselben Core-Verträge. Downloads sind eine
`copy`-Richtung; eigene `download`-/`open`-Befehle und CLI-Favoritenverwaltung gibt
es nicht. Bidirektionale CLI-Läufe sind auf lokale Paare begrenzt.

Headless-, Browser- und native Prüfberichte behalten ihre jeweiligen Grenzen.
Lokale macOS-Informationseditoren und F5-Kopiermetadaten sind nativ geprüft;
Paket-, Upgrade-, Recovery- und weitere Plattform-/Providerabnahme folgen mit dem
Releasekandidaten. Aktueller Restumfang: [Backlog](../backlog.md).

## Pflege

Verhaltensänderungen aktualisieren die betroffene Tabellenzeile und ihren Vertrag
im selben Commit. Technische Details und Prüfchronik stehen einmal im verlinkten
Vertrags-/Prüfdokument. Historische Erfolge ersetzen keine aktuelle Freigabe.
