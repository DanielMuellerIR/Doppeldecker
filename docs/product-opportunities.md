# Spätere Produkt- und Performancechancen

Stand: 2026-11-01. Diese Ideen sind keine Releasepflicht. Beauftragte offene Arbeit
steht im [Backlog](../backlog.md), das Releaseziel in der [Roadmap](roadmap.md).
Vor Umsetzung Nutzen, Fertigkriterium und Prüfweg festlegen.

## Alltagsfunktionen

| Idee | Nutzen / Grenze |
| --- | --- |
| Sync-Ausschlussmuster in der GUI | Core und CLI besitzen Muster bereits; Eingabe und konkrete Ausschlussbilanz im Assistenten ergänzen. |
| Gespeicherte Sync-Aufträge | Endpunkte und Optionen wiederverwenden; jeden Lauf frisch planen und bestätigen. Keine Permits oder Secrets speichern. |
| Zentrale Recovery-Ansicht | Vorhandene typisierte Journale gemeinsam lesbar machen; Aktionen bleiben an ihren jeweiligen Vertrag gebunden. |
| Fortsetzbare große Transfers | Bestätigte Teilbereiche und Tempidentität persistieren; kein blinder mutierender Retry. |
| Prüfsummenaktion | Frische gebundene Streams hashen und Pane-Auswahlen vergleichen. |
| Weitere Archivformate | tar/komprimierte tar-Dateien mit klarer Pfad-, Link- und Ressourcenbehandlung. |
| Metadaten in Stapelnamen | Foto-/Audio-/Dokumentfelder als ausdrückliche Namensregel verwenden. |
| Zusätzliche native Metadatenmodelle | Nur nach konkretem Bedarf; [ForkLift-Parität](forklift-parity.md) begrenzt die Releasepflicht. |

## Messbare Performancekandidaten

- Kanonische Baumbelege innerhalb derselben Prüfphase gemeinsam verwenden.
- Doppelte Pane-Sortierung und wiederholte Sortierschlüsselbildung vermeiden.
- Fortschritt günstiger persistieren, ohne Crash-Recovery zu schwächen.
- Linux-Listing mit einem geeigneten Metadatenaufruf je Eintrag.
- Ordnergrößenergebnisse und virtualisierte Scrollereignisse pro Frame bündeln.

Vor Optimierung Laufzeit, Spitzen-Speicher, Dateisystem-/Netzwerkbytes,
Provideranfragen, Journalbytes und UI-Frames messen. Große flache/tiefe Bäume,
hohe Netzlatenz und große Queues gehören zur Messung. Keine vermutete
Beschleunigung als Befund ausgeben; Vertragsänderungen ausdrücklich entscheiden.

[Frühere Analyse und Entwürfe](product-opportunities-history-2026-10.md).
