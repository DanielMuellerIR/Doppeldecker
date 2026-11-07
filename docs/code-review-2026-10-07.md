# Vollreview ab 2026-10-07

Der vollständige Review ist in Arbeit. Das aktuelle Inventar umfasst 193 eigene Dateien
in 26 Bereichen; unveränderte eigene Implementierungen und Tests gehören dazu.
Abgeschlossene Teilprüfungen ersetzen keinen Abschluss der übrigen Bereiche.
Sieben vollständige Bereiche sind abgeschlossen; neunzehn bleiben offen. Der genaue
Fortschritt steht in `.codeqa/full-review-2026-10-07.json`.

## Papierkorb: vollständige Teilprüfung

Alle elf Dateien unter `core/src/trash.rs` und `core/src/trash/` frisch vollständig
gelesen, einschließlich Tests: Journalcodec, private Anlage, Claim, Info-Publish,
Transfer, Rücksetzung, Recovery, Cleanup, Legacy sowie macOS- und XDG-Adapter.
Direkte Tauri-/CLI-Aufrufer und die Fehlerbilanz der Oberfläche sind einbezogen.

Geprüfte Grenzen: vollständiger Intent vor Wirkung, gehaltene Identitäten und
Mounts, private Rechte einschließlich geerbter ACLs, exklusiver Rename,
Quelltausch und späte Kollision, beobachteter Erfolg trotz Durability-Fehler,
konservative Transfer-Recovery, fremde Reste und nichtrekursiver Cleanup.
In dieser ersten Teilprüfung wurde kein neuer Fehler im Papierkorb-Kern bestätigt.
Die später bestätigte gemischte Claim-Recovery ist im Nachtrag unten korrigiert.

Ein Fehler im CLI-Aufrufer ist behoben: Zwei neue `io::Error`-Hüllen hatten
maschinenlesbare Core-Fehlercodes und strukturierte Wiederherstellungshinweise
verworfen. Die unveränderte Fehlerweitergabe erhält diese Informationen.
Eine neue Prozessregression begrenzt nur die Dateigröße des isolierten Kindes:
Der tatsächliche Journalwrite scheitert vor dem Claim, der Fehler bleibt
`outcome_unclear`, und die Quelle bleibt bytegleich. Vor der Korrektur lieferte
dieser Test `io` und scheiterte; danach besteht er auf beiden Plattformen.

Prüfung: Papierkorb-Modultests macOS 82, Linux 84 bestanden, jeweils vier bewusst
ignorierte Mount-/Fremd-UID-Spezialtests. Vollständige CLI jeweils 63 bestanden;
CLI-Clippy mit allen Targets und `-D warnings`, Format und Diffprüfung bestanden.
Die elf Core-Dateien der Linux-Testkopie sind bytegleich mit dem geprüften Stand.
Alle Testroots sind künstlich; kein echter Systempapierkorb wurde verwendet.
Die vorhandenen nativen Mount- und GUI-Belege wurden bei dieser unveränderten
Core-Implementierung nicht erneut ausgeführt. Der Queue-Teil ist unten gesondert dokumentiert.

## Quarantäne: vollständige Teilprüfung

`core/src/quarantine.rs` einschließlich sämtlicher Tests vollständig frisch gelesen.
Die direkten Sync-, bidirektionalen, Queue-, CLI- und Tauri-Grenzen sowie die
Quarantäneansicht sind einbezogen. Schwerpunkte: persistierter Claim-Digest samt
Schema-Kompatibilität, provider-/rootgebundene Lease, offene Restore-Intents,
Identitätswechsel, Purge-Statfehler und gebundener Leerheitsbeleg mit Tombstone.

Eine Durability-Lücke nach unterbrochenem Restore ist behoben: Der Rename konnte
bereits sichtbar sein, während die Verzeichnis-fsyncs noch ausstanden. Die Recovery
hatte den Manifestabschluss allein nach Identitäts-/Inhaltsprüfung nachgeholt.
Jetzt sichert sie erst die gesamte Ziel-Elternkette von innen nach außen sowie die
vorhandenen Quarantäne-Eltern bis zur Basis. Ein fehlender Quarantäne-Elternname
darf übersprungen werden, andere Fehler bleiben erhalten. Erst danach folgen die
frischen Zielprüfungen und der Manifestabschluss; es erfolgt kein erneuter Rename.

Eine Regression erzeugt den echten Zwischenstand mit durablem Intent und Rename.
Sechs einzeln verweigerte Verzeichnis-Syncs müssen `outcome_unclear` liefern und
den Manifeststatus offen lassen. Ein anschließender fehlerfreier Wiederanlauf
schließt denselben Eintrag ab. Vor der Korrektur meldete der erste Fall fälschlich
`reconciled`; nachher bestehen alle Fälle. Die Test-VFS leitet auch die echten
Metadaten weiter, damit kein früherer Digestfehler den Sync-Gegenbeleg verdeckt.

Prüfung: vollständiger Core macOS 868 und Linux 887 bestanden, jeweils zehn
ausdrücklich ignorierte Spezialtests; darunter 48 Quarantäne-Modultests je Plattform.
Core-Clippy mit allen Targets und `-D warnings`, Format und Diff bestanden.
Die Linux-Kopie verwendet dieselbe Quarantäne-Quelldatei. Ein Stromausfall wurde
nicht herbeigeführt; geprüft sind der persistierte Zwischenstand, die Fehlerstellen
und der nachfolgende Abschluss. Der vollständige Queue-Review folgt unten.

## Queue: vollständige Teilprüfung

`core/src/queue.rs` einschließlich sämtlicher Tests und des gespeicherten
Property-Test-Gegenbeispiels vollständig frisch gelesen. Zustandsmodell,
Migrationen, Journal-/Prozesslock, alle Operationsklassen, Scheduler, Executor,
Abschluss und Recovery sowie direkte Tauri-/CLI-Grenzen sind einbezogen.

Vier bestätigte Fehler sind behoben:

- Ein sichtbarer neuer Journalstand galt nach Schreibfehler schon als erfolgreicher
  Commit. Jetzt werden Datei und Elternverzeichnis erneut synchronisiert und der
  gebundene Stand nochmals geprüft. Scheitert dies, bleibt die Queue bis zum
  Wiederöffnen gesperrt; ein möglicherweise persistiertes `Running` wird bei
  Recovery unterbrochen und nie automatisch erneut ausgeführt.
- Pending-Einträge konnten beim Umordnen einen offenen Recovery-Eintrag überholen.
  Beide Richtungen halten jetzt vor `Interrupted` oder `AwaitingConfirmation` an;
  abgeschlossene Historie darf weiterhin übersprungen werden.
- WebDAV-Altplanmigrationen konnten trotz eines früheren Ausführungsversuchs einen
  harmlosen Replan zulassen. Vorherige Wirkungshinweise erzwingen jetzt manuelle
  Prüfung mit `outcome_unclear`. Bereits falsch migrierte Journale werden ebenfalls
  repariert, ohne Fortschritt oder Wiederherstellungshinweise zu verlieren.
- Quarantäne-Eltern wurden grundsätzlich ohne Beachtung der Groß-/Kleinschreibung
  gesucht. Auf case-sensitivem Linux konnte dadurch ein anderer Name gebunden
  werden. Exakte Treffer haben jetzt Vorrang; ein Case-Alias ist erst nach einer
  tatsächlichen `AlreadyExists`-Anlageprobe zulässig. Der Gegenbeleg umfasst sowohl
  verschachtelte Parkordner als auch die Quarantänebasis.

Alle vier Regressionen wurden vor der jeweiligen Korrektur rot und danach grün
beobachtet; der Case-Gegenbeleg war auf Linux rot und auf case-insensitivem APFS
bereits grün. Das unabhängige Queue-Property-Modell berücksichtigt jetzt ebenfalls
offene Recovery-Barrieren.

Die vier protokollspezifischen Queue-Vorbereitungen verwenden einen gemeinsamen
Rahmen für Verbindungsfehler, Planvergleich und Bestätigung. Die Validierung bleibt
vor jedem Providerzugriff. Eine zusätzliche Regression unterscheidet fehlende
Zugangsdaten beim Verbinden von einem gleichartigen Namespacefehler während der
Vorbereitung. Weitere bereits erfasste Executor-Duplikate bleiben offen.

Prüfung des endgültigen Stands: vollständiger Core macOS 873 und Linux 892 bestanden,
jeweils zehn ausdrücklich ignorierte Spezialtests; CLI jeweils 63, Tauri macOS 172.
Workspace-Clippy macOS und Core-/CLI-Clippy Linux mit allen Targets und
`-D warnings`, Fuzz-Workspace mit `--locked`, Format und Diff bestanden. Die
Queue-Quelldateien beider Plattformen sind SHA-256-bytegleich. Kein Stromausfall
wurde herbeigeführt; die Durability-Prüfung verwendet den tatsächlichen sichtbaren
Journal-Zwischenstand und einen gezielt verweigerten Sync. Unveränderte GUI-Ansichten
wurden für diese Core-Korrekturen nicht erneut nativ geprüft.

Der entsprechende Readback-ohne-Durability-Pfad im bidirektionalen Journal ist
unten als gezielter Folgefix dokumentiert. Der übrige Vollreview steht aus.

## Bidirektionales Journal: gezielter Folgefehler

Der vollständige bidirektionale Bereich bleibt offen. Gezielt geprüft wurden
Baseline-/Laufjournal-Speicher, späte Writerfehler, Entfernung und die direkten
Projektions-, Typwechsel- und Baselineabschluss-Caller.

Auch `update_run_journal_using` akzeptierte nach einem späten Writerfehler allein
den sichtbaren neuen Stand. Jetzt liest es das erwartete Journal aus einer
unter gehaltenem Elternordner geöffneten regulären Datei, synchronisiert Datei und
Elternverzeichnis und prüft anschließend den Stand erneut. Ein Fehler dieser
Nachsicherung bleibt `outcome_unclear` und beendet die Ausführung vor der nächsten
Projektion. Das Journal bleibt für die gebundene Recovery erhalten.

Die Regression erzeugt einen tatsächlichen Rename ohne nachfolgenden Sync und
verweigert die Nachsicherung. Vorher meldete die Funktion fälschlich Erfolg;
nachher erhält sie den unklaren Zustand, lässt Ziel und Baseline unverändert und
sperrt den nächsten Normalplan auch nach Wiederöffnen. Der vorhandene positive
Gegenfall verwendet den echten Datei-/Parent-Sync und schließt weiterhin ab.
Alle 102 bidirektionalen Modultests bestehen auf macOS.

Der erste vollständige Linux-Lauf scheiterte in einem anderen Queue-Test beim
nächsten eigenen Prozesslock-Erwerb mit `WouldBlock`; der exakte Einzelgegenlauf
bestand. Dort fehlte ein expliziter Besitzer-Unlock, während ein geerbter FD den
alten Lock noch halten konnte. Dieser Folgepunkt ist unten mit einem deterministischen
FD-Gegenbeleg geprüft; ein Lock-Retry wurde nicht ergänzt.

Abschlussprüfung dieses gezielten Fixes: Core macOS 874 und Linux im vollständigen
Gegenlauf 893 bestanden, jeweils zehn ignoriert; CLI jeweils 63, Tauri macOS 172.
Clippy mit allen Targets und `-D warnings` beidseitig, Fuzz `--locked`, Format,
Diff und Quellhashvergleich bestanden. Der erste rote Linux-Lauf bleibt als eigener
Queue-Lock-Folgebeleg erhalten. Kein Stromausfall wurde herbeigeführt.

## Queue-Prozesslock: gezielter Folgefehler

Ein Linux-Vollauf zeigte einen sporadischen `WouldBlock` beim nächsten eigenen
Queue-Erwerb. Der deterministische Gegenbeleg hält dieselbe offene
Dateibeschreibung über einen duplizierten FD weiter, wie es auch zwischen `fork`
und `exec` geschieht. Zwei Fensterqueues teilen den Besitzer: Nach Ende der ersten
muss der Lock weiterhin sperren, nach Ende der letzten muss er trotz des gehaltenen
FD frei sein. Vor der Korrektur scheiterte genau dieser letzte Schritt.

`OperationQueueProcessLock::drop` gibt den Lock jetzt ausdrücklich frei, sofern
das Objekt noch im erwerbenden Prozess lebt. Die gemeinsame `Arc`-Lebensdauer
schützt alle übrigen Fensterqueues. Ein zweiter Gegenfall simuliert die geerbte
Besitzer-PID und beweist, dass deren Ende den aktiven Elternlock nicht löst.
Beide Fälle und alle 132 Queue-Modultests bestehen auf macOS. Es wurde kein
Lock-Retry und keine größere Testfrist eingeführt.

Vollständige Abschlussprüfung: Core macOS 876 und Linux 895 bestanden, jeweils
zehn ignoriert; CLI jeweils 63, Tauri macOS 172. Workspace-Clippy macOS und
Core-/CLI-Clippy Linux, Format, Diff und Queue-Quellhashvergleich bestanden.
Die FD-Lebensdauer wird deterministisch ohne zusätzlichen echten Fork geprüft.

## Metadaten und ACL: vollständige Teilprüfung

Alle sechs eigenen Dateien des Bereichs `core-metadata-acl` einschließlich
ACL-/Manifesttests, Capabilityermittlung, Mounttests, Fuzz-Target und Seed wurden
vollständig frisch gelesen. Direkte lokale VFS-, Planungs- und
Übertragungsgrenzen wurden zusätzlich geprüft. Dies ersetzt keinen vollständigen
Review des übrigen Sync-Bereichs.

Ein bestätigter Fehler an der Planungsgrenze ist behoben: Die Erfassung von
Quell-Hardlinks hing von der Hardlink-Unterstützung des Ziels ab. Dadurch fehlte
bei `NativeBestEffort` die Verlustbilanz; auf Linux wurde derselbe Verlust bei
`NativeStrict` sogar als `strict_verified` akzeptiert. Der Gegenbeleg war vor der
Korrektur auf beiden Plattformen rot. Quellbeziehungen werden jetzt unabhängig
von der Übertragbarkeit erfasst. Best Effort bilanziert beide Namen und kopiert
ihren Inhalt in unabhängige Dateien; Strict lehnt den Verlust vor Mutation ab.
Die manifestgebundene Ausführung berücksichtigt auch ausgelassene Gruppen.

Ein weiterer roter Gegenbeleg prüft die Auswahlgrenze: Erst zwei tatsächlich
gewählte Namen bilden eine zu erhaltende Beziehung. Ein externer oder
explizit ausgeschlossener Partner erzeugt keine falsche Hardlink-Auslassung und
keine unnötige strikte Absage. Außerhalb der Auswahl wird nicht gesucht.

Abschlussprüfung des endgültigen Stands: Core macOS 878 und Linux 897 bestanden,
jeweils zehn ignoriert; davon Sync-Integration 74 beziehungsweise 79. CLI jeweils
63, Tauri macOS 172, Clippy mit allen Targets und `-D warnings` beidseitig,
Fuzz-Workspace mit `--locked`, Format und Diff bestanden. Zusätzlich bestanden
Metadaten-Modultests macOS 12/Linux 13, ACL jeweils zwei, Capability jeweils eins
sowie die tatsächliche Fuzz-Seed-Validierung auf beiden Plattformen. Die sechs
Bereichsdateien und drei geänderten Sync-Dateien sind SHA-256-bytegleich.
Die unveränderten nativen Mount-/GUI-Spezialprüfungen wurden nicht erneut ausgeführt.

Die dabei gefundene pauschale Absage von `NativeBestEffort` ohne schreibbare
native Zielklassen ist im folgenden gezielten Schnitt geprüft und korrigiert.

## NativeBestEffort ohne schreibbare native Zielklasse

Gezielt geprüft wurden Policy, Manifestaufnahme, Verlustbilanz und die direkten
Transfer-/Rechtepfade. Der reproduzierte Gegenbeleg verweigerte vorher jeden Lauf
mit „Kein exakt schreibbares natives Metadatenmerkmal verfügbar“. Jetzt bleibt
auch ein vollständig ausgelassener nativer Transfer manifestgebunden und wird
pfadgenau bilanziert. Die Vorschau verspricht dabei null native
Metadatenänderungen. Quelle und bestehendes Ziel werden weiterhin gegen ihre
Manifeste geprüft; fehlende Provider-Manifeste werden nicht erfunden.

Der allgemeine Rechtefallback darf ausgelassene native Modusbits weder während
der Kopie noch durch eine nachgeschobene `SetMode`-Aktion setzen. Die Regression
kopiert und aktualisiert reale Testdateien einschließlich Ordner und
Hardlinknamen; ein ausschließlich veränderter Quellmodus erzeugt keine falsche
Rechteaktion. Die vorhandene Oberfläche zeigt auch null übertragene native
Merkmale und die vollständigen Auslassungen und bestätigt dieses Profil weiterhin.

Lokale Manifeste lesen die Fähigkeiten am gehaltenen Dateideskriptor. Nur eine
explizit nicht unterstützte Klasse wird ausgelassen; unbekannte Fähigkeiten und
Lesefehler werden nicht zu „nicht vorhanden“. Die Mountidentität wird vor und nach
dem Scan gebunden. Der reale exFAT-Gegenlauf prüft Manifest, Zeitstempel sowie
Datei-/Ordnersync. Unter Linux bestehen zudem Kopie und Update mit exaktem
Inhaltsreadback. APFS-/ext4-Metadatenübertragung und beide Read-only-Gegenläufe
wurden erneut tatsächlich ausgeführt.

Ein separater Fehler bleibt offen: Auf macOS-exFAT liefert der exklusive Rename
`ENOTSUP`, während der bisherige lokale Commitweg diese Garantie voraussetzt.
Der ursprüngliche Inhaltsgegenlauf scheiterte deshalb trotz korrigierter
Metadatenprüfung; eine isolierte Rename-Probe bestätigt die Ursache. Die
Mountprüfung weist den dadurch nicht ausgeführten macOS-exFAT-Inhaltslauf
explizit aus. Sie ersetzt keinen erfolgreichen Transferbeleg. Dieser neue
VFS-/Commit-Folgepunkt ist gesondert erfasst.

Abschlussgates: Core macOS 879/Linux 898 bestanden, jeweils zehn ignoriert; CLI jeweils 63, Tauri macOS 172. Clippy mit allen Targets und `-D warnings` beidseitig, Fuzz-Workspace `--locked`, Format und Diff bestanden. Fünf geänderte Quelldateien sind zwischen den
Plattformen SHA-256-bytegleich. Der übrige vollständige Sync-/VFS-Review bleibt offen.

## VFS, lokales Dateisystem und Pfade: vollständige Teilprüfung

Die drei Dateien einschließlich unveränderter Tests wurden vollständig frisch
geprüft. Direkte Dateioperations-, Quarantäne-, Sync- und Staginggrenzen wurden
zusätzlich gelesen. Die vollständigen Dateioperations- und Stagingteile bleiben
getrennt nachzuweisen.

Ein bestätigter Fehler konnte lokale Worker unbegrenzt blockieren: Nach einem
Scan ließ sich eine reguläre Datei durch einen FIFO ersetzen. `O_NOFOLLOW`
verhindert diesen Typwechsel nicht. Der Gegenbeleg scheiterte auf beiden
Plattformen in zehn Zugriffsfällen, außerdem bei Restore, Purge und der
Leerheitsprüfung mit einem eingewechselten FIFO-Intent. Die Tests lösen den
alten blockierenden Zugriff über einen eigenen Peer wieder auf und beenden alle
Threads kontrolliert.

Die gemeinsame lokale Öffnungsgrenze verwendet jetzt `O_NONBLOCK` und
`O_NOCTTY` und prüft den tatsächlichen Typ am gehaltenen Deskriptor. Nur reguläre
Dateien und Ordner gelangen zu weiteren Wirkungen. Die drei gebundenen
Quarantäne-Intentöffnungen sind ebenfalls nicht blockierend; deren vorhandene
Identitäts- und Typprüfung bleibt verbindlich. Ein fehlgeschlagener Typnachweis
nach möglicher Anlage oder Kürzung erhält `outcome_unclear` und den
strukturierten Prüfpfad. Veraltete VFS-Kommentare und die Beschreibung der
relativen Pfadnormalisierung wurden berichtigt.

Abschlussprüfung: Core macOS 881/Linux 900 bestanden, jeweils zehn ignoriert;
CLI jeweils 63, Tauri macOS 172. Workspace-Clippy macOS und Core-/CLI-Clippy
Linux mit allen Targets und `-D warnings`, Fuzz-Workspace `--locked`, Format,
Diff und SHA-256-Vergleich der drei Dateien bestanden. Echte APFS-/ext4- und
Read-only-Mounttests bestanden erneut; Linux-exFAT kopiert und aktualisiert
Inhalte. Auf macOS-exFAT wurden Manifest und Vorschau geprüft; der bereits
separat erfasste `ENOTSUP` beim exklusiven Rename verhindert weiterhin einen
Inhaltstransferbeleg. Eine native GUI-Änderung war nicht erforderlich.

## Privates Staging und atomare Hilfsdateien: vollständige Teilprüfung

`local_staging.rs`, `atomic_file.rs` und `atomic_write.rs` wurden vollständig
frisch einschließlich Tests geprüft. Die direkten VFS-, Quarantäne- und
Dateioperationsgrenzen wurden einbezogen. Private Anker, Vererbung, Lease,
Journalphasen, Fensterbesitz und Recovery wurden gegen ihre Aufrufer geprüft.

Ein bestätigter Fehler verlor Wiederherstellungspfade beim erneuten Einhüllen
von Zustandsdateifehlern. Die Namen erschienen nur im deutschen Text und fehlten
in der maschinenlesbaren Bilanz. Ein Gegenbeleg mit echten Temp-Dateien und
injizierten Fehlern nach Anlage, während der Vorbereitung, nach ausgeführtem
Rename und beim Elternsync scheiterte in allen vier Fällen auf beiden
Plattformen. Die gemeinsamen Fehlerhüllen erhalten jetzt die bisherigen
strukturierten Pfade und ergänzen die möglichen Namen. Auch Fehler einer
identitätsgebundenen Bereinigung verlieren ihre bisherigen Pfade nicht.

Ein lokaler Adapter kann einen Rename bereits ausgeführt haben und erst bei
seiner Journalbestätigung scheitern. Ein solches `outcome_unclear` wird jetzt
auch lokal als unklarer Commit weitergegeben; der Tempname wird nicht durch den
Cleanupweg für einen unverändert abgelehnten Rename behandelt. Die Regression
prüft den tatsächlichen Zielinhalt und den erhaltenen früheren Datenrest.
Der vorhandene Gegenfall für einen fremd wiederbelegten Tempnamen besteht
weiterhin. Der Kommentar zum 255-Byte-Namensbudget wurde präzisiert.

Abschlussprüfung: Core macOS 882/Linux 901 bestanden, jeweils zehn ignoriert;
CLI jeweils 63, Tauri macOS 172. Clippy mit allen Targets und `-D warnings`
beidseitig, Fuzz-Workspace `--locked`, Format, Diff und SHA-256-Vergleich
der drei Bereichsdateien bestanden. Keine GUI- oder Mountwirkung wurde geändert;
die zuvor ausgeführten nativen Mountbelege bleiben für diesen Stand unverändert.

## Dateioperationen: vollständige Produktionspfade

Der gesamte Präfix von `fileops.rs` vor dem großen Testmodul wurde frisch gelesen,
einschließlich der dortigen Testzugänge. Geprüft wurden Konfliktbindung, Auswahl-
und Sandboxgrenzen, Copy, Move samt Quellabbau, Rename, Ordneranlage, Parken,
Rückweg sowie Commit-/Readback-/Cleanup-Phasen. Queue-, GUI- und CLI-Aufrufer und
die geänderten Regressionen wurden zusätzlich geprüft. Der vollständige große
Testbestand ist weiterhin ein eigener offener Teil.

Ein reproduzierter Fehler verlor frühere strukturierte Wiederherstellungspfade
beim Fehlerabschluss einer Dateiübertragung. Der Gegenbeleg prüft einen
unbelegten Tempnamen und eine wegen Identitätswechsels abgelehnte Bereinigung.
Beide Varianten verloren den früheren Datenrest auf beiden Plattformen. Quelle,
früherer Datenrest und fremder Tempnamensbestand bleiben im echten Test unverändert.
Die drei Fehlerabschlüsse für Transfer, Vorbereitung und mutationsfreien
Commitfehler verwenden jetzt denselben Helfer und erhalten die ursprünglichen
Prüfpfade auch dann, wenn die Bereinigung selbst scheitert.

Ein zweiter roter Gegenbeleg zeigt fehlende Ziel-/Sicherungsnamen nach unklarer
Ordneranlage beziehungsweise nach bereits wirkendem Park-Rename. Diese Namen
werden jetzt strukturiert weitergeführt; unklare Tempanlage und die lokalen
Bereinigungsfehler erhalten ebenfalls die Pfade ihrer Ursachen. Die gemeinsame
Pfadübernahme liegt im Fehlermodul und wird von Dateioperationen und atomaren
Zustandsdateien verwendet. `error.rs` wurde dafür vollständig gelesen; das ersetzt
keinen vollständigen Review seines größeren Remote-Gesamtbereichs.
Die veralteten Modulhinweise zu Papierkorb-Crate und Remote-Zukunft wurden
berichtigt. Der separat bestätigte macOS-exFAT-Commitfehler bleibt offen.

Abschlussprüfung: Dateioperations-Modultests jeweils 78, Core macOS 883/Linux 902
bestanden, jeweils zehn ignoriert; CLI jeweils 63, Tauri macOS 172. Clippy mit
allen Targets und `-D warnings` beidseitig, Fuzz-Workspace `--locked`, Format, Diff
und SHA-256-Vergleich der drei geänderten Quelldateien bestanden. Abschließende
Einrückungskorrekturen änderten ausschließlich führende Leerzeichen; dies wurde
mechanisch verglichen. Keine neue GUI-/Mountprüfung war erforderlich.

## Ausgangsprüfung

Der vollständige Headless-Runner meldete acht bestandene Gates und ein
fehlgeschlagenes Tauri-Gate. Die vorhandene Regression deckte ein fehlendes
statisches Tab-Fehlerlabel im Diagnoselog auf. Nach dessen Korrektur bestehen
alle 172 Tauri-Tests und Tauri-Clippy. Das UI-Gate besteht ebenfalls.
Die Ausgangsprüfung ersetzt die noch ausstehenden Bereichsreviews nicht.

## Dateioperationen: vollständiger Testbestand und Gesamtbereich

Auch das gesamte große Testmodul von `fileops.rs` und alle Integrationstests in
`core/tests/fileops_tests.rs` wurden frisch gelesen. Zusammen mit den vorherigen
Teilreviews umfasst der vollständige Bereich acht Dateien: VFS, lokale
Deskriptorzugriffe, Pfade, privates Staging, atomare Dateien und Dateioperationen.
Die Testdoubles wurden auf tatsächliche Mutationen, unveränderliche Providerbelege,
fehlende Listings, fremde Austauschobjekte und dauerhafte Abschlussphasen geprüft.
Vorhandene gemeinsame Fehler- und Testhelfer werden genutzt; unabhängige fachliche
Phasen bleiben getrennt. Kein weiterer tragfähiger struktureller Umbau war nötig.

Sechs vorhandene Gegenbelege prüften erhaltene Daten und Meldungstexte, aber nicht
die strukturierten Wiederherstellungspfade. Die ergänzten Prüfungen scheiterten
auf beiden Plattformen. Austausch, abgelehnter Rückweg, Rollback und Commit-Sync
führen jetzt ihre bekannten Wirkungsnamen sowie die Pfade der ursprünglichen und
sekundären Fehler weiter. Nach einem erfolgreich sichtbaren Rename wird nur dessen
Ziel als vorhandener Rest ergänzt; ein freigewordener Temp-/Sicherungsname wird
nicht neu als Datenrest ausgegeben. Mutationen und Identitätsschranken ändern sich
nicht. Die bekannte exFAT-Grenze und weitere Recoveryhüllen im Querschnitt bleiben
separat offen.

Abschlussprüfung: Dateioperations-Modultests jeweils 78, Integrationstests jeweils
36; Core macOS 883/Linux 902 bestanden, jeweils zehn ignoriert; CLI jeweils 63,
Tauri macOS 172. Workspace-Clippy auf macOS und Core-/CLI-Clippy auf Linux mit
allen Targets und `-D warnings`, Fuzz `--locked`, Format, Diff und acht identische
Bereichs-Quellhashes bestanden. Die isolierte Linux-Kopie enthielt zunächst eine
ältere reine CLI-Testformatierung; nach Abgleich mit der aktuellen Quelle besteht
auch ihr vollständiger Formatlauf.

## Triage des ergänzenden Nacht-Reviews

Die drei Funde auf Basis `366fe6f` wurden gegen `e3ffb80` geprüft und bleiben
begründet: Gemischte Papierkorb-Recovery bindet unnötig die Quell-Eltern terminaler
Einträge; Pane-Refresh übernimmt gemeinsame Ordnerdefaults statt Taboptionen; nach
einem Listingfehler kann die leere Ansicht den gespeicherten Auswahlzustand
überschreiben. Alle drei erhalten direkte Korrekturen mit roten Gegenbelegen.
Der Nachtbericht bleibt bis zu deren tatsächlichem Abschluss offen. Diese Triage
ersetzt keinen frischen vollständigen Review der noch offenen Frontendbereiche.

## Nachtrag: gemischte private Papierkorb-Recovery

Der erste ergänzende Nacht-Review-Fund wurde gegen den aktuellen Code reproduziert.
Zwölf echte gemischte v2-Journale prüfen alle drei terminalen Phasen, beide
Indexreihenfolgen und vorhandene beziehungsweise entfernte terminale Quell-Eltern.
Die sechs Fälle mit entferntem Ordner scheiterten auf macOS und Linux, obwohl der
unabhängige offene Claim vollständig wiederherstellbar blieb. Die sechs Kontrollen
mit vorhandenem Ordner bestanden.

Die gehaltenen Quellhandles sind nun optional und behalten exakt den Journalindex.
Nur Recoveryphasen mit tatsächlichem Quellzugriff öffnen den gespeicherten Elternort
neu. Terminale sowie rein manuell zu prüfende Transferphasen benötigen ihn nicht.
Ein gemeinsamer Zugriff weist einen fehlenden benötigten Handle ausdrücklich ab;
Objekt-, Mount-, Sandbox-, ACL- und Symlinkprüfungen bleiben bestehen. Das persistierte
Journalformat ändert sich nicht. Alle direkten Claim-/Restore-/Transferzugriffe und
Tests wurden auf die optionale Bindung abgeglichen.

Die zwölf positiven Fälle bestehen nach der Korrektur. Drei zusätzliche Gegenfälle
mit verschwundenem, neu angelegtem und durch Symlink ersetztem offenem Quell-Elternort
lassen Journal, privaten Claim und fremden Inhalt unverändert. Der alte Elternordner
bleibt beim Austausch unter einem eigenen Namen erhalten, damit Inode-Wiederverwendung
den Test nicht zufällig abschwächt. Die Papierkorb-Suite besteht mit 84 Fällen auf
macOS und 86 auf Linux, jeweils vier separat ignorierten Spezialprüfungen.
Der Nachtbericht bleibt bis zu beiden Tabkorrekturen offen.

Abschlussprüfung des Nachtrags: Core macOS 885/Linux 904 bestanden, jeweils zehn
ignoriert; CLI jeweils 63 und Tauri macOS 172. Clippy mit allen Targets und
`-D warnings` beidseitig, Fuzz `--locked`, Format/Diff und vier identische
Quellhashes bestanden. Eine GUI-Prüfung war für den unveränderten Oberflächenweg
nicht erforderlich; native Mount-/UID-Spezialfälle wurden nicht erneut ausgeführt.

## Nachtrag: Taboptionen, Listingfehler und direkter Retry

Die zwei verbliebenen Nachtfunde wurden in der echten Browseroberfläche rot
reproduziert: Nach unterschiedlicher Hidden-Einstellung zweier Tabs desselben
Pfads überschrieb Refresh den ersten Tab mit dem gemeinsamen Ordnerdefault. Nach
einem Fehler beim Aktivieren eines zuvor ausgewählten Tabs speicherte der
Sitzungs-Autosave eine leere Auswahl statt des letzten belegten Zustands.

Der Controller bindet Refreshoptionen an Provider, Profil und Pfad des aktiven
Tabs. Neue Orte laden weiterhin ihre Ordnerdefaults. Während eines fehlgeschlagenen
Listings ersetzt die geleerte Pane keinen belegten Tabstand. Ein direkter Retry
übernimmt die gespeicherten Namen und Scrollposition; nur tatsächlich neu gelistete
Namen werden ausgewählt. Sequenz-, Profil- und Lesetokenprüfungen bleiben bestehen.
Die bisherige Darstellungsprüfung erwartet neue globale Defaults nun passend zum
Tabvertrag erst nach Navigation an einen anderen Ort. Beide neuen Gegenbelege
bestehen nach der Korrektur.

Im echten App-Bundle auf macOS bestätigt der markierte Sandbox-Test getrennte
Hidden-Optionen trotz Refresh. Ein bei inaktivem Tab entfernter künstlicher Ordner
führt anschließend zum echten Listingfehler. Auswahl, Cursor, Anker und 1200 Pixel
Scrollposition bleiben im tatsächlichen Sitzungsstore erhalten. Nach Rückbenennen
des Ordners stellt direkter Cmd+R-Refresh diese Werte wieder her. Das Ergebnisfenster
wurde visuell geprüft; die eigene Test-App anschließend ausgeblendet und beendet.
Ein temporärer externer Testbeobachter gehört ausschließlich zum isolierten Bundle;
der normale Debug-Build wird danach ohne diesen Beobachter wiederhergestellt.
Der vollständige Frontendreview bleibt unabhängig von diesem gezielten Nachtrag offen.

Abschlussprüfung: sechs Modelltests und 236 Browserfälle sowie produktionsgleiche CSP grün; normaler Debug-Build, nativer Sandbox-Test und Diffprüfung bestanden.

## Workspace-Konfiguration und Fuzz-Harness: vollständiger Review

Alle acht Dateien des Workspace-Konfigurationsbereichs wurden frisch vollständig
geprüft: Workspacegrenzen, drei gemeinsame Produktversionen, eigenständiger
Fuzz-Workspace, lokaler SMB-Patch, Plattformfeatures, Abhängigkeitstrennung,
Buildprofile, ignorierte Artefakte und CLI-Testabsicherung. Die Versionswerte der
drei Rust-Crates, Tauri-Konfiguration und des UI-Pakets sind konsistent. Beide
Cargo-Metadatenabfragen mit gesperrten Lockfiles bestätigen die getrennten
Workspaces und vier registrierte Fuzztargets. Die Release-Vorprüfung besteht.

Ein echter Schutzfehler wurde reproduziert: Cargo übernahm ein geerbtes leeres
oder auf `0` gesetztes `DOPPELDECKER_NO_DEFAULT_USER_PATHS` statt des Repo-Werts.
Die CLI wertet beide Werte ausdrücklich als deaktiviert; damit war die globale
Cargo-Absicherung gegen echte Nutzerpfade nicht verlässlich. Der neue Gegenbeleg
kompiliert ausschließlich ein abhängigkeitfreies Programm in einem temporären
Baum. Es liest nur dieses Flag und startet weder die App noch die CLI. Beide
geerbten Abschaltwerte scheiterten auf macOS und Linux; der Kontrollfall ohne
geerbten Wert bestand. `force = true` erzwingt jetzt den Cargo-Wert in allen drei
Fällen. Direkte Aufrufe des gebauten CLI-Binaries behalten ihren dokumentierten
Vertrag. Die CLI-Dokumentation beschreibt diese Grenze ausdrücklich.

Generator und Schema-/Digesttest des Fuzz-Harness wurden ebenfalls vollständig
mit den direkten Parsergrenzen gelesen. Die drei JSON-Targets deserialisieren und
validieren ohne Dateizugriff; das Finder-Target verarbeitet Plist-/Bookmark-Bytes.
Der Generator arbeitet mit frischen lokalen Roots und schreibt die drei bewusst
versionierten Seeds. Die README unterschied zuvor fälschlich ausschließlich
JSON-Targets; sie benennt jetzt den Finder-Parser und seinen Aufruf. Der
Workspace-Kommentar zum Vendorpatch verweist auf dessen vollständige Dokumentation;
eine überholte Aussage zur Standardbibliothek wurde entfernt.

Prüfung: Cargo-Umgebungsgegenbelege beidseitig rot→grün, jeweils ein tatsächlicher
Seed-Schema-/Digesttest bestanden, Fuzz-Check `--locked`, Cargo-Metadaten,
Release-Vorprüfung, Format und Diff grün. Große übergeordnete Build-, Parser- und
Werkzeugbereiche bleiben unabhängig davon offen.

Die vollständige Werkzeug-Suite besteht abschließend mit 128 Tests. Der neue Cargo-Gegenbeleg ist dem weiterhin offenen vollständigen Werkzeugreview zugeordnet.

## Tauri-Build, Updatekanal und CLI-Einrichtung: vollständiger Review

Alle sechs Bereichsdateien einschließlich unveränderter Tests wurden frisch gelesen:
Buildskript, Info.plist, Capability, Tauri-Konfiguration, CLI-Einrichtung und
Updatecommands. Geprüft wurden der vollständige Updaterblock und öffentliche
Schlüssel, strikte CSP, Rechte beider Fensterlabelklassen, Bonjourdeklaration,
Backendbesitz der Updatehandles, bestätigte Version, appweite Updateschranke,
Generation und fenstereigene Operationsreservierung sowie der Fehlerpfad vor
Handleverbrauch. Fortschritt bleibt am auslösenden Fenster. Die Update-Dokumentation
beschreibt nun ausdrücklich die appweite Updateausnahme und Invalidierung früherer
Funde; interne Entstehungsgeschichte wurde durch den aktuellen Bedienvertrag ersetzt.

Die CLI-Einrichtung hatte trotz vorheriger Bestandsprüfung eine späte Kollision:
`ln -s` behandelt einen vorhandenen Ordner oder Ordner-Symlink als Zielverzeichnis
und legt darin einen weiteren Link an. Eine Gegenprobe führt das tatsächliche
Produktionsskript mit künstlichen Pfaden und mechanisch entfernten Administratorrechten
aus. Beide Ordnervarianten scheiterten rot mit einer unerlaubten Schreibwirkung.
Der Installationsschritt verwendet jetzt die native Foundation-Symlinkanlage über
JavaScript for Automation. Sie legt ausschließlich den exakt benannten Link an und
weist vorhandene Einträge ab. Skripttext bleibt fest; Bundlepfade werden als maskierte
Argumentdaten übergeben. Es entsteht keine zusätzliche Laufzeitabhängigkeit.

Fünf tatsächliche Gegenfälle prüfen fehlendes Ziel, vorhandene Datei, Ordner,
Ordner-Symlink und toten Symlink. Nur das fehlende Ziel erhält den erwarteten Link;
alle fremden Einträge und Ordnerinhalte bleiben erhalten. Der Quellpfad enthält
Leerzeichen, Anführungszeichen und Shellzeichen. Tests starten weder einen echten
Administrator-Dialog noch die Einrichtung unter `/usr/local/bin`.

Abschlussprüfung: alle 173 Tauri-Tests, Workspace-Clippy mit allen Targets und `-D warnings`, Format und Diff grün. Keine echte Installation oder Updateverbindung wurde ausgelöst.

## AP-050: volumenbezogener lokaler Best-Effort-Commit

Der bestätigte macOS-exFAT-Fehler aus AP-044/045/048 ist korrigiert.
Die tatsächliche OS-Fähigkeitsabfrage und exklusive Rename-/Hardlink-Gegenproben
auf eigenen Images ergeben APFS `Assured` und macOS-exFAT `BestEffort`.
`Vfs::file_commit_class_at` ersetzt die pauschale Zusage an allen interaktiven
Commit- und Ordnerbilanzgrenzen. Lokale Vorschau und Ausführung nutzen denselben
Core-Vertrag. Der bestehende Best-Effort-Commit prüft einen vollständigen frischen
Bestand, führt genau ein gewöhnliches Rename aus, synchronisiert die Eltern und
liest den Inhalt zurück. Das unvermeidbare kurze Kollisionsfenster ist Teil der
vorherigen ausdrücklichen Bestätigung; strikte Sync-Garantien bleiben unverändert.

Queueformat 21 speichert ausschließlich die bestätigte öffentliche Rootidentität.
Ein Queue-Neustart erhält diese Erwartung; Root-/Mountdrift verlangt neue
Zustimmung. Der ausführende lokale VFS hält den Zielroot. Eine rote tatsächliche
Gegenprobe tauschte diesen Root während des Kopierstroms aus: Der Inhalt blieb
zwar am gehaltenen alten Root, der Lauf meldete aber fälschlich den sichtbaren
Zielpfad als erfolgreich. Die erneute Sichtbarkeitsprüfung vor dem Commit stoppt
nun mit Drift, erhält die Quelle und lässt den fremden Nachfolger unverändert.
Auch lokale ProviderFile-Downloadziele durchlaufen die Rootbestätigung, ohne
eine zweite identische Klassenfrage zu erzeugen.

Zwei direkte Grenzen wurden im tatsächlichen Gegenlauf korrigiert: Ein engerer
gehaltener Pane-Root darf für privates Staging nicht seinen übergeordneten
Sandboxroot öffnen. Der Staginganker bleibt jetzt innerhalb beider Grenzen.
Außerdem darf Move auf FAT/exFAT keine nicht darstellbaren POSIX-Rechte als
übertragen verlangen. Die pfadbezogene gewöhnliche Rechtefähigkeit bleibt von
der strengeren Owner-/Sonderbitfähigkeit getrennt; Source-Readback, Inhaltshash,
mtime und der bestehende begrenzte Quellabbau bleiben verbindlich.

Die CLI veröffentlicht `local_copy_new_confirmation` in ihrer JSON-Vorschau und
liest die exakte Phrase vor dem Enqueue von stdin. Die GUI verwendet den
bestehenden Queue-Bestätigungsweg mit getrenntem Titel, Risikotext und beschriftetem
Eingabefeld. Text bleibt als Text maskiert, Tastatur und Modalbesitz bleiben zentral.
Ein Browser-Gegenbeleg prüft insbesondere einen Zielnamen mit Markupzeichen und
eine falsche Phrase. Die native macOS-GUI-Sandbox belegt das vorher leere Ziel,
die tatsächliche Eingabe und 90.112 bytegleiche Zielbytes.

Fünf tatsächliche Core-Mounttests auf beiden Plattformen sowie CLI-/GUI-
Gegenläufe bestehen. Headless-Abschluss: Core macOS885/Linux904, im Standardlauf
je15 ausdrücklich ignoriert und je fünf Mounttests separat ausgeführt; CLI je63,
Tauri173, Tools128 und UI6+237. Clippy mit allen Targets und `-D warnings`, Fuzz
`--locked --all-targets`, Format, Diff und beidseitiger Quellhashvergleich bestehen. Der bestehende Vollreview der lokalen VFS- und
Queuebereiche wird um dieses Delta und seine Grenzen ergänzt; noch offene
Frontend-, CLI-, Realmodus- und Werkzeugbereiche erhalten dadurch keinen
pauschalen Vollreviewnachweis.


## Bidirektionaler Sync: vollständige Teilprüfung

Die vollständige Implementierung einschließlich aller Tests, beide Fuzz-Targets
und beide Seed-Dateien frisch gelesen: Snapshot und Grenzen, Drei-Wege-Plan,
Baseline-/Endpunktbindung, Konflikte und Skip, Projektionen, Permits, Quarantäne,
atomare Typwechsel, Resume, Rollback, Prozesssperre und Abschluss. Direkte
Provider-, Einweg- und Queuegrenzen sind einbezogen. Der frühere Journalfix ersetzt
diesen vollständigen Durchgang nicht.

Bestätigte und korrigierte Funde:

- **P1, tatsächliche Streamgröße:** `hash_file` las bis EOF, obwohl nur Listing und
  Stat die Scangrenze einhielten. Ein stabil falsch gemeldeter Stream wurde mit
  falscher Größe akzeptiert. Jeder Hashlauf liest jetzt genau die gebundene Größe
  plus höchstens ein Prüfbyte; kurze und lange Streams liefern `plan_drift`.
- **P1, Dauerhaftigkeit von Rollback und Typwechsel:** Nach einem wirkenden Rename
  konnte Recovery allein aus sichtbaren Daten den offenen Schritt abschließen,
  obwohl fsync weiter scheiterte. Die betroffenen Namespacebarrieren werden vor
  jeder solchen Phasenübernahme erneut ausgeführt; ein weiterer Fehler erhält den
  offenen Schritt. Rename-, Modus- und spätere Readbackfehler liefern
  `outcome_unclear` mit den betroffenen Pfaden. Auch eine Kompensationsumbenennung
  wird geprüft und synchronisiert.
- **P1, Baseline-Abschluss:** Ein wirkender Baseline-Rename mit anschließendem
  Fehler verlor seine Wirkungsbilanz. Der Cleanup konnte das Journal entfernen,
  ohne die Baseline nochmals zu synchronisieren. Save versöhnt einen exakt
  sichtbaren Zustand jetzt erst nach Datei-/Eltern-fsync und frischem Readback.
  Cleanup hält das Journal bis zur gleichen Barriere fest.
- **P2, absichtlich ausgelassene Modi:** Lokale Rollback-Readbacks verglichen ihre
  tatsächlichen POSIX-Modi mit dem Auslassungsdigest eines FTPS-/WebDAV-/SMB-Paars.
  Der Recoveryvergleich berücksichtigt jetzt ausschließlich diese explizite
  Auslassung. Dateiinhalte, Unterbäume und gebundene Modi bleiben strikt geprüft.
- **P2, Fehlerkontext:** Drei Scan-Fehlerhüllen verloren Core-Codes und Recoverypfade.
  Die ergänzten Texte erhalten beides; ein Verbindungsabbruch bleibt maschinenlesbar.
- **Lesegrenze von Baseline und Journal:** Statt eines separaten Stat mit späterem
  gewöhnlichem Open verwenden beide Loader denselben descriptor-relativen,
  nicht blockierenden No-Follow-Weg. Typ, Größe und Identität werden am geöffneten
  Objekt und nach dem begrenzten Lesen erneut geprüft. Die bestehende lokale
  FIFO-Gegenprobe gehört zum vollständigen Core-Lauf; kein separater deterministischer
  Stat/Open-Race des Paarstores wurde injiziert.

Sechs tatsächlich rote Gegenbelege wurden vor der jeweiligen Korrektur ausgeführt;
acht neue Regressionen prüfen außerdem Modus-fsync und strukturierte Scanfehler.
Die vollständigen Core-Läufe bestehen mit macOS893/Linux912, je15 bewusst
ignorierten Spezialfällen; CLI je63, Tauri173, Workspace-Clippy, Fuzz
`--locked --all-targets` und Format bestehen. Nach drei letzten Fehlerhüllen wurden
alle 110 bidirektionalen Modultests und Clippy auf beiden Plattformen erneut
geprüft. Die finale Quelldatei ist auf beiden Plattformen SHA-256-identisch.
Diff und betroffene Verträge sind nachgeprüft. Keine neuen Produktfunktionen,
Provider-Schreibfreigaben, GUI-Abnahme oder Stromausfallprüfung in diesem Bereich.


## Einseitiger Sync: Teilprüfung vom 2026-10-07

Historischer Zwischenstand vom 2026-10-07, abgeschlossen am 2026-10-16; siehe
[Fortsetzung](code-review-2026-10-16.md#abdeckung-und-fortsetzung).

AP-025 war begonnen, aber noch nicht vollständig abgeschlossen. Acht der neun
Scope-Dateien frisch vollständig gelesen: `sync.rs`, `sync/atomic.rs`,
`sync/exclude.rs`, `sync/execute.rs`, `sync/metadata.rs`, `sync/plan.rs`,
`sync/safety.rs` und `tests/property_tests.rs` jeweils unter `core/`.
Die Hashbelege stehen im Teilnachweis von `.codeqa/full-review-2026-10-07.json`.
`core/tests/sync_tests.rs` ist bislang nur an den betroffenen Fixtures und
Regressionen gelesen; der vollständige Durchgang beginnt dort bei Zeile 1.

Korrigierte Funde:

- **P1, portable Rechteänderung:** `apply_pending_mode` quittierte den Rechtebefehl
  ohne Datei-/Ordner-fsync und ohne Rücklesen. Zwei tatsächlich rote Regressionen
  prüften den verweigerten fsync und einen wirkungslosen Rechtebefehl. Beide prüfen
  jeweils Datei und Ordner. Der Abschluss verlangt jetzt fsync, frische Rootbindung
  und passenden Typ/Modus; ein Folgefehler erhält `outcome_unclear` und das Ziel.
- **P2, strukturierte Recoverybilanz:** Atomare Sync-Fehlerhüllen nannten die
  betroffenen Namen ausschließlich im Text. Zehn vorhandene Fehlerfälle waren mit
  zusätzlichen Prüfungen der strukturierten Ausgabe tatsächlich rot: Commit,
  Quarantäne, Rückweg, Identitätswechsel und Laufanlage. Die Fehlerhüllen
  erhalten jetzt frühere Recoverypfade und die an der jeweiligen Wirkungsgrenze
  möglichen Namen. Bestätigt unveränderte Renames bleiben weiterhin wiederholbar;
  der stärkere Code eines unklaren Rückwegs bleibt erhalten.

Der Ressourcen-Fork-Wachstumskandidat wurde durch einen tatsächlichen nativen
macOS-Gegenlauf ausgeschlossen: Wachstum nach dem Öffnen vergrößerte das gebundene
Transferbudget nicht und führte korrekt zu Drift. Der vorhandene Fork-Reader
begrenzt diesen Stream bereits. Die temporäre Probe wurde aus dem Repo entfernt;
kleiner Quell-/Logbeleg separat behalten. Daraus folgt keine Aussage über reguläre
Dateistreams.

- [x] AP-025: am 2026-10-16 vollständig gelesen und Stream-/Fehlergrenzen geklärt.
  Die nachfolgenden Kandidaten und Prüfzahlen beschreiben den damaligen Stand.

Damals vorgesehene Fortsetzung: zuerst das tatsächliche Bytebudget des regulären
`transfer_file_data` und der Baumhash-Leseschleife gegen eine wachsende oder stabil
falsch gemeldete Quelle prüfen. Beide Schleifen lesen derzeit bis EOF; der
Ressourcen-Fork-Gegenbeleg deckt sie nicht ab. Danach den Fehlervertrag des
bedingten Provider-Claims prüfen: `claim_target_into_quarantine` setzt jeden
Providerfehler auf `plan_drift`; ohne eine garantierte unveränderte Wirkung wäre
das zu schwach. Diese beiden Kandidaten sind noch nicht dynamisch bestätigt.
Anschließend `core/tests/sync_tests.rs` ab Zeile 1 vollständig lesen, Deltas und
Grenzen prüfen und erst dann AP-025 schließen. Der Bereich bleibt bis zu diesen
Prüfungen offen.

Die vollständigen Core-Läufe bestehen mit macOS895/Linux914, je15 bewusst
ignorierten Spezialfällen; CLI je63, Tauri173, Workspace-Clippy, Fuzz
`--locked --all-targets` und Format sind grün. Nach einer ausschließlich optischen
Einrückung der Fehlerhüllen wurden die 21 atomaren Modultests und die Formatprüfung
beidseitig nochmals ausgeführt; Linux-Clippy ebenfalls. Die drei geänderten
Quelldateien stimmen auf beiden Plattformen per SHA-256 überein. Diff und betroffene
Dokumentation sind nachgeprüft. Keine neue GUI-/Mount-/Stromausfallprüfung für diese
Änderungen; die nativen AP-050-Belege bleiben separat gültig.
