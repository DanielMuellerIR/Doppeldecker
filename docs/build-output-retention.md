# Builddaten und Testartefakte begrenzen

Stand: 2026-11-18.

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

## Automatisierte GUI-Prüfblöcke

Ein automatisierter Lauf besitzt einen temporären Elternordner für die gesamte
Folge aus Vorbereitung, App-Starts, Ergebnisprüfung und Wiederanlauf. Der
Launcher kann den markierten Testbereich vor dem ersten Fenster anlegen.
Damit kann die Steuerung kleine, deterministische Eingaben vorbereiten und
mit `--reuse` denselben Zustand erneut öffnen. Beispiel für einen Python-Test:

```python
import os
from pathlib import Path
import subprocess
import tempfile

launcher = Path("tools/run-test-sandbox.sh").resolve()
with tempfile.TemporaryDirectory(prefix="doppeldecker-gui-") as block:
    env = {**os.environ, "TMPDIR": block}
    sandbox = Path(subprocess.check_output([launcher, "--prepare"], env=env, text=True).rstrip("\n"))
    (sandbox / "quelle" / "Probe.txt").write_text("Test", encoding="utf-8")
    command = [launcher, "--reuse", sandbox]
    # Hier startet die Teststeuerung command mit env, bedient die App und wartet
    # auf ihr Ende. Ergebnis und ggf. Wiederanlauf innerhalb dieses Blocks prüfen.
```

Bei einem vorhandenen Testbuild `--binary <Programm>` ergänzen. Die Teststeuerung
wartet auf die eigene App und beendet ihre Prozesse auch bei Fehler oder Timeout,
bevor der Block seinen Ordner freigibt. Manuelle Kontrollen und bewusst erhaltene
Recovery-Fälle können die Sandbox wie bisher über das App-Ende hinaus behalten.
Ein Absturz der Teststeuerung kann Reste hinterlassen. Beim Wiederanlauf nur den
eigenen beendeten Prüfblock zuordnen und nach der Auswertung aufräumen.

Im Normalfall genügen wenige kleine Dateien und ein absichtlich gleicher Name
für eine Kollision. Große Bäume oder Dateien nur erzeugen, wenn Umfang, Dauer
oder Abbruch selbst Gegenstand des Tests sind. Je Fall möglichst einen kurzen
Ergebnisbeleg mit Stand und Grenze behalten. Bildschirmbilder und ausführliche
Logs gezielt für einen sichtbaren Befund oder eine offene Diagnose sichern.
