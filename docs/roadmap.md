# Roadmap bis Version 1.0

Stand: 2026-11-08. Ziel ist ein komfortabler, umfassender und effizienter
Two-Pane-Dateimanager für macOS und Linux. Offene Einzelaufgaben stehen nur im
[Backlog](../backlog.md); die [Funktionsmatrix](function-audit.md) beschreibt das
aktuell implementierte Verhalten.

## Beauftragter Ausbau der Alltagstauglichkeit

1. **Metadatenumfang umgesetzt:** Die belegten Alltagsfunktionen einschließlich
   macOS-Administratorfreigabe sind implementiert. Über die belegte ForkLift-Parität
   hinausgehende Spezialfähigkeiten sind keine Releasepflicht. Der
   [Vergleich und Prüfstand](forklift-parity.md) begrenzt diesen Umfang.
2. **Komfort umgesetzt:** Entfernte Namenzyklen sowie ZIP-Erzeugung und Entpacken
   zu Remotezielen sind implementiert und gegen reale Provider geprüft.
   Die native Archivmatrix ist für alle fünf Anbieter auf beiden Plattformen
   abgenommen. [Prüfstand](remote-pack.md).
3. **Kandidaten stabilisieren:** Neue Funktionen gezielt prüfen, anschließend den
   aktuellen Kandidaten auf beiden unterstützten Plattformen als echte App und
   Paket abnehmen. [Release- und Plattformplan](release-plan.md).
4. **Auslieferung vorbereiten:** Funktionsmatrix, READMEs, Release Notes und Website
   auf denselben abgenommenen Kandidaten beziehen. Die
   [Entwicklungshinweise](release-notes-next.md) beschreiben den Umfang nach
   0.1.125; die Website beschreibt weiterhin den verfügbaren Previewdownload.
   Eine Veröffentlichung braucht einen eigenen konkreten Auftrag.

## Was „fertig“ bedeutet

Version 1.0 benötigt:

- direkt bedienbare gewöhnliche lokale und entfernte Dateiaktionen mit klaren
  Konflikt-, Abbruch- und Fehlerzuständen, auch bei großen Beständen;
- die oben genannten Komfortabläufe und den begrenzten
  [Metadaten-Paritätsumfang](forklift-parity.md);
- zuverlässige fenstereigene Queues, nachvollziehbare Wiederherstellung sowie
  einseitigen und bidirektionalen Sync mit passendem Providervertrag;
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
Der frühere Gesamtstand bleibt als [Historie bis 2026-10](roadmap-history-2026-10.md)
erhalten; seine Aufwandszahlen sind keine Schätzung für den heutigen Restumfang.
