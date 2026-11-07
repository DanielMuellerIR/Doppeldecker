# Builddaten und Agentenausgaben begrenzen

Stand: 2026-11-03.

Die Untersuchung alter Linux-Teststände bestätigt eine Aufbewahrungslücke:
Ein abgeschlossener temporärer Nightfix-Stand enthielt rund 14,7 GB im `target`;
weitere Kandidaten hielten zusätzliche eigene Compiler-Zwischenprodukte.
`tools/check-gates.py` bereinigt nach einem vollständigen grünen Lauf nur
`target/debug` oberhalb von 6 GiB. Manuelle Cargo-Läufe, Teilgates und getrennte
Kandidatenverzeichnisse werden dadurch nicht automatisch aufgeräumt.

Für neue Arbeit vor dem Build vorhandene passende Caches prüfen. Einen zusätzlichen
Target-Baum nur bei notwendiger Isolation erzeugen und dessen erwartete Größe,
Zweck und Entfernung nach Abnahme festhalten. Keine vollständigen Buildbäume pro
kleiner Variante kopieren. Parallele Builds brauchen getrennte Writer; Wiederverwendung
darf keine aktiven Ergebnisse überschreiben. Nach Abschluss auch Teilgate- und
manuelle Cargo-Caches prüfen, nicht allein auf die Full-Gate-Bereinigung vertrauen.
Quellen, ungesicherte Änderungen, freigegebene Pakete und benötigte Prüfreferenzen
bewahren; entbehrliche Compiler-Zwischenprodukte und eigene Testkopien entfernen.

Der Gate-Helfer hält ausführliche Compilerlogs bereits in Dateien und zeigt bei
Fehlern einen begrenzten Ausschnitt. Dieses Muster beibehalten: dem Agenten
Ergebnis, betroffene Prüfung, kurze Fehlerstelle und Belegpfad liefern. Große
Logs und Verzeichnislisten nicht anschließend vollständig zurücklesen und keine
wiederholten Vollprüfungen ohne neue Änderung oder Erkenntnis starten.
Binäre Buildgrößen belegen keinen bestimmten Anteil früherer Tokenkosten;
solche Kosten entstehen durch Modellaufrufe und eingelesenen Kontext.
