# Code-Review vom 2026-10-02

Ausgangsstand: `f84116f`. Risikoorientiertes Review mit einer Obergrenze von zwei
Stunden. Kern, Provider, Queue, Tauri-Grenzen, CLI, Oberfläche und Build-/Testskripte
wurden einbezogen. Der Schwerpunkt lag auf Datenintegrität, Fehler- und
Abbruchpfaden, nebenläufigen Zuständen und Änderungen seit den gespeicherten
CodeQA-Nachweisen. Unveränderte, bereits geprüfte Bereiche wurden nicht vollständig
erneut gelesen. Fremdcode und generierte Assets wurden nicht verändert.

## Einordnung nach späteren Korrekturen

Dieser Bericht beschreibt den Reviewstand vom 2026-10-02. Das damalige
Papierkorb-Claim-Rennen ist durch den [privaten v2-Claim-Kern](trash-private-claim.md)
mit nativen macOS-/XDG-Adaptern geschlossen. Die damaligen Restbefunde zu
Recoverypfaden sind durch die [Korrekturen vom 2026-10-03](code-review-2026-10-03.md)
und die [Recoverybilanz über mehrere Aktionen](recovery-aggregation.md) erledigt.
Aktueller Reststand: [Backlog](../backlog.md). Die folgenden historischen
Befunde und damaligen Prüfgrenzen bleiben als Beleg erhalten.

## Ergebnis

Achtzehn Fehlergruppen wurden bearbeitet. Die wichtigste verbleibende Grenze ist der
Papierkorb-Claim: Ein anderer Schreibberechtigter kann den Claim vor seinem letzten
Rename austauschen. Der neue finale Identitätscheck erkennt den falschen Ausgang
und verhindert falsche Erfolgsmeldungen und eine blinde Recovery. Er verhindert
die vorausgegangene Verschiebung selbst noch nicht.

Die Schichtung aus sicherheitsrelevantem Core, dünnen Tauri-/CLI-Adaptern und einer
ereignisgesteuerten Oberfläche passt weiterhin. Sie ist durch mehrfach vorhandene
Fehler- und Lebenszykluspfade belastet: Ein übersehener Cleanup-Fehler trat an 17
Remote-Queue-Grenzen auf. Dafür gibt es jetzt einen gemeinsamen Helfer. Ein
Architekturwechsel ist nicht angezeigt; sinnvoll sind gemeinsam verwendete
Abschlussverträge und gezielte Fehlerfalltests an den Schichtgrenzen.

## Bestätigte Befunde und Korrekturen

P1 bezeichnet hier einen möglichen Eingriff in fremden Bestand oder eine falsche
Datenwirkungsbilanz. P2 bezeichnet einen reproduzierbaren Funktions-, Persistenz-
oder Bedienfehler. Die Nummern dienen nur diesem Bericht.

| Nr. | Priorität | Auslöser und bisherige Folge | Änderung und Beleg |
| --- | --- | --- | --- |
| 1 | P1 | Eine FTPS-/SMB-Fähigkeitsprobe findet beim Cleanup zusätzliche Namen. FTPS entfernte alle gelisteten Dateien; SMB entfernte Unterordner rekursiv. | [FTPS](../core/src/ftps.rs) prüft erzeugte Namen und deren Inhalt vor dem ersten Delete. [SMB](../core/src/smb.rs) prüft Namen/Typen und entfernt Probeordner nur nichtrekursiv. Socket-Regression: kein Delete bei fremdem Bestand; Verzeichnisregression: fremde Kinder bleiben bytegleich. |
| 2 | P2 | Mirror setzt einen Ordner auf `0555`, bevor überzählige Kinder in Quarantäne verschoben werden. Native Zeitstempel werden danach durch eigene Renames wieder verändert. Bei anfangs gleichen Metadaten fehlte teils jede Abschlussaktion. | [Planung](../core/src/sync/plan.rs) bindet nötige Metadatenabschlüsse für direkte Kindänderungen. [Ausführung](../core/src/sync/execute.rs) finalisiert Ordner nach Entfernungen. [Realmodus](../core/src/real_mode.rs) erlaubt nur vorher bestätigte ausstehende Metadaten; fremde Zieländerungen bleiben gesperrt. Reale Permit-Tests prüfen Rechte, Quarantäne, Zeitstempel und leeren Folgeplan. |
| 3 | P2 | SMB konnte gültige Zeitstempel vor 1970 schreiben, las sie aber als unbekannt zurück. | [FILETIME-Konvertierung](../core/src/smb.rs) rechnet ab 1601 mit korrekter Abrundung negativer Sekunden. Grenzwerttests und tatsächlicher Samba-Readback für Unix-Zeit `-1`. |
| 4 | P2 | Ein 255-Byte-Name mit Papierkorb-Kollisionssuffix oder zusätzlichem `.trashinfo` überschritt die Dateisystemgrenze. | [Papierkorb-Namen](../core/src/trash.rs) reservieren Platz für beide Suffixe, erhalten kurze Endungen und schneiden gültiges UTF-8 nur an Zeichengrenzen. Tests für lange ASCII-/UTF-8-Namen, Kollisionen und Infodateien. |
| 5 | P1, teilweise behoben | Nach der ersten Claimprüfung ausgetauschter Bestand konnte im Papierkorb landen und als erfolgreich gemeldet werden. | [Finaler Readback](../core/src/trash.rs) prüft Geräte-/Inode-/Typidentität erneut. Ein vorab persistierter Transferhinweis hält ungeklärte Fälle über Neustarts blockierend. Deterministische Austausch- und Journalfehlerproben prüfen Bilanz und erhaltene Bytes. Das Claim-Rennen bleibt offen; siehe unten. |
| 6 | P2 | Ein erneut gespeicherter CLI-Dateioperationsfehler erhielt Temp-Recoverypfade, verlor aber die Klassifikation bekannter Verzeichnissicherungen. | [CLI-Fehlercommit](../cli/src/main.rs) übergibt vollständige `FileOperationActuals`. Nach Schließen und Wiederöffnen der Queue bleiben beide Pfadlisten erhalten. |
| 7 | P2 | Zwei Einstellungsaufrufe während der asynchronen Datenabfrage überschrieben das Modal-Token. Fertig und Escape konnten den sichtbaren Dialog nicht mehr schließen. | [Einstellungsdialog](../frontend/settings-ui.js) sperrt bereits die Öffnungsphase. Browserregression mit verzögerter IPC-Antwort prüft Fertig, erneutes Öffnen und Escape. |
| 8 | P2 | Nach erfolgreichem Auswerfen und gescheitertem Standardpfadabruf blieb eine alte Pane-Auswahl bedienbar. Eine verspätete Antwort konnte außerdem eine neuere Navigation überschreiben. | [Geräteansicht](../frontend/devices-ui.js) entwertet betroffene Panes sofort und bindet den Rückfall an deren Ladesequenz. Zwei Browserregressionen prüfen Fehler und überholte Navigation. |
| 9 | P1/P2 | Der Sandbox-Launcher schrieb Pfade mit `&`/`<` ungefiltert in XML und verwendete alte Testbundles erneut. Darin vorhandene Links konnten Schreibzugriffe umlenken. | [Launcher](../tools/run-test-sandbox.sh) verwendet `plistlib` und einen exklusiven neuen Bundle-Elternordner. Isolierte Tests prüfen Sonderzeichen und erhaltenen fremden Linkbestand. |
| 10 | P2 | Nach atomarem App-Austausch scheiterte eine Prüfung; der Altstand war bereits verloren beziehungsweise eine fehlgeschlagene Erstinstallation blieb stehen. | [Installer](../install) hält den Altstand bis zum Abschluss, prüft auch das Staging und rollt Fehler/Signale zurück. Cleanup arbeitet relativ zu gehaltenen Verzeichnisdeskriptoren. Zwölf Tests verwenden echte Swift-Renames mit künstlichen Bundles und Prüfern ausschließlich unter `/tmp`, einschließlich Wrapper-Austausch und Symlink-Gegenprobe. |
| 11 | P2 | Ein dauerhafter Fehler in `claim_next()` ließ einen Pending-Eintrag zurück. Der Fensterworker startete sich danach unbegrenzt selbst neu. | [Workerabschluss](../src-tauri/src/queue_runtime.rs) unterscheidet Claimfehler vom normalen Enqueue-Rennen. Der automatische Retry endet; `OutcomeUnclear` bleibt erhalten. Eine isolierte Queue-Regressionsprobe prüft Fehler, reparierten Pfad und bewusste Wiederaufnahme nach Neustart. |
| 12 | P2 | Das Zurücksetzen einer verbrauchten Remote-Vorbereitung scheiterte; 17 Aufrufer verwarfen den Journalfehler. Sichtbar blieb eine Bestätigung ohne RAM-Permit und ohne verständlichen Wiederherstellungsweg. | [Gemeinsamer Reset-Helfer](../src-tauri/src/queue_runtime.rs) erhält Journalcode, Ursache und ursprünglichen Fachfehler und nennt Neustart/Neuprüfung. Alle sechs Command-Adapter verwenden ihn. Tests prüfen sicheren Nichtcommit, terminale Fehlercodes, erfolgreichen Reset und bewusste Neuprüfung; es wird kein Ersatzpermit erzeugt. |
| 13 | P2 | Bei einem Finder-Drop wurde die erste Kopie bereits eingereiht, während ihre IPC-Antwort noch ausstand. Eine zwischenzeitliche Zielnavigation ließ die Oberfläche diese Kopie anschließend als fehlend zählen. | [Dateiaktionen](../frontend/file-actions.js) erhalten die erfolgreiche Einreihung auch bei überholtem Oberflächenzustand. Die Browserregression prüft getrennt die korrekte Teilbilanz, den Queue-Inhalt und das ursprüngliche Ziel; weitere Gruppen starten nach der Navigation nicht. |
| 14 | P1 | Ein vorhandener Symlink in `app-config/cache` lenkte Vorschau- und Diff-Momentaufnahmen aus der markierten Testsandbox in einen fremden Ordner um. | [Snapshot-Cache](../src-tauri/src/browser_commands.rs) prüft beide neuen Verzeichnisebenen mit dem bestehenden direkten Kindordner-Helfer. Regressionsfälle prüfen beide Cachetypen und beide Symlinkebenen; fremde Daten, Einträge und Rechte bleiben erhalten. Ein aktives Austauschrennen nach der Pfadprüfung ist damit nicht als gelöst behauptet. |
| 15 | P2 | Ein früher Quell-Lesefehler, Abbruch oder fehlender Hardlink-Anker verließ den bereits angelegten privaten Arbeitsbereich ohne Abschluss und ohne sichtbaren Wiederherstellungspfad. | [Gemeinsamer Abschluss](../core/src/atomic_file.rs) umfasst jetzt alle Ausgänge der besitzenden Datei-/Sync-Funktionen. Leere, sicher abgeschlossene Arbeitsordner verschwinden; unsichere Reste bleiben mit Text, Fehlerpayload und Bilanz erhalten. `OutcomeUnclear` bewahrt auch ein datenleeres Intentjournal. 21 gezielte C2-Tests prüfen Fehler, Abbruch, Rekursion, Hardlinks und erfolgreiche Kopie trotz Cleanupfehler; sechs Gegenproben scheitern am Original. |
| 16 | P2 | Queue-Fehlerhüllen, ein bidirektionaler Abbruch und die öffentliche Dateiexecutor-Grenze verloren vorhandene strukturierte Recoverypfade. Der Text konnte den Pfad noch enthalten, das Journal beziehungsweise der aufrufende Code aber nicht. | [Fehlerpayload](../core/src/error.rs), [Queue](../core/src/queue.rs) und [Abbruchgrenze](../core/src/bidirectional.rs) erhalten die belegten Pfade einschließlich sekundärer Journalfehler. Die tatsächlichen Fehlercommit-Aufrufer persistieren sie; Reopen-Tests erhalten Pfad, `OutcomeUnclear` und Retry-Sperre. Die weiter unten beschriebene Bilanz über mehrere bereits erfolgreiche Teilaktionen bleibt offen. |
| 17 | P2 | Nach Einstellungen oder Informationen erbte ein normaler Dialog deren Breitenklasse. Ein Kinddialog konnte außerdem den `onchange`-Handler seines Elternbesitzers auslösen oder verlieren. | [Modalverwaltung](../frontend/modal.js) beginnt jeden Renderer mit neutraler Hülle und sichert/restauriert auch `onchange`. Laufende Browserfälle prüfen Einstellungen/Informationen → Umbenennen, die tatsächliche Breite und isolierte Kind-/Elternhandler. |
| 18 | P2 | Zwei Fenster lasen noch uninitialisierte Altwerte. Nach der ersten Migration und einer neuen Nutzerwahl konnte die verspätete zweite Migration diese Wahl überschreiben. | [Einstellungsstore](../src-tauri/src/desktop_settings.rs) prüft und committet die einmalige Migration unter derselben Sperre. Eine überholte Migration liefert den aktuellen Snapshot ohne neue Revision, Iconwirkung oder Rundruf. Zwei Leserthreads, ein verspäteter Worker, Persistenzreadback und Browserregression prüfen den Fall. |

Die Probe-Cleanup-Korrekturen behaupten keine atomare Identitätsbindung zwischen
Readback und einem späteren Remote-Delete. Sie verhindern den konkret belegten
unbedingten Abbau zusätzlicher Namen und Unterbäume.

## Weitere Prüfbefunde

- Der Ausgangsstand bestand acht der neun gebündelten Headless-Gates. Das
  Formatierungsgate war bereits rot; die betroffenen Rust-Stellen wurden mit
  `cargo fmt` formatiert. Daraus entsteht keine zusätzliche Funktionsänderung.
- Zwei C2-Recovery-Testfixtures setzten implizit eine restriktive Prozess-Umask
  voraus. Sie legen ihren privaten Test-Konfigurationsordner jetzt ausdrücklich
  mit Modus `0700` an. Die produktive Privatheitsprüfung bleibt streng.
- Der ignorierte Samba-E2E erwartete noch bidirektionale SMB-Zielwirkungen, obwohl
  der Provider diese mangels identitätsgebundener Quarantäne inzwischen sperrt.
  Die aktualisierte Matrix prüft diese Sperren ohne Mutation sowie die erlaubten
  Additionen, Ersetzungen und Entfernungen zum lokalen Ziel. Gewöhnliches
  AddOnly-Upload, Download, Reconnect, Paging und Queue-Ausführung bleiben enthalten.
- Der statische Rust-Modulwächter hielt unterschiedliche Betriebssystemausdrücke
  pauschal für gegenseitig ausgeschlossen. `not(linux)` und `macos` gelten aber
  gleichzeitig. Die Prüfung vergleicht die Bedingungen jetzt paarweise und behandelt
  komplexe Ausdrücke konservativ; eine Regression deckt die Überschneidungen ab.
- Die SMB-Runner-Tests sichern wieder den Abbruch bei null oder zwei Treffern in
  der Testliste sowie die Bindung des eigentlichen Aufrufs mit `--exact` ab. Die
  Laufzeitprüfung war vorhanden, ihre frühere Testabdeckung jedoch verloren gegangen.
- Parallele Linux-Tests legten zwei Fixture-Rennen offen: frisch geschriebene
  Fastra-Helfer meldeten `ETXTBSY`, und ein sofortiger Lockwechsel nach Queue-Drop
  meldete bei parallelen Helferstarts zeitweise noch eine belegte Sperre. Die künstlichen
  Fastra-Fixtures werden jetzt serialisiert; der Journal-Reopen-Test behält seine
  Prozesssperre über den geprüften Reload. Die Exklusivitätsproben im Core bleiben
  erhalten. Die regulär parallele Tauri-Suite besteht danach auf beiden Plattformen.

## Prüfungen

Der vollständige macOS-Lauf von `tools/check-gates.py --json` bestand **9/9 Gates**:
Rustfmt, gelockte Frontend-Installation, Werkzeugtests, Core, CLI, Tauri, Clippy,
Fuzz-Build und Browser-Suite. Nach den weiteren Funden wurden die betroffenen Gates
erneut ausgeführt. Die letzten vollständigen Rust-Suites enthalten 817 erfolgreiche
Core-, 62 CLI- und 164 Tauri-Tests; zusätzlich bestehen 127 Werkzeugtests. Die vier im
Core-Standardlauf ignorierten Tests betreffen aktive LAN-Suche, zwei SFTP-E2Es und
den separat ausgeführten Samba-E2E.

Auf Linux bestehen Core, CLI und Tauri, Workspace-Clippy mit `-D warnings` und der
gelockte Fuzz-Build mit `--all-targets`. Der letzte vollständige Core-Lauf enthält
828 erfolgreiche Tests. Nach der letzten öffentlichen Fehlerhülle wurden die
Queue-Tests erneut geprüft; die abschließende Tauri-Suite umfasst 162 Tests.
Der zusätzliche echte
Samba-Lauf besteht mit SMB 3.1.1, Signierung/Verschlüsselung, 1.500 Listing-Einträgen,
Zeitstempel-Readback, Capability-Sperren und Queue-Ausführung. FTP/FTPS- und
WebDAV-Loopback-E2Es sind Teil der grünen Core-Suite.

Die Browser-Suite prüft die tatsächlichen gerenderten Zustände mit einer
kontrollierten Tauri-Mockbrücke. Ein nativer App-Lauf wurde nicht durchgeführt.
Alle Datenwirkungen erfolgen in künstlichen Wegwerfbereichen.
Die echten App-Einstellungen, Nutzerdateien und der Systempapierkorb werden durch
die Tests nicht verwendet. Der Installer-Test baut und notarisiert keine App und
schreibt nicht nach `/Applications`.
Auch `npm run release:check` besteht. Ein Releasepaket wurde nicht erzeugt.

## Abdeckung und Grenzen

Der persistente Nachweis steht in [`.codeqa/coverage.json`](../.codeqa/coverage.json).
Vollständige Bereichsabschlüsse beziehen sich auf das jeweilige Delta seit dem dort
genannten Vorgängercommit plus die heutigen Ein-/Ausgangsverträge. Alle geänderten
Bereiche wurden so abgeschlossen: 21 von 26 Bereichen erhielten in diesem Review
einen neuen Nachweis; alle 26 sind bezogen auf ihren geprüften Inhalt aktuell, keine
Scope-Datei ist unzugeordnet. Unveränderte unabhängige Funktionen und die
unveränderten fünf Bereichsnachweise wurden nicht pauschal neu gelesen.

Priorisierte Restarbeit:

1. **Papierkorb-Claim privat binden.** Der offene Quellordner erlaubt weiterhin
   Austausch vor dem letzten Rename. Ein privater Namensraum muss die macOS-ACLs
   und die Reihenfolge der XDG-Infodatei berücksichtigen. [Vertrag und
   reproduzierbarer Restbefund](trash-toctou-design.md).
   Ein bloßes `mkdir(0700)` im gemeinsam beschreibbaren Quellordner genügt nicht:
   Vererbte Allow-ACLs können vor der späteren Privatheitsprüfung fremde Handles
   ermöglichen. Benötigt werden ein bereits privater Anker auf demselben Mount,
   gebundene Recovery-Identitäten und ein versionierter Journalübergang.
2. **Native GUI-Abnahme.** Einstellungen und Auswerfen sind im laufenden
   Headless-Browser mit Tauri-Mock geprüft. Native macOS-Menüs, echte Mehrfenster-
   Bedienung und die interaktive Linux-Abnahme wurden in diesem Review nicht
   wiederholt. Es wurden keine Layoutänderungen vorgenommen.
3. **Offene frühere Vertragsbefunde.** Der [Backlog](../backlog.md) enthält unter
   anderem mountabhängige Metadatenfähigkeiten, Linux-Mountfristen und noch fehlende
   Provider-Root-Readbacks. Ein grüner Gate-Lauf schließt diese nicht automatisch.
4. **Harte Installationsabbrüche.** Bei Stromausfall oder `SIGKILL` kann kein
   Shell-Trap zurückrollen. Der eigene Transaktionsordner hält Identitätsangaben
   und gegebenenfalls den Altstand für manuelle Wiederherstellung bereit.
5. **Recoverybilanz über mehrere Teilaktionen.** Ein späterer Sync-Aktionsfehler
   kann zuvor in `SyncStats` gesammelte Restpfade verlieren; vergleichbare Lücken
   bestehen bei späten bidirektionalen Folgeprüfungen und beim normalen erfolgreichen
   SFTP-Queueabschluss. Die Daten und Journale bleiben erhalten. Registrierte lokale
   Queue-Arbeitsbereiche werden zusätzlich wiedergefunden; direkte Core-Aufrufe und
   nicht registrierte Provider-Downloads haben diesen Rückweg nicht allgemein.
   Der Codebefund ist festgehalten; eine vollständige injizierte Gesamtlauf-
   Reproduktion und eine gemeinsame Abschlussbilanz stehen aus.

Dieses zeitlich begrenzte Review ist kein Nachweis, dass jede Quellzeile neu gelesen
wurde oder keine weiteren Fehler bestehen.
