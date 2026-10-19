# Gewöhnliche additive WebDAV-Uploads

Stand: 2026-10-18. F5, Dateimenü und CLI `copy` verwenden für lokale reguläre
Dateien und begrenzte Verzeichnisbäume unter einem vorhandenen WebDAV-Pane-Ordner
eine private Best-Effort-Zielsicht.
Sie benötigt keine künstliche Collection, Collection-Löschprobe oder Quarantäne.
Strikter Sync, Mirror, Ersetzen und Move übernehmen diese Freigabe nicht;
die allgemeinen strikten WebDAV-Mutationen bleiben passiv gesperrt.

Die Elternkette wird vollständig vor und nach der Wirkung geprüft. Genau eine
getaggte `If`-Liste bindet den unmittelbaren Elternordner; weitere Listen könnten
diese Bedingung durch ODER abschwächen. Ein später Vorfahrenwechsel kann weiterhin
bereits wirken und erst im Readback als unklar auffallen.
[Mutationsgrenze und Gegenbelege](webdav-mutation-boundary.md).
Gewöhnliches lokales `copy --replace` wird deshalb konkret vor Providerzugriff
abgelehnt, statt auf den allgemeinen Probenweg auszuweichen.

## Auswahl, Vorbereitung und Queue

`ProviderEndpoint::interactive_file_contexts` liefert für GUI, CLI und Queue
gehaltene Quell- und Zielansichten samt der konkreten interaktiven Zielklasse.
Die globale Capability-Abfrage erteilt keine Schreibfreigabe und führt keine künstliche Probe aus. Diese Klasse ist ausschließlich
für additive lokale Inhaltsauswahlen bestimmt. Symlinks und Spezialdateien werden
auch in einer direkten Mehrfachauswahl pfadgenau ausgelassen; unterstützte Dateien
und Ordner bleiben übertragbar. Der bestätigte Umfang bindet diese Auslassungen.

Eingaben und Optionen werden vor Providerzugriff geprüft. Die Auswahl umfasst
1 bis 100 direkte Auswahleinträge, insgesamt höchstens 1000 Baumeinträge und
16 Ebenen unterhalb der direkten Auswahl (Dateien mitgezählt), 8 GiB je Datei
und 16 GiB Gesamtinhalt. Höchstens 128
Quellhandles bleiben offen, einschließlich Pane-Root und aller gebundenen Eltern;
kurzlebige Prüf-/Listingdeskriptoren und der Transport kommen hinzu. Die Handlegrenze
kann vor der Eintragsgrenze greifen. Namespacepfade
bleiben auf 4096 UTF-8-Bytes und 64 Komponenten begrenzt. Lokale Pfade werden zuerst
an ihren kanonischen Pane-Root gebunden; jeder Zugriff öffnet Eltern ohne
Symlinkfolgen descriptor-relativ. Der Core hält reguläre Datei-Deskriptoren,
Rootidentität, Größe, device/inode, mode sowie mtime/ctime mit Nanosekunden.
Ein vollständiger BLAKE3-Vorlauf bindet den Inhalt. Der Transfer liest denselben
gehaltenen Deskriptor mit eigenem Offset, prüft seine Version zwischen Blöcken,
vergleicht den vollständigen Hash am EOF und prüft die Quelle vor dem Zielcommit
erneut. Größe und zurückgesetzte mtime allein können einen Austausch nicht verbergen.

Die lokale Quellvorbereitung prüft eine Frist von 60 Sekunden zwischen den
I/O-Schritten; ein bereits blockierender Zugriff muss zurückkehren. GUI-Vorschau und
Einreihung halten ihre fenstereigene abbrechbare Reservierung und zeigen die
Inhaltsvorbereitung an. Navigation bleibt erreichbar; eine inzwischen veraltete
Antwort darf keine neue Pane-Auswahl ausführen. Der Queuekopf öffnet Quellen,
Verbindung und Profilbindungen frisch. Die versionierte Profildatei wird bereits
vor Secret-/Providerzugriff und anschließend bis vor jede weitere Uploadwirkung
gegen Revision und exakte Verbindungskonfiguration geprüft. Änderungen eines
unabhängigen Profils unterbrechen diesen Lauf nicht. Ein schon gestarteter PUT
kann seinen eigenen Tempbestand hinterlassen; nach Profilwechsel folgt kein MOVE
oder Cleanup-DELETE. Secrets, Handles, Sitzungen und
Serverversionstoken bleiben im Backend-RAM.

Die Oberfläche zeigt die konkrete Best-Effort-Klasse vor dem Start. Neue Dateien
brauchen keine Laufphrase. Eine gespeicherte Freigabe ersetzt nach Queue-Reopen
keine neue Anzeige: Journalformat 26 verwirft bei offenen lokalen WebDAV-Kopien
die alte Commit-Erwartung. Eine sicher noch nicht gestartete Addition fragt am
Queuekopf erneut; ein offener Datei-/Ordnerintent sperrt jede Wiederholung. Auch
zwischen abgeschlossenen Additionen bleibt eine beim Prozessende laufende Kopie
`outcome_unclear`: gespeicherte Zielpfade müssen geprüft werden. Ein neuer Executor
darf einen Baumbeleg nicht still ignorieren.
Fehlende frische Zustimmung wird ausdrücklich als solche erklärt. Ein kurzer
Dialogtitel und ein eigener, per Tab/PageDown erreichbarer Erklärungstext halten
Risikobilanz, Eingabefeld und Bestätigung auch im kleinen Fenster bedienbar.

## Begrenzte Bäume und gewöhnliche Ordneradditionen

Die Baumvorbereitung hält alle unterstützten Datei- und Eltern-Deskriptoren.
Verzeichnisse besitzen vollständige Rohname-/Attributlisten und eigene Versionen;
Dateien zusätzlich ihren vollständigen BLAKE3-Inhalt. Der Bestätigungsdigest bindet
Auswahl, Umfang, Auslassungen, Inhalte sowie Root-, Mount- und Elternidentitäten.
Eine frische Queuevorbereitung erkennt deshalb auch Inhaltswechsel bei gleicher
Größe. Während der Ausführung werden die gehaltenen Eltern, Dateiversionen und
beobachteten Ordnerbestände erneut geprüft. Zugriffe folgen keinem Symlink.

Symlinks und Spezialdateien sowie nicht dekodierbare und nicht bedienbare Namen innerhalb
eines Baums bleiben unangetastet. Ihre Pfade und Gründe stehen im Umfang; unsichere
Rohnamen erscheinen als ausdrücklich erklärte Prozent-Hex-Anzeige. Auch Namen,
die eine Eingabenormalisierung verändern würde, werden innerhalb eines Baums
ausgelassen; ein direkt gewählter solcher Ordner wird vor der Zielvorbereitung
abgelehnt. Der Core legt niemals einen still getrimmten Ersatznamen an. Möglicherweise
gleichwertige unterstützte Case-/Unicode-Namen sperren die Auswahl vor der ersten
Zielwirkung. Die Inhaltsvorbereitung ist vom gemeinsamen rekursiven Kopierer
getrennt; es gibt keinen zweiten Baumkopieralgorithmus.

Bestehende exakt zugeordnete echte Zielordner werden additiv zusammengeführt.
Reguläre vorhandene Dateien und Typkonflikte werden übersprungen oder führen zum
Abbruch, ohne Ersetzung oder unbewiesene Konfliktkopie. Vor jeder Kinderwirkung
bleibt die vollständige starke Elternkette vom Profilroot über den Pane-Root bis
zum jeweiligen Unterordner gebunden.

Neue Ordner verwenden denselben gewöhnlichen MKCOL-Vertrag wie die Einzelanlage:
frisches vollständiges Kollisionslisting, genau ein MKCOL mit `If-None-Match: *`
und genau einer unmittelbaren Elternbedingung, danach unabhängiger Readback von exaktem Namen,
leerem Collectiontyp, starkem Token und unveränderter Elternkette. Der private
Fenster-Queueordner enthält schon vor dem MKCOL einen geheimnisfreien Ordnerintent.
Ein fehlender neuer Collectiontoken verhindert jede Kinderwirkung. Ein Tokenwechsel
nach eigenen Kindern wird nicht als neue Identität übernommen: Der Lauf stoppt
und erhält den begrenzten möglichen Bestand als `outcome_unclear`.

Neue Ordner werden bei Fehler, Abbruch oder Prozessende niemals rekursiv entfernt.
Bereits fertige Additionen bleiben erhalten, einschließlich fremder Kinder in
zusammengeführten Ordnern. Offene Intents und Prüfpfade bleiben ihrer Fensterqueue
zugeordnet und erlauben weder automatisches Wiederholen noch späteres Löschen.

Vorschau, Queue und Abschluss zeigen Umfang und Auslassungen. Lange Quell- und
Metadatenbilanzen sind aufklappbare schreibgeschützte Textfelder, mit Tab/PageDown
bedienbar und kopierbar; das Dock bleibt begrenzt. Ein Dateizähler behauptet keine
Ordneranzahl. Alle Serverversionen, Handles und Secrets bleiben im Backend.

## Anlage, Inhaltsprüfung und Commit

Die private Zielsicht bindet die Sitzungsgeneration und alle starken Collection-
ETags vom kanonischen Profilroot bis zum gewählten Pane-Ordner. Vollständige frische
Listings prüfen auch mögliche Case- und Unicode-Kollisionen. Namen werden pro
Segment kodiert: `literal%2F.txt` bleibt ein wörtlicher Dateiname.

Der gemeinsame Dateikopierer erzeugt seinen zufälligen Tempnachbarn. Vor dem PUT
speichert die private Fensterqueue einen dauerhaften geheimnisfreien Intent mit
Operation, Tempname und beabsichtigtem Endnamen. Ein Prozessabbruch verliert damit
keinen möglichen Wirkpfad. Der Index enthält keine ETag und erteilt kein späteres
Löschrecht.

Es folgt genau ein gestreamtes `PUT` mit `If-None-Match: *` und genau einer getaggten
unmittelbaren Elternbedingung. Nur HTTP 201 samt starkem Antworttoken bindet den eigenen
Tempbestand. Ein bedingtes GET liest den vollständigen Inhalt zurück; der
BLAKE3-Hash muss zu den tatsächlich geschriebenen Bytes passen. Der Provider
verwendet den vorhandenen Streamingworker und der Core den vorhandenen Kopierer.
Es gibt keinen zweiten Dateitransferalgorithmus oder Klartext-Spool.

Nach erneuten Quell-, Eltern-, Temp- und Kollisionsprüfungen folgt genau ein
`MOVE` mit `If-Match` für die Tempdatei, genau einer getaggten Bedingung für den unmittelbaren
Elternordner und `Overwrite: F`. Erfolg verlangt unabhängig vom HTTP-Erfolgscode:
Tempname fehlt, exakter Endname ist eindeutig regulär, Größe und starker Token
passen zum PUT-Beleg, vollständiger BLAKE3-Inhalt stimmt und Sitzung/Eltern bleiben
gebunden. Der gemeinsame Koordinator liest das Ergebnis zusätzlich zurück.

Die HTTP-Schicht verwendet ausdrücklich `reqwest::retry::never()` und keine
Mutations-Redirects. Alle Mutationen laufen über einen eigenen Client mit frischen
HTTP/1-Verbindungen ohne Poolwiederverwendung; Leseverbindungen bleiben
wiederverwendbar. Auch ein Verbindungsverlust führt nicht zu einem zweiten
MKCOL/PUT/MOVE/DELETE.
PUT und Inhalts-GET haben eine Gesamtfrist von jeweils 300 Sekunden, Namespace-
Requests höchstens 30 Sekunden. Abbruch erfolgt zwischen Quellblöcken und vor dem
Commit. Ein bereits blockierender HTTP-Request muss seine Antwort oder Frist
abwarten; die Oberfläche behauptet keinen sofortigen Transportabbruch.

## Fehlerphase, Cleanup und Metadaten

Vor dem Workerstart beziehungsweise vor MOVE belegte Fehler bleiben mutationsfrei
für die jeweilige Wirkung. Ein abgebrochener Transfer darf ausschließlich seinen
bei PUT gebundenen eigenen regulären Tempbestand aufräumen: frische Elternkette,
frischer identischer starker Dateitoken, ein bedingtes DELETE ohne Collection-Depth
und anschließender echter NotFound-Readback. Eine Providerstörung ist keine Abwesenheit.
Ein nachweislich fremder oder unklarer Bestand bleibt erhalten. Drop führt in der
privaten Uploadklasse keine versteckte Mutation aus.

Antwortverlust nach PUT, fehlender/schwacher Anlagetoken, unklarer MOVE oder
fehlgeschlagenes DELETE liefern `outcome_unclear` mit konkreten Prüfpfaden.
Nach möglichem MOVE gibt es kein Temp-Cleanup. Der private Intent bleibt bei
unklarem Ausgang erhalten; ein Queue-Reopen ordnet ihn der verursachenden Operation
als `Interrupted` zu. Nur ausdrückliches Quittieren nach manueller Prüfung schließt
den Lauf ohne Mutation. Früher fertig kopierte Dateien und Teilbilanzen bleiben
bei einem späteren Fehler erhalten.

Diese Klasse überträgt Dateiinhalte und Ordnerstruktur. Dateizeit, POSIX-Rechte, Owner, ACL,
xattrs, Resource Forks, Sparse- und Hardlinkstruktur werden pfadgenau als ausgelassen
bilanziert, auch für Ordner. Erfolgs- und Teilfehlerdaten enthalten `target_write_limitations`.
Die GUI bietet die vollständige Bilanz als aufklappbaren schreibgeschützten Text;
CLI-Text und JSONL erhalten dieselben Angaben. Daraus entsteht kein NativeStrict-
oder anderer Sync-Metadatenvertrag.

## Grenzen und Prüfung

Best Effort vertraut den serverseitigen Bedingungen ohne künstliche Gegenprobe.
Missachtet der Server `If-None-Match` oder `Overwrite: F`, kann eine späte Kollision
genau ein Ziel ersetzen. Starke ETags sind Serverversionsbelege, keine lokalen
Objekt-Deskriptoren. Dauerhafte Speicherung ist nicht bewiesen. Ändert eine
Collection ihre ETag schon durch die eigene Addition, kann die unveränderte
Elternbindung den Erfolg nicht bestätigen; mögliche Inhalte bleiben mit unklarem
Ausgang erhalten. Server ohne starke Eltern- oder Datei-ETags erhalten keine
pauschale Freigabe. Konfliktkopien benötigen weiterhin bewiesenes No-Replace.

Die AP11-HTTPS-Fälle in `core/tests/webdav_upload.rs` prüfen Mehrdatei-/Leer-/Großupload,
Unicode und Prozentnamen, vorhandene/späte Kollisionen, Root-/Elterntausch,
Sitzungswechsel, Quelle gleicher Größe/Zeit, unvollständige Listings, fehlende/
schwache/wechselnde Tokens, falschen Inhaltsreadback, jede Mutationsverlustphase,
Abbruch vor und nach Teilwirkung sowie echte Prozessabbrüche vor und nach MOVE.
Wire-Zähler prüfen fehlende Collection-Proben, Mutationswiederholung und Ersetzung.
Browserfälle prüfen den F5-Klassenvertrag und eine lange, kopierbare, escaped
Metadatenbilanz samt Space/Tab/PageDown sowie lange Risiko- und SFTP-Auslassungstexte
mit erreichbarem Eingabefeld. Die damaligen vollständigen Rust-Gates sind auf
macOS und Linux bestanden. Die damalige native Abnahme belegt beide Themes,
echten F5, Abbruch, Fehlerbilanz, Prozessabbruch, frische Zustimmung nach pausiertem
Neustart und zwei gleichzeitig arbeitende Fensterqueues samt Navigation. Ihr
Wire-Gegenlauf zählt 12 PUT, 9 MOVE, ein eigenes Temp-DELETE und kein MKCOL;
Quellen sowie Sentinels außerhalb des gewählten Ordners blieben unverändert.

Standardbezug: [PUT, RFC 4918 §9.7](https://www.rfc-editor.org/rfc/rfc4918.html#section-9.7),
[MOVE/Overwrite, §9.9](https://www.rfc-editor.org/rfc/rfc4918.html#section-9.9),
[If-Bedingungen, §10.4](https://www.rfc-editor.org/rfc/rfc4918.html#section-10.4) und
[reqwest-Retry-Vertrag](https://docs.rs/reqwest/0.13.4/reqwest/retry/index.html).

Die AP12-Gegenfälle in `core/tests/webdav_upload_tree.rs` prüfen zusätzlich
Baum-Merge, leere Unterordner, Mehrfachauswahl, Auslassungen, Inhaltsdrift,
MKCOL-Readback und -Antwortverlust, geänderte Collectiontokens, Profilwechsel nach
Teilwirkung, Abbruch, Datei-Cleanupfehler und echten Prozessabbruch nach MKCOL.
Mutationzähler und Sentinels unterscheiden echte Additionen von Proben, Retry und
Ordnercleanup. Die lokalen Quelltests prüfen Auswahl-, Eintrags-, Tiefen-, Handle-,
Byte- und Zeitgrenzen sowie descriptor-relative Eltern-/Bestandsdrift.
Die vollständigen AP12-Rust-Gates sind auf macOS und Linux bestanden, einschließlich
Clippy und Fuzz mit `--locked`. 28 Upload- und 18 zusätzliche Baum-/Crashfälle sowie die
lokalen Umfangsgrenzen sind grün; 337 Browserfälle, sechs Pane-Tests, CSP und 135
Tooltests ebenfalls. Je Plattform wurden zwei OpenSSH-Gegenläufe ausdrücklich gestartet.
Die übrigen standardmäßig ignorierten Mount-, LAN-, Samba- und GUI-Spezialtests sind
damit nicht pauschal abgenommen. Die native Abnahme am endgültigen Test-App-Bundle
belegt beide Themes, echten F5 mit gemischter Auswahl, Zusammenführen mit erhaltenem
Altbestand, lange Quell-/Metadatenbilanzen, Tastatur, 800×600-Bestätigung, Abbruch nach
Teilwirkung, MKCOL-/PUT-/MOVE-Verlust, harten Prozessabbruch nach MKCOL, Neustart und
zwei parallel laufende Fensterqueues samt Navigation. Der Wire-Gegenlauf zählt 12 MKCOL,
17 PUT, 14 MOVE und 2 eigenes Datei-DELETE, keine Collection-Löschprobe und kein
Ordnercleanup. Quellen und fremde Sentinels blieben erhalten; eigenes Cleanup, Listener
und künstliche Schlüsselbundeinträge sind bestätigt.
