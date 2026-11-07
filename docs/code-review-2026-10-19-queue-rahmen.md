# Gemeinsame Queue-Fortschritts- und Dateiabschlussrahmen

Der aktuelle Code enthält sieben gleiche Fortschrittsrahmen: vorbereiteter lokaler
Sync, vorbereiteter Remote-Sync, vorbereiteter bidirektionaler Sync, lokale und
Provider-Dateioperation, Provider-Löschen sowie der ältere lokale Syncexecutor.
Der historische Fund nennt sechs; für dieses Delta zählt der tatsächliche Bestand.
Daneben stehen die beiden gleichen terminalen Dateiabschlüsse für lokale und
Provider-Dateioperationen. Dies ist eine strukturelle Konsolidierung; ein neuer
roter Produktfehler wird dafür nicht behauptet.

## Gemeinsame private Abläufe

`QueueProgress` hält ausschließlich Zustand eines einzelnen Executors und dessen
übergebener Fensterqueue. Er übernimmt die bisherige Momentaufnahme, das bestehende
250-ms-Intervall, den ersten Fortschrittscommit, das Festhalten des ersten
Journalfehlers, den gemeinsamen Cancelkanal und die anschließende Weiterleitung
des Fortschritts. Der Executor beendet nach einer Abbruchanforderung weiterhin
seinen eigenen Cleanup-/Recoveryweg. Danach erhält sein Aufrufer Fortschritt und
ersten Persistenzfehler zurück.

Endphasen werden weiterhin nicht separat als Fortschritt gespeichert. Die
unterschiedlichen abschließenden Flushbedingungen und alle protokollspezifischen
Fehler-, Intent- und Recoverywege bleiben an ihren bisherigen Aufrufern.
`OutcomeUnclear` und strukturierte Prüfpfade werden dort wie bisher ausgewertet.
Es entsteht keine gemeinsame Fensterqueue, neue Ausführungsschranke oder neue
öffentliche Schnittstelle; das Journalformat bleibt unverändert.

`complete_file_queue_entry` übernimmt die beiden Dateiabschlüsse. Die tatsächlichen
Wirkungen ergänzen weiterhin den vorab bestätigten Auditvertrag. Vor dem terminalen
Journalcommit bleibt `PendingFileCompletion` erhalten. Erst nach erfolgreichem
Commit wird dieser Abschlussbeleg entfernt und das Audit geschrieben. Ein
Commitfehler erlaubt nur den bisherigen reinen Abschlussretry; ein anschließender
Auditfehler macht die vollzogene Dateiwirkung weiterhin nicht erneut ausführbar.
FTPS-Intentabschluss und die Bewertung eines Abbruchs bleiben davor im Providerweg.

## Prüfung

Die vollständig grüne macOS-/Linux-Matrix des unveränderten Ausgangsstands ist
als Baseline vorhanden. Die Nachprüfung verwendet die bestehenden operativen
Queue-Gegenläufe für Fortschritts-, Commit- und Auditfehler, Teilwirkungen,
Abbruch, Recovery, terminalen Abschluss und mehrere Fensterqueues. Es werden keine
Tests ergänzt, die nur die neue Hilfsstruktur nacherzählen.

Der Queue-Teillauf besteht mit 146 Tests; Workspace-Clippy ist ebenfalls grün. Die vollständigen Rust-Gates bestehen auf beiden Systemen: macOS Core 1142, CLI 76, App 219; Linux Core 1164, CLI 77, App 209. Core ignoriert je 22 Fälle, CLI je 1, App macOS 2 und Linux 4. Explizite Kindtests laufen über ihre Eltern; echte Mount-/Providerfälle und zwei Linux-GTK-Fälle behalten ihre Voraussetzungen. Workspace-Clippy mit allen Targets und `-D warnings`, Fuzz-Build mit gelocktem Manifest und Formatprüfung bestehen ebenfalls beidseitig. Alle 385 Quellen bleiben während der Läufe unverändert. Ein erster eingeschränkter Teillauf bestand 144 Tests; zwei Loopback-Fixtures scheiterten vor ihrer Ausführung an einem verweigerten Socket-Bind. Der identische freigegebene Lauf und die finalen Vollgates sind grün; die Sandboxfehler sind kein Produktgegenbeleg. Die Browserprüfung des unmittelbaren Ausgangsstands mit 408 Prüfungen einschließlich CSP und sechs Pane-Tabtests wird für die bytegleichen Anzeigequellen übernommen. Das Delta umfasst nur die gemeinsamen Rahmen und ihre
sieben beziehungsweise zwei direkten Aufrufer; historische Vollreviewhashes bleiben
erhalten. Oberfläche und nativer Fensterablauf ändern sich dadurch nicht.
