# Spätere Produkt- und Performancechancen

Stand: 2026-11-18. Diese Ideen sind keine Releasepflicht. Beauftragte offene Arbeit
steht im [Backlog](../backlog.md), das Releaseziel in der [Roadmap](roadmap.md).
Vor Umsetzung Nutzen, Fertigkriterium und Prüfweg festlegen.

## Alltagsfunktionen

| Idee | Nutzen / Grenze |
| --- | --- |
| Weitere fortsetzbare Provider-Transfers | WebDAV-Downloads verwenden bedingte Bytebereiche. Kompatible FTPS- und SFTP-Server liefern den vollständigen SHA-256 der Quelle. Weitere Server und Remoteziele benötigen belastbare Inhalts- und Tempidentitätsbelege über die Wiederverbindung hinweg. |
| Prüfsummenaktion | Frische gebundene Streams hashen und Pane-Auswahlen vergleichen. |
| Weitere TAR-Varianten | GNU-Sparse und unbekannte PAX-Erweiterungen benötigen bei konkretem Bedarf einen geprüften Katalog- und Nutzdatenvertrag. |
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
