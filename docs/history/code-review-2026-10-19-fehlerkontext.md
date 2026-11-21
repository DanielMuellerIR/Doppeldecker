# Fehlerkontext: Codes und Prüfpfade bis zum Tauri-Payload

Vier ursprüngliche Gegenläufe, ein zusätzlicher Claim-Gegenlauf und vier
Abschlussgegenläufe belegen die verlorenen Fehlerdaten. Die jeweilige Gegenprobe läuft vor
der zugehörigen Korrektur tatsächlich rot; der Zusatzlauf erfolgt nach den
ersten Kontextkorrekturen bei noch unveränderten Claim-Umklassifizierungen.

## Befunde und Korrektur

Drei gezielte VFS-Faults erreichen den echten `plan_remote_delete`-Aufruf an
Rootbindung, Rootlisting und rekursiver Auswahl. Alle liefern `plan_drift` mit
einem strukturierten Prüfpfad. Die bisherigen Kontextmeldungen erzeugen daraus
`io` ohne Pfade. Der neue gemeinsame Core-Helfer `with_context` ergänzt nur den
Anzeigetext und erhält Fehlerart, Domänencode und vorhandene Pfade. Er ersetzt
auch den bereits korrekten privaten Snapshot-Helfer und die übrigen entsprechenden
Snapshot-Kontexte. Der Verbleib-Kontext beim lokalen Restore-Cleanup verwendet
denselben Vertrag; dessen Übernahme ist statisch geprüft.

Zwei bestehende Quarantänegegenläufe führen tatsächliche künstliche Dateiwirkungen
aus: ein Fehler nach dem abschließenden Manifestwechsel und Drift beim zweiten
Claim. Die erhaltenen Laufdaten sind über die Quarantäneauflistung nachweisbar,
fehlen jedoch im strukturierten Fehler. Die Hülle für unklare Quarantänewirkung
übernimmt jetzt alle Ursprungspfade und den konkreten Laufpfad. Ein bereits
unklarer Fehler behält ebenfalls diesen Laufpfad.

Der zusätzliche VFS-Gegenlauf meldet einen Verbindungsverlust vor dem
Manifestwechsel sowie einen Drift vor dem Claim. Die bisherigen Zweige melden
im ersten Fall fälschlich `plan_drift`; beide verlieren ihre Prüfpfade. Eine
geteilte Claim-Fehlerprüfung klassifiziert jetzt nur fehlende oder widersprüchliche
Belege als Drift. Transport- und andere bereits klassifizierte Fehler bleiben
unverändert. Quellen und vorhandene Manifestdaten bleiben in beiden künstlichen
Fällen erhalten. Es wird kein echter verlorener Netzwerktransport behauptet.

Ein tatsächlicher Dateisystem-Cleanupfehler durch einen nicht leeren eigenen
Testordner läuft über den gemeinsamen Core-Ergebnishelfer und `AppError::from`
in Tauri `InvokeError`. Vorher fehlt dort der strukturierte Pfad. `AppError`
überträgt nun `recovery_paths`, wenn welche vorhanden sind. Ohne Prüfpfade bleibt
das bisherige JSON aus `code` und `message` byteinhaltlich gleich. Der Test
verwendet Tauri selbst für die Fehlerkonvertierung; er ersetzt keinen nativen
Fensterlauf und behauptet auch keinen solchen.

## Zusammengesetzte DELETE- und Queuefehler

Der Abschlussabgleich findet weitere Verluste am bereits vorhandenen
DELETE-Readback und beim Reset einer verbrauchten Queuevorbereitung. Vier
bestehende Caller-Fault-Tests werden verstärkt und laufen vor der Korrektur rot.
Sie prüfen gescheiterten Identitätsneuaufbau, fehlenden Abwesenheitsbeleg,
unvollständigen/mehrdeutigen/gescheiterten Inhaltsbeleg und die Kombination eines
unklaren Probeausgangs mit einem Journalresetfehler.

Die DELETE-Fehlerhüllen führen jetzt die Pfade des ursprünglichen Transportfehlers,
des Readbacks und des konkreten Records zusammen. Auch die Vorprüfung vor der
irreversiblen Mutation verwendet die gemeinsame Codeeinordnung statt eines
pauschalen Driftcodes. Der gemeinsame Helfer heißt entsprechend allgemein
`bound_delete_error` beziehungsweise `delete_unclear`. Die Mutation bleibt
unverändert auf genau einen Aufruf begrenzt; ein Wiederverbinden dient nur dem
lesenden Zustandsbeleg.

Beim Queue-Reset bleiben beide Pfadmengen erhalten. Ein ursprüngliches
`outcome_unclear` behält gegenüber einem gewöhnlichen Resetfehler Vorrang;
sonst benennt der Resetfehler weiterhin den nötigen Schreibfehler-/Neustartweg.
Die vorhandenen Wartestände, fehlende Ausführbarkeit und bewusste Neuprüfung
bleiben erhalten. Die DELETE-Fälle verwenden den vorhandenen geskripteten VFS,
keinen echten Nutzerdaten-Löschlauf; der Queuefall hält seinen echten isolierten
Journalbestand und künstliche Belegdateien.

## Abgleich und Grenzen

Die CLI-JSONL-Übernahme aus dem früheren CLI-Paket und der bestehende
Realmodus-Auditabschluss wurden abgeglichen. Sie erhalten bereits den führenden
Fehlercode und dessen Prüfpfade. Die frühere Probe-/Cleanup-Konsolidierung bleibt
erhalten. Fehler der Zufallsquelle und der JSON-Serialisierung besitzen an diesen
Stellen keine zusätzlichen Domänen- oder Recoverydaten; sie sind keine weiteren
Kandidaten für diesen Fund.

Die Änderungen lockern keine Providerfähigkeit, erlauben keinen Wiederholversuch
für Mutationen und erteilen kein Entfernungsrecht über einen Prüfpfad. Die
VFS-Faults sind deterministische Aufrufgegenproben; Quarantänedateiwirkungen und
Cleanupfehler sind tatsächlich ausgeführt. Alle Daten gehören eigenen temporären
Wurzeln. Frontenddarstellung und Bedienung bleiben unverändert.

## Prüfung

Alle neun Gegenläufe bestehen nach der Korrektur. Die vollständigen
Rust-Gates bestehen auf beiden Systemen. macOS: Core 1136, CLI 76, App 219 bestanden.
Linux: Core 1155, CLI 76, App 209 bestanden. Core jeweils 22 ignoriert; CLI jeweils ein
separat durch seinen Elternfall ausgeführter Kindtest; App macOS zwei Kindtests, Linux
zusätzlich zwei GTK-Fälle. Workspace-Clippy mit allen Targets und `-D warnings`,
Fuzz-Build mit gelocktem Manifest und Formatprüfung bestehen ebenfalls auf beiden
Systemen. Alle 385 erfassten Quellen bleiben während der Prüfungen unverändert. Frontend
und native GUI wurden bei unveränderter Oberfläche nicht erneut geprüft. Dieser Review
umfasst das Kontext-/Payload-Delta und seine aktuellen direkten Grenzen, keinen
erneuten vollständigen Review der großen beteiligten Bereiche.


Die abschließenden Vollgates nach dem zusätzlichen DELETE-/Queue-Delta bestehen mit
denselben Testzahlen wie zuvor auf beiden Systemen. Alle 385 Quellen bleiben auch in
dieser abschließenden Prüfrunde unverändert. Die beiden zuvor präzisierten Kommentare
sind darin enthalten.
