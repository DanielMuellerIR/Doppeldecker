# Deltareview: Dateioperationen und Sync

Der Bereich `core-bidirectional` erhält eine Deltaprüfung seit dem vollständigen
Review auf `53f53e0`, einschließlich seiner heutigen Vorbereitungs-, Bestätigungs-,
Projektions-, Resume-/Rollback- und Tauri-Anfragegrenzen. Vier Fuzzdateien sind
gegen diesen Vollreview unverändert; ihre Hashbelege bleiben erhalten. Dies ist
kein neuer Vollreview der übrigen Projektbereiche.

## Korrigierte Befunde

- **P2: spätere Typwechselwahl ohne sichtbaren Blocker.** Nach 100 gewählten
  lokalen Datei-/Ordnerwechseln blieb ein weiterer Wechsel auswählbar. Die
  anschließende Vorbereitung verweigerte denselben 101. Typwechsel bereits.
  Anzeige und Vorbereitung verwenden jetzt denselben Mengenvalidator.
  `KeepBoth` prüft seine tatsächlichen Wirkungen ebenfalls; `Skip` bleibt bei
  diesem beidseitigen Konflikt zulässig. Ein unabhängiger Dateibaum reproduziert
  die Abweichung zwischen Anzeige und Vorbereitung am alten Stand. Der
  korrigierte Gegenlauf prüft vier gesperrte Varianten, das weiterhin verfügbare
  Überspringen, unveränderte Daten und ein fehlendes Laufjournal.
- **P2: Namensalias zwischen neuen Konfliktkopien.** Der Resolver prüfte einen
  neuen Namen gegen beide Snapshots, verglich mehrere neue Konfliktnamen
  untereinander aber nur bytegenau. `kopie.txt` und `Kopie.txt` ergaben deshalb
  einen ausführbaren Plan für zwei möglicherweise identische Ziele. Dieselbe
  konservative Case-/Unicode-Prüfung gilt jetzt auch zwischen neuen
  Geschwisternamen. Die Ablehnung erfolgt vor Journalanlage und Wirkung.
  Der alte Stand scheitert am Case-Gegenbeleg; der korrigierte Test prüft Case,
  NFC/NFD, einen zulässigen Plan mit unterschiedlichen Namen und unveränderte
  echte Quelldateien. Ein weiterer roter Gegenlauf prüft dieselbe Grenze auf
  einem bereits teilweise aufgelösten Plan. Dessen früher gewählte Zielnamen
  bleiben nun reserviert; neue Vorschläge sparen diese Namen ebenfalls aus.

Die Ausführungsschranken, lokalen und entfernten Fähigkeiten, Fristen,
Journalformate, Root-/Snapshotbindung und einmaligen Permits bleiben erhalten.
Der Namensvergleich bindet keine fremden Elternpfade und ersetzt keine frische
Kollisionsprüfung unmittelbar vor einem Commit.

## Nachfolgender Abschluss am 2026-10-19

Die nachstehend historisch offenen Caller- und Querschnittspunkte sind inzwischen
geprüft und geklärt. Die bidirektionale Vorbereitung und der verbliebene
Startgeometriezugriff sind mit nativen Gegenläufen korrigiert. Alle 26 Bereiche
besitzen aktuelle Vollreview- oder daran gebundene Deltabelege; kein
Querschnittskandidat ist mehr offen. [Abschlussbericht](code-review-2026-10-19-abschluss.md).
Die menschliche Linux-Kandidatenabnahme bleibt eine getrennte Releasebedingung.

## Weitere Grenzen

Die Callerprüfung hält einen gesonderten Kandidaten fest: Die synchrone
bidirektionale Tauri-Vorbereitung führt vollständige Baum-/Inhaltsscans und
Projektionsvorbereitung aus. Ein nativer Gegenlauf muss im Tauri-Bereich prüfen,
ob das Fenster dabei blockiert; der Kandidat gilt hier nicht als korrigiert.
Die übrigen offenen Bereiche, der Querschnitt und die menschliche interaktive
Linux-Kandidatenabnahme bleiben separat offen.

## Verifikation

Die zwei neuen Regressionen besitzen drei tatsächlich rote Gegenläufe vor der
Korrektur, einschließlich schrittweiser Namenswahl. Danach bestehen die
vollständigen Core-Suiten mit 1110 Tests auf macOS und 1129 auf Linux; je
21 umgebungsgebundene Tests bleiben ignoriert. CLI besteht auf beiden Plattformen
mit 69 Tests. Die vollständige isolierte macOS-App-Suite besteht nach Freigabe
mit 204 Tests; Linux besteht mit 194 Tests und zwei GTK-Ausnahmen. Workspace-Clippy
mit allen Targets und `-D warnings`, Fuzz mit `--locked --all-targets` sowie
Formatprüfung sind beidseitig grün. 383 Code-/Werkzeugdateien sind zum geprüften
Stand hashgebunden. Der gezielte Browser-Erstbaselinefall und die CSP-Prüfung
bestehen ebenfalls.

Im echten macOS-Testbundle zeigt der 101. Konflikt in hellem und dunklem Theme
vier gesperrte Varianten mit sichtbarer Mengenbegründung. Überspringen bleibt
aktiv und per Tab erreichbar; Escape verwirft die Vorbereitung. Die 101 Dateien
und 101 leeren Gegenordner bleiben unverändert, Queue und Laufjournal leer.
Die eigene Test-App und Wachhaltung sind beendet; Cleanup meldet keine übrigen
eigenen Prozesse. Der erste native Lauf bleibt als fehlgeschlagene
Testtext-Assertion dokumentiert: Seine AX-Buttonprüfung bestand, die Erklärung
war im Screenshot sichtbar. Der Wiederholungslauf besteht einschließlich der
anschließenden visuellen Prüfung. Die menschliche Linux-Kandidatenabnahme wird
damit nicht ersetzt.

## Queue und Recovery

Der anschließende Bereich `core-queue-recovery` erhält eine Deltaprüfung seit
`8c2ffa5`. Geändert ist `queue.rs`; die übrigen 13 Dateien des früher vollständig
geprüften Papierkorb-/Quarantänebereichs sind gegen diese Basis unverändert.
Gelesen sind das gesamte Delta einschließlich Tests sowie die heutigen Claim-,
Retry-, Bestätigungs-, Journal-Durability- und manuellen Recoverygrenzen und ihre
direkten Desktop-Caller. Reine FTPS-Recovery bleibt ohne Executor, Netzwerk oder
Löschweg. Frühere unklare Probenreste werden nicht still geschlossen.

**P2: Sync ersetzte eine unabhängig beobachtete FTP-Zielabbildung.** Zwei Sitzungen
konnten trotz gleicher Konfiguration unterschiedliche kanonische Profilroots
melden. Die Dateioperation prüfte dies bereits; der generische Sync ersetzte die
Zielsitzung dagegen noch durch die Quellverbindung. Der neue Gegenbeleg verwendet
zwei echte künstliche FTP-Kontrollverbindungen mit `/first/from` und `/second/to`.
Am alten Stand scheitert der öffentliche Queue-Vorbereitungsweg mit fehlendem
FTPS-Pfad, bevor er die eigentliche Fähigkeitsgrenze erreicht. Jetzt teilen Sync
und Dateioperationen denselben Binder und erhalten die Zielsitzung bei ungleicher
Abbildung. Der Gegenlauf erreicht die absichtlich verweigerte
Capability-Verbindung; kein Mutationsbefehl wird angenommen und keine
Zugangsdaten-Rückfrage erfunden. Eine vollständige Übertragung auf einem wechselnd
abbildenden Server wird damit nicht als geprüft behauptet.

Die vollständigen Gates bestehen auf macOS mit 1111 Core-, 69 CLI- und 204
App-Tests und auf Linux mit 1130 Core-, 69 CLI- und 194 App-Tests. Je 21
Core-Spezialfälle bleiben ignoriert; auf Linux zusätzlich zwei GTK-Fälle.
Workspace-Clippy, Fuzz mit `--locked --all-targets` und Formatprüfung bestehen
beidseitig. Die zuletzt ergänzte Kanonisierung des künstlichen privaten
Testordners wurde danach unter Linux nochmals im gesamten Queue-Modul sowie
mit Workspace-Clippy und Formatprüfung geprüft; der Produktcode ist dabei
unverändert. Der endgültige Stand ist mit 383 Code-/Werkzeughashes gebunden.
Die künstlichen Testordner besitzen ausdrücklich private Rechte und keine
geerbten ACL-Freigaben; die reale Produktschranke wurde nicht gelockert.
Es gab keine neue GUI-Änderung, Produktinstallation oder menschliche
Linux-Kandidatenabnahme.

## Einweg-Sync: dauerhafte Hardlink-Ersetzungen

Der Bereich `core-one-way-sync` erhält eine Deltaprüfung seit `74d7c04`: alle
Änderungen der fünf betroffenen Dateien und die heutigen Plan-, Commit-,
Metadaten-, Permit-, Mirror-, Abbruch- und Recoverygrenzen. Vier weitere Dateien
sind gegen den vorherigen vollständigen Review unverändert hashgebunden.

**P2: Die Ergebnisbilanz ließ eine tatsächlich abgeschlossene Hardlink-Ersetzung
aus.** Der bestätigte schwache lokale Auftrag zählte die geplante Ersetzung;
der Ausführungspfad `CreateHardlink` erhöhte anschließend jedoch ausschließlich
die Dateizahl. Der Zähler dauerhafter Ersetzungen blieb null. Er berücksichtigt
nun auch diesen erfolgreich abgeschlossenen Ersatz eines vorhandenen Namens;
reine Additionen und Simulationen erhöhen ihn nicht.

Der rote Dateigegenlauf durchläuft die öffentliche lokale Vorbereitung,
Bestätigung und Ausführung. Er prüft zuerst den neuen Inhalt und die tatsächliche
Inode-Verbindung beider Zieldateien und scheitert erst danach an der Bilanz
null statt eins. Der korrigierte Test prüft außerdem eine reine Addition mit
weiterhin null dauerhaften Ersetzungen. Dafür meldet ausschließlich der Testthread
unter seinem privaten künstlichen Zielroot schwächere Fähigkeiten; alle echten
Pfad-, Snapshot-, Permit- und Commitprüfungen bleiben aktiv. Dies belegt keinen
realen schwachen Mount mit Hardlink-Unterstützung oder einen Stromausfall.
Die Produkt-Fähigkeitsprüfung und die Datei-/Quarantänewirkung ändern sich nicht.

Die vollständigen Gates bestehen am endgültigen unveränderten Quellstand auf
macOS mit 1112 Core-, 69 CLI- und 204 App-Tests sowie auf Linux mit 1131 Core-,
69 CLI- und 194 App-Tests. Je 21 Core-Spezialfälle und unter Linux zwei native
GTK-Fälle bleiben ignoriert. Clippy mit allen Targets und `-D warnings`, Fuzz
mit `--locked --all-targets` und Formatprüfung bestehen beidseitig. 383
Code-/Werkzeugdateien sind hashgleich gebunden. Die neue Regression läuft in
beiden Core-Gesamtsuiten. Eine zusätzliche native GUI-Abnahme ist durch diesen
reinen Ergebniszählerfix nicht behauptet.


## Dateioperationen und VFS

Der Bereich `core-fileops-vfs` erhält eine Deltaprüfung seit `fb14df1`: sämtliche
Änderungen einschließlich der neuen Upload-, Konfliktanzeige- und Konfliktkopie-
Dateien sowie die heutigen Root-, Sandbox-, Staging-, Commit-, Move- und
Recoverygrenzen. Drei Dateien sind gegen die vorherige vollständige Prüfung
unverändert hashgebunden. Die unmittelbaren Queue- und Desktop-Caller sind
mitgelesen. Frühere Vollreviewbelege bleiben erhalten.

**P2: lokale Uploadgrößen waren an einen veralteten Stat gebunden.** Die
Vorbereitung öffnete und hashte die neue Dateiversion, addierte aber die zuvor
beobachtete Pfadgröße. Ein kontrollierter Dateigegenlauf änderte eine leere Datei
vor dem Öffnen zu drei Byte: Der gehaltene Inhalt war `abc`, die Baumbilanz null.
Größenbilanz und Acht-GiB-Dateigrenze verwenden jetzt die Metadaten des gehaltenen
Handles. Der Größenlimitfall nutzt eine künstliche Sparse-Datei; ein Abbruch
begrenzt auch den alten fehlerhaften Gegenweg, bevor er riesige Inhalte hasht.
Die einmalige Zeitpunktsteuerung existiert ausschließlich im Testthread.

**P2: „Beide behalten“ erkannte geänderte Kinder eines nativen Quellordners
nicht.** Die gespeicherte Ordneridentität blieb unverändert, obwohl sich der
Inhalt einer vorhandenen Datei geändert hatte. Der öffentliche Queue-Gegenlauf
übernahm diesen neuen Inhalt nach einem Neustart ungeprüft unter dem Aliasnamen.
Der Quellbeleg erhält nun zusätzlich einen rekursiven Hash mit Kinderidentitäten,
Inhalten und geordneten vollständigen Listings samt Nachprüfung. Der strikte
Elternbeleg und Provider-Token bleiben erhalten. Die Listings erfolgen gemeinsam
je Verzeichnis statt einmal je Kind. Der korrigierte Gegenlauf prüft Copy und
Move sowohl direkt als auch in einem Unterordner, bei gleicher Dateigröße und
zurückgesetzter Dateiänderungszeit. Drift verhindert schon das Aliasziel;
Quelle und vorhandenes Ziel bleiben erhalten. Die bisherigen erfolgreichen
Konfliktkopien sind weiterhin grün.

Die vorhandenen optionalen Inhaltsfelder tragen auch den Ordnerbeleg. Ein älterer
noch unbelegter Auftrag verlangt daher eine neue Planung. Zielbelege, passive
Skip-Vorschauen, bestätigte Ersetzungen und der schwächere beobachtete
SFTP-Baumvertrag behalten ihre bisherigen getrennten Verträge.

Die vollständigen Gates bestehen am endgültigen unveränderten Quellstand auf
macOS mit 1114 Core-, 69 CLI- und 204 App-Tests sowie auf Linux mit 1133 Core-,
69 CLI- und 194 App-Tests. Je 21 Core-Spezialfälle und unter Linux zwei native
GTK-Fälle bleiben ignoriert. Workspace-Clippy mit allen Targets und `-D warnings`,
Fuzz mit `--locked --all-targets` und Formatprüfung sind beidseitig grün.
383 Code-/Werkzeugdateien sind vor und nach den Läufen hashgleich gebunden.
Der vollständige macOS-App-Lauf schließt die freigegebenen isolierten Setup-
und privaten Pasteboardfälle ein. Der geänderte Produktcode besitzt keine neue
GUI-Wirkung; die menschliche Linux-Kandidatenabnahme bleibt offen.


## Vollständiger Realmodus-Review

Der Bereich `core-real-mode` ist mit allen fünf heutigen Dateien vollständig
geprüft: lokale und entfernte Vorbereitung, Permit und Audit, Remote-Snapshots,
rekursive Mengenbilanz und die gesonderten Mounttests. Gelesen sind zusätzlich
die direkten VFS-, Sync-Ausführungs-, Bestätigungs-, Queue- und Desktopgrenzen.
Der Review umfasst auch die aktuellen normalen lokalen AddOnly-/Update-Profile;
er übernimmt deren Erleichterungen nicht für Remote, Mirror oder bidirektionale
Läufe. Veraltete SFTP-Inhalts- und WebDAV-Zielangaben im Realmodusvertrag sind
an die bereits geltenden getrennten Produktverträge angepasst.

**P1: Restore-Proben-Cleanup löschte fremde Kollisionsdateien.** Bei fehlgeschlagener
exklusiver Anlage oder einem der drei No-Replace-Renames bereinigte der lokale
Pfad alle möglichen Probenamen. Ein künstlicher VFS-Gegenlauf legt unmittelbar
vor dem jeweiligen echten Dateisystemaufruf eine fremde Datei an. Am alten Stand
gehen alle vier Kollisionsdateien verloren; der Fehler wird dabei weiterhin als
Kollision gemeldet. Die Korrektur folgt nur dem nach erfolgreicher Anlage oder
bestätigtem Rename tatsächlich bekannten Probeort und synchronisiert dessen
Elternordner nach dem Cleanup. Ein noch nicht erreichter Kollisionsname gehört
nicht zur eigenen Probe.

Anlage und Rename beachten außerdem die bereits vorhandenen Fehlerphasen. Bei
unklarem Ausgang bleibt die Wirkung erhalten und beide möglichen Renameorte
werden als strukturierte Recoverypfade gemeldet. Ein ergänzender Gegenlauf
führt die Anlage beziehungsweise jedes der drei Renames tatsächlich aus und
verliert erst anschließend die Antwort. Alle vier Fälle behalten ihren einen
realen Zwischenstand; kein neuer Cleanup interpretiert ihn als sichere Kollision.
Der lokale Pfad verspricht weiterhin keine atomare bedingte Entfernung nach
Inode. Der gesonderte stärkere Remote-Restore-Proofvertrag bleibt unverändert.

**P2: Snapshot-Streams waren vor der Größenprüfung unbeschränkt.** Ein Provider
konnte drei Byte ankündigen und mehr Inhalt liefern; die Momentaufnahme schrieb
bis EOF, bevor sie die Größenabweichung ablehnte. Der rote Gegenlauf liest und
schreibt auf diese Weise 65.536 Byte in einen künstlichen lokalen Cache. Nun
begrenzen sowohl Download als auch die zweite Remote-Kontrolllesung den Strom
auf die angekündigte Größe plus genau ein Prüfbyte. Ein überlanger Chunk erreicht
die lokale Datei nicht mehr; ein verkürzter Readback bleibt ebenfalls ein Fehler.
Der Test zählt die gelesenen Bytes bei Überlänge unabhängig im ersten und zweiten
Stream und prüft den leeren Cache nach Ablehnung. BLAKE3, Abbruch,
Dateirechte und manifestgebundene Aufbewahrung bleiben bestehen.

Die vollständigen Gates bestehen am endgültigen unveränderten Quellstand auf
macOS mit 1117 Core-, 69 CLI- und 204 App-Tests sowie auf Linux mit 1136 Core-,
69 CLI- und 194 App-Tests. Je 21 Core-Spezialfälle und unter Linux zwei native
GTK-Fälle bleiben ignoriert. Workspace-Clippy mit allen Targets und `-D warnings`,
Fuzz mit `--locked --all-targets` sowie Formatprüfung bestehen beidseitig.
383 Code-/Werkzeugdateien sind vor und nach den Läufen hashgleich gebunden.
Alle drei neuen Regressionen sind in beiden Core-Gesamtsuiten enthalten. Die
vorhandenen opt-in-Mounttests bleiben durch diesen Lauf unberührt; ein neuer
Image-, Stromausfall- oder nativer GUI-Versuch wird nicht behauptet.

## Fenster, AppState und Queue

Der vollständige Bereichsreview umfasst alle zehn aktuellen Dateien für Start,
App-Helfer, Mutex, Queue-Laufzeit und Commands, Fenster, Menü, Sitzungseinstellungen,
Pane-Tabs und Operationsereignisse. Gelesen sind 8742 Ausgangszeilen einschließlich
Tests sowie die direkte Exit-Snapshot-/Bestätigungsgrenze in `app_commands.rs`.
Deren übrige Commands bleiben Teil des noch offenen eigenen Bereichsreviews.
Fenster behalten getrennte Journale, Reservierungen, Permits und Ereignisziele;
der externe Prozesslock und die Providerverbindungen bleiben prozessweit.

**P1: Eine gesetzte ungültige Sandbox fiel auf den Normalmodus zurück.** Leere,
nur aus Leerraum bestehende und nicht als UTF-8 darstellbare Umgebungswerte wurden
wie eine fehlende Variable behandelt. Ein echter Kindprozess-Gegenlauf reproduziert
alle drei Fälle. Nur eine tatsächlich fehlende Variable bedeutet jetzt Normalmodus;
jeder gesetzte ungültige Wert bleibt ein Fehler. Der Gegenlauf prüft zusätzlich
einen gültig markierten künstlichen Root und die fehlende Variable, ohne globale
Testumgebung oder Nutzerpfade zu verändern.

**P2: Eine geschlossene Startschranke konnte eine Worker-Neustartkette erzeugen.**
Nach abgewiesener Reservierung endete der Queue-Worker, hielt einen offenen Eintrag
aber weiterhin für automatisch neu startbar. Der Gegenbeleg verwendet echte
Fenster-/Appzustände und ein künstliches Pending-Journal an der produktiven
Dispatchentscheidung; er misst keine tatsächlich erzeugte Threadanzahl. Lokales
Schließen und appweites Beenden verhindern jetzt Worker-Claim und automatischen
Neustart. Wird ein Schließversuch abgewiesen, öffnet derselbe Backendpfad die
Schranke und weckt bereits bestätigte Arbeit wieder. Die Prüfung beider möglichen
Reihenfolgen von Workerende und Freigabe erlaubt genau einen neuen Worker.
Andere Fenster bleiben von einer lokalen Schließschranke unabhängig.

Elf App-Hilfstests und 42 Queue-Laufzeittests bestehen nach den beiden roten
Gegenläufen. Die vollständige App-Suite besteht auf macOS mit 207 Tests und auf
Linux mit 197 Tests sowie zwei unveränderten GTK-Ausnahmen. Workspace-Clippy mit
allen Targets und `-D warnings`, Formatprüfung und der lokale native QA-Build
bestehen. 383 Code-/Werkzeughashes bleiben während der Schlussprüfungen identisch.
Nach den getrennten SMB-Korrekturen bestehen zusätzlich die vollständigen
Core-/CLI-/App-/Clippy-/Fuzz-/Formatgates beidseitig: macOS mit 1122 Core-, 69
CLI- und 207 App-Tests, Linux mit 1141 Core-, 69 CLI- und 197 App-Tests. Je 21
Core-Spezialfälle und zwei Linux-GTK-Fälle bleiben ignoriert. Die abschließende
macOS-Schließkorrektur besteht anschließend ebenfalls App, Clippy, Build und
Formatprüfung sowie Linux-App-/Clippy-/Formatgates.

**P1: Das vordefinierte native Beenden umging die Appprüfung.** Ein echter
macOS-Gegenlauf startet eine künstliche Kopie von 3000 kleinen Dateien. Cmd-W
lässt das aktive Fenster korrekt offen; Cmd-Q beendet dagegen den Prozess nach
drei Zieldateien, während das Journal noch `Running` mit zwei bilanzierten Dateien
enthält. Es entsteht kein Absturzbericht. Im gesperrten Bibliothekscode sendet
der vordefinierte Menüpunkt unmittelbar `terminate:`; der verwendete App-Delegate
besitzt keine verhinderbare `applicationShouldTerminate:`-Grenze. Eine zunächst
geprüfte Menüumleitung schützt Cmd-Q, der reguläre macOS-Beenden-Aufruf beendet
aber weiterhin den laufenden Prozess; der zweite Gegenbeleg enthält sieben
kopierte Dateien und ein unverändert laufendes Journal.

Die Korrektur ergänzt deshalb die gemeinsame native Terminierungsgrenze am
bestehenden App-Delegate. Eine unmittelbare Unterklasse ohne zusätzliche
Instanzvariablen erhält dessen Zustand und übrige Methoden. Die typgerechte
AppKit-Rückfrage verwirft den unmittelbaren Prozessabbruch und fordert über
`AppHandle::exit(0)` die bestehende vollständige Backendprüfung samt einmaliger
Exitbestätigung an. Instanzgröße und ursprüngliche Klasse werden geprüft; die
Installation erfolgt auf dem Hauptthread vor der Fenstererzeugung. Das normale
native Menü bleibt erhalten. Die native Gegenprüfung erreicht danach die
bestehenden Backend-Sperren.


**P2: Das letzte saubere Fenster beendete die macOS-App.** Ein nativer Gegenlauf
schließt eine vollständig kopierte und bytegeprüfte Menge von 24 Dateien ab.
Cmd-W beendet danach den Prozess, obwohl der bestehende Sitzungsvertrag die App
ohne Fenster weiterlaufen lässt. Der verwendete Tauri-Runtime sendet nach dem
letzten zerstörten Fenster `ExitRequested` ohne Exitcode. Auf macOS wird genau
dieser automatische Exit jetzt vor dem appweiten Schließgate verhindert;
explizites Beenden mit Exitcode erreicht weiterhin sämtliche Backendprüfungen.
Die native Abschlussprüfung umfasst zusätzlich ein neu geöffnetes Fenster samt
erneuter Dateioperation und anschließendes sauberes Cmd-Q.


Der finale Mehrfensterlauf weist Cmd-W, Cmd-Q und reguläres AppleEvent-Quit während
laufender Arbeit ab. Im zweiten Fenster wird eine Einzeldatei abgeschlossen,
während die erste Queue weiterarbeitet; das zweite saubere Fenster lässt sich
schließen. Die erste Queue schließt anschließend alle 120 Dateien bytegeprüft ab.
Das letzte Schließen lässt die App aktiv. Der Gesamtjob bleibt als fehlgeschlagen
dokumentiert, weil danach eine Pfadeingabe des Testhelfers den Listenfokus traf.
Ein gesonderter vollständig erfolgreicher Lauf mit demselben Binary kopiert
24 Dateien, schließt das letzte Fenster, öffnet ein neues, kopiert eine frische
Einzeldatei und beendet die App ohne Fenster per Cmd-Q. Beide Läufe melden
Cleanup ohne eigene verbleibende Doppeldecker-Prozesse; ihre relevanten Ansichten
wurden visuell geprüft. Die menschliche Linux-Kandidatenabnahme bleibt offen.
