# Remote-Proben: ein gemeinsamer Ergebnis- und Cleanup-Vertrag

Der Realmodus besaß mit `combine_probe_and_cleanup` einen zweiten Helfer neben
`remote::combine_probe_outcome`. Der lokale Abdruck erhielt keine strukturierten
Recoverypfade. Zusätzlich wandelten die Delete-Capabilityzweige und die
Ersetzungsquarantäne Fehler noch bedingungslos in Blockertexte um.

## Aktueller Befund und Reproduktion

Der historische Befund beschreibt nicht mehr den heute erreichbaren Remoteweg:
SFTP, FTPS, WebDAV und SMB erfüllen den vollständigen identitätsgebundenen
Restorevertrag derzeit nicht. Ihre Quarantäneabfrage endet vor einer mutierenden
Probe. Auch die strikte WebDAV-Dauerlöschprobe lehnt vorher passiv ab.
Diese Schranken wurden geprüft und bleiben unverändert. Es wird kein tatsächlich
auf einem heutigen Remoteprovider entstandener Quarantänerest behauptet.

Zwei bestehende VFS-Gegenläufe belegen den verbleibenden Metadatenverlust mit
echten künstlichen Dateiwirkungen. Im ersten Fall wird der frisch angelegte
Probeordner vor dem bedingten Entfernen gegen fremden Inhalt ausgetauscht. Im
zweiten scheitert nach der tatsächlichen Anlage zuerst die Elternsynchronisierung
und danach derselbe bedingte Cleanup. Eigener verdrängter Bestand und fremder
Inhalt bleiben erhalten; der Code ist bereits `outcome_unclear`. Neu ergänzte
Assertions auf den strukturierten Prüfpfad laufen in beiden Fällen gegen die
bisherige Implementierung tatsächlich rot: Der Pfad steht nur im Fehlertext.

## Korrektur

Alle sechs bisherigen Aufrufe verwenden nun den bestehenden gemeinsamen Helfer.
Der doppelte Helfer entfällt. Fehler nach tatsächlicher Probenanlage und beim
bedingten Cleanup erhalten Ursache, Code und Prüfpfade über den vorhandenen
Recovery-Kontextvertrag. Auch die bereits vorhandene Restore-Fehlerhülle erhält
den konkreten Prüfpfad. Ein Prüfpfad erteilt weiterhin kein Entfernungsrecht.

Die Capabilityzweige verwenden dieselbe schon für Datei- und Rename-Proben
vorhandene Fehlerprüfung: `outcome_unclear`, `connection_lost`, `plan_drift` und
`interrupted` bleiben Fehler und stoppen weitere Proben. Gewöhnliche fehlende
Fähigkeiten bleiben sichtbare Blocker. Die frühe passive Providerabsage und die
starken Restoreanforderungen werden dadurch nicht gelockert.

Die App-Vorbereitung übernimmt den Fehler schon über `AppError::from`; die
Queue reicht ihn an ihrer Planbindungsgrenze mit `?` weiter. Dort ist keine
Quelländerung nötig. Die fehlenden strukturierten Recoveryfelder im allgemeinen
App-Payload bleiben Gegenstand des gesonderten Fehlerkontext-Themas; dieses
Paket behauptet keine solche Schnittstellenerweiterung.

## Prüfung und Umfang

Beide zuvor roten Gegenläufe bestehen. Sie prüfen auch, dass die gemeinsame
Capability-Fehlerprüfung aus dem konkreten Cleanupfehler keinen Blocker macht.
Die Remote-Verzweigungen sind statisch geprüft; die direkten VFS-Fälle ersetzen
keinen Provider-Ende-zu-Ende-Beleg für einen künftig stärkeren Restorevertrag.

Die vollständigen Rust-Gates bestehen auf beiden Systemen. macOS: Core 1134, CLI 76, App
218 bestanden. Linux: Core 1153, CLI 76, App 208 bestanden. Die Coreläufe haben jeweils
22 ignorierte Fälle; CLI jeweils 1, App macOS 2 und Linux 4. Explizite Kindtests werden
durch ihre Eltern ausgeführt; echte Mount-/Providerfälle und zwei Linux-GTK-Fälle
behalten ihre Voraussetzungen. Workspace-Clippy mit allen Targets und `-D warnings`,
Fuzz-Build mit gelocktem Manifest und Formatprüfung bestehen ebenfalls auf beiden
Systemen. Alle 385 Quellen bleiben während der Läufe unverändert. Frontend und native
GUI wurden nicht erneut geprüft, da ausschließlich der Core-Fehlervertrag geändert wird.
Dieser Review betrifft den
Ergebnis-/Cleanup-Delta und seine aktuellen Core-/App-/Queuegrenzen. Er ist
kein neuer vollständiger Review der großen beteiligten Dateien.
