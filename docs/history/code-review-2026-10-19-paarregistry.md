# Paarregistry: begrenztes Lesen regulärer Dateien

Der Leser der bidirektionalen Paarregistry verwendete `std::fs::read`. Ein FIFO
konnte dadurch bereits beim Öffnen blockieren. Finale Symlinks wurden verfolgt;
für gültiges JSON gab es keine Größenbegrenzung. Vier Gegenproben laufen gegen
die unveränderte Implementierung tatsächlich rot: FIFO ohne Schreiber,
16 MiB plus ein Byte gültiges JSON, finaler Symlink und übergroßer Schreibbestand.

## Korrektur und Grenzen

Der Leser öffnet unter macOS und Linux mit `NOFOLLOW` und `NONBLOCK` und prüft
Dateityp und Größe am gehaltenen Handle. Nur eine wirklich fehlende Datei
liefert weiterhin ein leeres Register. Auch ein ins Leere zeigender finaler
Symlink wird als `invalid_data` zurückgewiesen. Die Grenze beträgt 16 MiB.
Nach der Handleprüfung liest der Code höchstens ein zusätzliches Byte und
verwirft Übergröße; das begrenzt auch ein zwischenzeitliches Wachstum.
Dieser Wachstumsfall wurde am Code geprüft, nicht durch einen erzwungenen
konkurrierenden Dateizugriff reproduziert.

Der Schreiber prüft dieselbe Grenze einschließlich seines Zeilenumbruchs, bevor
er die temporäre Datei öffnet. Übergröße liefert `invalid_input`, erhält den
vorherigen Bestand und hinterlässt keine temporäre Datei. Die Serialisierung
verwendet eine geliehene Map statt einer vollständigen Kopie. Exklusives Anlegen,
Synchronisierung, atomarer Rename, Schutz fremder Tempdateien sowie die
anschließende Aktualisierung des Registry-Caches bleiben erhalten.

Versions-, Paar-ID-, Schlüssel- und Backend-Bindungsprüfungen bleiben unverändert.
Der gezielte Vergleich der übrigen relevanten Leser ergibt keine weitere nötige
Änderung: Der zentrale Core-Öffner setzt schon vor seiner Handle-Typprüfung
`NONBLOCK` und `NOFOLLOW`; Baseline, Favoriten, Profile, Desktop-Einstellungen,
Finder-Import und CLI-Entscheidungsdatei verwenden passende begrenzte Leser.
Dies ist ein Delta mit angrenzenden Verträgen, kein neuer Vollreview aller Dateien.

## Prüfung

Die vier roten Gegenproben bestehen nach der Korrektur. Der FIFO-Gegenlauf
prüft genau einen vorhandenen Kindtest, wartet auf dessen tatsächlichen Start
und beendet und erntet bei einem Hänger ausschließlich sein eigenes Kind.
Genau 16 MiB gültiges JSON bleiben lesbar. Vorhandener und ins Leere zeigender
Symlink werden abgewiesen; der referenzierte künstliche Bestand bleibt erhalten.
Der übergroße Schreibversuch erhält den alten Bestand ohne Tempdateirest.

Die vollständige App-Suite besteht auf macOS mit 218 Tests; zwei separate
Kindtests sind im normalen Lauf ignoriert und werden durch ihre Eltern ausgeführt.
Auf Linux bestehen 208 Tests; zusätzlich sind zwei GTK-Fälle ignoriert.
Workspace-Clippy mit allen Targets und `-D warnings` sowie die Formatprüfung
bestehen auf beiden Systemen. Alle 385 erfassten Quellen bleiben während der
Prüfungen unverändert. Nur die App-Datei unterscheidet sich vom letzten grünen
Quellstand; frühere Core-, CLI-, Fuzz-, Frontend- und native GUI-Prüfungen werden
weiterverwendet und nicht als erneut ausgeführt gezählt.

Ein erster macOS-Lauf innerhalb der Ausführungssandbox scheitert ausschließlich
an sieben unveränderten AppKit-/Setup-Fällen. Derselbe Quellstand besteht mit
der vorhandenen Freigabe für den vollständigen isolierten App-Lauf. Alle Daten
und Pasteboards gehören den Tests; Nutzerdaten und Installationsziele bleiben
unberührt.
