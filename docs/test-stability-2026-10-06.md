# Begrenzte Untersuchung sporadischer Linux-Testfehler

Stand: 2026-10-06. Untersucht werden die einmaligen Baseline-`flock`-/`EAGAIN`-
Fehler der bidirektionalen CLI sowie die Signaltest-Frist bei
`phase=cargo-listing, signal=SIGINT`. Eine nicht wieder aufgetretene Ursache
wird nicht als behoben behauptet.

## Versuch

Eine isolierte Linux-x86_64-Arbeitskopie verwendet Rust/Cargo 1.96.1. Nach der
Übersetzung laufen fünf Wiederholungen ohne zusätzliche Last und fünf mit vier
eigenen CPU-Lastprozessen. Jede Wiederholung umfasst:

- die gesamte CLI-Integrationssuite mit 16 Testthreads; beide betroffenen
  Resume-/Rollback-Tests werden vorab als genau ein vorhandener Test bestätigt;
- die vollständige Klasse `SftpE2eSignalTests`, einschließlich aller drei Signale
  in den Startfenstern, Prozessbesitz, Cleanup und Schutz fremder Prozesse.

Der Versuch besitzt eine Gesamtfrist von 900 Sekunden, je CLI-Lauf 120 Sekunden
und je Signaltestklasse 180 Sekunden. Die bestehenden internen Fristen bleiben
unverändert: 15 Sekunden bis zum Bereitschaftsmarker und acht Sekunden für den
normalen Signal-/Cleanup-Ausgang. Erst nach den vorhandenen Markern wird das
Signal gesendet. Alle CLI-Prozesse verwenden ihre isolierte `CliSandbox`.

Die Lastprozesse werden beim Abschluss beendet und geerntet. Es gibt keine
Verbindung zu echten SFTP-Accounts und keine Wirkung auf Nutzerdaten.

## Messergebnis

| Phase | Wiederholungen | Ergebnis | Laufdauer je Suite |
| --- | ---: | --- | --- |
| CLI ohne Zusatzlast, vorher | 5 × 42 | 210 bestanden | 0,415–0,666 s |
| CLI mit vier Lastprozessen, vorher | 5 × 42 | 209 bestanden, 1 EAGAIN-Fehler | 0,515–1,667 s |
| Signaltests ohne Zusatzlast | 5 × 25 | 125 bestanden | 54,770–55,666 s |
| Signaltests mit vier Lastprozessen | 5 × 25 | 125 bestanden | 56,849–60,218 s |
| CLI mit vier Lastprozessen, nachher | 30 × 42 | 1.260 bestanden | 0,465–1,316 s |

Der erste Versuch brach am roten CLI-Lauf nach 546,681 Sekunden ab. Vier
Signaltestklassen unter Last waren bis dahin grün; die fünfte folgte im
Gegenlauf. Der Gegenlauf dauerte 73,199 Sekunden bei einer Gesamtfrist von 300
Sekunden. Beide Versuche beendeten und ernteten jeweils ihre vier Lastprozesse.

## Belegte Ursache und Korrektur

Der Resume-Test scheiterte unmittelbar nach dem erfolgreichen ersten Core-Lauf
beim mutationslosen `finish_bidirectional_run_completion`: Der neue
`BaselineStore::open` erhielt `WouldBlock`. Ein parallel gestartetes Kind kann
zwischen `fork` und `exec` den geerbten Lock-FD halten. `close` des ursprünglichen
Besitzers allein beendet `flock` dann erst, wenn auch das Kind seinen FD schließt.

Der neue Test
`baseline_lock_wird_bei_geerbtem_fd_am_besitzerende_frei` hält genau diesen
Zustand über Pipes fest: Solange der ursprüngliche Store lebt, bleibt eine
zweite Öffnung gesperrt; nach seinem Ende muss sie trotz weiterlebendem Kind
möglich sein. Vor der Korrektur war er auf macOS und Linux rot, danach grün.
Der Fork läuft in einem eigenen Testprozess und beeinflusst die FDs anderer
Tests nicht. Das Kind führt nur signal-sichere Syscalls aus und wird geerntet.

`BaselineStore::drop` gibt den Lock nun ausdrücklich frei. Die beim Erwerb
gespeicherte Prozess-ID verhindert, dass ein geerbtes Objekt den noch aktiven
Elternbesitz freigibt. Ein weiterer Test prüft diesen Schutz mit einem
simulierten fremden Besitzer und einem tatsächlich weitergehaltenen Lock-FD.
Es gibt keinen Lock-Retry und keine größere Frist.

Die Signal-/Timing-Ursache wurde in den zehn vollständigen Klassenläufen nicht
reproduziert. Bereitschaftsmarker und Acht-Sekunden-Frist bleiben erhalten; das
Ergebnis ist eine begrenzte Nichtreproduktion, keine behauptete Behebung.

Vollständige Core-/CLI-Abschlussprüfung: macOS 807 Core-Tests und 62 CLI-Tests
bestanden; Linux 824 Core-Tests und 62 CLI-Tests bestanden. Je vier
Core-Tests bleiben ausdrücklich ignoriert. Core-/CLI-Clippy mit allen Targets
und `-D warnings` sowie Formatprüfung sind auf beiden Plattformen grün.

## Erneuter Auslöser

Bei einem erneuten roten Lauf die vollständige konkrete Testausgabe, Signalphase,
Signal, parallele Testzahl, Laufdauer, Toolchain und tatsächliche Last erhalten.
Für `EAGAIN` zusätzlich den Baseline-/Lockpfad und den Besitzer zum Fehlerzeitpunkt
binden. Eine größere Frist oder ein Lock-Retry folgt erst aus diesem Beleg.


## AP14: SFTP-Browser, FTPS-Erstbaseline und Fastra

Stand: 2026-10-18. AP13R meldete zwei rote SFTP-Browserfälle, zweimal eine
FTPS-Vorschaudrift unter Linux und einmal einen abweichenden Fastra-Fehlercode
unter macOS. Die damaligen isolierten Nachläufe waren grün. AP14 trennt diese
Befunde; ein bestandener Gegenlauf allein gilt nicht als Ursachenbehebung.

### Browser: beobachtete Anfragegrenzen

Der Agent-Test wartete nach dem Klick auf einen Editor, den die 80-ms-Antwort
bereits wieder schließen konnte. Der Listing-Test nahm an, der zweite Klick
komme vor der 120-ms-Antwort. Bei langsamer Browsersteuerung darf SFTP vorher
bereits korrekt verbinden; die spätere Navigation macht diesen früheren Connect
nicht rückwirkend falsch. Beide Testorakel waren deshalb von der Dauer der
Browsersteuerung abhängig.

Die Mockbrücke hält jetzt genau die angeforderte Agent- beziehungsweise
Listingantwort. Der Agent-Test beobachtet den offenen Editor und Revision 11,
bevor er die Profilentfernung mit der alten Antwort freigibt. Der Listing-Test
beobachtet Profil, Revision und Pfad der gestarteten Anfrage, navigiert sichtbar
zu WebDAV und gibt erst danach die alte SFTP-Antwort frei. Der Test wartet auf
deren Rückgabe und verlangt weiterhin, dass kein SFTP-Connect folgt.

Beide Gegenfälle bestehen gegen die Anwendung. In privaten Quellkopien werden
sie rot, wenn jeweils die Profil- oder Nachantwort-Sequenzprüfung entfernt wird.
Die Produktschranken bleiben unverändert; die Korrektur erhöht keine Frist und
entfernt keinen Gegenfall.

### FTPS: eigene Proben veränderten den leeren Plan

Der unveränderte Einzeltest, ein verkürzter Erstbaseline-Gegenlauf und zunächst
auch ein instrumentierter Core-Gesamtlauf bestanden. Der nächste uninstrumentierte
Gesamtlauf reproduzierte die Drift. Die neue Testdiagnose zeigte einen leeren
Plan und danach eigene `.doppeldecker-delete-probe-*`-/`.doppeldecker-probe-*`-
Ordner samt Dateien im FTPS-Root. Lokale Wurzel, Serveridentität, Sitzungsgeneration
und Fähigkeiten blieben gleich. Die Driftabsage war richtig; zusätzliche eigene
Proben hatten den zuvor leeren Baum verändert.

Die bidirektionale Vorbereitung fragte bisher Schreibfähigkeiten beider Seiten
ab, auch für eine leere Erstbaseline oder eine reine Quelle. Fehler der
Probe konnten dabei erhaltene Recoveryobjekte hinterlassen. AP14 ermittelt
Schreibfähigkeiten jetzt ausschließlich für Seiten mit tatsächlichen Zielwirkungen:
Copy, Delete und TypeSwap nach Richtung, ConflictCopy nach Zielseite. Dasselbe gilt
für Resume und Rollback. Die Fähigkeiten einer unveränderten Seite bleiben falsch;
keine positive Zielgarantie wird erfunden. Alle bisherigen Zielprüfungen, Hashes,
Listings, Journalbindungen und Driftabsagen bleiben bestehen. Es gibt keinen
neuen Snapshot nach einer eigenen Mutation und keinen Retry.

Der Regressionstest
`ftps_baseline_und_quelle_bleiben_ohne_mutierende_proben` verliert absichtlich die
Antwort nach MKD. Gegen den alten Core wird er bereits wegen dieses unnötigen
MKD rot. Nach der Korrektur legt er die leere Baseline an und kopiert anschließend
eine echte Remote-Datei ins lokale Ziel, ohne einen mutierenden FTP-Befehl zu senden.
Der künstliche Quellbestand bleibt exakt erhalten. Der große FTPS-Gegenlauf erhält
bei einer erneuten Drift außerdem Plan-, Capability- und frische Snapshotbelege;
im erfolgreichen Weg startet die Diagnose keine zusätzlichen Providerabfragen.

Ein unabhängiger Rest bleibt: Im reproduzierten Fehler enthielten einzelne eigene
STOU-Probedateien null Bytes. Warum die Datenübertragung beziehungsweise ihr
Readback so ausging, ist noch nicht belegt. Inhaltsprüfung und konservativer
Cleanup erhielten diese unklaren Objekte. AP14 ersetzt das nicht durch blindes
Löschen oder eine Schreibfreigabe. Benötigte FTPS-Schreibproben und ihr
Transport-/Cleanup-Vertrag benötigen einen eigenen begrenzten Gegenlauf.

AP15 hat diesen Gegenlauf inzwischen ausgeführt: Der vorzeitige Datenkanal-Drop
mit ungelesenen TLS-Sessiontickets erzeugt unter Linux einen Reset. An einer
kontrollierten Lesegrenze erhält auch die gesperrte `libunftp`-Fixture dadurch eine
leere Datei trotz erfolgreichem Client-Flush. Derselbe Gegenlauf liefert mit
geprüftem TLS-/TCP-Schreibende und gehaltenem Lesekanal den vollständigen Inhalt.
[Ursache, Gegenbelege und verbleibender Intentvertrag](ftps-stou-transport.md).

### Fastra: begrenzte Nichtreproduktion

Der unveränderte Fastra-Einzeltest und die vollständige parallele macOS-App-Suite
bestehen. Auch die zehn Browser-Command-Tests unter vier eigenen CPU-Lastprozessen
bestehen. Ein gezielt geerbter offener Skript-Schreibdescriptor verändert auf
macOS den Exitcode 4 nicht. Damit ist weder Schedulerlast noch dieser
Descriptorfall als Ursache belegt. Die Fristen bleiben unverändert.

Der Fastra-Exitcodetest prüft jetzt den tatsächlichen Kindstatus vor seiner
Zuordnung zum App-Fehlercode und unterscheidet dabei Prozessfehler und Fristablauf.
Bei erneutem Fehlschlag bleiben erwarteter Exitcode, beobachteter Status,
Laufdauer und stdout sichtbar. Pipe-EOF, große Ausgabe und Timeout bleiben durch
die vorhandenen eigenen Gegenfälle geprüft.

Die historische Fastra-Ursache bleibt offen. Die erhaltenen STOU-Reste bleiben
Referenzen und werden nicht anhand ihres Namens bereinigt. AP15 belegt die
Transportursache; AP16 bindet neue Proben-/STOU-Intents dauerhaft vor der Wirkung.
Ein aktueller grüner Gesamtlauf ersetzt keine Prüfung der historischen Reste. Erneute Fehler brauchen die konkreten
Vorher-/Nachherbelege beziehungsweise den tatsächlichen Prozessstatus; kein
Lock-/Mutationsretry, keine schwächere Driftprüfung und keine pauschale
Fristverlängerung folgt aus dieser Nichtreproduktion.

AP14 korrigiert die Provider-Vorbereitung und ihre Gegenbelege. Es verändert
keine Ansicht, Fensterqueue oder native Schnittstelle; die geänderte Grenze wird
mit denselben Core-Plänen und einem echten Loopback-FTP-/FTPS-Gegenlauf geprüft.
Eine erneute native GUI-Abnahme wurde dafür nicht ausgeführt. Die native AP13R-Abnahme bleibt auf deren damaligen Build begrenzt.
Acht offene CodeQA-Ganzbereiche, Querschnitt und die menschliche interaktive
Linux-Kandidatenabnahme bleiben eigenständige Hürden.


### Abschließende Gateprüfung

macOS: 1054 Core-, 68 CLI- und 203 App-Tests bestanden. Linux: 1073 Core-,
68 CLI- und 193 App-Tests bestanden. Beide vollständigen Core-/App-Läufe verwenden
die normale parallele Ausführung und schließen den bisherigen FTPS-Gegenfall ein.
Je Plattform bleiben 21 opt-in Core-Spezialtests ignoriert, unter Linux zusätzlich
zwei native GTK-Clipboard-Tests. Die Grenzen betreffen insbesondere echte Volumes,
externe SFTP-/SMB-Server und native Plattformressourcen; sie sind keine grüne
Abnahme dieser Spezialumgebungen.

Formatierung, Workspace-Clippy mit allen Targets und `-D warnings` sowie der
Fuzz-Check mit `--locked --all-targets` bestehen beidseitig. 339 Browserfälle,
CSP, sechs Pane- und 135 Tooltests bestehen. Die Frontend-/Toolquellen dieses
Gesamtlaufs sind mit dem endgültigen Implementierungsstand byteidentisch; nach
dem während der Untersuchung gefundenen FTPS-Fehler wurden alle betroffenen
Rust-Gates auf beiden Plattformen mit frischen Quellzeitstempeln und geprüftem
SHA-256-Manifest erneut ausgeführt. Rote Vorläufe und isolierte Gegenläufe bleiben
getrennt erhalten. Die offenen STOU-/Fastra-Befunde werden dadurch nicht als
behoben oder vollständig nativ geprüft bezeichnet.

### AP16: dauerhafte FTPS-Belege

AP16 begrenzt Proben auf die benötigte Wirkung und bindet ihre Intents vor dem
ersten mutierenden Befehl an die Fensterqueue. Die passenden Vorher-/Nachherfälle
belegen passive Zielabsage, fehlenden Owner, Queueordnerwechsel, private neue
Konfigurationsordner, reine Leseseiten und parallele beziehungsweise widersprochene
Ausführungsklassen. Die vollständigen macOS-/Linux-Gates sowie die native Prüfung
beider Absturzgrenzen sind im [FTPS-Vertrag](ftps-stou-transport.md#ap16-verifikation-und-native-grenzen)
mit ihren Grenzen bilanziert. Das ist ein Implementierungsdelta; es erledigt keine
der offenen Vollbereichsreviews und keine historische Fastra-Ursache.

## Fastra-Testhelfer: Start ohne neue ausführbare Skripte

Die zwei in AP22 auch einzeln fehlgeschlagenen Fähigkeiten-/Exitcodefälle
benutzten jeweils ein frisch geschriebenes und ausführbar gesetztes Shellskript.
Ein erneuter unveränderter Einzel- und App-Teilprüflauf reproduziert die damaligen
Fristen zunächst nicht. macOS protokolliert im damaligen Fehlerzeitfenster jedoch
noch XProtect-Ausführungsprüfungen für bereits entfernte `fastra-diff`-Fixtures.

Der unabhängige Gegenlauf startet 24 neue triviale Exit-0-Skripte gleichzeitig.
Zwei erreichen innerhalb von zwei Sekunden nicht einmal ihre erste Anweisung;
der eigene Kindprozess wird anschließend beendet und eingesammelt. Dieselben
24 Fälle mit festem `/bin/sh` als Programm und dem Skript als Eingabe bestehen
alle, maximal nach 29 ms. Ein vorheriger serieller Vergleich mit 30 Fällen je
Variante zeigt einen Median von 93 ms beim direkten Skriptstart und 4 ms beim
festen Interpreter. Dieser Gegenlauf reproduziert den Startstillstand der
künstlichen Helfer, nicht erneut den genauen historischen Zwölfsekundenablauf.
Die macOS-Prüflogs stützen die Zuordnung zur Ausführungsprüfung; sie sind kein
Nachweis eines Fehlers im installierten Fastra-Helfer.

Die Testfixtures verwenden jetzt `/bin/sh -c` mit festem künstlichem Programmtext
und getrennten Argumenten. Sie erzeugen keine neuen ausführbaren Dateien und
brauchen den bisherigen Linux-ETXTBSY-Mutex nicht mehr. Der produktive
Bundle-Helfer bleibt direkt gestartet. Die gemeinsame Fähigkeitenauswertung,
Pipeentleerung, echte Kindstatusprüfung und Fristen bleiben erhalten; auch ein
hängender Nachkomme darf weiterhin nicht zum Warten auf Pipe-EOF führen.
Unicode, führende Bindestriche und Shellsonderzeichen bleiben getrennte Daten.

Die fünf Fastra-Fälle und der macOS-App-Teilprüflauf mit 194 Tests bestehen am
korrigierten Stand. Dieser Teilprüflauf enthält beide früher ausgefilterten
Fastra-Fälle. Er filtert die zehn Tests der Module `cli_setup::tests` und
`file_clipboard::native` aus: Für diese nativen Setup-/AppKit-Fälle lag in der
Prüfumgebung keine Ausführungsfreigabe vor.
Die grüne Teilprüfung ersetzt deshalb keine vollständige App-Gesamtprüfung.
Formatprüfung und App-Clippy mit allen Targets und `-D warnings` bestehen.

Sechs zusätzliche Wiederholungen bestehen jeweils alle fünf Fastra-Fälle.
Die vollständige Linux-App-Suite besteht mit 194 Tests und zwei separat ignorierten
GTK-Spezialfällen. Beide Plattformen verwenden dieselben 383 per SHA-256
gebundenen Code-/Werkzeugdateien. Ein erster Linux-Aufbau scheiterte ausschließlich
an übertragenen AppleDouble-Dateien; der korrigierte Archivtransfer überträgt
keine macOS-Dateimetadaten. Der echte installierte Fastra-Bundle-Helfer bestätigt
weiterhin direkt Protokoll 1 und alle fünf benötigten Fähigkeiten. Diese Abfrage
öffnet gemäß Vertrag kein Fenster; die GUI-Integration wurde hier nicht erneut
abgenommen.

Am 2026-10-19 besteht nach gesonderter Ausführungsfreigabe auch die vollständige
isolierte macOS-App-Suite mit 204 Tests am gleichen Helferstand. Die zehn zuvor
ausgenommenen Setup-/AppKit-Fälle laufen dabei mit temporären Setupzielen und
privaten Pasteboards. Der frühere Teilgate-Beleg bleibt historisch erhalten.
