# Deltareview: WebDAV-Namespace und Upload

Geprüft sind die Änderungen seit dem vollständigen Review auf `fb14df1` sowie
die heutigen direkten Aufrufgrenzen. Die zehn seitdem hinzugekommenen Dateien
für Ordneranlage, Umbenennung, Upload und deren HTTPS-Tests sind vollständig
gelesen. Der damalige Vollreview der beiden bisherigen Dateien bleibt die
Basis; dies ist kein erneuter Vollreview des gesamten Providers.

## Korrigierter Befund

**P2: Die Ordner-Vorbereitung erneuerte ihre eigene Frist.** Der öffentliche
Ordneraufruf begann eine Frist von 60 Sekunden für die erste Elternkette. Die
anschließend delegierte Anlage begann weitere 60 Sekunden. Ein echter
HTTPS-Gegenlauf mit vier verzögerten Metadatenantworten reproduzierte vor der
Korrektur nach 100,03 Sekunden noch ein MKCOL und eine erfolgreiche Anlage.
Die Verzögerungen liegen jeweils unter der einzelnen I/O-Frist von 30 Sekunden.

Die erste Elternprüfung, die delegierte Vorbereitung und der Upload-Callback
teilen jetzt dieselbe Frist. Unmittelbar vor MKCOL werden Frist und Sitzung
erneut geprüft. Der Gegenlauf endet nach 75,03 Sekunden mit `connection_lost`,
ohne mutierenden Request, ohne Zielordner und mit unverändertem Sentinel.
Die bereits laufende Metadatenanfrage darf weiterhin ihre eigene Frist nutzen.
Der Test verwendet weder eine künstliche Produktionsuhr noch echte Nutzerdaten.

## Erhaltene Verträge

Gewöhnliche Additionen, Ordneranlage und Umbenennung behalten ihre bestätigte
Best-Effort-Klasse. Genau eine getaggte If-Bedingung bindet den unmittelbaren
Elternordner; die übrige Kette wird vor und nach der Wirkung geprüft. Mehrere
getaggte Listen würden keine gemeinsame UND-Bindung ergeben. Die Semantik wurde
an [RFC 4918, Abschnitt 10.4.3](https://www.rfc-editor.org/rfc/rfc4918.html#section-10.4.3)
abgeglichen. Der bekannte mögliche späte Vorfahrenwechsel bleibt sichtbar.

Allgemeine VFS-Schreibfreigaben, strikte Ziel-Syncs, Quellentfernung und
künstliche Collection-Löschproben bleiben passiv gesperrt. Die Nachprüfung
umfasst außerdem Profil-/Sitzungswechsel, vollständige Listings, Kollisionen,
Temp-Eigentum, verlorene Antworten, Inhaltsreadback, Abbruch und Crash-Intents.

## Verifikation

Der gezielte echte Fristgegenlauf ist nach der Korrektur grün. Die vollständigen
Gates bestehen auf macOS mit 1128 Core-, 69 CLI- und 207 App-Tests sowie auf
Linux mit 1147 Core-, 69 CLI- und 197 App-Tests. Je 21 Core-Spezialfälle und
zwei Linux-GTK-Fälle bleiben ignoriert. Workspace-Clippy mit allen Targets und
`-D warnings`, Fuzz mit `--locked --all-targets` und Formatprüfung bestehen
beidseitig. 383 Quell- und Werkzeughashes sind vor und nach den Gates identisch.

Der erste vollständige macOS-Lauf war durch die Werkzeugsandbox eingeschränkt:
Lokale Testserver und ein Sonderbit-Metadatenfall scheiterten an fehlenden
Rechten. Der anschließende vollständige Lauf außerhalb dieser Sandbox besteht
mit der bereits erteilten Nutzerfreigabe; die ersten Protokolle bleiben erhalten.

Der historische Restpunkt zu den doppelten Helfern `probe_resource_absent` und
`resource_present` ist überholt. Beide wurden bereits mit den allgemeinen
Mutationspfaden entfernt; die Suche in Produktion und Tests liefert keinen
Treffer. Die heutigen spezialisierten Wirkungen und ihre passiven Grenzen sind
mit diesem Deltareview nachgeprüft.
