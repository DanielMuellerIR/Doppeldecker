# Hintergrundarbeit bei Verbindungsaufbau, Listing und Dateiöffnung

Stand: 2026-10-18.

`connect_sftp_profile`, `scan_sftp_dir` und `open_remote_snapshot` sind asynchrone
Tauri-Commands. Sie verschieben den `AppHandle` in `spawn_blocking` und holen
`AppState` erst dort. Netzwerkzugriffe, Provider-Mutexe, Download, Cachepflege und
System-App-Übergabe laufen deshalb außerhalb des Fenster-Threads. Fehler eines
Workers bleiben strukturierte `AppError`s.

Das SFTP-Listing behält seinen einmaligen lesenden Reconnect und liefert Einträge,
Sitzungsgeneration und Sicherheitsklasse aus derselben gehaltenen Providerantwort.
Vor der Rückgabe werden die Profilbindungen erneut geprüft. Auch eine
Verbindungsstatusantwort darf keinen inzwischen geänderten Profilstand bestätigen.

## Bidirektionale Vorbereitung

`prepare_bidirectional_sync`, `prepare_bidirectional_resume` und
`prepare_bidirectional_rollback` holen ihren App-State im Blocking-Worker.
Baum-, Inhalts- und Projektionsvorbereitung blockieren damit den nativen GUI-Thread
nicht. Der bewährte reservierende Backendrahmen hält weiterhin Prozessbesitz und
fenstereigene Single-Flight-Schranke. Fehler und Panic geben nur die eigene
Reservierung frei; andere Fenster behalten ihren Zustand und Cancelkanal.

Der fenstereigene Abbruchtoken wird vor der Vorbereitung und nach Rückkehr des
Core geprüft. Ein laufender nativer Scan wird dadurch nicht unmittelbar
unterbrochen. Eine abgebrochene frische Vorbereitung verwirft ihr eigenes
mutationsloses Journal vor der Rückgabe; Resume und Rollback behalten ihre
bestehenden Recovery-Journale. Profilrevisionen, Paarbindung und nicht
serialisierbare Pending-/Permitbelege folgen weiterhin dem bisherigen Vertrag.
Die Antwortformate bleiben gleich. [Nativer Gegenlauf](release-validation.md).

## Remote-Dateiöffnung

Die Reservierung der fenstereigenen Single-Flight-Schranke liegt vor dem ersten
Providerzugriff und bleibt bis nach der geprüften System-App-Übergabe gehalten.
Andere Fenster behalten ihre eigene Queue und ihren eigenen Abbruchtoken.
Der bestehende Download erhält den tatsächlich in diesem Fenster reservierten
`Cancel`; ein losgelöster neuer Token wäre durch `cancel_sync` nicht erreichbar.

`remote-snapshot-started` zeigt Phase und Abbruchknopf über die vorhandene
Operationsanzeige. Der Knopf und Escape verwenden `cancel_sync`. Abbruch wird
zwischen blockierenden Schritten sowie in den bestehenden Download-/Hashschleifen
geprüft. Er beendet einen laufenden Netzwerkaufruf nicht gewaltsam. Der
SFTP-Verbindungsaufbau bleibt durch seine bestehende Frist von 15 Sekunden begrenzt.
Wird während des Verbindungsaufbaus abgebrochen, hat der Abbruch nach diesem Schritt
Vorrang vor einer gleichzeitig eintreffenden Verbindungsfehlermeldung.

Inhalt, frische Providerbindung und lokale Datei werden vor dem Öffnen unverändert
geprüft. Ein Abbruch vor der Übergabe startet keine System-App. Ein bereits an das
Betriebssystem übergebener Start lässt sich durch einen späteren Abbruch nicht
zurücknehmen. Der bestehende Snapshotvertrag räumt unvollständige Downloads auf;
vollständig geprüfte Momentaufnahmen folgen weiterhin ihrer Aufbewahrungsfrist.

Die Reservierung wird vor dem Terminalevent freigegeben. Das Ereignis
`remote-snapshot-done` beziehungsweise `remote-snapshot-error` geht nur an das
verursachende Fenster und kommt vor dem Start des nächsten Queueworkers.
Die Command-Antwort bestätigt danach lediglich den erledigten Ablauf und verändert
keine Fortschrittsanzeige mehr. Fehler vor dem Erwerb der Reservierung, etwa `busy`,
kommen unmittelbar als Command-Fehler zurück; sie dürfen keine fremde Reservierung
freigeben. Ein Worker-Panic wird ohne Übernahme seines Textes in einen App-Fehler
übersetzt, gemeldet und gibt die eigene Reservierung ebenfalls frei.

## SFTP-Inhalt und Konfliktvorbereitung

SFTP-Öffnen verwendet die eigene [begrenzte Leseklasse](sftp-contents.md).
Start- und Ergebnisereignis nennen deren tatsächliche Einschränkungen; das
strikte Sync-VFS bleibt gesperrt. Erst ein neuer Leseversuch darf eine verlorene
Sitzung identitätsgleich reconnecten. Der fehlgeschlagene Versuch wird nicht
fortgesetzt und wiederholt keine lokale Mutation.

Auch ein Konfliktvergleich kann den vollständigen SFTP-Inhalt lesen. Die
Provider-Dateivorbereitung hält deshalb bei dieser Quelle die abbrechbare
Fensterreservierung. `fileop-preparation-started` und `fileop-preparation-done`
gehen ausschließlich an das verursachende Fenster; Escape und Abbruchknopf nutzen
dessen echten Token. Das Terminalevent kommt vor dem nächsten Queueworker.
Die begrenzte iterative Baumvorbereitung nutzt dieselbe Reservierung und ihren
Token; sie hat zusätzlich eine 60-Sekunden-Frist zuzüglich laufendem Request und
Cleanup. Eine abgebrochene Vorbereitung erzeugt keinen Queueeintrag. Navigation
bleibt während der Vorbereitung erreichbar.

## FTP-/FTPS-Dateivorbereitung

Auch F5/F6 mit einer FTP-/FTPS-Seite hält die abbrechbare Fensterreservierung.
Die vorhandenen `fileop-preparation-*`-Ereignisse zeigen eine neutrale Vorbereitung;
die spätere Ausführungsklasse stammt weiterhin aus dem Core-Vertrag. Escape und
Abbruchknopf erreichen denselben `Cancel`, den die privaten FTPS-Probenowner
halten. Das Flag wird weder persistiert noch mit anderen Fenstern geteilt.

Abbruch beendet einen bereits laufenden Serverzugriff nicht gewaltsam. Die
begonnene Teilprobe prüft ihren Transportabschluss, ihre Inhalte und ihren eigenen
Cleanup weiter. Vor der nächsten Teilprobe und nach der Ziel-/Quellvorbereitung
wird das Flag erneut geprüft. Ein sauberer Abbruch liefert `interrupted` und reiht
nichts ein. Transport-/Cleanupfehler und `outcome_unclear` behalten dagegen ihren
ursprünglichen Code und die dauerhaften Recoveryhinweise. Ein terminaler Abbruch
gibt die eigene Reservierung vor dem nächsten Worker frei; eine neue Operation
erhält einen neuen Token. Der [FTPS-Vertrag](ftps-stou-transport.md) enthält die
gezielten Gegenbelege und ihre Grenzen.

## Prüfung

Ein isolierter Downloadtest löst den Abbruch über den gespeicherten Fenster-Token
aus. Er belegt unveränderte Quelldaten, entfernte Teilkopie, eine anschließend erneut
nutzbare Reservierung und den unberührten Token eines zweiten Fensters. Ein
Panic-Gegenlauf prüft Fehlermeldung und Freigabe. Die bisherigen App-Tests prüfen
weiterhin Listing-/Profilbindung, Reconnect, Queue-Reentranz und gezielte Ereignisse.

Die Browser-Suite prüft die sichtbare Phase, Erfolg, Fehler und Escape-Abbruch einer
Remote-Dateiöffnung. Der native macOS-Gegenlauf vom 2026-10-06 verwendet ein
echtes Tauri-Fenster mit den Produkt-Commands und einer zusätzlichen
Testbeobachtung: Ein eigener
Loopback-Server lässt den SSH-Handshake jeweils zwei Sekunden warten, während
`requestAnimationFrame` Anzahl und größte Unterbrechung der Fensterframes misst.
Die Remote-Dateiöffnung wurde über den tatsächlichen Abbruchknopf beendet; das
Terminalevent meldete `interrupted`, das Fenster war danach wieder freigegeben.
Während der jeweils etwa zwei Sekunden Wartezeit liefen 122, 120 beziehungsweise
119 Frames weiter; die größten Framelücken betrugen 28, 18 beziehungsweise 50 ms.
Das eigene Testfenster wurde visuell geprüft und anschließend geschlossen.
Die App-Suiten bestanden auf macOS mit 172 und Linux mit 170 Tests, die
Browser-Suite mit 225 Fällen; App-Clippy war auf beiden Plattformen grün.
Diese Fehlerfixture ersetzt keinen erfolgreichen Serverdownload oder den
Finder-zu-FTPS-End-to-End-Test.


### FTPS-Konfliktentscheidung

Bei FTP-/FTPS-Konflikten bleibt die erste Vorschau ohne Mutation. Die Entscheidung
wird mit der vorhandenen fenstereigenen Konflikt-ID erneut vorbereitet. Ersetzen
und neue Namen werden anschließend konkret bestätigt. Die passive FTPS-Anzeige
und Skip lesen ausschließlich Metadaten der Konfliktobjekte. Ersetzen und neue
Namen binden danach wieder deren vollständigen Inhalt. Skip benötigt keine Probe
für ausgeschlossene Wirkungen, auch innerhalb zusammengeführter Ordner.
Quellbäume mit übersprungenen Kindern bleiben vollständig erhalten; der Core
meldet, ob die übrige Auswahl überhaupt Quellentfernung benötigt. Beide Vorbereitungsphasen halten die
bestehende Fensterreservierung und ihr Abbruchsignal. Die Konfliktwartezeit selbst
hält keine globale Providersperre. Nach jeder Antwort gelten Aktionsgeneration,
Pane-Revision und Profilrevision weiterhin; ein verspätetes Ergebnis nach
Navigation reiht nichts ein. Die Backendprüfung bindet ID, Auswahl und Snapshot
vor einer Teilprobe erneut. Der Ablauf steht im
[FTPS-Vertrag](ftps-stou-transport.md#ap21-konfliktentscheidung-vor-den-proben).
