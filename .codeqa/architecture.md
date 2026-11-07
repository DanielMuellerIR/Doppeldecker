# Strukturbild und vollständiger Review

Stand: 2026-10-18. Ursprüngliche Ausgangsbasis:
`2d32bcffb52aa41c5bc9379ef242127e286f312f`.

## Erkennbares Paradigma

Doppeldecker folgt einem Schichtenmodell mit einer ereignisgesteuerten Oberfläche.
Die Rust-Crates teilen Wirkung und Bedienung: `core` besitzt VFS, Dateioperationen,
Planung, Ausführung, Permits und Recovery; Desktop und CLI adaptieren diese
Verträge. Das Frontend nutzt ES-Module mit Factory-Funktionen und einen gemeinsamen
Kompositionspunkt. Es hält Darstellungs-/Interaktionszustand und schickt Commands,
aber keine ausführbaren Planbelege oder serialisierten Permits.

Der erkennbare Datenfluss ist:

```text
Frontend-Aktion → Tauri-Command → fenstereigene Vorbereitung/Queue → Core/VFS
                       ↑                  ↓                         ↓
                    Fehlercode ← gezieltes Fensterereignis ← Ergebnis/Recovery
CLI-Argumente → CLI-Adapter → dieselben Core-Pläne/Permits/VFS → JSONL/Audit
```

Konkrete Anker:

- `core/src/lib.rs` exportiert den GUI-freien Vertrag; `core/src/vfs.rs` trennt
  gewöhnliche Operationen und wirkungsbezogene Beweisprimitive.
- `core/src/sync.rs` trennt `plan` und `execute`; `core/src/real_mode.rs` bindet
  echte Läufe an frisch gehaltene Wurzeln und Einmalfreigaben.
- `core/src/queue.rs` besitzt Journal und externen Prozesslock. Mehrere Journale
  teilen dessen Besitzer, nicht ihren Inhalt oder ihren Scheduler.
- `src-tauri/src/main.rs` registriert Commands und erzeugt Fenster;
  `src-tauri/src/queue_runtime.rs` trennt `AppState` und `WindowState`. Verbindungen,
  Profiltransaktionen und Papierkorb-Recovery sind prozessweit, laufende Operationen,
  Vorbereitungen, Queue und Abbruchsignal fenstereigen.
- `frontend/main.js` komponiert UI-Module. `frontend/tauri-api.js` bindet Listener
  an das aktuelle Webview; einzelne globale Verbindungsänderungen bleiben bewusst
  prozessweit.
- `cli/src/main.rs` löst den Konfigurationsordner vor dem Command auf und nutzt
  dieselben Core-Verträge. Nutzertestpfade sind ausdrücklich isoliert.

## Passung: gespannt, weiterhin tragfähig

Die Schichtung passt zum Datei-/Sync-Produkt und zu mehreren parallelen Fenstern.
Kein Paradigmenwechsel ist begründet. Der Core bleibt headless testbar; Provider-
Fähigkeiten und schwächere Ausführungsklassen sind ausdrückliche Verträge.

Die Spannung liegt an den Grenzen: sehr große Executor-/Queue-Dateien verbinden
viele Operationstypen, Fehlerhüllen müssen sowohl stabile Codes als auch bereits
wirksame Teilaktionen erhalten, und schwache Remote-Primitive dürfen weder stärkere
Garantien vortäuschen noch unabhängige Operationen sperren. Prozessweiter und
fenstereigener Zustand müssen auch bei Start, Close, Exit und Profiländerung
konsistent bleiben. Diese Punkte priorisieren den Review, rechtfertigen allein
aber keinen Umbau und sind noch keine neuen bestätigten Fehler.

Der Review vom 2026-10-16 bestätigt dieses Strukturbild. Seine konkreten Fehler
lagen an asynchronem Bedienbesitz, blockierendem Desktop-I/O und
Providerinteroperabilität. Hintergrundqueue und native Datei-Zwischenablage sind
anschließend umgesetzt; die nächsten Produktarbeiten betreffen verständliche
Konflikte und nutzbare Remote-Dateiabläufe. Gewöhnliche Dateiaktionen
brauchen eigene begrenzte Ausführungsklassen, wenn strengere Sync- oder
Löschprimitive fehlen. Ein neues Framework ist dafür nicht erforderlich.

## Nachweis und Abgrenzung

`full-review-2026-10-07.json` ordnet jede eigene Datei genau einem von 26 Bereichen
zu. Alle Bereiche verlangen einen frischen vollständigen Review einschließlich
unveränderter Dateien und direkter Caller-/Fehlergrenzen. Die früheren `last_run`-
Belege bleiben in `coverage.json` je Bereich unter `review_history` erhalten;
Kampagnenhistorie, Budget und gemeldeter Verbrauch bleiben unverändert.

Das Inventar enthält eigene Tests, Fuzz-Seeds, Konfiguration, Build-/Release- und
Prüfwerkzeuge. Der eigene SVG-Renderer gehört zum Buildbereich. Der Vergleich des
vendorten smb2-Bestands mit dem lokal vorhandenen originalen Crate-Archiv bestätigt
die beiden in `upstream_comparison.changed_files` aufgeführten Änderungen. Diese
und die Patchbeschreibung gehören zum SMB-Review; unveränderter Upstreambestand
bleibt mit konkretem Grund abgegrenzt. Generierte Lockfiles werden über gesperrte
Abhängigkeitsauflösung und passende Build-Gates geprüft. Binäre Assets, historische
Dokumente, Lizenztexte und Reviewmetadaten sind im Inventar getrennt begründet.

Queue, Quarantäne und Papierkorb erhalten jeweils eigene Teilbelege. Der gemeinsame
Bereich darf erst nach allen drei Teilen als vollständig geprüft gebucht werden.
Ein Dateiinventar, eine alte `current`-Markierung oder grüne Tests allein schließen
keinen Reviewbereich ab. Nach jedem Fix folgen passende Gegenläufe, Diffprüfung,
Versionierung und Aktualisierung des tatsächlichen Abdeckungsstands.

Zum Abschluss des Reviews am 2026-10-16, vor dem beauftragten Produktausbau,
waren 18 von 26 registrierten Bereichen aktuell abgeschlossen. Neue Produktänderungen
machen die betroffenen Hashbelege erneut fällig; neue Dateien sind ihren Bereichen
zugeordnet und gelten damit noch nicht als vollständig überprüft.
AP-025 ist geschlossen. Acht vollständige Bereiche und der abschließende
Querschnitt bleiben offen; Änderungen an einzelnen Tauri-Grenzen schließen deren
übergeordnete Bereiche nicht pauschal. Der genaue Stand steht im
[aktuellen Bericht](../docs/code-review-2026-10-16.md) und in den Bereichshashes.

## Konflikt- und Clipboarddelta 2026-10-16

Die gemeinsame Konfliktdarstellung liegt in `core/src/conflict_view.rs` und gehört
zum Bereich `core-fileops-vfs`; `core/tests/conflict_choices.rs` prüft dessen echte
Queue-/VFS-Wege. `frontend/conflict-dialog.js` gehört zu `frontend-file-queue`.
Bidirektionale Auswahlmöglichkeiten und Namen bleiben im Core, Tauri hält nur die
fenstereigene Anzeigebindung. Deren Anfragegeneration schützt die neuere Bindung
vor verspäteten älteren Antworten. Die native Clipboard-Reihenfolge ist prozessweit;
die Fensterqueues bleiben getrennt.

Dies ist eine gezielte Implementierungs- und Deltaprüfung. Die historischen
Vollbereichs-Hashes wurden nicht als aktuell überschrieben, kein vollständiger
Bereichsreview oder Querschnittsabschluss wird daraus abgeleitet. Geänderte Bereiche
benötigen weiterhin ihren regulären QA-Abgleich; die acht offenen Ganzbereiche
und der Querschnitt bleiben bestehen.

## Lokales Syncdelta 2026-10-17 bis 2026-10-18

Der normale lokale Start bindet sein wirkungsbezogenes Commit-/Flushprofil im
Core an Plan, gehaltene Roots und Eltern sowie Mountfähigkeiten. Er verwendet die
Commit-/Readbackhelfer aus `fileops.rs`; Tauri und Frontend zeigen das Ergebnis und
bestätigen die konkrete Wirkung. Ein persistierter Queuehinweis ist ausschließlich
Darstellung. Ein privater Recoveryintent meldet schwache Nachbar-Temps nach einem
Prozessabbruch, ohne daraus Wiederholungs- oder Löschrechte abzuleiten.
`core/tests/local_sync_mounts.rs` gehört zu `core-real-mode` und prüft die echte
Vorbereitung/Ausführung auf eigenen nativen und exFAT-Mounts.

Das AP03B-Delta wurde gezielt geprüft, einschließlich kompletter Headless-Gates
auf macOS/Linux, realer Image-Gegenläufe und nativer macOS-Abnahme mit bestätigtem
Cleanup. Die Clipboardkorrektur vom 2026-10-17
gehört zu `tauri-local-commands`. Daraus entsteht kein neuer Vollbereichsreview;
historische Hashbelege und Kampagnenbudgets bleiben unverändert. Die acht offenen
Ganzbereiche und der Querschnitt bleiben offen.
