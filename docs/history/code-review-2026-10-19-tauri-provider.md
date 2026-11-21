# Vollreview: Tauri-Providerbefehle

Alle acht aktuellen Bereichsdateien wurden mit ihren 8329 Ausgangszeilen
vollständig gelesen. Dazu gehören Profil- und Secretübergänge, Verbindungen,
einseitiger und bidirektionaler Sync, Quarantäne, Bestätigungen und Queueadapter.
Die direkten Cache-, Pending-, Fehler- und Frontendgrenzen wurden zusätzlich geprüft.

## Korrigierte Befunde

**P2: Kollidierter Registry-Tempname wurde gelöscht.** Der Fehlerpfad des
Paarregistry-Schreibers entfernte den gewählten Tempnamen auch dann, wenn
create_new wegen einer vorhandenen Datei scheiterte. Ein echter Dateigegenlauf
belegt den Verlust des fremden Inhalts vor der Korrektur. Cleanup gilt jetzt
nur nach erfolgreicher eigener Anlage und vor dem erfolgreichen Rename.
Der Gegenlauf erhält danach sowohl fremden Tempbestand als auch die alte Registry.
Der produktive Tempname bleibt zufällig; nur die private I/O-Grenze lässt im Test
den Kollisionsnamen eindeutig vorgeben.

**P2: Früher Profilfehler verlor seinen Code.** Drei bidirektionale Startwege
wandelten einen AppError erst in einen gewöhnlichen I/O-Textfehler zurück.
Dadurch wurde beispielsweise plan_drift zu io. Die gemeinsame Ausführungsgrenze
behält den App-Fehler direkt und übersetzt ausschließlich echte Core-I/O-Fehler.
Ein rot/grüner Gegenlauf injiziert denselben Profilvalidierungsfehler vor der
Wirkung und prüft den unveränderten echten Dateisentinel.

**P2: Bidirektionale Terminalfehler waren teilweise reiner Text.** Direkter Lauf,
Rollback und Teile der Queuevorbereitung senden jetzt den strukturierten
AppError. Die bestehende Frontendgrenze kann bereits Code und Text lesen.
Ein weiterer Dateigegenlauf bereitet zwei echte gleiche Verzeichnisbäume vor,
ändert ausschließlich den eigenen Quellinhalt und prüft den tatsächlich vom
Core gemeldeten interrupted-Code samt unverändertem Ziel im serialisierten Fehler.
Strukturierte Recoverypfade über den allgemeinen AppError sind ein eigener noch
ungeprüfter Querschnittspunkt; diese Änderung behauptet deren Weitergabe nicht.

**P2: Verbindungsstatus konnte einen überholten Profilstand zurückgeben.**
FTPS, WebDAV und SMB prüfen die Profilrevision jetzt auch nach dem Lesen von
Identität und Fähigkeiten, wie es SFTP bereits tat. Die Cacheprüfung hält ihren
Profilmutex während dieser möglicherweise wartenden Providerabfragen nicht mehr;
ein paralleles Speichern konnte deshalb einen alten Status zurücklassen.
Dieser Befund beruht auf dem vollständig gelesenen Sperr- und Aufrufablauf.
Ein mit echter Netzwerkwartezeit gehaltener Konkurrenzlauf wurde dafür bislang
nicht ausgeführt und wird nicht als Reproduktionsbeleg ausgegeben.

## Erhaltene Grenzen

Optionen und Roots werden vor der Remote-Vorbereitung geprüft. Vorbereitungen,
Permits, Queue und Fortschritt bleiben fenstergebunden; nur Profiländerungen
gehen absichtlich an alle Fenster. Die Pending-Veröffentlichung hält weiterhin
dieselbe Profiltransaktionssperre wie Save und Remove. Quarantäne erwirbt ihre
Lease vor einer möglichen Probe, und gesperrte Providerwirkungen bleiben gesperrt.
Paarrollen, Schemawechsel, Recoveryidentitäten und duplikatfreie Queueübergänge
bleiben erhalten. Es wird kein verlorener mutierender Befehl wiederholt.

## Verifikation

Die vollständigen App-Suiten bestehen auf macOS mit 210 und auf Linux mit
200 Tests; 2 Linux-GTK-Fälle bleiben ignoriert. Workspace-Clippy mit
allen Targets und `-D warnings` sowie Formatprüfung sind beidseitig grün.
Der Mac-Testbuild und die vollständige Browser-Suite mit 380 Prüfungen sowie
sechs Tabtests bestehen ebenfalls. Alle 383 Quell- und Werkzeughashes bleiben
vor und nach diesen Gates identisch.

Core, CLI, Fuzz, Manifeste und Lockfiles sind gegenüber dem unmittelbar zuvor
vollständig grün geprüften Plattformpaket bytegleich. Dessen Core-, CLI- und
Fuzz-Belege werden weiterverwendet; ein neuer Lauf wird hier nicht behauptet.

Die ergänzende native macOS-Diagnoseprüfung ist abgeschlossen. Zwei unveränderliche
Testbuilds durchlaufen dieselbe tatsächliche Bestätigung; anschließend wird nur die
eigene Quelle absichtlich geändert. Das Ziel bleibt jeweils bytegleich, die Queue
endet mit Interrupted. Der Vorherbuild protokolliert `app`, der korrigierte Build
`interrupted`. Die sichtbare Fehlermeldung bleibt verständlich und gleichlautend.
Screenshots und eigene Datei-/Queuebelege wurden geprüft.

Der Vorherhelfer R4 scheiterte insgesamt, weil er das Diagnoseprotokoll im falschen
Ordner suchte. Der bereits erreichte Produktfehler wurde danach am richtigen
privaten Logpfad samt unverändertem Ziel, Queue und Cleanup verifiziert; der ganze
Vorherblock wird deshalb nicht als bestanden gezählt. Der Nachherblock besteht
vollständig und sein Cleanup bestätigt keine verbleibenden eigenen Prozesse.
Frühere Fokus- und Eingabehelferfehler zählen nicht als bestandene Blöcke.
Die anfängliche Konsolensperre wurde durch den Nutzer aufgehoben. Die menschliche
Linux-Kandidatenabnahme bleibt davon unabhängig offen.
