# Vollreview: Plattformdienste im Core

Alle acht aktuellen Bereichsdateien wurden vollständig gelesen, einschließlich
Linux-Speicherproben, Finder-Parser, Netzwerkentdeckung, Favoriten,
Ordnergrößen, Tests und Finder-Fuzz-Eingang. Der Anfangsumfang umfasst 3932 Zeilen.
Zusätzlich wurden die direkten App-Aufrufgrenzen für Mounts, Favoriten und
Größen sowie die betroffenen Verträge geprüft.

## Korrigierte Befunde

**P2: Laufwerksmerkmale ohne belegten Mountpunkt.** Eine diskutil-Antwort ohne
MountPoint oder mit falschem Feldtyp wurde bisher trotzdem übernommen.
Der öffentliche Parser verlangt jetzt den exakt passenden Mountpunkt, bevor
er irgendein Merkmal verändert. Künstliche Plists belegen die Ablehnung samt
vollständig unverändertem Ausgangsdatensatz.

**P2: Auswerfbarer Systemroot bei externer Startplatte.** Die physischen Merkmale
Internal=false und Ejectable=true konnten auch den Mountpunkt `/` als extern
und auswerfbar markieren. Der laufende Systemroot bleibt unabhängig von der
physischen Platte intern, nicht auswerfbar und ohne Unmountstrategie.
Der Test nutzt ausschließlich künstliche Plistdaten; kein echtes Volume wird
verändert oder ausgehängt.

**P2: Blockierender Favoritenleser.** Das lesende Open einer FIFO wartete bereits
vor der anschließenden Prüfung auf eine reguläre Datei. O_NONBLOCK verhindert
dieses Warten; die bestehende Handle- und Dateitypprüfung bleibt maßgeblich.
Ein eigener FIFO in einem markierten Wegwerfbaum und ein isolierter Kindprozess
belegen den vorherigen Stillstand und die jetzige InvalidData-Ablehnung.
Der Test beendet und räumt ausschließlich sein eigenes Kind auf.

**P2: Alter Größenwert nach fehlgeschlagener Neuberechnung.** Ein früherer
vollständiger Cacheeintrag überlebte den Start einer neuen Berechnung. Schlug
diese fehl, erschien beim nächsten Cacheaufruf wieder die alte vollständige
Zahl. Jede neue Berechnung entfernt jetzt das vorherige Ergebnis; die
Generationsbindung gegen überholte parallele Ergebnisse bleibt bestehen.
Der Gegenlauf verwendet echte Wegwerfverzeichnisse mit zuerst drei und später
zwölf Bytes, einem dazwischen fehlenden Root und erhaltenen ursprünglichen Daten.

## Weitere Grenzen

Die Linux-Speicherprobe behält ihre gemeinsame Listingfrist, die harte Frist
pro Kindprozess und begrenzte Prozess- und Pipebudgets. Finder-Parser und
Netzwerkentdeckung bleiben bei Datenmenge, Records und Laufzeit begrenzt.
Es wurden keine Originalfavoriten, echten Netzfreigaben oder fremden Volumes
für die Gegenläufe verwendet.

Der Finder-Import in der App besitzt ebenfalls ein Open vor der Prüfung auf
eine reguläre Datei. Dieses bereichsübergreifende Muster ist als offener
Querschnittspunkt erfasst; dort steht ein eigener Laufzeitgegenbeleg noch aus.

## Verifikation

Alle vier neuen Gegenläufe scheiterten vor der Korrektur an der erwarteten
fachlichen Assertion und bestehen danach. Die vollständigen Gates bestehen auf
macOS mit 1134 Core-, 69 CLI- und 207 App-Tests sowie auf Linux mit
1153 Core-, 69 CLI- und 197 App-Tests. Je 22 Core-Spezialfälle
und 2 Linux-GTK-Fälle bleiben ignoriert; der darin enthaltene FIFO-Kindhelfer
wird vom aktiven Haupttest separat und exakt gestartet.

Workspace-Clippy mit allen Targets und `-D warnings`, Fuzz mit
`--locked --all-targets` und Formatprüfung bestehen beidseitig. Alle 383
festgehaltenen Quell- und Werkzeughashes sind vor und nach den Gates identisch.
