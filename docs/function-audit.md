# Funktionsaudit: Datei- und Sync-Operationen

Stand: Preview 0.1.124 vom 2026-10-19. Die Grundmatrix wurde am 2026-10-07 abgeglichen.
Ergänzung 2026-10-18: normaler lokaler Sync einschließlich schwacher Zielklassen
und begrenzte interaktive SFTP-Inhalte samt Anzeigebilanz sowie gewöhnliches
WebDAV-mkdir, -Rename und additive Datei-/Baumuploads;
übrige Providerfreigaben behalten ihren
damaligen Prüfstand.

## Zweck und Legende

Diese Matrix beschreibt das heute angebotene Verhalten. Frühere erfolgreiche
Providerproben sind kein Ersatz für den aktuellen Sicherheitsvertrag. Browsertests
mit simulierten Providern geben keine echte Serverfähigkeit frei. Die
[Produktprinzipien](product-principles.md) verlangen die stärkste begrenzbare
Wirkung und konkrete Gründe für verbleibende Sperren.

- **A:** abgesichert durch die für die konkrete Wirkung erforderlichen Belege.
- **BE:** Best Effort mit sichtbarer Einschränkung, frischer Prüfung, genau einer
  Mutation und Readback; kein blinder Retry.
- **N:** derzeit gesperrt; benötigte Bindung oder Fähigkeit fehlt.
- **—:** kein eigener Weg für diese Funktion.

Die Tabellen gelten nach erfolgreicher Verbindung und den erforderlichen frischen
Proben. Ablehnung, unvollständiges Listing oder Identitätsdrift können einzelne
Wirkungen zusätzlich sperren. „Umgesetzt“ umfasst auch belegte Sperren und bedeutet
keine vollständige Protokollunterstützung.

## 1. Navigation und Darstellung

| Funktion | Lokal | SFTP | FTP/FTPS | WebDAV | SMB |
| --- | --- | --- | --- | --- | --- |
| Listing, Refresh, Verlauf, Sortierung | A | BE | A | A | A |
| Mehrere Tabs je Pane | A | A | A | A | A |
| Reguläre Datei öffnen / geprüfter Nur-Lese-Snapshot | A | BE | A | A | A |
| Ordnerreferenz als Favorit speichern | A | A | A | A | A |

Tabs und Favoriten speichern Navigationsreferenzen, keine Serverberechtigung.
Das [Tab-Modell](pane-tabs.md) hält Verlauf, Auswahl, Sortierung und Scrollposition
je Tab; die native macOS-Abnahme schließt einen echten App-Neustart ein.

SFTP verwendet das getrennte `SftpPathBestEffort`-Listing sowie eine private
Inhaltslesesicht für reguläre Dateien und begrenzte Verzeichnisbäume. Hostkey, Sitzung,
beobachtbare Pfadattribute, READ-Handles, Größe und vollständiger BLAKE3-Readback sind gebunden.
Das erste OPEN bleibt pfadbasiert; ein unbemerkter Austausch mit identischen
Attributen ist kein ausgeschlossenes Risiko. Striktes VFS, Rootidentitäten, Sync,
Quarantäne und Remote-Mutationen bleiben gesperrt. Vertrag und Gegenbelege:
[SFTP-Inhalte](sftp-contents.md).

SFTP-Ordnergrößen verwenden eine eigene begrenzte Metadatenbeobachtung ohne Datei-
OPEN. Große Größen sind damit zählbar, ohne Inhaltsdownloadgrenzen zu lockern.
`observed`, „≈“, Best Effort und sichtbare Teilbilanzen ersetzen keine strikte
Listingfähigkeit. Vertrag: [SFTP-Ordnergrößen](sftp-directory-sizes.md).
Das interaktive Listing lässt nicht bedienbare Namen und fehlende Dateitypen
mit sichtbarer Bilanz aus; unabhängige Dateien bleiben nutzbar. Doppelte Namen
und Protokollfehler bleiben Fehler. [Vertrag](sftp-listing.md).

## 2. Interaktive Dateioperationen

| Funktion | Lokal | SFTP | FTP/FTPS | WebDAV | SMB |
| --- | --- | --- | --- | --- | --- |
| Ordner anlegen | A | N | A | BE ⁶ | A |
| Umbenennen im selben Ordner | A | N | A / BE ¹ | BE | A |
| Neue Datei / rekursive Kopie / Upload | A | N | A / BE ¹ | BE ⁷ | A |
| Reguläre Zieldatei bestätigt ersetzen | A / BE ² | N | BE | N | N |
| Download in einen lokalen Ordner | A | BE ⁵ | A | A | A |
| Vorhandene Ordner zusammenführen | A | N | A / BE ¹ | BE ⁷ | A |
| In-Place-Schreiben in Remote-Datei | — | N | N | N | N |

Bei Schreibaktionen bezeichnet die Providerspalte das Ziel; die Downloadzeile
bezeichnet die Quelle. SFTP-Downloads dürfen deshalb vorhandene lokale Ordner
kontrolliert zusammenführen. Schreiben in eine SFTP-Collection bleibt gesperrt.

¹ FTP/FTPS ist nur nach bewiesener No-Replace-Semantik abgesichert. Bei
überschreibendem RNTO steht interaktiven Aktionen ein eigener Best-Effort-Weg zur
Verfügung: vollständiges Elternlisting frisch prüfen, genau eine Mutation senden,
Ergebnis zurücklesen. Zwischen Listing und Rename kann ein neu entstandenes Ziel
dennoch ersetzt werden; der begrenzte Schaden betrifft genau diesen Eintrag.
Der unbeaufsichtigte Sync verwendet diese Lockerung nicht.

² Lokale Ersetzung nutzt den privaten C2-Arbeitsbereich, soweit dessen Bindung
belegt ist. Erhaltene Verzeichnissicherungen werden getrennt bilanziert. Der
generische Provider-Dateiweg kann eine ausdrücklich gekennzeichnete lokale
Best-Effort-Ersetzung liefern, ohne eine wiederherstellbare Altversion zu
versprechen. Vertrag: [lokale Temp- und Sicherungsreste](decision-lokale-restbereinigung.md).

⁵ Ausschließlich additive SFTP→lokal-Downloads von 1 bis 100 direkten Auswahlen
des Pane-Roots: Dateien bis 8 GiB und begrenzte Bäume bis 1000 Einträge,
16 Unterordnerebenen und 16 GiB. Keine Ersetzung oder Quellentfernung. Überspringen
und abgesichertes „Beide behalten“ erhalten bestehende
Ziele; lokale Commitgrenzen gelten zusätzlich zur schwachen SFTP-Leseklasse.

⁶ Gewöhnliches WebDAV-mkdir benötigt keine künstliche Collection-/Löschprobe.
Starke Eltern-ETags, vollständiges frisches Listing, genau ein bedingtes MKCOL und
Readback bleiben erforderlich. Die Antwort-ETag ist optional; ein unklarer Ausgang
erhält den möglichen neuen Ordner und seinen Prüfpfad. Vertrag:
[WebDAV-Ordneranlage](webdav-folder.md).

⁷ Lokale Dateiadditionen und begrenzte Verzeichnisbäume unter vorhandenen
WebDAV-Pane-Ordnern verwenden bedingtes MKCOL/PUT/MOVE ohne künstliche Collection-/
Löschprobe als eigene Best-Effort-Klasse. Exaktes Zusammenführen schützt vorhandene
Blätter; Ordnerintents erhalten Teilwirkungen ohne rekursives Cleanup. Überschreibende
und andere strikte Wege bleiben ohne gemeinsame Root-/Elternbindung passiv gesperrt. [WebDAV-Uploads](webdav-upload.md).

### Kopieren und Verschieben getrennt bewerten

Verschieben braucht zusätzlich zur Kopie eine eigene Quellentfernung.
`ProviderEndpoint::probe_file_capabilities` in `core/src/remote_real_mode.rs`
liefert getrennt `remove_source` und `remove_source_best_effort`. Die heutigen
lokalen, FTPS- und SMB-Fallbacks entfernen vollständig kopierte Quellen
als begrenztes Best Effort: frischer manifestgebundener Readback, Blatt für Blatt,
Ordner ausschließlich leer. Zwischen Readback und Einzelwirkung bleibt ein
sichtbares Austauschfenster. SFTP und WebDAV bieten keinen solchen Weg. Ein möglicher
No-Replace-Rename im selben Namespace wird gesondert bewertet und belegt keine
beliebigen Quelle→Ziel-Moves.

Lokales WebDAV-Copy mit Ersetzung ist seit der
[AP13-Tragfähigkeitsprüfung](webdav-replace-boundary.md) konkret gesperrt:
Standard-MOVE bindet Temp, Altziel und sämtliche Eltern nicht gemeinsam atomar.
Die privaten AP11/AP12-Additionen bleiben verfügbar. Die folgende historische
Ersetzungsprobe erteilt dafür keine Ausnahme.

Bestätigtes Ersetzen auf FTPS ist eine eigene verhaltensgeprüfte
Best-Effort-Wirkung für reguläre Datei gegen reguläre Datei. Die Altversion wird
ohne Quarantäne dauerhaft verworfen. GUI und CLI benennen diese Folge vor der
Bestätigung. Danach werden Temp-Verschwinden, Endname, Typ, Größe und BLAKE3-Inhalt
geprüft. Typkonflikte, Sync und Mirror nutzen diesen Weg nicht. SMB bietet derzeit
kein identitätsgebundenes bedingtes Parken und deshalb kein solches Ersetzen.

Beide behalten ist für Copy/Move über denselben Queue-/VFS-Weg umgesetzt, wenn
der konkrete Zielprovider einen atomaren No-Replace-Commit liefert. Der Core vergibt
und prüft den sichtbaren neuen Namen; ein ausgewählter kollidierender Ordner wird
vollständig daneben erhalten. Ohne diese Garantie nennt der Dialog den Grund für
die fehlende Variante; die normalen Best-Effort-Wege bleiben unverändert.
Details: [Konfliktentscheidungen](conflict-decisions.md).

### Umbenennen und Quellenbindung

Die gewöhnliche WebDAV-Umbenennung benutzt einen eigenen bedingten Best-Effort-Weg
ohne künstliche Löschprobe. Quelle, Sitzung und Elternkette werden bis zum
Einzel-MOVE gebunden; der Readback muss Quellabwesenheit und exakte Zielversion
belegen. Antwortverlust löst keinen automatischen Retry aus. Serverbedingungen
und starke ETags bleiben ausdrücklich begrenzte Serverbelege. Details:
[Gewöhnliche WebDAV-Umbenennung](webdav-rename.md).

`ProviderEndpoint::execute_interactive_rename` verwendet die Einstufung aus
`core/src/remote_rename.rs`. Auch die abgesicherte Klasse beschreibt den Schutz des
Ziels durch No-Replace. Die Quelle ist über ihren Pfad gebunden: Ein zwischen
Listing und Mutation ausgetauschter gleichnamiger Eintrag kann umbenannt werden.
`SOURCE_PATH_BINDING_LIMITATION` wird in Vorschau und Ergebnis mitgeführt.
Best Effort hat zusätzlich das oben beschriebene Zielkollisionsfenster. Sichtbare
Kollisionen stoppen mit `already_exists`; nach Antwortverlust darf nur eine
identitätsgleiche Lesesitzung den Zustand klären. Bleibt er unklar, folgt
`outcome_unclear`, ohne erneute Mutation.

### Finder-Drop in eine FTPS-Pane

Der integrierte native macOS-Lauf vom 2026-10-07 belegt echte Finder-Quelle,
Pane-Root als Ziel trotz unterlegter Ordnerzeile, Best-Effort-Kopie, Überspringen,
bestätigtes dauerhaftes Ersetzen, Fortschritt, Abbruch und konkreten Providerfehler.
Quelldaten bleiben unverändert; unabhängige Zielprüfungen bestätigen Inhalte und
fehlende Proben-/Tempreste. Die Korrektur erhält den konkreten Fehler zusammen mit
der Bilanz bereits eingereihter Gruppen. Beleg: [Finder→FTPS](finder-ftps-gui.md).

## 3. Löschen und Wiederherstellen

| Funktion | Lokal | SFTP | FTP/FTPS | WebDAV | SMB |
| --- | --- | --- | --- | --- | --- |
| Systempapierkorb | A | — | — | — | — |
| Neue Einträge in Provider-Quarantäne verschieben | — | N | N | N | N |
| Dauerhaftes Remote-Löschen mit exakter Phrase | — | N | N | N | N |
| Vorhandene Remote-Quarantäneläufe anzeigen | — | N | N | N | A |
| Quarantäne-Restore / endgültiges Leeren | A ³ | N | N | N | N |

³ Lokale Sync-Quarantäne ist vom Systempapierkorb getrennt. Restore ersetzt nie;
Leeren verlangt den exakten Lauftext. Die Aufbewahrungsdauer ist kein automatischer
Löschauftrag.

Der lokale [Papierkorb](trash-private-claim.md) verwendet auf macOS und Linux/XDG
private Claims und Journal v2. Neue Operationen benutzen keinen v1-Schreibweg.
Altjournal-Recovery bleibt konservativ und erfindet keine private Herkunft.

Remote-Quarantäne benötigt den vollständigen Vertrag aus
`Vfs::supports_bound_restore_probe`: identitätsgebundenes Create, Rename zwischen
gebundenen Eltern, Manifest-CAS und Remove. Keiner der aktuellen Remote-Provider
liefert ihn vollständig. Ein erfolgreicher gewöhnlicher Rename-/Delete-Test
genügt nicht. SMB kann alte Läufe lesen; `quarantine_management_capabilities`
gibt Restore und Purge ausdrücklich nicht frei.

WebDAV-Dauerlöschen bleibt seit AP13R passiv gesperrt: Ein Datei-`If-Match`
bindet nicht gemeinsam die vollständige Root-/Elternkette. Auch seine strikten
Capability- und Quarantäneabfragen erzeugen keinen Probenbestand.
[Mutationsvertrag](webdav-mutation-boundary.md). FTPS-DELE/RMD kann den bestätigten
Token nicht an die Wirkung binden. Ein Stat-/Providerfehler belegt niemals Abwesenheit.

## 4. Synchronisation und Richtungen

Der normale lokale GUI-Start führt AddOnly und Update auf abgesicherten lokalen
Zielen über die Fensterqueue aus. Additionen brauchen keine Laufphrase; Update
verlangt ContentHash, Wirkungsbestätigung und den vorhandenen Quarantäneweg.
Nach Neustart wird frisch vorbereitet und erneut bestätigt. Simulation ist ein
eigener Start ohne Schreibwirkung. Begrenzte schwächere lokale Zielklassen sind
ebenfalls umgesetzt; Mirror behält seinen getrennten Assistenten.
Konkrete Ziel- und Mengengrenzen: [Normaler lokaler Sync](local-sync.md).

Diese Tabelle bezeichnet den **Zielprovider**. Die Quelle muss einen vollständigen,
rootgebundenen Scan und die benötigten Dateiinhalte liefern; SFTP scheidet bereits
als Quelle aus. FTPS, WebDAV und SMB können dagegen alle drei Einwegmodi in ein
starkes lokales Ziel speisen.

| Zielwirkung | Lokal | SFTP | FTP/FTPS | WebDAV | SMB |
| --- | --- | --- | --- | --- | --- |
| Einweg AddOnly | A | N | A ⁴ | N | A |
| Einweg Update | A | N | N | N | N |
| Einweg Mirror | A | N | N | N | N |
| Bidirektionale Zielprojektion | A | N | N | N | N |

⁴ Nur nach erfolgreicher STOU-/Readback-/No-Replace-Probe. Ein FTP-/FTPS-Server
mit überschreibendem RNTO kann weiterhin interaktives Best-Effort-Kopieren bieten,
aber keinen AddOnly-Sync. Seine Transportstufe allein ist kein Funktionsgate.

`RemoteCapabilities` in `core/src/remote.rs` hält fünfzehn Fähigkeiten;
Capability-Digest Version 8 bindet sie an Vorschau und Permit:

- `permits_directory_create`: vollständiges Listing und exklusives Ordneranlegen.
- `permits_add_only`: vollständiges Listing, exklusives Create und dauerhaftes
  Datei-Flush oder verifizierter Datei-Commit.
- `permits_update`: zusätzlich No-Replace, Replace, Datei-Flush,
  Eintragsentfernung und geprüfte Quarantäne.
- `permits_mirror`: zusätzlich Verzeichnis-Flush und Platzabfrage.

Die stärkere Restore-Proofgrenze wird vor künstlichen Quarantäneobjekten geprüft.
Deshalb gibt SMB trotz Create-/FLUSH-/Rename-Fähigkeiten heute kein zielseitiges
Update/Mirror frei. Bidirektionale Zieladditionen brauchen ebenfalls den starken
Rollbackvertrag; ein gewöhnlicher AddOnly-Upload reicht nicht. Frühere
SFTP-/SMB-Freigaben aus der Implementierungschronik sind insoweit überholt.

Remote↔Remote benutzt dieselben `ProviderEndpoint`- und `ProviderSync`-Verträge,
keinen protokollspezifischen Ausweichweg und keinen lokalen Klartext-Spool.
Quelle und Ziel werden frisch verbunden; Profile, Roots, Fähigkeiten, Listings und
Plan sind gebunden. Die Zielmatrix gilt unverändert. Quellen und Prüfgrenzen:
[Remote-Vertrag](remote-providers-design.md),
[bidirektionaler Vertrag](bidirectional-sync-design.md).

## 5. Queue, Fehlercodes, Metadaten und Oberfläche

Jedes Fenster besitzt eigene Queue, Schranke, Vorbereitungen und Permits;
Verbindungen und externer Prozesslock sind gemeinsam. Journalversion 25 speichert
keine Handles, Secretwerte oder Permits. `outcome_unclear` sperrt den normalen Retry
auch nach Neustart. Verträge: [Queue](operations-queue-design.md),
[mehrere Fenster](goal-mehrere-fenster.md), [Recoverybilanz](recovery-aggregation.md).

`unsupported`, `capability_probe_required` und `permission_denied` unterscheiden
dauerhafte Grenze, fehlenden Sitzungsbeleg und echte Ablehnung. `connection_lost`
bleibt vor einer Mutation erhalten; ein unklärbarer möglicher Mutationsausgang wird
`outcome_unclear`. Core, Tauri und CLI reichen die Codes strukturiert durch.

`Portable` bleibt Default. Native Profile verlangen ContentHash; NativeBestEffort
bleibt für Mirror und bidirektionale Paare gesperrt. Die tatsächlich gehaltene
Mount-/Providerfähigkeit bestimmt jede Metadatenklasse, nicht der Plattformname.
Pane-Listen bleiben zugänglich und virtualisiert; Templates halten die strikte CSP.

## 6. CLI und Prüfbelege

Die [CLI](headless-cli.md) bietet `plan`, `real`, `mkdir`, `list`, `sync`, `copy`,
`move`, `rename`, `trash`, `delete`, `capabilities`, `quarantine`, `bidirectional`
und `connections import`. Es gibt keinen eigenen `download`-/`open`-Befehl und
keine CLI-Favoritenverwaltung. Ein Download ist eine passende `copy`-Richtung.
Die Providergrenzen dieses Dokuments gelten auch für alle CLI-Einstiege.

Vorliegende Belege umfassen Core-/CLI-/App-Regressionen auf macOS/Linux,
künstliche FTPS-/WebDAV-/verschlüsselte Samba-E2Es, SFTP-Schutztests und Browserfälle.
Der vollständige UI-Lauf vom 2026-10-07 besteht aus sechs Modelltests, 234
Browserfällen und CSP-Prüfung. Native macOS-Belege decken Finder→FTPS,
Papierkorb v2 und Tabs einschließlich Neustart ab. Historische Testzahlen bleiben
an ihren jeweiligen Stand gebunden; daraus folgt kein erneut ausgeführter Test.
Die interaktive Linux-GUI-/Installationsabnahme muss für den Releasekandidaten
wiederholt werden. Aktuelle Restarbeit: [Backlog](../backlog.md).

## Pflege

Bei Änderungen an Verhalten, Capabilities oder Gates wird diese Matrix im selben
Commit aktualisiert. Quellen möglichst über Datei und Symbol angeben; verschobene
Zeilennummern sind kein stabiler Vertrag. Neue Freigaben brauchen einen konkreten
Gegenbeleg, passende Tests und den Abgleich von GUI, CLI, Queue und Recovery.
Eine simulierte UI-Antwort oder ein historischer Erfolg hebt keine aktuelle
Sicherheitsgrenze auf.
