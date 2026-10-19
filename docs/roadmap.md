# Roadmap

Stand: 2026-10-19 · erste öffentliche Preview 0.1.124.

## Vorhandene Grundlage

Doppeldecker bietet mehrere unabhängige Dual-Pane-Fenster, Tabs, lokale
Dateioperationen und eine persistente Queue je Fenster. Quelle und Ziel werden
im Core gebunden. Kopien, Sync, Quarantäne und Papierkorb verwenden getrennte,
wirkungsbezogene Bestätigungs- und Wiederherstellungsverträge.

Hintergrundübertragungen lassen Navigation und weitere Aufträge zu. Lokale
Dateien können über die native Datei-Zwischenablage übergeben werden. Sync besitzt
Vorschau, Simulation und einen getrennten bestätigten Start. GUI und CLI teilen
denselben Kern. Remote-Funktionen richten sich nach der aktuellen
[Funktionsmatrix](function-audit.md), nicht nach einer pauschalen Protokollzusage.

## Nächste Produktziele

1. SFTP-Ordneranlage, Umbenennung und additive Uploads anhand eigener begrenzter
   Ausführungsklassen ergänzen. Der vorhandene Lesepfad erteilt keine Schreibfreigabe.
2. Problematische Namen und Metadaten pfadgenau bilanzieren; unabhängige Dateien
   sollen trotz benannter Auslassungen bedienbar bleiben.
3. Schnellfilter und abbrechbare rekursive Namenssuche mit sichtbaren Teilbilanzen.
4. Integrierte lokale und Remote-Vorschau für Grundformate; danach Remote-Bearbeitung
   mit geprüftem Rückupload und Versionskonflikten.
5. Duplizieren, Stapelumbenennung mit Vorschau sowie Archivnavigation und
   Packen/Entpacken mit klaren Zielen, Konflikten und Fortschritt.

Weitere Ideen und ihre Sicherheitsgrenzen stehen unter
[Produkt- und Performancechancen](product-opportunities.md).

## Voraussetzungen für Version 1.0

- Lokale Dateioperationen, Synchronisation, Installation, Upgrade und Recovery
  bleiben auf macOS und Linux reproduzierbar geprüft.
- Native Metadaten einschließlich Hardlinks werden entsprechend dem gewählten
  Profil vollständig übertragen oder sichtbar bilanziert.
- Alle Provider erfüllen für die freigegebenen Wirkungen denselben Plan-, Abbruch-
  und Recoveryvertrag; verbleibende Protokollgrenzen sind konkret dokumentiert.
- Bidirektionaler Sync behandelt Konflikte und Löschungen anhand persistenter
  Baselines, ohne unbemerkte Konfliktauflösung.
- Oberfläche und Dokumentation versprechen nur die tatsächlich verfügbaren
  Fähigkeiten. Ein erfolgreicher Cross-Build ersetzt keinen nativen Plattformtest.

Dateisystem-Snapshots, APFS-Klonerhalt und die automatische Übersetzung
inkompatibler ACL-Modelle sind keine Voraussetzung für 1.0.
