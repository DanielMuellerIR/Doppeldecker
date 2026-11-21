# Deltareview: begrenzte SFTP-Leser

Geprüft sind die Änderungen seit dem vollständigen Review auf `fb14df1` und
die heutigen direkten Grenzen für Anzeige, Inhaltskopie, Momentaufnahme,
Ordnergrößen und Queue. Die vier neuen Produktionsmodule und die neue Datei
mit den SSH-/SFTP-Paketgegenläufen sind vollständig gelesen. Beide bisherigen
Produktions-/E2E-Deltas sind geprüft. Launcher und Launcher-Selbsttests sind
gegenüber der früheren Vollreviewbasis bytegleich; deren Nachweis bleibt erhalten.
Dies ist kein erneuter Ganzreview aller neun Bereichsdateien.

## Korrigierter Befund

**P2: Ein sekundärer Closeverlust wurde hinter Inhaltsdrift verworfen.**
Der Inhaltsleser prüft vor EOF über einen zweiten gehaltenen Datei-Handle den
vollständigen Inhalt. Bei Inhaltsdrift und anschließendem `ConnectionLost` beim
Schließen dieses zweiten Handles blieb die Drift richtigerweise führend. Der
Verbindungsverlust erreichte aber die Sitzungsverwaltung nicht. Solange der
fehlgeschlagene Leser noch gehalten wurde, bereitete ein neuer öffentlicher
Leseaufruf dadurch dieselbe Sitzung erneut vor.

Der echte SSH-/SFTP-Paketgegenlauf reproduziert vor der Korrektur Generation 1
statt der erwarteten frischen Generation 2. Das Schließen durchläuft jetzt
unabhängig vom primären Fehler die vorhandene Fehlererfassung. Der Gegenlauf
bleibt bei `plan_drift`, verbindet den nächsten neuen Versuch frisch, liest
dieselben Originalbytes und sendet keine Mutation. Der alte Leser bleibt dabei
bis nach dem neuen Versuch gehalten. Der künstliche Server meldet einen echten
SFTP-Status `ConnectionLost`; ein physischer Transportabbruch wird damit nicht
behauptet. Es gibt keinen Retry des fehlgeschlagenen Inhaltsversuchs.

## Erhaltene Grenzen

Die private Inhaltsklasse bleibt eine ausdrücklich ausgewiesene additive
Lesequelle. Strikte Rootidentitäten, vollständige Plannerlistings und sämtliche
SFTP-Mutationen bleiben gesperrt. Der Review umfasst Mengen-/Zeitgrenzen,
READDIR bis EOF, Auslassungen, Pfad-/Attributdrift, gehaltene Handles,
Inhaltsreadback, Abbruch, Reconnect, lokale Teilwirkungen und Crash-Recovery.
Der gemeinsame READDIR-Transport erfasst einen sekundären Closeverlust bereits
unabhängig vom primären Fehler; dieses vorhandene Muster wird auch beim
Inhaltsreadback verwendet. Eine atomare Namespacebindung wird nicht behauptet.

## Verifikation

Der gezielte Paketgegenlauf war vor der Korrektur rot und ist danach grün.
Die vollständigen Gates bestehen auf macOS mit 1129 Core-, 69 CLI- und 207
App-Tests sowie auf Linux mit 1148 Core-, 69 CLI- und 197 App-Tests. Je 21
Core-Spezialfälle und zwei Linux-GTK-Fälle bleiben ignoriert. Workspace-Clippy
mit allen Targets und `-D warnings`, Fuzz mit `--locked --all-targets` und
Formatprüfung bestehen beidseitig. 383 Quell- und Werkzeughashes sind vor und
nach den Gates identisch.

Beide frischen isolierten OpenSSH-Vertragsfälle bestehen auf beiden Plattformen
mit jeweils genau einem Filtertreffer: der normale Server samt Snapshot und
rekursivem Download sowie der Server ohne funktionierendes `fsync`. Die
historischen Timingfälle der Launcher-Selbsttests bleiben ein gesonderter
offener Punkt; sie werden durch diese erfolgreichen Vertragsfälle nicht
stillschweigend abgeschlossen.
