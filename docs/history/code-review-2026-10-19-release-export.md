# Release-Delta: fertige App und DMG erhalten

Das vollständige Bereichsdelta seit dem letzten Vollreview wurde geprüft. Es
verschiebt Notarisierung, Staple und Validierung auf den temporären DMG-Kandidaten;
erst danach ersetzt dieser das vorhandene fertige Image. Die drei geänderten
Dateien und die aktuellen Build-, Updater-, App-Export-, Mount- und Detachgrenzen
wurden abgeglichen. Unveränderte Bereichsdateien behalten ihren früheren Vollbeleg.

## Belegter Fehler im App-Export

Die angrenzende Ausgabe der App in den Projektroot entfernte den alten Export
vor `ditto`. Eine isolierte Gegenprobe führt die tatsächliche Exportfunktion mit
künstlichen App-Verzeichnissen aus. Der ersetzte Kopierbefehl schreibt nur einen
Teil und scheitert danach: Am bisherigen App-Pfad bleibt ausschließlich die
unvollständige Kopie. Der alte Inhalt ist verloren.

Die App wird jetzt vollständig in ein eigenes Staging-Verzeichnis auf demselben
Dateisystem kopiert. Ein Erstexport wird über Darwins exklusives Rename angelegt;
nur eine tatsächliche Kollision erlaubt den atomaren Tausch. Dieser verschiebt
den bisherigen Export ins eigene Staging, wo er erst nach erfolgreicher Übergabe
entfernt wird. Kopier- und Übergabefehler lassen die vorherige App erhalten. Die
Elternverzeichnisse werden ohne Folgen eines finalen Symlinks geöffnet und über
gehaltene Deskriptoren an die Rename-Operation gebunden. Der Installationsweg nach
/Applications wird dabei nicht verändert.

## Prüfung

Auf macOS bestehen 32 gezielte Werkzeugtests. Die neuen Fälle prüfen Erhalt nach
Teilkopier- und Übergabefehler sowie die tatsächliche Darwin-Atomik für Erstexport,
vorhandene App und fehlenden Kandidaten. Dateien sowie intakte oder hängende
Symlinks am Exportpfad werden als ungeeignete Ziele unverändert erhalten.
Alle App-Verzeichnisse sind künstlich.
Auf Linux bestehen 29 Fälle; die drei Darwin-Fälle sind ausdrücklich übersprungen.
Die 383 erfassten Quellen bleiben während beider Läufe unverändert.
`npm run release:check`, Python-Syntaxprüfung und Diffprüfung sind ebenfalls grün.

Die vorhandenen DMG-Fälle prüfen Fehler in Submit, Staple und Validate sowie den
anschließenden Erfolg. Signieren, Notarisieren, Mounten und Kopieren sind dort
ersetzte Kommandos. Der Detach-Verhaltenslauf verwendet das echte Prüfskript mit
künstlichen Apple-Werkzeugen. Es fand keine echte Release-Erzeugung, Notarisierung,
Installation oder Veröffentlichung statt.

Geändert sind ausschließlich Releasewerkzeug und zugehörige Tests. Core, CLI,
App- und Frontendquellen sowie Lockfiles bleiben bytegleich; deren zuvor grüne
Gates werden weiterverwendet und nicht als neu ausgeführt gezählt. Dies ist ein
Delta mit aktuellen Grenzen, kein neuer vollständiger Review aller 22 Dateien.
