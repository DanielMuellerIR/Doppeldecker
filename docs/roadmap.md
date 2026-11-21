# Produktziele und Roadmap

Stand: 2026-11-18. Ziel ist ein komfortabler, umfassender und effizienter
Two-Pane-Dateimanager für macOS und Linux. Offene Einzelaufgaben stehen nur im
[Backlog](../backlog.md); die [Funktionsmatrix](function-audit.md) beschreibt das
aktuell implementierte Verhalten.

## Reihenfolge

1. **Dateiarbeit über Protokollgrenzen vervollständigen.** Häufige Abläufe vom
   Verbinden bis zur abgeschlossenen Dateiaktion an der echten Oberfläche prüfen,
   beginnend mit SFTP. Für FTP, FTPS, WebDAV und SMB die jeweils betroffene
   Fähigkeit nachweisen. Konkrete Lücken nach Häufigkeit und Nutzerwirkung
   einordnen. Die [Funktionsmatrix](function-audit.md) liefert den Ausgangsstand.
   Weitere Fähigkeiten moderner Dateimanager an konkreten Arbeitsabläufen
   vergleichen, etwa Suche und Vorschau, Archive, Stapelaktionen oder lange
   Transfers. Fehlende Möglichkeiten mit ihrem Nutzen in den Backlog übernehmen.
   Der ForkLift-Metadatenvergleich begrenzt nur den Metadatenumfang.
2. **Bedienung in jedem Ausbau mitprüfen.** Eine sichtbare Menüaktion mit klarer
   Wirkung hilft beim ersten Versuch. Tastatur, Mehrfachauswahl, erhaltener
   Fokus und eine reaktionsfähige Fortschrittsanzeige machen dieselbe Arbeit
   bei Wiederholung schnell. Am echten Fenster auch Beschriftung, Lesbarkeit,
   Ausrichtung und Darstellung bei knapper Breite beurteilen.
3. **Kandidaten abnehmen und ausliefern.** Den gewählten Stand als echte App
   und Paket auf macOS und Linux prüfen. Funktionsmatrix, READMEs, Release Notes
   und Website an den abgenommenen Stand angleichen. Die Veröffentlichung
   braucht einen eigenen Auftrag. [Release- und Plattformplan](release-plan.md).

## Kriterien für Version 1.0

Zur Abnahme von Version 1.0 gehören:

- direkt bedienbare gewöhnliche lokale und entfernte Dateiaktionen mit klaren
  Konflikt-, Abbruch- und Fehlerzuständen, auch bei großen Beständen;
- auffindbare und per Tastatur bedienbare Abläufe sowie den belegten
  [Metadaten-Paritätsumfang](forklift-parity.md);
- zuverlässige fenstereigene Queues, nachvollziehbare Wiederherstellung sowie
  einseitigen und bidirektionalen Sync mit verständlicher Wirkungsvorschau,
  einstellbaren Ausschlüssen und wiederverwendbaren Aufträgen;
- reproduzierbare macOS-/Linux-Pakete mit belegter Installation, Upgrade,
  Recovery und konsistenten öffentlichen Funktionszusagen.

Fehlende lokale Atomaritätsgarantien auf entfernten Protokollen verlangen passende
Best-Effort-Wege, keine pauschale Schreibsperre. Gewöhnliche Dateiaktionen brauchen
keine allgemeine Vorschau. Remote-Quarantäne, plattformübergreifende ACL-Übersetzung,
Dateisystem-Snapshots und vollständige Backupgarantien sind kein 1.0-Ziel.

## Aktueller Stand

Lokale/entfernte Dateiaktionen, mehrere Fenster und Pane-Tabs, Zwischenablage,
Hintergrundarbeit, Suche, passive Vorschau, Stapelumbenennung, lokale ZIP-Erzeugung,
Archivnavigation und Entpacken in lokale oder entfernte Ziele sind implementiert. Einseitiger
und bidirektionaler gewöhnlicher Inhaltssync unterstützen die angebotenen Provider.
Der konkrete Prüf- und Einschränkungsstand steht in der Funktionsmatrix und den
verlinkten Verträgen; daraus folgt keine neue Releasefreigabe.

## Dokumentation pflegen

Roadmap: Ziele und Fertigkriterien. Backlog: offene Aufgaben. Changelog: Änderungen.
Vertragsdokumente: notwendige technische Grenzen und gebündelte Prüfbelege.
Historische Abläufe und wiederholte Testzahlen gehören nicht in die aktiven Listen.
Der frühere Gesamtstand bleibt als [Historie bis 2026-10](history/roadmap-history-2026-10.md)
erhalten; seine Aufwandszahlen sind keine Schätzung für den heutigen Restumfang.
