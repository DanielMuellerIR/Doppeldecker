# ZIP-Inhalt anzeigen und lesender Core

Stand: 2026-10-24. `core::archive_read` bietet ein lokales, GUI-freies Fundament
für ZIP-Kataloge, virtuelle Ordner und geprüfte Inhaltsstreams. Die App zeigt lokale
und entfernte ZIP-Dateien über „Befehle → ZIP-Inhalt anzeigen …“ und das Kontextmenü an. Die
Archivquelle bleibt unverändert. [Entpacken](archive-extract.md) startet als
eigener Ablauf zu einem lokalen Ziel: aus lokalen Archiven und Momentaufnahmen das ganze Archiv,
eine gewählte Datei oder der geöffnete virtuelle Unterordner. Die Auswahl ist
vor dem Schließen an die aktuelle Quelle und das Fenster gebunden.
Entfernte Quellen werden zunächst vollständig als geprüfte lokale Momentaufnahme
geladen. Entfernte Entpackziele bleiben ein eigener Ausbauschritt.

## Bedienung und Fensterbindung

Eine ausgewählte reguläre Datei öffnet einen eigenen Dialog. Der Backend-
Reader prüft das ZIP-Format; die Dateiendung erteilt keine Formatfreigabe. Virtuelle
Ordner lassen sich mit Enter oder Doppelklick öffnen, Pfadschaltflächen führen
zurück. Jede Seite enthält höchstens 100 Zeilen. Explizite leere Ordner, gleiche
Datei-/Ordnernamen und doppelte zentrale Einträge behalten ihre eigene Zeile.
Ungültige UTF-8-Namen erscheinen mit ihren hexadezimalen Rohbytes. Problemzeilen
und die konkrete Teilkatalogbilanz bleiben beim Navigieren sichtbar.

F3 oder „Vorschau (F3)“ liest ausschließlich den gewählten Eintrag. UTF-8-Text ist
bis 1 MiB, statisches PNG/JPEG bis 8 MiB und 24 Millionen Pixel darstellbar. HTML,
SVG und andere Formate werden nicht aktiv ausgeführt. Die Vorschau erscheint im
selben Dialog; virtuelle Einträge werden nicht an eine externe App übergeben.
Escape oder „Schließen“ beendet den Dialog und gibt den Fokus an die Pane zurück.

Jedes Fenster besitzt eine eigene kurzlebige Archivlesesitzung. Nur das Öffnen
nimmt Dateisystempfade an; Navigation und Vorschau verwenden danach ausschließlich
backendseitige Sitzungs-, Ordner- und Eintrags-IDs. Lokale Roots und Quellen durchlaufen die
gemeinsame kanonische Sandboxprüfung. Die Ansicht verwendet weder Operations-Queue
noch Sync-Abbruchzustand oder Mutationsfreigabe.

Anfragen werden vor dem Hintergrundstart reserviert. Früher Abbruch, verspätete
Antworten, Fensterzerstörung und ein Wechsel von Pane, Tab oder Auswahl dürfen
keine inzwischen verworfene Ansicht wiederherstellen. Sitzungs- und einzelne
Vorschauabbrüche sind getrennt: Nach einer abgebrochenen Vorschau bleibt ein
anderer Eintrag derselben Sitzung lesbar. Während ein Inhaltslesevorgang endet,
bleibt die Katalognavigation nutzbar; ein weiterer Inhaltsworker startet erst
danach. Zehn Minuten ohne Aktivität verwerfen die Sitzung zeitgesteuert und
brechen ihren laufenden Lesevorgang kooperativ ab.

## Entfernte Quellen und Lebensdauer der Momentaufnahme

SFTP, FTP/FTPS, WebDAV und SMB verwenden den vorhandenen
[Momentaufnahmevertrag](remote-providers-design.md). Das Öffnen lädt die gesamte
Archivdatei, bevor der ZIP-Katalog erscheint; es gibt keinen entfernten Range-Reader.
Der Ladezustand nennt diese Wirkung und lässt sich schließen oder mit Escape
abbrechen. Profil, ausgewählter Root und Quellpfad werden im Backend aufgelöst;
eine zwischenzeitlich geänderte Profilbindung verhindert die Übernahme der Ansicht.
Die jeweiligen Protokoll- und Quellenbindungsgrenzen bleiben erhalten.

Die Herkunftszeile „Momentaufnahme von …“ nennt Protokoll und Quellpfad.
Navigation und Vorschau lesen danach dieselbe lokale Kopie; spätere Änderungen
auf dem Server werden erst nach erneutem Öffnen sichtbar. Die bestehenden ZIP-,
Katalog- und Vorschaugrenzen gelten unverändert. Das ganze Archiv, eine ausgewählte
reguläre Datei oder der geöffnete virtuelle Unterbaum lassen sich aus dieser
gebundenen Aufnahme in die lokale oder entfernte Gegenpane
[entpacken](archive-extract.md#bedienung).
Der Ganzarchivstart verwendet auch aus einem tief geöffneten Ordner die
Archivwurzel; die Unterbaumaktion bleibt am geöffneten Ordner gebunden.
Entfernte Ziele verwenden den [zweiphasigen Archivupload](remote-pack.md). Es gibt beim Entpackclaim keinen
erneuten Download.

Die appverwaltete Momentaufnahme besitzt einen geteilten Manifestlock, solange
ein Archivreader sie hält. Der Lock reist mit laufenden Inhaltsworkern; Schließen,
Fensterzerstörung oder Sitzungsablauf geben die Datei nicht vor deren Ende zur
Bereinigung frei. Mehrere Leser dürfen dieselbe Aufnahme halten. Die beim Öffnen
angestoßene Bereinigung mindestens sieben Tage alter Aufnahmen überspringt aktive Aufnahmen mit einem
nichtblockierenden exklusiven Lockversuch. Der letzte Leser gibt nur seinen Lock
frei; er löscht die Aufnahme nicht automatisch. Fremde oder unzulässige
Manifesttypen werden ausgelassen, echte Cache-/I/O-Fehler bleiben sichtbar.
Entpackvorbereitung und ausführender Plan halten ebenfalls ihre eigene Lease;
eine wartende Queueabsicht garantiert dagegen keine dauerhafte Aufbewahrung.
Beim Claim werden dieselbe Referenz, das Manifest und der vollständige Inhalt
frisch geprüft. Eine fehlende Aufnahme verlangt erneutes Öffnen.

## Katalog, Namen und Navigation

`ArchiveRead::open` bindet eine reguläre lokale Datei unter einem gehaltenen Root.
Der Quellpfad ist relativ und enthält ausschließlich normale Komponenten. Ein
Ordner wird vor einer rekursiven Quellbindung als Nichtdatei abgelehnt.
`ArchiveRead::open_snapshot` übernimmt ausschließlich einen vom bekannten Store
geprüften Snapshotbeleg mit gehaltenem Datei-Deskriptor und Lease. Es öffnet keine
vom Frontend übergebene Cachequelle erneut per Pfad. Der bekannte Cache-Root wird
wie beim Download kanonisch gebunden; Snapshotkinder bleiben descriptor-relativ
ohne Symlinkfolgen geprüft.

Das Zentralverzeichnis bestimmt die Einträge. `raw_name` enthält die unveränderten
Namensbytes; `name` entsteht nur bei strikt gültigem UTF-8. Es gibt weder
Ersatzdekodierung noch Normalisierung oder Zusammenlegung von Case-/Unicode-Namen.
Exakt gleiche Rohbytes werden als Duplikate markiert. Jeder zentrale Eintrag behält
seine eigene backendseitige `ArchiveEntryId`; gleiche Namen bleiben dadurch getrennt
lesbar. Eine ID gilt ausschließlich für die erzeugende Lesesitzung.

Ungültiges UTF-8, unsichere Pfade, zu lange oder zu tiefe Namen, Verschlüsselung,
unbekannte Kompression, nicht unterstützte Typen sowie fehlerhafte Header oder
Bereiche erscheinen als einzelne Problemzeilen. Symlinks und Spezialtypen werden
nicht als reguläre Inhalte gelesen. `read_entry_to` akzeptiert ausschließlich
Dateieinträge ohne solche Probleme; das Duplikatkennzeichen allein sperrt einen
Eintrag nicht.

`directory` bildet virtuelle Ordner aus unveränderten sicheren Pfadsegmenten.
Absolute Pfade, leere innere Segmente, `.`/`..`, Backslashes, Doppelpunkte,
Nullbytes und Steuerzeichen sind nicht navigierbar. Unsichere Namen bleiben im
Wurzelkatalog erreichbar. Diese Navigation ist eine Sicht auf den gespeicherten
Katalog, keine Dateisystempfad-Auflösung und kein erneuter Quellscan.

Ein lesbarer Katalog muss nicht vollständig sein: Eintrags-/Metadatenlimit oder
ein späterer Strukturfehler können einen bereits gelesenen Präfix samt
`stop_reason()` liefern. `is_complete()` bestätigt nur die Vollständigkeit des
zentralen Katalogs. Aufrufer müssen die Teilbilanz sichtbar erhalten. Quellen-,
I/O-, Abbruch- und I/O-Budgetfehler können stattdessen bereits `open` scheitern
lassen; ein Teilkatalog ist nicht für jeden Fehler zugesagt.

## Tatsächliche Ressourcengrenzen

`CatalogBudget` begrenzt die Katalogaufnahme. Die Standardwerte sind:

| Größe | Standard | Wirkung |
| --- | ---: | --- |
| Einträge | 20.000 | Katalog vor dem nächsten Eintrag anhalten |
| Zentrale Metadaten | 16 MiB | Summe aus 46 Byte Zentralheader, Rohname, Extras und Kommentar pro aufgenommenem Eintrag |
| Namensbytes | 4.096 | Längeren Namen als Problemzeile führen |
| Pfadtiefe | 128 | Tieferen Namen als Problemzeile führen |
| Gelesene Archivbytes | 64 MiB | Weiteren Lesezugriff ablehnen |
| Nichtleere Leseaufrufe | 100.000 | Weiteren Lesezugriff ablehnen |

Namens- und Tiefengrenzen entfernen keine Rohdaten aus dem Katalog. Der feste
Parser-Scratchbedarf beträgt ungefähr 320 KiB; variable Katalogstrukturen sind
zusätzlich durch Eintragszahl und zentrale Metadaten begrenzt. Das Metadatenbudget
ist keine exakte Heapobergrenze: Indizes, Zeichenkettenkopien, Container und
Navigationsresultate benötigen weiteren Speicher. Die Archivgröße allein ist
keine Eintrittsgrenze; große Bereiche werden nicht vollständig eingelesen.

Jeder `read_entry_to`-Aufruf erhält ein eigenes `ReadBudget` für Ausgabebytes,
gelesene Archivbytes und Leseaufrufe. Die Eingabezähler werden pro Aufruf neu
begonnen. Die Ausgabe wird während des Streams begrenzt, einschließlich einer
zusätzlichen Byteprobe gegen zu lange Inhalte. Auch fehlerhafte Deflate-Daten
müssen innerhalb dieser Eingabe-/Ausgabegrenzen bleiben.

Die App begrenzt zusätzlich den virtuellen Ansichtsindex auf 10.000 Ordner und
32 MiB bilanzierte Metadaten. Sie prüft den Bedarf vor Aufnahme eines Eintrags;
Pfadschaltflächen entstehen erst für die angefragte Seite aus Elternverweisen.
Damit vervielfachen viele tiefe Namen nicht für jeden Ordner dessen gesamte
Pfadkette. Erreicht die Ansicht ihre Grenze, nennt sie die Zahl der dargestellten
und ausgelassenen Katalogeinträge. Dieses Budget ist ebenfalls keine exakte
Heapobergrenze.

Je Fenster werden eine Sitzung und höchstens ein laufender Katalog- oder
Inhaltsworker gehalten. Jede explizite Vorschau erhält 8 MiB Ausgabebudget,
16 MiB Archiv-Lesebudget und höchstens 100.000 Leseaufrufe; bereits größer
angekündigte Inhalte werden vor dem Entpacken für die Vorschau abgewiesen.
Navigation entpackt keine Inhalte. Über mehrere bewusste Vorschauaufrufe gibt es
keine kleine kumulative Verbrauchsgrenze. Der Entpacker verwendet eigene
Wirkungs- und Ressourcenverträge. Diese Grenzen betreffen weder gewöhnliches
Kopieren noch die Dateigröße beim ZIP-Erstellen.

## Inhaltsstream und Erfolgsbeleg

Stored und Deflate streamen an einen vom Aufrufer gestellten `Write`-Sink. **Alle
Sinkbytes bleiben bis zur erfolgreichen Rückgabe von `VerifiedArchiveRead`
vorläufig.** Bei Fehler oder Abbruch muss der Aufrufer sie verwerfen. Es gibt
keinen verifizierten Teilerfolg und keinen Rückrollmechanismus für den Sink.

Erfolg verlangt die angekündigte unkomprimierte Größe und CRC. Stored muss seinen
gebundenen Datenbereich bis zum Ende lesen. Deflate muss das tatsächliche
Streamende erreichen und exakt die vollständige zentral angekündigte komprimierte
Bytezahl verbrauchen; abgeschnittene Ströme und angehängte Daten ergeben keinen
Erfolgsbeleg. Sinkfehler werden durchgereicht. Der Reader verspricht weder einen
Sink-Flush noch einen Dateisystemcommit; beides gehört zum späteren Aufrufer.

## Quellenbindung, Abbruch und Strukturgrenzen

Die Sitzung hält Root, Elternordner und Datei-Deskriptor. Datei-/Eltern-/Root- und
Mountbelege sowie Dateiversionen einschließlich hochauflösender mtime/ctime
werden ohne vollständiges Vorhashen gebunden. Lesezugriffe prüfen die Quelle davor
und danach; vor dem Inhalts-Erfolgsbeleg folgt eine erneute Prüfung. Beobachteter
Tausch oder Drift verhindert den Erfolgsbeleg. Die Quellenbindung und laufenden
Lese-/Inflate-Schritte beachten `Cancel`. Das ist kooperativer Abbruch, keine harte
Zeitgrenze für einen blockierenden Dateisystem- oder Sinkaufruf.

Lokale und zentrale Namen, Flags und Kompression müssen zusammenpassen. Ohne
Data-Descriptor-Flag werden zusätzlich die lokalen Größen und CRC verglichen.
Ungültige Datenbereiche sowie bekannte Überlappungen von Headern und Payloads
werden bilanziert und für Inhaltslesen gesperrt.

Eine vollständige ZIP-Strukturvalidierung wird ausdrücklich nicht zugesagt.
Data Descriptors bestimmen keine Lesegrenzen; insbesondere werden ihre
nachlaufenden Bereiche nicht vollständig validiert. Ein vollständiger Katalog
oder ein erfolgreicher Inhaltsaufruf beweist deshalb weder die strukturelle
Gültigkeit des ganzen Archivs noch die anderer Einträge. CRC ist eine
ZIP-Inhaltsprüfung, kein kryptografischer Echtheitsbeleg.
