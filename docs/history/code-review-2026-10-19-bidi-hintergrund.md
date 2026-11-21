# Bidirektionale Vorbereitungen im Hintergrund

Der native Gegenlauf hält ausschließlich den ersten tatsächlichen Read einer
künstlichen Quelldatei im unveränderten Debug-Build an. Während dieser Haltephase
schlägt die native Fenstergrößenänderung mit Fehlercode 3 fehl; das Fenster bleibt
1180×850 groß. Der Prozess-Sample zeigt `prepare_bidirectional_sync` und den
gehaltenen Read auf `com.apple.main-thread`. Nach der Freigabe erscheint die echte
Konfliktvorschau. Quelle und Ziel bleiben unverändert. Der native Prüfauftrag und
sein eigenständiges Cleanup enden jeweils mit Exitcode 0.

## Korrektur und Grenzen

Die drei Commands `prepare_bidirectional_sync`, `prepare_bidirectional_resume` und
`prepare_bidirectional_rollback` sind asynchron und führen ihre bisherige
Vorbereitung in `spawn_blocking` aus. Der App-State wird erst dort gebunden.
Der bestehende fenstereigene reservierende Rahmen mit Cancel- und Panicbehandlung
schützt weiterhin Queueprozessbesitz, Single Flight und Freigabe der eigenen
Reservierung. Es entsteht keine globale Sperre; andere Fenster behalten ihre
eigenen Queues und Abbruchtoken.

Der Abbruchtoken wird vor und nach der Corevorbereitung geprüft. Diese Änderung
verspricht keine sofortige Unterbrechung einer bereits laufenden nativen
I/O-Funktion oder aller inneren Scanschritte. Eine frisch erzeugte Vorbereitung
verwirft bei erkanntem Abbruch ihr eigenes mutationsloses Journal. Resume und
Rollback lassen ihr bestehendes Recovery-Journal ausdrücklich erhalten. Die
Profilprüfung vor dem Einlagern, registrierte Paar-/Rootbindung, zufällige
Recoveryvorbereitungs-ID und ausschließlich backendgehaltenen Permits bleiben
erhalten. Kein Fehler führt einen mutierenden Befehl erneut aus.

Vier bestehende Quellguardtests erkannten zunächst die asynchronen Signaturen
nicht als ihre Einstieg- beziehungsweise Folgemarker. Die Marker und der bekannte
reservierende Cancelrahmen sind jetzt berücksichtigt; die Reservierung muss
weiterhin vor dem ersten Vorbereitungsschritt stehen. Die operativen Tests für
Cancel, Panicfreigabe, andere Fenster, Profilbindung, Pending-Enqueue und
Recoveryübergang bleiben erhalten.

## Native Nachprüfung

Der identische tatsächliche Read wird mit dem neuen Build erneut gehalten.
Die native Fensteränderung gelingt währenddessen in 22 ms; das Fenster ist
nachweislich 1160×820 groß, bevor der Read freigegeben wird. Der Sample zeigt
den vorbereitenden Scan und den Haltepunkt auf einem Worker, während der
Hauptthread frei bleibt. Die Konfliktvorschau erscheint nach der Freigabe.
Quelle und Ziel bleiben unverändert; Prüfauftrag und Cleanup enden mit Exitcode 0.
Die betroffenen Zustände wurden visuell geprüft. Dies ist ein gezielter
Livenessbeleg, kein allgemeiner Leistungsbenchmark.

Die native Halteprobe betrifft konkret die frische lokale Vorbereitung. Resume
und Rollback teilen denselben geprüften Dispatch-/Reservierungsweg und behalten
ihre operativen App-/Core-Recoverytests; ein eigener nativer gehaltener
Resume-/Rollbacklauf wird nicht behauptet. Eine frühe fehlgeschlagene private
C-Helferprobe startete keine Test-App und zählt nicht als Produktgegenlauf.

Die finalen App-Suiten bestehen mit 219 Tests auf macOS und 209 auf Linux; 2 beziehungsweise 4 Fälle bleiben ignoriert. Explizite Kindtests werden über ihre Eltern ausgeführt; zwei Linux-GTK-Fälle behalten ihre Umgebungsvoraussetzung. Workspace-Clippy mit allen Targets und `-D warnings` sowie Formatprüfung bestehen auf beiden Systemen. Alle 385 Quellen sind während der Läufe unverändert. Core, CLI, Fuzz und Frontend sind bytegleich zum unmittelbar vorherigen vollständigen Prüfstand; dessen beidseitige Core-/CLI-/Fuzzgates und 408 Browserprüfungen einschließlich CSP plus sechs Pane-Tabtests werden ausdrücklich übernommen. Die vollständige neue Prüfung betrifft App, Clippy und Format; es wird kein erneuter Core-/CLI-/Browserlauf behauptet. Dieser Review
betrifft ausschließlich die drei Dispatchgrenzen, den bestehenden gemeinsamen
Reservierungsrahmen und die angepassten Schutzmarker. Historische Vollreviewhashes
bleiben erhalten.
