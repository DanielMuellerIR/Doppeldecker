# Deltareview: FTP-/FTPS-Transport und Operationsbelege

Geprüft sind sämtliche Änderungen seit dem vollständigen Review auf `fb14df1`
und die heutigen Grenzen von Proben, Uploads, typisierten Rename-Belegen,
Queueowner und dauerhaften Intents. Die neue Datei `ftps_transport_tests.rs`
ist vollständig gelesen. Die bisherigen beiden Dateien wurden anhand ihrer
Deltas und direkten Aufrufgrenzen geprüft; dies ist kein erneuter Ganzreview
aller drei Bereichsdateien.

## Korrigierter Befund

**P2: Weitere Streamaufrufe verloren einen bereits unklaren Uploadabschluss.**
Nach einer verlorenen Rename-Antwort gab der erste Flush korrekt
`outcome_unclear` mit STOU-, Ziel- und Intentpfaden zurück. Der Datenstrom war
bereits verbraucht. Ein weiterer Flush oder Schreibaufruf meldete anschließend
nur `connection_lost` ohne die strukturierten Hinweise. Der unklare mögliche
Serverzustand wurde dadurch im öffentlichen Schreibvertrag abgeschwächt.

Der echte FTP-Protokollgegenlauf verwendet den öffentlichen VFS-Schreibstrom
und verliert die Kontrollverbindung einmal vor und einmal nach dem
serverseitigen Rename. Vor der Korrektur scheitert er am abgeschwächten Code.
Der Writer hält jetzt auch einen fehlgeschlagenen Abschluss fest. Weitere
Schreib- und Flushaufrufe erhalten den unklaren Ausgang und sämtliche zuvor
genannten Pfade. Sie senden keinen weiteren Befehl. Der Gegenlauf prüft genau
ein RNTO zum angeforderten Ziel, die tatsächlich vorhandenen vollständigen
Payloadbytes und einen unveränderten unabhängigen Sentinel. Beide Fälle sind
nach der Korrektur grün. Die Prüfung verwendet Klartext-FTP; die gemeinsame
Writerlogik wird zusätzlich von den bestehenden TLS-Tests durchlaufen.

## Erhaltene Grenzen

Klartext-FTP, die sichtbaren TLS-Identitätsstufen und gewöhnliche Best-Effort-
Wirkungen bleiben erhalten. Der Review umfasst getrennte Probenfamilien,
überholte parallele Antworten, Typ-/Rootbindung, widersprechende spätere
Belege, Datenkanalabschluss, Inhaltsreadback, Cancel und manuelle Recovery.
Die direkten Core-Grenzen binden Owner, gewählte Roots, tatsächliche Wirkung
und Queueeintrag weiterhin zusammen. Kein verlorener mutierender Befehl wird
wiederholt.

Zwei statische Verdachtsfälle sind ausgeschlossen: Der Intentprüfer bindet
jeden Wirkungspfad an die ausgewählten physischen Roots. Der gemeinsame
Dateikopierer erhält FTPS-Temps ohne atomaren Anlagebeleg als Recoverypfad;
er fällt dafür nicht auf ein pfadbasiertes DELE zurück.

## Verifikation

Der neue öffentliche Protokollgegenlauf war vor der Korrektur rot und besteht
danach mit beiden Verlustzeitpunkten. Die vollständigen Gates bestehen auf
macOS mit 1130 Core-, 69 CLI- und 207 App-Tests sowie auf Linux mit 1149 Core-,
69 CLI- und 197 App-Tests. Je 21 Core-Spezialfälle und zwei Linux-GTK-Fälle
bleiben ignoriert. Die vollständige Core-Suite enthält die künstlichen FTP-
und TLS-Peers einschließlich Datenkanalabschluss und Kollisionen.

Workspace-Clippy mit allen Targets und `-D warnings`, Fuzz mit
`--locked --all-targets` und Formatprüfung bestehen beidseitig. Alle 383
festgehaltenen Quell- und Werkzeughashes sind vor und nach den Gates identisch.
